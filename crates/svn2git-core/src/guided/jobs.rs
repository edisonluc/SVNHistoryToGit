use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{convert, invalid, publish, source, Manifest, VerifiedRef};
use crate::credential::{AskPassServer, CredentialKind, KeyringVault, ResolvedSecret};
use crate::error::{Result, Svn2GitError};
use crate::orchestrator::build_git_auth_env;
use crate::process::{run_command, CommandSpec, ProcessRegistry};
use crate::storage::{AuthorEntry, Storage};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub manifest: Manifest,
    // Older jobs stored only the merged snapshot used for conversion and retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_overrides: Option<Vec<AuthorEntry>>,
    pub stage: String,
    pub message: String,
    pub failed_stage: Option<String>,
    pub publication_started: bool,
    pub verified: Vec<VerifiedRef>,
    pub extra_refs: Vec<String>,
    pub commit_count: u64,
    pub workdir: PathBuf,
}

pub fn credential(
    storage: &Storage,
    id: Option<&str>,
    for_svn: bool,
) -> Result<Option<ResolvedSecret>> {
    let Some(id) = id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let profile = storage
        .get_credential(id)?
        .ok_or_else(|| invalid("凭据已移除，请重新填写"))?;
    if (profile.kind == CredentialKind::SvnPassword) != for_svn {
        return Err(invalid("凭据类型与仓库不匹配"));
    }
    let secret = if profile.kind == CredentialKind::GitSshKey {
        // Earlier versions omitted the vault entry for keys without a passphrase.
        KeyringVault::optional_secret(id)?.unwrap_or_default()
    } else {
        KeyringVault::load_secret(id)?
    };
    Ok(Some(ResolvedSecret {
        username: profile.username,
        secret,
        ssh_key_path: profile.ssh_key_path,
    }))
}

pub async fn git_auth(
    storage: &Storage,
    id: Option<&str>,
) -> Result<(HashMap<String, String>, Option<AskPassServer>)> {
    let secret = credential(storage, id, false)?;
    build_git_auth_env(secret.as_ref()).await
}

pub fn create(storage: &Storage, mut manifest: Manifest, parent: Option<&Path>) -> Result<Job> {
    manifest.validate()?;
    let mut authors = storage.list_authors()?;
    let overrides = std::mem::take(&mut manifest.authors);
    for entry in &overrides {
        authors.retain(|saved| saved.svn_user != entry.svn_user);
        authors.push(entry.clone());
    }
    manifest.authors = authors;
    let base = parent
        .map(Path::to_owned)
        .unwrap_or_else(|| storage.data_dir().join("work"));
    fs::create_dir_all(&base)?;
    let id = Uuid::new_v4().to_string();
    let folder = base.canonicalize()?.join(format!("migration-{id}"));
    fs::create_dir(&folder)?;
    let job = Job {
        id,
        manifest,
        author_overrides: Some(overrides),
        stage: "created".into(),
        message: "已保存发布清单".into(),
        failed_stage: None,
        publication_started: false,
        verified: vec![],
        extra_refs: vec![],
        commit_count: 0,
        workdir: folder.join("git"),
    };
    storage.save_guided(&job)?;
    Ok(job)
}

type Progress<'a> = &'a (dyn Fn(&str, &str) + Sync);

/// The OS releases this lock even when the desktop or its helper exits abruptly.
pub async fn execute(
    storage: &Storage,
    id: &str,
    svn_id: Option<String>,
    git_id: Option<String>,
    progress: Progress<'_>,
) -> Result<Job> {
    let locks = storage.data_dir().join("locks");
    fs::create_dir_all(&locks)?;
    let uuid = Uuid::parse_str(id).map_err(|_| invalid("任务编号无效"))?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(locks.join(uuid.to_string()))?;
    lock.try_lock()
        .map_err(|_| invalid("该任务正在另一个窗口中运行"))?;
    let mut job = storage.guided_task(id)?;
    if job.stage == "completed" {
        return Ok(job);
    }
    if let Some(id) = svn_id {
        job.manifest.svn_credential_id = Some(id);
    }
    if let Some(id) = git_id {
        job.manifest.git_credential_id = Some(id);
    }
    if let Some(previous) = job.failed_stage.take() {
        job.stage = previous;
    }
    let result = deliver(storage, &mut job, progress).await;
    match result {
        Ok(()) => {
            job.stage = "completed".into();
            job.message = "所有选定引用均已在远端核对通过".into();
        }
        Err(error) => {
            job.failed_stage = Some(job.stage.clone());
            if job.stage != "remote_verify" {
                job.stage = "failed".into();
            }
            job.message = error.to_string();
            storage.save_guided(&job)?;
            return Err(error);
        }
    }
    storage.save_guided(&job)?;
    Ok(job)
}

