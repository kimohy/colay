use std::str::FromStr;

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    ConversationMessage, EventType, MessageId, MessageState, SessionId, SessionState, TaskEvent,
    TaskId,
};
use rusqlite::{OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{Database, StateError, StateResult, database::append_event_in_transaction};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewSessionRecord {
    pub session_id: SessionId,
    pub schema_version: String,
    pub title: String,
    pub state: SessionState,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSession {
    pub session_id: SessionId,
    pub schema_version: String,
    pub revision: u64,
    pub title: String,
    pub state: SessionState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionListFilter {
    pub include_archived: bool,
    pub limit: usize,
}

impl Default for SessionListFilter {
    fn default() -> Self {
        Self {
            include_archived: false,
            limit: 100,
        }
    }
}

impl Database {
    pub fn create_session_with_event(
        &self,
        session: &NewSessionRecord,
        mut event: TaskEvent,
    ) -> StateResult<StoredSession> {
        validate_new_session(session)?;
        validate_session_event(session.session_id, EventType::SessionCreated, &event)?;
        self.with_transaction(|transaction| {
            transaction.execute(
                "INSERT INTO sessions(
                    session_id, schema_version, revision, title, state, created_at, updated_at,
                    archived_at
                 ) VALUES (?1, ?2, 0, ?3, ?4, ?5, ?5, NULL)",
                params![
                    session.session_id.to_string(),
                    session.schema_version,
                    session.title.trim(),
                    enum_text(&session.state)?,
                    session.created_at.to_rfc3339(),
                ],
            )?;
            append_event_in_transaction(transaction, &mut event)?;
            session_by_id(transaction, session.session_id)?
                .ok_or_else(|| StateError::InvalidRecord("created session is missing".to_owned()))
        })
    }

    pub fn load_session(&self, session_id: SessionId) -> StateResult<Option<StoredSession>> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT session_id, schema_version, revision, title, state, created_at,
                     updated_at, archived_at FROM sessions WHERE session_id = ?1",
                    [session_id.to_string()],
                    map_session,
                )
                .optional()
                .map_err(StateError::from)
        })
    }

    pub fn list_sessions(&self, filter: &SessionListFilter) -> StateResult<Vec<StoredSession>> {
        let limit = i64::try_from(filter.limit).unwrap_or(i64::MAX);
        self.with_connection(|connection| {
            let sql = if filter.include_archived {
                "SELECT session_id, schema_version, revision, title, state, created_at,
                 updated_at, archived_at FROM sessions
                 ORDER BY updated_at DESC, session_id DESC LIMIT ?1"
            } else {
                "SELECT session_id, schema_version, revision, title, state, created_at,
                 updated_at, archived_at FROM sessions WHERE archived_at IS NULL
                 ORDER BY updated_at DESC, session_id DESC LIMIT ?1"
            };
            let mut statement = connection.prepare(sql)?;
            statement
                .query_map([limit], map_session)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(StateError::from)
        })
    }

    pub fn transition_session_with_event(
        &self,
        session_id: SessionId,
        expected_revision: u64,
        next: SessionState,
        updated_at: DateTime<Utc>,
        mut event: TaskEvent,
    ) -> StateResult<StoredSession> {
        validate_session_event(session_id, EventType::SessionStateTransitioned, &event)?;
        self.with_transaction(|transaction| {
            let current = session_by_id(transaction, session_id)?.ok_or_else(|| {
                StateError::InvalidRecord(format!("session {session_id} does not exist"))
            })?;
            if current.revision != expected_revision {
                return Err(StateError::OptimisticConflict {
                    entity: format!("session {session_id}"),
                });
            }
            current
                .state
                .validate_transition(next)
                .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
            let changed = transaction.execute(
                "UPDATE sessions SET state = ?1, revision = revision + 1, updated_at = ?2
                 WHERE session_id = ?3 AND revision = ?4",
                params![
                    enum_text(&next)?,
                    updated_at.to_rfc3339(),
                    session_id.to_string(),
                    expected_revision,
                ],
            )?;
            if changed != 1 {
                return Err(StateError::OptimisticConflict {
                    entity: format!("session {session_id}"),
                });
            }
            append_event_in_transaction(transaction, &mut event)?;
            session_by_id(transaction, session_id)?.ok_or_else(|| {
                StateError::InvalidRecord("transitioned session is missing".to_owned())
            })
        })
    }

    pub fn append_message(&self, message: &ConversationMessage) -> StateResult<u64> {
        message
            .validate()
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        self.with_transaction(|transaction| {
            let next: i64 = transaction.query_row(
                "SELECT coalesce(max(ordinal), 0) + 1 FROM conversation_messages
                 WHERE session_id = ?1",
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
                    message.task_id.map(|id| id.to_string()),
                    next,
                    enum_text(&message.role)?,
                    enum_text(&message.kind)?,
                    enum_text(&message.state)?,
                    message.content_redacted,
                    message.created_at.to_rfc3339(),
                    message.finalized_at.map(|value| value.to_rfc3339()),
                ],
            )?;
            u64::try_from(next)
                .map_err(|_| StateError::InvalidRecord("message ordinal is negative".to_owned()))
        })
    }

    pub fn finalize_message(
        &self,
        session_id: SessionId,
        message_id: MessageId,
        state: MessageState,
        content_redacted: &str,
        finalized_at: DateTime<Utc>,
    ) -> StateResult<ConversationMessage> {
        if state == MessageState::Streaming {
            return Err(StateError::InvalidRecord(
                "message final state cannot be streaming".to_owned(),
            ));
        }
        self.with_transaction(|transaction| {
            let (_, mut message) =
                message_by_id(transaction, session_id, message_id)?.ok_or_else(|| {
                    StateError::InvalidRecord(format!(
                        "message {message_id} does not belong to session {session_id}"
                    ))
                })?;
            if message.state != MessageState::Streaming {
                return Err(StateError::OptimisticConflict {
                    entity: format!("message {message_id}"),
                });
            }
            message.state = state;
            content_redacted.clone_into(&mut message.content_redacted);
            message.finalized_at = Some(finalized_at);
            message
                .validate()
                .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
            let changed = transaction.execute(
                "UPDATE conversation_messages SET state = ?1, content_redacted = ?2,
                 finalized_at = ?3 WHERE session_id = ?4 AND message_id = ?5
                 AND state = 'streaming'",
                params![
                    enum_text(&state)?,
                    content_redacted,
                    finalized_at.to_rfc3339(),
                    session_id.to_string(),
                    message_id.to_string(),
                ],
            )?;
            if changed != 1 {
                return Err(StateError::OptimisticConflict {
                    entity: format!("message {message_id}"),
                });
            }
            Ok(message)
        })
    }

    pub fn messages_after(
        &self,
        session_id: SessionId,
        ordinal: u64,
        limit: usize,
    ) -> StateResult<Vec<(u64, ConversationMessage)>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT ordinal, message_id, session_id, task_id, role, kind, state,
                 content_redacted, created_at, finalized_at FROM conversation_messages
                 WHERE session_id = ?1 AND ordinal > ?2 ORDER BY ordinal LIMIT ?3",
            )?;
            statement
                .query_map(params![session_id.to_string(), ordinal, limit], map_message)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(StateError::from)
        })
    }
}

