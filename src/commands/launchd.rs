use anyhow::{anyhow, bail, Context, Result};
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

/// Prefer the brew-managed symlink over the versioned Cellar path.
///
/// `current_exe()` on a Homebrew install resolves to the Cellar path
/// (`/opt/homebrew/Cellar/sessync/<version>/bin/sessync`), which goes stale
/// every `brew upgrade sessync`.  The symlink at `/opt/homebrew/bin/sessync`
/// is maintained by Homebrew across upgrades and is the right path to embed
/// in the plist.
pub fn resolve_binary_path() -> Result<PathBuf> {
    // Apple-silicon Homebrew prefix.
    let brew_arm = PathBuf::from("/opt/homebrew/bin/sessync");
    if brew_arm.exists() {
        return Ok(brew_arm);
    }
    // Intel-mac Homebrew prefix.
    let brew_x86 = PathBuf::from("/usr/local/bin/sessync");
    if brew_x86.exists() {
        return Ok(brew_x86);
    }
    // Manually-installed via `sessync install` default target.
    let local = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".local/bin/sessync"));
    if let Ok(p) = local {
        if p.exists() {
            return Ok(p);
        }
    }
    // Last resort: wherever this binary actually lives (Cellar path or custom).
    std::env::current_exe().context("resolve binary path")
}

/// Generate the plist XML content.
///
/// The agent runs `sessync sync --quiet` on every tick.  `sync` performs push
/// then pull in a single binary invocation, sharing one storage list call per
/// tool between the two directions (see `commands/sync.rs`).  Using a single
/// `ProgramArguments` array avoids the shell-operator workaround that the old
/// `sh -c "push && pull"` pattern required.
///
/// Changed from 120 s (v0.7.1) to 1800 s in v0.7.4 to reduce OSS list-call
/// volume.  With ~1500 remote objects per tool, each list response is ~500 KB
/// of XML; at 120 s intervals the old plist generated ~40 GB/month of outbound
/// traffic from list calls alone.  30-minute intervals reduce that to ~1.3 GB.
/// Users can override per-install via `sessync launchd install --interval <secs>`.
pub const DEFAULT_INTERVAL_SECS: u64 = 1800;

fn render_plist(binary_path: &Path, log_dir: &Path, interval_secs: u64) -> String {
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
        <string>sync</string>
        <string>--quiet</string>
    </array>
    <key>StartInterval</key><integer>{interval_secs}</integer>
    <key>RunAtLoad</key><false/>
    <key>StandardOutPath</key><string>{out_log}</string>
    <key>StandardErrorPath</key><string>{err_log}</string>
</dict>
</plist>
"#,
        binary_str = binary_str,
        interval_secs = interval_secs,
        out_log = out_log.display(),
        err_log = err_log.display(),
    )
}

/// Write (or overwrite) the plist file — pure filesystem I/O, no launchctl.
/// Callers that need the agent actually loaded should call `load_via_launchctl` after.
pub fn write_plist_at(
    plist_path: &Path,
    binary_path: &Path,
    log_dir: &Path,
    interval_secs: u64,
) -> Result<()> {
    // Create parent directories for both the plist and the log directory.
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir {}", parent.display()))?;
    }
    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("create log dir {}", log_dir.display()))?;

    let content = render_plist(binary_path, log_dir, interval_secs);

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

/// Return the current user's numeric UID.
fn current_uid() -> u32 {
    // SAFETY: getuid() has no preconditions and always succeeds.
    unsafe { libc::getuid() }
}

/// Build the `launchctl bootstrap` argument list for the given uid + plist.
///
/// Extracted for unit-testability.
pub fn bootstrap_args(uid: u32, plist_path: &Path) -> Vec<String> {
    vec![
        "bootstrap".to_owned(),
        format!("gui/{uid}"),
        plist_path.display().to_string(),
    ]
}

/// Build the `launchctl bootout` argument list for unload / best-effort teardown.
///
/// `bootout` takes the service identifier (`gui/<uid>/<label>`), NOT the plist path.
pub fn bootout_args(uid: u32) -> Vec<String> {
    vec![
        "bootout".to_owned(),
        format!("gui/{uid}/{PLIST_LABEL}"),
    ]
}

/// Return a hint string for macOS 15+ / Tahoe's Login Items approval quirk,
/// or `None` on older macOS and non-macOS platforms.
///
/// Extracted as a pure function so tests can call it without shelling out.
pub fn tahoe_hint_for_macos_major(major: u32) -> Option<&'static str> {
    if major >= 15 {
        Some(
            "Hint (macOS 15+ / Tahoe quirk): brew upgrade invalidates the prior\n\
             Login Items approval. Open System Settings → General → Login Items &\n\
             Extensions → 'Background', find 'sessync', toggle OFF then ON, then\n\
             re-run `sessync launchd install`.",
        )
    } else {
        None
    }
}

/// Query the running macOS major version.  Returns `None` on non-macOS or
/// if `sw_vers` fails / returns unparseable output.
#[cfg(target_os = "macos")]
fn macos_major_version() -> Option<u32> {
    let out = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    let version = String::from_utf8_lossy(&out.stdout);
    version
        .trim()
        .split('.')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
}

