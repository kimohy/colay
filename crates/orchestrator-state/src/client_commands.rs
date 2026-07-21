use std::str::FromStr as _;

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    ClientCommand, ClientCommandAction, ClientCommandId, ClientCommandState, SessionId, TaskId,
};
use rusqlite::{OptionalExtension as _, Row, Transaction, TransactionBehavior, params};

use crate::{Database, StateError, StateResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientCommandRecoveryDisposition {
    StillClaimed,
    Requeued,
    ManualReconciliationRequired,
}

impl Database {
    pub fn load_client_command(
        &self,
        command_id: ClientCommandId,
    ) -> StateResult<Option<ClientCommand>> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT command_id, session_id, task_id, action, payload_json,
                            idempotency_key, state, requested_by, requested_at, claimed_at,
                            completed_at, outcome
                     FROM client_commands WHERE command_id = ?1",
                    [command_id.to_string()],
                    map_client_command,
                )
                .optional()
                .map_err(StateError::from)
        })
    }

    pub fn load_client_command_by_idempotency_key(
        &self,
        idempotency_key: &str,
    ) -> StateResult<Option<ClientCommand>> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT command_id, session_id, task_id, action, payload_json,
                            idempotency_key, state, requested_by, requested_at, claimed_at,
                            completed_at, outcome
                     FROM client_commands WHERE idempotency_key = ?1",
                    [idempotency_key],
                    map_client_command,
                )
                .optional()
                .map_err(StateError::from)
        })
    }

    pub fn submit_client_command(&self, command: &ClientCommand) -> StateResult<ClientCommand> {
        command
            .validate()
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        if command.state != ClientCommandState::Pending {
            return Err(StateError::InvalidRecord(
                "new client command must be pending".to_owned(),
            ));
        }

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_by_idempotency_key(&transaction, &command.idempotency_key)? {
            ensure_idempotent_match(&existing, command)?;
            transaction.commit()?;
            return Ok(existing);
        }

        transaction.execute(
            "INSERT INTO client_commands(
                command_id, session_id, task_id, action, payload_json, idempotency_key, state,
                requested_by, requested_at, claimed_at, completed_at, outcome
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, NULL, NULL, NULL)",
            params![
                command.command_id.to_string(),
                command.session_id.map(|value| value.to_string()),
                command.task_id.map(|value| value.to_string()),
                action_name(command.action),
                serde_json::to_string(&command.payload)?,
                command.idempotency_key,
                command.requested_by,
                command.requested_at.to_rfc3339(),
            ],
        )?;
        transaction.commit()?;
        Ok(command.clone())
    }

    pub fn claim_next_client_command(
        &self,
        claimed_at: DateTime<Utc>,
    ) -> StateResult<Option<ClientCommand>> {
        self.claim_client_command(claimed_at, false)
    }

    pub fn claim_next_session_client_command(
        &self,
        claimed_at: DateTime<Utc>,
    ) -> StateResult<Option<ClientCommand>> {
        self.claim_client_command(claimed_at, true)
    }

    fn claim_client_command(
        &self,
        claimed_at: DateTime<Utc>,
        session_actions_only: bool,
    ) -> StateResult<Option<ClientCommand>> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let command = {
            let sql = if session_actions_only {
                "SELECT command_id, session_id, task_id, action, payload_json, idempotency_key,
                        state, requested_by, requested_at, claimed_at, completed_at, outcome
                 FROM client_commands WHERE state = 'pending'
                   AND action IN ('create_session', 'append_message')
                 ORDER BY requested_at, command_id LIMIT 1"
            } else {
                "SELECT command_id, session_id, task_id, action, payload_json, idempotency_key,
                        state, requested_by, requested_at, claimed_at, completed_at, outcome
                 FROM client_commands WHERE state = 'pending'
                 ORDER BY requested_at, command_id LIMIT 1"
            };
            let mut statement = transaction.prepare(sql)?;
            statement.query_row([], map_client_command).optional()?
        };
        let Some(mut command) = command else {
            transaction.commit()?;
            return Ok(None);
        };
        let changed = transaction.execute(
            "UPDATE client_commands SET state = 'claimed', claimed_at = ?1
             WHERE command_id = ?2 AND state = 'pending'",
            params![claimed_at.to_rfc3339(), command.command_id.to_string()],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("client command {}", command.command_id),
            });
        }
        transaction.commit()?;
        command.state = ClientCommandState::Claimed;
        command.claimed_at = Some(claimed_at);
        Ok(Some(command))
    }

    pub fn claim_next_orchestration_client_command(
        &self,
        claimed_at: DateTime<Utc>,
    ) -> StateResult<Option<ClientCommand>> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let command = {
            let mut statement = transaction.prepare(
                "SELECT command_id, session_id, task_id, action, payload_json, idempotency_key,
                        state, requested_by, requested_at, claimed_at, completed_at, outcome
                 FROM client_commands WHERE state = 'pending'
                   AND action IN (
                       'request_plan', 'approve_graph', 'revise_graph', 'cancel_plan',
                       'request_integration', 'approve_integration', 'create_resolution_task')
                 ORDER BY requested_at, command_id LIMIT 1",
            )?;
            statement.query_row([], map_client_command).optional()?
        };
        let Some(mut command) = command else {
            transaction.commit()?;
            return Ok(None);
        };
        let changed = transaction.execute(
            "UPDATE client_commands SET state = 'claimed', claimed_at = ?1
             WHERE command_id = ?2 AND state = 'pending'",
            params![claimed_at.to_rfc3339(), command.command_id.to_string()],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("client command {}", command.command_id),
            });
        }
        transaction.commit()?;
        command.state = ClientCommandState::Claimed;
        command.claimed_at = Some(claimed_at);
        Ok(Some(command))
    }

    pub fn complete_client_command(
        &self,
        command_id: ClientCommandId,
        outcome: &str,
        completed_at: DateTime<Utc>,
    ) -> StateResult<()> {
        self.finish_client_command(
            command_id,
            ClientCommandState::Completed,
            outcome,
            completed_at,
        )
    }

    pub fn fail_client_command(
        &self,
        command_id: ClientCommandId,
        outcome: &str,
        completed_at: DateTime<Utc>,
    ) -> StateResult<()> {
        self.finish_client_command(
            command_id,
            ClientCommandState::Failed,
            outcome,
            completed_at,
        )
    }

    pub fn recover_stale_client_commands(
        &self,
        stale_before: DateTime<Utc>,
    ) -> StateResult<Vec<(ClientCommand, ClientCommandRecoveryDisposition)>> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let commands = {
            let mut statement = transaction.prepare(
                "SELECT command_id, session_id, task_id, action, payload_json, idempotency_key,
                        state, requested_by, requested_at, claimed_at, completed_at, outcome
                 FROM client_commands WHERE state = 'claimed'
                 ORDER BY requested_at, command_id",
            )?;
            statement
                .query_map([], map_client_command)?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut recovered = Vec::with_capacity(commands.len());
        for command in commands {
            let claimed_at = command.claimed_at.ok_or_else(|| {
                StateError::InvalidRecord(format!(
                    "claimed client command {} has no claim timestamp",
                    command.command_id
                ))
            })?;
            let disposition = if claimed_at >= stale_before {
                ClientCommandRecoveryDisposition::StillClaimed
            } else if matches!(
                command.action,
                ClientCommandAction::CreateSession
                    | ClientCommandAction::AppendMessage
                    | ClientCommandAction::RequestPlan
                    | ClientCommandAction::ApproveGraph
                    | ClientCommandAction::RequestIntegration
            ) {
                let changed = transaction.execute(
                    "UPDATE client_commands SET state = 'pending', claimed_at = NULL
                     WHERE command_id = ?1 AND state = 'claimed' AND claimed_at = ?2",
                    params![command.command_id.to_string(), claimed_at.to_rfc3339()],
                )?;
                if changed != 1 {
                    return Err(StateError::OptimisticConflict {
                        entity: format!("client command {}", command.command_id),
                    });
                }
                ClientCommandRecoveryDisposition::Requeued
            } else {
                ClientCommandRecoveryDisposition::ManualReconciliationRequired
            };
            recovered.push((command, disposition));
        }
        transaction.commit()?;
        Ok(recovered)
    }

    fn finish_client_command(
        &self,
        command_id: ClientCommandId,
        state: ClientCommandState,
        outcome: &str,
        completed_at: DateTime<Utc>,
    ) -> StateResult<()> {
        if outcome.trim().is_empty() {
            return Err(StateError::InvalidRecord(
                "client command outcome must not be blank".to_owned(),
            ));
        }
        let changed = self.lock()?.execute(
            "UPDATE client_commands SET state = ?1, completed_at = ?2, outcome = ?3
             WHERE command_id = ?4 AND state = 'claimed'",
            params![
                state_name(state),
                completed_at.to_rfc3339(),
                outcome,
                command_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("client command {command_id}"),
            });
        }
        Ok(())
    }
}

