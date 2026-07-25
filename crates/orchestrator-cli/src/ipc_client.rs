use std::{
    path::Path,
    pin::Pin,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use orchestrator_daemon::{IPC_SCHEMA_VERSION, IpcRequest, IpcResponse, ipc_endpoint};
use orchestrator_state::{GlobalStatePaths, StateEnvironment, WorkspaceId};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

type ResponseReader = Pin<Box<dyn AsyncBufRead + Send>>;

#[derive(Clone, Debug)]
pub struct DaemonClient {
    paths: GlobalStatePaths,
    workspace_id: WorkspaceId,
}

pub struct IpcResponseStream {
    lines: Lines<ResponseReader>,
}

impl DaemonClient {
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub async fn connect_or_start(
        repository: &Path,
        explicit_config: Option<&Path>,
    ) -> Result<Self> {
        let paths = GlobalStatePaths::resolve(&StateEnvironment::from_process())?;
        if ping(&paths).await.is_err() {
            wait_until_ready(&paths, repository, explicit_config).await?;
        }
        let workspace_id = register_workspace(&paths, repository, explicit_config).await?;
        Ok(Self {
            paths,
            workspace_id,
        })
    }

    pub async fn connect(repository: &Path) -> Result<Self> {
        Self::connect_with_config(repository, None).await
    }

    pub async fn connect_with_config(
        repository: &Path,
        explicit_config: Option<&Path>,
    ) -> Result<Self> {
        let paths = GlobalStatePaths::resolve(&StateEnvironment::from_process())?;
        ping(&paths).await?;
        let workspace_id = register_workspace(&paths, repository, explicit_config).await?;
        Ok(Self {
            paths,
            workspace_id,
        })
    }

    pub async fn request(&self, action: &str, payload: Value) -> Result<IpcResponse> {
        let mut stream = self.stream(action, payload).await?;
        let response = tokio::time::timeout(RESPONSE_TIMEOUT, stream.next())
            .await
            .context("timed out waiting for the user daemon")??
            .ok_or_else(|| anyhow!("user daemon closed IPC without replying"))?;
        ensure_success(&response)?;
        Ok(response)
    }

    pub async fn stream(&self, action: &str, payload: Value) -> Result<IpcResponseStream> {
        open_response_stream(
            &self.paths,
            &IpcRequest {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: Uuid::now_v7().to_string(),
                workspace_id: Some(self.workspace_id),
                action: action.to_owned(),
                payload,
            },
        )
        .await
    }
}

impl IpcResponseStream {
    pub async fn next(&mut self) -> Result<Option<IpcResponse>> {
        let Some(line) = self.lines.next_line().await? else {
            return Ok(None);
        };
        let response = serde_json::from_str::<IpcResponse>(&line)?;
        if response.schema_version != IPC_SCHEMA_VERSION {
            bail!(
                "user daemon returned IPC schema {}; supported schema is {IPC_SCHEMA_VERSION}",
                response.schema_version
            );
        }
        Ok(Some(response))
    }
}

async fn ping(paths: &GlobalStatePaths) -> Result<()> {
    let request = IpcRequest {
        schema_version: IPC_SCHEMA_VERSION,
        request_id: Uuid::now_v7().to_string(),
        workspace_id: None,
        action: "daemon.ping".to_owned(),
        payload: json!({}),
    };
    let mut stream = open_response_stream(paths, &request).await?;
    let response = tokio::time::timeout(RESPONSE_TIMEOUT, stream.next())
        .await
        .context("timed out waiting for the user daemon readiness response")??
        .ok_or_else(|| anyhow!("user daemon closed readiness IPC without replying"))?;
    ensure_success(&response)
}

async fn register_workspace(
    paths: &GlobalStatePaths,
    repository: &Path,
    explicit_config: Option<&Path>,
) -> Result<WorkspaceId> {
    let config = crate::daemon::load_daemon_config(repository, explicit_config)?;
    let request = IpcRequest {
        schema_version: IPC_SCHEMA_VERSION,
        request_id: Uuid::now_v7().to_string(),
        workspace_id: None,
        action: "workspace.register".to_owned(),
        payload: json!({
            "repository": repository,
            "state_dir": config.orchestrator.state_dir,
        }),
    };
    let mut stream = open_response_stream(paths, &request).await?;
    let response = tokio::time::timeout(RESPONSE_TIMEOUT, stream.next())
        .await
        .context("timed out registering the workspace with the user daemon")??
        .ok_or_else(|| anyhow!("user daemon closed workspace IPC without replying"))?;
    ensure_success(&response)?;
    let workspace_id = response
        .outcome
        .pointer("/data/workspace_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("user daemon omitted the registered workspace identifier"))?;
    workspace_id
        .parse()
        .context("user daemon returned an invalid workspace identifier")
}

fn ensure_success(response: &IpcResponse) -> Result<()> {
    if response.outcome.get("status").and_then(Value::as_str) == Some("ok") {
        return Ok(());
    }
    let message = response
        .outcome
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("user daemon rejected the request");
    bail!("{message}")
}

async fn wait_until_ready(
    paths: &GlobalStatePaths,
    repository: &Path,
    explicit_config: Option<&Path>,
) -> Result<()> {
    let started = Instant::now();
    let mut child: Option<Child> = None;
    let mut spawn_attempted = false;
    let mut last_child_exit = None;
    loop {
        if ping(paths).await.is_ok() {
            return Ok(());
        }
        if let Some(process) = child.as_mut()
            && let Some(exit) = process.try_wait().context("cannot inspect daemon child")?
        {
            last_child_exit = Some(exit);
            child = None;
        }
        spawn_contender_once(&mut child, &mut spawn_attempted, || {
            spawn_server(repository, explicit_config)
        })?;
        if started.elapsed() >= CONNECT_TIMEOUT {
            if let Some(process) = child.as_mut() {
                let _ = process.kill();
            }
            if let Some(exit) = last_child_exit {
                bail!("user daemon contenders exited before IPC readiness; last exit: {exit}");
            }
            bail!("user daemon did not publish IPC within ten seconds");
        }
        tokio::time::sleep(CONNECT_POLL_INTERVAL).await;
    }
}

fn spawn_contender_once<T>(
    child: &mut Option<T>,
    spawn_attempted: &mut bool,
    spawn: impl FnOnce() -> Result<T>,
) -> Result<()> {
    if child.is_none() && !*spawn_attempted {
        *spawn_attempted = true;
        *child = Some(spawn()?);
    }
    Ok(())
}

fn spawn_server(repository: &Path, explicit_config: Option<&Path>) -> Result<Child> {
    let executable = std::env::current_exe().context("cannot resolve current colay executable")?;
    let mut command = Command::new(executable);
    if let Some(config) = explicit_config {
        command.arg("--config").arg(config);
    }
    command
        .arg("daemon")
        .arg("serve")
        .current_dir(repository)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    configure_daemon_stderr(&mut command)?;
    configure_background_process(&mut command);
    command.spawn().context("cannot spawn user daemon")
}

fn configure_daemon_stderr(command: &mut Command) -> Result<()> {
    #[cfg(feature = "test-fixtures")]
    if let Some(path) = std::env::var_os("COLAY_TEST_DAEMON_STDERR") {
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "cannot open test daemon stderr log {}",
                    Path::new(&path).display()
                )
            })?;
        command.stderr(stderr);
        return Ok(());
    }
    command.stderr(Stdio::null());
    Ok(())
}

