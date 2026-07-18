use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{RepoPath, SchemaVersion, TaskAssessment, TaskId};

pub const TASK_ENVELOPE_SCHEMA_VERSION: &str = SchemaVersion::V1;
pub const SUPPORTED_TASK_ENVELOPE_SCHEMA_VERSIONS: &[&str] = &[TASK_ENVELOPE_SCHEMA_VERSION];

/// Persisted provider-neutral task input. Callers must redact secrets before assigning
/// `original_request_redacted`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskEnvelope {
    #[serde(deserialize_with = "crate::schema::deserialize_v1_schema_version")]
    pub schema_version: SchemaVersion,
    pub task_id: TaskId,
    pub objective: String,
    pub original_request_redacted: String,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub allowed_write_paths: Vec<RepoPath>,
    #[serde(default)]
    pub repository_wide_write_scope: bool,
    pub assessment: Option<TaskAssessment>,
    pub created_at: DateTime<Utc>,
}

impl TaskEnvelope {
    #[must_use]
    pub fn has_supported_schema(&self) -> bool {
        self.schema_version
            .is_supported_by(SUPPORTED_TASK_ENVELOPE_SCHEMA_VERSIONS)
    }

    #[must_use]
    pub fn new(
        objective: impl Into<String>,
        original_request_redacted: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            schema_version: SchemaVersion::v1(),
            task_id: TaskId::new(),
            objective: objective.into(),
            original_request_redacted: original_request_redacted.into(),
            constraints: Vec::new(),
            acceptance_criteria: Vec::new(),
            allowed_write_paths: Vec::new(),
            repository_wide_write_scope: false,
            assessment: None,
            created_at: now,
        }
    }
}
