mod app;
mod args;

use clap::Parser as _;

#[tokio::main]
async fn main() {
    let arguments = args::Cli::parse();
    if let Err(error) = Box::pin(app::run(arguments)).await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
