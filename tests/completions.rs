use std::process::Command;

#[test]
fn completions_zsh_starts_with_compdef() {
    let bin = env!("CARGO_BIN_EXE_sessync");
    let output = Command::new(bin)
        .args(["completions", "zsh"])
        .output()
        .expect("failed to run sessync completions zsh");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("#compdef sessync"),
        "expected stdout to start with '#compdef sessync', got: {:?}",
        &stdout[..stdout.len().min(80)]
    );
}

#[test]
fn completions_bash_contains_function_name() {
    let bin = env!("CARGO_BIN_EXE_sessync");
    let output = Command::new(bin)
        .args(["completions", "bash"])
        .output()
        .expect("failed to run sessync completions bash");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("_sessync()"),
        "expected stdout to contain '_sessync()', got: {:?}",
        &stdout[..stdout.len().min(200)]
    );
}

#[test]
fn completions_fish_contains_complete_keyword() {
    let bin = env!("CARGO_BIN_EXE_sessync");
    let output = Command::new(bin)
        .args(["completions", "fish"])
        .output()
        .expect("failed to run sessync completions fish");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("complete -c sessync"),
        "expected stdout to contain 'complete -c sessync', got: {:?}",
        &stdout[..stdout.len().min(200)]
    );
}
