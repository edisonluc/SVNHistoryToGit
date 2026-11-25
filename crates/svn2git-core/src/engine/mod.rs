pub mod fast_import;
pub mod git_svn;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{LfsConvertedEntry, TaskConfig, TaskProgress};
use crate::credential::ResolvedSecret;
use crate::error::Result;
use crate::process::{LineCallback, ProcessRegistry};

pub type ProgressCallback = Arc<dyn Fn(TaskProgress) + Send + Sync>;
pub type CheckpointCallback = Arc<dyn Fn(u64, u64) + Send + Sync>;

pub struct EngineContext {
    pub task_id: i64,
    pub config: TaskConfig,
    pub workdir: PathBuf,
    pub authors_map: HashMap<String, (String, String)>,
    pub authors_file: PathBuf,
    pub svn_secret: Option<ResolvedSecret>,
    pub svn_auth_env: HashMap<String, String>,
    pub svn_config_dir: Option<PathBuf>,
    pub start_rev: u64,
    pub end_rev: u64,
    pub registry: ProcessRegistry,
    pub on_line: Option<LineCallback>,
    pub on_progress: Option<ProgressCallback>,
    pub on_checkpoint: Option<CheckpointCallback>,
}

#[derive(Debug, Default, Clone)]
pub struct EngineRunOutcome {
    pub last_synced_rev: u64,
    pub lfs_converted: Vec<LfsConvertedEntry>,
    pub excluded_files_count: usize,
    pub gitignore_generated: bool,
}

#[async_trait]
pub trait MigrationEngine: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run(&self, ctx: &EngineContext) -> Result<EngineRunOutcome>;
}
