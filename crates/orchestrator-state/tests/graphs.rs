#![allow(clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeZone, Utc};
use orchestrator_domain::{
    GraphRevisionId, GraphValidationPolicy, MessageId, ModelProfile, PlanningAttemptId, ProviderId,
    RepoPath, SchemaVersion, SessionId, TaskGraphNode, TaskGraphProposal, validate_task_graph,
};
use orchestrator_state::{
    ApprovedGraph, Database, GraphApprovalRequest, GraphRevisionStatus, NewGraphAttempt,
};
use rusqlite::params;

fn timestamp(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 21, 1, 0, second)
        .single()
        .unwrap_or_else(Utc::now)
}

fn database() -> Result<Database, Box<dyn std::error::Error>> {
    let database = Database::open_in_memory()?;
    database.migrate_with_backup(std::path::Path::new("unused"))?;
    Ok(database)
}

fn seed_session(
    database: &Database,
    session_id: SessionId,
    goal_id: MessageId,
) -> Result<(), Box<dyn std::error::Error>> {
    database.with_connection(|connection| {
        connection.execute(
            "INSERT INTO sessions(session_id, schema_version, revision, title, state, created_at, updated_at) VALUES (?1, '1', 0, 'graph test', 'awaiting_approval', ?2, ?2)",
            params![session_id.to_string(), timestamp(0).to_rfc3339()],
        )?;
        connection.execute(
            "INSERT INTO conversation_messages(message_id, session_id, ordinal, role, kind, state, content_redacted, created_at, finalized_at) VALUES (?1, ?2, 1, 'user', 'user_message', 'final', 'build the graph', ?3, ?3)",
            params![goal_id.to_string(), session_id.to_string(), timestamp(0).to_rfc3339()],
        )?;
        Ok(())
    })?;
    Ok(())
}

fn validated_graph(
    session_id: SessionId,
    goal_id: MessageId,
) -> orchestrator_domain::ValidatedTaskGraph {
    let node = |key: &str, dependencies: &[&str], scope: &str| TaskGraphNode {
        key: key.to_owned(),
        title: format!("{key} title"),
        objective: format!("implement {key}"),
        dependencies: dependencies
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        constraints: vec!["local only".to_owned()],
        acceptance_criteria: vec!["tests pass".to_owned()],
        provider: Some(ProviderId::Codex),
        profile: ModelProfile::Standard,
        write_scopes: vec![RepoPath::try_from(scope).unwrap_or_else(|error| panic!("{error}"))],
        repository_wide_write_scope: false,
        risks: Vec::new(),
        parallel_safety: "dependency ordered".to_owned(),
    };
    validate_task_graph(
        TaskGraphProposal {
            schema_version: SchemaVersion::v1(),
            revision_id: GraphRevisionId::new(),
            session_id,
            goal_message_id: goal_id,
            planner_provider: ProviderId::Codex,
            proposed_at: timestamp(1),
            nodes: vec![
                node("domain", &[], "src/domain"),
                node("ui", &["domain"], "src/ui"),
            ],
        },
        &GraphValidationPolicy {
            eligible_providers: BTreeSet::from([ProviderId::Codex]),
            eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
            max_parallel_workers: 2,
            per_provider_limits: BTreeMap::from([(ProviderId::Codex, 1)]),
        },
    )
    .unwrap_or_else(|error| panic!("{error}"))
}

fn record_valid(
    database: &Database,
    graph: &orchestrator_domain::ValidatedTaskGraph,
) -> Result<(), Box<dyn std::error::Error>> {
    let attempt = NewGraphAttempt::from_validated(
        PlanningAttemptId::new(),
        graph.clone(),
        timestamp(1),
        timestamp(2),
    );
    let stored = database.record_graph_attempt(&attempt)?;
    assert_eq!(stored.status, GraphRevisionStatus::AwaitingApproval);
    Ok(())
}

#[test]
fn valid_and_invalid_attempts_are_immutable_session_scoped_and_create_no_tasks()
-> Result<(), Box<dyn std::error::Error>> {
    let database = database()?;
    let session_id = SessionId::new();
    let goal_id = MessageId::new();
    seed_session(&database, session_id, goal_id)?;
    let graph = validated_graph(session_id, goal_id);
    record_valid(&database, &graph)?;

    assert_eq!(
        database
            .load_graph_revision(graph.proposal.revision_id)?
            .map(|value| value.proposal_hash),
        Some(Some(graph.proposal_hash.clone()))
    );
    assert_eq!(
        database
            .current_graph(session_id)?
            .map(|value| value.revision.revision_id),
        Some(graph.proposal.revision_id)
    );
    assert!(database.current_graph(SessionId::new())?.is_none());
    database.with_connection(|connection| {
        let task_count: i64 =
            connection.query_row("SELECT count(*) FROM tasks", [], |row| row.get(0))?;
        assert_eq!(task_count, 0);
        let mutation = connection.execute(
            "UPDATE graph_revisions SET proposal_json = '{}' WHERE revision_id = ?1",
            [graph.proposal.revision_id.to_string()],
        );
        assert!(mutation.is_err());
        Ok(())
    })?;

    let invalid_revision = GraphRevisionId::new();
    database.record_graph_attempt(&NewGraphAttempt::invalid(
        PlanningAttemptId::new(),
        invalid_revision,
        session_id,
        goal_id,
        ProviderId::Codex,
        serde_json::json!({"errors": ["cycle"]}),
        "cycle",
        timestamp(3),
        timestamp(4),
    ))?;
    assert_eq!(
        database
            .load_graph_revision(invalid_revision)?
            .map(|value| value.status),
        Some(GraphRevisionStatus::Invalid)
    );
    Ok(())
}

