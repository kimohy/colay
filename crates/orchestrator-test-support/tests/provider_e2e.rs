use std::path::{Path, PathBuf};
use std::sync::Arc;

use orchestrator_domain::{
    AttemptId, ModelProfile, ProviderId, QuotaPeriod, QuotaScope, ReasoningEffort, SandboxMode,
    SchemaVersion, TaskId, UsageUnit, WorkerEvent, WorkerRequest,
};
use orchestrator_providers::{
    ClaudeAdapter, ClaudeAdapterConfig, CodexAdapter, CodexAdapterConfig, GeminiAdapter,
    GeminiAdapterConfig, ProcessAdapterRuntime, ProviderError, RuntimeTermination,
    UsageProbeConfig, UsageProbeFormat, WorkerAdapter,
};
use orchestrator_test_support::{FakeAdapterRuntime, FakeRuntimeScenario};

fn fake_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-provider-cli"))
}

fn request(provider: ProviderId, prompt: &str) -> Result<WorkerRequest, std::io::Error> {
    Ok(WorkerRequest {
        schema_version: SchemaVersion::v1(),
        task_id: TaskId::new(),
        attempt_id: AttemptId::new(),
        provider,
        objective: "exercise fake provider".to_owned(),
        prompt: prompt.to_owned(),
        constraints: vec!["no network".to_owned()],
        acceptance_criteria: vec!["structured result".to_owned()],
        workspace_root: std::env::current_dir()?,
        sandbox: SandboxMode::ReadOnly,
        profile: ModelProfile::Standard,
        model: Some(String::new()),
        reasoning_effort: Some(ReasoningEffort::Medium),
        timeout_seconds: 10,
        max_output_bytes: 1024 * 1024,
        resume_session_id: None,
        handover_payload: None,
    })
}

fn scope(provider: ProviderId) -> QuotaScope {
    match provider {
        ProviderId::Gemini => {
            QuotaScope::new("daily", QuotaPeriod::CalendarDay, UsageUnit::Requests)
        }
        ProviderId::Codex | ProviderId::Claude => {
            QuotaScope::new("monthly", QuotaPeriod::CalendarMonth, UsageUnit::Credits)
        }
    }
}

fn runtime() -> Arc<ProcessAdapterRuntime> {
    Arc::new(ProcessAdapterRuntime::default())
}

#[test]
fn in_memory_runtime_is_locked_to_the_compiled_fake_binary() {
    assert!(FakeAdapterRuntime::new(fake_binary(), FakeRuntimeScenario::Success).is_ok());
    assert!(FakeAdapterRuntime::new("codex", FakeRuntimeScenario::Success).is_err());
}

#[tokio::test]
async fn fake_codex_stream_runs_through_production_process_runtime()
-> Result<(), Box<dyn std::error::Error>> {
    let adapter = CodexAdapter::new(
        CodexAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Codex),
            allow_untested_read_only: false,
        },
        runtime(),
    );
    let handle = adapter
        .start(request(ProviderId::Codex, "success")?)
        .await?;
    let mut completed = false;
    while let Some(raw) = adapter.next_event(&handle).await? {
        if matches!(
            adapter.parse_event(raw).await?,
            WorkerEvent::Completed { .. }
        ) {
            completed = true;
        }
    }
    let result = adapter.wait(&handle).await?;
    assert_eq!(result.exit_code, Some(0));
    assert!(completed);
    Ok(())
}

#[tokio::test]
async fn wait_does_not_require_event_drain() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = CodexAdapter::new(
        CodexAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Codex),
            allow_untested_read_only: false,
        },
        runtime(),
    );
    let handle = adapter
        .start(request(ProviderId::Codex, "success")?)
        .await?;
    let result = adapter.wait(&handle).await?;
    assert_eq!(result.exit_code, Some(0));
    Ok(())
}

