//! Local filesystem storage adapter.
//!
//! For development/smoke-test loops where you want to exercise the full
//! push → encrypt → "upload" → list → "download" → decrypt path without
//! touching a real cloud bucket. Behaves like a directory-backed S3:
//! object keys map directly to relative file paths under `root`.
//!
//! Not designed for cross-device use — point two machines at the same root
//! via Syncthing/NFS/etc. only if you accept that this adapter does no
//! conflict resolution.

use super::storage::{StorageAdapter, StorageObject};
use crate::error::{Result, SessyncError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

pub struct LocalFsStorage {
    root: PathBuf,
}

impl LocalFsStorage {
    pub fn new<P: Into<PathBuf>>(root: P) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| {
            SessyncError::Storage(format!("create local-fs root {}: {e}", root.display()))
        })?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn key_to_path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

#[async_trait]
impl StorageAdapter for LocalFsStorage {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let path = self.key_to_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Atomic write: tmp + rename so a crash mid-write doesn't leave a half-file.
        let tmp = path.with_extension(format!(
            "{}.tmp",
            path.extension().and_then(|s| s.to_str()).unwrap_or("part")
        ));
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.key_to_path(key);
        tokio::fs::read(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SessyncError::Storage(format!("not found: {key}"))
            } else {
                SessyncError::Storage(format!("read {key}: {e}"))
            }
        })
    }

    async fn list(&self, prefix: &str) -> Result<Vec<StorageObject>> {
        let mut out = vec![];
        if !self.root.exists() {
            return Ok(out);
        }
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let mut entries = match tokio::fs::read_dir(&dir).await {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let key = path
                    .strip_prefix(&self.root)
                    .map_err(|e| SessyncError::Storage(format!("strip root: {e}")))?
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                if !key.starts_with(prefix) {
                    continue;
                }
                if key.ends_with(".tmp") {
                    continue; // skip in-flight writes
                }
                let metadata = entry.metadata().await?;
                let last_modified: DateTime<Utc> = metadata
                    .modified()
                    .map(DateTime::<Utc>::from)
                    .unwrap_or_else(|_| Utc::now());
                out.push(StorageObject {
                    key,
                    size: metadata.len(),
                    last_modified,
                });
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.key_to_path(key);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SessyncError::Storage(format!("delete {key}: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn put_get_roundtrip() {
        let dir = tempdir().unwrap();
        let s = LocalFsStorage::new(dir.path()).unwrap();
        s.put("a/b/c.bin", b"hello".to_vec()).await.unwrap();
        let got = s.get("a/b/c.bin").await.unwrap();
        assert_eq!(got, b"hello");
    }

    #[tokio::test]
    async fn list_filters_by_prefix() {
        let dir = tempdir().unwrap();
        let s = LocalFsStorage::new(dir.path()).unwrap();
        s.put("a/1.bin", vec![1]).await.unwrap();
        s.put("a/2.bin", vec![2]).await.unwrap();
        s.put("b/1.bin", vec![3]).await.unwrap();
        let listed = s.list("a/").await.unwrap();
        let keys: Vec<_> = listed.into_iter().map(|o| o.key).collect();
        assert_eq!(keys, vec!["a/1.bin".to_string(), "a/2.bin".to_string()]);
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let dir = tempdir().unwrap();
        let s = LocalFsStorage::new(dir.path()).unwrap();
        s.delete("nope").await.unwrap();
        s.put("k", vec![1]).await.unwrap();
        s.delete("k").await.unwrap();
        assert!(s.get("k").await.is_err());
    }

    #[tokio::test]
    async fn list_skips_tmp_files() {
        let dir = tempdir().unwrap();
        let s = LocalFsStorage::new(dir.path()).unwrap();
        s.put("a.bin", vec![1]).await.unwrap();
        // Drop a stray .tmp manually to simulate a crashed write
        std::fs::write(dir.path().join("b.tmp"), vec![2]).unwrap();
        let keys: Vec<_> = s
            .list("")
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        assert_eq!(keys, vec!["a.bin".to_string()]);
    }
}
