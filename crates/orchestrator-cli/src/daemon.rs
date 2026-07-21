use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use orchestrator_daemon::{
    DaemonSettings, ExecutionServices, MessageRedactor, PlanningServices,
    serve_with_full_orchestration,
};
use orchestrator_domain::{DaemonInstanceId, GraphValidationPolicy, ModelProfile, ProviderId};
use orchestrator_engine::{PlannerFailure, PlannerRequest, PlannerResponse, TaskPlanner};
use orchestrator_process::{RedactionConfig, Redactor};
use orchestrator_providers::{AdapterRuntime, ProcessAdapterRuntime};
use orchestrator_state::{DaemonStatus, Database, EventLog, RepositoryStatePaths, RootConfig};
use serde::Serialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::args::DaemonAction;
use colay::task_executor::OfficialCliTaskExecutor;
use colay::task_planner::OfficialCliTaskPlanner;

const START_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

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

    let mut child = spawn_server(repository, explicit_config)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match database.daemon_status(Utc::now())? {
            online @ DaemonStatus::Online(_) => return Ok(online),
            DaemonStatus::Stopped | DaemonStatus::Stale(_) => {}
        }
        if let Some(exit) = child.try_wait().context("cannot inspect daemon child")? {
            bail!("daemon child exited before becoming healthy: {exit}");
        }
        if Instant::now() >= deadline {
            bail!("daemon did not publish a healthy heartbeat within five seconds");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn serve_foreground(repository: &Path, config: &RootConfig) -> Result<()> {
    let paths = RepositoryStatePaths::from_config(repository, config)?;
    let database = Arc::new(open_ready_database(&paths)?);
    let cancellation = CancellationToken::new();
    let signal_cancellation = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancellation.cancel();
        }
    });
    let redaction = RedactionConfig {
        literals: Vec::new(),
        patterns: config.orchestrator.redaction.patterns.clone(),
    };
    let redactor: Arc<dyn MessageRedactor> =
        Arc::new(ProcessMessageRedactor(Redactor::new(&redaction)?));
    let runtime: Arc<dyn AdapterRuntime> = Arc::new(ProcessAdapterRuntime::new(redaction));
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
    let executor = Arc::new(OfficialCliTaskExecutor::new(config, repository, runtime)?);
    let provider_limits = config
        .orchestrator
        .provider_parallel_limits
        .iter()
        .filter_map(|(provider, limit)| {
            let provider = match provider.as_str() {
                "codex" => ProviderId::Codex,
                "claude" => ProviderId::Claude,
                "gemini" => ProviderId::Gemini,
                _ => return None,
            };
            Some((provider, usize::try_from(*limit).unwrap_or(usize::MAX)))
        })
        .collect();
    let result = serve_with_full_orchestration(
        database,
        DaemonInstanceId::new(),
        std::process::id(),
        cancellation,
        DaemonSettings::default(),
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
        },
        ExecutionServices {
            executor,
            repository_root: std::fs::canonicalize(repository)?,
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
        DaemonStatus::Online(instance) => {
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
            DaemonStatus::Online(_) => {}
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
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_background_process(&mut command);
    command.spawn().context("cannot spawn repository daemon")
}

#[cfg(windows)]
fn configure_background_process(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
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
