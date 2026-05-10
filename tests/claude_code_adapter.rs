use sessync::adapter::claude_code::ClaudeCodeAdapter;
use sessync::adapter::tool::ToolAdapter;
use sessync::types::SessionId;
use std::io::Write as _;
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_projects")
}

#[tokio::test]
async fn list_local_sessions_finds_fixture() {
    let adapter = ClaudeCodeAdapter::with_root(fixture_root());
    let sessions = adapter.list_local_sessions().await.unwrap();
    assert!(sessions.iter().any(|s| s.meta.session_id.0 == "abc123-def"));
}

#[tokio::test]
async fn read_session_returns_raw_bytes() {
    let adapter = ClaudeCodeAdapter::with_root(fixture_root());
    let bytes = adapter
        .read_session(&SessionId("abc123-def".into()))
        .await
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("hello world"));
    assert!(text.contains("\"type\":\"user\""));
}

#[tokio::test]
async fn write_session_creates_file_under_target_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let adapter = ClaudeCodeAdapter::with_root(tmp.path().to_path_buf());
    let written = adapter
        .write_session(
            &SessionId("xyz-789".into()),
            "/Users/test/some/cwd",
            b"{\"type\":\"user\"}\n",
        )
        .await
        .unwrap();
    assert!(written.exists());
    let dir = written.parent().unwrap();
    assert_eq!(
        dir.file_name().unwrap().to_str().unwrap(),
        "-Users-test-some-cwd"
    );
}

/// Q3: A 2 MiB junk line followed by a real user message. The adapter must
/// skip the oversized line and return the normal preview.
#[tokio::test]
async fn preview_skips_oversize_lines() {
    let tmp = tempfile::tempdir().unwrap();
    // Write a single JSONL file directly into a project sub-dir.
    let proj_dir = tmp.path().join("-tmp-preview-skip");
    std::fs::create_dir_all(&proj_dir).unwrap();
    let jsonl_path = proj_dir.join("session-oversize.jsonl");
    let mut f = std::fs::File::create(&jsonl_path).unwrap();
    // First line: 2 MiB of junk (not valid JSON, definitely > 1 MiB limit).
    let junk = "x".repeat(2 * 1_048_576);
    writeln!(f, "{junk}").unwrap();
    // Second line: valid user message.
    writeln!(
        f,
        r#"{{"type":"user","message":{{"role":"user","content":"hello from oversize test"}}}}"#
    )
    .unwrap();

    let adapter = ClaudeCodeAdapter::with_root(tmp.path().to_path_buf());
    let sessions = adapter.list_local_sessions().await.unwrap();
    let session = sessions
        .iter()
        .find(|s| s.meta.session_id.0 == "session-oversize")
        .expect("session not found");
    assert_eq!(session.meta.preview, "hello from oversize test");
}

/// Q3: Sanity baseline — normal user message is previewed correctly.
#[tokio::test]
async fn preview_returns_normal_user_message() {
    let adapter = ClaudeCodeAdapter::with_root(fixture_root());
    let sessions = adapter.list_local_sessions().await.unwrap();
    let session = sessions
        .iter()
        .find(|s| s.meta.session_id.0 == "abc123-def")
        .expect("fixture session not found");
    assert_eq!(session.meta.preview, "hello world");
}
