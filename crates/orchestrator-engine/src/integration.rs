use std::{
    collections::BTreeSet,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    Checkpoint, GraphRevisionId, IntegrationApplication, IntegrationApplicationId,
    IntegrationApproval, IntegrationBatchId, IntegrationBlocker, IntegrationPreview,
    IntegrationSource, SessionId, TaskId, VerificationResult,
};
use orchestrator_process::{CommandSpec, ProcessResult, ProcessRunner};
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{EngineError, EngineResult, GitWorktree, GitWorktreeManager, canonicalize_directory};

#[derive(Clone, Debug)]
pub struct IntegrationCandidate {
    pub task_id: TaskId,
    pub graph_order: u64,
    pub dependencies: Vec<TaskId>,
    pub worktree: Option<GitWorktree>,
    pub checkpoint: Option<Checkpoint>,
    pub verification: Option<VerificationResult>,
}

#[derive(Clone, Debug)]
pub struct IntegrationPreviewRequest {
    pub batch_id: IntegrationBatchId,
    pub session_id: SessionId,
    pub graph_revision_id: GraphRevisionId,
    pub repository_root: PathBuf,
    pub state_root: PathBuf,
    pub base_revision: String,
    pub candidates: Vec<IntegrationCandidate>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationWorktree {
    pub path: PathBuf,
    pub branch: String,
    pub base_revision: String,
}

pub struct GitIntegrationManager {
    repository_root: PathBuf,
    integration_root: PathBuf,
    runner: ProcessRunner,
}

impl GitIntegrationManager {
    pub fn new(repository_root: &Path, state_root: &Path) -> EngineResult<Self> {
        let repository_root = canonicalize_directory(repository_root)?;
        let integration_root = state_root.join("integration");
        std::fs::create_dir_all(&integration_root)
            .map_err(|error| EngineError::io(&integration_root, error))?;
        let integration_root = canonicalize_directory(&integration_root)?;
        Ok(Self {
            repository_root,
            integration_root,
            runner: ProcessRunner,
        })
    }

    pub async fn preview(
        &self,
        request: &IntegrationPreviewRequest,
    ) -> EngineResult<IntegrationPreview> {
        if canonicalize_directory(&request.repository_root)? != self.repository_root {
            return Err(EngineError::UnsafePath(request.repository_root.clone()));
        }
        let worktrees_root = request.state_root.join("worktrees");
        let worktrees = GitWorktreeManager::open(&self.repository_root, &worktrees_root)?;
        let mut blockers = Vec::new();
        let mut sources = Vec::new();
        for candidate in dependency_order(&request.candidates)? {
            let (Some(worktree), Some(checkpoint), Some(verification)) = (
                candidate.worktree.as_ref(),
                candidate.checkpoint.as_ref(),
                candidate.verification.as_ref(),
            ) else {
                blockers.push(IntegrationBlocker::MissingEvidence {
                    task_id: candidate.task_id,
                    detail: "worktree, checkpoint, or verification is missing".to_owned(),
                });
                continue;
            };
            if worktree.task_id != candidate.task_id
                || checkpoint.task_id != candidate.task_id
                || verification.task_id != candidate.task_id
                || !checkpoint.verify_integrity().unwrap_or(false)
            {
                blockers.push(IntegrationBlocker::SourceChanged {
                    task_id: candidate.task_id,
                });
                continue;
            }
            if !verification.passes_completion_gate(false) {
                blockers.push(IntegrationBlocker::VerificationFailed {
                    task_id: candidate.task_id,
                });
                continue;
            }
            let snapshot = worktrees.snapshot(worktree).await?;
            if snapshot.base_revision != request.base_revision
                || checkpoint.git_base.as_deref() != Some(request.base_revision.as_str())
            {
                blockers.push(IntegrationBlocker::StaleBase {
                    task_id: candidate.task_id,
                    found: snapshot.base_revision,
                });
                continue;
            }
            if snapshot.changed_files != checkpoint.files_changed
                || snapshot
                    .diff
                    .windows(b"# orchestrator-untracked-evidence-v1".len())
                    .any(|part| part == b"# orchestrator-untracked-evidence-v1")
            {
                blockers.push(IntegrationBlocker::SourceChanged {
                    task_id: candidate.task_id,
                });
                continue;
            }
            sources.push(IntegrationSource {
                task_id: candidate.task_id,
                checkpoint_id: checkpoint.checkpoint_id,
                verification_id: verification.verification_id,
                base_revision: snapshot.base_revision,
                diff_sha256: hex::encode(Sha256::digest(&snapshot.diff)),
                changed_files: snapshot.changed_files,
            });
        }
        IntegrationPreview::seal(
            request.batch_id,
            request.session_id,
            request.graph_revision_id,
            request.base_revision.clone(),
            sources,
            blockers,
            request.created_at,
        )
        .map_err(|_| EngineError::IntegrityMismatch {
            artifact: "integration preview seal",
        })
    }

