use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use orchestrator_domain::{
    AttemptId, CancelOutcome, CancelResult, CapabilitySupport, ProviderCapabilities, ProviderId,
    RawEvent, RawEventChannel, UntrustedWorkerClaim, WorkerHandle, WorkerRequest,
};
use orchestrator_providers::{
    AdapterRuntime, ExecutableKind, ExecutableValidationContext, PreparedInvocation, ProviderError,
    ResolvedExecutable, RuntimeOutput, RuntimeTermination,
};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeRuntimeScenario {
    Success,
    QuotaExceeded,
    TerminalError,
    MalformedOutput,
    UnknownEvent,
    ProcessCrash,
    DiagnosticNoise,
    Timeout,
    SecretOutput,
    ConversationResponseAlias,
    ReadOnlyCommand,
    ReadOnlyCommandWithFileChange,
    AmbiguousScalarPrefix,
    GeminiDetailedErrorThenErrorResult,
    GeminiWarningBetweenDeltas,
    GeminiCompleteMessage,
}

fn is_conversation_scenario(scenario: FakeRuntimeScenario) -> bool {
    matches!(
        scenario,
        FakeRuntimeScenario::Success
            | FakeRuntimeScenario::ConversationResponseAlias
            | FakeRuntimeScenario::ReadOnlyCommand
            | FakeRuntimeScenario::ReadOnlyCommandWithFileChange
            | FakeRuntimeScenario::AmbiguousScalarPrefix
            | FakeRuntimeScenario::GeminiWarningBetweenDeltas
    )
}

/// Returns verified fake-only capability evidence for a read-only conversation provider.
#[must_use]
pub fn fake_conversation_capability(provider: ProviderId) -> ProviderCapabilities {
    let mut capability = ProviderCapabilities::unsupported(provider);
    capability.non_interactive = CapabilitySupport::Verified;
    capability.structured_output = CapabilitySupport::Verified;
    capability.read_only = CapabilitySupport::Verified;
    capability.evidence = vec![format!("fake {provider} marker")];
    capability
}

#[derive(Debug)]
struct FakeJob {
    provider: ProviderId,
    events: VecDeque<RawEvent>,
    output: RuntimeOutput,
    cancelled: bool,
    first_event_ready_at: Option<tokio::time::Instant>,
}

#[derive(Debug, Clone)]
pub struct FakeAdapterRuntime {
    allowed_executable: PathBuf,
    scenario: FakeRuntimeScenario,
    jobs: Arc<Mutex<HashMap<AttemptId, FakeJob>>>,
}

