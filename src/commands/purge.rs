//! `sessync purge` — delete remote sessions matching a source_cwd pattern.
//!
//! Purge is a surgical alternative to `uninstall --purge-remote`. It lists all
//! remote `.meta.json` objects, decrypts each, and deletes the matching pairs
//! (`*.age` + `*.age.meta.json`) based on a user-supplied substring pattern
//! compared against `source_cwd`.
//!
//! The user must type `delete` to confirm unless `--yes` is given.

use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::s3::S3Storage;
use crate::adapter::storage::StorageAdapter;
use crate::config::{Config, StorageKind};
use crate::crypto;
use crate::passphrase_store;
use crate::types::SessionMeta;
use anyhow::{Context, Result};
use dialoguer::Input;
use futures::stream::{self, StreamExt};

/// Maximum number of parallel delete calls.
const DELETE_CONCURRENCY: usize = 8;

pub async fn run(pattern: String, dry_run: bool, yes: bool) -> Result<()> {
    let cfg =
        Config::load(&Config::default_path()).context("load config (run `sessync init` first?)")?;
    let passphrase = passphrase_store::load_passphrase().context("load passphrase")?;
    let salt = crypto::decode_salt_hex(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;

    match cfg.storage_kind {
        StorageKind::Oss => {
            let oss = cfg
                .oss
                .as_ref()
                .context("storage_kind = oss but [oss] section missing")?;
            let storage = OssStorage::new(oss)?;
            purge_by_pattern(&storage, &key, &pattern, dry_run, yes).await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            purge_by_pattern(&storage, &key, &pattern, dry_run, yes).await
        }
        StorageKind::S3 => {
            let s3cfg = cfg
                .s3
                .as_ref()
                .context("storage_kind = s3 but [s3] section missing")?;
            let storage = S3Storage::new(s3cfg)?;
            purge_by_pattern(&storage, &key, &pattern, dry_run, yes).await
        }
    }
}

/// Core purge logic — generic over storage so tests can use InMemoryStorage.
pub async fn purge_by_pattern<S: StorageAdapter>(
    storage: &S,
    key: &[u8; 32],
    pattern: &str,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    // 1. List all remote .meta.json objects.
    let all_objects = storage.list("").await?;

    // v0.10.0: meta sidecar suffix changed from `.age.meta.json` to `.meta.json`.
    // Accept both so purge keeps working on pre-migration buckets and on
    // post-migration v0.10 buckets.
    let meta_objects: Vec<_> = all_objects
        .iter()
        .filter(|o| o.key.ends_with(".meta.json"))
        .collect();

    if meta_objects.is_empty() {
        println!("No remote sessions found.");
        return Ok(());
    }

    // 2. Decrypt each .meta.json, collect keys for matching sessions.
    let mut matched_pairs: Vec<(String, String)> = Vec::new(); // (age_key, meta_key)
    let mut fetch_errors: Vec<String> = Vec::new();

    for obj in &meta_objects {
        let meta_key = &obj.key;
        let meta: SessionMeta = match storage.get(meta_key).await {
            Ok(ct) => match crypto::decrypt(&ct, key) {
                Ok(pt) => match serde_json::from_slice::<SessionMeta>(&pt) {
                    Ok(m) => m,
                    Err(e) => {
                        fetch_errors.push(format!("parse {meta_key}: {e}"));
                        continue;
                    }
                },
                Err(e) => {
                    fetch_errors.push(format!("decrypt {meta_key}: {e}"));
                    continue;
                }
            },
            Err(e) => {
                fetch_errors.push(format!("fetch {meta_key}: {e}"));
                continue;
            }
        };

        if meta.source_cwd.contains(pattern) {
            // v0.10.0: meta key is `<tool>/<sid>.meta.json`, base is
            // `<tool>/<sid>.age` — strip `.meta.json`, append `.age`.
            // Pre-v0.10.0: meta key was `<tool>/<pk>/<sid>.age.meta.json`,
            // base was the same string with `.meta.json` stripped — so just
            // stripping `.meta.json` from a `.age.meta.json` meta gives the
            // right base for old-layout objects too.
            let age_key = if meta_key.ends_with(".age.meta.json") {
                // Old layout
                meta_key
                    .strip_suffix(".meta.json")
                    .unwrap_or(meta_key)
                    .to_string()
            } else {
                // New layout: <tool>/<sid>.meta.json -> <tool>/<sid>.age
                let stem = meta_key.strip_suffix(".meta.json").unwrap_or(meta_key);
                format!("{stem}.age")
            };
            matched_pairs.push((age_key, meta_key.clone()));
        }
    }

    if !fetch_errors.is_empty() {
        eprintln!("warning: {} object(s) could not be decoded:", fetch_errors.len());
        for e in &fetch_errors {
            eprintln!("  {e}");
        }
    }

    if matched_pairs.is_empty() {
        println!("No sessions matching {:?} found.", pattern);
        return Ok(());
    }

    // v0.9.8: also collect delta files for each matched session. Pre-v0.9.8
    // purge only deleted .age + .age.meta.json (the base + meta), leaving
    // .age.delta-NNNN-DEV.age objects behind. Those orphan deltas couldn't
    // be reconstructed (no base) and produced
    // "session has no base object — cannot reconstruct" errors on every pull.
    // For each matched session, find its delta objects by key prefix:
    //   {tool}/{project_key}/{session_id}.delta-...
    let mut delta_keys: Vec<String> = Vec::new();
    for (age_key, _meta_key) in &matched_pairs {
        let Some(stem) = age_key.strip_suffix(".age") else {
            continue;
        };
        let delta_prefix = format!("{stem}.delta-");
        for obj in &all_objects {
            if obj.key.starts_with(&delta_prefix) && obj.key.ends_with(".age") {
                delta_keys.push(obj.key.clone());
            }
        }
    }

    let session_count = matched_pairs.len();
    let object_count = session_count * 2 + delta_keys.len(); // base + meta + N deltas

    // 3. Dry-run: print what would be deleted and exit.
    if dry_run {
        println!("dry-run: would delete {session_count} session(s) ({object_count} objects) matching {:?}:", pattern);
        for (age_key, meta_key) in &matched_pairs {
            println!("  {age_key}");
            println!("  {meta_key}");
        }
        for k in &delta_keys {
            println!("  {k}");
        }
        return Ok(());
    }

    // 4. Real run: print summary and prompt for confirmation.
    println!(
        "About to delete {session_count} session(s) ({object_count} objects) matching {:?}. This is irreversible.",
        pattern
    );

    if !yes {
        let typed: String = Input::new()
            .with_prompt("Type 'delete' to confirm")
            .interact_text()
            .context("confirmation prompt")?;
        if typed.trim() != "delete" {
            println!("Confirmation did not match 'delete'. Aborted.");
            return Ok(());
        }
    }

    // 5. Delete in parallel (buffered 8), collect errors.
    // Order: base+meta first, then deltas. Deletion order doesn't matter for
    // correctness (we delete the whole set; OSS is eventually consistent),
    // but base+meta first gives cleaner partial-failure semantics — if only
    // deltas remain, future pulls will still fail with "no base object", but
    // at least the session is conceptually gone from listings (no meta).
    let all_keys: Vec<String> = matched_pairs
        .into_iter()
        .flat_map(|(age_key, meta_key)| [age_key, meta_key])
        .chain(delta_keys.into_iter())
        .collect();

    let total = all_keys.len();
    let mut deleted = 0usize;
    let mut delete_errors: Vec<String> = Vec::new();

    let results: Vec<(String, Result<()>)> = stream::iter(all_keys.into_iter().map(|key| {
        let k = key.clone();
        async move {
            let result = storage.delete(&k).await.with_context(|| format!("delete {k}"));
            (k, result)
        }
    }))
    .buffered(DELETE_CONCURRENCY)
    .collect()
    .await;

    for (k, result) in results {
        match result {
            Ok(()) => {
                deleted += 1;
                tracing::info!("purge: deleted {k}");
            }
            Err(e) => {
                delete_errors.push(format!("{e}"));
            }
        }
    }

    println!("deleted {deleted}/{total} objects");

    if !delete_errors.is_empty() {
        anyhow::bail!(
            "{} delete(s) failed:\n{}",
            delete_errors.len(),
            delete_errors.join("\n")
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::memory::InMemoryStorage;
    use crate::types::{ProjectKey, SessionId, SessionMeta};
    use chrono::Utc;

    fn test_key() -> [u8; 32] {
        [0xAB; 32]
    }

    fn make_meta(id: &str, source_cwd: &str) -> SessionMeta {
        SessionMeta {
            schema_version: 1,
            session_id: SessionId(id.to_string()),
            project_key: ProjectKey("proj1".to_string()),
            source_cwd: source_cwd.to_string(),
            source_hostname: "testhost".to_string(),
            modified_at: Utc::now(),
            byte_size: 42,
            preview: "hello".to_string(),
            has_user_message: true,
        }
    }

    /// Seed a session pair (.age + .age.meta.json) into storage.
    async fn seed_session(
        storage: &InMemoryStorage,
        tool_name: &str,
        meta: &SessionMeta,
        raw_content: &[u8],
        key: &[u8; 32],
    ) {
        let age_key = format!(
            "{}/{}/{}.age",
            tool_name, meta.project_key.0, meta.session_id.0
        );
        let meta_key = format!("{}.meta.json", age_key);

        let ct = crate::crypto::encrypt(raw_content, key).unwrap();
        storage.put(&age_key, ct).await.unwrap();

        let meta_json = serde_json::to_vec(meta).unwrap();
        let meta_ct = crate::crypto::encrypt(&meta_json, key).unwrap();
        storage.put(&meta_key, meta_ct).await.unwrap();
    }

    // ── Test 1: dry-run lists matching keys but does not delete ───────────────

    #[tokio::test]
    async fn purge_dry_run_does_not_delete() {
        let key = test_key();
        let storage = InMemoryStorage::new();

        let meta_match = make_meta("sess-match", "/home/user/.claude-mem/sessions/proj");
        let meta_keep = make_meta("sess-keep", "/home/user/my-project");

        seed_session(&storage, "mock", &meta_match, b"content-a", &key).await;
        seed_session(&storage, "mock", &meta_keep, b"content-b", &key).await;

        // Before purge: 4 objects (2 sessions × 2 files each).
        let before = storage.list("").await.unwrap().len();
        assert_eq!(before, 4);

        // Dry-run should not delete anything.
        purge_by_pattern(&storage, &key, "claude-mem", /*dry_run=*/ true, /*yes=*/ true)
            .await
            .unwrap();

        let after = storage.list("").await.unwrap().len();
        assert_eq!(after, 4, "dry-run must not delete any objects");
    }

    // ── Test 2: real run with --yes deletes matching session pair ─────────────

    #[tokio::test]
    async fn purge_deletes_matching_pair_with_yes_flag() {
        let key = test_key();
        let storage = InMemoryStorage::new();

        let meta_match = make_meta("sess-match", "/home/user/.claude-mem/sessions/proj");
        let meta_keep = make_meta("sess-keep", "/home/user/my-project");

        seed_session(&storage, "mock", &meta_match, b"content-a", &key).await;
        seed_session(&storage, "mock", &meta_keep, b"content-b", &key).await;

        // Real run with --yes — should delete the 2 matching objects.
        purge_by_pattern(&storage, &key, "claude-mem", /*dry_run=*/ false, /*yes=*/ true)
            .await
            .unwrap();

        let remaining: Vec<_> = storage.list("").await.unwrap();
        // 2 objects remain (sess-keep pair).
        assert_eq!(remaining.len(), 2, "only sess-keep pair should remain: {:?}", remaining.iter().map(|o| &o.key).collect::<Vec<_>>());
        // The remaining objects must NOT mention sess-match.
        assert!(
            remaining.iter().all(|o| !o.key.contains("sess-match")),
            "sess-match objects must be deleted"
        );
        // sess-keep must still be there.
        assert!(
            remaining.iter().any(|o| o.key.contains("sess-keep")),
            "sess-keep must be retained"
        );
    }

    // ── Test 3: no pattern match → prints message, deletes nothing ────────────

    #[tokio::test]
    async fn purge_no_match_does_nothing() {
        let key = test_key();
        let storage = InMemoryStorage::new();

        let meta = make_meta("sess-a", "/home/user/normal-project");
        seed_session(&storage, "mock", &meta, b"content", &key).await;

        purge_by_pattern(&storage, &key, "claude-mem", false, true)
            .await
            .unwrap();

        // Nothing deleted.
        assert_eq!(storage.list("").await.unwrap().len(), 2);
    }

    // ── Test 4: empty remote → "No remote sessions found." ───────────────────

    #[tokio::test]
    async fn purge_empty_storage_returns_ok() {
        let key = test_key();
        let storage = InMemoryStorage::new();

        purge_by_pattern(&storage, &key, "anything", false, true)
            .await
            .unwrap();

        assert!(storage.list("").await.unwrap().is_empty());
    }

    // ── Test 5: pattern is case-sensitive substring ───────────────────────────

    #[tokio::test]
    async fn purge_pattern_is_case_sensitive() {
        let key = test_key();
        let storage = InMemoryStorage::new();

        // source_cwd uses lowercase "claude-mem".
        let meta = make_meta("sess-a", "/home/user/.claude-mem/sessions");
        seed_session(&storage, "mock", &meta, b"content", &key).await;

        // Uppercase "CLAUDE-MEM" must NOT match.
        purge_by_pattern(&storage, &key, "CLAUDE-MEM", false, true)
            .await
            .unwrap();

        // Nothing deleted.
        assert_eq!(storage.list("").await.unwrap().len(), 2, "case-sensitive mismatch must not delete");
    }

    // ── Test 6 (v0.9.8): purge deletes delta files for matched sessions ───────

    /// Regression test for the "no base object" bug. Before v0.9.8, purge only
    /// removed `.age` + `.age.meta.json`, leaving `.age.delta-NNNN-DEV.age`
    /// objects on OSS as orphans. Subsequent pulls of those sessions failed
    /// with "session has no base object — cannot reconstruct".
    ///
    /// This test seeds a session that has a base + 2 deltas, runs purge on
    /// its source_cwd pattern, and verifies all 4 objects (base, meta, 2x
    /// delta) are deleted.
    #[tokio::test]
    async fn purge_deletes_delta_objects_for_matched_session() {
        let key = test_key();
        let storage = InMemoryStorage::new();

        let meta = make_meta("sess-with-deltas", "/home/user/.claude-mem/sessions/proj");
        seed_session(&storage, "mock", &meta, b"base-content", &key).await;

        // Add 2 delta objects in the same session prefix.
        let delta_0 = format!(
            "mock/{}/{}.delta-0001-aaaa1111.age",
            meta.project_key.0, meta.session_id.0
        );
        let delta_1 = format!(
            "mock/{}/{}.delta-0002-bbbb2222.age",
            meta.project_key.0, meta.session_id.0
        );
        let ct = crate::crypto::encrypt(b"delta-content-0", &key).unwrap();
        storage.put(&delta_0, ct).await.unwrap();
        let ct = crate::crypto::encrypt(b"delta-content-1", &key).unwrap();
        storage.put(&delta_1, ct).await.unwrap();

        // Sanity: 4 objects total (base + meta + 2x delta).
        assert_eq!(storage.list("").await.unwrap().len(), 4);

        // Purge by the session's source_cwd pattern.
        purge_by_pattern(&storage, &key, "claude-mem", false, true)
            .await
            .unwrap();

        // All 4 objects should be gone — no orphan deltas.
        let remaining = storage.list("").await.unwrap();
        assert!(
            remaining.is_empty(),
            "purge must delete base + meta + ALL deltas; orphans left: {:?}",
            remaining.iter().map(|o| &o.key).collect::<Vec<_>>()
        );
    }

    /// Edge case: delta files exist on OSS but their base/meta was already
    /// deleted (so the session's source_cwd pattern doesn't match anything).
    /// Purge should NOT try to delete those orphan deltas — it only deletes
    /// deltas for sessions that match the pattern via meta.source_cwd.
    /// (If you want to clean true orphan deltas, that's a different feature.)
    #[tokio::test]
    async fn purge_does_not_touch_orphan_deltas_when_no_meta_matches() {
        let key = test_key();
        let storage = InMemoryStorage::new();

        // Only an orphan delta — no base, no meta.
        let orphan_delta = "mock/orphan-pk/orphan-sess.delta-0001-zzzz9999.age";
        let ct = crate::crypto::encrypt(b"orphan-delta-content", &key).unwrap();
        storage.put(orphan_delta, ct).await.unwrap();

        // Pattern wouldn't match anyway (no meta to read).
        purge_by_pattern(&storage, &key, "anything", false, true)
            .await
            .unwrap();

        // Orphan still there. We don't aggressively clean unrelated objects.
        assert_eq!(storage.list("").await.unwrap().len(), 1);
    }
}