async fn deliver(storage: &Storage, job: &mut Job, progress: Progress<'_>) -> Result<()> {
    let (env, _askpass) = git_auth(storage, job.manifest.git_credential_id.as_deref()).await?;
    if job.stage == "remote_verify" && !job.verified.is_empty() {
        progress("remote_verify", "正在重新核对远端对象");
        job.extra_refs =
            publish::verify_remote(&job.manifest.remote, &job.workdir, &job.verified, &env).await?;
        return Ok(());
    }
    if job.verified.is_empty() {
        job.stage = "convert".into();
        job.message = "正在检查已确认的源修订及远端仓库".into();
        storage.save_guided(job)?;
        progress(&job.stage, &job.message);
        let targets: Vec<_> = job.manifest.refs.iter().map(|r| r.full_ref()).collect();
        publish::check_targets(&job.manifest.remote, &targets, &env).await?;
        let secret = credential(storage, job.manifest.svn_credential_id.as_deref(), true)?;
        let info = source::info(
            &job.manifest.svn_url,
            secret.as_ref(),
            Some(job.manifest.revision),
        )
        .await?;
        if info.uuid != job.manifest.uuid || info.head_revision != job.manifest.revision {
            return Err(invalid("SVN 仓库标识或修订已变化，请重新选择来源"));
        }
        if job.workdir.exists() {
            // Preserve an interrupted conversion for inspection; never reuse an unverified index.
            job.workdir.set_file_name(format!("git-{}", Uuid::new_v4()));
        }
        fs::create_dir(&job.workdir)?;
        storage.save_guided(job)?;
        let stage = Mutex::new(job.stage.clone());
        let notify = |phase: &str, detail: &str| {
            let mut last = stage.lock().expect("conversion stage lock");
            if phase != *last {
                *last = phase.into();
                let mut snapshot = job.clone();
                snapshot.stage = phase.into();
                snapshot.message = detail.into();
                let _ = storage.save_guided(&snapshot);
            }
            progress(phase, detail);
        };
        let verified = convert::convert(
            &job.manifest,
            &job.workdir,
            &job.manifest.authors,
            secret.as_ref(),
            &notify,
        )
        .await;
        job.stage = stage.into_inner().expect("conversion stage lock");
        let verified = verified?;
        let mut expected: Vec<_> = job.manifest.refs.iter().map(|r| r.full_ref()).collect();
        let mut actual: Vec<_> = verified.iter().map(|r| r.full_ref.clone()).collect();
        expected.sort();
        actual.sort();
        if actual != expected {
            return Err(Svn2GitError::Verification(
                "转换结果与用户确认的清单不一致".into(),
            ));
        }
        let tips = verified
            .iter()
            .map(|r| r.commit_oid.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let count = CommandSpec::new(
            "git",
            vec!["rev-list".into(), "--count".into(), "--stdin".into()],
        )
        .cwd(&job.workdir)
        .stdin_bytes(format!("{tips}\n").into_bytes())
        .log_output(false);
        job.commit_count = run_command(&ProcessRegistry::new(), 0, count, None)
            .await?
            .stdout
            .trim()
            .parse()
            .map_err(|_| invalid("未能读取已核验的提交数"))?;
        job.verified = verified;
        job.stage = "push".into();
        job.message = "本地核验通过，准备发布".into();
        storage.save_guided(job)?;
    }
    let resume = job.publication_started;
    let saved = Mutex::new(job.clone());
    let checkpoint = |phase: &str, detail: &str| {
        let mut record = saved.lock().expect("publication stage lock");
        let mut next = record.clone();
        next.stage = phase.to_string();
        next.message = detail.into();
        if phase == "push" {
            next.publication_started = true;
        }
        storage.save_guided(&next)?;
        *record = next;
        progress(phase, detail);
        Ok(())
    };
    let result = publish::publish(
        &job.manifest.remote,
        &job.workdir,
        &job.verified,
        &env,
        resume,
        job.manifest.lfs_threshold_mb.is_some(),
        &checkpoint,
    )
    .await;
    *job = saved.into_inner().expect("publication stage lock");
    job.extra_refs = result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[tokio::test]
    async fn retry_after_push_only_reads_the_remote_even_if_a_branch_disappeared() {
        let dir = tempfile::tempdir().unwrap();
        let run = |path: &Path, args: &[&str]| {
            let output = Command::new("git")
                .current_dir(path)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        run(dir.path(), &["init", "--bare", "remote.git"]);
        let remote = dir.path().join("remote.git");
        let mut plan = super::super::tests::manifest();
        plan.remote = remote.to_string_lossy().into_owned();
        let storage = Storage::open(dir.path().join("state")).unwrap();
        let mut job = create(&storage, plan, None).unwrap();
        fs::create_dir(&job.workdir).unwrap();
        run(&job.workdir, &["init", "-b", "main"]);
        run(
            &job.workdir,
            &[
                "-c",
                "user.email=qa@example.org",
                "-c",
                "user.name=QA",
                "commit",
                "--allow-empty",
                "-m",
                "selected",
            ],
        );
        let oid = run(&job.workdir, &["rev-parse", "HEAD"]);
        job.verified.push(VerifiedRef {
            full_ref: "refs/heads/main".into(),
            oid: oid.clone(),
            commit_oid: oid,
            files: 0,
            revisions: 1,
        });
        job.publication_started = true;
        job.stage = "remote_verify".into();
        storage.save_guided(&job).unwrap();
        let phases = Mutex::new(Vec::new());
        let result = execute(&storage, &job.id, None, None, &|stage, _| {
            phases.lock().unwrap().push(stage.to_owned())
        })
        .await;
        assert!(result.is_err());
        assert!(phases
            .into_inner()
            .unwrap()
            .iter()
            .all(|s| s == "remote_verify"));
        assert!(run(&remote, &["for-each-ref"]).is_empty());
        assert_eq!(storage.guided_task(&job.id).unwrap().stage, "remote_verify");
    }
}
