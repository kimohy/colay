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
    parse_usage_probe_output,
};

#[derive(Debug, Clone)]
pub struct AgyAdapterConfig {
    pub executable: PathBuf,
    pub usage_probe: UsageProbeConfig,
    pub usage_scope: QuotaScope,
}

pub struct AgyAdapter {
    config: AgyAdapterConfig,
    runtime: SharedRuntime,
}

impl AgyAdapter {
    #[must_use]
    pub const fn new(config: AgyAdapterConfig, runtime: SharedRuntime) -> Self {
        Self { config, runtime }
    }

    async fn probe_output(&self, args: &[&str]) -> Result<crate::RuntimeOutput, ProviderError> {
        let args = args.iter().map(OsString::from).collect::<Vec<_>>();
        self.runtime.run_probe(&self.config.executable, &args).await
    }

    async fn detected_capabilities(&self) -> Result<ProviderCapabilities, ProviderError> {
        let version = self.probe_output(&["--version"]).await?;
        let help = self.probe_output(&["--help"]).await?;
        let help_text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&help.stdout),
            String::from_utf8_lossy(&help.stderr)
        );
        Ok(capabilities_from_probe(
            version.exit_code == Some(0),
            &String::from_utf8_lossy(&version.stdout),
            help.exit_code == Some(0),
            &help_text,
        ))
    }

    fn prepare_with(
        &self,
        request: &WorkerRequest,
        allow_resume: bool,
    ) -> Result<PreparedInvocation, ProviderError> {
        ensure_provider(request, ProviderId::Agy)?;
        let (stdout_limit, stderr_limit) = output_limits(request);
        let invocation = PreparedInvocation {
            executable: self.config.executable.clone(),
            args: invocation_args(request, allow_resume),
            stdin: prompt_payload(request)?,
            working_directory: request.workspace_root.clone(),
            timeout_seconds: request.timeout_seconds,
            stdout_limit,
            stderr_limit,
            output: StructuredOutput::AgyText,
            codex_app_server: None,
            fallback: None,
        };
        invocation.validate()?;
        Ok(invocation)
    }
}

#[async_trait]
impl WorkerAdapter for AgyAdapter {
    fn provider(&self) -> ProviderId {
        ProviderId::Agy
    }

    async fn probe(&self) -> Result<ProviderHealth, ProviderError> {
        let capabilities = self.detected_capabilities().await?;
        let healthy = capabilities.non_interactive.usable()
            && capabilities.structured_output.usable()
            && capabilities.read_only.usable();
        Ok(ProviderHealth {
            provider: ProviderId::Agy,
            status: if healthy {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            },
            checked_at: Utc::now(),
            latency_ms: None,
            consecutive_failures: u32::from(!healthy),
            detail: (!healthy).then(|| "required Agy CLI options are missing".to_owned()),
        })
    }

    async fn capabilities(&self) -> Result<ProviderCapabilities, ProviderError> {
        self.detected_capabilities().await
    }

    async fn collect_usage(&self) -> Result<Vec<UsageSnapshot>, ProviderError> {
        let Some(invocation) = self.config.usage_probe.prepare(Path::new("."))? else {
            return Ok(vec![UsageSnapshot::unknown(
                ProviderId::Agy,
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
            ProviderId::Agy,
            self.config.usage_scope.clone(),
            &output.stdout,
            Utc::now(),
        )?])
    }

    fn prepare(&self, request: &WorkerRequest) -> Result<PreparedInvocation, ProviderError> {
        self.prepare_with(request, true)
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
                "Agy CLI lacks required safe non-interactive capabilities".to_owned(),
            ));
        }
        let allow_resume =
            request.resume_session_id.is_none() || capabilities.session_resume.usable();
        let invocation = self.prepare_with(&request, allow_resume)?;
        self.runtime
            .start_worker(ProviderId::Agy, &request, invocation)
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
            RawEventChannel::Stdout => Ok(WorkerEvent::Message {
                text: String::from_utf8_lossy(&event.bytes).into_owned(),
            }),
            RawEventChannel::Stderr => Ok(WorkerEvent::Unknown {
                event_type: "agy.stderr".to_owned(),
                payload: serde_json::json!({ "sequence": event.sequence }),
                affects_lifecycle: false,
            }),
            RawEventChannel::Protocol => parse_exit_event(&event.bytes),
        }
    }
}

