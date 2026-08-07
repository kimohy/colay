#![cfg(feature = "test-fixtures")]

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Barrier, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::Utc;
use orchestrator_state::{GlobalStatePaths, LegacyImportResult, StateEnvironment};
use rusqlite::{Connection, params};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

const CLIENT_COUNT: usize = 32;
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
    daemon_log_directory: PathBuf,
    next_daemon_log: Arc<AtomicU64>,
    temp: PathBuf,
}

#[derive(Debug)]
struct DaemonDiagnostic {
    pid: i64,
    phase: String,
    startup_error: String,
    stop_requested_at: String,
    released_at: String,
}

#[derive(Debug)]
struct DaemonDiagnostics {
    instances: Vec<DaemonDiagnostic>,
    stderr: String,
}

struct LegacyWorkspaceSeed {
    repository: PathBuf,
    database: PathBuf,
    session_id: String,
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
        let daemon_log_directory = root.join("daemon-stderr");
        fs::create_dir_all(&daemon_log_directory)?;
        let command = CommandContext {
            executable,
            path,
            colay_home: colay_home.clone(),
            daemon_log_directory,
            next_daemon_log: Arc::new(AtomicU64::new(1)),
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

    fn run_parallel_status_clients(&self, repositories: &[PathBuf]) -> Result<Vec<Output>> {
        if repositories.is_empty() {
            bail!("at least one repository is required");
        }
        let spawn_barrier = self.root.join("daemon-spawn-barrier");
        fs::create_dir(&spawn_barrier)?;
        let barrier = Arc::new(Barrier::new(repositories.len()));
        let mut clients = Vec::with_capacity(repositories.len());
        for repository in repositories {
            let barrier = Arc::clone(&barrier);
            let command = self.command.clone();
            let repository = repository.clone();
            let spawn_barrier = spawn_barrier.clone();
            let expected = repositories.len();
            clients.push(thread::spawn(move || {
                barrier.wait();
                command.run_with_spawn_barrier(
                    &repository,
                    &[OsString::from("--json"), OsString::from("status")],
                    &spawn_barrier,
                    expected,
                )
            }));
        }
        clients
            .into_iter()
            .map(|client| {
                client
                    .join()
                    .map_err(|_| anyhow!("concurrent legacy client thread panicked"))?
            })
            .collect()
    }

    fn seed_distinct_legacy_workspaces(&self, count: usize) -> Result<Vec<LegacyWorkspaceSeed>> {
        let mut seeds = Vec::with_capacity(count);
        for index in 0..count {
            let repository = self.root.join(format!("legacy-workspace-{index}"));
            let state = repository.join(".colay");
            fs::create_dir_all(&state)?;
            fs::write(state.join("config.toml"), "config_version = 4\n")?;
            let database = state.join("orchestrator.db");
            let connection = Connection::open(&database)?;
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
            let session_id = format!("01987d4e-2a54-7000-8000-{index:012}");
            connection.execute(
                "INSERT INTO sessions(\
                     session_id, schema_version, revision, title, state, created_at, updated_at\
                 ) VALUES (?1, '1.0', 0, ?2, 'planning', ?3, ?3)",
                params![
                    session_id,
                    format!("legacy contender {index}"),
                    "2026-08-02T00:00:00Z",
                ],
            )?;
            drop(connection);
            seeds.push(LegacyWorkspaceSeed {
                repository,
                database,
                session_id,
            });
        }
        Ok(seeds)
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

    fn daemon_diagnostics(&self) -> Result<DaemonDiagnostics> {
        let connection = Connection::open(self.database())?;
        let mut statement = connection.prepare(
            "SELECT pid, phase, COALESCE(startup_error, ''), \
                    COALESCE(stop_requested_at, ''), COALESCE(released_at, '') \
             FROM daemon_instances ORDER BY started_at",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(DaemonDiagnostic {
                pid: row.get(0)?,
                phase: row.get(1)?,
                startup_error: row.get(2)?,
                stop_requested_at: row.get(3)?,
                released_at: row.get(4)?,
            })
        })?;
        let instances = rows.collect::<Result<Vec<_>, _>>()?;
        let mut daemon_logs = fs::read_dir(&self.command.daemon_log_directory)
            .with_context(|| {
                format!(
                    "failed to read daemon stderr directory {}",
                    self.command.daemon_log_directory.display()
                )
            })?
            .collect::<Result<Vec<_>, _>>()?;
        daemon_logs.sort_by_key(std::fs::DirEntry::file_name);
        let mut stderr_records = Vec::new();
        for entry in daemon_logs {
            if !entry.file_type()?.is_file()
                || !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("daemon-stderr-")
            {
                continue;
            }
            let path = entry.path();
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("failed to read daemon stderr log {}", path.display()))?;
            stderr_records.extend(contents.lines().map(str::to_owned));
        }
        let stderr = stderr_records.join("\n");
        Ok(DaemonDiagnostics { instances, stderr })
    }

