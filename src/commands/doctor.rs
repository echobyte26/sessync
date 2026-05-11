//! `sessync doctor` — battery of diagnostic checks.
//!
//! Each check produces a `CheckResult` (Pass / Fail / Info).  The checks are
//! grouped into sections matching the failure modes users most often hit:
//! config, storage, hook, launchd (macOS only), queue, cache, PATH.

use crate::adapter::oss::OssStorage;
use crate::adapter::registry::all_adapters;
use crate::adapter::storage::StorageAdapter;
use crate::config::{Config, StorageKind};
use crate::error::SessyncError;
use crate::queue::Queue;
use crate::ui::style;
use anyhow::Result;

// ── public types (tested in tests/doctor.rs) ─────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    Pass(String),
    Fail { reason: String, hint: Option<String> },
    Info(String),
}

// ── pure classifiers (unit-testable, no I/O) ─────────────────────────────────

/// Classify the consecutive-failure count from the queue.
/// Threshold >= 3 matches the notify threshold used by the daemon.
pub fn classify_consecutive_failures(n: u32) -> CheckResult {
    if n >= 3 {
        CheckResult::Fail {
            reason: format!("{n} consecutive push failures"),
            hint: Some(
                "run `sessync push` manually and check for auth / network errors".to_string(),
            ),
        }
    } else {
        CheckResult::Pass(format!("{n} consecutive failures (below threshold)"))
    }
}

/// Classify a storage error into an actionable doctor hint.
///
/// Distinguishes auth failures (bad creds) from network / DNS failures so the
/// user knows whether to rotate keys or check their internet connection.
pub fn classify_storage_error(err: &SessyncError) -> CheckResult {
    let msg = err.to_string();
    if msg.contains("InvalidAccessKeyId") || msg.contains("SignatureDoesNotMatch") {
        CheckResult::Fail {
            reason: format!("auth error: {msg}"),
            hint: Some(
                "check access_key_id / access_key_secret in ~/.config/sessync/config.toml"
                    .to_string(),
            ),
        }
    } else if msg.contains("dns")
        || msg.contains("DNS")
        || msg.contains("timed out")
        || msg.contains("connection refused")
        || msg.contains("network")
        || msg.contains("tcp")
        || msg.contains("hyper")
        || msg.contains("reqwest")
    {
        CheckResult::Fail {
            reason: format!("network/DNS error: {msg}"),
            hint: Some(
                "check your internet connection and the `endpoint` value in config.toml"
                    .to_string(),
            ),
        }
    } else {
        CheckResult::Fail {
            reason: msg,
            hint: Some("check storage config and credentials".to_string()),
        }
    }
}

// ── printer ───────────────────────────────────────────────────────────────────

fn print_check(name: &str, result: &CheckResult) {
    match result {
        CheckResult::Pass(detail) => {
            println!(
                "  {} {} — {}",
                style::success(style::check_ok()),
                style::header(name),
                detail
            );
        }
        CheckResult::Fail { reason, hint } => {
            println!(
                "  {} {} — {}",
                style::error(style::cross_fail()),
                style::header(name),
                style::error(reason)
            );
            if let Some(h) = hint {
                println!("      {} {}", style::dim("hint:"), style::hint(h));
            }
        }
        CheckResult::Info(detail) => {
            println!(
                "  {} {} — {}",
                style::dim("·"),
                style::header(name),
                style::dim(detail)
            );
        }
    }
}

fn print_section(name: &str) {
    println!();
    println!("  {}", style::header(name));
}

// ── individual checks ─────────────────────────────────────────────────────────

fn check_config_file_exists() -> CheckResult {
    let path = Config::default_path();
    if path.exists() {
        CheckResult::Pass(path.display().to_string())
    } else {
        CheckResult::Fail {
            reason: format!("{} not found", path.display()),
            hint: Some("run `sessync init`".to_string()),
        }
    }
}

