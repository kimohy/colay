use chrono::Utc;
use codex_compat::{CodexItem, CodexItemPhase, CompatEvent, QuotaErrorKind};
use orchestrator_domain::{
    ProviderId, QuotaPeriod, QuotaScope, RepoPath, UsageConfidence, UsageObservation, UsageSource,
    UsageUnit, WorkerEvent,
};
use serde_json::Value;

use crate::ProviderError;

#[must_use]
pub fn classify_provider_quota(text: &str, value: Option<&Value>) -> Option<QuotaErrorKind> {
    codex_compat::classify_quota_error(text, value)
}

pub(crate) fn normalize_codex_event(event: CompatEvent) -> Result<WorkerEvent, ProviderError> {
    let completed_usage = codex_usage_observation(&event);
    match event {
        CompatEvent::ThreadStarted { thread_id } => Ok(WorkerEvent::Started {
            session_id: Some(thread_id),
        }),
        CompatEvent::TurnStarted => Ok(WorkerEvent::Unknown {
            event_type: "turn.started".to_owned(),
            payload: serde_json::json!({}),
            affects_lifecycle: false,
        }),
        CompatEvent::TurnCompleted { .. } => Ok(WorkerEvent::Completed {
            summary: None,
            usage: completed_usage,
        }),
        CompatEvent::TurnFailed { message, quota } => {
            if quota.is_some_and(is_exhausting_quota) {
                Ok(WorkerEvent::QuotaExceeded {
                    detail: Some(message),
                })
            } else {
                Ok(WorkerEvent::Error {
                    code: Some(
                        if quota == Some(QuotaErrorKind::RateLimit) {
                            "rate_limited"
                        } else {
                            "turn_failed"
                        }
                        .to_owned(),
                    ),
                    message,
                    retryable: quota == Some(QuotaErrorKind::RateLimit),
                })
            }
        }
        CompatEvent::Error {
            code,
            message,
            quota,
        } => {
            if quota.is_some_and(is_exhausting_quota) {
                Ok(WorkerEvent::QuotaExceeded {
                    detail: Some(message),
                })
            } else {
                Ok(WorkerEvent::Error {
                    code: if quota == Some(QuotaErrorKind::RateLimit) {
                        Some("rate_limited".to_owned())
                    } else {
                        code
                    },
                    message,
                    retryable: true,
                })
            }
        }
        CompatEvent::Item { phase, item } => normalize_codex_item(phase, item),
        CompatEvent::Opaque { event_type, raw } => Ok(WorkerEvent::Unknown {
            event_type,
            payload: raw,
            affects_lifecycle: false,
        }),
    }
}

fn normalize_codex_item(
    phase: CodexItemPhase,
    item: CodexItem,
) -> Result<WorkerEvent, ProviderError> {
    match item {
        CodexItem::AgentMessage { text, .. } => Ok(WorkerEvent::Message { text }),
        CodexItem::Reasoning { id, text } => Ok(WorkerEvent::Unknown {
            event_type: "codex.reasoning".to_owned(),
            payload: serde_json::json!({ "id": id, "text": text }),
            affects_lifecycle: false,
        }),
        CodexItem::CommandExecution {
            id,
            command,
            exit_code,
            ..
        } => {
            let command_id = id.unwrap_or_else(|| "codex-command".to_owned());
            if phase == CodexItemPhase::Completed {
                Ok(WorkerEvent::CommandCompleted {
                    command_id,
                    exit_code,
                })
            } else {
                Ok(WorkerEvent::CommandStarted {
                    command_id,
                    // This is evidence text only. It is never re-executed or
                    // split into a shell argv.
                    executable: command.unwrap_or_else(|| "unknown".to_owned()),
                    args: Vec::new(),
                })
            }
        }
        CodexItem::FileChange {
            path: Some(path), ..
        } => Ok(WorkerEvent::FileChanged {
            path: RepoPath::try_from(path)?,
        }),
        CodexItem::FileChange {
            id,
            path: None,
            status,
        } => Ok(WorkerEvent::Unknown {
            event_type: "codex.file_change_without_path".to_owned(),
            payload: serde_json::json!({ "id": id, "status": status }),
            affects_lifecycle: false,
        }),
        CodexItem::McpToolCall {
            id,
            server,
            tool,
            status,
        } => Ok(WorkerEvent::Unknown {
            event_type: "codex.mcp_tool_call".to_owned(),
            payload: serde_json::json!({
                "id": id,
                "server": server,
                "tool": tool,
                "status": status,
            }),
            affects_lifecycle: false,
        }),
        CodexItem::WebSearch { id, query } => Ok(WorkerEvent::Unknown {
            event_type: "codex.web_search".to_owned(),
            payload: serde_json::json!({ "id": id, "query": query }),
            affects_lifecycle: false,
        }),
        CodexItem::Plan { text, .. } => Ok(WorkerEvent::CheckpointClaim {
            summary: text.unwrap_or_else(|| "Codex emitted a plan update".to_owned()),
        }),
        CodexItem::Unknown { item_type, raw } => Ok(WorkerEvent::Unknown {
            event_type: format!("codex.item.{item_type}"),
            payload: raw,
            affects_lifecycle: false,
        }),
    }
}

