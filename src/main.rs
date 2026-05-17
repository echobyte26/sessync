use clap::{Parser, Subcommand};
use sessync::commands;
use std::path::PathBuf;

const AFTER_HELP: &str = "\
EXAMPLES:
  sessync init                            First-time setup
  sessync push                            Push sessions from all tools
  sessync push --tool claude-code         Push only Claude Code sessions
  sessync push --tool codex               Push only Codex sessions
  sessync push --dry-run                  Preview without uploading
  sessync pull                            Download all newer remote sessions
  sessync pull --tool codex               Only Codex
  sessync pull --dry-run                  Preview without downloading
  sessync resume                          Pick from all tools, sorted by recency
  sessync resume --tool codex             Pick only from Codex sessions
  sessync ls                              List remote sessions (grouped by tool)
  sessync ls --json                       Machine-readable {\"tools\": [...]}
  sessync status                          Show sync state + per-tool counts
  sessync doctor                          Diagnose config / storage / hooks / queue
  sessync logs -n 10 --failed             Last 10 failed pushes
  sessync hook install                    Auto-push on Claude Code session end
  sessync hook install --tool codex       Same for Codex (enables codex_hooks)
  sessync auto-push setup                 Install hook + launchd for all tools
  sessync launchd install                 Periodic safety-net push (macOS)
  sessync purge --pattern claude-mem --dry-run    Preview cleanup
  sessync purge --pattern claude-mem               Delete after confirm
  sessync upgrade                         Update sessync via Homebrew

CONFIG:
  ~/.config/sessync/config.toml           OSS endpoint / bucket / creds
  ~/.config/sessync/passphrase.enc        Machine-bound passphrase
  ~/.local/share/sessync/queue.db         Pending pushes + outcomes + etags
  ~/.cache/sessync/meta-cache.age         Decrypted meta cache

CLAUDE CODE:
  ~/.claude/projects/<encoded-cwd>/*.jsonl    Session files
  ~/.claude/settings.json                     Stop hook (JSON)

CODEX:
  ~/.codex/state_*.sqlite                                  Session index (SQLite)
  ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl             Conversation rollouts
  ~/.codex/config.toml                                     Stop hook (TOML)";

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
    /// Download all remote sessions newer than local copies.
    Pull {
        /// Suppress normal output. Errors still surface.
        #[arg(long)]
        quiet: bool,
        /// Limit to one tool: "claude-code", "codex", etc. Default: all registered tools.
        #[arg(long)]
        tool: Option<String>,
        /// Print the pull plan without downloading anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Browse remote sessions and pull one into the current project.
    Resume {
        /// Don't auto-exec claude/codex after pulling.
        #[arg(long)]
        no_launch: bool,
        /// Restart Codex.app after writing (Codex doesn't reload its session list
        /// from disk; this kills+reopens it). Ignored for Claude Code.
        #[arg(long)]
        restart_app: bool,
        /// Limit to one tool. If given, skips the tool-picker step.
        #[arg(long)]
        tool: Option<String>,
        /// Limit to one project_key. If given, skips the project-picker step.
        #[arg(long)]
        project: Option<String>,
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
    /// Delete remote sessions matching a source_cwd pattern. Irreversible.
    Purge {
        /// Substring to match against each session's source_cwd.
        #[arg(long, value_name = "SUBSTRING")]
        pattern: String,
        /// Print what would be deleted without doing it.
        #[arg(long)]
        dry_run: bool,
        /// Skip the typed confirmation. Use with care.
        #[arg(short, long)]
        yes: bool,
    },
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
    /// Push and pull in a single invocation, sharing one storage list call per
    /// tool. Preferred over `sh -c "push && pull"` in launchd plists because it
    /// halves the number of OSS list requests per cycle.
    Sync {
        /// Suppress normal output. Errors still surface.
        #[arg(long)]
        quiet: bool,
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
    // Fix 3: restore the default SIGPIPE handler so that piping sessync output
    // to programs like `head` or `grep` that close stdin early doesn't cause
    // a panic from tracing-subscriber writing to a broken pipe.
    //
    // SAFETY: signal() with SIG_DFL is safe to call from any thread. It sets
    // the process-wide SIGPIPE disposition to default (terminate quietly on
    // broken pipe) rather than Rust's override that converts it into a panic.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

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
        Some(Cmd::Sync { quiet }) => commands::sync::run(quiet).await,
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
        Some(Cmd::Pull { quiet, tool, dry_run }) => {
            commands::pull::run(quiet, tool, dry_run).await
        }
        Some(Cmd::Resume { no_launch, restart_app, tool, project }) => {
            commands::resume::run(no_launch, restart_app, tool, project).await
        }
        Some(Cmd::Ls { project, json, tool }) => commands::ls::run(project, json, tool).await,
        Some(Cmd::Status) => commands::status::run().await,
        Some(Cmd::Logs { limit, since, failed }) => commands::logs::run(limit, since, failed),
        Some(Cmd::Doctor) => commands::doctor::run().await,
        Some(Cmd::Purge { pattern, dry_run, yes }) => {
            commands::purge::run(pattern, dry_run, yes).await
        }
        Some(Cmd::Uninstall { purge_remote, yes }) => {
            commands::uninstall::run(purge_remote, yes).await
        }
        // Handled by early-return above; this arm is unreachable.
        Some(Cmd::Completions { .. }) => unreachable!(),
        Some(Cmd::Upgrade) => commands::upgrade::run().await,
    }
}