fn parse_exit_event(bytes: &[u8]) -> Result<WorkerEvent, ProviderError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| ProviderError::MalformedOutput(error.to_string()))?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("orchestrator.process_exited")
    {
        return Err(ProviderError::MalformedOutput(
            "unexpected Agy runtime protocol event".to_owned(),
        ));
    }
    let exit_code = value
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| {
            ProviderError::MalformedOutput("Agy exit event has no integer exit code".to_owned())
        })?;
    if exit_code == 0 {
        Ok(WorkerEvent::Completed {
            summary: None,
            usage: None,
        })
    } else {
        Ok(WorkerEvent::Error {
            code: Some("agy_process_exit".to_owned()),
            message: format!("Agy exited with code {exit_code}"),
            retryable: false,
        })
    }
}

fn invocation_args(request: &WorkerRequest, allow_resume: bool) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--print"),
        OsString::from("--mode"),
        OsString::from(match request.sandbox {
            SandboxMode::ReadOnly => "plan",
            SandboxMode::WorkspaceWrite => "accept-edits",
        }),
        OsString::from("--sandbox"),
    ];
    if let Some(model) = request.model.as_ref().filter(|model| !model.is_empty()) {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }
    if allow_resume && let Some(session) = request.resume_session_id.as_ref() {
        args.push(OsString::from("--conversation"));
        args.push(OsString::from(session));
    }
    args
}

fn capabilities_from_probe(
    version_succeeded: bool,
    version_text: &str,
    help_succeeded: bool,
    help_text: &str,
) -> ProviderCapabilities {
    let mut result = ProviderCapabilities::unsupported(ProviderId::Agy);
    result.version = version_succeeded.then(|| version_text.trim().to_owned());
    result.non_interactive = advertised(help_succeeded && help_text.contains("--print"));
    result.read_only =
        verified(help_succeeded && help_text.contains("--mode") && help_text.contains("plan"));
    result.writable = advertised(help_succeeded && help_text.contains("accept-edits"));
    result.session_resume = advertised(help_succeeded && help_text.contains("--conversation"));
    result.structured_output = if result.non_interactive.usable()
        && result.read_only.usable()
        && help_text.contains("--sandbox")
    {
        CapabilitySupport::Degraded
    } else {
        CapabilitySupport::Unsupported
    };
    result.evidence = vec!["agy --version".to_owned(), "agy --help".to_owned()];
    result
}

