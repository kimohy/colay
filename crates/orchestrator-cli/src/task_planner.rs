use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use orchestrator_domain::{
    AttemptId, GraphRevisionId, ModelProfile, ProviderCapabilities, ProviderId, QuotaPeriod,
    QuotaScope, ReasoningEffort, SandboxMode, SchemaVersion, TaskId, UsageUnit, WorkerEvent,
    WorkerRequest,
};
use orchestrator_engine::{
    PLANNER_MAX_OUTPUT_BYTES, PlannerExit, PlannerFailure, PlannerRequest, PlannerResponse,
    TaskPlanner,
};
use orchestrator_providers::{
    AdapterRuntime, AgyAdapter, AgyAdapterConfig, ClaudeAdapter, ClaudeAdapterConfig, CodexAdapter,
    CodexAdapterConfig, CodexTransportFeatures, GeminiAdapter, GeminiAdapterConfig,
    RuntimeTermination, UsageProbeConfig, WorkerAdapter,
};
use orchestrator_state::{OrchestratorConfig, ProviderConfig, RootConfig};
use serde::Serialize;

pub struct OfficialCliTaskPlanner {
    config: RootConfig,
    repository: PathBuf,
    runtime: Arc<dyn AdapterRuntime>,
    capabilities: BTreeMap<ProviderId, ProviderCapabilities>,
    profile: ModelProfile,
}

impl OfficialCliTaskPlanner {
    /// Builds a planner only from configured providers with explicit safe capability evidence.
    ///
    /// # Errors
    ///
    /// Returns [`PlannerFailure`] when the repository is unsafe or no evidenced provider is
    /// configured for read-only structured planning.
    pub fn from_config(
        config: &RootConfig,
        repository: &Path,
        runtime: Arc<dyn AdapterRuntime>,
        capabilities: &[ProviderCapabilities],
        profile: ModelProfile,
    ) -> Result<Self, PlannerFailure> {
        if !repository.is_absolute() || !repository.is_dir() {
            return Err(invocation_failure(
                "planner repository must be an existing absolute directory",
            ));
        }
        let capabilities: BTreeMap<_, _> = capabilities
            .iter()
            .filter(|capability| capability_is_safe(capability))
            .filter(|capability| {
                configured_provider(&config.orchestrator.providers, capability.provider)
                    .is_some_and(|provider| provider.enabled)
            })
            .map(|capability| (capability.provider, capability.clone()))
            .collect();
        if capabilities.is_empty() {
            return Err(invocation_failure(
                "no configured provider has evidenced read-only structured CLI capabilities",
            ));
        }
        Ok(Self {
            config: config.clone(),
            repository: repository.to_path_buf(),
            runtime,
            capabilities,
            profile,
        })
    }

    /// Probes only configured official CLIs and constructs a planner from the observed evidence.
    ///
    /// # Errors
    ///
    /// Returns [`PlannerFailure`] when no configured provider proves the required safe
    /// non-interactive, structured-output, and read-only capabilities.
    pub async fn probe_from_config(
        config: &RootConfig,
        repository: &Path,
        runtime: Arc<dyn AdapterRuntime>,
        profile: ModelProfile,
    ) -> Result<Self, PlannerFailure> {
        let mut capabilities = Vec::new();
        for provider in [
            ProviderId::Gemini,
            ProviderId::Agy,
            ProviderId::Codex,
            ProviderId::Claude,
        ] {
            let Some(provider_config) =
                configured_provider(&config.orchestrator.providers, provider)
            else {
                continue;
            };
            if !provider_config.enabled {
                continue;
            }
            let adapter =
                build_provider_adapter(provider, config, Arc::clone(&runtime), repository)?;
            if let Ok(observed) = adapter.capabilities().await
                && capability_is_safe(&observed)
            {
                capabilities.push(observed);
            }
        }
        Self::from_config(config, repository, runtime, &capabilities, profile)
    }

