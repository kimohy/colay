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
    AppendMessageCommandPayload, ClientCommand, ClientCommandAction, ClientCommandId,
    ClientCommandState, CreateSessionCommandPayload, MessageId, SessionId,
};
use orchestrator_state::Database;

const README: &str = include_str!("../../../README.md");

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
        let mut command = Command::new(executable);
        command
            .current_dir(&self.repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("PATH", executable_parent)
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.output(&["daemon", "stop"]);
    }
}

fn command(
    action: ClientCommandAction,
    session_id: Option<SessionId>,
    payload: serde_json::Value,
    key: String,
) -> ClientCommand {
    ClientCommand {
        command_id: ClientCommandId::new(),
        session_id,
        task_id: None,
        action,
        payload,
        idempotency_key: key,
        state: ClientCommandState::Pending,
        requested_by: "reconnect-test".to_owned(),
        requested_at: Utc::now(),
        claimed_at: None,
        completed_at: None,
        outcome: None,
    }
}

fn wait_for_projection(
    description: &str,
    mut projected: impl FnMut() -> Result<bool>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if projected()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("daemon did not {description} within five seconds");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn chat_tui_help_and_durable_reconnect_keep_daemon_alive() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.output(&["tui", "--help"])?;
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout)?;
    for expected in ["durable chat workspace", "Ctrl+T", "/tasks", "/admin"] {
        assert!(help.contains(expected), "missing `{expected}` in {help}");
    }

    let initialized = fixture.output(&["init"])?;
    assert!(initialized.status.success());
    assert!(
        fixture
            .status_without_capture(&["daemon", "start"])?
            .success()
    );
    fixture.wait_online()?;

    let database = fixture.database()?;
    let session_id = SessionId::new();
    database.submit_client_command(&command(
        ClientCommandAction::CreateSession,
        None,
        serde_json::to_value(CreateSessionCommandPayload {
            session_id,
            title: "Reconnect chat".to_owned(),
        })?,
        "reconnect-session".to_owned(),
    ))?;
    wait_for_projection("create session", || {
        Ok(database.load_session(session_id)?.is_some())
    })?;

    let message_id = MessageId::new();
    database.submit_client_command(&command(
        ClientCommandAction::AppendMessage,
        Some(session_id),
        serde_json::to_value(AppendMessageCommandPayload {
            message_id,
            content: "authorization=super-secret-token".to_owned(),
        })?,
        format!("reconnect-message-{message_id}"),
    ))?;
    wait_for_projection("persist message", || {
        Ok(database.load_message(message_id)?.is_some())
    })?;
    drop(database);

    let reopened = fixture.database()?;
    let sessions = reopened.list_sessions(&orchestrator_state::SessionListFilter {
        include_archived: false,
        limit: 1,
    })?;
    assert_eq!(sessions[0].session_id, session_id);
    let messages = reopened.messages_after(session_id, 0, 10)?;
    let stored_user_message = messages
        .iter()
        .find(|(_, message)| message.message_id == message_id)
        .map(|(_, message)| message)
        .context("reconnected timeline lost the durable user message")?;
    assert!(stored_user_message.content_redacted.contains("[REDACTED]"));
    assert!(
        !stored_user_message
            .content_redacted
            .contains("super-secret-token")
    );
    fixture.wait_online()?;

    assert!(fixture.output(&["daemon", "stop"])?.status.success());
    Ok(())
}

#[test]
fn chat_tui_readme_documents_navigation_reconnect_and_phase_boundary() {
    for expected in [
        "colay tui",
        "Ctrl+T",
        "/tasks",
        "daemon reconnect",
        "Phase 3",
        "/plan",
        "/integrate",
        "/approve",
        "/resolve",
        "proposal hash",
        "read-only sandbox",
        "No writable task before approval",
        "parallel execution",
    ] {
        assert!(README.contains(expected), "README is missing `{expected}`");
    }
}