impl FakeAdapterRuntime {
    /// Creates a runtime locked to the compiled fake CLI path.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if the executable name is not exactly
    /// `fake-provider-cli` (platform extension excluded).
    pub fn new(
        allowed_executable: impl Into<PathBuf>,
        scenario: FakeRuntimeScenario,
    ) -> Result<Self, ProviderError> {
        let path = std::fs::canonicalize(allowed_executable.into()).map_err(|error| {
            ProviderError::Runtime(format!("fake provider path is not executable: {error}"))
        })?;
        let file_name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !file_name.eq_ignore_ascii_case("fake-provider-cli") {
            return Err(ProviderError::Runtime(
                "test runtime permits only fake-provider-cli".to_owned(),
            ));
        }
        Ok(Self {
            allowed_executable: path,
            scenario,
            jobs: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn ensure_fake(&self, executable: &Path) -> Result<(), ProviderError> {
        let canonical = std::fs::canonicalize(executable).map_err(|error| {
            ProviderError::Runtime(format!("fake provider path is unavailable: {error}"))
        })?;
        if canonical == self.allowed_executable {
            Ok(())
        } else {
            Err(ProviderError::Runtime(format!(
                "refusing non-fake executable {}",
                executable.display()
            )))
        }
    }

    /// Returns the number of fake jobs that observed a cancellation request.
    ///
    /// This is test evidence for lifecycle cleanup; it never starts a real provider.
    pub async fn cancelled_job_count(&self) -> usize {
        self.jobs
            .lock()
            .await
            .values()
            .filter(|job| job.cancelled)
            .count()
    }

    /// Returns the number of fake jobs started by this runtime.
    pub async fn started_job_count(&self) -> usize {
        self.jobs.lock().await.len()
    }
}

#[async_trait]
impl AdapterRuntime for FakeAdapterRuntime {
    async fn run_probe(
        &self,
        executable: &Path,
        args: &[OsString],
    ) -> Result<RuntimeOutput, ProviderError> {
        self.ensure_fake(executable)?;
        let args = args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let stdout = fake_probe_output(&args, None).into_bytes();
        Ok(RuntimeOutput {
            resolved_executable: None,
            exit_code: Some(0),
            termination: RuntimeTermination::Exited,
            tree_termination_error: None,
            stdout,
            stderr: Vec::new(),
            truncated: false,
        })
    }

    async fn start_worker(
        &self,
        provider: ProviderId,
        request: &WorkerRequest,
        invocation: PreparedInvocation,
    ) -> Result<WorkerHandle, ProviderError> {
        self.ensure_fake(&invocation.executable)?;
        let lines = if request.objective == "Conduct a read-only conversation turn"
            && is_conversation_scenario(self.scenario)
        {
            conversation_lines(provider, &request.prompt, self.scenario)
        } else {
            scenario_lines(provider, self.scenario)
        };
        let mut events = lines
            .into_iter()
            .enumerate()
            .map(|(index, mut bytes)| {
                // The production fake CLI writes every Agy text frame with
                // `println!`; preserve that transport byte in the in-memory
                // runtime now that Agy stdout is assembled as deltas.
                if provider == ProviderId::Agy {
                    bytes.push(b'\n');
                }
                RawEvent {
                    channel: RawEventChannel::Stdout,
                    sequence: u64::try_from(index + 1).unwrap_or(u64::MAX),
                    bytes,
                    received_at: Utc::now(),
                }
            })
            .collect::<VecDeque<_>>();
        if provider == ProviderId::Agy && self.scenario != FakeRuntimeScenario::Timeout {
            let exit_code = if matches!(
                self.scenario,
                FakeRuntimeScenario::ProcessCrash | FakeRuntimeScenario::DiagnosticNoise
            ) {
                17
            } else {
                0
            };
            events.push_back(RawEvent {
                channel: RawEventChannel::Protocol,
                sequence: u64::try_from(events.len() + 1).unwrap_or(u64::MAX),
                bytes: serde_json::to_vec(&serde_json::json!({
                    "type": "orchestrator.process_exited",
                    "exit_code": exit_code,
                }))
                .unwrap_or_default(),
                received_at: Utc::now(),
            });
        }
        let output = RuntimeOutput {
            resolved_executable: Some(ResolvedExecutable {
                configured: invocation.executable.clone(),
                path: self.allowed_executable.clone(),
                kind: ExecutableKind::Native,
                validation: ExecutableValidationContext {
                    working_directory: std::fs::canonicalize(&invocation.working_directory)
                        .map_err(|error| ProviderError::Runtime(error.to_string()))?,
                    search_directory: None,
                },
            }),
            exit_code: match self.scenario {
                FakeRuntimeScenario::ProcessCrash | FakeRuntimeScenario::DiagnosticNoise => {
                    Some(17)
                }
                FakeRuntimeScenario::Timeout => None,
                _ => Some(0),
            },
            termination: match self.scenario {
                FakeRuntimeScenario::Timeout => RuntimeTermination::TimedOut,
                _ => RuntimeTermination::Exited,
            },
            tree_termination_error: None,
            stdout: Vec::new(),
            stderr: match self.scenario {
                FakeRuntimeScenario::ProcessCrash => b"fake process crash".to_vec(),
                FakeRuntimeScenario::DiagnosticNoise => b"unsupported account for this client\n\
                    at first (client.js:1:1)\n\
                    at second (client.js:2:1)\n\
                    at third (client.js:3:1)\n\
                    at fourth (client.js:4:1)\n\
                    at fifth (client.js:5:1)\n\
                    at sixth (client.js:6:1)\n\
                    at sixth (client.js:6:1)\n\
                    retry with --dangerously-skip-permissions"
                    .to_vec(),
                _ => Vec::new(),
            },
            truncated: false,
        };
        self.jobs.lock().await.insert(
            request.attempt_id,
            FakeJob {
                provider,
                events,
                output,
                cancelled: false,
                first_event_ready_at: (self.scenario == FakeRuntimeScenario::TerminalError)
                    .then(|| tokio::time::Instant::now() + Duration::from_secs(6)),
            },
        );
        Ok(WorkerHandle {
            attempt_id: request.attempt_id,
            provider,
            process_id: None,
            session_id: None,
        })
    }

    async fn next_event(&self, handle: &WorkerHandle) -> Result<Option<RawEvent>, ProviderError> {
        let ready_at = {
            let jobs = self.jobs.lock().await;
            jobs.get(&handle.attempt_id)
                .ok_or_else(|| ProviderError::Runtime("unknown fake worker".to_owned()))?
                .first_event_ready_at
        };
        if let Some(ready_at) = ready_at {
            tokio::time::sleep_until(ready_at).await;
        }
        let mut jobs = self.jobs.lock().await;
        let job = jobs
            .get_mut(&handle.attempt_id)
            .ok_or_else(|| ProviderError::Runtime("unknown fake worker".to_owned()))?;
        job.first_event_ready_at = None;
        Ok(job.events.pop_front())
    }

    async fn wait(&self, handle: &WorkerHandle) -> Result<RuntimeOutput, ProviderError> {
        let jobs = self.jobs.lock().await;
        let job = jobs
            .get(&handle.attempt_id)
            .ok_or_else(|| ProviderError::Runtime("unknown fake worker".to_owned()))?;
        if job.cancelled {
            return Ok(RuntimeOutput {
                resolved_executable: job.output.resolved_executable.clone(),
                exit_code: Some(130),
                termination: RuntimeTermination::Cancelled,
                tree_termination_error: None,
                stdout: Vec::new(),
                stderr: b"cancelled".to_vec(),
                truncated: false,
            });
        }
        Ok(job.output.clone())
    }

    async fn checkpoint(
        &self,
        handle: &WorkerHandle,
    ) -> Result<UntrustedWorkerClaim, ProviderError> {
        let jobs = self.jobs.lock().await;
        let job = jobs
            .get(&handle.attempt_id)
            .ok_or_else(|| ProviderError::Checkpoint("unknown fake worker".to_owned()))?;
        Ok(UntrustedWorkerClaim {
            provider: job.provider,
            summary: "fake worker checkpoint".to_owned(),
            claimed_files_changed: Vec::new(),
            claimed_tests_passed: Vec::new(),
        })
    }

    async fn cancel(&self, handle: &WorkerHandle) -> Result<CancelResult, ProviderError> {
        let mut jobs = self.jobs.lock().await;
        let job = jobs
            .get_mut(&handle.attempt_id)
            .ok_or_else(|| ProviderError::Runtime("unknown fake worker".to_owned()))?;
        let outcome = if job.cancelled {
            CancelOutcome::AlreadyExited
        } else {
            job.cancelled = true;
            CancelOutcome::Cancelled
        };
        Ok(CancelResult {
            outcome,
            detail: Some("fake cancellation".to_owned()),
        })
    }

    async fn run_usage_probe(
        &self,
        invocation: PreparedInvocation,
    ) -> Result<RuntimeOutput, ProviderError> {
        self.ensure_fake(&invocation.executable)?;
        Ok(RuntimeOutput {
            resolved_executable: None,
            exit_code: Some(0),
            termination: RuntimeTermination::Exited,
            tree_termination_error: None,
            stdout: br#"{"used":25,"limit":100,"remaining":75,"confidence":"confirmed"}"#.to_vec(),
            stderr: Vec::new(),
            truncated: false,
        })
    }
}

fn fake_probe_output(args: &[String], codex_version: Option<&str>) -> String {
    if args == ["--version"] {
        let version = codex_version.unwrap_or("0.144.5");
        format!("codex-cli {version}\n")
    } else if args == ["exec", "--help"] {
        "--json --output-schema --sandbox read-only workspace-write -c model_reasoning_effort=[low|medium|high]\n".to_owned()
    } else if args == ["exec", "resume", "--help"] {
        "Usage: codex exec resume [SESSION_ID]\n".to_owned()
    } else if args == ["app-server", "--help"] {
        "--listen stdio:// generate-json-schema\n".to_owned()
    } else if args.first().is_some_and(|value| value == "app-server")
        && args
            .get(1)
            .is_some_and(|value| value == "generate-json-schema")
    {
        r#"{"definitions":{"initialize":{"method":"initialize"},"initialized":{"method":"initialized"},"threadStart":{"method":"thread/start","sandbox":["read-only","workspace-write"]},"threadResume":{"method":"thread/resume"},"turnStart":{"method":"turn/start"},"itemStarted":{"method":"item/started"},"itemCompleted":{"method":"item/completed"},"turnCompleted":{"method":"turn/completed","tokenUsage":{}}}}"#.to_owned()
    } else {
        "Commands: exec app-server\n--print --prompt --output-format stream-json --permission-mode plan acceptEdits --approval-mode auto_edit --resume --effort --mode accept-edits --sandbox --conversation\nRead a piped prompt from stdin.\n".to_owned()
    }
}

fn scenario_lines(provider: ProviderId, scenario: FakeRuntimeScenario) -> Vec<Vec<u8>> {
    if provider == ProviderId::Gemini {
        return gemini_scenario_lines(scenario);
    }
    let lines: Vec<&str> = match (provider, scenario) {
        (ProviderId::Codex, FakeRuntimeScenario::Success) => vec![
            r#"{"type":"thread.started","thread_id":"fake-codex-session"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"done"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}"#,
        ],
        (ProviderId::Codex, FakeRuntimeScenario::QuotaExceeded) => vec![
            r#"{"type":"error","code":"usage_limit_reached","message":"Monthly usage limit reached"}"#,
        ],
        (ProviderId::Codex, FakeRuntimeScenario::TerminalError) => {
            vec![r#"{"type":"error","code":"billing_error","message":"Credit balance is too low"}"#]
        }
        (ProviderId::Codex, FakeRuntimeScenario::MalformedOutput) => vec!["{not-json}"],
        (ProviderId::Codex, FakeRuntimeScenario::UnknownEvent) => {
            vec![r#"{"type":"turn.paused"}"#]
        }
        (ProviderId::Codex, FakeRuntimeScenario::SecretOutput) => vec![
            r#"{"type":"thread.started","thread_id":"fake-codex-session"}"#,
            r#"{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"api_key=supersecretvalue"}}"#,
            r#"{"type":"turn.completed","usage":{}}"#,
        ],
        (ProviderId::Claude, FakeRuntimeScenario::Success) => vec![
            r#"{"type":"system","subtype":"init","session_id":"fake-claude-session"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"result","is_error":false,"result":"done"}"#,
        ],
        (ProviderId::Claude, FakeRuntimeScenario::QuotaExceeded) => {
            vec![r#"{"type":"result","is_error":true,"result":"Monthly usage limit reached"}"#]
        }
        (ProviderId::Claude, FakeRuntimeScenario::TerminalError) => {
            vec![r#"{"type":"result","is_error":true,"result":"Credit balance is too low"}"#]
        }
        (ProviderId::Claude, FakeRuntimeScenario::MalformedOutput) => vec!["not-json"],
        (ProviderId::Claude, FakeRuntimeScenario::UnknownEvent) => {
            vec![r#"{"type":"new_optional_event","payload":1}"#]
        }
        (ProviderId::Claude, FakeRuntimeScenario::SecretOutput) => vec![
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"api_key=supersecretvalue"}]}}"#,
            r#"{"type":"result","is_error":false,"result":"done"}"#,
        ],
        (ProviderId::Gemini, _) => unreachable!("Gemini scenarios are handled above"),
        (ProviderId::Agy, FakeRuntimeScenario::Success) => vec!["done"],
        (ProviderId::Agy, FakeRuntimeScenario::QuotaExceeded) => vec!["Daily quota exceeded"],
        (ProviderId::Agy, FakeRuntimeScenario::TerminalError) => vec!["Credit balance is too low"],
        (ProviderId::Agy, FakeRuntimeScenario::MalformedOutput) => vec!["plain output"],
        (ProviderId::Agy, FakeRuntimeScenario::UnknownEvent) => vec!["optional output"],
        (ProviderId::Agy, FakeRuntimeScenario::SecretOutput) => {
            vec!["api_key=supersecretvalue"]
        }
        (
            _,
            FakeRuntimeScenario::ProcessCrash
            | FakeRuntimeScenario::DiagnosticNoise
            | FakeRuntimeScenario::Timeout
            | FakeRuntimeScenario::ConversationResponseAlias
            | FakeRuntimeScenario::ReadOnlyCommand
            | FakeRuntimeScenario::ReadOnlyCommandWithFileChange
            | FakeRuntimeScenario::AmbiguousScalarPrefix
            | FakeRuntimeScenario::GeminiDetailedErrorThenErrorResult
            | FakeRuntimeScenario::GeminiWarningBetweenDeltas
            | FakeRuntimeScenario::GeminiCompleteMessage,
        ) => Vec::new(),
    };
    lines
        .into_iter()
        .map(|line| line.as_bytes().to_vec())
        .collect()
}

fn gemini_scenario_lines(scenario: FakeRuntimeScenario) -> Vec<Vec<u8>> {
    match scenario {
        FakeRuntimeScenario::Success | FakeRuntimeScenario::SecretOutput => {
            let assistant = if scenario == FakeRuntimeScenario::Success {
                "done"
            } else {
                "api_key=supersecretvalue"
            };
            gemini_stream_lines(
                "fake-gemini-session",
                "exercise fake provider",
                assistant,
                Some(serde_json::json!({
                    "total_tokens": 21,
                    "input_tokens": 13,
                    "output_tokens": 8,
                    "cached": 2
                })),
            )
        }
        FakeRuntimeScenario::QuotaExceeded => vec![
            br#"{"type":"error","message":"Daily quota exceeded","timestamp":"2026-08-07T00:00:00Z"}"#.to_vec(),
        ],
        FakeRuntimeScenario::TerminalError => vec![
            br#"{"type":"error","message":"Credit balance is too low","timestamp":"2026-08-07T00:00:00Z"}"#.to_vec(),
        ],
        FakeRuntimeScenario::MalformedOutput => vec![b"not-json".to_vec()],
        FakeRuntimeScenario::UnknownEvent => vec![
            br#"{"type":"new_optional_event","payload":1,"timestamp":"2026-08-07T00:00:00Z"}"#.to_vec(),
        ],
        FakeRuntimeScenario::DiagnosticNoise => [0, 1, 2]
            .map(|second| {
                serde_json::to_vec(&serde_json::json!({
                    "type": "stderr",
                    "message": "provider diagnostic",
                    "timestamp": format!("2026-08-07T00:00:0{second}Z")
                }))
                .unwrap_or_default()
            })
            .to_vec(),
        FakeRuntimeScenario::GeminiDetailedErrorThenErrorResult => vec![
            br#"{"type":"error","severity":"error","message":"Authentication failed. Run `gemini auth login` and retry the request.","timestamp":"2026-08-07T00:00:00Z"}"#.to_vec(),
            br#"{"type":"result","status":"error","timestamp":"2026-08-07T00:00:01Z"}"#.to_vec(),
        ],
        FakeRuntimeScenario::GeminiWarningBetweenDeltas => gemini_stream_lines_with_warning(
            "fake-gemini-warning",
            "exercise fake provider",
            "done",
            None,
        ),
        FakeRuntimeScenario::GeminiCompleteMessage => gemini_complete_message_lines(
            "fake-gemini-complete",
            "exercise fake provider",
            "done",
        ),
        FakeRuntimeScenario::ProcessCrash
        | FakeRuntimeScenario::Timeout
        | FakeRuntimeScenario::ConversationResponseAlias
        | FakeRuntimeScenario::ReadOnlyCommand
        | FakeRuntimeScenario::ReadOnlyCommandWithFileChange
        | FakeRuntimeScenario::AmbiguousScalarPrefix => Vec::new(),
    }
}

fn codex_conversation_lines(text: &str, scenario: FakeRuntimeScenario) -> Vec<serde_json::Value> {
    let mut lines = vec![
        serde_json::json!({"type":"thread.started","thread_id":"fake-conversation"}),
        serde_json::json!({"type":"turn.started"}),
    ];
    if matches!(
        scenario,
        FakeRuntimeScenario::ReadOnlyCommand | FakeRuntimeScenario::ReadOnlyCommandWithFileChange
    ) {
        lines.push(serde_json::json!({
            "type": "item.completed",
            "item": {
                "id": "read-only-progress",
                "type": "agent_message",
                "text": "I'll verify that with a read-only command."
            }
        }));
        lines.push(serde_json::json!({
            "type": "item.started",
            "item": {
                "id": "read-only-command",
                "type": "command_execution",
                "command": "/bin/sh -lc pwd",
                "status": "in_progress"
            }
        }));
    }
    if scenario == FakeRuntimeScenario::ReadOnlyCommandWithFileChange {
        lines.push(serde_json::json!({
            "type": "item.started",
            "item": {
                "id": "unexpected-write",
                "type": "file_change",
                "path": "README.md",
                "status": "in_progress"
            }
        }));
    }
    if scenario == FakeRuntimeScenario::AmbiguousScalarPrefix {
        lines.extend([
            serde_json::json!({
                "type": "item.completed",
                "item": {
                    "id": "ambiguous-progress",
                    "type": "agent_message",
                    "text": "Checking the request."
                }
            }),
            serde_json::json!({
                "type": "item.completed",
                "item": {
                    "id": "ambiguous-scalar",
                    "type": "agent_message",
                    "text": "null"
                }
            }),
        ]);
    }
    lines.extend([
        serde_json::json!({"type":"item.completed","item":{"id":"m1","type":"agent_message","text":text}}),
        serde_json::json!({"type":"turn.completed","usage":{}}),
    ]);
    lines
}

fn conversation_lines(
    provider: ProviderId,
    prompt: &str,
    scenario: FakeRuntimeScenario,
) -> Vec<Vec<u8>> {
    let prompt: serde_json::Value = serde_json::from_str(prompt).unwrap_or_default();
    let transcript = prompt
        .get("transcript_redacted")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let text =
        serde_json::to_string(&conversation_outcome(transcript, scenario)).unwrap_or_default();
    let lines = match provider {
        ProviderId::Codex => codex_conversation_lines(&text, scenario),
        ProviderId::Claude => {
            let mut lines = vec![serde_json::json!({
                "type":"system","subtype":"init","session_id":"fake-conversation"
            })];
            if scenario == FakeRuntimeScenario::AmbiguousScalarPrefix {
                lines.extend([
                    serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"Checking the request."}]}}),
                    serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"null"}]}}),
                ]);
            }
            lines.extend([
                serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}}),
                serde_json::json!({"type":"result","is_error":false,"result":text}),
            ]);
            lines
        }
        ProviderId::Gemini => {
            let assistant = if scenario == FakeRuntimeScenario::AmbiguousScalarPrefix {
                format!("Checking the request.\nnull\n{text}")
            } else {
                text
            };
            if scenario == FakeRuntimeScenario::GeminiWarningBetweenDeltas {
                return gemini_stream_lines_with_warning(
                    "fake-conversation",
                    "echoed user request",
                    &assistant,
                    None,
                );
            }
            return gemini_stream_lines(
                "fake-conversation",
                "echoed user request",
                &assistant,
                None,
            );
        }
        ProviderId::Agy => {
            if scenario == FakeRuntimeScenario::AmbiguousScalarPrefix {
                return vec![
                    b"Checking the request.".to_vec(),
                    b"null".to_vec(),
                    text.into_bytes(),
                ];
            }
            return vec![text.into_bytes()];
        }
    };
    lines
        .into_iter()
        .map(|line| serde_json::to_vec(&line).unwrap_or_default())
        .collect()
}

