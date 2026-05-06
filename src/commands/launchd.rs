use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tracing::info;

const PLIST_LABEL: &str = "com.sessync.push";
const PLIST_FILENAME: &str = "com.sessync.push.plist";

fn default_plist_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| {
        anyhow!("$HOME is not set — refusing to install launchd agent to a literal '~' path")
    })?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(PLIST_FILENAME))
}

fn default_log_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| {
        anyhow!("$HOME is not set — cannot resolve log directory")
    })?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Logs")
        .join("sessync"))
}

fn default_binary_path() -> Result<PathBuf> {
    std::env::current_exe().context("could not resolve current executable path")
}

/// Generate the plist XML content.
fn render_plist(binary_path: &Path, log_dir: &Path) -> String {
    let binary_str = binary_path.display();
    let out_log = log_dir.join("launchd.out.log");
    let err_log = log_dir.join("launchd.err.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.sessync.push</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary_str}</string>
        <string>push</string>
        <string>--quiet</string>
    </array>
    <key>StartInterval</key><integer>1800</integer>
    <key>RunAtLoad</key><false/>
    <key>StandardOutPath</key><string>{out_log}</string>
    <key>StandardErrorPath</key><string>{err_log}</string>
</dict>
</plist>
"#,
        binary_str = binary_str,
        out_log = out_log.display(),
        err_log = err_log.display(),
    )
}

/// Write (or overwrite) the plist file — pure filesystem I/O, no launchctl.
/// Callers that need the agent actually loaded should call `load_via_launchctl` after.
pub fn write_plist_at(plist_path: &Path, binary_path: &Path, log_dir: &Path) -> Result<()> {
    // Create parent directories for both the plist and the log directory.
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir {}", parent.display()))?;
    }
    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("create log dir {}", log_dir.display()))?;

    let content = render_plist(binary_path, log_dir);

    // Atomic write: tmp → rename.
    let tmp_path = plist_path.with_extension("plist.tmp");
    std::fs::write(&tmp_path, &content)
        .with_context(|| format!("write tmp file {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, plist_path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), plist_path.display()))?;

    // Explicit chmod 0644.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(plist_path, std::fs::Permissions::from_mode(0o644))
            .with_context(|| format!("chmod 0644 {}", plist_path.display()))?;
    }

    Ok(())
}

/// Run `launchctl unload -w` (best effort) then `launchctl load -w`.
pub fn load_via_launchctl(plist_path: &Path) -> Result<()> {
    // Unload first — ignore failures (may not be loaded yet).
    let _ = std::process::Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(plist_path)
        .output();

    // Load — surface any failure.
    let out = std::process::Command::new("launchctl")
        .args(["load", "-w"])
        .arg(plist_path)
        .output()
        .context("failed to run launchctl load")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "launchctl load failed (exit {}): {}",
            out.status,
            stderr.trim()
        ));
    }

    Ok(())
}

/// Install the periodic push agent at an explicit path (testable helper).
///
/// `enable_launchctl` — pass `false` in tests to skip the real `launchctl` calls.
pub fn install_at(
    plist_path: &Path,
    binary_path: &Path,
    log_dir: &Path,
    enable_launchctl: bool,
) -> Result<()> {
    write_plist_at(plist_path, binary_path, log_dir)?;
    info!("wrote plist to {}", plist_path.display());

    if enable_launchctl {
        load_via_launchctl(plist_path)?;
        info!("launchd agent loaded");
    }

    println!("launchd agent installed at {}", plist_path.display());
    println!(
        "It will run `sessync push --quiet` every 30 minutes."
    );
    println!(
        "NOTE: if you move the sessync binary, run `sessync launchd install` again to update the path."
    );
    Ok(())
}

/// Remove the periodic push agent at an explicit path (testable helper).
pub fn uninstall_at(plist_path: &Path) -> Result<()> {
    if !plist_path.exists() {
        println!("launchd agent is not installed — nothing to remove.");
        return Ok(());
    }

    // Best-effort unload.
    let _ = std::process::Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(plist_path)
        .output();

    std::fs::remove_file(plist_path)
        .with_context(|| format!("remove {}", plist_path.display()))?;

    info!("removed launchd plist at {}", plist_path.display());
    println!("launchd agent uninstalled.");
    Ok(())
}

/// Return `true` if the plist file exists (testable helper).
/// Also prints a human-readable status line.
pub fn status_at(plist_path: &Path) -> Result<bool> {
    let installed = plist_path.exists();

    if !installed {
        println!("Status: NOT installed");
        println!("Run `sessync launchd install` to set it up.");
        return Ok(false);
    }

    println!("Status: INSTALLED ({})", plist_path.display());

    // Try to determine if it's actually loaded by launchd.
    let loaded = is_loaded_via_launchctl();
    if loaded {
        println!("launchd: LOADED (agent is active)");
    } else {
        println!("launchd: installed but NOT loaded — try `launchctl load -w {}`", plist_path.display());
    }

    Ok(true)
}

/// Return true if `launchctl list` reports our label as active.
fn is_loaded_via_launchctl() -> bool {
    std::process::Command::new("launchctl")
        .arg("list")
        .output()
        .map(|o| {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.contains(PLIST_LABEL)
        })
        .unwrap_or(false)
}

// ── Public dispatch entry point ────────────────────────────────────────────────

#[derive(clap::Subcommand)]
pub enum LaunchdAction {
    /// Install the periodic push agent (~/Library/LaunchAgents/com.sessync.push.plist).
    Install,
    /// Remove the periodic push agent.
    Uninstall,
    /// Show whether the agent is installed and loaded.
    Status,
}

pub fn run(action: LaunchdAction) -> Result<()> {
    let plist_path = default_plist_path()?;
    match action {
        LaunchdAction::Install => {
            let binary_path = default_binary_path()?;
            let log_dir = default_log_dir()?;
            install_at(&plist_path, &binary_path, &log_dir, true)
        }
        LaunchdAction::Uninstall => uninstall_at(&plist_path),
        LaunchdAction::Status => status_at(&plist_path).map(|_| ()),
    }
}
