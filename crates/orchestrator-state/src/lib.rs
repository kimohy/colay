//! Local persistence primitives for the orchestrator.
//!
//! `SQLite` is the system of record. The JSONL event log is an append-only, hash-chained
//! audit replica exported from the database outbox.
#![allow(clippy::missing_errors_doc)]
#![cfg_attr(test, allow(clippy::panic))]

mod artifacts;
mod client_commands;
mod config;
mod config_layers;
mod database;
mod error;
mod event_log;
mod leases;
mod migrations;
mod paths;
mod permissions;
mod records;
mod sessions;

pub use artifacts::{ArtifactStore, StoredArtifact};
pub use client_commands::ClientCommandRecoveryDisposition;
pub use config::{
    CONFIG_SCHEMA_VERSION, ConfigDocument, ConfigMigrationApplyResult, ConfigMigrationPlan,
    ConfigMigrationPreview, ConfigMigrationResult, ConfigMigrationStep, ConfigValidationError,
    FeatureConfig, MigratableConfigDocument, ModelProfileConfig, OrchestratorConfig,
    ProviderConfig, ProviderConfigs, RedactionSettings, RootConfig, UsageProbeConfig,
};
pub use config_layers::{
    ConfigEnvironment, ConfigLayerKind, ConfigRequest, ConfigSource, EffectiveConfig,
    load_effective_config,
};
pub use database::{Database, DatabaseHealth, OutboxRecord};
pub use error::{StateError, StateResult};
pub use event_log::{EventLog, ReconciliationReport};
pub use leases::{
    CoordinatorLease, CoordinatorLeaseRequest, LeaseRenewal, WorkerLease, WorkerLeaseMode,
    WorkerLeaseRequest,
};
pub use migrations::{
    AppliedMigration, MigrationManager, MigrationPlan, MigrationStatus,
    ROLLBACK_PLAN_SCHEMA_VERSION, RollbackApplyResult, RollbackPlan, STATE_SCHEMA_VERSION,
};
pub use paths::RepositoryStatePaths;
pub use permissions::{
    ensure_private_directory, ensure_private_file, reject_symlink_components, verify_private_file,
};
pub use records::{
    ClaimedControlRecoveryPolicy, ControlAction, ControlRecoveryDisposition, ControlRequest,
    NewTaskRecord, RecoveredControl, RoutingAuditRecord, StoredHandover, StoredTask,
    StoredTaskAttempt, StoredWorktree, TaskListFilter,
};
pub use sessions::{NewSessionRecord, SessionListFilter, StoredSession};

pub(crate) struct CanonicalTempDir {
    _directory: tempfile::TempDir,
    canonical_path: std::path::PathBuf,
}

impl CanonicalTempDir {
    pub(crate) fn new(context: impl Into<std::path::PathBuf>) -> StateResult<Self> {
        let context = context.into();
        let directory = tempfile::tempdir().map_err(|error| StateError::io(&context, error))?;
        let canonical_path = std::fs::canonicalize(directory.path())
            .map_err(|error| StateError::io(directory.path(), error))?;
        Ok(Self {
            _directory: directory,
            canonical_path,
        })
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.canonical_path
    }
}
