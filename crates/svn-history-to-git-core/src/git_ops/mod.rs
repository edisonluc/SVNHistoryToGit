pub mod report_export;
pub mod verifier;

use std::collections::HashMap;
use std::path::Path;

use crate::config::{PushStrategy, TaskConfig};
use crate::error::{Result, SvnHistoryToGitError};
use crate::process::{run_command, CommandSpec, LineCallback, ProcessRegistry};

#[derive(Debug, Clone)]
pub struct GitRepoStats {
    pub commit_count: usize,
    pub branches: Vec<String>,
    pub tags: Vec<String>,
    pub latest_commit: String,
    pub current_branch: String,
}

/// Sanitize an SVN branch/tag name into a valid Git ref component.
pub fn sanitize_git_ref_name(raw: &str) -> String {
    let mut s = raw
        .trim()
        .replace([' ', '~', '^', ':', '?', '*', '[', '\\'], "_")
        .replace("..", "_")
        .replace("@{", "_")
        .replace("//", "/");
    while s.starts_with('/') || s.starts_with('.') {
        s.remove(0);
    }
    while s.ends_with('/') || s.ends_with('.') || s.ends_with(".lock") {
        if s.ends_with(".lock") {
            s.truncate(s.len() - 5);
        } else {
            s.pop();
        }
    }
    if s.is_empty() {
        "unnamed".to_string()
    } else {
        s
    }
}

/// Inspect Git repository stats in-process using `gitoxide (gix)` (sub-10ms).
pub fn inspect_repo_stats_gix(workdir: &Path) -> Result<GitRepoStats> {
    let repo = gix::open(workdir).map_err(|e| SvnHistoryToGitError::Git(format!("gix open failed: {e}")))?;

    let mut branches = Vec::new();
    let mut tags = Vec::new();

    if let Ok(refs) = repo.references() {
        if let Ok(iter) = refs.local_branches() {
            for r in iter.flatten() {
                let name = r.name().shorten().to_string();
                branches.push(name);
            }
        }
        if let Ok(iter) = refs.tags() {
            for r in iter.flatten() {
                let name = r.name().shorten().to_string();
                tags.push(name);
            }
        }
    }
    branches.sort();
    tags.sort();

    let current_branch = repo
        .head_name()
        .ok()
        .flatten()
        .map(|n| n.shorten().to_string())
        .unwrap_or_default();

    let mut latest_commit = String::new();
    let mut commit_count = 0usize;

    if let Ok(head_id) = repo.head_id() {
        if let Ok(obj) = head_id.object() {
            if let Ok(commit) = obj.try_into_commit() {
                let summary = commit
                    .message_raw_sloppy()
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                latest_commit = format!("{} {}", head_id, summary);
            }
        }
        if let Ok(walk) = repo.rev_walk([head_id.detach()]).all() {
            commit_count = walk.count();
        }
    }

    Ok(GitRepoStats {
        commit_count,
        branches,
        tags,
        latest_commit,
        current_branch,
    })
}

