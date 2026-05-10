// The core behavior of `sessync upgrade` is shelling out to `brew update` and
// `brew upgrade sessync`.  There is no pure logic worth unit-testing, and
// calling the real brew in CI would require a macOS runner with Homebrew
// installed — so real-brew invocation is a manual test only.
//
// This file contains one integration test: verify the subcommand is wired into
// the CLI (parses cleanly and shows help) without invoking brew at all.

use std::process::Command;

#[test]
fn upgrade_help_exits_zero() {
    let bin = env!("CARGO_BIN_EXE_sessync");
    let output = Command::new(bin)
        .args(["upgrade", "--help"])
        .output()
        .expect("failed to run sessync upgrade --help");

    assert!(
        output.status.success(),
        "sessync upgrade --help exited non-zero: {}",
        output.status
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("brew"),
        "expected 'brew' in upgrade --help output, got:\n{stdout}"
    );
}
