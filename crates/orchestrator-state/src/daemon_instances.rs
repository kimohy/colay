use std::str::FromStr as _;

use chrono::{DateTime, TimeDelta, Utc};
use orchestrator_domain::DaemonInstanceId;
use rusqlite::{OptionalExtension as _, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{Database, StateError, StateResult};

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
        let changed = self.lock()?.execute(
            "UPDATE daemon_instances SET heartbeat_at = ?1, lease_expires_at = ?2
             WHERE instance_id = ?3 AND released_at IS NULL
               AND lease_expires_at > ?1 AND heartbeat_at <= ?1",
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
                        phase, startup_error, stop_requested_at, released_at
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
                        phase, startup_error, stop_requested_at, released_at
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

    use super::{DaemonLeaseRequest, DaemonPhase, DaemonStatus};
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
