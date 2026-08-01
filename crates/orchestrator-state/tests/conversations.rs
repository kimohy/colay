use std::path::Path;

use chrono::Utc;
use orchestrator_domain::{
    ConversationAttemptId, ConversationOutcome, MessageId, ProviderId, RequirementRevision,
    RequirementRevisionId, RequirementSnapshot, SessionId, VerificationCommand,
};
use orchestrator_state::{
    ConversationAttemptStatus, Database, NewConversationAttempt, WorkspaceDatabase, WorkspaceId,
};
use rusqlite::params;

mod support;
use support::{fresh_database, with_workspace_connection};

fn seed_session_message(
    database_path: &Path,
    database: &WorkspaceDatabase<'_>,
) -> Result<(SessionId, MessageId), Box<dyn std::error::Error>> {
    let session_id = SessionId::new();
    let message_id = MessageId::new();
    let now = Utc::now().to_rfc3339();
    with_workspace_connection(database_path, database, |connection| {
        connection.execute(
            "INSERT INTO main.sessions(session_id, schema_version, revision, title, state, created_at, updated_at)
             VALUES (?1, '1.0', 0, 'conversation test', 'drafting', ?2, ?2)",
            params![session_id.to_string(), now],
        )?;
        connection.execute(
            "INSERT INTO main.conversation_messages(
                message_id, session_id, ordinal, role, kind, state, content_redacted,
                created_at, finalized_at)
             VALUES (?1, ?2, 1, 'user', 'user_message', 'final', 'fix the issue', ?3, ?3)",
            params![message_id.to_string(), session_id.to_string(), now],
        )?;
        Ok(())
    })?;
    Ok((session_id, message_id))
}

fn database() -> Result<(Database, WorkspaceId), Box<dyn std::error::Error>> {
    fresh_database()
}

fn ready_snapshot() -> RequirementSnapshot {
    RequirementSnapshot {
        objective: "fix the conversation flow".to_owned(),
        in_scope: vec!["conversation flow".to_owned()],
        out_of_scope: vec!["automatic merge".to_owned()],
        constraints: vec!["no task before approval".to_owned()],
        acceptance_criteria: vec!["ordinary answers create zero tasks".to_owned()],
        verification_plan: vec![VerificationCommand {
            executable: "cargo".to_owned(),
            args: vec![
                "test".to_owned(),
                "--workspace".to_owned(),
                "--all-features".to_owned(),
            ],
        }],
        risks: vec!["stale approval".to_owned()],
        open_questions: Vec::new(),
    }
}

#[test]
fn attempts_and_requirement_revisions_are_immutable_and_session_scoped()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id) = database()?;
    let database_path = database.path().to_path_buf();
    let database = database.workspace(workspace_id);
    let (session_id, message_id) = seed_session_message(&database_path, &database)?;
    let attempt_id = ConversationAttemptId::new();
    database.begin_conversation_attempt(&NewConversationAttempt {
        attempt_id,
        session_id,
        source_message_id: message_id,
        provider: ProviderId::Codex,
        started_at: Utc::now(),
    })?;
    let outcome = ConversationOutcome::WorktreeTaskCandidate {
        response_redacted: "ready for deterministic validation".to_owned(),
        requirements: ready_snapshot(),
    };
    let completed = database.finish_conversation_attempt(attempt_id, &outcome, Utc::now())?;
    assert_eq!(completed.outcome, Some(outcome));

    let revision = RequirementRevision::seal(
        RequirementRevisionId::new(),
        session_id,
        message_id,
        1,
        ready_snapshot(),
        Utc::now(),
    )?;
    database.record_requirement_revision(&revision)?;
    assert_eq!(
        database.current_requirement_revision(session_id)?,
        Some(revision.clone())
    );
    database.record_requirement_revision(&revision)?;

    with_workspace_connection(&database_path, &database, |connection| {
        assert!(
            connection
                .execute(
                    "UPDATE main.requirement_revisions SET snapshot_hash = ?1 WHERE requirement_revision_id = ?2",
                    params!["0".repeat(64), revision.requirement_revision_id.to_string()],
                )
                .is_err()
        );
        for table in [
            "tasks",
            "task_attempts",
            "worktrees",
            "coordinator_leases",
            "worker_leases",
        ] {
            let count: i64 =
                connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
            assert_eq!(count, 0, "pre-approval row in {table}");
        }
        Ok(())
    })?;
    Ok(())
}

fn begin_attempt(
    database_path: &Path,
    database: &WorkspaceDatabase<'_>,
) -> Result<ConversationAttemptId, Box<dyn std::error::Error>> {
    let (session_id, message_id) = seed_session_message(database_path, database)?;
    let attempt_id = ConversationAttemptId::new();
    database.begin_conversation_attempt(&NewConversationAttempt {
        attempt_id,
        session_id,
        source_message_id: message_id,
        provider: ProviderId::Codex,
        started_at: Utc::now(),
    })?;
    Ok(attempt_id)
}

fn needs_attention(evidence: &str) -> ConversationOutcome {
    ConversationOutcome::NeedsAttention {
        response_redacted: "Reconnect the provider and retry.".to_owned(),
        evidence_redacted: evidence.to_owned(),
    }
}

#[test]
fn failed_and_cancelled_attempts_persist_the_recovery_outcome()
-> Result<(), Box<dyn std::error::Error>> {
    for (status, error) in [
        (ConversationAttemptStatus::Failed, "authentication failed"),
        (ConversationAttemptStatus::Cancelled, "request cancelled"),
    ] {
        let (database, workspace_id) = database()?;
        let database_path = database.path().to_path_buf();
        let database = database.workspace(workspace_id);
        let attempt_id = begin_attempt(&database_path, &database)?;
        let outcome = needs_attention(error);
        let completed = database.finalize_conversation_failure(
            attempt_id,
            status,
            &outcome,
            error,
            Utc::now(),
        )?;

        assert_eq!(completed.status, status);
        assert_eq!(completed.outcome, Some(outcome));
        assert_eq!(completed.error_redacted.as_deref(), Some(error));
        assert!(completed.completed_at.is_some());
    }
    Ok(())
}

