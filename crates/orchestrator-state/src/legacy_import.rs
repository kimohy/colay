use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    ops::Deref,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    AppendMessageCommandPayload, ApproveGraphCommandPayload, ApproveIntegrationCommandPayload,
    Checkpoint, CommandEvidence, CorrelationId, CreateResolutionTaskCommandPayload,
    CreateSessionCommandPayload, EventActor, EventId, EventType, GraphValidationSummary,
    HandoverAcknowledgement, HandoverBundle, IntegrationBlocker, IntegrationPreview,
    IntegrationSource, MessageId, RepoPath, RequestConversationTurnCommandPayload,
    RequestPlanCommandPayload, RequirementRevisionId, RequirementSnapshot, SchemaVersion,
    SessionId, TaskEnvelope, TaskEvent, TaskGraphProposal, VerificationResult, WorkerResult,
    canonical_sha256, task_graph_proposal_hash,
};
use rusqlite::{Connection, MAIN_DB, OpenFlags, OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest as _, Sha256};

use crate::{
    Database, GlobalStatePaths, MigrationManager, RepositoryStatePaths, StateError, StateResult,
    WorkspaceId, artifacts::LegacyImportStaging, database::append_workspace_event_in_transaction,
    ensure_private_directory, ensure_private_file, import_scratch::LegacyImportScratch,
    reject_symlink_components, source_guard::SourceOpenGuard,
};

const LAST_LEGACY_SCHEMA_VERSION: u32 = 13;
const RESERVED_LEGACY_WORKSPACE: &str = "00000000-0000-0000-0000-000000000001";
const SOURCE_SNAPSHOT_NAME: &str = "legacy.db";

const REWRITE_TRIGGERS: &[(&str, &str)] = &[
    (
        "graph_revisions_immutable_payload",
        "CREATE TRIGGER graph_revisions_immutable_payload \
         BEFORE UPDATE OF workspace_id, session_id, goal_message_id, ordinal, proposal_hash, \
         proposal_json, validation_json, planner_provider, created_at ON graph_revisions \
         WHEN OLD.status <> 'planning' \
         BEGIN SELECT RAISE(ABORT, 'graph revision payload is immutable'); END",
    ),
    (
        "graph_revision_authority_immutable",
        "CREATE TRIGGER graph_revision_authority_immutable \
         BEFORE UPDATE OF requirement_revision_id, validation_hash, base_commit ON graph_revisions \
         WHEN OLD.status <> 'planning' \
         BEGIN SELECT RAISE(ABORT, 'graph validation authority is immutable'); END",
    ),
    (
        "graph_approvals_no_update",
        "CREATE TRIGGER graph_approvals_no_update BEFORE UPDATE ON graph_approvals \
         BEGIN SELECT RAISE(ABORT, 'graph approvals are immutable'); END",
    ),
    (
        "integration_batches_payload_immutable",
        "CREATE TRIGGER integration_batches_payload_immutable \
         BEFORE UPDATE OF workspace_id, batch_id, session_id, revision_id, ordinal, base_revision, \
         preview_hash, preview_json, created_at ON integration_batches \
         BEGIN SELECT RAISE(ABORT, 'integration preview payload is immutable'); END",
    ),
    (
        "integration_sources_no_update",
        "CREATE TRIGGER integration_sources_no_update BEFORE UPDATE ON integration_sources \
         BEGIN SELECT RAISE(ABORT, 'integration sources are immutable'); END",
    ),
    (
        "integration_approvals_no_update",
        "CREATE TRIGGER integration_approvals_no_update BEFORE UPDATE ON integration_approvals \
         BEGIN SELECT RAISE(ABORT, 'integration approvals are immutable'); END",
    ),
];

const IMPORT_TABLES: &[&str] = &[
    "tasks",
    "task_attempts",
    "provider_usage_snapshots",
    "routing_decisions",
    "routing_decision_usage",
    "artifacts",
    "command_evidence",
    "checkpoints",
    "handovers",
    "verification_results",
    "task_controls",
    "worktrees",
    "coordinator_leases",
    "worker_leases",
    "changed_files",
    "approval_records",
    "sessions",
    "conversation_messages",
    "client_commands",
    "session_workspace_state",
    "conversation_attempts",
    "requirement_revisions",
    "session_requirement_heads",
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
    "task_events",
    "event_log_state",
];

