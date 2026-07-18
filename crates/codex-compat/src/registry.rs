use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::{CapabilitySupport, CodexCapabilities};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct VersionContract {
    pub version: Version,
    pub adapter_name: String,
    pub maintenance: bool,
    #[serde(default)]
    pub exec: bool,
    #[serde(default)]
    pub jsonl: bool,
    #[serde(default)]
    pub resume: bool,
    #[serde(default)]
    pub output_schema: bool,
    #[serde(default)]
    pub sandbox: bool,
    pub reasoning_effort: bool,
    pub usage_events: bool,
}

impl VersionContract {
    pub(crate) fn enrich_capabilities(&self, capabilities: &mut CodexCapabilities) {
        verify_advertised(&mut capabilities.exec, self.exec);
        verify_advertised(&mut capabilities.jsonl_output, self.jsonl);
        verify_advertised(&mut capabilities.session_resume, self.resume);
        verify_advertised(&mut capabilities.output_schema, self.output_schema);
        verify_advertised(&mut capabilities.read_only_sandbox, self.sandbox);
        verify_advertised(&mut capabilities.workspace_write_sandbox, self.sandbox);
        if self.usage_events && capabilities.usage_events.is_available() {
            capabilities.usage_events = CapabilitySupport::Verified;
        }
    }
}

fn verify_advertised(capability: &mut CapabilitySupport, fixture_verified: bool) {
    if fixture_verified && capability.is_available() {
        *capability = CapabilitySupport::Verified;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdapterSelection {
    Exact { contract: VersionContract },
    CompatibleRange { contract: VersionContract },
    GenericUntested,
    SafeMode,
}

impl AdapterSelection {
    #[must_use]
    pub const fn contract(&self) -> Option<&VersionContract> {
        match self {
            Self::Exact { contract } | Self::CompatibleRange { contract } => Some(contract),
            Self::GenericUntested | Self::SafeMode => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionPolicy {
    pub supported_min: Version,
    pub tested_versions: Vec<Version>,
    pub recommended: Version,
    pub pinned_revision: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompatibilityRegistry {
    exact: Vec<VersionContract>,
    compatible_ranges: Vec<(VersionReq, VersionContract)>,
}

impl Default for CompatibilityRegistry {
    fn default() -> Self {
        let n = VersionContract {
            version: Version::new(0, 144, 5),
            adapter_name: "v0_144_generic".to_owned(),
            maintenance: false,
            exec: true,
            jsonl: true,
            resume: true,
            output_schema: true,
            sandbox: true,
            reasoning_effort: true,
            usage_events: true,
        };
        let n_minus_one = VersionContract {
            version: Version::new(0, 144, 4),
            adapter_name: "v0_144_generic".to_owned(),
            maintenance: false,
            exec: true,
            jsonl: true,
            resume: true,
            output_schema: true,
            sandbox: true,
            reasoning_effort: true,
            usage_events: true,
        };
        Self {
            exact: vec![n, n_minus_one],
            // Range adapters are intentionally empty until a real protocol
            // difference has been fixture-tested.
            compatible_ranges: Vec::new(),
        }
    }
}

impl CompatibilityRegistry {
    #[must_use]
    pub fn select(&self, version: Option<&Version>) -> AdapterSelection {
        let Some(version) = version else {
            return AdapterSelection::SafeMode;
        };
        if let Some(contract) = self
            .exact
            .iter()
            .find(|contract| contract.version == *version)
        {
            return AdapterSelection::Exact {
                contract: contract.clone(),
            };
        }
        if let Some((_, contract)) = self
            .compatible_ranges
            .iter()
            .find(|(requirement, _)| requirement.matches(version))
        {
            return AdapterSelection::CompatibleRange {
                contract: contract.clone(),
            };
        }
        AdapterSelection::GenericUntested
    }

    #[must_use]
    pub fn tested_versions(&self) -> Vec<Version> {
        self.exact
            .iter()
            .map(|contract| contract.version.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_exact_tested_versions() {
        let registry = CompatibilityRegistry::default();
        assert!(matches!(
            registry.select(Some(&Version::new(0, 144, 5))),
            AdapterSelection::Exact { .. }
        ));
        assert_eq!(
            registry.select(Some(&Version::new(0, 145, 0))),
            AdapterSelection::GenericUntested
        );
        assert_eq!(registry.select(None), AdapterSelection::SafeMode);
    }
}
