#![cfg(feature = "test-fixtures")]

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};

use anyhow::{Context as _, Result};
use chrono::Utc;
use orchestrator_state::{
    Database, GlobalStatePaths, LegacyImporter, RepositoryStatePaths, RootConfig,
    STATE_SCHEMA_VERSION, StateEnvironment,
};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const MIGRATIONS_THROUGH_V8: &[(u32, &str, &str)] = &[
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
    (
        4,
        "durable_sessions",
        include_str!("../../../migrations/0004_durable_sessions.sql"),
    ),
    (
        5,
        "chat_workspace_state",
        include_str!("../../../migrations/0005_chat_workspace_state.sql"),
    ),
    (
        6,
        "approved_task_graphs",
        include_str!("../../../migrations/0006_approved_task_graphs.sql"),
    ),
    (
        7,
        "parallel_execution",
        include_str!("../../../migrations/0007_parallel_execution.sql"),
    ),
    (
        8,
        "result_integration",
        include_str!("../../../migrations/0008_result_integration.sql"),
    ),
];
const LEGACY_IMPORT_DETAIL_MAX_CHARS: usize = 256;
const LEGACY_IMPORT_INCOMPLETE_PROPOSAL_DETAIL: &str = "legacy import source has an incomplete proposal seal; restore the repository-local database from a trusted backup or repair it, then rerun `colay doctor`";
const LEGACY_IMPORT_INVALID_SOURCE_DETAIL: &str = "legacy import source failed integrity validation; restore the repository-local database from a trusted backup or repair it, then rerun `colay doctor`";

struct DoctorFixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    repository: PathBuf,
    colay_home: PathBuf,
}

struct FutureWalState {
    _writer: Connection,
    database: Vec<u8>,
    wal: Vec<u8>,
    shm: Vec<u8>,
}

struct CurrentWalState {
    _writer: Connection,
    database: Vec<u8>,
    wal: Vec<u8>,
    shm: Vec<u8>,
    last_seen_at: String,
    workspace_root: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct FileContentSnapshot {
    bytes: Vec<u8>,
    sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
struct LegacyDoctorMutationSnapshot {
    global_database_exists: bool,
    global_database_is_directory: bool,
    global_database_directory_entries: Option<BTreeSet<PathBuf>>,
    global_table_rows: Option<BTreeMap<String, i64>>,
    source_database: Option<FileContentSnapshot>,
    published_files: Option<BTreeSet<(PathBuf, u64, String)>>,
}

impl LegacyDoctorMutationSnapshot {
    fn capture(fixture: &DoctorFixture, source_database: Option<&Path>) -> Result<Self> {
        let global_database = fixture.global_database();
        let global_metadata = fs::metadata(&global_database).ok();
        let global_database_exists = global_metadata.is_some();
        let global_database_is_directory = global_metadata
            .as_ref()
            .is_some_and(std::fs::Metadata::is_dir);
        let global_database_directory_entries = global_database_is_directory
            .then(|| directory_entries(&global_database))
            .transpose()?;
        let global_table_rows = global_metadata
            .as_ref()
            .is_some_and(std::fs::Metadata::is_file)
            .then(|| global_table_row_counts(&fixture.global_database()))
            .transpose()?;
        let source_database = source_database
            .filter(|path| path.is_file())
            .map(file_content_snapshot)
            .transpose()?;
        let published_files = published_import_metadata(fixture)?;
        Ok(Self {
            global_database_exists,
            global_database_is_directory,
            global_database_directory_entries,
            global_table_rows,
            source_database,
            published_files: (!published_files.is_empty()).then_some(published_files),
        })
    }
}

impl DoctorFixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let repository = root.join("repository");
        let colay_home = root.join("home");
        fs::create_dir_all(&repository)?;
        Ok(Self {
            _temp: temp,
            root,
            repository,
            colay_home,
        })
    }

    fn old_global_schema_untrusted_provider() -> Result<Self> {
        let fixture = Self::new()?;
        let database = fixture.global_database();
        Self::seed_schema_through_v8(&database)?;
        Ok(fixture)
    }

    fn seed_repository_legacy_schema_v8(&self) -> Result<PathBuf> {
        let state = self.repository.join(".colay");
        fs::create_dir_all(&state)?;
        fs::write(state.join("config.toml"), "config_version = 4\n")?;
        let database = state.join("orchestrator.db");
        Self::seed_schema_through_v8(&database)?;
        Ok(database)
    }

    fn seed_repository_legacy_invalid_graph_schema_v8(&self) -> Result<PathBuf> {
        self.seed_repository_legacy_invalid_graph_schema_v8_with_proposal_hash(None)
    }

