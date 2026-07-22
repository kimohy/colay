use std::sync::Arc;

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    ApproveGraphCommandPayload, ClientCommand, ClientCommandAction, ConversationMessage,
    CorrelationId, EventActor, EventId, EventType, GraphRevisionId, GraphValidationPolicy,
    MessageId, MessageKind, MessageRole, MessageState, PlanningAttemptId, ProviderId,
    RequestPlanCommandPayload, SandboxMode, SchemaVersion, SessionId, SessionState, TaskEvent,
};
use orchestrator_engine::{PlannerRequest, TaskPlanner, collect_planner_response};
use orchestrator_state::{
    Database, GraphApprovalRequest, GraphRevisionStatus, NewGraphAttempt, NewPlanningAttempt,
    StateError,
};

use crate::{CommandProcessingResult, DaemonError, MessageRedactor};

#[derive(Clone)]
pub struct PlanningServices {
    pub planner: Arc<dyn TaskPlanner>,
    pub planner_provider: ProviderId,
    pub validation_policy: GraphValidationPolicy,
    pub integration: Option<crate::IntegrationServices>,
}

pub async fn process_next_orchestration_command(
    database: &Database,
    services: &PlanningServices,
    redactor: &dyn MessageRedactor,
    now: DateTime<Utc>,
) -> Result<Option<CommandProcessingResult>, DaemonError> {
    let Some(command) = database.claim_next_orchestration_client_command(now)? else {
        return Ok(None);
    };
    let result = match command.action {
        ClientCommandAction::RequestPlan => {
            request_plan(database, services, redactor, &command, now).await
        }
        ClientCommandAction::RequestConversationTurn => Err(ExecutionError::Rejected(
            "conversation orchestration is not enabled".to_owned(),
        )),
        ClientCommandAction::ApproveGraph => approve_graph(database, &command, now),
        ClientCommandAction::ReviseGraph | ClientCommandAction::CancelPlan => Err(
            ExecutionError::Rejected("revise/cancel planning commands are not enabled".to_owned()),
        ),
        ClientCommandAction::RequestIntegration => {
            if let Some(integration) = services.integration.as_ref() {
                crate::integration::request_integration(database, integration, &command, now)
                    .await
                    .map_err(map_integration_error)
            } else {
                Err(ExecutionError::Rejected(
                    "integration services are unavailable".to_owned(),
                ))
            }
        }
        ClientCommandAction::ApproveIntegration => {
            if let Some(integration) = services.integration.as_ref() {
                crate::integration::approve_integration(database, integration, &command, now)
                    .await
                    .map_err(map_integration_error)
            } else {
                Err(ExecutionError::Rejected(
                    "integration services are unavailable".to_owned(),
                ))
            }
        }
        ClientCommandAction::CreateResolutionTask => {
            crate::integration::create_resolution_task(database, &command, now)
                .map_err(map_integration_error)
        }
        ClientCommandAction::CreateSession
        | ClientCommandAction::AppendMessage
        | ClientCommandAction::StopDaemon => Err(ExecutionError::Rejected(
            "non-orchestration command reached planning processor".to_owned(),
        )),
    };
    match result {
        Ok(outcome) => {
            database.complete_client_command(command.command_id, &outcome, Utc::now())?;
            Ok(Some(CommandProcessingResult::Completed(command.command_id)))
        }
        Err(ExecutionError::Rejected(reason)) => {
            database.fail_client_command(command.command_id, &reason, Utc::now())?;
            Ok(Some(CommandProcessingResult::Failed(command.command_id)))
        }
        Err(ExecutionError::State(error)) if is_rejected_state_error(&error) => {
            database.fail_client_command(command.command_id, &error.to_string(), Utc::now())?;
            Ok(Some(CommandProcessingResult::Failed(command.command_id)))
        }
        Err(ExecutionError::State(error)) => Err(error.into()),
    }
}

fn map_integration_error(error: crate::integration::IntegrationCommandError) -> ExecutionError {
    match error {
        crate::integration::IntegrationCommandError::Rejected(reason) => {
            ExecutionError::Rejected(reason)
        }
        crate::integration::IntegrationCommandError::State(error) => ExecutionError::State(error),
    }
}