fn load_by_idempotency_key(
    transaction: &Transaction<'_>,
    idempotency_key: &str,
) -> StateResult<Option<ClientCommand>> {
    transaction
        .query_row(
            "SELECT command_id, session_id, task_id, action, payload_json, idempotency_key,
                    state, requested_by, requested_at, claimed_at, completed_at, outcome
             FROM client_commands WHERE idempotency_key = ?1",
            [idempotency_key],
            map_client_command,
        )
        .optional()
        .map_err(StateError::from)
}

fn ensure_idempotent_match(existing: &ClientCommand, submitted: &ClientCommand) -> StateResult<()> {
    if existing.action == submitted.action
        && existing.session_id == submitted.session_id
        && existing.task_id == submitted.task_id
        && existing.payload == submitted.payload
    {
        Ok(())
    } else {
        Err(StateError::InvalidRecord(format!(
            "idempotency key `{}` was reused with a different command",
            submitted.idempotency_key
        )))
    }
}

fn map_client_command(row: &Row<'_>) -> rusqlite::Result<ClientCommand> {
    let command_id: String = row.get(0)?;
    let session_id: Option<String> = row.get(1)?;
    let task_id: Option<String> = row.get(2)?;
    let action: String = row.get(3)?;
    let payload: String = row.get(4)?;
    let state: String = row.get(6)?;
    let requested_at: String = row.get(8)?;
    let claimed_at: Option<String> = row.get(9)?;
    let completed_at: Option<String> = row.get(10)?;
    Ok(ClientCommand {
        command_id: ClientCommandId::from_str(&command_id)
            .map_err(|error| conversion_error(0, error))?,
        session_id: session_id
            .map(|value| SessionId::from_str(&value).map_err(|error| conversion_error(1, error)))
            .transpose()?,
        task_id: task_id
            .map(|value| TaskId::from_str(&value).map_err(|error| conversion_error(2, error)))
            .transpose()?,
        action: parse_action(&action).map_err(|error| conversion_error(3, error))?,
        payload: serde_json::from_str(&payload).map_err(|error| conversion_error(4, error))?,
        idempotency_key: row.get(5)?,
        state: parse_state(&state).map_err(|error| conversion_error(6, error))?,
        requested_by: row.get(7)?,
        requested_at: parse_timestamp(&requested_at, 8)?,
        claimed_at: claimed_at
            .map(|value| parse_timestamp(&value, 9))
            .transpose()?,
        completed_at: completed_at
            .map(|value| parse_timestamp(&value, 10))
            .transpose()?,
        outcome: row.get(11)?,
    })
}

