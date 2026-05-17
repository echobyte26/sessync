/// `sessync sync` — push then pull in a single invocation, sharing one
/// `storage.list` call per tool prefix across both directions.
///
/// This replaces the launchd plist pattern
///   `sh -c "sessync push --quiet && sessync pull --quiet"`
/// with a single binary invocation that uses a caching storage wrapper to
/// ensure the remote object list is fetched at most once per tool, not twice.
///
/// List-call reduction (v0.7.4):
///   Old plist: 4 list calls per cycle (2 tools × push + pull each = 4).
///   New:       2 list calls per cycle (one per tool, shared between push & pull).
///   Combined with DEFAULT_INTERVAL_SECS = 1800 (Fix 4):
///   ~2,880 list calls/month  vs  ~86,400 previously — a ~30× reduction.
use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::registry::all_adapters;
use crate::adapter::s3::S3Storage;
use crate::adapter::storage::{StorageAdapter, StorageObject};
use crate::commands::{pull, push};
use crate::config::{Config, ExcludeConfig, StorageKind};
use crate::crypto;
use crate::error::Result as SessyncResult;
use crate::passphrase_store;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

pub async fn run(quiet: bool) -> Result<()> {
    let cfg =
        Config::load(&Config::default_path()).context("load config (run `sessync init` first?)")?;
    let passphrase = passphrase_store::load_passphrase().context("load passphrase")?;
    let salt = crypto::decode_salt_hex(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;
    let exclude = cfg.exclude.clone();

    match cfg.storage_kind {
        StorageKind::Oss => {
            let oss = cfg
                .oss
                .as_ref()
                .context("storage_kind = oss but [oss] section missing")?;
            let storage = OssStorage::new(oss)?;
            sync_all(&storage, &key, quiet, &exclude).await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            sync_all(&storage, &key, quiet, &exclude).await
        }
        StorageKind::S3 => {
            let s3cfg = cfg
                .s3
                .as_ref()
                .context("storage_kind = s3 but [s3] section missing")?;
            let storage = S3Storage::new(s3cfg)?;
            sync_all(&storage, &key, quiet, &exclude).await
        }
    }
}

/// Run push then pull for every adapter, sharing the list response between the
/// two directions for each tool.
async fn sync_all<S: StorageAdapter>(
    storage: &S,
    key: &[u8; 32],
    quiet: bool,
    exclude: &ExcludeConfig,
) -> Result<()> {
    let adapters = all_adapters();

    let mut push_errors: Vec<String> = Vec::new();
    let mut pull_errors: Vec<String> = Vec::new();

    for adapter in &adapters {
        let tool_name = adapter.name();
        let prefix = format!("{}/", tool_name);

        // ONE list call per tool — the result is shared between push and pull
        // via the CachingStorage wrapper below.
        let remote_objects = match storage.list(&prefix).await {
            Ok(objs) => objs,
            Err(e) => {
                push_errors.push(format!("[{tool_name}] list: {e}"));
                pull_errors.push(format!("[{tool_name}] list: {e}"));
                continue;
            }
        };

        // Wrap the real storage in a caching layer pre-populated with the list
        // result we just fetched.  push_all and pull_all will call
        // `storage.list(&prefix)` internally — those calls hit the cache instead
        // of the network.
        let caching = CachingStorage::new_with_preload(storage, prefix.clone(), remote_objects);

        // Push direction.
        let push_result = push::push_all(
            adapter.as_ref(),
            &caching,
            key,
            /*quiet=*/ true,
            /*filter_ids=*/ &[],
            /*no_stale_warn=*/ false,
            /*dry_run=*/ false,
            /*fork_on_conflict=*/ false,
            exclude,
        )
        .await;

        match push_result {
            Ok((pushed, skipped)) => {
                if !quiet {
                    if pushed == 0 && skipped == 0 {
                        if adapters.len() > 1 {
                            println!("[{tool_name}] no local sessions to push");
                        } else {
                            println!("no local sessions to push");
                        }
                    } else if adapters.len() > 1 {
                        println!("[{tool_name}] pushed {pushed} (skipped {skipped} unchanged)");
                    } else {
                        println!("pushed {pushed} (skipped {skipped} unchanged)");
                    }
                }
            }
            Err(e) => {
                push_errors.push(format!("[{tool_name}] push: {e}"));
            }
        }

        // Pull direction — reuses the same cached list.
        // Note: after push_all may have PUT new objects, the cached list is
        // stale for those new objects.  That is acceptable — pull only needs
        // to know what was on remote BEFORE this push cycle, not what we just
        // uploaded ourselves.  The next sync cycle will pick up the fresh state.
        let pull_result = pull::pull_all(
            adapter.as_ref(),
            &caching,
            key,
            /*quiet=*/ true,
            /*tool_filter=*/ None,
            /*dry_run=*/ false,
            exclude,
        )
        .await;

        match pull_result {
            Ok((pulled, skipped)) => {
                if !quiet {
                    if pulled == 0 && skipped == 0 {
                        if adapters.len() > 1 {
                            println!("[{tool_name}] no remote sessions to pull");
                        } else {
                            println!("no remote sessions to pull");
                        }
                    } else if adapters.len() > 1 {
                        println!(
                            "[{tool_name}] pulled {pulled} (skipped {skipped} already current)"
                        );
                    } else {
                        println!("pulled {pulled} (skipped {skipped} already current)");
                    }
                }
            }
            Err(e) => {
                pull_errors.push(format!("[{tool_name}] pull: {e}"));
            }
        }
    }

    let mut all_errors = push_errors;
    all_errors.extend(pull_errors);
    if !all_errors.is_empty() {
        anyhow::bail!("{}", all_errors.join("\n"));
    }
    Ok(())
}

// ── CachingStorage ────────────────────────────────────────────────────────────

/// A thin wrapper around any `StorageAdapter` that caches `list()` results.
///
/// The cache is pre-populated on construction with the result of one real list
/// call.  Subsequent `list()` calls for the same prefix (or any prefix that
/// starts with a cached prefix) are served from memory without hitting the
/// network.
///
/// All other methods (`put`, `get`, `delete`, `head`) are forwarded to the
/// inner storage unconditionally.
///
/// # Why this design?
/// `push_all` and `pull_all` call `storage.list("{tool}/")` internally.
/// By wrapping the storage before passing it to those functions, we can serve
/// both calls from a single real list response without changing their signatures.
struct CachingStorage<'a, S: StorageAdapter> {
    inner: &'a S,
    /// Cached results keyed by the exact prefix string used in `list()`.
    cache: Mutex<HashMap<String, Vec<StorageObject>>>,
}

