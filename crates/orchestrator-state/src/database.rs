use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use chrono::Utc;
use orchestrator_domain::TaskEvent;
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::{
    MigrationManager, MigrationStatus, RollbackApplyResult, RollbackPlan, StateError, StateResult,
    ensure_private_directory, ensure_private_file, reject_symlink_components,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxRecord {
    pub sequence: i64,
    pub event: TaskEvent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseHealth {
    pub integrity_ok: bool,
    pub foreign_key_violations: i64,
    pub current_schema_version: u32,
    pub last_event_sequence: i64,
}

/// Serialized access to the local `SQLite` state database.
pub struct Database {
    path: PathBuf,
    connection: Mutex<Connection>,
}

impl Database {
    pub fn open(path: impl Into<PathBuf>) -> StateResult<Self> {
        let path = path.into();
        let parent = path.parent().ok_or_else(|| {
            StateError::RollbackGuard(format!("database path has no parent: {}", path.display()))
        })?;
        ensure_private_directory(parent)?;
        reject_symlink_components(&path)?;
        let connection = Connection::open(&path)?;
        crate::migrations::configure_connection(&connection)?;
        ensure_private_file(&path)?;
        Ok(Self {
            path,
            connection: Mutex::new(connection),
        })
    }

    pub fn open_in_memory() -> StateResult<Self> {
        let connection = Connection::open_in_memory()?;
        crate::migrations::configure_connection(&connection)?;
        Ok(Self {
            path: PathBuf::from(":memory:"),
            connection: Mutex::new(connection),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn migration_status(&self) -> StateResult<MigrationStatus> {
        let connection = self.lock()?;
        MigrationManager::status(&connection)
    }

    pub fn migrate_with_backup(&self, backup_directory: &Path) -> StateResult<MigrationStatus> {
        let mut connection = self.lock()?;
        let plan = MigrationManager::plan(&connection)?;
        if plan.pending_versions.is_empty() {
            return MigrationManager::status(&connection);
        }
        if plan.current_version > 0 {
            ensure_private_directory(backup_directory)?;
            let timestamp = Utc::now().format("%Y%m%dT%H%M%S%.fZ");
            let destination = backup_directory.join(format!("orchestrator.db.backup.{timestamp}"));
            MigrationManager::backup(&connection, &destination)?;
        }
        MigrationManager::apply(&mut connection)
    }

    pub fn dry_run_migrations(&self) -> StateResult<MigrationStatus> {
        let connection = self.lock()?;
        MigrationManager::dry_run(&connection)
    }

    /// Applies one integrity-sealed migration rollback while retaining exclusive access to
    /// the live connection for backup, guard revalidation, restore, and verification.
    pub fn apply_migration_rollback(
        &self,
        plan: &RollbackPlan,
        expected_plan_hash: &str,
        approved_by: &str,
        recovery_backup_path: &Path,
    ) -> StateResult<RollbackApplyResult> {
        let mut connection = self.lock()?;
        MigrationManager::apply_rollback(
            &mut connection,
            plan,
            expected_plan_hash,
            approved_by,
            recovery_backup_path,
        )
    }

    pub fn with_connection<T>(
        &self,
        operation: impl FnOnce(&Connection) -> StateResult<T>,
    ) -> StateResult<T> {
        let connection = self.lock()?;
        operation(&connection)
    }

    pub fn with_transaction<T>(
        &self,
        operation: impl FnOnce(&rusqlite::Transaction<'_>) -> StateResult<T>,
    ) -> StateResult<T> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let result = operation(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Assigns the next global sequence, seals the event hash, and inserts it into the
    /// `SQLite` outbox in one transaction.
    pub fn append_event(&self, mut event: TaskEvent) -> StateResult<TaskEvent> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        append_event_in_transaction(&transaction, &mut event)?;
        transaction.commit()?;
        Ok(event)
    }

    pub fn outbox_after(&self, sequence: i64, limit: usize) -> StateResult<Vec<OutboxRecord>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT sequence, event_json FROM task_events \
             WHERE sequence > ?1 ORDER BY sequence LIMIT ?2",
        )?;
        let records = statement.query_map(params![sequence, limit], |row| {
            let sequence: i64 = row.get(0)?;
            let json: String = row.get(1)?;
            let event = serde_json::from_str(&json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            Ok(OutboxRecord { sequence, event })
        })?;
        records
            .collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    pub fn event_at(&self, sequence: i64) -> StateResult<Option<TaskEvent>> {
        let connection = self.lock()?;
        let json: Option<String> = connection
            .query_row(
                "SELECT event_json FROM task_events WHERE sequence = ?1",
                [sequence],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|value| serde_json::from_str(&value).map_err(StateError::from))
            .transpose()
    }

    pub fn mark_exported(&self, sequence: i64, event_hash: &str) -> StateResult<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let stored_hash: String = transaction.query_row(
            "SELECT event_hash FROM task_events WHERE sequence = ?1",
            [sequence],
            |row| row.get(0),
        )?;
        if stored_hash != event_hash {
            return Err(StateError::InvalidEventChain {
                sequence,
                reason: "export marker hash does not match database".to_owned(),
            });
        }
        let now = Utc::now().to_rfc3339();
        transaction.execute(
            "UPDATE task_events SET exported_at = coalesce(exported_at, ?1) \
             WHERE sequence <= ?2",
            params![now, sequence],
        )?;
        transaction.execute(
            "UPDATE event_log_state SET last_exported_sequence = ?1, \
             last_exported_hash = ?2, updated_at = ?3 \
             WHERE singleton = 1 AND last_exported_sequence <= ?1",
            params![sequence, event_hash, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn health(&self) -> StateResult<DatabaseHealth> {
        let connection = self.lock()?;
        let integrity: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        let foreign_key_violations: i64 =
            connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        let status = MigrationManager::status(&connection)?;
        let last_event_sequence = if status.current_version >= 3 {
            connection.query_row(
                "SELECT coalesce(max(sequence), 0) FROM task_events",
                [],
                |row| row.get(0),
            )?
        } else {
            0
        };
        Ok(DatabaseHealth {
            integrity_ok: integrity == "ok",
            foreign_key_violations,
            current_schema_version: status.current_version,
            last_event_sequence,
        })
    }

    pub(crate) fn lock(&self) -> StateResult<MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| StateError::LockPoisoned)
    }
}

pub(crate) fn append_event_in_transaction(
    transaction: &Transaction<'_>,
    event: &mut TaskEvent,
) -> StateResult<()> {
    let previous: Option<(i64, String)> = transaction
        .query_row(
            "SELECT sequence, event_hash FROM task_events ORDER BY sequence DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let sequence = previous.as_ref().map_or(1_i64, |(value, _)| value + 1);
    event.sequence = u64::try_from(sequence).map_err(|_| StateError::InvalidEventChain {
        sequence,
        reason: "negative sequence generated by SQLite".to_owned(),
    })?;
    event.previous_hash = previous.map(|(_, hash)| hash);
    event
        .refresh_event_hash()
        .map_err(|error| StateError::InvalidEventChain {
            sequence,
            reason: error.to_string(),
        })?;
    let event_json = serde_json::to_string(&event)?;
    let event_type = serde_string(&event.event_type)?;
    transaction.execute(
        "INSERT INTO task_events( \
            sequence, event_id, task_id, event_type, schema_version, occurred_at, event_json, \
            previous_hash, event_hash, exported_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
        params![
            sequence,
            event.event_id.to_string(),
            event.task_id.map(|id| id.to_string()),
            event_type,
            event.schema_version.as_str(),
            event.occurred_at.to_rfc3339(),
            event_json,
            event.previous_hash,
            event.event_hash,
        ],
    )?;
    Ok(())
}

fn serde_string(value: &impl Serialize) -> StateResult<String> {
    let value = serde_json::to_value(value)?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| StateError::InvalidEventChain {
            sequence: 0,
            reason: "expected string representation".to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use orchestrator_domain::{
        CorrelationId, EventActor, EventId, EventType, SchemaVersion, TaskEvent,
    };
    use serde_json::json;

    use super::Database;
    use crate::{MigrationManager, STATE_SCHEMA_VERSION, StateError, StateResult};

    #[test]
    fn events_are_sequenced_and_hash_chained() {
        let database =
            Database::open_in_memory().unwrap_or_else(|error| panic!("database: {error}"));
        database
            .migrate_with_backup(std::path::Path::new("unused"))
            .unwrap_or_else(|error| panic!("migrations: {error}"));
        let make_event = || TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            session_id: None,
            task_id: None,
            occurred_at: Utc::now(),
            event_type: EventType::CompatibilityWarning,
            from_state: None,
            to_state: None,
            reason: None,
            actor: EventActor::System,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: json!({}),
            previous_hash: None,
            event_hash: String::new(),
        };
        let first = database
            .append_event(make_event())
            .unwrap_or_else(|error| panic!("first event: {error}"));
        let second = database
            .append_event(make_event())
            .unwrap_or_else(|error| panic!("second event: {error}"));
        assert_eq!(first.sequence, 1);
        assert_eq!(
            second.previous_hash.as_deref(),
            Some(first.event_hash.as_str())
        );
        assert!(second.verify_hash().unwrap_or(false));
    }

    #[test]
    fn database_wrapper_keeps_migration_restore_inside_connection_lock() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let database = Database::open(temporary.path().join("live.db"))?;
        database.migrate_with_backup(&temporary.path().join("migration-backups"))?;
        database.with_connection(|connection| {
            connection.execute_batch(
                "CREATE TABLE rollback_wrapper_marker(value TEXT PRIMARY KEY NOT NULL) STRICT; \
                 INSERT INTO rollback_wrapper_marker(value) VALUES ('before');",
            )?;
            Ok(())
        })?;

        let prior = temporary.path().join("prior.db");
        database.with_connection(|connection| {
            MigrationManager::backup(connection, &prior).map(|_| ())
        })?;
        database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO rollback_wrapper_marker(value) VALUES ('after')",
                [],
            )?;
            Ok(())
        })?;
        let plan = database.with_connection(|connection| {
            MigrationManager::create_rollback_plan(connection, &prior)
        })?;

        let recovery = temporary.path().join("recovery.db");
        let result = database.apply_migration_rollback(
            &plan,
            &plan.integrity_hash,
            "enterprise-admin",
            &recovery,
        )?;

        assert_eq!(result.restored_schema_version, STATE_SCHEMA_VERSION);
        assert!(result.recovery_backup_path.exists());
        let marker_count = database.with_connection(|connection| {
            connection
                .query_row("SELECT count(*) FROM rollback_wrapper_marker", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(StateError::from)
        })?;
        assert_eq!(marker_count, 1);
        Ok(())
    }
}