    #[must_use]
    pub fn primary_provider(&self) -> ProviderId {
        let mut candidates = self
            .capabilities
            .keys()
            .copied()
            .filter_map(|provider| {
                configured_provider(&self.config.orchestrator.providers, provider)
                    .map(|config| (config.priority, provider))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| right.cmp(left));
        candidates
            .first()
            .map_or(ProviderId::Codex, |(_, provider)| *provider)
    }

    fn select_provider(&self, request: &PlannerRequest) -> Result<ProviderId, PlannerFailure> {
        if request.sandbox != SandboxMode::ReadOnly {
            return Err(PlannerFailure::NotReadOnly);
        }
        if !request
            .validation_policy
            .eligible_profiles
            .contains(&self.profile)
        {
            return Err(invocation_failure(
                "configured planner profile is not eligible for this graph",
            ));
        }
        let mut candidates = self
            .capabilities
            .keys()
            .copied()
            .filter(|provider| {
                request
                    .validation_policy
                    .eligible_providers
                    .contains(provider)
            })
            .filter_map(|provider| {
                configured_provider(&self.config.orchestrator.providers, provider)
                    .map(|config| (config.priority, provider))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| right.cmp(left));
        candidates
            .first()
            .map(|(_, provider)| *provider)
            .ok_or_else(|| invocation_failure("no evidenced provider is eligible for this plan"))
    }

    fn worker_request(
        &self,
        request: &PlannerRequest,
        provider: ProviderId,
    ) -> Result<WorkerRequest, PlannerFailure> {
        let (model, reasoning_effort) =
            profile_settings(&self.config.orchestrator, provider, self.profile)
                .map_err(|error| invocation_failure(&error.to_string()))?;
        let timeout_seconds = self
            .config
            .orchestrator
            .default_timeout_minutes
            .saturating_mul(60)
            .clamp(1, 3_600);
        let prompt = serde_json::to_string(&PlanningPrompt {
            schema_version: SchemaVersion::V1,
            revision_id: request.revision_id,
            session_id: request.session_id,
            goal_message_id: request.goal_message_id,
            planner_provider: provider,
            goal_redacted: &request.goal_redacted,
            repository_summary_redacted: &request.repository_summary_redacted,
            required_output: "Return exactly one TaskGraphProposal JSON object and no fences or prose",
            timeout_seconds,
            stdout_limit: PLANNER_MAX_OUTPUT_BYTES,
        })
        .map_err(|error| invocation_failure(&error.to_string()))?;
        Ok(WorkerRequest {
            schema_version: SchemaVersion::v1(),
            task_id: TaskId::new(),
            attempt_id: AttemptId::new(),
            provider,
            objective: "Propose a read-only task graph".to_owned(),
            prompt,
            constraints: vec![
                "Do not modify files or invoke write-capable tools".to_owned(),
                "Return exactly one JSON object".to_owned(),
            ],
            acceptance_criteria: vec![
                "The proposal identity matches the supplied session, goal, revision, and provider"
                    .to_owned(),
            ],
            workspace_root: self.repository.clone(),
            sandbox: SandboxMode::ReadOnly,
            profile: self.profile,
            model,
            reasoning_effort,
            timeout_seconds,
            max_output_bytes: u64::try_from(PLANNER_MAX_OUTPUT_BYTES).unwrap_or(u64::MAX),
            resume_session_id: None,
            handover_payload: None,
        })
    }
}

#[derive(Serialize)]
struct PlanningPrompt<'a> {
    schema_version: &'static str,
    revision_id: GraphRevisionId,
    session_id: orchestrator_domain::SessionId,
    goal_message_id: orchestrator_domain::MessageId,
    planner_provider: ProviderId,
    goal_redacted: &'a str,
    repository_summary_redacted: &'a str,
    required_output: &'static str,
    timeout_seconds: u64,
    stdout_limit: usize,
}

#[async_trait]
impl TaskPlanner for OfficialCliTaskPlanner {
    #[allow(clippy::too_many_lines)]
    async fn propose(&self, request: PlannerRequest) -> Result<PlannerResponse, PlannerFailure> {
        let provider = self.select_provider(&request)?;
        let worker_request = self.worker_request(&request, provider)?;
        let adapter: Arc<dyn WorkerAdapter> = Arc::from(
            build_provider_adapter(
                provider,
                &self.config,
                Arc::clone(&self.runtime),
                &self.repository,
            )
            .map_err(|error| invocation_failure(&error.to_string()))?,
        );
        let handle = adapter
            .start(worker_request)
            .await
            .map_err(|error| invocation_failure(&error.to_string()))?;
        let mut guard = ActivePlannerGuard::new(Arc::clone(&adapter), handle.clone());
        let mut messages = Vec::new();
        let mut evidence = self.capabilities[&provider].evidence.clone();
        let mut quota_exhausted = false;
        let mut completed = false;
        let mut lifecycle_error = None;
        while let Some(raw) = adapter
            .next_event(&handle)
            .await
            .map_err(|error| invocation_failure(&error.to_string()))?
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
                    lifecycle_error =
                        Some(format!("read-only planner reported a file change: {path}"));
                }
                Ok(WorkerEvent::CommandStarted { executable, .. }) => {
                    lifecycle_error = Some(format!(
                        "read-only planner reported command execution: {executable}"
                    ));
                }
                Ok(_) => {}
                Err(error) => lifecycle_error = Some(error.to_string()),
            }
        }
        let output = adapter
            .wait(&handle)
            .await
            .map_err(|error| invocation_failure(&error.to_string()))?;
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
            PlannerExit::QuotaExhausted
        } else {
            match output.termination {
                RuntimeTermination::TimedOut => PlannerExit::TimedOut,
                RuntimeTermination::Cancelled => PlannerExit::Cancelled,
                RuntimeTermination::Exited
                    if output.exit_code == Some(0) && completed && lifecycle_error.is_none() =>
                {
                    PlannerExit::Succeeded
                }
                RuntimeTermination::Exited => PlannerExit::Crashed {
                    exit_code: output.exit_code,
                },
            }
        };
        if let Some(error) = lifecycle_error {
            evidence.push(error);
        }
        Ok(PlannerResponse {
            schema_version: SchemaVersion::v1(),
            session_id: request.session_id,
            goal_message_id: request.goal_message_id,
            provider,
            sandbox: SandboxMode::ReadOnly,
            exit,
            output_redacted: messages.join("\n").into_bytes(),
            evidence_redacted: evidence.join("\n"),
        })
    }
}