fn check_config_parseable() -> (CheckResult, Option<Config>) {
    let path = Config::default_path();
    if !path.exists() {
        return (
            CheckResult::Info("skipped (config file missing)".to_string()),
            None,
        );
    }
    match Config::load(&path) {
        Ok(cfg) => (CheckResult::Pass("parsed OK".to_string()), Some(cfg)),
        Err(e) => (
            CheckResult::Fail {
                reason: format!("{e}"),
                hint: Some(
                    "fix the TOML syntax or re-run `sessync init` to regenerate".to_string(),
                ),
            },
            None,
        ),
    }
}

fn check_passphrase_file_exists() -> CheckResult {
    if crate::passphrase_store::passphrase_is_set() {
        CheckResult::Pass("set".to_string())
    } else {
        CheckResult::Fail {
            reason: "passphrase file missing".to_string(),
            hint: Some("run `sessync init`".to_string()),
        }
    }
}

async fn check_storage_reachable(cfg: &Config) -> CheckResult {
    match cfg.storage_kind {
        StorageKind::Oss => {
            let oss_cfg = match cfg.oss.as_ref() {
                Some(c) => c,
                None => {
                    return CheckResult::Fail {
                        reason: "storage_kind = oss but [oss] section missing".to_string(),
                        hint: Some("add [oss] section to config.toml".to_string()),
                    }
                }
            };
            let storage = match OssStorage::new(oss_cfg) {
                Ok(s) => s,
                Err(e) => return classify_storage_error(&e),
            };
            // Use a sentinel prefix that will never match real objects.
            // An empty result is healthy; an auth/network error is a failure.
            match storage.list("__sessync_doctor__").await {
                Ok(_) => CheckResult::Pass(format!("oss://{} reachable", oss_cfg.bucket)),
                Err(e) => classify_storage_error(&e),
            }
        }
        StorageKind::LocalFs => {
            let lf_cfg = match cfg.local_fs.as_ref() {
                Some(c) => c,
                None => {
                    return CheckResult::Fail {
                        reason: "storage_kind = local-fs but [local_fs] section missing"
                            .to_string(),
                        hint: Some("add [local_fs] section to config.toml".to_string()),
                    }
                }
            };
            let root = &lf_cfg.root;
            if !root.exists() {
                return CheckResult::Fail {
                    reason: format!("{} does not exist", root.display()),
                    hint: Some("run `sessync init --mock` or create the directory".to_string()),
                };
            }
            // Check write access with a sentinel file (avoids tempfile dep in prod).
            let probe = root.join(".sessync-doctor-probe");
            match std::fs::write(&probe, b"") {
                Ok(_) => {
                    let _ = std::fs::remove_file(&probe);
                    CheckResult::Pass(format!("{} exists and is writable", root.display()))
                }
                Err(e) => CheckResult::Fail {
                    reason: format!("{} not writable: {e}", root.display()),
                    hint: Some("check directory permissions".to_string()),
                },
            }
        }
    }
}

fn check_hook_installed_for_tool(tool: &str) -> CheckResult {
    match crate::commands::hook::status_for_tool(tool) {
        Ok(true) => CheckResult::Pass(format!("{tool} Stop hook installed")),
        Ok(false) => CheckResult::Fail {
            reason: format!("sessync Stop hook not found for {tool}"),
            hint: Some(format!("run `sessync hook install --tool {tool}`")),
        },
        Err(e) => CheckResult::Fail {
            reason: format!("checking {tool} hook: {e}"),
            hint: Some(format!("run `sessync hook install --tool {tool}`")),
        },
    }
}

// Kept for backward compatibility — delegates to the tool-agnostic version.
fn check_hook_installed() -> CheckResult {
    check_hook_installed_for_tool("claude-code")
}

