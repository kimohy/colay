use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, broadcast},
    task::JoinHandle,
    time,
};
use tokio_util::sync::CancellationToken;

use crate::{RedactionConfig, RedactionError, Redactor};

const DEFAULT_STDOUT_LIMIT: usize = 16 * 1024 * 1024;
const DEFAULT_STDERR_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub working_dir: Option<PathBuf>,
    pub stdin: Vec<u8>,
    /// Keeps the child's stdin pipe open for an explicitly bidirectional
    /// protocol. Ordinary commands always close stdin after the initial
    /// payload.
    pub keep_stdin_open: bool,
    /// Maximum size accepted by one live-stdin write.
    pub stdin_write_limit: usize,
    pub timeout: Duration,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
    pub environment: EnvironmentPolicy,
    pub redaction: RedactionConfig,
}

impl CommandSpec {
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            args: Vec::new(),
            working_dir: None,
            stdin: Vec::new(),
            keep_stdin_open: false,
            stdin_write_limit: 1024 * 1024,
            timeout: Duration::from_mins(30),
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
            environment: EnvironmentPolicy::default(),
            redaction: RedactionConfig::default(),
        }
    }

    #[must_use]
    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.args.push(argument.into());
        self
    }

    #[must_use]
    pub fn args(mut self, arguments: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.args.extend(arguments.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn current_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(directory.into());
        self
    }

    #[must_use]
    pub fn with_stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = stdin.into();
        self
    }

    /// Enables bounded live stdin for a bidirectional stdio protocol.
    #[must_use]
    pub const fn keep_stdin_open(mut self, write_limit: usize) -> Self {
        self.keep_stdin_open = true;
        self.stdin_write_limit = write_limit;
        self
    }
}

#[derive(Clone, Debug)]
pub struct EnvironmentPolicy {
    inherited: BTreeSet<String>,
    overrides: BTreeMap<OsString, OsString>,
}

