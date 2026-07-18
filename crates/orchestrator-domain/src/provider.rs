use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    AttemptId, CommandEvidence, ProviderId, RepoPath, SchemaVersion, TaskId, TestEvidence,
    UsageObservation,
};

pub const WORKER_RESULT_SCHEMA_VERSION: &str = SchemaVersion::V1;
pub const SUPPORTED_WORKER_RESULT_SCHEMA_VERSIONS: &[&str] = &[WORKER_RESULT_SCHEMA_VERSION];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    Unsupported,
    Advertised,
    Verified,
    Degraded,
}

impl CapabilitySupport {
    #[must_use]
    pub const fn usable(self) -> bool {
        matches!(self, Self::Advertised | Self::Verified | Self::Degraded)
    }

    #[must_use]
    pub const fn verified(self) -> bool {
        matches!(self, Self::Verified)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub provider: ProviderId,
    pub status: HealthStatus,
    pub checked_at: DateTime<Utc>,
    pub latency_ms: Option<u64>,
    pub consecutive_failures: u32,
    pub detail: Option<String>,
}

impl ProviderHealth {
    #[must_use]
    pub fn unknown(provider: ProviderId, checked_at: DateTime<Utc>) -> Self {
        Self {
            provider,
            status: HealthStatus::Unknown,
            checked_at,
            latency_ms: None,
            consecutive_failures: 0,
            detail: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub provider: ProviderId,
    pub version: Option<String>,
    pub non_interactive: CapabilitySupport,
    pub structured_output: CapabilitySupport,
    pub writable: CapabilitySupport,
    pub read_only: CapabilitySupport,
    pub session_resume: CapabilitySupport,
    pub output_schema: CapabilitySupport,
    pub app_server: CapabilitySupport,
    pub reasoning_effort: CapabilitySupport,
    pub usage_events: CapabilitySupport,
    pub evidence: Vec<String>,
}

impl ProviderCapabilities {
    #[must_use]
    pub fn unsupported(provider: ProviderId) -> Self {
        Self {
            provider,
            version: None,
            non_interactive: CapabilitySupport::Unsupported,
            structured_output: CapabilitySupport::Unsupported,
            writable: CapabilitySupport::Unsupported,
            read_only: CapabilitySupport::Unsupported,
            session_resume: CapabilitySupport::Unsupported,
            output_schema: CapabilitySupport::Unsupported,
            app_server: CapabilitySupport::Unsupported,
            reasoning_effort: CapabilitySupport::Unsupported,
            usage_events: CapabilitySupport::Unsupported,
            evidence: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProfile {
    Economy,
    Standard,
    Premium,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerRequest {
    pub schema_version: SchemaVersion,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub provider: ProviderId,
    pub objective: String,
    pub prompt: String,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    /// Runtime-only absolute worktree root. It must never be written to the event log.
    pub workspace_root: PathBuf,
    pub sandbox: SandboxMode,
    pub profile: ModelProfile,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub timeout_seconds: u64,
    pub max_output_bytes: u64,
    pub resume_session_id: Option<String>,
    pub handover_payload: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerHandle {
    pub attempt_id: AttemptId,
    pub provider: ProviderId,
    pub process_id: Option<u32>,
    pub session_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawEventChannel {
    Stdout,
    Stderr,
    Protocol,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawEvent {
    pub channel: RawEventChannel,
    pub sequence: u64,
    pub bytes: Vec<u8>,
    pub received_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    Started {
        session_id: Option<String>,
    },
    Message {
        text: String,
    },
    CommandStarted {
        command_id: String,
        executable: String,
        args: Vec<String>,
    },
    CommandCompleted {
        command_id: String,
        exit_code: Option<i32>,
    },
    FileChanged {
        path: RepoPath,
    },
    Usage {
        observation: UsageObservation,
    },
    QuotaExceeded {
        detail: Option<String>,
    },
    CheckpointClaim {
        summary: String,
    },
    Completed {
        summary: Option<String>,
        /// Exact usage reported by the provider's structured result, recorded
        /// only as a local execution ledger observation. This is not a quota
        /// snapshot and must not be interpreted as provider quota remaining.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageObservation>,
    },
    Error {
        code: Option<String>,
        message: String,
        retryable: bool,
    },
    Unknown {
        event_type: String,
        payload: serde_json::Value,
        affects_lifecycle: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOutcome {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    QuotaExceeded,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerResult {
    #[serde(deserialize_with = "crate::schema::deserialize_v1_schema_version")]
    pub schema_version: SchemaVersion,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub provider: ProviderId,
    pub outcome: WorkerOutcome,
    pub exit_code: Option<i32>,
    pub session_id: Option<String>,
    pub summary: Option<String>,
    pub commands: Vec<CommandEvidence>,
    pub tests: Vec<TestEvidence>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub output_truncated: bool,
}

impl WorkerResult {
    #[must_use]
    pub fn has_supported_schema(&self) -> bool {
        self.schema_version
            .is_supported_by(SUPPORTED_WORKER_RESULT_SCHEMA_VERSIONS)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    Cancelled,
    AlreadyExited,
    Forced,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelResult {
    pub outcome: CancelOutcome,
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_event_reads_legacy_payload_without_usage() {
        let event = serde_json::from_value::<WorkerEvent>(serde_json::json!({
            "type": "completed",
            "summary": "done"
        }));

        assert!(matches!(
            event,
            Ok(WorkerEvent::Completed {
                summary: Some(ref summary),
                usage: None,
            }) if summary == "done"
        ));
    }
}
