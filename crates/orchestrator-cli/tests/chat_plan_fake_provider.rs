#![cfg(feature = "test-fixtures")]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::Arc,
};

use orchestrator_domain::{
    CapabilitySupport, GraphValidationPolicy, MessageId, ModelProfile, ProviderCapabilities,
    ProviderId, SandboxMode, SessionId,
};
use orchestrator_engine::{PlannerRequest, TaskPlanner, collect_planner_response};
use orchestrator_process::RedactionConfig;
use orchestrator_providers::{AdapterRuntime, ProcessAdapterRuntime};
use orchestrator_state::RootConfig;
use serde_json::Value;

use colay::task_planner::OfficialCliTaskPlanner;

fn fake_provider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_colay-e2e-fake-provider"))
}

fn capability() -> ProviderCapabilities {
    let mut capability = ProviderCapabilities::unsupported(ProviderId::Codex);
    capability.non_interactive = CapabilitySupport::Verified;
    capability.structured_output = CapabilitySupport::Verified;
    capability.read_only = CapabilitySupport::Verified;
    capability.reasoning_effort = CapabilitySupport::Verified;
    capability.evidence = vec!["fake CLI probe verified read-only JSONL".to_owned()];
    capability
}

fn request(goal: &str) -> PlannerRequest {
    PlannerRequest {
        revision_id: orchestrator_domain::GraphRevisionId::new(),
        session_id: SessionId::new(),
        goal_message_id: MessageId::new(),
        goal_redacted: goal.to_owned(),
        repository_summary_redacted: "one Rust workspace".to_owned(),
        validation_policy: GraphValidationPolicy {
            eligible_providers: BTreeSet::from([ProviderId::Codex]),
            eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
            max_parallel_workers: 2,
            per_provider_limits: BTreeMap::from([(ProviderId::Codex, 1)]),
        },
        sandbox: SandboxMode::ReadOnly,
    }
}

#[tokio::test]
async fn plans_through_a_bounded_read_only_shell_free_fake_cli()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = fs::canonicalize(directory.path())?;
    fs::create_dir_all(repository.join(".colay"))?;
    let mut config = RootConfig::default();
    config.features.codex_app_server_adapter = false;
    config.orchestrator.default_timeout_minutes = 1;
    config.orchestrator.providers.gemini = None;
    config.orchestrator.providers.claude = None;
    let codex = config
        .orchestrator
        .providers
        .codex
        .as_mut()
        .ok_or("codex config")?;
    codex.executable = fake_provider_binary().to_string_lossy().into_owned();
    let standard = config
        .orchestrator
        .model_profiles
        .get_mut("codex")
        .and_then(|profiles| profiles.get_mut("standard"))
        .ok_or("codex standard profile")?;
    standard.model = "configured-planner-model".to_owned();
    standard.effort = Some("high".to_owned());
    let runtime: Arc<dyn AdapterRuntime> =
        Arc::new(ProcessAdapterRuntime::new(RedactionConfig::default()));
    let planner = OfficialCliTaskPlanner::from_config(
        &config,
        &repository,
        runtime,
        &[capability()],
        ModelProfile::Standard,
    )?;

    let request = request("plan a task graph");
    let response = planner.propose(request.clone()).await?;
    assert_eq!(response.sandbox, SandboxMode::ReadOnly);
    let graph = collect_planner_response(&request, response)?;
    assert_eq!(graph.proposal.nodes.len(), 2);
    assert!(!repository.join(".colay/worktrees").exists());

    let log: Value = serde_json::from_slice(&fs::read(
        repository.join(".colay/fake-planner-invocation.json"),
    )?)?;
    let logged_cwd = log["cwd"].as_str().ok_or("missing cwd")?;
    assert_eq!(fs::canonicalize(logged_cwd)?, repository);
    let args = log["args"].as_array().ok_or("missing args")?;
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--sandbox" && pair[1] == "read-only")
    );
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--model" && pair[1] == "configured-planner-model")
    );
    assert!(
        args.iter()
            .any(|arg| arg == "model_reasoning_effort=\"high\"")
    );
    assert_eq!(log["timeout_seconds"], 60);
    assert_eq!(log["stdout_limit"], 1024 * 1024);
    Ok(())
}

#[tokio::test]
async fn no_capability_evidence_and_malformed_output_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = fs::canonicalize(directory.path())?;
    fs::create_dir_all(repository.join(".colay"))?;
    let mut config = RootConfig::default();
    config.features.codex_app_server_adapter = false;
    config.orchestrator.providers.gemini = None;
    config.orchestrator.providers.claude = None;
    config
        .orchestrator
        .providers
        .codex
        .as_mut()
        .ok_or("codex config")?
        .executable = fake_provider_binary().to_string_lossy().into_owned();
    let runtime: Arc<dyn AdapterRuntime> =
        Arc::new(ProcessAdapterRuntime::new(RedactionConfig::default()));
    assert!(
        OfficialCliTaskPlanner::from_config(
            &config,
            &repository,
            Arc::clone(&runtime),
            &[],
            ModelProfile::Standard,
        )
        .is_err()
    );

    let planner = OfficialCliTaskPlanner::from_config(
        &config,
        &repository,
        runtime,
        &[capability()],
        ModelProfile::Standard,
    )?;
    let malformed = request("scenario:malformed planner output");
    let response = planner.propose(malformed.clone()).await?;
    assert!(collect_planner_response(&malformed, response).is_err());
    Ok(())
}
