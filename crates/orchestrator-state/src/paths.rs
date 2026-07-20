use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use crate::{RootConfig, StateError, StateResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryStatePaths {
    pub root: PathBuf,
    pub database: PathBuf,
    pub events: PathBuf,
    pub backups: PathBuf,
    pub tasks: PathBuf,
    pub checkpoints: PathBuf,
    pub handovers: PathBuf,
    pub worktrees: PathBuf,
}

impl RepositoryStatePaths {
    /// Resolves configured state paths beneath a trusted canonical repository.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] when the repository cannot be canonicalized or the configured
    /// state directory escapes through lexical traversal or an existing symbolic-link ancestor.
    pub fn from_config(repository: &Path, config: &RootConfig) -> StateResult<Self> {
        let repository =
            fs::canonicalize(repository).map_err(|error| StateError::io(repository, error))?;
        let root = confined_local_path(&repository, &config.orchestrator.state_dir)?;
        Ok(Self {
            database: root.join("orchestrator.db"),
            events: root.join("events.jsonl"),
            backups: root.join("backups"),
            tasks: root.join("tasks"),
            checkpoints: root.join("checkpoints"),
            handovers: root.join("handovers"),
            worktrees: root.join("worktrees"),
            root,
        })
    }
}

fn confined_local_path(repository: &Path, configured: &Path) -> StateResult<PathBuf> {
    let candidate = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        repository.join(configured)
    };
    let normalized = normalize_lexically(&candidate)?;
    if !normalized.starts_with(repository) {
        return Err(StateError::InvalidConfig(format!(
            "configured path escapes the repository: {}",
            configured.display()
        )));
    }

    let mut ancestor = normalized.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| {
            StateError::InvalidConfig(format!(
                "configured path has no existing ancestor: {}",
                normalized.display()
            ))
        })?;
    }
    let canonical_ancestor =
        fs::canonicalize(ancestor).map_err(|error| StateError::io(ancestor, error))?;
    if !canonical_ancestor.starts_with(repository) {
        return Err(StateError::InvalidConfig(
            "configured path traverses a symlink outside the repository".to_owned(),
        ));
    }
    Ok(normalized)
}

fn normalize_lexically(path: &Path) -> StateResult<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(StateError::InvalidConfig(format!(
                        "path contains an invalid parent traversal: {}",
                        path.display()
                    )));
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::RepositoryStatePaths;
    use crate::RootConfig;

    #[test]
    fn default_paths_are_confined_to_repository_colay_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let repository = temporary.path().join("repository");
        fs::create_dir_all(&repository)?;
        let repository = fs::canonicalize(repository)?;

        let paths = RepositoryStatePaths::from_config(&repository, &RootConfig::default())?;

        let root = repository.join(".colay");
        assert_eq!(paths.root, root);
        assert_eq!(paths.database, root.join("orchestrator.db"));
        assert_eq!(paths.events, root.join("events.jsonl"));
        assert_eq!(paths.backups, root.join("backups"));
        assert_eq!(paths.tasks, root.join("tasks"));
        assert_eq!(paths.checkpoints, root.join("checkpoints"));
        assert_eq!(paths.handovers, root.join("handovers"));
        assert_eq!(paths.worktrees, root.join("worktrees"));
        Ok(())
    }

    #[test]
    fn relative_parent_escape_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let repository = temporary.path().join("repository");
        fs::create_dir_all(&repository)?;
        let repository = fs::canonicalize(repository)?;
        let mut config = RootConfig::default();
        config.orchestrator.state_dir = "../outside".into();

        let error = RepositoryStatePaths::from_config(&repository, &config)
            .expect_err("relative escape must fail");

        assert!(error.to_string().contains("escapes the repository"));
        Ok(())
    }

    #[test]
    fn absolute_path_outside_repository_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let repository = temporary.path().join("repository");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&repository)?;
        fs::create_dir_all(&outside)?;
        let repository = fs::canonicalize(repository)?;
        let outside = fs::canonicalize(outside)?;
        let mut config = RootConfig::default();
        config.orchestrator.state_dir = outside;

        let error = RepositoryStatePaths::from_config(&repository, &config)
            .expect_err("absolute escape must fail");

        assert!(error.to_string().contains("escapes the repository"));
        Ok(())
    }
}
