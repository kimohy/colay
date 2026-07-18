use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use orchestrator_domain::{
    AttemptId, CancelOutcome, CancelResult, ProviderId, RawEvent, RawEventChannel,
    UntrustedWorkerClaim, WorkerHandle, WorkerRequest,
};
use orchestrator_providers::{
    AdapterRuntime, PreparedInvocation, ProviderError, RuntimeOutput, RuntimeTermination,
};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeRuntimeScenario {
    Success,
    QuotaExceeded,
    MalformedOutput,
    UnknownEvent,
    ProcessCrash,
    Timeout,
    SecretOutput,
}

#[derive(Debug)]
struct FakeJob {
    provider: ProviderId,
    events: VecDeque<RawEvent>,
    output: RuntimeOutput,
    cancelled: bool,
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
        let stdout = fake_probe_output(&args).into_bytes();
        Ok(RuntimeOutput {
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
        let lines = scenario_lines(provider, self.scenario);
        let events = lines
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| RawEvent {
                channel: RawEventChannel::Stdout,
                sequence: u64::try_from(index + 1).unwrap_or(u64::MAX),
                bytes,
                received_at: Utc::now(),
            })
            .collect::<VecDeque<_>>();
        let output = RuntimeOutput {
            exit_code: match self.scenario {
                FakeRuntimeScenario::ProcessCrash => Some(17),
                FakeRuntimeScenario::Timeout => None,
                _ => Some(0),
            },
            termination: match self.scenario {
                FakeRuntimeScenario::Timeout => RuntimeTermination::TimedOut,
                _ => RuntimeTermination::Exited,
            },
            tree_termination_error: None,
            stdout: Vec::new(),
            stderr: if self.scenario == FakeRuntimeScenario::ProcessCrash {
                b"fake process crash".to_vec()
            } else {
                Vec::new()
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
        let mut jobs = self.jobs.lock().await;
        let job = jobs
            .get_mut(&handle.attempt_id)
            .ok_or_else(|| ProviderError::Runtime("unknown fake worker".to_owned()))?;
        Ok(job.events.pop_front())
    }

    async fn wait(&self, handle: &WorkerHandle) -> Result<RuntimeOutput, ProviderError> {
        let jobs = self.jobs.lock().await;
        let job = jobs
            .get(&handle.attempt_id)
            .ok_or_else(|| ProviderError::Runtime("unknown fake worker".to_owned()))?;
        if job.cancelled {
            return Ok(RuntimeOutput {
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
            exit_code: Some(0),
            termination: RuntimeTermination::Exited,
            tree_termination_error: None,
            stdout: br#"{"used":25,"limit":100,"remaining":75,"confidence":"confirmed"}"#.to_vec(),
            stderr: Vec::new(),
            truncated: false,
        })
    }
}

fn fake_probe_output(args: &[String]) -> String {
    if args == ["--version"] {
        "codex-cli 0.144.5\n".to_owned()
    } else if args == ["exec", "--help"] {
        "--json --output-schema --sandbox read-only workspace-write\n".to_owned()
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
        "Commands: exec app-server\n--print --prompt --output-format stream-json --permission-mode plan acceptEdits --approval-mode auto_edit --resume --effort\n".to_owned()
    }
}

fn scenario_lines(provider: ProviderId, scenario: FakeRuntimeScenario) -> Vec<Vec<u8>> {
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
        (ProviderId::Claude | ProviderId::Gemini, FakeRuntimeScenario::MalformedOutput) => {
            vec!["not-json"]
        }
        (ProviderId::Gemini, FakeRuntimeScenario::Success) => vec![
            r#"{"type":"init","session_id":"fake-gemini-session"}"#,
            r#"{"type":"message","role":"assistant","content":"done"}"#,
            r#"{"type":"result","result":"done"}"#,
        ],
        (ProviderId::Gemini, FakeRuntimeScenario::QuotaExceeded) => {
            vec![r#"{"type":"error","message":"Daily quota exceeded"}"#]
        }
        (ProviderId::Claude | ProviderId::Gemini, FakeRuntimeScenario::UnknownEvent) => {
            vec![r#"{"type":"new_optional_event","payload":1}"#]
        }
        (ProviderId::Claude, FakeRuntimeScenario::SecretOutput) => vec![
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"api_key=supersecretvalue"}]}}"#,
            r#"{"type":"result","is_error":false,"result":"done"}"#,
        ],
        (ProviderId::Gemini, FakeRuntimeScenario::SecretOutput) => vec![
            r#"{"type":"message","role":"assistant","content":"api_key=supersecretvalue"}"#,
            r#"{"type":"result","result":"done"}"#,
        ],
        (_, FakeRuntimeScenario::ProcessCrash | FakeRuntimeScenario::Timeout) => Vec::new(),
    };
    lines
        .into_iter()
        .map(|line| line.as_bytes().to_vec())
        .collect()
}

