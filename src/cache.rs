//! Encrypted local cache of decrypted `SessionMeta` objects.
//!
//! Stores: `~/.cache/sessync/meta-cache.age` (chmod 0600).
//! Encrypted with the same `&[u8; 32]` key as session content — passphrase
//! change auto-invalidates the cache (decryption fails → cold start).
//!
//! The cache is purely an optimisation: any failure in load or save is logged
//! and silently ignored.  The caller always falls back to full-fetch.

use crate::types::SessionMeta;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const CACHE_SCHEMA_VERSION: u32 = 1;

// ── on-disk types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaCache {
    pub schema_version: u32,
    /// Tool name (e.g. "claude-code") — separate logical caches per tool so
    /// running sessync against multiple tools through the same backend is safe.
    pub tool: String,
    /// Map: object key (e.g. `"claude-code/<pk>/<sid>.age.meta.json"`) →
    /// the decrypted `SessionMeta` + the remote `(mtime, size)` it came from.
    pub entries: HashMap<String, CachedEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedEntry {
    pub meta: SessionMeta,
    pub remote_mtime: DateTime<Utc>,
    pub remote_size: u64,
}

// ── impl ──────────────────────────────────────────────────────────────────────

impl MetaCache {
    /// Create an empty cache for `tool`.
    pub fn empty(tool: &str) -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            tool: tool.to_string(),
            entries: HashMap::new(),
        }
    }

    /// Load + decrypt the cache file.
    ///
    /// Returns `Self::empty(tool)` on **any** of:
    /// - file missing
    /// - decryption failure (wrong key / corrupted)
    /// - deserialise failure
    /// - `schema_version` mismatch
    /// - `tool` mismatch
    ///
    /// Never returns stale data — caller rebuilds from scratch on empty.
    pub fn load_or_empty(path: &Path, key: &[u8; 32], tool: &str) -> Self {
        match Self::try_load(path, key, tool) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("meta-cache load skipped ({}): {e}", path.display());
                Self::empty(tool)
            }
        }
    }

    fn try_load(path: &Path, key: &[u8; 32], tool: &str) -> Result<Self> {
        let ciphertext = std::fs::read(path)?;
        let plaintext = crate::crypto::decrypt(&ciphertext, key)
            .map_err(|e| anyhow::anyhow!("decrypt: {e}"))?;
        let cache: MetaCache = serde_json::from_slice(&plaintext)?;

        if cache.schema_version != CACHE_SCHEMA_VERSION {
            anyhow::bail!(
                "schema_version mismatch: expected {CACHE_SCHEMA_VERSION}, got {}",
                cache.schema_version
            );
        }
        if cache.tool != tool {
            anyhow::bail!("tool mismatch: expected {tool:?}, got {:?}", cache.tool);
        }
        Ok(cache)
    }

    /// Encrypt and write the cache atomically (tmp + rename) with mode 0600.
    pub fn save(&self, path: &Path, key: &[u8; 32]) -> Result<()> {
        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let plaintext = serde_json::to_vec(self)?;
        let ciphertext = crate::crypto::encrypt(&plaintext, key)
            .map_err(|e| anyhow::anyhow!("encrypt: {e}"))?;

        // Write to a sibling tmp file, then rename for atomicity.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &ciphertext)?;

        // chmod 0600 before rename so the file is never world-readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }

        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Return `Some(&meta)` iff the cache entry for `object_key` exists **and**
    /// both `remote_mtime` and `remote_size` match exactly.
    ///
    /// Requires both fields (paranoia against clock skew / mtime weirdness).
    pub fn get_if_fresh(
        &self,
        object_key: &str,
        remote_mtime: DateTime<Utc>,
        remote_size: u64,
    ) -> Option<&SessionMeta> {
        let entry = self.entries.get(object_key)?;
        if entry.remote_mtime == remote_mtime && entry.remote_size == remote_size {
            Some(&entry.meta)
        } else {
            None
        }
    }

    /// Insert or replace one cache entry.
    pub fn insert(
        &mut self,
        object_key: String,
        meta: SessionMeta,
        remote_mtime: DateTime<Utc>,
        remote_size: u64,
    ) {
        self.entries.insert(
            object_key,
            CachedEntry {
                meta,
                remote_mtime,
                remote_size,
            },
        );
    }

    /// Drop entries whose `object_key` is not present in the OSS listing
    /// (tombstone: the object was deleted remotely).
    pub fn retain_only(&mut self, present_keys: &HashSet<&str>) {
        self.entries.retain(|k, _| present_keys.contains(k.as_str()));
    }
}

// ── path helper ───────────────────────────────────────────────────────────────

/// Default path: `~/.cache/sessync/meta-cache.age`.
/// Returns `Err` if `$HOME` is not set.
pub fn default_cache_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("$HOME not set — refusing to use cache"))?;
    Ok(PathBuf::from(home).join(".cache/sessync/meta-cache.age"))
}