impl EnvironmentPolicy {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            inherited: BTreeSet::new(),
            overrides: BTreeMap::new(),
        }
    }

    pub fn allow_inherit(&mut self, name: impl Into<String>) -> Result<(), ProcessError> {
        let name = name.into();
        validate_environment_name(&name)?;
        self.inherited.insert(name);
        Ok(())
    }

    pub fn set(
        &mut self,
        name: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Result<(), ProcessError> {
        let name = name.into();
        let display = name.to_string_lossy();
        validate_environment_name(&display)?;
        self.overrides.insert(name, value.into());
        Ok(())
    }

    fn apply(&self, command: &mut Command) {
        command.env_clear();
        for name in &self.inherited {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.envs(&self.overrides);
    }
}

impl Default for EnvironmentPolicy {
    fn default() -> Self {
        let inherited = [
            "PATH",
            "PATHEXT",
            "SystemRoot",
            "WINDIR",
            "ComSpec",
            "SystemDrive",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "DevEnvDir",
            "ExtensionSdkDir",
            "Framework40Version",
            "FrameworkDir",
            "FrameworkDir32",
            "FrameworkVersion",
            "FrameworkVersion32",
            "INCLUDE",
            "LIB",
            "LIBPATH",
            "NETFXSDKDir",
            "UCRTVersion",
            "UniversalCRTSdkDir",
            "VCIDEInstallDir",
            "VCINSTALLDIR",
            "VCToolsInstallDir",
            "VCToolsRedistDir",
            "VSINSTALLDIR",
            "VSCMD_ARG_app_plat",
            "VSCMD_ARG_HOST_ARCH",
            "VSCMD_ARG_TGT_ARCH",
            "VSCMD_ARG_VCVARS_SPECTRE",
            "VSCMD_VER",
            "VisualStudioVersion",
            "WindowsLibPath",
            "WindowsSdkBinPath",
            "WindowsSdkDir",
            "WindowsSdkDir_10",
            "WindowsSdkVerBinPath",
            "WindowsSDKLibVersion",
            "WindowsSDKVersion",
            "__DOTNET_ADD_64BIT",
            "__DOTNET_PREFERRED_BITNESS",
            "__VSCMD_PREINIT_PATH",
            "HOME",
            "USERPROFILE",
            "TMP",
            "TEMP",
            "LANG",
            "LC_ALL",
            "TERM",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "NO_PROXY",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "GEMINI_CLI_HOME",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        Self {
            inherited,
            overrides: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    Exited,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct CapturedOutput {
    /// Bounded raw bytes for in-memory protocol parsing. Persist only `redacted_text`.
    pub bytes: Vec<u8>,
    pub redacted_text: String,
    pub bytes_seen: u64,
    pub truncated: bool,
    pub invalid_utf8: bool,
    pub(crate) redactor: Redactor,
}

impl CapturedOutput {
    #[must_use]
    pub fn for_test(bytes: Vec<u8>, redactor: Redactor) -> Self {
        let bytes_seen = bytes.len() as u64;
        let invalid_utf8 = std::str::from_utf8(&bytes).is_err();
        let redacted_text = redactor.redact(&String::from_utf8_lossy(&bytes));
        Self {
            bytes,
            redacted_text,
            bytes_seen,
            truncated: false,
            invalid_utf8,
            redactor,
        }
    }

    /// Explicitly removes transient unredacted bytes once structured parsing is complete.
    pub fn clear_sensitive_bytes(&mut self) {
        self.bytes.fill(0);
        self.bytes.clear();
    }
}

#[derive(Clone, Debug)]
pub struct ProcessResult {
    pub exit_code: Option<i32>,
    pub termination: TerminationReason,
    /// Set when the direct child was killed and reaped, but the operating
    /// system's process-tree termination mechanism could not be confirmed.
    /// Callers must not assume descendants were stopped when this is present.
    pub tree_termination_error: Option<String>,
    pub stdout: CapturedOutput,
    pub stderr: CapturedOutput,
    pub elapsed: Duration,
}

impl ProcessResult {
    #[must_use]
    pub fn success(&self) -> bool {
        self.termination == TerminationReason::Exited && self.exit_code == Some(0)
    }
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("invalid command specification: {0}")]
    InvalidSpec(String),
    #[error("invalid environment variable `{0}`")]
    InvalidEnvironment(String),
    #[error(transparent)]
    Redaction(#[from] RedactionError),
    #[error("failed to spawn `{executable}`: {source}")]
    Spawn {
        executable: String,
        #[source]
        source: io::Error,
    },
    #[error("subprocess I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("subprocess I/O task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("subprocess stdin is closed")]
    StdinClosed,
    #[error("subprocess stdin write of {actual} bytes exceeds the {limit} byte limit")]
    StdinWriteTooLarge { actual: usize, limit: usize },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessRunner;

impl ProcessRunner {
    pub async fn run(
        &self,
        spec: CommandSpec,
        cancellation: CancellationToken,
    ) -> Result<ProcessResult, ProcessError> {
        let session = ProcessSupervisor.start(spec).await?;
        let session_cancellation = session.cancellation_token();
        let completion = session.wait();
        tokio::pin!(completion);
        tokio::select! {
            result = &mut completion => result,
            () = cancellation.cancelled() => {
                session_cancellation.cancel();
                completion.await
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputChannel {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug)]
pub enum ProcessEvent {
    Started {
        process_id: Option<u32>,
    },
    Output {
        sequence: u64,
        channel: OutputChannel,
        /// Transient protocol bytes. These must not be persisted without redaction.
        bytes: Vec<u8>,
        redacted_text: String,
        invalid_utf8: bool,
    },
    FramesDropped {
        count: u64,
    },
    Exited {
        exit_code: Option<i32>,
        termination: TerminationReason,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessSupervisor;

pub struct ProcessSession {
    process_id: Option<u32>,
    cancellation: CancellationToken,
    input: Option<ProcessInput>,
    events: broadcast::Receiver<ProcessEvent>,
    completion: Option<JoinHandle<Result<ProcessResult, ProcessError>>>,
}

/// Cloneable, bounded writer for the small set of protocols that require
/// bidirectional stdio. It is unavailable unless the command explicitly opted
/// into live stdin.
#[derive(Clone)]
pub struct ProcessInput {
    writer: Arc<Mutex<Option<ChildStdin>>>,
    write_limit: usize,
}

impl ProcessInput {
    /// Writes one bounded protocol frame without closing stdin.
    pub async fn write_all(&self, bytes: &[u8]) -> Result<(), ProcessError> {
        if bytes.len() > self.write_limit {
            return Err(ProcessError::StdinWriteTooLarge {
                actual: bytes.len(),
                limit: self.write_limit,
            });
        }
        let mut guard = self.writer.lock().await;
        let writer = guard.as_mut().ok_or(ProcessError::StdinClosed)?;
        writer.write_all(bytes).await?;
        writer.flush().await?;
        Ok(())
    }

    /// Closes the live stdin pipe. Calling it more than once is harmless.
    pub async fn close(&self) -> Result<(), ProcessError> {
        let mut writer = self.writer.lock().await.take();
        if let Some(writer) = writer.as_mut() {
            writer.shutdown().await?;
        }
        Ok(())
    }
}

impl ProcessSession {
    #[must_use]
    pub const fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Returns a live stdin handle only for an explicitly opted-in command.
    #[must_use]
    pub fn input(&self) -> Option<ProcessInput> {
        self.input.clone()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn next_event(&mut self) -> Option<ProcessEvent> {
        match self.events.recv().await {
            Ok(event) => Some(event),
            Err(broadcast::error::RecvError::Lagged(count)) => {
                Some(ProcessEvent::FramesDropped { count })
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    pub async fn wait(mut self) -> Result<ProcessResult, ProcessError> {
        let completion = self.completion.take().ok_or_else(|| {
            ProcessError::InvalidSpec("process session was already consumed".to_owned())
        })?;
        completion.await?
    }
}

impl Drop for ProcessSession {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl ProcessSupervisor {
    #[allow(clippy::unused_async)]
    pub async fn start(&self, spec: CommandSpec) -> Result<ProcessSession, ProcessError> {
        validate_spec(&spec)?;
        let redactor = Redactor::new(&spec.redaction)?;
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(directory) = &spec.working_dir {
            command.current_dir(directory);
        }
        spec.environment.apply(&mut command);
        configure_process_group(&mut command);

        let display = spec.executable.to_string_lossy().into_owned();
        let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
            executable: display,
            source,
        })?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| {
            ProcessError::InvalidSpec("spawned process has no stdout pipe".to_owned())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ProcessError::InvalidSpec("spawned process has no stderr pipe".to_owned())
        })?;

        let (event_sender, events) = broadcast::channel(64);
        let _ = event_sender.send(ProcessEvent::Started { process_id: pid });
        let sequence = Arc::new(AtomicU64::new(1));
        let cancellation = CancellationToken::new();

        let shared_input = Arc::new(Mutex::new(stdin));
        let live_input = spec.keep_stdin_open.then(|| ProcessInput {
            writer: Arc::clone(&shared_input),
            write_limit: spec.stdin_write_limit,
        });
        let input_writer = Arc::clone(&shared_input);
        let input = spec.stdin;
        let keep_stdin_open = spec.keep_stdin_open;
        let input_task = tokio::spawn(async move {
            let mut guard = input_writer.lock().await;
            if let Some(writer) = guard.as_mut() {
                if !input.is_empty() {
                    writer.write_all(&input).await?;
                    writer.flush().await?;
                }
                if !keep_stdin_open {
                    guard.take();
                }
            } else if !input.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "subprocess stdin closed before the prompt was delivered",
                ));
            }
            Ok::<(), io::Error>(())
        });
        let stdout_task = tokio::spawn(capture(
            stdout,
            spec.stdout_limit,
            redactor.clone(),
            OutputChannel::Stdout,
            event_sender.clone(),
            Arc::clone(&sequence),
        ));
        let stderr_task = tokio::spawn(capture(
            stderr,
            spec.stderr_limit,
            redactor,
            OutputChannel::Stderr,
            event_sender.clone(),
            sequence,
        ));
        let session_cancellation = cancellation.clone();
        let completion = tokio::spawn(monitor(
            child,
            pid,
            spec.timeout,
            session_cancellation,
            input_task,
            stdout_task,
            stderr_task,
            event_sender,
        ));
        Ok(ProcessSession {
            process_id: pid,
            cancellation,
            input: live_input,
            events,
            completion: Some(completion),
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn monitor(
    mut child: Child,
    pid: Option<u32>,
    timeout: Duration,
    cancellation: CancellationToken,
    input_task: JoinHandle<io::Result<()>>,
    stdout_task: JoinHandle<io::Result<CapturedOutput>>,
    stderr_task: JoinHandle<io::Result<CapturedOutput>>,
    event_sender: broadcast::Sender<ProcessEvent>,
) -> Result<ProcessResult, ProcessError> {
    let started = Instant::now();
    let mut deadline = Box::pin(time::sleep(timeout));
    let (termination, exit_status, tree_termination_error) = tokio::select! {
        status = child.wait() => (TerminationReason::Exited, status?, None),
        () = cancellation.cancelled() => {
            let (status, tree_error) = terminate_tree(&mut child, pid).await?;
            (TerminationReason::Cancelled, status, tree_error)
        }
        () = &mut deadline => {
            let (status, tree_error) = terminate_tree(&mut child, pid).await?;
            (TerminationReason::TimedOut, status, tree_error)
        }
    };
    let input_result = input_task.await?;
    let stdout = join_capture(stdout_task).await?;
    let stderr = join_capture(stderr_task).await?;
    if let Err(error) = input_result
        && (termination == TerminationReason::Exited || error.kind() != io::ErrorKind::BrokenPipe)
    {
        return Err(error.into());
    }
    let result = ProcessResult {
        exit_code: exit_status.code(),
        termination,
        tree_termination_error,
        stdout,
        stderr,
        elapsed: started.elapsed(),
    };
    let _ = event_sender.send(ProcessEvent::Exited {
        exit_code: result.exit_code,
        termination,
    });
    Ok(result)
}

async fn capture<R>(
    mut reader: R,
    limit: usize,
    redactor: Redactor,
    channel: OutputChannel,
    event_sender: broadcast::Sender<ProcessEvent>,
    sequence: Arc<AtomicU64>,
) -> io::Result<CapturedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut bytes_seen = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    let mut pending_frame = Vec::new();
    let mut private_key_active = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        bytes_seen = bytes_seen.saturating_add(read as u64);
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        pending_frame.extend_from_slice(&buffer[..read]);
        emit_complete_frames(
            &mut pending_frame,
            false,
            channel,
            &redactor,
            &event_sender,
            &sequence,
            &mut private_key_active,
        );
    }
    emit_complete_frames(
        &mut pending_frame,
        true,
        channel,
        &redactor,
        &event_sender,
        &sequence,
        &mut private_key_active,
    );
    let invalid_utf8 = std::str::from_utf8(&bytes).is_err();
    let redacted_text = redactor.redact(&String::from_utf8_lossy(&bytes));
    Ok(CapturedOutput {
        truncated: bytes_seen > bytes.len() as u64,
        bytes,
        redacted_text,
        bytes_seen,
        invalid_utf8,
        redactor,
    })
}

fn emit_complete_frames(
    pending: &mut Vec<u8>,
    end_of_stream: bool,
    channel: OutputChannel,
    redactor: &Redactor,
    sender: &broadcast::Sender<ProcessEvent>,
    sequence: &AtomicU64,
    private_key_active: &mut bool,
) {
    const MAX_FRAME_BYTES: usize = 1024 * 1024;
    loop {
        let newline = pending.iter().position(|byte| *byte == b'\n');
        let frame_length = newline
            .map(|position| position + 1)
            .or_else(|| (pending.len() >= MAX_FRAME_BYTES).then_some(MAX_FRAME_BYTES));
        let Some(frame_length) = frame_length else {
            break;
        };
        let frame = pending.drain(..frame_length).collect::<Vec<_>>();
        send_frame(
            frame,
            newline.is_none(),
            channel,
            redactor,
            sender,
            sequence,
            private_key_active,
        );
    }
    if end_of_stream && !pending.is_empty() {
        let frame = std::mem::take(pending);
        send_frame(
            frame,
            false,
            channel,
            redactor,
            sender,
            sequence,
            private_key_active,
        );
    }
}

fn send_frame(
    bytes: Vec<u8>,
    overlong: bool,
    channel: OutputChannel,
    redactor: &Redactor,
    sender: &broadcast::Sender<ProcessEvent>,
    sequence: &AtomicU64,
    private_key_active: &mut bool,
) {
    let invalid_utf8 = std::str::from_utf8(&bytes).is_err();
    let lossy = String::from_utf8_lossy(&bytes);
    let begins_private_key = lossy.contains("-----BEGIN") && lossy.contains("PRIVATE KEY-----");
    let ends_private_key = lossy.contains("-----END") && lossy.contains("PRIVATE KEY-----");
    let redact_entire_frame = overlong || *private_key_active || begins_private_key;
    if begins_private_key {
        *private_key_active = true;
    }
    let redacted_text = if redact_entire_frame {
        "[REDACTED STREAM FRAME]".to_owned()
    } else {
        redactor.redact(&lossy)
    };
    if ends_private_key {
        *private_key_active = false;
    }
    let _ = sender.send(ProcessEvent::Output {
        sequence: sequence.fetch_add(1, Ordering::Relaxed),
        channel,
        bytes,
        redacted_text,
        invalid_utf8,
    });
}

async fn join_capture(
    task: JoinHandle<io::Result<CapturedOutput>>,
) -> Result<CapturedOutput, ProcessError> {
    task.await?.map_err(ProcessError::from)
}

async fn terminate_tree(
    child: &mut Child,
    pid: Option<u32>,
) -> Result<(std::process::ExitStatus, Option<String>), io::Error> {
    let platform_result = terminate_platform_tree(pid).await;
    let _ = child.start_kill();
    let status = time::timeout(Duration::from_secs(5), child.wait())
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out while reaping a terminated subprocess",
            )
        })??;
    let tree_termination_error = platform_result.err().map(|error| error.to_string());
    Ok((status, tree_termination_error))
}

#[cfg(windows)]
async fn terminate_platform_tree(pid: Option<u32>) -> io::Result<()> {
    let Some(pid) = pid else {
        return Ok(());
    };
    let pid_string = pid.to_string();
    let executable = trusted_taskkill_path()?;
    let status = time::timeout(
        Duration::from_secs(5),
        Command::new(executable)
            .args(["/PID", &pid_string, "/T", "/F"])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out while terminating the Windows process tree",
        )
    })??;
    if !status.success() {
        return Err(io::Error::other(format!(
            "System32 taskkill failed with status {status}"
        )));
    }
    Ok(())
}

#[cfg(unix)]
async fn terminate_platform_tree(pid: Option<u32>) -> io::Result<()> {
    let Some(pid) = pid else {
        return Ok(());
    };
    let group = format!("-{pid}");
    let kill = if std::path::Path::new("/bin/kill").is_file() {
        "/bin/kill"
    } else {
        "/usr/bin/kill"
    };
    let _ = Command::new(kill)
        .args(["-TERM", "--", &group])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await;
    time::sleep(Duration::from_millis(250)).await;
    let _ = Command::new(kill)
        .args(["-KILL", "--", &group])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
async fn terminate_platform_tree(_pid: Option<u32>) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn trusted_taskkill_path() -> io::Result<PathBuf> {
    const KERNEL_TASKKILL_PATH: &str = r"\\?\GLOBALROOT\SystemRoot\System32\taskkill.exe";

    let kernel_taskkill = std::path::Path::new(KERNEL_TASKKILL_PATH);
    let system32_path = kernel_taskkill.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "kernel taskkill path has no System32 parent",
        )
    })?;
    reject_windows_reparse_point(system32_path, "System32")?;
    if system32_path
        .file_name()
        .is_none_or(|name| !name.to_string_lossy().eq_ignore_ascii_case("System32"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kernel taskkill path is not inside System32",
        ));
    }

    reject_windows_reparse_point(kernel_taskkill, "kernel System32 taskkill.exe")?;
    let taskkill = std::fs::canonicalize(kernel_taskkill)?;
    if !taskkill.is_file()
        || taskkill
            .file_name()
            .is_none_or(|name| !name.to_string_lossy().eq_ignore_ascii_case("taskkill.exe"))
        || taskkill
            .parent()
            .and_then(std::path::Path::file_name)
            .is_none_or(|name| !name.to_string_lossy().eq_ignore_ascii_case("System32"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kernel taskkill path escaped System32 during canonicalization",
        ));
    }
    Ok(taskkill)
}

#[cfg(windows)]
fn reject_windows_reparse_point(path: &std::path::Path, label: &str) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} must not be a reparse point"),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

fn validate_spec(spec: &CommandSpec) -> Result<(), ProcessError> {
    if spec.executable.as_os_str().is_empty() {
        return Err(ProcessError::InvalidSpec(
            "executable must not be empty".to_owned(),
        ));
    }
    if spec.timeout.is_zero() {
        return Err(ProcessError::InvalidSpec(
            "timeout must be greater than zero".to_owned(),
        ));
    }
    if spec.stdout_limit == 0 || spec.stderr_limit == 0 {
        return Err(ProcessError::InvalidSpec(
            "output limits must be greater than zero".to_owned(),
        ));
    }
    if spec.keep_stdin_open && spec.stdin_write_limit == 0 {
        return Err(ProcessError::InvalidSpec(
            "live stdin write limit must be greater than zero".to_owned(),
        ));
    }
    Ok(())
}

fn validate_environment_name(name: &str) -> Result<(), ProcessError> {
    let upper = name.to_ascii_uppercase();
    let forbidden = [
        "TOKEN",
        "API_KEY",
        "SECRET",
        "PASSWORD",
        "CREDENTIAL",
        "AUTH",
    ];
    if name.is_empty()
        || name.contains(['=', '\0'])
        || forbidden.iter().any(|needle| upper.contains(needle))
    {
        return Err(ProcessError::InvalidEnvironment(name.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead as _, Write as _},
        time::Duration,
    };

    use tokio_util::sync::CancellationToken;

    use super::{
        CommandSpec, EnvironmentPolicy, ProcessEvent, ProcessRunner, ProcessSupervisor,
        TerminationReason,
    };
    #[cfg(windows)]
    use super::{terminate_platform_tree, trusted_taskkill_path};

    fn fixture(mode: &str) -> CommandSpec {
        let executable = std::env::current_exe()
            .unwrap_or_else(|error| panic!("current test executable: {error}"));
        let mut spec = CommandSpec::new(executable).args([
            "--exact",
            "runner::tests::fixture_child",
            "--nocapture",
        ]);
        spec.environment
            .set("ORCHESTRATOR_PROCESS_FIXTURE", mode)
            .unwrap_or_else(|error| panic!("fixture environment: {error}"));
        spec
    }

    #[test]
    fn fixture_child() {
        let Ok(mode) = std::env::var("ORCHESTRATOR_PROCESS_FIXTURE") else {
            return;
        };
        match mode.as_str() {
            "output" => {
                let _ = std::io::stdout().write_all(b"api_key=supersecret");
                let _ = std::io::stderr().write_all(b"err");
            }
            "large" => {
                let _ = std::io::stdout().write_all(&vec![b'x'; 10_000]);
            }
            "sleep" => std::thread::sleep(Duration::from_secs(10)),
            "interactive" => {
                for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                    let _ = writeln!(std::io::stdout(), "reply:{line}");
                }
            }
            _ => {}
        }
    }

    #[tokio::test]
    async fn captures_streams_and_redacts_persistable_text() {
        let spec = fixture("output");
        let result = ProcessRunner
            .run(spec, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("process: {error}"));
        assert!(result.success());
        assert!(!result.stdout.redacted_text.contains("supersecret"));
        assert!(result.stderr.redacted_text.contains("err"));
    }

    #[tokio::test]
    async fn output_is_bounded_while_pipe_is_fully_drained() {
        let mut spec = fixture("large");
        spec.stdout_limit = 128;
        let result = ProcessRunner
            .run(spec, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("process: {error}"));
        assert_eq!(result.stdout.bytes.len(), 128);
        assert!(result.stdout.truncated);
        assert!(result.stdout.bytes_seen >= 10_000);
    }

    #[tokio::test]
    async fn timeout_terminates_process() {
        let mut spec = fixture("sleep");
        spec.timeout = Duration::from_millis(100);
        let result = ProcessRunner
            .run(spec, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("process: {error}"));
        assert_eq!(result.termination, TerminationReason::TimedOut);
    }

    #[tokio::test]
    async fn cancellation_terminates_process() {
        let spec = fixture("sleep");
        let token = CancellationToken::new();
        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let result = ProcessRunner
            .run(spec, token)
            .await
            .unwrap_or_else(|error| panic!("process: {error}"));
        assert_eq!(result.termination, TerminationReason::Cancelled);
    }

    #[tokio::test]
    async fn cancellation_ignores_stdin_broken_pipe_from_terminated_child() {
        let mut spec = fixture("sleep");
        spec.stdin = vec![b'x'; 16 * 1024 * 1024];
        let token = CancellationToken::new();
        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });

        let result = ProcessRunner
            .run(spec, token)
            .await
            .unwrap_or_else(|error| panic!("process: {error}"));

        assert_eq!(result.termination, TerminationReason::Cancelled);
    }

    #[tokio::test]
    async fn supervisor_streams_redacted_frames_and_waits() {
        let mut session = ProcessSupervisor
            .start(fixture("output"))
            .await
            .unwrap_or_else(|error| panic!("start: {error}"));
        let mut observed_safe_output = false;
        while let Some(event) = session.next_event().await {
            match event {
                ProcessEvent::Output { redacted_text, .. } => {
                    assert!(!redacted_text.contains("supersecret"));
                    if redacted_text.contains("[REDACTED]") {
                        observed_safe_output = true;
                    }
                }
                ProcessEvent::Exited { .. } => break,
                ProcessEvent::Started { .. } | ProcessEvent::FramesDropped { .. } => {}
            }
        }
        let result = session
            .wait()
            .await
            .unwrap_or_else(|error| panic!("wait: {error}"));
        assert!(result.success());
        assert!(observed_safe_output);
    }

    #[tokio::test]
    async fn live_stdin_is_opt_in_and_bounded() {
        let spec = fixture("interactive").keep_stdin_open(16);
        let mut session = ProcessSupervisor
            .start(spec)
            .await
            .unwrap_or_else(|error| panic!("start: {error}"));
        let input = session
            .input()
            .unwrap_or_else(|| panic!("live input was not exposed"));
        assert!(input.write_all(b"0123456789abcdefg\n").await.is_err());
        input
            .write_all(b"hello\n")
            .await
            .unwrap_or_else(|error| panic!("write: {error}"));
        input
            .close()
            .await
            .unwrap_or_else(|error| panic!("close: {error}"));

        let mut reply = false;
        while let Some(event) = session.next_event().await {
            match event {
                ProcessEvent::Output { redacted_text, .. } => {
                    reply |= redacted_text.contains("reply:hello");
                }
                ProcessEvent::Exited { .. } => break,
                ProcessEvent::Started { .. } | ProcessEvent::FramesDropped { .. } => {}
            }
        }
        let result = session
            .wait()
            .await
            .unwrap_or_else(|error| panic!("wait: {error}"));
        assert!(result.success());
        assert!(reply);
    }

    #[test]
    fn environment_policy_rejects_credential_variables() {
        let mut environment = EnvironmentPolicy::empty();
        assert!(environment.set("OPENAI_API_KEY", "secret").is_err());
        assert!(environment.allow_inherit("ACCESS_TOKEN").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn default_environment_preserves_msvc_tool_discovery() {
        let environment = EnvironmentPolicy::default();
        for name in [
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "INCLUDE",
            "LIB",
            "LIBPATH",
            "UniversalCRTSdkDir",
            "UCRTVersion",
            "VCINSTALLDIR",
            "VCToolsInstallDir",
            "VCToolsRedistDir",
            "VSINSTALLDIR",
            "VSCMD_ARG_HOST_ARCH",
            "VSCMD_ARG_TGT_ARCH",
            "VSCMD_VER",
            "VisualStudioVersion",
            "WindowsLibPath",
            "WindowsSdkBinPath",
            "WindowsSdkDir",
            "WindowsSdkVerBinPath",
            "WindowsSDKLibVersion",
            "WindowsSDKVersion",
        ] {
            assert!(environment.inherited.contains(name), "missing {name}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn trusted_taskkill_is_canonical_system32_binary() {
        let taskkill = trusted_taskkill_path()
            .unwrap_or_else(|error| panic!("trusted taskkill path: {error}"));
        assert!(taskkill.is_absolute());
        assert!(taskkill.is_file());
        assert!(
            taskkill.file_name().is_some_and(|name| {
                name.to_string_lossy().eq_ignore_ascii_case("taskkill.exe")
            })
        );

        assert!(
            taskkill
                .parent()
                .and_then(std::path::Path::file_name)
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("System32"))
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_tree_kill_rejects_nonexistent_process_status() {
        match terminate_platform_tree(Some(u32::MAX)).await {
            Err(error) => assert!(
                error.kind() == std::io::ErrorKind::Other
                    || error.kind() == std::io::ErrorKind::PermissionDenied
            ),
            Ok(()) => panic!("an impossible process id reported successful tree termination"),
        }
    }
}
