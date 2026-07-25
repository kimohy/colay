//! User-local daemon heartbeat, IPC, and orchestration loop.
#![allow(clippy::missing_errors_doc)]
#![cfg_attr(test, allow(clippy::panic))]

use std::{sync::Arc, time::Duration};

use chrono::{TimeDelta, Utc};
use orchestrator_domain::DaemonInstanceId;
use orchestrator_state::{DaemonLeaseRequest, Database, StateError, WorkspaceId};
use thiserror::Error;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

mod commands;
mod conversation;
mod execution;
mod integration;
mod ipc;
mod planning;
#[cfg(test)]
mod test_support;

pub use commands::{CommandProcessingResult, MessageRedactor, process_next_client_command};
pub use execution::ExecutionServices;
pub use integration::IntegrationServices;
#[cfg(windows)]
pub use ipc::windows_named_pipe_security_descriptor;
pub use ipc::{
    DaemonOwnerLock, IPC_SCHEMA_VERSION, IpcError, IpcRequest, IpcResponse, IpcServer,
    WORKSPACE_DOCTOR_SCHEMA_VERSION, WorkspaceArtifactDiagnostics, WorkspaceArtifactScope,
    WorkspaceAuditDiagnostics, WorkspaceDoctorDiagnostics, ipc_endpoint,
};
pub use planning::{PlanningServices, process_next_orchestration_command};

#[derive(Clone, Copy, Debug)]
pub struct DaemonSettings {
    pub heartbeat_interval: Duration,
    pub command_poll_interval: Duration,
    pub lease_ttl: TimeDelta,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(1),
            command_poll_interval: Duration::from_millis(100),
            lease_ttl: TimeDelta::seconds(5),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonExit {
    StopRequested,
    Cancelled,
}

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("invalid daemon settings: {0}")]
    InvalidSettings(String),
    #[error(transparent)]
    State(#[from] StateError),
}

pub async fn serve(
    database: &Database,
    workspace_id: WorkspaceId,
    instance_id: DaemonInstanceId,
    pid: u32,
    cancellation: CancellationToken,
    settings: DaemonSettings,
) -> Result<DaemonExit, DaemonError> {
    serve_with_commands(
        database,
        workspace_id,
        instance_id,
        pid,
        cancellation,
        settings,
        &IdentityRedactor,
    )
    .await
}

struct IdentityRedactor;

impl MessageRedactor for IdentityRedactor {
    fn redact(&self, value: &str) -> String {
        value.to_owned()
    }
}

pub async fn serve_with_commands(
    database: &Database,
    workspace_id: WorkspaceId,
    instance_id: DaemonInstanceId,
    pid: u32,
    cancellation: CancellationToken,
    settings: DaemonSettings,
    redactor: &dyn MessageRedactor,
) -> Result<DaemonExit, DaemonError> {
    validate_settings(settings)?;
    reconcile_daemon_startup(database, workspace_id, Utc::now())?;
    let started_at = Utc::now();
    database.acquire_daemon_lease(&DaemonLeaseRequest {
        instance_id,
        pid,
        started_at,
        ttl: settings.lease_ttl,
    })?;
    let lease = OwnedLeaseGuard::new(database, instance_id);
    let result = serve_with_commands_loop(
        database,
        workspace_id,
        instance_id,
        cancellation,
        settings,
        redactor,
    )
    .await;
    if result.is_ok() {
        lease.finish()?;
    }
    result
}

#[cfg(test)]
async fn serve_with_commands_on_owned_lease(
    database: &Database,
    workspace_id: WorkspaceId,
    instance_id: DaemonInstanceId,
    cancellation: CancellationToken,
    settings: DaemonSettings,
    redactor: &dyn MessageRedactor,
) -> Result<DaemonExit, DaemonError> {
    validate_settings(settings)?;
    let lease = OwnedLeaseGuard::new(database, instance_id);
    reconcile_daemon_startup(database, workspace_id, Utc::now())?;
    let result = serve_with_commands_loop(
        database,
        workspace_id,
        instance_id,
        cancellation,
        settings,
        redactor,
    )
    .await;
    if result.is_ok() {
        lease.finish()?;
    }
    result
}