    pub async fn repository_head(&self) -> EngineResult<String> {
        git_text(&self.runner, &self.repository_root, &["rev-parse", "HEAD"]).await
    }

    pub async fn apply(
        &self,
        request: &IntegrationPreviewRequest,
        approved_preview: &IntegrationPreview,
        approval: &IntegrationApproval,
        application_id: IntegrationApplicationId,
    ) -> EngineResult<(IntegrationWorktree, IntegrationApplication)> {
        approval
            .validate_for(approved_preview)
            .map_err(|_| EngineError::IntegrityMismatch {
                artifact: "integration approval",
            })?;
        let fresh = self.preview(request).await?;
        if fresh.preview_hash != approved_preview.preview_hash || !fresh.is_approvable() {
            return Err(EngineError::IntegrityMismatch {
                artifact: "integration preview",
            });
        }
        let head = git_text(&self.runner, &self.repository_root, &["rev-parse", "HEAD"]).await?;
        if head != fresh.base_revision {
            return Err(EngineError::UnsafeGitBoundary(
                "repository HEAD changed after integration preview".to_owned(),
            ));
        }
        let worktree = self
            .create_worktree(fresh.batch_id, &fresh.base_revision)
            .await?;
        let worktrees =
            GitWorktreeManager::open(&self.repository_root, &request.state_root.join("worktrees"))?;
        let ordered = dependency_order(&request.candidates)?;
        if ordered.len() != fresh.sources.len() {
            return Err(EngineError::IntegrityMismatch {
                artifact: "integration source count",
            });
        }
        for (candidate, source) in ordered.into_iter().zip(&fresh.sources) {
            if candidate.task_id != source.task_id {
                return Err(EngineError::IntegrityMismatch {
                    artifact: "integration source order",
                });
            }
            let source_worktree =
                candidate
                    .worktree
                    .as_ref()
                    .ok_or(EngineError::IntegrityMismatch {
                        artifact: "integration source worktree",
                    })?;
            let snapshot = worktrees.snapshot(source_worktree).await?;
            if hex::encode(Sha256::digest(&snapshot.diff)) != source.diff_sha256 {
                return Err(EngineError::IntegrityMismatch {
                    artifact: "integration source diff",
                });
            }
            run_git(
                &self.runner,
                &worktree.path,
                &["apply", "--index", "--binary", "-"],
                snapshot.diff,
            )
            .await?;
        }
        let tree = git_text(&self.runner, &worktree.path, &["write-tree"]).await?;
        let application = IntegrationApplication {
            application_id,
            batch_id: fresh.batch_id,
            preview_hash: fresh.preview_hash,
            integration_worktree: worktree.path.to_string_lossy().into_owned(),
            integration_branch: worktree.branch.clone(),
            resulting_tree: Some(tree),
            succeeded: true,
            detail_redacted: "approved sources applied and indexed".to_owned(),
            completed_at: Utc::now(),
        };
        Ok((worktree, application))
    }

    async fn create_worktree(
        &self,
        batch_id: IntegrationBatchId,
        base_revision: &str,
    ) -> EngineResult<IntegrationWorktree> {
        let path = self.integration_root.join(batch_id.to_string());
        if path.exists() {
            return Err(EngineError::UnsafePath(path));
        }
        let branch = format!("orchestrator/integration-{batch_id}");
        let args = vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("-b"),
            OsString::from(&branch),
            path.as_os_str().to_os_string(),
            OsString::from(base_revision),
        ];
        run_git_os(&self.runner, &self.repository_root, args, Vec::new()).await?;
        Ok(IntegrationWorktree {
            path,
            branch,
            base_revision: base_revision.to_owned(),
        })
    }

