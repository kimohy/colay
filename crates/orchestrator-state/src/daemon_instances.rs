use std::{path::Path, str::FromStr as _};

use chrono::{DateTime, TimeDelta, Utc};
use orchestrator_domain::DaemonInstanceId;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension as _, Row, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};

use crate::{Database, StateError, StateResult, reject_symlink_components};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonPhase {
    Booting,
    Probing,
    Online,
    Failed,
}

impl DaemonPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Booting => "booting",
            Self::Probing => "probing",
            Self::Online => "online",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInstance {
    pub instance_id: DaemonInstanceId,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
    pub lease_expires_at: DateTime<Utc>,
    pub phase: DaemonPhase,
    pub startup_error: Option<String>,
    pub stop_requested_at: Option<DateTime<Utc>>,
    pub released_at: Option<DateTime<Utc>>,
    pub executable_path: Option<String>,
    pub build_version: Option<String>,
    pub build_target: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DaemonLeaseRequest {
    pub instance_id: DaemonInstanceId,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub ttl: TimeDelta,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "instance", rename_all = "snake_case")]
pub enum DaemonStatus {
    Stopped,
    Booting(DaemonInstance),
    Probing(DaemonInstance),
    Online(DaemonInstance),
    Failed(DaemonInstance),
    Stale(DaemonInstance),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DaemonOnlineIdentity {
    pub instance_id: DaemonInstanceId,
    pub pid: u32,
}

/// Reads only the online owner identity from an already-existing expected state database.
///
/// This opens `SQLite` read-only, performs no migration or schema mutation, and uses one read
/// transaction so a concurrent WAL writer cannot mix daemon-instance revisions.
pub fn read_online_daemon_identity(
    database_path: &Path,
    now: DateTime<Utc>,
) -> StateResult<Option<DaemonOnlineIdentity>> {
    match database_path.try_exists() {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(error) => return Err(StateError::io(database_path, error)),
    }
    reject_symlink_components(database_path)?;
    let mut connection = Connection::open_with_flags(
        database_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let schema_version: u32 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if schema_version > crate::STATE_SCHEMA_VERSION {
        return Err(StateError::FutureSchema {
            found: schema_version,
            supported: crate::STATE_SCHEMA_VERSION,
        });
    }
    if schema_version < 4 {
        transaction.commit()?;
        return Ok(None);
    }
    let identity_row = load_stored_daemon_identity(&transaction, schema_version)?;
    let identity = identity_row
        .as_ref()
        .map(|identity| parse_online_daemon_identity(identity, now))
        .transpose()?
        .flatten();
    transaction.commit()?;
    Ok(identity)
}

struct StoredDaemonIdentity {
    instance_id: String,
    pid: i64,
    lease_expires_at: String,
    phase: Option<String>,
}

fn load_stored_daemon_identity(
    transaction: &Transaction<'_>,
    schema_version: u32,
) -> StateResult<Option<StoredDaemonIdentity>> {
    let active_count: u32 = transaction.query_row(
        "SELECT count(*) FROM daemon_instances WHERE released_at IS NULL",
        [],
        |row| row.get(0),
    )?;
    if active_count > 1 {
        return Err(StateError::InvalidRecord(
            "multiple unreleased daemon owners exist in the expected state database".to_owned(),
        ));
    }
    if schema_version >= 9 {
        transaction
            .query_row(
                "SELECT instance_id, pid, lease_expires_at, phase
                 FROM daemon_instances WHERE released_at IS NULL",
                [],
                |row| {
                    Ok(StoredDaemonIdentity {
                        instance_id: row.get(0)?,
                        pid: row.get(1)?,
                        lease_expires_at: row.get(2)?,
                        phase: Some(row.get(3)?),
                    })
                },
            )
            .optional()
            .map_err(StateError::from)
    } else {
        transaction
            .query_row(
                "SELECT instance_id, pid, lease_expires_at
                 FROM daemon_instances WHERE released_at IS NULL",
                [],
                |row| {
                    Ok(StoredDaemonIdentity {
                        instance_id: row.get(0)?,
                        pid: row.get(1)?,
                        lease_expires_at: row.get(2)?,
                        phase: None,
                    })
                },
            )
            .optional()
            .map_err(StateError::from)
    }
}

fn parse_online_daemon_identity(
    identity: &StoredDaemonIdentity,
    now: DateTime<Utc>,
) -> StateResult<Option<DaemonOnlineIdentity>> {
    let instance_id = DaemonInstanceId::from_str(&identity.instance_id).map_err(|error| {
        StateError::InvalidRecord(format!(
            "invalid daemon instance identifier in expected state database: {error}"
        ))
    })?;
    let pid = u32::try_from(identity.pid).map_err(|error| {
        StateError::InvalidRecord(format!(
            "invalid daemon owner PID in expected state database: {error}"
        ))
    })?;
    if pid == 0 {
        return Err(StateError::InvalidRecord(
            "daemon owner PID in expected state database is zero".to_owned(),
        ));
    }
    let lease_expires_at = DateTime::parse_from_rfc3339(&identity.lease_expires_at)
        .map_err(|error| {
            StateError::InvalidRecord(format!(
                "invalid daemon lease expiry in expected state database: {error}"
            ))
        })?
        .with_timezone(&Utc);
    let online = match identity.phase.as_deref() {
        None | Some("online") => true,
        Some("booting" | "probing" | "failed") => false,
        Some(phase) => {
            return Err(StateError::InvalidRecord(format!(
                "invalid daemon phase in expected state database: {phase}"
            )));
        }
    };
    Ok((online && lease_expires_at > now).then_some(DaemonOnlineIdentity { instance_id, pid }))
}

impl Database {
    pub fn acquire_daemon_lease(
        &self,
        request: &DaemonLeaseRequest,
    ) -> StateResult<DaemonInstance> {
        self.acquire_daemon_lease_with_phase(request, DaemonPhase::Online)
    }

    pub fn acquire_daemon_startup_lease(
        &self,
        request: &DaemonLeaseRequest,
    ) -> StateResult<DaemonInstance> {
        self.acquire_daemon_lease_with_phase(request, DaemonPhase::Booting)
    }

    fn acquire_daemon_lease_with_phase(
        &self,
        request: &DaemonLeaseRequest,
        phase: DaemonPhase,
    ) -> StateResult<DaemonInstance> {
        validate_pid_and_ttl(request.pid, request.ttl)?;
        let lease_expires_at = request
            .started_at
            .checked_add_signed(request.ttl)
            .ok_or_else(|| StateError::InvalidRecord("daemon lease expiry overflow".to_owned()))?;
        let instance = DaemonInstance {
            instance_id: request.instance_id,
            pid: request.pid,
            started_at: request.started_at,
            heartbeat_at: request.started_at,
            lease_expires_at,
            phase,
            startup_error: None,
            stop_requested_at: None,
            released_at: None,
            executable_path: None,
            build_version: None,
            build_target: None,
        };

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE daemon_instances SET released_at = ?1
             WHERE released_at IS NULL AND lease_expires_at <= ?1",
            [request.started_at.to_rfc3339()],
        )?;
        let active: Option<String> = transaction
            .query_row(
                "SELECT instance_id FROM daemon_instances WHERE released_at IS NULL LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(active) = active {
            return Err(StateError::OptimisticConflict {
                entity: format!("repository daemon lease owned by {active}"),
            });
        }
        transaction.execute(
            "INSERT INTO daemon_instances(
                instance_id, pid, started_at, heartbeat_at, lease_expires_at,
                phase, startup_error, stop_requested_at, released_at
             ) VALUES (?1, ?2, ?3, ?3, ?4, ?5, NULL, NULL, NULL)",
            params![
                request.instance_id.to_string(),
                i64::from(request.pid),
                request.started_at.to_rfc3339(),
                lease_expires_at.to_rfc3339(),
                phase.as_str(),
            ],
        )?;
        transaction.commit()?;
        Ok(instance)
    }