    fn seed_repository_legacy_invalid_graph_schema_v8_with_proposal_hash(
        &self,
        proposal_hash: Option<&str>,
    ) -> Result<PathBuf> {
        let database = self.seed_repository_legacy_schema_v8()?;
        let connection = Connection::open(&database)?;
        let created_at = "2026-08-02T00:00:00Z";
        connection.execute(
            "INSERT INTO sessions(\
                 session_id, schema_version, revision, title, state, created_at, updated_at\
             ) VALUES (?1, '1.0', 0, 'legacy invalid graph', 'planning', ?2, ?2)",
            params!["01987d4e-2a54-7000-8000-000000000001", created_at],
        )?;
        connection.execute(
            "INSERT INTO conversation_messages(\
                 message_id, session_id, task_id, ordinal, role, kind, state, content_redacted, \
                 created_at, finalized_at\
             ) VALUES (?1, ?2, NULL, 1, 'user', 'user_message', 'final', \
                 'legacy invalid graph', ?3, ?3)",
            params![
                "01987d4e-2a54-7000-8000-000000000002",
                "01987d4e-2a54-7000-8000-000000000001",
                created_at,
            ],
        )?;
        connection.execute(
            "INSERT INTO graph_revisions(\
                 revision_id, session_id, goal_message_id, ordinal, status, \
                 proposal_hash, proposal_json, validation_json, planner_provider, \
                 created_at, completed_at\
             ) VALUES (?1, ?2, ?3, 1, 'invalid', ?4, NULL, ?5, 'codex', ?6, ?6)",
            params![
                "01987d4e-2a54-7000-8000-000000000003",
                "01987d4e-2a54-7000-8000-000000000001",
                "01987d4e-2a54-7000-8000-000000000002",
                proposal_hash,
                serde_json::to_string(&json!({"errors":["cycle"]}))?,
                created_at,
            ],
        )?;
        Ok(database)
    }

    fn seed_repository_legacy_sensitive_graph_mismatch_schema_v8(
        &self,
        sensitive_revision_id: &str,
    ) -> Result<PathBuf> {
        let proposal_hash = "a".repeat(64);
        let database = self.seed_repository_legacy_invalid_graph_schema_v8_with_proposal_hash(
            Some(&proposal_hash),
        )?;
        let connection = Connection::open(&database)?;
        connection.execute(
            "UPDATE graph_revisions SET status = 'planning' WHERE revision_id = ?1",
            ["01987d4e-2a54-7000-8000-000000000003"],
        )?;
        let proposal = json!({
            "schema_version": "1",
            "revision_id": "01987d4e-2a54-7000-8000-000000000003",
            "session_id": "01987d4e-2a54-7000-8000-000000000001",
            "goal_message_id": "01987d4e-2a54-7000-8000-000000000002",
            "planner_provider": "codex",
            "proposed_at": "2026-08-02T00:00:00Z",
            "nodes": [],
        });
        let validation = json!({
            "node_count": 0,
            "edge_count": 0,
            "topological_order": [],
            "maximum_parallel_width": 0,
            "configured_parallel_workers": 1,
        });
        connection.execute(
            "UPDATE graph_revisions
             SET revision_id = ?1, status = 'invalid', proposal_json = ?2, validation_json = ?3
             WHERE revision_id = ?4",
            params![
                sensitive_revision_id,
                serde_json::to_string(&proposal)?,
                serde_json::to_string(&validation)?,
                "01987d4e-2a54-7000-8000-000000000003",
            ],
        )?;
        Ok(database)
    }

