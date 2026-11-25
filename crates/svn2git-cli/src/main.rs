mod desktop;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use svn2git_core::config::TaskConfig;
use svn2git_core::credential::{execute_internal_askpass, ENV_ASKPASS_ADDR};
use svn2git_core::git_ops::report_export::{
    export_report_html, export_report_json, export_report_markdown,
};
use svn2git_core::orchestrator::TaskOrchestrator;
use svn2git_core::storage::Storage;

#[derive(Parser)]
#[command(name = "svn2git-cli", version, about = "SVN2Git headless CLI & native AskPass helper")]
struct Cli {
    /// Internal ultra-fast AskPass mode (<2ms) invoked by git / git-svn
    #[arg(long = "internal-askpass")]
    internal_askpass: Option<String>,

    /// Data directory for SQLite metadata and NDJSON logs
    #[arg(long, default_value = ".data")]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Execute one desktop request from stdin and stream NDJSON events to stdout.
    Desktop,
    /// Check local toolchain availability (svn, svnrdump, git, git-svn, git-lfs)
    Doctor,
    /// Run a migration or incremental delta sync from a TOML/JSON config file
    Migrate {
        #[arg(short, long)]
        config: PathBuf,
        #[arg(long)]
        delta: bool,
    },
    /// Export an audit report for a task
    ExportReport {
        #[arg(long)]
        task_id: i64,
        #[arg(long, default_value = "md")]
        format: String,
    },
}

fn main() -> Result<()> {
    // Fast-path: when invoked directly by git via GIT_ASKPASS=".../svn2git-cli.exe" "<prompt>"
    if std::env::var_os(ENV_ASKPASS_ADDR).is_some() {
        let raw_args: Vec<String> = std::env::args().skip(1).collect();
        if let Some(first) = raw_args.first() {
            let prompt = if first == "--internal-askpass" {
                raw_args.get(1).cloned().unwrap_or_default()
            } else {
                first.clone()
            };
            let secret = execute_internal_askpass(&prompt)?;
            println!("{secret}");
            return Ok(());
        }
    }

    let cli = Cli::parse();
    if let Some(prompt) = cli.internal_askpass {
        let secret = execute_internal_askpass(&prompt)?;
        println!("{secret}");
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    if matches!(cli.command, Some(Commands::Desktop)) {
        return desktop::serve(&cli.data_dir).await;
    }
    let storage = Storage::open(&cli.data_dir)?;
    let orch = TaskOrchestrator::new(storage.clone(), 2)?;

    match cli.command {
        Some(Commands::Desktop) => unreachable!(),
        Some(Commands::Doctor) => {
            let report = orch.doctor().await;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Some(Commands::Migrate { config, delta }) => {
            let content = std::fs::read_to_string(&config)?;
            let cfg = if config.extension().and_then(|e| e.to_str()) == Some("json") {
                TaskConfig::from_json(&content)?
            } else {
                TaskConfig::from_toml(&content)?
            };
            let task = storage.create_task(&cfg)?;
            println!("Created task #{} ({})", task.id, task.name);

            let mut log_rx = storage.logs.subscribe();
            let tid = task.id;
            let printer = tokio::spawn(async move {
                while let Ok(entry) = log_rx.recv().await {
                    if entry.task_id == tid {
                        println!("[{}] {}", entry.level.to_uppercase(), entry.message);
                    }
                }
            });

            orch.start_task(task.id, delta)?;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                if let Some(cur) = storage.get_task(task.id)? {
                    match cur.status {
                        svn2git_core::config::TaskStatus::Completed => {
                            println!("Migration completed successfully.");
                            break;
                        }
                        svn2git_core::config::TaskStatus::Failed
                        | svn2git_core::config::TaskStatus::Cancelled => {
                            anyhow::bail!(
                                "Migration ended with status {:?}: {}",
                                cur.status,
                                cur.error.unwrap_or_default()
                            );
                        }
                        _ => {}
                    }
                }
            }
            printer.abort();
        }
        Some(Commands::ExportReport { task_id, format }) => {
            let task = storage
                .get_task(task_id)?
                .ok_or_else(|| anyhow::anyhow!("task {task_id} not found"))?;
            let out = match format.as_str() {
                "json" => export_report_json(&task)?,
                "html" => export_report_html(&task),
                _ => export_report_markdown(&task),
            };
            println!("{out}");
        }
        None => {
            println!("Use --help to see available commands (doctor, migrate, export-report).");
        }
    }

    Ok(())
}
