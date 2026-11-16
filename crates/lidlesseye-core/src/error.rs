//! Error types shared by the LidlessEye crates.

use std::path::PathBuf;

/// Result alias used across the workspace.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while storing or querying profiles.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("invalid profile report: {0}")]
    InvalidReport(String),

    /// A database whose rows violate the schema's own invariants.
    #[error("inconsistent database {path}: {reason}")]
    CorruptDatabase { path: PathBuf, reason: String },

    #[error("database error: {0}")]
    Db(String),

    #[error("selector parse error: {0}")]
    Selector(String),

    #[error("invalid tenant id: {0}")]
    InvalidTenant(String),

    #[error("tenant {tenant} is over its limit: {reason}")]
    LimitExceeded { tenant: String, reason: String },

    #[error("malformed profile payload: {0}")]
    Malformed(String),

    #[error("time range is invalid: from={from} until={until}")]
    InvalidRange { from: u64, until: u64 },

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub(crate) fn corrupt(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Error::CorruptDatabase {
            path: path.into(),
            reason: reason.into(),
        }
    }
}
