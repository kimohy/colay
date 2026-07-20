use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use orchestrator_domain::ProviderId;

const fn selected_build_version(override_version: Option<&'static str>) -> &'static str {
    match override_version {
        Some(version) => version,
        None => env!("CARGO_PKG_VERSION"),
    }
}

const COLAY_VERSION: &str = selected_build_version(option_env!("COLAY_BUILD_VERSION"));

#[derive(Clone, Debug, Parser)]
#[command(
    name = "colay",
    version = COLAY_VERSION,
    about = "Local Enterprise multi-provider coding-agent relay",
    long_about = None
)]
pub struct Cli {
    /// Highest-precedence versioned Colay TOML configuration override.
    /// Relative paths resolve from the repository; otherwise layered discovery is used.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Emit a stable machine-readable JSON document.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Create local state and an editable configuration without invoking providers.
    Init(InitArgs),
    /// Analyze, route, and run one development task.
    Run(RunArgs),
    /// Show task state, or all recent task states.
    Status(TaskSelector),
    /// Show enabled providers and detected safe interfaces.
    Providers(ProviderArgs),
    /// Inspect or administratively override provider model profiles.
    Profiles(ProfileArgs),
    /// Inspect or administratively override usage evidence.
    Usage(UsageArgs),
    /// Request a safe checkpoint and provider handover.
    Handover(HandoverArgs),
    /// Pause a task at its next safe checkpoint.
    Pause(RequiredTask),
    /// Resume a checkpointed or blocked task.
    Resume(RequiredTask),
    /// Cancel a task after a safe checkpoint.
    Cancel(RequiredTask),
    /// Explain the complete recorded routing score.
    ExplainRouting(RequiredTask),
    /// Show the newest integrity-verified vendor-neutral checkpoint.
    Checkpoint(RequiredTask),
    /// Run non-inference configuration, binary, database, and compatibility checks.
    Doctor,
    /// Probe public Codex interfaces without starting a model turn.
    Compatibility,
    /// Inspect and apply sequential SQLite/config migrations.
    Migrate(MigrationArgs),
    /// Plan or explicitly approve recovery from versioned backups.
    Rollback(RollbackArgs),
    /// Open the local five-panel terminal dashboard.
    Tui(TaskSelector),
}

#[derive(Clone, Debug, Args)]
pub struct InitArgs {
    /// Repository whose local .colay state directory will be initialized.
    #[arg(long, default_value = ".")]
    pub repository: PathBuf,
}

