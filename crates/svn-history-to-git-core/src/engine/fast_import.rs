use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::DateTime;
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};

use crate::config::{LayoutMode, LfsConvertedEntry, TaskProgress};
use crate::engine::{EngineContext, EngineRunOutcome, MigrationEngine};
use crate::error::{Result, SvnHistoryToGitError};
use crate::git_ops::sanitize_git_ref_name;
use crate::process::{configure_silent_command, run_command, CommandSpec, JobGuard};
use crate::svn::build_svn_command;
use crate::svn::dump_parser::{
    DumpRevision, DumpStreamParser, FileEntry, NodeAction, NodeKind, VersionedTreeIndex,
};

/// High-speed streaming migration engine:
/// `svnrdump dump --incremental` (or local `svnadmin dump` file / parallel `.svndump.zst` prefetch shards)
/// -> Rust zero-copy Dump v3 parser
/// -> In-memory structural-sharing VersionedTreeIndex (microsecond copyfrom resolution)
/// -> Glob exclusion filter + automatic Git LFS pointer conversion
/// -> Single long-lived `git fast-import` stdin pipe (zero working-tree checkouts during import).
pub struct FastImportEngine;

#[async_trait]
impl MigrationEngine for FastImportEngine {
    fn name(&self) -> &'static str {
        "FastImportEngine"
    }

    async fn run(&self, ctx: &EngineContext) -> Result<EngineRunOutcome> {
        let git_dir = ctx.workdir.join(".git");
        let default_branch = ctx
            .config
            .target_branch
            .as_deref()
            .filter(|b| !b.trim().is_empty())
            .unwrap_or("main")
            .to_string();

        if !git_dir.exists() {
            fs::create_dir_all(&ctx.workdir)?;
            let init = CommandSpec::new(
                "git",
                vec!["init".into(), "-b".into(), default_branch.clone()],
            )
            .cwd(&ctx.workdir);
            run_command(&ctx.registry, ctx.task_id, init, ctx.on_line.clone()).await?;

            let lp = CommandSpec::new(
                "git",
                vec!["config".into(), "core.longpaths".into(), "true".into()],
            )
            .cwd(&ctx.workdir)
            .log_output(false);
            let _ = run_command(&ctx.registry, ctx.task_id, lp, None).await;
        }

        let exclude_set = build_glob_set(&ctx.config.exclude_globs)?;
        let lfs_threshold_bytes = ctx
            .config
            .lfs_threshold_mb
            .map(|mb| mb.saturating_mul(1024 * 1024));

        // Prepare dump source(s): local dump file, Turbo Prefetch zst shards, or direct svnrdump stream
        let shard_files = if let Some(local_path) = ctx
            .config
            .local_dump_file
            .as_deref()
            .filter(|p| !p.trim().is_empty())
        {
            if let Some(cb) = &ctx.on_line {
                cb("info", &format!("FastImportEngine 使用本地 Dump 文件: {local_path}"));
            }
            vec![(PathBuf::from(local_path), false)]
        } else if ctx.config.turbo_prefetch && ctx.end_rev > ctx.start_rev {
            prefetch_dump_shards(ctx).await?
        } else {
            let temp_dump = dump_range_to_zst(
                ctx,
                ctx.start_rev.max(1),
                ctx.end_rev.max(ctx.start_rev.max(1)),
                ctx.workdir.join(".git").join("svn-history-to-git-stream.svndump.zst"),
            )
            .await?;
            vec![(temp_dump, true)]
        };

        // Run streaming parser -> `git fast-import` on a dedicated blocking thread so Tokio stays responsive
        let workdir = ctx.workdir.clone();
        let config = ctx.config.clone();
        let authors_map = ctx.authors_map.clone();
        let on_line = ctx.on_line.clone();
        let on_progress = ctx.on_progress.clone();
        let on_checkpoint = ctx.on_checkpoint.clone();
        let task_id = ctx.task_id;
        let start_rev = ctx.start_rev.max(1);
        let end_rev = ctx.end_rev.max(start_rev);
        let registry = ctx.registry.clone();
        let default_branch_clone = default_branch.clone();

        let outcome = tokio::task::spawn_blocking(move || -> Result<EngineRunOutcome> {
            run_fast_import_pipeline(
                task_id,
                &workdir,
                &config,
                &authors_map,
                &default_branch_clone,
                &shard_files,
                &exclude_set,
                lfs_threshold_bytes,
                start_rev,
                end_rev,
                &registry,
                on_line,
                on_progress,
                on_checkpoint,
            )
        })
        .await
        .map_err(|e| SvnHistoryToGitError::Other(format!("fast-import worker panicked: {e}")))??;

        // Clean up temporary shard caches
        let shard_dir = ctx.workdir.join(".git").join("svn-history-to-git-shards");
        let _ = fs::remove_dir_all(&shard_dir);
        let _ = fs::remove_file(ctx.workdir.join(".git").join("svn-history-to-git-stream.svndump.zst"));

        // Checkout default branch into working tree so user & verifier see clean HEAD
        let co = CommandSpec::new(
            "git",
            vec!["checkout".into(), "-f".into(), default_branch],
        )
        .cwd(&ctx.workdir);
        let _ = run_command(&ctx.registry, ctx.task_id, co, ctx.on_line.clone()).await;

        Ok(outcome)
    }
}

