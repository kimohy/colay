use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexItemPhase {
    Started,
    Updated,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodexItem {
    AgentMessage {
        id: Option<String>,
        text: String,
    },
    Reasoning {
        id: Option<String>,
        text: Option<String>,
    },
    CommandExecution {
        id: Option<String>,
        command: Option<String>,
        status: Option<String>,
        exit_code: Option<i32>,
    },
    FileChange {
        id: Option<String>,
        path: Option<String>,
        status: Option<String>,
    },
    McpToolCall {
        id: Option<String>,
        server: Option<String>,
        tool: Option<String>,
        status: Option<String>,
    },
    WebSearch {
        id: Option<String>,
        query: Option<String>,
    },
    Plan {
        id: Option<String>,
        text: Option<String>,
    },
    Unknown {
        item_type: String,
        raw: Value,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
}

impl CodexUsage {
    #[must_use]
    pub fn total_observed_tokens(&self) -> Option<u64> {
        // Cached input is a subset of input tokens and reasoning output is a
        // subset of output tokens in the public usage shape. Use either only
        // as a fallback when its inclusive parent is absent, so the local
        // ledger never double-counts a structured result.
        let input = self.input_tokens.or(self.cached_input_tokens);
        let output = self.output_tokens.or(self.reasoning_output_tokens);
        match (input, output) {
            (Some(input), Some(output)) => input.checked_add(output),
            (Some(tokens), None) | (None, Some(tokens)) => Some(tokens),
            (None, None) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompatEvent {
    ThreadStarted {
        thread_id: String,
    },
    TurnStarted,
    TurnCompleted {
        usage: CodexUsage,
    },
    TurnFailed {
        message: String,
        quota: Option<QuotaErrorKind>,
    },
    Error {
        code: Option<String>,
        message: String,
        quota: Option<QuotaErrorKind>,
    },
    Item {
        phase: CodexItemPhase,
        item: CodexItem,
    },
    Opaque {
        event_type: String,
        raw: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaErrorKind {
    DailyQuota,
    MonthlyQuota,
    RateLimit,
    InsufficientQuota,
    QuotaExceeded,
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum CompatibilityError {
    #[error("no supported public Codex transport is available")]
    NoSupportedTransport,
    #[error("Codex option {option} is not supported by runtime capability evidence")]
    UnsupportedOption { option: &'static str },
    #[error("malformed JSONL at line {line}: {message}")]
    MalformedJson {
        line: usize,
        message: String,
        raw: String,
    },
    #[error("event at line {line} has no string type")]
    MissingEventType { line: usize, raw: Value },
    #[error("unknown lifecycle event {event_type} at line {line}")]
    UnknownLifecycle {
        line: usize,
        event_type: String,
        raw: Value,
    },
    #[error("required field {field} is invalid for {event_type} at line {line}")]
    InvalidRequiredField {
        line: usize,
        event_type: String,
        field: &'static str,
        raw: Value,
    },
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CodexEventParser;

impl CodexEventParser {
    #[must_use]
    pub fn parse_stream(&self, stream: &str) -> Vec<Result<CompatEvent, CompatibilityError>> {
        stream
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| self.parse_line(index + 1, line))
            .collect()
    }

    /// Parses one public `codex exec --json` line.
    ///
    /// # Errors
    ///
    /// Fails for malformed JSON, missing required fields, or an unknown event
    /// that can alter thread/turn lifecycle semantics.
    pub fn parse_line(
        &self,
        line_number: usize,
        line: &str,
    ) -> Result<CompatEvent, CompatibilityError> {
        let raw = serde_json::from_str::<Value>(line).map_err(|error| {
            CompatibilityError::MalformedJson {
                line: line_number,
                message: error.to_string(),
                raw: line.to_owned(),
            }
        })?;
        self.parse_value(line_number, raw)
    }

    /// Normalizes a decoded public Codex event.
    ///
    /// # Errors
    ///
    /// Fails for missing required fields or unknown lifecycle events. Unknown
    /// optional item kinds are preserved instead.
    pub fn parse_value(&self, line: usize, raw: Value) -> Result<CompatEvent, CompatibilityError> {
        let event_type = raw
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| CompatibilityError::MissingEventType {
                line,
                raw: raw.clone(),
            })?;

        match event_type.as_str() {
            "thread.started" => required_string(&raw, "thread_id").map_or_else(
                || {
                    Err(CompatibilityError::InvalidRequiredField {
                        line,
                        event_type: event_type.clone(),
                        field: "thread_id",
                        raw,
                    })
                },
                |thread_id| Ok(CompatEvent::ThreadStarted { thread_id }),
            ),
            "turn.started" => Ok(CompatEvent::TurnStarted),
            "turn.completed" => Ok(CompatEvent::TurnCompleted {
                usage: parse_usage(raw.get("usage")),
            }),
            "turn.failed" => {
                let message = extract_message(&raw).unwrap_or_else(|| "turn failed".to_owned());
                Ok(CompatEvent::TurnFailed {
                    quota: classify_quota_error(&message, raw.get("error")),
                    message,
                })
            }
            "error" => {
                let message = extract_message(&raw).unwrap_or_else(|| "Codex error".to_owned());
                let code = raw
                    .get("code")
                    .or_else(|| raw.pointer("/error/code"))
                    .and_then(value_to_string);
                Ok(CompatEvent::Error {
                    quota: classify_quota_error(&message, raw.get("error")),
                    code,
                    message,
                })
            }
            "item.started" => parse_item(line, &raw, CodexItemPhase::Started),
            "item.updated" => parse_item(line, &raw, CodexItemPhase::Updated),
            "item.completed" => parse_item(line, &raw, CodexItemPhase::Completed),
            // Any new thread/turn lifecycle can change state semantics and must
            // fail closed until a fixture establishes how to handle it.
            unknown if unknown.starts_with("thread.") || unknown.starts_with("turn.") => {
                Err(CompatibilityError::UnknownLifecycle {
                    line,
                    event_type: unknown.to_owned(),
                    raw,
                })
            }
            unknown => Ok(CompatEvent::Opaque {
                event_type: unknown.to_owned(),
                raw,
            }),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn parse_item(
    line: usize,
    raw: &Value,
    phase: CodexItemPhase,
) -> Result<CompatEvent, CompatibilityError> {
    let item =
        raw.get("item")
            .cloned()
            .ok_or_else(|| CompatibilityError::InvalidRequiredField {
                line,
                event_type: match phase {
                    CodexItemPhase::Started => "item.started",
                    CodexItemPhase::Updated => "item.updated",
                    CodexItemPhase::Completed => "item.completed",
                }
                .to_owned(),
                field: "item",
                raw: raw.clone(),
            })?;
    let item_type = item.get("type").and_then(Value::as_str).ok_or_else(|| {
        CompatibilityError::InvalidRequiredField {
            line,
            event_type: "item.*".to_owned(),
            field: "item.type",
            raw: raw.clone(),
        }
    })?;

    let id = item.get("id").and_then(value_to_string);
    let parsed = match item_type {
        "agent_message" => CodexItem::AgentMessage {
            id,
            text: item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        "reasoning" => CodexItem::Reasoning {
            id,
            text: item.get("text").and_then(Value::as_str).map(str::to_owned),
        },
        "command_execution" => CodexItem::CommandExecution {
            id,
            command: item
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned),
            status: item
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned),
            exit_code: item
                .get("exit_code")
                .or_else(|| item.get("exitCode"))
                .and_then(Value::as_i64)
                .and_then(|value| i32::try_from(value).ok()),
        },
        "file_change" => CodexItem::FileChange {
            id,
            path: item.get("path").and_then(Value::as_str).map(str::to_owned),
            status: item
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "mcp_tool_call" => CodexItem::McpToolCall {
            id,
            server: item
                .get("server")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tool: item
                .get("tool")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            status: item
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "web_search" => CodexItem::WebSearch {
            id,
            query: item.get("query").and_then(Value::as_str).map(str::to_owned),
        },
        "plan" | "plan_update" => CodexItem::Plan {
            id,
            text: item
                .get("text")
                .or_else(|| item.get("plan"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        unknown => CodexItem::Unknown {
            item_type: unknown.to_owned(),
            raw: item,
        },
    };
    Ok(CompatEvent::Item {
        phase,
        item: parsed,
    })
}

fn required_string(raw: &Value, field: &str) -> Option<String> {
    raw.get(field).and_then(Value::as_str).map(str::to_owned)
}

fn value_to_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|number| number.to_string()))
}

fn extract_message(raw: &Value) -> Option<String> {
    raw.get("message")
        .or_else(|| raw.pointer("/error/message"))
        .or_else(|| raw.get("error"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn parse_usage(value: Option<&Value>) -> CodexUsage {
    let value = value.unwrap_or(&Value::Null);
    CodexUsage {
        input_tokens: value.get("input_tokens").and_then(Value::as_u64),
        cached_input_tokens: value.get("cached_input_tokens").and_then(Value::as_u64),
        output_tokens: value.get("output_tokens").and_then(Value::as_u64),
        reasoning_output_tokens: value.get("reasoning_output_tokens").and_then(Value::as_u64),
    }
}

#[must_use]
pub fn classify_quota_error(message: &str, structured: Option<&Value>) -> Option<QuotaErrorKind> {
    let mut text = message.to_ascii_lowercase();
    if let Some(structured) = structured {
        text.push(' ');
        text.push_str(&structured.to_string().to_ascii_lowercase());
    }
    if text.contains("daily") && (text.contains("quota") || text.contains("limit")) {
        Some(QuotaErrorKind::DailyQuota)
    } else if text.contains("monthly") && (text.contains("quota") || text.contains("limit")) {
        Some(QuotaErrorKind::MonthlyQuota)
    } else if text.contains("insufficient_quota") || text.contains("insufficient quota") {
        Some(QuotaErrorKind::InsufficientQuota)
    } else if text.contains("quota exceeded")
        || text.contains("quota_exceeded")
        || text.contains("usage limit")
    {
        Some(QuotaErrorKind::QuotaExceeded)
    } else if text.contains("rate limit")
        || text.contains("rate_limit")
        || text.contains("too many requests")
    {
        Some(QuotaErrorKind::RateLimit)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unknown_item() {
        let event = CodexEventParser
            .parse_line(
                1,
                r#"{"type":"item.completed","item":{"id":"x","type":"new_optional_item","answer":42}}"#,
            )
            .err();
        assert!(event.is_none());

        let parsed = CodexEventParser.parse_line(
            1,
            r#"{"type":"item.completed","item":{"id":"x","type":"new_optional_item","answer":42}}"#,
        );
        assert!(matches!(
            parsed,
            Ok(CompatEvent::Item {
                item: CodexItem::Unknown { .. },
                ..
            })
        ));
    }

    #[test]
    fn fails_closed_for_unknown_lifecycle() {
        let parsed = CodexEventParser.parse_line(1, r#"{"type":"turn.paused"}"#);
        assert!(matches!(
            parsed,
            Err(CompatibilityError::UnknownLifecycle { .. })
        ));
    }

    #[test]
    fn malformed_line_reports_position() {
        let parsed = CodexEventParser.parse_line(7, "{not-json}");
        assert!(matches!(
            parsed,
            Err(CompatibilityError::MalformedJson { line: 7, .. })
        ));
    }

    #[test]
    fn identifies_quota_without_treating_generic_errors_as_quota() {
        assert_eq!(
            classify_quota_error("Monthly usage limit reached", None),
            Some(QuotaErrorKind::MonthlyQuota)
        );
        assert_eq!(classify_quota_error("repository is invalid", None), None);
    }

    #[test]
    fn token_total_does_not_double_count_usage_subcategories() {
        let usage = CodexUsage {
            input_tokens: Some(100),
            cached_input_tokens: Some(20),
            output_tokens: Some(25),
            reasoning_output_tokens: Some(5),
        };

        assert_eq!(usage.total_observed_tokens(), Some(125));
    }
}
