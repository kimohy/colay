use std::str::FromStr as _;

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    ConversationMessage, ProviderId, SessionId, SessionState, TaskId, TaskState,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

use crate::{
    Database, StateError, StateResult, StoredSession, StoredTask, StoredTaskAttempt,
    StoredWorktree,
    records::{map_task, map_task_attempt, map_worktree, validate_stored_task},
    sessions::{map_message, map_session},
};

const DEFAULT_MESSAGE_LIMIT: usize = 100;
const MAX_MESSAGE_LIMIT: usize = 200;
const DEFAULT_TASK_LIMIT: usize = 50;
const MAX_TASK_LIMIT: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceReadRequest {
    pub session_id: SessionId,
    pub selected_task_id: Option<TaskId>,
    pub before_ordinal: Option<i64>,
    pub message_limit: usize,
    pub task_limit: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceTask {
    pub task: StoredTask,
    pub latest_provider: Option<ProviderId>,
    pub latest_model_profile: Option<String>,
    pub latest_effort: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceVerification {
    pub outcome: String,
    pub reviewer_provider: Option<ProviderId>,
    pub completed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInspector {
    pub task: WorkspaceTask,
    pub latest_attempt: Option<StoredTaskAttempt>,
    pub active_worktree: Option<StoredWorktree>,
    pub changed_file_count: u64,
    pub latest_verification: Option<WorkspaceVerification>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAttentionKind {
    ApprovalRequired,
    Blocked,
    Failed,
    CheckpointRequested,
    HandoverRequested,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceAttention {
    pub task_id: Option<TaskId>,
    pub kind: WorkspaceAttentionKind,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceProjection {
    pub session: StoredSession,
    /// Phase 2 shows bounded recent repository tasks. Session graph membership arrives in Phase 3.
    pub recent_tasks: Vec<WorkspaceTask>,
    pub messages: Vec<(u64, ConversationMessage)>,
    pub has_older_messages: bool,
    pub attention: Vec<WorkspaceAttention>,
    pub inspector: Option<WorkspaceInspector>,
    pub last_event_sequence: i64,
}

impl Database {
    pub fn read_workspace_projection(
        &self,
        request: WorkspaceReadRequest,
    ) -> StateResult<WorkspaceProjection> {
        let message_limit = bounded_limit(
            request.message_limit,
            DEFAULT_MESSAGE_LIMIT,
            MAX_MESSAGE_LIMIT,
        );
        let task_limit = bounded_limit(request.task_limit, DEFAULT_TASK_LIMIT, MAX_TASK_LIMIT);
        self.with_connection(|connection| {
            let session = connection
                .query_row(
                    "SELECT session_id, schema_version, revision, title, state, created_at,
                     updated_at, archived_at FROM sessions WHERE session_id = ?1",
                    [request.session_id.to_string()],
                    map_session,
                )
                .optional()?
                .ok_or_else(|| {
                    StateError::InvalidRecord(format!(
                        "session {} does not exist",
                        request.session_id
                    ))
                })?;
            let (messages, has_older_messages) = read_messages(
                connection,
                request.session_id,
                request.before_ordinal,
                message_limit,
            )?;
            let recent_tasks = read_recent_tasks(connection, task_limit)?;
            let inspector = request
                .selected_task_id
                .map(|task_id| read_inspector(connection, task_id, &recent_tasks))
                .transpose()?
                .flatten();
            let attention = derive_attention(&session, &recent_tasks);
            let last_event_sequence = connection.query_row(
                "SELECT coalesce(max(sequence), 0) FROM task_events",
                [],
                |row| row.get(0),
            )?;
            Ok(WorkspaceProjection {
                session,
                recent_tasks,
                messages,
                has_older_messages,
                attention,
                inspector,
                last_event_sequence,
            })
        })
    }
}

fn bounded_limit(requested: usize, default: usize, maximum: usize) -> usize {
    if requested == 0 {
        default
    } else {
        requested.min(maximum)
    }
}

fn read_messages(
    connection: &Connection,
    session_id: SessionId,
    before_ordinal: Option<i64>,
    limit: usize,
) -> StateResult<(Vec<(u64, ConversationMessage)>, bool)> {
    let boundary = before_ordinal.unwrap_or(i64::MAX).max(1);
    let fetch_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
    let mut statement = connection.prepare(
        "SELECT ordinal, message_id, session_id, task_id, role, kind, state,
         content_redacted, created_at, finalized_at FROM conversation_messages
         WHERE session_id = ?1 AND ordinal < ?2 ORDER BY ordinal DESC LIMIT ?3",
    )?;
    let mut messages = statement
        .query_map(
            params![session_id.to_string(), boundary, fetch_limit],
            map_message,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let has_older = messages.len() > limit;
    messages.truncate(limit);
    messages.reverse();
    Ok((messages, has_older))
}

const TASK_SELECT: &str =
    "SELECT task_id, schema_version, revision, state, resume_state, paused, objective,
     original_request_redacted, task_envelope_json, created_at, updated_at, archived_at,
     coalesce(
       (SELECT selected_provider FROM routing_decisions r WHERE r.task_id = tasks.task_id
        ORDER BY decided_at DESC, rowid DESC LIMIT 1),
       (SELECT provider_id FROM task_attempts a WHERE a.task_id = tasks.task_id
        ORDER BY ordinal DESC LIMIT 1)
     ),
     (SELECT model_profile FROM routing_decisions r WHERE r.task_id = tasks.task_id
      ORDER BY decided_at DESC, rowid DESC LIMIT 1),
     (SELECT effort FROM routing_decisions r WHERE r.task_id = tasks.task_id
      ORDER BY decided_at DESC, rowid DESC LIMIT 1)
     FROM tasks";

fn read_recent_tasks(connection: &Connection, limit: usize) -> StateResult<Vec<WorkspaceTask>> {
    let sql = format!(
        "{TASK_SELECT} WHERE archived_at IS NULL ORDER BY updated_at DESC, task_id DESC LIMIT ?1"
    );
    let mut statement = connection.prepare(&sql)?;
    statement
        .query_map(
            [i64::try_from(limit).unwrap_or(i64::MAX)],
            map_workspace_task,
        )?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(validate_workspace_task)
        .collect()
}

fn read_workspace_task(
    connection: &Connection,
    task_id: TaskId,
) -> StateResult<Option<WorkspaceTask>> {
    let sql = format!("{TASK_SELECT} WHERE task_id = ?1");
    connection
        .query_row(&sql, [task_id.to_string()], map_workspace_task)
        .optional()?
        .map(validate_workspace_task)
        .transpose()
}

fn map_workspace_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceTask> {
    let provider = row
        .get::<_, Option<String>>(12)?
        .map(|value| {
            ProviderId::from_str(&value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    12,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    Ok(WorkspaceTask {
        task: map_task(row)?,
        latest_provider: provider,
        latest_model_profile: row.get(13)?,
        latest_effort: row.get(14)?,
    })
}

fn validate_workspace_task(mut task: WorkspaceTask) -> StateResult<WorkspaceTask> {
    task.task = validate_stored_task(task.task)?;
    Ok(task)
}

fn read_inspector(
    connection: &Connection,
    task_id: TaskId,
    recent_tasks: &[WorkspaceTask],
) -> StateResult<Option<WorkspaceInspector>> {
    let task = if let Some(task) = recent_tasks
        .iter()
        .find(|task| task.task.task_id == task_id)
    {
        Some(task.clone())
    } else {
        read_workspace_task(connection, task_id)?
    };
    let Some(task) = task else {
        return Ok(None);
    };
    let latest_attempt = connection
        .query_row(
            "SELECT attempt_id, task_id, ordinal, provider_id, worker_mode, started_at,
             ended_at, outcome, worker_result_json FROM task_attempts WHERE task_id = ?1
             ORDER BY ordinal DESC LIMIT 1",
            [task_id.to_string()],
            map_task_attempt,
        )
        .optional()?;
    let mut worktree_statement = connection.prepare(
        "SELECT worktree_id, task_id, repo_root, worktree_path, branch_name, base_revision,
         state, created_at, cleanup_approved_at, archived_at FROM worktrees
         WHERE task_id = ?1 AND state = 'active' AND archived_at IS NULL
         ORDER BY created_at DESC LIMIT 2",
    )?;
    let worktrees = worktree_statement
        .query_map([task_id.to_string()], map_worktree)?
        .collect::<Result<Vec<_>, _>>()?;
    let active_worktree = match worktrees.as_slice() {
        [] => None,
        [worktree] => Some(worktree.clone()),
        _ => {
            return Err(StateError::InvalidRecord(format!(
                "task {task_id} has multiple active worktrees"
            )));
        }
    };
    let changed_file_count = connection.query_row(
        "SELECT count(*) FROM changed_files WHERE task_id = ?1",
        [task_id.to_string()],
        |row| row.get(0),
    )?;
    let latest_verification = connection
        .query_row(
            "SELECT outcome, reviewer_provider, completed_at FROM verification_results
             WHERE task_id = ?1 ORDER BY completed_at DESC, rowid DESC LIMIT 1",
            [task_id.to_string()],
            map_verification,
        )
        .optional()?;
    Ok(Some(WorkspaceInspector {
        task,
        latest_attempt,
        active_worktree,
        changed_file_count,
        latest_verification,
    }))
}

fn map_verification(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceVerification> {
    let reviewer_provider = row
        .get::<_, Option<String>>(1)?
        .map(|value| {
            ProviderId::from_str(&value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    let completed_at = DateTime::parse_from_rfc3339(&row.get::<_, String>(2)?)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    Ok(WorkspaceVerification {
        outcome: row.get(0)?,
        reviewer_provider,
        completed_at,
    })
}

fn derive_attention(session: &StoredSession, tasks: &[WorkspaceTask]) -> Vec<WorkspaceAttention> {
    let mut attention = Vec::new();
    if session.state == SessionState::AwaitingApproval {
        attention.push(WorkspaceAttention {
            task_id: None,
            kind: WorkspaceAttentionKind::ApprovalRequired,
            summary: "session plan requires approval".to_owned(),
        });
    }
    for task in tasks {
        let kind = match task.task.state {
            TaskState::Blocked => Some(WorkspaceAttentionKind::Blocked),
            TaskState::Failed => Some(WorkspaceAttentionKind::Failed),
            TaskState::CheckpointRequested => Some(WorkspaceAttentionKind::CheckpointRequested),
            TaskState::HandoverRequested => Some(WorkspaceAttentionKind::HandoverRequested),
            _ => None,
        };
        if let Some(kind) = kind {
            attention.push(WorkspaceAttention {
                task_id: Some(task.task.task_id),
                kind,
                summary: task.task.objective.clone(),
            });
        }
    }
    attention
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone as _, Utc};
    use orchestrator_domain::{
        AttemptId, MessageId, SchemaVersion, SessionId, TaskEnvelope, TaskId,
    };
    use rusqlite::params;
    use uuid::Uuid;

    use super::{WorkspaceAttentionKind, WorkspaceReadRequest};
    use crate::{Database, StateResult};

    fn timestamp() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0)
            .single()
            .unwrap_or_else(|| panic!("fixed timestamp must be valid"))
    }

    fn database() -> Database {
        let database = Database::open_in_memory().unwrap_or_else(|error| panic!("db: {error}"));
        database
            .migrate_with_backup(std::path::Path::new("unused"))
            .unwrap_or_else(|error| panic!("migrations: {error}"));
        database
    }

    fn seed(database: &Database) -> StateResult<(SessionId, SessionId, TaskId)> {
        let session = SessionId::new();
        let other_session = SessionId::new();
        let running = TaskId::new();
        let blocked = TaskId::new();
        let completed = TaskId::new();
        let now = timestamp();
        database.with_transaction(|transaction| {
            for (id, title) in [(session, "primary"), (other_session, "other")] {
                transaction.execute(
                    "INSERT INTO sessions(session_id, schema_version, revision, title, state,
                     created_at, updated_at, archived_at) VALUES (?1, '1', 0, ?2, 'drafting',
                     ?3, ?3, NULL)",
                    params![id.to_string(), title, now.to_rfc3339()],
                )?;
            }
            for (offset, id, objective, state) in [
                (3, running, "running task", "running"),
                (2, blocked, "blocked task", "blocked"),
                (1, completed, "completed task", "completed"),
            ] {
                let mut envelope = TaskEnvelope::new(objective, objective, now);
                envelope.task_id = id;
                let updated = (now + Duration::seconds(offset)).to_rfc3339();
                transaction.execute(
                    "INSERT INTO tasks(task_id, schema_version, revision, state, resume_state,
                     paused, objective, original_request_redacted, task_envelope_json, created_at,
                     updated_at, archived_at) VALUES (?1, ?2, 0, ?3, NULL, 0, ?4, ?4, ?5, ?6,
                     ?7, NULL)",
                    params![
                        id.to_string(),
                        SchemaVersion::V1,
                        state,
                        objective,
                        serde_json::to_string(&envelope)?,
                        now.to_rfc3339(),
                        updated,
                    ],
                )?;
            }
            for ordinal in 1..=1_000_i64 {
                let created = (now + Duration::seconds(ordinal)).to_rfc3339();
                transaction.execute(
                    "INSERT INTO conversation_messages(message_id, session_id, task_id, ordinal,
                     role, kind, state, content_redacted, created_at, finalized_at)
                     VALUES (?1, ?2, NULL, ?3, 'user', 'user_message', 'final', ?4, ?5, ?5)",
                    params![
                        MessageId::new().to_string(),
                        session.to_string(),
                        ordinal,
                        format!("message {ordinal}"),
                        created,
                    ],
                )?;
            }
            transaction.execute(
                "INSERT INTO conversation_messages(message_id, session_id, task_id, ordinal,
                 role, kind, state, content_redacted, created_at, finalized_at)
                 VALUES (?1, ?2, NULL, 1, 'user', 'user_message', 'final', 'private other', ?3, ?3)",
                params![MessageId::new().to_string(), other_session.to_string(), now.to_rfc3339()],
            )?;
            transaction.execute(
                "INSERT INTO task_attempts(attempt_id, task_id, ordinal, provider_id, worker_mode,
                 started_at, ended_at, outcome, worker_result_json)
                 VALUES (?1, ?2, 1, 'claude', 'writable', ?3, NULL, NULL, NULL)",
                params![AttemptId::new().to_string(), running.to_string(), now.to_rfc3339()],
            )?;
            transaction.execute(
                "INSERT INTO routing_decisions(decision_id, task_id, selected_provider,
                 model_profile, effort, difficulty, risk_json, candidates_json, policy_json,
                 downgraded, rationale_json, schema_version, decided_at)
                 VALUES ('route-running', ?1, 'codex', 'principal', 'high', 'hard', '{}', '[]',
                 '{}', 0, '{}', '1', ?2)",
                params![running.to_string(), now.to_rfc3339()],
            )?;
            let worktree = Uuid::now_v7();
            transaction.execute(
                "INSERT INTO worktrees(worktree_id, task_id, repo_root, worktree_path, branch_name,
                 base_revision, state, created_at, cleanup_approved_at, archived_at)
                 VALUES (?1, ?2, 'C:/repo', 'C:/repo/.worktrees/running', 'task/running', 'abc',
                 'active', ?3, NULL, NULL)",
                params![worktree.to_string(), running.to_string(), now.to_rfc3339()],
            )?;
            transaction.execute(
                "INSERT INTO changed_files(task_id, worktree_id, relative_path, owner_lease_id,
                 sha256, first_seen_at, last_seen_at) VALUES (?1, ?2, 'src/lib.rs', NULL, NULL,
                 ?3, ?3)",
                params![running.to_string(), worktree.to_string(), now.to_rfc3339()],
            )?;
            transaction.execute(
                "INSERT INTO verification_results(verification_id, task_id, attempt_id,
                 reviewer_provider, outcome, schema_version, result_json, started_at, completed_at)
                 VALUES (?1, ?2, NULL, 'gemini', 'passed', '1', '{}', ?3, ?3)",
                params![Uuid::now_v7().to_string(), running.to_string(), now.to_rfc3339()],
            )?;
            Ok(())
        })?;
        Ok((session, other_session, running))
    }

    #[test]
    fn projection_is_session_isolated_bounded_and_maps_inspector() {
        let database = database();
        let (session_id, _, running) =
            seed(&database).unwrap_or_else(|error| panic!("seed: {error}"));
        let projection = database
            .read_workspace_projection(WorkspaceReadRequest {
                session_id,
                selected_task_id: Some(running),
                before_ordinal: None,
                message_limit: 25,
                task_limit: 2,
            })
            .unwrap_or_else(|error| panic!("projection: {error}"));

        assert_eq!(projection.messages.len(), 25);
        assert_eq!(
            projection.messages.first().map(|message| message.0),
            Some(976)
        );
        assert_eq!(
            projection.messages.last().map(|message| message.0),
            Some(1_000)
        );
        assert!(
            projection
                .messages
                .iter()
                .all(|(_, message)| message.session_id == session_id)
        );
        assert!(projection.has_older_messages);
        assert_eq!(projection.recent_tasks.len(), 2);
        assert_eq!(
            projection.recent_tasks[0]
                .latest_provider
                .map(orchestrator_domain::ProviderId::as_str),
            Some("codex")
        );
        assert_eq!(
            projection.recent_tasks[0].latest_model_profile.as_deref(),
            Some("principal")
        );
        let inspector = projection
            .inspector
            .as_ref()
            .unwrap_or_else(|| panic!("selected inspector"));
        assert_eq!(inspector.task.task.task_id, running);
        assert_eq!(inspector.changed_file_count, 1);
        assert!(inspector.active_worktree.is_some());
        assert_eq!(
            inspector
                .latest_verification
                .as_ref()
                .map(|value| value.outcome.as_str()),
            Some("passed")
        );
        assert!(
            projection
                .attention
                .iter()
                .any(|item| item.kind == WorkspaceAttentionKind::Blocked)
        );
    }

    #[test]
    fn before_ordinal_pages_backwards_and_limits_are_clamped() {
        let database = database();
        let (session_id, _, _) = seed(&database).unwrap_or_else(|error| panic!("seed: {error}"));
        let page = database
            .read_workspace_projection(WorkspaceReadRequest {
                session_id,
                selected_task_id: None,
                before_ordinal: Some(976),
                message_limit: 25,
                task_limit: usize::MAX,
            })
            .unwrap_or_else(|error| panic!("page: {error}"));
        assert_eq!(page.messages.first().map(|message| message.0), Some(951));
        assert_eq!(page.messages.last().map(|message| message.0), Some(975));
        assert!(page.recent_tasks.len() <= 100);

        let clamped = database
            .read_workspace_projection(WorkspaceReadRequest {
                session_id,
                selected_task_id: None,
                before_ordinal: None,
                message_limit: usize::MAX,
                task_limit: 1,
            })
            .unwrap_or_else(|error| panic!("clamped: {error}"));
        assert_eq!(clamped.messages.len(), 200);
        assert_eq!(clamped.recent_tasks.len(), 1);
    }
}
