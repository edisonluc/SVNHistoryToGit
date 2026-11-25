use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::config::{MigrationReport, TaskConfig, TaskRecord, TaskStatus};
use crate::credential::{CredentialKind, CredentialProfile};
use crate::error::{Result, Svn2GitError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: u64,
    pub task_id: i64,
    pub level: String,
    pub message: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorEntry {
    pub svn_user: String,
    pub git_name: String,
    pub git_email: String,
    #[serde(default)]
    pub commit_count: u64,
    #[serde(default)]
    pub updated_at: String,
}

/// High-throughput async NDJSON log writer (64KB BufWriter + 100ms periodic flush)
/// running on a dedicated OS thread, decoupled from both SQLite and Tokio runtime context.
#[derive(Clone)]
pub struct LogPipeline {
    logs_dir: PathBuf,
    tx: std_mpsc::Sender<LogEntry>,
    live_tx: broadcast::Sender<LogEntry>,
    seq: Arc<std::sync::atomic::AtomicU64>,
}

impl LogPipeline {
    pub fn new(logs_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&logs_dir)?;
        let (tx, rx) = std_mpsc::channel::<LogEntry>();
        let (live_tx, _) = broadcast::channel::<LogEntry>(4096);
        let dir_clone = logs_dir.clone();

        std::thread::Builder::new()
            .name("svn2git-log-writer".to_string())
            .spawn(move || {
                let mut writers: std::collections::HashMap<i64, (BufWriter<File>, Instant)> =
                    std::collections::HashMap::new();
                let flush_every = Duration::from_millis(100);
                let mut last_sweep = Instant::now();

                loop {
                    match rx.recv_timeout(flush_every) {
                        Ok(entry) => {
                            let path = dir_clone.join(format!("task-{}.ndjson", entry.task_id));
                            let writer_entry = writers.entry(entry.task_id).or_insert_with(|| {
                                let file = OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(&path)
                                    .expect("open task log file");
                                (BufWriter::with_capacity(64 * 1024, file), Instant::now())
                            });
                            if let Ok(line) = serde_json::to_string(&entry) {
                                let _ = writer_entry.0.write_all(line.as_bytes());
                                let _ = writer_entry.0.write_all(b"\n");
                            }
                            if last_sweep.elapsed() >= flush_every {
                                for (writer, last_flush) in writers.values_mut() {
                                    let _ = writer.flush();
                                    *last_flush = Instant::now();
                                }
                                last_sweep = Instant::now();
                            }
                        }
                        Err(std_mpsc::RecvTimeoutError::Timeout) => {
                            for (writer, last_flush) in writers.values_mut() {
                                let _ = writer.flush();
                                *last_flush = Instant::now();
                            }
                            last_sweep = Instant::now();
                        }
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                for (mut writer, _) in writers.into_values() {
                    let _ = writer.flush();
                }
            })?;

        Ok(Self {
            logs_dir,
            tx,
            live_tx,
            seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        })
    }

    pub fn emit(&self, task_id: i64, level: &str, message: &str) {
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let entry = LogEntry {
            seq,
            task_id,
            level: level.to_string(),
            message: message.to_string(),
            timestamp: Utc::now().to_rfc3339(),
        };
        let _ = self.tx.send(entry.clone());
        let _ = self.live_tx.send(entry);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.live_tx.subscribe()
    }

    pub fn log_file_path(&self, task_id: i64) -> PathBuf {
        self.logs_dir.join(format!("task-{task_id}.ndjson"))
    }

    pub fn compressed_log_path(&self, task_id: i64) -> PathBuf {
        self.logs_dir.join(format!("task-{task_id}.ndjson.zst"))
    }

    pub fn logs_dir(&self) -> &Path {
        &self.logs_dir
    }

    /// Read historical log lines (supports both live `.ndjson` and archived `.ndjson.zst`).
    pub fn read_logs(
        &self,
        task_id: i64,
        offset: usize,
        limit: usize,
        level_filter: Option<&str>,
        query: Option<&str>,
    ) -> Result<Vec<LogEntry>> {
        let plain = self.log_file_path(task_id);
        let zst = self.compressed_log_path(task_id);
        let q_lower = query.map(|s| s.to_lowercase());

        let reader: Box<dyn BufRead> = if plain.exists() {
            Box::new(BufReader::new(File::open(&plain)?))
        } else if zst.exists() {
            let file = File::open(&zst)?;
            let decoder = zstd::Decoder::new(file)?;
            Box::new(BufReader::new(decoder))
        } else {
            return Ok(Vec::new());
        };

        let mut results = Vec::new();
        let mut matched_idx = 0usize;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<LogEntry>(&line) else {
                continue;
            };
            if let Some(lvl) = level_filter {
                if !lvl.is_empty() && lvl != "all" && !entry.level.eq_ignore_ascii_case(lvl) {
                    continue;
                }
            }
            if let Some(q) = &q_lower {
                if !q.is_empty() && !entry.message.to_lowercase().contains(q) {
                    continue;
                }
            }
            if matched_idx >= offset {
                results.push(entry);
                if results.len() >= limit {
                    break;
                }
            }
            matched_idx += 1;
        }
        Ok(results)
    }

    /// Compress a completed task's `.ndjson` log using Zstd (~85-90% size reduction).
    pub fn archive_task_log(&self, task_id: i64) -> Result<Option<PathBuf>> {
        let plain = self.log_file_path(task_id);
        if !plain.exists() {
            return Ok(None);
        }
        let zst_path = self.compressed_log_path(task_id);
        let mut input = File::open(&plain)?;
        let output = File::create(&zst_path)?;
        let mut encoder = zstd::Encoder::new(output, 3)?;
        std::io::copy(&mut input, &mut encoder)?;
        encoder.finish()?;
        let _ = fs::remove_file(&plain);
        Ok(Some(zst_path))
    }
}

/// Metadata store backed by SQLite in WAL mode (stays KB-sized).
#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
    pub logs: LogPipeline,
    data_dir: PathBuf,
}

