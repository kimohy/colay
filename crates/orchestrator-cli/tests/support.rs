#![allow(dead_code, clippy::missing_errors_doc)]

use orchestrator_state::{Database, StateResult, WorkspaceDatabase};
use rusqlite::{Connection, functions::FunctionFlags};

pub fn with_database<T>(
    database: &Database,
    operation: impl FnOnce(&Connection) -> StateResult<T>,
) -> StateResult<T> {
    let connection = Connection::open(database.path())?;
    operation(&connection)
}

pub fn with_workspace<T>(
    database: &WorkspaceDatabase<'_>,
    operation: impl FnOnce(&Connection) -> StateResult<T>,
) -> StateResult<T> {
    let connection = Connection::open(database.database_path())?;
    let workspace_id = database.workspace_id().to_string();
    connection.create_scalar_function(
        "current_workspace",
        0,
        FunctionFlags::SQLITE_DETERMINISTIC,
        move |_| Ok(workspace_id.clone()),
    )?;
    operation(&connection)
}
