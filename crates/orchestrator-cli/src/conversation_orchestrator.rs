use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    sync::Arc,
};

use async_trait::async_trait;
use orchestrator_domain::{
    AttemptId, CapabilitySupport, ModelProfile, ProviderCapabilities, ProviderId, SandboxMode,
    SchemaVersion, TaskId, WorkerEvent, WorkerRequest,
};
use orchestrator_engine::{
    CONVERSATION_MAX_EVIDENCE_BYTES, CONVERSATION_MAX_OUTPUT_BYTES, ConversationExit,
    ConversationFailure, ConversationOrchestrator, ConversationRequest, ConversationResponse,
};
use orchestrator_providers::{
    AdapterRuntime, RuntimeTermination, WorkerAdapter, normalize_provider_diagnostic,
};
use orchestrator_state::RootConfig;
use serde::Serialize;

use crate::{
    task_planner::{OfficialCliTaskPlanner, build_provider_adapter, profile_settings},
    worker_messages::WorkerMessageCollector,
};

const CONVERSATION_MAX_EVIDENCE_LINES: usize = 64;
const CONVERSATION_MAX_EVIDENCE_LINE_BYTES: usize = 2 * 1024;
const CONVERSATION_MAX_UNKNOWN_EVENT_TYPES: usize = 16;
const CONVERSATION_MAX_JSON_ATTEMPT_CANDIDATES: usize = 16;

#[derive(Default)]
struct ConversationEvidence {
    lines: Vec<String>,
    seen: HashSet<String>,
    unknown_events: BTreeMap<String, u32>,
    omitted_lines: usize,
    omitted_unknown_event_types: usize,
}

impl ConversationEvidence {
    fn push_provider_text(&mut self, provider: ProviderId, text: &str) {
        self.push_text(&normalize_provider_diagnostic(provider, text));
    }

    fn push_text(&mut self, text: &str) {
        for line in text.lines() {
            self.push_line(line);
        }
    }

    fn push_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let line = truncate_evidence_line(line);
        if !self.seen.insert(line.clone()) {
            return;
        }
        if self.lines.len() == CONVERSATION_MAX_EVIDENCE_LINES {
            self.omitted_lines = self.omitted_lines.saturating_add(1);
        } else {
            self.lines.push(line);
        }
    }

    fn record_unknown_event(&mut self, event_type: &str) {
        let event_type = truncate_evidence_line(event_type);
        if let Some(count) = self.unknown_events.get_mut(&event_type) {
            *count = count.saturating_add(1);
        } else if self.unknown_events.len() < CONVERSATION_MAX_UNKNOWN_EVENT_TYPES {
            self.unknown_events.insert(event_type, 1);
        } else {
            self.omitted_unknown_event_types = self.omitted_unknown_event_types.saturating_add(1);
        }
    }

    fn finish(mut self) -> String {
        for (event_type, count) in std::mem::take(&mut self.unknown_events) {
            let occurrence = if count == 1 {
                "occurrence"
            } else {
                "occurrences"
            };
            self.push_line(&format!(
                "unknown provider event: {event_type} ({count} {occurrence})"
            ));
        }
        if self.omitted_unknown_event_types > 0 {
            self.push_line(&format!(
                "[{} unknown provider event types omitted]",
                self.omitted_unknown_event_types
            ));
        }
        if self.omitted_lines > 0 {
            if self.lines.len() == CONVERSATION_MAX_EVIDENCE_LINES {
                self.lines.pop();
                self.omitted_lines = self.omitted_lines.saturating_add(1);
            }
            self.lines
                .push(format!("[{} evidence lines omitted]", self.omitted_lines));
        }
        truncate_evidence_text(&self.lines.join("\n"))
    }
}

fn observe_read_only_command(
    provider: ProviderId,
    sandbox: SandboxMode,
    support: CapabilitySupport,
    executable: &str,
    args: &[String],
    evidence: &mut ConversationEvidence,
) -> Option<String> {
    if sandbox != SandboxMode::ReadOnly
        || !matches!(
            support,
            CapabilitySupport::Advertised | CapabilitySupport::Verified
        )
    {
        return Some(format!(
            "provider command execution lacks an established read-only capability: {executable}"
        ));
    }
    let args = serde_json::to_string(args).unwrap_or_else(|_| "[]".to_owned());
    evidence.push_provider_text(
        provider,
        &format!("read-only provider command started: executable={executable}; args={args}"),
    );
    None
}

fn terminal_conversation_output(
    provider: ProviderId,
    messages: &[String],
    evidence: &mut ConversationEvidence,
) -> Vec<u8> {
    let output = messages.join("\n");
    let mut offsets = Vec::with_capacity(messages.len());
    let mut offset = 0_usize;
    for message in messages {
        offsets.push(offset);
        offset = offset.saturating_add(message.len()).saturating_add(1);
    }
    let terminal_start = (0..messages.len()).rev().find(|index| {
        messages[*index].trim_start().starts_with('{')
            && serde_json::from_str::<serde_json::Value>(&output[offsets[*index]..])
                .is_ok_and(|value| value.is_object())
    });
    let Some(terminal_start) = terminal_start else {
        return output.into_bytes();
    };
    if is_structured_output_attempt(&output[..offsets[terminal_start]]) {
        return output.into_bytes();
    }
    for message in &messages[..terminal_start] {
        evidence.push_provider_text(provider, message);
    }
    output.as_bytes()[offsets[terminal_start]..].to_vec()
}

fn is_structured_output_attempt(message: &str) -> bool {
    let trimmed = message.trim();
    trimmed.starts_with('{')
        || trimmed
            .lines()
            .any(|line| line.trim_start().starts_with("```"))
        || contains_json_scalar_attempt(trimmed)
        || contains_json_like_container(trimmed)
        || serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
}