    pub fn transition_daemon_phase(
        &self,
        instance_id: DaemonInstanceId,
        phase: DaemonPhase,
        startup_error: Option<&str>,
    ) -> StateResult<DaemonInstance> {
        let allowed_source = match phase {
            DaemonPhase::Probing => "phase = 'booting'",
            DaemonPhase::Online => "phase = 'probing'",
            DaemonPhase::Failed => "phase IN ('booting', 'probing')",
            DaemonPhase::Booting => {
                return Err(StateError::InvalidRecord(
                    "daemon phase cannot transition back to booting".to_owned(),
                ));
            }
        };
        match (phase, startup_error) {
            (DaemonPhase::Failed, Some(error)) if !error.trim().is_empty() => {}
            (DaemonPhase::Failed, _) => {
                return Err(StateError::InvalidRecord(
                    "failed daemon phase requires a startup diagnostic".to_owned(),
                ));
            }
            (_, Some(_)) => {
                return Err(StateError::InvalidRecord(
                    "startup diagnostic is only valid for failed daemon phase".to_owned(),
                ));
            }
            (_, None) => {}
        }
        let sql = format!(
            "UPDATE daemon_instances SET phase = ?1, startup_error = ?2 \
             WHERE instance_id = ?3 AND released_at IS NULL AND {allowed_source}"
        );
        let changed = self.lock()?.execute(
            &sql,
            params![phase.as_str(), startup_error, instance_id.to_string()],
        )?;
        if changed != 1 {
            return Err(ownership_error(instance_id));
        }
        self.load_daemon_instance(instance_id)?.ok_or_else(|| {
            StateError::InvalidRecord(format!(
                "transitioned daemon instance {instance_id} disappeared"
            ))
        })
    }

