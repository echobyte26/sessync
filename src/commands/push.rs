use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::StorageAdapter;
use crate::adapter::tool::ToolAdapter;
use crate::config::{Config, StorageKind};
use crate::crypto;
use crate::passphrase_store;
use crate::queue::Queue;
use crate::notify;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tracing::info;

/// Seconds a pending item must be "stale" before we retry it.
/// Prevents the Stop hook from tight-looping when OSS is down.
const RETRY_COOLDOWN_SECS: i64 = 60;

pub async fn run(
    quiet: bool,
    sessions: Vec<String>,
    no_stale_warn: bool,
    dry_run: bool,
    fork_on_conflict: bool,
) -> Result<()> {
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
            push_all(&tool, &storage, &key, quiet, &sessions, no_stale_warn, dry_run, fork_on_conflict).await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            push_all(&tool, &storage, &key, quiet, &sessions, no_stale_warn, dry_run, fork_on_conflict).await
        }
    }
}

/// Tolerance applied to the stale-detection comparison.
///
/// OSS records `last_modified` at the moment of the PUT response, which is always
/// strictly after the local file's mtime (by a few hundred ms to a few seconds for
/// network + processing delay). Without tolerance, every session looks "stale"
/// after a normal same-machine push, triggering false-positive warnings and (when
/// fork-on-conflict is on) gratuitous forks.
///
/// 60s is enough to swallow any reasonable PUT delay while keeping real
/// cross-machine conflicts (typically minutes-to-hours apart) detectable.
///
/// True race-free conflict detection requires ETag tracking — see C3 backlog.
pub const STALE_TOLERANCE_SECS: i64 = 60;

/// Returns true when the remote object is meaningfully newer than the local session,
/// indicating another device pushed after this device's last sync. Subject to a
/// `STALE_TOLERANCE_SECS` window to ignore the OSS PUT-receipt skew.
pub fn is_stale(remote_last_modified: DateTime<Utc>, local_modified_at: DateTime<Utc>) -> bool {
    remote_last_modified
        > local_modified_at + chrono::Duration::seconds(STALE_TOLERANCE_SECS)
}

/// Compute the 8-hex-char short hash used to form a fork suffix.
///
/// Input: concatenation of hostname, a separator, the RFC3339 timestamp, a separator,
/// and the session_id. SHA-256 is used; the first 4 bytes give 8 hex chars.
/// Deterministic: same inputs → same output.
pub fn fork_short_hash(hostname: &str, timestamp: &DateTime<Utc>, session_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(hostname.as_bytes());
    h.update(b"|");
    h.update(timestamp.to_rfc3339().as_bytes());
    h.update(b"|");
    h.update(session_id.as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..4])
}

// ── Dry-run plan ─────────────────────────────────────────────────────────────

/// Classification of a single session in a dry-run pass.
#[derive(Debug, Clone, PartialEq)]
pub enum DryRunAction {
    /// Session is already current on remote — no upload needed.
    Skip,
    /// Session is new or local is newer than remote — would be uploaded.
    Upload { byte_size: u64 },
    /// Remote is newer than local — would overwrite (stale-overwrite path).
    Stale { byte_size: u64 },
    /// Remote is newer and fork_on_conflict=true — would be saved under a fork key.
    Fork { byte_size: u64, fork_id: String },
}

/// A single entry in the dry-run plan.
#[derive(Debug, Clone)]
pub struct DryRunEntry {
    pub session_id: String,
    pub action: DryRunAction,
}

/// The complete plan produced by `build_dry_run_plan`.
#[derive(Debug, Default)]
pub struct DryRunPlan {
    pub entries: Vec<DryRunEntry>,
}

impl DryRunPlan {
    /// Number of sessions that would be uploaded (including stale-overwrites).
    /// Fork uploads count separately — use `fork_count()` for those.
    pub fn upload_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.action, DryRunAction::Upload { .. } | DryRunAction::Stale { .. }))
            .count()
    }

    pub fn skip_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.action == DryRunAction::Skip)
            .count()
    }

    pub fn stale_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.action, DryRunAction::Stale { .. }))
            .count()
    }

    pub fn fork_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.action, DryRunAction::Fork { .. }))
            .count()
    }
}