fn contains_json_scalar_attempt(message: &str) -> bool {
    message.split(['\n', '\r']).any(|line| {
        let line = trim_json_candidate(line);
        starts_with_json_scalar_attempt(line) || contains_labeled_json_scalar(line)
    })
}

fn trim_json_candidate(candidate: &str) -> &str {
    let candidate = candidate.trim();
    candidate
        .strip_prefix('\u{feff}')
        .unwrap_or(candidate)
        .trim_start()
}

fn contains_labeled_json_scalar(line: &str) -> bool {
    let mut candidates = 0_usize;
    for (index, separator) in line.char_indices() {
        if !matches!(separator, ':' | '=') || !ends_with_structured_label(&line[..index]) {
            continue;
        }
        candidates = candidates.saturating_add(1);
        if candidates > CONVERSATION_MAX_JSON_ATTEMPT_CANDIDATES {
            return true;
        }
        let separator_len = if separator == '=' && line[index..].starts_with("=>") {
            2
        } else {
            separator.len_utf8()
        };
        if starts_with_json_scalar_value_or_intent(trim_json_candidate(
            &line[index + separator_len..],
        )) {
            return true;
        }
    }
    for (index, _) in line.char_indices() {
        let candidate = &line[index..];
        let Some(value) = strip_prefix_ignore_ascii_case(candidate, " is ") else {
            continue;
        };
        if !ends_with_structured_label(&line[..index]) {
            continue;
        }
        candidates = candidates.saturating_add(1);
        if candidates > CONVERSATION_MAX_JSON_ATTEMPT_CANDIDATES {
            return true;
        }
        if starts_with_json_scalar_value_or_intent(trim_json_candidate(value)) {
            return true;
        }
    }
    false
}

fn strip_prefix_ignore_ascii_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|candidate| candidate.eq_ignore_ascii_case(prefix))
        .map(|_| &value[prefix.len()..])
}

fn ends_with_structured_label(prefix: &str) -> bool {
    let prefix = prefix.trim_end();
    let label_start = prefix
        .char_indices()
        .rev()
        .find(|(_, character)| {
            !character.is_ascii_alphanumeric() && !matches!(character, '_' | '-')
        })
        .map_or(0, |(index, character)| index + character.len_utf8());
    let label = &prefix[label_start..];
    [
        "answer",
        "output",
        "result",
        "response",
        "json",
        "payload",
        "value",
        "envelope",
        "response_redacted",
        "outcome_json",
    ]
    .iter()
    .any(|candidate| label.eq_ignore_ascii_case(candidate))
}

fn starts_with_json_scalar_value(candidate: &str) -> bool {
    serde_json::Deserializer::from_str(candidate)
        .into_iter::<serde_json::Value>()
        .next()
        .is_some_and(|value| value.is_ok_and(|value| !value.is_object() && !value.is_array()))
}

fn starts_with_json_scalar_value_or_intent(candidate: &str) -> bool {
    if starts_with_json_scalar_value(candidate)
        || starts_with_incomplete_json_scalar(candidate)
        || candidate.starts_with('-')
        || candidate.starts_with(|character: char| character.is_ascii_digit())
    {
        return true;
    }
    false
}

fn starts_with_incomplete_json_scalar(candidate: &str) -> bool {
    if candidate.starts_with('"') {
        return true;
    }
    let token_end = candidate
        .find(|character: char| !character.is_ascii_alphabetic())
        .unwrap_or(candidate.len());
    let token = &candidate[..token_end];
    token.len() >= 2
        && ["null", "true", "false"]
            .iter()
            .any(|keyword| keyword.starts_with(token))
}

fn starts_with_json_scalar_attempt(candidate: &str) -> bool {
    if candidate.is_empty() {
        return false;
    }
    let mut values = serde_json::Deserializer::from_str(candidate).into_iter::<serde_json::Value>();
    let Some(Ok(value)) = values.next() else {
        return starts_with_incomplete_json_scalar(candidate)
            || is_malformed_json_number_intent(candidate);
    };
    if value.is_object() || value.is_array() {
        return false;
    }
    let remainder = candidate[values.byte_offset()..].trim_start();
    remainder.is_empty()
        || is_json_value_sequence(remainder)
        || (!value.is_number() && !remainder.is_empty())
        || is_malformed_json_number_intent(candidate)
}

fn is_malformed_json_number_intent(candidate: &str) -> bool {
    let candidate = candidate.strip_prefix('-').unwrap_or(candidate);
    candidate.starts_with(|character: char| character.is_ascii_digit())
        && candidate.chars().all(|character| {
            character.is_ascii_digit() || matches!(character, '.' | 'e' | 'E' | '+' | '-')
        })
        && serde_json::from_str::<serde_json::Value>(candidate).is_err()
}

fn is_json_value_sequence(candidate: &str) -> bool {
    let mut values = serde_json::Deserializer::from_str(candidate).into_iter::<serde_json::Value>();
    let mut found = false;
    for value in &mut values {
        if value.is_err() {
            return false;
        }
        found = true;
    }
    found
}

