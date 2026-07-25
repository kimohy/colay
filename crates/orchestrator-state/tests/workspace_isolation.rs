use chrono::Utc;
use orchestrator_domain::{
    CorrelationId, EventActor, EventId, EventType, ProviderId, QuotaPeriod, QuotaScope,
    SchemaVersion, TaskEvent, TaskId, TaskState, TransitionGuards, UsageSnapshot, UsageUnit,
};
use orchestrator_state::{Database, NewTaskRecord, RoutingAuditRecord, WorkspaceKind};

mod support;
use support::{fresh_database, with_database_connection};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const WORKSPACE_TABLES: &[&str] = &[
    "approval_records",
    "artifacts",
    "changed_files",
    "checkpoints",
    "client_commands",
    "client_command_invocations",
    "command_evidence",
    "conversation_attempts",
    "conversation_messages",
    "coordinator_leases",
    "event_log_state",
    "graph_approvals",
    "graph_revisions",
    "handovers",
    "integration_applications",
    "integration_approvals",
    "integration_batches",
    "integration_resolution_tasks",
    "integration_sources",
    "planning_attempts",
    "provider_usage_snapshots",
    "requirement_revisions",
    "resource_claims",
    "routing_decision_usage",
    "routing_decisions",
    "session_graph_heads",
    "session_requirement_heads",
    "session_tasks",
    "session_workspace_state",
    "sessions",
    "task_attempts",
    "task_controls",
    "task_dependencies",
    "task_events",
    "task_instructions",
    "task_schedule_claims",
    "tasks",
    "verification_results",
    "worker_leases",
    "worktrees",
];

fn event() -> TaskEvent {
    TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id: None,
        task_id: None,
        occurred_at: Utc::now(),
        event_type: EventType::CompatibilityWarning,
        from_state: None,
        to_state: None,
        reason: None,
        actor: EventActor::System,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload: serde_json::json!({}),
        previous_hash: None,
        event_hash: String::new(),
    }
}

#[test]
fn tasks_cannot_cross_workspace_boundaries() -> TestResult {
    let root = tempfile::tempdir()?;
    let first_path = root.path().join("first");
    let second_path = root.path().join("second");
    std::fs::create_dir_all(&first_path)?;
    std::fs::create_dir_all(&second_path)?;

    let database = Database::open_in_memory()?;
    database.migrate_with_backup(std::path::Path::new("unused"))?;
    let first_id = database
        .resolve_workspace(&first_path, WorkspaceKind::Directory)?
        .workspace_id;
    let second_id = database
        .resolve_workspace(&second_path, WorkspaceKind::Directory)?
        .workspace_id;
    let first = database.workspace(first_id);
    let second = database.workspace(second_id);

    let task_id = TaskId::new();
    first.create_task(&NewTaskRecord {
        task_id,
        schema_version: "1".to_owned(),
        state: TaskState::Queued,
        objective: "partition durable state".to_owned(),
        original_request_redacted: "partition durable state".to_owned(),
        envelope: serde_json::json!({"schema_version": "1"}),
        created_at: Utc::now(),
    })?;

    assert!(first.load_task(task_id)?.is_some());
    assert!(second.load_task(task_id)?.is_none());
    Ok(())
}

#[test]
fn each_workspace_event_chain_starts_at_one() -> TestResult {
    let root = tempfile::tempdir()?;
    let first_path = root.path().join("first");
    let second_path = root.path().join("second");
    std::fs::create_dir_all(&first_path)?;
    std::fs::create_dir_all(&second_path)?;

    let database = Database::open_in_memory()?;
    database.migrate_with_backup(std::path::Path::new("unused"))?;
    let first_id = database
        .resolve_workspace(&first_path, WorkspaceKind::Directory)?
        .workspace_id;
    let second_id = database
        .resolve_workspace(&second_path, WorkspaceKind::Directory)?
        .workspace_id;

    let first = database.workspace(first_id).append_event(event())?;
    let second = database.workspace(second_id).append_event(event())?;

    assert_eq!(first.sequence, 1);
    assert_eq!(second.sequence, 1);
    Ok(())
}

#[test]
fn every_workspace_table_has_a_required_partition_key() -> TestResult {
    let (database, _) = fresh_database()?;
    with_database_connection(&database, |connection| {
        for table in WORKSPACE_TABLES {
            let sql = format!(
                "SELECT count(*) FROM pragma_table_info('{table}', 'main') \
                 WHERE name = 'workspace_id' AND \"notnull\" = 1"
            );
            let count: i64 = connection.query_row(&sql, [], |row| row.get(0))?;
            assert_eq!(count, 1, "{table} must have a non-null workspace_id");
        }
        let reserved_count: i64 = connection.query_row(
            "SELECT count(*) FROM workspaces \
             WHERE workspace_id = '00000000-0000-0000-0000-000000000001'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            reserved_count, 0,
            "fresh databases must not expose the legacy workspace"
        );
        Ok(())
    })?;
    Ok(())
}

