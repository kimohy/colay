use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;
use codex_compat::{
    AppServerSessionPlan, CapabilityProbe, CapabilityProbeInput, CapabilitySupport as CodexSupport,
    CodexCapabilities, CodexEventParser, CodexInvocation, CodexRequest, CodexSandbox,
    CompatibilityError, CompatibilityStatus, ProbeCommandKind, ProbeOutput,
    ReasoningEffort as CodexEffort,
};
use orchestrator_domain::{
    CancelResult, CapabilitySupport, HealthStatus, ProviderCapabilities, ProviderHealth,
    ProviderId, QuotaScope, RawEvent, RawEventChannel, ReasoningEffort, SandboxMode,
    UntrustedWorkerClaim, UsageSnapshot, WorkerEvent, WorkerHandle, WorkerRequest,
};

use crate::adapter::{SharedRuntime, ensure_provider, output_limits, prompt_payload};
use crate::normalize::{codex_usage_observation, normalize_codex_event};
use crate::{
    PreparedInvocation, ProviderError, StructuredOutput, UsageProbeConfig, WorkerAdapter,
    parse_usage_probe_output,
};

#[derive(Debug, Clone)]
pub struct CodexAdapterConfig {
    pub executable: PathBuf,
    pub usage_probe: UsageProbeConfig,
    pub usage_scope: QuotaScope,
    pub allow_untested_read_only: bool,
}

/// Runtime feature gates corresponding to the repository's existing
/// `codex_app_server_adapter` and `codex_exec_fallback` config flags.
///
/// `exec_fallback` gates the exec transport itself as well as its use as an
/// App Server fallback. Disabling both flags deliberately leaves no transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexTransportFeatures {
    pub app_server_adapter: bool,
    pub exec_fallback: bool,
}