fn check_claude_settings_writable() -> CheckResult {
    let dir = match std::env::var("HOME") {
        Ok(home) => std::path::PathBuf::from(home).join(".claude"),
        Err(_) => {
            return CheckResult::Fail {
                reason: "$HOME not set".to_string(),
                hint: None,
            }
        }
    };

    // The dir might not exist yet (fresh machine). Create it implicitly so the
    // check tests real write access rather than dir presence.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return CheckResult::Fail {
            reason: format!("cannot create {}: {e}", dir.display()),
            hint: Some("check filesystem permissions".to_string()),
        };
    }

    let probe = dir.join(".sessync-doctor-probe");
    match std::fs::write(&probe, b"") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            CheckResult::Pass(format!("{} is writable", dir.display()))
        }
        Err(e) => CheckResult::Fail {
            reason: format!("{} not writable: {e}", dir.display()),
            hint: Some("check directory permissions".to_string()),
        },
    }
}

// launchd checks are macOS-only.
#[cfg(target_os = "macos")]
fn check_launchd_plist_present() -> CheckResult {
    let plist = match std::env::var("HOME") {
        Ok(home) => std::path::PathBuf::from(home)
            .join("Library/LaunchAgents/com.sessync.push.plist"),
        Err(_) => {
            return CheckResult::Fail {
                reason: "$HOME not set".to_string(),
                hint: None,
            }
        }
    };

    if plist.exists() {
        CheckResult::Pass(plist.display().to_string())
    } else {
        CheckResult::Fail {
            reason: format!("{} not found", plist.display()),
            hint: Some("install the launchd agent with `sessync install`".to_string()),
        }
    }
}

#[cfg(target_os = "macos")]
fn check_launchd_loaded() -> CheckResult {
    let out = std::process::Command::new("launchctl")
        .args(["list"])
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if stdout.contains("com.sessync.push") {
                CheckResult::Pass("com.sessync.push is loaded".to_string())
            } else {
                CheckResult::Fail {
                    reason: "com.sessync.push not found in launchctl list".to_string(),
                    hint: Some(
                        "run: launchctl load ~/Library/LaunchAgents/com.sessync.push.plist"
                            .to_string(),
                    ),
                }
            }
        }
        Ok(o) => CheckResult::Fail {
            reason: format!(
                "launchctl exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr)
            ),
            hint: None,
        },
        Err(e) => CheckResult::Fail {
            reason: format!("could not run launchctl: {e}"),
            hint: None,
        },
    }
}

fn check_queue_db_accessible() -> (CheckResult, Option<Queue>) {
    match Queue::open_default() {
        Ok(q) => (CheckResult::Pass("queue DB accessible".to_string()), Some(q)),
        Err(e) => (
            CheckResult::Fail {
                reason: format!("{e}"),
                hint: Some(
                    "check ~/.local/share/sessync/ directory permissions".to_string(),
                ),
            },
            None,
        ),
    }
}

fn check_pending_count(q: &Queue) -> CheckResult {
    let n = q.list_pending().map(|v| v.len()).unwrap_or(0);
    CheckResult::Info(format!("{n} pending item(s)"))
}

fn check_consecutive_failures(q: &Queue) -> CheckResult {
    // If we can't read the streak, treat as 0 — failing the queue check
    // itself is what surfaces the underlying problem.
    classify_consecutive_failures(q.consecutive_failures().unwrap_or(0))
}

fn check_cache_file_present() -> CheckResult {
    match crate::cache::default_cache_path() {
        Ok(p) if p.exists() => CheckResult::Pass(p.display().to_string()),
        Ok(p) => CheckResult::Info(format!(
            "{} not present (normal on first run)",
            p.display()
        )),
        Err(e) => CheckResult::Info(format!("cache path unknown: {e}")),
    }
}

fn check_sessync_in_path() -> CheckResult {
    let ok = std::process::Command::new("which")
        .arg("sessync")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if ok {
        CheckResult::Pass("`sessync` found in PATH".to_string())
    } else {
        CheckResult::Fail {
            reason: "`sessync` not found in PATH".to_string(),
            hint: Some(
                "the Stop hook runs `sessync push`; add the binary to PATH or run `sessync install`"
                    .to_string(),
            ),
        }
    }
}