#[test]
fn every_workspace_foreign_key_carries_the_partition_key() -> TestResult {
    let (database, _) = fresh_database()?;
    with_database_connection(&database, |connection| {
        for table in WORKSPACE_TABLES {
            let mut statement =
                connection.prepare(&format!("PRAGMA main.foreign_key_list('{table}')"))?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            let mut foreign_keys = std::collections::BTreeMap::new();
            for row in rows {
                let (id, target, from, to) = row?;
                let (_, columns): &mut (String, Vec<(String, String)>) = foreign_keys
                    .entry(id)
                    .or_insert_with(|| (target, Vec::new()));
                columns.push((from, to));
            }
            for (_, (target, columns)) in foreign_keys {
                if target == "workspaces" || WORKSPACE_TABLES.contains(&target.as_str()) {
                    assert!(
                        columns
                            .iter()
                            .any(|(from, to)| from == "workspace_id" && to == "workspace_id"),
                        "{table} foreign key to {target} does not carry workspace_id"
                    );
                }
            }
        }
        Ok(())
    })?;
    Ok(())
}

#[test]
fn direct_test_connections_match_production_pragmas_and_enforce_foreign_keys() -> TestResult {
    let (database, _) = fresh_database()?;
    with_database_connection(&database, |connection| {
        let foreign_keys: i64 =
            connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        let journal_mode: String =
            connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        let synchronous: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
        let temp_store: i64 = connection.query_row("PRAGMA temp_store", [], |row| row.get(0))?;
        let busy_timeout: i64 =
            connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;

        assert_eq!(foreign_keys, 1);
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(synchronous, 2, "FULL synchronous mode");
        assert_eq!(temp_store, 2, "MEMORY temporary storage");
        assert_eq!(busy_timeout, 5_000);

        let missing_workspace = uuid::Uuid::now_v7().to_string();
        let foreign_key_violation = connection.execute(
            "INSERT INTO workspace_paths(
                workspace_id, canonical_path, comparison_key, git_common_dir,
                is_current, first_seen_at, last_seen_at
             ) VALUES (?1, 'missing', 'missing', NULL, 1, ?2, ?2)",
            rusqlite::params![missing_workspace, Utc::now().to_rfc3339()],
        );
        assert!(
            foreign_key_violation.is_err(),
            "test helpers must reject invalid foreign keys"
        );
        Ok(())
    })?;
    Ok(())
}

#[test]
fn global_usage_can_be_linked_to_a_workspace_routing_decision() -> TestResult {
    let root = tempfile::tempdir()?;
    let workspace_path = root.path().join("workspace");
    std::fs::create_dir_all(&workspace_path)?;
    let database = Database::open_in_memory()?;
    database.migrate_with_backup(std::path::Path::new("unused"))?;
    let workspace = database.workspace(
        database
            .resolve_workspace(&workspace_path, WorkspaceKind::Directory)?
            .workspace_id,
    );
    let snapshot = UsageSnapshot::unknown(
        ProviderId::Codex,
        QuotaScope::new("monthly", QuotaPeriod::CalendarMonth, UsageUnit::Credits),
        Utc::now(),
    );

    let snapshot_id = database.record_global_usage_snapshot(&snapshot)?;
    let task_id = TaskId::new();
    workspace.create_task(&NewTaskRecord {
        task_id,
        schema_version: "1".to_owned(),
        state: TaskState::Queued,
        objective: "route from account usage".to_owned(),
        original_request_redacted: "route from account usage".to_owned(),
        envelope: serde_json::json!({"schema_version": "1"}),
        created_at: Utc::now(),
    })?;
    workspace.record_routing_audit(&RoutingAuditRecord {
        decision_id: "routing-from-global-usage".to_owned(),
        task_id,
        schema_version: "1".to_owned(),
        selected_provider: Some(ProviderId::Codex),
        model_profile: Some("standard".to_owned()),
        effort: None,
        difficulty: "simple".to_owned(),
        risks: serde_json::json!([]),
        candidates: serde_json::json!([]),
        policy: serde_json::json!({"name": "test"}),
        downgraded: false,
        rationale: serde_json::json!(["account usage"]),
        decided_at: Utc::now(),
    })?;
    workspace.link_routing_usage("routing-from-global-usage", &[snapshot_id])?;

    assert_eq!(
        database.list_global_usage_snapshots(None, 10)?,
        vec![snapshot]
    );
    assert_eq!(workspace.list_usage_snapshots(None, 10)?.len(), 1);
    Ok(())
}

