#![cfg(feature = "test-fixtures")]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use orchestrator_daemon::{
    DaemonSettings, ExecutionServices, MessageRedactor, PlanningServices,
    serve_with_full_orchestration,
};
use orchestrator_domain::{
    ConversationMessage, CorrelationId, DaemonInstanceId, EventActor, EventId, EventType,
    GraphRevisionId, GraphValidationPolicy, MessageId, MessageKind, MessageRole, MessageState,
    ModelProfile, PlanningAttemptId, ProviderId, RepoPath, RiskTag, SchemaVersion, SessionId,
    SessionState, TaskEvent, TaskGraphNode, TaskGraphProposal, TaskInstructionState, TaskState,
    validate_task_graph,
};
use orchestrator_engine::{PlannerFailure, PlannerRequest, PlannerResponse, TaskPlanner};
use orchestrator_process::RedactionConfig;
use orchestrator_providers::{AdapterRuntime, ProcessAdapterRuntime};
use orchestrator_state::{
    Database, GraphApprovalRequest, NewGraphAttempt, NewSessionRecord, RepositoryStatePaths,
    RootConfig, TaskListFilter,
};
use tokio_util::sync::CancellationToken;

use colay::task_executor::OfficialCliTaskExecutor;

fn fake_provider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_colay-e2e-fake-provider"))
}

fn git(repository: &Path, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

fn repository() -> Result<(tempfile::TempDir, PathBuf), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let repository = directory.path().join("repository");
    fs::create_dir_all(repository.join("crates/domain"))?;
    fs::create_dir_all(repository.join("crates/tui"))?;
    fs::write(
        repository.join("crates/domain/lib.rs"),
        "pub const DOMAIN: u8 = 1;\n",
    )?;
    fs::write(
        repository.join("crates/tui/lib.rs"),
        "pub const TUI: u8 = 1;\n",
    )?;
    fs::write(repository.join(".gitignore"), ".colay/\n")?;
    git(&repository, &["init"])?;
    git(&repository, &["config", "user.name", "Parallel E2E"])?;
    git(
        &repository,
        &["config", "user.email", "parallel-e2e@example.invalid"],
    )?;
    git(&repository, &["add", "."])?;
    git(&repository, &["commit", "-m", "fixture base"])?;
    Ok((directory, fs::canonicalize(repository)?))
}

fn config(
    repository: &Path,
) -> Result<(RootConfig, RepositoryStatePaths), Box<dyn std::error::Error>> {
    let mut config = RootConfig::default();
    config.features.codex_app_server_adapter = false;
    config.features.codex_exec_fallback = true;
    config.orchestrator.max_parallel_workers = 2;
    config.orchestrator.default_timeout_minutes = 1;
    config.orchestrator.provider_parallel_limits = BTreeMap::from([("codex".to_owned(), 2)]);
    config.orchestrator.providers.gemini = None;
    config.orchestrator.providers.claude = None;
    config
        .orchestrator
        .providers
        .codex
        .as_mut()
        .ok_or("codex config")?
        .executable = fake_provider_binary().to_string_lossy().into_owned();
    let paths = RepositoryStatePaths::from_config(repository, &config)?;
    Ok((config, paths))
}

fn event(
    session_id: SessionId,
    task_id: Option<orchestrator_domain::TaskId>,
    event_type: EventType,
) -> TaskEvent {
    TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id: Some(session_id),
        task_id,
        occurred_at: Utc::now(),
        event_type,
        from_state: None,
        to_state: None,
        reason: None,
        actor: EventActor::User,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload: serde_json::json!({}),
        previous_hash: None,
        event_hash: String::new(),
    }
}

