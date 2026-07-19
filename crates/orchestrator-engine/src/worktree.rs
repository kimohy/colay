use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fmt::Write as _,
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use orchestrator_domain::{RepoPath, TaskId};
use orchestrator_process::{CommandSpec, ProcessResult, ProcessRunner};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{EngineError, EngineResult};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitWorktree {
    pub task_id: TaskId,
    pub repository_root: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base_revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSnapshot {
    pub base_revision: String,
    pub head: String,
    pub status_porcelain: Vec<u8>,
    /// A binary Git diff from `base_revision` to the current working tree followed by
    /// a deterministic evidence section containing every untracked file's bytes.
    pub diff: Vec<u8>,
    pub changed_files: Vec<RepoPath>,
}

impl GitSnapshot {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.changed_files.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeCleanupPlan {
    pub task_id: TaskId,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub requires_user_approval: bool,
}

#[derive(Clone, Debug)]
pub struct GitWorktreeManager {
    repository_root: PathBuf,
    worktrees_root: PathBuf,
    runner: ProcessRunner,
}

impl GitWorktreeManager {
    pub fn open(repository_root: &Path, worktrees_root: &Path) -> EngineResult<Self> {
        let repository_root = canonicalize_directory(repository_root)?;
        ensure_no_symlink_components(worktrees_root)?;
        fs::create_dir_all(worktrees_root)
            .map_err(|error| EngineError::io(worktrees_root, error))?;
        let worktrees_root = canonicalize_directory(worktrees_root)?;
        if worktrees_root == repository_root {
            return Err(EngineError::UnsafePath(worktrees_root));
        }
        Ok(Self {
            repository_root,
            worktrees_root,
            runner: ProcessRunner,
        })
    }

    pub async fn create(&self, task_id: TaskId, base_revision: &str) -> EngineResult<GitWorktree> {
        validate_revision(base_revision)?;
        assert_git_repository(&self.runner, &self.repository_root).await?;
        self.assert_safe_boundary().await?;
        let resolved_base = git_text(
            &self.runner,
            &self.repository_root,
            [
                "rev-parse",
                "--verify",
                &format!("{base_revision}^{{commit}}"),
            ],
        )
        .await?;
        if !(40..=64).contains(&resolved_base.len())
            || !resolved_base.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(EngineError::UnsafeGitBoundary(
                "Git did not resolve the base to a full object ID".to_owned(),
            ));
        }
        let task_component = task_id.to_string();
        let worktree_path = self.worktrees_root.join(&task_component);
        if worktree_path.exists() {
            return Err(EngineError::UnsafePath(worktree_path));
        }
        let branch = format!("orchestrator/task-{task_component}");
        run_git_checked(
            &self.runner,
            &self.repository_root,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-b"),
                OsStr::new(&branch),
                worktree_path.as_os_str(),
                OsStr::new(&resolved_base),
            ],
        )
        .await?;
        let actual_base = git_text(&self.runner, &worktree_path, ["rev-parse", "HEAD"]).await?;
        Ok(GitWorktree {
            task_id,
            repository_root: self.repository_root.clone(),
            path: worktree_path,
            branch,
            base_revision: actual_base,
        })
    }

    pub async fn snapshot(&self, worktree: &GitWorktree) -> EngineResult<GitSnapshot> {
        self.validate_managed_worktree(worktree)?;
        validate_object_id(&worktree.base_revision)?;
        let resolved_base = git_text(
            &self.runner,
            &worktree.path,
            [
                "rev-parse",
                "--verify",
                &format!("{}^{{commit}}", worktree.base_revision),
            ],
        )
        .await?;
        if resolved_base != worktree.base_revision {
            return Err(EngineError::UnsafeGitBoundary(
                "stored worktree base revision no longer resolves to the sealed object ID"
                    .to_owned(),
            ));
        }
        let status = run_git_checked(
            &self.runner,
            &worktree.path,
            [
                OsStr::new("status"),
                OsStr::new("--porcelain=v1"),
                OsStr::new("-z"),
                OsStr::new("--untracked-files=all"),
            ],
        )
        .await?
        .stdout
        .bytes;
        let mut diff = run_git_checked(
            &self.runner,
            &worktree.path,
            [
                OsStr::new("diff"),
                OsStr::new("--binary"),
                OsStr::new("--full-index"),
                OsStr::new("--no-ext-diff"),
                OsStr::new(&worktree.base_revision),
                OsStr::new("--"),
            ],
        )
        .await?
        .stdout
        .bytes;
        let committed_and_tracked_paths = run_git_checked(
            &self.runner,
            &worktree.path,
            [
                OsStr::new("diff"),
                OsStr::new("--name-status"),
                OsStr::new("-z"),
                OsStr::new("--find-renames"),
                OsStr::new(&worktree.base_revision),
                OsStr::new("--"),
            ],
        )
        .await?
        .stdout
        .bytes;
        let head = git_text(&self.runner, &worktree.path, ["rev-parse", "HEAD"]).await?;
        let mut changed_files = parse_porcelain_paths(&status)?;
        changed_files.extend(parse_name_status_paths(&committed_and_tracked_paths)?);
        changed_files.sort();
        changed_files.dedup();
        let untracked_files = parse_untracked_paths(&status)?;
        append_untracked_evidence(&worktree.path, &untracked_files, &mut diff)?;
        Ok(GitSnapshot {
            base_revision: worktree.base_revision.clone(),
            head,
            status_porcelain: status,
            diff,
            changed_files,
        })
    }

    #[must_use]
    pub fn cleanup_plan(&self, worktree: &GitWorktree) -> WorktreeCleanupPlan {
        WorktreeCleanupPlan {
            task_id: worktree.task_id,
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            requires_user_approval: true,
        }
    }

    fn validate_managed_worktree(&self, worktree: &GitWorktree) -> EngineResult<()> {
        if worktree.repository_root != self.repository_root
            || worktree.path.parent() != Some(self.worktrees_root.as_path())
        {
            return Err(EngineError::UnsafePath(worktree.path.clone()));
        }
        let canonical = canonicalize_directory(&worktree.path)?;
        if canonical.parent() != Some(self.worktrees_root.as_path()) {
            return Err(EngineError::UnsafePath(canonical));
        }
        Ok(())
    }

    async fn assert_safe_boundary(&self) -> EngineResult<()> {
        let git_dir = git_text(
            &self.runner,
            &self.repository_root,
            ["rev-parse", "--git-dir"],
        )
        .await?;
        let git_dir = if Path::new(&git_dir).is_absolute() {
            PathBuf::from(git_dir)
        } else {
            self.repository_root.join(git_dir)
        };
        let unsafe_markers = [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_LOG",
            "rebase-apply",
            "rebase-merge",
        ];
        let active = unsafe_markers
            .iter()
            .filter(|marker| git_dir.join(marker).exists())
            .copied()
            .collect::<Vec<_>>();
        if active.is_empty() {
            Ok(())
        } else {
            Err(EngineError::UnsafeGitBoundary(active.join(", ")))
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FileOwnershipRegistry {
    owners: BTreeMap<RepoPath, String>,
}

impl FileOwnershipRegistry {
    pub fn claim(
        &mut self,
        worker_id: impl Into<String>,
        files: impl IntoIterator<Item = RepoPath>,
    ) -> EngineResult<()> {
        let worker_id = worker_id.into();
        let files = files.into_iter().collect::<Vec<_>>();
        if let Some((path, owner)) = files.iter().find_map(|path| {
            self.owners
                .get(path)
                .filter(|owner| *owner != &worker_id)
                .map(|owner| (path, owner))
        }) {
            return Err(EngineError::FileOwnershipConflict {
                path: path.to_string(),
                owner: owner.clone(),
            });
        }
        for path in files {
            self.owners.insert(path, worker_id.clone());
        }
        Ok(())
    }

    pub fn release_worker(&mut self, worker_id: &str) {
        self.owners.retain(|_, owner| owner != worker_id);
    }
}

/// Canonicalizes a directory while removing Windows verbatim prefixes that
/// public Git CLI builds cannot consume as worktree arguments.
pub fn canonicalize_directory(path: &Path) -> EngineResult<PathBuf> {
    let canonical = fs::canonicalize(path).map_err(|error| EngineError::io(path, error))?;
    let canonical = normalize_windows_verbatim_path(canonical);
    if !canonical.is_dir() {
        return Err(EngineError::UnsafePath(canonical));
    }
    Ok(canonical)
}

#[cfg(not(windows))]
fn normalize_windows_verbatim_path(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
fn normalize_windows_verbatim_path(path: PathBuf) -> PathBuf {
    use std::path::Prefix;

    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return path;
    };
    let mut normalized = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:\\", char::from(drive))),
        Prefix::VerbatimUNC(server, share) => PathBuf::from(format!(
            "\\\\{}\\{}",
            server.to_string_lossy(),
            share.to_string_lossy()
        )),
        _ => return path,
    };
    for component in path.components().skip(1) {
        if !matches!(component, Component::RootDir) {
            normalized.push(component.as_os_str());
        }
    }
    normalized
}

fn ensure_no_symlink_components(path: &Path) -> EngineResult<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => return Err(EngineError::UnsafePath(path.to_path_buf())),
            Component::Normal(part) => {
                current.push(part);
                if current.exists()
                    && fs::symlink_metadata(&current)
                        .map_err(|error| EngineError::io(&current, error))?
                        .file_type()
                        .is_symlink()
                {
                    return Err(EngineError::UnsafePath(current));
                }
            }
        }
    }
    Ok(())
}

