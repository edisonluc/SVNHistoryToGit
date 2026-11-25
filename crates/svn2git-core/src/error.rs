use thiserror::Error;

pub type Result<T> = std::result::Result<T, Svn2GitError>;

#[derive(Debug, Error)]
pub enum Svn2GitError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("executable not found: {0}")]
    ExecutableNotFound(String),

    #[error("command failed (exit={code:?}): {command} — {stderr}")]
    CommandFailed {
        command: String,
        code: Option<i32>,
        stderr: String,
    },

    #[error("command timed out after {seconds}s: {command}")]
    CommandTimeout { command: String, seconds: u64 },

    #[error("task cancelled by user")]
    Cancelled,

    #[error("credential error: {0}")]
    Credential(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("svn error: {0}")]
    Svn(String),

    #[error("git error: {0}")]
    Git(String),

    #[error("verification mismatch: {0}")]
    Verification(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("toml serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("toml deserialize error: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("{0}")]
    Other(String),
}
