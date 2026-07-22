use async_trait::async_trait;
use orchestrator_domain::{
    CONVERSATION_SCHEMA_VERSION, ConversationAttemptId, ConversationOutcome,
    ConversationValidationError, MessageId, ProviderId, SandboxMode, SchemaVersion, SessionId,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONVERSATION_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const CONVERSATION_MAX_EVIDENCE_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationRequest {
    pub attempt_id: ConversationAttemptId,
    pub session_id: SessionId,
    pub source_message_id: MessageId,
    pub transcript_redacted: String,
    pub repository_summary_redacted: String,
    pub sandbox: SandboxMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ConversationExit {
    Succeeded,
    QuotaExhausted,
    Crashed { exit_code: Option<i32> },
    Cancelled,
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationResponse {
    pub schema_version: SchemaVersion,
    pub attempt_id: ConversationAttemptId,
    pub session_id: SessionId,
    pub source_message_id: MessageId,
    pub provider: ProviderId,
    pub sandbox: SandboxMode,
    pub exit: ConversationExit,
    pub output_redacted: Vec<u8>,
    pub evidence_redacted: String,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ConversationFailure {
    #[error("conversation invocation failed: {reason}")]
    Invocation {
        reason: String,
        evidence_redacted: String,
    },
    #[error("conversation invocation was not read-only")]
    NotReadOnly,
    #[error("unsupported conversation response schema version `{found}`")]
    UnsupportedSchema {
        found: String,
        evidence_redacted: String,
    },
    #[error("conversation output exceeded {limit} bytes (observed {observed})")]
    OutputTooLarge {
        limit: usize,
        observed: usize,
        evidence_redacted: String,
    },
    #[error("conversation quota was exhausted")]
    QuotaExhausted { evidence_redacted: String },
    #[error("conversation lifecycle ended in {exit:?}")]
    Lifecycle {
        exit: ConversationExit,
        evidence_redacted: String,
    },
    #[error("conversation identity mismatch for {field}")]
    IdentityMismatch {
        field: &'static str,
        evidence_redacted: String,
    },
    #[error("conversation output is not one strict outcome JSON object: {reason}")]
    MalformedOutput {
        reason: String,
        evidence_redacted: String,
    },
    #[error("conversation outcome failed validation: {source}")]
    Validation {
        source: ConversationValidationError,
        evidence_redacted: String,
    },
}

#[async_trait]
pub trait ConversationOrchestrator: Send + Sync {
    async fn converse(
        &self,
        request: ConversationRequest,
    ) -> Result<ConversationResponse, ConversationFailure>;
}

/// Converts one completed read-only provider invocation into a strict conversation outcome.
///
/// # Errors
///
/// Fails closed for mutable execution, lifecycle failures, oversized or malformed JSON,
/// identity mismatches, unsupported schemas, and invalid outcome completeness.
pub fn collect_conversation_response(
    request: &ConversationRequest,
    response: ConversationResponse,
) -> Result<ConversationOutcome, ConversationFailure> {
    let evidence_redacted = bounded_evidence(&response);
    if request.sandbox != SandboxMode::ReadOnly || response.sandbox != SandboxMode::ReadOnly {
        return Err(ConversationFailure::NotReadOnly);
    }
    if response.schema_version.as_str() != CONVERSATION_SCHEMA_VERSION {
        return Err(ConversationFailure::UnsupportedSchema {
            found: response.schema_version.to_string(),
            evidence_redacted,
        });
    }
    for (field, mismatch) in [
        ("attempt_id", response.attempt_id != request.attempt_id),
        ("session_id", response.session_id != request.session_id),
        (
            "source_message_id",
            response.source_message_id != request.source_message_id,
        ),
    ] {
        if mismatch {
            return Err(ConversationFailure::IdentityMismatch {
                field,
                evidence_redacted,
            });
        }
    }
    match response.exit {
        ConversationExit::Succeeded => {}
        ConversationExit::QuotaExhausted => {
            return Err(ConversationFailure::QuotaExhausted { evidence_redacted });
        }
        exit => {
            return Err(ConversationFailure::Lifecycle {
                exit,
                evidence_redacted,
            });
        }
    }
    if response.output_redacted.len() > CONVERSATION_MAX_OUTPUT_BYTES {
        return Err(ConversationFailure::OutputTooLarge {
            limit: CONVERSATION_MAX_OUTPUT_BYTES,
            observed: response.output_redacted.len(),
            evidence_redacted,
        });
    }
    let outcome: ConversationOutcome =
        serde_json::from_slice(&response.output_redacted).map_err(|error| {
            ConversationFailure::MalformedOutput {
                reason: error.to_string(),
                evidence_redacted: evidence_redacted.clone(),
            }
        })?;
    outcome
        .validate()
        .map_err(|source| ConversationFailure::Validation {
            source,
            evidence_redacted,
        })?;
    Ok(outcome)
}

fn bounded_evidence(response: &ConversationResponse) -> String {
    const TRUNCATED: &str = "[truncated]";
    let content_limit = CONVERSATION_MAX_EVIDENCE_BYTES.saturating_sub(TRUNCATED.len());
    let mut evidence = response
        .evidence_redacted
        .chars()
        .take(content_limit)
        .collect::<String>();
    let remaining = content_limit.saturating_sub(evidence.len());
    if remaining > 0 {
        let output = String::from_utf8_lossy(
            &response.output_redacted[..response.output_redacted.len().min(remaining)],
        );
        evidence.extend(output.chars().take(remaining));
    }
    if evidence.len() < response.evidence_redacted.len()
        || response.output_redacted.len() > remaining
    {
        evidence.push_str(TRUNCATED);
    }
    evidence
}
