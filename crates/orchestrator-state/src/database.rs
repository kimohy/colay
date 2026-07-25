use std::{
    cell::Cell,
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use chrono::Utc;
use orchestrator_domain::TaskEvent;
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactStore, MigrationManager, MigrationPlan, MigrationStatus, RollbackApplyResult,
    RollbackPlan, STATE_SCHEMA_VERSION, StateError, StateResult, WorkspaceId,
    ensure_private_directory, ensure_private_file, reject_symlink_components,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxRecord {
    pub sequence: i64,
    pub event: TaskEvent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceOutboxRecord {
    pub workspace_id: WorkspaceId,
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
    #[cfg(test)]
    test_workspace: Mutex<Option<WorkspaceId>>,
}

/// Workspace-bound access to all durable task, session, graph, and audit state.
#[derive(Clone, Copy)]
pub struct WorkspaceDatabase<'a> {
    database: &'a Database,
    workspace_id: WorkspaceId,
}

thread_local! {
    static REQUESTED_WORKSPACE: Cell<Option<WorkspaceId>> = const { Cell::new(None) };
    static BOUND_WORKSPACE: Cell<Option<WorkspaceId>> = const { Cell::new(None) };
}

const WORKSPACE_TABLES: &[&str] = &[
    "approval_records",
    "artifacts",
    "changed_files",
    "checkpoints",
    "client_commands",
    "client_command_invocations",
    "command_evidence",
    "conversation_attempts",
    "conversation_messages",
    "coordinator_leases",
    "event_log_state",
    "graph_approvals",
    "graph_revisions",
    "handovers",
    "integration_applications",
    "integration_approvals",
    "integration_batches",
    "integration_resolution_tasks",
    "integration_sources",
    "planning_attempts",
    "provider_usage_snapshots",
    "requirement_revisions",
    "resource_claims",
    "routing_decision_usage",
    "routing_decisions",
    "session_graph_heads",
    "session_requirement_heads",
    "session_tasks",
    "session_workspace_state",
    "sessions",
    "task_attempts",
    "task_controls",
    "task_dependencies",
    "task_events",
    "task_instructions",
    "task_schedule_claims",
    "tasks",
    "verification_results",
    "worker_leases",
    "worktrees",
];

fn read_database_header_schema(path: &Path, byte_length: u64) -> StateResult<u32> {
    if byte_length == 0 {
        return Ok(0);
    }
    if byte_length < 64 {
        return Err(StateError::InvalidRecord(format!(
            "database header is truncated: {}",
            path.display()
        )));
    }
    let mut file = fs::File::open(path).map_err(|error| StateError::io(path, error))?;
    let mut header = [0_u8; 64];
    file.read_exact(&mut header)
        .map_err(|error| StateError::io(path, error))?;
    if &header[..16] != b"SQLite format 3\0" {
        return Err(StateError::InvalidRecord(format!(
            "database header is not SQLite format 3: {}",
            path.display()
        )));
    }
    Ok(u32::from_be_bytes(header[60..64].try_into().map_err(
        |_| StateError::InvalidRecord("database user_version is truncated".into()),
    )?))
}

fn sqlite_recovery_sidecar_exists(path: &Path) -> StateResult<bool> {
    for suffix in ["-wal", "-journal"] {
        let sidecar = sqlite_sidecar(path, suffix);
        match fs::symlink_metadata(&sidecar) {
            Ok(metadata) => {
                reject_symlink_components(&sidecar)?;
                if !metadata.is_file() {
                    return Err(StateError::InvalidRecord(format!(
                        "database sidecar is not a regular file: {}",
                        sidecar.display()
                    )));
                }
                return Ok(true);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(StateError::io(&sidecar, error)),
        }
    }
    Ok(false)
}

fn snapshot_schema(path: &Path) -> StateResult<u32> {
    let temporary = crate::CanonicalTempDir::new("database-schema-preflight")?;
    let snapshot = temporary.path().join("state.db");
    copy_database_component(path, &snapshot, false)?;
    for suffix in ["-wal", "-journal"] {
        let source = sqlite_sidecar(path, suffix);
        let destination = sqlite_sidecar(&snapshot, suffix);
        copy_database_component(&source, &destination, true)?;
    }
    let connection = Connection::open_with_flags(
        &snapshot,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(Into::into)
}

fn copy_database_component(source: &Path, destination: &Path, optional: bool) -> StateResult<()> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::io(source, error)),
    };
    reject_symlink_components(source)?;
    if !metadata.is_file() {
        return Err(StateError::InvalidRecord(format!(
            "database component is not a regular file: {}",
            source.display()
        )));
    }
    fs::copy(source, destination)
        .map(|_| ())
        .map_err(|error| StateError::io(destination, error))
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

impl Database {
    pub fn open(path: impl Into<PathBuf>) -> StateResult<Self> {
        let path = path.into();
        Self::preflight_schema(&path)?;
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
            #[cfg(test)]
            test_workspace: Mutex::new(None),
        })
    }

    /// Refuses an existing future-schema database from raw bytes or a private sidecar snapshot
    /// before normal open can change persistent `SQLite` settings such as journal mode.
    pub fn preflight_schema(path: &Path) -> StateResult<Option<u32>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(StateError::io(path, error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StateError::UnsafeArtifactPath(path.display().to_string()));
        }
        reject_symlink_components(path)?;
        let mut found = read_database_header_schema(path, metadata.len())?;
        if found <= STATE_SCHEMA_VERSION && sqlite_recovery_sidecar_exists(path)? {
            found = snapshot_schema(path)?;
        }
        if found > STATE_SCHEMA_VERSION {
            return Err(StateError::FutureSchema {
                found,
                supported: STATE_SCHEMA_VERSION,
            });
        }
        Ok(Some(found))
    }

    /// Opens an existing current-schema database without creating files, changing permissions,
    /// or applying persistent connection settings. Callers must remain read-only.
    pub fn open_read_only(path: impl Into<PathBuf>) -> StateResult<Self> {
        let path = path.into();
        let found = Self::preflight_schema(&path)?.ok_or_else(|| {
            StateError::InvalidRecord(format!("database does not exist: {}", path.display()))
        })?;
        if found != STATE_SCHEMA_VERSION {
            return Err(StateError::InvalidRecord(format!(
                "database schema version {found} requires migration to {STATE_SCHEMA_VERSION}"
            )));
        }
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        register_workspace_function(&connection)?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;\
             PRAGMA temp_store = MEMORY;\
             PRAGMA busy_timeout = 5000;",
        )?;
        Ok(Self {
            path,
            connection: Mutex::new(connection),
            #[cfg(test)]
            test_workspace: Mutex::new(None),
        })
    }

    pub fn open_in_memory() -> StateResult<Self> {
        let connection = Connection::open_in_memory()?;
        crate::migrations::configure_connection(&connection)?;
        Ok(Self {
            path: PathBuf::from(":memory:"),
            connection: Mutex::new(connection),
            #[cfg(test)]
            test_workspace: Mutex::new(None),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn artifact_store(&self) -> StateResult<ArtifactStore> {
        artifact_store_for_database_path(self.path())
    }

    #[must_use]
    pub const fn workspace(&self, workspace_id: WorkspaceId) -> WorkspaceDatabase<'_> {
        WorkspaceDatabase {
            database: self,
            workspace_id,
        }
    }

    /// Returns the explicit compatibility partition used only for databases migrated from the
    /// pre-workspace schema. Fresh schema-13 databases do not create this partition, so callers
    /// must handle [`StateError::WorkspaceNotFound`] and migrate to registry-selected workspace
    /// context instead of treating this as a default.
    pub fn legacy_workspace(&self) -> StateResult<WorkspaceDatabase<'_>> {
        let workspace_id = WorkspaceId::from_uuid(uuid::Uuid::from_u128(1));
        let connection = self.raw_lock()?;
        let state_version: u32 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if state_version < 13 {
            return Err(StateError::WorkspaceNotFound {
                workspace_id: workspace_id.to_string(),
            });
        }
        let exists = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.workspaces WHERE workspace_id = ?1)",
            [workspace_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Err(StateError::WorkspaceNotFound {
                workspace_id: workspace_id.to_string(),
            });
        }
        Ok(self.workspace(workspace_id))
    }

    pub fn migration_status(&self) -> StateResult<MigrationStatus> {
        let connection = self.lock()?;
        MigrationManager::status(&connection)
    }

    pub fn migration_plan(&self) -> StateResult<MigrationPlan> {
        let connection = self.lock()?;
        MigrationManager::plan(&connection)
    }

    pub fn create_validated_migration_rollback_plan(
        &self,
        backup_path: impl AsRef<Path>,
    ) -> StateResult<RollbackPlan> {
        let connection = self.lock()?;
        let plan = MigrationManager::create_rollback_plan(&connection, backup_path)?;
        MigrationManager::validate_rollback(&connection, &plan)?;
        Ok(plan)
    }

    pub fn validate_migration_rollback(&self, plan: &RollbackPlan) -> StateResult<()> {
        let connection = self.lock()?;
        MigrationManager::validate_rollback(&connection, plan)
    }

    pub fn migrate_with_backup(&self, backup_directory: &Path) -> StateResult<MigrationStatus> {
        let mut connection = self.raw_lock()?;
        let plan = MigrationManager::plan(&connection)?;
        if plan.pending_versions.is_empty() {
            let status = MigrationManager::status(&connection)?;
            #[cfg(test)]
            self.install_test_workspace(&connection)?;
            return Ok(status);
        }
        if plan.current_version > 0 {
            ensure_private_directory(backup_directory)?;
            let timestamp = Utc::now().format("%Y%m%dT%H%M%S%.fZ");
            let destination = backup_directory.join(format!("orchestrator.db.backup.{timestamp}"));
            MigrationManager::backup(&connection, &destination)?;
        }
        let status = MigrationManager::apply(&mut connection)?;
        #[cfg(test)]
        self.install_test_workspace(&connection)?;
        Ok(status)
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
        let mut connection = self.raw_lock()?;
        MigrationManager::apply_rollback(
            &mut connection,
            plan,
            expected_plan_hash,
            approved_by,
            recovery_backup_path,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_connection<T>(
        &self,
        operation: impl FnOnce(&Connection) -> StateResult<T>,
    ) -> StateResult<T> {
        let connection = self.lock()?;
        operation(&connection)
    }

    #[cfg(test)]
    pub(crate) fn with_transaction<T>(
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
    #[cfg(test)]
    pub fn append_event(&self, mut event: TaskEvent) -> StateResult<TaskEvent> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        append_event_in_transaction(&transaction, &mut event)?;
        transaction.commit()?;
        Ok(event)
    }

    #[cfg(test)]
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

    #[cfg(test)]
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

    #[cfg(test)]
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
            "UPDATE main.task_events SET exported_at = coalesce(exported_at, ?1) \
             WHERE workspace_id = current_workspace() AND sequence <= ?2",
            params![now, sequence],
        )?;
        transaction.execute(
            "INSERT INTO main.event_log_state( \
                last_exported_sequence, last_exported_hash, updated_at \
             ) VALUES (?1, ?2, ?3) \
             ON CONFLICT(workspace_id) DO UPDATE SET \
                last_exported_sequence = excluded.last_exported_sequence, \
                last_exported_hash = excluded.last_exported_hash, \
                updated_at = excluded.updated_at \
             WHERE event_log_state.last_exported_sequence <= excluded.last_exported_sequence",
            params![sequence, event_hash, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn health(&self) -> StateResult<DatabaseHealth> {
        let connection = self.raw_lock()?;
        let integrity: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        let foreign_key_violations: i64 =
            connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        let status = MigrationManager::status(&connection)?;
        let last_event_sequence = if status.current_version >= 3 {
            let sql = if status.current_version >= 13 {
                // Sequences restart in every workspace at schema 13. The aggregate count is
                // monotonic and therefore remains useful to global health and rollback guards.
                "SELECT count(*) FROM main.task_events"
            } else {
                "SELECT coalesce(max(sequence), 0) FROM main.task_events"
            };
            connection.query_row(sql, [], |row| row.get(0))?
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
        #[cfg(test)]
        let fallback = self.test_workspace.lock().ok().and_then(|value| *value);
        #[cfg(not(test))]
        let fallback = None;
        let requested = REQUESTED_WORKSPACE.take().or(fallback);
        let connection = self.connection_lock()?;
        BOUND_WORKSPACE.set(requested);
        if let Some(workspace_id) = requested {
            install_workspace_scope(&connection, workspace_id)?;
        } else {
            install_workspace_scope_value(&connection, "__unbound_workspace__")?;
        }
        Ok(connection)
    }

    pub(crate) fn raw_lock(&self) -> StateResult<MutexGuard<'_, Connection>> {
        REQUESTED_WORKSPACE.set(None);
        BOUND_WORKSPACE.set(None);
        let connection = self.connection_lock()?;
        let state_version: u32 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if state_version >= 13 {
            drop_workspace_scope(&connection)?;
        }
        Ok(connection)
    }

    fn connection_lock(&self) -> StateResult<MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| StateError::LockPoisoned)
    }

    #[cfg(test)]
    fn install_test_workspace(&self, connection: &Connection) -> StateResult<()> {
        let workspace_id = WorkspaceId::from_uuid(uuid::Uuid::from_u128(u128::MAX));
        connection.execute(
            "INSERT OR IGNORE INTO workspaces(workspace_id, kind, status, created_at, last_seen_at) \
             VALUES (?1, 'directory', 'detached', ?2, ?2)",
            params![workspace_id.to_string(), Utc::now().to_rfc3339()],
        )?;
        *self
            .test_workspace
            .lock()
            .map_err(|_| StateError::LockPoisoned)? = Some(workspace_id);
        Ok(())
    }
}