    fn seed_schema_through_v8(database: &Path) -> Result<()> {
        fs::create_dir_all(database.parent().context("global database has no parent")?)?;
        let connection = Connection::open(database)?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        for (version, name, sql) in MIGRATIONS_THROUGH_V8 {
            connection.execute_batch(sql)?;
            connection.execute(
                "INSERT INTO schema_migrations(version, name, checksum, applied_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    version,
                    name,
                    format!("{:x}", Sha256::digest(sql.as_bytes())),
                    Utc::now().to_rfc3339(),
                ],
            )?;
        }
        drop(connection);
        Ok(())
    }

    fn configure_fake_providers(&self) -> Result<()> {
        fs::create_dir_all(&self.colay_home)?;
        let executable = toml_path(&fake_provider_binary());
        fs::write(
            self.colay_home.join("config.toml"),
            format!(
                "config_version = 4\n\
                 [orchestrator.providers.codex]\nexecutable = {executable}\n\
                 [orchestrator.providers.claude]\nexecutable = {executable}\n\
                 [orchestrator.providers.gemini]\nexecutable = {executable}\n\
                 [orchestrator.providers.agy]\nexecutable = {executable}\n"
            ),
        )?;
        Ok(())
    }

    fn colay<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        self.colay_in(&self.repository, args)
    }

    fn colay_in<const N: usize>(&self, repository: &Path, args: [&str; N]) -> Result<Output> {
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        #[cfg(windows)]
        let system_root = env::var_os("SystemRoot").context("SystemRoot is not set")?;
        #[cfg(not(windows))]
        let system_root = "/";
        let status = Command::new(env!("CARGO_BIN_EXE_colay"))
            .args(args)
            .current_dir(repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
            .env(
                "PATH",
                fake_provider_binary()
                    .parent()
                    .context("fake provider has no parent")?,
            )
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root)
            .env("TEMP", &self.root)
            .env("TMP", &self.root)
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?))
            .status()
            .context("failed to invoke colay")?;
        stdout.seek(SeekFrom::Start(0))?;
        stderr.seek(SeekFrom::Start(0))?;
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        stdout.read_to_end(&mut stdout_bytes)?;
        stderr.read_to_end(&mut stderr_bytes)?;
        Ok(Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        })
    }

    fn global_database(&self) -> PathBuf {
        self.colay_home.join("state/state.db")
    }

    fn seed_current_global_workspace(&self) -> Result<()> {
        let database = Database::open(self.global_database())?;
        database.migrate_with_backup(&self.colay_home.join("state/backups"))?;
        database.resolve_repository_workspace(&self.repository)?;
        Ok(())
    }

    fn import_repository_legacy(&self) -> Result<()> {
        let environment = StateEnvironment::with_colay_home(self.colay_home.clone())?;
        let paths = GlobalStatePaths::resolve(&environment)?;
        let database = Database::open(&paths.database)?;
        database.migrate_with_backup(&paths.backups)?;
        let workspace_id = database
            .resolve_repository_workspace(&self.repository)?
            .workspace_id;
        let source = RepositoryStatePaths::from_config(&self.repository, &RootConfig::default())?;
        let plan = LegacyImporter::inspect(&source, &paths)?
            .context("repository legacy source was not inspectable")?;
        LegacyImporter::apply(&database, workspace_id, &plan, &paths)?;
        Ok(())
    }

    fn schema_version(&self) -> Result<u32> {
        Connection::open(self.global_database())?
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Into::into)
    }

    fn seed_future_schema_in_delete_journal_mode(&self) -> Result<Vec<u8>> {
        let database = self.global_database();
        fs::create_dir_all(database.parent().context("global database has no parent")?)?;
        let connection = Connection::open(&database)?;
        let journal_mode: String =
            connection.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
        assert_eq!(journal_mode, "delete");
        connection.pragma_update(None, "user_version", STATE_SCHEMA_VERSION + 1)?;
        drop(connection);
        fs::read(database).map_err(Into::into)
    }

    fn seed_future_schema_in_wal_mode(&self) -> Result<FutureWalState> {
        let database = self.global_database();
        fs::create_dir_all(database.parent().context("global database has no parent")?)?;
        let connection = Connection::open(&database)?;
        let journal_mode: String =
            connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        assert_eq!(journal_mode, "wal");
        connection.pragma_update(None, "wal_autocheckpoint", 0)?;
        connection.execute_batch("CREATE TABLE preflight_fixture(value INTEGER);")?;
        connection.pragma_update(None, "user_version", STATE_SCHEMA_VERSION + 1)?;
        let wal = sqlite_sidecar(&database, "-wal");
        let shm = sqlite_sidecar(&database, "-shm");
        Ok(FutureWalState {
            _writer: connection,
            database: fs::read(database)?,
            wal: fs::read(wal)?,
            shm: fs::read(shm)?,
        })
    }

    fn seed_current_schema_in_wal_mode(&self) -> Result<CurrentWalState> {
        let database_path = self.global_database();
        let database = Database::open(&database_path)?;
        database.migrate_with_backup(&self.colay_home.join("state/backups"))?;
        let workspace_id = database
            .resolve_repository_workspace(&self.repository)?
            .workspace_id;
        drop(database);

        let connection = Connection::open(&database_path)?;
        let journal_mode: String =
            connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        assert_eq!(journal_mode, "wal");
        connection.pragma_update(None, "wal_autocheckpoint", 0)?;
        let last_seen_at = "2099-01-02T03:04:05Z".to_owned();
        let workspace_changed = connection.execute(
            "UPDATE workspaces SET last_seen_at = ?2 WHERE workspace_id = ?1",
            params![workspace_id.to_string(), last_seen_at],
        )?;
        assert_eq!(workspace_changed, 1);
        let path_changed = connection.execute(
            "UPDATE workspace_paths SET last_seen_at = ?2 \
             WHERE workspace_id = ?1 AND is_current = 1",
            params![workspace_id.to_string(), last_seen_at],
        )?;
        assert_eq!(path_changed, 1);
        let wal = sqlite_sidecar(&database_path, "-wal");
        let shm = sqlite_sidecar(&database_path, "-shm");
        Ok(CurrentWalState {
            _writer: connection,
            database: fs::read(database_path)?,
            wal: fs::read(wal)?,
            shm: fs::read(shm)?,
            last_seen_at,
            workspace_root: self
                .colay_home
                .join("data/workspaces")
                .join(workspace_id.to_string()),
        })
    }

    fn read_only_journal_mode(&self) -> Result<String> {
        let connection = Connection::open_with_flags(
            self.global_database(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .map_err(Into::into)
    }

    fn seed_corrupt_artifact_reference(&self) -> Result<()> {
        let database = Database::open(self.global_database())?;
        database.migrate_with_backup(&self.colay_home.join("state/backups"))?;
        let workspace_id = database
            .resolve_repository_workspace(&self.repository)?
            .workspace_id;
        drop(database);
        let relative_path = "artifacts/corrupt.txt";
        let artifact_path = self
            .colay_home
            .join("data/workspaces")
            .join(workspace_id.to_string())
            .join(relative_path);
        fs::create_dir_all(artifact_path.parent().context("artifact has no parent")?)?;
        fs::write(&artifact_path, b"corrupt bytes")?;
        Connection::open(self.global_database())?.execute(
            "INSERT INTO artifacts( \
                 workspace_id, artifact_id, task_id, kind, relative_path, sha256, byte_length, \
                 media_type, created_at \
             ) VALUES (?1, ?2, NULL, 'doctor_fixture', ?3, ?4, ?5, NULL, ?6)",
            params![
                workspace_id.to_string(),
                "artifact-corrupt",
                relative_path,
                format!("{:x}", Sha256::digest(b"expected bytes")),
                b"expected bytes".len(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }
}

impl Drop for DoctorFixture {
    fn drop(&mut self) {
        let _ = self.colay(["daemon", "stop"]);
    }
}

fn fake_provider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_colay-e2e-fake-provider"))
}

fn toml_path(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

fn temporary_directory_entries(path: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|entry| {
            entry
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".tmp"))
        })
        .collect::<Vec<_>>();
    entries.sort();
    Ok(entries)
}

fn published_import_metadata(fixture: &DoctorFixture) -> Result<BTreeSet<(PathBuf, u64, String)>> {
    let workspace_root = fixture.colay_home.join("data/workspaces");
    if !workspace_root.exists() {
        return Ok(BTreeSet::new());
    }
    let mut pending = vec![workspace_root.clone()];
    let mut published = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            let relative = path.strip_prefix(&workspace_root)?.to_path_buf();
            if relative
                .components()
                .any(|component| component.as_os_str() == "imports")
            {
                let bytes = fs::read(&path)?;
                published.insert((
                    relative,
                    metadata.len(),
                    format!("{:x}", Sha256::digest(bytes)),
                ));
            }
        }
    }
    Ok(published)
}

