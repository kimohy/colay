use orchestrator_domain::WorkerEvent;
use orchestrator_process::{RedactionConfig, RedactionError, Redactor};

const MAX_PROVIDER_EVENTS: usize = 4_096;

pub(crate) fn redact_decoded_provider_text(redactor: &Redactor, text: &str) -> String {
    if text.contains("[REDACTED") {
        "[REDACTED]".to_owned()
    } else {
        redactor.redact(text)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkerOutputError {
    EventLimitExceeded { limit: usize },
    MessageBytesExceeded { limit: u64 },
}

impl std::fmt::Display for WorkerOutputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EventLimitExceeded { limit } => {
                write!(formatter, "provider event limit exceeded ({limit})")
            }
            Self::MessageBytesExceeded { limit } => {
                write!(formatter, "provider message byte limit exceeded ({limit})")
            }
        }
    }
}

impl std::error::Error for WorkerOutputError {}

/// Collects semantic provider messages while preserving streaming chunk
/// boundaries. Only semantic message-boundary events end a streamed message.
pub(crate) struct WorkerMessageCollector {
    messages: Vec<(String, bool)>,
    pending_delta: String,
    redactor: Redactor,
    message_byte_limit: u64,
    message_bytes: u64,
    events_seen: usize,
}

impl WorkerMessageCollector {
    // This source module is compiled into both the library and binary targets;
    // the binary already owns a configured Redactor and uses `with_redactor`.
    #[allow(dead_code)]
    pub(crate) fn new(
        patterns: &[String],
        message_byte_limit: u64,
    ) -> Result<Self, RedactionError> {
        let redactor = Redactor::new(&RedactionConfig {
            literals: Vec::new(),
            patterns: patterns.to_vec(),
        })?;
        Ok(Self::with_redactor(redactor, message_byte_limit))
    }

    pub(crate) fn with_redactor(redactor: Redactor, message_byte_limit: u64) -> Self {
        Self {
            messages: Vec::new(),
            pending_delta: String::new(),
            redactor,
            message_byte_limit,
            message_bytes: 0,
            events_seen: 0,
        }
    }

    pub(crate) fn observe(&mut self, event: &WorkerEvent) -> Result<(), WorkerOutputError> {
        self.events_seen = self.events_seen.saturating_add(1);
        if self.events_seen > MAX_PROVIDER_EVENTS {
            self.discard_pending_delta();
            return Err(WorkerOutputError::EventLimitExceeded {
                limit: MAX_PROVIDER_EVENTS,
            });
        }
        match event {
            WorkerEvent::Message { text } => self.push_message(text)?,
            WorkerEvent::MessageDelta { text } => {
                if let Err(error) = self.reserve_message_bytes(text.len()) {
                    self.discard_pending_delta();
                    return Err(error);
                }
                self.pending_delta.push_str(text);
            }
            WorkerEvent::Started { .. }
            | WorkerEvent::Usage { .. }
            | WorkerEvent::Unknown {
                affects_lifecycle: false,
                ..
            } => {}
            WorkerEvent::Error { .. }
            | WorkerEvent::QuotaExceeded { .. }
            | WorkerEvent::Unknown {
                affects_lifecycle: true,
                ..
            } => self.discard_pending_delta(),
            _ => self.flush_delta(),
        }
        Ok(())
    }

    pub(crate) fn push_message(&mut self, text: &str) -> Result<(), WorkerOutputError> {
        if let Err(error) = self.reserve_message_bytes(text.len()) {
            self.discard_pending_delta();
            return Err(error);
        }
        self.flush_delta();
        self.messages.push((self.redact_provider_text(text), false));
        Ok(())
    }

    pub(crate) fn redact_provider_text(&self, text: &str) -> String {
        redact_decoded_provider_text(&self.redactor, text)
    }

    pub(crate) fn into_messages(self) -> Vec<String> {
        self.messages
            .into_iter()
            .map(|(message, _)| message)
            .collect()
    }

