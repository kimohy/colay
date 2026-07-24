use std::{env, path::PathBuf};

use crate::{StateError, StateResult, WorkspaceId};

#[derive(Clone, Debug, Default)]
pub struct StateEnvironment {
    colay_home: Option<PathBuf>,
    home: Option<PathBuf>,
    xdg_state_home: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    xdg_runtime_dir: Option<PathBuf>,
    local_app_data: Option<PathBuf>,
    app_data: Option<PathBuf>,
    is_wsl: bool,
}

#[derive(Clone, Debug, Default)]
pub struct StateEnvironmentTestInput {
    pub colay_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub xdg_state_home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_runtime_dir: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
    pub app_data: Option<PathBuf>,
    pub is_wsl: bool,
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
            colay_home: captured_path("COLAY_HOME"),
            home: captured_path("HOME"),
            xdg_state_home: captured_path("XDG_STATE_HOME"),
            xdg_data_home: captured_path("XDG_DATA_HOME"),
            xdg_config_home: captured_path("XDG_CONFIG_HOME"),
            xdg_runtime_dir: captured_path("XDG_RUNTIME_DIR"),
            local_app_data: captured_path("LOCALAPPDATA"),
            app_data: captured_path("APPDATA"),
            is_wsl: environment_is_set("WSL_DISTRO_NAME") || environment_is_set("WSL_INTEROP"),
        }
    }

    pub fn with_colay_home(colay_home: PathBuf) -> StateResult<Self> {
        let environment = Self {
            colay_home: Some(colay_home),
            ..Self::from_process()
        };
        environment.validate_colay_home()?;
        Ok(environment)
    }

    pub fn for_test(input: StateEnvironmentTestInput) -> StateResult<Self> {
        let environment = Self {
            colay_home: input.colay_home,
            home: input.home,
            xdg_state_home: input.xdg_state_home,
            xdg_data_home: input.xdg_data_home,
            xdg_config_home: input.xdg_config_home,
            xdg_runtime_dir: input.xdg_runtime_dir,
            local_app_data: input.local_app_data,
            app_data: input.app_data,
            is_wsl: input.is_wsl,
        };
        environment.validate_colay_home()?;
        Ok(environment)
    }

    fn validate_colay_home(&self) -> StateResult<()> {
        let Some(colay_home) = &self.colay_home else {
            return Ok(());
        };
        if colay_home.as_os_str().is_empty() {
            return Err(StateError::InvalidConfig(
                "COLAY_HOME must not be empty".to_owned(),
            ));
        }
        self.reject_wsl_windows_mount("COLAY_HOME", colay_home)?;
        if !colay_home.is_absolute() {
            return Err(StateError::InvalidConfig(format!(
                "COLAY_HOME must be absolute: {}",
                colay_home.display()
            )));
        }
        Ok(())
    }

    fn reject_wsl_windows_mount(&self, label: &str, path: &std::path::Path) -> StateResult<()> {
        if self.is_wsl && is_windows_drive_mount(path) {
            return Err(StateError::InvalidConfig(format!(
                "{label} is on a Windows drive mount ({}); Windows and WSL must use separate native filesystems. Choose a Linux-native state directory.",
                path.display()
            )));
        }
        Ok(())
    }
}

impl GlobalStatePaths {
    pub fn resolve(environment: &StateEnvironment) -> StateResult<Self> {
        if let Some(root) = &environment.colay_home {
            environment.validate_colay_home()?;
            let paths = Self::under_colay_home(root.clone());
            environment.reject_wsl_windows_mount("COLAY_HOME state root", &paths.root)?;
            environment.reject_wsl_windows_mount("COLAY_HOME data root", &paths.workspaces)?;
            return Ok(paths);
        }
        Self::native(environment)
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
    fn native(environment: &StateEnvironment) -> StateResult<Self> {
        let home = home_directory(environment)?;
        let state_home = environment
            .xdg_state_home
            .clone()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| home.join(".local/state"));
        let data_home = environment
            .xdg_data_home
            .clone()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| home.join(".local/share"));
        let config_home = environment
            .xdg_config_home
            .clone()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| home.join(".config"));
        let root = state_home.join("colay");
        let runtime = environment
            .xdg_runtime_dir
            .clone()
            .filter(|path| !path.as_os_str().is_empty())
            .map(|directory| directory.join("colay"))
            .unwrap_or_else(|| root.join("runtime"));
        let paths = Self {
            database: root.join("state.db"),
            backups: root.join("backups"),
            workspaces: data_home.join("colay/workspaces"),
            runtime,
            config: config_home.join("colay/config.toml"),
            root,
        };
        environment.reject_wsl_windows_mount("XDG state root", &paths.root)?;
        environment.reject_wsl_windows_mount("XDG data root", &paths.workspaces)?;
        Ok(paths)
    }

    #[cfg(windows)]
    fn native(environment: &StateEnvironment) -> StateResult<Self> {
        let _ = (
            &environment.home,
            &environment.xdg_state_home,
            &environment.xdg_data_home,
            &environment.xdg_config_home,
            &environment.xdg_runtime_dir,
        );
        let local = environment
            .local_app_data
            .clone()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| StateError::StateEnvironment("LOCALAPPDATA is not set".to_owned()))?;
        let roaming = environment
            .app_data
            .clone()
            .filter(|path| !path.as_os_str().is_empty())
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
fn home_directory(environment: &StateEnvironment) -> StateResult<PathBuf> {
    environment
        .home
        .clone()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| StateError::StateEnvironment("HOME is not set".to_owned()))
}

fn captured_path(name: &str) -> Option<PathBuf> {
    env::var_os(name).map(PathBuf::from)
}

fn environment_is_set(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn is_windows_drive_mount(path: &std::path::Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let Some(remainder) = normalized.strip_prefix("/mnt/") else {
        return false;
    };
    let Some((drive, _)) = remainder.split_once('/') else {
        return false;
    };
    drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic()
}
