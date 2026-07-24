use std::{error::Error, fs, path::PathBuf};

use orchestrator_state::{Database, GlobalStatePaths, StateEnvironment, WorkspaceKind};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[test]
fn colay_home_override_is_independent_of_current_directory() -> TestResult {
    let root = tempfile::tempdir()?;
    let environment = StateEnvironment::with_colay_home(root.path().join("home"));

    let paths = GlobalStatePaths::resolve(&environment)?;

    assert_eq!(paths.database, root.path().join("home/state/state.db"));
    assert_eq!(paths.workspaces, root.path().join("home/data/workspaces"));
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

struct GlobalFixture {
    _root: tempfile::TempDir,
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
            _root: root,
            database,
            first,
            second,
            moved,
        })
    }
}
