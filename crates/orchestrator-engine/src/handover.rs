use chrono::{DateTime, Utc};
use orchestrator_domain::{
    Checkpoint, HANDOVER_SCHEMA_VERSION, HandoverAcknowledgement, HandoverBundle, HandoverId,
    ProviderId, SchemaVersion, UsageSnapshot,
};

use crate::{EngineError, EngineResult};

#[derive(Clone, Debug)]
pub struct HandoverInput {
    pub checkpoint: Checkpoint,
    pub original_request: String,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub recommended_next_worker: ProviderId,
    pub usage_snapshots: Vec<UsageSnapshot>,
    pub safe_boundary_confirmed: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default)]
pub struct HandoverManager;

impl HandoverManager {
    pub fn create(input: HandoverInput) -> EngineResult<HandoverBundle> {
        if !input.safe_boundary_confirmed {
            return Err(EngineError::UnsafeGitBoundary(
                "worker has not reached a safe checkpoint boundary".to_owned(),
            ));
        }
        if !input.checkpoint.has_supported_schema() || !input.checkpoint.verify_integrity()? {
            return Err(EngineError::IntegrityMismatch {
                artifact: "checkpoint",
            });
        }
        let checkpoint = input.checkpoint;
        HandoverBundle {
            schema_version: SchemaVersion::new(HANDOVER_SCHEMA_VERSION),
            handover_id: HandoverId::new(),
            task_id: checkpoint.task_id,
            objective: checkpoint.objective,
            original_request: input.original_request,
            constraints: input.constraints,
            acceptance_criteria: input.acceptance_criteria,
            current_plan: checkpoint.current_plan,
            completed_steps: checkpoint.completed_steps,
            pending_steps: checkpoint.pending_steps,
            files_read: checkpoint.files_read,
            files_changed: checkpoint.files_changed,
            git_base: checkpoint.git_base,
            diff_path: checkpoint.diff_path,
            commands_run: checkpoint.commands_run,
            tests: checkpoint.tests,
            decisions: checkpoint.decisions,
            unresolved_questions: checkpoint.unresolved_questions,
            known_failures: checkpoint.known_failures,
            current_worker: checkpoint.current_worker,
            recommended_next_worker: input.recommended_next_worker,
            usage_snapshots: input.usage_snapshots,
            concise_context_summary: checkpoint.concise_context_summary,
            created_at: input.created_at,
            integrity_hash: String::new(),
        }
        .seal()
        .map_err(Into::into)
    }

    pub fn stdin_payload(bundle: &HandoverBundle) -> EngineResult<Vec<u8>> {
        if !bundle.has_supported_schema() || !bundle.verify_integrity()? {
            return Err(EngineError::IntegrityMismatch {
                artifact: "handover bundle",
            });
        }
        Ok(serde_json::to_vec(bundle)?)
    }

    pub fn validate_acknowledgement(
        bundle: &HandoverBundle,
        acknowledgement: &HandoverAcknowledgement,
    ) -> EngineResult<()> {
        if bundle.verify_integrity()? && acknowledgement.matches(bundle) {
            Ok(())
        } else {
            Err(EngineError::InvalidHandoverAcknowledgement)
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use orchestrator_domain::{
        AttemptId, Checkpoint, CheckpointId, ProviderId, SchemaVersion, TaskId,
    };

    use super::{HandoverInput, HandoverManager};

    #[test]
    fn refuses_handover_without_safe_boundary() -> Result<(), Box<dyn std::error::Error>> {
        let checkpoint = empty_checkpoint().seal()?;
        let result = HandoverManager::create(HandoverInput {
            checkpoint,
            original_request: "request".to_owned(),
            constraints: Vec::new(),
            acceptance_criteria: Vec::new(),
            recommended_next_worker: ProviderId::Claude,
            usage_snapshots: Vec::new(),
            safe_boundary_confirmed: false,
            created_at: Utc::now(),
        });
        assert!(result.is_err());
        Ok(())
    }

    fn empty_checkpoint() -> Checkpoint {
        Checkpoint {
            schema_version: SchemaVersion::v1(),
            checkpoint_id: CheckpointId::new(),
            task_id: TaskId::new(),
            attempt_id: AttemptId::new(),
            objective: "objective".to_owned(),
            current_plan: Vec::new(),
            completed_steps: Vec::new(),
            pending_steps: Vec::new(),
            files_read: Vec::new(),
            files_changed: Vec::new(),
            git_base: Some("base".to_owned()),
            diff_path: None,
            commands_run: Vec::new(),
            tests: Vec::new(),
            decisions: Vec::new(),
            unresolved_questions: Vec::new(),
            known_failures: Vec::new(),
            worker_claim: None,
            current_worker: ProviderId::Codex,
            concise_context_summary: "summary".to_owned(),
            created_at: Utc::now(),
            integrity_hash: String::new(),
        }
    }
}
