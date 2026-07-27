#![cfg(feature = "test-fixtures")]

use std::{
    env, fs,
    path::PathBuf,
    process::{Command, ExitStatus, Output, Stdio},
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use chrono::Utc;
use orchestrator_daemon::{IPC_SCHEMA_VERSION, IpcRequest, IpcResponse, ipc_endpoint};
use orchestrator_domain::{
    AppendMessageCommandPayload, ApproveGraphCommandPayload, ClientCommand, ClientCommandAction,
    ClientCommandId, ClientCommandState, CreateSessionCommandPayload, GraphValidationSummary,
    MessageId, SessionId, TaskState,
};
use orchestrator_state::{
    DaemonStatus, Database, GlobalStatePaths, GraphRevisionStatus, RootConfig, StateEnvironment,
    TaskListFilter, WorkspaceDatabase, WorkspaceId,
};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};

mod support;
use support::with_workspace;

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
    _serial: MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    root: PathBuf,
    startup_repository: PathBuf,
    repository: PathBuf,
    colay_home: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let serial = fixture_guard();
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let startup_repository = root.join("startup-repository");
        let repository = root.join("repository");
        let colay_home = root.join("home/.colay");
        fs::create_dir_all(&startup_repository)?;
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
            _serial: serial,
            _temp: temp,
            root,
            startup_repository,
            repository,
            colay_home,
        })
    }

    fn command(&self) -> Result<Command> {
        self.command_in(&self.repository)
    }

    fn command_in(&self, repository: &std::path::Path) -> Result<Command> {
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
            .current_dir(repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("COLAY_TEST_DAEMON_STDERR", self.root.join("daemon.stderr"))
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
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

    fn output_in(&self, repository: &std::path::Path, args: &[&str]) -> Result<Output> {
        self.command_in(repository)?
            .args(args)
            .output()
            .map_err(Into::into)
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
        Database::open(self.colay_home.join("state/state.db")).map_err(Into::into)
    }

    fn ipc_request(
        &self,
        workspace_id: WorkspaceId,
        action: &str,
        payload: serde_json::Value,
    ) -> Result<IpcResponse> {
        let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
            self.colay_home.clone(),
        )?)?;
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: uuid::Uuid::now_v7().to_string(),
            workspace_id: Some(workspace_id),
            action: action.to_owned(),
            payload,
        };
        tokio::runtime::Runtime::new()?.block_on(send_ipc_request(ipc_endpoint(&paths), request))
    }

    fn wait_online(&self) -> Result<()> {
        self.wait_online_in(&self.repository)
    }

    fn wait_online_in(&self, repository: &std::path::Path) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let output = self.output_in(repository, &["--json", "daemon", "status"])?;
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

async fn exchange_ipc<S>(mut stream: S, request: &IpcRequest) -> Result<IpcResponse>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut encoded = serde_json::to_vec(request)?;
    encoded.push(b'\n');
    stream.write_all(&encoded).await?;
    stream.flush().await?;
    let mut response = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        BufReader::new(stream).read_line(&mut response),
    )
    .await
    .context("timed out waiting for daemon IPC")??;
    let response = serde_json::from_str::<IpcResponse>(&response)?;
    if response
        .outcome
        .get("status")
        .and_then(serde_json::Value::as_str)
        != Some("ok")
    {
        bail!(
            "daemon IPC rejected {}: {}",
            request.action,
            response.outcome
        );
    }
    Ok(response)
}

#[cfg(unix)]
async fn send_ipc_request(endpoint: PathBuf, request: IpcRequest) -> Result<IpcResponse> {
    exchange_ipc(tokio::net::UnixStream::connect(endpoint).await?, &request).await
}