fn gemini_delta_events(text: &str) -> Vec<serde_json::Value> {
    let split = text
        .char_indices()
        .nth(3)
        .map_or(text.len(), |(index, _)| index);
    [&text[..split], &text[split..]]
        .into_iter()
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| {
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": chunk,
                "delta": true,
                "timestamp": "2026-08-07T00:00:02Z"
            })
        })
        .collect()
}

fn gemini_stream_lines(
    session_id: &str,
    user_text: &str,
    assistant_text: &str,
    stats: Option<serde_json::Value>,
) -> Vec<Vec<u8>> {
    let mut values = vec![
        serde_json::json!({
            "type":"init",
            "session_id":session_id,
            "model":"fake-gemini",
            "timestamp":"2026-08-07T00:00:00Z"
        }),
        serde_json::json!({
            "type":"message",
            "role":"user",
            "content":user_text,
            "timestamp":"2026-08-07T00:00:01Z"
        }),
    ];
    values.extend(gemini_delta_events(assistant_text));
    let mut result = serde_json::json!({
        "type":"result",
        "status":"success",
        "timestamp":"2026-08-07T00:00:03Z"
    });
    if let Some(stats) = stats {
        result["stats"] = stats;
    }
    values.push(result);
    values
        .into_iter()
        .map(|value| serde_json::to_vec(&value).unwrap_or_default())
        .collect()
}

