use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    sync::Arc,
};

use async_trait::async_trait;
use orchestrator_domain::{
    AttemptId, ModelProfile, ProviderCapabilities, ProviderId, SandboxMode, SchemaVersion, TaskId,
    WorkerEvent, WorkerRequest,
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

use crate::task_planner::{OfficialCliTaskPlanner, build_provider_adapter, profile_settings};

const CONVERSATION_MAX_EVIDENCE_LINES: usize = 64;
const CONVERSATION_MAX_EVIDENCE_LINE_BYTES: usize = 2 * 1024;
const CONVERSATION_MAX_UNKNOWN_EVENT_TYPES: usize = 16;

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
            allowed_outcomes: [
                "answer_complete",
                "more_information_needed",
                "worktree_task_candidate",
                "needs_attention",
            ],
            required_output: "Return exactly one ConversationOutcome JSON object and no fences or prose",
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
    allowed_outcomes: [&'static str; 4],
    required_output: &'static str,
    requirements_contract: &'static str,
    timeout_seconds: u64,
    stdout_limit: usize,
}

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
        let mut messages = Vec::new();
        let mut evidence = ConversationEvidence::default();
        for capability_evidence in &self.planner.capabilities[&provider].evidence {
            evidence.push_provider_text(provider, capability_evidence);
        }
        let mut quota_exhausted = false;
        let mut completed = false;
        let mut lifecycle_error = None;
        while let Some(raw) = adapter
            .next_event(&handle)
            .await
            .map_err(invocation_failure)?
        {
            match adapter.parse_event(raw).await {
                Ok(WorkerEvent::Message { text }) => messages.push(text),
                Ok(WorkerEvent::Completed { .. }) => completed = true,
                Ok(WorkerEvent::QuotaExceeded { detail }) => {
                    quota_exhausted = true;
                    if let Some(detail) = detail {
                        evidence.push_provider_text(provider, &detail);
                    }
                }
                Ok(WorkerEvent::Error { message, .. }) => lifecycle_error = Some(message),
                Ok(WorkerEvent::Unknown {
                    event_type,
                    affects_lifecycle,
                    ..
                }) => {
                    evidence.record_unknown_event(&event_type);
                    if affects_lifecycle {
                        lifecycle_error =
                            Some(format!("unknown lifecycle-affecting event: {event_type}"));
                    }
                }
                Ok(WorkerEvent::FileChanged { path }) => {
                    lifecycle_error = Some(format!(
                        "read-only conversation reported a file change: {path}"
                    ));
                }
                Ok(WorkerEvent::CommandStarted { executable, .. }) => {
                    lifecycle_error = Some(format!(
                        "read-only conversation reported command execution: {executable}"
                    ));
                }
                Ok(_) => {}
                Err(error) => lifecycle_error = Some(error.to_string()),
            }
        }
        let output = adapter.wait(&handle).await.map_err(invocation_failure)?;
        guard.disarm();
        if !output.stderr.is_empty() {
            evidence.push_provider_text(provider, &String::from_utf8_lossy(&output.stderr));
        }
        if output.truncated {
            evidence.push_text("provider runtime truncated output");
        }
        if let Some(error) = output.tree_termination_error {
            lifecycle_error = Some(error);
        }
        let exit = if quota_exhausted {
            ConversationExit::QuotaExhausted
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
            messages.join("\n").into_bytes()
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
}
