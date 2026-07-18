use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{CapabilitySupport, CodexCapabilities, CompatibilityError};

/// Public Codex transports supported by the compatibility layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexTransport {
    ExecJsonl,
    AppServerStdio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexSandbox {
    ReadOnly,
    WorkspaceWrite,
}

impl CodexSandbox {
    #[must_use]
    pub const fn as_cli_value(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    #[must_use]
    pub const fn as_cli_value(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Provider-neutral inputs needed to construct a Codex invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRequest {
    pub working_directory: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<ReasoningEffort>,
    pub sandbox: CodexSandbox,
    pub resume_session: Option<String>,
    pub output_schema: Option<PathBuf>,
}

/// A shell-free invocation. The task is always supplied through stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexInvocation {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub stdin: Vec<u8>,
    pub working_directory: PathBuf,
    pub transport: CodexTransport,
}

impl CodexInvocation {
    /// Builds a capability-gated `codex exec --json` invocation.
    ///
    /// # Errors
    ///
    /// Returns [`CompatibilityError::NoSupportedTransport`] when exec JSONL
    /// was not observed, or [`CompatibilityError::UnsupportedOption`] when the
    /// requested sandbox mode was not observed.
    pub fn exec(
        executable: impl AsRef<Path>,
        request: &CodexRequest,
        capabilities: &CodexCapabilities,
    ) -> Result<Self, CompatibilityError> {
        let mut args = vec!["exec".to_owned()];

        if let Some(session) = request
            .resume_session
            .as_ref()
            .filter(|_| capabilities.session_resume.is_available())
        {
            args.push("resume".to_owned());
            args.push(session.clone());
        }

        if !capabilities.exec.is_available() || !capabilities.jsonl_output.is_available() {
            return Err(CompatibilityError::NoSupportedTransport);
        }
        let sandbox_support = match request.sandbox {
            CodexSandbox::ReadOnly => capabilities.read_only_sandbox,
            CodexSandbox::WorkspaceWrite => capabilities.workspace_write_sandbox,
        };
        if !sandbox_support.is_available() {
            return Err(CompatibilityError::UnsupportedOption {
                option: request.sandbox.as_cli_value(),
            });
        }

        args.push("--json".to_owned());
        args.push("--sandbox".to_owned());
        args.push(request.sandbox.as_cli_value().to_owned());
        args.push("-C".to_owned());
        args.push(request.working_directory.to_string_lossy().into_owned());

        if let Some(model) = request.model.as_ref().filter(|model| !model.is_empty()) {
            args.push("--model".to_owned());
            args.push(model.clone());
        }

        if let Some(effort) = request
            .effort
            .filter(|_| capabilities.exec_reasoning_effort.is_available())
        {
            args.push("-c".to_owned());
            args.push(format!(
                "model_reasoning_effort=\"{}\"",
                effort.as_cli_value()
            ));
        }

        if let Some(schema) = request
            .output_schema
            .as_ref()
            .filter(|_| capabilities.output_schema.is_available())
        {
            args.push("--output-schema".to_owned());
            args.push(schema.to_string_lossy().into_owned());
        }

        // A literal '-' makes stdin use explicit and keeps the task out of argv.
        args.push("-".to_owned());

        Ok(Self {
            executable: executable.as_ref().to_path_buf(),
            args,
            stdin: request.prompt.as_bytes().to_vec(),
            working_directory: request.working_directory.clone(),
            transport: CodexTransport::ExecJsonl,
        })
    }

    #[must_use]
    pub fn app_server(executable: impl AsRef<Path>, request: &CodexRequest) -> Self {
        Self {
            executable: executable.as_ref().to_path_buf(),
            args: vec![
                "app-server".to_owned(),
                "--listen".to_owned(),
                "stdio://".to_owned(),
            ],
            stdin: Vec::new(),
            working_directory: request.working_directory.clone(),
            transport: CodexTransport::AppServerStdio,
        }
    }
}

/// The public CLI JSONL contract is preferred. Stable App Server stdio is the
/// fallback only when exec JSONL is unavailable and its schema was verified.
#[must_use]
pub fn select_transport(capabilities: &CodexCapabilities) -> Option<CodexTransport> {
    if capabilities.exec.is_available() && capabilities.jsonl_output.is_available() {
        Some(CodexTransport::ExecJsonl)
    } else if capabilities.app_server == CapabilitySupport::Verified {
        Some(CodexTransport::AppServerStdio)
    } else {
        None
    }
}

/// Chooses the alternate public transport after a transport-specific failure.
/// It never retries the failed transport and never enables an unverified App
/// Server contract.
#[must_use]
pub fn fallback_transport(
    capabilities: &CodexCapabilities,
    failed: CodexTransport,
) -> Option<CodexTransport> {
    match failed {
        CodexTransport::AppServerStdio
            if capabilities.exec == CapabilitySupport::Verified
                && capabilities.jsonl_output == CapabilitySupport::Verified =>
        {
            Some(CodexTransport::ExecJsonl)
        }
        CodexTransport::ExecJsonl if capabilities.app_server == CapabilitySupport::Verified => {
            Some(CodexTransport::AppServerStdio)
        }
        CodexTransport::ExecJsonl | CodexTransport::AppServerStdio => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn exec_omits_empty_model_and_unsupported_options() -> Result<(), CompatibilityError> {
        let request = CodexRequest {
            working_directory: PathBuf::from("repo"),
            prompt: "do the task".to_owned(),
            model: Some(String::new()),
            effort: Some(ReasoningEffort::High),
            sandbox: CodexSandbox::ReadOnly,
            resume_session: Some("thread-1".to_owned()),
            output_schema: Some(PathBuf::from("schema.json")),
        };
        let capabilities = CodexCapabilities::default();

        let capabilities = CodexCapabilities {
            exec: CapabilitySupport::Advertised,
            jsonl_output: CapabilitySupport::Advertised,
            read_only_sandbox: CapabilitySupport::Advertised,
            ..capabilities
        };
        let invocation = CodexInvocation::exec("codex", &request, &capabilities)?;

        assert_eq!(invocation.args[0], "exec");
        assert!(invocation.args.contains(&"--json".to_owned()));
        assert!(!invocation.args.contains(&"--model".to_owned()));
        assert!(!invocation.args.contains(&"resume".to_owned()));
        assert!(!invocation.args.contains(&"--output-schema".to_owned()));
        assert!(!invocation.args.iter().any(|arg| arg.contains("reasoning")));
        assert_eq!(invocation.args.last().map(String::as_str), Some("-"));
        assert_eq!(invocation.stdin, b"do the task");
        Ok(())
    }

    #[test]
    fn exec_refuses_unverified_requested_sandbox_mode() {
        let request = CodexRequest {
            working_directory: PathBuf::from("repo"),
            prompt: "do the task".to_owned(),
            model: None,
            effort: None,
            sandbox: CodexSandbox::WorkspaceWrite,
            resume_session: None,
            output_schema: None,
        };
        let capabilities = CodexCapabilities {
            exec: CapabilitySupport::Verified,
            jsonl_output: CapabilitySupport::Verified,
            read_only_sandbox: CapabilitySupport::Verified,
            workspace_write_sandbox: CapabilitySupport::Unsupported,
            ..CodexCapabilities::default()
        };
        assert!(matches!(
            CodexInvocation::exec("codex", &request, &capabilities),
            Err(CompatibilityError::UnsupportedOption {
                option: "workspace-write"
            })
        ));
    }

    #[test]
    fn app_server_failure_falls_back_to_exec_jsonl() {
        let capabilities = CodexCapabilities {
            exec: CapabilitySupport::Verified,
            jsonl_output: CapabilitySupport::Verified,
            app_server: CapabilitySupport::Verified,
            ..CodexCapabilities::default()
        };
        assert_eq!(
            fallback_transport(&capabilities, CodexTransport::AppServerStdio),
            Some(CodexTransport::ExecJsonl)
        );
    }

    #[test]
    fn app_server_never_falls_back_to_merely_advertised_exec() {
        let capabilities = CodexCapabilities {
            exec: CapabilitySupport::Advertised,
            jsonl_output: CapabilitySupport::Advertised,
            app_server: CapabilitySupport::Verified,
            ..CodexCapabilities::default()
        };
        assert_eq!(
            fallback_transport(&capabilities, CodexTransport::AppServerStdio),
            None
        );
    }
}
