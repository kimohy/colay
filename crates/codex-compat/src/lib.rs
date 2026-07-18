//! Compatibility boundary for the public Codex CLI and App Server protocols.
//!
//! This crate deliberately does not depend on Codex Rust internals. Everything
//! accepted here is observable through stable command help, generated App
//! Server schemas, or public JSONL streams.

mod adapter;
mod app_server;
mod capability;
mod event;
mod invocation;
mod registry;

pub use adapter::{CodexCompatibilityAdapter, GenericCodexAdapter, RawCodexEvent};
pub use app_server::{
    AppServerClientInfo, AppServerError, AppServerId, AppServerMessage, AppServerNotification,
    AppServerRequest, AppServerResponse, AppServerSessionPlan, AppServerStep,
    DEFAULT_MAX_APP_SERVER_MESSAGE_BYTES, StableAppServerClient, StableAppServerSession, TextInput,
};
pub use capability::{
    CapabilityEvidence, CapabilityProbe, CapabilityProbeInput, CapabilitySource, CapabilitySupport,
    CodexCapabilities, CodexProbeReport, CompatibilityStatus, ProbeCommand, ProbeCommandKind,
    ProbeOutput,
};
pub use event::{
    CodexEventParser, CodexItem, CodexItemPhase, CodexUsage, CompatEvent, CompatibilityError,
    QuotaErrorKind, classify_quota_error,
};
pub use invocation::{
    CodexInvocation, CodexRequest, CodexSandbox, CodexTransport, ReasoningEffort,
    fallback_transport, select_transport,
};
pub use registry::{AdapterSelection, CompatibilityRegistry, VersionContract, VersionPolicy};
