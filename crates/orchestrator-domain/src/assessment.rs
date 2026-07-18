use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Difficulty {
    Trivial,
    Simple,
    Moderate,
    Complex,
    Critical,
}

impl Difficulty {
    #[must_use]
    pub const fn from_score(score: u8) -> Option<Self> {
        match score {
            0..=2 => Some(Self::Trivial),
            3..=4 => Some(Self::Simple),
            5..=6 => Some(Self::Moderate),
            7..=8 => Some(Self::Complex),
            9..=10 => Some(Self::Critical),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AssessmentScores {
    pub scope: u8,
    pub ambiguity: u8,
    pub technical_complexity: u8,
    pub failure_impact: u8,
    pub verification_complexity: u8,
}

impl AssessmentScores {
    /// Builds a score vector.
    ///
    /// # Errors
    ///
    /// Returns [`AssessmentError::ScoreOutOfRange`] when any component exceeds two.
    pub fn new(
        scope: u8,
        ambiguity: u8,
        technical_complexity: u8,
        failure_impact: u8,
        verification_complexity: u8,
    ) -> Result<Self, AssessmentError> {
        let scores = Self {
            scope,
            ambiguity,
            technical_complexity,
            failure_impact,
            verification_complexity,
        };
        scores.validate()?;
        Ok(scores)
    }

    /// # Errors
    ///
    /// Returns [`AssessmentError::ScoreOutOfRange`] when any component exceeds two.
    pub fn validate(&self) -> Result<(), AssessmentError> {
        for (name, value) in [
            ("scope", self.scope),
            ("ambiguity", self.ambiguity),
            ("technical_complexity", self.technical_complexity),
            ("failure_impact", self.failure_impact),
            ("verification_complexity", self.verification_complexity),
        ] {
            if value > 2 {
                return Err(AssessmentError::ScoreOutOfRange { name, value });
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn total(self) -> u8 {
        self.scope
            + self.ambiguity
            + self.technical_complexity
            + self.failure_impact
            + self.verification_complexity
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskTag {
    Security,
    Authentication,
    Production,
    Infrastructure,
    DestructiveMigration,
    DataLoss,
    Billing,
    Privacy,
    Compliance,
    Concurrency,
    LargeRepository,
    ExternalIntegration,
}

impl RiskTag {
    #[must_use]
    pub const fn forces_premium(&self) -> bool {
        matches!(
            self,
            Self::Security
                | Self::Authentication
                | Self::Production
                | Self::Infrastructure
                | Self::DestructiveMigration
                | Self::DataLoss
                | Self::Billing
                | Self::Privacy
                | Self::Compliance
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityTier {
    Economy,
    Standard,
    Premium,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageRange<T> {
    pub min: T,
    pub max: T,
}

impl<T: PartialOrd + Copy> UsageRange<T> {
    /// # Errors
    ///
    /// Returns [`AssessmentError::InvalidRange`] when `min` exceeds `max`.
    pub fn new(min: T, max: T) -> Result<Self, AssessmentError> {
        if min > max {
            return Err(AssessmentError::InvalidRange);
        }
        Ok(Self { min, max })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskAssessment {
    pub total_score: u8,
    pub difficulty: Difficulty,
    pub risk_tags: Vec<RiskTag>,
    pub estimated_turns: UsageRange<u32>,
    pub estimated_usage_units: UsageRange<f64>,
    pub requires_independent_review: bool,
    pub minimum_quality_tier: QualityTier,
    pub rationale: Vec<String>,
}

impl TaskAssessment {
    /// Creates an assessment while deriving difficulty, review, and quality invariants.
    ///
    /// # Errors
    ///
    /// Returns an [`AssessmentError`] for invalid scores or estimate ranges.
    pub fn from_scores(
        scores: AssessmentScores,
        mut risk_tags: Vec<RiskTag>,
        estimated_turns: UsageRange<u32>,
        estimated_usage_units: UsageRange<f64>,
        rationale: Vec<String>,
    ) -> Result<Self, AssessmentError> {
        scores.validate()?;
        if !estimated_usage_units.min.is_finite()
            || !estimated_usage_units.max.is_finite()
            || estimated_usage_units.min < 0.0
            || estimated_usage_units.max < estimated_usage_units.min
        {
            return Err(AssessmentError::InvalidRange);
        }

        risk_tags.sort_unstable();
        risk_tags.dedup();
        let total_score = scores.total();
        let difficulty = Difficulty::from_score(total_score)
            .ok_or(AssessmentError::TotalOutOfRange(total_score))?;
        let minimum_quality_tier = if difficulty == Difficulty::Critical
            || risk_tags.iter().any(RiskTag::forces_premium)
        {
            QualityTier::Premium
        } else if matches!(difficulty, Difficulty::Moderate | Difficulty::Complex) {
            QualityTier::Standard
        } else {
            QualityTier::Economy
        };

        Ok(Self {
            total_score,
            difficulty,
            risk_tags,
            estimated_turns,
            estimated_usage_units,
            requires_independent_review: total_score >= 7,
            minimum_quality_tier,
            rationale,
        })
    }

    /// Revalidates a deserialized assessment against all quality and review invariants.
    ///
    /// # Errors
    ///
    /// Returns an [`AssessmentError`] when a persisted invariant is violated.
    pub fn validate(&self) -> Result<(), AssessmentError> {
        let expected = Difficulty::from_score(self.total_score)
            .ok_or(AssessmentError::TotalOutOfRange(self.total_score))?;
        if expected != self.difficulty {
            return Err(AssessmentError::DifficultyMismatch);
        }
        if self.estimated_turns.min > self.estimated_turns.max
            || !self.estimated_usage_units.min.is_finite()
            || !self.estimated_usage_units.max.is_finite()
            || self.estimated_usage_units.min < 0.0
            || self.estimated_usage_units.min > self.estimated_usage_units.max
        {
            return Err(AssessmentError::InvalidRange);
        }
        let expected_quality_floor = if self.difficulty == Difficulty::Critical
            || self.risk_tags.iter().any(RiskTag::forces_premium)
        {
            QualityTier::Premium
        } else if matches!(self.difficulty, Difficulty::Moderate | Difficulty::Complex) {
            QualityTier::Standard
        } else {
            QualityTier::Economy
        };
        if self.minimum_quality_tier < expected_quality_floor {
            return Err(AssessmentError::QualityFloorViolation);
        }
        if self.total_score >= 7 && !self.requires_independent_review {
            return Err(AssessmentError::ReviewRequirementViolation);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AssessmentError {
    #[error("assessment score {name}={value} is outside 0..=2")]
    ScoreOutOfRange { name: &'static str, value: u8 },
    #[error("total assessment score {0} is outside 0..=10")]
    TotalOutOfRange(u8),
    #[error("difficulty does not match total score")]
    DifficultyMismatch,
    #[error("usage range is invalid")]
    InvalidRange,
    #[error("minimum quality tier violates a critical or risk-tag floor")]
    QualityFloorViolation,
    #[error("complex and critical work requires independent review")]
    ReviewRequirementViolation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difficulty_boundaries_match_contract() {
        let expected = [
            Difficulty::Trivial,
            Difficulty::Trivial,
            Difficulty::Trivial,
            Difficulty::Simple,
            Difficulty::Simple,
            Difficulty::Moderate,
            Difficulty::Moderate,
            Difficulty::Complex,
            Difficulty::Complex,
            Difficulty::Critical,
            Difficulty::Critical,
        ];
        for (score, expected_difficulty) in (0_u8..=10).zip(expected) {
            assert_eq!(Difficulty::from_score(score), Some(expected_difficulty));
        }
    }

    #[test]
    fn sensitive_risk_forces_premium() -> Result<(), AssessmentError> {
        let assessment = TaskAssessment::from_scores(
            AssessmentScores::new(1, 1, 1, 1, 1)?,
            vec![RiskTag::Security],
            UsageRange::new(4, 8)?,
            UsageRange::new(4.0, 8.0)?,
            vec!["security-sensitive change".to_owned()],
        )?;
        assert_eq!(assessment.minimum_quality_tier, QualityTier::Premium);
        Ok(())
    }

    #[test]
    fn score_seven_requires_review() -> Result<(), AssessmentError> {
        let assessment = TaskAssessment::from_scores(
            AssessmentScores::new(2, 1, 2, 1, 1)?,
            Vec::new(),
            UsageRange::new(8, 16)?,
            UsageRange::new(8.0, 16.0)?,
            Vec::new(),
        )?;
        assert!(assessment.requires_independent_review);
        assert_eq!(assessment.difficulty, Difficulty::Complex);
        Ok(())
    }
}
