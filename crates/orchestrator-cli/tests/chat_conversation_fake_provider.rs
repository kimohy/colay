#![cfg(feature = "test-fixtures")]

use std::{fs, path::PathBuf, sync::Arc};

use orchestrator_domain::{
    ConversationAttemptId, ConversationOutcome, MessageId, ModelProfile, ProviderCapabilities,
    ProviderId, SandboxMode, SessionId,
};
use orchestrator_engine::{
    CONVERSATION_MAX_EVIDENCE_BYTES, ConversationExit, ConversationFailure,
    ConversationOrchestrator, ConversationRequest, collect_conversation_response,
};
use orchestrator_providers::AdapterRuntime;
use orchestrator_state::RootConfig;
use orchestrator_test_support::{
    FakeAdapterRuntime, FakeRuntimeScenario, fake_conversation_capability,
};

use colay::conversation_orchestrator::OfficialCliConversationOrchestrator;

fn fake_provider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_colay-e2e-fake-provider"))
}

fn allowed_fake_binary(repository: &std::path::Path) -> Result<PathBuf, std::io::Error> {
    let destination = repository.join(format!("fake-provider-cli{}", std::env::consts::EXE_SUFFIX));
    fs::copy(fake_provider_binary(), &destination)?;
    Ok(destination)
}

fn capability() -> ProviderCapabilities {
    fake_conversation_capability(ProviderId::Codex)
}

#[tokio::test]
async fn requested_provider_reaches_the_claude_fake_adapter_despite_codex_priority()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = fs::canonicalize(directory.path())?;
    let executable = allowed_fake_binary(&repository)?;
    let runtime: Arc<dyn AdapterRuntime> = Arc::new(FakeAdapterRuntime::new(
        &executable,
        FakeRuntimeScenario::Success,
    )?);
    let mut config = RootConfig::default();
    config.orchestrator.providers.gemini = None;
    config.orchestrator.providers.agy = None;
    for provider in [
        config.orchestrator.providers.codex.as_mut(),
        config.orchestrator.providers.claude.as_mut(),
    ] {
        provider.ok_or("provider config")?.executable = executable.to_string_lossy().into_owned();
    }
    let orchestrator = OfficialCliConversationOrchestrator::from_config(
        &config,
        &repository,
        runtime,
        &[
            fake_conversation_capability(ProviderId::Codex),
            fake_conversation_capability(ProviderId::Claude),
        ],
        ModelProfile::Standard,
    )?;
    let mut request = request("Why does colay need Git?");
    request.provider = ProviderId::Claude;
    let response = orchestrator.converse(request.clone()).await?;
    assert_eq!(response.provider, ProviderId::Claude);
    assert!(response.evidence_redacted.contains("fake claude marker"));
    assert!(matches!(
        collect_conversation_response(&request, response)?,
        ConversationOutcome::AnswerComplete { .. }
    ));
    Ok(())
}

fn request(transcript: &str) -> ConversationRequest {
    ConversationRequest {
        attempt_id: ConversationAttemptId::new(),
        session_id: SessionId::new(),
        source_message_id: MessageId::new(),
        provider: ProviderId::Codex,
        transcript_redacted: transcript.to_owned(),
        repository_summary_redacted: "Git availability is not required for answers".to_owned(),
        sandbox: SandboxMode::ReadOnly,
    }
}

#[tokio::test]
async fn all_fake_providers_preserve_the_canonical_read_only_conversation_contract()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = fs::canonicalize(directory.path())?;
    let executable = allowed_fake_binary(&repository)?;
    let runtime: Arc<dyn AdapterRuntime> = Arc::new(FakeAdapterRuntime::new(
        &executable,
        FakeRuntimeScenario::Success,
    )?);
    let mut config = RootConfig::default();
    for provider in [
        config.orchestrator.providers.codex.as_mut(),
        config.orchestrator.providers.claude.as_mut(),
        config.orchestrator.providers.gemini.as_mut(),
        config.orchestrator.providers.agy.as_mut(),
    ] {
        provider.ok_or("provider config")?.executable = executable.to_string_lossy().into_owned();
    }
    let providers = [
        ProviderId::Codex,
        ProviderId::Claude,
        ProviderId::Gemini,
        ProviderId::Agy,
    ];
    let capabilities = providers.map(fake_conversation_capability);
    let orchestrator = OfficialCliConversationOrchestrator::from_config(
        &config,
        &repository,
        runtime,
        &capabilities,
        ModelProfile::Standard,
    )?;

    for provider in providers {
        let mut request = request("Why does colay need Git?");
        request.provider = provider;
        let response = orchestrator.converse(request.clone()).await?;

        assert_eq!(response.provider, provider);
        assert_eq!(response.sandbox, SandboxMode::ReadOnly);
        assert!(
            response
                .evidence_redacted
                .contains(&format!("fake {provider} marker"))
        );
        assert_eq!(
            serde_json::to_value(collect_conversation_response(&request, response)?)?,
            serde_json::json!({
                "outcome": "answer_complete",
                "response_redacted": "Git is needed only after an approved writable task candidate."
            })
        );
    }
    assert!(!repository.join(".colay/worktrees").exists());
    Ok(())
}

