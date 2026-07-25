use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::Path,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use orchestrator_daemon::{
    DaemonOwnerLock, DaemonSettings, ExecutionServices, IntegrationServices, IpcServer,
    MessageRedactor, PlanningServices, serve_with_full_orchestration_on_owned_lease,
};
use orchestrator_domain::{DaemonInstanceId, GraphValidationPolicy, ModelProfile, ProviderId};
use orchestrator_engine::{
    ConversationFailure, ConversationOrchestrator, ConversationRequest, ConversationResponse,
    GitIntegrationManager, PlannerFailure, PlannerRequest, PlannerResponse, TaskPlanner,
};
use orchestrator_process::{RedactionConfig, Redactor, terminate_child_tree};
use orchestrator_providers::{AdapterRuntime, ProcessAdapterRuntime};
use orchestrator_state::{
    ConfigEnvironment, ConfigRequest, DaemonLeaseRequest, DaemonPhase, DaemonStatus, Database,
    EventLog, GlobalStatePaths, LegacyImporter, RepositoryStatePaths, RootConfig, StateEnvironment,
    WorkspaceId, load_effective_config,
};
use serde::Serialize;
use serde_json::json;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::args::DaemonAction;
use crate::ipc_client::DaemonClient;
use colay::conversation_orchestrator::OfficialCliConversationOrchestrator;
use colay::task_executor::OfficialCliTaskExecutor;
use colay::task_planner::OfficialCliTaskPlanner;

const PROVIDER_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const STARTUP_MARGIN: Duration = Duration::from_secs(5);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
struct SpawnedDaemon {
    child: Child,
    pid: u32,
}

pub async fn run(
    repository: &Path,
    _config: &RootConfig,
    explicit_config: Option<&Path>,
    action: DaemonAction,
    json_output: bool,
) -> Result<()> {
    match action {
        DaemonAction::Start => {
            let client = DaemonClient::connect_or_start(repository, explicit_config).await?;
            let response = client.request("daemon.status", json!({})).await?;
            emit(json_output, "daemon_start", &response.outcome["data"])
        }
        DaemonAction::Serve => serve_global(repository, explicit_config).await,
        DaemonAction::Status => {
            let status = match DaemonClient::connect(repository).await {
                Ok(client) => {
                    client.request("daemon.status", json!({})).await?.outcome["data"]["status"]
                        .clone()
                }
                Err(_) => json!(DaemonStatus::Stopped),
            };
            emit(json_output, "daemon_status", &json!({"status": status}))
        }
        DaemonAction::Stop => {
            let status = stop_global(repository).await?;
            emit(json_output, "daemon_stop", &json!({"status": status}))
        }
        DaemonAction::Restart => {
            stop_global(repository).await?;
            let client = DaemonClient::connect_or_start(repository, explicit_config).await?;
            let response = client.request("daemon.status", json!({})).await?;
            emit(json_output, "daemon_restart", &response.outcome["data"])
        }
    }
}

pub(crate) async fn serve_global(repository: &Path, explicit_config: Option<&Path>) -> Result<()> {
    let bootstrap = GlobalDaemonBootstrap::prepare(repository)?;
    let config = load_daemon_config(repository, explicit_config)?;
    let workspace_id = bootstrap.register_workspace(repository, &config)?;
    serve_foreground(repository, &config, bootstrap, workspace_id).await
}

pub(crate) fn load_daemon_config(
    repository: &Path,
    explicit_config: Option<&Path>,
) -> Result<RootConfig> {
    let explicit_config = explicit_config.map(|path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            repository.join(path)
        }
    });
    let environment = ConfigEnvironment {
        colay_home: std::env::var_os("COLAY_HOME").map(Into::into),
        user_home: platform_user_home(),
        colay_config: std::env::var_os("COLAY_CONFIG").map(Into::into),
    };
    let effective = load_effective_config(&ConfigRequest {
        repository,
        cli_config: explicit_config.as_deref(),
        environment,
    })?;
    Ok(effective.config().clone())
}