#[derive(Clone, Debug, Args)]
pub struct RunArgs {
    /// Task text. Mutually exclusive with --task-file.
    #[arg(value_name = "TASK", required_unless_present = "task_file")]
    pub task: Option<String>,
    /// Versioned JSON task envelope input.
    #[arg(long, conflicts_with = "task")]
    pub task_file: Option<PathBuf>,
    /// Force one approved provider while retaining all quality and safety gates.
    #[arg(long, value_enum)]
    pub provider: Option<ProviderName>,
    /// Analyze and route without creating a worktree or invoking a provider.
    #[arg(long)]
    pub plan_only: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct TaskSelector {
    #[arg(value_name = "TASK_ID")]
    pub task_id: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct RequiredTask {
    pub task_id: String,
}

#[derive(Clone, Debug, Args)]
pub struct HandoverArgs {
    pub task_id: String,
    #[arg(long, value_enum)]
    pub to: ProviderName,
}

#[derive(Clone, Debug, Default, Args)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub action: Option<ProviderAction>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum ProviderAction {
    /// Enable an administrator-configured provider.
    Enable { provider: ProviderName },
    /// Disable a provider without deleting its usage or audit history.
    Disable { provider: ProviderName },
}

#[derive(Clone, Debug, Args)]
pub struct UsageArgs {
    #[command(subcommand)]
    pub action: Option<UsageAction>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum UsageAction {
    /// Record an administrator-supplied value as explicit manual evidence.
    Override(UsageOverrideArgs),
}

#[derive(Clone, Debug, Args)]
pub struct UsageOverrideArgs {
    #[arg(value_enum)]
    pub provider: ProviderName,
    #[arg(long)]
    pub used: Option<f64>,
    #[arg(long)]
    pub limit: Option<f64>,
    #[arg(long)]
    pub remaining: Option<f64>,
    /// Audit identity; this is a human/admin label, never a credential.
    #[arg(long)]
    pub entered_by: String,
}

#[derive(Clone, Debug, Args)]
pub struct MigrationArgs {
    #[command(subcommand)]
    pub action: MigrationAction,
}

#[derive(Clone, Debug, Subcommand)]
pub enum MigrationAction {
    Status,
    Plan,
    Apply {
        #[arg(long)]
        dry_run: bool,
    },
    /// Plan or explicitly approve restoration of a sealed `SQLite` backup.
    Rollback(MigrationRollbackArgs),
}

#[derive(Clone, Debug, Args)]
pub struct MigrationRollbackArgs {
    #[command(subcommand)]
    pub action: MigrationRollbackAction,
}

#[derive(Clone, Debug, Subcommand)]
pub enum MigrationRollbackAction {
    /// Seal and persist a rollback plan without replacing the live database.
    Plan {
        /// Prior `SQLite` backup; defaults to the newest migration backup.
        #[arg(long)]
        backup: Option<PathBuf>,
    },
    /// Apply one previously persisted plan after an explicit plan-bound approval.
    Apply {
        /// Integrity hash printed by `migrate rollback plan`.
        #[arg(long)]
        plan_hash: String,
        /// Required human/admin audit identity; never a credential.
        #[arg(long)]
        approved_by: String,
    },
}

#[derive(Clone, Debug, Args)]
pub struct RollbackArgs {
    #[command(subcommand)]
    pub action: RollbackAction,
}

#[derive(Clone, Debug, Subcommand)]
pub enum RollbackAction {
    Plan {
        #[arg(long)]
        to: String,
    },
    Apply {
        #[arg(long)]
        to: String,
        /// Integrity hash printed by a prior `rollback plan` invocation.
        #[arg(long)]
        plan_hash: String,
        /// Required explicit approval identity recorded in the audit log.
        #[arg(long)]
        approved_by: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProviderName {
    Gemini,
    Codex,
    Claude,
}

impl From<ProviderName> for ProviderId {
    fn from(value: ProviderName) -> Self {
        match value {
            ProviderName::Gemini => Self::Gemini,
            ProviderName::Codex => Self::Codex,
            ProviderName::Claude => Self::Claude,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_uses_the_selected_build_version() {
        assert_eq!(
            selected_build_version(Some("0.1.1-nightly.20260719.a1b2c3d")),
            "0.1.1-nightly.20260719.a1b2c3d"
        );
        assert_eq!(Cli::command().get_version(), Some(COLAY_VERSION));
    }

    #[test]
    fn parses_migration_rollback_plan_with_explicit_backup() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from([
            "colay", "migrate", "rollback", "plan", "--backup", "prior.db",
        ])?;
        assert!(matches!(
            cli.command,
            Command::Migrate(MigrationArgs {
                action: MigrationAction::Rollback(MigrationRollbackArgs {
                    action: MigrationRollbackAction::Plan { backup: Some(_) }
                })
            })
        ));
        Ok(())
    }

    #[test]
    fn parses_plan_bound_migration_rollback_approval() -> Result<(), clap::Error> {
        let expected_hash = "a".repeat(64);
        let cli = Cli::try_parse_from([
            "colay",
            "migrate",
            "rollback",
            "apply",
            "--plan-hash",
            expected_hash.as_str(),
            "--approved-by",
            "enterprise-admin",
        ])?;
        assert!(matches!(
            cli.command,
            Command::Migrate(MigrationArgs {
                action: MigrationAction::Rollback(MigrationRollbackArgs {
                    action: MigrationRollbackAction::Apply {
                        plan_hash,
                        approved_by
                    }
                })
            }) if plan_hash.len() == 64 && approved_by == "enterprise-admin"
        ));
        Ok(())
    }

    #[test]
    fn parses_profile_set_with_versioned_model_and_effort() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from([
            "colay",
            "profiles",
            "set",
            "claude",
            "premium",
            "--model",
            "claude-fable-5",
            "--effort",
            "high",
        ])?;
        assert!(matches!(
            cli.command,
            Command::Profiles(ProfileArgs {
                action: Some(ProfileAction::Set(ProfileSetArgs {
                    provider: ProviderName::Claude,
                    profile: ProfileName::Premium,
                    model,
                    effort: Some(EffortName::High),
                }))
            }) if model == "claude-fable-5"
        ));
        Ok(())
    }

    #[test]
    fn parses_profile_reset_target() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["colay", "profiles", "reset", "gemini", "standard"])?;
        assert!(matches!(
            cli.command,
            Command::Profiles(ProfileArgs {
                action: Some(ProfileAction::Reset(ProfileTargetArgs {
                    provider: ProviderName::Gemini,
                    profile: ProfileName::Standard,
                }))
            })
        ));
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Args)]
pub struct ProfileArgs {
    #[command(subcommand)]
    pub action: Option<ProfileAction>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum ProfileAction {
    /// Override one effective provider profile in the selected writable config layer.
    Set(ProfileSetArgs),
    /// Remove one override and reveal the next lower-precedence value.
    Reset(ProfileTargetArgs),
}

#[derive(Clone, Debug, Args)]
pub struct ProfileSetArgs {
    #[arg(value_enum)]
    pub provider: ProviderName,
    #[arg(value_enum)]
    pub profile: ProfileName,
    #[arg(long)]
    pub model: String,
    #[arg(long, value_enum)]
    pub effort: Option<EffortName>,
}

#[derive(Clone, Debug, Args)]
pub struct ProfileTargetArgs {
    #[arg(value_enum)]
    pub provider: ProviderName,
    #[arg(value_enum)]
    pub profile: ProfileName,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProfileName {
    Economy,
    Standard,
    Premium,
}

impl ProfileName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Economy => "economy",
            Self::Standard => "standard",
            Self::Premium => "premium",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum EffortName {
    Low,
    Medium,
    High,
}

impl EffortName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}
