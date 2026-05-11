use clap::{Parser, Subcommand};
use sessync::commands;
use std::path::PathBuf;

const AFTER_HELP: &str = "\
EXAMPLES:
  sessync init                    First-time setup (config + passphrase)
  sessync init --mock             First-time setup with local-fs backend (smoke test)
  sessync push                    Encrypt and upload changed sessions
  sessync push --dry-run          Preview what push would do, no upload
  sessync resume                  Pick a remote session and resume it locally
  sessync ls                      List remote sessions (no prompts)
  sessync status                  Show device, sync state, queue, last push
  sessync doctor                  Diagnose config / storage / hook / queue
  sessync logs -n 10              Show last 10 push outcomes
  sessync hook install            Auto-push on every Claude Code session end
  sessync launchd install         Periodic safety-net push every 30 min (macOS)
  sessync auto-push setup        Install Stop hook + launchd in one go
  sessync upgrade                 Update sessync via Homebrew

DOCS:
  https://github.com/echobyte26/sessync

CONFIG:
  ~/.config/sessync/config.toml             OSS endpoint / bucket / creds
  ~/.config/sessync/passphrase.enc          Machine-bound passphrase
  ~/.local/share/sessync/queue.db           Pending pushes + outcomes log
  ~/.cache/sessync/meta-cache.age           Decrypted meta cache (resume speed)";

#[derive(Parser)]
#[command(
    name = "sessync",
    version,
    about = "Cross-device sync for Claude Code sessions, with client-side encryption",
    after_help = AFTER_HELP,
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
        /// On stale-overwrite, save the local version as a forked copy on the remote
        /// instead of overwriting. The remote-newer copy is left untouched.
        #[arg(long)]
        fork_on_conflict: bool,
        /// Limit to one tool: "claude-code", etc. Default: all registered tools.
        #[arg(long)]
        tool: Option<String>,
    },
    /// Browse remote sessions and pull one into the current project.
    Resume {
        /// Don't auto-exec the tool's CLI after pulling. Just print the command.
        #[arg(long)]
        no_launch: bool,
        /// Limit to one tool's sessions when listing. Default: all.
        #[arg(long)]
        tool: Option<String>,
    },
    /// Non-interactive listing of remote sessions (useful for scripting).
    Ls {
        /// Show only the given project_key.
        #[arg(long)]
        project: Option<String>,
        /// Emit machine-readable JSON instead of the human view.
        #[arg(long)]
        json: bool,
        /// Limit to one tool's sessions. Default: all.
        #[arg(long)]
        tool: Option<String>,
    },
    /// Show sync state.
    Status,
    /// Show recent push outcomes from the queue (success/failure log).
    Logs {
        /// How many recent entries to show. Default: 20.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Show only entries from the last duration (e.g. "1h", "30m", "1d").
        #[arg(long)]
        since: Option<String>,
        /// Show only failed pushes.
        #[arg(long)]
        failed: bool,
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
    /// Set up automatic push (Stop hook + macOS launchd) in one command.
    AutoPush {
        #[command(subcommand)]
        action: commands::auto_push::AutoPushAction,
    },
    /// Generate shell completion script (zsh / bash / fish / powershell / elvish).
    /// Pipe to your shell's completion dir to install.
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Upgrade sessync via Homebrew (runs `brew update && brew upgrade sessync`).
    Upgrade,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("sessync=info".parse().unwrap()),
        )
        .without_time()
        .init();

    let cli = Cli::parse();

    if let Some(Cmd::Completions { shell }) = &cli.command {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        clap_complete::generate(*shell, &mut cmd, "sessync", &mut std::io::stdout());
        return Ok(());
    }

    match cli.command {
        None => commands::status::run().await,
        Some(Cmd::Init { mock }) => commands::init::run(mock).await,
        Some(Cmd::Install { target, no_codesign }) => {
            commands::install::run(target, no_codesign).await
        }
        Some(Cmd::AutoPush { action }) => commands::auto_push::run(action),
        Some(Cmd::Hook { action }) => commands::hook::run(action),
        #[cfg(target_os = "macos")]
        Some(Cmd::Launchd { action }) => commands::launchd::run(action),
        Some(Cmd::Push {
            quiet,
            sessions,
            no_stale_warn,
            dry_run,
            fork_on_conflict,
            tool,
        }) => commands::push::run(quiet, sessions, no_stale_warn, dry_run, fork_on_conflict, tool).await,
        Some(Cmd::Resume { no_launch, tool }) => commands::resume::run(no_launch, tool).await,
        Some(Cmd::Ls { project, json, tool }) => commands::ls::run(project, json, tool).await,
        Some(Cmd::Status) => commands::status::run().await,
        Some(Cmd::Logs { limit, since, failed }) => commands::logs::run(limit, since, failed),
        Some(Cmd::Doctor) => commands::doctor::run().await,
        Some(Cmd::Uninstall { purge_remote, yes }) => {
            commands::uninstall::run(purge_remote, yes).await
        }
        // Handled by early-return above; this arm is unreachable.
        Some(Cmd::Completions { .. }) => unreachable!(),
        Some(Cmd::Upgrade) => commands::upgrade::run().await,
    }
}
