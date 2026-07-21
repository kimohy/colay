use chrono::{DateTime, Utc};
use orchestrator_domain::{
    InstructionId, MessageId, SessionId, TaskId, TaskInstructionState, TaskState,
};
use rusqlite::{OptionalExtension as _, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{Database, StateError, StateResult};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTaskInstruction {
    pub instruction_id: InstructionId,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub message_id: MessageId,
    pub ordinal: u64,
    pub state: TaskInstructionState,
    pub content_redacted: String,
    pub queued_at: DateTime<Utc>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub outcome_redacted: Option<String>,
}

impl Database {
    pub fn list_task_instructions(
        &self,
        task_id: TaskId,
    ) -> StateResult<Vec<StoredTaskInstruction>> {
        self.with_connection(|connection| {
            let mut statement =
                connection.prepare(&instruction_select("WHERE task_id = ?1 ORDER BY ordinal"))?;
            let rows = statement.query_map([task_id.to_string()], map_instruction)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StateError::from)
        })
    }

    pub fn instruction_for_message(
        &self,
        message_id: MessageId,
    ) -> StateResult<Option<StoredTaskInstruction>> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    &instruction_select("WHERE message_id = ?1"),
                    [message_id.to_string()],
                    map_instruction,
                )
                .optional()
                .map_err(StateError::from)
        })
    }

    pub fn claim_next_task_instruction(
        &self,
        task_id: TaskId,
        claimed_at: DateTime<Utc>,
    ) -> StateResult<Option<StoredTaskInstruction>> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let instruction_id: Option<String> = transaction
            .query_row(
                "SELECT instruction_id FROM task_instructions
                 WHERE task_id = ?1 AND state IN ('queued', 'interrupted')
                 ORDER BY ordinal LIMIT 1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(instruction_id) = instruction_id else {
            transaction.commit()?;
            return Ok(None);
        };
        let changed = transaction.execute(
            "UPDATE task_instructions SET state = 'applying', claimed_at = ?1,
                completed_at = NULL, outcome_redacted = NULL
             WHERE instruction_id = ?2 AND state IN ('queued', 'interrupted')",
            params![claimed_at.to_rfc3339(), instruction_id],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("task instruction {instruction_id}"),
            });
        }
        let instruction = transaction.query_row(
            &instruction_select("WHERE instruction_id = ?1"),
            [instruction_id],
            map_instruction,
        )?;
        transaction.commit()?;
        Ok(Some(instruction))
    }

    pub fn finish_task_instruction(
        &self,
        instruction_id: InstructionId,
        state: TaskInstructionState,
        completed_at: DateTime<Utc>,
        outcome_redacted: Option<&str>,
    ) -> StateResult<StoredTaskInstruction> {
        if !matches!(
            state,
            TaskInstructionState::Applied
                | TaskInstructionState::Rejected
                | TaskInstructionState::Interrupted
        ) {
            return Err(StateError::InvalidRecord(
                "applying instruction must finish as applied, rejected, or interrupted".to_owned(),
            ));
        }
        let outcome = outcome_redacted
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE task_instructions SET state = ?1, completed_at = ?2, outcome_redacted = ?3
             WHERE instruction_id = ?4 AND state = 'applying'",
            params![
                enum_text(&state)?,
                completed_at.to_rfc3339(),
                outcome,
                instruction_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("applying task instruction {instruction_id}"),
            });
        }
        let instruction = transaction.query_row(
            &instruction_select("WHERE instruction_id = ?1"),
            [instruction_id.to_string()],
            map_instruction,
        )?;
        transaction.commit()?;
        Ok(instruction)
    }

    pub fn interrupt_applying_instructions(
        &self,
        interrupted_at: DateTime<Utc>,
        outcome_redacted: &str,
    ) -> StateResult<usize> {
        let outcome = outcome_redacted.trim();
        if outcome.is_empty() {
            return Err(StateError::InvalidRecord(
                "instruction interruption outcome must not be blank".to_owned(),
            ));
        }
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE task_instructions SET state = 'interrupted', completed_at = ?1,
                        outcome_redacted = ?2 WHERE state = 'applying'",
                    params![interrupted_at.to_rfc3339(), outcome],
                )
                .map_err(StateError::from)
        })
    }
}

