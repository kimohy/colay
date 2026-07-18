use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    AttemptId, CheckpointId, CommandEvidence, CompletedStep, DecisionRecord, FailureRecord,
    IntegrityError, PlanStep, ProviderId, RepoPath, SchemaVersion, TaskId, TestEvidence,
    canonical_sha256,
};

/// Checkpoint contract written by this orchestrator release.
pub const CHECKPOINT_SCHEMA_VERSION: &str = SchemaVersion::V1;
/// Checkpoint contracts that this release can safely read and verify.
pub const SUPPORTED_CHECKPOINT_SCHEMA_VERSIONS: &[&str] = &[CHECKPOINT_SCHEMA_VERSION];

/// A provider summary is advisory until reconciled against Git and command evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UntrustedWorkerClaim {
    pub provider: ProviderId,
    pub summary: String,
    pub claimed_files_changed: Vec<RepoPath>,
    pub claimed_tests_passed: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    #[serde(deserialize_with = "crate::schema::deserialize_v1_schema_version")]
    pub schema_version: SchemaVersion,
    pub checkpoint_id: CheckpointId,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub objective: String,
    pub current_plan: Vec<PlanStep>,
    pub completed_steps: Vec<CompletedStep>,
    pub pending_steps: Vec<PlanStep>,
    pub files_read: Vec<RepoPath>,
    pub files_changed: Vec<RepoPath>,
    pub git_base: Option<String>,
    pub diff_path: Option<RepoPath>,
    pub commands_run: Vec<CommandEvidence>,
    pub tests: Vec<TestEvidence>,
    pub decisions: Vec<DecisionRecord>,
    pub unresolved_questions: Vec<String>,
    pub known_failures: Vec<FailureRecord>,
    pub worker_claim: Option<UntrustedWorkerClaim>,
    pub current_worker: ProviderId,
    pub concise_context_summary: String,
    pub created_at: DateTime<Utc>,
    pub integrity_hash: String,
}

impl Checkpoint {
    /// Whether this checkpoint uses a contract version understood by this release.
    #[must_use]
    pub fn has_supported_schema(&self) -> bool {
        self.schema_version
            .is_supported_by(SUPPORTED_CHECKPOINT_SCHEMA_VERSIONS)
    }

    /// Calculates and installs the canonical integrity hash.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrityError`] when the checkpoint cannot be serialized.
    pub fn seal(mut self) -> Result<Self, IntegrityError> {
        self.refresh_integrity_hash()?;
        Ok(self)
    }

    /// # Errors
    ///
    /// Returns [`IntegrityError`] when the checkpoint cannot be serialized.
    pub fn refresh_integrity_hash(&mut self) -> Result<(), IntegrityError> {
        self.integrity_hash.clear();
        self.integrity_hash = canonical_sha256(self)?;
        Ok(())
    }

    /// # Errors
    ///
    /// Returns [`IntegrityError`] when the checkpoint cannot be serialized for comparison.
    pub fn verify_integrity(&self) -> Result<bool, IntegrityError> {
        let mut candidate = self.clone();
        let expected = std::mem::take(&mut candidate.integrity_hash);
        Ok(!expected.is_empty() && canonical_sha256(&candidate)? == expected)
    }
}

#[cfg(test)]
mod tests {
    use super::{SUPPORTED_CHECKPOINT_SCHEMA_VERSIONS, SchemaVersion};

    #[test]
    fn checkpoint_schema_support_is_explicit_and_fail_closed() {
        assert!(SchemaVersion::v1().is_supported_by(SUPPORTED_CHECKPOINT_SCHEMA_VERSIONS));
        assert!(!SchemaVersion::new("999").is_supported_by(SUPPORTED_CHECKPOINT_SCHEMA_VERSIONS));
    }
}
