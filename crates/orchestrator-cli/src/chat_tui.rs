use std::{
    path::{Path, PathBuf},
    str::FromStr as _,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use orchestrator_domain::{
    AppendMessageCommandPayload, ClientCommand, ClientCommandAction, ClientCommandId,
    ClientCommandState, CreateSessionCommandPayload, MessageId, MessageKind, ProviderId, SessionId,
    TaskId, TaskState,
};
use orchestrator_process::{RedactionConfig, Redactor};
use orchestrator_state::{
    ControlAction as StateControlAction, DaemonStatus, Database, RepositoryStatePaths, RootConfig,
    SessionListFilter, WorkspaceAttentionKind, WorkspaceProjection, WorkspaceReadRequest,
};
use orchestrator_tui::chat::{
    ActionFeedback, AttentionItem, AttentionSeverity, ComposerTarget, DaemonConnectivity,
    DriverError, TaskControlIntent, TaskInspector, TaskSummary, TimelineEntry, WorkspaceAction,
    WorkspaceCursor, WorkspaceDriver, WorkspaceSnapshot,
};
use serde::Serialize;
use serde_json::json;

const DEFAULT_SESSION_KEY: &str = "chat-default-session-v1";
const COMMAND_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub(crate) struct SqliteWorkspaceDriver {
    repository: PathBuf,
    database: Database,
    session_id: SessionId,
    selected_task_id: Option<TaskId>,
    redactor: Redactor,
}

impl SqliteWorkspaceDriver {
    pub(crate) async fn connect(
        repository: &Path,
        config: &RootConfig,
        explicit_config: Option<&Path>,
        selected_task: Option<&str>,
    ) -> Result<Self> {
        crate::daemon::ensure_started(repository, config, explicit_config).await?;
        let paths = RepositoryStatePaths::from_config(repository, config)?;
        let database = crate::daemon::open_ready_database(&paths)?;
        let redactor = Redactor::new(&RedactionConfig {
            literals: Vec::new(),
            patterns: config.orchestrator.redaction.patterns.clone(),
        })?;
        let session_id = ensure_default_session(&database, &redactor).await?;
        let selected_task_id = match selected_task {
            Some(task_id) => {
                let task_id = TaskId::from_str(task_id)?;
                database.save_workspace_selected_task(session_id, Some(task_id), Utc::now())?;
                Some(task_id)
            }
            None => database.load_workspace_selected_task(session_id)?,
        };
        Ok(Self {
            repository: repository.to_path_buf(),
            database,
            session_id,
            selected_task_id,
            redactor,
        })
    }

    #[cfg(test)]
    fn from_database(
        repository: PathBuf,
        database: Database,
        session_id: SessionId,
        selected_task_id: Option<TaskId>,
        redactor: Redactor,
    ) -> Self {
        Self {
            repository,
            database,
            session_id,
            selected_task_id,
            redactor,
        }
    }

    fn online(&self) -> Result<bool, DriverError> {
        self.database
            .daemon_status(Utc::now())
            .map(|status| matches!(status, DaemonStatus::Online(_)))
            .map_err(driver_error)
    }

    fn submit_message(
        &self,
        target: ComposerTarget,
        content: &str,
    ) -> Result<ActionFeedback, DriverError> {
        if !self.online()? {
            return Err(DriverError::new(
                "daemon is offline or stale; message submission is read-only",
            ));
        }
        let task_id = match target {
            ComposerTarget::Orchestrator => None,
            ComposerTarget::Task(task_id) => Some(
                TaskId::from_str(&task_id)
                    .map_err(|error| DriverError::new(format!("invalid task target: {error}")))?,
            ),
            ComposerTarget::AllRunning => {
                return Ok(ActionFeedback::unavailable("broadcast messaging"));
            }
        };
        let message_id = MessageId::new();
        let command_id = ClientCommandId::new();
        let content = self.redactor.redact(content);
        let command = ClientCommand {
            command_id,
            session_id: Some(self.session_id),
            task_id,
            action: ClientCommandAction::AppendMessage,
            payload: serde_json::to_value(AppendMessageCommandPayload {
                message_id,
                content,
            })
            .map_err(driver_error)?,
            idempotency_key: format!("chat-message-{message_id}"),
            state: ClientCommandState::Pending,
            requested_by: "local-tui".to_owned(),
            requested_at: Utc::now(),
            claimed_at: None,
            completed_at: None,
            outcome: None,
        };
        self.database
            .submit_client_command(&command)
            .map_err(driver_error)?;
        Ok(ActionFeedback::info(
            "message accepted by repository daemon",
        ))
    }

    fn request_control(
        &self,
        task_id: &str,
        intent: TaskControlIntent,
    ) -> Result<ActionFeedback, DriverError> {
        if !self.online()? {
            return Err(DriverError::new(
                "daemon is offline or stale; task controls are read-only",
            ));
        }
        let task_id = TaskId::from_str(task_id)
            .map_err(|error| DriverError::new(format!("invalid task ID: {error}")))?;
        let (action, payload) = match intent {
            TaskControlIntent::Pause => (StateControlAction::Pause, json!({})),
            TaskControlIntent::Resume => (StateControlAction::Resume, json!({})),
            TaskControlIntent::Cancel => (StateControlAction::Cancel, json!({})),
            TaskControlIntent::Handover { provider } => {
                let provider = ProviderId::from_str(&provider).map_err(|error| {
                    DriverError::new(format!("invalid handover provider: {error}"))
                })?;
                (StateControlAction::Handover, json!({"to": provider}))
            }
            TaskControlIntent::Retry => return Ok(ActionFeedback::unavailable("retry")),
            TaskControlIntent::Checkpoint => {
                return Ok(ActionFeedback::unavailable("chat checkpoint control"));
            }
            TaskControlIntent::Provider => {
                return Ok(ActionFeedback::unavailable("chat provider selection"));
            }
        };
        self.database
            .request_control(task_id, action, payload, "local-tui", Utc::now())
            .map_err(driver_error)?;
        Ok(ActionFeedback::info("task control accepted"))
    }
}

impl WorkspaceDriver for SqliteWorkspaceDriver {
    fn refresh(&mut self, _cursor: &WorkspaceCursor) -> Result<WorkspaceSnapshot, DriverError> {
        let projection = self
            .database
            .read_workspace_projection(WorkspaceReadRequest {
                session_id: self.session_id,
                selected_task_id: self.selected_task_id,
                before_ordinal: None,
                message_limit: 200,
                task_limit: 100,
            })
            .map_err(driver_error)?;
        let daemon = self
            .database
            .daemon_status(Utc::now())
            .map_err(driver_error)?;
        projection_to_snapshot(&self.repository, projection, &daemon).map_err(driver_error)
    }

    fn dispatch(&mut self, action: WorkspaceAction) -> Result<ActionFeedback, DriverError> {
        match action {
            WorkspaceAction::SubmitMessage { target, content } => {
                self.submit_message(target, &content)
            }
            WorkspaceAction::RequestTaskControl { task_id, intent } => {
                self.request_control(&task_id, intent)
            }
            WorkspaceAction::OpenAdministration => {
                Ok(ActionFeedback::info("opening administration dashboard"))
            }
            WorkspaceAction::Quit => Ok(ActionFeedback::info("workspace closed")),
        }
    }

    fn selection_changed(&mut self, task_id: Option<&str>) -> Result<(), DriverError> {
        let task_id = task_id
            .map(TaskId::from_str)
            .transpose()
            .map_err(driver_error)?;
        self.database
            .save_workspace_selected_task(self.session_id, task_id, Utc::now())
            .map_err(driver_error)?;
        self.selected_task_id = task_id;
        Ok(())
    }
}

async fn ensure_default_session(database: &Database, redactor: &Redactor) -> Result<SessionId> {
    if let Some(session) = database
        .list_sessions(&SessionListFilter {
            include_archived: false,
            limit: 1,
        })?
        .pop()
    {
        return Ok(session.session_id);
    }
    let existing = database.load_client_command_by_idempotency_key(DEFAULT_SESSION_KEY)?;
    let session_id = if let Some(command) = existing.as_ref() {
        serde_json::from_value::<CreateSessionCommandPayload>(command.payload.clone())?.session_id
    } else {
        let session_id = SessionId::new();
        let command = ClientCommand {
            command_id: ClientCommandId::new(),
            session_id: None,
            task_id: None,
            action: ClientCommandAction::CreateSession,
            payload: serde_json::to_value(CreateSessionCommandPayload {
                session_id,
                title: redactor.redact("Colay workspace"),
            })?,
            idempotency_key: DEFAULT_SESSION_KEY.to_owned(),
            state: ClientCommandState::Pending,
            requested_by: "local-tui".to_owned(),
            requested_at: Utc::now(),
            claimed_at: None,
            completed_at: None,
            outcome: None,
        };
        database.submit_client_command(&command)?;
        session_id
    };
    let deadline = Instant::now() + COMMAND_WAIT_TIMEOUT;
    loop {
        if database.load_session(session_id)?.is_some() {
            return Ok(session_id);
        }
        if let Some(command) =
            database.load_client_command_by_idempotency_key(DEFAULT_SESSION_KEY)?
            && command.state == ClientCommandState::Failed
        {
            bail!(
                "daemon rejected default session creation: {}",
                command
                    .outcome
                    .unwrap_or_else(|| "unknown failure".to_owned())
            );
        }
        if Instant::now() >= deadline {
            bail!("daemon did not create the default chat session within three seconds");
        }
        tokio::time::sleep(COMMAND_POLL_INTERVAL).await;
    }
}

#[allow(clippy::too_many_lines)]
fn projection_to_snapshot(
    repository: &Path,
    mut projection: WorkspaceProjection,
    daemon: &DaemonStatus,
) -> Result<WorkspaceSnapshot> {
    if let Some(inspector) = projection.inspector.as_ref()
        && !projection
            .recent_tasks
            .iter()
            .any(|task| task.task.task_id == inspector.task.task.task_id)
    {
        projection.recent_tasks.push(inspector.task.clone());
    }
    let attention_task_ids = projection
        .attention
        .iter()
        .filter_map(|item| item.task_id)
        .collect::<Vec<_>>();
    let tasks = projection
        .recent_tasks
        .iter()
        .map(|task| TaskSummary {
            task_id: task.task.task_id.to_string(),
            title: task.task.objective.clone(),
            state: enum_name(&task.task.state).unwrap_or_else(|_| "unknown".to_owned()),
            state_symbol: task_state_symbol(task.task.state).to_owned(),
            dependency_status: "repository task".to_owned(),
            needs_attention: attention_task_ids.contains(&task.task.task_id),
        })
        .collect::<Vec<_>>();
    let messages = projection
        .messages
        .iter()
        .map(|(ordinal, message)| {
            Ok(TimelineEntry {
                ordinal: i64::try_from(*ordinal).unwrap_or(i64::MAX),
                message_id: message.message_id.to_string(),
                task_id: message.task_id.map(|task_id| task_id.to_string()),
                role: enum_name(&message.role)?,
                kind: enum_name(&message.kind)?,
                state: enum_name(&message.state)?,
                content: message.content_redacted.clone(),
                created_at: message.created_at.to_rfc3339(),
                folded: message.kind == MessageKind::ToolSummary,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let attention = projection
        .attention
        .iter()
        .enumerate()
        .map(|(index, item)| AttentionItem {
            key: format!("attention-{index}"),
            task_id: item.task_id.map(|task_id| task_id.to_string()),
            severity: match item.kind {
                WorkspaceAttentionKind::Failed => AttentionSeverity::Critical,
                WorkspaceAttentionKind::Blocked
                | WorkspaceAttentionKind::ApprovalRequired
                | WorkspaceAttentionKind::CheckpointRequested
                | WorkspaceAttentionKind::HandoverRequested => AttentionSeverity::Warning,
            },
            label: item.summary.clone(),
        })
        .collect();
    let inspector = projection.inspector.as_ref().map(|inspector| {
        let elapsed = inspector.latest_attempt.as_ref().map_or_else(
            || "not started".to_owned(),
            |attempt| {
                let end = attempt.ended_at.unwrap_or_else(Utc::now);
                format!("{}s", (end - attempt.started_at).num_seconds().max(0))
            },
        );
        TaskInspector {
            task_id: inspector.task.task.task_id.to_string(),
            state: enum_name(&inspector.task.task.state).unwrap_or_else(|_| "unknown".to_owned()),
            provider: inspector
                .task
                .latest_provider
                .map_or_else(|| "unassigned".to_owned(), |provider| provider.to_string()),
            profile: inspector
                .task
                .latest_model_profile
                .clone()
                .unwrap_or_else(|| "default".to_owned()),
            effort: inspector
                .task
                .latest_effort
                .clone()
                .unwrap_or_else(|| "default".to_owned()),
            progress: inspector
                .latest_attempt
                .as_ref()
                .and_then(|attempt| attempt.outcome.clone())
                .unwrap_or_else(|| "pending".to_owned()),
            elapsed,
            dependencies: vec!["dependency graph arrives in Phase 3".to_owned()],
            worktree: inspector.active_worktree.as_ref().map_or_else(
                || "not allocated".to_owned(),
                |worktree| worktree.worktree_path.display().to_string(),
            ),
            changed_files: vec![format!("{} changed file(s)", inspector.changed_file_count)],
            tests: inspector
                .latest_verification
                .as_ref()
                .map_or_else(Vec::new, |verification| {
                    vec![format!("verification: {}", verification.outcome)]
                }),
        }
    });
    let connectivity = match daemon {
        DaemonStatus::Online(_) => DaemonConnectivity::Online,
        DaemonStatus::Stale(_) => DaemonConnectivity::Stale,
        DaemonStatus::Stopped => DaemonConnectivity::Offline,
    };
    let read_only_reason = match connectivity {
        DaemonConnectivity::Online => None,
        DaemonConnectivity::Stale => {
            Some("daemon heartbeat is stale; restart before mutating".to_owned())
        }
        DaemonConnectivity::Offline => {
            Some("daemon is offline; start it before mutating".to_owned())
        }
    };
    Ok(WorkspaceSnapshot {
        repository: repository.display().to_string(),
        session_id: projection.session.session_id.to_string(),
        session_title: projection.session.title,
        session_state: enum_name(&projection.session.state)?,
        daemon: connectivity,
        running_count: projection
            .recent_tasks
            .iter()
            .filter(|task| task.task.state == TaskState::Running)
            .count(),
        blocked_count: projection
            .recent_tasks
            .iter()
            .filter(|task| task.task.state == TaskState::Blocked)
            .count(),
        tasks,
        messages,
        has_older_messages: projection.has_older_messages,
        attention,
        inspector,
        cursor: WorkspaceCursor {
            message_ordinal: projection
                .messages
                .last()
                .map_or(0, |message| i64::try_from(message.0).unwrap_or(i64::MAX)),
            event_sequence: projection.last_event_sequence,
        },
        read_only_reason,
    })
}

fn enum_name<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("expected string enum serialization"))
}

const fn task_state_symbol(state: TaskState) -> &'static str {
    match state {
        TaskState::Running => "RUN",
        TaskState::Blocked => "BLOCK",
        TaskState::Failed => "FAIL",
        TaskState::Completed => "DONE",
        TaskState::Cancelled => "CANCEL",
        _ => "WAIT",
    }
}

fn driver_error(error: impl std::fmt::Display) -> DriverError {
    DriverError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::TimeDelta;
    use orchestrator_daemon::{MessageRedactor, process_next_client_command};
    use orchestrator_domain::{
        ClientCommand, ClientCommandAction, ClientCommandId, ClientCommandState,
        CreateSessionCommandPayload, DaemonInstanceId, SessionId,
    };
    use orchestrator_process::{RedactionConfig, Redactor};
    use orchestrator_state::{DaemonLeaseRequest, Database};
    use orchestrator_tui::chat::{
        ComposerTarget, DaemonConnectivity, WorkspaceAction, WorkspaceCursor, WorkspaceDriver,
    };

    use super::SqliteWorkspaceDriver;

    struct Adapter(Redactor);

    impl MessageRedactor for Adapter {
        fn redact(&self, value: &str) -> String {
            self.0.redact(value)
        }
    }

    fn database() -> anyhow::Result<Database> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        Ok(database)
    }

    fn create_session(database: &Database, redactor: &Adapter) -> anyhow::Result<SessionId> {
        let session_id = SessionId::new();
        let command = ClientCommand {
            command_id: ClientCommandId::new(),
            session_id: None,
            task_id: None,
            action: ClientCommandAction::CreateSession,
            payload: serde_json::to_value(CreateSessionCommandPayload {
                session_id,
                title: "test chat".to_owned(),
            })?,
            idempotency_key: "test-session".to_owned(),
            state: ClientCommandState::Pending,
            requested_by: "test".to_owned(),
            requested_at: chrono::Utc::now(),
            claimed_at: None,
            completed_at: None,
            outcome: None,
        };
        database.submit_client_command(&command)?;
        process_next_client_command(database, redactor, chrono::Utc::now())?;
        Ok(session_id)
    }

    #[test]
    fn chat_tui_driver_redacts_persists_and_becomes_read_only_offline() -> anyhow::Result<()> {
        let database = database()?;
        let redactor = Redactor::new(&RedactionConfig::default())?;
        let adapter = Adapter(redactor.clone());
        let session_id = create_session(&database, &adapter)?;
        let instance = DaemonInstanceId::new();
        database.acquire_daemon_lease(&DaemonLeaseRequest {
            instance_id: instance,
            pid: 42,
            started_at: chrono::Utc::now(),
            ttl: TimeDelta::seconds(30),
        })?;
        let mut driver = SqliteWorkspaceDriver::from_database(
            PathBuf::from("C:/repo"),
            database,
            session_id,
            None,
            redactor,
        );
        let initial = driver.refresh(&WorkspaceCursor::default())?;
        assert_eq!(initial.daemon, DaemonConnectivity::Online);
        driver.dispatch(WorkspaceAction::SubmitMessage {
            target: ComposerTarget::Orchestrator,
            content: "api_key=secret-value".to_owned(),
        })?;
        process_next_client_command(&driver.database, &adapter, chrono::Utc::now())?;
        let refreshed = driver.refresh(&WorkspaceCursor::default())?;
        assert_eq!(refreshed.messages.len(), 1);
        assert!(!refreshed.messages[0].content.contains("secret-value"));
        assert!(refreshed.messages[0].content.contains("[REDACTED]"));

        driver
            .database
            .release_daemon(instance, chrono::Utc::now())?;
        let offline = driver.refresh(&WorkspaceCursor::default())?;
        assert_eq!(offline.daemon, DaemonConnectivity::Offline);
        assert!(
            driver
                .dispatch(WorkspaceAction::SubmitMessage {
                    target: ComposerTarget::Orchestrator,
                    content: "must fail".to_owned(),
                })
                .is_err()
        );
        Ok(())
    }
}