#[cfg(windows)]
fn platform_user_home() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE").map(Into::into)
}

#[cfg(not(windows))]
fn platform_user_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(Into::into)
}

struct GlobalDaemonBootstrap {
    _owner_lock: DaemonOwnerLock,
    paths: GlobalStatePaths,
    database: Arc<Database>,
}

impl GlobalDaemonBootstrap {
    fn prepare(_repository: &Path) -> Result<Self> {
        let paths = GlobalStatePaths::resolve(&StateEnvironment::from_process())?;
        let owner_lock = DaemonOwnerLock::acquire(&paths)?;
        let database = Arc::new(Database::open(&paths.database)?);
        database.migrate_with_backup(&paths.backups)?;
        Ok(Self {
            _owner_lock: owner_lock,
            paths,
            database,
        })
    }

    fn register_workspace(&self, repository: &Path, config: &RootConfig) -> Result<WorkspaceId> {
        let workspace_id = self
            .database
            .resolve_repository_workspace(repository)?
            .workspace_id;
        let legacy = RepositoryStatePaths::from_config(repository, config)?;
        if let Some(plan) = LegacyImporter::inspect(&legacy, &self.paths)? {
            LegacyImporter::apply(&self.database, workspace_id, &plan, &self.paths)?;
        }
        Ok(workspace_id)
    }
}

