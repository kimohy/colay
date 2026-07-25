use std::{
    collections::HashMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    str::FromStr as _,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, TimeDelta, Utc};
use codex_compat::{
    CapabilityProbe, CapabilitySource, CodexProbeReport, ProbeCommand, ProbeOutput,
};
use orchestrator_domain::{
    AcceptanceEvidence, AppendMessageCommandPayload, AttemptId, CapabilitySupport, ClientCommand,
    ClientCommandAction, ClientCommandId, ClientCommandState, CommandEvidence, CommandEvidenceId,
    CorrelationId, CreateSessionCommandPayload, DecisionRecord, EventActor, EventId, EventType,
    FailureRecord, HandoverAcknowledgement, HealthStatus, MessageId, ModelProfile, PlanStep,
    PlanStepStatus, ProviderCapabilities, ProviderHealth, ProviderId, QuotaPeriod, QuotaScope,
    ReasoningEffort, RepoPath, RoutingDecision, SandboxMode, SchemaVersion, SessionId,
    TaskEnvelope, TaskEvent, TaskId, TaskState, TestEvidence, TestStatus, UsageConfidence,
    UsageSnapshot, UsageSource, UsageUnit, VerificationStatus, WorkerEvent, WorkerOutcome,
    WorkerRequest, WorkerResult,
};
use orchestrator_engine::{
    CheckpointInput, CheckpointManager, CodexExecutionPolicy, GitCheckpointEvidence, GitWorktree,
    GitWorktreeManager, HandoverInput, HandoverManager, StartupGuard, VerificationEngine,
    VerificationInput, canonicalize_directory,
};
use orchestrator_policy::{
    BudgetForecaster, ForecastConfig, ResetPolicy, RoutingCandidate, RoutingConfig, RoutingContext,
    RoutingEngine, TaskRole, period_window,
};
use orchestrator_process::{
    CommandSpec, ProcessError, ProcessRunner, RedactionConfig, Redactor, ResolvedExecutable,
    validate_resolution_evidence,
};
use orchestrator_providers::{
    AdapterRuntime, AgyAdapter, AgyAdapterConfig, ClaudeAdapter, ClaudeAdapterConfig, CodexAdapter,
    CodexAdapterConfig, CodexTransportFeatures, GeminiAdapter, GeminiAdapterConfig,
    ProcessAdapterRuntime, RuntimeTermination, WorkerAdapter,
};
use orchestrator_state::{
    ArtifactStore, ConfigDocument, ConfigEnvironment, ConfigLayerKind, ConfigRequest,
    ControlAction, DaemonStatus, Database, EffectiveConfig, EventLog, GlobalStatePaths,
    LeaseRenewal, MigratableConfigDocument, NewTaskAttemptRecord, NewWorktreeRecord,
    OrchestratorConfig, ProviderConfig, RepositoryStatePaths as StatePaths, RootConfig,
    RoutingAuditRecord, StateEnvironment, StateError, StoredHandover, StoredTask, WorkerLease,
    WorkerLeaseMode, WorkerLeaseRequest, WorkspaceDatabase, load_effective_config,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use toml_edit::{DocumentMut, Item, Table, TableLike};
use uuid::Uuid;

#[cfg(test)]
use orchestrator_state::{CoordinatorLease, CoordinatorLeaseRequest};

use crate::args::{
    Cli, Command, DoctorAction, EffortName, HandoverArgs, MigrationAction, MigrationRollbackAction,
    ProfileAction, ProfileName, ProviderAction, ProviderName, RequiredTask, RollbackAction,
    RunArgs, TaskSelector, UsageAction, UsageOverrideArgs,
};
use crate::profile_config::{
    ProfileReportRow, ProfileSource, effective_profile_rows, reset_profile_override,
    set_profile_override,
};

const CONFIG_TEMPLATE: &str = include_str!("../../../config.example.toml");
const DEFAULT_CONFIG_PATH: &str = ".colay/config.toml";
const LEGACY_CONFIG_PATH: &str = ".codex/orchestrator/config.toml";
const WORKER_LEASE_TTL_SECONDS: i64 = 20;
#[cfg(test)]
const COORDINATOR_LEASE_TTL_SECONDS: i64 = 30;
const LEASE_RENEWAL_INTERVAL_SECONDS: u64 = 5;
const RUN_SESSION_KEY: &str = "cli-run-session-v1";
const RUN_REQUESTED_BY: &str = "local-cli-run";
const RUN_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(25);
const RUN_COMMAND_MAX_POLL_INTERVAL: Duration = Duration::from_secs(1);
const RUN_COMMAND_TIMEOUT: Duration = Duration::from_mins(2);

struct ConfigRuntime {
    effective: EffectiveConfig,
    explicit_edit_path: PathBuf,
}

#[allow(clippy::too_many_lines)]
pub async fn run(cli: Cli) -> Result<()> {
    configure_tracing();
    let repository = match &cli.command {
        Command::Init(arguments) => fs::canonicalize(&arguments.repository).with_context(|| {
            format!(
                "cannot canonicalize repository directory: {}",
                arguments.repository.display()
            )
        })?,
        _ => current_repository()?,
    };
    let environment = config_environment();
    let runtime_result = if matches!(cli.command, Command::Init(_)) {
        load_initialization_config_runtime(&repository, cli.config.as_deref(), environment.clone())
    } else {
        load_config_runtime(&repository, cli.config.as_deref(), environment.clone())
    };
    let runtime = match runtime_result {
        Ok(runtime) => runtime,
        Err(load_error) => {
            if let Command::Migrate(arguments) = &cli.command {
                match migration_fallback_path(&repository, cli.config.as_deref(), &environment)? {
                    Some(explicit_edit_path) => {
                        return migrate_without_runtime(
                            &repository,
                            &explicit_edit_path,
                            arguments.action.clone(),
                            cli.json,
                        );
                    }
                    None => return Err(load_error),
                }
            }
            return Err(load_error);
        }
    };
    match cli.command {
        Command::Init(_) => initialize(&repository, &runtime, cli.json),
        Command::Daemon(arguments) => {
            crate::daemon::run(
                &repository,
                runtime.effective.config(),
                cli.config.as_deref(),
                arguments.action,
                cli.json,
            )
            .await
        }
        Command::Run(arguments) => {
            run_conversation(
                &repository,
                cli.config.as_deref(),
                &runtime.effective,
                arguments,
                cli.json,
            )
            .await
        }
        Command::Status(selector) => {
            status(&repository, cli.config.as_deref(), &selector, cli.json).await
        }
        Command::Providers(arguments) => match arguments.action {
            Some(ProviderAction::Enable { provider }) => {
                set_provider_enabled(
                    &repository,
                    cli.config.as_deref(),
                    environment,
                    &runtime,
                    provider.into(),
                    true,
                    cli.json,
                )
                .await
            }
            Some(ProviderAction::Disable { provider }) => {
                set_provider_enabled(
                    &repository,
                    cli.config.as_deref(),
                    environment,
                    &runtime,
                    provider.into(),
                    false,
                    cli.json,
                )
                .await
            }
            None => providers(&runtime.effective, cli.json).await,
        },
        Command::Profiles(arguments) => match arguments.action {
            Some(ProfileAction::Set(arguments)) => {
                set_model_profile(
                    &repository,
                    cli.config.as_deref(),
                    environment,
                    &runtime,
                    arguments.provider,
                    arguments.profile,
                    &arguments.model,
                    arguments.effort,
                    cli.json,
                )
                .await
            }
            Some(ProfileAction::Reset(arguments)) => {
                reset_model_profile(
                    &repository,
                    cli.config.as_deref(),
                    environment,
                    &runtime,
                    arguments.provider,
                    arguments.profile,
                    cli.json,
                )
                .await
            }
            None => profiles(&runtime.effective, cli.json),
        },
        Command::Usage(arguments) => match arguments.action {
            Some(UsageAction::Override(arguments)) => {
                usage_override(
                    &repository,
                    cli.config.as_deref(),
                    &runtime.effective,
                    arguments,
                    cli.json,
                )
                .await
            }
            None => {
                usage(
                    &repository,
                    cli.config.as_deref(),
                    &runtime.effective,
                    cli.json,
                )
                .await
            }
        },
        Command::Handover(arguments) => {
            control_handover(&repository, cli.config.as_deref(), arguments, cli.json).await
        }
        Command::Pause(task) => {
            control(
                &repository,
                cli.config.as_deref(),
                task,
                "pause",
                json!({}),
                cli.json,
            )
            .await
        }
        Command::Resume(task) => {
            resume_task(
                &repository,
                cli.config.as_deref(),
                &runtime.effective,
                &task,
                cli.json,
            )
            .await
        }
        Command::Cancel(task) => {
            control(
                &repository,
                cli.config.as_deref(),
                task,
                "cancel",
                json!({}),
                cli.json,
            )
            .await
        }
        Command::ExplainRouting(task) => {
            explain_routing(&repository, cli.config.as_deref(), &task.task_id, cli.json).await
        }
        Command::Checkpoint(task) => {
            checkpoint(&repository, cli.config.as_deref(), &task.task_id, cli.json).await
        }
        Command::Doctor(arguments) => match arguments.action {
            Some(DoctorAction::Providers) => {
                doctor_providers(&runtime.effective, "doctor_providers", cli.json).await
            }
            None => {
                doctor(
                    &repository,
                    &runtime.effective,
                    cli.config.as_deref(),
                    cli.json,
                )
                .await
            }
        },
        Command::Compatibility => {
            doctor_providers(&runtime.effective, "compatibility", cli.json).await
        }
        Command::Migrate(arguments) => migrate(
            &repository,
            &runtime.effective,
            &runtime.explicit_edit_path,
            arguments.action,
            cli.json,
        ),
        Command::Rollback(arguments) => rollback(
            &repository,
            &runtime.effective,
            &runtime.explicit_edit_path,
            arguments.action,
            cli.json,
        ),
        Command::Tui(selector) => {
            Box::pin(tui(
                &repository,
                cli.config.as_deref(),
                environment,
                &runtime,
                &selector,
                cli.json,
            ))
            .await
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn load_config_runtime(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: ConfigEnvironment,
) -> Result<ConfigRuntime> {
    let repository = fs::canonicalize(repository).with_context(|| {
        format!(
            "cannot canonicalize repository directory: {}",
            repository.display()
        )
    })?;
    let cli_config = cli_config.map(|path| resolve_from(&repository, path));
    let effective = load_effective_config(&ConfigRequest {
        repository: &repository,
        cli_config: cli_config.as_deref(),
        environment: environment.clone(),
    })?;
    let explicit_edit_path =
        config_edit_path(&repository, cli_config.as_deref(), &environment, &effective);
    Ok(ConfigRuntime {
        effective,
        explicit_edit_path,
    })
}

fn config_edit_path(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: &ConfigEnvironment,
    effective: &EffectiveConfig,
) -> PathBuf {
    cli_config
        .map(|path| resolve_from(repository, path))
        .or_else(|| environment.colay_config.clone())
        .or_else(|| {
            effective.sources().iter().find_map(|source| {
                matches!(
                    source.kind,
                    ConfigLayerKind::Repository | ConfigLayerKind::LegacyRepository
                )
                .then(|| source.path.clone())
            })
        })
        .unwrap_or_else(|| repository.join(DEFAULT_CONFIG_PATH))
}

fn configured_edit_path(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: &ConfigEnvironment,
) -> PathBuf {
    cli_config
        .map(|path| resolve_from(repository, path))
        .or_else(|| environment.colay_config.clone())
        .unwrap_or_else(|| repository.join(DEFAULT_CONFIG_PATH))
}

fn migration_fallback_path(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: &ConfigEnvironment,
) -> Result<Option<PathBuf>> {
    let current = repository.join(DEFAULT_CONFIG_PATH);
    let legacy = repository.join(LEGACY_CONFIG_PATH);
    let cli_config = cli_config.map(|path| resolve_from(repository, path));
    if config_source_exists(&current)?
        && config_source_exists(&legacy)?
        && !cli_config
            .as_ref()
            .is_some_and(|path| path == &current || path == &legacy)
    {
        return Ok(None);
    }

    let (target, environment_target) = if let Some(path) = cli_config {
        (path, false)
    } else if let Some(path) = environment.colay_config.clone() {
        (path, true)
    } else if config_source_exists(&current)? {
        (current.clone(), false)
    } else if config_source_exists(&legacy)? {
        (legacy.clone(), false)
    } else {
        return Ok(None);
    };
    if !config_source_exists(&target)? {
        return Ok(None);
    }
    let Ok(migratable) = MigratableConfigDocument::load(&target) else {
        return Ok(None);
    };
    if migratable.current_version() >= orchestrator_state::CONFIG_SCHEMA_VERSION {
        return Ok(None);
    }

    let mut preflight_environment = environment.clone();
    if environment_target {
        preflight_environment.colay_config = None;
    }
    let temporary_repository;
    let canonical_temporary_repository;
    let preflight_repository = if target == current || target == legacy {
        temporary_repository = tempfile::tempdir()?;
        canonical_temporary_repository = fs::canonicalize(temporary_repository.path())?;
        &canonical_temporary_repository
    } else {
        repository
    };
    load_effective_config(&ConfigRequest::new(
        preflight_repository,
        preflight_environment,
    ))?;
    Ok(Some(target))
}

fn config_source_exists(path: &Path) -> Result<bool> {
    orchestrator_state::reject_symlink_components(path)?;
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("cannot inspect configuration source: {}", path.display())),
    }
}

fn load_initialization_config_runtime(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: ConfigEnvironment,
) -> Result<ConfigRuntime> {
    let cli_config = cli_config.map(|path| resolve_from(repository, path));
    let explicit_edit_path = configured_edit_path(repository, cli_config.as_deref(), &environment);
    let load_cli = cli_config.as_deref().filter(|path| path.exists());
    let mut load_environment = environment;
    if load_environment
        .colay_config
        .as_deref()
        .is_some_and(|path| !path.exists())
    {
        load_environment.colay_config = None;
    }
    let mut runtime = load_config_runtime(repository, load_cli, load_environment)?;
    runtime.explicit_edit_path = explicit_edit_path;
    Ok(runtime)
}

fn config_environment() -> ConfigEnvironment {
    ConfigEnvironment {
        colay_home: std::env::var_os("COLAY_HOME").map(PathBuf::from),
        user_home: platform_user_home(),
        colay_config: std::env::var_os("COLAY_CONFIG").map(PathBuf::from),
    }
}

#[cfg(windows)]
fn platform_user_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

#[cfg(not(windows))]
fn platform_user_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn configure_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

fn initialize(_repository: &Path, runtime: &ConfigRuntime, json_output: bool) -> Result<()> {
    let config_path = &runtime.explicit_edit_path;
    if config_path.exists() {
        bail!("configuration already exists: {}", config_path.display());
    }
    if runtime
        .effective
        .sources()
        .iter()
        .any(|source| source.kind == ConfigLayerKind::LegacyRepository)
    {
        bail!(
            "legacy Colay configuration already exists; initialization refused to avoid splitting local state (use `--config` explicitly)"
        );
    }
    let document = CONFIG_TEMPLATE.parse::<DocumentMut>()?;
    save_override_atomic(&document, config_path)?;
    let state = GlobalStatePaths::resolve(&StateEnvironment::from_process())?;
    emit(
        json_output,
        "initialized",
        &json!({
            "config": config_path,
            "state_dir": state.root,
            "migration": Value::Null,
            "event_log": Value::Null,
            "provider_inference_started": false,
        }),
    )
}

async fn doctor(
    repository: &Path,
    effective: &EffectiveConfig,
    explicit_config: Option<&Path>,
    json_output: bool,
) -> Result<()> {
    let mut checks = vec![Check::pass("config", "schema and values are valid")];
    let executable = std::env::current_exe()?;
    checks.push(Check::with_data(
        "runtime",
        true,
        json!({
            "version": crate::args::COLAY_VERSION,
            "executable": &executable,
            "invoked_as": std::env::args_os().next(),
            "target_os": std::env::consts::OS,
            "target_arch": std::env::consts::ARCH,
        }),
    ));
    if let Some(warning) = mixed_git_checkout_warning(repository, std::env::consts::OS) {
        checks.push(Check::warn("wsl_mixed_git_checkout", warning));
    }
    doctor_state_checks(repository, explicit_config, &executable, &mut checks).await;
    checks.push(git_health_check(repository));
    for report in collect_provider_reports(effective).await {
        let status = match report.health.status {
            HealthStatus::Healthy => CheckStatus::Pass,
            HealthStatus::Degraded | HealthStatus::Unhealthy | HealthStatus::Unknown => {
                CheckStatus::Warn
            }
        };
        let detail = report
            .health
            .detail
            .clone()
            .unwrap_or_else(|| "safe public provider probes completed".to_owned());
        let resolution = provider_config(&effective.config().orchestrator, report.provider).map(
            |config| async {
                diagnostic_command(
                    &config.executable,
                    ["--version"],
                    repository,
                    &process_redaction(&effective.config().orchestrator),
                )
                .await
            },
        );
        let resolution = match resolution {
            Some(resolution) => resolution
                .await
                .ok()
                .filter(orchestrator_process::ProcessResult::success),
            None => None,
        };
        checks.push(Check::with_status_data(
            format!("provider_{}", report.provider.as_str()),
            status,
            detail,
            json!({
                "provider": report,
                "configured_executable": resolution.as_ref().map(|output| &output.resolved_executable.configured),
                "resolved_executable": resolution.as_ref().map(|output| &output.resolved_executable.path),
                "executable_kind": resolution.as_ref().map(|output| output.resolved_executable.kind),
            }),
        ));
    }

    let passed = checks.iter().all(|check| check.status != CheckStatus::Fail);
    emit(
        json_output,
        "doctor",
        &DoctorReport {
            schema_version: "1",
            passed,
            checks,
            inference_requests: 0,
        },
    )
}

#[allow(clippy::too_many_lines)]
async fn doctor_state_checks(
    repository: &Path,
    explicit_config: Option<&Path>,
    executable: &Path,
    checks: &mut Vec<Check>,
) {
    let paths = match GlobalStatePaths::resolve(&StateEnvironment::from_process()) {
        Ok(paths) => paths,
        Err(error) => {
            checks.push(Check::fail("state", error.to_string()));
            return;
        }
    };
    match crate::daemon::acquire_maintenance() {
        Ok(maintenance) => {
            let database = match Database::open_read_only_snapshot(&maintenance.paths.database) {
                Ok(Some(database)) => database,
                Ok(None) => {
                    checks.push(Check::with_status_data(
                        "state",
                        CheckStatus::Warn,
                        "user-global database does not exist; run a writable command or `colay migrate apply` to create it",
                        json!({
                            "root": maintenance.paths.root,
                            "database": maintenance.paths.database,
                            "backups": maintenance.paths.backups,
                            "current_schema_version": Value::Null,
                            "target_schema_version": orchestrator_state::STATE_SCHEMA_VERSION,
                        }),
                    ));
                    checks.push(daemon_runtime_check(&DaemonStatus::Stopped, executable));
                    push_unavailable_workspace_checks(
                        checks,
                        "user-global database is not initialized",
                    );
                    return;
                }
                Err(error) => {
                    checks.push(Check::fail("state", error.to_string()));
                    return;
                }
            };
            let schema_version = match database.migration_status() {
                Ok(status) => status.current_version,
                Err(error) => {
                    checks.push(Check::fail("state", error.to_string()));
                    return;
                }
            };
            if schema_version != orchestrator_state::STATE_SCHEMA_VERSION {
                checks.push(Check::with_status_data(
                    "state",
                    CheckStatus::Warn,
                    format!(
                        "database schema {schema_version} requires explicit `colay migrate apply` to reach {}",
                        orchestrator_state::STATE_SCHEMA_VERSION
                    ),
                    json!({
                        "root": maintenance.paths.root,
                        "database": maintenance.paths.database,
                        "backups": maintenance.paths.backups,
                        "current_schema_version": schema_version,
                        "target_schema_version": orchestrator_state::STATE_SCHEMA_VERSION,
                    }),
                ));
                checks.push(daemon_runtime_check(&DaemonStatus::Stopped, executable));
                push_unavailable_workspace_checks(
                    checks,
                    "workspace integrity requires explicit database migration",
                );
                return;
            }
            let health = match database.health() {
                Ok(health) => health,
                Err(error) => {
                    checks.push(Check::fail("state", error.to_string()));
                    return;
                }
            };
            let state_healthy = health.integrity_ok
                && health.foreign_key_violations == 0
                && health.current_schema_version == orchestrator_state::STATE_SCHEMA_VERSION;
            checks.push(Check::with_data(
                "state",
                state_healthy,
                json!({
                    "root": maintenance.paths.root,
                    "database": maintenance.paths.database,
                    "backups": maintenance.paths.backups,
                    "health": health,
                }),
            ));
            checks.push(daemon_runtime_check(
                &database
                    .daemon_status(Utc::now())
                    .unwrap_or(DaemonStatus::Stopped),
                executable,
            ));
            let registration = match database.find_repository_workspace(repository) {
                Ok(Some(registration)) => registration,
                Ok(None) => {
                    checks.push(Check::warn(
                        "workspace",
                        "current repository is not registered; run a writable command to register it",
                    ));
                    checks.push(Check::warn(
                        "audit",
                        "workspace audit is unavailable until the repository is registered",
                    ));
                    checks.push(Check::warn(
                        "artifacts",
                        "workspace artifacts are unavailable until the repository is registered",
                    ));
                    return;
                }
                Err(error) => {
                    checks.push(Check::fail("workspace", error.to_string()));
                    return;
                }
            };
            checks.push(Check::with_data("workspace", true, json!(registration)));
            let workspace = database.workspace(registration.workspace_id);
            checks.push(match verify_workspace_audit(&workspace) {
                Ok(data) => Check::with_data("audit", true, data),
                Err(error) => Check::fail("audit", error.to_string()),
            });
            let workspace_paths = maintenance.paths.for_workspace(registration.workspace_id);
            checks.push(
                match verify_workspace_artifacts(&workspace, &workspace_paths) {
                    Ok(data) => Check::with_data("artifacts", true, data),
                    Err(error) => Check::fail("artifacts", error.to_string()),
                },
            );
        }
        Err(lock_error) => {
            let client = match crate::ipc_client::DaemonClient::connect_with_config(
                repository,
                explicit_config,
            )
            .await
            {
                Ok(client) => client,
                Err(ipc_error) => {
                    checks.push(Check::fail(
                        "state",
                        format!(
                            "cannot acquire maintenance ownership ({lock_error}) or query the user daemon ({ipc_error})"
                        ),
                    ));
                    return;
                }
            };
            let diagnostics = match client.request("workspace.doctor", json!({})).await {
                Ok(diagnostics) => diagnostics,
                Err(error) => {
                    checks.push(Check::fail("state", error.to_string()));
                    return;
                }
            };
            append_live_doctor_checks(
                checks,
                &diagnostics,
                &paths,
                client.workspace_id(),
                repository,
                executable,
            );
        }
    }
}

fn append_live_doctor_checks(
    checks: &mut Vec<Check>,
    diagnostics: &orchestrator_daemon::IpcResponse,
    paths: &GlobalStatePaths,
    expected_workspace_id: orchestrator_state::WorkspaceId,
    expected_repository: &Path,
    executable: &Path,
) {
    let parsed = diagnostics
        .outcome
        .get("data")
        .cloned()
        .ok_or_else(|| anyhow!("workspace doctor response omitted data"))
        .and_then(|data| {
            serde_json::from_value::<orchestrator_daemon::WorkspaceDoctorDiagnostics>(data)
                .context("workspace doctor response is malformed")
        })
        .and_then(|parsed| {
            parsed
                .validate(
                    expected_workspace_id,
                    expected_repository,
                    &paths.for_workspace(expected_workspace_id).root,
                )
                .map_err(anyhow::Error::from)?;
            Ok(parsed)
        });
    let diagnostics = match parsed {
        Ok(diagnostics) => diagnostics,
        Err(error) => {
            let detail = format!("invalid workspace doctor IPC response: {error}");
            for name in ["state", "daemon", "workspace", "audit", "artifacts"] {
                checks.push(Check::fail(name, &detail));
            }
            return;
        }
    };
    let health = &diagnostics.database;
    let healthy = health.integrity_ok
        && health.foreign_key_violations == 0
        && health.current_schema_version == orchestrator_state::STATE_SCHEMA_VERSION;
    checks.push(Check::with_data(
        "state",
        healthy,
        json!({
            "root": paths.root,
            "database": paths.database,
            "backups": paths.backups,
            "health": &diagnostics.database,
            "via": "daemon_ipc",
        }),
    ));
    checks.push(daemon_runtime_check(&diagnostics.daemon, executable));
    checks.push(Check::with_data(
        "workspace",
        true,
        json!(diagnostics.workspace),
    ));
    checks.push(Check::with_data("audit", true, json!(diagnostics.audit)));
    checks.push(Check::with_data(
        "artifacts",
        true,
        json!(diagnostics.artifacts),
    ));
}

fn push_unavailable_workspace_checks(checks: &mut Vec<Check>, detail: &str) {
    checks.push(Check::warn("workspace", detail));
    checks.push(Check::warn("audit", detail));
    checks.push(Check::warn("artifacts", detail));
}

fn verify_workspace_audit(database: &WorkspaceDatabase<'_>) -> Result<Value> {
    let head = database.latest_outbox_sequence()?;
    if head < 0 {
        bail!("workspace event head is negative");
    }
    let mut previous_hash = None;
    for sequence in 1..=head {
        let event = database
            .event_at(sequence)?
            .ok_or_else(|| anyhow!("workspace event sequence {sequence} is missing"))?;
        if event.sequence != u64::try_from(sequence)? || event.previous_hash != previous_hash {
            bail!("workspace event chain diverges at sequence {sequence}");
        }
        if !event.verify_hash()? {
            bail!("workspace event hash is invalid at sequence {sequence}");
        }
        previous_hash = Some(event.event_hash);
    }
    Ok(json!({
        "workspace_id": database.workspace_id(),
        "verified_events": head,
        "last_sequence": head,
        "last_hash": previous_hash,
    }))
}

fn verify_workspace_artifacts(
    database: &WorkspaceDatabase<'_>,
    paths: &orchestrator_state::WorkspaceStatePaths,
) -> Result<Value> {
    let artifacts = database.list_artifacts()?;
    if artifacts.is_empty() {
        return Ok(json!({
            "root": paths.root,
            "verified_references": 0,
            "scope": "persisted_references",
        }));
    }
    let store = ArtifactStore::open_workspace_read_only(paths)?;
    for artifact in &artifacts {
        let _ = store.read_verified(artifact)?;
    }
    Ok(json!({
        "root": store.root(),
        "verified_references": artifacts.len(),
        "scope": "persisted_references",
    }))
}

fn git_health_check(repository: &Path) -> Check {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(repository)
        .output();
    match output {
        Ok(output) if output.status.success() => Check::with_data(
            "git",
            true,
            json!({"top_level": String::from_utf8_lossy(&output.stdout).trim()}),
        ),
        Ok(output) => Check::warn(
            "git",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ),
        Err(error) => Check::warn("git", format!("Git is unavailable: {error}")),
    }
}

fn daemon_runtime_check(daemon_status: &DaemonStatus, executable: &Path) -> Check {
    let (status, detail) = match daemon_status {
        DaemonStatus::Online(instance)
        | DaemonStatus::Booting(instance)
        | DaemonStatus::Probing(instance) => {
            let version_matches =
                instance.build_version.as_deref() == Some(crate::args::COLAY_VERSION);
            let executable_matches =
                instance
                    .executable_path
                    .as_deref()
                    .is_some_and(|daemon_executable| {
                        let daemon = Path::new(daemon_executable);
                        if cfg!(windows) {
                            daemon
                                .to_string_lossy()
                                .eq_ignore_ascii_case(&executable.to_string_lossy())
                        } else {
                            daemon == executable
                        }
                    });
            if version_matches && executable_matches {
                (
                    CheckStatus::Pass,
                    "daemon runtime matches this CLI".to_owned(),
                )
            } else {
                (
                    CheckStatus::Warn,
                    "daemon executable or build version differs from this CLI; restart the user daemon with the intended Colay binary".to_owned(),
                )
            }
        }
        DaemonStatus::Stopped => (
            CheckStatus::Warn,
            "user daemon is stopped; state was checked under exclusive maintenance ownership"
                .to_owned(),
        ),
        DaemonStatus::Failed(_) | DaemonStatus::Stale(_) => (
            CheckStatus::Warn,
            "user daemon is failed or stale; restart it before writable execution".to_owned(),
        ),
    };
    Check::with_status_data(
        "daemon",
        status,
        detail,
        json!({
            "current_executable": executable,
            "current_version": crate::args::COLAY_VERSION,
            "status": daemon_status,
        }),
    )
}

async fn doctor_providers(
    effective: &EffectiveConfig,
    command: &str,
    json_output: bool,
) -> Result<()> {
    emit_versioned(
        json_output,
        "2",
        command,
        &DoctorProvidersReport {
            schema_version: "2",
            providers: collect_provider_reports(effective).await,
            inference_requests: 0,
        },
    )
}

async fn providers(effective: &EffectiveConfig, json_output: bool) -> Result<()> {
    emit(
        json_output,
        "providers",
        &collect_provider_reports(effective).await,
    )
}

async fn collect_provider_reports(effective: &EffectiveConfig) -> Vec<ProviderReport> {
    let redaction = process_redaction(&effective.config().orchestrator);
    let mut reports = Vec::new();
    for (provider, config) in provider_configs(&effective.config().orchestrator) {
        let diagnostic = probe_provider(provider, config, &redaction).await;
        reports.push(match diagnostic {
            Ok((health, capabilities)) => ProviderReport {
                provider,
                enabled: config.enabled,
                health,
                capabilities,
            },
            Err(error) => ProviderReport {
                provider,
                enabled: config.enabled,
                health: ProviderHealth {
                    provider,
                    status: HealthStatus::Unhealthy,
                    checked_at: Utc::now(),
                    latency_ms: None,
                    consecutive_failures: 1,
                    detail: Some(error.to_string()),
                },
                capabilities: ProviderCapabilities::unsupported(provider),
            },
        });
    }
    reports
}

async fn set_provider_enabled(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: ConfigEnvironment,
    runtime: &ConfigRuntime,
    provider: ProviderId,
    enabled: bool,
    json_output: bool,
) -> Result<()> {
    let mut document = load_edit_document(&runtime.explicit_edit_path)?;
    let orchestrator = ensure_override_table(document.as_table_mut(), "orchestrator")?;
    let providers = ensure_override_table(orchestrator, "providers")?;
    let provider_override = ensure_override_table(providers, provider.as_str())?;
    provider_override.insert("enabled", toml_edit::value(enabled));
    write_override_via_daemon(
        repository,
        cli_config,
        &document,
        &runtime.explicit_edit_path,
    )
    .await?;
    let reloaded = load_config_runtime(repository, cli_config, environment)?;
    let persisted = provider_config(&reloaded.effective.config().orchestrator, provider)
        .is_some_and(|config| config.enabled == enabled);
    if !persisted {
        bail!("provider override did not survive effective configuration reload");
    }
    emit(
        json_output,
        "provider_updated",
        &json!({"provider": provider, "enabled": enabled}),
    )
}

fn profiles(effective: &EffectiveConfig, json_output: bool) -> Result<()> {
    let defaults = RootConfig::default();
    let rows = effective_profile_rows(effective.config(), &defaults)?;
    emit(json_output, "profiles", &rows)
}

fn selected_profile_row(
    config: &RootConfig,
    provider: ProviderName,
    profile: ProfileName,
) -> Result<ProfileReportRow> {
    let provider_id = ProviderId::from(provider);
    effective_profile_rows(config, &RootConfig::default())?
        .into_iter()
        .find(|row| row.provider == provider_id.as_str() && row.profile == profile.as_str())
        .ok_or_else(|| anyhow!("effective profile disappeared after configuration reload"))
}

#[allow(clippy::too_many_arguments)]
async fn set_model_profile(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: ConfigEnvironment,
    runtime: &ConfigRuntime,
    provider: ProviderName,
    profile: ProfileName,
    model: &str,
    effort: Option<EffortName>,
    json_output: bool,
) -> Result<()> {
    let mut document = load_edit_document(&runtime.explicit_edit_path)?;
    let provider_id = ProviderId::from(provider);
    set_profile_override(
        &mut document,
        provider_id.as_str(),
        profile.as_str(),
        model,
        effort.map(EffortName::as_str),
    )?;
    write_override_via_daemon(
        repository,
        cli_config,
        &document,
        &runtime.explicit_edit_path,
    )
    .await?;
    let reloaded = load_config_runtime(repository, cli_config, environment)?;
    let row = selected_profile_row(reloaded.effective.config(), provider, profile)?;
    if row.model != model.trim() {
        bail!("model profile override did not survive effective configuration reload");
    }
    if effort.is_some_and(|value| row.effort.as_deref() != Some(value.as_str())) {
        bail!("model profile effort did not survive effective configuration reload");
    }
    emit(json_output, "profile_updated", &row)
}

async fn reset_model_profile(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: ConfigEnvironment,
    runtime: &ConfigRuntime,
    provider: ProviderName,
    profile: ProfileName,
    json_output: bool,
) -> Result<()> {
    let mut document = load_edit_document(&runtime.explicit_edit_path)?;
    let provider_id = ProviderId::from(provider);
    if !reset_profile_override(&mut document, provider_id.as_str(), profile.as_str())? {
        bail!("selected writable layer has no override for this model profile");
    }
    write_override_via_daemon(
        repository,
        cli_config,
        &document,
        &runtime.explicit_edit_path,
    )
    .await?;
    let reloaded = load_config_runtime(repository, cli_config, environment)?;
    let row = selected_profile_row(reloaded.effective.config(), provider, profile)?;
    emit(json_output, "profile_reset", &row)
}

fn load_edit_document(path: &Path) -> Result<DocumentMut> {
    if path.exists() {
        orchestrator_state::reject_symlink_components(path)?;
        fs::read_to_string(path)
            .with_context(|| format!("cannot read configuration override: {}", path.display()))?
            .parse::<DocumentMut>()
            .map_err(Into::into)
    } else {
        CONFIG_TEMPLATE.parse::<DocumentMut>().map_err(Into::into)
    }
}

fn ensure_override_table<'a>(
    parent: &'a mut dyn TableLike,
    key: &str,
) -> Result<&'a mut dyn TableLike> {
    if !parent.contains_key(key) {
        parent.insert(key, Item::Table(Table::new()));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| anyhow!("configuration override `{key}` must be a table"))
}