#[derive(Debug, thiserror::Error)]
enum ExecutionError {
    #[error("{0}")]
    Rejected(String),
    #[error(transparent)]
    State(StateError),
}

impl From<StateError> for ExecutionError {
    fn from(error: StateError) -> Self {
        Self::State(error)
    }
}

async fn request_plan(
    database: &Database,
    services: &PlanningServices,
    redactor: &dyn MessageRedactor,
    command: &ClientCommand,
    now: DateTime<Utc>,
) -> Result<String, ExecutionError> {
    let session_id = command.session_id.ok_or_else(|| {
        ExecutionError::Rejected("request-plan command requires a session target".to_owned())
    })?;
    if command.task_id.is_some() {
        return Err(ExecutionError::Rejected(
            "request-plan command cannot target a task".to_owned(),
        ));
    }
    let payload: RequestPlanCommandPayload = serde_json::from_value(command.payload.clone())
        .map_err(|_| ExecutionError::Rejected("request-plan payload is invalid".to_owned()))?;
    let goal = database
        .load_message(payload.goal_message_id)?
        .ok_or_else(|| {
            ExecutionError::Rejected("planning goal message does not exist".to_owned())
        })?;
    if goal.session_id != session_id
        || goal.role != MessageRole::User
        || goal.state != MessageState::Final
    {
        return Err(ExecutionError::Rejected(
            "planning goal must be a final user message in the target session".to_owned(),
        ));
    }

    let revision_id = GraphRevisionId::from_uuid(command.command_id.into_uuid());
    let attempt_id = PlanningAttemptId::from_uuid(command.command_id.into_uuid());
    if let Some(existing) = database.load_graph_revision(revision_id)?
        && existing.status != GraphRevisionStatus::Planning
    {
        reconcile_completed_plan(database, command, &existing, now)?;
        return Ok(format!("graph:{revision_id}"));
    }

    transition_to_planning(database, command, session_id, now)?;
    database.begin_graph_attempt(&NewPlanningAttempt {
        attempt_id,
        revision_id,
        session_id,
        goal_message_id: payload.goal_message_id,
        planner_provider: services.planner_provider,
        started_at: command.requested_at,
    })?;
    let planner_request = PlannerRequest {
        revision_id,
        session_id,
        goal_message_id: payload.goal_message_id,
        goal_redacted: goal.content_redacted,
        repository_summary_redacted: "repository-local Rust workspace".to_owned(),
        validation_policy: services.validation_policy.clone(),
        sandbox: SandboxMode::ReadOnly,
    };
    let completed_at = Utc::now();
    let result = match services.planner.propose(planner_request.clone()).await {
        Ok(response) => collect_planner_response(&planner_request, response),
        Err(error) => Err(error),
    };
    let (attempt, next_state, kind, content) = match result {
        Ok(graph) => {
            let hash = graph.proposal_hash.clone();
            let nodes = graph.proposal.nodes.len();
            (
                NewGraphAttempt::from_validated(
                    attempt_id,
                    graph,
                    command.requested_at,
                    completed_at,
                ),
                SessionState::AwaitingApproval,
                MessageKind::ApprovalRequest,
                format!(
                    "Task graph {revision_id} with {nodes} tasks awaits exact hash approval: {hash}"
                ),
            )
        }
        Err(error) => {
            let detail = redactor.redact(&error.to_string());
            (
                NewGraphAttempt::invalid(
                    attempt_id,
                    revision_id,
                    session_id,
                    payload.goal_message_id,
                    services.planner_provider,
                    serde_json::json!({"errors": [detail]}),
                    detail.clone(),
                    command.requested_at,
                    completed_at,
                ),
                SessionState::NeedsAttention,
                MessageKind::Warning,
                format!("Task graph planning needs attention: {detail}"),
            )
        }
    };
    database.finish_graph_attempt(&attempt)?;
    transition_from_planning(database, command, session_id, next_state, completed_at)?;
    append_timeline(database, command, session_id, kind, &content, completed_at)?;
    Ok(format!("graph:{revision_id}"))
}

