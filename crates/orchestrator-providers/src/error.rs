use std::path::PathBuf;

use codex_compat::{AppServerError, CompatibilityError};
use orchestrator_domain::{ProviderId, RepoPathError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("request targets {actual:?}, but adapter is {expected:?}")]
    WrongProvider {
        expected: ProviderId,
        actual: ProviderId,
    },
    #[error("provider executable is empty")]
    EmptyExecutable,
    #[error("unsafe configured argument contains a NUL byte")]
    NulArgument,
    #[error("provider runtime is unavailable: {0}")]
    Runtime(String),
    #[error("provider probe failed: {0}")]
    Probe(String),
    #[error("invalid UTF-8 from provider")]
    InvalidUtf8,
    #[error("structured provider output is malformed: {0}")]
    MalformedOutput(String),
    #[error("Codex compatibility error: {0}")]
    CodexCompatibility(#[from] CompatibilityError),
    #[error("Codex App Server error: {0}")]
    AppServer(#[from] AppServerError),
    #[error("unsafe provider-reported repository path: {0}")]
    UnsafePath(#[from] RepoPathError),
    #[error("usage probe executable is empty")]
    EmptyUsageProbeExecutable,
    #[error("usage probe failed: {0}")]
    UsageProbe(String),
    #[error("usage probe JSON is invalid: {0}")]
    UsageProbeJson(String),
    #[error("usage probe returned an unsupported format")]
    UnsupportedUsageProbeFormat,
    #[error("usage probe returned invalid path {0}")]
    InvalidProbePath(PathBuf),
    #[error("worker checkpoint is unavailable: {0}")]
    Checkpoint(String),
}
