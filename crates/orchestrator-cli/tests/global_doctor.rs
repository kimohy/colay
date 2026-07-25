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
use rusqlite::{Connection, params};
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
                 [orchestrator.providers.claude]\nexecutable = {executable}\n"
            ),
        )?;
        Ok(())
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

    fn schema_version(&self) -> Result<u32> {
        Connection::open(self.global_database())?
            .query_row("PRAGMA user_version", [], |row| row.get(0))
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
    fixture.configure_fake_providers()?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.repository.join(".colay").exists());
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
    ] {
        check_named(&document, name)?;
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