pub(crate) async fn ensure_started(
    repository: &Path,
    config: &RootConfig,
    explicit_config: Option<&Path>,
) -> Result<DaemonStatus> {
    let paths = RepositoryStatePaths::from_config(repository, config)?;
    let (database, _) = initialize_database(&paths, repository)?;
    if let DaemonStatus::Online(instance) = database.daemon_status(Utc::now())? {
        return Ok(DaemonStatus::Online(instance));
    }

    let mut spawned = spawn_server(repository, explicit_config)?;
    let startup_timeout = startup_timeout(config);
    let deadline = Instant::now() + startup_timeout;
    loop {
        match database.daemon_status(Utc::now())? {
            online @ DaemonStatus::Online(_) => return Ok(online),
            DaemonStatus::Booting(_)
            | DaemonStatus::Probing(_)
            | DaemonStatus::Failed(_)
            | DaemonStatus::Stopped
            | DaemonStatus::Stale(_) => {}
        }
        if let Some(exit) = spawned
            .child
            .try_wait()
            .context("cannot inspect daemon child")?
        {
            fail_and_release_spawned_lease(
                &database,
                spawned.pid,
                "daemon child exited during startup",
            )?;
            let diagnostic = database
                .daemon_startup_diagnostic_for_pid(spawned.pid)?
                .unwrap_or_default();
            bail!(
                "daemon child exited before becoming healthy: {exit}{}",
                format_startup_diagnostic(&diagnostic)
            );
        }
        if Instant::now() >= deadline {
            let status = database.daemon_status(Utc::now())?;
            if let online @ DaemonStatus::Online(_) = status {
                return Ok(online);
            }
            let phase = daemon_phase_name(&status);
            let (_, tree_error) = terminate_child_tree(&mut spawned.child)
                .await
                .context("cannot terminate timed-out daemon child")?;
            fail_and_release_spawned_lease(
                &database,
                spawned.pid,
                "daemon startup exceeded its bounded deadline",
            )?;
            let diagnostic = database
                .daemon_startup_diagnostic_for_pid(spawned.pid)?
                .unwrap_or_default();
            let tree_error = tree_error
                .map(|error| format!("; process-tree cleanup warning: {error}"))
                .unwrap_or_default();
            bail!(
                "daemon did not become online within {} seconds (last phase: {phase}){tree_error}{}",
                startup_timeout.as_secs(),
                format_startup_diagnostic(&diagnostic)
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn daemon_phase_name(status: &DaemonStatus) -> &'static str {
    match status {
        DaemonStatus::Stopped => "stopped",
        DaemonStatus::Booting(_) => "booting",
        DaemonStatus::Probing(_) => "probing",
        DaemonStatus::Online(_) => "online",
        DaemonStatus::Failed(_) => "failed",
        DaemonStatus::Stale(_) => "stale",
    }
}

fn fail_and_release_spawned_lease(database: &Database, pid: u32, diagnostic: &str) -> Result<()> {
    let instance = match database.daemon_status(Utc::now())? {
        DaemonStatus::Booting(instance)
        | DaemonStatus::Probing(instance)
        | DaemonStatus::Online(instance)
        | DaemonStatus::Failed(instance)
        | DaemonStatus::Stale(instance)
            if instance.pid == pid =>
        {
            Some(instance)
        }
        _ => None,
    };
    if let Some(instance) = instance {
        if matches!(instance.phase, DaemonPhase::Booting | DaemonPhase::Probing) {
            database.transition_daemon_phase(
                instance.instance_id,
                DaemonPhase::Failed,
                Some(diagnostic),
            )?;
        }
        database.release_daemon(instance.instance_id, Utc::now())?;
    }
    Ok(())
}

fn format_startup_diagnostic(diagnostic: &str) -> String {
    if diagnostic.is_empty() {
        String::new()
    } else {
        format!("; startup diagnostic: {diagnostic}")
    }
}

fn startup_timeout(config: &RootConfig) -> Duration {
    let providers = &config.orchestrator.providers;
    let enabled_count = [
        providers.gemini.as_ref(),
        providers.agy.as_ref(),
        providers.codex.as_ref(),
        providers.claude.as_ref(),
    ]
    .into_iter()
    .flatten()
    .filter(|provider| provider.enabled)
    .count();
    let probe_budget =
        PROVIDER_PROBE_TIMEOUT.saturating_mul(u32::try_from(enabled_count).unwrap_or(u32::MAX));
    probe_budget.saturating_add(STARTUP_MARGIN)
}

#[allow(clippy::too_many_lines)]
async fn serve_foreground(
    repository: &Path,
    config: &RootConfig,
    bootstrap: GlobalDaemonBootstrap,
    workspace_id: WorkspaceId,
) -> Result<()> {
    let GlobalDaemonBootstrap {
        _owner_lock,
        paths,
        database,
    } = bootstrap;
    let repository_root = std::fs::canonicalize(repository)?;
    let workspace_paths = paths.for_workspace(workspace_id);
    let settings = DaemonSettings::default();
    let instance_id = DaemonInstanceId::new();
    database.acquire_daemon_startup_lease(&DaemonLeaseRequest {
        instance_id,
        pid: std::process::id(),
        started_at: Utc::now(),
        ttl: settings.lease_ttl,
    })?;
    let startup_lease = StartupLeaseGuard::new(Arc::clone(&database), instance_id);
    database.record_daemon_runtime_identity(
        instance_id,
        &std::env::current_exe()?.to_string_lossy(),
        crate::args::COLAY_VERSION,
        &format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
    )?;
    let cancellation = CancellationToken::new();
    let signal_cancellation = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancellation.cancel();
        }
    });
    let heartbeat_database = Arc::clone(&database);
    let heartbeat_cancellation = cancellation.clone();
    let startup_heartbeat = tokio::spawn(async move {
        let mut interval = tokio::time::interval(settings.heartbeat_interval);
        loop {
            tokio::select! {
                () = heartbeat_cancellation.cancelled() => return Ok::<(), anyhow::Error>(()),
                _ = interval.tick() => {
                    if heartbeat_database.daemon_stop_requested(instance_id)? {
                        heartbeat_cancellation.cancel();
                        return Ok(());
                    }
                    heartbeat_database.heartbeat_daemon(
                        instance_id,
                        Utc::now(),
                        settings.lease_ttl,
                    )?;
                }
            }
        }
    });
    let ipc_server = IpcServer::bind(&paths, Arc::clone(&database))?;
    let ipc_cancellation = cancellation.clone();
    let ipc_task = tokio::spawn(async move { ipc_server.serve(ipc_cancellation).await });
    let redaction = RedactionConfig {
        literals: Vec::new(),
        patterns: config.orchestrator.redaction.patterns.clone(),
    };
    let process_redactor = match Redactor::new(&redaction) {
        Ok(redactor) => ProcessMessageRedactor(redactor),
        Err(error) => {
            let diagnostic = "daemon redaction configuration is invalid";
            database.transition_daemon_phase(instance_id, DaemonPhase::Failed, Some(diagnostic))?;
            database.release_daemon(instance_id, Utc::now())?;
            startup_heartbeat.abort();
            signal_task.abort();
            return Err(error.into());
        }
    };
    let redactor: Arc<dyn MessageRedactor> = Arc::new(process_redactor);
    let runtime: Arc<dyn AdapterRuntime> = Arc::new(ProcessAdapterRuntime::new(redaction));
    database.transition_daemon_phase(instance_id, DaemonPhase::Probing, None)?;
    database.transition_daemon_phase(instance_id, DaemonPhase::Online, None)?;
    let (planner, planner_provider, conversation): (
        Arc<dyn TaskPlanner>,
        ProviderId,
        Arc<dyn ConversationOrchestrator>,
    ) = match OfficialCliTaskPlanner::probe_from_config(
        config,
        repository,
        Arc::clone(&runtime),
        ModelProfile::Standard,
    )
    .await
    {
        Ok(planner) => {
            let provider = planner.primary_provider();
            let conversation = Arc::new(OfficialCliConversationOrchestrator::from_task_planner(
                &planner,
            ));
            (Arc::new(planner), provider, conversation)
        }
        Err(error) => {
            let reason = error.to_string();
            (
                Arc::new(UnavailablePlanner {
                    reason: reason.clone(),
                }),
                ProviderId::Codex,
                Arc::new(UnavailableConversation { reason }),
            )
        }
    };
    let executor = match OfficialCliTaskExecutor::new(config, repository, runtime) {
        Ok(executor) => Arc::new(executor),
        Err(error) => {
            return fail_startup(
                &database,
                instance_id,
                &startup_heartbeat,
                &signal_task,
                redactor.as_ref(),
                &error,
            );
        }
    };
    let integration_manager = match GitIntegrationManager::new(repository, &workspace_paths.root) {
        Ok(manager) => Arc::new(manager),
        Err(error) => {
            return fail_startup(
                &database,
                instance_id,
                &startup_heartbeat,
                &signal_task,
                redactor.as_ref(),
                &error,
            );
        }
    };
    let integration = IntegrationServices {
        manager: integration_manager,
        repository_root: repository_root.clone(),
        state_root: workspace_paths.root.clone(),
    };
    let provider_limits = config
        .orchestrator
        .provider_parallel_limits
        .iter()
        .filter_map(|(provider, limit)| {
            let provider = match provider.as_str() {
                "agy" => ProviderId::Agy,
                "codex" => ProviderId::Codex,
                "claude" => ProviderId::Claude,
                "gemini" => ProviderId::Gemini,
                _ => return None,
            };
            Some((provider, usize::try_from(*limit).unwrap_or(usize::MAX)))
        })
        .collect();
    if cancellation.is_cancelled() {
        database.release_daemon(instance_id, Utc::now())?;
        startup_heartbeat.abort();
        signal_task.abort();
        return Ok(());
    }
    if startup_heartbeat.is_finished() {
        match startup_heartbeat.await {
            Ok(Ok(())) if cancellation.is_cancelled() => {
                database.release_daemon(instance_id, Utc::now())?;
                signal_task.abort();
                return Ok(());
            }
            Ok(Ok(())) => {
                return fail_startup_without_heartbeat(
                    &database,
                    instance_id,
                    &signal_task,
                    redactor.as_ref(),
                    &anyhow::anyhow!("daemon startup heartbeat stopped unexpectedly"),
                );
            }
            Ok(Err(error)) => {
                return fail_startup_without_heartbeat(
                    &database,
                    instance_id,
                    &signal_task,
                    redactor.as_ref(),
                    &error,
                );
            }
            Err(error) => {
                return fail_startup_without_heartbeat(
                    &database,
                    instance_id,
                    &signal_task,
                    redactor.as_ref(),
                    &anyhow::anyhow!("daemon startup heartbeat task failed: {error}"),
                );
            }
        }
    }
    startup_heartbeat.abort();
    let _ = startup_heartbeat.await;
    let runtime_cancellation = cancellation.clone();
    let runtime = async move {
        let mut startup_lease = startup_lease;
        let result = serve_with_full_orchestration_on_owned_lease(
            database,
            workspace_id,
            instance_id,
            runtime_cancellation,
            settings,
            redactor,
            PlanningServices {
                conversation,
                repository_root: repository_root.clone(),
                planner,
                planner_provider,
                validation_policy: GraphValidationPolicy {
                    eligible_providers: BTreeSet::from([planner_provider]),
                    eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
                    max_parallel_workers: usize::try_from(config.orchestrator.max_parallel_workers)
                        .unwrap_or(usize::MAX)
                        .max(1),
                    per_provider_limits: BTreeMap::new(),
                },
                integration: Some(integration),
            },
            ExecutionServices {
                executor,
                repository_root,
                state_root: workspace_paths.root.clone(),
                global_limit: usize::try_from(config.orchestrator.max_parallel_workers)
                    .unwrap_or(usize::MAX)
                    .max(1),
                provider_limits,
                claim_ttl: TimeDelta::minutes(
                    i64::try_from(config.orchestrator.default_timeout_minutes)
                        .unwrap_or(i64::MAX)
                        .saturating_add(10),
                ),
            },
        )
        .await;
        startup_lease.handoff();
        result?;
        Ok(())
    };
    let result = supervise_daemon_runtime(cancellation.clone(), ipc_task, runtime).await;
    signal_task.abort();
    result
}

