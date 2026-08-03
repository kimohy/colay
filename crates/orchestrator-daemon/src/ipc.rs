use std::{
    fs::{File, OpenOptions},
    io::Write as _,
    path::{Component, Path, PathBuf},
    str::FromStr as _,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use fs2::FileExt as _;
use orchestrator_domain::{
    ClientCommand, ClientCommandId, CorrelationId, EventActor, EventId, EventType, GraphRevisionId,
    ProviderHealth, SchemaVersion, SessionId, TaskEvent, TaskId, UsageSnapshot, UsageSource,
};
#[cfg(windows)]
use orchestrator_state::reject_symlink_components;
use orchestrator_state::{
    ArtifactStore, ConfigEnvironment, ConfigRequest, Database, DatabaseHealth, GlobalStatePaths,
    LegacyImporter, RepositoryStatePaths, ResumeDisposition, RootConfig, SessionListFilter,
    StateError, TaskListFilter, WorkspaceDatabase, WorkspaceId, WorkspaceOutboxRecord,
    WorkspaceReadRequest, WorkspaceRegistration, WorkspaceStatePaths, ensure_private_directory,
    ensure_private_file, load_effective_config,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(windows)]
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::{
    io::{
        AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _,
        BufReader,
    },
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

pub const IPC_SCHEMA_VERSION: u32 = 1;
pub const WORKSPACE_DOCTOR_SCHEMA_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const TASK_STREAM_POLL_INTERVAL: Duration = Duration::from_millis(25);
const TASK_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(test)]
struct TaskStreamInterleaveHook {
    request_id: String,
    status_read: oneshot::Sender<()>,
    resume_outbox_scan: oneshot::Receiver<()>,
}

#[cfg(test)]
static TASK_STREAM_INTERLEAVE_HOOK: std::sync::Mutex<Option<TaskStreamInterleaveHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
async fn pause_task_stream_after_status_read(request_id: &str) {
    let hook = {
        let mut hook = TASK_STREAM_INTERLEAVE_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if hook
            .as_ref()
            .is_some_and(|hook| hook.request_id == request_id)
        {
            hook.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        let _ = hook.status_read.send(());
        let _ = hook.resume_outbox_scan.await;
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcRequest {
    pub schema_version: u32,
    pub request_id: String,
    pub workspace_id: Option<WorkspaceId>,
    pub action: String,
    pub payload: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcResponse {
    pub schema_version: u32,
    pub request_id: String,
    pub outcome: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDoctorDiagnostics {
    pub schema_version: u32,
    pub database: DatabaseHealth,
    pub daemon: orchestrator_state::DaemonStatus,
    pub workspace: WorkspaceRegistration,
    pub audit: WorkspaceAuditDiagnostics,
    pub artifacts: WorkspaceArtifactDiagnostics,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDoctorLookup {
    pub registered: bool,
    pub database: DatabaseHealth,
    pub daemon: orchestrator_state::DaemonStatus,
    pub diagnostics: Option<WorkspaceDoctorDiagnostics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_import: Option<LegacyImportDoctorStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDoctorCapabilities {
    pub legacy_import_evidence_supported: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LegacyImportDoctorStatus {
    pub source_fingerprint: String,
    pub pending: bool,
    pub imported: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceActivation {
    pub workspace_id: WorkspaceId,
    pub repository: PathBuf,
    pub explicit_config: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceAuditDiagnostics {
    pub workspace_id: WorkspaceId,
    pub verified_events: i64,
    pub last_sequence: i64,
    pub last_hash: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceArtifactScope {
    PersistedReferences,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceArtifactDiagnostics {
    pub root: PathBuf,
    pub verified_references: usize,
    pub scope: WorkspaceArtifactScope,
}

impl WorkspaceDoctorDiagnostics {
    pub fn validate(
        &self,
        expected_workspace_id: WorkspaceId,
        expected_repository: &Path,
        expected_artifact_root: &Path,
    ) -> Result<(), IpcError> {
        if self.schema_version != WORKSPACE_DOCTOR_SCHEMA_VERSION {
            return Err(IpcError::Protocol(format!(
                "workspace doctor schema {} is unsupported; expected {WORKSPACE_DOCTOR_SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        if self.workspace.workspace_id != expected_workspace_id
            || self.audit.workspace_id != expected_workspace_id
        {
            return Err(IpcError::Protocol(
                "workspace doctor response identity does not match the request".to_owned(),
            ));
        }
        let expected_repository = std::fs::canonicalize(expected_repository).map_err(|error| {
            IpcError::Protocol(format!(
                "workspace doctor request repository could not be canonicalized: {error}"
            ))
        })?;
        if self.workspace.canonical_path != expected_repository {
            return Err(IpcError::Protocol(
                "workspace doctor response path does not match the request".to_owned(),
            ));
        }
        if self.audit.verified_events < 0
            || self.audit.last_sequence != self.audit.verified_events
            || (self.audit.last_sequence == 0) != self.audit.last_hash.is_none()
        {
            return Err(IpcError::Protocol(
                "workspace doctor audit summary is internally inconsistent".to_owned(),
            ));
        }
        if self.artifacts.root != expected_artifact_root {
            return Err(IpcError::Protocol(
                "workspace doctor artifact root does not match the registered workspace".to_owned(),
            ));
        }
        Ok(())
    }
}

impl IpcResponse {
    fn success(request_id: String, data: &Value) -> Self {
        Self {
            schema_version: IPC_SCHEMA_VERSION,
            request_id,
            outcome: json!({"status": "ok", "data": data}),
        }
    }

    fn failure(request_id: String, error: impl Into<String>) -> Self {
        Self {
            schema_version: IPC_SCHEMA_VERSION,
            request_id,
            outcome: json!({"status": "error", "error": error.into()}),
        }
    }
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("daemon singleton is already owned by another process")]
    AlreadyOwned,
    #[error("IPC protocol error: {0}")]
    Protocol(String),
    #[error("daemon writer queue is unavailable")]
    WriterUnavailable,
    #[error("IPC I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    State(#[from] StateError),
}

pub struct DaemonOwnerLock {
    file: File,
}

impl DaemonOwnerLock {
    pub fn acquire(paths: &GlobalStatePaths) -> Result<Self, IpcError> {
        let lock_path = paths.runtime.join("daemon.lock");

        #[cfg(windows)]
        let _bootstrap_guard = windows_owner_bootstrap_guard(paths)?;
        #[cfg(windows)]
        let lock_preexisting = lock_path.try_exists().map_err(|source| IpcError::Io {
            path: lock_path.clone(),
            source,
        })?;

        #[cfg(not(windows))]
        ensure_private_directory(&paths.runtime)?;
        #[cfg(windows)]
        if !lock_preexisting {
            // The current-user-only kernel mutex makes this the sole bootstrap process.
            // Harden the directory before the lock file is created so that the new file
            // inherits a private DACL even before its explicit owner verification.
            ensure_private_directory(&paths.runtime)?;
        }

        #[cfg(windows)]
        reject_symlink_components(&lock_path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| IpcError::Io {
                path: lock_path.clone(),
                source,
            })?;
        #[cfg(not(windows))]
        ensure_private_file(&lock_path)?;
        file.try_lock_exclusive().map_err(|error| {
            if lock_is_contended(&error) {
                IpcError::AlreadyOwned
            } else {
                IpcError::Io {
                    path: lock_path.clone(),
                    source: error,
                }
            }
        })?;

        #[cfg(windows)]
        // Only the file-lock owner may mutate existing shared ACLs. Both artifacts are
        // private before `acquire` returns, so callers cannot open the database or IPC
        // listener first. A losing contender returns above without changing either DACL.
        ensure_private_directory(&paths.runtime)?;
        #[cfg(windows)]
        ensure_private_file(&lock_path)?;
        Ok(Self { file })
    }
}

#[cfg(windows)]
struct WindowsOwnerBootstrapGuard {
    _mutex: orchestrator_windows_ipc::CurrentUserMutex,
    _directory_tree: orchestrator_windows_ipc::CurrentUserDirectoryTree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpcEndpointCandidates {
    primary: PathBuf,
    #[cfg(windows)]
    legacy: PathBuf,
    #[cfg(windows)]
    serve_legacy: bool,
}

impl IpcEndpointCandidates {
    #[must_use]
    pub fn primary(&self) -> &Path {
        &self.primary
    }

    #[must_use]
    pub fn legacy(&self) -> Option<&Path> {
        #[cfg(windows)]
        {
            Some(&self.legacy)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    #[must_use]
    pub fn server_endpoints(&self) -> Vec<&Path> {
        let endpoints = vec![self.primary.as_path()];
        #[cfg(windows)]
        {
            let mut endpoints = endpoints;
            if self.serve_legacy && self.legacy != self.primary {
                endpoints.push(self.legacy.as_path());
            }
            endpoints
        }
        #[cfg(not(windows))]
        {
            endpoints
        }
    }
}

#[cfg(windows)]
struct PreparedWindowsIpcIdentity {
    digest: String,
    current_user_sid: String,
    directory_tree: orchestrator_windows_ipc::CurrentUserDirectoryTree,
}

#[cfg(windows)]
fn windows_owner_bootstrap_guard(
    paths: &GlobalStatePaths,
) -> Result<WindowsOwnerBootstrapGuard, IpcError> {
    let identity = prepare_windows_ipc_identity(paths)?;
    let name = windows_owner_mutex_name(&identity.digest);
    let mutex =
        orchestrator_windows_ipc::acquire_current_user_mutex(&name, &identity.current_user_sid)
            .map_err(|source| IpcError::Io {
                path: PathBuf::from(name),
                source,
            })?;
    Ok(WindowsOwnerBootstrapGuard {
        _mutex: mutex,
        _directory_tree: identity.directory_tree,
    })
}

#[cfg(windows)]
fn prepare_windows_ipc_identity(
    paths: &GlobalStatePaths,
) -> Result<PreparedWindowsIpcIdentity, IpcError> {
    let current_user_sid = orchestrator_state::current_windows_user_sid()?;
    let directory_tree = orchestrator_windows_ipc::ensure_current_user_only_directory_tree(
        &paths.root,
        &current_user_sid,
    )
    .map_err(|source| IpcError::Io {
        path: paths.root.clone(),
        source,
    })?;
    let canonical_root = windows_canonical_state_root(paths)?;
    let digest = windows_ipc_identity_digest(&canonical_root, &current_user_sid);
    Ok(PreparedWindowsIpcIdentity {
        digest,
        current_user_sid,
        directory_tree,
    })
}

#[cfg(windows)]
fn windows_canonical_state_root(paths: &GlobalStatePaths) -> Result<PathBuf, IpcError> {
    reject_symlink_components(&paths.root)?;
    let canonical = std::fs::canonicalize(&paths.root).map_err(|source| IpcError::Io {
        path: paths.root.clone(),
        source,
    })?;
    reject_symlink_components(&canonical)?;
    Ok(canonical)
}

#[cfg(windows)]
fn windows_ipc_identity_digest(canonical_root: &Path, current_user_sid: &str) -> String {
    use std::os::windows::ffi::OsStrExt as _;

    let root_units = canonical_root.as_os_str().encode_wide().collect::<Vec<_>>();
    let sid_bytes = current_user_sid.as_bytes();
    let mut digest = Sha256::new();
    digest.update(b"colay-daemon-windows-ipc-identity-v2\0");
    digest.update(
        u64::try_from(root_units.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    for unit in root_units {
        digest.update(unit.to_le_bytes());
    }
    digest.update(
        u64::try_from(sid_bytes.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    digest.update(sid_bytes);
    let digest = digest.finalize();
    hex::encode(&digest[..16])
}

#[cfg(windows)]
fn windows_owner_mutex_name(identity_digest: &str) -> String {
    // Microsoft documents `Global\` as the namespace for one system-wide named instance across
    // logon sessions. Its SeCreateGlobalPrivilege check is limited to file-mapping and symbolic-
    // link objects, not mutexes. CreateMutexW errors still fail closed; there is no `Local\`
    // fallback. The SID-qualified hash and verified current-user-only DACL prevent broader access.
    format!(r"Global\ColayDaemonOwner-v2-{identity_digest}")
}

#[cfg(windows)]
fn windows_primary_pipe_name(identity_digest: &str) -> String {
    format!(r"\\.\pipe\colay-v2-{identity_digest}")
}

#[cfg(windows)]
fn windows_endpoint_candidates_from_identity(
    paths: &GlobalStatePaths,
    identity_digest: &str,
) -> IpcEndpointCandidates {
    let legacy_digest = Sha256::digest(paths.root.to_string_lossy().as_bytes());
    let legacy_suffix = hex::encode(&legacy_digest[..16]);
    IpcEndpointCandidates {
        primary: PathBuf::from(windows_primary_pipe_name(identity_digest)),
        legacy: PathBuf::from(format!(r"\\.\pipe\colay-{legacy_suffix}")),
        // A v1 client cannot prove which non-Unicode root a lossy legacy name represents.
        // New clients may still probe it because they validate the v1 instance against the
        // expected read-only state database before sending their real request.
        serve_legacy: paths.root.to_str().is_some(),
    }
}

pub fn ipc_endpoint_candidates(
    paths: &GlobalStatePaths,
) -> Result<IpcEndpointCandidates, IpcError> {
    #[cfg(unix)]
    {
        Ok(IpcEndpointCandidates {
            primary: paths.runtime.join("daemon.sock"),
        })
    }
    #[cfg(windows)]
    {
        let identity = prepare_windows_ipc_identity(paths)?;
        Ok(windows_endpoint_candidates_from_identity(
            paths,
            &identity.digest,
        ))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(IpcEndpointCandidates {
            primary: paths.runtime.join("daemon.ipc"),
        })
    }
}

fn lock_is_contended(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        // Windows reports a non-blocking LockFileEx collision as either a sharing or lock
        // violation rather than `WouldBlock`.
        matches!(error.raw_os_error(), Some(32 | 33))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

impl Drop for DaemonOwnerLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

pub struct IpcServer {
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
    #[cfg(windows)]
    pipe_names: Vec<String>,
    #[cfg(windows)]
    pipe_owner_sid: String,
    database: Arc<Database>,
    paths: GlobalStatePaths,
    workspace_activations: Option<mpsc::UnboundedSender<WorkspaceActivation>>,
}

impl IpcServer {
    pub fn bind(paths: &GlobalStatePaths, database: Arc<Database>) -> Result<Self, IpcError> {
        ensure_private_directory(&paths.runtime)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let endpoint = ipc_endpoint(paths);
            match std::fs::remove_file(&endpoint) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(IpcError::Io {
                        path: endpoint,
                        source,
                    });
                }
            }
            let listener =
                tokio::net::UnixListener::bind(&endpoint).map_err(|source| IpcError::Io {
                    path: endpoint.clone(),
                    source,
                })?;
            std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).map_err(
                |source| IpcError::Io {
                    path: endpoint,
                    source,
                },
            )?;
            Ok(Self {
                listener,
                database,
                paths: paths.clone(),
                workspace_activations: None,
            })
        }
        #[cfg(windows)]
        {
            let candidates = ipc_endpoint_candidates(paths)?;
            let pipe_names = candidates
                .server_endpoints()
                .into_iter()
                .map(|endpoint| {
                    endpoint.to_str().map(str::to_owned).ok_or_else(|| {
                        IpcError::Protocol(
                            "derived Windows named-pipe endpoint is not Unicode".to_owned(),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Self {
                pipe_names,
                pipe_owner_sid: orchestrator_state::current_windows_user_sid()?,
                database,
                paths: paths.clone(),
                workspace_activations: None,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = database;
            Err(IpcError::Protocol(
                "local IPC is unsupported on this platform".to_owned(),
            ))
        }
    }

    #[must_use]
    pub fn with_workspace_activations(
        mut self,
        workspace_activations: mpsc::UnboundedSender<WorkspaceActivation>,
    ) -> Self {
        self.workspace_activations = Some(workspace_activations);
        self
    }

    pub async fn serve(self, cancellation: CancellationToken) -> Result<(), IpcError> {
        let (writer, receiver) = mpsc::channel(64);
        let writer_database = Arc::clone(&self.database);
        let writer_paths = self.paths.clone();
        let workspace_activations = self.workspace_activations.clone();
        let writer_task = tokio::spawn(async move {
            writer_loop(
                writer_database,
                writer_paths,
                receiver,
                workspace_activations,
            )
            .await;
        });
        let result = self.accept_loop(writer, cancellation).await;
        writer_task.abort();
        let _ = writer_task.await;
        result
    }

    #[cfg(unix)]
    async fn accept_loop(
        self,
        writer: mpsc::Sender<WriterRequest>,
        cancellation: CancellationToken,
    ) -> Result<(), IpcError> {
        let endpoint = ipc_endpoint(&self.paths);
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                () = cancellation.cancelled() => break,
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted.map_err(|source| IpcError::Io {
                        path: endpoint.clone(),
                        source,
                    })?;
                    let connection_writer = writer.clone();
                    let connection_database = Arc::clone(&self.database);
                    let connection_paths = self.paths.clone();
                    connections.spawn(async move {
                        handle_connection(
                            stream,
                            connection_database,
                            connection_paths,
                            connection_writer,
                        ).await
                    });
                }
            }
            while connections.try_join_next().is_some() {}
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        match std::fs::remove_file(&endpoint) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(IpcError::Io {
                    path: endpoint,
                    source,
                });
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    async fn accept_loop(
        self,
        writer: mpsc::Sender<WriterRequest>,
        cancellation: CancellationToken,
    ) -> Result<(), IpcError> {
        let listener_cancellation = cancellation.child_token();
        let mut listeners = JoinSet::new();
        for pipe_name in self.pipe_names {
            let pipe_owner_sid = self.pipe_owner_sid.clone();
            let database = Arc::clone(&self.database);
            let paths = self.paths.clone();
            let writer = writer.clone();
            let cancellation = listener_cancellation.clone();
            listeners.spawn(async move {
                accept_windows_pipe_loop(
                    pipe_name,
                    pipe_owner_sid,
                    database,
                    paths,
                    writer,
                    cancellation,
                )
                .await
            });
        }
        drop(writer);

        let mut first_error = None;
        while let Some(joined) = listeners.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                    listener_cancellation.cancel();
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| {
                        IpcError::Protocol(format!(
                            "Windows named-pipe listener task failed: {error}"
                        ))
                    });
                    listener_cancellation.cancel();
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(windows)]
async fn accept_windows_pipe_loop(
    pipe_name: String,
    pipe_owner_sid: String,
    database: Arc<Database>,
    paths: GlobalStatePaths,
    writer: mpsc::Sender<WriterRequest>,
    cancellation: CancellationToken,
) -> Result<(), IpcError> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut first_instance = true;
    let mut connections = JoinSet::new();
    loop {
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first_instance)
            .reject_remote_clients(true);
        let server = orchestrator_windows_ipc::create_current_user_only_named_pipe(
            &options,
            &pipe_name,
            &pipe_owner_sid,
        )
        .map_err(|source| IpcError::Io {
            path: PathBuf::from(&pipe_name),
            source,
        })?;
        first_instance = false;
        tokio::select! {
            () = cancellation.cancelled() => break,
            connected = server.connect() => {
                connected.map_err(|source| IpcError::Io {
                    path: PathBuf::from(&pipe_name),
                    source,
                })?;
                let connection_writer = writer.clone();
                let connection_database = Arc::clone(&database);
                let connection_paths = paths.clone();
                connections.spawn(async move {
                    handle_connection(
                        server,
                        connection_database,
                        connection_paths,
                        connection_writer,
                    ).await
                });
            }
        }
        while connections.try_join_next().is_some() {}
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

#[cfg(windows)]
pub fn windows_named_pipe_security_descriptor(
    client: &tokio::net::windows::named_pipe::NamedPipeClient,
) -> std::io::Result<String> {
    orchestrator_windows_ipc::named_pipe_security_descriptor(client)
}

#[must_use]
#[cfg(not(windows))]
pub fn ipc_endpoint(paths: &GlobalStatePaths) -> PathBuf {
    #[cfg(unix)]
    {
        paths.runtime.join("daemon.sock")
    }
    #[cfg(not(any(unix, windows)))]
    {
        paths.runtime.join("daemon.ipc")
    }
}

async fn handle_connection<S>(
    stream: S,
    database: Arc<Database>,
    paths: GlobalStatePaths,
    writer: mpsc::Sender<WriterRequest>,
) -> Result<(), IpcError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut output) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = Vec::new();
        let count = (&mut reader)
            .take(u64::try_from(MAX_REQUEST_BYTES.saturating_add(1)).unwrap_or(u64::MAX))
            .read_until(b'\n', &mut line)
            .await
            .map_err(|source| IpcError::Io {
                path: PathBuf::from("local IPC stream"),
                source,
            })?;
        if count == 0 {
            return Ok(());
        }
        let oversized = count > MAX_REQUEST_BYTES;
        let response = if oversized {
            IpcResponse::failure(String::new(), "IPC request exceeds the one MiB limit")
        } else {
            match serde_json::from_slice::<IpcRequest>(&line) {
                Ok(request)
                    if request.schema_version == IPC_SCHEMA_VERSION
                        && request.action == "workspace.task.stream" =>
                {
                    stream_task_status(&mut output, &database, &request).await?;
                    return Ok(());
                }
                Ok(request) => dispatch_request(request, &database, &paths, &writer).await,
                Err(_) => IpcResponse::failure(String::new(), "IPC request is not valid JSON"),
            }
        };
        write_response(&mut output, &response).await?;
        if oversized {
            return Ok(());
        }
    }
}

async fn stream_task_status<W>(
    output: &mut W,
    database: &Database,
    request: &IpcRequest,
) -> Result<(), IpcError>
where
    W: AsyncWrite + Unpin,
{
    let (workspace_id, task_id, requested_cursor) =
        match requested_task_with_cursor(database, request) {
            Ok(task) => task,
            Err(error) => {
                write_response(
                    output,
                    &IpcResponse::failure(request.request_id.clone(), error.to_string()),
                )
                .await?;
                return Ok(());
            }
        };
    let workspace = database.workspace(workspace_id);
    let mut scan_cursor = match requested_cursor {
        Some(cursor) => i64::try_from(cursor).map_err(|_| {
            IpcError::Protocol("task stream cursor exceeds the supported range".to_owned())
        })?,
        None => workspace.latest_outbox_sequence()?,
    };
    let mut sent_status = false;
    let mut idle_deadline = Instant::now() + TASK_STREAM_IDLE_TIMEOUT;
    loop {
        let Some(status) =
            load_task_stream_status(output, database, workspace_id, task_id, &request.request_id)
                .await?
        else {
            return Ok(());
        };
        #[cfg(test)]
        pause_task_stream_after_status_read(&request.request_id).await;
        let records = workspace.outbox_after(scan_cursor, 256)?;
        let Some(status) = refresh_task_stream_status_after_scan(
            output,
            database,
            workspace_id,
            task_id,
            &request.request_id,
            status,
            &records,
        )
        .await?
        else {
            return Ok(());
        };
        for record in records {
            scan_cursor = record.sequence;
            if record.event.task_id != Some(task_id) {
                continue;
            }
            let terminal = record
                .event
                .to_state
                .is_some_and(orchestrator_domain::TaskState::is_terminal);
            write_response(
                output,
                &IpcResponse::success(
                    request.request_id.clone(),
                    &json!({
                        "status": status,
                        "event": record.event,
                        "cursor": record.sequence,
                    }),
                ),
            )
            .await?;
            sent_status = true;
            idle_deadline = Instant::now() + TASK_STREAM_IDLE_TIMEOUT;
            if terminal {
                return Ok(());
            }
        }
        if !sent_status {
            let terminal = status.state.is_terminal();
            write_response(
                output,
                &IpcResponse::success(
                    request.request_id.clone(),
                    &json!({
                        "status": status,
                        "cursor": scan_cursor,
                    }),
                ),
            )
            .await?;
            sent_status = true;
            idle_deadline = Instant::now() + TASK_STREAM_IDLE_TIMEOUT;
            if terminal {
                return Ok(());
            }
        }
        let now = Instant::now();
        if now >= idle_deadline {
            return Ok(());
        }
        tokio::time::sleep(TASK_STREAM_POLL_INTERVAL.min(idle_deadline - now)).await;
    }
}

async fn load_task_stream_status<W>(
    output: &mut W,
    database: &Database,
    workspace_id: WorkspaceId,
    task_id: TaskId,
    request_id: &str,
) -> Result<Option<crate::execution::ActiveTaskStatus>, IpcError>
where
    W: AsyncWrite + Unpin,
{
    let status = crate::execution::active_task_status(database, workspace_id, task_id)?;
    if status.is_none() {
        write_response(
            output,
            &IpcResponse::failure(
                request_id.to_owned(),
                format!("task {task_id} no longer exists"),
            ),
        )
        .await?;
    }
    Ok(status)
}

async fn refresh_task_stream_status_after_scan<W>(
    output: &mut W,
    database: &Database,
    workspace_id: WorkspaceId,
    task_id: TaskId,
    request_id: &str,
    previous_status: crate::execution::ActiveTaskStatus,
    records: &[WorkspaceOutboxRecord],
) -> Result<Option<crate::execution::ActiveTaskStatus>, IpcError>
where
    W: AsyncWrite + Unpin,
{
    if records
        .iter()
        .any(|record| record.event.task_id == Some(task_id))
    {
        load_task_stream_status(output, database, workspace_id, task_id, request_id).await
    } else {
        Ok(Some(previous_status))
    }
}

async fn write_response<W>(output: &mut W, response: &IpcResponse) -> Result<(), IpcError>
where
    W: AsyncWrite + Unpin,
{
    let mut encoded = serde_json::to_vec(response)?;
    encoded.push(b'\n');
    output
        .write_all(&encoded)
        .await
        .map_err(|source| IpcError::Io {
            path: PathBuf::from("local IPC stream"),
            source,
        })
}

async fn dispatch_request(
    request: IpcRequest,
    database: &Database,
    paths: &GlobalStatePaths,
    writer: &mpsc::Sender<WriterRequest>,
) -> IpcResponse {
    if request.schema_version != IPC_SCHEMA_VERSION {
        return IpcResponse::failure(
            request.request_id,
            format!(
                "unsupported IPC schema {}; supported schema is {IPC_SCHEMA_VERSION}",
                request.schema_version
            ),
        );
    }
    if let Some(response) = dispatch_read_request(database, paths, &request) {
        return response;
    }
    if !matches!(
        request.action.as_str(),
        "workspace.register"
            | "workspace.command.submit"
            | "workspace.config.write"
            | "workspace.control"
            | "workspace.run.submit"
            | "workspace.resume"
            | "workspace.selection"
            | "workspace.usage.override"
            | "daemon.stop"
    ) {
        return IpcResponse::failure(request.request_id, "unsupported IPC action");
    }
    let request_id = request.request_id.clone();
    let (response, reply) = oneshot::channel();
    let command = WriterRequest { request, response };
    if writer.send(command).await.is_err() {
        return IpcResponse::failure(request_id, "daemon writer is unavailable");
    }
    reply.await.unwrap_or_else(|_| {
        IpcResponse::failure(request_id, "daemon writer stopped before replying")
    })
}

fn dispatch_read_request(
    database: &Database,
    paths: &GlobalStatePaths,
    request: &IpcRequest,
) -> Option<IpcResponse> {
    let result = match request.action.as_str() {
        "daemon.ping" => Ok(json!({"ready": true, "owner_pid": std::process::id()})),
        "daemon.status" => database
            .daemon_status(Utc::now())
            .map(|status| json!({"status": status}))
            .map_err(IpcError::from),
        "workspace.status" => workspace_status(database, paths, request),
        "workspace.doctor.capabilities" => workspace_doctor_capabilities(request),
        "workspace.doctor.lookup" => workspace_doctor_lookup(database, paths, request),
        "workspace.doctor" => workspace_doctor(database, paths, request),
        "workspace.task.stream" => workspace_task_stream(database, request),
        "workspace.checkpoint" => workspace_checkpoint(database, request),
        "workspace.routing" => workspace_routing(database, request),
        "workspace.usage" => workspace_usage(database, request),
        "workspace.dashboard" => workspace_dashboard(database, request),
        "workspace.command.status" => workspace_command_status(database, request),
        "workspace.conversation" => workspace_conversation(database, request),
        "workspace.sessions" => workspace_sessions(database, request),
        "workspace.projection" => workspace_projection(database, request),
        "workspace.graph.revision" => workspace_graph_revision(database, request),
        _ => return None,
    };
    Some(match result {
        Ok(data) => IpcResponse::success(request.request_id.clone(), &data),
        Err(error) => IpcResponse::failure(request.request_id.clone(), error.to_string()),
    })
}

fn workspace_doctor(
    database: &Database,
    paths: &GlobalStatePaths,
    request: &IpcRequest,
) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.doctor requires a registered workspace".to_owned())
    })?;
    let registration = database.load_workspace(workspace_id)?.ok_or_else(|| {
        IpcError::Protocol("workspace.doctor targets an unknown workspace".to_owned())
    })?;
    let diagnostics = workspace_doctor_diagnostics(database, paths, registration)?;
    serde_json::to_value(diagnostics).map_err(IpcError::from)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceDoctorLookupPayload {
    repository: PathBuf,
    #[serde(default)]
    legacy_state_dir: Option<PathBuf>,
    #[serde(default)]
    legacy_source_fingerprint: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceDoctorCapabilitiesPayload {}

fn workspace_doctor_capabilities(request: &IpcRequest) -> Result<Value, IpcError> {
    let _: WorkspaceDoctorCapabilitiesPayload = serde_json::from_value(request.payload.clone())?;
    serde_json::to_value(WorkspaceDoctorCapabilities {
        legacy_import_evidence_supported: true,
    })
    .map_err(IpcError::from)
}

fn workspace_doctor_lookup(
    database: &Database,
    paths: &GlobalStatePaths,
    request: &IpcRequest,
) -> Result<Value, IpcError> {
    if request.workspace_id.is_some() {
        return Err(IpcError::Protocol(
            "workspace.doctor.lookup must not register or bind a workspace".to_owned(),
        ));
    }
    let payload = serde_json::from_value::<WorkspaceDoctorLookupPayload>(request.payload.clone())?;
    let database_health = database.health()?;
    let daemon = database.daemon_status(Utc::now())?;
    let registration = database.find_repository_workspace(&payload.repository)?;
    let legacy_import = match (
        registration.as_ref(),
        payload.legacy_source_fingerprint.as_deref(),
    ) {
        (Some(registration), Some(expected_fingerprint)) => {
            Some(inspect_legacy_import_doctor_status(
                database,
                paths,
                registration.workspace_id,
                &payload.repository,
                payload.legacy_state_dir.as_deref(),
                expected_fingerprint,
            )?)
        }
        _ => None,
    };
    let diagnostics = registration
        .map(|registration| workspace_doctor_diagnostics(database, paths, registration))
        .transpose()?;
    serde_json::to_value(WorkspaceDoctorLookup {
        registered: diagnostics.is_some(),
        database: database_health,
        daemon,
        diagnostics,
        legacy_import,
    })
    .map_err(IpcError::from)
}

fn inspect_legacy_import_doctor_status(
    database: &Database,
    paths: &GlobalStatePaths,
    workspace_id: WorkspaceId,
    repository: &Path,
    legacy_state_dir: Option<&Path>,
    expected_fingerprint: &str,
) -> Result<LegacyImportDoctorStatus, IpcError> {
    let source = daemon_legacy_source(paths, repository, legacy_state_dir)?;
    let plan = LegacyImporter::inspect(&source, paths)
        .map_err(|_| legacy_import_doctor_validation_error())?
        .ok_or_else(legacy_import_doctor_validation_error)?;
    if plan.source_fingerprint != expected_fingerprint {
        return Err(legacy_import_doctor_validation_error());
    }
    let imported = LegacyImporter::completed_import(database, workspace_id, &plan, paths)
        .map_err(|_| legacy_import_doctor_validation_error())?
        .is_some();
    Ok(LegacyImportDoctorStatus {
        source_fingerprint: plan.source_fingerprint,
        pending: !imported,
        imported,
    })
}

fn daemon_legacy_source(
    paths: &GlobalStatePaths,
    repository: &Path,
    expected_state_dir: Option<&Path>,
) -> Result<RepositoryStatePaths, IpcError> {
    let effective = load_effective_config(&ConfigRequest {
        repository,
        cli_config: None,
        environment: ConfigEnvironment {
            colay_home: Some(paths.root.clone()),
            user_home: None,
            colay_config: std::env::var_os("COLAY_CONFIG").map(Into::into),
        },
    })
    .map_err(|_| legacy_import_doctor_validation_error())?;
    let configured = RepositoryStatePaths::from_config(repository, effective.config())
        .map_err(|_| legacy_import_doctor_validation_error())?;
    if let Some(expected_state_dir) = expected_state_dir {
        let mut expected_config = RootConfig::default();
        expected_config.orchestrator.state_dir = expected_state_dir.to_path_buf();
        let expected = RepositoryStatePaths::from_config(repository, &expected_config)
            .map_err(|_| legacy_import_doctor_validation_error())?;
        if expected.root != configured.root {
            return Err(legacy_import_doctor_validation_error());
        }
    }
    Ok(configured)
}

fn legacy_import_doctor_validation_error() -> IpcError {
    IpcError::Protocol("legacy import completion evidence could not be validated".to_owned())
}

fn workspace_doctor_diagnostics(
    database: &Database,
    paths: &GlobalStatePaths,
    registration: WorkspaceRegistration,
) -> Result<WorkspaceDoctorDiagnostics, IpcError> {
    let workspace_id = registration.workspace_id;
    let workspace = database.workspace(workspace_id);
    let workspace_paths = paths.for_workspace(workspace_id);
    let diagnostics = WorkspaceDoctorDiagnostics {
        schema_version: WORKSPACE_DOCTOR_SCHEMA_VERSION,
        database: database.health()?,
        daemon: database.daemon_status(Utc::now())?,
        workspace: registration,
        audit: verify_workspace_audit(&workspace)?,
        artifacts: verify_workspace_artifacts(&workspace, &workspace_paths)?,
    };
    diagnostics.validate(
        workspace_id,
        &diagnostics.workspace.canonical_path,
        &workspace_paths.root,
    )?;
    Ok(diagnostics)
}

fn verify_workspace_audit(
    workspace: &WorkspaceDatabase<'_>,
) -> Result<WorkspaceAuditDiagnostics, IpcError> {
    let head = workspace.latest_outbox_sequence()?;
    if head < 0 {
        return Err(IpcError::Protocol(
            "workspace event head is negative".to_owned(),
        ));
    }
    let mut previous_hash = None;
    for sequence in 1..=head {
        let event = workspace.event_at(sequence)?.ok_or_else(|| {
            IpcError::Protocol(format!("workspace event sequence {sequence} is missing"))
        })?;
        let expected_sequence = u64::try_from(sequence)
            .map_err(|_| IpcError::Protocol("workspace event sequence exceeds u64".to_owned()))?;
        if event.sequence != expected_sequence || event.previous_hash != previous_hash {
            return Err(IpcError::Protocol(format!(
                "workspace event chain diverges at sequence {sequence}"
            )));
        }
        if !event
            .verify_hash()
            .map_err(|error| IpcError::Protocol(error.to_string()))?
        {
            return Err(IpcError::Protocol(format!(
                "workspace event hash is invalid at sequence {sequence}"
            )));
        }
        previous_hash = Some(event.event_hash);
    }
    Ok(WorkspaceAuditDiagnostics {
        workspace_id: workspace.workspace_id(),
        verified_events: head,
        last_sequence: head,
        last_hash: previous_hash,
    })
}

fn verify_workspace_artifacts(
    workspace: &WorkspaceDatabase<'_>,
    paths: &WorkspaceStatePaths,
) -> Result<WorkspaceArtifactDiagnostics, IpcError> {
    let store = ArtifactStore::open_workspace(paths)?;
    let artifacts = workspace.list_artifacts()?;
    for artifact in &artifacts {
        let _ = store.read_verified(artifact)?;
    }
    Ok(WorkspaceArtifactDiagnostics {
        root: store.root().to_path_buf(),
        verified_references: artifacts.len(),
        scope: WorkspaceArtifactScope::PersistedReferences,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceCommandStatusPayload {
    command_id: Option<String>,
    idempotency_key: Option<String>,
}

fn workspace_command_status(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.command.status requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<WorkspaceCommandStatusPayload>(request.payload.clone())?;
    let workspace = database.workspace(workspace_id);
    let command = match (payload.command_id, payload.idempotency_key) {
        (Some(command_id), None) => {
            let command_id = ClientCommandId::from_str(&command_id)
                .map_err(|error| IpcError::Protocol(error.to_string()))?;
            workspace.load_client_command(command_id)?
        }
        (None, Some(idempotency_key)) if !idempotency_key.trim().is_empty() => {
            workspace.load_client_command_by_idempotency_key(&idempotency_key)?
        }
        _ => {
            return Err(IpcError::Protocol(
                "workspace.command.status requires exactly one command identifier".to_owned(),
            ));
        }
    };
    Ok(json!({"command": command}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceConversationPayload {
    session_id: String,
}

fn workspace_conversation(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.conversation requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<WorkspaceConversationPayload>(request.payload.clone())?;
    let session_id = SessionId::from_str(&payload.session_id)
        .map_err(|error| IpcError::Protocol(error.to_string()))?;
    let workspace = database.workspace(workspace_id);
    let session = workspace.load_session(session_id)?;
    let messages = workspace.latest_messages(session_id, 200)?;
    Ok(json!({
        "session": session,
        "messages": messages,
        "graph": workspace.current_graph(session_id)?,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceStatusPayload {
    task_id: Option<String>,
}

fn workspace_status(
    database: &Database,
    paths: &GlobalStatePaths,
    request: &IpcRequest,
) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.status requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<WorkspaceStatusPayload>(request.payload.clone())?;
    let workspace = database.workspace(workspace_id);
    let tasks = if let Some(task_id) = payload.task_id {
        let task_id =
            TaskId::from_str(&task_id).map_err(|error| IpcError::Protocol(error.to_string()))?;
        workspace.load_task(task_id)?.into_iter().collect()
    } else {
        workspace.list_tasks(&TaskListFilter {
            state: None,
            include_archived: false,
            limit: 100,
        })?
    };
    let tasks = tasks
        .into_iter()
        .map(|task| {
            json!({
                "task_id": task.task_id,
                "state": task.state,
                "objective": task.objective,
                "created_at": task.created_at,
                "updated_at": task.updated_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "tasks": tasks,
        "database": database.health()?,
        "state_dir": paths.root,
    }))
}

fn workspace_task_stream(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let (workspace_id, task_id) = requested_task(database, request)?;
    let status = crate::execution::active_task_status(database, workspace_id, task_id)?
        .ok_or_else(|| IpcError::Protocol(format!("task {task_id} does not exist")))?;
    Ok(json!({
        "status": status,
        "cursor": database.workspace(workspace_id).latest_outbox_sequence()?,
    }))
}

struct WriterRequest {
    request: IpcRequest,
    response: oneshot::Sender<IpcResponse>,
}

async fn writer_loop(
    database: Arc<Database>,
    paths: GlobalStatePaths,
    mut receiver: mpsc::Receiver<WriterRequest>,
    workspace_activations: Option<mpsc::UnboundedSender<WorkspaceActivation>>,
) {
    while let Some(command) = receiver.recv().await {
        let activation_request = command.request.clone();
        let mut response = process_writer_request(&database, &paths, command.request);
        if response.outcome.get("status").and_then(Value::as_str) == Some("ok")
            && activation_request.action == "workspace.register"
            && let Some(sender) = workspace_activations.as_ref()
        {
            match workspace_activation(&database, &activation_request, &response).and_then(
                |activation| {
                    sender
                        .send(activation)
                        .map_err(|_| IpcError::WriterUnavailable)
                },
            ) {
                Ok(()) => {}
                Err(error) => {
                    response = IpcResponse::failure(response.request_id, error.to_string());
                }
            }
        }
        let _ = command.response.send(response);
    }
}

fn workspace_activation(
    database: &Database,
    request: &IpcRequest,
    response: &IpcResponse,
) -> Result<WorkspaceActivation, IpcError> {
    let payload = serde_json::from_value::<RegisterWorkspacePayload>(request.payload.clone())?;
    let workspace_id = response
        .outcome
        .pointer("/data/workspace_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            IpcError::Protocol("workspace registration omitted its identity".to_owned())
        })?
        .parse::<WorkspaceId>()
        .map_err(|error| IpcError::Protocol(error.to_string()))?;
    let registration = database.load_workspace(workspace_id)?.ok_or_else(|| {
        IpcError::Protocol("registered workspace disappeared before activation".to_owned())
    })?;
    Ok(WorkspaceActivation {
        workspace_id,
        repository: registration.canonical_path,
        explicit_config: payload.explicit_config,
    })
}

fn process_writer_request(
    database: &Database,
    paths: &GlobalStatePaths,
    request: IpcRequest,
) -> IpcResponse {
    let result = match request.action.as_str() {
        "workspace.register" => register_workspace(database, paths, &request),
        "workspace.command.submit" => submit_workspace_command(database, &request, false),
        "workspace.config.write" => write_workspace_config(database, paths, &request),
        "workspace.control" => submit_workspace_control(database, &request),
        "workspace.run.submit" => submit_run_command(database, &request),
        "workspace.resume" => resume_workspace_task(database, &request),
        "workspace.selection" => save_workspace_selection(database, &request),
        "workspace.usage.override" => override_workspace_usage(database, &request),
        "daemon.stop" => request_daemon_stop(database),
        _ => Err(IpcError::Protocol("unsupported writer action".to_owned())),
    };
    match result {
        Ok(data) => IpcResponse::success(request.request_id, &data),
        Err(error) => IpcResponse::failure(request.request_id, error.to_string()),
    }
}

fn workspace_sessions(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.sessions requires a registered workspace".to_owned())
    })?;
    let sessions = database
        .workspace(workspace_id)
        .list_sessions(&SessionListFilter {
            include_archived: false,
            limit: 100,
        })?;
    Ok(json!({"sessions": sessions}))
}

fn workspace_projection(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.projection requires a registered workspace".to_owned())
    })?;
    let mut read = serde_json::from_value::<WorkspaceReadRequest>(request.payload.clone())?;
    let workspace = database.workspace(workspace_id);
    if read.selected_task_id.is_none() {
        read.selected_task_id = workspace.load_workspace_selected_task(read.session_id)?;
    }
    Ok(json!({
        "projection": workspace.read_workspace_projection(read)?,
        "daemon": database.daemon_status(Utc::now())?,
        "requirement": workspace.current_requirement_revision(read.session_id)?,
        "integration": workspace.current_integration_batch(read.session_id)?,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceGraphRevisionPayload {
    revision_id: String,
}

fn workspace_graph_revision(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.graph.revision requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<WorkspaceGraphRevisionPayload>(request.payload.clone())?;
    let revision_id = GraphRevisionId::from_str(&payload.revision_id)
        .map_err(|error| IpcError::Protocol(error.to_string()))?;
    let revision = database
        .workspace(workspace_id)
        .load_graph_revision(revision_id)?;
    Ok(json!({"revision": revision}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceSelectionPayload {
    session_id: String,
    task_id: Option<String>,
}

fn save_workspace_selection(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.selection requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<WorkspaceSelectionPayload>(request.payload.clone())?;
    let session_id = SessionId::from_str(&payload.session_id)
        .map_err(|error| IpcError::Protocol(error.to_string()))?;
    let task_id = payload
        .task_id
        .map(|value| TaskId::from_str(&value))
        .transpose()
        .map_err(|error| IpcError::Protocol(error.to_string()))?;
    let workspace = database.workspace(workspace_id);
    if workspace.load_session(session_id)?.is_none() {
        return Err(IpcError::Protocol(format!(
            "session {session_id} does not exist"
        )));
    }
    if let Some(task_id) = task_id
        && workspace.load_task(task_id)?.is_none()
    {
        return Err(IpcError::Protocol(format!("task {task_id} does not exist")));
    }
    workspace.save_workspace_selected_task(session_id, task_id, Utc::now())?;
    Ok(json!({"session_id": session_id, "task_id": task_id}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceConfigWritePayload {
    path: PathBuf,
    content: String,
}

fn write_workspace_config(
    database: &Database,
    paths: &GlobalStatePaths,
    request: &IpcRequest,
) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.config.write requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<WorkspaceConfigWritePayload>(request.payload.clone())?;
    let registration = database
        .load_workspace(workspace_id)?
        .ok_or_else(|| IpcError::Protocol("registered workspace disappeared".to_owned()))?;
    let target = normalize_path(&payload.path)?;
    let workspace = normalize_path(&registration.canonical_path)?;
    let global_config = normalize_path(&paths.config)?;
    if target != global_config && !target.starts_with(&workspace) {
        return Err(IpcError::Protocol(
            "configuration mutation escaped the registered workspace and user-global config"
                .to_owned(),
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| IpcError::Protocol("configuration path has no parent".to_owned()))?;
    ensure_private_directory(parent)?;
    orchestrator_state::reject_symlink_components(&target)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|source| IpcError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    temporary
        .write_all(payload.content.as_bytes())
        .map_err(|source| IpcError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| IpcError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary.persist(&target).map_err(|error| IpcError::Io {
        path: target.clone(),
        source: error.error,
    })?;
    ensure_private_file(&target)?;
    Ok(json!({"path": target}))
}

fn normalize_path(path: &Path) -> Result<PathBuf, IpcError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(IpcError::Protocol(
                        "configuration path contains invalid parent traversal".to_owned(),
                    ));
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    if !normalized.is_absolute() {
        return Err(IpcError::Protocol(
            "configuration path must be absolute".to_owned(),
        ));
    }
    Ok(normalized)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceTaskPayload {
    task_id: String,
    #[serde(default)]
    cursor: Option<u64>,
}

fn requested_task(
    database: &Database,
    request: &IpcRequest,
) -> Result<(WorkspaceId, TaskId), IpcError> {
    requested_task_with_cursor(database, request)
        .map(|(workspace_id, task_id, _)| (workspace_id, task_id))
}

fn requested_task_with_cursor(
    database: &Database,
    request: &IpcRequest,
) -> Result<(WorkspaceId, TaskId, Option<u64>), IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace task request requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<WorkspaceTaskPayload>(request.payload.clone())?;
    let task_id = TaskId::from_str(&payload.task_id)
        .map_err(|error| IpcError::Protocol(error.to_string()))?;
    if database
        .workspace(workspace_id)
        .load_task(task_id)?
        .is_none()
    {
        return Err(IpcError::Protocol(format!("task {task_id} does not exist")));
    }
    Ok((workspace_id, task_id, payload.cursor))
}

fn workspace_checkpoint(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let (workspace_id, task_id) = requested_task(database, request)?;
    let checkpoint = database
        .workspace(workspace_id)
        .latest_sealed_checkpoint(task_id)?
        .ok_or_else(|| IpcError::Protocol(format!("task {task_id} has no checkpoint")))?;
    Ok(json!({"checkpoint": checkpoint}))
}

fn workspace_routing(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let (workspace_id, task_id) = requested_task(database, request)?;
    let decision = database
        .workspace(workspace_id)
        .list_routing_audits(task_id, 1)?
        .into_iter()
        .next()
        .ok_or_else(|| IpcError::Protocol(format!("no routing decision for task {task_id}")))?;
    Ok(json!({"decision": decision}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceUsagePayload {
    defaults: Vec<UsageSnapshot>,
}

fn workspace_usage(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.usage requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<WorkspaceUsagePayload>(request.payload.clone())?;
    let snapshots = resolved_usage_snapshots(database, payload.defaults)?;
    Ok(json!({"snapshots": snapshots}))
}

fn resolved_usage_snapshots(
    database: &Database,
    defaults: Vec<UsageSnapshot>,
) -> Result<Vec<UsageSnapshot>, IpcError> {
    defaults
        .into_iter()
        .map(|fallback| {
            database
                .list_global_usage_snapshots(Some(fallback.provider), 256)
                .map(|stored| {
                    stored
                        .into_iter()
                        .find(|snapshot| snapshot.quota_scope == fallback.quota_scope)
                        .unwrap_or(fallback)
                })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceDashboardPayload {
    task_id: Option<String>,
    defaults: Vec<UsageSnapshot>,
}

fn workspace_dashboard(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.dashboard requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<WorkspaceDashboardPayload>(request.payload.clone())?;
    let workspace = database.workspace(workspace_id);
    let tasks = if let Some(task_id) = payload.task_id {
        let task_id =
            TaskId::from_str(&task_id).map_err(|error| IpcError::Protocol(error.to_string()))?;
        workspace
            .load_task(task_id)?
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        workspace.list_tasks(&TaskListFilter {
            state: None,
            include_archived: false,
            limit: 100,
        })?
    };
    let selected_task_id = tasks.first().map(|task| task.task_id);
    let routing = selected_task_id
        .map(|task_id| workspace.list_routing_audits(task_id, 1))
        .transpose()?
        .and_then(|mut records| records.pop());
    let handover = selected_task_id
        .map(|task_id| workspace.latest_handover(task_id))
        .transpose()?
        .flatten();
    let handover_count = selected_task_id
        .map(|task_id| workspace.count_handovers(task_id))
        .transpose()?
        .unwrap_or(0);
    let verification = selected_task_id
        .map(|task_id| workspace.latest_verification(task_id))
        .transpose()?
        .flatten();
    let usage = resolved_usage_snapshots(database, payload.defaults)?;
    let health = usage
        .iter()
        .map(|snapshot| database.latest_provider_health(snapshot.provider))
        .collect::<Result<Vec<Option<ProviderHealth>>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    Ok(json!({
        "tasks": tasks,
        "usage": usage,
        "routing": routing,
        "handover": handover,
        "handover_count": handover_count,
        "verification": verification,
        "provider_health": health,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceUsageOverridePayload {
    snapshot: UsageSnapshot,
    entered_by: String,
}

fn override_workspace_usage(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.usage.override requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<WorkspaceUsageOverridePayload>(request.payload.clone())?;
    if payload.entered_by.trim().is_empty() {
        return Err(IpcError::Protocol(
            "manual usage audit identity must not be blank".to_owned(),
        ));
    }
    if payload.snapshot.source != UsageSource::ManualOverride {
        return Err(IpcError::Protocol(
            "usage override requires a manual-override snapshot".to_owned(),
        ));
    }
    payload.snapshot.validate().map_err(|error| {
        IpcError::Protocol(format!("manual usage snapshot is invalid: {error}"))
    })?;
    let now = Utc::now();
    database.record_global_usage_snapshot_with_event(
        workspace_id,
        &payload.snapshot,
        TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            session_id: None,
            task_id: None,
            occurred_at: now,
            event_type: EventType::UsageCollected,
            from_state: None,
            to_state: None,
            reason: None,
            actor: EventActor::Administrator,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: json!({
                "snapshot": payload.snapshot,
                "entered_by": payload.entered_by,
            }),
            previous_hash: None,
            event_hash: String::new(),
        },
    )?;
    Ok(json!({"snapshot": payload.snapshot}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceControlPayload {
    task_id: String,
    action: orchestrator_state::ControlAction,
    payload: Value,
}

fn submit_workspace_control(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.control requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<WorkspaceControlPayload>(request.payload.clone())?;
    let task_id = TaskId::from_str(&payload.task_id)
        .map_err(|error| IpcError::Protocol(error.to_string()))?;
    let workspace = database.workspace(workspace_id);
    let task = workspace
        .load_task(task_id)?
        .ok_or_else(|| IpcError::Protocol(format!("task {task_id} does not exist")))?;
    if task.state.is_terminal() {
        return Err(IpcError::Protocol(format!(
            "task {task_id} is terminal ({:?})",
            task.state
        )));
    }
    let requested_at = Utc::now();
    let control = workspace.request_control_with_event(
        task_id,
        payload.action,
        payload.payload.clone(),
        "local-ipc-client",
        requested_at,
        TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            session_id: None,
            task_id: Some(task_id),
            occurred_at: requested_at,
            event_type: EventType::ControlRequested,
            from_state: None,
            to_state: None,
            reason: None,
            actor: EventActor::User,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: json!({}),
            previous_hash: None,
            event_hash: String::new(),
        },
    )?;
    Ok(json!({
        "task_id": task_id,
        "control_id": control.control_id,
        "action": payload.action,
        "safe_checkpoint_required": matches!(
            payload.action,
            orchestrator_state::ControlAction::Pause
                | orchestrator_state::ControlAction::Cancel
                | orchestrator_state::ControlAction::Handover
        ),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeWorkspaceTaskPayload {
    task_id: String,
}

fn resume_workspace_task(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.resume requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<ResumeWorkspaceTaskPayload>(request.payload.clone())?;
    let task_id = TaskId::from_str(&payload.task_id)
        .map_err(|error| IpcError::Protocol(error.to_string()))?;
    let daemon_instance_id = match database.daemon_status(Utc::now())? {
        orchestrator_state::DaemonStatus::Online(instance) => instance.instance_id,
        _ => {
            return Err(IpcError::Protocol(
                "resume requires a healthy online user daemon".to_owned(),
            ));
        }
    };
    let workspace = database.workspace(workspace_id);
    let task = workspace
        .load_task(task_id)?
        .ok_or_else(|| IpcError::Protocol(format!("task {task_id} does not exist")))?;
    if task.state.is_terminal() {
        return Err(IpcError::Protocol(format!(
            "terminal task {task_id} cannot be resumed"
        )));
    }
    let disposition = workspace.resume_disposition(task_id, daemon_instance_id, Utc::now())?;
    match disposition {
        ResumeDisposition::Attached => Ok(json!({
            "disposition": disposition,
            "task_id": task_id,
            "stream": "workspace.task.stream",
            "cursor": workspace.latest_outbox_sequence()?,
        })),
        ResumeDisposition::Requeued => {
            let requested_at = Utc::now();
            let control = workspace.requeue_task_for_resume_with_event(
                task_id,
                task.revision,
                "local-ipc-client",
                requested_at,
                TaskEvent {
                    schema_version: SchemaVersion::state_current(),
                    sequence: 0,
                    event_id: EventId::new(),
                    session_id: None,
                    task_id: Some(task_id),
                    occurred_at: requested_at,
                    event_type: EventType::ControlRequested,
                    from_state: Some(task.state),
                    to_state: Some(orchestrator_domain::TaskState::Queued),
                    reason: Some("verified replay-safe resume requeue".to_owned()),
                    actor: EventActor::User,
                    correlation_id: CorrelationId::new(),
                    causation_id: None,
                    payload: json!({"replay_safe": true}),
                    previous_hash: None,
                    event_hash: String::new(),
                },
            )?;
            Ok(json!({
                "disposition": disposition,
                "task_id": task_id,
                "state": orchestrator_domain::TaskState::Queued,
                "control_id": control.control_id,
                "cursor": workspace.latest_outbox_sequence()?,
            }))
        }
        ResumeDisposition::Rejected => Err(IpcError::Protocol(format!(
            "task {task_id} may still be owned by an external process; automatic resume is rejected; reconcile process and worktree ownership, then run `colay task takeover {task_id}`"
        ))),
    }
}

fn submit_workspace_command(
    database: &Database,
    request: &IpcRequest,
    plan_only: bool,
) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.command.submit requires a registered workspace".to_owned())
    })?;
    if database.load_workspace(workspace_id)?.is_none() {
        return Err(IpcError::Protocol(
            "workspace.command.submit targets an unknown workspace".to_owned(),
        ));
    }
    let mut command = serde_json::from_value::<ClientCommand>(request.payload.clone())?;
    "local-ipc-client".clone_into(&mut command.requested_by);
    let stored = database
        .workspace(workspace_id)
        .submit_client_command_with_invocation(&command, plan_only)?;
    Ok(json!({"command": stored}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunCommandPayload {
    command: ClientCommand,
    plan_only: bool,
}

fn submit_run_command(database: &Database, request: &IpcRequest) -> Result<Value, IpcError> {
    let workspace_id = request.workspace_id.ok_or_else(|| {
        IpcError::Protocol("workspace.run.submit requires a registered workspace".to_owned())
    })?;
    let payload = serde_json::from_value::<RunCommandPayload>(request.payload.clone())?;
    if payload.command.action != orchestrator_domain::ClientCommandAction::AppendMessage {
        return Err(IpcError::Protocol(
            "workspace.run.submit requires an append-message command".to_owned(),
        ));
    }
    let mut command = payload.command;
    "local-cli-run".clone_into(&mut command.requested_by);
    let stored = database
        .workspace(workspace_id)
        .submit_client_command_with_invocation(&command, payload.plan_only)?;
    Ok(json!({"command": stored}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterWorkspacePayload {
    repository: PathBuf,
    state_dir: PathBuf,
    explicit_config: Option<PathBuf>,
}

fn register_workspace(
    database: &Database,
    paths: &GlobalStatePaths,
    request: &IpcRequest,
) -> Result<Value, IpcError> {
    let payload = serde_json::from_value::<RegisterWorkspacePayload>(request.payload.clone())?;
    let registration = database.resolve_repository_workspace(&payload.repository)?;
    if request
        .workspace_id
        .is_some_and(|expected| expected != registration.workspace_id)
    {
        return Err(IpcError::Protocol(
            "request workspace does not match the registered repository".to_owned(),
        ));
    }
    let mut config = RootConfig::default();
    config.orchestrator.state_dir = payload.state_dir;
    let legacy = RepositoryStatePaths::from_config(&payload.repository, &config)?;
    let import = LegacyImporter::inspect(&legacy, paths)?
        .map(|plan| LegacyImporter::apply(database, registration.workspace_id, &plan, paths))
        .transpose()?;
    Ok(json!({
        "workspace_id": registration.workspace_id,
        "imported_legacy_state": import.is_some_and(|result| result.imported),
    }))
}

fn request_daemon_stop(database: &Database) -> Result<Value, IpcError> {
    let status = database.daemon_status(Utc::now())?;
    match status {
        orchestrator_state::DaemonStatus::Stopped => {}
        orchestrator_state::DaemonStatus::Booting(instance)
        | orchestrator_state::DaemonStatus::Probing(instance)
        | orchestrator_state::DaemonStatus::Online(instance)
        | orchestrator_state::DaemonStatus::Failed(instance)
        | orchestrator_state::DaemonStatus::Stale(instance) => {
            database.request_daemon_stop(instance.instance_id, Utc::now())?;
        }
    }
    Ok(json!({"requested": true}))
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    #[cfg(windows)]
    use std::{ffi::OsString, os::windows::ffi::OsStringExt as _};

    use chrono::Utc;
    use orchestrator_domain::{
        ConversationMessage, CorrelationId, DaemonInstanceId, EventActor, EventId, EventType,
        MessageId, MessageKind, MessageRole, MessageState, SchemaVersion, SessionId, SessionState,
        TaskEnvelope, TaskEvent, TaskId, TaskState, TransitionGuards,
    };
    use rusqlite::{Connection, params};
    use sha2::{Digest as _, Sha256};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::{
        IPC_SCHEMA_VERSION, IpcError, IpcRequest, IpcResponse, LegacyImportDoctorStatus,
        MAX_REQUEST_BYTES, TASK_STREAM_INTERLEAVE_HOOK, TaskStreamInterleaveHook,
        WorkspaceDoctorLookup, WorkspaceDoctorLookupPayload, dispatch_read_request,
        handle_connection, resume_workspace_task, workspace_conversation, workspace_doctor_lookup,
        workspace_projection,
    };
    #[cfg(windows)]
    use super::{
        ipc_endpoint_candidates, windows_canonical_state_root,
        windows_endpoint_candidates_from_identity, windows_ipc_identity_digest,
        windows_owner_bootstrap_guard, windows_owner_mutex_name, windows_primary_pipe_name,
    };
    use orchestrator_state::{
        DaemonLeaseRequest, DaemonStatus, Database, DatabaseHealth, GlobalStatePaths,
        LegacyImporter, NewSessionRecord, NewTaskRecord, RepositoryStatePaths, RootConfig,
        StateEnvironment, WorkspaceReadRequest,
    };

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacySchemaOneDoctorLookup {
        registered: bool,
        database: DatabaseHealth,
        daemon: DaemonStatus,
        diagnostics: Option<super::WorkspaceDoctorDiagnostics>,
    }

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

    fn seed_legacy_source(
        repository: &Path,
        state_dir: &Path,
        paths: &GlobalStatePaths,
    ) -> Result<(RepositoryStatePaths, String), Box<dyn std::error::Error>> {
        let mut config = RootConfig::default();
        config.orchestrator.state_dir = state_dir.to_path_buf();
        let source = RepositoryStatePaths::from_config(repository, &config)?;
        std::fs::create_dir_all(&source.root)?;
        std::fs::write(source.root.join("config.toml"), "config_version = 4\n")?;
        let connection = Connection::open(&source.database)?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        for (version, name, sql) in MIGRATIONS_THROUGH_V8 {
            connection.execute_batch(sql)?;
            connection.execute(
                "INSERT INTO schema_migrations(version, name, checksum, applied_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    version,
                    name,
                    format!("{:x}", Sha256::digest(sql.as_bytes())),
                    Utc::now().to_rfc3339(),
                ],
            )?;
        }
        let created_at = "2026-08-02T00:00:00Z";
        connection.execute(
            "INSERT INTO sessions(\
                 session_id, schema_version, revision, title, state, created_at, updated_at\
             ) VALUES (?1, '1.0', 0, 'legacy invalid graph', 'planning', ?2, ?2)",
            params!["01987d4e-2a54-7000-8000-000000000001", created_at],
        )?;
        connection.execute(
            "INSERT INTO conversation_messages(\
                 message_id, session_id, task_id, ordinal, role, kind, state, content_redacted, \
                 created_at, finalized_at\
             ) VALUES (?1, ?2, NULL, 1, 'user', 'user_message', 'final', \
                 'legacy invalid graph', ?3, ?3)",
            params![
                "01987d4e-2a54-7000-8000-000000000002",
                "01987d4e-2a54-7000-8000-000000000001",
                created_at,
            ],
        )?;
        connection.execute(
            "INSERT INTO graph_revisions(\
                 revision_id, session_id, goal_message_id, ordinal, status, \
                 proposal_hash, proposal_json, validation_json, planner_provider, \
                 created_at, completed_at\
             ) VALUES (?1, ?2, ?3, 1, 'invalid', NULL, NULL, ?4, 'codex', ?5, ?5)",
            params![
                "01987d4e-2a54-7000-8000-000000000003",
                "01987d4e-2a54-7000-8000-000000000001",
                "01987d4e-2a54-7000-8000-000000000002",
                serde_json::to_string(&serde_json::json!({"errors":["cycle"]}))?,
                created_at,
            ],
        )?;
        drop(connection);
        let plan = LegacyImporter::inspect(&source, paths)?
            .ok_or_else(|| std::io::Error::other("legacy source was not inspectable"))?;
        Ok((source, plan.source_fingerprint))
    }

    #[test]
    fn workspace_doctor_lookup_payload_preserves_schema_one_compatibility()
    -> Result<(), Box<dyn std::error::Error>> {
        let older = serde_json::from_value::<WorkspaceDoctorLookupPayload>(serde_json::json!({
            "repository": "repository"
        }))?;
        let evidence_aware =
            serde_json::from_value::<WorkspaceDoctorLookupPayload>(serde_json::json!({
                "repository": "repository",
                "legacy_state_dir": ".legacy-colay",
                "legacy_source_fingerprint": "sealed-fingerprint"
            }))?;

        assert_eq!(older.legacy_state_dir, None);
        assert_eq!(older.legacy_source_fingerprint, None);
        assert_eq!(
            evidence_aware.legacy_state_dir,
            Some(std::path::PathBuf::from(".legacy-colay"))
        );
        assert_eq!(
            evidence_aware.legacy_source_fingerprint.as_deref(),
            Some("sealed-fingerprint")
        );
        assert_eq!(IPC_SCHEMA_VERSION, 1);
        Ok(())
    }

    #[test]
    fn workspace_doctor_lookup_response_preserves_schema_one_compatibility()
    -> Result<(), Box<dyn std::error::Error>> {
        let older = serde_json::from_value::<WorkspaceDoctorLookup>(serde_json::json!({
            "registered": false,
            "database": {
                "integrity_ok": true,
                "foreign_key_violations": 0,
                "current_schema_version": orchestrator_state::STATE_SCHEMA_VERSION,
                "last_event_sequence": 0
            },
            "daemon": {"state": "stopped"},
            "diagnostics": null
        }))?;
        let evidence_aware = serde_json::from_value::<WorkspaceDoctorLookup>(serde_json::json!({
            "registered": false,
            "database": {
                "integrity_ok": true,
                "foreign_key_violations": 0,
                "current_schema_version": orchestrator_state::STATE_SCHEMA_VERSION,
                "last_event_sequence": 0
            },
            "daemon": {"state": "stopped"},
            "diagnostics": null,
            "legacy_import": {
                "source_fingerprint": "sealed-fingerprint",
                "pending": false,
                "imported": true
            }
        }))?;

        assert_eq!(older.legacy_import, None);
        assert_eq!(
            evidence_aware.legacy_import,
            Some(LegacyImportDoctorStatus {
                source_fingerprint: "sealed-fingerprint".to_owned(),
                pending: false,
                imported: true,
            })
        );
        Ok(())
    }

    #[test]
    fn repository_lookup_response_remains_readable_by_legacy_schema_one_client()
    -> Result<(), Box<dyn std::error::Error>> {
        let response = serde_json::to_value(WorkspaceDoctorLookup {
            registered: false,
            database: DatabaseHealth {
                integrity_ok: true,
                foreign_key_violations: 0,
                current_schema_version: orchestrator_state::STATE_SCHEMA_VERSION,
                last_event_sequence: 0,
            },
            daemon: DaemonStatus::Stopped,
            diagnostics: None,
            legacy_import: None,
        })?;

        let legacy = serde_json::from_value::<LegacySchemaOneDoctorLookup>(response)?;

        assert!(!legacy.registered);
        assert!(legacy.database.integrity_ok);
        assert_eq!(legacy.daemon, DaemonStatus::Stopped);
        assert!(legacy.diagnostics.is_none());
        Ok(())
    }

    #[test]
    fn workspace_doctor_capability_is_discovered_outside_legacy_lookup_response()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let home = temporary.path().join("home");
        std::fs::create_dir_all(&home)?;
        let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
            std::fs::canonicalize(home)?,
        )?)?;
        let database = Database::open(&paths.database)?;
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "doctor-capabilities".to_owned(),
            workspace_id: None,
            action: "workspace.doctor.capabilities".to_owned(),
            payload: serde_json::json!({}),
        };

        let response = dispatch_read_request(&database, &paths, &request)
            .ok_or_else(|| std::io::Error::other("capability action was not dispatched"))?;

        assert_eq!(
            response.outcome,
            serde_json::json!({
                "status": "ok",
                "data": {"legacy_import_evidence_supported": true}
            })
        );
        Ok(())
    }

    #[test]
    fn doctor_lookup_never_trusts_a_client_fingerprint_without_current_source_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let repository = temporary.path().join("repository");
        std::fs::create_dir_all(&repository)?;
        let home = temporary.path().join("home");
        std::fs::create_dir_all(&home)?;
        let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
            std::fs::canonicalize(home)?,
        )?)?;
        let database = Database::open(&paths.database)?;
        database.migrate_with_backup(&paths.backups)?;
        let registration = database.resolve_repository_workspace(&repository)?;
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "doctor-missing-source".to_owned(),
            workspace_id: None,
            action: "workspace.doctor.lookup".to_owned(),
            payload: serde_json::json!({
                "repository": repository,
                "legacy_state_dir": ".colay",
                "legacy_source_fingerprint": "client-only-fingerprint"
            }),
        };

        let Err(error) = workspace_doctor_lookup(&database, &paths, &request) else {
            return Err(std::io::Error::other(
                "client-only evidence must not imply a completed import",
            )
            .into());
        };

        assert_eq!(
            error.to_string(),
            "IPC protocol error: legacy import completion evidence could not be validated"
        );
        assert_eq!(
            database
                .find_repository_workspace(&repository)?
                .map(|workspace| workspace.workspace_id),
            Some(registration.workspace_id)
        );
        assert!(!repository.join(".colay").exists());
        Ok(())
    }

    #[test]
    fn doctor_lookup_rejects_spoofed_alternate_repository_state_dir()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let repository = temporary.path().join("repository");
        std::fs::create_dir_all(&repository)?;
        let home = temporary.path().join("home");
        std::fs::create_dir_all(&home)?;
        let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
            std::fs::canonicalize(home)?,
        )?)?;
        let database = Database::open(&paths.database)?;
        database.migrate_with_backup(&paths.backups)?;
        database.resolve_repository_workspace(&repository)?;
        std::fs::write(
            &paths.config,
            "config_version = 4\n[orchestrator]\nstate_dir = \".trusted-state\"\n",
        )?;
        let (spoofed_source, spoofed_fingerprint) =
            seed_legacy_source(&repository, Path::new(".spoofed-state"), &paths)?;
        let source_before = std::fs::read(&spoofed_source.database)?;
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "doctor-spoofed-state-dir".to_owned(),
            workspace_id: None,
            action: "workspace.doctor.lookup".to_owned(),
            payload: serde_json::json!({
                "repository": repository,
                "legacy_state_dir": ".spoofed-state",
                "legacy_source_fingerprint": spoofed_fingerprint
            }),
        };

        let Err(error) = workspace_doctor_lookup(&database, &paths, &request) else {
            return Err(std::io::Error::other(
                "client-selected alternate state directory was accepted",
            )
            .into());
        };

        assert_eq!(
            error.to_string(),
            "IPC protocol error: legacy import completion evidence could not be validated"
        );
        assert_eq!(std::fs::read(&spoofed_source.database)?, source_before);
        Ok(())
    }

    #[test]
    fn doctor_lookup_uses_daemon_derived_non_default_repository_state_dir()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let repository = temporary.path().join("repository");
        std::fs::create_dir_all(&repository)?;
        let home = temporary.path().join("home");
        std::fs::create_dir_all(&home)?;
        let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
            std::fs::canonicalize(home)?,
        )?)?;
        let database = Database::open(&paths.database)?;
        database.migrate_with_backup(&paths.backups)?;
        database.resolve_repository_workspace(&repository)?;
        std::fs::write(
            &paths.config,
            "config_version = 4\n[orchestrator]\nstate_dir = \".legacy-colay\"\n",
        )?;
        let (_source, fingerprint) =
            seed_legacy_source(&repository, Path::new(".legacy-colay"), &paths)?;
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "doctor-configured-state-dir".to_owned(),
            workspace_id: None,
            action: "workspace.doctor.lookup".to_owned(),
            payload: serde_json::json!({
                "repository": repository,
                "legacy_state_dir": ".legacy-colay",
                "legacy_source_fingerprint": fingerprint
            }),
        };

        let value = workspace_doctor_lookup(&database, &paths, &request)?;
        let lookup = serde_json::from_value::<WorkspaceDoctorLookup>(value)?;
        let status = lookup
            .legacy_import
            .ok_or_else(|| std::io::Error::other("legacy import evidence was omitted"))?;

        assert_eq!(status.source_fingerprint, fingerprint);
        assert!(status.pending);
        assert!(!status.imported);
        Ok(())
    }

    #[test]
    fn doctor_lookup_unregistered_repository_remains_read_only()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let repository = temporary.path().join("repository");
        std::fs::create_dir_all(&repository)?;
        let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
            temporary.path().join("home"),
        )?)?;
        let database = Database::open(&paths.database)?;
        database.migrate_with_backup(&paths.backups)?;
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "doctor-unregistered".to_owned(),
            workspace_id: None,
            action: "workspace.doctor.lookup".to_owned(),
            payload: serde_json::json!({
                "repository": repository,
                "legacy_state_dir": "chosen-state",
                "legacy_source_fingerprint": "unregistered-fingerprint"
            }),
        };

        let value = workspace_doctor_lookup(&database, &paths, &request)?;
        let lookup = serde_json::from_value::<WorkspaceDoctorLookup>(value)?;

        assert!(!lookup.registered);
        assert!(lookup.diagnostics.is_none());
        assert!(lookup.legacy_import.is_none());
        assert!(database.find_repository_workspace(&repository)?.is_none());
        assert!(!repository.join("chosen-state").exists());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_ipc_identity_uses_canonical_case_insensitive_state_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("Daemon-Identity");
        std::fs::create_dir_all(&root)?;
        let canonical = std::fs::canonicalize(&root)?;
        let case_alias = temporary.path().join("dAEMON-iDENTITY");
        let alias_canonical = std::fs::canonicalize(case_alias)?;
        let sid = "S-1-5-21-100-200-300-400";

        assert_eq!(canonical, alias_canonical);
        let identity = windows_ipc_identity_digest(&canonical, sid);
        let alias_identity = windows_ipc_identity_digest(&alias_canonical, sid);

        assert_eq!(identity, alias_identity);
        assert!(windows_owner_mutex_name(&identity).starts_with(r"Global\"));
        assert_eq!(
            windows_owner_mutex_name(&identity).strip_prefix(r"Global\ColayDaemonOwner-v2-"),
            windows_primary_pipe_name(&identity).strip_prefix(r"\\.\pipe\colay-v2-")
        );
        assert_ne!(
            identity,
            windows_ipc_identity_digest(&canonical, "S-1-5-21-100-200-300-401")
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_ipc_identity_hashes_non_unicode_units_losslessly() {
        let prefix = [u16::from(b'C'), u16::from(b':'), u16::from(b'\\')];
        let mut unpaired_units = prefix.to_vec();
        unpaired_units.push(0xD800);
        let mut replacement_units = prefix.to_vec();
        replacement_units.push(0xFFFD);
        let unpaired = PathBuf::from(OsString::from_wide(&unpaired_units));
        let replacement = PathBuf::from(OsString::from_wide(&replacement_units));
        let sid = "S-1-5-21-100-200-300-400";

        assert_ne!(
            windows_ipc_identity_digest(&unpaired, sid),
            windows_ipc_identity_digest(&replacement, sid)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_short_state_root_alias_uses_the_same_primary_endpoint_when_available()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("Colay State Root Long Name");
        std::fs::create_dir_all(&root)?;
        let short_root = orchestrator_windows_ipc::short_path_name(&root)?;

        if short_root == root {
            eprintln!(
                "GetShortPathNameW returned the original path; this volume has no 8.3 alias for the test root"
            );
            assert_eq!(short_root, root);
            return Ok(());
        }

        let paths = |root: PathBuf| GlobalStatePaths {
            database: root.join("state/state.db"),
            backups: root.join("state/backups"),
            workspaces: root.join("data/workspaces"),
            runtime: root.join("runtime"),
            config: root.join("config.toml"),
            root,
        };
        let long_paths = paths(root);
        let short_paths = paths(short_root);
        assert_eq!(
            windows_canonical_state_root(&long_paths)?,
            windows_canonical_state_root(&short_paths)?
        );
        assert_eq!(
            ipc_endpoint_candidates(&long_paths)?.primary(),
            ipc_endpoint_candidates(&short_paths)?.primary()
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_endpoint_candidates_preserve_primary_and_version_one_legacy_names() {
        use sha2::{Digest as _, Sha256};

        let root = PathBuf::from(r"C:\Users\current\Colay");
        let paths = GlobalStatePaths {
            database: root.join("state/state.db"),
            backups: root.join("state/backups"),
            workspaces: root.join("data/workspaces"),
            runtime: root.join("runtime"),
            config: root.join("config.toml"),
            root: root.clone(),
        };
        let identity = "0123456789abcdef0123456789abcdef";
        let candidates = windows_endpoint_candidates_from_identity(&paths, identity);
        let legacy_digest = Sha256::digest(root.to_string_lossy().as_bytes());
        let legacy_suffix = hex::encode(&legacy_digest[..16]);

        assert_eq!(
            candidates.primary(),
            Path::new(r"\\.\pipe\colay-v2-0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            candidates.legacy(),
            Some(Path::new(&format!(r"\\.\pipe\colay-{legacy_suffix}")))
        );
        assert_eq!(candidates.server_endpoints().len(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn windows_non_unicode_root_keeps_legacy_client_candidate_but_not_server_listener() {
        let mut units = vec![u16::from(b'C'), u16::from(b':'), u16::from(b'\\')];
        units.push(0xD800);
        let root = PathBuf::from(OsString::from_wide(&units));
        let paths = GlobalStatePaths {
            database: root.join("state/state.db"),
            backups: root.join("state/backups"),
            workspaces: root.join("data/workspaces"),
            runtime: root.join("runtime"),
            config: root.join("config.toml"),
            root,
        };

        let candidates =
            windows_endpoint_candidates_from_identity(&paths, "0123456789abcdef0123456789abcdef");

        assert!(candidates.legacy().is_some());
        assert_eq!(candidates.server_endpoints(), [candidates.primary()]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_bootstrap_securely_creates_a_missing_state_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("missing-state-root");
        let paths = GlobalStatePaths {
            database: root.join("state/state.db"),
            backups: root.join("state/backups"),
            workspaces: root.join("data/workspaces"),
            runtime: root.join("runtime"),
            config: root.join("config.toml"),
            root: root.clone(),
        };

        let _guard = windows_owner_bootstrap_guard(&paths)?;

        assert!(root.is_dir());
        assert!(std::fs::canonicalize(&root)?.is_dir());
        Ok(())
    }

    #[tokio::test]
    async fn oversized_request_is_rejected_and_connection_is_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = Arc::new(Database::open_in_memory()?);
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        let root = std::path::PathBuf::from("unused");
        let paths = GlobalStatePaths {
            database: root.join("state.db"),
            backups: root.join("backups"),
            workspaces: root.join("workspaces"),
            runtime: root.join("runtime"),
            config: root.join("config.toml"),
            root,
        };
        let (writer, _receiver) = tokio::sync::mpsc::channel(1);
        let (mut client, server) = tokio::io::duplex(MAX_REQUEST_BYTES.saturating_add(2));
        let server_task = tokio::spawn(handle_connection(server, database, paths, writer));

        client
            .write_all(&vec![b'x'; MAX_REQUEST_BYTES.saturating_add(1)])
            .await?;
        let mut response = String::new();
        BufReader::new(client).read_line(&mut response).await?;
        let response = serde_json::from_str::<IpcResponse>(&response)?;

        assert_eq!(
            response
                .outcome
                .get("error")
                .and_then(serde_json::Value::as_str),
            Some("IPC request exceeds the one MiB limit")
        );
        server_task.await??;
        Ok(())
    }

    #[test]
    fn resume_requeues_pre_worker_state_into_scheduler_eligible_queue()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        let repository = tempfile::tempdir()?;
        let workspace_id = database
            .resolve_repository_workspace(repository.path())?
            .workspace_id;
        let task_id = TaskId::new();
        let now = Utc::now();
        let envelope = TaskEnvelope::new("resume analyzed task", "resume analyzed task", now);
        database
            .workspace(workspace_id)
            .create_task(&NewTaskRecord {
                task_id,
                schema_version: SchemaVersion::V1.to_owned(),
                state: TaskState::Analyzing,
                objective: envelope.objective.clone(),
                original_request_redacted: envelope.original_request_redacted.clone(),
                envelope,
                created_at: now,
            })?;
        database.acquire_daemon_lease(&DaemonLeaseRequest {
            instance_id: DaemonInstanceId::new(),
            pid: 41,
            started_at: now,
            ttl: chrono::TimeDelta::minutes(5),
        })?;

        let response = resume_workspace_task(
            &database,
            &IpcRequest {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: "resume-pre-worker".to_owned(),
                workspace_id: Some(workspace_id),
                action: "workspace.resume".to_owned(),
                payload: serde_json::json!({"task_id": task_id}),
            },
        )?;

        assert_eq!(response["disposition"], "requeued");
        assert_eq!(response["cursor"], 1);
        let task = database
            .workspace(workspace_id)
            .load_task(task_id)?
            .ok_or("resumed task disappeared")?;
        assert_eq!(task.state, TaskState::Queued);
        assert!(!task.paused);
        Ok(())
    }

    #[tokio::test]
    async fn task_status_stream_emits_new_revisions_and_closes_after_terminal_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = Arc::new(Database::open_in_memory()?);
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        let repository = tempfile::tempdir()?;
        let workspace_id = database
            .resolve_repository_workspace(repository.path())?
            .workspace_id;
        let workspace = database.workspace(workspace_id);
        let task_id = TaskId::new();
        let now = Utc::now();
        let envelope = TaskEnvelope::new("stream active task", "stream active task", now);
        workspace.create_task(&NewTaskRecord {
            task_id,
            schema_version: SchemaVersion::V1.to_owned(),
            state: TaskState::Running,
            objective: envelope.objective.clone(),
            original_request_redacted: envelope.original_request_redacted.clone(),
            envelope,
            created_at: now,
        })?;
        let root = std::path::PathBuf::from("unused");
        let paths = GlobalStatePaths {
            database: root.join("state.db"),
            backups: root.join("backups"),
            workspaces: root.join("workspaces"),
            runtime: root.join("runtime"),
            config: root.join("config.toml"),
            root,
        };
        let (writer, _receiver) = tokio::sync::mpsc::channel(1);
        let (client, server) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(handle_connection(
            server,
            Arc::clone(&database),
            paths,
            writer,
        ));
        let (reader, mut output) = tokio::io::split(client);
        let mut reader = BufReader::new(reader);
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "task-stream".to_owned(),
            workspace_id: Some(workspace_id),
            action: "workspace.task.stream".to_owned(),
            payload: serde_json::json!({"task_id": task_id}),
        };
        output
            .write_all(format!("{}\n", serde_json::to_string(&request)?).as_bytes())
            .await?;

        let initial = read_response(&mut reader).await?;
        assert_eq!(initial.outcome["data"]["cursor"], 0);
        assert_eq!(initial.outcome["data"]["status"]["state"], "running");

        workspace.transition_task_with_event(
            task_id,
            0,
            TaskState::Running,
            TaskState::Verifying,
            None,
            false,
            &TransitionGuards::default(),
            now,
            transition_event(task_id, TaskState::Running, TaskState::Verifying, now),
        )?;
        let verifying =
            tokio::time::timeout(Duration::from_secs(1), read_response(&mut reader)).await??;
        assert_eq!(verifying.outcome["data"]["cursor"], 1);
        assert_eq!(verifying.outcome["data"]["status"]["state"], "verifying");

        workspace.transition_task_with_event(
            task_id,
            1,
            TaskState::Verifying,
            TaskState::Completed,
            None,
            false,
            &TransitionGuards {
                verification_passed: true,
                ..TransitionGuards::default()
            },
            now,
            transition_event(task_id, TaskState::Verifying, TaskState::Completed, now),
        )?;
        let completed =
            tokio::time::timeout(Duration::from_secs(1), read_response(&mut reader)).await??;
        assert_eq!(completed.outcome["data"]["cursor"], 2);
        assert_eq!(completed.outcome["data"]["status"]["state"], "completed");
        tokio::time::timeout(Duration::from_secs(1), server_task).await???;
        Ok(())
    }

    #[tokio::test]
    async fn task_status_stream_never_pairs_terminal_event_with_stale_status()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = Arc::new(Database::open_in_memory()?);
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        let repository = tempfile::tempdir()?;
        let workspace_id = database
            .resolve_repository_workspace(repository.path())?
            .workspace_id;
        let workspace = database.workspace(workspace_id);
        let task_id = TaskId::new();
        let now = Utc::now();
        let envelope = TaskEnvelope::new("status race", "status race", now);
        workspace.create_task(&NewTaskRecord {
            task_id,
            schema_version: SchemaVersion::V1.to_owned(),
            state: TaskState::Running,
            objective: envelope.objective.clone(),
            original_request_redacted: envelope.original_request_redacted.clone(),
            envelope,
            created_at: now,
        })?;
        workspace.transition_task_with_event(
            task_id,
            0,
            TaskState::Running,
            TaskState::Verifying,
            None,
            false,
            &TransitionGuards::default(),
            now,
            transition_event(task_id, TaskState::Running, TaskState::Verifying, now),
        )?;

        let root = std::path::PathBuf::from("unused");
        let paths = GlobalStatePaths {
            database: root.join("state.db"),
            backups: root.join("backups"),
            workspaces: root.join("workspaces"),
            runtime: root.join("runtime"),
            config: root.join("config.toml"),
            root,
        };
        let (writer, _receiver) = tokio::sync::mpsc::channel(1);
        let (client, server) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(handle_connection(
            server,
            Arc::clone(&database),
            paths,
            writer,
        ));
        let (reader, mut output) = tokio::io::split(client);
        let mut reader = BufReader::new(reader);
        let request_id = "task-stream-status-race";
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: request_id.to_owned(),
            workspace_id: Some(workspace_id),
            action: "workspace.task.stream".to_owned(),
            payload: serde_json::json!({"task_id": task_id}),
        };
        output
            .write_all(format!("{}\n", serde_json::to_string(&request)?).as_bytes())
            .await?;

        let initial = read_response(&mut reader).await?;
        assert_eq!(initial.outcome["data"]["status"]["state"], "verifying");
        let (status_read, status_read_waiter) = tokio::sync::oneshot::channel();
        let (resume_outbox_scan, outbox_scan_waiter) = tokio::sync::oneshot::channel();
        *TASK_STREAM_INTERLEAVE_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(TaskStreamInterleaveHook {
            request_id: request_id.to_owned(),
            status_read,
            resume_outbox_scan: outbox_scan_waiter,
        });
        tokio::time::timeout(Duration::from_secs(1), status_read_waiter).await??;

        workspace.transition_task_with_event(
            task_id,
            1,
            TaskState::Verifying,
            TaskState::Completed,
            None,
            false,
            &TransitionGuards {
                verification_passed: true,
                ..TransitionGuards::default()
            },
            now,
            transition_event(task_id, TaskState::Verifying, TaskState::Completed, now),
        )?;
        resume_outbox_scan
            .send(())
            .map_err(|()| "task stream closed before its outbox scan resumed")?;

        let completed =
            tokio::time::timeout(Duration::from_secs(1), read_response(&mut reader)).await??;
        assert_eq!(completed.outcome["data"]["event"]["to_state"], "completed");
        assert_eq!(completed.outcome["data"]["status"]["state"], "completed");
        tokio::time::timeout(Duration::from_secs(1), server_task).await???;
        Ok(())
    }

    #[tokio::test]
    async fn task_status_stream_replays_intervening_events_after_reconnect_cursor()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = Arc::new(Database::open_in_memory()?);
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        let repository = tempfile::tempdir()?;
        let workspace_id = database
            .resolve_repository_workspace(repository.path())?
            .workspace_id;
        let workspace = database.workspace(workspace_id);
        let task_id = TaskId::new();
        let now = Utc::now();
        let envelope = TaskEnvelope::new("reconnect stream", "reconnect stream", now);
        workspace.create_task(&NewTaskRecord {
            task_id,
            schema_version: SchemaVersion::V1.to_owned(),
            state: TaskState::Running,
            objective: envelope.objective.clone(),
            original_request_redacted: envelope.original_request_redacted.clone(),
            envelope,
            created_at: now,
        })?;
        workspace.transition_task_with_event(
            task_id,
            0,
            TaskState::Running,
            TaskState::Verifying,
            None,
            false,
            &TransitionGuards::default(),
            now,
            transition_event(task_id, TaskState::Running, TaskState::Verifying, now),
        )?;
        workspace.transition_task_with_event(
            task_id,
            1,
            TaskState::Verifying,
            TaskState::Completed,
            None,
            false,
            &TransitionGuards {
                verification_passed: true,
                ..TransitionGuards::default()
            },
            now,
            transition_event(task_id, TaskState::Verifying, TaskState::Completed, now),
        )?;

        let root = std::path::PathBuf::from("unused");
        let paths = GlobalStatePaths {
            database: root.join("state.db"),
            backups: root.join("backups"),
            workspaces: root.join("workspaces"),
            runtime: root.join("runtime"),
            config: root.join("config.toml"),
            root,
        };
        let (writer, _receiver) = tokio::sync::mpsc::channel(1);
        let (client, server) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(handle_connection(server, database, paths, writer));
        let (reader, mut output) = tokio::io::split(client);
        let mut reader = BufReader::new(reader);
        let request = IpcRequest {
            schema_version: IPC_SCHEMA_VERSION,
            request_id: "task-stream-reconnect".to_owned(),
            workspace_id: Some(workspace_id),
            action: "workspace.task.stream".to_owned(),
            payload: serde_json::json!({"task_id": task_id, "cursor": 0}),
        };
        output
            .write_all(format!("{}\n", serde_json::to_string(&request)?).as_bytes())
            .await?;

        let verifying = read_response(&mut reader).await?;
        assert_eq!(verifying.outcome["status"], "ok");
        assert_eq!(verifying.outcome["data"]["cursor"], 1);
        assert_eq!(verifying.outcome["data"]["event"]["to_state"], "verifying");
        let completed = read_response(&mut reader).await?;
        assert_eq!(completed.outcome["data"]["cursor"], 2);
        assert_eq!(completed.outcome["data"]["event"]["to_state"], "completed");
        tokio::time::timeout(Duration::from_secs(1), server_task).await???;
        Ok(())
    }

    async fn read_response<R>(reader: &mut BufReader<R>) -> Result<IpcResponse, IpcError>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .await
            .map_err(|source| IpcError::Io {
                path: std::path::PathBuf::from("test IPC stream"),
                source,
            })?;
        serde_json::from_str(&response).map_err(Into::into)
    }

    fn transition_event(
        task_id: TaskId,
        from_state: TaskState,
        to_state: TaskState,
        occurred_at: chrono::DateTime<Utc>,
    ) -> TaskEvent {
        TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            session_id: None,
            task_id: Some(task_id),
            occurred_at,
            event_type: EventType::StateTransitioned,
            from_state: Some(from_state),
            to_state: Some(to_state),
            reason: None,
            actor: EventActor::Orchestrator,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: serde_json::json!({}),
            previous_hash: None,
            event_hash: String::new(),
        }
    }

    #[test]
    fn workspace_conversation_returns_latest_two_hundred_messages()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        let repository = tempfile::tempdir()?;
        let workspace_id = database
            .resolve_repository_workspace(repository.path())?
            .workspace_id;
        let session_id = SessionId::new();
        let now = Utc::now();
        let workspace = database.workspace(workspace_id);
        workspace.create_session_with_event(
            &NewSessionRecord {
                session_id,
                schema_version: SchemaVersion::V1.to_owned(),
                title: "long IPC conversation".to_owned(),
                state: SessionState::Drafting,
                created_at: now,
            },
            TaskEvent {
                schema_version: SchemaVersion::state_current(),
                sequence: 0,
                event_id: EventId::new(),
                session_id: Some(session_id),
                task_id: None,
                occurred_at: now,
                event_type: EventType::SessionCreated,
                from_state: None,
                to_state: None,
                reason: None,
                actor: EventActor::User,
                correlation_id: CorrelationId::new(),
                causation_id: None,
                payload: serde_json::json!({}),
                previous_hash: None,
                event_hash: String::new(),
            },
        )?;
        for ordinal in 1..=205 {
            workspace.append_message(&ConversationMessage {
                message_id: MessageId::new(),
                session_id,
                task_id: None,
                role: MessageRole::User,
                kind: MessageKind::UserMessage,
                state: MessageState::Final,
                content_redacted: if ordinal == 1 {
                    "first-dropped-marker".to_owned()
                } else {
                    format!("message-{ordinal}")
                },
                created_at: now,
                finalized_at: Some(now),
            })?;
        }
        let response = workspace_conversation(
            &database,
            &IpcRequest {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: "conversation-tail".to_owned(),
                workspace_id: Some(workspace_id),
                action: "workspace.conversation".to_owned(),
                payload: serde_json::json!({"session_id": session_id}),
            },
        )?;
        let messages = response["messages"]
            .as_array()
            .ok_or("messages are not an array")?;
        assert_eq!(messages.len(), 200);
        assert_eq!(messages.first().and_then(|item| item[0].as_u64()), Some(6));
        assert_eq!(messages.last().and_then(|item| item[0].as_u64()), Some(205));
        assert_eq!(
            messages
                .last()
                .and_then(|item| item[1]["content_redacted"].as_str()),
            Some("message-205")
        );
        assert!(!response.to_string().contains("first-dropped-marker"));
        Ok(())
    }

    #[test]
    fn workspace_projection_restores_the_durable_selection_when_no_task_is_explicit()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        let repository = tempfile::tempdir()?;
        let workspace_id = database
            .resolve_repository_workspace(repository.path())?
            .workspace_id;
        let workspace = database.workspace(workspace_id);
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let now = Utc::now();
        workspace.create_session_with_event(
            &NewSessionRecord {
                session_id,
                schema_version: SchemaVersion::V1.to_owned(),
                title: "durable TUI selection".to_owned(),
                state: SessionState::Drafting,
                created_at: now,
            },
            TaskEvent {
                schema_version: SchemaVersion::state_current(),
                sequence: 0,
                event_id: EventId::new(),
                session_id: Some(session_id),
                task_id: None,
                occurred_at: now,
                event_type: EventType::SessionCreated,
                from_state: None,
                to_state: None,
                reason: None,
                actor: EventActor::User,
                correlation_id: CorrelationId::new(),
                causation_id: None,
                payload: serde_json::json!({}),
                previous_hash: None,
                event_hash: String::new(),
            },
        )?;
        let envelope = TaskEnvelope::new("selected task", "selected task", now);
        workspace.create_task(&NewTaskRecord {
            task_id,
            schema_version: SchemaVersion::V1.to_owned(),
            state: TaskState::Running,
            objective: envelope.objective.clone(),
            original_request_redacted: envelope.original_request_redacted.clone(),
            envelope,
            created_at: now,
        })?;
        workspace.save_workspace_selected_task(session_id, Some(task_id), now)?;

        let response = workspace_projection(
            &database,
            &IpcRequest {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: "durable-selection".to_owned(),
                workspace_id: Some(workspace_id),
                action: "workspace.projection".to_owned(),
                payload: serde_json::to_value(WorkspaceReadRequest {
                    session_id,
                    selected_task_id: None,
                    before_ordinal: None,
                    message_limit: 10,
                    task_limit: 10,
                })?,
            },
        )?;

        assert_eq!(
            response["projection"]["inspector"]["task"]["task"]["task_id"],
            task_id.to_string()
        );
        Ok(())
    }
}
