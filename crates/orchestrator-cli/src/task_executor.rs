use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use orchestrator_domain::{
    AcceptanceEvidence, AttemptId, FailureRecord, SandboxMode, SchemaVersion, UntrustedWorkerClaim,
    VerificationStatus, WorkerEvent, WorkerOutcome, WorkerRequest,
};
use orchestrator_engine::{
    CheckpointInput, CheckpointManager, EngineError, EngineResult, GitCheckpointEvidence,
    GitWorktreeManager, TaskExecutionReport, TaskExecutionRequest, TaskExecutor,
    VerificationEngine, VerificationInput, canonicalize_directory,
};
use orchestrator_providers::{AdapterRuntime, RuntimeTermination, WorkerAdapter};
use orchestrator_state::{ArtifactStore, RootConfig};
use tokio_util::sync::CancellationToken;

use crate::{
    task_planner::{build_provider_adapter, profile_settings},
    worker_messages::WorkerMessageCollector,
};

pub struct OfficialCliTaskExecutor {
    config: RootConfig,
    repository_root: PathBuf,
    runtime: Arc<dyn AdapterRuntime>,
    worktree_creation: tokio::sync::Mutex<()>,
}

impl OfficialCliTaskExecutor {
    /// Creates a writable executor rooted at an existing canonical repository.
    ///
    /// # Errors
    ///
    /// Returns an engine error when the repository is missing or cannot be canonicalized.
    pub fn new(
        config: &RootConfig,
        repository_root: &Path,
        runtime: Arc<dyn AdapterRuntime>,
    ) -> EngineResult<Self> {
        let repository_root = canonicalize_directory(repository_root)?;
        if !repository_root.is_dir() {
            return Err(EngineError::UnsafePath(repository_root));
        }
        Ok(Self {
            config: config.clone(),
            repository_root,
            runtime,
            worktree_creation: tokio::sync::Mutex::new(()),
        })
    }

    fn worker_request(
        &self,
        request: &TaskExecutionRequest,
        workspace_root: PathBuf,
        attempt_id: AttemptId,
    ) -> EngineResult<WorkerRequest> {
        let claim = &request.claim;
        let (model, reasoning_effort) =
            profile_settings(&self.config.orchestrator, claim.provider, claim.profile)
                .map_err(|error| invocation_error(claim.provider.as_str(), &error.to_string()))?;
        let instructions = request
            .instructions
            .iter()
            .map(|instruction| instruction.content_redacted.as_str())
            .collect::<Vec<_>>();
        let prompt = serde_json::to_string(&serde_json::json!({
            "objective": claim.envelope.objective,
            "original_request": claim.envelope.original_request_redacted,
            "task_instructions": instructions,
            "write_scopes": claim.scope.paths,
            "repository_wide_write_scope": claim.scope.repository_wide,
            "required_result": "Perform the task in this isolated worktree and emit structured completion evidence."
        }))?;
        Ok(WorkerRequest {
            schema_version: SchemaVersion::v1(),
            task_id: claim.task_id,
            attempt_id,
            provider: claim.provider,
            objective: claim.envelope.objective.clone(),
            prompt,
            constraints: claim.envelope.constraints.clone(),
            acceptance_criteria: claim.envelope.acceptance_criteria.clone(),
            workspace_root,
            sandbox: SandboxMode::WorkspaceWrite,
            profile: claim.profile,
            model,
            reasoning_effort,
            timeout_seconds: self
                .config
                .orchestrator
                .default_timeout_minutes
                .saturating_mul(60)
                .clamp(1, 86_400),
            max_output_bytes: 8 * 1024 * 1024,
            resume_session_id: None,
            handover_payload: None,
        })
    }
}

