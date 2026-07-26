#![cfg(feature = "test-fixtures")]

use std::{
    env, fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use chrono::{TimeDelta, Utc};
use orchestrator_domain::{
    AttemptId, CorrelationId, DaemonInstanceId, EventActor, EventId, EventType, ProviderId,
    RepoPath, SchemaVersion, TaskEnvelope, TaskEvent, TaskId, TaskState, TransitionGuards,
};
use orchestrator_state::{
    CoordinatorLeaseRequest, DaemonStatus, Database, NewTaskAttemptRecord, NewTaskRecord,
    WorkspaceId,
};

struct ResumeFixture {
    _temp: tempfile::TempDir,
    repository: PathBuf,
    colay_home: PathBuf,
    task_id: TaskId,
    workspace_id: Option<WorkspaceId>,
}

impl ResumeFixture {
    fn active_task() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let repository = root.join("repository");
        let colay_home = root.join("home");
        fs::create_dir_all(repository.join("src"))?;
        fs::create_dir_all(colay_home.join("temp"))?;
        fs::write(repository.join("src/lib.rs"), "pub fn fixture() {}\n")?;
        fs::write(
            colay_home.join("config.toml"),
            format!(
                "config_version = 4\n[orchestrator.providers.codex]\nexecutable = {}\n",
                toml_path(&PathBuf::from(env!(
                    "CARGO_BIN_EXE_colay-e2e-fake-provider"
                )))
            ),
        )?;

        let mut fixture = Self {
            _temp: temp,
            repository,
            colay_home,
            task_id: TaskId::new(),
            workspace_id: None,
        };
        let started = fixture.colay(["status"])?;
        if !started.status.success() {
            bail!(
                "failed to start fixture daemon: {}",
                String::from_utf8_lossy(&started.stderr)
            );
        }
        fixture.seed_active_task()?;
        Ok(fixture)
    }

    fn colay<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        let executable = PathBuf::from(env!("CARGO_BIN_EXE_colay"));
        let executable_parent = executable
            .parent()
            .context("colay binary has no parent")?
            .to_path_buf();
        let mut command = Command::new(executable);
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        command
            .args(args)
            .current_dir(&self.repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
            .env("PATH", executable_parent)
            .env("PATHEXT", ".EXE;.CMD")
            .env("TEMP", self.colay_home.join("temp"))
            .env("TMP", self.colay_home.join("temp"))
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?));
        #[cfg(windows)]
        command.env(
            "SystemRoot",
            env::var_os("SystemRoot").context("SystemRoot is not set")?,
        );
        let status = command.status().context("failed to invoke colay")?;
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

    fn seed_active_task(&mut self) -> Result<()> {
        let database = Database::open(self.global_database())?;
        let workspace_id = database
            .resolve_repository_workspace(&self.repository)?
            .workspace_id;
        self.workspace_id = Some(workspace_id);
        let deadline = Instant::now() + Duration::from_secs(10);
        let daemon_instance = loop {
            let status = database.daemon_status(Utc::now())?;
            match &status {
                DaemonStatus::Online(instance) => break instance.instance_id,
                DaemonStatus::Stopped | DaemonStatus::Failed(_) | DaemonStatus::Stale(_) => {
                    bail!("fixture daemon stopped before becoming online: {status:?}");
                }
                DaemonStatus::Booting(_) | DaemonStatus::Probing(_) => {}
            }
            if Instant::now() >= deadline {
                bail!("fixture daemon did not become online within ten seconds: {status:?}");
            }
            thread::sleep(Duration::from_millis(25));
        };
        let now = Utc::now();
        let envelope = TaskEnvelope {
            schema_version: SchemaVersion::v1(),
            task_id: self.task_id,
            objective: "keep the active fake worker attached".to_owned(),
            original_request_redacted: "active resume fixture".to_owned(),
            constraints: Vec::new(),
            acceptance_criteria: vec!["do not start a second attempt".to_owned()],
            allowed_write_paths: vec![RepoPath::try_from("src/lib.rs")?],
            repository_wide_write_scope: false,
            assessment: None,
            created_at: now,
        };
        let workspace = database.workspace(workspace_id);
        workspace.create_task(&NewTaskRecord {
            task_id: self.task_id,
            schema_version: SchemaVersion::v1().to_string(),
            state: TaskState::Running,
            objective: envelope.objective.clone(),
            original_request_redacted: envelope.original_request_redacted.clone(),
            envelope,
            created_at: now,
        })?;
        workspace.record_task_attempt_started(&NewTaskAttemptRecord {
            attempt_id: AttemptId::new(),
            task_id: self.task_id,
            provider: ProviderId::Codex,
            worker_mode: "workspace_write".to_owned(),
            started_at: now,
        })?;
        workspace.acquire_coordinator_lease(&CoordinatorLeaseRequest {
            task_id: self.task_id,
            worktree_id: None,
            owner_id: daemon_uuid(daemon_instance),
            acquired_at: now,
            ttl: TimeDelta::minutes(5),
        })?;
        Ok(())
    }

    fn worker_attempts(&self) -> Result<u64> {
        let database = Database::open(self.global_database())?;
        let workspace_id = database
            .resolve_repository_workspace(&self.repository)?
            .workspace_id;
        let attempts = database
            .workspace(workspace_id)
            .list_task_attempts(self.task_id)?;
        Ok(u64::try_from(attempts.len()).unwrap_or(u64::MAX))
    }

    fn requested_controls(&self) -> Result<u64> {
        let connection = rusqlite::Connection::open(self.global_database())?;
        connection
            .query_row(
                "SELECT count(*) FROM task_controls WHERE task_id = ?1",
                [self.task_id.to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    fn transition_active_task_to_verifying(&self) -> Result<()> {
        let database = Database::open(self.global_database())?;
        let workspace_id = self
            .workspace_id
            .context("active resume fixture omitted its workspace")?;
        let workspace = database.workspace(workspace_id);
        let task = workspace
            .load_task(self.task_id)?
            .context("active resume task disappeared")?;
        let now = Utc::now();
        workspace.transition_task_with_event(
            self.task_id,
            task.revision,
            TaskState::Running,
            TaskState::Verifying,
            None,
            false,
            &TransitionGuards::default(),
            now,
            TaskEvent {
                schema_version: SchemaVersion::state_current(),
                sequence: 0,
                event_id: EventId::new(),
                session_id: None,
                task_id: Some(self.task_id),
                occurred_at: now,
                event_type: EventType::StateTransitioned,
                from_state: Some(TaskState::Running),
                to_state: Some(TaskState::Verifying),
                reason: None,
                actor: EventActor::Orchestrator,
                correlation_id: CorrelationId::new(),
                causation_id: None,
                payload: serde_json::json!({}),
                previous_hash: None,
                event_hash: String::new(),
            },
        )?;
        Ok(())
    }

    fn global_database(&self) -> PathBuf {
        self.colay_home.join("state/state.db")
    }
}

impl Drop for ResumeFixture {
    fn drop(&mut self) {
        let _ = self.colay(["daemon", "stop"]);
    }
}

fn daemon_uuid(instance_id: DaemonInstanceId) -> uuid::Uuid {
    instance_id.into_uuid()
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
fn resume_attaches_when_current_daemon_owns_active_task() -> Result<()> {
    let fixture = ResumeFixture::active_task()?;
    let task_id = fixture.task_id.to_string();
    let output = fixture.colay(["resume", task_id.as_str()])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("attached"));
    assert!(stdout.contains("active_status"));
    assert!(stdout.contains("attempt_count"));
    assert_eq!(fixture.worker_attempts()?, 1);
    Ok(())
}

#[test]
fn pause_mutation_uses_global_daemon_without_opening_repository_sqlite() -> Result<()> {
    let fixture = ResumeFixture::active_task()?;
    let task_id = fixture.task_id.to_string();
    let output = fixture.colay(["pause", task_id.as_str()])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.requested_controls()?, 1);
    assert!(!fixture.repository.join(".colay/orchestrator.db").exists());
    Ok(())
}

#[test]
fn resume_reports_the_latest_published_revision() -> Result<()> {
    let fixture = ResumeFixture::active_task()?;
    let task_id = fixture.task_id.to_string();
    fixture.transition_active_task_to_verifying()?;
    let output = fixture.colay(["--json", "resume", task_id.as_str()])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response = serde_json::from_slice::<serde_json::Value>(&output.stdout)?;
    assert_eq!(
        response["data"]["active_status"]["status"]["revision"], 1,
        "{response:#}"
    );
    assert_eq!(
        response["data"]["active_status"]["status"]["state"],
        "verifying"
    );
    assert_eq!(fixture.worker_attempts()?, 1);
    Ok(())
}