    pub fn heartbeat_daemon(
        &self,
        instance_id: DaemonInstanceId,
        heartbeat_at: DateTime<Utc>,
        ttl: TimeDelta,
    ) -> StateResult<DaemonInstance> {
        validate_pid_and_ttl(1, ttl)?;
        let lease_expires_at = heartbeat_at
            .checked_add_signed(ttl)
            .ok_or_else(|| StateError::InvalidRecord("daemon lease expiry overflow".to_owned()))?;
        // A delayed owner may renew after expiry until acquisition atomically releases this row.
        // Once takeover wins, `released_at IS NULL` still rejects the former owner's heartbeat.
        let changed = self.lock()?.execute(
            "UPDATE daemon_instances SET heartbeat_at = ?1, lease_expires_at = ?2
             WHERE instance_id = ?3 AND released_at IS NULL
               AND heartbeat_at <= ?1",
            params![
                heartbeat_at.to_rfc3339(),
                lease_expires_at.to_rfc3339(),
                instance_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(ownership_error(instance_id));
        }
        self.load_daemon_instance(instance_id)?.ok_or_else(|| {
            StateError::InvalidRecord(format!(
                "heartbeaten daemon instance {instance_id} disappeared"
            ))
        })
    }

    pub fn daemon_status(&self, now: DateTime<Utc>) -> StateResult<DaemonStatus> {
        let connection = self.lock()?;
        let instance = connection
            .query_row(
                "SELECT instance_id, pid, started_at, heartbeat_at, lease_expires_at,
                        phase, startup_error, stop_requested_at, released_at,
                        executable_path, build_version, build_target
                 FROM daemon_instances WHERE released_at IS NULL
                 ORDER BY started_at DESC LIMIT 1",
                [],
                map_daemon_instance,
            )
            .optional()?;
        Ok(match instance {
            None => DaemonStatus::Stopped,
            Some(instance) if instance.lease_expires_at > now => match instance.phase {
                DaemonPhase::Booting => DaemonStatus::Booting(instance),
                DaemonPhase::Probing => DaemonStatus::Probing(instance),
                DaemonPhase::Online => DaemonStatus::Online(instance),
                DaemonPhase::Failed => DaemonStatus::Failed(instance),
            },
            Some(instance) => DaemonStatus::Stale(instance),
        })
    }

    pub fn daemon_startup_diagnostic_for_pid(&self, pid: u32) -> StateResult<Option<String>> {
        self.lock()?
            .query_row(
                "SELECT startup_error FROM daemon_instances \
                 WHERE pid = ?1 AND startup_error IS NOT NULL \
                 ORDER BY started_at DESC LIMIT 1",
                [i64::from(pid)],
                |row| row.get(0),
            )
            .optional()
            .map_err(StateError::from)
    }

    pub fn record_daemon_runtime_identity(
        &self,
        instance_id: DaemonInstanceId,
        executable_path: &str,
        build_version: &str,
        build_target: &str,
    ) -> StateResult<DaemonInstance> {
        if [executable_path, build_version, build_target]
            .iter()
            .any(|value| value.trim().is_empty())
        {
            return Err(StateError::InvalidRecord(
                "daemon runtime identity fields must not be blank".to_owned(),
            ));
        }
        let changed = self.lock()?.execute(
            "UPDATE daemon_instances
             SET executable_path = ?1, build_version = ?2, build_target = ?3
             WHERE instance_id = ?4 AND released_at IS NULL",
            params![
                executable_path,
                build_version,
                build_target,
                instance_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(ownership_error(instance_id));
        }
        self.load_daemon_instance(instance_id)?.ok_or_else(|| {
            StateError::InvalidRecord(format!(
                "daemon instance {instance_id} disappeared after identity update"
            ))
        })
    }

    pub fn request_daemon_stop(
        &self,
        instance_id: DaemonInstanceId,
        requested_at: DateTime<Utc>,
    ) -> StateResult<()> {
        let changed = self.lock()?.execute(
            "UPDATE daemon_instances SET stop_requested_at = COALESCE(stop_requested_at, ?1)
             WHERE instance_id = ?2 AND released_at IS NULL",
            params![requested_at.to_rfc3339(), instance_id.to_string()],
        )?;
        if changed != 1 {
            return Err(ownership_error(instance_id));
        }
        Ok(())
    }

    pub fn daemon_stop_requested(&self, instance_id: DaemonInstanceId) -> StateResult<bool> {
        let connection = self.lock()?;
        let requested: Option<i64> = connection
            .query_row(
                "SELECT stop_requested_at IS NOT NULL FROM daemon_instances
                 WHERE instance_id = ?1 AND released_at IS NULL",
                [instance_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(requested == Some(1))
    }

    pub fn release_daemon(
        &self,
        instance_id: DaemonInstanceId,
        released_at: DateTime<Utc>,
    ) -> StateResult<()> {
        let changed = self.lock()?.execute(
            "UPDATE daemon_instances SET released_at = ?1
             WHERE instance_id = ?2 AND released_at IS NULL",
            params![released_at.to_rfc3339(), instance_id.to_string()],
        )?;
        if changed != 1 {
            return Err(ownership_error(instance_id));
        }
        Ok(())
    }

    fn load_daemon_instance(
        &self,
        instance_id: DaemonInstanceId,
    ) -> StateResult<Option<DaemonInstance>> {
        self.lock()?
            .query_row(
                "SELECT instance_id, pid, started_at, heartbeat_at, lease_expires_at,
                        phase, startup_error, stop_requested_at, released_at,
                        executable_path, build_version, build_target
                 FROM daemon_instances WHERE instance_id = ?1",
                [instance_id.to_string()],
                map_daemon_instance,
            )
            .optional()
            .map_err(StateError::from)
    }
}

fn validate_pid_and_ttl(pid: u32, ttl: TimeDelta) -> StateResult<()> {
    if pid == 0 {
        return Err(StateError::InvalidRecord(
            "daemon PID must be positive".to_owned(),
        ));
    }
    if ttl <= TimeDelta::zero() {
        return Err(StateError::InvalidRecord(
            "daemon lease TTL must be positive".to_owned(),
        ));
    }
    Ok(())
}

fn ownership_error(instance_id: DaemonInstanceId) -> StateError {
    StateError::OptimisticConflict {
        entity: format!("daemon instance {instance_id}"),
    }
}

fn map_daemon_instance(row: &Row<'_>) -> rusqlite::Result<DaemonInstance> {
    let instance_id: String = row.get(0)?;
    let pid: i64 = row.get(1)?;
    let started_at: String = row.get(2)?;
    let heartbeat_at: String = row.get(3)?;
    let lease_expires_at: String = row.get(4)?;
    let phase: String = row.get(5)?;
    let startup_error: Option<String> = row.get(6)?;
    let stop_requested_at: Option<String> = row.get(7)?;
    let released_at: Option<String> = row.get(8)?;
    let executable_path: Option<String> = row.get(9)?;
    let build_version: Option<String> = row.get(10)?;
    let build_target: Option<String> = row.get(11)?;
    Ok(DaemonInstance {
        instance_id: DaemonInstanceId::from_str(&instance_id)
            .map_err(|error| conversion_error(0, error))?,
        pid: u32::try_from(pid).map_err(|error| conversion_error(1, error))?,
        started_at: parse_timestamp(&started_at, 2)?,
        heartbeat_at: parse_timestamp(&heartbeat_at, 3)?,
        lease_expires_at: parse_timestamp(&lease_expires_at, 4)?,
        phase: parse_phase(&phase, 5)?,
        startup_error,
        stop_requested_at: stop_requested_at
            .map(|value| parse_timestamp(&value, 7))
            .transpose()?,
        released_at: released_at
            .map(|value| parse_timestamp(&value, 8))
            .transpose()?,
        executable_path,
        build_version,
        build_target,
    })
}

fn parse_phase(value: &str, column: usize) -> rusqlite::Result<DaemonPhase> {
    match value {
        "booting" => Ok(DaemonPhase::Booting),
        "probing" => Ok(DaemonPhase::Probing),
        "online" => Ok(DaemonPhase::Online),
        "failed" => Ok(DaemonPhase::Failed),
        _ => Err(conversion_error(
            column,
            StateError::InvalidRecord(format!("unknown daemon phase {value}")),
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
    use orchestrator_domain::DaemonInstanceId;

    use super::{
        DaemonLeaseRequest, DaemonOnlineIdentity, DaemonPhase, DaemonStatus,
        read_online_daemon_identity,
    };
    use crate::{Database, StateError, StateResult};

    fn timestamp() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0)
            .single()
            .unwrap_or_else(|| panic!("fixed timestamp must be valid"))
    }

    fn request(started_at: chrono::DateTime<Utc>) -> DaemonLeaseRequest {
        DaemonLeaseRequest {
            instance_id: DaemonInstanceId::new(),
            pid: 42,
            started_at,
            ttl: TimeDelta::seconds(10),
        }
    }

    fn migrated_database() -> Database {
        let database =
            Database::open_in_memory().unwrap_or_else(|error| panic!("database: {error}"));
        database
            .migrate_with_backup(std::path::Path::new("unused"))
            .unwrap_or_else(|error| panic!("migrations: {error}"));
        database
    }

    #[test]
    fn read_only_daemon_identity_requires_online_unexpired_expected_database() -> StateResult<()> {
        let directory = tempfile::tempdir().map_err(|error| StateError::io("temp", error))?;
        let root = std::fs::canonicalize(directory.path())
            .map_err(|error| StateError::io("temp", error))?;
        let path = root.join("state.db");
        let database = Database::open(&path)?;
        database.migrate_with_backup(&root.join("backups"))?;
        let lease = request(timestamp());
        database.acquire_daemon_lease(&lease)?;

        assert_eq!(
            read_online_daemon_identity(&path, timestamp())?,
            Some(DaemonOnlineIdentity {
                instance_id: lease.instance_id,
                pid: lease.pid,
            })
        );
        assert_eq!(
            read_online_daemon_identity(&path, timestamp() + TimeDelta::seconds(10))?,
            None
        );
        let booting = request(timestamp() + TimeDelta::seconds(10));
        database.acquire_daemon_startup_lease(&booting)?;
        assert_eq!(
            read_online_daemon_identity(&path, booting.started_at)?,
            None
        );
        Ok(())
    }

    #[test]
    fn read_only_daemon_identity_treats_pre_daemon_schema_as_not_online() -> StateResult<()> {
        let directory = tempfile::tempdir().map_err(|error| StateError::io("temp", error))?;
        let root = std::fs::canonicalize(directory.path())
            .map_err(|error| StateError::io("temp", error))?;
        let path = root.join("state.db");
        let connection = rusqlite::Connection::open(&path)?;
        for schema_version in 1..=3 {
            connection.pragma_update(None, "user_version", schema_version)?;
            assert_eq!(read_online_daemon_identity(&path, timestamp())?, None);
        }
        drop(connection);

        let connection = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let schema_version: u32 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let daemon_table_count: u32 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'daemon_instances'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(schema_version, 3);
        assert_eq!(daemon_table_count, 0);
        Ok(())
    }

    #[test]
    fn read_only_daemon_identity_supports_pre_phase_schemas_and_rejects_multiple_owners()
    -> StateResult<()> {
        let directory = tempfile::tempdir().map_err(|error| StateError::io("temp", error))?;
        let root = std::fs::canonicalize(directory.path())
            .map_err(|error| StateError::io("temp", error))?;
        let path = root.join("state.db");
        let connection = rusqlite::Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE daemon_instances (
                instance_id TEXT PRIMARY KEY NOT NULL,
                pid INTEGER NOT NULL,
                started_at TEXT NOT NULL,
                heartbeat_at TEXT NOT NULL,
                lease_expires_at TEXT NOT NULL,
                stop_requested_at TEXT,
                released_at TEXT
            );",
        )?;
        let lease = request(timestamp());
        let expires_at = timestamp() + TimeDelta::seconds(10);
        connection.execute(
            "INSERT INTO daemon_instances(
                instance_id, pid, started_at, heartbeat_at, lease_expires_at,
                stop_requested_at, released_at
             ) VALUES (?1, ?2, ?3, ?3, ?4, NULL, NULL)",
            rusqlite::params![
                lease.instance_id.to_string(),
                i64::from(lease.pid),
                timestamp().to_rfc3339(),
                expires_at.to_rfc3339(),
            ],
        )?;

        for schema_version in 4..=8 {
            connection.pragma_update(None, "user_version", schema_version)?;
            assert_eq!(
                read_online_daemon_identity(&path, timestamp())?,
                Some(DaemonOnlineIdentity {
                    instance_id: lease.instance_id,
                    pid: lease.pid,
                })
            );
        }

        connection.execute(
            "UPDATE daemon_instances SET lease_expires_at = ?1",
            [timestamp().to_rfc3339()],
        )?;
        assert_eq!(read_online_daemon_identity(&path, timestamp())?, None);
        connection.execute(
            "UPDATE daemon_instances SET lease_expires_at = ?1, released_at = ?2",
            [expires_at.to_rfc3339(), timestamp().to_rfc3339()],
        )?;
        assert_eq!(read_online_daemon_identity(&path, timestamp())?, None);

        connection.execute("UPDATE daemon_instances SET released_at = NULL", [])?;
        let second = DaemonInstanceId::new();
        connection.execute(
            "INSERT INTO daemon_instances(
                instance_id, pid, started_at, heartbeat_at, lease_expires_at,
                stop_requested_at, released_at
             ) VALUES (?1, 43, ?2, ?2, ?3, NULL, NULL)",
            rusqlite::params![
                second.to_string(),
                timestamp().to_rfc3339(),
                expires_at.to_rfc3339(),
            ],
        )?;
        let Err(error) = read_online_daemon_identity(&path, timestamp()) else {
            return Err(StateError::InvalidRecord(
                "multiple unreleased daemon owners did not fail closed".to_owned(),
            ));
        };
        assert!(error.to_string().contains("multiple unreleased daemon"));
        Ok(())
    }

    #[test]
    fn read_only_daemon_identity_honors_phase_from_schema_nine() -> StateResult<()> {
        let directory = tempfile::tempdir().map_err(|error| StateError::io("temp", error))?;
        let root = std::fs::canonicalize(directory.path())
            .map_err(|error| StateError::io("temp", error))?;
        let path = root.join("state.db");
        let connection = rusqlite::Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE daemon_instances (
                instance_id TEXT PRIMARY KEY NOT NULL,
                pid INTEGER NOT NULL,
                started_at TEXT NOT NULL,
                heartbeat_at TEXT NOT NULL,
                lease_expires_at TEXT NOT NULL,
                stop_requested_at TEXT,
                released_at TEXT,
                phase TEXT NOT NULL,
                startup_error TEXT
            );
            PRAGMA user_version = 9;",
        )?;
        let lease = request(timestamp());
        connection.execute(
            "INSERT INTO daemon_instances(
                instance_id, pid, started_at, heartbeat_at, lease_expires_at,
                stop_requested_at, released_at, phase, startup_error
             ) VALUES (?1, ?2, ?3, ?3, ?4, NULL, NULL, 'online', NULL)",
            rusqlite::params![
                lease.instance_id.to_string(),
                i64::from(lease.pid),
                timestamp().to_rfc3339(),
                (timestamp() + TimeDelta::seconds(10)).to_rfc3339(),
            ],
        )?;
        assert_eq!(
            read_online_daemon_identity(&path, timestamp())?,
            Some(DaemonOnlineIdentity {
                instance_id: lease.instance_id,
                pid: lease.pid,
            })
        );
        for phase in ["booting", "probing", "failed"] {
            connection.execute("UPDATE daemon_instances SET phase = ?1", [phase])?;
            assert_eq!(read_online_daemon_identity(&path, timestamp())?, None);
        }
        Ok(())
    }