fn gemini_stream_lines_with_warning(
    session_id: &str,
    user_text: &str,
    assistant_text: &str,
    stats: Option<serde_json::Value>,
) -> Vec<Vec<u8>> {
    let mut lines = gemini_stream_lines(session_id, user_text, assistant_text, stats);
    let warning = serde_json::to_vec(&serde_json::json!({
        "type": "error",
        "severity": "warning",
        "message": "Loop detected, stopping execution",
        "timestamp": "2026-08-07T00:00:02.500Z"
    }))
    .unwrap_or_default();
    lines.insert(3.min(lines.len()), warning);
    lines
}

fn gemini_complete_message_lines(
    session_id: &str,
    user_text: &str,
    assistant_text: &str,
) -> Vec<Vec<u8>> {
    [
        serde_json::json!({
            "type":"init",
            "session_id":session_id,
            "model":"fake-gemini",
            "timestamp":"2026-08-07T00:00:00Z"
        }),
        serde_json::json!({
            "type":"message",
            "role":"user",
            "content":user_text,
            "timestamp":"2026-08-07T00:00:01Z"
        }),
        serde_json::json!({
            "type":"message",
            "role":"assistant",
            "content":assistant_text,
            "timestamp":"2026-08-07T00:00:02Z"
        }),
        serde_json::json!({
            "type":"result",
            "status":"success",
            "timestamp":"2026-08-07T00:00:03Z"
        }),
    ]
    .into_iter()
    .map(|value| serde_json::to_vec(&value).unwrap_or_default())
    .collect()
}

