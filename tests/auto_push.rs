//! Integration tests for `sessync auto-push`.
//!
//! Full setup/teardown requires a real Claude settings.json and (on macOS)
//! launchctl, so tests here focus on the parseable surface and the hook
//! sub-logic, which is already tested in `hook_install.rs`.

use std::process::Command;

/// Smoke-test: `sessync auto-push --help` must parse without error.
#[test]
fn auto_push_help_parses() {
    // Run the built binary so we exercise the real CLI parser.
    // If the binary isn't built yet this will fail with "No such file" — in
    // that case `cargo test` will compile it first, which is the normal flow.
    let bin = env!("CARGO_BIN_EXE_sessync");
    let out = Command::new(bin)
        .args(["auto-push", "--help"])
        .output()
        .expect("failed to launch sessync binary");
    assert!(
        out.status.success(),
        "auto-push --help exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("setup") && stdout.contains("teardown") && stdout.contains("status"),
        "expected subcommand names in --help output: {stdout}"
    );
}

/// Smoke-test: `sessync auto-push setup --help` must parse without error.
#[test]
fn auto_push_setup_help_parses() {
    let bin = env!("CARGO_BIN_EXE_sessync");
    let out = Command::new(bin)
        .args(["auto-push", "setup", "--help"])
        .output()
        .expect("failed to launch sessync binary");
    assert!(
        out.status.success(),
        "auto-push setup --help exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
