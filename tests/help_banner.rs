use std::process::Command;

/// Smoke-test that `sessync --help` emits the EXAMPLES banner we injected via
/// clap's `after_help` attribute.
#[test]
fn help_banner_contains_examples_section() {
    let bin = env!("CARGO_BIN_EXE_sessync");

    let output = Command::new(bin)
        .arg("--help")
        .output()
        .expect("failed to run sessync --help");

    // clap prints help to stdout and exits 0.
    assert!(
        output.status.success(),
        "sessync --help exited with non-zero status: {}",
        output.status
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("EXAMPLES:"),
        "expected 'EXAMPLES:' in help output, got:\n{stdout}"
    );

    assert!(
        stdout.contains("sessync init"),
        "expected 'sessync init' in help output, got:\n{stdout}"
    );

    assert!(
        stdout.contains("sessync push --dry-run"),
        "expected 'sessync push --dry-run' in help output, got:\n{stdout}"
    );

    assert!(
        stdout.contains("sessync doctor"),
        "expected 'sessync doctor' in help output, got:\n{stdout}"
    );
}
