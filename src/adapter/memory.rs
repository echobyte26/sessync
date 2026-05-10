use super::storage::{StorageAdapter, StorageObject};
use crate::error::{Result, SessyncError};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

type Entry = (Vec<u8>, chrono::DateTime<chrono::Utc>);

#[derive(Default)]
pub struct InMemoryStorage {
    // Invariant: never .await while holding `inner` — keeps this a sync Mutex safely.
    inner: Mutex<HashMap<String, Entry>>,
}

impl InMemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a pre-existing entry with an explicit timestamp — for tests that
    /// need to control `last_modified` without waiting for real time to pass.
    #[cfg(test)]
    pub fn put_at(&self, key: &str, bytes: Vec<u8>, ts: chrono::DateTime<chrono::Utc>) {
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_string(), (bytes, ts));
    }
}

/// Synthesise an ETag from content bytes in the same quoted-hex format that OSS
/// uses. We take the first 16 bytes of SHA-256 (128-bit) and wrap in `"..."`:
///   `"\"<32-hex-chars>\""`
/// This matches OSS's quoted MD5 shape closely enough for comparison tests.
fn synthesise_etag(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    format!("\"{}\"", hex::encode(&digest[..16]))
}

#[async_trait]
impl StorageAdapter for InMemoryStorage {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.insert(key.to_string(), (bytes, chrono::Utc::now()));
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let g = self.inner.lock().unwrap();
        g.get(key)
            .map(|(b, _)| b.clone())
            .ok_or_else(|| SessyncError::Storage(format!("not found: {key}")))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<StorageObject>> {
        let g = self.inner.lock().unwrap();
        let mut out: Vec<_> = g
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, (b, t))| StorageObject {
                key: k.clone(),
                size: b.len() as u64,
                last_modified: *t,
                etag: Some(synthesise_etag(b)),
            })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.remove(key);
        Ok(())
    }

    async fn head(&self, key: &str) -> Result<StorageObject> {
        let g = self.inner.lock().unwrap();
        g.get(key)
            .map(|(b, t)| StorageObject {
                key: key.to_string(),
                size: b.len() as u64,
                last_modified: *t,
                etag: Some(synthesise_etag(b)),
            })
            .ok_or_else(|| SessyncError::Storage(format!("not found: {key}")))
    }
}
