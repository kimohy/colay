#![cfg(feature = "test-fixtures")]

use std::{
    env,
    ffi::OsString,
    fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::{Context as _, Result};
use chrono::Utc;
use orchestrator_state::STATE_SCHEMA_VERSION;
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

struct CliFixture {
    _temp: tempfile::TempDir,
    temp_root: PathBuf,
    repository: PathBuf,
    colay_home: PathBuf,
}

impl CliFixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let temp_root = fs::canonicalize(temp.path())?;
        let repository = temp_root.join("repository");
        let colay_home = temp_root.join("home/.colay");
        fs::create_dir_all(&repository)?;
        Ok(Self {
            _temp: temp,
            temp_root,
            repository,
            colay_home,
        })
    }

    fn colay<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        #[cfg(windows)]
        let system_root = system_root()?;
        #[cfg(not(windows))]
        let system_root = system_root();

        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        let status = Command::new(env!("CARGO_BIN_EXE_colay"))
            .args(args)
            .current_dir(&self.repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
            .env("PATH", test_path()?)
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root)
            .env("TEMP", &self.temp_root)
            .env("TMP", &self.temp_root)
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

    fn configure_fake_codex(&self) -> Result<()> {
        fs::create_dir_all(&self.colay_home)?;
        fs::write(
            self.colay_home.join("config.toml"),
            format!(
                "config_version = 4\n[orchestrator.providers.codex]\nexecutable = {}\n",
                toml_path(&fake_provider_binary())
            ),
        )?;
        Ok(())
    }

    fn seed_v8_database(&self) -> Result<PathBuf> {
        let state = self.colay_home.join("state");
        fs::create_dir_all(&state)?;
        let database_path = state.join("state.db");
        let connection = Connection::open(&database_path)?;
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
                    Utc::now().to_rfc3339()
                ],
            )?;
        }
        Ok(database_path)
    }
}

fn fake_provider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_colay-e2e-fake-provider"))
}

fn fake_provider_path() -> Result<PathBuf> {
    fake_provider_binary()
        .parent()
        .context("fake provider binary has no parent directory")
        .map(Path::to_path_buf)
}

fn test_path() -> Result<OsString> {
    let mut paths = vec![fake_provider_path()?];
    if let Some(host_path) = env::var_os("PATH") {
        paths.extend(env::split_paths(&host_path));
    }
    env::join_paths(paths).context("failed to construct test PATH")
}

#[cfg(windows)]
fn system_root() -> Result<PathBuf> {
    env::var_os("SystemRoot")
        .map(PathBuf::from)
        .context("SystemRoot must be set for Windows subprocess tests")
}

#[cfg(not(windows))]
fn system_root() -> PathBuf {
    PathBuf::from("/")
}

fn toml_path(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

#[test]
fn doctor_uses_defaults_without_creating_repository_state() -> Result<()> {
    let fixture = CliFixture::new()?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.repository.join(".colay").exists());
    let json: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["data"]["checks"][0]["status"], "pass");
    let runtime = json["data"]["checks"]
        .as_array()
        .context("doctor checks must be an array")?
        .iter()
        .find(|check| check["name"] == "runtime")
        .context("doctor must report the running Colay binary")?;
    assert_eq!(runtime["status"], "pass");
    assert_eq!(runtime["data"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        runtime["data"]["executable"]
            .as_str()
            .is_some_and(|path| !path.trim().is_empty())
    );
    assert_eq!(runtime["data"]["target_os"], std::env::consts::OS);
    assert_eq!(runtime["data"]["target_arch"], std::env::consts::ARCH);
    let state = json["data"]["checks"]
        .as_array()
        .context("doctor checks must be an array")?
        .iter()
        .find(|check| check["name"] == "state")
        .context("doctor must report user-global database state")?;
    assert_eq!(state["status"], "pass");
    assert_eq!(
        PathBuf::from(
            state["data"]["database"]
                .as_str()
                .context("state check omitted the global database path")?
        ),
        fixture.colay_home.join("state/state.db")
    );
    Ok(())
}

#[test]
fn doctor_reports_fake_provider_executable_resolution() -> Result<()> {
    let fixture = CliFixture::new()?;
    fixture.configure_fake_codex()?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.repository.join(".colay").exists());
    let json: Value = serde_json::from_slice(&output.stdout)?;
    let codex = json["data"]["checks"]
        .as_array()
        .context("doctor checks must be an array")?
        .iter()
        .find(|check| check["name"] == "provider_codex")
        .context("doctor must report the configured fake Codex provider")?;
    let expected = fake_provider_binary();
    assert_eq!(codex["data"]["configured_executable"], json!(expected));
    assert_eq!(codex["data"]["resolved_executable"], json!(expected));
    assert_eq!(codex["data"]["executable_kind"], "native");
    Ok(())
}

#[test]
fn first_plan_only_run_initializes_global_state() -> Result<()> {
    let fixture = CliFixture::new()?;

    let first = fixture.colay(["run", "inspect repository", "--plan-only"])?;

    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(!fixture.repository.join(".colay").exists());
    let connection = Connection::open(fixture.colay_home.join("state/state.db"))?;
    let workspace_id: String = connection.query_row(
        "SELECT workspace_id FROM workspaces WHERE status = 'active'",
        [],
        |row| row.get(0),
    )?;
    assert_ne!(workspace_id, "00000000-0000-0000-0000-000000000001");
    Ok(())
}

impl Drop for CliFixture {
    fn drop(&mut self) {
        let _ = self.colay(["daemon", "stop"]);
    }
}

#[test]
fn compatibility_and_status_do_not_create_repository_state() -> Result<()> {
    let fixture = CliFixture::new()?;
    fixture.configure_fake_codex()?;

    let compatibility = fixture.colay(["--json", "compatibility"])?;
    assert!(
        compatibility.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&compatibility.stderr)
    );
    let status = fixture.colay(["--json", "status"])?;
    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(!fixture.repository.join(".colay").exists());
    Ok(())
}

#[test]
fn migrate_apply_upgrades_existing_database_without_repository_config() -> Result<()> {
    let fixture = CliFixture::new()?;
    fixture.configure_fake_codex()?;
    let database_path = fixture.seed_v8_database()?;

    let output = fixture.colay(["--json", "migrate", "apply"])?;

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.repository.join(".colay/config.toml").exists());
    let connection = Connection::open(database_path)?;
    assert_eq!(
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?,
        STATE_SCHEMA_VERSION
    );
    let applied = connection
        .prepare("SELECT version FROM schema_migrations WHERE version >= 9 ORDER BY version")?
        .query_map([], |row| row.get::<_, u32>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(applied, (9..=STATE_SCHEMA_VERSION).collect::<Vec<_>>());
    let backups =
        fs::read_dir(fixture.colay_home.join("state/backups"))?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(backups.len(), 1);
    assert!(backups[0].file_type()?.is_file());
    Ok(())
}
