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
    command: Option<Cmd>,
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
    /// Manage the Claude Code Stop hook that auto-runs `sessync push`.
    Hook {
        #[command(subcommand)]
        action: commands::hook::HookAction,
    },
    /// Manage the launchd periodic-push agent (macOS only).
    #[cfg(target_os = "macos")]
    Launchd {
        #[command(subcommand)]
        action: commands::launchd::LaunchdAction,
    },
    /// Encrypt and upload local sessions to the configured backend.
    Push {
        /// Suppress normal output (used by Stop hook). Errors still surface.
        #[arg(long)]
        quiet: bool,
        /// Only push these session IDs (default: all). May be passed multiple times.
        #[arg(value_name = "SESSION_ID")]
        sessions: Vec<String>,
        /// Silence the stale-overwrite warning when remote is newer than local.
        #[arg(long)]
        no_stale_warn: bool,
        /// Print the push plan without uploading anything. Skips queue + notifications.
        #[arg(long)]
        dry_run: bool,
    },
    /// Browse remote sessions and pull one into the current project.
    Resume,
    /// Non-interactive listing of remote sessions (useful for scripting).
    Ls {
        /// Show only the given project_key.
        #[arg(long)]
        project: Option<String>,
        /// Emit machine-readable JSON instead of the human view.
        #[arg(long)]
        json: bool,
    },
    /// Show sync state.
    Status,
    /// Show recent push outcomes from the queue (success/failure log).
    Logs {
        /// How many recent entries to show. Default: 20.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Run a battery of diagnostic checks (config, storage, hook, queue, …).
    Doctor,
    /// Remove local installation (binary, config, keychain entry, optional mock store).
    Uninstall {
        /// Also delete all sessync objects from the configured remote backend.
        /// Irreversible.
        #[arg(long)]
        purge_remote: bool,
        /// Skip the confirmation prompt. Use with care.
        #[arg(short, long)]
        yes: bool,
    },
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
        None => commands::status::run().await,
        Some(Cmd::Init { mock }) => commands::init::run(mock).await,
        Some(Cmd::Install { target, no_codesign }) => {
            commands::install::run(target, no_codesign).await
        }
        Some(Cmd::Hook { action }) => commands::hook::run(action),
        #[cfg(target_os = "macos")]
        Some(Cmd::Launchd { action }) => commands::launchd::run(action),
        Some(Cmd::Push {
            quiet,
            sessions,
            no_stale_warn,
            dry_run,
        }) => commands::push::run(quiet, sessions, no_stale_warn, dry_run).await,
        Some(Cmd::Resume) => commands::resume::run().await,
        Some(Cmd::Ls { project, json }) => commands::ls::run(project, json).await,
        Some(Cmd::Status) => commands::status::run().await,
        Some(Cmd::Logs { limit }) => commands::logs::run(limit),
        Some(Cmd::Doctor) => commands::doctor::run().await,
        Some(Cmd::Uninstall { purge_remote, yes }) => {
            commands::uninstall::run(purge_remote, yes).await
        }
    }
}
