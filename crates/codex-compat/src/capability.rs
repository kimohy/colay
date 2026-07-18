use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AdapterSelection, CompatibilityRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    #[default]
    Unsupported,
    Advertised,
    Verified,
    Degraded,
}

impl CapabilitySupport {
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Advertised | Self::Verified | Self::Degraded)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCapabilities {
    pub version: Option<Version>,
    pub exec: CapabilitySupport,
    pub jsonl_output: CapabilitySupport,
    pub app_server: CapabilitySupport,
    pub session_resume: CapabilitySupport,
    pub output_schema: CapabilitySupport,
    pub workspace_write_sandbox: CapabilitySupport,
    pub read_only_sandbox: CapabilitySupport,
    pub reasoning_effort: CapabilitySupport,
    pub exec_reasoning_effort: CapabilitySupport,
    pub app_server_reasoning_effort: CapabilitySupport,
    pub usage_events: CapabilitySupport,
}

impl Default for CodexCapabilities {
    fn default() -> Self {
        Self {
            version: None,
            exec: CapabilitySupport::Unsupported,
            jsonl_output: CapabilitySupport::Unsupported,
            app_server: CapabilitySupport::Unsupported,
            session_resume: CapabilitySupport::Unsupported,
            output_schema: CapabilitySupport::Unsupported,
            workspace_write_sandbox: CapabilitySupport::Unsupported,
            read_only_sandbox: CapabilitySupport::Unsupported,
            reasoning_effort: CapabilitySupport::Unsupported,
            exec_reasoning_effort: CapabilitySupport::Unsupported,
            app_server_reasoning_effort: CapabilitySupport::Unsupported,
            usage_events: CapabilitySupport::Unsupported,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityStatus {
    Compatible,
    CompatibleWithWarnings,
    Untested,
    Incompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityEvidence {
    pub capability: String,
    pub support: CapabilitySupport,
    pub source: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexProbeReport {
    pub capabilities: CodexCapabilities,
    pub status: CompatibilityStatus,
    pub adapter: AdapterSelection,
    pub evidence: Vec<CapabilityEvidence>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProbeCommandKind {
    Version,
    RootHelp,
    ExecHelp,
    ExecResumeHelp,
    AppServerHelp,
    AppServerSchema,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeCommand {
    pub kind: ProbeCommandKind,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl ProbeOutput {
    #[must_use]
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            exit_code: Some(0),
            stdout: text.into(),
            stderr: String::new(),
        }
    }

    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self.exit_code, Some(0))
    }

    fn combined(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

pub trait CapabilitySource {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Runs only the supplied non-inference version/help/schema command.
    ///
    /// # Errors
    ///
    /// Returns the source-specific error if the safe diagnostic command could
    /// not be executed or captured.
    fn run(&mut self, command: &ProbeCommand) -> Result<ProbeOutput, Self::Error>;
}

#[derive(Debug, Clone, Default)]
pub struct CapabilityProbeInput {
    pub outputs: BTreeMap<ProbeCommandKind, ProbeOutput>,
    /// Concatenated stable schemas generated into the temporary schema folder.
    pub generated_schema: Option<String>,
}

#[derive(Debug, Error)]
pub enum CapabilityProbeError {
    #[error("capability source failed for {command:?}: {message}")]
    Source {
        command: ProbeCommandKind,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct CapabilityProbe {
    registry: CompatibilityRegistry,
}

impl Default for CapabilityProbe {
    fn default() -> Self {
        Self::new(CompatibilityRegistry::default())
    }
}

impl CapabilityProbe {
    #[must_use]
    pub const fn new(registry: CompatibilityRegistry) -> Self {
        Self { registry }
    }

    /// Returns the complete safe probe plan. No command starts a model turn.
    #[must_use]
    pub fn plan(schema_directory: &Path) -> Vec<ProbeCommand> {
        vec![
            ProbeCommand {
                kind: ProbeCommandKind::Version,
                args: vec!["--version".to_owned()],
            },
            ProbeCommand {
                kind: ProbeCommandKind::RootHelp,
                args: vec!["--help".to_owned()],
            },
            ProbeCommand {
                kind: ProbeCommandKind::ExecHelp,
                args: vec!["exec".to_owned(), "--help".to_owned()],
            },
            ProbeCommand {
                kind: ProbeCommandKind::ExecResumeHelp,
                args: vec!["exec".to_owned(), "resume".to_owned(), "--help".to_owned()],
            },
            ProbeCommand {
                kind: ProbeCommandKind::AppServerHelp,
                args: vec!["app-server".to_owned(), "--help".to_owned()],
            },
            ProbeCommand {
                kind: ProbeCommandKind::AppServerSchema,
                args: vec![
                    "app-server".to_owned(),
                    "generate-json-schema".to_owned(),
                    "--out".to_owned(),
                    schema_directory.to_string_lossy().into_owned(),
                ],
            },
        ]
    }

    /// Executes the safe probe plan and reads generated stable schemas.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilityProbeError`] if a probe command cannot run.
    pub fn collect<S: CapabilitySource>(
        &self,
        source: &mut S,
        schema_directory: &Path,
    ) -> Result<CapabilityProbeInput, CapabilityProbeError> {
        let mut outputs = BTreeMap::new();
        for command in Self::plan(schema_directory) {
            let output = source
                .run(&command)
                .map_err(|error| CapabilityProbeError::Source {
                    command: command.kind,
                    message: error.to_string(),
                })?;
            outputs.insert(command.kind, output);
        }
        Ok(CapabilityProbeInput {
            outputs,
            generated_schema: read_schema_tree(schema_directory),
        })
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn evaluate(&self, input: &CapabilityProbeInput) -> CodexProbeReport {
        let mut capabilities = CodexCapabilities::default();
        let mut evidence = Vec::new();
        let mut diagnostics = Vec::new();

        let version_output = input.outputs.get(&ProbeCommandKind::Version);
        capabilities.version = version_output.and_then(parse_codex_version);
        add_evidence(
            &mut evidence,
            "version",
            if capabilities.version.is_some() {
                CapabilitySupport::Verified
            } else {
                CapabilitySupport::Unsupported
            },
            "codex --version",
            capabilities.version.as_ref().map_or_else(
                || "unparseable or unavailable".to_owned(),
                ToString::to_string,
            ),
        );

        let root_help = successful_text(input, ProbeCommandKind::RootHelp);
        let exec_help = successful_text(input, ProbeCommandKind::ExecHelp);
        let resume_help = successful_text(input, ProbeCommandKind::ExecResumeHelp);
        let app_help = successful_text(input, ProbeCommandKind::AppServerHelp);

        capabilities.exec = support_if(
            exec_help.is_some()
                && root_help
                    .as_deref()
                    .is_some_and(|text| contains_token(text, "exec")),
        );
        capabilities.jsonl_output = support_if(
            exec_help
                .as_deref()
                .is_some_and(|text| text.contains("--json")),
        );
        capabilities.session_resume = support_if(resume_help.is_some());
        capabilities.output_schema = support_if(
            exec_help
                .as_deref()
                .is_some_and(|text| text.contains("--output-schema")),
        );
        capabilities.workspace_write_sandbox = support_if(
            exec_help
                .as_deref()
                .is_some_and(|text| text.contains("workspace-write")),
        );
        capabilities.read_only_sandbox = support_if(
            exec_help
                .as_deref()
                .is_some_and(|text| text.contains("read-only")),
        );
        capabilities.app_server = support_if(app_help.is_some());
        // Unlike a version-number contract, the installed binary's explicit
        // help is runtime evidence for this exact option and value set.
        capabilities.exec_reasoning_effort = if exec_help.as_deref().is_some_and(|text| {
            text.contains("model_reasoning_effort")
                && text.contains("low")
                && text.contains("medium")
                && text.contains("high")
        }) {
            CapabilitySupport::Verified
        } else {
            CapabilitySupport::Unsupported
        };

        let generated_schema = input.generated_schema.as_deref();
        if input
            .outputs
            .get(&ProbeCommandKind::AppServerSchema)
            .is_some_and(ProbeOutput::succeeded)
            && generated_schema.is_some_and(stable_app_server_schema_verified)
        {
            capabilities.app_server = CapabilitySupport::Verified;
            if generated_schema.is_some_and(stable_app_server_sandbox_verified) {
                capabilities.read_only_sandbox = CapabilitySupport::Verified;
                capabilities.workspace_write_sandbox = CapabilitySupport::Verified;
            }
            if generated_schema.is_some_and(stable_app_server_reasoning_verified) {
                capabilities.app_server_reasoning_effort = CapabilitySupport::Verified;
            }
        } else if generated_schema.is_some() {
            diagnostics.push(
                "generated App Server schema is missing required stable lifecycle fields"
                    .to_owned(),
            );
        }

        if generated_schema.is_some_and(|schema| {
            schema.contains("turn/completed")
                && (schema.contains("tokenUsage") || schema.contains("usage"))
        }) {
            capabilities.usage_events = CapabilitySupport::Advertised;
        }

        // This config key cannot be proven by a generic `-c` flag. Exact
        // contracts may advertise it; unknown versions safely omit it.
        let adapter = self.registry.select(capabilities.version.as_ref());
        if let Some(contract) = adapter.contract() {
            contract.enrich_capabilities(&mut capabilities);
        }
        capabilities.reasoning_effort = strongest_capability(
            capabilities.exec_reasoning_effort,
            capabilities.app_server_reasoning_effort,
        );

        for (name, support, source) in [
            ("exec", capabilities.exec, "codex exec --help"),
            (
                "jsonl_output",
                capabilities.jsonl_output,
                "codex exec --help",
            ),
            (
                "session_resume",
                capabilities.session_resume,
                "codex exec resume --help",
            ),
            (
                "output_schema",
                capabilities.output_schema,
                "codex exec --help",
            ),
            (
                "workspace_write_sandbox",
                capabilities.workspace_write_sandbox,
                "codex exec --help",
            ),
            (
                "read_only_sandbox",
                capabilities.read_only_sandbox,
                "codex exec --help",
            ),
            (
                "app_server",
                capabilities.app_server,
                "codex app-server schema",
            ),
            (
                "reasoning_effort",
                capabilities.reasoning_effort,
                "exec help/App Server schema",
            ),
            ("usage_events", capabilities.usage_events, "schema/contract"),
        ] {
            add_evidence(&mut evidence, name, support, source, format!("{support:?}"));
        }

        let exec_transport = capabilities.exec.is_available()
            && capabilities.jsonl_output.is_available()
            && capabilities.read_only_sandbox.is_available();
        let app_server_transport = capabilities.app_server == CapabilitySupport::Verified
            && capabilities.read_only_sandbox == CapabilitySupport::Verified;
        let status = if (!exec_transport && !app_server_transport) || capabilities.version.is_none()
        {
            diagnostics.push("no verified safe Codex transport is available".to_owned());
            CompatibilityStatus::Incompatible
        } else if matches!(adapter, AdapterSelection::GenericUntested) {
            diagnostics.push(
                "Codex version is untested; writable execution must remain disabled".to_owned(),
            );
            CompatibilityStatus::Untested
        } else if capabilities.app_server != CapabilitySupport::Verified
            || !capabilities.output_schema.is_available()
            || !capabilities.session_resume.is_available()
        {
            diagnostics.push(
                "optional capabilities are unavailable; graceful fallback applies".to_owned(),
            );
            CompatibilityStatus::CompatibleWithWarnings
        } else {
            CompatibilityStatus::Compatible
        };

        CodexProbeReport {
            capabilities,
            status,
            adapter,
            evidence,
            diagnostics,
        }
    }
}

fn successful_text(input: &CapabilityProbeInput, kind: ProbeCommandKind) -> Option<String> {
    input
        .outputs
        .get(&kind)
        .filter(|output| output.succeeded())
        .map(ProbeOutput::combined)
}

fn support_if(value: bool) -> CapabilitySupport {
    if value {
        CapabilitySupport::Advertised
    } else {
        CapabilitySupport::Unsupported
    }
}

fn contains_token(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .any(|token| token == needle)
}

fn stable_app_server_schema_verified(schema: &str) -> bool {
    [
        "initialize",
        "initialized",
        "thread/start",
        "thread/resume",
        "turn/start",
        "turn/completed",
        "item/started",
        "item/completed",
    ]
    .iter()
    .all(|required| schema.contains(required))
}

fn stable_app_server_sandbox_verified(schema: &str) -> bool {
    schema.contains("sandbox") && schema.contains("read-only") && schema.contains("workspace-write")
}

fn stable_app_server_reasoning_verified(schema: &str) -> bool {
    schema.contains("effort")
        && schema.contains("low")
        && schema.contains("medium")
        && schema.contains("high")
}

const fn strongest_capability(
    left: CapabilitySupport,
    right: CapabilitySupport,
) -> CapabilitySupport {
    use CapabilitySupport::{Advertised, Degraded, Unsupported, Verified};
    match (left, right) {
        (Verified, _) | (_, Verified) => Verified,
        (Advertised, _) | (_, Advertised) => Advertised,
        (Degraded, _) | (_, Degraded) => Degraded,
        (Unsupported, Unsupported) => Unsupported,
    }
}

fn parse_codex_version(output: &ProbeOutput) -> Option<Version> {
    if !output.succeeded() {
        return None;
    }
    output
        .combined()
        .split_ascii_whitespace()
        .find_map(|token| Version::parse(token.trim_start_matches('v')).ok())
}

fn add_evidence(
    target: &mut Vec<CapabilityEvidence>,
    capability: impl Into<String>,
    support: CapabilitySupport,
    source: impl Into<String>,
    detail: impl Into<String>,
) {
    target.push(CapabilityEvidence {
        capability: capability.into(),
        support,
        source: source.into(),
        detail: detail.into(),
    });
}

fn read_schema_tree(directory: &Path) -> Option<String> {
    let entries = std::fs::read_dir(directory).ok()?;
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<PathBuf>>();
    paths.sort();
    let mut combined = String::new();
    for path in paths {
        if let Ok(text) = std::fs::read_to_string(path) {
            combined.push_str(&text);
            combined.push('\n');
        }
    }
    (!combined.is_empty()).then_some(combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known_input(version: &str) -> CapabilityProbeInput {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            ProbeCommandKind::Version,
            ProbeOutput::success(format!("codex-cli {version}")),
        );
        outputs.insert(
            ProbeCommandKind::RootHelp,
            ProbeOutput::success("Commands: exec app-server"),
        );
        outputs.insert(
            ProbeCommandKind::ExecHelp,
            ProbeOutput::success(
                "--json --output-schema --sandbox [read-only|workspace-write] \
                 -c model_reasoning_effort=[low|medium|high]",
            ),
        );
        outputs.insert(
            ProbeCommandKind::ExecResumeHelp,
            ProbeOutput::success("Usage: codex exec resume [SESSION_ID]"),
        );
        outputs.insert(
            ProbeCommandKind::AppServerHelp,
            ProbeOutput::success("generate-json-schema --listen stdio://"),
        );
        outputs.insert(
            ProbeCommandKind::AppServerSchema,
            ProbeOutput::success(String::new()),
        );
        CapabilityProbeInput {
            outputs,
            generated_schema: Some(
                "initialize initialized thread/start thread/resume turn/start turn/completed \
                 item/started item/completed tokenUsage sandbox read-only workspace-write"
                    .to_owned(),
            ),
        }
    }

    #[test]
    fn exact_contract_is_compatible() {
        let report = CapabilityProbe::default().evaluate(&known_input("0.144.5"));
        assert_eq!(report.status, CompatibilityStatus::Compatible);
        assert!(matches!(report.adapter, AdapterSelection::Exact { .. }));
        assert_eq!(report.capabilities.app_server, CapabilitySupport::Verified);
        assert_eq!(report.capabilities.exec, CapabilitySupport::Verified);
        assert_eq!(
            report.capabilities.jsonl_output,
            CapabilitySupport::Verified
        );
        assert_eq!(
            report.capabilities.exec_reasoning_effort,
            CapabilitySupport::Verified
        );
    }

    #[test]
    fn unknown_version_is_untested_even_with_help_support() {
        let report = CapabilityProbe::default().evaluate(&known_input("9.0.0"));
        assert_eq!(report.status, CompatibilityStatus::Untested);
    }

    #[test]
    fn missing_jsonl_is_incompatible() {
        let mut input = known_input("0.144.5");
        input.outputs.insert(
            ProbeCommandKind::ExecHelp,
            ProbeOutput::success("--sandbox [read-only|workspace-write]"),
        );
        let report = CapabilityProbe::default().evaluate(&input);
        assert_eq!(report.status, CompatibilityStatus::CompatibleWithWarnings);
        assert_eq!(report.capabilities.app_server, CapabilitySupport::Verified);
    }

    #[test]
    fn version_contract_never_invents_reasoning_effort() {
        let mut input = known_input("0.144.5");
        input.outputs.insert(
            ProbeCommandKind::ExecHelp,
            ProbeOutput::success("--json --sandbox [read-only|workspace-write]"),
        );
        input.generated_schema = Some(
            "initialize initialized thread/start thread/resume turn/start turn/completed \
             item/started item/completed tokenUsage sandbox read-only workspace-write"
                .to_owned(),
        );
        let report = CapabilityProbe::default().evaluate(&input);
        assert_eq!(
            report.capabilities.reasoning_effort,
            CapabilitySupport::Unsupported
        );
    }
}
