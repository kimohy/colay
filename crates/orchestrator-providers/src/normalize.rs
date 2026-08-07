use std::collections::HashSet;

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

/// Reduces provider-authored diagnostic text to stable, inert evidence.
#[must_use]
pub fn normalize_provider_diagnostic(provider: ProviderId, input: &str) -> String {
    const SAFE_PERMISSION_BOUNDARY: &str =
        "provider requested an unsafe permission bypass; Colay did not enable it";
    const MAX_STACK_FRAMES: usize = 4;

    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    let mut retained_stack_frames = 0_usize;
    let mut omitted_stack_frames = 0_usize;

    for raw_line in input.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let line = if line.contains("--dangerously-skip-permissions") {
            SAFE_PERMISSION_BOUNDARY
        } else {
            line
        };
        if !seen.insert(line.to_owned()) {
            continue;
        }
        if is_provider_stack_frame(provider, line) {
            if retained_stack_frames >= MAX_STACK_FRAMES {
                omitted_stack_frames = omitted_stack_frames.saturating_add(1);
                continue;
            }
            retained_stack_frames = retained_stack_frames.saturating_add(1);
        }
        normalized.push(line.to_owned());
    }

    if omitted_stack_frames > 0 {
        normalized.push(format!(
            "[{omitted_stack_frames} provider stack frames omitted]"
        ));
    }
    normalized.join("\n")
}