#[cfg(windows)]
fn configure_background_process(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn configure_background_process(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    command.process_group(0);
}

#[cfg(not(any(unix, windows)))]
fn configure_background_process(command: &mut Command) {
    let _ = command;
}

async fn open_response_stream(
    paths: &GlobalStatePaths,
    request: &IpcRequest,
) -> Result<IpcResponseStream> {
    let mut encoded = serde_json::to_vec(request)?;
    encoded.push(b'\n');
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(ipc_endpoint(paths)).await?;
        response_stream(stream, &encoded).await
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;

        let endpoint = ipc_endpoint(paths);
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        let stream = open_windows_pipe_with_retry(
            deadline,
            || ClientOptions::new().open(&endpoint),
            tokio::time::sleep,
        )
        .await?;
        response_stream(stream, &encoded).await
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (paths, encoded);
        bail!("local IPC is unsupported on this platform")
    }
}

#[cfg(windows)]
async fn open_windows_pipe_with_retry<S, Open, Sleep, SleepFuture>(
    deadline: Instant,
    mut open: Open,
    mut sleep: Sleep,
) -> std::io::Result<S>
where
    Open: FnMut() -> std::io::Result<S>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: std::future::Future<Output = ()>,
{
    loop {
        match open() {
            Ok(stream) => return Ok(stream),
            Err(error) if error.raw_os_error() == Some(231) && Instant::now() < deadline => {
                sleep(CONNECT_POLL_INTERVAL).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn response_stream<S>(mut stream: S, encoded: &[u8]) -> Result<IpcResponseStream>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    stream.write_all(encoded).await?;
    let (reader, _) = tokio::io::split(stream);
    let reader: ResponseReader = Box::pin(BufReader::new(reader));
    Ok(IpcResponseStream {
        lines: reader.lines(),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    #[cfg(windows)]
    use std::{future, io, time::Instant};

    use super::spawn_contender_once;
    #[cfg(windows)]
    use super::{RESPONSE_TIMEOUT, open_windows_pipe_with_retry};

    #[test]
    fn startup_contender_is_spawned_at_most_once() -> anyhow::Result<()> {
        let mut child = None;
        let mut spawn_attempted = false;
        let spawn_count = Cell::new(0_u32);

        spawn_contender_once(&mut child, &mut spawn_attempted, || {
            spawn_count.set(spawn_count.get() + 1);
            Ok(())
        })?;
        child = None;
        spawn_contender_once(&mut child, &mut spawn_attempted, || {
            spawn_count.set(spawn_count.get() + 1);
            Ok(())
        })?;

        assert_eq!(spawn_count.get(), 1);
        assert!(child.is_none());
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_pipe_busy_retries_until_success() -> io::Result<()> {
        let open_count = Cell::new(0_u32);
        let sleep_count = Cell::new(0_u32);

        open_windows_pipe_with_retry(
            Instant::now() + RESPONSE_TIMEOUT,
            || {
                let attempt = open_count.get() + 1;
                open_count.set(attempt);
                if attempt < 3 {
                    Err(io::Error::from_raw_os_error(231))
                } else {
                    Ok(())
                }
            },
            |_| {
                sleep_count.set(sleep_count.get() + 1);
                future::ready(())
            },
        )
        .await?;

        assert_eq!(open_count.get(), 3);
        assert_eq!(sleep_count.get(), 2);
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_pipe_non_busy_error_fails_without_retry() -> io::Result<()> {
        let open_count = Cell::new(0_u32);
        let sleep_count = Cell::new(0_u32);

        let result = open_windows_pipe_with_retry::<(), _, _, _>(
            Instant::now() + RESPONSE_TIMEOUT,
            || {
                open_count.set(open_count.get() + 1);
                Err(io::Error::from_raw_os_error(2))
            },
            |_| {
                sleep_count.set(sleep_count.get() + 1);
                future::ready(())
            },
        )
        .await;
        let Err(error) = result else {
            return Err(io::Error::other(
                "non-busy pipe open unexpectedly succeeded",
            ));
        };

        assert_eq!(error.raw_os_error(), Some(2));
        assert_eq!(open_count.get(), 1);
        assert_eq!(sleep_count.get(), 0);
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_pipe_busy_error_stops_at_the_deadline() -> io::Result<()> {
        let open_count = Cell::new(0_u32);
        let sleep_count = Cell::new(0_u32);

        let result = open_windows_pipe_with_retry::<(), _, _, _>(
            Instant::now(),
            || {
                open_count.set(open_count.get() + 1);
                Err(io::Error::from_raw_os_error(231))
            },
            |_| {
                sleep_count.set(sleep_count.get() + 1);
                future::ready(())
            },
        )
        .await;
        let Err(error) = result else {
            return Err(io::Error::other(
                "expired busy pipe open unexpectedly succeeded",
            ));
        };

        assert_eq!(error.raw_os_error(), Some(231));
        assert_eq!(open_count.get(), 1);
        assert_eq!(sleep_count.get(), 0);
        Ok(())
    }
}
