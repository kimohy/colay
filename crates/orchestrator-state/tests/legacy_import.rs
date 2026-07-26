use std::{
    collections::BTreeMap,
    error::Error,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

use chrono::Utc;
use fs2::FileExt as _;
use orchestrator_domain::{
    AttemptId, Checkpoint, CheckpointId, CorrelationId, EventActor, EventId, EventType,
    GraphRevisionId, GraphValidationAuthority, GraphValidationSummary, IntegrationApplicationId,
    IntegrationBatchId, IntegrationPreview, IntegrationSource, MessageId, ModelProfile, ProviderId,
    RepoPath, RequirementRevision, RequirementRevisionId, RequirementSnapshot, SchemaVersion,
    SessionId, TaskEnvelope, TaskEvent, TaskGraphNode, TaskGraphProposal, TaskId, TaskState,
    VerificationCommand, VerificationId, VerificationResult, VerificationStatus, WorkerOutcome,
    WorkerResult, task_graph_proposal_hash,
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
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;

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

    let error = LegacyImporter::inspect(&fixture.source, &fixture.paths)
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

    let error = LegacyImporter::inspect(&fixture.source, &fixture.paths)
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
fn nested_link_in_artifact_path_is_refused_before_source_read() -> TestResult {
    let fixture = ImportFixture::new()?;
    let outside = fixture.root.path().join("outside-artifacts");
    fs::create_dir_all(&outside)?;
    fs::write(outside.join("evidence.txt"), b"legacy artifact evidence")?;
    let linked_parent = fixture.source.root.join("linked");
    fs::create_dir_all(&linked_parent)?;
    let nested_link = linked_parent.join("nested");
    create_directory_link(&outside, &nested_link)?;
    Connection::open(&fixture.source.database)?.execute(
        "UPDATE artifacts SET relative_path = 'linked/nested/evidence.txt'",
        [],
    )?;
    let before = target_mutation_counts(&fixture)?;

    let error = LegacyImporter::inspect(&fixture.source, &fixture.paths)
        .err()
        .ok_or("nested link in artifact path was accepted")?;

    assert!(error.to_string().contains("symbolic-link traversal"));
    assert_eq!(before, target_mutation_counts(&fixture)?);
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

    let error = LegacyImporter::inspect(&fixture.source, &fixture.paths)
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
        "DROP TABLE client_command_invocations; \
         DROP TABLE legacy_import_id_mappings; \
         DROP TABLE legacy_imports; \
         DELETE FROM schema_migrations WHERE version >= 14; \
         PRAGMA user_version = 13;",
    )?;
    drop(connection);

    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("v13 source was not found")?;
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
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;

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
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;

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
fn approved_graph_collision_rewrites_only_scratch_and_propagates_hash() -> TestResult {
    let fixture = ImportFixture::new()?;
    migrate_source_to_v13(&fixture)?;
    let graph = seed_approved_source_graph(&fixture)?;
    seed_target_graph_collision(&fixture, &graph)?;
    fixture
        .global
        .workspace(fixture.workspace_id)
        .append_event(audit_event(None, "existing graph collision evidence"))?;
    let source_before = fixture.source_evidence_hashes()?;
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;

    let result =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;

    let connection = Connection::open(fixture.global.path())?;
    let mapped_revision: String = connection.query_row(
        "SELECT target_id FROM legacy_import_id_mappings \
         WHERE source_fingerprint = ?1 AND workspace_id = ?2 \
           AND entity_type = 'graph_revisions.revision_id' AND source_id = ?3",
        params![
            result.source_fingerprint,
            fixture.workspace_id.to_string(),
            graph.revision_id.to_string()
        ],
        |row| row.get(0),
    )?;
    let (proposal_hash, proposal_json, validation_json): (String, String, String) = connection
        .query_row(
            "SELECT proposal_hash, proposal_json, validation_json FROM graph_revisions \
             WHERE workspace_id = ?1 AND revision_id = ?2",
            params![fixture.workspace_id.to_string(), mapped_revision],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    let proposal: TaskGraphProposal = serde_json::from_str(&proposal_json)?;
    let validation: GraphValidationSummary = serde_json::from_str(&validation_json)?;
    let expected_requirement = graph
        .requirement_revision_id
        .ok_or("approved graph fixture has no requirement authority")?;
    let authority = validation
        .authority
        .as_ref()
        .ok_or("imported approved graph has no validation authority")?;
    assert_eq!(authority.requirement_revision_id, expected_requirement);
    assert_eq!(authority.validation_hash, "e".repeat(64));
    assert_eq!(authority.base_commit, "a".repeat(40));
    assert_eq!(authority.validation_checks, ["git_ready", "graph_valid"]);
    assert_eq!(proposal.revision_id.to_string(), mapped_revision);
    assert_eq!(
        proposal_hash,
        task_graph_proposal_hash(&proposal, &validation)?
    );
    let (approval_hash, approval_requirement, approval_validation, approval_base): (
        String,
        String,
        String,
        String,
    ) = connection.query_row(
        "SELECT proposal_hash, requirement_revision_id, validation_hash, base_commit \
         FROM graph_approvals \
         WHERE workspace_id = ?1 AND revision_id = ?2",
        params![fixture.workspace_id.to_string(), mapped_revision],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    assert_eq!(approval_hash, proposal_hash);
    assert_eq!(approval_requirement, expected_requirement.to_string());
    assert_eq!(approval_validation, authority.validation_hash);
    assert_eq!(approval_base, authority.base_commit);
    assert_ne!(proposal_hash, graph.proposal_hash);
    assert_eq!(source_before, fixture.source_evidence_hashes()?);

    let published = Connection::open(result.published_path.join("legacy.db"))?;
    let (evidence_hash, evidence_json): (String, String) = published.query_row(
        "SELECT proposal_hash, proposal_json FROM graph_revisions \
         WHERE workspace_id = ?1 AND revision_id = ?2",
        params![RESERVED_LEGACY_WORKSPACE, graph.revision_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let evidence_proposal: TaskGraphProposal = serde_json::from_str(&evidence_json)?;
    assert_eq!(evidence_hash, graph.proposal_hash);
    assert_eq!(evidence_proposal.revision_id, graph.revision_id);
    Ok(())
}

#[test]
fn integration_collision_rewrites_only_scratch_and_propagates_preview_hash() -> TestResult {
    let fixture = ImportFixture::new()?;
    migrate_source_to_v13(&fixture)?;
    let graph = seed_source_graph(&fixture, "planning", false)?;
    let integration = seed_source_integration(&fixture, &graph)?;
    seed_target_integration_collisions(&fixture, &graph, &integration)?;
    fixture
        .global
        .workspace(fixture.workspace_id)
        .append_event(audit_event(
            Some(fixture.task_id),
            "existing integration collision evidence",
        ))?;
    let source_before = fixture.source_evidence_hashes()?;
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;

    let result =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;

    let connection = Connection::open(fixture.global.path())?;
    let mapped_batch: String = connection.query_row(
        "SELECT target_id FROM legacy_import_id_mappings \
         WHERE source_fingerprint = ?1 AND workspace_id = ?2 \
           AND entity_type = 'integration_batches.batch_id' AND source_id = ?3",
        params![
            result.source_fingerprint,
            fixture.workspace_id.to_string(),
            integration.batch_id.to_string()
        ],
        |row| row.get(0),
    )?;
    let (preview_hash, preview_json): (String, String) = connection.query_row(
        "SELECT preview_hash, preview_json FROM integration_batches \
         WHERE workspace_id = ?1 AND batch_id = ?2",
        params![fixture.workspace_id.to_string(), mapped_batch],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let preview: IntegrationPreview = serde_json::from_str(&preview_json)?;
    assert!(preview.verify_integrity());
    assert_eq!(preview.batch_id.to_string(), mapped_batch);
    assert_eq!(preview.preview_hash, preview_hash);
    assert_ne!(preview_hash, integration.preview_hash);
    let source_json: String = connection.query_row(
        "SELECT source_json FROM integration_sources \
         WHERE workspace_id = ?1 AND batch_id = ?2",
        params![fixture.workspace_id.to_string(), mapped_batch],
        |row| row.get(0),
    )?;
    let imported_source: IntegrationSource = serde_json::from_str(&source_json)?;
    assert_eq!(preview.sources, vec![imported_source]);
    for table in ["integration_approvals", "integration_applications"] {
        let sql =
            format!("SELECT preview_hash FROM {table} WHERE workspace_id = ?1 AND batch_id = ?2");
        let dependent_hash: String = connection.query_row(
            &sql,
            params![fixture.workspace_id.to_string(), mapped_batch],
            |row| row.get(0),
        )?;
        assert_eq!(dependent_hash, preview_hash);
    }
    assert_eq!(source_before, fixture.source_evidence_hashes()?);

    let published = Connection::open(result.published_path.join("legacy.db"))?;
    let (evidence_hash, evidence_json): (String, String) = published.query_row(
        "SELECT preview_hash, preview_json FROM integration_batches \
         WHERE workspace_id = ?1 AND batch_id = ?2",
        params![RESERVED_LEGACY_WORKSPACE, integration.batch_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let evidence_preview: IntegrationPreview = serde_json::from_str(&evidence_json)?;
    assert_eq!(evidence_hash, integration.preview_hash);
    assert_eq!(evidence_preview.batch_id, integration.batch_id);
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

    let error = LegacyImporter::inspect(&fixture.source, &fixture.paths)
        .err()
        .ok_or("missing declared JSONL export was accepted")?;

    assert!(error.to_string().contains("JSONL"));
    Ok(())
}

#[test]
fn stale_scoped_staging_is_recovered_before_retry() -> TestResult {
    let fixture = ImportFixture::new()?;
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;
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
fn scratch_crash_residue_is_scavenged_while_active_attempt_and_outside_files_survive() -> TestResult
{
    let fixture = ImportFixture::new()?;
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;
    let database_parent = fixture
        .paths
        .database
        .parent()
        .ok_or("global database has no parent")?;
    let scratch_root = database_parent.join("import-scratch");
    let fingerprint_root = scratch_root.join(&plan.source_fingerprint);
    fs::create_dir_all(&fingerprint_root)?;

    let stale = fingerprint_root.join(format!("attempt-{}", uuid::Uuid::now_v7()));
    fs::create_dir(&stale)?;
    fs::write(stale.join("owner.lock"), b"")?;
    fs::write(stale.join("migrated.db"), b"crash residue")?;
    fs::write(stale.join("migrated.db-wal"), b"stale wal")?;
    fs::write(stale.join("migrated.db-shm"), b"stale shm")?;

    let active = fingerprint_root.join(format!("attempt-{}", uuid::Uuid::now_v7()));
    fs::create_dir(&active)?;
    let active_lock_path = active.join("owner.lock");
    let active_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&active_lock_path)?;
    active_lock.try_lock_exclusive()?;
    fs::write(active.join("rewrite.db-wal"), b"active wal")?;
    fs::write(active.join("rewrite.db-shm"), b"active shm")?;

    let outside = database_parent.join("legacy-import-outside-sentinel");
    fs::write(&outside, b"outside")?;

    let result =
        LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)?;

    assert!(result.imported);
    assert!(!stale.exists());
    assert!(active.is_dir());
    assert_eq!(fs::read(active.join("rewrite.db-wal"))?, b"active wal");
    assert_eq!(fs::read(active.join("rewrite.db-shm"))?, b"active shm");
    assert_eq!(fs::read(&outside)?, b"outside");
    let attempts = fs::read_dir(&fingerprint_root)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("attempt-"))
        .count();
    assert_eq!(attempts, 1);
    active_lock.unlock()?;
    Ok(())
}

#[test]
fn fingerprint_is_stable_across_vacuum_and_source_relocation() -> TestResult {
    let fixture = ImportFixture::new()?;
    let before = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?
        .source_fingerprint;
    Connection::open(&fixture.source.database)?.execute_batch("VACUUM;")?;
    let after_vacuum = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
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
    let after_relocation = LegacyImporter::inspect(&relocated, &fixture.paths)?
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
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;

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
fn corrupt_earlier_target_event_is_refused_before_any_target_mutation() -> TestResult {
    let fixture = ImportFixture::new()?;
    let workspace = fixture.global.workspace(fixture.workspace_id);
    workspace.append_event(audit_event(None, "target event one"))?;
    workspace.append_event(audit_event(None, "target event two"))?;
    Connection::open(fixture.global.path())?.execute(
        "DELETE FROM task_events WHERE workspace_id = ?1 AND sequence = 1",
        [fixture.workspace_id.to_string()],
    )?;
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;
    let before = target_mutation_counts(&fixture)?;

    let error = LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)
        .err()
        .ok_or("gapped target event chain was accepted")?;

    assert!(error.to_string().contains("audit event chain is invalid"));
    assert_only_pretransaction_backup_added(before, target_mutation_counts(&fixture)?);
    assert_no_published_import(&fixture, &plan.source_fingerprint);
    Ok(())
}

#[test]
fn inconsistent_target_event_cursor_is_refused_before_any_target_mutation() -> TestResult {
    let fixture = ImportFixture::new()?;
    let workspace = fixture.global.workspace(fixture.workspace_id);
    workspace.append_event(audit_event(None, "target event one"))?;
    let second = workspace.append_event(audit_event(None, "target event two"))?;
    Connection::open(fixture.global.path())?.execute(
        "INSERT INTO event_log_state(workspace_id, last_exported_sequence, last_exported_hash, \
            updated_at) VALUES (?1, 1, ?2, ?3)",
        params![
            fixture.workspace_id.to_string(),
            second.event_hash,
            Utc::now().to_rfc3339()
        ],
    )?;
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;
    let before = target_mutation_counts(&fixture)?;

    let error = LegacyImporter::apply(&fixture.global, fixture.workspace_id, &plan, &fixture.paths)
        .err()
        .ok_or("inconsistent target event cursor was accepted")?;

    assert!(error.to_string().contains("event cursor"));
    assert_only_pretransaction_backup_added(before, target_mutation_counts(&fixture)?);
    assert_no_published_import(&fixture, &plan.source_fingerprint);
    Ok(())
}

#[test]
fn mismatched_requirement_snapshot_hash_is_refused_before_target_mutation() -> TestResult {
    let fixture = ImportFixture::new()?;
    migrate_source_to_v13(&fixture)?;
    seed_source_requirement(&fixture, Some("f".repeat(64)), None)?;
    let before = target_mutation_counts(&fixture)?;

    let error = LegacyImporter::inspect(&fixture.source, &fixture.paths)
        .err()
        .ok_or("mismatched requirement snapshot hash was accepted")?;

    assert!(error.to_string().contains("requirement revision"));
    assert_eq!(before, target_mutation_counts(&fixture)?);
    Ok(())
}

#[test]
fn inconsistent_requirement_row_and_snapshot_is_refused_before_target_mutation() -> TestResult {
    let fixture = ImportFixture::new()?;
    migrate_source_to_v13(&fixture)?;
    seed_source_requirement(&fixture, None, Some(false))?;
    let before = target_mutation_counts(&fixture)?;

    let error = LegacyImporter::inspect(&fixture.source, &fixture.paths)
        .err()
        .ok_or("inconsistent requirement row and snapshot were accepted")?;

    assert!(error.to_string().contains("requirement revision"));
    assert_eq!(before, target_mutation_counts(&fixture)?);
    Ok(())
}

#[test]
fn replay_refuses_a_published_snapshot_that_no_longer_matches_its_manifest() -> TestResult {
    let fixture = ImportFixture::new()?;
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;
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

    let error = LegacyImporter::inspect(&fixture.source, &fixture.paths)
        .err()
        .ok_or("tampered checkpoint was accepted")?;

    assert!(error.to_string().contains("invalid seal"));
    Ok(())
}

#[test]
fn replay_refuses_corruption_in_the_imported_target_event_chain() -> TestResult {
    let fixture = ImportFixture::new()?;
    let plan = LegacyImporter::inspect(&fixture.source, &fixture.paths)?
        .ok_or("legacy source was not found")?;
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
    root: tempfile::TempDir,
    source: RepositoryStatePaths,
    paths: GlobalStatePaths,
    global: Database,
    workspace_id: orchestrator_state::WorkspaceId,
    task_id: TaskId,
}

const RESERVED_LEGACY_WORKSPACE: &str = "00000000-0000-0000-0000-000000000001";

struct GraphSeed {
    session_id: SessionId,
    message_id: MessageId,
    revision_id: GraphRevisionId,
    proposal_hash: String,
    requirement_revision_id: Option<RequirementRevisionId>,
}

struct IntegrationSeed {
    batch_id: IntegrationBatchId,
    application_id: IntegrationApplicationId,
    checkpoint: Checkpoint,
    verification: VerificationResult,
    preview_hash: String,
}

impl ImportFixture {
    fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let canonical_root = fs::canonicalize(root.path())?;
        let repository = canonical_root.join("repository");
        fs::create_dir_all(&repository)?;
        let source = RepositoryStatePaths::from_config(&repository, &RootConfig::default())?;
        fs::create_dir_all(&source.root)?;
        let task_id = seed_v3_source(&source)?;

        let environment = StateEnvironment::with_colay_home(canonical_root.join("global"))?;
        let paths = GlobalStatePaths::resolve(&environment)?;
        let global = Database::open(&paths.database)?;
        global.migrate_with_backup(&paths.backups)?;
        let workspace_id = global
            .resolve_repository_workspace(&repository)?
            .workspace_id;
        Ok(Self {
            root,
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

    fn source_evidence_hashes(&self) -> TestResult<BTreeMap<PathBuf, String>> {
        Ok(self
            .source_hashes()?
            .into_iter()
            .filter(|(path, _)| {
                let text = path.to_string_lossy();
                !text.ends_with(".db-wal") && !text.ends_with(".db-shm")
            })
            .collect())
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

fn migrate_source_to_v13(fixture: &ImportFixture) -> TestResult {
    {
        let source = Database::open(&fixture.source.database)?;
        source.migrate_with_backup(&fixture.source.backups)?;
    }
    Connection::open(&fixture.source.database)?.execute_batch(
        "DROP TABLE client_command_invocations; \
         DROP TABLE legacy_import_id_mappings; \
         DROP TABLE legacy_imports; \
         DELETE FROM schema_migrations WHERE version >= 14; \
         PRAGMA user_version = 13;",
    )?;
    Ok(())
}

fn seed_approved_source_graph(fixture: &ImportFixture) -> TestResult<GraphSeed> {
    seed_source_graph(fixture, "approved", true)
}

#[allow(clippy::too_many_lines)]
fn seed_source_graph(
    fixture: &ImportFixture,
    status: &str,
    include_approval: bool,
) -> TestResult<GraphSeed> {
    let now = Utc::now();
    let session_id = SessionId::new();
    let message_id = MessageId::new();
    let revision_id = GraphRevisionId::new();
    let requirement = include_approval
        .then(|| build_requirement_revision(session_id, message_id, now))
        .transpose()?;
    let proposal = TaskGraphProposal {
        schema_version: SchemaVersion::v1(),
        revision_id,
        session_id,
        goal_message_id: message_id,
        planner_provider: ProviderId::Codex,
        proposed_at: now,
        nodes: vec![TaskGraphNode {
            key: "legacy-node".to_owned(),
            title: "Legacy node".to_owned(),
            objective: "Import an approved graph".to_owned(),
            dependencies: Vec::new(),
            constraints: vec!["local only".to_owned()],
            acceptance_criteria: vec!["imported".to_owned()],
            provider: Some(ProviderId::Codex),
            profile: ModelProfile::Standard,
            write_scopes: vec![RepoPath::try_from("src/legacy.rs")?],
            repository_wide_write_scope: false,
            risks: Vec::new(),
            parallel_safety: "isolated".to_owned(),
        }],
    };
    let authority = requirement
        .as_ref()
        .map(|revision| GraphValidationAuthority {
            requirement_revision_id: revision.requirement_revision_id,
            validation_hash: "e".repeat(64),
            base_commit: "a".repeat(40),
            git_root_redacted: "<repository>".to_owned(),
            validation_checks: vec!["git_ready".to_owned(), "graph_valid".to_owned()],
        });
    let validation = GraphValidationSummary {
        node_count: 1,
        edge_count: 0,
        topological_order: vec!["legacy-node".to_owned()],
        maximum_parallel_width: 1,
        configured_parallel_workers: 1,
        authority: authority.clone(),
    };
    let proposal_hash = task_graph_proposal_hash(&proposal, &validation)?;
    let connection = Connection::open(&fixture.source.database)?;
    connection.execute(
        "INSERT INTO sessions(workspace_id, session_id, schema_version, revision, title, state, \
            created_at, updated_at, archived_at, state_v2) \
         VALUES (?1, ?2, '1', 0, 'legacy graph', 'running', ?3, ?3, NULL, 'running')",
        params![
            RESERVED_LEGACY_WORKSPACE,
            session_id.to_string(),
            now.to_rfc3339()
        ],
    )?;
    connection.execute(
        "INSERT INTO conversation_messages(workspace_id, message_id, session_id, task_id, ordinal, \
            role, kind, state, content_redacted, created_at, finalized_at) \
         VALUES (?1, ?2, ?3, NULL, 1, 'user', 'user_message', 'final', 'build graph', ?4, ?4)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            message_id.to_string(),
            session_id.to_string(),
            now.to_rfc3339()
        ],
    )?;
    if let Some(revision) = &requirement {
        insert_requirement_revision(&connection, revision, None, None)?;
    }
    connection.execute(
        "INSERT INTO graph_revisions(workspace_id, revision_id, session_id, goal_message_id, \
            ordinal, status, proposal_hash, proposal_json, validation_json, planner_provider, \
            created_at, completed_at, planner_provider_v2, requirement_revision_id, \
            validation_hash, base_commit) \
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, 'codex', ?9, ?9, 'codex', ?10, ?11, ?12)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            revision_id.to_string(),
            session_id.to_string(),
            message_id.to_string(),
            status,
            proposal_hash,
            serde_json::to_string(&proposal)?,
            serde_json::to_string(&validation)?,
            now.to_rfc3339(),
            authority
                .as_ref()
                .map(|value| value.requirement_revision_id.to_string()),
            authority
                .as_ref()
                .map(|value| value.validation_hash.as_str()),
            authority.as_ref().map(|value| value.base_commit.as_str())
        ],
    )?;
    connection.execute(
        "INSERT INTO session_graph_heads(workspace_id, session_id, revision_id, updated_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            session_id.to_string(),
            revision_id.to_string(),
            now.to_rfc3339()
        ],
    )?;
    if include_approval {
        connection.execute(
            "INSERT INTO graph_approvals(workspace_id, revision_id, proposal_hash, approved_by, \
                approved_at, session_id, requirement_revision_id, validation_hash, base_commit) \
             VALUES (?1, ?2, ?3, 'fixture', ?4, ?5, ?6, ?7, ?8)",
            params![
                RESERVED_LEGACY_WORKSPACE,
                revision_id.to_string(),
                proposal_hash,
                now.to_rfc3339(),
                session_id.to_string(),
                authority
                    .as_ref()
                    .map(|value| value.requirement_revision_id.to_string()),
                authority
                    .as_ref()
                    .map(|value| value.validation_hash.as_str()),
                authority.as_ref().map(|value| value.base_commit.as_str())
            ],
        )?;
    }
    Ok(GraphSeed {
        session_id,
        message_id,
        revision_id,
        proposal_hash,
        requirement_revision_id: requirement.map(|value| value.requirement_revision_id),
    })
}

#[allow(clippy::too_many_lines)]
fn seed_source_integration(
    fixture: &ImportFixture,
    graph: &GraphSeed,
) -> TestResult<IntegrationSeed> {
    let now = Utc::now();
    let checkpoint = Checkpoint {
        schema_version: SchemaVersion::v1(),
        checkpoint_id: CheckpointId::new(),
        task_id: fixture.task_id,
        attempt_id: AttemptId::new(),
        objective: "integration evidence".to_owned(),
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
        concise_context_summary: "integration evidence".to_owned(),
        created_at: now,
        integrity_hash: String::new(),
    }
    .seal()?;
    let verification = VerificationResult {
        schema_version: SchemaVersion::v1(),
        verification_id: VerificationId::new(),
        task_id: fixture.task_id,
        implementation_provider: ProviderId::Codex,
        reviewer_provider: None,
        status: VerificationStatus::Pass,
        checks: Vec::new(),
        acceptance_criteria: Vec::new(),
        changed_files: vec![RepoPath::try_from("src/legacy.rs")?],
        out_of_scope_files: Vec::new(),
        unresolved_todos: Vec::new(),
        requires_approval: false,
        verified_at: now,
    };
    let batch_id = IntegrationBatchId::new();
    let application_id = IntegrationApplicationId::new();
    let source = IntegrationSource {
        task_id: fixture.task_id,
        checkpoint_id: checkpoint.checkpoint_id,
        verification_id: verification.verification_id,
        base_revision: "a".repeat(40),
        diff_sha256: "b".repeat(64),
        changed_files: vec![RepoPath::try_from("src/legacy.rs")?],
    };
    let preview = IntegrationPreview::seal(
        batch_id,
        graph.session_id,
        graph.revision_id,
        "a".repeat(40),
        vec![source.clone()],
        Vec::new(),
        now,
    )?;
    let connection = Connection::open(&fixture.source.database)?;
    connection.execute(
        "INSERT INTO task_attempts(workspace_id, attempt_id, task_id, ordinal, provider_id, \
            worker_mode, started_at) \
         VALUES (?1, ?2, ?3, 1, 'codex', 'writable', ?4)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            checkpoint.attempt_id.to_string(),
            fixture.task_id.to_string(),
            now.to_rfc3339()
        ],
    )?;
    connection.execute(
        "INSERT INTO checkpoints(workspace_id, checkpoint_id, task_id, attempt_id, \
            schema_version, checkpoint_json, integrity_hash, diff_artifact_id, git_head, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            checkpoint.checkpoint_id.to_string(),
            fixture.task_id.to_string(),
            checkpoint.attempt_id.to_string(),
            checkpoint.schema_version.as_str(),
            serde_json::to_string(&checkpoint)?,
            checkpoint.integrity_hash,
            now.to_rfc3339()
        ],
    )?;
    connection.execute(
        "INSERT INTO verification_results(workspace_id, verification_id, task_id, attempt_id, \
            reviewer_provider, outcome, schema_version, result_json, started_at, completed_at) \
         VALUES (?1, ?2, ?3, NULL, NULL, 'pass', ?4, ?5, ?6, ?6)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            verification.verification_id.to_string(),
            fixture.task_id.to_string(),
            verification.schema_version.as_str(),
            serde_json::to_string(&verification)?,
            now.to_rfc3339()
        ],
    )?;
    connection.execute(
        "INSERT INTO integration_batches(workspace_id, batch_id, session_id, revision_id, ordinal, \
            status, base_revision, preview_hash, preview_json, created_at, completed_at) \
         VALUES (?1, ?2, ?3, ?4, 1, 'applying', ?5, ?6, ?7, ?8, NULL)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            batch_id.to_string(),
            graph.session_id.to_string(),
            graph.revision_id.to_string(),
            preview.base_revision,
            preview.preview_hash,
            serde_json::to_string(&preview)?,
            now.to_rfc3339()
        ],
    )?;
    connection.execute(
        "INSERT INTO integration_sources(workspace_id, batch_id, source_order, task_id, \
            checkpoint_id, verification_id, diff_sha256, source_json) \
         VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            batch_id.to_string(),
            fixture.task_id.to_string(),
            checkpoint.checkpoint_id.to_string(),
            verification.verification_id.to_string(),
            source.diff_sha256,
            serde_json::to_string(&source)?
        ],
    )?;
    connection.execute(
        "INSERT INTO integration_approvals(workspace_id, batch_id, preview_hash, approved_by, \
            approved_at) VALUES (?1, ?2, ?3, 'fixture', ?4)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            batch_id.to_string(),
            preview.preview_hash,
            now.to_rfc3339()
        ],
    )?;
    connection.execute(
        "INSERT INTO integration_applications(workspace_id, application_id, batch_id, \
            preview_hash, state, worktree_path, branch_name, resulting_tree, detail_redacted, \
            started_at, completed_at) \
         VALUES (?1, ?2, ?3, ?4, 'applying', '.colay/integration/source', \
            'source-integration', NULL, '', ?5, NULL)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            application_id.to_string(),
            batch_id.to_string(),
            preview.preview_hash,
            now.to_rfc3339()
        ],
    )?;
    Ok(IntegrationSeed {
        batch_id,
        application_id,
        checkpoint,
        verification,
        preview_hash: preview.preview_hash,
    })
}

#[allow(clippy::too_many_lines)]
fn seed_target_integration_collisions(
    fixture: &ImportFixture,
    graph: &GraphSeed,
    integration: &IntegrationSeed,
) -> TestResult {
    seed_target_graph_collision(fixture, graph)?;
    let workspace = fixture.global.workspace(fixture.workspace_id);
    let mut task = TaskEnvelope::new("target collision", "target collision", Utc::now());
    task.task_id = fixture.task_id;
    workspace.create_task_envelope(&task)?;
    let now = Utc::now().to_rfc3339();
    let connection = Connection::open(fixture.global.path())?;
    connection.execute(
        "INSERT INTO task_attempts(workspace_id, attempt_id, task_id, ordinal, provider_id, \
            worker_mode, started_at) \
         VALUES (?1, ?2, ?3, 1, 'codex', 'writable', ?4)",
        params![
            fixture.workspace_id.to_string(),
            integration.checkpoint.attempt_id.to_string(),
            fixture.task_id.to_string(),
            now
        ],
    )?;
    connection.execute(
        "INSERT INTO checkpoints(workspace_id, checkpoint_id, task_id, attempt_id, \
            schema_version, checkpoint_json, integrity_hash, diff_artifact_id, git_head, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8)",
        params![
            fixture.workspace_id.to_string(),
            integration.checkpoint.checkpoint_id.to_string(),
            fixture.task_id.to_string(),
            integration.checkpoint.attempt_id.to_string(),
            integration.checkpoint.schema_version.as_str(),
            serde_json::to_string(&integration.checkpoint)?,
            integration.checkpoint.integrity_hash,
            now
        ],
    )?;
    connection.execute(
        "INSERT INTO verification_results(workspace_id, verification_id, task_id, attempt_id, \
            reviewer_provider, outcome, schema_version, result_json, started_at, completed_at) \
         VALUES (?1, ?2, ?3, NULL, NULL, 'pass', ?4, ?5, ?6, ?6)",
        params![
            fixture.workspace_id.to_string(),
            integration.verification.verification_id.to_string(),
            fixture.task_id.to_string(),
            integration.verification.schema_version.as_str(),
            serde_json::to_string(&integration.verification)?,
            now
        ],
    )?;
    let target_source = IntegrationSource {
        task_id: fixture.task_id,
        checkpoint_id: integration.checkpoint.checkpoint_id,
        verification_id: integration.verification.verification_id,
        base_revision: "c".repeat(40),
        diff_sha256: "d".repeat(64),
        changed_files: vec![RepoPath::try_from("src/target.rs")?],
    };
    let target_preview = IntegrationPreview::seal(
        integration.batch_id,
        graph.session_id,
        graph.revision_id,
        "c".repeat(40),
        vec![target_source.clone()],
        Vec::new(),
        Utc::now(),
    )?;
    connection.execute(
        "INSERT INTO integration_batches(workspace_id, batch_id, session_id, revision_id, ordinal, \
            status, base_revision, preview_hash, preview_json, created_at, completed_at) \
         VALUES (?1, ?2, ?3, ?4, 1, 'applying', ?5, ?6, ?7, ?8, NULL)",
        params![
            fixture.workspace_id.to_string(),
            integration.batch_id.to_string(),
            graph.session_id.to_string(),
            graph.revision_id.to_string(),
            target_preview.base_revision,
            target_preview.preview_hash,
            serde_json::to_string(&target_preview)?,
            now
        ],
    )?;
    connection.execute(
        "INSERT INTO integration_sources(workspace_id, batch_id, source_order, task_id, \
            checkpoint_id, verification_id, diff_sha256, source_json) \
         VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7)",
        params![
            fixture.workspace_id.to_string(),
            integration.batch_id.to_string(),
            fixture.task_id.to_string(),
            integration.checkpoint.checkpoint_id.to_string(),
            integration.verification.verification_id.to_string(),
            target_source.diff_sha256,
            serde_json::to_string(&target_source)?
        ],
    )?;
    connection.execute(
        "INSERT INTO integration_applications(workspace_id, application_id, batch_id, \
            preview_hash, state, worktree_path, branch_name, resulting_tree, detail_redacted, \
            started_at, completed_at) \
         VALUES (?1, ?2, ?3, ?4, 'applying', '.colay/integration/target', \
            'target-integration', NULL, '', ?5, NULL)",
        params![
            fixture.workspace_id.to_string(),
            integration.application_id.to_string(),
            integration.batch_id.to_string(),
            target_preview.preview_hash,
            now
        ],
    )?;
    Ok(())
}

fn seed_source_requirement(
    fixture: &ImportFixture,
    snapshot_hash_override: Option<String>,
    complete_override: Option<bool>,
) -> TestResult<RequirementRevisionId> {
    let now = Utc::now();
    let session_id = SessionId::new();
    let message_id = MessageId::new();
    let revision = build_requirement_revision(session_id, message_id, now)?;
    let connection = Connection::open(&fixture.source.database)?;
    connection.execute(
        "INSERT INTO sessions(workspace_id, session_id, schema_version, revision, title, state, \
            created_at, updated_at, archived_at, state_v2) \
         VALUES (?1, ?2, '1', 0, 'legacy requirement', 'planning', ?3, ?3, NULL, 'planning')",
        params![
            RESERVED_LEGACY_WORKSPACE,
            session_id.to_string(),
            now.to_rfc3339()
        ],
    )?;
    connection.execute(
        "INSERT INTO conversation_messages(workspace_id, message_id, session_id, task_id, ordinal, \
            role, kind, state, content_redacted, created_at, finalized_at) \
         VALUES (?1, ?2, ?3, NULL, 1, 'user', 'user_message', 'final', 'requirements', ?4, ?4)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            message_id.to_string(),
            session_id.to_string(),
            now.to_rfc3339()
        ],
    )?;
    insert_requirement_revision(
        &connection,
        &revision,
        snapshot_hash_override,
        complete_override,
    )?;
    Ok(revision.requirement_revision_id)
}

