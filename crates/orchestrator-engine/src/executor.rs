use std::path::PathBuf;

use async_trait::async_trait;
use orchestrator_domain::{
    AttemptId, Checkpoint, FailureRecord, ProviderId, RepoPath, TaskId, VerificationResult,
    WorkerOutcome,
};
use orchestrator_state::{ClaimedTask, StoredTaskInstruction};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::EngineResult;
use crate::GitWorktree;

#[derive(Clone, Debug)]
pub struct TaskExecutionRequest {
    pub claim: ClaimedTask,
    pub repository_root: PathBuf,
    pub state_root: PathBuf,
    pub instructions: Vec<StoredTaskInstruction>,
    pub existing_worktree: Option<GitWorktree>,
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
    #[serde(default)]
    pub lifecycle_failure: Option<FailureRecord>,
}

impl TaskExecutionReport {
    fn verification_matches_report(&self, verification: &VerificationResult) -> bool {
        verification.task_id == self.task_id
            && verification.implementation_provider == self.provider
            && verification.changed_files == self.changed_files
    }

    #[must_use]
    pub fn passed_completion_gate(&self) -> bool {
        self.outcome == WorkerOutcome::Succeeded
            && self.verification.as_ref().is_some_and(|verification| {
                self.verification_matches_report(verification)
                    && verification.passes_completion_gate(false)
            })
            && self.checkpoint.as_ref().is_some_and(|checkpoint| {
                checkpoint.files_changed == self.changed_files
                    && checkpoint.verify_integrity().unwrap_or(false)
            })
    }

    /// Confirms that lifecycle failure evidence is bound to this report's sealed checkpoint.
    #[must_use]
    pub fn validate_failure_contract(&self) -> bool {
        if self.outcome == WorkerOutcome::Succeeded && self.lifecycle_failure.is_some() {
            return false;
        }
        let expected = self.lifecycle_failure.iter().cloned().collect::<Vec<_>>();
        self.checkpoint
            .as_ref()
            .map_or(self.lifecycle_failure.is_none(), |checkpoint| {
                checkpoint.task_id == self.task_id
                    && checkpoint.attempt_id == self.attempt_id
                    && checkpoint.current_worker == self.provider
                    && checkpoint.git_base.as_deref() == Some(self.base_revision.as_str())
                    && checkpoint.files_changed == self.changed_files
                    && checkpoint.has_supported_schema()
                    && checkpoint.verify_integrity().unwrap_or(false)
                    && checkpoint.known_failures == expected
            })
    }

    /// Returns a non-retryable lifecycle failure only when its checkpoint evidence is consistent.
    #[must_use]
    pub fn non_retryable_failure(&self) -> Option<&FailureRecord> {
        self.validate_failure_contract()
            .then_some(self.lifecycle_failure.as_ref())
            .flatten()
            .filter(|failure| !failure.retryable)
    }

