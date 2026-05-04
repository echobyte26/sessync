use sessync::adapter::claude_code::ClaudeCodeAdapter;
use sessync::adapter::memory::InMemoryStorage;
use sessync::adapter::storage::StorageAdapter;
use sessync::commands::push;
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_projects")
}

#[tokio::test]
async fn push_uploads_encrypted_session_to_storage() {
    let tool = ClaudeCodeAdapter::with_root(fixture_root());
    let storage = InMemoryStorage::new();
    let key = [9u8; 32];

    push::push_all(&tool, &storage, &key, "test-device").await.unwrap();

    let listed = storage.list("claude-code/").await.unwrap();
    assert!(listed.iter().any(|o| o.key.ends_with(".age")));
    assert!(listed.iter().any(|o| o.key.ends_with(".meta.json")));

    // Verify the .age object is NOT plaintext
    let age_key = listed.iter().find(|o| o.key.ends_with(".age") && !o.key.contains(".meta.")).unwrap().key.clone();
    let ct = storage.get(&age_key).await.unwrap();
    assert!(!String::from_utf8_lossy(&ct).contains("hello world"),
        "ciphertext should not contain plaintext substring");
}
