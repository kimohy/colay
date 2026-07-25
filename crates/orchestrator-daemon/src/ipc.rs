use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    str::FromStr as _,
    sync::Arc,
};

use chrono::Utc;
use fs2::FileExt as _;
use orchestrator_domain::{ClientCommand, ClientCommandId, SessionId, TaskId};
use orchestrator_state::{
    Database, GlobalStatePaths, LegacyImporter, RepositoryStatePaths, RootConfig, StateError,
    TaskListFilter, WorkspaceId, ensure_private_directory, ensure_private_file,
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
            if lock_is_contended(&error) {
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
    pipe_name: String,
    #[cfg(windows)]
    pipe_owner_sid: String,
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
                pipe_owner_sid: orchestrator_state::current_windows_user_sid()?,
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
                &self.pipe_name,
                &self.pipe_owner_sid,
            )
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
                    let connection_paths = self.paths.clone();
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
}

#[cfg(windows)]
pub fn windows_named_pipe_security_descriptor(
    client: &tokio::net::windows::named_pipe::NamedPipeClient,
) -> std::io::Result<String> {
    orchestrator_windows_ipc::named_pipe_security_descriptor(client)
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
                Ok(request) => dispatch_request(request, &database, &paths, &writer).await,
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
    match request.action.as_str() {
        "daemon.ping" => IpcResponse::success(request.request_id, &json!({"ready": true})),
        "daemon.status" => match database.daemon_status(Utc::now()) {
            Ok(status) => IpcResponse::success(request.request_id, &json!({"status": status})),
            Err(_) => IpcResponse::failure(request.request_id, "daemon status is unavailable"),
        },
        "workspace.status" => {
            let request_id = request.request_id.clone();
            match workspace_status(database, paths, &request) {
                Ok(status) => IpcResponse::success(request_id, &status),
                Err(_) => IpcResponse::failure(request_id, "workspace status is unavailable"),
            }
        }
        "workspace.command.status" => {
            let request_id = request.request_id.clone();
            match workspace_command_status(database, &request) {
                Ok(status) => IpcResponse::success(request_id, &status),
                Err(_) => IpcResponse::failure(request_id, "workspace command is unavailable"),
            }
        }
        "workspace.conversation" => {
            let request_id = request.request_id.clone();
            match workspace_conversation(database, &request) {
                Ok(conversation) => IpcResponse::success(request_id, &conversation),
                Err(_) => IpcResponse::failure(request_id, "workspace conversation is unavailable"),
            }
        }
        "workspace.register"
        | "workspace.command.submit"
        | "workspace.run.submit"
        | "daemon.stop" => {
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
        "workspace.command.submit" => submit_workspace_command(database, &request, false),
        "workspace.run.submit" => submit_run_command(database, &request),
        "daemon.stop" => request_daemon_stop(database),
        _ => Err(IpcError::Protocol("unsupported writer action".to_owned())),
    };
    match result {
        Ok(data) => IpcResponse::success(request.request_id, &data),
        Err(_) => IpcResponse::failure(request.request_id, "daemon mutation was rejected"),
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

    use chrono::Utc;
    use orchestrator_domain::{
        ConversationMessage, CorrelationId, EventActor, EventId, EventType, MessageId, MessageKind,
        MessageRole, MessageState, SchemaVersion, SessionId, SessionState, TaskEvent,
    };
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::{
        IPC_SCHEMA_VERSION, IpcRequest, IpcResponse, MAX_REQUEST_BYTES, handle_connection,
        workspace_conversation,
    };
    use orchestrator_state::{Database, GlobalStatePaths, NewSessionRecord};

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
}
