#![cfg(feature = "test-fixtures")]

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context as _, Result};
use rusqlite::Connection;
use serde_json::{Value, json};

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

        let mut command = Command::new(env!("CARGO_BIN_EXE_colay"));
        command
            .args(args)
            .current_dir(&self.repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
            .env("PATH", test_path()?)
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root)
            .env("TEMP", &self.temp_root)
            .env("TMP", &self.temp_root);
        command.output().context("failed to invoke colay")
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

    fn git<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        Command::new("git")
            .args(args)
            .current_dir(&self.repository)
            .output()
            .context("failed to invoke git")
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
    let database = json["data"]["checks"]
        .as_array()
        .context("doctor checks must be an array")?
        .iter()
        .find(|check| check["name"] == "database")
        .context("doctor must report absent database state")?;
    assert_eq!(
        database["detail"],
        "state database does not exist; run `colay init` or the first `colay run` (including `--plan-only`) to initialize it; `colay migrate apply` is only for an existing database with pending schemas"
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
fn first_plan_only_run_initializes_local_state() -> Result<()> {
    let fixture = CliFixture::new()?;

    let output = fixture.colay(["run", "inspect repository", "--plan-only"])?;

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.repository.join(".colay/orchestrator.db").is_file());
    assert!(fixture.repository.join(".colay/events.jsonl").is_file());
    Ok(())
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
fn failed_event_reconciliation_blocks_retries_before_task_mutation() -> Result<()> {
    let fixture = CliFixture::new()?;
    let state = fixture.repository.join(".colay");
    fs::create_dir_all(&state)?;
    fs::write(state.join("events.jsonl"), "not valid jsonl\n")?;

    for attempt in 1..=2 {
        let output = fixture.colay(["run", "inspect repository", "--plan-only"])?;
        assert!(
            !output.status.success(),
            "attempt {attempt} unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    let status = fixture.colay(["--json", "status"])?;
    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(status["data"]["tasks"], Value::Array(Vec::new()));
    assert_eq!(status["data"]["database"]["last_event_sequence"], 0);

    let database = Connection::open(state.join("orchestrator.db"))?;
    let tasks: i64 = database.query_row("SELECT count(*) FROM tasks", [], |row| row.get(0))?;
    let events: i64 =
        database.query_row("SELECT count(*) FROM task_events", [], |row| row.get(0))?;
    let exported: i64 = database.query_row(
        "SELECT last_exported_sequence FROM event_log_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(tasks, 0);
    assert_eq!(events, 0);
    assert_eq!(exported, 0);
    Ok(())
}

#[test]
fn direct_run_rejects_non_git_before_state_mutation() -> Result<()> {
    let fixture = CliFixture::new()?;
    fixture.configure_fake_codex()?;

    let output = fixture.colay(["run", "hello"])?;

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("requires a Git repository"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.repository.join(".colay").exists());
    Ok(())
}

#[test]
fn direct_run_rejects_unborn_head_before_state_mutation() -> Result<()> {
    let fixture = CliFixture::new()?;
    let git = fixture.git(["init", "--quiet"])?;
    assert!(
        git.status.success(),
        "git stderr: {}",
        String::from_utf8_lossy(&git.stderr)
    );
    fixture.configure_fake_codex()?;

    let output = fixture.colay(["run", "hello"])?;

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no base commit"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.repository.join(".colay").exists());
    Ok(())
}
