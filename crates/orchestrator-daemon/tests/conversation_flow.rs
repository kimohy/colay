use std::{
    collections::{BTreeMap, BTreeSet},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use chrono::Utc;
use orchestrator_daemon::{
    DaemonSettings, MessageRedactor, PlanningServices, process_next_client_command,
    process_next_orchestration_command, serve_with_orchestration,
};
use orchestrator_domain::{
    AppendMessageCommandPayload, ApproveGraphCommandPayload, ClientCommand, ClientCommandAction,
    ClientCommandId, ClientCommandState, ConversationAttemptId, ConversationOutcome,
    DaemonInstanceId, GraphValidationPolicy, MessageId, ModelProfile, ProviderId,
    RequirementSnapshot, SandboxMode, SessionId, SessionState, VerificationCommand,
};
use orchestrator_engine::{
    CONVERSATION_MAX_EVIDENCE_BYTES, ConversationExit, ConversationFailure,
    ConversationOrchestrator, ConversationRequest, ConversationResponse, PlannerExit,
    PlannerFailure, PlannerRequest, PlannerResponse, TaskPlanner,
};
use orchestrator_state::{
    ConversationAttemptStatus, Database, GraphRevisionStatus, NewConversationAttempt,
    WorkspaceDatabase, WorkspaceId,
};
use rusqlite::params;
use tokio_util::sync::CancellationToken;

mod support;
use support::with_workspace;

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

struct SecretCommandEvidenceConversation;

#[derive(Clone)]
enum FailureFixture {
    Error(ConversationFailure),
    Response {
        exit: ConversationExit,
        output_redacted: Vec<u8>,
        evidence_redacted: String,
    },
}

struct ProviderFailureCase {
    name: &'static str,
    fixture: FailureFixture,
    expected_status: ConversationAttemptStatus,
    expected_action: &'static str,
    expected_evidence: &'static str,
}

struct ProviderFailureRun {
    database: Database,
    workspace_id: WorkspaceId,
    database_path: std::path::PathBuf,
    session_id: SessionId,
    attempt_id: ConversationAttemptId,
    command_id: ClientCommandId,
    services: PlanningServices,
    starts: Arc<AtomicUsize>,
}

struct FailingConversation {
    fixture: FailureFixture,
    starts: Arc<AtomicUsize>,
}

struct CapturingConversation {
    transcript: Arc<Mutex<Option<String>>>,
}

struct ProviderAwareConversation;

fn provider_failure_cases() -> Vec<ProviderFailureCase> {
    vec![
        ProviderFailureCase {
            name: "authentication",
            fixture: FailureFixture::Error(ConversationFailure::Invocation {
                reason: "token_expired secret-token".to_owned(),
                evidence_redacted: "credential secret-token expired".to_owned(),
            }),
            expected_status: ConversationAttemptStatus::Failed,
            expected_action: "authenticate",
            expected_evidence: "credential [REDACTED] expired",
        },
        ProviderFailureCase {
            name: "quota",
            fixture: FailureFixture::Response {
                exit: ConversationExit::QuotaExhausted,
                output_redacted: Vec::new(),
                evidence_redacted: "Credit balance is too low".to_owned(),
            },
            expected_status: ConversationAttemptStatus::Failed,
            expected_action: "quota or billing",
            expected_evidence: "Credit balance is too low",
        },
        ProviderFailureCase {
            name: "unsupported client",
            fixture: FailureFixture::Error(ConversationFailure::Invocation {
                reason: "UNSUPPORTED_CLIENT".to_owned(),
                evidence_redacted: "account rejected this client".to_owned(),
            }),
            expected_status: ConversationAttemptStatus::Failed,
            expected_action: "not supported",
            expected_evidence: "account rejected this client",
        },
        ProviderFailureCase {
            name: "timeout",
            fixture: FailureFixture::Response {
                exit: ConversationExit::TimedOut,
                output_redacted: Vec::new(),
                evidence_redacted: "provider exceeded its deadline".to_owned(),
            },
            expected_status: ConversationAttemptStatus::Failed,
            expected_action: "timed out",
            expected_evidence: "provider exceeded its deadline",
        },
        ProviderFailureCase {
            name: "cancellation",
            fixture: FailureFixture::Response {
                exit: ConversationExit::Cancelled,
                output_redacted: Vec::new(),
                evidence_redacted: "request was cancelled".to_owned(),
            },
            expected_status: ConversationAttemptStatus::Cancelled,
            expected_action: "cancelled",
            expected_evidence: "request was cancelled",
        },
        ProviderFailureCase {
            name: "malformed output",
            fixture: FailureFixture::Response {
                exit: ConversationExit::Succeeded,
                output_redacted: b"not-json".to_vec(),
                evidence_redacted: "provider returned malformed output".to_owned(),
            },
            expected_status: ConversationAttemptStatus::Failed,
            expected_action: "incompatible",
            expected_evidence: "provider returned malformed output",
        },
        ProviderFailureCase {
            name: "nonzero exit",
            fixture: FailureFixture::Response {
                exit: ConversationExit::Crashed {
                    exit_code: Some(17),
                },
                output_redacted: Vec::new(),
                evidence_redacted:
                    "provider exited 17\nat internal_one (provider.js:1:1)\nat internal_two (provider.js:2:1)"
                        .to_owned(),
            },
            expected_status: ConversationAttemptStatus::Failed,
            expected_action: "process failed",
            expected_evidence: "provider exited 17",
        },
    ]
}

fn verification_plan() -> Vec<VerificationCommand> {
    vec![VerificationCommand {
        executable: "cargo".to_owned(),
        args: vec![
            "test".to_owned(),
            "--workspace".to_owned(),
            "--all-features".to_owned(),
        ],
    }]
}

fn candidate_outcome() -> ConversationOutcome {
    ConversationOutcome::WorktreeTaskCandidate {
        response_redacted: "Ready for validation.".to_owned(),
        requirements: RequirementSnapshot {
            objective: "fix conversation flow".to_owned(),
            in_scope: vec!["conversation flow".to_owned()],
            out_of_scope: vec!["automatic merge".to_owned()],
            constraints: vec!["no task before approval".to_owned()],
            acceptance_criteria: vec!["tests pass".to_owned()],
            verification_plan: verification_plan(),
            risks: vec!["stale approval".to_owned()],
            open_questions: Vec::new(),
        },
    }
}

#[async_trait]
impl ConversationOrchestrator for FailingConversation {
    async fn converse(
        &self,
        request: ConversationRequest,
    ) -> Result<ConversationResponse, ConversationFailure> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        match self.fixture.clone() {
            FailureFixture::Error(error) => Err(error),
            FailureFixture::Response {
                exit,
                output_redacted,
                evidence_redacted,
            } => Ok(ConversationResponse {
                schema_version: orchestrator_domain::SchemaVersion::v1(),
                attempt_id: request.attempt_id,
                session_id: request.session_id,
                source_message_id: request.source_message_id,
                provider: request.provider,
                sandbox: SandboxMode::ReadOnly,
                exit,
                output_redacted,
                evidence_redacted,
            }),
        }
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

#[async_trait]
impl ConversationOrchestrator for SecretCommandEvidenceConversation {
    async fn converse(
        &self,
        request: ConversationRequest,
    ) -> Result<ConversationResponse, ConversationFailure> {
        Ok(ConversationResponse {
            schema_version: orchestrator_domain::SchemaVersion::v1(),
            attempt_id: request.attempt_id,
            session_id: request.session_id,
            source_message_id: request.source_message_id,
            provider: request.provider,
            sandbox: SandboxMode::ReadOnly,
            exit: ConversationExit::Succeeded,
            output_redacted: serde_json::to_vec(&ConversationOutcome::AnswerComplete {
                response_redacted: "safe answer".to_owned(),
            })
            .unwrap_or_default(),
            evidence_redacted: format!(
                "read-only provider command started: executable=/bin/sh; args={}",
                "secret-token".repeat(CONVERSATION_MAX_EVIDENCE_BYTES)
            ),
        })
    }
}

#[async_trait]
impl ConversationOrchestrator for CapturingConversation {
    async fn converse(
        &self,
        request: ConversationRequest,
    ) -> Result<ConversationResponse, ConversationFailure> {
        *self
            .transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(request.transcript_redacted.clone());
        Ok(ConversationResponse {
            schema_version: orchestrator_domain::SchemaVersion::v1(),
            attempt_id: request.attempt_id,
            session_id: request.session_id,
            source_message_id: request.source_message_id,
            provider: ProviderId::Codex,
            sandbox: SandboxMode::ReadOnly,
            exit: ConversationExit::Succeeded,
            output_redacted: serde_json::to_vec(&ConversationOutcome::AnswerComplete {
                response_redacted: "captured".to_owned(),
            })
            .unwrap_or_default(),
            evidence_redacted: "captured transcript".to_owned(),
        })
    }
}

#[async_trait]
impl ConversationOrchestrator for ProviderAwareConversation {
    async fn converse(
        &self,
        request: ConversationRequest,
    ) -> Result<ConversationResponse, ConversationFailure> {
        Ok(ConversationResponse {
            schema_version: orchestrator_domain::SchemaVersion::v1(),
            attempt_id: request.attempt_id,
            session_id: request.session_id,
            source_message_id: request.source_message_id,
            provider: request.provider,
            sandbox: SandboxMode::ReadOnly,
            exit: ConversationExit::Succeeded,
            output_redacted: serde_json::to_vec(&ConversationOutcome::AnswerComplete {
                response_redacted: format!("fake-provider:{}", request.provider),
            })
            .unwrap_or_default(),
            evidence_redacted: format!("fake-provider:{}", request.provider),
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

fn database() -> Result<(Database, WorkspaceId, std::path::PathBuf), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let root = std::fs::canonicalize(temporary.path())?;
    let _persisted = temporary.keep();
    let database_path = root.join("state.db");
    let database = Database::open(&database_path)?;
    database.migrate_with_backup(&root.join("backups"))?;
    let workspace_path = root.join("workspace");
    std::fs::create_dir_all(&workspace_path)?;
    let workspace_id = database
        .resolve_repository_workspace(&workspace_path)?
        .workspace_id;
    Ok((database, workspace_id, database_path))
}

fn seed_session(
    database_path: &std::path::Path,
    database: &WorkspaceDatabase<'_>,
) -> Result<SessionId, Box<dyn std::error::Error>> {
    let session_id = SessionId::new();
    let now = Utc::now().to_rfc3339();
    with_workspace(database_path, database, |connection| {
        connection.execute(
            "INSERT INTO main.sessions(workspace_id, session_id, schema_version, revision, title, state, created_at, updated_at)
             VALUES (current_workspace(), ?1, '1', 0, 'conversation', 'drafting', ?2, ?2)",
            params![session_id.to_string(), now],
        )?;
        Ok(())
    })?;
    Ok(session_id)
}

fn append_command(session_id: SessionId, content: &str) -> ClientCommand {
    append_command_with_provider(session_id, content, None)
}

fn append_command_with_provider(
    session_id: SessionId,
    content: &str,
    requested_provider: Option<ProviderId>,
) -> ClientCommand {
    let message_id = MessageId::new();
    ClientCommand {
        command_id: ClientCommandId::new(),
        session_id: Some(session_id),
        task_id: None,
        action: ClientCommandAction::AppendMessage,
        payload: serde_json::to_value(AppendMessageCommandPayload {
            message_id,
            content: content.to_owned(),
            requested_provider,
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
        conversation_providers: vec![ProviderId::Codex],
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

fn assert_zero_writable_rows(
    database_path: &std::path::Path,
    database: &WorkspaceDatabase<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    with_workspace(database_path, database, |connection| {
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
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    database.submit_client_command(&append_command(session_id, "Why is Git needed?"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?
        .ok_or("append command was not processed")?;
    assert_zero_writable_rows(&database_path, &database)?;

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
    assert_zero_writable_rows(&database_path, &database)?;
    Ok(())
}

#[tokio::test]
async fn requested_provider_uses_the_eligible_requested_provider_before_creating_an_attempt()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    let append = append_command_with_provider(session_id, "inspect", Some(ProviderId::Claude));
    let source_message_id =
        serde_json::from_value::<AppendMessageCommandPayload>(append.payload.clone())?.message_id;
    database.submit_client_command(&append)?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let mut services = services_with_conversation(
        tempfile::tempdir()?.path().to_path_buf(),
        Arc::new(ProviderAwareConversation),
    );
    services.conversation_providers =
        vec![ProviderId::Codex, ProviderId::Claude, ProviderId::Gemini];

    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;

    let attempt_id = ConversationAttemptId::from_uuid(source_message_id.into_uuid());
    assert_eq!(
        database
            .load_conversation_attempt(attempt_id)?
            .ok_or("conversation attempt is missing")?
            .provider,
        ProviderId::Claude
    );
    let messages = database.messages_after(session_id, 0, 10)?;
    assert_eq!(messages[1].1.content_redacted, "fake-provider:claude");
    Ok(())
}

#[tokio::test]
async fn requested_provider_falls_back_before_creating_an_attempt_when_unavailable()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    let append = append_command_with_provider(session_id, "inspect", Some(ProviderId::Agy));
    let source_message_id =
        serde_json::from_value::<AppendMessageCommandPayload>(append.payload.clone())?.message_id;
    database.submit_client_command(&append)?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let mut services = services_with_conversation(
        tempfile::tempdir()?.path().to_path_buf(),
        Arc::new(ProviderAwareConversation),
    );
    services.conversation_providers =
        vec![ProviderId::Codex, ProviderId::Claude, ProviderId::Gemini];

    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;

    let attempt_id = ConversationAttemptId::from_uuid(source_message_id.into_uuid());
    assert_eq!(
        database
            .load_conversation_attempt(attempt_id)?
            .ok_or("conversation attempt is missing")?
            .provider,
        ProviderId::Codex
    );
    let messages = database.messages_after(session_id, 0, 10)?;
    assert_eq!(
        messages[1].1.content_redacted,
        "Requested provider agy is unavailable; using codex for this read-only turn.\nfake-provider:codex"
    );
    Ok(())
}

#[tokio::test]
async fn unavailable_activation_shape_with_no_eligible_provider_creates_no_attempt_or_notice()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    let append = append_command_with_provider(session_id, "inspect", Some(ProviderId::Agy));
    let source_message_id =
        serde_json::from_value::<AppendMessageCommandPayload>(append.payload.clone())?.message_id;
    database.submit_client_command(&append)?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let mut services = services_with_conversation(
        tempfile::tempdir()?.path().to_path_buf(),
        Arc::new(ProviderAwareConversation),
    );
    services.conversation_providers = Vec::new();

    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;

    let attempt_id = ConversationAttemptId::from_uuid(source_message_id.into_uuid());
    assert!(database.load_conversation_attempt(attempt_id)?.is_none());
    let messages = database.messages_after(session_id, 0, 10)?;
    assert_eq!(messages.len(), 1);
    let command_id = ClientCommandId::from_uuid(source_message_id.into_uuid());
    assert_eq!(
        database
            .load_client_command(command_id)?
            .ok_or("conversation command is missing")?
            .state,
        ClientCommandState::Failed
    );
    Ok(())
}

#[tokio::test]
async fn provider_transcript_includes_latest_message_after_two_hundred_turns()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    let now = Utc::now().to_rfc3339();
    with_workspace(&database_path, &database, |connection| {
        for ordinal in 1..=204 {
            let content = if ordinal == 1 {
                "first-dropped-marker".to_owned()
            } else {
                format!("old-message-{ordinal}")
            };
            connection.execute(
                "INSERT INTO main.conversation_messages(
                    workspace_id, message_id, session_id, task_id, ordinal, role, kind, state,
                    content_redacted, created_at, finalized_at)
                 VALUES (current_workspace(), ?1, ?2, NULL, ?3, 'user', 'user_message', 'final',
                         ?4, ?5, ?5)",
                params![
                    MessageId::new().to_string(),
                    session_id.to_string(),
                    ordinal,
                    content,
                    now
                ],
            )?;
        }
        Ok(())
    })?;
    database.submit_client_command(&append_command(session_id, "latest-source-marker"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let transcript = Arc::new(Mutex::new(None));
    let services = services_with_conversation(
        tempfile::tempdir()?.path().to_path_buf(),
        Arc::new(CapturingConversation {
            transcript: Arc::clone(&transcript),
        }),
    );

    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;

    let transcript = transcript
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .ok_or("provider did not receive a transcript")?;
    assert!(transcript.contains("latest-source-marker"));
    assert!(!transcript.contains("first-dropped-marker"));
    Ok(())
}

#[tokio::test]
async fn interview_records_partial_requirements_without_starting_a_plan()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    database.submit_client_command(&append_command(session_id, "please improve the flow"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let directory = tempfile::tempdir()?;
    let services = services(
        std::fs::canonicalize(directory.path())?,
        ConversationOutcome::MoreInformationNeeded {
            response_redacted: "Which verification target should be required?".to_owned(),
            requirements: RequirementSnapshot {
                objective: "improve the flow".to_owned(),
                in_scope: vec!["conversation flow".to_owned()],
                out_of_scope: Vec::new(),
                constraints: vec!["stay read-only before approval".to_owned()],
                acceptance_criteria: Vec::new(),
                verification_plan: Vec::new(),
                risks: Vec::new(),
                open_questions: vec!["Which verification target is required?".to_owned()],
            },
        },
    );
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;

    let requirement = database
        .current_requirement_revision(session_id)?
        .ok_or("missing partial requirement revision")?;
    assert!(!requirement.snapshot.is_complete());
    with_workspace(&database_path, &database, |connection| {
        let plan_commands: i64 = connection.query_row(
            "SELECT count(*) FROM client_commands WHERE action = 'request_plan'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(plan_commands, 0);
        Ok(())
    })?;
    assert_zero_writable_rows(&database_path, &database)?;
    Ok(())
}

async fn start_provider_failure_case(
    case: &ProviderFailureCase,
) -> Result<ProviderFailureRun, Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let workspace = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &workspace)?;
    let append = append_command(session_id, case.name);
    let source_message_id =
        serde_json::from_value::<AppendMessageCommandPayload>(append.payload.clone())?.message_id;
    workspace.submit_client_command(&append)?;
    process_next_client_command(&workspace, &SecretRedactor, Utc::now())?;
    let starts = Arc::new(AtomicUsize::new(0));
    let directory = tempfile::tempdir()?;
    let services = services_with_conversation(
        std::fs::canonicalize(directory.path())?,
        Arc::new(FailingConversation {
            fixture: case.fixture.clone(),
            starts: Arc::clone(&starts),
        }),
    );
    process_next_orchestration_command(&workspace, &services, &SecretRedactor, Utc::now()).await?;
    let source_uuid = source_message_id.into_uuid();
    Ok(ProviderFailureRun {
        database,
        workspace_id,
        database_path,
        session_id,
        attempt_id: ConversationAttemptId::from_uuid(source_uuid),
        command_id: ClientCommandId::from_uuid(source_uuid),
        services,
        starts,
    })
}

fn assert_terminal_provider_failure(
    run: &ProviderFailureRun,
    case: &ProviderFailureCase,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = run.database.workspace(run.workspace_id);
    let attempt = workspace
        .load_conversation_attempt(run.attempt_id)?
        .ok_or("conversation attempt is missing")?;
    assert_eq!(
        attempt.status, case.expected_status,
        "fixture: {}",
        case.name
    );
    let error = attempt.error_redacted.ok_or("missing redacted error")?;
    assert!(!error.contains("secret-token"), "fixture: {}", case.name);
    assert!(error.len() <= CONVERSATION_MAX_EVIDENCE_BYTES);
    assert!(
        error.contains(case.expected_action),
        "fixture: {}: {error}",
        case.name
    );
    assert!(!error.contains("Evidence:"), "fixture: {}", case.name);
    assert!(
        !error.contains(case.expected_evidence),
        "fixture: {} leaked detailed evidence: {error}",
        case.name
    );
    let outcome = attempt.outcome.ok_or("missing recovery outcome")?;
    let ConversationOutcome::NeedsAttention {
        response_redacted,
        evidence_redacted,
    } = outcome
    else {
        return Err(format!("fixture {} did not store needs_attention", case.name).into());
    };
    assert!(
        response_redacted.contains(case.expected_action),
        "fixture: {}",
        case.name
    );
    assert!(
        !evidence_redacted.contains("secret-token"),
        "fixture: {}",
        case.name
    );
    assert!(evidence_redacted.len() <= CONVERSATION_MAX_EVIDENCE_BYTES);
    assert!(
        evidence_redacted.contains(case.expected_evidence),
        "fixture: {} lost detailed evidence: {evidence_redacted}",
        case.name
    );

    let command = workspace
        .load_client_command(run.command_id)?
        .ok_or("conversation command is missing")?;
    assert_eq!(
        command.state,
        ClientCommandState::Failed,
        "fixture: {}",
        case.name
    );
    let command_outcome = command.outcome.unwrap_or_default();
    assert!(
        command_outcome.contains(case.expected_action),
        "fixture: {}",
        case.name
    );
    assert!(!command_outcome.contains("Evidence:"));
    assert!(!command_outcome.contains(case.expected_evidence));
    let messages = workspace.messages_after(run.session_id, 0, 10)?;
    assert_eq!(messages.len(), 2, "fixture: {}", case.name);
    assert_eq!(messages[1].1.content_redacted, response_redacted);
    Ok(())
}

async fn assert_terminal_provider_failure_replay(
    run: &mut ProviderFailureRun,
    case: &ProviderFailureCase,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = run.database.workspace(run.workspace_id);
    with_workspace(&run.database_path, &workspace, |connection| {
        connection.execute(
            "UPDATE client_commands
             SET state = 'pending', claimed_at = NULL, completed_at = NULL, outcome = NULL
             WHERE command_id = ?1",
            [run.command_id.to_string()],
        )?;
        Ok(())
    })?;
    run.services.conversation_providers.clear();
    process_next_orchestration_command(&workspace, &run.services, &SecretRedactor, Utc::now())
        .await?;
    let replayed_command = workspace
        .load_client_command(run.command_id)?
        .ok_or("replayed conversation command is missing")?;
    assert_eq!(
        replayed_command.state,
        ClientCommandState::Failed,
        "fixture: {}",
        case.name
    );
    let replayed_outcome = replayed_command.outcome.unwrap_or_default();
    assert!(
        replayed_outcome.contains(case.expected_action),
        "fixture: {}",
        case.name
    );
    assert!(!replayed_outcome.contains("Evidence:"));
    assert!(!replayed_outcome.contains(case.expected_evidence));
    assert_eq!(
        workspace.messages_after(run.session_id, 0, 10)?.len(),
        2,
        "fixture: {}",
        case.name
    );
    assert_zero_writable_rows(&run.database_path, &workspace)?;
    assert_eq!(
        run.starts.load(Ordering::SeqCst),
        1,
        "fixture: {}",
        case.name
    );
    Ok(())
}

#[tokio::test]
async fn provider_failures_are_terminal_actionable_and_preserve_the_session()
-> Result<(), Box<dyn std::error::Error>> {
    for case in provider_failure_cases() {
        let mut run = start_provider_failure_case(&case).await?;
        assert_terminal_provider_failure(&run, &case)?;
        assert_terminal_provider_failure_replay(&mut run, &case).await?;
    }
    Ok(())
}

#[tokio::test]
async fn successful_command_evidence_is_redacted_and_bounded_before_persistence()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let workspace = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &workspace)?;
    let append = append_command(session_id, "inspect safely");
    let source_message_id =
        serde_json::from_value::<AppendMessageCommandPayload>(append.payload.clone())?.message_id;
    workspace.submit_client_command(&append)?;
    process_next_client_command(&workspace, &SecretRedactor, Utc::now())?;
    let services = services_with_conversation(
        tempfile::tempdir()?.path().to_path_buf(),
        Arc::new(SecretCommandEvidenceConversation),
    );

    process_next_orchestration_command(&workspace, &services, &SecretRedactor, Utc::now()).await?;

    let attempt_id = ConversationAttemptId::from_uuid(source_message_id.into_uuid());
    let attempt = workspace
        .load_conversation_attempt(attempt_id)?
        .ok_or("conversation attempt is missing")?;
    let evidence = attempt
        .evidence_redacted
        .ok_or("successful command evidence is missing")?;
    assert!(evidence.contains("read-only provider command started"));
    assert!(evidence.contains("[REDACTED]"));
    assert!(!evidence.contains("secret-token"));
    assert!(evidence.len() <= CONVERSATION_MAX_EVIDENCE_BYTES);
    assert_eq!(
        attempt.outcome,
        Some(ConversationOutcome::AnswerComplete {
            response_redacted: "safe answer".to_owned(),
        })
    );
    assert_zero_writable_rows(&database_path, &workspace)?;
    Ok(())
}

#[tokio::test]
async fn provider_failure_replay_normalizes_legacy_terminal_errors_without_starting_provider()
-> Result<(), Box<dyn std::error::Error>> {
    for legacy_error in [
        String::new(),
        "x".repeat(CONVERSATION_MAX_EVIDENCE_BYTES * 2),
        "\u{d55c}".repeat(CONVERSATION_MAX_EVIDENCE_BYTES),
    ] {
        let (database, workspace_id, database_path) = database()?;
        let database = database.workspace(workspace_id);
        let session_id = seed_session(&database_path, &database)?;
        let append = append_command(session_id, "replay legacy failure");
        let source_message_id =
            serde_json::from_value::<AppendMessageCommandPayload>(append.payload.clone())?
                .message_id;
        database.submit_client_command(&append)?;
        process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
        let command_id = ClientCommandId::from_uuid(source_message_id.into_uuid());
        let command = database
            .load_client_command(command_id)?
            .ok_or("conversation command is missing")?;
        let attempt_id = ConversationAttemptId::from_uuid(command_id.into_uuid());
        database.begin_conversation_attempt(&NewConversationAttempt {
            attempt_id,
            session_id,
            source_message_id,
            provider: ProviderId::Codex,
            started_at: command.requested_at,
        })?;
        let stored_outcome = ConversationOutcome::NeedsAttention {
            response_redacted: "Reconnect the provider, then retry this conversation.".to_owned(),
            evidence_redacted: "legacy provider failure".to_owned(),
        };
        with_workspace(&database_path, &database, |connection| {
            connection.execute_batch("PRAGMA ignore_check_constraints = ON;")?;
            connection.execute(
                "UPDATE conversation_attempts
                 SET status = 'failed', outcome_json = ?1, error_redacted = ?2,
                     completed_at = ?3
                 WHERE attempt_id = ?4 AND status = 'running'",
                params![
                    serde_json::to_string(&stored_outcome)?,
                    legacy_error,
                    Utc::now().to_rfc3339(),
                    attempt_id.to_string(),
                ],
            )?;
            connection.execute_batch("PRAGMA ignore_check_constraints = OFF;")?;
            Ok(())
        })?;

        let starts = Arc::new(AtomicUsize::new(0));
        let mut services = services_with_conversation(
            tempfile::tempdir()?.path().to_path_buf(),
            Arc::new(FailingConversation {
                fixture: FailureFixture::Error(ConversationFailure::Invocation {
                    reason: "must not run".to_owned(),
                    evidence_redacted: "must not run".to_owned(),
                }),
                starts: Arc::clone(&starts),
            }),
        );
        services.conversation_providers.clear();
        process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now())
            .await?;

        let replayed = database
            .load_client_command(command_id)?
            .ok_or("replayed conversation command is missing")?;
        assert_eq!(replayed.state, ClientCommandState::Failed);
        let replayed_error = replayed.outcome.ok_or("replayed error is missing")?;
        assert!(!replayed_error.trim().is_empty());
        assert!(replayed_error.len() <= CONVERSATION_MAX_EVIDENCE_BYTES);
        assert!(replayed_error.contains("Reconnect the provider"));
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert_eq!(database.messages_after(session_id, 0, 10)?.len(), 2);
        assert_zero_writable_rows(&database_path, &database)?;
    }
    Ok(())
}

#[tokio::test]
async fn complete_candidate_in_non_git_directory_preserves_plan_until_writable_approval()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    database.submit_client_command(&append_command(session_id, "candidate"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let directory = tempfile::tempdir()?;
    let services = services(
        std::fs::canonicalize(directory.path())?,
        ConversationOutcome::WorktreeTaskCandidate {
            response_redacted: "Ready for validation.".to_owned(),
            requirements: RequirementSnapshot {
                objective: "fix conversation flow".to_owned(),
                in_scope: vec!["conversation flow".to_owned()],
                out_of_scope: vec!["automatic merge".to_owned()],
                constraints: vec!["no task before approval".to_owned()],
                acceptance_criteria: vec!["tests pass".to_owned()],
                verification_plan: verification_plan(),
                risks: vec!["stale approval".to_owned()],
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
        .ok_or("missing proposal hash")?;
    let authority = serde_json::from_value::<orchestrator_domain::GraphValidationSummary>(
        graph.revision.validation.clone(),
    )?
    .authority
    .ok_or("missing deferred graph authority")?;
    let approval_id = ClientCommandId::new();
    database.submit_client_command(&ClientCommand {
        command_id: approval_id,
        session_id: Some(session_id),
        task_id: None,
        action: ClientCommandAction::ApproveGraph,
        payload: serde_json::to_value(ApproveGraphCommandPayload {
            revision_id: graph.revision.revision_id,
            requirement_revision_id: authority.requirement_revision_id,
            validation_hash: authority.validation_hash,
            base_commit: authority.base_commit,
            proposal_hash,
            approved_by: "operator".to_owned(),
        })?,
        idempotency_key: "non-git-writable-approval".to_owned(),
        state: ClientCommandState::Pending,
        requested_by: "operator".to_owned(),
        requested_at: Utc::now(),
        claimed_at: None,
        completed_at: None,
        outcome: None,
    })?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    let approval = database
        .load_client_command(approval_id)?
        .ok_or("missing approval command")?;
    assert_eq!(approval.state, ClientCommandState::Failed);
    assert!(
        approval
            .outcome
            .unwrap_or_default()
            .contains("committed Git repository")
    );
    assert_zero_writable_rows(&database_path, &database)?;
    Ok(())
}

#[tokio::test]
async fn planning_is_structural_when_writable_policy_is_ineligible()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    database.submit_client_command(&append_command(session_id, "candidate"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let repository = git_repository()?;
    let mut services = services(
        std::fs::canonicalize(repository.path())?,
        candidate_outcome(),
    );
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    services.validation_policy.eligible_providers.clear();
    services.validation_policy.eligible_profiles.clear();

    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;

    let graph = database.current_graph(session_id)?.ok_or("missing graph")?;
    assert_eq!(graph.revision.status, GraphRevisionStatus::AwaitingApproval);
    assert_zero_writable_rows(&database_path, &database)?;
    Ok(())
}

#[tokio::test]
async fn approval_revalidates_complete_current_policy_before_materializing()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    database.submit_client_command(&append_command(session_id, "candidate"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let repository = git_repository()?;
    let mut services = services(
        std::fs::canonicalize(repository.path())?,
        candidate_outcome(),
    );
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    let graph = database.current_graph(session_id)?.ok_or("missing graph")?;
    let summary: orchestrator_domain::GraphValidationSummary =
        serde_json::from_value(graph.revision.validation.clone())?;
    let authority = summary.authority.ok_or("missing graph authority")?;
    let approval = ClientCommand {
        command_id: ClientCommandId::new(),
        session_id: Some(session_id),
        task_id: None,
        action: ClientCommandAction::ApproveGraph,
        payload: serde_json::to_value(ApproveGraphCommandPayload {
            revision_id: graph.revision.revision_id,
            requirement_revision_id: authority.requirement_revision_id,
            validation_hash: authority.validation_hash,
            base_commit: authority.base_commit,
            proposal_hash: graph
                .revision
                .proposal_hash
                .ok_or("missing proposal hash")?,
            approved_by: "operator".to_owned(),
        })?,
        idempotency_key: "reject-invalid-current-policy".to_owned(),
        state: ClientCommandState::Pending,
        requested_by: "operator".to_owned(),
        requested_at: Utc::now(),
        claimed_at: None,
        completed_at: None,
        outcome: None,
    };
    database.submit_client_command_with_invocation(&approval, false)?;
    services.validation_policy.max_parallel_workers = 0;
    services
        .validation_policy
        .per_provider_limits
        .insert(ProviderId::Codex, 0);

    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;

    let stored = database
        .load_client_command(approval.command_id)?
        .ok_or("missing approval")?;
    assert_eq!(stored.state, ClientCommandState::Failed);
    assert!(
        stored
            .outcome
            .unwrap_or_default()
            .contains("current writable policy")
    );
    assert_zero_writable_rows(&database_path, &database)?;
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn plan_only_invocation_rejects_spoofed_approval_then_later_explicit_approval_succeeds()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    let mut append = append_command(session_id, "candidate");
    append.requested_by = "spoofable-client-field".to_owned();
    let message_id =
        serde_json::from_value::<AppendMessageCommandPayload>(append.payload.clone())?.message_id;
    database.submit_client_command_with_invocation(&append, true)?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let repository = git_repository()?;
    let services = services(
        std::fs::canonicalize(repository.path())?,
        ConversationOutcome::WorktreeTaskCandidate {
            response_redacted: "Ready for validation.".to_owned(),
            requirements: RequirementSnapshot {
                objective: "fix conversation flow".to_owned(),
                in_scope: vec!["conversation flow".to_owned()],
                out_of_scope: vec!["automatic merge".to_owned()],
                constraints: vec!["no task before approval".to_owned()],
                acceptance_criteria: vec!["tests pass".to_owned()],
                verification_plan: verification_plan(),
                risks: vec!["stale approval".to_owned()],
                open_questions: Vec::new(),
            },
        },
    );
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    let plan = database
        .load_client_command_by_idempotency_key(&format!("conversation-plan-{message_id}"))?
        .ok_or("missing plan-only planning command")?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    let graph = database.current_graph(session_id)?.ok_or("missing graph")?;
    assert_eq!(graph.revision.status, GraphRevisionStatus::AwaitingApproval);
    let session = database
        .load_session(session_id)?
        .ok_or("missing session")?;
    assert_eq!(session.state, SessionState::AwaitingApproval);
    assert_eq!(
        session.revision, 3,
        "planning must persist a validating phase"
    );
    let proposal_hash = graph
        .revision
        .proposal_hash
        .clone()
        .ok_or("missing validated hash")?;
    let summary: orchestrator_domain::GraphValidationSummary =
        serde_json::from_value(graph.revision.validation.clone())?;
    let authority = summary.authority.ok_or("missing graph authority")?;
    assert_zero_writable_rows(&database_path, &database)?;

    let same_invocation_approval = ClientCommand {
        command_id: ClientCommandId::new(),
        session_id: Some(session_id),
        task_id: None,
        action: ClientCommandAction::ApproveGraph,
        payload: serde_json::to_value(ApproveGraphCommandPayload {
            revision_id: graph.revision.revision_id,
            requirement_revision_id: authority.requirement_revision_id,
            validation_hash: authority.validation_hash.clone(),
            base_commit: authority.base_commit.clone(),
            proposal_hash,
            approved_by: "operator".to_owned(),
        })?,
        idempotency_key: "reject-plan-only-derived-approval".to_owned(),
        state: ClientCommandState::Pending,
        requested_by: "operator".to_owned(),
        requested_at: Utc::now(),
        claimed_at: None,
        completed_at: None,
        outcome: None,
    };
    database.submit_derived_client_command(plan.command_id, &same_invocation_approval)?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    let rejected = database
        .load_client_command(same_invocation_approval.command_id)?
        .ok_or("missing rejected plan-only approval")?;
    assert_eq!(rejected.state, ClientCommandState::Failed);
    assert!(rejected.outcome.unwrap_or_default().contains("plan-only"));
    assert_zero_writable_rows(&database_path, &database)?;

    let explicit_approval = ClientCommand {
        command_id: ClientCommandId::new(),
        idempotency_key: "later-explicit-graph-approval".to_owned(),
        requested_by: "spoofable-client-field".to_owned(),
        ..same_invocation_approval
    };
    database.submit_client_command_with_invocation(&explicit_approval, false)?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    let approved = database
        .load_client_command(explicit_approval.command_id)?
        .ok_or("missing explicit approval")?;
    assert_eq!(approved.state, ClientCommandState::Completed);
    database.submit_client_command_with_invocation(&explicit_approval, false)?;

    with_workspace(&database_path, &database, |connection| {
        let tasks: i64 =
            connection.query_row("SELECT count(*) FROM tasks", [], |row| row.get(0))?;
        let worktrees: i64 =
            connection.query_row("SELECT count(*) FROM worktrees", [], |row| row.get(0))?;
        assert_eq!(tasks, 1);
        assert_eq!(worktrees, 0);
        let persisted_authority: (String, String, String, String) = connection.query_row(
            "SELECT session_id, requirement_revision_id, validation_hash, base_commit
             FROM graph_approvals WHERE revision_id = ?1",
            [graph.revision.revision_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(persisted_authority.0, session_id.to_string());
        assert_eq!(
            persisted_authority.1,
            authority.requirement_revision_id.to_string()
        );
        assert_eq!(persisted_authority.2, authority.validation_hash);
        assert_eq!(persisted_authority.3, authority.base_commit);
        Ok(())
    })?;
    Ok(())
}

#[tokio::test]
async fn new_user_message_atomically_supersedes_approval_candidate()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    database.submit_client_command(&append_command(session_id, "candidate"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let repository = git_repository()?;
    let services = services(
        std::fs::canonicalize(repository.path())?,
        ConversationOutcome::WorktreeTaskCandidate {
            response_redacted: "Ready for validation.".to_owned(),
            requirements: RequirementSnapshot {
                objective: "fix conversation flow".to_owned(),
                in_scope: vec!["conversation flow".to_owned()],
                out_of_scope: vec!["automatic merge".to_owned()],
                constraints: vec!["no task before approval".to_owned()],
                acceptance_criteria: vec!["tests pass".to_owned()],
                verification_plan: verification_plan(),
                risks: vec!["stale approval".to_owned()],
                open_questions: Vec::new(),
            },
        },
    );
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    process_next_orchestration_command(&database, &services, &IdentityRedactor, Utc::now()).await?;
    let graph_id = database
        .current_graph(session_id)?
        .ok_or("missing graph")?
        .revision
        .revision_id;

    database.submit_client_command(&append_command(session_id, "change the scope"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;

    assert_eq!(
        database
            .load_graph_revision(graph_id)?
            .map(|graph| graph.status),
        Some(GraphRevisionStatus::Superseded)
    );
    assert_eq!(
        database
            .load_session(session_id)?
            .map(|session| session.state),
        Some(SessionState::Drafting)
    );
    assert_zero_writable_rows(&database_path, &database)?;
    Ok(())
}

#[tokio::test]
async fn daemon_restart_finalizes_interrupted_conversation_before_polling()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = Arc::new(database);
    let workspace = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &workspace)?;
    workspace.submit_client_command(&append_command(session_id, "hello"))?;
    process_next_client_command(&workspace, &IdentityRedactor, Utc::now())?;
    let claimed = workspace
        .claim_next_orchestration_client_command(Utc::now())?
        .ok_or("missing conversation command")?;
    let attempt_id = ConversationAttemptId::from_uuid(claimed.command_id.into_uuid());
    workspace.begin_conversation_attempt(&NewConversationAttempt {
        attempt_id,
        session_id,
        source_message_id: claimed
            .payload
            .get("source_message_id")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing source message")?
            .parse()?,
        provider: ProviderId::Codex,
        started_at: claimed.requested_at,
    })?;
    let directory = tempfile::tempdir()?;
    let services = services_with_conversation(
        std::fs::canonicalize(directory.path())?,
        Arc::new(FakeConversation {
            outcome: ConversationOutcome::AnswerComplete {
                response_redacted: "unused".to_owned(),
            },
        }),
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    serve_with_orchestration(
        Arc::clone(&database),
        workspace.workspace_id(),
        DaemonInstanceId::new(),
        42,
        cancellation,
        DaemonSettings::default(),
        Arc::new(IdentityRedactor),
        services,
    )
    .await?;

    assert_eq!(
        workspace
            .load_conversation_attempt(attempt_id)?
            .map(|attempt| attempt.status),
        Some(ConversationAttemptStatus::Failed)
    );
    assert_eq!(
        workspace
            .load_client_command(claimed.command_id)?
            .map(|command| command.state),
        Some(ClientCommandState::Failed)
    );
    assert_zero_writable_rows(&database_path, &workspace)?;
    Ok(())
}

#[tokio::test]
async fn repository_head_drift_rejects_approval_without_materializing_tasks()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id, database_path) = database()?;
    let database = database.workspace(workspace_id);
    let session_id = seed_session(&database_path, &database)?;
    database.submit_client_command(&append_command(session_id, "candidate"))?;
    process_next_client_command(&database, &IdentityRedactor, Utc::now())?;
    let repository = git_repository()?;
    let services = services(
        std::fs::canonicalize(repository.path())?,
        ConversationOutcome::WorktreeTaskCandidate {
            response_redacted: "Ready for validation.".to_owned(),
            requirements: RequirementSnapshot {
                objective: "fix conversation flow".to_owned(),
                in_scope: vec!["conversation flow".to_owned()],
                out_of_scope: vec!["automatic merge".to_owned()],
                constraints: vec!["no task before approval".to_owned()],
                acceptance_criteria: vec!["tests pass".to_owned()],
                verification_plan: verification_plan(),
                risks: vec!["stale approval".to_owned()],
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
    let authority = serde_json::from_value::<orchestrator_domain::GraphValidationSummary>(
        graph.revision.validation.clone(),
    )?
    .authority
    .ok_or("missing graph authority")?;

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
            requirement_revision_id: authority.requirement_revision_id,
            validation_hash: authority.validation_hash,
            base_commit: authority.base_commit,
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
    assert_zero_writable_rows(&database_path, &database)?;
    Ok(())
}
