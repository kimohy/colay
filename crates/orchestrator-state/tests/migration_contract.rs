use std::path::Path;

use chrono::Utc;
use orchestrator_domain::{
    CorrelationId, EventActor, EventId, EventType, SchemaVersion, TaskEvent,
};
use orchestrator_state::{Database, MigrationManager, STATE_SCHEMA_VERSION, StateError};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::json;
use sha2::{Digest as _, Sha256};

const CORE_MIGRATION: &str = include_str!("../../../migrations/0001_core.sql");
const EXECUTION_MIGRATION: &str = include_str!("../../../migrations/0002_execution.sql");
const AUDIT_MIGRATION: &str = include_str!("../../../migrations/0003_audit_and_control.sql");

fn seed_v1(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let connection = Connection::open(path)?;
    connection.execute_batch(CORE_MIGRATION)?;
    connection.execute(
        "INSERT INTO schema_migrations(version, name, checksum, applied_at) \
         VALUES (1, 'core', ?1, ?2)",
        params![
            format!("{:x}", Sha256::digest(CORE_MIGRATION.as_bytes())),
            Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

#[test]
fn v1_to_current_dry_run_is_non_mutating_and_apply_keeps_a_readable_backup()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = std::fs::canonicalize(directory.path())?;
    let database_path = root.join("orchestrator.db");
    let backup_directory = root.join("backups");
    seed_v1(&database_path)?;
    let database = Database::open(&database_path)?;

    let initial = database.migration_status()?;
    assert_eq!(initial.current_version, 1);
    assert_eq!(initial.pending_versions, vec![2, 3, 4, 5]);

    let dry_run = database.dry_run_migrations()?;
    assert_eq!(dry_run.current_version, STATE_SCHEMA_VERSION);
    assert_eq!(database.migration_status()?.current_version, 1);

    let applied = database.migrate_with_backup(&backup_directory)?;
    assert_eq!(applied.current_version, STATE_SCHEMA_VERSION);
    assert!(applied.pending_versions.is_empty());
    let health = database.health()?;
    assert!(health.integrity_ok);
    assert_eq!(health.foreign_key_violations, 0);
    database.with_connection(|connection| {
        for table in [
            "sessions",
            "conversation_messages",
            "client_commands",
            "daemon_instances",
            "session_workspace_state",
        ] {
            let count: i64 = connection.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )?;
            assert_eq!(count, 1, "missing table {table}");
        }
        let session_column_count: i64 = connection.query_row(
            "SELECT count(*) FROM pragma_table_info('task_events') WHERE name = 'session_id'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(session_column_count, 1);
        Ok(())
    })?;

    let backups = std::fs::read_dir(&backup_directory)?
        .map(|entry| entry.map(|value| value.path()))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(backups.len(), 1);
    let backup_path = backups.first().ok_or("migration backup was not created")?;
    let backup = Connection::open_with_flags(backup_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let backup_status = MigrationManager::status(&backup)?;
    assert_eq!(backup_status.current_version, 1);
    assert_eq!(backup_status.pending_versions, vec![2, 3, 4, 5]);
    Ok(())
}

fn seed_v3(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    seed_v1(path)?;
    let connection = Connection::open(path)?;
    for (version, name, sql) in [
        (2, "execution", EXECUTION_MIGRATION),
        (3, "audit_and_control", AUDIT_MIGRATION),
    ] {
        connection.execute_batch(sql)?;
        connection.execute(
            "INSERT INTO schema_migrations(version, name, checksum, applied_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                version,
                name,
                format!("{:x}", Sha256::digest(sql.as_bytes())),
                Utc::now().to_rfc3339()
            ],
        )?;
    }
    Ok(())
}

#[test]
fn v3_event_hash_remains_verifiable_after_current_migration()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = std::fs::canonicalize(directory.path())?;
    let database_path = root.join("orchestrator.db");
    seed_v3(&database_path)?;
    let database = Database::open(&database_path)?;
    let status = database.migration_status()?;
    assert_eq!(status.current_version, 3);
    let historical = database.append_event(TaskEvent {
        schema_version: SchemaVersion::new(SchemaVersion::V3),
        sequence: 0,
        event_id: EventId::new(),
        session_id: None,
        task_id: None,
        occurred_at: Utc::now(),
        event_type: EventType::CompatibilityWarning,
        from_state: None,
        to_state: None,
        reason: Some("historical event".to_owned()),
        actor: EventActor::System,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload: json!({}),
        previous_hash: None,
        event_hash: String::new(),
    })?;
    assert!(historical.verify_hash()?);

    let migrated = database.migrate_with_backup(&root.join("backups-v4"))?;
    assert_eq!(migrated.current_version, STATE_SCHEMA_VERSION);
    let reloaded = database.event_at(1)?.ok_or("historical event missing")?;
    assert_eq!(reloaded, historical);
    assert!(reloaded.verify_hash()?);
    Ok(())
}

#[test]
fn checksum_tampering_and_future_schemas_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
    let first = tempfile::tempdir()?;
    let first_root = std::fs::canonicalize(first.path())?;
    let first_path = first_root.join("orchestrator.db");
    seed_v1(&first_path)?;
    let first_database = Database::open(&first_path)?;
    first_database.migrate_with_backup(&first_root.join("backups"))?;
    first_database.with_connection(|connection| {
        connection.execute(
            "UPDATE schema_migrations SET checksum = ?1 WHERE version = 2",
            ["0".repeat(64)],
        )?;
        Ok(())
    })?;
    assert!(matches!(
        first_database.migration_status(),
        Err(StateError::MigrationChecksum { version: 2 })
    ));

    let second = tempfile::tempdir()?;
    let second_root = std::fs::canonicalize(second.path())?;
    let second_path = second_root.join("orchestrator.db");
    seed_v1(&second_path)?;
    let second_database = Database::open(&second_path)?;
    second_database.migrate_with_backup(&second_root.join("backups"))?;
    second_database.with_connection(|connection| {
        let future = STATE_SCHEMA_VERSION + 1;
        connection.execute(
            "INSERT INTO schema_migrations(version, name, checksum, applied_at) \
             VALUES (?1, 'future', ?2, ?3)",
            params![future, "f".repeat(64), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    })?;
    assert!(matches!(
        second_database.migration_status(),
        Err(StateError::FutureSchema {
            found,
            supported: STATE_SCHEMA_VERSION
        }) if found == STATE_SCHEMA_VERSION + 1
    ));
    Ok(())
}
