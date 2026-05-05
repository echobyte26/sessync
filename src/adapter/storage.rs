use crate::error::Result;
use async_trait::async_trait;

/// A blob storage backend. Operations are keyed by opaque string keys.
/// Implementations: `OssStorage` (production), `InMemoryStorage` (tests).
#[async_trait]
pub trait StorageAdapter: Send + Sync {
    /// Upload bytes under `key`. Overwrites if exists.
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()>;

    /// Download bytes for `key`. Returns Err if missing.
    async fn get(&self, key: &str) -> Result<Vec<u8>>;

    /// List keys under a given prefix (no trailing-slash semantics; literal prefix match).
    async fn list(&self, prefix: &str) -> Result<Vec<StorageObject>>;

    /// Delete `key`. Idempotent (no error if missing).
    async fn delete(&self, key: &str) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct StorageObject {
    pub key: String,
    pub size: u64,
    /// Preserved as-returned by the backend. Precision varies (OSS reports
    /// second-level RFC3339; LocalFsStorage uses fs metadata which may carry
    /// nanosecond resolution on APFS). The meta cache compares this field
    /// bytewise — if a future backend returns a different precision for the
    /// same logical object, every cache entry will silently miss.
    pub last_modified: chrono::DateTime<chrono::Utc>,
}
