use chrono::Utc;
use sessync::cache::*;
use sessync::types::*;
use std::collections::HashSet;
use tempfile::tempdir;

fn sample_meta(sid: &str) -> SessionMeta {
    SessionMeta {
        schema_version: 1,
        session_id: SessionId(sid.into()),
        project_key: ProjectKey("pk".into()),
        source_cwd: "/Users/test/x".into(),
        source_hostname: "test.local".into(),
        modified_at: Utc::now(),
        byte_size: 100,
        preview: "hi".into(),
        has_user_message: true,
    }
}

#[test]
fn save_then_load_roundtrips() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("c.age");
    let key = [42u8; 32];

    let mut c = MetaCache::empty("claude-code");
    c.insert("k1".into(), sample_meta("s1"), Utc::now(), 100);
    c.save(&path, &key).unwrap();

    let loaded = MetaCache::load_or_empty(&path, &key, "claude-code");
    assert_eq!(loaded.entries.len(), 1);
    assert!(loaded.entries.contains_key("k1"));
}

#[test]
fn load_with_wrong_key_returns_empty() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("c.age");

    let mut c = MetaCache::empty("claude-code");
    c.insert("k1".into(), sample_meta("s1"), Utc::now(), 100);
    c.save(&path, &[1u8; 32]).unwrap();

    let loaded = MetaCache::load_or_empty(&path, &[2u8; 32], "claude-code");
    assert_eq!(loaded.entries.len(), 0);
}

#[test]
fn load_with_wrong_tool_returns_empty() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("c.age");
    let key = [42u8; 32];

    let mut c = MetaCache::empty("claude-code");
    c.insert("k1".into(), sample_meta("s1"), Utc::now(), 100);
    c.save(&path, &key).unwrap();

    let loaded = MetaCache::load_or_empty(&path, &key, "codex");
    assert_eq!(loaded.entries.len(), 0);
}

#[test]
fn get_if_fresh_returns_none_on_mtime_change() {
    let mut c = MetaCache::empty("claude-code");
    let original = Utc::now();
    c.insert("k1".into(), sample_meta("s1"), original, 100);

    // Same key, different mtime → cache miss.
    let later = original + chrono::Duration::seconds(1);
    assert!(c.get_if_fresh("k1", later, 100).is_none());

    // Same key, different size → cache miss.
    assert!(c.get_if_fresh("k1", original, 200).is_none());

    // Exact match → hit.
    assert!(c.get_if_fresh("k1", original, 100).is_some());
}

#[test]
fn retain_only_drops_tombstoned_keys() {
    let mut c = MetaCache::empty("claude-code");
    c.insert("k1".into(), sample_meta("s1"), Utc::now(), 100);
    c.insert("k2".into(), sample_meta("s2"), Utc::now(), 100);

    let mut present = HashSet::new();
    present.insert("k1");
    c.retain_only(&present);

    assert_eq!(c.entries.len(), 1);
    assert!(c.entries.contains_key("k1"));
    assert!(!c.entries.contains_key("k2"));
}

#[cfg(unix)]
#[test]
fn save_writes_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir().unwrap();
    let path = dir.path().join("c.age");
    let key = [42u8; 32];

    MetaCache::empty("claude-code").save(&path, &key).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn load_from_nonexistent_returns_empty() {
    let dir = tempdir().unwrap();
    let loaded =
        MetaCache::load_or_empty(&dir.path().join("nope.age"), &[0u8; 32], "claude-code");
    assert!(loaded.entries.is_empty());
}

#[test]
fn load_with_corrupt_bytes_returns_empty() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("c.age");
    std::fs::write(&path, b"this is not a valid age file at all").unwrap();
    let loaded = MetaCache::load_or_empty(&path, &[0u8; 32], "claude-code");
    assert!(loaded.entries.is_empty());
}

#[test]
fn load_with_wrong_schema_version_returns_empty() {
    // Hand-craft an encrypted cache blob with schema_version = 999, then
    // confirm load_or_empty rejects it.
    let dir = tempdir().unwrap();
    let path = dir.path().join("c.age");
    let key = [42u8; 32];

    let body = serde_json::json!({
        "schema_version": 999,
        "tool": "claude-code",
        "entries": {}
    });
    let plaintext = serde_json::to_vec(&body).unwrap();
    let ciphertext = sessync::crypto::encrypt(&plaintext, &key).unwrap();
    std::fs::write(&path, ciphertext).unwrap();

    let loaded = MetaCache::load_or_empty(&path, &key, "claude-code");
    assert_eq!(loaded.entries.len(), 0);
}