fn approve_graph(
    database: &Database,
    command: &ClientCommand,
    now: DateTime<Utc>,
) -> Result<String, ExecutionError> {
    let session_id = command.session_id.ok_or_else(|| {
        ExecutionError::Rejected("approve-graph command requires a session target".to_owned())
    })?;
    if command.task_id.is_some() {
        return Err(ExecutionError::Rejected(
            "approve-graph command cannot target a task".to_owned(),
        ));
    }
    let payload: ApproveGraphCommandPayload = serde_json::from_value(command.payload.clone())
        .map_err(|_| ExecutionError::Rejected("approve-graph payload is invalid".to_owned()))?;
    payload
        .validate()
        .map_err(|error| ExecutionError::Rejected(error.to_string()))?;
    let revision = database
        .load_graph_revision(payload.revision_id)?
        .ok_or_else(|| {
            ExecutionError::Rejected("approved graph revision does not exist".to_owned())
        })?;
    if revision.session_id != session_id {
        return Err(ExecutionError::Rejected(
            "approved graph revision belongs to another session".to_owned(),
        ));
    }
    let approved = database.approve_graph_and_materialize_tasks(&GraphApprovalRequest {
        revision_id: payload.revision_id,
        expected_proposal_hash: payload.proposal_hash,
        approved_by: payload.approved_by,
        approved_at: now,
    })?;
    let session = database
        .load_session(session_id)?
        .ok_or_else(|| ExecutionError::Rejected("approval session does not exist".to_owned()))?;
    if session.state == SessionState::AwaitingApproval {
        database.transition_session_with_event(
            session_id,
            session.revision,
            SessionState::Running,
            now,
            session_event(
                command,
                session_id,
                EventType::SessionStateTransitioned,
                now,
            ),
        )?;
    } else if session.state != SessionState::Running {
        return Err(ExecutionError::Rejected(
            "approval session is not awaiting approval".to_owned(),
        ));
    }
    Ok(format!(
        "approved:{}:{}",
        approved.revision_id,
        approved.task_ids.len()
    ))
}

fn transition_to_planning(
    database: &Database,
    command: &ClientCommand,
    session_id: SessionId,
    now: DateTime<Utc>,
) -> Result<(), ExecutionError> {
    let session = database
        .load_session(session_id)?
        .ok_or_else(|| ExecutionError::Rejected("planning session does not exist".to_owned()))?;
    if session.state == SessionState::Planning {
        return Ok(());
    }
    if !matches!(
        session.state,
        SessionState::Drafting | SessionState::NeedsAttention | SessionState::AwaitingApproval
    ) {
        return Err(ExecutionError::Rejected(
            "session state does not allow graph planning".to_owned(),
        ));
    }
    database.transition_session_with_event(
        session_id,
        session.revision,
        SessionState::Planning,
        now,
        session_event(
            command,
            session_id,
            EventType::SessionStateTransitioned,
            now,
        ),
    )?;
    Ok(())
}

fn transition_from_planning(
    database: &Database,
    command: &ClientCommand,
    session_id: SessionId,
    next: SessionState,
    now: DateTime<Utc>,
) -> Result<(), ExecutionError> {
    let session = database
        .load_session(session_id)?
        .ok_or_else(|| ExecutionError::Rejected("planning session disappeared".to_owned()))?;
    if session.state == next {
        return Ok(());
    }
    if session.state != SessionState::Planning {
        return Err(ExecutionError::Rejected(
            "planning completion session state conflicts".to_owned(),
        ));
    }
    database.transition_session_with_event(
        session_id,
        session.revision,
        next,
        now,
        session_event(
            command,
            session_id,
            EventType::SessionStateTransitioned,
            now,
        ),
    )?;
    Ok(())
}

