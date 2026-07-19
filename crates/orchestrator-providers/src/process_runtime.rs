use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use codex_compat::{AppServerError, StableAppServerSession};
use orchestrator_domain::{
    AttemptId, CancelOutcome, CancelResult, ProviderId, RawEvent, RawEventChannel,
    UntrustedWorkerClaim, WorkerHandle, WorkerRequest,
};
use orchestrator_process::{
    CommandSpec, OutputChannel, ProcessEvent, ProcessRunner, ProcessSession, ProcessSupervisor,
    RedactionConfig, Redactor,
};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    AdapterRuntime, PreparedInvocation, ProviderError, RuntimeOutput, RuntimeTermination,
    StructuredOutput,
};

struct ProcessJob {
    provider: ProviderId,
    process_id: Mutex<Option<u32>>,
    cancellation: CancellationToken,
    events: Mutex<mpsc::Receiver<RawEvent>>,
    dropped_frames: AtomicU64,
    event_sequence: AtomicU64,
    result: Mutex<Option<Result<RuntimeOutput, String>>>,
    completion: Notify,
}

/// Production adapter runtime backed by the hardened process supervisor.
#[derive(Clone)]
pub struct ProcessAdapterRuntime {
    supervisor: ProcessSupervisor,
    runner: ProcessRunner,
    redaction: RedactionConfig,
    jobs: Arc<Mutex<HashMap<AttemptId, Arc<ProcessJob>>>>,
}

