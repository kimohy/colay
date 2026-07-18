use std::path::{Path, PathBuf};

use semver::Version;

use crate::{
    CodexCapabilities, CodexEventParser, CodexInvocation, CodexRequest, CodexTransport, CodexUsage,
    CompatEvent, CompatibilityError, select_transport,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCodexEvent {
    pub line_number: usize,
    pub line: String,
}

/// Public-interface-only Codex compatibility boundary.
pub trait CodexCompatibilityAdapter: Send + Sync {
    fn detected_version(&self) -> Option<&Version>;
    fn capabilities(&self) -> &CodexCapabilities;

    /// Builds a shell-free invocation supported by the observed capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`CompatibilityError`] when neither CLI JSONL nor stable App
    /// Server stdio is available.
    fn build_request(&self, request: &CodexRequest) -> Result<CodexInvocation, CompatibilityError>;

    /// Parses one raw public protocol event.
    ///
    /// # Errors
    ///
    /// Returns [`CompatibilityError`] for malformed or lifecycle-incompatible
    /// events.
    fn parse_event(&self, event: &RawCodexEvent) -> Result<CompatEvent, CompatibilityError>;

    /// Extracts local token evidence without interpreting it as provider quota.
    ///
    /// # Errors
    ///
    /// Returns [`CompatibilityError`] if the underlying event is incompatible.
    fn normalize_usage(
        &self,
        event: &RawCodexEvent,
    ) -> Result<Option<CodexUsage>, CompatibilityError>;

    fn supports_resume(&self) -> bool;
    fn supports_output_schema(&self) -> bool;
    fn supports_app_server(&self) -> bool;
}

#[derive(Debug, Clone)]
pub struct GenericCodexAdapter {
    executable: PathBuf,
    capabilities: CodexCapabilities,
    parser: CodexEventParser,
}

impl GenericCodexAdapter {
    #[must_use]
    pub fn new(executable: impl AsRef<Path>, capabilities: CodexCapabilities) -> Self {
        Self {
            executable: executable.as_ref().to_path_buf(),
            capabilities,
            parser: CodexEventParser,
        }
    }
}

impl CodexCompatibilityAdapter for GenericCodexAdapter {
    fn detected_version(&self) -> Option<&Version> {
        self.capabilities.version.as_ref()
    }

    fn capabilities(&self) -> &CodexCapabilities {
        &self.capabilities
    }

    fn build_request(&self, request: &CodexRequest) -> Result<CodexInvocation, CompatibilityError> {
        match select_transport(&self.capabilities) {
            Some(CodexTransport::ExecJsonl) => {
                CodexInvocation::exec(&self.executable, request, &self.capabilities)
            }
            Some(CodexTransport::AppServerStdio) => {
                Ok(CodexInvocation::app_server(&self.executable, request))
            }
            None => Err(CompatibilityError::NoSupportedTransport),
        }
    }

    fn parse_event(&self, event: &RawCodexEvent) -> Result<CompatEvent, CompatibilityError> {
        self.parser.parse_line(event.line_number, &event.line)
    }

    fn normalize_usage(
        &self,
        event: &RawCodexEvent,
    ) -> Result<Option<CodexUsage>, CompatibilityError> {
        let event = self.parse_event(event)?;
        Ok(match event {
            CompatEvent::TurnCompleted { usage } => Some(usage),
            _ => None,
        })
    }

    fn supports_resume(&self) -> bool {
        self.capabilities.session_resume.is_available()
    }

    fn supports_output_schema(&self) -> bool {
        self.capabilities.output_schema.is_available()
    }

    fn supports_app_server(&self) -> bool {
        self.capabilities.app_server.is_available()
    }
}

#[cfg(test)]
mod tests {
    use crate::CapabilitySupport;

    use super::*;

    #[test]
    fn generic_adapter_fails_without_public_transport() {
        let adapter = GenericCodexAdapter::new("codex", CodexCapabilities::default());
        let request = CodexRequest {
            working_directory: PathBuf::from("repo"),
            prompt: "inspect".to_owned(),
            model: None,
            effort: None,
            sandbox: crate::CodexSandbox::ReadOnly,
            resume_session: None,
            output_schema: None,
        };
        assert!(matches!(
            adapter.build_request(&request),
            Err(CompatibilityError::NoSupportedTransport)
        ));
    }

    #[test]
    fn exec_has_priority_over_app_server() {
        let capabilities = CodexCapabilities {
            exec: CapabilitySupport::Verified,
            jsonl_output: CapabilitySupport::Verified,
            app_server: CapabilitySupport::Verified,
            read_only_sandbox: CapabilitySupport::Verified,
            ..CodexCapabilities::default()
        };
        let adapter = GenericCodexAdapter::new("codex", capabilities);
        let request = CodexRequest {
            working_directory: PathBuf::from("repo"),
            prompt: "inspect".to_owned(),
            model: None,
            effort: None,
            sandbox: crate::CodexSandbox::ReadOnly,
            resume_session: None,
            output_schema: None,
        };
        assert!(matches!(
            adapter.build_request(&request),
            Ok(CodexInvocation {
                transport: CodexTransport::ExecJsonl,
                ..
            })
        ));
    }
}
