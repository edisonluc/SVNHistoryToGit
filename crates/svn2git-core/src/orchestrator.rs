use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::{broadcast, Semaphore};
use tokio::task::JoinHandle;

use crate::config::{
    EngineKind, LayoutMode, MigrationReport, PrecheckItem, PrecheckReport, TaskConfig,
    TaskProgress, TaskRecord, TaskStatus,
};
use crate::credential::{
    resolve_askpass_binary, AskPassServer, CredentialKind, KeyringVault, ResolvedSecret,
};
use crate::doctor::{run_doctor, DoctorReport};
use crate::engine::fast_import::FastImportEngine;
use crate::engine::git_svn::GitSvnEngine;
use crate::engine::{EngineContext, MigrationEngine};
use crate::error::{Result, Svn2GitError};
use crate::git_ops::verifier::verify_svn_vs_git_tree;
use crate::git_ops::{
    check_git_remote, inspect_repo_stats_gix, optimize_git_repo, push_to_remote,
};
use crate::process::{LineCallback, ProcessRegistry};
use crate::storage::{AuthorEntry, Storage};
use crate::svn::inspector::{
    detect_layout, discover_authors, fetch_svn_info, list_svn_directory, scan_svn_externals,
    SvnTreeEntry,
};

/// Central task orchestrator: manages task lifecycle state machine, prechecks,
/// resumable checkpoints, Delta Sync, and bounded-concurrency batch queue execution.
#[derive(Clone)]
pub struct TaskOrchestrator {
    pub storage: Storage,
    pub registry: ProcessRegistry,
    running_handles: Arc<Mutex<HashMap<i64, JoinHandle<()>>>>,
    progress_tx: broadcast::Sender<TaskProgress>,
    queue_semaphore: Arc<Semaphore>,
    svn_config_dir: PathBuf,
}

impl TaskOrchestrator {
    pub fn new(storage: Storage, max_concurrent_tasks: usize) -> Result<Self> {
        let svn_config_dir = storage.data_dir().join("svn-config");
        fs::create_dir_all(&svn_config_dir)?;
        let (progress_tx, _) = broadcast::channel(512);
        Ok(Self {
            storage,
            registry: ProcessRegistry::new(),
            running_handles: Arc::new(Mutex::new(HashMap::new())),
            progress_tx,
            queue_semaphore: Arc::new(Semaphore::new(max_concurrent_tasks.max(1))),
            svn_config_dir,
        })
    }

    pub fn subscribe_progress(&self) -> broadcast::Receiver<TaskProgress> {
        self.progress_tx.subscribe()
    }

    pub async fn doctor(&self) -> DoctorReport {
        run_doctor(&self.registry).await
    }

    pub async fn browse_svn_tree(
        &self,
        config: &TaskConfig,
        relative_path: Option<&str>,
    ) -> Result<Vec<SvnTreeEntry>> {
        let secret = self.resolve_credential(config.svn_credential_id.as_deref())?;
        list_svn_directory(
            &self.registry,
            0,
            config,
            relative_path,
            secret.as_ref(),
            Some(&self.svn_config_dir),
        )
        .await
    }

    pub async fn scan_authors_for_config(&self, config: &TaskConfig) -> Result<Vec<AuthorEntry>> {
        let secret = self.resolve_credential(config.svn_credential_id.as_deref())?;
        let discovered = discover_authors(
            &self.registry,
            0,
            config,
            secret.as_ref(),
            Some(&self.svn_config_dir),
            None,
        )
        .await?;

        // Merge with existing global Authors Hub entries
        let existing: HashMap<String, AuthorEntry> = self
            .storage
            .list_authors()?
            .into_iter()
            .map(|a| (a.svn_user.clone(), a))
            .collect();

        let merged: Vec<AuthorEntry> = discovered
            .into_iter()
            .map(|mut item| {
                if let Some(saved) = existing.get(&item.svn_user) {
                    item.git_name = saved.git_name.clone();
                    item.git_email = saved.git_email.clone();
                }
                item
            })
            .collect();
        Ok(merged)
    }

