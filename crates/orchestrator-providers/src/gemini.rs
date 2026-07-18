use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;
use orchestrator_domain::{
    CancelResult, CapabilitySupport, HealthStatus, ProviderCapabilities, ProviderHealth,
    ProviderId, QuotaScope, RawEvent, RawEventChannel, SandboxMode, UntrustedWorkerClaim,
    UsageSnapshot, WorkerEvent, WorkerHandle, WorkerRequest,
};

use crate::adapter::{SharedRuntime, ensure_provider, output_limits, prompt_payload};
use crate::{
    PreparedInvocation, ProviderError, StructuredOutput, UsageProbeConfig, WorkerAdapter,
    parse_gemini_event, parse_usage_probe_output,
};

const STDIN_BRIDGE: &str = "Follow the enterprise task envelope provided on stdin. Return structured progress events and obey the configured permission mode.";

#[derive(Debug, Clone)]
pub struct GeminiAdapterConfig {
    pub executable: PathBuf,
    pub usage_probe: UsageProbeConfig,
    pub usage_scope: QuotaScope,
}

pub struct GeminiAdapter {
    config: GeminiAdapterConfig,
    runtime: SharedRuntime,
}

impl GeminiAdapter {
    #[must_use]
    pub const fn new(config: GeminiAdapterConfig, runtime: SharedRuntime) -> Self {
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
        let mut result = ProviderCapabilities::unsupported(ProviderId::Gemini);
        result.version = (version.exit_code == Some(0))
            .then(|| String::from_utf8_lossy(&version.stdout).trim().to_owned());
        result.non_interactive = advertised(help.exit_code == Some(0) && text.contains("--prompt"));
        result.structured_output = advertised(
            text.contains("--output-format")
                && (text.contains("stream-json") || text.contains("stream_json")),
        );
        result.read_only = verified(
            help.exit_code == Some(0) && text.contains("--approval-mode") && text.contains("plan"),
        );
        result.writable = advertised(text.contains("auto_edit"));
        result.session_resume = advertised(text.contains("--resume"));
        result.evidence = vec!["gemini --version".to_owned(), "gemini --help".to_owned()];
        Ok(result)
    }

    fn prepare_with(
        &self,
        request: &WorkerRequest,
        allow_resume: bool,
    ) -> Result<PreparedInvocation, ProviderError> {
        ensure_provider(request, ProviderId::Gemini)?;
        let mut args = vec![
            OsString::from("-p"),
            OsString::from(STDIN_BRIDGE),
            OsString::from("--output-format"),
            OsString::from("stream-json"),
            OsString::from("--approval-mode"),
            OsString::from(match request.sandbox {
                SandboxMode::ReadOnly => "plan",
                SandboxMode::WorkspaceWrite => "auto_edit",
            }),
        ];
        if let Some(model) = request.model.as_ref().filter(|model| !model.is_empty()) {
            args.push(OsString::from("--model"));
            args.push(OsString::from(model));
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
            output: StructuredOutput::GeminiStreamJson,
            codex_app_server: None,
            fallback: None,
        };
        invocation.validate()?;
        Ok(invocation)
    }
}

#[async_trait]
impl WorkerAdapter for GeminiAdapter {
    fn provider(&self) -> ProviderId {
        ProviderId::Gemini
    }

    async fn probe(&self) -> Result<ProviderHealth, ProviderError> {
        let capabilities = self.detected_capabilities().await?;
        let healthy = capabilities.non_interactive.usable()
            && capabilities.structured_output.usable()
            && capabilities.read_only.usable();
        Ok(ProviderHealth {
            provider: ProviderId::Gemini,
            status: if healthy {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            },
            checked_at: Utc::now(),
            latency_ms: None,
            consecutive_failures: u32::from(!healthy),
            detail: (!healthy).then(|| "required Gemini CLI options are missing".to_owned()),
        })
    }

    async fn capabilities(&self) -> Result<ProviderCapabilities, ProviderError> {
        self.detected_capabilities().await
    }

    async fn collect_usage(&self) -> Result<Vec<UsageSnapshot>, ProviderError> {
        let Some(invocation) = self.config.usage_probe.prepare(Path::new("."))? else {
            return Ok(vec![UsageSnapshot::unknown(
                ProviderId::Gemini,
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
            ProviderId::Gemini,
            self.config.usage_scope.clone(),
            &output.stdout,
            Utc::now(),
        )?])
    }

    fn prepare(&self, request: &WorkerRequest) -> Result<PreparedInvocation, ProviderError> {
        self.prepare_with(request, false)
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
                "Gemini CLI lacks required safe non-interactive capabilities".to_owned(),
            ));
        }
        let allow_resume =
            request.resume_session_id.is_none() || capabilities.session_resume.usable();
        let invocation = self.prepare_with(&request, allow_resume)?;
        self.runtime
            .start_worker(ProviderId::Gemini, &request, invocation)
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
                    event_type: "gemini.stderr".to_owned(),
                    payload: serde_json::json!({ "sequence": event.sequence }),
                    affects_lifecycle: false,
                });
            }
            RawEventChannel::Stdout => {}
        }
        let value = serde_json::from_slice(&event.bytes)
            .map_err(|error| ProviderError::MalformedOutput(error.to_string()))?;
        parse_gemini_event(value)
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