const ENTITY_KEYS: &[(&str, &str)] = &[
    ("tasks", "task_id"),
    ("task_attempts", "attempt_id"),
    ("provider_usage_snapshots", "snapshot_id"),
    ("routing_decisions", "decision_id"),
    ("artifacts", "artifact_id"),
    ("command_evidence", "command_id"),
    ("checkpoints", "checkpoint_id"),
    ("handovers", "handover_id"),
    ("verification_results", "verification_id"),
    ("task_controls", "control_id"),
    ("worktrees", "worktree_id"),
    ("coordinator_leases", "lease_id"),
    ("worker_leases", "lease_id"),
    ("approval_records", "approval_id"),
    ("sessions", "session_id"),
    ("conversation_messages", "message_id"),
    ("client_commands", "command_id"),
    ("conversation_attempts", "attempt_id"),
    ("requirement_revisions", "requirement_revision_id"),
    ("graph_revisions", "revision_id"),
    ("planning_attempts", "attempt_id"),
    ("task_schedule_claims", "schedule_claim_id"),
    ("resource_claims", "resource_claim_id"),
    ("task_instructions", "instruction_id"),
    ("integration_batches", "batch_id"),
    ("integration_applications", "application_id"),
    ("task_events", "event_id"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct IdMapping {
    entity_type: String,
    source_id: String,
    target_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TableColumn {
    cid: i64,
    name: String,
    declared_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key_ordinal: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ManifestFile {
    relative_path: PathBuf,
    sha256: String,
    byte_length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LegacyEventEvidence {
    count: u64,
    root_hash: Option<String>,
    tip_hash: Option<String>,
}

/// A sealed description of one repository-local database and its referenced immutable files.
#[derive(Clone, Debug)]
pub struct LegacyImportPlan {
    source: RepositoryStatePaths,
    pub source_schema_version: u32,
    pub source_fingerprint: String,
    pub source_root_hash: String,
    pub manifest_hash: String,
    event_evidence: LegacyEventEvidence,
    database_sha256: String,
    database_length: u64,
    files: Vec<ManifestFile>,
}

/// Durable outcome recorded in `legacy_imports.result_json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyImportResult {
    pub imported: bool,
    pub source_fingerprint: String,
    pub workspace_id: WorkspaceId,
    pub manifest_hash: String,
    pub source_root_hash: String,
    pub legacy_event_count: u64,
    pub legacy_event_root_hash: Option<String>,
    pub legacy_event_tip_hash: Option<String>,
    pub copied_legacy_event_chain: bool,
    pub id_mapping_count: u64,
    pub id_mapping_manifest_hash: String,
    pub imported_rows: u64,
    pub anchor_sequence: Option<u64>,
    pub published_path: PathBuf,
    pub imported_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LegacyImporter;

struct GuardedSourceConnection {
    connection: Connection,
    guard: SourceOpenGuard,
}

impl GuardedSourceConnection {
    fn revalidate(&self) -> StateResult<()> {
        self.guard.revalidate()
    }
}

impl Deref for GuardedSourceConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

impl LegacyImporter {
    /// Inspects only the explicitly supplied repository-local state store. The source connection
    /// is read-only; migration and validation operate against online-backup copies.
    pub fn inspect(
        source: &RepositoryStatePaths,
        paths: &GlobalStatePaths,
    ) -> StateResult<Option<LegacyImportPlan>> {
        if !source.database.exists() {
            return Ok(None);
        }
        let connection = open_source_read_only(&source.root, &source.database)?;
        let status = MigrationManager::status(&connection)?;
        connection.revalidate()?;
        if status.current_version == 0 {
            return Err(StateError::InvalidRecord(
                "legacy database has no migration history".to_owned(),
            ));
        }
        if status.current_version > LAST_LEGACY_SCHEMA_VERSION {
            return Err(StateError::FutureSchema {
                found: status.current_version,
                supported: LAST_LEGACY_SCHEMA_VERSION,
            });
        }
        validate_integrity(&connection, "legacy source")?;
        connection.revalidate()?;
        reject_live_legacy_daemon(&connection, status.current_version)?;
        connection.revalidate()?;
        let source_identity_hash = source_identity_hash(&connection)?;
        connection.revalidate()?;

        let scratch_fingerprint =
            inspection_scratch_fingerprint(status.current_version, &source_identity_hash);
        let scratch = LegacyImportScratch::acquire(paths, &scratch_fingerprint)?;
        let raw_snapshot = scratch.path().join("source.db");
        connection.revalidate()?;
        backup_stable_source(&connection, &raw_snapshot)?;
        connection.revalidate()?;
        let database_sha256 = sha256_file(&raw_snapshot)?;
        let database_length = file_length(&raw_snapshot)?;
        let migrated_path = scratch.path().join("migrated.db");
        let migrated = migrate_snapshot(&raw_snapshot, &migrated_path)?;
        if !legacy_workspace_exists(&migrated)? {
            if workspace_row_count(&migrated)? == 0 {
                return Ok(None);
            }
            return Err(StateError::InvalidRecord(
                "schema-13 legacy source does not contain the reserved migration workspace"
                    .to_owned(),
            ));
        }
        let events = validate_event_chain(&migrated)?;
        validate_source_documents(&migrated)?;
        validate_jsonl_evidence(&source.root, &source.events, &migrated, &events)?;
        let event_evidence = event_evidence(&events)?;
        let files = collect_manifest_files(source, &migrated)?;
        connection.revalidate()?;
        let manifest_hash = manifest_hash(&database_sha256, database_length, &files);
        let logical_content_hash = logical_workspace_hash(&migrated)?;
        let source_root_hash = hash_domain(
            b"colay/legacy-source-root/v1\0",
            connection
                .guard
                .canonical_root()
                .to_string_lossy()
                .as_bytes(),
        );
        let source_fingerprint = source_fingerprint(
            status.current_version,
            &source_identity_hash,
            &logical_content_hash,
            &files,
        );
        Ok(Some(LegacyImportPlan {
            source: source.clone(),
            source_schema_version: status.current_version,
            source_fingerprint,
            source_root_hash,
            manifest_hash,
            event_evidence,
            database_sha256,
            database_length,
            files,
        }))
    }

    /// Imports a sealed plan into one registry-selected workspace. All database rows and the
    /// ledger entry share one transaction; staged files are atomically renamed before commit and
    /// removed automatically if that commit fails.
    pub fn apply(
        global: &Database,
        target: WorkspaceId,
        plan: &LegacyImportPlan,
        paths: &GlobalStatePaths,
    ) -> StateResult<LegacyImportResult> {
        if global.path() != paths.database {
            return Err(StateError::InvalidRecord(
                "legacy import database does not match the supplied global state paths".to_owned(),
            ));
        }
        ensure_target_database(global, target)?;
        if let Some(existing) = load_existing_result(global, target, plan)? {
            validate_published_import(&existing, target, paths)?;
            return Ok(existing);
        }
        let scratch = LegacyImportScratch::acquire(paths, &plan.source_fingerprint)?;
        let inspected = Self::inspect(&plan.source, paths)?.ok_or_else(|| {
            StateError::InvalidRecord("legacy source disappeared before import".to_owned())
        })?;
        ensure_plan_unchanged(plan, &inspected)?;

        let workspace_paths = paths.for_workspace(target);
        let mut staging =
            LegacyImportStaging::acquire(&workspace_paths.root, &plan.source_fingerprint)?;
        if let Some(existing) = load_existing_result(global, target, plan)? {
            validate_published_import(&existing, target, paths)?;
            return Ok(existing);
        }
        staging.prepare()?;
        let source = open_source_read_only(&plan.source.root, &plan.source.database)?;
        let staged_database = staging.root().join(SOURCE_SNAPSHOT_NAME);
        source.revalidate()?;
        backup_stable_source(&source, &staged_database)?;
        source.revalidate()?;
        verify_file(
            &staged_database,
            &plan.database_sha256,
            plan.database_length,
        )?;
        for file in &plan.files {
            let source_file = SourceOpenGuard::open(
                &plan.source.root,
                &plan.source.root.join(&file.relative_path),
            )?;
            let bytes = source_file.read_all()?;
            staging.stage_verified_bytes(
                &file.relative_path,
                &bytes,
                &file.sha256,
                file.byte_length,
            )?;
        }
        let staged_files = manifest_files_below(staging.root(), Some(SOURCE_SNAPSHOT_NAME))?;
        let staged_manifest =
            manifest_hash(&plan.database_sha256, plan.database_length, &staged_files);
        if staged_manifest != plan.manifest_hash {
            return Err(StateError::InvalidRecord(
                "legacy import staging manifest differs from inspected source".to_owned(),
            ));
        }

        let migrated_path = scratch.path().join("migrated.db");
        let migrated = migrate_snapshot(&staged_database, &migrated_path)?;
        validate_event_chain(&migrated)?;
        validate_source_documents(&migrated)?;
        drop(migrated);
        let rewrite_path = scratch.path().join("rewrite.db");
        prepare_rewrite_scratch(&migrated_path, &rewrite_path)?;

        let published_path = staging.published_path().to_path_buf();
        let imported_at = Utc::now();
        let mut connection = global.raw_lock()?;
        let rewrite_path_text = rewrite_path.to_str().ok_or_else(|| {
            StateError::InvalidRecord(
                "legacy import rewrite path is not valid Unicode for SQLite ATTACH".to_owned(),
            )
        })?;
        connection.execute(
            "ATTACH DATABASE ?1 AS legacy_import_source",
            [rewrite_path_text],
        )?;
        let transaction_result = apply_transaction(
            &mut connection,
            target,
            plan,
            &published_path,
            imported_at,
            &mut staging,
            &paths.backups,
        );
        let detach_result = connection.execute_batch("DETACH DATABASE legacy_import_source;");
        let result = transaction_result?;
        if result.imported {
            staging.finish();
        }
        detach_result?;
        Ok(result)
    }
}

fn apply_transaction(
    connection: &mut Connection,
    target: WorkspaceId,
    plan: &LegacyImportPlan,
    published_path: &Path,
    imported_at: DateTime<Utc>,
    staging: &mut LegacyImportStaging,
    backups: &Path,
) -> StateResult<LegacyImportResult> {
    let transaction = connection.transaction()?;
    if let Some(existing) = load_existing_result_in(&transaction, target, plan)? {
        return Ok(existing);
    }
    let target_string = target.to_string();
    let workspace_exists = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.workspaces WHERE workspace_id = ?1)",
        [&target_string],
        |row| row.get::<_, bool>(0),
    )?;
    if !workspace_exists {
        return Err(StateError::WorkspaceNotFound {
            workspace_id: target_string,
        });
    }
    let target_events = validate_event_chain_for_workspace(&transaction, &target_string)?;
    validate_event_log_cursor(&transaction, &target_string, &target_events)?;
    ensure_private_directory(backups)?;
    let backup_path = backups.join(format!(
        "legacy-import-{}-{}.before.db",
        plan.source_fingerprint,
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    MigrationManager::backup(&transaction, &backup_path)?;
    let target_event_count = i64::try_from(target_events.len())
        .map_err(|_| StateError::InvalidRecord("target event count overflow".to_owned()))?;
    let (imported_rows, target_events, id_mappings) =
        import_workspace_rows(&transaction, &target_string, plan, target_event_count)?;
    let id_mapping_manifest_hash = id_mapping_manifest_hash(&id_mappings);
    let anchor_sequence = append_import_anchor(
        &transaction,
        target,
        plan,
        imported_at,
        target_events,
        id_mappings.len(),
        &id_mapping_manifest_hash,
    )?;
    validate_import_foreign_keys(&transaction)?;
    let result = LegacyImportResult {
        imported: true,
        source_fingerprint: plan.source_fingerprint.clone(),
        workspace_id: target,
        manifest_hash: plan.manifest_hash.clone(),
        source_root_hash: plan.source_root_hash.clone(),
        legacy_event_count: plan.event_evidence.count,
        legacy_event_root_hash: plan.event_evidence.root_hash.clone(),
        legacy_event_tip_hash: plan.event_evidence.tip_hash.clone(),
        copied_legacy_event_chain: target_events == 0,
        id_mapping_count: u64::try_from(id_mappings.len()).map_err(|_| {
            StateError::InvalidRecord("legacy ID mapping count overflow".to_owned())
        })?,
        id_mapping_manifest_hash,
        imported_rows,
        anchor_sequence,
        published_path: published_path.to_path_buf(),
        imported_at,
    };
    record_import_ledger(&transaction, target, plan, &result, id_mappings)?;
    staging.publish()?;
    transaction.commit()?;
    Ok(result)
}

fn import_workspace_rows(
    transaction: &Transaction<'_>,
    target_workspace: &str,
    plan: &LegacyImportPlan,
    target_events: i64,
) -> StateResult<(u64, i64, Vec<IdMapping>)> {
    if target_events == 0 {
        reject_unaudited_target_rows(transaction, target_workspace)?;
    }
    let id_mappings = prepare_collision_mappings(
        transaction,
        target_workspace,
        &plan.source_fingerprint,
        target_events > 0,
    )?;
    copy_required_daemon_instances(transaction)?;
    rewrite_sealed_documents(transaction, &id_mappings, &plan.source_fingerprint)?;
    let mut imported_rows = 0_u64;
    for table in IMPORT_TABLES {
        if target_events > 0 && matches!(*table, "task_events" | "event_log_state") {
            continue;
        }
        let changed = copy_workspace_table(
            transaction,
            table,
            target_workspace,
            &plan.source_fingerprint,
        )?;
        imported_rows = imported_rows
            .checked_add(u64::try_from(changed).map_err(|_| {
                StateError::InvalidRecord("legacy import row count overflow".to_owned())
            })?)
            .ok_or_else(|| {
                StateError::InvalidRecord("legacy import row count overflow".to_owned())
            })?;
    }
    Ok((imported_rows, target_events, id_mappings))
}

fn append_import_anchor(
    transaction: &Transaction<'_>,
    target: WorkspaceId,
    plan: &LegacyImportPlan,
    imported_at: DateTime<Utc>,
    target_events: i64,
    id_mapping_count: usize,
    id_mapping_manifest_hash: &str,
) -> StateResult<Option<u64>> {
    if target_events > 0 || id_mapping_count > 0 {
        let mut anchor = TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            session_id: None,
            task_id: None,
            occurred_at: imported_at,
            event_type: EventType::CompatibilityWarning,
            from_state: None,
            to_state: None,
            reason: Some("legacy repository state imported".to_owned()),
            actor: EventActor::System,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: json!({
                "kind": "legacy_import_anchor",
                "source_fingerprint": plan.source_fingerprint,
                "source_root_hash": plan.source_root_hash,
                "manifest_hash": plan.manifest_hash,
                "legacy_event_count": plan.event_evidence.count,
                "legacy_event_root_hash": plan.event_evidence.root_hash,
                "legacy_event_tip_hash": plan.event_evidence.tip_hash,
                "copied_legacy_event_chain": target_events == 0,
                "id_mapping_count": id_mapping_count,
                "id_mapping_manifest_hash": id_mapping_manifest_hash,
            }),
            previous_hash: None,
            event_hash: String::new(),
        };
        append_workspace_event_in_transaction(transaction, target, &mut anchor)?;
        Ok(Some(anchor.sequence))
    } else {
        Ok(None)
    }
}

fn validate_import_foreign_keys(transaction: &Transaction<'_>) -> StateResult<()> {
    let foreign_key_failures: i64 =
        transaction.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_failures != 0 {
        return Err(StateError::InvalidRecord(format!(
            "legacy import created {foreign_key_failures} foreign-key violations"
        )));
    }
    Ok(())
}

fn record_import_ledger(
    transaction: &Transaction<'_>,
    target: WorkspaceId,
    plan: &LegacyImportPlan,
    result: &LegacyImportResult,
    id_mappings: Vec<IdMapping>,
) -> StateResult<()> {
    transaction.execute(
        "INSERT INTO main.legacy_imports( \
            source_fingerprint, workspace_id, manifest_hash, imported_at, result_json \
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            plan.source_fingerprint,
            target.to_string(),
            plan.manifest_hash,
            result.imported_at.to_rfc3339(),
            serde_json::to_string(&result)?,
        ],
    )?;
    for mapping in id_mappings {
        transaction.execute(
            "INSERT INTO main.legacy_import_id_mappings( \
                source_fingerprint, workspace_id, entity_type, source_id, target_id \
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                plan.source_fingerprint,
                target.to_string(),
                mapping.entity_type,
                mapping.source_id,
                mapping.target_id,
            ],
        )?;
    }
    Ok(())
}

fn copy_workspace_table(
    transaction: &Transaction<'_>,
    table: &str,
    target_workspace: &str,
    source_fingerprint: &str,
) -> StateResult<usize> {
    let source_columns = table_columns(transaction, "legacy_import_source", table)?;
    let target_columns = table_columns(transaction, "main", table)?;
    if source_columns != target_columns {
        return Err(StateError::InvalidRecord(format!(
            "legacy import table {table} does not match the canonical target schema"
        )));
    }
    let columns = source_columns
        .iter()
        .map(|column| column.name.clone())
        .collect::<Vec<_>>();
    if columns.is_empty()
        || columns
            .first()
            .is_none_or(|column| column != "workspace_id")
    {
        return Err(StateError::InvalidRecord(format!(
            "legacy import table {table} is not workspace-partitioned"
        )));
    }
    let quoted = columns
        .iter()
        .map(|column| format!("\"{column}\""))
        .collect::<Vec<_>>();
    let foreign_keys = foreign_key_entities(transaction, table)?;
    let projections = columns
        .iter()
        .map(|column| -> StateResult<String> {
            if column == "workspace_id" {
                Ok("?1".to_owned())
            } else if table == "tasks" && column == "task_envelope_json" {
                Ok(format!(
                    "json_set(source.\"task_envelope_json\", '$.task_id', {})",
                    remapped_id_expression("tasks.task_id", "task_id")
                ))
            } else if table == "verification_results" && column == "result_json" {
                Ok(format!(
                    "json_set(source.\"result_json\", \
                     '$.verification_id', {}, '$.task_id', {})",
                    remapped_id_expression(
                        "verification_results.verification_id",
                        "verification_id"
                    ),
                    remapped_id_expression("tasks.task_id", "task_id")
                ))
            } else if table == "artifacts" && column == "relative_path" {
                Ok(format!(
                    "'imports/{source_fingerprint}/' || source.\"relative_path\""
                ))
            } else if table == "client_commands" && column == "idempotency_key" {
                Ok(format!(
                    "'legacy-import:{source_fingerprint}:' || source.\"idempotency_key\""
                ))
            } else if table == "worktrees"
                && matches!(column.as_str(), "worktree_path" | "branch_name")
            {
                Ok(format!(
                    "'legacy-import/{source_fingerprint}/' || source.\"{column}\""
                ))
            } else {
                let entity = entity_key(table, column)
                    .or_else(|| foreign_keys.get(column).map(String::as_str));
                Ok(entity.map_or_else(
                    || format!("source.\"{column}\""),
                    |entity| remapped_id_expression(entity, column),
                ))
            }
        })
        .collect::<StateResult<Vec<_>>>()?;
    let sql = format!(
        "INSERT INTO main.\"{table}\"({}) \
         SELECT {} FROM legacy_import_source.\"{table}\" AS source WHERE workspace_id = ?2",
        quoted.join(", "),
        projections.join(", ")
    );
    transaction
        .execute(&sql, params![target_workspace, RESERVED_LEGACY_WORKSPACE])
        .map_err(StateError::from)
}

fn table_columns(
    connection: &Connection,
    schema: &str,
    table: &str,
) -> StateResult<Vec<TableColumn>> {
    let pragma = format!("PRAGMA {schema}.table_info(\"{table}\")");
    let mut statement = connection.prepare(&pragma)?;
    statement
        .query_map([], |row| {
            Ok(TableColumn {
                cid: row.get(0)?,
                name: row.get(1)?,
                declared_type: row.get(2)?,
                not_null: row.get(3)?,
                default_value: row.get(4)?,
                primary_key_ordinal: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StateError::from)
}

fn entity_key(table: &str, column: &str) -> Option<&'static str> {
    ENTITY_KEYS
        .iter()
        .find(|(candidate_table, candidate_column)| {
            *candidate_table == table && *candidate_column == column
        })
        .map(|(candidate_table, candidate_column)| {
            // All entries are static strings, so the formatted spelling can be matched below.
            match (*candidate_table, *candidate_column) {
                ("tasks", "task_id") => "tasks.task_id",
                ("task_attempts", "attempt_id") => "task_attempts.attempt_id",
                ("provider_usage_snapshots", "snapshot_id") => {
                    "provider_usage_snapshots.snapshot_id"
                }
                ("routing_decisions", "decision_id") => "routing_decisions.decision_id",
                ("artifacts", "artifact_id") => "artifacts.artifact_id",
                ("command_evidence", "command_id") => "command_evidence.command_id",
                ("checkpoints", "checkpoint_id") => "checkpoints.checkpoint_id",
                ("handovers", "handover_id") => "handovers.handover_id",
                ("verification_results", "verification_id") => {
                    "verification_results.verification_id"
                }
                ("task_controls", "control_id") => "task_controls.control_id",
                ("worktrees", "worktree_id") => "worktrees.worktree_id",
                ("coordinator_leases", "lease_id") => "coordinator_leases.lease_id",
                ("worker_leases", "lease_id") => "worker_leases.lease_id",
                ("approval_records", "approval_id") => "approval_records.approval_id",
                ("sessions", "session_id") => "sessions.session_id",
                ("conversation_messages", "message_id") => "conversation_messages.message_id",
                ("client_commands", "command_id") => "client_commands.command_id",
                ("conversation_attempts", "attempt_id") => "conversation_attempts.attempt_id",
                ("requirement_revisions", "requirement_revision_id") => {
                    "requirement_revisions.requirement_revision_id"
                }
                ("graph_revisions", "revision_id") => "graph_revisions.revision_id",
                ("planning_attempts", "attempt_id") => "planning_attempts.attempt_id",
                ("task_schedule_claims", "schedule_claim_id") => {
                    "task_schedule_claims.schedule_claim_id"
                }
                ("resource_claims", "resource_claim_id") => "resource_claims.resource_claim_id",
                ("task_instructions", "instruction_id") => "task_instructions.instruction_id",
                ("integration_batches", "batch_id") => "integration_batches.batch_id",
                ("integration_applications", "application_id") => {
                    "integration_applications.application_id"
                }
                ("task_events", "event_id") => "task_events.event_id",
                _ => unreachable!("entity key table is exhaustive"),
            }
        })
}

fn foreign_key_entities(
    connection: &Connection,
    table: &str,
) -> StateResult<BTreeMap<String, String>> {
    let pragma = format!("PRAGMA main.foreign_key_list(\"{table}\")");
    let mut statement = connection.prepare(&pragma)?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut entities = BTreeMap::new();
    for row in rows {
        let (referenced_table, source_column, referenced_column) = row?;
        let entity = if referenced_table == "daemon_instances" && referenced_column == "instance_id"
        {
            Some("daemon_instances.instance_id")
        } else {
            entity_key(&referenced_table, &referenced_column)
        };
        if source_column != "workspace_id"
            && let Some(entity) = entity
        {
            entities.insert(source_column, entity.to_owned());
        }
    }
    Ok(entities)
}

fn remapped_id_expression(entity: &str, column: &str) -> String {
    format!(
        "COALESCE((SELECT mapping.target_id FROM temp.legacy_import_id_map AS mapping \
         WHERE mapping.entity_type = '{entity}' \
           AND mapping.source_id = source.\"{column}\"), source.\"{column}\")"
    )
}

fn reject_unaudited_target_rows(
    connection: &Connection,
    target_workspace: &str,
) -> StateResult<()> {
    for table in IMPORT_TABLES {
        if matches!(*table, "task_events" | "event_log_state") {
            continue;
        }
        let sql = format!("SELECT EXISTS(SELECT 1 FROM main.\"{table}\" WHERE workspace_id = ?1)");
        let exists: bool = connection.query_row(&sql, [target_workspace], |row| row.get(0))?;
        if exists {
            return Err(StateError::InvalidRecord(format!(
                "target workspace contains {table} rows without an audit event chain"
            )));
        }
    }
    Ok(())
}

fn prepare_collision_mappings(
    transaction: &Transaction<'_>,
    target_workspace: &str,
    source_fingerprint: &str,
    skip_events: bool,
) -> StateResult<Vec<IdMapping>> {
    transaction.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS legacy_import_id_map( \
            entity_type TEXT NOT NULL, \
            source_id TEXT NOT NULL, \
            target_id TEXT NOT NULL, \
            PRIMARY KEY(entity_type, source_id), \
            UNIQUE(entity_type, target_id) \
         ) WITHOUT ROWID; \
         DELETE FROM temp.legacy_import_id_map;",
    )?;
    let mut mappings = Vec::new();
    for (table, column) in ENTITY_KEYS {
        if skip_events && *table == "task_events" {
            continue;
        }
        let entity = entity_key(table, column).ok_or_else(|| {
            StateError::InvalidRecord(format!("missing entity mapping for {table}.{column}"))
        })?;
        let collision_sql = format!(
            "SELECT source.\"{column}\" \
             FROM legacy_import_source.\"{table}\" AS source \
             JOIN main.\"{table}\" AS target \
               ON target.workspace_id = ?1 \
              AND target.\"{column}\" = source.\"{column}\" \
             WHERE source.workspace_id = ?2 \
             ORDER BY source.\"{column}\""
        );
        let mut statement = transaction.prepare(&collision_sql)?;
        let collisions = statement
            .query_map(
                params![target_workspace, RESERVED_LEGACY_WORKSPACE],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for source_id in collisions {
            let mut nonce = 0_u64;
            let target_id = loop {
                let candidate =
                    deterministic_replacement_id(source_fingerprint, entity, &source_id, nonce);
                let target_collision_sql = format!(
                    "SELECT EXISTS(SELECT 1 FROM main.\"{table}\" \
                     WHERE workspace_id = ?1 AND \"{column}\" = ?2)"
                );
                let target_collision: bool = transaction.query_row(
                    &target_collision_sql,
                    params![target_workspace, candidate],
                    |row| row.get(0),
                )?;
                let mapping_collision: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM temp.legacy_import_id_map \
                     WHERE entity_type = ?1 AND target_id = ?2)",
                    params![entity, candidate],
                    |row| row.get(0),
                )?;
                if !target_collision && !mapping_collision {
                    break candidate;
                }
                nonce = nonce.checked_add(1).ok_or_else(|| {
                    StateError::InvalidRecord("legacy ID replacement nonce overflow".to_owned())
                })?;
            };
            transaction.execute(
                "INSERT INTO temp.legacy_import_id_map(entity_type, source_id, target_id) \
                 VALUES (?1, ?2, ?3)",
                params![entity, source_id, target_id],
            )?;
            mappings.push(IdMapping {
                entity_type: entity.to_owned(),
                source_id,
                target_id,
            });
        }
    }
    prepare_daemon_instance_mappings(transaction, source_fingerprint, &mut mappings)?;
    Ok(mappings)
}

fn prepare_daemon_instance_mappings(
    transaction: &Transaction<'_>,
    source_fingerprint: &str,
    mappings: &mut Vec<IdMapping>,
) -> StateResult<()> {
    let source_ids = {
        let mut statement = transaction.prepare(
            "SELECT DISTINCT daemon_instance_id \
             FROM legacy_import_source.task_schedule_claims \
             WHERE workspace_id = ?1 ORDER BY daemon_instance_id",
        )?;
        statement
            .query_map([RESERVED_LEGACY_WORKSPACE], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let entity = "daemon_instances.instance_id";
    for source_id in source_ids {
        let mut nonce = 0_u64;
        let target_id = loop {
            let candidate =
                deterministic_replacement_id(source_fingerprint, entity, &source_id, nonce);
            let target_collision: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM main.daemon_instances WHERE instance_id = ?1)",
                [&candidate],
                |row| row.get(0),
            )?;
            let mapping_collision: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM temp.legacy_import_id_map \
                 WHERE entity_type = ?1 AND target_id = ?2)",
                params![entity, candidate],
                |row| row.get(0),
            )?;
            if !target_collision && !mapping_collision {
                break candidate;
            }
            nonce = nonce.checked_add(1).ok_or_else(|| {
                StateError::InvalidRecord("legacy daemon ID replacement nonce overflow".to_owned())
            })?;
        };
        transaction.execute(
            "INSERT INTO temp.legacy_import_id_map(entity_type, source_id, target_id) \
             VALUES (?1, ?2, ?3)",
            params![entity, source_id, target_id],
        )?;
        mappings.push(IdMapping {
            entity_type: entity.to_owned(),
            source_id,
            target_id,
        });
    }
    Ok(())
}