async fn prefetch_dump_shards(ctx: &EngineContext) -> Result<Vec<(PathBuf, bool)>> {
    let shard_dir = ctx.workdir.join(".git").join("svn-history-to-git-shards");
    fs::create_dir_all(&shard_dir)?;
    let window = ctx.config.window_size.max(200);
    let concurrency = ctx.config.prefetch_concurrency.clamp(2, 4);
    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));

    let mut ranges = Vec::new();
    let mut cur = ctx.start_rev.max(1);
    let end = ctx.end_rev.max(cur);
    while cur <= end {
        let w_end = (cur + window - 1).min(end);
        ranges.push((cur, w_end));
        cur = w_end + 1;
    }

    if let Some(cb) = &ctx.on_line {
        cb(
            "info",
            &format!(
                "Turbo Prefetch 已开启：共 {} 个分片，并发度 {}，流式压缩至 .svndump.zst",
                ranges.len(),
                concurrency
            ),
        );
    }

    let mut handles = Vec::with_capacity(ranges.len());
    for (idx, (r_start, r_end)) in ranges.into_iter().enumerate() {
        let permit = sem.clone().acquire_owned().await.map_err(|e| SvnHistoryToGitError::Other(e.to_string()))?;
        let out_path = shard_dir.join(format!("shard-{idx:05}-r{r_start}-{r_end}.svndump.zst"));
        let config = ctx.config.clone();
        let secret = ctx.svn_secret.clone();
        let svn_cfg = ctx.svn_config_dir.clone();
        let registry = ctx.registry.clone();
        let task_id = ctx.task_id;
        let on_line = ctx.on_line.clone();

        handles.push(tokio::spawn(async move {
            let _permit = permit;
            if out_path.exists() && fs::metadata(&out_path).map(|m| m.len() > 0).unwrap_or(false) {
                return Ok((out_path, true));
            }
            let spec = build_svn_command(
                "svnrdump",
                vec![
                    "dump".into(),
                    "-q".into(),
                    "--incremental".into(),
                    "-r".into(),
                    format!("{r_start}:{r_end}"),
                    config.effective_svn_url(),
                ],
                &config,
                secret.as_ref(),
                svn_cfg.as_deref(),
            );
            capture_command_stdout_to_zst(&registry, task_id, spec, &out_path).await?;
            if let Some(cb) = &on_line {
                cb("info", &format!("分片 r{r_start}:r{r_end} 预取完成"));
            }
            Ok::<(PathBuf, bool), SvnHistoryToGitError>((out_path, true))
        }));
    }

    let mut ordered = Vec::with_capacity(handles.len());
    for h in handles {
        let item = h
            .await
            .map_err(|e| SvnHistoryToGitError::Other(format!("prefetch task error: {e}")))??;
        ordered.push(item);
    }
    Ok(ordered)
}