#[test]
fn composite_pause_and_resume_calls_keep_the_workspace_binding() -> TestResult {
    let root = tempfile::tempdir()?;
    let workspace_path = root.path().join("workspace");
    std::fs::create_dir_all(&workspace_path)?;
    let database = Database::open_in_memory()?;
    database.migrate_with_backup(std::path::Path::new("unused"))?;
    let workspace = database.workspace(
        database
            .resolve_workspace(&workspace_path, WorkspaceKind::Directory)?
            .workspace_id,
    );
    let task_id = TaskId::new();
    workspace.create_task(&NewTaskRecord {
        task_id,
        schema_version: "1".to_owned(),
        state: TaskState::Planned,
        objective: "preserve composite binding".to_owned(),
        original_request_redacted: "preserve composite binding".to_owned(),
        envelope: serde_json::json!({"schema_version": "1"}),
        created_at: Utc::now(),
    })?;
    let projection_event = |from, to, paused| TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id: None,
        task_id: Some(task_id),
        occurred_at: Utc::now(),
        event_type: EventType::StateTransitioned,
        from_state: Some(from),
        to_state: Some(to),
        reason: Some(if paused { "pause" } else { "resume" }.to_owned()),
        actor: EventActor::Orchestrator,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload: serde_json::json!({"paused": paused}),
        previous_hash: None,
        event_hash: String::new(),
    };

    workspace.pause_task_with_event(
        task_id,
        0,
        Utc::now(),
        projection_event(TaskState::Planned, TaskState::Blocked, true),
    )?;
    workspace.resume_task_with_event(
        task_id,
        1,
        Utc::now(),
        projection_event(TaskState::Blocked, TaskState::Planned, false),
    )?;

    let task = workspace.load_task(task_id)?.ok_or("task missing")?;
    assert_eq!(task.state, TaskState::Planned);
    assert_eq!(task.revision, 2);
    assert_eq!(workspace.outbox_after(0, 10)?.len(), 2);
    Ok(())
}

#[test]
fn global_health_counts_events_across_all_workspace_chains() -> TestResult {
    let root = tempfile::tempdir()?;
    let first_path = root.path().join("first");
    let second_path = root.path().join("second");
    std::fs::create_dir_all(&first_path)?;
    std::fs::create_dir_all(&second_path)?;
    let database = Database::open_in_memory()?;
    database.migrate_with_backup(std::path::Path::new("unused"))?;
    let first = database.workspace(
        database
            .resolve_workspace(&first_path, WorkspaceKind::Directory)?
            .workspace_id,
    );
    let second = database.workspace(
        database
            .resolve_workspace(&second_path, WorkspaceKind::Directory)?
            .workspace_id,
    );
    first.append_event(event())?;
    second.append_event(event())?;
    second.append_event(event())?;
    assert!(first.event_at(1)?.is_some());

    assert_eq!(database.health()?.last_event_sequence, 3);
    Ok(())
}

#[test]
fn writes_with_colliding_ids_only_mutate_the_bound_workspace() -> TestResult {
    let root = tempfile::tempdir()?;
    let first_path = root.path().join("first");
    let second_path = root.path().join("second");
    std::fs::create_dir_all(&first_path)?;
    std::fs::create_dir_all(&second_path)?;
    let database = Database::open_in_memory()?;
    database.migrate_with_backup(std::path::Path::new("unused"))?;
    let first = database.workspace(
        database
            .resolve_workspace(&first_path, WorkspaceKind::Directory)?
            .workspace_id,
    );
    let second = database.workspace(
        database
            .resolve_workspace(&second_path, WorkspaceKind::Directory)?
            .workspace_id,
    );
    let task_id = TaskId::new();
    let task = NewTaskRecord {
        task_id,
        schema_version: "1".to_owned(),
        state: TaskState::Queued,
        objective: "colliding identifier".to_owned(),
        original_request_redacted: "colliding identifier".to_owned(),
        envelope: serde_json::json!({"schema_version": "1"}),
        created_at: Utc::now(),
    };
    first.create_task(&task)?;
    second.create_task(&task)?;
    let mut transition = event();
    transition.event_type = EventType::AssessmentCompleted;
    transition.task_id = Some(task_id);
    transition.from_state = Some(TaskState::Queued);
    transition.to_state = Some(TaskState::Analyzing);
    first.transition_task_with_event(
        task_id,
        0,
        TaskState::Queued,
        TaskState::Analyzing,
        None,
        false,
        &TransitionGuards::default(),
        Utc::now(),
        transition,
    )?;

    assert_eq!(
        first.load_task(task_id)?.map(|task| task.state),
        Some(TaskState::Analyzing)
    );
    assert_eq!(
        second.load_task(task_id)?.map(|task| task.state),
        Some(TaskState::Queued)
    );
    Ok(())
}
