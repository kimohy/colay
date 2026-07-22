use std::{collections::BTreeSet, fs, path::Path, process::Command, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;
use orchestrator_daemon::{
    IntegrationServices, MessageRedactor, PlanningServices, process_next_orchestration_command,
};
use orchestrator_domain::{
    AcceptanceEvidence, ApproveIntegrationCommandPayload, AttemptId, ClientCommand,
    ClientCommandAction, ClientCommandId, ClientCommandState, GraphRevisionId,
    GraphValidationPolicy, MessageId, ModelProfile, ProviderId, SchemaVersion, SessionId,
    SessionState, TaskEnvelope, TaskId, VerificationId, VerificationResult, VerificationStatus,
};
use orchestrator_engine::{
    CheckpointInput, CheckpointManager, ConversationFailure, ConversationOrchestrator,
    ConversationRequest, ConversationResponse, GitCheckpointEvidence, GitIntegrationManager,
    GitWorktreeManager, PlannerFailure, PlannerRequest, PlannerResponse, TaskPlanner,
    canonicalize_directory,
};
use orchestrator_state::{
    ArtifactStore, Database, IntegrationBatchStatus, NewTaskAttemptRecord, NewWorktreeRecord,
};
use rusqlite::params;

struct UnusedPlanner;

struct UnusedConversation;

#[async_trait]
impl ConversationOrchestrator for UnusedConversation {
    async fn converse(
        &self,
        _request: ConversationRequest,
    ) -> Result<ConversationResponse, ConversationFailure> {
        Err(ConversationFailure::Invocation {
            reason: "conversation is not expected".to_owned(),
            evidence_redacted: String::new(),
        })
    }
}

#[async_trait]
impl TaskPlanner for UnusedPlanner {
    async fn propose(&self, _request: PlannerRequest) -> Result<PlannerResponse, PlannerFailure> {
        Err(PlannerFailure::Invocation {
            reason: "planning is not expected".to_owned(),
            evidence_redacted: String::new(),
        })
    }
}

struct IdentityRedactor;

impl MessageRedactor for IdentityRedactor {
    fn redact(&self, value: &str) -> String {
        value.to_owned()
    }
}

fn git(repository: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn command(
    session_id: SessionId,
    action: ClientCommandAction,
    payload: serde_json::Value,
) -> ClientCommand {
    let command_id = ClientCommandId::new();
    ClientCommand {
        command_id,
        session_id: Some(session_id),
        task_id: None,
        action,
        payload,
        idempotency_key: format!("integration-e2e-{command_id}"),
        state: ClientCommandState::Pending,
        requested_by: "integration-e2e".to_owned(),
        requested_at: Utc::now(),
        claimed_at: None,
        completed_at: None,
        outcome: None,
    }
}

fn verification(task_id: TaskId) -> VerificationResult {
    VerificationResult {
        schema_version: SchemaVersion::v1(),
        verification_id: VerificationId::new(),
        task_id,
        implementation_provider: ProviderId::Codex,
        reviewer_provider: None,
        status: VerificationStatus::Pass,
        checks: Vec::new(),
        acceptance_criteria: vec![AcceptanceEvidence {
            criterion: "integrate exact source".to_owned(),
            status: VerificationStatus::Pass,
            evidence: vec!["fixture verified".to_owned()],
        }],
        changed_files: Vec::new(),
        out_of_scope_files: Vec::new(),
        unresolved_todos: Vec::new(),
        requires_approval: false,
        verified_at: Utc::now(),
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn typed_preview_and_approval_apply_only_to_dedicated_integration_worktree()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = directory.path().join("repository");
    fs::create_dir_all(&repository)?;
    fs::write(repository.join("a.txt"), "base a\n")?;
    fs::write(repository.join("b.txt"), "base b\n")?;
    fs::write(repository.join(".gitignore"), ".colay/\n")?;
    git(&repository, &["init"])?;
    git(&repository, &["config", "user.name", "Integration E2E"])?;
    git(
        &repository,
        &["config", "user.email", "integration-e2e@example.invalid"],
    )?;
    git(&repository, &["add", "."])?;
    git(&repository, &["commit", "-m", "base"])?;
    let repository = canonicalize_directory(&repository)?;
    let state_root = repository.join(".colay");
    fs::create_dir_all(&state_root)?;
    let database = Database::open(state_root.join("orchestrator.db"))?;
    database.migrate_with_backup(&state_root.join("backups"))?;
    let session_id = SessionId::new();
    let message_id = MessageId::new();
    let revision_id = GraphRevisionId::new();
    let task_ids = [TaskId::new(), TaskId::new()];
    let now = Utc::now();
    database.with_connection(|connection| {
        connection.execute(
            "INSERT INTO sessions(session_id, schema_version, revision, title, state,
                created_at, updated_at, archived_at)
             VALUES (?1, ?2, 0, 'integration e2e', 'running', ?3, ?3, NULL)",
            params![session_id.to_string(), SchemaVersion::V1, now.to_rfc3339()],
        )?;
        connection.execute(
            "INSERT INTO conversation_messages(message_id, session_id, task_id, ordinal, role,
                kind, state, content_redacted, created_at, finalized_at)
             VALUES (?1, ?2, NULL, 1, 'user', 'user_message', 'final', 'integrate', ?3, ?3)",
            params![
                message_id.to_string(),
                session_id.to_string(),
                now.to_rfc3339()
            ],
        )?;
        connection.execute(
            "INSERT INTO graph_revisions(revision_id, session_id, goal_message_id, ordinal,
                status, proposal_hash, proposal_json, validation_json, planner_provider,
                created_at, completed_at)
             VALUES (?1, ?2, ?3, 1, 'approved', NULL, NULL, '{}', 'codex', ?4, ?4)",
            params![
                revision_id.to_string(),
                session_id.to_string(),
                message_id.to_string(),
                now.to_rfc3339()
            ],
        )?;
        connection.execute(
            "INSERT INTO session_graph_heads(session_id, revision_id, updated_at)
             VALUES (?1, ?2, ?3)",
            params![
                session_id.to_string(),
                revision_id.to_string(),
                now.to_rfc3339()
            ],
        )?;
        for (index, task_id) in task_ids.iter().enumerate() {
            let mut envelope = TaskEnvelope::new(format!("source {index}"), "integrate", now);
            envelope.task_id = *task_id;
            connection.execute(
                "INSERT INTO tasks(task_id, schema_version, revision, state, resume_state, paused,
                    objective, original_request_redacted, task_envelope_json, created_at,
                    updated_at, archived_at)
                 VALUES (?1, ?2, 0, 'completed', NULL, 0, ?3, 'integrate', ?4, ?5, ?5, NULL)",
                params![
                    task_id.to_string(),
                    SchemaVersion::V1,
                    envelope.objective,
                    serde_json::to_string(&envelope)?,
                    now.to_rfc3339()
                ],
            )?;
            connection.execute(
                "INSERT INTO session_tasks(session_id, revision_id, task_id, node_key,
                    display_order, provider_id, model_profile)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'codex', 'standard')",
                params![
                    session_id.to_string(),
                    revision_id.to_string(),
                    task_id.to_string(),
                    format!("source-{index}"),
                    i64::try_from(index + 1).unwrap_or(i64::MAX)
                ],
            )?;
        }
        Ok(())
    })?;

    let worktree_manager = GitWorktreeManager::open(&repository, &state_root.join("worktrees"))?;
    let artifacts = ArtifactStore::open(&state_root)?;
    let checkpoints = CheckpointManager::new(artifacts);
    let mut source_paths = Vec::new();
    for (index, task_id) in task_ids.iter().enumerate() {
        let worktree = worktree_manager.create(*task_id, "HEAD").await?;
        let file = if index == 0 { "a.txt" } else { "b.txt" };
        fs::write(worktree.path.join(file), format!("integrated {file}\n"))?;
        let snapshot = worktree_manager.snapshot(&worktree).await?;
        database.record_active_worktree(&NewWorktreeRecord {
            task_id: *task_id,
            repo_root: repository.clone(),
            worktree_path: worktree.path.clone(),
            branch_name: worktree.branch.clone(),
            base_revision: worktree.base_revision.clone(),
            created_at: now,
        })?;
        let attempt_id = AttemptId::new();
        database.record_task_attempt_started(&NewTaskAttemptRecord {
            attempt_id,
            task_id: *task_id,
            provider: ProviderId::Codex,
            worker_mode: "integration-e2e".to_owned(),
            started_at: now,
        })?;
        let checkpoint = checkpoints.create(
            CheckpointInput {
                task_id: *task_id,
                attempt_id,
                objective: "integration fixture".to_owned(),
                current_plan: Vec::new(),
                completed_steps: Vec::new(),
                pending_steps: Vec::new(),
                files_read: Vec::new(),
                commands_run: Vec::new(),
                tests: Vec::new(),
                decisions: Vec::new(),
                unresolved_questions: Vec::new(),
                known_failures: Vec::new(),
                worker_claim: None,
                current_worker: ProviderId::Codex,
                concise_context_summary: "fixture".to_owned(),
                created_at: now,
            },
            GitCheckpointEvidence::from(&snapshot),
        )?;
        database.record_checkpoint(&checkpoint)?;
        database.record_verification(&verification(*task_id))?;
        source_paths.push(worktree.path);
    }

    let integration = IntegrationServices {
        manager: Arc::new(GitIntegrationManager::new(&repository, &state_root)?),
        repository_root: repository.clone(),
        state_root: state_root.clone(),
    };
    let services = PlanningServices {
        conversation: Arc::new(UnusedConversation),
        repository_root: repository.clone(),
        planner: Arc::new(UnusedPlanner),
        planner_provider: ProviderId::Codex,
        validation_policy: GraphValidationPolicy {
            eligible_providers: BTreeSet::from([ProviderId::Codex]),
            eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
            max_parallel_workers: 2,
            per_provider_limits: std::collections::BTreeMap::from([(ProviderId::Codex, 2)]),
        },
        integration: Some(integration),
    };
    let request = command(
        session_id,
        ClientCommandAction::RequestIntegration,
        serde_json::json!({}),
    );
    database.submit_client_command(&request)?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, now)
        .await?
        .ok_or("preview command was not processed")?;
    let preview = database
        .current_integration_batch(session_id)?
        .ok_or_else(|| {
            let stored = database
                .load_client_command_by_idempotency_key(&request.idempotency_key)
                .ok()
                .flatten();
            format!("preview missing; command={stored:?}")
        })?;
    assert_eq!(preview.status, IntegrationBatchStatus::Preview);
    assert!(preview.preview.is_approvable());
    let destination = state_root
        .join("integration")
        .join(preview.preview.batch_id.to_string());
    assert!(!destination.exists(), "preview must be read-only");

    let approval = command(
        session_id,
        ClientCommandAction::ApproveIntegration,
        serde_json::to_value(ApproveIntegrationCommandPayload {
            batch_id: preview.preview.batch_id,
            preview_hash: preview.preview.preview_hash.clone(),
            approved_by: "integration-e2e".to_owned(),
        })?,
    );
    database.submit_client_command(&approval)?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now())
        .await?
        .ok_or("approval command was not processed")?;
    let applied = database
        .current_integration_batch(session_id)?
        .ok_or("applied batch missing")?;
    assert_eq!(applied.status, IntegrationBatchStatus::Applied);
    assert_eq!(
        database
            .load_session(session_id)?
            .ok_or("session missing")?
            .state,
        SessionState::Completed
    );
    assert_eq!(fs::read_to_string(repository.join("a.txt"))?, "base a\n");
    assert_eq!(fs::read_to_string(repository.join("b.txt"))?, "base b\n");
    assert_eq!(
        fs::read_to_string(destination.join("a.txt"))?.trim(),
        "integrated a.txt"
    );
    assert_eq!(
        fs::read_to_string(destination.join("b.txt"))?.trim(),
        "integrated b.txt"
    );
    assert!(source_paths.iter().all(|path| path.exists()));
    Ok(())
}