fn file_content_snapshot(path: &Path) -> Result<FileContentSnapshot> {
    let bytes = fs::read(path)?;
    Ok(FileContentSnapshot {
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        bytes,
    })
}

fn directory_entries(directory: &Path) -> Result<BTreeSet<PathBuf>> {
    fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()
        .map_err(Into::into)
}

fn global_table_row_counts(database: &Path) -> Result<BTreeMap<String, i64>> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_schema \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut rows = BTreeMap::new();
    for table in tables {
        let quoted = table.replace('"', "\"\"");
        let count =
            connection.query_row(&format!("SELECT count(*) FROM \"{quoted}\""), [], |row| {
                row.get(0)
            })?;
        rows.insert(table, count);
    }
    Ok(rows)
}

fn check_named<'a>(document: &'a Value, name: &str) -> Result<&'a Value> {
    document["data"]["checks"]
        .as_array()
        .context("doctor checks must be an array")?
        .iter()
        .find(|check| check["name"] == name)
        .with_context(|| format!("doctor omitted the {name} check"))
}

fn normalize_provider_timestamps(document: &mut Value) {
    if let Some(providers) = document["data"]["providers"].as_array_mut() {
        for provider in providers {
            if let Some(health) = provider.get_mut("health").and_then(Value::as_object_mut) {
                health.remove("checked_at");
                health.remove("latency_ms");
            }
            if let Some(readiness) = provider
                .get_mut("account_readiness")
                .and_then(Value::as_object_mut)
            {
                readiness.remove("checked_at");
            }
        }
    }
}

fn assert_account_readiness_unverified(document: &Value) -> Result<()> {
    let providers = document["data"]["providers"]
        .as_array()
        .context("provider list missing")?;
    assert!(!providers.is_empty());
    for provider in providers {
        assert_eq!(
            provider["account_readiness"]["status"], "unverified",
            "provider report claimed account readiness: {provider}"
        );
        assert!(
            provider["account_readiness"]["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("safe public probes")),
            "provider report omitted the readiness boundary: {provider}"
        );
        assert!(provider["account_readiness"]["checked_at"].is_string());
    }
    assert_eq!(document["data"]["inference_requests"], 0);
    Ok(())
}

#[test]
fn doctor_reports_global_workspace_and_operational_checks() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.seed_current_global_workspace()?;
    fixture.configure_fake_providers()?;
    let database_before = fs::read(fixture.global_database())?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.repository.join(".colay").exists());
    assert_eq!(fs::read(fixture.global_database())?, database_before);
    let document: Value = serde_json::from_slice(&output.stdout)?;
    for name in [
        "state",
        "daemon",
        "workspace",
        "audit",
        "artifacts",
        "git",
        "runtime",
        "provider_codex",
        "provider_claude",
        "provider_gemini",
        "provider_agy",
    ] {
        check_named(&document, name)?;
    }
    for provider in [
        "provider_codex",
        "provider_claude",
        "provider_gemini",
        "provider_agy",
    ] {
        let check = check_named(&document, provider)?;
        assert_eq!(check["status"], "warn");
        assert!(
            check["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("account readiness unverified")),
            "{provider} did not expose the account-readiness boundary"
        );
        assert_eq!(
            check["data"]["provider"]["account_readiness"]["status"],
            "unverified"
        );
        assert_eq!(
            PathBuf::from(
                check["data"]["configured_executable"]
                    .as_str()
                    .with_context(|| format!("{provider} did not resolve a fake provider"))?
            ),
            fake_provider_binary(),
            "{provider} did not exercise the configured fake provider"
        );
    }
    assert_eq!(
        PathBuf::from(
            check_named(&document, "state")?["data"]["database"]
                .as_str()
                .context("state check omitted the database path")?
        ),
        fixture.global_database()
    );
    assert_eq!(
        PathBuf::from(
            check_named(&document, "workspace")?["data"]["canonical_path"]
                .as_str()
                .context("workspace check omitted its canonical path")?
        ),
        fs::canonicalize(&fixture.repository)?
    );
    assert!(
        check_named(&document, "workspace")?["data"]["workspace_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    Ok(())
}

#[test]
fn doctor_preserves_current_schema_wal_database_and_sidecars() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    let before = fixture.seed_current_schema_in_wal_mode()?;
    let database = fixture.global_database();
    let wal = sqlite_sidecar(&database, "-wal");
    let shm = sqlite_sidecar(&database, "-shm");
    let journal = sqlite_sidecar(&database, "-journal");
    let temporary_entries = temporary_directory_entries(&fixture.root)?;
    assert!(!fixture.colay_home.join("state/backups").exists());
    assert!(!before.workspace_root.exists());
    assert!(!journal.exists());

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    for name in ["state", "workspace", "audit", "artifacts"] {
        let check = check_named(&document, name)?;
        assert_eq!(check["status"], "pass", "{name} check failed: {check}");
    }
    assert_eq!(
        check_named(&document, "workspace")?["data"]["last_seen_at"],
        Value::String(before.last_seen_at.clone())
    );
    assert_eq!(fs::read(database)?, before.database);
    assert_eq!(fs::read(wal)?, before.wal);
    assert_eq!(fs::read(shm)?, before.shm);
    assert!(!journal.exists());
    assert!(!fixture.colay_home.join("state/backups").exists());
    assert!(!before.workspace_root.exists());
    assert_eq!(
        temporary_directory_entries(&fixture.root)?,
        temporary_entries
    );
    Ok(())
}

#[test]
fn compatibility_is_a_behavioral_alias_of_doctor_providers() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;

    let doctor = fixture.colay(["--json", "doctor", "providers"])?;
    let compatibility = fixture.colay(["--json", "compatibility"])?;

    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert!(
        compatibility.status.success(),
        "{}",
        String::from_utf8_lossy(&compatibility.stderr)
    );
    let mut doctor: Value = serde_json::from_slice(&doctor.stdout)?;
    let mut compatibility: Value = serde_json::from_slice(&compatibility.stdout)?;
    assert_eq!(doctor["schema_version"], "2");
    assert_eq!(doctor["data"]["schema_version"], "2");
    assert_eq!(compatibility["schema_version"], "2");
    assert_eq!(compatibility["data"]["schema_version"], "2");
    assert_account_readiness_unverified(&doctor)?;
    assert_account_readiness_unverified(&compatibility)?;
    doctor["command"] = json!("provider_doctor");
    compatibility["command"] = json!("provider_doctor");
    normalize_provider_timestamps(&mut doctor);
    normalize_provider_timestamps(&mut compatibility);
    assert_eq!(doctor, compatibility);
    assert!(!fixture.repository.join(".colay").exists());
    Ok(())
}

