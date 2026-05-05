//! Integration tests for B1 shared-salt fix (2026-05-05).
//!
//! Two simulated devices pointing at the same backend must end up with
//! identical salts — and therefore identical derived keys — even though each
//! calls `load_or_create_salt` independently.

use sessync::adapter::memory::InMemoryStorage;
use sessync::adapter::storage::StorageAdapter;
use sessync::commands::init::load_or_create_salt;

#[tokio::test]
async fn first_init_creates_salt_on_backend() {
    let storage = InMemoryStorage::new();
    let salt = load_or_create_salt(&storage).await.unwrap();

    // The helper must have persisted the salt to the well-known key.
    let stored = storage.get(".sessync-salt").await.unwrap();
    assert_eq!(stored.as_slice(), &salt);
    assert_eq!(salt.len(), 16);
}

#[tokio::test]
async fn second_init_reuses_existing_salt() {
    let storage = InMemoryStorage::new();

    // Device A: first init — creates and uploads the salt.
    let salt_a = load_or_create_salt(&storage).await.unwrap();

    // Device B (or re-init on same device): must reuse, not replace.
    let salt_b = load_or_create_salt(&storage).await.unwrap();

    assert_eq!(
        salt_a, salt_b,
        "second init should reuse the salt the first init uploaded"
    );
}

#[tokio::test]
async fn corrupt_salt_size_is_rejected() {
    let storage = InMemoryStorage::new();

    // Pre-seed a salt with wrong size (3 bytes instead of 16).
    storage
        .put(".sessync-salt", vec![1, 2, 3])
        .await
        .unwrap();

    let err = load_or_create_salt(&storage).await.unwrap_err();
    assert!(
        err.to_string().contains("16 bytes"),
        "expected error mentioning '16 bytes', got: {err}"
    );
}
