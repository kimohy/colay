use std::{collections::BTreeMap, str::FromStr as _};

use chrono::{DateTime, TimeDelta, Utc};
use orchestrator_domain::{
    ActiveResourceClaim, DaemonInstanceId, DependencyState, GraphRevisionId, ModelProfile,
    ProviderId, RepoPath, ResourceClaimId, ResourceScope, ScheduleCandidate, ScheduleCapacity,
    ScheduleClaimId, SessionId, TaskEnvelope, TaskId, select_ready_tasks,
};
use rusqlite::{OptionalExtension as _, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{Database, StateError, StateResult};

#[derive(Clone, Debug)]
pub struct ClaimReadyTaskRequest {
    pub daemon_instance_id: DaemonInstanceId,
    pub global_limit: usize,
    pub provider_limits: BTreeMap<ProviderId, usize>,
    pub now: DateTime<Utc>,
    pub ttl: TimeDelta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaimedTask {
    pub schedule_claim_id: ScheduleClaimId,
    pub daemon_instance_id: DaemonInstanceId,
    pub session_id: SessionId,
    pub revision_id: GraphRevisionId,
    pub task_id: TaskId,
    pub node_key: String,
    pub display_order: u64,
    pub provider: ProviderId,
    pub profile: ModelProfile,
    pub envelope: TaskEnvelope,
    pub scope: ResourceScope,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
struct CandidateRecord {
    candidate: ScheduleCandidate,
    node_key: String,
    profile: ModelProfile,
    envelope: TaskEnvelope,
}

impl Database {
    pub fn claim_next_ready_task(
        &self,
        request: &ClaimReadyTaskRequest,
    ) -> StateResult<Option<ClaimedTask>> {
        validate_request(request)?;
        let expires_at = request.now.checked_add_signed(request.ttl).ok_or_else(|| {
            StateError::InvalidRecord("schedule claim expiry overflow".to_owned())
        })?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_claims(&transaction, request.now)?;
        ensure_daemon_owner(&transaction, request.daemon_instance_id, request.now)?;

        let active_global: i64 = transaction.query_row(
            "SELECT count(*) FROM task_schedule_claims
             WHERE released_at IS NULL AND expires_at > ?1",
            [request.now.to_rfc3339()],
            |row| row.get(0),
        )?;
        let mut active_by_provider = BTreeMap::new();
        {
            let mut statement = transaction.prepare(
                "SELECT provider_id, count(*) FROM task_schedule_claims
                 WHERE released_at IS NULL AND expires_at > ?1 GROUP BY provider_id",
            )?;
            let rows = statement.query_map([request.now.to_rfc3339()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (provider, count) = row?;
                active_by_provider.insert(
                    parse_provider(&provider)?,
                    usize::try_from(count).map_err(|_| {
                        StateError::InvalidRecord("negative active provider count".to_owned())
                    })?,
                );
            }
        }
        let active_claims = load_active_resource_claims(&transaction, request.now)?;
        let candidates = load_candidates(&transaction)?;
        let capacity = ScheduleCapacity {
            global_limit: request.global_limit,
            active_global: usize::try_from(active_global).map_err(|_| {
                StateError::InvalidRecord("negative active schedule count".to_owned())
            })?,
            provider_limits: request.provider_limits.clone(),
            active_by_provider,
        };
        let domain_candidates = candidates
            .iter()
            .map(|record| record.candidate.clone())
            .collect::<Vec<_>>();
        let selected = select_ready_tasks(&domain_candidates, &capacity, &active_claims)
            .map_err(|error| StateError::InvalidConfig(error.to_string()))?;
        let Some(task_id) = selected.first().copied() else {
            transaction.commit()?;
            return Ok(None);
        };
        let record = candidates
            .into_iter()
            .find(|record| record.candidate.task_id == task_id)
            .ok_or_else(|| {
                StateError::InvalidRecord("selected schedule task is missing".to_owned())
            })?;
        let schedule_claim_id = ScheduleClaimId::new();
        transaction.execute(
            "INSERT INTO task_schedule_claims(
                schedule_claim_id, daemon_instance_id, session_id, revision_id, task_id,
                provider_id, acquired_at, expires_at, released_at, release_reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL)",
            params![
                schedule_claim_id.to_string(),
                request.daemon_instance_id.to_string(),
                record.candidate.session_id.to_string(),
                record.candidate.revision_id.to_string(),
                task_id.to_string(),
                record.candidate.provider.as_str(),
                request.now.to_rfc3339(),
                expires_at.to_rfc3339(),
            ],
        )?;
        insert_resource_claims(
            &transaction,
            schedule_claim_id,
            &record.candidate,
            request.now,
            expires_at,
        )?;
        transaction.commit()?;
        Ok(Some(ClaimedTask {
            schedule_claim_id,
            daemon_instance_id: request.daemon_instance_id,
            session_id: record.candidate.session_id,
            revision_id: record.candidate.revision_id,
            task_id,
            node_key: record.node_key,
            display_order: record.candidate.graph_order,
            provider: record.candidate.provider,
            profile: record.profile,
            envelope: record.envelope,
            scope: record.candidate.scope,
            acquired_at: request.now,
            expires_at,
        }))
    }

    pub fn renew_schedule_claim(
        &self,
        schedule_claim_id: ScheduleClaimId,
        daemon_instance_id: DaemonInstanceId,
        now: DateTime<Utc>,
        ttl: TimeDelta,
    ) -> StateResult<DateTime<Utc>> {
        if ttl <= TimeDelta::zero() {
            return Err(StateError::InvalidRecord(
                "schedule claim TTL must be greater than zero".to_owned(),
            ));
        }
        let expires_at = now.checked_add_signed(ttl).ok_or_else(|| {
            StateError::InvalidRecord("schedule claim expiry overflow".to_owned())
        })?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_daemon_owner(&transaction, daemon_instance_id, now)?;
        let changed = transaction.execute(
            "UPDATE task_schedule_claims SET expires_at = ?1
             WHERE schedule_claim_id = ?2 AND daemon_instance_id = ?3
               AND released_at IS NULL AND expires_at > ?4",
            params![
                expires_at.to_rfc3339(),
                schedule_claim_id.to_string(),
                daemon_instance_id.to_string(),
                now.to_rfc3339(),
            ],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("active schedule claim {schedule_claim_id}"),
            });
        }
        transaction.execute(
            "UPDATE resource_claims SET expires_at = ?1
             WHERE schedule_claim_id = ?2 AND released_at IS NULL",
            params![expires_at.to_rfc3339(), schedule_claim_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(expires_at)
    }

    pub fn release_schedule_claim(
        &self,
        schedule_claim_id: ScheduleClaimId,
        daemon_instance_id: DaemonInstanceId,
        released_at: DateTime<Utc>,
        reason: &str,
    ) -> StateResult<bool> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(StateError::InvalidRecord(
                "schedule release reason must not be blank".to_owned(),
            ));
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE task_schedule_claims SET released_at = ?1, release_reason = ?2
             WHERE schedule_claim_id = ?3 AND daemon_instance_id = ?4 AND released_at IS NULL",
            params![
                released_at.to_rfc3339(),
                reason,
                schedule_claim_id.to_string(),
                daemon_instance_id.to_string(),
            ],
        )?;
        if changed == 1 {
            transaction.execute(
                "UPDATE resource_claims SET released_at = ?1, release_reason = ?2
                 WHERE schedule_claim_id = ?3 AND released_at IS NULL",
                params![
                    released_at.to_rfc3339(),
                    reason,
                    schedule_claim_id.to_string()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }
}

fn validate_request(request: &ClaimReadyTaskRequest) -> StateResult<()> {
    if request.global_limit == 0 {
        return Err(StateError::InvalidConfig(
            "global scheduling limit must be greater than zero".to_owned(),
        ));
    }
    if request.provider_limits.values().any(|limit| *limit == 0) {
        return Err(StateError::InvalidConfig(
            "provider scheduling limits must be greater than zero".to_owned(),
        ));
    }
    if request.ttl <= TimeDelta::zero() {
        return Err(StateError::InvalidRecord(
            "schedule claim TTL must be greater than zero".to_owned(),
        ));
    }
    Ok(())
}

fn expire_claims(transaction: &Transaction<'_>, now: DateTime<Utc>) -> StateResult<()> {
    let now = now.to_rfc3339();
    transaction.execute(
        "UPDATE resource_claims SET released_at = ?1, release_reason = 'schedule claim expired'
         WHERE released_at IS NULL AND expires_at <= ?1",
        [&now],
    )?;
    transaction.execute(
        "UPDATE task_schedule_claims SET released_at = ?1, release_reason = 'schedule claim expired'
         WHERE released_at IS NULL AND expires_at <= ?1",
        [&now],
    )?;
    Ok(())
}

fn ensure_daemon_owner(
    transaction: &Transaction<'_>,
    daemon_instance_id: DaemonInstanceId,
    now: DateTime<Utc>,
) -> StateResult<()> {
    let active = transaction
        .query_row(
            "SELECT 1 FROM daemon_instances WHERE instance_id = ?1 AND released_at IS NULL
             AND stop_requested_at IS NULL AND lease_expires_at > ?2",
            params![daemon_instance_id.to_string(), now.to_rfc3339()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if active {
        Ok(())
    } else {
        Err(StateError::OptimisticConflict {
            entity: format!("active daemon instance {daemon_instance_id}"),
        })
    }
}

fn load_candidates(transaction: &Transaction<'_>) -> StateResult<Vec<CandidateRecord>> {
    let mut statement = transaction.prepare(
        "SELECT st.session_id, st.revision_id, st.task_id, st.node_key, st.display_order,
                st.provider_id, st.model_profile, t.state, t.paused, t.task_envelope_json,
                t.created_at
         FROM session_tasks st
         JOIN tasks t ON t.task_id = st.task_id
         JOIN graph_revisions gr ON gr.revision_id = st.revision_id
         JOIN session_graph_heads gh ON gh.session_id = st.session_id
                                    AND gh.revision_id = st.revision_id
         WHERE gr.status = 'approved' AND t.archived_at IS NULL
         ORDER BY t.created_at, st.display_order, st.task_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, String>(10)?,
        ))
    })?;
    let mut candidates = Vec::new();
    for row in rows {
        let (
            session_id,
            revision_id,
            task_id,
            node_key,
            display_order,
            provider,
            profile,
            state,
            paused,
            envelope_json,
            created_at,
        ) = row?;
        let task_id = parse_id("task id", &task_id)?;
        let envelope: TaskEnvelope = serde_json::from_str(&envelope_json)?;
        let scope = ResourceScope {
            paths: envelope.allowed_write_paths.clone(),
            repository_wide: envelope.repository_wide_write_scope,
        };
        let dependencies = load_dependencies(transaction, task_id)?;
        candidates.push(CandidateRecord {
            candidate: ScheduleCandidate {
                task_id,
                session_id: parse_id("session id", &session_id)?,
                revision_id: parse_id("graph revision id", &revision_id)?,
                graph_is_current: true,
                graph_order: u64::try_from(display_order).map_err(|_| {
                    StateError::InvalidRecord("negative graph display order".to_owned())
                })?,
                ready_since: parse_time("task creation time", &created_at)?,
                state: parse_enum("task state", &state)?,
                paused: paused != 0,
                provider: parse_provider(&provider)?,
                dependencies,
                scope,
            },
            node_key,
            profile: parse_enum("model profile", &profile)?,
            envelope,
        });
    }
    Ok(candidates)
}

fn load_dependencies(
    transaction: &Transaction<'_>,
    task_id: TaskId,
) -> StateResult<Vec<DependencyState>> {
    let mut statement = transaction.prepare(
        "SELECT dependency.task_id, dependency.state,
                coalesce((SELECT outcome = 'pass' FROM verification_results
                          WHERE task_id = dependency.task_id
                          ORDER BY completed_at DESC, verification_id DESC LIMIT 1), 0)
         FROM task_dependencies edge
         JOIN tasks dependency ON dependency.task_id = edge.depends_on_task_id
         WHERE edge.task_id = ?1 ORDER BY edge.depends_on_task_id",
    )?;
    let rows = statement.query_map([task_id.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    rows.map(|row| {
        let (dependency_id, state, verification_passed) = row?;
        Ok(DependencyState {
            task_id: parse_id("dependency task id", &dependency_id)?,
            state: parse_enum("dependency task state", &state)?,
            verification_passed: verification_passed != 0,
        })
    })
    .collect()
}

fn load_active_resource_claims(
    transaction: &Transaction<'_>,
    now: DateTime<Utc>,
) -> StateResult<Vec<ActiveResourceClaim>> {
    let mut statement = transaction.prepare(
        "SELECT rc.task_id, rc.path, rc.repository_wide
         FROM resource_claims rc
         JOIN task_schedule_claims sc ON sc.schedule_claim_id = rc.schedule_claim_id
         WHERE rc.released_at IS NULL AND sc.released_at IS NULL
           AND rc.expires_at > ?1 AND sc.expires_at > ?1
         ORDER BY rc.task_id, rc.resource_claim_id",
    )?;
    let rows = statement.query_map([now.to_rfc3339()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut grouped: BTreeMap<TaskId, ResourceScope> = BTreeMap::new();
    for row in rows {
        let (task_id, path, repository_wide) = row?;
        let scope = grouped
            .entry(parse_id("resource task id", &task_id)?)
            .or_insert(ResourceScope {
                paths: Vec::new(),
                repository_wide: false,
            });
        scope.repository_wide |= repository_wide != 0;
        if let Some(path) = path {
            scope.paths.push(RepoPath::try_from(path).map_err(|error| {
                StateError::InvalidRecord(format!("invalid resource claim path: {error}"))
            })?);
        }
    }
    Ok(grouped
        .into_iter()
        .map(|(task_id, scope)| ActiveResourceClaim { task_id, scope })
        .collect())
}

fn insert_resource_claims(
    transaction: &Transaction<'_>,
    schedule_claim_id: ScheduleClaimId,
    candidate: &ScheduleCandidate,
    acquired_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> StateResult<()> {
    if !candidate.scope.repository_wide && candidate.scope.paths.is_empty() {
        return Err(StateError::InvalidRecord(
            "scheduled task has no writable resource scope".to_owned(),
        ));
    }
    let paths = if candidate.scope.repository_wide {
        vec![None]
    } else {
        candidate
            .scope
            .paths
            .iter()
            .map(|path| Some(path.to_string()))
            .collect()
    };
    for path in paths {
        transaction.execute(
            "INSERT INTO resource_claims(
                resource_claim_id, schedule_claim_id, session_id, revision_id, task_id,
                path, repository_wide, acquired_at, expires_at, released_at, release_reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL)",
            params![
                ResourceClaimId::new().to_string(),
                schedule_claim_id.to_string(),
                candidate.session_id.to_string(),
                candidate.revision_id.to_string(),
                candidate.task_id.to_string(),
                path,
                i64::from(candidate.scope.repository_wide),
                acquired_at.to_rfc3339(),
                expires_at.to_rfc3339(),
            ],
        )?;
    }
    Ok(())
}

fn parse_provider(value: &str) -> StateResult<ProviderId> {
    ProviderId::from_str(value)
        .map_err(|error| StateError::InvalidRecord(format!("invalid provider: {error}")))
}

fn parse_id<T: std::str::FromStr>(label: &str, value: &str) -> StateResult<T>
where
    T::Err: std::fmt::Display,
{
    T::from_str(value)
        .map_err(|error| StateError::InvalidRecord(format!("invalid {label}: {error}")))
}

fn parse_enum<T: for<'de> Deserialize<'de>>(label: &str, value: &str) -> StateResult<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|error| StateError::InvalidRecord(format!("invalid {label}: {error}")))
}

fn parse_time(label: &str, value: &str) -> StateResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| StateError::InvalidRecord(format!("invalid {label}: {error}")))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{TimeDelta, TimeZone as _, Utc};
    use orchestrator_domain::{
        DaemonInstanceId, ModelProfile, ProviderId, RepoPath, SchemaVersion, SessionId,
        TaskEnvelope, TaskId,
    };
    use rusqlite::params;

    use super::{ClaimReadyTaskRequest, Database};
    use crate::DaemonLeaseRequest;

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0)
            .single()
            .unwrap_or_default()
    }

    fn setup() -> Result<(tempfile::TempDir, Database, DaemonInstanceId), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let root = std::fs::canonicalize(directory.path())?;
        let path = root.join("orchestrator.db");
        let database = Database::open(&path)?;
        database.migrate_with_backup(&root.join("backups"))?;
        let daemon = DaemonInstanceId::new();
        database.acquire_daemon_lease(&DaemonLeaseRequest {
            instance_id: daemon,
            pid: 7,
            started_at: now(),
            ttl: TimeDelta::minutes(10),
        })?;
        Ok((directory, database, daemon))
    }

    fn seed_task(
        database: &Database,
        session: SessionId,
        revision: orchestrator_domain::GraphRevisionId,
        order: i64,
        provider: ProviderId,
        path: &str,
        dependencies: &[TaskId],
    ) -> Result<TaskId, Box<dyn std::error::Error>> {
        let task_id = TaskId::new();
        let envelope = TaskEnvelope {
            schema_version: SchemaVersion::v1(),
            task_id,
            objective: format!("task {order}"),
            original_request_redacted: "goal".to_owned(),
            constraints: Vec::new(),
            acceptance_criteria: vec!["done".to_owned()],
            allowed_write_paths: vec![RepoPath::try_from(path)?],
            repository_wide_write_scope: false,
            assessment: None,
            created_at: now(),
        };
        database.with_transaction(|transaction| {
            transaction.execute(
                "INSERT INTO tasks(task_id, schema_version, state, objective,
                    original_request_redacted, task_envelope_json, created_at, updated_at)
                 VALUES (?1, 'v1', 'queued', ?2, 'goal', ?3, ?4, ?4)",
                params![
                    task_id.to_string(),
                    envelope.objective,
                    serde_json::to_string(&envelope)?,
                    now().to_rfc3339(),
                ],
            )?;
            transaction.execute(
                "INSERT INTO session_tasks(session_id, revision_id, task_id, node_key,
                    display_order, provider_id, model_profile)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session.to_string(),
                    revision.to_string(),
                    task_id.to_string(),
                    format!("task-{order}"),
                    order,
                    provider.as_str(),
                    serde_json::to_value(ModelProfile::Standard)?
                        .as_str()
                        .unwrap_or_default(),
                ],
            )?;
            for dependency in dependencies {
                transaction.execute(
                    "INSERT INTO task_dependencies(session_id, revision_id, task_id,
                        depends_on_task_id) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        session.to_string(),
                        revision.to_string(),
                        task_id.to_string(),
                        dependency.to_string(),
                    ],
                )?;
            }
            Ok(())
        })?;
        Ok(task_id)
    }

    fn seed_graph(
        database: &Database,
    ) -> Result<(SessionId, orchestrator_domain::GraphRevisionId), Box<dyn std::error::Error>> {
        let session = SessionId::new();
        let message = orchestrator_domain::MessageId::new();
        let revision = orchestrator_domain::GraphRevisionId::new();
        database.with_transaction(|transaction| {
            transaction.execute(
                "INSERT INTO sessions(session_id, schema_version, title, state, created_at, updated_at)
                 VALUES (?1, 'v1', 'test', 'running', ?2, ?2)",
                params![session.to_string(), now().to_rfc3339()],
            )?;
            transaction.execute(
                "INSERT INTO conversation_messages(message_id, session_id, ordinal, role, kind,
                    state, content_redacted, created_at, finalized_at)
                 VALUES (?1, ?2, 1, 'user', 'user_message', 'final', 'goal', ?3, ?3)",
                params![message.to_string(), session.to_string(), now().to_rfc3339()],
            )?;
            transaction.execute(
                "INSERT INTO graph_revisions(revision_id, session_id, goal_message_id, ordinal,
                    status, proposal_hash, validation_json, planner_provider, created_at, completed_at)
                 VALUES (?1, ?2, ?3, 1, 'approved', ?4, '{}', 'codex', ?5, ?5)",
                params![
                    revision.to_string(),
                    session.to_string(),
                    message.to_string(),
                    "0".repeat(64),
                    now().to_rfc3339(),
                ],
            )?;
            transaction.execute(
                "INSERT INTO session_graph_heads(session_id, revision_id, updated_at)
                 VALUES (?1, ?2, ?3)",
                params![session.to_string(), revision.to_string(), now().to_rfc3339()],
            )?;
            Ok(())
        })?;
        Ok((session, revision))
    }

    fn request(daemon: DaemonInstanceId) -> ClaimReadyTaskRequest {
        ClaimReadyTaskRequest {
            daemon_instance_id: daemon,
            global_limit: 2,
            provider_limits: BTreeMap::from([(ProviderId::Codex, 1)]),
            now: now(),
            ttl: TimeDelta::minutes(2),
        }
    }

    #[test]
    fn two_connections_never_claim_the_same_task_and_release_is_idempotent()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, database, daemon) = setup()?;
        let (session, revision) = seed_graph(&database)?;
        let task = seed_task(
            &database,
            session,
            revision,
            1,
            ProviderId::Codex,
            "src/a",
            &[],
        )?;
        let other = Database::open(directory.path().join("orchestrator.db"))?;

        let claimed = database
            .claim_next_ready_task(&request(daemon))?
            .ok_or("claim missing")?;
        assert_eq!(claimed.task_id, task);
        assert!(other.claim_next_ready_task(&request(daemon))?.is_none());
        assert!(database.release_schedule_claim(
            claimed.schedule_claim_id,
            daemon,
            now(),
            "done"
        )?);
        assert!(!database.release_schedule_claim(
            claimed.schedule_claim_id,
            daemon,
            now(),
            "done"
        )?);
        Ok(())
    }

    #[test]
    fn provider_capacity_and_overlapping_resource_claims_block_admission()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, database, daemon) = setup()?;
        let (session, revision) = seed_graph(&database)?;
        let first = seed_task(
            &database,
            session,
            revision,
            1,
            ProviderId::Codex,
            "src/a",
            &[],
        )?;
        let overlapping = seed_task(
            &database,
            session,
            revision,
            2,
            ProviderId::Claude,
            "src/a/nested",
            &[],
        )?;
        let provider_limited = seed_task(
            &database,
            session,
            revision,
            3,
            ProviderId::Codex,
            "src/b",
            &[],
        )?;
        let mut limits = request(daemon);
        limits.provider_limits.insert(ProviderId::Claude, 1);
        let claim = database
            .claim_next_ready_task(&limits)?
            .ok_or("claim missing")?;
        assert_eq!(claim.task_id, first);
        assert!(database.claim_next_ready_task(&limits)?.is_none());

        database.release_schedule_claim(claim.schedule_claim_id, daemon, now(), "done")?;
        let next = database
            .claim_next_ready_task(&limits)?
            .ok_or("next claim missing")?;
        assert_eq!(
            next.task_id, first,
            "queued tasks remain eligible until execution starts"
        );
        database.with_connection(|connection| {
            let count: i64 = connection.query_row(
                "SELECT count(*) FROM task_schedule_claims WHERE task_id IN (?1, ?2)",
                params![overlapping.to_string(), provider_limited.to_string()],
                |row| row.get(0),
            )?;
            assert_eq!(count, 0);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn dependency_requires_completed_state_and_latest_pass_verification()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, database, daemon) = setup()?;
        let (session, revision) = seed_graph(&database)?;
        let dependency = seed_task(
            &database,
            session,
            revision,
            1,
            ProviderId::Claude,
            "src/a",
            &[],
        )?;
        let dependent = seed_task(
            &database,
            session,
            revision,
            2,
            ProviderId::Codex,
            "src/b",
            &[dependency],
        )?;
        database.with_connection(|connection| {
            connection.execute(
                "UPDATE tasks SET state = 'completed' WHERE task_id = ?1",
                [dependency.to_string()],
            )?;
            Ok(())
        })?;
        let mut codex_only = request(daemon);
        codex_only.provider_limits.insert(ProviderId::Claude, 1);
        assert!(database.claim_next_ready_task(&codex_only)?.is_none());

        database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO verification_results(verification_id, task_id, outcome,
                    schema_version, result_json, started_at, completed_at)
                 VALUES (?1, ?2, 'pass', 'v1', '{}', ?3, ?3)",
                params![
                    uuid::Uuid::now_v7().to_string(),
                    dependency.to_string(),
                    now().to_rfc3339()
                ],
            )?;
            Ok(())
        })?;
        let claimed = database
            .claim_next_ready_task(&codex_only)?
            .ok_or("dependent claim missing")?;
        assert_eq!(claimed.task_id, dependent);
        Ok(())
    }
}
