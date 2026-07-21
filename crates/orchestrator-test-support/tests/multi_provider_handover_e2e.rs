use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use orchestrator_domain::{
    AcceptanceEvidence, AttemptId, FailureRecord, HandoverAcknowledgement, HandoverBundle,
    ModelProfile, PlanStep, PlanStepStatus, ProviderId, QuotaPeriod, QuotaScope, ReasoningEffort,
    RepoPath, SandboxMode, SchemaVersion, TaskId, TaskState, TestEvidence, TestStatus,
    TransitionGuards, UntrustedWorkerClaim, UsageConfidence, UsageSnapshot, UsageSource, UsageUnit,
    VerificationStatus, WorkerEvent, WorkerRequest,
};
use orchestrator_engine::{
    CheckpointInput, CheckpointManager, GitCheckpointEvidence, GitSnapshot, HandoverInput,
    HandoverManager, TaskLifecycle, VerificationEngine, VerificationInput,
};
use orchestrator_providers::{
    ClaudeAdapter, ClaudeAdapterConfig, CodexAdapter, CodexAdapterConfig, GeminiAdapter,
    GeminiAdapterConfig, RuntimeOutput, UsageProbeConfig, WorkerAdapter,
};
use orchestrator_state::ArtifactStore;
use orchestrator_test_support::{FakeAdapterRuntime, FakeRuntimeScenario};

fn fake_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-provider-cli"))
}

fn scope(provider: ProviderId) -> QuotaScope {
    match provider {
        ProviderId::Gemini | ProviderId::Agy => {
            QuotaScope::new("daily", QuotaPeriod::CalendarDay, UsageUnit::Requests)
        }
        ProviderId::Codex | ProviderId::Claude => {
            QuotaScope::new("monthly", QuotaPeriod::CalendarMonth, UsageUnit::Credits)
        }
    }
}

fn confirmed_usage(provider: ProviderId, used: f64, limit: f64, remaining: f64) -> UsageSnapshot {
    let quota_scope = scope(provider);
    UsageSnapshot {
        schema_version: SchemaVersion::v1(),
        provider,
        quota_period: quota_scope.period,
        quota_scope,
        used: Some(used),
        limit: Some(limit),
        remaining: Some(remaining),
        used_percent: Some(used / limit * 100.0),
        remaining_percent: Some(remaining / limit * 100.0),
        period_started_at: None,
        resets_at: None,
        source: UsageSource::OfficialCli,
        confidence: UsageConfidence::Confirmed,
        collected_at: Utc::now(),
    }
}

#[allow(clippy::too_many_arguments)]
fn request(
    task_id: TaskId,
    attempt_id: AttemptId,
    provider: ProviderId,
    workspace_root: PathBuf,
    sandbox: SandboxMode,
    profile: ModelProfile,
    prompt: &str,
    handover: Option<&HandoverBundle>,
) -> Result<WorkerRequest, serde_json::Error> {
    Ok(WorkerRequest {
        schema_version: SchemaVersion::v1(),
        task_id,
        attempt_id,
        provider,
        objective: "implement and independently verify the requested change".to_owned(),
        prompt: prompt.to_owned(),
        constraints: vec!["use only the supplied fake provider contract".to_owned()],
        acceptance_criteria: vec!["the independent verification gate passes".to_owned()],
        workspace_root,
        sandbox,
        profile,
        model: Some(String::new()),
        reasoning_effort: Some(ReasoningEffort::Medium),
        timeout_seconds: 10,
        max_output_bytes: 1024 * 1024,
        resume_session_id: None,
        handover_payload: handover.map(serde_json::to_value).transpose()?,
    })
}

#[allow(clippy::type_complexity)]
async fn run_fake_worker<A: WorkerAdapter>(
    adapter: &A,
    request: WorkerRequest,
) -> Result<(Vec<WorkerEvent>, UntrustedWorkerClaim, RuntimeOutput), Box<dyn std::error::Error>> {
    let handle = adapter.start(request).await?;
    assert!(
        handle.process_id.is_none(),
        "the in-memory fake runtime must not spawn a process"
    );
    let mut events = Vec::new();
    while let Some(raw) = adapter.next_event(&handle).await? {
        events.push(adapter.parse_event(raw).await?);
    }
    let claim = adapter.checkpoint(&handle).await?;
    let output = adapter.wait(&handle).await?;
    Ok((events, claim, output))
}

