use std::{
    path::{Path, PathBuf},
    pin::Pin,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
#[cfg(windows)]
use chrono::Utc;
use orchestrator_daemon::{
    IPC_SCHEMA_VERSION, IpcEndpointCandidates, IpcRequest, IpcResponse, ipc_endpoint_candidates,
};
use orchestrator_domain::DaemonInstanceId;
#[cfg(windows)]
use orchestrator_state::read_online_daemon_identity;
use orchestrator_state::{GlobalStatePaths, StateEnvironment, WorkspaceId};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

type ResponseReader = Pin<Box<dyn AsyncBufRead + Send>>;

#[derive(Clone, Debug)]
pub struct DaemonClient {
    paths: GlobalStatePaths,
    endpoint: DaemonEndpoint,
    workspace_id: WorkspaceId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LegacyDaemonIdentity {
    instance_id: DaemonInstanceId,
    owner_pid: u32,
}

#[derive(Clone, Debug)]
struct DaemonEndpoint {
    path: PathBuf,
    validation: EndpointValidation,
}

#[derive(Clone, Copy, Debug)]
enum EndpointValidation {
    Primary,
    #[cfg(windows)]
    Legacy(LegacyDaemonIdentity),
}

impl DaemonEndpoint {
    fn primary(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            validation: EndpointValidation::Primary,
        }
    }

    #[cfg(windows)]
    fn legacy(path: &Path, identity: LegacyDaemonIdentity) -> Self {
        Self {
            path: path.to_path_buf(),
            validation: EndpointValidation::Legacy(identity),
        }
    }
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
        let candidates = ipc_endpoint_candidates(&paths)?;
        let endpoint = match discover_live_endpoint(&paths, &candidates).await? {
            Some((endpoint, _)) => endpoint,
            None => wait_until_ready(&paths, &candidates, repository, explicit_config).await?,
        };
        let workspace_id =
            register_workspace(&paths, &endpoint, repository, explicit_config).await?;
        Ok(Self {
            paths,
            endpoint,
            workspace_id,
        })
    }

    pub async fn doctor_lookup(repository: &Path) -> Result<IpcResponse> {
        let paths = GlobalStatePaths::resolve(&StateEnvironment::from_process())?;
        let candidates = ipc_endpoint_candidates(&paths)?;
        let (endpoint, _) = discover_live_endpoint(&paths, &candidates)
            .await?
            .ok_or_else(|| endpoint_refused("user daemon is not listening"))?;
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: Uuid::now_v7().to_string(),
            workspace_id: None,
            action: "workspace.doctor.lookup".to_owned(),
            payload: json!({"repository": repository}),
        };
        let mut stream = open_response_stream(&paths, &endpoint, &request).await?;
        let response = tokio::time::timeout(RESPONSE_TIMEOUT, stream.next())
            .await
            .context("timed out waiting for read-only workspace doctor lookup")??
            .ok_or_else(|| anyhow!("user daemon closed doctor IPC without replying"))?;
        ensure_success(&response)?;
        Ok(response)
    }

    pub async fn request_global(action: &str, payload: Value) -> Result<IpcResponse> {
        let paths = GlobalStatePaths::resolve(&StateEnvironment::from_process())?;
        let candidates = ipc_endpoint_candidates(&paths)?;
        let (endpoint, _) = discover_live_endpoint(&paths, &candidates)
            .await?
            .ok_or_else(|| endpoint_refused("user daemon is not listening"))?;
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: Uuid::now_v7().to_string(),
            workspace_id: None,
            action: action.to_owned(),
            payload,
        };
        let mut stream = open_response_stream(&paths, &endpoint, &request).await?;
        let response = tokio::time::timeout(RESPONSE_TIMEOUT, stream.next())
            .await
            .context("timed out waiting for the user daemon")??
            .ok_or_else(|| endpoint_refused("user daemon closed global IPC without replying"))?;
        ensure_success(&response)?;
        Ok(response)
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
            &self.endpoint,
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

async fn ping(paths: &GlobalStatePaths, endpoint: &DaemonEndpoint) -> Result<PingReadiness> {
    let request = IpcRequest {
        schema_version: IPC_SCHEMA_VERSION,
        request_id: Uuid::now_v7().to_string(),
        workspace_id: None,
        action: "daemon.ping".to_owned(),
        payload: json!({}),
    };
    let mut stream = open_response_stream(paths, endpoint, &request).await?;
    let response = tokio::time::timeout(RESPONSE_TIMEOUT, stream.next())
        .await
        .context("timed out waiting for the user daemon readiness response")??
        .ok_or_else(|| endpoint_refused("user daemon closed readiness IPC without replying"))?;
    ping_readiness(&response)
}