    pub async fn test_credential_connection(
        &self,
        profile_id: &str,
        test_url: &str,
    ) -> Result<String> {
        let profile = self
            .storage
            .get_credential(profile_id)?
            .ok_or_else(|| Svn2GitError::Credential("credential profile not found".into()))?;
        let secret_val = KeyringVault::load_secret(profile_id).unwrap_or_default();
        let resolved = ResolvedSecret {
            username: profile.username.clone(),
            secret: secret_val,
            ssh_key_path: profile.ssh_key_path.clone(),
        };

        match profile.kind {
            CredentialKind::SvnPassword => {
                let mut cfg = dummy_config_for_url(test_url);
                cfg.trust_self_signed_cert = true;
                let info = fetch_svn_info(
                    &self.registry,
                    0,
                    &cfg,
                    Some(&resolved),
                    Some(&self.svn_config_dir),
                    None,
                )
                .await?;
                Ok(format!(
                    "SVN 连接成功：HEAD r{} (UUID: {})",
                    info.head_revision, info.uuid
                ))
            }
            CredentialKind::GitHttpsToken | CredentialKind::GitSshKey => {
                let env = build_git_auth_env(Some(&resolved)).await?;
                let msg =
                    check_git_remote(&self.registry, 0, test_url, &env.0, None).await?;
                Ok(msg)
            }
        }
    }