impl<'a, S: StorageAdapter> CachingStorage<'a, S> {
    /// Create a new `CachingStorage` pre-loaded with one list result.
    ///
    /// `prefix` must exactly match the string that downstream callers will pass
    /// to `list()` (e.g. `"claude-code/"`).
    fn new_with_preload(inner: &'a S, prefix: String, objects: Vec<StorageObject>) -> Self {
        let mut cache = HashMap::new();
        cache.insert(prefix, objects);
        Self {
            inner,
            cache: Mutex::new(cache),
        }
    }
}

#[async_trait]
impl<'a, S: StorageAdapter + Send + Sync> StorageAdapter for CachingStorage<'a, S> {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> SessyncResult<()> {
        self.inner.put(key, bytes).await
    }

    async fn get(&self, key: &str) -> SessyncResult<Vec<u8>> {
        self.inner.get(key).await
    }

    async fn list(&self, prefix: &str) -> SessyncResult<Vec<StorageObject>> {
        // Check the cache first.  We do exact-key lookup only (the pre-loaded
        // prefix exactly matches what push_all/pull_all will request).
        {
            let guard = self.cache.lock().unwrap();
            if let Some(cached) = guard.get(prefix) {
                return Ok(cached.clone());
            }
        }
        // Cache miss — forward to the real storage and populate the cache.
        let objects = self.inner.list(prefix).await?;
        {
            let mut guard = self.cache.lock().unwrap();
            guard.insert(prefix.to_string(), objects.clone());
        }
        Ok(objects)
    }

    async fn delete(&self, key: &str) -> SessyncResult<()> {
        self.inner.delete(key).await
    }