    fn verify_spawned_contenders_resolved(&self, expected: Option<usize>) -> Result<()> {
        let mut stderr_logs = Vec::new();
        let mut resolutions = Vec::new();
        for entry in fs::read_dir(&self.command.daemon_log_directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name
                .strip_prefix("daemon-stderr-")
                .and_then(|name| name.strip_suffix(".log"))
            {
                stderr_logs.push(id.to_owned());
            } else if let Some(id) = name
                .strip_prefix("daemon-child-resolution-")
                .and_then(|name| name.strip_suffix(".log"))
            {
                resolutions.push((id.to_owned(), fs::read_to_string(entry.path())?));
            }
        }
        stderr_logs.sort();
        resolutions.sort_by(|left, right| left.0.cmp(&right.0));
        let resolution_ids = resolutions
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if stderr_logs.is_empty() || stderr_logs != resolution_ids {
            bail!(
                "daemon contenders were not all resolved before diagnostics: stderr={stderr_logs:?} resolutions={resolution_ids:?}"
            );
        }
        let owner_count = resolutions
            .iter()
            .filter(|(_, resolution)| resolution.starts_with("owner:"))
            .count();
        if owner_count != 1 {
            bail!("expected one spawned live owner resolution, found {owner_count}");
        }
        let reaped_count = resolutions
            .iter()
            .filter(|(_, resolution)| resolution.starts_with("reaped:"))
            .count();
        if let Some(expected) = expected {
            if resolutions.len() != expected {
                bail!(
                    "expected exactly {expected} spawned contender resolutions, found {}",
                    resolutions.len()
                );
            }
            if reaped_count != expected.saturating_sub(1) {
                bail!(
                    "expected {} reaped contender resolutions, found {reaped_count}",
                    expected.saturating_sub(1)
                );
            }
        }
        for (id, resolution) in resolutions {
            let valid = resolution
                .strip_prefix("owner:")
                .or_else(|| resolution.strip_prefix("reaped:"))
                .is_some_and(|pid| pid.parse::<u32>().is_ok());
            if !valid {
                bail!("invalid daemon child resolution for {id}: {resolution:?}");
            }
        }
        Ok(())
    }

