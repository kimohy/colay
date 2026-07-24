use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use chrono::Utc;
use orchestrator_domain::{
    AttemptId, Checkpoint, CheckpointId, CorrelationId, EventActor, EventId, EventType, ProviderId,
    RepoPath, SchemaVersion, TaskEnvelope, TaskEvent, TaskId, TaskState, WorkerOutcome,
    WorkerResult,
};
use orchestrator_state::{
    ArtifactStore, Database, GlobalStatePaths, LegacyImporter, RepositoryStatePaths, RootConfig,
    StateEnvironment, StoredArtifact, TaskListFilter,
};
use rusqlite::{Connection, params};
use serde_json::json;
use sha2::{Digest as _, Sha256};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const LEGACY_MIGRATIONS: &[(u32, &str, &str)] = &[
    (1, "core", include_str!("../../../migrations/0001_core.sql")),
    (
        2,
        "execution",
        include_str!("../../../migrations/0002_execution.sql"),
    ),
    (
        3,
        "audit_and_control",
        include_str!("../../../migrations/0003_audit_and_control.sql"),
    ),
];
const DURABLE_SESSIONS_MIGRATION: &str =
    include_str!("../../../migrations/0004_durable_sessions.sql");

#[test]
fn legacy_repository_state_imports_once_without_source_mutation() -> TestResult {
    let fixture = ImportFixture::new()?;
    let before = fixture.source_hashes()?;
    let plan = LegacyImporter::inspect(&fixture.source)?.ok_or("legacy source was not found")?;

    let first =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;
    let second =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;

    assert!(first.imported);
    assert!(!second.imported);
    assert_eq!(first.imported_rows, 4);
    assert_eq!(first.source_fingerprint, second.source_fingerprint);
    assert_eq!(before, fixture.source_hashes()?);
    assert_eq!(
        fixture
            .global
            .workspace(fixture.workspace_id)
            .list_tasks(&TaskListFilter::default())?
            .len(),
        1
    );
    assert!(
        fixture
            .paths
            .for_workspace(fixture.workspace_id)
            .root
            .join("imports")
            .join(&first.source_fingerprint)
            .join("legacy.db")
            .is_file()
    );
    let relative_path = RepoPath::try_from(format!(
        "imports/{}/tasks/{}/evidence.txt",
        first.source_fingerprint, fixture.task_id
    ))?;
    let artifact = StoredArtifact {
        relative_path,
        sha256: hex::encode(Sha256::digest(b"legacy artifact evidence")),
        byte_length: u64::try_from(b"legacy artifact evidence".len())?,
    };
    assert_eq!(
        ArtifactStore::open_workspace(&fixture.paths.for_workspace(fixture.workspace_id))?
            .read_verified(&artifact)?,
        b"legacy artifact evidence"
    );
    assert_eq!(regular_file_count(&fixture.paths.backups)?, 1);
    Ok(())
}

#[test]
fn corrupt_legacy_event_chain_is_refused_without_target_or_source_mutation() -> TestResult {
    let fixture = ImportFixture::new()?;
    let before = fixture.source_hashes()?;
    Connection::open(&fixture.source.database)?.execute(
        "UPDATE task_events SET event_hash = ?1 WHERE sequence = 1",
        ["f".repeat(64)],
    )?;
    let corrupted = fixture.source_hashes()?;

    let error = LegacyImporter::inspect(&fixture.source)
        .err()
        .ok_or("corrupt event chain was accepted")?;

    assert!(error.to_string().contains("audit event chain is invalid"));
    assert_ne!(before, corrupted);
    assert_eq!(corrupted, fixture.source_hashes()?);
    assert!(
        fixture
            .global
            .workspace(fixture.workspace_id)
            .list_tasks(&TaskListFilter::default())?
            .is_empty()
    );
    assert!(
        !fixture
            .paths
            .for_workspace(fixture.workspace_id)
            .root
            .join("imports")
            .exists()
    );
    Ok(())
}

#[test]
fn artifact_metadata_that_disagrees_with_source_bytes_is_refused() -> TestResult {
    let fixture = ImportFixture::new()?;
    Connection::open(&fixture.source.database)?
        .execute("UPDATE artifacts SET sha256 = ?1", ["f".repeat(64)])?;

    let error = LegacyImporter::inspect(&fixture.source)
        .err()
        .ok_or("artifact metadata mismatch was accepted")?;

    assert!(error.to_string().contains("artifact metadata"));
    assert!(
        fixture
            .global
            .workspace(fixture.workspace_id)
            .list_tasks(&TaskListFilter::default())?
            .is_empty()
    );
    Ok(())
}

