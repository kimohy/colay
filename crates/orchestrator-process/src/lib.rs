//! Hardened, shell-free subprocess execution for provider CLIs and Git.
#![allow(clippy::missing_errors_doc)]
#![cfg_attr(test, allow(clippy::panic))]

mod git;
mod jsonl;
mod redaction;
mod runner;

pub use git::{GitCommandBuilder, GitSafetyError, resolve_repo_path};
pub use jsonl::{JsonLines, MalformedJsonLine, parse_json_lines};
pub use redaction::{RedactionConfig, RedactionError, Redactor};
pub use runner::{
    CapturedOutput, CommandSpec, EnvironmentPolicy, OutputChannel, ProcessError, ProcessEvent,
    ProcessInput, ProcessResult, ProcessRunner, ProcessSession, ProcessSupervisor,
    TerminationReason,
};