    fn verify_spawn_barrier_arrivals(&self, expected: usize) -> Result<()> {
        let barrier = self.root.join("daemon-spawn-barrier");
        let entries = fs::read_dir(&barrier)?.collect::<std::io::Result<Vec<_>>>()?;
        let mut arrivals = 0;
        for entry in entries {
            if entry.file_type()?.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "ready")
            {
                arrivals += 1;
            }
        }
        if arrivals != expected {
            bail!("expected {expected} pre-spawn barrier arrivals, found {arrivals}");
        }
        Ok(())
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
        let endpoints = orchestrator_daemon::ipc_endpoint_candidates(&paths)?
            .server_endpoints()
            .into_iter()
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
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
            let endpoint_absent = endpoints
                .iter()
                .map(|endpoint| endpoint_is_absent(endpoint))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .all(|absent| absent);
            #[cfg(windows)]
            let endpoint_absent = endpoints
                .iter()
                .all(|endpoint| endpoint_is_absent(endpoint));
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
        self.run_with_colay_home(repository, args, &self.colay_home)
    }

    fn run_with_colay_home(
        &self,
        repository: &Path,
        args: &[OsString],
        colay_home: &Path,
    ) -> Result<Output> {
        #[cfg(windows)]
        let system_root = env::var_os("SystemRoot").context("SystemRoot is not set")?;
        #[cfg(not(windows))]
        let system_root = "/";
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        let command_id = format!(
            "{}-{}",
            std::process::id(),
            self.next_daemon_log.fetch_add(1, Ordering::Relaxed)
        );
        let daemon_log = self
            .daemon_log_directory
            .join(format!("daemon-stderr-{command_id}.log"));
        let daemon_child_resolution = self
            .daemon_log_directory
            .join(format!("daemon-child-resolution-{command_id}.log"));
        let mut command = Command::new(&self.executable);
        command
            .args(args)
            .current_dir(repository)
            .env_clear()
            .env("COLAY_HOME", colay_home)
            .env("COLAY_TEST_DAEMON_STDERR", daemon_log)
            .env(
                "COLAY_TEST_DAEMON_CHILD_RESOLUTION",
                daemon_child_resolution,
            )
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
            .env("PATH", &self.path)
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root)
            .env("TEMP", &self.temp)
            .env("TMP", &self.temp)
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?));
        let status = command.status().context("failed to invoke colay")?;
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

    fn run_with_spawn_barrier(
        &self,
        repository: &Path,
        args: &[OsString],
        barrier: &Path,
        expected: usize,
    ) -> Result<Output> {
        #[cfg(windows)]
        let system_root = env::var_os("SystemRoot").context("SystemRoot is not set")?;
        #[cfg(not(windows))]
        let system_root = "/";
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        let command_id = format!(
            "{}-{}",
            std::process::id(),
            self.next_daemon_log.fetch_add(1, Ordering::Relaxed)
        );
        let daemon_log = self
            .daemon_log_directory
            .join(format!("daemon-stderr-{command_id}.log"));
        let daemon_child_resolution = self
            .daemon_log_directory
            .join(format!("daemon-child-resolution-{command_id}.log"));
        let status = Command::new(&self.executable)
            .args(args)
            .current_dir(repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("COLAY_TEST_DAEMON_STDERR", daemon_log)
            .env(
                "COLAY_TEST_DAEMON_CHILD_RESOLUTION",
                daemon_child_resolution,
            )
            .env("COLAY_TEST_DAEMON_SPAWN_BARRIER", barrier)
            .env(
                "COLAY_TEST_DAEMON_SPAWN_BARRIER_COUNT",
                expected.to_string(),
            )
            // This debug fixture owns cold-start receipt/import semantics. Dedicated Windows
            // stress QA owns the unchanged ten-second production latency acceptance.
            .env("COLAY_TEST_WORKSPACE_REGISTER_RESPONSE_TIMEOUT_MS", "30000")
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
            .env("PATH", &self.path)
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root)
            .env("TEMP", &self.temp)
            .env("TMP", &self.temp)
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?))
            .status()
            .context("failed to invoke colay with a daemon spawn barrier")?;
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

fn sqlite_family_content_snapshot(database: &Path) -> Result<BTreeMap<String, String>> {
    let mut snapshot = BTreeMap::new();
    for suffix in ["", "-wal", "-shm"] {
        let path = if suffix.is_empty() {
            database.to_path_buf()
        } else {
            let mut path = database.as_os_str().to_os_string();
            path.push(suffix);
            PathBuf::from(path)
        };
        if path.try_exists()? {
            snapshot.insert(
                suffix.to_owned(),
                format!("{:x}", Sha256::digest(fs::read(path)?)),
            );
        }
    }
    if !snapshot.contains_key("") {
        bail!(
            "legacy SQLite database is missing at {}",
            database.display()
        );
    }
    Ok(snapshot)
}

