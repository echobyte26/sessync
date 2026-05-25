//! `sessync migrate-oss-layout` — one-shot OSS path migration for v0.10.0.
//!
//! v0.9.x and earlier stored sessions at:
//!   `<tool>/<project_key>/<session_id>.{age, delta-NNNN-DEV.age, age.meta.json}`
//!
//! v0.10.0 changed the layout to drop the `<project_key>/` segment:
//!   `<tool>/<session_id>.{age, delta-NNNN-DEV.age, meta.json}`
//!
//! Without project_key in the path, the same session_id pushed from any
//! cwd context lands at one OSS location.  Deltas from different devices
//! (already disambiguated by device-id in their filenames) coexist and
//! merge naturally at reconstruction time, fixing the cross-device
//! session-split that caused pull-dedup to silently hide peer content.
//!
//! ## What this command does
//!
//! 1. List every OSS object under the bucket.
//! 2. Partition objects by `<tool>/<session_id>` group (parsing the old
//!    `<tool>/<project_key>/<session_id>...` layout).
//! 3. For each group:
//!    - Pick the LATEST `.age` base (by mtime).  Move to `<tool>/<sid>.age`.
//!    - Pick the LATEST `.age.meta.json` (by mtime).  Move to `<tool>/<sid>.meta.json`.
//!    - For each delta key (filename suffix `.delta-NNNN-DEV.age`), copy
//!      to the new path.  Deltas with the same `(seq, device_id)` from
//!      multiple project_keys are deduped — they should be identical
//!      content from the same device's same logical sequence.
//! 4. After all renames succeed, delete the old objects.
//!
//! ## Idempotency
//!
//! Re-running the command after a successful migration is a no-op: the
//! old keys no longer exist, so there's nothing to migrate.  Re-running
//! after a partial failure picks up where it left off — each rename is
//! `copy(old, new)` + `delete(old)`, so a half-done state has the new
//! key already present (skip the copy) and the old key still around
//! (do the delete).
//!
//! ## Pre-requisites
//!
//! - All sessync clients (launchd + Stop hooks) must be stopped to
//!   prevent concurrent writes during migration.  The command prints
//!   warnings if it detects we're not in a clean state.
//! - After migration, old (≤v0.9.x) clients will fail to find sessions
//!   at the expected `<tool>/<pk>/<sid>...` paths.  Migration is a
//!   one-way door — plan accordingly.

use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::s3::S3Storage;
use crate::adapter::storage::StorageAdapter;
use crate::config::{Config, StorageKind};
use anyhow::{Context, Result};
use dialoguer::Input;
use futures::stream::{self, StreamExt};
use std::collections::HashMap;
use tracing::info;

const COPY_CONCURRENCY: usize = 8;
const DELETE_CONCURRENCY: usize = 8;

pub async fn run(dry_run: bool, yes: bool) -> Result<()> {
    let cfg =
        Config::load(&Config::default_path()).context("load config (run `sessync init` first?)")?;

    match cfg.storage_kind {
        StorageKind::Oss => {
            let oss = cfg
                .oss
                .as_ref()
                .context("storage_kind = oss but [oss] section missing")?;
            let storage = OssStorage::new(oss)?;
            migrate(&storage, dry_run, yes).await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            migrate(&storage, dry_run, yes).await
        }
        StorageKind::S3 => {
            let s3cfg = cfg
                .s3
                .as_ref()
                .context("storage_kind = s3 but [s3] section missing")?;
            let storage = S3Storage::new(s3cfg)?;
            migrate(&storage, dry_run, yes).await
        }
    }
}

/// Parsed components of an old-layout OSS key.
#[derive(Debug, Clone)]
enum OldKey {
    /// `<tool>/<pk>/<sid>.age` — base object.
    Base {
        tool: String,
        sid: String,
    },
    /// `<tool>/<pk>/<sid>.age.meta.json` — meta sidecar.
    Meta {
        tool: String,
        sid: String,
    },
    /// `<tool>/<pk>/<sid>.delta-NNNN-DEV.age` — delta.
    Delta {
        tool: String,
        sid: String,
        seq: u32,
        device: String,
    },
}

