use std::path::Path;

use chrono::Utc;
use orchestrator_state::{Database, MigrationManager, STATE_SCHEMA_VERSION, StateError};
use rusqlite::{Connection, OpenFlags, params};
use sha2::{Digest as _, Sha256};

const CORE_MIGRATION: &str = include_str!("../../../migrations/0001_core.sql");

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
fn v1_to_v3_dry_run_is_non_mutating_and_apply_keeps_a_readable_backup()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = std::fs::canonicalize(directory.path())?;
    let database_path = root.join("orchestrator.db");
    let backup_directory = root.join("backups");
    seed_v1(&database_path)?;
    let database = Database::open(&database_path)?;

    let initial = database.migration_status()?;
    assert_eq!(initial.current_version, 1);
    assert_eq!(initial.pending_versions, vec![2, 3]);

    let dry_run = database.dry_run_migrations()?;
    assert_eq!(dry_run.current_version, STATE_SCHEMA_VERSION);
    assert_eq!(database.migration_status()?.current_version, 1);

    let applied = database.migrate_with_backup(&backup_directory)?;
    assert_eq!(applied.current_version, STATE_SCHEMA_VERSION);
    assert!(applied.pending_versions.is_empty());
    let health = database.health()?;
    assert!(health.integrity_ok);
    assert_eq!(health.foreign_key_violations, 0);

    let backups = std::fs::read_dir(&backup_directory)?
        .map(|entry| entry.map(|value| value.path()))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(backups.len(), 1);
    let backup_path = backups.first().ok_or("migration backup was not created")?;
    let backup = Connection::open_with_flags(backup_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let backup_status = MigrationManager::status(&backup)?;
    assert_eq!(backup_status.current_version, 1);
    assert_eq!(backup_status.pending_versions, vec![2, 3]);
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
        connection.execute(
            "INSERT INTO schema_migrations(version, name, checksum, applied_at) \
             VALUES (4, 'future', ?1, ?2)",
            params!["f".repeat(64), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    })?;
    assert!(matches!(
        second_database.migration_status(),
        Err(StateError::FutureSchema {
            found: 4,
            supported: STATE_SCHEMA_VERSION
        })
    ));
    Ok(())
}
