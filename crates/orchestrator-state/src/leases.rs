use std::str::FromStr as _;

use chrono::{DateTime, TimeDelta, Utc};
use orchestrator_domain::{ProviderId, TaskId};
use rusqlite::{OptionalExtension as _, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Database, StateError, StateResult};

/// One orchestrator process' exclusive authority to coordinate a task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorLease {
    pub lease_id: Uuid,
    pub task_id: TaskId,
    pub worktree_id: Option<Uuid>,
    pub owner_id: Uuid,
    pub acquired_at: DateTime<Utc>,
    pub renewed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoordinatorLeaseRequest {
    pub task_id: TaskId,
    pub worktree_id: Option<Uuid>,
    pub owner_id: Uuid,
    pub acquired_at: DateTime<Utc>,
    pub ttl: TimeDelta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseRenewal {
    pub renewed_at: DateTime<Utc>,
    pub ttl: TimeDelta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLeaseMode {
    ReadOnly,
    Writable,
}

impl WorkerLeaseMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Writable => "writable",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "read_only" => Some(Self::ReadOnly),
            "writable" => Some(Self::Writable),
            _ => None,
        }
    }
}

/// A provider process lease linked to the coordinator that owns the task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerLease {
    pub lease_id: Uuid,
    pub task_id: TaskId,
    pub worktree_id: Option<Uuid>,
    pub coordinator_lease_id: Option<Uuid>,
    pub provider: ProviderId,
    pub mode: WorkerLeaseMode,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerLeaseRequest {
    pub task_id: TaskId,
    pub worktree_id: Option<Uuid>,
    pub coordinator_lease_id: Uuid,
    pub provider: ProviderId,
    pub mode: WorkerLeaseMode,
    pub acquired_at: DateTime<Utc>,
    pub ttl: TimeDelta,
}

impl Database {
    /// Acquires exclusive task coordination authority.
    ///
    /// Stale rows are expired in the same immediate transaction as conflict detection. An
    /// unexpired coordinator or an unexpired legacy/orphan worker blocks acquisition.
    pub fn acquire_coordinator_lease(
        &self,
        request: &CoordinatorLeaseRequest,
    ) -> StateResult<CoordinatorLease> {
        validate_owner(request.owner_id)?;
        let expires_at = checked_expiry(request.acquired_at, request.ttl)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_stale_leases(&transaction, request.acquired_at)?;

        let active_coordinator = row_exists(
            &transaction,
            "SELECT EXISTS(SELECT 1 FROM coordinator_leases \
             WHERE task_id = ?1 AND released_at IS NULL)",
            &request.task_id.to_string(),
        )?;
        if active_coordinator {
            return Err(lease_conflict(
                request.task_id,
                "another coordinator lease is active",
            ));
        }
        let active_worker = row_exists(
            &transaction,
            "SELECT EXISTS(SELECT 1 FROM worker_leases \
             WHERE task_id = ?1 AND released_at IS NULL)",
            &request.task_id.to_string(),
        )?;
        if active_worker {
            return Err(lease_conflict(
                request.task_id,
                "an active worker without this coordinator must finish or expire",
            ));
        }

        let lease_id = Uuid::now_v7();
        transaction.execute(
            "INSERT INTO coordinator_leases( \
                lease_id, task_id, worktree_id, owner_id, acquired_at, renewed_at, \
                expires_at, released_at \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, NULL)",
            params![
                lease_id.to_string(),
                request.task_id.to_string(),
                request.worktree_id.map(|id| id.to_string()),
                request.owner_id.to_string(),
                request.acquired_at.to_rfc3339(),
                expires_at.to_rfc3339(),
            ],
        )?;
        let lease = coordinator_by_id(&transaction, lease_id)?.ok_or_else(|| {
            StateError::InvalidRecord("new coordinator lease was not readable".to_owned())
        })?;
        transaction.commit()?;
        Ok(lease)
    }