fn seed_approved_graph(
    database: &Database,
) -> Result<(SessionId, Vec<orchestrator_domain::TaskId>), Box<dyn std::error::Error>> {
    let session_id = SessionId::new();
    database.create_session_with_event(
        &NewSessionRecord {
            session_id,
            schema_version: SchemaVersion::V1.to_owned(),
            title: "parallel execution".to_owned(),
            state: SessionState::Drafting,
            created_at: Utc::now(),
        },
        event(session_id, None, EventType::SessionCreated),
    )?;
    let goal_message_id = MessageId::new();
    let goal = ConversationMessage {
        message_id: goal_message_id,
        session_id,
        task_id: None,
        role: MessageRole::User,
        kind: MessageKind::UserMessage,
        state: MessageState::Final,
        content_redacted: "execute two independent tasks".to_owned(),
        created_at: Utc::now(),
        finalized_at: Some(Utc::now()),
    };
    database
        .append_message_with_event(&goal, event(session_id, None, EventType::MessageAppended))?;
    let node = |key: &str, scope: &str| TaskGraphNode {
        key: key.to_owned(),
        title: format!("{key} task"),
        objective: format!("Inspect and complete {key}"),
        dependencies: Vec::new(),
        constraints: vec!["stay inside the declared scope".to_owned()],
        acceptance_criteria: vec![format!("{key} structured execution completes")],
        provider: Some(ProviderId::Codex),
        profile: ModelProfile::Standard,
        write_scopes: RepoPath::try_from(scope).ok().into_iter().collect(),
        repository_wide_write_scope: false,
        risks: vec![RiskTag::Concurrency],
        parallel_safety: "disjoint worktree and write scope".to_owned(),
    };
    let graph = validate_task_graph(
        TaskGraphProposal {
            schema_version: SchemaVersion::v1(),
            revision_id: GraphRevisionId::new(),
            session_id,
            goal_message_id,
            planner_provider: ProviderId::Codex,
            proposed_at: Utc::now(),
            nodes: vec![node("domain", "crates/domain"), node("tui", "crates/tui")],
        },
        &GraphValidationPolicy {
            eligible_providers: BTreeSet::from([ProviderId::Codex]),
            eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
            max_parallel_workers: 2,
            per_provider_limits: BTreeMap::from([(ProviderId::Codex, 2)]),
        },
    )?;
    database.record_graph_attempt(&NewGraphAttempt::from_validated(
        PlanningAttemptId::new(),
        graph.clone(),
        Utc::now(),
        Utc::now(),
    ))?;
    let approved = database.approve_graph_and_materialize_tasks(&GraphApprovalRequest {
        revision_id: graph.proposal.revision_id,
        expected_proposal_hash: graph.proposal_hash,
        approved_by: "parallel-e2e".to_owned(),
        approved_at: Utc::now(),
    })?;
    Ok((session_id, approved.task_ids))
}

fn queue_instruction(
    database: &Database,
    session_id: SessionId,
    task_id: orchestrator_domain::TaskId,
) -> Result<(), Box<dyn std::error::Error>> {
    let message = ConversationMessage {
        message_id: MessageId::new(),
        session_id,
        task_id: Some(task_id),
        role: MessageRole::User,
        kind: MessageKind::UserMessage,
        state: MessageState::Final,
        content_redacted: "preserve the declared boundary".to_owned(),
        created_at: Utc::now(),
        finalized_at: Some(Utc::now()),
    };
    database.append_message_with_event_and_instruction(
        &message,
        event(session_id, Some(task_id), EventType::MessageAppended),
    )?;
    Ok(())
}

struct UnusedPlanner;