#[test]
fn migrate_apply_is_idempotent_with_untrusted_provider_and_missing_local_config() -> Result<()> {
    let fixture = DoctorFixture::old_global_schema_untrusted_provider()?;

    let first = fixture.colay(["migrate", "apply"])?;
    let second = fixture.colay(["migrate", "apply"])?;

    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(fixture.schema_version()?, STATE_SCHEMA_VERSION);
    assert!(!fixture.repository.join(".colay").exists());
    Ok(())
}

#[test]
fn migrate_refuses_a_future_schema_before_opening_it_for_writes() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    let before = fixture.seed_future_schema_in_delete_journal_mode()?;

    let migration = fixture.colay(["migrate", "apply"])?;

    assert!(!migration.status.success());
    assert_eq!(fs::read(fixture.global_database())?, before);
    assert_eq!(fixture.read_only_journal_mode()?, "delete");
    assert!(!fixture.global_database().with_extension("db-wal").exists());
    assert!(!fixture.global_database().with_extension("db-shm").exists());
    assert!(!fixture.colay_home.join("state/backups").exists());
    Ok(())
}

#[test]
fn migrate_preflight_does_not_change_existing_future_wal_sidecars() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    let before = fixture.seed_future_schema_in_wal_mode()?;
    let database = fixture.global_database();
    let wal = sqlite_sidecar(&database, "-wal");
    let shm = sqlite_sidecar(&database, "-shm");

    let migration = fixture.colay(["migrate", "apply"])?;

    assert!(!migration.status.success());
    assert_eq!(fs::read(database)?, before.database);
    assert_eq!(fs::read(wal)?, before.wal);
    assert_eq!(fs::read(shm)?, before.shm);
    assert!(!fixture.colay_home.join("state/backups").exists());
    Ok(())
}

#[test]
fn doctor_rejects_future_schema_wal_without_changing_source_or_leaking_snapshot() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    let before = fixture.seed_future_schema_in_wal_mode()?;
    let database = fixture.global_database();
    let wal = sqlite_sidecar(&database, "-wal");
    let shm = sqlite_sidecar(&database, "-shm");
    let journal = sqlite_sidecar(&database, "-journal");
    let temporary_entries = temporary_directory_entries(&fixture.root)?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(document["data"]["passed"], false);
    let state = check_named(&document, "state")?;
    assert_eq!(state["status"], "fail", "unexpected state check: {state}");
    assert!(
        state["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("newer than supported")),
        "unexpected state detail: {state}"
    );
    assert_eq!(fs::read(database)?, before.database);
    assert_eq!(fs::read(wal)?, before.wal);
    assert_eq!(fs::read(shm)?, before.shm);
    assert!(!journal.exists());
    assert!(!fixture.colay_home.join("state/backups").exists());
    assert!(!fixture.colay_home.join("data/workspaces").exists());
    assert_eq!(
        temporary_directory_entries(&fixture.root)?,
        temporary_entries
    );
    Ok(())
}

