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
    pub provider: ProviderId,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationFailureKind {
    Authentication,
    QuotaOrBilling,
    UnsupportedClientOrAccount,
    Timeout,
    Cancelled,
    Compatibility,
    ProcessFailure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationFailureDiagnostic {
    pub kind: ConversationFailureKind,
    pub response_redacted: String,
    pub evidence_redacted: String,
}

#[must_use]
pub fn diagnose_conversation_failure(
    provider: ProviderId,
    failure: &ConversationFailure,
) -> ConversationFailureDiagnostic {
    let evidence = failure_evidence(failure);
    let kind = match failure {
        ConversationFailure::QuotaExhausted { .. } => ConversationFailureKind::QuotaOrBilling,
        ConversationFailure::Lifecycle {
            exit: ConversationExit::TimedOut,
            ..
        } => ConversationFailureKind::Timeout,
        ConversationFailure::Lifecycle {
            exit: ConversationExit::Cancelled,
            ..
        } => ConversationFailureKind::Cancelled,
        ConversationFailure::UnsupportedSchema { .. }
        | ConversationFailure::NotReadOnly
        | ConversationFailure::OutputTooLarge { .. }
        | ConversationFailure::IdentityMismatch { .. }
        | ConversationFailure::MalformedOutput { .. }
        | ConversationFailure::Validation { .. } => ConversationFailureKind::Compatibility,
        ConversationFailure::Invocation { .. } | ConversationFailure::Lifecycle { .. } => {
            classify_redacted_evidence(&evidence)
        }
    };
    let response_redacted = match kind {
        ConversationFailureKind::Authentication => format!(
            "{provider} could not authenticate. Reauthenticate the selected provider, then retry this conversation."
        ),
        ConversationFailureKind::QuotaOrBilling => format!(
            "{provider} cannot continue because quota or billing is unavailable. Check the selected provider's quota or billing, then retry this conversation."
        ),
        ConversationFailureKind::UnsupportedClientOrAccount => format!(
            "{provider} is not supported by the installed client or current account. Update the selected provider client or use a supported account, then retry this conversation."
        ),
        ConversationFailureKind::Timeout => format!(
            "{provider} timed out. Check the selected provider's availability, then retry this conversation."
        ),
        ConversationFailureKind::Cancelled => {
            format!("{provider} was cancelled. Retry this conversation when you are ready.")
        }
        ConversationFailureKind::Compatibility => format!(
            "{provider} is incompatible with the required read-only conversation protocol. Update or reconfigure the selected provider client, then retry this conversation."
        ),
        ConversationFailureKind::ProcessFailure => format!(
            "{provider} process failed. Review the redacted evidence, then retry this conversation."
        ),
    };
    ConversationFailureDiagnostic {
        kind,
        response_redacted,
        evidence_redacted: bound_redacted_text(&evidence),
    }
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
        ("provider", response.provider != request.provider),
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

fn failure_evidence(failure: &ConversationFailure) -> String {
    let evidence_redacted = match failure {
        ConversationFailure::Invocation {
            evidence_redacted, ..
        }
        | ConversationFailure::UnsupportedSchema {
            evidence_redacted, ..
        }
        | ConversationFailure::OutputTooLarge {
            evidence_redacted, ..
        }
        | ConversationFailure::QuotaExhausted { evidence_redacted }
        | ConversationFailure::Lifecycle {
            evidence_redacted, ..
        }
        | ConversationFailure::IdentityMismatch {
            evidence_redacted, ..
        }
        | ConversationFailure::MalformedOutput {
            evidence_redacted, ..
        }
        | ConversationFailure::Validation {
            evidence_redacted, ..
        } => evidence_redacted.as_str(),
        ConversationFailure::NotReadOnly => "",
    };
    if evidence_redacted.is_empty() {
        failure.to_string()
    } else {
        format!("{failure}; {evidence_redacted}")
    }
}

fn classify_redacted_evidence(evidence: &str) -> ConversationFailureKind {
    let evidence = evidence.to_ascii_lowercase();
    if contains_any(
        &evidence,
        &[
            "token_expired",
            "authentication",
            "unauthenticated",
            "unauthorized",
            "invalid api key",
            "login required",
        ],
    ) {
        ConversationFailureKind::Authentication
    } else if contains_any(
        &evidence,
        &["quota", "billing", "credit balance", "rate limit"],
    ) {
        ConversationFailureKind::QuotaOrBilling
    } else if contains_any(
        &evidence,
        &[
            "unsupported_client",
            "unsupported client",
            "unsupported account",
            "account not supported",
        ],
    ) {
        ConversationFailureKind::UnsupportedClientOrAccount
    } else if contains_any(&evidence, &["timed out", "timeout", "deadline exceeded"]) {
        ConversationFailureKind::Timeout
    } else if contains_any(&evidence, &["cancelled", "canceled"]) {
        ConversationFailureKind::Cancelled
    } else if contains_any(
        &evidence,
        &[
            "no supported transport",
            "unsupported schema",
            "incompatible",
            "malformed output",
        ],
    ) {
        ConversationFailureKind::Compatibility
    } else {
        ConversationFailureKind::ProcessFailure
    }
}

fn contains_any(value: &str, candidates: &[&str]) -> bool {
    candidates.iter().any(|candidate| value.contains(candidate))
}

fn bound_redacted_text(value: &str) -> String {
    const TRUNCATED: &str = "[truncated]";
    if value.len() <= CONVERSATION_MAX_EVIDENCE_BYTES {
        return value.to_owned();
    }
    let mut end = CONVERSATION_MAX_EVIDENCE_BYTES.saturating_sub(TRUNCATED.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{TRUNCATED}", &value[..end])
}