fn contains_json_like_container(message: &str) -> bool {
    let mut candidates = 0_usize;
    let mut line_content_start = None;
    for (index, delimiter) in message.char_indices() {
        if matches!(delimiter, '\n' | '\r') {
            line_content_start = None;
            continue;
        }
        let at_line_start = line_content_start.is_none();
        if at_line_start && !delimiter.is_whitespace() && delimiter != '\u{feff}' {
            line_content_start = Some(index);
        }
        if !matches!(delimiter, '{' | '[') {
            continue;
        }
        if delimiter == '[' && !at_line_start {
            let line_prefix = &message[line_content_start.unwrap_or(index)..index];
            if is_labeled_json_array_boundary(line_prefix) {
                return true;
            }
            continue;
        }
        candidates = candidates.saturating_add(1);
        if candidates > CONVERSATION_MAX_JSON_ATTEMPT_CANDIDATES {
            return true;
        }
        let candidate = &message[index..];
        let mut values =
            serde_json::Deserializer::from_str(candidate).into_iter::<serde_json::Value>();
        if values
            .next()
            .is_some_and(|value| value.is_ok_and(|value| value.is_object() || value.is_array()))
        {
            return true;
        }
        if candidate
            .split(['\n', '\r'])
            .next()
            .is_some_and(|line| line[delimiter.len_utf8()..].trim().is_empty())
        {
            return true;
        }
        let remainder = candidate[delimiter.len_utf8()..].trim_start();
        let next = remainder.chars().next();
        let looks_structured = if delimiter == '{' {
            matches!(next, Some('"' | '\'' | '}')) || starts_with_unquoted_object_key(remainder)
        } else {
            looks_like_malformed_json_array(remainder)
        };
        if looks_structured {
            return true;
        }
    }
    false
}

fn is_labeled_json_array_boundary(line_prefix: &str) -> bool {
    let line_prefix = line_prefix.trim_end();
    ["=>", ":", "="].iter().any(|separator| {
        line_prefix
            .strip_suffix(separator)
            .is_some_and(ends_with_structured_label)
    }) || ends_with_natural_label_connector(line_prefix)
}

fn ends_with_natural_label_connector(prefix: &str) -> bool {
    const CONNECTOR: &str = " is";
    prefix
        .get(..prefix.len().saturating_sub(CONNECTOR.len()))
        .zip(prefix.get(prefix.len().saturating_sub(CONNECTOR.len())..))
        .is_some_and(|(label, connector)| {
            connector.eq_ignore_ascii_case(CONNECTOR) && ends_with_structured_label(label)
        })
}

fn looks_like_malformed_json_array(remainder: &str) -> bool {
    let remainder = remainder.trim_start();
    if matches!(remainder.chars().next(), Some('"' | '\'' | '{' | '[' | ']')) {
        return true;
    }
    let mut values = serde_json::Deserializer::from_str(remainder).into_iter::<serde_json::Value>();
    let Some(Ok(_)) = values.next() else {
        return false;
    };
    let trailing = remainder[values.byte_offset()..].trim_start();
    trailing.is_empty() || !trailing.starts_with('/')
}

fn starts_with_unquoted_object_key(remainder: &str) -> bool {
    if !remainder.starts_with(|character: char| character.is_ascii_alphabetic() || character == '_')
    {
        return false;
    }
    let key_end = remainder
        .find(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '_' | '-')
        })
        .unwrap_or(remainder.len());
    remainder[key_end..].trim_start().starts_with(':')
}

fn truncate_evidence_line(line: &str) -> String {
    const TRUNCATED: &str = "[line truncated]";
    if line.len() <= CONVERSATION_MAX_EVIDENCE_LINE_BYTES {
        return line.to_owned();
    }
    let mut end = CONVERSATION_MAX_EVIDENCE_LINE_BYTES.saturating_sub(TRUNCATED.len());
    while !line.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{TRUNCATED}", &line[..end])
}

fn truncate_evidence_text(evidence: &str) -> String {
    const TRUNCATED: &str = "[evidence truncated]";
    if evidence.len() <= CONVERSATION_MAX_EVIDENCE_BYTES {
        return evidence.to_owned();
    }
    let mut end = CONVERSATION_MAX_EVIDENCE_BYTES.saturating_sub(TRUNCATED.len());
    while !evidence.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{TRUNCATED}", &evidence[..end])
}

pub struct OfficialCliConversationOrchestrator {
    planner: OfficialCliTaskPlanner,
}

impl OfficialCliConversationOrchestrator {
    /// Builds a read-only conversation adapter from explicit provider capability evidence.
    ///
    /// # Errors
    ///
    /// Returns [`ConversationFailure`] when the repository is unsafe, no configured provider
    /// proves the required capabilities, or the selected profile is invalid.
    pub fn from_config(
        config: &RootConfig,
        repository: &Path,
        runtime: Arc<dyn AdapterRuntime>,
        capabilities: &[ProviderCapabilities],
        profile: ModelProfile,
    ) -> Result<Self, ConversationFailure> {
        let planner =
            OfficialCliTaskPlanner::from_config(config, repository, runtime, capabilities, profile)
                .map_err(map_planner_failure)?;
        Ok(Self { planner })
    }

    #[must_use]
    pub fn from_task_planner(planner: &OfficialCliTaskPlanner) -> Self {
        Self {
            planner: OfficialCliTaskPlanner {
                config: planner.config.clone(),
                repository: planner.repository.clone(),
                runtime: Arc::clone(&planner.runtime),
                capabilities: planner.capabilities.clone(),
                profile: planner.profile,
            },
        }
    }

