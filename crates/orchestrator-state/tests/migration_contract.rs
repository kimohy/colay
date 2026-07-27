use std::path::Path;

use chrono::Utc;
use orchestrator_domain::{
    ClientCommandId, CorrelationId, EventActor, EventId, EventType, SchemaVersion, TaskEvent,
};
use orchestrator_state::{
    Database, MigrationManager, STATE_SCHEMA_VERSION, StateError, WorkspaceId,
};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::json;
use sha2::{Digest as _, Sha256};

mod support;
use support::with_database_connection;

const CORE_MIGRATION: &str = include_str!("../../../migrations/0001_core.sql");
const EXECUTION_MIGRATION: &str = include_str!("../../../migrations/0002_execution.sql");
const AUDIT_MIGRATION: &str = include_str!("../../../migrations/0003_audit_and_control.sql");
const SESSION_MIGRATION: &str = include_str!("../../../migrations/0004_durable_sessions.sql");
const WORKSPACE_MIGRATION: &str = include_str!("../../../migrations/0005_chat_workspace_state.sql");
const GRAPH_MIGRATION: &str = include_str!("../../../migrations/0006_approved_task_graphs.sql");
const PARALLEL_MIGRATION: &str = include_str!("../../../migrations/0007_parallel_execution.sql");
const INTEGRATION_MIGRATION: &str = include_str!("../../../migrations/0008_result_integration.sql");

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
    assert_eq!(
        initial.pending_versions,
        vec![2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
    );

    let dry_run = database.dry_run_migrations()?;
    assert_eq!(dry_run.current_version, STATE_SCHEMA_VERSION);
    assert_eq!(database.migration_status()?.current_version, 1);

    let applied = database.migrate_with_backup(&backup_directory)?;
    assert_eq!(applied.current_version, STATE_SCHEMA_VERSION);
    assert!(applied.pending_versions.is_empty());
    let health = database.health()?;
    assert!(health.integrity_ok);
    assert_eq!(health.foreign_key_violations, 0);
    with_database_connection(&database, |connection| {
        for table in [
            "sessions",
            "conversation_messages",
            "client_commands",
            "daemon_instances",
            "session_workspace_state",
            "graph_revisions",
            "planning_attempts",
            "session_tasks",
            "task_dependencies",
            "session_graph_heads",
            "graph_approvals",
            "task_schedule_claims",
            "resource_claims",
            "task_instructions",
            "integration_batches",
            "integration_sources",
            "integration_approvals",
            "integration_applications",
            "integration_resolution_tasks",
            "conversation_attempts",
            "requirement_revisions",
            "session_requirement_heads",
            "client_command_invocations",
            "workspaces",
            "workspace_paths",
            "legacy_imports",
            "legacy_import_id_mappings",
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
    assert_eq!(
        backup_status.pending_versions,
        vec![2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
    );
    Ok(())
}

#[test]
fn v14_plan_only_requester_is_backfilled_into_authoritative_invocation_fence()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = std::fs::canonicalize(directory.path())?;
    let database_path = root.join("state.db");
    let backup_directory = root.join("backups");
    let database = Database::open(&database_path)?;
    database.migrate_with_backup(&backup_directory)?;
    let workspace_root = root.join("workspace");
    std::fs::create_dir_all(&workspace_root)?;
    let workspace_id = database
        .resolve_repository_workspace(&workspace_root)?
        .workspace_id;
    let command_id = ClientCommandId::new();
    with_database_connection(&database, |connection| {
        restore_v15_conversation_attempts_schema(connection)?;
        connection.execute_batch(
            "DROP TABLE client_command_invocations;
             DELETE FROM schema_migrations WHERE version = 15;
             PRAGMA user_version = 14;",
        )?;
        connection.execute(
            "INSERT INTO client_commands(
                workspace_id, command_id, session_id, task_id, action, payload_json,
                idempotency_key, state, requested_by, requested_at)
             VALUES (?1, ?2, NULL, NULL, 'stop_daemon', '{}', ?3, 'pending',
                     'local-cli-run-plan-only', ?4)",
            params![
                workspace_id.to_string(),
                command_id.to_string(),
                format!("plan-only-{command_id}"),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    })?;

    database.migrate_with_backup(&backup_directory)?;

    with_database_connection(&database, |connection| {
        let (root_command_id, plan_only): (String, bool) = connection.query_row(
            "SELECT root_command_id, plan_only FROM client_command_invocations
             WHERE command_id = ?1",
            [command_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(root_command_id, command_id.to_string());
        assert!(plan_only);
        Ok(())
    })?;
    Ok(())
}

#[test]
fn v15_conversation_attempts_gain_durable_failure_outcomes_without_losing_rows()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = std::fs::canonicalize(directory.path())?;
    let database_path = root.join("state.db");
    let backup_directory = root.join("backups");
    let database = Database::open(&database_path)?;
    database.migrate_with_backup(&backup_directory)?;
    let workspace_root = root.join("workspace");
    std::fs::create_dir_all(&workspace_root)?;
    let workspace_id = database
        .resolve_repository_workspace(&workspace_root)?
        .workspace_id;
    let session_id = uuid::Uuid::now_v7().to_string();
    let message_id = uuid::Uuid::now_v7().to_string();
    let running_id = uuid::Uuid::now_v7().to_string();
    let succeeded_id = uuid::Uuid::now_v7().to_string();
    let failed_id = uuid::Uuid::now_v7().to_string();
    let null_error_id = uuid::Uuid::now_v7().to_string();
    let blank_error_id = uuid::Uuid::now_v7().to_string();
    let whitespace_error_id = uuid::Uuid::now_v7().to_string();
    let oversized_ascii_id = uuid::Uuid::now_v7().to_string();
    let oversized_unicode_id = uuid::Uuid::now_v7().to_string();
    let oversized_ascii = "x".repeat(20 * 1024);
    let oversized_unicode = "한".repeat(6 * 1024);
    let now = Utc::now().to_rfc3339();
    with_database_connection(&database, |connection| {
        restore_v15_conversation_attempts_schema(connection)?;
        connection.execute(
            "INSERT INTO sessions(
                workspace_id, session_id, schema_version, revision, title, state,
                created_at, updated_at)
             VALUES (?1, ?2, '1.0', 0, 'v15 migration', 'drafting', ?3, ?3)",
            params![workspace_id.to_string(), session_id, now],
        )?;
        connection.execute(
            "INSERT INTO conversation_messages(
                workspace_id, message_id, session_id, ordinal, role, kind, state,
                content_redacted, created_at, finalized_at)
             VALUES (?1, ?2, ?3, 1, 'user', 'user_message', 'final',
                     'migration source', ?4, ?4)",
            params![workspace_id.to_string(), message_id, session_id, now],
        )?;
        connection.execute(
            "INSERT INTO conversation_attempts(
                workspace_id, attempt_id, session_id, source_message_id, provider_id,
                status, started_at)
             VALUES (?1, ?2, ?3, ?4, 'codex', 'running', ?5)",
            params![
                workspace_id.to_string(),
                running_id,
                session_id,
                message_id,
                now
            ],
        )?;
        connection.execute(
            "INSERT INTO conversation_attempts(
                workspace_id, attempt_id, session_id, source_message_id, provider_id,
                status, outcome_json, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, 'codex', 'succeeded', ?5, ?6, ?6)",
            params![
                workspace_id.to_string(),
                succeeded_id,
                session_id,
                message_id,
                r#"{"outcome":"answer_complete","response_redacted":"preserved"}"#,
                now,
            ],
        )?;
        connection.execute(
            "INSERT INTO conversation_attempts(
                workspace_id, attempt_id, session_id, source_message_id, provider_id,
                status, error_redacted, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, 'codex', 'failed', 'legacy failure', ?5, ?5)",
            params![
                workspace_id.to_string(),
                failed_id,
                session_id,
                message_id,
                now
            ],
        )?;
        connection.execute_batch("PRAGMA ignore_check_constraints = ON;")?;
        connection.execute(
            "INSERT INTO conversation_attempts(
                workspace_id, attempt_id, session_id, source_message_id, provider_id,
                status, error_redacted, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, 'codex', 'failed', NULL, ?5, ?5)",
            params![
                workspace_id.to_string(),
                null_error_id,
                session_id,
                message_id,
                now
            ],
        )?;
        connection.execute_batch("PRAGMA ignore_check_constraints = OFF;")?;
        for (attempt_id, error_redacted) in [(&blank_error_id, ""), (&whitespace_error_id, " \t ")]
        {
            connection.execute(
                "INSERT INTO conversation_attempts(
                    workspace_id, attempt_id, session_id, source_message_id, provider_id,
                    status, error_redacted, started_at, completed_at)
                 VALUES (?1, ?2, ?3, ?4, 'codex', 'cancelled', ?5, ?6, ?6)",
                params![
                    workspace_id.to_string(),
                    attempt_id,
                    session_id,
                    message_id,
                    error_redacted,
                    now,
                ],
            )?;
        }
        for (attempt_id, error_redacted) in [
            (&oversized_ascii_id, &oversized_ascii),
            (&oversized_unicode_id, &oversized_unicode),
        ] {
            connection.execute(
                "INSERT INTO conversation_attempts(
                    workspace_id, attempt_id, session_id, source_message_id, provider_id,
                    status, error_redacted, started_at, completed_at)
                 VALUES (?1, ?2, ?3, ?4, 'codex', 'failed', ?5, ?6, ?6)",
                params![
                    workspace_id.to_string(),
                    attempt_id,
                    session_id,
                    message_id,
                    error_redacted,
                    now,
                ],
            )?;
        }
        Ok(())
    })?;

    // This fixture deliberately represents a legacy row that violates the v15 CHECK.
    // Apply the registered migration directly so backup integrity validation does not
    // reject the recovery input before v16 can normalize it.
    drop(database);
    let mut connection = Connection::open(&database_path)?;
    let migrated = MigrationManager::apply(&mut connection)?;
    drop(connection);
    let database = Database::open(&database_path)?;
    assert_eq!(migrated.current_version, 16);
    with_database_connection(&database, |connection| {
        let running: (String, Option<String>, Option<String>, Option<String>) = connection
            .query_row(
                "SELECT status, outcome_json, error_redacted, completed_at
                 FROM conversation_attempts WHERE workspace_id = ?1 AND attempt_id = ?2",
                params![workspace_id.to_string(), running_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(running, ("running".to_owned(), None, None, None));

        let succeeded: (String, String, Option<String>) = connection.query_row(
            "SELECT status, outcome_json, error_redacted FROM conversation_attempts
             WHERE workspace_id = ?1 AND attempt_id = ?2",
            params![workspace_id.to_string(), succeeded_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(succeeded.0, "succeeded");
        assert_eq!(
            succeeded.1,
            r#"{"outcome":"answer_complete","response_redacted":"preserved"}"#
        );
        assert_eq!(succeeded.2, None);

        let failed: (String, String, String) = connection.query_row(
            "SELECT status, outcome_json, error_redacted FROM conversation_attempts
             WHERE workspace_id = ?1 AND attempt_id = ?2",
            params![workspace_id.to_string(), failed_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(failed.0, "failed");
        assert_eq!(
            failed.2,
            "Previous conversation failed. Review this redacted evidence and retry: legacy failure"
        );
        let failed_outcome: serde_json::Value = serde_json::from_str(&failed.1)?;
        assert_eq!(failed_outcome["outcome"], "needs_attention");
        assert_eq!(failed_outcome["evidence_redacted"], failed.2);

        for (attempt_id, expected_status) in [
            (&null_error_id, "failed"),
            (&blank_error_id, "cancelled"),
            (&whitespace_error_id, "cancelled"),
        ] {
            let (status, outcome_json, error_redacted, completed_at): (
                String,
                String,
                String,
                String,
            ) = connection.query_row(
                "SELECT status, outcome_json, error_redacted, completed_at
                 FROM conversation_attempts WHERE workspace_id = ?1 AND attempt_id = ?2",
                params![workspace_id.to_string(), attempt_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            assert_eq!(status, expected_status);
            assert_eq!(completed_at, now);
            assert_eq!(
                error_redacted,
                "Previous conversation failed. Review provider configuration and retry."
            );
            let outcome: serde_json::Value = serde_json::from_str(&outcome_json)?;
            assert_eq!(outcome["outcome"], "needs_attention");
            assert_eq!(outcome["evidence_redacted"], error_redacted);
        }

        for (attempt_id, original_error) in [
            (&oversized_ascii_id, &oversized_ascii),
            (&oversized_unicode_id, &oversized_unicode),
        ] {
            let (outcome_json, error_redacted): (String, String) = connection.query_row(
                "SELECT outcome_json, error_redacted FROM conversation_attempts
                 WHERE workspace_id = ?1 AND attempt_id = ?2",
                params![workspace_id.to_string(), attempt_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let outcome: serde_json::Value = serde_json::from_str(&outcome_json)?;
            let evidence = outcome["evidence_redacted"].as_str().ok_or_else(|| {
                StateError::InvalidRecord("legacy recovery evidence is not a string".to_owned())
            })?;
            assert!(evidence.len() <= 16 * 1024);
            assert!(evidence.ends_with("[truncated]"));
            assert!(evidence.starts_with(
                "Previous conversation failed. Review this redacted evidence and retry: "
            ));
            assert_eq!(error_redacted, evidence);
            assert_ne!(&error_redacted, original_error);
        }

        for status in ["failed", "cancelled"] {
            connection.execute(
                "INSERT INTO conversation_attempts(
                    workspace_id, attempt_id, session_id, source_message_id, provider_id,
                    status, outcome_json, error_redacted, started_at, completed_at)
                 VALUES (?1, ?2, ?3, ?4, 'codex', ?5, ?6, 'new failure', ?7, ?7)",
                params![
                    workspace_id.to_string(),
                    uuid::Uuid::now_v7().to_string(),
                    session_id,
                    message_id,
                    status,
                    r#"{"outcome":"needs_attention","response_redacted":"retry","evidence_redacted":"new failure"}"#,
                    now,
                ],
            )?;
        }

        let invalid_rows = [
            ("running", Some("{}"), None, None),
            ("succeeded", Some("{}"), Some("error"), Some(now.as_str())),
            ("failed", None, Some("error"), Some(now.as_str())),
            ("failed", Some("{}"), Some("error"), Some(now.as_str())),
            (
                "failed",
                Some(r#"{"outcome":"answer_complete","response_redacted":"wrong"}"#),
                Some("error"),
                Some(now.as_str()),
            ),
            ("cancelled", Some("{}"), Some("error"), Some(now.as_str())),
            ("cancelled", Some("{}"), None, Some(now.as_str())),
        ];
        for (status, outcome, error, completed) in invalid_rows {
            assert!(
                connection
                    .execute(
                        "INSERT INTO conversation_attempts(
                            workspace_id, attempt_id, session_id, source_message_id,
                            provider_id, status, outcome_json, error_redacted, started_at,
                            completed_at)
                         VALUES (?1, ?2, ?3, ?4, 'codex', ?5, ?6, ?7, ?8, ?9)",
                        params![
                            workspace_id.to_string(),
                            uuid::Uuid::now_v7().to_string(),
                            session_id,
                            message_id,
                            status,
                            outcome,
                            error,
                            now,
                            completed,
                        ],
                    )
                    .is_err(),
                "invalid terminal combination for {status} was accepted"
            );
        }
        let invalid_failure_outcomes = [
            r#"{"outcome":"needs_attention"}"#,
            r#"{"outcome":"needs_attention","response_redacted":null,"evidence_redacted":"evidence"}"#,
            r#"{"outcome":"needs_attention","response_redacted":"retry","evidence_redacted":null}"#,
            r#"{"outcome":"needs_attention","response_redacted":17,"evidence_redacted":"evidence"}"#,
            r#"{"outcome":"needs_attention","response_redacted":"retry","evidence_redacted":17}"#,
            r#"{"outcome":"needs_attention","response_redacted":"  ","evidence_redacted":"evidence"}"#,
            r#"{"outcome":"needs_attention","response_redacted":"\u2003","evidence_redacted":"evidence"}"#,
            r#"{"outcome":"needs_attention","response_redacted":"retry","evidence_redacted":" \t "}"#,
            r#"{"outcome":"needs_attention","response_redacted":"retry","evidence_redacted":"\u2003"}"#,
            &format!(
                r#"{{"outcome":"needs_attention","response_redacted":"retry","evidence_redacted":"{}"}}"#,
                "x".repeat(16 * 1024 + 1)
            ),
            "{not-json}",
        ];
        for status in ["failed", "cancelled"] {
            for outcome_json in invalid_failure_outcomes {
                assert!(
                    connection
                        .execute(
                            "INSERT INTO conversation_attempts(
                                workspace_id, attempt_id, session_id, source_message_id,
                                provider_id, status, outcome_json, error_redacted, started_at,
                                completed_at)
                             VALUES (?1, ?2, ?3, ?4, 'codex', ?5, ?6,
                                     'actionable failure', ?7, ?7)",
                            params![
                                workspace_id.to_string(),
                                uuid::Uuid::now_v7().to_string(),
                                session_id,
                                message_id,
                                status,
                                outcome_json,
                                now,
                            ],
                        )
                        .is_err(),
                    "invalid {status} outcome was accepted: {outcome_json}"
                );
            }
            for error_redacted in ["", " \t ", "\u{2003}", &"x".repeat(16 * 1024 + 1)] {
                assert!(
                    connection
                        .execute(
                            "INSERT INTO conversation_attempts(
                                workspace_id, attempt_id, session_id, source_message_id,
                                provider_id, status, outcome_json, error_redacted, started_at,
                                completed_at)
                             VALUES (?1, ?2, ?3, ?4, 'codex', ?5,
                                     '{\"outcome\":\"needs_attention\",\"response_redacted\":\"retry\",\"evidence_redacted\":\"actionable failure\"}',
                                     ?6, ?7, ?7)",
                            params![
                                workspace_id.to_string(),
                                uuid::Uuid::now_v7().to_string(),
                                session_id,
                                message_id,
                                status,
                                error_redacted,
                                now,
                            ],
                        )
                        .is_err(),
                    "invalid {status} error was accepted"
                );
            }
        }
        assert!(
            connection
                .execute(
                    "INSERT INTO conversation_attempts(
                        workspace_id, attempt_id, session_id, source_message_id, provider_id,
                        status, started_at)
                     VALUES (?1, ?2, 'missing-session', ?3, 'codex', 'running', ?4)",
                    params![
                        workspace_id.to_string(),
                        uuid::Uuid::now_v7().to_string(),
                        message_id,
                        now,
                    ],
                )
                .is_err()
        );
        let trigger_count: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'trigger'
             AND name IN ('conversation_attempts_immutable_identity',
                          'conversation_attempts_single_completion',
                          'conversation_attempts_no_delete')",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(trigger_count, 3);
        let index_count: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index'
             AND name = 'conversation_attempts_session_time'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(index_count, 1);
        let integrity: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        assert_eq!(integrity, "ok");
        let foreign_key_failures: i64 =
            connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        assert_eq!(foreign_key_failures, 0);
        Ok(())
    })?;
    Ok(())
}

fn restore_v15_conversation_attempts_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "PRAGMA defer_foreign_keys = ON;
         DROP TRIGGER conversation_attempts_immutable_identity;
         DROP TRIGGER conversation_attempts_single_completion;
         DROP TRIGGER conversation_attempts_no_delete;
         DROP INDEX conversation_attempts_session_time;
         ALTER TABLE conversation_attempts RENAME TO conversation_attempts_after_v15;
         CREATE TABLE conversation_attempts (
             workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
             attempt_id TEXT NOT NULL,
             session_id TEXT NOT NULL,
             source_message_id TEXT NOT NULL,
             provider_id TEXT NOT NULL CHECK (provider_id IN ('gemini', 'codex', 'claude', 'agy')),
             status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed', 'cancelled')),
             outcome_json TEXT CHECK (outcome_json IS NULL OR json_valid(outcome_json)),
             error_redacted TEXT,
             started_at TEXT NOT NULL,
             completed_at TEXT,
             PRIMARY KEY(workspace_id, attempt_id),
             FOREIGN KEY(workspace_id, session_id)
                 REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
             FOREIGN KEY(workspace_id, source_message_id)
                 REFERENCES conversation_messages(workspace_id, message_id) ON DELETE RESTRICT,
             CHECK (
                 (status = 'running' AND outcome_json IS NULL AND error_redacted IS NULL AND completed_at IS NULL)
                 OR (status = 'succeeded' AND outcome_json IS NOT NULL AND error_redacted IS NULL AND completed_at IS NOT NULL)
                 OR (status IN ('failed', 'cancelled') AND outcome_json IS NULL AND error_redacted IS NOT NULL AND completed_at IS NOT NULL)
             )
         ) STRICT;
         INSERT INTO conversation_attempts
         SELECT * FROM conversation_attempts_after_v15;
         DROP TABLE conversation_attempts_after_v15;
         CREATE INDEX conversation_attempts_session_time
             ON conversation_attempts(workspace_id, session_id, started_at DESC);
         CREATE TRIGGER conversation_attempts_immutable_identity
         BEFORE UPDATE OF workspace_id, attempt_id, session_id, source_message_id,
                          provider_id, started_at ON conversation_attempts
         BEGIN SELECT RAISE(ABORT, 'conversation attempt identity is immutable'); END;
         CREATE TRIGGER conversation_attempts_single_completion
         BEFORE UPDATE OF status, outcome_json, error_redacted, completed_at
         ON conversation_attempts
         WHEN OLD.status <> 'running' OR NEW.status = 'running' OR NEW.completed_at IS NULL
         BEGIN SELECT RAISE(ABORT, 'conversation attempt completion is append-once'); END;
         CREATE TRIGGER conversation_attempts_no_delete BEFORE DELETE ON conversation_attempts
         BEGIN SELECT RAISE(ABORT, 'conversation attempts are append-only'); END;
         DELETE FROM schema_migrations WHERE version = 16;
         PRAGMA user_version = 15;",
    )?;
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

fn seed_v5(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    seed_v3(path)?;
    let connection = Connection::open(path)?;
    connection.execute_batch("PRAGMA foreign_keys = ON;")?;
    for (version, name, sql) in [
        (4, "durable_sessions", SESSION_MIGRATION),
        (5, "chat_workspace_state", WORKSPACE_MIGRATION),
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
    connection.execute(
        "INSERT INTO client_commands(
            command_id, action, payload_json, idempotency_key, state,
            requested_by, requested_at, outcome
         ) VALUES (?1, 'stop_daemon', '{}', 'preserved-v5-command', 'completed',
                   'migration-test', ?2, 'stopped')",
        params![uuid::Uuid::now_v7().to_string(), Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn seed_v8(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    seed_v5(path)?;
    let connection = Connection::open(path)?;
    connection.execute_batch("PRAGMA foreign_keys = ON;")?;
    for (version, name, sql) in [
        (6, "approved_task_graphs", GRAPH_MIGRATION),
        (7, "parallel_execution", PARALLEL_MIGRATION),
        (8, "result_integration", INTEGRATION_MIGRATION),
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
fn v8_daemon_rows_migrate_to_online_phase() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = std::fs::canonicalize(directory.path())?;
    let database_path = root.join("orchestrator.db");
    seed_v8(&database_path)?;
    let now = Utc::now();
    let expires = now + chrono::TimeDelta::minutes(1);
    Connection::open(&database_path)?.execute(
        "INSERT INTO daemon_instances( \
            instance_id, pid, started_at, heartbeat_at, lease_expires_at, \
            stop_requested_at, released_at \
         ) VALUES ('legacy-daemon', 42, ?1, ?1, ?2, NULL, NULL)",
        params![now.to_rfc3339(), expires.to_rfc3339()],
    )?;

    let database = Database::open(&database_path)?;
    database.migrate_with_backup(&root.join("backups"))?;
    with_database_connection(&database, |connection| {
        let migrated: (String, Option<String>) = connection.query_row(
            "SELECT phase, startup_error FROM daemon_instances WHERE instance_id = ?1",
            ["legacy-daemon"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(migrated, ("online".to_owned(), None));
        Ok(())
    })?;
    Ok(())
}

#[test]
fn v5_to_current_dry_run_backup_and_command_rebuild_preserve_rows()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = std::fs::canonicalize(directory.path())?;
    let database_path = root.join("orchestrator.db");
    seed_v5(&database_path)?;
    let database = Database::open(&database_path)?;
    assert_eq!(database.migration_status()?.current_version, 5);

    assert_eq!(
        database.dry_run_migrations()?.current_version,
        STATE_SCHEMA_VERSION
    );
    assert_eq!(database.migration_status()?.current_version, 5);
    database.migrate_with_backup(&root.join("v5-backups"))?;

    with_database_connection(&database, |connection| {
        let preserved: (String, String) = connection.query_row(
            "SELECT action, outcome FROM main.client_commands WHERE idempotency_key = ?1",
            ["preserved-v5-command"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(preserved, ("stop_daemon".to_owned(), "stopped".to_owned()));
        connection.execute(
            "INSERT INTO main.client_commands(
                workspace_id, command_id, action, payload_json, idempotency_key, state,
                requested_by, requested_at
             ) VALUES (?1, ?2, 'request_plan', '{}', 'new-v6-command', 'pending',
                       'migration-test', ?3)",
            params![
                "00000000-0000-0000-0000-000000000001",
                uuid::Uuid::now_v7().to_string(),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    })?;
    let backups = std::fs::read_dir(root.join("v5-backups"))?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(backups.len(), 1);
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
    let mut historical = TaskEvent {
        schema_version: SchemaVersion::new(SchemaVersion::V3),
        sequence: 1,
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
    };
    historical.refresh_event_hash()?;
    let event_type = serde_json::to_value(historical.event_type)?
        .as_str()
        .ok_or("event type is not a string")?
        .to_owned();
    with_database_connection(&database, |connection| {
        connection.execute(
            "INSERT INTO main.task_events( \
                sequence, event_id, task_id, event_type, schema_version, occurred_at, event_json, \
                previous_hash, event_hash, exported_at \
             ) VALUES (1, ?1, NULL, ?2, ?3, ?4, ?5, NULL, ?6, NULL)",
            params![
                historical.event_id.to_string(),
                event_type,
                historical.schema_version.as_str(),
                historical.occurred_at.to_rfc3339(),
                serde_json::to_string(&historical)?,
                historical.event_hash,
            ],
        )?;
        Ok(())
    })?;
    assert!(historical.verify_hash()?);

    let migrated = database.migrate_with_backup(&root.join("backups-v4"))?;
    assert_eq!(migrated.current_version, STATE_SCHEMA_VERSION);
    let reserved_workspace: WorkspaceId = "00000000-0000-0000-0000-000000000001".parse()?;
    let reloaded = database
        .workspace(reserved_workspace)
        .event_at(1)?
        .ok_or("historical event missing")?;
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
    with_database_connection(&first_database, |connection| {
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
    with_database_connection(&second_database, |connection| {
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