fn conversation_outcome(transcript: &str, scenario: FakeRuntimeScenario) -> serde_json::Value {
    if scenario == FakeRuntimeScenario::ConversationResponseAlias {
        serde_json::json!({
            "outcome": "answer_complete",
            "response": "Hello! How can I help?"
        })
    } else if transcript.contains("needs-info") {
        serde_json::json!({
            "outcome": "more_information_needed",
            "response_redacted": "Which crate and acceptance boundary should be used?",
            "requirements": {
                "objective": "clarify the requested change",
                "in_scope": ["requested change"],
                "out_of_scope": [],
                "constraints": ["no task before approval"],
                "acceptance_criteria": [],
                "verification_plan": [],
                "risks": [],
                "open_questions": ["Which crate should change?"]
            }
        })
    } else if transcript.contains("candidate") {
        serde_json::json!({
            "outcome": "worktree_task_candidate",
            "response_redacted": "The requirement is ready for deterministic validation.",
            "requirements": {
                "objective": "implement the approved candidate",
                "in_scope": ["approved candidate"],
                "out_of_scope": ["automatic merge or push"],
                "constraints": ["no task before approval"],
                "acceptance_criteria": ["fake integration test passes"],
                "verification_plan": [{
                    "executable": "cargo",
                    "args": ["test", "--workspace", "--all-features"]
                }],
                "risks": ["stale approval"],
                "open_questions": []
            }
        })
    } else if transcript.contains("attention") {
        serde_json::json!({
            "outcome": "needs_attention",
            "response_redacted": "The provider could not classify this turn.",
            "evidence_redacted": "fake attention fixture"
        })
    } else {
        serde_json::json!({
            "outcome": "answer_complete",
            "response_redacted": "Git is needed only after an approved writable task candidate."
        })
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn run_fake_cli<I>(args: I, codex_version: Option<&str>)
where
    I: IntoIterator<Item = OsString>,
{
    enforce_schema_guard();
    let args = args
        .into_iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let app_server_probe = args.first().is_some_and(|arg| arg == "app-server")
        && (args.iter().any(|arg| arg == "--help")
            || args.get(1).is_some_and(|arg| arg == "generate-json-schema"));
    if args.iter().any(|arg| arg == "--version")
        && let Some(delay) = fake_probe_delay()
    {
        mark_fake_probe_started();
        std::thread::sleep(Duration::from_millis(delay));
    }
    if args.iter().any(|arg| arg == "--version" || arg == "--help") || app_server_probe {
        print!("{}", fake_probe_output(&args, codex_version));
        return;
    }
    if args.first().is_some_and(|arg| arg == "app-server") {
        run_fake_app_server();
        return;
    }
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);
    let provider = if args.iter().any(|arg| arg == "exec") {
        ProviderId::Codex
    } else if args.iter().any(|arg| arg == "--permission-mode") {
        ProviderId::Claude
    } else if args.iter().any(|arg| arg == "--mode") && args.iter().any(|arg| arg == "--sandbox") {
        ProviderId::Agy
    } else {
        ProviderId::Gemini
    };
    let provider_input = if provider == ProviderId::Agy {
        if args.iter().any(|arg| arg == "--print") {
            let Some(prompt) = argument_value(&args, "--print") else {
                eprintln!("flag needs an argument: -print");
                std::process::exit(2);
            };
            prompt
        } else {
            &stdin
        }
    } else {
        &stdin
    };

    if let Some(prompt) = planning_prompt(provider_input) {
        emit_planner_fixture(provider, &args, &prompt);
        return;
    }
    if emit_conversation_fixture(provider, provider_input) {
        return;
    }
    if is_handover_acknowledgement(provider_input) {
        emit_handover_acknowledgement(provider, provider_input);
        return;
    }
    if provider == ProviderId::Agy
        && provider_input.contains("scenario:agy-overlong-redaction-boundary")
    {
        emit_agy_overlong_redaction_boundary();
        return;
    }

    if provider_input.contains("scenario:codex-quota") {
        if provider == ProviderId::Codex {
            write_partial_handover_fixture();
            for line in scenario_lines(provider, FakeRuntimeScenario::QuotaExceeded) {
                println!("{}", String::from_utf8_lossy(&line));
            }
            return;
        }
        if provider == ProviderId::Claude
            && args
                .windows(2)
                .any(|pair| pair[0] == "--permission-mode" && pair[1] == "acceptEdits")
        {
            write_completed_handover_fixture();
        }
    }
    let scenario = argument_value(&args, "--scenario").unwrap_or_else(|| {
        if provider_input.contains("scenario:quota") {
            "quota"
        } else if provider_input.contains("scenario:malformed") {
            "malformed"
        } else if provider_input.contains("scenario:timeout") {
            "timeout"
        } else if provider_input.contains("scenario:crash") {
            "crash"
        } else if provider_input.contains("scenario:unknown") {
            "unknown"
        } else if provider_input.contains("scenario:secret") {
            "secret"
        } else if provider_input.contains("scenario:gemini-complete-message") {
            "gemini-complete-message"
        } else {
            "success"
        }
    });
    if scenario == "timeout" {
        std::thread::sleep(Duration::from_mins(5));
        return;
    }
    if provider_input.contains("scenario:delayed-success") {
        std::thread::sleep(Duration::from_millis(1_200));
    }
    if scenario == "crash" {
        eprintln!("fake provider crash");
        std::process::exit(17);
    }
    if args.first().is_some_and(|arg| arg == "usage") {
        println!(r#"{{"used":25,"limit":100,"remaining":75,"confidence":"confirmed"}}"#);
        return;
    }
    let scenario = match scenario {
        "quota" => FakeRuntimeScenario::QuotaExceeded,
        "malformed" => FakeRuntimeScenario::MalformedOutput,
        "unknown" => FakeRuntimeScenario::UnknownEvent,
        "secret" => FakeRuntimeScenario::SecretOutput,
        "gemini-complete-message" => FakeRuntimeScenario::GeminiCompleteMessage,
        _ => FakeRuntimeScenario::Success,
    };
    for line in scenario_lines(provider, scenario) {
        println!("{}", String::from_utf8_lossy(&line));
    }
}

fn emit_agy_overlong_redaction_boundary() {
    const STREAM_FRAME_BYTES: usize = 1024 * 1024;
    const TOKEN_PREFIX: &[u8] = b"Bearer abcdefgh";
    const TOKEN_SUFFIX: &[u8] = b"ijklmnop";
    let padding = vec![b'x'; STREAM_FRAME_BYTES - TOKEN_PREFIX.len()];
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(&padding);
    let _ = stdout.write_all(TOKEN_PREFIX);
    let _ = stdout.write_all(TOKEN_SUFFIX);
    let _ = stdout.flush();
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaGuard {
    database: PathBuf,
    required_schema_version: u32,
    observation: PathBuf,
}

fn enforce_schema_guard() {
    let Some(guard_path) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("schema-guard.json")))
        .filter(|path| path.exists())
    else {
        return;
    };
    let guard = std::fs::read(&guard_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SchemaGuard>(&bytes).ok());
    let Some(guard) = guard else {
        eprintln!(
            "fake provider schema guard is invalid: {}",
            guard_path.display()
        );
        std::process::exit(86);
    };
    let observed = rusqlite::Connection::open_with_flags(
        &guard.database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .and_then(|connection| {
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
    });
    let observed_schema_version = observed.as_ref().ok().copied();
    let guard_passed = observed_schema_version == Some(guard.required_schema_version);
    let observation = serde_json::json!({
        "observed_schema_version": observed_schema_version,
        "required_schema_version": guard.required_schema_version,
        "guard_passed": guard_passed,
        "database": guard.database,
    });
    if let Some(parent) = guard.observation.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &guard.observation,
        serde_json::to_vec_pretty(&observation).unwrap_or_default(),
    );
    if !guard_passed {
        eprintln!(
            "fake provider observed schema {:?}; required {}",
            observed_schema_version, guard.required_schema_version
        );
        std::process::exit(86);
    }
}

fn fake_probe_delay() -> Option<u64> {
    let executable = std::env::current_exe().ok()?;
    let stem = executable.file_stem()?.to_string_lossy();
    stem.rsplit_once("probe-delay-")?.1.parse().ok()
}

fn mark_fake_probe_started() {
    let Ok(mut executable) = std::env::current_exe() else {
        return;
    };
    let Some(file_name) = executable.file_name() else {
        return;
    };
    let mut marker_name = file_name.to_os_string();
    marker_name.push(".probe-started");
    executable.set_file_name(marker_name);
    let _ = std::fs::write(executable, b"started");
}

fn planning_prompt(stdin: &str) -> Option<serde_json::Value> {
    let bridge: serde_json::Value = serde_json::from_str(stdin).ok()?;
    if bridge.get("objective")?.as_str()? != "Propose a read-only task graph" {
        return None;
    }
    serde_json::from_str(bridge.get("task")?.as_str()?).ok()
}

fn conversation_prompt(stdin: &str) -> Option<String> {
    let bridge: serde_json::Value = serde_json::from_str(stdin).ok()?;
    if bridge.get("objective")?.as_str()? != "Conduct a read-only conversation turn" {
        return None;
    }
    bridge.get("task")?.as_str().map(ToOwned::to_owned)
}

fn emit_conversation_fixture(provider: ProviderId, stdin: &str) -> bool {
    let Some(prompt) = conversation_prompt(stdin) else {
        return false;
    };
    mark_fake_conversation_started();
    if prompt.contains("scenario:timeout") {
        std::thread::sleep(Duration::from_mins(5));
        return true;
    }
    if prompt.contains("scenario:crash") {
        eprintln!("fake conversation provider crash");
        std::process::exit(17);
    }
    let scenario = if prompt.contains("scenario:ambiguous-scalar-prefix") {
        FakeRuntimeScenario::AmbiguousScalarPrefix
    } else if prompt.contains("scenario:gemini-warning-between-deltas") {
        FakeRuntimeScenario::GeminiWarningBetweenDeltas
    } else if prompt.contains("scenario:read-only-command-file-change") {
        FakeRuntimeScenario::ReadOnlyCommandWithFileChange
    } else if prompt.contains("scenario:read-only-command") {
        FakeRuntimeScenario::ReadOnlyCommand
    } else {
        FakeRuntimeScenario::Success
    };
    for line in conversation_lines(provider, &prompt, scenario) {
        println!("{}", String::from_utf8_lossy(&line));
    }
    true
}

fn mark_fake_conversation_started() {
    let marker_path = std::env::var_os("COLAY_TEST_FAKE_CONVERSATION_MARKER")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("TEMP")
                .or_else(|| std::env::var_os("TMP"))
                .map(PathBuf::from)
                .map(|directory| directory.join("colay-fake-conversation-starts.json"))
        });
    let Some(marker_path) = marker_path else {
        return;
    };
    let invocation_count = std::fs::read(&marker_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.get("invocation_count")?.as_u64())
        .unwrap_or_default()
        .saturating_add(1);
    let marker = serde_json::json!({ "invocation_count": invocation_count });
    if let Some(parent) = marker_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        marker_path,
        serde_json::to_vec_pretty(&marker).unwrap_or_default(),
    );
}

