use std::str::FromStr;

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    ConversationAttemptId, ConversationOutcome, MessageId, ProviderId, RequirementRevision,
    RequirementRevisionId, RequirementSnapshot, SchemaVersion, SessionId,
};
use rusqlite::{OptionalExtension as _, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{Database, StateError, StateResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewConversationAttempt {
    pub attempt_id: ConversationAttemptId,
    pub session_id: SessionId,
    pub source_message_id: MessageId,
    pub provider: ProviderId,
    pub started_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationAttemptStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredConversationAttempt {
    pub attempt_id: ConversationAttemptId,
    pub session_id: SessionId,
    pub source_message_id: MessageId,
    pub provider: ProviderId,
    pub status: ConversationAttemptStatus,
    pub outcome: Option<ConversationOutcome>,
    pub error_redacted: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl Database {
    pub fn begin_conversation_attempt(
        &self,
        attempt: &NewConversationAttempt,
    ) -> StateResult<StoredConversationAttempt> {
        self.with_transaction(|transaction| {
            if let Some(existing) = load_attempt(transaction, attempt.attempt_id)? {
                if existing.session_id == attempt.session_id
                    && existing.source_message_id == attempt.source_message_id
                    && existing.provider == attempt.provider
                    && existing.started_at == attempt.started_at
                {
                    return Ok(existing);
                }
                return Err(StateError::InvalidRecord(
                    "conversation attempt identity conflicts with existing row".to_owned(),
                ));
            }
            transaction.execute(
                "INSERT INTO conversation_attempts(
                    attempt_id, session_id, source_message_id, provider_id, status, started_at)
                 VALUES (?1, ?2, ?3, ?4, 'running', ?5)",
                params![
                    attempt.attempt_id.to_string(),
                    attempt.session_id.to_string(),
                    attempt.source_message_id.to_string(),
                    provider_text(attempt.provider),
                    attempt.started_at.to_rfc3339(),
                ],
            )?;
            load_attempt(transaction, attempt.attempt_id)?.ok_or_else(|| {
                StateError::InvalidRecord("created conversation attempt is missing".to_owned())
            })
        })
    }

    pub fn load_conversation_attempt(
        &self,
        attempt_id: ConversationAttemptId,
    ) -> StateResult<Option<StoredConversationAttempt>> {
        self.with_connection(|connection| load_attempt(connection, attempt_id))
    }

    pub fn finish_conversation_attempt(
        &self,
        attempt_id: ConversationAttemptId,
        outcome: &ConversationOutcome,
        completed_at: DateTime<Utc>,
    ) -> StateResult<StoredConversationAttempt> {
        outcome
            .validate()
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        let outcome_json = serde_json::to_string(outcome)?;
        self.with_transaction(|transaction| {
            let existing = load_attempt(transaction, attempt_id)?.ok_or_else(|| {
                StateError::InvalidRecord("conversation attempt does not exist".to_owned())
            })?;
            if existing.status == ConversationAttemptStatus::Succeeded {
                if existing.outcome.as_ref() == Some(outcome) {
                    return Ok(existing);
                }
                return Err(StateError::InvalidRecord(
                    "completed conversation attempt outcome conflicts".to_owned(),
                ));
            }
            if existing.status != ConversationAttemptStatus::Running {
                return Err(StateError::InvalidRecord(
                    "conversation attempt is not running".to_owned(),
                ));
            }
            let changed = transaction.execute(
                "UPDATE conversation_attempts
                 SET status = 'succeeded', outcome_json = ?1, completed_at = ?2
                 WHERE attempt_id = ?3 AND status = 'running'",
                params![
                    outcome_json,
                    completed_at.to_rfc3339(),
                    attempt_id.to_string()
                ],
            )?;
            if changed != 1 {
                return Err(StateError::OptimisticConflict {
                    entity: format!("conversation attempt {attempt_id}"),
                });
            }
            load_attempt(transaction, attempt_id)?.ok_or_else(|| {
                StateError::InvalidRecord("completed conversation attempt is missing".to_owned())
            })
        })
    }

    pub fn reconcile_interrupted_conversation_attempts(
        &self,
        completed_at: DateTime<Utc>,
        error_redacted: &str,
    ) -> StateResult<Vec<ConversationAttemptId>> {
        if error_redacted.trim().is_empty() {
            return Err(StateError::InvalidRecord(
                "interrupted conversation evidence must not be blank".to_owned(),
            ));
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt_ids = {
            let mut statement = transaction.prepare(
                "SELECT attempt_id FROM conversation_attempts
                 WHERE status = 'running' ORDER BY started_at, attempt_id",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|value| parse_id("attempt_id", &value))
                .collect::<StateResult<Vec<ConversationAttemptId>>>()?
        };
        for attempt_id in &attempt_ids {
            let changed = transaction.execute(
                "UPDATE conversation_attempts
                 SET status = 'failed', error_redacted = ?1, completed_at = ?2
                 WHERE attempt_id = ?3 AND status = 'running'",
                params![
                    error_redacted.trim(),
                    completed_at.to_rfc3339(),
                    attempt_id.to_string(),
                ],
            )?;
            if changed != 1 {
                return Err(StateError::OptimisticConflict {
                    entity: format!("conversation attempt {attempt_id}"),
                });
            }
            transaction.execute(
                "UPDATE client_commands
                 SET state = 'failed', completed_at = ?1, outcome = ?2
                 WHERE command_id = ?3 AND action = 'request_conversation_turn'
                 AND state IN ('pending', 'claimed')",
                params![
                    completed_at.to_rfc3339(),
                    error_redacted.trim(),
                    attempt_id.to_string(),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(attempt_ids)
    }

    pub fn record_requirement_revision(
        &self,
        revision: &RequirementRevision,
    ) -> StateResult<RequirementRevision> {
        let expected_hash = orchestrator_domain::canonical_sha256(&revision.snapshot)
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        if expected_hash != revision.snapshot_hash || revision.ordinal == 0 {
            return Err(StateError::InvalidRecord(
                "requirement revision seal or ordinal is invalid".to_owned(),
            ));
        }
        let snapshot_json = serde_json::to_string(&revision.snapshot)?;
        self.with_transaction(|transaction| {
            if let Some(existing) = load_requirement(transaction, revision.requirement_revision_id)? {
                if &existing == revision {
                    return Ok(existing);
                }
                return Err(StateError::InvalidRecord(
                    "requirement revision conflicts with existing immutable row".to_owned(),
                ));
            }
            transaction.execute(
                "INSERT INTO requirement_revisions(
                    requirement_revision_id, session_id, source_message_id, ordinal,
                    schema_version, snapshot_hash, snapshot_json, complete, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    revision.requirement_revision_id.to_string(),
                    revision.session_id.to_string(),
                    revision.source_message_id.to_string(),
                    i64::try_from(revision.ordinal).map_err(|_| StateError::InvalidRecord(
                        "requirement ordinal exceeds SQLite range".to_owned()
                    ))?,
                    revision.schema_version.to_string(),
                    revision.snapshot_hash,
                    snapshot_json,
                    i64::from(revision.snapshot.is_complete()),
                    revision.created_at.to_rfc3339(),
                ],
            )?;
            transaction.execute(
                "INSERT INTO session_requirement_heads(session_id, requirement_revision_id, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET
                    requirement_revision_id = excluded.requirement_revision_id,
                    updated_at = excluded.updated_at",
                params![
                    revision.session_id.to_string(),
                    revision.requirement_revision_id.to_string(),
                    revision.created_at.to_rfc3339(),
                ],
            )?;
            load_requirement(transaction, revision.requirement_revision_id)?.ok_or_else(|| {
                StateError::InvalidRecord("stored requirement revision is missing".to_owned())
            })
        })
    }

    pub fn current_requirement_revision(
        &self,
        session_id: SessionId,
    ) -> StateResult<Option<RequirementRevision>> {
        self.with_connection(|connection| {
            let id: Option<String> = connection
                .query_row(
                    "SELECT requirement_revision_id FROM session_requirement_heads
                     WHERE session_id = ?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            id.map(|value| parse_id::<RequirementRevisionId>("requirement_revision_id", &value))
                .transpose()?
                .map(|id| load_requirement(connection, id))
                .transpose()
                .map(Option::flatten)
        })
    }
}

fn load_attempt(
    connection: &rusqlite::Connection,
    attempt_id: ConversationAttemptId,
) -> StateResult<Option<StoredConversationAttempt>> {
    connection
        .query_row(
            "SELECT attempt_id, session_id, source_message_id, provider_id, status,
                    outcome_json, error_redacted, started_at, completed_at
             FROM conversation_attempts WHERE attempt_id = ?1",
            [attempt_id.to_string()],
            map_attempt,
        )
        .optional()
        .map_err(StateError::from)
}

fn map_attempt(row: &Row<'_>) -> rusqlite::Result<StoredConversationAttempt> {
    let outcome_json: Option<String> = row.get(5)?;
    Ok(StoredConversationAttempt {
        attempt_id: parse_row_id(row, 0, "attempt_id")?,
        session_id: parse_row_id(row, 1, "session_id")?,
        source_message_id: parse_row_id(row, 2, "source_message_id")?,
        provider: parse_provider(&row.get::<_, String>(3)?)?,
        status: parse_status(&row.get::<_, String>(4)?)?,
        outcome: outcome_json
            .map(|value| serde_json::from_str(&value).map_err(to_sql_error))
            .transpose()?,
        error_redacted: row.get(6)?,
        started_at: parse_timestamp(&row.get::<_, String>(7)?)?,
        completed_at: row
            .get::<_, Option<String>>(8)?
            .map(|value| parse_timestamp(&value))
            .transpose()?,
    })
}

fn load_requirement(
    connection: &rusqlite::Connection,
    revision_id: RequirementRevisionId,
) -> StateResult<Option<RequirementRevision>> {
    connection
        .query_row(
            "SELECT requirement_revision_id, session_id, source_message_id, ordinal,
                    schema_version, snapshot_hash, snapshot_json, created_at
             FROM requirement_revisions WHERE requirement_revision_id = ?1",
            [revision_id.to_string()],
            |row| {
                let ordinal: i64 = row.get(3)?;
                let snapshot_json: String = row.get(6)?;
                Ok(RequirementRevision {
                    requirement_revision_id: parse_row_id(row, 0, "requirement_revision_id")?,
                    session_id: parse_row_id(row, 1, "session_id")?,
                    source_message_id: parse_row_id(row, 2, "source_message_id")?,
                    ordinal: u64::try_from(ordinal).map_err(to_sql_error)?,
                    schema_version: SchemaVersion::new(row.get::<_, String>(4)?),
                    snapshot_hash: row.get(5)?,
                    snapshot: serde_json::from_str::<RequirementSnapshot>(&snapshot_json)
                        .map_err(to_sql_error)?,
                    created_at: parse_timestamp(&row.get::<_, String>(7)?)?,
                })
            },
        )
        .optional()
        .map_err(StateError::from)
}

fn provider_text(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Gemini => "gemini",
        ProviderId::Codex => "codex",
        ProviderId::Claude => "claude",
        ProviderId::Agy => "agy",
    }
}

fn parse_provider(value: &str) -> rusqlite::Result<ProviderId> {
    ProviderId::from_str(value).map_err(to_sql_error)
}

fn parse_status(value: &str) -> rusqlite::Result<ConversationAttemptStatus> {
    match value {
        "running" => Ok(ConversationAttemptStatus::Running),
        "succeeded" => Ok(ConversationAttemptStatus::Succeeded),
        "failed" => Ok(ConversationAttemptStatus::Failed),
        "cancelled" => Ok(ConversationAttemptStatus::Cancelled),
        other => Err(to_sql_error(format!("invalid conversation status {other}"))),
    }
}

fn parse_timestamp(value: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(to_sql_error)
}

fn parse_row_id<T: FromStr>(row: &Row<'_>, index: usize, field: &str) -> rusqlite::Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    parse_id(field, &row.get::<_, String>(index)?).map_err(|error| match error {
        StateError::InvalidRecord(message) => to_sql_error(message),
        other => to_sql_error(other),
    })
}

fn parse_id<T: FromStr>(field: &str, value: &str) -> StateResult<T>
where
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| StateError::InvalidRecord(format!("invalid {field} `{value}`: {error}")))
}

fn to_sql_error(error: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.to_string(),
        )),
    )
}
