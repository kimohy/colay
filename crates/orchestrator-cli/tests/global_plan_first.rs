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
                "config_version = 4\n\
                 [orchestrator.providers.codex]\nexecutable = {fake}\n\
                 [orchestrator.providers.claude]\nexecutable = {fake}\n\
                 [orchestrator.providers.gemini]\nexecutable = {fake}\n\
                 [orchestrator.providers.agy]\nexecutable = {fake}\n",
                fake = toml_path(&PathBuf::from(env!(
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
        self.colay_in(&self.repository, args)
    }

    fn colay_in<const N: usize>(&self, repository: &Path, args: [&str; N]) -> Result<Output> {
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
            .current_dir(repository)
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

    fn plan_only(&self, provider: &str, scenario: &str) -> Result<Output> {
        let marker = self.fake_conversation_marker_path();
        if marker.try_exists()? {
            fs::remove_file(&marker)?;
        }
        let workspace = self.root.join("matrix-workspaces").join(format!(
            "{provider}-{}",
            scenario.trim_start_matches("scenario:")
        ));
        fs::create_dir_all(&workspace)?;
        self.colay_in(
            &workspace,
            ["run", "--plan-only", "--provider", provider, scenario],
        )
    }

    fn stop_daemon(&self) -> Result<()> {
        let output = self.colay(["daemon", "stop"])?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
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
        let marker = fs::read(self.fake_conversation_marker_path())?;
        let marker: serde_json::Value = serde_json::from_slice(&marker)?;
        marker["invocation_count"]
            .as_u64()
            .context("fake conversation marker has no invocation_count")
    }

    fn fake_conversation_marker_path(&self) -> PathBuf {
        self.root.join("colay-fake-conversation-starts.json")
    }

    fn assert_fake_conversation_marker(&self, provider: &str, scenario: &str) -> Result<()> {
        let marker = fs::read(self.fake_conversation_marker_path())?;
        let marker: serde_json::Value = serde_json::from_slice(&marker)?;
        assert_eq!(marker["invocation_count"], 1);
        assert_eq!(marker["provider"], provider);
        assert_eq!(marker["scenario"], scenario);
        Ok(())
    }

    fn conversation_attempt(
        &self,
        provider: &str,
    ) -> Result<(String, String, Option<String>, Option<String>)> {
        self.database()?
            .query_row(
                "SELECT status, outcome_json, error_redacted, evidence_redacted
             FROM conversation_attempts WHERE provider_id = ?1 LIMIT 1",
                [provider],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(Into::into)
    }

    fn assert_database_health(&self) -> Result<()> {
        let database = self.database()?;
        let integrity: String =
            database.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        assert_eq!(integrity, "ok");
        let mut foreign_keys = database.prepare("PRAGMA foreign_key_check")?;
        assert!(foreign_keys.query([])?.next()?.is_none());
        Ok(())
    }

    fn assert_no_writable_state(&self) -> Result<()> {
        for table in [
            "tasks",
            "task_attempts",
            "worktrees",
            "coordinator_leases",
            "worker_leases",
        ] {
            assert_eq!(self.count(table)?, 0, "unexpected row in {table}");
        }
        Ok(())
    }

    fn assert_matrix_workspace_isolation(&self) -> Result<()> {
        let database = self.database()?;
        let (attempts, workspaces): (i64, i64) = database.query_row(
            "SELECT count(*), count(DISTINCT workspace_id) FROM conversation_attempts",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(attempts, 4);
        assert_eq!(workspaces, 4);
        Ok(())
    }

    fn scanned_surfaces(&self, output: &Output) -> Result<Vec<ByteSurface>> {
        let mut surfaces = vec![
            ByteSurface::new("stdout", output.stdout.clone()),
            ByteSurface::new("stderr", output.stderr.clone()),
        ];
        collect_file_surfaces(&self.colay_home.join("state"), &mut surfaces)?;
        collect_jsonl_surfaces(&self.root, &mut surfaces)?;
        collect_file_surfaces(&self.colay_home.join("data/workspaces"), &mut surfaces)?;
        Ok(surfaces)
    }
}

struct ByteSurface {
    label: String,
    bytes: Vec<u8>,
}

impl ByteSurface {
    fn new(label: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            label: label.into(),
            bytes,
        }
    }

    fn contains(&self, needle: &[u8]) -> bool {
        self.label
            .as_bytes()
            .windows(needle.len())
            .any(|window| window == needle)
            || self
                .bytes
                .windows(needle.len())
                .any(|window| window == needle)
    }
}

fn push_surface(path: &Path, surfaces: &mut Vec<ByteSurface>) -> Result<()> {
    if path.try_exists()? && path.metadata()?.is_file() {
        surfaces.push(ByteSurface::new(
            path.display().to_string(),
            fs::read(path)?,
        ));
    }
    Ok(())
}

fn collect_jsonl_surfaces(path: &Path, surfaces: &mut Vec<ByteSurface>) -> Result<()> {
    if !path.try_exists()? {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_jsonl_surfaces(&entry.path(), surfaces)?;
        } else if file_type.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|value| value == "jsonl")
        {
            push_surface(&entry.path(), surfaces)?;
        }
    }
    Ok(())
}

fn collect_file_surfaces(path: &Path, surfaces: &mut Vec<ByteSurface>) -> Result<()> {
    if !path.try_exists()? {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_file_surfaces(&entry.path(), surfaces)?;
        } else if file_type.is_file() {
            push_surface(&entry.path(), surfaces)?;
        }
    }
    Ok(())
}