struct ActivePlannerGuard {
    adapter: Arc<dyn WorkerAdapter>,
    handle: Option<orchestrator_domain::WorkerHandle>,
}

impl ActivePlannerGuard {
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

impl Drop for ActivePlannerGuard {
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

fn capability_is_safe(capability: &ProviderCapabilities) -> bool {
    !capability.evidence.is_empty()
        && capability.non_interactive.usable()
        && capability.structured_output.usable()
        && capability.read_only.usable()
}

fn configured_provider(
    providers: &orchestrator_state::ProviderConfigs,
    provider: ProviderId,
) -> Option<&ProviderConfig> {
    match provider {
        ProviderId::Gemini => providers.gemini.as_ref(),
        ProviderId::Agy => providers.agy.as_ref(),
        ProviderId::Codex => providers.codex.as_ref(),
        ProviderId::Claude => providers.claude.as_ref(),
    }
}

fn invocation_failure(reason: &str) -> PlannerFailure {
    PlannerFailure::Invocation {
        reason: reason.to_owned(),
        evidence_redacted: String::new(),
    }
}

pub(crate) fn build_provider_adapter(
    provider: ProviderId,
    config: &RootConfig,
    runtime: Arc<dyn AdapterRuntime>,
    _repository: &Path,
) -> Result<Box<dyn WorkerAdapter>, PlannerFailure> {
    let provider_config = configured_provider(&config.orchestrator.providers, provider)
        .ok_or_else(|| invocation_failure("selected planner provider is not configured"))?;
    let scope = QuotaScope::new(
        provider_config
            .quota_scope
            .clone()
            .unwrap_or_else(|| format!("{}_planner", provider.as_str())),
        parse_quota_period(&provider_config.quota_period)?,
        UsageUnit::Custom(provider_config.quota_unit.clone()),
    );
    let usage_probe = UsageProbeConfig::ManualOrLedger;
    match provider {
        ProviderId::Codex => Ok(Box::new(
            CodexAdapter::new(
                CodexAdapterConfig {
                    executable: PathBuf::from(&provider_config.executable),
                    usage_probe,
                    usage_scope: scope,
                    allow_untested_read_only: true,
                },
                runtime,
            )
            .with_transport_features(CodexTransportFeatures {
                app_server_adapter: config.features.codex_app_server_adapter,
                exec_fallback: config.features.codex_exec_fallback,
            }),
        )),
        ProviderId::Claude => Ok(Box::new(ClaudeAdapter::new(
            ClaudeAdapterConfig {
                executable: PathBuf::from(&provider_config.executable),
                usage_probe,
                usage_scope: scope,
                effort_flag_enabled: provider_config.effort_flag_enabled,
            },
            runtime,
        ))),
        ProviderId::Gemini => Ok(Box::new(GeminiAdapter::new(
            GeminiAdapterConfig {
                executable: PathBuf::from(&provider_config.executable),
                usage_probe,
                usage_scope: scope,
            },
            runtime,
        ))),
        ProviderId::Agy => Ok(Box::new(AgyAdapter::new(
            AgyAdapterConfig {
                executable: PathBuf::from(&provider_config.executable),
                usage_probe,
                usage_scope: scope,
            },
            runtime,
        ))),
    }
}

pub(crate) fn profile_settings(
    config: &OrchestratorConfig,
    provider: ProviderId,
    profile: ModelProfile,
) -> Result<(Option<String>, Option<ReasoningEffort>), PlannerFailure> {
    let name = match profile {
        ModelProfile::Economy => "economy",
        ModelProfile::Standard => "standard",
        ModelProfile::Premium => "premium",
    };
    let configured = config
        .model_profiles
        .get(provider.as_str())
        .and_then(|profiles| profiles.get(name))
        .ok_or_else(|| invocation_failure("selected planner model profile is not configured"))?;
    let effort = configured
        .effort
        .as_deref()
        .map(|value| match value {
            "low" => Ok(ReasoningEffort::Low),
            "medium" => Ok(ReasoningEffort::Medium),
            "high" => Ok(ReasoningEffort::High),
            _ => Err(invocation_failure("planner reasoning effort is invalid")),
        })
        .transpose()?;
    Ok((
        (!configured.model.trim().is_empty()).then(|| configured.model.clone()),
        effort,
    ))
}

fn parse_quota_period(value: &str) -> Result<QuotaPeriod, PlannerFailure> {
    match value {
        "calendar_day" => Ok(QuotaPeriod::CalendarDay),
        "rolling_day" => Ok(QuotaPeriod::RollingDay),
        "calendar_month" => Ok(QuotaPeriod::CalendarMonth),
        "rolling_month" => Ok(QuotaPeriod::RollingMonth),
        "custom" => Ok(QuotaPeriod::Custom),
        _ => Err(invocation_failure(
            "planner provider quota period is invalid",
        )),
    }
}
