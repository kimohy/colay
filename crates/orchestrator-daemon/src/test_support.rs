#![allow(dead_code)]

use orchestrator_state::{Database, StateResult, WorkspaceDatabase, WorkspaceId, WorkspaceKind};
use rusqlite::{Connection, Transaction, functions::FunctionFlags};

pub(crate) fn fresh_database() -> StateResult<(Database, WorkspaceId)> {
    let root = tempfile::tempdir()
        .map_err(|error| {
            orchestrator_state::StateError::InvalidRecord(format!("test tempdir failed: {error}"))
        })?
        .keep();
    let database = Database::open(root.join("state.db"))?;
    database.migrate_with_backup(&root.join("backups"))?;
    let workspace_path = root.join("workspace");
    std::fs::create_dir_all(&workspace_path).map_err(|error| {
        orchestrator_state::StateError::InvalidRecord(format!(
            "test workspace creation failed: {error}"
        ))
    })?;
    let workspace_id = database
        .resolve_workspace(&workspace_path, WorkspaceKind::Directory)?
        .workspace_id;
    Ok((database, workspace_id))
}

pub(crate) fn with_database<T>(
    database: &Database,
    operation: impl FnOnce(&Connection) -> StateResult<T>,
) -> StateResult<T> {
    let connection = Connection::open(database.path())?;
    operation(&connection)
}

pub(crate) fn with_workspace<T>(
    database: &WorkspaceDatabase<'_>,
    operation: impl FnOnce(&Connection) -> StateResult<T>,
) -> StateResult<T> {
    let mut connection = workspace_connection(database)?;
    operation(&mut connection)
}

pub(crate) fn with_workspace_transaction<T>(
    database: &WorkspaceDatabase<'_>,
    operation: impl FnOnce(&Transaction<'_>) -> StateResult<T>,
) -> StateResult<T> {
    let mut connection = workspace_connection(database)?;
    let transaction = connection.transaction()?;
    let result = operation(&transaction)?;
    transaction.commit()?;
    Ok(result)
}

fn workspace_connection(database: &WorkspaceDatabase<'_>) -> StateResult<Connection> {
    let connection = Connection::open(database.database_path())?;
    let workspace_id = database.workspace_id().to_string();
    connection.create_scalar_function(
        "current_workspace",
        0,
        FunctionFlags::SQLITE_DETERMINISTIC,
        move |_| Ok(workspace_id.clone()),
    )?;
    Ok(connection)
}