fn surface_contains(surfaces: &[ByteSurface], needle: &[u8]) -> bool {
    surfaces.iter().any(|surface| surface.contains(needle))
}

fn assert_surfaces_exclude(surfaces: &[ByteSurface], needle: &[u8]) -> Result<()> {
    if let Some(surface) = surfaces.iter().find(|surface| surface.contains(needle)) {
        bail!("sensitive or over-bound bytes found on {}", surface.label);
    }
    Ok(())
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
fn read_only_provider_command_completes_with_durable_evidence_and_zero_writable_state() -> Result<()>
{
    let fixture = PlanFixture::non_git()?;
    let output = fixture.colay(["run", "--plan-only", "scenario:read-only-command"])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let database = fixture.database()?;
    let (status, outcome_json, evidence_redacted): (String, String, String) = database.query_row(
        "SELECT status, outcome_json, evidence_redacted FROM conversation_attempts LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(status, "succeeded");
    let outcome: serde_json::Value = serde_json::from_str(&outcome_json)?;
    assert_eq!(outcome["outcome"], "answer_complete");
    assert!(outcome.get("evidence_redacted").is_none());
    assert!(evidence_redacted.contains("read-only provider command started"));
    for table in [
        "tasks",
        "task_attempts",
        "worktrees",
        "coordinator_leases",
        "worker_leases",
    ] {
        assert_eq!(fixture.count(table)?, 0, "unexpected row in {table}");
    }
    Ok(())
}

#[test]
fn file_change_after_read_only_command_fails_without_writable_state() -> Result<()> {
    let fixture = PlanFixture::non_git()?;
    let output = fixture.colay([
        "run",
        "--plan-only",
        "scenario:read-only-command-file-change",
    ])?;
    assert!(
        !output.status.success(),
        "file-change conversation unexpectedly succeeded: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let database = fixture.database()?;
    let (status, outcome_json, error_redacted, evidence_redacted): (
        String,
        String,
        String,
        Option<String>,
    ) = database.query_row(
        "SELECT status, outcome_json, error_redacted, evidence_redacted
         FROM conversation_attempts LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    assert_eq!(status, "failed");
    let outcome: serde_json::Value = serde_json::from_str(&outcome_json)?;
    assert_eq!(outcome["outcome"], "needs_attention");
    assert!(error_redacted.contains("process failed"));
    assert!(
        outcome["evidence_redacted"].as_str().is_some_and(
            |evidence| evidence.contains("read-only conversation reported a file change")
        )
    );
    assert_eq!(evidence_redacted, None);
    for table in [
        "tasks",
        "task_attempts",
        "worktrees",
        "coordinator_leases",
        "worker_leases",
    ] {
        assert_eq!(fixture.count(table)?, 0, "unexpected row in {table}");
    }
    Ok(())
}

#[test]
fn ambiguous_scalar_prefix_fails_closed_without_writable_state() -> Result<()> {
    let fixture = PlanFixture::non_git()?;
    let output = fixture.colay(["run", "--plan-only", "scenario:ambiguous-scalar-prefix"])?;
    assert!(
        !output.status.success(),
        "ambiguous provider output unexpectedly succeeded: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let database = fixture.database()?;
    let (status, outcome_json, error_redacted): (String, String, String) = database.query_row(
        "SELECT status, outcome_json, error_redacted FROM conversation_attempts LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(status, "failed");
    let outcome: serde_json::Value = serde_json::from_str(&outcome_json)?;
    assert_eq!(outcome["outcome"], "needs_attention");
    let evidence = outcome["evidence_redacted"]
        .as_str()
        .context("failed outcome has no evidence")?;
    assert!(evidence.contains("Checking the request."));
    assert!(evidence.contains("\nnull\n"));
    assert!(evidence.contains(r#""outcome":"answer_complete""#));
    assert!(
        error_redacted.contains("incompatible with the required read-only conversation protocol"),
        "{error_redacted}"
    );
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

#[test]
fn decoded_secret_is_redacted_across_every_provider_and_durable_surface() -> Result<()> {
    let fixture = PlanFixture::non_git()?;
    for provider in ["codex", "claude", "gemini", "agy"] {
        let output = fixture.plan_only(provider, "scenario:decoded-secret")?;
        assert!(
            output.status.success(),
            "{provider}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture.stop_daemon()?;
        fixture.assert_fake_conversation_marker(provider, "decoded-secret")?;
        let (status, outcome_json, error_redacted, _) = fixture.conversation_attempt(provider)?;
        assert_eq!(status, "succeeded", "{provider}");
        assert_eq!(error_redacted, None, "{provider}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&outcome_json)?,
            serde_json::json!({
                "outcome": "answer_complete",
                "response_redacted": "api_key=[REDACTED]"
            }),
            "{provider}"
        );
        let surfaces = fixture.scanned_surfaces(&output)?;
        assert!(
            surface_contains(&surfaces, b"[REDACTED]"),
            "{provider}: redaction positive control was absent"
        );
        assert_surfaces_exclude(&surfaces, b"api_key=secret-token")?;
        fixture.assert_database_health()?;
        fixture.assert_no_writable_state()?;
    }
    fixture.assert_matrix_workspace_isolation()?;
    Ok(())
}

#[test]
fn durable_surface_scan_includes_rollback_journal_and_artifact_path_names() -> Result<()> {
    let canary = b"api_key=secret-token";
    let journal_detected = {
        let fixture = PlanFixture::non_git()?;
        let output = fixture.colay(["--help"])?;
        let journal = fixture.colay_home.join("state/state.db-journal");
        fs::create_dir_all(journal.parent().context("journal has no parent")?)?;
        fs::write(&journal, canary)?;
        assert_surfaces_exclude(&fixture.scanned_surfaces(&output)?, canary).is_err()
    };
    let artifact_path_detected = {
        let fixture = PlanFixture::non_git()?;
        let output = fixture.colay(["--help"])?;
        let artifact = fixture
            .colay_home
            .join("data/workspaces/nested/api_key=secret-token.txt");
        fs::create_dir_all(artifact.parent().context("artifact has no parent")?)?;
        fs::write(artifact, b"safe artifact contents")?;
        assert_surfaces_exclude(&fixture.scanned_surfaces(&output)?, canary).is_err()
    };

    assert_eq!((journal_detected, artifact_path_detected), (true, true));
    Ok(())
}

#[test]
fn byte_overflow_fails_boundedly_once_across_every_provider() -> Result<()> {
    assert_overflow_matrix(
        "scenario:byte-overflow",
        "byte-overflow",
        b"byte-overflow-sentinel",
    )
}

#[test]
fn event_overflow_fails_boundedly_once_across_every_provider() -> Result<()> {
    assert_overflow_matrix(
        "scenario:event-overflow",
        "event-overflow",
        b"event overflow complete",
    )
}

fn assert_overflow_matrix(
    scenario: &str,
    marker_scenario: &str,
    over_bound_sentinel: &[u8],
) -> Result<()> {
    let fixture = PlanFixture::non_git()?;
    for provider in ["codex", "claude", "gemini", "agy"] {
        let output = fixture.plan_only(provider, scenario)?;
        assert!(
            !output.status.success(),
            "{provider}/{marker_scenario}: overflow unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        fixture.stop_daemon()?;
        fixture.assert_fake_conversation_marker(provider, marker_scenario)?;
        let (status, outcome_json, error_redacted, evidence_column) =
            fixture.conversation_attempt(provider)?;
        assert_eq!(
            status, "failed",
            "{provider}/{marker_scenario}: error={error_redacted:?}, outcome={outcome_json}"
        );
        assert_eq!(evidence_column, None, "{provider}/{marker_scenario}");
        let outcome: serde_json::Value = serde_json::from_str(&outcome_json)?;
        assert_eq!(
            outcome["outcome"], "needs_attention",
            "{provider}/{marker_scenario}"
        );
        assert!(
            !outcome_json.contains("answer_complete"),
            "{provider}/{marker_scenario}: success outcome survived overflow"
        );
        let evidence = outcome["evidence_redacted"]
            .as_str()
            .context("overflow outcome has no evidence_redacted")?;
        assert!(
            evidence.len() <= 16 * 1024,
            "{provider}/{marker_scenario}: evidence was {} bytes",
            evidence.len()
        );
        let error_redacted = error_redacted.context("overflow attempt has no error_redacted")?;
        assert!(
            error_redacted.contains("process failed"),
            "{provider}/{marker_scenario}: {error_redacted}"
        );
        assert!(
            !error_redacted.contains("was cancelled"),
            "{provider}/{marker_scenario}: internal safety loss became user cancellation"
        );
        let succeeded = fixture.database()?.query_row(
            "SELECT count(*) FROM conversation_attempts
             WHERE provider_id = ?1 AND status = 'succeeded'",
            [provider],
            |row| row.get::<_, i64>(0),
        )?;
        assert_eq!(succeeded, 0, "{provider}/{marker_scenario}");
        let surfaces = fixture.scanned_surfaces(&output)?;
        assert_surfaces_exclude(&surfaces, over_bound_sentinel)?;
        fixture.assert_database_health()?;
        fixture.assert_no_writable_state()?;
    }
    fixture.assert_matrix_workspace_isolation()?;
    Ok(())
}
