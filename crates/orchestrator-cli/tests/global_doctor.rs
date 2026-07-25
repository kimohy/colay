#![cfg(feature = "test-fixtures")]

use std::{
    env, fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};

use anyhow::{Context as _, Result};
use chrono::Utc;
use orchestrator_state::{Database, STATE_SCHEMA_VERSION};
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
        Ok(fixture)
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

    fn seed_corrupt_explicit_legacy_config(&self) -> Result<PathBuf> {
        let config = self.repository.join("doctor-explicit.toml");
        fs::write(
            &config,
            "config_version = 4\n[orchestrator]\nstate_dir = \"chosen-state\"\n",
        )?;
        let selected_state = self.repository.join("chosen-state");
        fs::create_dir_all(&selected_state)?;
        fs::write(
            selected_state.join("orchestrator.db"),
            b"not a SQLite database",
        )?;
        Ok(config)
    }

    fn colay<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        #[cfg(windows)]
        let system_root = env::var_os("SystemRoot").context("SystemRoot is not set")?;
        #[cfg(not(windows))]
        let system_root = "/";
        let status = Command::new(env!("CARGO_BIN_EXE_colay"))
            .args(args)
            .current_dir(&self.repository)
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
        }
    }
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
        assert_eq!(
            PathBuf::from(
                check_named(&document, provider)?["data"]["configured_executable"]
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

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
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
    Ok(())
}

#[test]
fn live_doctor_registration_honors_the_explicit_config_state_dir() -> Result<()> {
    let fixture = DoctorFixture::new()?;
    fixture.configure_fake_providers()?;
    let explicit = fixture.seed_corrupt_explicit_legacy_config()?;
    let started = fixture.colay(["daemon", "start"])?;
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );

    let output = fixture.colay([
        "--config",
        explicit
            .file_name()
            .and_then(|name| name.to_str())
            .context("explicit config filename is invalid")?,
        "--json",
        "doctor",
    ])?;

    assert!(output.status.success());
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(check_named(&document, "state")?["status"], "fail");
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
