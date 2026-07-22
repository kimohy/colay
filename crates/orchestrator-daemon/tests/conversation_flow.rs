use std::{
    collections::{BTreeMap, BTreeSet},
    process::Command,
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use orchestrator_daemon::{
    MessageRedactor, PlanningServices, process_next_client_command,
    process_next_orchestration_command,
};
use orchestrator_domain::{
    AppendMessageCommandPayload, ApproveGraphCommandPayload, ClientCommand, ClientCommandAction,
    ClientCommandId, ClientCommandState, ConversationOutcome, GraphValidationPolicy, MessageId,
    ModelProfile, ProviderId, RequirementSnapshot, SandboxMode, SessionId,
};
use orchestrator_engine::{
    ConversationExit, ConversationFailure, ConversationOrchestrator, ConversationRequest,
    ConversationResponse, PlannerExit, PlannerFailure, PlannerRequest, PlannerResponse,
    TaskPlanner,
};
use orchestrator_state::{Database, GraphRevisionStatus};
use rusqlite::params;

struct IdentityRedactor;

impl MessageRedactor for IdentityRedactor {
    fn redact(&self, value: &str) -> String {
        value.to_owned()
    }
}

struct SecretRedactor;

impl MessageRedactor for SecretRedactor {
    fn redact(&self, value: &str) -> String {
        value.replace("secret-token", "[REDACTED]")
    }
}

struct FakeConversation {
    outcome: ConversationOutcome,
}

struct FailingConversation;

#[async_trait]
impl ConversationOrchestrator for FailingConversation {
    async fn converse(
        &self,
        _request: ConversationRequest,
    ) -> Result<ConversationResponse, ConversationFailure> {
        Err(ConversationFailure::Invocation {
            reason: "provider rejected secret-token".to_owned(),
            evidence_redacted: "secret-token".to_owned(),
        })
    }
}

#[async_trait]
impl ConversationOrchestrator for FakeConversation {
    async fn converse(
        &self,
        request: ConversationRequest,
    ) -> Result<ConversationResponse, ConversationFailure> {
        Ok(ConversationResponse {
            schema_version: orchestrator_domain::SchemaVersion::v1(),
            attempt_id: request.attempt_id,
            session_id: request.session_id,
            source_message_id: request.source_message_id,
            provider: ProviderId::Codex,
            sandbox: SandboxMode::ReadOnly,
            exit: ConversationExit::Succeeded,
            output_redacted: serde_json::to_vec(&self.outcome).unwrap_or_default(),
            evidence_redacted: "fake conversation".to_owned(),
        })
    }
}

struct FakePlanner;

#[async_trait]
impl TaskPlanner for FakePlanner {
    async fn propose(&self, request: PlannerRequest) -> Result<PlannerResponse, PlannerFailure> {
        Ok(PlannerResponse {
            schema_version: orchestrator_domain::SchemaVersion::v1(),
            session_id: request.session_id,
            goal_message_id: request.goal_message_id,
            provider: ProviderId::Codex,
            sandbox: SandboxMode::ReadOnly,
            exit: PlannerExit::Succeeded,
            output_redacted: serde_json::to_vec(&serde_json::json!({
                "schema_version": "1",
                "revision_id": request.revision_id,
                "session_id": request.session_id,
                "goal_message_id": request.goal_message_id,
                "planner_provider": "codex",
                "proposed_at": Utc::now(),
                "nodes": [{
                    "key": "fix", "title": "Fix", "objective": "fix the issue",
                    "dependencies": [], "constraints": ["local only"],
                    "acceptance_criteria": ["tests pass"], "provider": "codex",
                    "profile": "standard", "write_scopes": ["crates/example"],
                    "repository_wide_write_scope": false, "risks": [],
                    "parallel_safety": "isolated"
                }]
            }))
            .unwrap_or_default(),
            evidence_redacted: "fake planner".to_owned(),
        })
    }
}

fn database() -> Result<Database, Box<dyn std::error::Error>> {
    let database = Database::open_in_memory()?;
    database.migrate_with_backup(std::path::Path::new("unused"))?;
    Ok(database)
}

fn seed_session(database: &Database) -> Result<SessionId, Box<dyn std::error::Error>> {
    let session_id = SessionId::new();
    let now = Utc::now().to_rfc3339();
    database.with_connection(|connection| {
        connection.execute(
            "INSERT INTO sessions(session_id, schema_version, revision, title, state, created_at, updated_at)
             VALUES (?1, '1', 0, 'conversation', 'drafting', ?2, ?2)",
            params![session_id.to_string(), now],
        )?;
        Ok(())
    })?;
    Ok(session_id)
}

fn append_command(session_id: SessionId, content: &str) -> ClientCommand {
    let message_id = MessageId::new();
    ClientCommand {
        command_id: ClientCommandId::new(),
        session_id: Some(session_id),
        task_id: None,
        action: ClientCommandAction::AppendMessage,
        payload: serde_json::to_value(AppendMessageCommandPayload {
            message_id,
            content: content.to_owned(),
        })
        .unwrap_or_default(),
        idempotency_key: format!("append-{message_id}"),
        state: ClientCommandState::Pending,
        requested_by: "test".to_owned(),
        requested_at: Utc::now(),
        claimed_at: None,
        completed_at: None,
        outcome: None,
    }
}

fn services_with_conversation(
    repository_root: std::path::PathBuf,
    conversation: Arc<dyn ConversationOrchestrator>,
) -> PlanningServices {
    PlanningServices {
        conversation,
        repository_root,
        planner: Arc::new(FakePlanner),
        planner_provider: ProviderId::Codex,
        validation_policy: GraphValidationPolicy {
            eligible_providers: BTreeSet::from([ProviderId::Codex]),
            eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
            max_parallel_workers: 1,
            per_provider_limits: BTreeMap::from([(ProviderId::Codex, 1)]),
        },
        integration: None,
    }
}

fn services(repository_root: std::path::PathBuf, outcome: ConversationOutcome) -> PlanningServices {
    services_with_conversation(repository_root, Arc::new(FakeConversation { outcome }))
}

fn assert_zero_writable_rows(database: &Database) -> Result<(), Box<dyn std::error::Error>> {
    database.with_connection(|connection| {
        for table in [
            "tasks",
            "task_attempts",
            "worktrees",
            "coordinator_leases",
            "worker_leases",
        ] {
            let count: i64 =
                connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
            assert_eq!(count, 0, "unexpected pre-approval row in {table}");
        }
        Ok(())
    })?;
    Ok(())
}

fn git(repository: &std::path::Path, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(())
}

fn git_repository() -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("README.md"), "fixture\n")?;
    git(directory.path(), &["init"])?;
    git(
        directory.path(),
        &["config", "user.name", "Conversation Test"],
    )?;
    git(
        directory.path(),
        &["config", "user.email", "conversation@example.invalid"],
    )?;
    git(directory.path(), &["add", "."])?;
    git(directory.path(), &["commit", "-m", "fixture"])?;
    Ok(directory)
}