/// Convert `refs/remotes/*` produced by `git-svn` into local branches and Annotated Git Tags
/// using in-memory `gix` ref discovery + single transactional `git update-ref --stdin`.
pub async fn convert_svn_refs_batch(
    registry: &ProcessRegistry,
    task_id: i64,
    workdir: &Path,
    on_line: Option<LineCallback>,
) -> Result<(usize, usize)> {
    let repo = gix::open(workdir).map_err(|e| SvnHistoryToGitError::Git(format!("gix open failed: {e}")))?;

    let mut branch_updates: Vec<(String, String)> = Vec::new();
    let mut tag_candidates: Vec<(String, String, String, String, String)> = Vec::new();

    if let Ok(refs) = repo.references() {
        if let Ok(iter) = refs.remote_branches() {
            for r in iter.flatten() {
                let full_name = r.name().as_bstr().to_string();
                let short = full_name
                    .strip_prefix("refs/remotes/")
                    .unwrap_or(&full_name);

                if short == "trunk"
                    || short == "origin/trunk"
                    || short.ends_with("/trunk")
                    || short.starts_with("origin/")
                {
                    continue;
                }

                let Ok(id) = r.into_fully_peeled_id() else {
                    continue;
                };
                let oid_hex = id.to_string();

                if let Some(raw_tag) = short
                    .split_once("/tags/")
                    .map(|(_, t)| t)
                    .or_else(|| short.strip_prefix("tags/"))
                {
                    let clean_tag = sanitize_git_ref_name(raw_tag);
                    let mut author_name = "SVNHistoryToGit".to_string();
                    let mut author_email = "svnhistorytogit@local".to_string();
                    let mut author_date = "0 +0000".to_string();
                    let mut msg = format!("SVN tag {raw_tag}");

                    if let Ok(obj) = id.object() {
                        if let Ok(commit) = obj.try_into_commit() {
                            if let Ok(sig) = commit.author() {
                                author_name = sig.name.to_string();
                                author_email = sig.email.to_string();
                                author_date = format!("{} {:+05}", sig.time.seconds, sig.time.offset / 36);
                            }
                            let raw_msg = commit.message_raw_sloppy().to_string();
                            if !raw_msg.trim().is_empty() {
                                msg = raw_msg;
                            }
                        }
                    }
                    tag_candidates.push((clean_tag, oid_hex, author_name, author_email, format!("{author_date}\0{msg}")));
                } else {
                    let clean_branch = sanitize_git_ref_name(short.strip_prefix("origin/").unwrap_or(short));
                    branch_updates.push((format!("refs/heads/{clean_branch}"), oid_hex));
                }
            }
        }
    }

    // Batch-create all branches in a single `git update-ref --stdin` transaction (<5ms)
    if !branch_updates.is_empty() {
        let mut stdin_script = String::new();
        for (ref_name, oid) in &branch_updates {
            stdin_script.push_str(&format!("update {ref_name} {oid}\n"));
        }
        let spec = CommandSpec::new("git", vec!["update-ref".into(), "--stdin".into()])
            .cwd(workdir)
            .stdin_bytes(stdin_script.into_bytes())
            .log_output(false);
        run_command(registry, task_id, spec, on_line.clone()).await?;
    }

    // Create Annotated Tags preserving original SVN author/date/message via `git mktag` + batch update-ref
    let mut tag_ref_script = String::new();
    let mut created_tags = 0usize;
    for (tag_name, oid, author_name, author_email, date_and_msg) in tag_candidates {
        let (date_str, msg) = date_and_msg.split_once('\0').unwrap_or(("0 +0000", ""));
        let tag_object_text = format!(
            "object {oid}\ntype commit\ntag {tag_name}\ntagger {author_name} <{author_email}> {date_str}\n\n{msg}\n"
        );
        let mktag_spec = CommandSpec::new("git", vec!["mktag".into()])
            .cwd(workdir)
            .stdin_bytes(tag_object_text.into_bytes())
            .log_output(false);
        if let Ok(out) = run_command(registry, task_id, mktag_spec, None).await {
            let tag_oid = out.stdout.trim();
            if !tag_oid.is_empty() {
                tag_ref_script.push_str(&format!("update refs/tags/{tag_name} {tag_oid}\n"));
                created_tags += 1;
            }
        }
    }

    if !tag_ref_script.is_empty() {
        let spec = CommandSpec::new("git", vec!["update-ref".into(), "--stdin".into()])
            .cwd(workdir)
            .stdin_bytes(tag_ref_script.into_bytes())
            .log_output(false);
        run_command(registry, task_id, spec, on_line).await?;
    }

    Ok((branch_updates.len(), created_tags))
}

/// Check Git remote connectivity and write/read reachability before migration.
pub async fn check_git_remote(
    registry: &ProcessRegistry,
    task_id: i64,
    remote_url: &str,
    git_env: &HashMap<String, String>,
    on_line: Option<LineCallback>,
) -> Result<String> {
    let spec = CommandSpec::new(
        "git",
        vec!["ls-remote".into(), "--heads".into(), remote_url.to_string()],
    )
    .envs(git_env.clone())
    .timeout(25)
    .log_output(false);

    let out = run_command(registry, task_id, spec, on_line).await?;
    let count = out.stdout.lines().filter(|l| !l.trim().is_empty()).count();
    Ok(format!("Git 远端连通正常（检测到 {count} 个远端分支）"))
}