fn validate_new_session(session: &NewSessionRecord) -> StateResult<()> {
    if session.schema_version != orchestrator_domain::SchemaVersion::V1 {
        return Err(StateError::InvalidRecord(format!(
            "unsupported session schema version {}",
            session.schema_version
        )));
    }
    if session.title.trim().is_empty() {
        return Err(StateError::InvalidRecord(
            "session title must not be blank".to_owned(),
        ));
    }
    if session.state != SessionState::Drafting {
        return Err(StateError::InvalidRecord(
            "new session must start in drafting state".to_owned(),
        ));
    }
    Ok(())
}

fn validate_session_event(
    session_id: SessionId,
    expected_type: EventType,
    event: &TaskEvent,
) -> StateResult<()> {
    if event.session_id != Some(session_id) || event.task_id.is_some() {
        return Err(StateError::InvalidRecord(
            "session event target does not match its session projection".to_owned(),
        ));
    }
    if event.event_type != expected_type {
        return Err(StateError::InvalidRecord(format!(
            "session event type {:?} does not match expected {expected_type:?}",
            event.event_type
        )));
    }
    Ok(())
}

fn session_by_id(
    transaction: &Transaction<'_>,
    session_id: SessionId,
) -> StateResult<Option<StoredSession>> {
    transaction
        .query_row(
            "SELECT session_id, schema_version, revision, title, state, created_at,
             updated_at, archived_at FROM sessions WHERE session_id = ?1",
            [session_id.to_string()],
            map_session,
        )
        .optional()
        .map_err(StateError::from)
}

fn map_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSession> {
    Ok(StoredSession {
        session_id: parse_id(&row.get::<_, String>(0)?, 0)?,
        schema_version: row.get(1)?,
        revision: row.get(2)?,
        title: row.get(3)?,
        state: parse_enum(&row.get::<_, String>(4)?, 4)?,
        created_at: parse_datetime(&row.get::<_, String>(5)?, 5)?,
        updated_at: parse_datetime(&row.get::<_, String>(6)?, 6)?,
        archived_at: row
            .get::<_, Option<String>>(7)?
            .map(|value| parse_datetime(&value, 7))
            .transpose()?,
    })
}