pub(crate) fn run_fake_cli<I>(args: I)
where
    I: IntoIterator<Item = OsString>,
{
    let args = args
        .into_iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let app_server_probe = args.first().is_some_and(|arg| arg == "app-server")
        && (args.iter().any(|arg| arg == "--help")
            || args.get(1).is_some_and(|arg| arg == "generate-json-schema"));
    if args.iter().any(|arg| arg == "--version" || arg == "--help") || app_server_probe {
        print!("{}", fake_probe_output(&args));
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
    } else {
        ProviderId::Gemini
    };

    if stdin.contains("scenario:codex-quota") {
        if provider == ProviderId::Codex {
            write_partial_handover_fixture();
            for line in scenario_lines(provider, FakeRuntimeScenario::QuotaExceeded) {
                println!("{}", String::from_utf8_lossy(&line));
            }
            return;
        }
        if is_handover_acknowledgement(&stdin) {
            emit_handover_acknowledgement(provider, &stdin);
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
        if stdin.contains("scenario:quota") {
            "quota"
        } else if stdin.contains("scenario:malformed") {
            "malformed"
        } else if stdin.contains("scenario:timeout") {
            "timeout"
        } else if stdin.contains("scenario:crash") {
            "crash"
        } else if stdin.contains("scenario:unknown") {
            "unknown"
        } else if stdin.contains("scenario:secret") {
            "secret"
        } else {
            "success"
        }
    });
    if scenario == "timeout" {
        std::thread::sleep(Duration::from_mins(5));
        return;
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
        _ => FakeRuntimeScenario::Success,
    };
    for line in scenario_lines(provider, scenario) {
        println!("{}", String::from_utf8_lossy(&line));
    }
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
    let Some(bundle) = serde_json::from_str::<serde_json::Value>(stdin)
        .ok()
        .and_then(|payload| payload.get("handover").cloned())
    else {
        return;
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
    match provider {
        ProviderId::Claude => {
            println!(
                "{}",
                serde_json::json!({
                    "type": "assistant",
                    "message": {"content": [{"type": "text", "text": acknowledgement}]}
                })
            );
            println!(
                "{}",
                serde_json::json!({"type": "result", "is_error": false, "result": "acknowledged"})
            );
        }
        ProviderId::Gemini => {
            println!(
                "{}",
                serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": acknowledgement
                })
            );
            println!(
                "{}",
                serde_json::json!({"type": "result", "result": "acknowledged"})
            );
        }
        ProviderId::Codex => {
            println!(
                "{}",
                serde_json::json!({
                    "type": "item.completed",
                    "item": {"id": "ack", "type": "agent_message", "text": acknowledgement}
                })
            );
            println!(
                "{}",
                serde_json::json!({"type": "turn.completed", "usage": {}})
            );
        }
    }
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
                let text = if prompt.contains("scenario:secret") {
                    "api_key=supersecretvalue"
                } else {
                    "done"
                };
                write_fake_message(
                    &mut stdout,
                    &serde_json::json!({
                        "method": "item/completed",
                        "params": {
                            "threadId": "fake-codex-session",
                            "turnId": "fake-turn",
                            "item": {"id": "m1", "type": "agentMessage", "text": text}
                        }
                    }),
                );
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