fn is_provider_stack_frame(provider: ProviderId, line: &str) -> bool {
    matches!(
        provider,
        ProviderId::Claude | ProviderId::Gemini | ProviderId::Agy
    ) && line.starts_with("at ")
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
                let retryable = code.as_deref() != Some("app_server_protocol_error");
                Ok(WorkerEvent::Error {
                    code: if quota == Some(QuotaErrorKind::RateLimit) {
                        Some("rate_limited".to_owned())
                    } else {
                        code
                    },
                    message,
                    retryable,
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
        // Colay does not request Claude's partial-message mode. A stream delta is
        // therefore diagnostic protocol data, not a complete semantic message;
        // treating arbitrary chunks as messages would invent newline boundaries.
        "stream_event" => Ok(WorkerEvent::Unknown {
            event_type: "claude.stream_event".to_owned(),
            payload: value,
            affects_lifecycle: false,
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
        "init" | "session.started" => Ok(gemini_started(&value)),
        "message" => parse_gemini_message(value),
        "tool_use" | "tool.started" => Ok(gemini_command_started(&value)),
        "tool_result" | "tool.completed" => Ok(gemini_command_completed(&value)),
        "file_change" => parse_gemini_file_change(&value),
        "result" | "completed" => parse_gemini_result(&value),
        "error" => parse_gemini_error(value),
        unknown => Ok(WorkerEvent::Unknown {
            event_type: format!("gemini.{unknown}"),
            payload: value,
            affects_lifecycle: false,
        }),
    }
}

fn gemini_started(value: &Value) -> WorkerEvent {
    WorkerEvent::Started {
        session_id: value
            .get("session_id")
            .or_else(|| value.get("sessionId"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn parse_gemini_message(value: Value) -> Result<WorkerEvent, ProviderError> {
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::MalformedOutput("Gemini message has no role".to_owned()))?;
    let delta = gemini_optional_delta(&value)?;
    let text = gemini_message_text(&value)?;
    if role == "user" {
        return Ok(WorkerEvent::Unknown {
            event_type: "gemini.message.user".to_owned(),
            payload: value,
            affects_lifecycle: false,
        });
    }
    if role != "assistant" {
        return Err(ProviderError::MalformedOutput(format!(
            "Gemini message has unsupported role: {role}"
        )));
    }
    Ok(if delta {
        WorkerEvent::MessageDelta { text }
    } else {
        WorkerEvent::Message { text }
    })
}

fn gemini_optional_delta(value: &Value) -> Result<bool, ProviderError> {
    match value.get("delta") {
        None => Ok(false),
        Some(Value::Bool(delta)) => Ok(*delta),
        Some(_) => Err(ProviderError::MalformedOutput(
            "Gemini message delta flag is not boolean".to_owned(),
        )),
    }
}

fn gemini_message_text(value: &Value) -> Result<String, ProviderError> {
    value
        .get("content")
        .or_else(|| value.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ProviderError::MalformedOutput("Gemini message has no text".to_owned()))
}

fn gemini_command_started(value: &Value) -> WorkerEvent {
    WorkerEvent::CommandStarted {
        command_id: event_id(value, "gemini-tool"),
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
    }
}

fn gemini_command_completed(value: &Value) -> WorkerEvent {
    WorkerEvent::CommandCompleted {
        command_id: event_id(value, "gemini-tool"),
        exit_code: value
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(|code| i32::try_from(code).ok()),
    }
}

fn parse_gemini_file_change(value: &Value) -> Result<WorkerEvent, ProviderError> {
    let path = value
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::MalformedOutput("file change has no path".to_owned()))?;
    Ok(WorkerEvent::FileChanged {
        path: RepoPath::try_from(path.to_owned())?,
    })
}

fn parse_gemini_result(value: &Value) -> Result<WorkerEvent, ProviderError> {
    match gemini_result_status(value)? {
        Some("error") => {
            let message = extract_error_message(value)
                .unwrap_or_else(|| "Gemini emitted an error result".to_owned());
            Ok(quota_or_error(message, value.get("error"), "gemini_error"))
        }
        Some("success") if gemini_has_error_payload(value) => Err(ProviderError::MalformedOutput(
            "Gemini success result contains an error payload".to_owned(),
        )),
        None if gemini_has_error_payload(value) => Err(ProviderError::MalformedOutput(
            "Gemini result without status contains an error payload".to_owned(),
        )),
        Some("success") | None => Ok(WorkerEvent::Completed {
            summary: value
                .get("result")
                .or_else(|| value.get("text"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            usage: gemini_usage_observation(value),
        }),
        Some(status) => Err(ProviderError::MalformedOutput(format!(
            "Gemini result has unsupported status: {status}"
        ))),
    }
}

fn gemini_result_status(value: &Value) -> Result<Option<&str>, ProviderError> {
    value
        .get("status")
        .map(|status| {
            status.as_str().ok_or_else(|| {
                ProviderError::MalformedOutput("Gemini result status is not a string".to_owned())
            })
        })
        .transpose()
}

fn gemini_has_error_payload(value: &Value) -> bool {
    match value.get("error") {
        None | Some(Value::Null) => false,
        Some(Value::String(error)) => !error.is_empty(),
        Some(Value::Array(errors)) => !errors.is_empty(),
        Some(Value::Object(error)) => !error.is_empty(),
        Some(Value::Bool(_) | Value::Number(_)) => true,
    }
}

fn parse_gemini_error(value: Value) -> Result<WorkerEvent, ProviderError> {
    let severity = value
        .get("severity")
        .map(|severity| {
            severity.as_str().ok_or_else(|| {
                ProviderError::MalformedOutput("Gemini error severity is not a string".to_owned())
            })
        })
        .transpose()?;
    match severity {
        Some("warning") => Ok(WorkerEvent::Unknown {
            event_type: "gemini.warning".to_owned(),
            payload: value,
            affects_lifecycle: false,
        }),
        Some("error") | None => {
            let message = extract_error_message(&value)
                .unwrap_or_else(|| "Gemini emitted an error".to_owned());
            Ok(quota_or_error(message, value.get("error"), "gemini_error"))
        }
        Some(severity) => Err(ProviderError::MalformedOutput(format!(
            "Gemini error has unsupported severity: {severity}"
        ))),
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
    fn codex_app_server_protocol_error_is_never_retryable() {
        let event = normalize_codex_event(CompatEvent::Error {
            code: Some("app_server_protocol_error".to_owned()),
            message: "turn dispatch outcome is unknown".to_owned(),
            quota: None,
        });

        assert!(matches!(
            event,
            Ok(WorkerEvent::Error {
                code: Some(ref code),
                retryable: false,
                ..
            }) if code == "app_server_protocol_error"
        ));
    }

    #[test]
    fn claude_partial_stream_delta_is_not_a_semantic_message() {
        let payload = serde_json::json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "delta": {"type": "text_delta", "text": "42"}
            }
        });
        let event = parse_claude_event(payload.clone());

        assert!(matches!(
            &event,
            Ok(WorkerEvent::Unknown {
                event_type,
                affects_lifecycle: false,
                ..
            }) if event_type == "claude.stream_event"
        ));
        if let Ok(WorkerEvent::Unknown {
            payload: normalized_payload,
            ..
        }) = event
        {
            assert_eq!(normalized_payload, payload);
        }
    }

    #[test]
    fn gemini_assistant_delta_preserves_the_continuation_contract() {
        let event = parse_gemini_event(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": "partial",
            "delta": true,
            "timestamp": "2026-08-07T00:00:00Z"
        }));

        assert!(matches!(
            event,
            Ok(WorkerEvent::MessageDelta { ref text }) if text == "partial"
        ));
    }

    #[test]
    fn gemini_assistant_message_without_delta_is_complete() {
        let event = parse_gemini_event(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": "complete",
            "timestamp": "2026-08-07T00:00:00Z"
        }));

        assert!(matches!(
            event,
            Ok(WorkerEvent::Message { ref text }) if text == "complete"
        ));
    }

    #[test]
    fn gemini_assistant_message_with_false_delta_is_complete() {
        let event = parse_gemini_event(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": "complete",
            "delta": false,
            "timestamp": "2026-08-07T00:00:00Z"
        }));

        assert!(matches!(
            event,
            Ok(WorkerEvent::Message { ref text }) if text == "complete"
        ));
    }

    #[test]
    fn gemini_user_echo_is_not_a_semantic_provider_message() {
        let payload = serde_json::json!({
            "type": "message",
            "role": "user",
            "content": "original prompt"
        });
        let event = parse_gemini_event(payload.clone());

        assert!(matches!(
            &event,
            Ok(WorkerEvent::Unknown {
                event_type,
                affects_lifecycle: false,
                ..
            }) if event_type == "gemini.message.user"
        ));
        if let Ok(WorkerEvent::Unknown {
            payload: normalized_payload,
            ..
        }) = event
        {
            assert_eq!(normalized_payload, payload);
        }
    }

    #[test]
    fn gemini_user_echo_still_requires_content_and_a_boolean_delta() {
        for payload in [
            serde_json::json!({
                "type": "message",
                "role": "user",
                "timestamp": "2026-08-07T00:00:00Z"
            }),
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": 42,
                "timestamp": "2026-08-07T00:00:00Z"
            }),
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": "prompt",
                "delta": "false",
                "timestamp": "2026-08-07T00:00:00Z"
            }),
        ] {
            assert!(matches!(
                parse_gemini_event(payload),
                Err(ProviderError::MalformedOutput(_))
            ));
        }
    }

    #[test]
    fn gemini_error_result_is_not_treated_as_completion() {
        let event = parse_gemini_event(serde_json::json!({
            "type": "result",
            "status": "error",
            "error": {
                "type": "AUTHENTICATION_ERROR",
                "message": "authentication required"
            }
        }));

        assert!(matches!(
            event,
            Ok(WorkerEvent::Error {
                code: Some(ref code),
                ref message,
                retryable: true
            }) if code == "gemini_error" && message == "authentication required"
        ));
    }

    #[test]
    fn gemini_success_result_with_error_payload_is_rejected() {
        let event = parse_gemini_event(serde_json::json!({
            "type": "result",
            "status": "success",
            "result": "done",
            "error": { "message": "contradictory failure" }
        }));

        assert!(matches!(event, Err(ProviderError::MalformedOutput(_))));
    }

    #[test]
    fn gemini_result_without_status_cannot_hide_an_error_payload() {
        let event = parse_gemini_event(serde_json::json!({
            "type": "result",
            "error": { "message": "authentication failed" }
        }));

        assert!(matches!(event, Err(ProviderError::MalformedOutput(_))));
    }

    #[test]
    fn gemini_warning_event_is_non_lifecycle_protocol_evidence() {
        let payload = serde_json::json!({
            "type": "error",
            "severity": "warning",
            "message": "Loop detected, stopping execution"
        });
        let event = parse_gemini_event(payload.clone());

        assert!(matches!(
            &event,
            Ok(WorkerEvent::Unknown {
                event_type,
                affects_lifecycle: false,
                ..
            }) if event_type == "gemini.warning"
        ));
        if let Ok(WorkerEvent::Unknown {
            payload: normalized_payload,
            ..
        }) = event
        {
            assert_eq!(normalized_payload, payload);
        }
    }

    #[test]
    fn gemini_rejects_invalid_delta_and_result_status_shapes() {
        let invalid_delta = parse_gemini_event(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": "partial",
            "delta": "true"
        }));
        let invalid_status = parse_gemini_event(serde_json::json!({
            "type": "result",
            "status": "maybe"
        }));

        assert!(matches!(
            invalid_delta,
            Err(ProviderError::MalformedOutput(_))
        ));
        assert!(matches!(
            invalid_status,
            Err(ProviderError::MalformedOutput(_))
        ));
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

    #[test]
    fn diagnostic_replaces_unsafe_permission_bypass_advice() {
        let diagnostic = normalize_provider_diagnostic(
            ProviderId::Agy,
            "Permission denied. Retry with --dangerously-skip-permissions to continue.",
        );

        assert_eq!(
            diagnostic,
            "provider requested an unsafe permission bypass; Colay did not enable it"
        );
        assert!(!diagnostic.contains("dangerously-skip-permissions"));
    }

    #[test]
    fn diagnostic_compacts_gemini_javascript_stack() {
        let diagnostic = normalize_provider_diagnostic(
            ProviderId::Gemini,
            "unsupported account\r\n\
             at first (client.js:1:1)\r\n\
             at second (client.js:2:1)\r\n\
             at third (client.js:3:1)\r\n\
             at fourth (client.js:4:1)\r\n\
             at fifth (client.js:5:1)\r\n\
             at sixth (client.js:6:1)\r\n\
             at sixth (client.js:6:1)\r\n",
        );

        assert!(diagnostic.starts_with("unsupported account\n"));
        assert_eq!(diagnostic.matches("at sixth").count(), 0);
        assert!(diagnostic.contains("[2 provider stack frames omitted]"));
        assert_eq!(diagnostic.lines().count(), 6);
    }

    #[test]
    fn diagnostic_preserves_failure_classification_terms() {
        for (provider, input, marker) in [
            (
                ProviderId::Claude,
                "authentication failed",
                "authentication",
            ),
            (
                ProviderId::Claude,
                "Credit balance is too low",
                "Credit balance",
            ),
            (
                ProviderId::Gemini,
                "unsupported account for this client",
                "unsupported account",
            ),
        ] {
            let diagnostic = normalize_provider_diagnostic(provider, input);
            assert!(diagnostic.contains(marker), "{provider}: {diagnostic}");
        }
    }
}
