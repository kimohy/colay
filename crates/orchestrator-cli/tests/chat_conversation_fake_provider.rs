#![cfg(feature = "test-fixtures")]

use std::{fs, path::PathBuf, sync::Arc};

use orchestrator_domain::{
    CapabilitySupport, ConversationAttemptId, ConversationOutcome, MessageId, ModelProfile,
    ProviderCapabilities, ProviderId, SandboxMode, SessionId,
};
use orchestrator_engine::{
    ConversationOrchestrator, ConversationRequest, collect_conversation_response,
};
use orchestrator_providers::AdapterRuntime;
use orchestrator_state::RootConfig;
use orchestrator_test_support::{FakeAdapterRuntime, FakeRuntimeScenario};

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
    let mut capability = ProviderCapabilities::unsupported(ProviderId::Codex);
    capability.non_interactive = CapabilitySupport::Verified;
    capability.structured_output = CapabilitySupport::Verified;
    capability.read_only = CapabilitySupport::Verified;
    capability.evidence = vec!["fake CLI supports read-only JSONL".to_owned()];
    capability
}

fn request(transcript: &str) -> ConversationRequest {
    ConversationRequest {
        attempt_id: ConversationAttemptId::new(),
        session_id: SessionId::new(),
        source_message_id: MessageId::new(),
        transcript_redacted: transcript.to_owned(),
        repository_summary_redacted: "Git availability is not required for answers".to_owned(),
        sandbox: SandboxMode::ReadOnly,
    }
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
