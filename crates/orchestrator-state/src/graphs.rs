use std::{collections::BTreeMap, str::FromStr};

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    ConversationMessage, CorrelationId, EventActor, EventId, EventType, GraphRevisionId,
    GraphValidationSummary, MessageId, MessageKind, MessageRole, MessageState, ModelProfile,
    PlanningAttemptId, ProviderId, SchemaVersion, SessionId, TaskEnvelope, TaskEvent,
    TaskGraphProposal, TaskId, TaskState, task_graph_proposal_hash,
};
use rusqlite::{OptionalExtension as _, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{Database, StateError, StateResult, database::append_event_in_transaction};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphRevisionStatus {
    Planning,
    Invalid,
    AwaitingApproval,
    Approved,
    Superseded,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPlanningAttempt {
    pub attempt_id: PlanningAttemptId,
    pub revision_id: GraphRevisionId,
    pub session_id: SessionId,
    pub goal_message_id: MessageId,
    pub planner_provider: ProviderId,
    pub started_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewGraphAttempt {
    pub attempt_id: PlanningAttemptId,
    pub revision_id: GraphRevisionId,
    pub session_id: SessionId,
    pub goal_message_id: MessageId,
    pub planner_provider: ProviderId,
    pub status: GraphRevisionStatus,
    pub proposal: Option<TaskGraphProposal>,
    pub proposal_hash: Option<String>,
    pub validation: serde_json::Value,
    pub error_redacted: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

impl NewGraphAttempt {
    #[must_use]
    pub fn from_validated(
        attempt_id: PlanningAttemptId,
        graph: orchestrator_domain::ValidatedTaskGraph,
        started_at: DateTime<Utc>,
        completed_at: DateTime<Utc>,
    ) -> Self {
        let validation = serde_json::to_value(&graph.validation).unwrap_or_else(
            |_| serde_json::json!({"error": "validation summary serialization failed"}),
        );
        Self {
            attempt_id,
            revision_id: graph.proposal.revision_id,
            session_id: graph.proposal.session_id,
            goal_message_id: graph.proposal.goal_message_id,
            planner_provider: graph.proposal.planner_provider,
            status: GraphRevisionStatus::AwaitingApproval,
            proposal: Some(graph.proposal),
            proposal_hash: Some(graph.proposal_hash),
            validation,
            error_redacted: None,
            started_at,
            completed_at,
        }
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn invalid(
        attempt_id: PlanningAttemptId,
        revision_id: GraphRevisionId,
        session_id: SessionId,
        goal_message_id: MessageId,
        planner_provider: ProviderId,
        validation: serde_json::Value,
        error_redacted: impl Into<String>,
        started_at: DateTime<Utc>,
        completed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            attempt_id,
            revision_id,
            session_id,
            goal_message_id,
            planner_provider,
            status: GraphRevisionStatus::Invalid,
            proposal: None,
            proposal_hash: None,
            validation,
            error_redacted: Some(error_redacted.into()),
            started_at,
            completed_at,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGraphRevision {
    pub revision_id: GraphRevisionId,
    pub session_id: SessionId,
    pub goal_message_id: MessageId,
    pub ordinal: u64,
    pub status: GraphRevisionStatus,
    pub proposal_hash: Option<String>,
    pub proposal: Option<TaskGraphProposal>,
    pub validation: serde_json::Value,
    pub planner_provider: Option<ProviderId>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphTaskProjection {
    pub task_id: TaskId,
    pub node_key: String,
    pub display_order: u64,
    pub provider: ProviderId,
    pub profile: ModelProfile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphTaskDependency {
    pub task_id: TaskId,
    pub depends_on_task_id: TaskId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphProjection {
    pub revision: StoredGraphRevision,
    pub tasks: Vec<GraphTaskProjection>,
    pub dependencies: Vec<GraphTaskDependency>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphApprovalRequest {
    pub revision_id: GraphRevisionId,
    pub expected_proposal_hash: String,
    pub authority: Option<orchestrator_domain::GraphValidationAuthority>,
    pub approved_by: String,
    pub approved_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovedGraph {
    pub revision_id: GraphRevisionId,
    pub proposal_hash: String,
    pub task_ids: Vec<TaskId>,
    pub replayed: bool,
}

impl Database {
    pub fn begin_graph_attempt(
        &self,
        attempt: &NewPlanningAttempt,
    ) -> StateResult<StoredGraphRevision> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_goal_identity(&transaction, attempt.session_id, attempt.goal_message_id)?;
        if let Some(existing) = graph_revision_by_id(&transaction, attempt.revision_id)? {
            let identity_matches = existing.session_id == attempt.session_id
                && existing.goal_message_id == attempt.goal_message_id
                && existing.planner_provider == Some(attempt.planner_provider)
                && existing.created_at == attempt.started_at;
            if !identity_matches {
                return Err(StateError::InvalidRecord(
                    "planning attempt replay conflicts with its revision".to_owned(),
                ));
            }
            transaction.commit()?;
            return Ok(existing);
        }
        let ordinal: i64 = transaction.query_row(
            "SELECT coalesce(max(ordinal), 0) + 1 FROM graph_revisions WHERE session_id = ?1",
            [attempt.session_id.to_string()],
            |row| row.get(0),
        )?;
        transaction.execute(
            "UPDATE graph_revisions SET status = 'superseded', completed_at = coalesce(completed_at, ?1)
             WHERE revision_id = (SELECT revision_id FROM session_graph_heads WHERE session_id = ?2)
             AND status IN ('planning', 'awaiting_approval', 'approved')",
            params![attempt.started_at.to_rfc3339(), attempt.session_id.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO graph_revisions(
                revision_id, session_id, goal_message_id, ordinal, status, proposal_hash,
                proposal_json, validation_json, planner_provider, planner_provider_v2,
                created_at, completed_at
             ) VALUES (?1, ?2, ?3, ?4, 'planning', NULL, NULL, '{}', ?5, ?6, ?7, NULL)",
            params![
                attempt.revision_id.to_string(),
                attempt.session_id.to_string(),
                attempt.goal_message_id.to_string(),
                ordinal,
                legacy_provider_text(attempt.planner_provider),
                attempt.planner_provider.as_str(),
                attempt.started_at.to_rfc3339(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO planning_attempts(
                attempt_id, revision_id, session_id, goal_message_id, planner_provider,
                planner_provider_v2,
                outcome, error_redacted, started_at, completed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'planning', NULL, ?7, NULL)",
            params![
                attempt.attempt_id.to_string(),
                attempt.revision_id.to_string(),
                attempt.session_id.to_string(),
                attempt.goal_message_id.to_string(),
                legacy_provider_text(attempt.planner_provider),
                attempt.planner_provider.as_str(),
                attempt.started_at.to_rfc3339(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO session_graph_heads(session_id, revision_id, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET revision_id = excluded.revision_id,
                 updated_at = excluded.updated_at",
            params![
                attempt.session_id.to_string(),
                attempt.revision_id.to_string(),
                attempt.started_at.to_rfc3339(),
            ],
        )?;
        let mut event = graph_event(
            attempt.session_id,
            EventType::GraphRevisionRecorded,
            EventActor::Provider(attempt.planner_provider),
            attempt.started_at,
            serde_json::json!({"revision_id": attempt.revision_id, "status": "planning"}),
        );
        append_event_in_transaction(&transaction, &mut event)?;
        let stored = graph_revision_by_id(&transaction, attempt.revision_id)?.ok_or_else(|| {
            StateError::InvalidRecord("started graph revision is missing".to_owned())
        })?;
        transaction.commit()?;
        Ok(stored)
    }

    pub fn finish_graph_attempt(
        &self,
        attempt: &NewGraphAttempt,
    ) -> StateResult<StoredGraphRevision> {
        validate_new_attempt(attempt)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing =
            graph_revision_by_id(&transaction, attempt.revision_id)?.ok_or_else(|| {
                StateError::InvalidRecord("planning graph revision does not exist".to_owned())
            })?;
        if existing.status != GraphRevisionStatus::Planning
            || existing.session_id != attempt.session_id
            || existing.goal_message_id != attempt.goal_message_id
            || existing.planner_provider != Some(attempt.planner_provider)
        {
            return Err(StateError::InvalidRecord(
                "planning graph completion conflicts with its started attempt".to_owned(),
            ));
        }
        let authority = authority_from_validation(&attempt.validation);
        let changed = transaction.execute(
            "UPDATE graph_revisions SET status = ?1, proposal_hash = ?2, proposal_json = ?3,
                    validation_json = ?4, completed_at = ?5, requirement_revision_id = ?6,
                    validation_hash = ?7, base_commit = ?8
             WHERE revision_id = ?9 AND status = 'planning'",
            params![
                enum_text(&attempt.status)?,
                attempt.proposal_hash,
                attempt
                    .proposal
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                serde_json::to_string(&attempt.validation)?,
                attempt.completed_at.to_rfc3339(),
                authority
                    .as_ref()
                    .map(|value| value.requirement_revision_id.to_string()),
                authority.as_ref().map(|value| &value.validation_hash),
                authority.as_ref().map(|value| &value.base_commit),
                attempt.revision_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("graph revision {}", attempt.revision_id),
            });
        }
        let changed = transaction.execute(
            "UPDATE planning_attempts SET outcome = ?1, error_redacted = ?2, completed_at = ?3
             WHERE attempt_id = ?4 AND revision_id = ?5 AND outcome = 'planning'",
            params![
                enum_text(&attempt.status)?,
                attempt.error_redacted,
                attempt.completed_at.to_rfc3339(),
                attempt.attempt_id.to_string(),
                attempt.revision_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("planning attempt {}", attempt.attempt_id),
            });
        }
        let mut event = graph_event(
            attempt.session_id,
            EventType::GraphRevisionRecorded,
            EventActor::Provider(attempt.planner_provider),
            attempt.completed_at,
            serde_json::json!({
                "revision_id": attempt.revision_id,
                "status": attempt.status,
                "proposal_hash": attempt.proposal_hash,
            }),
        );
        append_event_in_transaction(&transaction, &mut event)?;
        let stored = graph_revision_by_id(&transaction, attempt.revision_id)?.ok_or_else(|| {
            StateError::InvalidRecord("finished graph revision is missing".to_owned())
        })?;
        transaction.commit()?;
        Ok(stored)
    }

    pub fn record_graph_attempt(
        &self,
        attempt: &NewGraphAttempt,
    ) -> StateResult<StoredGraphRevision> {
        validate_new_attempt(attempt)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_goal_identity(&transaction, attempt.session_id, attempt.goal_message_id)?;
        let ordinal: i64 = transaction.query_row(
            "SELECT coalesce(max(ordinal), 0) + 1 FROM graph_revisions WHERE session_id = ?1",
            [attempt.session_id.to_string()],
            |row| row.get(0),
        )?;
        transaction.execute(
            "UPDATE graph_revisions SET status = 'superseded', completed_at = coalesce(completed_at, ?1)
             WHERE revision_id = (SELECT revision_id FROM session_graph_heads WHERE session_id = ?2)
             AND status IN ('planning', 'awaiting_approval', 'approved')",
            params![attempt.completed_at.to_rfc3339(), attempt.session_id.to_string()],
        )?;
        let authority = authority_from_validation(&attempt.validation);
        transaction.execute(
            "INSERT INTO graph_revisions(
                revision_id, session_id, goal_message_id, ordinal, status, proposal_hash,
                proposal_json, validation_json, planner_provider, planner_provider_v2,
                created_at, completed_at,
                requirement_revision_id, validation_hash, base_commit
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                attempt.revision_id.to_string(),
                attempt.session_id.to_string(),
                attempt.goal_message_id.to_string(),
                ordinal,
                enum_text(&attempt.status)?,
                attempt.proposal_hash,
                attempt
                    .proposal
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                serde_json::to_string(&attempt.validation)?,
                legacy_provider_text(attempt.planner_provider),
                attempt.planner_provider.as_str(),
                attempt.started_at.to_rfc3339(),
                attempt.completed_at.to_rfc3339(),
                authority
                    .as_ref()
                    .map(|value| value.requirement_revision_id.to_string()),
                authority.as_ref().map(|value| &value.validation_hash),
                authority.as_ref().map(|value| &value.base_commit),
            ],
        )?;
        transaction.execute(
            "INSERT INTO planning_attempts(
                attempt_id, revision_id, session_id, goal_message_id, planner_provider,
                planner_provider_v2,
                outcome, error_redacted, started_at, completed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                attempt.attempt_id.to_string(),
                attempt.revision_id.to_string(),
                attempt.session_id.to_string(),
                attempt.goal_message_id.to_string(),
                legacy_provider_text(attempt.planner_provider),
                attempt.planner_provider.as_str(),
                enum_text(&attempt.status)?,
                attempt.error_redacted,
                attempt.started_at.to_rfc3339(),
                attempt.completed_at.to_rfc3339(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO session_graph_heads(session_id, revision_id, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET revision_id = excluded.revision_id,
                 updated_at = excluded.updated_at",
            params![
                attempt.session_id.to_string(),
                attempt.revision_id.to_string(),
                attempt.completed_at.to_rfc3339(),
            ],
        )?;
        let mut event = graph_event(
            attempt.session_id,
            EventType::GraphRevisionRecorded,
            EventActor::Provider(attempt.planner_provider),
            attempt.completed_at,
            serde_json::json!({
                "revision_id": attempt.revision_id,
                "status": attempt.status,
                "proposal_hash": attempt.proposal_hash,
            }),
        );
        append_event_in_transaction(&transaction, &mut event)?;
        let stored = graph_revision_by_id(&transaction, attempt.revision_id)?.ok_or_else(|| {
            StateError::InvalidRecord("recorded graph revision is missing".to_owned())
        })?;
        transaction.commit()?;
        Ok(stored)
    }

    pub fn load_graph_revision(
        &self,
        revision_id: GraphRevisionId,
    ) -> StateResult<Option<StoredGraphRevision>> {
        self.with_connection(|connection| graph_revision_by_id(connection, revision_id))
    }

    pub fn current_graph(&self, session_id: SessionId) -> StateResult<Option<GraphProjection>> {
        self.with_connection(|connection| {
            let revision_id: Option<String> = connection
                .query_row(
                    "SELECT revision_id FROM session_graph_heads WHERE session_id = ?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            revision_id
                .map(|value| {
                    let revision_id = parse_id::<GraphRevisionId>("revision_id", &value)?;
                    graph_projection(connection, revision_id)
                })
                .transpose()
        })
    }

    #[allow(clippy::too_many_lines)]
    pub fn approve_graph_and_materialize_tasks(
        &self,
        request: &GraphApprovalRequest,
    ) -> StateResult<ApprovedGraph> {
        if request.approved_by.trim().is_empty() {
            return Err(StateError::InvalidRecord(
                "graph approver must be non-empty".to_owned(),
            ));
        }
        if request.expected_proposal_hash.len() != 64 {
            return Err(StateError::InvalidRecord(
                "expected graph proposal hash must contain 64 hexadecimal characters".to_owned(),
            ));
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision =
            graph_revision_by_id(&transaction, request.revision_id)?.ok_or_else(|| {
                StateError::InvalidRecord(format!(
                    "graph revision {} does not exist",
                    request.revision_id
                ))
            })?;
        let stored_authority = authority_from_validation(&revision.validation);
        if stored_authority != request.authority {
            return Err(StateError::InvalidRecord(
                "graph approval authority does not match the sealed graph validation".to_owned(),
            ));
        }

        if let Some((
            stored_hash,
            approved_by,
            session_id,
            requirement_revision_id,
            validation_hash,
            base_commit,
        )) = transaction
            .query_row(
                "SELECT proposal_hash, approved_by, session_id, requirement_revision_id,
                        validation_hash, base_commit
                 FROM graph_approvals WHERE revision_id = ?1",
                [request.revision_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .optional()?
        {
            if stored_hash != request.expected_proposal_hash
                || approved_by != request.approved_by.trim()
                || session_id.as_deref() != Some(revision.session_id.to_string().as_str())
                || request.authority.as_ref().is_some_and(|authority| {
                    requirement_revision_id.as_deref()
                        != Some(authority.requirement_revision_id.to_string().as_str())
                        || validation_hash.as_deref() != Some(authority.validation_hash.as_str())
                        || base_commit.as_deref() != Some(authority.base_commit.as_str())
                })
            {
                return Err(StateError::InvalidRecord(
                    "graph approval replay does not match the stored approval".to_owned(),
                ));
            }
            let task_ids = task_ids_for_revision(&transaction, request.revision_id)?;
            transaction.commit()?;
            return Ok(ApprovedGraph {
                revision_id: request.revision_id,
                proposal_hash: stored_hash,
                task_ids,
                replayed: true,
            });
        }

        let head: Option<String> = transaction
            .query_row(
                "SELECT revision_id FROM session_graph_heads WHERE session_id = ?1",
                [revision.session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if head.as_deref() != Some(&request.revision_id.to_string()) {
            return Err(StateError::InvalidRecord(
                "only the current graph revision may be approved".to_owned(),
            ));
        }
        if revision.status != GraphRevisionStatus::AwaitingApproval {
            return Err(StateError::InvalidRecord(format!(
                "graph revision is not awaiting approval: {:?}",
                revision.status
            )));
        }
        let latest_user_message: Option<String> = transaction
            .query_row(
                "SELECT message_id FROM conversation_messages
                 WHERE session_id = ?1 AND task_id IS NULL AND role = 'user' AND state = 'final'
                 ORDER BY ordinal DESC LIMIT 1",
                [revision.session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if latest_user_message.as_deref() != Some(&revision.goal_message_id.to_string()) {
            return Err(StateError::InvalidRecord(
                "graph approval is stale because a newer user message exists".to_owned(),
            ));
        }
        let stored_hash = revision.proposal_hash.as_deref().ok_or_else(|| {
            StateError::InvalidRecord("approvable graph has no proposal hash".to_owned())
        })?;
        if stored_hash != request.expected_proposal_hash {
            return Err(StateError::InvalidRecord(
                "graph proposal hash does not match approval".to_owned(),
            ));
        }
        let proposal = revision.proposal.as_ref().ok_or_else(|| {
            StateError::InvalidRecord("approvable graph has no proposal".to_owned())
        })?;
        let validation: GraphValidationSummary =
            serde_json::from_value(revision.validation.clone())?;
        let recomputed = task_graph_proposal_hash(proposal, &validation)
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        if recomputed != request.expected_proposal_hash {
            return Err(StateError::InvalidRecord(
                "persisted graph seal does not match approval".to_owned(),
            ));
        }
        if proposal.revision_id != revision.revision_id
            || proposal.session_id != revision.session_id
            || proposal.goal_message_id != revision.goal_message_id
        {
            return Err(StateError::InvalidRecord(
                "persisted graph proposal identity does not match its revision".to_owned(),
            ));
        }

        let original_request_redacted: String = transaction.query_row(
            "SELECT content_redacted FROM conversation_messages
             WHERE message_id = ?1 AND session_id = ?2",
            params![
                proposal.goal_message_id.to_string(),
                proposal.session_id.to_string()
            ],
            |row| row.get(0),
        )?;
        let mut task_ids = Vec::with_capacity(proposal.nodes.len());
        let mut key_to_id = BTreeMap::new();
        for (index, node) in proposal.nodes.iter().enumerate() {
            let task_id = TaskId::new();
            let envelope = TaskEnvelope {
                schema_version: SchemaVersion::v1(),
                task_id,
                objective: node.objective.clone(),
                original_request_redacted: original_request_redacted.clone(),
                constraints: node.constraints.clone(),
                acceptance_criteria: node.acceptance_criteria.clone(),
                allowed_write_paths: node.write_scopes.clone(),
                repository_wide_write_scope: node.repository_wide_write_scope,
                assessment: None,
                created_at: request.approved_at,
            };
            insert_task(&transaction, &envelope)?;
            let provider = node.provider.unwrap_or(proposal.planner_provider);
            transaction.execute(
                "INSERT INTO session_tasks(
                    session_id, revision_id, task_id, node_key, display_order,
                    provider_id, provider_id_v2, model_profile
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    proposal.session_id.to_string(),
                    proposal.revision_id.to_string(),
                    task_id.to_string(),
                    node.key,
                    i64::try_from(index + 1).map_err(|_| StateError::InvalidRecord(
                        "graph has too many nodes".to_owned()
                    ))?,
                    legacy_provider_text(provider),
                    provider.as_str(),
                    enum_text(&node.profile)?,
                ],
            )?;
            let mut event = task_created_event(
                proposal.session_id,
                task_id,
                request.approved_at,
                proposal.revision_id,
                &node.key,
            );
            append_event_in_transaction(&transaction, &mut event)?;
            key_to_id.insert(node.key.clone(), task_id);
            task_ids.push(task_id);
        }
        for node in &proposal.nodes {
            let task_id = key_to_id[&node.key];
            for dependency in &node.dependencies {
                transaction.execute(
                    "INSERT INTO task_dependencies(
                        session_id, revision_id, task_id, depends_on_task_id
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        proposal.session_id.to_string(),
                        proposal.revision_id.to_string(),
                        task_id.to_string(),
                        key_to_id[dependency].to_string(),
                    ],
                )?;
            }
        }
        transaction.execute(
            "INSERT INTO graph_approvals(
                revision_id, proposal_hash, approved_by, approved_at, session_id,
                requirement_revision_id, validation_hash, base_commit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                request.revision_id.to_string(),
                request.expected_proposal_hash,
                request.approved_by.trim(),
                request.approved_at.to_rfc3339(),
                revision.session_id.to_string(),
                request
                    .authority
                    .as_ref()
                    .map(|authority| authority.requirement_revision_id.to_string()),
                request
                    .authority
                    .as_ref()
                    .map(|authority| &authority.validation_hash),
                request
                    .authority
                    .as_ref()
                    .map(|authority| &authority.base_commit),
            ],
        )?;
        transaction.execute(
            "UPDATE graph_revisions SET status = 'approved', completed_at = ?1
             WHERE revision_id = ?2 AND status = 'awaiting_approval'",
            params![
                request.approved_at.to_rfc3339(),
                request.revision_id.to_string()
            ],
        )?;
        let message = ConversationMessage {
            message_id: MessageId::new(),
            session_id: proposal.session_id,
            task_id: None,
            role: MessageRole::Orchestrator,
            kind: MessageKind::StateChange,
            state: MessageState::Final,
            content_redacted: format!(
                "Approved task graph {} and queued {} tasks.",
                proposal.revision_id,
                task_ids.len()
            ),
            created_at: request.approved_at,
            finalized_at: Some(request.approved_at),
        };
        insert_message(&transaction, &message)?;
        let mut message_event = graph_event(
            proposal.session_id,
            EventType::MessageAppended,
            EventActor::Orchestrator,
            request.approved_at,
            serde_json::json!({"message_id": message.message_id, "revision_id": proposal.revision_id}),
        );
        append_event_in_transaction(&transaction, &mut message_event)?;
        let mut approval_event = graph_event(
            proposal.session_id,
            EventType::GraphApproved,
            EventActor::User,
            request.approved_at,
            serde_json::json!({
                "revision_id": proposal.revision_id,
                "proposal_hash": request.expected_proposal_hash,
                "approved_by": request.approved_by.trim(),
                "task_count": task_ids.len(),
            }),
        );
        append_event_in_transaction(&transaction, &mut approval_event)?;
        transaction.commit()?;
        Ok(ApprovedGraph {
            revision_id: request.revision_id,
            proposal_hash: request.expected_proposal_hash.clone(),
            task_ids,
            replayed: false,
        })
    }
}

fn authority_from_validation(
    validation: &serde_json::Value,
) -> Option<orchestrator_domain::GraphValidationAuthority> {
    serde_json::from_value::<GraphValidationSummary>(validation.clone())
        .ok()
        .and_then(|summary| summary.authority)
}

fn validate_new_attempt(attempt: &NewGraphAttempt) -> StateResult<()> {
    if attempt.completed_at < attempt.started_at {
        return Err(StateError::InvalidRecord(
            "planning attempt completed before it started".to_owned(),
        ));
    }
    match attempt.status {
        GraphRevisionStatus::AwaitingApproval => {
            let proposal = attempt.proposal.as_ref().ok_or_else(|| {
                StateError::InvalidRecord("valid graph attempt has no proposal".to_owned())
            })?;
            let hash = attempt.proposal_hash.as_deref().ok_or_else(|| {
                StateError::InvalidRecord("valid graph attempt has no proposal hash".to_owned())
            })?;
            if hash.len() != 64
                || proposal.revision_id != attempt.revision_id
                || proposal.session_id != attempt.session_id
                || proposal.goal_message_id != attempt.goal_message_id
                || proposal.planner_provider != attempt.planner_provider
            {
                return Err(StateError::InvalidRecord(
                    "valid graph attempt identity or hash is inconsistent".to_owned(),
                ));
            }
        }
        GraphRevisionStatus::Invalid => {
            if attempt.proposal_hash.is_some() {
                return Err(StateError::InvalidRecord(
                    "invalid graph attempt cannot carry an approval hash".to_owned(),
                ));
            }
        }
        _ => {
            return Err(StateError::InvalidRecord(
                "new graph attempt must be invalid or awaiting approval".to_owned(),
            ));
        }
    }
    Ok(())
}

fn ensure_goal_identity(
    connection: &rusqlite::Connection,
    session_id: SessionId,
    goal_message_id: MessageId,
) -> StateResult<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversation_messages WHERE session_id = ?1 AND message_id = ?2)",
        params![session_id.to_string(), goal_message_id.to_string()],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(StateError::InvalidRecord(
            "planning goal message does not belong to its session".to_owned(),
        ));
    }
    Ok(())
}

fn graph_revision_by_id(
    connection: &rusqlite::Connection,
    revision_id: GraphRevisionId,
) -> StateResult<Option<StoredGraphRevision>> {
    let row = connection
        .query_row(
            "SELECT revision_id, session_id, goal_message_id, ordinal, status, proposal_hash,
                    proposal_json, validation_json, coalesce(planner_provider_v2, planner_provider),
                    created_at, completed_at
             FROM graph_revisions WHERE revision_id = ?1",
            [revision_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            },
        )
        .optional()?;
    row.map(|row| {
        Ok(StoredGraphRevision {
            revision_id: parse_id("revision_id", &row.0)?,
            session_id: parse_id("session_id", &row.1)?,
            goal_message_id: parse_id("goal_message_id", &row.2)?,
            ordinal: u64::try_from(row.3)
                .map_err(|_| StateError::InvalidRecord("negative graph ordinal".to_owned()))?,
            status: parse_enum("graph status", &row.4)?,
            proposal_hash: row.5,
            proposal: row
                .6
                .map(|value| serde_json::from_str(&value))
                .transpose()?,
            validation: serde_json::from_str(&row.7)?,
            planner_provider: row
                .8
                .map(|value| ProviderId::from_str(&value))
                .transpose()
                .map_err(|error| StateError::InvalidRecord(error.to_string()))?,
            created_at: parse_time("graph created_at", &row.9)?,
            completed_at: row
                .10
                .map(|value| parse_time("graph completed_at", &value))
                .transpose()?,
        })
    })
    .transpose()
}

pub(crate) fn graph_projection(
    connection: &rusqlite::Connection,
    revision_id: GraphRevisionId,
) -> StateResult<GraphProjection> {
    let revision = graph_revision_by_id(connection, revision_id)?.ok_or_else(|| {
        StateError::InvalidRecord("graph head references a missing revision".to_owned())
    })?;
    let mut task_statement = connection.prepare(
        "SELECT task_id, node_key, display_order, coalesce(provider_id_v2, provider_id), model_profile
         FROM session_tasks WHERE revision_id = ?1 ORDER BY display_order",
    )?;
    let tasks = task_statement
        .query_map([revision_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .map(|row| {
            let row = row?;
            Ok(GraphTaskProjection {
                task_id: parse_id("task_id", &row.0)?,
                node_key: row.1,
                display_order: u64::try_from(row.2).map_err(|_| {
                    StateError::InvalidRecord("negative graph task order".to_owned())
                })?,
                provider: ProviderId::from_str(&row.3)
                    .map_err(|error| StateError::InvalidRecord(error.to_string()))?,
                profile: parse_enum("model profile", &row.4)?,
            })
        })
        .collect::<StateResult<Vec<_>>>()?;
    let mut dependency_statement = connection.prepare(
        "SELECT task_id, depends_on_task_id FROM task_dependencies
         WHERE revision_id = ?1 ORDER BY task_id, depends_on_task_id",
    )?;
    let dependencies = dependency_statement
        .query_map([revision_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .map(|row| {
            let row = row?;
            Ok(GraphTaskDependency {
                task_id: parse_id("task_id", &row.0)?,
                depends_on_task_id: parse_id("depends_on_task_id", &row.1)?,
            })
        })
        .collect::<StateResult<Vec<_>>>()?;
    Ok(GraphProjection {
        revision,
        tasks,
        dependencies,
    })
}

fn insert_task(
    transaction: &rusqlite::Transaction<'_>,
    envelope: &TaskEnvelope,
) -> StateResult<()> {
    let timestamp = envelope.created_at.to_rfc3339();
    transaction.execute(
        "INSERT INTO tasks(
            task_id, schema_version, revision, state, resume_state, paused, objective,
            original_request_redacted, task_envelope_json, created_at, updated_at, archived_at
         ) VALUES (?1, ?2, 0, 'queued', NULL, 0, ?3, ?4, ?5, ?6, ?6, NULL)",
        params![
            envelope.task_id.to_string(),
            envelope.schema_version.as_str(),
            envelope.objective,
            envelope.original_request_redacted,
            serde_json::to_string(envelope)?,
            timestamp,
        ],
    )?;
    Ok(())
}

fn insert_message(
    transaction: &rusqlite::Transaction<'_>,
    message: &ConversationMessage,
) -> StateResult<()> {
    message
        .validate()
        .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
    let ordinal: i64 = transaction.query_row(
        "SELECT coalesce(max(ordinal), 0) + 1 FROM conversation_messages WHERE session_id = ?1",
        [message.session_id.to_string()],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO conversation_messages(
            message_id, session_id, task_id, ordinal, role, kind, state,
            content_redacted, created_at, finalized_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            message.message_id.to_string(),
            message.session_id.to_string(),
            message.task_id.map(|value| value.to_string()),
            ordinal,
            enum_text(&message.role)?,
            enum_text(&message.kind)?,
            enum_text(&message.state)?,
            message.content_redacted,
            message.created_at.to_rfc3339(),
            message.finalized_at.map(|value| value.to_rfc3339()),
        ],
    )?;
    Ok(())
}

fn task_ids_for_revision(
    connection: &rusqlite::Connection,
    revision_id: GraphRevisionId,
) -> StateResult<Vec<TaskId>> {
    let mut statement = connection.prepare(
        "SELECT task_id FROM session_tasks WHERE revision_id = ?1 ORDER BY display_order",
    )?;
    statement
        .query_map([revision_id.to_string()], |row| row.get::<_, String>(0))?
        .map(|row| parse_id("task_id", &row?))
        .collect()
}

fn task_created_event(
    session_id: SessionId,
    task_id: TaskId,
    occurred_at: DateTime<Utc>,
    revision_id: GraphRevisionId,
    node_key: &str,
) -> TaskEvent {
    TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id: Some(session_id),
        task_id: Some(task_id),
        occurred_at,
        event_type: EventType::TaskCreated,
        from_state: None,
        to_state: Some(TaskState::Queued),
        reason: Some("approved task graph materialization".to_owned()),
        actor: EventActor::Orchestrator,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload: serde_json::json!({"revision_id": revision_id, "node_key": node_key}),
        previous_hash: None,
        event_hash: String::new(),
    }
}

fn graph_event(
    session_id: SessionId,
    event_type: EventType,
    actor: EventActor,
    occurred_at: DateTime<Utc>,
    payload: serde_json::Value,
) -> TaskEvent {
    TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id: Some(session_id),
        task_id: None,
        occurred_at,
        event_type,
        from_state: None,
        to_state: None,
        reason: None,
        actor,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload,
        previous_hash: None,
        event_hash: String::new(),
    }
}

fn enum_text(value: &impl Serialize) -> StateResult<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| StateError::InvalidRecord("expected enum string".to_owned()))
}

const fn legacy_provider_text(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Gemini => "gemini",
        ProviderId::Agy | ProviderId::Codex => "codex",
        ProviderId::Claude => "claude",
    }
}

fn parse_enum<T: for<'de> Deserialize<'de>>(label: &str, value: &str) -> StateResult<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|error| StateError::InvalidRecord(format!("invalid {label}: {error}")))
}

fn parse_id<T: FromStr>(label: &str, value: &str) -> StateResult<T>
where
    T::Err: std::fmt::Display,
{
    T::from_str(value)
        .map_err(|error| StateError::InvalidRecord(format!("invalid {label}: {error}")))
}

fn parse_time(label: &str, value: &str) -> StateResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| StateError::InvalidRecord(format!("invalid {label}: {error}")))
}
