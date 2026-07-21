//! Repository-local daemon heartbeat and shutdown loop.
#![allow(clippy::missing_errors_doc)]
#![cfg_attr(test, allow(clippy::panic))]

use std::{sync::Arc, time::Duration};

use chrono::{TimeDelta, Utc};
use orchestrator_domain::DaemonInstanceId;
use orchestrator_state::{DaemonLeaseRequest, Database, StateError};
use thiserror::Error;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

mod commands;
mod execution;
mod integration;
mod planning;

pub use commands::{CommandProcessingResult, MessageRedactor, process_next_client_command};
pub use execution::ExecutionServices;
pub use integration::IntegrationServices;
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
    instance_id: DaemonInstanceId,
    pid: u32,
    cancellation: CancellationToken,
    settings: DaemonSettings,
) -> Result<DaemonExit, DaemonError> {
    serve_with_commands(
        database,
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
    instance_id: DaemonInstanceId,
    pid: u32,
    cancellation: CancellationToken,
    settings: DaemonSettings,
    redactor: &dyn MessageRedactor,
) -> Result<DaemonExit, DaemonError> {
    validate_settings(settings)?;
    let started_at = Utc::now();
    database.acquire_daemon_lease(&DaemonLeaseRequest {
        instance_id,
        pid,
        started_at,
        ttl: settings.lease_ttl,
    })?;

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
                process_next_client_command(database, redactor, Utc::now())?;
            }
        }
    };
    database.release_daemon(instance_id, Utc::now())?;
    Ok(exit)
}

pub async fn serve_with_orchestration(
    database: Arc<Database>,
    instance_id: DaemonInstanceId,
    pid: u32,
    cancellation: CancellationToken,
    settings: DaemonSettings,
    redactor: Arc<dyn MessageRedactor>,
    planning: PlanningServices,
) -> Result<DaemonExit, DaemonError> {
    serve_with_runtime(
        database,
        instance_id,
        pid,
        cancellation,
        settings,
        redactor,
        planning,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn serve_with_full_orchestration(
    database: Arc<Database>,
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
        database,
        instance_id,
        pid,
        cancellation,
        settings,
        redactor,
        planning,
        Some(execution),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn serve_with_runtime(
    database: Arc<Database>,
    instance_id: DaemonInstanceId,
    pid: u32,
    cancellation: CancellationToken,
    settings: DaemonSettings,
    redactor: Arc<dyn MessageRedactor>,
    planning: PlanningServices,
    execution: Option<ExecutionServices>,
) -> Result<DaemonExit, DaemonError> {
    validate_settings(settings)?;
    let started_at = Utc::now();
    database.acquire_daemon_lease(&DaemonLeaseRequest {
        instance_id,
        pid,
        started_at,
        ttl: settings.lease_ttl,
    })?;
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
                process_next_client_command(&database, redactor.as_ref(), Utc::now())?;
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
                    let job_database = Arc::clone(&database);
                    let job_redactor = Arc::clone(&redactor);
                    let job_services = planning.clone();
                    active_planning = Some(tokio::spawn(async move {
                        process_next_orchestration_command(
                            &job_database,
                            &job_services,
                            job_redactor.as_ref(),
                            Utc::now(),
                        )
                        .await
                    }));
                }
                if let Some(execution) = execution.as_ref() {
                    execution::spawn_ready_tasks(
                        &database,
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
    execution::stop_execution_jobs(&execution_cancellation, execution_jobs).await?;
    database.release_daemon(instance_id, Utc::now())?;
    Ok(exit)
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
    use std::{sync::Arc, time::Duration};

    use chrono::{TimeDelta, Utc};
    use orchestrator_domain::{
        ClientCommand, ClientCommandAction, ClientCommandId, ClientCommandState, DaemonInstanceId,
        SessionId,
    };
    use orchestrator_state::{DaemonStatus, Database, StateResult};
    use tokio_util::sync::CancellationToken;

    use super::{
        DaemonError, DaemonExit, DaemonSettings, MessageRedactor, serve, serve_with_commands,
    };

    struct IdentityRedactor;

    impl MessageRedactor for IdentityRedactor {
        fn redact(&self, value: &str) -> String {
            value.to_owned()
        }
    }

    fn database() -> StateResult<Arc<Database>> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        Ok(Arc::new(database))
    }

    fn settings() -> DaemonSettings {
        DaemonSettings {
            heartbeat_interval: Duration::from_millis(10),
            command_poll_interval: Duration::from_millis(5),
            lease_ttl: TimeDelta::milliseconds(100),
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
        let database = database()?;
        let instance_id = DaemonInstanceId::new();
        let cancellation = CancellationToken::new();
        let service_database = Arc::clone(&database);
        let service_cancellation = cancellation.clone();
        let service = tokio::spawn(async move {
            serve(
                &service_database,
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
    async fn stop_request_exits_and_second_runtime_is_rejected() -> Result<(), DaemonError> {
        let database = database()?;
        let instance_id = DaemonInstanceId::new();
        let cancellation = CancellationToken::new();
        let service_database = Arc::clone(&database);
        let service_cancellation = cancellation.clone();
        let service = tokio::spawn(async move {
            serve(
                &service_database,
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
        let database = database()?;
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
        database.submit_client_command(&command)?;
        let cancellation = CancellationToken::new();
        let service_database = Arc::clone(&database);
        let service_cancellation = cancellation.clone();
        let service = tokio::spawn(async move {
            serve_with_commands(
                &service_database,
                DaemonInstanceId::new(),
                42,
                service_cancellation,
                settings(),
                &IdentityRedactor,
            )
            .await
        });
        for _ in 0..100 {
            if database.load_session(session_id)?.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(database.load_session(session_id)?.is_some());
        assert_eq!(
            database
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