async fn supervise_daemon_runtime<F>(
    cancellation: CancellationToken,
    mut ipc_task: tokio::task::JoinHandle<Result<(), orchestrator_daemon::IpcError>>,
    runtime: F,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    tokio::pin!(runtime);
    tokio::select! {
        runtime_result = &mut runtime => {
            cancellation.cancel();
            let ipc_result = ipc_task
                .await
                .map_err(|error| anyhow::anyhow!("IPC task failed: {error}"))?;
            ipc_result?;
            runtime_result
        }
        ipc_join = &mut ipc_task => {
            let ipc_result = ipc_join
                .map_err(|error| anyhow::anyhow!("IPC task failed: {error}"))?;
            if cancellation.is_cancelled() {
                let runtime_result = runtime.await;
                ipc_result?;
                runtime_result
            } else {
                cancellation.cancel();
                ipc_result?;
                bail!("IPC server stopped unexpectedly while the daemon was online")
            }
        }
    }
}

struct StartupLeaseGuard {
    database: Arc<Database>,
    instance_id: DaemonInstanceId,
    armed: bool,
}

impl StartupLeaseGuard {
    const fn new(database: Arc<Database>, instance_id: DaemonInstanceId) -> Self {
        Self {
            database,
            instance_id,
            armed: true,
        }
    }