#[test]
fn doctor_reports_pending_migrations_without_changing_the_database() -> Result<()> {
    let fixture = DoctorFixture::old_global_schema_untrusted_provider()?;
    let before = fs::read(fixture.global_database())?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(fixture.global_database())?, before);
    assert_eq!(fixture.schema_version()?, 8);
    assert!(!fixture.colay_home.join("state/backups").exists());
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(check_named(&document, "state")?["status"], "warn");
    assert_eq!(
        check_named(&document, "state")?["data"]["current_schema_version"],
        8
    );
    Ok(())
}

#[test]
fn doctor_does_not_query_future_columns_from_schema_eight_legacy_workspace() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    let legacy_database = fixture.seed_repository_legacy_schema_v8()?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("no such column: phase"), "{stderr}");
    assert!(!fixture.global_database().exists());
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(check_named(&document, "state")?["status"], "warn");
    assert_eq!(
        check_named(&document, "state")?["data"]["current_schema_version"],
        Value::Null
    );
    let legacy_check = check_named(&document, "legacy_import")?;
    assert_eq!(legacy_check["status"], "pass", "{legacy_check}");
    assert_eq!(legacy_check["data"]["pending"], false);
    assert_eq!(legacy_check["data"]["imported"], false);
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    Ok(())
}

#[test]
fn legacy_import_doctor_reports_import_ready_invalid_graph_source() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.seed_current_global_workspace()?;
    fixture.configure_fake_providers()?;
    let legacy_database = fixture.seed_repository_legacy_invalid_graph_schema_v8()?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(check_named(&document, "legacy_import")?["status"], "pass");
    assert_eq!(
        check_named(&document, "legacy_import")?["data"]["pending"],
        true
    );
    assert_eq!(
        check_named(&document, "legacy_import")?["data"]["imported"],
        false
    );
    assert_eq!(
        check_named(&document, "legacy_import")?["data"]["source_schema_version"],
        8
    );
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    assert_eq!(document["data"]["inference_requests"], 0);
    Ok(())
}

#[test]
fn legacy_import_doctor_reports_completed_import() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    let legacy_database = fixture.seed_repository_legacy_invalid_graph_schema_v8()?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let before_import = fixture.colay(["--json", "doctor"])?;

    assert!(
        before_import.status.success(),
        "{}",
        String::from_utf8_lossy(&before_import.stderr)
    );
    let before_import: Value = serde_json::from_slice(&before_import.stdout)?;
    let legacy_check = check_named(&before_import, "legacy_import")?;
    assert_eq!(legacy_check["status"], "pass", "{legacy_check}");
    assert_eq!(legacy_check["data"]["pending"], true);
    assert_eq!(legacy_check["data"]["imported"], false);
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );

    fixture.import_repository_legacy()?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;
    assert!(before.published_files.is_some());

    let after_import = fixture.colay(["--json", "doctor"])?;

    assert!(
        after_import.status.success(),
        "{}",
        String::from_utf8_lossy(&after_import.stderr)
    );
    let after_import: Value = serde_json::from_slice(&after_import.stdout)?;
    let legacy_check = check_named(&after_import, "legacy_import")?;
    assert_eq!(legacy_check["status"], "pass");
    assert_eq!(legacy_check["data"]["pending"], false);
    assert_eq!(legacy_check["data"]["imported"], true);
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    Ok(())
}

#[test]
fn legacy_import_doctor_reports_no_source() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.seed_current_global_workspace()?;
    fixture.configure_fake_providers()?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, None)?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    let legacy_check = check_named(&document, "legacy_import")?;
    assert_eq!(legacy_check["status"], "pass");
    assert_eq!(legacy_check["data"]["pending"], false);
    assert_eq!(legacy_check["data"]["imported"], false);
    assert!(legacy_check["data"].get("source_database").is_none());
    assert!(legacy_check["data"].get("source_fingerprint").is_none());
    assert!(legacy_check["data"].get("source_schema_version").is_none());
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, None)?,
        before
    );
    Ok(())
}

#[test]
fn legacy_import_doctor_reports_changed_source_as_pending() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    let legacy_database = fixture.seed_repository_legacy_invalid_graph_schema_v8()?;
    fixture.import_repository_legacy()?;
    Connection::open(&legacy_database)?.execute(
        "UPDATE conversation_messages SET content_redacted = 'changed legacy source'",
        [],
    )?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    let legacy_check = check_named(&document, "legacy_import")?;
    assert_eq!(legacy_check["status"], "pass");
    assert_eq!(legacy_check["data"]["pending"], true);
    assert_eq!(legacy_check["data"]["imported"], false);
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    Ok(())
}

#[test]
fn legacy_import_doctor_reports_unregistered_source_as_pending() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.seed_current_global_workspace()?;
    fixture.configure_fake_providers()?;
    let unregistered = fixture.root.join("unregistered-offline");
    fs::create_dir_all(&unregistered)?;
    let fixture_legacy_database = fixture.seed_repository_legacy_invalid_graph_schema_v8()?;
    let legacy_database = unregistered.join(".colay/orchestrator.db");
    fs::create_dir_all(
        legacy_database
            .parent()
            .context("legacy database has no parent")?,
    )?;
    fs::copy(&fixture_legacy_database, &legacy_database)?;
    fs::write(
        legacy_database
            .parent()
            .context("legacy database has no parent")?
            .join("config.toml"),
        "config_version = 4\n",
    )?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let output = fixture.colay_in(&unregistered, ["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    let legacy_check = check_named(&document, "legacy_import")?;
    assert_eq!(legacy_check["status"], "pass");
    assert_eq!(legacy_check["data"]["pending"], true);
    assert_eq!(legacy_check["data"]["imported"], false);
    assert_eq!(check_named(&document, "workspace")?["status"], "warn");
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    Ok(())
}

#[test]
fn legacy_import_doctor_fails_a_corrupt_completion_ledger_without_mutation() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    let legacy_database = fixture.seed_repository_legacy_invalid_graph_schema_v8()?;
    fixture.import_repository_legacy()?;
    Connection::open(fixture.global_database())?
        .execute("UPDATE legacy_imports SET result_json = '{}'", [])?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    let legacy_check = check_named(&document, "legacy_import")?;
    assert_eq!(legacy_check["status"], "fail");
    assert_eq!(legacy_check["detail"], LEGACY_IMPORT_INVALID_SOURCE_DETAIL);
    assert_eq!(document["data"]["passed"], false);
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    Ok(())
}

