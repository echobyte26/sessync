use sessync::adapter::claude_code::ClaudeCodeAdapter;
use sessync::adapter::tool::ToolAdapter;

/// Smoke test: the function must return a bool without panicking.
/// We do not assert a specific value because `claude` may or may not be
/// present on PATH in any given environment.
#[test]
fn claude_in_path_returns_bool() {
    let adapter = ClaudeCodeAdapter::new();
    let _: bool = adapter.launch_binary_on_path();
}

/// When PATH is forced to an empty directory, `claude` cannot be found.
/// We validate this in a child process to avoid mutating the test-runner's
/// PATH, which would be racy under parallel test execution.
#[test]
fn claude_not_found_when_path_is_empty_dir() {
    let tmp = tempfile::tempdir().unwrap();

    // Re-invoke ourselves as a subprocess with a sterile PATH so we can
    // assert the return value without any cross-test interference.
    let out = std::process::Command::new(
        std::env::current_exe().expect("test binary path"),
    )
    .args([
        "--test",
        "resume_launch",
        "--include-ignored",
        "--",
        "claude_in_path_empty_path_inner",
        "--nocapture",
    ])
    .env("PATH", tmp.path())
    .output()
    .expect("failed to spawn subprocess");

    assert!(
        out.status.success(),
        "inner subprocess test failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Inner test invoked by `claude_not_found_when_path_is_empty_dir` with PATH
/// set to an empty temporary directory — asserts the helper returns false.
///
/// Marked `#[ignore]` so it only runs when explicitly invoked by its parent
/// test (via `--include-ignored`); running it with a normal PATH would fail
/// spuriously on machines where `claude` is installed.
#[test]
#[ignore]
fn claude_in_path_empty_path_inner() {
    let adapter = ClaudeCodeAdapter::new();
    assert!(
        !adapter.launch_binary_on_path(),
        "launch_binary_on_path() should return false when PATH has no executables"
    );
}
