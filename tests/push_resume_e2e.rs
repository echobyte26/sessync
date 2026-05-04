use sessync::adapter::claude_code::ClaudeCodeAdapter;
use sessync::adapter::memory::InMemoryStorage;
use sessync::adapter::storage::StorageAdapter;
use sessync::commands::push;
use sessync::types::SessionMeta;
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_projects")
}

fn fixture_session_path() -> PathBuf {
    fixture_root().join("-tmp-test-foo/abc123-def.jsonl")
}

#[tokio::test]
async fn push_uploads_encrypted_session_to_storage() {
    let tool = ClaudeCodeAdapter::with_root(fixture_root());
    let storage = InMemoryStorage::new();
    let key = [9u8; 32];

    push::push_all(&tool, &storage, &key).await.unwrap();

    let listed = storage.list("claude-code/").await.unwrap();
    assert!(listed.iter().any(|o| o.key.ends_with(".age")));
    assert!(listed.iter().any(|o| o.key.ends_with(".meta.json")));

    // Verify the .age object is NOT plaintext
    let age_key = listed
        .iter()
        .find(|o| o.key.ends_with(".age") && !o.key.contains(".meta."))
        .unwrap()
        .key
        .clone();
    let ct = storage.get(&age_key).await.unwrap();
    assert!(
        !String::from_utf8_lossy(&ct).contains("hello world"),
        "ciphertext should not contain plaintext substring",
    );
}

#[tokio::test]
async fn push_then_decrypt_roundtrips_content_and_meta() {
    let tool = ClaudeCodeAdapter::with_root(fixture_root());
    let storage = InMemoryStorage::new();
    let key = [9u8; 32];

    push::push_all(&tool, &storage, &key).await.unwrap();

    let listed = storage.list("claude-code/").await.unwrap();
    let content_key = listed
        .iter()
        .find(|o| o.key.ends_with(".age") && !o.key.contains(".meta."))
        .unwrap()
        .key
        .clone();
    let meta_key = listed
        .iter()
        .find(|o| o.key.ends_with(".meta.json"))
        .unwrap()
        .key
        .clone();

    // Content roundtrip — bytes must equal the on-disk fixture verbatim.
    let content_ct = storage.get(&content_key).await.unwrap();
    let content_pt = sessync::crypto::decrypt(&content_ct, &key).unwrap();
    let on_disk = std::fs::read(fixture_session_path()).unwrap();
    assert_eq!(content_pt, on_disk);

    // Meta roundtrip — decrypted bytes must deserialize back to a SessionMeta.
    let meta_ct = storage.get(&meta_key).await.unwrap();
    let meta_pt = sessync::crypto::decrypt(&meta_ct, &key).unwrap();
    let meta: SessionMeta = serde_json::from_slice(&meta_pt).unwrap();
    assert_eq!(meta.session_id.0, "abc123-def");
    assert_eq!(meta.schema_version, 1);
}

#[tokio::test]
async fn push_on_empty_fixture_uploads_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = ClaudeCodeAdapter::with_root(tmp.path().to_path_buf());
    let storage = InMemoryStorage::new();
    let key = [9u8; 32];

    push::push_all(&tool, &storage, &key).await.unwrap();

    let listed = storage.list("claude-code/").await.unwrap();
    assert!(listed.is_empty(), "expected no uploads, got {:?}", listed);
}