    pub async fn precheck(&self, task_id: i64) -> Result<PrecheckReport> {
        let task = self
            .storage
            .get_task(task_id)?
            .ok_or_else(|| Svn2GitError::Storage(format!("task {task_id} not found")))?;
        let config = &task.config;

        self.storage.update_task_state(
            task_id,
            Some(TaskStatus::Prechecking),
            Some("precheck"),
            Some(None),
        )?;
        let log_cb = self.make_log_callback(task_id);
        log_cb("info", "开始执行全链路迁移预检...");

        let mut checks = Vec::new();

        // 1. Toolchain check
        let doc = run_doctor(&self.registry).await;
        let engine_ready = match config.engine {
            EngineKind::FastImport => doc.fast_engine_ready || config.local_dump_file.is_some(),
            EngineKind::GitSvn => doc.all_core_ready,
        };
        checks.push(PrecheckItem {
            name: "本机工具链".into(),
            ok: engine_ready,
            message: if engine_ready {
                format!("所选引擎 ({:?}) 所需工具全部就绪", config.engine)
            } else {
                "缺少必要命令行工具，请前往 Doctor 页面查看详情".into()
            },
            suggestion: if engine_ready {
                None
            } else {
                Some("请安装 TortoiseSVN CLI Tools 与 Git for Windows".into())
            },
        });

        // 2. SVN connectivity & head revision
        let svn_secret = self.resolve_credential(config.svn_credential_id.as_deref())?;
        let mut head_revision = None;
        match fetch_svn_info(
            &self.registry,
            task_id,
            config,
            svn_secret.as_ref(),
            Some(&self.svn_config_dir),
            Some(log_cb.clone()),
        )
        .await
        {
            Ok(info) => {
                head_revision = Some(info.head_revision);
                self.storage
                    .update_task_checkpoint(task_id, task.last_synced_rev, info.head_revision)?;
                checks.push(PrecheckItem {
                    name: "SVN 仓库连通性".into(),
                    ok: true,
                    message: format!(
                        "连通正常 · HEAD r{} · 最近提交者 {}",
                        info.head_revision, info.last_changed_author
                    ),
                    suggestion: None,
                });
            }
            Err(e) => {
                checks.push(PrecheckItem {
                    name: "SVN 仓库连通性".into(),
                    ok: false,
                    message: e.to_string(),
                    suggestion: Some("请检查 SVN URL、凭据档案或勾选“信任自签名 SSL 证书”".into()),
                });
            }
        }

        // 3. Layout detection
        let mut detected_layout = None;
        match detect_layout(
            &self.registry,
            task_id,
            config,
            svn_secret.as_ref(),
            Some(&self.svn_config_dir),
        )
        .await
        {
            Ok(det) => {
                detected_layout = Some(det.suggested_mode);
                let ok = config.layout_mode == LayoutMode::Root
                    || det.has_trunk
                    || config.layout_mode == LayoutMode::Custom;
                checks.push(PrecheckItem {
                    name: "SVN 布局检测".into(),
                    ok,
                    message: det.summary,
                    suggestion: if ok {
                        None
                    } else {
                        Some("当前目录未发现 trunk，可切换为“单目录直迁 (Root)”或自定义布局".into())
                    },
                });
            }
            Err(e) => {
                checks.push(PrecheckItem {
                    name: "SVN 布局检测".into(),
                    ok: false,
                    message: e.to_string(),
                    suggestion: None,
                });
            }
        }

        // 4. Git remote check (if push_remote enabled)
        if config.push_remote {
            if let Some(remote_url) = config.git_remote_url.as_deref() {
                let git_secret = self.resolve_credential(config.git_credential_id.as_deref())?;
                let (git_env, _guard) = build_git_auth_env(git_secret.as_ref()).await?;
                match check_git_remote(
                    &self.registry,
                    task_id,
                    remote_url,
                    &git_env,
                    Some(log_cb.clone()),
                )
                .await
                {
                    Ok(msg) => checks.push(PrecheckItem {
                        name: "Git 远端连通性".into(),
                        ok: true,
                        message: msg,
                        suggestion: None,
                    }),
                    Err(e) => checks.push(PrecheckItem {
                        name: "Git 远端连通性".into(),
                        ok: false,
                        message: e.to_string(),
                        suggestion: Some("请在凭据保险箱中配置 Git PAT Token 或 SSH 私钥".into()),
                    }),
                }
            }
        }

        // 5. Author coverage check against global Authors Hub
        let mut unmapped_authors = Vec::new();
        if let Ok(discovered) = discover_authors(
            &self.registry,
            task_id,
            config,
            svn_secret.as_ref(),
            Some(&self.svn_config_dir),
            None,
        )
        .await
        {
            let hub: HashMap<String, AuthorEntry> = self
                .storage
                .list_authors()?
                .into_iter()
                .map(|a| (a.svn_user.clone(), a))
                .collect();
            for a in &discovered {
                if !hub.contains_key(&a.svn_user) {
                    unmapped_authors.push(a.svn_user.clone());
                }
            }
            checks.push(PrecheckItem {
                name: "作者映射覆盖度".into(),
                ok: true,
                message: if unmapped_authors.is_empty() {
                    format!("全部 {} 位历史作者已在全局花名册中映射", discovered.len())
                } else {
                    format!(
                        "发现 {} 位作者（其中 {} 位将使用默认邮箱或待工作台补全）",
                        discovered.len(),
                        unmapped_authors.len()
                    )
                },
                suggestion: if unmapped_authors.is_empty() {
                    None
                } else {
                    Some("可在 Authors Hub 中补全真实姓名与企业邮箱".into())
                },
            });
        }

        let all_ok = checks.iter().all(|c| c.ok);
        let next_status = if !all_ok {
            TaskStatus::PrecheckFailed
        } else {
            TaskStatus::Ready
        };
        let err_opt = if all_ok {
            None
        } else {
            Some("预检未全部通过，请根据提示修正配置")
        };
        self.storage
            .update_task_state(task_id, Some(next_status), Some("precheck"), Some(err_opt))?;

        Ok(PrecheckReport {
            task_id,
            ok: all_ok,
            checks,
            head_revision,
            detected_layout,
            unmapped_authors,
        })
    }

