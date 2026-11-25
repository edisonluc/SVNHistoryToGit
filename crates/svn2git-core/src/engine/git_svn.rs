use std::fs;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::config::{LayoutMode, TaskProgress};
use crate::engine::{EngineContext, EngineRunOutcome, MigrationEngine};
use crate::error::Result;
use crate::git_ops::convert_svn_refs_batch;
use crate::process::{run_command, CommandSpec};
use crate::svn::inspector::fetch_svn_ignore_rules;

/// Resumable windowed `git svn` migration engine.
/// Splits large repositories into revision windows, persists checkpoints per window,
/// computes real-time speed/ETA, and retries transient network errors with exponential backoff.
pub struct GitSvnEngine;

#[async_trait]
impl MigrationEngine for GitSvnEngine {
    fn name(&self) -> &'static str {
        "GitSvnEngine"
    }

    async fn run(&self, ctx: &EngineContext) -> Result<EngineRunOutcome> {
        let git_dir = ctx.workdir.join(".git");
        let is_resume = git_dir.exists();

        if !is_resume {
            fs::create_dir_all(&ctx.workdir)?;
            let mut init_args = vec!["svn".into(), "init".into()];
            match ctx.config.layout_mode {
                LayoutMode::Standard => {
                    init_args.push("--stdlayout".into());
                }
                LayoutMode::Custom => {
                    if !ctx.config.custom_layout.trunk.is_empty() {
                        init_args.push(format!("--trunk={}", ctx.config.custom_layout.trunk));
                    }
                    for b in &ctx.config.custom_layout.branches {
                        init_args.push(format!("--branches={b}"));
                    }
                    for t in &ctx.config.custom_layout.tags {
                        init_args.push(format!("--tags={t}"));
                    }
                }
                LayoutMode::Root => {}
            }
            if let Some(user) = ctx
                .svn_secret
                .as_ref()
                .map(|s| s.username.as_str())
                .or(ctx.config.svn_username.as_deref())
                .filter(|u| !u.trim().is_empty())
            {
                init_args.push("--username".into());
                init_args.push(user.to_string());
            }
            init_args.push(ctx.config.effective_svn_url());
            init_args.push(ctx.workdir.to_string_lossy().to_string());

            let init_spec = CommandSpec::new("git", init_args).envs(ctx.svn_auth_env.clone());
            run_command(&ctx.registry, ctx.task_id, init_spec, ctx.on_line.clone()).await?;

            // Enable Windows long paths immediately
            let lp_spec = CommandSpec::new(
                "git",
                vec!["config".into(), "core.longpaths".into(), "true".into()],
            )
            .cwd(&ctx.workdir)
            .log_output(false);
            let _ = run_command(&ctx.registry, ctx.task_id, lp_spec, None).await;
        } else if let Some(cb) = &ctx.on_line {
            cb(
                "info",
                &format!(
                    "检测到已有工作区，从断点 r{} 继续执行分窗口 git svn fetch",
                    ctx.start_rev
                ),
            );
        }

        let window_size = ctx.config.window_size.max(50);
        let start_rev = ctx.start_rev.max(1);
        let end_rev = ctx.end_rev.max(start_rev);
        let total_span = end_rev.saturating_sub(start_rev) + 1;
        let started_at = Instant::now();

        let mut cursor = start_rev;
        let mut last_synced = start_rev.saturating_sub(1);

        while cursor <= end_rev {
            let win_end = (cursor + window_size - 1).min(end_rev);
            let mut attempt = 0u32;

            loop {
                attempt += 1;
                let mut fetch_args = vec![
                    "svn".into(),
                    "fetch".into(),
                    "--no-auth-cache".into(),
                    "--authors-file".into(),
                    ctx.authors_file.to_string_lossy().to_string(),
                    "-r".into(),
                    format!("{cursor}:{win_end}"),
                ];
                if let Some(user) = ctx
                    .svn_secret
                    .as_ref()
                    .map(|s| s.username.as_str())
                    .or(ctx.config.svn_username.as_deref())
                    .filter(|u| !u.trim().is_empty())
                {
                    fetch_args.push("--username".into());
                    fetch_args.push(user.to_string());
                }

                let fetch_spec = CommandSpec::new("git", fetch_args)
                    .cwd(&ctx.workdir)
                    .envs(ctx.svn_auth_env.clone());

                match run_command(&ctx.registry, ctx.task_id, fetch_spec, ctx.on_line.clone()).await
                {
                    Ok(_) => break,
                    Err(err) if attempt < 3 => {
                        let backoff_secs = 2u64.pow(attempt);
                        if let Some(cb) = &ctx.on_line {
                            cb(
                                "warn",
                                &format!(
                                    "窗口 r{cursor}:r{win_end} 同步失败（第 {attempt} 次），{backoff_secs}s 后重试: {err}"
                                ),
                            );
                        }
                        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    }
                    Err(err) => return Err(err),
                }
            }

            last_synced = win_end;
            if let Some(cp) = &ctx.on_checkpoint {
                cp(last_synced, end_rev);
            }

            let done = last_synced.saturating_sub(start_rev) + 1;
            let elapsed = started_at.elapsed().as_secs_f64().max(0.001);
            let rate = done as f64 / elapsed;
            let remaining = total_span.saturating_sub(done);
            let eta = if rate > 0.01 {
                Some((remaining as f64 / rate).ceil() as u64)
            } else {
                None
            };
            if let Some(prog) = &ctx.on_progress {
                prog(TaskProgress {
                    task_id: ctx.task_id,
                    stage: "sync".to_string(),
                    current_rev: last_synced,
                    total_revs: end_rev,
                    percent: ((done as f64 / total_span as f64) * 100.0).min(100.0),
                    revs_per_sec: rate,
                    eta_seconds: eta,
                });
            }

            cursor = win_end + 1;
        }

        // Ensure local working branch is checked out from svn remote trunk/git-svn
        let checkout_candidates = ["refs/remotes/origin/trunk", "refs/remotes/trunk", "refs/remotes/git-svn"];
        let target_local = ctx
            .config
            .target_branch
            .as_deref()
            .filter(|b| !b.trim().is_empty())
            .unwrap_or("main");
        for cand in checkout_candidates {
            let check = CommandSpec::new(
                "git",
                vec!["rev-parse".into(), "--verify".into(), cand.to_string()],
            )
            .cwd(&ctx.workdir)
            .log_output(false);
            if run_command(&ctx.registry, ctx.task_id, check, None).await.is_ok() {
                let co = CommandSpec::new(
                    "git",
                    vec![
                        "checkout".into(),
                        "-B".into(),
                        target_local.to_string(),
                        cand.to_string(),
                    ],
                )
                .cwd(&ctx.workdir);
                let _ = run_command(&ctx.registry, ctx.task_id, co, ctx.on_line.clone()).await;
                break;
            }
        }

        // Fast batch conversion of branches and Annotated Tags via gix + update-ref --stdin
        if ctx.config.layout_mode != LayoutMode::Root {
            let (b_cnt, t_cnt) =
                convert_svn_refs_batch(&ctx.registry, ctx.task_id, &ctx.workdir, ctx.on_line.clone())
                    .await?;
            if let Some(cb) = &ctx.on_line {
                cb(
                    "info",
                    &format!("原生 gix 批量引用转换完成：{b_cnt} 个分支，{t_cnt} 个 Annotated Tags"),
                );
            }
        }

        // Optional svn:ignore -> .gitignore generation
        let mut gitignore_generated = false;
        if ctx.config.convert_gitignore {
            if let Ok(rules) = fetch_svn_ignore_rules(
                &ctx.registry,
                ctx.task_id,
                &ctx.config,
                ctx.svn_secret.as_ref(),
                ctx.svn_config_dir.as_deref(),
            )
            .await
            {
                if !rules.trim().is_empty() {
                    let gi_path = ctx.workdir.join(".gitignore");
                    let mut existing = fs::read_to_string(&gi_path).unwrap_or_default();
                    if !existing.contains("Auto-generated from svn:ignore by SVN2Git") {
                        if !existing.is_empty() && !existing.ends_with('\n') {
                            existing.push('\n');
                        }
                        existing.push_str(&rules);
                        fs::write(&gi_path, existing)?;
                        let add = CommandSpec::new("git", vec!["add".into(), ".gitignore".into()])
                            .cwd(&ctx.workdir)
                            .log_output(false);
                        let _ = run_command(&ctx.registry, ctx.task_id, add, None).await;
                        let commit = CommandSpec::new(
                            "git",
                            vec![
                                "-c".into(),
                                "user.name=SVN2Git".into(),
                                "-c".into(),
                                "user.email=svn2git@local".into(),
                                "commit".into(),
                                "-m".into(),
                                "chore: convert svn:ignore to .gitignore".into(),
                            ],
                        )
                        .cwd(&ctx.workdir)
                        .log_output(false);
                        let _ = run_command(&ctx.registry, ctx.task_id, commit, None).await;
                        gitignore_generated = true;
                    }
                }
            }
        }

        Ok(EngineRunOutcome {
            last_synced_rev: last_synced,
            lfs_converted: Vec::new(),
            excluded_files_count: 0,
            gitignore_generated,
        })
    }
}