#[test]
fn live_legacy_daemon_lease_blocks_import_inspection() -> TestResult {
    let fixture = ImportFixture::new()?;
    let connection = Connection::open(&fixture.source.database)?;
    connection.execute_batch(DURABLE_SESSIONS_MIGRATION)?;
    connection.execute(
        "INSERT INTO schema_migrations(version, name, checksum, applied_at) \
         VALUES (4, 'durable_sessions', ?1, ?2)",
        params![
            hex::encode(Sha256::digest(DURABLE_SESSIONS_MIGRATION.as_bytes())),
            Utc::now().to_rfc3339(),
        ],
    )?;
    let now = Utc::now();
    connection.execute(
        "INSERT INTO daemon_instances( \
            instance_id, pid, started_at, heartbeat_at, lease_expires_at, \
            stop_requested_at, released_at \
         ) VALUES ('live-legacy-daemon', 42, ?1, ?1, ?2, NULL, NULL)",
        params![
            now.to_rfc3339(),
            (now + chrono::TimeDelta::minutes(1)).to_rfc3339()
        ],
    )?;
    drop(connection);

    let error = LegacyImporter::inspect(&fixture.source)
        .err()
        .ok_or("live legacy daemon lease was accepted")?;

    assert!(error.to_string().contains("stop it before importing"));
    Ok(())
}

#[test]
fn schema_v13_reserved_workspace_source_is_supported() -> TestResult {
    let fixture = ImportFixture::new()?;
    {
        let source = Database::open(&fixture.source.database)?;
        source.migrate_with_backup(&fixture.source.backups)?;
    }
    let connection = Connection::open(&fixture.source.database)?;
    connection.execute_batch(
        "DROP TABLE legacy_import_id_mappings; \
         DROP TABLE legacy_imports; \
         DELETE FROM schema_migrations WHERE version = 14; \
         PRAGMA user_version = 13;",
    )?;
    drop(connection);

    let plan = LegacyImporter::inspect(&fixture.source)?.ok_or("v13 source was not found")?;
    assert_eq!(plan.source_schema_version, 13);

    let result =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;
    assert!(result.imported);
    assert_eq!(result.imported_rows, 4);
    Ok(())
}