    pub fn start_task(&self, task_id: i64, is_delta_sync: bool) -> Result<()> {
        let task = self
            .storage
            .get_task(task_id)?
            .ok_or_else(|| Svn2GitError::Storage(format!("task {task_id} not found")))?;

        if let Ok(map) = self.running_handles.lock() {
            if let Some(h) = map.get(&task_id) {
                if !h.is_finished() {
                    return Err(Svn2GitError::Other("任务已在运行中".into()));
                }
            }
        }

        let initial_status = if is_delta_sync {
            TaskStatus::SyncingDelta
        } else {
            TaskStatus::Running
        };
        self.storage.update_task_state(
            task_id,
            Some(initial_status),
            Some("queued"),
            Some(None),
        )?;

        let orch = self.clone();
        let handle = tokio::spawn(async move {
            let _permit = orch.queue_semaphore.clone().acquire_owned().await;
            if let Err(err) = orch.execute_task(task, is_delta_sync).await {
                let is_cancelled = matches!(err, Svn2GitError::Cancelled);
                let final_status = if is_cancelled {
                    TaskStatus::Cancelled
                } else {
                    TaskStatus::Failed
                };
                let msg = err.to_string();
                let _ = orch.storage.update_task_state(
                    task_id,
                    Some(final_status),
                    Some(final_status.as_str()),
                    Some(Some(&msg)),
                );
                orch.storage.logs.emit(task_id, "error", &msg);
            }
        });

        if let Ok(mut map) = self.running_handles.lock() {
            map.insert(task_id, handle);
        }
        Ok(())
    }

    pub fn pause_task(&self, task_id: i64) -> Result<bool> {
        let killed = self.registry.cancel_task(task_id);
        if let Ok(mut map) = self.running_handles.lock() {
            if let Some(h) = map.remove(&task_id) {
                h.abort();
            }
        }
        self.storage.update_task_state(
            task_id,
            Some(TaskStatus::Paused),
            Some("paused"),
            Some(None),
        )?;
        self.storage
            .logs
            .emit(task_id, "warn", "任务已暂停，已保留当前 Revision 检查点，可随时断点续传");
        Ok(killed)
    }

    pub fn cancel_task(&self, task_id: i64) -> Result<bool> {
        let killed = self.registry.cancel_task(task_id);
        if let Ok(mut map) = self.running_handles.lock() {
            if let Some(h) = map.remove(&task_id) {
                h.abort();
            }
        }
        self.storage.update_task_state(
            task_id,
            Some(TaskStatus::Cancelled),
            Some("cancelled"),
            Some(Some("用户取消任务")),
        )?;
        self.storage.logs.emit(task_id, "warn", "任务已被用户取消（进程树已由内核回收）");
        Ok(killed)
    }

    /// Start multiple tasks in the bounded-concurrency batch queue.
    pub fn start_batch(&self, task_ids: &[i64]) -> Result<Vec<i64>> {
        let mut started = Vec::new();
        for &id in task_ids {
            if self.start_task(id, false).is_ok() {
                started.push(id);
            }
        }
        Ok(started)
    }

    /// Clone an existing task config into a new Draft task.
    pub fn clone_task(&self, task_id: i64, new_workdir: Option<String>) -> Result<TaskRecord> {
        let orig = self
            .storage
            .get_task(task_id)?
            .ok_or_else(|| Svn2GitError::Storage(format!("task {task_id} not found")))?;
        let mut cfg = orig.config;
        cfg.name = format!("{} (Copy)", orig.name);
        if let Some(wd) = new_workdir {
            cfg.workdir = wd;
        }
        self.storage.create_task(&cfg)
    }

