use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use tracing::info;

/// Trailing comment in the hook command field — kept as a strong "ours" marker.
/// See `is_managed_by_sessync` for the full identification rule. Don't strip
/// the suffix when editing the command without also updating the matcher.
const SESSYNC_HOOK_TAG: &str = "sessync-auto-push";

/// The Claude Code hook command (pushed to ~/.claude/settings.json).
const CLAUDE_HOOK_COMMAND: &str = "sessync push --quiet # sessync-auto-push";

/// The Codex hook command.  Includes --tool codex so the hook only pushes
/// Codex sessions and doesn't double-push Claude Code sessions.
const CODEX_HOOK_COMMAND: &str = "sessync push --quiet --tool codex # sessync-auto-push";

// ─────────────────────────────────────────────────────────────────────────────
// Shared helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Identify a hook entry command as one we manage. Two acceptable signals:
///   1. The command contains our `SESSYNC_HOOK_TAG` (the canonical case).
///   2. The command starts with `"sessync push"` (covers users who hand-edited
///      our entry and stripped the trailing comment — we still claim it as ours
///      to keep idempotency and uninstall correctness).
fn is_managed_by_sessync(command: &str) -> bool {
    command.contains(SESSYNC_HOOK_TAG) || command.trim_start().starts_with("sessync push")
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
             The Stop hook runs `sessync push --quiet` in the environment the tool \
             inherits. Make sure the binary is on your PATH (e.g. run `sessync install`)."
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Claude Code — JSON (settings.json)
// ─────────────────────────────────────────────────────────────────────────────

fn default_claude_settings_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| {
        anyhow!("$HOME is not set — refusing to install hook to a literal '~' path")
    })?;
    Ok(PathBuf::from(home).join(".claude").join("settings.json"))
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
                        .map(is_managed_by_sessync)
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
                "command": CLAUDE_HOOK_COMMAND
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
                        .map(is_managed_by_sessync)
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
        println!("Hook command: {CLAUDE_HOOK_COMMAND}");
    } else {
        println!("Status: NOT installed");
        println!("Run `sessync hook install` to set it up.");
    }

    check_sessync_in_path();

    Ok(installed)
}

/// Atomically write JSON settings back: write to .tmp then rename.
fn write_atomic(path: &Path, settings: &serde_json::Value) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_string_pretty(settings)?;
    std::fs::write(&tmp, pretty)
        .with_context(|| format!("write tmp file {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Claude Code — tool-specific dispatch wrappers
// ─────────────────────────────────────────────────────────────────────────────

fn install_claude_code(path: &Path) -> Result<()> {
    install_hook_at(path)
}

fn uninstall_claude_code(path: &Path) -> Result<()> {
    uninstall_hook_at(path)
}

fn status_claude_code(path: &Path) -> Result<bool> {
    status_hook_at(path)
}

// ─────────────────────────────────────────────────────────────────────────────
// Codex — TOML (config.toml)
// ─────────────────────────────────────────────────────────────────────────────

fn default_codex_config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| {
        anyhow!("$HOME is not set — refusing to install hook to a literal '~' path")
    })?;
    Ok(PathBuf::from(home).join(".codex").join("config.toml"))
}

/// Read a TOML config file, returning an empty table if the file doesn't exist.
/// Creates parent directories as needed.
fn read_codex_config(path: &Path) -> Result<toml::Value> {
    if path.exists() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        Ok(toml::Value::Table(toml::map::Map::new()))
    }
}

/// Atomically write a TOML value back: write to .toml.tmp then rename.
///
/// NOTE: TOML round-trip through the `toml` crate will lose comments and may
/// reorder keys within tables.  This is a known limitation accepted for now.
pub fn write_atomic_toml(path: &Path, value: &toml::Value) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    let serialized =
        toml::to_string_pretty(value).with_context(|| "serialize TOML")?;
    std::fs::write(&tmp, serialized)
        .with_context(|| format!("write tmp file {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Ensure `[features] codex_hooks = true` is present in the config.
/// Returns an error if `codex_hooks` is set to a non-boolean value.
fn ensure_codex_hooks_feature(config: &mut toml::Value) -> Result<()> {
    let root = config
        .as_table_mut()
        .ok_or_else(|| anyhow!("config.toml root is not a TOML table"))?;

    let features = root
        .entry("features")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));

    let features_table = features
        .as_table_mut()
        .ok_or_else(|| anyhow!("config.toml [features] is not a TOML table"))?;

    match features_table.get("codex_hooks") {
        None | Some(toml::Value::Boolean(false)) => {
            features_table.insert(
                "codex_hooks".to_string(),
                toml::Value::Boolean(true),
            );
            info!("enabled codex_hooks feature flag");
        }
        Some(toml::Value::Boolean(true)) => {
            // Already enabled — nothing to do.
        }
        Some(other) => {
            bail!(
                "config.toml [features].codex_hooks is set to `{other}`, which is not a boolean. \
                 Please set it to `true` or remove the key and re-run `sessync hook install --tool codex`."
            );
        }
    }

    Ok(())
}