fn message_by_id(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    message_id: MessageId,
) -> StateResult<Option<(u64, ConversationMessage)>> {
    transaction
        .query_row(
            "SELECT ordinal, message_id, session_id, task_id, role, kind, state,
             content_redacted, created_at, finalized_at FROM conversation_messages
             WHERE session_id = ?1 AND message_id = ?2",
            params![session_id.to_string(), message_id.to_string()],
            map_message,
        )
        .optional()
        .map_err(StateError::from)
}

fn map_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<(u64, ConversationMessage)> {
    Ok((
        row.get(0)?,
        ConversationMessage {
            message_id: parse_id(&row.get::<_, String>(1)?, 1)?,
            session_id: parse_id(&row.get::<_, String>(2)?, 2)?,
            task_id: row
                .get::<_, Option<String>>(3)?
                .map(|value| parse_id::<TaskId>(&value, 3))
                .transpose()?,
            role: parse_enum(&row.get::<_, String>(4)?, 4)?,
            kind: parse_enum(&row.get::<_, String>(5)?, 5)?,
            state: parse_enum(&row.get::<_, String>(6)?, 6)?,
            content_redacted: row.get(7)?,
            created_at: parse_datetime(&row.get::<_, String>(8)?, 8)?,
            finalized_at: row
                .get::<_, Option<String>>(9)?
                .map(|value| parse_datetime(&value, 9))
                .transpose()?,
        },
    ))
}

fn enum_text(value: &impl Serialize) -> StateResult<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| StateError::InvalidRecord("enum did not serialize as a string".to_owned()))
}

fn parse_enum<T: DeserializeOwned>(value: &str, column: usize) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|error| conversion_error(column, error))
}

fn parse_id<T>(value: &str, column: usize) -> rusqlite::Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    T::from_str(value).map_err(|error| conversion_error(column, error))
}

fn parse_datetime(value: &str, column: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|date_time| date_time.with_timezone(&Utc))
        .map_err(|error| conversion_error(column, error))
}

