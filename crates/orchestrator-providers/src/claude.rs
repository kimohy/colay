use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;
use orchestrator_domain::{
    CancelResult, CapabilitySupport, HealthStatus, ProviderCapabilities, ProviderHealth,
    ProviderId, QuotaScope, RawEvent, RawEventChannel, ReasoningEffort, SandboxMode,
    UntrustedWorkerClaim, UsageSnapshot, WorkerEvent, WorkerHandle, WorkerRequest,
};

use crate::adapter::{SharedRuntime, ensure_provider, output_limits, prompt_payload};
use crate::{
    PreparedInvocation, ProviderError, StructuredOutput, UsageProbeConfig, WorkerAdapter,
    parse_claude_event, parse_usage_probe_output,
};

#[derive(Debug, Clone)]
pub struct ClaudeAdapterConfig {
    pub executable: PathBuf,
    pub usage_probe: UsageProbeConfig,
    pub usage_scope: QuotaScope,
    /// Enables `--effort` only when an administrator has validated it for the
    /// installed Enterprise CLI contract.
    pub effort_flag_enabled: bool,
}

pub struct ClaudeAdapter {
    config: ClaudeAdapterConfig,
    runtime: SharedRuntime,
}

impl ClaudeAdapter {
    #[must_use]
    pub const fn new(config: ClaudeAdapterConfig, runtime: SharedRuntime) -> Self {
        Self { config, runtime }
    }

    async fn probe_output(&self, args: &[&str]) -> Result<crate::RuntimeOutput, ProviderError> {
        let args = args.iter().map(OsString::from).collect::<Vec<_>>();
        self.runtime.run_probe(&self.config.executable, &args).await
    }

    async fn detected_capabilities(&self) -> Result<ProviderCapabilities, ProviderError> {
        let version = self.probe_output(&["--version"]).await?;
        let help = self.probe_output(&["--help"]).await?;
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&help.stdout),
            String::from_utf8_lossy(&help.stderr)
        );
        let mut result = ProviderCapabilities::unsupported(ProviderId::Claude);
        result.version = (version.exit_code == Some(0))
            .then(|| String::from_utf8_lossy(&version.stdout).trim().to_owned());
        result.non_interactive = advertised(help.exit_code == Some(0) && text.contains("--print"));
        result.structured_output = advertised(
            text.contains("--output-format")
                && (text.contains("stream-json") || text.contains("stream_json")),
        );
        result.read_only = verified(
            help.exit_code == Some(0)
                && text.contains("--permission-mode")
                && text.contains("plan"),
        );
        result.writable = advertised(text.contains("acceptEdits"));
        result.session_resume = advertised(text.contains("--resume"));
        result.reasoning_effort =
            advertised(self.config.effort_flag_enabled && text.contains("--effort"));
        result.evidence = vec!["claude --version".to_owned(), "claude --help".to_owned()];
        Ok(result)
    }

    fn prepare_with(
        &self,
        request: &WorkerRequest,
        allow_resume: bool,
        allow_effort: bool,
    ) -> Result<PreparedInvocation, ProviderError> {
        ensure_provider(request, ProviderId::Claude)?;
        let mut args = vec![
            OsString::from("-p"),
            OsString::from("--output-format"),
            OsString::from("stream-json"),
            OsString::from("--verbose"),
            OsString::from("--permission-mode"),
            OsString::from(match request.sandbox {
                SandboxMode::ReadOnly => "plan",
                SandboxMode::WorkspaceWrite => "acceptEdits",
            }),
        ];
        if let Some(model) = request.model.as_ref().filter(|model| !model.is_empty()) {
            args.push(OsString::from("--model"));
            args.push(OsString::from(model));
        }
        if allow_effort && let Some(effort) = request.reasoning_effort {
            args.push(OsString::from("--effort"));
            args.push(OsString::from(effort_value(effort)));
        }
        if allow_resume && let Some(session) = request.resume_session_id.as_ref() {
            args.push(OsString::from("--resume"));
            args.push(OsString::from(session));
        }
        let (stdout_limit, stderr_limit) = output_limits(request);
        let invocation = PreparedInvocation {
            executable: self.config.executable.clone(),
            args,
            stdin: prompt_payload(request)?,
            working_directory: request.workspace_root.clone(),
            timeout_seconds: request.timeout_seconds,
            stdout_limit,
            stderr_limit,
            output: StructuredOutput::ClaudeStreamJson,
            codex_app_server: None,
            fallback: None,
        };
        invocation.validate()?;
        Ok(invocation)
    }
}