    async fn execute_task(&self, task: TaskRecord, is_delta_sync: bool) -> Result<()> {
        let task_id = task.id;
        let config = task.config.clone();
        let started_at = Instant::now();
        let log_cb = self.make_log_callback(task_id);

        log_cb(
            "info",
            &format!(
                "启动迁移任务 #{} ({})，引擎: {:?}",
                task_id,
                if is_delta_sync { "增量同步 Delta Sync" } else { "全量/断点续传" },
                config.engine
            ),
        );

        let svn_secret = self.resolve_credential(config.svn_credential_id.as_deref())?;
        let git_secret = self.resolve_credential(config.git_credential_id.as_deref())?;

        // Start native `<2ms` AskPass server(s) if credentials exist
        let (svn_auth_env, _svn_askpass_guard) = build_git_auth_env(svn_secret.as_ref()).await?;
        let (git_auth_env, _git_askpass_guard) = build_git_auth_env(git_secret.as_ref()).await?;

        // Stage 1: Probe HEAD revision
        self.storage
            .update_task_state(task_id, None, Some("probe"), None)?;
        let head_rev = if let Some(end) = config.end_revision {
            end
        } else if config.local_dump_file.is_some() {
            task.total_revs.max(1)
        } else {
            let info = fetch_svn_info(
                &self.registry,
                task_id,
                &config,
                svn_secret.as_ref(),
                Some(&self.svn_config_dir),
                Some(log_cb.clone()),
            )
            .await?;
            info.head_revision
        };

        let start_rev = if task.last_synced_rev > 0 {
            task.last_synced_rev + 1
        } else {
            config.start_revision.unwrap_or(1)
        };

        if start_rev > head_rev && is_delta_sync {
            log_cb(
                "info",
                &format!("SVN 已处于最新版本 r{head_rev}，无需增量拉取"),
            );
            self.storage.update_task_state(
                task_id,
                Some(TaskStatus::Completed),
                Some("completed"),
                Some(None),
            )?;
            return Ok(());
        }

        // Stage 2: Build merged authors map & temporary authors file
        self.storage
            .update_task_state(task_id, None, Some("authors"), None)?;
        let (authors_map, authors_file) =
            self.prepare_authors(task_id, &config, svn_secret.as_ref(), &log_cb).await?;

        // Stage 3: Execute selected migration engine
        self.storage
            .update_task_state(task_id, None, Some("sync"), None)?;
        let progress_tx = self.progress_tx.clone();
        let storage_for_cp = self.storage.clone();

        let ctx = EngineContext {
            task_id,
            config: config.clone(),
            workdir: PathBuf::from(&config.workdir),
            authors_map,
            authors_file: authors_file.clone(),
            svn_secret: svn_secret.clone(),
            svn_auth_env,
            svn_config_dir: Some(self.svn_config_dir.clone()),
            start_rev,
            end_rev: head_rev,
            registry: self.registry.clone(),
            on_line: Some(log_cb.clone()),
            on_progress: Some(Arc::new(move |prog| {
                let _ = progress_tx.send(prog);
            })),
            on_checkpoint: Some(Arc::new(move |synced, total| {
                let _ = storage_for_cp.update_task_checkpoint(task_id, synced, total);
            })),
        };

        let engine: Box<dyn MigrationEngine> = match config.engine {
            EngineKind::FastImport => Box::new(FastImportEngine),
            EngineKind::GitSvn => Box::new(GitSvnEngine),
        };
        let outcome = engine.run(&ctx).await?;
        let final_rev = outcome.last_synced_rev.max(task.last_synced_rev);
        self.storage
            .update_task_checkpoint(task_id, final_rev, head_rev)?;

        // Stage 4: Scan svn:externals warnings
        let externals_found = if config.scan_externals && config.local_dump_file.is_none() {
            scan_svn_externals(
                &self.registry,
                task_id,
                &config,
                svn_secret.as_ref(),
                Some(&self.svn_config_dir),
            )
            .await
            .unwrap_or_default()
        } else {
            Vec::new()
        };
        if !externals_found.is_empty() {
            log_cb(
                "warn",
                &format!(
                    "检测到 {} 条 svn:externals 外链配置，请在审计报告中复核是否需转为 Git Submodule",
                    externals_found.len()
                ),
            );
        }

        // Stage 5: Optional Git repo optimization
        let workdir = PathBuf::from(&config.workdir);
        if config.optimize_git_repo {
            self.storage
                .update_task_state(task_id, None, Some("optimize"), None)?;
            optimize_git_repo(&self.registry, task_id, &workdir, Some(log_cb.clone())).await?;
        }

        // Stage 6: Byte-level Tree Consistency Verification
        self.storage.update_task_state(
            task_id,
            Some(TaskStatus::Verifying),
            Some("verify"),
            None,
        )?;
        let verification = if config.verify_tree_consistency && config.local_dump_file.is_none() {
            log_cb("info", "正在执行 SVN vs Git 字节级目录树一致性校验...");
            match verify_svn_vs_git_tree(
                &self.registry,
                task_id,
                &config,
                &workdir,
                final_rev,
                svn_secret.as_ref(),
                Some(&self.svn_config_dir),
                Some(log_cb.clone()),
            )
            .await
            {
                Ok(v) => {
                    if v.matched {
                        log_cb(
                            "info",
                            &format!(
                                "✅ 树一致性校验 100% 通过（核验 {} 个文件，0 缺失，0 损坏）",
                                v.checked_files
                            ),
                        );
                    } else {
                        log_cb(
                            "warn",
                            &format!(
                                "⚠ 树一致性校验存在差异：缺失 {}, 多余 {}, 哈希不匹配 {}",
                                v.missing_in_git.len(),
                                v.extra_in_git.len(),
                                v.hash_mismatches.len()
                            ),
                        );
                    }
                    Some(v)
                }
                Err(e) => {
                    log_cb("warn", &format!("树一致性校验跳过: {e}"));
                    None
                }
            }
        } else {
            None
        };

        // Stage 7: Push to Git Remote
        if config.git_remote_url.is_some() {
            self.storage
                .update_task_state(task_id, None, Some("push"), None)?;
            push_to_remote(
                &self.registry,
                task_id,
                &workdir,
                &config,
                &git_auth_env,
                !outcome.lfs_converted.is_empty(),
                Some(log_cb.clone()),
            )
            .await?;
        }

        // Stage 8: Build final audit report via gix
        let stats = inspect_repo_stats_gix(&workdir)?;
        let report = MigrationReport {
            commit_count: stats.commit_count,
            branch_count: stats.branches.len(),
            tag_count: stats.tags.len(),
            branches: stats.branches,
            tags: stats.tags,
            latest_commit: stats.latest_commit,
            current_branch: stats.current_branch,
            workdir: config.workdir.clone(),
            duration_secs: started_at.elapsed().as_secs_f64(),
            engine_used: config.engine,
            last_synced_rev: final_rev,
            gitignore_generated: outcome.gitignore_generated,
            externals_found,
            lfs_converted_files: outcome.lfs_converted,
            excluded_files_count: outcome.excluded_files_count,
            verification,
        };

        self.storage.update_task_report(task_id, &report)?;
        self.storage.update_task_state(
            task_id,
            Some(TaskStatus::Completed),
            Some("completed"),
            Some(None),
        )?;
        log_cb(
            "info",
            &format!(
                "🎉 迁移完成！共 {} 个提交、{} 个分支、{} 个标签，耗时 {:.2}s",
                report.commit_count, report.branch_count, report.tag_count, report.duration_secs
            ),
        );

        let _ = fs::remove_file(&authors_file);
        Ok(())
    }