    #[must_use]
    pub fn worktree_identity(
        &self,
        batch_id: IntegrationBatchId,
        base_revision: &str,
    ) -> IntegrationWorktree {
        IntegrationWorktree {
            path: self.integration_root.join(batch_id.to_string()),
            branch: format!("orchestrator/integration-{batch_id}"),
            base_revision: base_revision.to_owned(),
        }
    }
}

fn dependency_order(
    candidates: &[IntegrationCandidate],
) -> EngineResult<Vec<&IntegrationCandidate>> {
    let ids = candidates
        .iter()
        .map(|candidate| candidate.task_id)
        .collect::<BTreeSet<_>>();
    if candidates
        .iter()
        .flat_map(|candidate| &candidate.dependencies)
        .any(|id| !ids.contains(id))
    {
        return Err(EngineError::IntegrityMismatch {
            artifact: "integration dependencies",
        });
    }
    let mut remaining = candidates.iter().collect::<Vec<_>>();
    remaining.sort_by_key(|candidate| (candidate.graph_order, candidate.task_id));
    let mut emitted = BTreeSet::new();
    let mut ordered = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let Some(index) = remaining.iter().position(|candidate| {
            candidate
                .dependencies
                .iter()
                .all(|dependency| emitted.contains(dependency))
        }) else {
            return Err(EngineError::IntegrityMismatch {
                artifact: "integration dependency cycle",
            });
        };
        let candidate = remaining.remove(index);
        emitted.insert(candidate.task_id);
        ordered.push(candidate);
    }
    Ok(ordered)
}

async fn git_text(runner: &ProcessRunner, cwd: &Path, args: &[&str]) -> EngineResult<String> {
    let output = run_git(runner, cwd, args, Vec::new()).await?;
    String::from_utf8(output.stdout.bytes)
        .map(|value| value.trim().to_owned())
        .map_err(|error| EngineError::CommandFailed {
            executable: "git".to_owned(),
            exit_code: Some(0),
            message: error.to_string(),
        })
}

async fn run_git(
    runner: &ProcessRunner,
    cwd: &Path,
    args: &[&str],
    stdin: Vec<u8>,
) -> EngineResult<ProcessResult> {
    run_git_os(
        runner,
        cwd,
        args.iter().map(OsString::from).collect(),
        stdin,
    )
    .await
}