#[tokio::test]
async fn ordinary_question_uses_bounded_read_only_fake_provider_without_worktree()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = fs::canonicalize(directory.path())?;
    let executable = allowed_fake_binary(&repository)?;
    let runtime: Arc<dyn AdapterRuntime> = Arc::new(FakeAdapterRuntime::new(
        &executable,
        FakeRuntimeScenario::Success,
    )?);
    let mut config = RootConfig::default();
    config.orchestrator.providers.gemini = None;
    config.orchestrator.providers.agy = None;
    config.orchestrator.providers.claude = None;
    config
        .orchestrator
        .providers
        .codex
        .as_mut()
        .ok_or("codex config")?
        .executable = executable.to_string_lossy().into_owned();
    let orchestrator = OfficialCliConversationOrchestrator::from_config(
        &config,
        &repository,
        runtime,
        &[capability()],
        ModelProfile::Standard,
    )?;
    let request = request("Why does colay need Git?");
    let response = orchestrator.converse(request.clone()).await?;
    assert_eq!(response.sandbox, SandboxMode::ReadOnly);
    assert!(matches!(
        collect_conversation_response(&request, response)?,
        ConversationOutcome::AnswerComplete { .. }
    ));
    assert!(!repository.join(".colay/worktrees").exists());
    Ok(())
}

#[tokio::test]
async fn fake_provider_response_alias_normalizes_to_canonical_session_output()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = fs::canonicalize(directory.path())?;
    let executable = allowed_fake_binary(&repository)?;
    let runtime: Arc<dyn AdapterRuntime> = Arc::new(FakeAdapterRuntime::new(
        &executable,
        FakeRuntimeScenario::ConversationResponseAlias,
    )?);
    let mut config = RootConfig::default();
    config.orchestrator.providers.gemini = None;
    config.orchestrator.providers.agy = None;
    config.orchestrator.providers.claude = None;
    config
        .orchestrator
        .providers
        .codex
        .as_mut()
        .ok_or("codex config")?
        .executable = executable.to_string_lossy().into_owned();
    let orchestrator = OfficialCliConversationOrchestrator::from_config(
        &config,
        &repository,
        runtime,
        &[capability()],
        ModelProfile::Standard,
    )?;
    let request = request("Compatibility response alias");

    let response = orchestrator.converse(request.clone()).await?;
    assert_eq!(response.exit, ConversationExit::Succeeded);
    let outcome = collect_conversation_response(&request, response)?;
    let persisted_output = serde_json::to_value(outcome)?;

    assert_eq!(
        persisted_output,
        serde_json::json!({
            "outcome": "answer_complete",
            "response_redacted": "Hello! How can I help?"
        })
    );
    assert!(persisted_output.get("response").is_none());
    Ok(())
}

#[tokio::test]
async fn fake_provider_emits_interview_and_candidate_outcomes()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = fs::canonicalize(directory.path())?;
    let executable = allowed_fake_binary(&repository)?;
    let runtime: Arc<dyn AdapterRuntime> = Arc::new(FakeAdapterRuntime::new(
        &executable,
        FakeRuntimeScenario::Success,
    )?);
    let mut config = RootConfig::default();
    config.orchestrator.providers.gemini = None;
    config.orchestrator.providers.agy = None;
    config.orchestrator.providers.claude = None;
    config
        .orchestrator
        .providers
        .codex
        .as_mut()
        .ok_or("codex config")?
        .executable = executable.to_string_lossy().into_owned();
    let orchestrator = OfficialCliConversationOrchestrator::from_config(
        &config,
        &repository,
        runtime,
        &[capability()],
        ModelProfile::Standard,
    )?;
    for (text, expected_candidate) in [("needs-info", false), ("candidate", true)] {
        let request = request(text);
        let response = orchestrator.converse(request.clone()).await?;
        let outcome = collect_conversation_response(&request, response)?;
        assert_eq!(
            matches!(outcome, ConversationOutcome::WorktreeTaskCandidate { .. }),
            expected_candidate
        );
    }
    Ok(())
}