fn emit_planner_fixture(provider: ProviderId, args: &[String], prompt: &serde_json::Value) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let log_path = cwd.join(".colay/fake-planner-invocation.json");
    let invocation_count = std::fs::read(&log_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| {
            value
                .get("invocation_count")
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or_default()
        .saturating_add(1);
    let log = serde_json::json!({
        "args": args,
        "cwd": cwd,
        "timeout_seconds": prompt.get("timeout_seconds"),
        "stdout_limit": prompt.get("stdout_limit"),
        "invocation_count": invocation_count,
    });
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &log_path,
        serde_json::to_vec_pretty(&log).unwrap_or_default(),
    );

    let goal = prompt
        .get("goal_redacted")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let text = if goal.contains("scenario:malformed") {
        "not-json".to_owned()
    } else {
        serde_json::json!({
            "schema_version": "1",
            "revision_id": prompt.get("revision_id"),
            "session_id": prompt.get("session_id"),
            "goal_message_id": prompt.get("goal_message_id"),
            "planner_provider": prompt.get("planner_provider"),
            "proposed_at": Utc::now(),
            "nodes": [
                {
                    "key": "domain",
                    "title": "Domain contract",
                    "objective": "Implement domain contract",
                    "dependencies": [],
                    "constraints": ["local only"],
                    "acceptance_criteria": ["domain tests pass"],
                    "provider": prompt.get("planner_provider"),
                    "profile": "standard",
                    "write_scopes": ["crates/orchestrator-domain"],
                    "repository_wide_write_scope": false,
                    "risks": ["concurrency"],
                    "parallel_safety": "isolated domain scope"
                },
                {
                    "key": "tui",
                    "title": "TUI integration",
                    "objective": "Render the approved plan",
                    "dependencies": ["domain"],
                    "constraints": ["text only"],
                    "acceptance_criteria": ["TUI tests pass"],
                    "provider": prompt.get("planner_provider"),
                    "profile": "standard",
                    "write_scopes": ["crates/orchestrator-tui"],
                    "repository_wide_write_scope": false,
                    "risks": [],
                    "parallel_safety": "runs after domain"
                }
            ]
        })
        .to_string()
    };
    let include_read_only_command = goal.contains("scenario:read-only-command");
    for line in planner_lines(provider, &text, include_read_only_command) {
        println!("{}", String::from_utf8_lossy(&line));
    }
}

