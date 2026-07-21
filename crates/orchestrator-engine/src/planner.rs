use async_trait::async_trait;
use orchestrator_domain::{
    GraphValidationError, GraphValidationPolicy, MessageId, ProviderId, SandboxMode, SchemaVersion,
    SessionId, TaskGraphProposal, ValidatedTaskGraph, validate_task_graph,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PLANNER_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const PLANNER_MAX_EVIDENCE_BYTES: usize = 16 * 1024;
pub const PLANNER_RESPONSE_SCHEMA_VERSION: &str = SchemaVersion::V1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannerRequest {
    pub session_id: SessionId,
    pub goal_message_id: MessageId,
    pub goal_redacted: String,
    pub repository_summary_redacted: String,
    pub validation_policy: GraphValidationPolicy,
    pub sandbox: SandboxMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum PlannerExit {
    Succeeded,
    QuotaExhausted,
    Crashed { exit_code: Option<i32> },
    Cancelled,
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannerResponse {
    pub schema_version: SchemaVersion,
    pub session_id: SessionId,
    pub goal_message_id: MessageId,
    pub provider: ProviderId,
    pub sandbox: SandboxMode,
    pub exit: PlannerExit,
    pub output_redacted: Vec<u8>,
    pub evidence_redacted: String,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PlannerFailure {
    #[error("planner invocation failed: {reason}")]
    Invocation {
        reason: String,
        evidence_redacted: String,
    },
    #[error("planner invocation was not read-only")]
    NotReadOnly,
    #[error("unsupported {contract} schema version `{found}`")]
    UnsupportedSchema {
        contract: &'static str,
        found: String,
        evidence_redacted: String,
    },
    #[error("planner output exceeded {limit} bytes (observed {observed})")]
    OutputTooLarge {
        limit: usize,
        observed: usize,
        evidence_redacted: String,
    },
    #[error("planner quota was exhausted")]
    QuotaExhausted { evidence_redacted: String },
    #[error("planner lifecycle ended in {exit:?}")]
    Lifecycle {
        exit: PlannerExit,
        evidence_redacted: String,
    },
    #[error("planner identity mismatch for {field}")]
    IdentityMismatch {
        field: &'static str,
        evidence_redacted: String,
    },
    #[error("planner output is not one strict task graph JSON object: {reason}")]
    MalformedOutput {
        reason: String,
        evidence_redacted: String,
    },
    #[error("planner task graph failed semantic validation: {source}")]
    Validation {
        source: GraphValidationError,
        evidence_redacted: String,
    },
}

#[async_trait]
pub trait TaskPlanner: Send + Sync {
    async fn propose(&self, request: PlannerRequest) -> Result<PlannerResponse, PlannerFailure>;
}

/// Converts one completed read-only planner invocation into a validated graph.
///
/// # Errors
///
/// Fails closed for mutable execution, lifecycle failures, oversized or non-strict JSON,
/// identity mismatches, unsupported schemas, and semantic graph validation errors.
pub fn collect_planner_response(
    request: &PlannerRequest,
    response: PlannerResponse,
) -> Result<ValidatedTaskGraph, PlannerFailure> {
    let evidence_redacted = bounded_evidence(&response);
    if request.sandbox != SandboxMode::ReadOnly || response.sandbox != SandboxMode::ReadOnly {
        return Err(PlannerFailure::NotReadOnly);
    }
    if response.schema_version.as_str() != PLANNER_RESPONSE_SCHEMA_VERSION {
        return Err(PlannerFailure::UnsupportedSchema {
            contract: "planner response",
            found: response.schema_version.to_string(),
            evidence_redacted,
        });
    }
    if response.session_id != request.session_id {
        return Err(PlannerFailure::IdentityMismatch {
            field: "session_id",
            evidence_redacted,
        });
    }
    if response.goal_message_id != request.goal_message_id {
        return Err(PlannerFailure::IdentityMismatch {
            field: "goal_message_id",
            evidence_redacted,
        });
    }
    match response.exit {
        PlannerExit::Succeeded => {}
        PlannerExit::QuotaExhausted => {
            return Err(PlannerFailure::QuotaExhausted { evidence_redacted });
        }
        exit => {
            return Err(PlannerFailure::Lifecycle {
                exit,
                evidence_redacted,
            });
        }
    }
    if response.output_redacted.len() > PLANNER_MAX_OUTPUT_BYTES {
        return Err(PlannerFailure::OutputTooLarge {
            limit: PLANNER_MAX_OUTPUT_BYTES,
            observed: response.output_redacted.len(),
            evidence_redacted,
        });
    }
    let proposal: TaskGraphProposal =
        serde_json::from_slice(&response.output_redacted).map_err(|error| {
            PlannerFailure::MalformedOutput {
                reason: error.to_string(),
                evidence_redacted: evidence_redacted.clone(),
            }
        })?;
    if proposal.schema_version.as_str() != orchestrator_domain::TASK_GRAPH_SCHEMA_VERSION {
        return Err(PlannerFailure::UnsupportedSchema {
            contract: "task graph",
            found: proposal.schema_version.to_string(),
            evidence_redacted,
        });
    }
    if proposal.session_id != request.session_id {
        return Err(PlannerFailure::IdentityMismatch {
            field: "proposal.session_id",
            evidence_redacted,
        });
    }
    if proposal.goal_message_id != request.goal_message_id {
        return Err(PlannerFailure::IdentityMismatch {
            field: "proposal.goal_message_id",
            evidence_redacted,
        });
    }
    if proposal.planner_provider != response.provider {
        return Err(PlannerFailure::IdentityMismatch {
            field: "proposal.planner_provider",
            evidence_redacted,
        });
    }
    validate_task_graph(proposal, &request.validation_policy).map_err(|source| {
        if let GraphValidationError::UnsupportedSchema { version } = &source {
            PlannerFailure::UnsupportedSchema {
                contract: "task graph",
                found: version.clone(),
                evidence_redacted,
            }
        } else {
            PlannerFailure::Validation {
                source,
                evidence_redacted,
            }
        }
    })
}

fn bounded_evidence(response: &PlannerResponse) -> String {
    const TRUNCATED: &str = "…[truncated]";
    let content_limit = PLANNER_MAX_EVIDENCE_BYTES.saturating_sub(TRUNCATED.len());
    let mut evidence = String::with_capacity(PLANNER_MAX_EVIDENCE_BYTES);
    push_str_prefix(&mut evidence, &response.evidence_redacted, content_limit);
    let mut truncated = evidence.len() < response.evidence_redacted.len();
    if evidence.len() < content_limit {
        push_str_prefix(&mut evidence, "\noutput: ", content_limit);
        let remaining = content_limit.saturating_sub(evidence.len());
        let input_prefix =
            &response.output_redacted[..response.output_redacted.len().min(remaining)];
        let output = String::from_utf8_lossy(input_prefix);
        push_str_prefix(&mut evidence, &output, content_limit);
        truncated |= input_prefix.len() < response.output_redacted.len()
            || evidence.len()
                < response.evidence_redacted.len() + "\noutput: ".len() + output.len();
    }
    if truncated {
        evidence.push_str(TRUNCATED);
    }
    evidence
}

fn push_str_prefix(target: &mut String, value: &str, limit: usize) {
    let remaining = limit.saturating_sub(target.len());
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= remaining)
        .last()
        .unwrap_or(0);
    if value.len() <= remaining {
        target.push_str(value);
    } else {
        target.push_str(&value[..end]);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::Utc;
    use orchestrator_domain::{
        GraphRevisionId, GraphValidationPolicy, MessageId, ModelProfile, ProviderId, SandboxMode,
        SchemaVersion, SessionId,
    };
    use serde_json::json;

    use super::{
        PLANNER_MAX_OUTPUT_BYTES, PlannerExit, PlannerFailure, PlannerRequest, PlannerResponse,
        collect_planner_response,
    };

    fn request() -> PlannerRequest {
        PlannerRequest {
            session_id: SessionId::new(),
            goal_message_id: MessageId::new(),
            goal_redacted: "build chat orchestration".to_owned(),
            repository_summary_redacted: "Rust workspace".to_owned(),
            validation_policy: GraphValidationPolicy {
                eligible_providers: BTreeSet::from([ProviderId::Codex]),
                eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
                max_parallel_workers: 2,
                per_provider_limits: BTreeMap::new(),
            },
            sandbox: SandboxMode::ReadOnly,
        }
    }

    fn valid_json(request: &PlannerRequest) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": SchemaVersion::V1,
            "revision_id": GraphRevisionId::new(),
            "session_id": request.session_id,
            "goal_message_id": request.goal_message_id,
            "planner_provider": "codex",
            "proposed_at": Utc::now(),
            "nodes": [{
                "key": "domain",
                "title": "Domain contract",
                "objective": "Implement graph contracts",
                "dependencies": [],
                "constraints": ["local only"],
                "acceptance_criteria": ["tests pass"],
                "provider": "codex",
                "profile": "standard",
                "write_scopes": ["crates/domain"],
                "repository_wide_write_scope": false,
                "risks": ["concurrency"],
                "parallel_safety": "isolated scope"
            }]
        }))
        .expect("fixture serializes")
    }

    fn response(request: &PlannerRequest, output_redacted: Vec<u8>) -> PlannerResponse {
        PlannerResponse {
            schema_version: SchemaVersion::v1(),
            session_id: request.session_id,
            goal_message_id: request.goal_message_id,
            provider: ProviderId::Codex,
            sandbox: SandboxMode::ReadOnly,
            exit: PlannerExit::Succeeded,
            output_redacted,
            evidence_redacted: "fake provider exited 0".to_owned(),
        }
    }

    #[test]
    fn accepts_exactly_one_valid_json_object() {
        let request = request();
        let graph = collect_planner_response(&request, response(&request, valid_json(&request)))
            .expect("valid response");
        assert_eq!(graph.proposal.session_id, request.session_id);
        assert_eq!(graph.proposal.goal_message_id, request.goal_message_id);
    }

    #[test]
    fn rejects_fences_prose_and_multiple_json_values() {
        let request = request();
        let valid = String::from_utf8(valid_json(&request)).expect("UTF-8 fixture");
        for output in [
            format!("```json\n{valid}\n```"),
            format!("Here is the plan: {valid}"),
            format!("{valid}\n{valid}"),
        ] {
            assert!(matches!(
                collect_planner_response(&request, response(&request, output.into_bytes())),
                Err(PlannerFailure::MalformedOutput { .. })
            ));
        }
    }

    #[test]
    fn rejects_output_over_one_mib_before_parsing() {
        let request = request();
        let output = vec![b' '; PLANNER_MAX_OUTPUT_BYTES + 1];
        assert!(matches!(
            collect_planner_response(&request, response(&request, output)),
            Err(PlannerFailure::OutputTooLarge { .. })
        ));
    }

    #[test]
    fn failure_evidence_is_bounded() {
        let request = request();
        let mut malformed = response(&request, vec![b'x'; PLANNER_MAX_OUTPUT_BYTES]);
        malformed.evidence_redacted = "e".repeat(PLANNER_MAX_OUTPUT_BYTES);
        let error = collect_planner_response(&request, malformed).expect_err("must fail");
        let PlannerFailure::MalformedOutput {
            evidence_redacted, ..
        } = error
        else {
            panic!("unexpected failure: {error}");
        };
        assert!(evidence_redacted.len() <= super::PLANNER_MAX_EVIDENCE_BYTES);
        assert!(evidence_redacted.ends_with("[truncated]"));
    }

    #[test]
    fn lifecycle_quota_and_crash_fail_without_parsing_output() {
        let request = request();
        for (exit, expected_quota) in [
            (PlannerExit::QuotaExhausted, true),
            (
                PlannerExit::Crashed {
                    exit_code: Some(17),
                },
                false,
            ),
        ] {
            let mut response = response(&request, valid_json(&request));
            response.exit = exit;
            let error = collect_planner_response(&request, response).expect_err("must fail");
            assert_eq!(
                matches!(&error, PlannerFailure::QuotaExhausted { .. }),
                expected_quota
            );
            assert!(expected_quota || matches!(&error, PlannerFailure::Lifecycle { .. }));
        }
    }

    #[test]
    fn rejects_wrong_response_and_proposal_identity() {
        let request = request();
        let mut wrong_response = response(&request, valid_json(&request));
        wrong_response.session_id = SessionId::new();
        assert!(matches!(
            collect_planner_response(&request, wrong_response),
            Err(PlannerFailure::IdentityMismatch { .. })
        ));

        let mut value: serde_json::Value =
            serde_json::from_slice(&valid_json(&request)).expect("JSON fixture");
        value["goal_message_id"] = json!(MessageId::new());
        assert!(matches!(
            collect_planner_response(
                &request,
                response(
                    &request,
                    serde_json::to_vec(&value).expect("fixture serializes")
                )
            ),
            Err(PlannerFailure::IdentityMismatch { .. })
        ));
    }

    #[test]
    fn rejects_non_read_only_request_or_response() {
        let mut writable_request = request();
        writable_request.sandbox = SandboxMode::WorkspaceWrite;
        assert!(matches!(
            collect_planner_response(
                &writable_request,
                response(&writable_request, valid_json(&writable_request))
            ),
            Err(PlannerFailure::NotReadOnly)
        ));

        let request = request();
        let mut writable = response(&request, valid_json(&request));
        writable.sandbox = SandboxMode::WorkspaceWrite;
        assert!(matches!(
            collect_planner_response(&request, writable),
            Err(PlannerFailure::NotReadOnly)
        ));
    }

    #[test]
    fn rejects_future_response_and_graph_schemas() {
        let request = request();
        let mut future_response = response(&request, valid_json(&request));
        future_response.schema_version = SchemaVersion::new("999");
        assert!(matches!(
            collect_planner_response(&request, future_response),
            Err(PlannerFailure::UnsupportedSchema { .. })
        ));

        let mut value: serde_json::Value =
            serde_json::from_slice(&valid_json(&request)).expect("JSON fixture");
        value["schema_version"] = json!("999");
        assert!(matches!(
            collect_planner_response(
                &request,
                response(
                    &request,
                    serde_json::to_vec(&value).expect("fixture serializes")
                )
            ),
            Err(PlannerFailure::UnsupportedSchema { .. })
        ));
    }
}
