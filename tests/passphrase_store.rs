use sessync::passphrase_store;
use tempfile::tempdir;

#[test]
fn store_and_load_roundtrips_on_same_machine() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("passphrase.enc");
    passphrase_store::store_passphrase_at("hunter2", &path).unwrap();
    let loaded = passphrase_store::load_passphrase_at(&path).unwrap();
    assert_eq!(loaded, "hunter2");
}

#[test]
fn store_overwrites_existing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("passphrase.enc");
    passphrase_store::store_passphrase_at("first", &path).unwrap();
    passphrase_store::store_passphrase_at("second", &path).unwrap();
    let loaded = passphrase_store::load_passphrase_at(&path).unwrap();
    assert_eq!(loaded, "second");
}

#[test]
fn load_from_nonexistent_errors() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("nope.enc");
    assert!(passphrase_store::load_passphrase_at(&path).is_err());
}

#[test]
fn delete_is_idempotent() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("passphrase.enc");
    passphrase_store::delete_passphrase_at(&path).unwrap();  // doesn't exist
    passphrase_store::store_passphrase_at("x", &path).unwrap();
    passphrase_store::delete_passphrase_at(&path).unwrap();  // exists, removed
    passphrase_store::delete_passphrase_at(&path).unwrap();  // doesn't exist again
}

#[cfg(unix)]
#[test]
fn store_writes_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir().unwrap();
    let path = dir.path().join("passphrase.enc");
    passphrase_store::store_passphrase_at("x", &path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

// Cross-machine simulation: encrypt on this machine, replace machine-id
// implicitly by writing a file with random-bytes ciphertext, expect
// decrypt failure with the "different machine" hint.
#[test]
fn load_garbage_fails_with_helpful_error() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("passphrase.enc");
    std::fs::write(&path, b"random bytes that are not valid age ciphertext").unwrap();
    let err = passphrase_store::load_passphrase_at(&path).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("different machine") || msg.contains("unreadable"),
            "got: {msg}");
}
