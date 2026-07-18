use std::{io, path::PathBuf};

use thiserror::Error;

pub type StateResult<T> = Result<T, StateError>;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("I/O operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TOML syntax is invalid: {0}")]
    TomlSyntax(#[from] toml_edit::TomlError),
    #[error("TOML data is invalid: {0}")]
    TomlData(#[from] toml_edit::de::Error),
    #[error("configuration validation failed: {0}")]
    InvalidConfig(String),
    #[error("persisted record is invalid: {0}")]
    InvalidRecord(String),
    #[error("optimistic update conflict for {entity}")]
    OptimisticConflict { entity: String },
    #[error("lease conflict for task {task_id}: {reason}")]
    LeaseConflict { task_id: String, reason: String },
    #[error("lease {lease_id} is not active or is owned by another coordinator")]
    LeaseOwnership { lease_id: String },
    #[error("artifact path is unsafe: {0}")]
    UnsafeArtifactPath(String),
    #[error("symbolic-link traversal is forbidden: {0}")]
    SymlinkEscape(PathBuf),
    #[error("artifact already exists with different content: {0}")]
    ArtifactConflict(PathBuf),
    #[error("migration checksum mismatch for version {version}")]
    MigrationChecksum { version: u32 },
    #[error("database schema version {found} is newer than supported version {supported}")]
    FutureSchema { found: u32, supported: u32 },
    #[error("migration {version} cannot be skipped; expected version {expected}")]
    MigrationGap { version: u32, expected: u32 },
    #[error("database lock was poisoned")]
    LockPoisoned,
    #[error("audit event chain is invalid at sequence {sequence}: {reason}")]
    InvalidEventChain { sequence: i64, reason: String },
    #[error("audit log has an incomplete final line")]
    TornEventLogTail,
    #[error("rollback guard failed: {0}")]
    RollbackGuard(String),
}

impl StateError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
