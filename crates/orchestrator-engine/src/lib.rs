//! Stateful orchestration services built exclusively on vendor-neutral contracts.
#![allow(clippy::missing_errors_doc)]

mod checkpoint;
mod coordinator;
mod error;
mod handover;
mod rollback;
mod startup;
mod verification;
mod worktree;

pub use checkpoint::{CheckpointInput, CheckpointManager, GitCheckpointEvidence};
pub use coordinator::TaskLifecycle;
pub use error::{EngineError, EngineResult};
pub use handover::{HandoverInput, HandoverManager};
pub use orchestrator_domain::TaskEnvelope;
pub use rollback::{
    RollbackApproval, RollbackExecutionReport, RollbackManager, RollbackRecoveryPlan, RollbackStep,
};
pub use startup::{CodexExecutionPolicy, StartupGuard, StartupGuardReport};
pub use verification::{SecretFinding, SecretScanReport, VerificationEngine, VerificationInput};
pub use worktree::{
    FileOwnershipRegistry, GitSnapshot, GitWorktree, GitWorktreeManager, WorktreeCleanupPlan,
    canonicalize_directory,
};
