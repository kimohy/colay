use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use codex_compat::AppServerSessionPlan;
use orchestrator_domain::{
    CancelResult, ProviderCapabilities, ProviderHealth, ProviderId, RawEvent, UntrustedWorkerClaim,
    UsageSnapshot, WorkerEvent, WorkerHandle, WorkerRequest,
};

use crate::ProviderError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredOutput {
    AgyText,
    CodexJsonl,
    CodexAppServerStdio,
    ClaudeStreamJson,
    GeminiStreamJson,
    UsageJson,
}

/// Complete shell-free process specification emitted by an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedInvocation {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub stdin: Vec<u8>,
    pub working_directory: PathBuf,
    pub timeout_seconds: u64,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
    pub output: StructuredOutput,
    /// Present only for the verified stable Codex App Server stdio transport.
    pub codex_app_server: Option<AppServerSessionPlan>,
    /// A single pre-validated alternate invocation. Nested fallback chains are
    /// rejected so a task can never loop between transports.
    pub fallback: Option<Box<Self>>,
}

impl PreparedInvocation {
    /// Checks invariants that can be validated without starting a process.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] for an empty executable or a NUL-containing
    /// argument.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.executable.as_os_str().is_empty() {
            return Err(ProviderError::EmptyExecutable);
        }
        if self
            .args
            .iter()
            .any(|arg| arg.to_string_lossy().contains('\0'))
        {
            return Err(ProviderError::NulArgument);
        }
        match (self.output, self.codex_app_server.is_some()) {
            (StructuredOutput::CodexAppServerStdio, true)
            | (
                StructuredOutput::AgyText
                | StructuredOutput::CodexJsonl
                | StructuredOutput::ClaudeStreamJson
                | StructuredOutput::GeminiStreamJson
                | StructuredOutput::UsageJson,
                false,
            ) => {}
            _ => {
                return Err(ProviderError::Runtime(
                    "prepared stdio protocol metadata does not match its output type".to_owned(),
                ));
            }
        }
        if let Some(plan) = &self.codex_app_server {
            plan.validate()?;
        }
        if let Some(fallback) = &self.fallback {
            if fallback.fallback.is_some() {
                return Err(ProviderError::Runtime(
                    "nested provider transport fallback is forbidden".to_owned(),
                ));
            }
            fallback.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn args_lossy(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeTermination {
    Exited,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeOutput {
    /// Exact generic process-boundary identity for the process that produced
    /// this output. Worker completions persist it outside the domain contract.
    pub resolved_executable: Option<orchestrator_process::ResolvedExecutable>,
    pub exit_code: Option<i32>,
    pub termination: RuntimeTermination,
    /// Present when the direct child was reaped but the process layer could
    /// not confirm termination of every descendant. Callers must treat this
    /// as an uncertain, fail-closed termination rather than successful
    /// cleanup.
    pub tree_termination_error: Option<String>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

/// Runtime port implemented by the process layer. Tests inject a fake runtime;
/// adapters never invoke a provider binary directly.
#[async_trait]
pub trait AdapterRuntime: Send + Sync {
    async fn run_probe(
        &self,
        executable: &Path,
        args: &[OsString],
    ) -> Result<RuntimeOutput, ProviderError>;

    async fn start_worker(
        &self,
        provider: ProviderId,
        request: &WorkerRequest,
        invocation: PreparedInvocation,
    ) -> Result<WorkerHandle, ProviderError>;

    /// Returns the next structured stdout/stderr/protocol frame while the
    /// worker is running. `None` means the event stream is closed.
    async fn next_event(&self, handle: &WorkerHandle) -> Result<Option<RawEvent>, ProviderError>;

    /// Waits for process termination and returns bounded, redacted output.
    async fn wait(&self, handle: &WorkerHandle) -> Result<RuntimeOutput, ProviderError>;

    async fn checkpoint(
        &self,
        handle: &WorkerHandle,
    ) -> Result<UntrustedWorkerClaim, ProviderError>;

    async fn cancel(&self, handle: &WorkerHandle) -> Result<CancelResult, ProviderError>;

    async fn run_usage_probe(
        &self,
        invocation: PreparedInvocation,
    ) -> Result<RuntimeOutput, ProviderError>;
}

pub type SharedRuntime = Arc<dyn AdapterRuntime>;

#[async_trait]
pub trait WorkerAdapter: Send + Sync {
    fn provider(&self) -> ProviderId;
    async fn probe(&self) -> Result<ProviderHealth, ProviderError>;
    async fn capabilities(&self) -> Result<ProviderCapabilities, ProviderError>;
    async fn collect_usage(&self) -> Result<Vec<UsageSnapshot>, ProviderError>;
    /// Builds a conservative shell-free invocation without probing.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the request targets another provider or
    /// contains an invalid process specification.
    fn prepare(&self, request: &WorkerRequest) -> Result<PreparedInvocation, ProviderError>;
    async fn start(&self, request: WorkerRequest) -> Result<WorkerHandle, ProviderError>;
    async fn next_event(&self, handle: &WorkerHandle) -> Result<Option<RawEvent>, ProviderError>;
    async fn wait(&self, handle: &WorkerHandle) -> Result<RuntimeOutput, ProviderError>;
    async fn checkpoint(
        &self,
        handle: &WorkerHandle,
    ) -> Result<UntrustedWorkerClaim, ProviderError>;
    async fn cancel(&self, handle: &WorkerHandle) -> Result<CancelResult, ProviderError>;
    async fn parse_event(&self, event: RawEvent) -> Result<WorkerEvent, ProviderError>;
}

pub(crate) fn ensure_provider(
    request: &WorkerRequest,
    expected: ProviderId,
) -> Result<(), ProviderError> {
    if request.provider == expected {
        Ok(())
    } else {
        Err(ProviderError::WrongProvider {
            expected,
            actual: request.provider,
        })
    }
}

pub(crate) fn prompt_payload(request: &WorkerRequest) -> Result<Vec<u8>, ProviderError> {
    #[derive(serde::Serialize)]
    struct BridgePayload<'a> {
        schema_version: &'static str,
        objective: &'a str,
        task: &'a str,
        constraints: &'a [String],
        acceptance_criteria: &'a [String],
        handover: &'a Option<serde_json::Value>,
    }

    serde_json::to_vec(&BridgePayload {
        schema_version: "1",
        objective: &request.objective,
        task: &request.prompt,
        constraints: &request.constraints,
        acceptance_criteria: &request.acceptance_criteria,
        handover: &request.handover_payload,
    })
    .map_err(|error| ProviderError::MalformedOutput(error.to_string()))
}

pub(crate) fn output_limits(request: &WorkerRequest) -> (usize, usize) {
    let stdout = usize::try_from(request.max_output_bytes).unwrap_or(usize::MAX);
    let stderr = (stdout / 2).max(1024 * 1024);
    (stdout, stderr)
}