    #[test]
    fn current_schema_read_only_daemon_identity_is_consistent_with_concurrent_wal_heartbeats()
    -> StateResult<()> {
        let directory = tempfile::tempdir().map_err(|error| StateError::io("temp", error))?;
        let root = std::fs::canonicalize(directory.path())
            .map_err(|error| StateError::io("temp", error))?;
        let path = root.join("state.db");
        let setup = Database::open(&path)?;
        setup.migrate_with_backup(&root.join("backups"))?;
        let lease = request(timestamp());
        setup.acquire_daemon_lease(&lease)?;
        let barrier = Arc::new(Barrier::new(2));
        let writer_path = path.clone();
        let writer_barrier = Arc::clone(&barrier);
        let writer = std::thread::spawn(move || -> StateResult<()> {
            let writer = Database::open(writer_path)?;
            writer_barrier.wait();
            for offset in 1..=100 {
                writer.heartbeat_daemon(
                    lease.instance_id,
                    timestamp() + TimeDelta::milliseconds(offset),
                    TimeDelta::seconds(10),
                )?;
            }
            Ok(())
        });
        barrier.wait();
        for _ in 0..100 {
            assert_eq!(
                read_online_daemon_identity(&path, timestamp())?,
                Some(DaemonOnlineIdentity {
                    instance_id: lease.instance_id,
                    pid: lease.pid,
                })
            );
        }
        writer
            .join()
            .map_err(|_| StateError::InvalidRecord("heartbeat writer panicked".to_owned()))??;
        Ok(())
    }