async fn dump_range_to_zst(
    ctx: &EngineContext,
    r_start: u64,
    r_end: u64,
    out_path: PathBuf,
) -> Result<PathBuf> {
    if let Some(cb) = &ctx.on_line {
        cb(
            "info",
            &format!("FastImportEngine 正在拉取 svnrdump r{r_start}:r{r_end}..."),
        );
    }
    let spec = build_svn_command(
        "svnrdump",
        vec![
            "dump".into(),
            "-q".into(),
            "--incremental".into(),
            "-r".into(),
            format!("{r_start}:{r_end}"),
            ctx.config.effective_svn_url(),
        ],
        &ctx.config,
        ctx.svn_secret.as_ref(),
        ctx.svn_config_dir.as_deref(),
    );
    capture_command_stdout_to_zst(&ctx.registry, ctx.task_id, spec, &out_path).await?;
    Ok(out_path)
}

async fn capture_command_stdout_to_zst(
    registry: &crate::process::ProcessRegistry,
    task_id: i64,
    spec: CommandSpec,
    out_path: &Path,
) -> Result<()> {
    let mut cmd = configure_silent_command(&spec.program);
    cmd.args(&spec.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if spec.stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| SvnHistoryToGitError::CommandFailed {
        command: spec.display_command(),
        code: None,
        stderr: e.to_string(),
    })?;

    let job = Arc::new(JobGuard::new()?);
    if let Some(pid) = child.id() {
        let _ = job.assign_pid(pid);
    }
    registry.register(task_id, Arc::clone(&job));

    if let (Some(mut stdin), Some(data)) = (child.stdin.take(), spec.stdin_data.clone()) {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(&data).await;
            let _ = stdin.shutdown().await;
        });
    }

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| SvnHistoryToGitError::Svn("svnrdump stdout unavailable".into()))?;
    let mut stderr = child.stderr.take();

    let out_file = File::create(out_path)?;
    let mut encoder = zstd::Encoder::new(out_file, 1)?;

    let copy_task = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = futures_read_sync(&mut stdout, &mut buf)?;
            if n == 0 {
                break;
            }
            encoder.write_all(&buf[..n])?;
        }
        encoder.finish()?;
        Ok(())
    });

    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut s = Vec::new();
        if let Some(mut e) = stderr.take() {
            let _ = e.read_to_end(&mut s).await;
        }
        String::from_utf8_lossy(&s).to_string()
    });

    let copy_res = copy_task
        .await
        .map_err(|e| SvnHistoryToGitError::Other(e.to_string()))?;
    let status = child.wait().await?;
    let err_msg = stderr_task.await.unwrap_or_default();
    registry.unregister(task_id, &job);

    copy_res?;
    if !status.success() {
        return Err(SvnHistoryToGitError::CommandFailed {
            command: spec.display_command(),
            code: status.code(),
            stderr: err_msg,
        });
    }
    Ok(())
}

fn futures_read_sync(
    pipe: &mut tokio::process::ChildStdout,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    use tokio::io::AsyncReadExt;
    tokio::runtime::Handle::current().block_on(pipe.read(buf))
}