fn build_requirement_revision(
    session_id: SessionId,
    message_id: MessageId,
    now: chrono::DateTime<Utc>,
) -> TestResult<RequirementRevision> {
    Ok(RequirementRevision::seal(
        RequirementRevisionId::new(),
        session_id,
        message_id,
        1,
        RequirementSnapshot {
            objective: "Validate legacy requirements".to_owned(),
            in_scope: vec!["legacy import".to_owned()],
            out_of_scope: Vec::new(),
            constraints: vec!["local only".to_owned()],
            acceptance_criteria: vec!["validation passes".to_owned()],
            verification_plan: vec![VerificationCommand {
                executable: "cargo".to_owned(),
                args: vec!["test".to_owned()],
            }],
            risks: Vec::new(),
            open_questions: Vec::new(),
        },
        now,
    )?)
}

fn insert_requirement_revision(
    connection: &Connection,
    revision: &RequirementRevision,
    snapshot_hash_override: Option<String>,
    complete_override: Option<bool>,
) -> TestResult {
    let snapshot_hash = snapshot_hash_override.unwrap_or_else(|| revision.snapshot_hash.clone());
    let complete = complete_override.unwrap_or_else(|| revision.snapshot.is_complete());
    connection.execute(
        "INSERT INTO requirement_revisions(workspace_id, requirement_revision_id, session_id, \
            source_message_id, ordinal, schema_version, snapshot_hash, snapshot_json, complete, \
            created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            revision.requirement_revision_id.to_string(),
            revision.session_id.to_string(),
            revision.source_message_id.to_string(),
            i64::try_from(revision.ordinal)?,
            revision.schema_version.as_str(),
            snapshot_hash,
            serde_json::to_string(&revision.snapshot)?,
            i64::from(complete),
            revision.created_at.to_rfc3339()
        ],
    )?;
    connection.execute(
        "INSERT INTO session_requirement_heads(workspace_id, session_id, requirement_revision_id, \
            updated_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            RESERVED_LEGACY_WORKSPACE,
            revision.session_id.to_string(),
            revision.requirement_revision_id.to_string(),
            revision.created_at.to_rfc3339()
        ],
    )?;
    Ok(())
}