impl OldKey {
    /// Return the v0.11 (subfolder) key this object should move to.
    /// Routes through `delta::` helpers so a future layout change is one-place.
    fn new_key(&self) -> String {
        match self {
            OldKey::Base { tool, sid } => crate::delta::base_key(tool, sid),
            OldKey::Meta { tool, sid } => crate::delta::meta_key(tool, sid),
            OldKey::Delta {
                tool,
                sid,
                seq,
                device,
            } => crate::delta::delta_key(tool, sid, *seq, device),
        }
    }

    fn tool_and_sid(&self) -> (&str, &str) {
        match self {
            OldKey::Base { tool, sid }
            | OldKey::Meta { tool, sid }
            | OldKey::Delta { tool, sid, .. } => (tool, sid),
        }
    }
}

/// Parse a key in EITHER pre-v0.11 layout (v0.9 `<tool>/<pk>/<sid>...` OR
/// v0.10 `<tool>/<sid>...`).  Returns None for keys already in v0.11
/// (`<tool>/<sid>/<filename>`) or the salt file / unrelated objects.
fn parse_old_key(key: &str) -> Option<OldKey> {
    // Reject v0.11 keys: they have `/base.age`, `/meta.json`, or `/delta-...age`
    // as the final segment (always 3+ parts AND the last filename has no leading
    // "<sid>." pattern).  Easier: if last path segment is exactly `base.age` or
    // `meta.json` or starts with `delta-`, it's already v0.11.
    if let Some(filename) = key.rsplit('/').next() {
        if filename == "base.age" || filename == "meta.json" || filename.starts_with("delta-") {
            return None;
        }
    }

    let parts: Vec<&str> = key.splitn(3, '/').collect();
    let (tool, filename) = match parts.len() {
        // v0.9 layout: `<tool>/<pk>/<filename>`
        3 => (parts[0].to_string(), parts[2]),
        // v0.10 layout: `<tool>/<filename>`
        2 => (parts[0].to_string(), parts[1]),
        _ => return None,
    };

    // Meta: `<sid>.age.meta.json` (v0.9) OR `<sid>.meta.json` (v0.10)
    if let Some(sid) = filename.strip_suffix(".age.meta.json") {
        return Some(OldKey::Meta { tool, sid: sid.to_string() });
    }
    if let Some(sid) = filename.strip_suffix(".meta.json") {
        return Some(OldKey::Meta { tool, sid: sid.to_string() });
    }

    if filename.ends_with(".age") {
        let stripped = &filename[..filename.len() - ".age".len()];
        if let Some(dotdelta) = stripped.rfind(".delta-") {
            let sid = &stripped[..dotdelta];
            let suffix = &stripped[dotdelta + ".delta-".len()..];
            let mut suffix_parts = suffix.splitn(2, '-');
            if let (Some(seq_str), Some(device)) = (suffix_parts.next(), suffix_parts.next()) {
                if let Ok(seq) = seq_str.parse::<u32>() {
                    return Some(OldKey::Delta {
                        tool,
                        sid: sid.to_string(),
                        seq,
                        device: device.to_string(),
                    });
                }
            }
            return None;
        }
        // Base: `<sid>.age` (no .delta-)
        return Some(OldKey::Base { tool, sid: stripped.to_string() });
    }

    None
}

/// Per-session work plan.
#[derive(Debug)]
struct SessionMigrationPlan {
    /// (old_key, new_key, mtime) for the chosen base to migrate.
    base: Option<(String, String, chrono::DateTime<chrono::Utc>)>,
    /// (old_key, new_key, mtime) for the chosen meta to migrate.
    meta: Option<(String, String, chrono::DateTime<chrono::Utc>)>,
    /// All delta migrations: (old_key, new_key) — dedup by new_key (which
    /// includes seq + device, so same-device same-seq from different
    /// project_keys collapse).
    deltas: Vec<(String, String)>,
    /// Old keys that lost the contest (older bases/metas not chosen) —
    /// deleted at end without copying.  Also includes deltas with key
    /// conflicts (we kept one of the duplicates).
    discard: Vec<String>,
}

