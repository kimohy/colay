use std::{io, path::PathBuf};

use orchestrator_domain::{IntegrityError, TransitionError};
use thiserror::Error;

pub type EngineResult<T> = Result<T, EngineError>;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("I/O failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("command `{executable}` failed with exit code {exit_code:?}: {message}")]
    CommandFailed {
        executable: String,
        exit_code: Option<i32>,
        message: String,
    },
    #[error("unsafe repository or worktree path: {0}")]
    UnsafePath(PathBuf),
    #[error("repository contains unresolved Git operations: {0}")]
    UnsafeGitBoundary(String),
    #[error("file {path} is already owned by writable worker {owner}")]
    FileOwnershipConflict { path: String, owner: String },
    #[error("worktree cleanup requires a matching explicit approval")]
    CleanupApprovalRequired,
    #[error("checkpoint has no authoritative Git evidence")]
    MissingGitEvidence,
    #[error("handover acknowledgement does not match the sealed bundle")]
    InvalidHandoverAcknowledgement,
    #[error("integrity check failed for {artifact}")]
    IntegrityMismatch { artifact: &'static str },
    #[error("invalid repository-relative path: {0}")]
    InvalidRepoPath(String),
    #[error("state transition rejected: {0}")]
    Transition(#[from] TransitionError),
    #[error("integrity operation failed: {0}")]
    Integrity(#[from] IntegrityError),
    #[error("state persistence failed: {0}")]
    State(#[from] orchestrator_state::StateError),
    #[error("subprocess execution failed: {0}")]
    Process(#[from] orchestrator_process::ProcessError),
    #[error("JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("rollback target or approval is invalid: {0}")]
    Rollback(String),
}

impl EngineError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