#[test]
fn exact_current_hash_approval_materializes_once_and_wrong_or_stale_hashes_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let database = database()?;
    let session_id = SessionId::new();
    let goal_id = MessageId::new();
    seed_session(&database, session_id, goal_id)?;
    let first = validated_graph(session_id, goal_id);
    record_valid(&database, &first)?;

    let wrong = database.approve_graph_and_materialize_tasks(&GraphApprovalRequest {
        revision_id: first.proposal.revision_id,
        expected_proposal_hash: "0".repeat(64),
        approved_by: "tester".to_owned(),
        approved_at: timestamp(5),
    });
    assert!(wrong.is_err());

    let approved = database.approve_graph_and_materialize_tasks(&GraphApprovalRequest {
        revision_id: first.proposal.revision_id,
        expected_proposal_hash: first.proposal_hash.clone(),
        approved_by: "tester".to_owned(),
        approved_at: timestamp(5),
    })?;
    assert_eq!(approved.task_ids.len(), 2);
    assert!(!approved.replayed);
    let replay: ApprovedGraph =
        database.approve_graph_and_materialize_tasks(&GraphApprovalRequest {
            revision_id: first.proposal.revision_id,
            expected_proposal_hash: first.proposal_hash.clone(),
            approved_by: "tester".to_owned(),
            approved_at: timestamp(5),
        })?;
    assert_eq!(replay.task_ids, approved.task_ids);
    assert!(replay.replayed);

    database.with_connection(|connection| {
        let counts = (
            connection.query_row("SELECT count(*) FROM tasks", [], |row| row.get::<_, i64>(0))?,
            connection.query_row("SELECT count(*) FROM task_dependencies", [], |row| {
                row.get::<_, i64>(0)
            })?,
            connection.query_row("SELECT count(*) FROM graph_approvals", [], |row| {
                row.get::<_, i64>(0)
            })?,
        );
        assert_eq!(counts, (2, 1, 1));
        Ok(())
    })?;

    let mut stale = validated_graph(session_id, goal_id);
    stale.proposal.revision_id = GraphRevisionId::new();
    stale.proposal.proposed_at = timestamp(6);
    stale = validate_task_graph(
        stale.proposal,
        &GraphValidationPolicy {
            eligible_providers: BTreeSet::from([ProviderId::Codex]),
            eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
            max_parallel_workers: 2,
            per_provider_limits: BTreeMap::new(),
        },
    )
    .unwrap_or_else(|error| panic!("{error}"));
    record_valid(&database, &stale)?;
    let stale_revision_id = stale.proposal.revision_id;
    let stale_hash = stale.proposal_hash.clone();
    let mut current_proposal = stale.proposal.clone();
    current_proposal.revision_id = GraphRevisionId::new();
    current_proposal.proposed_at = timestamp(7);
    let current = validate_task_graph(
        current_proposal,
        &GraphValidationPolicy {
            eligible_providers: BTreeSet::from([ProviderId::Codex]),
            eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
            max_parallel_workers: 2,
            per_provider_limits: BTreeMap::new(),
        },
    )
    .unwrap_or_else(|error| panic!("{error}"));
    record_valid(&database, &current)?;
    assert!(
        database
            .approve_graph_and_materialize_tasks(&GraphApprovalRequest {
                revision_id: stale_revision_id,
                expected_proposal_hash: stale_hash,
                approved_by: "tester".to_owned(),
                approved_at: timestamp(8),
            })
            .is_err()
    );
    Ok(())
}

#[test]
fn approval_is_atomic_when_dependency_insert_fails() -> Result<(), Box<dyn std::error::Error>> {
    let database = database()?;
    let session_id = SessionId::new();
    let goal_id = MessageId::new();
    seed_session(&database, session_id, goal_id)?;
    let graph = validated_graph(session_id, goal_id);
    record_valid(&database, &graph)?;
    database.with_connection(|connection| {
        connection.execute_batch(
            "CREATE TRIGGER fail_dependency BEFORE INSERT ON task_dependencies
             BEGIN
                 INSERT INTO task_dependencies(
                     session_id, revision_id, task_id, depends_on_task_id
                 ) VALUES (
                     NEW.session_id, NEW.revision_id, NEW.task_id,
                     '00000000-0000-0000-0000-000000000000'
                 );
             END;",
        )?;
        Ok(())
    })?;

    assert!(
        database
            .approve_graph_and_materialize_tasks(&GraphApprovalRequest {
                revision_id: graph.proposal.revision_id,
                expected_proposal_hash: graph.proposal_hash,
                approved_by: "tester".to_owned(),
                approved_at: timestamp(8),
            })
            .is_err()
    );
    database.with_connection(|connection| {
        for table in [
            "tasks",
            "session_tasks",
            "task_dependencies",
            "graph_approvals",
        ] {
            let count: i64 =
                connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
            assert_eq!(count, 0, "partial rows remained in {table}");
        }
        let messages: i64 =
            connection.query_row("SELECT count(*) FROM conversation_messages", [], |row| {
                row.get(0)
            })?;
        assert_eq!(messages, 1, "approval message escaped the rollback");
        let events: i64 =
            connection.query_row("SELECT count(*) FROM task_events", [], |row| row.get(0))?;
        assert_eq!(events, 1, "approval events escaped the rollback");
        Ok(())
    })?;
    Ok(())
}