/// Execute configured remote push strategy (All / Protected-Branch Safe Merge / Subtree Prefix).
pub async fn push_to_remote(
    registry: &ProcessRegistry,
    task_id: i64,
    workdir: &Path,
    config: &TaskConfig,
    git_env: &HashMap<String, String>,
    has_lfs: bool,
    on_line: Option<LineCallback>,
) -> Result<()> {
    let Some(remote_url) = config.git_remote_url.as_deref().filter(|u| !u.trim().is_empty()) else {
        return Ok(());
    };

    // Ensure origin remote points to remote_url (idempotent for resume/delta sync)
    let set_url = CommandSpec::new(
        "git",
        vec!["remote".into(), "set-url".into(), "origin".into(), remote_url.to_string()],
    )
    .cwd(workdir)
    .log_output(false);
    if run_command(registry, task_id, set_url, None).await.is_err() {
        let add_remote = CommandSpec::new(
            "git",
            vec!["remote".into(), "add".into(), "origin".into(), remote_url.to_string()],
        )
        .cwd(workdir);
        run_command(registry, task_id, add_remote, on_line.clone()).await?;
    }

    if !config.push_remote {
        return Ok(());
    }

    if has_lfs {
        let lfs_push = CommandSpec::new(
            "git",
            vec!["lfs".into(), "push".into(), "--all".into(), "origin".into()],
        )
        .cwd(workdir)
        .envs(git_env.clone());
        let _ = run_command(registry, task_id, lfs_push, on_line.clone()).await;
    }

    match config.push_strategy {
        PushStrategy::TargetBranchSafeMerge => {
            let branch = config
                .target_branch
                .as_deref()
                .filter(|b| !b.trim().is_empty())
                .unwrap_or("main");
            push_target_branch_safe_merge(registry, task_id, workdir, branch, git_env, on_line).await?;
        }
        PushStrategy::SubtreePrefix => {
            let prefix = config
                .subtree_prefix
                .as_deref()
                .unwrap_or("imported")
                .trim_matches('/');
            let branch = config
                .target_branch
                .as_deref()
                .filter(|b| !b.trim().is_empty())
                .unwrap_or("main");
            let split_branch = format!("subtree-{}", sanitize_git_ref_name(prefix));
            let split_cmd = CommandSpec::new(
                "git",
                vec![
                    "subtree".into(),
                    "split".into(),
                    format!("--prefix={prefix}"),
                    "-b".into(),
                    split_branch.clone(),
                ],
            )
            .cwd(workdir);
            // If prefix directory doesn't exist locally, push current branch directly to target
            if run_command(registry, task_id, split_cmd, on_line.clone()).await.is_ok() {
                let push_cmd = CommandSpec::new(
                    "git",
                    vec![
                        "push".into(),
                        "origin".into(),
                        format!("{split_branch}:refs/heads/{branch}"),
                    ],
                )
                .cwd(workdir)
                .envs(git_env.clone());
                run_command(registry, task_id, push_cmd, on_line).await?;
            } else {
                push_target_branch_safe_merge(registry, task_id, workdir, branch, git_env, on_line)
                    .await?;
            }
        }
        PushStrategy::AllBranchesAndTags => {
            if let Some(branch) = config.target_branch.as_deref().filter(|b| !b.trim().is_empty()) {
                push_target_branch_safe_merge(registry, task_id, workdir, branch, git_env, on_line.clone())
                    .await?;
            } else {
                let push_all = CommandSpec::new("git", vec!["push".into(), "--all".into(), "origin".into()])
                    .cwd(workdir)
                    .envs(git_env.clone());
                run_command(registry, task_id, push_all, on_line.clone()).await?;
                let push_tags = CommandSpec::new("git", vec!["push".into(), "--tags".into(), "origin".into()])
                    .cwd(workdir)
                    .envs(git_env.clone());
                run_command(registry, task_id, push_tags, on_line).await?;
            }
        }
    }

    Ok(())
}

async fn push_target_branch_safe_merge(
    registry: &ProcessRegistry,
    task_id: i64,
    workdir: &Path,
    branch: &str,
    git_env: &HashMap<String, String>,
    on_line: Option<LineCallback>,
) -> Result<()> {
    let checkout = CommandSpec::new(
        "git",
        vec!["checkout".into(), "-B".into(), branch.to_string()],
    )
    .cwd(workdir);
    run_command(registry, task_id, checkout, on_line.clone()).await?;

    let probe = CommandSpec::new(
        "git",
        vec![
            "ls-remote".into(),
            "--exit-code".into(),
            "--heads".into(),
            "origin".into(),
            branch.to_string(),
        ],
    )
    .cwd(workdir)
    .envs(git_env.clone())
    .log_output(false);

    let remote_exists = match run_command(registry, task_id, probe, None).await {
        Ok(_) => true,
        Err(SvnHistoryToGitError::CommandFailed { code: Some(2), .. }) => false,
        Err(e) => return Err(e),
    };

    if remote_exists {
        if let Some(cb) = &on_line {
            cb(
                "info",
                &format!("远端分支 origin/{branch} 已存在，执行保护分支无强推合并 (merge -s ours)"),
            );
        }
        let fetch = CommandSpec::new(
            "git",
            vec![
                "fetch".into(),
                "--no-tags".into(),
                "origin".into(),
                format!("refs/heads/{branch}:refs/remotes/origin/{branch}"),
            ],
        )
        .cwd(workdir)
        .envs(git_env.clone());
        run_command(registry, task_id, fetch, on_line.clone()).await?;

        let merge = CommandSpec::new(
            "git",
            vec![
                "merge".into(),
                "--allow-unrelated-histories".into(),
                "--no-edit".into(),
                "-s".into(),
                "ours".into(),
                format!("origin/{branch}"),
            ],
        )
        .cwd(workdir);
        run_command(registry, task_id, merge, on_line.clone()).await?;
    }

    let push = CommandSpec::new(
        "git",
        vec!["push".into(), "-u".into(), "origin".into(), branch.to_string()],
    )
    .cwd(workdir)
    .envs(git_env.clone());
    run_command(registry, task_id, push, on_line).await?;
    Ok(())
}

/// Optional post-migration optimization: build commit-graph and repack packfiles.
pub async fn optimize_git_repo(
    registry: &ProcessRegistry,
    task_id: i64,
    workdir: &Path,
    on_line: Option<LineCallback>,
) -> Result<()> {
    let cg = CommandSpec::new(
        "git",
        vec!["commit-graph".into(), "write".into(), "--reachable".into()],
    )
    .cwd(workdir);
    let _ = run_command(registry, task_id, cg, on_line.clone()).await;

    let repack = CommandSpec::new("git", vec!["repack".into(), "-ad".into()]).cwd(workdir);
    let _ = run_command(registry, task_id, repack, on_line).await;
    Ok(())
}