impl Storage {
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("svn2git.sqlite3");
        let conn = Connection::open(&db_path)?;

        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;

            CREATE TABLE IF NOT EXISTS tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                status TEXT NOT NULL,
                stage TEXT NOT NULL,
                config_json TEXT NOT NULL,
                last_synced_rev INTEGER NOT NULL DEFAULT 0,
                total_revs INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                report_json TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS authors_hub (
                svn_user TEXT PRIMARY KEY,
                git_name TEXT NOT NULL,
                git_email TEXT NOT NULL,
                commit_count INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS guided_tasks (
                id TEXT PRIMARY KEY,
                record TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS guided_events (
                task_id TEXT NOT NULL REFERENCES guided_tasks(id),
                stage TEXT NOT NULL,
                message TEXT NOT NULL,
                recorded_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS guided_events_task ON guided_events(task_id);

            CREATE TABLE IF NOT EXISTS credential_profiles (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                host_pattern TEXT NOT NULL,
                username TEXT NOT NULL,
                ssh_key_path TEXT,
                created_at TEXT NOT NULL
            );
            ",
        )?;

        let logs = LogPipeline::new(data_dir.join("logs"))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            logs,
            data_dir,
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub(crate) fn save_guided(&self, job: &crate::guided::jobs::Job) -> Result<()> {
        let mut conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        let tx = conn.transaction()?;
        let now = Utc::now().to_rfc3339();
        tx.execute("INSERT INTO guided_tasks(id, record, updated_at) VALUES(?1, ?2, ?3)
            ON CONFLICT(id) DO UPDATE SET record=excluded.record, updated_at=excluded.updated_at",
            params![job.id, serde_json::to_string(job)?, now])?;
        tx.execute("INSERT INTO guided_events(task_id,stage,message,recorded_at) VALUES(?1,?2,?3,?4)",
            params![job.id,job.stage,job.message,now])?;
        tx.commit()?;
        Ok(())
    }

    pub fn guided_events(&self, id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        let mut stmt = conn.prepare("SELECT recorded_at,message FROM guided_events WHERE task_id=?1 ORDER BY rowid DESC LIMIT 500")?;
        let mut rows = stmt.query_map([id], |r| Ok(format!("{}  {}", r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        rows.reverse();
        Ok(rows)
    }

    pub fn guided_task(&self, id: &str) -> Result<crate::guided::jobs::Job> {
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        let record: String = conn.query_row("SELECT record FROM guided_tasks WHERE id=?1", [id], |r| r.get(0))?;
        Ok(serde_json::from_str(&record)?)
    }

    pub fn guided_history(&self) -> Result<Vec<crate::guided::jobs::Job>> {
        Ok(self.guided_history_with_times()?.into_iter().map(|(job, _)| job).collect())
    }

    pub fn guided_history_with_times(&self) -> Result<Vec<(crate::guided::jobs::Job, String)>> {
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        let mut statement = conn.prepare("SELECT record, updated_at FROM guided_tasks ORDER BY updated_at DESC")?;
        let mut jobs = Vec::new();
        for row in statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (record, updated_at) = row?;
            jobs.push((serde_json::from_str(&record)?, updated_at));
        }
        Ok(jobs)
    }

    pub fn create_task(&self, config: &TaskConfig) -> Result<TaskRecord> {
        config.validate()?;
        let now = Utc::now().to_rfc3339();
        let name = if config.name.trim().is_empty() {
            config.effective_svn_url()
        } else {
            config.name.trim().to_string()
        };
        let cfg_json = serde_json::to_string(config)?;
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        conn.execute(
            "INSERT INTO tasks (name, status, stage, config_json, last_synced_rev, total_revs, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, ?6)",
            params![name, TaskStatus::Draft.as_str(), "draft", cfg_json, now, now],
        )?;
        let id = conn.last_insert_rowid();
        drop(conn);
        self.get_task(id)?.ok_or_else(|| Svn2GitError::Storage("inserted task not found".into()))
    }

    pub fn get_task(&self, task_id: i64) -> Result<Option<TaskRecord>> {
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, status, stage, config_json, last_synced_rev, total_revs, error, report_json, created_at, updated_at
             FROM tasks WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![task_id], row_to_task)
            .optional()?;
        Ok(row)
    }

    pub fn list_tasks(&self) -> Result<Vec<TaskRecord>> {
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, status, stage, config_json, last_synced_rev, total_revs, error, report_json, created_at, updated_at
             FROM tasks ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map([], row_to_task)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn delete_task(&self, task_id: i64) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        conn.execute("DELETE FROM tasks WHERE id = ?1", params![task_id])?;
        Ok(())
    }

    pub fn update_task_state(
        &self,
        task_id: i64,
        status: Option<TaskStatus>,
        stage: Option<&str>,
        error: Option<Option<&str>>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        if let Some(st) = status {
            conn.execute(
                "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![st.as_str(), now, task_id],
            )?;
        }
        if let Some(sg) = stage {
            conn.execute(
                "UPDATE tasks SET stage = ?1, updated_at = ?2 WHERE id = ?3",
                params![sg, now, task_id],
            )?;
        }
        if let Some(err) = error {
            conn.execute(
                "UPDATE tasks SET error = ?1, updated_at = ?2 WHERE id = ?3",
                params![err, now, task_id],
            )?;
        }
        Ok(())
    }

    pub fn update_task_checkpoint(
        &self,
        task_id: i64,
        last_synced_rev: u64,
        total_revs: u64,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        conn.execute(
            "UPDATE tasks SET last_synced_rev = ?1, total_revs = ?2, updated_at = ?3 WHERE id = ?4",
            params![last_synced_rev as i64, total_revs as i64, now, task_id],
        )?;
        Ok(())
    }

    pub fn update_task_report(&self, task_id: i64, report: &MigrationReport) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let report_json = serde_json::to_string(report)?;
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        conn.execute(
            "UPDATE tasks SET report_json = ?1, updated_at = ?2 WHERE id = ?3",
            params![report_json, now, task_id],
        )?;
        Ok(())
    }

    pub fn update_task_config(&self, task_id: i64, config: &TaskConfig) -> Result<()> {
        config.validate()?;
        let now = Utc::now().to_rfc3339();
        let cfg_json = serde_json::to_string(config)?;
        let name = if config.name.trim().is_empty() {
            config.effective_svn_url()
        } else {
            config.name.trim().to_string()
        };
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        conn.execute(
            "UPDATE tasks SET name = ?1, config_json = ?2, updated_at = ?3 WHERE id = ?4",
            params![name, cfg_json, now, task_id],
        )?;
        Ok(())
    }

    // --- Authors Hub ---

    pub fn list_authors(&self) -> Result<Vec<AuthorEntry>> {
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT svn_user, git_name, git_email, commit_count, updated_at
             FROM authors_hub ORDER BY commit_count DESC, svn_user ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AuthorEntry {
                    svn_user: row.get(0)?,
                    git_name: row.get(1)?,
                    git_email: row.get(2)?,
                    commit_count: row.get::<_, i64>(3)? as u64,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn upsert_authors(&self, entries: &[AuthorEntry]) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let mut conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        let tx = conn.transaction()?;
        for item in entries {
            tx.execute(
                "INSERT INTO authors_hub (svn_user, git_name, git_email, commit_count, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(svn_user) DO UPDATE SET
                   git_name = excluded.git_name,
                   git_email = excluded.git_email,
                   commit_count = MAX(authors_hub.commit_count, excluded.commit_count),
                   updated_at = excluded.updated_at",
                params![
                    item.svn_user.trim(),
                    item.git_name.trim(),
                    item.git_email.trim(),
                    item.commit_count as i64,
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_author(&self, svn_user: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        conn.execute("DELETE FROM authors_hub WHERE svn_user = ?1", params![svn_user])?;
        Ok(())
    }

    // --- Credential Profiles ---

    pub fn list_credentials(&self) -> Result<Vec<CredentialProfile>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, host_pattern, username, ssh_key_path, created_at
             FROM credential_profiles ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], credential_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_credential(&self, id: &str) -> Result<Option<CredentialProfile>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        read_credential(&conn, id)
    }

    pub fn save_credential(&self, profile: &CredentialProfile) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        write_credential(&conn, profile)
    }

    pub fn delete_credential(&self, id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        conn.execute("DELETE FROM credential_profiles WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Stage metadata before touching the vault. If committing SQLite fails, restore the vault.
    pub fn change_credential<Undo>(
        &self,
        id: &str,
        expected: Option<&CredentialProfile>,
        replacement: Option<&CredentialProfile>,
        change_secret: impl FnOnce() -> Result<Undo>,
    ) -> Result<()>
    where
        Undo: FnOnce() -> Result<()>,
    {
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| Svn2GitError::Storage(e.to_string()))?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if read_credential(&tx, id)?.as_ref() != expected {
            return Err(Svn2GitError::Credential("凭据已变更，请刷新后重试".into()));
        }
        if let Some(profile) = replacement {
            write_credential(&tx, profile)?;
        } else {
            tx.execute("DELETE FROM credential_profiles WHERE id = ?1", [id])?;
        }
        let undo = change_secret()?;
        // Keep the transaction guard (and SQLite write lock) until the vault is restored.
        if let Err(err) = tx.execute_batch("COMMIT") {
            undo().map_err(|restore| {
                Svn2GitError::Credential(format!(
                    "无法提交凭据元数据 ({err})，恢复系统凭据失败 ({restore})，请重试"
                ))
            })?;
            return Err(err.into());
        }
        Ok(())
    }
}

fn credential_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CredentialProfile> {
    let kind_str: String = row.get(2)?;
    let kind = match kind_str.as_str() {
        "git_https_token" => CredentialKind::GitHttpsToken,
        "git_ssh_key" => CredentialKind::GitSshKey,
        _ => CredentialKind::SvnPassword,
    };
    Ok(CredentialProfile {
        id: row.get(0)?,
        name: row.get(1)?,
        kind,
        host_pattern: row.get(3)?,
        username: row.get(4)?,
        ssh_key_path: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn read_credential(conn: &Connection, id: &str) -> Result<Option<CredentialProfile>> {
    Ok(conn
        .query_row(
            "SELECT id, name, kind, host_pattern, username, ssh_key_path, created_at
         FROM credential_profiles WHERE id = ?1",
            [id],
            credential_row,
        )
        .optional()?)
}

fn write_credential(conn: &Connection, profile: &CredentialProfile) -> Result<()> {
    let kind_str = match profile.kind {
        CredentialKind::SvnPassword => "svn_password",
        CredentialKind::GitHttpsToken => "git_https_token",
        CredentialKind::GitSshKey => "git_ssh_key",
    };
    conn.execute(
            "INSERT INTO credential_profiles (id, name, kind, host_pattern, username, ssh_key_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
               name = excluded.name,
               kind = excluded.kind,
               host_pattern = excluded.host_pattern,
               username = excluded.username,
               ssh_key_path = excluded.ssh_key_path",
            params![
                profile.id,
                profile.name,
                kind_str,
                profile.host_pattern,
                profile.username,
                profile.ssh_key_path,
                profile.created_at
            ],
        )?;
    Ok(())
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    let status_str: String = row.get(2)?;
    let cfg_json: String = row.get(4)?;
    let report_json: Option<String> = row.get(8)?;
    let config: TaskConfig = serde_json::from_str(&cfg_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let report: Option<MigrationReport> = match report_json {
        Some(s) if !s.is_empty() => serde_json::from_str(&s).ok(),
        _ => None,
    };
    Ok(TaskRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        status: TaskStatus::from_db(&status_str),
        stage: row.get(3)?,
        config,
        last_synced_rev: row.get::<_, i64>(5)? as u64,
        total_revs: row.get::<_, i64>(6)? as u64,
        error: row.get(7)?,
        report,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

#[cfg(test)]
mod credential_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn failed_commit_restores_the_vault_and_original_profile() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let mut profile = CredentialProfile {
            id: "id".into(),
            name: "old".into(),
            kind: CredentialKind::SvnPassword,
            host_pattern: String::new(),
            username: "account".into(),
            ssh_key_path: None,
            created_at: "created".into(),
        };
        storage.save_credential(&profile).unwrap();
        storage
            .conn
            .lock()
            .unwrap()
            .execute_batch(
                "
            CREATE TABLE commit_guard (
              profile_id TEXT REFERENCES credential_profiles(id) DEFERRABLE INITIALLY DEFERRED
            );
            CREATE TRIGGER fail_commit AFTER UPDATE ON credential_profiles
            BEGIN INSERT INTO commit_guard VALUES ('missing'); END;
        ",
            )
            .unwrap();
        let original = profile.clone();
        profile.name = "new".into();
        let has_new_secret = Cell::new(false);
        let result = storage.change_credential("id", Some(&original), Some(&profile), || {
            has_new_secret.set(true);
            Ok(|| {
                has_new_secret.set(false);
                Ok(())
            })
        });
        assert!(result.is_err());
        assert!(!has_new_secret.get());
        assert_eq!(storage.get_credential("id").unwrap(), Some(original));
    }
}
