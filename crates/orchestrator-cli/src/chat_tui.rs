use std::{
    path::{Path, PathBuf},
    str::FromStr as _,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use orchestrator_domain::{
    AppendMessageCommandPayload, ApproveGraphCommandPayload, ApproveIntegrationCommandPayload,
    ClientCommand, ClientCommandAction, ClientCommandId, ClientCommandState,
    CreateResolutionTaskCommandPayload, CreateSessionCommandPayload, GraphRevisionId,
    GraphValidationSummary, IntegrationBatchId, IntegrationBlocker, MessageId, MessageKind,
    MessageRole, ProviderId, RequestPlanCommandPayload, SessionId, TaskId, TaskState,
};
use orchestrator_process::{RedactionConfig, Redactor};
use orchestrator_state::{
    ControlAction as StateControlAction, DaemonStatus, Database, IntegrationBatchStatus,
    RepositoryStatePaths, RootConfig, SessionListFilter, StoredIntegrationBatch,
    WorkspaceAttentionKind, WorkspaceProjection, WorkspaceReadRequest,
};
use orchestrator_tui::chat::{
    ActionFeedback, AttentionItem, AttentionSeverity, ComposerTarget, DaemonConnectivity,
    DriverError, IntegrationApprovalCard, IntegrationSourceSummary, PlanApprovalCard,
    PlanNodeSummary, TaskControlIntent, TaskInspector, TaskSummary, TimelineEntry, WorkspaceAction,
    WorkspaceCursor, WorkspaceDriver, WorkspaceSnapshot,
};
use serde::Serialize;
use serde_json::json;

const DEFAULT_SESSION_KEY: &str = "chat-default-session-v1";
const COMMAND_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub(crate) struct SqliteWorkspaceDriver {
    repository: PathBuf,
    state_root: PathBuf,
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
            state_root: paths.root,
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
            state_root: repository.join(".colay"),
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
                let task_ids: Vec<TaskId> = self
                    .database
                    .with_connection(|connection| {
                        let mut statement = connection.prepare(
                            "SELECT st.task_id FROM session_tasks st
                             JOIN session_graph_heads gh ON gh.session_id = st.session_id
                                                        AND gh.revision_id = st.revision_id
                             JOIN tasks t ON t.task_id = st.task_id
                             WHERE st.session_id = ?1 AND t.state = 'running'
                             ORDER BY st.display_order",
                        )?;
                        let values = statement
                            .query_map([self.session_id.to_string()], |row| {
                                row.get::<_, String>(0)
                            })?
                            .collect::<Result<Vec<_>, _>>()?;
                        values
                            .into_iter()
                            .map(|value| {
                                TaskId::from_str(&value).map_err(|error| {
                                    orchestrator_state::StateError::InvalidRecord(format!(
                                        "invalid running task ID: {error}"
                                    ))
                                })
                            })
                            .collect()
                    })
                    .map_err(driver_error)?;
                if task_ids.is_empty() {
                    return Ok(ActionFeedback::unavailable(
                        "broadcast messaging: no running graph tasks",
                    ));
                }
                for task_id in &task_ids {
                    self.enqueue_message(Some(*task_id), content)?;
                }
                return Ok(ActionFeedback::info(format!(
                    "instruction queued for {} running tasks",
                    task_ids.len()
                )));
            }
        };
        self.enqueue_message(task_id, content)?;
        Ok(ActionFeedback::info(
            "message accepted by repository daemon",
        ))
    }

    fn enqueue_message(&self, task_id: Option<TaskId>, content: &str) -> Result<(), DriverError> {
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
        Ok(())
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

    fn request_plan(&self, goal_message_id: &str) -> Result<ActionFeedback, DriverError> {
        if !self.online()? {
            return Err(DriverError::new(
                "daemon is offline or stale; planning is read-only",
            ));
        }
        let goal_message_id = MessageId::from_str(goal_message_id)
            .map_err(|error| DriverError::new(format!("invalid goal message ID: {error}")))?;
        let command_id = ClientCommandId::new();
        let command = ClientCommand {
            command_id,
            session_id: Some(self.session_id),
            task_id: None,
            action: ClientCommandAction::RequestPlan,
            payload: serde_json::to_value(RequestPlanCommandPayload { goal_message_id })
                .map_err(driver_error)?,
            idempotency_key: format!("chat-plan-{command_id}"),
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
        Ok(ActionFeedback::info("task graph planning requested"))
    }

    fn approve_graph(
        &self,
        revision_id: &str,
        proposal_hash: &str,
        approved_by: &str,
    ) -> Result<ActionFeedback, DriverError> {
        if !self.online()? {
            return Err(DriverError::new(
                "daemon is offline or stale; graph approval is read-only",
            ));
        }
        let revision_id = GraphRevisionId::from_str(revision_id)
            .map_err(|error| DriverError::new(format!("invalid graph revision ID: {error}")))?;
        let revision = self
            .database
            .load_graph_revision(revision_id)
            .map_err(driver_error)?
            .ok_or_else(|| DriverError::new("graph revision no longer exists"))?;
        if revision.proposal_hash.as_deref() != Some(proposal_hash) {
            return Err(DriverError::new(
                "approval card proposal hash is stale; refresh the workspace",
            ));
        }
        let validation = serde_json::from_value::<GraphValidationSummary>(revision.validation)
            .map_err(driver_error)?;
        let authority = validation.authority.ok_or_else(|| {
            DriverError::new("graph has no sealed requirement and repository validation authority")
        })?;
        let payload = ApproveGraphCommandPayload {
            revision_id,
            requirement_revision_id: authority.requirement_revision_id,
            validation_hash: authority.validation_hash,
            base_commit: authority.base_commit,
            proposal_hash: proposal_hash.to_owned(),
            approved_by: approved_by.to_owned(),
        };
        payload
            .validate()
            .map_err(|error| DriverError::new(error.to_string()))?;
        let command_id = ClientCommandId::new();
        let command = ClientCommand {
            command_id,
            session_id: Some(self.session_id),
            task_id: None,
            action: ClientCommandAction::ApproveGraph,
            payload: serde_json::to_value(&payload).map_err(driver_error)?,
            idempotency_key: format!(
                "chat-approve-{}-{}",
                payload.revision_id, payload.proposal_hash
            ),
            state: ClientCommandState::Pending,
            requested_by: approved_by.to_owned(),
            requested_at: Utc::now(),
            claimed_at: None,
            completed_at: None,
            outcome: None,
        };
        self.database
            .submit_client_command(&command)
            .map_err(driver_error)?;
        Ok(ActionFeedback::info("exact task graph approval accepted"))
    }

    fn request_integration(&self) -> Result<ActionFeedback, DriverError> {
        if !self.online()? {
            return Err(DriverError::new(
                "daemon is offline or stale; integration preview is read-only",
            ));
        }
        self.submit_integration_command(
            ClientCommandAction::RequestIntegration,
            json!({}),
            format!("chat-integration-preview-{}", ClientCommandId::new()),
            "local-tui",
        )?;
        Ok(ActionFeedback::info("exact integration preview requested"))
    }

    fn approve_integration(
        &self,
        batch_id: &str,
        preview_hash: &str,
        approved_by: &str,
    ) -> Result<ActionFeedback, DriverError> {
        if !self.online()? {
            return Err(DriverError::new(
                "daemon is offline or stale; integration approval is read-only",
            ));
        }
        let payload = ApproveIntegrationCommandPayload {
            batch_id: IntegrationBatchId::from_str(batch_id).map_err(|error| {
                DriverError::new(format!("invalid integration batch ID: {error}"))
            })?,
            preview_hash: preview_hash.to_owned(),
            approved_by: approved_by.to_owned(),
        };
        payload
            .validate()
            .map_err(|error| DriverError::new(error.to_string()))?;
        self.submit_integration_command(
            ClientCommandAction::ApproveIntegration,
            serde_json::to_value(&payload).map_err(driver_error)?,
            format!(
                "chat-integration-approve-{}-{}",
                payload.batch_id, payload.preview_hash
            ),
            approved_by,
        )?;
        Ok(ActionFeedback::info(
            "exact integration preview approval accepted",
        ))
    }

    fn create_resolution_task(&self, batch_id: &str) -> Result<ActionFeedback, DriverError> {
        if !self.online()? {
            return Err(DriverError::new(
                "daemon is offline or stale; resolution task creation is read-only",
            ));
        }
        let payload = CreateResolutionTaskCommandPayload {
            batch_id: IntegrationBatchId::from_str(batch_id).map_err(|error| {
                DriverError::new(format!("invalid integration batch ID: {error}"))
            })?,
        };
        self.submit_integration_command(
            ClientCommandAction::CreateResolutionTask,
            serde_json::to_value(&payload).map_err(driver_error)?,
            format!("chat-integration-resolve-{}", payload.batch_id),
            "local-tui",
        )?;
        Ok(ActionFeedback::info(
            "integration resolution task requested",
        ))
    }

    fn submit_integration_command(
        &self,
        action: ClientCommandAction,
        payload: serde_json::Value,
        idempotency_key: String,
        requested_by: &str,
    ) -> Result<(), DriverError> {
        let command = ClientCommand {
            command_id: ClientCommandId::new(),
            session_id: Some(self.session_id),
            task_id: None,
            action,
            payload,
            idempotency_key,
            state: ClientCommandState::Pending,
            requested_by: requested_by.to_owned(),
            requested_at: Utc::now(),
            claimed_at: None,
            completed_at: None,
            outcome: None,
        };
        self.database
            .submit_client_command(&command)
            .map(|_| ())
            .map_err(driver_error)
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
        let mut snapshot =
            projection_to_snapshot(&self.repository, projection, &daemon).map_err(driver_error)?;
        if let Some(plan) = snapshot.plan_approval.as_mut() {
            let requirement = self
                .database
                .current_requirement_revision(self.session_id)
                .map_err(driver_error)?;
            if let Some(requirement) = requirement.filter(|requirement| {
                plan.requirement_revision_id.as_deref()
                    == Some(requirement.requirement_revision_id.to_string().as_str())
            }) {
                plan.objective = requirement.snapshot.objective;
                plan.in_scope = requirement.snapshot.in_scope;
                plan.out_of_scope = requirement.snapshot.out_of_scope;
                plan.acceptance_criteria = requirement.snapshot.acceptance_criteria;
                plan.verification_commands = requirement
                    .snapshot
                    .verification_plan
                    .into_iter()
                    .map(|command| {
                        format!(
                            "{} {}",
                            command.executable,
                            serde_json::to_string(&command.args)
                                .unwrap_or_else(|_| "[]".to_owned())
                        )
                    })
                    .collect();
                plan.risks.extend(requirement.snapshot.risks);
                plan.risks.sort();
                plan.risks.dedup();
                plan.required_approvals = vec!["exact validated graph approval".to_owned()];
                if plan
                    .nodes
                    .iter()
                    .any(|node| node.repository_wide_write_scope)
                {
                    plan.required_approvals
                        .push("repository-wide write scope acknowledgement".to_owned());
                }
                if !plan.risks.is_empty() {
                    plan.required_approvals
                        .push("recorded risk acknowledgement".to_owned());
                }
            }
        }
        snapshot.integration_approval = self
            .database
            .current_integration_batch(self.session_id)
            .map_err(driver_error)?
            .as_ref()
            .and_then(|batch| integration_to_card(&self.state_root, batch));
        snapshot.validate().map_err(driver_error)?;
        Ok(snapshot)
    }

    fn dispatch(&mut self, action: WorkspaceAction) -> Result<ActionFeedback, DriverError> {
        match action {
            WorkspaceAction::SubmitMessage { target, content } => {
                self.submit_message(target, &content)
            }
            WorkspaceAction::RequestTaskControl { task_id, intent } => {
                self.request_control(&task_id, intent)
            }
            WorkspaceAction::RequestPlan { goal_message_id } => self.request_plan(&goal_message_id),
            WorkspaceAction::ApproveGraph {
                revision_id,
                proposal_hash,
                approved_by,
            } => self.approve_graph(&revision_id, &proposal_hash, &approved_by),
            WorkspaceAction::RequestIntegration => self.request_integration(),
            WorkspaceAction::ApproveIntegration {
                batch_id,
                preview_hash,
                approved_by,
            } => self.approve_integration(&batch_id, &preview_hash, &approved_by),
            WorkspaceAction::CreateResolutionTask { batch_id } => {
                self.create_resolution_task(&batch_id)
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
    let graph_node_labels = projection
        .recent_tasks
        .iter()
        .map(|task| {
            (
                task.task.task_id,
                task.graph_node_key
                    .clone()
                    .unwrap_or_else(|| task.task.objective.clone()),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    let tasks = projection
        .recent_tasks
        .iter()
        .map(|task| TaskSummary {
            task_id: task.task.task_id.to_string(),
            title: task.task.objective.clone(),
            state: enum_name(&task.task.state).unwrap_or_else(|_| "unknown".to_owned()),
            state_symbol: task_state_symbol(task.task.state).to_owned(),
            dependency_status: format!(
                "{} | {} | {}",
                if task.graph_node_key.is_some() {
                    if task.dependency_task_ids.is_empty() {
                        "ready (no dependencies)".to_owned()
                    } else {
                        format!(
                            "after {}",
                            task.dependency_task_ids
                                .iter()
                                .map(|task_id| graph_node_labels
                                    .get(task_id)
                                    .cloned()
                                    .unwrap_or_else(|| task_id.to_string()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                } else {
                    "repository task".to_owned()
                },
                task_execution_status(task),
                task_instruction_status(task)
            ),
            needs_attention: attention_task_ids.contains(&task.task.task_id),
        })
        .collect::<Vec<_>>();
    let latest_session_user_message_id = projection
        .messages
        .iter()
        .rev()
        .find(|(_, message)| message.task_id.is_none() && message.role == MessageRole::User)
        .map(|(_, message)| message.message_id);
    let plan_approval = projection
        .current_graph
        .as_ref()
        .and_then(|workspace_graph| {
            let revision = &workspace_graph.graph.revision;
            if revision.status != orchestrator_state::GraphRevisionStatus::AwaitingApproval {
                return None;
            }
            let proposal = revision.proposal.as_ref()?;
            if latest_session_user_message_id != Some(proposal.goal_message_id) {
                return None;
            }
            let proposal_hash = revision.proposal_hash.clone()?;
            let validation =
                serde_json::from_value::<GraphValidationSummary>(revision.validation.clone())
                    .ok()?;
            let mut risks = proposal
                .nodes
                .iter()
                .flat_map(|node| node.risks.iter())
                .filter_map(|risk| enum_name(risk).ok())
                .collect::<Vec<_>>();
            risks.sort();
            risks.dedup();
            let authority = validation.authority.as_ref();
            Some(PlanApprovalCard {
                revision_id: revision.revision_id.to_string(),
                proposal_hash,
                objective: String::new(),
                in_scope: Vec::new(),
                out_of_scope: Vec::new(),
                acceptance_criteria: Vec::new(),
                verification_commands: Vec::new(),
                requirement_revision_id: authority
                    .map(|value| value.requirement_revision_id.to_string()),
                validation_hash: authority.map(|value| value.validation_hash.clone()),
                git_root_redacted: authority.map(|value| value.git_root_redacted.clone()),
                base_commit: authority.map(|value| value.base_commit.clone()),
                validation_checks: authority
                    .map_or_else(Vec::new, |value| value.validation_checks.clone()),
                required_approvals: Vec::new(),
                nodes: proposal
                    .nodes
                    .iter()
                    .map(|node| PlanNodeSummary {
                        key: node.key.clone(),
                        title: node.title.clone(),
                        objective: node.objective.clone(),
                        dependencies: node.dependencies.clone(),
                        constraints: node.constraints.clone(),
                        acceptance_criteria: node.acceptance_criteria.clone(),
                        provider: node
                            .provider
                            .unwrap_or(proposal.planner_provider)
                            .to_string(),
                        profile: enum_name(&node.profile).unwrap_or_else(|_| "unknown".to_owned()),
                        write_scopes: node.write_scopes.iter().map(ToString::to_string).collect(),
                        repository_wide_write_scope: node.repository_wide_write_scope,
                        risks: node
                            .risks
                            .iter()
                            .filter_map(|risk| enum_name(risk).ok())
                            .collect(),
                        parallel_safety: node.parallel_safety.clone(),
                    })
                    .collect(),
                proposed_parallelism: validation.maximum_parallel_width,
                risks,
            })
        });
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
            dependencies: inspector
                .task
                .dependency_task_ids
                .iter()
                .map(|task_id| {
                    graph_node_labels
                        .get(task_id)
                        .cloned()
                        .unwrap_or_else(|| task_id.to_string())
                })
                .collect(),
            schedule: task_execution_status(&inspector.task),
            instructions: inspector
                .instructions
                .iter()
                .map(|instruction| {
                    format!(
                        "#{} {}: {}",
                        instruction.ordinal,
                        enum_name(&instruction.state).unwrap_or_else(|_| "unknown".to_owned()),
                        instruction
                            .content_redacted
                            .chars()
                            .take(80)
                            .collect::<String>()
                    )
                })
                .collect(),
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
        DaemonStatus::Booting(_)
        | DaemonStatus::Probing(_)
        | DaemonStatus::Failed(_)
        | DaemonStatus::Stopped => DaemonConnectivity::Offline,
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
        plan_approval,
        integration_approval: None,
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

fn integration_to_card(
    state_root: &Path,
    batch: &StoredIntegrationBatch,
) -> Option<IntegrationApprovalCard> {
    if !matches!(
        batch.status,
        IntegrationBatchStatus::Preview
            | IntegrationBatchStatus::Blocked
            | IntegrationBatchStatus::NeedsAttention
    ) {
        return None;
    }
    let preview = &batch.preview;
    Some(IntegrationApprovalCard {
        batch_id: preview.batch_id.to_string(),
        preview_hash: preview.preview_hash.clone(),
        base_revision: preview.base_revision.clone(),
        destination: batch.application.as_ref().map_or_else(
            || {
                state_root
                    .join("integration")
                    .join(preview.batch_id.to_string())
                    .display()
                    .to_string()
            },
            |application| application.integration_worktree.clone(),
        ),
        sources: preview
            .sources
            .iter()
            .map(|source| IntegrationSourceSummary {
                task_id: source.task_id.to_string(),
                checkpoint_id: source.checkpoint_id.to_string(),
                verification_id: source.verification_id.to_string(),
                diff_sha256: source.diff_sha256.clone(),
                changed_files: source
                    .changed_files
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            })
            .collect(),
        blockers: preview
            .blockers
            .iter()
            .map(integration_blocker_label)
            .chain(batch.application.as_ref().and_then(|application| {
                (!application.succeeded && !application.detail_redacted.trim().is_empty())
                    .then(|| format!("application: {}", application.detail_redacted))
            }))
            .collect(),
        approvable: batch.status == IntegrationBatchStatus::Preview && preview.is_approvable(),
        resolution_available: batch.status == IntegrationBatchStatus::NeedsAttention
            || preview.blockers.iter().any(|blocker| {
                matches!(
                    blocker,
                    IntegrationBlocker::PathOverlap { .. } | IntegrationBlocker::PatchFailed { .. }
                )
            }),
    })
}

fn integration_blocker_label(blocker: &IntegrationBlocker) -> String {
    match blocker {
        IntegrationBlocker::MissingEvidence { task_id, detail } => {
            format!("{task_id}: missing evidence ({detail})")
        }
        IntegrationBlocker::VerificationFailed { task_id } => {
            format!("{task_id}: verification failed")
        }
        IntegrationBlocker::StaleBase { task_id, found } => {
            format!("{task_id}: stale base {found}")
        }
        IntegrationBlocker::SourceChanged { task_id } => {
            format!("{task_id}: source changed after checkpoint")
        }
        IntegrationBlocker::PathOverlap { left, right, path } => {
            format!("{left} and {right}: overlapping path {path}")
        }
        IntegrationBlocker::PatchFailed { task_id, detail } => {
            format!("{task_id}: patch failed ({detail})")
        }
    }
}

fn task_execution_status(task: &orchestrator_state::WorkspaceTask) -> String {
    if task.active_schedule_claim_id.is_some() {
        return format!(
            "claimed by {}",
            task.latest_provider
                .map_or_else(|| "scheduler".to_owned(), |provider| provider.to_string())
        );
    }
    match task.task.state {
        TaskState::Queued => "awaiting scheduler".to_owned(),
        TaskState::Analyzing | TaskState::Planned => "starting isolated worker".to_owned(),
        TaskState::Running => format!(
            "running on {}",
            task.latest_provider
                .map_or_else(|| "provider".to_owned(), |provider| provider.to_string())
        ),
        TaskState::Verifying => "verifying Git evidence".to_owned(),
        TaskState::Completed => "verified complete".to_owned(),
        TaskState::Failed => "execution failed".to_owned(),
        TaskState::Blocked => "blocked".to_owned(),
        state => enum_name(&state).unwrap_or_else(|_| "unknown".to_owned()),
    }
}

fn task_instruction_status(task: &orchestrator_state::WorkspaceTask) -> String {
    if task.queued_instruction_count == 0 && task.applying_instruction_count == 0 {
        return task.latest_instruction_state.map_or_else(
            || "no instructions".to_owned(),
            |state| {
                format!(
                    "last instruction {}",
                    enum_name(&state).unwrap_or_else(|_| "unknown".to_owned())
                )
            },
        );
    }
    format!(
        "{} queued / {} applying",
        task.queued_instruction_count, task.applying_instruction_count
    )
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
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::PathBuf,
    };

    use anyhow::anyhow;
    use chrono::TimeDelta;
    use orchestrator_daemon::{MessageRedactor, process_next_client_command};
    use orchestrator_domain::{
        ApproveGraphCommandPayload, ApproveIntegrationCommandPayload, CheckpointId, ClientCommand,
        ClientCommandAction, ClientCommandId, ClientCommandState,
        CreateResolutionTaskCommandPayload, CreateSessionCommandPayload, DaemonInstanceId,
        GraphRevisionId, GraphValidationAuthority, GraphValidationPolicy, IntegrationBatchId,
        IntegrationPreview, IntegrationSource, ModelProfile, PlanningAttemptId, ProviderId,
        RepoPath, RequestPlanCommandPayload, RequirementRevision, RequirementRevisionId,
        RequirementSnapshot, RiskTag, SchemaVersion, SessionId, TaskGraphNode, TaskGraphProposal,
        TaskId, VerificationCommand, VerificationId, validate_task_graph,
        validate_task_graph_with_authority,
    };
    use orchestrator_process::{RedactionConfig, Redactor};
    use orchestrator_state::{
        DaemonLeaseRequest, Database, GraphApprovalRequest, IntegrationBatchStatus,
        NewGraphAttempt, StoredIntegrationBatch, WorkspaceReadRequest,
    };
    use orchestrator_tui::chat::{
        ComposerTarget, DaemonConnectivity, WorkspaceAction, WorkspaceCursor, WorkspaceDriver,
    };

    use super::{SqliteWorkspaceDriver, integration_to_card};

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

    #[test]
    #[allow(clippy::too_many_lines)]
    fn chat_tui_projects_full_plan_card_and_dependency_labels() -> anyhow::Result<()> {
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
        driver.dispatch(WorkspaceAction::SubmitMessage {
            target: ComposerTarget::Orchestrator,
            content: "build graph".to_owned(),
        })?;
        process_next_client_command(&driver.database, &adapter, chrono::Utc::now())?;
        let goal_id = driver
            .database
            .read_workspace_projection(WorkspaceReadRequest {
                session_id,
                selected_task_id: None,
                before_ordinal: None,
                message_limit: 10,
                task_limit: 10,
            })?
            .messages
            .last()
            .map(|(_, message)| message.message_id)
            .ok_or_else(|| anyhow!("goal message missing"))?;
        let node =
            |key: &str, dependencies: &[&str], scope: &str, risks: Vec<RiskTag>| TaskGraphNode {
                key: key.to_owned(),
                title: format!("{key} title"),
                objective: format!("implement {key}"),
                dependencies: dependencies
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect(),
                constraints: vec!["local only".to_owned()],
                acceptance_criteria: vec!["tests pass".to_owned()],
                provider: Some(ProviderId::Codex),
                profile: ModelProfile::Standard,
                write_scopes: RepoPath::try_from(scope).ok().into_iter().collect(),
                repository_wide_write_scope: false,
                risks,
                parallel_safety: "dependency ordered".to_owned(),
            };
        let graph = validate_task_graph(
            TaskGraphProposal {
                schema_version: SchemaVersion::v1(),
                revision_id: GraphRevisionId::new(),
                session_id,
                goal_message_id: goal_id,
                planner_provider: ProviderId::Codex,
                proposed_at: chrono::Utc::now(),
                nodes: vec![
                    node("domain", &[], "src/domain", vec![RiskTag::Concurrency]),
                    node("ui", &["domain"], "src/ui", Vec::new()),
                ],
            },
            &GraphValidationPolicy {
                eligible_providers: BTreeSet::from([ProviderId::Codex]),
                eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
                max_parallel_workers: 2,
                per_provider_limits: BTreeMap::from([(ProviderId::Codex, 2)]),
            },
        )?;
        driver
            .database
            .record_graph_attempt(&NewGraphAttempt::from_validated(
                PlanningAttemptId::new(),
                graph.clone(),
                chrono::Utc::now(),
                chrono::Utc::now(),
            ))?;

        let proposed = driver.refresh(&WorkspaceCursor::default())?;
        let card = proposed
            .plan_approval
            .as_ref()
            .ok_or_else(|| anyhow!("plan approval card missing"))?;
        assert_eq!(card.revision_id, graph.proposal.revision_id.to_string());
        assert_eq!(card.proposal_hash, graph.proposal_hash);
        assert_eq!(card.nodes[1].dependencies, vec!["domain"]);
        assert_eq!(card.nodes[0].constraints, vec!["local only"]);
        assert_eq!(card.nodes[0].acceptance_criteria, vec!["tests pass"]);
        assert_eq!(card.nodes[0].write_scopes, vec!["src/domain"]);
        assert_eq!(card.nodes[0].provider, "codex");
        assert_eq!(card.nodes[0].profile, "standard");
        assert_eq!(card.risks, vec!["concurrency"]);
        assert_eq!(card.proposed_parallelism, 1);
        proposed.validate()?;

        driver
            .database
            .approve_graph_and_materialize_tasks(&GraphApprovalRequest {
                revision_id: graph.proposal.revision_id,
                expected_proposal_hash: graph.proposal_hash,
                authority: None,
                approved_by: "test".to_owned(),
                approved_at: chrono::Utc::now(),
            })?;
        let approved = driver.refresh(&WorkspaceCursor::default())?;
        assert!(approved.plan_approval.is_none());
        assert_eq!(approved.tasks.len(), 2);
        assert_eq!(
            approved.tasks[0].dependency_status,
            "ready (no dependencies) | awaiting scheduler | no instructions"
        );
        assert_eq!(
            approved.tasks[1].dependency_status,
            "after domain | awaiting scheduler | no instructions"
        );
        approved.validate()?;

        let target = approved.tasks[0].task_id.clone();
        driver.dispatch(WorkspaceAction::SubmitMessage {
            target: ComposerTarget::Task(target.clone()),
            content: "also update the focused tests".to_owned(),
        })?;
        process_next_client_command(&driver.database, &adapter, chrono::Utc::now())?;
        driver.selection_changed(Some(&target))?;
        let instructed = driver.refresh(&WorkspaceCursor::default())?;
        assert!(
            instructed.tasks[0]
                .dependency_status
                .contains("1 queued / 0 applying")
        );
        let inspector = instructed
            .inspector
            .as_ref()
            .ok_or_else(|| anyhow!("instruction inspector missing"))?;
        assert_eq!(inspector.schedule, "awaiting scheduler");
        assert!(inspector.instructions[0].contains("also update the focused tests"));
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn chat_tui_submits_typed_plan_and_exact_approval_commands() -> anyhow::Result<()> {
        let database = database()?;
        let redactor = Redactor::new(&RedactionConfig::default())?;
        let adapter = Adapter(redactor.clone());
        let session_id = create_session(&database, &adapter)?;
        database.acquire_daemon_lease(&DaemonLeaseRequest {
            instance_id: DaemonInstanceId::new(),
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
        let goal_message_id = orchestrator_domain::MessageId::new();
        driver.database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO conversation_messages(
                    message_id, session_id, ordinal, role, kind, state,
                    content_redacted, created_at, finalized_at
                 ) VALUES (?1, ?2, 1, 'user', 'user_message', 'final', ?3, ?4, ?4)",
                rusqlite::params![
                    goal_message_id.to_string(),
                    session_id.to_string(),
                    "test approval goal",
                    chrono::Utc::now().to_rfc3339(),
                ],
            )?;
            Ok(())
        })?;
        let requirement = RequirementRevision::seal(
            RequirementRevisionId::new(),
            session_id,
            goal_message_id,
            1,
            RequirementSnapshot {
                objective: "test typed approval authority".to_owned(),
                in_scope: vec!["typed approval payload".to_owned()],
                out_of_scope: Vec::new(),
                constraints: vec!["local only".to_owned()],
                acceptance_criteria: vec!["typed payload is exact".to_owned()],
                verification_plan: vec![VerificationCommand {
                    executable: "cargo".to_owned(),
                    args: vec!["test".to_owned()],
                }],
                risks: Vec::new(),
                open_questions: Vec::new(),
            },
            chrono::Utc::now(),
        )?;
        driver.database.record_requirement_revision(&requirement)?;
        let authority = GraphValidationAuthority {
            requirement_revision_id: requirement.requirement_revision_id,
            validation_hash: "b".repeat(64),
            base_commit: "c".repeat(40),
            git_root_redacted: "C:/repo".to_owned(),
            validation_checks: vec!["git head is ready".to_owned()],
        };
        let graph = validate_task_graph_with_authority(
            TaskGraphProposal {
                schema_version: SchemaVersion::v1(),
                revision_id: GraphRevisionId::new(),
                session_id,
                goal_message_id,
                planner_provider: ProviderId::Codex,
                proposed_at: chrono::Utc::now(),
                nodes: vec![TaskGraphNode {
                    key: "test".to_owned(),
                    title: "test approval".to_owned(),
                    objective: "test typed approval authority".to_owned(),
                    dependencies: Vec::new(),
                    constraints: vec!["local only".to_owned()],
                    acceptance_criteria: vec!["typed payload is exact".to_owned()],
                    provider: Some(ProviderId::Codex),
                    profile: ModelProfile::Standard,
                    write_scopes: vec![RepoPath::try_from("src")?],
                    repository_wide_write_scope: false,
                    risks: Vec::new(),
                    parallel_safety: "one task".to_owned(),
                }],
            },
            &GraphValidationPolicy {
                eligible_providers: BTreeSet::from([ProviderId::Codex]),
                eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
                max_parallel_workers: 1,
                per_provider_limits: BTreeMap::from([(ProviderId::Codex, 1)]),
            },
            authority.clone(),
        )?;
        driver
            .database
            .record_graph_attempt(&NewGraphAttempt::from_validated(
                PlanningAttemptId::new(),
                graph.clone(),
                chrono::Utc::now(),
                chrono::Utc::now(),
            ))?;
        driver.dispatch(WorkspaceAction::RequestPlan {
            goal_message_id: goal_message_id.to_string(),
        })?;
        let revision_id = graph.proposal.revision_id;
        let proposal_hash = graph.proposal_hash;
        driver.dispatch(WorkspaceAction::ApproveGraph {
            revision_id: revision_id.to_string(),
            proposal_hash: proposal_hash.clone(),
            approved_by: "operator".to_owned(),
        })?;
        driver.dispatch(WorkspaceAction::RequestIntegration)?;
        let batch_id = IntegrationBatchId::new();
        let preview_hash = "b".repeat(64);
        driver.dispatch(WorkspaceAction::ApproveIntegration {
            batch_id: batch_id.to_string(),
            preview_hash: preview_hash.clone(),
            approved_by: "operator".to_owned(),
        })?;
        driver.dispatch(WorkspaceAction::CreateResolutionTask {
            batch_id: batch_id.to_string(),
        })?;

        driver.database.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT action, payload_json, requested_by FROM client_commands
                 WHERE action IN ('request_plan', 'approve_graph', 'request_integration',
                                  'approve_integration', 'create_resolution_task')
                 ORDER BY requested_at, rowid",
            )?;
            let commands = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(commands.len(), 5);
            assert_eq!(commands[0].0, "request_plan");
            assert_eq!(
                serde_json::from_str::<RequestPlanCommandPayload>(&commands[0].1)?.goal_message_id,
                goal_message_id
            );
            let approval = serde_json::from_str::<ApproveGraphCommandPayload>(&commands[1].1)?;
            assert_eq!(commands[1].0, "approve_graph");
            assert_eq!(approval.revision_id, revision_id);
            assert_eq!(approval.proposal_hash, proposal_hash);
            assert_eq!(
                approval.requirement_revision_id,
                authority.requirement_revision_id
            );
            assert_eq!(approval.validation_hash, authority.validation_hash);
            assert_eq!(approval.base_commit, authority.base_commit);
            assert_eq!(approval.approved_by, "operator");
            assert_eq!(commands[1].2, "operator");
            assert_eq!(commands[2].0, "request_integration");
            assert_eq!(commands[2].1, "{}");
            let integration =
                serde_json::from_str::<ApproveIntegrationCommandPayload>(&commands[3].1)?;
            assert_eq!(commands[3].0, "approve_integration");
            assert_eq!(integration.batch_id, batch_id);
            assert_eq!(integration.preview_hash, preview_hash);
            assert_eq!(integration.approved_by, "operator");
            let resolution =
                serde_json::from_str::<CreateResolutionTaskCommandPayload>(&commands[4].1)?;
            assert_eq!(commands[4].0, "create_resolution_task");
            assert_eq!(resolution.batch_id, batch_id);
            Ok(())
        })?;
        assert!(
            driver
                .dispatch(WorkspaceAction::ApproveGraph {
                    revision_id: revision_id.to_string(),
                    proposal_hash: "not-a-hash".to_owned(),
                    approved_by: "operator".to_owned(),
                })
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn integration_preview_projects_exact_tui_authority_fields() -> anyhow::Result<()> {
        let task_id = TaskId::new();
        let preview = IntegrationPreview::seal(
            IntegrationBatchId::new(),
            SessionId::new(),
            GraphRevisionId::new(),
            "a".repeat(40),
            vec![IntegrationSource {
                task_id,
                checkpoint_id: CheckpointId::new(),
                verification_id: VerificationId::new(),
                base_revision: "a".repeat(40),
                diff_sha256: "b".repeat(64),
                changed_files: vec![RepoPath::try_from("src/lib.rs")?],
            }],
            Vec::new(),
            chrono::Utc::now(),
        )?;
        let card = integration_to_card(
            std::path::Path::new("C:/repo/.colay"),
            &StoredIntegrationBatch {
                preview: preview.clone(),
                status: IntegrationBatchStatus::Preview,
                approval: None,
                application: None,
            },
        )
        .ok_or_else(|| anyhow!("integration card missing"))?;
        assert_eq!(card.batch_id, preview.batch_id.to_string());
        assert_eq!(card.preview_hash, preview.preview_hash);
        assert_eq!(card.base_revision, "a".repeat(40));
        assert_eq!(card.sources[0].task_id, task_id.to_string());
        assert_eq!(card.sources[0].changed_files, vec!["src/lib.rs"]);
        assert!(card.destination.contains(&preview.batch_id.to_string()));
        assert!(card.approvable);
        Ok(())
    }
}
