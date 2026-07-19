use std::{path::PathBuf, str::FromStr};

use chrono::{DateTime, TimeDelta, Utc};
use orchestrator_domain::{
    AttemptId, Checkpoint, CheckpointId, EventType, HandoverAcknowledgement, HandoverBundle,
    HandoverId, ProviderId, RepoPath, RoutingDecision, SUPPORTED_ROUTING_DECISION_SCHEMA_VERSIONS,
    SUPPORTED_TASK_ENVELOPE_SCHEMA_VERSIONS, SchemaVersion, TaskAssessment, TaskEnvelope,
    TaskEvent, TaskId, TaskState, TransitionGuards, UsageSnapshot, VerificationResult,
    WorkerResult,
};
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ArtifactStore, Database, StateError, StateResult, StoredArtifact,
    database::append_event_in_transaction,
};

const CHECKPOINT_DIFF_KIND: &str = "checkpoint_diff";
const CHECKPOINT_DIFF_MEDIA_TYPE: &str = "text/x-diff";

#[derive(Clone, Debug)]
pub struct NewTaskRecord<T> {
    pub task_id: TaskId,
    pub schema_version: String,
    pub state: TaskState,
    pub objective: String,
    pub original_request_redacted: String,
    pub envelope: T,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredTask {
    pub task_id: TaskId,
    pub schema_version: String,
    pub revision: u64,
    pub state: TaskState,
    pub resume_state: Option<TaskState>,
    pub paused: bool,
    pub objective: String,
    pub original_request_redacted: String,
    pub envelope: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskListFilter {
    pub state: Option<TaskState>,
    pub include_archived: bool,
    pub limit: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    Pause,
    Resume,
    Cancel,
    Handover,
    UsageOverride,
}

impl ControlAction {
    /// Whether replay after a process crash can be reconciled solely from the task projection.
    #[must_use]
    pub const fn is_restart_replay_safe(self) -> bool {
        matches!(self, Self::Pause | Self::Resume | Self::Cancel)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlRequest {
    pub control_id: Uuid,
    pub task_id: TaskId,
    pub action: ControlAction,
    pub payload: serde_json::Value,
    pub requested_by: String,
    pub requested_at: DateTime<Utc>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub outcome: Option<String>,
}

/// Restart recovery policy for controls claimed by a process that did not record completion.
///
/// Only idempotent projection controls are automatically requeued. Controls that can create
/// duplicate accounting or handover records always require reconciliation by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaimedControlRecoveryPolicy {
    pub stale_after: TimeDelta,
}

impl Default for ClaimedControlRecoveryPolicy {
    fn default() -> Self {
        Self {
            stale_after: TimeDelta::minutes(5),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRecoveryDisposition {
    StillClaimed,
    Requeued,
    ManualReconciliationRequired,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveredControl {
    pub request: ControlRequest,
    pub disposition: ControlRecoveryDisposition,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredTaskAttempt {
    pub attempt_id: AttemptId,
    pub task_id: TaskId,
    pub ordinal: u32,
    pub provider: Option<ProviderId>,
    pub worker_mode: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: Option<String>,
    /// Kept as JSON because a process may persist structured start-failure evidence before a
    /// complete domain `WorkerResult` exists.
    pub worker_result: Option<serde_json::Value>,
}

impl StoredTaskAttempt {
    /// Decodes a complete persisted worker result. Incomplete start-failure
    /// evidence deliberately remains available through [`Self::worker_result`].
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] when the JSON is not a complete supported
    /// [`WorkerResult`] contract.
    pub fn decoded_worker_result(&self) -> StateResult<Option<WorkerResult>> {
        self.worker_result
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(StateError::from)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredWorktree {
    pub worktree_id: Uuid,
    pub task_id: TaskId,
    pub repo_root: PathBuf,
    pub worktree_path: PathBuf,
    pub branch_name: String,
    pub base_revision: String,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub cleanup_approved_at: Option<DateTime<Utc>>,
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredHandover {
    pub checkpoint_id: CheckpointId,
    pub reason: String,
    pub bundle: HandoverBundle,
    pub acknowledgement: Option<HandoverAcknowledgement>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
struct CheckpointRow {
    checkpoint: Checkpoint,
    schema_version: String,
    integrity_hash: String,
    checkpoint_id: String,
    task_id: String,
    attempt_id: Option<String>,
    diff_artifact_id: Option<String>,
    git_head: Option<String>,
    artifact_task_id: Option<String>,
    artifact_kind: Option<String>,
    artifact_media_type: Option<String>,
    artifact: Option<StoredArtifact>,
}

/// Persistence-facing representation of a routing decision. The policy crate owns score
/// calculation; state only records the complete audit evidence supplied by the caller.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingAuditRecord {
    pub decision_id: String,
    pub task_id: TaskId,
    pub schema_version: String,
    pub selected_provider: Option<ProviderId>,
    pub model_profile: Option<String>,
    pub effort: Option<String>,
    pub difficulty: String,
    pub risks: serde_json::Value,
    pub candidates: serde_json::Value,
    pub policy: serde_json::Value,
    pub downgraded: bool,
    pub rationale: serde_json::Value,
    pub decided_at: DateTime<Utc>,
}

impl Database {
    pub fn create_task_envelope(&self, envelope: &TaskEnvelope) -> StateResult<()> {
        if !envelope.has_supported_schema() {
            return Err(StateError::InvalidRecord(format!(
                "unsupported task envelope schema version {}",
                envelope.schema_version
            )));
        }
        self.create_task(&NewTaskRecord {
            task_id: envelope.task_id,
            schema_version: envelope.schema_version.to_string(),
            state: TaskState::Queued,
            objective: envelope.objective.clone(),
            original_request_redacted: envelope.original_request_redacted.clone(),
            envelope,
            created_at: envelope.created_at,
        })
    }

    pub fn load_task_envelope(&self, task_id: TaskId) -> StateResult<Option<TaskEnvelope>> {
        self.load_task(task_id)?
            .map(|record| {
                let envelope: TaskEnvelope = serde_json::from_value(record.envelope)?;
                if record.schema_version != envelope.schema_version.as_str() {
                    return Err(StateError::InvalidRecord(
                        "task schema column does not match its envelope".to_owned(),
                    ));
                }
                Ok(envelope)
            })
            .transpose()
    }

    pub fn create_task<T: Serialize>(&self, task: &NewTaskRecord<T>) -> StateResult<()> {
        if task.objective.trim().is_empty() {
            return Err(StateError::InvalidRecord(
                "task objective must be non-empty".to_owned(),
            ));
        }
        ensure_supported_schema(
            "task envelope",
            &task.schema_version,
            SUPPORTED_TASK_ENVELOPE_SCHEMA_VERSIONS,
        )?;
        let envelope = serde_json::to_string(&task.envelope)?;
        let state = serde_string(&task.state)?;
        let timestamp = task.created_at.to_rfc3339();
        self.lock()?.execute(
            "INSERT INTO tasks( \
                task_id, schema_version, revision, state, resume_state, paused, objective, \
                original_request_redacted, task_envelope_json, created_at, updated_at, archived_at \
             ) VALUES (?1, ?2, 0, ?3, NULL, 0, ?4, ?5, ?6, ?7, ?7, NULL)",
            params![
                task.task_id.to_string(),
                task.schema_version,
                state,
                task.objective,
                task.original_request_redacted,
                envelope,
                timestamp,
            ],
        )?;
        Ok(())
    }

    /// Atomically creates a task projection and its initial `task_created` audit event.
    pub fn create_task_with_event<T: Serialize>(
        &self,
        task: &NewTaskRecord<T>,
        mut event: TaskEvent,
    ) -> StateResult<TaskEvent> {
        if event.event_type != EventType::TaskCreated
            || event.task_id != Some(task.task_id)
            || event.from_state.is_some()
            || event.to_state != Some(task.state)
        {
            return Err(StateError::InvalidRecord(
                "task-created event does not match the initial task projection".to_owned(),
            ));
        }
        if task.objective.trim().is_empty() {
            return Err(StateError::InvalidRecord(
                "task objective must be non-empty".to_owned(),
            ));
        }
        ensure_supported_schema(
            "task envelope",
            &task.schema_version,
            SUPPORTED_TASK_ENVELOPE_SCHEMA_VERSIONS,
        )?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let timestamp = task.created_at.to_rfc3339();
        transaction.execute(
            "INSERT INTO tasks( \
                task_id, schema_version, revision, state, resume_state, paused, objective, \
                original_request_redacted, task_envelope_json, created_at, updated_at, archived_at \
             ) VALUES (?1, ?2, 0, ?3, NULL, 0, ?4, ?5, ?6, ?7, ?7, NULL)",
            params![
                task.task_id.to_string(),
                task.schema_version,
                serde_string(&task.state)?,
                task.objective,
                task.original_request_redacted,
                serde_json::to_string(&task.envelope)?,
                timestamp,
            ],
        )?;
        append_event_in_transaction(&transaction, &mut event)?;
        transaction.commit()?;
        Ok(event)
    }

    pub fn load_task(&self, task_id: TaskId) -> StateResult<Option<StoredTask>> {
        let task = self
            .lock()?
            .query_row(
                "SELECT task_id, schema_version, revision, state, resume_state, paused, objective, \
                 original_request_redacted, task_envelope_json, created_at, updated_at, archived_at \
                 FROM tasks WHERE task_id = ?1",
                [task_id.to_string()],
                map_task,
            )
            .optional()?;
        task.map(validate_stored_task).transpose()
    }

    pub fn list_tasks(&self, filter: &TaskListFilter) -> StateResult<Vec<StoredTask>> {
        let limit = if filter.limit == 0 {
            100_i64
        } else {
            i64::try_from(filter.limit).unwrap_or(i64::MAX)
        };
        let state = filter.state.map(|value| serde_string(&value)).transpose()?;
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT task_id, schema_version, revision, state, resume_state, paused, objective, \
             original_request_redacted, task_envelope_json, created_at, updated_at, archived_at \
             FROM tasks WHERE (?1 IS NULL OR state = ?1) \
             AND (?2 = 1 OR archived_at IS NULL) ORDER BY updated_at DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![state, i64::from(filter.include_archived), limit],
            map_task,
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(validate_stored_task)
            .collect()
    }

    /// Atomically advances the task projection and inserts its hash-chained audit event.
    #[allow(clippy::too_many_arguments)]
    pub fn transition_task_with_event(
        &self,
        task_id: TaskId,
        expected_revision: u64,
        expected_state: TaskState,
        next_state: TaskState,
        resume_state: Option<TaskState>,
        paused: bool,
        guards: &TransitionGuards,
        updated_at: DateTime<Utc>,
        mut event: TaskEvent,
    ) -> StateResult<(u64, TaskEvent)> {
        expected_state
            .validate_transition(next_state, guards)
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        if event.task_id != Some(task_id)
            || event.from_state != Some(expected_state)
            || event.to_state != Some(next_state)
        {
            return Err(StateError::InvalidRecord(
                "transition event does not match task projection update".to_owned(),
            ));
        }
        let next_revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| StateError::InvalidRecord("task revision overflow".to_owned()))?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE tasks SET revision = ?1, state = ?2, resume_state = ?3, paused = ?4, \
             updated_at = ?5 WHERE task_id = ?6 AND revision = ?7 AND state = ?8 \
             AND archived_at IS NULL",
            params![
                next_revision,
                serde_string(&next_state)?,
                resume_state.map(|value| serde_string(&value)).transpose()?,
                i64::from(paused),
                updated_at.to_rfc3339(),
                task_id.to_string(),
                expected_revision,
                serde_string(&expected_state)?,
            ],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("task {task_id}"),
            });
        }
        append_event_in_transaction(&transaction, &mut event)?;
        transaction.commit()?;
        Ok((next_revision, event))
    }

    /// Atomically projects a safe-boundary pause and records the corresponding audit event.
    ///
    /// The task's current state is retained as its resume point. Running work must first reach a
    /// state from which the domain state machine permits `Blocked` (normally `Checkpointed`).
    pub fn pause_task_with_event(
        &self,
        task_id: TaskId,
        expected_revision: u64,
        updated_at: DateTime<Utc>,
        event: TaskEvent,
    ) -> StateResult<(u64, TaskEvent)> {
        let task = self
            .load_task(task_id)?
            .ok_or_else(|| StateError::InvalidRecord(format!("task {task_id} does not exist")))?;
        if task.revision != expected_revision {
            return Err(StateError::OptimisticConflict {
                entity: format!("task {task_id}"),
            });
        }
        if task.paused || task.resume_state.is_some() {
            return Err(StateError::InvalidRecord(format!(
                "task {task_id} already has a pause projection"
            )));
        }
        validate_pause_projection_event(&event, true)?;
        self.transition_task_with_event(
            task_id,
            expected_revision,
            task.state,
            TaskState::Blocked,
            Some(task.state),
            true,
            &TransitionGuards::default(),
            updated_at,
            event,
        )
    }

    /// Atomically clears a pause projection and returns to the exact recorded safe resume point.
    pub fn resume_task_with_event(
        &self,
        task_id: TaskId,
        expected_revision: u64,
        updated_at: DateTime<Utc>,
        event: TaskEvent,
    ) -> StateResult<(u64, TaskEvent)> {
        let task = self
            .load_task(task_id)?
            .ok_or_else(|| StateError::InvalidRecord(format!("task {task_id} does not exist")))?;
        if task.revision != expected_revision {
            return Err(StateError::OptimisticConflict {
                entity: format!("task {task_id}"),
            });
        }
        if task.state != TaskState::Blocked || !task.paused {
            return Err(StateError::InvalidRecord(format!(
                "task {task_id} is not paused"
            )));
        }
        let resume_state = task.resume_state.ok_or_else(|| {
            StateError::InvalidRecord(format!("paused task {task_id} has no resume point"))
        })?;
        if resume_state == TaskState::Blocked
            || resume_state.is_terminal()
            || resume_state
                .validate_transition(TaskState::Blocked, &TransitionGuards::default())
                .is_err()
        {
            return Err(StateError::InvalidRecord(format!(
                "paused task {task_id} has unsafe resume point {resume_state:?}"
            )));
        }
        validate_pause_projection_event(&event, false)?;
        self.transition_task_with_event(
            task_id,
            expected_revision,
            TaskState::Blocked,
            resume_state,
            None,
            false,
            &TransitionGuards {
                resume_point: Some(resume_state),
                ..TransitionGuards::default()
            },
            updated_at,
            event,
        )
    }

    pub fn record_usage_snapshot(
        &self,
        task_id: Option<TaskId>,
        snapshot: &UsageSnapshot,
    ) -> StateResult<Uuid> {
        snapshot
            .validate()
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        let snapshot_id = Uuid::now_v7();
        self.lock()?.execute(
            "INSERT INTO provider_usage_snapshots( \
                snapshot_id, task_id, provider_id, quota_scope, quota_period, usage_unit, used, \
                quota_limit, remaining, used_percent, remaining_percent, period_started_at, \
                resets_at, source, confidence, snapshot_json, collected_at \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                snapshot_id.to_string(),
                task_id.map(|value| value.to_string()),
                snapshot.provider.as_str(),
                snapshot.quota_scope.name,
                serde_string(&snapshot.quota_period)?,
                serde_json::to_string(&snapshot.quota_scope.unit)?,
                snapshot.used,
                snapshot.limit,
                snapshot.remaining,
                snapshot.used_percent,
                snapshot.remaining_percent,
                snapshot.period_started_at.map(|value| value.to_rfc3339()),
                snapshot.resets_at.map(|value| value.to_rfc3339()),
                serde_string(&snapshot.source)?,
                serde_string(&snapshot.confidence)?,
                serde_json::to_string(snapshot)?,
                snapshot.collected_at.to_rfc3339(),
            ],
        )?;
        Ok(snapshot_id)
    }

    pub fn list_usage_snapshots(
        &self,
        provider: Option<ProviderId>,
        limit: usize,
    ) -> StateResult<Vec<UsageSnapshot>> {
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT snapshot_json FROM provider_usage_snapshots \
             WHERE (?1 IS NULL OR provider_id = ?1) ORDER BY collected_at DESC LIMIT ?2",
        )?;
        let rows =
            statement.query_map(params![provider.map(ProviderId::as_str), limit], |row| {
                let json: String = row.get(0)?;
                let snapshot: UsageSnapshot = serde_json::from_str(&json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                snapshot.validate().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(snapshot)
            })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    pub fn record_routing_audit(&self, record: &RoutingAuditRecord) -> StateResult<()> {
        ensure_supported_schema(
            "routing decision",
            &record.schema_version,
            SUPPORTED_ROUTING_DECISION_SCHEMA_VERSIONS,
        )?;
        self.lock()?.execute(
            "INSERT INTO routing_decisions( \
                decision_id, task_id, selected_provider, model_profile, effort, difficulty, \
                risk_json, candidates_json, policy_json, downgraded, rationale_json, \
                schema_version, decided_at \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                record.decision_id,
                record.task_id.to_string(),
                record.selected_provider.map(ProviderId::as_str),
                record.model_profile,
                record.effort,
                record.difficulty,
                serde_json::to_string(&record.risks)?,
                serde_json::to_string(&record.candidates)?,
                serde_json::to_string(&record.policy)?,
                i64::from(record.downgraded),
                serde_json::to_string(&record.rationale)?,
                record.schema_version,
                record.decided_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn record_routing_decision(
        &self,
        decision: &RoutingDecision,
        assessment: &TaskAssessment,
    ) -> StateResult<()> {
        if !decision.has_supported_schema() {
            return Err(StateError::InvalidRecord(format!(
                "unsupported routing decision schema version {}",
                decision.schema_version
            )));
        }
        let record = RoutingAuditRecord {
            decision_id: decision.decision_id.to_string(),
            task_id: decision.task_id,
            schema_version: decision.schema_version.to_string(),
            selected_provider: decision.selected_provider,
            model_profile: decision
                .selected_profile
                .map(|value| serde_string(&value))
                .transpose()?,
            effort: decision
                .reasoning_effort
                .map(|value| serde_string(&value))
                .transpose()?,
            difficulty: serde_string(&assessment.difficulty)?,
            risks: serde_json::to_value(&assessment.risk_tags)?,
            candidates: serde_json::to_value(&decision.candidate_scores)?,
            policy: serde_json::json!({
                "name": decision.applied_policy,
                "parallel_workers": decision.parallel_workers,
                "blocked_options": decision.blocked_options,
            }),
            downgraded: decision.downgrade,
            rationale: serde_json::to_value(&decision.rationale)?,
            decided_at: decision.created_at,
        };
        self.record_routing_audit(&record)
    }

    pub fn link_routing_usage(&self, decision_id: &str, snapshot_ids: &[Uuid]) -> StateResult<()> {
        if decision_id.trim().is_empty() {
            return Err(StateError::InvalidRecord(
                "routing decision ID must be non-empty".to_owned(),
            ));
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        for snapshot_id in snapshot_ids {
            transaction.execute(
                "INSERT INTO routing_decision_usage(decision_id, snapshot_id) VALUES (?1, ?2)",
                params![decision_id, snapshot_id.to_string()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn list_routing_audits(
        &self,
        task_id: TaskId,
        limit: usize,
    ) -> StateResult<Vec<RoutingAuditRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT decision_id, task_id, schema_version, selected_provider, model_profile, \
             effort, difficulty, risk_json, candidates_json, policy_json, downgraded, \
             rationale_json, decided_at FROM routing_decisions \
             WHERE task_id = ?1 ORDER BY decided_at DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![
                task_id.to_string(),
                i64::try_from(limit.max(1)).unwrap_or(i64::MAX)
            ],
            map_routing_audit,
        )?;
        let records = rows.collect::<Result<Vec<_>, _>>()?;
        for record in &records {
            ensure_supported_schema(
                "routing decision",
                &record.schema_version,
                SUPPORTED_ROUTING_DECISION_SCHEMA_VERSIONS,
            )?;
        }
        Ok(records)
    }

    /// Returns all attempts in execution order so restart recovery can reconstruct worker history.
    pub fn list_task_attempts(&self, task_id: TaskId) -> StateResult<Vec<StoredTaskAttempt>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT attempt_id, task_id, ordinal, provider_id, worker_mode, started_at, ended_at, \
             outcome, worker_result_json FROM task_attempts WHERE task_id = ?1 ORDER BY ordinal",
        )?;
        let rows = statement.query_map([task_id.to_string()], map_task_attempt)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    pub fn latest_task_attempt(&self, task_id: TaskId) -> StateResult<Option<StoredTaskAttempt>> {
        self.lock()?
            .query_row(
                "SELECT attempt_id, task_id, ordinal, provider_id, worker_mode, started_at, \
                 ended_at, outcome, worker_result_json FROM task_attempts WHERE task_id = ?1 \
                 ORDER BY ordinal DESC LIMIT 1",
                [task_id.to_string()],
                map_task_attempt,
            )
            .optional()
            .map_err(StateError::from)
    }

    /// Returns the one active worktree for a task.
    ///
    /// Multiple active rows are treated as corrupted/ambiguous recovery state instead of choosing
    /// one silently.
    pub fn active_worktree(&self, task_id: TaskId) -> StateResult<Option<StoredWorktree>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT worktree_id, task_id, repo_root, worktree_path, branch_name, base_revision, \
             state, created_at, cleanup_approved_at, archived_at FROM worktrees \
             WHERE task_id = ?1 AND state = 'active' AND archived_at IS NULL \
             ORDER BY created_at DESC LIMIT 2",
        )?;
        let rows = statement.query_map([task_id.to_string()], map_worktree)?;
        let worktrees = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)?;
        match worktrees.as_slice() {
            [] => Ok(None),
            [worktree] => Ok(Some(worktree.clone())),
            _ => Err(StateError::InvalidRecord(format!(
                "task {task_id} has multiple active worktrees"
            ))),
        }
    }

    pub fn request_control(
        &self,
        task_id: TaskId,
        action: ControlAction,
        payload: serde_json::Value,
        requested_by: &str,
        requested_at: DateTime<Utc>,
    ) -> StateResult<ControlRequest> {
        if requested_by.trim().is_empty() {
            return Err(StateError::InvalidRecord(
                "control requester must be non-empty".to_owned(),
            ));
        }
        let request = ControlRequest {
            control_id: Uuid::now_v7(),
            task_id,
            action,
            payload,
            requested_by: requested_by.to_owned(),
            requested_at,
            claimed_at: None,
            completed_at: None,
            outcome: None,
        };
        self.lock()?.execute(
            "INSERT INTO task_controls(control_id, task_id, action, payload_json, requested_by, \
             requested_at, claimed_at, completed_at, outcome) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL)",
            params![
                request.control_id.to_string(),
                task_id.to_string(),
                serde_string(&action)?,
                serde_json::to_string(&request.payload)?,
                request.requested_by,
                requested_at.to_rfc3339(),
            ],
        )?;
        Ok(request)
    }

    pub fn pending_controls(&self, task_id: TaskId) -> StateResult<Vec<ControlRequest>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT control_id, task_id, action, payload_json, requested_by, requested_at, \
             claimed_at, completed_at, outcome FROM task_controls \
             WHERE task_id = ?1 AND claimed_at IS NULL AND completed_at IS NULL \
             ORDER BY requested_at",
        )?;
        let rows = statement.query_map([task_id.to_string()], map_control)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    /// Controls claimed by an earlier process but never completed.
    pub fn claimed_incomplete_controls(&self, task_id: TaskId) -> StateResult<Vec<ControlRequest>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT control_id, task_id, action, payload_json, requested_by, requested_at, \
             claimed_at, completed_at, outcome FROM task_controls \
             WHERE task_id = ?1 AND claimed_at IS NOT NULL AND completed_at IS NULL \
             ORDER BY requested_at",
        )?;
        let rows = statement.query_map([task_id.to_string()], map_control)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    /// Recovers stale claims after an orchestrator restart according to a conservative policy.
    ///
    /// Pause, resume, and cancel are requeued once their claim is stale. Handover and usage
    /// override remain claimed and are reported for manual reconciliation, since replay could
    /// duplicate sealed handovers or accounting evidence.
    pub fn recover_claimed_controls(
        &self,
        task_id: TaskId,
        now: DateTime<Utc>,
        policy: ClaimedControlRecoveryPolicy,
    ) -> StateResult<Vec<RecoveredControl>> {
        if policy.stale_after < TimeDelta::zero() {
            return Err(StateError::InvalidRecord(
                "control recovery stale interval cannot be negative".to_owned(),
            ));
        }
        let stale_before = now.checked_sub_signed(policy.stale_after).ok_or_else(|| {
            StateError::InvalidRecord(
                "control recovery cutoff is outside the time range".to_owned(),
            )
        })?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let controls = {
            let mut statement = transaction.prepare(
                "SELECT control_id, task_id, action, payload_json, requested_by, requested_at, \
                 claimed_at, completed_at, outcome FROM task_controls \
                 WHERE task_id = ?1 AND claimed_at IS NOT NULL AND completed_at IS NULL \
                 ORDER BY requested_at",
            )?;
            let rows = statement.query_map([task_id.to_string()], map_control)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut recovered = Vec::with_capacity(controls.len());
        for request in controls {
            let claimed_at = request.claimed_at.ok_or_else(|| {
                StateError::InvalidRecord(format!(
                    "claimed control {} has no claim timestamp",
                    request.control_id
                ))
            })?;
            let disposition = if claimed_at > stale_before {
                ControlRecoveryDisposition::StillClaimed
            } else if request.action.is_restart_replay_safe() {
                let changed = transaction.execute(
                    "UPDATE task_controls SET claimed_at = NULL \
                     WHERE control_id = ?1 AND claimed_at = ?2 AND completed_at IS NULL",
                    params![request.control_id.to_string(), claimed_at.to_rfc3339()],
                )?;
                if changed != 1 {
                    return Err(StateError::OptimisticConflict {
                        entity: format!("control {}", request.control_id),
                    });
                }
                ControlRecoveryDisposition::Requeued
            } else {
                ControlRecoveryDisposition::ManualReconciliationRequired
            };
            recovered.push(RecoveredControl {
                request,
                disposition,
            });
        }
        transaction.commit()?;
        Ok(recovered)
    }

    pub fn claim_control(&self, control_id: Uuid, claimed_at: DateTime<Utc>) -> StateResult<bool> {
        let changed = self.lock()?.execute(
            "UPDATE task_controls SET claimed_at = ?1 \
             WHERE control_id = ?2 AND claimed_at IS NULL AND completed_at IS NULL",
            params![claimed_at.to_rfc3339(), control_id.to_string()],
        )?;
        Ok(changed == 1)
    }

    pub fn complete_control(
        &self,
        control_id: Uuid,
        outcome: &str,
        completed_at: DateTime<Utc>,
    ) -> StateResult<()> {
        if outcome.trim().is_empty() {
            return Err(StateError::InvalidRecord(
                "control outcome must be non-empty".to_owned(),
            ));
        }
        let changed = self.lock()?.execute(
            "UPDATE task_controls SET completed_at = ?1, outcome = ?2 \
             WHERE control_id = ?3 AND claimed_at IS NOT NULL AND completed_at IS NULL",
            params![completed_at.to_rfc3339(), outcome, control_id.to_string(),],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("control {control_id}"),
            });
        }
        Ok(())
    }

    pub fn record_checkpoint(&self, checkpoint: &Checkpoint) -> StateResult<()> {
        if !checkpoint.has_supported_schema() {
            return Err(StateError::InvalidRecord(format!(
                "unsupported checkpoint schema version {}",
                checkpoint.schema_version
            )));
        }
        let valid = checkpoint
            .verify_integrity()
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        if !valid {
            return Err(StateError::InvalidRecord(
                "checkpoint integrity hash is invalid".to_owned(),
            ));
        }
        let diff_artifact = checkpoint
            .diff_path
            .as_ref()
            .map(|path| self.inspect_checkpoint_diff(checkpoint, path))
            .transpose()?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let diff_artifact_id = diff_artifact
            .as_ref()
            .map(|artifact| register_checkpoint_artifact(&transaction, checkpoint, artifact))
            .transpose()?;
        transaction.execute(
            "INSERT INTO checkpoints(checkpoint_id, task_id, attempt_id, schema_version, \
             checkpoint_json, integrity_hash, diff_artifact_id, git_head, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                checkpoint.checkpoint_id.to_string(),
                checkpoint.task_id.to_string(),
                checkpoint.attempt_id.to_string(),
                checkpoint.schema_version.as_str(),
                serde_json::to_string(checkpoint)?,
                checkpoint.integrity_hash,
                diff_artifact_id,
                checkpoint.git_base,
                checkpoint.created_at.to_rfc3339(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn load_checkpoint(&self, checkpoint_id: CheckpointId) -> StateResult<Option<Checkpoint>> {
        let connection = self.lock()?;
        let row = connection
            .query_row(
                checkpoint_select(
                    "WHERE checkpoints.checkpoint_id = ?1 ORDER BY checkpoints.rowid DESC LIMIT 1",
                )
                .as_str(),
                [checkpoint_id.to_string()],
                map_checkpoint_row,
            )
            .optional()?;
        drop(connection);
        row.map(|row| self.verify_checkpoint_row(None, row))
            .transpose()
    }

    /// Loads the newest checkpoint and verifies its sealed integrity before returning it.
    pub fn latest_sealed_checkpoint(&self, task_id: TaskId) -> StateResult<Option<Checkpoint>> {
        let connection = self.lock()?;
        let row = connection
            .query_row(
                checkpoint_select(
                    "WHERE checkpoints.task_id = ?1 \
                     ORDER BY checkpoints.created_at DESC, checkpoints.rowid DESC LIMIT 1",
                )
                .as_str(),
                [task_id.to_string()],
                map_checkpoint_row,
            )
            .optional()?;
        drop(connection);
        row.map(|row| self.verify_checkpoint_row(Some(task_id), row))
            .transpose()
    }

    fn inspect_checkpoint_diff(
        &self,
        checkpoint: &Checkpoint,
        path: &RepoPath,
    ) -> StateResult<StoredArtifact> {
        let expected_digest = checkpoint_diff_digest(checkpoint, path)?;
        let artifact = self.artifact_store()?.inspect(path.clone())?;
        if artifact.sha256 != expected_digest {
            return Err(StateError::InvalidRecord(format!(
                "checkpoint diff digest does not match its content-addressed path: {path}"
            )));
        }
        Ok(artifact)
    }

    fn verify_checkpoint_row(
        &self,
        expected_task_id: Option<TaskId>,
        row: CheckpointRow,
    ) -> StateResult<Checkpoint> {
        let checkpoint = row.checkpoint;
        if !checkpoint.has_supported_schema() {
            return Err(StateError::InvalidRecord(format!(
                "unsupported checkpoint schema version {}",
                checkpoint.schema_version
            )));
        }
        if expected_task_id.is_some_and(|task_id| checkpoint.task_id != task_id) {
            return Err(StateError::InvalidRecord(
                "checkpoint task does not match its database owner".to_owned(),
            ));
        }
        let checkpoint_attempt_id = checkpoint.attempt_id.to_string();
        if row.schema_version != checkpoint.schema_version.as_str()
            || row.integrity_hash != checkpoint.integrity_hash
            || row.checkpoint_id != checkpoint.checkpoint_id.to_string()
            || row.task_id != checkpoint.task_id.to_string()
            || row.attempt_id.as_deref() != Some(checkpoint_attempt_id.as_str())
            || row.git_head.as_deref() != checkpoint.git_base.as_deref()
        {
            return Err(StateError::InvalidRecord(
                "checkpoint database columns do not match its sealed document".to_owned(),
            ));
        }
        let valid = checkpoint
            .verify_integrity()
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        if !valid {
            return Err(StateError::InvalidRecord(
                "checkpoint integrity hash is invalid".to_owned(),
            ));
        }

        let checkpoint_task_id = checkpoint.task_id.to_string();
        match (
            checkpoint.diff_path.as_ref(),
            row.diff_artifact_id.as_deref(),
            row.artifact.as_ref(),
        ) {
            (None, None, None) => {}
            (Some(path), Some(_), Some(artifact)) => {
                if artifact.relative_path != *path
                    || row.artifact_task_id.as_deref() != Some(checkpoint_task_id.as_str())
                    || row.artifact_kind.as_deref() != Some(CHECKPOINT_DIFF_KIND)
                    || row.artifact_media_type.as_deref() != Some(CHECKPOINT_DIFF_MEDIA_TYPE)
                {
                    return Err(StateError::InvalidRecord(
                        "checkpoint diff artifact metadata does not match its owner".to_owned(),
                    ));
                }
                let expected_digest = checkpoint_diff_digest(&checkpoint, path)?;
                if artifact.sha256 != expected_digest {
                    return Err(StateError::InvalidRecord(
                        "checkpoint diff artifact digest does not match its sealed path".to_owned(),
                    ));
                }
                self.artifact_store()?
                    .read_verified(artifact)
                    .map_err(|error| {
                        StateError::InvalidRecord(format!(
                            "checkpoint diff artifact failed file verification: {error}"
                        ))
                    })?;
            }
            _ => {
                return Err(StateError::InvalidRecord(
                    "checkpoint diff path and registered artifact are inconsistent".to_owned(),
                ));
            }
        }
        Ok(checkpoint)
    }

    fn artifact_store(&self) -> StateResult<ArtifactStore> {
        if self.path() == std::path::Path::new(":memory:") {
            return Err(StateError::InvalidRecord(
                "an in-memory database cannot verify external checkpoint artifacts".to_owned(),
            ));
        }
        let root = self.path().parent().ok_or_else(|| {
            StateError::InvalidRecord("database path has no artifact root".to_owned())
        })?;
        ArtifactStore::open(root)
    }

    pub fn record_handover(
        &self,
        checkpoint_id: CheckpointId,
        reason: &str,
        bundle: &HandoverBundle,
        acknowledgement: Option<&HandoverAcknowledgement>,
    ) -> StateResult<()> {
        if !bundle.has_supported_schema()
            || acknowledgement.is_some_and(|value| !value.has_supported_schema())
        {
            return Err(StateError::InvalidRecord(format!(
                "unsupported handover schema version {}",
                bundle.schema_version
            )));
        }
        let checkpoint = self.load_checkpoint(checkpoint_id)?.ok_or_else(|| {
            StateError::InvalidRecord(format!(
                "handover references missing checkpoint {checkpoint_id}"
            ))
        })?;
        if checkpoint.task_id != bundle.task_id
            || checkpoint.diff_path != bundle.diff_path
            || checkpoint.git_base != bundle.git_base
            || checkpoint.files_changed != bundle.files_changed
        {
            return Err(StateError::InvalidRecord(
                "handover evidence does not match its sealed checkpoint".to_owned(),
            ));
        }
        let valid = bundle
            .verify_integrity()
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        if !valid || acknowledgement.is_some_and(|value| !value.matches(bundle)) {
            return Err(StateError::InvalidRecord(
                "handover bundle or acknowledgement failed integrity validation".to_owned(),
            ));
        }
        self.lock()?.execute(
            "INSERT INTO handovers(handover_id, task_id, checkpoint_id, schema_version, \
             from_provider, to_provider, reason, bundle_json, integrity_hash, \
             acknowledgement_json, started_at, completed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                bundle.handover_id.to_string(),
                bundle.task_id.to_string(),
                checkpoint_id.to_string(),
                bundle.schema_version.as_str(),
                bundle.current_worker.as_str(),
                bundle.recommended_next_worker.as_str(),
                reason,
                serde_json::to_string(bundle)?,
                bundle.integrity_hash,
                acknowledgement.map(serde_json::to_string).transpose()?,
                bundle.created_at.to_rfc3339(),
                acknowledgement.map(|value| value.acknowledged_at.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    /// Completes a previously inserted handover exactly once after validating the
    /// acknowledgement against the sealed bundle stored in the database.
    pub fn complete_handover(
        &self,
        handover_id: HandoverId,
        acknowledgement: &HandoverAcknowledgement,
    ) -> StateResult<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let (bundle_json, stored_schema_version, stored_integrity_hash): (String, String, String) = transaction
            .query_row(
                "SELECT bundle_json, schema_version, integrity_hash FROM handovers WHERE handover_id = ?1 \
                 AND acknowledgement_json IS NULL AND completed_at IS NULL",
                [handover_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| StateError::OptimisticConflict {
                entity: format!("handover {handover_id}"),
            })?;
        let bundle: HandoverBundle = serde_json::from_str(&bundle_json)?;
        if !bundle.has_supported_schema() || !acknowledgement.has_supported_schema() {
            return Err(StateError::InvalidRecord(
                "unsupported handover acknowledgement schema version".to_owned(),
            ));
        }
        if stored_schema_version != bundle.schema_version.as_str()
            || stored_integrity_hash != bundle.integrity_hash
        {
            return Err(StateError::InvalidRecord(
                "handover database columns do not match its sealed bundle".to_owned(),
            ));
        }
        let bundle_valid = bundle
            .verify_integrity()
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        if !bundle_valid || !acknowledgement.matches(&bundle) {
            return Err(StateError::InvalidRecord(
                "handover acknowledgement does not match the stored sealed bundle".to_owned(),
            ));
        }
        let changed = transaction.execute(
            "UPDATE handovers SET acknowledgement_json = ?1, completed_at = ?2 \
             WHERE handover_id = ?3 AND acknowledgement_json IS NULL AND completed_at IS NULL",
            params![
                serde_json::to_string(acknowledgement)?,
                acknowledgement.acknowledged_at.to_rfc3339(),
                handover_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("handover {handover_id}"),
            });
        }
        transaction.commit()?;
        Ok(())
    }

    /// Loads and integrity-checks the newest handover, including its acknowledgement when present.
    pub fn latest_handover(&self, task_id: TaskId) -> StateResult<Option<StoredHandover>> {
        let connection = self.lock()?;
        let handover = connection
            .query_row(
                "SELECT checkpoint_id, reason, bundle_json, acknowledgement_json, started_at, \
                 completed_at, schema_version, integrity_hash FROM handovers WHERE task_id = ?1 \
                 ORDER BY started_at DESC, rowid DESC LIMIT 1",
                [task_id.to_string()],
                |row| {
                    Ok((
                        map_handover(row)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;
        drop(connection);
        let handover = handover
            .map(|(handover, stored_schema_version, stored_integrity_hash)| {
                if handover.bundle.task_id != task_id {
                    return Err(StateError::InvalidRecord(
                        "handover task does not match its database owner".to_owned(),
                    ));
                }
                if !handover.bundle.has_supported_schema()
                    || handover
                        .acknowledgement
                        .as_ref()
                        .is_some_and(|value| !value.has_supported_schema())
                {
                    return Err(StateError::InvalidRecord(
                        "latest handover uses an unsupported schema version".to_owned(),
                    ));
                }
                if stored_schema_version != handover.bundle.schema_version.as_str()
                    || stored_integrity_hash != handover.bundle.integrity_hash
                {
                    return Err(StateError::InvalidRecord(
                        "handover database columns do not match its sealed bundle".to_owned(),
                    ));
                }
                let valid = handover
                    .bundle
                    .verify_integrity()
                    .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
                if !valid
                    || handover.acknowledgement.is_some() != handover.completed_at.is_some()
                    || handover
                        .acknowledgement
                        .as_ref()
                        .is_some_and(|acknowledgement| !acknowledgement.matches(&handover.bundle))
                {
                    return Err(StateError::InvalidRecord(
                        "latest handover failed integrity validation".to_owned(),
                    ));
                }
                Ok(handover)
            })
            .transpose()?;
        if let Some(stored) = handover.as_ref() {
            let checkpoint = self.load_checkpoint(stored.checkpoint_id)?.ok_or_else(|| {
                StateError::InvalidRecord("handover checkpoint is missing".to_owned())
            })?;
            if checkpoint.task_id != stored.bundle.task_id
                || checkpoint.diff_path != stored.bundle.diff_path
                || checkpoint.git_base != stored.bundle.git_base
                || checkpoint.files_changed != stored.bundle.files_changed
            {
                return Err(StateError::InvalidRecord(
                    "latest handover evidence does not match its verified checkpoint".to_owned(),
                ));
            }
        }
        Ok(handover)
    }

    pub fn record_verification(&self, result: &VerificationResult) -> StateResult<()> {
        self.lock()?.execute(
            "INSERT INTO verification_results(verification_id, task_id, attempt_id, \
             reviewer_provider, outcome, schema_version, result_json, started_at, completed_at) \
             VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                result.verification_id.to_string(),
                result.task_id.to_string(),
                result.reviewer_provider.map(ProviderId::as_str),
                serde_string(&result.status)?,
                result.schema_version.as_str(),
                serde_json::to_string(result)?,
                result.verified_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }
}

fn checkpoint_select(suffix: &str) -> String {
    format!(
        "SELECT checkpoints.checkpoint_json, checkpoints.schema_version, \
         checkpoints.integrity_hash, checkpoints.checkpoint_id, checkpoints.task_id, \
         checkpoints.attempt_id, checkpoints.diff_artifact_id, checkpoints.git_head, \
         artifacts.task_id, artifacts.kind, artifacts.relative_path, artifacts.sha256, \
         artifacts.byte_length, artifacts.media_type FROM checkpoints \
         LEFT JOIN artifacts ON artifacts.artifact_id = checkpoints.diff_artifact_id {suffix}"
    )
}

fn map_checkpoint_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointRow> {
    let relative_path = row.get::<_, Option<String>>(10)?;
    let sha256 = row.get::<_, Option<String>>(11)?;
    let byte_length = row.get::<_, Option<i64>>(12)?;
    let artifact = match (relative_path, sha256, byte_length) {
        (None, None, None) => None,
        (Some(path), Some(sha256), Some(byte_length)) => Some(StoredArtifact {
            relative_path: RepoPath::try_from(path).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    10,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
            sha256,
            byte_length: u64::try_from(byte_length).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    12,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })?,
        }),
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "checkpoint artifact metadata is incomplete",
                )),
            ));
        }
    };
    Ok(CheckpointRow {
        checkpoint: parse_json(row.get(0)?, 0)?,
        schema_version: row.get(1)?,
        integrity_hash: row.get(2)?,
        checkpoint_id: row.get(3)?,
        task_id: row.get(4)?,
        attempt_id: row.get(5)?,
        diff_artifact_id: row.get(6)?,
        git_head: row.get(7)?,
        artifact_task_id: row.get(8)?,
        artifact_kind: row.get(9)?,
        artifact_media_type: row.get(13)?,
        artifact,
    })
}

fn checkpoint_diff_digest(checkpoint: &Checkpoint, path: &RepoPath) -> StateResult<String> {
    let expected_parent =
        std::path::PathBuf::from("checkpoints").join(checkpoint.checkpoint_id.to_string());
    if path.as_path().parent() != Some(expected_parent.as_path()) {
        return Err(StateError::InvalidRecord(format!(
            "checkpoint diff path is not owned by checkpoint {}: {path}",
            checkpoint.checkpoint_id
        )));
    }
    let file_name = path
        .as_path()
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| {
            StateError::InvalidRecord("checkpoint diff filename is invalid".to_owned())
        })?;
    let digest = file_name
        .strip_prefix("worktree.")
        .and_then(|value| value.strip_suffix(".diff"))
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| {
            StateError::InvalidRecord(
                "checkpoint diff path is not a content-addressed worktree diff".to_owned(),
            )
        })?;
    Ok(digest.to_owned())
}

fn register_checkpoint_artifact(
    transaction: &rusqlite::Transaction<'_>,
    checkpoint: &Checkpoint,
    artifact: &StoredArtifact,
) -> StateResult<String> {
    let artifact_id = Uuid::now_v7().to_string();
    let checkpoint_task_id = checkpoint.task_id.to_string();
    let byte_length = i64::try_from(artifact.byte_length).map_err(|_| {
        StateError::InvalidRecord("checkpoint diff exceeds SQLite length range".to_owned())
    })?;
    transaction.execute(
        "INSERT INTO artifacts(artifact_id, task_id, kind, relative_path, sha256, byte_length, \
         media_type, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT(relative_path) DO NOTHING",
        params![
            artifact_id,
            checkpoint_task_id,
            CHECKPOINT_DIFF_KIND,
            artifact.relative_path.to_string(),
            artifact.sha256,
            byte_length,
            CHECKPOINT_DIFF_MEDIA_TYPE,
            checkpoint.created_at.to_rfc3339(),
        ],
    )?;
    let stored: (
        String,
        Option<String>,
        String,
        String,
        String,
        i64,
        Option<String>,
    ) = transaction.query_row(
        "SELECT artifact_id, task_id, kind, relative_path, sha256, byte_length, media_type \
             FROM artifacts WHERE relative_path = ?1",
        [artifact.relative_path.to_string()],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;
    if stored.1.as_deref() != Some(checkpoint_task_id.as_str())
        || stored.2 != CHECKPOINT_DIFF_KIND
        || stored.3 != artifact.relative_path.to_string()
        || stored.4 != artifact.sha256
        || stored.5 != byte_length
        || stored.6.as_deref() != Some(CHECKPOINT_DIFF_MEDIA_TYPE)
    {
        return Err(StateError::InvalidRecord(
            "registered checkpoint artifact metadata conflicts with sealed evidence".to_owned(),
        ));
    }
    Ok(stored.0)
}

fn map_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredTask> {
    Ok(StoredTask {
        task_id: parse_id(row.get::<_, String>(0)?, 0)?,
        schema_version: row.get(1)?,
        revision: row.get(2)?,
        state: parse_json_string(row.get::<_, String>(3)?, 3)?,
        resume_state: row
            .get::<_, Option<String>>(4)?
            .map(|value| parse_json_string(value, 4))
            .transpose()?,
        paused: row.get(5)?,
        objective: row.get(6)?,
        original_request_redacted: row.get(7)?,
        envelope: parse_json(row.get(8)?, 8)?,
        created_at: parse_datetime(row.get(9)?, 9)?,
        updated_at: parse_datetime(row.get(10)?, 10)?,
        archived_at: row
            .get::<_, Option<String>>(11)?
            .map(|value| parse_datetime(value, 11))
            .transpose()?,
    })
}

fn validate_stored_task(task: StoredTask) -> StateResult<StoredTask> {
    ensure_supported_schema(
        "task envelope",
        &task.schema_version,
        SUPPORTED_TASK_ENVELOPE_SCHEMA_VERSIONS,
    )?;
    let embedded_schema = task
        .envelope
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            StateError::InvalidRecord("task envelope has no string schema version".to_owned())
        })?;
    if embedded_schema != task.schema_version {
        return Err(StateError::InvalidRecord(
            "task schema column does not match its envelope".to_owned(),
        ));
    }
    Ok(task)
}

fn map_task_attempt(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredTaskAttempt> {
    let attempt_id = parse_id(row.get(0)?, 0)?;
    let task_id = parse_id(row.get(1)?, 1)?;
    let worker_result: Option<serde_json::Value> = row
        .get::<_, Option<String>>(8)?
        .map(|value| parse_json(value, 8))
        .transpose()?;
    if let Some(value) = worker_result
        .as_ref()
        .filter(|value| value.get("schema_version").is_some())
    {
        let result: WorkerResult = serde_json::from_value(value.clone()).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        if result.attempt_id != attempt_id || result.task_id != task_id {
            return Err(invalid_sql_record(
                8,
                "worker result identity does not match its attempt row",
            ));
        }
    }
    Ok(StoredTaskAttempt {
        attempt_id,
        task_id,
        ordinal: row.get(2)?,
        provider: row
            .get::<_, Option<String>>(3)?
            .map(|value| parse_json_string(value, 3))
            .transpose()?,
        worker_mode: row.get(4)?,
        started_at: parse_datetime(row.get(5)?, 5)?,
        ended_at: row
            .get::<_, Option<String>>(6)?
            .map(|value| parse_datetime(value, 6))
            .transpose()?,
        outcome: row.get(7)?,
        worker_result,
    })
}

fn map_worktree(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredWorktree> {
    Ok(StoredWorktree {
        worktree_id: parse_id(row.get(0)?, 0)?,
        task_id: parse_id(row.get(1)?, 1)?,
        repo_root: PathBuf::from(row.get::<_, String>(2)?),
        worktree_path: PathBuf::from(row.get::<_, String>(3)?),
        branch_name: row.get(4)?,
        base_revision: row.get(5)?,
        state: row.get(6)?,
        created_at: parse_datetime(row.get(7)?, 7)?,
        cleanup_approved_at: row
            .get::<_, Option<String>>(8)?
            .map(|value| parse_datetime(value, 8))
            .transpose()?,
        archived_at: row
            .get::<_, Option<String>>(9)?
            .map(|value| parse_datetime(value, 9))
            .transpose()?,
    })
}

fn map_handover(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredHandover> {
    Ok(StoredHandover {
        checkpoint_id: parse_id(row.get(0)?, 0)?,
        reason: row.get(1)?,
        bundle: parse_json(row.get(2)?, 2)?,
        acknowledgement: row
            .get::<_, Option<String>>(3)?
            .map(|value| parse_json(value, 3))
            .transpose()?,
        started_at: parse_datetime(row.get(4)?, 4)?,
        completed_at: row
            .get::<_, Option<String>>(5)?
            .map(|value| parse_datetime(value, 5))
            .transpose()?,
    })
}

fn map_routing_audit(row: &rusqlite::Row<'_>) -> rusqlite::Result<RoutingAuditRecord> {
    Ok(RoutingAuditRecord {
        decision_id: row.get(0)?,
        task_id: parse_id(row.get(1)?, 1)?,
        schema_version: row.get(2)?,
        selected_provider: row
            .get::<_, Option<String>>(3)?
            .map(|value| parse_json_string(value, 3))
            .transpose()?,
        model_profile: row.get(4)?,
        effort: row.get(5)?,
        difficulty: row.get(6)?,
        risks: parse_json(row.get(7)?, 7)?,
        candidates: parse_json(row.get(8)?, 8)?,
        policy: parse_json(row.get(9)?, 9)?,
        downgraded: row.get(10)?,
        rationale: parse_json(row.get(11)?, 11)?,
        decided_at: parse_datetime(row.get(12)?, 12)?,
    })
}

fn map_control(row: &rusqlite::Row<'_>) -> rusqlite::Result<ControlRequest> {
    Ok(ControlRequest {
        control_id: parse_id(row.get(0)?, 0)?,
        task_id: parse_id(row.get(1)?, 1)?,
        action: parse_json_string(row.get(2)?, 2)?,
        payload: parse_json(row.get(3)?, 3)?,
        requested_by: row.get(4)?,
        requested_at: parse_datetime(row.get(5)?, 5)?,
        claimed_at: row
            .get::<_, Option<String>>(6)?
            .map(|value| parse_datetime(value, 6))
            .transpose()?,
        completed_at: row
            .get::<_, Option<String>>(7)?
            .map(|value| parse_datetime(value, 7))
            .transpose()?,
        outcome: row.get(8)?,
    })
}

fn serde_string(value: &impl Serialize) -> StateResult<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| StateError::InvalidRecord("expected string enum".to_owned()))
}

#[allow(clippy::needless_pass_by_value)]
fn parse_id<T: FromStr>(value: String, column: usize) -> rusqlite::Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

#[allow(clippy::needless_pass_by_value)]
fn parse_json<T: for<'de> Deserialize<'de>>(value: String, column: usize) -> rusqlite::Result<T> {
    serde_json::from_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn ensure_supported_schema(contract: &str, version: &str, supported: &[&str]) -> StateResult<()> {
    let version = SchemaVersion::new(version);
    if version.is_supported_by(supported) {
        Ok(())
    } else {
        Err(StateError::InvalidRecord(format!(
            "unsupported {contract} schema version {version}"
        )))
    }
}

fn invalid_sql_record(column: usize, message: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn parse_json_string<T: for<'de> Deserialize<'de>>(
    value: String,
    column: usize,
) -> rusqlite::Result<T> {
    parse_json(format!("\"{value}\""), column)
}

#[allow(clippy::needless_pass_by_value)]
fn parse_datetime(value: String, column: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

fn validate_pause_projection_event(event: &TaskEvent, paused: bool) -> StateResult<()> {
    if event.event_type != EventType::StateTransitioned
        || event
            .payload
            .get("paused")
            .and_then(serde_json::Value::as_bool)
            != Some(paused)
    {
        return Err(StateError::InvalidRecord(format!(
            "pause projection event must be state_transitioned with paused={paused}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};
    use orchestrator_domain::{
        AttemptId, Checkpoint, CheckpointId, CorrelationId, EventActor, EventId, EventType,
        HandoverAcknowledgement, HandoverBundle, HandoverId, ProviderId, QuotaPeriod, QuotaScope,
        RepoPath, SchemaVersion, TaskEnvelope, TaskEvent, TaskId, TaskState, TransitionGuards,
        UsageSnapshot, UsageUnit, WorkerOutcome, WorkerResult,
    };
    use rusqlite::params;
    use serde_json::json;
    use sha2::{Digest as _, Sha256};
    use uuid::Uuid;

    use crate::{
        ArtifactStore, ClaimedControlRecoveryPolicy, ControlAction, ControlRecoveryDisposition,
        Database, NewTaskRecord, RoutingAuditRecord, TaskListFilter,
    };

    #[test]
    #[allow(clippy::too_many_lines)]
    fn state_readers_reject_future_domain_contracts() -> Result<(), Box<dyn std::error::Error>> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        let now = Utc::now();
        let task = TaskEnvelope::new("schema reader", "redacted", now);
        database.create_task_envelope(&task)?;
        assert!(database.load_task_envelope(task.task_id)?.is_some());

        let usage = UsageSnapshot::unknown(
            ProviderId::Codex,
            QuotaScope::new("monthly", QuotaPeriod::CalendarMonth, UsageUnit::Credits),
            now,
        );
        database.record_usage_snapshot(Some(task.task_id), &usage)?;
        assert_eq!(database.list_usage_snapshots(None, 10)?.len(), 1);

        let routing = RoutingAuditRecord {
            decision_id: "schema-decision".to_owned(),
            task_id: task.task_id,
            schema_version: SchemaVersion::V1.to_owned(),
            selected_provider: Some(ProviderId::Codex),
            model_profile: Some("standard".to_owned()),
            effort: None,
            difficulty: "simple".to_owned(),
            risks: json!([]),
            candidates: json!([]),
            policy: json!({"name": "test"}),
            downgraded: false,
            rationale: json!(["fixture"]),
            decided_at: now,
        };
        database.record_routing_audit(&routing)?;
        assert_eq!(database.list_routing_audits(task.task_id, 10)?.len(), 1);

        let attempt_id = AttemptId::new();
        let worker_result = WorkerResult {
            schema_version: SchemaVersion::v1(),
            task_id: task.task_id,
            attempt_id,
            provider: ProviderId::Codex,
            outcome: WorkerOutcome::Succeeded,
            exit_code: Some(0),
            session_id: None,
            summary: None,
            commands: Vec::new(),
            tests: Vec::new(),
            started_at: now,
            finished_at: now,
            output_truncated: false,
        };
        database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO task_attempts(attempt_id, task_id, ordinal, provider_id, \
                 worker_mode, started_at, ended_at, outcome, worker_result_json) \
                 VALUES (?1, ?2, 1, 'codex', 'read_only', ?3, ?3, 'succeeded', ?4)",
                params![
                    attempt_id.to_string(),
                    task.task_id.to_string(),
                    now.to_rfc3339(),
                    serde_json::to_string(&worker_result)?,
                ],
            )?;
            Ok(())
        })?;
        let attempts = database.list_task_attempts(task.task_id)?;
        assert_eq!(attempts.len(), 1);
        assert!(attempts[0].decoded_worker_result()?.is_some());

        let mut future_task = serde_json::to_value(&task)?;
        future_task["schema_version"] = json!("999");
        let mut future_usage = usage.clone();
        future_usage.schema_version = SchemaVersion::new("999");
        let mut future_worker_result = worker_result;
        future_worker_result.schema_version = SchemaVersion::new("999");
        database.with_connection(|connection| {
            connection.execute(
                "UPDATE tasks SET schema_version = '999', task_envelope_json = ?1 \
                 WHERE task_id = ?2",
                params![future_task.to_string(), task.task_id.to_string()],
            )?;
            connection.execute(
                "UPDATE provider_usage_snapshots SET snapshot_json = ?1",
                [serde_json::to_string(&future_usage)?],
            )?;
            connection.execute(
                "UPDATE routing_decisions SET schema_version = '999' \
                 WHERE decision_id = ?1",
                [&routing.decision_id],
            )?;
            connection.execute(
                "UPDATE task_attempts SET worker_result_json = ?1 WHERE attempt_id = ?2",
                params![
                    serde_json::to_string(&future_worker_result)?,
                    attempt_id.to_string(),
                ],
            )?;
            Ok(())
        })?;

        assert!(database.load_task(task.task_id).is_err());
        assert!(database.load_task_envelope(task.task_id).is_err());
        assert!(database.list_usage_snapshots(None, 10).is_err());
        assert!(database.list_routing_audits(task.task_id, 10).is_err());
        assert!(database.list_task_attempts(task.task_id).is_err());
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn task_usage_and_control_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        let task_id = TaskId::new();
        database.create_task(&NewTaskRecord {
            task_id,
            schema_version: "1".to_owned(),
            state: TaskState::Queued,
            objective: "implement safely".to_owned(),
            original_request_redacted: "request".to_owned(),
            envelope: json!({"schema_version": "1"}),
            created_at: Utc::now(),
        })?;
        assert_eq!(
            database.load_task(task_id)?.map(|task| task.state),
            Some(TaskState::Queued)
        );
        let transition_event = TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            task_id: Some(task_id),
            occurred_at: Utc::now(),
            event_type: EventType::AssessmentCompleted,
            from_state: Some(TaskState::Queued),
            to_state: Some(TaskState::Analyzing),
            reason: None,
            actor: EventActor::Orchestrator,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: json!({}),
            previous_hash: None,
            event_hash: String::new(),
        };
        let (_, sealed_event) = database.transition_task_with_event(
            task_id,
            0,
            TaskState::Queued,
            TaskState::Analyzing,
            None,
            false,
            &TransitionGuards::default(),
            Utc::now(),
            transition_event,
        )?;
        assert_eq!(sealed_event.sequence, 1);
        assert_eq!(
            database.load_task(task_id)?.map(|task| task.state),
            Some(TaskState::Analyzing)
        );
        assert_eq!(database.list_tasks(&TaskListFilter::default())?.len(), 1);

        let usage = UsageSnapshot::unknown(
            ProviderId::Codex,
            QuotaScope::new("monthly", QuotaPeriod::CalendarMonth, UsageUnit::Credits),
            Utc::now(),
        );
        database.record_usage_snapshot(Some(task_id), &usage)?;
        assert_eq!(
            database
                .list_usage_snapshots(Some(ProviderId::Codex), 10)?
                .len(),
            1
        );

        let control = database.request_control(
            task_id,
            ControlAction::Pause,
            json!({}),
            "operator",
            Utc::now(),
        )?;
        assert_eq!(database.pending_controls(task_id)?.len(), 1);
        assert!(database.claim_control(control.control_id, Utc::now())?);
        assert!(!database.claim_control(control.control_id, Utc::now())?);
        database.complete_control(control.control_id, "checkpointed", Utc::now())?;
        assert!(database.pending_controls(task_id)?.is_empty());

        let checkpoint_id = CheckpointId::new();
        let checkpoint_attempt_id = AttemptId::new();
        database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO task_attempts(attempt_id, task_id, ordinal, provider_id, \
                 worker_mode, started_at) VALUES (?1, ?2, 1, 'codex', 'workspace_write', ?3)",
                params![
                    checkpoint_attempt_id.to_string(),
                    task_id.to_string(),
                    Utc::now().to_rfc3339(),
                ],
            )?;
            Ok(())
        })?;
        let checkpoint = Checkpoint {
            schema_version: SchemaVersion::v1(),
            checkpoint_id,
            task_id,
            attempt_id: checkpoint_attempt_id,
            objective: "implement safely".to_owned(),
            current_plan: Vec::new(),
            completed_steps: Vec::new(),
            pending_steps: Vec::new(),
            files_read: Vec::new(),
            files_changed: Vec::new(),
            git_base: None,
            diff_path: None,
            commands_run: Vec::new(),
            tests: Vec::new(),
            decisions: Vec::new(),
            unresolved_questions: Vec::new(),
            known_failures: Vec::new(),
            worker_claim: None,
            current_worker: ProviderId::Codex,
            concise_context_summary: "continue".to_owned(),
            created_at: Utc::now(),
            integrity_hash: String::new(),
        }
        .seal()?;
        database.record_checkpoint(&checkpoint)?;
        let bundle = HandoverBundle {
            schema_version: SchemaVersion::v1(),
            handover_id: HandoverId::new(),
            task_id,
            objective: "implement safely".to_owned(),
            original_request: "redacted".to_owned(),
            constraints: Vec::new(),
            acceptance_criteria: Vec::new(),
            current_plan: Vec::new(),
            completed_steps: Vec::new(),
            pending_steps: Vec::new(),
            files_read: Vec::new(),
            files_changed: Vec::new(),
            git_base: None,
            diff_path: None,
            commands_run: Vec::new(),
            tests: Vec::new(),
            decisions: Vec::new(),
            unresolved_questions: Vec::new(),
            known_failures: Vec::new(),
            current_worker: ProviderId::Codex,
            recommended_next_worker: ProviderId::Claude,
            usage_snapshots: Vec::new(),
            concise_context_summary: "continue".to_owned(),
            created_at: Utc::now(),
            integrity_hash: String::new(),
        }
        .seal()?;
        let mut future_bundle = bundle.clone();
        future_bundle.handover_id = HandoverId::new();
        future_bundle.schema_version = SchemaVersion::new("999");
        future_bundle.refresh_integrity_hash()?;
        assert!(
            database
                .record_handover(checkpoint_id, "future schema", &future_bundle, None)
                .is_err()
        );
        database.record_handover(checkpoint_id, "quota threshold", &bundle, None)?;
        let acknowledgement = HandoverAcknowledgement {
            schema_version: SchemaVersion::v1(),
            task_id,
            bundle_hash: bundle.integrity_hash.clone(),
            provider: ProviderId::Claude,
            understood_objective: bundle.objective.clone(),
            understood_constraints: Vec::new(),
            understood_acceptance_criteria: Vec::new(),
            next_step_id: None,
            unresolved_questions: Vec::new(),
            can_resume: true,
            acknowledged_at: Utc::now(),
        };
        database.complete_handover(bundle.handover_id, &acknowledgement)?;
        let stored_handover = database
            .latest_handover(task_id)?
            .ok_or("latest handover missing")?;
        assert_eq!(stored_handover.bundle.handover_id, bundle.handover_id);
        assert_eq!(
            stored_handover.acknowledgement,
            Some(acknowledgement.clone())
        );
        assert!(
            database
                .complete_handover(bundle.handover_id, &acknowledgement)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn pause_and_resume_projection_is_atomic_with_events() -> Result<(), Box<dyn std::error::Error>>
    {
        let database = migrated_database()?;
        let task_id = create_task_in_state(&database, TaskState::Planned)?;
        let paused_at = Utc::now();
        let (_, pause_event) = database.pause_task_with_event(
            task_id,
            0,
            paused_at,
            projection_event(task_id, TaskState::Planned, TaskState::Blocked, true),
        )?;
        let paused = database.load_task(task_id)?.ok_or("paused task missing")?;
        assert_eq!(pause_event.sequence, 1);
        assert_eq!(paused.state, TaskState::Blocked);
        assert_eq!(paused.resume_state, Some(TaskState::Planned));
        assert!(paused.paused);
        assert_eq!(paused.revision, 1);

        let (_, resume_event) = database.resume_task_with_event(
            task_id,
            1,
            Utc::now(),
            projection_event(task_id, TaskState::Blocked, TaskState::Planned, false),
        )?;
        let resumed = database.load_task(task_id)?.ok_or("resumed task missing")?;
        assert_eq!(resume_event.sequence, 2);
        assert_eq!(resumed.state, TaskState::Planned);
        assert_eq!(resumed.resume_state, None);
        assert!(!resumed.paused);
        assert_eq!(resumed.revision, 2);
        assert_eq!(database.outbox_after(0, 10)?.len(), 2);
        Ok(())
    }

    #[test]
    fn running_task_cannot_be_paused_before_safe_checkpoint()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = migrated_database()?;
        let task_id = create_task_in_state(&database, TaskState::Running)?;
        let result = database.pause_task_with_event(
            task_id,
            0,
            Utc::now(),
            projection_event(task_id, TaskState::Running, TaskState::Blocked, true),
        );
        assert!(result.is_err());
        let task = database.load_task(task_id)?.ok_or("task missing")?;
        assert_eq!(task.state, TaskState::Running);
        assert!(!task.paused);
        assert!(database.outbox_after(0, 10)?.is_empty());
        Ok(())
    }

    #[test]
    fn restart_requeues_only_stale_replay_safe_controls() -> Result<(), Box<dyn std::error::Error>>
    {
        let database = migrated_database()?;
        let task_id = create_task_in_state(&database, TaskState::Queued)?;
        let requested_at = Utc::now() - TimeDelta::minutes(20);
        let stale_claim = requested_at + TimeDelta::minutes(1);
        let pause = database.request_control(
            task_id,
            ControlAction::Pause,
            json!({}),
            "operator",
            requested_at,
        )?;
        let handover = database.request_control(
            task_id,
            ControlAction::Handover,
            json!({"to": "claude"}),
            "operator",
            requested_at,
        )?;
        let fresh = database.request_control(
            task_id,
            ControlAction::Cancel,
            json!({}),
            "operator",
            Utc::now(),
        )?;
        assert!(database.claim_control(pause.control_id, stale_claim)?);
        assert!(database.claim_control(handover.control_id, stale_claim)?);
        assert!(database.claim_control(fresh.control_id, Utc::now())?);

        let recovered = database.recover_claimed_controls(
            task_id,
            Utc::now(),
            ClaimedControlRecoveryPolicy {
                stale_after: TimeDelta::minutes(5),
            },
        )?;
        assert_eq!(recovered.len(), 3);
        assert_eq!(
            disposition_for(&recovered, pause.control_id),
            ControlRecoveryDisposition::Requeued
        );
        assert_eq!(
            disposition_for(&recovered, handover.control_id),
            ControlRecoveryDisposition::ManualReconciliationRequired
        );
        assert_eq!(
            disposition_for(&recovered, fresh.control_id),
            ControlRecoveryDisposition::StillClaimed
        );
        assert_eq!(database.pending_controls(task_id)?.len(), 1);
        assert_eq!(database.claimed_incomplete_controls(task_id)?.len(), 2);
        Ok(())
    }

    #[test]
    fn recovery_queries_return_attempt_worktree_and_sealed_checkpoint()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = migrated_database()?;
        let task_id = create_task_in_state(&database, TaskState::Checkpointed)?;
        let first_attempt = AttemptId::new();
        let latest_attempt = AttemptId::new();
        let now = Utc::now();
        database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO task_attempts(attempt_id, task_id, ordinal, provider_id, \
                 worker_mode, started_at, ended_at, outcome, worker_result_json) \
                 VALUES (?1, ?2, 1, 'gemini', 'read_only', ?3, ?3, 'succeeded', '{}')",
                params![
                    first_attempt.to_string(),
                    task_id.to_string(),
                    now.to_rfc3339()
                ],
            )?;
            connection.execute(
                "INSERT INTO task_attempts(attempt_id, task_id, ordinal, provider_id, \
                 worker_mode, started_at) VALUES (?1, ?2, 2, 'codex', 'workspace_write', ?3)",
                params![
                    latest_attempt.to_string(),
                    task_id.to_string(),
                    now.to_rfc3339()
                ],
            )?;
            connection.execute(
                "INSERT INTO worktrees(worktree_id, task_id, repo_root, worktree_path, \
                 branch_name, base_revision, state, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'base-sha', 'active', ?6)",
                params![
                    Uuid::now_v7().to_string(),
                    task_id.to_string(),
                    "C:/repo",
                    "C:/repo/.worktrees/task",
                    format!("orchestrator/{task_id}"),
                    now.to_rfc3339(),
                ],
            )?;
            Ok(())
        })?;
        let checkpoint = Checkpoint {
            schema_version: SchemaVersion::v1(),
            checkpoint_id: CheckpointId::new(),
            task_id,
            attempt_id: latest_attempt,
            objective: "recover safely".to_owned(),
            current_plan: Vec::new(),
            completed_steps: Vec::new(),
            pending_steps: Vec::new(),
            files_read: Vec::new(),
            files_changed: Vec::new(),
            git_base: Some("base-sha".to_owned()),
            diff_path: None,
            commands_run: Vec::new(),
            tests: Vec::new(),
            decisions: Vec::new(),
            unresolved_questions: Vec::new(),
            known_failures: Vec::new(),
            worker_claim: None,
            current_worker: ProviderId::Codex,
            concise_context_summary: "resume from the sealed checkpoint".to_owned(),
            created_at: now,
            integrity_hash: String::new(),
        }
        .seal()?;
        let mut future_checkpoint = checkpoint.clone();
        future_checkpoint.checkpoint_id = CheckpointId::new();
        future_checkpoint.schema_version = SchemaVersion::new("999");
        future_checkpoint.refresh_integrity_hash()?;
        assert!(database.record_checkpoint(&future_checkpoint).is_err());
        database.record_checkpoint(&checkpoint)?;

        let attempts = database.list_task_attempts(task_id)?;
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].attempt_id, first_attempt);
        assert_eq!(
            database
                .latest_task_attempt(task_id)?
                .map(|value| value.attempt_id),
            Some(latest_attempt)
        );
        let worktree = database
            .active_worktree(task_id)?
            .ok_or("active worktree missing")?;
        assert_eq!(
            worktree.worktree_path,
            std::path::Path::new("C:/repo/.worktrees/task")
        );
        assert_eq!(
            database
                .latest_sealed_checkpoint(task_id)?
                .map(|value| value.checkpoint_id),
            Some(checkpoint.checkpoint_id)
        );
        Ok(())
    }

    #[test]
    fn checkpoint_diff_is_registered_and_missing_file_is_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, database, task_id, checkpoint) = file_checkpoint_fixture()?;
        let (linked_artifacts, stored_digest): (i64, String) =
            database.with_connection(|connection| {
                let metadata = connection.query_row(
                    "SELECT count(*), artifacts.sha256 FROM checkpoints \
                     JOIN artifacts ON artifacts.artifact_id = checkpoints.diff_artifact_id \
                     WHERE checkpoints.checkpoint_id = ?1",
                    [checkpoint.checkpoint_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                Ok(metadata)
            })?;
        assert_eq!(linked_artifacts, 1);
        let diff_path = checkpoint.diff_path.as_ref().ok_or("diff path missing")?;
        assert!(diff_path.to_string().contains(&stored_digest));
        assert!(database.latest_sealed_checkpoint(task_id)?.is_some());

        std::fs::remove_file(diff_path.join_to(directory.path()))?;
        assert!(database.latest_sealed_checkpoint(task_id).is_err());
        Ok(())
    }

    #[test]
    fn tampered_checkpoint_diff_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let (directory, database, task_id, checkpoint) = file_checkpoint_fixture()?;
        let diff_path = checkpoint.diff_path.as_ref().ok_or("diff path missing")?;
        std::fs::write(diff_path.join_to(directory.path()), b"tampered diff")?;
        assert!(database.latest_sealed_checkpoint(task_id).is_err());
        assert!(database.load_checkpoint(checkpoint.checkpoint_id).is_err());
        Ok(())
    }

    fn file_checkpoint_fixture()
    -> Result<(crate::CanonicalTempDir, Database, TaskId, Checkpoint), Box<dyn std::error::Error>>
    {
        let directory = crate::CanonicalTempDir::new("tempdir")?;
        let database = Database::open(directory.path().join("orchestrator.db"))?;
        database.migrate_with_backup(&directory.path().join("backups"))?;
        let task_id = create_task_in_state(&database, TaskState::Checkpointed)?;
        let attempt_id = AttemptId::new();
        let created_at = Utc::now();
        database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO task_attempts(attempt_id, task_id, ordinal, provider_id, \
                 worker_mode, started_at) VALUES (?1, ?2, 1, 'codex', 'workspace_write', ?3)",
                params![
                    attempt_id.to_string(),
                    task_id.to_string(),
                    created_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })?;
        let checkpoint_id = CheckpointId::new();
        let diff = b"diff --git a/src/lib.rs b/src/lib.rs\n+verified\n";
        let digest = hex::encode(Sha256::digest(diff));
        let diff_path = RepoPath::try_from(format!(
            "checkpoints/{checkpoint_id}/worktree.{digest}.diff"
        ))?;
        ArtifactStore::open(directory.path())?.put(diff_path.clone(), diff)?;
        let checkpoint = Checkpoint {
            schema_version: SchemaVersion::v1(),
            checkpoint_id,
            task_id,
            attempt_id,
            objective: "persist diff evidence".to_owned(),
            current_plan: Vec::new(),
            completed_steps: Vec::new(),
            pending_steps: Vec::new(),
            files_read: Vec::new(),
            files_changed: vec![RepoPath::try_from("src/lib.rs")?],
            git_base: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            diff_path: Some(diff_path),
            commands_run: Vec::new(),
            tests: Vec::new(),
            decisions: Vec::new(),
            unresolved_questions: Vec::new(),
            known_failures: Vec::new(),
            worker_claim: None,
            current_worker: ProviderId::Codex,
            concise_context_summary: "sealed diff evidence".to_owned(),
            created_at,
            integrity_hash: String::new(),
        }
        .seal()?;
        database.record_checkpoint(&checkpoint)?;
        Ok((directory, database, task_id, checkpoint))
    }

    fn migrated_database() -> Result<Database, Box<dyn std::error::Error>> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        Ok(database)
    }

    fn create_task_in_state(
        database: &Database,
        state: TaskState,
    ) -> Result<TaskId, Box<dyn std::error::Error>> {
        let task_id = TaskId::new();
        database.create_task(&NewTaskRecord {
            task_id,
            schema_version: "1".to_owned(),
            state,
            objective: "restart safely".to_owned(),
            original_request_redacted: "request".to_owned(),
            envelope: json!({"schema_version": "1"}),
            created_at: Utc::now(),
        })?;
        Ok(task_id)
    }

    fn projection_event(
        task_id: TaskId,
        from: TaskState,
        to: TaskState,
        paused: bool,
    ) -> TaskEvent {
        TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            task_id: Some(task_id),
            occurred_at: Utc::now(),
            event_type: EventType::StateTransitioned,
            from_state: Some(from),
            to_state: Some(to),
            reason: Some(if paused { "pause" } else { "resume" }.to_owned()),
            actor: EventActor::Orchestrator,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: json!({"paused": paused}),
            previous_hash: None,
            event_hash: String::new(),
        }
    }

    fn disposition_for(
        recovered: &[super::RecoveredControl],
        control_id: Uuid,
    ) -> ControlRecoveryDisposition {
        recovered
            .iter()
            .find(|value| value.request.control_id == control_id)
            .map_or(ControlRecoveryDisposition::StillClaimed, |value| {
                value.disposition
            })
    }
}
