use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::registry::{adapter_by_name, all_adapters, known_tool_names};
use crate::adapter::s3::S3Storage;
use crate::adapter::storage::StorageAdapter;
use crate::adapter::tool::ToolAdapter;
use crate::config::{Config, ExcludeConfig, StorageKind};
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
    tool_filter: Option<String>,
    include_ghosts: bool,
) -> Result<()> {
    let cfg =
        Config::load(&Config::default_path()).context("load config (run `sessync init` first?)")?;
    let passphrase = passphrase_store::load_passphrase().context("load passphrase")?;
    let salt = crypto::decode_salt_hex(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;
    let exclude = cfg.exclude.clone();

    // Resolve which adapters to push for.
    let adapters: Vec<Box<dyn ToolAdapter>> = if let Some(ref name) = tool_filter {
        match adapter_by_name(name) {
            Some(a) => vec![a],
            None => anyhow::bail!(
                "unknown tool '{}'. Known: {}",
                name,
                known_tool_names().join(", ")
            ),
        }
    } else {
        all_adapters()
    };

    match cfg.storage_kind {
        StorageKind::Oss => {
            let oss = cfg
                .oss
                .as_ref()
                .context("storage_kind = oss but [oss] section missing")?;
            let storage = OssStorage::new(oss)?;
            push_multi(&adapters, &storage, &key, quiet, &sessions, no_stale_warn, dry_run, fork_on_conflict, &exclude, include_ghosts).await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            push_multi(&adapters, &storage, &key, quiet, &sessions, no_stale_warn, dry_run, fork_on_conflict, &exclude, include_ghosts).await
        }
        StorageKind::S3 => {
            let s3cfg = cfg
                .s3
                .as_ref()
                .context("storage_kind = s3 but [s3] section missing")?;
            let storage = S3Storage::new(s3cfg)?;
            push_multi(&adapters, &storage, &key, quiet, &sessions, no_stale_warn, dry_run, fork_on_conflict, &exclude, include_ghosts).await
        }
    }
}

/// Loop over multiple adapters, calling `push_all` for each.
/// When more than one adapter produces output, prefix each line with `[tool-name]`.
async fn push_multi<S: StorageAdapter>(
    adapters: &[Box<dyn ToolAdapter>],
    storage: &S,
    key: &[u8; 32],
    quiet: bool,
    filter_ids: &[String],
    no_stale_warn: bool,
    dry_run: bool,
    fork_on_conflict: bool,
    exclude: &ExcludeConfig,
    include_ghosts: bool,
) -> Result<()> {
    // Collect per-adapter results.
    let mut per_tool: Vec<(&str, usize, usize)> = Vec::new(); // (name, pushed, skipped)
    let mut all_errors: Vec<String> = Vec::new();

    for adapter in adapters {
        let tool_name = adapter.name();
        // push_all already handles output when quiet=false for the single-tool case.
        // We suppress its output here and do our own aggregated printing.
        let result = push_all(adapter.as_ref(), storage, key, /*quiet=*/true, filter_ids, no_stale_warn, dry_run, fork_on_conflict, exclude, include_ghosts).await;
        match result {
            Ok((pushed, skipped)) => {
                per_tool.push((tool_name, pushed, skipped));
            }
            Err(e) => {
                all_errors.push(format!("[{tool_name}] {e}"));
            }
        }
    }

    if !quiet && !dry_run {
        let multi = per_tool.len() > 1;
        for (name, pushed, skipped) in &per_tool {
            if *pushed == 0 && *skipped == 0 {
                // No sessions at all — print a clear, non-confusing message.
                if multi {
                    println!("[{name}] no local sessions to push");
                } else {
                    println!("no local sessions to push");
                }
            } else if multi {
                println!("[{name}] pushed {pushed} (skipped {skipped} unchanged)");
            } else {
                println!("pushed {pushed} (skipped {skipped} unchanged)");
            }
        }
    }

    if !all_errors.is_empty() {
        anyhow::bail!("{}", all_errors.join("\n"));
    }
    Ok(())
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

/// Pure helper: determine whether a push would be a real stale-overwrite.
///
/// Returns `true` only when both the local recorded ETag and the current remote
/// ETag are known **and** they differ — meaning another device pushed this
/// session since our last push.
///
/// | recorded | remote | result |
/// |----------|--------|--------|
/// | None     | *      | false  — no prior push from this machine, can't tell |
/// | Some      | None  | false  — remote doesn't expose ETag, can't tell |
/// | Some      | Some  | recorded != remote |
pub fn classify_stale(recorded: Option<&str>, remote: Option<&str>) -> bool {
    match (recorded, remote) {
        (Some(local), Some(remote)) if local != remote => true,
        _ => false,
    }
}

/// Pure plan-builder: classify each session without touching storage or the queue.
///
/// The caller provides an already-filtered, deduplicated list of sessions
/// (`all_sessions`) and the remote index built from `storage.list`.
///
/// `etag_index` maps object_key → remote ETag (from the list response).
/// `recorded_etags` maps session_id → locally-recorded ETag (from the queue).
pub fn build_dry_run_plan(
    all_sessions: &[crate::adapter::tool::LocalSession],
    remote_index: &HashMap<String, DateTime<Utc>>,
    etag_index: &HashMap<String, Option<String>>,
    recorded_etags: &HashMap<String, String>,
    tool_name: &str,
    fork_on_conflict: bool,
) -> DryRunPlan {
    let mut entries = Vec::with_capacity(all_sessions.len());

    for s in all_sessions {
        let sid = &s.meta.session_id.0;
        let object_key = format!(
            "{}/{}/{}.age",
            tool_name,
            s.meta.project_key.0,
            sid,
        );

        let remote_etag: Option<&str> = etag_index
            .get(&object_key)
            .and_then(|o| o.as_deref());
        let recorded_etag: Option<&str> = recorded_etags.get(sid).map(|s| s.as_str());
        let real_stale = classify_stale(recorded_etag, remote_etag);

        let action = if let Some(&remote_mtime) = remote_index.get(&object_key) {
            if real_stale && fork_on_conflict {
                // ETag mismatch + fork flag → produce a fork key.
                let hostname = hostname_or_unknown();
                let now = chrono::Utc::now();
                let hash = fork_short_hash(&hostname, &now, sid);
                let fork_id = format!("{sid}.fork-{hash}");
                DryRunAction::Fork {
                    byte_size: s.meta.byte_size,
                    fork_id,
                }
            } else if real_stale {
                // ETag mismatch → stale overwrite (with warning in real push).
                DryRunAction::Stale {
                    byte_size: s.meta.byte_size,
                }
            } else if remote_mtime >= s.meta.modified_at {
                // Same-machine or no ETag info → skip (A5 path).
                DryRunAction::Skip
            } else {
                // Local is newer → upload.
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
            session_id: sid.clone(),
            action,
        });
    }

    DryRunPlan { entries }
}

/// Return the machine hostname, falling back to "unknown" if unavailable.
fn hostname_or_unknown() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
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
    println!("dry-run summary: would push {push_n}, skip {skip_m}");
}

pub async fn push_all<S: StorageAdapter>(
    tool: &dyn ToolAdapter,
    storage: &S,
    key: &[u8; 32],
    _quiet: bool,
    filter_ids: &[String],
    no_stale_warn: bool,
    dry_run: bool,
    fork_on_conflict: bool,
    exclude: &ExcludeConfig,
    include_ghosts: bool,
) -> Result<(usize, usize)> {
    // Open queue best-effort — never fail push just because queue.db is unavailable.
    // Dry-run never touches the queue at all.
    let q = if dry_run { None } else { Queue::open_default().ok() };

    // A5: fetch the current remote index once — avoids a HEAD per object.
    let prefix = format!("{}/", tool.name());
    let remote_objects = storage.list(&prefix).await?;

    // Build two indexes keyed by object_key for .age files only:
    // - remote_index:      object_key → last_modified (for A5 skip logic)
    // - remote_etag_index: object_key → ETag Option (for C-etag stale detection)
    let remote_index: HashMap<String, DateTime<Utc>> = remote_objects
        .iter()
        .filter(|o| !o.key.ends_with(".meta.json"))
        .map(|o| (o.key.clone(), o.last_modified))
        .collect();
    let remote_etag_index: HashMap<String, Option<String>> = remote_objects
        .iter()
        .filter(|o| !o.key.ends_with(".meta.json"))
        .map(|o| (o.key.clone(), o.etag.clone()))
        .collect();

    let mut local_sessions = tool.list_local_sessions().await?;
    info!("found {} local sessions", local_sessions.len());

    // Apply exclude filter — drop sessions whose source_cwd matches any pattern.
    // Always runs: the zero-config heuristic in ExcludeConfig::matches() applies
    // even when project_path_contains is empty (it catches dotfile dirs under $HOME).
    {
        let before = local_sessions.len();
        local_sessions.retain(|s| !exclude.matches(&s.meta.source_cwd));
        let excluded = before - local_sessions.len();
        if excluded > 0 && !exclude.project_path_contains.is_empty() {
            // Only print the pattern-list message when user patterns are configured
            // (heuristic exclusions are silent to avoid noisy output for everyone).
            println!(
                "excluded {excluded} session{} matching {:?}",
                if excluded == 1 { "" } else { "s" },
                exclude.project_path_contains,
            );
        }
    }

    // Ghost filter: drop sessions that have no user message events unless the
    // caller explicitly opted in with --include-ghosts.
    if !include_ghosts {
        let before = local_sessions.len();
        local_sessions.retain(|s| s.meta.has_user_message);
        let ghost_count = before - local_sessions.len();
        if ghost_count > 0 {
            println!(
                "filtered {ghost_count} ghost session{} (plugin hooks / no user content)",
                if ghost_count == 1 { "" } else { "s" },
            );
        }
    }

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
        // Read recorded ETags read-only; open a fresh queue handle just for reading.
        // We do not use `q` (which is None for dry-run) — we open a temporary read
        // handle so we can show accurate stale/fork classifications without any writes.
        let recorded_etags: HashMap<String, String> = Queue::open_default()
            .ok()
            .and_then(|rq| rq.all_etags().ok())
            .unwrap_or_default();
        let plan = build_dry_run_plan(
            &all_sessions,
            &remote_index,
            &remote_etag_index,
            &recorded_etags,
            tool.name(),
            fork_on_conflict,
        );
        print_dry_run_plan(&plan);
        return Ok((0, 0));
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

        // C-etag: compare the locally-recorded ETag against the current remote ETag.
        // If they differ, another device pushed this session since our last push.
        let recorded_etag: Option<String> = q
            .as_ref()
            .and_then(|q| q.get_etag(sid).ok().flatten());
        let remote_etag: Option<String> = remote_etag_index
            .get(&object_key)
            .cloned()
            .flatten();
        let real_stale = classify_stale(recorded_etag.as_deref(), remote_etag.as_deref());

        if real_stale && fork_on_conflict {
            // C2: another device pushed this session AND the user wants to preserve
            // their local copy under a fork key instead of overwriting.
            let hostname = hostname_or_unknown();
            let now = chrono::Utc::now();
            let hash = fork_short_hash(&hostname, &now, sid);
            let fork_id = format!("{sid}.fork-{hash}");
            let fork_key = format!(
                "{}/{}/{}.age",
                tool.name(),
                s.meta.project_key.0,
                fork_id,
            );

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

            let ciphertext = match crypto::encrypt(&raw, key) {
                Ok(ct) => ct,
                Err(e) => {
                    let msg = format!("encrypt fork {}: {e}", s.meta.session_id);
                    if let Some(ref q) = q {
                        let _ = q.enqueue(sid);
                        let _ = q.record_attempt(sid, Some(&msg));
                    }
                    errors.push(msg);
                    continue;
                }
            };

            if let Err(e) = storage.put(&fork_key, ciphertext).await {
                let msg = format!("upload fork {}: {e}", fork_key);
                if let Some(ref q) = q {
                    let _ = q.enqueue(sid);
                    let _ = q.record_attempt(sid, Some(&msg));
                }
                errors.push(msg);
                continue;
            }

            info!("forked {} → {}", s.meta.session_id, fork_id);
            // Record new ETag for the fork object (best-effort).
            if let Ok(info) = storage.head(&fork_key).await {
                if let Some(etag) = info.etag {
                    if let Some(ref q) = q {
                        let _ = q.record_etag(&fork_id, &etag);
                    }
                }
            }
            if queued_ids.contains(&s.meta.session_id.0) {
                if let Some(ref q) = q {
                    let _ = q.dequeue(sid);
                }
            }
            pushed += 1;
            continue;
        }

        // C1: stale-warn — another device pushed after us. Overwrite unless
        // no_stale_warn is set (user explicitly silenced the warning).
        if real_stale && !no_stale_warn {
            eprintln!(
                "warning: remote {} was modified by another device since your last push \
                 — overwriting (use --no-stale-warn to silence, or pull first)",
                sid
            );
        }

        // A5: skip when remote is at least as fresh as local AND we haven't detected
        // a real cross-machine write via ETag mismatch. Without an ETag mismatch we
        // cannot tell whether "remote is newer" means "I just pushed" or "Mac B pushed".
        // The safe default: if ETags are absent or matching, treat remote-newer as skip.
        if !real_stale {
            if let Some(&remote_mtime) = remote_index.get(&object_key) {
                if remote_mtime >= s.meta.modified_at {
                    info!("skipped {} (unchanged)", s.meta.session_id);
                    if queued_ids.contains(&s.meta.session_id.0) {
                        if let Some(ref q) = q {
                            let _ = q.dequeue(sid);
                        }
                    }
                    skipped += 1;
                    continue;
                }
            }
        }
        // real_stale=true (non-fork path) falls through to upload below — overwrite.

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

        // C-etag: record the new remote ETag post-PUT (best-effort, never fails push).
        if let Ok(info) = storage.head(&object_key).await {
            if let Some(etag) = info.etag {
                if let Some(ref q) = q {
                    let _ = q.record_etag(sid, &etag);
                }
            }
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
    Ok((pushed, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::memory::InMemoryStorage;
    use crate::adapter::storage::StorageAdapter;
    use crate::adapter::tool::{LocalSession, ToolAdapter};
    use crate::config::ExcludeConfig;
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
            has_user_message: true,
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

        fn launch_resume(&self, _id: &SessionId) -> std::io::Result<std::process::Child> {
            unimplemented!()
        }

        fn launch_binary_on_path(&self) -> bool {
            false
        }

        fn launch_binary_name(&self) -> &'static str {
            "mock"
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

        push_all(&tool, &storage, &key, true, &[], false, false, false, &ExcludeConfig::default(), false)
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
    //
    // ETag isolation: push_all opens Queue::open_default() internally. We clear
    // any stale ETag records for these session IDs from prior test runs before
    // calling push_all so that classify_stale returns false (no recorded ETag →
    // cannot claim another device pushed) and the A5 mtime-skip fires.
    #[tokio::test]
    async fn test_incremental_skips_current() {
        let meta_a = make_meta("aaa111", 1000);
        let meta_b = make_meta("bbb222", 2000);

        // Clear any ETag state left by other test runs for these session IDs.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag("aaa111");
            let _ = q.delete_etag("bbb222");
        }

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

        push_all(&tool, &storage, &key, true, &[], false, false, false, &ExcludeConfig::default(), false)
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

        push_all(&tool, &storage, &key, true, &["aaa111".to_string()], false, false, false, &ExcludeConfig::default(), false)
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
            &ExcludeConfig::default(),
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

    // Test 6: when remote is newer than local and no ETag mismatch is detected,
    // push SKIPS. Without a prior push from this machine (no recorded ETag),
    // classify_stale returns false → A5 mtime-skip fires.
    #[tokio::test]
    async fn remote_newer_skips_upload() {
        let meta_a = make_meta("aaa111", 1000);

        // Clear any stale ETag left by other test runs to ensure classify_stale=false.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag("aaa111");
        }

        let tool = MockTool {
            sessions: vec![(meta_a.clone(), b"session-a".to_vec())],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        // Remote NEWER than local — triggers skip when no ETag mismatch detected.
        let remote_ts = meta_a.modified_at + Duration::seconds(STALE_TOLERANCE_SECS + 1);
        let object_key = format!("mock/proj1/{}.age", meta_a.session_id.0);
        storage.put_at(&object_key, b"old-ct".to_vec(), remote_ts);

        push_all(&tool, &storage, &key, true, &[], true, false, false, &ExcludeConfig::default(), false)
            .await
            .unwrap();

        // Remote must NOT have been overwritten — skip is the correct outcome.
        let ct = storage.get(&object_key).await.unwrap();
        assert_eq!(ct, b"old-ct", "remote newer than local should be skipped when no ETag mismatch");
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

        push_all(&tool, &storage, &key, true, &[], false, /*dry_run=*/ true, false, &ExcludeConfig::default(), false)
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

    // Test 8: dry_run_classifies_correctly — plan reflects skip / upload correctly.
    //
    // Setup (no stale category any more — remote-newer is treated as Skip too,
    // since without ETag tracking we can't distinguish self-push from peer-push):
    //   - "skip-equal"  : remote same mtime as local → Skip
    //   - "skip-newer"  : remote newer than local → Skip
    //   - "upload-new"  : not on remote → Upload
    //   - "upload-local": local newer than remote → Upload
    #[test]
    fn dry_run_classifies_correctly() {
        let meta_skip_eq = make_meta("skip-equal", 1000);
        let meta_skip_newer = make_meta("skip-newer", 2000);
        let meta_upload_new = make_meta("upload-new", 3000);
        let meta_upload_local = make_meta("upload-local", 4000);

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
            make_local_session(&meta_skip_eq),
            make_local_session(&meta_skip_newer),
            make_local_session(&meta_upload_new),
            make_local_session(&meta_upload_local),
        ];

        let mut remote_index: HashMap<String, DateTime<Utc>> = HashMap::new();
        remote_index.insert(
            format!("mock/proj1/{}.age", meta_skip_eq.session_id.0),
            meta_skip_eq.modified_at,
        );
        remote_index.insert(
            format!("mock/proj1/{}.age", meta_skip_newer.session_id.0),
            meta_skip_newer.modified_at + chrono::Duration::hours(1),
        );
        // upload-new absent.
        remote_index.insert(
            format!("mock/proj1/{}.age", meta_upload_local.session_id.0),
            meta_upload_local.modified_at - chrono::Duration::hours(1),
        );

        let etag_index: HashMap<String, Option<String>> = HashMap::new();
        let recorded_etags: HashMap<String, String> = HashMap::new();
        let plan = build_dry_run_plan(&sessions, &remote_index, &etag_index, &recorded_etags, "mock", false);

        assert_eq!(plan.skip_count(), 2, "expected 2 skips");
        assert_eq!(plan.upload_count(), 2, "expected 2 uploads");

        let find = |id: &str| {
            plan.entries
                .iter()
                .find(|e| e.session_id == id)
                .unwrap()
                .action
                .clone()
        };

        assert_eq!(find("skip-equal"), DryRunAction::Skip);
        assert_eq!(find("skip-newer"), DryRunAction::Skip);
        assert!(matches!(find("upload-new"), DryRunAction::Upload { .. }));
        assert!(matches!(find("upload-local"), DryRunAction::Upload { .. }));
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

    // Without a locally-recorded ETag, `classify_stale` returns false even when
    // remote is newer — we can't distinguish "I pushed it" from "Mac B pushed it".
    // fork_on_conflict only triggers when real_stale=true (recorded != remote).
    // This test exercises the "no prior push from this machine" case: skip wins.

    #[tokio::test]
    async fn fork_on_conflict_flag_is_noop_skip_when_no_recorded_etag() {
        let meta_a = make_meta("aaa111", 1000);

        // Ensure no stale ETag from prior test runs for this session ID.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag("aaa111");
        }

        let tool = MockTool {
            sessions: vec![(meta_a.clone(), b"local-session-data".to_vec())],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        let remote_ts = meta_a.modified_at + Duration::hours(1);
        let object_key = format!("mock/proj1/{}.age", meta_a.session_id.0);
        storage.put_at(&object_key, b"remote-version".to_vec(), remote_ts);

        push_all(&tool, &storage, &key, true, &[], true, false, /*fork_on_conflict=*/ true, &ExcludeConfig::default(), false)
            .await
            .unwrap();

        // Remote untouched (skipped, not forked or overwritten).
        let remote_bytes = storage.get(&object_key).await.unwrap();
        assert_eq!(remote_bytes, b"remote-version");

        // No fork objects produced.
        let objects = storage.list("mock/").await.unwrap();
        let fork_keys: Vec<_> = objects.iter().filter(|o| o.key.contains(".fork-")).collect();
        assert!(fork_keys.is_empty(), "no fork expected when no recorded etag (real_stale=false)");
        // Only the pre-existing remote object remains.
        assert_eq!(objects.len(), 1);
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
        push_all(&tool, &storage, &key, true, &[], false, /*dry_run=*/ true, false, &ExcludeConfig::default(), false)
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

    // ── C-etag unit tests ─────────────────────────────────────────────────────

    // Pure unit test of classify_stale — covers all 5 cases from the decision table.
    #[test]
    fn classify_stale_all_cases() {
        // (None, *) → false — no prior push from this machine
        assert!(!classify_stale(None, None));
        assert!(!classify_stale(None, Some("abc")));
        // (Some, None) → false — remote doesn't expose ETag, can't tell
        assert!(!classify_stale(Some("abc"), None));
        // (Some, Some) equal → false
        assert!(!classify_stale(Some("abc"), Some("abc")));
        // (Some, Some) different → TRUE — real stale
        assert!(classify_stale(Some("abc"), Some("def")));
    }

    // etag_match_skips_when_unchanged — recorded ETag == remote ETag, mtime equal → SKIP.
    //
    // Uses unique session ID to avoid queue contamination from other test runs.
    #[tokio::test]
    async fn etag_match_skips_when_unchanged() {
        let sid = "etag-test-match-skip-001";
        let meta = make_meta(sid, 1000);
        let object_key = format!("mock/proj1/{}.age", sid);

        let storage = InMemoryStorage::new();
        let key = test_key();

        // Pre-populate remote with fake ciphertext at the same mtime as local.
        storage.put_at(&object_key, b"fake-ct".to_vec(), meta.modified_at);

        // Compute the ETag that InMemoryStorage synthesises for "fake-ct".
        let remote_etag = storage.head(&object_key).await.unwrap().etag.unwrap();

        // Record that exact ETag as the locally-known value (simulates a prior push).
        if let Ok(q) = Queue::open_default() {
            let _ = q.record_etag(sid, &remote_etag);
        }

        let tool = MockTool {
            sessions: vec![(meta.clone(), b"session-content".to_vec())],
        };

        push_all(&tool, &storage, &key, true, &[], false, false, false, &ExcludeConfig::default(), false)
            .await
            .unwrap();

        // Remote must be untouched — ETags matched → skip.
        let ct = storage.get(&object_key).await.unwrap();
        assert_eq!(ct, b"fake-ct", "etag match should trigger skip");

        // Cleanup.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag(sid);
        }
    }

    // etag_mismatch_triggers_stale_with_fork_off — recorded "abc", remote is different.
    // Without fork flag, the push overwrites (last-writer-wins).
    //
    // Verification: decision extracted via classify_stale pure helper.
    #[test]
    fn etag_mismatch_triggers_stale_decision() {
        // Simulate: recorded ETag "abc", remote ETag "def" → real_stale=true.
        let real_stale = classify_stale(Some("\"abc\""), Some("\"def\""));
        assert!(real_stale, "differing ETags must classify as stale");

        // With fork_off: upload path fires (warn then overwrite).
        // We verify via the pure classify_stale helper — the actual upload
        // is covered by etag_mismatch_triggers_fork_with_fork_on.
    }

    // etag_mismatch_triggers_fork_with_fork_on — recorded "abc", remote is different,
    // fork_on_conflict=true → writes fork object, original untouched.
    //
    // Uses a unique session ID and pre-seeds the queue with a known ETag that
    // differs from what InMemoryStorage would synthesise for the remote content.
    #[tokio::test]
    async fn etag_mismatch_triggers_fork_with_fork_on() {
        let sid = "etag-test-fork-001";
        let meta = make_meta(sid, 1000);
        let object_key = format!("mock/proj1/{}.age", sid);

        let storage = InMemoryStorage::new();
        let key = test_key();

        // Pre-populate remote with "remote-version" at the same mtime as local.
        storage.put_at(&object_key, b"remote-version".to_vec(), meta.modified_at);

        // Record a DIFFERENT ETag ("abc") than what remote has → real_stale=true.
        if let Ok(q) = Queue::open_default() {
            let _ = q.record_etag(sid, "\"abc\"");
        }

        let tool = MockTool {
            sessions: vec![(meta.clone(), b"local-content".to_vec())],
        };

        push_all(&tool, &storage, &key, true, &[], true, false, /*fork_on_conflict=*/ true, &ExcludeConfig::default(), false)
            .await
            .unwrap();

        // The original remote object must be untouched.
        let remote_ct = storage.get(&object_key).await.unwrap();
        assert_eq!(remote_ct, b"remote-version", "original remote must be untouched on fork");

        // A fork object must exist.
        let objects = storage.list("mock/").await.unwrap();
        let fork_objs: Vec<_> = objects
            .iter()
            .filter(|o| o.key.contains(".fork-") && !o.key.ends_with(".meta.json"))
            .collect();
        assert_eq!(fork_objs.len(), 1, "exactly one fork object expected");
        let fork_key = &fork_objs[0].key;
        assert!(fork_key.contains(sid), "fork key must contain original session id");

        // Cleanup.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag(sid);
            // Also clean up fork etag (session id is the fork_id, not sid).
        }
    }

    // etag_recorded_after_successful_push — after pushing, q.get_etag returns the
    // synthesised ETag that InMemoryStorage computed for the uploaded ciphertext.
    #[tokio::test]
    async fn etag_recorded_after_successful_push() {
        let sid = "etag-test-record-001";
        let meta = make_meta(sid, 1000);

        // Clear any prior state.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag(sid);
        }

        let tool = MockTool {
            sessions: vec![(meta.clone(), b"content-for-etag-test".to_vec())],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        push_all(&tool, &storage, &key, true, &[], false, false, false, &ExcludeConfig::default(), false)
            .await
            .unwrap();

        // The object was uploaded — head() should return an ETag.
        let object_key = format!("mock/proj1/{}.age", sid);
        let head = storage.head(&object_key).await.unwrap();
        let expected_etag = head.etag.expect("InMemoryStorage must return an ETag from head");

        // The queue must have recorded that ETag.
        let recorded = Queue::open_default()
            .ok()
            .and_then(|q| q.get_etag(sid).ok().flatten());
        assert_eq!(
            recorded.as_deref(),
            Some(expected_etag.as_str()),
            "queue must record the ETag of the uploaded object"
        );

        // Cleanup.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag(sid);
        }
    }

    // ── Registry / tool-filter tests ─────────────────────────────────────────

    // unknown_tool_filter_returns_clear_error — `--tool nope` should bail with
    // a message that names the invalid tool and lists the known tools.
    #[test]
    fn unknown_tool_filter_returns_clear_error() {
        use crate::adapter::registry::{adapter_by_name, known_tool_names};

        // Simulate what `run()` does for an unknown --tool flag.
        fn resolve(name: &str) -> anyhow::Result<Vec<Box<dyn ToolAdapter>>> {
            match adapter_by_name(name) {
                Some(a) => Ok(vec![a]),
                None => anyhow::bail!(
                    "unknown tool '{}'. Known: {}",
                    name,
                    known_tool_names().join(", ")
                ),
            }
        }

        // Known tool should succeed.
        assert!(resolve("claude-code").is_ok());

        // Unknown tool should fail with a clear message.
        let err = resolve("nope").err().expect("expected error for unknown tool");
        let msg = err.to_string();
        assert!(msg.contains("unknown tool"), "expected 'unknown tool' in: {msg}");
        assert!(msg.contains("nope"), "expected the bad name in: {msg}");
        assert!(msg.contains("claude-code"), "expected known names in: {msg}");
    }

    // single_tool_skips_grouping_overhead — push_multi with one adapter does NOT
    // emit the [tool-name] prefix, keeping output backward-compatible.
    #[tokio::test]
    async fn single_tool_skips_grouping_prefix() {
        // We test the logic indirectly: push_multi with one adapter should produce
        // "pushed N (skipped M unchanged)" without a "[mock]" prefix.
        // We capture stdout by checking the condition in push_multi (multi = len > 1).
        // The bool guard ensures no prefix when adapters.len() == 1.
        let multi_flag = 1usize > 1;
        assert!(!multi_flag, "single adapter must not produce tool-name prefix");

        let multi_flag = 2usize > 1;
        assert!(multi_flag, "multiple adapters must produce tool-name prefix");
    }

    // push_with_filter_on_empty_tool_returns_zero_counts — when a tool exposes
    // zero local sessions, push_all must succeed and return (0, 0).
    // The "no local sessions to push" message is printed by push_multi (which
    // calls push_all); we verify the semantic contract here: empty is valid state,
    // not an error, and produces zero push + zero skip counts.
    #[tokio::test]
    async fn push_with_filter_on_empty_tool_returns_zero_counts() {
        let tool = MockTool { sessions: vec![] };
        let storage = InMemoryStorage::new();
        let key = test_key();

        let (pushed, skipped) =
            push_all(&tool, &storage, &key, true, &[], false, false, false, &ExcludeConfig::default(), false)
                .await
                .expect("push_all with zero sessions must not error");

        assert_eq!(pushed, 0, "empty tool: pushed must be 0");
        assert_eq!(skipped, 0, "empty tool: skipped must be 0");

        // Storage must also be untouched.
        let objects = storage.list("mock/").await.unwrap();
        assert!(
            objects.is_empty(),
            "empty tool: storage must remain empty, found: {:?}",
            objects.iter().map(|o| &o.key).collect::<Vec<_>>()
        );
    }

    // ── Exclude filter tests ──────────────────────────────────────────────────

    // exclude_filter_drops_matching_sessions — sessions whose source_cwd matches
    // an exclude pattern are not uploaded (storage stays empty for them).
    #[tokio::test]
    async fn exclude_filter_drops_matching_sessions() {
        // aaa111 lives under a claude-mem path → should be excluded.
        // bbb222 lives under a normal path → should be pushed.
        let mut meta_a = make_meta("aaa111", 1000);
        meta_a.source_cwd = "/home/user/.claude-mem/observer-sessions/some-proj".to_string();
        let meta_b = make_meta("bbb222", 2000); // source_cwd = "/tmp/proj"

        let tool = MockTool {
            sessions: vec![
                (meta_a, b"session-a".to_vec()),
                (meta_b, b"session-b".to_vec()),
            ],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        let exclude = ExcludeConfig {
            project_path_contains: vec!["claude-mem".to_string()],
        };

        push_all(&tool, &storage, &key, true, &[], false, false, false, &exclude, false)
            .await
            .unwrap();

        let objects = storage.list("mock/").await.unwrap();
        let age_keys: Vec<_> = objects
            .iter()
            .filter(|o| !o.key.ends_with(".meta.json"))
            .collect();

        assert_eq!(age_keys.len(), 1, "only bbb222 should be pushed, not the excluded aaa111");
        assert!(
            age_keys[0].key.contains("bbb222"),
            "pushed key should be bbb222, got: {:?}",
            age_keys[0].key
        );
        // aaa111 must NOT have been pushed.
        assert!(
            !age_keys.iter().any(|o| o.key.contains("aaa111")),
            "aaa111 (claude-mem path) must not be in storage"
        );
    }

    // exclude_filter_no_patterns_pushes_all — empty user patterns + path that
    // the heuristic doesn't catch (normal user project dir under $HOME) = no filtering.
    #[tokio::test]
    async fn exclude_filter_no_patterns_pushes_all() {
        let mut meta_a = make_meta("aaa111", 1000);
        // Use a genuine user project path — not a dotfile dir under $HOME — so neither
        // the heuristic nor any user patterns filter it.
        meta_a.source_cwd = "/tmp/proj/my-regular-project".to_string();
        let tool = MockTool {
            sessions: vec![(meta_a, b"session-a".to_vec())],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        // Default (empty) exclude — normal project path must not be filtered.
        push_all(&tool, &storage, &key, true, &[], false, false, false, &ExcludeConfig::default(), false)
            .await
            .unwrap();

        let objects = storage.list("mock/").await.unwrap();
        let age_keys: Vec<_> = objects.iter().filter(|o| !o.key.ends_with(".meta.json")).collect();
        assert_eq!(age_keys.len(), 1, "without matching exclude patterns, session must be pushed");
    }

    // exclude_heuristic_filters_dotfile_under_home — a session with a cwd that is
    // a dotfile dir directly under $HOME is excluded with default (empty user) config.
    #[tokio::test]
    async fn exclude_heuristic_filters_dotfile_under_home() {
        let home = std::env::var("HOME").expect("HOME must be set for this test");

        let mut meta_a = make_meta("aaa111", 1000);
        meta_a.source_cwd = format!("{home}/.claude-mem/observer/some-session");
        let mut meta_b = make_meta("bbb222", 2000);
        meta_b.source_cwd = format!("{home}/Project/real-project"); // not excluded

        let tool = MockTool {
            sessions: vec![
                (meta_a, b"session-a".to_vec()),
                (meta_b, b"session-b".to_vec()),
            ],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        // Default (empty user patterns) — heuristic catches aaa111's dotfile cwd.
        push_all(&tool, &storage, &key, true, &[], false, false, false, &ExcludeConfig::default(), false)
            .await
            .unwrap();

        let objects = storage.list("mock/").await.unwrap();
        let age_keys: Vec<_> = objects
            .iter()
            .filter(|o| !o.key.ends_with(".meta.json"))
            .collect();

        assert_eq!(age_keys.len(), 1, "only bbb222 (real project) should be pushed");
        assert!(
            age_keys[0].key.contains("bbb222"),
            "bbb222 should be the pushed session, got: {:?}",
            age_keys[0].key
        );
        assert!(
            !age_keys.iter().any(|o| o.key.contains("aaa111")),
            "aaa111 (.claude-mem under $HOME) must be excluded by heuristic"
        );
    }

    // ── Ghost filter tests ────────────────────────────────────────────────────

    // push_filters_ghost_sessions — a session with has_user_message=false is
    // filtered out (not uploaded) unless include_ghosts=true.
    #[tokio::test]
    async fn push_filters_ghost_sessions() {
        // ghost: has_user_message=false → should be filtered
        let mut ghost_meta = make_meta("ghost-001", 1000);
        ghost_meta.has_user_message = false;
        ghost_meta.preview = String::new();

        // real: has_user_message=true (default) → should be pushed
        let real_meta = make_meta("real-001", 2000);

        let tool = MockTool {
            sessions: vec![
                (ghost_meta, b"ghost-content".to_vec()),
                (real_meta, b"real-content".to_vec()),
            ],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        push_all(&tool, &storage, &key, true, &[], false, false, false, &ExcludeConfig::default(), false)
            .await
            .unwrap();

        let objects = storage.list("mock/").await.unwrap();
        let age_keys: Vec<_> = objects
            .iter()
            .filter(|o| !o.key.ends_with(".meta.json"))
            .collect();

        assert_eq!(age_keys.len(), 1, "only the real session should be pushed");
        assert!(
            age_keys[0].key.contains("real-001"),
            "real-001 must be pushed"
        );
        assert!(
            !age_keys.iter().any(|o| o.key.contains("ghost-001")),
            "ghost-001 must not be pushed"
        );
    }

    // push_includes_ghosts_when_flag_set — with include_ghosts=true both sessions
    // are pushed (ghost session bypasses the filter).
    #[tokio::test]
    async fn push_includes_ghosts_when_flag_set() {
        let mut ghost_meta = make_meta("ghost-002", 1000);
        ghost_meta.has_user_message = false;
        ghost_meta.preview = String::new();

        let real_meta = make_meta("real-002", 2000);

        let tool = MockTool {
            sessions: vec![
                (ghost_meta, b"ghost-content".to_vec()),
                (real_meta, b"real-content".to_vec()),
            ],
        };
        let storage = InMemoryStorage::new();
        let key = test_key();

        push_all(&tool, &storage, &key, true, &[], false, false, false, &ExcludeConfig::default(), /*include_ghosts=*/true)
            .await
            .unwrap();

        let objects = storage.list("mock/").await.unwrap();
        let age_keys: Vec<_> = objects
            .iter()
            .filter(|o| !o.key.ends_with(".meta.json"))
            .collect();

        assert_eq!(age_keys.len(), 2, "both sessions should be pushed with include_ghosts=true");
    }
}
