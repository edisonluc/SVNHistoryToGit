use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Result, Svn2GitError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Draft,
    Prechecking,
    PrecheckFailed,
    AuthorMapping,
    Ready,
    Running,
    Paused,
    Verifying,
    Completed,
    SyncingDelta,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Prechecking => "prechecking",
            Self::PrecheckFailed => "precheck_failed",
            Self::AuthorMapping => "author_mapping",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Verifying => "verifying",
            Self::Completed => "completed",
            Self::SyncingDelta => "syncing_delta",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "draft" => Self::Draft,
            "prechecking" => Self::Prechecking,
            "precheck_failed" => Self::PrecheckFailed,
            "author_mapping" => Self::AuthorMapping,
            "ready" => Self::Ready,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "verifying" => Self::Verifying,
            "completed" => Self::Completed,
            "syncing_delta" => Self::SyncingDelta,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Draft,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    /// Streaming svnrdump -> in-memory tree index -> git fast-import
    #[default]
    FastImport,
    /// Windowed resumable git svn fetch
    GitSvn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LayoutMode {
    #[default]
    Standard,
    Root,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CustomLayoutSpec {
    #[serde(default = "default_trunk")]
    pub trunk: String,
    #[serde(default = "default_branches")]
    pub branches: Vec<String>,
    #[serde(default = "default_tags")]
    pub tags: Vec<String>,
}

fn default_trunk() -> String {
    "trunk".to_string()
}
fn default_branches() -> Vec<String> {
    vec!["branches".to_string()]
}
fn default_tags() -> Vec<String> {
    vec!["tags".to_string()]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PushStrategy {
    /// Push all branches and tags (--all + --tags)
    #[default]
    AllBranchesAndTags,
    /// Push target_branch; if remote branch exists, fetch + merge -s ours (no force push)
    TargetBranchSafeMerge,
    /// Mount migrated history into a monorepo subdirectory prefix
    SubtreePrefix,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfig {
    #[serde(default)]
    pub name: String,
    pub svn_url: String,
    #[serde(default)]
    pub svn_subpath: Option<String>,
    /// Reference to a saved SVN credential profile ID in OS Keyring
    #[serde(default)]
    pub svn_credential_id: Option<String>,
    /// Optional inline username (password is never persisted in DB/config)
    #[serde(default)]
    pub svn_username: Option<String>,
    #[serde(default)]
    pub trust_self_signed_cert: bool,
    /// Optional local svnadmin dump file path (enables ultra-fast offline FastImport)
    #[serde(default)]
    pub local_dump_file: Option<String>,

    #[serde(default)]
    pub git_remote_url: Option<String>,
    #[serde(default)]
    pub git_credential_id: Option<String>,
    #[serde(default)]
    pub target_branch: Option<String>,
    #[serde(default)]
    pub subtree_prefix: Option<String>,
    #[serde(default)]
    pub push_remote: bool,
    #[serde(default)]
    pub push_strategy: PushStrategy,

    pub workdir: String,
    #[serde(default)]
    pub authors_file: Option<String>,
    #[serde(default)]
    pub default_email_domain: Option<String>,

    #[serde(default)]
    pub start_revision: Option<u64>,
    #[serde(default)]
    pub end_revision: Option<u64>,
    #[serde(default)]
    pub layout_mode: LayoutMode,
    #[serde(default)]
    pub custom_layout: CustomLayoutSpec,
    #[serde(default)]
    pub engine: EngineKind,

    /// Revision batch window size for GitSvnEngine and Turbo Prefetch
    #[serde(default = "default_window_size")]
    pub window_size: u64,
    /// Enable parallel prefetch shards (2..=4) for FastImportEngine
    #[serde(default)]
    pub turbo_prefetch: bool,
    #[serde(default = "default_prefetch_concurrency")]
    pub prefetch_concurrency: usize,

    /// Semantic hygiene & governance options
    #[serde(default = "default_true")]
    pub convert_gitignore: bool,
    #[serde(default = "default_true")]
    pub scan_externals: bool,
    #[serde(default = "default_true")]
    pub repair_gbk_messages: bool,
    #[serde(default = "default_true")]
    pub verify_tree_consistency: bool,
    #[serde(default)]
    pub optimize_git_repo: bool,

    /// Path exclusion glob patterns (e.g. "bin/**", "obj/**", "*.bak")
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    /// Automatic Git LFS conversion threshold in MB (None = disabled)
    #[serde(default)]
    pub lfs_threshold_mb: Option<u64>,
}

fn default_window_size() -> u64 {
    500
}
fn default_prefetch_concurrency() -> usize {
    3
}
fn default_true() -> bool {
    true
}

impl TaskConfig {
    pub fn effective_svn_url(&self) -> String {
        let base = self.svn_url.trim().trim_end_matches('/');
        match &self.svn_subpath {
            Some(sub) if !sub.trim().is_empty() => {
                let clean = sub.trim().trim_matches(|c| c == '/' || c == '\\').replace('\\', "/");
                format!("{base}/{clean}")
            }
            _ => base.to_string(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        let url = self.svn_url.trim();
        let allowed = ["http://", "https://", "svn://", "svn+ssh://", "file://"];
        if !allowed.iter().any(|prefix| url.starts_with(prefix)) {
            return Err(Svn2GitError::Config(
                "SVN URL must start with http://, https://, svn://, svn+ssh://, or file://".into(),
            ));
        }

        if let Some(sub) = &self.svn_subpath {
            let normalized = sub.trim().replace('\\', "/");
            if normalized.contains("://") {
                return Err(Svn2GitError::Config(
                    "svn_subpath must be a relative path, not a URL".into(),
                ));
            }
            if normalized.split('/').any(|seg| seg == "..") {
                return Err(Svn2GitError::Config(
                    "svn_subpath cannot contain '..' segments".into(),
                ));
            }
        }

        if let Some(branch) = &self.target_branch {
            let b = branch.trim();
            if !b.is_empty() {
                let forbidden = [" ", "~", "^", ":", "?", "*", "[", "\\", "..", "@{", "//"];
                if b.starts_with('/')
                    || b.ends_with('/')
                    || b.ends_with('.')
                    || forbidden.iter().any(|tok| b.contains(tok))
                {
                    return Err(Svn2GitError::Config(format!(
                        "invalid Git branch name: {b}"
                    )));
                }
            }
        }

        if self.workdir.trim().is_empty() {
            return Err(Svn2GitError::Config("workdir is required".into()));
        }
        let workdir_path = PathBuf::from(&self.workdir);
        if let Some(parent) = workdir_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                return Err(Svn2GitError::Config(format!(
                    "workdir parent directory does not exist: {}",
                    parent.display()
                )));
            }
        }

        if let Some(authors) = &self.authors_file {
            if !authors.trim().is_empty() {
                let p = PathBuf::from(authors);
                if !p.exists() || !p.is_file() {
                    return Err(Svn2GitError::Config(format!(
                        "authors_file does not exist: {}",
                        p.display()
                    )));
                }
            }
        }

        if let (Some(s), Some(e)) = (self.start_revision, self.end_revision) {
            if s == 0 || e == 0 || s > e {
                return Err(Svn2GitError::Config(format!(
                    "invalid revision range: {s}..={e}"
                )));
            }
        }

        if self.push_remote && self.git_remote_url.as_deref().unwrap_or("").trim().is_empty() {
            return Err(Svn2GitError::Config(
                "git_remote_url is required when push_remote is enabled".into(),
            ));
        }

        if self.push_strategy == PushStrategy::SubtreePrefix
            && self.subtree_prefix.as_deref().unwrap_or("").trim().is_empty()
        {
            return Err(Svn2GitError::Config(
                "subtree_prefix is required when push_strategy is subtree_prefix".into(),
            ));
        }

        Ok(())
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn from_toml(input: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(input)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn from_json(input: &str) -> Result<Self> {
        let cfg: Self = serde_json::from_str(input)?;
        cfg.validate()?;
        Ok(cfg)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrecheckItem {
    pub name: String,
    pub ok: bool,
    pub message: String,
    #[serde(default)]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrecheckReport {
    pub task_id: i64,
    pub ok: bool,
    pub checks: Vec<PrecheckItem>,
    pub head_revision: Option<u64>,
    pub detected_layout: Option<LayoutMode>,
    pub unmapped_authors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProgress {
    pub task_id: i64,
    pub stage: String,
    pub current_rev: u64,
    pub total_revs: u64,
    pub percent: f64,
    pub revs_per_sec: f64,
    pub eta_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationReport {
    pub commit_count: usize,
    pub branch_count: usize,
    pub tag_count: usize,
    pub branches: Vec<String>,
    pub tags: Vec<String>,
    pub latest_commit: String,
    pub current_branch: String,
    pub workdir: String,
    pub duration_secs: f64,
    pub engine_used: EngineKind,
    pub last_synced_rev: u64,
    pub gitignore_generated: bool,
    pub externals_found: Vec<SvnExternalEntry>,
    pub lfs_converted_files: Vec<LfsConvertedEntry>,
    pub excluded_files_count: usize,
    pub verification: Option<TreeVerificationReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SvnExternalEntry {
    pub path: String,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LfsConvertedEntry {
    pub path: String,
    pub oid: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeVerificationReport {
    pub matched: bool,
    pub checked_files: usize,
    pub missing_in_git: Vec<String>,
    pub extra_in_git: Vec<String>,
    pub hash_mismatches: Vec<String>,
    pub svn_revision: u64,
    pub git_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: i64,
    pub name: String,
    pub status: TaskStatus,
    pub stage: String,
    pub config: TaskConfig,
    pub last_synced_rev: u64,
    pub total_revs: u64,
    pub error: Option<String>,
    pub report: Option<MigrationReport>,
    pub created_at: String,
    pub updated_at: String,
}