const fn advertised(value: bool) -> CapabilitySupport {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use orchestrator_domain::{
        AttemptId, CapabilitySupport, ModelProfile, ProviderId, QuotaPeriod, QuotaScope, RawEvent,
        RawEventChannel, ReasoningEffort, SandboxMode, SchemaVersion, TaskId, UsageConfidence,
        UsageUnit, WorkerEvent, WorkerRequest,
    };

    use super::*;
    use crate::{ProcessAdapterRuntime, StructuredOutput, UsageProbeConfig, WorkerAdapter};

    fn request(sandbox: SandboxMode) -> WorkerRequest {
        WorkerRequest {
            schema_version: SchemaVersion::v1(),
            task_id: TaskId::new(),
            attempt_id: AttemptId::new(),
            provider: ProviderId::Agy,
            objective: "test Agy".to_owned(),
            prompt: "perform the task".to_owned(),
            constraints: Vec::new(),
            acceptance_criteria: Vec::new(),
            workspace_root: PathBuf::from("."),
            sandbox,
            profile: ModelProfile::Standard,
            model: Some("gemini-3.5-flash-medium".to_owned()),
            reasoning_effort: Some(ReasoningEffort::Medium),
            timeout_seconds: 60,
            max_output_bytes: 1024,
            resume_session_id: None,
            handover_payload: None,
        }
    }

    #[test]
    fn prepares_safe_read_only_and_writable_arguments() {
        let read_only = invocation_args(&request(SandboxMode::ReadOnly), true)
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            read_only,
            [
                "--print",
                "--mode",
                "plan",
                "--sandbox",
                "--model",
                "gemini-3.5-flash-medium",
            ]
        );
        let writable = invocation_args(&request(SandboxMode::WorkspaceWrite), true)
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            writable
                .windows(2)
                .any(|pair| pair == ["--mode", "accept-edits"])
        );
        assert!(
            !writable
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
    }

    #[test]
    fn resume_is_a_separate_conversation_argument() {
        let mut worker = request(SandboxMode::ReadOnly);
        worker.resume_session_id = Some("conversation-7".to_owned());
        let args = invocation_args(&worker, true)
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--conversation", "conversation-7"])
        );
    }

    #[test]
    fn observed_help_is_a_degraded_plain_text_contract() {
        let capabilities = capabilities_from_probe(
            true,
            "1.1.4",
            true,
            "--print --mode plan accept-edits --sandbox --model --conversation",
        );
        assert_eq!(capabilities.provider, ProviderId::Agy);
        assert_eq!(capabilities.non_interactive, CapabilitySupport::Advertised);
        assert_eq!(capabilities.read_only, CapabilitySupport::Verified);
        assert_eq!(capabilities.writable, CapabilitySupport::Advertised);
        assert_eq!(capabilities.session_resume, CapabilitySupport::Advertised);
        assert_eq!(capabilities.structured_output, CapabilitySupport::Degraded);
        assert_eq!(capabilities.output_schema, CapabilitySupport::Unsupported);
    }

    #[test]
    fn adapter_prepares_agy_text_without_starting_inference() -> Result<(), ProviderError> {
        let adapter = AgyAdapter::new(
            AgyAdapterConfig {
                executable: PathBuf::from("agy"),
                usage_probe: UsageProbeConfig::ManualOrLedger,
                usage_scope: QuotaScope::new(
                    "agy_daily",
                    QuotaPeriod::CalendarDay,
                    UsageUnit::Custom("provider_defined".to_owned()),
                ),
            },
            Arc::new(ProcessAdapterRuntime::default()),
        );
        let invocation = adapter.prepare(&request(SandboxMode::ReadOnly))?;
        assert_eq!(invocation.output, StructuredOutput::AgyText);
        assert_eq!(invocation.executable, PathBuf::from("agy"));
        Ok(())
    }

    #[tokio::test]
    async fn missing_usage_probe_stays_unknown_and_separate() -> Result<(), ProviderError> {
        let adapter = AgyAdapter::new(
            AgyAdapterConfig {
                executable: PathBuf::from("agy"),
                usage_probe: UsageProbeConfig::ManualOrLedger,
                usage_scope: QuotaScope::new(
                    "agy_daily",
                    QuotaPeriod::CalendarDay,
                    UsageUnit::Custom("provider_defined".to_owned()),
                ),
            },
            Arc::new(ProcessAdapterRuntime::default()),
        );
        let usage = adapter.collect_usage().await?;
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].provider, ProviderId::Agy);
        assert_eq!(usage[0].confidence, UsageConfidence::Unknown);
        assert!(usage[0].used.is_none());
        assert!(usage[0].remaining.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn plain_text_and_runtime_exit_are_normalized_strictly() -> Result<(), ProviderError> {
        let adapter = AgyAdapter::new(
            AgyAdapterConfig {
                executable: PathBuf::from("agy"),
                usage_probe: UsageProbeConfig::ManualOrLedger,
                usage_scope: QuotaScope::new(
                    "agy_daily",
                    QuotaPeriod::CalendarDay,
                    UsageUnit::Custom("provider_defined".to_owned()),
                ),
            },
            Arc::new(ProcessAdapterRuntime::default()),
        );
        let message = adapter
            .parse_event(RawEvent {
                channel: RawEventChannel::Stdout,
                sequence: 1,
                bytes: b"done".to_vec(),
                received_at: chrono::Utc::now(),
            })
            .await?;
        assert_eq!(
            message,
            WorkerEvent::Message {
                text: "done".to_owned()
            }
        );

        let completed = adapter
            .parse_event(RawEvent {
                channel: RawEventChannel::Protocol,
                sequence: 2,
                bytes: br#"{"type":"orchestrator.process_exited","exit_code":0}"#.to_vec(),
                received_at: chrono::Utc::now(),
            })
            .await?;
        assert!(matches!(
            completed,
            WorkerEvent::Completed { usage: None, .. }
        ));

        let failed = adapter
            .parse_event(RawEvent {
                channel: RawEventChannel::Protocol,
                sequence: 3,
                bytes: br#"{"type":"orchestrator.process_exited","exit_code":17}"#.to_vec(),
                received_at: chrono::Utc::now(),
            })
            .await?;
        assert!(matches!(
            failed,
            WorkerEvent::Error {
                code: Some(ref code),
                retryable: false,
                ..
            } if code == "agy_process_exit"
        ));

        let malformed = adapter
            .parse_event(RawEvent {
                channel: RawEventChannel::Protocol,
                sequence: 4,
                bytes: br#"{"type":"unexpected","exit_code":0}"#.to_vec(),
                received_at: chrono::Utc::now(),
            })
            .await;
        assert!(matches!(malformed, Err(ProviderError::MalformedOutput(_))));
        Ok(())
    }
}
