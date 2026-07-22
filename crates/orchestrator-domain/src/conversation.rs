use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    GraphRevisionId, MessageId, ModelProfile, ProviderId, RequirementRevisionId, SchemaVersion,
    SessionId, canonical_sha256,
};

pub const CONVERSATION_SCHEMA_VERSION: &str = SchemaVersion::V1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCommand {
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl VerificationCommand {
    fn is_safe_separated_command(&self) -> bool {
        let executable = self.executable.trim();
        if executable.is_empty()
            || executable != self.executable
            || executable.chars().any(|character| {
                character.is_whitespace()
                    || character.is_control()
                    || matches!(character, ';' | '&' | '|' | '`' | '$')
            })
            || self
                .args
                .iter()
                .any(|argument| argument.contains(['\0', '\r', '\n']))
        {
            return false;
        }
        let executable_name = executable
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(executable)
            .trim_end_matches(".exe")
            .to_ascii_lowercase();
        !matches!(
            executable_name.as_str(),
            "sh" | "bash" | "zsh" | "fish" | "cmd" | "powershell" | "pwsh"
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementSnapshot {
    pub objective: String,
    #[serde(default)]
    pub in_scope: Vec<String>,
    #[serde(default)]
    pub out_of_scope: Vec<String>,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub verification_plan: Vec<VerificationCommand>,
    #[serde(default)]
    pub risks: Vec<String>,
    pub open_questions: Vec<String>,
}

impl RequirementSnapshot {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.objective.trim().is_empty()
            && !self.in_scope.is_empty()
            && !self.acceptance_criteria.is_empty()
            && !self.verification_plan.is_empty()
            && self.open_questions.is_empty()
            && all_non_blank(&self.in_scope)
            && all_non_blank(&self.out_of_scope)
            && all_non_blank(&self.constraints)
            && all_non_blank(&self.acceptance_criteria)
            && all_non_blank(&self.risks)
            && self
                .verification_plan
                .iter()
                .all(VerificationCommand::is_safe_separated_command)
    }

    fn validate(&self) -> Result<(), ConversationValidationError> {
        if self.objective.trim().is_empty() {
            return Err(ConversationValidationError::BlankObjective);
        }
        if !all_non_blank(&self.in_scope)
            || !all_non_blank(&self.out_of_scope)
            || !all_non_blank(&self.constraints)
            || !all_non_blank(&self.acceptance_criteria)
            || !all_non_blank(&self.risks)
            || !all_non_blank(&self.open_questions)
        {
            return Err(ConversationValidationError::BlankRequirementItem);
        }
        if !self
            .verification_plan
            .iter()
            .all(VerificationCommand::is_safe_separated_command)
        {
            return Err(ConversationValidationError::UnsafeVerificationCommand);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome", deny_unknown_fields)]
pub enum ConversationOutcome {
    AnswerComplete {
        response_redacted: String,
    },
    MoreInformationNeeded {
        response_redacted: String,
        requirements: RequirementSnapshot,
    },
    WorktreeTaskCandidate {
        response_redacted: String,
        requirements: RequirementSnapshot,
    },
    NeedsAttention {
        response_redacted: String,
        evidence_redacted: String,
    },
}

impl ConversationOutcome {
    /// Validates the provider-neutral outcome contract and candidate completeness.
    ///
    /// # Errors
    ///
    /// Returns a deterministic validation error for blank content or an outcome whose
    /// requirement completeness does not match its discriminator.
    pub fn validate(&self) -> Result<(), ConversationValidationError> {
        let response = match self {
            Self::AnswerComplete { response_redacted }
            | Self::MoreInformationNeeded {
                response_redacted, ..
            }
            | Self::WorktreeTaskCandidate {
                response_redacted, ..
            }
            | Self::NeedsAttention {
                response_redacted, ..
            } => response_redacted,
        };
        if response.trim().is_empty() {
            return Err(ConversationValidationError::BlankResponse);
        }
        match self {
            Self::MoreInformationNeeded { requirements, .. } => {
                requirements.validate()?;
                if requirements.open_questions.is_empty() {
                    return Err(ConversationValidationError::MissingOpenQuestion);
                }
            }
            Self::WorktreeTaskCandidate { requirements, .. } => {
                requirements.validate()?;
                if !requirements.is_complete() {
                    return Err(ConversationValidationError::IncompleteCandidate);
                }
            }
            Self::NeedsAttention {
                evidence_redacted, ..
            } if evidence_redacted.trim().is_empty() => {
                return Err(ConversationValidationError::BlankEvidence);
            }
            Self::AnswerComplete { .. } | Self::NeedsAttention { .. } => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementRevision {
    pub schema_version: SchemaVersion,
    pub requirement_revision_id: RequirementRevisionId,
    pub session_id: SessionId,
    pub source_message_id: MessageId,
    pub ordinal: u64,
    pub snapshot: RequirementSnapshot,
    pub snapshot_hash: String,
    pub created_at: DateTime<Utc>,
}

impl RequirementRevision {
    /// Creates one immutable, canonically sealed requirement revision.
    ///
    /// # Errors
    ///
    /// Returns a validation error for an invalid ordinal or snapshot, or when canonical
    /// serialization cannot produce the snapshot hash.
    pub fn seal(
        requirement_revision_id: RequirementRevisionId,
        session_id: SessionId,
        source_message_id: MessageId,
        ordinal: u64,
        snapshot: RequirementSnapshot,
        created_at: DateTime<Utc>,
    ) -> Result<Self, ConversationValidationError> {
        if ordinal == 0 {
            return Err(ConversationValidationError::InvalidOrdinal);
        }
        snapshot.validate()?;
        let snapshot_hash = canonical_sha256(&snapshot).map_err(|error| {
            ConversationValidationError::Integrity {
                message: error.to_string(),
            }
        })?;
        Ok(Self {
            schema_version: SchemaVersion::v1(),
            requirement_revision_id,
            session_id,
            source_message_id,
            ordinal,
            snapshot,
            snapshot_hash,
            created_at,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoValidationEvidence {
    pub schema_version: SchemaVersion,
    pub requirement_revision_id: RequirementRevisionId,
    pub requirement_snapshot_hash: String,
    pub graph_revision_id: GraphRevisionId,
    pub git_root_redacted: String,
    pub base_commit: String,
    pub eligible_providers: Vec<ProviderId>,
    pub eligible_profiles: Vec<ModelProfile>,
    pub max_parallel_workers: usize,
    pub per_provider_limits: BTreeMap<ProviderId, usize>,
    pub normalized_write_scopes: Vec<String>,
    pub verification_plan: Vec<VerificationCommand>,
    pub required_approvals: Vec<String>,
    pub checks: Vec<String>,
    pub validated_at: DateTime<Utc>,
}

impl RepoValidationEvidence {
    /// Validates repository evidence for the provider selected by the graph.
    ///
    /// # Errors
    ///
    /// Returns a validation error for unsupported schema, malformed Git identity, missing
    /// checks, or an ineligible provider.
    pub fn validate_for(&self, provider: ProviderId) -> Result<(), ConversationValidationError> {
        if self.schema_version.as_str() != CONVERSATION_SCHEMA_VERSION {
            return Err(ConversationValidationError::UnsupportedSchema);
        }
        if !is_sha256(&self.requirement_snapshot_hash) {
            return Err(ConversationValidationError::InvalidRequirementSnapshotHash);
        }
        if self.git_root_redacted.trim().is_empty() {
            return Err(ConversationValidationError::BlankGitRoot);
        }
        if !matches!(self.base_commit.len(), 40 | 64)
            || !self
                .base_commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ConversationValidationError::InvalidBaseCommit);
        }
        if self.checks.is_empty() || !all_non_blank(&self.checks) {
            return Err(ConversationValidationError::MissingValidationChecks);
        }
        if self.eligible_profiles.is_empty()
            || self.max_parallel_workers == 0
            || self.per_provider_limits.values().any(|limit| *limit == 0)
            || self.normalized_write_scopes.is_empty()
            || !all_non_blank(&self.normalized_write_scopes)
            || self.verification_plan.is_empty()
            || !self
                .verification_plan
                .iter()
                .all(VerificationCommand::is_safe_separated_command)
            || self.required_approvals.is_empty()
            || !all_non_blank(&self.required_approvals)
        {
            return Err(ConversationValidationError::MissingPolicyEvidence);
        }
        if !self.eligible_providers.contains(&provider) {
            return Err(ConversationValidationError::IneligibleProvider { provider });
        }
        Ok(())
    }

    /// Returns the canonical SHA-256 seal for all repository validation evidence fields.
    ///
    /// # Errors
    ///
    /// Returns an integrity error when canonical serialization fails.
    pub fn seal(&self) -> Result<String, ConversationValidationError> {
        canonical_sha256(self).map_err(|error| ConversationValidationError::Integrity {
            message: error.to_string(),
        })
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ConversationValidationError {
    #[error("conversation response must not be blank")]
    BlankResponse,
    #[error("requirement objective must not be blank")]
    BlankObjective,
    #[error("requirement list items must not be blank")]
    BlankRequirementItem,
    #[error("verification commands must use a safe separated executable and argument list")]
    UnsafeVerificationCommand,
    #[error("more-information outcome requires at least one open question")]
    MissingOpenQuestion,
    #[error("worktree task candidate has incomplete requirements")]
    IncompleteCandidate,
    #[error("needs-attention evidence must not be blank")]
    BlankEvidence,
    #[error("requirement revision ordinal must be positive")]
    InvalidOrdinal,
    #[error("unsupported conversation schema version")]
    UnsupportedSchema,
    #[error("validated Git root must not be blank")]
    BlankGitRoot,
    #[error("base commit must be a hexadecimal Git object ID")]
    InvalidBaseCommit,
    #[error("requirement snapshot hash must be a hexadecimal SHA-256 value")]
    InvalidRequirementSnapshotHash,
    #[error("validation evidence must contain non-blank checks")]
    MissingValidationChecks,
    #[error("validation evidence is missing scope, provider, verification, or approval policy")]
    MissingPolicyEvidence,
    #[error("provider `{provider}` is not eligible for the validated proposal")]
    IneligibleProvider { provider: ProviderId },
    #[error("cannot seal conversation authority: {message}")]
    Integrity { message: String },
}

fn all_non_blank(values: &[String]) -> bool {
    values.iter().all(|value| !value.trim().is_empty())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
