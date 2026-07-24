use std::{error::Error, fs, path::PathBuf};

use orchestrator_state::{
    Database, GlobalStatePaths, StateEnvironment, StateEnvironmentTestInput, WorkspaceKind,
};

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
fn native_linux_allows_mnt_paths_when_not_running_under_wsl() -> TestResult {
    let environment = StateEnvironment::for_test(StateEnvironmentTestInput {
        home: Some(PathBuf::from("/home/test")),
        xdg_state_home: Some(PathBuf::from("/mnt/c/state")),
        xdg_data_home: Some(PathBuf::from("/mnt/c/data")),
        xdg_config_home: Some(PathBuf::from("/mnt/c/config")),
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
