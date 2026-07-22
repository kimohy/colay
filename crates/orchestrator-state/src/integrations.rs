use std::str::FromStr as _;

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    CorrelationId, EventActor, EventId, EventType, IntegrationApplication,
    IntegrationApplicationId, IntegrationApproval, IntegrationBatchId, IntegrationBlocker,
    IntegrationPreview, SchemaVersion, SessionId, SessionState, TaskEnvelope, TaskEvent, TaskId,
    TaskState,
};
use rusqlite::{OptionalExtension as _, TransactionBehavior, params};

use crate::{Database, StateError, StateResult, database::append_event_in_transaction};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrationBatchStatus {
    Preview,
    Blocked,
    Approved,
    Applying,
    Applied,
    NeedsAttention,
    Superseded,
}

impl IntegrationBatchStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Blocked => "blocked",
            Self::Approved => "approved",
            Self::Applying => "applying",
            Self::Applied => "applied",
            Self::NeedsAttention => "needs_attention",
            Self::Superseded => "superseded",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredIntegrationBatch {
    pub preview: IntegrationPreview,
    pub status: IntegrationBatchStatus,
    pub approval: Option<IntegrationApproval>,
    pub application: Option<IntegrationApplication>,
}

impl Database {
    pub fn record_integration_preview(
        &self,
        preview: &IntegrationPreview,
    ) -> StateResult<StoredIntegrationBatch> {
        if !preview.verify_integrity() {
            return Err(StateError::InvalidRecord(
                "integration preview integrity check failed".to_owned(),
            ));
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let current_revision: Option<String> = transaction
            .query_row(
                "SELECT revision_id FROM session_graph_heads WHERE session_id = ?1",
                [preview.session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if current_revision.as_deref() != Some(&preview.graph_revision_id.to_string()) {
            return Err(StateError::InvalidRecord(
                "integration preview graph is not the current session graph".to_owned(),
            ));
        }
        transaction.execute(
            "UPDATE integration_batches SET status = 'superseded', completed_at = ?1
             WHERE session_id = ?2 AND status IN ('preview', 'blocked')",
            params![
                preview.created_at.to_rfc3339(),
                preview.session_id.to_string()
            ],
        )?;
        let ordinal: i64 = transaction.query_row(
            "SELECT coalesce(max(ordinal), 0) + 1 FROM integration_batches WHERE session_id = ?1",
            [preview.session_id.to_string()],
            |row| row.get(0),
        )?;
        let status = if preview.is_approvable() {
            IntegrationBatchStatus::Preview
        } else {
            IntegrationBatchStatus::Blocked
        };
        transaction.execute(
            "INSERT INTO integration_batches(batch_id, session_id, revision_id, ordinal,
                status, base_revision, preview_hash, preview_json, created_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
            params![
                preview.batch_id.to_string(),
                preview.session_id.to_string(),
                preview.graph_revision_id.to_string(),
                ordinal,
                status.as_str(),
                preview.base_revision,
                preview.preview_hash,
                serde_json::to_string(preview)?,
                preview.created_at.to_rfc3339(),
            ],
        )?;
        for (index, source) in preview.sources.iter().enumerate() {
            transaction.execute(
                "INSERT INTO integration_sources(batch_id, source_order, task_id,
                    checkpoint_id, verification_id, diff_sha256, source_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    preview.batch_id.to_string(),
                    i64::try_from(index + 1).unwrap_or(i64::MAX),
                    source.task_id.to_string(),
                    source.checkpoint_id.to_string(),
                    source.verification_id.to_string(),
                    source.diff_sha256,
                    serde_json::to_string(source)?,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(StoredIntegrationBatch {
            preview: preview.clone(),
            status,
            approval: None,
            application: None,
        })
    }

    pub fn current_integration_batch(
        &self,
        session_id: SessionId,
    ) -> StateResult<Option<StoredIntegrationBatch>> {
        self.with_connection(|connection| {
            let row: Option<(String, String)> = connection
                .query_row(
                    "SELECT preview_json, status FROM integration_batches
                     WHERE session_id = ?1 ORDER BY ordinal DESC LIMIT 1",
                    [session_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            row.map(|(preview, status)| {
                let preview: IntegrationPreview = serde_json::from_str(&preview)?;
                load_batch(connection, preview, parse_status(&status)?)
            })
            .transpose()
        })
    }

    pub fn approve_integration(
        &self,
        approval: &IntegrationApproval,
    ) -> StateResult<StoredIntegrationBatch> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let (preview_json, status): (String, String) = transaction.query_row(
            "SELECT preview_json, status FROM integration_batches WHERE batch_id = ?1",
            [approval.batch_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if parse_status(&status)? != IntegrationBatchStatus::Preview {
            return Err(StateError::InvalidRecord(
                "integration batch is not awaiting approval".to_owned(),
            ));
        }
        let preview: IntegrationPreview = serde_json::from_str(&preview_json)?;
        approval
            .validate_for(&preview)
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        transaction.execute(
            "INSERT INTO integration_approvals(batch_id, preview_hash, approved_by, approved_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                approval.batch_id.to_string(),
                approval.preview_hash,
                approval.approved_by.trim(),
                approval.approved_at.to_rfc3339(),
            ],
        )?;
        transaction.execute(
            "UPDATE integration_batches SET status = 'approved'
             WHERE batch_id = ?1 AND status = 'preview'",
            [approval.batch_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(StoredIntegrationBatch {
            preview,
            status: IntegrationBatchStatus::Approved,
            approval: Some(approval.clone()),
            application: None,
        })
    }

    pub fn start_integration_application(
        &self,
        batch_id: IntegrationBatchId,
        application_id: IntegrationApplicationId,
        worktree_path: &str,
        branch: &str,
        started_at: DateTime<Utc>,
    ) -> StateResult<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let preview_hash: String = transaction.query_row(
            "SELECT preview_hash FROM integration_batches WHERE batch_id = ?1 AND status = 'approved'",
            [batch_id.to_string()],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO integration_applications(application_id, batch_id, preview_hash,
                state, worktree_path, branch_name, resulting_tree, detail_redacted,
                started_at, completed_at)
             VALUES (?1, ?2, ?3, 'applying', ?4, ?5, NULL, '', ?6, NULL)",
            params![
                application_id.to_string(),
                batch_id.to_string(),
                preview_hash,
                worktree_path,
                branch,
                started_at.to_rfc3339(),
            ],
        )?;
        transaction.execute(
            "UPDATE integration_batches SET status = 'applying' WHERE batch_id = ?1",
            [batch_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn finish_integration_application(
        &self,
        application: &IntegrationApplication,
    ) -> StateResult<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let state = if application.succeeded {
            "applied"
        } else {
            "failed"
        };
        let changed = transaction.execute(
            "UPDATE integration_applications SET state = ?1, resulting_tree = ?2,
                detail_redacted = ?3, completed_at = ?4
             WHERE application_id = ?5 AND batch_id = ?6 AND state = 'applying'
               AND preview_hash = ?7",
            params![
                state,
                application.resulting_tree,
                application.detail_redacted,
                application.completed_at.to_rfc3339(),
                application.application_id.to_string(),
                application.batch_id.to_string(),
                application.preview_hash,
            ],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("integration application {}", application.application_id),
            });
        }
        transaction.execute(
            "UPDATE integration_batches SET status = ?1, completed_at = ?2 WHERE batch_id = ?3",
            params![
                if application.succeeded {
                    "applied"
                } else {
                    "needs_attention"
                },
                application.completed_at.to_rfc3339(),
                application.batch_id.to_string(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub fn create_integration_resolution_task(
        &self,
        batch_id: IntegrationBatchId,
        created_by: &str,
        now: DateTime<Utc>,
    ) -> StateResult<TaskId> {
        if created_by.trim().is_empty() {
            return Err(StateError::InvalidRecord(
                "integration resolution task creator must be non-empty".to_owned(),
            ));
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(task_id) = transaction
            .query_row(
                "SELECT task_id FROM integration_resolution_tasks WHERE batch_id = ?1",
                [batch_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return TaskId::from_str(&task_id)
                .map_err(|error| StateError::InvalidRecord(error.to_string()));
        }
        let (preview_json, status, session_id, revision_id): (String, String, String, String) =
            transaction.query_row(
                "SELECT batch.preview_json, batch.status, batch.session_id, batch.revision_id
                 FROM integration_batches batch
                 WHERE batch.batch_id = ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM integration_batches newer
                       WHERE newer.session_id = batch.session_id
                         AND newer.ordinal > batch.ordinal
                   )",
                [batch_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        let status = parse_status(&status)?;
        if !matches!(
            status,
            IntegrationBatchStatus::Blocked | IntegrationBatchStatus::NeedsAttention
        ) {
            return Err(StateError::InvalidRecord(
                "integration resolution task requires the current blocked preview".to_owned(),
            ));
        }
        let preview: IntegrationPreview = serde_json::from_str(&preview_json)?;
        let resolution_available = status == IntegrationBatchStatus::NeedsAttention
            || preview.blockers.iter().any(|blocker| {
                matches!(
                    blocker,
                    IntegrationBlocker::PathOverlap { .. } | IntegrationBlocker::PatchFailed { .. }
                )
            });
        if !resolution_available {
            return Err(StateError::InvalidRecord(
                "integration blocker requires source remediation, not a resolution task".to_owned(),
            ));
        }
        let session_id = SessionId::from_str(&session_id)
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        let dependency_ids = integration_dependency_ids(&preview);
        if dependency_ids.is_empty() {
            return Err(StateError::InvalidRecord(
                "integration blockers do not identify a source task".to_owned(),
            ));
        }
        let (provider, profile): (String, String) = transaction.query_row(
            "SELECT coalesce(provider_id_v2, provider_id), model_profile FROM session_tasks
             WHERE session_id = ?1 AND revision_id = ?2 ORDER BY display_order LIMIT 1",
            params![session_id.to_string(), revision_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let display_order: i64 = transaction.query_row(
            "SELECT coalesce(max(display_order), 0) + 1 FROM session_tasks
             WHERE session_id = ?1 AND revision_id = ?2",
            params![session_id.to_string(), revision_id],
            |row| row.get(0),
        )?;
        let mut write_paths = Vec::new();
        for source in &preview.sources {
            for path in &source.changed_files {
                if !write_paths.contains(path) {
                    write_paths.push(path.clone());
                }
            }
        }
        let blocker_summary = if preview.blockers.is_empty() {
            "the previous integration application was interrupted or failed".to_owned()
        } else {
            preview
                .blockers
                .iter()
                .map(|blocker| {
                    serde_json::to_string(blocker).unwrap_or_else(|_| "blocker".to_owned())
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        let objective = format!(
            "Resolve integration batch {batch_id} conflicts and produce one verified, consolidated result. Blockers: {blocker_summary}"
        );
        let task_id = TaskId::new();
        let envelope = TaskEnvelope {
            schema_version: SchemaVersion::v1(),
            task_id,
            objective: objective.clone(),
            original_request_redacted: objective,
            constraints: vec![
                "Treat source task worktrees and sealed checkpoints as read-only evidence"
                    .to_owned(),
                "Resolve every recorded integration blocker in this task worktree".to_owned(),
            ],
            acceptance_criteria: vec![
                "All intended source changes are represented once without conflict markers"
                    .to_owned(),
                "Verification passes and a new sealed checkpoint is produced".to_owned(),
            ],
            repository_wide_write_scope: write_paths.is_empty(),
            allowed_write_paths: write_paths,
            assessment: None,
            created_at: now,
        };
        let timestamp = now.to_rfc3339();
        transaction.execute(
            "INSERT INTO tasks(task_id, schema_version, revision, state, resume_state, paused,
                objective, original_request_redacted, task_envelope_json, created_at, updated_at,
                archived_at)
             VALUES (?1, ?2, 0, 'queued', NULL, 0, ?3, ?4, ?5, ?6, ?6, NULL)",
            params![
                task_id.to_string(),
                envelope.schema_version.as_str(),
                envelope.objective,
                envelope.original_request_redacted,
                serde_json::to_string(&envelope)?,
                timestamp,
            ],
        )?;
        transaction.execute(
            "INSERT INTO session_tasks(session_id, revision_id, task_id, node_key,
                display_order, provider_id, provider_id_v2, model_profile)
             VALUES (?1, ?2, ?3, ?4, ?5,
                     CASE WHEN ?6 = 'agy' THEN 'codex' ELSE ?6 END, ?6, ?7)",
            params![
                session_id.to_string(),
                revision_id,
                task_id.to_string(),
                format!("integration-resolution-{batch_id}"),
                display_order,
                provider,
                profile,
            ],
        )?;
        for dependency_id in dependency_ids {
            transaction.execute(
                "INSERT INTO task_dependencies(session_id, revision_id, task_id,
                    depends_on_task_id) VALUES (?1, ?2, ?3, ?4)",
                params![
                    session_id.to_string(),
                    revision_id,
                    task_id.to_string(),
                    dependency_id.to_string(),
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO integration_resolution_tasks(batch_id, task_id, created_by, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                batch_id.to_string(),
                task_id.to_string(),
                created_by.trim(),
                timestamp,
            ],
        )?;
        let mut event = TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            session_id: Some(session_id),
            task_id: Some(task_id),
            occurred_at: now,
            event_type: EventType::TaskCreated,
            from_state: None,
            to_state: Some(TaskState::Queued),
            reason: Some("integration conflict resolution task generated".to_owned()),
            actor: EventActor::Orchestrator,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: serde_json::json!({"batch_id": batch_id}),
            previous_hash: None,
            event_hash: String::new(),
        };
        append_event_in_transaction(&transaction, &mut event)?;
        transaction.commit()?;
        Ok(task_id)
    }

    pub fn latest_completed_resolution_task(
        &self,
        session_id: SessionId,
    ) -> StateResult<Option<TaskId>> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT resolution.task_id
                     FROM integration_resolution_tasks resolution
                     JOIN integration_batches batch ON batch.batch_id = resolution.batch_id
                     JOIN tasks task ON task.task_id = resolution.task_id
                     JOIN session_graph_heads head ON head.session_id = batch.session_id
                     JOIN session_tasks graph_task ON graph_task.task_id = resolution.task_id
                                                  AND graph_task.revision_id = head.revision_id
                     WHERE batch.session_id = ?1 AND task.state = 'completed'
                     ORDER BY batch.ordinal DESC LIMIT 1",
                    [session_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .map(|value| {
                    TaskId::from_str(&value)
                        .map_err(|error| StateError::InvalidRecord(error.to_string()))
                })
                .transpose()
        })
    }

    pub fn reconcile_interrupted_integrations(&self, now: DateTime<Utc>) -> StateResult<usize> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let batches = {
            let mut statement = transaction.prepare(
                "SELECT batch.batch_id, batch.session_id, batch.status,
                        coalesce(session.state_v2, session.state)
                 FROM integration_batches batch
                 JOIN sessions session ON session.session_id = batch.session_id
                 WHERE batch.status IN ('approved', 'applying', 'applied')
                 ORDER BY batch.created_at, batch.batch_id",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut reconciled = 0;
        for (batch_id, session_id, status, session_state) in batches {
            let session_id = SessionId::from_str(&session_id)
                .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
            let session_state: SessionState =
                serde_json::from_value(serde_json::Value::String(session_state))?;
            match status.as_str() {
                "approved" | "applying" => {
                    if status == "applying" {
                        transaction.execute(
                            "UPDATE integration_applications
                             SET state = 'interrupted',
                                 detail_redacted = 'daemon stopped during integration application',
                                 completed_at = ?1
                             WHERE batch_id = ?2 AND state = 'applying'",
                            params![now.to_rfc3339(), batch_id],
                        )?;
                    }
                    transaction.execute(
                        "UPDATE integration_batches SET status = 'needs_attention', completed_at = ?1
                         WHERE batch_id = ?2 AND status IN ('approved', 'applying')",
                        params![now.to_rfc3339(), batch_id],
                    )?;
                    if session_state != SessionState::NeedsAttention {
                        transition_session_in_transaction(
                            &transaction,
                            session_id,
                            session_state,
                            SessionState::NeedsAttention,
                            now,
                            &batch_id,
                        )?;
                    }
                    reconciled += 1;
                }
                "applied" => {
                    let mut current = session_state;
                    if current == SessionState::Integrating {
                        transition_session_in_transaction(
                            &transaction,
                            session_id,
                            current,
                            SessionState::Verifying,
                            now,
                            &batch_id,
                        )?;
                        current = SessionState::Verifying;
                    }
                    if current == SessionState::Verifying {
                        transition_session_in_transaction(
                            &transaction,
                            session_id,
                            current,
                            SessionState::Completed,
                            now,
                            &batch_id,
                        )?;
                        reconciled += 1;
                    }
                }
                _ => {}
            }
        }
        transaction.commit()?;
        Ok(reconciled)
    }
}

fn transition_session_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    session_id: SessionId,
    from: SessionState,
    to: SessionState,
    now: DateTime<Utc>,
    batch_id: &str,
) -> StateResult<()> {
    from.validate_transition(to)
        .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
    let to_text = serde_json::to_value(to)?
        .as_str()
        .ok_or_else(|| StateError::InvalidRecord("invalid session state".to_owned()))?
        .to_owned();
    let legacy_to_text = if to == SessionState::Validating {
        "planning".to_owned()
    } else {
        to_text.clone()
    };
    let from_text = serde_json::to_value(from)?
        .as_str()
        .ok_or_else(|| StateError::InvalidRecord("invalid session state".to_owned()))?
        .to_owned();
    let changed = transaction.execute(
        "UPDATE sessions SET state = ?1, state_v2 = ?2,
         revision = revision + 1, updated_at = ?3
         WHERE session_id = ?4 AND coalesce(state_v2, state) = ?5",
        params![
            legacy_to_text,
            to_text,
            now.to_rfc3339(),
            session_id.to_string(),
            from_text,
        ],
    )?;
    if changed != 1 {
        return Err(StateError::OptimisticConflict {
            entity: format!("integration session {session_id}"),
        });
    }
    let mut event = TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id: Some(session_id),
        task_id: None,
        occurred_at: now,
        event_type: EventType::SessionStateTransitioned,
        from_state: None,
        to_state: None,
        reason: Some("interrupted integration reconciliation".to_owned()),
        actor: EventActor::Orchestrator,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload: serde_json::json!({"batch_id": batch_id, "from": from, "to": to}),
        previous_hash: None,
        event_hash: String::new(),
    };
    append_event_in_transaction(transaction, &mut event)?;
    Ok(())
}

fn integration_dependency_ids(preview: &IntegrationPreview) -> Vec<TaskId> {
    let mut task_ids = Vec::new();
    let mut push = |task_id| {
        if !task_ids.contains(&task_id) {
            task_ids.push(task_id);
        }
    };
    for source in &preview.sources {
        push(source.task_id);
    }
    for blocker in &preview.blockers {
        match blocker {
            IntegrationBlocker::MissingEvidence { task_id, .. }
            | IntegrationBlocker::VerificationFailed { task_id }
            | IntegrationBlocker::StaleBase { task_id, .. }
            | IntegrationBlocker::SourceChanged { task_id }
            | IntegrationBlocker::PatchFailed { task_id, .. } => push(*task_id),
            IntegrationBlocker::PathOverlap { left, right, .. } => {
                push(*left);
                push(*right);
            }
        }
    }
    task_ids
}

fn load_batch(
    connection: &rusqlite::Connection,
    preview: IntegrationPreview,
    status: IntegrationBatchStatus,
) -> StateResult<StoredIntegrationBatch> {
    if !preview.verify_integrity() {
        return Err(StateError::InvalidRecord(
            "stored integration preview integrity check failed".to_owned(),
        ));
    }
    let approval = connection
        .query_row(
            "SELECT preview_hash, approved_by, approved_at FROM integration_approvals
             WHERE batch_id = ?1",
            [preview.batch_id.to_string()],
            |row| {
                let approved_at: String = row.get(2)?;
                Ok(IntegrationApproval {
                    batch_id: preview.batch_id,
                    preview_hash: row.get(0)?,
                    approved_by: row.get(1)?,
                    approved_at: parse_time(&approved_at, 2)?,
                })
            },
        )
        .optional()?;
    let application = connection
        .query_row(
            "SELECT application_id, preview_hash, worktree_path, branch_name, resulting_tree,
                    state, detail_redacted, completed_at FROM integration_applications
             WHERE batch_id = ?1",
            [preview.batch_id.to_string()],
            |row| {
                let id: String = row.get(0)?;
                let state: String = row.get(5)?;
                let completed: Option<String> = row.get(7)?;
                Ok(IntegrationApplication {
                    application_id: IntegrationApplicationId::from_str(&id)
                        .map_err(|error| conversion(0, error))?,
                    batch_id: preview.batch_id,
                    preview_hash: row.get(1)?,
                    integration_worktree: row.get(2)?,
                    integration_branch: row.get(3)?,
                    resulting_tree: row.get(4)?,
                    succeeded: state == "applied",
                    detail_redacted: row.get(6)?,
                    completed_at: completed
                        .as_deref()
                        .map(|value| parse_time(value, 7))
                        .transpose()?
                        .unwrap_or(preview.created_at),
                })
            },
        )
        .optional()?;
    Ok(StoredIntegrationBatch {
        preview,
        status,
        approval,
        application,
    })
}

fn parse_status(value: &str) -> StateResult<IntegrationBatchStatus> {
    match value {
        "preview" => Ok(IntegrationBatchStatus::Preview),
        "blocked" => Ok(IntegrationBatchStatus::Blocked),
        "approved" => Ok(IntegrationBatchStatus::Approved),
        "applying" => Ok(IntegrationBatchStatus::Applying),
        "applied" => Ok(IntegrationBatchStatus::Applied),
        "needs_attention" => Ok(IntegrationBatchStatus::NeedsAttention),
        "superseded" => Ok(IntegrationBatchStatus::Superseded),
        _ => Err(StateError::InvalidRecord(format!(
            "unknown integration batch status `{value}`"
        ))),
    }
}

fn parse_time(value: &str, column: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| conversion(column, error))
}

fn conversion(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use orchestrator_domain::{
        GraphRevisionId, IntegrationApplicationId, IntegrationBatchId, IntegrationBlocker,
        IntegrationPreview, MessageId, RepoPath, SchemaVersion, SessionId, TaskEnvelope, TaskId,
    };
    use rusqlite::params;

    use crate::Database;

    #[test]
    #[allow(clippy::too_many_lines)]
    fn blocked_preview_materializes_one_audited_resolution_task()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        let now = Utc::now();
        let session_id = SessionId::new();
        let message_id = MessageId::new();
        let revision_id = GraphRevisionId::new();
        let source_task_id = TaskId::new();
        let second_source_task_id = TaskId::new();
        let mut source = TaskEnvelope::new("source", "source", now);
        source.task_id = source_task_id;
        let mut second_source = TaskEnvelope::new("second source", "second source", now);
        second_source.task_id = second_source_task_id;
        database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO sessions(session_id, schema_version, revision, title, state,
                    created_at, updated_at, archived_at)
                 VALUES (?1, ?2, 0, 'integration', 'running', ?3, ?3, NULL)",
                params![
                    session_id.to_string(),
                    SchemaVersion::v1().as_str(),
                    now.to_rfc3339()
                ],
            )?;
            connection.execute(
                "INSERT INTO conversation_messages(message_id, session_id, task_id, ordinal,
                    role, kind, state, content_redacted, created_at, finalized_at)
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
            for (index, envelope) in [source.clone(), second_source.clone()].iter().enumerate() {
                connection.execute(
                    "INSERT INTO tasks(task_id, schema_version, revision, state, resume_state,
                        paused, objective, original_request_redacted, task_envelope_json,
                        created_at, updated_at, archived_at)
                     VALUES (?1, ?2, 0, 'completed', NULL, 0, ?3, ?4, ?5, ?6, ?6, NULL)",
                    params![
                        envelope.task_id.to_string(),
                        SchemaVersion::v1().as_str(),
                        envelope.objective,
                        envelope.original_request_redacted,
                        serde_json::to_string(envelope)?,
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
                        envelope.task_id.to_string(),
                        format!("source-{index}"),
                        i64::try_from(index + 1).unwrap_or(i64::MAX)
                    ],
                )?;
            }
            Ok(())
        })?;
        let batch_id = IntegrationBatchId::new();
        let preview = IntegrationPreview::seal(
            batch_id,
            session_id,
            revision_id,
            "a".repeat(40),
            Vec::new(),
            vec![IntegrationBlocker::PathOverlap {
                left: source_task_id,
                right: second_source_task_id,
                path: RepoPath::try_from("src/lib.rs")?,
            }],
            now,
        )?;
        database.record_integration_preview(&preview)?;

        let resolution = database.create_integration_resolution_task(batch_id, "operator", now)?;
        assert_eq!(
            database.create_integration_resolution_task(batch_id, "operator", now)?,
            resolution
        );
        let envelope = database
            .load_task_envelope(resolution)?
            .ok_or("resolution envelope missing")?;
        assert!(envelope.objective.contains(&batch_id.to_string()));
        assert!(envelope.repository_wide_write_scope);
        database.with_connection(|connection| {
            let dependencies: i64 = connection.query_row(
                "SELECT count(*) FROM task_dependencies WHERE task_id = ?1",
                [resolution.to_string()],
                |row| row.get(0),
            )?;
            assert_eq!(dependencies, 2);
            connection.execute(
                "UPDATE tasks SET state = 'completed' WHERE task_id = ?1",
                [resolution.to_string()],
            )?;
            Ok(())
        })?;
        assert_eq!(
            database.latest_completed_resolution_task(session_id)?,
            Some(resolution)
        );
        let application_id = IntegrationApplicationId::new();
        database.with_connection(|connection| {
            connection.execute(
                "UPDATE sessions SET state = 'integrating' WHERE session_id = ?1",
                [session_id.to_string()],
            )?;
            connection.execute(
                "UPDATE integration_batches SET status = 'applying' WHERE batch_id = ?1",
                [batch_id.to_string()],
            )?;
            connection.execute(
                "INSERT INTO integration_applications(application_id, batch_id, preview_hash,
                    state, worktree_path, branch_name, resulting_tree, detail_redacted,
                    started_at, completed_at)
                 VALUES (?1, ?2, ?3, 'applying', '.colay/integration/test',
                         'orchestrator/integration-test', NULL, '', ?4, NULL)",
                params![
                    application_id.to_string(),
                    batch_id.to_string(),
                    preview.preview_hash,
                    now.to_rfc3339()
                ],
            )?;
            Ok(())
        })?;
        assert_eq!(database.reconcile_interrupted_integrations(now)?, 1);
        let recovered = database
            .current_integration_batch(session_id)?
            .ok_or("recovered batch missing")?;
        assert_eq!(
            recovered.status,
            super::IntegrationBatchStatus::NeedsAttention
        );
        assert!(
            recovered
                .application
                .is_some_and(|application| application.detail_redacted.contains("daemon stopped"))
        );
        assert_eq!(
            database
                .load_session(session_id)?
                .ok_or("session missing")?
                .state,
            orchestrator_domain::SessionState::NeedsAttention
        );
        Ok(())
    }
}
