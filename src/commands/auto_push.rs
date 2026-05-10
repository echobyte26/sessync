//! `sessync auto-push` — unified setup / teardown / status for the Stop hook
//! and the macOS launchd periodic-push agent.

use anyhow::Result;

#[derive(clap::Subcommand)]
pub enum AutoPushAction {
    /// Install the Stop hook and (macOS) the launchd agent in one step.
    Setup,
    /// Remove the Stop hook and (macOS) the launchd agent.
    Teardown,
    /// Show the installation state of the hook and the launchd agent.
    Status,
}

pub fn run(action: AutoPushAction) -> Result<()> {
    match action {
        AutoPushAction::Setup => setup(),
        AutoPushAction::Teardown => teardown(),
        AutoPushAction::Status => status(),
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn default_settings_path() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").map_err(|_| {
        anyhow::anyhow!("$HOME is not set — cannot resolve Claude settings.json path")
    })?;
    Ok(std::path::PathBuf::from(home)
        .join(".claude")
        .join("settings.json"))
}

// ── setup ──────────────────────────────────────────────────────────────────────

fn setup() -> Result<()> {
    println!("Setting up auto-push...");

    // Step 1: Stop hook.
    let settings_path = default_settings_path()?;
    match super::hook::install_hook_at(&settings_path) {
        Ok(()) => println!("✓ Stop hook installed"),
        Err(e) => println!("✗ Stop hook: {e}"),
    }

    // Step 2: launchd agent (macOS only).
    #[cfg(target_os = "macos")]
    {
        use super::launchd;
        let plist_path = match launchd::default_plist_path_pub() {
            Ok(p) => p,
            Err(e) => {
                println!("✗ launchd agent: {e}");
                println!("Run `sessync status` to verify.");
                return Ok(());
            }
        };
        let binary_path = match launchd::resolve_binary_path() {
            Ok(p) => p,
            Err(e) => {
                println!("✗ launchd agent: {e}");
                println!("Run `sessync status` to verify.");
                return Ok(());
            }
        };
        let log_dir = match launchd::default_log_dir_pub() {
            Ok(d) => d,
            Err(e) => {
                println!("✗ launchd agent: {e}");
                println!("Run `sessync status` to verify.");
                return Ok(());
            }
        };
        match launchd::install_at(&plist_path, &binary_path, &log_dir, true) {
            Ok(()) => println!("✓ launchd agent installed (macOS)"),
            Err(e) => println!("✗ launchd agent: {e}"),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        println!("  (launchd agent skipped — not macOS)");
    }

    println!("Run `sessync status` to verify.");
    Ok(())
}

// ── teardown ───────────────────────────────────────────────────────────────────

fn teardown() -> Result<()> {
    println!("Tearing down auto-push...");

    // Remove Stop hook — best effort.
    let settings_path = match default_settings_path() {
        Ok(p) => p,
        Err(e) => {
            println!("✗ Stop hook: {e}");
            return finish_teardown();
        }
    };
    match super::hook::uninstall_hook_at(&settings_path) {
        Ok(()) => println!("✓ Stop hook removed"),
        Err(e) => println!("✗ Stop hook: {e}"),
    }

    finish_teardown()
}

fn finish_teardown() -> Result<()> {
    // Remove launchd agent (macOS only) — best effort.
    #[cfg(target_os = "macos")]
    {
        use super::launchd;
        match launchd::default_plist_path_pub() {
            Ok(plist_path) => match launchd::uninstall_at(&plist_path) {
                Ok(()) => println!("✓ launchd agent removed (macOS)"),
                Err(e) => println!("✗ launchd agent: {e}"),
            },
            Err(e) => println!("✗ launchd agent: {e}"),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        println!("  (launchd agent skipped — not macOS)");
    }

    Ok(())
}

// ── status ─────────────────────────────────────────────────────────────────────

fn status() -> Result<()> {
    println!("=== auto-push status ===");

    // Hook status.
    let settings_path = match default_settings_path() {
        Ok(p) => p,
        Err(e) => {
            println!("Stop hook: error resolving settings path: {e}");
            return launchd_status();
        }
    };
    println!("--- Stop hook ---");
    match super::hook::status_hook_at(&settings_path) {
        Ok(_) => {}
        Err(e) => println!("Stop hook check error: {e}"),
    }

    launchd_status()
}

fn launchd_status() -> Result<()> {
    println!("--- launchd agent ---");

    #[cfg(target_os = "macos")]
    {
        use super::launchd;
        match launchd::default_plist_path_pub() {
            Ok(plist_path) => match launchd::status_at(&plist_path) {
                Ok(_) => {}
                Err(e) => println!("launchd check error: {e}"),
            },
            Err(e) => println!("launchd check error: {e}"),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        println!("  (launchd not applicable — not macOS)");
    }

    Ok(())
}