async fn assert_git_repository(runner: &ProcessRunner, path: &Path) -> EngineResult<()> {
    let value = git_text(runner, path, ["rev-parse", "--is-inside-work-tree"]).await?;
    if value == "true" {
        Ok(())
    } else {
        Err(EngineError::UnsafePath(path.to_path_buf()))
    }
}

fn validate_revision(revision: &str) -> EngineResult<()> {
    if revision.is_empty()
        || revision.starts_with('-')
        || revision.contains(char::is_whitespace)
        || revision.contains('\0')
    {
        Err(EngineError::UnsafeGitBoundary(
            "base revision is not a safe Git argument".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn validate_object_id(object_id: &str) -> EngineResult<()> {
    if (40..=64).contains(&object_id.len())
        && object_id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err(EngineError::UnsafeGitBoundary(
            "stored base revision is not a full Git object ID".to_owned(),
        ))
    }
}

async fn git_text<const N: usize>(
    runner: &ProcessRunner,
    cwd: &Path,
    args: [&str; N],
) -> EngineResult<String> {
    let output = run_git_checked(runner, cwd, args.map(OsStr::new)).await?;
    String::from_utf8(output.stdout.bytes)
        .map(|text| text.trim().to_owned())
        .map_err(|error| EngineError::CommandFailed {
            executable: "git".to_owned(),
            exit_code: Some(0),
            message: format!("non-UTF-8 output: {error}"),
        })
}

async fn run_git_checked<I, S>(
    runner: &ProcessRunner,
    cwd: &Path,
    args: I,
) -> EngineResult<ProcessResult>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut spec = CommandSpec::new("git")
        .args(
            args.into_iter()
                .map(|argument| argument.as_ref().to_os_string()),
        )
        .current_dir(cwd);
    spec.timeout = Duration::from_mins(5);
    spec.stdout_limit = 64 * 1024 * 1024;
    spec.stderr_limit = 2 * 1024 * 1024;
    spec.environment.set("GIT_TERMINAL_PROMPT", "0")?;
    spec.environment.set("GIT_OPTIONAL_LOCKS", "0")?;
    let output = runner.run(spec, CancellationToken::new()).await?;
    if output.stdout.truncated || output.stderr.truncated {
        return Err(EngineError::CommandFailed {
            executable: "git".to_owned(),
            exit_code: output.exit_code,
            message: "Git evidence exceeded the configured output bound".to_owned(),
        });
    }
    if output.success() {
        return Ok(output);
    }
    Err(EngineError::CommandFailed {
        executable: "git".to_owned(),
        exit_code: output.exit_code,
        message: output.stderr.redacted_text.trim().to_owned(),
    })
}

fn parse_porcelain_paths(status: &[u8]) -> EngineResult<Vec<RepoPath>> {
    let mut records = status
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty());
    let mut paths = Vec::new();
    while let Some(record) = records.next() {
        if record.len() < 4 || record[2] != b' ' {
            return Err(EngineError::InvalidRepoPath(
                "malformed Git porcelain record".to_owned(),
            ));
        }
        let status_code = &record[..2];
        let path = std::str::from_utf8(&record[3..])
            .map_err(|_| EngineError::InvalidRepoPath("non-UTF-8 Git path".to_owned()))?;
        push_repo_path(&mut paths, path)?;
        if status_code.contains(&b'R') || status_code.contains(&b'C') {
            let source = records.next().ok_or_else(|| {
                EngineError::InvalidRepoPath("rename record has no source path".to_owned())
            })?;
            let source = std::str::from_utf8(source)
                .map_err(|_| EngineError::InvalidRepoPath("non-UTF-8 Git path".to_owned()))?;
            push_repo_path(&mut paths, source)?;
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn parse_untracked_paths(status: &[u8]) -> EngineResult<Vec<RepoPath>> {
    let mut records = status
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty());
    let mut paths = Vec::new();
    while let Some(record) = records.next() {
        if record.len() < 4 || record[2] != b' ' {
            return Err(EngineError::InvalidRepoPath(
                "malformed Git porcelain record".to_owned(),
            ));
        }
        let status_code = &record[..2];
        let path = std::str::from_utf8(&record[3..])
            .map_err(|_| EngineError::InvalidRepoPath("non-UTF-8 Git path".to_owned()))?;
        if status_code == b"??" {
            push_repo_path(&mut paths, path)?;
        }
        if status_code.contains(&b'R') || status_code.contains(&b'C') {
            records.next().ok_or_else(|| {
                EngineError::InvalidRepoPath("rename record has no source path".to_owned())
            })?;
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn parse_name_status_paths(status: &[u8]) -> EngineResult<Vec<RepoPath>> {
    let mut records = status
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty());
    let mut paths = Vec::new();
    while let Some(status_code) = records.next() {
        let status_code = std::str::from_utf8(status_code).map_err(|_| {
            EngineError::InvalidRepoPath("non-UTF-8 Git name-status code".to_owned())
        })?;
        let path = records.next().ok_or_else(|| {
            EngineError::InvalidRepoPath("Git name-status record has no path".to_owned())
        })?;
        let path = std::str::from_utf8(path)
            .map_err(|_| EngineError::InvalidRepoPath("non-UTF-8 Git path".to_owned()))?;
        push_repo_path(&mut paths, path)?;
        if status_code.starts_with('R') || status_code.starts_with('C') {
            let destination = records.next().ok_or_else(|| {
                EngineError::InvalidRepoPath(
                    "Git rename/copy record has no destination path".to_owned(),
                )
            })?;
            let destination = std::str::from_utf8(destination)
                .map_err(|_| EngineError::InvalidRepoPath("non-UTF-8 Git path".to_owned()))?;
            push_repo_path(&mut paths, destination)?;
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

const MAX_UNTRACKED_EVIDENCE_BYTES: usize = 64 * 1024 * 1024;

fn append_untracked_evidence(
    worktree_root: &Path,
    paths: &[RepoPath],
    diff: &mut Vec<u8>,
) -> EngineResult<()> {
    if paths.is_empty() {
        return Ok(());
    }
    diff.extend_from_slice(b"\n# orchestrator-untracked-evidence-v1\n");
    for relative in paths {
        let path = relative.join_to(worktree_root);
        ensure_no_symlink_components(path.parent().unwrap_or(worktree_root))?;
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| EngineError::io(&path, error))?;
        let (kind, contents) = if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path).map_err(|error| EngineError::io(&path, error))?;
            let target = target.to_str().ok_or_else(|| {
                EngineError::InvalidRepoPath(format!(
                    "untracked symlink target is not UTF-8: {relative}"
                ))
            })?;
            ("symlink", target.as_bytes().to_vec())
        } else if metadata.is_file() {
            let length =
                usize::try_from(metadata.len()).map_err(|_| EngineError::CommandFailed {
                    executable: "git snapshot".to_owned(),
                    exit_code: None,
                    message: "untracked file length cannot be represented safely".to_owned(),
                })?;
            ensure_evidence_capacity(diff.len(), length, relative)?;
            let contents = fs::read(&path).map_err(|error| EngineError::io(&path, error))?;
            let after =
                fs::symlink_metadata(&path).map_err(|error| EngineError::io(&path, error))?;
            if !after.is_file() || after.len() != metadata.len() || contents.len() != length {
                return Err(EngineError::UnsafeGitBoundary(format!(
                    "untracked file changed while checkpointing: {relative}"
                )));
            }
            ("regular", contents)
        } else {
            return Err(EngineError::UnsafePath(path));
        };
        ensure_evidence_capacity(diff.len(), contents.len(), relative)?;
        let digest = format!("{:x}", Sha256::digest(&contents));
        let path_json = serde_json::to_string(&relative.to_string())?;
        let mut header = String::new();
        writeln!(&mut header, "path {path_json}")
            .map_err(|_| EngineError::InvalidRepoPath(relative.to_string()))?;
        writeln!(&mut header, "kind {kind}")
            .map_err(|_| EngineError::InvalidRepoPath(relative.to_string()))?;
        writeln!(&mut header, "length {}", contents.len())
            .map_err(|_| EngineError::InvalidRepoPath(relative.to_string()))?;
        writeln!(&mut header, "sha256 {digest}")
            .map_err(|_| EngineError::InvalidRepoPath(relative.to_string()))?;
        header.push_str("content-hex ");
        diff.extend_from_slice(header.as_bytes());
        header.clear();
        for byte in contents {
            write!(header, "{byte:02x}")
                .map_err(|_| EngineError::InvalidRepoPath(relative.to_string()))?;
            if header.len() >= 8192 {
                diff.extend_from_slice(header.as_bytes());
                header.clear();
            }
        }
        diff.extend_from_slice(header.as_bytes());
        diff.push(b'\n');
    }
    Ok(())
}

fn ensure_evidence_capacity(
    current_length: usize,
    content_length: usize,
    relative: &RepoPath,
) -> EngineResult<()> {
    let projected = current_length
        .saturating_add(content_length.saturating_mul(2))
        .saturating_add(relative.to_string().len())
        .saturating_add(256);
    if projected > MAX_UNTRACKED_EVIDENCE_BYTES {
        Err(EngineError::CommandFailed {
            executable: "git snapshot".to_owned(),
            exit_code: None,
            message: "untracked content exceeds the complete-evidence bound".to_owned(),
        })
    } else {
        Ok(())
    }
}

fn push_repo_path(paths: &mut Vec<RepoPath>, path: &str) -> EngineResult<()> {
    let normalized = path.replace('\\', "/");
    let path = RepoPath::try_from(normalized)
        .map_err(|error| EngineError::InvalidRepoPath(error.to_string()))?;
    paths.push(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command};

    use orchestrator_domain::{RepoPath, TaskId};

    use super::{
        FileOwnershipRegistry, GitWorktreeManager, append_untracked_evidence,
        parse_name_status_paths, parse_porcelain_paths,
    };

    fn run_git(cwd: &Path, args: &[&str]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new("git").args(args).current_dir(cwd).output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            )
            .into())
        }
    }

    #[test]
    fn parses_nul_delimited_porcelain_and_renames() -> Result<(), Box<dyn std::error::Error>> {
        let paths = parse_porcelain_paths(b" M src/lib.rs\0R  src/new.rs\0src/old.rs\0")?;
        assert_eq!(paths.len(), 3);
        assert!(paths.contains(&RepoPath::try_from("src/new.rs")?));
        Ok(())
    }

    #[test]
    fn parses_base_diff_name_status_and_rename_paths() -> Result<(), Box<dyn std::error::Error>> {
        let paths = parse_name_status_paths(b"M\0src/lib.rs\0R100\0old.rs\0new.rs\0")?;
        assert_eq!(paths.len(), 3);
        assert!(paths.contains(&RepoPath::try_from("src/lib.rs")?));
        assert!(paths.contains(&RepoPath::try_from("old.rs")?));
        assert!(paths.contains(&RepoPath::try_from("new.rs")?));
        Ok(())
    }

    #[test]
    fn writable_file_has_one_owner() -> Result<(), Box<dyn std::error::Error>> {
        let file = RepoPath::try_from("src/lib.rs")?;
        let mut owners = FileOwnershipRegistry::default();
        owners.claim("worker-a", [file.clone()])?;
        assert!(owners.claim("worker-b", [file.clone()]).is_err());
        owners.release_worker("worker-a");
        owners.claim("worker-b", [file])?;
        Ok(())
    }

    #[test]
    fn oversized_untracked_file_is_rejected_before_content_capture()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = crate::test_support::CanonicalTempDir::new()?;
        let path = directory.path().join("large.bin");
        let file = std::fs::File::create(&path)?;
        file.set_len(40 * 1024 * 1024)?;
        let mut diff = Vec::new();

        let result = append_untracked_evidence(
            directory.path(),
            &[RepoPath::try_from("large.bin")?],
            &mut diff,
        );

        assert!(result.is_err());
        assert!(diff.len() < 1024);
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_includes_worker_commits_and_untracked_contents()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let directory = crate::test_support::CanonicalTempDir::new()?;
        let repository = directory.path().join("repository");
        let worktrees = directory.path().join("worktrees");
        std::fs::create_dir(&repository)?;
        run_git(&repository, &["init"])?;
        run_git(&repository, &["config", "user.name", "Orchestrator Test"])?;
        run_git(
            &repository,
            &["config", "user.email", "orchestrator@example.invalid"],
        )?;
        std::fs::write(repository.join("tracked.txt"), b"base\n")?;
        run_git(&repository, &["add", "tracked.txt"])?;
        run_git(&repository, &["commit", "-m", "base"])?;

        let manager = GitWorktreeManager::open(&repository, &worktrees)?;
        let worktree = manager.create(TaskId::new(), "HEAD").await?;
        std::fs::write(worktree.path.join("tracked.txt"), b"committed change\n")?;
        run_git(&worktree.path, &["add", "tracked.txt"])?;
        run_git(&worktree.path, &["commit", "-m", "worker commit"])?;
        std::fs::write(worktree.path.join("untracked.bin"), b"untracked\0content")?;

        let snapshot = manager.snapshot(&worktree).await?;
        assert_eq!(snapshot.base_revision, worktree.base_revision);
        assert_ne!(snapshot.head, snapshot.base_revision);
        assert!(
            snapshot
                .changed_files
                .contains(&RepoPath::try_from("tracked.txt")?)
        );
        assert!(
            snapshot
                .changed_files
                .contains(&RepoPath::try_from("untracked.bin")?)
        );
        let evidence = String::from_utf8_lossy(&snapshot.diff);
        assert!(evidence.contains("committed change"));
        assert!(evidence.contains("# orchestrator-untracked-evidence-v1"));
        assert!(evidence.contains("path \"untracked.bin\""));
        assert!(evidence.contains("756e747261636b656400636f6e74656e74"));
        Ok(())
    }
}