    const fn handoff(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartupLeaseGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.database.release_daemon(self.instance_id, Utc::now());
        }
    }
}

fn fail_startup(
    database: &Database,
    instance_id: DaemonInstanceId,
    startup_heartbeat: &tokio::task::JoinHandle<Result<()>>,
    signal_task: &tokio::task::JoinHandle<()>,
    redactor: &dyn MessageRedactor,
    error: &dyn std::fmt::Display,
) -> Result<()> {
    startup_heartbeat.abort();
    fail_startup_without_heartbeat(database, instance_id, signal_task, redactor, error)
}

fn fail_startup_without_heartbeat(
    database: &Database,
    instance_id: DaemonInstanceId,
    signal_task: &tokio::task::JoinHandle<()>,
    redactor: &dyn MessageRedactor,
    error: &dyn std::fmt::Display,
) -> Result<()> {
    let diagnostic = redactor.redact(&error.to_string());
    signal_task.abort();
    let status = database.daemon_status(Utc::now())?;
    if matches!(
        status,
        DaemonStatus::Booting(ref instance) | DaemonStatus::Probing(ref instance)
            if instance.instance_id == instance_id
    ) {
        database.transition_daemon_phase(instance_id, DaemonPhase::Failed, Some(&diagnostic))?;
    }
    database.release_daemon(instance_id, Utc::now())?;
    Err(anyhow::anyhow!(diagnostic))
}