async fn serve_with_commands_loop(
    database: &Database,
    workspace_id: WorkspaceId,
    instance_id: DaemonInstanceId,
    cancellation: CancellationToken,
    settings: DaemonSettings,
    redactor: &dyn MessageRedactor,
) -> Result<DaemonExit, DaemonError> {
    let workspace = database.workspace(workspace_id);
    database.heartbeat_daemon(instance_id, Utc::now(), settings.lease_ttl)?;
    let mut heartbeat_interval = tokio::time::interval(settings.heartbeat_interval);
    heartbeat_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut command_interval = tokio::time::interval(settings.command_poll_interval);
    command_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let exit = loop {
        tokio::select! {
            () = cancellation.cancelled() => break DaemonExit::Cancelled,
            _ = heartbeat_interval.tick() => {
                if database.daemon_stop_requested(instance_id)? {
                    break DaemonExit::StopRequested;
                }
                database.heartbeat_daemon(instance_id, Utc::now(), settings.lease_ttl)?;
            }
            _ = command_interval.tick() => {
                process_next_client_command(&workspace, redactor, Utc::now())?;
            }
        }
    };
    Ok(exit)
}

#[allow(clippy::too_many_arguments)]
pub async fn serve_with_orchestration(
    database: Arc<Database>,
    workspace_id: WorkspaceId,
    instance_id: DaemonInstanceId,
    pid: u32,
    cancellation: CancellationToken,
    settings: DaemonSettings,
    redactor: Arc<dyn MessageRedactor>,
    planning: PlanningServices,
) -> Result<DaemonExit, DaemonError> {
    serve_with_runtime(
        &database,
        workspace_id,
        instance_id,
        pid,
        cancellation,
        settings,
        redactor,
        planning,
        None,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn serve_with_full_orchestration(
    database: Arc<Database>,
    workspace_id: WorkspaceId,
    instance_id: DaemonInstanceId,
    pid: u32,
    cancellation: CancellationToken,
    settings: DaemonSettings,
    redactor: Arc<dyn MessageRedactor>,
    planning: PlanningServices,
    execution: ExecutionServices,
) -> Result<DaemonExit, DaemonError> {
    execution::validate_execution_services(&execution)?;
    serve_with_runtime(
        &database,
        workspace_id,
        instance_id,
        pid,
        cancellation,
        settings,
        redactor,
        planning,
        Some(execution),
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn serve_with_full_orchestration_on_owned_lease(
    database: Arc<Database>,
    workspace_id: WorkspaceId,
    instance_id: DaemonInstanceId,
    cancellation: CancellationToken,
    settings: DaemonSettings,
    redactor: Arc<dyn MessageRedactor>,
    planning: PlanningServices,
    execution: ExecutionServices,
) -> Result<DaemonExit, DaemonError> {
    let lease = OwnedLeaseGuard::new(&database, instance_id);
    execution::validate_execution_services(&execution)?;
    serve_with_runtime(
        &database,
        workspace_id,
        instance_id,
        0,
        cancellation,
        settings,
        redactor,
        planning,
        Some(execution),
        Some(lease),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn serve_with_runtime<'a>(
    database: &'a Arc<Database>,
    workspace_id: WorkspaceId,
    instance_id: DaemonInstanceId,
    pid: u32,
    cancellation: CancellationToken,
    settings: DaemonSettings,
    redactor: Arc<dyn MessageRedactor>,
    planning: PlanningServices,
    execution: Option<ExecutionServices>,
    owned_lease: Option<OwnedLeaseGuard<'a>>,
) -> Result<DaemonExit, DaemonError> {
    validate_settings(settings)?;
    let started_at = Utc::now();
    let lease = if let Some(lease) = owned_lease {
        database.heartbeat_daemon(instance_id, started_at, settings.lease_ttl)?;
        reconcile_daemon_startup(database, workspace_id, started_at)?;
        lease
    } else {
        reconcile_daemon_startup(database, workspace_id, started_at)?;
        let started_at = Utc::now();
        database.acquire_daemon_lease(&DaemonLeaseRequest {
            instance_id,
            pid,
            started_at,
            ttl: settings.lease_ttl,
        })?;
        OwnedLeaseGuard::new(database, instance_id)
    };
    let workspace = database.workspace(workspace_id);
    let mut heartbeat_interval = tokio::time::interval(settings.heartbeat_interval);
    heartbeat_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut command_interval = tokio::time::interval(settings.command_poll_interval);
    command_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut active_planning = None;
    let execution_cancellation = cancellation.child_token();
    let mut execution_jobs = Vec::new();
    let exit = loop {
        tokio::select! {
            () = cancellation.cancelled() => break DaemonExit::Cancelled,
            _ = heartbeat_interval.tick() => {
                if database.daemon_stop_requested(instance_id)? {
                    break DaemonExit::StopRequested;
                }
                database.heartbeat_daemon(instance_id, Utc::now(), settings.lease_ttl)?;
            }
            _ = command_interval.tick() => {
                process_next_client_command(&workspace, redactor.as_ref(), Utc::now())?;
                execution::reap_finished_tasks(&mut execution_jobs).await?;
                if active_planning
                    .as_ref()
                    .is_some_and(tokio::task::JoinHandle::is_finished)
                {
                    let finished = active_planning.take().ok_or_else(|| {
                        DaemonError::InvalidSettings("finished planning job disappeared".to_owned())
                    })?;
                    finished.await.map_err(|error| {
                        DaemonError::InvalidSettings(format!("planning job failed: {error}"))
                    })??;
                }
                if active_planning.is_none() {
                    let job_database = Arc::clone(database);
                    let job_redactor = Arc::clone(&redactor);
                    let job_services = planning.clone();
                    active_planning = Some(tokio::spawn(async move {
                        let workspace = job_database.workspace(workspace_id);
                        process_next_orchestration_command(
                            &workspace,
                            &job_services,
                            job_redactor.as_ref(),
                            Utc::now(),
                        )
                        .await
                    }));
                }
                if let Some(execution) = execution.as_ref() {
                    execution::spawn_ready_tasks(
                        database,
                        workspace_id,
                        instance_id,
                        execution,
                        &redactor,
                        &execution_cancellation,
                        &mut execution_jobs,
                    )?;
                }
            }
        }
    };
    if let Some(job) = active_planning {
        job.abort();
        let _ = job.await;
    }
    workspace.reconcile_interrupted_conversation_attempts(
        Utc::now(),
        "conversation attempt was interrupted by daemon shutdown",
    )?;
    execution::stop_execution_jobs(&execution_cancellation, execution_jobs).await?;
    lease.finish()?;
    Ok(exit)
}

fn reconcile_daemon_startup(
    database: &Database,
    workspace_id: WorkspaceId,
    started_at: chrono::DateTime<Utc>,
) -> Result<(), DaemonError> {
    if database.load_workspace(workspace_id)?.is_none() {
        return Err(StateError::WorkspaceNotFound {
            workspace_id: workspace_id.to_string(),
        }
        .into());
    }
    let workspace = database.workspace(workspace_id);
    workspace.reconcile_interrupted_conversation_attempts(
        started_at,
        "conversation attempt was interrupted before daemon startup",
    )?;
    workspace.recover_stale_client_commands(started_at)?;
    workspace.reconcile_interrupted_integrations(started_at)?;
    Ok(())
}

struct OwnedLeaseGuard<'a> {
    database: &'a Database,
    instance_id: DaemonInstanceId,
    armed: bool,
}

impl<'a> OwnedLeaseGuard<'a> {
    const fn new(database: &'a Database, instance_id: DaemonInstanceId) -> Self {
        Self {
            database,
            instance_id,
            armed: true,
        }
    }

    fn finish(mut self) -> Result<(), StateError> {
        self.database.release_daemon(self.instance_id, Utc::now())?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for OwnedLeaseGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.database.release_daemon(self.instance_id, Utc::now());
        }
    }
}

fn validate_settings(settings: DaemonSettings) -> Result<(), DaemonError> {
    if settings.heartbeat_interval.is_zero() {
        return Err(DaemonError::InvalidSettings(
            "heartbeat interval must be positive".to_owned(),
        ));
    }
    if settings.command_poll_interval.is_zero() {
        return Err(DaemonError::InvalidSettings(
            "command poll interval must be positive".to_owned(),
        ));
    }
    if settings.lease_ttl <= TimeDelta::zero() {
        return Err(DaemonError::InvalidSettings(
            "lease TTL must be positive".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::PathBuf,
        sync::Arc,
        time::Duration,
    };

    use async_trait::async_trait;
    use chrono::{TimeDelta, Utc};
    use orchestrator_domain::{
        ClientCommand, ClientCommandAction, ClientCommandId, ClientCommandState, DaemonInstanceId,
        GraphValidationPolicy, ModelProfile, ProviderId, SessionId,
    };
    use orchestrator_engine::{
        ConversationFailure, ConversationOrchestrator, ConversationRequest, ConversationResponse,
        EngineResult, PlannerFailure, PlannerRequest, PlannerResponse, TaskExecutionReport,
        TaskExecutionRequest, TaskExecutor, TaskPlanner,
    };
    use orchestrator_state::{
        DaemonLeaseRequest, DaemonPhase, DaemonStatus, Database, StateResult, WorkspaceId,
    };
    use tokio_util::sync::CancellationToken;

    use super::{
        DaemonError, DaemonExit, DaemonSettings, MessageRedactor, serve, serve_with_commands,
        serve_with_commands_on_owned_lease, serve_with_full_orchestration_on_owned_lease,
    };
    use crate::{ExecutionServices, PlanningServices, test_support::fresh_database};

    struct IdentityRedactor;

    impl MessageRedactor for IdentityRedactor {
        fn redact(&self, value: &str) -> String {
            value.to_owned()
        }
    }

    struct UnusedExecutor;

    #[async_trait]
    impl TaskExecutor for UnusedExecutor {
        async fn execute(
            &self,
            _request: TaskExecutionRequest,
            _cancellation: CancellationToken,
        ) -> EngineResult<TaskExecutionReport> {
            panic!("invalid startup settings must be rejected before execution")
        }
    }

    struct UnusedPlanner;

    #[async_trait]
    impl TaskPlanner for UnusedPlanner {
        async fn propose(
            &self,
            _request: PlannerRequest,
        ) -> Result<PlannerResponse, PlannerFailure> {
            Err(PlannerFailure::Invocation {
                reason: "unused planner".to_owned(),
                evidence_redacted: "unused planner".to_owned(),
            })
        }
    }

    struct UnusedConversation;

    #[async_trait]
    impl ConversationOrchestrator for UnusedConversation {
        async fn converse(
            &self,
            _request: ConversationRequest,
        ) -> Result<ConversationResponse, ConversationFailure> {
            Err(ConversationFailure::Invocation {
                reason: "unused conversation".to_owned(),
                evidence_redacted: "unused conversation".to_owned(),
            })
        }
    }

    fn planning_services() -> PlanningServices {
        PlanningServices {
            conversation: Arc::new(UnusedConversation),
            repository_root: PathBuf::from("."),
            planner: Arc::new(UnusedPlanner),
            planner_provider: ProviderId::Codex,
            validation_policy: GraphValidationPolicy {
                eligible_providers: BTreeSet::from([ProviderId::Codex]),
                eligible_profiles: BTreeSet::from([ModelProfile::Standard]),
                max_parallel_workers: 1,
                per_provider_limits: BTreeMap::from([(ProviderId::Codex, 1)]),
            },
            integration: None,
        }
    }

    fn execution_services(global_limit: usize) -> ExecutionServices {
        ExecutionServices {
            executor: Arc::new(UnusedExecutor),
            repository_root: PathBuf::from("."),
            state_root: PathBuf::from("."),
            global_limit,
            provider_limits: BTreeMap::new(),
            claim_ttl: TimeDelta::seconds(5),
        }
    }

    fn database() -> StateResult<(Arc<Database>, WorkspaceId)> {
        let (database, workspace_id) = fresh_database()?;
        Ok((Arc::new(database), workspace_id))
    }

    fn settings() -> DaemonSettings {
        DaemonSettings {
            heartbeat_interval: Duration::from_millis(10),
            command_poll_interval: Duration::from_millis(5),
            lease_ttl: TimeDelta::seconds(5),
        }
    }

    async fn wait_until_online(database: &Database) -> DaemonStatus {
        for _ in 0..50 {
            let status = database
                .daemon_status(Utc::now())
                .unwrap_or(DaemonStatus::Stopped);
            if matches!(status, DaemonStatus::Online(_)) {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        DaemonStatus::Stopped
    }

    #[tokio::test]
    async fn heartbeat_runs_until_cancellation_and_releases_lease() -> Result<(), DaemonError> {
        let (database, service_workspace_id) = database()?;
        let instance_id = DaemonInstanceId::new();
        let cancellation = CancellationToken::new();
        let service_database = Arc::clone(&database);
        let service_cancellation = cancellation.clone();
        let service = tokio::spawn(async move {
            serve(
                &service_database,
                service_workspace_id,
                instance_id,
                42,
                service_cancellation,
                settings(),
            )
            .await
        });
        let initial = wait_until_online(&database).await;
        let DaemonStatus::Online(initial) = initial else {
            return Err(DaemonError::InvalidSettings(
                "daemon did not become online".to_owned(),
            ));
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        let current = database.daemon_status(Utc::now())?;
        let DaemonStatus::Online(current) = current else {
            return Err(DaemonError::InvalidSettings(
                "daemon did not remain online".to_owned(),
            ));
        };
        assert!(current.heartbeat_at >= initial.heartbeat_at);
        cancellation.cancel();
        assert_eq!(
            service.await.map_err(|error| {
                DaemonError::InvalidSettings(format!("daemon task failed: {error}"))
            })??,
            DaemonExit::Cancelled
        );
        assert_eq!(database.daemon_status(Utc::now())?, DaemonStatus::Stopped);
        Ok(())
    }

    #[tokio::test]
    async fn owned_startup_lease_enters_loop_without_reacquiring() -> Result<(), DaemonError> {
        let (database, service_workspace_id) = database()?;
        let instance_id = DaemonInstanceId::new();
        let now = Utc::now();
        database.acquire_daemon_startup_lease(&DaemonLeaseRequest {
            instance_id,
            pid: 42,
            started_at: now,
            ttl: settings().lease_ttl,
        })?;
        database.transition_daemon_phase(instance_id, DaemonPhase::Probing, None)?;
        database.transition_daemon_phase(instance_id, DaemonPhase::Online, None)?;
        let cancellation = CancellationToken::new();
        let service_database = Arc::clone(&database);
        let service_cancellation = cancellation.clone();
        let service = tokio::spawn(async move {
            serve_with_commands_on_owned_lease(
                &service_database,
                service_workspace_id,
                instance_id,
                service_cancellation,
                settings(),
                &IdentityRedactor,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        let DaemonStatus::Online(online) = database.daemon_status(Utc::now())? else {
            return Err(DaemonError::InvalidSettings(
                "owned daemon lease did not remain online".to_owned(),
            ));
        };
        assert!(online.heartbeat_at > now);
        cancellation.cancel();
        assert_eq!(
            service.await.map_err(|error| {
                DaemonError::InvalidSettings(format!("daemon task failed: {error}"))
            })??,
            DaemonExit::Cancelled
        );
        assert_eq!(database.daemon_status(Utc::now())?, DaemonStatus::Stopped);
        Ok(())
    }

    #[tokio::test]
    async fn reconciliation_failure_after_acquisition_releases_owned_lease()
    -> Result<(), DaemonError> {
        let (database, _) = database()?;
        let instance_id = DaemonInstanceId::new();
        database.acquire_daemon_startup_lease(&DaemonLeaseRequest {
            instance_id,
            pid: 42,
            started_at: Utc::now(),
            ttl: settings().lease_ttl,
        })?;
        let missing_workspace =
            "0198f7e0-3db2-7aa2-a982-8c334805b3ad"
                .parse()
                .map_err(|error| {
                    DaemonError::InvalidSettings(format!("invalid fixture UUID: {error}"))
                })?;

        let result = serve_with_commands_on_owned_lease(
            &database,
            missing_workspace,
            instance_id,
            CancellationToken::new(),
            settings(),
            &IdentityRedactor,
        )
        .await;

        assert!(matches!(result, Err(DaemonError::State(_))));
        assert_eq!(database.daemon_status(Utc::now())?, DaemonStatus::Stopped);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_execution_services_release_an_already_owned_lease() -> Result<(), DaemonError>
    {
        let (database, workspace_id) = database()?;
        let instance_id = DaemonInstanceId::new();
        database.acquire_daemon_startup_lease(&DaemonLeaseRequest {
            instance_id,
            pid: 42,
            started_at: Utc::now(),
            ttl: settings().lease_ttl,
        })?;

        let result = serve_with_full_orchestration_on_owned_lease(
            Arc::clone(&database),
            workspace_id,
            instance_id,
            CancellationToken::new(),
            settings(),
            Arc::new(IdentityRedactor),
            planning_services(),
            execution_services(0),
        )
        .await;

        assert!(matches!(result, Err(DaemonError::InvalidSettings(_))));
        assert_eq!(database.daemon_status(Utc::now())?, DaemonStatus::Stopped);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_runtime_settings_release_an_already_owned_lease() -> Result<(), DaemonError> {
        let (database, workspace_id) = database()?;
        let instance_id = DaemonInstanceId::new();
        database.acquire_daemon_startup_lease(&DaemonLeaseRequest {
            instance_id,
            pid: 42,
            started_at: Utc::now(),
            ttl: settings().lease_ttl,
        })?;
        let mut invalid_settings = settings();
        invalid_settings.heartbeat_interval = Duration::ZERO;

        let result = serve_with_full_orchestration_on_owned_lease(
            Arc::clone(&database),
            workspace_id,
            instance_id,
            CancellationToken::new(),
            invalid_settings,
            Arc::new(IdentityRedactor),
            planning_services(),
            execution_services(1),
        )
        .await;

        assert!(matches!(result, Err(DaemonError::InvalidSettings(_))));
        assert_eq!(database.daemon_status(Utc::now())?, DaemonStatus::Stopped);
        Ok(())
    }

    #[tokio::test]
    async fn stop_request_exits_and_second_runtime_is_rejected() -> Result<(), DaemonError> {
        let (database, service_workspace_id) = database()?;
        let instance_id = DaemonInstanceId::new();
        let cancellation = CancellationToken::new();
        let service_database = Arc::clone(&database);
        let service_cancellation = cancellation.clone();
        let service = tokio::spawn(async move {
            serve(
                &service_database,
                service_workspace_id,
                instance_id,
                42,
                service_cancellation,
                settings(),
            )
            .await
        });
        assert!(matches!(
            wait_until_online(&database).await,
            DaemonStatus::Online(_)
        ));

        let conflict = serve(
            &database,
            service_workspace_id,
            DaemonInstanceId::new(),
            43,
            CancellationToken::new(),
            settings(),
        )
        .await;
        assert!(matches!(conflict, Err(DaemonError::State(_))));

        database.request_daemon_stop(instance_id, Utc::now())?;
        assert_eq!(
            service.await.map_err(|error| {
                DaemonError::InvalidSettings(format!("daemon task failed: {error}"))
            })??,
            DaemonExit::StopRequested
        );
        assert_eq!(database.daemon_status(Utc::now())?, DaemonStatus::Stopped);
        Ok(())
    }

    #[tokio::test]
    async fn service_loop_processes_pending_session_commands() -> Result<(), DaemonError> {
        let (database, service_workspace_id) = database()?;
        let workspace = database.workspace(service_workspace_id);
        let session_id = SessionId::new();
        let command = ClientCommand {
            command_id: ClientCommandId::new(),
            session_id: None,
            task_id: None,
            action: ClientCommandAction::CreateSession,
            payload: serde_json::json!({
                "session_id": session_id,
                "title": "chat session",
            }),
            idempotency_key: "runtime-create-session".to_owned(),
            state: ClientCommandState::Pending,
            requested_by: "test".to_owned(),
            requested_at: Utc::now(),
            claimed_at: None,
            completed_at: None,
            outcome: None,
        };
        workspace.submit_client_command(&command)?;
        let cancellation = CancellationToken::new();
        let service_database = Arc::clone(&database);
        let service_cancellation = cancellation.clone();
        let service = tokio::spawn(async move {
            serve_with_commands(
                &service_database,
                service_workspace_id,
                DaemonInstanceId::new(),
                42,
                service_cancellation,
                settings(),
                &IdentityRedactor,
            )
            .await
        });
        for _ in 0..100 {
            if workspace.load_session(session_id)?.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(workspace.load_session(session_id)?.is_some());
        assert_eq!(
            workspace
                .load_client_command(command.command_id)?
                .map(|value| value.state),
            Some(ClientCommandState::Completed)
        );
        cancellation.cancel();
        assert_eq!(
            service.await.map_err(|error| {
                DaemonError::InvalidSettings(format!("daemon task failed: {error}"))
            })??,
            DaemonExit::Cancelled
        );
        Ok(())
    }
}