#[tokio::test]
async fn ordinary_answer_is_automatic_and_creates_no_writable_state()
-> Result<(), Box<dyn std::error::Error>> {
    let database = database()?;
    let session_id = seed_session(&database)?;
    database.submit_client_command(&append_command(session_id, "Why is Git needed?"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?
        .ok_or("append command was not processed")?;
    assert_zero_writable_rows(&database)?;

    let directory = tempfile::tempdir()?;
    let services = services(
        std::fs::canonicalize(directory.path())?,
        ConversationOutcome::AnswerComplete {
            response_redacted: "Git is only needed for approved writable execution.".to_owned(),
        },
    );
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now())
        .await?
        .ok_or("conversation command was not processed")?;
    let messages = database.messages_after(session_id, 0, 10)?;
    assert_eq!(messages.len(), 2);
    assert!(messages[1].1.content_redacted.contains("approved writable"));
    assert!(database.current_requirement_revision(session_id)?.is_none());
    assert_zero_writable_rows(&database)?;
    Ok(())
}

#[tokio::test]
async fn interview_records_partial_requirements_without_starting_a_plan()
-> Result<(), Box<dyn std::error::Error>> {
    let database = database()?;
    let session_id = seed_session(&database)?;
    database.submit_client_command(&append_command(session_id, "please improve the flow"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let directory = tempfile::tempdir()?;
    let services = services(
        std::fs::canonicalize(directory.path())?,
        ConversationOutcome::MoreInformationNeeded {
            response_redacted: "Which verification target should be required?".to_owned(),
            requirements: RequirementSnapshot {
                objective: "improve the flow".to_owned(),
                constraints: vec!["stay read-only before approval".to_owned()],
                acceptance_criteria: Vec::new(),
                verification_plan: Vec::new(),
                open_questions: vec!["Which verification target is required?".to_owned()],
            },
        },
    );
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;

    let requirement = database
        .current_requirement_revision(session_id)?
        .ok_or("missing partial requirement revision")?;
    assert!(!requirement.snapshot.is_complete());
    database.with_connection(|connection| {
        let plan_commands: i64 = connection.query_row(
            "SELECT count(*) FROM client_commands WHERE action = 'request_plan'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(plan_commands, 0);
        Ok(())
    })?;
    assert_zero_writable_rows(&database)?;
    Ok(())
}

#[tokio::test]
async fn provider_failure_is_redacted_and_preserves_the_session()
-> Result<(), Box<dyn std::error::Error>> {
    let database = database()?;
    let session_id = seed_session(&database)?;
    database.submit_client_command(&append_command(session_id, "hello"))?;
    process_next_client_command(&database, &SecretRedactor, Utc::now())?;
    let directory = tempfile::tempdir()?;
    let services = services_with_conversation(
        std::fs::canonicalize(directory.path())?,
        Arc::new(FailingConversation),
    );
    process_next_orchestration_command(&database, &services, &SecretRedactor, Utc::now()).await?;

    let messages = database.messages_after(session_id, 0, 10)?;
    assert_eq!(messages.len(), 2);
    assert!(
        messages[1]
            .1
            .content_redacted
            .contains("session were preserved")
    );
    database.with_connection(|connection| {
        let outcome: String = connection.query_row(
            "SELECT outcome_json FROM conversation_attempts LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        assert!(!outcome.contains("secret-token"));
        assert!(outcome.contains("[REDACTED]"));
        Ok(())
    })?;
    assert_zero_writable_rows(&database)?;
    Ok(())
}

#[tokio::test]
async fn complete_candidate_in_non_git_directory_preserves_chat_and_blocks_approval()
-> Result<(), Box<dyn std::error::Error>> {
    let database = database()?;
    let session_id = seed_session(&database)?;
    database.submit_client_command(&append_command(session_id, "candidate"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let directory = tempfile::tempdir()?;
    let services = services(
        std::fs::canonicalize(directory.path())?,
        ConversationOutcome::WorktreeTaskCandidate {
            response_redacted: "Ready for validation.".to_owned(),
            requirements: RequirementSnapshot {
                objective: "fix conversation flow".to_owned(),
                constraints: vec!["no task before approval".to_owned()],
                acceptance_criteria: vec!["tests pass".to_owned()],
                verification_plan: vec!["cargo test --workspace --all-features".to_owned()],
                open_questions: Vec::new(),
            },
        },
    );
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    let graph = database.current_graph(session_id)?.ok_or("missing graph")?;
    assert_eq!(graph.revision.status, GraphRevisionStatus::Invalid);
    assert!(graph.revision.proposal_hash.is_none());
    let messages = database.messages_after(session_id, 0, 10)?;
    assert!(
        messages
            .iter()
            .any(|(_, message)| { message.content_redacted.contains("Initialize Git") })
    );
    assert_zero_writable_rows(&database)?;
    Ok(())
}

#[tokio::test]
async fn validated_candidate_materializes_once_only_after_exact_approval()
-> Result<(), Box<dyn std::error::Error>> {
    let database = database()?;
    let session_id = seed_session(&database)?;
    database.submit_client_command(&append_command(session_id, "candidate"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let repository = git_repository()?;
    let services = services(
        std::fs::canonicalize(repository.path())?,
        ConversationOutcome::WorktreeTaskCandidate {
            response_redacted: "Ready for validation.".to_owned(),
            requirements: RequirementSnapshot {
                objective: "fix conversation flow".to_owned(),
                constraints: vec!["no task before approval".to_owned()],
                acceptance_criteria: vec!["tests pass".to_owned()],
                verification_plan: vec!["cargo test --workspace --all-features".to_owned()],
                open_questions: Vec::new(),
            },
        },
    );
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    let graph = database.current_graph(session_id)?.ok_or("missing graph")?;
    assert_eq!(graph.revision.status, GraphRevisionStatus::AwaitingApproval);
    let proposal_hash = graph
        .revision
        .proposal_hash
        .clone()
        .ok_or("missing validated hash")?;
    let summary: orchestrator_domain::GraphValidationSummary =
        serde_json::from_value(graph.revision.validation.clone())?;
    assert!(summary.authority.is_some());
    assert_zero_writable_rows(&database)?;

    let approval = ClientCommand {
        command_id: ClientCommandId::new(),
        session_id: Some(session_id),
        task_id: None,
        action: ClientCommandAction::ApproveGraph,
        payload: serde_json::to_value(ApproveGraphCommandPayload {
            revision_id: graph.revision.revision_id,
            proposal_hash,
            approved_by: "operator".to_owned(),
        })?,
        idempotency_key: "approve-validated-candidate".to_owned(),
        state: ClientCommandState::Pending,
        requested_by: "operator".to_owned(),
        requested_at: Utc::now(),
        claimed_at: None,
        completed_at: None,
        outcome: None,
    };
    database.submit_client_command(&approval)?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    database.submit_client_command(&approval)?;

    database.with_connection(|connection| {
        let tasks: i64 =
            connection.query_row("SELECT count(*) FROM tasks", [], |row| row.get(0))?;
        let worktrees: i64 =
            connection.query_row("SELECT count(*) FROM worktrees", [], |row| row.get(0))?;
        assert_eq!(tasks, 1);
        assert_eq!(worktrees, 0);
        Ok(())
    })?;
    Ok(())
}

#[tokio::test]
async fn repository_head_drift_rejects_approval_without_materializing_tasks()
-> Result<(), Box<dyn std::error::Error>> {
    let database = database()?;
    let session_id = seed_session(&database)?;
    database.submit_client_command(&append_command(session_id, "candidate"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let repository = git_repository()?;
    let services = services(
        std::fs::canonicalize(repository.path())?,
        ConversationOutcome::WorktreeTaskCandidate {
            response_redacted: "Ready for validation.".to_owned(),
            requirements: RequirementSnapshot {
                objective: "fix conversation flow".to_owned(),
                constraints: vec!["no task before approval".to_owned()],
                acceptance_criteria: vec!["tests pass".to_owned()],
                verification_plan: vec!["cargo test --workspace --all-features".to_owned()],
                open_questions: Vec::new(),
            },
        },
    );
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    let graph = database.current_graph(session_id)?.ok_or("missing graph")?;
    let proposal_hash = graph
        .revision
        .proposal_hash
        .clone()
        .ok_or("missing validated hash")?;

    std::fs::write(repository.path().join("README.md"), "changed\n")?;
    git(repository.path(), &["add", "."])?;
    git(repository.path(), &["commit", "-m", "drift"])?;

    let command_id = ClientCommandId::new();
    database.submit_client_command(&ClientCommand {
        command_id,
        session_id: Some(session_id),
        task_id: None,
        action: ClientCommandAction::ApproveGraph,
        payload: serde_json::to_value(ApproveGraphCommandPayload {
            revision_id: graph.revision.revision_id,
            proposal_hash,
            approved_by: "operator".to_owned(),
        })?,
        idempotency_key: "reject-drifted-approval".to_owned(),
        state: ClientCommandState::Pending,
        requested_by: "operator".to_owned(),
        requested_at: Utc::now(),
        claimed_at: None,
        completed_at: None,
        outcome: None,
    })?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;

    let stored = database
        .load_client_command(command_id)?
        .ok_or("missing approval command")?;
    assert_eq!(stored.state, ClientCommandState::Failed);
    assert!(stored.outcome.unwrap_or_default().contains("HEAD changed"));
    assert_zero_writable_rows(&database)?;
    Ok(())
}