fn planner_lines(
    provider: ProviderId,
    text: &str,
    include_read_only_command: bool,
) -> Vec<Vec<u8>> {
    if provider == ProviderId::Agy {
        return vec![text.as_bytes().to_vec()];
    }
    let values = match provider {
        ProviderId::Codex => {
            let mut values = vec![
                serde_json::json!({"type":"thread.started","thread_id":"fake-planner"}),
                serde_json::json!({"type":"turn.started"}),
            ];
            if include_read_only_command {
                values.push(serde_json::json!({
                    "type": "item.started",
                    "item": {
                        "id": "inspect-repository",
                        "type": "command_execution",
                        "command": "rg --files",
                        "status": "in_progress"
                    }
                }));
            }
            values.extend([
                serde_json::json!({"type":"item.completed","item":{"id":"plan","type":"agent_message","text":text}}),
                serde_json::json!({"type":"turn.completed","usage":{}}),
            ]);
            values
        }
        ProviderId::Claude => vec![
            serde_json::json!({"type":"system","subtype":"init","session_id":"fake-planner"}),
            serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}}),
            serde_json::json!({"type":"result","is_error":false,"result":text}),
        ],
        ProviderId::Gemini => {
            return gemini_stream_lines("fake-planner", "planning request", text, None);
        }
        ProviderId::Agy => unreachable!("Agy plain text is handled above"),
    };
    values
        .into_iter()
        .map(|value| serde_json::to_vec(&value).unwrap_or_default())
        .collect()
}

fn is_handover_acknowledgement(stdin: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(stdin)
        .ok()
        .and_then(|payload| {
            payload
                .get("objective")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|objective| objective == "Acknowledge a sealed vendor-neutral handover")
}

fn emit_handover_acknowledgement(provider: ProviderId, stdin: &str) {
    for line in handover_acknowledgement_lines(provider, stdin) {
        println!("{}", String::from_utf8_lossy(&line));
    }
}

fn handover_acknowledgement_lines(provider: ProviderId, stdin: &str) -> Vec<Vec<u8>> {
    let Some(bundle) = serde_json::from_str::<serde_json::Value>(stdin)
        .ok()
        .and_then(|payload| payload.get("handover").cloned())
    else {
        return Vec::new();
    };
    let acknowledgement = serde_json::json!({
        "type": "handover_ack",
        "bundle_hash": bundle.get("integrity_hash"),
        "can_resume": true,
        "understood_objective": bundle.get("objective"),
        "understood_constraints": bundle.get("constraints"),
        "understood_acceptance_criteria": bundle.get("acceptance_criteria"),
        "unresolved_questions": bundle.get("unresolved_questions"),
    })
    .to_string();
    let values = match provider {
        ProviderId::Agy => return vec![acknowledgement.into_bytes()],
        ProviderId::Claude => vec![
            serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": acknowledgement}]}
            }),
            serde_json::json!({"type": "result", "is_error": false, "result": "acknowledged"}),
        ],
        ProviderId::Gemini => {
            return gemini_stream_lines(
                "fake-handover",
                "handover acknowledgement request",
                &acknowledgement,
                None,
            );
        }
        ProviderId::Codex => vec![
            serde_json::json!({
                "type": "item.completed",
                "item": {"id": "ack", "type": "agent_message", "text": acknowledgement}
            }),
            serde_json::json!({"type": "turn.completed", "usage": {}}),
        ],
    };
    values
        .into_iter()
        .map(|value| serde_json::to_vec(&value).unwrap_or_default())
        .collect()
}