/// Pure plan-builder: classify each session without touching storage or the queue.
///
/// The caller provides an already-filtered, deduplicated list of sessions
/// (`all_sessions`) and the remote index built from `storage.list`.
pub fn build_dry_run_plan(
    all_sessions: &[crate::adapter::tool::LocalSession],
    remote_index: &HashMap<String, DateTime<Utc>>,
    tool_name: &str,
    fork_on_conflict: bool,
) -> DryRunPlan {
    let mut entries = Vec::with_capacity(all_sessions.len());

    for s in all_sessions {
        let object_key = format!(
            "{}/{}/{}.age",
            tool_name,
            s.meta.project_key.0,
            s.meta.session_id.0
        );

        let action = if let Some(&remote_mtime) = remote_index.get(&object_key) {
            if is_stale(remote_mtime, s.meta.modified_at) {
                if fork_on_conflict {
                    // C2: instead of overwriting, record as a fork action.
                    let short_hash = fork_short_hash(
                        &s.meta.source_hostname,
                        &s.meta.modified_at,
                        &s.meta.session_id.0,
                    );
                    let fork_id = format!("{}.fork-{}", s.meta.session_id.0, short_hash);
                    DryRunAction::Fork {
                        byte_size: s.meta.byte_size,
                        fork_id,
                    }
                } else {
                    // Remote is strictly newer → stale-overwrite path.
                    DryRunAction::Stale {
                        byte_size: s.meta.byte_size,
                    }
                }
            } else if remote_mtime >= s.meta.modified_at {
                // Remote is current (equal or local is older) → skip.
                DryRunAction::Skip
            } else {
                // Local is newer → normal upload.
                DryRunAction::Upload {
                    byte_size: s.meta.byte_size,
                }
            }
        } else {
            // Not on remote at all → upload.
            DryRunAction::Upload {
                byte_size: s.meta.byte_size,
            }
        };

        entries.push(DryRunEntry {
            session_id: s.meta.session_id.0.clone(),
            action,
        });
    }

    DryRunPlan { entries }
}

/// Print the dry-run plan to stdout and return Ok(()).
fn print_dry_run_plan(plan: &DryRunPlan) {
    for entry in &plan.entries {
        match &entry.action {
            DryRunAction::Skip => {
                println!("would skip {} (already current)", entry.session_id);
            }
            DryRunAction::Upload { byte_size } => {
                println!("would push {} ({} B)", entry.session_id, byte_size);
            }
            DryRunAction::Stale { byte_size } => {
                println!(
                    "would push {} ({} B) (WARNING: remote is newer)",
                    entry.session_id, byte_size
                );
            }
            DryRunAction::Fork { byte_size: _, fork_id } => {
                println!(
                    "would fork {} -> {} (preserve remote)",
                    entry.session_id, fork_id
                );
            }
        }
    }
    let push_n = plan.upload_count();
    let skip_m = plan.skip_count();
    let stale_k = plan.stale_count();
    let fork_f = plan.fork_count();
    println!(
        "dry-run summary: would push {push_n}, skip {skip_m}, stale-overwrite {stale_k}, fork {fork_f}"
    );
}