fn reconcile_completed_plan(
    database: &Database,
    command: &ClientCommand,
    revision: &orchestrator_state::StoredGraphRevision,
    now: DateTime<Utc>,
) -> Result<(), ExecutionError> {
    let next = match revision.status {
        GraphRevisionStatus::AwaitingApproval => SessionState::AwaitingApproval,
        GraphRevisionStatus::Invalid => SessionState::NeedsAttention,
        GraphRevisionStatus::Approved => SessionState::Running,
        GraphRevisionStatus::Planning
        | GraphRevisionStatus::Superseded
        | GraphRevisionStatus::Cancelled => {
            return Err(ExecutionError::Rejected(
                "stored planning revision cannot be reconciled".to_owned(),
            ));
        }
    };
    transition_from_planning(database, command, revision.session_id, next, now)?;
    let completed_at = revision.completed_at.unwrap_or(now);
    let (kind, content) = match revision.status {
        GraphRevisionStatus::AwaitingApproval => {
            let nodes = revision
                .proposal
                .as_ref()
                .map_or(0, |proposal| proposal.nodes.len());
            let hash = revision.proposal_hash.as_deref().unwrap_or("missing-hash");
            (
                MessageKind::ApprovalRequest,
                format!(
                    "Task graph {} with {nodes} tasks awaits exact hash approval: {hash}",
                    revision.revision_id
                ),
            )
        }
        GraphRevisionStatus::Invalid => {
            let detail = revision
                .validation
                .pointer("/errors/0")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("invalid task graph");
            (
                MessageKind::Warning,
                format!("Task graph planning needs attention: {detail}"),
            )
        }
        GraphRevisionStatus::Approved => (
            MessageKind::StateChange,
            format!("Task graph {} is approved.", revision.revision_id),
        ),
        GraphRevisionStatus::Planning
        | GraphRevisionStatus::Superseded
        | GraphRevisionStatus::Cancelled => {
            return Err(ExecutionError::Rejected(
                "stored planning revision cannot be reconciled".to_owned(),
            ));
        }
    };
    append_timeline(
        database,
        command,
        revision.session_id,
        kind,
        &content,
        completed_at,
    )
}

fn append_timeline(
    database: &Database,
    command: &ClientCommand,
    session_id: SessionId,
    kind: MessageKind,
    content: &str,
    now: DateTime<Utc>,
) -> Result<(), ExecutionError> {
    let message_id = MessageId::from_uuid(command.command_id.into_uuid());
    let message = ConversationMessage {
        message_id,
        session_id,
        task_id: None,
        role: MessageRole::Orchestrator,
        kind,
        state: MessageState::Final,
        content_redacted: content.to_owned(),
        created_at: now,
        finalized_at: Some(now),
    };
    if let Some(existing) = database.load_message(message_id)? {
        if existing == message {
            return Ok(());
        }
        return Err(ExecutionError::Rejected(
            "planning timeline replay conflicts".to_owned(),
        ));
    }
    database.append_message_with_event(
        &message,
        session_event(command, session_id, EventType::MessageAppended, now),
    )?;
    Ok(())
}

fn session_event(
    command: &ClientCommand,
    session_id: SessionId,
    event_type: EventType,
    now: DateTime<Utc>,
) -> TaskEvent {
    TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id: Some(session_id),
        task_id: None,
        occurred_at: now,
        event_type,
        from_state: None,
        to_state: None,
        reason: Some(format!("client command {}", command.command_id)),
        actor: EventActor::Orchestrator,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload: serde_json::json!({"command_id": command.command_id}),
        previous_hash: None,
        event_hash: String::new(),
    }
}

