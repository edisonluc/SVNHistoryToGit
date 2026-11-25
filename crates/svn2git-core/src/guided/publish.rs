use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;

use super::VerifiedRef;
use crate::error::{Result, Svn2GitError};
use crate::process::{run_command, CommandSpec, ProcessRegistry};

type RemoteRefs = BTreeMap<String, String>;
type Progress<'a> = &'a (dyn Fn(&str, &str) -> Result<()> + Sync);

fn protocol(remote: &str) -> Result<&'static str> {
    let invalid = || {
        Svn2GitError::Config("请输入 HTTP(S)、SSH 或本地 Git 仓库地址，凭据请在认证区域填写".into())
    };
    if remote.is_empty()
        || remote.starts_with('-')
        || remote.trim() != remote
        || remote.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    if Path::new(remote).is_absolute() {
        return Ok("file");
    }
    if remote.contains("://") {
        let url = url::Url::parse(remote).map_err(|_| invalid())?;
        if url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path().is_empty()
        {
            return Err(invalid());
        }
        return match url.scheme() {
            "https" if url.host().is_some() && url.username().is_empty() => Ok("https"),
            "http" if url.host().is_some() && url.username().is_empty() => Ok("http"),
            "ssh" if url.host().is_some() && !url.host_str().unwrap_or("").starts_with('-') => {
                Ok("ssh")
            }
            "file" if url.username().is_empty() && url.to_file_path().is_ok() => Ok("file"),
            _ => Err(invalid()),
        };
    }
    // SCP-style SSH, including user@[IPv6]:path. Git's helper:: syntax is never a repository path.
    let separator = remote
        .find("]:")
        .map(|i| i + 1)
        .or_else(|| remote.find(':'))
        .ok_or_else(invalid)?;
    let (authority, path) = remote.split_at(separator);
    let path = &path[1..];
    if authority.contains(['/', '\\', '%', '?', '#'])
        || authority.chars().any(char::is_whitespace)
        || path.is_empty()
        || path.starts_with(':')
    {
        return Err(invalid());
    }
    let endpoint = url::Url::parse(&format!("ssh://{authority}/")).map_err(|_| invalid())?;
    if endpoint.password().is_some() || endpoint.host_str().is_none_or(|h| h.starts_with('-')) {
        return Err(invalid());
    }
    Ok("ssh")
}

pub fn validate_remote(remote: &str) -> Result<()> {
    protocol(remote).map(|_| ())
}

struct Remote<'a> {
    address: &'a str,
    protocol: &'static str,
    env: &'a HashMap<String, String>,
    workdir: Option<&'a Path>,
}

impl<'a> Remote<'a> {
    fn new(
        address: &'a str,
        env: &'a HashMap<String, String>,
        workdir: Option<&'a Path>,
    ) -> Result<Self> {
        Ok(Self {
            address,
            protocol: protocol(address)?,
            env,
            workdir,
        })
    }

    async fn git(&self, mut spec: CommandSpec) -> Result<String> {
        if let Some(path) = self.workdir {
            spec = spec.cwd(path);
        }
        let spec = spec
            .envs(self.env.clone())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", self.protocol)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("LC_ALL", "C")
            .env("LANGUAGE", "C");
        let output = run_command(&ProcessRegistry::new(), 0, spec, None)
            .await
            .map_err(|err| {
                if let Svn2GitError::CommandFailed { stderr, .. } = &err {
                    let reason = stderr.to_ascii_lowercase();
                    if [
                        "authentication failed",
                        "could not read username",
                        "permission denied (publickey",
                        "returned error: 401",
                        "returned error: 403",
                        "invalid username or token",
                    ]
                    .iter()
                    .any(|hint| reason.contains(hint))
                    {
                        return Svn2GitError::Credential(format!("远端 Git 认证失败：{stderr}"));
                    }
                }
                err
            })?;
        Ok(output.stdout)
    }

    async fn read_refs(&self) -> Result<RemoteRefs> {
        // --refs would also hide annotated tags' peeled entries; these are needed for verification.
        let args = ["ls-remote", "--heads", "--tags", "--", self.address].map(str::to_owned);
        let output = self
            .git(CommandSpec::new("git", args.to_vec()).timeout(120))
            .await?;
        let mut refs = BTreeMap::new();
        for line in output.lines() {
            let Some((oid, name)) = line.split_once('\t') else {
                return Err(Svn2GitError::Git("无法解析远端 Git 引用".into()));
            };
            if !valid_oid(oid)
                || !is_destination(name.trim_end_matches("^{}"))
                || refs.insert(name.to_owned(), oid.to_owned()).is_some()
            {
                return Err(Svn2GitError::Git(format!("远端 Git 引用无效：{name}")));
            }
        }
        Ok(refs)
    }