#[cfg(windows)]
async fn send_ipc_request(endpoint: PathBuf, request: IpcRequest) -> Result<IpcResponse> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let deadline = Instant::now() + Duration::from_secs(10);
    let client = loop {
        match ClientOptions::new().open(&endpoint) {
            Ok(client) => break client,
            Err(error) if error.raw_os_error() == Some(231) && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    exchange_ipc(client, &request).await
}

fn fixture_guard() -> MutexGuard<'static, ()> {
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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

fn wait_command(
    fixture: &Fixture,
    workspace_id: WorkspaceId,
    command_id: ClientCommandId,
) -> Result<ClientCommand> {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let response = fixture.ipc_request(
            workspace_id,
            "workspace.command.status",
            serde_json::json!({"command_id": command_id, "idempotency_key": null}),
        )?;
        let command = serde_json::from_value::<Option<ClientCommand>>(
            response.outcome["data"]["command"].clone(),
        )?;
        if let Some(command) = command.as_ref()
            && matches!(
                command.state,
                ClientCommandState::Completed | ClientCommandState::Failed
            )
        {
            return Ok(command.clone());
        }
        if Instant::now() >= deadline {
            let daemon = fixture.database()?.daemon_status(Utc::now())?;
            bail!(
                "client command {command_id} did not finish: {command:?}; daemon status: {daemon:?}"
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_approval_candidate(
    database: &WorkspaceDatabase<'_>,
    session_id: SessionId,
) -> Result<()> {
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

fn wait_for_task_completion(
    global_database: &Database,
    database: &WorkspaceDatabase<'_>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let tasks = database.list_tasks(&TaskListFilter {
            state: None,
            include_archived: false,
            limit: 10,
        })?;
        let active_schedule_claims =
            with_workspace(global_database.path(), database, |connection| {
                connection
                    .query_row(
                        "SELECT count(*) FROM task_schedule_claims WHERE released_at IS NULL",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(Into::into)
            })?;
        if tasks.len() == 2
            && tasks.iter().all(|task| task.state == TaskState::Completed)
            && active_schedule_claims == 0
        {
            return Ok(());
        }
        if tasks.iter().any(|task| task.state == TaskState::Failed) {
            let events = database.outbox_after(0, 256)?;
            bail!(
                "approved conversation graph produced a failed task: {tasks:?}; events: {events:?}"
            );
        }
        let daemon = global_database.daemon_status(Utc::now())?;
        if matches!(daemon, DaemonStatus::Stopped) {
            bail!("daemon stopped before approved tasks completed: {tasks:?}");
        }
        if Instant::now() >= deadline {
            bail!(
                "approved conversation graph did not complete through worktree execution: {tasks:?}; active schedule claims: {active_schedule_claims}; daemon: {daemon:?}"
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn submit_and_wait(
    fixture: &Fixture,
    workspace_id: WorkspaceId,
    command: &ClientCommand,
) -> Result<ClientCommand> {
    fixture.ipc_request(
        workspace_id,
        "workspace.command.submit",
        serde_json::to_value(command)?,
    )?;
    wait_command(fixture, workspace_id, command.command_id)
}

fn mutation_counts(
    database_path: &std::path::Path,
    database: &WorkspaceDatabase<'_>,
) -> Result<(i64, i64, i64, i64)> {
    with_workspace(database_path, database, |connection| {
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
    let database = fixture.database()?;
    database.migrate_with_backup(&fixture.colay_home.join("state/backups"))?;
    let database_path = database.path().to_path_buf();
    let workspace_id = database
        .resolve_repository_workspace(&fixture.repository)?
        .workspace_id;
    assert!(
        fixture
            .status_without_capture(&["daemon", "start"])?
            .success()
    );
    fixture.wait_online()?;
    let workspace = database.workspace(workspace_id);

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
        submit_and_wait(&fixture, workspace_id, &create)?.state,
        ClientCommandState::Completed
    );

    let goal_message_id = MessageId::new();
    let goal = pending_command(
        ClientCommandAction::AppendMessage,
        Some(session_id),
        serde_json::to_value(AppendMessageCommandPayload {
            message_id: goal_message_id,
            content: "candidate: implement a local task graph".to_owned(),
            requested_provider: None,
        })?,
        "plan-e2e-goal",
    );
    assert_eq!(
        submit_and_wait(&fixture, workspace_id, &goal)?.state,
        ClientCommandState::Completed
    );

    wait_for_approval_candidate(&workspace, session_id)?;
    let graph = workspace
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
    assert_eq!(mutation_counts(&database_path, &workspace)?, (0, 0, 0, 0));
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
    let wrong = submit_and_wait(&fixture, workspace_id, &wrong)?;
    assert_eq!(wrong.state, ClientCommandState::Failed);
    assert_eq!(mutation_counts(&database_path, &workspace)?, (0, 0, 0, 0));

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
        submit_and_wait(&fixture, workspace_id, &exact)?.state,
        ClientCommandState::Completed
    );
    assert_eq!(
        workspace
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
    let stored = workspace.submit_client_command(&replay)?;
    assert_eq!(stored.command_id, exact.command_id);
    wait_for_task_completion(&database, &workspace)?;
    let completed_counts = mutation_counts(&database_path, &workspace)?;
    assert_eq!(completed_counts.0, 2);
    assert_eq!(completed_counts.1, 2);
    assert_eq!(completed_counts.3, 1);
    let global_worktree_root = fs::canonicalize(
        fixture
            .colay_home
            .join("data/workspaces")
            .join(workspace_id.to_string())
            .join("worktrees"),
    )?;
    let worktree_paths = with_workspace(&database_path, &workspace, |connection| {
        let mut statement = connection.prepare("SELECT worktree_path FROM worktrees")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    })?;
    assert_eq!(worktree_paths.len(), 2);
    let worktree_paths = worktree_paths
        .iter()
        .map(fs::canonicalize)
        .collect::<Result<Vec<_>, _>>()?;
    assert!(
        worktree_paths
            .iter()
            .all(|path| path.starts_with(&global_worktree_root))
    );

    drop(database);
    let reopened = fixture.database()?;
    let reopened_id = reopened
        .resolve_repository_workspace(&fixture.repository)?
        .workspace_id;
    let reopened = reopened.workspace(reopened_id);
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

#[test]
fn daemon_started_elsewhere_activates_plan_and_execution_for_registered_workspace() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.initialize_with_fake_planner()?;
    let database = fixture.database()?;
    database.migrate_with_backup(&fixture.colay_home.join("state/backups"))?;
    let database_path = database.path().to_path_buf();
    let started = fixture
        .command_in(&fixture.startup_repository)?
        .args(["daemon", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    assert!(started.success());
    fixture.wait_online_in(&fixture.startup_repository)?;

    let registered = fixture.output(&["status"])?;
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    let registration = database
        .find_repository_workspace(&fixture.repository)?
        .context("run did not register the execution workspace")?;
    let workspace = database.workspace(registration.workspace_id);
    let session_id = SessionId::new();
    let create = pending_command(
        ClientCommandAction::CreateSession,
        None,
        serde_json::to_value(CreateSessionCommandPayload {
            session_id,
            title: "Cross-workspace plan approval".to_owned(),
        })?,
        "cross-workspace-session",
    );
    assert_eq!(
        submit_and_wait(&fixture, registration.workspace_id, &create)?.state,
        ClientCommandState::Completed
    );
    let goal = pending_command(
        ClientCommandAction::AppendMessage,
        Some(session_id),
        serde_json::to_value(AppendMessageCommandPayload {
            message_id: MessageId::new(),
            content: "candidate: implement a local task graph".to_owned(),
            requested_provider: None,
        })?,
        "cross-workspace-goal",
    );
    assert_eq!(
        submit_and_wait(&fixture, registration.workspace_id, &goal)?.state,
        ClientCommandState::Completed
    );
    wait_for_approval_candidate(&workspace, session_id)?;
    let graph = workspace
        .current_graph(session_id)?
        .context("workspace runtime did not create an approvable graph")?;
    let authority =
        serde_json::from_value::<GraphValidationSummary>(graph.revision.validation.clone())?
            .authority
            .context("approvable graph omitted validation authority")?;
    let approval = pending_command(
        ClientCommandAction::ApproveGraph,
        Some(session_id),
        serde_json::to_value(ApproveGraphCommandPayload {
            revision_id: graph.revision.revision_id,
            requirement_revision_id: authority.requirement_revision_id,
            validation_hash: authority.validation_hash,
            base_commit: authority.base_commit,
            proposal_hash: graph
                .revision
                .proposal_hash
                .context("approvable graph omitted proposal hash")?,
            approved_by: "operator".to_owned(),
        })?,
        "cross-workspace-exact-approval",
    );
    let approved =
        submit_and_wait(&fixture, registration.workspace_id, &approval).map_err(|error| {
            let daemon_stderr = fs::read_to_string(fixture.root.join("daemon.stderr"))
                .unwrap_or_else(|read_error| format!("<cannot read daemon stderr: {read_error}>"));
            anyhow::anyhow!("{error}; daemon stderr: {daemon_stderr}")
        })?;
    assert_eq!(approved.state, ClientCommandState::Completed);
    if let Err(error) = wait_for_task_completion(&database, &workspace) {
        let daemon_stderr = fs::read_to_string(fixture.root.join("daemon.stderr"))
            .unwrap_or_else(|read_error| format!("<cannot read daemon stderr: {read_error}>"));
        bail!("{error}; daemon stderr: {daemon_stderr}");
    }
    assert_eq!(mutation_counts(&database_path, &workspace)?.1, 2);
    let stopped = fixture.output(&["daemon", "stop"])?;
    assert!(
        stopped.status.success(),
        "daemon stop failed: stdout={} stderr={} daemon_stderr={}",
        String::from_utf8_lossy(&stopped.stdout),
        String::from_utf8_lossy(&stopped.stderr),
        fs::read_to_string(fixture.root.join("daemon.stderr")).unwrap_or_default()
    );
    fixture.wait_stopped()?;
    Ok(())
}