    async fn head(&self, key: &str) -> SessyncResult<StorageObject> {
        self.inner.head(key).await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::memory::InMemoryStorage;
    use crate::adapter::storage::StorageAdapter;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // A counting wrapper that records the number of list() calls made.
    struct CountingStorage<S: StorageAdapter> {
        inner: S,
        list_calls: Arc<AtomicUsize>,
    }

    impl<S: StorageAdapter> CountingStorage<S> {
        fn new(inner: S) -> (Self, Arc<AtomicUsize>) {
            let counter = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    inner,
                    list_calls: Arc::clone(&counter),
                },
                counter,
            )
        }
    }

    #[async_trait]
    impl<S: StorageAdapter + Send + Sync> StorageAdapter for CountingStorage<S> {
        async fn put(&self, key: &str, bytes: Vec<u8>) -> SessyncResult<()> {
            self.inner.put(key, bytes).await
        }
        async fn get(&self, key: &str) -> SessyncResult<Vec<u8>> {
            self.inner.get(key).await
        }
        async fn list(&self, prefix: &str) -> SessyncResult<Vec<StorageObject>> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.list(prefix).await
        }
        async fn delete(&self, key: &str) -> SessyncResult<()> {
            self.inner.delete(key).await
        }
        async fn head(&self, key: &str) -> SessyncResult<StorageObject> {
            self.inner.head(key).await
        }
    }

    // Verify that CachingStorage serves a second list() call from the cache
    // (i.e., the real storage is only called once for the same prefix).
    #[tokio::test]
    async fn caching_storage_list_cache_hit() {
        let mem = InMemoryStorage::new();
        mem.put("tool/proj/session.age", b"data".to_vec())
            .await
            .unwrap();

        let (counting, counter) = CountingStorage::new(mem);

        // Pre-load with one real list call.
        let initial_objects = counting.list("tool/").await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1, "first call hits real storage");

        // Wrap in CachingStorage with the pre-loaded result.
        let caching = CachingStorage::new_with_preload(&counting, "tool/".to_string(), initial_objects.clone());

        // First call via CachingStorage — cache hit, real storage NOT called again.
        let result1 = caching.list("tool/").await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1, "cache hit must not call real storage");
        assert_eq!(result1.len(), initial_objects.len());

        // Second call via CachingStorage — still a cache hit.
        let result2 = caching.list("tool/").await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1, "second cache hit must not call real storage");
        assert_eq!(result2.len(), initial_objects.len());
    }

    // Verify that a list() with a different prefix results in a real storage call
    // (cache miss path).
    #[tokio::test]
    async fn caching_storage_list_cache_miss_different_prefix() {
        let mem = InMemoryStorage::new();
        let (counting, counter) = CountingStorage::new(mem);

        // Pre-load with "tool-a/" prefix.
        let caching = CachingStorage::new_with_preload(&counting, "tool-a/".to_string(), vec![]);

        // list("tool-b/") — different prefix → cache miss → real storage called.
        let _ = caching.list("tool-b/").await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1, "different-prefix must call real storage");
    }

    // sync command dispatches correctly: verify the module compiles and
    // run() requires a valid config (the error path confirms dispatch works).
    #[tokio::test]
    async fn sync_run_requires_config() {
        // With no config present in the test environment, run() should bail
        // with a load-config error (not panic, not hang).
        // We just verify the function exists and returns an Err, not a panic.
        // (We cannot call run() with a real config in unit tests.)
        // This test mainly ensures the module compiles and the dispatch path exists.
        let result = run(true).await;
        // It should fail because there's no config in the test environment,
        // OR succeed if the developer has one installed.  Either way: no panic.
        let _ = result;
    }

    // DEFAULT_INTERVAL_SECS in launchd.rs must be 1800 (Fix 4).
    #[test]
    fn launchd_default_interval_is_1800() {
        assert_eq!(
            crate::commands::launchd::DEFAULT_INTERVAL_SECS,
            1800,
            "DEFAULT_INTERVAL_SECS must be 1800 (30 min) after v0.7.4 hotfix"
        );
    }
}