fn copy_required_daemon_instances(transaction: &Transaction<'_>) -> StateResult<()> {
    transaction.execute(
        "INSERT INTO main.daemon_instances( \
            instance_id, pid, started_at, heartbeat_at, lease_expires_at, \
            stop_requested_at, released_at \
         ) \
         SELECT mapping.target_id, daemon.pid, daemon.started_at, daemon.heartbeat_at, \
                daemon.lease_expires_at, daemon.stop_requested_at, \
                COALESCE(daemon.released_at, daemon.lease_expires_at) \
         FROM legacy_import_source.daemon_instances AS daemon \
         JOIN ( \
             SELECT DISTINCT daemon_instance_id \
             FROM legacy_import_source.task_schedule_claims \
             WHERE workspace_id = ?1 \
         ) AS claim ON claim.daemon_instance_id = daemon.instance_id \
         JOIN temp.legacy_import_id_map AS mapping \
           ON mapping.entity_type = 'daemon_instances.instance_id' \
          AND mapping.source_id = daemon.instance_id",
        [RESERVED_LEGACY_WORKSPACE],
    )?;
    Ok(())
}

fn deterministic_replacement_id(
    source_fingerprint: &str,
    entity_type: &str,
    source_id: &str,
    nonce: u64,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"colay/legacy-import-id/v1\0");
    digest.update(source_fingerprint.as_bytes());
    digest.update(b"\0");
    digest.update(entity_type.as_bytes());
    digest.update(b"\0");
    digest.update(source_id.as_bytes());
    digest.update(b"\0");
    digest.update(nonce.to_le_bytes());
    let digest = digest.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn id_mapping_manifest_hash(mappings: &[IdMapping]) -> String {
    let mut ordered = mappings.to_vec();
    ordered.sort_by(|left, right| {
        (&left.entity_type, &left.source_id, &left.target_id).cmp(&(
            &right.entity_type,
            &right.source_id,
            &right.target_id,
        ))
    });
    let mut digest = Sha256::new();
    digest.update(b"colay/legacy-id-mappings/v1\0");
    for mapping in ordered {
        for value in [mapping.entity_type, mapping.source_id, mapping.target_id] {
            update_digest_length(&mut digest, value.len());
            digest.update(value.as_bytes());
        }
    }
    hex::encode(digest.finalize())
}

fn rewrite_sealed_documents(
    transaction: &Transaction<'_>,
    mappings: &[IdMapping],
    source_fingerprint: &str,
) -> StateResult<()> {
    validate_source_documents(transaction)?;
    rewrite_worker_results(transaction, mappings, source_fingerprint)?;
    rewrite_checkpoints(transaction, mappings, source_fingerprint)?;
    rewrite_handovers(transaction, mappings, source_fingerprint)?;
    rewrite_verification_results(transaction, mappings, source_fingerprint)?;
    let graph_hashes = rewrite_graphs(transaction, mappings)?;
    let integration_hashes = rewrite_integrations(transaction, mappings)?;
    rewrite_remaining_json(transaction, mappings, &graph_hashes, &integration_hashes)
}

fn validate_source_documents(connection: &Connection) -> StateResult<()> {
    validate_source_tasks_and_attempts(connection)?;
    validate_source_checkpoints(connection)?;
    validate_source_handovers(connection)?;
    validate_source_verifications(connection)?;
    validate_source_requirements(connection)?;
    validate_source_graphs(connection)?;
    validate_source_integrations(connection)
}

fn validate_source_requirements(connection: &Connection) -> StateResult<()> {
    let mut statement = connection.prepare(
        "SELECT requirement_revision_id, session_id, source_message_id, ordinal, \
                schema_version, snapshot_hash, snapshot_json, complete \
         FROM requirement_revisions WHERE workspace_id = ?1",
    )?;
    let rows = statement.query_map([RESERVED_LEGACY_WORKSPACE], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, i64>(7)?,
        ))
    })?;
    for row in rows {
        let (revision_id, session_id, message_id, ordinal, schema, stored_hash, json, complete) =
            row?;
        let invalid = |reason: &str| {
            StateError::InvalidRecord(format!(
                "legacy requirement revision {revision_id} is invalid: {reason}"
            ))
        };
        parse_source_id::<RequirementRevisionId>("requirement_revision_id", &revision_id)
            .map_err(|_| invalid("revision identity is malformed"))?;
        parse_source_id::<SessionId>("session_id", &session_id)
            .map_err(|_| invalid("session identity is malformed"))?;
        parse_source_id::<MessageId>("source_message_id", &message_id)
            .map_err(|_| invalid("source-message identity is malformed"))?;
        let ordinal = u64::try_from(ordinal).map_err(|_| invalid("ordinal is negative"))?;
        if ordinal == 0 || schema != SchemaVersion::V1 {
            return Err(invalid("schema version or ordinal is unsupported"));
        }
        let snapshot: RequirementSnapshot =
            serde_json::from_str(&json).map_err(|_| invalid("snapshot JSON is not typed"))?;
        if i64::from(snapshot.is_complete()) != complete {
            return Err(invalid("complete flag does not match the typed snapshot"));
        }
        let expected_hash =
            canonical_sha256(&snapshot).map_err(|_| invalid("snapshot cannot be sealed"))?;
        if expected_hash != stored_hash {
            return Err(invalid("snapshot hash does not match canonical content"));
        }
    }
    Ok(())
}

fn parse_source_id<T>(field: &str, value: &str) -> StateResult<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value.parse().map_err(|error| {
        StateError::InvalidRecord(format!(
            "legacy source {field} identifier `{value}` is invalid: {error}"
        ))
    })
}

fn validate_source_tasks_and_attempts(connection: &Connection) -> StateResult<()> {
    let mut tasks = connection.prepare(
        "SELECT task_id, schema_version, task_envelope_json FROM tasks WHERE workspace_id = ?1",
    )?;
    let rows = tasks.query_map([RESERVED_LEGACY_WORKSPACE], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (task_id, schema_version, envelope_json) = row?;
        let envelope: TaskEnvelope = serde_json::from_str(&envelope_json)?;
        if !envelope.has_supported_schema()
            || envelope.task_id.to_string() != task_id
            || envelope.schema_version.as_str() != schema_version
        {
            return Err(StateError::InvalidRecord(format!(
                "legacy task envelope {task_id} does not match its row"
            )));
        }
    }
    drop(tasks);

    let mut attempts = connection.prepare(
        "SELECT attempt_id, task_id, worker_result_json FROM task_attempts \
         WHERE workspace_id = ?1 AND worker_result_json IS NOT NULL",
    )?;
    let rows = attempts.query_map([RESERVED_LEGACY_WORKSPACE], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (attempt_id, task_id, result_json) = row?;
        let value: serde_json::Value = serde_json::from_str(&result_json)?;
        if value.get("schema_version").is_none() {
            continue;
        }
        let result: WorkerResult = serde_json::from_value(value)?;
        if !result.has_supported_schema()
            || result.attempt_id.to_string() != attempt_id
            || result.task_id.to_string() != task_id
        {
            return Err(StateError::InvalidRecord(format!(
                "legacy worker result {attempt_id} does not match its row"
            )));
        }
    }
    Ok(())
}