#[test]
fn legacy_import_doctor_fails_when_global_snapshot_is_unreadable() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    let legacy_database = fixture.seed_repository_legacy_invalid_graph_schema_v8()?;
    fs::create_dir_all(fixture.global_database())?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    let legacy_check = check_named(&document, "legacy_import")?;
    assert_eq!(legacy_check["status"], "fail", "{legacy_check}");
    assert_eq!(document["data"]["passed"], false);
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    Ok(())
}

#[test]
fn legacy_import_doctor_fails_when_migration_status_is_unreadable() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    let legacy_database = fixture.seed_repository_legacy_invalid_graph_schema_v8()?;
    fs::create_dir_all(
        fixture
            .global_database()
            .parent()
            .context("global database has no parent")?,
    )?;
    let connection = Connection::open(fixture.global_database())?;
    connection.pragma_update(None, "user_version", STATE_SCHEMA_VERSION)?;
    drop(connection);
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    let legacy_check = check_named(&document, "legacy_import")?;
    assert_eq!(legacy_check["status"], "fail", "{legacy_check}");
    assert_eq!(document["data"]["passed"], false);
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    Ok(())
}

#[test]
fn legacy_import_doctor_fails_when_global_health_is_unreadable() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.seed_current_global_workspace()?;
    fixture.configure_fake_providers()?;
    let legacy_database = fixture.seed_repository_legacy_invalid_graph_schema_v8()?;
    Connection::open(fixture.global_database())?.execute_batch("DROP TABLE task_events;")?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    let legacy_check = check_named(&document, "legacy_import")?;
    assert_eq!(legacy_check["status"], "fail", "{legacy_check}");
    assert_eq!(document["data"]["passed"], false);
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    Ok(())
}

#[test]
fn legacy_import_doctor_fails_when_workspace_registry_is_unreadable() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.seed_current_global_workspace()?;
    fixture.configure_fake_providers()?;
    let legacy_database = fixture.seed_repository_legacy_invalid_graph_schema_v8()?;
    Connection::open(fixture.global_database())?.execute_batch("DROP TABLE workspace_paths;")?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    let legacy_check = check_named(&document, "legacy_import")?;
    assert_eq!(legacy_check["status"], "fail", "{legacy_check}");
    assert_eq!(document["data"]["passed"], false);
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    Ok(())
}

#[test]
fn legacy_import_doctor_fails_an_incomplete_proposal_seal_without_mutation() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.seed_current_global_workspace()?;
    fixture.configure_fake_providers()?;
    let proposal_hash = "a".repeat(64);
    let legacy_database = fixture
        .seed_repository_legacy_invalid_graph_schema_v8_with_proposal_hash(Some(&proposal_hash))?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(document["data"]["passed"], false);
    let check = check_named(&document, "legacy_import")?;
    assert_eq!(check["status"], "fail");
    assert_eq!(check["detail"], LEGACY_IMPORT_INCOMPLETE_PROPOSAL_DETAIL);
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    assert_eq!(document["data"]["inference_requests"], 0);
    Ok(())
}

#[test]
fn legacy_import_doctor_redacts_and_bounds_repository_controlled_graph_errors() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.seed_current_global_workspace()?;
    fixture.configure_fake_providers()?;
    let sensitive_marker = format!("sensitive-doctor-marker-{}", "x".repeat(4_096));
    let legacy_database =
        fixture.seed_repository_legacy_sensitive_graph_mismatch_schema_v8(&sensitive_marker)?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(document["data"]["passed"], false);
    let check = check_named(&document, "legacy_import")?;
    assert_eq!(check["status"], "fail");
    let detail = check["detail"]
        .as_str()
        .context("legacy import detail is missing")?;
    assert_eq!(detail, LEGACY_IMPORT_INVALID_SOURCE_DETAIL);
    assert!(!detail.contains(&sensitive_marker));
    assert!(detail.chars().count() <= LEGACY_IMPORT_DETAIL_MAX_CHARS);
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    assert_eq!(document["data"]["inference_requests"], 0);
    Ok(())
}