#[test]
fn existing_workspace_chain_receives_one_import_anchor() -> TestResult {
    let fixture = ImportFixture::new()?;
    let workspace = fixture.global.workspace(fixture.workspace_id);
    workspace.append_event(audit_event(None, "existing target evidence"))?;
    let plan = LegacyImporter::inspect(&fixture.source)?.ok_or("legacy source was not found")?;

    let result =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;

    assert!(result.imported);
    assert_eq!(result.imported_rows, 2);
    assert_eq!(result.anchor_sequence, Some(2));
    let anchor = workspace
        .event_at(2)?
        .ok_or("import anchor was not appended")?;
    assert_eq!(
        anchor.previous_hash,
        workspace.event_at(1)?.map(|event| event.event_hash)
    );
    assert_eq!(anchor.payload["kind"], "legacy_import_anchor");
    assert_eq!(
        anchor.payload["source_fingerprint"],
        result.source_fingerprint
    );
    let source_event_hash: String = Connection::open(&fixture.source.database)?.query_row(
        "SELECT event_hash FROM task_events WHERE sequence = 1",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(anchor.payload["legacy_event_count"], 1);
    assert_eq!(anchor.payload["legacy_event_root_hash"], source_event_hash);
    assert_eq!(anchor.payload["legacy_event_tip_hash"], source_event_hash);
    assert_eq!(anchor.payload["copied_legacy_event_chain"], false);
    assert!(workspace.event_at(3)?.is_none());
    let replay =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;
    assert!(!replay.imported);
    Ok(())
}

#[test]
fn colliding_ids_are_replaced_deterministically_and_recorded() -> TestResult {
    let fixture = ImportFixture::new()?;
    let source_attempt_id = seed_worker_result(&fixture.source, fixture.task_id)?;
    Connection::open(&fixture.source.database)?.execute(
        "INSERT INTO approval_records( \
            approval_id, task_id, action, scope_json, approved_by, approved_at \
         ) VALUES (?1, NULL, 'preserve_literal', ?2, 'fixture', ?3)",
        params![
            uuid::Uuid::now_v7().to_string(),
            serde_json::to_string(&json!({"literal": fixture.task_id.to_string()}))?,
            Utc::now().to_rfc3339(),
        ],
    )?;
    let workspace = fixture.global.workspace(fixture.workspace_id);
    let now = Utc::now();
    let mut existing = TaskEnvelope::new("existing global task", "existing request", now);
    existing.task_id = fixture.task_id;
    workspace.create_task_envelope(&existing)?;
    workspace.append_event(audit_event(
        Some(fixture.task_id),
        "existing target evidence",
    ))?;
    let plan = LegacyImporter::inspect(&fixture.source)?.ok_or("legacy source was not found")?;

    let result =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;

    let connection = Connection::open(fixture.global.path())?;
    let mapped: String = connection.query_row(
        "SELECT target_id FROM legacy_import_id_mappings \
         WHERE source_fingerprint = ?1 AND workspace_id = ?2 \
           AND entity_type = 'tasks.task_id' AND source_id = ?3",
        params![
            result.source_fingerprint,
            fixture.workspace_id.to_string(),
            fixture.task_id.to_string()
        ],
        |row| row.get(0),
    )?;
    assert_ne!(mapped, fixture.task_id.to_string());
    assert_eq!(workspace.list_tasks(&TaskListFilter::default())?.len(), 2);
    let mapped_task_id = mapped.parse::<TaskId>()?;
    let imported = workspace
        .load_task(mapped_task_id)?
        .ok_or("mapped task was not imported")?;
    assert_eq!(imported.task_id.to_string(), mapped);
    assert_eq!(
        workspace
            .load_task_envelope(mapped_task_id)?
            .ok_or("mapped task envelope was not imported")?
            .task_id,
        mapped_task_id
    );
    assert_eq!(result.anchor_sequence, Some(2));
    let attempts = workspace.list_task_attempts(mapped_task_id)?;
    let decoded = attempts
        .first()
        .ok_or("mapped worker attempt was not imported")?
        .decoded_worker_result()?
        .ok_or("mapped worker result was not imported")?;
    assert_eq!(decoded.task_id, mapped_task_id);
    assert_eq!(decoded.attempt_id, source_attempt_id);
    let scope_json: String = connection.query_row(
        "SELECT scope_json FROM approval_records \
         WHERE workspace_id = ?1 AND action = 'preserve_literal'",
        [fixture.workspace_id.to_string()],
        |row| row.get(0),
    )?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&scope_json)?["literal"],
        fixture.task_id.to_string()
    );
    connection.execute(
        "UPDATE legacy_import_id_mappings SET target_id = ?1 \
         WHERE source_fingerprint = ?2 AND entity_type = 'tasks.task_id'",
        params![uuid::Uuid::now_v7().to_string(), result.source_fingerprint],
    )?;
    let replay_error =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)
            .err()
            .ok_or("tampered ID mapping ledger was accepted")?;
    assert!(replay_error.to_string().contains("mapping ledger"));
    Ok(())
}

#[test]
fn declared_jsonl_export_without_exact_file_is_refused() -> TestResult {
    let fixture = ImportFixture::new()?;
    let connection = Connection::open(&fixture.source.database)?;
    let event_hash: String = connection.query_row(
        "SELECT event_hash FROM task_events WHERE sequence = 1",
        [],
        |row| row.get(0),
    )?;
    connection.execute(
        "UPDATE event_log_state SET last_exported_sequence = 1, last_exported_hash = ?1",
        [event_hash],
    )?;
    drop(connection);

    let error = LegacyImporter::inspect(&fixture.source)
        .err()
        .ok_or("missing declared JSONL export was accepted")?;

    assert!(error.to_string().contains("JSONL"));
    Ok(())
}

#[test]
fn stale_scoped_staging_is_recovered_before_retry() -> TestResult {
    let fixture = ImportFixture::new()?;
    let plan = LegacyImporter::inspect(&fixture.source)?.ok_or("legacy source was not found")?;
    let staging = fixture
        .paths
        .for_workspace(fixture.workspace_id)
        .root
        .join("imports")
        .join(format!("{}.staging", plan.source_fingerprint));
    fs::create_dir_all(&staging)?;
    fs::write(staging.join("partial"), b"crash residue")?;

    let result =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;

    assert!(result.imported);
    assert!(!staging.exists());
    Ok(())
}