async fn run_git_os(
    runner: &ProcessRunner,
    cwd: &Path,
    args: Vec<OsString>,
    stdin: Vec<u8>,
) -> EngineResult<ProcessResult> {
    let mut spec = CommandSpec::new("git")
        .args(args)
        .current_dir(cwd)
        .with_stdin(stdin);
    spec.timeout = Duration::from_mins(5);
    spec.stdout_limit = 64 * 1024 * 1024;
    spec.stderr_limit = 2 * 1024 * 1024;
    spec.environment.set("GIT_TERMINAL_PROMPT", "0")?;
    let output = runner.run(spec, CancellationToken::new()).await?;
    if output.success() && !output.stdout.truncated && !output.stderr.truncated {
        Ok(output)
    } else {
        Err(EngineError::CommandFailed {
            executable: "git".to_owned(),
            exit_code: output.exit_code,
            message: output.stderr.redacted_text.trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use chrono::Utc;
    use orchestrator_domain::{
        AcceptanceEvidence, AttemptId, GraphRevisionId, IntegrationApplicationId,
        IntegrationApproval, IntegrationBatchId, ProviderId, SchemaVersion, SessionId, TaskId,
        VerificationId, VerificationResult, VerificationStatus,
    };
    use orchestrator_state::ArtifactStore;

    use super::{GitIntegrationManager, IntegrationCandidate, IntegrationPreviewRequest};
    use crate::{
        CheckpointInput, CheckpointManager, GitCheckpointEvidence, GitWorktreeManager,
        canonicalize_directory,
    };

    fn git(repository: &std::path::Path, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
        let output = Command::new("git")
            .current_dir(repository)
            .args(args)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned().into())
        }
    }

    fn verification(task_id: TaskId) -> VerificationResult {
        VerificationResult {
            schema_version: SchemaVersion::v1(),
            verification_id: VerificationId::new(),
            task_id,
            implementation_provider: ProviderId::Codex,
            reviewer_provider: None,
            status: VerificationStatus::Pass,
            checks: Vec::new(),
            acceptance_criteria: vec![AcceptanceEvidence {
                criterion: "integrate".to_owned(),
                status: VerificationStatus::Pass,
                evidence: vec!["verified fixture".to_owned()],
            }],
            changed_files: Vec::new(),
            out_of_scope_files: Vec::new(),
            unresolved_todos: Vec::new(),
            requires_approval: false,
            verified_at: Utc::now(),
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn preview_is_read_only_and_exact_approval_applies_only_to_integration_worktree()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let repository = directory.path().join("repository");
        fs::create_dir_all(&repository)?;
        fs::write(repository.join("a.txt"), "base a\n")?;
        fs::write(repository.join("b.txt"), "base b\n")?;
        fs::write(repository.join(".gitignore"), ".colay/\n")?;
        git(&repository, &["init"])?;
        git(&repository, &["config", "user.name", "Integration Test"])?;
        git(
            &repository,
            &["config", "user.email", "integration@example.invalid"],
        )?;
        git(&repository, &["add", "."])?;
        git(&repository, &["commit", "-m", "base"])?;
        let repository = canonicalize_directory(&repository)?;
        let state_root = repository.join(".colay");
        let task_root = state_root.join("worktrees");
        let worktrees = GitWorktreeManager::open(&repository, &task_root)?;
        let first = worktrees.create(TaskId::new(), "HEAD").await?;
        let second = worktrees.create(TaskId::new(), "HEAD").await?;
        fs::write(first.path.join("a.txt"), "integrated a\n")?;
        fs::write(second.path.join("b.txt"), "integrated b\n")?;
        let artifacts = ArtifactStore::open(&state_root)?;
        let checkpoint_manager = CheckpointManager::new(artifacts);
        let mut candidates = Vec::new();
        for (order, worktree) in [first.clone(), second.clone()].into_iter().enumerate() {
            let task_id = worktree.task_id;
            let snapshot = worktrees.snapshot(&worktree).await?;
            let checkpoint = checkpoint_manager.create(
                CheckpointInput {
                    task_id: worktree.task_id,
                    attempt_id: AttemptId::new(),
                    objective: "integration fixture".to_owned(),
                    current_plan: Vec::new(),
                    completed_steps: Vec::new(),
                    pending_steps: Vec::new(),
                    files_read: Vec::new(),
                    commands_run: Vec::new(),
                    tests: Vec::new(),
                    decisions: Vec::new(),
                    unresolved_questions: Vec::new(),
                    known_failures: Vec::new(),
                    worker_claim: None,
                    current_worker: ProviderId::Codex,
                    concise_context_summary: "fixture".to_owned(),
                    created_at: Utc::now(),
                },
                GitCheckpointEvidence::from(&snapshot),
            )?;
            candidates.push(IntegrationCandidate {
                task_id,
                graph_order: u64::try_from(order + 1)?,
                dependencies: Vec::new(),
                worktree: Some(worktree),
                checkpoint: Some(checkpoint),
                verification: Some(verification(task_id)),
            });
        }
        let batch_id = IntegrationBatchId::new();
        let request = IntegrationPreviewRequest {
            batch_id,
            session_id: SessionId::new(),
            graph_revision_id: GraphRevisionId::new(),
            repository_root: repository.clone(),
            state_root: state_root.clone(),
            base_revision: first.base_revision.clone(),
            candidates,
            created_at: Utc::now(),
        };
        let manager = GitIntegrationManager::new(&repository, &state_root)?;
        let identity = manager.worktree_identity(batch_id, &request.base_revision);
        let preview = manager.preview(&request).await?;
        assert!(preview.is_approvable());
        assert!(
            !identity.path.exists(),
            "preview must not create a worktree"
        );
        let approval = IntegrationApproval {
            batch_id,
            preview_hash: preview.preview_hash.clone(),
            approved_by: "operator".to_owned(),
            approved_at: Utc::now(),
        };
        let (integration, application) = manager
            .apply(
                &request,
                &preview,
                &approval,
                IntegrationApplicationId::new(),
            )
            .await?;
        assert!(application.succeeded);
        assert_eq!(
            fs::read_to_string(integration.path.join("a.txt"))?.trim(),
            "integrated a"
        );
        assert_eq!(
            fs::read_to_string(integration.path.join("b.txt"))?.trim(),
            "integrated b"
        );
        assert_eq!(fs::read_to_string(repository.join("a.txt"))?, "base a\n");
        assert_eq!(fs::read_to_string(repository.join("b.txt"))?, "base b\n");
        assert!(first.path.exists() && second.path.exists());
        Ok(())
    }
}