    /// Confirms that a self-consistent report belongs to the scheduler claim that produced it.
    #[must_use]
    pub fn validates_claim(&self, claim: &ClaimedTask) -> bool {
        self.task_id == claim.task_id
            && self.provider == claim.provider
            && self.base_revision == claim.approved_base_commit
            && self
                .verification
                .as_ref()
                .is_none_or(|verification| self.verification_matches_report(verification))
            && self.validate_failure_contract()
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
        AcceptanceEvidence, AttemptId, Checkpoint, CheckpointId, FailureRecord, ProviderId,
        SchemaVersion, TaskId, VerificationId, VerificationResult, VerificationStatus,
        WorkerOutcome,
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
            lifecycle_failure: None,
        };
        assert!(!report.passed_completion_gate());
        report.outcome = WorkerOutcome::Failed;
        assert!(!report.passed_completion_gate());
    }

    fn sealed_checkpoint(
        report: &TaskExecutionReport,
        known_failures: Vec<FailureRecord>,
    ) -> Result<Checkpoint, orchestrator_domain::IntegrityError> {
        Checkpoint {
            schema_version: SchemaVersion::v1(),
            checkpoint_id: CheckpointId::new(),
            task_id: report.task_id,
            attempt_id: report.attempt_id,
            objective: "exercise lifecycle contract".to_owned(),
            current_plan: Vec::new(),
            completed_steps: Vec::new(),
            pending_steps: Vec::new(),
            files_read: Vec::new(),
            files_changed: Vec::new(),
            git_base: Some(report.base_revision.clone()),
            diff_path: None,
            commands_run: Vec::new(),
            tests: Vec::new(),
            decisions: Vec::new(),
            unresolved_questions: Vec::new(),
            known_failures,
            worker_claim: None,
            current_worker: report.provider,
            concise_context_summary: report.summary_redacted.clone(),
            created_at: Utc::now(),
            integrity_hash: String::new(),
        }
        .seal()
    }

    fn failed_report() -> TaskExecutionReport {
        TaskExecutionReport {
            task_id: TaskId::new(),
            attempt_id: AttemptId::new(),
            provider: ProviderId::Agy,
            outcome: WorkerOutcome::Failed,
            summary_redacted: "agy process exited".to_owned(),
            worktree_path: "worktree".into(),
            branch: "task".to_owned(),
            base_revision: "0".repeat(40),
            changed_files: Vec::new(),
            checkpoint: None,
            verification: None,
            lifecycle_failure: None,
        }
    }

    #[test]
    fn serde_defaults_missing_lifecycle_failure_for_older_reports()
    -> Result<(), Box<dyn std::error::Error>> {
        let report = failed_report();
        let mut document = serde_json::to_value(report)?;
        document
            .as_object_mut()
            .ok_or("report should be an object")?
            .remove("lifecycle_failure");

        let decoded: TaskExecutionReport = serde_json::from_value(document)?;

        assert_eq!(decoded.lifecycle_failure, None);
        assert!(decoded.validate_failure_contract());
        Ok(())
    }

    #[test]
    fn non_retryable_failure_requires_exact_sealed_checkpoint_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut report = failed_report();
        let failure = FailureRecord {
            code: Some("agy_process_exit".to_owned()),
            summary: "agy process exited unexpectedly".to_owned(),
            retryable: false,
            occurred_at: Utc::now(),
        };
        report.lifecycle_failure = Some(failure.clone());
        report.checkpoint = Some(sealed_checkpoint(&report, vec![failure.clone()])?);

        assert!(report.validate_failure_contract());
        assert_eq!(report.non_retryable_failure(), Some(&failure));

        let mut mismatched = sealed_checkpoint(&report, vec![failure.clone()])?;
        mismatched.current_worker = ProviderId::Codex;
        mismatched.git_base = Some("1".repeat(40));
        mismatched.refresh_integrity_hash()?;
        report.checkpoint = Some(mismatched);
        assert!(!report.validate_failure_contract());

        report.checkpoint = Some(sealed_checkpoint(&report, vec![failure])?);
        report
            .lifecycle_failure
            .as_mut()
            .ok_or("failure missing")?
            .retryable = true;
        assert!(!report.validate_failure_contract());
        assert_eq!(report.non_retryable_failure(), None);
        Ok(())
    }

    #[test]
    fn successful_report_cannot_carry_a_lifecycle_failure() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut report = failed_report();
        let failure = FailureRecord {
            code: Some("provider_error".to_owned()),
            summary: "provider failed".to_owned(),
            retryable: false,
            occurred_at: Utc::now(),
        };
        report.outcome = WorkerOutcome::Succeeded;
        report.lifecycle_failure = Some(failure.clone());
        report.checkpoint = Some(sealed_checkpoint(&report, vec![failure])?);

        assert!(!report.validate_failure_contract());
        assert_eq!(report.non_retryable_failure(), None);
        Ok(())
    }
}
