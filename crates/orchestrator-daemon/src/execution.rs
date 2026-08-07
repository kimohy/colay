use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use chrono::{TimeDelta, Utc};
use orchestrator_domain::{
    CorrelationId, DaemonInstanceId, EventActor, EventId, EventType, ProviderId, SchemaVersion,
    TaskEvent, TaskId, TaskInstructionState, TaskState, TransitionGuards, WorkerOutcome,
};
use orchestrator_engine::{
    GitWorktree, TaskExecutionReport, TaskExecutionRequest, TaskExecutor, canonicalize_directory,
};
use orchestrator_state::{
    ClaimReadyTaskRequest, ClaimedTask, CompletedTaskAttemptRecord, Database, NewTaskAttemptRecord,
    NewWorktreeRecord, WorkspaceDatabase, WorkspaceId,
};
use serde::{Deserialize, Serialize};
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

/// A revision-cursor snapshot emitted when a CLI attaches to a task already owned by this
/// daemon. Re-reading after `revision` changes is replay-safe and never creates a worker attempt.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ActiveTaskStatus {
    pub task_id: TaskId,
    pub state: TaskState,
    pub revision: u64,
    pub attempt_count: usize,
    pub updated_at: chrono::DateTime<Utc>,
}

pub(crate) fn active_task_status(
    database: &Database,
    workspace_id: WorkspaceId,
    task_id: TaskId,
) -> orchestrator_state::StateResult<Option<ActiveTaskStatus>> {
    let workspace = database.workspace(workspace_id);
    let Some(task) = workspace.load_task(task_id)? else {
        return Ok(None);
    };
    Ok(Some(ActiveTaskStatus {
        task_id,
        state: task.state,
        revision: task.revision,
        attempt_count: workspace.list_task_attempts(task_id)?.len(),
        updated_at: task.updated_at,
    }))
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
    workspace_id: WorkspaceId,
    instance_id: DaemonInstanceId,
    services: &ExecutionServices,
    redactor: &Arc<dyn MessageRedactor>,
    cancellation: &CancellationToken,
    jobs: &mut Vec<tokio::task::JoinHandle<Result<(), DaemonError>>>,
) -> Result<(), DaemonError> {
    let workspace = database.workspace(workspace_id);
    while jobs.len() < services.global_limit {
        let request = ClaimReadyTaskRequest {
            daemon_instance_id: instance_id,
            global_limit: services.global_limit,
            provider_limits: services.provider_limits.clone(),
            now: Utc::now(),
            ttl: services.claim_ttl,
        };
        let Some(claim) = workspace.claim_next_ready_task(&request)? else {
            break;
        };
        let job_database = Arc::clone(database);
        let job_services = services.clone();
        let job_redactor = Arc::clone(redactor);
        let job_cancellation = cancellation.child_token();
        jobs.push(tokio::spawn(async move {
            run_claimed_task(
                job_database,
                workspace_id,
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
    workspace_id: WorkspaceId,
    instance_id: DaemonInstanceId,
    claim: ClaimedTask,
    services: ExecutionServices,
    redactor: Arc<dyn MessageRedactor>,
    cancellation: CancellationToken,
) -> Result<(), DaemonError> {
    let workspace = database.workspace(workspace_id);
    let result = async {
        if claim.workspace_id != workspace_id {
            return Err(DaemonError::InvalidSettings(
                "claimed task workspace identity does not match its execution runtime".to_owned(),
            ));
        }
        let registration = database.load_workspace(workspace_id)?.ok_or_else(|| {
            DaemonError::InvalidSettings(
                "claimed task workspace registration disappeared".to_owned(),
            )
        })?;
        let runtime_repository = canonicalize_directory(&services.repository_root)
            .map_err(|error| DaemonError::InvalidSettings(error.to_string()))?;
        let registered_repository = canonicalize_directory(&registration.canonical_path)
            .map_err(|error| DaemonError::InvalidSettings(error.to_string()))?;
        if runtime_repository != registered_repository {
            return Err(DaemonError::InvalidSettings(
                "claimed task repository identity does not match its execution runtime".to_owned(),
            ));
        }
        run_claimed_task_inner(
            &workspace,
            instance_id,
            &claim,
            &services,
            redactor.as_ref(),
            cancellation,
        )
        .await
    }
    .await;
    let reason = if result.is_ok() {
        "task execution finished"
    } else {
        "task execution failed"
    };
    workspace.release_schedule_claim(claim.schedule_claim_id, instance_id, Utc::now(), reason)?;
    result
}

#[allow(clippy::too_many_lines)]
async fn run_claimed_task_inner(
    database: &WorkspaceDatabase<'_>,
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
        if !report_matches_execution_target(&report, claim, &services.state_root) {
            finish_instructions(database, &instructions, false)?;
            transition(
                database,
                claim,
                TaskState::Running,
                TaskState::Failed,
                false,
            )?;
            break;
        }
        if let Err(error) = persist_report(
            database,
            claim,
            &report,
            &services.repository_root,
            redactor,
        ) {
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

fn report_matches_execution_target(
    report: &TaskExecutionReport,
    claim: &ClaimedTask,
    state_root: &std::path::Path,
) -> bool {
    if !report.validates_claim(claim)
        || report.branch != format!("orchestrator/task-{}", report.task_id)
    {
        return false;
    }
    let Ok(managed_worktrees) = canonicalize_directory(&state_root.join("worktrees")) else {
        return false;
    };
    let Ok(reported_worktree) = canonicalize_directory(&report.worktree_path) else {
        return false;
    };
    reported_worktree == managed_worktrees.join(report.task_id.to_string())
}

fn persist_report(
    database: &WorkspaceDatabase<'_>,
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
    let result = serde_json::json!({
        "task_id": report.task_id,
        "attempt_id": report.attempt_id,
        "provider": report.provider,
        "outcome": report.outcome,
        "summary_redacted": redactor.redact(&report.summary_redacted),
        "changed_files": report.changed_files,
        "checkpoint_id": report.checkpoint.as_ref().map(|value| value.checkpoint_id),
        "verification_id": report.verification.as_ref().map(|value| value.verification_id),
        "lifecycle_failure": report.lifecycle_failure.as_ref().map(|failure| serde_json::json!({
            "code": failure.code.as_ref().map(|code| redactor.redact(code)),
            "summary": redactor.redact(&failure.summary),
            "retryable": failure.retryable,
            "occurred_at": failure.occurred_at,
        })),
    });
    database.record_completed_task_attempt(&CompletedTaskAttemptRecord {
        attempt: NewTaskAttemptRecord {
            attempt_id: report.attempt_id,
            task_id: report.task_id,
            provider: report.provider,
            worker_mode: "workspace_write".to_owned(),
            started_at: claim.acquired_at,
        },
        checkpoint: report.checkpoint.clone(),
        verification: report.verification.clone(),
        outcome: worker_outcome_text(report.outcome).to_owned(),
        worker_result: result,
        ended_at: Utc::now(),
    })?;
    Ok(())
}

fn finish_instructions(
    database: &WorkspaceDatabase<'_>,
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
                "task execution did not pass; instruction was not applied"
            }),
        )?;
    }
    Ok(())
}

fn transition(
    database: &WorkspaceDatabase<'_>,
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
        fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use chrono::{TimeDelta, Utc};
    use orchestrator_domain::{
        Checkpoint, CheckpointId, DaemonInstanceId, FailureRecord, GraphRevisionId, MessageId,
        ModelProfile, ProviderId, RepoPath, ResourceScope, ScheduleClaimId, SchemaVersion,
        SessionId, TaskEnvelope, TaskId, TaskState, VerificationId, VerificationResult,
        VerificationStatus, WorkerOutcome,
    };
    use orchestrator_engine::{
        EngineError, EngineResult, TaskExecutionReport, TaskExecutionRequest, TaskExecutor,
    };
    use orchestrator_state::{
        ClaimedTask, DaemonLeaseRequest, Database, NewWorktreeRecord, WorkspaceDatabase,
        WorkspaceId,
    };
    use rusqlite::params;
    use tokio_util::sync::CancellationToken;

    use super::{
        ExecutionServices, reap_finished_tasks, report_matches_execution_target, spawn_ready_tasks,
        stop_execution_jobs,
    };
    use crate::MessageRedactor;
    use crate::test_support::{with_workspace, with_workspace_transaction};

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
                lifecycle_failure: None,
            })
        }
    }

    struct NonRetryableExecutor {
        calls: AtomicUsize,
        wrong_task: AtomicBool,
        wrong_base: AtomicBool,
        wrong_verification: AtomicBool,
        verification_id: Option<VerificationId>,
    }

    #[async_trait]
    impl TaskExecutor for NonRetryableExecutor {
        async fn execute(
            &self,
            request: TaskExecutionRequest,
            _cancellation: CancellationToken,
        ) -> EngineResult<TaskExecutionReport> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let attempt_id = orchestrator_domain::AttemptId::new();
            let report_task_id = if self.wrong_task.load(Ordering::SeqCst) {
                TaskId::new()
            } else {
                request.claim.task_id
            };
            let report_base = if self.wrong_base.load(Ordering::SeqCst) {
                "1".repeat(40)
            } else {
                request.claim.approved_base_commit.clone()
            };
            let failure = FailureRecord {
                code: Some("app_server_protocol_error".to_owned()),
                summary: "provider result is unknown after writable dispatch".to_owned(),
                retryable: false,
                occurred_at: Utc::now(),
            };
            let checkpoint = Checkpoint {
                schema_version: SchemaVersion::v1(),
                checkpoint_id: CheckpointId::new(),
                task_id: report_task_id,
                attempt_id,
                objective: request.claim.envelope.objective.clone(),
                current_plan: Vec::new(),
                completed_steps: Vec::new(),
                pending_steps: Vec::new(),
                files_read: Vec::new(),
                files_changed: Vec::new(),
                git_base: Some(report_base.clone()),
                diff_path: None,
                commands_run: Vec::new(),
                tests: Vec::new(),
                decisions: Vec::new(),
                unresolved_questions: Vec::new(),
                known_failures: vec![failure.clone()],
                worker_claim: None,
                current_worker: request.claim.provider,
                concise_context_summary: failure.summary.clone(),
                created_at: Utc::now(),
                integrity_hash: String::new(),
            }
            .seal()?;
            let verification = if let Some(verification_id) = self.verification_id {
                Some(VerificationResult {
                    schema_version: SchemaVersion::v1(),
                    verification_id,
                    task_id: report_task_id,
                    implementation_provider: request.claim.provider,
                    reviewer_provider: None,
                    status: VerificationStatus::Fail,
                    checks: Vec::new(),
                    acceptance_criteria: Vec::new(),
                    changed_files: Vec::new(),
                    out_of_scope_files: Vec::new(),
                    unresolved_todos: Vec::new(),
                    requires_approval: false,
                    verified_at: Utc::now(),
                })
            } else if self.wrong_verification.load(Ordering::SeqCst) {
                let foreign_change = RepoPath::try_from("foreign/change.rs")
                    .map_err(|error| EngineError::InvalidRepoPath(error.to_string()))?;
                Some(VerificationResult {
                    schema_version: SchemaVersion::v1(),
                    verification_id: VerificationId::new(),
                    task_id: TaskId::new(),
                    implementation_provider: ProviderId::Agy,
                    reviewer_provider: None,
                    status: VerificationStatus::Fail,
                    checks: Vec::new(),
                    acceptance_criteria: Vec::new(),
                    changed_files: vec![foreign_change],
                    out_of_scope_files: Vec::new(),
                    unresolved_todos: Vec::new(),
                    requires_approval: false,
                    verified_at: Utc::now(),
                })
            } else {
                None
            };
            Ok(TaskExecutionReport {
                task_id: report_task_id,
                attempt_id,
                provider: request.claim.provider,
                outcome: WorkerOutcome::Failed,
                summary_redacted: failure.summary.clone(),
                worktree_path: request
                    .state_root
                    .join("worktrees")
                    .join(report_task_id.to_string()),
                branch: format!("orchestrator/task-{report_task_id}"),
                base_revision: report_base,
                changed_files: Vec::new(),
                checkpoint: Some(checkpoint),
                verification,
                lifecycle_failure: Some(failure),
            })
        }
    }

    fn target_claim() -> Result<ClaimedTask, Box<dyn std::error::Error>> {
        let now = Utc::now();
        let envelope = TaskEnvelope::new("target path", "target path", now);
        Ok(ClaimedTask {
            workspace_id: "00000000-0000-0000-0000-000000000002".parse::<WorkspaceId>()?,
            schedule_claim_id: ScheduleClaimId::new(),
            daemon_instance_id: DaemonInstanceId::new(),
            session_id: SessionId::new(),
            revision_id: GraphRevisionId::new(),
            task_id: envelope.task_id,
            node_key: "target-path".to_owned(),
            display_order: 1,
            provider: ProviderId::Codex,
            profile: ModelProfile::Standard,
            envelope,
            scope: ResourceScope {
                paths: Vec::new(),
                repository_wide: true,
            },
            approved_base_commit: "0".repeat(40),
            acquired_at: now,
            expires_at: now + TimeDelta::minutes(5),
        })
    }

    fn target_report(claim: &ClaimedTask, worktree_path: PathBuf) -> TaskExecutionReport {
        TaskExecutionReport {
            task_id: claim.task_id,
            attempt_id: orchestrator_domain::AttemptId::new(),
            provider: claim.provider,
            outcome: WorkerOutcome::Failed,
            summary_redacted: "target path report".to_owned(),
            worktree_path,
            branch: format!("orchestrator/task-{}", claim.task_id),
            base_revision: claim.approved_base_commit.clone(),
            changed_files: Vec::new(),
            checkpoint: None,
            verification: None,
            lifecycle_failure: None,
        }
    }

    #[test]
    fn execution_target_accepts_canonical_equivalent_path_and_rejects_other_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let state_root = directory.path().join("state");
        let alias_component = state_root.join("alias-component");
        let claim = target_claim()?;
        let expected_worktree = state_root.join("worktrees").join(claim.task_id.to_string());
        fs::create_dir_all(&expected_worktree)?;
        fs::create_dir_all(&alias_component)?;

        let lexical_state_root = alias_component.join("..");
        let mut report = target_report(&claim, fs::canonicalize(&expected_worktree)?);
        assert!(report_matches_execution_target(
            &report,
            &claim,
            &lexical_state_root
        ));

        let other_worktree = directory
            .path()
            .join("other")
            .join(claim.task_id.to_string());
        fs::create_dir_all(&other_worktree)?;
        report.worktree_path = fs::canonicalize(other_worktree)?;
        assert!(!report_matches_execution_target(
            &report,
            &claim,
            &lexical_state_root
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn execution_target_accepts_symlinked_state_root_for_same_managed_worktree()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let state_root = directory.path().join("state");
        let claim = target_claim()?;
        let expected_worktree = state_root.join("worktrees").join(claim.task_id.to_string());
        fs::create_dir_all(&expected_worktree)?;
        let state_alias = directory.path().join("state-alias");
        std::os::unix::fs::symlink(&state_root, &state_alias)?;
        let report = target_report(&claim, fs::canonicalize(expected_worktree)?);

        assert!(report_matches_execution_target(
            &report,
            &claim,
            &state_alias
        ));
        Ok(())
    }

    fn seed_graph(
        database_path: &std::path::Path,
        database: &WorkspaceDatabase<'_>,
    ) -> Result<(SessionId, GraphRevisionId), Box<dyn std::error::Error>> {
        let session = SessionId::new();
        let message = MessageId::new();
        let revision = GraphRevisionId::new();
        let now = Utc::now().to_rfc3339();
        with_workspace_transaction(database_path, database, |transaction| {
            transaction.execute(
                "INSERT INTO main.sessions(workspace_id, session_id, schema_version, title, state, created_at, updated_at)
                 VALUES (current_workspace(), ?1, 'v1', 'parallel', 'running', ?2, ?2)",
                params![session.to_string(), now],
            )?;
            transaction.execute(
                "INSERT INTO main.conversation_messages(workspace_id, message_id, session_id, ordinal, role, kind,
                    state, content_redacted, created_at, finalized_at)
                 VALUES (current_workspace(), ?1, ?2, 1, 'user', 'user_message', 'final', 'goal', ?3, ?3)",
                params![message.to_string(), session.to_string(), now],
            )?;
            transaction.execute(
                "INSERT INTO main.graph_revisions(workspace_id, revision_id, session_id, goal_message_id, ordinal,
                    status, proposal_hash, validation_json, planner_provider, created_at, completed_at)
                 VALUES (current_workspace(), ?1, ?2, ?3, 1, 'approved', ?4, '{}', 'codex', ?5, ?5)",
                params![
                    revision.to_string(),
                    session.to_string(),
                    message.to_string(),
                    "0".repeat(64),
                    now,
                ],
            )?;
            transaction.execute(
                "INSERT INTO main.session_graph_heads(workspace_id, session_id, revision_id, updated_at)
                 VALUES (current_workspace(), ?1, ?2, ?3)",
                params![session.to_string(), revision.to_string(), now],
            )?;
            transaction.execute(
                "INSERT INTO main.graph_approvals(
                    workspace_id, revision_id, proposal_hash, approved_by, approved_at,
                    session_id, base_commit)
                 VALUES (current_workspace(), ?1, ?2, 'daemon-execution-test', ?3, ?4, ?5)",
                params![
                    revision.to_string(),
                    "0".repeat(64),
                    now,
                    session.to_string(),
                    "0".repeat(40),
                ],
            )?;
            Ok(())
        })?;
        Ok((session, revision))
    }

    fn seed_task(
        database_path: &std::path::Path,
        database: &WorkspaceDatabase<'_>,
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
        with_workspace_transaction(database_path, database, |transaction| {
            transaction.execute(
                "INSERT INTO main.tasks(workspace_id, task_id, schema_version, state, objective,
                    original_request_redacted, task_envelope_json, created_at, updated_at)
                 VALUES (current_workspace(), ?1, ?2, 'queued', ?3, 'goal', ?4, ?5, ?5)",
                params![
                    task_id.to_string(),
                    SchemaVersion::V1,
                    envelope.objective,
                    serde_json::to_string(&envelope)?,
                    now.to_rfc3339(),
                ],
            )?;
            transaction.execute(
                "INSERT INTO main.session_tasks(workspace_id, session_id, revision_id, task_id, node_key,
                    display_order, provider_id, model_profile)
                 VALUES (current_workspace(), ?1, ?2, ?3, ?4, ?5, 'codex', 'standard')",
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

    async fn execute_one_ready_task(
        database: &Arc<Database>,
        workspace_id: orchestrator_state::WorkspaceId,
        daemon: DaemonInstanceId,
        services: &ExecutionServices,
        redactor: &Arc<dyn MessageRedactor>,
        cancellation: &CancellationToken,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut jobs = Vec::new();
        spawn_ready_tasks(
            database,
            workspace_id,
            daemon,
            services,
            redactor,
            cancellation,
            &mut jobs,
        )?;
        if jobs.len() != 1 {
            return Err(format!("expected one scheduled task, found {}", jobs.len()).into());
        }
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            reap_finished_tasks(&mut jobs).await?;
            if jobs.is_empty() {
                return Ok(());
            }
        }
        cancellation.cancel();
        stop_execution_jobs(cancellation, jobs).await?;
        Err("scheduled task did not finish in time".into())
    }

    #[tokio::test]
    async fn scheduler_runs_disjoint_tasks_in_parallel_and_releases_all_claims()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = std::fs::canonicalize(directory.path())?;
        let database_path = root.join("state.db");
        let database = Arc::new(Database::open(&database_path)?);
        database.migrate_with_backup(&root.join("backups"))?;
        let workspace_id = database.resolve_repository_workspace(&root)?.workspace_id;
        let workspace = database.workspace(workspace_id);
        let daemon = DaemonInstanceId::new();
        database.acquire_daemon_lease(&DaemonLeaseRequest {
            instance_id: daemon,
            pid: 42,
            started_at: Utc::now(),
            ttl: TimeDelta::minutes(2),
        })?;
        let (session, revision) = seed_graph(&database_path, &workspace)?;
        let first = seed_task(&database_path, &workspace, session, revision, 1)?;
        let second = seed_task(&database_path, &workspace, session, revision, 2)?;
        let executor = Arc::new(FakeExecutor {
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
        });
        let services = ExecutionServices {
            executor: executor.clone(),
            repository_root: root.clone(),
            state_root: root,
            global_limit: 2,
            provider_limits: BTreeMap::from([(ProviderId::Codex, 2)]),
            claim_ttl: TimeDelta::seconds(30),
        };
        let redactor: Arc<dyn MessageRedactor> = Arc::new(IdentityRedactor);
        let cancellation = CancellationToken::new();
        let mut jobs = Vec::new();
        spawn_ready_tasks(
            &database,
            workspace.workspace_id(),
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
            workspace.load_task(first)?.map(|task| task.state),
            Some(TaskState::Failed)
        );
        assert_eq!(
            workspace.load_task(second)?.map(|task| task.state),
            Some(TaskState::Failed)
        );
        with_workspace(&database_path, &workspace, |connection| {
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

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn non_retryable_report_is_terminal_after_one_attempt_and_preserves_failure_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = std::fs::canonicalize(directory.path())?;
        let database_path = root.join("state.db");
        let database = Arc::new(Database::open(&database_path)?);
        database.migrate_with_backup(&root.join("backups"))?;
        let workspace_id = database.resolve_repository_workspace(&root)?.workspace_id;
        let workspace = database.workspace(workspace_id);
        let daemon = DaemonInstanceId::new();
        database.acquire_daemon_lease(&DaemonLeaseRequest {
            instance_id: daemon,
            pid: 43,
            started_at: Utc::now(),
            ttl: TimeDelta::minutes(2),
        })?;
        let (session, revision) = seed_graph(&database_path, &workspace)?;
        let task_id = seed_task(&database_path, &workspace, session, revision, 1)?;
        fs::create_dir_all(root.join("worktrees").join(task_id.to_string()))?;
        let executor = Arc::new(NonRetryableExecutor {
            calls: AtomicUsize::new(0),
            wrong_task: AtomicBool::new(false),
            wrong_base: AtomicBool::new(false),
            wrong_verification: AtomicBool::new(false),
            verification_id: None,
        });
        let services = ExecutionServices {
            executor: executor.clone(),
            repository_root: root.clone(),
            state_root: root,
            global_limit: 1,
            provider_limits: BTreeMap::from([(ProviderId::Codex, 1)]),
            claim_ttl: TimeDelta::seconds(30),
        };
        let redactor: Arc<dyn MessageRedactor> = Arc::new(IdentityRedactor);
        let cancellation = CancellationToken::new();
        let mut jobs = Vec::new();

        spawn_ready_tasks(
            &database,
            workspace.workspace_id(),
            daemon,
            &services,
            &redactor,
            &cancellation,
            &mut jobs,
        )?;
        assert_eq!(jobs.len(), 1);
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            reap_finished_tasks(&mut jobs).await?;
            if jobs.is_empty() {
                break;
            }
        }

        assert!(jobs.is_empty());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        let task = workspace.load_task(task_id)?.ok_or("task missing")?;
        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(task.resume_state, None);
        let attempts = workspace.list_task_attempts(task_id)?;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].outcome.as_deref(), Some("failed"));
        assert!(attempts[0].ended_at.is_some());
        let persisted_failure = attempts[0]
            .worker_result
            .as_ref()
            .and_then(|result| result.get("lifecycle_failure"))
            .ok_or("attempt omitted lifecycle failure")?;
        assert_eq!(persisted_failure["code"], "app_server_protocol_error");
        assert_eq!(persisted_failure["retryable"].as_bool(), Some(false));
        let checkpoint = workspace
            .latest_sealed_checkpoint(task_id)?
            .ok_or("checkpoint missing")?;
        assert!(checkpoint.verify_integrity()?);
        assert_eq!(checkpoint.known_failures.len(), 1);
        assert!(!checkpoint.known_failures[0].retryable);
        assert_eq!(
            checkpoint.known_failures[0].code.as_deref(),
            Some("app_server_protocol_error")
        );
        assert_eq!(workspace.count_handovers(task_id)?, 0);
        with_workspace(&database_path, &workspace, |connection| {
            let active: i64 = connection.query_row(
                "SELECT count(*) FROM task_schedule_claims WHERE released_at IS NULL",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(active, 0);
            Ok(())
        })?;

        spawn_ready_tasks(
            &database,
            workspace.workspace_id(),
            daemon,
            &services,
            &redactor,
            &cancellation,
            &mut jobs,
        )?;
        assert!(jobs.is_empty());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        stop_execution_jobs(&cancellation, jobs).await?;
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn verification_conflict_rolls_back_report_evidence_and_releases_claim()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = std::fs::canonicalize(directory.path())?;
        let database_path = root.join("state.db");
        let database = Arc::new(Database::open(&database_path)?);
        database.migrate_with_backup(&root.join("backups"))?;
        let workspace_id = database.resolve_repository_workspace(&root)?.workspace_id;
        let workspace = database.workspace(workspace_id);
        let daemon = DaemonInstanceId::new();
        database.acquire_daemon_lease(&DaemonLeaseRequest {
            instance_id: daemon,
            pid: 45,
            started_at: Utc::now(),
            ttl: TimeDelta::minutes(2),
        })?;
        let (session, revision) = seed_graph(&database_path, &workspace)?;
        let task_id = seed_task(&database_path, &workspace, session, revision, 1)?;
        fs::create_dir_all(root.join("worktrees").join(task_id.to_string()))?;

        let verification_id = VerificationId::new();
        let existing_verification = VerificationResult {
            schema_version: SchemaVersion::v1(),
            verification_id,
            task_id,
            implementation_provider: ProviderId::Codex,
            reviewer_provider: None,
            status: VerificationStatus::Inconclusive,
            checks: Vec::new(),
            acceptance_criteria: Vec::new(),
            changed_files: Vec::new(),
            out_of_scope_files: Vec::new(),
            unresolved_todos: vec!["pre-existing verification evidence".to_owned()],
            requires_approval: false,
            verified_at: Utc::now(),
        };
        workspace.record_verification(&existing_verification)?;

        let executor = Arc::new(NonRetryableExecutor {
            calls: AtomicUsize::new(0),
            wrong_task: AtomicBool::new(false),
            wrong_base: AtomicBool::new(false),
            wrong_verification: AtomicBool::new(false),
            verification_id: Some(verification_id),
        });
        let services = ExecutionServices {
            executor: executor.clone(),
            repository_root: root.clone(),
            state_root: root,
            global_limit: 1,
            provider_limits: BTreeMap::from([(ProviderId::Codex, 1)]),
            claim_ttl: TimeDelta::seconds(30),
        };
        let redactor: Arc<dyn MessageRedactor> = Arc::new(IdentityRedactor);
        let cancellation = CancellationToken::new();

        execute_one_ready_task(
            &database,
            workspace_id,
            daemon,
            &services,
            &redactor,
            &cancellation,
        )
        .await?;

        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            workspace.load_task(task_id)?.map(|task| task.state),
            Some(TaskState::Failed)
        );
        assert!(workspace.list_task_attempts(task_id)?.is_empty());
        assert!(workspace.latest_sealed_checkpoint(task_id)?.is_none());
        assert_eq!(
            workspace.latest_verification(task_id)?,
            Some(existing_verification)
        );
        assert!(workspace.active_worktree(task_id)?.is_some());
        with_workspace(&database_path, &workspace, |connection| {
            let open_attempts: i64 = connection.query_row(
                "SELECT count(*) FROM task_attempts WHERE ended_at IS NULL",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(open_attempts, 0);
            let checkpoints: i64 =
                connection.query_row("SELECT count(*) FROM checkpoints", [], |row| row.get(0))?;
            assert_eq!(checkpoints, 0);
            let verifications: i64 =
                connection.query_row("SELECT count(*) FROM verification_results", [], |row| {
                    row.get(0)
                })?;
            assert_eq!(verifications, 1);
            let active_claims: i64 = connection.query_row(
                "SELECT count(*) FROM task_schedule_claims WHERE released_at IS NULL",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(active_claims, 0);
            Ok(())
        })?;
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn misbound_reports_fail_terminal_without_persistence_or_reclaim()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = std::fs::canonicalize(directory.path())?;
        let database_path = root.join("state.db");
        let database = Arc::new(Database::open(&database_path)?);
        database.migrate_with_backup(&root.join("backups"))?;
        let workspace_id = database.resolve_repository_workspace(&root)?.workspace_id;
        let workspace = database.workspace(workspace_id);
        let daemon = DaemonInstanceId::new();
        database.acquire_daemon_lease(&DaemonLeaseRequest {
            instance_id: daemon,
            pid: 44,
            started_at: Utc::now(),
            ttl: TimeDelta::minutes(2),
        })?;
        let (session, revision) = seed_graph(&database_path, &workspace)?;
        let wrong_task = seed_task(&database_path, &workspace, session, revision, 1)?;
        let wrong_base = seed_task(&database_path, &workspace, session, revision, 2)?;
        let mismatched_worktree = seed_task(&database_path, &workspace, session, revision, 3)?;
        let misbound_verification = seed_task(&database_path, &workspace, session, revision, 4)?;
        let executor = Arc::new(NonRetryableExecutor {
            calls: AtomicUsize::new(0),
            wrong_task: AtomicBool::new(true),
            wrong_base: AtomicBool::new(false),
            wrong_verification: AtomicBool::new(false),
            verification_id: None,
        });
        let services = ExecutionServices {
            executor: executor.clone(),
            repository_root: root.clone(),
            state_root: root.clone(),
            global_limit: 1,
            provider_limits: BTreeMap::from([(ProviderId::Codex, 1)]),
            claim_ttl: TimeDelta::seconds(30),
        };
        let redactor: Arc<dyn MessageRedactor> = Arc::new(IdentityRedactor);
        let cancellation = CancellationToken::new();

        execute_one_ready_task(
            &database,
            workspace_id,
            daemon,
            &services,
            &redactor,
            &cancellation,
        )
        .await?;
        assert_eq!(
            workspace.load_task(wrong_task)?.map(|task| task.state),
            Some(TaskState::Failed)
        );
        assert!(workspace.list_task_attempts(wrong_task)?.is_empty());
        assert!(workspace.active_worktree(wrong_task)?.is_none());

        executor.wrong_task.store(false, Ordering::SeqCst);
        executor.wrong_base.store(true, Ordering::SeqCst);
        execute_one_ready_task(
            &database,
            workspace_id,
            daemon,
            &services,
            &redactor,
            &cancellation,
        )
        .await?;
        assert_eq!(
            workspace.load_task(wrong_base)?.map(|task| task.state),
            Some(TaskState::Failed)
        );
        assert!(workspace.list_task_attempts(wrong_base)?.is_empty());
        assert!(workspace.active_worktree(wrong_base)?.is_none());

        executor.wrong_base.store(false, Ordering::SeqCst);
        fs::create_dir_all(root.join("worktrees").join(mismatched_worktree.to_string()))?;
        workspace.record_active_worktree(&NewWorktreeRecord {
            task_id: mismatched_worktree,
            repo_root: root.clone(),
            worktree_path: root.join("worktrees/mismatched"),
            branch_name: "orchestrator/task-mismatched".to_owned(),
            base_revision: "0".repeat(40),
            created_at: Utc::now(),
        })?;
        execute_one_ready_task(
            &database,
            workspace_id,
            daemon,
            &services,
            &redactor,
            &cancellation,
        )
        .await?;
        assert_eq!(
            workspace
                .load_task(mismatched_worktree)?
                .map(|task| task.state),
            Some(TaskState::Failed)
        );
        assert!(
            workspace
                .list_task_attempts(mismatched_worktree)?
                .is_empty()
        );
        assert!(workspace.active_worktree(mismatched_worktree)?.is_some());

        executor.wrong_verification.store(true, Ordering::SeqCst);
        fs::create_dir_all(
            root.join("worktrees")
                .join(misbound_verification.to_string()),
        )?;
        execute_one_ready_task(
            &database,
            workspace_id,
            daemon,
            &services,
            &redactor,
            &cancellation,
        )
        .await?;
        assert_eq!(
            workspace
                .load_task(misbound_verification)?
                .map(|task| task.state),
            Some(TaskState::Failed)
        );
        assert!(
            workspace
                .list_task_attempts(misbound_verification)?
                .is_empty()
        );
        assert!(workspace.active_worktree(misbound_verification)?.is_none());
        assert!(
            workspace
                .latest_sealed_checkpoint(misbound_verification)?
                .is_none()
        );
        assert!(
            workspace
                .latest_verification(misbound_verification)?
                .is_none()
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 4);

        let mut jobs = Vec::new();
        spawn_ready_tasks(
            &database,
            workspace_id,
            daemon,
            &services,
            &redactor,
            &cancellation,
            &mut jobs,
        )?;
        assert!(jobs.is_empty());
        with_workspace(&database_path, &workspace, |connection| {
            let active: i64 = connection.query_row(
                "SELECT count(*) FROM task_schedule_claims WHERE released_at IS NULL",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(active, 0);
            let verification_results: i64 =
                connection.query_row("SELECT count(*) FROM verification_results", [], |row| {
                    row.get(0)
                })?;
            assert_eq!(verification_results, 0);
            Ok(())
        })?;
        stop_execution_jobs(&cancellation, jobs).await?;
        Ok(())
    }
}
