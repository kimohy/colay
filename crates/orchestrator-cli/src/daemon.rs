use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use orchestrator_daemon::{
    DaemonSettings, ExecutionServices, IntegrationServices, MessageRedactor, PlanningServices,
    serve_with_full_orchestration_on_owned_lease,
};
use orchestrator_domain::{DaemonInstanceId, GraphValidationPolicy, ModelProfile, ProviderId};
use orchestrator_engine::{
    GitIntegrationManager, PlannerFailure, PlannerRequest, PlannerResponse, TaskPlanner,
};
use orchestrator_process::{RedactionConfig, Redactor, terminate_child_tree};
use orchestrator_providers::{AdapterRuntime, ProcessAdapterRuntime};
use orchestrator_state::{
    DaemonLeaseRequest, DaemonPhase, DaemonStatus, Database, EventLog, RepositoryStatePaths,
    RootConfig,
};
use serde::Serialize;
use serde_json::json;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::args::DaemonAction;
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
    config: &RootConfig,
    explicit_config: Option<&Path>,
    action: DaemonAction,
    json_output: bool,
) -> Result<()> {
    match action {
        DaemonAction::Start => {
            let status = ensure_started(repository, config, explicit_config).await?;
            emit(json_output, "daemon_start", &json!({"status": status}))
        }
        DaemonAction::Serve => serve_foreground(repository, config).await,
        DaemonAction::Status => {
            let status = status(repository, config)?;
            emit(json_output, "daemon_status", &json!({"status": status}))
        }
        DaemonAction::Stop => {
            let status = stop(repository, config).await?;
            emit(json_output, "daemon_stop", &json!({"status": status}))
        }
        DaemonAction::Restart => {
            stop(repository, config).await?;
            let status = ensure_started(repository, config, explicit_config).await?;
            emit(json_output, "daemon_restart", &json!({"status": status}))
        }
    }
}

pub(crate) async fn ensure_started(
    repository: &Path,
    config: &RootConfig,
    explicit_config: Option<&Path>,
) -> Result<DaemonStatus> {
    let paths = RepositoryStatePaths::from_config(repository, config)?;
    let database = initialize_database(&paths)?;
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
async fn serve_foreground(repository: &Path, config: &RootConfig) -> Result<()> {
    let paths = RepositoryStatePaths::from_config(repository, config)?;
    let repository_root = std::fs::canonicalize(repository)?;
    let database = Arc::new(open_ready_database(&paths)?);
    let settings = DaemonSettings::default();
    let instance_id = DaemonInstanceId::new();
    database.acquire_daemon_startup_lease(&DaemonLeaseRequest {
        instance_id,
        pid: std::process::id(),
        started_at: Utc::now(),
        ttl: settings.lease_ttl,
    })?;
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
    let (planner, planner_provider): (Arc<dyn TaskPlanner>, ProviderId) =
        match OfficialCliTaskPlanner::probe_from_config(
            config,
            repository,
            Arc::clone(&runtime),
            ModelProfile::Standard,
        )
        .await
        {
            Ok(planner) => {
                let provider = planner.primary_provider();
                (Arc::new(planner), provider)
            }
            Err(error) => (
                Arc::new(UnavailablePlanner {
                    reason: error.to_string(),
                }),
                ProviderId::Codex,
            ),
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
    let integration_manager = match GitIntegrationManager::new(repository, &paths.root) {
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
        state_root: paths.root.clone(),
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
    database.transition_daemon_phase(instance_id, DaemonPhase::Online, None)?;
    startup_heartbeat.abort();
    let result = serve_with_full_orchestration_on_owned_lease(
        database,
        instance_id,
        cancellation,
        settings,
        redactor,
        PlanningServices {
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
            state_root: paths.root.clone(),
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
    signal_task.abort();
    result?;
    Ok(())
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
    database.transition_daemon_phase(instance_id, DaemonPhase::Failed, Some(&diagnostic))?;
    database.release_daemon(instance_id, Utc::now())?;
    signal_task.abort();
    Err(anyhow::anyhow!(diagnostic))
}

pub(crate) fn status(repository: &Path, config: &RootConfig) -> Result<DaemonStatus> {
    let paths = RepositoryStatePaths::from_config(repository, config)?;
    if !paths.database.exists() {
        return Ok(DaemonStatus::Stopped);
    }
    open_ready_database(&paths)?
        .daemon_status(Utc::now())
        .map_err(Into::into)
}

async fn stop(repository: &Path, config: &RootConfig) -> Result<DaemonStatus> {
    let paths = RepositoryStatePaths::from_config(repository, config)?;
    if !paths.database.exists() {
        return Ok(DaemonStatus::Stopped);
    }
    let database = open_ready_database(&paths)?;
    match database.daemon_status(Utc::now())? {
        DaemonStatus::Stopped => return Ok(DaemonStatus::Stopped),
        DaemonStatus::Stale(instance) => {
            database.release_daemon(instance.instance_id, Utc::now())?;
            return Ok(DaemonStatus::Stopped);
        }
        DaemonStatus::Booting(instance)
        | DaemonStatus::Probing(instance)
        | DaemonStatus::Online(instance)
        | DaemonStatus::Failed(instance) => {
            database.request_daemon_stop(instance.instance_id, Utc::now())?;
        }
    }

    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        match database.daemon_status(Utc::now())? {
            DaemonStatus::Stopped => return Ok(DaemonStatus::Stopped),
            DaemonStatus::Stale(instance) => {
                database.release_daemon(instance.instance_id, Utc::now())?;
                return Ok(DaemonStatus::Stopped);
            }
            DaemonStatus::Booting(_)
            | DaemonStatus::Probing(_)
            | DaemonStatus::Online(_)
            | DaemonStatus::Failed(_) => {}
        }
        if Instant::now() >= deadline {
            bail!("daemon did not release its lease within ten seconds");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

pub(crate) fn initialize_database(paths: &RepositoryStatePaths) -> Result<Database> {
    let database = Database::open(&paths.database)?;
    database.migrate_with_backup(&paths.backups)?;
    EventLog::open(&paths.events)?.reconcile(&database)?;
    Ok(database)
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
    use chrono::{TimeDelta, Utc};
    use orchestrator_domain::DaemonInstanceId;
    use orchestrator_state::{DaemonLeaseRequest, DaemonStatus, Database};

    use super::fail_and_release_spawned_lease;

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
}