fn conversion_error(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use orchestrator_domain::{
        ConversationMessage, CorrelationId, EventActor, EventId, EventType, MessageId, MessageKind,
        MessageRole, MessageState, SchemaVersion, SessionId, SessionState, TaskEvent, TaskId,
    };
    use rusqlite::params;

    use super::{NewSessionRecord, SessionListFilter};
    use crate::{Database, StateError, StateResult};

    fn timestamp() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 21, 10, 0, 0)
            .single()
            .unwrap_or_else(|| panic!("fixed timestamp must be valid"))
    }

    fn migrated_database() -> Database {
        let database =
            Database::open_in_memory().unwrap_or_else(|error| panic!("database: {error}"));
        database
            .migrate_with_backup(std::path::Path::new("unused"))
            .unwrap_or_else(|error| panic!("migrations: {error}"));
        database
    }

    fn new_session(title: &str) -> NewSessionRecord {
        NewSessionRecord {
            session_id: SessionId::new(),
            schema_version: SchemaVersion::V1.to_owned(),
            title: title.to_owned(),
            state: SessionState::Drafting,
            created_at: timestamp(),
        }
    }

    fn session_event(session_id: SessionId, event_type: EventType) -> TaskEvent {
        TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            session_id: Some(session_id),
            task_id: None,
            occurred_at: timestamp(),
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

    fn seed_task(database: &Database, task_id: TaskId) -> StateResult<()> {
        let now = timestamp().to_rfc3339();
        database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO tasks(
                    task_id, schema_version, revision, state, resume_state, paused, objective,
                    original_request_redacted, task_envelope_json, created_at, updated_at,
                    archived_at
                 ) VALUES (?1, '1', 0, 'queued', NULL, 0, 'message target',
                    'message target', '{}', ?2, ?2, NULL)",
                params![task_id.to_string(), now],
            )?;
            Ok(())
        })
    }

    #[test]
    fn session_creation_and_transition_are_atomic_with_events() -> StateResult<()> {
        let database = migrated_database();
        let new = new_session("auth refactor");
        let created = database.create_session_with_event(
            &new,
            session_event(new.session_id, EventType::SessionCreated),
        )?;
        assert_eq!(created.revision, 0);
        assert_eq!(created.state, SessionState::Drafting);
        assert_eq!(
            database.event_at(1)?.and_then(|event| event.session_id),
            Some(new.session_id)
        );

        let transitioned = database.transition_session_with_event(
            new.session_id,
            0,
            SessionState::Planning,
            timestamp(),
            session_event(new.session_id, EventType::SessionStateTransitioned),
        )?;
        assert_eq!(transitioned.revision, 1);
        assert_eq!(transitioned.state, SessionState::Planning);
        assert_eq!(
            database.event_at(2)?.and_then(|event| event.session_id),
            Some(new.session_id)
        );

        let conflict = database.transition_session_with_event(
            new.session_id,
            0,
            SessionState::AwaitingApproval,
            timestamp(),
            session_event(new.session_id, EventType::SessionStateTransitioned),
        );
        assert!(matches!(
            conflict,
            Err(StateError::OptimisticConflict { .. })
        ));
        assert_eq!(database.event_at(3)?, None);
        Ok(())
    }

    #[test]
    fn invalid_transition_and_mismatched_event_do_not_mutate_session() -> StateResult<()> {
        let database = migrated_database();
        let new = new_session("safe session");
        let mismatched = database.create_session_with_event(
            &new,
            session_event(SessionId::new(), EventType::SessionCreated),
        );
        assert!(matches!(mismatched, Err(StateError::InvalidRecord(_))));
        assert!(database.load_session(new.session_id)?.is_none());

        database.create_session_with_event(
            &new,
            session_event(new.session_id, EventType::SessionCreated),
        )?;
        let invalid = database.transition_session_with_event(
            new.session_id,
            0,
            SessionState::Completed,
            timestamp(),
            session_event(new.session_id, EventType::SessionStateTransitioned),
        );
        assert!(matches!(invalid, Err(StateError::InvalidRecord(_))));
        let stored = database
            .load_session(new.session_id)?
            .ok_or_else(|| StateError::InvalidRecord("created session is missing".to_owned()))?;
        assert_eq!(stored.state, SessionState::Drafting);
        assert_eq!(stored.revision, 0);
        Ok(())
    }

    #[test]
    fn session_listing_is_newest_first_and_can_exclude_archived() -> StateResult<()> {
        let database = migrated_database();
        let first = new_session("first");
        let mut second = new_session("second");
        second.created_at = timestamp() + chrono::TimeDelta::seconds(1);
        for session in [&first, &second] {
            database.create_session_with_event(
                session,
                session_event(session.session_id, EventType::SessionCreated),
            )?;
        }
        let sessions = database.list_sessions(&SessionListFilter {
            include_archived: false,
            limit: 10,
        })?;
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.title.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"]
        );
        Ok(())
    }

    #[test]
    fn messages_keep_per_session_order_and_task_target() -> StateResult<()> {
        let database = migrated_database();
        let session = new_session("messages");
        database.create_session_with_event(
            &session,
            session_event(session.session_id, EventType::SessionCreated),
        )?;
        let task_id = TaskId::new();
        seed_task(&database, task_id)?;
        let first = ConversationMessage {
            message_id: MessageId::new(),
            session_id: session.session_id,
            task_id: None,
            role: MessageRole::User,
            kind: MessageKind::UserMessage,
            state: MessageState::Final,
            content_redacted: "refactor auth".to_owned(),
            created_at: timestamp(),
            finalized_at: Some(timestamp()),
        };
        let second = ConversationMessage {
            message_id: MessageId::new(),
            session_id: session.session_id,
            task_id: Some(task_id),
            role: MessageRole::Agent,
            kind: MessageKind::AgentMessage,
            state: MessageState::Streaming,
            content_redacted: String::new(),
            created_at: timestamp(),
            finalized_at: None,
        };
        assert_eq!(database.append_message(&first)?, 1);
        assert_eq!(database.append_message(&second)?, 2);

        let messages = database.messages_after(session.session_id, 0, 10)?;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], (1, first));
        assert_eq!(messages[1], (2, second));
        Ok(())
    }

    #[test]
    fn streaming_message_finalizes_once_and_only_in_its_session() -> StateResult<()> {
        let database = migrated_database();
        let first = new_session("first");
        let second = new_session("second");
        for session in [&first, &second] {
            database.create_session_with_event(
                session,
                session_event(session.session_id, EventType::SessionCreated),
            )?;
        }
        let message = ConversationMessage {
            message_id: MessageId::new(),
            session_id: first.session_id,
            task_id: None,
            role: MessageRole::Agent,
            kind: MessageKind::AgentMessage,
            state: MessageState::Streaming,
            content_redacted: String::new(),
            created_at: timestamp(),
            finalized_at: None,
        };
        database.append_message(&message)?;
        assert!(
            database
                .finalize_message(
                    second.session_id,
                    message.message_id,
                    MessageState::Final,
                    "done",
                    timestamp(),
                )
                .is_err()
        );
        let finalized = database.finalize_message(
            first.session_id,
            message.message_id,
            MessageState::Final,
            "done",
            timestamp(),
        )?;
        assert_eq!(finalized.state, MessageState::Final);
        assert_eq!(finalized.content_redacted, "done");
        assert!(
            database
                .finalize_message(
                    first.session_id,
                    message.message_id,
                    MessageState::Interrupted,
                    "done",
                    timestamp(),
                )
                .is_err()
        );
        Ok(())
    }
}
