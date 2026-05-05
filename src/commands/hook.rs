use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tracing::info;

const SESSYNC_HOOK_TAG: &str = "sessync-auto-push";
const HOOK_COMMAND: &str = "sessync push --quiet # sessync-auto-push";

fn default_settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
    PathBuf::from(home).join(".claude").join("settings.json")
}

/// Install the sessync Stop hook at the given settings.json path (testable helper).
pub fn install_hook_at(path: &Path) -> Result<()> {
    let mut settings: serde_json::Value = if path.exists() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?
    } else {
        // Create parent directories if needed.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        serde_json::json!({})
    };

    // Ensure top-level object.
    let root = settings
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json root is not a JSON object"))?;

    // Ensure hooks object.
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json hooks is not a JSON object"))?;

    // Ensure Stop array.
    let stop = hooks_obj
        .entry("Stop")
        .or_insert_with(|| serde_json::json!([]));
    let stop_arr = stop
        .as_array_mut()
        .ok_or_else(|| anyhow!("settings.json hooks.Stop is not an array"))?;

    // Check idempotency — look for any entry whose hooks commands contain our tag.
    let already_installed = stop_arr.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|cmd| cmd.contains(SESSYNC_HOOK_TAG))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if already_installed {
        println!("Stop hook already installed.");
        return Ok(());
    }

    stop_arr.push(serde_json::json!({
        "matcher": "",
        "hooks": [
            {
                "type": "command",
                "command": HOOK_COMMAND
            }
        ]
    }));

    write_atomic(path, &settings)?;
    info!("installed sessync Stop hook at {}", path.display());
    println!("Stop hook installed at {}", path.display());
    Ok(())
}

/// Remove the sessync Stop hook at the given settings.json path (testable helper).
pub fn uninstall_hook_at(path: &Path) -> Result<()> {
    if !path.exists() {
        println!("No settings.json found — nothing to uninstall.");
        return Ok(());
    }

    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut settings: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;

    let root = settings
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json root is not a JSON object"))?;

    let hooks = match root.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        Some(h) => h,
        None => {
            println!("No hooks found — nothing to uninstall.");
            return Ok(());
        }
    };

    let stop = match hooks.get_mut("Stop").and_then(|s| s.as_array_mut()) {
        Some(s) => s,
        None => {
            println!("No Stop hooks found — nothing to uninstall.");
            return Ok(());
        }
    };

    let before = stop.len();
    stop.retain(|entry| {
        // Keep entries that do NOT contain our tag in any of their hook commands.
        !entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|cmd| cmd.contains(SESSYNC_HOOK_TAG))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });
    let after = stop.len();

    if before == after {
        println!("sessync Stop hook was not installed — nothing to remove.");
        return Ok(());
    }

    write_atomic(path, &settings)?;
    info!("uninstalled sessync Stop hook from {}", path.display());
    println!("Stop hook removed from {}", path.display());
    Ok(())
}

/// Show whether the sessync Stop hook is installed (testable helper).
pub fn status_hook_at(path: &Path) -> Result<bool> {
    if !path.exists() {
        println!("settings.json not found at {}", path.display());
        println!("Status: NOT installed");
        return Ok(false);
    }

    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let settings: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;

    let installed = settings
        .get("hooks")
        .and_then(|h| h.get("Stop"))
        .and_then(|s| s.as_array())
        .map(|stop| {
            stop.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|cmd| cmd.contains(SESSYNC_HOOK_TAG))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    if installed {
        println!("Status: INSTALLED");
        println!("Hook command: {HOOK_COMMAND}");
    } else {
        println!("Status: NOT installed");
        println!("Run `sessync hook install` to set it up.");
    }

    // Warn if sessync is not in PATH (can't be found by the hook at runtime).
    check_sessync_in_path();

    Ok(installed)
}

fn check_sessync_in_path() {
    let in_path = std::process::Command::new("which")
        .arg("sessync")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !in_path {
        eprintln!(
            "WARNING: `sessync` was not found in PATH. \
             The Stop hook runs `sessync push --quiet` in the environment Claude Code \
             inherits. Make sure the binary is on your PATH (e.g. run `sessync install`)."
        );
    }
}

/// Atomically write settings back: write to .json.tmp then rename.
fn write_atomic(path: &Path, settings: &serde_json::Value) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_string_pretty(settings)?;
    std::fs::write(&tmp, pretty)
        .with_context(|| format!("write tmp file {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

// ── Public dispatch entry point ────────────────────────────────────────────────

#[derive(clap::Subcommand)]
pub enum HookAction {
    /// Install the Stop hook into ~/.claude/settings.json (idempotent).
    Install,
    /// Remove the sessync Stop hook from ~/.claude/settings.json.
    Uninstall,
    /// Show whether the hook is currently installed.
    Status,
}

pub fn run(action: HookAction) -> Result<()> {
    let path = default_settings_path();
    match action {
        HookAction::Install => install_hook_at(&path),
        HookAction::Uninstall => uninstall_hook_at(&path),
        HookAction::Status => status_hook_at(&path).map(|_| ()),
    }
}