#[allow(clippy::too_many_arguments)]
fn run_fast_import_pipeline(
    task_id: i64,
    workdir: &Path,
    config: &crate::config::TaskConfig,
    authors_map: &HashMap<String, (String, String)>,
    default_branch: &str,
    shards: &[(PathBuf, bool)],
    exclude_set: &GlobSet,
    lfs_threshold_bytes: Option<u64>,
    start_rev: u64,
    end_rev: u64,
    registry: &crate::process::ProcessRegistry,
    on_line: Option<crate::process::LineCallback>,
    on_progress: Option<crate::engine::ProgressCallback>,
    on_checkpoint: Option<crate::engine::CheckpointCallback>,
) -> Result<EngineRunOutcome> {
    let mut fi_cmd = std::process::Command::new("git");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        fi_cmd.creation_flags(0x0800_0000);
    }
    let mut child = fi_cmd
        .args(["fast-import", "--quiet", "--done"])
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| SvnHistoryToGitError::Git(format!("failed to spawn git fast-import: {e}")))?;

    let job = Arc::new(JobGuard::new()?);
    let _ = job.assign_pid(child.id());
    registry.register(task_id, Arc::clone(&job));

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| SvnHistoryToGitError::Git("git fast-import stdin unavailable".into()))?;
    let mut writer = BufWriter::with_capacity(256 * 1024, stdin);

    let mut tree_index = VersionedTreeIndex::new();
    let mut next_mark: u64 = 1;
    let mut branch_marks: HashMap<String, u64> = HashMap::new();
    let mut lfs_converted: Vec<LfsConvertedEntry> = Vec::new();
    let mut lfs_extensions: HashSet<String> = HashSet::new();
    let mut excluded_files_count = 0usize;
    let mut gitignore_generated = false;
    let mut last_synced_rev = start_rev.saturating_sub(1);
    let started_at = Instant::now();
    let total_span = end_rev.saturating_sub(start_rev).max(1) + 1;

    for (path, is_zst) in shards {
        let file = File::open(path)?;
        let reader: Box<dyn std::io::BufRead> = if *is_zst {
            Box::new(BufReader::with_capacity(
                128 * 1024,
                zstd::Decoder::new(file)?,
            ))
        } else {
            Box::new(BufReader::with_capacity(128 * 1024, file))
        };

        let mut parser = DumpStreamParser::new(reader);
        while let Some(rev) = parser.next_revision()? {
            if rev.revision == 0 {
                tree_index.commit_snapshot(0);
                continue;
            }
            if rev.revision < start_rev {
                // Still track tree index for copyfrom if reading full local dump
                apply_revision_to_index_only(&rev, &mut tree_index, &mut next_mark);
                continue;
            }
            if rev.revision > end_rev {
                break;
            }

            process_revision_into_fast_import(
                &rev,
                workdir,
                config,
                authors_map,
                default_branch,
                exclude_set,
                lfs_threshold_bytes,
                &mut tree_index,
                &mut next_mark,
                &mut branch_marks,
                &mut lfs_converted,
                &mut lfs_extensions,
                &mut excluded_files_count,
                &mut gitignore_generated,
                &mut writer,
            )?;

            tree_index.commit_snapshot(rev.revision);
            last_synced_rev = rev.revision;

            if rev.revision % 200 == 0 || rev.revision == end_rev {
                writeln!(writer, "checkpoint")?;
                writer.flush()?;
                if let Some(cp) = &on_checkpoint {
                    cp(last_synced_rev, end_rev);
                }
                let done = last_synced_rev.saturating_sub(start_rev) + 1;
                let elapsed = started_at.elapsed().as_secs_f64().max(0.001);
                let rate = done as f64 / elapsed;
                let remaining = total_span.saturating_sub(done);
                if let Some(prog) = &on_progress {
                    prog(TaskProgress {
                        task_id,
                        stage: "sync".to_string(),
                        current_rev: last_synced_rev,
                        total_revs: end_rev,
                        percent: ((done as f64 / total_span as f64) * 100.0).min(100.0),
                        revs_per_sec: rate,
                        eta_seconds: if rate > 0.01 {
                            Some((remaining as f64 / rate).ceil() as u64)
                        } else {
                            None
                        },
                    });
                }
            }
        }
    }

    // If any files were converted to Git LFS, commit `.gitattributes` on the default branch
    if !lfs_extensions.is_empty() {
        let mut attrs = String::from("# Auto-generated by SVNHistoryToGit LFS pipeline\n");
        let mut sorted: Vec<String> = lfs_extensions.into_iter().collect();
        sorted.sort();
        for ext in sorted {
            attrs.push_str(&format!("{ext} filter=lfs diff=lfs merge=lfs -text\n"));
        }
        let blob_mark = next_mark;
        next_mark += 1;
        writeln!(writer, "blob\nmark :{blob_mark}\ndata {}\n{}", attrs.len(), attrs)?;
        let commit_mark = next_mark;
        let now_ts = chrono::Utc::now().timestamp();
        writeln!(writer, "commit refs/heads/{default_branch}")?;
        writeln!(writer, "mark :{commit_mark}")?;
        writeln!(writer, "committer SVNHistoryToGit <svnhistorytogit@local> {now_ts} +0000")?;
        let msg = "chore: track large binary files with Git LFS";
        writeln!(writer, "data {}\n{msg}", msg.len())?;
        if let Some(&parent) = branch_marks.get(default_branch) {
            writeln!(writer, "from :{parent}")?;
        }
        writeln!(writer, "M 100644 :{blob_mark} .gitattributes\n")?;
    }

    writeln!(writer, "done")?;
    writer.flush()?;
    drop(writer);

    let output = child.wait_with_output()?;
    registry.unregister(task_id, &job);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(SvnHistoryToGitError::Git(format!(
            "git fast-import failed (exit={:?}): {stderr}",
            output.status.code()
        )));
    }

    if let Some(cb) = &on_line {
        cb(
            "info",
            &format!(
                "FastImportEngine 流式导入完成：r{last_synced_rev}，LFS 转换 {} 个文件，规则排除 {} 个文件",
                lfs_converted.len(),
                excluded_files_count
            ),
        );
    }

    Ok(EngineRunOutcome {
        last_synced_rev,
        lfs_converted,
        excluded_files_count,
        gitignore_generated,
    })
}

