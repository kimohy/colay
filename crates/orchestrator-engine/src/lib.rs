//! Stateful orchestration services built exclusively on vendor-neutral contracts.
#![allow(clippy::missing_errors_doc)]

mod checkpoint;
mod coordinator;
mod error;
mod executor;
mod handover;
mod planner;
mod rollback;
mod startup;
mod verification;
mod worktree;

pub use checkpoint::{CheckpointInput, CheckpointManager, GitCheckpointEvidence};
pub use coordinator::TaskLifecycle;
pub use error::{EngineError, EngineResult};
pub use executor::{TaskExecutionReport, TaskExecutionRequest, TaskExecutor};
pub use handover::{HandoverInput, HandoverManager};
pub use orchestrator_domain::TaskEnvelope;
pub use planner::{
    PLANNER_MAX_OUTPUT_BYTES, PlannerExit, PlannerFailure, PlannerRequest, PlannerResponse,
    TaskPlanner, collect_planner_response,
};
pub use rollback::{
    RollbackApproval, RollbackExecutionReport, RollbackManager, RollbackRecoveryPlan, RollbackStep,
};
pub use startup::{CodexExecutionPolicy, StartupGuard, StartupGuardReport};
pub use verification::{SecretFinding, SecretScanReport, VerificationEngine, VerificationInput};
pub use worktree::{
    FileOwnershipRegistry, GitSnapshot, GitWorktree, GitWorktreeManager, WorktreeCleanupPlan,
    canonicalize_directory,
};

#[cfg(test)]
mod test_support {
    use std::{io, path::Path};

    pub(crate) struct CanonicalTempDir {
        _directory: tempfile::TempDir,
        canonical_path: std::path::PathBuf,
    }

    impl CanonicalTempDir {
        pub(crate) fn new() -> io::Result<Self> {
            let directory = tempfile::tempdir()?;
            let canonical_path = std::fs::canonicalize(directory.path())?;
            Ok(Self {
                _directory: directory,
                canonical_path,
            })
        }

        pub(crate) fn path(&self) -> &Path {
            &self.canonical_path
        }
    }
}