#[tokio::test]
async fn fake_quota_is_observed_before_wait() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = GeminiAdapter::new(
        GeminiAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Gemini),
        },
        runtime(),
    );
    let handle = adapter
        .start(request(ProviderId::Gemini, "scenario:quota")?)
        .await?;
    let mut quota = false;
    while let Some(raw) = adapter.next_event(&handle).await? {
        if matches!(
            adapter.parse_event(raw).await?,
            WorkerEvent::QuotaExceeded { .. }
        ) {
            quota = true;
        }
    }
    assert!(quota);
    assert_eq!(adapter.wait(&handle).await?.exit_code, Some(0));
    Ok(())
}

#[tokio::test]
async fn prepared_argv_omits_empty_models_and_uses_safe_permissions()
-> Result<(), Box<dyn std::error::Error>> {
    let shared = runtime();
    let claude = ClaudeAdapter::new(
        ClaudeAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Claude),
            effort_flag_enabled: true,
        },
        shared.clone(),
    );
    let gemini = GeminiAdapter::new(
        GeminiAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Gemini),
        },
        shared,
    );

    let claude_invocation = claude.prepare(&request(ProviderId::Claude, "secret task")?)?;
    let claude_args = claude_invocation.args_lossy();
    assert!(!claude_args.iter().any(|arg| arg == "--model"));
    assert!(
        claude_args
            .windows(2)
            .any(|pair| pair == ["--permission-mode", "plan"])
    );
    assert!(!claude_args.iter().any(|arg| arg.contains("secret task")));

    let gemini_invocation = gemini.prepare(&request(ProviderId::Gemini, "secret task")?)?;
    let gemini_args = gemini_invocation.args_lossy();
    assert!(!gemini_args.iter().any(|arg| arg == "--model"));
    assert!(
        gemini_args
            .windows(2)
            .any(|pair| pair == ["--approval-mode", "plan"])
    );
    assert!(!gemini_args.iter().any(|arg| arg.contains("secret task")));
    assert!(
        Path::new(&gemini_invocation.executable).ends_with("fake-provider-cli.exe")
            || Path::new(&gemini_invocation.executable).ends_with("fake-provider-cli")
    );
    Ok(())
}

#[tokio::test]
async fn fake_silent_worker_cancels_as_a_process_tree() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = GeminiAdapter::new(
        GeminiAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Gemini),
        },
        runtime(),
    );
    let handle = adapter
        .start(request(ProviderId::Gemini, "scenario:timeout")?)
        .await?;
    let cancellation = adapter.cancel(&handle).await?;
    assert_eq!(
        cancellation.outcome,
        orchestrator_domain::CancelOutcome::Cancelled
    );
    while adapter.next_event(&handle).await?.is_some() {}
    let result = adapter.wait(&handle).await?;
    assert_eq!(result.termination, RuntimeTermination::Cancelled);
    Ok(())
}

#[tokio::test]
async fn streaming_runtime_redacts_before_domain_normalization()
-> Result<(), Box<dyn std::error::Error>> {
    let adapter = CodexAdapter::new(
        CodexAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Codex),
            allow_untested_read_only: false,
        },
        runtime(),
    );
    let handle = adapter
        .start(request(ProviderId::Codex, "scenario:secret")?)
        .await?;
    let mut saw_redaction = false;
    while let Some(raw) = adapter.next_event(&handle).await? {
        assert!(!String::from_utf8_lossy(&raw.bytes).contains("supersecretvalue"));
        if matches!(
            adapter.parse_event(raw).await?,
            WorkerEvent::Message { ref text } if text.contains("[REDACTED]")
        ) {
            saw_redaction = true;
        }
    }
    assert!(saw_redaction);
    let _result = adapter.wait(&handle).await?;
    Ok(())
}

#[tokio::test]
async fn configured_usage_probe_uses_executable_and_argv_array()
-> Result<(), Box<dyn std::error::Error>> {
    let adapter = GeminiAdapter::new(
        GeminiAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::Command {
                executable: fake_binary(),
                args: vec!["usage".to_owned()],
                format: UsageProbeFormat::Json,
                working_directory: None,
            },
            usage_scope: scope(ProviderId::Gemini),
        },
        runtime(),
    );
    let snapshots = adapter.collect_usage().await?;
    assert!(matches!(
        snapshots.as_slice(),
        [snapshot]
            if snapshot.used == Some(25.0)
                && snapshot.remaining == Some(75.0)
                && snapshot.source == orchestrator_domain::UsageSource::ConfiguredProbe
    ));
    Ok(())
}

