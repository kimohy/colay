use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use orchestrator_domain::{
    AcceptanceEvidence, AttemptId, SandboxMode, SchemaVersion, UntrustedWorkerClaim,
    VerificationStatus, WorkerEvent, WorkerOutcome, WorkerRequest,
};
use orchestrator_engine::{
    CheckpointInput, CheckpointManager, EngineError, EngineResult, GitCheckpointEvidence,
    GitWorktreeManager, TaskExecutionReport, TaskExecutionRequest, TaskExecutor,
    VerificationEngine, VerificationInput,
};
use orchestrator_providers::{AdapterRuntime, RuntimeTermination, WorkerAdapter};
use orchestrator_state::{ArtifactStore, RootConfig};
use tokio_util::sync::CancellationToken;

use crate::task_planner::{build_provider_adapter, profile_settings};

pub struct OfficialCliTaskExecutor {
    config: RootConfig,
    repository_root: PathBuf,
    runtime: Arc<dyn AdapterRuntime>,
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
        let repository_root =
            std::fs::canonicalize(repository_root).map_err(|error| EngineError::CommandFailed {
                executable: "git".to_owned(),
                exit_code: None,
                message: error.to_string(),
            })?;
        if !repository_root.is_dir() {
            return Err(EngineError::UnsafePath(repository_root));
        }
        Ok(Self {
            config: config.clone(),
            repository_root,
            runtime,
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
        if request.repository_root != self.repository_root {
            return Err(EngineError::UnsafePath(request.repository_root));
        }
        let worktrees_root = request.state_root.join("worktrees");
        let manager = GitWorktreeManager::open(&self.repository_root, &worktrees_root)?;
        let worktree = manager.create(request.claim.task_id, "HEAD").await?;
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
        let mut lifecycle_error = None;
        let mut summaries = Vec::new();
        loop {
            let raw = tokio::select! {
                () = cancellation.cancelled() => {
                    adapter.cancel(&handle).await.map_err(|error| {
                        invocation_error(request.claim.provider.as_str(), &error.to_string())
                    })?;
                    break;
                }
                raw = adapter.next_event(&handle) => raw.map_err(|error| {
                    invocation_error(request.claim.provider.as_str(), &error.to_string())
                })?,
            };
            let Some(raw) = raw else { break };
            match adapter.parse_event(raw).await {
                Ok(WorkerEvent::Message { text }) => summaries.push(text),
                Ok(WorkerEvent::Completed { summary, .. }) => {
                    completed = true;
                    if let Some(summary) = summary {
                        summaries.push(summary);
                    }
                }
                Ok(WorkerEvent::QuotaExceeded { detail }) => {
                    quota_exhausted = true;
                    if let Some(detail) = detail {
                        lifecycle_error = Some(detail);
                    }
                }
                Ok(WorkerEvent::Error { message, .. }) => lifecycle_error = Some(message),
                Ok(WorkerEvent::Unknown {
                    event_type,
                    affects_lifecycle: true,
                    ..
                }) => {
                    lifecycle_error = Some(format!("unknown lifecycle event: {event_type}"));
                }
                Ok(_) => {}
                Err(error) => lifecycle_error = Some(error.to_string()),
            }
        }
        let output = adapter.wait(&handle).await.map_err(|error| {
            invocation_error(request.claim.provider.as_str(), &error.to_string())
        })?;
        if let Some(error) = output.tree_termination_error.as_ref() {
            lifecycle_error = Some(error.clone());
        }
        let outcome = if cancellation.is_cancelled() {
            WorkerOutcome::Cancelled
        } else if quota_exhausted {
            WorkerOutcome::QuotaExceeded
        } else {
            match output.termination {
                RuntimeTermination::TimedOut => WorkerOutcome::TimedOut,
                RuntimeTermination::Cancelled => WorkerOutcome::Cancelled,
                RuntimeTermination::Exited
                    if output.exit_code == Some(0) && completed && lifecycle_error.is_none() =>
                {
                    WorkerOutcome::Succeeded
                }
                RuntimeTermination::Exited => WorkerOutcome::Failed,
            }
        };
        if let Some(error) = lifecycle_error {
            summaries.push(error);
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
                known_failures: Vec::new(),
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
        })
    }
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
