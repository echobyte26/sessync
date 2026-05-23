//! `sessync upgrade` — update sessync via Homebrew.
//!
//! Runs `brew update` then `brew upgrade sessync`, then silently fixes up
//! the two things macOS / `brew upgrade` quietly break under the hood:
//!
//!   1. **launchd agent unloaded by macOS**.  `brew upgrade` rebuilds the
//!      `/opt/homebrew/bin/sessync` symlink, changing its inode.  macOS
//!      launchd, which holds an inode reference at bootstrap time, silently
//!      unloads the agent.  Pre-v0.9.9 the user had to run
//!      `sessync launchd install` manually after every upgrade.  Now we
//!      re-bootstrap automatically at the end of `upgrade`.
//!
//!   2. **Old bare-name hook command silently fails on PATH-restricted
//!      Claude Code spawn contexts**.  v0.9.8 changed the install command
//!      to embed the absolute binary path, but existing installs from
//!      ≤v0.9.7 still had `sessync push --quiet # sessync-auto-push` in
//!      their `~/.claude/settings.json`.  We now detect this at upgrade
//!      time and rewrite the entry to the absolute-path form — one-time
//!      migration, transparent.  Going forward, the hook command format
//!      WILL NOT CHANGE: write-once-stays-valid is the contract.

use anyhow::{bail, Result};
use tokio::process::Command;

/// Run `brew update && brew upgrade sessync`, streaming output in real time,
/// then silently restore the launchd registration and migrate the hook
/// command format if needed (so the user never has to run
/// `sessync launchd install` or `sessync hook install` manually).
pub async fn run() -> Result<()> {
    println!("Current version: {}", env!("CARGO_PKG_VERSION"));

    run_brew(&["update"]).await?;
    run_brew(&["upgrade", "sessync"]).await?;

    // ── Post-upgrade restoration (v0.9.9) ───────────────────────────────────
    //
    // These steps are SILENT on success — only print on actual change or
    // failure, so a clean upgrade still feels like just `brew upgrade`.
    if let Err(e) = post_upgrade_restore_launchd() {
        eprintln!("warning: launchd re-register failed: {e}");
        eprintln!("  run `sessync launchd install` manually to fix");
    }
    if let Err(e) = post_upgrade_migrate_hook() {
        eprintln!("warning: hook migration failed: {e}");
        eprintln!("  run `sessync hook install` manually to fix");
    }

    println!(
        "Done. Run `sessync --version` to confirm. \
        See CHANGELOG: https://github.com/echobyte26/sessync/blob/main/CHANGELOG.md"
    );
    Ok(())
}

/// If a sessync launchd plist exists on disk, re-bootstrap it.  Does nothing
/// (silently) if the user never installed the agent.  Re-bootstrap is
/// idempotent — `launchd install` does bootout-then-bootstrap internally,
/// so it tolerates whatever state macOS left us in after `brew upgrade`.
fn post_upgrade_restore_launchd() -> Result<()> {
    let plist_path = match crate::commands::launchd::default_plist_path_pub() {
        Ok(p) => p,
        Err(_) => return Ok(()), // non-macOS or no HOME — nothing to do
    };
    if !plist_path.exists() {
        return Ok(()); // user never installed launchd agent — skip
    }
    // Re-bootstrap with current (default) interval and current binary path.
    // Use install_with_options for direct programmatic invocation.
    crate::commands::launchd::install_default(true)?;
    println!("Re-registered launchd agent (brew upgrade can drop it).");
    Ok(())
}

/// If the Claude Code Stop hook is installed but uses an old bare-name
/// command (e.g. `sessync push --quiet # sessync-auto-push`), rewrite it
/// to the absolute-path form so PATH-restricted hook spawn contexts can
/// still find the binary.  Silent if the hook is already up-to-date or
/// not installed.  Same logic for the Codex hook.
fn post_upgrade_migrate_hook() -> Result<()> {
    // Claude Code side
    if let Ok(path) = crate::commands::hook::default_claude_settings_path_pub() {
        if path.exists() {
            let migrated = crate::commands::hook::migrate_hook_to_absolute_path(&path)?;
            if migrated {
                println!("Migrated Claude Code Stop hook to absolute-path command.");
            }
        }
    }
    // Codex side
    if let Ok(path) = crate::commands::hook::default_codex_config_path_pub() {
        if path.exists() {
            let migrated =
                crate::commands::hook::migrate_codex_hook_to_absolute_path(&path)?;
            if migrated {
                println!("Migrated Codex Stop hook to absolute-path command.");
            }
        }
    }
    Ok(())
}

/// Spawn `brew <args>` with inherited stdio and wait for it to finish.
///
/// On `NotFound` (brew not in PATH) we emit a helpful message and return an
/// error rather than bubbling up a raw OS error.
async fn run_brew(args: &[&str]) -> Result<()> {
    let mut child = match Command::new("brew").args(args).spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "brew not found in PATH — sessync upgrade requires Homebrew \
                (https://brew.sh)"
            );
        }
        Err(e) => return Err(e.into()),
    };

    let status = child.wait().await?;
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        bail!("`brew {}` exited with status {code}", args.join(" "));
    }
    Ok(())
}