    /// Renews an active coordinator lease without allowing it to become shorter than a child
    /// worker lease.
    pub fn renew_coordinator_lease(
        &self,
        lease_id: Uuid,
        owner_id: Uuid,
        renewal: LeaseRenewal,
    ) -> StateResult<CoordinatorLease> {
        validate_owner(owner_id)?;
        let expires_at = checked_expiry(renewal.renewed_at, renewal.ttl)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_stale_leases(&transaction, renewal.renewed_at)?;
        let current = coordinator_by_id(&transaction, lease_id)?
            .filter(|lease| lease.released_at.is_none() && lease.owner_id == owner_id)
            .ok_or_else(|| lease_ownership(lease_id))?;

        let latest_child_expiry: Option<String> = transaction.query_row(
            "SELECT max(expires_at) FROM worker_leases \
             WHERE coordinator_lease_id = ?1 AND released_at IS NULL",
            [lease_id.to_string()],
            |row| row.get(0),
        )?;
        if let Some(child_expiry) = latest_child_expiry {
            let child_expiry = parse_datetime_value(&child_expiry, 0)?;
            if child_expiry > expires_at {
                return Err(lease_conflict(
                    current.task_id,
                    "coordinator renewal would expire before an active child worker",
                ));
            }
        }

        let changed = transaction.execute(
            "UPDATE coordinator_leases SET renewed_at = ?1, expires_at = ?2 \
             WHERE lease_id = ?3 AND owner_id = ?4 AND released_at IS NULL",
            params![
                renewal.renewed_at.to_rfc3339(),
                expires_at.to_rfc3339(),
                lease_id.to_string(),
                owner_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(lease_ownership(lease_id));
        }
        let lease = coordinator_by_id(&transaction, lease_id)?.ok_or_else(|| {
            StateError::InvalidRecord("renewed coordinator lease was not readable".to_owned())
        })?;
        transaction.commit()?;
        Ok(lease)
    }

    /// Releases a coordinator after all of its child workers have been released.
    ///
    /// The return value is `false` when the same owner repeats a completed release.
    pub fn release_coordinator_lease(
        &self,
        lease_id: Uuid,
        owner_id: Uuid,
        released_at: DateTime<Utc>,
    ) -> StateResult<bool> {
        validate_owner(owner_id)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_stale_leases(&transaction, released_at)?;
        let current = coordinator_by_id(&transaction, lease_id)?
            .filter(|lease| lease.owner_id == owner_id)
            .ok_or_else(|| lease_ownership(lease_id))?;
        if current.released_at.is_some() {
            transaction.commit()?;
            return Ok(false);
        }
        let active_children = row_exists(
            &transaction,
            "SELECT EXISTS(SELECT 1 FROM worker_leases \
             WHERE coordinator_lease_id = ?1 AND released_at IS NULL)",
            &lease_id.to_string(),
        )?;
        if active_children {
            return Err(lease_conflict(
                current.task_id,
                "active child workers must be released before the coordinator",
            ));
        }
        let changed = transaction.execute(
            "UPDATE coordinator_leases SET released_at = ?1 \
             WHERE lease_id = ?2 AND owner_id = ?3 AND released_at IS NULL",
            params![
                released_at.to_rfc3339(),
                lease_id.to_string(),
                owner_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(lease_ownership(lease_id));
        }
        transaction.commit()?;
        Ok(true)
    }

    /// Returns the active coordinator for a task after atomically expiring stale leases.
    pub fn active_coordinator_lease(
        &self,
        task_id: TaskId,
        now: DateTime<Utc>,
    ) -> StateResult<Option<CoordinatorLease>> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_stale_leases(&transaction, now)?;
        let lease = transaction
            .query_row(
                "SELECT lease_id, task_id, worktree_id, owner_id, acquired_at, renewed_at, \
                 expires_at, released_at FROM coordinator_leases \
                 WHERE task_id = ?1 AND released_at IS NULL",
                [task_id.to_string()],
                map_coordinator,
            )
            .optional()?;
        transaction.commit()?;
        Ok(lease)
    }

    /// Acquires a child provider lease under an active coordinator.
    pub fn acquire_worker_lease(&self, request: &WorkerLeaseRequest) -> StateResult<WorkerLease> {
        let expires_at = checked_expiry(request.acquired_at, request.ttl)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_stale_leases(&transaction, request.acquired_at)?;
        let coordinator = coordinator_by_id(&transaction, request.coordinator_lease_id)?
            .filter(|lease| lease.released_at.is_none())
            .ok_or_else(|| lease_ownership(request.coordinator_lease_id))?;
        if coordinator.task_id != request.task_id {
            return Err(lease_conflict(
                request.task_id,
                "worker and coordinator belong to different tasks",
            ));
        }
        if coordinator.worktree_id.is_some() && coordinator.worktree_id != request.worktree_id {
            return Err(lease_conflict(
                request.task_id,
                "worker worktree differs from its coordinator worktree",
            ));
        }
        if expires_at > coordinator.expires_at {
            return Err(lease_conflict(
                request.task_id,
                "worker lease cannot outlive its coordinator",
            ));
        }
        if request.mode == WorkerLeaseMode::Writable
            && row_exists(
                &transaction,
                "SELECT EXISTS(SELECT 1 FROM worker_leases \
                 WHERE task_id = ?1 AND mode = 'writable' AND released_at IS NULL)",
                &request.task_id.to_string(),
            )?
        {
            return Err(lease_conflict(
                request.task_id,
                "another writable worker lease is active",
            ));
        }

        let lease_id = Uuid::now_v7();
        transaction.execute(
            "INSERT INTO worker_leases( \
                lease_id, task_id, worktree_id, coordinator_lease_id, provider_id, mode, \
                acquired_at, expires_at, released_at \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
            params![
                lease_id.to_string(),
                request.task_id.to_string(),
                request.worktree_id.map(|id| id.to_string()),
                request.coordinator_lease_id.to_string(),
                request.provider.as_str(),
                request.mode.as_str(),
                request.acquired_at.to_rfc3339(),
                expires_at.to_rfc3339(),
            ],
        )?;
        let lease = worker_by_id(&transaction, lease_id)?.ok_or_else(|| {
            StateError::InvalidRecord("new worker lease was not readable".to_owned())
        })?;
        transaction.commit()?;
        Ok(lease)
    }

    /// Renews an active child worker without allowing it to outlive its coordinator.
    pub fn renew_worker_lease(
        &self,
        coordinator_lease_id: Uuid,
        lease_id: Uuid,
        renewal: LeaseRenewal,
    ) -> StateResult<WorkerLease> {
        let expires_at = checked_expiry(renewal.renewed_at, renewal.ttl)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_stale_leases(&transaction, renewal.renewed_at)?;
        let worker = worker_by_id(&transaction, lease_id)?
            .filter(|lease| {
                lease.released_at.is_none()
                    && lease.coordinator_lease_id == Some(coordinator_lease_id)
            })
            .ok_or_else(|| lease_ownership(lease_id))?;
        let coordinator = coordinator_by_id(&transaction, coordinator_lease_id)?
            .filter(|lease| lease.released_at.is_none())
            .ok_or_else(|| lease_ownership(coordinator_lease_id))?;
        if expires_at > coordinator.expires_at {
            return Err(lease_conflict(
                worker.task_id,
                "worker renewal cannot outlive its coordinator",
            ));
        }
        let changed = transaction.execute(
            "UPDATE worker_leases SET expires_at = ?1 \
             WHERE lease_id = ?2 AND coordinator_lease_id = ?3 AND released_at IS NULL",
            params![
                expires_at.to_rfc3339(),
                lease_id.to_string(),
                coordinator_lease_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(lease_ownership(lease_id));
        }
        let lease = worker_by_id(&transaction, lease_id)?.ok_or_else(|| {
            StateError::InvalidRecord("renewed worker lease was not readable".to_owned())
        })?;
        transaction.commit()?;
        Ok(lease)
    }

    /// Releases a worker owned by the supplied coordinator.
    ///
    /// The return value is `false` for an idempotent repeated release.
    pub fn release_worker_lease(
        &self,
        coordinator_lease_id: Uuid,
        lease_id: Uuid,
        released_at: DateTime<Utc>,
    ) -> StateResult<bool> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_stale_leases(&transaction, released_at)?;
        let worker = worker_by_id(&transaction, lease_id)?
            .filter(|lease| lease.coordinator_lease_id == Some(coordinator_lease_id))
            .ok_or_else(|| lease_ownership(lease_id))?;
        if worker.released_at.is_some() {
            transaction.commit()?;
            return Ok(false);
        }
        let changed = transaction.execute(
            "UPDATE worker_leases SET released_at = ?1 \
             WHERE lease_id = ?2 AND coordinator_lease_id = ?3 AND released_at IS NULL",
            params![
                released_at.to_rfc3339(),
                lease_id.to_string(),
                coordinator_lease_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(lease_ownership(lease_id));
        }
        transaction.commit()?;
        Ok(true)
    }

    /// Returns every active worker for a task after atomically expiring stale leases.
    pub fn active_worker_leases(
        &self,
        task_id: TaskId,
        now: DateTime<Utc>,
    ) -> StateResult<Vec<WorkerLease>> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_stale_leases(&transaction, now)?;
        let leases = {
            let mut statement = transaction.prepare(
                "SELECT lease_id, task_id, worktree_id, coordinator_lease_id, provider_id, mode, \
                 acquired_at, expires_at, released_at FROM worker_leases \
                 WHERE task_id = ?1 AND released_at IS NULL ORDER BY acquired_at, lease_id",
            )?;
            let rows = statement.query_map([task_id.to_string()], map_worker)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        transaction.commit()?;
        Ok(leases)
    }
}

fn validate_owner(owner_id: Uuid) -> StateResult<()> {
    if owner_id.is_nil() {
        return Err(StateError::InvalidRecord(
            "coordinator owner ID cannot be nil".to_owned(),
        ));
    }
    Ok(())
}

fn checked_expiry(started_at: DateTime<Utc>, ttl: TimeDelta) -> StateResult<DateTime<Utc>> {
    if ttl <= TimeDelta::zero() {
        return Err(StateError::InvalidRecord(
            "lease TTL must be positive".to_owned(),
        ));
    }
    started_at.checked_add_signed(ttl).ok_or_else(|| {
        StateError::InvalidRecord("lease expiry is outside the supported time range".to_owned())
    })
}

fn expire_stale_leases(transaction: &Transaction<'_>, now: DateTime<Utc>) -> StateResult<()> {
    let timestamp = now.to_rfc3339();
    transaction.execute(
        "UPDATE coordinator_leases SET released_at = ?1 \
         WHERE released_at IS NULL AND expires_at <= ?1",
        [&timestamp],
    )?;
    transaction.execute(
        "UPDATE worker_leases SET released_at = ?1 \
         WHERE released_at IS NULL AND ( \
            expires_at <= ?1 OR coordinator_lease_id IN ( \
                SELECT lease_id FROM coordinator_leases WHERE released_at IS NOT NULL \
            ) \
         )",
        [&timestamp],
    )?;
    Ok(())
}

fn row_exists(transaction: &Transaction<'_>, sql: &str, parameter: &str) -> StateResult<bool> {
    transaction
        .query_row(sql, [parameter], |row| row.get(0))
        .map_err(StateError::from)
}

fn coordinator_by_id(
    transaction: &Transaction<'_>,
    lease_id: Uuid,
) -> StateResult<Option<CoordinatorLease>> {
    transaction
        .query_row(
            "SELECT lease_id, task_id, worktree_id, owner_id, acquired_at, renewed_at, \
             expires_at, released_at FROM coordinator_leases WHERE lease_id = ?1",
            [lease_id.to_string()],
            map_coordinator,
        )
        .optional()
        .map_err(StateError::from)
}

fn worker_by_id(transaction: &Transaction<'_>, lease_id: Uuid) -> StateResult<Option<WorkerLease>> {
    transaction
        .query_row(
            "SELECT lease_id, task_id, worktree_id, coordinator_lease_id, provider_id, mode, \
             acquired_at, expires_at, released_at FROM worker_leases WHERE lease_id = ?1",
            [lease_id.to_string()],
            map_worker,
        )
        .optional()
        .map_err(StateError::from)
}

fn map_coordinator(row: &rusqlite::Row<'_>) -> rusqlite::Result<CoordinatorLease> {
    Ok(CoordinatorLease {
        lease_id: parse_uuid_value(&row.get::<_, String>(0)?, 0)?,
        task_id: parse_task_id_value(&row.get::<_, String>(1)?, 1)?,
        worktree_id: row
            .get::<_, Option<String>>(2)?
            .map(|value| parse_uuid_value(&value, 2))
            .transpose()?,
        owner_id: parse_uuid_value(&row.get::<_, String>(3)?, 3)?,
        acquired_at: parse_datetime_value(&row.get::<_, String>(4)?, 4)?,
        renewed_at: parse_datetime_value(&row.get::<_, String>(5)?, 5)?,
        expires_at: parse_datetime_value(&row.get::<_, String>(6)?, 6)?,
        released_at: row
            .get::<_, Option<String>>(7)?
            .map(|value| parse_datetime_value(&value, 7))
            .transpose()?,
    })
}

fn map_worker(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkerLease> {
    let provider = row.get::<_, String>(4)?;
    let provider = ProviderId::from_str(&provider).map_err(|error| conversion_error(4, error))?;
    let mode = row.get::<_, String>(5)?;
    let mode = WorkerLeaseMode::parse(&mode).ok_or_else(|| {
        conversion_error(
            5,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown worker lease mode `{mode}`"),
            ),
        )
    })?;
    Ok(WorkerLease {
        lease_id: parse_uuid_value(&row.get::<_, String>(0)?, 0)?,
        task_id: parse_task_id_value(&row.get::<_, String>(1)?, 1)?,
        worktree_id: row
            .get::<_, Option<String>>(2)?
            .map(|value| parse_uuid_value(&value, 2))
            .transpose()?,
        coordinator_lease_id: row
            .get::<_, Option<String>>(3)?
            .map(|value| parse_uuid_value(&value, 3))
            .transpose()?,
        provider,
        mode,
        acquired_at: parse_datetime_value(&row.get::<_, String>(6)?, 6)?,
        expires_at: parse_datetime_value(&row.get::<_, String>(7)?, 7)?,
        released_at: row
            .get::<_, Option<String>>(8)?
            .map(|value| parse_datetime_value(&value, 8))
            .transpose()?,
    })
}

fn parse_uuid_value(value: &str, column: usize) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(value).map_err(|error| conversion_error(column, error))
}

fn parse_task_id_value(value: &str, column: usize) -> rusqlite::Result<TaskId> {
    TaskId::from_str(value).map_err(|error| conversion_error(column, error))
}

fn parse_datetime_value(value: &str, column: usize) -> rusqlite::Result<DateTime<Utc>> {
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

fn lease_conflict(task_id: TaskId, reason: impl Into<String>) -> StateError {
    StateError::LeaseConflict {
        task_id: task_id.to_string(),
        reason: reason.into(),
    }
}

fn lease_ownership(lease_id: Uuid) -> StateError {
    StateError::LeaseOwnership {
        lease_id: lease_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use chrono::{TimeZone as _, Utc};
    use orchestrator_domain::{ProviderId, TaskId};
    use rusqlite::params;
    use uuid::Uuid;

    use super::{CoordinatorLeaseRequest, LeaseRenewal, WorkerLeaseMode, WorkerLeaseRequest};
    use crate::{Database, StateError, StateResult};

    fn timestamp() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 18, 9, 0, 0)
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

    fn seed_task(database: &Database, task_id: TaskId) -> StateResult<()> {
        let now = timestamp().to_rfc3339();
        database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO tasks( \
                    task_id, schema_version, revision, state, resume_state, paused, objective, \
                    original_request_redacted, task_envelope_json, created_at, updated_at, \
                    archived_at \
                 ) VALUES (?1, '1', 0, 'queued', NULL, 0, 'lease test', 'lease test', '{}', \
                    ?2, ?2, NULL)",
                params![task_id.to_string(), now],
            )?;
            Ok(())
        })
    }

    fn coordinator_request(
        task_id: TaskId,
        owner_id: Uuid,
        acquired_at: chrono::DateTime<Utc>,
    ) -> CoordinatorLeaseRequest {
        CoordinatorLeaseRequest {
            task_id,
            worktree_id: None,
            owner_id,
            acquired_at,
            ttl: chrono::TimeDelta::minutes(5),
        }
    }

    #[test]
    fn coordinator_expiry_is_atomic_and_allows_takeover() {
        let database = migrated_database();
        let task_id = TaskId::new();
        seed_task(&database, task_id).unwrap_or_else(|error| panic!("seed: {error}"));
        let started_at = timestamp();
        let owner = Uuid::now_v7();
        let mut request = coordinator_request(task_id, owner, started_at);
        request.ttl = chrono::TimeDelta::seconds(10);
        let first = database
            .acquire_coordinator_lease(&request)
            .unwrap_or_else(|error| panic!("first acquisition: {error}"));

        assert_eq!(
            database
                .active_coordinator_lease(task_id, started_at + chrono::TimeDelta::seconds(9))
                .unwrap_or_else(|error| panic!("active query: {error}"))
                .map(|lease| lease.lease_id),
            Some(first.lease_id)
        );
        assert!(
            database
                .active_coordinator_lease(task_id, started_at + chrono::TimeDelta::seconds(10))
                .unwrap_or_else(|error| panic!("expiry query: {error}"))
                .is_none()
        );

        let takeover = coordinator_request(
            task_id,
            Uuid::now_v7(),
            started_at + chrono::TimeDelta::seconds(10),
        );
        let second = database
            .acquire_coordinator_lease(&takeover)
            .unwrap_or_else(|error| panic!("takeover: {error}"));
        assert_ne!(first.lease_id, second.lease_id);
    }

    #[test]
    fn child_workers_coexist_only_under_their_coordinator() {
        let database = migrated_database();
        let task_id = TaskId::new();
        seed_task(&database, task_id).unwrap_or_else(|error| panic!("seed: {error}"));
        let now = timestamp();
        let owner = Uuid::now_v7();
        let coordinator = database
            .acquire_coordinator_lease(&coordinator_request(task_id, owner, now))
            .unwrap_or_else(|error| panic!("coordinator: {error}"));
        let worker_request = |provider, mode| WorkerLeaseRequest {
            task_id,
            worktree_id: None,
            coordinator_lease_id: coordinator.lease_id,
            provider,
            mode,
            acquired_at: now,
            ttl: chrono::TimeDelta::minutes(2),
        };
        let reader = database
            .acquire_worker_lease(&worker_request(
                ProviderId::Gemini,
                WorkerLeaseMode::ReadOnly,
            ))
            .unwrap_or_else(|error| panic!("reader: {error}"));
        let writer = database
            .acquire_worker_lease(&worker_request(
                ProviderId::Codex,
                WorkerLeaseMode::Writable,
            ))
            .unwrap_or_else(|error| panic!("writer: {error}"));
        let second_writer = database.acquire_worker_lease(&worker_request(
            ProviderId::Claude,
            WorkerLeaseMode::Writable,
        ));
        assert!(matches!(
            second_writer,
            Err(StateError::LeaseConflict { .. })
        ));

        let early_release = database.release_coordinator_lease(
            coordinator.lease_id,
            owner,
            now + chrono::TimeDelta::seconds(1),
        );
        assert!(matches!(
            early_release,
            Err(StateError::LeaseConflict { .. })
        ));
        database
            .release_worker_lease(
                coordinator.lease_id,
                reader.lease_id,
                now + chrono::TimeDelta::seconds(2),
            )
            .unwrap_or_else(|error| panic!("release reader: {error}"));
        database
            .release_worker_lease(
                coordinator.lease_id,
                writer.lease_id,
                now + chrono::TimeDelta::seconds(2),
            )
            .unwrap_or_else(|error| panic!("release writer: {error}"));
        assert!(
            database
                .release_coordinator_lease(
                    coordinator.lease_id,
                    owner,
                    now + chrono::TimeDelta::seconds(3),
                )
                .unwrap_or_else(|error| panic!("release coordinator: {error}"))
        );
    }

    #[test]
    fn renewals_preserve_parent_child_expiry_order() {
        let database = migrated_database();
        let task_id = TaskId::new();
        seed_task(&database, task_id).unwrap_or_else(|error| panic!("seed: {error}"));
        let now = timestamp();
        let owner = Uuid::now_v7();
        let coordinator = database
            .acquire_coordinator_lease(&coordinator_request(task_id, owner, now))
            .unwrap_or_else(|error| panic!("coordinator: {error}"));
        let worker = database
            .acquire_worker_lease(&WorkerLeaseRequest {
                task_id,
                worktree_id: None,
                coordinator_lease_id: coordinator.lease_id,
                provider: ProviderId::Codex,
                mode: WorkerLeaseMode::Writable,
                acquired_at: now,
                ttl: chrono::TimeDelta::minutes(4),
            })
            .unwrap_or_else(|error| panic!("worker: {error}"));

        let short_parent = database.renew_coordinator_lease(
            coordinator.lease_id,
            owner,
            LeaseRenewal {
                renewed_at: now + chrono::TimeDelta::minutes(1),
                ttl: chrono::TimeDelta::minutes(1),
            },
        );
        assert!(matches!(
            short_parent,
            Err(StateError::LeaseConflict { .. })
        ));
        let long_child = database.renew_worker_lease(
            coordinator.lease_id,
            worker.lease_id,
            LeaseRenewal {
                renewed_at: now + chrono::TimeDelta::minutes(1),
                ttl: chrono::TimeDelta::minutes(5),
            },
        );
        assert!(matches!(long_child, Err(StateError::LeaseConflict { .. })));
    }

    #[test]
    fn orphan_worker_blocks_coordinator_acquisition_until_expiry() {
        let database = migrated_database();
        let task_id = TaskId::new();
        seed_task(&database, task_id).unwrap_or_else(|error| panic!("seed: {error}"));
        let now = timestamp();
        database
            .with_connection(|connection| {
                connection.execute(
                    "INSERT INTO worker_leases( \
                        lease_id, task_id, worktree_id, coordinator_lease_id, provider_id, mode, \
                        acquired_at, expires_at, released_at \
                     ) VALUES (?1, ?2, NULL, NULL, 'codex', 'writable', ?3, ?4, NULL)",
                    params![
                        Uuid::now_v7().to_string(),
                        task_id.to_string(),
                        now.to_rfc3339(),
                        (now + chrono::TimeDelta::seconds(30)).to_rfc3339(),
                    ],
                )?;
                Ok(())
            })
            .unwrap_or_else(|error| panic!("orphan worker: {error}"));
        let blocked =
            database.acquire_coordinator_lease(&coordinator_request(task_id, Uuid::now_v7(), now));
        assert!(matches!(blocked, Err(StateError::LeaseConflict { .. })));

        let request = coordinator_request(
            task_id,
            Uuid::now_v7(),
            now + chrono::TimeDelta::seconds(30),
        );
        database
            .acquire_coordinator_lease(&request)
            .unwrap_or_else(|error| panic!("acquire after orphan expiry: {error}"));
    }

    #[test]
    fn concurrent_coordinator_acquisition_has_one_winner() {
        let directory = crate::CanonicalTempDir::new("tempdir")
            .unwrap_or_else(|error| panic!("tempdir: {error}"));
        let path = directory.path().join("coordinator-race.db");
        let task_id = TaskId::new();
        {
            let database =
                Database::open(&path).unwrap_or_else(|error| panic!("database: {error}"));
            database
                .migrate_with_backup(directory.path())
                .unwrap_or_else(|error| panic!("migrations: {error}"));
            seed_task(&database, task_id).unwrap_or_else(|error| panic!("seed: {error}"));
        }

        let barrier = Arc::new(Barrier::new(2));
        let spawn_contender = |owner_id| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let database =
                    Database::open(path).unwrap_or_else(|error| panic!("database: {error}"));
                barrier.wait();
                database.acquire_coordinator_lease(&coordinator_request(
                    task_id,
                    owner_id,
                    timestamp(),
                ))
            })
        };
        let first = spawn_contender(Uuid::now_v7());
        let second = spawn_contender(Uuid::now_v7());
        let outcomes = [
            first
                .join()
                .unwrap_or_else(|_| panic!("first contender panicked")),
            second
                .join()
                .unwrap_or_else(|_| panic!("second contender panicked")),
        ];
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, Err(StateError::LeaseConflict { .. })))
                .count(),
            1
        );
    }
}
