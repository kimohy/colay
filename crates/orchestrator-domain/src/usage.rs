use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::SchemaVersion;

pub const USAGE_SNAPSHOT_SCHEMA_VERSION: &str = SchemaVersion::V1;
pub const SUPPORTED_USAGE_SNAPSHOT_SCHEMA_VERSIONS: &[&str] = &[USAGE_SNAPSHOT_SCHEMA_VERSION];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    Gemini,
    Agy,
    Codex,
    Claude,
}

impl ProviderId {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::Agy => "agy",
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProviderId {
    type Err = ProviderIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "gemini" => Ok(Self::Gemini),
            "agy" => Ok(Self::Agy),
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            _ => Err(ProviderIdParseError(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("unknown provider `{0}`")]
pub struct ProviderIdParseError(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaPeriod {
    CalendarDay,
    RollingDay,
    CalendarMonth,
    RollingMonth,
    Custom,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "name")]
pub enum UsageUnit {
    Tokens,
    Requests,
    Credits,
    WorkUnits,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QuotaScope {
    /// Stable administrator/provider name such as `primary_monthly`.
    pub name: String,
    pub period: QuotaPeriod,
    pub unit: UsageUnit,
}

impl QuotaScope {
    #[must_use]
    pub fn new(name: impl Into<String>, period: QuotaPeriod, unit: UsageUnit) -> Self {
        Self {
            name: name.into(),
            period,
            unit,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    OfficialCli,
    OfficialProtocol,
    ConfiguredProbe,
    LocalLedger,
    ManualOverride,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageConfidence {
    Confirmed,
    Estimated,
    Unknown,
}

impl UsageConfidence {
    #[must_use]
    pub const fn weight(self) -> f64 {
        match self {
            Self::Confirmed => 1.0,
            Self::Estimated => 0.6,
            Self::Unknown => 0.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    #[serde(deserialize_with = "crate::schema::deserialize_v1_schema_version")]
    pub schema_version: SchemaVersion,
    pub provider: ProviderId,
    pub quota_scope: QuotaScope,
    pub quota_period: QuotaPeriod,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub period_started_at: Option<DateTime<Utc>>,
    pub resets_at: Option<DateTime<Utc>>,
    pub source: UsageSource,
    pub confidence: UsageConfidence,
    pub collected_at: DateTime<Utc>,
}

impl UsageSnapshot {
    #[must_use]
    pub fn has_supported_schema(&self) -> bool {
        self.schema_version
            .is_supported_by(SUPPORTED_USAGE_SNAPSHOT_SCHEMA_VERSIONS)
    }

    #[must_use]
    pub fn unknown(
        provider: ProviderId,
        quota_scope: QuotaScope,
        collected_at: DateTime<Utc>,
    ) -> Self {
        let quota_period = quota_scope.period;
        Self {
            schema_version: SchemaVersion::v1(),
            provider,
            quota_scope,
            quota_period,
            used: None,
            limit: None,
            remaining: None,
            used_percent: None,
            remaining_percent: None,
            period_started_at: None,
            resets_at: None,
            source: UsageSource::Unknown,
            confidence: UsageConfidence::Unknown,
            collected_at,
        }
    }

    /// Revalidates a normalized snapshot without deriving missing values.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] for inconsistent periods, invalid numbers, or known values
    /// mislabeled with unknown confidence.
    pub fn validate(&self) -> Result<(), UsageError> {
        if !self.has_supported_schema() {
            return Err(UsageError::UnsupportedSchema(
                self.schema_version.to_string(),
            ));
        }
        if self.quota_scope.period != self.quota_period {
            return Err(UsageError::ScopePeriodMismatch);
        }
        for (name, value) in [
            ("used", self.used),
            ("limit", self.limit),
            ("remaining", self.remaining),
        ] {
            if let Some(value) = value
                && (!value.is_finite() || value < 0.0)
            {
                return Err(UsageError::InvalidValue(name));
            }
        }
        if let Some(limit) = self.limit {
            if self.used.is_some_and(|used| used > limit)
                || self.remaining.is_some_and(|remaining| remaining > limit)
            {
                return Err(UsageError::InconsistentValues);
            }
            let unit_tolerance = (limit.abs() * 1.0e-6).max(1.0e-9);
            if let (Some(used), Some(remaining)) = (self.used, self.remaining)
                && (used + remaining - limit).abs() > unit_tolerance
            {
                return Err(UsageError::InconsistentValues);
            }
            if limit > 0.0 {
                if let (Some(used), Some(percent)) = (self.used, self.used_percent)
                    && (used / limit * 100.0 - percent).abs() > 1.0e-4
                {
                    return Err(UsageError::InconsistentValues);
                }
                if let (Some(remaining), Some(percent)) = (self.remaining, self.remaining_percent)
                    && (remaining / limit * 100.0 - percent).abs() > 1.0e-4
                {
                    return Err(UsageError::InconsistentValues);
                }
            }
        }
        if let (Some(used), Some(remaining)) = (self.used_percent, self.remaining_percent)
            && (used + remaining - 100.0).abs() > 1.0e-4
        {
            return Err(UsageError::InconsistentValues);
        }
        for (name, value) in [
            ("used_percent", self.used_percent),
            ("remaining_percent", self.remaining_percent),
        ] {
            if let Some(value) = value
                && (!value.is_finite() || !(0.0..=100.0).contains(&value))
            {
                return Err(UsageError::InvalidPercent(name));
            }
        }
        if let (Some(start), Some(reset)) = (self.period_started_at, self.resets_at)
            && reset <= start
        {
            return Err(UsageError::InvalidPeriod);
        }
        if self.confidence == UsageConfidence::Unknown
            && (self.used.is_some()
                || self.remaining.is_some()
                || self.used_percent.is_some()
                || self.remaining_percent.is_some())
        {
            return Err(UsageError::UnknownWithKnownValues);
        }
        Ok(())
    }

    #[must_use]
    pub fn effective_used_percent(&self) -> Option<f64> {
        self.used_percent.or_else(|| {
            let used = self.used?;
            let limit = self.limit?;
            (limit > 0.0).then_some((used / limit * 100.0).clamp(0.0, 100.0))
        })
    }

    #[must_use]
    pub fn effective_remaining_percent(&self) -> Option<f64> {
        self.remaining_percent.or_else(|| {
            if let (Some(remaining), Some(limit)) = (self.remaining, self.limit) {
                return (limit > 0.0).then_some((remaining / limit * 100.0).clamp(0.0, 100.0));
            }
            self.effective_used_percent().map(|used| 100.0 - used)
        })
    }

    #[must_use]
    pub fn is_confirmed_exhausted(&self) -> bool {
        self.confidence == UsageConfidence::Confirmed
            && (self.remaining.is_some_and(|value| value <= 0.0)
                || self
                    .effective_remaining_percent()
                    .is_some_and(|value| value <= 0.0))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageObservation {
    pub provider: ProviderId,
    pub quota_scope: QuotaScope,
    pub amount: f64,
    pub observed_at: DateTime<Utc>,
    pub source: UsageSource,
    pub confidence: UsageConfidence,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum UsageError {
    #[error("unsupported usage snapshot schema version {0}")]
    UnsupportedSchema(String),
    #[error("quota scope period differs from snapshot period")]
    ScopePeriodMismatch,
    #[error("usage field {0} must be finite and non-negative")]
    InvalidValue(&'static str),
    #[error("usage percent field {0} must be within 0..=100")]
    InvalidPercent(&'static str),
    #[error("quota reset must be later than period start")]
    InvalidPeriod,
    #[error("known usage values contradict the quota limit or each other")]
    InconsistentValues,
    #[error("unknown-confidence usage cannot contain known usage values")]
    UnknownWithKnownValues,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn agy_provider_identity_is_stable() -> Result<(), serde_json::Error> {
        assert_eq!(ProviderId::from_str("agy"), Ok(ProviderId::Agy));
        assert_eq!(ProviderId::Agy.as_str(), "agy");
        assert_eq!(serde_json::to_string(&ProviderId::Agy)?, "\"agy\"");
        Ok(())
    }

    #[test]
    fn unknown_never_invents_values() {
        let snapshot = UsageSnapshot::unknown(
            ProviderId::Codex,
            QuotaScope::new("monthly", QuotaPeriod::CalendarMonth, UsageUnit::Credits),
            Utc::now(),
        );
        assert!(snapshot.effective_remaining_percent().is_none());
        assert_eq!(snapshot.validate(), Ok(()));
    }

    #[test]
    fn derives_remaining_percent_only_with_compatible_values() {
        let mut snapshot = UsageSnapshot::unknown(
            ProviderId::Gemini,
            QuotaScope::new("daily", QuotaPeriod::CalendarDay, UsageUnit::Requests),
            Utc::now(),
        );
        snapshot.source = UsageSource::OfficialCli;
        snapshot.confidence = UsageConfidence::Confirmed;
        snapshot.used = Some(25.0);
        snapshot.limit = Some(100.0);
        assert_eq!(snapshot.effective_remaining_percent(), Some(75.0));
    }

    #[test]
    fn rejects_mathematically_inconsistent_snapshot() {
        let mut snapshot = UsageSnapshot::unknown(
            ProviderId::Claude,
            QuotaScope::new("monthly", QuotaPeriod::CalendarMonth, UsageUnit::Credits),
            Utc::now(),
        );
        snapshot.source = UsageSource::ConfiguredProbe;
        snapshot.confidence = UsageConfidence::Confirmed;
        snapshot.used = Some(10.0);
        snapshot.remaining = Some(90.0);
        snapshot.limit = Some(50.0);
        assert_eq!(snapshot.validate(), Err(UsageError::InconsistentValues));
    }
}