fn write_partial_handover_fixture() {
    let path = Path::new("src").join("partial.txt");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, b"partial work preserved across handover\n");
}

fn write_completed_handover_fixture() {
    let path = Path::new("src").join("lib.rs");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        path,
        b"pub fn answer() -> u32 {\n    42\n}\n\n#[cfg(test)]\nmod tests {\n    use super::answer;\n\n    #[test]\n    fn returns_answer() {\n        assert_eq!(answer(), 42);\n    }\n}\n",
    );
}

#[allow(clippy::too_many_lines)]
fn run_fake_app_server() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let method = message.get("method").and_then(serde_json::Value::as_str);
        let id = message
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match method {
            Some("initialize") => write_fake_message(
                &mut stdout,
                &serde_json::json!({"id": id, "result": {"userAgent": "fake"}}),
            ),
            Some("thread/start" | "thread/resume") => {
                if message
                    .pointer("/params/model")
                    .and_then(serde_json::Value::as_str)
                    == Some("fake-appserver-pre-turn-protocol-error")
                {
                    let _ = writeln!(stdout, "{{not-json}}");
                    let _ = stdout.flush();
                    continue;
                }
                write_fake_message(
                    &mut stdout,
                    &serde_json::json!({
                        "id": id,
                        "result": {"thread": {"id": "fake-codex-session"}}
                    }),
                );
                write_fake_message(
                    &mut stdout,
                    &serde_json::json!({
                        "method": "thread/started",
                        "params": {"thread": {"id": "fake-codex-session"}}
                    }),
                );
            }
            Some("turn/start") => {
                let prompt = message
                    .pointer("/params/input/0/text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if prompt.contains("scenario:appserver-awaiting-turn-protocol-error") {
                    let _ = writeln!(stdout, "{{not-json}}");
                    let _ = stdout.flush();
                    continue;
                }
                write_fake_message(
                    &mut stdout,
                    &serde_json::json!({
                        "id": id,
                        "result": {"turn": {"id": "fake-turn"}}
                    }),
                );
                if prompt.contains("scenario:appserver-protocol-error") {
                    let _ = writeln!(stdout, "{{not-json}}");
                    let _ = stdout.flush();
                    continue;
                }
                write_fake_message(
                    &mut stdout,
                    &serde_json::json!({
                        "method": "turn/started",
                        "params": {"turn": {"id": "fake-turn", "items": [], "status": "inProgress"}}
                    }),
                );
                let messages = if prompt.contains("scenario:appserver-split-private-key") {
                    vec![
                        "-----BEGIN PRIVATE KEY-----\nprivate-part-a",
                        "private-part-b\n-----END PRIVATE KEY-----",
                    ]
                } else if prompt.contains("scenario:secret") {
                    vec!["api_key=supersecretvalue"]
                } else {
                    vec!["done"]
                };
                for (index, text) in messages.into_iter().enumerate() {
                    write_fake_message(
                        &mut stdout,
                        &serde_json::json!({
                            "method": "item/completed",
                            "params": {
                                "threadId": "fake-codex-session",
                                "turnId": "fake-turn",
                                "item": {
                                    "id": format!("m{}", index + 1),
                                    "type": "agentMessage",
                                    "text": text
                                }
                            }
                        }),
                    );
                }
                write_fake_message(
                    &mut stdout,
                    &serde_json::json!({
                        "method": "thread/tokenUsage/updated",
                        "params": {
                            "threadId": "fake-codex-session",
                            "turnId": "fake-turn",
                            "tokenUsage": {
                                "last": {
                                    "inputTokens": 10,
                                    "cachedInputTokens": 0,
                                    "outputTokens": 2,
                                    "reasoningOutputTokens": 0,
                                    "totalTokens": 12
                                },
                                "total": {
                                    "inputTokens": 10,
                                    "cachedInputTokens": 0,
                                    "outputTokens": 2,
                                    "reasoningOutputTokens": 0,
                                    "totalTokens": 12
                                }
                            }
                        }
                    }),
                );
                write_fake_message(
                    &mut stdout,
                    &serde_json::json!({
                        "method": "turn/completed",
                        "params": {
                            "threadId": "fake-codex-session",
                            "turn": {"id": "fake-turn", "items": [], "status": "completed"}
                        }
                    }),
                );
            }
            _ => {}
        }
    }
}

fn write_fake_message(writer: &mut impl std::io::Write, message: &serde_json::Value) {
    let _ = serde_json::to_writer(&mut *writer, message);
    let _ = writer.write_all(b"\n");
    let _ = writer.flush();
}

fn argument_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().enumerate().find_map(|(index, argument)| {
        if argument == name {
            args.get(index + 1).map(String::as_str)
        } else {
            argument
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agy_handover_acknowledgement_is_plain_text_json() -> Result<(), serde_json::Error> {
        let stdin = serde_json::json!({
            "handover": {
                "integrity_hash": "sealed-hash",
                "objective": "continue safely",
                "constraints": ["preserve work"],
                "acceptance_criteria": ["tests pass"],
                "unresolved_questions": []
            }
        })
        .to_string();

        let lines = handover_acknowledgement_lines(ProviderId::Agy, &stdin);

        assert_eq!(lines.len(), 1);
        let acknowledgement: serde_json::Value = serde_json::from_slice(&lines[0])?;
        assert_eq!(acknowledgement["type"], "handover_ack");
        assert_eq!(acknowledgement["bundle_hash"], "sealed-hash");
        Ok(())
    }

    #[test]
    fn rejects_real_provider_names() {
        let runtime = FakeAdapterRuntime::new("codex", FakeRuntimeScenario::Success);
        assert!(runtime.is_err());
    }

    #[test]
    fn rejects_missing_fake_binary() {
        let runtime =
            FakeAdapterRuntime::new("missing/fake-provider-cli", FakeRuntimeScenario::Success);
        assert!(runtime.is_err());
    }
}
