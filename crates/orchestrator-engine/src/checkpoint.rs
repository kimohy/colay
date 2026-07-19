use chrono::{DateTime, Utc};
use orchestrator_domain::{
    AttemptId, CHECKPOINT_SCHEMA_VERSION, Checkpoint, CheckpointId, CommandEvidence, CompletedStep,
    DecisionRecord, FailureRecord, PlanStep, ProviderId, RepoPath, SchemaVersion, TaskId,
    TestEvidence, UntrustedWorkerClaim,
};
use orchestrator_state::ArtifactStore;
use sha2::{Digest as _, Sha256};

use crate::{EngineError, EngineResult, GitSnapshot};

#[derive(Clone, Debug)]
pub struct GitCheckpointEvidence {
    pub base_revision: String,
    pub head: String,
    pub diff: Vec<u8>,
    pub changed_files: Vec<RepoPath>,
}

impl From<&GitSnapshot> for GitCheckpointEvidence {
    fn from(snapshot: &GitSnapshot) -> Self {
        Self {
            base_revision: snapshot.base_revision.clone(),
            head: snapshot.head.clone(),
            diff: snapshot.diff.clone(),
            changed_files: snapshot.changed_files.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CheckpointInput {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub objective: String,
    pub current_plan: Vec<PlanStep>,
    pub completed_steps: Vec<CompletedStep>,
    pub pending_steps: Vec<PlanStep>,
    pub files_read: Vec<RepoPath>,
    pub commands_run: Vec<CommandEvidence>,
    pub tests: Vec<TestEvidence>,
    pub decisions: Vec<DecisionRecord>,
    pub unresolved_questions: Vec<String>,
    pub known_failures: Vec<FailureRecord>,
    pub worker_claim: Option<UntrustedWorkerClaim>,
    pub current_worker: ProviderId,
    pub concise_context_summary: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct CheckpointManager {
    artifacts: ArtifactStore,
}

impl CheckpointManager {
    #[must_use]
    pub const fn new(artifacts: ArtifactStore) -> Self {
        Self { artifacts }
    }

    /// Creates a checkpoint from Git-derived evidence. Provider claims are retained as
    /// untrusted context but never determine the authoritative file list or diff.
    pub fn create(
        &self,
        input: CheckpointInput,
        git: GitCheckpointEvidence,
    ) -> EngineResult<Checkpoint> {
        if !is_full_object_id(&git.base_revision) || !is_full_object_id(&git.head) {
            return Err(EngineError::MissingGitEvidence);
        }
        let checkpoint_id = CheckpointId::new();
        let diff_path = if git.diff.is_empty() {
            None
        } else {
            // The content digest is part of the path covered by Checkpoint::integrity_hash.
            // This binds the sealed checkpoint to tracked, committed, and untracked bytes
            // without introducing provider-specific fields into the domain schema.
            let digest = format!("{:x}", Sha256::digest(&git.diff));
            let relative = RepoPath::try_from(format!(
                "checkpoints/{checkpoint_id}/worktree.{digest}.diff"
            ))
            .map_err(|error| EngineError::InvalidRepoPath(error.to_string()))?;
            let stored = self.artifacts.put(relative.clone(), &git.diff)?;
            if stored.sha256 != digest {
                return Err(EngineError::IntegrityMismatch {
                    artifact: "checkpoint diff",
                });
            }
            Some(relative)
        };

        let checkpoint = Checkpoint {
            schema_version: SchemaVersion::new(CHECKPOINT_SCHEMA_VERSION),
            checkpoint_id,
            task_id: input.task_id,
            attempt_id: input.attempt_id,
            objective: input.objective,
            current_plan: input.current_plan,
            completed_steps: input.completed_steps,
            pending_steps: input.pending_steps,
            files_read: input.files_read,
            files_changed: git.changed_files,
            git_base: Some(git.base_revision),
            diff_path,
            commands_run: input.commands_run,
            tests: input.tests,
            decisions: input.decisions,
            unresolved_questions: input.unresolved_questions,
            known_failures: input.known_failures,
            worker_claim: input.worker_claim,
            current_worker: input.current_worker,
            concise_context_summary: input.concise_context_summary,
            created_at: input.created_at,
            integrity_hash: String::new(),
        }
        .seal()?;

        let document_path =
            RepoPath::try_from(format!("checkpoints/{checkpoint_id}/checkpoint.json"))
                .map_err(|error| EngineError::InvalidRepoPath(error.to_string()))?;
        self.artifacts
            .put(document_path, &serde_json::to_vec_pretty(&checkpoint)?)?;
        Ok(checkpoint)
    }
}

fn is_full_object_id(value: &str) -> bool {
    (40..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use orchestrator_domain::{AttemptId, ProviderId, RepoPath, TaskId};
    use sha2::{Digest as _, Sha256};

    use super::{CheckpointInput, CheckpointManager, GitCheckpointEvidence};

    #[test]
    fn git_evidence_overrides_worker_claimed_files() -> Result<(), Box<dyn std::error::Error>> {
        let directory = crate::test_support::CanonicalTempDir::new()?;
        let store = orchestrator_state::ArtifactStore::open(directory.path())?;
        let manager = CheckpointManager::new(store);
        let input = CheckpointInput {
            task_id: TaskId::new(),
            attempt_id: AttemptId::new(),
            objective: "implement".to_owned(),
            current_plan: Vec::new(),
            completed_steps: Vec::new(),
            pending_steps: Vec::new(),
            files_read: Vec::new(),
            commands_run: Vec::new(),
            tests: Vec::new(),
            decisions: Vec::new(),
            unresolved_questions: Vec::new(),
            known_failures: Vec::new(),
            worker_claim: None,
            current_worker: ProviderId::Codex,
            concise_context_summary: "summary".to_owned(),
            created_at: Utc::now(),
        };
        let diff = b"diff --git a/src/lib.rs b/src/lib.rs".to_vec();
        let expected_digest = format!("{:x}", Sha256::digest(&diff));
        let checkpoint = manager.create(
            input,
            GitCheckpointEvidence {
                base_revision: "0000000000000000000000000000000000000000".to_owned(),
                head: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                diff,
                changed_files: vec![RepoPath::try_from("src/lib.rs")?],
            },
        )?;
        assert!(checkpoint.verify_integrity()?);
        assert_eq!(
            checkpoint.files_changed,
            vec![RepoPath::try_from("src/lib.rs")?]
        );
        assert_eq!(
            checkpoint.git_base.as_deref(),
            Some("0000000000000000000000000000000000000000")
        );
        assert!(
            checkpoint
                .diff_path
                .as_ref()
                .is_some_and(|path| path.to_string().contains(&expected_digest))
        );
        Ok(())
    }
}