// ── main entry point ──────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    println!("\n{}", style::header("sessync doctor"));

    let mut passed = 0u32;
    let mut failed = 0u32;

    macro_rules! record {
        ($r:expr) => {{
            let r: CheckResult = $r;
            match &r {
                CheckResult::Pass(_) => passed += 1,
                CheckResult::Fail { .. } => failed += 1,
                CheckResult::Info(_) => {} // info items don't count for pass/fail
            }
            r
        }};
    }

    // ── Config ────────────────────────────────────────────────────────────────
    print_section("Config");

    let r = record!(check_config_file_exists());
    print_check("config_file_exists", &r);

    let (r, cfg_opt) = check_config_parseable();
    let r = record!(r);
    print_check("config_parseable", &r);

    let r = record!(check_passphrase_file_exists());
    print_check("passphrase_file_exists", &r);

    // ── Storage ───────────────────────────────────────────────────────────────
    print_section("Storage");

    let r = if let Some(ref cfg) = cfg_opt {
        record!(check_storage_reachable(cfg).await)
    } else {
        record!(CheckResult::Info(
            "skipped (config not loaded)".to_string()
        ))
    };
    print_check("storage_reachable", &r);

    // ── Hook ──────────────────────────────────────────────────────────────────
    print_section("Hook");

    let r = record!(check_hook_installed());
    print_check("claude_code_hook_installed", &r);

    let r = record!(check_hook_installed_for_tool("codex"));
    print_check("codex_hook_installed", &r);

    let r = record!(check_claude_settings_writable());
    print_check("claude_settings_writable", &r);

    // ── launchd (macOS only) ──────────────────────────────────────────────────
    #[cfg(target_os = "macos")]
    {
        print_section("launchd");

        let r = record!(check_launchd_plist_present());
        print_check("launchd_plist_present", &r);

        let r = record!(check_launchd_loaded());
        print_check("launchd_loaded", &r);
    }

    // ── Queue ─────────────────────────────────────────────────────────────────
    print_section("Queue");

    let (r, queue_opt) = check_queue_db_accessible();
    let r = record!(r);
    print_check("queue_db_accessible", &r);

    if let Some(ref q) = queue_opt {
        // pending_count is Info — not pass/fail.
        let r = check_pending_count(q);
        print_check("pending_count", &r);

        let r = record!(check_consecutive_failures(q));
        print_check("consecutive_failures", &r);
    } else {
        print_check(
            "pending_count",
            &CheckResult::Info("skipped (queue unavailable)".to_string()),
        );
        print_check(
            "consecutive_failures",
            &CheckResult::Info("skipped (queue unavailable)".to_string()),
        );
    }

    // ── Cache ─────────────────────────────────────────────────────────────────
    print_section("Cache");

    // cache_file_present is Info only — absence is normal on first run.
    let r = check_cache_file_present();
    print_check("cache_file_present", &r);

    // ── Tools ─────────────────────────────────────────────────────────────────
    print_section("Tools");

    let adapters = all_adapters();
    for adapter in &adapters {
        // Count local sessions — this is a best-effort diagnostic; never fail doctor on it.
        let local_count = match adapter.list_local_sessions().await {
            Ok(sessions) => sessions.len(),
            Err(e) => {
                print_check(
                    adapter.name(),
                    &CheckResult::Info(format!("could not list sessions: {e}")),
                );
                continue;
            }
        };
        let r = if local_count > 0 {
            CheckResult::Pass(format!("{} local session(s)", local_count))
        } else {
            CheckResult::Info("0 local sessions (tool may not be in use yet)".to_string())
        };
        print_check(adapter.name(), &r);
    }

    // ── PATH ──────────────────────────────────────────────────────────────────
    print_section("PATH");

    let r = record!(check_sessync_in_path());
    print_check("sessync_in_path", &r);

    // ── Summary ───────────────────────────────────────────────────────────────
    println!();
    println!("{}", style::dim("━".repeat(40).as_str()));
    let summary = format!("{passed} checks passed, {failed} failed");
    if failed == 0 {
        println!("{}", style::success(&summary));
    } else {
        println!("{}", style::error(&summary));
    }
    println!();

    if failed > 0 {
        anyhow::bail!("{failed} check(s) failed — see hints above");
    }
    Ok(())
}