async fn stop_global(repository: &Path) -> Result<DaemonStatus> {
    let Ok(client) = DaemonClient::connect(repository).await else {
        return Ok(DaemonStatus::Stopped);
    };
    client.request("daemon.stop", json!({})).await?;
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        if DaemonClient::connect(repository).await.is_err() {
            return Ok(DaemonStatus::Stopped);
        }
        if Instant::now() >= deadline {
            bail!("user daemon did not release its IPC endpoint within ten seconds");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

pub(crate) fn initialize_database(
    paths: &RepositoryStatePaths,
    repository: &Path,
) -> Result<(Database, WorkspaceId)> {
    let database = Database::open(&paths.database)?;
    database.migrate_with_backup(&paths.backups)?;
    let workspace_id = database
        .resolve_repository_workspace(repository)?
        .workspace_id;
    let workspace = database.workspace(workspace_id);
    EventLog::open(&paths.events)?.reconcile_workspace(&workspace)?;
    Ok((database, workspace_id))
}

pub(crate) fn open_ready_database(paths: &RepositoryStatePaths) -> Result<Database> {
    if !paths.database.exists() {
        bail!(
            "state database does not exist at {}; run `colay init` or `colay daemon start`",
            paths.database.display()
        );
    }
    let database = Database::open(&paths.database)?;
    let migration = database.migration_status()?;
    if !migration.pending_versions.is_empty() {
        bail!(
            "state schema migration is required ({:?}); run `colay migrate apply`",
            migration.pending_versions
        );
    }
    Ok(database)
}

fn spawn_server(repository: &Path, explicit_config: Option<&Path>) -> Result<SpawnedDaemon> {
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
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_background_process(&mut command);
    let child = command.spawn().context("cannot spawn repository daemon")?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("spawned repository daemon has no process ID"))?;
    Ok(SpawnedDaemon { child, pid })
}

#[cfg(windows)]
fn configure_background_process(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
}

#[cfg(unix)]
fn configure_background_process(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(any(unix, windows)))]
fn configure_background_process(_command: &mut Command) {}

struct ProcessMessageRedactor(Redactor);

impl MessageRedactor for ProcessMessageRedactor {
    fn redact(&self, value: &str) -> String {
        self.0.redact(value)
    }
}

struct UnavailablePlanner {
    reason: String,
}

struct UnavailableConversation {
    reason: String,
}

#[async_trait]
impl ConversationOrchestrator for UnavailableConversation {
    async fn converse(
        &self,
        _request: ConversationRequest,
    ) -> Result<ConversationResponse, ConversationFailure> {
        Err(ConversationFailure::Invocation {
            reason: self.reason.clone(),
            evidence_redacted: String::new(),
        })
    }
}

#[async_trait]
impl TaskPlanner for UnavailablePlanner {
    async fn propose(&self, _request: PlannerRequest) -> Result<PlannerResponse, PlannerFailure> {
        Err(PlannerFailure::Invocation {
            reason: self.reason.clone(),
            evidence_redacted: String::new(),
        })
    }
}