    pub(crate) fn discard_pending_delta(&mut self) {
        self.pending_delta.clear();
    }

    pub(crate) fn discard_streamed_output(&mut self) {
        self.discard_pending_delta();
        self.messages.retain(|(_, streamed)| !streamed);
    }

    fn flush_delta(&mut self) {
        if !self.pending_delta.is_empty() {
            let message = std::mem::take(&mut self.pending_delta);
            self.messages
                .push((self.redact_provider_text(&message), true));
        }
    }

    fn reserve_message_bytes(&mut self, bytes: usize) -> Result<(), WorkerOutputError> {
        let bytes = u64::try_from(bytes).map_err(|_| WorkerOutputError::MessageBytesExceeded {
            limit: self.message_byte_limit,
        })?;
        let next = self.message_bytes.checked_add(bytes).ok_or(
            WorkerOutputError::MessageBytesExceeded {
                limit: self.message_byte_limit,
            },
        )?;
        if next > self.message_byte_limit {
            return Err(WorkerOutputError::MessageBytesExceeded {
                limit: self.message_byte_limit,
            });
        }
        self.message_bytes = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_deltas_are_concatenated_without_inventing_newlines()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut messages = WorkerMessageCollector::new(&[], 1_024)?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: "{\"out".to_owned(),
        })?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: "come\":\"answer_complete\"}".to_owned(),
        })?;
        messages.observe(&WorkerEvent::Completed {
            summary: None,
            usage: None,
        })?;

        assert_eq!(
            messages.into_messages(),
            vec![r#"{"outcome":"answer_complete"}"#]
        );
        Ok(())
    }

    #[test]
    fn non_lifecycle_warning_does_not_split_a_delta_sequence()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut messages = WorkerMessageCollector::new(&[], 1_024)?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: r#"{"out"#.to_owned(),
        })?;
        messages.observe(&WorkerEvent::Unknown {
            event_type: "gemini.warning".to_owned(),
            payload: serde_json::json!({
                "type": "error",
                "severity": "warning",
                "message": "Loop detected, stopping execution",
                "timestamp": "2026-08-07T00:00:01Z"
            }),
            affects_lifecycle: false,
        })?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: r#"come":"answer_complete"}"#.to_owned(),
        })?;
        messages.observe(&WorkerEvent::Completed {
            summary: None,
            usage: None,
        })?;

        assert_eq!(
            messages.into_messages(),
            vec![r#"{"outcome":"answer_complete"}"#]
        );
        Ok(())
    }

    #[test]
    fn a_structural_event_separates_delta_sequences() -> Result<(), Box<dyn std::error::Error>> {
        let mut messages = WorkerMessageCollector::new(&[], 1_024)?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: "before tool".to_owned(),
        })?;
        messages.observe(&WorkerEvent::CommandStarted {
            command_id: "command-1".to_owned(),
            executable: "pwd".to_owned(),
            args: Vec::new(),
        })?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: "after tool".to_owned(),
        })?;
        messages.observe(&WorkerEvent::Completed {
            summary: None,
            usage: None,
        })?;

        assert_eq!(messages.into_messages(), vec!["before tool", "after tool"]);
        Ok(())
    }

    #[test]
    fn credentials_split_across_deltas_are_redacted_after_reassembly()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut messages = WorkerMessageCollector::new(&[], 1_024)?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: "api_".to_owned(),
        })?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: "key=topsecretvalue".to_owned(),
        })?;
        messages.observe(&WorkerEvent::Completed {
            summary: None,
            usage: None,
        })?;

        assert_eq!(messages.into_messages(), vec!["api_key=[REDACTED]"]);
        Ok(())
    }

    #[test]
    fn an_earlier_frame_redaction_discards_the_entire_delta_sequence()
    -> Result<(), Box<dyn std::error::Error>> {
        let boundary_redactor = Redactor::new(&RedactionConfig::default())?;
        for (prefix, suffix) in [("Bearer abcdefgh", "ijklmnop"), ("sk-abcdefgh", "ijklmnop")] {
            let mut messages = WorkerMessageCollector::new(&[], 1_024)?;
            messages.observe(&WorkerEvent::MessageDelta {
                text: boundary_redactor.redact(prefix),
            })?;
            messages.observe(&WorkerEvent::MessageDelta {
                text: suffix.to_owned(),
            })?;
            messages.observe(&WorkerEvent::Completed {
                summary: None,
                usage: None,
            })?;
            assert_eq!(messages.into_messages(), vec!["[REDACTED]"]);
        }

        let patterns = vec!["CUSTOM-[A-Z]+".to_owned()];
        let boundary_redactor = Redactor::new(&RedactionConfig {
            literals: Vec::new(),
            patterns: patterns.clone(),
        })?;
        let mut messages = WorkerMessageCollector::new(&patterns, 1_024)?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: boundary_redactor.redact("CUSTOM-ABC"),
        })?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: "DEF".to_owned(),
        })?;
        messages.observe(&WorkerEvent::Completed {
            summary: None,
            usage: None,
        })?;
        assert_eq!(messages.into_messages(), vec!["[REDACTED]"]);
        Ok(())
    }

    #[test]
    fn decoded_provider_text_is_redacted_after_json_parsing()
    -> Result<(), Box<dyn std::error::Error>> {
        let messages = WorkerMessageCollector::new(&[], 1_024)?;

        assert_eq!(
            messages.redact_provider_text(r#"api_key="supersecretvalue""#),
            "api_key=[REDACTED]"
        );
        Ok(())
    }

    #[test]
    fn aggregate_message_and_event_limits_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
        let mut messages = WorkerMessageCollector::new(&[], 5)?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: "123".to_owned(),
        })?;
        assert_eq!(
            messages.observe(&WorkerEvent::MessageDelta {
                text: "456".to_owned(),
            }),
            Err(WorkerOutputError::MessageBytesExceeded { limit: 5 })
        );

        let mut messages = WorkerMessageCollector::new(&[], u64::MAX)?;
        let event = WorkerEvent::Unknown {
            event_type: "fixture".to_owned(),
            payload: serde_json::Value::Null,
            affects_lifecycle: false,
        };
        for _ in 0..MAX_PROVIDER_EVENTS {
            messages.observe(&event)?;
        }
        assert_eq!(
            messages.observe(&event),
            Err(WorkerOutputError::EventLimitExceeded {
                limit: MAX_PROVIDER_EVENTS
            })
        );
        Ok(())
    }

    #[test]
    fn an_incomplete_secret_prefix_is_discarded_when_the_message_limit_fails()
    -> Result<(), Box<dyn std::error::Error>> {
        let redactor = Redactor::new(&RedactionConfig {
            literals: vec!["SUPERSECRET123456".to_owned()],
            patterns: Vec::new(),
        })?;
        let mut messages = WorkerMessageCollector::with_redactor(redactor, 17);
        messages.observe(&WorkerEvent::MessageDelta {
            text: "SUPERSECRET12345".to_owned(),
        })?;

        assert_eq!(
            messages.observe(&WorkerEvent::MessageDelta {
                text: "6X".to_owned(),
            }),
            Err(WorkerOutputError::MessageBytesExceeded { limit: 17 })
        );
        assert!(messages.into_messages().is_empty());
        Ok(())
    }

    #[test]
    fn runtime_protocol_loss_discards_already_flushed_streamed_output()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut messages = WorkerMessageCollector::new(&[], 1_024)?;
        messages.observe(&WorkerEvent::MessageDelta {
            text: "[REDACTED STREAM FRAME]secret-suffix".to_owned(),
        })?;
        messages.observe(&WorkerEvent::Completed {
            summary: None,
            usage: None,
        })?;

        messages.discard_streamed_output();

        assert!(messages.into_messages().is_empty());
        Ok(())
    }
}