    #[test]
    fn lease_acquisition_heartbeat_stop_and_release_follow_owner() -> StateResult<()> {
        let database = migrated_database();
        let lease = request(timestamp());
        let acquired = database.acquire_daemon_lease(&lease)?;
        assert_eq!(
            database.daemon_status(timestamp())?,
            DaemonStatus::Online(acquired.clone())
        );
        assert!(
            database
                .acquire_daemon_lease(&request(timestamp()))
                .is_err()
        );
        assert!(
            database
                .heartbeat_daemon(
                    DaemonInstanceId::new(),
                    timestamp() + TimeDelta::seconds(1),
                    TimeDelta::seconds(10),
                )
                .is_err()
        );

        let heartbeat_at = timestamp() + TimeDelta::seconds(5);
        let heartbeaten =
            database.heartbeat_daemon(lease.instance_id, heartbeat_at, TimeDelta::seconds(10))?;
        assert_eq!(heartbeaten.heartbeat_at, heartbeat_at);
        assert_eq!(
            heartbeaten.lease_expires_at,
            heartbeat_at + TimeDelta::seconds(10)
        );
        database.request_daemon_stop(lease.instance_id, heartbeat_at)?;
        assert!(database.daemon_stop_requested(lease.instance_id)?);
        database.release_daemon(lease.instance_id, heartbeat_at + TimeDelta::seconds(1))?;
        assert_eq!(
            database.daemon_status(heartbeat_at + TimeDelta::seconds(1))?,
            DaemonStatus::Stopped
        );
        Ok(())
    }