fn assert_distinct_workspace_imports(
    connection: &Connection,
    fixture: &ConcurrencyFixture,
    seeds: &[LegacyWorkspaceSeed],
) -> Result<()> {
    let expected = i64::try_from(seeds.len()).context("legacy workspace count overflow")?;
    for (table, count) in [
        ("workspaces", expected),
        ("workspace_paths", expected),
        ("legacy_imports", expected),
        ("sessions", expected),
    ] {
        let actual = connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })?;
        if actual != count {
            bail!("expected {count} rows in {table}, found {actual}");
        }
    }
    let distinct_import_workspaces = connection.query_row(
        "SELECT count(DISTINCT workspace_id) FROM legacy_imports",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if distinct_import_workspaces != expected {
        bail!(
            "expected {expected} distinct imported workspaces, found {distinct_import_workspaces}"
        );
    }

    let mut statement = connection
        .prepare("SELECT canonical_path, workspace_id FROM workspace_paths WHERE is_current = 1")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut workspace_ids = BTreeMap::new();
    for row in rows {
        let (canonical_path, workspace_id) = row?;
        workspace_ids.insert(fs::canonicalize(canonical_path)?, workspace_id);
    }

    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        fixture.colay_home.clone(),
    )?)?;
    for seed in seeds {
        let canonical_repository = fs::canonicalize(&seed.repository)?;
        let workspace_id = workspace_ids.get(&canonical_repository).with_context(|| {
            format!(
                "workspace path was not recorded for {}",
                canonical_repository.display()
            )
        })?;
        let session_workspace: String = connection.query_row(
            "SELECT workspace_id FROM sessions WHERE session_id = ?1",
            [&seed.session_id],
            |row| row.get(0),
        )?;
        if session_workspace != *workspace_id {
            bail!(
                "session {} was imported into workspace {}, expected {}",
                seed.session_id,
                session_workspace,
                workspace_id
            );
        }

        let (source_fingerprint, manifest_hash, result_json): (String, String, String) = connection
            .query_row(
                "SELECT source_fingerprint, manifest_hash, result_json \
                 FROM legacy_imports WHERE workspace_id = ?1",
                [workspace_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let result: LegacyImportResult = serde_json::from_str(&result_json)?;
        if !result.imported
            || result.workspace_id.to_string() != *workspace_id
            || result.source_fingerprint != source_fingerprint
            || result.manifest_hash != manifest_hash
            || !is_lower_hex_digest(&source_fingerprint)
            || !is_lower_hex_digest(&manifest_hash)
        {
            bail!("legacy import ledger did not match its indexed fingerprint for {workspace_id}");
        }
        let expected_published = paths
            .for_workspace(result.workspace_id)
            .root
            .join("imports")
            .join(&source_fingerprint);
        if result.published_path != expected_published
            || !result.published_path.join("legacy.db").is_file()
        {
            bail!(
                "legacy import publication did not match fingerprint {source_fingerprint} for workspace {workspace_id}"
            );
        }
    }
    Ok(())
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_clean_client(
    index: usize,
    output: &Output,
    daemon_diagnostics: &DaemonDiagnostics,
) -> Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let normalized = stderr.to_ascii_lowercase();
    if !output.status.success() {
        bail!(
            "client {index} failed with {}: {stderr}; stdout: {stdout}; daemon: {daemon_diagnostics:?}",
            output.status
        );
    }
    for forbidden in [
        "database is locked",
        "database is busy",
        "sqlite_busy",
        "sqlite_locked",
    ] {
        if normalized.contains(forbidden) {
            bail!("client {index} observed {forbidden}: {stderr}");
        }
    }
    let document: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("client {index} did not return valid JSON: {stdout}"))?;
    if !document.is_object() {
        bail!("client {index} returned non-object JSON: {document}");
    }
    Ok(())
}

fn validate_daemon_diagnostics(daemon_diagnostics: &DaemonDiagnostics) -> Result<()> {
    const EXPECTED_CONTENDER_RECORD: &str =
        "error: daemon singleton is already owned by another process";

    for instance in &daemon_diagnostics.instances {
        if !instance.startup_error.is_empty() {
            bail!(
                "daemon {} entered phase {} with startup_error {:?}; stop_requested_at={:?} released_at={:?}",
                instance.pid,
                instance.phase,
                instance.startup_error,
                instance.stop_requested_at,
                instance.released_at
            );
        }
    }

    for record in daemon_diagnostics.stderr.lines() {
        if !record.is_empty() && record != EXPECTED_CONTENDER_RECORD {
            bail!("unexpected daemon contender stderr record: {record:?}");
        }
    }
    Ok(())
}

