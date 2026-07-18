use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{CommandEvidenceId, ProviderId, RepoPath};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub description: String,
    pub status: PlanStepStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedStep {
    pub step_id: String,
    pub summary: String,
    pub completed_at: DateTime<Utc>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEvidence {
    pub id: CommandEvidenceId,
    pub executable: String,
    pub args: Vec<String>,
    pub cwd: Option<RepoPath>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output_truncated: bool,
    pub stdout_artifact: Option<RepoPath>,
    pub stderr_artifact: Option<RepoPath>,
    pub stdout_sha256: Option<String>,
    pub stderr_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed,
    Skipped,
    TimedOut,
    Inconclusive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestEvidence {
    pub name: String,
    pub status: TestStatus,
    pub command_id: Option<CommandEvidenceId>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub decision: String,
    pub rationale: String,
    pub alternatives: Vec<String>,
    pub decided_by: ProviderId,
    pub decided_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    pub code: Option<String>,
    pub summary: String,
    pub retryable: bool,
    pub occurred_at: DateTime<Utc>,
}
