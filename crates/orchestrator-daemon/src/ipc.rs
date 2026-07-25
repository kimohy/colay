use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::Arc,
};

use chrono::Utc;
use fs2::FileExt as _;
use orchestrator_state::{
    Database, GlobalStatePaths, LegacyImporter, RepositoryStatePaths, RootConfig, StateError,
    WorkspaceId, ensure_private_directory, ensure_private_file,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

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
        ensure_private_directory(&paths.runtime)?;
        let lock_path = paths.runtime.join("daemon.lock");
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
        ensure_private_file(&lock_path)?;
        file.try_lock_exclusive().map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                IpcError::AlreadyOwned
            } else {
                IpcError::Io {
                    path: lock_path,
                    source: error,
                }
            }
        })?;
        Ok(Self { file })
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
    pipe_name: String,
    database: Arc<Database>,
    paths: GlobalStatePaths,
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
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                pipe_name: ipc_endpoint(paths).to_string_lossy().into_owned(),
                database,
                paths: paths.clone(),
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

    pub async fn serve(self, cancellation: CancellationToken) -> Result<(), IpcError> {
        let (writer, receiver) = mpsc::channel(64);
        let writer_database = Arc::clone(&self.database);
        let writer_paths = self.paths.clone();
        let writer_task = tokio::spawn(async move {
            writer_loop(writer_database, writer_paths, receiver).await;
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
                    connections.spawn(async move {
                        handle_connection(stream, connection_database, connection_writer).await
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
        use tokio::net::windows::named_pipe::ServerOptions;

        let mut first_instance = true;
        let mut connections = JoinSet::new();
        loop {
            let server = ServerOptions::new()
                .first_pipe_instance(first_instance)
                .reject_remote_clients(true)
                .create(&self.pipe_name)
                .map_err(|source| IpcError::Io {
                    path: PathBuf::from(&self.pipe_name),
                    source,
                })?;
            first_instance = false;
            tokio::select! {
                () = cancellation.cancelled() => break,
                connected = server.connect() => {
                    connected.map_err(|source| IpcError::Io {
                        path: PathBuf::from(&self.pipe_name),
                        source,
                    })?;
                    let connection_writer = writer.clone();
                    let connection_database = Arc::clone(&self.database);
                    connections.spawn(async move {
                        handle_connection(server, connection_database, connection_writer).await
                    });
                }
            }
            while connections.try_join_next().is_some() {}
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(())
    }
}

#[must_use]
pub fn ipc_endpoint(paths: &GlobalStatePaths) -> PathBuf {
    #[cfg(unix)]
    {
        paths.runtime.join("daemon.sock")
    }
    #[cfg(windows)]
    {
        let digest = Sha256::digest(paths.root.to_string_lossy().as_bytes());
        let suffix = hex::encode(&digest[..16]);
        PathBuf::from(format!(r"\\.\pipe\colay-{suffix}"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        paths.runtime.join("daemon.ipc")
    }
}

async fn handle_connection<S>(
    stream: S,
    database: Arc<Database>,
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
                Ok(request) => dispatch_request(request, &database, &writer).await,
                Err(_) => IpcResponse::failure(String::new(), "IPC request is not valid JSON"),
            }
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        output
            .write_all(&encoded)
            .await
            .map_err(|source| IpcError::Io {
                path: PathBuf::from("local IPC stream"),
                source,
            })?;
        if oversized {
            return Ok(());
        }
    }
}

async fn dispatch_request(
    request: IpcRequest,
    database: &Database,
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
    match request.action.as_str() {
        "daemon.ping" => IpcResponse::success(request.request_id, &json!({"ready": true})),
        "daemon.status" => match database.daemon_status(Utc::now()) {
            Ok(status) => IpcResponse::success(request.request_id, &json!({"status": status})),
            Err(_) => IpcResponse::failure(request.request_id, "daemon status is unavailable"),
        },
        "workspace.register" | "daemon.stop" => {
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
        _ => IpcResponse::failure(request.request_id, "unsupported IPC action"),
    }
}

struct WriterRequest {
    request: IpcRequest,
    response: oneshot::Sender<IpcResponse>,
}

async fn writer_loop(
    database: Arc<Database>,
    paths: GlobalStatePaths,
    mut receiver: mpsc::Receiver<WriterRequest>,
) {
    while let Some(command) = receiver.recv().await {
        let response = process_writer_request(&database, &paths, command.request);
        let _ = command.response.send(response);
    }
}

fn process_writer_request(
    database: &Database,
    paths: &GlobalStatePaths,
    request: IpcRequest,
) -> IpcResponse {
    let result = match request.action.as_str() {
        "workspace.register" => register_workspace(database, paths, &request),
        "daemon.stop" => request_daemon_stop(database),
        _ => Err(IpcError::Protocol("unsupported writer action".to_owned())),
    };
    match result {
        Ok(data) => IpcResponse::success(request.request_id, &data),
        Err(_) => IpcResponse::failure(request.request_id, "daemon mutation was rejected"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterWorkspacePayload {
    repository: PathBuf,
    state_dir: PathBuf,
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
    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::{IpcResponse, MAX_REQUEST_BYTES, handle_connection};
    use orchestrator_state::Database;

    #[tokio::test]
    async fn oversized_request_is_rejected_and_connection_is_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = Arc::new(Database::open_in_memory()?);
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        let (writer, _receiver) = tokio::sync::mpsc::channel(1);
        let (mut client, server) = tokio::io::duplex(MAX_REQUEST_BYTES.saturating_add(2));
        let server_task = tokio::spawn(handle_connection(server, database, writer));

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
}