#[allow(clippy::cast_precision_loss)]
pub(crate) fn codex_usage_observation(event: &CompatEvent) -> Option<UsageObservation> {
    let CompatEvent::TurnCompleted { usage } = event else {
        return None;
    };
    let amount = usage.total_observed_tokens()? as f64;
    Some(UsageObservation {
        provider: ProviderId::Codex,
        quota_scope: QuotaScope::new("execution_ledger", QuotaPeriod::Custom, UsageUnit::Tokens),
        amount,
        observed_at: Utc::now(),
        source: UsageSource::LocalLedger,
        confidence: UsageConfidence::Confirmed,
    })
}

/// Normalizes one Claude `stream-json` value.
///
/// # Errors
///
/// Returns [`ProviderError`] if the event has no type or violates a required
/// normalized field contract.
pub fn parse_claude_event(value: Value) -> Result<WorkerEvent, ProviderError> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::MalformedOutput("Claude event has no type".to_owned()))?;
    match event_type {
        "system" if value.get("subtype").and_then(Value::as_str) == Some("init") => {
            Ok(WorkerEvent::Started {
                session_id: value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        }
        "assistant" => Ok(WorkerEvent::Message {
            text: extract_claude_text(&value).unwrap_or_default(),
        }),
        "stream_event" => Ok(WorkerEvent::Message {
            text: value
                .pointer("/event/delta/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        "result" => {
            let usage = claude_usage_observation(&value);
            let text = value
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let is_error = value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_error {
                let message = text.unwrap_or_else(|| "Claude result reported an error".to_owned());
                Ok(quota_or_error(message, value.get("error"), "claude_result"))
            } else {
                Ok(WorkerEvent::Completed {
                    summary: text,
                    usage,
                })
            }
        }
        "error" => {
            let message = extract_error_message(&value)
                .unwrap_or_else(|| "Claude emitted an error".to_owned());
            Ok(quota_or_error(message, value.get("error"), "claude_error"))
        }
        unknown => Ok(WorkerEvent::Unknown {
            event_type: format!("claude.{unknown}"),
            payload: value,
            affects_lifecycle: false,
        }),
    }
}

/// Normalizes one Gemini `stream-json` value.
///
/// # Errors
///
/// Returns [`ProviderError`] if the event has no type, lacks a required field,
/// or reports an unsafe repository path.
pub fn parse_gemini_event(value: Value) -> Result<WorkerEvent, ProviderError> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::MalformedOutput("Gemini event has no type".to_owned()))?;
    match event_type {
        "init" | "session.started" => Ok(WorkerEvent::Started {
            session_id: value
                .get("session_id")
                .or_else(|| value.get("sessionId"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        "message" => Ok(WorkerEvent::Message {
            text: value
                .get("content")
                .or_else(|| value.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        "tool_use" | "tool.started" => Ok(WorkerEvent::CommandStarted {
            command_id: event_id(&value, "gemini-tool"),
            executable: value
                .get("name")
                .or_else(|| value.get("tool_name"))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned(),
            args: value
                .get("parameters")
                .map(ToString::to_string)
                .into_iter()
                .collect(),
        }),
        "tool_result" | "tool.completed" => Ok(WorkerEvent::CommandCompleted {
            command_id: event_id(&value, "gemini-tool"),
            exit_code: value
                .get("exit_code")
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok()),
        }),
        "file_change" => {
            let path = value.get("path").and_then(Value::as_str).ok_or_else(|| {
                ProviderError::MalformedOutput("file change has no path".to_owned())
            })?;
            Ok(WorkerEvent::FileChanged {
                path: RepoPath::try_from(path.to_owned())?,
            })
        }
        "result" | "completed" => {
            let usage = gemini_usage_observation(&value);
            Ok(WorkerEvent::Completed {
                summary: value
                    .get("result")
                    .or_else(|| value.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                usage,
            })
        }
        "error" => {
            let message = extract_error_message(&value)
                .unwrap_or_else(|| "Gemini emitted an error".to_owned());
            Ok(quota_or_error(message, value.get("error"), "gemini_error"))
        }
        unknown => Ok(WorkerEvent::Unknown {
            event_type: format!("gemini.{unknown}"),
            payload: value,
            affects_lifecycle: false,
        }),
    }
}

fn quota_or_error(message: String, structured: Option<&Value>, code: &str) -> WorkerEvent {
    let quota = classify_provider_quota(&message, structured);
    if quota.is_some_and(is_exhausting_quota) {
        WorkerEvent::QuotaExceeded {
            detail: Some(message),
        }
    } else {
        WorkerEvent::Error {
            code: Some(
                if quota == Some(QuotaErrorKind::RateLimit) {
                    "rate_limited"
                } else {
                    code
                }
                .to_owned(),
            ),
            message,
            retryable: true,
        }
    }
}

fn is_exhausting_quota(kind: QuotaErrorKind) -> bool {
    !matches!(kind, QuotaErrorKind::RateLimit)
}

fn extract_claude_text(value: &Value) -> Option<String> {
    if let Some(text) = value
        .pointer("/message/content/0/text")
        .and_then(Value::as_str)
    {
        return Some(text.to_owned());
    }
    value
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn extract_error_message(value: &Value) -> Option<String> {
    value
        .get("message")
        .or_else(|| value.pointer("/error/message"))
        .or_else(|| value.get("error"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn event_id(value: &Value, fallback: &str) -> String {
    value
        .get("id")
        .or_else(|| value.get("tool_id"))
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_owned()
}

fn claude_usage_observation(value: &Value) -> Option<UsageObservation> {
    let usage = value.get("usage")?;
    let amount = first_u64_field(usage, &["total_tokens", "totalTokens"]).or_else(|| {
        checked_sum_u64_fields(
            usage,
            &[
                "input_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
                "output_tokens",
            ],
        )
    })?;
    Some(local_token_observation(ProviderId::Claude, amount))
}

fn gemini_usage_observation(value: &Value) -> Option<UsageObservation> {
    // Gemini's final stream-json result exposes an inclusive total under
    // `stats`. The other accepted paths are public structured usage aliases.
    // We deliberately require an explicit total instead of guessing from
    // provider-specific subcategories such as cached or thought tokens.
    let amount = [
        "/stats/total_tokens",
        "/stats/totalTokens",
        "/usage/total_tokens",
        "/usage/totalTokens",
        "/usage_metadata/total_token_count",
        "/usageMetadata/totalTokenCount",
    ]
    .into_iter()
    .find_map(|pointer| value.pointer(pointer).and_then(Value::as_u64))?;
    Some(local_token_observation(ProviderId::Gemini, amount))
}

fn local_token_observation(provider: ProviderId, amount: u64) -> UsageObservation {
    UsageObservation {
        provider,
        quota_scope: QuotaScope::new("execution_ledger", QuotaPeriod::Custom, UsageUnit::Tokens),
        #[allow(clippy::cast_precision_loss)]
        amount: amount as f64,
        observed_at: Utc::now(),
        source: UsageSource::LocalLedger,
        confidence: UsageConfidence::Confirmed,
    }
}

fn first_u64_field(value: &Value, fields: &[&str]) -> Option<u64> {
    fields
        .iter()
        .find_map(|field| value.get(field).and_then(Value::as_u64))
}

fn checked_sum_u64_fields(value: &Value, fields: &[&str]) -> Option<u64> {
    let mut total = 0_u64;
    let mut observed = false;
    for field in fields {
        if let Some(amount) = value.get(field).and_then(Value::as_u64) {
            total = total.checked_add(amount)?;
            observed = true;
        }
    }
    observed.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_completed_ledger(
        event: &Result<WorkerEvent, ProviderError>,
        expected_provider: ProviderId,
        expected_amount: f64,
    ) {
        assert!(matches!(
            event,
            Ok(WorkerEvent::Completed {
                usage: Some(usage),
                ..
            }) if usage.provider == expected_provider
                && (usage.amount - expected_amount).abs() < f64::EPSILON
                && usage.quota_scope.period == QuotaPeriod::Custom
                && usage.quota_scope.unit == UsageUnit::Tokens
                && usage.source == UsageSource::LocalLedger
                && usage.confidence == UsageConfidence::Confirmed
        ));
    }

    fn last_fixture_line(fixture: &str) -> Result<&str, ProviderError> {
        fixture
            .lines()
            .rfind(|line| !line.trim().is_empty())
            .ok_or_else(|| ProviderError::MalformedOutput("empty test fixture".to_owned()))
    }

    fn last_fixture_value(fixture: &str) -> Result<Value, ProviderError> {
        let line = last_fixture_line(fixture)?;
        serde_json::from_str(line)
            .map_err(|error| ProviderError::MalformedOutput(error.to_string()))
    }

    #[test]
    fn codex_completion_preserves_fixture_usage_as_local_ledger() {
        let event = last_fixture_line(include_str!("../../../fixtures/codex/jsonl-success.jsonl"))
            .and_then(|line| {
                codex_compat::CodexEventParser
                    .parse_line(1, line)
                    .map_err(ProviderError::from)
            })
            .and_then(normalize_codex_event);

        assert_completed_ledger(&event, ProviderId::Codex, 125.0);
    }

    #[test]
    fn claude_completion_preserves_fixture_usage_as_local_ledger() {
        let event = last_fixture_value(include_str!(
            "../../../fixtures/providers/claude/stream-success.jsonl"
        ))
        .and_then(parse_claude_event);

        assert_completed_ledger(&event, ProviderId::Claude, 20.0);
    }

    #[test]
    fn gemini_completion_preserves_fixture_usage_as_local_ledger() {
        let event = last_fixture_value(include_str!(
            "../../../fixtures/providers/gemini/stream-success.jsonl"
        ))
        .and_then(parse_gemini_event);

        assert_completed_ledger(&event, ProviderId::Gemini, 21.0);
    }

    #[test]
    fn completed_event_without_structured_usage_stays_unknown() {
        let event = parse_gemini_event(serde_json::json!({
            "type": "result",
            "result": "done"
        }));

        assert!(matches!(
            event,
            Ok(WorkerEvent::Completed { usage: None, .. })
        ));
    }

    #[test]
    fn claude_quota_is_normalized() {
        let event = parse_claude_event(serde_json::json!({
            "type": "result",
            "is_error": true,
            "result": "Monthly usage limit reached"
        }));
        assert!(matches!(event, Ok(WorkerEvent::QuotaExceeded { .. })));
    }

    #[test]
    fn transient_rate_limit_is_retryable_not_exhausted() {
        let event = parse_gemini_event(serde_json::json!({
            "type": "error",
            "message": "rate limit reached; retry later"
        }));
        assert!(matches!(
            event,
            Ok(WorkerEvent::Error {
                ref code,
                retryable: true,
                ..
            }) if code.as_deref() == Some("rate_limited")
        ));
    }

    #[test]
    fn gemini_rejects_traversal_in_file_event() {
        let event = parse_gemini_event(serde_json::json!({
            "type": "file_change",
            "path": "../outside"
        }));
        assert!(matches!(event, Err(ProviderError::UnsafePath(_))));
    }
}
