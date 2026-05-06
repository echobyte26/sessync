use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::StorageAdapter;
use crate::adapter::tool::ToolAdapter;
use crate::config::{Config, StorageKind};
use crate::crypto;
use crate::passphrase_store;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use tracing::info;

pub async fn run(quiet: bool, sessions: Vec<String>, no_stale_warn: bool) -> Result<()> {
    let cfg =
        Config::load(&Config::default_path()).context("load config (run `sessync init` first?)")?;
    let passphrase = passphrase_store::load_passphrase().context("load passphrase")?;
    let salt = crypto::decode_salt_hex(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;

    let tool = ClaudeCodeAdapter::new();

    match cfg.storage_kind {
        StorageKind::Oss => {
            let oss = cfg
                .oss
                .as_ref()
                .context("storage_kind = oss but [oss] section missing")?;
            let storage = OssStorage::new(oss)?;
            push_all(&tool, &storage, &key, quiet, &sessions, no_stale_warn).await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            push_all(&tool, &storage, &key, quiet, &sessions, no_stale_warn).await
        }
    }
}

/// Returns true when the remote object is strictly newer than the local session,
/// meaning another device pushed after this device's last sync.
pub fn is_stale(remote_last_modified: DateTime<Utc>, local_modified_at: DateTime<Utc>) -> bool {
    remote_last_modified > local_modified_at
}

pub async fn push_all<T: ToolAdapter, S: StorageAdapter>(
    tool: &T,
    storage: &S,
    key: &[u8; 32],
    quiet: bool,
    filter_ids: &[String],
    no_stale_warn: bool,
) -> Result<()> {
    // A5: fetch the current remote index once — avoids a HEAD per object.
    let prefix = format!("{}/", tool.name());
    let remote_objects = storage.list(&prefix).await?;

    // Build index keyed by object_key for .age files only.
    // Cipher overhead means remote size != plaintext size; mtime alone is the freshness signal.
    let remote_index: HashMap<String, DateTime<Utc>> = remote_objects
        .iter()
        .filter(|o| !o.key.ends_with(".meta.json"))
        .map(|o| (o.key.clone(), o.last_modified))
        .collect();

    let mut local_sessions = tool.list_local_sessions().await?;
    info!("found {} local sessions", local_sessions.len());

    // A6: filter to requested IDs if any were specified.
    if !filter_ids.is_empty() {
        for id in filter_ids {
            let found = local_sessions
                .iter()
                .any(|s| s.meta.session_id.0 == *id);
            if !found {
                anyhow::bail!("session {id} not found locally");
            }
        }
        local_sessions.retain(|s| filter_ids.contains(&s.meta.session_id.0));
    }

    let mut pushed = 0usize;
    let mut skipped = 0usize;

    for s in local_sessions {
        // Object key layout: {tool}/{project_key}/{session_id}.age
        let object_key = format!(
            "{}/{}/{}.age",
            tool.name(),
            s.meta.project_key.0,
            s.meta.session_id.0
        );
        let meta_key = format!("{}.meta.json", object_key);

        // A5 + C1: check remote state before deciding whether to upload.
        if let Some(&remote_mtime) = remote_index.get(&object_key) {
            if is_stale(remote_mtime, s.meta.modified_at) {
                // C1: remote is strictly newer — another device pushed in between.
                // Warn but proceed (last-writer-wins; C2 will add conflict resolution).
                if !no_stale_warn {
                    eprintln!(
                        "warning: remote {} is newer than local — overwriting \
                         (use --no-stale-warn to silence, or pull first)",
                        s.meta.session_id
                    );
                }
                // Fall through to upload.
            } else if remote_mtime >= s.meta.modified_at {
                // A5: remote is current (mtime equal or local is older) — skip upload.
                info!("skipped {} (unchanged)", s.meta.session_id);
                skipped += 1;
                continue;
            }
        }

        let raw = tokio::fs::read(&s.local_path).await.map_err(|e| {
            anyhow::anyhow!(
                "read {} ({}): {e}",
                s.meta.session_id,
                s.local_path.display()
            )
        })?;

        let ciphertext = crypto::encrypt(&raw, key)
            .map_err(|e| anyhow::anyhow!("encrypt {}: {e}", s.meta.session_id))?;
        let meta_json = serde_json::to_vec(&s.meta)?;
        let meta_ciphertext = crypto::encrypt(&meta_json, key)
            .map_err(|e| anyhow::anyhow!("encrypt meta {}: {e}", s.meta.session_id))?;

        storage
            .put(&object_key, ciphertext)
            .await
            .map_err(|e| anyhow::anyhow!("upload {}: {e}", object_key))?;
        storage
            .put(&meta_key, meta_ciphertext)
            .await
            .map_err(|e| anyhow::anyhow!("upload meta {}: {e}", meta_key))?;

        info!(
            "pushed {} ({} plaintext bytes)",
            s.meta.session_id, s.meta.byte_size
        );
        pushed += 1;
    }

    info!("pushed {pushed} (skipped {skipped} unchanged)");
    if !quiet {
        println!("pushed {pushed} (skipped {skipped} unchanged)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::memory::InMemoryStorage;
    use crate::adapter::storage::StorageAdapter;
    use crate::adapter::tool::{LocalSession, ToolAdapter};
    use crate::error::Result as SessyncResult;
    use crate::types::{ProjectKey, SessionId, SessionMeta};
    use async_trait::async_trait;
    use chrono::{Duration, TimeZone, Utc};
    use std::path::PathBuf;

    // Minimal dummy key — real age encryption is too slow for unit tests.
    // We use identity-like bytes; crypto correctness is tested elsewhere.
    fn test_key() -> [u8; 32] {
        [0xAB; 32]
    }

    fn make_meta(id: &str, modified_secs: i64) -> SessionMeta {
        SessionMeta {
            schema_version: 1,
            session_id: SessionId(id.to_string()),
            project_key: ProjectKey("proj1".to_string()),
            source_cwd: "/tmp/proj".to_string(),
            source_hostname: "testhost".to_string(),
            modified_at: Utc.timestamp_opt(modified_secs, 0).unwrap(),
            byte_size: 42,
            preview: "hello".to_string(),
        }
    }

    struct MockTool {
        sessions: Vec<(SessionMeta, Vec<u8>)>,
    }

    #[async_trait]
    impl ToolAdapter for MockTool {
        fn name(&self) -> &'static str {
            "mock"
        }

        async fn list_local_sessions(&self) -> SessyncResult<Vec<LocalSession>> {
            // Write each session's bytes to a real temp file so push_all can read it.
            let mut out = vec![];
            for (meta, bytes) in &self.sessions {
                let path =
                    std::env::temp_dir().join(format!("sessync_test_{}.jsonl", meta.session_id));
                std::fs::write(&path, bytes).unwrap();
                out.push(LocalSession {
                    meta: meta.clone(),
                    local_path: path,
                });
            }
            Ok(out)
        }

        async fn read_session(&self, _id: &SessionId) -> SessyncResult<Vec<u8>> {
            unimplemented!()
        }

        async fn write_session(
            &self,
            _id: &SessionId,
            _cwd: &str,
            _raw: &[u8],
        ) -> SessyncResult<PathBuf> {
            unimplemented!()
        }

        fn project_key_for(&self, _cwd: &str) -> ProjectKey {
            ProjectKey("proj1".to_string())
        }
    }

    // Test 1: empty remote, 2 local sessions → both pushed, none skipped.
    #[tokio::test]
    async fn test_empty_remote_pushes_all() {
        let tool = MockTool {
            sessions: vec![
                (make_meta("aaa111", 1000), b"session-a".to_vec()),
                (make_meta("bbb222", 2000), b"session-b".to_vec()),
            ],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        push_all(&tool, &storage, &key, true, &[], false)
            .await
            .unwrap();

        // Both .age and .meta.json objects should exist for each session.
        let objects = storage.list("mock/").await.unwrap();
        let age_keys: Vec<_> = objects
            .iter()
            .filter(|o| !o.key.ends_with(".meta.json"))
            .collect();
        assert_eq!(age_keys.len(), 2, "expected 2 pushed .age objects");
    }

    // Test 2: 2 local sessions already current on remote → both skipped.
    #[tokio::test]
    async fn test_incremental_skips_current() {
        let meta_a = make_meta("aaa111", 1000);
        let meta_b = make_meta("bbb222", 2000);

        let tool = MockTool {
            sessions: vec![
                (meta_a.clone(), b"session-a".to_vec()),
                (meta_b.clone(), b"session-b".to_vec()),
            ],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        // Pre-populate remote with exact same mtime as local — remote is "current".
        let key_a = format!("mock/proj1/{}.age", meta_a.session_id.0);
        let key_b = format!("mock/proj1/{}.age", meta_b.session_id.0);
        storage.put_at(&key_a, b"fake-ct".to_vec(), meta_a.modified_at);
        storage.put_at(&key_b, b"fake-ct".to_vec(), meta_b.modified_at);

        push_all(&tool, &storage, &key, true, &[], false)
            .await
            .unwrap();

        // The remote bytes should still be the stub "fake-ct" (not replaced).
        let got_a = storage.get(&key_a).await.unwrap();
        assert_eq!(got_a, b"fake-ct", "should not have overwritten a");
        let got_b = storage.get(&key_b).await.unwrap();
        assert_eq!(got_b, b"fake-ct", "should not have overwritten b");
    }

    // Test 3: selective push by id — only the named session is pushed.
    #[tokio::test]
    async fn test_selective_push_by_id() {
        let tool = MockTool {
            sessions: vec![
                (make_meta("aaa111", 1000), b"session-a".to_vec()),
                (make_meta("bbb222", 2000), b"session-b".to_vec()),
            ],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        push_all(&tool, &storage, &key, true, &["aaa111".to_string()], false)
            .await
            .unwrap();

        let objects = storage.list("mock/").await.unwrap();
        let age_keys: Vec<_> = objects
            .iter()
            .filter(|o| !o.key.ends_with(".meta.json"))
            .collect();
        assert_eq!(age_keys.len(), 1, "only one session should be pushed");
        assert!(
            age_keys[0].key.contains("aaa111"),
            "pushed key should be aaa111"
        );
    }

    // Test 4: selective push with unknown id → error.
    #[tokio::test]
    async fn test_selective_push_unknown_id_errors() {
        let tool = MockTool {
            sessions: vec![(make_meta("aaa111", 1000), b"session-a".to_vec())],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        let err = push_all(
            &tool,
            &storage,
            &key,
            true,
            &["nonexistent".to_string()],
            false,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string().contains("not found locally"),
            "expected 'not found locally' error, got: {err}"
        );
    }

    // Test 5: is_stale pure helper — remote newer than local → true.
    #[test]
    fn test_is_stale_remote_newer() {
        let local = Utc.timestamp_opt(1000, 0).unwrap();
        let remote_newer = local + Duration::seconds(1);
        assert!(is_stale(remote_newer, local));
    }

    #[test]
    fn test_is_stale_remote_equal_not_stale() {
        let ts = Utc.timestamp_opt(1000, 0).unwrap();
        assert!(!is_stale(ts, ts));
    }

    #[test]
    fn test_is_stale_remote_older_not_stale() {
        let local = Utc.timestamp_opt(2000, 0).unwrap();
        let remote_older = Utc.timestamp_opt(1000, 0).unwrap();
        assert!(!is_stale(remote_older, local));
    }

    // Test 6 (C1 integration): stale remote still results in upload proceeding.
    #[tokio::test]
    async fn test_stale_remote_still_uploads() {
        let meta_a = make_meta("aaa111", 1000);
        let tool = MockTool {
            sessions: vec![(meta_a.clone(), b"session-a".to_vec())],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        // Remote is 1 second NEWER than local → stale overwrite scenario.
        let remote_ts = meta_a.modified_at + Duration::seconds(1);
        let object_key = format!("mock/proj1/{}.age", meta_a.session_id.0);
        storage.put_at(&object_key, b"old-ct".to_vec(), remote_ts);

        // Should succeed (last-writer-wins), with --no-stale-warn to suppress stderr noise.
        push_all(&tool, &storage, &key, true, &[], true)
            .await
            .unwrap();

        // Remote should now have new ciphertext (not the stub).
        let new_ct = storage.get(&object_key).await.unwrap();
        assert_ne!(new_ct, b"old-ct", "stale remote should have been overwritten");
    }
}