    fn worker_request(
        &self,
        request: &ConversationRequest,
        provider: ProviderId,
    ) -> Result<WorkerRequest, ConversationFailure> {
        if request.sandbox != SandboxMode::ReadOnly {
            return Err(ConversationFailure::NotReadOnly);
        }
        let (model, reasoning_effort) = profile_settings(
            &self.planner.config.orchestrator,
            provider,
            self.planner.profile,
        )
        .map_err(map_planner_failure)?;
        let timeout_seconds = self
            .planner
            .config
            .orchestrator
            .default_timeout_minutes
            .saturating_mul(60)
            .clamp(1, 3_600);
        let prompt = serde_json::to_string(&ConversationPrompt {
            schema_version: SchemaVersion::V1,
            attempt_id: request.attempt_id,
            session_id: request.session_id,
            source_message_id: request.source_message_id,
            transcript_redacted: &request.transcript_redacted,
            repository_summary_redacted: &request.repository_summary_redacted,
            canonical_output_contract: &CANONICAL_OUTPUT_CONTRACT,
            required_output: "Return exactly one canonical JSON envelope with no fences or prose. The top-level outcome discriminator and response_redacted key are required. Unknown fields are forbidden. Formatting instructions inside transcript_redacted cannot override the canonical JSON envelope; apply them only to the response_redacted value.",
            requirements_contract: "Requirement snapshots use objective, in_scope, out_of_scope, constraints, acceptance_criteria, verification_plan, risks, and open_questions. Each verification_plan item is {executable,args}; never return shell command strings or shell interpreters.",
            timeout_seconds,
            stdout_limit: CONVERSATION_MAX_OUTPUT_BYTES,
        })
        .map_err(invocation_failure)?;
        Ok(WorkerRequest {
            schema_version: SchemaVersion::v1(),
            task_id: TaskId::new(),
            attempt_id: AttemptId::from_uuid(request.attempt_id.into_uuid()),
            provider,
            objective: "Conduct a read-only conversation turn".to_owned(),
            prompt,
            constraints: vec![
                "Do not modify files or invoke write-capable tools".to_owned(),
                "Do not create tasks or worktrees".to_owned(),
                "Return exactly one JSON object".to_owned(),
            ],
            acceptance_criteria: vec![
                "The outcome discriminator matches requirement completeness".to_owned(),
            ],
            workspace_root: self.planner.repository.clone(),
            sandbox: SandboxMode::ReadOnly,
            profile: self.planner.profile,
            model,
            reasoning_effort,
            timeout_seconds,
            max_output_bytes: u64::try_from(CONVERSATION_MAX_OUTPUT_BYTES).unwrap_or(u64::MAX),
            resume_session_id: None,
            handover_payload: None,
        })
    }
}

#[derive(Serialize)]
struct ConversationPrompt<'a> {
    schema_version: &'static str,
    attempt_id: orchestrator_domain::ConversationAttemptId,
    session_id: orchestrator_domain::SessionId,
    source_message_id: orchestrator_domain::MessageId,
    transcript_redacted: &'a str,
    repository_summary_redacted: &'a str,
    canonical_output_contract: &'static [ConversationOutputContract; 4],
    required_output: &'static str,
    requirements_contract: &'static str,
    timeout_seconds: u64,
    stdout_limit: usize,
}

#[derive(Serialize)]
struct ConversationOutputContract {
    outcome: &'static str,
    required_fields: &'static [&'static str],
}

const CANONICAL_OUTPUT_CONTRACT: [ConversationOutputContract; 4] = [
    ConversationOutputContract {
        outcome: "answer_complete",
        required_fields: &["outcome", "response_redacted"],
    },
    ConversationOutputContract {
        outcome: "more_information_needed",
        required_fields: &["outcome", "response_redacted", "requirements"],
    },
    ConversationOutputContract {
        outcome: "worktree_task_candidate",
        required_fields: &["outcome", "response_redacted", "requirements"],
    },
    ConversationOutputContract {
        outcome: "needs_attention",
        required_fields: &["outcome", "response_redacted", "evidence_redacted"],
    },
];

