use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    ApproveIntegrationCommandPayload, ClientCommand, CorrelationId,
    CreateResolutionTaskCommandPayload, EventActor, EventId, EventType, IntegrationApplication,
    IntegrationApplicationId, IntegrationApproval, IntegrationBatchId, SchemaVersion, SessionId,
    SessionState, TaskEvent, TaskState,
};
use orchestrator_engine::{
    GitIntegrationManager, GitWorktree, IntegrationCandidate, IntegrationPreviewRequest,
};
use orchestrator_state::{Database, IntegrationBatchStatus, StateError};

#[derive(Clone)]
pub struct IntegrationServices {
    pub manager: Arc<GitIntegrationManager>,
    pub repository_root: std::path::PathBuf,
    pub state_root: std::path::PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum IntegrationCommandError {
    #[error("{0}")]
    Rejected(String),
    #[error(transparent)]
    State(#[from] StateError),
}

pub async fn request_integration(
    database: &Database,
    services: &IntegrationServices,
    command: &ClientCommand,
    now: DateTime<Utc>,
) -> Result<String, IntegrationCommandError> {
    let session_id = command.session_id.ok_or_else(|| {
        IntegrationCommandError::Rejected(
            "request-integration command requires a session target".to_owned(),
        )
    })?;
    if command.task_id.is_some() {
        return Err(IntegrationCommandError::Rejected(
            "request-integration cannot target one task".to_owned(),
        ));
    }
    let batch_id = IntegrationBatchId::from_uuid(command.command_id.into_uuid());
    if let Some(existing) = database.current_integration_batch(session_id)?
        && existing.preview.batch_id == batch_id
    {
        return Ok(format!("integration:{batch_id}"));
    }
    let request = build_request(database, services, session_id, batch_id, now).await?;
    let preview = services
        .manager
        .preview(&request)
        .await
        .map_err(|error| IntegrationCommandError::Rejected(error.to_string()))?;
    let stored = database.record_integration_preview(&preview)?;
    if stored.status == IntegrationBatchStatus::Blocked {
        transition_session(
            database,
            command,
            session_id,
            SessionState::NeedsAttention,
            now,
        )?;
    }
    Ok(format!("integration:{batch_id}"))
}

#[allow(clippy::too_many_lines)]
pub async fn approve_integration(
    database: &Database,
    services: &IntegrationServices,
    command: &ClientCommand,
    now: DateTime<Utc>,
) -> Result<String, IntegrationCommandError> {
    let session_id = command.session_id.ok_or_else(|| {
        IntegrationCommandError::Rejected(
            "approve-integration command requires a session target".to_owned(),
        )
    })?;
    if command.task_id.is_some() {
        return Err(IntegrationCommandError::Rejected(
            "approve-integration cannot target one task".to_owned(),
        ));
    }
    let payload: ApproveIntegrationCommandPayload = serde_json::from_value(command.payload.clone())
        .map_err(|_| {
            IntegrationCommandError::Rejected("approve-integration payload is invalid".to_owned())
        })?;
    payload
        .validate()
        .map_err(|error| IntegrationCommandError::Rejected(error.to_string()))?;
    let current = database
        .current_integration_batch(session_id)?
        .ok_or_else(|| {
            IntegrationCommandError::Rejected("integration preview does not exist".to_owned())
        })?;
    if current.preview.batch_id != payload.batch_id {
        return Err(IntegrationCommandError::Rejected(
            "integration approval batch is not current".to_owned(),
        ));
    }
    if current.status == IntegrationBatchStatus::Applied
        && current.preview.preview_hash == payload.preview_hash
    {
        return Ok(format!("integration-applied:{}", payload.batch_id));
    }
    let approval = IntegrationApproval {
        batch_id: payload.batch_id,
        preview_hash: payload.preview_hash,
        approved_by: payload.approved_by,
        approved_at: now,
    };
    let approved = database.approve_integration(&approval)?;
    transition_session(
        database,
        command,
        session_id,
        SessionState::Integrating,
        now,
    )?;
    let request = build_request(
        database,
        services,
        session_id,
        approved.preview.batch_id,
        approved.preview.created_at,
    )
    .await?;
    let application_id = IntegrationApplicationId::new();
    let identity = services
        .manager
        .worktree_identity(approved.preview.batch_id, &approved.preview.base_revision);
    database.start_integration_application(
        approved.preview.batch_id,
        application_id,
        &identity.path.to_string_lossy(),
        &identity.branch,
        now,
    )?;
    match services
        .manager
        .apply(&request, &approved.preview, &approval, application_id)
        .await
    {
        Ok((_worktree, application)) => {
            database.finish_integration_application(&application)?;
            transition_session(
                database,
                command,
                session_id,
                SessionState::Verifying,
                application.completed_at,
            )?;
            transition_session(
                database,
                command,
                session_id,
                SessionState::Completed,
                application.completed_at,
            )?;
            Ok(format!("integration-applied:{}", approved.preview.batch_id))
        }
        Err(error) => {
            let failed_at = Utc::now();
            database.finish_integration_application(&IntegrationApplication {
                application_id,
                batch_id: approved.preview.batch_id,
                preview_hash: approved.preview.preview_hash,
                integration_worktree: identity.path.to_string_lossy().into_owned(),
                integration_branch: identity.branch,
                resulting_tree: None,
                succeeded: false,
                detail_redacted: error.to_string(),
                completed_at: failed_at,
            })?;
            transition_session(
                database,
                command,
                session_id,
                SessionState::NeedsAttention,
                failed_at,
            )?;
            Err(IntegrationCommandError::Rejected(
                "integration application needs attention".to_owned(),
            ))
        }
    }
}

pub fn create_resolution_task(
    database: &Database,
    command: &ClientCommand,
    now: DateTime<Utc>,
) -> Result<String, IntegrationCommandError> {
    let session_id = command.session_id.ok_or_else(|| {
        IntegrationCommandError::Rejected(
            "create-resolution-task command requires a session target".to_owned(),
        )
    })?;
    if command.task_id.is_some() {
        return Err(IntegrationCommandError::Rejected(
            "create-resolution-task cannot target one task".to_owned(),
        ));
    }
    let payload: CreateResolutionTaskCommandPayload =
        serde_json::from_value(command.payload.clone()).map_err(|_| {
            IntegrationCommandError::Rejected(
                "create-resolution-task payload is invalid".to_owned(),
            )
        })?;
    let current = database
        .current_integration_batch(session_id)?
        .ok_or_else(|| {
            IntegrationCommandError::Rejected("integration preview does not exist".to_owned())
        })?;
    if current.preview.batch_id != payload.batch_id {
        return Err(IntegrationCommandError::Rejected(
            "integration resolution batch is not current".to_owned(),
        ));
    }
    let task_id = database.create_integration_resolution_task(
        payload.batch_id,
        &command.requested_by,
        now,
    )?;
    transition_session(database, command, session_id, SessionState::Running, now)?;
    Ok(format!("integration-resolution-task:{task_id}"))
}

fn transition_session(
    database: &Database,
    command: &ClientCommand,
    session_id: SessionId,
    next: SessionState,
    now: DateTime<Utc>,
) -> Result<(), IntegrationCommandError> {
    let session = database.load_session(session_id)?.ok_or_else(|| {
        IntegrationCommandError::Rejected("integration session does not exist".to_owned())
    })?;
    if session.state == next {
        return Ok(());
    }
    session
        .state
        .validate_transition(next)
        .map_err(|error| IntegrationCommandError::Rejected(error.to_string()))?;
    database.transition_session_with_event(
        session_id,
        session.revision,
        next,
        now,
        TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            session_id: Some(session_id),
            task_id: None,
            occurred_at: now,
            event_type: EventType::SessionStateTransitioned,
            from_state: None,
            to_state: None,
            reason: Some(format!("integration command {}", command.command_id)),
            actor: EventActor::Orchestrator,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: serde_json::json!({"command_id": command.command_id, "next": next}),
            previous_hash: None,
            event_hash: String::new(),
        },
    )?;
    Ok(())
}

async fn build_request(
    database: &Database,
    services: &IntegrationServices,
    session_id: orchestrator_domain::SessionId,
    batch_id: IntegrationBatchId,
    created_at: DateTime<Utc>,
) -> Result<IntegrationPreviewRequest, IntegrationCommandError> {
    let graph = database.current_graph(session_id)?.ok_or_else(|| {
        IntegrationCommandError::Rejected("current approved graph does not exist".to_owned())
    })?;
    if graph.revision.status != orchestrator_state::GraphRevisionStatus::Approved {
        return Err(IntegrationCommandError::Rejected(
            "current graph is not approved".to_owned(),
        ));
    }
    let dependency_map =
        graph
            .dependencies
            .iter()
            .fold(BTreeMap::<_, Vec<_>>::new(), |mut values, dependency| {
                values
                    .entry(dependency.task_id)
                    .or_default()
                    .push(dependency.depends_on_task_id);
                values
            });
    let resolution_task = database.latest_completed_resolution_task(session_id)?;
    let selected_tasks = graph
        .tasks
        .iter()
        .filter(|graph_task| resolution_task.is_none_or(|task_id| graph_task.task_id == task_id))
        .collect::<Vec<_>>();
    let mut candidates = Vec::with_capacity(selected_tasks.len());
    for graph_task in selected_tasks {
        let task = database.load_task(graph_task.task_id)?.ok_or_else(|| {
            IntegrationCommandError::Rejected("graph task disappeared".to_owned())
        })?;
        let evidence_allowed = task.state == TaskState::Completed;
        let worktree = if evidence_allowed {
            database
                .active_worktree(task.task_id)?
                .map(|stored| GitWorktree {
                    task_id: stored.task_id,
                    repository_root: stored.repo_root,
                    path: stored.worktree_path,
                    branch: stored.branch_name,
                    base_revision: stored.base_revision,
                })
        } else {
            None
        };
        candidates.push(IntegrationCandidate {
            task_id: graph_task.task_id,
            graph_order: graph_task.display_order,
            dependencies: if resolution_task.is_some() {
                Vec::new()
            } else {
                dependency_map
                    .get(&graph_task.task_id)
                    .cloned()
                    .unwrap_or_default()
            },
            worktree,
            checkpoint: if evidence_allowed {
                database.latest_sealed_checkpoint(task.task_id)?
            } else {
                None
            },
            verification: if evidence_allowed {
                database.latest_verification(task.task_id)?
            } else {
                None
            },
        });
    }
    let base_revision = services
        .manager
        .repository_head()
        .await
        .map_err(|error| IntegrationCommandError::Rejected(error.to_string()))?;
    Ok(IntegrationPreviewRequest {
        batch_id,
        session_id,
        graph_revision_id: graph.revision.revision_id,
        repository_root: services.repository_root.clone(),
        state_root: services.state_root.clone(),
        base_revision,
        candidates,
        created_at,
    })
}