#[async_trait]
impl WorkerAdapter for ClaudeAdapter {
    fn provider(&self) -> ProviderId {
        ProviderId::Claude
    }

    async fn probe(&self) -> Result<ProviderHealth, ProviderError> {
        let capabilities = self.detected_capabilities().await?;
        let healthy = capabilities.non_interactive.usable()
            && capabilities.structured_output.usable()
            && capabilities.read_only.usable();
        Ok(ProviderHealth {
            provider: ProviderId::Claude,
            status: if healthy {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            },
            checked_at: Utc::now(),
            latency_ms: None,
            consecutive_failures: u32::from(!healthy),
            detail: (!healthy).then(|| "required Claude CLI options are missing".to_owned()),
        })
    }

    async fn capabilities(&self) -> Result<ProviderCapabilities, ProviderError> {
        self.detected_capabilities().await
    }

    async fn collect_usage(&self) -> Result<Vec<UsageSnapshot>, ProviderError> {
        let Some(invocation) = self.config.usage_probe.prepare(Path::new("."))? else {
            return Ok(vec![UsageSnapshot::unknown(
                ProviderId::Claude,
                self.config.usage_scope.clone(),
                Utc::now(),
            )]);
        };
        let output = self.runtime.run_usage_probe(invocation).await?;
        if output.exit_code != Some(0) {
            return Err(ProviderError::UsageProbe(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        Ok(vec![parse_usage_probe_output(
            ProviderId::Claude,
            self.config.usage_scope.clone(),
            &output.stdout,
            Utc::now(),
        )?])
    }

    fn prepare(&self, request: &WorkerRequest) -> Result<PreparedInvocation, ProviderError> {
        self.prepare_with(request, false, false)
    }

    async fn start(&self, request: WorkerRequest) -> Result<WorkerHandle, ProviderError> {
        let capabilities = self.detected_capabilities().await?;
        let permission = match request.sandbox {
            SandboxMode::ReadOnly => capabilities.read_only,
            SandboxMode::WorkspaceWrite => capabilities.writable,
        };
        if !capabilities.non_interactive.usable()
            || !capabilities.structured_output.usable()
            || !permission.usable()
        {
            return Err(ProviderError::Probe(
                "Claude CLI lacks required safe non-interactive capabilities".to_owned(),
            ));
        }
        let invocation = self.prepare_with(
            &request,
            capabilities.session_resume.usable(),
            capabilities.reasoning_effort.usable(),
        )?;
        self.runtime
            .start_worker(ProviderId::Claude, &request, invocation)
            .await
    }

    async fn next_event(&self, handle: &WorkerHandle) -> Result<Option<RawEvent>, ProviderError> {
        self.runtime.next_event(handle).await
    }

    async fn wait(&self, handle: &WorkerHandle) -> Result<crate::RuntimeOutput, ProviderError> {
        self.runtime.wait(handle).await
    }

    async fn checkpoint(
        &self,
        handle: &WorkerHandle,
    ) -> Result<UntrustedWorkerClaim, ProviderError> {
        self.runtime.checkpoint(handle).await
    }

    async fn cancel(&self, handle: &WorkerHandle) -> Result<CancelResult, ProviderError> {
        self.runtime.cancel(handle).await
    }

    async fn parse_event(&self, event: RawEvent) -> Result<WorkerEvent, ProviderError> {
        match event.channel {
            RawEventChannel::Protocol => {
                return Ok(WorkerEvent::Error {
                    code: Some("runtime_protocol_loss".to_owned()),
                    message: String::from_utf8_lossy(&event.bytes).into_owned(),
                    retryable: false,
                });
            }
            RawEventChannel::Stderr => {
                return Ok(WorkerEvent::Unknown {
                    event_type: "claude.stderr".to_owned(),
                    payload: serde_json::json!({ "sequence": event.sequence }),
                    affects_lifecycle: false,
                });
            }
            RawEventChannel::Stdout => {}
        }
        let value = serde_json::from_slice(&event.bytes)
            .map_err(|error| ProviderError::MalformedOutput(error.to_string()))?;
        parse_claude_event(value)
    }
}

fn effort_value(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
    }
}

fn advertised(value: bool) -> CapabilitySupport {
    if value {
        CapabilitySupport::Advertised
    } else {
        CapabilitySupport::Unsupported
    }
}

const fn verified(value: bool) -> CapabilitySupport {
    if value {
        CapabilitySupport::Verified
    } else {
        CapabilitySupport::Unsupported
    }
}
