pub use anyhow::{anyhow, Context, Result};

#[derive(thiserror::Error, Debug)]
pub enum SyncError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("unsupported tool combination: {0} → {1}")]
    UnsupportedDirection(String, String),
    #[error("no sessions found for {0}")]
    NoSessions(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
}
