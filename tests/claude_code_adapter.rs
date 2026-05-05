use sessync::adapter::claude_code::ClaudeCodeAdapter;
use sessync::adapter::tool::ToolAdapter;
use sessync::types::SessionId;
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