#[async_trait]
impl TaskPlanner for UnusedPlanner {
    async fn propose(&self, _request: PlannerRequest) -> Result<PlannerResponse, PlannerFailure> {
        Err(PlannerFailure::Invocation {
            reason: "no planning command expected".to_owned(),
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

async fn wait_for_completion(database: &Database) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..400 {
        let tasks = database.list_tasks(&TaskListFilter {
            state: None,
            include_archived: false,
            limit: 10,
        })?;
        if tasks.len() == 2 && tasks.iter().all(|task| task.state == TaskState::Completed) {
            return Ok(());
        }
        if tasks.iter().any(|task| task.state == TaskState::Failed) {
            return Err("parallel execution produced a failed task".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err("parallel execution did not complete in time".into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn real_fake_cli_processes_run_parallel_tasks_and_restart_without_duplicates()
-> Result<(), Box<dyn std::error::Error>> {
    let (_directory, repository) = repository()?;
    let (config, paths) = config(&repository)?;
    fs::create_dir_all(&paths.root)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let (session_id, task_ids) = seed_approved_graph(&database)?;
    queue_instruction(&database, session_id, task_ids[0])?;

    let runtime: Arc<dyn AdapterRuntime> =
        Arc::new(ProcessAdapterRuntime::new(RedactionConfig::default()));
    let executor = Arc::new(OfficialCliTaskExecutor::new(&config, &repository, runtime)?);
    let planning = PlanningServices {
        planner: Arc::new(UnusedPlanner),
        planner_provider: ProviderId::Codex,
        validation_policy: GraphValidationPolicy {
            eligible_providers: BTreeSet::from([ProviderId::Codex]),
            eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
            max_parallel_workers: 2,
            per_provider_limits: BTreeMap::from([(ProviderId::Codex, 2)]),
        },
    };
    let execution = ExecutionServices {
        executor,
        repository_root: repository.clone(),
        state_root: paths.root.clone(),
        global_limit: 2,
        provider_limits: BTreeMap::from([(ProviderId::Codex, 2)]),
        claim_ttl: TimeDelta::seconds(30),
    };
    let cancellation = CancellationToken::new();
    let service_database = Arc::clone(&database);
    let service_cancellation = cancellation.clone();
    let service = tokio::spawn(async move {
        serve_with_full_orchestration(
            service_database,
            DaemonInstanceId::new(),
            77,
            service_cancellation,
            DaemonSettings {
                heartbeat_interval: Duration::from_millis(20),
                command_poll_interval: Duration::from_millis(10),
                lease_ttl: TimeDelta::seconds(2),
            },
            Arc::new(IdentityRedactor),
            planning,
            execution,
        )
        .await
    });
    wait_for_completion(&database).await?;
    cancellation.cancel();
    service.await??;

    for task_id in &task_ids {
        assert!(database.latest_sealed_checkpoint(*task_id)?.is_some());
        assert!(database.active_worktree(*task_id)?.is_some());
        assert_eq!(database.list_task_attempts(*task_id)?.len(), 1);
    }
    assert_eq!(
        database.list_task_instructions(task_ids[0])?[0].state,
        TaskInstructionState::Applied
    );
    database.with_connection(|connection| {
        let claims: i64 =
            connection.query_row("SELECT count(*) FROM task_schedule_claims", [], |row| {
                row.get(0)
            })?;
        let active: i64 = connection.query_row(
            "SELECT count(*) FROM task_schedule_claims WHERE released_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        let overlapping: i64 = connection.query_row(
            "SELECT count(*) FROM task_schedule_claims left_claim
             JOIN task_schedule_claims right_claim
               ON left_claim.schedule_claim_id < right_claim.schedule_claim_id
              AND left_claim.acquired_at < right_claim.released_at
              AND right_claim.acquired_at < left_claim.released_at",
            [],
            |row| row.get(0),
        )?;
        assert_eq!((claims, active), (2, 0));
        assert_eq!(overlapping, 1, "the two real CLI executions must overlap");
        Ok(())
    })?;

    let restart_cancellation = CancellationToken::new();
    let restart_signal = restart_cancellation.clone();
    let restart_database = Arc::clone(&database);
    let restart = tokio::spawn(async move {
        orchestrator_daemon::serve(
            &restart_database,
            DaemonInstanceId::new(),
            78,
            restart_signal,
            DaemonSettings {
                heartbeat_interval: Duration::from_millis(20),
                command_poll_interval: Duration::from_millis(10),
                lease_ttl: TimeDelta::seconds(2),
            },
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(75)).await;
    restart_cancellation.cancel();
    restart.await??;
    for task_id in task_ids {
        assert_eq!(database.list_task_attempts(task_id)?.len(), 1);
    }
    Ok(())
}