fn validate_source_checkpoints(connection: &Connection) -> StateResult<()> {
    let mut statement = connection.prepare(
        "SELECT checkpoint_id, task_id, attempt_id, schema_version, checkpoint_json, integrity_hash \
         FROM checkpoints WHERE workspace_id = ?1",
    )?;
    let rows = statement.query_map([RESERVED_LEGACY_WORKSPACE], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (checkpoint_id, task_id, attempt_id, schema_version, json, stored_hash) = row?;
        let checkpoint: Checkpoint = serde_json::from_str(&json)?;
        let valid = checkpoint.has_supported_schema()
            && checkpoint
                .verify_integrity()
                .map_err(|error| StateError::InvalidRecord(error.to_string()))?
            && checkpoint.integrity_hash == stored_hash
            && checkpoint.checkpoint_id.to_string() == checkpoint_id
            && checkpoint.task_id.to_string() == task_id
            && Some(checkpoint.attempt_id.to_string()) == attempt_id
            && checkpoint.schema_version.as_str() == schema_version;
        if !valid {
            return Err(StateError::InvalidRecord(format!(
                "legacy checkpoint {checkpoint_id} has an invalid seal or row identity"
            )));
        }
    }
    Ok(())
}

fn validate_source_handovers(connection: &Connection) -> StateResult<()> {
    let mut statement = connection.prepare(
        "SELECT handover_id, task_id, schema_version, bundle_json, integrity_hash, acknowledgement_json \
         FROM handovers WHERE workspace_id = ?1",
    )?;
    let rows = statement.query_map([RESERVED_LEGACY_WORKSPACE], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    })?;
    for row in rows {
        let (handover_id, task_id, schema_version, json, stored_hash, acknowledgement) = row?;
        let bundle: HandoverBundle = serde_json::from_str(&json)?;
        let acknowledgement = acknowledgement
            .map(|value| serde_json::from_str::<HandoverAcknowledgement>(&value))
            .transpose()?;
        let valid = bundle.has_supported_schema()
            && bundle
                .verify_integrity()
                .map_err(|error| StateError::InvalidRecord(error.to_string()))?
            && bundle.integrity_hash == stored_hash
            && bundle.handover_id.to_string() == handover_id
            && bundle.task_id.to_string() == task_id
            && bundle.schema_version.as_str() == schema_version
            && acknowledgement
                .as_ref()
                .is_none_or(|value| value.matches(&bundle));
        if !valid {
            return Err(StateError::InvalidRecord(format!(
                "legacy handover {handover_id} has an invalid seal or row identity"
            )));
        }
    }
    Ok(())
}

fn validate_source_verifications(connection: &Connection) -> StateResult<()> {
    let mut statement = connection.prepare(
        "SELECT verification_id, task_id, schema_version, result_json \
         FROM verification_results WHERE workspace_id = ?1",
    )?;
    let rows = statement.query_map([RESERVED_LEGACY_WORKSPACE], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (verification_id, task_id, schema_version, json) = row?;
        let result: VerificationResult = serde_json::from_str(&json)?;
        if result.verification_id.to_string() != verification_id
            || result.task_id.to_string() != task_id
            || result.schema_version.as_str() != schema_version
        {
            return Err(StateError::InvalidRecord(format!(
                "legacy verification {verification_id} does not match its row"
            )));
        }
    }
    Ok(())
}

fn validate_source_graphs(connection: &Connection) -> StateResult<()> {
    let mut statement = connection.prepare(
        "SELECT revision_id, session_id, goal_message_id, proposal_hash, proposal_json, \
                validation_json, requirement_revision_id, validation_hash, base_commit \
         FROM graph_revisions WHERE workspace_id = ?1",
    )?;
    let rows = statement.query_map([RESERVED_LEGACY_WORKSPACE], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<String>>(8)?,
        ))
    })?;
    for row in rows {
        let (
            revision,
            session,
            goal,
            hash,
            proposal,
            validation,
            requirement,
            validation_hash,
            base,
        ) = row?;
        let validation: GraphValidationSummary = serde_json::from_str(&validation)?;
        let authority_matches = validation.authority.as_ref().map(|authority| {
            (
                authority.requirement_revision_id.to_string(),
                authority.validation_hash.clone(),
                authority.base_commit.clone(),
            )
        }) == requirement
            .zip(validation_hash)
            .zip(base)
            .map(|((requirement, hash), base)| (requirement, hash, base));
        let proposal_matches = match (proposal, hash) {
            (None, None) => true,
            (Some(json), Some(hash)) => {
                let proposal: TaskGraphProposal = serde_json::from_str(&json)?;
                proposal.revision_id.to_string() == revision
                    && proposal.session_id.to_string() == session
                    && proposal.goal_message_id.to_string() == goal
                    && proposal
                        .schema_version
                        .is_supported_by(orchestrator_domain::SUPPORTED_TASK_GRAPH_SCHEMA_VERSIONS)
                    && task_graph_proposal_hash(&proposal, &validation)
                        .is_ok_and(|observed| observed == hash)
            }
            _ => false,
        };
        if !authority_matches || !proposal_matches {
            return Err(StateError::InvalidRecord(format!(
                "legacy graph revision {revision} has invalid authority or proposal evidence"
            )));
        }
    }
    let approval_mismatches: i64 = connection.query_row(
        "SELECT count(*) FROM graph_approvals AS approval \
         JOIN graph_revisions AS revision \
           ON revision.workspace_id = approval.workspace_id \
          AND revision.revision_id = approval.revision_id \
         WHERE approval.workspace_id = ?1 \
           AND approval.proposal_hash <> revision.proposal_hash",
        [RESERVED_LEGACY_WORKSPACE],
        |row| row.get(0),
    )?;
    if approval_mismatches != 0 {
        return Err(StateError::InvalidRecord(
            "legacy graph approvals do not match their proposal seals".to_owned(),
        ));
    }
    Ok(())
}

