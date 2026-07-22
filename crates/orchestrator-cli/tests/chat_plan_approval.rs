#![cfg(feature = "test-fixtures")]

use std::{
    env, fs,
    path::PathBuf,
    process::{Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use chrono::Utc;
use orchestrator_domain::{
    AppendMessageCommandPayload, ApproveGraphCommandPayload, ClientCommand, ClientCommandAction,
    ClientCommandId, ClientCommandState, CreateSessionCommandPayload, GraphValidationSummary,
    MessageId, SessionId, TaskState,
};
use orchestrator_state::{Database, GraphRevisionStatus, RootConfig, TaskListFilter};

fn git(repository: &std::path::Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(args)
        .output()?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    repository: PathBuf,
    colay_home: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let repository = root.join("repository");
        let colay_home = root.join("home/.colay");
        fs::create_dir_all(&repository)?;
        fs::write(repository.join(".gitignore"), ".colay/\n")?;
        git(&repository, &["init"])?;
        git(&repository, &["config", "user.name", "Chat Plan E2E"])?;
        git(
            &repository,
            &["config", "user.email", "chat-plan-e2e@example.invalid"],
        )?;
        git(&repository, &["add", "."])?;
        git(&repository, &["commit", "-m", "fixture base"])?;
        Ok(Self {
            _temp: temp,
            root,
            repository,
            colay_home,
        })
    }

    fn command(&self) -> Result<Command> {
        #[cfg(windows)]
        let system_root = env::var_os("SystemRoot").context("SystemRoot must be set")?;
        #[cfg(not(windows))]
        let system_root = "/";
        let executable = PathBuf::from(env!("CARGO_BIN_EXE_colay"));
        let executable_parent = executable
            .parent()
            .context("colay binary parent")?
            .to_path_buf();
        let inherited_path = env::var_os("PATH").unwrap_or_default();
        let command_path = env::join_paths(
            std::iter::once(executable_parent).chain(env::split_paths(&inherited_path)),
        )?;
        let mut command = Command::new(executable);
        command
            .current_dir(&self.repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("PATH", command_path)
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root)
            .env("TEMP", &self.root)
            .env("TMP", &self.root);
        Ok(command)
    }

    fn output(&self, args: &[&str]) -> Result<Output> {
        self.command()?.args(args).output().map_err(Into::into)
    }

    fn status_without_capture(&self, args: &[&str]) -> Result<ExitStatus> {
        self.command()?
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(Into::into)
    }

    fn initialize_with_fake_planner(&self) -> Result<()> {
        let initialized = self.output(&["init"])?;
        if !initialized.status.success() {
            bail!(
                "init failed: {}",
                String::from_utf8_lossy(&initialized.stderr)
            );
        }
        let mut config = RootConfig::default();
        config.features.codex_app_server_adapter = false;
        config.orchestrator.max_parallel_workers = 2;
        config.orchestrator.default_timeout_minutes = 1;
        config.orchestrator.providers.gemini = None;
        config.orchestrator.providers.claude = None;
        let codex = config
            .orchestrator
            .providers
            .codex
            .as_mut()
            .context("default codex provider")?;
        env!("CARGO_BIN_EXE_colay-e2e-fake-provider").clone_into(&mut codex.executable);
        let config_path = self.repository.join(".colay/config.toml");
        fs::write(config_path, toml_edit::ser::to_string(&config)?)?;
        Ok(())
    }

    fn database(&self) -> Result<Database> {
        Database::open(self.repository.join(".colay/orchestrator.db")).map_err(Into::into)
    }

    fn wait_online(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let output = self.output(&["--json", "daemon", "status"])?;
            if output.status.success()
                && serde_json::from_slice::<serde_json::Value>(&output.stdout)?["data"]["status"]["state"]
                    == "online"
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("daemon did not become online");
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_stopped(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let output = self.output(&["--json", "daemon", "status"])?;
            if output.status.success()
                && serde_json::from_slice::<serde_json::Value>(&output.stdout)?["data"]["status"]["state"]
                    == "stopped"
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("daemon did not stop");
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.output(&["daemon", "stop"]);
    }
}

fn pending_command(
    action: ClientCommandAction,
    session_id: Option<SessionId>,
    payload: serde_json::Value,
    key: impl Into<String>,
) -> ClientCommand {
    ClientCommand {
        command_id: ClientCommandId::new(),
        session_id,
        task_id: None,
        action,
        payload,
        idempotency_key: key.into(),
        state: ClientCommandState::Pending,
        requested_by: "chat-plan-e2e".to_owned(),
        requested_at: Utc::now(),
        claimed_at: None,
        completed_at: None,
        outcome: None,
    }
}

fn wait_command(database: &Database, command_id: ClientCommandId) -> Result<ClientCommand> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(command) = database.load_client_command(command_id)?
            && matches!(
                command.state,
                ClientCommandState::Completed | ClientCommandState::Failed
            )
        {
            return Ok(command);
        }
        if Instant::now() >= deadline {
            bail!("client command {command_id} did not finish");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_approval_candidate(database: &Database, session_id: SessionId) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if database
            .current_graph(session_id)?
            .is_some_and(|graph| graph.revision.status == GraphRevisionStatus::AwaitingApproval)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("conversation-first planning did not produce an approval candidate");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_task_completion(database: &Database) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let tasks = database.list_tasks(&TaskListFilter {
            state: None,
            include_archived: false,
            limit: 10,
        })?;
        if tasks.len() == 2 && tasks.iter().all(|task| task.state == TaskState::Completed) {
            return Ok(());
        }
        if tasks.iter().any(|task| task.state == TaskState::Failed) {
            bail!("approved conversation graph produced a failed task: {tasks:?}");
        }
        if Instant::now() >= deadline {
            bail!("approved conversation graph did not complete through worktree execution");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn submit_and_wait(database: &Database, command: &ClientCommand) -> Result<ClientCommand> {
    database.submit_client_command(command)?;
    wait_command(database, command.command_id)
}

fn mutation_counts(database: &Database) -> Result<(i64, i64, i64, i64)> {
    database
        .with_connection(|connection| {
            Ok((
                connection.query_row("SELECT count(*) FROM tasks", [], |row| row.get(0))?,
                connection.query_row("SELECT count(*) FROM worktrees", [], |row| row.get(0))?,
                connection.query_row("SELECT count(*) FROM worker_leases", [], |row| row.get(0))?,
                connection.query_row("SELECT count(*) FROM task_dependencies", [], |row| {
                    row.get(0)
                })?,
            ))
        })
        .map_err(Into::into)
}

#[test]
#[allow(clippy::too_many_lines)]
fn conversation_to_exact_approval_executes_fake_workers_in_worktrees() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.initialize_with_fake_planner()?;
    assert!(
        fixture
            .status_without_capture(&["daemon", "start"])?
            .success()
    );
    fixture.wait_online()?;
    let database = fixture.database()?;

    let session_id = SessionId::new();
    let create = pending_command(
        ClientCommandAction::CreateSession,
        None,
        serde_json::to_value(CreateSessionCommandPayload {
            session_id,
            title: "Plan approval E2E".to_owned(),
        })?,
        "plan-e2e-session",
    );
    assert_eq!(
        submit_and_wait(&database, &create)?.state,
        ClientCommandState::Completed
    );

    let goal_message_id = MessageId::new();
    let goal = pending_command(
        ClientCommandAction::AppendMessage,
        Some(session_id),
        serde_json::to_value(AppendMessageCommandPayload {
            message_id: goal_message_id,
            content: "candidate: implement a local task graph".to_owned(),
        })?,
        "plan-e2e-goal",
    );
    assert_eq!(
        submit_and_wait(&database, &goal)?.state,
        ClientCommandState::Completed
    );

    wait_for_approval_candidate(&database, session_id)?;
    let graph = database
        .current_graph(session_id)?
        .context("current graph after successful planning")?;
    let proposal_hash = graph
        .revision
        .proposal_hash
        .clone()
        .context("approvable graph hash")?;
    let authority =
        serde_json::from_value::<GraphValidationSummary>(graph.revision.validation.clone())?
            .authority
            .context("validated graph authority")?;
    assert_eq!(mutation_counts(&database)?, (0, 0, 0, 0));
    assert!(!fixture.repository.join(".colay/worktrees").exists());

    let wrong = pending_command(
        ClientCommandAction::ApproveGraph,
        Some(session_id),
        serde_json::to_value(ApproveGraphCommandPayload {
            revision_id: graph.revision.revision_id,
            requirement_revision_id: authority.requirement_revision_id,
            validation_hash: authority.validation_hash.clone(),
            base_commit: authority.base_commit.clone(),
            proposal_hash: "0".repeat(64),
            approved_by: "operator".to_owned(),
        })?,
        "plan-e2e-wrong-approval",
    );
    let wrong = submit_and_wait(&database, &wrong)?;
    assert_eq!(wrong.state, ClientCommandState::Failed);
    assert_eq!(mutation_counts(&database)?, (0, 0, 0, 0));

    let exact_payload = ApproveGraphCommandPayload {
        revision_id: graph.revision.revision_id,
        requirement_revision_id: authority.requirement_revision_id,
        validation_hash: authority.validation_hash,
        base_commit: authority.base_commit,
        proposal_hash: proposal_hash.clone(),
        approved_by: "operator".to_owned(),
    };
    let exact = pending_command(
        ClientCommandAction::ApproveGraph,
        Some(session_id),
        serde_json::to_value(&exact_payload)?,
        "plan-e2e-exact-approval",
    );
    assert_eq!(
        submit_and_wait(&database, &exact)?.state,
        ClientCommandState::Completed
    );
    assert_eq!(
        database
            .list_tasks(&TaskListFilter {
                state: None,
                include_archived: false,
                limit: 10,
            })?
            .len(),
        2
    );

    let replay = pending_command(
        ClientCommandAction::ApproveGraph,
        Some(session_id),
        serde_json::to_value(&exact_payload)?,
        "plan-e2e-exact-approval",
    );
    let stored = database.submit_client_command(&replay)?;
    assert_eq!(stored.command_id, exact.command_id);
    wait_for_task_completion(&database)?;
    let completed_counts = mutation_counts(&database)?;
    assert_eq!(completed_counts.0, 2);
    assert_eq!(completed_counts.1, 2);
    assert_eq!(completed_counts.3, 1);

    drop(database);
    let reopened = fixture.database()?;
    let approved = reopened
        .current_graph(session_id)?
        .context("reopened graph")?;
    assert_eq!(
        approved.revision.proposal_hash.as_deref(),
        Some(proposal_hash.as_str())
    );
    assert_eq!(approved.tasks.len(), 2);
    assert_eq!(approved.dependencies.len(), 1);
    let invocation: serde_json::Value = serde_json::from_slice(&fs::read(
        fixture
            .repository
            .join(".colay/fake-planner-invocation.json"),
    )?)?;
    assert_eq!(invocation["invocation_count"], 1);
    let args = invocation["args"].as_array().context("planner args")?;
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--sandbox" && pair[1] == "read-only")
    );

    assert!(fixture.output(&["daemon", "stop"])?.status.success());
    fixture.wait_stopped()?;
    Ok(())
}
