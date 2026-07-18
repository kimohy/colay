use chrono::{DateTime, Utc};
use orchestrator_domain::{ProviderId, QuotaPeriod, QuotaScope, UsageConfidence, UsageSnapshot};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::PeriodWindow;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ForecastConfig {
    pub minimum_progress: f64,
    pub grace_window_daily_seconds: i64,
    pub grace_window_monthly_seconds: i64,
    pub ewma_alpha: f64,
    pub minimum_observations: usize,
    pub reserve_percent: f64,
    pub amber_safe_remaining_percent: f64,
    pub green_safe_remaining_percent: f64,
    pub hard_limit_remaining_percent: f64,
    pub projected_limit_percent: f64,
}

impl Default for ForecastConfig {
    fn default() -> Self {
        Self {
            minimum_progress: 0.05,
            grace_window_daily_seconds: 60 * 60,
            grace_window_monthly_seconds: 24 * 60 * 60,
            ewma_alpha: 0.3,
            minimum_observations: 3,
            reserve_percent: 15.0,
            amber_safe_remaining_percent: 20.0,
            green_safe_remaining_percent: 50.0,
            hard_limit_remaining_percent: 5.0,
            projected_limit_percent: 100.0,
        }
    }
}

impl ForecastConfig {
    /// # Errors
    ///
    /// Returns [`ForecastError::InvalidConfig`] when a smoothing, maturity, or threshold
    /// value is outside its supported range.
    pub fn validate(&self) -> Result<(), ForecastError> {
        if !(0.0..=1.0).contains(&self.minimum_progress) || self.minimum_progress == 0.0 {
            return Err(ForecastError::InvalidConfig("minimum_progress"));
        }
        if !(0.0..=1.0).contains(&self.ewma_alpha) || self.ewma_alpha == 0.0 {
            return Err(ForecastError::InvalidConfig("ewma_alpha"));
        }
        if self.minimum_observations == 0
            || self.grace_window_daily_seconds < 0
            || self.grace_window_monthly_seconds < 0
        {
            return Err(ForecastError::InvalidConfig("observation maturity"));
        }
        for (name, value) in [
            ("reserve_percent", self.reserve_percent),
            (
                "amber_safe_remaining_percent",
                self.amber_safe_remaining_percent,
            ),
            (
                "green_safe_remaining_percent",
                self.green_safe_remaining_percent,
            ),
            (
                "hard_limit_remaining_percent",
                self.hard_limit_remaining_percent,
            ),
            ("projected_limit_percent", self.projected_limit_percent),
        ] {
            if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                return Err(ForecastError::InvalidConfig(name));
            }
        }
        if self.hard_limit_remaining_percent > self.amber_safe_remaining_percent
            || self.amber_safe_remaining_percent > self.green_safe_remaining_percent
        {
            return Err(ForecastError::InvalidConfig("status thresholds"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastStatus {
    Green,
    Amber,
    Red,
    Exhausted,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetForecast {
    pub provider: ProviderId,
    pub quota_scope: QuotaScope,
    pub confidence: UsageConfidence,
    pub status: ForecastStatus,
    pub period_progress: f64,
    pub usage_progress: Option<f64>,
    pub burn_index: Option<f64>,
    pub projected_end_usage_percent: Option<f64>,
    pub safe_remaining_percent: Option<f64>,
    pub remaining_units: Option<f64>,
    pub smoothed_usage_progress: Option<f64>,
    pub observation_count: usize,
    pub projection_mature: bool,
    pub resets_at: DateTime<Utc>,
    pub rationale: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct BudgetForecaster;

impl BudgetForecaster {
    /// Normalizes period progress, applies confidence-weighted EWMA, and classifies headroom.
    ///
    /// # Errors
    ///
    /// Returns [`ForecastError`] when the configuration, usage snapshot, or period window is
    /// invalid.
    pub fn forecast(
        snapshot: &UsageSnapshot,
        history: &[UsageSnapshot],
        window: &PeriodWindow,
        now: DateTime<Utc>,
        quota_exceeded_error: bool,
        config: &ForecastConfig,
    ) -> Result<BudgetForecast, ForecastError> {
        config.validate()?;
        snapshot.validate()?;
        if window.resets_at <= window.started_at {
            return Err(ForecastError::InvalidWindow);
        }

        let period_progress = window.progress_at(now);
        let used_percent = effective_used_percent(snapshot);
        let remaining_percent = snapshot.effective_remaining_percent();
        let (smoothed_usage_progress, observation_count) =
            smoothed_progress(snapshot, history, config.ewma_alpha);
        let usage_progress = used_percent.map(|value| value / 100.0);
        let denominator = period_progress.max(config.minimum_progress);
        let burn_index = smoothed_usage_progress.map(|progress| progress / denominator);
        let projected_end_usage_percent = burn_index.map(|index| index * 100.0);
        let safe_remaining_percent =
            remaining_percent.map(|remaining| remaining - config.reserve_percent);
        let grace_seconds = match snapshot.quota_period {
            QuotaPeriod::CalendarDay | QuotaPeriod::RollingDay => config.grace_window_daily_seconds,
            QuotaPeriod::CalendarMonth | QuotaPeriod::RollingMonth | QuotaPeriod::Custom => {
                config.grace_window_monthly_seconds
            }
        };
        let projection_mature = observation_count >= config.minimum_observations
            && (now - window.started_at).num_seconds() >= grace_seconds;

        let mut rationale = Vec::new();
        let status = if quota_exceeded_error || snapshot.is_confirmed_exhausted() {
            rationale.push(
                "provider reported quota exhaustion or confirmed remaining is zero".to_owned(),
            );
            ForecastStatus::Exhausted
        } else if snapshot.confidence == UsageConfidence::Unknown
            || remaining_percent.is_none()
            || used_percent.is_none()
        {
            rationale.push("reliable usage and remaining values are unavailable".to_owned());
            ForecastStatus::Unknown
        } else if remaining_percent
            .is_some_and(|remaining| remaining <= config.hard_limit_remaining_percent)
            || safe_remaining_percent
                .is_some_and(|remaining| remaining < config.amber_safe_remaining_percent)
        {
            rationale.push("safe remaining quota is below the red threshold".to_owned());
            ForecastStatus::Red
        } else if projection_mature
            && projected_end_usage_percent
                .is_some_and(|projection| projection > config.projected_limit_percent)
        {
            rationale.push("mature projection exceeds the configured period limit".to_owned());
            ForecastStatus::Amber
        } else if safe_remaining_percent
            .is_some_and(|remaining| remaining < config.green_safe_remaining_percent)
        {
            rationale.push("safe remaining quota is below the green threshold".to_owned());
            ForecastStatus::Amber
        } else if !projection_mature {
            rationale.push("known headroom retained, but projection is not yet mature".to_owned());
            ForecastStatus::Amber
        } else {
            rationale.push("safe remaining and projected usage are within policy".to_owned());
            ForecastStatus::Green
        };

        Ok(BudgetForecast {
            provider: snapshot.provider,
            quota_scope: snapshot.quota_scope.clone(),
            confidence: snapshot.confidence,
            status,
            period_progress,
            usage_progress,
            burn_index,
            projected_end_usage_percent,
            safe_remaining_percent,
            remaining_units: snapshot.remaining,
            smoothed_usage_progress,
            observation_count,
            projection_mature,
            resets_at: window.resets_at,
            rationale,
        })
    }
}

fn effective_used_percent(snapshot: &UsageSnapshot) -> Option<f64> {
    snapshot.effective_used_percent().or_else(|| {
        snapshot
            .effective_remaining_percent()
            .map(|remaining| 100.0 - remaining)
    })
}

fn smoothed_progress(
    snapshot: &UsageSnapshot,
    history: &[UsageSnapshot],
    alpha: f64,
) -> (Option<f64>, usize) {
    let mut samples: Vec<_> = history
        .iter()
        .filter(|item| {
            item.provider == snapshot.provider
                && item.quota_scope == snapshot.quota_scope
                && item.confidence != UsageConfidence::Unknown
                && item.collected_at <= snapshot.collected_at
        })
        .filter_map(|item| {
            effective_used_percent(item)
                .map(|used| (item.collected_at, used / 100.0, item.confidence.weight()))
        })
        .collect();
    if snapshot.confidence != UsageConfidence::Unknown
        && let Some(used) = effective_used_percent(snapshot)
    {
        samples.push((
            snapshot.collected_at,
            used / 100.0,
            snapshot.confidence.weight(),
        ));
    }
    samples.sort_by_key(|(timestamp, _, _)| *timestamp);
    samples.dedup_by(|left, right| left.0 == right.0);

    let mut smoothed = None;
    for (_, value, confidence_weight) in &samples {
        smoothed = Some(match smoothed {
            None => *value,
            Some(previous) => {
                let effective_alpha = alpha * confidence_weight;
                effective_alpha * value + (1.0 - effective_alpha) * previous
            }
        });
    }
    (smoothed, samples.len())
}

#[derive(Debug, Error)]
pub enum ForecastError {
    #[error("invalid forecast configuration field: {0}")]
    InvalidConfig(&'static str),
    #[error("invalid quota window")]
    InvalidWindow,
    #[error(transparent)]
    InvalidUsage(#[from] orchestrator_domain::UsageError),
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, Utc};
    use orchestrator_domain::{
        QuotaPeriod, QuotaScope, SchemaVersion, UsageConfidence, UsageSource, UsageUnit,
    };

    use super::*;

    fn utc(value: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
        value.parse()
    }

    fn snapshot(
        used: Option<f64>,
        remaining: Option<f64>,
        confidence: UsageConfidence,
        collected_at: DateTime<Utc>,
    ) -> UsageSnapshot {
        UsageSnapshot {
            schema_version: SchemaVersion::v1(),
            provider: ProviderId::Codex,
            quota_scope: QuotaScope::new("monthly", QuotaPeriod::CalendarMonth, UsageUnit::Credits),
            quota_period: QuotaPeriod::CalendarMonth,
            used,
            limit: (confidence != UsageConfidence::Unknown).then_some(100.0),
            remaining,
            used_percent: used,
            remaining_percent: remaining,
            period_started_at: None,
            resets_at: None,
            source: if confidence == UsageConfidence::Unknown {
                UsageSource::Unknown
            } else {
                UsageSource::OfficialCli
            },
            confidence,
            collected_at,
        }
    }

    #[test]
    fn status_precedence_is_exhausted_then_unknown_then_red() -> Result<(), ForecastError> {
        let now = Utc::now();
        let window = PeriodWindow::new(now - Duration::days(10), now + Duration::days(10))
            .map_err(|_| ForecastError::InvalidWindow)?;
        let unknown = snapshot(None, None, UsageConfidence::Unknown, now);
        let exhausted = BudgetForecaster::forecast(
            &unknown,
            &[],
            &window,
            now,
            true,
            &ForecastConfig::default(),
        )?;
        assert_eq!(exhausted.status, ForecastStatus::Exhausted);
        let forecast = BudgetForecaster::forecast(
            &unknown,
            &[],
            &window,
            now,
            false,
            &ForecastConfig::default(),
        )?;
        assert_eq!(forecast.status, ForecastStatus::Unknown);
        Ok(())
    }

    #[test]
    fn mature_sustainable_usage_is_green() -> Result<(), Box<dyn std::error::Error>> {
        let start = utc("2026-07-01T00:00:00Z")?;
        let reset = utc("2026-07-31T00:00:00Z")?;
        let now = utc("2026-07-16T00:00:00Z")?;
        let current = snapshot(Some(40.0), Some(60.0), UsageConfidence::Confirmed, now);
        let history = vec![
            snapshot(
                Some(10.0),
                Some(90.0),
                UsageConfidence::Confirmed,
                start + Duration::days(4),
            ),
            snapshot(
                Some(25.0),
                Some(75.0),
                UsageConfidence::Confirmed,
                start + Duration::days(9),
            ),
        ];
        let config = ForecastConfig {
            reserve_percent: 0.0,
            ..ForecastConfig::default()
        };
        let forecast = BudgetForecaster::forecast(
            &current,
            &history,
            &PeriodWindow::new(start, reset)?,
            now,
            false,
            &config,
        )?;
        assert_eq!(forecast.status, ForecastStatus::Green);
        assert!(forecast.projection_mature);
        Ok(())
    }

    #[test]
    fn early_period_never_becomes_green_from_one_sample() -> Result<(), Box<dyn std::error::Error>>
    {
        let start = utc("2026-07-01T00:00:00Z")?;
        let current = snapshot(
            Some(1.0),
            Some(99.0),
            UsageConfidence::Confirmed,
            start + Duration::minutes(5),
        );
        let forecast = BudgetForecaster::forecast(
            &current,
            &[],
            &PeriodWindow::new(start, start + Duration::days(30))?,
            current.collected_at,
            false,
            &ForecastConfig::default(),
        )?;
        assert_eq!(forecast.status, ForecastStatus::Amber);
        assert!(!forecast.projection_mature);
        Ok(())
    }

    #[test]
    fn low_safe_remaining_is_red() -> Result<(), Box<dyn std::error::Error>> {
        let now = utc("2026-07-16T00:00:00Z")?;
        let current = snapshot(Some(80.0), Some(20.0), UsageConfidence::Confirmed, now);
        let forecast = BudgetForecaster::forecast(
            &current,
            &[],
            &PeriodWindow::new(utc("2026-07-01T00:00:00Z")?, utc("2026-08-01T00:00:00Z")?)?,
            now,
            false,
            &ForecastConfig::default(),
        )?;
        assert_eq!(forecast.status, ForecastStatus::Red);
        Ok(())
    }
}
