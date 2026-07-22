use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    GraphRevisionId, MessageId, ProviderId, RequirementRevisionId, SchemaVersion, SessionId,
    canonical_sha256,
};

pub const CONVERSATION_SCHEMA_VERSION: &str = SchemaVersion::V1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementSnapshot {
    pub objective: String,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub verification_plan: Vec<String>,
    pub open_questions: Vec<String>,
}

impl RequirementSnapshot {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.objective.trim().is_empty()
            && !self.acceptance_criteria.is_empty()
            && !self.verification_plan.is_empty()
            && self.open_questions.is_empty()
            && all_non_blank(&self.constraints)
            && all_non_blank(&self.acceptance_criteria)
            && all_non_blank(&self.verification_plan)
    }

    fn validate(&self) -> Result<(), ConversationValidationError> {
        if self.objective.trim().is_empty() {
            return Err(ConversationValidationError::BlankObjective);
        }
        if !all_non_blank(&self.constraints)
            || !all_non_blank(&self.acceptance_criteria)
            || !all_non_blank(&self.verification_plan)
            || !all_non_blank(&self.open_questions)
        {
            return Err(ConversationValidationError::BlankRequirementItem);
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
    pub graph_revision_id: GraphRevisionId,
    pub git_root_redacted: String,
    pub base_commit: String,
    pub eligible_providers: Vec<ProviderId>,
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
    #[error("validation evidence must contain non-blank checks")]
    MissingValidationChecks,
    #[error("provider `{provider}` is not eligible for the validated proposal")]
    IneligibleProvider { provider: ProviderId },
    #[error("cannot seal conversation authority: {message}")]
    Integrity { message: String },
}

fn all_non_blank(values: &[String]) -> bool {
    values.iter().all(|value| !value.trim().is_empty())
}