const fn action_name(action: ClientCommandAction) -> &'static str {
    match action {
        ClientCommandAction::CreateSession => "create_session",
        ClientCommandAction::AppendMessage => "append_message",
        ClientCommandAction::StopDaemon => "stop_daemon",
        ClientCommandAction::RequestPlan => "request_plan",
        ClientCommandAction::ApproveGraph => "approve_graph",
        ClientCommandAction::ReviseGraph => "revise_graph",
        ClientCommandAction::CancelPlan => "cancel_plan",
        ClientCommandAction::RequestIntegration => "request_integration",
        ClientCommandAction::ApproveIntegration => "approve_integration",
        ClientCommandAction::CreateResolutionTask => "create_resolution_task",
    }
}

fn parse_action(value: &str) -> Result<ClientCommandAction, std::io::Error> {
    match value {
        "create_session" => Ok(ClientCommandAction::CreateSession),
        "append_message" => Ok(ClientCommandAction::AppendMessage),
        "stop_daemon" => Ok(ClientCommandAction::StopDaemon),
        "request_plan" => Ok(ClientCommandAction::RequestPlan),
        "approve_graph" => Ok(ClientCommandAction::ApproveGraph),
        "revise_graph" => Ok(ClientCommandAction::ReviseGraph),
        "cancel_plan" => Ok(ClientCommandAction::CancelPlan),
        "request_integration" => Ok(ClientCommandAction::RequestIntegration),
        "approve_integration" => Ok(ClientCommandAction::ApproveIntegration),
        "create_resolution_task" => Ok(ClientCommandAction::CreateResolutionTask),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unknown client command action `{value}`"),
        )),
    }
}

const fn state_name(state: ClientCommandState) -> &'static str {
    match state {
        ClientCommandState::Pending => "pending",
        ClientCommandState::Claimed => "claimed",
        ClientCommandState::Completed => "completed",
        ClientCommandState::Failed => "failed",
    }
}

fn parse_state(value: &str) -> Result<ClientCommandState, std::io::Error> {
    match value {
        "pending" => Ok(ClientCommandState::Pending),
        "claimed" => Ok(ClientCommandState::Claimed),
        "completed" => Ok(ClientCommandState::Completed),
        "failed" => Ok(ClientCommandState::Failed),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unknown client command state `{value}`"),
        )),
    }
}