pub async fn migrate<S: StorageAdapter>(
    storage: &S,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    // 1. List everything.
    println!("Scanning OSS for objects to migrate...");
    let all_objects = storage.list("").await?;
    println!("Found {} total object(s).", all_objects.len());

    // 2. Parse and group by (tool, sid).  Store original key alongside the
    //    parsed metadata so we can issue copies and deletes using the exact
    //    OSS path the object lives at today.
    let mut groups: HashMap<
        (String, String),
        Vec<(OldKey, chrono::DateTime<chrono::Utc>, String)>,
    > = HashMap::new();
    let mut unparseable: Vec<String> = Vec::new();

    for obj in &all_objects {
        match parse_old_key(&obj.key) {
            Some(parsed) => {
                let (tool, sid) = parsed.tool_and_sid();
                groups
                    .entry((tool.to_string(), sid.to_string()))
                    .or_default()
                    .push((parsed, obj.last_modified, obj.key.clone()));
            }
            None => {
                unparseable.push(obj.key.clone());
            }
        }
    }

    if !unparseable.is_empty() {
        println!(
            "Note: {} object(s) don't match the old layout (likely already migrated, or unrelated):",
            unparseable.len()
        );
        for k in unparseable.iter().take(5) {
            println!("  {k}");
        }
        if unparseable.len() > 5 {
            println!("  ... and {} more", unparseable.len() - 5);
        }
    }

    if groups.is_empty() {
        println!("No old-layout objects to migrate. (Already migrated, or empty bucket.)");
        return Ok(());
    }

    // 3. Build per-session migration plans.
    let mut plans: Vec<((String, String), SessionMigrationPlan)> = Vec::new();
    for (key, mut items) in groups {
        // Pick latest base by mtime.
        let mut best_base: Option<(OldKey, chrono::DateTime<chrono::Utc>, String)> = None;
        let mut best_meta: Option<(OldKey, chrono::DateTime<chrono::Utc>, String)> = None;
        let mut deltas_seen: HashMap<String, (OldKey, chrono::DateTime<chrono::Utc>, String)> =
            HashMap::new();
        let mut discard: Vec<String> = Vec::new();

        items.sort_by_key(|(_, mtime, _)| std::cmp::Reverse(*mtime));
        for (parsed, mtime, original_key) in items {
            match &parsed {
                OldKey::Base { .. } => {
                    if best_base.is_none() {
                        best_base = Some((parsed.clone(), mtime, original_key));
                    } else {
                        discard.push(original_key);
                    }
                }
                OldKey::Meta { .. } => {
                    if best_meta.is_none() {
                        best_meta = Some((parsed.clone(), mtime, original_key));
                    } else {
                        discard.push(original_key);
                    }
                }
                OldKey::Delta { .. } => {
                    let new_k = parsed.new_key();
                    if deltas_seen.contains_key(&new_k) {
                        // Same (seq, device) already seen with newer mtime — drop this one.
                        discard.push(original_key);
                    } else {
                        deltas_seen.insert(new_k, (parsed.clone(), mtime, original_key));
                    }
                }
            }
        }

        let plan = SessionMigrationPlan {
            base: best_base.map(|(p, m, old)| (old, p.new_key(), m)),
            meta: best_meta.map(|(p, m, old)| (old, p.new_key(), m)),
            deltas: deltas_seen
                .into_iter()
                .map(|(_new_key, (p, _m, old))| (old, p.new_key()))
                .collect(),
            discard,
        };
        plans.push((key, plan));
    }

    // 4. Print summary.
    let total_sessions = plans.len();
    let total_base = plans.iter().filter(|(_, p)| p.base.is_some()).count();
    let total_meta = plans.iter().filter(|(_, p)| p.meta.is_some()).count();
    let total_delta: usize = plans.iter().map(|(_, p)| p.deltas.len()).sum();
    let total_discard: usize = plans.iter().map(|(_, p)| p.discard.len()).sum();

    println!();
    println!("Migration plan:");
    println!("  {total_sessions} session(s) across the bucket");
    println!("  {total_base} base object(s) to rename");
    println!("  {total_meta} meta object(s) to rename");
    println!("  {total_delta} delta object(s) to rename");
    if total_discard > 0 {
        println!("  {total_discard} object(s) to drop (older/superseded copies)");
    }

    if dry_run {
        println!();
        println!("dry-run: showing first 10 renames for verification:");
        let mut shown = 0;
        for (_, plan) in &plans {
            if let Some((old, new, _)) = &plan.base {
                println!("  base  {old}  ->  {new}");
                shown += 1;
                if shown >= 10 {
                    break;
                }
            }
        }
        for (_, plan) in &plans {
            if shown >= 10 {
                break;
            }
            for (old, new) in &plan.deltas {
                println!("  delta {old}  ->  {new}");
                shown += 1;
                if shown >= 10 {
                    break;
                }
            }
        }
        println!();
        println!("(dry-run) — no objects were changed. Re-run with --yes to perform the migration.");
        return Ok(());
    }

    // 5. Confirmation prompt.
    println!();
    println!("⚠️  This will rewrite OSS keys in place (copy + delete).");
    println!("    Stop all sessync clients first (sessync launchd uninstall on every Mac)");
    println!("    or you risk pushes mid-migration corrupting state.");
    println!();

    if !yes {
        let typed: String = Input::new()
            .with_prompt("Type 'migrate' to confirm")
            .interact_text()
            .context("confirmation prompt")?;
        if typed.trim() != "migrate" {
            println!("Confirmation did not match 'migrate'. Aborted.");
            return Ok(());
        }
    }

    // Save session_ids for post-migration queue reset (plans is consumed below).
    let migrated_sids: Vec<String> = plans.iter().map(|((_, sid), _)| sid.clone()).collect();

    // 6. Execute: collect all (old, new) copy pairs.
    let mut copy_ops: Vec<(String, String)> = Vec::new();
    let mut delete_ops: Vec<String> = Vec::new();
    for (_, plan) in plans {
        if let Some((old, new, _)) = plan.base {
            copy_ops.push((old.clone(), new));
            delete_ops.push(old);
        }
        if let Some((old, new, _)) = plan.meta {
            copy_ops.push((old.clone(), new));
            delete_ops.push(old);
        }
        for (old, new) in plan.deltas {
            copy_ops.push((old.clone(), new));
            delete_ops.push(old);
        }
        delete_ops.extend(plan.discard);
    }

    // 7. Copy phase.
    let total_copies = copy_ops.len();
    let mut copy_done = 0usize;
    let mut copy_errors: Vec<String> = Vec::new();

    let copy_results: Vec<(String, String, Result<(), String>)> = stream::iter(
        copy_ops.into_iter().map(|(old, new)| async move {
            // Skip if destination already exists (idempotent re-run).
            if storage.head(&new).await.is_ok() {
                return (old, new, Ok::<(), String>(()));
            }
            match storage.get(&old).await {
                Ok(data) => match storage.put(&new, data).await {
                    Ok(()) => (old, new, Ok(())),
                    Err(e) => (old, new, Err(format!("put {}: {}", "", e))),
                },
                Err(e) => (old, new, Err(format!("get: {e}"))),
            }
        }),
    )
    .buffered(COPY_CONCURRENCY)
    .collect()
    .await;

    for (old, new, result) in copy_results {
        match result {
            Ok(()) => {
                copy_done += 1;
                info!("migrate: copied {old} -> {new}");
            }
            Err(e) => {
                copy_errors.push(format!("{old} -> {new}: {e}"));
            }
        }
    }

    println!("Copied {copy_done}/{total_copies} object(s).");
    if !copy_errors.is_empty() {
        eprintln!("Copy errors ({}):", copy_errors.len());
        for e in &copy_errors {
            eprintln!("  {e}");
        }
        anyhow::bail!("aborting migration before delete phase due to copy errors above");
    }

    // 8. Delete phase.
    let total_deletes = delete_ops.len();
    let mut delete_done = 0usize;
    let mut delete_errors: Vec<String> = Vec::new();

    let delete_results: Vec<(String, Result<(), String>)> = stream::iter(
        delete_ops.into_iter().map(|key| async move {
            match storage.delete(&key).await {
                Ok(()) => (key, Ok(())),
                Err(e) => (key, Err(format!("{e}"))),
            }
        }),
    )
    .buffered(DELETE_CONCURRENCY)
    .collect()
    .await;

    for (key, result) in delete_results {
        match result {
            Ok(()) => {
                delete_done += 1;
                info!("migrate: deleted old {key}");
            }
            Err(e) => {
                delete_errors.push(format!("{key}: {e}"));
            }
        }
    }

    println!("Deleted {delete_done}/{total_deletes} old object(s).");
    if !delete_errors.is_empty() {
        eprintln!("Delete errors ({}):", delete_errors.len());
        for e in &delete_errors {
            eprintln!("  {e}");
        }
        anyhow::bail!(
            "migration partially complete: copies succeeded but {} delete(s) failed",
            delete_errors.len()
        );
    }

    // v0.11.0: clear queue.etag and queue.last_pushed_state for migrated
    // sessions.  Without this, the next push from each device sees a stale
    // etag (recorded for the OLD OSS path/object that no longer exists)
    // and falls into the stale-warn-and-overwrite branch — pushing every
    // session every cycle.  Fresh queue lets the FIRST push after migration
    // upload the session as a new base on the new layout, and from then on
    // the per-device etags match OSS and steady-state push is just the
    // current session.
    let mut cleared = 0usize;
    if let Ok(q) = crate::queue::Queue::open_default() {
        for sid in &migrated_sids {
            let _ = q.delete_etag(sid);
            let _ = q.delete_session_state(sid);
            cleared += 1;
        }
    }
    if cleared > 0 {
        println!(
            "Reset queue state for {cleared} migrated session(s) (etag + last_pushed_state)."
        );
    }

    println!();
    println!("✅ Migration complete. OSS layout is now v0.10 (single path per session_id).");
    println!("    You can now run `sessync launchd install` on each Mac to resume auto-sync.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::memory::InMemoryStorage;

    #[test]
    fn parse_old_key_handles_base() {
        let p = parse_old_key("claude-code/abc123/sess-001.age").unwrap();
        match p {
            OldKey::Base { tool, sid } => {
                assert_eq!(tool, "claude-code");
                assert_eq!(sid, "sess-001");
            }
            _ => panic!("expected Base, got {p:?}"),
        }
    }

    #[test]
    fn parse_old_key_handles_meta() {
        let p = parse_old_key("claude-code/abc123/sess-001.age.meta.json").unwrap();
        match p {
            OldKey::Meta { tool, sid } => {
                assert_eq!(tool, "claude-code");
                assert_eq!(sid, "sess-001");
            }
            _ => panic!("expected Meta, got {p:?}"),
        }
    }

    #[test]
    fn parse_old_key_handles_delta() {
        let p = parse_old_key("claude-code/abc123/sess-001.delta-0005-mini1234.age").unwrap();
        match p {
            OldKey::Delta {
                tool,
                sid,
                seq,
                device,
            } => {
                assert_eq!(tool, "claude-code");
                assert_eq!(sid, "sess-001");
                assert_eq!(seq, 5);
                assert_eq!(device, "mini1234");
            }
            _ => panic!("expected Delta, got {p:?}"),
        }
    }

    #[test]
    fn parse_old_key_rejects_v011_layout() {
        // v0.11 subdirectory layout: not an old key.
        assert!(parse_old_key("claude-code/sess-001/base.age").is_none());
        assert!(parse_old_key("claude-code/sess-001/meta.json").is_none());
        assert!(parse_old_key("claude-code/sess-001/delta-0001-dev.age").is_none());
    }

    #[test]
    fn parse_old_key_accepts_v010_flat() {
        // v0.10 flat layout: 2 segments (no project_key).
        let p = parse_old_key("claude-code/sess-001.age").unwrap();
        match p {
            OldKey::Base { tool, sid } => {
                assert_eq!(tool, "claude-code");
                assert_eq!(sid, "sess-001");
            }
            _ => panic!("expected Base, got {p:?}"),
        }
    }

    #[test]
    fn new_key_for_base_uses_v011_subfolder() {
        let p = parse_old_key("claude-code/abc123/sess-001.age").unwrap();
        assert_eq!(p.new_key(), "claude-code/sess-001/base.age");
    }

    #[test]
    fn new_key_for_meta_uses_v011_subfolder() {
        let p = parse_old_key("claude-code/abc123/sess-001.age.meta.json").unwrap();
        assert_eq!(p.new_key(), "claude-code/sess-001/meta.json");
    }

    #[test]
    fn new_key_for_delta_uses_v011_subfolder() {
        let p = parse_old_key("claude-code/abc123/sess-001.delta-0005-mini1234.age").unwrap();
        assert_eq!(p.new_key(), "claude-code/sess-001/delta-0005-mini1234.age");
    }

    #[tokio::test]
    async fn migrate_empty_storage_is_noop() {
        let storage = InMemoryStorage::new();
        migrate(&storage, false, true).await.unwrap();
    }

    /// End-to-end: seed an OSS bucket with v0.9.x layout (including the
    /// pathological cross-device duplicate case — same session_id at two
    /// project_keys), run migrate, verify v0.10 layout afterward.
    #[tokio::test]
    async fn migrate_cross_device_duplicate_consolidates_to_single_path() {
        let storage = InMemoryStorage::new();

        // Same UUID at two different project_keys, with deltas from each device.
        // pk1 (jameschen path): base + 2 deltas from "mini" device
        storage
            .put(
                "claude-code/9443abc/sess-cross.age",
                b"base-from-mini".to_vec(),
            )
            .await
            .unwrap();
        storage
            .put(
                "claude-code/9443abc/sess-cross.age.meta.json",
                b"meta-mini".to_vec(),
            )
            .await
            .unwrap();
        storage
            .put(
                "claude-code/9443abc/sess-cross.delta-0001-mini1234.age",
                b"mini-d1".to_vec(),
            )
            .await
            .unwrap();
        storage
            .put(
                "claude-code/9443abc/sess-cross.delta-0002-mini1234.age",
                b"mini-d2".to_vec(),
            )
            .await
            .unwrap();
        // pk2 (sakuragi path): base + 1 delta from "pro" device
        storage
            .put(
                "claude-code/9a6cdef/sess-cross.age",
                b"base-from-pro".to_vec(),
            )
            .await
            .unwrap();
        storage
            .put(
                "claude-code/9a6cdef/sess-cross.age.meta.json",
                b"meta-pro".to_vec(),
            )
            .await
            .unwrap();
        storage
            .put(
                "claude-code/9a6cdef/sess-cross.delta-0001-pro56789.age",
                b"pro-d1".to_vec(),
            )
            .await
            .unwrap();

        // Total before migration: 7 objects (2 bases + 2 metas + 3 deltas).
        let before = storage.list("").await.unwrap();
        assert_eq!(before.len(), 7);

        // Run migration.
        migrate(&storage, false, true).await.unwrap();

        // After migration, expect:
        //   - 1 base at new path (whichever had latest mtime; in InMemoryStorage
        //     both have wallclock now, so it's nondeterministic but at least 1
        //     base must exist)
        //   - 1 meta
        //   - 3 deltas (mini's 2 + pro's 1, distinguishable by device suffix)
        let after = storage.list("").await.unwrap();
        let after_keys: std::collections::HashSet<String> =
            after.iter().map(|o| o.key.clone()).collect();

        // v0.11 layout: subfolder per session.
        assert!(after_keys.contains("claude-code/sess-cross/base.age"));
        assert!(after_keys.contains("claude-code/sess-cross/meta.json"));
        assert!(after_keys.contains("claude-code/sess-cross/delta-0001-mini1234.age"));
        assert!(after_keys.contains("claude-code/sess-cross/delta-0002-mini1234.age"));
        assert!(after_keys.contains("claude-code/sess-cross/delta-0001-pro56789.age"));

        // Old layout: all gone.
        for k in &after_keys {
            assert!(
                !k.contains("9443abc") && !k.contains("9a6cdef"),
                "old project_key segment must be gone, found: {k}"
            );
        }

        // The freshly-renamed base from one of the project_keys won — the
        // other's content was dropped (with the older meta).  Re-running
        // migration is now a no-op.
        let total_after = after.len();
        migrate(&storage, false, true).await.unwrap();
        let after_second = storage.list("").await.unwrap().len();
        assert_eq!(
            after_second, total_after,
            "second migration must be idempotent"
        );
    }

    /// Idempotency on partial state: simulate a previous migration that
    /// successfully copied the new key but failed before deleting the old.
    /// Running migrate again should: detect new key exists (skip copy),
    /// proceed to delete the old.
    #[tokio::test]
    async fn migrate_resumes_from_partial_state() {
        let storage = InMemoryStorage::new();

        // Old v0.9 key still present (partial migration crashed before deleting it).
        storage
            .put("claude-code/abc123/half-migrated.age", b"content".to_vec())
            .await
            .unwrap();
        // New v0.11 key already there (the copy step succeeded last time).
        storage
            .put("claude-code/half-migrated/base.age", b"content".to_vec())
            .await
            .unwrap();

        migrate(&storage, false, true).await.unwrap();

        let after = storage.list("").await.unwrap();
        let after_keys: std::collections::HashSet<String> =
            after.iter().map(|o| o.key.clone()).collect();

        assert!(after_keys.contains("claude-code/half-migrated/base.age"));
        assert!(!after_keys.contains("claude-code/abc123/half-migrated.age"));
    }
}
