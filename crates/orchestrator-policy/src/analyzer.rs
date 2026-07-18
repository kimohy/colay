use std::collections::BTreeSet;

use orchestrator_domain::{AssessmentError, AssessmentScores, RiskTag, TaskAssessment, UsageRange};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// Independent hints keep the deterministic rubric auditable in configuration and tests.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisHints {
    pub estimated_files: Option<u32>,
    pub estimated_components: Option<u32>,
    pub repository_wide: bool,
    pub cross_component: bool,
    pub unclear_requirements: u32,
    pub advanced_technical_concerns: u32,
    pub production_impact: bool,
    pub rollback_difficult: bool,
    pub verification_layers: u32,
    pub needs_e2e: bool,
    pub lacks_clear_oracle: bool,
    pub risk_tags: Vec<RiskTag>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAnalysisInput {
    pub objective: String,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub hints: AnalysisHints,
}

#[derive(Clone, Debug, Default)]
pub struct TaskAnalyzer;

impl TaskAnalyzer {
    /// Applies the documented five-axis rubric and risk-tag quality floors.
    ///
    /// # Errors
    ///
    /// Returns [`AnalyzerError`] when the objective is empty or derived values violate
    /// domain assessment invariants.
    pub fn assess(input: &TaskAnalysisInput) -> Result<TaskAssessment, AnalyzerError> {
        if input.objective.trim().is_empty() {
            return Err(AnalyzerError::MissingObjective);
        }

        let scope = score_scope(&input.hints);
        let ambiguity = score_ambiguity(input);
        let technical_complexity = score_technical(&input.hints);
        let mut risk_tags: BTreeSet<_> = input.hints.risk_tags.iter().cloned().collect();
        detect_risks(input, &mut risk_tags);
        let failure_impact = score_failure(&input.hints, &risk_tags);
        let verification_complexity = score_verification(input);
        let scores = AssessmentScores::new(
            scope,
            ambiguity,
            technical_complexity,
            failure_impact,
            verification_complexity,
        )?;

        let (turns, usage) = estimates_for_total(scores.total());
        let rationale = vec![
            format!("scope={scope}: {}", scope_rationale(&input.hints)),
            format!(
                "ambiguity={ambiguity}: {} unclear item(s), {} acceptance criterion/criteria",
                input.hints.unclear_requirements,
                input.acceptance_criteria.len()
            ),
            format!(
                "technical_complexity={technical_complexity}: {} advanced concern(s)",
                input.hints.advanced_technical_concerns
            ),
            format!(
                "failure_impact={failure_impact}: production={}, difficult_rollback={}",
                input.hints.production_impact, input.hints.rollback_difficult
            ),
            format!(
                "verification_complexity={verification_complexity}: {} layer(s), e2e={}, clear_oracle={}",
                input.hints.verification_layers,
                input.hints.needs_e2e,
                !input.hints.lacks_clear_oracle
            ),
        ];

        TaskAssessment::from_scores(
            scores,
            risk_tags.into_iter().collect(),
            turns,
            usage,
            rationale,
        )
        .map_err(AnalyzerError::from)
    }
}

fn score_scope(hints: &AnalysisHints) -> u8 {
    if hints.repository_wide
        || hints.estimated_files.is_some_and(|files| files > 10)
        || hints
            .estimated_components
            .is_some_and(|components| components > 3)
    {
        2
    } else {
        u8::from(
            hints.cross_component
                || hints.estimated_files.is_some_and(|files| files > 1)
                || hints
                    .estimated_components
                    .is_some_and(|components| components > 1),
        )
    }
}

fn score_ambiguity(input: &TaskAnalysisInput) -> u8 {
    if input.acceptance_criteria.is_empty() || input.hints.unclear_requirements >= 3 {
        2
    } else {
        u8::from(input.hints.unclear_requirements > 0)
    }
}

fn score_technical(hints: &AnalysisHints) -> u8 {
    if hints.advanced_technical_concerns >= 2 {
        2
    } else {
        u8::from(hints.advanced_technical_concerns == 1 || hints.cross_component)
    }
}

fn score_failure(hints: &AnalysisHints, risks: &BTreeSet<RiskTag>) -> u8 {
    if hints.production_impact
        || hints.rollback_difficult
        || risks.iter().any(RiskTag::forces_premium)
    {
        2
    } else {
        u8::from(hints.cross_component || !risks.is_empty())
    }
}