#[tokio::test]
async fn malformed_gemini_stream_fails_without_crashing_the_runtime()
-> Result<(), Box<dyn std::error::Error>> {
    let adapter = GeminiAdapter::new(
        GeminiAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Gemini),
        },
        runtime(),
    );
    let handle = adapter
        .start(request(ProviderId::Gemini, "scenario:malformed")?)
        .await?;
    let raw = adapter
        .next_event(&handle)
        .await?
        .ok_or("fake malformed stream emitted no frame")?;
    assert!(matches!(
        adapter.parse_event(raw).await,
        Err(ProviderError::MalformedOutput(_))
    ));
    while adapter.next_event(&handle).await?.is_some() {}
    assert_eq!(adapter.wait(&handle).await?.exit_code, Some(0));
    Ok(())
}

#[tokio::test]
async fn unknown_codex_lifecycle_fails_closed_but_optional_gemini_event_is_retained()
-> Result<(), Box<dyn std::error::Error>> {
    let codex = CodexAdapter::new(
        CodexAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Codex),
            allow_untested_read_only: false,
        },
        runtime(),
    );
    let codex_handle = codex
        .start(request(ProviderId::Codex, "scenario:unknown")?)
        .await?;
    let codex_raw = codex
        .next_event(&codex_handle)
        .await?
        .ok_or("fake unknown Codex stream emitted no frame")?;
    assert!(matches!(
        codex.parse_event(codex_raw).await,
        Err(ProviderError::CodexCompatibility(_))
    ));
    while codex.next_event(&codex_handle).await?.is_some() {}
    let _codex_result = codex.wait(&codex_handle).await?;

    let gemini = GeminiAdapter::new(
        GeminiAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Gemini),
        },
        runtime(),
    );
    let gemini_handle = gemini
        .start(request(ProviderId::Gemini, "scenario:unknown")?)
        .await?;
    let gemini_raw = gemini
        .next_event(&gemini_handle)
        .await?
        .ok_or("fake unknown Gemini stream emitted no frame")?;
    assert!(matches!(
        gemini.parse_event(gemini_raw).await?,
        WorkerEvent::Unknown {
            affects_lifecycle: false,
            ..
        }
    ));
    while gemini.next_event(&gemini_handle).await?.is_some() {}
    let _gemini_result = gemini.wait(&gemini_handle).await?;
    Ok(())
}

#[tokio::test]
async fn fake_process_crash_preserves_exit_and_bounded_stderr()
-> Result<(), Box<dyn std::error::Error>> {
    let adapter = GeminiAdapter::new(
        GeminiAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Gemini),
        },
        runtime(),
    );
    let handle = adapter
        .start(request(ProviderId::Gemini, "scenario:crash")?)
        .await?;
    while adapter.next_event(&handle).await?.is_some() {}
    let result = adapter.wait(&handle).await?;
    assert_eq!(result.termination, RuntimeTermination::Exited);
    assert_eq!(result.exit_code, Some(17));
    assert!(String::from_utf8_lossy(&result.stderr).contains("fake provider crash"));
    assert!(!result.truncated);
    Ok(())
}

#[tokio::test]
async fn fake_process_timeout_is_reported_distinctly_from_cancellation()
-> Result<(), Box<dyn std::error::Error>> {
    let adapter = GeminiAdapter::new(
        GeminiAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Gemini),
        },
        runtime(),
    );
    let mut timeout_request = request(ProviderId::Gemini, "scenario:timeout")?;
    timeout_request.timeout_seconds = 1;
    let handle = adapter.start(timeout_request).await?;
    while adapter.next_event(&handle).await?.is_some() {}
    let result = adapter.wait(&handle).await?;
    assert_eq!(result.termination, RuntimeTermination::TimedOut);
    assert_ne!(result.exit_code, Some(0));
    Ok(())
}
