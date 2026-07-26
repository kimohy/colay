#![cfg(feature = "test-fixtures")]

use std::{
    env,
    ffi::OsString,
    fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Barrier, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use orchestrator_state::{GlobalStatePaths, StateEnvironment};
use rusqlite::Connection;

const CLIENT_COUNT: usize = 32;

struct ConcurrencyFixture {
    _serial: MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    root: PathBuf,
    repository: PathBuf,
    colay_home: PathBuf,
    command: CommandContext,
}

#[derive(Clone)]
struct CommandContext {
    executable: PathBuf,
    path: OsString,
    colay_home: PathBuf,
    daemon_log: PathBuf,
    temp: PathBuf,
}

impl ConcurrencyFixture {
    fn new() -> Result<Self> {
        let serial = concurrency_fixture_guard();
        #[cfg(target_os = "macos")]
        let temp = tempfile::tempdir_in("/tmp")?;
        #[cfg(not(target_os = "macos"))]
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let repository = root.join("Workspace-한글-Σ");
        let colay_home = root.join("Colay-전역-상태");
        fs::create_dir_all(&repository)?;
        fs::create_dir_all(&colay_home)?;
        let fake_provider = PathBuf::from(env!("CARGO_BIN_EXE_colay-e2e-fake-provider"));
        fs::write(
            colay_home.join("config.toml"),
            format!(
                "config_version = 4\n[orchestrator.providers.codex]\nexecutable = {}\n",
                toml_path(&fake_provider)
            ),
        )?;
        let executable = PathBuf::from(env!("CARGO_BIN_EXE_colay"));
        let executable_parent = executable
            .parent()
            .context("colay binary has no parent")?
            .to_path_buf();
        let inherited_path = env::var_os("PATH").unwrap_or_default();
        let path = env::join_paths(
            std::iter::once(executable_parent).chain(env::split_paths(&inherited_path)),
        )?;
        let command = CommandContext {
            executable,
            path,
            colay_home: colay_home.clone(),
            daemon_log: root.join("daemon-stderr.log"),
            temp: root.clone(),
        };
        Ok(Self {
            _serial: serial,
            _temp: temp,
            root,
            repository,
            colay_home,
            command,
        })
    }

    fn run_parallel_status_and_plan_clients(&self, count: usize) -> Result<Vec<Output>> {
        let barrier = Arc::new(Barrier::new(count));
        let mut clients = Vec::with_capacity(count);
        for index in 0..count {
            let barrier = Arc::clone(&barrier);
            let command = self.command.clone();
            let repository = self.repository.clone();
            clients.push(thread::spawn(move || {
                barrier.wait();
                let args = if index % 2 == 0 {
                    vec![OsString::from("--json"), OsString::from("status")]
                } else {
                    vec![
                        OsString::from("run"),
                        OsString::from("--plan-only"),
                        OsString::from(format!("stress plan client {index}")),
                    ]
                };
                command.run(&repository, &args)
            }));
        }
        clients
            .into_iter()
            .map(|client| {
                client
                    .join()
                    .map_err(|_| anyhow!("concurrent client thread panicked"))?
            })
            .collect()
    }

    fn run(&self, repository: &Path, args: &[&str]) -> Result<Output> {
        let args = args.iter().map(OsString::from).collect::<Vec<_>>();
        self.command.run(repository, &args)
    }

    fn database(&self) -> PathBuf {
        self.colay_home.join("state/state.db")
    }

    fn count(&self, sql: &str) -> Result<i64> {
        Connection::open(self.database())?
            .query_row(sql, [], |row| row.get(0))
            .map_err(Into::into)
    }

    fn daemon_diagnostics(&self) -> Result<String> {
        let connection = Connection::open(self.database())?;
        let mut statement = connection.prepare(
            "SELECT pid, phase, COALESCE(startup_error, ''), \
                    COALESCE(stop_requested_at, ''), COALESCE(released_at, '') \
             FROM daemon_instances ORDER BY started_at",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(format!(
                "pid={} phase={} startup_error={:?} stop_requested_at={:?} released_at={:?}",
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?
            ))
        })?;
        let rows = rows.collect::<Result<Vec<_>, _>>()?.join("; ");
        let daemon_log = fs::read_to_string(&self.command.daemon_log).unwrap_or_default();
        Ok(format!("{rows}; daemon_stderr={daemon_log:?}"))
    }

    fn database_files(&self) -> Result<Vec<PathBuf>> {
        fn collect(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
            if !directory.exists() {
                return Ok(());
            }
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    collect(&path, files)?;
                } else if path.extension().is_some_and(|extension| extension == "db") {
                    files.push(path);
                }
            }
            Ok(())
        }

        let mut files = Vec::new();
        collect(&self.root, &mut files)?;
        files.sort();
        Ok(files)
    }

    fn stop_and_verify(&self) -> Result<()> {
        let pid = self.live_daemon_pid()?;
        let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
            self.colay_home.clone(),
        )?)?;
        let endpoint = orchestrator_daemon::ipc_endpoint(&paths);
        let stopped = self.run(&self.repository, &["daemon", "stop"])?;
        if !stopped.status.success() {
            bail!(
                "daemon stop failed with {}: {}; stdout: {}",
                stopped.status,
                String::from_utf8_lossy(&stopped.stderr),
                String::from_utf8_lossy(&stopped.stdout)
            );
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let unreleased =
                self.count("SELECT count(*) FROM daemon_instances WHERE released_at IS NULL")?;
            #[cfg(unix)]
            let endpoint_absent = endpoint_is_absent(&endpoint)?;
            #[cfg(windows)]
            let endpoint_absent = endpoint_is_absent(&endpoint);
            let process_running = process_is_running(pid)?;
            if unreleased == 0 && endpoint_absent && !process_running {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "daemon cleanup timed out: pid={pid} process_running={process_running} \
                     endpoint_absent={endpoint_absent} unreleased_instances={unreleased}"
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn live_daemon_pid(&self) -> Result<u32> {
        let pid = Connection::open(self.database())?.query_row(
            "SELECT pid FROM daemon_instances WHERE released_at IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        u32::try_from(pid).context("live daemon PID is outside the u32 range")
    }
}

#[cfg(unix)]
fn endpoint_is_absent(endpoint: &Path) -> Result<bool> {
    Ok(!endpoint.try_exists()?)
}

#[cfg(windows)]
fn endpoint_is_absent(endpoint: &Path) -> bool {
    use tokio::net::windows::named_pipe::ClientOptions;

    match ClientOptions::new().open(endpoint) {
        Ok(client) => {
            drop(client);
            false
        }
        Err(error) => matches!(error.raw_os_error(), Some(2 | 3)),
    }
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> Result<bool> {
    Ok(PathBuf::from(format!("/proc/{pid}")).try_exists()?)
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> Result<bool> {
    let system_root = env::var_os("SystemRoot").context("SystemRoot is not set")?;
    let executable = PathBuf::from(system_root).join("System32/tasklist.exe");
    let filter = format!("PID eq {pid}");
    let output = Command::new(executable)
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .output()
        .context("failed to query daemon process state")?;
    if !output.status.success() {
        bail!(
            "tasklist failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let pid = pid.to_string();
    Ok(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let mut fields = line.split(',').map(|field| field.trim_matches('"'));
        fields.next().is_some_and(|name| {
            name.eq_ignore_ascii_case("colay.exe") && fields.next() == Some(pid.as_str())
        })
    }))
}

fn concurrency_fixture_guard() -> MutexGuard<'static, ()> {
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    match TEST_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl CommandContext {
    fn run(&self, repository: &Path, args: &[OsString]) -> Result<Output> {
        #[cfg(windows)]
        let system_root = env::var_os("SystemRoot").context("SystemRoot is not set")?;
        #[cfg(not(windows))]
        let system_root = "/";
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        let status = Command::new(&self.executable)
            .args(args)
            .current_dir(repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("COLAY_TEST_DAEMON_STDERR", &self.daemon_log)
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
            .env("PATH", &self.path)
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root)
            .env("TEMP", &self.temp)
            .env("TMP", &self.temp)
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?))
            .status()
            .context("failed to invoke colay")?;
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
}

impl Drop for ConcurrencyFixture {
    fn drop(&mut self) {
        let _ = self.run(&self.repository, &["daemon", "stop"]);
    }
}

fn toml_path(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

fn assert_clean_client(index: usize, output: &Output, daemon_diagnostics: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let normalized = stderr.to_ascii_lowercase();
    assert!(
        output.status.success(),
        "client {index} failed with {}: {stderr}; stdout: {stdout}; daemon: {daemon_diagnostics}",
        output.status,
    );
    for forbidden in [
        "database is locked",
        "database is busy",
        "sqlite_busy",
        "sqlite_locked",
    ] {
        assert!(
            !normalized.contains(forbidden),
            "client {index} observed {forbidden}: {stderr}"
        );
    }
}

#[test]
fn concurrent_clients_never_observe_sqlite_busy_or_duplicate_rows() -> Result<()> {
    let fixture = ConcurrencyFixture::new()?;

    let outputs = fixture.run_parallel_status_and_plan_clients(CLIENT_COUNT)?;
    let daemon_diagnostics = fixture
        .daemon_diagnostics()
        .unwrap_or_else(|error| format!("unavailable: {error:#}"));

    assert_eq!(outputs.len(), CLIENT_COUNT);
    for (index, output) in outputs.iter().enumerate() {
        assert_clean_client(index, output, &daemon_diagnostics);
    }
    assert_eq!(fixture.database_files()?, vec![fixture.database()]);
    assert_eq!(fixture.count("SELECT count(*) FROM workspaces")?, 1);
    assert_eq!(fixture.count("SELECT count(*) FROM workspace_paths")?, 1);
    assert_eq!(
        fixture.count("SELECT count(DISTINCT comparison_key) FROM workspace_paths")?,
        1
    );
    assert_eq!(fixture.count("SELECT count(*) FROM tasks")?, 0);
    assert_eq!(
        fixture.count("SELECT count(*) FROM daemon_instances WHERE released_at IS NULL")?,
        1
    );
    fixture.stop_and_verify()?;
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_unicode_and_case_variants_share_one_global_workspace() -> Result<()> {
    let fixture = ConcurrencyFixture::new()?;
    let differently_cased = fixture.root.join("wORKSPACE-한글-σ");

    let first = fixture.run(&fixture.repository, &["--json", "status"])?;
    let second = fixture.run(&differently_cased, &["--json", "status"])?;

    let daemon_diagnostics = fixture.daemon_diagnostics()?;
    assert_clean_client(0, &first, &daemon_diagnostics);
    assert_clean_client(1, &second, &daemon_diagnostics);
    assert_eq!(fixture.count("SELECT count(*) FROM workspaces")?, 1);
    assert_eq!(fixture.count("SELECT count(*) FROM workspace_paths")?, 1);
    assert_eq!(fixture.database_files()?, vec![fixture.database()]);
    fixture.stop_and_verify()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn unix_socket_is_private_and_owned_with_the_colay_home() -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let fixture = ConcurrencyFixture::new()?;
    let output = fixture.run(&fixture.repository, &["--json", "status"])?;
    assert_clean_client(0, &output, &fixture.daemon_diagnostics()?);
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        fixture.colay_home.clone(),
    )?)?;
    let endpoint = orchestrator_daemon::ipc_endpoint(&paths);
    let socket = fs::metadata(endpoint)?;
    let home = fs::metadata(&fixture.colay_home)?;

    assert_eq!(socket.permissions().mode() & 0o777, 0o600);
    assert_eq!(socket.uid(), home.uid());
    fixture.stop_and_verify()?;
    Ok(())
}
