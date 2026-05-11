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
fn macos_major_version_doctor() -> Option<u32> {
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

#[cfg(target_os = "macos")]
fn check_launchd_loaded() -> CheckResult {
    use crate::commands::launchd::tahoe_hint_for_macos_major;

    // Use the modern `launchctl print` API: exits 0 if loaded, non-zero if not.
    // This is consistent with `is_loaded_via_launchctl()` in launchd.rs.
    let uid = unsafe { libc::getuid() };
    let service = format!("gui/{uid}/com.sessync.push");

    let out = std::process::Command::new("launchctl")
        .args(["print", &service])
        .output();

    let loaded = match out {
        Ok(ref o) => o.status.success(),
        Err(_) => false,
    };

    if loaded {
        return CheckResult::Pass("com.sessync.push is loaded".to_string());
    }

    // Not loaded — build an actionable hint.
    // Check whether the plist file is present: if yes, this is likely the
    // Tahoe brew-upgrade approval invalidation issue on macOS 15+.
    let plist_exists = std::env::var("HOME")
        .map(|home| {
            std::path::PathBuf::from(home)
                .join("Library/LaunchAgents/com.sessync.push.plist")
                .exists()
        })
        .unwrap_or(false);

    let hint = if plist_exists {
        let major = macos_major_version_doctor().unwrap_or(0);
        if let Some(tahoe) = tahoe_hint_for_macos_major(major) {
            // Prepend the re-run suggestion before the Tahoe-specific detail.
            format!(
                "agent installed but not loaded. {tahoe}"
            )
        } else {
            "agent installed but not loaded — run `sessync launchd install` to re-register"
                .to_string()
        }
    } else {
        "run `sessync launchd install` to set up the agent".to_string()
    };

    match out {
        Ok(_) => CheckResult::Fail {
            reason: "com.sessync.push not found in launchctl print".to_string(),
            hint: Some(hint),
        },
        Err(e) => CheckResult::Fail {
            reason: format!("could not run launchctl: {e}"),
            hint: Some(hint),
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

// ── Codex install checks ──────────────────────────────────────────────────────

/// Returns the Codex data home directory (`~/.codex/` by default, or
/// `$CODEX_HOME` if set).  This mirrors `default_codex_root()` in the adapter.
fn codex_home() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("CODEX_HOME") {
        return Some(std::path::PathBuf::from(p));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".codex"))
}

/// Check 1: does `~/.codex/` (or `$CODEX_HOME`) exist?
pub fn check_codex_dir_exists() -> CheckResult {
    match codex_home() {
        None => CheckResult::Info("$HOME not set; cannot locate ~/.codex/".to_string()),
        Some(dir) if dir.exists() => CheckResult::Pass(dir.display().to_string()),
        Some(dir) => CheckResult::Info(format!(
            "{} not found (Codex not installed or never run)",
            dir.display()
        )),
    }
}

/// Check 2: does a `state_*.sqlite` file exist inside the Codex home?
/// Returns the actual filename when found so the user knows which DB is active.
pub fn check_codex_sqlite_present() -> CheckResult {
    let dir = match codex_home() {
        Some(d) => d,
        None => return CheckResult::Info("$HOME not set; cannot locate Codex DB".to_string()),
    };

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            return CheckResult::Info(format!(
                "cannot read {}: {e}",
                dir.display()
            ))
        }
    };

    let mut best: Option<(u64, String)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(rest) = name.strip_prefix("state_") {
            if let Some(num_str) = rest.strip_suffix(".sqlite") {
                if let Ok(num) = num_str.parse::<u64>() {
                    match &best {
                        None => best = Some((num, name)),
                        Some((b, _)) if num > *b => best = Some((num, name)),
                        _ => {}
                    }
                }
            }
        }
    }

    match best {
        Some((_, fname)) => CheckResult::Pass(fname),
        None => CheckResult::Info(
            "no state_*.sqlite found in ~/.codex/ (Codex not yet run)".to_string(),
        ),
    }
}

/// Check 3: is the `codex` binary reachable (on PATH or via the macOS app bundle)?
pub fn check_codex_binary_reachable() -> CheckResult {
    // First try `which codex` / `codex --version`.
    let on_path = std::process::Command::new("codex")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_or(false, |o| o.status.success());

    if on_path {
        // Ask `which` for the resolved path for a friendlier message.
        let path = std::process::Command::new("which")
            .arg("codex")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "codex".to_string());
        return CheckResult::Pass(path);
    }

    // macOS desktop app fallback.
    const MACOS_BUNDLE: &str = "/Applications/Codex.app/Contents/Resources/codex";
    if std::path::Path::new(MACOS_BUNDLE).exists() {
        return CheckResult::Pass(MACOS_BUNDLE.to_string());
    }

    CheckResult::Info(
        "codex binary not found on PATH and /Applications/Codex.app not present".to_string(),
    )
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

    // ── Codex install verification ─────────────────────────────────────────────
    // These are Info-only — Codex is optional, so a missing install is never a failure.
    print_section("Codex");

    let dir_check = check_codex_dir_exists();
    print_check("codex_dir_exists", &dir_check);

    if matches!(dir_check, CheckResult::Info(_)) {
        // Codex directory absent — the remaining checks are meaningless.
        print_check(
            "codex_sqlite_present",
            &CheckResult::Info("skipped (Codex not installed)".to_string()),
        );
        print_check(
            "codex_binary_reachable",
            &CheckResult::Info("skipped (Codex not installed)".to_string()),
        );
    } else {
        let r = check_codex_sqlite_present();
        print_check("codex_sqlite_present", &r);

        let r = check_codex_binary_reachable();
        print_check("codex_binary_reachable", &r);
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
