use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, MAIN_DB, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    StateError, StateResult, ensure_private_directory, ensure_private_file,
    reject_symlink_components, verify_private_file,
};

pub const STATE_SCHEMA_VERSION: u32 = 4;
pub const ROLLBACK_PLAN_SCHEMA_VERSION: u32 = 1;

const MIGRATIONS: &[(u32, &str, &str)] = &[
    (1, "core", include_str!("../../../migrations/0001_core.sql")),
    (
        2,
        "execution",
        include_str!("../../../migrations/0002_execution.sql"),
    ),
    (
        3,
        "audit_and_control",
        include_str!("../../../migrations/0003_audit_and_control.sql"),
    ),
    (
        4,
        "durable_sessions",
        include_str!("../../../migrations/0004_durable_sessions.sql"),
    ),
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedMigration {
    pub version: u32,
    pub name: String,
    pub checksum: String,
    pub applied_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationStatus {
    pub current_version: u32,
    pub target_version: u32,
    pub applied: Vec<AppliedMigration>,
    pub pending_versions: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub current_version: u32,
    pub target_version: u32,
    pub pending_versions: Vec<u32>,
    pub destructive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackPlan {
    pub plan_schema_version: u32,
    pub backup_path: PathBuf,
    pub backup_sha256: String,
    pub expected_current_schema_version: u32,
    pub schema_version: u32,
    pub expected_last_event_sequence: i64,
    pub backup_last_event_sequence: i64,
    pub created_at: DateTime<Utc>,
    pub integrity_hash: String,
}

impl RollbackPlan {
    pub fn verify_integrity_hash(&self) -> StateResult<()> {
        if self.plan_schema_version != ROLLBACK_PLAN_SCHEMA_VERSION {
            return Err(StateError::RollbackGuard(format!(
                "rollback plan schema {} is unsupported; expected {}",
                self.plan_schema_version, ROLLBACK_PLAN_SCHEMA_VERSION
            )));
        }
        validate_sha256("backup_sha256", &self.backup_sha256)?;
        validate_sha256("integrity_hash", &self.integrity_hash)?;
        let expected = rollback_plan_hash(self)?;
        if expected != self.integrity_hash {
            return Err(StateError::RollbackGuard(
                "rollback plan integrity hash does not match its sealed fields".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackApplyResult {
    pub plan_hash: String,
    pub approved_by: String,
    pub restored_schema_version: u32,
    pub restored_last_event_sequence: i64,
    pub recovery_backup_path: PathBuf,
    pub recovery_backup_sha256: String,
    pub completed_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MigrationManager;

impl MigrationManager {
    pub fn status(connection: &Connection) -> StateResult<MigrationStatus> {
        let applied = load_applied(connection)?;
        let current_version = applied.last().map_or(0, |migration| migration.version);
        if current_version > STATE_SCHEMA_VERSION {
            return Err(StateError::FutureSchema {
                found: current_version,
                supported: STATE_SCHEMA_VERSION,
            });
        }
        validate_sequence_and_checksums(&applied)?;
        let pragma_version: u32 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if pragma_version != current_version {
            return Err(StateError::InvalidRecord(format!(
                "SQLite user_version {pragma_version} differs from migration table version {current_version}"
            )));
        }
        let pending_versions = ((current_version + 1)..=STATE_SCHEMA_VERSION).collect();
        Ok(MigrationStatus {
            current_version,
            target_version: STATE_SCHEMA_VERSION,
            applied,
            pending_versions,
        })
    }

    pub fn plan(connection: &Connection) -> StateResult<MigrationPlan> {
        let status = Self::status(connection)?;
        Ok(MigrationPlan {
            current_version: status.current_version,
            target_version: status.target_version,
            pending_versions: status.pending_versions,
            destructive: false,
        })
    }

    pub fn apply(connection: &mut Connection) -> StateResult<MigrationStatus> {
        let plan = Self::plan(connection)?;
        for version in plan.pending_versions {
            let (expected_version, name, sql) = migration(version)?;
            if expected_version != version {
                return Err(StateError::MigrationGap {
                    version: expected_version,
                    expected: version,
                });
            }
            let transaction = connection.transaction()?;
            transaction.execute_batch(sql)?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, name, checksum, applied_at)\
                 VALUES (?1, ?2, ?3, ?4)",
                params![version, name, checksum(sql), Utc::now().to_rfc3339()],
            )?;
            transaction.commit()?;
        }
        Self::status(connection)
    }

    /// Executes all pending migrations against an online-backup copy and verifies `SQLite`
    /// integrity. The live database is not changed.
    pub fn dry_run(connection: &Connection) -> StateResult<MigrationStatus> {
        let temporary = crate::CanonicalTempDir::new("migration-dry-run")?;
        let path = temporary.path().join("dry-run.db");
        connection.backup(MAIN_DB, &path, None)?;
        let mut copy = Connection::open(&path)?;
        configure_connection(&copy)?;
        let status = Self::apply(&mut copy)?;
        let integrity: String = copy.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(StateError::RollbackGuard(format!(
                "dry-run integrity_check returned `{integrity}`"
            )));
        }
        let foreign_key_failures: i64 =
            copy.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_failures != 0 {
            return Err(StateError::RollbackGuard(format!(
                "dry-run found {foreign_key_failures} foreign-key violations"
            )));
        }
        Ok(status)
    }

    pub fn backup(connection: &Connection, destination: &Path) -> StateResult<PathBuf> {
        let parent = destination.parent().ok_or_else(|| {
            StateError::RollbackGuard(format!(
                "backup path has no parent: {}",
                destination.display()
            ))
        })?;
        ensure_private_directory(parent)?;
        reject_symlink_components(destination)?;
        if destination.exists() {
            return Err(StateError::RollbackGuard(format!(
                "refusing to overwrite backup {}",
                destination.display()
            )));
        }
        connection.backup(MAIN_DB, destination, None)?;
        ensure_private_file(destination)?;
        // Online backup preserves the source journal-mode header. Normalize the
        // standalone recovery artifact to DELETE mode so opening it never leaves
        // untracked `-wal`/`-shm` sidecars beside the sealed backup.
        let opened = Connection::open(destination)?;
        let journal_mode: String =
            opened.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("delete") {
            return Err(StateError::RollbackGuard(format!(
                "new backup could not be normalized to DELETE journal mode: {journal_mode}"
            )));
        }
        let integrity: String = opened.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            drop(opened);
            let _ = fs::remove_file(destination);
            return Err(StateError::RollbackGuard(format!(
                "new backup failed integrity check: {integrity}"
            )));
        }
        drop(opened);
        ensure_private_file(destination)?;
        Ok(destination.to_path_buf())
    }

    pub fn create_rollback_plan(
        connection: &Connection,
        backup_path: impl AsRef<Path>,
    ) -> StateResult<RollbackPlan> {
        let backup_path = canonical_existing_private_file(backup_path.as_ref(), "rollback backup")?;
        let live_status = Self::status(connection)?;
        let expected_last_event_sequence = last_event_sequence(connection)?;
        validate_live_rollback_guards(connection, expected_last_event_sequence)?;

        let backup_sha256 = sha256_file(&backup_path)?;
        let backup = open_read_only_database(&backup_path)?;
        let backup_status = validate_database_image(&backup, "rollback backup")?;
        let backup_last_event_sequence = last_event_sequence(&backup)?;
        if backup_status.current_version == 0 {
            return Err(StateError::RollbackGuard(
                "refusing to restore an unversioned empty database image".to_owned(),
            ));
        }
        if backup_status.current_version > live_status.current_version {
            return Err(StateError::RollbackGuard(format!(
                "backup schema {} is newer than live schema {}",
                backup_status.current_version, live_status.current_version
            )));
        }
        if backup_last_event_sequence != expected_last_event_sequence {
            return Err(StateError::RollbackGuard(format!(
                "rollback backup event sequence {backup_last_event_sequence} differs from live append-only event sequence {expected_last_event_sequence}"
            )));
        }
        if sha256_file(&backup_path)? != backup_sha256 {
            return Err(StateError::RollbackGuard(
                "rollback backup changed while its plan was being created".to_owned(),
            ));
        }

        let mut plan = RollbackPlan {
            plan_schema_version: ROLLBACK_PLAN_SCHEMA_VERSION,
            backup_path,
            backup_sha256,
            expected_current_schema_version: live_status.current_version,
            schema_version: backup_status.current_version,
            expected_last_event_sequence,
            backup_last_event_sequence,
            created_at: Utc::now(),
            integrity_hash: String::new(),
        };
        plan.integrity_hash = rollback_plan_hash(&plan)?;
        Ok(plan)
    }

    pub fn validate_rollback(connection: &Connection, plan: &RollbackPlan) -> StateResult<()> {
        plan.verify_integrity_hash()?;
        if plan.expected_last_event_sequence < 0 {
            return Err(StateError::RollbackGuard(
                "rollback plan contains a negative event sequence".to_owned(),
            ));
        }
        if plan.backup_last_event_sequence != plan.expected_last_event_sequence {
            return Err(StateError::RollbackGuard(format!(
                "sealed backup event sequence {} differs from the live plan sequence {}",
                plan.backup_last_event_sequence, plan.expected_last_event_sequence
            )));
        }
        let backup_path = canonical_existing_private_file(&plan.backup_path, "rollback backup")?;
        if backup_path != plan.backup_path {
            return Err(StateError::RollbackGuard(
                "rollback backup path no longer resolves to its sealed canonical path".to_owned(),
            ));
        }
        let observed_sha256 = sha256_file(&backup_path)?;
        if observed_sha256 != plan.backup_sha256 {
            return Err(StateError::RollbackGuard(format!(
                "rollback backup SHA-256 changed: expected {}, observed {observed_sha256}",
                plan.backup_sha256
            )));
        }

        let live_status = Self::status(connection)?;
        if live_status.current_version != plan.expected_current_schema_version {
            return Err(StateError::RollbackGuard(format!(
                "live schema changed from {} to {} after rollback planning",
                plan.expected_current_schema_version, live_status.current_version
            )));
        }
        validate_live_rollback_guards(connection, plan.expected_last_event_sequence)?;

        let backup = open_read_only_database(&backup_path)?;
        let backup_status = validate_database_image(&backup, "rollback backup")?;
        if backup_status.current_version != plan.schema_version {
            return Err(StateError::RollbackGuard(format!(
                "rollback backup schema changed from {} to {}",
                plan.schema_version, backup_status.current_version
            )));
        }
        let backup_sequence = last_event_sequence(&backup)?;
        if backup_sequence != plan.backup_last_event_sequence {
            return Err(StateError::RollbackGuard(format!(
                "rollback backup event sequence changed from {} to {backup_sequence}",
                plan.backup_last_event_sequence
            )));
        }
        if backup_status.current_version == 0
            || backup_status.current_version > live_status.current_version
        {
            return Err(StateError::RollbackGuard(format!(
                "rollback target schema {} is not a supported prior schema for live schema {}",
                backup_status.current_version, live_status.current_version
            )));
        }
        if sha256_file(&backup_path)? != plan.backup_sha256 {
            return Err(StateError::RollbackGuard(
                "rollback backup changed during validation".to_owned(),
            ));
        }
        Ok(())
    }

    /// Restores a sealed prior database image only after explicit, plan-bound approval.
    ///
    /// A full online backup of the live connection is created first. The live guards and
    /// sealed backup are then revalidated immediately before `rusqlite` restore. Any restore
    /// or post-restore verification failure triggers an automatic restore from that recovery
    /// backup; the recovery artifact is retained in every outcome.
    pub fn apply_rollback(
        connection: &mut Connection,
        plan: &RollbackPlan,
        expected_plan_hash: &str,
        approved_by: &str,
        recovery_backup_path: &Path,
    ) -> StateResult<RollbackApplyResult> {
        apply_rollback_with_target_verifier(
            connection,
            plan,
            expected_plan_hash,
            approved_by,
            recovery_backup_path,
            |restored, expected_schema| verify_restored_database(restored, expected_schema, None),
        )
    }
}

fn apply_rollback_with_target_verifier<F>(
    connection: &mut Connection,
    plan: &RollbackPlan,
    expected_plan_hash: &str,
    approved_by: &str,
    recovery_backup_path: &Path,
    verify_target: F,
) -> StateResult<RollbackApplyResult>
where
    F: FnOnce(&Connection, u32) -> StateResult<(MigrationStatus, i64)>,
{
    validate_approval(approved_by)?;
    validate_sha256("expected_plan_hash", expected_plan_hash)?;
    plan.verify_integrity_hash()?;
    if expected_plan_hash != plan.integrity_hash {
        return Err(StateError::RollbackGuard(
            "explicitly approved plan hash does not match the sealed rollback plan".to_owned(),
        ));
    }
    MigrationManager::validate_rollback(connection, plan)?;
    reject_symlink_components(recovery_backup_path)?;
    if recovery_backup_path == plan.backup_path {
        return Err(StateError::RollbackGuard(
            "recovery backup path must differ from the rollback source".to_owned(),
        ));
    }

    let recovery_backup_path = MigrationManager::backup(connection, recovery_backup_path)?;
    let recovery_backup_path =
        canonical_existing_private_file(&recovery_backup_path, "recovery backup")?;
    let recovery_backup_sha256 = sha256_file(&recovery_backup_path)?;
    verify_recovery_backup(
        &recovery_backup_path,
        plan.expected_current_schema_version,
        plan.expected_last_event_sequence,
        &recovery_backup_sha256,
    )?;

    // The online backup can take time. Re-evaluate every immutable-plan, event-sequence,
    // active-task, and active-lease guard at the last safe boundary before replacement.
    MigrationManager::validate_rollback(connection, plan)?;

    let rollback_outcome = restore_database(connection, &plan.backup_path)
        .and_then(|()| verify_target(connection, plan.schema_version))
        .and_then(|(status, reported_sequence)| {
            let observed_sequence = last_event_sequence(connection)?;
            if reported_sequence != observed_sequence {
                return Err(StateError::RollbackGuard(format!(
                    "post-restore verifier reported event sequence {reported_sequence}, but SQLite contains {observed_sequence}"
                )));
            }
            if observed_sequence != plan.backup_last_event_sequence {
                return Err(StateError::RollbackGuard(format!(
                    "restored event sequence {observed_sequence} differs from sealed backup sequence {}",
                    plan.backup_last_event_sequence
                )));
            }
            Ok((status, observed_sequence))
        });
    match rollback_outcome {
        Ok((status, restored_last_event_sequence)) => Ok(RollbackApplyResult {
            plan_hash: plan.integrity_hash.clone(),
            approved_by: approved_by.trim().to_owned(),
            restored_schema_version: status.current_version,
            restored_last_event_sequence,
            recovery_backup_path,
            recovery_backup_sha256,
            completed_at: Utc::now(),
        }),
        Err(rollback_error) => {
            let recovery_outcome = restore_database(connection, &recovery_backup_path)
                .and_then(|()| {
                    verify_restored_database(
                        connection,
                        plan.expected_current_schema_version,
                        Some(plan.expected_last_event_sequence),
                    )
                })
                .and_then(|_| {
                    let observed = sha256_file(&recovery_backup_path)?;
                    if observed != recovery_backup_sha256 {
                        return Err(StateError::RollbackGuard(
                            "recovery backup changed during automatic recovery".to_owned(),
                        ));
                    }
                    Ok(())
                });
            match recovery_outcome {
                Ok(()) => Err(StateError::RollbackGuard(format!(
                    "rollback restore or verification failed ({rollback_error}); the live database was automatically restored from {}",
                    recovery_backup_path.display()
                ))),
                Err(recovery_error) => Err(StateError::RollbackGuard(format!(
                    "rollback restore or verification failed ({rollback_error}); automatic recovery from {} also failed ({recovery_error}); the recovery backup was retained for administrator repair",
                    recovery_backup_path.display()
                ))),
            }
        }
    }
}

fn restore_database(connection: &mut Connection, source: &Path) -> StateResult<()> {
    reject_symlink_components(source)?;
    connection.restore(MAIN_DB, source, None::<fn(rusqlite::backup::Progress)>)?;
    configure_connection(connection)
}

fn verify_restored_database(
    connection: &Connection,
    expected_schema_version: u32,
    expected_event_sequence: Option<i64>,
) -> StateResult<(MigrationStatus, i64)> {
    let status = validate_database_image(connection, "restored live database")?;
    if status.current_version != expected_schema_version {
        return Err(StateError::RollbackGuard(format!(
            "restored schema version {} differs from sealed version {expected_schema_version}",
            status.current_version
        )));
    }
    let restored_sequence = last_event_sequence(connection)?;
    if let Some(expected) = expected_event_sequence
        && restored_sequence != expected
    {
        return Err(StateError::RollbackGuard(format!(
            "restored recovery event sequence {restored_sequence} differs from expected {expected}"
        )));
    }
    Ok((status, restored_sequence))
}

fn verify_recovery_backup(
    path: &Path,
    expected_schema_version: u32,
    expected_event_sequence: i64,
    expected_sha256: &str,
) -> StateResult<()> {
    let backup = open_read_only_database(path)?;
    verify_restored_database(
        &backup,
        expected_schema_version,
        Some(expected_event_sequence),
    )?;
    let observed = sha256_file(path)?;
    if observed != expected_sha256 {
        return Err(StateError::RollbackGuard(
            "recovery backup changed while it was being verified".to_owned(),
        ));
    }
    Ok(())
}

fn validate_database_image(
    connection: &Connection,
    description: &str,
) -> StateResult<MigrationStatus> {
    let mut statement = connection.prepare("PRAGMA integrity_check")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let findings = rows.collect::<Result<Vec<_>, _>>()?;
    if findings.as_slice() != ["ok"] {
        return Err(StateError::RollbackGuard(format!(
            "{description} integrity_check failed: {}",
            findings.join("; ")
        )));
    }
    let foreign_key_failures: i64 =
        connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_failures != 0 {
        return Err(StateError::RollbackGuard(format!(
            "{description} has {foreign_key_failures} foreign-key violations"
        )));
    }
    MigrationManager::status(connection)
}

fn validate_live_rollback_guards(
    connection: &Connection,
    expected_last_event_sequence: i64,
) -> StateResult<()> {
    let current_sequence = last_event_sequence(connection)?;
    if current_sequence != expected_last_event_sequence {
        return Err(StateError::RollbackGuard(format!(
            "event sequence advanced from {expected_last_event_sequence} to {current_sequence}"
        )));
    }
    let active_tasks = count_where_nonzero(
        connection,
        "tasks",
        "SELECT count(*) FROM tasks WHERE state NOT IN ('completed', 'failed', 'cancelled')",
    )?;
    if active_tasks != 0 {
        return Err(StateError::RollbackGuard(format!(
            "{active_tasks} tasks are still active"
        )));
    }
    let active_coordinator_leases = count_where_nonzero(
        connection,
        "coordinator_leases",
        "SELECT count(*) FROM coordinator_leases WHERE released_at IS NULL",
    )?;
    let active_worker_leases = count_where_nonzero(
        connection,
        "worker_leases",
        "SELECT count(*) FROM worker_leases WHERE released_at IS NULL",
    )?;
    if active_coordinator_leases != 0 || active_worker_leases != 0 {
        return Err(StateError::RollbackGuard(format!(
            "rollback requires released leases; found {active_coordinator_leases} coordinator and {active_worker_leases} worker leases"
        )));
    }
    Ok(())
}

fn count_where_nonzero(connection: &Connection, table: &str, query: &str) -> StateResult<i64> {
    if table_exists(connection, table)? {
        connection
            .query_row(query, [], |row| row.get(0))
            .map_err(StateError::from)
    } else {
        Ok(0)
    }
}

fn validate_approval(approved_by: &str) -> StateResult<()> {
    let approved_by = approved_by.trim();
    if approved_by.is_empty() {
        return Err(StateError::RollbackGuard(
            "rollback apply requires a non-empty approved_by audit identity".to_owned(),
        ));
    }
    if approved_by.len() > 256 || approved_by.chars().any(char::is_control) {
        return Err(StateError::RollbackGuard(
            "approved_by must be at most 256 bytes and contain no control characters".to_owned(),
        ));
    }
    Ok(())
}

fn canonical_existing_private_file(path: &Path, description: &str) -> StateResult<PathBuf> {
    reject_symlink_components(path)?;
    let canonical = fs::canonicalize(path).map_err(|error| StateError::io(path, error))?;
    reject_symlink_components(&canonical)?;
    let metadata =
        fs::symlink_metadata(&canonical).map_err(|error| StateError::io(&canonical, error))?;
    if !metadata.is_file() {
        return Err(StateError::RollbackGuard(format!(
            "{description} is not a regular file: {}",
            canonical.display()
        )));
    }
    verify_private_file(&canonical)?;
    Ok(canonical)
}

fn open_read_only_database(path: &Path) -> StateResult<Connection> {
    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(StateError::from)
}

fn sha256_file(path: &Path) -> StateResult<String> {
    let mut file = fs::File::open(path).map_err(|error| StateError::io(path, error))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| StateError::io(path, error))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn validate_sha256(field: &str, value: &str) -> StateResult<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StateError::RollbackGuard(format!(
            "{field} must be a lowercase 64-character SHA-256 value"
        )));
    }
    Ok(())
}

#[derive(Serialize)]
struct RollbackPlanHashMaterial<'a> {
    plan_schema_version: u32,
    backup_path: &'a Path,
    backup_sha256: &'a str,
    expected_current_schema_version: u32,
    schema_version: u32,
    expected_last_event_sequence: i64,
    backup_last_event_sequence: i64,
    created_at: &'a DateTime<Utc>,
}

fn rollback_plan_hash(plan: &RollbackPlan) -> StateResult<String> {
    let material = RollbackPlanHashMaterial {
        plan_schema_version: plan.plan_schema_version,
        backup_path: &plan.backup_path,
        backup_sha256: &plan.backup_sha256,
        expected_current_schema_version: plan.expected_current_schema_version,
        schema_version: plan.schema_version,
        expected_last_event_sequence: plan.expected_last_event_sequence,
        backup_last_event_sequence: plan.backup_last_event_sequence,
        created_at: &plan.created_at,
    };
    let encoded = serde_json::to_vec(&material)?;
    let mut digest = Sha256::new();
    // Persisted v1 plans were sealed with this historical product identifier.
    // Renaming the domain separator would invalidate approved rollback evidence.
    digest.update(b"codex-orchestrator/rollback-plan/v1\0");
    digest.update(encoded);
    Ok(hex::encode(digest.finalize()))
}

pub(crate) fn configure_connection(connection: &Connection) -> StateResult<()> {
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;\
         PRAGMA journal_mode = WAL;\
         PRAGMA synchronous = FULL;\
         PRAGMA temp_store = MEMORY;\
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(())
}

fn load_applied(connection: &Connection) -> StateResult<Vec<AppliedMigration>> {
    if !table_exists(connection, "schema_migrations")? {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(
        "SELECT version, name, checksum, applied_at FROM schema_migrations ORDER BY version",
    )?;
    let rows = statement.query_map([], |row| {
        let timestamp: String = row.get(3)?;
        let applied_at = DateTime::parse_from_rfc3339(&timestamp)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
        Ok(AppliedMigration {
            version: row.get(0)?,
            name: row.get(1)?,
            checksum: row.get(2)?,
            applied_at,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StateError::from)
}

fn validate_sequence_and_checksums(applied: &[AppliedMigration]) -> StateResult<()> {
    for (index, value) in applied.iter().enumerate() {
        let expected_version = u32::try_from(index)
            .map_err(|_| StateError::RollbackGuard("migration index overflow".to_owned()))?
            + 1;
        if value.version != expected_version {
            return Err(StateError::MigrationGap {
                version: value.version,
                expected: expected_version,
            });
        }
        let (_, name, sql) = migration(value.version)?;
        if value.name != name || value.checksum != checksum(sql) {
            return Err(StateError::MigrationChecksum {
                version: value.version,
            });
        }
    }
    Ok(())
}

fn migration(version: u32) -> StateResult<(u32, &'static str, &'static str)> {
    MIGRATIONS
        .iter()
        .copied()
        .find(|(candidate, _, _)| *candidate == version)
        .ok_or(StateError::MigrationGap {
            version,
            expected: version,
        })
}

fn checksum(sql: &str) -> String {
    hex::encode(Sha256::digest(sql.as_bytes()))
}

fn table_exists(connection: &Connection, name: &str) -> StateResult<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn last_event_sequence(connection: &Connection) -> StateResult<i64> {
    if !table_exists(connection, "task_events")? {
        return Ok(0);
    }
    connection
        .query_row(
            "SELECT coalesce(max(sequence), 0) FROM task_events",
            [],
            |row| row.get(0),
        )
        .map_err(StateError::from)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rusqlite::{Connection, params};

    use super::{
        MigrationManager, ROLLBACK_PLAN_SCHEMA_VERSION, RollbackPlan, STATE_SCHEMA_VERSION,
        apply_rollback_with_target_verifier, configure_connection,
    };
    use crate::{StateError, StateResult};

    #[test]
    fn applies_all_migrations_in_order_and_is_idempotent() {
        let mut connection =
            Connection::open_in_memory().unwrap_or_else(|error| panic!("in-memory db: {error}"));
        configure_connection(&connection).unwrap_or_else(|error| panic!("configure: {error}"));
        let status = MigrationManager::apply(&mut connection)
            .unwrap_or_else(|error| panic!("migrate: {error}"));
        assert_eq!(status.current_version, STATE_SCHEMA_VERSION);
        assert!(status.pending_versions.is_empty());
        let second = MigrationManager::apply(&mut connection)
            .unwrap_or_else(|error| panic!("second migrate: {error}"));
        assert_eq!(second.current_version, STATE_SCHEMA_VERSION);
    }

    #[test]
    fn dry_run_does_not_mutate_source() {
        let connection =
            Connection::open_in_memory().unwrap_or_else(|error| panic!("in-memory db: {error}"));
        configure_connection(&connection).unwrap_or_else(|error| panic!("configure: {error}"));
        let status = MigrationManager::dry_run(&connection)
            .unwrap_or_else(|error| panic!("dry-run: {error}"));
        assert_eq!(status.current_version, STATE_SCHEMA_VERSION);
        let live =
            MigrationManager::status(&connection).unwrap_or_else(|error| panic!("status: {error}"));
        assert_eq!(live.current_version, 0);
    }

    #[test]
    fn rollback_plan_seals_backup_and_rejects_changed_or_future_metadata() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let live = migrated_database(&temporary.path().join("live.db"))?;
        install_marker(&live, "before")?;
        let backup_path = temporary.path().join("prior.db");
        MigrationManager::backup(&live, &backup_path)?;

        let plan = MigrationManager::create_rollback_plan(&live, backup_path)?;
        assert_eq!(plan.plan_schema_version, ROLLBACK_PLAN_SCHEMA_VERSION);
        assert_eq!(plan.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(plan.backup_sha256.len(), 64);
        assert_eq!(plan.integrity_hash.len(), 64);
        plan.verify_integrity_hash()?;

        let mut changed = plan.clone();
        changed.expected_last_event_sequence += 1;
        assert!(changed.verify_integrity_hash().is_err());

        let mut future = plan.clone();
        future.plan_schema_version += 1;
        assert!(future.verify_integrity_hash().is_err());

        let mut json = serde_json::to_value(&plan)?;
        json.as_object_mut()
            .ok_or_else(|| StateError::InvalidRecord("plan JSON was not an object".to_owned()))?
            .insert("future_field".to_owned(), serde_json::json!(true));
        assert!(serde_json::from_value::<RollbackPlan>(json).is_err());

        live.execute("INSERT INTO rollback_marker(value) VALUES ('after')", [])?;
        Ok(())
    }

    #[test]
    fn rollback_validation_rejects_backup_content_changes() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let live = migrated_database(&temporary.path().join("live.db"))?;
        install_marker(&live, "before")?;
        let backup_path = temporary.path().join("prior.db");
        MigrationManager::backup(&live, &backup_path)?;
        let plan = MigrationManager::create_rollback_plan(&live, backup_path.clone())?;

        let tampered = Connection::open(&backup_path)?;
        tampered.execute("INSERT INTO rollback_marker(value) VALUES ('tampered')", [])?;
        drop(tampered);

        let Err(error) = MigrationManager::validate_rollback(&live, &plan) else {
            return Err(StateError::InvalidRecord(
                "changed backup unexpectedly passed validation".to_owned(),
            ));
        };
        assert!(error.to_string().contains("SHA-256 changed"));
        Ok(())
    }

    #[test]
    fn rollback_plan_rejects_backup_behind_append_only_event_log() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let live = migrated_database(&temporary.path().join("live.db"))?;
        let backup_path = temporary.path().join("prior.db");
        MigrationManager::backup(&live, &backup_path)?;
        insert_audit_event(&live, "event-after-backup")?;

        let Err(error) = MigrationManager::create_rollback_plan(&live, backup_path) else {
            return Err(StateError::InvalidRecord(
                "event-stale backup unexpectedly produced a rollback plan".to_owned(),
            ));
        };
        assert!(
            error
                .to_string()
                .contains("differs from live append-only event sequence")
        );
        Ok(())
    }

    #[test]
    fn rollback_validation_rejects_active_tasks_and_advanced_events() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let live = migrated_database(&temporary.path().join("live.db"))?;
        let backup_path = temporary.path().join("prior.db");
        MigrationManager::backup(&live, &backup_path)?;
        let plan = MigrationManager::create_rollback_plan(&live, backup_path)?;

        live.execute(
            "INSERT INTO tasks( \
                task_id, schema_version, state, objective, original_request_redacted, \
                task_envelope_json, created_at, updated_at \
             ) VALUES ('task-active', 'state-v3', 'queued', 'objective', 'request', '{}', ?1, ?1)",
            [chrono::Utc::now().to_rfc3339()],
        )?;
        let Err(active_error) = MigrationManager::validate_rollback(&live, &plan) else {
            return Err(StateError::InvalidRecord(
                "active task unexpectedly passed rollback validation".to_owned(),
            ));
        };
        assert!(active_error.to_string().contains("still active"));
        live.execute("DELETE FROM tasks WHERE task_id = 'task-active'", [])?;

        insert_audit_event(&live, "event-1")?;
        let Err(sequence_error) = MigrationManager::validate_rollback(&live, &plan) else {
            return Err(StateError::InvalidRecord(
                "advanced sequence unexpectedly passed rollback validation".to_owned(),
            ));
        };
        assert!(
            sequence_error
                .to_string()
                .contains("event sequence advanced")
        );
        Ok(())
    }

    #[test]
    fn rollback_apply_requires_exact_hash_and_explicit_approval() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let mut live = migrated_database(&temporary.path().join("live.db"))?;
        let backup_path = temporary.path().join("prior.db");
        MigrationManager::backup(&live, &backup_path)?;
        let plan = MigrationManager::create_rollback_plan(&live, backup_path)?;

        let Err(empty_approval) = MigrationManager::apply_rollback(
            &mut live,
            &plan,
            &plan.integrity_hash,
            "  ",
            &temporary.path().join("empty-approval-recovery.db"),
        ) else {
            return Err(StateError::InvalidRecord(
                "empty rollback approval unexpectedly succeeded".to_owned(),
            ));
        };
        assert!(empty_approval.to_string().contains("non-empty approved_by"));

        let Err(wrong_hash) = MigrationManager::apply_rollback(
            &mut live,
            &plan,
            &"0".repeat(64),
            "enterprise-admin",
            &temporary.path().join("wrong-hash-recovery.db"),
        ) else {
            return Err(StateError::InvalidRecord(
                "wrong rollback plan hash unexpectedly succeeded".to_owned(),
            ));
        };
        assert!(wrong_hash.to_string().contains("does not match"));
        assert!(!temporary.path().join("wrong-hash-recovery.db").exists());
        Ok(())
    }

    #[test]
    fn rollback_apply_restores_prior_image_and_preserves_live_recovery_backup() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let mut live = migrated_database(&temporary.path().join("live.db"))?;
        install_marker(&live, "before")?;
        let backup_path = temporary.path().join("prior.db");
        MigrationManager::backup(&live, &backup_path)?;
        live.execute("INSERT INTO rollback_marker(value) VALUES ('after')", [])?;
        let plan = MigrationManager::create_rollback_plan(&live, backup_path)?;
        let recovery_path = temporary.path().join("live-recovery.db");

        let result = MigrationManager::apply_rollback(
            &mut live,
            &plan,
            &plan.integrity_hash,
            "enterprise-admin",
            &recovery_path,
        )?;

        assert_eq!(result.restored_schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(result.restored_last_event_sequence, 0);
        assert_eq!(result.approved_by, "enterprise-admin");
        assert!(result.recovery_backup_path.exists());
        assert_eq!(marker_count(&live)?, 1);
        let recovery = Connection::open_with_flags(
            &result.recovery_backup_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        assert_eq!(marker_count(&recovery)?, 2);
        Ok(())
    }

    #[test]
    fn failed_post_restore_verification_automatically_recovers_live_database() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let mut live = migrated_database(&temporary.path().join("live.db"))?;
        install_marker(&live, "before")?;
        let backup_path = temporary.path().join("prior.db");
        MigrationManager::backup(&live, &backup_path)?;
        live.execute("INSERT INTO rollback_marker(value) VALUES ('after')", [])?;
        let plan = MigrationManager::create_rollback_plan(&live, backup_path)?;
        let recovery_path = temporary.path().join("automatic-recovery.db");

        let Err(error) = apply_rollback_with_target_verifier(
            &mut live,
            &plan,
            &plan.integrity_hash,
            "enterprise-admin",
            &recovery_path,
            |_, _| {
                Err(StateError::RollbackGuard(
                    "injected post-restore verification failure".to_owned(),
                ))
            },
        ) else {
            return Err(StateError::InvalidRecord(
                "injected post-restore failure unexpectedly succeeded".to_owned(),
            ));
        };

        assert!(error.to_string().contains("automatically restored"));
        assert!(recovery_path.exists());
        assert_eq!(marker_count(&live)?, 2);
        Ok(())
    }

    fn migrated_database(path: &Path) -> StateResult<Connection> {
        let mut connection = Connection::open(path)?;
        configure_connection(&connection)?;
        MigrationManager::apply(&mut connection)?;
        Ok(connection)
    }

    fn install_marker(connection: &Connection, value: &str) -> StateResult<()> {
        connection.execute_batch(
            "CREATE TABLE rollback_marker(value TEXT PRIMARY KEY NOT NULL) STRICT;",
        )?;
        connection.execute("INSERT INTO rollback_marker(value) VALUES (?1)", [value])?;
        Ok(())
    }

    fn marker_count(connection: &Connection) -> StateResult<i64> {
        connection
            .query_row("SELECT count(*) FROM rollback_marker", [], |row| row.get(0))
            .map_err(StateError::from)
    }

    fn insert_audit_event(connection: &Connection, event_id: &str) -> StateResult<()> {
        connection.execute(
            "INSERT INTO task_events( \
                event_id, task_id, event_type, schema_version, occurred_at, event_json, \
                previous_hash, event_hash, exported_at \
             ) VALUES (?1, NULL, 'compatibility_warning', 'state-v3', ?2, '{}', \
                NULL, ?3, NULL)",
            params![event_id, chrono::Utc::now().to_rfc3339(), "0".repeat(64)],
        )?;
        Ok(())
    }
}