fn apply_revision_to_index_only(
    rev: &DumpRevision,
    tree_index: &mut VersionedTreeIndex,
    next_mark: &mut u64,
) {
    for node in &rev.nodes {
        match node.action {
            NodeAction::Delete => tree_index.delete_path(&node.path),
            NodeAction::Replace => {
                tree_index.delete_path(&node.path);
                if let (Some(cf_path), Some(cf_rev)) = (&node.copyfrom_path, node.copyfrom_rev) {
                    tree_index.copy_from(cf_rev, cf_path, &node.path);
                }
            }
            _ => {
                if let (Some(cf_path), Some(cf_rev)) = (&node.copyfrom_path, node.copyfrom_rev) {
                    tree_index.copy_from(cf_rev, cf_path, &node.path);
                }
            }
        }
        if node.kind == NodeKind::File && node.text.is_some() {
            let mark = *next_mark;
            *next_mark += 1;
            tree_index.put_file(
                node.path.clone(),
                FileEntry {
                    mark_id: mark,
                    mode: "100644",
                },
            );
        }
    }
    tree_index.commit_snapshot(rev.revision);
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TargetRef {
    Branch(String),
    Tag(String),
}

fn map_svn_path(
    raw_path: &str,
    config: &crate::config::TaskConfig,
    default_branch: &str,
) -> Option<(TargetRef, String)> {
    let mut p = raw_path.trim_matches('/');
    if let Some(sub) = config.svn_subpath.as_deref().map(|s| s.trim_matches('/')).filter(|s| !s.is_empty()) {
        p = p.strip_prefix(sub)?.trim_start_matches('/');
    }

    match config.layout_mode {
        LayoutMode::Root => Some((TargetRef::Branch(default_branch.to_string()), p.to_string())),
        LayoutMode::Standard => {
            if p == "trunk" {
                Some((TargetRef::Branch(default_branch.to_string()), String::new()))
            } else if let Some(rest) = p.strip_prefix("trunk/") {
                Some((TargetRef::Branch(default_branch.to_string()), rest.to_string()))
            } else if let Some(rest) = p.strip_prefix("branches/") {
                let mut parts = rest.splitn(2, '/');
                let bname = sanitize_git_ref_name(parts.next()?);
                let rel = parts.next().unwrap_or("").to_string();
                Some((TargetRef::Branch(bname), rel))
            } else if let Some(rest) = p.strip_prefix("tags/") {
                let mut parts = rest.splitn(2, '/');
                let tname = sanitize_git_ref_name(parts.next()?);
                let rel = parts.next().unwrap_or("").to_string();
                Some((TargetRef::Tag(tname), rel))
            } else {
                None
            }
        }
        LayoutMode::Custom => {
            let trunk = config.custom_layout.trunk.trim_matches('/');
            if !trunk.is_empty() {
                if p == trunk {
                    return Some((TargetRef::Branch(default_branch.to_string()), String::new()));
                }
                if let Some(rest) = p.strip_prefix(&format!("{trunk}/")) {
                    return Some((TargetRef::Branch(default_branch.to_string()), rest.to_string()));
                }
            }
            for b_root in &config.custom_layout.branches {
                let br = b_root.trim_matches('/');
                if let Some(rest) = p.strip_prefix(&format!("{br}/")) {
                    let mut parts = rest.splitn(2, '/');
                    let bname = sanitize_git_ref_name(parts.next()?);
                    let rel = parts.next().unwrap_or("").to_string();
                    return Some((TargetRef::Branch(bname), rel));
                }
            }
            for t_root in &config.custom_layout.tags {
                let tr = t_root.trim_matches('/');
                if let Some(rest) = p.strip_prefix(&format!("{tr}/")) {
                    let mut parts = rest.splitn(2, '/');
                    let tname = sanitize_git_ref_name(parts.next()?);
                    let rel = parts.next().unwrap_or("").to_string();
                    return Some((TargetRef::Tag(tname), rel));
                }
            }
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn process_revision_into_fast_import<W: Write>(
    rev: &DumpRevision,
    workdir: &Path,
    config: &crate::config::TaskConfig,
    authors_map: &HashMap<String, (String, String)>,
    default_branch: &str,
    exclude_set: &GlobSet,
    lfs_threshold_bytes: Option<u64>,
    tree_index: &mut VersionedTreeIndex,
    next_mark: &mut u64,
    branch_marks: &mut HashMap<String, u64>,
    lfs_converted: &mut Vec<LfsConvertedEntry>,
    lfs_extensions: &mut HashSet<String>,
    excluded_files_count: &mut usize,
    gitignore_generated: &mut bool,
    writer: &mut W,
) -> Result<()> {
    // Group file operations per Git branch and record created tags
    let mut branch_ops: HashMap<String, Vec<String>> = HashMap::new();
    let mut created_tags: Vec<(String, Option<String>)> = Vec::new();

    for node in &rev.nodes {
        let mapped = map_svn_path(&node.path, config, default_branch);
        if node.action == NodeAction::Delete || node.action == NodeAction::Replace {
            tree_index.delete_path(&node.path);
            if let Some((TargetRef::Branch(ref b), ref rel)) = mapped {
                if !rel.is_empty() {
                    branch_ops
                        .entry(b.clone())
                        .or_default()
                        .push(format!("D {rel}"));
                }
            }
        }

        if let (Some(cf_path), Some(cf_rev)) = (&node.copyfrom_path, node.copyfrom_rev) {
            let copied = tree_index.copy_from(cf_rev, cf_path, &node.path);
            match &mapped {
                Some((TargetRef::Tag(tag_name), rel)) if rel.is_empty() => {
                    let source_branch = map_svn_path(cf_path, config, default_branch).and_then(
                        |(tref, _)| match tref {
                            TargetRef::Branch(b) => Some(b),
                            TargetRef::Tag(_) => None,
                        },
                    );
                    created_tags.push((tag_name.clone(), source_branch));
                }
                Some((TargetRef::Branch(b), _)) => {
                    for (full_copied_path, entry) in copied {
                        if let Some((TargetRef::Branch(tb), rel_p)) =
                            map_svn_path(&full_copied_path, config, default_branch)
                        {
                            if tb == *b && !rel_p.is_empty() && !exclude_set.is_match(&rel_p) {
                                branch_ops
                                    .entry(b.clone())
                                    .or_default()
                                    .push(format!("M {} :{} {}", entry.mode, entry.mark_id, rel_p));
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Convert svn:ignore directory property to .gitignore if enabled
        if config.convert_gitignore && node.kind == NodeKind::Dir {
            if let Some(ignore_bytes) = node.props.get("svn:ignore") {
                if let Some((TargetRef::Branch(b), rel)) = &mapped {
                    let gi_rel = if rel.is_empty() {
                        ".gitignore".to_string()
                    } else {
                        format!("{rel}/.gitignore")
                    };
                    let mark = *next_mark;
                    *next_mark += 1;
                    writeln!(writer, "blob\nmark :{mark}\ndata {}", ignore_bytes.len())?;
                    writer.write_all(ignore_bytes)?;
                    writeln!(writer)?;
                    branch_ops
                        .entry(b.clone())
                        .or_default()
                        .push(format!("M 100644 :{mark} {gi_rel}"));
                    *gitignore_generated = true;
                }
            }
        }

        if node.kind == NodeKind::File {
            if let Some(raw_bytes) = &node.text {
                let mode = if node.props.contains_key("svn:special") {
                    "120000"
                } else if node.props.contains_key("svn:executable") {
                    "100755"
                } else {
                    "100644"
                };

                let Some((TargetRef::Branch(b), rel)) = mapped else {
                    continue;
                };
                if rel.is_empty() {
                    continue;
                }
                if exclude_set.is_match(&rel) {
                    *excluded_files_count += 1;
                    continue;
                }

                // Check Git LFS threshold
                let payload: Vec<u8> = if let Some(threshold) = lfs_threshold_bytes {
                    if raw_bytes.len() as u64 >= threshold {
                        let (oid, pointer) = store_lfs_object(workdir, raw_bytes)?;
                        lfs_converted.push(LfsConvertedEntry {
                            path: rel.clone(),
                            oid,
                            size_bytes: raw_bytes.len() as u64,
                        });
                        if let Some(ext) = Path::new(&rel).extension().and_then(|e| e.to_str()) {
                            lfs_extensions.insert(format!("*.{ext}"));
                        } else {
                            lfs_extensions.insert(rel.clone());
                        }
                        pointer.into_bytes()
                    } else {
                        raw_bytes.clone()
                    }
                } else {
                    raw_bytes.clone()
                };

                let mark = *next_mark;
                *next_mark += 1;
                writeln!(writer, "blob\nmark :{mark}\ndata {}", payload.len())?;
                writer.write_all(&payload)?;
                writeln!(writer)?;

                tree_index.put_file(
                    node.path.clone(),
                    FileEntry {
                        mark_id: mark,
                        mode,
                    },
                );
                branch_ops
                    .entry(b)
                    .or_default()
                    .push(format!("M {mode} :{mark} {rel}"));
            }
        }
    }

    let (author_name, author_email) = authors_map
        .get(&rev.author)
        .cloned()
        .unwrap_or_else(|| {
            let domain = config
                .default_email_domain
                .as_deref()
                .filter(|d| !d.is_empty())
                .unwrap_or("svn.local");
            (rev.author.clone(), format!("{}@{domain}", rev.author))
        });
    let unix_ts = parse_svn_timestamp(&rev.date);
    let commit_msg = if rev.log.trim().is_empty() {
        format!("SVN revision r{}", rev.revision)
    } else {
        rev.log.clone()
    };

    for (branch, ops) in branch_ops {
        if ops.is_empty() {
            continue;
        }
        let commit_mark = *next_mark;
        *next_mark += 1;

        writeln!(writer, "commit refs/heads/{branch}")?;
        writeln!(writer, "mark :{commit_mark}")?;
        writeln!(writer, "author {author_name} <{author_email}> {unix_ts} +0000")?;
        writeln!(writer, "committer {author_name} <{author_email}> {unix_ts} +0000")?;
        writeln!(writer, "data {}", commit_msg.len())?;
        writer.write_all(commit_msg.as_bytes())?;
        writeln!(writer)?;
        if let Some(&parent) = branch_marks.get(&branch) {
            writeln!(writer, "from :{parent}")?;
        }
        for op in ops {
            writeln!(writer, "{op}")?;
        }
        writeln!(writer)?;
        branch_marks.insert(branch, commit_mark);
    }

    for (tag_name, src_branch) in created_tags {
        let target_mark = src_branch
            .as_deref()
            .and_then(|b| branch_marks.get(b).copied())
            .or_else(|| branch_marks.get(default_branch).copied());
        if let Some(mark) = target_mark {
            writeln!(writer, "tag {tag_name}")?;
            writeln!(writer, "from :{mark}")?;
            writeln!(writer, "tagger {author_name} <{author_email}> {unix_ts} +0000")?;
            writeln!(writer, "data {}", commit_msg.len())?;
            writer.write_all(commit_msg.as_bytes())?;
            writeln!(writer)?;
        }
    }

    Ok(())
}

fn store_lfs_object(workdir: &Path, bytes: &[u8]) -> Result<(String, String)> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let oid = hex::encode(hasher.finalize());
    let obj_dir = workdir
        .join(".git")
        .join("lfs")
        .join("objects")
        .join(&oid[0..2])
        .join(&oid[2..4]);
    fs::create_dir_all(&obj_dir)?;
    let obj_path = obj_dir.join(&oid);
    if !obj_path.exists() {
        fs::write(&obj_path, bytes)?;
    }
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
        bytes.len()
    );
    Ok((oid, pointer))
}

fn parse_svn_timestamp(date_str: &str) -> i64 {
    DateTime::parse_from_rfc3339(date_str.trim())
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|_| chrono::Utc::now().timestamp())
}

fn build_glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let p = pat.trim();
        if !p.is_empty() {
            let glob = Glob::new(p)
                .map_err(|e| SvnHistoryToGitError::Config(format!("invalid exclude glob '{p}': {e}")))?;
            builder.add(glob);
        }
    }
    builder
        .build()
        .map_err(|e| SvnHistoryToGitError::Config(format!("failed to compile exclude globs: {e}")))
}
