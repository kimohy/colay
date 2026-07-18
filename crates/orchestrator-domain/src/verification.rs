use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ProviderId, RepoPath, SchemaVersion, TaskId, VerificationId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Pass,
    Fail,
    NeedsApproval,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationCheckKind {
    GitDiff,
    ChangedFiles,
    Scope,
    CommandExit,
    Test,
    Lint,
    TypeCheck,
    Build,
    AcceptanceCriterion,
    UnresolvedTodo,
    SecretScan,
    IndependentReview,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationCheck {
    pub kind: VerificationCheckKind,
    pub name: String,
    pub status: VerificationStatus,
    pub detail: Option<String>,
    pub evidence_paths: Vec<RepoPath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceEvidence {
    pub criterion: String,
    pub status: VerificationStatus,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationResult {
    pub schema_version: SchemaVersion,
    pub verification_id: VerificationId,
    pub task_id: TaskId,
    pub implementation_provider: ProviderId,
    pub reviewer_provider: Option<ProviderId>,
    pub status: VerificationStatus,
    pub checks: Vec<VerificationCheck>,
    pub acceptance_criteria: Vec<AcceptanceEvidence>,
    pub changed_files: Vec<RepoPath>,
    pub out_of_scope_files: Vec<RepoPath>,
    pub unresolved_todos: Vec<String>,
    pub requires_approval: bool,
    pub verified_at: DateTime<Utc>,
}

impl VerificationResult {
    #[must_use]
    pub fn passes_completion_gate(&self, independent_review_required: bool) -> bool {
        self.status == VerificationStatus::Pass
            && !self.requires_approval
            && self.out_of_scope_files.is_empty()
            && self
                .checks
                .iter()
                .all(|check| check.status == VerificationStatus::Pass)
            && self
                .acceptance_criteria
                .iter()
                .all(|criterion| criterion.status == VerificationStatus::Pass)
            && (!independent_review_required
                || self.reviewer_provider.is_some_and(|reviewer| {
                    reviewer != self.implementation_provider
                        && self.checks.iter().any(|check| {
                            check.kind == VerificationCheckKind::IndependentReview
                                && check.status == VerificationStatus::Pass
                        })
                }))
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn review_must_come_from_another_provider() {
        let result = VerificationResult {
            schema_version: SchemaVersion::v1(),
            verification_id: VerificationId::new(),
            task_id: TaskId::new(),
            implementation_provider: ProviderId::Codex,
            reviewer_provider: Some(ProviderId::Codex),
            status: VerificationStatus::Pass,
            checks: vec![VerificationCheck {
                kind: VerificationCheckKind::IndependentReview,
                name: "review".to_owned(),
                status: VerificationStatus::Pass,
                detail: None,
                evidence_paths: Vec::new(),
            }],
            acceptance_criteria: Vec::new(),
            changed_files: Vec::new(),
            out_of_scope_files: Vec::new(),
            unresolved_todos: Vec::new(),
            requires_approval: false,
            verified_at: Utc::now(),
        };
        assert!(!result.passes_completion_gate(true));
    }
}