fn validate_source_integrations(connection: &Connection) -> StateResult<()> {
    let mut statement = connection.prepare(
        "SELECT batch_id, session_id, revision_id, base_revision, preview_hash, preview_json \
         FROM integration_batches WHERE workspace_id = ?1",
    )?;
    let rows = statement.query_map([RESERVED_LEGACY_WORKSPACE], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (batch, session, revision, base, hash, json) = row?;
        let preview: IntegrationPreview = serde_json::from_str(&json)?;
        if !preview.verify_integrity()
            || preview.batch_id.to_string() != batch
            || preview.session_id.to_string() != session
            || preview.graph_revision_id.to_string() != revision
            || preview.base_revision != base
            || preview.preview_hash != hash
        {
            return Err(StateError::InvalidRecord(format!(
                "legacy integration batch {batch} has invalid preview evidence"
            )));
        }
        let sources = {
            let mut source_statement = connection.prepare(
                "SELECT task_id, checkpoint_id, verification_id, diff_sha256, source_json \
                 FROM integration_sources \
                 WHERE workspace_id = ?1 AND batch_id = ?2 ORDER BY source_order",
            )?;
            source_statement
                .query_map(params![RESERVED_LEGACY_WORKSPACE, batch], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        if sources.len() != preview.sources.len() {
            return Err(StateError::InvalidRecord(format!(
                "legacy integration batch {batch} source rows do not match its preview"
            )));
        }
        for (stored, expected) in sources.into_iter().zip(&preview.sources) {
            let source: IntegrationSource = serde_json::from_str(&stored.4)?;
            if source != *expected
                || source.task_id.to_string() != stored.0
                || source.checkpoint_id.to_string() != stored.1
                || source.verification_id.to_string() != stored.2
                || source.diff_sha256 != stored.3
            {
                return Err(StateError::InvalidRecord(format!(
                    "legacy integration batch {batch} contains inconsistent source evidence"
                )));
            }
        }
    }
    let hash_mismatches: i64 = connection.query_row(
        "SELECT ( \
             SELECT count(*) FROM integration_approvals AS approval \
             JOIN integration_batches AS batch \
               ON batch.workspace_id = approval.workspace_id AND batch.batch_id = approval.batch_id \
             WHERE approval.workspace_id = ?1 AND approval.preview_hash <> batch.preview_hash \
         ) + ( \
             SELECT count(*) FROM integration_applications AS application \
             JOIN integration_batches AS batch \
               ON batch.workspace_id = application.workspace_id AND batch.batch_id = application.batch_id \
             WHERE application.workspace_id = ?1 AND application.preview_hash <> batch.preview_hash \
         )",
        [RESERVED_LEGACY_WORKSPACE],
        |row| row.get(0),
    )?;
    if hash_mismatches != 0 {
        return Err(StateError::InvalidRecord(
            "legacy integration approvals or applications do not match their preview seals"
                .to_owned(),
        ));
    }
    Ok(())
}

fn rewrite_worker_results(
    transaction: &Transaction<'_>,
    mappings: &[IdMapping],
    source_fingerprint: &str,
) -> StateResult<()> {
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT attempt_id, worker_result_json FROM legacy_import_source.task_attempts \
             WHERE workspace_id = ?1 AND worker_result_json IS NOT NULL",
        )?;
        statement
            .query_map([RESERVED_LEGACY_WORKSPACE], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (source_id, result_json) in rows {
        let value: serde_json::Value = serde_json::from_str(&result_json)?;
        if value.get("schema_version").is_none() {
            continue;
        }
        let mut result: WorkerResult = serde_json::from_value(value)?;
        result.task_id = mapped_parse(mappings, "tasks.task_id", &result.task_id.to_string())?;
        result.attempt_id = mapped_parse(
            mappings,
            "task_attempts.attempt_id",
            &result.attempt_id.to_string(),
        )?;
        rewrite_command_paths_and_ids(&mut result.commands, mappings, source_fingerprint)?;
        for test in &mut result.tests {
            if let Some(command_id) = test.command_id {
                test.command_id = Some(mapped_parse(
                    mappings,
                    "command_evidence.command_id",
                    &command_id.to_string(),
                )?);
            }
        }
        transaction.execute(
            "UPDATE legacy_import_source.task_attempts SET worker_result_json = ?1 \
             WHERE workspace_id = ?2 AND attempt_id = ?3",
            params![
                serde_json::to_string(&result)?,
                RESERVED_LEGACY_WORKSPACE,
                source_id,
            ],
        )?;
    }
    Ok(())
}

fn rewrite_checkpoints(
    transaction: &Transaction<'_>,
    mappings: &[IdMapping],
    source_fingerprint: &str,
) -> StateResult<()> {
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT checkpoint_id, checkpoint_json \
             FROM legacy_import_source.checkpoints WHERE workspace_id = ?1",
        )?;
        statement
            .query_map([RESERVED_LEGACY_WORKSPACE], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (source_id, checkpoint_json) in rows {
        let mut checkpoint: Checkpoint = serde_json::from_str(&checkpoint_json)?;
        checkpoint.checkpoint_id = mapped_parse(
            mappings,
            "checkpoints.checkpoint_id",
            &checkpoint.checkpoint_id.to_string(),
        )?;
        checkpoint.task_id =
            mapped_parse(mappings, "tasks.task_id", &checkpoint.task_id.to_string())?;
        checkpoint.attempt_id = mapped_parse(
            mappings,
            "task_attempts.attempt_id",
            &checkpoint.attempt_id.to_string(),
        )?;
        namespace_optional_path(&mut checkpoint.diff_path, source_fingerprint)?;
        rewrite_command_paths_and_ids(&mut checkpoint.commands_run, mappings, source_fingerprint)?;
        for test in &mut checkpoint.tests {
            if let Some(command_id) = test.command_id {
                test.command_id = Some(mapped_parse(
                    mappings,
                    "command_evidence.command_id",
                    &command_id.to_string(),
                )?);
            }
        }
        checkpoint.refresh_integrity_hash().map_err(|error| {
            StateError::InvalidRecord(format!(
                "could not reseal imported checkpoint {source_id}: {error}"
            ))
        })?;
        transaction.execute(
            "UPDATE legacy_import_source.checkpoints \
             SET checkpoint_json = ?1, integrity_hash = ?2 \
             WHERE workspace_id = ?3 AND checkpoint_id = ?4",
            params![
                serde_json::to_string(&checkpoint)?,
                checkpoint.integrity_hash,
                RESERVED_LEGACY_WORKSPACE,
                source_id,
            ],
        )?;
    }
    Ok(())
}

fn rewrite_handovers(
    transaction: &Transaction<'_>,
    mappings: &[IdMapping],
    source_fingerprint: &str,
) -> StateResult<()> {
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT handover_id, bundle_json, acknowledgement_json \
             FROM legacy_import_source.handovers WHERE workspace_id = ?1",
        )?;
        statement
            .query_map([RESERVED_LEGACY_WORKSPACE], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (source_id, bundle_json, acknowledgement_json) in rows {
        let mut bundle: HandoverBundle = serde_json::from_str(&bundle_json)?;
        bundle.handover_id = mapped_parse(
            mappings,
            "handovers.handover_id",
            &bundle.handover_id.to_string(),
        )?;
        bundle.task_id = mapped_parse(mappings, "tasks.task_id", &bundle.task_id.to_string())?;
        namespace_optional_path(&mut bundle.diff_path, source_fingerprint)?;
        rewrite_command_paths_and_ids(&mut bundle.commands_run, mappings, source_fingerprint)?;
        for test in &mut bundle.tests {
            if let Some(command_id) = test.command_id {
                test.command_id = Some(mapped_parse(
                    mappings,
                    "command_evidence.command_id",
                    &command_id.to_string(),
                )?);
            }
        }
        bundle.refresh_integrity_hash().map_err(|error| {
            StateError::InvalidRecord(format!(
                "could not reseal imported handover {source_id}: {error}"
            ))
        })?;
        let acknowledgement_json = acknowledgement_json
            .map(|value| -> StateResult<String> {
                let mut acknowledgement: HandoverAcknowledgement = serde_json::from_str(&value)?;
                acknowledgement.task_id = bundle.task_id;
                acknowledgement
                    .bundle_hash
                    .clone_from(&bundle.integrity_hash);
                serde_json::to_string(&acknowledgement).map_err(StateError::from)
            })
            .transpose()?;
        transaction.execute(
            "UPDATE legacy_import_source.handovers \
             SET bundle_json = ?1, integrity_hash = ?2, acknowledgement_json = ?3 \
             WHERE workspace_id = ?4 AND handover_id = ?5",
            params![
                serde_json::to_string(&bundle)?,
                bundle.integrity_hash,
                acknowledgement_json,
                RESERVED_LEGACY_WORKSPACE,
                source_id,
            ],
        )?;
    }
    Ok(())
}

fn rewrite_verification_results(
    transaction: &Transaction<'_>,
    mappings: &[IdMapping],
    source_fingerprint: &str,
) -> StateResult<()> {
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT verification_id, result_json \
             FROM legacy_import_source.verification_results WHERE workspace_id = ?1",
        )?;
        statement
            .query_map([RESERVED_LEGACY_WORKSPACE], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (source_id, result_json) in rows {
        let mut result: VerificationResult = serde_json::from_str(&result_json)?;
        result.verification_id = mapped_parse(
            mappings,
            "verification_results.verification_id",
            &result.verification_id.to_string(),
        )?;
        result.task_id = mapped_parse(mappings, "tasks.task_id", &result.task_id.to_string())?;
        for check in &mut result.checks {
            for path in &mut check.evidence_paths {
                *path = namespace_path(path, source_fingerprint)?;
            }
        }
        transaction.execute(
            "UPDATE legacy_import_source.verification_results SET result_json = ?1 \
             WHERE workspace_id = ?2 AND verification_id = ?3",
            params![
                serde_json::to_string(&result)?,
                RESERVED_LEGACY_WORKSPACE,
                source_id,
            ],
        )?;
    }
    Ok(())
}

fn rewrite_graphs(
    transaction: &Transaction<'_>,
    mappings: &[IdMapping],
) -> StateResult<BTreeMap<String, String>> {
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT revision_id, proposal_hash, proposal_json, validation_json \
             FROM legacy_import_source.graph_revisions \
             WHERE workspace_id = ?1 AND proposal_json IS NOT NULL",
        )?;
        statement
            .query_map([RESERVED_LEGACY_WORKSPACE], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut hashes = BTreeMap::new();
    for (source_revision, old_hash, proposal_json, validation_json) in rows {
        let mut proposal: TaskGraphProposal = serde_json::from_str(&proposal_json)?;
        let mut validation: GraphValidationSummary = serde_json::from_str(&validation_json)?;
        proposal.revision_id = mapped_parse(
            mappings,
            "graph_revisions.revision_id",
            &proposal.revision_id.to_string(),
        )?;
        proposal.session_id = mapped_parse(
            mappings,
            "sessions.session_id",
            &proposal.session_id.to_string(),
        )?;
        proposal.goal_message_id = mapped_parse(
            mappings,
            "conversation_messages.message_id",
            &proposal.goal_message_id.to_string(),
        )?;
        if let Some(authority) = &mut validation.authority {
            authority.requirement_revision_id = mapped_parse(
                mappings,
                "requirement_revisions.requirement_revision_id",
                &authority.requirement_revision_id.to_string(),
            )?;
        }
        let new_hash = task_graph_proposal_hash(&proposal, &validation)
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        transaction.execute(
            "UPDATE legacy_import_source.graph_revisions \
             SET proposal_hash = ?1, proposal_json = ?2, validation_json = ?3 \
             WHERE workspace_id = ?4 AND revision_id = ?5",
            params![
                new_hash,
                serde_json::to_string(&proposal)?,
                serde_json::to_string(&validation)?,
                RESERVED_LEGACY_WORKSPACE,
                source_revision,
            ],
        )?;
        transaction.execute(
            "UPDATE legacy_import_source.graph_approvals SET proposal_hash = ?1 \
             WHERE workspace_id = ?2 AND revision_id = ?3",
            params![new_hash, RESERVED_LEGACY_WORKSPACE, source_revision],
        )?;
        hashes.insert(old_hash, new_hash);
    }
    Ok(hashes)
}

fn rewrite_integrations(
    transaction: &Transaction<'_>,
    mappings: &[IdMapping],
) -> StateResult<BTreeMap<String, String>> {
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT batch_id, preview_hash, preview_json \
             FROM legacy_import_source.integration_batches WHERE workspace_id = ?1",
        )?;
        statement
            .query_map([RESERVED_LEGACY_WORKSPACE], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut hashes = BTreeMap::new();
    for (source_batch, old_hash, preview_json) in rows {
        let preview: IntegrationPreview = serde_json::from_str(&preview_json)?;
        let batch_id = mapped_parse(
            mappings,
            "integration_batches.batch_id",
            &preview.batch_id.to_string(),
        )?;
        let session_id = mapped_parse(
            mappings,
            "sessions.session_id",
            &preview.session_id.to_string(),
        )?;
        let graph_revision_id = mapped_parse(
            mappings,
            "graph_revisions.revision_id",
            &preview.graph_revision_id.to_string(),
        )?;
        let sources = preview
            .sources
            .iter()
            .map(|source| remap_integration_source(source, mappings))
            .collect::<StateResult<Vec<_>>>()?;
        let blockers = preview
            .blockers
            .iter()
            .filter(|blocker| !matches!(blocker, IntegrationBlocker::PathOverlap { .. }))
            .map(|blocker| remap_integration_blocker(blocker, mappings))
            .collect::<StateResult<Vec<_>>>()?;
        let rewritten = IntegrationPreview::seal(
            batch_id,
            session_id,
            graph_revision_id,
            preview.base_revision,
            sources,
            blockers,
            preview.created_at,
        )
        .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        transaction.execute(
            "UPDATE legacy_import_source.integration_batches \
             SET preview_hash = ?1, preview_json = ?2 \
             WHERE workspace_id = ?3 AND batch_id = ?4",
            params![
                rewritten.preview_hash,
                serde_json::to_string(&rewritten)?,
                RESERVED_LEGACY_WORKSPACE,
                source_batch,
            ],
        )?;
        for (index, source) in rewritten.sources.iter().enumerate() {
            transaction.execute(
                "UPDATE legacy_import_source.integration_sources SET source_json = ?1 \
                 WHERE workspace_id = ?2 AND batch_id = ?3 AND source_order = ?4",
                params![
                    serde_json::to_string(source)?,
                    RESERVED_LEGACY_WORKSPACE,
                    source_batch,
                    i64::try_from(index + 1).map_err(|_| StateError::InvalidRecord(
                        "integration source ordinal overflow".to_owned()
                    ))?,
                ],
            )?;
        }
        for table in ["integration_approvals", "integration_applications"] {
            let sql = format!(
                "UPDATE legacy_import_source.\"{table}\" SET preview_hash = ?1 \
                 WHERE workspace_id = ?2 AND batch_id = ?3"
            );
            transaction.execute(
                &sql,
                params![
                    rewritten.preview_hash,
                    RESERVED_LEGACY_WORKSPACE,
                    source_batch
                ],
            )?;
        }
        hashes.insert(old_hash, rewritten.preview_hash);
    }
    Ok(hashes)
}

fn remap_integration_source(
    source: &IntegrationSource,
    mappings: &[IdMapping],
) -> StateResult<IntegrationSource> {
    Ok(IntegrationSource {
        task_id: mapped_parse(mappings, "tasks.task_id", &source.task_id.to_string())?,
        checkpoint_id: mapped_parse(
            mappings,
            "checkpoints.checkpoint_id",
            &source.checkpoint_id.to_string(),
        )?,
        verification_id: mapped_parse(
            mappings,
            "verification_results.verification_id",
            &source.verification_id.to_string(),
        )?,
        base_revision: source.base_revision.clone(),
        diff_sha256: source.diff_sha256.clone(),
        changed_files: source.changed_files.clone(),
    })
}

fn remap_integration_blocker(
    blocker: &IntegrationBlocker,
    mappings: &[IdMapping],
) -> StateResult<IntegrationBlocker> {
    let task = |task_id: orchestrator_domain::TaskId| {
        mapped_parse(mappings, "tasks.task_id", &task_id.to_string())
    };
    Ok(match blocker {
        IntegrationBlocker::MissingEvidence { task_id, detail } => {
            IntegrationBlocker::MissingEvidence {
                task_id: task(*task_id)?,
                detail: detail.clone(),
            }
        }
        IntegrationBlocker::VerificationFailed { task_id } => {
            IntegrationBlocker::VerificationFailed {
                task_id: task(*task_id)?,
            }
        }
        IntegrationBlocker::StaleBase { task_id, found } => IntegrationBlocker::StaleBase {
            task_id: task(*task_id)?,
            found: found.clone(),
        },
        IntegrationBlocker::SourceChanged { task_id } => IntegrationBlocker::SourceChanged {
            task_id: task(*task_id)?,
        },
        IntegrationBlocker::PathOverlap { left, right, path } => IntegrationBlocker::PathOverlap {
            left: task(*left)?,
            right: task(*right)?,
            path: path.clone(),
        },
        IntegrationBlocker::PatchFailed { task_id, detail } => IntegrationBlocker::PatchFailed {
            task_id: task(*task_id)?,
            detail: detail.clone(),
        },
    })
}

fn rewrite_remaining_json(
    transaction: &Transaction<'_>,
    mappings: &[IdMapping],
    graph_hashes: &BTreeMap<String, String>,
    integration_hashes: &BTreeMap<String, String>,
) -> StateResult<()> {
    rewrite_task_envelopes(transaction, mappings)?;
    rewrite_client_command_payloads(transaction, mappings, graph_hashes, integration_hashes)
}

fn rewrite_task_envelopes(
    transaction: &Transaction<'_>,
    mappings: &[IdMapping],
) -> StateResult<()> {
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT task_id, task_envelope_json FROM legacy_import_source.tasks \
             WHERE workspace_id = ?1",
        )?;
        statement
            .query_map([RESERVED_LEGACY_WORKSPACE], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (source_id, json) in rows {
        let mut envelope: TaskEnvelope = serde_json::from_str(&json)?;
        envelope.task_id = mapped_parse(mappings, "tasks.task_id", &envelope.task_id.to_string())?;
        transaction.execute(
            "UPDATE legacy_import_source.tasks SET task_envelope_json = ?1 \
             WHERE workspace_id = ?2 AND task_id = ?3",
            params![
                serde_json::to_string(&envelope)?,
                RESERVED_LEGACY_WORKSPACE,
                source_id,
            ],
        )?;
    }
    Ok(())
}

fn rewrite_client_command_payloads(
    transaction: &Transaction<'_>,
    mappings: &[IdMapping],
    graph_hashes: &BTreeMap<String, String>,
    integration_hashes: &BTreeMap<String, String>,
) -> StateResult<()> {
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT command_id, action, payload_json \
             FROM legacy_import_source.client_commands WHERE workspace_id = ?1",
        )?;
        statement
            .query_map([RESERVED_LEGACY_WORKSPACE], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (source_id, action, payload_json) in rows {
        let payload = rewrite_client_command_payload(
            &action,
            &payload_json,
            mappings,
            graph_hashes,
            integration_hashes,
        )?;
        transaction.execute(
            "UPDATE legacy_import_source.client_commands SET payload_json = ?1 \
             WHERE workspace_id = ?2 AND command_id = ?3",
            params![payload, RESERVED_LEGACY_WORKSPACE, source_id],
        )?;
    }
    Ok(())
}

fn rewrite_client_command_payload(
    action: &str,
    payload_json: &str,
    mappings: &[IdMapping],
    graph_hashes: &BTreeMap<String, String>,
    integration_hashes: &BTreeMap<String, String>,
) -> StateResult<String> {
    match action {
        "create_session" => {
            let mut payload: CreateSessionCommandPayload = serde_json::from_str(payload_json)?;
            payload.session_id = mapped_parse(
                mappings,
                "sessions.session_id",
                &payload.session_id.to_string(),
            )?;
            serde_json::to_string(&payload).map_err(StateError::from)
        }
        "append_message" => {
            let mut payload: AppendMessageCommandPayload = serde_json::from_str(payload_json)?;
            payload.message_id = mapped_parse(
                mappings,
                "conversation_messages.message_id",
                &payload.message_id.to_string(),
            )?;
            serde_json::to_string(&payload).map_err(StateError::from)
        }
        "request_conversation_turn" => {
            let mut payload: RequestConversationTurnCommandPayload =
                serde_json::from_str(payload_json)?;
            payload.source_message_id = mapped_parse(
                mappings,
                "conversation_messages.message_id",
                &payload.source_message_id.to_string(),
            )?;
            serde_json::to_string(&payload).map_err(StateError::from)
        }
        "request_plan" => {
            let mut payload: RequestPlanCommandPayload = serde_json::from_str(payload_json)?;
            payload.goal_message_id = mapped_parse(
                mappings,
                "conversation_messages.message_id",
                &payload.goal_message_id.to_string(),
            )?;
            serde_json::to_string(&payload).map_err(StateError::from)
        }
        "approve_graph" => {
            let mut payload: ApproveGraphCommandPayload = serde_json::from_str(payload_json)?;
            payload.revision_id = mapped_parse(
                mappings,
                "graph_revisions.revision_id",
                &payload.revision_id.to_string(),
            )?;
            payload.requirement_revision_id = mapped_parse(
                mappings,
                "requirement_revisions.requirement_revision_id",
                &payload.requirement_revision_id.to_string(),
            )?;
            if let Some(hash) = graph_hashes.get(&payload.proposal_hash) {
                payload.proposal_hash.clone_from(hash);
            }
            serde_json::to_string(&payload).map_err(StateError::from)
        }
        "approve_integration" => {
            let mut payload: ApproveIntegrationCommandPayload = serde_json::from_str(payload_json)?;
            payload.batch_id = mapped_parse(
                mappings,
                "integration_batches.batch_id",
                &payload.batch_id.to_string(),
            )?;
            if let Some(hash) = integration_hashes.get(&payload.preview_hash) {
                payload.preview_hash.clone_from(hash);
            }
            serde_json::to_string(&payload).map_err(StateError::from)
        }
        "create_resolution_task" => {
            let mut payload: CreateResolutionTaskCommandPayload =
                serde_json::from_str(payload_json)?;
            payload.batch_id = mapped_parse(
                mappings,
                "integration_batches.batch_id",
                &payload.batch_id.to_string(),
            )?;
            serde_json::to_string(&payload).map_err(StateError::from)
        }
        "stop_daemon" | "revise_graph" | "cancel_plan" | "request_integration" => {
            let _: serde_json::Value = serde_json::from_str(payload_json)?;
            Ok(payload_json.to_owned())
        }
        _ => Err(StateError::InvalidRecord(format!(
            "legacy client command action `{action}` is unsupported"
        ))),
    }
}

fn rewrite_command_paths_and_ids(
    commands: &mut [CommandEvidence],
    mappings: &[IdMapping],
    source_fingerprint: &str,
) -> StateResult<()> {
    for command in commands {
        command.id = mapped_parse(
            mappings,
            "command_evidence.command_id",
            &command.id.to_string(),
        )?;
        namespace_optional_path(&mut command.stdout_artifact, source_fingerprint)?;
        namespace_optional_path(&mut command.stderr_artifact, source_fingerprint)?;
    }
    Ok(())
}

fn namespace_optional_path(
    path: &mut Option<RepoPath>,
    source_fingerprint: &str,
) -> StateResult<()> {
    if let Some(value) = path {
        *value = namespace_path(value, source_fingerprint)?;
    }
    Ok(())
}

fn namespace_path(path: &RepoPath, source_fingerprint: &str) -> StateResult<RepoPath> {
    RepoPath::try_from(format!("imports/{source_fingerprint}/{path}"))
        .map_err(|error| StateError::InvalidRecord(error.to_string()))
}

fn mapped_parse<T>(mappings: &[IdMapping], entity_type: &str, source_id: &str) -> StateResult<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let target_id = mappings
        .iter()
        .find(|mapping| mapping.entity_type == entity_type && mapping.source_id == source_id)
        .map_or(source_id, |mapping| mapping.target_id.as_str());
    target_id.parse().map_err(|error| {
        StateError::InvalidRecord(format!(
            "legacy mapped {entity_type} identifier `{target_id}` is invalid: {error}"
        ))
    })
}

fn ensure_target_database(global: &Database, target: WorkspaceId) -> StateResult<()> {
    let status = global.migration_status()?;
    if status.current_version != crate::STATE_SCHEMA_VERSION {
        return Err(StateError::InvalidRecord(format!(
            "global database schema {} is not ready for legacy import; expected {}",
            status.current_version,
            crate::STATE_SCHEMA_VERSION
        )));
    }
    let connection = global.raw_lock()?;
    let exists = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_id = ?1)",
        [target.to_string()],
        |row| row.get::<_, bool>(0),
    )?;
    if !exists {
        return Err(StateError::WorkspaceNotFound {
            workspace_id: target.to_string(),
        });
    }
    Ok(())
}