impl ProcessAdapterRuntime {
    #[must_use]
    pub fn new(redaction: RedactionConfig) -> Self {
        Self {
            supervisor: ProcessSupervisor,
            runner: ProcessRunner,
            redaction,
            jobs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn job(&self, handle: &WorkerHandle) -> Result<Arc<ProcessJob>, ProviderError> {
        let job = self
            .jobs
            .lock()
            .await
            .get(&handle.attempt_id)
            .cloned()
            .ok_or_else(|| ProviderError::Runtime("unknown worker handle".to_owned()))?;
        if job.provider != handle.provider {
            return Err(ProviderError::Runtime(
                "worker handle provider does not match its process lease".to_owned(),
            ));
        }
        Ok(job)
    }

    async fn run_bounded(&self, spec: CommandSpec) -> Result<RuntimeOutput, ProviderError> {
        let result = self
            .runner
            .run(spec, CancellationToken::new())
            .await
            .map_err(|error| ProviderError::Runtime(error.to_string()))?;
        Ok(process_output(&result))
    }

    async fn start_session(
        &self,
        invocation: &PreparedInvocation,
    ) -> Result<ProcessSession, ProviderError> {
        self.supervisor
            .start(command_spec(invocation, &self.redaction))
            .await
            .map_err(|error| ProviderError::Runtime(error.to_string()))
    }
}

impl Default for ProcessAdapterRuntime {
    fn default() -> Self {
        Self::new(RedactionConfig::default())
    }
}

#[async_trait]
impl AdapterRuntime for ProcessAdapterRuntime {
    async fn run_probe(
        &self,
        executable: &Path,
        args: &[OsString],
    ) -> Result<RuntimeOutput, ProviderError> {
        validate_safe_probe(args)?;
        let mut spec = CommandSpec::new(executable).args(args.iter().cloned());
        spec.timeout = Duration::from_secs(30);
        spec.stdout_limit = 4 * 1024 * 1024;
        spec.stderr_limit = 1024 * 1024;
        spec.redaction = self.redaction.clone();
        self.run_bounded(spec).await
    }

    async fn start_worker(
        &self,
        provider: ProviderId,
        request: &WorkerRequest,
        invocation: PreparedInvocation,
    ) -> Result<WorkerHandle, ProviderError> {
        enforce_fake_provider_only(&invocation.executable)?;
        invocation.validate()?;
        let mut primary = invocation;
        let fallback = primary.fallback.take().map(|fallback| *fallback);
        let (active, session, remaining_fallback, startup_fallback_reason) =
            match self.start_session(&primary).await {
                Ok(session) => (primary, session, fallback, None),
                Err(primary_error) => {
                    let Some(fallback) = fallback else {
                        return Err(primary_error);
                    };
                    let reason = primary_error.to_string();
                    let session = self.start_session(&fallback).await?;
                    (fallback, session, None, Some(reason))
                }
            };
        let process_id = session.process_id();
        let (event_sender, event_receiver) = mpsc::channel(64);
        let job = Arc::new(ProcessJob {
            provider,
            process_id: Mutex::new(process_id),
            cancellation: CancellationToken::new(),
            events: Mutex::new(event_receiver),
            dropped_frames: AtomicU64::new(0),
            event_sequence: AtomicU64::new(1),
            result: Mutex::new(None),
            completion: Notify::new(),
        });
        self.jobs
            .lock()
            .await
            .insert(request.attempt_id, Arc::clone(&job));

        let supervisor = self.supervisor;
        let redaction = self.redaction.clone();
        tokio::spawn(async move {
            let redactor = Redactor::new(&redaction).map_err(|error| error.to_string());
            let result = match redactor {
                Ok(redactor) => {
                    if let Some(reason) = startup_fallback_reason {
                        emit_fallback_event(&job, &event_sender, &redactor, &reason);
                    }
                    run_worker_chain(
                        supervisor,
                        active,
                        session,
                        remaining_fallback,
                        &job,
                        &event_sender,
                        &redactor,
                        &redaction,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            drop(event_sender);
            *job.result.lock().await = Some(result);
            job.completion.notify_waiters();
        });
        Ok(WorkerHandle {
            attempt_id: request.attempt_id,
            provider,
            process_id,
            session_id: None,
        })
    }

    async fn next_event(&self, handle: &WorkerHandle) -> Result<Option<RawEvent>, ProviderError> {
        let job = self.job(handle).await?;
        let dropped = job.dropped_frames.swap(0, Ordering::AcqRel);
        if dropped > 0 {
            return Ok(Some(RawEvent {
                channel: RawEventChannel::Protocol,
                sequence: u64::MAX,
                bytes: serde_json::to_vec(&serde_json::json!({
                    "type": "orchestrator.frames_dropped",
                    "count": dropped,
                }))
                .unwrap_or_default(),
                received_at: Utc::now(),
            }));
        }
        Ok(job.events.lock().await.recv().await)
    }

    async fn wait(&self, handle: &WorkerHandle) -> Result<RuntimeOutput, ProviderError> {
        let job = self.job(handle).await?;
        loop {
            let notified = job.completion.notified();
            if let Some(result) = job.result.lock().await.clone() {
                self.jobs.lock().await.remove(&handle.attempt_id);
                return result.map_err(ProviderError::Runtime);
            }
            notified.await;
        }
    }

    async fn checkpoint(
        &self,
        handle: &WorkerHandle,
    ) -> Result<UntrustedWorkerClaim, ProviderError> {
        let job = self.job(handle).await?;
        let finished = job.result.lock().await.is_some();
        Ok(UntrustedWorkerClaim {
            provider: job.provider,
            summary: if finished {
                "provider process finished; reconcile output with Git evidence"
            } else {
                "provider process is active; checkpoint only at a safe boundary"
            }
            .to_owned(),
            claimed_files_changed: Vec::new(),
            claimed_tests_passed: Vec::new(),
        })
    }

    async fn cancel(&self, handle: &WorkerHandle) -> Result<CancelResult, ProviderError> {
        let job = self.job(handle).await?;
        let outcome = if job.result.lock().await.is_some() {
            CancelOutcome::AlreadyExited
        } else {
            job.cancellation.cancel();
            CancelOutcome::Cancelled
        };
        Ok(CancelResult {
            outcome,
            detail: Some(format!("process_id={:?}", *job.process_id.lock().await)),
        })
    }

    async fn run_usage_probe(
        &self,
        invocation: PreparedInvocation,
    ) -> Result<RuntimeOutput, ProviderError> {
        invocation.validate()?;
        self.run_bounded(command_spec(&invocation, &self.redaction))
            .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_worker_chain(
    supervisor: ProcessSupervisor,
    active: PreparedInvocation,
    session: ProcessSession,
    fallback: Option<PreparedInvocation>,
    job: &Arc<ProcessJob>,
    event_sender: &mpsc::Sender<RawEvent>,
    redactor: &Redactor,
    redaction: &RedactionConfig,
) -> Result<RuntimeOutput, String> {
    if active.output != StructuredOutput::CodexAppServerStdio {
        return drive_static(session, job, event_sender).await;
    }

    match drive_app_server(session, &active, job, event_sender, redactor).await {
        AppServerDriveResult::Completed(output) => Ok(output),
        AppServerDriveResult::Failed(failure) => {
            let Some(mut fallback) = fallback.filter(|_| failure.safe_to_fallback) else {
                emit_protocol_error(job, event_sender, redactor, &failure.message);
                return failure.output.ok_or(failure.message);
            };
            fallback.fallback = None;
            emit_fallback_event(job, event_sender, redactor, &failure.message);
            let session = supervisor
                .start(command_spec(&fallback, redaction))
                .await
                .map_err(|error| format!("exec fallback failed to start: {error}"))?;
            *job.process_id.lock().await = session.process_id();
            drive_static(session, job, event_sender).await
        }
    }
}

async fn drive_static(
    mut session: ProcessSession,
    job: &Arc<ProcessJob>,
    event_sender: &mpsc::Sender<RawEvent>,
) -> Result<RuntimeOutput, String> {
    let mut cancellation_requested = false;
    loop {
        tokio::select! {
            () = job.cancellation.cancelled(), if !cancellation_requested => {
                cancellation_requested = true;
                session.cancel();
            }
            event = session.next_event() => {
                let Some(event) = event else {
                    break;
                };
                let exited = matches!(event, ProcessEvent::Exited { .. });
                match event {
                    ProcessEvent::FramesDropped { count } => add_dropped(job, count),
                    other => {
                        if let Some(raw) = raw_process_event(job, other) {
                            try_send(job, event_sender, raw);
                        }
                    }
                }
                if exited {
                    break;
                }
            }
        }
    }
    session
        .wait()
        .await
        .map(|result| process_output(&result))
        .map_err(|error| error.to_string())
}

struct AppServerFailure {
    message: String,
    safe_to_fallback: bool,
    output: Option<RuntimeOutput>,
}

enum AppServerDriveResult {
    Completed(RuntimeOutput),
    Failed(AppServerFailure),
}

#[allow(clippy::too_many_lines)]
async fn drive_app_server(
    mut process: ProcessSession,
    invocation: &PreparedInvocation,
    job: &Arc<ProcessJob>,
    event_sender: &mpsc::Sender<RawEvent>,
    redactor: &Redactor,
) -> AppServerDriveResult {
    let Some(plan) = invocation.codex_app_server.clone() else {
        return app_server_failure(process, "missing App Server session plan".to_owned(), true)
            .await;
    };
    let Some(input) = process.input() else {
        return app_server_failure(process, "live stdin is unavailable".to_owned(), true).await;
    };
    let mut protocol = match StableAppServerSession::new(plan) {
        Ok(protocol) => protocol,
        Err(error) => {
            return app_server_failure(process, error.to_string(), true).await;
        }
    };
    let initialize = match protocol.start() {
        Ok(frame) => frame,
        Err(error) => {
            return app_server_failure(process, error.to_string(), protocol.can_fallback()).await;
        }
    };
    if let Err(error) = input.write_all(&initialize).await {
        return app_server_failure(process, error.to_string(), protocol.can_fallback()).await;
    }

    loop {
        tokio::select! {
            () = job.cancellation.cancelled() => {
                return app_server_failure(process, "worker cancelled".to_owned(), false).await;
            }
            event = process.next_event() => {
                let Some(event) = event else {
                    return app_server_failure(
                        process,
                        "App Server event stream closed before turn completion".to_owned(),
                        protocol.can_fallback(),
                    ).await;
                };
                match event {
                    ProcessEvent::Output {
                        channel: OutputChannel::Stdout,
                        bytes,
                        invalid_utf8,
                        ..
                    } => {
                        if invalid_utf8 {
                            return app_server_failure(
                                process,
                                AppServerError::InvalidUtf8.to_string(),
                                protocol.can_fallback(),
                            ).await;
                        }
                        let step = match protocol.handle_frame(&bytes) {
                            Ok(step) => step,
                            Err(error) => {
                                return app_server_failure(
                                    process,
                                    error.to_string(),
                                    protocol.can_fallback(),
                                ).await;
                            }
                        };
                        for frame in step.outbound {
                            if let Err(error) = input.write_all(&frame).await {
                                return app_server_failure(
                                    process,
                                    error.to_string(),
                                    protocol.can_fallback(),
                                ).await;
                            }
                        }
                        for event in step.events {
                            match canonical_raw_event(job, redactor, &event) {
                                Ok(raw) => try_send(job, event_sender, raw),
                                Err(error) => {
                                    return app_server_failure(
                                        process,
                                        error,
                                        protocol.can_fallback(),
                                    ).await;
                                }
                            }
                        }
                        if step.completed {
                            let _ = input.close().await;
                            process.cancel();
                            let result = process.wait().await;
                            return match result {
                                Ok(result) => {
                                    let mut output = process_output(&result);
                                    // Cancelling the long-lived server after a terminal
                                    // turn notification is an orchestrator shutdown, not
                                    // a worker cancellation.
                                    output.exit_code = Some(0);
                                    output.termination = RuntimeTermination::Exited;
                                    AppServerDriveResult::Completed(output)
                                }
                                Err(error) => AppServerDriveResult::Failed(AppServerFailure {
                                    message: error.to_string(),
                                    safe_to_fallback: false,
                                    output: None,
                                }),
                            };
                        }
                    }
                    ProcessEvent::Output {
                        channel: OutputChannel::Stderr,
                        redacted_text,
                        invalid_utf8,
                        ..
                    } => {
                        let raw = RawEvent {
                            channel: if invalid_utf8 {
                                RawEventChannel::Protocol
                            } else {
                                RawEventChannel::Stderr
                            },
                            sequence: next_sequence(job),
                            bytes: redacted_text.into_bytes(),
                            received_at: Utc::now(),
                        };
                        try_send(job, event_sender, raw);
                    }
                    ProcessEvent::FramesDropped { count } => {
                        return app_server_failure(
                            process,
                            format!("App Server protocol lost {count} frame(s)"),
                            protocol.can_fallback(),
                        ).await;
                    }
                    ProcessEvent::Exited { termination, .. } => {
                        let safe = protocol.can_fallback()
                            && termination == orchestrator_process::TerminationReason::Exited;
                        let output = process.wait().await.ok().map(|result| process_output(&result));
                        return AppServerDriveResult::Failed(AppServerFailure {
                            message: "App Server exited before turn completion".to_owned(),
                            safe_to_fallback: safe,
                            output,
                        });
                    }
                    ProcessEvent::Started { .. } => {}
                }
            }
        }
    }
}

async fn app_server_failure(
    process: ProcessSession,
    message: String,
    safe_to_fallback: bool,
) -> AppServerDriveResult {
    process.cancel();
    let output = process
        .wait()
        .await
        .ok()
        .map(|result| process_output(&result));
    AppServerDriveResult::Failed(AppServerFailure {
        message,
        safe_to_fallback,
        output,
    })
}

fn canonical_raw_event(
    job: &ProcessJob,
    redactor: &Redactor,
    event: &serde_json::Value,
) -> Result<RawEvent, String> {
    let serialized = serde_json::to_string(&event).map_err(|error| error.to_string())?;
    Ok(RawEvent {
        channel: RawEventChannel::Stdout,
        sequence: next_sequence(job),
        bytes: redactor.redact(&serialized).into_bytes(),
        received_at: Utc::now(),
    })
}

fn emit_fallback_event(
    job: &ProcessJob,
    sender: &mpsc::Sender<RawEvent>,
    redactor: &Redactor,
    reason: &str,
) {
    let event = serde_json::json!({
        "type": "orchestrator.transport_fallback",
        "from": "codex_app_server_stdio",
        "to": "codex_exec_jsonl",
        "reason": redactor.redact(reason),
    });
    if let Ok(raw) = canonical_raw_event(job, redactor, &event) {
        try_send(job, sender, raw);
    }
}

fn emit_protocol_error(
    job: &ProcessJob,
    sender: &mpsc::Sender<RawEvent>,
    redactor: &Redactor,
    reason: &str,
) {
    let event = serde_json::json!({
        "type": "error",
        "code": "app_server_protocol_error",
        "message": redactor.redact(reason),
    });
    if let Ok(raw) = canonical_raw_event(job, redactor, &event) {
        try_send(job, sender, raw);
    }
}

fn try_send(job: &ProcessJob, sender: &mpsc::Sender<RawEvent>, event: RawEvent) {
    if let Err(mpsc::error::TrySendError::Full(_)) = sender.try_send(event) {
        add_dropped(job, 1);
    }
}

fn add_dropped(job: &ProcessJob, count: u64) {
    let _ = job
        .dropped_frames
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            Some(current.saturating_add(count))
        });
}

fn next_sequence(job: &ProcessJob) -> u64 {
    job.event_sequence.fetch_add(1, Ordering::Relaxed)
}

fn command_spec(invocation: &PreparedInvocation, redaction: &RedactionConfig) -> CommandSpec {
    let mut spec = CommandSpec::new(&invocation.executable)
        .args(invocation.args.iter().cloned())
        .current_dir(&invocation.working_directory)
        .with_stdin(invocation.stdin.clone());
    if let Some(plan) = &invocation.codex_app_server {
        spec = spec.keep_stdin_open(plan.max_message_bytes);
    }
    spec.timeout = Duration::from_secs(invocation.timeout_seconds);
    spec.stdout_limit = invocation.stdout_limit;
    spec.stderr_limit = invocation.stderr_limit;
    spec.redaction = redaction.clone();
    spec
}

fn process_output(result: &orchestrator_process::ProcessResult) -> RuntimeOutput {
    RuntimeOutput {
        resolved_executable: Some(result.resolved_executable.clone()),
        exit_code: result.exit_code,
        termination: match result.termination {
            orchestrator_process::TerminationReason::Exited => RuntimeTermination::Exited,
            orchestrator_process::TerminationReason::TimedOut => RuntimeTermination::TimedOut,
            orchestrator_process::TerminationReason::Cancelled => RuntimeTermination::Cancelled,
        },
        tree_termination_error: result.tree_termination_error.clone(),
        // RuntimeOutput may cross subsystem boundaries, so expose only the
        // redacted representation. Raw capture bytes remain transient inside
        // orchestrator-process.
        stdout: result.stdout.redacted_text.clone().into_bytes(),
        stderr: result.stderr.redacted_text.clone().into_bytes(),
        truncated: result.stdout.truncated || result.stderr.truncated,
    }
}

fn raw_process_event(job: &ProcessJob, event: ProcessEvent) -> Option<RawEvent> {
    match event {
        ProcessEvent::Output {
            channel,
            redacted_text,
            invalid_utf8,
            ..
        } => Some(RawEvent {
            channel: if invalid_utf8 {
                RawEventChannel::Protocol
            } else {
                match channel {
                    OutputChannel::Stdout => RawEventChannel::Stdout,
                    OutputChannel::Stderr => RawEventChannel::Stderr,
                }
            },
            sequence: next_sequence(job),
            bytes: if invalid_utf8 {
                serde_json::to_vec(&serde_json::json!({
                    "type": "orchestrator.invalid_utf8",
                    "redacted": redacted_text,
                }))
                .unwrap_or_default()
            } else {
                redacted_text.into_bytes()
            },
            received_at: Utc::now(),
        }),
        ProcessEvent::FramesDropped { count } => Some(RawEvent {
            channel: RawEventChannel::Protocol,
            sequence: next_sequence(job),
            bytes: serde_json::to_vec(&serde_json::json!({
                "type": "orchestrator.frames_dropped",
                "count": count,
            }))
            .unwrap_or_default(),
            received_at: Utc::now(),
        }),
        ProcessEvent::Started { .. } | ProcessEvent::Exited { .. } => None,
    }
}

fn validate_safe_probe(args: &[OsString]) -> Result<(), ProviderError> {
    let args = args
        .iter()
        .map(|value| value.to_string_lossy())
        .collect::<Vec<_>>();
    let safe = matches!(args.as_slice(), [value] if value == "--version" || value == "--help")
        || matches!(args.as_slice(), [command, help] if command == "exec" && help == "--help")
        || matches!(args.as_slice(), [exec, resume, help] if exec == "exec" && resume == "resume" && help == "--help")
        || matches!(args.as_slice(), [app, help] if app == "app-server" && help == "--help")
        || matches!(args.as_slice(), [app, generate, out, _] if app == "app-server" && generate == "generate-json-schema" && out == "--out");
    if safe {
        Ok(())
    } else {
        Err(ProviderError::Probe(
            "refusing capability command that could start inference".to_owned(),
        ))
    }
}

fn enforce_fake_provider_only(executable: &Path) -> Result<(), ProviderError> {
    let fake_only =
        std::env::var_os("COLAY_TEST_FAKE_PROVIDERS_ONLY").is_some_and(|value| value == "1");
    validate_fake_provider_executable(executable, fake_only)
}

fn validate_fake_provider_executable(
    executable: &Path,
    fake_only: bool,
) -> Result<(), ProviderError> {
    if !fake_only {
        return Ok(());
    }
    let basename = executable
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        basename.as_str(),
        "codex" | "codex.exe" | "claude" | "claude.exe" | "gemini" | "gemini.exe"
    ) {
        return Err(ProviderError::Runtime(format!(
            "fake-provider-only mode refuses real provider executable {basename}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_result_with_uncertain_tree(
        termination: orchestrator_process::TerminationReason,
    ) -> Result<orchestrator_process::ProcessResult, orchestrator_process::RedactionError> {
        let redactor = Redactor::new(&RedactionConfig::default())?;
        Ok(orchestrator_process::ProcessResult {
            resolved_executable: orchestrator_process::ResolvedExecutable {
                configured: "fake-provider-cli".into(),
                path: "fake-provider-cli".into(),
                kind: orchestrator_process::ExecutableKind::Native,
                validation: orchestrator_process::ExecutableValidationContext {
                    working_directory: ".".into(),
                    search_directory: None,
                },
            },
            exit_code: None,
            termination,
            tree_termination_error: Some("process-tree termination was not confirmed".to_owned()),
            stdout: orchestrator_process::CapturedOutput::for_test(
                b"safe output".to_vec(),
                redactor.clone(),
            ),
            stderr: orchestrator_process::CapturedOutput::for_test(Vec::new(), redactor),
            elapsed: Duration::from_millis(5),
        })
    }

    #[test]
    fn capability_allowlist_rejects_inference() {
        assert!(validate_safe_probe(&[OsString::from("--version")]).is_ok());
        assert!(
            validate_safe_probe(&[OsString::from("exec"), OsString::from("do work"),]).is_err()
        );
    }

    #[test]
    fn fake_provider_only_guard_rejects_real_cli_basenames() {
        for executable in [
            "codex",
            "codex.exe",
            "claude",
            "claude.exe",
            "gemini",
            "gemini.exe",
        ] {
            assert!(validate_fake_provider_executable(Path::new(executable), true).is_err());
        }
        assert!(
            validate_fake_provider_executable(Path::new("fake-provider-cli.exe"), true).is_ok()
        );
        assert!(validate_fake_provider_executable(Path::new("codex"), false).is_ok());
    }

    #[test]
    fn timeout_preserves_uncertain_process_tree_termination()
    -> Result<(), orchestrator_process::RedactionError> {
        let output = process_output(&process_result_with_uncertain_tree(
            orchestrator_process::TerminationReason::TimedOut,
        )?);

        assert_eq!(output.termination, RuntimeTermination::TimedOut);
        assert_eq!(
            output.tree_termination_error.as_deref(),
            Some("process-tree termination was not confirmed")
        );
        Ok(())
    }

    #[test]
    fn cancellation_preserves_uncertain_process_tree_termination()
    -> Result<(), orchestrator_process::RedactionError> {
        let output = process_output(&process_result_with_uncertain_tree(
            orchestrator_process::TerminationReason::Cancelled,
        )?);

        assert_eq!(output.termination, RuntimeTermination::Cancelled);
        assert_eq!(
            output.tree_termination_error.as_deref(),
            Some("process-tree termination was not confirmed")
        );
        Ok(())
    }
}