#[test]
fn daemon_diagnostics_allow_only_expected_owner_contention() -> Result<()> {
    let diagnostics = DaemonDiagnostics {
        instances: vec![DaemonDiagnostic {
            pid: 42,
            phase: "online".to_owned(),
            startup_error: String::new(),
            stop_requested_at: String::new(),
            released_at: String::new(),
        }],
        stderr: concat!(
            "error: daemon singleton is already owned by another process\n",
            "error: daemon singleton is already owned by another process\n",
        )
        .to_owned(),
    };

    validate_daemon_diagnostics(&diagnostics)
}

#[test]
fn daemon_diagnostics_reject_non_exact_owner_contention_records() {
    let invalid_records = [
        "daemon singleton is already owned by another process\n",
        "error: \n",
        "error: error: daemon singleton is already owned by another process\n",
        concat!(
            "error: daemon singleton is already owned by another process",
            "daemon singleton is already owned by another process\n",
        ),
        " error: daemon singleton is already owned by another process\n",
        "error: daemon singleton is already owned by another process \n",
        "error: daemon singleton is already owned by another process: extra\n",
    ];

    for stderr in invalid_records {
        let diagnostics = DaemonDiagnostics {
            instances: Vec::new(),
            stderr: stderr.to_owned(),
        };
        assert!(
            validate_daemon_diagnostics(&diagnostics).is_err(),
            "accepted non-exact daemon diagnostic record {stderr:?}"
        );
    }
}

#[test]
fn daemon_diagnostics_reject_persisted_startup_error() {
    let diagnostics = DaemonDiagnostics {
        instances: vec![DaemonDiagnostic {
            pid: 42,
            phase: "failed".to_owned(),
            startup_error: "unable to open database file".to_owned(),
            stop_requested_at: String::new(),
            released_at: String::new(),
        }],
        stderr: String::new(),
    };

    assert!(validate_daemon_diagnostics(&diagnostics).is_err());
}

#[test]
fn daemon_diagnostics_reject_unexpected_contender_output() {
    let diagnostics = DaemonDiagnostics {
        instances: Vec::new(),
        stderr: "error: IPC I/O failed at daemon.lock: access denied\n".to_owned(),
    };

    assert!(validate_daemon_diagnostics(&diagnostics).is_err());
}

