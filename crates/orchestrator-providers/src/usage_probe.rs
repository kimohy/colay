use std::ffi::OsString;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    ProviderId, QuotaPeriod, QuotaScope, UsageConfidence, UsageSnapshot, UsageSource, UsageUnit,
};
use serde::{Deserialize, Serialize};

use crate::{PreparedInvocation, ProviderError, StructuredOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageProbeFormat {
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UsageProbeConfig {
    Command {
        executable: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        format: UsageProbeFormat,
        #[serde(default)]
        working_directory: Option<PathBuf>,
    },
    ManualOrLedger,
}

impl UsageProbeConfig {
    /// Converts the configured executable and argv array to a shell-free probe.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the executable or argument specification
    /// is invalid.
    pub fn prepare(
        &self,
        fallback_working_directory: &Path,
    ) -> Result<Option<PreparedInvocation>, ProviderError> {
        let Self::Command {
            executable,
            args,
            format,
            working_directory,
        } = self
        else {
            return Ok(None);
        };
        if executable.as_os_str().is_empty() {
            return Err(ProviderError::EmptyUsageProbeExecutable);
        }
        if *format != UsageProbeFormat::Json {
            return Err(ProviderError::UnsupportedUsageProbeFormat);
        }
        let invocation = PreparedInvocation {
            executable: executable.clone(),
            args: args.iter().map(OsString::from).collect(),
            stdin: Vec::new(),
            working_directory: working_directory
                .clone()
                .unwrap_or_else(|| fallback_working_directory.to_path_buf()),
            timeout_seconds: 30,
            stdout_limit: 1024 * 1024,
            stderr_limit: 1024 * 1024,
            output: StructuredOutput::UsageJson,
            codex_app_server: None,
            fallback: None,
        };
        invocation.validate()?;
        Ok(Some(invocation))
    }
}

#[derive(Debug, Deserialize)]
struct ProbePayload {
    #[serde(default)]
    used: Option<f64>,
    #[serde(default)]
    limit: Option<f64>,
    #[serde(default)]
    remaining: Option<f64>,
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    remaining_percent: Option<f64>,
    #[serde(default)]
    period_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    resets_at: Option<DateTime<Utc>>,
    #[serde(default)]
    confidence: Option<UsageConfidence>,
}

/// Parses an administrator-provided machine-readable usage result.
///
/// # Errors
///
/// Returns [`ProviderError`] for malformed JSON, invalid values, or an
/// inconsistent quota period.
pub fn parse_usage_probe_output(
    provider: ProviderId,
    scope: QuotaScope,
    bytes: &[u8],
    collected_at: DateTime<Utc>,
) -> Result<UsageSnapshot, ProviderError> {
    let payload: ProbePayload = serde_json::from_slice(bytes)
        .map_err(|error| ProviderError::UsageProbeJson(error.to_string()))?;
    let has_usage_value = payload.used.is_some()
        || payload.limit.is_some()
        || payload.remaining.is_some()
        || payload.used_percent.is_some()
        || payload.remaining_percent.is_some();
    let confidence = payload.confidence.unwrap_or(if has_usage_value {
        UsageConfidence::Estimated
    } else {
        UsageConfidence::Unknown
    });
    let mut snapshot = UsageSnapshot::unknown(provider, scope, collected_at);
    snapshot.used = payload.used;
    snapshot.limit = payload.limit;
    snapshot.remaining = payload.remaining.or(match (payload.limit, payload.used) {
        (Some(limit), Some(used)) if limit >= used => Some(limit - used),
        _ => None,
    });
    snapshot.used_percent = payload.used_percent.or_else(|| {
        let used = payload.used?;
        let limit = payload.limit?;
        (limit > 0.0).then_some((used / limit * 100.0).clamp(0.0, 100.0))
    });
    snapshot.remaining_percent = payload.remaining_percent.or_else(|| {
        if let (Some(remaining), Some(limit)) = (snapshot.remaining, payload.limit) {
            return (limit > 0.0).then_some((remaining / limit * 100.0).clamp(0.0, 100.0));
        }
        snapshot.used_percent.map(|used| 100.0 - used)
    });
    snapshot.period_started_at = payload.period_started_at;
    snapshot.resets_at = payload.resets_at;
    snapshot.source = UsageSource::ConfiguredProbe;
    snapshot.confidence = confidence;
    snapshot
        .validate()
        .map_err(|error| ProviderError::UsageProbeJson(error.to_string()))?;
    Ok(snapshot)
}

#[must_use]
pub fn unknown_usage(provider: ProviderId, now: DateTime<Utc>) -> UsageSnapshot {
    let (name, period) = match provider {
        ProviderId::Gemini | ProviderId::Agy => ("primary_daily", QuotaPeriod::CalendarDay),
        ProviderId::Codex | ProviderId::Claude => ("primary_monthly", QuotaPeriod::CalendarMonth),
    };
    UsageSnapshot::unknown(
        provider,
        QuotaScope::new(
            name,
            period,
            UsageUnit::Custom("provider_defined".to_owned()),
        ),
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_derives_only_mathematically_supported_values() {
        let snapshot = parse_usage_probe_output(
            ProviderId::Gemini,
            QuotaScope::new("daily", QuotaPeriod::CalendarDay, UsageUnit::Requests),
            br#"{"used":25,"limit":100,"confidence":"confirmed"}"#,
            Utc::now(),
        );
        assert!(matches!(snapshot, Ok(ref value) if value.remaining == Some(75.0)));
        assert!(matches!(snapshot, Ok(ref value) if value.remaining_percent == Some(75.0)));
    }

    #[test]
    fn missing_values_remain_unknown_values() {
        let snapshot = parse_usage_probe_output(
            ProviderId::Claude,
            QuotaScope::new("monthly", QuotaPeriod::CalendarMonth, UsageUnit::Credits),
            br"{}",
            Utc::now(),
        );
        assert!(matches!(snapshot, Ok(ref value) if value.used.is_none()));
        assert!(matches!(
            snapshot,
            Ok(ref value) if value.confidence == UsageConfidence::Unknown
        ));
    }

    #[test]
    fn shell_string_is_never_interpreted() {
        let config = UsageProbeConfig::Command {
            executable: PathBuf::from("company-usage"),
            args: vec!["$(whoami)".to_owned(), "; rm".to_owned()],
            format: UsageProbeFormat::Json,
            working_directory: None,
        };
        let invocation = config.prepare(Path::new("repo"));
        assert!(matches!(
            invocation,
            Ok(Some(ref value)) if value.args.len() == 2
        ));
    }
}
