use std::path::PathBuf;
use std::sync::Arc;

use orchestrator_domain::{
    AttemptId, ModelProfile, ProviderId, QuotaPeriod, QuotaScope, ReasoningEffort, SandboxMode,
    SchemaVersion, TaskId, UsageUnit, WorkerEvent, WorkerRequest,
};
use orchestrator_providers::{
    CodexAdapter, CodexAdapterConfig, CodexTransportFeatures, CodexTransportPreference,
    ProcessAdapterRuntime, RuntimeTermination, UsageProbeConfig, WorkerAdapter,
};

fn fake_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-provider-cli"))
}

fn request(prompt: &str) -> Result<WorkerRequest, std::io::Error> {
    Ok(WorkerRequest {
        schema_version: SchemaVersion::v1(),
        task_id: TaskId::new(),
        attempt_id: AttemptId::new(),
        provider: ProviderId::Codex,
        objective: "exercise stable App Server transport".to_owned(),
        prompt: prompt.to_owned(),
        constraints: vec!["fake binary only".to_owned()],
        acceptance_criteria: vec!["terminal structured event".to_owned()],
        workspace_root: std::env::current_dir()?,
        sandbox: SandboxMode::ReadOnly,
        profile: ModelProfile::Standard,
        model: None,
        reasoning_effort: Some(ReasoningEffort::Medium),
        timeout_seconds: 10,
        max_output_bytes: 1024 * 1024,
        resume_session_id: None,
        handover_payload: None,
    })
}

fn adapter(features: CodexTransportFeatures, preference: CodexTransportPreference) -> CodexAdapter {
    CodexAdapter::new(
        CodexAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: QuotaScope::new("monthly", QuotaPeriod::CalendarMonth, UsageUnit::Credits),
            allow_untested_read_only: false,
        },
        Arc::new(ProcessAdapterRuntime::default()),
    )
    .with_transport_features(features)
    .with_transport_preference(preference)
}

#[tokio::test]
async fn stable_app_server_completes_through_production_runtime()
-> Result<(), Box<dyn std::error::Error>> {
    let adapter = adapter(
        CodexTransportFeatures::default(),
        CodexTransportPreference::AppServerFirst,
    );
    let handle = adapter.start(request("success")?).await?;
    let mut started = false;
    let mut completed = false;
    let mut fallback = false;
    while let Some(raw) = adapter.next_event(&handle).await? {
        match adapter.parse_event(raw).await? {
            WorkerEvent::Started { .. } => started = true,
            WorkerEvent::Completed { .. } => completed = true,
            WorkerEvent::Unknown { event_type, .. }
                if event_type == "orchestrator.transport_fallback" =>
            {
                fallback = true;
            }
            _ => {}
        }
    }
    let output = adapter.wait(&handle).await?;
    assert_eq!(output.termination, RuntimeTermination::Exited);
    assert_eq!(output.exit_code, Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("turn/completed"));
    assert!(started);
    assert!(completed);
    assert!(!fallback);
    Ok(())
}

#[tokio::test]
async fn protocol_failure_falls_back_once_to_verified_exec_jsonl()
-> Result<(), Box<dyn std::error::Error>> {
    let adapter = adapter(
        CodexTransportFeatures::default(),
        CodexTransportPreference::AppServerFirst,
    );
    let handle = adapter
        .start(request("scenario:appserver-protocol-error")?)
        .await?;
    let mut completed = false;
    let mut fallback_count = 0_u8;
    while let Some(raw) = adapter.next_event(&handle).await? {
        match adapter.parse_event(raw).await? {
            WorkerEvent::Completed { .. } => completed = true,
            WorkerEvent::Unknown { event_type, .. }
                if event_type == "orchestrator.transport_fallback" =>
            {
                fallback_count = fallback_count.saturating_add(1);
            }
            _ => {}
        }
    }
    let output = adapter.wait(&handle).await?;
    assert_eq!(output.termination, RuntimeTermination::Exited);
    assert!(completed);
    assert_eq!(fallback_count, 1);
    Ok(())
}

#[tokio::test]
async fn fallback_chain_never_retries_a_second_transport() -> Result<(), Box<dyn std::error::Error>>
{
    let adapter = adapter(
        CodexTransportFeatures::default(),
        CodexTransportPreference::AppServerFirst,
    );
    let handle = adapter
        .start(request(
            "scenario:appserver-protocol-error scenario:malformed",
        )?)
        .await?;
    let mut fallback_count = 0_u8;
    let mut malformed_exec = false;
    while let Some(raw) = adapter.next_event(&handle).await? {
        match adapter.parse_event(raw).await {
            Ok(WorkerEvent::Unknown { event_type, .. })
                if event_type == "orchestrator.transport_fallback" =>
            {
                fallback_count = fallback_count.saturating_add(1);
            }
            Err(_) => malformed_exec = true,
            Ok(_) => {}
        }
    }
    let _ = adapter.wait(&handle).await?;
    assert_eq!(fallback_count, 1);
    assert!(malformed_exec);
    Ok(())
}

#[tokio::test]
async fn disabled_app_server_feature_uses_exec_directly() -> Result<(), Box<dyn std::error::Error>>
{
    let adapter = adapter(
        CodexTransportFeatures {
            app_server_adapter: false,
            exec_fallback: true,
        },
        CodexTransportPreference::ExecJsonlFirst,
    );
    let handle = adapter.start(request("success")?).await?;
    let mut fallback = false;
    let mut completed = false;
    while let Some(raw) = adapter.next_event(&handle).await? {
        match adapter.parse_event(raw).await? {
            WorkerEvent::Completed { .. } => completed = true,
            WorkerEvent::Unknown { event_type, .. }
                if event_type == "orchestrator.transport_fallback" =>
            {
                fallback = true;
            }
            _ => {}
        }
    }
    let _ = adapter.wait(&handle).await?;
    assert!(completed);
    assert!(!fallback);
    Ok(())
}

#[tokio::test]
async fn default_transport_priority_is_exec_jsonl() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = adapter(
        CodexTransportFeatures::default(),
        CodexTransportPreference::default(),
    );
    let handle = adapter.start(request("success")?).await?;
    while adapter.next_event(&handle).await?.is_some() {}
    let output = adapter.wait(&handle).await?;
    assert_eq!(output.termination, RuntimeTermination::Exited);
    assert!(String::from_utf8_lossy(&output.stdout).contains("thread.started"));
    Ok(())
}

#[tokio::test]
async fn disabling_both_codex_transports_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = adapter(
        CodexTransportFeatures {
            app_server_adapter: false,
            exec_fallback: false,
        },
        CodexTransportPreference::ExecJsonlFirst,
    );
    assert!(adapter.start(request("success")?).await.is_err());
    Ok(())
}