impl WorkspaceDatabase<'_> {
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) fn artifact_store(&self) -> StateResult<ArtifactStore> {
        artifact_store_for_database_path(self.database.path())
    }

    pub(crate) fn lock(&self) -> StateResult<MutexGuard<'_, Connection>> {
        REQUESTED_WORKSPACE.set(Some(self.workspace_id));
        self.database.lock()
    }

    pub(crate) fn with_connection<T>(
        &self,
        operation: impl FnOnce(&Connection) -> StateResult<T>,
    ) -> StateResult<T> {
        let connection = self.lock()?;
        operation(&connection)
    }

    pub(crate) fn with_transaction<T>(
        &self,
        operation: impl FnOnce(&rusqlite::Transaction<'_>) -> StateResult<T>,
    ) -> StateResult<T> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let result = operation(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Assigns the next sequence within this workspace and seals the event hash.
    pub fn append_event(&self, mut event: TaskEvent) -> StateResult<TaskEvent> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        append_workspace_event_in_transaction(&transaction, self.workspace_id, &mut event)?;
        transaction.commit()?;
        Ok(event)
    }

    pub fn outbox_after(
        &self,
        sequence: i64,
        limit: usize,
    ) -> StateResult<Vec<WorkspaceOutboxRecord>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT sequence, event_json FROM main.task_events \
             WHERE workspace_id = ?1 AND sequence > ?2 ORDER BY sequence LIMIT ?3",
        )?;
        let records = statement.query_map(
            params![self.workspace_id.to_string(), sequence, limit],
            |row| {
                let sequence: i64 = row.get(0)?;
                let json: String = row.get(1)?;
                let event = serde_json::from_str(&json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(WorkspaceOutboxRecord {
                    workspace_id: self.workspace_id,
                    sequence,
                    event,
                })
            },
        )?;
        records
            .collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    /// Returns the latest workspace event cursor, or zero before the first event.
    pub fn latest_outbox_sequence(&self) -> StateResult<i64> {
        self.lock()?
            .query_row(
                "SELECT coalesce(max(sequence), 0) FROM task_events",
                [],
                |row| row.get(0),
            )
            .map_err(StateError::from)
    }

    pub fn event_at(&self, sequence: i64) -> StateResult<Option<TaskEvent>> {
        let connection = self.lock()?;
        let json: Option<String> = connection
            .query_row(
                "SELECT event_json FROM main.task_events WHERE workspace_id = ?1 AND sequence = ?2",
                params![self.workspace_id.to_string(), sequence],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|value| serde_json::from_str(&value).map_err(StateError::from))
            .transpose()
    }

    pub fn mark_exported(&self, sequence: i64, event_hash: &str) -> StateResult<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let workspace_id = self.workspace_id.to_string();
        let stored_hash: String = transaction.query_row(
            "SELECT event_hash FROM main.task_events WHERE workspace_id = ?1 AND sequence = ?2",
            params![workspace_id, sequence],
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
            "UPDATE main.task_events SET exported_at = coalesce(exported_at, ?1) \
             WHERE workspace_id = ?2 AND sequence <= ?3",
            params![now, workspace_id, sequence],
        )?;
        transaction.execute(
            "INSERT INTO main.event_log_state(workspace_id, last_exported_sequence, \
                last_exported_hash, updated_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(workspace_id) DO UPDATE SET \
                last_exported_sequence = excluded.last_exported_sequence, \
                last_exported_hash = excluded.last_exported_hash, updated_at = excluded.updated_at \
             WHERE event_log_state.last_exported_sequence <= excluded.last_exported_sequence",
            params![workspace_id, sequence, event_hash, now],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

fn artifact_store_for_database_path(path: &Path) -> StateResult<ArtifactStore> {
    if path == Path::new(":memory:") {
        return Err(StateError::InvalidRecord(
            "an in-memory database cannot verify external checkpoint artifacts".to_owned(),
        ));
    }
    let root = path.parent().ok_or_else(|| {
        StateError::InvalidRecord("database path has no artifact root".to_owned())
    })?;
    ArtifactStore::open(root)
}

fn install_workspace_scope(connection: &Connection, workspace_id: WorkspaceId) -> StateResult<()> {
    install_workspace_scope_value(connection, &workspace_id.to_string())
}

fn install_workspace_scope_value(connection: &Connection, workspace_id: &str) -> StateResult<()> {
    let state_version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if state_version < 13 {
        return Ok(());
    }
    let context_exists = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM temp.sqlite_temp_master
            WHERE type = 'table' AND name = '_orchestrator_workspace_context'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if context_exists {
        let changed = connection.execute(
            "UPDATE temp._orchestrator_workspace_context SET workspace_id = ?1",
            [workspace_id],
        )?;
        if changed == 1 {
            return Ok(());
        }
    }
    drop_workspace_scope(connection)?;
    connection.execute_batch(
        "CREATE TEMP TABLE _orchestrator_workspace_context( \
            workspace_id TEXT PRIMARY KEY NOT NULL \
         ) WITHOUT ROWID;",
    )?;
    connection.execute(
        "INSERT INTO temp._orchestrator_workspace_context(workspace_id) VALUES (?1)",
        [workspace_id],
    )?;

    for table in WORKSPACE_TABLES {
        let mut statement = connection.prepare(&format!("PRAGMA main.table_info('{table}')"))?;
        let columns = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let visible = columns
            .iter()
            .filter(|(name, _, _)| name != "workspace_id")
            .collect::<Vec<_>>();
        let select_columns = visible
            .iter()
            .map(|(name, _, _)| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(", ");
        connection.execute_batch(&format!(
            "CREATE TEMP VIEW \"{table}\" AS \
                 SELECT rowid AS rowid, {select_columns} FROM main.\"{table}\" \
                 WHERE workspace_id = (SELECT workspace_id FROM temp._orchestrator_workspace_context);"
        ))?;
    }
    Ok(())
}

fn drop_workspace_scope(connection: &Connection) -> StateResult<()> {
    for table in WORKSPACE_TABLES {
        connection.execute_batch(&format!("DROP VIEW IF EXISTS temp.\"{table}\";"))?;
    }
    connection.execute_batch("DROP TABLE IF EXISTS temp._orchestrator_workspace_context;")?;
    Ok(())
}

pub(crate) fn register_workspace_function(connection: &Connection) -> StateResult<()> {
    connection.create_scalar_function(
        "current_workspace",
        0,
        rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |_| {
            #[cfg(test)]
            let fallback = Some(WorkspaceId::from_uuid(uuid::Uuid::from_u128(u128::MAX)));
            #[cfg(not(test))]
            let fallback = None;
            let workspace_id = BOUND_WORKSPACE.get().or(fallback);
            Ok(workspace_id.map(|workspace_id| workspace_id.to_string()))
        },
    )?;
    Ok(())
}

pub(crate) fn append_workspace_event_in_transaction(
    transaction: &Transaction<'_>,
    workspace_id: WorkspaceId,
    event: &mut TaskEvent,
) -> StateResult<()> {
    let workspace_id = workspace_id.to_string();
    let previous: Option<(i64, String)> = transaction
        .query_row(
            "SELECT sequence, event_hash FROM main.task_events \
             WHERE workspace_id = ?1 ORDER BY sequence DESC LIMIT 1",
            [&workspace_id],
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
    transaction.execute(
        "INSERT INTO main.task_events( \
            workspace_id, sequence, event_id, task_id, event_type, schema_version, occurred_at, \
            event_json, previous_hash, event_hash, exported_at, session_id \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11)",
        params![
            workspace_id,
            sequence,
            event.event_id.to_string(),
            event.task_id.map(|id| id.to_string()),
            serde_string(&event.event_type)?,
            event.schema_version.as_str(),
            event.occurred_at.to_rfc3339(),
            serde_json::to_string(&event)?,
            event.previous_hash,
            event.event_hash,
            event.session_id.map(|id| id.to_string()),
        ],
    )?;
    Ok(())
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
    let state_version: u32 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if state_version >= 4 {
        transaction.execute(
            "INSERT INTO main.task_events( \
                sequence, event_id, task_id, event_type, schema_version, occurred_at, event_json, \
                previous_hash, event_hash, exported_at, session_id \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10)",
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
                event.session_id.map(|id| id.to_string()),
            ],
        )?;
    } else {
        if event.session_id.is_some() {
            return Err(StateError::InvalidRecord(
                "session events require state schema version 4".to_owned(),
            ));
        }
        transaction.execute(
            "INSERT INTO main.task_events( \
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
    }
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