fn acknowledgement(bundle: &HandoverBundle) -> HandoverAcknowledgement {
    HandoverAcknowledgement {
        schema_version: SchemaVersion::v1(),
        task_id: bundle.task_id,
        bundle_hash: bundle.integrity_hash.clone(),
        provider: bundle.recommended_next_worker,
        understood_objective: bundle.objective.clone(),
        understood_constraints: bundle.constraints.clone(),
        understood_acceptance_criteria: bundle.acceptance_criteria.clone(),
        next_step_id: bundle.pending_steps.first().map(|step| step.id.clone()),
        unresolved_questions: bundle.unresolved_questions.clone(),
        can_resume: true,
        acknowledged_at: Utc::now(),
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn gemini_quota_checkpoint_codex_implementation_and_claude_review_complete_safely()
-> Result<(), Box<dyn std::error::Error>> {
    let state = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let state_root = std::fs::canonicalize(state.path())?;
    let workspace_root = std::fs::canonicalize(workspace.path())?;
    std::fs::create_dir_all(workspace_root.join("src"))?;
    let task_id = TaskId::new();
    let gemini_attempt = AttemptId::new();
    let codex_attempt = AttemptId::new();
    let claude_attempt = AttemptId::new();
    let mut lifecycle = TaskLifecycle::new();

    lifecycle.transition(TaskState::Analyzing, TransitionGuards::default())?;
    lifecycle.transition(TaskState::Planned, TransitionGuards::default())?;
    lifecycle.transition(TaskState::Running, TransitionGuards::default())?;

    let gemini = GeminiAdapter::new(
        GeminiAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Gemini),
        },
        Arc::new(FakeAdapterRuntime::new(
            fake_binary(),
            FakeRuntimeScenario::QuotaExceeded,
        )?),
    );
    let (gemini_events, gemini_claim, gemini_output) = run_fake_worker(
        &gemini,
        request(
            task_id,
            gemini_attempt,
            ProviderId::Gemini,
            workspace_root.clone(),
            SandboxMode::ReadOnly,
            ModelProfile::Economy,
            "survey the repository; scenario:quota",
            None,
        )?,
    )
    .await?;
    assert_eq!(gemini_output.exit_code, Some(0));
    assert!(
        gemini_events
            .iter()
            .any(|event| matches!(event, WorkerEvent::QuotaExceeded { .. }))
    );

    lifecycle.transition(TaskState::CheckpointRequested, TransitionGuards::default())?;
    lifecycle.transition(TaskState::Checkpointing, TransitionGuards::default())?;
    let checkpoint_manager = CheckpointManager::new(ArtifactStore::open(&state_root)?);
    let investigation_checkpoint = checkpoint_manager.create(
        CheckpointInput {
            task_id,
            attempt_id: gemini_attempt,
            objective: "implement and independently verify the requested change".to_owned(),
            current_plan: vec![PlanStep {
                id: "implement".to_owned(),
                description: "implement the requested change".to_owned(),
                status: PlanStepStatus::Pending,
            }],
            completed_steps: Vec::new(),
            pending_steps: vec![PlanStep {
                id: "implement".to_owned(),
                description: "implement the requested change".to_owned(),
                status: PlanStepStatus::Pending,
            }],
            files_read: vec![RepoPath::try_from("src")?],
            commands_run: Vec::new(),
            tests: Vec::new(),
            decisions: Vec::new(),
            unresolved_questions: Vec::new(),
            known_failures: vec![FailureRecord {
                code: Some("quota_exceeded".to_owned()),
                summary: "Gemini daily quota was exhausted".to_owned(),
                retryable: false,
                occurred_at: Utc::now(),
            }],
            worker_claim: Some(gemini_claim),
            current_worker: ProviderId::Gemini,
            concise_context_summary: "repository survey reached a safe read-only boundary"
                .to_owned(),
            created_at: Utc::now(),
        },
        GitCheckpointEvidence {
            base_revision: "1111111111111111111111111111111111111111".to_owned(),
            head: "1111111111111111111111111111111111111111".to_owned(),
            diff: Vec::new(),
            changed_files: Vec::new(),
        },
    )?;
    lifecycle.transition(
        TaskState::Checkpointed,
        TransitionGuards {
            checkpoint_integrity_verified: investigation_checkpoint.verify_integrity()?,
            ..TransitionGuards::default()
        },
    )?;
    lifecycle.transition(TaskState::HandoverRequested, TransitionGuards::default())?;
    lifecycle.transition(TaskState::HandingOver, TransitionGuards::default())?;

    let codex_bundle = HandoverManager::create(HandoverInput {
        checkpoint: investigation_checkpoint,
        original_request: "implement and verify the change".to_owned(),
        constraints: vec!["use only the supplied fake provider contract".to_owned()],
        acceptance_criteria: vec!["the independent verification gate passes".to_owned()],
        recommended_next_worker: ProviderId::Codex,
        usage_snapshots: vec![confirmed_usage(ProviderId::Gemini, 100.0, 100.0, 0.0)],
        safe_boundary_confirmed: true,
        created_at: Utc::now(),
    })?;
    let codex_payload = HandoverManager::stdin_payload(&codex_bundle)?;
    let decoded: HandoverBundle = serde_json::from_slice(&codex_payload)?;
    assert_eq!(decoded.integrity_hash, codex_bundle.integrity_hash);
    assert!(matches!(
        decoded.usage_snapshots.as_slice(),
        [snapshot] if snapshot.is_confirmed_exhausted()
    ));
    let codex_ack = acknowledgement(&codex_bundle);
    HandoverManager::validate_acknowledgement(&codex_bundle, &codex_ack)?;
    lifecycle.transition(
        TaskState::Resuming,
        TransitionGuards {
            handover_integrity_verified: codex_bundle.verify_integrity()?,
            handover_acknowledged: true,
            ..TransitionGuards::default()
        },
    )?;
    lifecycle.transition(TaskState::Running, TransitionGuards::default())?;

    let codex = CodexAdapter::new(
        CodexAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Codex),
            allow_untested_read_only: false,
        },
        Arc::new(FakeAdapterRuntime::new(
            fake_binary(),
            FakeRuntimeScenario::Success,
        )?),
    );
    let (codex_events, codex_claim, codex_output) = run_fake_worker(
        &codex,
        request(
            task_id,
            codex_attempt,
            ProviderId::Codex,
            workspace_root.clone(),
            SandboxMode::WorkspaceWrite,
            ModelProfile::Premium,
            "continue from the sealed handover and implement",
            Some(&codex_bundle),
        )?,
    )
    .await?;
    assert_eq!(codex_output.exit_code, Some(0));
    assert!(
        codex_events
            .iter()
            .any(|event| matches!(event, WorkerEvent::Completed { .. }))
    );
    std::fs::write(
        workspace_root.join("src/lib.rs"),
        b"pub fn answer() -> u32 { 42 }\n",
    )?;
    let changed = RepoPath::try_from("src/lib.rs")?;
    let implementation_diff = b"diff --git a/src/lib.rs b/src/lib.rs\n--- /dev/null\n+++ b/src/lib.rs\n@@ -0,0 +1 @@\n+pub fn answer() -> u32 { 42 }\n".to_vec();
    let implementation_snapshot = GitSnapshot {
        base_revision: "1111111111111111111111111111111111111111".to_owned(),
        head: "1111111111111111111111111111111111111111".to_owned(),
        status_porcelain: b"?? src/lib.rs\0".to_vec(),
        diff: implementation_diff.clone(),
        changed_files: vec![changed.clone()],
    };
    assert!(
        VerificationEngine::new()?
            .preflight_persistence(&workspace_root, &implementation_snapshot)?
            .safe_to_persist_or_share()
    );

    lifecycle.transition(TaskState::CheckpointRequested, TransitionGuards::default())?;
    lifecycle.transition(TaskState::Checkpointing, TransitionGuards::default())?;
    let implementation_checkpoint = checkpoint_manager.create(
        CheckpointInput {
            task_id,
            attempt_id: codex_attempt,
            objective: codex_bundle.objective.clone(),
            current_plan: vec![PlanStep {
                id: "review".to_owned(),
                description: "independently review the implementation".to_owned(),
                status: PlanStepStatus::Pending,
            }],
            completed_steps: Vec::new(),
            pending_steps: vec![PlanStep {
                id: "review".to_owned(),
                description: "independently review the implementation".to_owned(),
                status: PlanStepStatus::Pending,
            }],
            files_read: vec![RepoPath::try_from("src/lib.rs")?],
            commands_run: Vec::new(),
            tests: vec![TestEvidence {
                name: "fake implementation test".to_owned(),
                status: TestStatus::Passed,
                command_id: None,
                detail: Some("fixture evidence".to_owned()),
            }],
            decisions: Vec::new(),
            unresolved_questions: Vec::new(),
            known_failures: Vec::new(),
            worker_claim: Some(codex_claim),
            current_worker: ProviderId::Codex,
            concise_context_summary:
                "Codex completed the implementation in the preserved workspace".to_owned(),
            created_at: Utc::now(),
        },
        GitCheckpointEvidence {
            base_revision: "1111111111111111111111111111111111111111".to_owned(),
            head: "1111111111111111111111111111111111111111".to_owned(),
            diff: implementation_diff,
            changed_files: vec![changed.clone()],
        },
    )?;
    lifecycle.transition(
        TaskState::Checkpointed,
        TransitionGuards {
            checkpoint_integrity_verified: implementation_checkpoint.verify_integrity()?,
            ..TransitionGuards::default()
        },
    )?;
    lifecycle.transition(TaskState::HandoverRequested, TransitionGuards::default())?;
    lifecycle.transition(TaskState::HandingOver, TransitionGuards::default())?;

    let claude_bundle = HandoverManager::create(HandoverInput {
        checkpoint: implementation_checkpoint,
        original_request: codex_bundle.original_request,
        constraints: codex_bundle.constraints,
        acceptance_criteria: codex_bundle.acceptance_criteria,
        recommended_next_worker: ProviderId::Claude,
        usage_snapshots: vec![confirmed_usage(ProviderId::Codex, 90.0, 100.0, 10.0)],
        safe_boundary_confirmed: true,
        created_at: Utc::now(),
    })?;
    let claude_ack = acknowledgement(&claude_bundle);
    assert!(matches!(
        claude_bundle.usage_snapshots.as_slice(),
        [snapshot] if snapshot
            .remaining_percent
            .is_some_and(|value| (value - 10.0).abs() < f64::EPSILON)
    ));
    HandoverManager::validate_acknowledgement(&claude_bundle, &claude_ack)?;
    lifecycle.transition(
        TaskState::Resuming,
        TransitionGuards {
            handover_integrity_verified: claude_bundle.verify_integrity()?,
            handover_acknowledged: true,
            ..TransitionGuards::default()
        },
    )?;
    lifecycle.transition(TaskState::Verifying, TransitionGuards::default())?;

    let claude = ClaudeAdapter::new(
        ClaudeAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Claude),
            effort_flag_enabled: true,
        },
        Arc::new(FakeAdapterRuntime::new(
            fake_binary(),
            FakeRuntimeScenario::Success,
        )?),
    );
    let (claude_events, _review_claim, claude_output) = run_fake_worker(
        &claude,
        request(
            task_id,
            claude_attempt,
            ProviderId::Claude,
            workspace_root.clone(),
            SandboxMode::ReadOnly,
            ModelProfile::Premium,
            "review the implementation without changing files",
            Some(&claude_bundle),
        )?,
    )
    .await?;
    assert_eq!(claude_output.exit_code, Some(0));
    assert!(
        claude_events
            .iter()
            .any(|event| matches!(event, WorkerEvent::Completed { .. }))
    );

    let verification = VerificationEngine::new()?.verify(VerificationInput {
        task_id,
        implementation_provider: ProviderId::Codex,
        reviewer_provider: Some(ProviderId::Claude),
        independent_review_required: true,
        independent_review_passed: true,
        snapshot: implementation_snapshot,
        worktree_root: workspace_root,
        expected_paths: vec![changed],
        commands: Vec::new(),
        tests: vec![TestEvidence {
            name: "fake implementation test".to_owned(),
            status: TestStatus::Passed,
            command_id: None,
            detail: Some("fixture evidence".to_owned()),
        }],
        acceptance_criteria: vec![AcceptanceEvidence {
            criterion: "the independent verification gate passes".to_owned(),
            status: VerificationStatus::Pass,
            evidence: vec!["fake Claude review completed read-only".to_owned()],
        }],
        unresolved_todos: Vec::new(),
        verified_at: Utc::now(),
    })?;
    assert!(verification.passes_completion_gate(true));
    lifecycle.transition(
        TaskState::Completed,
        TransitionGuards {
            verification_passed: true,
            independent_review_required: true,
            independent_review_satisfied: true,
            ..TransitionGuards::default()
        },
    )?;
    assert_eq!(lifecycle.state(), TaskState::Completed);
    Ok(())
}