    async fn prepare_authors(
        &self,
        task_id: i64,
        config: &TaskConfig,
        secret: Option<&ResolvedSecret>,
        log_cb: &LineCallback,
    ) -> Result<(HashMap<String, (String, String)>, PathBuf)> {
        let mut map: HashMap<String, (String, String)> = HashMap::new();

        // 1. Load global Authors Hub
        for a in self.storage.list_authors()? {
            map.insert(a.svn_user, (a.git_name, a.git_email));
        }

        // 2. Overlay explicit authors_file if configured
        if let Some(path) = config.authors_file.as_deref().filter(|p| !p.trim().is_empty()) {
            let content = fs::read_to_string(path)?;
            for line in content.lines() {
                let trimmed = line.trim().trim_start_matches('\u{feff}');
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if let Some((svn_u, rest)) = trimmed.split_once('=') {
                    let (name, email) = parse_git_author_rhs(rest.trim());
                    map.insert(svn_u.trim().to_string(), (name, email));
                }
            }
        }

        // 3. Auto-complete any missing SVN authors from live log
        if config.local_dump_file.is_none() {
            if let Ok(discovered) = discover_authors(
                &self.registry,
                task_id,
                config,
                secret,
                Some(&self.svn_config_dir),
                None,
            )
            .await
            {
                let mut added = 0usize;
                for a in discovered {
                    if !map.contains_key(&a.svn_user) {
                        map.insert(a.svn_user, (a.git_name, a.git_email));
                        added += 1;
                    }
                }
                if added > 0 {
                    log_cb(
                        "info",
                        &format!("已自动补全 {added} 位未映射 SVN 作者的默认邮箱"),
                    );
                }
            }
        }

        // Write merged authors file to data_dir/tmp
        let tmp_dir = self.storage.data_dir().join("tmp");
        fs::create_dir_all(&tmp_dir)?;
        let out_file = tmp_dir.join(format!("authors-task-{task_id}.txt"));
        let mut lines: Vec<String> = map
            .iter()
            .map(|(u, (n, e))| format!("{u} = {n} <{e}>"))
            .collect();
        lines.sort();
        fs::write(&out_file, lines.join("\n") + "\n")?;
        Ok((map, out_file))
    }

