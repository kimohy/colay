use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ModelProfile, ProviderId, ReasoningEffort, RoutingDecisionId, SchemaVersion, TaskId};

pub const ROUTING_DECISION_SCHEMA_VERSION: &str = SchemaVersion::V1;
pub const SUPPORTED_ROUTING_DECISION_SCHEMA_VERSIONS: &[&str] = &[ROUTING_DECISION_SCHEMA_VERSION];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreComponents {
    pub role_affinity: f64,
    pub capability: f64,
    pub credit_headroom: f64,
    pub provider_health: f64,
    pub admin_priority: f64,
    pub usage_confidence: f64,
    pub exhaustion_penalty: f64,
    pub failure_penalty: f64,
    pub handover_penalty: f64,
    pub uncertainty_penalty: f64,
}

impl ScoreComponents {
    #[must_use]
    pub fn total(&self) -> f64 {
        self.role_affinity
            + self.capability
            + self.credit_headroom
            + self.provider_health
            + self.admin_priority
            + self.usage_confidence
            - self.exhaustion_penalty
            - self.failure_penalty
            - self.handover_penalty
            - self.uncertainty_penalty
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateRoutingScore {
    pub provider: ProviderId,
    pub eligible: bool,
    pub exclusions: Vec<String>,
    pub components: ScoreComponents,
    pub total: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingDecision {
    #[serde(deserialize_with = "crate::schema::deserialize_v1_schema_version")]
    pub schema_version: SchemaVersion,
    pub decision_id: RoutingDecisionId,
    pub task_id: TaskId,
    pub selected_provider: Option<ProviderId>,
    pub selected_profile: Option<ModelProfile>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub parallel_workers: usize,
    pub candidate_scores: Vec<CandidateRoutingScore>,
    pub rationale: Vec<String>,
    pub downgrade: bool,
    pub applied_policy: String,
    pub blocked_options: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl RoutingDecision {
    #[must_use]
    pub fn has_supported_schema(&self) -> bool {
        self.schema_version
            .is_supported_by(SUPPORTED_ROUTING_DECISION_SCHEMA_VERSIONS)
    }
}