    async fn verify_delivery(&self, refs: &[VerifiedRef]) -> Result<Vec<String>> {
        let actual = self.read_refs().await?;
        let status = compare(refs, &actual);
        if !status.complete() {
            return Err(Svn2GitError::Verification(status.summary()));
        }
        let expected: HashSet<_> = refs.iter().map(|item| item.full_ref.as_str()).collect();
        Ok(actual
            .into_keys()
            .filter(|name| !name.ends_with("^{}") && !expected.contains(name.as_str()))
            .collect())
    }

    async fn verify_local(&self, refs: &[VerifiedRef]) -> Result<()> {
        if refs.is_empty() {
            return Err(Svn2GitError::Verification("缺少已核验的本地引用".into()));
        }
        let mut destinations = HashSet::new();
        let mut input = String::new();
        for item in refs {
            if !is_destination(&item.full_ref)
                || !destinations.insert(&item.full_ref)
                || !valid_oid(&item.oid)
                || !valid_oid(&item.commit_oid)
            {
                return Err(Svn2GitError::Verification(format!(
                    "本地核验记录无效：{}",
                    item.full_ref
                )));
            }
            writeln!(input, "{}\n{}^{{commit}}", item.oid, item.oid).unwrap();
        }
        let args = vec![
            "cat-file".into(),
            "--batch-check=%(objectname) %(objecttype)".into(),
        ];
        let output = self
            .git(CommandSpec::new("git", args).stdin_bytes(input.into_bytes()))
            .await?;
        let mut lines = output.lines();
        for item in refs {
            let actual = lines.next().unwrap_or("");
            let peeled = lines.next().unwrap_or("");
            let object_matches = actual.split_once(' ').is_some_and(|(oid, kind)| {
                oid == item.oid
                    && (kind == "commit"
                        || (kind == "tag" && item.full_ref.starts_with("refs/tags/")))
            });
            if !object_matches || peeled != format!("{} commit", item.commit_oid) {
                return Err(Svn2GitError::Verification(format!(
                    "{} 本地对象校验失败，预期 {}（提交 {}），实际 {actual} / {peeled}",
                    item.full_ref, item.oid, item.commit_oid
                )));
            }
        }
        Ok(())
    }

    async fn create(&self, missing: &[&VerifiedRef], atomic: bool) -> Result<()> {
        // An explicit pushurl keeps a user's pushInsteadOf rule from writing to a
        // different destination than ls-remote checked. The remote is process-local.
        let name = format!("svn2git-{}", uuid::Uuid::new_v4());
        let mut args = vec![
            "-c".into(),
            format!("remote.{name}.url={}", self.address),
            "-c".into(),
            format!("remote.{name}.pushurl={}", self.address),
            "push".into(),
            "--porcelain".into(),
            "--no-follow-tags".into(),
            "--recurse-submodules=no".into(),
        ];
        if atomic {
            args.push("--atomic".into());
        }
        for item in missing {
            // Empty expected value means this destination must not already exist.
            args.push(format!("--force-with-lease={}:", item.full_ref));
        }
        args.extend(["--".into(), name]);
        for item in missing {
            args.push(format!("{}:{}", item.oid, item.full_ref));
        }
        // The guided job handles LFS explicitly for its frozen object IDs.
        self.git(CommandSpec::new("git", args).env("GIT_LFS_SKIP_PUSH", "1"))
            .await?;
        Ok(())
    }