async fn discover_live_endpoint(
    paths: &GlobalStatePaths,
    candidates: &IpcEndpointCandidates,
) -> Result<Option<(DaemonEndpoint, PingReadiness)>> {
    let primary = DaemonEndpoint::primary(candidates.primary());
    match ping(paths, &primary).await {
        Ok(readiness) => return Ok(Some((primary, readiness))),
        Err(error) if endpoint_is_unavailable(&error) => {}
        Err(error) => return Err(error).context("primary daemon endpoint is not healthy"),
    }

    #[cfg(windows)]
    {
        let Some(legacy_path) = candidates.legacy() else {
            return Ok(None);
        };
        let Some(expected) = expected_legacy_daemon_identity(paths)? else {
            return Ok(None);
        };
        let legacy = DaemonEndpoint::legacy(legacy_path, expected);
        return match ping(paths, &legacy).await {
            Ok(readiness) => Ok(Some((legacy, readiness))),
            Err(error) if endpoint_is_unavailable(&error) => Ok(None),
            Err(error) => Err(error)
                .context("legacy daemon endpoint failed expected state-root identity validation"),
        };
    }
    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

fn endpoint_is_unavailable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) || matches!(error.raw_os_error(), Some(2 | 3))
        })
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PingReadiness {
    Legacy,
    Owner(u32),
}

fn ping_readiness(response: &IpcResponse) -> Result<PingReadiness> {
    ensure_success(response)?;
    if response
        .outcome
        .pointer("/data/ready")
        .and_then(Value::as_bool)
        != Some(true)
    {
        bail!("user daemon readiness response was not ready");
    }
    let Some(owner_pid) = response.outcome.pointer("/data/owner_pid") else {
        return Ok(PingReadiness::Legacy);
    };
    let owner_pid = owner_pid
        .as_u64()
        .ok_or_else(|| anyhow!("user daemon returned a non-numeric owner PID"))?;
    let owner_pid = u32::try_from(owner_pid)
        .context("user daemon returned an owner PID outside the u32 range")?;
    if owner_pid == 0 {
        bail!("user daemon returned an invalid zero owner PID");
    }
    Ok(PingReadiness::Owner(owner_pid))
}

fn legacy_status_identity(response: &IpcResponse) -> Result<LegacyDaemonIdentity> {
    ensure_success(response)?;
    if response
        .outcome
        .pointer("/data/status/state")
        .and_then(Value::as_str)
        != Some("online")
    {
        bail!("user daemon legacy status was not online");
    }
    let instance_id = response
        .outcome
        .pointer("/data/status/instance/instance_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("user daemon legacy status omitted its instance identifier"))?
        .parse()
        .context("user daemon legacy status returned an invalid instance identifier")?;
    let owner_pid = response
        .outcome
        .pointer("/data/status/instance/pid")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("user daemon legacy status omitted its authoritative owner PID"))?;
    let owner_pid = u32::try_from(owner_pid)
        .context("user daemon legacy status returned an owner PID outside the u32 range")?;
    if owner_pid == 0 {
        bail!("user daemon legacy status returned an invalid zero owner PID");
    }
    Ok(LegacyDaemonIdentity {
        instance_id,
        owner_pid,
    })
}

#[cfg(windows)]
fn expected_legacy_daemon_identity(
    paths: &GlobalStatePaths,
) -> Result<Option<LegacyDaemonIdentity>> {
    Ok(
        read_online_daemon_identity(&paths.database, Utc::now())?.map(|identity| {
            LegacyDaemonIdentity {
                instance_id: identity.instance_id,
                owner_pid: identity.pid,
            }
        }),
    )
}

#[cfg(any(windows, test))]
fn validate_legacy_daemon_identity(
    observed: LegacyDaemonIdentity,
    expected: LegacyDaemonIdentity,
    pinned: LegacyDaemonIdentity,
) -> Result<()> {
    if observed != expected {
        bail!("legacy daemon endpoint identity does not match the expected state database");
    }
    if observed != pinned {
        bail!("legacy daemon endpoint identity changed after route selection");
    }
    Ok(())
}

async fn request_legacy_status_owner_pid(
    paths: &GlobalStatePaths,
    endpoint: &DaemonEndpoint,
) -> Result<u32> {
    let request = legacy_status_request();
    let mut stream = open_response_stream(paths, endpoint, &request).await?;
    let response = tokio::time::timeout(RESPONSE_TIMEOUT, stream.next())
        .await
        .context("timed out waiting for the legacy user daemon status response")??
        .ok_or_else(|| endpoint_refused("user daemon closed status IPC without replying"))?;
    Ok(legacy_status_identity(&response)?.owner_pid)
}

fn legacy_status_request() -> IpcRequest {
    IpcRequest {
        schema_version: IPC_SCHEMA_VERSION,
        request_id: Uuid::now_v7().to_string(),
        workspace_id: None,
        action: "daemon.status".to_owned(),
        payload: json!({}),
    }
}

fn endpoint_refused(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::ConnectionRefused, message)
}