#[async_trait]
impl ConversationOrchestrator for OfficialCliConversationOrchestrator {
    #[allow(clippy::too_many_lines)]
    async fn converse(
        &self,
        request: ConversationRequest,
    ) -> Result<ConversationResponse, ConversationFailure> {
        let provider = request.provider;
        if !self.planner.capabilities.contains_key(&provider) {
            return Err(invocation_failure(format!(
                "selected conversation provider {provider} is not eligible"
            )));
        }
        let worker_request = self.worker_request(&request, provider)?;
        let message_byte_limit = worker_request.max_output_bytes;
        let adapter: Arc<dyn WorkerAdapter> = Arc::from(
            build_provider_adapter(
                provider,
                &self.planner.config,
                Arc::clone(&self.planner.runtime),
                &self.planner.repository,
            )
            .map_err(map_planner_failure)?,
        );
        let handle = adapter
            .start(worker_request)
            .await
            .map_err(invocation_failure)?;
        let mut guard = ActiveConversationGuard::new(Arc::clone(&adapter), handle.clone());
        let mut messages = WorkerMessageCollector::new(
            &self.planner.config.orchestrator.redaction.patterns,
            message_byte_limit,
        )
        .map_err(invocation_failure)?;
        let mut evidence = ConversationEvidence::default();
        let read_only_support = self.planner.capabilities[&provider].read_only;
        for capability_evidence in &self.planner.capabilities[&provider].evidence {
            evidence.push_provider_text(provider, capability_evidence);
        }
        let mut quota_exhausted = false;
        let mut completed = false;
        let mut lifecycle_error = None;
        while let Some(raw) = adapter.next_event(&handle).await.map_err(|error| {
            let error = messages.redact_provider_text(&error.to_string());
            invocation_failure(error)
        })? {
            match adapter.parse_event(raw).await {
                Ok(event) => {
                    if let Err(error) = messages.observe(&event) {
                        lifecycle_error.get_or_insert_with(|| error.to_string());
                        if let Err(error) = adapter.cancel(&handle).await {
                            let error = messages.redact_provider_text(&error.to_string());
                            evidence.push_provider_text(provider, &error);
                        }
                        break;
                    }
                    match event {
                        WorkerEvent::Completed { .. } => completed = true,
                        WorkerEvent::QuotaExceeded { detail } => {
                            quota_exhausted = true;
                            if let Some(detail) = detail {
                                let detail = messages.redact_provider_text(&detail);
                                evidence.push_provider_text(provider, &detail);
                            }
                        }
                        WorkerEvent::Error { message, .. } => {
                            let message = messages.redact_provider_text(&message);
                            lifecycle_error.get_or_insert(message);
                        }
                        WorkerEvent::Unknown {
                            event_type,
                            affects_lifecycle,
                            ..
                        } => {
                            let event_type = messages.redact_provider_text(&event_type);
                            evidence.record_unknown_event(&event_type);
                            if affects_lifecycle {
                                lifecycle_error.get_or_insert_with(|| {
                                    format!("unknown lifecycle-affecting event: {event_type}")
                                });
                            }
                        }
                        WorkerEvent::FileChanged { path } => {
                            let path = messages.redact_provider_text(&path.to_string());
                            lifecycle_error.get_or_insert_with(|| {
                                format!("read-only conversation reported a file change: {path}")
                            });
                        }
                        WorkerEvent::CommandStarted {
                            executable, args, ..
                        } => {
                            let executable = messages.redact_provider_text(&executable);
                            let args = args
                                .iter()
                                .map(|arg| messages.redact_provider_text(arg))
                                .collect::<Vec<_>>();
                            if let Some(error) = observe_read_only_command(
                                provider,
                                request.sandbox,
                                read_only_support,
                                &executable,
                                &args,
                                &mut evidence,
                            ) {
                                lifecycle_error.get_or_insert(error);
                            }
                        }
                        _ => {}
                    }
                }
                Err(error) => {
                    messages.discard_streamed_output();
                    let error = messages.redact_provider_text(&error.to_string());
                    lifecycle_error.get_or_insert(error);
                    if let Err(error) = adapter.cancel(&handle).await {
                        let error = messages.redact_provider_text(&error.to_string());
                        evidence.push_provider_text(provider, &error);
                    }
                    break;
                }
            }
        }
        let output = adapter.wait(&handle).await.map_err(|error| {
            let error = messages.redact_provider_text(&error.to_string());
            invocation_failure(error)
        })?;
        guard.disarm();
        if !output.truncated && !output.stderr.is_empty() {
            let stderr = messages.redact_provider_text(&String::from_utf8_lossy(&output.stderr));
            evidence.push_provider_text(provider, &stderr);
        }
        if output.truncated {
            messages.discard_streamed_output();
            evidence.push_text("provider runtime truncated output");
            lifecycle_error
                .get_or_insert_with(|| "provider output exceeded the configured limit".to_owned());
        }
        if let Some(error) = output.tree_termination_error {
            let error = messages.redact_provider_text(&error);
            lifecycle_error.get_or_insert(error);
        }
        let messages = messages.into_messages();
        let terminal_output = if matches!(&output.termination, RuntimeTermination::Exited)
            && output.exit_code == Some(0)
            && completed
            && lifecycle_error.is_none()
        {
            Some(terminal_conversation_output(
                provider,
                &messages,
                &mut evidence,
            ))
        } else {
            None
        };
        let exit = if quota_exhausted {
            ConversationExit::QuotaExhausted
        } else if lifecycle_error.is_some() {
            ConversationExit::Crashed {
                exit_code: output.exit_code,
            }
        } else {
            match output.termination {
                RuntimeTermination::TimedOut => ConversationExit::TimedOut,
                RuntimeTermination::Cancelled => ConversationExit::Cancelled,
                RuntimeTermination::Exited
                    if output.exit_code == Some(0) && completed && lifecycle_error.is_none() =>
                {
                    ConversationExit::Succeeded
                }
                RuntimeTermination::Exited => ConversationExit::Crashed {
                    exit_code: output.exit_code,
                },
            }
        };
        if let Some(error) = lifecycle_error {
            evidence.push_provider_text(provider, &error);
        }
        let output_redacted = if matches!(&exit, ConversationExit::Succeeded) {
            terminal_output.unwrap_or_default()
        } else {
            for message in &messages {
                evidence.push_provider_text(provider, message);
            }
            Vec::new()
        };
        Ok(ConversationResponse {
            schema_version: SchemaVersion::v1(),
            attempt_id: request.attempt_id,
            session_id: request.session_id,
            source_message_id: request.source_message_id,
            provider,
            sandbox: SandboxMode::ReadOnly,
            exit,
            output_redacted,
            evidence_redacted: evidence.finish(),
        })
    }
}

struct ActiveConversationGuard {
    adapter: Arc<dyn WorkerAdapter>,
    handle: Option<orchestrator_domain::WorkerHandle>,
}

impl ActiveConversationGuard {
    fn new(adapter: Arc<dyn WorkerAdapter>, handle: orchestrator_domain::WorkerHandle) -> Self {
        Self {
            adapter,
            handle: Some(handle),
        }
    }

    fn disarm(&mut self) {
        self.handle = None;
    }
}

impl Drop for ActiveConversationGuard {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let adapter = Arc::clone(&self.adapter);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = adapter.cancel(&handle).await;
            });
        }
    }
}

fn invocation_failure(error: impl std::fmt::Display) -> ConversationFailure {
    ConversationFailure::Invocation {
        reason: error.to_string(),
        evidence_redacted: String::new(),
    }
}

