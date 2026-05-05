use clap::{Parser, Subcommand};
use sessync::commands;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "sessync",
    version,
    about = "Cross-device sync for Claude Code sessions"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive first-time setup.
    Init {
        /// Use the local-fs storage backend instead of OSS (for smoke testing).
        #[arg(long)]
        mock: bool,
    },
    /// Install the sessync binary into PATH (one-step macOS deploy).
    Install {
        /// Where to install the binary. Defaults to ~/.local/bin/sessync.
        #[arg(long)]
        target: Option<PathBuf>,
        /// Skip codesign step (useful on Linux or if you have a real signing identity).
        #[arg(long)]
        no_codesign: bool,
    },
    /// Encrypt and upload local sessions to the configured backend.
    Push,
    /// Browse remote sessions and pull one into the current project.
    Resume,
    /// Show sync state.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("sessync=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Cmd::Init { mock } => commands::init::run(mock).await,
        Cmd::Install { target, no_codesign } => {
            commands::install::run(target, no_codesign).await
        }
        Cmd::Push => commands::push::run().await,
        Cmd::Resume => commands::resume::run().await,
        Cmd::Status => commands::status::run().await,
    }
}
