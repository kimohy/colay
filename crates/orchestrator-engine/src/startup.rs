use codex_compat::{CodexProbeReport, CompatibilityStatus};
use orchestrator_domain::{CapabilitySupport, ProviderCapabilities, ProviderId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexExecutionPolicy {
    ReadWrite,
    ReadOnly,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupGuardReport {
    pub codex_policy: CodexExecutionPolicy,
    pub safe_mode: bool,
    pub task_execution_available: bool,
    pub available_providers: Vec<ProviderId>,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct StartupGuard;

impl StartupGuard {
    #[must_use]
    pub fn evaluate(
        codex: Option<&CodexProbeReport>,
        other_providers: &[ProviderCapabilities],
        state_schema_compatible: bool,
        config_schema_compatible: bool,
        handover_schema_compatible: bool,
    ) -> StartupGuardReport {
        let mut warnings = Vec::new();
        let mut blockers = Vec::new();
        let mut available = other_providers
            .iter()
            .filter(|provider| {
                provider.provider != ProviderId::Codex
                    && provider.non_interactive.usable()
                    && provider.read_only.usable()
            })
            .map(|provider| provider.provider)
            .collect::<Vec<_>>();

        let codex_policy = if let Some(report) = codex {
            match report.status {
                CompatibilityStatus::Compatible => {
                    codex_policy_from_capabilities(report, &mut warnings)
                }
                CompatibilityStatus::CompatibleWithWarnings => {
                    warnings.push(
                        "Codex optional capabilities are degraded; exec fallback applies"
                            .to_owned(),
                    );
                    codex_policy_from_capabilities(report, &mut warnings)
                }
                CompatibilityStatus::Untested => {
                    warnings
                        .push("Codex version is untested; writable work is disabled".to_owned());
                    if report.capabilities.read_only_sandbox.is_available() {
                        CodexExecutionPolicy::ReadOnly
                    } else {
                        warnings.push(
                            "Codex read-only sandbox capability is unavailable; Codex is disabled"
                                .to_owned(),
                        );
                        CodexExecutionPolicy::Disabled
                    }
                }
                CompatibilityStatus::Incompatible => {
                    warnings.push(
                        "Codex compatibility contract is incompatible; Codex is disabled"
                            .to_owned(),
                    );
                    CodexExecutionPolicy::Disabled
                }
            }
        } else {
            warnings.push("Codex binary was not detected; Codex is disabled".to_owned());
            CodexExecutionPolicy::Disabled
        };

        if codex_policy != CodexExecutionPolicy::Disabled {
            available.push(ProviderId::Codex);
        }
        available.sort();
        available.dedup();

        if !state_schema_compatible {
            blockers.push("SQLite state schema is incompatible".to_owned());
        }
        if !config_schema_compatible {
            blockers.push("configuration schema is incompatible".to_owned());
        }
        if !handover_schema_compatible {
            blockers.push("handover schema is incompatible".to_owned());
        }
        if available.is_empty() {
            blockers.push("no approved provider has a usable non-interactive interface".to_owned());
        }

        StartupGuardReport {
            codex_policy,
            safe_mode: codex_policy != CodexExecutionPolicy::ReadWrite
                || !state_schema_compatible
                || !config_schema_compatible
                || !handover_schema_compatible,
            task_execution_available: blockers.is_empty(),
            available_providers: available,
            warnings,
            blockers,
        }
    }

    #[must_use]
    pub fn codex_domain_capabilities(report: &CodexProbeReport) -> ProviderCapabilities {
        let map = |support: codex_compat::CapabilitySupport| match support {
            codex_compat::CapabilitySupport::Unsupported => CapabilitySupport::Unsupported,
            codex_compat::CapabilitySupport::Advertised => CapabilitySupport::Advertised,
            codex_compat::CapabilitySupport::Verified => CapabilitySupport::Verified,
            codex_compat::CapabilitySupport::Degraded => CapabilitySupport::Degraded,
        };
        ProviderCapabilities {
            provider: ProviderId::Codex,
            version: report
                .capabilities
                .version
                .as_ref()
                .map(ToString::to_string),
            non_interactive: map(report.capabilities.exec),
            structured_output: map(report.capabilities.jsonl_output),
            writable: map(report.capabilities.workspace_write_sandbox),
            read_only: map(report.capabilities.read_only_sandbox),
            session_resume: map(report.capabilities.session_resume),
            output_schema: map(report.capabilities.output_schema),
            app_server: map(report.capabilities.app_server),
            reasoning_effort: map(report.capabilities.reasoning_effort),
            usage_events: map(report.capabilities.usage_events),
            evidence: report
                .evidence
                .iter()
                .map(|evidence| format!("{}: {}", evidence.capability, evidence.detail))
                .collect(),
        }
    }
}

fn codex_policy_from_capabilities(
    report: &CodexProbeReport,
    warnings: &mut Vec<String>,
) -> CodexExecutionPolicy {
    let read_only = report.capabilities.read_only_sandbox.is_available();
    let writable = report.capabilities.workspace_write_sandbox.is_available();

    match (read_only, writable) {
        (true, true) => CodexExecutionPolicy::ReadWrite,
        (true, false) => {
            warnings.push(
                "Codex workspace-write sandbox capability is unavailable; writable work is disabled"
                    .to_owned(),
            );
            CodexExecutionPolicy::ReadOnly
        }
        (false, _) => {
            warnings.push(
                "Codex read-only sandbox capability is unavailable; Codex is disabled".to_owned(),
            );
            CodexExecutionPolicy::Disabled
        }
    }
}

#[cfg(test)]
mod tests {
    use codex_compat::{
        AdapterSelection, CapabilitySupport as CodexCapabilitySupport, CodexCapabilities,
        CodexProbeReport, CompatibilityStatus,
    };
    use orchestrator_domain::{ProviderCapabilities, ProviderId};

    use super::{CodexExecutionPolicy, StartupGuard};

    #[test]
    fn missing_codex_does_not_block_other_providers() {
        let mut claude = ProviderCapabilities::unsupported(ProviderId::Claude);
        claude.non_interactive = orchestrator_domain::CapabilitySupport::Verified;
        claude.read_only = orchestrator_domain::CapabilitySupport::Verified;
        let report = StartupGuard::evaluate(None, &[claude], true, true, true);
        assert!(report.task_execution_available);
        assert_eq!(report.codex_policy, CodexExecutionPolicy::Disabled);
        assert!(report.safe_mode);
    }

    #[test]
    fn aggregate_compatible_status_does_not_override_missing_write_capability() {
        let mut codex = codex_report(CompatibilityStatus::Compatible);
        codex.capabilities.read_only_sandbox = CodexCapabilitySupport::Advertised;

        let report = StartupGuard::evaluate(Some(&codex), &[], true, true, true);

        assert_eq!(report.codex_policy, CodexExecutionPolicy::ReadOnly);
        assert!(report.safe_mode);
    }

    #[test]
    fn compatible_codex_is_writable_only_when_both_sandbox_capabilities_are_available() {
        let mut codex = codex_report(CompatibilityStatus::Compatible);
        codex.capabilities.read_only_sandbox = CodexCapabilitySupport::Advertised;
        codex.capabilities.workspace_write_sandbox = CodexCapabilitySupport::Advertised;

        let report = StartupGuard::evaluate(Some(&codex), &[], true, true, true);

        assert_eq!(report.codex_policy, CodexExecutionPolicy::ReadWrite);
        assert!(report.task_execution_available);
        assert!(!report.safe_mode);
    }

    #[test]
    fn aggregate_compatible_status_does_not_override_missing_read_only_capability() {
        let mut codex = codex_report(CompatibilityStatus::Compatible);
        codex.capabilities.workspace_write_sandbox = CodexCapabilitySupport::Advertised;

        let report = StartupGuard::evaluate(Some(&codex), &[], true, true, true);

        assert_eq!(report.codex_policy, CodexExecutionPolicy::Disabled);
        assert!(!report.task_execution_available);
    }

    #[test]
    fn untested_codex_is_never_writable_even_when_both_sandboxes_are_advertised() {
        let mut codex = codex_report(CompatibilityStatus::Untested);
        codex.capabilities.read_only_sandbox = CodexCapabilitySupport::Advertised;
        codex.capabilities.workspace_write_sandbox = CodexCapabilitySupport::Advertised;

        let report = StartupGuard::evaluate(Some(&codex), &[], true, true, true);

        assert_eq!(report.codex_policy, CodexExecutionPolicy::ReadOnly);
        assert!(report.safe_mode);
    }

    #[test]
    fn incompatible_codex_is_disabled_even_when_both_sandboxes_are_advertised() {
        let mut codex = codex_report(CompatibilityStatus::Incompatible);
        codex.capabilities.read_only_sandbox = CodexCapabilitySupport::Advertised;
        codex.capabilities.workspace_write_sandbox = CodexCapabilitySupport::Advertised;

        let report = StartupGuard::evaluate(Some(&codex), &[], true, true, true);

        assert_eq!(report.codex_policy, CodexExecutionPolicy::Disabled);
        assert!(!report.task_execution_available);
    }

    fn codex_report(status: CompatibilityStatus) -> CodexProbeReport {
        CodexProbeReport {
            capabilities: CodexCapabilities::default(),
            status,
            adapter: AdapterSelection::GenericUntested,
            evidence: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}
