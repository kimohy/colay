use std::cmp::Ordering;

use chrono::{DateTime, Utc};
pub use orchestrator_domain::{CandidateRoutingScore, RoutingDecision, ScoreComponents};
use orchestrator_domain::{
    CapabilitySupport, Difficulty, HealthStatus, ModelProfile, ProviderCapabilities,
    ProviderHealth, ProviderId, QualityTier, ReasoningEffort, TaskAssessment, TaskId,
    UsageConfidence,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{BudgetForecast, ForecastStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRole {
    Implementation,
    Debugging,
    Testing,
    Refactoring,
    Architecture,
    SecurityReview,
    IndependentReview,
    RepositoryResearch,
    Planning,
    Integration,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingCandidate {
    pub provider: ProviderId,
    pub enabled: bool,
    pub capabilities: ProviderCapabilities,
    pub health: ProviderHealth,
    pub budgets: Vec<BudgetForecast>,
    pub available_profiles: Vec<ModelProfile>,
    /// Comparable only when an administrator or a minimum-five-observation calibration
    /// converted provider quota units into orchestrator work units.
    pub calibrated_remaining_work_units: Option<f64>,
    pub recent_failure_rate: f64,
    /// Administrator priority in the inclusive range 0..=100.
    pub admin_priority: i32,
    /// Normalized 0..=1 cost of moving context to this provider.
    pub handover_cost: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoutingContext {
    pub task_id: TaskId,
    pub assessment: TaskAssessment,
    pub role: TaskRole,
    pub writable: bool,
    pub candidates: Vec<RoutingCandidate>,
    pub current_provider: Option<ProviderId>,
    pub implementation_provider: Option<ProviderId>,
    pub manually_requested_provider: Option<ProviderId>,
    pub conserve_budget: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub max_parallel_workers: usize,
    pub allow_amber: bool,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            max_parallel_workers: 1,
            allow_amber: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RoutingEngine;

impl RoutingEngine {
    /// Applies eligibility gates before deterministic weighted provider scoring.
    ///
    /// # Errors
    ///
    /// Returns [`RoutingError`] for invalid assessment invariants or zero configured workers.
    pub fn route(
        context: &RoutingContext,
        config: &RoutingConfig,
        now: DateTime<Utc>,
    ) -> Result<RoutingDecision, RoutingError> {
        if config.max_parallel_workers == 0 {
            return Err(RoutingError::ZeroParallelWorkers);
        }
        context.assessment.validate()?;
        let required_profile = profile_for_quality(context.assessment.minimum_quality_tier);
        let mut scores: Vec<_> = context
            .candidates
            .iter()
            .map(|candidate| score_candidate(context, candidate, required_profile, config))
            .collect();
        scores.sort_by(compare_scores);

        let selected_provider = scores
            .iter()
            .find(|score| score.eligible)
            .map(|score| score.provider);
        let selected_profile = selected_provider.map(|_| required_profile);
        let reasoning_effort = selected_profile.map(effort_for_profile);
        let eligible_count = scores.iter().filter(|score| score.eligible).count();
        let parallel_workers = if selected_provider.is_none() {
            0
        } else if context.writable || context.conserve_budget {
            1
        } else {
            config.max_parallel_workers.min(eligible_count).max(1)
        };

        let (rationale, blocked_options) = if let Some(provider) = selected_provider {
            let selected = scores
                .iter()
                .find(|score| score.provider == provider)
                .ok_or(RoutingError::SelectedScoreMissing)?;
            (
                vec![format!(
                    "selected {} with score {:.2}; profile {:?} satisfies {:?} quality floor",
                    provider.as_str(),
                    selected.total,
                    required_profile,
                    context.assessment.minimum_quality_tier
                )],
                Vec::new(),
            )
        } else {
            (
                vec![
                    "no approved provider satisfies health, capability, budget, and quality gates"
                        .to_owned(),
                ],
                vec![
                    "wait for quota reset".to_owned(),
                    "use another approved provider".to_owned(),
                    "reduce task scope".to_owned(),
                    "ask an administrator for a usage override".to_owned(),
                    "explicitly approve a quality downgrade when policy permits".to_owned(),
                ],
            )
        };

        Ok(RoutingDecision {
            schema_version: orchestrator_domain::SchemaVersion::v1(),
            decision_id: orchestrator_domain::RoutingDecisionId::new(),
            task_id: context.task_id,
            selected_provider,
            selected_profile,
            reasoning_effort,
            parallel_workers,
            candidate_scores: scores,
            rationale,
            // This engine never crosses the assessed quality floor. A separately persisted
            // administrator/user approval policy may request a new assessment and route.
            downgrade: false,
            applied_policy: "enterprise-routing-v1".to_owned(),
            blocked_options,
            created_at: now,
        })
    }
}

fn score_candidate(
    context: &RoutingContext,
    candidate: &RoutingCandidate,
    required_profile: ModelProfile,
    config: &RoutingConfig,
) -> CandidateRoutingScore {
    let mut exclusions = Vec::new();
    if !candidate.enabled {
        exclusions.push("provider is disabled".to_owned());
    }
    if candidate.provider != candidate.capabilities.provider
        || candidate.provider != candidate.health.provider
    {
        exclusions
            .push("provider identity differs across capability or health evidence".to_owned());
    }
    if context
        .manually_requested_provider
        .is_some_and(|provider| provider != candidate.provider)
    {
        exclusions.push("another provider was manually requested".to_owned());
    }
    if candidate.health.status == HealthStatus::Unhealthy {
        exclusions.push("provider health is unhealthy".to_owned());
    }
    if !candidate.capabilities.non_interactive.usable()
        || !candidate.capabilities.structured_output.usable()
    {
        exclusions.push("safe non-interactive structured execution is unsupported".to_owned());
    }
    let execution_mode = if context.writable {
        candidate.capabilities.writable
    } else {
        candidate.capabilities.read_only
    };
    if !execution_mode.usable() {
        exclusions.push("required sandbox execution mode is unsupported".to_owned());
    }
    if context.role == TaskRole::IndependentReview && !execution_mode.verified() {
        exclusions.push("reviewer read-only mode has not been verified".to_owned());
    }
    if context.role == TaskRole::IndependentReview
        && context
            .implementation_provider
            .is_some_and(|provider| provider == candidate.provider)
    {
        exclusions.push("independent review must use a different provider".to_owned());
    }
    if !candidate.available_profiles.contains(&required_profile) {
        exclusions.push(format!(
            "required {required_profile:?} model profile is unavailable"
        ));
    }
    if candidate
        .budgets
        .iter()
        .any(|budget| budget.status == ForecastStatus::Exhausted)
    {
        exclusions.push("a required quota scope is exhausted".to_owned());
    }
    if candidate
        .budgets
        .iter()
        .any(|budget| budget.status == ForecastStatus::Red)
    {
        exclusions.push("a required quota scope is below the safe handover threshold".to_owned());
    }
    if !config.allow_amber
        && candidate
            .budgets
            .iter()
            .any(|budget| budget.status == ForecastStatus::Amber)
    {
        exclusions.push("amber quota is disabled by administrator policy".to_owned());
    }
    if context.assessment.difficulty == Difficulty::Critical
        && (candidate.budgets.is_empty()
            || candidate.budgets.iter().any(|budget| {
                budget.status == ForecastStatus::Unknown
                    || budget.confidence == UsageConfidence::Unknown
            }))
    {
        exclusions.push("critical work requires known provider headroom".to_owned());
    }
    if candidate
        .calibrated_remaining_work_units
        .is_some_and(|remaining| remaining < context.assessment.estimated_usage_units.max)
    {
        exclusions
            .push("estimated task usage exceeds calibrated safe remaining work units".to_owned());
    }
    if !score_inputs_valid(candidate) {
        exclusions.push("candidate score inputs are outside configured bounds".to_owned());
    }

    let components = score_components(context, candidate, execution_mode);
    let total = components.total();
    CandidateRoutingScore {
        provider: candidate.provider,
        eligible: exclusions.is_empty(),
        exclusions,
        components,
        total,
    }
}

fn score_inputs_valid(candidate: &RoutingCandidate) -> bool {
    candidate.recent_failure_rate.is_finite()
        && (0.0..=1.0).contains(&candidate.recent_failure_rate)
        && candidate.handover_cost.is_finite()
        && (0.0..=1.0).contains(&candidate.handover_cost)
        && (0..=100).contains(&candidate.admin_priority)
}

fn score_components(
    context: &RoutingContext,
    candidate: &RoutingCandidate,
    execution_mode: CapabilitySupport,
) -> ScoreComponents {
    let credit_headroom = candidate
        .budgets
        .iter()
        .filter_map(|budget| budget.safe_remaining_percent)
        .reduce(f64::min)
        .map_or(0.0, |remaining| remaining.clamp(0.0, 100.0) / 5.0);
    let confidence = candidate
        .budgets
        .iter()
        .map(|budget| budget.confidence.weight())
        .reduce(f64::min);
    let worst_status = candidate
        .budgets
        .iter()
        .map(|budget| budget.status)
        .max_by_key(|status| status_severity(*status));

    ScoreComponents {
        role_affinity: role_affinity(context.role, candidate.provider),
        capability: capability_score(
            candidate.capabilities.non_interactive,
            candidate.capabilities.structured_output,
            execution_mode,
        ),
        credit_headroom,
        provider_health: match candidate.health.status {
            HealthStatus::Healthy => 15.0,
            HealthStatus::Degraded => 7.5,
            HealthStatus::Unknown => 3.0,
            HealthStatus::Unhealthy => 0.0,
        },
        admin_priority: f64::from(candidate.admin_priority.clamp(0, 100)) / 10.0,
        usage_confidence: confidence.unwrap_or(0.0) * 10.0,
        exhaustion_penalty: match worst_status {
            Some(ForecastStatus::Amber) => 10.0,
            Some(ForecastStatus::Red | ForecastStatus::Exhausted) => 30.0,
            Some(ForecastStatus::Green | ForecastStatus::Unknown) | None => 0.0,
        },
        failure_penalty: candidate.recent_failure_rate.clamp(0.0, 1.0) * 15.0,
        handover_penalty: if context.current_provider == Some(candidate.provider) {
            0.0
        } else {
            candidate.handover_cost.clamp(0.0, 1.0) * 10.0
        },
        uncertainty_penalty: match confidence {
            None | Some(0.0) => 20.0,
            Some(value) if value < 1.0 => 8.0,
            Some(_) => 0.0,
        },
    }
}

fn capability_score(
    non_interactive: CapabilitySupport,
    structured: CapabilitySupport,
    execution_mode: CapabilitySupport,
) -> f64 {
    [non_interactive, structured, execution_mode]
        .into_iter()
        .map(|support| match support {
            CapabilitySupport::Verified => 1.0,
            CapabilitySupport::Advertised => 0.75,
            CapabilitySupport::Degraded => 0.5,
            CapabilitySupport::Unsupported => 0.0,
        })
        .sum::<f64>()
        / 3.0
        * 20.0
}

fn role_affinity(role: TaskRole, provider: ProviderId) -> f64 {
    match role {
        TaskRole::Implementation
        | TaskRole::Debugging
        | TaskRole::Testing
        | TaskRole::Refactoring => match provider {
            ProviderId::Codex => 25.0,
            ProviderId::Claude => 18.0,
            ProviderId::Gemini | ProviderId::Agy => 15.0,
        },
        TaskRole::Architecture => match provider {
            ProviderId::Claude => 25.0,
            ProviderId::Codex => 20.0,
            ProviderId::Gemini | ProviderId::Agy => 18.0,
        },
        TaskRole::SecurityReview => match provider {
            ProviderId::Claude => 25.0,
            ProviderId::Codex => 22.0,
            ProviderId::Gemini | ProviderId::Agy => 15.0,
        },
        TaskRole::IndependentReview => match provider {
            ProviderId::Claude => 25.0,
            ProviderId::Codex => 23.0,
            ProviderId::Gemini | ProviderId::Agy => 18.0,
        },
        TaskRole::RepositoryResearch => match provider {
            ProviderId::Gemini | ProviderId::Agy => 25.0,
            ProviderId::Codex => 20.0,
            ProviderId::Claude => 18.0,
        },
        TaskRole::Planning | TaskRole::Integration => match provider {
            ProviderId::Codex => 25.0,
            ProviderId::Claude => 20.0,
            ProviderId::Gemini | ProviderId::Agy => 18.0,
        },
    }
}

const fn status_severity(status: ForecastStatus) -> u8 {
    match status {
        ForecastStatus::Green => 0,
        ForecastStatus::Amber => 1,
        ForecastStatus::Unknown => 2,
        ForecastStatus::Red => 3,
        ForecastStatus::Exhausted => 4,
    }
}

fn compare_scores(left: &CandidateRoutingScore, right: &CandidateRoutingScore) -> Ordering {
    right
        .eligible
        .cmp(&left.eligible)
        .then_with(|| right.total.total_cmp(&left.total))
        .then_with(|| {
            right
                .components
                .admin_priority
                .total_cmp(&left.components.admin_priority)
        })
        .then_with(|| left.provider.as_str().cmp(right.provider.as_str()))
}

const fn profile_for_quality(quality: QualityTier) -> ModelProfile {
    match quality {
        QualityTier::Economy => ModelProfile::Economy,
        QualityTier::Standard => ModelProfile::Standard,
        QualityTier::Premium => ModelProfile::Premium,
    }
}

const fn effort_for_profile(profile: ModelProfile) -> ReasoningEffort {
    match profile {
        ModelProfile::Economy => ReasoningEffort::Low,
        ModelProfile::Standard => ReasoningEffort::Medium,
        ModelProfile::Premium => ReasoningEffort::High,
    }
}

#[derive(Debug, Error)]
pub enum RoutingError {
    #[error("maximum parallel workers must be at least one")]
    ZeroParallelWorkers,
    #[error("selected provider has no candidate score")]
    SelectedScoreMissing,
    #[error(transparent)]
    InvalidAssessment(#[from] orchestrator_domain::AssessmentError),
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use orchestrator_domain::{
        AssessmentScores, HealthStatus, QuotaPeriod, QuotaScope, RiskTag, UsageRange, UsageUnit,
    };

    use super::*;

    fn assessment(
        scores: AssessmentScores,
        risks: Vec<RiskTag>,
    ) -> Result<TaskAssessment, orchestrator_domain::AssessmentError> {
        TaskAssessment::from_scores(
            scores,
            risks,
            UsageRange::new(4, 8)?,
            UsageRange::new(4.0, 8.0)?,
            Vec::new(),
        )
    }

    fn candidate(provider: ProviderId, status: ForecastStatus) -> RoutingCandidate {
        let mut capabilities = ProviderCapabilities::unsupported(provider);
        capabilities.non_interactive = CapabilitySupport::Verified;
        capabilities.structured_output = CapabilitySupport::Verified;
        capabilities.writable = CapabilitySupport::Verified;
        capabilities.read_only = CapabilitySupport::Verified;
        RoutingCandidate {
            provider,
            enabled: true,
            capabilities,
            health: ProviderHealth {
                provider,
                status: HealthStatus::Healthy,
                checked_at: Utc::now(),
                latency_ms: Some(10),
                consecutive_failures: 0,
                detail: None,
            },
            budgets: vec![BudgetForecast {
                provider,
                quota_scope: QuotaScope::new(
                    "primary",
                    QuotaPeriod::CalendarMonth,
                    UsageUnit::Credits,
                ),
                confidence: if status == ForecastStatus::Unknown {
                    UsageConfidence::Unknown
                } else {
                    UsageConfidence::Confirmed
                },
                status,
                period_progress: 0.5,
                usage_progress: Some(0.25),
                burn_index: Some(0.5),
                projected_end_usage_percent: Some(50.0),
                safe_remaining_percent: Some(60.0),
                remaining_units: Some(60.0),
                smoothed_usage_progress: Some(0.25),
                observation_count: 3,
                projection_mature: true,
                resets_at: Utc::now(),
                rationale: Vec::new(),
            }],
            available_profiles: vec![
                ModelProfile::Economy,
                ModelProfile::Standard,
                ModelProfile::Premium,
            ],
            calibrated_remaining_work_units: Some(100.0),
            recent_failure_rate: 0.0,
            admin_priority: 50,
            handover_cost: 0.0,
        }
    }

    #[test]
    fn implementation_prefers_codex_when_other_inputs_are_equal()
    -> Result<(), Box<dyn std::error::Error>> {
        let context = RoutingContext {
            task_id: TaskId::new(),
            assessment: assessment(AssessmentScores::new(1, 1, 1, 1, 1)?, Vec::new())?,
            role: TaskRole::Implementation,
            writable: true,
            candidates: vec![
                candidate(ProviderId::Gemini, ForecastStatus::Green),
                candidate(ProviderId::Claude, ForecastStatus::Green),
                candidate(ProviderId::Codex, ForecastStatus::Green),
            ],
            current_provider: None,
            implementation_provider: None,
            manually_requested_provider: None,
            conserve_budget: false,
        };
        let decision = RoutingEngine::route(&context, &RoutingConfig::default(), Utc::now())?;
        assert_eq!(decision.selected_provider, Some(ProviderId::Codex));
        assert_eq!(decision.selected_profile, Some(ModelProfile::Standard));
        Ok(())
    }

    #[test]
    fn quality_tiers_select_matching_profiles_and_efforts() {
        for (quality, profile, effort) in [
            (
                QualityTier::Economy,
                ModelProfile::Economy,
                ReasoningEffort::Low,
            ),
            (
                QualityTier::Standard,
                ModelProfile::Standard,
                ReasoningEffort::Medium,
            ),
            (
                QualityTier::Premium,
                ModelProfile::Premium,
                ReasoningEffort::High,
            ),
        ] {
            assert_eq!(profile_for_quality(quality), profile);
            assert_eq!(effort_for_profile(profile), effort);
        }
    }

    #[test]
    fn critical_work_with_unknown_budget_is_blocked() -> Result<(), Box<dyn std::error::Error>> {
        let context = RoutingContext {
            task_id: TaskId::new(),
            assessment: assessment(AssessmentScores::new(2, 2, 2, 2, 2)?, Vec::new())?,
            role: TaskRole::Implementation,
            writable: true,
            candidates: vec![candidate(ProviderId::Codex, ForecastStatus::Unknown)],
            current_provider: None,
            implementation_provider: None,
            manually_requested_provider: None,
            conserve_budget: false,
        };
        let decision = RoutingEngine::route(&context, &RoutingConfig::default(), Utc::now())?;
        assert_eq!(decision.selected_provider, None);
        assert!(!decision.blocked_options.is_empty());
        assert!(!decision.downgrade);
        Ok(())
    }

    #[test]
    fn quality_floor_cannot_select_lower_profile() -> Result<(), Box<dyn std::error::Error>> {
        let mut codex = candidate(ProviderId::Codex, ForecastStatus::Green);
        codex.available_profiles = vec![ModelProfile::Economy, ModelProfile::Standard];
        let context = RoutingContext {
            task_id: TaskId::new(),
            assessment: assessment(
                AssessmentScores::new(1, 1, 1, 1, 1)?,
                vec![RiskTag::Security],
            )?,
            role: TaskRole::Implementation,
            writable: true,
            candidates: vec![codex],
            current_provider: None,
            implementation_provider: None,
            manually_requested_provider: None,
            conserve_budget: false,
        };
        let decision = RoutingEngine::route(&context, &RoutingConfig::default(), Utc::now())?;
        assert_eq!(decision.selected_provider, None);
        assert!(!decision.downgrade);
        Ok(())
    }

    #[test]
    fn disabled_provider_is_never_selected() -> Result<(), Box<dyn std::error::Error>> {
        let mut codex = candidate(ProviderId::Codex, ForecastStatus::Green);
        codex.enabled = false;
        let context = RoutingContext {
            task_id: TaskId::new(),
            assessment: assessment(AssessmentScores::new(0, 0, 0, 0, 0)?, Vec::new())?,
            role: TaskRole::Implementation,
            writable: true,
            candidates: vec![codex],
            current_provider: None,
            implementation_provider: None,
            manually_requested_provider: None,
            conserve_budget: false,
        };
        let decision = RoutingEngine::route(&context, &RoutingConfig::default(), Utc::now())?;
        assert_eq!(decision.selected_provider, None);
        Ok(())
    }
}