    #[test]
    fn same_owner_renews_an_expired_lease_before_any_takeover() -> StateResult<()> {
        let database = migrated_database();
        let lease = request(timestamp());
        let acquired = database.acquire_daemon_lease(&lease)?;
        let delayed_heartbeat = acquired.lease_expires_at + TimeDelta::seconds(1);

        let renewed = database.heartbeat_daemon(
            acquired.instance_id,
            delayed_heartbeat,
            TimeDelta::seconds(10),
        )?;

        assert_eq!(renewed.heartbeat_at, delayed_heartbeat);
        assert_eq!(
            renewed.lease_expires_at,
            delayed_heartbeat + TimeDelta::seconds(10)
        );
        assert_eq!(
            database.daemon_status(delayed_heartbeat)?,
            DaemonStatus::Online(renewed)
        );
        Ok(())
    }

    #[test]
    fn startup_phase_transitions_are_owned_and_monotonic() -> StateResult<()> {
        let database = migrated_database();
        let lease = request(timestamp());
        let booting = database.acquire_daemon_startup_lease(&lease)?;
        assert_eq!(booting.phase, DaemonPhase::Booting);
        assert_eq!(
            database.daemon_status(timestamp())?,
            DaemonStatus::Booting(booting)
        );

        assert!(
            database
                .transition_daemon_phase(DaemonInstanceId::new(), DaemonPhase::Probing, None,)
                .is_err()
        );
        let probing =
            database.transition_daemon_phase(lease.instance_id, DaemonPhase::Probing, None)?;
        assert_eq!(
            database.daemon_status(timestamp())?,
            DaemonStatus::Probing(probing)
        );
        assert!(
            database
                .transition_daemon_phase(lease.instance_id, DaemonPhase::Booting, None)
                .is_err()
        );

        let online =
            database.transition_daemon_phase(lease.instance_id, DaemonPhase::Online, None)?;
        assert_eq!(
            database.daemon_status(timestamp())?,
            DaemonStatus::Online(online)
        );
        Ok(())
    }

