use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;
use orchestrator_domain::{
    CancelResult, CapabilitySupport, HealthStatus, ProviderCapabilities, ProviderHealth,
    ProviderId, QuotaScope, RawEvent, RawEventChannel, SandboxMode, UntrustedWorkerClaim,
    UsageSnapshot, WorkerEvent, WorkerHandle, WorkerRequest,
};
use semver::Version;

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
            // Agy's plain stdout is one semantic response split into transport
            // frames. Preserve it as deltas so consumers reassemble and redact
            // across frame boundaries without inventing newlines.
            RawEventChannel::Stdout => Ok(WorkerEvent::MessageDelta {
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
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("orchestrator.frames_dropped") => {
            return Ok(WorkerEvent::Error {
                code: Some("runtime_protocol_loss".to_owned()),
                message: "Agy runtime dropped provider output frames".to_owned(),
                retryable: false,
            });
        }
        Some("orchestrator.process_exited") => {}
        _ => {
            return Err(ProviderError::MalformedOutput(
                "unexpected Agy runtime protocol event".to_owned(),
            ));
        }
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
    let piped_prompt =
        supports_piped_prompt(version_succeeded, version_text, help_succeeded, help_text);
    result.non_interactive = advertised(piped_prompt);
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
    result.evidence = vec![
        "agy --version".to_owned(),
        "agy --help".to_owned(),
        format!(
            "piped stdin prompt: minimum version 1.1.2 or explicit public help; supported={piped_prompt}"
        ),
    ];
    result
}

fn supports_piped_prompt(
    version_succeeded: bool,
    version_text: &str,
    help_succeeded: bool,
    help_text: &str,
) -> bool {
    let version_supported = version_succeeded
        && parse_agy_version(version_text).is_some_and(|version| version >= Version::new(1, 1, 2));
    let help_supported = help_succeeded && help_explicitly_supports_piped_prompt(help_text);
    version_supported || help_supported
}

fn parse_agy_version(version_text: &str) -> Option<Version> {
    let trimmed = version_text.trim();
    if let Some(version) = parse_version_token(trimmed) {
        return Some(version);
    }

    version_text.lines().find_map(|line| {
        let tokens = line.split_ascii_whitespace().collect::<Vec<_>>();
        let provider_index = tokens.iter().position(|token| {
            let label = normalized_version_label(token);
            label.eq_ignore_ascii_case("agy")
                || label.eq_ignore_ascii_case("agy-cli")
                || label.eq_ignore_ascii_case("antigravity")
                || label.eq_ignore_ascii_case("antigravity-cli")
        })?;
        let provider_fields = &tokens[provider_index + 1..];
        provider_fields
            .first()
            .and_then(|token| parse_version_token(token))
            .or_else(|| {
                let [label, version, ..] = provider_fields else {
                    return None;
                };
                normalized_version_label(label)
                    .eq_ignore_ascii_case("version")
                    .then(|| parse_version_token(version))
                    .flatten()
            })
    })
}

fn normalized_version_label(token: &str) -> &str {
    token.trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '-')
}

fn parse_version_token(token: &str) -> Option<Version> {
    let token = token.trim_matches(|character: char| {
        matches!(character, ',' | ';' | ':' | '(' | ')' | '[' | ']')
    });
    let token = token
        .strip_prefix('v')
        .or_else(|| token.strip_prefix('V'))
        .unwrap_or(token);
    Version::parse(token).ok()
}