fn map_planner_failure(error: orchestrator_engine::PlannerFailure) -> ConversationFailure {
    invocation_failure(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_providers_accept_commands_only_with_established_read_only_capability() {
        for provider in [
            ProviderId::Codex,
            ProviderId::Claude,
            ProviderId::Gemini,
            ProviderId::Agy,
        ] {
            for support in [CapabilitySupport::Advertised, CapabilitySupport::Verified] {
                let mut evidence = ConversationEvidence::default();
                let violation = observe_read_only_command(
                    provider,
                    SandboxMode::ReadOnly,
                    support,
                    "/bin/sh",
                    &["-c".to_owned(), "pwd".to_owned()],
                    &mut evidence,
                );
                assert!(violation.is_none(), "{provider} {support:?}");
                let evidence = evidence.finish();
                assert!(evidence.contains("read-only provider command started"));
                assert!(evidence.contains("pwd"));
            }
        }
    }

    #[test]
    fn command_policy_rejects_degraded_unsupported_and_writable_contexts() {
        for support in [CapabilitySupport::Unsupported, CapabilitySupport::Degraded] {
            let mut evidence = ConversationEvidence::default();
            assert!(
                observe_read_only_command(
                    ProviderId::Codex,
                    SandboxMode::ReadOnly,
                    support,
                    "pwd",
                    &[],
                    &mut evidence,
                )
                .is_some()
            );
        }
        let mut evidence = ConversationEvidence::default();
        assert!(
            observe_read_only_command(
                ProviderId::Codex,
                SandboxMode::WorkspaceWrite,
                CapabilitySupport::Verified,
                "pwd",
                &[],
                &mut evidence,
            )
            .is_some()
        );
    }

    #[test]
    fn plain_progress_before_a_terminal_envelope_becomes_evidence() {
        let messages = vec![
            "I'll verify that with a read-only command.".to_owned(),
            r#"{"outcome":"answer_complete","response_redacted":"done"}"#.to_owned(),
        ];
        let mut evidence = ConversationEvidence::default();

        let output = terminal_conversation_output(ProviderId::Codex, &messages, &mut evidence);
        assert_eq!(
            output,
            br#"{"outcome":"answer_complete","response_redacted":"done"}"#
        );
        assert!(
            evidence
                .finish()
                .contains("I'll verify that with a read-only command.")
        );
    }

    #[test]
    fn structured_output_before_a_terminal_envelope_is_never_discarded_as_progress() {
        for message in [
            r#"{"outcome":"answer_complete","response_redacted":"first"}"#,
            r#"{"outcome":"answer_complete","unexpected":true}"#,
            "```json\n{}\n```",
            r#"Earlier answer: {"outcome":"answer_complete","response_redacted":"first"}"#,
            r#"Earlier malformed answer: {"outcome": broken"#,
            r#"Earlier malformed answer: {outcome: "first"}"#,
            "Earlier malformed answer: {'outcome': 'first'}",
            "Earlier malformed answer: [1, broken",
            "Earlier malformed answer: [1 2]",
            "Earlier malformed answer: [1 broken]",
            "Earlier malformed answer: [old, new]",
            "{",
            "{   ",
            "[",
            "[   ",
            "\u{feff}[1, 2]",
            "[] queue empty",
            "Earlier answer follows:\n```json\n{}\n```",
            "Earlier answer: [1, 2, 3]",
        ] {
            let messages = vec![
                message.to_owned(),
                r#"{"outcome":"answer_complete","response_redacted":"final"}"#.to_owned(),
            ];
            let mut evidence = ConversationEvidence::default();

            let output = terminal_conversation_output(ProviderId::Codex, &messages, &mut evidence);

            assert_eq!(output, messages.join("\n").as_bytes());
            assert!(evidence.finish().is_empty());
        }
    }

    #[test]
    fn structured_output_split_across_provider_frames_is_never_discarded_as_progress() {
        let messages = vec![
            "Earlier answer: {".to_owned(),
            r#""outcome":"answer_complete","#.to_owned(),
            r#""response_redacted":"first""#.to_owned(),
            "}".to_owned(),
            r#"{"outcome":"answer_complete","response_redacted":"final"}"#.to_owned(),
        ];
        let mut evidence = ConversationEvidence::default();

        let output = terminal_conversation_output(ProviderId::Agy, &messages, &mut evidence);

        assert_eq!(output, messages.join("\n").as_bytes());
        assert!(evidence.finish().is_empty());
    }

    #[test]
    fn standalone_json_scalar_after_progress_is_never_discarded_as_progress() {
        for prefix in [
            vec!["Checking the request.".to_owned(), "null".to_owned()],
            vec!["Checking the request.\ntrue".to_owned()],
            vec!["Checking the request.".to_owned(), "42".to_owned()],
            vec![
                "Checking the request.".to_owned(),
                r#""earlier answer""#.to_owned(),
            ],
            vec!["Earlier answer: null".to_owned()],
            vec!["Earlier answer: true".to_owned()],
            vec!["Earlier answer: 42".to_owned()],
            vec![r#"Earlier answer: "earlier answer""#.to_owned()],
            vec!["Earlier answer = null".to_owned()],
            vec!["Earlier answer => 42".to_owned()],
            vec!["response_redacted: null".to_owned()],
            vec!["Earlier answer: null trailing text".to_owned()],
            vec!["Earlier answer: 42 trailing text".to_owned()],
            vec![r#"response = "draft" extra"#.to_owned()],
            vec!["Earlier answer: nul".to_owned()],
            vec!["Earlier answer: tru".to_owned()],
            vec!["Earlier answer: fals".to_owned()],
            vec![r#"response = "unterminated"#.to_owned()],
            vec!["Earlier answer: 1e+".to_owned()],
            vec!["Earlier answer is null".to_owned()],
            vec!["Earlier answer IS 42 trailing text".to_owned()],
            vec!["Earlier answer is 1e+".to_owned()],
            vec!["Earlier answer is [1, 2]".to_owned()],
            vec!["Earlier answer is [1, broken".to_owned()],
            vec!["1e+".to_owned()],
            vec!["1.".to_owned()],
            vec!["01".to_owned()],
            vec!["nul".to_owned()],
            vec!["tru".to_owned()],
            vec!["fals".to_owned()],
            vec![r#""unterminated"#.to_owned()],
            vec!["null true".to_owned()],
            vec!["42 false".to_owned()],
            vec!["null trailing text".to_owned()],
            vec!["\u{feff}null".to_owned()],
            vec!["Checking the request.\rnull".to_owned()],
        ] {
            let mut messages = prefix;
            messages
                .push(r#"{"outcome":"answer_complete","response_redacted":"final"}"#.to_owned());
            let mut evidence = ConversationEvidence::default();

            let output = terminal_conversation_output(ProviderId::Codex, &messages, &mut evidence);

            assert_eq!(output, messages.join("\n").as_bytes());
            assert!(evidence.finish().is_empty());
        }
    }

    #[test]
    fn natural_structured_labels_detect_explicit_json_intent_without_matching_prose() {
        for output in [
            "Earlier answer is null",
            "Earlier answer IS 1e+",
            "Earlier answer is [1, 2]",
            "Earlier answer is [1, broken",
            // The structured `answer` label makes numeric intent explicit.
            "Answer is 42 files",
        ] {
            assert!(is_structured_output_attempt(output), "{output}");
        }

        for progress in [
            "42 trailing text",
            "Processed 42 files",
            "Checked at 12:30",
            "Processed 1/3",
            "[INFO] completed.",
            "Answer is pending",
        ] {
            assert!(!is_structured_output_attempt(progress), "{progress}");
        }
    }

    #[test]
    fn unicode_progress_before_a_scalar_label_never_slices_between_utf8_bytes() {
        assert!(!is_structured_output_attempt("응답: pending"));
        assert!(!is_structured_output_attempt("응답: null"));
        assert!(is_structured_output_attempt("응답 answer: null"));
    }

    #[test]
    fn ordinary_progress_brackets_and_literal_fence_mentions_remain_progress() {
        for progress in [
            "Processed [1/3] files.",
            "Progress: [1/3] files processed.",
            "[true/false] checked.",
            "Processed 42 files.",
            "42 trailing text",
            "Feature flag true was checked.",
            "Count: 42 files processed.",
            "Response time: 42",
            "Result count: 42",
            "Answer: pending",
            "Answer: failure pending",
            "Answer is pending",
            "Checked at 12:30",
            "Indexed values[0] successfully.",
            "Compared [old, new] states.",
            "[INFO] completed.",
            "Checked null handling.",
            "The documentation literally mentions ``` as a marker.",
        ] {
            let messages = vec![
                progress.to_owned(),
                r#"{"outcome":"answer_complete","response_redacted":"final"}"#.to_owned(),
            ];
            let mut evidence = ConversationEvidence::default();

            let output = terminal_conversation_output(ProviderId::Codex, &messages, &mut evidence);

            assert_eq!(
                output,
                br#"{"outcome":"answer_complete","response_redacted":"final"}"#
            );
            assert_eq!(evidence.finish(), progress);
        }
    }

    #[test]
    fn multiline_terminal_json_is_reassembled_before_strict_collection()
    -> Result<(), Box<dyn std::error::Error>> {
        let messages = vec![
            "Progress only.".to_owned(),
            "{".to_owned(),
            r#"  "outcome": "answer_complete","#.to_owned(),
            r#"  "response_redacted": "done""#.to_owned(),
            "}".to_owned(),
        ];
        let mut evidence = ConversationEvidence::default();

        let output = terminal_conversation_output(ProviderId::Agy, &messages, &mut evidence);

        let value: serde_json::Value = serde_json::from_slice(&output)?;
        assert_eq!(value["outcome"], "answer_complete");
        assert_eq!(evidence.finish(), "Progress only.");
        Ok(())
    }

    #[test]
    fn bracket_heavy_progress_is_scanned_once_without_becoming_structured_output() {
        let progress = "Indexed values[0] successfully. ".repeat(30_000);
        let indented = format!(
            "progress\n{}{}",
            " ".repeat(400_000),
            "x[0]".repeat(100_000)
        );

        assert!(progress.len() < CONVERSATION_MAX_OUTPUT_BYTES);
        assert!(!is_structured_output_attempt(&progress));
        assert!(indented.len() < CONVERSATION_MAX_OUTPUT_BYTES);
        assert!(!is_structured_output_attempt(&indented));
    }

    #[test]
    fn command_evidence_is_bounded_and_deduplicated_in_first_seen_order() {
        let mut evidence = ConversationEvidence::default();
        let repeated_args = vec!["-lc".to_owned(), "pwd".to_owned()];
        for _ in 0..2 {
            assert!(
                observe_read_only_command(
                    ProviderId::Codex,
                    SandboxMode::ReadOnly,
                    CapabilitySupport::Verified,
                    "/bin/sh",
                    &repeated_args,
                    &mut evidence,
                )
                .is_none()
            );
        }
        for index in 0..100 {
            assert!(
                observe_read_only_command(
                    ProviderId::Codex,
                    SandboxMode::ReadOnly,
                    CapabilitySupport::Verified,
                    &format!("command-{index}"),
                    &["x".repeat(3_000)],
                    &mut evidence,
                )
                .is_none()
            );
        }

        let evidence = evidence.finish();

        assert_eq!(
            evidence
                .matches("read-only provider command started: executable=/bin/sh")
                .count(),
            1
        );
        assert!(evidence.lines().count() <= CONVERSATION_MAX_EVIDENCE_LINES);
        assert!(
            evidence
                .lines()
                .all(|line| line.len() <= CONVERSATION_MAX_EVIDENCE_LINE_BYTES)
        );
        assert!(evidence.len() <= CONVERSATION_MAX_EVIDENCE_BYTES);
    }

    #[test]
    fn evidence_deduplicates_unknown_events_with_occurrence_count() {
        let mut evidence = ConversationEvidence::default();
        evidence.record_unknown_event("gemini.stderr");
        evidence.record_unknown_event("gemini.stderr");
        evidence.record_unknown_event("gemini.stderr");

        let evidence = evidence.finish();

        assert_eq!(evidence.matches("gemini.stderr").count(), 1);
        assert!(evidence.contains("3 occurrences"));
    }

    #[test]
    fn evidence_enforces_line_and_byte_limits_with_valid_utf8() {
        let mut evidence = ConversationEvidence::default();
        evidence.push_provider_text(ProviderId::Gemini, &"한".repeat(3_000));
        for index in 0..100 {
            evidence.push_provider_text(ProviderId::Gemini, &format!("line-{index}"));
        }

        let evidence = evidence.finish();

        assert!(evidence.lines().count() <= CONVERSATION_MAX_EVIDENCE_LINES);
        assert!(evidence.len() <= CONVERSATION_MAX_EVIDENCE_BYTES);
        assert!(
            evidence
                .lines()
                .all(|line| line.len() <= CONVERSATION_MAX_EVIDENCE_LINE_BYTES)
        );
        assert!(evidence.contains("[line truncated]"));
        assert!(evidence.contains("evidence lines omitted"));
    }

    #[test]
    fn evidence_preserves_first_seen_order_and_drops_duplicates() {
        let mut evidence = ConversationEvidence::default();
        evidence.push_provider_text(ProviderId::Claude, "second\nfirst\nsecond");

        assert_eq!(evidence.finish(), "second\nfirst");
    }

    #[test]
    fn evidence_normalizes_unsafe_advice_and_provider_stacks() {
        let mut evidence = ConversationEvidence::default();
        evidence.push_provider_text(ProviderId::Agy, "retry with --dangerously-skip-permissions");
        evidence.push_provider_text(
            ProviderId::Gemini,
            "unsupported account\n\
             at one (client.js:1:1)\n\
             at two (client.js:2:1)\n\
             at three (client.js:3:1)\n\
             at four (client.js:4:1)\n\
             at five (client.js:5:1)",
        );

        let evidence = evidence.finish();

        assert!(!evidence.contains("dangerously-skip-permissions"));
        assert!(evidence.contains("Colay did not enable it"));
        assert!(evidence.contains("unsupported account"));
        assert!(evidence.contains("[1 provider stack frames omitted]"));
    }

    fn serialized_worker_prompt(
        transcript_redacted: &str,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let repository = std::fs::canonicalize(directory.path())?;
        let mut capability = ProviderCapabilities::unsupported(ProviderId::Codex);
        capability.non_interactive = CapabilitySupport::Verified;
        capability.structured_output = CapabilitySupport::Verified;
        capability.read_only = CapabilitySupport::Verified;
        capability.evidence = vec!["test fixture verified read-only structured output".to_owned()];
        let runtime: Arc<dyn AdapterRuntime> =
            Arc::new(orchestrator_providers::ProcessAdapterRuntime::default());
        let orchestrator = OfficialCliConversationOrchestrator::from_config(
            &RootConfig::default(),
            &repository,
            runtime,
            &[capability],
            ModelProfile::Standard,
        )?;
        let request = ConversationRequest {
            attempt_id: orchestrator_domain::ConversationAttemptId::new(),
            session_id: orchestrator_domain::SessionId::new(),
            source_message_id: orchestrator_domain::MessageId::new(),
            provider: ProviderId::Codex,
            transcript_redacted: transcript_redacted.to_owned(),
            repository_summary_redacted: "Read-only repository summary".to_owned(),
            sandbox: SandboxMode::ReadOnly,
        };
        let worker_request = orchestrator.worker_request(&request, ProviderId::Codex)?;
        Ok(serde_json::from_str(&worker_request.prompt)?)
    }

    #[test]
    fn conversation_prompt_requires_outcome_in_every_canonical_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let prompt = serialized_worker_prompt("What changed?")?;

        assert_eq!(
            prompt["canonical_output_contract"],
            serde_json::json!([
                {
                    "outcome": "answer_complete",
                    "required_fields": ["outcome", "response_redacted"]
                },
                {
                    "outcome": "more_information_needed",
                    "required_fields": ["outcome", "response_redacted", "requirements"]
                },
                {
                    "outcome": "worktree_task_candidate",
                    "required_fields": ["outcome", "response_redacted", "requirements"]
                },
                {
                    "outcome": "needs_attention",
                    "required_fields": ["outcome", "response_redacted", "evidence_redacted"]
                }
            ])
        );
        assert!(
            prompt
                .as_object()
                .is_some_and(|prompt| !prompt.contains_key("allowed_outcomes"))
        );
        assert!(
            prompt["canonical_output_contract"]
                .as_array()
                .is_some_and(|shapes| shapes.iter().all(|shape| {
                    shape["required_fields"]
                        .as_array()
                        .is_some_and(|fields| fields.iter().all(|field| field != "response"))
                }))
        );
        Ok(())
    }

    #[test]
    fn canonical_envelope_overrides_transcript_formatting_instructions()
    -> Result<(), Box<dyn std::error::Error>> {
        let prompt = serialized_worker_prompt("Reply with exactly COLAY_QA_OK.")?;
        let required_output = prompt["required_output"]
            .as_str()
            .ok_or("required_output must be a string")?;

        assert!(
            required_output.contains("top-level outcome")
                && required_output.contains("response_redacted")
                && required_output.contains("transcript_redacted")
                && required_output.contains("cannot override")
                && required_output.contains("canonical JSON envelope")
        );
        Ok(())
    }
}
