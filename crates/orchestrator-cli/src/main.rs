mod app;
mod args;
mod chat_tui;
mod daemon;
mod ipc_client;
mod profile_config;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::Parser as _;

#[tokio::main]
async fn main() {
    let arguments = args::Cli::parse();
    if let Err(error) = Box::pin(run(arguments)).await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run(arguments: args::Cli) -> Result<()> {
    let repository = current_repository()?;
    if matches!(
        &arguments.command,
        args::Command::Daemon(args::DaemonArgs {
            action: args::DaemonAction::Serve,
        })
    ) {
        return daemon::serve_global(&repository, arguments.config.as_deref()).await;
    }
    app::run(arguments).await
}

fn current_repository() -> Result<PathBuf> {
    let current = std::env::current_dir().context("cannot determine current directory")?;
    std::fs::canonicalize(&current).with_context(|| {
        format!(
            "cannot canonicalize current directory: {}",
            current.display()
        )
    })
}
