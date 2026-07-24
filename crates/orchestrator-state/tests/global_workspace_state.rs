use std::{error::Error, fs, path::PathBuf};

use orchestrator_state::{
    Database, GlobalStatePaths, StateEnvironment, StateEnvironmentTestInput, WorkspaceKind,
};
use rusqlite::Connection;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[test]
fn colay_home_override_is_independent_of_current_directory() -> TestResult {
    let root = tempfile::tempdir()?;
    let environment = StateEnvironment::with_colay_home(root.path().join("home"))?;

    let paths = GlobalStatePaths::resolve(&environment)?;

    assert_eq!(paths.database, root.path().join("home/state/state.db"));
    assert_eq!(paths.workspaces, root.path().join("home/data/workspaces"));
    Ok(())
}

#[test]
fn empty_colay_home_is_rejected() -> TestResult {
    let Err(error) = StateEnvironment::with_colay_home(PathBuf::new()) else {
        return Err("empty COLAY_HOME was accepted".into());
    };

    assert!(error.to_string().contains("COLAY_HOME must not be empty"));
    Ok(())
}

#[test]
fn relative_colay_home_is_rejected() -> TestResult {
    let Err(error) = StateEnvironment::with_colay_home(PathBuf::from("relative-home")) else {
        return Err("relative COLAY_HOME was accepted".into());
    };

    assert!(error.to_string().contains("COLAY_HOME must be absolute"));
    Ok(())
}

#[test]
fn wsl_rejects_colay_home_on_a_windows_drive_mount() -> TestResult {
    let Err(error) = StateEnvironment::for_test(StateEnvironmentTestInput {
        colay_home: Some(PathBuf::from("/mnt/c/colay")),
        is_wsl: true,
        ..StateEnvironmentTestInput::default()
    }) else {
        return Err("WSL COLAY_HOME on a Windows mount was accepted".into());
    };

    assert!(error.to_string().contains("separate native filesystems"));
    Ok(())
}

#[test]
fn wsl_kernel_and_mountinfo_reject_custom_windows_mount_without_environment_hints() -> TestResult {
    let Err(error) = StateEnvironment::for_test(StateEnvironmentTestInput {
        colay_home: Some(PathBuf::from("/windows/c/colay")),
        kernel_release: Some("6.6.87.2-microsoft-standard-WSL2".to_owned()),
        mountinfo: Some("36 25 0:32 / /windows/c rw,relatime - 9p drvfs rw,dirsync".to_owned()),
        ..StateEnvironmentTestInput::default()
    }) else {
        return Err("WSL drvfs mount was accepted without environment hints".into());
    };

    assert!(error.to_string().contains("separate native filesystems"));
    Ok(())
}

#[test]
fn explicit_drvfs_mount_is_rejected_without_wsl_evidence() -> TestResult {
    let Err(error) = StateEnvironment::for_test(StateEnvironmentTestInput {
        colay_home: Some(PathBuf::from("/windows/c/colay")),
        mountinfo: Some("36 25 0:32 / /windows/c rw,relatime - drvfs C: rw".to_owned()),
        ..StateEnvironmentTestInput::default()
    }) else {
        return Err("explicit drvfs mount was accepted without WSL evidence".into());
    };

    assert!(error.to_string().contains("Windows-backed mount"));
    Ok(())
}

#[test]
fn drvfs_9p_evidence_is_rejected_without_wsl_evidence() -> TestResult {
    let Err(error) = StateEnvironment::for_test(StateEnvironmentTestInput {
        colay_home: Some(PathBuf::from("/windows/c/colay")),
        mountinfo: Some(
            "36 25 0:32 / /windows/c rw,relatime - 9p server rw,aname=drvfs".to_owned(),
        ),
        ..StateEnvironmentTestInput::default()
    }) else {
        return Err("9p drvfs evidence was accepted without WSL evidence".into());
    };

    assert!(error.to_string().contains("Windows-backed mount"));
    Ok(())
}

