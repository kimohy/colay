use chrono::Utc;
use orchestrator_domain::{
    ConversationAttemptId, ConversationOutcome, MessageId, ProviderId, RequirementRevision,
    RequirementRevisionId, RequirementSnapshot, SessionId,
};
use orchestrator_state::{Database, NewConversationAttempt};
use rusqlite::params;

fn seed_session_message(
    database: &Database,
) -> Result<(SessionId, MessageId), Box<dyn std::error::Error>> {
    let session_id = SessionId::new();
    let message_id = MessageId::new();
    let now = Utc::now().to_rfc3339();
    database.with_connection(|connection| {
        connection.execute(
            "INSERT INTO sessions(session_id, schema_version, revision, title, state, created_at, updated_at)
             VALUES (?1, '1.0', 0, 'conversation test', 'drafting', ?2, ?2)",
            params![session_id.to_string(), now],
        )?;
        connection.execute(
            "INSERT INTO conversation_messages(
                message_id, session_id, ordinal, role, kind, state, content_redacted,
                created_at, finalized_at)
             VALUES (?1, ?2, 1, 'user', 'user_message', 'final', 'fix the issue', ?3, ?3)",
            params![message_id.to_string(), session_id.to_string(), now],
        )?;
        Ok(())
    })?;
    Ok((session_id, message_id))
}

fn ready_snapshot() -> RequirementSnapshot {
    RequirementSnapshot {
        objective: "fix the conversation flow".to_owned(),
        constraints: vec!["no task before approval".to_owned()],
        acceptance_criteria: vec!["ordinary answers create zero tasks".to_owned()],
        verification_plan: vec!["cargo test --workspace --all-features".to_owned()],
        open_questions: Vec::new(),
    }
}

#[test]
fn attempts_and_requirement_revisions_are_immutable_and_session_scoped()
-> Result<(), Box<dyn std::error::Error>> {
    let database = Database::open_in_memory()?;
    database.migrate_with_backup(std::path::Path::new("unused"))?;
    let (session_id, message_id) = seed_session_message(&database)?;
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

    database.with_connection(|connection| {
        assert!(
            connection
                .execute(
                    "UPDATE requirement_revisions SET snapshot_hash = ?1 WHERE requirement_revision_id = ?2",
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
            let count: i64 = connection.query_row(
                &format!("SELECT count(*) FROM {table}"),
                [],
                |row| row.get(0),
            )?;
            assert_eq!(count, 0, "pre-approval row in {table}");
        }
        Ok(())
    })?;
    Ok(())
}
