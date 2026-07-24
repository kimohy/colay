#![allow(dead_code, clippy::missing_errors_doc)]

use std::path::Path;

use orchestrator_state::{Database, StateResult, WorkspaceDatabase, WorkspaceId, WorkspaceKind};
use rusqlite::{Connection, functions::FunctionFlags};

pub fn fresh_database() -> Result<(Database, WorkspaceId), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?.keep();
    let database = Database::open(root.join("state.db"))?;
    database.migrate_with_backup(&root.join("backups"))?;
    let workspace_path = root.join("workspace");
    std::fs::create_dir_all(&workspace_path)?;
    let registration = database.resolve_workspace(&workspace_path, WorkspaceKind::Directory)?;
    Ok((database, registration.workspace_id))
}

pub fn with_database_connection<T>(
    database: &Database,
    operation: impl FnOnce(&Connection) -> StateResult<T>,
) -> StateResult<T> {
    let connection = open_test_connection(database.path())?;
    operation(&connection)
}

pub fn with_workspace_connection<T>(
    database_path: &Path,
    database: &WorkspaceDatabase<'_>,
    operation: impl FnOnce(&Connection) -> StateResult<T>,
) -> StateResult<T> {
    let connection = open_test_connection(database_path)?;
    let workspace_id = database.workspace_id().to_string();
    connection.create_scalar_function(
        "current_workspace",
        0,
        FunctionFlags::SQLITE_DETERMINISTIC,
        move |_| Ok(workspace_id.clone()),
    )?;
    operation(&connection)
}

#[allow(dead_code)]
pub fn database_at(path: &Path) -> Result<Connection, rusqlite::Error> {
    open_test_connection(path)
}

fn open_test_connection(path: &Path) -> Result<Connection, rusqlite::Error> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;\
         PRAGMA journal_mode = WAL;\
         PRAGMA synchronous = FULL;\
         PRAGMA temp_store = MEMORY;\
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(connection)
}