fn is_rejected_state_error(error: &StateError) -> bool {
    matches!(
        error,
        StateError::InvalidRecord(_) | StateError::OptimisticConflict { .. }
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use chrono::{TimeDelta, Utc};
    use orchestrator_domain::{
        ApproveGraphCommandPayload, ClientCommand, ClientCommandAction, ClientCommandId,
        ClientCommandState, GraphValidationPolicy, MessageId, ModelProfile, ProviderId,
        RequestPlanCommandPayload, SandboxMode, SessionId,
    };
    use orchestrator_engine::{
        PlannerExit, PlannerFailure, PlannerRequest, PlannerResponse, TaskPlanner,
    };
    use orchestrator_state::{DaemonStatus, Database, GraphRevisionStatus};
    use rusqlite::params;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use crate::{
        CommandProcessingResult, DaemonExit, DaemonSettings, MessageRedactor,
        serve_with_orchestration,
    };

    use super::{PlanningServices, process_next_orchestration_command, request_plan};

    #[derive(Clone, Copy)]
    enum FakeMode {
        Valid,
        Malformed,
        SecretError,
    }

    struct FakePlanner {
        mode: FakeMode,
        delay: Duration,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl TaskPlanner for FakePlanner {
        async fn propose(
            &self,
            request: PlannerRequest,
        ) -> Result<PlannerResponse, PlannerFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            if matches!(self.mode, FakeMode::SecretError) {
                return Err(PlannerFailure::Invocation {
                    reason: "secret planner failure".to_owned(),
                    evidence_redacted: "secret evidence".to_owned(),
                });
            }
            let output_redacted = if matches!(self.mode, FakeMode::Malformed) {
                b"not-json".to_vec()
            } else {
                serde_json::to_vec(&json!({
                    "schema_version": "1",
                    "revision_id": request.revision_id,
                    "session_id": request.session_id,
                    "goal_message_id": request.goal_message_id,
                    "planner_provider": "codex",
                    "proposed_at": Utc::now(),
                    "nodes": [
                        {
                            "key": "domain", "title": "Domain", "objective": "domain",
                            "dependencies": [], "constraints": [],
                            "acceptance_criteria": ["tests"], "provider": "codex",
                            "profile": "standard", "write_scopes": ["crates/domain"],
                            "repository_wide_write_scope": false, "risks": [],
                            "parallel_safety": "isolated"
                        },
                        {
                            "key": "tui", "title": "TUI", "objective": "tui",
                            "dependencies": ["domain"], "constraints": [],
                            "acceptance_criteria": ["tests"], "provider": "codex",
                            "profile": "standard", "write_scopes": ["crates/tui"],
                            "repository_wide_write_scope": false, "risks": [],
                            "parallel_safety": "after domain"
                        }
                    ]
                }))
                .unwrap_or_default()
            };
            Ok(PlannerResponse {
                schema_version: orchestrator_domain::SchemaVersion::v1(),
                session_id: request.session_id,
                goal_message_id: request.goal_message_id,
                provider: ProviderId::Codex,
                sandbox: SandboxMode::ReadOnly,
                exit: PlannerExit::Succeeded,
                output_redacted,
                evidence_redacted: "fake planner".to_owned(),
            })
        }
    }

    struct SecretRedactor;

    impl MessageRedactor for SecretRedactor {
        fn redact(&self, value: &str) -> String {
            value.replace("secret", "[REDACTED]")
        }
    }

    fn database() -> Result<Arc<Database>, Box<dyn std::error::Error>> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        Ok(Arc::new(database))
    }

    fn seed_goal(
        database: &Database,
    ) -> Result<(SessionId, MessageId), Box<dyn std::error::Error>> {
        let session_id = SessionId::new();
        let message_id = MessageId::new();
        let now = Utc::now();
        database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO sessions(session_id, schema_version, revision, title, state,
                    created_at, updated_at) VALUES (?1, '1', 0, 'planning', 'drafting', ?2, ?2)",
                params![session_id.to_string(), now.to_rfc3339()],
            )?;
            connection.execute(
                "INSERT INTO conversation_messages(message_id, session_id, ordinal, role, kind,
                    state, content_redacted, created_at, finalized_at)
                 VALUES (?1, ?2, 1, 'user', 'user_message', 'final', 'build it', ?3, ?3)",
                params![
                    message_id.to_string(),
                    session_id.to_string(),
                    now.to_rfc3339()
                ],
            )?;
            Ok(())
        })?;
        Ok((session_id, message_id))
    }

    fn services(mode: FakeMode, delay: Duration) -> (PlanningServices, Arc<FakePlanner>) {
        let planner = Arc::new(FakePlanner {
            mode,
            delay,
            calls: AtomicUsize::new(0),
        });
        let service = PlanningServices {
            planner: planner.clone(),
            planner_provider: ProviderId::Codex,
            validation_policy: GraphValidationPolicy {
                eligible_providers: BTreeSet::from([ProviderId::Codex]),
                eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
                max_parallel_workers: 2,
                per_provider_limits: BTreeMap::new(),
            },
            integration: None,
        };
        (service, planner)
    }

    fn command(
        action: ClientCommandAction,
        session_id: SessionId,
        payload: serde_json::Value,
        key: &str,
    ) -> ClientCommand {
        ClientCommand {
            command_id: ClientCommandId::new(),
            session_id: Some(session_id),
            task_id: None,
            action,
            payload,
            idempotency_key: key.to_owned(),
            state: ClientCommandState::Pending,
            requested_by: "tui".to_owned(),
            requested_at: Utc::now(),
            claimed_at: None,
            completed_at: None,
            outcome: None,
        }
    }

    fn plan_command(session_id: SessionId, goal: MessageId, key: &str) -> ClientCommand {
        command(
            ClientCommandAction::RequestPlan,
            session_id,
            serde_json::to_value(RequestPlanCommandPayload {
                goal_message_id: goal,
            })
            .unwrap_or_default(),
            key,
        )
    }

    #[tokio::test]
    async fn valid_plan_awaits_approval_without_creating_tasks()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let (session_id, goal) = seed_goal(&database)?;
        let command = plan_command(session_id, goal, "valid-plan");
        database.submit_client_command(&command)?;
        let (services, _) = services(FakeMode::Valid, Duration::ZERO);
        assert_eq!(
            process_next_orchestration_command(&database, &services, &SecretRedactor, Utc::now())
                .await?,
            Some(CommandProcessingResult::Completed(command.command_id))
        );
        assert_eq!(
            database
                .load_session(session_id)?
                .map(|session| session.state),
            Some(orchestrator_domain::SessionState::AwaitingApproval)
        );
        let graph = database.current_graph(session_id)?.ok_or("missing graph")?;
        assert_eq!(graph.revision.status, GraphRevisionStatus::AwaitingApproval);
        assert!(graph.revision.proposal_hash.is_some());
        database.with_connection(|connection| {
            let count: i64 =
                connection.query_row("SELECT count(*) FROM tasks", [], |row| row.get(0))?;
            assert_eq!(count, 0);
            Ok(())
        })?;
        Ok(())
    }

    #[tokio::test]
    async fn invalid_plan_records_redacted_attention_timeline()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let (session_id, goal) = seed_goal(&database)?;
        let command = plan_command(session_id, goal, "invalid-plan");
        database.submit_client_command(&command)?;
        let (invalid_services, _) = services(FakeMode::Malformed, Duration::ZERO);
        process_next_orchestration_command(
            &database,
            &invalid_services,
            &SecretRedactor,
            Utc::now(),
        )
        .await?;
        assert_eq!(
            database
                .load_session(session_id)?
                .map(|session| session.state),
            Some(orchestrator_domain::SessionState::NeedsAttention)
        );
        let graph = database.current_graph(session_id)?.ok_or("missing graph")?;
        assert_eq!(graph.revision.status, GraphRevisionStatus::Invalid);
        assert!(graph.revision.proposal_hash.is_none());
        assert!(!database.messages_after(session_id, 1, 10)?.is_empty());

        let (secret_session, secret_goal) = seed_goal(&database)?;
        let secret_command = plan_command(secret_session, secret_goal, "secret-plan");
        database.submit_client_command(&secret_command)?;
        let (secret_services, _) = services(FakeMode::SecretError, Duration::ZERO);
        process_next_orchestration_command(
            &database,
            &secret_services,
            &SecretRedactor,
            Utc::now(),
        )
        .await?;
        let messages = database.messages_after(secret_session, 1, 10)?;
        assert!(messages.iter().any(|(_, message)| {
            message.content_redacted.contains("[REDACTED]")
                && !message.content_redacted.contains("secret")
        }));
        Ok(())
    }

    #[tokio::test]
    async fn typed_exact_approval_materializes_once_and_wrong_hash_fails()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let (session_id, goal) = seed_goal(&database)?;
        database.submit_client_command(&plan_command(session_id, goal, "approval-plan"))?;
        let (services, _) = services(FakeMode::Valid, Duration::ZERO);
        process_next_orchestration_command(&database, &services, &SecretRedactor, Utc::now())
            .await?;
        let graph = database.current_graph(session_id)?.ok_or("missing graph")?;
        let hash = graph.revision.proposal_hash.clone().ok_or("missing hash")?;
        let wrong = command(
            ClientCommandAction::ApproveGraph,
            session_id,
            serde_json::to_value(ApproveGraphCommandPayload {
                revision_id: graph.revision.revision_id,
                proposal_hash: "0".repeat(64),
                approved_by: "operator".to_owned(),
            })?,
            "wrong-approval",
        );
        database.submit_client_command(&wrong)?;
        assert_eq!(
            process_next_orchestration_command(&database, &services, &SecretRedactor, Utc::now())
                .await?,
            Some(CommandProcessingResult::Failed(wrong.command_id))
        );
        let approve = command(
            ClientCommandAction::ApproveGraph,
            session_id,
            serde_json::to_value(ApproveGraphCommandPayload {
                revision_id: graph.revision.revision_id,
                proposal_hash: hash,
                approved_by: "operator".to_owned(),
            })?,
            "exact-approval",
        );
        database.submit_client_command(&approve)?;
        process_next_orchestration_command(&database, &services, &SecretRedactor, Utc::now())
            .await?;
        assert_eq!(
            database
                .load_session(session_id)?
                .map(|session| session.state),
            Some(orchestrator_domain::SessionState::Running)
        );
        assert_eq!(
            database
                .current_graph(session_id)?
                .ok_or("graph")?
                .tasks
                .len(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn completed_projection_reconciles_after_command_crash_without_replanning()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let (session_id, goal) = seed_goal(&database)?;
        let command = plan_command(session_id, goal, "crash-plan");
        database.submit_client_command(&command)?;
        let claimed = database
            .claim_next_orchestration_client_command(Utc::now())?
            .ok_or("command not claimed")?;
        let (services, planner) = services(FakeMode::Valid, Duration::ZERO);
        request_plan(&database, &services, &SecretRedactor, &claimed, Utc::now()).await?;
        database.recover_stale_client_commands(Utc::now() + TimeDelta::seconds(1))?;
        process_next_orchestration_command(&database, &services, &SecretRedactor, Utc::now())
            .await?;
        assert_eq!(planner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            database
                .load_client_command(command.command_id)?
                .map(|value| value.state),
            Some(ClientCommandState::Completed)
        );
        Ok(())
    }

    #[tokio::test]
    async fn slow_planner_does_not_interrupt_daemon_heartbeats()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let (session_id, goal) = seed_goal(&database)?;
        let command = plan_command(session_id, goal, "slow-plan");
        database.submit_client_command(&command)?;
        let (services, _) = services(FakeMode::Valid, Duration::from_millis(120));
        let cancellation = CancellationToken::new();
        let task_database = Arc::clone(&database);
        let task_cancel = cancellation.clone();
        let service = tokio::spawn(async move {
            serve_with_orchestration(
                task_database,
                orchestrator_domain::DaemonInstanceId::new(),
                42,
                task_cancel,
                DaemonSettings {
                    heartbeat_interval: Duration::from_millis(10),
                    command_poll_interval: Duration::from_millis(5),
                    lease_ttl: TimeDelta::milliseconds(100),
                },
                Arc::new(SecretRedactor),
                services,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(35)).await;
        let first = database.daemon_status(Utc::now())?;
        tokio::time::sleep(Duration::from_millis(45)).await;
        let second = database.daemon_status(Utc::now())?;
        let (DaemonStatus::Online(first), DaemonStatus::Online(second)) = (first, second) else {
            panic!("daemon was not online during planning");
        };
        assert!(second.heartbeat_at > first.heartbeat_at);
        for _ in 0..80 {
            if database
                .load_client_command(command.command_id)?
                .is_some_and(|value| value.state == ClientCommandState::Completed)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            database
                .load_client_command(command.command_id)?
                .map(|value| value.state),
            Some(ClientCommandState::Completed)
        );
        cancellation.cancel();
        assert_eq!(service.await??, DaemonExit::Cancelled);
        Ok(())
    }
}