#[test]
fn wsl_kernel_version_rejects_unknown_custom_mount_when_mountinfo_is_unavailable() -> TestResult {
    let Err(error) = StateEnvironment::for_test(StateEnvironmentTestInput {
        colay_home: Some(PathBuf::from("/linux/state/colay")),
        kernel_version: Some("Linux version 6.6.87.2-microsoft-standard-WSL2".to_owned()),
        ..StateEnvironmentTestInput::default()
    }) else {
        return Err("WSL state without mountinfo was accepted".into());
    };

    assert!(
        error
            .to_string()
            .contains("mount information is unavailable")
    );
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn wsl_rejects_windows_mounted_xdg_state_root() -> TestResult {
    let environment = StateEnvironment::for_test(wsl_test_input(
        Some(PathBuf::from("/mnt/c/state")),
        Some(PathBuf::from("/home/test/data")),
    ))?;

    let Err(error) = GlobalStatePaths::resolve(&environment) else {
        return Err("WSL state root on a Windows mount was accepted".into());
    };

    assert!(error.to_string().contains("separate native filesystems"));
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn wsl_rejects_windows_mounted_xdg_data_root() -> TestResult {
    let environment = StateEnvironment::for_test(wsl_test_input(
        Some(PathBuf::from("/home/test/state")),
        Some(PathBuf::from("/mnt/d/data")),
    ))?;

    let Err(error) = GlobalStatePaths::resolve(&environment) else {
        return Err("WSL data root on a Windows mount was accepted".into());
    };

    assert!(error.to_string().contains("separate native filesystems"));
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn native_linux_allows_9p_mnt_paths_when_not_running_under_wsl() -> TestResult {
    let environment = StateEnvironment::for_test(StateEnvironmentTestInput {
        home: Some(PathBuf::from("/home/test")),
        xdg_state_home: Some(PathBuf::from("/mnt/c/state")),
        xdg_data_home: Some(PathBuf::from("/mnt/c/data")),
        xdg_config_home: Some(PathBuf::from("/mnt/c/config")),
        mountinfo: Some("36 25 0:32 / /mnt/c rw,relatime - 9p server rw".to_owned()),
        is_wsl: false,
        ..StateEnvironmentTestInput::default()
    })?;

    let paths = GlobalStatePaths::resolve(&environment)?;

    assert_eq!(paths.database, PathBuf::from("/mnt/c/state/colay/state.db"));
    assert_eq!(
        paths.workspaces,
        PathBuf::from("/mnt/c/data/colay/workspaces")
    );
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn complete_xdg_paths_do_not_require_home() -> TestResult {
    let environment = StateEnvironment::for_test(StateEnvironmentTestInput {
        xdg_state_home: Some(PathBuf::from("/state")),
        xdg_data_home: Some(PathBuf::from("/data")),
        xdg_config_home: Some(PathBuf::from("/config")),
        xdg_runtime_dir: Some(PathBuf::from("/runtime")),
        ..StateEnvironmentTestInput::default()
    })?;

    let paths = GlobalStatePaths::resolve(&environment)?;

    assert_eq!(paths.database, PathBuf::from("/state/colay/state.db"));
    assert_eq!(paths.workspaces, PathBuf::from("/data/colay/workspaces"));
    assert_eq!(paths.config, PathBuf::from("/config/colay/config.toml"));
    assert_eq!(paths.runtime, PathBuf::from("/runtime/colay"));
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn relative_xdg_roots_are_rejected_before_paths_are_joined() -> TestResult {
    for (label, input) in [
        (
            "XDG_STATE_HOME",
            StateEnvironmentTestInput {
                xdg_state_home: Some(PathBuf::from("relative-state")),
                ..absolute_unix_environment()
            },
        ),
        (
            "XDG_DATA_HOME",
            StateEnvironmentTestInput {
                xdg_data_home: Some(PathBuf::from("relative-data")),
                ..absolute_unix_environment()
            },
        ),
        (
            "XDG_CONFIG_HOME",
            StateEnvironmentTestInput {
                xdg_config_home: Some(PathBuf::from("relative-config")),
                ..absolute_unix_environment()
            },
        ),
        (
            "XDG_RUNTIME_DIR",
            StateEnvironmentTestInput {
                xdg_runtime_dir: Some(PathBuf::from("relative-runtime")),
                ..absolute_unix_environment()
            },
        ),
        (
            "HOME",
            StateEnvironmentTestInput {
                home: Some(PathBuf::from("relative-home")),
                xdg_state_home: None,
                xdg_data_home: None,
                xdg_config_home: None,
                ..absolute_unix_environment()
            },
        ),
    ] {
        let environment = StateEnvironment::for_test(input)?;
        let Err(error) = GlobalStatePaths::resolve(&environment) else {
            return Err(format!("{label} created a current-directory-relative state path").into());
        };
        assert!(error.to_string().contains(label));
    }
    Ok(())
}

#[cfg(windows)]
#[test]
fn relative_windows_roots_are_rejected_before_paths_are_joined() -> TestResult {
    for (label, input) in [
        (
            "LOCALAPPDATA",
            StateEnvironmentTestInput {
                local_app_data: Some(PathBuf::from("relative-local")),
                app_data: Some(PathBuf::from(r"C:\\Users\\test\\AppData\\Roaming")),
                ..StateEnvironmentTestInput::default()
            },
        ),
        (
            "APPDATA",
            StateEnvironmentTestInput {
                local_app_data: Some(PathBuf::from(r"C:\\Users\\test\\AppData\\Local")),
                app_data: Some(PathBuf::from("relative-roaming")),
                ..StateEnvironmentTestInput::default()
            },
        ),
    ] {
        let environment = StateEnvironment::for_test(input)?;
        let Err(error) = GlobalStatePaths::resolve(&environment) else {
            return Err(format!("{label} created a current-directory-relative state path").into());
        };
        assert!(error.to_string().contains(label));
    }
    Ok(())
}

#[test]
fn two_directories_receive_distinct_stable_workspace_ids() -> TestResult {
    let fixture = GlobalFixture::new()?;

    let first = fixture
        .database
        .resolve_workspace(&fixture.first, WorkspaceKind::Directory)?;
    let again = fixture
        .database
        .resolve_workspace(&fixture.first, WorkspaceKind::Directory)?;
    let second = fixture
        .database
        .resolve_workspace(&fixture.second, WorkspaceKind::Directory)?;

    assert_eq!(first.workspace_id, again.workspace_id);
    assert_ne!(first.workspace_id, second.workspace_id);
    Ok(())
}

#[test]
fn fresh_repository_resolution_creates_a_uuid_v7_workspace() -> TestResult {
    let fixture = GlobalFixture::new()?;

    let registration = fixture
        .database
        .resolve_repository_workspace(&fixture.first)?;

    assert_eq!(registration.workspace_id.as_uuid().get_version_num(), 7);
    assert_ne!(
        registration.workspace_id.to_string(),
        "00000000-0000-0000-0000-000000000001"
    );
    assert_eq!(
        registration.canonical_path,
        fs::canonicalize(&fixture.first)?
    );
    Ok(())
}

#[test]
fn migrated_reserved_workspace_is_attached_once_and_never_reused() -> TestResult {
    let root = tempfile::tempdir()?;
    let first = root.path().join("first");
    let second = root.path().join("second");
    fs::create_dir_all(&first)?;
    fs::create_dir_all(&second)?;
    let database_path = root.path().join("state/state.db");
    {
        let database = Database::open(&database_path)?;
        database.migrate_with_backup(&root.path().join("backups"))?;
    }
    let connection = Connection::open(&database_path)?;
    connection.execute(
        "INSERT INTO workspaces(workspace_id, kind, status, created_at, last_seen_at) \
         VALUES ('00000000-0000-0000-0000-000000000001', 'directory', 'detached', ?1, ?1)",
        [chrono::Utc::now().to_rfc3339()],
    )?;
    drop(connection);
    let database = Database::open(&database_path)?;

    let adopted = database.resolve_repository_workspace(&first)?;
    let stable = database.resolve_repository_workspace(&first)?;
    let new_workspace = database.resolve_repository_workspace(&second)?;

    assert_eq!(
        adopted.workspace_id.to_string(),
        "00000000-0000-0000-0000-000000000001"
    );
    assert_eq!(stable.workspace_id, adopted.workspace_id);
    assert_ne!(new_workspace.workspace_id, adopted.workspace_id);
    assert_eq!(new_workspace.workspace_id.as_uuid().get_version_num(), 7);
    Ok(())
}

#[test]
fn attach_workspace_updates_the_registered_current_path() -> TestResult {
    let fixture = GlobalFixture::new()?;
    let registered = fixture
        .database
        .resolve_workspace(&fixture.first, WorkspaceKind::Directory)?;

    let attached = fixture
        .database
        .attach_workspace(registered.workspace_id, &fixture.moved)?;
    let loaded = fixture
        .database
        .load_workspace(registered.workspace_id)?
        .ok_or("registered workspace was not found")?;

    assert_eq!(attached.workspace_id, registered.workspace_id);
    assert_eq!(attached.canonical_path, fs::canonicalize(&fixture.moved)?);
    assert_eq!(loaded, attached);
    Ok(())
}

#[test]
fn git_workspace_uses_its_canonical_common_directory_for_identity() -> TestResult {
    let fixture = GlobalFixture::new()?;
    let repository = fixture.root_path().join("repository");
    let common_directory = repository.join(".git");
    fs::create_dir_all(&common_directory)?;

    let registration = fixture
        .database
        .resolve_workspace(&repository, WorkspaceKind::Git)?;
    let again = fixture
        .database
        .resolve_workspace(&repository, WorkspaceKind::Git)?;

    assert_eq!(registration.workspace_id, again.workspace_id);
    assert_eq!(
        registration.git_common_dir,
        Some(fs::canonicalize(common_directory)?)
    );
    Ok(())
}

#[cfg(not(windows))]
fn wsl_test_input(
    xdg_state_home: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
) -> StateEnvironmentTestInput {
    StateEnvironmentTestInput {
        home: Some(PathBuf::from("/home/test")),
        xdg_state_home,
        xdg_data_home,
        xdg_config_home: Some(PathBuf::from("/home/test/config")),
        is_wsl: true,
        ..StateEnvironmentTestInput::default()
    }
}

#[cfg(not(windows))]
fn absolute_unix_environment() -> StateEnvironmentTestInput {
    StateEnvironmentTestInput {
        home: Some(PathBuf::from("/home/test")),
        xdg_state_home: Some(PathBuf::from("/home/test/state")),
        xdg_data_home: Some(PathBuf::from("/home/test/data")),
        xdg_config_home: Some(PathBuf::from("/home/test/config")),
        xdg_runtime_dir: Some(PathBuf::from("/run/test")),
        ..StateEnvironmentTestInput::default()
    }
}

struct GlobalFixture {
    root: tempfile::TempDir,
    database: Database,
    first: PathBuf,
    second: PathBuf,
    moved: PathBuf,
}

impl GlobalFixture {
    fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let first = root.path().join("first");
        let second = root.path().join("second");
        let moved = root.path().join("moved");
        fs::create_dir_all(&first)?;
        fs::create_dir_all(&second)?;
        fs::create_dir_all(&moved)?;

        let database = Database::open(root.path().join("state/state.db"))?;
        database.migrate_with_backup(&root.path().join("backups"))?;
        Ok(Self {
            root,
            database,
            first,
            second,
            moved,
        })
    }

    fn root_path(&self) -> &std::path::Path {
        self.root.path()
    }
}