#[test]
fn daemon_fixtures_with_distinct_homes_start_concurrently() -> Result<()> {
    let first = DoctorFixture::new()?;
    first.configure_fake_providers()?;
    let second = DoctorFixture::new()?;
    second.configure_fake_providers()?;
    let (first_start, second_start) = std::thread::scope(|scope| {
        let first_start = scope.spawn(|| first.colay(["daemon", "start"]));
        let second_start = scope.spawn(|| second.colay(["daemon", "start"]));
        let first_start = first_start
            .join()
            .map_err(|_| anyhow::anyhow!("first daemon-start thread panicked"))??;
        let second_start = second_start
            .join()
            .map_err(|_| anyhow::anyhow!("second daemon-start thread panicked"))??;
        Ok::<_, anyhow::Error>((first_start, second_start))
    })?;

    assert!(
        first_start.status.success(),
        "{}",
        String::from_utf8_lossy(&first_start.stderr)
    );
    assert!(
        second_start.status.success(),
        "{}",
        String::from_utf8_lossy(&second_start.stderr)
    );
    Ok(())
}

#[test]
fn migrate_apply_refuses_a_live_daemon_owner() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    let started = fixture.colay(["daemon", "start"])?;
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    std::thread::sleep(Duration::from_millis(50));

    let migration = fixture.colay(["migrate", "apply"])?;

    assert!(!migration.status.success());
    assert!(
        String::from_utf8_lossy(&migration.stderr)
            .contains("daemon singleton is already owned by another process"),
        "{}",
        String::from_utf8_lossy(&migration.stderr)
    );
    Ok(())
}

#[test]
fn release_rollback_refuses_a_live_daemon_owner_before_reading_release_files() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    let started = fixture.colay(["daemon", "start"])?;
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    std::thread::sleep(Duration::from_millis(50));

    let rollback = fixture.colay(["rollback", "plan", "--to", "missing-release"])?;

    assert!(!rollback.status.success());
    assert!(
        String::from_utf8_lossy(&rollback.stderr)
            .contains("daemon singleton is already owned by another process"),
        "{}",
        String::from_utf8_lossy(&rollback.stderr)
    );
    Ok(())
}

#[test]
fn doctor_deep_checks_a_workspace_through_the_live_daemon() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    let started = fixture.colay(["daemon", "start"])?;
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, None)?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    let legacy_import = check_named(&document, "legacy_import")?;
    assert_eq!(legacy_import["status"], "pass");
    assert_eq!(legacy_import["data"]["pending"], false);
    assert_eq!(legacy_import["data"]["imported"], false);
    assert_eq!(
        check_named(&document, "state")?["data"]["via"],
        "daemon_ipc"
    );
    assert_eq!(check_named(&document, "audit")?["status"], "pass");
    assert_eq!(check_named(&document, "artifacts")?["status"], "pass");
    assert_eq!(
        check_named(&document, "artifacts")?["data"]["scope"],
        "persisted_references"
    );
    assert_eq!(
        check_named(&document, "audit")?["data"]["workspace_id"],
        check_named(&document, "workspace")?["data"]["workspace_id"]
    );
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, None)?,
        before
    );
    Ok(())
}

#[test]
fn live_doctor_in_unregistered_legacy_workspace_is_read_only() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    let started = fixture.colay(["daemon", "start"])?;
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let registered_workspace_id: String = Connection::open(fixture.global_database())?.query_row(
        "SELECT workspace_id FROM workspace_paths WHERE is_current = 1",
        [],
        |row| row.get(0),
    )?;
    let registered_workspace_root = fixture
        .colay_home
        .join("data/workspaces")
        .join(registered_workspace_id);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !registered_workspace_root.exists() {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("startup workspace runtime did not initialize its artifact root");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let unregistered = fixture.root.join("unregistered");
    fs::create_dir_all(&unregistered)?;
    let explicit = unregistered.join("doctor-explicit.toml");
    fs::write(
        &explicit,
        "config_version = 4\n[orchestrator]\nstate_dir = \"chosen-state\"\n",
    )?;
    let selected_state = unregistered.join("chosen-state");
    fs::create_dir_all(&selected_state)?;
    let legacy_database = selected_state.join("orchestrator.db");
    let legacy_bytes = b"not a SQLite database";
    fs::write(&legacy_database, legacy_bytes)?;
    let before = LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?;
    let workspace_root = fixture.colay_home.join("data/workspaces");
    fs::create_dir_all(&workspace_root)?;
    let before_workspace_directories = fs::read_dir(&workspace_root)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<BTreeSet<_>>>()?;

    let output = fixture.colay_in(
        &unregistered,
        [
            "--config",
            explicit
                .file_name()
                .and_then(|name| name.to_str())
                .context("explicit config filename is invalid")?,
            "--json",
            "doctor",
        ],
    )?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    let legacy_import = check_named(&document, "legacy_import")?;
    assert_eq!(legacy_import["status"], "warn");
    assert_eq!(
        legacy_import["detail"],
        "import readiness is unavailable through live-daemon IPC"
    );
    assert_eq!(
        legacy_import["data"]["source_database"],
        json!(&legacy_database)
    );
    assert_eq!(check_named(&document, "workspace")?["status"], "warn");
    assert_eq!(
        LegacyDoctorMutationSnapshot::capture(&fixture, Some(&legacy_database))?,
        before
    );
    let after_workspace_directories = fs::read_dir(workspace_root)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<BTreeSet<_>>>()?;
    assert_eq!(after_workspace_directories, before_workspace_directories);
    Ok(())
}

#[test]
fn doctor_fails_a_corrupt_workspace_artifact_reference() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    fixture.seed_corrupt_artifact_reference()?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(output.status.success());
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(check_named(&document, "artifacts")?["status"], "fail");
    assert_eq!(document["data"]["passed"], false);
    Ok(())
}