fn parse_timestamp(value: &str, column: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
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
    use std::sync::{Arc, Barrier};

    use chrono::{TimeDelta, TimeZone as _, Utc};
    use orchestrator_domain::{
        ClientCommand, ClientCommandAction, ClientCommandId, ClientCommandState,
    };

    use super::ClientCommandRecoveryDisposition;
    use crate::{Database, StateError, StateResult};

    fn timestamp() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 21, 11, 0, 0)
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

    fn command(action: ClientCommandAction, key: &str) -> ClientCommand {
        ClientCommand {
            command_id: ClientCommandId::new(),
            session_id: None,
            task_id: None,
            action,
            payload: serde_json::json!({"value": 1}),
            idempotency_key: key.to_owned(),
            state: ClientCommandState::Pending,
            requested_by: "terminal-user".to_owned(),
            requested_at: timestamp(),
            claimed_at: None,
            completed_at: None,
            outcome: None,
        }
    }

    #[test]
    fn identical_idempotency_submission_returns_original() -> StateResult<()> {
        let database = migrated_database();
        let first = command(ClientCommandAction::CreateSession, "create-1");
        assert_eq!(database.submit_client_command(&first)?, first);
        let mut replay = first.clone();
        replay.command_id = ClientCommandId::new();
        assert_eq!(database.submit_client_command(&replay)?, first);

        replay.payload = serde_json::json!({"value": 2});
        assert!(matches!(
            database.submit_client_command(&replay),
            Err(StateError::InvalidRecord(_))
        ));
        Ok(())
    }

    #[test]
    fn oldest_pending_command_is_claimed_once_and_completed_is_not_reclaimed() -> StateResult<()> {
        let database = migrated_database();
        let mut first = command(ClientCommandAction::CreateSession, "first");
        let mut second = command(ClientCommandAction::AppendMessage, "second");
        second.requested_at += TimeDelta::seconds(1);
        database.submit_client_command(&first)?;
        database.submit_client_command(&second)?;

        first.state = ClientCommandState::Claimed;
        first.claimed_at = Some(timestamp() + TimeDelta::seconds(2));
        assert_eq!(
            database.claim_next_client_command(timestamp() + TimeDelta::seconds(2))?,
            Some(first.clone())
        );
        database.complete_client_command(
            first.command_id,
            "created",
            timestamp() + TimeDelta::seconds(3),
        )?;
        assert_eq!(
            database
                .claim_next_client_command(timestamp() + TimeDelta::seconds(4))?
                .map(|claimed| claimed.command_id),
            Some(second.command_id)
        );
        Ok(())
    }

    #[test]
    fn concurrent_claim_has_one_winner() -> StateResult<()> {
        let directory = tempfile::tempdir().map_err(|error| StateError::io("temp", error))?;
        let root = std::fs::canonicalize(directory.path())
            .map_err(|error| StateError::io("temp", error))?;
        let path = root.join("state.db");
        let setup = Database::open(&path)?;
        setup.migrate_with_backup(&root.join("backups"))?;
        let pending = command(ClientCommandAction::CreateSession, "concurrent");
        setup.submit_client_command(&pending)?;

        let barrier = Arc::new(Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || -> StateResult<_> {
                    let database = Database::open(path)?;
                    barrier.wait();
                    database.claim_next_client_command(timestamp())
                })
            })
            .collect::<Vec<_>>();
        let claimed = handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| panic!("claim thread panicked"))
            })
            .collect::<StateResult<Vec<_>>>()?;
        assert_eq!(claimed.iter().filter(|value| value.is_some()).count(), 1);
        Ok(())
    }

    #[test]
    fn stale_replay_safe_commands_requeue_but_stop_requires_reconciliation() -> StateResult<()> {
        let database = migrated_database();
        for (action, key) in [
            (ClientCommandAction::CreateSession, "create"),
            (ClientCommandAction::AppendMessage, "append"),
            (ClientCommandAction::StopDaemon, "stop"),
        ] {
            database.submit_client_command(&command(action, key))?;
            database.claim_next_client_command(timestamp())?;
        }

        let recovered =
            database.recover_stale_client_commands(timestamp() + TimeDelta::seconds(1))?;
        assert_eq!(recovered.len(), 3);
        assert_eq!(
            recovered
                .iter()
                .filter(|(_, disposition)| {
                    *disposition == ClientCommandRecoveryDisposition::Requeued
                })
                .count(),
            2
        );
        assert_eq!(
            recovered
                .iter()
                .filter(|(_, disposition)| {
                    *disposition == ClientCommandRecoveryDisposition::ManualReconciliationRequired
                })
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn client_command_can_be_loaded_by_stable_id() -> StateResult<()> {
        let database = migrated_database();
        let command = command(ClientCommandAction::AppendMessage, "load-command");
        database.submit_client_command(&command)?;
        assert_eq!(
            database.load_client_command(command.command_id)?,
            Some(command)
        );
        assert_eq!(database.load_client_command(ClientCommandId::new())?, None);
        Ok(())
    }
}