impl Default for CodexTransportFeatures {
    fn default() -> Self {
        Self {
            app_server_adapter: true,
            exec_fallback: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodexTransportPreference {
    #[default]
    ExecJsonlFirst,
    AppServerFirst,
}

pub struct CodexAdapter {
    config: CodexAdapterConfig,
    runtime: SharedRuntime,
    probe: CapabilityProbe,
    transport_features: CodexTransportFeatures,
    transport_preference: CodexTransportPreference,
}

impl CodexAdapter {
    #[must_use]
    pub fn new(config: CodexAdapterConfig, runtime: SharedRuntime) -> Self {
        Self {
            config,
            runtime,
            probe: CapabilityProbe::default(),
            transport_features: CodexTransportFeatures::default(),
            transport_preference: CodexTransportPreference::default(),
        }
    }

    /// Applies the externally loaded feature flags without coupling this crate
    /// to the state/config layer.
    #[must_use]
    pub const fn with_transport_features(mut self, features: CodexTransportFeatures) -> Self {
        self.transport_features = features;
        self
    }

    /// Overrides the default public-interface priority. The default remains
    /// CLI exec JSONL first; App Server first must be an explicit policy.
    #[must_use]
    pub const fn with_transport_preference(mut self, preference: CodexTransportPreference) -> Self {
        self.transport_preference = preference;
        self
    }

    async fn capability_report(&self) -> Result<codex_compat::CodexProbeReport, ProviderError> {
        let schema_dir = tempfile::tempdir()
            .map_err(|error| ProviderError::Probe(format!("schema tempdir: {error}")))?;
        let mut outputs = BTreeMap::new();
        for command in CapabilityProbe::plan(schema_dir.path()) {
            let args = command.args.iter().map(OsString::from).collect::<Vec<_>>();
            let output = self
                .runtime
                .run_probe(&self.config.executable, &args)
                .await?;
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            outputs.insert(
                command.kind,
                ProbeOutput {
                    exit_code: output.exit_code,
                    stdout,
                    stderr,
                },
            );
        }
        let generated_schema = read_generated_schemas(schema_dir.path()).or_else(|| {
            outputs
                .get(&ProbeCommandKind::AppServerSchema)
                .filter(|output| output.stdout.trim_start().starts_with('{'))
                .map(|output| output.stdout.clone())
        });
        Ok(self.probe.evaluate(&CapabilityProbeInput {
            outputs,
            generated_schema,
        }))
    }

    fn compatibility_request(request: &WorkerRequest) -> Result<CodexRequest, ProviderError> {
        ensure_provider(request, ProviderId::Codex)?;
        let payload = prompt_payload(request)?;
        Ok(CodexRequest {
            working_directory: request.workspace_root.clone(),
            prompt: String::from_utf8(payload).map_err(|_| ProviderError::InvalidUtf8)?,
            model: request.model.clone(),
            effort: request.reasoning_effort.map(map_effort),
            sandbox: match request.sandbox {
                SandboxMode::ReadOnly => CodexSandbox::ReadOnly,
                SandboxMode::WorkspaceWrite => CodexSandbox::WorkspaceWrite,
            },
            resume_session: request.resume_session_id.clone(),
            output_schema: None,
        })
    }

    fn prepare_exec(
        &self,
        request: &WorkerRequest,
        compat_request: &CodexRequest,
        capabilities: &CodexCapabilities,
    ) -> Result<PreparedInvocation, ProviderError> {
        let invocation =
            CodexInvocation::exec(&self.config.executable, compat_request, capabilities)?;
        let (stdout_limit, stderr_limit) = output_limits(request);
        let prepared = PreparedInvocation {
            executable: invocation.executable,
            args: invocation.args.into_iter().map(OsString::from).collect(),
            stdin: invocation.stdin,
            working_directory: invocation.working_directory,
            timeout_seconds: request.timeout_seconds,
            stdout_limit,
            stderr_limit,
            output: StructuredOutput::CodexJsonl,
            codex_app_server: None,
            fallback: None,
        };
        prepared.validate()?;
        Ok(prepared)
    }

    fn prepare_app_server(
        &self,
        request: &WorkerRequest,
        compat_request: &CodexRequest,
        capabilities: &CodexCapabilities,
    ) -> Result<PreparedInvocation, ProviderError> {
        let mut app_request = compat_request.clone();
        if capabilities.app_server_reasoning_effort != CodexSupport::Verified {
            app_request.effort = None;
        }
        let invocation = CodexInvocation::app_server(&self.config.executable, &app_request);
        let (stdout_limit, stderr_limit) = output_limits(request);
        let prepared = PreparedInvocation {
            executable: invocation.executable,
            args: invocation.args.into_iter().map(OsString::from).collect(),
            stdin: invocation.stdin,
            working_directory: invocation.working_directory,
            timeout_seconds: request.timeout_seconds,
            stdout_limit,
            stderr_limit,
            output: StructuredOutput::CodexAppServerStdio,
            codex_app_server: Some(AppServerSessionPlan::new(app_request)),
            fallback: None,
        };
        prepared.validate()?;
        Ok(prepared)
    }

    fn prepare_with_capabilities(
        &self,
        request: &WorkerRequest,
        capabilities: &CodexCapabilities,
    ) -> Result<PreparedInvocation, ProviderError> {
        let compat_request = Self::compatibility_request(request)?;
        let sandbox_support = match request.sandbox {
            SandboxMode::ReadOnly => capabilities.read_only_sandbox == CodexSupport::Verified,
            SandboxMode::WorkspaceWrite => {
                capabilities.workspace_write_sandbox == CodexSupport::Verified
            }
        };
        let exec_available = self.transport_features.exec_fallback
            && capabilities.exec.is_available()
            && capabilities.jsonl_output.is_available()
            && match request.sandbox {
                SandboxMode::ReadOnly => capabilities.read_only_sandbox.is_available(),
                SandboxMode::WorkspaceWrite => capabilities.workspace_write_sandbox.is_available(),
            };
        let exec_verified = self.transport_features.exec_fallback
            && capabilities.exec == CodexSupport::Verified
            && capabilities.jsonl_output == CodexSupport::Verified
            && sandbox_support;
        let app_server_available = self.transport_features.app_server_adapter
            && capabilities.app_server == CodexSupport::Verified
            && sandbox_support;

        match self.transport_preference {
            CodexTransportPreference::ExecJsonlFirst if exec_available => {
                let mut primary = self.prepare_exec(request, &compat_request, capabilities)?;
                if app_server_available {
                    primary.fallback = Some(Box::new(self.prepare_app_server(
                        request,
                        &compat_request,
                        capabilities,
                    )?));
                }
                primary.validate()?;
                Ok(primary)
            }
            CodexTransportPreference::AppServerFirst if app_server_available => {
                let mut primary =
                    self.prepare_app_server(request, &compat_request, capabilities)?;
                if exec_verified {
                    primary.fallback = Some(Box::new(self.prepare_exec(
                        request,
                        &compat_request,
                        capabilities,
                    )?));
                }
                primary.validate()?;
                Ok(primary)
            }
            _ if app_server_available => {
                self.prepare_app_server(request, &compat_request, capabilities)
            }
            _ if exec_available => self.prepare_exec(request, &compat_request, capabilities),
            _ => Err(CompatibilityError::NoSupportedTransport.into()),
        }
    }

    /// Extracts local token-ledger evidence without treating it as quota usage.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] for invalid UTF-8 or an incompatible Codex
    /// event contract.
    pub fn normalize_usage(
        &self,
        event: &RawEvent,
    ) -> Result<Option<orchestrator_domain::UsageObservation>, ProviderError> {
        if event.channel != RawEventChannel::Stdout {
            return Ok(None);
        }
        let line = std::str::from_utf8(&event.bytes).map_err(|_| ProviderError::InvalidUtf8)?;
        let parsed = CodexEventParser.parse_line(usize_from_sequence(event.sequence), line)?;
        Ok(codex_usage_observation(&parsed))
    }
}

#[async_trait]
impl WorkerAdapter for CodexAdapter {
    fn provider(&self) -> ProviderId {
        ProviderId::Codex
    }

    async fn probe(&self) -> Result<ProviderHealth, ProviderError> {
        let report = self.capability_report().await?;
        let status = match report.status {
            CompatibilityStatus::Compatible => HealthStatus::Healthy,
            CompatibilityStatus::CompatibleWithWarnings | CompatibilityStatus::Untested => {
                HealthStatus::Degraded
            }
            CompatibilityStatus::Incompatible => HealthStatus::Unhealthy,
        };
        Ok(ProviderHealth {
            provider: ProviderId::Codex,
            status,
            checked_at: Utc::now(),
            latency_ms: None,
            consecutive_failures: u32::from(status == HealthStatus::Unhealthy),
            detail: (!report.diagnostics.is_empty()).then(|| report.diagnostics.join("; ")),
        })
    }

    async fn capabilities(&self) -> Result<ProviderCapabilities, ProviderError> {
        let report = self.capability_report().await?;
        let untested = report.status == CompatibilityStatus::Untested;
        let incompatible = report.status == CompatibilityStatus::Incompatible;
        let mut capabilities = ProviderCapabilities::unsupported(ProviderId::Codex);
        capabilities.version = report
            .capabilities
            .version
            .as_ref()
            .map(ToString::to_string);
        capabilities.non_interactive = strongest_support(
            map_support(report.capabilities.exec),
            map_support(report.capabilities.app_server),
        );
        capabilities.structured_output = strongest_support(
            map_support(report.capabilities.jsonl_output),
            map_support(report.capabilities.app_server),
        );
        capabilities.read_only =
            if incompatible || (untested && !self.config.allow_untested_read_only) {
                CapabilitySupport::Unsupported
            } else {
                map_support(report.capabilities.read_only_sandbox)
            };
        capabilities.writable = if incompatible || untested {
            CapabilitySupport::Unsupported
        } else {
            map_support(report.capabilities.workspace_write_sandbox)
        };
        capabilities.session_resume = map_support(report.capabilities.session_resume);
        capabilities.output_schema = map_support(report.capabilities.output_schema);
        capabilities.app_server = map_support(report.capabilities.app_server);
        capabilities.reasoning_effort = map_support(report.capabilities.reasoning_effort);
        capabilities.usage_events = map_support(report.capabilities.usage_events);
        capabilities.evidence = report
            .evidence
            .into_iter()
            .map(|item| format!("{}: {:?} ({})", item.capability, item.support, item.source))
            .chain(report.diagnostics)
            .collect();
        Ok(capabilities)
    }

    async fn collect_usage(&self) -> Result<Vec<UsageSnapshot>, ProviderError> {
        let Some(invocation) = self.config.usage_probe.prepare(Path::new("."))? else {
            return Ok(vec![UsageSnapshot::unknown(
                ProviderId::Codex,
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
            ProviderId::Codex,
            self.config.usage_scope.clone(),
            &output.stdout,
            Utc::now(),
        )?])
    }

    fn prepare(&self, request: &WorkerRequest) -> Result<PreparedInvocation, ProviderError> {
        // Conservative generic invocation: optional flags are emitted only by
        // `start` after the runtime probe has advertised them.
        self.prepare_with_capabilities(request, &CodexCapabilities::default())
    }

    async fn start(&self, request: WorkerRequest) -> Result<WorkerHandle, ProviderError> {
        let report = self.capability_report().await?;
        match (report.status, request.sandbox) {
            (CompatibilityStatus::Incompatible, _)
            | (CompatibilityStatus::Untested, SandboxMode::WorkspaceWrite) => {
                return Err(ProviderError::Probe(
                    "Codex compatibility guard blocked this worker mode".to_owned(),
                ));
            }
            (CompatibilityStatus::Untested, SandboxMode::ReadOnly)
                if !self.config.allow_untested_read_only =>
            {
                return Err(ProviderError::Probe(
                    "untested Codex read-only mode is disabled".to_owned(),
                ));
            }
            _ => {}
        }
        let invocation = self.prepare_with_capabilities(&request, &report.capabilities)?;
        self.runtime
            .start_worker(ProviderId::Codex, &request, invocation)
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
                return Ok(runtime_protocol_error(&event));
            }
            RawEventChannel::Stderr => {
                return Ok(WorkerEvent::Unknown {
                    event_type: "codex.stderr".to_owned(),
                    payload: serde_json::json!({ "sequence": event.sequence }),
                    affects_lifecycle: false,
                });
            }
            RawEventChannel::Stdout => {}
        }
        let line = std::str::from_utf8(&event.bytes).map_err(|_| ProviderError::InvalidUtf8)?;
        let parsed = CodexEventParser.parse_line(usize_from_sequence(event.sequence), line)?;
        normalize_codex_event(parsed)
    }
}

fn runtime_protocol_error(event: &RawEvent) -> WorkerEvent {
    WorkerEvent::Error {
        code: Some("runtime_protocol_loss".to_owned()),
        message: String::from_utf8_lossy(&event.bytes).into_owned(),
        retryable: false,
    }
}

fn map_effort(effort: ReasoningEffort) -> CodexEffort {
    match effort {
        ReasoningEffort::Low => CodexEffort::Low,
        ReasoningEffort::Medium => CodexEffort::Medium,
        ReasoningEffort::High => CodexEffort::High,
    }
}

fn map_support(support: CodexSupport) -> CapabilitySupport {
    match support {
        CodexSupport::Unsupported => CapabilitySupport::Unsupported,
        CodexSupport::Advertised => CapabilitySupport::Advertised,
        CodexSupport::Verified => CapabilitySupport::Verified,
        CodexSupport::Degraded => CapabilitySupport::Degraded,
    }
}

fn strongest_support(left: CapabilitySupport, right: CapabilitySupport) -> CapabilitySupport {
    use CapabilitySupport::{Advertised, Degraded, Unsupported, Verified};
    match (left, right) {
        (Verified, _) | (_, Verified) => Verified,
        (Advertised, _) | (_, Advertised) => Advertised,
        (Degraded, _) | (_, Degraded) => Degraded,
        (Unsupported, Unsupported) => Unsupported,
    }
}

fn usize_from_sequence(sequence: u64) -> usize {
    usize::try_from(sequence).unwrap_or(usize::MAX)
}

fn read_generated_schemas(directory: &Path) -> Option<String> {
    let mut paths = std::fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    let mut result = String::new();
    for path in paths {
        if let Ok(text) = std::fs::read_to_string(path) {
            result.push_str(&text);
            result.push('\n');
        }
    }
    (!result.is_empty()).then_some(result)
}