fn emit<T: Serialize>(json_output: bool, command: &str, data: &T) -> Result<()> {
    let envelope = json!({
        "schema_version": "1",
        "command": command,
        "data": data,
    });
    if json_output {
        println!("{}", serde_json::to_string(&envelope)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{TimeDelta, Utc};
    use orchestrator_domain::DaemonInstanceId;
    use orchestrator_state::{DaemonLeaseRequest, DaemonStatus, Database};
    use tokio_util::sync::CancellationToken;

    use super::{
        StartupLeaseGuard, fail_and_release_spawned_lease, fail_startup_without_heartbeat,
        supervise_daemon_runtime,
    };

    struct IdentityRedactor;

    impl orchestrator_daemon::MessageRedactor for IdentityRedactor {
        fn redact(&self, value: &str) -> String {
            value.to_owned()
        }
    }

    fn database() -> Result<Database, Box<dyn std::error::Error>> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        Ok(database)
    }

    #[test]
    fn timeout_cleanup_fails_and_releases_only_the_spawned_pid()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let instance_id = DaemonInstanceId::new();
        database.acquire_daemon_startup_lease(&DaemonLeaseRequest {
            instance_id,
            pid: 42,
            started_at: Utc::now(),
            ttl: TimeDelta::seconds(5),
        })?;

        fail_and_release_spawned_lease(&database, 43, "wrong owner")?;
        assert!(matches!(
            database.daemon_status(Utc::now())?,
            DaemonStatus::Booting(_)
        ));

        fail_and_release_spawned_lease(&database, 42, "bounded timeout")?;
        assert_eq!(database.daemon_status(Utc::now())?, DaemonStatus::Stopped);
        assert_eq!(
            database.daemon_startup_diagnostic_for_pid(42)?.as_deref(),
            Some("bounded timeout")
        );
        Ok(())
    }

    #[tokio::test]
    async fn failure_after_ipc_readiness_releases_an_online_startup_lease()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let instance_id = DaemonInstanceId::new();
        database.acquire_daemon_startup_lease(&DaemonLeaseRequest {
            instance_id,
            pid: 42,
            started_at: Utc::now(),
            ttl: TimeDelta::seconds(5),
        })?;
        database.transition_daemon_phase(
            instance_id,
            orchestrator_state::DaemonPhase::Probing,
            None,
        )?;
        database.transition_daemon_phase(
            instance_id,
            orchestrator_state::DaemonPhase::Online,
            None,
        )?;
        let signal_task = tokio::spawn(std::future::pending::<()>());

        let error = match fail_startup_without_heartbeat(
            &database,
            instance_id,
            &signal_task,
            &IdentityRedactor,
            &"service setup failed",
        ) {
            Ok(()) => {
                return Err(std::io::Error::other("online setup failure must be reported").into());
            }
            Err(error) => error,
        };

        assert_eq!(error.to_string(), "service setup failed");
        assert_eq!(database.daemon_status(Utc::now())?, DaemonStatus::Stopped);
        Ok(())
    }

    #[tokio::test]
    async fn ipc_failure_cancels_runtime_and_releases_online_lease()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = Arc::new(database()?);
        let instance_id = DaemonInstanceId::new();
        database.acquire_daemon_startup_lease(&DaemonLeaseRequest {
            instance_id,
            pid: 42,
            started_at: Utc::now(),
            ttl: TimeDelta::seconds(5),
        })?;
        database.transition_daemon_phase(
            instance_id,
            orchestrator_state::DaemonPhase::Probing,
            None,
        )?;
        database.transition_daemon_phase(
            instance_id,
            orchestrator_state::DaemonPhase::Online,
            None,
        )?;
        let cancellation = CancellationToken::new();
        let runtime_cancellation = cancellation.clone();
        let guard = StartupLeaseGuard::new(Arc::clone(&database), instance_id);
        let runtime = async move {
            let _guard = guard;
            runtime_cancellation.cancelled().await;
            Ok(())
        };
        let ipc_task = tokio::spawn(async {
            Err(orchestrator_daemon::IpcError::Protocol(
                "injected accept failure".to_owned(),
            ))
        });

        let error = match supervise_daemon_runtime(cancellation.clone(), ipc_task, runtime).await {
            Ok(()) => {
                return Err(
                    std::io::Error::other("IPC failure must stop the daemon runtime").into(),
                );
            }
            Err(error) => error,
        };

        assert!(error.to_string().contains("injected accept failure"));
        assert!(cancellation.is_cancelled());
        assert_eq!(database.daemon_status(Utc::now())?, DaemonStatus::Stopped);
        Ok(())
    }
}