fn score_verification(input: &TaskAnalysisInput) -> u8 {
    if input.hints.needs_e2e
        || input.hints.lacks_clear_oracle
        || input.hints.verification_layers >= 3
    {
        2
    } else {
        u8::from(input.hints.verification_layers >= 2 || input.acceptance_criteria.len() > 1)
    }
}

fn estimates_for_total(total: u8) -> (UsageRange<u32>, UsageRange<f64>) {
    let (min, max) = match total {
        0..=2 => (1, 2),
        3..=4 => (2, 4),
        5..=6 => (4, 8),
        7..=8 => (8, 16),
        _ => (12, 24),
    };
    (
        UsageRange { min, max },
        UsageRange {
            min: f64::from(min),
            max: f64::from(max),
        },
    )
}

fn scope_rationale(hints: &AnalysisHints) -> String {
    format!(
        "files={}, components={}, repository_wide={}, cross_component={}",
        hints
            .estimated_files
            .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
        hints
            .estimated_components
            .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
        hints.repository_wide,
        hints.cross_component
    )
}

fn detect_risks(input: &TaskAnalysisInput, risks: &mut BTreeSet<RiskTag>) {
    let text = std::iter::once(input.objective.as_str())
        .chain(input.constraints.iter().map(String::as_str))
        .chain(input.acceptance_criteria.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mappings: &[(&[&str], RiskTag)] = &[
        (
            &["security", "vulnerability", "암호", "보안"],
            RiskTag::Security,
        ),
        (
            &["authentication", "authorization", "oauth", "로그인", "인증"],
            RiskTag::Authentication,
        ),
        (
            &["production", "프로덕션", "운영 환경"],
            RiskTag::Production,
        ),
        (
            &["infrastructure", "terraform", "kubernetes", "인프라"],
            RiskTag::Infrastructure,
        ),
        (
            &["destructive migration", "drop table", "파괴적 마이그레이션"],
            RiskTag::DestructiveMigration,
        ),
        (&["data loss", "데이터 손실"], RiskTag::DataLoss),
        (&["billing", "payment", "결제", "청구"], RiskTag::Billing),
        (&["privacy", "pii", "개인정보"], RiskTag::Privacy),
        (&["compliance", "규정 준수", "감사"], RiskTag::Compliance),
    ];
    for (needles, risk) in mappings {
        if needles.iter().any(|needle| text.contains(needle)) {
            risks.insert(risk.clone());
        }
    }
}

#[derive(Debug, Error)]
pub enum AnalyzerError {
    #[error("task objective must not be empty")]
    MissingObjective,
    #[error(transparent)]
    InvalidAssessment(#[from] AssessmentError),
}

#[cfg(test)]
mod tests {
    use orchestrator_domain::{Difficulty, QualityTier, RiskTag};

    use super::*;

    #[test]
    fn repository_wide_security_work_is_complex_and_premium() -> Result<(), AnalyzerError> {
        let assessment = TaskAnalyzer::assess(&TaskAnalysisInput {
            objective: "replace production authentication flow".to_owned(),
            constraints: vec!["prevent data loss".to_owned()],
            acceptance_criteria: vec!["integration tests pass".to_owned()],
            hints: AnalysisHints {
                estimated_files: Some(20),
                estimated_components: Some(4),
                repository_wide: true,
                advanced_technical_concerns: 2,
                production_impact: true,
                verification_layers: 3,
                needs_e2e: true,
                ..AnalysisHints::default()
            },
        })?;
        assert_eq!(assessment.difficulty, Difficulty::Complex);
        assert_eq!(assessment.minimum_quality_tier, QualityTier::Premium);
        assert!(assessment.risk_tags.contains(&RiskTag::Authentication));
        assert!(assessment.requires_independent_review);
        Ok(())
    }

    #[test]
    fn missing_acceptance_criteria_scores_high_ambiguity() -> Result<(), AnalyzerError> {
        let assessment = TaskAnalyzer::assess(&TaskAnalysisInput {
            objective: "rename a local variable".to_owned(),
            constraints: Vec::new(),
            acceptance_criteria: Vec::new(),
            hints: AnalysisHints::default(),
        })?;
        assert!(
            assessment
                .rationale
                .iter()
                .any(|line| line.starts_with("ambiguity=2"))
        );
        Ok(())
    }
}