/// Check whether the config has `[features] codex_hooks = true`.
fn codex_hooks_feature_enabled(config: &toml::Value) -> bool {
    config
        .get("features")
        .and_then(|f| f.get("codex_hooks"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Install the sessync Stop hook into a Codex config.toml (testable helper).
///
/// Schema used (per research doc section 3, Option A — inline TOML):
///
/// ```toml
/// [features]
/// codex_hooks = true
///
/// [[hooks.Stop]]
///
/// [[hooks.Stop.hooks]]
/// type = "command"
/// command = "sessync push --quiet --tool codex # sessync-auto-push"
/// timeout = 30
/// ```
///
/// verify against actual Codex config — the research doc calls this "Option A"
/// and cites codex-rs/hooks source files; the exact key names (`hooks.Stop` vs
/// `hooks.stop`) were confirmed from the source-level `Stop` event name.
pub fn install_codex_hook_at(path: &Path) -> Result<()> {
    let mut config = read_codex_config(path)?;

    // Ensure the feature flag is set.
    ensure_codex_hooks_feature(&mut config)?;

    let root = config
        .as_table_mut()
        .ok_or_else(|| anyhow!("config.toml root is not a TOML table"))?;

    // Ensure [hooks] table exists.
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let hooks_table = hooks
        .as_table_mut()
        .ok_or_else(|| anyhow!("config.toml [hooks] is not a TOML table"))?;

    // Ensure hooks.Stop is an array-of-tables.
    let stop_arr = hooks_table
        .entry("Stop")
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    let stop_vec = stop_arr
        .as_array_mut()
        .ok_or_else(|| anyhow!("config.toml hooks.Stop is not a TOML array"))?;

    // Idempotency: scan existing entries for our tag in any nested hook command.
    let already_installed = stop_vec.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|inner_hooks| {
                inner_hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(is_managed_by_sessync)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if already_installed {
        println!("Codex Stop hook already installed.");
        return Ok(());
    }

    // Build the inner hook entry.
    let mut inner_hook = toml::map::Map::new();
    inner_hook.insert("type".to_string(), toml::Value::String("command".to_string()));
    inner_hook.insert(
        "command".to_string(),
        toml::Value::String(CODEX_HOOK_COMMAND.to_string()),
    );
    inner_hook.insert("timeout".to_string(), toml::Value::Integer(30));

    // Build the [[hooks.Stop]] entry wrapping the inner hook.
    let mut stop_entry = toml::map::Map::new();
    stop_entry.insert(
        "hooks".to_string(),
        toml::Value::Array(vec![toml::Value::Table(inner_hook)]),
    );

    stop_vec.push(toml::Value::Table(stop_entry));

    write_atomic_toml(path, &config)?;
    info!("installed sessync Codex Stop hook at {}", path.display());
    println!(
        "Codex Stop hook installed at {} (codex_hooks feature enabled)",
        path.display()
    );
    Ok(())
}

/// Remove the sessync Stop hook from a Codex config.toml (testable helper).
///
/// Judgment call: we do NOT remove `codex_hooks = true` on uninstall.
/// Rationale: the user may have enabled the feature for other hooks they own.
/// Removing it would silently break their other hooks.  If they want to
/// disable the feature, they can set it to `false` manually.
pub fn uninstall_codex_hook_at(path: &Path) -> Result<()> {
    if !path.exists() {
        println!("No config.toml found — nothing to uninstall.");
        return Ok(());
    }

    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut config: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;

    let root = config
        .as_table_mut()
        .ok_or_else(|| anyhow!("config.toml root is not a TOML table"))?;

    let hooks_table = match root.get_mut("hooks").and_then(|h| h.as_table_mut()) {
        Some(h) => h,
        None => {
            println!("No [hooks] section found — nothing to uninstall.");
            return Ok(());
        }
    };

    let stop_arr = match hooks_table
        .get_mut("Stop")
        .and_then(|s| s.as_array_mut())
    {
        Some(a) => a,
        None => {
            println!("No hooks.Stop entries found — nothing to uninstall.");
            return Ok(());
        }
    };

    let before = stop_arr.len();
    stop_arr.retain(|entry| {
        !entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|inner_hooks| {
                inner_hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(is_managed_by_sessync)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });
    let after = stop_arr.len();

    if before == after {
        println!("sessync Codex Stop hook was not installed — nothing to remove.");
        return Ok(());
    }

    write_atomic_toml(path, &config)?;
    info!("uninstalled sessync Codex Stop hook from {}", path.display());
    println!("Codex Stop hook removed from {}", path.display());
    Ok(())
}

/// Show whether the Codex Stop hook is installed (testable helper).
///
/// Returns `true` only when BOTH the feature flag AND the hook entry are present.
/// If the hook entry exists but `codex_hooks = false`, auto-push would silently
/// not fire, so status correctly returns false.
pub fn status_codex_hook_at(path: &Path) -> Result<bool> {
    if !path.exists() {
        println!("config.toml not found at {}", path.display());
        println!("Status: NOT installed");
        return Ok(false);
    }

    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let config: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;

    let feature_enabled = codex_hooks_feature_enabled(&config);

    let hook_entry_present = config
        .get("hooks")
        .and_then(|h| h.get("Stop"))
        .and_then(|s| s.as_array())
        .map(|stop| {
            stop.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|inner_hooks| {
                        inner_hooks.iter().any(|h| {
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

    let installed = feature_enabled && hook_entry_present;

    if installed {
        println!("Status: INSTALLED");
        println!("Hook command: {CODEX_HOOK_COMMAND}");
        println!("[features] codex_hooks = true");
    } else if hook_entry_present && !feature_enabled {
        println!("Status: PARTIALLY installed (hook entry present but codex_hooks feature is disabled)");
        println!("Run `sessync hook install --tool codex` to enable the feature flag.");
    } else {
        println!("Status: NOT installed");
        println!("Run `sessync hook install --tool codex` to set it up.");
    }

    check_sessync_in_path();

    Ok(installed)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tool-agnostic dispatch entry points
// ─────────────────────────────────────────────────────────────────────────────

/// Install the Stop hook for the named tool.
/// Returns an error if the tool name is not recognised.
pub fn install_for_tool(tool: &str) -> Result<()> {
    match tool {
        "claude-code" => {
            let path = default_claude_settings_path()?;
            install_claude_code(&path)
        }
        "codex" => {
            let path = default_codex_config_path()?;
            install_codex_hook_at(&path)
        }
        other => bail!(
            "unknown tool {:?}. Known tools: {}",
            other,
            crate::adapter::registry::known_tool_names().join(", ")
        ),
    }
}

/// Uninstall the Stop hook for the named tool.
pub fn uninstall_for_tool(tool: &str) -> Result<()> {
    match tool {
        "claude-code" => {
            let path = default_claude_settings_path()?;
            uninstall_claude_code(&path)
        }
        "codex" => {
            let path = default_codex_config_path()?;
            uninstall_codex_hook_at(&path)
        }
        other => bail!(
            "unknown tool {:?}. Known tools: {}",
            other,
            crate::adapter::registry::known_tool_names().join(", ")
        ),
    }
}

/// Check whether the Stop hook is installed for the named tool.
/// Returns `Ok(true)` if fully installed, `Ok(false)` if not (or partial).
pub fn status_for_tool(tool: &str) -> Result<bool> {
    match tool {
        "claude-code" => {
            let path = default_claude_settings_path()?;
            status_claude_code(&path)
        }
        "codex" => {
            let path = default_codex_config_path()?;
            status_codex_hook_at(&path)
        }
        other => bail!(
            "unknown tool {:?}. Known tools: {}",
            other,
            crate::adapter::registry::known_tool_names().join(", ")
        ),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CLI subcommand definition
// ─────────────────────────────────────────────────────────────────────────────

#[derive(clap::Subcommand)]
pub enum HookAction {
    /// Install the Stop hook (Claude Code: settings.json; Codex: config.toml).
    Install {
        /// Tool to install for. Default: claude-code.
        #[arg(long, default_value = "claude-code")]
        tool: String,
    },
    /// Remove the sessync Stop hook for the given tool.
    Uninstall {
        /// Tool to uninstall for. Default: claude-code.
        #[arg(long, default_value = "claude-code")]
        tool: String,
    },
    /// Show whether the Stop hook is currently installed for the given tool.
    Status {
        /// Tool to check. Default: claude-code.
        #[arg(long, default_value = "claude-code")]
        tool: String,
    },
}

pub fn run(action: HookAction) -> Result<()> {
    match action {
        HookAction::Install { tool } => install_for_tool(&tool),
        HookAction::Uninstall { tool } => uninstall_for_tool(&tool),
        HookAction::Status { tool } => status_for_tool(&tool).map(|_| ()),
    }
}
