//! Fake-only provider test harness.
//!
//! The harness rejects any executable other than the compiled
//! `fake-provider-cli`, preventing accidental Enterprise quota consumption.

mod runtime;

pub use runtime::{FakeAdapterRuntime, FakeRuntimeScenario, fake_conversation_capability};

/// Entry point shared by the fixture binary and subprocess integration tests.
pub fn fake_cli_main<I>(args: I)
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    runtime::run_fake_cli(args, None);
}

/// Entry point for compatibility tests that need a future Codex version.
pub fn future_codex_fake_cli_main<I>(args: I)
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    runtime::run_fake_cli(args, Some("0.145.0"));
}
