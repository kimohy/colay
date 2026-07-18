use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use orchestrator_domain::RepoPath;
use thiserror::Error;

use crate::CommandSpec;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum GitSafetyError {
    #[error("repository root is not a directory: {0}")]
    InvalidRoot(PathBuf),
    #[error("unsafe Git revision `{0}`; a full hexadecimal object ID is required")]
    UnsafeRevision(String),
    #[error("unsafe generated branch name `{0}`")]
    UnsafeBranch(String),
    #[error("repository path traverses a symbolic link: {0}")]
    SymlinkEscape(PathBuf),
    #[error("repository path escapes root: {0}")]
    PathEscape(PathBuf),
    #[error("failed to inspect {path}: {message}")]
    Io { path: PathBuf, message: String },
}

#[derive(Clone, Debug)]
pub struct GitCommandBuilder {
    executable: PathBuf,
    repo_root: PathBuf,
    timeout: Duration,
}

impl GitCommandBuilder {
    pub fn new(
        executable: impl Into<PathBuf>,
        repo_root: impl Into<PathBuf>,
    ) -> Result<Self, GitSafetyError> {
        let requested = repo_root.into();
        let repo_root = fs::canonicalize(&requested).map_err(|error| GitSafetyError::Io {
            path: requested.clone(),
            message: error.to_string(),
        })?;
        if !repo_root.is_dir() {
            return Err(GitSafetyError::InvalidRoot(repo_root));
        }
        Ok(Self {
            executable: executable.into(),
            repo_root,
            timeout: Duration::from_mins(5),
        })
    }

    #[must_use]
    pub fn status_porcelain_v2(&self) -> CommandSpec {
        self.base()
            .args(["status", "--porcelain=v2", "-z", "--untracked-files=all"])
    }

    pub fn diff_binary(&self, base_object_id: &str) -> Result<CommandSpec, GitSafetyError> {
        validate_object_id(base_object_id)?;
        Ok(self
            .base()
            .args(["diff", "--binary", "--no-ext-diff", base_object_id, "--"]))
    }

    pub fn diff_name_only(&self, base_object_id: &str) -> Result<CommandSpec, GitSafetyError> {
        validate_object_id(base_object_id)?;
        Ok(self.base().args([
            "diff",
            "--name-only",
            "-z",
            "--no-ext-diff",
            base_object_id,
            "--",
        ]))
    }

    pub fn worktree_add(
        &self,
        worktree_path: &Path,
        branch_name: &str,
        base_object_id: &str,
    ) -> Result<CommandSpec, GitSafetyError> {
        validate_branch(branch_name)?;
        validate_object_id(base_object_id)?;
        let mut command = self.base().args(["worktree", "add", "-b"]);
        command.args.push(branch_name.into());
        command.args.push(worktree_path.as_os_str().to_owned());
        command.args.push(base_object_id.into());
        Ok(command)
    }

    fn base(&self) -> CommandSpec {
        let mut command = CommandSpec::new(&self.executable)
            .args(["-C"])
            .arg(self.repo_root.as_os_str().to_owned());
        command.timeout = self.timeout;
        command
    }
}

/// Resolves a repository-relative path and rejects any existing symbolic-link component.
pub fn resolve_repo_path(root: &Path, path: &RepoPath) -> Result<PathBuf, GitSafetyError> {
    let root = fs::canonicalize(root).map_err(|error| GitSafetyError::Io {
        path: root.to_path_buf(),
        message: error.to_string(),
    })?;
    let mut current = root.clone();
    for component in path.as_path().components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(GitSafetyError::SymlinkEscape(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(GitSafetyError::Io {
                    path: current,
                    message: error.to_string(),
                });
            }
        }
    }
    if !current.starts_with(&root) {
        return Err(GitSafetyError::PathEscape(current));
    }
    Ok(current)
}

fn validate_object_id(value: &str) -> Result<(), GitSafetyError> {
    if !(40..=64).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitSafetyError::UnsafeRevision(value.to_owned()));
    }
    Ok(())
}

fn validate_branch(value: &str) -> Result<(), GitSafetyError> {
    let safe = value.starts_with("orchestrator/")
        && !value.contains("..")
        && !value.ends_with(['.', '/'])
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'/' | b'.'));
    if !safe {
        return Err(GitSafetyError::UnsafeBranch(value.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use orchestrator_domain::RepoPath;

    use super::{GitCommandBuilder, resolve_repo_path};

    #[test]
    fn revisions_must_be_full_hex_object_ids() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let git = GitCommandBuilder::new("git", directory.path())
            .unwrap_or_else(|error| panic!("builder: {error}"));
        assert!(git.diff_binary("--output=/tmp/escape").is_err());
        assert!(git.diff_binary(&"a".repeat(40)).is_ok());
    }

    #[test]
    fn resolves_normal_repo_paths() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let path =
            RepoPath::try_from("src/lib.rs").unwrap_or_else(|error| panic!("repo path: {error}"));
        let resolved = resolve_repo_path(directory.path(), &path)
            .unwrap_or_else(|error| panic!("resolve: {error}"));
        let canonical = std::fs::canonicalize(directory.path())
            .unwrap_or_else(|error| panic!("canonical tempdir: {error}"));
        assert!(resolved.starts_with(canonical));
    }
}