#[test]
fn fingerprint_is_stable_across_vacuum_and_source_relocation() -> TestResult {
    let fixture = ImportFixture::new()?;
    let before = LegacyImporter::inspect(&fixture.source)?
        .ok_or("legacy source was not found")?
        .source_fingerprint;
    Connection::open(&fixture.source.database)?.execute_batch("VACUUM;")?;
    let after_vacuum = LegacyImporter::inspect(&fixture.source)?
        .ok_or("vacuumed legacy source was not found")?
        .source_fingerprint;

    let relocated_repository = fixture
        .source
        .root
        .parent()
        .ok_or("no repository parent")?
        .join("relocated-repository");
    fs::create_dir_all(&relocated_repository)?;
    let relocated =
        RepositoryStatePaths::from_config(&relocated_repository, &RootConfig::default())?;
    copy_tree(&fixture.source.root, &relocated.root)?;
    let after_relocation = LegacyImporter::inspect(&relocated)?
        .ok_or("relocated legacy source was not found")?
        .source_fingerprint;

    assert_eq!(before, after_vacuum);
    assert_eq!(before, after_relocation);
    Ok(())
}

#[test]
fn target_projections_without_audit_events_are_refused() -> TestResult {
    let fixture = ImportFixture::new()?;
    let workspace = fixture.global.workspace(fixture.workspace_id);
    workspace.create_task_envelope(&TaskEnvelope::new(
        "unaudited target task",
        "invalid pre-existing target state",
        Utc::now(),
    ))?;
    let plan = LegacyImporter::inspect(&fixture.source)?.ok_or("legacy source was not found")?;

    let error = LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)
        .err()
        .ok_or("unaudited target projections were accepted")?;

    assert!(error.to_string().contains("without an audit event chain"));
    assert!(
        !fixture
            .paths
            .for_workspace(fixture.workspace_id)
            .root
            .join("imports")
            .join(plan.source_fingerprint)
            .exists()
    );
    Ok(())
}

#[test]
fn replay_refuses_a_published_snapshot_that_no_longer_matches_its_manifest() -> TestResult {
    let fixture = ImportFixture::new()?;
    let plan = LegacyImporter::inspect(&fixture.source)?.ok_or("legacy source was not found")?;
    let first =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;
    fs::write(
        first
            .published_path
            .join(format!("tasks/{}/evidence.txt", fixture.task_id)),
        b"tampered after import",
    )?;

    let error = LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)
        .err()
        .ok_or("tampered published import was accepted as a no-op")?;

    assert!(error.to_string().contains("sealed manifest"));
    Ok(())
}

#[test]
fn tampered_checkpoint_is_refused_instead_of_resealed() -> TestResult {
    let fixture = ImportFixture::new()?;
    let attempt_id = AttemptId::new();
    let now = Utc::now();
    let connection = Connection::open(&fixture.source.database)?;
    connection.execute(
        "INSERT INTO task_attempts( \
            attempt_id, task_id, ordinal, provider_id, worker_mode, started_at \
         ) VALUES (?1, ?2, 1, 'codex', 'writable', ?3)",
        params![
            attempt_id.to_string(),
            fixture.task_id.to_string(),
            now.to_rfc3339(),
        ],
    )?;
    let mut checkpoint = Checkpoint {
        schema_version: SchemaVersion::v1(),
        checkpoint_id: CheckpointId::new(),
        task_id: fixture.task_id,
        attempt_id,
        objective: "sealed legacy checkpoint".to_owned(),
        current_plan: Vec::new(),
        completed_steps: Vec::new(),
        pending_steps: Vec::new(),
        files_read: Vec::new(),
        files_changed: Vec::new(),
        git_base: None,
        diff_path: None,
        commands_run: Vec::new(),
        tests: Vec::new(),
        decisions: Vec::new(),
        unresolved_questions: Vec::new(),
        known_failures: Vec::new(),
        worker_claim: None,
        current_worker: ProviderId::Codex,
        concise_context_summary: "legacy checkpoint".to_owned(),
        created_at: now,
        integrity_hash: String::new(),
    }
    .seal()?;
    let original_hash = checkpoint.integrity_hash.clone();
    checkpoint.objective = "tampered after sealing".to_owned();
    connection.execute(
        "INSERT INTO checkpoints( \
            checkpoint_id, task_id, attempt_id, schema_version, checkpoint_json, \
            integrity_hash, diff_artifact_id, git_head, created_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7)",
        params![
            checkpoint.checkpoint_id.to_string(),
            fixture.task_id.to_string(),
            attempt_id.to_string(),
            checkpoint.schema_version.as_str(),
            serde_json::to_string(&checkpoint)?,
            original_hash,
            now.to_rfc3339(),
        ],
    )?;
    drop(connection);

    let error = LegacyImporter::inspect(&fixture.source)
        .err()
        .ok_or("tampered checkpoint was accepted")?;

    assert!(error.to_string().contains("invalid seal"));
    Ok(())
}