async fn write_override_via_daemon(
    repository: &Path,
    explicit_config: Option<&Path>,
    document: &DocumentMut,
    path: &Path,
) -> Result<()> {
    #[cfg(test)]
    {
        let _ = (repository, explicit_config);
        std::future::ready(()).await;
        save_override_atomic(document, path)
    }
    #[cfg(not(test))]
    {
        let client =
            crate::ipc_client::DaemonClient::connect_or_start(repository, explicit_config).await?;
        client
            .request(
                "workspace.config.write",
                json!({"path": path, "content": document.to_string()}),
            )
            .await?;
        Ok(())
    }
}

fn save_override_atomic(document: &DocumentMut, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent: {}", path.display()))?;
    orchestrator_state::ensure_private_directory(parent)?;
    orchestrator_state::reject_symlink_components(path)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("cannot create temporary config in {}", parent.display()))?;
    temporary.write_all(document.to_string().as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot atomically replace config: {}", path.display()))?;
    orchestrator_state::ensure_private_file(path)?;
    sync_override_directory(parent)
}

#[cfg(windows)]
fn sync_override_directory(path: &Path) -> Result<()> {
    fs::metadata(path)?;
    Ok(())
}

#[cfg(not(windows))]
fn sync_override_directory(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

async fn run_conversation(
    repository: &Path,
    explicit_config: Option<&Path>,
    effective: &EffectiveConfig,
    arguments: RunArgs,
    json_output: bool,
) -> Result<()> {
    let document = effective.document();
    if !document.config().orchestrator.enabled || !document.config().features.orchestrator {
        bail!("orchestrator execution is disabled by configuration");
    }
    let structured_input = arguments.task_file.is_some();
    let input = load_task_input(&arguments)?;
    let requested_provider = arguments.provider.map(ProviderId::from);
    let content = conversation_requirement_input(&input, structured_input, requested_provider)?;
    let client =
        crate::ipc_client::DaemonClient::connect_or_start(repository, explicit_config).await?;
    let session_id = ensure_run_session(&client).await?;
    let message_id = MessageId::new();
    let command = pending_client_command(
        Some(session_id),
        ClientCommandAction::AppendMessage,
        serde_json::to_value(AppendMessageCommandPayload {
            message_id,
            content,
        })?,
        format!("cli-run-message-{message_id}"),
        RUN_REQUESTED_BY,
    );
    client
        .request(
            "workspace.run.submit",
            json!({"command": command, "plan_only": arguments.plan_only}),
        )
        .await?;
    wait_for_client_command(&client, command.command_id).await?;
    let conversation_command_id = ClientCommandId::from_uuid(message_id.into_uuid());
    wait_for_client_command(&client, conversation_command_id).await?;
    let response = client
        .request("workspace.conversation", json!({"session_id": session_id}))
        .await?;
    emit(
        json_output,
        "run_conversation",
        &json!({
            "session_id": session_id,
            "source_message_id": message_id,
            "plan_only": arguments.plan_only,
            "requested_provider": requested_provider,
            "conversation": response.outcome["data"],
        }),
    )
}

fn conversation_requirement_input(
    input: &TaskInput,
    structured_input: bool,
    requested_provider: Option<ProviderId>,
) -> Result<String> {
    if !structured_input && requested_provider.is_none() {
        return Ok(input.original_request.clone());
    }
    Ok(serde_json::to_string_pretty(&json!({
        "original_request": input.original_request,
        "objective": input.objective,
        "constraints": input.constraints,
        "acceptance_criteria": input.acceptance_criteria,
        "allowed_write_paths": input.allowed_write_paths,
        "repository_wide_write_scope": input.repository_wide_write_scope,
        "requested_provider": requested_provider,
    }))?)
}

async fn ensure_run_session(client: &crate::ipc_client::DaemonClient) -> Result<SessionId> {
    let command =
        if let Some(command) = load_client_command(client, None, Some(RUN_SESSION_KEY)).await? {
            command
        } else {
            let session_id = SessionId::new();
            let command = pending_client_command(
                None,
                ClientCommandAction::CreateSession,
                serde_json::to_value(CreateSessionCommandPayload {
                    session_id,
                    title: "Colay plan conversation".to_owned(),
                })?,
                RUN_SESSION_KEY.to_owned(),
                RUN_REQUESTED_BY,
            );
            match client
                .request("workspace.command.submit", serde_json::to_value(&command)?)
                .await
            {
                Ok(_) => command,
                Err(submit_error) => load_client_command(client, None, Some(RUN_SESSION_KEY))
                    .await?
                    .ok_or(submit_error)?,
            }
        };
    let payload: CreateSessionCommandPayload = serde_json::from_value(command.payload.clone())
        .context("stored run session command has an invalid payload")?;
    wait_for_client_command(client, command.command_id).await?;
    Ok(payload.session_id)
}

fn pending_client_command(
    session_id: Option<SessionId>,
    action: ClientCommandAction,
    payload: Value,
    idempotency_key: String,
    requested_by: &str,
) -> ClientCommand {
    ClientCommand {
        command_id: ClientCommandId::new(),
        session_id,
        task_id: None,
        action,
        payload,
        idempotency_key,
        state: ClientCommandState::Pending,
        requested_by: requested_by.to_owned(),
        requested_at: Utc::now(),
        claimed_at: None,
        completed_at: None,
        outcome: None,
    }
}

async fn wait_for_client_command(
    client: &crate::ipc_client::DaemonClient,
    command_id: ClientCommandId,
) -> Result<ClientCommand> {
    let started = Instant::now();
    let mut poll_interval = RUN_COMMAND_POLL_INTERVAL;
    loop {
        if let Some(command) = load_client_command(client, Some(command_id), None).await? {
            match command.state {
                ClientCommandState::Completed => return Ok(command),
                ClientCommandState::Failed => bail!(
                    "user daemon rejected {:?}: {}",
                    command.action,
                    command.outcome.as_deref().unwrap_or("unknown failure")
                ),
                ClientCommandState::Pending | ClientCommandState::Claimed => {}
            }
        }
        if started.elapsed() >= RUN_COMMAND_TIMEOUT {
            bail!("user daemon did not complete command {command_id} within two minutes");
        }
        tokio::time::sleep(poll_interval).await;
        poll_interval = next_run_command_poll_interval(poll_interval);
    }
}

fn next_run_command_poll_interval(current: Duration) -> Duration {
    current.saturating_mul(2).min(RUN_COMMAND_MAX_POLL_INTERVAL)
}

async fn load_client_command(
    client: &crate::ipc_client::DaemonClient,
    command_id: Option<ClientCommandId>,
    idempotency_key: Option<&str>,
) -> Result<Option<ClientCommand>> {
    let response = client
        .request(
            "workspace.command.status",
            json!({
                "command_id": command_id,
                "idempotency_key": idempotency_key,
            }),
        )
        .await?;
    serde_json::from_value(response.outcome["data"]["command"].clone()).map_err(Into::into)
}

#[allow(clippy::too_many_lines)]
async fn resume_task(
    repository: &Path,
    explicit_config: Option<&Path>,
    effective: &EffectiveConfig,
    task: &RequiredTask,
    json_output: bool,
) -> Result<()> {
    let document = effective.document();
    if !document.config().orchestrator.enabled || !document.config().features.orchestrator {
        bail!("orchestrator execution is disabled by configuration");
    }
    let task_id = TaskId::from_str(&task.task_id)?;
    let client =
        crate::ipc_client::DaemonClient::connect_or_start(repository, explicit_config).await?;
    let response = client
        .request("workspace.resume", json!({"task_id": task_id}))
        .await?;
    let mut data = response.outcome["data"].clone();
    if data["disposition"].as_str() == Some("attached") {
        let cursor = data["cursor"].as_u64().unwrap_or(0);
        let mut stream = client
            .stream(
                "workspace.task.stream",
                json!({"task_id": task_id, "cursor": cursor}),
            )
            .await?;
        let mut updates = Vec::new();
        loop {
            let status = tokio::time::timeout(Duration::from_secs(10), stream.next())
                .await
                .context("timed out attaching to the active task status stream")??;
            let Some(status) = status else {
                break;
            };
            if status.outcome["status"].as_str() != Some("ok") {
                bail!(
                    "{}",
                    status.outcome["error"]
                        .as_str()
                        .unwrap_or("active task status stream failed")
                );
            }
            updates.push(status.outcome["data"].clone());
        }
        let latest = updates
            .last()
            .cloned()
            .ok_or_else(|| anyhow!("user daemon closed the active task status stream"))?;
        data["active_status"] = latest;
        data["active_status_updates"] = json!(updates);
    }
    emit(json_output, "resume", &data)
}

#[allow(dead_code)]
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::needless_borrow
)]
async fn resume_task_coordinated(
    repository: &Path,
    document: &ConfigDocument,
    state: &StatePaths,
    global_database: &Database,
    database: &WorkspaceDatabase<'_>,
    task_id: TaskId,
    correlation_id: CorrelationId,
    mut stored: orchestrator_state::StoredTask,
    coordinator_lease_id: Uuid,
    json_output: bool,
) -> Result<()> {
    let recovered_controls = database.recover_claimed_controls(
        task_id,
        Utc::now(),
        orchestrator_state::ClaimedControlRecoveryPolicy::default(),
    )?;
    if recovered_controls.iter().any(|control| {
        control.disposition != orchestrator_state::ControlRecoveryDisposition::Requeued
    }) {
        bail!(
            "task has a claimed control owned by another coordinator or requiring manual reconciliation"
        );
    }
    let envelope: TaskEnvelope = serde_json::from_value(stored.envelope.clone())?;
    let assessment = envelope
        .assessment
        .clone()
        .ok_or_else(|| anyhow!("persisted task has no assessment"))?;

    if stored.state == TaskState::Queued {
        transition_task(
            &database,
            task_id,
            TaskState::Analyzing,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "restart resumed a task before analysis projection completed",
        )?;
        stored = database
            .load_task(task_id)?
            .ok_or_else(|| anyhow!("task disappeared during pre-execution recovery"))?;
    }

    if stored.paused {
        let resume_state = stored
            .resume_state
            .ok_or_else(|| anyhow!("paused task has no safe resume point"))?;
        let event = state_transition_event(
            task_id,
            TaskState::Blocked,
            resume_state,
            correlation_id,
            "user resumed a safely checkpointed task",
            json!({"paused": false}),
        );
        database.resume_task_with_event(task_id, stored.revision, Utc::now(), event)?;
        stored = database
            .load_task(task_id)?
            .ok_or_else(|| anyhow!("task disappeared during resume"))?;
    } else if stored.state == TaskState::Blocked {
        let resume_state = stored
            .resume_state
            .ok_or_else(|| anyhow!("blocked task has no recorded resume point"))?;
        transition_task(
            &database,
            task_id,
            resume_state,
            orchestrator_domain::TransitionGuards {
                resume_point: Some(resume_state),
                ..orchestrator_domain::TransitionGuards::default()
            },
            correlation_id,
            "retrying a blocked task from its recorded resume point",
        )?;
        stored = database
            .load_task(task_id)?
            .ok_or_else(|| anyhow!("task disappeared during resume"))?;
    }

    let stored_worktree = database.active_worktree(task_id)?;
    let mut worktree = stored_worktree
        .map(|stored| validate_recovered_worktree(&repository, &state, stored))
        .transpose()?;
    let mut initial_handover = None;
    let mut handover_already_acknowledged = false;
    let mut checkpoint = database.latest_sealed_checkpoint(task_id)?;

    if stored.state == TaskState::HandoverRequested && database.latest_handover(task_id)?.is_some()
    {
        transition_task(
            database,
            task_id,
            TaskState::HandingOver,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "recovered a persisted handover bundle after projection interruption",
        )?;
        stored = database
            .load_task(task_id)?
            .ok_or_else(|| anyhow!("task disappeared during handover recovery"))?;
    }

    if stored.state == TaskState::HandingOver {
        if let Some(handover) = database.latest_handover(task_id)? {
            if let Some(acknowledgement) = handover.acknowledgement.as_ref() {
                HandoverManager::validate_acknowledgement(&handover.bundle, acknowledgement)?;
                transition_task(
                    database,
                    task_id,
                    TaskState::Resuming,
                    orchestrator_domain::TransitionGuards {
                        handover_integrity_verified: true,
                        handover_acknowledged: true,
                        ..orchestrator_domain::TransitionGuards::default()
                    },
                    correlation_id,
                    "recovered a completed handover acknowledgement",
                )?;
                transition_task(
                    database,
                    task_id,
                    TaskState::Running,
                    orchestrator_domain::TransitionGuards::default(),
                    correlation_id,
                    "resuming writable work after recovered acknowledgement",
                )?;
                handover_already_acknowledged = true;
            }
            initial_handover = Some(handover.bundle);
            stored = database
                .load_task(task_id)?
                .ok_or_else(|| anyhow!("task disappeared after handover recovery"))?;
        } else {
            let sealed = checkpoint
                .as_ref()
                .ok_or_else(|| anyhow!("handing-over task has neither bundle nor checkpoint"))?;
            transition_task(
                database,
                task_id,
                TaskState::Checkpointed,
                orchestrator_domain::TransitionGuards {
                    checkpoint_integrity_verified: sealed.verify_integrity()?,
                    ..orchestrator_domain::TransitionGuards::default()
                },
                correlation_id,
                "rolled an interrupted pre-persistence handover back to its checkpoint",
            )?;
            stored = database
                .load_task(task_id)?
                .ok_or_else(|| anyhow!("task disappeared after handover rollback"))?;
        }
    } else if stored.state == TaskState::Resuming {
        let handover = database
            .latest_handover(task_id)?
            .filter(|handover| handover.acknowledgement.is_some())
            .ok_or_else(|| anyhow!("resuming task has no completed handover acknowledgement"))?;
        HandoverManager::validate_acknowledgement(
            &handover.bundle,
            handover
                .acknowledgement
                .as_ref()
                .ok_or_else(|| anyhow!("completed handover acknowledgement disappeared"))?,
        )?;
        transition_task(
            database,
            task_id,
            TaskState::Running,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "recovered an interrupted post-acknowledgement resume",
        )?;
        initial_handover = Some(handover.bundle);
        handover_already_acknowledged = true;
        stored = database
            .load_task(task_id)?
            .ok_or_else(|| anyhow!("task disappeared after resume recovery"))?;
    } else if !matches!(stored.state, TaskState::Planned | TaskState::Analyzing) {
        let recovered_worktree = worktree
            .as_ref()
            .ok_or_else(|| anyhow!("resumable task has no active isolated worktree"))?;
        checkpoint = Some(
            recover_checkpoint_if_needed(
                &database,
                &state,
                &repository,
                recovered_worktree,
                &envelope,
                stored.state,
                correlation_id,
            )
            .await?,
        );
        stored = database
            .load_task(task_id)?
            .ok_or_else(|| anyhow!("task disappeared after recovery checkpoint"))?;
    }

    let requested_provider = initial_handover
        .as_ref()
        .map(|bundle| bundle.recommended_next_worker);
    let candidates = routing_candidates(
        &document.config().orchestrator,
        global_database,
        &database,
        &assessment,
        requested_provider,
        task_id,
        correlation_id,
    )
    .await?;
    let routing = RoutingEngine::route(
        &RoutingContext {
            task_id,
            assessment: assessment.clone(),
            role: infer_role(&envelope.objective),
            writable: true,
            candidates,
            current_provider: checkpoint.as_ref().map(|value| value.current_worker),
            implementation_provider: checkpoint.as_ref().map(|value| value.current_worker),
            manually_requested_provider: requested_provider,
            conserve_budget: true,
        },
        &RoutingConfig {
            max_parallel_workers: 1,
            allow_amber: true,
        },
        Utc::now(),
    )?;
    persist_routing(&database, &routing, &envelope)?;
    let Some(selected_provider) = routing.selected_provider else {
        if stored.state != TaskState::Blocked {
            transition_task(
                &database,
                task_id,
                TaskState::Blocked,
                orchestrator_domain::TransitionGuards::default(),
                correlation_id,
                "no provider can safely resume the task",
            )?;
        }
        reconcile_events(&state, &database)?;
        return emit(
            json_output,
            "resume_blocked",
            &json!({"task_id": task_id, "routing": routing}),
        );
    };

    if stored.state == TaskState::Analyzing {
        transition_task(
            &database,
            task_id,
            TaskState::Planned,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "restart routing selected a provider before worktree creation",
        )?;
        stored = database
            .load_task(task_id)?
            .ok_or_else(|| anyhow!("task disappeared after recovered routing"))?;
    }

    if initial_handover.is_none() && stored.state != TaskState::Planned {
        let checkpoint = checkpoint
            .as_ref()
            .ok_or_else(|| anyhow!("resume requires a sealed checkpoint"))?;
        if stored.state != TaskState::Checkpointed {
            bail!("recovery did not reach a checkpointed safe boundary");
        }
        transition_task(
            &database,
            task_id,
            TaskState::HandoverRequested,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "restart resume requested a vendor-neutral handover",
        )?;
        let bundle = HandoverManager::create(HandoverInput {
            checkpoint: checkpoint.clone(),
            original_request: envelope.original_request_redacted.clone(),
            constraints: envelope.constraints.clone(),
            acceptance_criteria: envelope.acceptance_criteria.clone(),
            recommended_next_worker: selected_provider,
            usage_snapshots: latest_usage_snapshots(
                global_database,
                &document.config().orchestrator,
            )?,
            safe_boundary_confirmed: true,
            created_at: Utc::now(),
        })?;
        database.record_handover(checkpoint.checkpoint_id, "restart recovery", &bundle, None)?;
        transition_task(
            database,
            task_id,
            TaskState::HandingOver,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "restart resume persisted a sealed handover bundle",
        )?;
        initial_handover = Some(bundle);
    }

    if initial_handover.is_some() && worktree.is_none() {
        bail!("handover resume requires the persisted isolated worktree");
    }

    reconcile_events(&state, &database)?;
    execute_planned_task(
        &repository,
        &state,
        document.config(),
        global_database,
        database,
        PlannedTask {
            task: envelope.clone(),
            routing,
            plan_only: false,
        },
        correlation_id,
        envelope.original_request_redacted.clone(),
        json_output,
        worktree.take(),
        initial_handover,
        handover_already_acknowledged,
        coordinator_lease_id,
    )
    .await
}