    async fn upload_lfs(&self, refs: &[VerifiedRef]) -> Result<()> {
        let resolve = ["ls-remote", "--get-url", "--", self.address].map(str::to_owned);
        let resolved = self.git(CommandSpec::new("git", resolve.to_vec())).await?;
        if protocol(&resolved)? != self.protocol {
            return Err(Svn2GitError::Config(
                "Git LFS 目标的传输协议与已检查的仓库不一致".into(),
            ));
        }
        let endpoint = match self.protocol {
            "file" if Path::new(&resolved).is_absolute() => url::Url::from_file_path(&resolved)
                .map_err(|_| Svn2GitError::Config("Git LFS 文件仓库地址无效".into()))?
                .to_string(),
            "http" | "https" => {
                let base = resolved.trim_end_matches('/');
                let suffix = if base.ends_with(".git") {
                    "/info/lfs"
                } else {
                    ".git/info/lfs"
                };
                format!("{base}{suffix}")
            }
            // SSH negotiates the actual upload endpoint with git-lfs-authenticate.
            _ => resolved.clone(),
        };
        let name = format!("svn2git-lfs-{}", uuid::Uuid::new_v4());
        // LFS applies pushInsteadOf even to explicit LFS URLs. This private one-step
        // alias selects the checked endpoint despite repository .lfsconfig or user rewrites.
        let args = vec![
            "-c".into(),
            format!("remote.{name}.url={resolved}"),
            "-c".into(),
            format!("lfs.pushurl={name}"),
            "-c".into(),
            format!("url.{endpoint}.pushInsteadOf={name}"),
            "-c".into(),
            "lfs.allowincompletepush=false".into(),
            "-c".into(),
            "lfs.storage=lfs".into(),
            "lfs".into(),
            "push".into(),
            "--all".into(),
            "--stdin".into(),
            name,
        ];
        let mut tips = String::new();
        for item in refs {
            writeln!(tips, "{}", item.commit_oid).unwrap();
        }
        self.git(
            CommandSpec::new("git", args)
                .env("GIT_LFS_SKIP_PUSH", "0")
                .stdin_bytes(tips.into_bytes()),
        )
        .await?;
        Ok(())
    }

    async fn reconcile_push_error(&self, refs: &[VerifiedRef], error: Svn2GitError) -> Result<()> {
        // A lost acknowledgement can leave some or all destinations already published.
        let actual = self.read_refs().await.map_err(|read_error| {
            Svn2GitError::Git(format!("{error}\n无法核对发布结果：{read_error}"))
        })?;
        let status = compare(refs, &actual);
        if status.complete() {
            return Ok(());
        }
        Err(Svn2GitError::Git(format!("{error}\n{}", status.summary())))
    }

    async fn push_missing(
        &self,
        refs: &[VerifiedRef],
        missing: &[&VerifiedRef],
        progress: Progress<'_>,
    ) -> Result<()> {
        match self.create(missing, missing.len() > 1).await {
            Ok(()) => return Ok(()),
            Err(Svn2GitError::CommandFailed {
                ref stderr, code, ..
            }) if missing.len() > 1
                && (stderr.lines().any(|line| {
                    line == "fatal: the receiving end does not support --atomic push"
                }) || (cfg!(windows)
                    && code.is_none()
                    && stderr.ends_with("(os error 206)"))) => {}
            Err(error) => return self.reconcile_push_error(refs, error).await,
        }
        for (index, item) in missing.iter().enumerate() {
            progress("push", &format!("正在逐项创建 {}", item.full_ref))?;
            if let Err(error) = self.create(&[item], false).await {
                return self.reconcile_push_error(refs, error).await;
            }
            // Once the last write was acknowledged, any read failure must leave the
            // job in remote verification, including on servers without atomic pushes.
            let phase = if index + 1 == missing.len() {
                progress(
                    "remote_verify",
                    "所有选定引用均已推送，正在核对远端 Git 对象",
                )?;
                "remote_verify"
            } else {
                "push"
            };
            let actual = self.read_refs().await?;
            let status = compare(std::slice::from_ref(*item), &actual);
            // Persist a per-ref receipt only after read-back, including annotated tags.
            if !status.complete() {
                return Err(Svn2GitError::Verification(status.summary()));
            }
            progress(phase, &format!("已核对 {} {}", item.full_ref, item.oid))?;
        }
        Ok(())
    }
}