pub(crate) fn queue_instruction_in_transaction(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    task_id: TaskId,
    message_id: MessageId,
    content_redacted: &str,
    queued_at: DateTime<Utc>,
) -> StateResult<StoredTaskInstruction> {
    let state: Option<String> = transaction
        .query_row(
            "SELECT t.state FROM tasks t
             JOIN session_tasks st ON st.task_id = t.task_id AND st.session_id = ?1
             JOIN session_graph_heads gh ON gh.session_id = st.session_id
                                        AND gh.revision_id = st.revision_id
             JOIN graph_revisions gr ON gr.revision_id = st.revision_id AND gr.status = 'approved'
             WHERE t.task_id = ?2",
            params![session_id.to_string(), task_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let state = state.ok_or_else(|| {
        StateError::InvalidRecord(
            "task message target must belong to the current approved session graph".to_owned(),
        )
    })?;
    let task_state: TaskState = parse_enum("task state", &state)?;
    if task_state.is_terminal() {
        return Err(StateError::InvalidRecord(format!(
            "terminal task {task_id} cannot accept instructions"
        )));
    }
    let ordinal: i64 = transaction.query_row(
        "SELECT coalesce(max(ordinal), 0) + 1 FROM task_instructions WHERE task_id = ?1",
        [task_id.to_string()],
        |row| row.get(0),
    )?;
    let instruction_id = InstructionId::new();
    transaction.execute(
        "INSERT INTO task_instructions(instruction_id, session_id, task_id, message_id,
            ordinal, state, content_redacted, queued_at, claimed_at, completed_at,
            outcome_redacted)
         VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, NULL, NULL, NULL)",
        params![
            instruction_id.to_string(),
            session_id.to_string(),
            task_id.to_string(),
            message_id.to_string(),
            ordinal,
            content_redacted,
            queued_at.to_rfc3339(),
        ],
    )?;
    Ok(StoredTaskInstruction {
        instruction_id,
        session_id,
        task_id,
        message_id,
        ordinal: u64::try_from(ordinal)
            .map_err(|_| StateError::InvalidRecord("negative instruction ordinal".to_owned()))?,
        state: TaskInstructionState::Queued,
        content_redacted: content_redacted.to_owned(),
        queued_at,
        claimed_at: None,
        completed_at: None,
        outcome_redacted: None,
    })
}

fn instruction_select(suffix: &str) -> String {
    format!(
        "SELECT instruction_id, session_id, task_id, message_id, ordinal, state,
                content_redacted, queued_at, claimed_at, completed_at, outcome_redacted
         FROM task_instructions {suffix}"
    )
}

fn map_instruction(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredTaskInstruction> {
    let instruction_id = row.get::<_, String>(0)?;
    let session_id = row.get::<_, String>(1)?;
    let task_id = row.get::<_, String>(2)?;
    let message_id = row.get::<_, String>(3)?;
    let ordinal = row.get::<_, i64>(4)?;
    let state = row.get::<_, String>(5)?;
    let queued_at = row.get::<_, String>(7)?;
    let claimed_at = row.get::<_, Option<String>>(8)?;
    let completed_at = row.get::<_, Option<String>>(9)?;
    Ok(StoredTaskInstruction {
        instruction_id: parse_id_row("instruction id", &instruction_id, 0)?,
        session_id: parse_id_row("session id", &session_id, 1)?,
        task_id: parse_id_row("task id", &task_id, 2)?,
        message_id: parse_id_row("message id", &message_id, 3)?,
        ordinal: u64::try_from(ordinal).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        state: parse_enum_row("instruction state", &state, 5)?,
        content_redacted: row.get(6)?,
        queued_at: parse_time_row(&queued_at, 7)?,
        claimed_at: claimed_at
            .map(|value| parse_time_row(&value, 8))
            .transpose()?,
        completed_at: completed_at
            .map(|value| parse_time_row(&value, 9))
            .transpose()?,
        outcome_redacted: row.get(10)?,
    })
}

fn enum_text(value: &impl Serialize) -> StateResult<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| StateError::InvalidRecord("expected enum string".to_owned()))
}

fn parse_enum<T: for<'de> Deserialize<'de>>(label: &str, value: &str) -> StateResult<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|error| StateError::InvalidRecord(format!("invalid {label}: {error}")))
}

fn parse_id_row<T: std::str::FromStr>(
    label: &str,
    value: &str,
    column: usize,
) -> rusqlite::Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    T::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(StateError::InvalidRecord(format!(
                "invalid {label}: {error}"
            ))),
        )
    })
}

fn parse_enum_row<T: for<'de> Deserialize<'de>>(
    label: &str,
    value: &str,
    column: usize,
) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(StateError::InvalidRecord(format!(
                "invalid {label}: {error}"
            ))),
        )
    })
}