#[async_trait]
impl TaskExecutor for OfficialCliTaskExecutor {
    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        request: TaskExecutionRequest,
        cancellation: CancellationToken,
    ) -> EngineResult<TaskExecutionReport> {
        if canonicalize_directory(&request.repository_root)? != self.repository_root {
            return Err(EngineError::UnsafePath(request.repository_root));
        }
        let worktrees_root = request.state_root.join("worktrees");
        let manager = GitWorktreeManager::open(&self.repository_root, &worktrees_root)?;
        let worktree = if let Some(worktree) = request.existing_worktree.clone() {
            if worktree.task_id != request.claim.task_id
                || worktree.repository_root != self.repository_root
                || worktree.base_revision != request.claim.approved_base_commit
            {
                return Err(EngineError::IntegrityMismatch {
                    artifact: "continued task worktree identity",
                });
            }
            manager.snapshot(&worktree).await?;
            worktree
        } else {
            let _creation_guard = self.worktree_creation.lock().await;
            manager
                .create(request.claim.task_id, &request.claim.approved_base_commit)
                .await?
        };
        let attempt_id = AttemptId::new();
        let worker_request = self.worker_request(&request, worktree.path.clone(), attempt_id)?;
        let adapter: Arc<dyn WorkerAdapter> = Arc::from(
            build_provider_adapter(
                request.claim.provider,
                &self.config,
                Arc::clone(&self.runtime),
                &self.repository_root,
            )
            .map_err(|error| {
                invocation_error(request.claim.provider.as_str(), &error.to_string())
            })?,
        );
        let handle = adapter
            .start(worker_request.clone())
            .await
            .map_err(|error| {
                invocation_error(request.claim.provider.as_str(), &error.to_string())
            })?;
        let mut completed = false;
        let mut quota_exhausted = false;
        let mut lifecycle_failure = None;
        let mut summaries = WorkerMessageCollector::new(
            &self.config.orchestrator.redaction.patterns,
            worker_request.max_output_bytes,
        )
        .map_err(|error| invocation_error("redaction", &error.to_string()))?;
        loop {
            let raw = tokio::select! {
                () = cancellation.cancelled() => {
                    if let Err(error) = adapter.cancel(&handle).await {
                        let error = summaries.redact_provider_text(&error.to_string());
                        record_lifecycle_failure(
                            &mut lifecycle_failure,
                            Some("provider_cancel_error".to_owned()),
                            &error,
                            false,
                            Utc::now(),
                        );
                    }
                    break;
                }
                raw = adapter.next_event(&handle) => match raw {
                    Ok(raw) => raw,
                    Err(error) => {
                        let error = summaries.redact_provider_text(&error.to_string());
                        record_lifecycle_failure(
                            &mut lifecycle_failure,
                            Some("provider_event_error".to_owned()),
                            &error,
                            false,
                            Utc::now(),
                        );
                        if let Err(error) = adapter.cancel(&handle).await {
                            let error = summaries.redact_provider_text(&error.to_string());
                            record_lifecycle_failure(
                                &mut lifecycle_failure,
                                Some("provider_cancel_error".to_owned()),
                                &error,
                                false,
                                Utc::now(),
                            );
                        }
                        break;
                    }
                },
            };
            let Some(raw) = raw else { break };
            let occurred_at = raw.received_at;
            match adapter.parse_event(raw).await {
                Ok(event) => {
                    if let Err(error) = summaries.observe(&event) {
                        let error = summaries.redact_provider_text(&error.to_string());
                        record_lifecycle_failure(
                            &mut lifecycle_failure,
                            Some("provider_output_limit_exceeded".to_owned()),
                            &error,
                            false,
                            occurred_at,
                        );
                        if let Err(error) = adapter.cancel(&handle).await {
                            let error = summaries.redact_provider_text(&error.to_string());
                            record_lifecycle_failure(
                                &mut lifecycle_failure,
                                Some("provider_cancel_error".to_owned()),
                                &error,
                                false,
                                Utc::now(),
                            );
                        }
                        break;
                    }
                    match event {
                        WorkerEvent::Completed { summary, .. } => {
                            completed = true;
                            if let Some(summary) = summary
                                && let Err(error) = summaries.push_message(&summary)
                            {
                                let error = summaries.redact_provider_text(&error.to_string());
                                record_lifecycle_failure(
                                    &mut lifecycle_failure,
                                    Some("provider_output_limit_exceeded".to_owned()),
                                    &error,
                                    false,
                                    occurred_at,
                                );
                                if let Err(error) = adapter.cancel(&handle).await {
                                    let error = summaries.redact_provider_text(&error.to_string());
                                    record_lifecycle_failure(
                                        &mut lifecycle_failure,
                                        Some("provider_cancel_error".to_owned()),
                                        &error,
                                        false,
                                        Utc::now(),
                                    );
                                }
                                break;
                            }
                        }
                        WorkerEvent::QuotaExceeded { detail } => {
                            quota_exhausted = true;
                            let detail = detail.as_deref().map_or(
                                "provider reported quota exhaustion".to_owned(),
                                |detail| summaries.redact_provider_text(detail),
                            );
                            record_lifecycle_failure(
                                &mut lifecycle_failure,
                                Some("provider_quota_exceeded".to_owned()),
                                &detail,
                                true,
                                occurred_at,
                            );
                        }
                        WorkerEvent::Error {
                            code,
                            message,
                            retryable,
                        } => {
                            let code = code.map(|code| summaries.redact_provider_text(&code));
                            let message = summaries.redact_provider_text(&message);
                            record_lifecycle_failure(
                                &mut lifecycle_failure,
                                code,
                                &message,
                                retryable,
                                occurred_at,
                            );
                            if let Err(error) = adapter.cancel(&handle).await {
                                let error = summaries.redact_provider_text(&error.to_string());
                                record_lifecycle_failure(
                                    &mut lifecycle_failure,
                                    Some("provider_cancel_error".to_owned()),
                                    &error,
                                    false,
                                    Utc::now(),
                                );
                            }
                            break;
                        }
                        WorkerEvent::Unknown {
                            event_type,
                            affects_lifecycle: true,
                            ..
                        } => {
                            let event_type = summaries.redact_provider_text(&event_type);
                            record_lifecycle_failure(
                                &mut lifecycle_failure,
                                Some("unknown_lifecycle_event".to_owned()),
                                &format!("unknown lifecycle event: {event_type}"),
                                false,
                                occurred_at,
                            );
                            if let Err(error) = adapter.cancel(&handle).await {
                                let error = summaries.redact_provider_text(&error.to_string());
                                record_lifecycle_failure(
                                    &mut lifecycle_failure,
                                    Some("provider_cancel_error".to_owned()),
                                    &error,
                                    false,
                                    Utc::now(),
                                );
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                Err(error) => {
                    summaries.discard_streamed_output();
                    let error = summaries.redact_provider_text(&error.to_string());
                    record_lifecycle_failure(
                        &mut lifecycle_failure,
                        Some("provider_compatibility_error".to_owned()),
                        &error,
                        false,
                        occurred_at,
                    );
                    if let Err(error) = adapter.cancel(&handle).await {
                        let error = summaries.redact_provider_text(&error.to_string());
                        record_lifecycle_failure(
                            &mut lifecycle_failure,
                            Some("provider_cancel_error".to_owned()),
                            &error,
                            false,
                            Utc::now(),
                        );
                    }
                    break;
                }
            }
        }
        let output = adapter.wait(&handle).await.map_err(|error| {
            let error = summaries.redact_provider_text(&error.to_string());
            invocation_error(request.claim.provider.as_str(), &error)
        })?;
        if let Some(error) = output.tree_termination_error.as_ref() {
            let error = summaries.redact_provider_text(error);
            return Err(invocation_error(
                request.claim.provider.as_str(),
                &format!("provider process-tree termination was not confirmed: {error}"),
            ));
        }
        if output.truncated {
            summaries.discard_streamed_output();
            record_lifecycle_failure(
                &mut lifecycle_failure,
                Some("provider_output_limit_exceeded".to_owned()),
                "provider output exceeded the configured limit",
                false,
                Utc::now(),
            );
        }
        if !cancellation.is_cancelled() && !quota_exhausted {
            match output.termination {
                RuntimeTermination::TimedOut => record_lifecycle_failure(
                    &mut lifecycle_failure,
                    Some("provider_timeout".to_owned()),
                    "provider execution timed out",
                    true,
                    Utc::now(),
                ),
                RuntimeTermination::Cancelled => record_lifecycle_failure(
                    &mut lifecycle_failure,
                    Some("provider_cancelled".to_owned()),
                    "provider execution was cancelled before completion",
                    true,
                    Utc::now(),
                ),
                RuntimeTermination::Exited if output.exit_code != Some(0) => {
                    record_lifecycle_failure(
                        &mut lifecycle_failure,
                        Some("provider_process_exit".to_owned()),
                        &format!(
                            "provider process exited with code {}",
                            output
                                .exit_code
                                .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
                        ),
                        false,
                        Utc::now(),
                    );
                }
                RuntimeTermination::Exited if !completed => record_lifecycle_failure(
                    &mut lifecycle_failure,
                    Some("provider_completion_missing".to_owned()),
                    "provider exited without structured completion evidence",
                    false,
                    Utc::now(),
                ),
                RuntimeTermination::Exited => {}
            }
        }
        let outcome = if cancellation.is_cancelled() {
            WorkerOutcome::Cancelled
        } else if quota_exhausted {
            WorkerOutcome::QuotaExceeded
        } else {
            match output.termination {
                RuntimeTermination::TimedOut => WorkerOutcome::TimedOut,
                RuntimeTermination::Cancelled if lifecycle_failure.is_some() => {
                    WorkerOutcome::Failed
                }
                RuntimeTermination::Cancelled => WorkerOutcome::Cancelled,
                RuntimeTermination::Exited
                    if output.exit_code == Some(0) && completed && lifecycle_failure.is_none() =>
                {
                    WorkerOutcome::Succeeded
                }
                RuntimeTermination::Exited => WorkerOutcome::Failed,
            }
        };
        let mut summaries = summaries.into_messages();
        if let Some(failure) = lifecycle_failure.as_ref() {
            summaries.push(failure.summary.clone());
        }
        let summary = bounded_summary(&summaries.join("\n"));
        let snapshot = manager.snapshot(&worktree).await?;
        let verification_engine = VerificationEngine::new()
            .map_err(|error| invocation_error("verification", &error.to_string()))?;
        let preflight = verification_engine.preflight_persistence(&worktree.path, &snapshot)?;
        if !preflight.safe_to_persist_or_share() {
            return Err(EngineError::IntegrityMismatch {
                artifact: "task worktree secret scan",
            });
        }
        let worker_claim = UntrustedWorkerClaim {
            provider: request.claim.provider,
            summary: summary.clone(),
            claimed_files_changed: Vec::new(),
            claimed_tests_passed: Vec::new(),
        };
        let checkpoint = CheckpointManager::new(ArtifactStore::open(&request.state_root)?).create(
            CheckpointInput {
                task_id: request.claim.task_id,
                attempt_id,
                objective: request.claim.envelope.objective.clone(),
                current_plan: Vec::new(),
                completed_steps: Vec::new(),
                pending_steps: Vec::new(),
                files_read: Vec::new(),
                commands_run: Vec::new(),
                tests: Vec::new(),
                decisions: Vec::new(),
                unresolved_questions: Vec::new(),
                known_failures: lifecycle_failure.iter().cloned().collect(),
                worker_claim: Some(worker_claim),
                current_worker: request.claim.provider,
                concise_context_summary: summary.clone(),
                created_at: Utc::now(),
            },
            GitCheckpointEvidence::from(&snapshot),
        )?;
        let criteria_status = if outcome == WorkerOutcome::Succeeded {
            VerificationStatus::Pass
        } else {
            VerificationStatus::Fail
        };
        let acceptance_criteria = request
            .claim
            .envelope
            .acceptance_criteria
            .iter()
            .map(|criterion| AcceptanceEvidence {
                criterion: criterion.clone(),
                status: criteria_status,
                evidence: vec![
                    "official CLI structured completion and authoritative Git snapshot".to_owned(),
                ],
            })
            .collect();
        let expected_paths = if request.claim.scope.repository_wide {
            snapshot.changed_files.clone()
        } else {
            request.claim.scope.paths.clone()
        };
        let verification = verification_engine.verify(VerificationInput {
            task_id: request.claim.task_id,
            implementation_provider: request.claim.provider,
            reviewer_provider: None,
            independent_review_required: false,
            independent_review_passed: false,
            snapshot: snapshot.clone(),
            worktree_root: worktree.path.clone(),
            expected_paths,
            commands: Vec::new(),
            tests: Vec::new(),
            acceptance_criteria,
            unresolved_todos: Vec::new(),
            verified_at: Utc::now(),
        })?;
        Ok(TaskExecutionReport {
            task_id: request.claim.task_id,
            attempt_id,
            provider: request.claim.provider,
            outcome,
            summary_redacted: summary,
            worktree_path: worktree.path,
            branch: worktree.branch,
            base_revision: worktree.base_revision,
            changed_files: snapshot.changed_files,
            checkpoint: Some(checkpoint),
            verification: Some(verification),
            lifecycle_failure,
        })
    }
}

fn record_lifecycle_failure(
    current: &mut Option<FailureRecord>,
    code: Option<String>,
    summary: &str,
    retryable: bool,
    occurred_at: DateTime<Utc>,
) {
    let code = code
        .map(|code| bounded_summary(code.trim()))
        .filter(|code| !code.is_empty());
    let summary = if summary.trim().is_empty() {
        "provider lifecycle failure".to_owned()
    } else {
        bounded_summary(summary.trim())
    };
    if let Some(existing) = current.as_mut() {
        let promotes_non_retryable_cause = existing.retryable && !retryable;
        existing.retryable &= retryable;
        if promotes_non_retryable_cause {
            existing.code = code;
            existing.occurred_at = occurred_at;
        } else if existing.code.is_none() {
            existing.code = code;
        }
        if existing.summary != summary {
            existing.summary = bounded_summary(&format!("{}\n{summary}", existing.summary));
        }
        return;
    }
    *current = Some(FailureRecord {
        code,
        summary,
        retryable,
        occurred_at,
    });
}

fn invocation_error(executable: &str, message: &str) -> EngineError {
    EngineError::CommandFailed {
        executable: executable.to_owned(),
        exit_code: None,
        message: bounded_summary(message),
    }
}

fn bounded_summary(value: &str) -> String {
    value.chars().take(4_096).collect()
}

#[cfg(all(test, feature = "test-fixtures"))]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::Arc,
    };

    use chrono::{TimeDelta, Utc};
    use orchestrator_domain::{
        DaemonInstanceId, GraphRevisionId, ModelProfile, ProviderId, ResourceScope,
        ScheduleClaimId, SessionId, TaskEnvelope, WorkerOutcome,
    };
    use orchestrator_engine::{TaskExecutionRequest, TaskExecutor};
    use orchestrator_providers::AdapterRuntime;
    use orchestrator_state::{ClaimedTask, RootConfig, WorkspaceId};
    use orchestrator_test_support::{FakeAdapterRuntime, FakeRuntimeScenario};
    use tokio_util::sync::CancellationToken;

    use super::{OfficialCliTaskExecutor, record_lifecycle_failure};

    fn git(repository: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
        let output = Command::new("git")
            .current_dir(repository)
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn repository() -> Result<(tempfile::TempDir, PathBuf, String), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let repository = temporary.path().join("repository");
        fs::create_dir_all(&repository)?;
        fs::write(repository.join("README.md"), "fake provider fixture\n")?;
        git(&repository, &["init"])?;
        git(&repository, &["config", "user.name", "Task Executor Test"])?;
        git(
            &repository,
            &["config", "user.email", "task-executor@example.invalid"],
        )?;
        git(&repository, &["config", "commit.gpgsign", "false"])?;
        git(&repository, &["add", "."])?;
        git(&repository, &["commit", "-m", "fixture base"])?;
        let base = git(&repository, &["rev-parse", "HEAD"])?;
        Ok((temporary, fs::canonicalize(repository)?, base))
    }

    async fn execute_fake_agy(
        scenario: FakeRuntimeScenario,
    ) -> Result<
        (
            orchestrator_engine::TaskExecutionReport,
            Arc<FakeAdapterRuntime>,
        ),
        Box<dyn std::error::Error>,
    > {
        let (temporary, repository, base) = repository()?;
        let fake_executable = temporary.path().join(if cfg!(windows) {
            "fake-provider-cli.exe"
        } else {
            "fake-provider-cli"
        });
        fs::copy(std::env::current_exe()?, &fake_executable)?;
        let runtime = Arc::new(FakeAdapterRuntime::new(&fake_executable, scenario)?);
        let runtime_adapter: Arc<dyn AdapterRuntime> = runtime.clone();
        let mut config = RootConfig::default();
        config
            .orchestrator
            .providers
            .agy
            .as_mut()
            .ok_or("default Agy provider is missing")?
            .executable = fake_executable.to_string_lossy().into_owned();
        let executor = OfficialCliTaskExecutor::new(&config, &repository, runtime_adapter)?;
        let now = Utc::now();
        let envelope = TaskEnvelope::new("exercise fake Agy", "exercise fake Agy", now);
        let task_id = envelope.task_id;
        let report = executor
            .execute(
                TaskExecutionRequest {
                    claim: ClaimedTask {
                        workspace_id: "00000000-0000-0000-0000-000000000002"
                            .parse::<WorkspaceId>()?,
                        schedule_claim_id: ScheduleClaimId::new(),
                        daemon_instance_id: DaemonInstanceId::new(),
                        session_id: SessionId::new(),
                        revision_id: GraphRevisionId::new(),
                        task_id,
                        node_key: "fake-agy".to_owned(),
                        display_order: 1,
                        provider: ProviderId::Agy,
                        profile: ModelProfile::Standard,
                        envelope,
                        scope: ResourceScope {
                            paths: Vec::new(),
                            repository_wide: true,
                        },
                        approved_base_commit: base,
                        acquired_at: now,
                        expires_at: now + TimeDelta::minutes(5),
                    },
                    repository_root: repository,
                    state_root: temporary.path().join("state"),
                    instructions: Vec::new(),
                    existing_worktree: None,
                },
                CancellationToken::new(),
            )
            .await?;
        Ok((report, runtime))
    }

    #[tokio::test]
    async fn fake_agy_process_crash_preserves_non_retryable_failure_in_checkpoint()
    -> Result<(), Box<dyn std::error::Error>> {
        let (report, runtime) = execute_fake_agy(FakeRuntimeScenario::ProcessCrash).await?;

        assert_eq!(report.outcome, WorkerOutcome::Failed);
        let failure = report
            .lifecycle_failure
            .as_ref()
            .ok_or("lifecycle failure missing")?;
        assert_eq!(failure.code.as_deref(), Some("agy_process_exit"));
        assert!(!failure.retryable);
        assert_eq!(report.non_retryable_failure(), Some(failure));
        let checkpoint = report.checkpoint.as_ref().ok_or("checkpoint missing")?;
        assert!(checkpoint.verify_integrity()?);
        assert_eq!(checkpoint.known_failures, vec![failure.clone()]);
        assert_eq!(runtime.started_job_count().await, 1);
        assert_eq!(runtime.cancelled_job_count().await, 1);
        Ok(())
    }

    #[tokio::test]
    async fn fake_agy_success_has_no_lifecycle_failure() -> Result<(), Box<dyn std::error::Error>> {
        let (report, _) = execute_fake_agy(FakeRuntimeScenario::Success).await?;

        assert_eq!(report.outcome, WorkerOutcome::Succeeded);
        assert_eq!(report.lifecycle_failure, None);
        assert!(report.validate_failure_contract());
        assert!(
            report
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.known_failures.is_empty())
        );
        Ok(())
    }

    #[test]
    fn lifecycle_failure_retryability_is_sticky_false() -> Result<(), Box<dyn std::error::Error>> {
        let first_at = Utc::now();
        let mut failure = None;
        record_lifecycle_failure(
            &mut failure,
            Some("temporary_failure".to_owned()),
            "temporary provider failure",
            true,
            first_at,
        );
        record_lifecycle_failure(
            &mut failure,
            Some("output_truncated".to_owned()),
            "provider output was truncated",
            false,
            first_at + TimeDelta::seconds(1),
        );
        record_lifecycle_failure(
            &mut failure,
            None,
            "later retryable detail",
            true,
            first_at + TimeDelta::seconds(2),
        );

        let failure = failure.ok_or("failure should be recorded")?;
        assert!(!failure.retryable);
        assert_eq!(failure.occurred_at, first_at + TimeDelta::seconds(1));
        assert_eq!(failure.code.as_deref(), Some("output_truncated"));
        Ok(())
    }
}
