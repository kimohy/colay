//! Provider adapters for official non-interactive CLI interfaces.
//!
//! Adapters construct shell-free invocations and normalize structured events.
//! Process ownership is injected through [`AdapterRuntime`], keeping credentials
//! inside each already-authenticated Enterprise CLI.

mod adapter;
mod claude;
mod codex;
mod error;
mod gemini;
mod normalize;
mod process_runtime;
mod usage_probe;

pub use adapter::{
    AdapterRuntime, PreparedInvocation, RuntimeOutput, RuntimeTermination, StructuredOutput,
    WorkerAdapter,
};
pub use claude::{ClaudeAdapter, ClaudeAdapterConfig};
pub use codex::{
    CodexAdapter, CodexAdapterConfig, CodexTransportFeatures, CodexTransportPreference,
};
pub use error::ProviderError;
pub use gemini::{GeminiAdapter, GeminiAdapterConfig};
pub use normalize::{classify_provider_quota, parse_claude_event, parse_gemini_event};
pub use process_runtime::ProcessAdapterRuntime;
pub use usage_probe::{
    UsageProbeConfig, UsageProbeFormat, parse_usage_probe_output, unknown_usage,
};