fn seed_target_graph_collision(fixture: &ImportFixture, graph: &GraphSeed) -> TestResult {
    let now = Utc::now().to_rfc3339();
    let connection = Connection::open(fixture.global.path())?;
    connection.execute(
        "INSERT INTO sessions(workspace_id, session_id, schema_version, revision, title, state, \
            created_at, updated_at, archived_at, state_v2) \
         VALUES (?1, ?2, '1', 0, 'target graph', 'running', ?3, ?3, NULL, 'running')",
        params![
            fixture.workspace_id.to_string(),
            graph.session_id.to_string(),
            now
        ],
    )?;
    connection.execute(
        "INSERT INTO conversation_messages(workspace_id, message_id, session_id, task_id, ordinal, \
            role, kind, state, content_redacted, created_at, finalized_at) \
         VALUES (?1, ?2, ?3, NULL, 1, 'user', 'user_message', 'final', 'target graph', ?4, ?4)",
        params![
            fixture.workspace_id.to_string(),
            graph.message_id.to_string(),
            graph.session_id.to_string(),
            now
        ],
    )?;
    connection.execute(
        "INSERT INTO graph_revisions(workspace_id, revision_id, session_id, goal_message_id, \
            ordinal, status, proposal_hash, proposal_json, validation_json, planner_provider, \
            created_at, completed_at, planner_provider_v2) \
         VALUES (?1, ?2, ?3, ?4, 1, 'invalid', NULL, NULL, '{}', 'codex', ?5, ?5, 'codex')",
        params![
            fixture.workspace_id.to_string(),
            graph.revision_id.to_string(),
            graph.session_id.to_string(),
            graph.message_id.to_string(),
            now
        ],
    )?;
    Ok(())
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

fn target_mutation_counts(fixture: &ImportFixture) -> TestResult<(i64, i64, i64, i64, usize)> {
    let connection = Connection::open(fixture.global.path())?;
    Ok((
        connection.query_row(
            "SELECT count(*) FROM tasks WHERE workspace_id = ?1",
            [fixture.workspace_id.to_string()],
            |row| row.get(0),
        )?,
        connection.query_row(
            "SELECT count(*) FROM task_events WHERE workspace_id = ?1",
            [fixture.workspace_id.to_string()],
            |row| row.get(0),
        )?,
        connection.query_row(
            "SELECT count(*) FROM legacy_imports WHERE workspace_id = ?1",
            [fixture.workspace_id.to_string()],
            |row| row.get(0),
        )?,
        connection.query_row(
            "SELECT count(*) FROM legacy_import_id_mappings WHERE workspace_id = ?1",
            [fixture.workspace_id.to_string()],
            |row| row.get(0),
        )?,
        regular_file_count(&fixture.paths.backups)?,
    ))
}

fn assert_only_pretransaction_backup_added(
    before: (i64, i64, i64, i64, usize),
    after: (i64, i64, i64, i64, usize),
) {
    assert_eq!(before.0, after.0);
    assert_eq!(before.1, after.1);
    assert_eq!(before.2, after.2);
    assert_eq!(before.3, after.3);
    assert_eq!(before.4 + 1, after.4);
}

fn assert_no_published_import(fixture: &ImportFixture, fingerprint: &str) {
    let imports = fixture
        .paths
        .for_workspace(fixture.workspace_id)
        .root
        .join("imports");
    assert!(!imports.join(fingerprint).exists());
    assert!(!imports.join(format!("{fingerprint}.staging")).exists());
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) -> TestResult {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link: &Path) -> TestResult {
    let status = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .status()?;
    if !status.success() {
        return Err(format!("could not create test junction: {status}").into());
    }
    Ok(())
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