/// Return a Tahoe hint string if running on macOS 15+, else empty string.
fn tahoe_hint_if_applicable() -> String {
    #[cfg(target_os = "macos")]
    {
        // macOS 15.x (Sequoia) and 26.x+ (Tahoe) introduced stricter
        // LaunchAgent approval. brew upgrade changes the binary signature
        // and invalidates the user's prior approval, while the Login Items
        // UI still shows the toggle as ON. Bootstrap then fails with
        // cryptic exit 5 / "Input/output error".
        if let Some(major) = macos_major_version() {
            if let Some(hint) = tahoe_hint_for_macos_major(major) {
                return format!("\n\n{hint}");
            }
        }
    }
    String::new()
}

/// Run `launchctl bootout` (best effort) then `launchctl bootstrap`.
///
/// Uses the modern macOS 11+ API.  `bootstrap` persists across reboots;
/// the old `load -w` is documented legacy and unreliable on macOS 14+.
pub fn load_via_launchctl(plist_path: &Path) -> Result<()> {
    let uid = current_uid();

    // Bootout first — ignore all failures (service may not be loaded yet).
    let _ = std::process::Command::new("launchctl")
        .args(bootout_args(uid))
        .output();

    // Bootstrap — load + enable; persists across reboots.
    let out = std::process::Command::new("launchctl")
        .args(bootstrap_args(uid, plist_path))
        .output()
        .context("failed to run launchctl bootstrap")?;

    match out.status.code() {
        Some(0) => Ok(()),
        Some(c) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let extra = tahoe_hint_if_applicable();
            bail!(
                "launchctl bootstrap exit {c}: {}{extra}",
                stderr.trim()
            )
        }
        None => bail!("launchctl bootstrap killed by signal"),
    }
}

/// Install the periodic push agent at an explicit path (testable helper).
///
/// `enable_launchctl` — pass `false` in tests to skip the real `launchctl` calls.
pub fn install_at(
    plist_path: &Path,
    binary_path: &Path,
    log_dir: &Path,
    enable_launchctl: bool,
    interval_secs: u64,
) -> Result<()> {
    write_plist_at(plist_path, binary_path, log_dir, interval_secs)?;
    info!("wrote plist to {} (interval={interval_secs}s)", plist_path.display());

    if enable_launchctl {
        load_via_launchctl(plist_path)?;
        info!("launchd agent loaded");
    }

    println!("launchd agent installed at {}", plist_path.display());
    let mins = interval_secs / 60;
    let interval_human = if interval_secs % 60 == 0 && mins >= 1 {
        format!("{mins} minute{}", if mins == 1 { "" } else { "s" })
    } else {
        format!("{interval_secs} seconds")
    };
    println!("It will run `sessync sync --quiet` every {interval_human}.");
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

    // Best-effort bootout (modern API).
    let uid = current_uid();
    let _ = std::process::Command::new("launchctl")
        .args(bootout_args(uid))
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

    // Determine if it's actually loaded using the modern `launchctl print` API.
    let loaded = is_loaded_via_launchctl();
    if loaded {
        println!("launchd: LOADED (agent is active)");
    } else {
        println!(
            "launchd: installed but NOT loaded — try `sessync launchd install` to re-register"
        );
    }

    Ok(true)
}

/// Return true if `launchctl print` reports our label as loaded.
///
/// Uses the modern `print gui/<uid>/<label>` form: exits 0 if loaded,
/// non-zero if not.  More reliable than parsing `launchctl list` output.
fn is_loaded_via_launchctl() -> bool {
    let uid = current_uid();
    std::process::Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{PLIST_LABEL}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Public re-export of `default_plist_path` for use by `auto_push`.
pub fn default_plist_path_pub() -> Result<PathBuf> {
    default_plist_path()
}

/// Convenience for `sessync upgrade`: re-install with default paths and
/// default interval, optionally engaging `launchctl bootstrap`.  Used after
/// `brew upgrade sessync` to restore the agent that macOS silently dropped
/// when the binary's inode changed.  No-op printable output — caller
/// (`upgrade`) prints its own status line.
pub fn install_default(enable_launchctl: bool) -> Result<()> {
    let plist_path = default_plist_path()?;
    let log_dir = default_log_dir()?;
    let binary_path = resolve_binary_path()?;
    write_plist_at(&plist_path, &binary_path, &log_dir, DEFAULT_INTERVAL_SECS)?;
    if enable_launchctl {
        load_via_launchctl(&plist_path)?;
    }
    Ok(())
}

/// Public re-export of `default_log_dir` for use by `auto_push`.
pub fn default_log_dir_pub() -> Result<PathBuf> {
    default_log_dir()
}

// ── Public dispatch entry point ────────────────────────────────────────────────

#[derive(clap::Subcommand)]
pub enum LaunchdAction {
    /// Install the periodic sync agent (~/Library/LaunchAgents/com.sessync.push.plist).
    Install {
        /// How often to run `sync`, in seconds. Default: 1800 (30 min).
        #[arg(long, default_value_t = DEFAULT_INTERVAL_SECS)]
        interval: u64,
    },
    /// Remove the periodic push agent.
    Uninstall,
    /// Show whether the agent is installed and loaded.
    Status,
}

pub fn run(action: LaunchdAction) -> Result<()> {
    let plist_path = default_plist_path()?;
    match action {
        LaunchdAction::Install { interval } => {
            let binary_path = resolve_binary_path()?;
            let log_dir = default_log_dir()?;
            install_at(&plist_path, &binary_path, &log_dir, true, interval)
        }
        LaunchdAction::Uninstall => uninstall_at(&plist_path),
        LaunchdAction::Status => status_at(&plist_path).map(|_| ()),
    }
}
