use std::path::PathBuf;

use async_trait::async_trait;
use orchestrator_domain::{
    AttemptId, Checkpoint, ProviderId, RepoPath, TaskId, VerificationResult, WorkerOutcome,
};
use orchestrator_state::{ClaimedTask, StoredTaskInstruction};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::EngineResult;

#[derive(Clone, Debug)]
pub struct TaskExecutionRequest {
    pub claim: ClaimedTask,
    pub repository_root: PathBuf,
    pub state_root: PathBuf,
    pub instructions: Vec<StoredTaskInstruction>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskExecutionReport {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub provider: ProviderId,
    pub outcome: WorkerOutcome,
    pub summary_redacted: String,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub base_revision: String,
    pub changed_files: Vec<RepoPath>,
    pub checkpoint: Option<Checkpoint>,
    pub verification: Option<VerificationResult>,
}

impl TaskExecutionReport {
    #[must_use]
    pub fn passed_completion_gate(&self) -> bool {
        self.outcome == WorkerOutcome::Succeeded
            && self
                .verification
                .as_ref()
                .is_some_and(|verification| verification.passes_completion_gate(false))
            && self
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.verify_integrity().unwrap_or(false))
    }
}

#[async_trait]
pub trait TaskExecutor: Send + Sync {
    async fn execute(
        &self,
        request: TaskExecutionRequest,
        cancellation: CancellationToken,
    ) -> EngineResult<TaskExecutionReport>;
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use orchestrator_domain::{
        AcceptanceEvidence, AttemptId, ProviderId, SchemaVersion, TaskId, VerificationId,
        VerificationResult, VerificationStatus, WorkerOutcome,
    };

    use super::TaskExecutionReport;

    #[test]
    fn completion_requires_success_verification_and_sealed_checkpoint() {
        let task_id = TaskId::new();
        let mut report = TaskExecutionReport {
            task_id,
            attempt_id: AttemptId::new(),
            provider: ProviderId::Codex,
            outcome: WorkerOutcome::Succeeded,
            summary_redacted: "done".to_owned(),
            worktree_path: "worktree".into(),
            branch: "task".to_owned(),
            base_revision: "0".repeat(40),
            changed_files: Vec::new(),
            checkpoint: None,
            verification: Some(VerificationResult {
                schema_version: SchemaVersion::v1(),
                verification_id: VerificationId::new(),
                task_id,
                implementation_provider: ProviderId::Codex,
                reviewer_provider: None,
                status: VerificationStatus::Pass,
                checks: Vec::new(),
                acceptance_criteria: vec![AcceptanceEvidence {
                    criterion: "done".to_owned(),
                    status: VerificationStatus::Pass,
                    evidence: vec!["structured completion".to_owned()],
                }],
                changed_files: Vec::new(),
                out_of_scope_files: Vec::new(),
                unresolved_todos: Vec::new(),
                requires_approval: false,
                verified_at: Utc::now(),
            }),
        };
        assert!(!report.passed_completion_gate());
        report.outcome = WorkerOutcome::Failed;
        assert!(!report.passed_completion_gate());
    }
}