fn validate_recovered_worktree(
    repository: &Path,
    state: &StatePaths,
    stored: orchestrator_state::StoredWorktree,
) -> Result<GitWorktree> {
    let repository = canonicalize_directory(repository)?;
    let recorded_repository = canonicalize_directory(&stored.repo_root)?;
    let worktrees_root = canonicalize_directory(&state.worktrees)?;
    let worktree_path = canonicalize_directory(&stored.worktree_path)?;
    if recorded_repository != repository || !worktree_path.starts_with(worktrees_root) {
        bail!("persisted worktree escaped its repository or state trust boundary");
    }
    Ok(GitWorktree {
        task_id: stored.task_id,
        repository_root: recorded_repository,
        path: worktree_path,
        branch: stored.branch_name,
        base_revision: stored.base_revision,
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn recover_checkpoint_if_needed(
    database: &WorkspaceDatabase<'_>,
    state: &StatePaths,
    repository: &Path,
    worktree: &GitWorktree,
    task: &TaskEnvelope,
    mut current: TaskState,
    correlation_id: CorrelationId,
) -> Result<orchestrator_domain::Checkpoint> {
    if current == TaskState::Checkpointed {
        return database
            .latest_sealed_checkpoint(task.task_id)?
            .ok_or_else(|| anyhow!("checkpointed task has no sealed checkpoint"));
    }
    if current == TaskState::HandoverRequested {
        transition_task(
            database,
            task.task_id,
            TaskState::Checkpointed,
            orchestrator_domain::TransitionGuards {
                checkpoint_integrity_verified: true,
                ..orchestrator_domain::TransitionGuards::default()
            },
            correlation_id,
            "recovered an incomplete handover request",
        )?;
        return database
            .latest_sealed_checkpoint(task.task_id)?
            .ok_or_else(|| anyhow!("handover request has no sealed checkpoint"));
    }
    if current == TaskState::Resuming {
        transition_task(
            database,
            task.task_id,
            TaskState::Running,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "recovered an interrupted resume transition",
        )?;
        current = TaskState::Running;
    }
    if matches!(current, TaskState::Running | TaskState::Verifying) {
        transition_task(
            database,
            task.task_id,
            TaskState::CheckpointRequested,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "restart recovery requested an authoritative checkpoint",
        )?;
        current = TaskState::CheckpointRequested;
    }
    if current == TaskState::CheckpointRequested {
        transition_task(
            database,
            task.task_id,
            TaskState::Checkpointing,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "restart recovery is capturing Git evidence",
        )?;
        current = TaskState::Checkpointing;
    }
    if current != TaskState::Checkpointing {
        bail!("task state {current:?} cannot be safely recovered automatically");
    }

    let attempt = database
        .latest_task_attempt(task.task_id)?
        .ok_or_else(|| anyhow!("running task has no persisted worker attempt"))?;
    let provider = attempt
        .provider
        .ok_or_else(|| anyhow!("latest worker attempt has no provider"))?;
    let worker_result = attempt
        .worker_result
        .and_then(|value| serde_json::from_value::<WorkerResult>(value).ok());
    let worktrees = GitWorktreeManager::open(repository, &state.worktrees)?;
    let snapshot = worktrees.snapshot(worktree).await?;
    let persistence_scan =
        VerificationEngine::new()?.preflight_persistence(&worktree.path, &snapshot)?;
    if !persistence_scan.safe_to_persist_or_share() {
        transition_task(
            database,
            task.task_id,
            TaskState::Blocked,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "restart checkpoint blocked by secret-scan policy",
        )?;
        bail!(
            "restart checkpoint contains potential secrets or unscanned large files; administrator review is required"
        );
    }
    let checkpoint_plan = vendor_neutral_plan(task);
    let checkpoint = CheckpointManager::new(ArtifactStore::open(&state.root)?).create(
        CheckpointInput {
            task_id: task.task_id,
            attempt_id: attempt.attempt_id,
            objective: task.objective.clone(),
            current_plan: checkpoint_plan.clone(),
            completed_steps: Vec::new(),
            pending_steps: checkpoint_plan,
            files_read: Vec::new(),
            commands_run: worker_result
                .as_ref()
                .map_or_else(Vec::new, |result| result.commands.clone()),
            tests: worker_result
                .as_ref()
                .map_or_else(Vec::new, |result| result.tests.clone()),
            decisions: vec![DecisionRecord {
                decision: "recover from a persisted safe boundary".to_owned(),
                rationale: "the prior orchestrator process did not record a terminal attempt"
                    .to_owned(),
                alternatives: vec!["leave the task blocked for manual recovery".to_owned()],
                decided_by: provider,
                decided_at: Utc::now(),
            }],
            unresolved_questions: vec![
                "Confirm whether the interrupted provider completed any external side effect"
                    .to_owned(),
            ],
            known_failures: vec![FailureRecord {
                code: Some("orchestrator_restart_recovery".to_owned()),
                summary: "orchestrator restarted before the attempt reached a terminal state"
                    .to_owned(),
                retryable: true,
                occurred_at: Utc::now(),
            }],
            worker_claim: None,
            current_worker: provider,
            concise_context_summary:
                "Recovered from persisted task, attempt, worktree, and Git evidence".to_owned(),
            created_at: Utc::now(),
        },
        GitCheckpointEvidence::from(&snapshot),
    )?;
    database.record_checkpoint(&checkpoint)?;
    transition_task(
        database,
        task.task_id,
        TaskState::Checkpointed,
        orchestrator_domain::TransitionGuards {
            checkpoint_integrity_verified: checkpoint.verify_integrity()?,
            ..orchestrator_domain::TransitionGuards::default()
        },
        correlation_id,
        "restart recovery checkpoint integrity verified",
    )?;
    append_event(
        database,
        Some(task.task_id),
        EventType::CheckpointCreated,
        Some(TaskState::Checkpointing),
        Some(TaskState::Checkpointed),
        EventActor::Orchestrator,
        correlation_id,
        json!({"checkpoint_id": checkpoint.checkpoint_id, "recovered_after_restart": true}),
    )?;
    Ok(checkpoint)
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
#[allow(clippy::needless_borrow, clippy::single_match_else)]
async fn execute_planned_task(
    repository: &Path,
    state: &StatePaths,
    config: &RootConfig,
    global_database: &Database,
    database: &WorkspaceDatabase<'_>,
    plan: PlannedTask,
    correlation_id: CorrelationId,
    runtime_prompt: String,
    json_output: bool,
    existing_worktree: Option<GitWorktree>,
    initial_handover: Option<orchestrator_domain::HandoverBundle>,
    handover_already_acknowledged: bool,
    coordinator_lease_id: Uuid,
) -> Result<()> {
    fs::create_dir_all(&state.worktrees)?;
    let worktrees = GitWorktreeManager::open(repository, &state.worktrees)?;
    let resumed = existing_worktree.is_some();
    let worktree = match existing_worktree {
        Some(worktree) => worktree,
        None => {
            let worktree = worktrees.create(plan.task.task_id, "HEAD").await?;
            persist_worktree(&database, &worktree)?;
            worktree
        }
    };
    let artifacts = ArtifactStore::open(&state.root)?;
    let checkpoint_manager = CheckpointManager::new(artifacts.clone());
    let redaction = process_redaction(&config.orchestrator);
    let runtime: Arc<dyn AdapterRuntime> = Arc::new(ProcessAdapterRuntime::new(redaction.clone()));
    let assessment = plan
        .task
        .assessment
        .clone()
        .ok_or_else(|| anyhow!("planned task has no assessment"))?;
    let mut provider = plan
        .routing
        .selected_provider
        .ok_or_else(|| anyhow!("planned task has no selected provider"))?;
    let mut profile = plan
        .routing
        .selected_profile
        .ok_or_else(|| anyhow!("planned task has no selected profile"))?;
    let mut effort = plan.routing.reasoning_effort;
    let mut handover_payload = initial_handover
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?;
    let mut pending_handover = (!handover_already_acknowledged)
        .then_some(initial_handover)
        .flatten();
    let mut implementation_provider = provider;
    let max_attempts = usize::try_from(config.orchestrator.max_retries)
        .unwrap_or(usize::MAX)
        .saturating_add(3)
        .min(6);

    let mut successful_run = None;
    for _round in 1..=max_attempts {
        let (model, configured_effort) = profile_settings(&config.orchestrator, provider, profile)?;
        effort = configured_effort.or(effort);
        let adapter = provider_adapter(provider, config, Arc::clone(&runtime), repository)?;
        let active_provider_config = provider_config(&config.orchestrator, provider)
            .ok_or_else(|| anyhow!("provider configuration disappeared"))?;

        // A provider handover is acknowledged in a separate read-only turn. The next
        // provider cannot mutate the worktree until it has echoed the sealed bundle's
        // objective, constraints, and acceptance criteria and that acknowledgement is
        // persisted atomically.
        if let Some(bundle) = pending_handover.take() {
            let acknowledgement_request = WorkerRequest {
                schema_version: SchemaVersion::v1(),
                task_id: plan.task.task_id,
                attempt_id: AttemptId::new(),
                provider,
                objective: "Acknowledge a sealed vendor-neutral handover".to_owned(),
                prompt: format!(
                    "Read the attached handover bundle without changing any file. Return exactly one JSON object \
                     and no other text. Copy the objective, constraints, and acceptance criteria exactly from the \
                     bundle: {{\"type\":\"handover_ack\",\"bundle_hash\":\"{}\",\
                     \"can_resume\":true,\"understood_objective\":\"...\",\
                     \"understood_constraints\":[],\"understood_acceptance_criteria\":[],\
                     \"unresolved_questions\":[]}}",
                    bundle.integrity_hash
                ),
                constraints: vec!["Read-only acknowledgement; do not change files".to_owned()],
                acceptance_criteria: vec![
                    "The sole response is a valid handover_ack JSON object".to_owned(),
                ],
                workspace_root: worktree.path.clone(),
                sandbox: SandboxMode::ReadOnly,
                profile,
                model: model.clone(),
                reasoning_effort: effort,
                timeout_seconds: config
                    .orchestrator
                    .default_timeout_minutes
                    .saturating_mul(60),
                max_output_bytes: 1024 * 1024,
                resume_session_id: None,
                handover_payload: Some(serde_json::to_value(&bundle)?),
            };
            let acknowledgement_lease = acquire_worker_lease(
                &database,
                coordinator_lease_id,
                plan.task.task_id,
                provider,
                SandboxMode::ReadOnly,
            )?;
            let acknowledgement_run = run_worker(
                adapter.as_ref(),
                acknowledgement_request,
                active_provider_config,
                -1.0,
                false,
                false,
                &redaction,
                state,
                &database,
                coordinator_lease_id,
                &acknowledgement_lease,
                correlation_id,
            )
            .await;
            let acknowledgement_run = match acknowledgement_run {
                Ok(run) => {
                    release_worker_lease(&database, coordinator_lease_id, &acknowledgement_lease)?;
                    run
                }
                Err(error) => return Err(error),
            };
            let acknowledgement = (|| -> Result<HandoverAcknowledgement> {
                if acknowledgement_run.result.outcome != WorkerOutcome::Succeeded
                    || acknowledgement_run.lifecycle_error.is_some()
                {
                    bail!("handover acknowledgement worker did not complete safely");
                }
                let acknowledgement = acknowledgement_from_messages(
                    &bundle,
                    provider,
                    &acknowledgement_run.messages,
                )?;
                HandoverManager::validate_acknowledgement(&bundle, &acknowledgement)?;
                Ok(acknowledgement)
            })();
            let acknowledgement = match acknowledgement {
                Ok(acknowledgement) => acknowledgement,
                Err(error) => {
                    transition_task(
                        &database,
                        plan.task.task_id,
                        TaskState::Blocked,
                        orchestrator_domain::TransitionGuards::default(),
                        correlation_id,
                        "handover acknowledgement failed before writable resume",
                    )?;
                    append_event(
                        &database,
                        Some(plan.task.task_id),
                        EventType::TaskBlocked,
                        Some(TaskState::HandingOver),
                        Some(TaskState::Blocked),
                        EventActor::Orchestrator,
                        correlation_id,
                        json!({"reason": "handover_acknowledgement_failed", "detail": error.to_string()}),
                    )?;
                    reconcile_events(state, &database)?;
                    return emit(
                        json_output,
                        "run_blocked",
                        &json!({"task_id": plan.task.task_id, "reason": "handover acknowledgement failed"}),
                    );
                }
            };
            complete_handover(&database, &bundle, &acknowledgement)?;
            transition_task(
                &database,
                plan.task.task_id,
                TaskState::Resuming,
                orchestrator_domain::TransitionGuards {
                    handover_integrity_verified: true,
                    handover_acknowledged: true,
                    ..orchestrator_domain::TransitionGuards::default()
                },
                correlation_id,
                "read-only handover acknowledgement validated",
            )?;
            append_event(
                &database,
                Some(plan.task.task_id),
                EventType::HandoverCompleted,
                None,
                None,
                EventActor::Provider(provider),
                correlation_id,
                json!({"handover_id": bundle.handover_id, "to": provider}),
            )?;
            transition_task(
                &database,
                plan.task.task_id,
                TaskState::Running,
                orchestrator_domain::TransitionGuards {
                    handover_integrity_verified: true,
                    handover_acknowledged: true,
                    ..orchestrator_domain::TransitionGuards::default()
                },
                correlation_id,
                "writable execution resumed after acknowledgement",
            )?;
        }

        let attempt_id = AttemptId::new();
        let resume_session_id = latest_resume_session_id(&database, plan.task.task_id, provider)?;
        let request = WorkerRequest {
            schema_version: SchemaVersion::v1(),
            task_id: plan.task.task_id,
            attempt_id,
            provider,
            objective: plan.task.objective.clone(),
            prompt: runtime_prompt.clone(),
            constraints: plan.task.constraints.clone(),
            acceptance_criteria: plan.task.acceptance_criteria.clone(),
            workspace_root: worktree.path.clone(),
            sandbox: SandboxMode::WorkspaceWrite,
            profile,
            model,
            reasoning_effort: effort,
            timeout_seconds: config
                .orchestrator
                .default_timeout_minutes
                .saturating_mul(60),
            max_output_bytes: 16 * 1024 * 1024,
            resume_session_id,
            handover_payload: handover_payload.clone(),
        };
        // Persist the first writable attempt before projecting Running. A crash can
        // therefore never leave a Running task without an authoritative attempt record.
        let task_state = database
            .load_task(plan.task.task_id)?
            .ok_or_else(|| anyhow!("task disappeared before worker execution"))?
            .state;
        let attempt_already_persisted = if task_state == TaskState::Planned {
            persist_attempt_started(&database, &request, Utc::now())?;
            transition_task(
                &database,
                plan.task.task_id,
                TaskState::Running,
                orchestrator_domain::TransitionGuards::default(),
                correlation_id,
                if resumed {
                    "resumed planned task after persisting its worker attempt"
                } else {
                    "worker attempt persisted before execution started"
                },
            )?;
            true
        } else {
            false
        };
        let worker_lease = acquire_worker_lease(
            &database,
            coordinator_lease_id,
            plan.task.task_id,
            provider,
            SandboxMode::WorkspaceWrite,
        )?;
        let run = run_worker(
            adapter.as_ref(),
            request,
            active_provider_config,
            config.orchestrator.handover_threshold_percent,
            true,
            attempt_already_persisted,
            &redaction,
            state,
            &database,
            coordinator_lease_id,
            &worker_lease,
            correlation_id,
        )
        .await;
        let run = match run {
            Ok(run) => run,
            Err(error) => return Err(error),
        };

        if run.result.outcome == WorkerOutcome::Succeeded && run.lifecycle_error.is_none() {
            let completed_snapshot = worktrees.snapshot(&worktree).await?;
            record_changed_file_ownership(
                &database,
                plan.task.task_id,
                &worker_lease,
                &completed_snapshot.changed_files,
            )?;
            release_worker_lease(&database, coordinator_lease_id, &worker_lease)?;
            implementation_provider = provider;
            successful_run = Some(run);
            break;
        }

        let quota_exceeded = run.result.outcome == WorkerOutcome::QuotaExceeded;
        if quota_exceeded {
            let exhausted = confirmed_exhaustion(
                provider,
                provider_config(&config.orchestrator, provider)
                    .ok_or_else(|| anyhow!("provider config disappeared"))?,
            );
            database.record_usage_snapshot(Some(plan.task.task_id), &exhausted)?;
            append_event(
                &database,
                Some(plan.task.task_id),
                EventType::ProviderExhausted,
                None,
                None,
                EventActor::Provider(provider),
                correlation_id,
                json!({"provider": provider}),
            )?;
        }

        transition_task(
            &database,
            plan.task.task_id,
            TaskState::CheckpointRequested,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "provider handover checkpoint requested",
        )?;
        transition_task(
            &database,
            plan.task.task_id,
            TaskState::Checkpointing,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "capturing authoritative Git evidence",
        )?;
        let snapshot = worktrees.snapshot(&worktree).await?;
        record_changed_file_ownership(
            &database,
            plan.task.task_id,
            &worker_lease,
            &snapshot.changed_files,
        )?;
        // Ownership must be durable before another writable worker can acquire
        // this task's worktree lease.
        release_worker_lease(&database, coordinator_lease_id, &worker_lease)?;
        let persistence_scan =
            VerificationEngine::new()?.preflight_persistence(&worktree.path, &snapshot)?;
        if !persistence_scan.safe_to_persist_or_share() {
            transition_task(
                &database,
                plan.task.task_id,
                TaskState::Blocked,
                orchestrator_domain::TransitionGuards::default(),
                correlation_id,
                "checkpoint persistence blocked by secret-scan policy",
            )?;
            append_event(
                &database,
                Some(plan.task.task_id),
                EventType::TaskBlocked,
                Some(TaskState::Checkpointing),
                Some(TaskState::Blocked),
                EventActor::Orchestrator,
                correlation_id,
                json!({
                    "reason": "checkpoint_secret_preflight",
                    "finding_kinds": persistence_scan.findings.iter().map(|finding| &finding.kind).collect::<Vec<_>>(),
                    "unscanned_large_files": persistence_scan.truncated_files,
                }),
            )?;
            reconcile_events(state, &database)?;
            return emit(
                json_output,
                "run_blocked",
                &json!({"task_id": plan.task.task_id, "reason": "checkpoint secret preflight requires administrator review"}),
            );
        }
        let checkpoint_plan = vendor_neutral_plan(&plan.task);
        let checkpoint = checkpoint_manager.create(
            CheckpointInput {
                task_id: plan.task.task_id,
                attempt_id,
                objective: plan.task.objective.clone(),
                current_plan: checkpoint_plan.clone(),
                completed_steps: Vec::new(),
                pending_steps: checkpoint_plan,
                files_read: Vec::new(),
                commands_run: run.result.commands.clone(),
                tests: run.result.tests.clone(),
                decisions: vec![DecisionRecord {
                    decision: format!("run implementation with {}", provider.as_str()),
                    rationale: plan.routing.rationale.join("; "),
                    alternatives: plan
                        .routing
                        .candidate_scores
                        .iter()
                        .filter(|candidate| candidate.provider != provider)
                        .map(|candidate| candidate.provider.as_str().to_owned())
                        .collect(),
                    decided_by: provider,
                    decided_at: plan.routing.created_at,
                }],
                unresolved_questions: run
                    .lifecycle_error
                    .iter()
                    .map(|error| format!("Resolve worker lifecycle failure: {error}"))
                    .collect(),
                known_failures: vec![FailureRecord {
                    code: run.lifecycle_error.clone().or_else(|| {
                        run.proactive_handover
                            .then(|| "proactive_quota_handover".to_owned())
                    }),
                    summary: if quota_exceeded {
                        "provider reported quota exhaustion".to_owned()
                    } else if run.proactive_handover {
                        "provider reached the configured safe handover threshold".to_owned()
                    } else {
                        "worker did not complete successfully".to_owned()
                    },
                    retryable: true,
                    occurred_at: Utc::now(),
                }],
                worker_claim: run.claim.clone(),
                current_worker: provider,
                concise_context_summary: bounded_text(
                    run.result.summary.as_deref().unwrap_or("worker stopped"),
                    4_096,
                ),
                created_at: Utc::now(),
            },
            GitCheckpointEvidence::from(&snapshot),
        )?;
        database.record_checkpoint(&checkpoint)?;
        transition_task(
            &database,
            plan.task.task_id,
            TaskState::Checkpointed,
            orchestrator_domain::TransitionGuards {
                checkpoint_integrity_verified: checkpoint.verify_integrity()?,
                ..orchestrator_domain::TransitionGuards::default()
            },
            correlation_id,
            "checkpoint integrity verified",
        )?;
        append_event(
            &database,
            Some(plan.task.task_id),
            EventType::CheckpointCreated,
            None,
            Some(TaskState::Checkpointed),
            EventActor::Orchestrator,
            correlation_id,
            json!({"checkpoint_id": checkpoint.checkpoint_id}),
        )?;

        let mut manually_requested_provider = None;
        let mut handover_control_id = None;
        if let Some(control) = &run.requested_control {
            match control.action {
                ControlAction::Cancel => {
                    transition_task(
                        &database,
                        plan.task.task_id,
                        TaskState::Cancelled,
                        orchestrator_domain::TransitionGuards::default(),
                        correlation_id,
                        "user cancellation applied at a safe checkpoint",
                    )?;
                    database.complete_control(control.control_id, "cancelled", Utc::now())?;
                    reconcile_events(state, &database)?;
                    return emit(
                        json_output,
                        "run_cancelled",
                        &json!({"task_id": plan.task.task_id, "checkpoint_id": checkpoint.checkpoint_id}),
                    );
                }
                ControlAction::Pause => {
                    transition_task_projection(
                        &database,
                        plan.task.task_id,
                        TaskState::Blocked,
                        true,
                        orchestrator_domain::TransitionGuards::default(),
                        correlation_id,
                        "user pause applied at a safe checkpoint",
                    )?;
                    database.complete_control(control.control_id, "paused", Utc::now())?;
                    reconcile_events(state, &database)?;
                    return emit(
                        json_output,
                        "run_paused",
                        &json!({"task_id": plan.task.task_id, "checkpoint_id": checkpoint.checkpoint_id}),
                    );
                }
                ControlAction::Handover => {
                    manually_requested_provider = Some(
                        serde_json::from_value(
                            control
                                .payload
                                .get("to")
                                .cloned()
                                .ok_or_else(|| anyhow!("manual handover target is missing"))?,
                        )
                        .context("manual handover target is invalid")?,
                    );
                    handover_control_id = Some(control.control_id);
                }
                ControlAction::Resume | ControlAction::UsageOverride => {
                    database.complete_control(
                        control.control_id,
                        "ignored_in_running_worker",
                        Utc::now(),
                    )?;
                }
            }
        }

        let next_route = reroute_after_failure(
            &config.orchestrator,
            global_database,
            &database,
            &assessment,
            provider,
            implementation_provider,
            plan.task.task_id,
            manually_requested_provider,
            correlation_id,
        )
        .await?;
        persist_routing(&database, &next_route, &plan.task)?;
        let Some(next_provider) = next_route.selected_provider else {
            transition_task(
                &database,
                plan.task.task_id,
                TaskState::Blocked,
                orchestrator_domain::TransitionGuards::default(),
                correlation_id,
                "no provider can safely accept handover",
            )?;
            reconcile_events(state, &database)?;
            return emit(
                json_output,
                "run_blocked",
                &json!({"task_id": plan.task.task_id, "routing": next_route}),
            );
        };
        transition_task(
            &database,
            plan.task.task_id,
            TaskState::HandoverRequested,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "provider handover requested",
        )?;
        let bundle = HandoverManager::create(HandoverInput {
            checkpoint: checkpoint.clone(),
            original_request: plan.task.original_request_redacted.clone(),
            constraints: plan.task.constraints.clone(),
            acceptance_criteria: plan.task.acceptance_criteria.clone(),
            recommended_next_worker: next_provider,
            usage_snapshots: latest_usage_snapshots(global_database, &config.orchestrator)?,
            safe_boundary_confirmed: true,
            created_at: Utc::now(),
        })?;
        database.record_handover(
            checkpoint.checkpoint_id,
            "quota or repeatable provider failure",
            &bundle,
            None,
        )?;
        transition_task(
            &database,
            plan.task.task_id,
            TaskState::HandingOver,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "sealed handover bundle persisted",
        )?;
        if let Some(control_id) = handover_control_id {
            database.complete_control(control_id, "handover_started", Utc::now())?;
        }
        append_event(
            &database,
            Some(plan.task.task_id),
            EventType::HandoverStarted,
            Some(TaskState::Checkpointed),
            Some(TaskState::HandingOver),
            EventActor::Orchestrator,
            correlation_id,
            json!({"handover_id": bundle.handover_id, "from": provider, "to": next_provider}),
        )?;
        if !bundle.verify_integrity()? {
            bail!("sealed handover bundle failed integrity verification");
        }
        handover_payload = Some(serde_json::to_value(&bundle)?);
        pending_handover = Some(bundle);
        provider = next_provider;
        profile = next_route
            .selected_profile
            .ok_or_else(|| anyhow!("handover profile missing"))?;
        effort = next_route.reasoning_effort;
    }

    let Some(successful_run) = successful_run else {
        transition_task(
            &database,
            plan.task.task_id,
            TaskState::Failed,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "maximum safe attempts exhausted",
        )?;
        reconcile_events(state, &database)?;
        return emit(
            json_output,
            "run_failed",
            &json!({"task_id": plan.task.task_id, "reason": "maximum safe attempts exhausted"}),
        );
    };

    verify_and_finish(
        repository,
        state,
        config,
        global_database,
        &database,
        &worktrees,
        &worktree,
        &plan.task,
        implementation_provider,
        successful_run,
        coordinator_lease_id,
        correlation_id,
        json_output,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_worker(
    adapter: &dyn WorkerAdapter,
    request: WorkerRequest,
    provider_config: &ProviderConfig,
    handover_threshold_percent: f64,
    poll_controls: bool,
    attempt_already_persisted: bool,
    redaction_config: &RedactionConfig,
    state: &StatePaths,
    database: &WorkspaceDatabase<'_>,
    coordinator_lease_id: Uuid,
    worker_lease: &WorkerLease,
    correlation_id: CorrelationId,
) -> Result<WorkerRunRecord> {
    let started_at = Utc::now();
    if !attempt_already_persisted {
        persist_attempt_started(database, &request, started_at)?;
    }
    let handle = match adapter.start(request.clone()).await {
        Ok(handle) => handle,
        Err(error) => {
            persist_attempt_start_failure(database, request.attempt_id, &error.to_string())?;
            let finished_at = Utc::now();
            return Ok(WorkerRunRecord {
                result: WorkerResult {
                    schema_version: SchemaVersion::v1(),
                    task_id: request.task_id,
                    attempt_id: request.attempt_id,
                    provider: request.provider,
                    outcome: WorkerOutcome::Failed,
                    exit_code: None,
                    session_id: None,
                    summary: Some("provider process could not be started".to_owned()),
                    commands: Vec::new(),
                    tests: Vec::new(),
                    started_at,
                    finished_at,
                    output_truncated: false,
                },
                quota_exceeded: false,
                completed: false,
                lifecycle_error: Some(format!("provider_start_error:{error}")),
                messages: Vec::new(),
                claim: None,
                requested_control: None,
                proactive_handover: false,
            });
        }
    };
    append_event(
        database,
        Some(request.task_id),
        EventType::WorkerStarted,
        None,
        None,
        EventActor::Provider(request.provider),
        correlation_id,
        worker_started_payload(&request),
    )?;

    let redactor = Redactor::new(redaction_config)?;
    let mut quota_exceeded = false;
    let mut completed = false;
    let mut lifecycle_error = None;
    let mut messages = Vec::new();
    let mut session_id = None;
    let mut active_commands = HashMap::<String, (String, Vec<String>, DateTime<Utc>)>::new();
    let mut commands = Vec::new();
    let mut tests = Vec::new();
    let mut requested_control = None;
    let mut proactive_handover = false;
    let mut configured_usage_observed = false;
    let mut control_poll = tokio::time::interval(Duration::from_millis(500));
    control_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    control_poll.tick().await;
    let mut lease_renewal =
        tokio::time::interval(Duration::from_secs(LEASE_RENEWAL_INTERVAL_SECONDS));
    lease_renewal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    lease_renewal.tick().await;
    loop {
        let next_event = tokio::select! {
            result = adapter.next_event(&handle) => Some(result),
            _ = lease_renewal.tick() => {
                database.renew_worker_lease(
                    coordinator_lease_id,
                    worker_lease.lease_id,
                    LeaseRenewal {
                        renewed_at: Utc::now(),
                        ttl: TimeDelta::seconds(WORKER_LEASE_TTL_SECONDS),
                    },
                ).context("worker lease heartbeat failed")?;
                continue;
            }
            _ = control_poll.tick() => {
                if poll_controls
                    && active_commands.is_empty()
                    && let Some(control) = claim_next_control(database, request.task_id)?
                {
                    if let Err(error) = adapter.cancel(&handle).await {
                        lifecycle_error = Some(format!("provider_cancel_error:{error}"));
                    }
                    requested_control = Some(control);
                    None
                } else {
                    continue;
                }
            }
        };
        let Some(next_event) = next_event else {
            break;
        };
        let raw = match next_event {
            Ok(Some(raw)) => raw,
            Ok(None) => break,
            Err(error) => {
                lifecycle_error = Some(format!("provider_event_error:{error}"));
                if let Err(cancel_error) = adapter.cancel(&handle).await {
                    lifecycle_error = Some(format!(
                        "provider_event_error:{error};provider_cancel_error:{cancel_error}"
                    ));
                }
                break;
            }
        };
        match adapter.parse_event(raw).await {
            Ok(event) => {
                let terminal_error = matches!(&event, WorkerEvent::Error { .. });
                match &event {
                    WorkerEvent::Started { session_id: value } => session_id.clone_from(value),
                    WorkerEvent::Message { text } => messages.push(text.clone()),
                    WorkerEvent::CommandStarted {
                        command_id,
                        executable,
                        args,
                    } => {
                        if active_commands
                            .insert(
                                command_id.clone(),
                                (
                                    redactor.redact(executable),
                                    args.iter().map(|arg| redactor.redact(arg)).collect(),
                                    Utc::now(),
                                ),
                            )
                            .is_some()
                        {
                            lifecycle_error =
                                Some(format!("duplicate_command_started:{command_id}"));
                        }
                    }
                    WorkerEvent::CommandCompleted {
                        command_id,
                        exit_code,
                    } => {
                        if let Some((executable, args, command_started_at)) =
                            active_commands.remove(command_id)
                        {
                            let evidence_id = CommandEvidenceId::new();
                            let finished_at = Utc::now();
                            if looks_like_test_command(&executable, &args) {
                                tests.push(TestEvidence {
                                    name: bounded_text(
                                        &format!("{} {}", executable, args.join(" ")),
                                        1_024,
                                    ),
                                    status: if *exit_code == Some(0) {
                                        TestStatus::Passed
                                    } else {
                                        TestStatus::Failed
                                    },
                                    command_id: Some(evidence_id),
                                    detail: None,
                                });
                            }
                            commands.push(CommandEvidence {
                                id: evidence_id,
                                executable,
                                args,
                                cwd: None,
                                started_at: command_started_at,
                                finished_at,
                                exit_code: *exit_code,
                                timed_out: false,
                                output_truncated: false,
                                stdout_artifact: None,
                                stderr_artifact: None,
                                stdout_sha256: None,
                                stderr_sha256: None,
                            });
                        } else {
                            lifecycle_error =
                                Some(format!("unknown_command_completed:{command_id}"));
                        }
                    }
                    WorkerEvent::Usage { observation } => {
                        if observation.provider == request.provider {
                            let snapshot = persist_usage_observation(
                                database,
                                request.task_id,
                                observation,
                                provider_config,
                            )?;
                            configured_usage_observed |= snapshot.quota_scope
                                == quota_scope(request.provider, provider_config)?;
                            proactive_handover |= snapshot
                                .effective_remaining_percent()
                                .is_some_and(|remaining| remaining <= handover_threshold_percent);
                        } else {
                            lifecycle_error = Some("usage_provider_mismatch".to_owned());
                        }
                    }
                    WorkerEvent::QuotaExceeded { .. } => quota_exceeded = true,
                    WorkerEvent::Completed { usage, .. } => {
                        completed = true;
                        if let Some(observation) = usage {
                            if observation.provider == request.provider {
                                let snapshot = persist_usage_observation(
                                    database,
                                    request.task_id,
                                    observation,
                                    provider_config,
                                )?;
                                configured_usage_observed |= snapshot.quota_scope
                                    == quota_scope(request.provider, provider_config)?;
                                proactive_handover |=
                                    snapshot.effective_remaining_percent().is_some_and(
                                        |remaining| remaining <= handover_threshold_percent,
                                    );
                            } else {
                                lifecycle_error = Some("usage_provider_mismatch".to_owned());
                            }
                        }
                    }
                    WorkerEvent::Error { code, .. } => {
                        lifecycle_error =
                            Some(code.clone().unwrap_or_else(|| "worker_error".to_owned()));
                    }
                    WorkerEvent::Unknown {
                        event_type,
                        affects_lifecycle: true,
                        ..
                    } => {
                        lifecycle_error = Some(format!("unknown_lifecycle_event:{event_type}"));
                    }
                    WorkerEvent::FileChanged { .. }
                    | WorkerEvent::CheckpointClaim { .. }
                    | WorkerEvent::Unknown { .. } => {}
                }
                append_event(
                    database,
                    Some(request.task_id),
                    EventType::WorkerEvent,
                    None,
                    None,
                    EventActor::Provider(request.provider),
                    correlation_id,
                    audit_worker_event(&event, &redactor),
                )?;
                if terminal_error {
                    if let Err(error) = adapter.cancel(&handle).await {
                        lifecycle_error = Some(format!(
                            "{};provider_cancel_error:{error}",
                            lifecycle_error.as_deref().unwrap_or("worker_error")
                        ));
                    }
                    break;
                }
            }
            Err(error) => {
                lifecycle_error = Some(format!("compatibility_error:{error}"));
                if let Err(cancel_error) = adapter.cancel(&handle).await {
                    lifecycle_error = Some(format!(
                        "compatibility_error:{error};provider_cancel_error:{cancel_error}"
                    ));
                }
                break;
            }
        }

        if poll_controls
            && active_commands.is_empty()
            && let Some(control) = claim_next_control(database, request.task_id)?
        {
            if let Err(error) = adapter.cancel(&handle).await {
                lifecycle_error = Some(format!("provider_cancel_error:{error}"));
            }
            requested_control = Some(control);
            break;
        }
        if active_commands.is_empty() && proactive_handover {
            if let Err(error) = adapter.cancel(&handle).await {
                lifecycle_error = Some(format!("provider_cancel_error:{error}"));
            }
            break;
        }
    }
    if poll_controls && requested_control.is_none() && active_commands.is_empty() {
        requested_control = claim_next_control(database, request.task_id)?;
    }
    let claim = adapter.checkpoint(&handle).await.ok();
    let output = match adapter.wait(&handle).await {
        Ok(output) => output,
        Err(error) => {
            block_for_unconfirmed_termination(
                state,
                database,
                &request,
                correlation_id,
                &error.to_string(),
                &redactor,
            )?;
            bail!(
                "provider process termination could not be confirmed; worker lease is retained until expiry: {error}"
            );
        }
    };
    if let Some(error) = &output.tree_termination_error {
        block_for_unconfirmed_termination(
            state,
            database,
            &request,
            correlation_id,
            error,
            &redactor,
        )?;
        bail!(
            "provider process tree termination could not be confirmed; worker lease is retained until expiry"
        );
    }
    if poll_controls && requested_control.is_none() {
        requested_control = claim_next_control(database, request.task_id)?;
    }
    let finished_at = Utc::now();
    if !configured_usage_observed && let Some(amount) = provider_config.ledger_units_per_execution {
        let _ = persist_usage_observation(
            database,
            request.task_id,
            &orchestrator_domain::UsageObservation {
                provider: request.provider,
                quota_scope: quota_scope(request.provider, provider_config)?,
                amount,
                observed_at: finished_at,
                source: UsageSource::LocalLedger,
                confidence: UsageConfidence::Estimated,
            },
            provider_config,
        )?;
    }
    if !active_commands.is_empty() && lifecycle_error.is_none() {
        lifecycle_error = Some("incomplete_command_lifecycle".to_owned());
    }
    for (_, (executable, args, command_started_at)) in active_commands {
        let evidence_id = CommandEvidenceId::new();
        if looks_like_test_command(&executable, &args) {
            tests.push(TestEvidence {
                name: bounded_text(&format!("{} {}", executable, args.join(" ")), 1_024),
                status: TestStatus::Inconclusive,
                command_id: Some(evidence_id),
                detail: Some("command did not report completion before worker stopped".to_owned()),
            });
        }
        commands.push(CommandEvidence {
            id: evidence_id,
            executable,
            args,
            cwd: None,
            started_at: command_started_at,
            finished_at,
            exit_code: None,
            timed_out: output.termination == RuntimeTermination::TimedOut,
            output_truncated: output.truncated,
            stdout_artifact: None,
            stderr_artifact: None,
            stdout_sha256: None,
            stderr_sha256: None,
        });
    }
    let outcome = if requested_control.is_some() {
        WorkerOutcome::Cancelled
    } else if quota_exceeded {
        WorkerOutcome::QuotaExceeded
    } else {
        match output.termination {
            RuntimeTermination::TimedOut => WorkerOutcome::TimedOut,
            RuntimeTermination::Cancelled => WorkerOutcome::Cancelled,
            RuntimeTermination::Exited
                if output.exit_code == Some(0) && completed && lifecycle_error.is_none() =>
            {
                WorkerOutcome::Succeeded
            }
            RuntimeTermination::Exited => WorkerOutcome::Failed,
        }
    };
    let summary = messages
        .last()
        .map(|message| bounded_text(&redactor.redact(message), 8_192));
    let result = WorkerResult {
        schema_version: SchemaVersion::v1(),
        task_id: request.task_id,
        attempt_id: request.attempt_id,
        provider: request.provider,
        outcome,
        exit_code: output.exit_code,
        session_id,
        summary,
        commands,
        tests,
        started_at,
        finished_at,
        output_truncated: output.truncated,
    };
    let process_execution = output
        .resolved_executable
        .as_ref()
        .ok_or_else(|| anyhow!("completed worker omitted process execution evidence"))?;
    persist_attempt_result(database, &result, process_execution)?;
    Ok(WorkerRunRecord {
        result,
        quota_exceeded,
        completed,
        lifecycle_error,
        messages,
        claim,
        requested_control,
        proactive_handover,
    })
}

fn looks_like_test_command(executable: &str, args: &[String]) -> bool {
    let command = format!("{} {}", executable, args.join(" ")).to_lowercase();
    [
        " test",
        "pytest",
        "cargo nextest",
        "go test",
        "npm test",
        "pnpm test",
    ]
    .iter()
    .any(|marker| command.contains(marker))
}

fn claim_next_control(
    database: &WorkspaceDatabase<'_>,
    task_id: TaskId,
) -> Result<Option<orchestrator_state::ControlRequest>> {
    for control in database.pending_controls(task_id)? {
        if database.claim_control(control.control_id, Utc::now())? {
            return Ok(Some(control));
        }
    }
    Ok(None)
}

fn audit_worker_event(event: &WorkerEvent, redactor: &Redactor) -> Value {
    match event {
        WorkerEvent::Started { session_id } => {
            json!({"type": "started", "session_id_present": session_id.is_some()})
        }
        WorkerEvent::Message { text } => json!({
            "type": "message",
            "text": bounded_text(&redactor.redact(text), 4_096),
        }),
        WorkerEvent::CommandStarted {
            command_id,
            executable,
            args,
        } => json!({
            "type": "command_started",
            "command_id": command_id,
            "executable": bounded_text(&redactor.redact(executable), 512),
            "args": args.iter().map(|arg| bounded_text(&redactor.redact(arg), 512)).collect::<Vec<_>>(),
        }),
        WorkerEvent::CommandCompleted {
            command_id,
            exit_code,
        } => json!({"type": "command_completed", "command_id": command_id, "exit_code": exit_code}),
        WorkerEvent::FileChanged { path } => json!({"type": "file_changed", "path": path}),
        WorkerEvent::Usage { observation } => json!({
            "type": "usage",
            "provider": observation.provider,
            "amount": observation.amount,
            "unit": observation.quota_scope.unit,
        }),
        WorkerEvent::QuotaExceeded { detail } => json!({
            "type": "quota_exceeded",
            "detail": detail.as_deref().map(|value| bounded_text(&redactor.redact(value), 1_024)),
        }),
        WorkerEvent::CheckpointClaim { summary } => json!({
            "type": "checkpoint_claim",
            "summary": bounded_text(&redactor.redact(summary), 2_048),
            "trusted": false,
        }),
        WorkerEvent::Completed { summary, usage } => json!({
            "type": "completed",
            "summary": summary.as_deref().map(|value| bounded_text(&redactor.redact(value), 2_048)),
            "usage": usage.as_ref().map(|observation| json!({
                "provider": observation.provider,
                "amount": observation.amount,
                "unit": observation.quota_scope.unit,
                "source": observation.source,
                "confidence": observation.confidence,
            })),
        }),
        WorkerEvent::Error {
            code,
            message,
            retryable,
        } => json!({
            "type": "error",
            "code": code,
            "message": bounded_text(&redactor.redact(message), 2_048),
            "retryable": retryable,
        }),
        WorkerEvent::Unknown {
            event_type,
            affects_lifecycle,
            ..
        } => json!({
            "type": "unknown",
            "event_type": event_type,
            "affects_lifecycle": affects_lifecycle,
            "payload_persisted": false,
        }),
    }
}

fn provider_adapter(
    provider: ProviderId,
    config: &RootConfig,
    runtime: Arc<dyn AdapterRuntime>,
    repository: &Path,
) -> Result<Box<dyn WorkerAdapter>> {
    let provider_config = provider_config(&config.orchestrator, provider)
        .ok_or_else(|| anyhow!("provider {} is not configured", provider.as_str()))?;
    let usage_probe = adapter_usage_probe(&provider_config.usage_probe, repository);
    let scope = quota_scope(provider, provider_config)?;
    match provider {
        ProviderId::Codex => Ok(Box::new(
            CodexAdapter::new(
                CodexAdapterConfig {
                    executable: PathBuf::from(&provider_config.executable),
                    usage_probe,
                    usage_scope: scope,
                    allow_untested_read_only: true,
                },
                runtime,
            )
            .with_transport_features(CodexTransportFeatures {
                app_server_adapter: config.features.codex_app_server_adapter,
                exec_fallback: config.features.codex_exec_fallback,
            }),
        )),
        ProviderId::Claude => Ok(Box::new(ClaudeAdapter::new(
            ClaudeAdapterConfig {
                executable: PathBuf::from(&provider_config.executable),
                usage_probe,
                usage_scope: scope,
                effort_flag_enabled: provider_config.effort_flag_enabled,
            },
            runtime,
        ))),
        ProviderId::Gemini => Ok(Box::new(GeminiAdapter::new(
            GeminiAdapterConfig {
                executable: PathBuf::from(&provider_config.executable),
                usage_probe,
                usage_scope: scope,
            },
            runtime,
        ))),
        ProviderId::Agy => Ok(Box::new(AgyAdapter::new(
            AgyAdapterConfig {
                executable: PathBuf::from(&provider_config.executable),
                usage_probe,
                usage_scope: scope,
            },
            runtime,
        ))),
    }
}

fn adapter_usage_probe(
    config: &orchestrator_state::UsageProbeConfig,
    repository: &Path,
) -> orchestrator_providers::UsageProbeConfig {
    match config {
        orchestrator_state::UsageProbeConfig::Command {
            executable, args, ..
        } => orchestrator_providers::UsageProbeConfig::Command {
            executable: PathBuf::from(executable),
            args: args.clone(),
            format: orchestrator_providers::UsageProbeFormat::Json,
            working_directory: Some(repository.to_path_buf()),
        },
        orchestrator_state::UsageProbeConfig::ManualOrLedger => {
            orchestrator_providers::UsageProbeConfig::ManualOrLedger
        }
    }
}

fn profile_settings(
    config: &OrchestratorConfig,
    provider: ProviderId,
    profile: ModelProfile,
) -> Result<(Option<String>, Option<ReasoningEffort>)> {
    let name = match profile {
        ModelProfile::Economy => "economy",
        ModelProfile::Standard => "standard",
        ModelProfile::Premium => "premium",
    };
    let profile = config
        .model_profiles
        .get(provider.as_str())
        .and_then(|profiles| profiles.get(name))
        .ok_or_else(|| {
            anyhow!(
                "{} {name} model profile is not configured",
                provider.as_str()
            )
        })?;
    let effort = profile
        .effort
        .as_deref()
        .map(|value| match value {
            "low" => Ok(ReasoningEffort::Low),
            "medium" => Ok(ReasoningEffort::Medium),
            "high" => Ok(ReasoningEffort::High),
            _ => bail!("invalid reasoning effort `{value}`"),
        })
        .transpose()?;
    Ok((
        (!profile.model.trim().is_empty()).then(|| profile.model.clone()),
        effort,
    ))
}

fn persist_attempt_started(
    database: &WorkspaceDatabase<'_>,
    request: &WorkerRequest,
    started_at: DateTime<Utc>,
) -> Result<()> {
    database.record_task_attempt_started(&NewTaskAttemptRecord {
        attempt_id: request.attempt_id,
        task_id: request.task_id,
        provider: request.provider,
        worker_mode: enum_name(&request.sandbox)?,
        started_at,
    })?;
    Ok(())
}

fn worker_started_payload(request: &WorkerRequest) -> Value {
    json!({
        "attempt_id": request.attempt_id,
        "provider": request.provider,
        "sandbox": request.sandbox,
        "profile": request.profile,
        "model": request.model.as_deref(),
        "reasoning_effort": request.reasoning_effort,
        "session_resume_requested": request.resume_session_id.is_some(),
    })
}

fn persist_attempt_start_failure(
    database: &WorkspaceDatabase<'_>,
    attempt_id: AttemptId,
    detail: &str,
) -> Result<()> {
    database.finish_task_attempt(
        attempt_id,
        "failed",
        &json!({"start_error": bounded_text(detail, 2_048)}),
        Utc::now(),
    )?;
    Ok(())
}

fn persist_attempt_unconfirmed_termination(
    database: &WorkspaceDatabase<'_>,
    attempt_id: AttemptId,
    detail: &str,
) -> Result<()> {
    database.mark_task_attempt_termination_unconfirmed(attempt_id, &bounded_text(detail, 2_048))?;
    Ok(())
}

fn block_for_unconfirmed_termination(
    state: &StatePaths,
    database: &WorkspaceDatabase<'_>,
    request: &WorkerRequest,
    correlation_id: CorrelationId,
    detail: &str,
    redactor: &Redactor,
) -> Result<()> {
    let detail = bounded_text(&redactor.redact(detail), 2_048);
    persist_attempt_unconfirmed_termination(database, request.attempt_id, &detail)?;
    let from_state = transition_task(
        database,
        request.task_id,
        TaskState::Blocked,
        orchestrator_domain::TransitionGuards {
            process_tree_termination_unconfirmed: true,
            ..orchestrator_domain::TransitionGuards::default()
        },
        correlation_id,
        "provider process-tree termination could not be confirmed",
    )?;
    append_event(
        database,
        Some(request.task_id),
        EventType::TaskBlocked,
        Some(from_state),
        Some(TaskState::Blocked),
        EventActor::Orchestrator,
        correlation_id,
        json!({
            "reason": "process_tree_termination_unconfirmed",
            "provider": request.provider,
            "attempt_id": request.attempt_id,
            "detail": detail,
            "worker_lease_retained": true,
        }),
    )?;
    reconcile_events(state, database)?;
    Ok(())
}

fn persist_attempt_result(
    database: &WorkspaceDatabase<'_>,
    result: &WorkerResult,
    process_execution: &ResolvedExecutable,
) -> Result<()> {
    validate_resolution_evidence(process_execution)?;
    let mut persisted = serde_json::to_value(result)?;
    let object = persisted
        .as_object_mut()
        .ok_or_else(|| anyhow!("worker result must serialize as a JSON object"))?;
    object.insert(
        "process_execution".to_owned(),
        serde_json::to_value(process_execution)?,
    );
    database.finish_task_attempt(
        result.attempt_id,
        &enum_name(&result.outcome)?,
        &persisted,
        result.finished_at,
    )?;
    Ok(())
}

fn persist_worktree(database: &WorkspaceDatabase<'_>, worktree: &GitWorktree) -> Result<()> {
    database.record_active_worktree(&NewWorktreeRecord {
        task_id: worktree.task_id,
        repo_root: worktree.repository_root.clone(),
        worktree_path: worktree.path.clone(),
        branch_name: worktree.branch.clone(),
        base_revision: worktree.base_revision.clone(),
        created_at: Utc::now(),
    })?;
    Ok(())
}

#[cfg(test)]
fn acquire_task_coordinator(
    database: &WorkspaceDatabase<'_>,
    task_id: TaskId,
) -> Result<CoordinatorLease> {
    let now = Utc::now();
    let worktree_id = database
        .active_worktree(task_id)?
        .map(|value| value.worktree_id);
    match database.acquire_coordinator_lease(&CoordinatorLeaseRequest {
        task_id,
        worktree_id,
        owner_id: Uuid::now_v7(),
        acquired_at: now,
        ttl: TimeDelta::seconds(COORDINATOR_LEASE_TTL_SECONDS),
    }) {
        Ok(lease) => Ok(lease),
        Err(error @ StateError::LeaseConflict { .. }) => {
            Err(coordinator_conflict_diagnostic(database, task_id, error))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
fn coordinator_conflict_diagnostic(
    database: &WorkspaceDatabase<'_>,
    task_id: TaskId,
    error: StateError,
) -> anyhow::Error {
    let now = Utc::now();
    let Ok(coordinator) = database.active_coordinator_lease(task_id, now) else {
        return error.into();
    };
    let Ok(workers) = database.active_worker_leases(task_id, now) else {
        return error.into();
    };
    let safe_retry_at = coordinator
        .as_ref()
        .map(|lease| lease.expires_at)
        .into_iter()
        .chain(workers.iter().map(|lease| lease.expires_at))
        .max();
    let coordinator_detail = coordinator.map_or_else(
        || "coordinator=none".to_owned(),
        |lease| {
            format!(
                "coordinator_owner={} renewed_at={} expires_at={}",
                lease.owner_id,
                lease.renewed_at.to_rfc3339(),
                lease.expires_at.to_rfc3339()
            )
        },
    );
    let retry_detail = safe_retry_at.map_or_else(
        || "safe retry time unavailable".to_owned(),
        |expires_at| format!("safe retry after {}", expires_at.to_rfc3339()),
    );
    anyhow!(
        "{error}; {coordinator_detail}; active_workers={}; {retry_detail}",
        workers.len()
    )
}

#[cfg(test)]
async fn run_with_coordinator_renewal(
    database: &WorkspaceDatabase<'_>,
    coordinator: &CoordinatorLease,
    mut operation: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + '_>>,
) -> Result<()> {
    let mut renewal = tokio::time::interval(Duration::from_secs(LEASE_RENEWAL_INTERVAL_SECONDS));
    renewal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    renewal.tick().await;
    loop {
        tokio::select! {
            result = operation.as_mut() => return result,
            _ = renewal.tick() => {
                database.renew_coordinator_lease(
                    coordinator.lease_id,
                    coordinator.owner_id,
                    LeaseRenewal {
                        renewed_at: Utc::now(),
                        ttl: TimeDelta::seconds(COORDINATOR_LEASE_TTL_SECONDS),
                    },
                ).context("coordinator lease heartbeat failed")?;
            }
        }
    }
}

fn acquire_worker_lease(
    database: &WorkspaceDatabase<'_>,
    coordinator_lease_id: Uuid,
    task_id: TaskId,
    provider: ProviderId,
    sandbox: SandboxMode,
) -> Result<WorkerLease> {
    let now = Utc::now();
    let mode = if sandbox == SandboxMode::WorkspaceWrite {
        WorkerLeaseMode::Writable
    } else {
        WorkerLeaseMode::ReadOnly
    };
    let worktree_id = database
        .active_worktree(task_id)?
        .map(|value| value.worktree_id);
    database
        .acquire_worker_lease(&WorkerLeaseRequest {
            task_id,
            worktree_id,
            coordinator_lease_id,
            provider,
            mode,
            acquired_at: now,
            ttl: TimeDelta::seconds(WORKER_LEASE_TTL_SECONDS),
        })
        .map_err(Into::into)
}

fn release_worker_lease(
    database: &WorkspaceDatabase<'_>,
    coordinator_lease_id: Uuid,
    lease: &WorkerLease,
) -> Result<()> {
    database.release_worker_lease(coordinator_lease_id, lease.lease_id, Utc::now())?;
    Ok(())
}

fn record_changed_file_ownership(
    database: &WorkspaceDatabase<'_>,
    task_id: TaskId,
    lease: &WorkerLease,
    changed_files: &[RepoPath],
) -> Result<()> {
    let worktree_id = lease
        .worktree_id
        .ok_or_else(|| anyhow!("changed-file ownership requires a worktree-bound lease"))?;
    database.record_changed_file_ownership(
        task_id,
        worktree_id,
        lease.lease_id,
        changed_files,
        Utc::now(),
    )?;
    Ok(())
}

fn persist_usage_observation(
    database: &WorkspaceDatabase<'_>,
    task_id: TaskId,
    observation: &orchestrator_domain::UsageObservation,
    provider_config: &ProviderConfig,
) -> Result<UsageSnapshot> {
    let configured_scope = quota_scope(observation.provider, provider_config)?;
    let configured_quota_observation = observation.quota_scope == configured_scope;
    let window = if configured_quota_observation {
        Some(period_window(
            &reset_policy(provider_config)?,
            observation.observed_at,
        )?)
    } else {
        None
    };
    let prior = database
        .list_usage_snapshots(Some(observation.provider), 256)?
        .into_iter()
        .filter(|snapshot| {
            snapshot.source == UsageSource::LocalLedger
                && snapshot.quota_scope == observation.quota_scope
                && window.as_ref().is_none_or(|window| {
                    snapshot.collected_at >= window.started_at
                        && snapshot.collected_at < window.resets_at
                })
        })
        .filter_map(|snapshot| snapshot.used)
        .reduce(f64::max)
        .unwrap_or(0.0);
    let mut snapshot = UsageSnapshot::unknown(
        observation.provider,
        observation.quota_scope.clone(),
        observation.observed_at,
    );
    let used = prior + observation.amount;
    // A provider's token accounting is useful local ledger evidence, but it is
    // not the Enterprise quota unless the administrator configured the exact
    // same scope and unit. Never infer remaining quota across unlike units.
    snapshot.limit = configured_quota_observation
        .then_some(provider_config.quota_limit)
        .flatten();
    if let Some(limit) = snapshot.limit {
        let used = used.min(limit);
        snapshot.used = Some(used);
        let remaining = limit - used;
        snapshot.remaining = Some(remaining);
        snapshot.used_percent = Some((used / limit * 100.0).clamp(0.0, 100.0));
        snapshot.remaining_percent = Some((remaining / limit * 100.0).clamp(0.0, 100.0));
    } else {
        snapshot.used = Some(used);
    }
    if let Some(window) = window {
        snapshot.period_started_at = Some(window.started_at);
        snapshot.resets_at = Some(window.resets_at);
    }
    snapshot.source = UsageSource::LocalLedger;
    snapshot.confidence = UsageConfidence::Estimated;
    snapshot.validate()?;
    database.record_usage_snapshot(Some(task_id), &snapshot)?;
    Ok(snapshot)
}

fn confirmed_exhaustion(provider: ProviderId, config: &ProviderConfig) -> UsageSnapshot {
    let period = parse_quota_period(&config.quota_period).unwrap_or(QuotaPeriod::Custom);
    let mut snapshot = UsageSnapshot::unknown(
        provider,
        QuotaScope::new(
            config
                .quota_scope
                .clone()
                .unwrap_or_else(|| format!("{}_enterprise_primary", provider.as_str())),
            period,
            UsageUnit::Custom(config.quota_unit.clone()),
        ),
        Utc::now(),
    );
    snapshot.quota_period = period;
    snapshot.remaining = Some(0.0);
    snapshot.remaining_percent = Some(0.0);
    snapshot.limit = config.quota_limit;
    snapshot.used = config.quota_limit;
    snapshot.used_percent = config.quota_limit.map(|_| 100.0);
    snapshot.source = UsageSource::OfficialProtocol;
    snapshot.confidence = UsageConfidence::Confirmed;
    snapshot
}

#[allow(clippy::too_many_arguments)]
async fn reroute_after_failure(
    config: &OrchestratorConfig,
    global_database: &Database,
    database: &WorkspaceDatabase<'_>,
    assessment: &orchestrator_domain::TaskAssessment,
    failed_provider: ProviderId,
    implementation_provider: ProviderId,
    task_id: TaskId,
    manually_requested_provider: Option<ProviderId>,
    correlation_id: CorrelationId,
) -> Result<RoutingDecision> {
    let mut candidates = routing_candidates(
        config,
        global_database,
        database,
        assessment,
        manually_requested_provider,
        task_id,
        correlation_id,
    )
    .await?;
    for candidate in &mut candidates {
        if candidate.provider == failed_provider {
            candidate.health.status = HealthStatus::Unhealthy;
            candidate.health.detail = Some("excluded after quota or repeatable failure".to_owned());
        }
    }
    Ok(RoutingEngine::route(
        &RoutingContext {
            task_id,
            assessment: assessment.clone(),
            role: TaskRole::Implementation,
            writable: true,
            candidates,
            current_provider: Some(failed_provider),
            implementation_provider: Some(implementation_provider),
            manually_requested_provider,
            conserve_budget: true,
        },
        &RoutingConfig {
            max_parallel_workers: 1,
            allow_amber: true,
        },
        Utc::now(),
    )?)
}

fn acknowledgement_from_messages(
    bundle: &orchestrator_domain::HandoverBundle,
    provider: ProviderId,
    messages: &[String],
) -> Result<HandoverAcknowledgement> {
    let value = messages
        .iter()
        .rev()
        .find_map(|message| extract_json(message).ok())
        .ok_or_else(|| anyhow!("next provider did not emit a final structured response"))?;
    let wire: HandoverAckWire = serde_json::from_value(value)
        .context("final structured response is not a handover acknowledgement")?;
    if wire.kind != "handover_ack" || wire.bundle_hash != bundle.integrity_hash {
        bail!("provider acknowledgement references a different handover bundle");
    }
    Ok(HandoverAcknowledgement {
        schema_version: SchemaVersion::v1(),
        task_id: bundle.task_id,
        bundle_hash: wire.bundle_hash,
        provider,
        understood_objective: wire.understood_objective,
        understood_constraints: wire.understood_constraints,
        understood_acceptance_criteria: wire.understood_acceptance_criteria,
        next_step_id: bundle.pending_steps.first().map(|step| step.id.clone()),
        unresolved_questions: wire.unresolved_questions,
        can_resume: wire.can_resume,
        acknowledged_at: Utc::now(),
    })
}

fn complete_handover(
    database: &WorkspaceDatabase<'_>,
    bundle: &orchestrator_domain::HandoverBundle,
    acknowledgement: &HandoverAcknowledgement,
) -> Result<()> {
    database.complete_handover(bundle.handover_id, acknowledgement)?;
    Ok(())
}

fn extract_json(text: &str) -> Result<Value> {
    if let Ok(value) = serde_json::from_str(text.trim()) {
        return Ok(value);
    }
    let start = text
        .find('{')
        .ok_or_else(|| anyhow!("message has no JSON object"))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow!("message has no JSON object"))?;
    if start >= end {
        bail!("message has malformed JSON boundaries");
    }
    Ok(serde_json::from_str(&text[start..=end])?)
}

fn bounded_text(text: &str, maximum_chars: usize) -> String {
    let mut output = text.chars().take(maximum_chars).collect::<String>();
    if text.chars().count() > maximum_chars {
        output.push_str("…[truncated]");
    }
    output
}

#[derive(Clone, Debug)]
struct WorkerRunRecord {
    result: WorkerResult,
    #[allow(dead_code)]
    quota_exceeded: bool,
    #[allow(dead_code)]
    completed: bool,
    lifecycle_error: Option<String>,
    messages: Vec<String>,
    claim: Option<orchestrator_domain::UntrustedWorkerClaim>,
    requested_control: Option<orchestrator_state::ControlRequest>,
    proactive_handover: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct HandoverAckWire {
    #[serde(rename = "type")]
    kind: String,
    bundle_hash: String,
    can_resume: bool,
    understood_objective: String,
    understood_constraints: Vec<String>,
    understood_acceptance_criteria: Vec<String>,
    #[serde(default)]
    unresolved_questions: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
async fn verify_and_finish(
    repository: &Path,
    state: &StatePaths,
    config: &RootConfig,
    global_database: &Database,
    database: &WorkspaceDatabase<'_>,
    worktrees: &GitWorktreeManager,
    worktree: &GitWorktree,
    task: &TaskEnvelope,
    implementation_provider: ProviderId,
    worker: WorkerRunRecord,
    coordinator_lease_id: Uuid,
    correlation_id: CorrelationId,
    json_output: bool,
) -> Result<()> {
    transition_task(
        database,
        task.task_id,
        TaskState::Verifying,
        orchestrator_domain::TransitionGuards::default(),
        correlation_id,
        "independent verification started",
    )?;
    append_event(
        database,
        Some(task.task_id),
        EventType::VerificationStarted,
        None,
        Some(TaskState::Verifying),
        EventActor::Orchestrator,
        correlation_id,
        json!({"implementation_provider": implementation_provider}),
    )?;
    let snapshot = worktrees.snapshot(worktree).await?;
    let assessment = task
        .assessment
        .as_ref()
        .ok_or_else(|| anyhow!("task assessment is missing"))?;
    let persistence_scan =
        VerificationEngine::new()?.preflight_persistence(&worktree.path, &snapshot)?;
    let review = if !persistence_scan.safe_to_persist_or_share() {
        ReviewOutcome {
            provider: None,
            passed: false,
            acceptance_criteria_met: false,
            findings: vec![
                "review sharing blocked because the diff may contain secrets or unscanned large files"
                    .to_owned(),
            ],
        }
    } else if assessment.requires_independent_review {
        perform_independent_review(
            repository,
            state,
            config,
            global_database,
            database,
            worktrees,
            worktree,
            task,
            implementation_provider,
            &snapshot,
            coordinator_lease_id,
            correlation_id,
        )
        .await?
    } else {
        ReviewOutcome {
            provider: None,
            passed: true,
            acceptance_criteria_met: task.acceptance_criteria.is_empty(),
            findings: Vec::new(),
        }
    };
    let redaction = process_redaction(&config.orchestrator);
    let (verification_commands, verification_tests) =
        run_verification_commands(state, worktree, task.task_id, &redaction).await?;
    let mut commands = worker.result.commands.clone();
    commands.extend(verification_commands);
    let mut tests = worker.result.tests.clone();
    tests.extend(verification_tests);
    let expected_paths = if task.repository_wide_write_scope {
        snapshot.changed_files.clone()
    } else {
        task.allowed_write_paths.clone()
    };
    let scope_matches = snapshot.changed_files.iter().all(|path| {
        expected_paths
            .iter()
            .any(|prefix| path.as_path().starts_with(prefix.as_path()))
    });
    let acceptance_criteria = task
        .acceptance_criteria
        .iter()
        .map(|criterion| {
            acceptance_evidence(
                criterion,
                &tests,
                assessment.requires_independent_review,
                &review,
                persistence_scan.safe_to_persist_or_share(),
                scope_matches,
            )
        })
        .collect();
    let mut unresolved_todos = added_todo_markers(&snapshot.diff);
    unresolved_todos.extend(review.findings);
    let verification = VerificationEngine::new()?.verify(VerificationInput {
        task_id: task.task_id,
        implementation_provider,
        reviewer_provider: review.provider,
        independent_review_required: assessment.requires_independent_review,
        independent_review_passed: review.passed,
        snapshot,
        worktree_root: worktree.path.clone(),
        expected_paths,
        commands,
        tests,
        acceptance_criteria,
        unresolved_todos,
        verified_at: Utc::now(),
    })?;
    database.record_verification(&verification)?;
    append_event(
        database,
        Some(task.task_id),
        EventType::VerificationCompleted,
        None,
        Some(TaskState::Verifying),
        EventActor::Orchestrator,
        correlation_id,
        serde_json::to_value(&verification)?,
    )?;
    let passed = verification.passes_completion_gate(assessment.requires_independent_review);
    if passed {
        transition_task(
            database,
            task.task_id,
            TaskState::Completed,
            orchestrator_domain::TransitionGuards {
                verification_passed: true,
                independent_review_required: assessment.requires_independent_review,
                independent_review_satisfied: review.passed,
                ..orchestrator_domain::TransitionGuards::default()
            },
            correlation_id,
            "verification completion gate passed",
        )?;
        append_event(
            database,
            Some(task.task_id),
            EventType::TaskCompleted,
            Some(TaskState::Verifying),
            Some(TaskState::Completed),
            EventActor::Orchestrator,
            correlation_id,
            json!({"verification_id": verification.verification_id}),
        )?;
    } else {
        transition_task(
            database,
            task.task_id,
            TaskState::Blocked,
            orchestrator_domain::TransitionGuards::default(),
            correlation_id,
            "verification did not satisfy completion gate",
        )?;
        append_event(
            database,
            Some(task.task_id),
            EventType::TaskBlocked,
            Some(TaskState::Verifying),
            Some(TaskState::Blocked),
            EventActor::Orchestrator,
            correlation_id,
            json!({"verification_id": verification.verification_id}),
        )?;
    }
    reconcile_events(state, database)?;
    emit(
        json_output,
        if passed {
            "run_completed"
        } else {
            "run_blocked"
        },
        &json!({
            "task_id": task.task_id,
            "worker_result": worker.result,
            "verification": verification,
            "worktree": worktree,
            "cleanup_requires_user_approval": true,
            "automatic_merge": false,
            "automatic_push": false,
        }),
    )
}

fn added_todo_markers(diff: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(diff)
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .filter(|line| {
            let upper = line.to_ascii_uppercase();
            ["TODO", "FIXME", "XXX"]
                .iter()
                .any(|marker| upper.contains(marker))
        })
        .take(100)
        .map(|line| bounded_text(line, 512))
        .collect()
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
async fn perform_independent_review(
    repository: &Path,
    state: &StatePaths,
    config: &RootConfig,
    global_database: &Database,
    database: &WorkspaceDatabase<'_>,
    worktrees: &GitWorktreeManager,
    worktree: &GitWorktree,
    task: &TaskEnvelope,
    implementation_provider: ProviderId,
    before: &orchestrator_engine::GitSnapshot,
    coordinator_lease_id: Uuid,
    correlation_id: CorrelationId,
) -> Result<ReviewOutcome> {
    let assessment = task
        .assessment
        .as_ref()
        .ok_or_else(|| anyhow!("assessment is missing"))?;
    let candidates = routing_candidates(
        &config.orchestrator,
        global_database,
        database,
        assessment,
        None,
        task.task_id,
        correlation_id,
    )
    .await?;
    let routing = RoutingEngine::route(
        &RoutingContext {
            task_id: task.task_id,
            assessment: assessment.clone(),
            role: TaskRole::IndependentReview,
            writable: false,
            candidates,
            current_provider: Some(implementation_provider),
            implementation_provider: Some(implementation_provider),
            manually_requested_provider: None,
            conserve_budget: false,
        },
        &RoutingConfig {
            max_parallel_workers: 1,
            allow_amber: true,
        },
        Utc::now(),
    )?;
    persist_routing(database, &routing, task)?;
    let Some(reviewer) = routing.selected_provider else {
        return Ok(ReviewOutcome {
            provider: None,
            passed: false,
            acceptance_criteria_met: false,
            findings: vec!["no independent reviewer satisfies policy and quota gates".to_owned()],
        });
    };
    let profile = routing
        .selected_profile
        .ok_or_else(|| anyhow!("review routing profile is missing"))?;
    let (model, configured_effort) = profile_settings(&config.orchestrator, reviewer, profile)?;
    let redaction = process_redaction(&config.orchestrator);
    let runtime: Arc<dyn AdapterRuntime> = Arc::new(ProcessAdapterRuntime::new(redaction.clone()));
    let adapter = provider_adapter(reviewer, config, runtime, repository)?;
    let diff = bounded_text(&String::from_utf8_lossy(&before.diff), 1_000_000);
    let prompt = format!(
        "Independently review this Git diff against the objective and acceptance criteria. \
         Do not modify files. Your final assistant message MUST be one JSON object: \
         {{\"type\":\"independent_review\",\"approved\":true|false,\
         \"acceptance_criteria_met\":true|false,\"findings\":[\"...\"]}}.\n\nDIFF:\n{diff}"
    );
    let reviewer_config = provider_config(&config.orchestrator, reviewer)
        .ok_or_else(|| anyhow!("reviewer provider configuration disappeared"))?;
    let reviewer_timeout = config
        .orchestrator
        .default_timeout_minutes
        .saturating_mul(60);
    let reviewer_lease = acquire_worker_lease(
        database,
        coordinator_lease_id,
        task.task_id,
        reviewer,
        SandboxMode::ReadOnly,
    )?;
    let run = run_worker(
        adapter.as_ref(),
        WorkerRequest {
            schema_version: SchemaVersion::v1(),
            task_id: task.task_id,
            attempt_id: AttemptId::new(),
            provider: reviewer,
            objective: format!("Independent review: {}", task.objective),
            prompt,
            constraints: task.constraints.clone(),
            acceptance_criteria: task.acceptance_criteria.clone(),
            workspace_root: worktree.path.clone(),
            sandbox: SandboxMode::ReadOnly,
            profile,
            model,
            reasoning_effort: configured_effort.or(routing.reasoning_effort),
            timeout_seconds: reviewer_timeout,
            max_output_bytes: 8 * 1024 * 1024,
            resume_session_id: None,
            handover_payload: None,
        },
        reviewer_config,
        -1.0,
        false,
        false,
        &redaction,
        state,
        database,
        coordinator_lease_id,
        &reviewer_lease,
        correlation_id,
    )
    .await;
    let run = match run {
        Ok(run) => run,
        Err(error) => return Err(error),
    };
    let after = worktrees.snapshot(worktree).await?;
    release_worker_lease(database, coordinator_lease_id, &reviewer_lease)?;
    if before.diff != after.diff || before.changed_files != after.changed_files {
        return Ok(ReviewOutcome {
            provider: Some(reviewer),
            passed: false,
            acceptance_criteria_met: false,
            findings: vec!["read-only reviewer changed the worktree".to_owned()],
        });
    }
    let wire = run
        .messages
        .iter()
        .rev()
        .find_map(|message| extract_json(message).ok())
        .and_then(|value| serde_json::from_value::<ReviewWire>(value).ok());
    let Some(wire) = wire else {
        return Ok(ReviewOutcome {
            provider: Some(reviewer),
            passed: false,
            acceptance_criteria_met: false,
            findings: vec!["reviewer did not return the required structured decision".to_owned()],
        });
    };
    let passed = wire.kind == "independent_review"
        && wire.approved
        && run.result.outcome == WorkerOutcome::Succeeded
        && run.lifecycle_error.is_none();
    Ok(ReviewOutcome {
        provider: Some(reviewer),
        passed,
        acceptance_criteria_met: wire.acceptance_criteria_met,
        findings: wire.findings,
    })
}

async fn run_verification_commands(
    state: &StatePaths,
    worktree: &GitWorktree,
    task_id: TaskId,
    redaction: &RedactionConfig,
) -> Result<(Vec<CommandEvidence>, Vec<TestEvidence>)> {
    let specs = verification_specs(&worktree.path, redaction);
    let artifacts = ArtifactStore::open(&state.root)?;
    let mut commands = Vec::new();
    let mut tests = Vec::new();
    for (name, spec) in specs {
        let command_id = CommandEvidenceId::new();
        let executable = spec.executable.to_string_lossy().into_owned();
        let args = spec
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let started_at = Utc::now();
        let result = ProcessRunner.run(spec, CancellationToken::new()).await?;
        let finished_at = Utc::now();
        let stdout = store_redacted_output(
            &artifacts,
            task_id,
            command_id,
            "stdout",
            &result.stdout.redacted_text,
        )?;
        let stderr = store_redacted_output(
            &artifacts,
            task_id,
            command_id,
            "stderr",
            &result.stderr.redacted_text,
        )?;
        let timed_out = result.termination == orchestrator_process::TerminationReason::TimedOut;
        let output_truncated = result.stdout.truncated || result.stderr.truncated;
        commands.push(CommandEvidence {
            id: command_id,
            executable,
            args,
            cwd: None,
            started_at,
            finished_at,
            exit_code: result.exit_code,
            timed_out,
            output_truncated,
            stdout_artifact: stdout
                .as_ref()
                .map(|artifact| artifact.relative_path.clone()),
            stderr_artifact: stderr
                .as_ref()
                .map(|artifact| artifact.relative_path.clone()),
            stdout_sha256: stdout.as_ref().map(|artifact| artifact.sha256.clone()),
            stderr_sha256: stderr.as_ref().map(|artifact| artifact.sha256.clone()),
        });
        tests.push(TestEvidence {
            name,
            status: if timed_out {
                TestStatus::TimedOut
            } else if result.success() && !output_truncated {
                TestStatus::Passed
            } else {
                TestStatus::Failed
            },
            command_id: Some(command_id),
            detail: (!result.success()).then(|| bounded_text(&result.stderr.redacted_text, 2_048)),
        });
    }
    if tests.is_empty() {
        tests.push(TestEvidence {
            name: "automatic verification command discovery".to_owned(),
            status: TestStatus::Inconclusive,
            command_id: None,
            detail: Some("no supported project manifest was found".to_owned()),
        });
    }
    Ok((commands, tests))
}

fn verification_specs(root: &Path, redaction: &RedactionConfig) -> Vec<(String, CommandSpec)> {
    let mut specs = Vec::new();
    let timeout = Duration::from_mins(30);
    if root.join("Cargo.toml").is_file() {
        specs.push((
            "cargo fmt --check".to_owned(),
            verification_spec(
                "cargo",
                ["fmt", "--all", "--", "--check"],
                root,
                timeout,
                redaction,
            ),
        ));
        specs.push((
            "cargo clippy".to_owned(),
            verification_spec(
                "cargo",
                [
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--all-features",
                    "--",
                    "-D",
                    "warnings",
                ],
                root,
                timeout,
                redaction,
            ),
        ));
        specs.push((
            "cargo check".to_owned(),
            verification_spec(
                "cargo",
                ["check", "--workspace", "--all-targets", "--all-features"],
                root,
                timeout,
                redaction,
            ),
        ));
        specs.push((
            "cargo test".to_owned(),
            verification_spec(
                "cargo",
                ["test", "--workspace", "--all-targets", "--all-features"],
                root,
                timeout,
                redaction,
            ),
        ));
    } else if root.join("go.mod").is_file() {
        specs.push((
            "go test".to_owned(),
            verification_spec("go", ["test", "./..."], root, timeout, redaction),
        ));
    } else if root.join("pyproject.toml").is_file() {
        specs.push((
            "pytest".to_owned(),
            verification_spec("python", ["-m", "pytest"], root, timeout, redaction),
        ));
    } else if root.join("package.json").is_file() {
        specs.push((
            "npm test".to_owned(),
            verification_spec("npm", ["test", "--if-present"], root, timeout, redaction),
        ));
    }
    specs
}

fn verification_spec<const N: usize>(
    executable: &str,
    args: [&str; N],
    root: &Path,
    timeout: Duration,
    redaction: &RedactionConfig,
) -> CommandSpec {
    let mut spec = CommandSpec::new(executable).args(args).current_dir(root);
    spec.timeout = timeout;
    spec.stdout_limit = 16 * 1024 * 1024;
    spec.stderr_limit = 8 * 1024 * 1024;
    spec.redaction = redaction.clone();
    spec
}

fn store_redacted_output(
    artifacts: &ArtifactStore,
    task_id: TaskId,
    command_id: CommandEvidenceId,
    channel: &str,
    text: &str,
) -> Result<Option<orchestrator_state::StoredArtifact>> {
    if text.is_empty() {
        return Ok(None);
    }
    let path = RepoPath::try_from(format!(
        "results/{task_id}/commands/{command_id}.{channel}.log"
    ))?;
    Ok(Some(artifacts.put(path, text.as_bytes())?))
}

fn latest_resume_session_id(
    database: &WorkspaceDatabase<'_>,
    task_id: TaskId,
    provider: ProviderId,
) -> Result<Option<String>> {
    let Some(attempt) = database.latest_task_attempt(task_id)? else {
        return Ok(None);
    };
    if attempt.provider != Some(provider) {
        return Ok(None);
    }
    Ok(attempt
        .decoded_worker_result()?
        .and_then(|result| result.session_id)
        .filter(|session_id| !session_id.trim().is_empty()))
}

#[derive(Clone, Debug)]
struct ReviewOutcome {
    provider: Option<ProviderId>,
    passed: bool,
    acceptance_criteria_met: bool,
    findings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReviewWire {
    #[serde(rename = "type")]
    kind: String,
    approved: bool,
    acceptance_criteria_met: bool,
    #[serde(default)]
    findings: Vec<String>,
}

fn acceptance_evidence(
    criterion: &str,
    tests: &[TestEvidence],
    independent_review_required: bool,
    review: &ReviewOutcome,
    secret_scan_safe: bool,
    scope_matches: bool,
) -> AcceptanceEvidence {
    let normalized = criterion.to_lowercase();
    let mut oracle_results = Vec::new();
    let mut evidence = Vec::new();

    if contains_any(
        &normalized,
        &["secret", "credential", "api key", "비밀", "자격 증명"],
    ) {
        oracle_results.push(secret_scan_safe);
        evidence.push("local changed-file and Git-diff secret scan".to_owned());
    }
    if contains_any(
        &normalized,
        &[
            "scope",
            "unexpected file",
            "out of scope",
            "변경 범위",
            "예상 범위",
        ],
    ) {
        oracle_results.push(scope_matches);
        evidence.push("authoritative Git changed-file scope comparison".to_owned());
    }

    let categories: &[(&[&str], &[&str])] = &[
        (
            &["test", "e2e", "end-to-end", "테스트", "통합"],
            &["test", "pytest"],
        ),
        (&["lint", "clippy", "린트"], &["clippy", "lint"]),
        (
            &["build", "compile", "type check", "빌드", "컴파일"],
            &["cargo check", "build", "compile"],
        ),
        (&["format", "fmt", "포맷", "서식"], &["fmt", "format"]),
    ];
    for (criterion_markers, evidence_markers) in categories {
        if !contains_any(&normalized, criterion_markers) {
            continue;
        }
        let matching = tests
            .iter()
            .filter(|test| {
                let name = test.name.to_lowercase();
                contains_any(&name, evidence_markers)
            })
            .collect::<Vec<_>>();
        oracle_results.push(
            !matching.is_empty()
                && matching
                    .iter()
                    .all(|test| test.status == TestStatus::Passed),
        );
        evidence.extend(
            matching
                .iter()
                .map(|test| format!("{}: {:?}", test.name, test.status)),
        );
    }

    let review_satisfied = !independent_review_required
        || (review.passed && review.acceptance_criteria_met && review.provider.is_some());
    if independent_review_required && review_satisfied {
        evidence.push("structured independent provider review of all criteria".to_owned());
    }
    let status = if !review_satisfied {
        VerificationStatus::Inconclusive
    } else if oracle_results.iter().all(|passed| *passed)
        && (!oracle_results.is_empty() || independent_review_required)
    {
        VerificationStatus::Pass
    } else if oracle_results.is_empty() {
        // Arbitrary natural-language criteria need a criterion-specific oracle
        // or an independent structured review; generic command success cannot
        // prove them.
        VerificationStatus::Inconclusive
    } else {
        VerificationStatus::Fail
    };

    AcceptanceEvidence {
        criterion: criterion.to_owned(),
        status,
        evidence,
    }
}

async fn status(
    repository: &Path,
    explicit_config: Option<&Path>,
    selector: &TaskSelector,
    json_output: bool,
) -> Result<()> {
    let client =
        crate::ipc_client::DaemonClient::connect_or_start(repository, explicit_config).await?;
    let response = client
        .request(
            "workspace.status",
            json!({"task_id": selector.task_id.as_deref()}),
        )
        .await?;
    emit(json_output, "status", &response.outcome["data"])
}

async fn usage(
    repository: &Path,
    explicit_config: Option<&Path>,
    effective: &EffectiveConfig,
    json_output: bool,
) -> Result<()> {
    let defaults = usage_defaults(&effective.config().orchestrator)?;
    let client =
        crate::ipc_client::DaemonClient::connect_or_start(repository, explicit_config).await?;
    let response = client
        .request("workspace.usage", json!({"defaults": defaults}))
        .await?;
    emit(json_output, "usage", &response.outcome["data"]["snapshots"])
}

#[allow(clippy::needless_pass_by_value)]
async fn usage_override(
    repository: &Path,
    explicit_config: Option<&Path>,
    effective: &EffectiveConfig,
    arguments: UsageOverrideArgs,
    json_output: bool,
) -> Result<()> {
    if arguments.entered_by.trim().is_empty() {
        bail!("--entered-by must be a non-empty audit identity");
    }
    let provider = ProviderId::from(arguments.provider);
    let provider_config = provider_config(&effective.config().orchestrator, provider)
        .ok_or_else(|| anyhow!("provider {} is not configured", provider.as_str()))?;
    let scope = quota_scope(provider, provider_config)?;
    let period = scope.period;
    let limit = arguments.limit.or(provider_config.quota_limit);
    for (name, value) in [
        ("used", arguments.used),
        ("remaining", arguments.remaining),
        ("limit", limit),
    ] {
        if value.is_some_and(|value| !value.is_finite() || value < 0.0) {
            bail!("manual usage {name} must be a finite non-negative number");
        }
    }
    if arguments.used.is_none() && arguments.remaining.is_none() {
        bail!("manual usage override requires --used or --remaining");
    }
    let used = arguments.used.or(match (limit, arguments.remaining) {
        (Some(limit), Some(remaining)) if limit >= remaining => Some(limit - remaining),
        _ => None,
    });
    let remaining = arguments.remaining.or(match (limit, used) {
        (Some(limit), Some(used)) if limit >= used => Some(limit - used),
        _ => None,
    });
    if let Some(limit) = limit {
        if used.is_some_and(|used| used > limit) {
            bail!("manual usage used value exceeds the configured limit");
        }
        if remaining.is_some_and(|remaining| remaining > limit) {
            bail!("manual usage remaining value exceeds the configured limit");
        }
        if let (Some(used), Some(remaining)) = (used, remaining) {
            let tolerance = (limit.abs() * 1.0e-6).max(1.0e-9);
            if (used + remaining - limit).abs() > tolerance {
                bail!("manual usage values are inconsistent: used + remaining must equal limit");
            }
        }
    }
    let mut snapshot = UsageSnapshot::unknown(provider, scope, Utc::now());
    snapshot.used = used;
    snapshot.limit = limit;
    snapshot.remaining = remaining;
    snapshot.used_percent = percentage(used, limit);
    snapshot.remaining_percent = percentage(remaining, limit);
    snapshot.quota_period = period;
    let window = period_window(&reset_policy(provider_config)?, snapshot.collected_at)?;
    snapshot.period_started_at = Some(window.started_at);
    snapshot.resets_at = Some(window.resets_at);
    snapshot.source = UsageSource::ManualOverride;
    snapshot.confidence = UsageConfidence::Confirmed;
    snapshot.validate()?;
    let client =
        crate::ipc_client::DaemonClient::connect_or_start(repository, explicit_config).await?;
    let response = client
        .request(
            "workspace.usage.override",
            json!({"snapshot": snapshot, "entered_by": arguments.entered_by}),
        )
        .await?;
    emit(
        json_output,
        "usage_override",
        &response.outcome["data"]["snapshot"],
    )
}

async fn control_handover(
    repository: &Path,
    explicit_config: Option<&Path>,
    arguments: HandoverArgs,
    json_output: bool,
) -> Result<()> {
    control(
        repository,
        explicit_config,
        RequiredTask {
            task_id: arguments.task_id,
        },
        "handover",
        json!({"to": ProviderId::from(arguments.to)}),
        json_output,
    )
    .await
}

#[allow(clippy::needless_pass_by_value)]
async fn control(
    repository: &Path,
    explicit_config: Option<&Path>,
    task: RequiredTask,
    action: &str,
    payload: Value,
    json_output: bool,
) -> Result<()> {
    let task_id = TaskId::from_str(&task.task_id)?;
    let control_action = match action {
        "pause" => orchestrator_state::ControlAction::Pause,
        "resume" => orchestrator_state::ControlAction::Resume,
        "cancel" => orchestrator_state::ControlAction::Cancel,
        "handover" => orchestrator_state::ControlAction::Handover,
        _ => bail!("unsupported control action {action}"),
    };
    let client =
        crate::ipc_client::DaemonClient::connect_or_start(repository, explicit_config).await?;
    let response = client
        .request(
            "workspace.control",
            json!({
                "task_id": task_id,
                "action": control_action,
                "payload": payload,
            }),
        )
        .await?;
    emit(json_output, "control_requested", &response.outcome["data"])
}

async fn explain_routing(
    repository: &Path,
    explicit_config: Option<&Path>,
    task_id: &str,
    json_output: bool,
) -> Result<()> {
    let task_id = TaskId::from_str(task_id)?;
    let client =
        crate::ipc_client::DaemonClient::connect_or_start(repository, explicit_config).await?;
    let response = client
        .request("workspace.routing", json!({"task_id": task_id}))
        .await?;
    emit(
        json_output,
        "explain_routing",
        &response.outcome["data"]["decision"],
    )
}

async fn checkpoint(
    repository: &Path,
    explicit_config: Option<&Path>,
    task_id: &str,
    json_output: bool,
) -> Result<()> {
    let task_id = TaskId::from_str(task_id)?;
    let client =
        crate::ipc_client::DaemonClient::connect_or_start(repository, explicit_config).await?;
    let response = client
        .request("workspace.checkpoint", json!({"task_id": task_id}))
        .await?;
    emit(
        json_output,
        "checkpoint",
        &response.outcome["data"]["checkpoint"],
    )
}

#[allow(clippy::needless_pass_by_value)]
fn migrate(
    repository: &Path,
    effective: &EffectiveConfig,
    explicit_edit_path: &Path,
    action: MigrationAction,
    json_output: bool,
) -> Result<()> {
    migrate_inner(
        repository,
        Some(effective),
        explicit_edit_path,
        action,
        json_output,
    )
}

fn migrate_without_runtime(
    repository: &Path,
    explicit_edit_path: &Path,
    action: MigrationAction,
    json_output: bool,
) -> Result<()> {
    migrate_inner(repository, None, explicit_edit_path, action, json_output)
}

fn migration_config_preview(
    effective: Option<&EffectiveConfig>,
    explicit_edit_path: &Path,
) -> Result<(
    Option<MigratableConfigDocument>,
    bool,
    orchestrator_state::ConfigMigrationPreview,
)> {
    let migratable = config_source_exists(explicit_edit_path)?
        .then(|| MigratableConfigDocument::load(explicit_edit_path))
        .transpose()?;
    let source_is_current = migratable.as_ref().is_none_or(|document| {
        document.current_version() == orchestrator_state::CONFIG_SCHEMA_VERSION
    });
    let preview = if source_is_current {
        let effective = effective.ok_or_else(|| anyhow!("effective config disappeared"))?;
        MigratableConfigDocument::parse(&effective.document().document().to_string())?.dry_run()?
    } else {
        migratable
            .as_ref()
            .ok_or_else(|| anyhow!("migration config source disappeared"))?
            .dry_run()?
    };
    Ok((migratable, source_is_current, preview))
}

#[allow(clippy::needless_pass_by_value)]
fn migrate_inner(
    _repository: &Path,
    effective: Option<&EffectiveConfig>,
    explicit_edit_path: &Path,
    action: MigrationAction,
    json_output: bool,
) -> Result<()> {
    let (migratable, source_is_current, config_preview) =
        migration_config_preview(effective, explicit_edit_path)?;
    let maintenance = crate::daemon::acquire_maintenance()?;
    let state = &maintenance.paths;
    let database = Database::open(&state.database)?;
    match action {
        MigrationAction::Status => emit(
            json_output,
            "migrate_status",
            &json!({
                "config": config_preview.result(),
                "database": database.migration_status()?,
            }),
        ),
        MigrationAction::Plan => {
            let database_plan = database.migration_plan()?;
            emit(
                json_output,
                "migrate_plan",
                &json!({
                    "config": config_preview.plan(),
                    "database": database_plan,
                    "sequential": true,
                    "destructive": false,
                }),
            )
        }
        MigrationAction::Apply { dry_run: true } => emit(
            json_output,
            "migrate_dry_run",
            &json!({
                "config": config_preview.result(),
                "database": database.dry_run_migrations()?,
                "files_changed": false,
            }),
        ),
        MigrationAction::Apply { dry_run: false } => {
            // Validate the complete catalog, including future-schema refusal, before the first
            // append-only audit write. Provider compatibility never authorizes state repair.
            let _ = database.dry_run_migrations()?;
            append_event_if_schema_available(
                &database,
                EventType::MigrationStarted,
                json!({
                    "config_target": orchestrator_state::CONFIG_SCHEMA_VERSION,
                    "database_target": orchestrator_state::STATE_SCHEMA_VERSION,
                }),
            )?;
            // Both plans are validated before the first write. Config apply is
            // backup-first and DB migrations run transactionally with their own
            // backup; neither path skips an intermediate schema.
            let config_status = if source_is_current {
                orchestrator_state::ConfigMigrationApplyResult {
                    result: config_preview.result().clone(),
                    backup_path: None,
                }
            } else {
                migratable
                    .as_ref()
                    .ok_or_else(|| anyhow!("migration config source disappeared"))?
                    .apply_to_file(explicit_edit_path, Utc::now())?
            };
            let database_status = database.migrate_with_backup(&state.backups)?;
            let migration_payload = json!({
                "config": {
                    "result": config_status.result,
                    "backup_path": config_status.backup_path,
                },
                "database": database_status,
            });
            if let Some(workspace) = legacy_migration_workspace_if_present(&database)? {
                append_event(
                    &workspace,
                    None,
                    EventType::MigrationCompleted,
                    None,
                    None,
                    EventActor::Administrator,
                    CorrelationId::new(),
                    migration_payload.clone(),
                )?;
            }
            emit(json_output, "migrate_apply", &migration_payload)
        }
        MigrationAction::Rollback(arguments) => migrate_rollback(
            state,
            &database,
            config_preview.migrated().config(),
            arguments.action,
            json_output,
        ),
    }
}

#[allow(clippy::too_many_lines)]
fn migrate_rollback(
    state: &GlobalStatePaths,
    database: &Database,
    config: &RootConfig,
    action: MigrationRollbackAction,
    json_output: bool,
) -> Result<()> {
    match action {
        MigrationRollbackAction::Plan { backup } => {
            let backup = trusted_migration_backup(&state.backups, backup.as_deref())?;
            let plan = database.create_validated_migration_rollback_plan(backup)?;
            let relative = migration_rollback_plan_relative_path(&plan.integrity_hash)?;
            let stored = ArtifactStore::open(&state.root)?
                .put(relative.clone(), &serde_json::to_vec_pretty(&plan)?)?;
            emit(
                json_output,
                "migrate_rollback_plan",
                &json!({
                    "plan": plan,
                    "plan_artifact": stored,
                    "requires_explicit_approval": true,
                    "apply_with": "migrate rollback apply --plan-hash <hash> --approved-by <identity>",
                    "audit_note": "the immutable plan is recorded in SQLite/JSONL after successful restore so its sealed event sequence cannot be invalidated",
                }),
            )
        }
        MigrationRollbackAction::Apply {
            plan_hash,
            approved_by,
        } => {
            ensure_migration_write_allowed(config)?;
            validate_sha256(&plan_hash)?;
            validate_approval_identity(&approved_by)?;

            let plan_relative = migration_rollback_plan_relative_path(&plan_hash)?;
            let plan_path = plan_relative.join_to(&state.root);
            let plan: orchestrator_state::RollbackPlan = serde_json::from_slice(
                &read_regular_file_below(&state.root, &plan_path)
                    .context("sealed migration rollback plan is unavailable")?,
            )?;
            if plan.integrity_hash != plan_hash {
                bail!("stored migration rollback plan does not match --plan-hash");
            }
            plan.verify_integrity_hash()?;
            database.validate_migration_rollback(&plan)?;

            let approved_at = Utc::now();
            let approval_relative =
                migration_rollback_approval_relative_path(&plan_hash, Uuid::now_v7())?;
            let artifacts = ArtifactStore::open(&state.root)?;
            let approval_artifact = artifacts.put(
                approval_relative,
                &serde_json::to_vec_pretty(&json!({
                    "schema_version": 1,
                    "operation": "migration_rollback_apply_approval",
                    "plan_hash": plan_hash,
                    "approved_by": approved_by.trim(),
                    "approved_at": approved_at,
                }))?,
            )?;
            let recovery_path = state
                .backups
                .join(format!("orchestrator.db.recovery.{}.db", Uuid::now_v7()));
            let result = database.apply_migration_rollback(
                &plan,
                &plan_hash,
                &approved_by,
                &recovery_path,
            )?;
            let result_relative = migration_rollback_result_relative_path(&plan_hash)?;
            let result_artifact = artifacts.put(
                result_relative,
                &serde_json::to_vec_pretty(&json!({
                    "schema_version": 1,
                    "operation": "migration_rollback_apply",
                    "plan": plan,
                    "approval_artifact": approval_artifact,
                    "result": result,
                }))?,
            )?;

            let audit_workspace = legacy_migration_workspace_if_present(database)?;
            let audit_event_recorded = audit_workspace.is_some();
            if let Some(workspace) = audit_workspace {
                append_event(
                    &workspace,
                    None,
                    EventType::RollbackPlanned,
                    None,
                    None,
                    EventActor::Administrator,
                    CorrelationId::new(),
                    json!({
                        "plan_hash": plan_hash,
                        "planned_at": plan.created_at,
                        "applied": true,
                        "approved_by": approved_by.trim(),
                    }),
                )?;
                append_event(
                    &workspace,
                    None,
                    EventType::MigrationCompleted,
                    None,
                    None,
                    EventActor::Administrator,
                    CorrelationId::new(),
                    json!({
                        "operation": "rollback",
                        "plan_hash": plan_hash,
                        "restored_schema_version": result.restored_schema_version,
                        "recovery_backup_path": result.recovery_backup_path,
                        "result_artifact": result_artifact,
                    }),
                )?;
            }
            emit(
                json_output,
                "migrate_rollback_apply",
                &json!({
                    "plan": plan,
                    "execution": result,
                    "approval_artifact": approval_artifact,
                    "result_artifact": result_artifact,
                    "audit_event_recorded": audit_event_recorded,
                    "restart_required": true,
                }),
            )
        }
    }
}

fn ensure_migration_write_allowed(config: &RootConfig) -> Result<()> {
    let redaction = process_redaction(&config.orchestrator);
    let codex_report = config
        .orchestrator
        .providers
        .codex
        .as_ref()
        .filter(|provider| provider.enabled)
        .and_then(|provider| probe_codex(&provider.executable, &redaction).ok());
    let guard = StartupGuard::evaluate(codex_report.as_ref(), &[], true, true, true);
    if guard.safe_mode {
        bail!(
            "migration apply is disabled in safe mode; run `colay compatibility` and resolve: {}",
            guard
                .warnings
                .iter()
                .chain(&guard.blockers)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn rollback(
    repository: &Path,
    effective: &EffectiveConfig,
    explicit_edit_path: &Path,
    action: RollbackAction,
    json_output: bool,
) -> Result<()> {
    let maintenance = crate::daemon::acquire_maintenance()?;
    let database = Database::open(&maintenance.paths.database)?;
    database.migrate_with_backup(&maintenance.paths.backups)?;
    let registration = database.resolve_repository_workspace(repository)?;
    let workspace_paths = maintenance.paths.for_workspace(registration.workspace_id);
    let state = global_release_state_paths(&maintenance.paths, &workspace_paths);
    let workspace = database.workspace(registration.workspace_id);
    match action {
        RollbackAction::Plan { to } => {
            validate_release_component(&to)?;
            let release_root = state.backups.join("releases").join(&to);
            let manifest_path = release_root.join("manifest.json");
            let manifest: RollbackManifest = serde_json::from_slice(
                &read_regular_file_below(&state.backups, &manifest_path).with_context(|| {
                    format!("rollback manifest is missing: {}", manifest_path.display())
                })?,
            )?;
            if manifest.schema_version != 1 || manifest.version != to {
                bail!("rollback manifest schema/version does not match --to {to}");
            }
            let resolution_context = rollback_resolution_context(
                repository,
                &state,
                effective.config(),
                &manifest.steps,
            )?;
            let steps = trusted_rollback_steps(
                repository,
                explicit_edit_path,
                &state,
                effective.config(),
                &resolution_context,
                &release_root,
                manifest.steps,
            )?;
            let manager = rollback_manager(&state, &steps)?;
            let preserved: Vec<PathBuf> = [
                &state.tasks,
                &state.checkpoints,
                &state.handovers,
                &state.worktrees,
            ]
            .into_iter()
            .filter(|path| path.exists())
            .cloned()
            .collect();
            let plan = manager.plan(&to, steps, &preserved, Utc::now())?;
            let plan_relative = rollback_plan_relative_path(&to, &plan.integrity_hash)?;
            let stored = ArtifactStore::open(&state.root)?
                .put(plan_relative.clone(), &serde_json::to_vec_pretty(&plan)?)?;
            append_event(
                &workspace,
                None,
                EventType::RollbackPlanned,
                None,
                None,
                EventActor::User,
                CorrelationId::new(),
                json!({"target": to, "plan_hash": plan.integrity_hash, "artifact": stored}),
            )?;
            emit(
                json_output,
                "rollback_plan",
                &json!({
                    "plan": plan,
                    "plan_artifact": plan_relative,
                    "requires_explicit_approval": true,
                    "apply_with": "rollback apply --to <version> --plan-hash <hash> --approved-by <identity>"
                }),
            )
        }
        RollbackAction::Apply {
            to,
            plan_hash,
            approved_by,
        } => {
            validate_release_component(&to)?;
            validate_sha256(&plan_hash)?;
            let relative = rollback_plan_relative_path(&to, &plan_hash)?;
            let absolute = relative.join_to(&state.root);
            let plan: orchestrator_engine::RollbackRecoveryPlan =
                serde_json::from_slice(&read_regular_file_below(&state.root, &absolute)?)?;
            if plan.target_version != to || plan.integrity_hash != plan_hash || !plan.verify()? {
                bail!("stored rollback plan does not match the explicitly approved plan hash");
            }
            let release_root = state.backups.join("releases").join(&to);
            let manifest_path = release_root.join("manifest.json");
            let manifest: RollbackManifest =
                serde_json::from_slice(&read_regular_file_below(&state.backups, &manifest_path)?)?;
            if manifest.schema_version != 1 || manifest.version != to {
                bail!("rollback release manifest schema/version does not match the sealed plan");
            }
            let resolution_context = rollback_resolution_context(
                repository,
                &state,
                effective.config(),
                &manifest.steps,
            )?;
            let manifest_steps = trusted_rollback_steps(
                repository,
                explicit_edit_path,
                &state,
                effective.config(),
                &resolution_context,
                &release_root,
                manifest.steps,
            )?;
            if manifest_steps != plan.steps {
                bail!("rollback release manifest changed after the plan was sealed");
            }
            validate_sealed_rollback_destinations(
                repository,
                explicit_edit_path,
                &state,
                effective.config(),
                &resolution_context,
                &plan,
            )?;
            let manager = rollback_manager(&state, &plan.steps)?;
            let approval =
                orchestrator_engine::RollbackApproval::for_plan(&plan, approved_by, Utc::now());
            ensure_rollback_quiescent(&workspace)?;
            workspace.record_release_rollback_approval(
                &json!({"target_version": to, "plan_hash": plan_hash}),
                &approval.approved_by,
                approval.approved_at,
            )?;
            let report = manager.apply(&plan, &approval)?;
            emit(
                json_output,
                "rollback_apply",
                &json!({"plan": plan, "execution": report, "restart_required": true}),
            )
        }
    }
}

fn global_release_state_paths(
    global: &GlobalStatePaths,
    workspace: &orchestrator_state::WorkspaceStatePaths,
) -> StatePaths {
    StatePaths {
        root: workspace.root.clone(),
        database: global.database.clone(),
        events: workspace.root.join("events.jsonl"),
        backups: workspace.backups.clone(),
        tasks: workspace.root.join("tasks"),
        checkpoints: workspace.checkpoints.clone(),
        handovers: workspace.handovers.clone(),
        worktrees: workspace.worktrees.clone(),
    }
}

fn validate_release_component(version: &str) -> Result<()> {
    if version.is_empty()
        || version == "."
        || version == ".."
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        bail!("rollback version is not a safe path component");
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("rollback plan hash must be a 64-character SHA-256 value");
    }
    Ok(())
}

fn validate_approval_identity(value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        bail!("--approved-by must be 1..=256 bytes and contain no control characters");
    }
    Ok(())
}

fn trusted_migration_backup(directory: &Path, requested: Option<&Path>) -> Result<PathBuf> {
    let canonical_root = fs::canonicalize(directory)
        .with_context(|| format!("backup directory is missing: {}", directory.display()))?;
    let candidate = match requested {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => canonical_root.join(path),
        None => latest_database_backup(&canonical_root)?,
    };
    canonical_regular_file_below(&canonical_root, &candidate)
        .context("migration rollback backup is not a trusted local backup")
}

fn migration_rollback_plan_relative_path(hash: &str) -> Result<RepoPath> {
    validate_sha256(hash)?;
    RepoPath::try_from(format!("backups/migration-rollback-plans/{hash}.json")).map_err(Into::into)
}

fn migration_rollback_approval_relative_path(hash: &str, approval_id: Uuid) -> Result<RepoPath> {
    validate_sha256(hash)?;
    RepoPath::try_from(format!(
        "backups/migration-rollback-approvals/{hash}-{approval_id}.json"
    ))
    .map_err(Into::into)
}

fn migration_rollback_result_relative_path(hash: &str) -> Result<RepoPath> {
    validate_sha256(hash)?;
    RepoPath::try_from(format!("backups/migration-rollback-results/{hash}.json"))
        .map_err(Into::into)
}

fn rollback_plan_relative_path(version: &str, hash: &str) -> Result<RepoPath> {
    validate_release_component(version)?;
    validate_sha256(hash)?;
    RepoPath::try_from(format!("backups/rollback-plans/{version}-{hash}.json")).map_err(Into::into)
}

fn read_regular_file_below(root: &Path, path: &Path) -> Result<Vec<u8>> {
    const MAX_ROLLBACK_METADATA_BYTES: u64 = 1024 * 1024;
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("rollback trust root is missing: {}", root.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("rollback artifact is missing: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("rollback artifact must be a non-symlink regular file");
    }
    if metadata.len() > MAX_ROLLBACK_METADATA_BYTES {
        bail!("rollback metadata exceeds the 1 MiB safety limit");
    }
    let canonical = fs::canonicalize(path)?;
    if !canonical.starts_with(&canonical_root) {
        bail!("rollback artifact escapes its trusted release directory");
    }
    Ok(fs::read(canonical)?)
}

fn ensure_rollback_quiescent(database: &WorkspaceDatabase<'_>) -> Result<()> {
    if !database.is_release_rollback_quiescent()? {
        bail!(
            "rollback requires all running tasks to reach a safe checkpoint and every worker/coordinator lease to be released"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
struct RollbackResolutionContext {
    codex_execution: Option<ResolvedExecutable>,
}

impl RollbackResolutionContext {
    fn working_directory_for(&self, component: &str) -> Option<&Path> {
        is_codex_rollback_component(component)
            .then_some(
                self.codex_execution
                    .as_ref()
                    .map(|evidence| evidence.validation.working_directory.as_path()),
            )
            .flatten()
    }

    fn resolved_executable_for(&self, component: &str) -> Option<&Path> {
        is_codex_rollback_component(component)
            .then_some(
                self.codex_execution
                    .as_ref()
                    .map(|evidence| evidence.path.as_path()),
            )
            .flatten()
    }
}

fn rollback_resolution_context(
    repository: &Path,
    state: &StatePaths,
    _config: &RootConfig,
    steps: &[RollbackManifestStep],
) -> Result<RollbackResolutionContext> {
    if !steps
        .iter()
        .any(|step| is_codex_rollback_component(&step.component))
    {
        return Ok(RollbackResolutionContext::default());
    }
    if !state.database.exists() {
        bail!(
            "Codex rollback requires persisted process execution evidence; state database is missing"
        );
    }
    let database = open_ready_database(state)?;
    let database = workspace_for_repository(&database, repository)?;
    let selected = database
        .latest_completed_writable_attempt(ProviderId::Codex)?
        .ok_or_else(|| anyhow!("Codex rollback has no completed writable provider attempt"))?;
    let attempt_id = selected.attempt_id;
    let task_id = selected.task_id;
    let persisted = selected
        .worker_result
        .ok_or_else(|| anyhow!("selected Codex attempt has no process execution evidence"))?;
    let worker_result: WorkerResult = serde_json::from_value(persisted.clone())
        .context("selected Codex attempt does not contain a complete WorkerResult v1")?;
    if worker_result.attempt_id != attempt_id
        || worker_result.task_id != task_id
        || worker_result.provider != ProviderId::Codex
    {
        bail!(
            "selected Codex attempt process execution evidence does not match its attempt identity"
        );
    }
    let process_execution: ResolvedExecutable = serde_json::from_value(
        persisted
            .get("process_execution")
            .cloned()
            .ok_or_else(|| anyhow!("selected Codex attempt has no process execution evidence"))?,
    )
    .context("selected Codex attempt has malformed process execution evidence")?;
    validate_resolution_evidence(&process_execution)
        .context("selected Codex attempt has invalid process execution evidence")?;
    if process_execution.configured.is_absolute() {
        return Ok(RollbackResolutionContext {
            codex_execution: Some(process_execution),
        });
    }
    let stored = database
        .active_worktree(task_id)?
        .ok_or_else(|| anyhow!("Codex rollback invocation has no active isolated worktree"))?;
    let worktree = validate_recovered_worktree(repository, state, stored)?;
    let canonical_worktree = fs::canonicalize(&worktree.path)?;
    if fs::canonicalize(&process_execution.validation.working_directory)? != canonical_worktree {
        bail!(
            "selected Codex attempt process execution evidence does not match its trusted worktree"
        );
    }
    Ok(RollbackResolutionContext {
        codex_execution: Some(process_execution),
    })
}

fn is_codex_rollback_component(component: &str) -> bool {
    matches!(component, "codex" | "codex_binary")
}

fn trusted_rollback_steps(
    repository: &Path,
    config_path: &Path,
    state: &StatePaths,
    config: &RootConfig,
    resolution_context: &RollbackResolutionContext,
    release_root: &Path,
    steps: Vec<RollbackManifestStep>,
) -> Result<Vec<orchestrator_engine::RollbackStep>> {
    let canonical_release = fs::canonicalize(release_root).with_context(|| {
        format!(
            "rollback release directory is missing: {}",
            release_root.display()
        )
    })?;
    steps
        .into_iter()
        .map(|step| {
            let backup_source = resolve_from(&canonical_release, &step.backup_source);
            let backup_source = canonical_regular_file_below(&canonical_release, &backup_source)?;
            let trusted_destination = trusted_rollback_destination(
                repository,
                config_path,
                state,
                config,
                &step.component,
                resolution_context,
            )?;
            let manifest_base = resolution_context
                .working_directory_for(&step.component)
                .unwrap_or(repository);
            let manifest_destination = resolve_from(manifest_base, &step.destination);
            let manifest_destination =
                fs::canonicalize(&manifest_destination).with_context(|| {
                    format!(
                        "rollback destination is missing: {}",
                        manifest_destination.display()
                    )
                })?;
            if manifest_destination != trusted_destination {
                bail!(
                    "rollback component `{}` targets an unapproved destination",
                    step.component
                );
            }
            Ok(orchestrator_engine::RollbackStep {
                component: step.component,
                backup_source,
                destination: trusted_destination,
            })
        })
        .collect()
}

fn canonical_regular_file_below(root: &Path, path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("rollback source is missing: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("rollback source must be a non-symlink regular file");
    }
    let canonical = fs::canonicalize(path)?;
    if !canonical.starts_with(root) {
        bail!("rollback source escapes the selected release directory");
    }
    Ok(canonical)
}

fn trusted_rollback_destination(
    repository: &Path,
    config_path: &Path,
    _state: &StatePaths,
    _config: &RootConfig,
    component: &str,
    resolution_context: &RollbackResolutionContext,
) -> Result<PathBuf> {
    let path = match component {
        "colay" | "colay_binary" | "orchestrator" | "orchestrator_binary" => {
            std::env::current_exe()?
        }
        "config" => resolve_from(repository, config_path),
        "codex" | "codex_binary" => resolution_context
            .resolved_executable_for(component)
            .ok_or_else(|| anyhow!("rollback has no persisted Codex process execution evidence"))?
            .to_path_buf(),
        "database" | "state_database" => bail!(
            "release rollback cannot replace the live task database; use validated schema migration recovery"
        ),
        _ => bail!("rollback manifest contains unsupported component `{component}`"),
    };
    let metadata = fs::symlink_metadata(&path).with_context(|| {
        format!(
            "trusted rollback destination is missing: {}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("trusted rollback destination must be a non-symlink regular file");
    }
    Ok(fs::canonicalize(path)?)
}

fn rollback_manager(
    state: &StatePaths,
    steps: &[orchestrator_engine::RollbackStep],
) -> Result<orchestrator_engine::RollbackManager> {
    let mut roots = vec![state.root.clone()];
    for step in steps {
        if let Some(parent) = step.destination.parent()
            && !roots.iter().any(|root| root == parent)
        {
            roots.push(parent.to_path_buf());
        }
    }
    Ok(orchestrator_engine::RollbackManager::new(roots)?)
}

fn validate_sealed_rollback_destinations(
    repository: &Path,
    config_path: &Path,
    state: &StatePaths,
    config: &RootConfig,
    resolution_context: &RollbackResolutionContext,
    plan: &orchestrator_engine::RollbackRecoveryPlan,
) -> Result<()> {
    for step in &plan.steps {
        let trusted = trusted_rollback_destination(
            repository,
            config_path,
            state,
            config,
            &step.component,
            resolution_context,
        )?;
        if step.destination != trusted {
            bail!("sealed rollback plan contains an unapproved destination");
        }
        canonical_regular_file_below(&state.root, &step.backup_source)?;
    }
    Ok(())
}

async fn tui(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: ConfigEnvironment,
    runtime: &ConfigRuntime,
    selector: &TaskSelector,
    json_output: bool,
) -> Result<()> {
    if !runtime.effective.config().features.orchestrator_tui {
        bail!("orchestrator TUI is disabled by configuration");
    }
    let mut driver = crate::chat_tui::SqliteWorkspaceDriver::connect(
        repository,
        runtime.effective.config(),
        cli_config,
        selector.task_id.as_deref(),
    )
    .await?;
    let mut workspace_state = orchestrator_tui::chat::WorkspaceState::default();
    loop {
        match orchestrator_tui::chat::run_workspace_session(&mut driver, &mut workspace_state)? {
            orchestrator_tui::chat::WorkspaceExit::Quit => return Ok(()),
            orchestrator_tui::chat::WorkspaceExit::Administration => {
                Box::pin(legacy_tui(
                    repository,
                    cli_config,
                    environment.clone(),
                    runtime,
                    selector,
                    json_output,
                ))
                .await?;
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn legacy_tui(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: ConfigEnvironment,
    runtime: &ConfigRuntime,
    selector: &TaskSelector,
    json_output: bool,
) -> Result<()> {
    let config = runtime.effective.config();
    if !config.features.orchestrator_tui {
        bail!("orchestrator TUI is disabled by configuration");
    }
    let defaults = usage_defaults(&config.orchestrator)?;
    let client = crate::ipc_client::DaemonClient::connect_or_start(repository, cli_config).await?;
    let dashboard = client
        .request(
            "workspace.dashboard",
            json!({"task_id": selector.task_id.as_deref(), "defaults": defaults}),
        )
        .await?
        .outcome["data"]
        .clone();
    let tasks = serde_json::from_value::<Vec<StoredTask>>(dashboard["tasks"].clone())?;
    let task = tasks.first();
    let usage = serde_json::from_value::<Vec<UsageSnapshot>>(dashboard["usage"].clone())?;
    let task_envelope = task
        .map(|stored| serde_json::from_value::<TaskEnvelope>(stored.envelope.clone()))
        .transpose()?;
    let latest_routing =
        serde_json::from_value::<Option<RoutingAuditRecord>>(dashboard["routing"].clone())?;
    let latest_handover =
        serde_json::from_value::<Option<StoredHandover>>(dashboard["handover"].clone())?;
    let handover_count = serde_json::from_value::<u32>(dashboard["handover_count"].clone())?;
    let latest_verification = serde_json::from_value::<
        Option<orchestrator_domain::VerificationResult>,
    >(dashboard["verification"].clone())?;
    let provider_health =
        serde_json::from_value::<Vec<ProviderHealth>>(dashboard["provider_health"].clone())?;
    let entered_by = std::env::var("USERNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "local-administrator".to_owned());
    let usage_override_drafts = usage
        .iter()
        .map(|snapshot| orchestrator_tui::UsageOverrideDraft {
            provider: snapshot.provider.as_str().to_owned(),
            used: snapshot.used,
            limit: snapshot.limit,
            remaining: snapshot.remaining,
            entered_by: entered_by.clone(),
        })
        .collect();
    let provider_controls = provider_configs(&config.orchestrator)
        .map(
            |(provider, config)| orchestrator_tui::ProviderControlOption {
                provider: provider.as_str().to_owned(),
                enabled: config.enabled,
            },
        )
        .collect();
    let snapshot = orchestrator_tui::DashboardSnapshot {
        task: task.map_or_else(orchestrator_tui::TaskPanel::default, |task| {
            let assessment = task_envelope
                .as_ref()
                .and_then(|envelope| envelope.assessment.as_ref());
            orchestrator_tui::TaskPanel {
                id: task.task_id.to_string(),
                objective: task.objective.clone(),
                state: format!("{:?}", task.state),
                difficulty: assessment.map_or_else(
                    || "Unknown".to_owned(),
                    |value| format!("{:?} ({})", value.difficulty, value.total_score),
                ),
                risks: assessment
                    .map(|value| {
                        value
                            .risk_tags
                            .iter()
                            .map(|risk| format!("{risk:?}"))
                            .collect()
                    })
                    .unwrap_or_default(),
                phase: format!("{:?}", task.state),
            }
        }),
        providers: usage
            .iter()
            .map(|usage| orchestrator_tui::ProviderRow {
                provider: usage.provider.as_str().to_owned(),
                usage: usage
                    .used_percent
                    .map_or_else(|| "Unknown".to_owned(), |value| format!("{value:.1}%")),
                reset: usage
                    .resets_at
                    .map_or_else(|| "Unknown".to_owned(), |value| value.to_rfc3339()),
                source: format!("{:?}", usage.source),
                confidence: format!("{:?}", usage.confidence),
                health: provider_health
                    .iter()
                    .find(|health| health.provider == usage.provider)
                    .map_or_else(
                        || "Unknown".to_owned(),
                        |health| format!("{:?}", health.status),
                    ),
            })
            .collect(),
        routing: latest_routing.as_ref().map_or_else(
            orchestrator_tui::RoutingPanel::default,
            |routing| orchestrator_tui::RoutingPanel {
                selected: routing.selected_provider.map_or_else(
                    || "Blocked".to_owned(),
                    |provider| provider.as_str().to_owned(),
                ),
                profile: routing.model_profile.clone().unwrap_or_default(),
                effort: routing.effort.clone().unwrap_or_default(),
                rationale: serde_json::from_value(routing.rationale.clone()).unwrap_or_default(),
                alternatives: routing_alternatives(&routing.candidates, routing.selected_provider),
            },
        ),
        handover: latest_handover.as_ref().map_or_else(
            orchestrator_tui::HandoverPanel::default,
            |handover| orchestrator_tui::HandoverPanel {
                previous_provider: handover.bundle.current_worker.as_str().to_owned(),
                next_provider: handover.bundle.recommended_next_worker.as_str().to_owned(),
                reason: handover.reason.clone(),
                checkpoint: handover.checkpoint_id.to_string(),
                count: handover_count,
            },
        ),
        verification: latest_verification.as_ref().map_or_else(
            orchestrator_tui::VerificationPanel::default,
            |verification| orchestrator_tui::VerificationPanel {
                changed_files: verification.changed_files.len(),
                tests: verification
                    .checks
                    .iter()
                    .filter(|check| check.kind == orchestrator_domain::VerificationCheckKind::Test)
                    .map(|check| format!("{}: {:?}", check.name, check.status))
                    .collect(),
                failures: verification
                    .checks
                    .iter()
                    .filter(|check| check.status != VerificationStatus::Pass)
                    .map(|check| check.detail.clone().unwrap_or_else(|| check.name.clone()))
                    .collect(),
                approval_required: verification.requires_approval,
            },
        ),
        controls: orchestrator_tui::ControlContext {
            automatic_routing_enabled: config.orchestrator.automatic_routing,
            manual_provider: None,
            handover_target: None,
            usage_override: None,
        },
        provider_controls,
        usage_override_drafts,
        model_profiles: effective_profile_rows(config, &RootConfig::default())?
            .into_iter()
            .map(|row| orchestrator_tui::ModelProfileRow {
                provider: row.provider,
                profile: row.profile,
                model: row.model,
                effort: row.effort.unwrap_or_default(),
                description: row.description,
                customized: row.source == ProfileSource::Customized,
            })
            .collect(),
    };
    let action = orchestrator_tui::run(&snapshot)?;
    match action {
        orchestrator_tui::ControlAction::SetAutomaticRouting { enabled } => {
            let mut document = load_edit_document(&runtime.explicit_edit_path)?;
            ensure_override_table(document.as_table_mut(), "orchestrator")?
                .insert("automatic_routing", toml_edit::value(enabled));
            write_override_via_daemon(
                repository,
                cli_config,
                &document,
                &runtime.explicit_edit_path,
            )
            .await?;
            let _ = load_config_runtime(repository, cli_config, environment)?;
            emit(
                json_output,
                "automatic_routing_updated",
                &json!({"enabled": enabled}),
            )
        }
        orchestrator_tui::ControlAction::SetProviderEnabled { provider, enabled } => {
            set_provider_enabled(
                repository,
                cli_config,
                environment,
                runtime,
                parse_provider_id(&provider)?,
                enabled,
                json_output,
            )
            .await
        }
        orchestrator_tui::ControlAction::SelectProvider { task_id, provider }
        | orchestrator_tui::ControlAction::Handover {
            task_id,
            to_provider: provider,
        } => {
            control(
                repository,
                cli_config,
                RequiredTask { task_id },
                "handover",
                json!({"to": parse_provider_id(&provider)?}),
                json_output,
            )
            .await
        }
        orchestrator_tui::ControlAction::Pause { task_id } => {
            control(
                repository,
                cli_config,
                RequiredTask { task_id },
                "pause",
                json!({}),
                json_output,
            )
            .await
        }
        orchestrator_tui::ControlAction::Resume { task_id } => {
            resume_task(
                repository,
                cli_config,
                &runtime.effective,
                &RequiredTask { task_id },
                json_output,
            )
            .await
        }
        orchestrator_tui::ControlAction::Cancel { task_id } => {
            control(
                repository,
                cli_config,
                RequiredTask { task_id },
                "cancel",
                json!({}),
                json_output,
            )
            .await
        }
        orchestrator_tui::ControlAction::UsageOverride {
            provider,
            used,
            limit,
            remaining,
            entered_by,
        } => {
            usage_override(
                repository,
                cli_config,
                &runtime.effective,
                UsageOverrideArgs {
                    provider: parse_provider_name(&provider)?,
                    used,
                    limit,
                    remaining,
                    entered_by,
                },
                json_output,
            )
            .await
        }
        orchestrator_tui::ControlAction::SetModelProfile {
            provider,
            profile,
            model,
            effort,
        } => {
            set_model_profile(
                repository,
                cli_config,
                environment,
                runtime,
                parse_provider_name(&provider)?,
                parse_profile_name(&profile)?,
                &model,
                Some(parse_effort_name(&effort)?),
                json_output,
            )
            .await
        }
        orchestrator_tui::ControlAction::ResetModelProfile { provider, profile } => {
            reset_model_profile(
                repository,
                cli_config,
                environment,
                runtime,
                parse_provider_name(&provider)?,
                parse_profile_name(&profile)?,
                json_output,
            )
            .await
        }
        orchestrator_tui::ControlAction::Quit => Ok(()),
    }
}

fn parse_provider_id(value: &str) -> Result<ProviderId> {
    Ok(ProviderId::from(parse_provider_name(value)?))
}

fn parse_provider_name(value: &str) -> Result<crate::args::ProviderName> {
    match value.to_ascii_lowercase().as_str() {
        "gemini" => Ok(crate::args::ProviderName::Gemini),
        "agy" => Ok(crate::args::ProviderName::Agy),
        "codex" => Ok(crate::args::ProviderName::Codex),
        "claude" => Ok(crate::args::ProviderName::Claude),
        _ => bail!("unknown approved provider `{value}`"),
    }
}

fn parse_profile_name(value: &str) -> Result<ProfileName> {
    match value {
        "economy" => Ok(ProfileName::Economy),
        "standard" => Ok(ProfileName::Standard),
        "premium" => Ok(ProfileName::Premium),
        _ => bail!("unknown model profile `{value}`"),
    }
}

fn parse_effort_name(value: &str) -> Result<EffortName> {
    match value {
        "low" => Ok(EffortName::Low),
        "medium" => Ok(EffortName::Medium),
        "high" => Ok(EffortName::High),
        _ => bail!("unsupported reasoning effort `{value}`"),
    }
}

fn routing_alternatives(value: &Value, selected: Option<ProviderId>) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|candidate| candidate.get("eligible").and_then(Value::as_bool) == Some(true))
        .filter_map(|candidate| candidate.get("provider").and_then(Value::as_str))
        .filter(|provider| selected.is_none_or(|selected| provider != &selected.as_str()))
        .map(ToOwned::to_owned)
        .collect()
}

async fn routing_candidates(
    config: &OrchestratorConfig,
    global_database: &Database,
    database: &WorkspaceDatabase<'_>,
    assessment: &orchestrator_domain::TaskAssessment,
    manually_requested: Option<ProviderId>,
    task_id: TaskId,
    correlation_id: CorrelationId,
) -> Result<Vec<RoutingCandidate>> {
    let now = Utc::now();
    let redaction = process_redaction(config);
    let mut candidates = Vec::new();
    for (provider, provider_config) in provider_configs(config) {
        let (health, capabilities) =
            match probe_provider(provider, provider_config, &redaction).await {
                Ok(result) => result,
                Err(error) => (
                    ProviderHealth {
                        provider,
                        status: HealthStatus::Unhealthy,
                        checked_at: now,
                        latency_ms: None,
                        consecutive_failures: 1,
                        detail: Some(error.to_string()),
                    },
                    ProviderCapabilities::unsupported(provider),
                ),
            };
        persist_health(global_database, &health)?;
        if provider == ProviderId::Codex && health.status != HealthStatus::Healthy {
            append_event(
                database,
                Some(task_id),
                EventType::CompatibilityWarning,
                None,
                None,
                EventActor::Orchestrator,
                correlation_id,
                json!({
                    "provider": provider,
                    "health": health.status,
                    "detail": &health.detail,
                    "capabilities": &capabilities,
                }),
            )?;
        }
        let snapshot =
            collect_usage(provider, provider_config, global_database, now, &redaction).await?;
        let budgets =
            budget_for_snapshot(config, provider_config, &snapshot, global_database, now)?;
        let calibrated_remaining_work_units =
            provider_config
                .quota_units_per_work_unit
                .and_then(|quota_units_per_work_unit| {
                    let limit = snapshot.limit?;
                    budgets
                        .iter()
                        .filter_map(|budget| budget.safe_remaining_percent)
                        .map(|percent| {
                            (percent.max(0.0) / 100.0 * limit) / quota_units_per_work_unit
                        })
                        .reduce(f64::min)
                });
        let available_profiles = configured_profiles(config, provider);
        candidates.push(RoutingCandidate {
            provider,
            enabled: provider_config.enabled
                && if config.automatic_routing {
                    manually_requested.is_none_or(|requested| requested == provider)
                } else {
                    manually_requested == Some(provider)
                },
            capabilities,
            health,
            budgets,
            available_profiles,
            // Provider-specific units are compared only after an explicit administrator
            // calibration. Missing calibration remains unknown rather than guessed.
            calibrated_remaining_work_units,
            recent_failure_rate: recent_failure_rate(database, provider)?,
            admin_priority: provider_config.priority.clamp(0, 100),
            handover_cost: 0.25,
        });
    }
    if assessment.difficulty == orchestrator_domain::Difficulty::Critical && candidates.is_empty() {
        bail!("critical task has no configured approved providers");
    }
    Ok(candidates)
}

async fn collect_usage(
    provider: ProviderId,
    config: &ProviderConfig,
    database: &Database,
    now: DateTime<Utc>,
    redaction: &RedactionConfig,
) -> Result<UsageSnapshot> {
    if let orchestrator_state::UsageProbeConfig::Command {
        executable,
        args,
        format,
    } = &config.usage_probe
    {
        let probe_result = async {
            if format != "json" {
                bail!("only JSON usage probes are supported");
            }
            let working_directory = current_repository()?;
            let result = run_bounded_command(
                executable,
                args.iter().map(String::as_str),
                &working_directory,
                30,
                redaction,
            )
            .await?;
            if !result.success() {
                bail!(
                    "configured {} usage probe failed: {}",
                    provider.as_str(),
                    result.stderr.redacted_text
                );
            }
            let mut snapshot = orchestrator_providers::parse_usage_probe_output(
                provider,
                quota_scope(provider, config)?,
                &result.stdout.bytes,
                now,
            )?;
            let window = period_window(&reset_policy(config)?, now)?;
            normalize_usage_window(&mut snapshot, &window, now)?;
            snapshot.validate()?;
            Ok::<_, anyhow::Error>(snapshot)
        }
        .await;
        match probe_result {
            Ok(snapshot) => {
                persist_global_usage(database, &snapshot)?;
                return Ok(snapshot);
            }
            Err(error) => {
                tracing::warn!(provider = provider.as_str(), %error, "configured usage probe failed; falling back to current-period local evidence");
            }
        }
    }

    if let Some(snapshot) = preferred_recorded_usage_for_period(database, provider, config, now)? {
        return Ok(snapshot);
    }
    let mut unknown = UsageSnapshot::unknown(provider, quota_scope(provider, config)?, now);
    unknown.quota_period = parse_quota_period(&config.quota_period)?;
    let window = period_window(&reset_policy(config)?, now)?;
    unknown.period_started_at = Some(window.started_at);
    unknown.resets_at = Some(window.resets_at);
    Ok(unknown)
}

fn normalize_usage_window(
    snapshot: &mut UsageSnapshot,
    configured: &orchestrator_policy::PeriodWindow,
    now: DateTime<Utc>,
) -> Result<()> {
    match (snapshot.period_started_at, snapshot.resets_at) {
        (Some(started_at), Some(resets_at)) => {
            if started_at > now || resets_at <= now || started_at >= resets_at {
                bail!("usage probe returned a stale or invalid quota window");
            }
            let tolerance = chrono::TimeDelta::seconds(60);
            if (started_at - configured.started_at).abs() > tolerance
                || (resets_at - configured.resets_at).abs() > tolerance
            {
                bail!("usage probe quota window does not match configured reset policy");
            }
        }
        (None, None) => {}
        _ => bail!("usage probe must provide both period boundaries or neither"),
    }
    snapshot.period_started_at = Some(configured.started_at);
    snapshot.resets_at = Some(configured.resets_at);
    Ok(())
}

fn budget_for_snapshot(
    config: &OrchestratorConfig,
    provider_config: &ProviderConfig,
    snapshot: &UsageSnapshot,
    global_database: &Database,
    now: DateTime<Utc>,
) -> Result<Vec<orchestrator_policy::BudgetForecast>> {
    let policy = reset_policy(provider_config)?;
    let window = if let (Some(started_at), Some(resets_at)) =
        (snapshot.period_started_at, snapshot.resets_at)
    {
        orchestrator_policy::PeriodWindow::new(started_at, resets_at)?
    } else {
        period_window(&policy, now)?
    };
    let history = usage_history(global_database, snapshot.provider)?
        .into_iter()
        .filter(|observation| observation.quota_scope == snapshot.quota_scope)
        .filter(|observation| {
            observation.collected_at >= window.started_at
                && observation.collected_at < window.resets_at
        })
        .collect::<Vec<_>>();
    Ok(vec![BudgetForecaster::forecast(
        snapshot,
        &history,
        &window,
        now,
        false,
        &forecast_config(config),
    )?])
}

fn reset_policy(provider_config: &ProviderConfig) -> Result<ResetPolicy> {
    let timezone = provider_config
        .reset_timezone
        .parse::<chrono_tz::Tz>()
        .with_context(|| format!("invalid timezone {}", provider_config.reset_timezone))?;
    let quota_period = parse_quota_period(&provider_config.quota_period)?;
    Ok(match quota_period {
        QuotaPeriod::CalendarDay => ResetPolicy::calendar_day(timezone),
        QuotaPeriod::CalendarMonth => {
            ResetPolicy::calendar_month(timezone, provider_config.reset_day.unwrap_or(1))
        }
        QuotaPeriod::RollingDay | QuotaPeriod::RollingMonth => ResetPolicy {
            quota_period,
            timezone,
            reset_day: None,
            rolling_anchor: provider_config.rolling_anchor,
            rolling_period_seconds: provider_config
                .rolling_period_seconds
                .and_then(|seconds| i64::try_from(seconds).ok()),
            custom_started_at: None,
            custom_resets_at: None,
        },
        QuotaPeriod::Custom => ResetPolicy {
            quota_period: QuotaPeriod::Custom,
            timezone,
            reset_day: None,
            rolling_anchor: None,
            rolling_period_seconds: None,
            custom_started_at: provider_config.custom_started_at,
            custom_resets_at: provider_config.custom_resets_at,
        },
    })
}

fn forecast_config(config: &OrchestratorConfig) -> ForecastConfig {
    ForecastConfig {
        minimum_progress: config.minimum_progress,
        grace_window_daily_seconds: i64::try_from(config.daily_grace_minutes)
            .unwrap_or(i64::MAX / 60)
            .saturating_mul(60),
        grace_window_monthly_seconds: i64::try_from(config.monthly_grace_minutes)
            .unwrap_or(i64::MAX / 60)
            .saturating_mul(60),
        ewma_alpha: config.forecast_alpha,
        minimum_observations: usize::try_from(config.minimum_forecast_observations)
            .unwrap_or(usize::MAX),
        reserve_percent: config.critical_reserve_percent,
        ..ForecastConfig::default()
    }
}

#[allow(clippy::too_many_lines)]
async fn probe_provider(
    provider: ProviderId,
    config: &ProviderConfig,
    redaction: &RedactionConfig,
) -> Result<(ProviderHealth, ProviderCapabilities)> {
    let started = std::time::Instant::now();
    if provider == ProviderId::Codex {
        let report = probe_codex(&config.executable, redaction)?;
        let guard = StartupGuard::evaluate(Some(&report), &[], true, true, true);
        let mut capabilities = StartupGuard::codex_domain_capabilities(&report);
        match guard.codex_policy {
            CodexExecutionPolicy::ReadWrite => {}
            CodexExecutionPolicy::ReadOnly => {
                capabilities.writable = CapabilitySupport::Unsupported;
            }
            CodexExecutionPolicy::Disabled => {
                let version = capabilities.version.clone();
                let evidence = capabilities.evidence.clone();
                capabilities = ProviderCapabilities::unsupported(ProviderId::Codex);
                capabilities.version = version;
                capabilities.evidence = evidence;
            }
        }
        let status = match (report.status, guard.codex_policy) {
            (_, CodexExecutionPolicy::Disabled) => HealthStatus::Unhealthy,
            (_, CodexExecutionPolicy::ReadOnly)
            | (
                codex_compat::CompatibilityStatus::CompatibleWithWarnings
                | codex_compat::CompatibilityStatus::Untested,
                CodexExecutionPolicy::ReadWrite,
            ) => HealthStatus::Degraded,
            (codex_compat::CompatibilityStatus::Compatible, CodexExecutionPolicy::ReadWrite) => {
                HealthStatus::Healthy
            }
            (codex_compat::CompatibilityStatus::Incompatible, CodexExecutionPolicy::ReadWrite) => {
                HealthStatus::Unhealthy
            }
        };
        return Ok((
            ProviderHealth {
                provider,
                status,
                checked_at: Utc::now(),
                latency_ms: Some(duration_millis(started.elapsed())),
                consecutive_failures: u32::from(status == HealthStatus::Unhealthy),
                detail: (!report.diagnostics.is_empty() || !guard.warnings.is_empty()).then(|| {
                    report
                        .diagnostics
                        .iter()
                        .chain(&guard.warnings)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ")
                }),
            },
            capabilities,
        ));
    }

    let repository = current_repository()?;
    let version =
        diagnostic_command(&config.executable, ["--version"], &repository, redaction).await?;
    if !version.success() {
        bail!("{} --version failed", provider.as_str());
    }
    let help = diagnostic_command(&config.executable, ["--help"], &repository, redaction).await?;
    let help_text = format!(
        "{}\n{}",
        help.stdout.redacted_text, help.stderr.redacted_text
    );
    let structured = match provider {
        ProviderId::Agy => {
            help_text.contains("--print")
                && help_text.contains("--mode")
                && help_text.contains("plan")
                && help_text.contains("accept-edits")
                && help_text.contains("--sandbox")
        }
        ProviderId::Claude => {
            help_text.contains("--output-format")
                && (help_text.contains("stream-json") || help_text.contains("json"))
                && (help_text.contains("-p") || help_text.contains("--print"))
        }
        ProviderId::Gemini => {
            (help_text.contains("--output-format") || help_text.contains("stream-json"))
                && (help_text.contains("-p") || help_text.contains("--prompt"))
        }
        ProviderId::Codex => false,
    };
    let support = if structured && provider == ProviderId::Agy {
        CapabilitySupport::Degraded
    } else if structured {
        CapabilitySupport::Advertised
    } else {
        CapabilitySupport::Unsupported
    };
    let mut capabilities = ProviderCapabilities::unsupported(provider);
    capabilities.version = Some(version.stdout.redacted_text.trim().to_owned());
    capabilities.non_interactive = support;
    capabilities.structured_output = support;
    capabilities.writable = support;
    capabilities.read_only = support;
    capabilities.reasoning_effort = if provider == ProviderId::Agy {
        CapabilitySupport::Unsupported
    } else {
        CapabilitySupport::Advertised
    };
    capabilities.evidence = vec!["public --version and --help output".to_owned()];
    let status = if structured {
        HealthStatus::Healthy
    } else {
        HealthStatus::Degraded
    };
    Ok((
        ProviderHealth {
            provider,
            status,
            checked_at: Utc::now(),
            latency_ms: Some(duration_millis(started.elapsed())),
            consecutive_failures: 0,
            detail: (!structured)
                .then(|| "structured non-interactive flags were not detected".to_owned()),
        },
        capabilities,
    ))
}

fn probe_codex(executable: &str, redaction: &RedactionConfig) -> Result<CodexProbeReport> {
    let schema = tempfile::tempdir()?;
    let probe = CapabilityProbe::default();
    let mut source = ProcessCapabilitySource {
        executable: PathBuf::from(executable),
        working_directory: current_repository()?,
        handle: Handle::current(),
        redaction: redaction.clone(),
    };
    let input = tokio::task::block_in_place(|| probe.collect(&mut source, schema.path()))?;
    Ok(probe.evaluate(&input))
}

struct ProcessCapabilitySource {
    executable: PathBuf,
    working_directory: PathBuf,
    handle: Handle,
    redaction: RedactionConfig,
}

impl CapabilitySource for ProcessCapabilitySource {
    type Error = ProcessError;

    fn run(&mut self, command: &ProbeCommand) -> Result<ProbeOutput, Self::Error> {
        let mut spec = CommandSpec::new(&self.executable)
            .args(command.args.clone())
            .current_dir(&self.working_directory);
        spec.timeout = Duration::from_secs(20);
        spec.stdout_limit = 4 * 1024 * 1024;
        spec.stderr_limit = 2 * 1024 * 1024;
        spec.redaction = self.redaction.clone();
        let result = self
            .handle
            .block_on(ProcessRunner.run(spec, CancellationToken::new()))?;
        Ok(ProbeOutput {
            exit_code: result.exit_code,
            stdout: result.stdout.redacted_text,
            stderr: result.stderr.redacted_text,
        })
    }
}

async fn diagnostic_command<const N: usize>(
    executable: &str,
    args: [&str; N],
    working_directory: &Path,
    redaction: &RedactionConfig,
) -> Result<orchestrator_process::ProcessResult> {
    run_bounded_command(executable, args, working_directory, 20, redaction).await
}

async fn run_bounded_command<I, S>(
    executable: &str,
    args: I,
    working_directory: &Path,
    timeout_seconds: u64,
    redaction: &RedactionConfig,
) -> Result<orchestrator_process::ProcessResult>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let mut spec = CommandSpec::new(executable)
        .args(args)
        .current_dir(working_directory);
    spec.timeout = Duration::from_secs(timeout_seconds);
    spec.stdout_limit = 4 * 1024 * 1024;
    spec.stderr_limit = 2 * 1024 * 1024;
    spec.redaction = redaction.clone();
    ProcessRunner
        .run(spec, CancellationToken::new())
        .await
        .map_err(Into::into)
}

fn transition_task(
    database: &WorkspaceDatabase<'_>,
    task_id: TaskId,
    next: TaskState,
    guards: orchestrator_domain::TransitionGuards,
    correlation_id: CorrelationId,
    reason: &str,
) -> Result<TaskState> {
    transition_task_projection(
        database,
        task_id,
        next,
        false,
        guards,
        correlation_id,
        reason,
    )
}

fn state_transition_event(
    task_id: TaskId,
    from: TaskState,
    to: TaskState,
    correlation_id: CorrelationId,
    reason: &str,
    payload: Value,
) -> TaskEvent {
    TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id: None,
        task_id: Some(task_id),
        occurred_at: Utc::now(),
        event_type: EventType::StateTransitioned,
        from_state: Some(from),
        to_state: Some(to),
        reason: Some(reason.to_owned()),
        actor: EventActor::Orchestrator,
        correlation_id,
        causation_id: None,
        payload,
        previous_hash: None,
        event_hash: String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn transition_task_projection(
    database: &WorkspaceDatabase<'_>,
    task_id: TaskId,
    next: TaskState,
    paused: bool,
    mut guards: orchestrator_domain::TransitionGuards,
    correlation_id: CorrelationId,
    reason: &str,
) -> Result<TaskState> {
    let stored = database
        .load_task(task_id)?
        .ok_or_else(|| anyhow!("task {task_id} does not exist"))?;
    let current = stored.state;
    if current == TaskState::Blocked {
        guards.resume_point = stored.resume_state;
    }
    let resume_state = (next == TaskState::Blocked).then_some(current);
    let event = state_transition_event(
        task_id,
        current,
        next,
        correlation_id,
        reason,
        json!({"paused": paused}),
    );
    database.transition_task_with_event(
        task_id,
        stored.revision,
        current,
        next,
        resume_state,
        paused,
        &guards,
        Utc::now(),
        event,
    )?;
    Ok(current)
}

fn persist_routing(
    database: &WorkspaceDatabase<'_>,
    decision: &RoutingDecision,
    task: &TaskEnvelope,
) -> Result<()> {
    let assessment = task
        .assessment
        .as_ref()
        .ok_or_else(|| anyhow!("task assessment is missing"))?;
    database.record_routing_decision(decision, assessment)?;
    Ok(())
}

fn persist_global_usage(database: &Database, snapshot: &UsageSnapshot) -> Result<()> {
    database.record_global_usage_snapshot(snapshot)?;
    Ok(())
}

fn persist_health(database: &Database, health: &ProviderHealth) -> Result<()> {
    database.record_provider_health(health)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn append_event(
    database: &WorkspaceDatabase<'_>,
    task_id: Option<TaskId>,
    event_type: EventType,
    from_state: Option<TaskState>,
    to_state: Option<TaskState>,
    actor: EventActor,
    correlation_id: CorrelationId,
    payload: Value,
) -> Result<TaskEvent> {
    Ok(database.append_event(TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id: None,
        task_id,
        occurred_at: Utc::now(),
        event_type,
        from_state,
        to_state,
        reason: None,
        actor,
        correlation_id,
        causation_id: None,
        payload,
        previous_hash: None,
        event_hash: String::new(),
    })?)
}

fn append_event_if_schema_available(
    database: &Database,
    event_type: EventType,
    payload: Value,
) -> Result<()> {
    if let Some(workspace) = legacy_migration_workspace_if_present(database)? {
        append_event(
            &workspace,
            None,
            event_type,
            None,
            None,
            EventActor::Administrator,
            CorrelationId::new(),
            payload,
        )?;
    }
    Ok(())
}

fn preferred_recorded_usage_for_period(
    database: &Database,
    provider: ProviderId,
    config: &ProviderConfig,
    now: DateTime<Utc>,
) -> Result<Option<UsageSnapshot>> {
    let scope = quota_scope(provider, config)?;
    let window = period_window(&reset_policy(config)?, now)?;
    let priority = |source: &UsageSource| match source {
        UsageSource::OfficialCli | UsageSource::OfficialProtocol => 0,
        UsageSource::ConfiguredProbe => 1,
        UsageSource::LocalLedger => 2,
        UsageSource::ManualOverride => 3,
        UsageSource::Unknown => 4,
    };
    let snapshots = usage_history(database, provider)?
        .into_iter()
        .filter(|snapshot| snapshot.quota_scope == scope)
        .filter(|snapshot| {
            snapshot.collected_at >= window.started_at && snapshot.collected_at < window.resets_at
        })
        .collect::<Vec<_>>();
    if let Some(exhausted) = snapshots
        .iter()
        .filter(|snapshot| snapshot.confidence == UsageConfidence::Confirmed)
        .filter(|snapshot| snapshot.remaining.is_some_and(|remaining| remaining <= 0.0))
        .max_by_key(|snapshot| snapshot.collected_at)
    {
        return Ok(Some(exhausted.clone()));
    }
    let freshness_cutoff = now - chrono::TimeDelta::minutes(15);
    Ok(snapshots
        .into_iter()
        .filter(|snapshot| match snapshot.source {
            UsageSource::OfficialCli
            | UsageSource::OfficialProtocol
            | UsageSource::ConfiguredProbe => snapshot.collected_at >= freshness_cutoff,
            UsageSource::LocalLedger | UsageSource::ManualOverride => true,
            UsageSource::Unknown => false,
        })
        .min_by_key(|snapshot| {
            (
                priority(&snapshot.source),
                std::cmp::Reverse(snapshot.collected_at),
            )
        }))
}

fn latest_usage_snapshots(
    database: &Database,
    config: &OrchestratorConfig,
) -> Result<Vec<UsageSnapshot>> {
    let mut snapshots = Vec::new();
    let now = Utc::now();
    for (provider, provider_config) in provider_configs(config) {
        let mut snapshot =
            match preferred_recorded_usage_for_period(database, provider, provider_config, now)? {
                Some(snapshot) => snapshot,
                None => {
                    UsageSnapshot::unknown(provider, quota_scope(provider, provider_config)?, now)
                }
            };
        if snapshot.period_started_at.is_none() || snapshot.resets_at.is_none() {
            let window = period_window(&reset_policy(provider_config)?, now)?;
            snapshot.period_started_at = Some(window.started_at);
            snapshot.resets_at = Some(window.resets_at);
        }
        snapshots.push(snapshot);
    }
    Ok(snapshots)
}

fn usage_defaults(config: &OrchestratorConfig) -> Result<Vec<UsageSnapshot>> {
    let now = Utc::now();
    provider_configs(config)
        .map(|(provider, provider_config)| {
            let mut snapshot =
                UsageSnapshot::unknown(provider, quota_scope(provider, provider_config)?, now);
            let window = period_window(&reset_policy(provider_config)?, now)?;
            snapshot.period_started_at = Some(window.started_at);
            snapshot.resets_at = Some(window.resets_at);
            Ok(snapshot)
        })
        .collect()
}

fn usage_history(database: &Database, provider: ProviderId) -> Result<Vec<UsageSnapshot>> {
    let mut snapshots = database.list_global_usage_snapshots(Some(provider), 256)?;
    snapshots.reverse();
    Ok(snapshots)
}

fn recent_failure_rate(database: &WorkspaceDatabase<'_>, provider: ProviderId) -> Result<f64> {
    Ok(database.recent_attempt_failure_rate(provider)?)
}

fn load_task_input(arguments: &RunArgs) -> Result<TaskInput> {
    if let Some(path) = &arguments.task_file {
        let link_metadata = fs::symlink_metadata(path)
            .with_context(|| format!("task file is unavailable: {}", path.display()))?;
        if link_metadata.file_type().is_symlink() {
            bail!("task file must not be a symbolic link");
        }
        let canonical = fs::canonicalize(path)?;
        let repository = current_repository()?;
        if !canonical.starts_with(&repository) {
            bail!("task file must be located inside the current repository");
        }
        let metadata = fs::metadata(&canonical)?;
        if !metadata.is_file() || metadata.len() > 1024 * 1024 {
            bail!("task file must be a regular file no larger than 1 MiB");
        }
        orchestrator_state::verify_private_file(&canonical)?;
        let input: TaskFile = serde_json::from_slice(&fs::read(canonical)?)?;
        if input.schema_version != "1" {
            bail!(
                "unsupported task file schema_version {}",
                input.schema_version
            );
        }
        if input.objective.trim().is_empty() {
            bail!("task objective is empty");
        }
        return Ok(TaskInput {
            original_request: input
                .original_request
                .unwrap_or_else(|| input.objective.clone()),
            objective: input.objective,
            constraints: input.constraints,
            acceptance_criteria: input.acceptance_criteria,
            allowed_write_paths: input.allowed_write_paths,
            repository_wide_write_scope: input.repository_wide_write_scope,
        });
    }
    let task = arguments
        .task
        .as_ref()
        .ok_or_else(|| anyhow!("task text or --task-file is required"))?;
    if task.trim().is_empty() {
        bail!("task text is empty");
    }
    Ok(TaskInput {
        objective: task.clone(),
        original_request: task.clone(),
        constraints: Vec::new(),
        acceptance_criteria: Vec::new(),
        allowed_write_paths: infer_allowed_write_paths(task),
        repository_wide_write_scope: explicitly_repository_wide(task),
    })
}

fn infer_allowed_write_paths(task: &str) -> Vec<RepoPath> {
    let mut paths = task
        .split('`')
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .filter_map(|(_, candidate)| {
            let candidate = candidate.trim().replace('\\', "/");
            (candidate.contains('/') || Path::new(&candidate).extension().is_some())
                .then(|| RepoPath::try_from(candidate).ok())
                .flatten()
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

fn vendor_neutral_plan(task: &TaskEnvelope) -> Vec<PlanStep> {
    let mut steps = vec![
        PlanStep {
            id: "analyze".to_owned(),
            description: "Inspect repository context and constraints".to_owned(),
            status: PlanStepStatus::InProgress,
        },
        PlanStep {
            id: "implement".to_owned(),
            description: "Implement the approved task scope".to_owned(),
            status: PlanStepStatus::Pending,
        },
        PlanStep {
            id: "verify".to_owned(),
            description: "Run independent verification and acceptance checks".to_owned(),
            status: PlanStepStatus::Pending,
        },
    ];
    steps.extend(
        task.acceptance_criteria
            .iter()
            .enumerate()
            .map(|(index, criterion)| PlanStep {
                id: format!("acceptance-{}", index.saturating_add(1)),
                description: criterion.clone(),
                status: PlanStepStatus::Pending,
            }),
    );
    steps
}

fn explicitly_repository_wide(task: &str) -> bool {
    let task = task.to_lowercase();
    contains_any(
        &task,
        &[
            "repository-wide",
            "entire repository",
            "workspace-wide",
            "저장소 전체",
            "전체 저장소",
        ],
    )
}

fn infer_role(text: &str) -> TaskRole {
    let text = text.to_lowercase();
    if contains_any(&text, &["security review", "보안 검토"]) {
        TaskRole::SecurityReview
    } else if contains_any(&text, &["architecture", "설계", "아키텍처"]) {
        TaskRole::Architecture
    } else if contains_any(&text, &["research", "investigate", "조사", "분석"]) {
        TaskRole::RepositoryResearch
    } else if contains_any(&text, &["debug", "fix", "버그", "수정"]) {
        TaskRole::Debugging
    } else if contains_any(&text, &["test", "테스트"]) {
        TaskRole::Testing
    } else if contains_any(&text, &["refactor", "리팩터"]) {
        TaskRole::Refactoring
    } else {
        TaskRole::Implementation
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn provider_configs(
    config: &OrchestratorConfig,
) -> impl Iterator<Item = (ProviderId, &ProviderConfig)> {
    [
        (ProviderId::Gemini, config.providers.gemini.as_ref()),
        (ProviderId::Agy, config.providers.agy.as_ref()),
        (ProviderId::Codex, config.providers.codex.as_ref()),
        (ProviderId::Claude, config.providers.claude.as_ref()),
    ]
    .into_iter()
    .filter_map(|(provider, config)| config.map(|config| (provider, config)))
}

fn provider_config(config: &OrchestratorConfig, provider: ProviderId) -> Option<&ProviderConfig> {
    match provider {
        ProviderId::Gemini => config.providers.gemini.as_ref(),
        ProviderId::Agy => config.providers.agy.as_ref(),
        ProviderId::Codex => config.providers.codex.as_ref(),
        ProviderId::Claude => config.providers.claude.as_ref(),
    }
}

fn process_redaction(config: &OrchestratorConfig) -> RedactionConfig {
    RedactionConfig {
        // Persisting literal credentials would violate the credential-handling policy.
        // Organization-specific formats are therefore configured as validated regexes.
        literals: Vec::new(),
        patterns: config.redaction.patterns.clone(),
    }
}

fn configured_profiles(config: &OrchestratorConfig, provider: ProviderId) -> Vec<ModelProfile> {
    let Some(profiles) = config.model_profiles.get(provider.as_str()) else {
        return Vec::new();
    };
    [
        ("economy", ModelProfile::Economy),
        ("standard", ModelProfile::Standard),
        ("premium", ModelProfile::Premium),
    ]
    .into_iter()
    .filter_map(|(name, profile)| profiles.contains_key(name).then_some(profile))
    .collect()
}

fn quota_scope(provider: ProviderId, config: &ProviderConfig) -> Result<QuotaScope> {
    Ok(QuotaScope::new(
        config
            .quota_scope
            .clone()
            .unwrap_or_else(|| format!("{}_enterprise_primary", provider.as_str())),
        parse_quota_period(&config.quota_period)?,
        UsageUnit::Custom(config.quota_unit.clone()),
    ))
}

fn parse_quota_period(value: &str) -> Result<QuotaPeriod> {
    match value {
        "calendar_day" => Ok(QuotaPeriod::CalendarDay),
        "rolling_day" => Ok(QuotaPeriod::RollingDay),
        "calendar_month" => Ok(QuotaPeriod::CalendarMonth),
        "rolling_month" => Ok(QuotaPeriod::RollingMonth),
        "custom" => Ok(QuotaPeriod::Custom),
        _ => bail!("unsupported quota period `{value}`"),
    }
}

fn percentage(value: Option<f64>, limit: Option<f64>) -> Option<f64> {
    match (value, limit) {
        (Some(value), Some(limit)) if limit > 0.0 => {
            Some((value / limit * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    }
}

fn enum_name<T: Serialize>(value: &T) -> Result<String> {
    let value = serde_json::to_value(value)?;
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("expected string serialization for enum"))
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn current_repository() -> Result<PathBuf> {
    let current =
        std::env::current_dir().context("cannot determine current repository directory")?;
    fs::canonicalize(&current).with_context(|| {
        format!(
            "cannot canonicalize repository directory: {}",
            current.display()
        )
    })
}

fn resolve_from(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn open_ready_database(state: &StatePaths) -> Result<Database> {
    if !state.database.exists() {
        bail!(
            "state database does not exist at {}; run `colay init`",
            state.database.display()
        );
    }
    let database = Database::open(&state.database)?;
    let status = database.migration_status()?;
    if !status.pending_versions.is_empty() {
        bail!(
            "state schema migration is required ({:?}); run `colay migrate apply`",
            status.pending_versions
        );
    }
    Ok(database)
}

fn workspace_for_repository<'a>(
    database: &'a Database,
    repository: &Path,
) -> Result<WorkspaceDatabase<'a>> {
    let registration = database.resolve_repository_workspace(repository)?;
    Ok(database.workspace(registration.workspace_id))
}

fn legacy_migration_workspace_if_present(
    database: &Database,
) -> Result<Option<WorkspaceDatabase<'_>>> {
    match database.legacy_workspace() {
        Ok(workspace) => Ok(Some(workspace)),
        Err(StateError::WorkspaceNotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn reconcile_events(state: &StatePaths, database: &WorkspaceDatabase<'_>) -> Result<()> {
    EventLog::open(&state.events)?.reconcile_workspace(database)?;
    Ok(())
}

fn latest_database_backup(directory: &Path) -> Result<PathBuf> {
    let mut backups = fs::read_dir(directory)
        .with_context(|| format!("backup directory is missing: {}", directory.display()))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with("orchestrator.db.backup.")
            })
        })
        .collect::<Vec<_>>();
    backups.sort();
    backups
        .pop()
        .ok_or_else(|| anyhow!("no database backup is available"))
}

fn emit<T: Serialize>(json_output: bool, command: &str, data: &T) -> Result<()> {
    emit_versioned(json_output, "1", command, data)
}

fn emit_versioned<T: Serialize>(
    json_output: bool,
    schema_version: &str,
    command: &str,
    data: &T,
) -> Result<()> {
    let envelope = json!({
        "schema_version": schema_version,
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

#[derive(Clone, Debug, Deserialize)]
struct TaskFile {
    schema_version: String,
    objective: String,
    #[serde(default)]
    original_request: Option<String>,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    allowed_write_paths: Vec<RepoPath>,
    #[serde(default)]
    repository_wide_write_scope: bool,
}

#[derive(Clone, Debug)]
struct TaskInput {
    objective: String,
    original_request: String,
    constraints: Vec<String>,
    acceptance_criteria: Vec<String>,
    allowed_write_paths: Vec<RepoPath>,
    repository_wide_write_scope: bool,
}

#[derive(Clone, Debug, Serialize)]
struct PlannedTask {
    task: TaskEnvelope,
    routing: RoutingDecision,
    plan_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Clone, Debug, Serialize)]
struct Check {
    name: String,
    status: CheckStatus,
    detail: Option<String>,
    data: Option<Value>,
}

impl Check {
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Pass,
            detail: Some(detail.into()),
            data: None,
        }
    }

    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Warn,
            detail: Some(detail.into()),
            data: None,
        }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail,
            detail: Some(detail.into()),
            data: None,
        }
    }

    fn with_data(name: impl Into<String>, passed: bool, data: Value) -> Self {
        Self {
            name: name.into(),
            status: if passed {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            detail: None,
            data: Some(data),
        }
    }

    fn with_status_data(
        name: impl Into<String>,
        status: CheckStatus,
        detail: impl Into<String>,
        data: Value,
    ) -> Self {
        Self {
            name: name.into(),
            status,
            detail: Some(detail.into()),
            data: Some(data),
        }
    }
}

fn mixed_git_checkout_warning(repository: &Path, target_os: &str) -> Option<String> {
    if target_os != "linux" {
        return None;
    }
    let path = repository.to_string_lossy().replace('\\', "/");
    let remainder = path.strip_prefix("/mnt/")?;
    let mut components = remainder.split('/');
    let drive = components.next()?;
    if drive.len() != 1 || !drive.as_bytes()[0].is_ascii_alphabetic() || components.next().is_none()
    {
        return None;
    }
    Some(
        "repository is on a Windows-mounted /mnt/<drive> path; Windows Git and WSL Git may report mass line-ending changes. Use a WSL-native clone under the Linux filesystem for Linux Colay, or use Windows Colay with the Windows checkout."
            .to_owned(),
    )
}

#[derive(Clone, Debug, Serialize)]
struct DoctorReport {
    schema_version: &'static str,
    passed: bool,
    checks: Vec<Check>,
    inference_requests: u32,
}

#[derive(Clone, Debug, Serialize)]
struct DoctorProvidersReport {
    schema_version: &'static str,
    providers: Vec<ProviderReport>,
    inference_requests: u32,
}

#[derive(Clone, Debug, Serialize)]
struct ProviderReport {
    provider: ProviderId,
    enabled: bool,
    health: ProviderHealth,
    capabilities: ProviderCapabilities,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackManifest {
    schema_version: u32,
    version: String,
    steps: Vec<RollbackManifestStep>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackManifestStep {
    component: String,
    backup_source: PathBuf,
    destination: PathBuf,
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use crate::args::{EffortName, ProfileName, ProviderName};
    use anyhow::{Context as _, Result};
    use chrono::Utc;
    use orchestrator_domain::{
        AttemptId, ModelProfile, ProviderId, ReasoningEffort, SandboxMode, SchemaVersion,
        TaskEnvelope, TaskEvent, TaskId, TaskState, TestEvidence, TestStatus, VerificationStatus,
        WorkerOutcome, WorkerRequest, WorkerResult,
    };
    use orchestrator_process::{EnvironmentPolicy, RedactionConfig, Redactor, resolve_executable};
    use orchestrator_state::{
        ConfigEnvironment, DaemonStatus, Database, DatabaseHealth, GlobalStatePaths, NewTaskRecord,
        RootConfig, WorkspaceDatabase, WorkspaceId, WorkspaceKind, WorkspaceRegistration,
        WorkspaceStatus,
    };
    use rusqlite::params;
    use toml_edit::DocumentMut;

    use super::{
        CheckStatus, ReviewOutcome, RollbackManifestStep, StatePaths, acceptance_evidence,
        acquire_task_coordinator, acquire_worker_lease, append_live_doctor_checks,
        block_for_unconfirmed_termination, initialize, load_config_runtime,
        mixed_git_checkout_warning, next_run_command_poll_interval, provider_adapter,
        reset_model_profile, rollback_resolution_context, run_with_coordinator_renewal, run_worker,
        set_model_profile, set_provider_enabled, trusted_rollback_steps, worker_started_payload,
    };

    #[test]
    fn run_command_polling_backs_off_and_caps_at_one_second() {
        let mut interval = Duration::from_millis(25);
        let expected = [50_u64, 100, 200, 400, 800, 1_000, 1_000];

        for expected_millis in expected {
            interval = next_run_command_poll_interval(interval);
            assert_eq!(interval, Duration::from_millis(expected_millis));
        }
    }

    fn test_with_database<T>(
        database: &Database,
        operation: impl FnOnce(&rusqlite::Connection) -> orchestrator_state::StateResult<T>,
    ) -> orchestrator_state::StateResult<T> {
        let connection = open_test_connection(database.path())?;
        operation(&connection)
    }

    fn test_with_workspace<T>(
        database_path: &Path,
        database: &WorkspaceDatabase<'_>,
        operation: impl FnOnce(&rusqlite::Connection) -> orchestrator_state::StateResult<T>,
    ) -> orchestrator_state::StateResult<T> {
        use rusqlite::functions::FunctionFlags;
        let connection = open_test_connection(database_path)?;
        let workspace_id = database.workspace_id().to_string();
        connection.create_scalar_function(
            "current_workspace",
            0,
            FunctionFlags::SQLITE_DETERMINISTIC,
            move |_| Ok(workspace_id.clone()),
        )?;
        operation(&connection)
    }

    fn open_test_connection(path: &Path) -> orchestrator_state::StateResult<rusqlite::Connection> {
        let connection = rusqlite::Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;\
             PRAGMA journal_mode = WAL;\
             PRAGMA synchronous = FULL;\
             PRAGMA temp_store = MEMORY;\
             PRAGMA busy_timeout = 5000;",
        )?;
        Ok(connection)
    }

    fn test_state(root: PathBuf) -> StatePaths {
        StatePaths {
            database: root.join("orchestrator.db"),
            events: root.join("events.jsonl"),
            backups: root.join("backups"),
            tasks: root.join("tasks"),
            checkpoints: root.join("checkpoints"),
            handovers: root.join("handovers"),
            worktrees: root.join("worktrees"),
            root,
        }
    }

    #[test]
    fn linux_mounted_windows_checkout_warns_about_mixed_git_line_endings() {
        let warning = mixed_git_checkout_warning(Path::new("/mnt/c/work/project"), "linux")
            .unwrap_or_default();
        assert!(warning.contains("WSL-native clone"));
        assert!(warning.contains("line-ending"));
        assert!(mixed_git_checkout_warning(Path::new("/home/user/project"), "linux").is_none());
        assert!(mixed_git_checkout_warning(Path::new("C:/work/project"), "windows").is_none());
    }

    #[test]
    fn malformed_live_doctor_response_fails_every_integrity_category() -> Result<()> {
        let root = PathBuf::from("doctor-test-root");
        let paths = GlobalStatePaths {
            database: root.join("state/state.db"),
            backups: root.join("state/backups"),
            workspaces: root.join("data/workspaces"),
            runtime: root.join("runtime"),
            config: root.join("config.toml"),
            root,
        };
        let response = orchestrator_daemon::IpcResponse {
            schema_version: orchestrator_daemon::IPC_SCHEMA_VERSION,
            request_id: "doctor-test".to_owned(),
            outcome: serde_json::json!({
                "status": "ok",
                "data": {
                    "workspace": null
                }
            }),
        };
        let mut checks = Vec::new();
        let workspace_id = "00000000-0000-0000-0000-000000000002".parse::<WorkspaceId>()?;

        append_live_doctor_checks(
            &mut checks,
            &response,
            &paths,
            workspace_id,
            Path::new("doctor-test-repository"),
            Path::new("colay"),
        );

        assert_eq!(checks.len(), 5);
        for name in ["state", "daemon", "workspace", "audit", "artifacts"] {
            let check = checks
                .iter()
                .find(|check| check.name == name)
                .with_context(|| format!("missing {name} failure"))?;
            assert_eq!(check.status, CheckStatus::Fail, "{name} must fail closed");
        }
        Ok(())
    }

    #[test]
    fn mismatched_live_doctor_workspace_path_fails_every_integrity_category() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = fs::canonicalize(temporary.path())?;
        let expected_repository = root.join("expected-repository");
        let wrong_repository = root.join("wrong-repository");
        fs::create_dir_all(&expected_repository)?;
        fs::create_dir_all(&wrong_repository)?;
        let expected_repository = fs::canonicalize(expected_repository)?;
        let wrong_repository = fs::canonicalize(wrong_repository)?;
        let workspace_id = "00000000-0000-0000-0000-000000000003".parse::<WorkspaceId>()?;
        let paths = GlobalStatePaths {
            database: root.join("state/state.db"),
            backups: root.join("state/backups"),
            workspaces: root.join("data/workspaces"),
            runtime: root.join("runtime"),
            config: root.join("config.toml"),
            root,
        };
        let now = Utc::now();
        let diagnostics = orchestrator_daemon::WorkspaceDoctorDiagnostics {
            schema_version: orchestrator_daemon::WORKSPACE_DOCTOR_SCHEMA_VERSION,
            database: DatabaseHealth {
                integrity_ok: true,
                foreign_key_violations: 0,
                current_schema_version: orchestrator_state::STATE_SCHEMA_VERSION,
                last_event_sequence: 0,
            },
            daemon: DaemonStatus::Stopped,
            workspace: WorkspaceRegistration {
                workspace_id,
                kind: WorkspaceKind::Directory,
                status: WorkspaceStatus::Active,
                canonical_path: wrong_repository,
                git_common_dir: None,
                created_at: now,
                last_seen_at: now,
            },
            audit: orchestrator_daemon::WorkspaceAuditDiagnostics {
                workspace_id,
                verified_events: 0,
                last_sequence: 0,
                last_hash: None,
            },
            artifacts: orchestrator_daemon::WorkspaceArtifactDiagnostics {
                root: paths.for_workspace(workspace_id).root,
                verified_references: 0,
                scope: orchestrator_daemon::WorkspaceArtifactScope::PersistedReferences,
            },
        };
        let response = orchestrator_daemon::IpcResponse {
            schema_version: orchestrator_daemon::IPC_SCHEMA_VERSION,
            request_id: "doctor-test".to_owned(),
            outcome: serde_json::json!({
                "status": "ok",
                "data": serde_json::to_value(diagnostics)?,
            }),
        };
        let mut checks = Vec::new();

        append_live_doctor_checks(
            &mut checks,
            &response,
            &paths,
            workspace_id,
            &expected_repository,
            Path::new("colay"),
        );

        assert_eq!(checks.len(), 5);
        for name in ["state", "daemon", "workspace", "audit", "artifacts"] {
            let check = checks
                .iter()
                .find(|check| check.name == name)
                .with_context(|| format!("missing {name} failure"))?;
            assert_eq!(check.status, CheckStatus::Fail, "{name} must fail closed");
        }
        Ok(())
    }

    fn write_fake_executable(path: &Path, bytes: &[u8]) -> Result<()> {
        fs::write(path, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    fn canonical_tempdir() -> Result<(tempfile::TempDir, PathBuf)> {
        let temporary = tempfile::tempdir()?;
        let root = fs::canonicalize(temporary.path())?;
        Ok((temporary, root))
    }

    fn install_legacy_workspace(database: &Database) -> Result<()> {
        test_with_database(database, |connection| {
            connection.execute(
                "INSERT INTO main.workspaces(workspace_id, kind, status, created_at, last_seen_at) \
                 VALUES ('00000000-0000-0000-0000-000000000001', 'directory', 'detached', ?1, ?1)",
                [Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    fn attempt_completion(
        database_path: &Path,
        database: &WorkspaceDatabase<'_>,
        attempt_id: AttemptId,
    ) -> Result<(Option<String>, Option<String>)> {
        Ok(test_with_workspace(
            database_path,
            database,
            |connection| {
                Ok(connection.query_row(
                    "SELECT ended_at, outcome FROM task_attempts WHERE attempt_id = ?1",
                    [attempt_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            },
        )?)
    }

    #[test]
    fn cli_config_is_the_highest_layer() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let global = root.join("home/config.toml");
        let environment = root.join("environment.toml");
        let cli = root.join("cli.toml");
        write_layer(&global, 2)?;
        write_layer(&root.join(".colay/config.toml"), 3)?;
        write_layer(&environment, 4)?;
        write_layer(&cli, 5)?;

        let runtime = load_config_runtime(
            &root,
            Some(&cli),
            ConfigEnvironment {
                colay_home: Some(root.join("home")),
                user_home: None,
                colay_config: Some(environment),
            },
        )?;
        assert_eq!(
            runtime.effective.config().orchestrator.max_parallel_workers,
            5
        );
        Ok(())
    }

    fn write_layer(path: &Path, workers: u32) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            format!("config_version = 4\n[orchestrator]\nmax_parallel_workers = {workers}\n"),
        )?;
        Ok(())
    }

    #[test]
    fn init_writes_only_a_minimal_override() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let runtime = load_config_runtime(&root, None, ConfigEnvironment::isolated())?;

        initialize(&root, &runtime, true)?;

        let persisted = fs::read_to_string(root.join(".colay/config.toml"))?;
        assert!(!persisted.contains("quota_limit"));
        assert!(!persisted.contains("warning_threshold_percent"));
        assert!(!persisted.to_ascii_lowercase().contains("credential"));
        assert!(!persisted.to_ascii_lowercase().contains("api_key"));
        assert!(!persisted.to_ascii_lowercase().contains("token"));
        Ok(())
    }

    #[tokio::test]
    async fn profile_set_persists_one_override_and_reloads_effective_config() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let environment = ConfigEnvironment::isolated();
        let runtime = load_config_runtime(&root, None, environment.clone())?;

        set_model_profile(
            &root,
            None,
            environment,
            &runtime,
            ProviderName::Claude,
            ProfileName::Premium,
            "company-fable",
            Some(EffortName::High),
            true,
        )
        .await?;

        let reloaded = load_config_runtime(&root, None, ConfigEnvironment::isolated())?;
        let profiles = &reloaded.effective.config().orchestrator.model_profiles;
        assert_eq!(profiles["claude"]["premium"].model, "company-fable");
        assert_eq!(profiles["codex"]["standard"].model, "gpt-5.6-terra");
        Ok(())
    }

    #[tokio::test]
    async fn profile_reset_reveals_the_compiled_preset() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let environment = ConfigEnvironment::isolated();
        let runtime = load_config_runtime(&root, None, environment.clone())?;
        set_model_profile(
            &root,
            None,
            environment.clone(),
            &runtime,
            ProviderName::Gemini,
            ProfileName::Standard,
            "company-gemini",
            None,
            true,
        )
        .await?;
        let runtime = load_config_runtime(&root, None, environment.clone())?;
        reset_model_profile(
            &root,
            None,
            environment,
            &runtime,
            ProviderName::Gemini,
            ProfileName::Standard,
            true,
        )
        .await?;

        let reloaded = load_config_runtime(&root, None, ConfigEnvironment::isolated())?;
        assert_eq!(
            reloaded.effective.config().orchestrator.model_profiles["gemini"]["standard"].model,
            "gemini-3.5-flash"
        );
        Ok(())
    }

    #[test]
    fn worker_started_audit_records_effective_model_profile_and_effort() -> Result<()> {
        let request = WorkerRequest {
            schema_version: SchemaVersion::v1(),
            task_id: TaskId::new(),
            attempt_id: AttemptId::new(),
            provider: ProviderId::Claude,
            objective: "audit selection".to_owned(),
            prompt: "do work".to_owned(),
            constraints: Vec::new(),
            acceptance_criteria: Vec::new(),
            workspace_root: std::env::current_dir()?,
            sandbox: SandboxMode::WorkspaceWrite,
            profile: ModelProfile::Premium,
            model: Some("claude-fable-5".to_owned()),
            reasoning_effort: Some(ReasoningEffort::High),
            timeout_seconds: 60,
            max_output_bytes: 1024,
            resume_session_id: None,
            handover_payload: None,
        };
        let payload = worker_started_payload(&request);
        assert_eq!(payload["model"], "claude-fable-5");
        assert_eq!(payload["profile"], "premium");
        assert_eq!(payload["reasoning_effort"], "high");
        Ok(())
    }

    #[cfg(feature = "test-fixtures")]
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn terminal_provider_error_requests_cancel_and_finalizes_attempt() -> Result<()> {
        use orchestrator_test_support::{FakeAdapterRuntime, FakeRuntimeScenario};

        let (_temporary, root) = canonical_tempdir()?;
        let state = test_state(root.join(".colay"));
        let database = Database::open(&state.database)?;
        database.migrate_with_backup(&state.backups)?;
        install_legacy_workspace(&database)?;
        let database = database.legacy_workspace()?;
        let now = Utc::now();
        let envelope = TaskEnvelope::new("terminal provider error", "terminal provider error", now);
        database.create_task(&NewTaskRecord {
            task_id: envelope.task_id,
            schema_version: envelope.schema_version.to_string(),
            state: TaskState::Running,
            objective: envelope.objective.clone(),
            original_request_redacted: envelope.original_request_redacted.clone(),
            envelope: &envelope,
            created_at: now,
        })?;
        let coordinator = acquire_task_coordinator(&database, envelope.task_id)?;
        let worker = acquire_worker_lease(
            &database,
            coordinator.lease_id,
            envelope.task_id,
            ProviderId::Claude,
            SandboxMode::WorkspaceWrite,
        )?;

        let fake_executable = root.join(if cfg!(windows) {
            "fake-provider-cli.exe"
        } else {
            "fake-provider-cli"
        });
        fs::copy(std::env::current_exe()?, &fake_executable)?;
        let mut config = RootConfig::default();
        config
            .orchestrator
            .providers
            .claude
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("default Claude provider is missing"))?
            .executable = fake_executable.to_string_lossy().into_owned();
        let runtime =
            FakeAdapterRuntime::new(&fake_executable, FakeRuntimeScenario::TerminalError)?;
        let adapter = provider_adapter(ProviderId::Claude, &config, Arc::new(runtime), &root)?;
        let request = WorkerRequest {
            schema_version: SchemaVersion::v1(),
            task_id: envelope.task_id,
            attempt_id: AttemptId::new(),
            provider: ProviderId::Claude,
            objective: envelope.objective,
            prompt: "return a terminal credit error".to_owned(),
            constraints: Vec::new(),
            acceptance_criteria: Vec::new(),
            workspace_root: root.clone(),
            sandbox: SandboxMode::WorkspaceWrite,
            profile: ModelProfile::Standard,
            model: None,
            reasoning_effort: None,
            timeout_seconds: 60,
            max_output_bytes: 1024,
            resume_session_id: None,
            handover_payload: None,
        };
        let provider_config = config
            .orchestrator
            .providers
            .claude
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("default Claude provider is missing"))?;

        let run = run_worker(
            adapter.as_ref(),
            request.clone(),
            provider_config,
            -1.0,
            false,
            false,
            &RedactionConfig::default(),
            &state,
            &database,
            coordinator.lease_id,
            &worker,
            orchestrator_domain::CorrelationId::new(),
        )
        .await?;

        assert_eq!(run.result.outcome, WorkerOutcome::Cancelled);
        assert_eq!(run.lifecycle_error.as_deref(), Some("claude_result"));
        let renewed_worker = database
            .active_worker_leases(envelope.task_id, Utc::now())?
            .into_iter()
            .find(|lease| lease.lease_id == worker.lease_id)
            .ok_or_else(|| anyhow::anyhow!("worker lease disappeared before release"))?;
        assert!(renewed_worker.expires_at > worker.expires_at);
        let (ended_at, outcome) =
            attempt_completion(&state.database, &database, request.attempt_id)?;
        assert!(ended_at.is_some());
        assert_eq!(outcome.as_deref(), Some("cancelled"));
        database.release_worker_lease(coordinator.lease_id, worker.lease_id, Utc::now())?;
        database.release_coordinator_lease(
            coordinator.lease_id,
            coordinator.owner_id,
            Utc::now(),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn direct_execution_leases_have_bounded_recovery_ttls() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let state = test_state(root.join(".colay"));
        let database = Database::open(&state.database)?;
        database.migrate_with_backup(&state.backups)?;
        install_legacy_workspace(&database)?;
        let database = database.legacy_workspace()?;
        let now = Utc::now();
        let envelope = TaskEnvelope::new("bounded leases", "bounded leases", now);
        database.create_task(&NewTaskRecord {
            task_id: envelope.task_id,
            schema_version: envelope.schema_version.to_string(),
            state: TaskState::Planned,
            objective: envelope.objective.clone(),
            original_request_redacted: envelope.original_request_redacted.clone(),
            envelope: &envelope,
            created_at: now,
        })?;
        let coordinator = acquire_task_coordinator(&database, envelope.task_id)?;
        let worker = acquire_worker_lease(
            &database,
            coordinator.lease_id,
            envelope.task_id,
            ProviderId::Claude,
            SandboxMode::WorkspaceWrite,
        )?;

        assert!(coordinator.expires_at - coordinator.acquired_at <= chrono::TimeDelta::seconds(30));
        assert!(worker.expires_at - worker.acquired_at <= chrono::TimeDelta::seconds(20));
        let Err(conflict) = acquire_task_coordinator(&database, envelope.task_id) else {
            anyhow::bail!("a second coordinator unexpectedly acquired the live task");
        };
        let diagnostic = conflict.to_string();
        assert!(diagnostic.contains("renewed_at="));
        assert!(diagnostic.contains("expires_at="));
        assert!(diagnostic.contains("active_workers=1"));
        assert!(diagnostic.contains("safe retry after"));
        database.release_worker_lease(coordinator.lease_id, worker.lease_id, Utc::now())?;
        run_with_coordinator_renewal(
            &database,
            &coordinator,
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                Ok(())
            }),
        )
        .await?;
        let renewed = database
            .active_coordinator_lease(envelope.task_id, Utc::now())?
            .ok_or_else(|| anyhow::anyhow!("coordinator lease expired while owner was active"))?;
        assert!(renewed.renewed_at > renewed.acquired_at);
        database.release_coordinator_lease(
            coordinator.lease_id,
            coordinator.owner_id,
            Utc::now(),
        )?;
        Ok(())
    }

    #[test]
    fn shipped_docs_describe_profile_management_and_current_presets() {
        let readme = include_str!("../../../README.md");
        let example = include_str!("../../../config.example.toml");
        for required in [
            "colay profiles",
            "colay profiles set",
            "colay profiles reset",
            "claude-fable-5",
            "gemini-3.5-flash",
            "gemini-3.5-flash-low",
            "gemini-3.5-flash-medium",
            "gemini-3.1-pro-high",
            "gpt-5.6-sol",
            "f:profiles",
        ] {
            assert!(readme.contains(required), "README is missing {required}");
        }
        assert!(example.contains("orchestrator.model_profiles.claude.premium"));
        assert!(example.contains("claude-fable-5"));
        assert!(example.contains("orchestrator.providers.agy"));
        assert!(example.contains("orchestrator.model_profiles.agy.premium"));
    }

    #[tokio::test]
    async fn provider_enable_adds_only_the_requested_agy_boolean() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let environment = ConfigEnvironment::isolated();
        let runtime = load_config_runtime(&root, None, environment.clone())?;

        set_provider_enabled(
            &root,
            None,
            environment,
            &runtime,
            ProviderId::Agy,
            false,
            true,
        )
        .await?;

        let persisted = fs::read_to_string(&runtime.explicit_edit_path)?.parse::<DocumentMut>()?;
        let providers = persisted["orchestrator"]["providers"]
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("providers override is not a table"))?;
        let provider = providers["agy"]
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("agy override is not a table"))?;
        assert_eq!(provider.len(), 1);
        assert_eq!(provider["enabled"].as_bool(), Some(false));
        assert_eq!(providers.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn repository_provider_edit_preserves_global_comments() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let global = root.join("home/config.toml");
        fs::create_dir_all(
            global
                .parent()
                .ok_or_else(|| anyhow::anyhow!("global config has no parent"))?,
        )?;
        let original = "# retain this administrator comment\nconfig_version = 4\n[orchestrator]\nmax_parallel_workers = 7\n";
        fs::write(&global, original)?;
        let environment = ConfigEnvironment {
            colay_home: Some(root.join("home")),
            user_home: None,
            colay_config: None,
        };
        let runtime = load_config_runtime(&root, None, environment.clone())?;

        set_provider_enabled(
            &root,
            None,
            environment,
            &runtime,
            ProviderId::Claude,
            false,
            true,
        )
        .await?;

        assert_eq!(fs::read_to_string(global)?, original);
        assert!(runtime.explicit_edit_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn provider_edit_targets_the_environment_override() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let environment_path = root.join("environment.toml");
        fs::write(
            &environment_path,
            "# environment comment\nconfig_version = 4\n",
        )?;
        let environment = ConfigEnvironment {
            colay_home: None,
            user_home: None,
            colay_config: Some(environment_path.clone()),
        };
        let runtime = load_config_runtime(&root, None, environment.clone())?;

        set_provider_enabled(
            &root,
            None,
            environment,
            &runtime,
            ProviderId::Gemini,
            false,
            true,
        )
        .await?;

        let persisted = fs::read_to_string(environment_path)?;
        assert!(persisted.starts_with("# environment comment\n"));
        assert!(persisted.contains("enabled = false"));
        assert!(!root.join(".colay/config.toml").exists());
        Ok(())
    }

    #[test]
    fn legacy_full_config_can_enter_cli_migration() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let path = root.join("legacy-v3.toml");
        let current = toml_edit::ser::to_string(&RootConfig::default())?;
        fs::write(
            &path,
            current.replacen("config_version = 4", "config_version = 3", 1),
        )?;
        assert!(load_config_runtime(&root, Some(&path), ConfigEnvironment::isolated()).is_err());

        let (migratable, source_is_current, preview) =
            super::migration_config_preview(None, &path)?;
        assert!(migratable.is_some());
        assert!(!source_is_current);
        assert_eq!(preview.result().initial_version, 3);
        assert_eq!(preview.result().final_version, 4);
        Ok(())
    }

    #[tokio::test]
    async fn automatic_legacy_provider_edit_updates_only_legacy_source() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let legacy = root.join(".codex/orchestrator/config.toml");
        write_layer(&legacy, 3)?;
        let environment = ConfigEnvironment::isolated();
        let runtime = load_config_runtime(&root, None, environment.clone())?;

        set_provider_enabled(
            &root,
            None,
            environment,
            &runtime,
            ProviderId::Codex,
            false,
            true,
        )
        .await?;

        assert!(fs::read_to_string(&legacy)?.contains("enabled = false"));
        assert!(!root.join(".colay/config.toml").exists());
        Ok(())
    }

    #[test]
    fn automatic_legacy_migration_targets_legacy_source() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let legacy = root.join(".codex/orchestrator/config.toml");
        write_full_version(&legacy, 3)?;

        assert_eq!(
            super::migration_fallback_path(&root, None, &ConfigEnvironment::isolated())?,
            Some(legacy)
        );
        Ok(())
    }

    #[test]
    fn migration_does_not_bypass_current_legacy_conflict() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        write_full_version(&root.join(".colay/config.toml"), 3)?;
        write_full_version(&root.join(".codex/orchestrator/config.toml"), 3)?;

        assert!(
            super::migration_fallback_path(&root, None, &ConfigEnvironment::isolated())?.is_none()
        );
        Ok(())
    }

    #[test]
    fn explicit_repository_config_resolves_migration_conflict() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let current = root.join(".colay/config.toml");
        let legacy = root.join(".codex/orchestrator/config.toml");
        write_full_version(&current, 3)?;
        write_full_version(&legacy, 3)?;

        assert_eq!(
            super::migration_fallback_path(&root, Some(&current), &ConfigEnvironment::isolated())?,
            Some(current)
        );
        Ok(())
    }

    #[test]
    fn migration_does_not_bypass_invalid_global_layer() -> Result<()> {
        let (_temporary, root) = canonical_tempdir()?;
        let global = root.join("home/config.toml");
        fs::create_dir_all(
            global
                .parent()
                .ok_or_else(|| anyhow::anyhow!("no global parent"))?,
        )?;
        fs::write(&global, "not valid toml = [")?;
        write_full_version(&root.join(".colay/config.toml"), 3)?;
        let environment = ConfigEnvironment {
            colay_home: Some(root.join("home")),
            user_home: None,
            colay_config: None,
        };

        let Err(error) = super::migration_fallback_path(&root, None, &environment) else {
            anyhow::bail!("invalid global layer did not fail closed");
        };
        assert!(error.to_string().contains("global config layer"));
        Ok(())
    }

    fn write_full_version(path: &Path, version: u32) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let current = toml_edit::ser::to_string(&RootConfig::default())?;
        fs::write(
            path,
            current.replacen(
                "config_version = 4",
                &format!("config_version = {version}"),
                1,
            ),
        )?;
        Ok(())
    }

    fn passed_test(name: &str) -> TestEvidence {
        TestEvidence {
            name: name.to_owned(),
            status: TestStatus::Passed,
            command_id: None,
            detail: None,
        }
    }

    #[test]
    fn generic_commands_do_not_prove_arbitrary_acceptance_criteria() {
        let review = ReviewOutcome {
            provider: None,
            passed: true,
            acceptance_criteria_met: false,
            findings: Vec::new(),
        };
        let evidence = acceptance_evidence(
            "The user workflow is intuitive",
            &[passed_test("cargo test")],
            false,
            &review,
            true,
            true,
        );
        assert_eq!(evidence.status, VerificationStatus::Inconclusive);
        assert!(evidence.evidence.is_empty());
    }

    #[test]
    fn matching_local_oracle_can_prove_a_test_criterion() {
        let review = ReviewOutcome {
            provider: None,
            passed: true,
            acceptance_criteria_met: false,
            findings: Vec::new(),
        };
        let evidence = acceptance_evidence(
            "All tests pass",
            &[passed_test("cargo test")],
            false,
            &review,
            true,
            true,
        );
        assert_eq!(evidence.status, VerificationStatus::Pass);
        assert_eq!(evidence.evidence, vec!["cargo test: Passed"]);
    }

    #[test]
    fn required_review_cannot_be_masked_by_passing_commands() {
        let review = ReviewOutcome {
            provider: Some(ProviderId::Claude),
            passed: false,
            acceptance_criteria_met: false,
            findings: Vec::new(),
        };
        let evidence = acceptance_evidence(
            "All tests pass",
            &[passed_test("cargo test")],
            true,
            &review,
            true,
            true,
        );
        assert_eq!(evidence.status, VerificationStatus::Inconclusive);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn rollback_relative_codex_target_matches_persisted_writable_worker_worktree() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let repository = fs::canonicalize(temporary.path())?;
        let state = test_state(repository.join(".colay"));
        let worktree = state.worktrees.join("writable-worker");
        let configured = PathBuf::from("tools/fake-provider.exe");
        let repository_executable = repository.join(&configured);
        let worktree_executable = worktree.join(&configured);
        for (path, bytes) in [
            (&repository_executable, b"repository fake".as_slice()),
            (&worktree_executable, b"worktree fake".as_slice()),
        ] {
            fs::create_dir_all(
                path.parent()
                    .ok_or_else(|| anyhow::anyhow!("fake executable has no parent"))?,
            )?;
            write_fake_executable(path, bytes)?;
        }

        let database = Database::open(&state.database)?;
        database.migrate_with_backup(&state.backups)?;
        install_legacy_workspace(&database)?;
        let database = database.legacy_workspace()?;
        let now = Utc::now();
        let envelope = TaskEnvelope::new("writable fake provider", "writable fake provider", now);
        database.create_task(&NewTaskRecord {
            task_id: envelope.task_id,
            schema_version: envelope.schema_version.to_string(),
            state: TaskState::Completed,
            objective: envelope.objective.clone(),
            original_request_redacted: envelope.original_request_redacted.clone(),
            envelope: &envelope,
            created_at: now,
        })?;
        let attempt_id = AttemptId::new();
        let worker_result = WorkerResult {
            schema_version: SchemaVersion::v1(),
            task_id: envelope.task_id,
            attempt_id,
            provider: ProviderId::Codex,
            outcome: WorkerOutcome::Succeeded,
            exit_code: Some(0),
            session_id: None,
            summary: None,
            commands: Vec::new(),
            tests: Vec::new(),
            started_at: now,
            finished_at: now,
            output_truncated: false,
        };
        let mut persisted = serde_json::to_value(&worker_result)?;
        persisted["process_execution"] = serde_json::json!({
            "configured": configured,
            "path": fs::canonicalize(&worktree_executable)?,
            "kind": "native",
            "validation": {
                "working_directory": fs::canonicalize(&worktree)?,
                "search_directory": null
            }
        });
        test_with_workspace(&state.database, &database, |connection| {
            connection.execute(
                "INSERT INTO main.worktrees(
                    workspace_id, worktree_id, task_id, repo_root, worktree_path, branch_name,
                    base_revision, state, created_at
                 ) VALUES (current_workspace(), ?1, ?2, ?3, ?4, 'codex/test', 'base', 'active', ?5)",
                params![
                    uuid::Uuid::now_v7().to_string(),
                    envelope.task_id.to_string(),
                    repository.to_string_lossy(),
                    worktree.to_string_lossy(),
                    now.to_rfc3339(),
                ],
            )?;
            connection.execute(
                "INSERT INTO main.task_attempts(
                    workspace_id, attempt_id, task_id, ordinal, provider_id, worker_mode, started_at,
                    ended_at, outcome, worker_result_json
                 ) VALUES (current_workspace(), ?1, ?2, 1, 'codex', 'workspace_write', ?3, ?3, 'succeeded', ?4)",
                params![
                    attempt_id.to_string(),
                    envelope.task_id.to_string(),
                    now.to_rfc3339(),
                    persisted.to_string(),
                ],
            )?;
            Ok(())
        })?;

        let mut config = RootConfig::default();
        config
            .orchestrator
            .providers
            .codex
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("default Codex provider is missing"))?
            .executable = "tools/changed-after-attempt.exe".to_owned();
        let changed_executable = worktree.join("tools/changed-after-attempt.exe");
        write_fake_executable(&changed_executable, b"changed fake")?;
        let release_root = state.backups.join("releases/fake-v1");
        fs::create_dir_all(&release_root)?;
        fs::write(release_root.join("fake-provider.backup"), b"rollback fake")?;
        let steps = vec![RollbackManifestStep {
            component: "codex".to_owned(),
            backup_source: PathBuf::from("fake-provider.backup"),
            destination: configured.clone(),
        }];
        let context = rollback_resolution_context(&repository, &state, &config, &steps)?;
        let planned = trusted_rollback_steps(
            &repository,
            &repository.join(".colay/config.toml"),
            &state,
            &config,
            &context,
            &release_root,
            steps,
        )?;
        let trusted = planned
            .first()
            .ok_or_else(|| anyhow::anyhow!("rollback planning omitted Codex step"))?
            .destination
            .clone();

        let actual_worker = resolve_executable(
            &configured,
            &EnvironmentPolicy::default().executable_search(&worktree),
        )?;
        assert_eq!(trusted, fs::canonicalize(actual_worker.path)?);
        assert_ne!(trusted, fs::canonicalize(&repository_executable)?);

        let path_a = worktree.join("path-a");
        let path_b = worktree.join("path-b");
        fs::create_dir_all(&path_a)?;
        fs::create_dir_all(&path_b)?;
        let bare = PathBuf::from("fake-path-provider");
        #[cfg(windows)]
        let filename = "fake-path-provider.exe";
        #[cfg(not(windows))]
        let filename = "fake-path-provider";
        let executable_a = path_a.join(filename);
        let executable_b = path_b.join(filename);
        write_fake_executable(&executable_a, b"path A fake")?;
        write_fake_executable(&executable_b, b"path B fake")?;
        let mut attempt_environment = EnvironmentPolicy::empty();
        attempt_environment.set("PATH", std::env::join_paths([&path_a])?)?;
        #[cfg(windows)]
        attempt_environment.set("PATHEXT", ".EXE")?;
        let attempt_execution =
            resolve_executable(&bare, &attempt_environment.executable_search(&worktree))?;
        let mut current_environment = EnvironmentPolicy::empty();
        current_environment.set("PATH", std::env::join_paths([&path_b])?)?;
        #[cfg(windows)]
        current_environment.set("PATHEXT", ".EXE")?;
        let current_resolution =
            resolve_executable(&bare, &current_environment.executable_search(&worktree))?;
        assert_eq!(current_resolution.path, fs::canonicalize(&executable_b)?);

        let mut path_persisted = serde_json::to_value(&worker_result)?;
        path_persisted["process_execution"] = serde_json::to_value(&attempt_execution)?;
        test_with_workspace(&state.database, &database, |connection| {
            connection.execute(
                "UPDATE main.task_attempts SET worker_result_json = ?1
                 WHERE workspace_id = current_workspace() AND attempt_id = ?2",
                params![path_persisted.to_string(), attempt_id.to_string()],
            )?;
            Ok(())
        })?;
        config
            .orchestrator
            .providers
            .codex
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("default Codex provider is missing"))?
            .executable = bare.to_string_lossy().into_owned();
        let path_steps = vec![RollbackManifestStep {
            component: "codex".to_owned(),
            backup_source: PathBuf::from("fake-provider.backup"),
            destination: PathBuf::from("path-a").join(filename),
        }];
        let path_context = rollback_resolution_context(&repository, &state, &config, &path_steps)?;
        let path_plan = trusted_rollback_steps(
            &repository,
            &repository.join(".colay/config.toml"),
            &state,
            &config,
            &path_context,
            &release_root,
            path_steps,
        )?;
        assert_eq!(path_plan[0].destination, fs::canonicalize(&executable_a)?);
        assert_ne!(path_plan[0].destination, current_resolution.path);

        let mut mismatched = path_persisted;
        mismatched["process_execution"]["path"] =
            serde_json::to_value(fs::canonicalize(&executable_b)?)?;
        test_with_workspace(&state.database, &database, |connection| {
            connection.execute(
                "UPDATE main.task_attempts SET worker_result_json = ?1
                 WHERE workspace_id = current_workspace() AND attempt_id = ?2",
                params![mismatched.to_string(), attempt_id.to_string()],
            )?;
            Ok(())
        })?;
        let mismatched_result = rollback_resolution_context(
            &repository,
            &state,
            &config,
            &[RollbackManifestStep {
                component: "codex".to_owned(),
                backup_source: PathBuf::from("fake-provider.backup"),
                destination: PathBuf::from("path-a").join(filename),
            }],
        );
        let Err(error) = mismatched_result else {
            anyhow::bail!("mismatched process execution evidence authorized rollback");
        };
        assert!(
            error
                .to_string()
                .contains("invalid process execution evidence")
        );

        test_with_workspace(&state.database, &database, |connection| {
            connection.execute(
                "UPDATE main.task_attempts SET worker_result_json = ?1
                 WHERE workspace_id = current_workspace() AND attempt_id = ?2",
                params![
                    serde_json::to_string(&worker_result)?,
                    attempt_id.to_string()
                ],
            )?;
            Ok(())
        })?;
        let legacy_result = rollback_resolution_context(
            &repository,
            &state,
            &config,
            &[RollbackManifestStep {
                component: "codex".to_owned(),
                backup_source: PathBuf::from("fake-provider.backup"),
                destination: configured.clone(),
            }],
        );
        let Err(error) = legacy_result else {
            anyhow::bail!("legacy worker result authorized executable rollback");
        };
        assert!(error.to_string().contains("process execution evidence"));

        let mut relative_persisted = serde_json::to_value(&worker_result)?;
        relative_persisted["process_execution"] = serde_json::json!({
            "configured": configured,
            "path": fs::canonicalize(&worktree_executable)?,
            "kind": "native",
            "validation": {
                "working_directory": fs::canonicalize(&worktree)?,
                "search_directory": null
            }
        });
        let other_worktree = state.worktrees.join("other-writable-worker");
        fs::create_dir_all(&other_worktree)?;
        test_with_workspace(&state.database, &database, |connection| {
            connection.execute(
                "UPDATE main.task_attempts SET worker_result_json = ?1
                 WHERE workspace_id = current_workspace() AND attempt_id = ?2",
                params![relative_persisted.to_string(), attempt_id.to_string()],
            )?;
            connection.execute(
                "UPDATE main.worktrees SET worktree_path = ?1
                 WHERE workspace_id = current_workspace() AND task_id = ?2 AND state = 'active'",
                params![
                    other_worktree.to_string_lossy(),
                    envelope.task_id.to_string()
                ],
            )?;
            Ok(())
        })?;
        let relative_mismatch = rollback_resolution_context(
            &repository,
            &state,
            &config,
            &[RollbackManifestStep {
                component: "codex".to_owned(),
                backup_source: PathBuf::from("fake-provider.backup"),
                destination: configured.clone(),
            }],
        );
        let Err(error) = relative_mismatch else {
            anyhow::bail!("relative identity bypassed its trusted active worktree");
        };
        assert!(error.to_string().contains("trusted worktree"));
        test_with_workspace(&state.database, &database, |connection| {
            connection.execute(
                "UPDATE main.worktrees SET worktree_path = ?1
                 WHERE workspace_id = current_workspace() AND task_id = ?2 AND state = 'active'",
                params![worktree.to_string_lossy(), envelope.task_id.to_string()],
            )?;
            Ok(())
        })?;

        let mut absolute_persisted = serde_json::to_value(&worker_result)?;
        absolute_persisted["process_execution"] = serde_json::json!({
            "configured": fs::canonicalize(&repository_executable)?,
            "path": fs::canonicalize(&repository_executable)?,
            "kind": "native",
            "validation": {
                "working_directory": fs::canonicalize(&worktree)?,
                "search_directory": null
            }
        });
        test_with_workspace(&state.database, &database, |connection| {
            connection.execute(
                "UPDATE main.task_attempts SET worker_result_json = ?1
                 WHERE workspace_id = current_workspace() AND attempt_id = ?2",
                params![absolute_persisted.to_string(), attempt_id.to_string()],
            )?;
            Ok(())
        })?;
        fs::remove_dir_all(&worktree)?;

        let absolute_context = rollback_resolution_context(
            &repository,
            &state,
            &config,
            &[RollbackManifestStep {
                component: "codex".to_owned(),
                backup_source: PathBuf::from("fake-provider.backup"),
                destination: repository_executable.clone(),
            }],
        )?;
        assert_eq!(
            absolute_context
                .codex_execution
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("absolute execution identity was omitted"))?
                .path,
            fs::canonicalize(repository_executable)?
        );
        Ok(())
    }

    #[test]
    fn unconfirmed_process_tree_blocks_task_and_redacts_audit_detail()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let temporary_root = fs::canonicalize(temporary.path())?;
        let state = test_state(temporary_root.join("state"));
        let database = Database::open(&state.database)?;
        database.migrate_with_backup(&state.backups)?;
        install_legacy_workspace(&database)?;
        let database = database.legacy_workspace()?;
        let now = Utc::now();
        let envelope = TaskEnvelope::new("verify termination", "verify termination", now);
        database.create_task(&NewTaskRecord {
            task_id: envelope.task_id,
            schema_version: envelope.schema_version.to_string(),
            state: TaskState::Running,
            objective: envelope.objective.clone(),
            original_request_redacted: envelope.original_request_redacted.clone(),
            envelope: &envelope,
            created_at: now,
        })?;
        let attempt_id = AttemptId::new();
        test_with_workspace(&state.database, &database, |connection| {
            connection.execute(
                "INSERT INTO main.task_attempts(
                    workspace_id, attempt_id, task_id, ordinal, provider_id, worker_mode, started_at
                 ) VALUES (current_workspace(), ?1, ?2, 1, 'codex', 'workspace_write', ?3)",
                params![
                    attempt_id.to_string(),
                    envelope.task_id.to_string(),
                    now.to_rfc3339(),
                ],
            )?;
            Ok(())
        })?;
        let request = WorkerRequest {
            schema_version: SchemaVersion::v1(),
            task_id: envelope.task_id,
            attempt_id,
            provider: ProviderId::Codex,
            objective: envelope.objective,
            prompt: "test".to_owned(),
            constraints: Vec::new(),
            acceptance_criteria: Vec::new(),
            workspace_root: temporary_root,
            sandbox: SandboxMode::WorkspaceWrite,
            profile: ModelProfile::Standard,
            model: None,
            reasoning_effort: None,
            timeout_seconds: 30,
            max_output_bytes: 1_024,
            resume_session_id: None,
            handover_payload: None,
        };
        let redactor = Redactor::new(&RedactionConfig::default())?;
        block_for_unconfirmed_termination(
            &state,
            &database,
            &request,
            orchestrator_domain::CorrelationId::new(),
            "taskkill failed after sk-abcdefghijklmnopqrstuvwxyz1234",
            &redactor,
        )?;

        let task = database.load_task(request.task_id)?.ok_or("task missing")?;
        assert_eq!(task.state, TaskState::Blocked);
        assert_eq!(task.resume_state, Some(TaskState::Running));
        let attempt = database
            .latest_task_attempt(request.task_id)?
            .ok_or("attempt missing")?;
        assert_eq!(attempt.outcome.as_deref(), Some("termination_unconfirmed"));
        let event_text = std::fs::read_to_string(&state.events)?;
        assert!(!event_text.contains("abcdefghijklmnopqrstuvwxyz1234"));
        let events = event_text
            .lines()
            .map(serde_json::from_str::<TaskEvent>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| event.verify_hash().unwrap_or(false))
        );
        Ok(())
    }
}
