use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    CommandEvidence, CompletedStep, DecisionRecord, FailureRecord, HandoverId, IntegrityError,
    PlanStep, ProviderId, RepoPath, SchemaVersion, TaskId, TestEvidence, UsageSnapshot,
    canonical_sha256,
};

/// Handover contract written by this orchestrator release.
pub const HANDOVER_SCHEMA_VERSION: &str = SchemaVersion::V1;
/// Handover contracts that this release can safely read and verify.
pub const SUPPORTED_HANDOVER_SCHEMA_VERSIONS: &[&str] = &[HANDOVER_SCHEMA_VERSION];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HandoverBundle {
    #[serde(deserialize_with = "crate::schema::deserialize_v1_schema_version")]
    pub schema_version: SchemaVersion,
    pub handover_id: HandoverId,
    pub task_id: TaskId,
    pub objective: String,
    pub original_request: String,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<String>,
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
    pub current_worker: ProviderId,
    pub recommended_next_worker: ProviderId,
    pub usage_snapshots: Vec<UsageSnapshot>,
    pub concise_context_summary: String,
    pub created_at: DateTime<Utc>,
    pub integrity_hash: String,
}

impl HandoverBundle {
    /// Whether this bundle uses a contract version understood by this release.
    #[must_use]
    pub fn has_supported_schema(&self) -> bool {
        self.schema_version
            .is_supported_by(SUPPORTED_HANDOVER_SCHEMA_VERSIONS)
    }

    /// Calculates and installs the bundle's canonical integrity hash.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrityError`] when the bundle cannot be serialized.
    pub fn seal(mut self) -> Result<Self, IntegrityError> {
        self.refresh_integrity_hash()?;
        Ok(self)
    }

    /// # Errors
    ///
    /// Returns [`IntegrityError`] when the bundle cannot be serialized.
    pub fn refresh_integrity_hash(&mut self) -> Result<(), IntegrityError> {
        self.integrity_hash.clear();
        self.integrity_hash = canonical_sha256(self)?;
        Ok(())
    }

    /// # Errors
    ///
    /// Returns [`IntegrityError`] when the bundle cannot be serialized for comparison.
    pub fn verify_integrity(&self) -> Result<bool, IntegrityError> {
        let mut candidate = self.clone();
        let expected = std::mem::take(&mut candidate.integrity_hash);
        Ok(!expected.is_empty() && canonical_sha256(&candidate)? == expected)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoverAcknowledgement {
    pub schema_version: SchemaVersion,
    pub task_id: TaskId,
    pub bundle_hash: String,
    pub provider: ProviderId,
    pub understood_objective: String,
    pub understood_constraints: Vec<String>,
    pub understood_acceptance_criteria: Vec<String>,
    pub next_step_id: Option<String>,
    pub unresolved_questions: Vec<String>,
    pub can_resume: bool,
    pub acknowledged_at: DateTime<Utc>,
}

impl HandoverAcknowledgement {
    /// Whether this acknowledgement uses a contract version understood by this release.
    #[must_use]
    pub fn has_supported_schema(&self) -> bool {
        self.schema_version
            .is_supported_by(SUPPORTED_HANDOVER_SCHEMA_VERSIONS)
    }

    #[must_use]
    pub fn matches(&self, bundle: &HandoverBundle) -> bool {
        self.has_supported_schema()
            && bundle.has_supported_schema()
            && self.task_id == bundle.task_id
            && self.bundle_hash == bundle.integrity_hash
            && self.provider == bundle.recommended_next_worker
            && self.understood_objective == bundle.objective
            && self.understood_constraints == bundle.constraints
            && self.understood_acceptance_criteria == bundle.acceptance_criteria
            && self.can_resume
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn handover_schema_support_is_explicit_and_fail_closed() {
        assert!(SchemaVersion::v1().is_supported_by(SUPPORTED_HANDOVER_SCHEMA_VERSIONS));
        assert!(!SchemaVersion::new("999").is_supported_by(SUPPORTED_HANDOVER_SCHEMA_VERSIONS));
    }

    #[test]
    fn bundle_hash_detects_mutation() -> Result<(), Box<dyn std::error::Error>> {
        let mut bundle = HandoverBundle {
            schema_version: SchemaVersion::v1(),
            handover_id: HandoverId::new(),
            task_id: TaskId::new(),
            objective: "implement safely".to_owned(),
            original_request: "request".to_owned(),
            constraints: Vec::new(),
            acceptance_criteria: Vec::new(),
            current_plan: Vec::new(),
            completed_steps: Vec::new(),
            pending_steps: Vec::new(),
            files_read: Vec::new(),
            files_changed: Vec::new(),
            git_base: None,
            diff_path: None,
            commands_run: Vec::new(),
            tests: Vec::new(),
            decisions: Vec::new(),
            unresolved_questions: Vec::new(),
            known_failures: Vec::new(),
            current_worker: ProviderId::Gemini,
            recommended_next_worker: ProviderId::Codex,
            usage_snapshots: Vec::new(),
            concise_context_summary: "summary".to_owned(),
            created_at: Utc::now(),
            integrity_hash: String::new(),
        }
        .seal()?;
        assert!(bundle.verify_integrity()?);
        bundle.objective.push_str(" changed");
        assert!(!bundle.verify_integrity()?);
        Ok(())
    }
}
