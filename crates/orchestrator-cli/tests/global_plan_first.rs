#![cfg(feature = "test-fixtures")]

use std::{
    env, fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Mutex, MutexGuard},
};

use anyhow::{Context as _, Result, bail};
use rusqlite::Connection;

struct PlanFixture {
    _serial: MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    root: PathBuf,
    repository: PathBuf,
    colay_home: PathBuf,
}

impl PlanFixture {
    fn non_git() -> Result<Self> {
        Self::new(false)
    }

    fn committed_git() -> Result<Self> {
        Self::new(true)
    }

    fn new(committed_git: bool) -> Result<Self> {
        let serial = plan_fixture_guard();
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let repository = root.join("repository");
        let colay_home = root.join("home");
        fs::create_dir_all(&repository)?;
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
        let fixture = Self {
            _serial: serial,
            _temp: temp,
            root,
            repository,
            colay_home,
        };
        if committed_git {
            fixture.git(&["init", "--quiet"])?;
            fixture.git(&["config", "user.name", "Plan First E2E"])?;
            fixture.git(&["config", "user.email", "plan-first-e2e@example.invalid"])?;
            fs::write(fixture.repository.join("README.md"), "plan-first fixture\n")?;
            fixture.git(&["add", "README.md"])?;
            fixture.git(&["commit", "--quiet", "-m", "fixture base"])?;
        }
        Ok(fixture)
    }

    fn colay<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        #[cfg(windows)]
        let system_root = env::var_os("SystemRoot").context("SystemRoot is not set")?;
        #[cfg(not(windows))]
        let system_root = "/";
        let executable = PathBuf::from(env!("CARGO_BIN_EXE_colay"));
        let executable_parent = executable
            .parent()
            .context("colay binary has no parent")?
            .to_path_buf();
        let inherited_path = env::var_os("PATH").unwrap_or_default();
        let command_path = env::join_paths(
            std::iter::once(executable_parent).chain(env::split_paths(&inherited_path)),
        )?;
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        let status = Command::new(executable)
            .args(args)
            .current_dir(&self.repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
            .env("PATH", command_path)
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

    fn git(&self, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.repository)
            .output()
            .context("failed to invoke git")?;
        if !output.status.success() {
            bail!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    fn sessions(&self) -> Result<i64> {
        self.count("sessions")
    }

    fn tasks(&self) -> Result<i64> {
        self.count("tasks")
    }

    fn worktrees(&self) -> Result<i64> {
        self.count("worktrees")
    }

    fn count(&self, table: &str) -> Result<i64> {
        let database = Connection::open(self.colay_home.join("state/state.db"))?;
        database
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(Into::into)
    }

    fn database(&self) -> Result<Connection> {
        Connection::open(self.colay_home.join("state/state.db")).map_err(Into::into)
    }

    fn fake_conversation_starts(&self) -> Result<u64> {
        let marker = fs::read(self.root.join("colay-fake-conversation-starts.json"))?;
        let marker: serde_json::Value = serde_json::from_slice(&marker)?;
        marker["invocation_count"]
            .as_u64()
            .context("fake conversation marker has no invocation_count")
    }
}

impl Drop for PlanFixture {
    fn drop(&mut self) {
        let _ = self.colay(["daemon", "stop"]);
    }
}

fn plan_fixture_guard() -> MutexGuard<'static, ()> {
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    match TEST_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
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

#[test]
fn run_in_non_git_directory_creates_conversation_without_writable_state() -> Result<()> {
    let fixture = PlanFixture::non_git()?;
    let output = fixture.colay(["run", "hello"])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.sessions()?, 1);
    assert_eq!(fixture.tasks()?, 0);
    assert_eq!(fixture.worktrees()?, 0);
    assert!(!fixture.repository.join(".colay").exists());
    Ok(())
}

#[test]
fn plan_only_session_cannot_be_promoted_by_same_command() -> Result<()> {
    let fixture = PlanFixture::committed_git()?;
    let output = fixture.colay(["run", "--plan-only", "change code"])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.tasks()?, 0);
    assert_eq!(fixture.worktrees()?, 0);
    Ok(())
}

#[test]
fn provider_failure_exits_nonzero_with_actionable_redacted_outcome() -> Result<()> {
    let fixture = PlanFixture::non_git()?;
    let output = fixture.colay(["run", "--plan-only", "scenario:crash"])?;
    assert!(
        !output.status.success(),
        "provider failure unexpectedly exited zero: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("process failed"), "{stderr}");
    assert!(!stderr.contains("supersecretvalue"), "{stderr}");

    let database = fixture.database()?;
    let (status, outcome_json, error_redacted): (String, String, String) = database.query_row(
        "SELECT status, outcome_json, error_redacted FROM conversation_attempts LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(status, "failed");
    assert!(outcome_json.contains("needs_attention"));
    assert!(error_redacted.contains("process failed"));
    let (command_state, command_outcome): (String, String) = database.query_row(
        "SELECT state, outcome FROM client_commands
         WHERE action = 'request_conversation_turn' LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(command_state, "failed");
    assert!(command_outcome.contains("process failed"));
    for table in [
        "tasks",
        "task_attempts",
        "worktrees",
        "coordinator_leases",
        "worker_leases",
    ] {
        assert_eq!(fixture.count(table)?, 0, "unexpected row in {table}");
    }
    assert_eq!(fixture.fake_conversation_starts()?, 1);
    Ok(())
}