#[test]
fn replay_refuses_corruption_in_the_imported_target_event_chain() -> TestResult {
    let fixture = ImportFixture::new()?;
    let plan = LegacyImporter::inspect(&fixture.source)?.ok_or("legacy source was not found")?;
    LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;
    Connection::open(fixture.global.path())?.execute(
        "UPDATE task_events SET event_hash = ?1 WHERE workspace_id = ?2 AND sequence = 1",
        params!["f".repeat(64), fixture.workspace_id.to_string()],
    )?;

    let error = LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)
        .err()
        .ok_or("corrupt imported event chain was accepted on replay")?;

    assert!(error.to_string().contains("audit event chain is invalid"));
    Ok(())
}

struct ImportFixture {
    _root: tempfile::TempDir,
    source: RepositoryStatePaths,
    paths: GlobalStatePaths,
    global: Database,
    workspace_id: orchestrator_state::WorkspaceId,
    task_id: TaskId,
}

impl ImportFixture {
    fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let repository = root.path().join("repository");
        fs::create_dir_all(&repository)?;
        let source = RepositoryStatePaths::from_config(&repository, &RootConfig::default())?;
        fs::create_dir_all(&source.root)?;
        let task_id = seed_v3_source(&source)?;

        let environment = StateEnvironment::with_colay_home(root.path().join("global"))?;
        let paths = GlobalStatePaths::resolve(&environment)?;
        let global = Database::open(&paths.database)?;
        global.migrate_with_backup(&paths.backups)?;
        let workspace_id = global
            .resolve_repository_workspace(&repository)?
            .workspace_id;
        Ok(Self {
            _root: root,
            source,
            paths,
            global,
            workspace_id,
            task_id,
        })
    }

    fn source_hashes(&self) -> TestResult<BTreeMap<PathBuf, String>> {
        hash_tree(&self.source.root)
    }
}

