use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use chrono::{TimeDelta, Utc};
use orchestrator_domain::{
    CorrelationId, DaemonInstanceId, EventActor, EventId, EventType, ProviderId, SchemaVersion,
    TaskEvent, TaskInstructionState, TaskState, TransitionGuards, WorkerOutcome,
};
use orchestrator_engine::{
    GitWorktree, TaskExecutionReport, TaskExecutionRequest, TaskExecutor, canonicalize_directory,
};
use orchestrator_state::{
    ClaimReadyTaskRequest, ClaimedTask, Database, NewTaskAttemptRecord, NewWorktreeRecord,
};
use tokio_util::sync::CancellationToken;

use crate::{DaemonError, MessageRedactor};

#[derive(Clone)]
pub struct ExecutionServices {
    pub executor: Arc<dyn TaskExecutor>,
    pub repository_root: PathBuf,
    pub state_root: PathBuf,
    pub global_limit: usize,
    pub provider_limits: BTreeMap<ProviderId, usize>,
    pub claim_ttl: TimeDelta,
}

pub(crate) fn validate_execution_services(services: &ExecutionServices) -> Result<(), DaemonError> {
    if services.global_limit == 0 {
        return Err(DaemonError::InvalidSettings(
            "execution global limit must be positive".to_owned(),
        ));
    }
    if services.provider_limits.values().any(|limit| *limit == 0) {
        return Err(DaemonError::InvalidSettings(
            "execution provider limits must be positive".to_owned(),
        ));
    }
    if services.claim_ttl <= TimeDelta::zero() {
        return Err(DaemonError::InvalidSettings(
            "execution claim TTL must be positive".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn spawn_ready_tasks(
    database: &Arc<Database>,
    instance_id: DaemonInstanceId,
    services: &ExecutionServices,
    redactor: &Arc<dyn MessageRedactor>,
    cancellation: &CancellationToken,
    jobs: &mut Vec<tokio::task::JoinHandle<Result<(), DaemonError>>>,
) -> Result<(), DaemonError> {
    while jobs.len() < services.global_limit {
        let request = ClaimReadyTaskRequest {
            daemon_instance_id: instance_id,
            global_limit: services.global_limit,
            provider_limits: services.provider_limits.clone(),
            now: Utc::now(),
            ttl: services.claim_ttl,
        };
        let Some(claim) = database.claim_next_ready_task(&request)? else {
            break;
        };
        let job_database = Arc::clone(database);
        let job_services = services.clone();
        let job_redactor = Arc::clone(redactor);
        let job_cancellation = cancellation.child_token();
        jobs.push(tokio::spawn(async move {
            run_claimed_task(
                job_database,
                instance_id,
                claim,
                job_services,
                job_redactor,
                job_cancellation,
            )
            .await
        }));
    }
    Ok(())
}

pub(crate) async fn reap_finished_tasks(
    jobs: &mut Vec<tokio::task::JoinHandle<Result<(), DaemonError>>>,
) -> Result<(), DaemonError> {
    let mut index = 0;
    while index < jobs.len() {
        if jobs[index].is_finished() {
            let job = jobs.swap_remove(index);
            job.await.map_err(|error| {
                DaemonError::InvalidSettings(format!("execution job failed: {error}"))
            })??;
        } else {
            index += 1;
        }
    }
    Ok(())
}

pub(crate) async fn stop_execution_jobs(
    cancellation: &CancellationToken,
    jobs: Vec<tokio::task::JoinHandle<Result<(), DaemonError>>>,
) -> Result<(), DaemonError> {
    cancellation.cancel();
    for job in jobs {
        job.await.map_err(|error| {
            DaemonError::InvalidSettings(format!("execution shutdown failed: {error}"))
        })??;
    }
    Ok(())
}

async fn run_claimed_task(
    database: Arc<Database>,
    instance_id: DaemonInstanceId,
    claim: ClaimedTask,
    services: ExecutionServices,
    redactor: Arc<dyn MessageRedactor>,
    cancellation: CancellationToken,
) -> Result<(), DaemonError> {
    let result = run_claimed_task_inner(
        &database,
        instance_id,
        &claim,
        &services,
        redactor.as_ref(),
        cancellation,
    )
    .await;
    let reason = if result.is_ok() {
        "task execution finished"
    } else {
        "task execution failed"
    };
    database.release_schedule_claim(claim.schedule_claim_id, instance_id, Utc::now(), reason)?;
    result
}

#[allow(clippy::too_many_lines)]
async fn run_claimed_task_inner(
    database: &Database,
    instance_id: DaemonInstanceId,
    claim: &ClaimedTask,
    services: &ExecutionServices,
    redactor: &dyn MessageRedactor,
    cancellation: CancellationToken,
) -> Result<(), DaemonError> {
    transition(
        database,
        claim,
        TaskState::Queued,
        TaskState::Analyzing,
        false,
    )?;
    transition(
        database,
        claim,
        TaskState::Analyzing,
        TaskState::Planned,
        false,
    )?;
    transition(
        database,
        claim,
        TaskState::Planned,
        TaskState::Running,
        false,
    )?;
    let mut existing_worktree = None;
    loop {
        let mut instructions = Vec::new();
        while let Some(instruction) =
            database.claim_next_task_instruction(claim.task_id, Utc::now())?
        {
            instructions.push(instruction);
        }
        let execution_request = TaskExecutionRequest {
            claim: claim.clone(),
            repository_root: services.repository_root.clone(),
            state_root: services.state_root.clone(),
            instructions: instructions.clone(),
            existing_worktree: existing_worktree.clone(),
        };
        let execution = services
            .executor
            .execute(execution_request, cancellation.clone());
        tokio::pin!(execution);
        let renew_millis = (services.claim_ttl.num_milliseconds() / 3).max(100);
        let mut renew = tokio::time::interval(Duration::from_millis(
            u64::try_from(renew_millis).unwrap_or(u64::MAX),
        ));
        let result = loop {
            tokio::select! {
                result = &mut execution => break result,
                _ = renew.tick() => {
                    database.renew_schedule_claim(
                        claim.schedule_claim_id,
                        instance_id,
                        Utc::now(),
                        services.claim_ttl,
                    )?;
                }
            }
        };
        let report = match result {
            Ok(report) => report,
            Err(error) => {
                finish_instructions(database, &instructions, false)?;
                transition(
                    database,
                    claim,
                    TaskState::Running,
                    TaskState::Failed,
                    false,
                )?;
                let _redacted_failure = redactor.redact(&error.to_string());
                break;
            }
        };
        persist_report(
            database,
            claim,
            &report,
            &services.repository_root,
            redactor,
        )?;
        let passed = report.passed_completion_gate();
        finish_instructions(database, &instructions, passed)?;
        if report.outcome != WorkerOutcome::Succeeded || !passed {
            transition(
                database,
                claim,
                TaskState::Running,
                TaskState::Failed,
                false,
            )?;
            break;
        }
        existing_worktree = Some(GitWorktree {
            task_id: report.task_id,
            repository_root: canonicalize_directory(&services.repository_root).map_err(
                |error| {
                    DaemonError::InvalidSettings(format!(
                        "continued task repository root is unsafe: {error}"
                    ))
                },
            )?,
            path: report.worktree_path,
            branch: report.branch,
            base_revision: report.base_revision,
        });
        let task = database
            .load_task(claim.task_id)?
            .ok_or_else(|| DaemonError::InvalidSettings("claimed task disappeared".to_owned()))?;
        let occurred_at = Utc::now();
        if database.transition_running_to_verifying_if_instructions_drained(
            claim.task_id,
            task.revision,
            occurred_at,
            transition_event(claim, TaskState::Running, TaskState::Verifying, occurred_at),
        )? {
            transition(
                database,
                claim,
                TaskState::Verifying,
                TaskState::Completed,
                true,
            )?;
            break;
        }
    }
    Ok(())
}

fn persist_report(
    database: &Database,
    claim: &ClaimedTask,
    report: &TaskExecutionReport,
    repository_root: &std::path::Path,
    redactor: &dyn MessageRedactor,
) -> Result<(), DaemonError> {
    if report.task_id != claim.task_id || report.provider != claim.provider {
        return Err(DaemonError::InvalidSettings(
            "task execution report identity mismatch".to_owned(),
        ));
    }
    if let Some(worktree) = database.active_worktree(report.task_id)? {
        if worktree.repo_root != repository_root
            || worktree.worktree_path != report.worktree_path
            || worktree.branch_name != report.branch
            || worktree.base_revision != report.base_revision
        {
            return Err(DaemonError::InvalidSettings(
                "continued task worktree projection mismatch".to_owned(),
            ));
        }
    } else {
        database.record_active_worktree(&NewWorktreeRecord {
            task_id: report.task_id,
            repo_root: repository_root.to_path_buf(),
            worktree_path: report.worktree_path.clone(),
            branch_name: report.branch.clone(),
            base_revision: report.base_revision.clone(),
            created_at: Utc::now(),
        })?;
    }
    database.record_task_attempt_started(&NewTaskAttemptRecord {
        attempt_id: report.attempt_id,
        task_id: report.task_id,
        provider: report.provider,
        worker_mode: "workspace_write".to_owned(),
        started_at: claim.acquired_at,
    })?;
    if let Some(checkpoint) = report.checkpoint.as_ref() {
        database.record_checkpoint(checkpoint)?;
    }
    if let Some(verification) = report.verification.as_ref() {
        database.record_verification(verification)?;
    }
    let result = serde_json::json!({
        "task_id": report.task_id,
        "attempt_id": report.attempt_id,
        "provider": report.provider,
        "outcome": report.outcome,
        "summary_redacted": redactor.redact(&report.summary_redacted),
        "changed_files": report.changed_files,
        "checkpoint_id": report.checkpoint.as_ref().map(|value| value.checkpoint_id),
        "verification_id": report.verification.as_ref().map(|value| value.verification_id),
    });
    database.finish_task_attempt(
        report.attempt_id,
        worker_outcome_text(report.outcome),
        &result,
        Utc::now(),
    )?;
    Ok(())
}

fn finish_instructions(
    database: &Database,
    instructions: &[orchestrator_state::StoredTaskInstruction],
    applied: bool,
) -> Result<(), DaemonError> {
    for instruction in instructions {
        database.finish_task_instruction(
            instruction.instruction_id,
            if applied {
                TaskInstructionState::Applied
            } else {
                TaskInstructionState::Interrupted
            },
            Utc::now(),
            Some(if applied {
                "instruction included in verified task execution"
            } else {
                "task execution did not pass; instruction will be retried"
            }),
        )?;
    }
    Ok(())
}

fn transition(
    database: &Database,
    claim: &ClaimedTask,
    expected: TaskState,
    next: TaskState,
    verification_passed: bool,
) -> Result<(), DaemonError> {
    let task = database
        .load_task(claim.task_id)?
        .ok_or_else(|| DaemonError::InvalidSettings("claimed task disappeared".to_owned()))?;
    if task.state != expected {
        return Err(DaemonError::InvalidSettings(format!(
            "claimed task state changed from {expected:?} to {:?}",
            task.state
        )));
    }
    let occurred_at = Utc::now();
    database.transition_task_with_event(
        claim.task_id,
        task.revision,
        expected,
        next,
        None,
        false,
        &TransitionGuards {
            verification_passed,
            ..TransitionGuards::default()
        },
        occurred_at,
        transition_event(claim, expected, next, occurred_at),
    )?;
    Ok(())
}

fn transition_event(
    claim: &ClaimedTask,
    expected: TaskState,
    next: TaskState,
    occurred_at: chrono::DateTime<Utc>,
) -> TaskEvent {
    TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id: Some(claim.session_id),
        task_id: Some(claim.task_id),
        occurred_at,
        event_type: match next {
            TaskState::Running => EventType::WorkerStarted,
            TaskState::Verifying => EventType::VerificationStarted,
            TaskState::Completed => EventType::TaskCompleted,
            _ => EventType::StateTransitioned,
        },
        from_state: Some(expected),
        to_state: Some(next),
        reason: Some("approved task graph execution".to_owned()),
        actor: EventActor::Orchestrator,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload: serde_json::json!({
            "revision_id": claim.revision_id,
            "schedule_claim_id": claim.schedule_claim_id,
            "provider": claim.provider,
        }),
        previous_hash: None,
        event_hash: String::new(),
    }
}

const fn worker_outcome_text(outcome: WorkerOutcome) -> &'static str {
    match outcome {
        WorkerOutcome::Succeeded => "succeeded",
        WorkerOutcome::Failed => "failed",
        WorkerOutcome::Cancelled => "cancelled",
        WorkerOutcome::TimedOut => "timed_out",
        WorkerOutcome::QuotaExceeded => "quota_exceeded",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use chrono::{TimeDelta, Utc};
    use orchestrator_domain::{
        DaemonInstanceId, GraphRevisionId, MessageId, ProviderId, RepoPath, SchemaVersion,
        SessionId, TaskEnvelope, TaskId, TaskState, WorkerOutcome,
    };
    use orchestrator_engine::{
        EngineResult, TaskExecutionReport, TaskExecutionRequest, TaskExecutor,
    };
    use orchestrator_state::{DaemonLeaseRequest, Database};
    use rusqlite::params;
    use tokio_util::sync::CancellationToken;

    use super::{ExecutionServices, reap_finished_tasks, spawn_ready_tasks, stop_execution_jobs};
    use crate::MessageRedactor;

    struct IdentityRedactor;

    impl MessageRedactor for IdentityRedactor {
        fn redact(&self, value: &str) -> String {
            value.to_owned()
        }
    }

    struct FakeExecutor {
        active: AtomicUsize,
        maximum: AtomicUsize,
    }

    #[async_trait]
    impl TaskExecutor for FakeExecutor {
        async fn execute(
            &self,
            request: TaskExecutionRequest,
            cancellation: CancellationToken,
        ) -> EngineResult<TaskExecutionReport> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            tokio::select! {
                () = cancellation.cancelled() => {}
                () = tokio::time::sleep(Duration::from_millis(40)) => {}
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(TaskExecutionReport {
                task_id: request.claim.task_id,
                attempt_id: orchestrator_domain::AttemptId::new(),
                provider: request.claim.provider,
                outcome: if cancellation.is_cancelled() {
                    WorkerOutcome::Cancelled
                } else {
                    WorkerOutcome::Failed
                },
                summary_redacted: "fake execution".to_owned(),
                worktree_path: request
                    .state_root
                    .join("worktrees")
                    .join(request.claim.task_id.to_string()),
                branch: format!("task-{}", request.claim.task_id),
                base_revision: "0".repeat(40),
                changed_files: Vec::new(),
                checkpoint: None,
                verification: None,
            })
        }
    }

    fn seed_graph(
        database: &Database,
    ) -> Result<(SessionId, GraphRevisionId), Box<dyn std::error::Error>> {
        let session = SessionId::new();
        let message = MessageId::new();
        let revision = GraphRevisionId::new();
        let now = Utc::now().to_rfc3339();
        database.with_transaction(|transaction| {
            transaction.execute(
                "INSERT INTO sessions(session_id, schema_version, title, state, created_at, updated_at)
                 VALUES (?1, 'v1', 'parallel', 'running', ?2, ?2)",
                params![session.to_string(), now],
            )?;
            transaction.execute(
                "INSERT INTO conversation_messages(message_id, session_id, ordinal, role, kind,
                    state, content_redacted, created_at, finalized_at)
                 VALUES (?1, ?2, 1, 'user', 'user_message', 'final', 'goal', ?3, ?3)",
                params![message.to_string(), session.to_string(), now],
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
                    now,
                ],
            )?;
            transaction.execute(
                "INSERT INTO session_graph_heads(session_id, revision_id, updated_at)
                 VALUES (?1, ?2, ?3)",
                params![session.to_string(), revision.to_string(), now],
            )?;
            Ok(())
        })?;
        Ok((session, revision))
    }

    fn seed_task(
        database: &Database,
        session: SessionId,
        revision: GraphRevisionId,
        order: i64,
    ) -> Result<TaskId, Box<dyn std::error::Error>> {
        let task_id = TaskId::new();
        let now = Utc::now();
        let envelope = TaskEnvelope {
            schema_version: SchemaVersion::v1(),
            task_id,
            objective: format!("task {order}"),
            original_request_redacted: "goal".to_owned(),
            constraints: Vec::new(),
            acceptance_criteria: vec!["done".to_owned()],
            allowed_write_paths: vec![RepoPath::try_from(format!("src/task-{order}"))?],
            repository_wide_write_scope: false,
            assessment: None,
            created_at: now,
        };
        database.with_transaction(|transaction| {
            transaction.execute(
                "INSERT INTO tasks(task_id, schema_version, state, objective,
                    original_request_redacted, task_envelope_json, created_at, updated_at)
                 VALUES (?1, ?2, 'queued', ?3, 'goal', ?4, ?5, ?5)",
                params![
                    task_id.to_string(),
                    SchemaVersion::V1,
                    envelope.objective,
                    serde_json::to_string(&envelope)?,
                    now.to_rfc3339(),
                ],
            )?;
            transaction.execute(
                "INSERT INTO session_tasks(session_id, revision_id, task_id, node_key,
                    display_order, provider_id, model_profile)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'codex', 'standard')",
                params![
                    session.to_string(),
                    revision.to_string(),
                    task_id.to_string(),
                    format!("task-{order}"),
                    order,
                ],
            )?;
            Ok(())
        })?;
        Ok(task_id)
    }

    #[tokio::test]
    async fn scheduler_runs_disjoint_tasks_in_parallel_and_releases_all_claims()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let database = Arc::new(Database::open(directory.path().join("state.db"))?);
        database.migrate_with_backup(&directory.path().join("backups"))?;
        let daemon = DaemonInstanceId::new();
        database.acquire_daemon_lease(&DaemonLeaseRequest {
            instance_id: daemon,
            pid: 42,
            started_at: Utc::now(),
            ttl: TimeDelta::minutes(2),
        })?;
        let (session, revision) = seed_graph(&database)?;
        let first = seed_task(&database, session, revision, 1)?;
        let second = seed_task(&database, session, revision, 2)?;
        let executor = Arc::new(FakeExecutor {
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
        });
        let services = ExecutionServices {
            executor: executor.clone(),
            repository_root: std::fs::canonicalize(directory.path())?,
            state_root: std::fs::canonicalize(directory.path())?,
            global_limit: 2,
            provider_limits: BTreeMap::from([(ProviderId::Codex, 2)]),
            claim_ttl: TimeDelta::seconds(30),
        };
        let redactor: Arc<dyn MessageRedactor> = Arc::new(IdentityRedactor);
        let cancellation = CancellationToken::new();
        let mut jobs = Vec::new();
        spawn_ready_tasks(
            &database,
            daemon,
            &services,
            &redactor,
            &cancellation,
            &mut jobs,
        )?;
        assert_eq!(jobs.len(), 2);
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            reap_finished_tasks(&mut jobs).await?;
            if jobs.is_empty() {
                break;
            }
        }
        assert!(jobs.is_empty());
        assert_eq!(executor.maximum.load(Ordering::SeqCst), 2);
        assert_eq!(
            database.load_task(first)?.map(|task| task.state),
            Some(TaskState::Failed)
        );
        assert_eq!(
            database.load_task(second)?.map(|task| task.state),
            Some(TaskState::Failed)
        );
        database.with_connection(|connection| {
            let active: i64 = connection.query_row(
                "SELECT count(*) FROM task_schedule_claims WHERE released_at IS NULL",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(active, 0);
            Ok(())
        })?;
        stop_execution_jobs(&cancellation, jobs).await?;
        Ok(())
    }
}
