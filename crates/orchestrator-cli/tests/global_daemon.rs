#![cfg(feature = "test-fixtures")]

use std::{
    env, fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::{Context as _, Result};
use chrono::Utc;
use orchestrator_daemon::{IPC_SCHEMA_VERSION, IpcRequest};
use orchestrator_state::{STATE_SCHEMA_VERSION, WorkspaceId};
use rusqlite::{Connection, params};
use serde_json::json;
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

struct GlobalDaemonFixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    first: PathBuf,
    second: PathBuf,
    colay_home: PathBuf,
}

impl GlobalDaemonFixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let first = root.join("first");
        let second = root.join("second");
        let colay_home = root.join("home");
        fs::create_dir_all(&first)?;
        fs::create_dir_all(&second)?;
        fs::create_dir_all(&colay_home)?;
        fs::write(
            colay_home.join("config.toml"),
            format!(
                "config_version = 4\n[orchestrator.providers.codex]\nexecutable = {}\n",
                toml_path(&PathBuf::from(env!(
                    "CARGO_BIN_EXE_colay-e2e-fake-provider"
                )))
            ),
        )?;
        Ok(Self {
            _temp: temp,
            root,
            first,
            second,
            colay_home,
        })
    }

    fn with_schema(version: u32) -> Result<Self> {
        let fixture = Self::new()?;
        let database = fixture.global_database();
        fs::create_dir_all(database.parent().context("global database has no parent")?)?;
        let connection = Connection::open(database)?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        for (migration_version, name, sql) in MIGRATIONS_THROUGH_V8 {
            if *migration_version > version {
                break;
            }
            connection.execute_batch(sql)?;
            connection.execute(
                "INSERT INTO schema_migrations(version, name, checksum, applied_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    migration_version,
                    name,
                    format!("{:x}", Sha256::digest(sql.as_bytes())),
                    Utc::now().to_rfc3339(),
                ],
            )?;
        }
        drop(connection);
        Ok(fixture)
    }

    fn configure_legacy_state_dir(&self, state_dir: &str) -> Result<()> {
        fs::write(
            self.colay_home.join("config.toml"),
            format!(
                "config_version = 4\n[orchestrator]\nstate_dir = \"{state_dir}\"\n\
                 [orchestrator.providers.codex]\nexecutable = {}\n",
                toml_path(&PathBuf::from(env!(
                    "CARGO_BIN_EXE_colay-e2e-fake-provider"
                )))
            ),
        )?;
        Ok(())
    }

    fn run<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        self.run_in(&self.first, args)
    }

    fn run_in<const N: usize>(&self, repository: &Path, args: [&str; N]) -> Result<Output> {
        let executable = PathBuf::from(env!("CARGO_BIN_EXE_colay"));
        let executable_parent = executable
            .parent()
            .context("colay binary has no parent")?
            .to_path_buf();
        #[cfg(windows)]
        let system_root = env::var_os("SystemRoot").context("SystemRoot is not set")?;
        #[cfg(not(windows))]
        let system_root = "/";

        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        let status = Command::new(executable)
            .args(args)
            .current_dir(repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
            .env("PATH", executable_parent)
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

    fn database_files(&self) -> Result<Vec<PathBuf>> {
        fn collect(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
            if !directory.exists() {
                return Ok(());
            }
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    collect(&path, files)?;
                } else if path.extension().is_some_and(|extension| extension == "db") {
                    files.push(path);
                }
            }
            Ok(())
        }

        let mut files = Vec::new();
        collect(&self.root, &mut files)?;
        Ok(files)
    }

    fn online_daemon_instances(&self) -> Result<u64> {
        let connection = Connection::open(self.global_database())?;
        connection
            .query_row(
                "SELECT count(*) FROM daemon_instances WHERE released_at IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    fn schema_version(&self) -> Result<u32> {
        Connection::open(self.global_database())?
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Into::into)
    }

    fn global_database(&self) -> PathBuf {
        self.colay_home.join("state/state.db")
    }
}

fn toml_path(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

impl Drop for GlobalDaemonFixture {
    fn drop(&mut self) {
        let _ = self.run(["daemon", "stop"]);
    }
}

#[test]
fn two_repositories_share_one_global_daemon_and_database() -> Result<()> {
    let fixture = GlobalDaemonFixture::new()?;
    let first = fixture.run_in(&fixture.first, ["status"])?;
    let second = fixture.run_in(&fixture.second, ["status"])?;
    assert!(first.status.success() && second.status.success());
    assert_eq!(fixture.database_files()?.len(), 1);
    assert_eq!(fixture.online_daemon_instances()?, 1);
    Ok(())
}

#[test]
fn old_schema_migrates_before_untrusted_provider_is_evaluated() -> Result<()> {
    let fixture = GlobalDaemonFixture::with_schema(8)?;
    let output = fixture.run(["daemon", "start"])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.schema_version()?, STATE_SCHEMA_VERSION);
    Ok(())
}

#[test]
fn configured_legacy_state_dir_is_resolved_after_global_migration() -> Result<()> {
    let fixture = GlobalDaemonFixture::new()?;
    fixture.configure_legacy_state_dir(".legacy-colay")?;
    fs::create_dir_all(fixture.first.join(".colay"))?;
    fs::write(
        fixture.first.join(".colay/orchestrator.db"),
        b"corrupt default legacy database",
    )?;

    let output = fixture.run(["daemon", "start"])?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.schema_version()?, STATE_SCHEMA_VERSION);
    Ok(())
}

#[test]
fn ipc_requests_use_the_versioned_newline_json_contract() -> Result<()> {
    let workspace_id = "018f68d2-00f0-7000-8000-000000000001".parse::<WorkspaceId>()?;
    let request = IpcRequest {
        schema_version: IPC_SCHEMA_VERSION,
        request_id: "request-1".to_owned(),
        workspace_id: Some(workspace_id),
        action: "daemon.status".to_owned(),
        payload: json!({}),
    };

    let mut encoded = serde_json::to_string(&request)?;
    encoded.push('\n');

    assert!(encoded.ends_with('\n'));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&encoded)?["schema_version"],
        1
    );
    assert_eq!(serde_json::from_str::<IpcRequest>(&encoded)?, request);
    Ok(())
}