fn help_explicitly_supports_piped_prompt(help_text: &str) -> bool {
    help_text.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        let words = line
            .split(|character: char| !character.is_ascii_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>();
        let negative = line.contains("n't")
            || words.iter().any(|word| {
                matches!(
                    *word,
                    "no" | "not"
                        | "cannot"
                        | "cant"
                        | "unsupported"
                        | "disabled"
                        | "unavailable"
                        | "never"
                        | "without"
                        | "false"
                        | "deny"
                        | "denies"
                        | "denied"
                        | "refuse"
                        | "refuses"
                        | "refused"
                        | "fail"
                        | "fails"
                        | "failed"
                        | "ignore"
                        | "ignores"
                        | "ignored"
                        | "interactive"
                        | "off"
                        | "absent"
                        | "remove"
                        | "removes"
                        | "removed"
                        | "prohibit"
                        | "prohibits"
                        | "prohibited"
                )
            });
        let normalized = words.join(" ");
        let affirmative = [
            "read a piped prompt from stdin",
            "read piped prompt from stdin",
            "reads a piped prompt from stdin",
            "reads piped prompt from stdin",
            "accept a piped prompt from stdin",
            "accepts a piped prompt from stdin",
            "consume a piped prompt from stdin",
            "consumes a piped prompt from stdin",
            "read a piped stdin prompt",
            "reads a piped stdin prompt",
            "accept a piped stdin prompt",
            "accepts a piped stdin prompt",
            "supports piped stdin prompt",
            "supports piped stdin prompts",
            "piped stdin prompt is supported",
            "piped stdin prompts are supported",
            "piped stdin prompt support true",
            "piped stdin prompts support true",
            "piped prompt from stdin is supported",
        ]
        .iter()
        .any(|phrase| normalized.starts_with(phrase));
        !negative && affirmative
    })
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
        assert!(
            !writable
                .iter()
                .any(|arg| matches!(arg.as_str(), "--print" | "-p" | "--prompt"))
        );
    }

    #[test]
    fn prompt_stays_on_stdin_without_prompt_valued_flags() -> Result<(), ProviderError> {
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
        let args = invocation.args_lossy();

        assert!(
            !args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--print" | "-p" | "--prompt"))
        );
        assert!(!args.iter().any(|arg| arg.contains("perform the task")));
        assert!(String::from_utf8_lossy(&invocation.stdin).contains("perform the task"));
        Ok(())
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
        assert!(
            !args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--print" | "-p" | "--prompt"))
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
    fn piped_prompt_requires_minimum_version_or_explicit_help() {
        let options = "--print --mode plan accept-edits --sandbox --model --conversation";
        let too_old = capabilities_from_probe(true, "agy 1.1.1", true, options);
        assert_eq!(too_old.non_interactive, CapabilitySupport::Unsupported);
        assert_eq!(too_old.structured_output, CapabilitySupport::Unsupported);

        let future = capabilities_from_probe(true, "agy 1.1.10", true, options);
        assert_eq!(future.non_interactive, CapabilitySupport::Advertised);

        let observed = capabilities_from_probe(
            true,
            "agy development build",
            true,
            &format!("{options}\nRead a piped prompt from stdin and print the response."),
        );
        assert_eq!(observed.non_interactive, CapabilitySupport::Advertised);

        let unrelated_runtime_version =
            capabilities_from_probe(true, "runtime 9.9.9\nagy 1.1.1", true, options);
        assert_eq!(
            unrelated_runtime_version.non_interactive,
            CapabilitySupport::Unsupported
        );

        let provider_runtime_before_version =
            capabilities_from_probe(true, "agy runtime 9.9.9 version 1.1.1", true, options);
        assert_eq!(
            provider_runtime_before_version.non_interactive,
            CapabilitySupport::Unsupported
        );

        let multiple_labeled_versions = capabilities_from_probe(
            true,
            "agy runtime version 9.9.9 cli version 1.1.1",
            true,
            options,
        );
        assert_eq!(
            multiple_labeled_versions.non_interactive,
            CapabilitySupport::Unsupported
        );

        for line in [
            "Prompt cannot be read from stdin; piped input is unsupported.",
            "Prompt is not read from stdin.",
            "No stdin prompt support.",
            "stdin prompt support: false",
            "Interactive prompts read answers from stdin.",
            "stdin accepts answers to interactive prompts.",
            "Support for piped stdin prompts: off",
            "Support for piped stdin prompts is absent",
            "Support for piped stdin prompts was removed",
            "Piped stdin prompts are prohibited",
        ] {
            let negative_help = capabilities_from_probe(
                true,
                "agy development build",
                true,
                &format!("{options}\n{line}"),
            );
            assert_eq!(
                negative_help.non_interactive,
                CapabilitySupport::Unsupported,
                "help line: {line}"
            );
        }
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
            WorkerEvent::MessageDelta {
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

        let protocol_loss = adapter
            .parse_event(RawEvent {
                channel: RawEventChannel::Protocol,
                sequence: 4,
                bytes: br#"{"type":"orchestrator.frames_dropped","count":1}"#.to_vec(),
                received_at: chrono::Utc::now(),
            })
            .await?;
        assert!(matches!(
            protocol_loss,
            WorkerEvent::Error {
                code: Some(ref code),
                retryable: false,
                ..
            } if code == "runtime_protocol_loss"
        ));

        let malformed = adapter
            .parse_event(RawEvent {
                channel: RawEventChannel::Protocol,
                sequence: 5,
                bytes: br#"{"type":"unexpected","exit_code":0}"#.to_vec(),
                received_at: chrono::Utc::now(),
            })
            .await;
        assert!(matches!(malformed, Err(ProviderError::MalformedOutput(_))));
        Ok(())
    }
}