#[test]
fn failure_finalization_replays_exactly_and_rejects_conflicts()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id) = database()?;
    let database_path = database.path().to_path_buf();
    let database = database.workspace(workspace_id);
    let attempt_id = begin_attempt(&database_path, &database)?;
    let outcome = needs_attention("authentication failed");
    let completed_at = Utc::now();
    let completed = database.finalize_conversation_failure(
        attempt_id,
        ConversationAttemptStatus::Failed,
        &outcome,
        "authentication failed",
        completed_at,
    )?;

    assert_eq!(
        database.finalize_conversation_failure(
            attempt_id,
            ConversationAttemptStatus::Failed,
            &outcome,
            "authentication failed",
            completed_at,
        )?,
        completed
    );
    assert!(
        database
            .finalize_conversation_failure(
                attempt_id,
                ConversationAttemptStatus::Failed,
                &outcome,
                "different redacted evidence",
                completed_at,
            )
            .is_err()
    );
    assert!(
        database
            .finalize_conversation_failure(
                attempt_id,
                ConversationAttemptStatus::Cancelled,
                &outcome,
                "authentication failed",
                completed_at,
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn failure_finalization_rejects_invalid_statuses_and_errors()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id) = database()?;
    let database_path = database.path().to_path_buf();
    let database = database.workspace(workspace_id);
    let outcome = needs_attention("provider failed");
    for status in [
        ConversationAttemptStatus::Running,
        ConversationAttemptStatus::Succeeded,
    ] {
        let attempt_id = begin_attempt(&database_path, &database)?;
        assert!(
            database
                .finalize_conversation_failure(
                    attempt_id,
                    status,
                    &outcome,
                    "provider failed",
                    Utc::now(),
                )
                .is_err()
        );
    }
    for error in ["   ".to_owned(), "x".repeat(16 * 1024 + 1)] {
        let attempt_id = begin_attempt(&database_path, &database)?;
        assert!(
            database
                .finalize_conversation_failure(
                    attempt_id,
                    ConversationAttemptStatus::Failed,
                    &outcome,
                    &error,
                    Utc::now(),
                )
                .is_err()
        );
    }

    let attempt_id = begin_attempt(&database_path, &database)?;
    assert!(
        database
            .finalize_conversation_failure(
                attempt_id,
                ConversationAttemptStatus::Failed,
                &ConversationOutcome::AnswerComplete {
                    response_redacted: "this is not a recovery outcome".to_owned(),
                },
                "provider failed",
                Utc::now(),
            )
            .is_err()
    );

    let succeeded_attempt = begin_attempt(&database_path, &database)?;
    database.finish_conversation_attempt(
        succeeded_attempt,
        &ConversationOutcome::AnswerComplete {
            response_redacted: "completed".to_owned(),
        },
        Utc::now(),
    )?;
    assert!(
        database
            .finalize_conversation_failure(
                succeeded_attempt,
                ConversationAttemptStatus::Failed,
                &outcome,
                "provider failed",
                Utc::now(),
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn interrupted_conversation_attempt_and_claimed_command_are_finalized_together()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, workspace_id) = database()?;
    let database_path = database.path().to_path_buf();
    let database = database.workspace(workspace_id);
    let (session_id, message_id) = seed_session_message(&database_path, &database)?;
    let attempt_id = ConversationAttemptId::new();
    let started_at = Utc::now();
    database.begin_conversation_attempt(&NewConversationAttempt {
        attempt_id,
        session_id,
        source_message_id: message_id,
        provider: ProviderId::Codex,
        started_at,
    })?;
    with_workspace_connection(&database_path, &database, |connection| {
        connection.execute(
            "INSERT INTO main.client_commands(
                command_id, session_id, action, payload_json, idempotency_key, state,
                requested_by, requested_at, claimed_at)
             VALUES (?1, ?2, 'request_conversation_turn', ?3, ?4, 'claimed',
                     'test', ?5, ?5)",
            params![
                attempt_id.to_string(),
                session_id.to_string(),
                serde_json::json!({"source_message_id": message_id}).to_string(),
                format!("interrupted-{attempt_id}"),
                started_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    })?;

    let recovered = database
        .reconcile_interrupted_conversation_attempts(Utc::now(), "interrupted by daemon restart")?;

    assert_eq!(recovered, vec![attempt_id]);
    let attempt = database
        .load_conversation_attempt(attempt_id)?
        .ok_or("missing attempt")?;
    assert_eq!(
        attempt.status,
        orchestrator_state::ConversationAttemptStatus::Failed
    );
    assert_eq!(
        attempt.error_redacted.as_deref(),
        Some("interrupted by daemon restart")
    );
    assert_eq!(
        attempt.outcome,
        Some(ConversationOutcome::NeedsAttention {
            response_redacted:
                "The conversation was interrupted by a daemon restart. Retry this conversation."
                    .to_owned(),
            evidence_redacted: "interrupted by daemon restart".to_owned(),
        })
    );
    with_workspace_connection(&database_path, &database, |connection| {
        let (state, outcome): (String, String) = connection.query_row(
            "SELECT state, outcome FROM client_commands WHERE command_id = ?1",
            [attempt_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(state, "failed");
        assert_eq!(outcome, "interrupted by daemon restart");
        Ok(())
    })?;
    Ok(())
}