    #[test]
    fn daemon_runtime_identity_round_trips_in_status() -> StateResult<()> {
        let database = migrated_database();
        let lease = request(timestamp());
        database.acquire_daemon_lease(&lease)?;
        database.record_daemon_runtime_identity(
            lease.instance_id,
            "C:/tools/colay.exe",
            "0.1.1-nightly.20260723.abcdef0",
            "windows/x86_64",
        )?;
        let DaemonStatus::Online(instance) = database.daemon_status(timestamp())? else {
            return Err(StateError::InvalidRecord("daemon is not online".to_owned()));
        };
        assert_eq!(
            instance.executable_path.as_deref(),
            Some("C:/tools/colay.exe")
        );
        assert_eq!(
            instance.build_version.as_deref(),
            Some("0.1.1-nightly.20260723.abcdef0")
        );
        assert_eq!(instance.build_target.as_deref(), Some("windows/x86_64"));
        Ok(())
    }

    #[test]
    fn startup_failure_preserves_redacted_diagnostic() -> StateResult<()> {
        let database = migrated_database();
        let lease = request(timestamp());
        database.acquire_daemon_startup_lease(&lease)?;
        let failed = database.transition_daemon_phase(
            lease.instance_id,
            DaemonPhase::Failed,
            Some("provider probe failed: [REDACTED]"),
        )?;
        assert_eq!(failed.phase, DaemonPhase::Failed);
        assert_eq!(
            failed.startup_error.as_deref(),
            Some("provider probe failed: [REDACTED]")
        );
        assert_eq!(
            database
                .daemon_startup_diagnostic_for_pid(lease.pid)?
                .as_deref(),
            Some("provider probe failed: [REDACTED]")
        );
        assert_eq!(
            database.daemon_status(timestamp())?,
            DaemonStatus::Failed(failed)
        );
        Ok(())
    }