#[tokio::test]
async fn fake_provider_timeout_is_reported_as_a_terminal_lifecycle_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = fs::canonicalize(directory.path())?;
    let executable = allowed_fake_binary(&repository)?;
    let runtime: Arc<dyn AdapterRuntime> = Arc::new(FakeAdapterRuntime::new(
        &executable,
        FakeRuntimeScenario::Timeout,
    )?);
    let mut config = RootConfig::default();
    config.orchestrator.providers.gemini = None;
    config.orchestrator.providers.agy = None;
    config.orchestrator.providers.claude = None;
    config
        .orchestrator
        .providers
        .codex
        .as_mut()
        .ok_or("codex config")?
        .executable = executable.to_string_lossy().into_owned();
    let orchestrator = OfficialCliConversationOrchestrator::from_config(
        &config,
        &repository,
        runtime,
        &[capability()],
        ModelProfile::Standard,
    )?;
    let request = request("candidate");
    let response = orchestrator.converse(request.clone()).await?;
    assert_eq!(response.exit, ConversationExit::TimedOut);
    assert!(matches!(
        collect_conversation_response(&request, response),
        Err(ConversationFailure::Lifecycle {
            exit: ConversationExit::TimedOut,
            ..
        })
    ));
    Ok(())
}

#[tokio::test]
async fn cancelling_a_conversation_future_cancels_the_active_fake_provider_job()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = fs::canonicalize(directory.path())?;
    let executable = allowed_fake_binary(&repository)?;
    let runtime = Arc::new(FakeAdapterRuntime::new(
        &executable,
        FakeRuntimeScenario::TerminalError,
    )?);
    let adapter_runtime: Arc<dyn AdapterRuntime> = runtime.clone();
    let mut config = RootConfig::default();
    config.orchestrator.providers.gemini = None;
    config.orchestrator.providers.agy = None;
    config.orchestrator.providers.claude = None;
    config
        .orchestrator
        .providers
        .codex
        .as_mut()
        .ok_or("codex config")?
        .executable = executable.to_string_lossy().into_owned();
    let orchestrator = OfficialCliConversationOrchestrator::from_config(
        &config,
        &repository,
        adapter_runtime,
        &[capability()],
        ModelProfile::Standard,
    )?;
    let conversation =
        tokio::spawn(async move { orchestrator.converse(request("candidate")).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    conversation.abort();
    let _ = conversation.await;
    for _ in 0..100 {
        if runtime.cancelled_job_count().await == 1 {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    Err("active fake provider job was not cancelled when the conversation future ended".into())
}

#[tokio::test]
async fn noisy_provider_failure_is_deduplicated_bounded_and_safe()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = fs::canonicalize(directory.path())?;
    let executable = allowed_fake_binary(&repository)?;
    let runtime = Arc::new(FakeAdapterRuntime::new(
        &executable,
        FakeRuntimeScenario::DiagnosticNoise,
    )?);
    let adapter_runtime: Arc<dyn AdapterRuntime> = runtime.clone();
    let mut config = RootConfig::default();
    config.orchestrator.providers.codex = None;
    config.orchestrator.providers.claude = None;
    config.orchestrator.providers.agy = None;
    config
        .orchestrator
        .providers
        .gemini
        .as_mut()
        .ok_or("gemini config")?
        .executable = executable.to_string_lossy().into_owned();
    let orchestrator = OfficialCliConversationOrchestrator::from_config(
        &config,
        &repository,
        adapter_runtime,
        &[fake_conversation_capability(ProviderId::Gemini)],
        ModelProfile::Standard,
    )?;
    let mut request = request("bounded diagnostic noise");
    request.provider = ProviderId::Gemini;

    let response = orchestrator.converse(request.clone()).await?;

    assert_eq!(
        response.exit,
        ConversationExit::Crashed {
            exit_code: Some(17)
        }
    );
    assert!(response.output_redacted.is_empty());
    assert_eq!(
        response.evidence_redacted.matches("gemini.stderr").count(),
        1
    );
    assert!(response.evidence_redacted.contains("3 occurrences"));
    assert!(response.evidence_redacted.contains("unsupported account"));
    assert!(
        response
            .evidence_redacted
            .contains("Colay did not enable it")
    );
    assert!(
        response
            .evidence_redacted
            .contains("provider stack frames omitted")
    );
    assert!(
        !response
            .evidence_redacted
            .contains("dangerously-skip-permissions")
    );
    assert!(response.evidence_redacted.lines().count() <= 64);
    assert!(response.evidence_redacted.len() <= CONVERSATION_MAX_EVIDENCE_BYTES);
    assert_eq!(runtime.started_job_count().await, 1);
    assert!(matches!(
        collect_conversation_response(&request, response),
        Err(ConversationFailure::Lifecycle {
            exit: ConversationExit::Crashed {
                exit_code: Some(17)
            },
            ..
        })
    ));
    Ok(())
}