async fn register_workspace(
    paths: &GlobalStatePaths,
    endpoint: &DaemonEndpoint,
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
            "explicit_config": explicit_config,
        }),
    };
    let mut stream = open_response_stream(paths, endpoint, &request).await?;
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
    candidates: &IpcEndpointCandidates,
    repository: &Path,
    explicit_config: Option<&Path>,
) -> Result<DaemonEndpoint> {
    let started = Instant::now();
    let deadline = started + CONNECT_TIMEOUT;
    let mut child: Option<Child> = None;
    let mut spawn_attempted = false;
    let mut last_child_exit = None;
    loop {
        if let Some((endpoint, readiness)) = discover_live_endpoint(paths, candidates).await? {
            match resolve_ready_child(child.as_mut(), readiness, || {
                request_legacy_status_owner_pid(paths, &endpoint)
            })
            .await?
            {
                ReadyChildDisposition::NoSpawn => return Ok(endpoint),
                ReadyChildDisposition::LiveOwner => {
                    let owner_pid = child
                        .as_ref()
                        .map(Child::id)
                        .ok_or_else(|| anyhow!("daemon owner child was lost before recording"))?;
                    record_startup_child_resolution("owner", owner_pid)?;
                    return Ok(endpoint);
                }
                ReadyChildDisposition::ReapContender => {
                    let process = child
                        .as_mut()
                        .ok_or_else(|| anyhow!("daemon contender child was lost before reaping"))?;
                    let child_pid = process.id();
                    loop {
                        match poll_non_owner_contender(process, Instant::now() >= deadline)? {
                            ReapProgress::Pending => {
                                tokio::time::sleep(CONNECT_POLL_INTERVAL).await;
                            }
                            ReapProgress::Reaped(_) => {
                                record_startup_child_resolution("reaped", child_pid)?;
                                return Ok(endpoint);
                            }
                        }
                    }
                }
            }
        }
        if let Some(process) = child.as_mut() {
            match poll_non_owner_contender(process, false)? {
                ReapProgress::Pending => {}
                ReapProgress::Reaped(exit) => {
                    record_startup_child_resolution("reaped", process.id())?;
                    last_child_exit = Some(exit);
                    child = None;
                }
            }
        }
        spawn_contender_once(&mut child, &mut spawn_attempted, || {
            spawn_server(repository, explicit_config)
        })?;
        if Instant::now() >= deadline {
            if let Some(process) = child.as_mut() {
                match inspect_child_at_startup_deadline(process) {
                    Ok(Some(exit)) => {
                        record_startup_child_resolution("reaped", process.id())?;
                        last_child_exit = Some(exit);
                    }
                    Ok(None) => bail!("daemon child remained pending after startup deadline"),
                    Err(cleanup_error) => {
                        bail!(
                            "user daemon did not publish IPC within {} seconds: {cleanup_error}",
                            CONNECT_TIMEOUT.as_secs()
                        );
                    }
                }
            }
            if let Some(exit) = last_child_exit {
                bail!("{}", daemon_contenders_exited_message(&exit));
            }
            bail!(
                "user daemon did not publish IPC within {} seconds",
                CONNECT_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(CONNECT_POLL_INTERVAL).await;
    }
}

#[cfg(feature = "test-fixtures")]
fn record_startup_child_resolution(disposition: &str, pid: u32) -> Result<()> {
    let Some(path) = std::env::var_os("COLAY_TEST_DAEMON_CHILD_RESOLUTION") else {
        return Ok(());
    };
    std::fs::write(&path, format!("{disposition}:{pid}")).with_context(|| {
        format!(
            "cannot record daemon child resolution at {}",
            Path::new(&path).display()
        )
    })
}

#[cfg(not(feature = "test-fixtures"))]
fn record_startup_child_resolution(_disposition: &str, _pid: u32) -> Result<()> {
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadyChildDisposition {
    NoSpawn,
    LiveOwner,
    ReapContender,
}

fn classify_ready_child(child_pid: Option<u32>, owner_pid: u32) -> ReadyChildDisposition {
    match child_pid {
        None => ReadyChildDisposition::NoSpawn,
        Some(child_pid) if child_pid == owner_pid => ReadyChildDisposition::LiveOwner,
        Some(_) => ReadyChildDisposition::ReapContender,
    }
}

async fn resolve_ready_child<C, Status, StatusFuture>(
    child: Option<&mut C>,
    readiness: PingReadiness,
    legacy_owner_pid: Status,
) -> Result<ReadyChildDisposition>
where
    C: StartupChild,
    Status: FnOnce() -> StatusFuture,
    StatusFuture: std::future::Future<Output = Result<u32>>,
{
    let Some(child) = child else {
        return Ok(ReadyChildDisposition::NoSpawn);
    };
    let owner_pid = match readiness {
        PingReadiness::Owner(owner_pid) => owner_pid,
        PingReadiness::Legacy => match legacy_owner_pid().await {
            Ok(owner_pid) => owner_pid,
            Err(status_error) => {
                let child_pid = child.id();
                if let Err(cleanup_error) = child.terminate_and_reap() {
                    bail!(
                        "cannot obtain authoritative owner PID from legacy daemon status: {status_error}; cleanup of daemon child {child_pid} also failed: {cleanup_error}"
                    );
                }
                record_startup_child_resolution("reaped", child_pid)?;
                bail!(
                    "cannot obtain authoritative owner PID from legacy daemon status: {status_error}; cleaned up daemon child {child_pid}"
                );
            }
        },
    };
    Ok(classify_ready_child(Some(child.id()), owner_pid))
}

#[derive(Debug, PartialEq, Eq)]
enum ReapProgress {
    Pending,
    Reaped(String),
}

trait StartupChild {
    fn id(&self) -> u32;
    fn try_wait(&mut self) -> std::io::Result<Option<String>>;
    fn terminate_and_reap(&mut self) -> std::io::Result<()>;
}

impl StartupChild for Child {
    fn id(&self) -> u32 {
        Child::id(self)
    }

    fn try_wait(&mut self) -> std::io::Result<Option<String>> {
        Child::try_wait(self).map(|exit| exit.map(|status| status.to_string()))
    }

    fn terminate_and_reap(&mut self) -> std::io::Result<()> {
        match Child::kill(self) {
            Ok(()) => Child::wait(self).map(|_| ()),
            Err(kill_error) => match Child::try_wait(self) {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(std::io::Error::new(
                    kill_error.kind(),
                    format!(
                        "cannot terminate daemon child {} and it may still be running: {kill_error}",
                        Child::id(self)
                    ),
                )),
                Err(wait_error) => Err(std::io::Error::new(
                    wait_error.kind(),
                    format!(
                        "cannot terminate daemon child {} ({kill_error}) or inspect it afterward ({wait_error})",
                        Child::id(self)
                    ),
                )),
            },
        }
    }
}

fn daemon_contenders_exited_message(exit: &str) -> String {
    format!(
        "user daemon contenders exited before IPC readiness; last exit: {exit}; run `colay doctor` for startup diagnostics"
    )
}

fn poll_non_owner_contender(
    child: &mut impl StartupChild,
    deadline_reached: bool,
) -> Result<ReapProgress> {
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(exit)) => Ok(ReapProgress::Reaped(exit)),
        Ok(None) if !deadline_reached => Ok(ReapProgress::Pending),
        Ok(None) => {
            child
                .terminate_and_reap()
                .with_context(|| format!("cannot clean up timed-out daemon child {pid}"))?;
            bail!("daemon contender {pid} did not exit before the startup deadline")
        }
        Err(inspect_error) => {
            if let Err(cleanup_error) = child.terminate_and_reap() {
                bail!(
                    "cannot inspect daemon child {pid}: {inspect_error}; cleanup also failed: {cleanup_error}"
                );
            }
            Err(inspect_error).context("cannot inspect daemon child")
        }
    }
}

fn inspect_child_at_startup_deadline(child: &mut impl StartupChild) -> Result<Option<String>> {
    match poll_non_owner_contender(child, true)? {
        ReapProgress::Pending => Ok(None),
        ReapProgress::Reaped(exit) => Ok(Some(exit)),
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
    #[cfg(windows)] paths: &GlobalStatePaths,
    #[cfg(not(windows))] _paths: &GlobalStatePaths,
    endpoint: &DaemonEndpoint,
    request: &IpcRequest,
) -> Result<IpcResponseStream> {
    let mut encoded = serde_json::to_vec(request)?;
    encoded.push(b'\n');
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(&endpoint.path).await?;
        match endpoint.validation {
            EndpointValidation::Primary => response_stream(stream, &encoded).await,
        }
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;

        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        let stream = open_windows_pipe_with_retry(
            deadline,
            || ClientOptions::new().open(&endpoint.path),
            tokio::time::sleep,
        )
        .await?;
        match endpoint.validation {
            EndpointValidation::Primary => response_stream(stream, &encoded).await,
            EndpointValidation::Legacy(pinned) => {
                let expected = expected_legacy_daemon_identity(paths)?.ok_or_else(|| {
                    anyhow!(
                        "selected legacy daemon is no longer online in the expected state database"
                    )
                })?;
                response_stream_with_legacy_identity(stream, request, expected, pinned).await
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (_paths, endpoint, encoded);
        bail!("local IPC is unsupported on this platform")
    }
}

#[cfg(any(windows, test))]
async fn response_stream_with_legacy_identity<S>(
    stream: S,
    request: &IpcRequest,
    expected: LegacyDaemonIdentity,
    pinned: LegacyDaemonIdentity,
) -> Result<IpcResponseStream>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let status_request = legacy_status_request();
    let mut encoded_status = serde_json::to_vec(&status_request)?;
    encoded_status.push(b'\n');
    let mut encoded_request = serde_json::to_vec(request)?;
    encoded_request.push(b'\n');
    let (reader, mut writer) = tokio::io::split(stream);
    writer.write_all(&encoded_status).await?;
    let reader: ResponseReader = Box::pin(BufReader::new(reader));
    let mut responses = IpcResponseStream {
        lines: reader.lines(),
    };
    let response = tokio::time::timeout(RESPONSE_TIMEOUT, responses.next())
        .await
        .context("timed out validating the selected legacy daemon endpoint")??
        .ok_or_else(|| endpoint_refused("legacy user daemon closed status IPC without replying"))?;
    if response.request_id != status_request.request_id {
        bail!("legacy daemon status response request identifier did not match");
    }
    let observed = legacy_status_identity(&response)?;
    validate_legacy_daemon_identity(observed, expected, pinned)?;
    writer.write_all(&encoded_request).await?;
    Ok(responses)
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
    #[cfg(windows)]
    use std::time::Instant;
    use std::{cell::Cell, collections::VecDeque, future, io};

    use super::{
        LegacyDaemonIdentity, PingReadiness, ReadyChildDisposition, ReapProgress, StartupChild,
        classify_ready_child, daemon_contenders_exited_message, inspect_child_at_startup_deadline,
        legacy_status_identity, legacy_status_request, ping_readiness, poll_non_owner_contender,
        resolve_ready_child, response_stream_with_legacy_identity, spawn_contender_once,
        validate_legacy_daemon_identity,
    };
    #[cfg(windows)]
    use super::{RESPONSE_TIMEOUT, open_windows_pipe_with_retry};
    use anyhow::Context as _;
    use orchestrator_daemon::{IPC_SCHEMA_VERSION, IpcRequest, IpcResponse};
    use serde_json::json;

    #[test]
    fn contender_exit_diagnostic_points_to_doctor_without_claiming_a_cause() {
        let message = daemon_contenders_exited_message("exit status: 1");
        assert!(message.contains("exited before IPC readiness"));
        assert!(message.contains("last exit: exit status: 1"));
        assert!(message.contains("run `colay doctor`"));
        assert!(!message.contains("node_count"));
    }

    struct FakeStartupChild {
        pid: u32,
        observations: VecDeque<io::Result<Option<String>>>,
        cleanup_count: usize,
        cleanup_fails: bool,
    }

    impl FakeStartupChild {
        fn new(pid: u32, observations: Vec<io::Result<Option<String>>>) -> Self {
            Self {
                pid,
                observations: observations.into(),
                cleanup_count: 0,
                cleanup_fails: false,
            }
        }
    }

    impl StartupChild for FakeStartupChild {
        fn id(&self) -> u32 {
            self.pid
        }

        fn try_wait(&mut self) -> io::Result<Option<String>> {
            self.observations.pop_front().unwrap_or(Ok(None))
        }

        fn terminate_and_reap(&mut self) -> io::Result<()> {
            self.cleanup_count += 1;
            if self.cleanup_fails {
                Err(io::Error::other("wait failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn readiness_response_preserves_version_one_legacy_shape() -> anyhow::Result<()> {
        let response = IpcResponse {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "ping".to_owned(),
            outcome: json!({"status": "ok", "data": {"ready": true, "owner_pid": 42}}),
        };
        assert_eq!(ping_readiness(&response)?, PingReadiness::Owner(42));

        let legacy = IpcResponse {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "ping".to_owned(),
            outcome: json!({"status": "ok", "data": {"ready": true}}),
        };
        assert_eq!(ping_readiness(&legacy)?, PingReadiness::Legacy);

        for invalid in [
            json!({"status": "ok", "data": {"ready": false, "owner_pid": 42}}),
            json!({"status": "ok", "data": {"ready": true, "owner_pid": 0}}),
            json!({"status": "ok", "data": {"ready": true, "owner_pid": "42"}}),
        ] {
            let response = IpcResponse {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: "ping".to_owned(),
                outcome: invalid,
            };
            assert!(ping_readiness(&response).is_err());
        }
        Ok(())
    }

    #[test]
    fn legacy_status_requires_an_online_authoritative_instance_and_owner() -> anyhow::Result<()> {
        let instance_id = "018f68d2-00f0-7000-8000-000000000042";
        let online = IpcResponse {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "status".to_owned(),
            outcome: json!({
                "status": "ok",
                "data": {"status": {"state": "online", "instance": {
                    "instance_id": instance_id,
                    "pid": 42
                }}}
            }),
        };
        assert_eq!(
            legacy_status_identity(&online)?,
            LegacyDaemonIdentity {
                instance_id: instance_id.parse()?,
                owner_pid: 42,
            }
        );

        for invalid in [
            json!({"status": "error", "error": "status unavailable"}),
            json!({"status": "ok", "data": {}}),
            json!({"status": "ok", "data": {"status": {"state": "stopped"}}}),
            json!({"status": "ok", "data": {"status": {"state": "booting", "instance": {"instance_id": instance_id, "pid": 42}}}}),
            json!({"status": "ok", "data": {"status": {"state": "online"}}}),
            json!({"status": "ok", "data": {"status": {"state": "online", "instance": {"instance_id": instance_id, "pid": 0}}}}),
            json!({"status": "ok", "data": {"status": {"state": "online", "instance": {"instance_id": instance_id, "pid": "42"}}}}),
            json!({"status": "ok", "data": {"status": {"state": "online", "instance": {"instance_id": instance_id, "pid": u64::from(u32::MAX) + 1}}}}),
            json!({"status": "ok", "data": {"status": {"state": "online", "instance": {"instance_id": "not-a-uuid", "pid": 42}}}}),
            json!({"status": "ok", "data": {"status": {"state": "online", "instance": {"pid": 42}}}}),
        ] {
            let response = IpcResponse {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: "status".to_owned(),
                outcome: invalid,
            };
            assert!(legacy_status_identity(&response).is_err());
        }
        Ok(())
    }

    #[test]
    fn legacy_identity_must_match_expected_root_and_pinned_route() -> anyhow::Result<()> {
        let expected = LegacyDaemonIdentity {
            instance_id: "018f68d2-00f0-7000-8000-000000000042".parse()?,
            owner_pid: 42,
        };
        let wrong_instance = LegacyDaemonIdentity {
            instance_id: "018f68d2-00f0-7000-8000-000000000043".parse()?,
            owner_pid: 42,
        };
        let wrong_pid = LegacyDaemonIdentity {
            instance_id: expected.instance_id,
            owner_pid: 43,
        };

        validate_legacy_daemon_identity(expected, expected, expected)?;
        assert!(validate_legacy_daemon_identity(wrong_instance, expected, expected).is_err());
        assert!(validate_legacy_daemon_identity(wrong_pid, expected, expected).is_err());
        assert!(validate_legacy_daemon_identity(expected, wrong_instance, expected).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn legacy_request_validates_identity_on_the_same_connection_before_reply()
    -> anyhow::Result<()> {
        let identity = LegacyDaemonIdentity {
            instance_id: "018f68d2-00f0-7000-8000-000000000042".parse()?,
            owner_pid: 42,
        };
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "actual-request".to_owned(),
            workspace_id: None,
            action: "daemon.stop".to_owned(),
            payload: json!({}),
        };
        let (client, server) = tokio::io::duplex(4096);
        let peer = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

            let (reader, mut writer) = tokio::io::split(server);
            let mut lines = BufReader::new(reader).lines();
            let status_request: IpcRequest =
                serde_json::from_str(&lines.next_line().await?.context("status request")?)?;
            assert_eq!(status_request.action, "daemon.status");
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), lines.next_line())
                    .await
                    .is_err(),
                "legacy client sent its actual request before status validation"
            );
            let status = IpcResponse {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: status_request.request_id,
                outcome: json!({"status": "ok", "data": {"status": {
                    "state": "online",
                    "instance": {"instance_id": identity.instance_id, "pid": identity.owner_pid}
                }}}),
            };
            writer.write_all(&serde_json::to_vec(&status)?).await?;
            writer.write_all(b"\n").await?;
            let actual_request: IpcRequest =
                serde_json::from_str(&lines.next_line().await?.context("actual request")?)?;
            assert_eq!(actual_request.action, "daemon.stop");
            let actual = IpcResponse {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: actual_request.request_id,
                outcome: json!({"status": "ok", "data": {"requested": true}}),
            };
            writer.write_all(&serde_json::to_vec(&actual)?).await?;
            writer.write_all(b"\n").await?;
            Ok::<_, anyhow::Error>(())
        });

        let mut stream =
            response_stream_with_legacy_identity(client, &request, identity, identity).await?;
        let response = stream.next().await?.context("actual response")?;
        assert_eq!(response.request_id, "actual-request");
        peer.await??;
        Ok(())
    }

    #[tokio::test]
    async fn legacy_identity_mismatch_never_receives_the_actual_request() -> anyhow::Result<()> {
        let expected = LegacyDaemonIdentity {
            instance_id: "018f68d2-00f0-7000-8000-000000000042".parse()?,
            owner_pid: 42,
        };
        let observed = LegacyDaemonIdentity {
            instance_id: expected.instance_id,
            owner_pid: 43,
        };
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "actual-request".to_owned(),
            workspace_id: None,
            action: "daemon.stop".to_owned(),
            payload: json!({}),
        };
        let (client, server) = tokio::io::duplex(4096);
        let peer = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

            let (reader, mut writer) = tokio::io::split(server);
            let mut lines = BufReader::new(reader).lines();
            let status_request: IpcRequest =
                serde_json::from_str(&lines.next_line().await?.context("status request")?)?;
            let status = IpcResponse {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: status_request.request_id,
                outcome: json!({"status": "ok", "data": {"status": {
                    "state": "online",
                    "instance": {"instance_id": observed.instance_id, "pid": observed.owner_pid}
                }}}),
            };
            writer.write_all(&serde_json::to_vec(&status)?).await?;
            writer.write_all(b"\n").await?;
            if let Ok(Ok(Some(actual))) =
                tokio::time::timeout(std::time::Duration::from_millis(20), lines.next_line()).await
            {
                anyhow::bail!("legacy identity mismatch received actual request: {actual}");
            }
            Ok::<_, anyhow::Error>(())
        });

        assert!(
            response_stream_with_legacy_identity(client, &request, expected, expected)
                .await
                .is_err()
        );
        peer.await??;
        Ok(())
    }

    #[test]
    fn legacy_owner_lookup_uses_the_version_one_status_operation() {
        let request = legacy_status_request();

        assert_eq!(request.schema_version, 1);
        assert_eq!(request.workspace_id, None);
        assert_eq!(request.action, "daemon.status");
        assert_eq!(request.payload, json!({}));
    }

    #[test]
    fn ready_spawned_owner_remains_live() {
        assert_eq!(
            classify_ready_child(Some(42), 42),
            ReadyChildDisposition::LiveOwner
        );
    }

    #[test]
    fn ready_without_spawn_needs_no_child_action() {
        assert_eq!(
            classify_ready_child(None, 42),
            ReadyChildDisposition::NoSpawn
        );
    }

    #[tokio::test]
    async fn legacy_ready_without_spawn_skips_owner_lookup() -> anyhow::Result<()> {
        let status_calls = Cell::new(0_u32);

        let disposition =
            resolve_ready_child(None::<&mut FakeStartupChild>, PingReadiness::Legacy, || {
                status_calls.set(status_calls.get() + 1);
                future::ready(Ok(42))
            })
            .await?;

        assert_eq!(disposition, ReadyChildDisposition::NoSpawn);
        assert_eq!(status_calls.get(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn legacy_ready_reaps_child_when_status_names_old_owner() -> anyhow::Result<()> {
        let mut child = FakeStartupChild::new(41, vec![Ok(Some("exit code: 1".to_owned()))]);

        let disposition = resolve_ready_child(Some(&mut child), PingReadiness::Legacy, || {
            future::ready(Ok(42))
        })
        .await?;
        assert_eq!(disposition, ReadyChildDisposition::ReapContender);
        assert_eq!(
            poll_non_owner_contender(&mut child, false)?,
            ReapProgress::Reaped("exit code: 1".to_owned())
        );
        assert_eq!(child.cleanup_count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn legacy_ready_preserves_child_when_status_names_it_owner() -> anyhow::Result<()> {
        let mut child = FakeStartupChild::new(42, vec![]);

        let disposition = resolve_ready_child(Some(&mut child), PingReadiness::Legacy, || {
            future::ready(Ok(42))
        })
        .await?;

        assert_eq!(disposition, ReadyChildDisposition::LiveOwner);
        assert_eq!(child.cleanup_count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_legacy_status_cleans_up_tracked_child() -> anyhow::Result<()> {
        for invalid in [
            json!({"status": "error", "error": "status unavailable"}),
            json!({"status": "ok", "data": {}}),
            json!({"status": "ok", "data": {"status": {"state": "stopped"}}}),
            json!({"status": "ok", "data": {"status": {"state": "online", "instance": {"pid": "42"}}}}),
        ] {
            let mut child = FakeStartupChild::new(41, vec![]);
            let response = IpcResponse {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: "status".to_owned(),
                outcome: invalid,
            };

            let result = resolve_ready_child(Some(&mut child), PingReadiness::Legacy, || {
                future::ready(legacy_status_identity(&response).map(|identity| identity.owner_pid))
            })
            .await;

            assert!(result.is_err());
            assert_eq!(child.cleanup_count, 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn modern_ready_with_child_skips_legacy_status_lookup() -> anyhow::Result<()> {
        let status_calls = Cell::new(0_u32);
        let mut child = FakeStartupChild::new(42, vec![]);

        let disposition = resolve_ready_child(Some(&mut child), PingReadiness::Owner(42), || {
            status_calls.set(status_calls.get() + 1);
            future::ready(Ok(41))
        })
        .await?;

        assert_eq!(disposition, ReadyChildDisposition::LiveOwner);
        assert_eq!(status_calls.get(), 0);
        assert_eq!(child.cleanup_count, 0);
        Ok(())
    }

    #[test]
    fn ready_non_owner_child_is_reaped_after_exit() -> anyhow::Result<()> {
        assert_eq!(
            classify_ready_child(Some(41), 42),
            ReadyChildDisposition::ReapContender
        );
        let mut child = FakeStartupChild::new(41, vec![Ok(Some("exit code: 1".to_owned()))]);

        assert_eq!(
            poll_non_owner_contender(&mut child, false)?,
            ReapProgress::Reaped("exit code: 1".to_owned())
        );
        assert_eq!(child.cleanup_count, 0);
        Ok(())
    }

    #[test]
    fn delayed_non_owner_is_not_returned_before_exit() -> anyhow::Result<()> {
        let mut child =
            FakeStartupChild::new(41, vec![Ok(None), Ok(Some("exit code: 1".to_owned()))]);

        assert_eq!(
            poll_non_owner_contender(&mut child, false)?,
            ReapProgress::Pending
        );
        assert_eq!(
            poll_non_owner_contender(&mut child, false)?,
            ReapProgress::Reaped("exit code: 1".to_owned())
        );
        assert_eq!(child.cleanup_count, 0);
        Ok(())
    }

    #[test]
    fn child_inspection_error_cleans_up_exact_contender() -> anyhow::Result<()> {
        let mut child = FakeStartupChild::new(41, vec![Err(io::Error::other("inspection failed"))]);

        let Err(error) = poll_non_owner_contender(&mut child, false) else {
            anyhow::bail!("inspection failure unexpectedly succeeded");
        };
        assert!(error.to_string().contains("cannot inspect daemon child"));
        assert_eq!(child.cleanup_count, 1);
        Ok(())
    }

    #[test]
    fn child_timeout_terminates_and_reaps_exact_contender() -> anyhow::Result<()> {
        let mut child = FakeStartupChild::new(41, vec![Ok(None)]);

        let Err(error) = poll_non_owner_contender(&mut child, true) else {
            anyhow::bail!("expired contender unexpectedly remained pending");
        };
        assert!(error.to_string().contains("startup deadline"));
        assert_eq!(child.cleanup_count, 1);
        Ok(())
    }

    #[test]
    fn child_wait_failure_is_reported_after_one_cleanup_attempt() -> anyhow::Result<()> {
        let mut child = FakeStartupChild::new(41, vec![Ok(None)]);
        child.cleanup_fails = true;

        let Err(error) = poll_non_owner_contender(&mut child, true) else {
            anyhow::bail!("failed contender cleanup unexpectedly succeeded");
        };
        assert!(
            error
                .to_string()
                .contains("cannot clean up timed-out daemon child")
        );
        assert_eq!(child.cleanup_count, 1);
        Ok(())
    }

    #[test]
    fn child_exit_at_deadline_is_reported_without_cleanup() -> anyhow::Result<()> {
        let mut child = FakeStartupChild::new(41, vec![Ok(Some("exit code: 1".to_owned()))]);

        assert_eq!(
            inspect_child_at_startup_deadline(&mut child)?,
            Some("exit code: 1".to_owned())
        );
        assert_eq!(child.cleanup_count, 0);
        Ok(())
    }

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

    #[cfg(windows)]
    async fn serve_fake_legacy_daemon(
        first_server: tokio::net::windows::named_pipe::NamedPipeServer,
        legacy: std::path::PathBuf,
        instance_id: orchestrator_domain::DaemonInstanceId,
    ) -> anyhow::Result<()> {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
        use tokio::net::windows::named_pipe::ServerOptions;

        let mut first_server = Some(first_server);
        for connection_index in 0..2 {
            let server = if let Some(server) = first_server.take() {
                server
            } else {
                let mut options = ServerOptions::new();
                options.reject_remote_clients(true);
                options.create(&legacy)?
            };
            server.connect().await?;
            let (reader, mut writer) = tokio::io::split(server);
            let mut lines = BufReader::new(reader).lines();
            let status_request: IpcRequest =
                serde_json::from_str(&lines.next_line().await?.context("status request")?)?;
            assert_eq!(status_request.action, "daemon.status");
            let status = IpcResponse {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: status_request.request_id,
                outcome: json!({"status": "ok", "data": {"status": {
                    "state": "online",
                    "instance": {"instance_id": instance_id, "pid": 42}
                }}}),
            };
            writer.write_all(&serde_json::to_vec(&status)?).await?;
            writer.write_all(b"\n").await?;
            let actual_request: IpcRequest =
                serde_json::from_str(&lines.next_line().await?.context("actual request")?)?;
            assert_eq!(
                actual_request.action,
                if connection_index == 0 {
                    "daemon.ping"
                } else {
                    "daemon.stop"
                }
            );
            let actual = IpcResponse {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: actual_request.request_id,
                outcome: if connection_index == 0 {
                    json!({"status": "ok", "data": {"ready": true}})
                } else {
                    json!({"status": "ok", "data": {"requested": true}})
                },
            };
            writer.write_all(&serde_json::to_vec(&actual)?).await?;
            writer.write_all(b"\n").await?;
        }
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_new_client_selects_and_stays_on_validated_legacy_daemon() -> anyhow::Result<()>
    {
        use anyhow::Context as _;
        use chrono::{TimeDelta, Utc};
        use orchestrator_domain::DaemonInstanceId;
        use orchestrator_state::{
            DaemonLeaseRequest, Database, GlobalStatePaths, StateEnvironment,
        };
        use tokio::net::windows::named_pipe::ServerOptions;

        use super::{DaemonEndpoint, discover_live_endpoint, open_response_stream};

        let temporary = tempfile::tempdir()?;
        let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
            temporary.path().join("legacy-home"),
        )?)?;
        let database = Database::open(&paths.database)?;
        database.migrate_with_backup(&paths.backups)?;
        let instance_id: DaemonInstanceId = "018f68d2-00f0-7000-8000-000000000042".parse()?;
        database.acquire_daemon_lease(&DaemonLeaseRequest {
            instance_id,
            pid: 42,
            started_at: Utc::now(),
            ttl: TimeDelta::minutes(1),
        })?;
        let candidates = orchestrator_daemon::ipc_endpoint_candidates(&paths)?;
        let legacy = candidates
            .legacy()
            .context("Windows candidates omitted the v1 legacy endpoint")?
            .to_path_buf();
        let mut first_options = ServerOptions::new();
        first_options
            .first_pipe_instance(true)
            .reject_remote_clients(true);
        let first_server = first_options.create(&legacy)?;
        let fake_legacy = tokio::spawn(serve_fake_legacy_daemon(first_server, legacy, instance_id));

        let (selected, readiness) = discover_live_endpoint(&paths, &candidates)
            .await?
            .context("new client did not discover the legacy daemon")?;
        assert_eq!(readiness, PingReadiness::Legacy);
        assert!(matches!(
            selected,
            DaemonEndpoint {
                validation: super::EndpointValidation::Legacy(_),
                ..
            }
        ));
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "legacy-stop".to_owned(),
            workspace_id: None,
            action: "daemon.stop".to_owned(),
            payload: json!({}),
        };
        let mut stream = open_response_stream(&paths, &selected, &request).await?;
        let response = stream.next().await?.context("legacy stop response")?;
        assert_eq!(response.request_id, "legacy-stop");
        fake_legacy.await??;
        Ok(())
    }
}