    #[test]
    fn stale_lease_is_visible_and_takeover_succeeds_exactly_at_expiry() -> StateResult<()> {
        let database = migrated_database();
        let first = request(timestamp());
        let acquired = database.acquire_daemon_lease(&first)?;
        assert_eq!(
            database.daemon_status(acquired.lease_expires_at)?,
            DaemonStatus::Stale(acquired.clone())
        );

        let second = request(acquired.lease_expires_at);
        let replacement = database.acquire_daemon_lease(&second)?;
        assert_eq!(replacement.instance_id, second.instance_id);
        let first_released: i64 = database.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT count(*) FROM daemon_instances
                     WHERE instance_id = ?1 AND released_at = ?2",
                    [
                        first.instance_id.to_string(),
                        second.started_at.to_rfc3339(),
                    ],
                    |row| row.get(0),
                )
                .map_err(StateError::from)
        })?;
        assert_eq!(first_released, 1);
        assert!(
            database
                .heartbeat_daemon(
                    first.instance_id,
                    second.started_at + TimeDelta::seconds(1),
                    TimeDelta::seconds(10),
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn concurrent_acquisition_has_one_winner() -> StateResult<()> {
        let directory = tempfile::tempdir().map_err(|error| StateError::io("temp", error))?;
        let root = std::fs::canonicalize(directory.path())
            .map_err(|error| StateError::io("temp", error))?;
        let path = root.join("state.db");
        let setup = Database::open(&path)?;
        setup.migrate_with_backup(&root.join("backups"))?;
        let barrier = Arc::new(Barrier::new(2));
        let handles = (0..2)
            .map(|pid| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || -> StateResult<_> {
                    let database = Database::open(path)?;
                    let mut lease = request(timestamp());
                    lease.pid += pid;
                    barrier.wait();
                    database.acquire_daemon_lease(&lease)
                })
            })
            .collect::<Vec<_>>();
        let results = handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| panic!("lease thread panicked"))
            })
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
        Ok(())
    }
}
