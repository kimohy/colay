use std::{env, path::PathBuf};

use crate::{StateError, StateResult, WorkspaceId};

#[derive(Clone, Debug, Default)]
pub struct StateEnvironment {
    colay_home: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalStatePaths {
    pub root: PathBuf,
    pub database: PathBuf,
    pub backups: PathBuf,
    pub workspaces: PathBuf,
    pub runtime: PathBuf,
    pub config: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceStatePaths {
    pub root: PathBuf,
    pub artifacts: PathBuf,
    pub checkpoints: PathBuf,
    pub handovers: PathBuf,
    pub backups: PathBuf,
    pub worktrees: PathBuf,
}

impl StateEnvironment {
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            colay_home: env::var_os("COLAY_HOME").map(PathBuf::from),
        }
    }

    #[must_use]
    pub fn with_colay_home(colay_home: PathBuf) -> Self {
        Self {
            colay_home: Some(colay_home),
        }
    }
}

impl GlobalStatePaths {
    pub fn resolve(environment: &StateEnvironment) -> StateResult<Self> {
        if let Some(root) = &environment.colay_home {
            return Ok(Self::under_colay_home(root.clone()));
        }
        Self::native()
    }

    #[must_use]
    pub fn for_workspace(&self, workspace_id: WorkspaceId) -> WorkspaceStatePaths {
        let root = self.workspaces.join(workspace_id.to_string());
        WorkspaceStatePaths {
            artifacts: root.join("artifacts"),
            checkpoints: root.join("checkpoints"),
            handovers: root.join("handovers"),
            backups: root.join("backups"),
            worktrees: root.join("worktrees"),
            root,
        }
    }

    fn under_colay_home(root: PathBuf) -> Self {
        let state = root.join("state");
        Self {
            database: state.join("state.db"),
            backups: state.join("backups"),
            workspaces: root.join("data/workspaces"),
            runtime: root.join("runtime"),
            config: root.join("config.toml"),
            root,
        }
    }

    #[cfg(not(windows))]
    fn native() -> StateResult<Self> {
        let home = home_directory()?;
        let state_home =
            environment_path("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"));
        let data_home =
            environment_path("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        let config_home =
            environment_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let root = state_home.join("colay");
        let runtime = environment_path("XDG_RUNTIME_DIR")
            .map(|directory| directory.join("colay"))
            .unwrap_or_else(|| root.join("runtime"));
        Ok(Self {
            database: root.join("state.db"),
            backups: root.join("backups"),
            workspaces: data_home.join("colay/workspaces"),
            runtime,
            config: config_home.join("colay/config.toml"),
            root,
        })
    }

    #[cfg(windows)]
    fn native() -> StateResult<Self> {
        let local = environment_path("LOCALAPPDATA")
            .ok_or_else(|| StateError::StateEnvironment("LOCALAPPDATA is not set".to_owned()))?;
        let roaming = environment_path("APPDATA")
            .ok_or_else(|| StateError::StateEnvironment("APPDATA is not set".to_owned()))?;
        let root = local.join("Colay");
        Ok(Self {
            database: root.join("state/state.db"),
            backups: root.join("state/backups"),
            workspaces: root.join("data/workspaces"),
            runtime: root.join("runtime"),
            config: roaming.join("Colay/config.toml"),
            root,
        })
    }
}

#[cfg(not(windows))]
fn home_directory() -> StateResult<PathBuf> {
    environment_path("HOME")
        .ok_or_else(|| StateError::StateEnvironment("HOME is not set".to_owned()))
}

fn environment_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