    fn resolve_credential(&self, profile_id: Option<&str>) -> Result<Option<ResolvedSecret>> {
        let Some(id) = profile_id.filter(|s| !s.trim().is_empty()) else {
            return Ok(None);
        };
        let Some(profile) = self.storage.get_credential(id)? else {
            return Ok(None);
        };
        let secret = KeyringVault::load_secret(id).unwrap_or_default();
        Ok(Some(ResolvedSecret {
            username: profile.username,
            secret,
            ssh_key_path: profile.ssh_key_path,
        }))
    }

    fn make_log_callback(&self, task_id: i64) -> LineCallback {
        let logs = self.storage.logs.clone();
        Arc::new(move |level: &str, msg: &str| {
            logs.emit(task_id, level, msg);
        })
    }
}

fn parse_git_author_rhs(rhs: &str) -> (String, String) {
    if let (Some(lt), Some(gt)) = (rhs.find('<'), rhs.rfind('>')) {
        if lt < gt {
            let name = rhs[..lt].trim().to_string();
            let email = rhs[lt + 1..gt].trim().to_string();
            return (name, email);
        }
    }
    (rhs.to_string(), format!("{rhs}@svn.local"))
}

pub(crate) async fn build_git_auth_env(
    secret: Option<&ResolvedSecret>,
) -> Result<(HashMap<String, String>, Option<AskPassServer>)> {
    let mut env = HashMap::new();
    env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
    let Some(sec) = secret else {
        return Ok((env, None));
    };

    if let Some(ssh_key) = sec.ssh_key_path.as_deref().filter(|p| !p.trim().is_empty()) {
        env.insert(
            "GIT_SSH_COMMAND".to_string(),
            format!("ssh -i '{}' -o StrictHostKeyChecking=accept-new", ssh_key.replace('\\', "/").replace('\'', "'\\''")),
        );
    }

    if !sec.secret.is_empty() {
        let server = AskPassServer::start(sec.username.clone(), sec.secret.clone()).await?;
        let askpass_bin = resolve_askpass_binary();
        env.extend(server.build_env(askpass_bin.as_ref()));
        return Ok((env, Some(server)));
    }
    Ok((env, None))
}

pub(crate) fn dummy_config_for_url(url: &str) -> TaskConfig {
    TaskConfig {
        name: "probe".into(),
        svn_url: url.to_string(),
        svn_subpath: None,
        svn_credential_id: None,
        svn_username: None,
        trust_self_signed_cert: false,
        local_dump_file: None,
        git_remote_url: None,
        git_credential_id: None,
        target_branch: None,
        subtree_prefix: None,
        push_remote: false,
        push_strategy: Default::default(),
        workdir: ".".into(),
        authors_file: None,
        default_email_domain: None,
        start_revision: None,
        end_revision: None,
        layout_mode: LayoutMode::Root,
        custom_layout: Default::default(),
        engine: EngineKind::GitSvn,
        window_size: 500,
        turbo_prefetch: false,
        prefetch_concurrency: 2,
        convert_gitignore: false,
        scan_externals: false,
        repair_gbk_messages: true,
        verify_tree_consistency: false,
        optimize_git_repo: false,
        exclude_globs: Vec::new(),
        lfs_threshold_mb: None,
    }
}