fn parse_time_row(value: &str, column: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone as _, Utc};
    use orchestrator_domain::{
        ConversationMessage, CorrelationId, EventActor, EventId, EventType, GraphRevisionId,
        MessageId, MessageKind, MessageRole, MessageState, SchemaVersion, SessionId, TaskEvent,
        TaskId, TaskInstructionState,
    };
    use rusqlite::params;

    use crate::{Database, StateResult};

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 21, 14, 0, 0)
            .single()
            .unwrap_or_default()
    }

    fn database() -> StateResult<Database> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        Ok(database)
    }

    fn seed_target(database: &Database) -> StateResult<(SessionId, TaskId)> {
        let session_id = SessionId::new();
        let goal_message_id = MessageId::new();
        let revision_id = GraphRevisionId::new();
        let task_id = TaskId::new();
        database.with_transaction(|transaction| {
            transaction.execute(
                "INSERT INTO sessions(session_id, schema_version, title, state, created_at, updated_at)
                 VALUES (?1, 'v1', 'test', 'running', ?2, ?2)",
                params![session_id.to_string(), now().to_rfc3339()],
            )?;
            transaction.execute(
                "INSERT INTO conversation_messages(message_id, session_id, ordinal, role, kind,
                    state, content_redacted, created_at, finalized_at)
                 VALUES (?1, ?2, 1, 'user', 'user_message', 'final', 'goal', ?3, ?3)",
                params![goal_message_id.to_string(), session_id.to_string(), now().to_rfc3339()],
            )?;
            transaction.execute(
                "INSERT INTO graph_revisions(revision_id, session_id, goal_message_id, ordinal,
                    status, proposal_hash, validation_json, planner_provider, created_at, completed_at)
                 VALUES (?1, ?2, ?3, 1, 'approved', ?4, '{}', 'codex', ?5, ?5)",
                params![
                    revision_id.to_string(),
                    session_id.to_string(),
                    goal_message_id.to_string(),
                    "0".repeat(64),
                    now().to_rfc3339(),
                ],
            )?;
            transaction.execute(
                "INSERT INTO session_graph_heads(session_id, revision_id, updated_at)
                 VALUES (?1, ?2, ?3)",
                params![session_id.to_string(), revision_id.to_string(), now().to_rfc3339()],
            )?;
            transaction.execute(
                "INSERT INTO tasks(task_id, schema_version, state, objective,
                    original_request_redacted, task_envelope_json, created_at, updated_at)
                 VALUES (?1, 'v1', 'queued', 'task', 'goal', '{}', ?2, ?2)",
                params![task_id.to_string(), now().to_rfc3339()],
            )?;
            transaction.execute(
                "INSERT INTO session_tasks(session_id, revision_id, task_id, node_key,
                    display_order, provider_id, model_profile)
                 VALUES (?1, ?2, ?3, 'task', 1, 'codex', 'standard')",
                params![session_id.to_string(), revision_id.to_string(), task_id.to_string()],
            )?;
            Ok(())
        })?;
        Ok((session_id, task_id))
    }

    fn message(
        session_id: SessionId,
        task_id: TaskId,
        content: &str,
        offset: i64,
    ) -> ConversationMessage {
        let created_at = now() + TimeDelta::seconds(offset);
        ConversationMessage {
            message_id: MessageId::new(),
            session_id,
            task_id: Some(task_id),
            role: MessageRole::User,
            kind: MessageKind::UserMessage,
            state: MessageState::Final,
            content_redacted: content.to_owned(),
            created_at,
            finalized_at: Some(created_at),
        }
    }

    fn event(message: &ConversationMessage) -> TaskEvent {
        TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            session_id: Some(message.session_id),
            task_id: message.task_id,
            occurred_at: message.created_at,
            event_type: EventType::MessageAppended,
            from_state: None,
            to_state: None,
            reason: None,
            actor: EventActor::User,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: serde_json::json!({"message_id": message.message_id}),
            previous_hash: None,
            event_hash: String::new(),
        }
    }

    #[test]
    fn targeted_messages_queue_ordered_recoverable_instructions_atomically()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let (session_id, task_id) = seed_target(&database)?;
        let first = message(session_id, task_id, "first", 1);
        let second = message(session_id, task_id, "second", 2);
        let (_, first_instruction) =
            database.append_message_with_event_and_instruction(&first, event(&first))?;
        database.append_message_with_event_and_instruction(&second, event(&second))?;
        let queued = database.list_task_instructions(task_id)?;
        assert_eq!(
            queued.iter().map(|value| value.ordinal).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(queued[0].content_redacted, "first");

        let applying = database
            .claim_next_task_instruction(task_id, now() + TimeDelta::seconds(3))?
            .ok_or("first instruction missing")?;
        assert_eq!(
            Some(applying.instruction_id),
            first_instruction.map(|value| value.instruction_id)
        );
        database.finish_task_instruction(
            applying.instruction_id,
            TaskInstructionState::Interrupted,
            now() + TimeDelta::seconds(4),
            Some("worker stopped"),
        )?;
        let replay = database
            .claim_next_task_instruction(task_id, now() + TimeDelta::seconds(5))?
            .ok_or("interrupted instruction missing")?;
        assert_eq!(replay.instruction_id, applying.instruction_id);
        database.finish_task_instruction(
            replay.instruction_id,
            TaskInstructionState::Applied,
            now() + TimeDelta::seconds(6),
            Some("accepted"),
        )?;
        let next = database
            .claim_next_task_instruction(task_id, now() + TimeDelta::seconds(7))?
            .ok_or("second instruction missing")?;
        assert_eq!(next.ordinal, 2);
        Ok(())
    }

    #[test]
    fn terminal_task_rejection_rolls_back_message_and_instruction()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let (session_id, task_id) = seed_target(&database)?;
        database.with_connection(|connection| {
            connection.execute(
                "UPDATE tasks SET state = 'completed' WHERE task_id = ?1",
                [task_id.to_string()],
            )?;
            Ok(())
        })?;
        let rejected = message(session_id, task_id, "too late", 1);
        assert!(
            database
                .append_message_with_event_and_instruction(&rejected, event(&rejected))
                .is_err()
        );
        assert!(database.load_message(rejected.message_id)?.is_none());
        assert!(database.list_task_instructions(task_id)?.is_empty());
        Ok(())
    }
}