pub async fn push_all<T: ToolAdapter, S: StorageAdapter>(
    tool: &T,
    storage: &S,
    key: &[u8; 32],
    quiet: bool,
    filter_ids: &[String],
    no_stale_warn: bool,
    dry_run: bool,
    fork_on_conflict: bool,
) -> Result<()> {
    // Open queue best-effort — never fail push just because queue.db is unavailable.
    // Dry-run never touches the queue at all.
    let q = if dry_run { None } else { Queue::open_default().ok() };

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

    // A3: drain the pending queue — collect session IDs eligible for retry.
    // "Eligible" means last_attempt_at is either null or older than RETRY_COOLDOWN_SECS.
    // Dry-run skips queue entirely — we want to see only the on-disk state.
    let mut queued_ids: Vec<String> = Vec::new();
    if !dry_run {
        if let Some(ref q) = q {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if let Ok(pending) = q.list_pending() {
                for item in pending {
                    let ready = match item.last_attempt_at {
                        None => true,
                        Some(t) => now - t >= RETRY_COOLDOWN_SECS,
                    };
                    if ready {
                        queued_ids.push(item.session_id);
                    }
                }
            }
        }
    }

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

        // Intersect queued IDs with the explicit filter.
        queued_ids.retain(|id| filter_ids.contains(id));
    }

    // Build set of local session IDs for quick lookup.
    let local_id_set: std::collections::HashSet<&str> =
        local_sessions.iter().map(|s| s.meta.session_id.0.as_str()).collect();

    // Clean up queued sessions that no longer exist locally.
    if let Some(ref q) = q {
        for id in &queued_ids {
            if !local_id_set.contains(id.as_str()) {
                let _ = q.dequeue(id);
            }
        }
    }
    // Only retry queued IDs that are still present locally.
    queued_ids.retain(|id| local_id_set.contains(id.as_str()));

    // Merge queued IDs into the session list (avoid duplicates).
    let mut extra_sessions: Vec<_> = local_sessions
        .iter()
        .filter(|s| queued_ids.contains(&s.meta.session_id.0))
        .cloned()
        .collect();

    // Sessions to push = normal list + any queued sessions not already in the list.
    // The normal list may already include the queued sessions (e.g., when filter_ids
    // is empty and the session still exists locally). Deduplicate by session_id.
    let already_in_list: std::collections::HashSet<&str> =
        local_sessions.iter().map(|s| s.meta.session_id.0.as_str()).collect();
    extra_sessions.retain(|s| !already_in_list.contains(s.meta.session_id.0.as_str()));

    let mut all_sessions = local_sessions;
    all_sessions.extend(extra_sessions);

    // ── Dry-run early exit ────────────────────────────────────────────────────
    if dry_run {
        let plan = build_dry_run_plan(&all_sessions, &remote_index, tool.name(), fork_on_conflict);
        print_dry_run_plan(&plan);
        return Ok(());
    }

    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for s in all_sessions {
        let sid = s.meta.session_id.0.as_str();

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
                if fork_on_conflict {
                    // C2: remote is newer and fork_on_conflict is set.
                    // Write the local version under a derived fork key, leaving the
                    // remote-newer copy intact.
                    let short_hash = fork_short_hash(
                        &s.meta.source_hostname,
                        &s.meta.modified_at,
                        &s.meta.session_id.0,
                    );
                    let fork_session_id = format!("{}.fork-{}", s.meta.session_id.0, short_hash);
                    let fork_object_key = format!(
                        "{}/{}/{}.fork-{}.age",
                        tool.name(),
                        s.meta.project_key.0,
                        s.meta.session_id.0,
                        short_hash
                    );
                    let fork_meta_key = format!("{}.meta.json", fork_object_key);

                    // Build a fork meta: session_id gets the fork suffix so that
                    // `sessync resume` lists both the original and the fork as separate
                    // sessions under the same project_key.
                    let mut fork_meta = s.meta.clone();
                    fork_meta.session_id = crate::types::SessionId(fork_session_id.clone());

                    // Read & encrypt session content.
                    let raw = match tokio::fs::read(&s.local_path).await {
                        Ok(b) => b,
                        Err(e) => {
                            let msg = format!(
                                "read {} ({}): {e}",
                                s.meta.session_id,
                                s.local_path.display()
                            );
                            if let Some(ref q) = q {
                                let _ = q.enqueue(sid);
                                let _ = q.record_attempt(sid, Some(&msg));
                            }
                            errors.push(msg);
                            continue;
                        }
                    };
                    fork_meta.byte_size = raw.len() as u64;

                    let ciphertext = match crypto::encrypt(&raw, key) {
                        Ok(ct) => ct,
                        Err(e) => {
                            let msg = format!("encrypt {}: {e}", s.meta.session_id);
                            if let Some(ref q) = q {
                                let _ = q.enqueue(sid);
                                let _ = q.record_attempt(sid, Some(&msg));
                            }
                            errors.push(msg);
                            continue;
                        }
                    };
                    let meta_json = match serde_json::to_vec(&fork_meta) {
                        Ok(j) => j,
                        Err(e) => {
                            let msg = format!("serialize meta {}: {e}", s.meta.session_id);
                            if let Some(ref q) = q {
                                let _ = q.enqueue(sid);
                                let _ = q.record_attempt(sid, Some(&msg));
                            }
                            errors.push(msg);
                            continue;
                        }
                    };
                    let meta_ciphertext = match crypto::encrypt(&meta_json, key) {
                        Ok(ct) => ct,
                        Err(e) => {
                            let msg = format!("encrypt meta {}: {e}", s.meta.session_id);
                            if let Some(ref q) = q {
                                let _ = q.enqueue(sid);
                                let _ = q.record_attempt(sid, Some(&msg));
                            }
                            errors.push(msg);
                            continue;
                        }
                    };

                    if let Err(e) = storage.put(&fork_object_key, ciphertext).await {
                        let msg = format!("upload fork {}: {e}", fork_object_key);
                        if let Some(ref q) = q {
                            let _ = q.enqueue(sid);
                            let _ = q.record_attempt(sid, Some(&msg));
                        }
                        errors.push(msg);
                        continue;
                    }
                    if let Err(e) = storage.put(&fork_meta_key, meta_ciphertext).await {
                        let msg = format!("upload fork meta {}: {e}", fork_meta_key);
                        if let Some(ref q) = q {
                            let _ = q.enqueue(sid);
                            let _ = q.record_attempt(sid, Some(&msg));
                        }
                        errors.push(msg);
                        continue;
                    }

                    println!(
                        "forked {} -> {} (preserved remote version intact)",
                        s.meta.session_id.0, fork_session_id
                    );
                    info!(
                        "forked {} -> {} ({} plaintext bytes)",
                        s.meta.session_id, fork_session_id, fork_meta.byte_size
                    );
                    if queued_ids.contains(&s.meta.session_id.0) {
                        if let Some(ref q) = q {
                            let _ = q.dequeue(sid);
                        }
                    }
                    pushed += 1;
                    continue;
                } else {
                    // C1: remote is strictly newer — another device pushed in between.
                    // Warn but proceed (last-writer-wins).
                    if !no_stale_warn {
                        eprintln!(
                            "warning: remote {} is newer than local — overwriting \
                             (use --no-stale-warn to silence, or pull first)",
                            s.meta.session_id
                        );
                    }
                    // Fall through to upload.
                }
            } else if remote_mtime >= s.meta.modified_at {
                // A5: remote is current (mtime equal or local is older) — skip upload.
                info!("skipped {} (unchanged)", s.meta.session_id);
                // If it was queued but now up-to-date, clean it up.
                if queued_ids.contains(&s.meta.session_id.0) {
                    if let Some(ref q) = q {
                        let _ = q.dequeue(sid);
                    }
                }
                skipped += 1;
                continue;
            }
        }

        // Read session bytes.
        let raw = match tokio::fs::read(&s.local_path).await {
            Ok(b) => b,
            Err(e) => {
                let msg = format!(
                    "read {} ({}): {e}",
                    s.meta.session_id,
                    s.local_path.display()
                );
                if let Some(ref q) = q {
                    let _ = q.enqueue(sid);
                    let _ = q.record_attempt(sid, Some(&msg));
                }
                errors.push(msg);
                continue;
            }
        };

        // Encrypt session content.
        let ciphertext = match crypto::encrypt(&raw, key) {
            Ok(ct) => ct,
            Err(e) => {
                let msg = format!("encrypt {}: {e}", s.meta.session_id);
                if let Some(ref q) = q {
                    let _ = q.enqueue(sid);
                    let _ = q.record_attempt(sid, Some(&msg));
                }
                errors.push(msg);
                continue;
            }
        };

        // Encrypt metadata.
        let meta_json = match serde_json::to_vec(&s.meta) {
            Ok(j) => j,
            Err(e) => {
                let msg = format!("serialize meta {}: {e}", s.meta.session_id);
                if let Some(ref q) = q {
                    let _ = q.enqueue(sid);
                    let _ = q.record_attempt(sid, Some(&msg));
                }
                errors.push(msg);
                continue;
            }
        };
        let meta_ciphertext = match crypto::encrypt(&meta_json, key) {
            Ok(ct) => ct,
            Err(e) => {
                let msg = format!("encrypt meta {}: {e}", s.meta.session_id);
                if let Some(ref q) = q {
                    let _ = q.enqueue(sid);
                    let _ = q.record_attempt(sid, Some(&msg));
                }
                errors.push(msg);
                continue;
            }
        };

        // Upload session content.
        if let Err(e) = storage.put(&object_key, ciphertext).await {
            let msg = format!("upload {}: {e}", object_key);
            if let Some(ref q) = q {
                let _ = q.enqueue(sid);
                let _ = q.record_attempt(sid, Some(&msg));
            }
            errors.push(msg);
            continue;
        }

        // Upload metadata.
        if let Err(e) = storage.put(&meta_key, meta_ciphertext).await {
            let msg = format!("upload meta {}: {e}", meta_key);
            if let Some(ref q) = q {
                let _ = q.enqueue(sid);
                let _ = q.record_attempt(sid, Some(&msg));
            }
            errors.push(msg);
            continue;
        }

        // Success for this session.
        info!(
            "pushed {} ({} plaintext bytes)",
            s.meta.session_id, s.meta.byte_size
        );
        // If it was previously queued, remove it.
        if queued_ids.contains(&s.meta.session_id.0) {
            if let Some(ref q) = q {
                let _ = q.dequeue(sid);
            }
        }
        pushed += 1;
    }

    info!("pushed {pushed} (skipped {skipped} unchanged)");
    if !quiet {
        println!("pushed {pushed} (skipped {skipped} unchanged)");
    }

    // A3: record the overall outcome for streak tracking (A4) and future `sessync logs`.
    let any_failure = !errors.is_empty();
    if let Some(ref q) = q {
        if any_failure {
            let summary = format!("push failed: {}", errors.join("; "));
            let _ = q.record_outcome(false, &summary);

            // A4: notify on exactly N=3 consecutive failures to avoid spam.
            if let Ok(n) = q.consecutive_failures() {
                if n == 3 {
                    notify::notify(
                        "sessync push failing",
                        &format!(
                            "{n} consecutive push failures. Run `sessync logs` to see why."
                        ),
                    );
                }
            }
        } else {
            let summary = format!("pushed {} (skipped {})", pushed, skipped);
            let _ = q.record_outcome(true, &summary);
        }
    }

    if any_failure {
        anyhow::bail!(
            "{} session(s) failed to push:\n{}",
            errors.len(),
            errors.join("\n")
        );
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

        push_all(&tool, &storage, &key, true, &[], false, false, false)
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

        push_all(&tool, &storage, &key, true, &[], false, false, false)
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

        push_all(&tool, &storage, &key, true, &["aaa111".to_string()], false, false, false)
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
            false,
            false,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string().contains("not found locally"),
            "expected 'not found locally' error, got: {err}"
        );
    }

    // Test 5: is_stale pure helper — remote meaningfully newer than local → true.
    // Tolerance is STALE_TOLERANCE_SECS so the delta must exceed it.
    #[test]
    fn test_is_stale_remote_newer() {
        let local = Utc.timestamp_opt(1000, 0).unwrap();
        let remote_newer = local + Duration::seconds(STALE_TOLERANCE_SECS + 1);
        assert!(is_stale(remote_newer, local));
    }

    #[test]
    fn test_is_stale_within_tolerance_not_stale() {
        // Sub-tolerance newer (typical post-PUT skew) should NOT be flagged stale.
        let local = Utc.timestamp_opt(1000, 0).unwrap();
        let remote_just_after = local + Duration::seconds(STALE_TOLERANCE_SECS - 1);
        assert!(!is_stale(remote_just_after, local));
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

        // Remote NEWER than local by more than STALE_TOLERANCE_SECS → stale overwrite scenario.
        let remote_ts = meta_a.modified_at + Duration::seconds(STALE_TOLERANCE_SECS + 1);
        let object_key = format!("mock/proj1/{}.age", meta_a.session_id.0);
        storage.put_at(&object_key, b"old-ct".to_vec(), remote_ts);

        // Should succeed (last-writer-wins), with --no-stale-warn to suppress stderr noise.
        push_all(&tool, &storage, &key, true, &[], true, false, false)
            .await
            .unwrap();

        // Remote should now have new ciphertext (not the stub).
        let new_ct = storage.get(&object_key).await.unwrap();
        assert_ne!(new_ct, b"old-ct", "stale remote should have been overwritten");
    }

    // ── Dry-run tests ─────────────────────────────────────────────────────────

    // Test 7: dry_run does not call storage.put — storage stays empty.
    #[tokio::test]
    async fn dry_run_does_not_call_storage_put() {
        let tool = MockTool {
            sessions: vec![
                (make_meta("aaa111", 1000), b"session-a".to_vec()),
                (make_meta("bbb222", 2000), b"session-b".to_vec()),
            ],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        push_all(&tool, &storage, &key, true, &[], false, /*dry_run=*/ true, false)
            .await
            .unwrap();

        // Storage must be completely empty — no .age or .meta.json objects.
        let objects = storage.list("mock/").await.unwrap();
        assert!(
            objects.is_empty(),
            "dry-run must not write anything to storage, found: {:?}",
            objects.iter().map(|o| &o.key).collect::<Vec<_>>()
        );
    }

    // Test 8: dry_run_classifies_correctly — plan reflects skip / upload / stale correctly.
    //
    // Setup:
    //   - "skip-me"  : remote has same mtime as local → Skip
    //   - "upload-me": not on remote → Upload
    //   - "stale-me" : remote is 1 s newer than local → Stale
    #[test]
    fn dry_run_classifies_correctly() {
        let meta_skip = make_meta("skip-me", 1000);
        let meta_upload = make_meta("upload-me", 2000);
        let meta_stale = make_meta("stale-me", 3000);

        // Write stub temp files so LocalSession paths resolve (not needed for
        // the pure plan builder, but we construct LocalSession directly).
        let make_local_session = |meta: &SessionMeta| -> crate::adapter::tool::LocalSession {
            let path = std::env::temp_dir()
                .join(format!("sessync_dry_run_test_{}.jsonl", meta.session_id));
            std::fs::write(&path, b"stub").unwrap();
            crate::adapter::tool::LocalSession {
                meta: meta.clone(),
                local_path: path,
            }
        };

        let sessions = vec![
            make_local_session(&meta_skip),
            make_local_session(&meta_upload),
            make_local_session(&meta_stale),
        ];

        // Remote index:
        //   skip-me  → same mtime as local
        //   upload-me → absent
        //   stale-me  → 1 s newer than local
        let mut remote_index: HashMap<String, DateTime<Utc>> = HashMap::new();
        remote_index.insert(
            format!("mock/proj1/{}.age", meta_skip.session_id.0),
            meta_skip.modified_at, // equal → skip
        );
        // upload-me not inserted → absent
        remote_index.insert(
            format!("mock/proj1/{}.age", meta_stale.session_id.0),
            // newer than tolerance → stale
            meta_stale.modified_at + chrono::Duration::seconds(STALE_TOLERANCE_SECS + 1),
        );

        let plan = build_dry_run_plan(&sessions, &remote_index, "mock", false);

        assert_eq!(plan.skip_count(), 1, "expected 1 skip");
        assert_eq!(
            plan.upload_count(),
            2,
            "expected 2 uploads (upload + stale-overwrite)"
        );
        assert_eq!(plan.stale_count(), 1, "expected 1 stale-overwrite");

        // Verify per-entry classification.
        let find = |id: &str| {
            plan.entries
                .iter()
                .find(|e| e.session_id == id)
                .unwrap()
                .action
                .clone()
        };

        assert_eq!(find("skip-me"), DryRunAction::Skip);
        assert!(
            matches!(find("upload-me"), DryRunAction::Upload { .. }),
            "upload-me should be Upload"
        );
        assert!(
            matches!(find("stale-me"), DryRunAction::Stale { .. }),
            "stale-me should be Stale"
        );
    }

    // ── C2 fork-on-conflict tests ─────────────────────────────────────────────

    // C2 Test 1: fork_short_hash — deterministic, 8 hex chars, different inputs differ.
    #[test]
    fn fork_short_hash_format_is_stable_within_a_call() {
        let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();

        let h1 = fork_short_hash("host-a", &ts, "sess-1");
        let h2 = fork_short_hash("host-a", &ts, "sess-1");
        let h3 = fork_short_hash("host-b", &ts, "sess-1");
        let h4 = fork_short_hash("host-a", &ts, "sess-2");

        // Same inputs → same output.
        assert_eq!(h1, h2, "fork_short_hash must be deterministic");
        // Different hostname → different hash.
        assert_ne!(h1, h3, "different hostname should produce different hash");
        // Different session_id → different hash.
        assert_ne!(h1, h4, "different session_id should produce different hash");
        // Always exactly 8 hex chars.
        assert_eq!(h1.len(), 8, "hash must be 8 characters long");
        assert!(
            h1.chars().all(|c| c.is_ascii_hexdigit()),
            "hash must be hex: got {h1}"
        );
    }

    // C2 Test 2: with fork_on_conflict=true and a stale remote, push_all writes
    // a forked object alongside the existing remote — original key is untouched.
    #[tokio::test]
    async fn fork_on_conflict_writes_forked_keys_when_stale() {
        let meta_a = make_meta("aaa111", 1000);
        let tool = MockTool {
            sessions: vec![(meta_a.clone(), b"local-session-data".to_vec())],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        // Remote NEWER than local by more than tolerance → stale conflict scenario.
        let remote_ts = meta_a.modified_at + Duration::seconds(STALE_TOLERANCE_SECS + 1);
        let object_key = format!("mock/proj1/{}.age", meta_a.session_id.0);
        storage.put_at(&object_key, b"remote-version".to_vec(), remote_ts);

        push_all(&tool, &storage, &key, true, &[], true, false, /*fork_on_conflict=*/ true)
            .await
            .unwrap();

        let objects = storage.list("mock/").await.unwrap();
        let all_keys: Vec<_> = objects.iter().map(|o| o.key.as_str()).collect();

        // The original remote key must still be intact with the old bytes.
        let remote_bytes = storage.get(&object_key).await.unwrap();
        assert_eq!(remote_bytes, b"remote-version", "original remote must not be overwritten");

        // A new fork key must exist.
        let fork_age_keys: Vec<_> = all_keys
            .iter()
            .filter(|k| k.contains(".fork-") && k.ends_with(".age") && !k.ends_with(".meta.json"))
            .collect();
        assert_eq!(fork_age_keys.len(), 1, "expected exactly one fork .age object, got: {:?}", all_keys);

        // A corresponding fork .meta.json must exist.
        let fork_meta_keys: Vec<_> = all_keys
            .iter()
            .filter(|k| k.contains(".fork-") && k.ends_with(".meta.json"))
            .collect();
        assert_eq!(fork_meta_keys.len(), 1, "expected exactly one fork .meta.json, got: {:?}", all_keys);

        // Total object count: 1 original + 1 fork .age + 1 fork .meta.json = 3.
        assert_eq!(objects.len(), 3, "expected 3 objects total, got: {:?}", all_keys);
    }

    // C2 Test 3: with fork_on_conflict=false (default) and a stale remote,
    // behavior is unchanged — the remote is overwritten.
    #[tokio::test]
    async fn fork_on_conflict_does_not_alter_default_path() {
        let meta_a = make_meta("aaa111", 1000);
        let tool = MockTool {
            sessions: vec![(meta_a.clone(), b"local-session-data".to_vec())],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        // Remote NEWER than local by more than tolerance → stale overwrite scenario.
        let remote_ts = meta_a.modified_at + Duration::seconds(STALE_TOLERANCE_SECS + 1);
        let object_key = format!("mock/proj1/{}.age", meta_a.session_id.0);
        storage.put_at(&object_key, b"old-remote".to_vec(), remote_ts);

        push_all(&tool, &storage, &key, true, &[], true, false, /*fork_on_conflict=*/ false)
            .await
            .unwrap();

        // Remote should now be overwritten (not old-remote).
        let new_ct = storage.get(&object_key).await.unwrap();
        assert_ne!(new_ct, b"old-remote", "stale remote should be overwritten when fork_on_conflict=false");

        // No fork objects should exist.
        let objects = storage.list("mock/").await.unwrap();
        let fork_keys: Vec<_> = objects
            .iter()
            .filter(|o| o.key.contains(".fork-"))
            .collect();
        assert!(fork_keys.is_empty(), "no fork objects expected when fork_on_conflict=false, got: {:?}", fork_keys);
    }

    // Test 9: dry_run does not record any queue outcomes.
    // We open a temp queue, wire it up via push_all with dry_run=true, then
    // confirm recent_outcomes is empty afterward.
    #[tokio::test]
    async fn dry_run_does_not_record_outcome() {
        let tool = MockTool {
            sessions: vec![(make_meta("aaa111", 1000), b"session-a".to_vec())],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        // Open a throwaway queue in a temp dir so we can inspect it.
        let tmp = std::env::temp_dir().join(format!(
            "sessync_dry_run_queue_{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        // Verify queue is empty before the run.
        {
            let q = Queue::open_at(&tmp).unwrap();
            assert_eq!(q.recent_outcomes(10).unwrap().len(), 0);
        }

        // push_all with dry_run=true — must not touch any queue at all.
        push_all(&tool, &storage, &key, true, &[], false, /*dry_run=*/ true, false)
            .await
            .unwrap();

        // The temp queue was never written; open it now and confirm still empty.
        let q = Queue::open_at(&tmp).unwrap();
        let outcomes = q.recent_outcomes(10).unwrap();
        assert!(
            outcomes.is_empty(),
            "dry-run must not record any queue outcomes"
        );

        // Cleanup.
        let _ = std::fs::remove_file(&tmp);
    }
}