fn is_destination(name: &str) -> bool {
    (name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
        && gix::validate::reference::name(name.into()).is_ok()
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

fn require_empty(remote: &RemoteRefs) -> Result<()> {
    if !remote.is_empty() {
        let names: Vec<_> = remote
            .keys()
            .filter(|name| !name.ends_with("^{}"))
            .map(String::as_str)
            .collect();
        return Err(Svn2GitError::Git(format!(
            "远端已有分支或标签：{}",
            names.join("、")
        )));
    }
    Ok(())
}

pub async fn check_empty(remote: &str, env: &HashMap<String, String>) -> Result<()> {
    require_empty(&Remote::new(remote, env, None)?.read_refs().await?)
}

fn require_available<'a>(
    remote: &RemoteRefs,
    targets: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    for name in targets {
        if !is_destination(name) {
            return Err(Svn2GitError::Config(format!("无效的 Git 目标名称 {name}")));
        }
        let child_prefix = format!("{name}/");
        let conflict = remote
            .get_key_value(name)
            .or_else(|| {
                name.rmatch_indices('/')
                    .find_map(|(i, _)| remote.get_key_value(&name[..i]))
            })
            .or_else(|| {
                remote
                    .range(child_prefix.clone()..)
                    .next()
                    .filter(|(r, _)| r.starts_with(&child_prefix))
            });
        if let Some((existing, _)) = conflict {
            return Err(Svn2GitError::Git(format!(
                "无法创建 {name}：远端已有 {existing}，请更换目标名称"
            )));
        }
    }
    Ok(())
}

pub async fn check_targets(
    remote: &str,
    targets: &[String],
    env: &HashMap<String, String>,
) -> Result<Vec<String>> {
    let refs = Remote::new(remote, env, None)?.read_refs().await?;
    require_available(&refs, targets.iter().map(String::as_str))?;
    Ok(refs.into_keys().filter(|r| !r.ends_with("^{}")).collect())
}

#[derive(Default)]
struct Status<'a> {
    missing: Vec<&'a VerifiedRef>,
    matching: Vec<&'a str>,
    conflicts: Vec<String>,
}

impl Status<'_> {
    fn complete(&self) -> bool {
        self.missing.is_empty() && self.conflicts.is_empty()
    }
    fn summary(&self) -> String {
        let missing: Vec<_> = self
            .missing
            .iter()
            .map(|item| item.full_ref.as_str())
            .collect();
        format!(
            "已核对 [{}]，远端缺失 [{}]，冲突 [{}]",
            self.matching.join("、"),
            missing.join("、"),
            self.conflicts.join("；")
        )
    }
}

fn compare<'a>(refs: &'a [VerifiedRef], remote: &RemoteRefs) -> Status<'a> {
    let mut status = Status::default();
    for item in refs {
        let Some(actual) = remote.get(&item.full_ref) else {
            status.missing.push(item);
            continue;
        };
        let peeled = remote
            .get(&format!("{}^{{}}", item.full_ref))
            .unwrap_or(actual);
        if actual == &item.oid && peeled == &item.commit_oid {
            status.matching.push(&item.full_ref);
        } else {
            status.conflicts.push(format!(
                "{} 预期 {}（提交 {}），实际 {}（提交 {}）",
                item.full_ref, item.oid, item.commit_oid, actual, peeled
            ));
        }
    }
    status
}

pub async fn publish(
    remote: &str,
    workdir: &Path,
    refs: &[VerifiedRef],
    env: &HashMap<String, String>,
    resume: bool,
    lfs_enabled: bool,
    progress: Progress<'_>,
) -> Result<Vec<String>> {
    let target = Remote::new(remote, env, Some(workdir))?;
    target.verify_local(refs).await?;
    let before = target.read_refs().await?;
    if !resume {
        require_available(&before, refs.iter().map(|r| r.full_ref.as_str()))?;
    }
    let initial = compare(refs, &before);
    if !initial.conflicts.is_empty() {
        return Err(Svn2GitError::Verification(initial.summary()));
    }
    if lfs_enabled || !initial.missing.is_empty() {
        // The caller must durably record this transition before we issue any writes.
        progress("push", "准备写入已核验的引用")?;
    }
    if lfs_enabled {
        progress("push", "正在上传所选历史中的 Git LFS 对象")?;
        target.upload_lfs(refs).await?;
    }
    if !initial.missing.is_empty() {
        target
            .push_missing(refs, &initial.missing, progress)
            .await?;
    }
    progress("remote_verify", "正在逐项核对远端 Git 对象")?;
    target.verify_delivery(refs).await
}

/// Recheck a previously acknowledged publication without changing refs or uploading LFS data.
pub async fn verify_remote(
    remote: &str,
    workdir: &Path,
    refs: &[VerifiedRef],
    env: &HashMap<String, String>,
) -> Result<Vec<String>> {
    let target = Remote::new(remote, env, Some(workdir))?;
    target.verify_local(refs).await?;
    target.verify_delivery(refs).await
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod tests;
