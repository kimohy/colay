use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use orchestrator_domain::QuotaPeriod;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResetPolicy {
    pub quota_period: QuotaPeriod,
    pub timezone: Tz,
    pub reset_day: Option<u8>,
    pub rolling_anchor: Option<DateTime<Utc>>,
    /// Overrides 24 hours for rolling-day or 30 days for rolling-month policies.
    pub rolling_period_seconds: Option<i64>,
    pub custom_started_at: Option<DateTime<Utc>>,
    pub custom_resets_at: Option<DateTime<Utc>>,
}

impl ResetPolicy {
    #[must_use]
    pub fn calendar_day(timezone: Tz) -> Self {
        Self {
            quota_period: QuotaPeriod::CalendarDay,
            timezone,
            reset_day: None,
            rolling_anchor: None,
            rolling_period_seconds: None,
            custom_started_at: None,
            custom_resets_at: None,
        }
    }

    #[must_use]
    pub fn calendar_month(timezone: Tz, reset_day: u8) -> Self {
        Self {
            quota_period: QuotaPeriod::CalendarMonth,
            timezone,
            reset_day: Some(reset_day),
            rolling_anchor: None,
            rolling_period_seconds: None,
            custom_started_at: None,
            custom_resets_at: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeriodWindow {
    pub started_at: DateTime<Utc>,
    pub resets_at: DateTime<Utc>,
}

impl PeriodWindow {
    /// # Errors
    ///
    /// Returns [`PeriodError::InvalidWindow`] when reset does not follow start.
    pub fn new(started_at: DateTime<Utc>, resets_at: DateTime<Utc>) -> Result<Self, PeriodError> {
        if resets_at <= started_at {
            return Err(PeriodError::InvalidWindow);
        }
        Ok(Self {
            started_at,
            resets_at,
        })
    }

    #[must_use]
    pub fn duration_seconds(&self) -> f64 {
        (self.resets_at - self.started_at)
            .to_std()
            .map_or(f64::INFINITY, |duration| duration.as_secs_f64())
    }

    #[must_use]
    pub fn elapsed_seconds(&self, now: DateTime<Utc>) -> f64 {
        let elapsed = now - self.started_at;
        if elapsed < Duration::zero() {
            -elapsed
                .abs()
                .to_std()
                .map_or(f64::INFINITY, |duration| duration.as_secs_f64())
        } else {
            elapsed
                .to_std()
                .map_or(f64::INFINITY, |duration| duration.as_secs_f64())
        }
    }

    #[must_use]
    pub fn progress_at(&self, now: DateTime<Utc>) -> f64 {
        (self.elapsed_seconds(now) / self.duration_seconds()).clamp(0.0, 1.0)
    }
}

/// Computes the quota window containing `now` from its reset policy.
///
/// # Errors
///
/// Returns [`PeriodError`] for missing rolling/custom anchors, invalid reset values, or date
/// arithmetic outside Chrono's representable range.
pub fn period_window(
    policy: &ResetPolicy,
    now: DateTime<Utc>,
) -> Result<PeriodWindow, PeriodError> {
    match policy.quota_period {
        QuotaPeriod::CalendarDay => calendar_day_window(policy.timezone, now),
        QuotaPeriod::CalendarMonth => {
            calendar_month_window(policy.timezone, policy.reset_day.unwrap_or(1), now)
        }
        QuotaPeriod::RollingDay => rolling_window(policy, now, 24 * 60 * 60),
        QuotaPeriod::RollingMonth => rolling_window(policy, now, 30 * 24 * 60 * 60),
        QuotaPeriod::Custom => PeriodWindow::new(
            policy
                .custom_started_at
                .ok_or(PeriodError::MissingCustomWindow)?,
            policy
                .custom_resets_at
                .ok_or(PeriodError::MissingCustomWindow)?,
        ),
    }
}

fn calendar_day_window(timezone: Tz, now: DateTime<Utc>) -> Result<PeriodWindow, PeriodError> {
    let date = now.with_timezone(&timezone).date_naive();
    let next_date = date.succ_opt().ok_or(PeriodError::DateOverflow)?;
    let start = resolve_boundary(timezone, midnight(date)?)?;
    let end = resolve_boundary(timezone, midnight(next_date)?)?;
    PeriodWindow::new(start.with_timezone(&Utc), end.with_timezone(&Utc))
}

fn calendar_month_window(
    timezone: Tz,
    reset_day: u8,
    now: DateTime<Utc>,
) -> Result<PeriodWindow, PeriodError> {
    if !(1..=31).contains(&reset_day) {
        return Err(PeriodError::InvalidResetDay(reset_day));
    }
    let local_now = now.with_timezone(&timezone);
    let this_candidate = month_reset(local_now.year(), local_now.month(), reset_day)?;
    let this_boundary = resolve_boundary(timezone, midnight(this_candidate)?)?;

    let (start_date, end_date) = if local_now >= this_boundary {
        let (next_year, next_month) = next_month(local_now.year(), local_now.month())?;
        (
            this_candidate,
            month_reset(next_year, next_month, reset_day)?,
        )
    } else {
        let (previous_year, previous_month) = previous_month(local_now.year(), local_now.month())?;
        (
            month_reset(previous_year, previous_month, reset_day)?,
            this_candidate,
        )
    };
    let start = resolve_boundary(timezone, midnight(start_date)?)?;
    let end = resolve_boundary(timezone, midnight(end_date)?)?;
    PeriodWindow::new(start.with_timezone(&Utc), end.with_timezone(&Utc))
}

fn rolling_window(
    policy: &ResetPolicy,
    now: DateTime<Utc>,
    default_seconds: i64,
) -> Result<PeriodWindow, PeriodError> {
    let anchor = policy
        .rolling_anchor
        .ok_or(PeriodError::MissingRollingAnchor)?;
    let seconds = policy.rolling_period_seconds.unwrap_or(default_seconds);
    if seconds <= 0 {
        return Err(PeriodError::InvalidRollingDuration(seconds));
    }
    let elapsed = (now - anchor).num_seconds();
    let index = elapsed.div_euclid(seconds);
    let offset = index
        .checked_mul(seconds)
        .ok_or(PeriodError::DateOverflow)?;
    let start = anchor
        .checked_add_signed(Duration::seconds(offset))
        .ok_or(PeriodError::DateOverflow)?;
    let end = start
        .checked_add_signed(Duration::seconds(seconds))
        .ok_or(PeriodError::DateOverflow)?;
    PeriodWindow::new(start, end)
}

fn resolve_boundary(timezone: Tz, mut value: NaiveDateTime) -> Result<DateTime<Tz>, PeriodError> {
    // A small number of IANA zones historically advanced clocks at midnight. The first
    // representable local instant is the correct inclusive boundary for those dates.
    for _ in 0..=180 {
        match timezone.from_local_datetime(&value) {
            LocalResult::Single(result) => return Ok(result),
            LocalResult::Ambiguous(first, second) => return Ok(first.min(second)),
            LocalResult::None => {
                value = value
                    .checked_add_signed(Duration::minutes(1))
                    .ok_or(PeriodError::DateOverflow)?;
            }
        }
    }
    Err(PeriodError::UnresolvableLocalBoundary)
}

fn midnight(date: NaiveDate) -> Result<NaiveDateTime, PeriodError> {
    date.and_hms_opt(0, 0, 0).ok_or(PeriodError::DateOverflow)
}

fn month_reset(year: i32, month: u32, reset_day: u8) -> Result<NaiveDate, PeriodError> {
    let last_day = days_in_month(year, month)?;
    NaiveDate::from_ymd_opt(year, month, u32::from(reset_day).min(last_day))
        .ok_or(PeriodError::DateOverflow)
}

fn days_in_month(year: i32, month: u32) -> Result<u32, PeriodError> {
    let (next_year, next_month) = next_month(year, month)?;
    let first_next =
        NaiveDate::from_ymd_opt(next_year, next_month, 1).ok_or(PeriodError::DateOverflow)?;
    let last_current = first_next.pred_opt().ok_or(PeriodError::DateOverflow)?;
    Ok(last_current.day())
}

fn next_month(year: i32, month: u32) -> Result<(i32, u32), PeriodError> {
    match month {
        1..=11 => Ok((year, month + 1)),
        12 => Ok((year.checked_add(1).ok_or(PeriodError::DateOverflow)?, 1)),
        _ => Err(PeriodError::DateOverflow),
    }
}

fn previous_month(year: i32, month: u32) -> Result<(i32, u32), PeriodError> {
    match month {
        2..=12 => Ok((year, month - 1)),
        1 => Ok((year.checked_sub(1).ok_or(PeriodError::DateOverflow)?, 12)),
        _ => Err(PeriodError::DateOverflow),
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PeriodError {
    #[error("calendar reset day {0} must be within 1..=31")]
    InvalidResetDay(u8),
    #[error("rolling quota requires an anchor")]
    MissingRollingAnchor,
    #[error("rolling duration {0} must be positive")]
    InvalidRollingDuration(i64),
    #[error("custom quota requires both start and reset timestamps")]
    MissingCustomWindow,
    #[error("quota window must have a reset after its start")]
    InvalidWindow,
    #[error("date arithmetic overflow")]
    DateOverflow,
    #[error("local reset boundary could not be resolved in its configured timezone")]
    UnresolvableLocalBoundary,
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use chrono_tz::{America::New_York, Asia::Seoul};

    use super::*;

    fn utc(value: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
        value.parse()
    }

    #[test]
    fn spring_dst_calendar_day_is_23_hours() -> Result<(), Box<dyn std::error::Error>> {
        let now = utc("2026-03-08T12:00:00Z")?;
        let window = period_window(&ResetPolicy::calendar_day(New_York), now)?;
        assert_eq!((window.resets_at - window.started_at).num_hours(), 23);
        assert_eq!(window.started_at, utc("2026-03-08T05:00:00Z")?);
        assert_eq!(window.resets_at, utc("2026-03-09T04:00:00Z")?);
        Ok(())
    }

    #[test]
    fn fall_dst_calendar_day_is_25_hours() -> Result<(), Box<dyn std::error::Error>> {
        let now = utc("2026-11-01T12:00:00Z")?;
        let window = period_window(&ResetPolicy::calendar_day(New_York), now)?;
        assert_eq!((window.resets_at - window.started_at).num_hours(), 25);
        Ok(())
    }

    #[test]
    fn reset_day_clamps_to_last_day_of_month() -> Result<(), Box<dyn std::error::Error>> {
        let now = utc("2026-02-28T10:00:00Z")?;
        let window = period_window(&ResetPolicy::calendar_month(Seoul, 31), now)?;
        assert_eq!(window.started_at, utc("2026-02-27T15:00:00Z")?);
        assert_eq!(window.resets_at, utc("2026-03-30T15:00:00Z")?);
        Ok(())
    }

    #[test]
    fn rolling_window_is_anchor_stable() -> Result<(), Box<dyn std::error::Error>> {
        let anchor = utc("2026-01-01T00:00:00Z")?;
        let policy = ResetPolicy {
            quota_period: QuotaPeriod::RollingDay,
            timezone: Seoul,
            reset_day: None,
            rolling_anchor: Some(anchor),
            rolling_period_seconds: None,
            custom_started_at: None,
            custom_resets_at: None,
        };
        let window = period_window(&policy, utc("2026-01-03T12:00:00Z")?)?;
        assert_eq!(window.started_at, utc("2026-01-03T00:00:00Z")?);
        assert_eq!(window.resets_at, utc("2026-01-04T00:00:00Z")?);
        Ok(())
    }
}
