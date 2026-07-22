use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use orchestrator_domain::{
    AttemptId, ModelProfile, ProviderCapabilities, ProviderId, SandboxMode, SchemaVersion, TaskId,
    WorkerEvent, WorkerRequest,
};
use orchestrator_engine::{
    CONVERSATION_MAX_OUTPUT_BYTES, ConversationExit, ConversationFailure, ConversationOrchestrator,
    ConversationRequest, ConversationResponse,
};
use orchestrator_providers::{AdapterRuntime, RuntimeTermination, WorkerAdapter};
use orchestrator_state::RootConfig;
use serde::Serialize;

use crate::task_planner::{OfficialCliTaskPlanner, build_provider_adapter, profile_settings};

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
        let provider = self.planner.primary_provider();
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
        let mut evidence = self.planner.capabilities[&provider].evidence.clone();
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
                        evidence.push(detail);
                    }
                }
                Ok(WorkerEvent::Error { message, .. }) => lifecycle_error = Some(message),
                Ok(WorkerEvent::Unknown {
                    event_type,
                    affects_lifecycle,
                    ..
                }) => {
                    evidence.push(format!("unknown event: {event_type}"));
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
            evidence.push(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        if output.truncated {
            evidence.push("provider runtime truncated output".to_owned());
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
            evidence.push(error);
        }
        Ok(ConversationResponse {
            schema_version: SchemaVersion::v1(),
            attempt_id: request.attempt_id,
            session_id: request.session_id,
            source_message_id: request.source_message_id,
            provider,
            sandbox: SandboxMode::ReadOnly,
            exit,
            output_redacted: messages.join("\n").into_bytes(),
            evidence_redacted: evidence.join("\n"),
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
