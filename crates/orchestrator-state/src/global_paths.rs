use std::{
    env, fs,
    path::{Path, PathBuf},
};

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
    user_profile: Option<PathBuf>,
    is_wsl: bool,
    kernel_release: Option<String>,
    mountinfo: Option<String>,
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
    pub user_profile: Option<PathBuf>,
    pub is_wsl: bool,
    pub kernel_release: Option<String>,
    pub mountinfo: Option<String>,
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
            user_profile: captured_path("USERPROFILE"),
            is_wsl: environment_is_set("WSL_DISTRO_NAME") || environment_is_set("WSL_INTEROP"),
            kernel_release: read_optional_text("/proc/sys/kernel/osrelease"),
            mountinfo: read_optional_text("/proc/self/mountinfo"),
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
            user_profile: input.user_profile,
            is_wsl: input.is_wsl,
            kernel_release: input.kernel_release,
            mountinfo: input.mountinfo,
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
        if !self.is_wsl() {
            return Ok(());
        }
        let windows_backed = self.mountinfo.as_deref().map_or_else(
            || is_windows_drive_mount(path),
            |mountinfo| has_windows_backed_mount(path, mountinfo),
        );
        if windows_backed {
            return Err(StateError::InvalidConfig(format!(
                "{label} is on a Windows-backed mount ({}); Windows and WSL must use separate native filesystems. Choose a Linux-native state directory.",
                path.display()
            )));
        }
        Ok(())
    }

    fn is_wsl(&self) -> bool {
        self.is_wsl
            || self.kernel_release.as_deref().is_some_and(|release| {
                let release = release.to_ascii_lowercase();
                release.contains("microsoft") || release.contains("wsl")
            })
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
        let _ = (
            &environment.local_app_data,
            &environment.app_data,
            &environment.user_profile,
        );
        let home = home_directory(environment)?;
        let state_home = configured_or_default(
            environment.xdg_state_home.as_ref(),
            "XDG_STATE_HOME",
            || home.join(".local/state"),
        )?;
        let data_home =
            configured_or_default(environment.xdg_data_home.as_ref(), "XDG_DATA_HOME", || {
                home.join(".local/share")
            })?;
        let config_home = configured_or_default(
            environment.xdg_config_home.as_ref(),
            "XDG_CONFIG_HOME",
            || home.join(".config"),
        )?;
        let root = state_home.join("colay");
        let runtime = configured_path(environment.xdg_runtime_dir.as_ref(), "XDG_RUNTIME_DIR")?
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
            &environment.user_profile,
        );
        let local = required_configured_path(environment.local_app_data.as_ref(), "LOCALAPPDATA")?;
        let roaming = required_configured_path(environment.app_data.as_ref(), "APPDATA")?;
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
    required_configured_path(environment.home.as_ref(), "HOME")
}

#[cfg(not(windows))]
fn configured_or_default(
    path: Option<&PathBuf>,
    label: &str,
    fallback: impl FnOnce() -> PathBuf,
) -> StateResult<PathBuf> {
    Ok(configured_path(path, label)?.unwrap_or_else(fallback))
}

fn configured_path(path: Option<&PathBuf>, label: &str) -> StateResult<Option<PathBuf>> {
    let Some(path) = path.filter(|path| !path.as_os_str().is_empty()) else {
        return Ok(None);
    };
    validate_absolute_path(path, label)?;
    Ok(Some(path.clone()))
}

fn required_configured_path(path: Option<&PathBuf>, label: &str) -> StateResult<PathBuf> {
    configured_path(path, label)?
        .ok_or_else(|| StateError::StateEnvironment(format!("{label} is not set")))
}

fn validate_absolute_path(path: &Path, label: &str) -> StateResult<()> {
    if !path.is_absolute() {
        return Err(StateError::InvalidConfig(format!(
            "{label} must be absolute: {}",
            path.display()
        )));
    }
    Ok(())
}

fn captured_path(name: &str) -> Option<PathBuf> {
    env::var_os(name).map(PathBuf::from)
}

fn environment_is_set(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn read_optional_text(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn has_windows_backed_mount(path: &Path, mountinfo: &str) -> bool {
    mountinfo
        .lines()
        .filter_map(parse_mountinfo_entry)
        .filter(|entry| path.starts_with(&entry.mount_point))
        .max_by_key(|entry| entry.mount_point.components().count())
        .is_some_and(|entry| entry.windows_backed)
}

struct MountinfoEntry {
    mount_point: PathBuf,
    windows_backed: bool,
}

fn parse_mountinfo_entry(line: &str) -> Option<MountinfoEntry> {
    let (before_separator, after_separator) = line.split_once(" - ")?;
    let mount_point = before_separator.split_whitespace().nth(4)?;
    let mut filesystem = after_separator.split_whitespace();
    let kind = filesystem.next()?;
    let source = filesystem.next()?;
    Some(MountinfoEntry {
        mount_point: unescape_mountinfo_path(mount_point),
        windows_backed: kind == "drvfs" || kind == "9p" || source == "drvfs",
    })
}

fn unescape_mountinfo_path(value: &str) -> PathBuf {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1..=index + 3].iter().all(u8::is_ascii_digit)
        {
            let octal = &value[index + 1..index + 4];
            if let Ok(character) = u8::from_str_radix(octal, 8) {
                decoded.push(character);
                index += 4;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&decoded).into_owned())
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::has_windows_backed_mount;

    #[test]
    fn mountinfo_parser_decodes_escaped_mount_points() {
        let mountinfo =
            "36 25 0:32 / /windows\\040drive\\011tab\\012line\\134slash/c rw - 9p drvfs rw";
        let path = Path::new("/windows drive\ttab\nline\\slash/c/state");

        assert!(has_windows_backed_mount(path, mountinfo));
    }
}