fn load_existing_result(
    global: &Database,
    target: WorkspaceId,
    plan: &LegacyImportPlan,
) -> StateResult<Option<LegacyImportResult>> {
    let connection = global.raw_lock()?;
    load_existing_result_in(&connection, target, plan)
}

fn load_existing_result_in(
    connection: &Connection,
    target: WorkspaceId,
    plan: &LegacyImportPlan,
) -> StateResult<Option<LegacyImportResult>> {
    let row: Option<(String, String, String, String)> = connection
        .query_row(
            "SELECT workspace_id, manifest_hash, imported_at, result_json FROM main.legacy_imports \
             WHERE source_fingerprint = ?1",
            [&plan.source_fingerprint],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((workspace_id, manifest_hash, imported_at, result_json)) = row else {
        return Ok(None);
    };
    if workspace_id != target.to_string() {
        return Err(StateError::InvalidRecord(
            "legacy import fingerprint is already recorded for a different workspace".to_owned(),
        ));
    }
    let mut result: LegacyImportResult = serde_json::from_str(&result_json)?;
    if !result.imported
        || result.source_fingerprint != plan.source_fingerprint
        || result.workspace_id != target
        || result.manifest_hash != manifest_hash
        || result.imported_at.to_rfc3339() != imported_at
        || result.legacy_event_count != plan.event_evidence.count
        || result.legacy_event_root_hash != plan.event_evidence.root_hash
        || result.legacy_event_tip_hash != plan.event_evidence.tip_hash
    {
        return Err(StateError::InvalidRecord(
            "legacy import ledger result does not match its indexed columns".to_owned(),
        ));
    }
    validate_replayed_audit(connection, &result)?;
    result.imported = false;
    Ok(Some(result))
}

fn validate_replayed_audit(
    connection: &Connection,
    result: &LegacyImportResult,
) -> StateResult<()> {
    validate_replayed_mappings(connection, result)?;
    validate_replayed_event_evidence(connection, result)?;
    validate_replayed_anchor(connection, result)
}

fn validate_replayed_mappings(
    connection: &Connection,
    result: &LegacyImportResult,
) -> StateResult<()> {
    let mut statement = connection.prepare(
        "SELECT entity_type, source_id, target_id \
         FROM main.legacy_import_id_mappings \
         WHERE source_fingerprint = ?1 AND workspace_id = ?2 \
         ORDER BY entity_type, source_id, target_id",
    )?;
    let mappings = statement
        .query_map(
            params![result.source_fingerprint, result.workspace_id.to_string()],
            |row| {
                Ok(IdMapping {
                    entity_type: row.get(0)?,
                    source_id: row.get(1)?,
                    target_id: row.get(2)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    if u64::try_from(mappings.len()).unwrap_or(u64::MAX) != result.id_mapping_count
        || id_mapping_manifest_hash(&mappings) != result.id_mapping_manifest_hash
    {
        return Err(StateError::InvalidRecord(
            "legacy import ID mapping ledger no longer matches its sealed result".to_owned(),
        ));
    }
    Ok(())
}

fn validate_replayed_event_evidence(
    connection: &Connection,
    result: &LegacyImportResult,
) -> StateResult<()> {
    let workspace_id = result.workspace_id.to_string();
    let events = validate_event_chain_for_workspace(connection, &workspace_id)?;
    if result.copied_legacy_event_chain {
        let legacy_count = usize::try_from(result.legacy_event_count).map_err(|_| {
            StateError::InvalidRecord("legacy event count exceeds memory range".to_owned())
        })?;
        let prefix = events.get(..legacy_count).ok_or_else(|| {
            StateError::InvalidRecord("imported legacy event prefix is missing".to_owned())
        })?;
        let evidence = event_evidence(prefix)?;
        if evidence.count != result.legacy_event_count
            || evidence.root_hash != result.legacy_event_root_hash
            || evidence.tip_hash != result.legacy_event_tip_hash
        {
            return Err(StateError::InvalidRecord(
                "imported legacy event chain no longer matches its sealed root and tip".to_owned(),
            ));
        }
    } else if result.anchor_sequence.is_none() {
        return Err(StateError::InvalidRecord(
            "merged legacy import result omits its audit anchor".to_owned(),
        ));
    }
    Ok(())
}

fn validate_replayed_anchor(
    connection: &Connection,
    result: &LegacyImportResult,
) -> StateResult<()> {
    let anchor_count: i64 = connection.query_row(
        "SELECT count(*) FROM main.task_events \
         WHERE workspace_id = ?1 \
           AND json_extract(event_json, '$.payload.kind') = 'legacy_import_anchor' \
           AND json_extract(event_json, '$.payload.source_fingerprint') = ?2",
        params![result.workspace_id.to_string(), result.source_fingerprint],
        |row| row.get(0),
    )?;
    let Some(sequence) = result.anchor_sequence else {
        if anchor_count != 0 || result.id_mapping_count != 0 {
            return Err(StateError::InvalidRecord(
                "legacy import result omits its durable audit anchor".to_owned(),
            ));
        }
        return Ok(());
    };
    if anchor_count != 1 {
        return Err(StateError::InvalidRecord(
            "legacy import audit anchor is missing or duplicated".to_owned(),
        ));
    }
    let (event_json, stored_hash): (String, String) = connection.query_row(
        "SELECT event_json, event_hash FROM main.task_events \
         WHERE workspace_id = ?1 AND sequence = ?2",
        params![
            result.workspace_id.to_string(),
            i64::try_from(sequence).map_err(|_| StateError::InvalidRecord(
                "legacy import anchor sequence exceeds SQLite range".to_owned()
            ))?
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let event: TaskEvent = serde_json::from_str(&event_json)?;
    let payload = &event.payload;
    let valid = event.event_type == EventType::CompatibilityWarning
        && event.sequence == sequence
        && event.event_hash == stored_hash
        && event
            .verify_hash()
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?
        && payload.get("kind") == Some(&json!("legacy_import_anchor"))
        && payload.get("source_fingerprint") == Some(&json!(result.source_fingerprint))
        && payload.get("source_root_hash") == Some(&json!(result.source_root_hash))
        && payload.get("manifest_hash") == Some(&json!(result.manifest_hash))
        && payload.get("legacy_event_count") == Some(&json!(result.legacy_event_count))
        && payload.get("legacy_event_root_hash") == Some(&json!(result.legacy_event_root_hash))
        && payload.get("legacy_event_tip_hash") == Some(&json!(result.legacy_event_tip_hash))
        && payload.get("copied_legacy_event_chain")
            == Some(&json!(result.copied_legacy_event_chain))
        && payload.get("id_mapping_count") == Some(&json!(result.id_mapping_count))
        && payload.get("id_mapping_manifest_hash") == Some(&json!(result.id_mapping_manifest_hash));
    if !valid {
        return Err(StateError::InvalidRecord(
            "legacy import anchor no longer matches its sealed result".to_owned(),
        ));
    }
    Ok(())
}

fn validate_published_import(
    result: &LegacyImportResult,
    target: WorkspaceId,
    paths: &GlobalStatePaths,
) -> StateResult<()> {
    let expected = paths
        .for_workspace(target)
        .root
        .join("imports")
        .join(&result.source_fingerprint);
    if result.published_path != expected {
        return Err(StateError::InvalidRecord(
            "legacy import ledger points outside its fingerprint namespace".to_owned(),
        ));
    }
    reject_symlink_components(&expected)?;
    let database = expected.join(SOURCE_SNAPSHOT_NAME);
    let database_sha256 = sha256_file(&database)?;
    let database_length = file_length(&database)?;
    let files = manifest_files_below(&expected, Some(SOURCE_SNAPSHOT_NAME))?;
    let observed = manifest_hash(&database_sha256, database_length, &files);
    if observed != result.manifest_hash {
        return Err(StateError::InvalidRecord(
            "published legacy import no longer matches its sealed manifest".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_plan_unchanged(
    expected: &LegacyImportPlan,
    actual: &LegacyImportPlan,
) -> StateResult<()> {
    if expected.source_schema_version != actual.source_schema_version
        || expected.source_fingerprint != actual.source_fingerprint
        || expected.source_root_hash != actual.source_root_hash
        || expected.manifest_hash != actual.manifest_hash
        || expected.event_evidence != actual.event_evidence
        || expected.database_sha256 != actual.database_sha256
        || expected.database_length != actual.database_length
        || expected.files != actual.files
    {
        return Err(StateError::InvalidRecord(
            "legacy source changed after inspection".to_owned(),
        ));
    }
    Ok(())
}

fn open_source_read_only(root: &Path, path: &Path) -> StateResult<GuardedSourceConnection> {
    let guard = SourceOpenGuard::open(root, path)?;
    let sqlite_path = guard.sqlite_path()?;
    let connection = Connection::open_with_flags(
        &sqlite_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    guard.revalidate()?;
    connection.busy_timeout(Duration::ZERO)?;
    connection.pragma_update(None, "query_only", true)?;
    guard.revalidate()?;
    Ok(GuardedSourceConnection { connection, guard })
}

fn validate_integrity(connection: &Connection, label: &str) -> StateResult<()> {
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(StateError::InvalidRecord(format!(
            "{label} integrity_check returned `{integrity}`"
        )));
    }
    let foreign_keys: i64 =
        connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_keys != 0 {
        return Err(StateError::InvalidRecord(format!(
            "{label} contains {foreign_keys} foreign-key violations"
        )));
    }
    Ok(())
}

fn reject_live_legacy_daemon(connection: &Connection, schema_version: u32) -> StateResult<()> {
    if schema_version < 4 || !table_exists(connection, "daemon_instances")? {
        return Ok(());
    }
    let mut statement = connection.prepare(
        "SELECT instance_id, lease_expires_at FROM daemon_instances WHERE released_at IS NULL",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (instance_id, expires_at) = row?;
        let expires_at = DateTime::parse_from_rfc3339(&expires_at)
            .map_err(|error| {
                StateError::InvalidRecord(format!(
                    "legacy daemon {instance_id} has invalid lease timestamp: {error}"
                ))
            })?
            .with_timezone(&Utc);
        if expires_at > Utc::now() {
            return Err(StateError::RollbackGuard(format!(
                "legacy state is owned by live daemon {instance_id} until {expires_at}; stop it before importing"
            )));
        }
    }
    Ok(())
}

fn backup_stable_source(source: &Connection, destination: &Path) -> StateResult<()> {
    if destination.exists() {
        return Err(StateError::ArtifactConflict(destination.to_path_buf()));
    }
    let before: i64 = source.pragma_query_value(None, "data_version", |row| row.get(0))?;
    source.backup(MAIN_DB, destination, None)?;
    ensure_private_file(destination)?;
    let after: i64 = source.pragma_query_value(None, "data_version", |row| row.get(0))?;
    if before != after {
        return Err(StateError::RollbackGuard(
            "legacy database changed while its read-only snapshot was created".to_owned(),
        ));
    }
    Ok(())
}

fn migrate_snapshot(source: &Path, destination: &Path) -> StateResult<Connection> {
    fs::copy(source, destination).map_err(|error| StateError::io(destination, error))?;
    ensure_private_file(destination)?;
    let mut connection = Connection::open(destination)?;
    crate::migrations::configure_connection(&connection)?;
    MigrationManager::apply(&mut connection)?;
    validate_integrity(&connection, "migrated legacy snapshot")?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(connection)
}

fn prepare_rewrite_scratch(validated: &Path, scratch: &Path) -> StateResult<()> {
    if scratch.exists() {
        return Err(StateError::ArtifactConflict(scratch.to_path_buf()));
    }
    fs::copy(validated, scratch).map_err(|error| StateError::io(scratch, error))?;
    ensure_private_file(scratch)?;
    let connection = Connection::open(scratch)?;
    connection.execute_batch("PRAGMA journal_mode = DELETE;")?;
    validate_integrity(&connection, "legacy rewrite scratch before trigger removal")?;
    validate_and_drop_rewrite_triggers(&connection)?;
    validate_integrity(&connection, "legacy rewrite scratch after trigger removal")?;
    connection.execute_batch("PRAGMA journal_mode = DELETE;")?;
    drop(connection);
    ensure_private_file(scratch)
}

fn validate_and_drop_rewrite_triggers(connection: &Connection) -> StateResult<()> {
    for (name, expected_sql) in REWRITE_TRIGGERS {
        let observed: Option<String> = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        let observed = observed.ok_or_else(|| {
            StateError::InvalidRecord(format!(
                "legacy rewrite scratch is missing required immutable trigger {name}"
            ))
        })?;
        if normalize_sql(&observed) != normalize_sql(expected_sql) {
            return Err(StateError::InvalidRecord(format!(
                "legacy rewrite scratch immutable trigger {name} does not match migration 13"
            )));
        }
    }
    let transaction = connection.unchecked_transaction()?;
    for (name, _) in REWRITE_TRIGGERS {
        transaction.execute_batch(&format!("DROP TRIGGER \"{name}\";"))?;
    }
    transaction.commit()?;
    Ok(())
}

fn normalize_sql(sql: &str) -> String {
    sql.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

fn legacy_workspace_exists(connection: &Connection) -> StateResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_id = ?1)",
            [RESERVED_LEGACY_WORKSPACE],
            |row| row.get(0),
        )
        .map_err(StateError::from)
}

fn workspace_row_count(connection: &Connection) -> StateResult<u64> {
    let mut total = 0_u64;
    for table in IMPORT_TABLES {
        let sql = format!("SELECT count(*) FROM \"{table}\"");
        let count: i64 = connection.query_row(&sql, [], |row| row.get(0))?;
        total =
            total
                .checked_add(u64::try_from(count).map_err(|_| {
                    StateError::InvalidRecord("negative legacy row count".to_owned())
                })?)
                .ok_or_else(|| StateError::InvalidRecord("legacy row count overflow".to_owned()))?;
    }
    Ok(total)
}

fn validate_event_chain(connection: &Connection) -> StateResult<Vec<TaskEvent>> {
    validate_event_chain_for_workspace(connection, RESERVED_LEGACY_WORKSPACE)
}

fn validate_event_chain_for_workspace(
    connection: &Connection,
    workspace_id: &str,
) -> StateResult<Vec<TaskEvent>> {
    let task_count: i64 = connection.query_row(
        "SELECT count(*) FROM tasks WHERE workspace_id = ?1",
        [workspace_id],
        |row| row.get(0),
    )?;
    let mut statement = connection.prepare(
        "SELECT sequence, event_id, task_id, event_type, schema_version, occurred_at, event_json, \
                previous_hash, event_hash, session_id \
         FROM task_events WHERE workspace_id = ?1 ORDER BY sequence",
    )?;
    let rows = statement.query_map([workspace_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, Option<String>>(9)?,
        ))
    })?;
    let mut events = Vec::new();
    let mut previous_hash: Option<String> = None;
    for row in rows {
        let (
            sequence,
            event_id,
            task_id,
            event_type,
            schema_version,
            occurred_at,
            event_json,
            stored_previous_hash,
            event_hash,
            session_id,
        ) = row?;
        let event: TaskEvent =
            serde_json::from_str(&event_json).map_err(|error| StateError::InvalidEventChain {
                sequence,
                reason: format!("event JSON is invalid: {error}"),
            })?;
        let expected_sequence =
            i64::try_from(events.len()).map_err(|_| StateError::InvalidEventChain {
                sequence,
                reason: "event count exceeds SQLite range".to_owned(),
            })? + 1;
        let event_type_json = serde_json::to_value(event.event_type)?;
        let event_type_value = event_type_json.as_str().unwrap_or_default();
        if sequence != expected_sequence
            || event.sequence != u64::try_from(sequence).unwrap_or(u64::MAX)
            || event.event_id.to_string() != event_id
            || event.task_id.map(|value| value.to_string()) != task_id
            || event.session_id.map(|value| value.to_string()) != session_id
            || event_type_value != event_type
            || event.schema_version.as_str() != schema_version
            || event.occurred_at.to_rfc3339() != occurred_at
            || stored_previous_hash != previous_hash
            || event.previous_hash != previous_hash
            || event.event_hash != event_hash
        {
            return Err(StateError::InvalidEventChain {
                sequence,
                reason: "event columns, JSON, sequence, or predecessor do not agree".to_owned(),
            });
        }
        if !event
            .verify_hash()
            .map_err(|error| StateError::InvalidEventChain {
                sequence,
                reason: error.to_string(),
            })?
        {
            return Err(StateError::InvalidEventChain {
                sequence,
                reason: "event hash verification failed".to_owned(),
            });
        }
        previous_hash = Some(event_hash);
        events.push(event);
    }
    if task_count > 0 && events.is_empty() {
        return Err(StateError::InvalidEventChain {
            sequence: 0,
            reason: "workspace projections exist without an audit event chain".to_owned(),
        });
    }
    Ok(events)
}

fn event_evidence(events: &[TaskEvent]) -> StateResult<LegacyEventEvidence> {
    Ok(LegacyEventEvidence {
        count: u64::try_from(events.len())
            .map_err(|_| StateError::InvalidRecord("legacy event count overflow".to_owned()))?,
        root_hash: events.first().map(|event| event.event_hash.clone()),
        tip_hash: events.last().map(|event| event.event_hash.clone()),
    })
}

fn validate_event_log_cursor(
    connection: &Connection,
    workspace_id: &str,
    events: &[TaskEvent],
) -> StateResult<()> {
    let cursor: Option<(i64, Option<String>)> = connection
        .query_row(
            "SELECT last_exported_sequence, last_exported_hash FROM event_log_state \
             WHERE workspace_id = ?1",
            [workspace_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((sequence, stored_hash)) = cursor else {
        return Ok(());
    };
    let index = usize::try_from(sequence).map_err(|_| StateError::InvalidEventChain {
        sequence,
        reason: "workspace event cursor sequence is negative".to_owned(),
    })?;
    let expected_hash = index
        .checked_sub(1)
        .and_then(|event_index| events.get(event_index))
        .map(|event| event.event_hash.clone());
    if index > events.len() || (index == 0) != stored_hash.is_none() || stored_hash != expected_hash
    {
        return Err(StateError::InvalidEventChain {
            sequence,
            reason: "workspace event cursor does not identify an exact event-chain prefix"
                .to_owned(),
        });
    }
    Ok(())
}

fn validate_jsonl_evidence(
    root: &Path,
    path: &Path,
    connection: &Connection,
    database_events: &[TaskEvent],
) -> StateResult<()> {
    let (exported_sequence, exported_hash): (i64, Option<String>) = connection.query_row(
        "SELECT last_exported_sequence, last_exported_hash FROM event_log_state \
         WHERE workspace_id = ?1",
        [RESERVED_LEGACY_WORKSPACE],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let exported_sequence = usize::try_from(exported_sequence).map_err(|_| {
        StateError::InvalidRecord("legacy JSONL export sequence is negative".to_owned())
    })?;
    if exported_sequence > database_events.len()
        || (exported_sequence == 0) != exported_hash.is_none()
        || exported_hash
            != exported_sequence
                .checked_sub(1)
                .and_then(|index| database_events.get(index))
                .map(|event| event.event_hash.clone())
    {
        return Err(StateError::InvalidEventChain {
            sequence: i64::try_from(exported_sequence).unwrap_or(i64::MAX),
            reason: "JSONL export state does not seal a valid SQLite event prefix".to_owned(),
        });
    }
    if !path.exists() {
        if exported_sequence == 0 {
            return Ok(());
        }
        return Err(StateError::InvalidEventChain {
            sequence: i64::try_from(exported_sequence).unwrap_or(i64::MAX),
            reason: "JSONL export state declares evidence but the JSONL file is missing".to_owned(),
        });
    }
    let bytes = read_contained_source(root, path)?;
    if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        return Err(StateError::TornEventLogTail);
    }
    let lines = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() != exported_sequence {
        return Err(StateError::InvalidEventChain {
            sequence: i64::try_from(lines.len()).unwrap_or(i64::MAX),
            reason: "JSONL evidence length differs from event_log_state".to_owned(),
        });
    }
    for (index, line) in lines.into_iter().enumerate() {
        let event: TaskEvent = serde_json::from_slice(line)?;
        let sequence = i64::try_from(index + 1).unwrap_or(i64::MAX);
        if database_events.get(index) != Some(&event) {
            return Err(StateError::InvalidEventChain {
                sequence,
                reason: "JSONL evidence differs from the SQLite event chain".to_owned(),
            });
        }
    }
    Ok(())
}

fn collect_manifest_files(
    source: &RepositoryStatePaths,
    migrated: &Connection,
) -> StateResult<Vec<ManifestFile>> {
    let mut relative_paths = BTreeSet::new();
    for directory in [&source.tasks, &source.checkpoints, &source.handovers] {
        if directory.exists() {
            collect_relative_files(&source.root, directory, &mut relative_paths)?;
        }
    }
    if source.events.exists() {
        relative_paths.insert(relative_from_root(&source.root, &source.events)?);
    }
    let mut statement = migrated.prepare(
        "SELECT relative_path, sha256, byte_length FROM artifacts \
         WHERE workspace_id = ?1 ORDER BY relative_path",
    )?;
    let rows = statement.query_map([RESERVED_LEGACY_WORKSPACE], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (path, stored_sha256, stored_length) = row?;
        let path = RepoPath::try_from(path).map_err(|error| {
            StateError::InvalidRecord(format!("legacy artifact path is unsafe: {error}"))
        })?;
        let relative_path = PathBuf::from(path.to_string());
        let actual = manifest_file(&source.root, relative_path.clone())?;
        let stored_length = u64::try_from(stored_length).map_err(|_| {
            StateError::InvalidRecord(format!(
                "legacy artifact metadata has a negative byte length: {path}"
            ))
        })?;
        if actual.sha256 != stored_sha256 || actual.byte_length != stored_length {
            return Err(StateError::InvalidRecord(format!(
                "legacy artifact metadata does not match source bytes: {path}"
            )));
        }
        relative_paths.insert(relative_path);
    }
    if relative_paths.contains(Path::new(SOURCE_SNAPSHOT_NAME)) {
        return Err(StateError::InvalidRecord(
            "legacy artifact path collides with reserved import snapshot name".to_owned(),
        ));
    }
    relative_paths
        .into_iter()
        .map(|relative_path| manifest_file(&source.root, relative_path))
        .collect()
}

fn collect_relative_files(
    root: &Path,
    directory: &Path,
    paths: &mut BTreeSet<PathBuf>,
) -> StateResult<()> {
    reject_symlink_components(root)?;
    reject_symlink_components(directory)?;
    let metadata =
        fs::symlink_metadata(directory).map_err(|error| StateError::io(directory, error))?;
    if metadata.file_type().is_symlink() {
        return Err(StateError::SymlinkEscape(directory.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Err(StateError::InvalidRecord(format!(
            "legacy artifact directory is not a directory: {}",
            directory.display()
        )));
    }
    let mut entries = fs::read_dir(directory)
        .map_err(|error| StateError::io(directory, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| StateError::io(directory, error))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        reject_symlink_components(&path)?;
        let metadata = fs::symlink_metadata(&path).map_err(|error| StateError::io(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(StateError::SymlinkEscape(path));
        }
        if metadata.is_dir() {
            collect_relative_files(root, &path, paths)?;
        } else if metadata.is_file() {
            paths.insert(relative_from_root(root, &path)?);
        } else {
            return Err(StateError::InvalidRecord(format!(
                "legacy import encountered a non-regular file: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn relative_from_root(root: &Path, path: &Path) -> StateResult<PathBuf> {
    let relative = path.strip_prefix(root).map_err(|_| {
        StateError::InvalidRecord(format!(
            "legacy import path escapes state root: {}",
            path.display()
        ))
    })?;
    validate_relative(relative)?;
    Ok(relative.to_path_buf())
}

fn validate_relative(path: &Path) -> StateResult<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(StateError::InvalidRecord(format!(
            "legacy import path is unsafe: {}",
            path.display()
        )));
    }
    Ok(())
}

fn manifest_file(root: &Path, relative_path: PathBuf) -> StateResult<ManifestFile> {
    validate_relative(&relative_path)?;
    let path = root.join(&relative_path);
    let bytes = read_contained_source(root, &path)?;
    Ok(ManifestFile {
        relative_path,
        sha256: hex::encode(Sha256::digest(&bytes)),
        byte_length: u64::try_from(bytes.len())
            .map_err(|_| StateError::InvalidRecord("legacy artifact is too large".to_owned()))?,
    })
}

fn read_contained_source(root: &Path, path: &Path) -> StateResult<Vec<u8>> {
    SourceOpenGuard::open(root, path)?.read_all()
}

fn manifest_files_below(root: &Path, excluded: Option<&str>) -> StateResult<Vec<ManifestFile>> {
    let mut paths = BTreeSet::new();
    collect_relative_files(root, root, &mut paths)?;
    paths
        .into_iter()
        .filter(|path| excluded.is_none_or(|name| path != Path::new(name)))
        .map(|path| manifest_file(root, path))
        .collect()
}

fn manifest_hash(database_sha256: &str, database_length: u64, files: &[ManifestFile]) -> String {
    let mut entries = BTreeMap::new();
    entries.insert(
        SOURCE_SNAPSHOT_NAME.to_owned(),
        (database_sha256.to_owned(), database_length),
    );
    for file in files {
        entries.insert(
            file.relative_path.to_string_lossy().replace('\\', "/"),
            (file.sha256.clone(), file.byte_length),
        );
    }
    let mut digest = Sha256::new();
    digest.update(b"colay/legacy-import-manifest/v1\0");
    for (path, (sha256, length)) in entries {
        digest.update(path.as_bytes());
        digest.update(b"\0");
        digest.update(sha256.as_bytes());
        digest.update(b"\0");
        digest.update(length.to_le_bytes());
    }
    hex::encode(digest.finalize())
}

fn source_identity_hash(connection: &Connection) -> StateResult<String> {
    let identity: (i64, String, String, String) = connection.query_row(
        "SELECT version, name, checksum, applied_at FROM schema_migrations \
         ORDER BY version LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let mut digest = Sha256::new();
    digest.update(b"colay/legacy-source-identity/v1\0");
    digest.update(identity.0.to_le_bytes());
    for value in [&identity.1, &identity.2, &identity.3] {
        update_digest_length(&mut digest, value.len());
        digest.update(value.as_bytes());
    }
    Ok(hex::encode(digest.finalize()))
}

fn logical_workspace_hash(connection: &Connection) -> StateResult<String> {
    use rusqlite::types::ValueRef;

    let mut digest = Sha256::new();
    digest.update(b"colay/legacy-logical-workspace/v1\0");
    for table in IMPORT_TABLES {
        let columns = table_columns(connection, "main", table)?;
        let quoted = columns
            .iter()
            .map(|column| format!("\"{}\"", column.name))
            .collect::<Vec<_>>();
        let sql = format!(
            "SELECT {} FROM main.\"{table}\" WHERE workspace_id = ?1 ORDER BY {}",
            quoted.join(", "),
            quoted.join(", ")
        );
        update_digest_length(&mut digest, table.len());
        digest.update(table.as_bytes());
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query([RESERVED_LEGACY_WORKSPACE])?;
        while let Some(row) = rows.next()? {
            digest.update(b"row\0");
            for index in 0..columns.len() {
                match row.get_ref(index)? {
                    ValueRef::Null => digest.update(b"n"),
                    ValueRef::Integer(value) => {
                        digest.update(b"i");
                        digest.update(value.to_le_bytes());
                    }
                    ValueRef::Real(value) => {
                        digest.update(b"r");
                        digest.update(value.to_bits().to_le_bytes());
                    }
                    ValueRef::Text(value) => {
                        digest.update(b"t");
                        update_digest_length(&mut digest, value.len());
                        digest.update(value);
                    }
                    ValueRef::Blob(value) => {
                        digest.update(b"b");
                        update_digest_length(&mut digest, value.len());
                        digest.update(value);
                    }
                }
            }
        }
    }
    Ok(hex::encode(digest.finalize()))
}

fn source_fingerprint(
    schema_version: u32,
    source_identity_hash: &str,
    logical_content_hash: &str,
    files: &[ManifestFile],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"colay/legacy-source-fingerprint/v2\0");
    digest.update(schema_version.to_le_bytes());
    digest.update(source_identity_hash.as_bytes());
    digest.update(logical_content_hash.as_bytes());
    for file in files {
        let path = file.relative_path.to_string_lossy().replace('\\', "/");
        update_digest_length(&mut digest, path.len());
        digest.update(path.as_bytes());
        digest.update(file.sha256.as_bytes());
        digest.update(file.byte_length.to_le_bytes());
    }
    hex::encode(digest.finalize())
}

fn update_digest_length(digest: &mut Sha256, length: usize) {
    digest.update(u64::try_from(length).unwrap_or(u64::MAX).to_le_bytes());
}

fn inspection_scratch_fingerprint(schema_version: u32, source_identity_hash: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"colay/legacy-import-inspection-scratch/v1\0");
    digest.update(schema_version.to_le_bytes());
    digest.update(source_identity_hash.as_bytes());
    hex::encode(digest.finalize())
}

fn hash_domain(domain: &[u8], bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    hex::encode(digest.finalize())
}

fn verify_file(path: &Path, sha256: &str, length: u64) -> StateResult<()> {
    if file_length(path)? != length || sha256_file(path)? != sha256 {
        return Err(StateError::ArtifactConflict(path.to_path_buf()));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> StateResult<String> {
    fs::read(path)
        .map(|bytes| hex::encode(Sha256::digest(bytes)))
        .map_err(|error| StateError::io(path, error))
}

fn file_length(path: &Path) -> StateResult<u64> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| StateError::io(path, error))
}

fn table_exists(connection: &Connection, name: &str) -> StateResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [name],
            |row| row.get(0),
        )
        .map_err(StateError::from)
}

#[cfg(test)]
mod final_hardening_tests {
    use std::fs;

    use rusqlite::Connection;

    use crate::{
        Database, GlobalStatePaths, RepositoryStatePaths, RootConfig, StateEnvironment,
        source_guard::{SourceOpenHookPhase, clear_source_open_hook, set_source_open_hook},
    };

    use super::{LegacyEventEvidence, LegacyImportPlan, LegacyImporter, sha256_file};

    #[test]
    fn source_parent_aba_refuses_import_without_source_or_target_mutation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let repository = temporary.path().join("repository");
        fs::create_dir_all(&repository)?;
        let source = RepositoryStatePaths::from_config(&repository, &RootConfig::default())?;
        fs::create_dir_all(&source.root)?;
        drop(Connection::open(&source.database)?);

        let environment = StateEnvironment::with_colay_home(temporary.path().join("global"))?;
        let paths = GlobalStatePaths::resolve(&environment)?;
        let global = Database::open(&paths.database)?;
        global.migrate_with_backup(&paths.backups)?;
        let workspace_id = global
            .resolve_repository_workspace(&repository)?
            .workspace_id;
        let plan = LegacyImportPlan {
            source: source.clone(),
            source_schema_version: 3,
            source_fingerprint: "a".repeat(64),
            source_root_hash: "b".repeat(64),
            manifest_hash: "c".repeat(64),
            event_evidence: LegacyEventEvidence {
                count: 0,
                root_hash: None,
                tip_hash: None,
            },
            database_sha256: "d".repeat(64),
            database_length: 0,
            files: Vec::new(),
        };
        let source_hash = sha256_file(&source.database)?;
        let target_before = target_import_counts(&global, workspace_id)?;

        let saved = source.root.with_extension("saved");
        let alternate = source.root.with_extension("alternate");
        fs::create_dir_all(&alternate)?;
        drop(Connection::open(
            alternate.join(
                source
                    .database
                    .file_name()
                    .ok_or("source database has no file name")?,
            ),
        )?);
        let source_root = source.root.clone();
        let saved_for_hook = saved.clone();
        let alternate_for_hook = alternate.clone();
        set_source_open_hook(move |phase, path| {
            if path.file_name() != source_root.file_name() {
                return Ok(());
            }
            match phase {
                SourceOpenHookPhase::BeforeRetainedOpen => {
                    fs::rename(&source_root, &saved_for_hook)
                        .map_err(|error| crate::StateError::io(&source_root, error))?;
                    fs::rename(&alternate_for_hook, &source_root)
                        .map_err(|error| crate::StateError::io(&source_root, error))?;
                }
                SourceOpenHookPhase::BeforePostOpenCheck => {
                    fs::rename(&source_root, &alternate_for_hook)
                        .map_err(|error| crate::StateError::io(&source_root, error))?;
                    fs::rename(&saved_for_hook, &source_root)
                        .map_err(|error| crate::StateError::io(&source_root, error))?;
                }
            }
            Ok(())
        });

        let result = LegacyImporter::apply(&global, workspace_id, &plan, &paths);
        clear_source_open_hook();

        assert!(result.is_err());
        assert_eq!(sha256_file(&source.database)?, source_hash);
        assert!(alternate.is_dir());
        assert!(!saved.exists());
        let target_after = target_import_counts(&global, workspace_id)?;
        assert_eq!(target_before, target_after);
        assert!(
            !paths
                .for_workspace(workspace_id)
                .root
                .join("imports")
                .exists()
        );
        Ok(())
    }

    fn target_import_counts(
        global: &Database,
        workspace_id: crate::WorkspaceId,
    ) -> Result<(i64, i64, i64, i64), Box<dyn std::error::Error>> {
        let connection = global.raw_lock()?;
        let workspace_id = workspace_id.to_string();
        Ok((
            connection.query_row(
                "SELECT count(*) FROM tasks WHERE workspace_id = ?1",
                [&workspace_id],
                |row| row.get(0),
            )?,
            connection.query_row(
                "SELECT count(*) FROM task_events WHERE workspace_id = ?1",
                [&workspace_id],
                |row| row.get(0),
            )?,
            connection.query_row(
                "SELECT count(*) FROM legacy_imports WHERE workspace_id = ?1",
                [&workspace_id],
                |row| row.get(0),
            )?,
            connection.query_row(
                "SELECT count(*) FROM legacy_import_id_mappings WHERE workspace_id = ?1",
                [&workspace_id],
                |row| row.get(0),
            )?,
        ))
    }
}