#[test]
fn concurrent_clients_never_observe_sqlite_busy_or_duplicate_rows() -> Result<()> {
    let fixture = ConcurrencyFixture::new()?;

    let outputs = fixture.run_parallel_status_and_plan_clients(CLIENT_COUNT)?;
    fixture.verify_spawned_contenders_resolved(None)?;
    let daemon_diagnostics = fixture.daemon_diagnostics()?;
    validate_daemon_diagnostics(&daemon_diagnostics)?;

    assert_eq!(outputs.len(), CLIENT_COUNT);
    for (index, output) in outputs.iter().enumerate() {
        validate_clean_client(index, output, &daemon_diagnostics)?;
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

#[test]
fn distinct_legacy_workspace_contenders_import_once_without_receipt_cross_reuse() -> Result<()> {
    let fixture = ConcurrencyFixture::new()?;
    let seeds = fixture.seed_distinct_legacy_workspaces(4)?;
    let source_before = seeds
        .iter()
        .map(|seed| sqlite_family_content_snapshot(&seed.database))
        .collect::<Result<Vec<_>>>()?;
    let repositories = seeds
        .iter()
        .map(|seed| seed.repository.clone())
        .collect::<Vec<_>>();

    let outputs = fixture.run_parallel_status_clients(&repositories)?;
    let verification = (|| -> Result<()> {
        fixture.verify_spawn_barrier_arrivals(seeds.len())?;
        fixture.verify_spawned_contenders_resolved(Some(seeds.len()))?;
        let daemon_diagnostics = fixture.daemon_diagnostics()?;
        validate_daemon_diagnostics(&daemon_diagnostics)?;
        if outputs.len() != seeds.len() {
            bail!(
                "expected {} contender outputs, found {}",
                seeds.len(),
                outputs.len()
            );
        }
        for (index, output) in outputs.iter().enumerate() {
            validate_clean_client(index, output, &daemon_diagnostics)?;
        }

        let connection = Connection::open(fixture.database())?;
        assert_distinct_workspace_imports(&connection, &fixture, &seeds)?;
        for (seed, expected) in seeds.iter().zip(&source_before) {
            let actual = sqlite_family_content_snapshot(&seed.database)?;
            if actual != *expected {
                bail!(
                    "legacy SQLite family changed for {}: expected {expected:?}, found {actual:?}",
                    seed.repository.display()
                );
            }
        }
        for table in [
            "tasks",
            "task_attempts",
            "worktrees",
            "coordinator_leases",
            "worker_leases",
        ] {
            let count =
                connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })?;
            if count != 0 {
                bail!("expected no writable residue in {table}, found {count} rows");
            }
        }
        if fixture.count("SELECT count(*) FROM daemon_instances WHERE released_at IS NULL")? != 1 {
            bail!("expected exactly one live daemon before cleanup");
        }
        Ok(())
    })();
    let cleanup = fixture.stop_and_verify();
    verification?;
    cleanup?;
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
    validate_daemon_diagnostics(&daemon_diagnostics)?;
    validate_clean_client(0, &first, &daemon_diagnostics)?;
    validate_clean_client(1, &second, &daemon_diagnostics)?;
    assert_eq!(fixture.count("SELECT count(*) FROM workspaces")?, 1);
    assert_eq!(fixture.count("SELECT count(*) FROM workspace_paths")?, 1);
    assert_eq!(fixture.database_files()?, vec![fixture.database()]);
    fixture.stop_and_verify()?;
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_colay_home_case_aliases_use_one_primary_daemon_endpoint() -> Result<()> {
    let fixture = ConcurrencyFixture::new()?;
    let home_name = fixture
        .colay_home
        .file_name()
        .context("COLAY_HOME has no final component")?
        .to_string_lossy();
    let differently_cased_home = fixture.root.join(
        home_name
            .chars()
            .map(|character| {
                if character.is_ascii_lowercase() {
                    character.to_ascii_uppercase()
                } else if character.is_ascii_uppercase() {
                    character.to_ascii_lowercase()
                } else {
                    character
                }
            })
            .collect::<String>(),
    );

    let first = fixture.run(&fixture.repository, &["--json", "status"])?;
    let second_args = [OsString::from("--json"), OsString::from("status")];
    let second = fixture.command.run_with_colay_home(
        &fixture.repository,
        &second_args,
        &differently_cased_home,
    )?;

    for (label, output) in [("canonical", &first), ("alias", &second)] {
        if !output.status.success() {
            let cleanup = fixture.run(&fixture.repository, &["daemon", "stop"])?;
            bail!(
                "{label} COLAY_HOME client failed with {}: stderr={} stdout={}; cleanup={} cleanup_stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout),
                cleanup.status,
                String::from_utf8_lossy(&cleanup.stderr),
            );
        }
    }

    fixture.verify_spawned_contenders_resolved(None)?;
    let daemon_diagnostics = fixture.daemon_diagnostics()?;
    validate_daemon_diagnostics(&daemon_diagnostics)?;
    validate_clean_client(0, &first, &daemon_diagnostics)?;
    validate_clean_client(1, &second, &daemon_diagnostics)?;
    assert_eq!(fixture.count("SELECT count(*) FROM workspaces")?, 1);
    assert_eq!(fixture.count("SELECT count(*) FROM workspace_paths")?, 1);
    assert_eq!(
        fixture.count("SELECT count(*) FROM daemon_instances WHERE released_at IS NULL")?,
        1
    );
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
    let daemon_diagnostics = fixture.daemon_diagnostics()?;
    validate_daemon_diagnostics(&daemon_diagnostics)?;
    validate_clean_client(0, &output, &daemon_diagnostics)?;
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