fn seed_v3_source(paths: &RepositoryStatePaths) -> TestResult<TaskId> {
    let connection = Connection::open(&paths.database)?;
    for (version, name, sql) in LEGACY_MIGRATIONS {
        connection.execute_batch(sql)?;
        connection.execute(
            "INSERT INTO schema_migrations(version, name, checksum, applied_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                version,
                name,
                hex::encode(Sha256::digest(sql.as_bytes())),
                Utc::now().to_rfc3339()
            ],
        )?;
    }
    let task_id = TaskId::new();
    let now = Utc::now();
    let mut envelope = TaskEnvelope::new("preserve legacy task", "legacy request", now);
    envelope.task_id = task_id;
    let mut event = TaskEvent {
        schema_version: SchemaVersion::new(SchemaVersion::V3),
        sequence: 1,
        event_id: EventId::new(),
        session_id: None,
        task_id: Some(task_id),
        occurred_at: now,
        event_type: EventType::TaskCreated,
        from_state: None,
        to_state: Some(TaskState::Queued),
        reason: None,
        actor: EventActor::System,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload: json!({"source": "legacy fixture"}),
        previous_hash: None,
        event_hash: String::new(),
    };
    event.refresh_event_hash()?;
    connection.execute(
        "INSERT INTO tasks( \
            task_id, schema_version, revision, state, resume_state, paused, objective, \
            original_request_redacted, task_envelope_json, created_at, updated_at, archived_at \
         ) VALUES (?1, ?2, 0, 'queued', NULL, 0, ?3, ?4, ?5, ?6, ?6, NULL)",
        params![
            task_id.to_string(),
            SchemaVersion::V1,
            "preserve legacy task",
            "legacy request",
            serde_json::to_string(&envelope)?,
            now.to_rfc3339()
        ],
    )?;
    connection.execute(
        "INSERT INTO task_events( \
            sequence, event_id, task_id, event_type, schema_version, occurred_at, event_json, \
            previous_hash, event_hash, exported_at \
         ) VALUES (1, ?1, ?2, 'task_created', ?3, ?4, ?5, NULL, ?6, NULL)",
        params![
            event.event_id.to_string(),
            task_id.to_string(),
            event.schema_version.as_str(),
            now.to_rfc3339(),
            serde_json::to_string(&event)?,
            event.event_hash
        ],
    )?;
    let artifact_relative_path = format!("tasks/{task_id}/evidence.txt");
    let artifact_path = paths.root.join(&artifact_relative_path);
    fs::create_dir_all(
        artifact_path
            .parent()
            .ok_or("artifact path has no parent")?,
    )?;
    let artifact_bytes = b"legacy artifact evidence";
    fs::write(&artifact_path, artifact_bytes)?;
    connection.execute(
        "INSERT INTO artifacts( \
            artifact_id, task_id, kind, relative_path, sha256, byte_length, media_type, created_at \
         ) VALUES (?1, ?2, 'fixture_evidence', ?3, ?4, ?5, 'text/plain', ?6)",
        params![
            uuid::Uuid::now_v7().to_string(),
            task_id.to_string(),
            artifact_relative_path,
            hex::encode(Sha256::digest(artifact_bytes)),
            i64::try_from(artifact_bytes.len())?,
            now.to_rfc3339(),
        ],
    )?;
    Ok(task_id)
}

fn seed_worker_result(paths: &RepositoryStatePaths, task_id: TaskId) -> TestResult<AttemptId> {
    let attempt_id = AttemptId::new();
    let now = Utc::now();
    let result = WorkerResult {
        schema_version: SchemaVersion::v1(),
        task_id,
        attempt_id,
        provider: ProviderId::Codex,
        outcome: WorkerOutcome::Succeeded,
        exit_code: Some(0),
        session_id: None,
        summary: Some("legacy worker completed".to_owned()),
        commands: Vec::new(),
        tests: Vec::new(),
        started_at: now,
        finished_at: now,
        output_truncated: false,
    };
    Connection::open(&paths.database)?.execute(
        "INSERT INTO task_attempts( \
            attempt_id, task_id, ordinal, provider_id, worker_mode, started_at, ended_at, \
            outcome, worker_result_json \
         ) VALUES (?1, ?2, 1, 'codex', 'writable', ?3, ?3, 'succeeded', ?4)",
        params![
            attempt_id.to_string(),
            task_id.to_string(),
            now.to_rfc3339(),
            serde_json::to_string(&result)?,
        ],
    )?;
    Ok(attempt_id)
}

fn audit_event(task_id: Option<TaskId>, reason: &str) -> TaskEvent {
    TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id: None,
        task_id,
        occurred_at: Utc::now(),
        event_type: EventType::CompatibilityWarning,
        from_state: None,
        to_state: None,
        reason: Some(reason.to_owned()),
        actor: EventActor::System,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload: json!({}),
        previous_hash: None,
        event_hash: String::new(),
    }
}

fn hash_tree(root: &Path) -> TestResult<BTreeMap<PathBuf, String>> {
    fn visit(root: &Path, directory: &Path, hashes: &mut BTreeMap<PathBuf, String>) -> TestResult {
        let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                visit(root, &path, hashes)?;
            } else if metadata.is_file() {
                hashes.insert(
                    path.strip_prefix(root)?.to_path_buf(),
                    hex::encode(Sha256::digest(fs::read(&path)?)),
                );
            }
        }
        Ok(())
    }

    let mut hashes = BTreeMap::new();
    visit(root, root, &mut hashes)?;
    Ok(hashes)
}

fn regular_file_count(root: &Path) -> TestResult<usize> {
    if !root.exists() {
        return Ok(0);
    }
    Ok(fs::read_dir(root)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.path().is_file())
        .count())
}

fn copy_tree(source: &Path, destination: &Path) -> TestResult {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&source_path, &destination_path)?;
        } else {
            fs::copy(source_path, destination_path)?;
        }
    }
    Ok(())
}
