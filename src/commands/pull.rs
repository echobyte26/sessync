use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::registry::{adapter_by_name, all_adapters, known_tool_names};
use crate::adapter::storage::StorageAdapter;
use crate::adapter::tool::ToolAdapter;
use crate::config::{Config, ExcludeConfig, StorageKind};
use crate::crypto;
use crate::passphrase_store;
use crate::queue::Queue;
use crate::types::{SessionId, SessionMeta};
use anyhow::{Context, Result};
use chrono::DateTime;
use chrono::Utc;
use std::collections::HashMap;
use tracing::info;

pub async fn run(quiet: bool, tool: Option<String>, dry_run: bool) -> Result<()> {
    let cfg =
        Config::load(&Config::default_path()).context("load config (run `sessync init` first?)")?;
    let passphrase = passphrase_store::load_passphrase().context("load passphrase")?;
    let salt = crypto::decode_salt_hex(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;
    let exclude = cfg.exclude.clone();

    // Resolve which adapters to pull for.
    let adapters: Vec<Box<dyn ToolAdapter>> = if let Some(ref name) = tool {
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
            pull_multi(&adapters, &storage, &key, quiet, tool.as_deref(), dry_run, &exclude).await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            pull_multi(&adapters, &storage, &key, quiet, tool.as_deref(), dry_run, &exclude).await
        }
    }
}

/// Loop over multiple adapters, calling `pull_all` for each.
/// When more than one adapter produces output, prefix each line with `[tool-name]`.
async fn pull_multi<S: StorageAdapter>(
    adapters: &[Box<dyn ToolAdapter>],
    storage: &S,
    key: &[u8; 32],
    quiet: bool,
    tool_filter: Option<&str>,
    dry_run: bool,
    exclude: &ExcludeConfig,
) -> Result<()> {
    let mut per_tool: Vec<(&str, usize, usize)> = Vec::new(); // (name, pulled, skipped)
    let mut all_errors: Vec<String> = Vec::new();

    for adapter in adapters {
        let tool_name = adapter.name();
        let result =
            pull_all(adapter.as_ref(), storage, key, /*quiet=*/ true, tool_filter, dry_run, exclude)
                .await;
        match result {
            Ok((pulled, skipped)) => {
                per_tool.push((tool_name, pulled, skipped));
            }
            Err(e) => {
                all_errors.push(format!("[{tool_name}] {e}"));
            }
        }
    }

    if !quiet && !dry_run {
        let multi = per_tool.len() > 1;
        for (name, pulled, skipped) in &per_tool {
            if *pulled == 0 && *skipped == 0 {
                if multi {
                    println!("[{name}] no remote sessions to pull");
                } else {
                    println!("no remote sessions to pull");
                }
            } else if multi {
                println!("[{name}] pulled {pulled} (skipped {skipped} already current)");
            } else {
                println!("pulled {pulled} (skipped {skipped} already current)");
            }
        }
    }

    if !all_errors.is_empty() {
        anyhow::bail!("{}", all_errors.join("\n"));
    }
    Ok(())
}

/// The core pull worker — generic over storage so tests can use InMemoryStorage.
///
/// Returns (pulled, skipped).
///
/// # Skip conditions
/// - Local session exists AND local.modified_at >= remote.last_modified
/// - Recorded ETag in queue matches remote ETag (we already pulled this version)
///
/// # Pull conditions
/// - Local session is missing from local store
/// - Remote ETag differs from recorded ETag (someone else pushed updates)
///
/// # Conflict resolution
/// For v0.7.0, remote wins: we overwrite local when remote is newer or ETag
/// differs. This is symmetric with push's last-writer-wins default. Fork
/// support for the pull direction is deferred to v0.7.1+.
///
/// # Codex cwd note
/// `write_session` is called with `source_cwd` from the meta — preserving the
/// original project association as best we can. For Codex, the session file will
/// land in the correct location for `codex --resume <id>`, but Codex.app's
/// sidebar groups by cwd. If `source_cwd` doesn't exist on this machine, the
/// session won't appear under a visible project. Use `sessync resume` (interactive
/// picker) when you need correct sidebar placement.
pub async fn pull_all<S: StorageAdapter>(
    tool: &dyn ToolAdapter,
    storage: &S,
    key: &[u8; 32],
    _quiet: bool,
    _tool_filter: Option<&str>,
    dry_run: bool,
    exclude: &ExcludeConfig,
) -> Result<(usize, usize)> {
    // Open queue best-effort — never fail pull because queue.db is unavailable.
    // Dry-run never touches the queue at all.
    let q = if dry_run { None } else { Queue::open_default().ok() };

    // 1. List all remote objects under this tool's prefix.
    let prefix = format!("{}/", tool.name());
    let remote_objects = storage.list(&prefix).await?;

    // Build indexes for .age files only (skip .meta.json sidecars in the index,
    // but we will fetch them individually as needed).
    let remote_age_index: HashMap<String, (DateTime<Utc>, Option<String>)> = remote_objects
        .iter()
        .filter(|o| o.key.ends_with(".age") && !o.key.ends_with(".meta.json"))
        .map(|o| (o.key.clone(), (o.last_modified, o.etag.clone())))
        .collect();

    if remote_age_index.is_empty() {
        info!("pull: no remote .age objects found for {}", tool.name());
        return Ok((0, 0));
    }

    // 2. Build a map of local session_id → modified_at for skip-if-current logic.
    let local_sessions = tool.list_local_sessions().await?;
    let local_mtime_index: HashMap<&str, DateTime<Utc>> = local_sessions
        .iter()
        .map(|s| (s.meta.session_id.0.as_str(), s.meta.modified_at))
        .collect();

    // ── Dry-run early exit ─────────────────────────────────────────────────────
    if dry_run {
        // Read recorded ETags read-only.
        let recorded_etags: HashMap<String, String> = Queue::open_default()
            .ok()
            .and_then(|rq| rq.all_etags().ok())
            .unwrap_or_default();

        let plan = build_dry_run_plan(
            &remote_age_index,
            &local_mtime_index,
            &recorded_etags,
        );
        print_dry_run_plan(&plan);
        return Ok((0, 0));
    }

    let mut pulled = 0usize;
    let mut skipped = 0usize;
    let mut excluded = 0usize;
    let mut errors: Vec<String> = Vec::new();

    // 3. For each remote .age object, decide whether to pull or skip.
    for (object_key, (remote_mtime, remote_etag)) in &remote_age_index {
        // Parse {tool}/{project_key}/{session_id}.age
        let session_id = match parse_session_id(object_key) {
            Some(id) => id,
            None => {
                // Malformed key (e.g., fork objects with ".fork-" suffix) — skip silently.
                continue;
            }
        };

        // Skip fork objects — they have the shape "{id}.fork-{hash}".
        if session_id.contains(".fork-") {
            continue;
        }

        // Retrieve the locally-recorded ETag for this session.
        let recorded_etag: Option<String> = q
            .as_ref()
            .and_then(|q| q.get_etag(&session_id).ok().flatten());

        // Skip condition A: ETag matches — we already have this version.
        if let (Some(ref rec), Some(ref rem)) = (&recorded_etag, remote_etag) {
            if rec == rem {
                info!("pull: skipped {} (ETag current)", session_id);
                skipped += 1;
                continue;
            }
        }

        // Skip condition B: Local exists and is at least as new as remote
        // (and no ETag mismatch was detected above).
        if let Some(&local_mtime) = local_mtime_index.get(session_id.as_str()) {
            // ETag mismatch → force download even if local_mtime >= remote_mtime.
            let etag_mismatch = match (&recorded_etag, remote_etag) {
                (Some(rec), Some(rem)) => rec != rem,
                _ => false,
            };

            if !etag_mismatch && local_mtime >= *remote_mtime {
                info!("pull: skipped {} (local is current)", session_id);
                skipped += 1;
                continue;
            }
        }

        // 4. Pull this session: fetch the sidecar meta + content, decrypt, write.
        let meta_key = format!("{}.meta.json", object_key);

        // GET and decrypt the .meta.json sidecar.
        let meta: SessionMeta = match storage.get(&meta_key).await {
            Ok(ct) => match crypto::decrypt(&ct, key) {
                Ok(pt) => match serde_json::from_slice::<SessionMeta>(&pt) {
                    Ok(m) => m,
                    Err(e) => {
                        let msg = format!("parse meta {meta_key}: {e}");
                        errors.push(msg);
                        continue;
                    }
                },
                Err(e) => {
                    let msg = format!("decrypt meta {meta_key}: {e}");
                    errors.push(msg);
                    continue;
                }
            },
            Err(e) => {
                let msg = format!("fetch meta {meta_key}: {e}");
                errors.push(msg);
                continue;
            }
        };

        // Apply exclude filter — skip if source_cwd matches any pattern.
        if exclude.matches(&meta.source_cwd) {
            info!(
                "pull: excluded {} (source_cwd {:?} matches exclude pattern)",
                session_id, meta.source_cwd
            );
            excluded += 1;
            continue;
        }

        // GET and decrypt the .age session content.
        let raw: Vec<u8> = match storage.get(object_key).await {
            Ok(ct) => match crypto::decrypt(&ct, key) {
                Ok(pt) => pt,
                Err(e) => {
                    let msg = format!("decrypt {object_key}: {e}");
                    errors.push(msg);
                    continue;
                }
            },
            Err(e) => {
                let msg = format!("fetch {object_key}: {e}");
                errors.push(msg);
                continue;
            }
        };

        // Write the session to disk using source_cwd as the target directory.
        // This preserves the original project association as best we can on the
        // receiving machine. See the Codex cwd note in this function's doc comment.
        let session_id_typed = SessionId(session_id.clone());
        if let Err(e) = tool
            .write_session(&session_id_typed, &meta.source_cwd, &raw)
            .await
        {
            let msg = format!("write_session {session_id}: {e}");
            errors.push(msg);
            continue;
        }

        // Record the remote ETag so next pull can use it for skip-if-current.
        if let Some(ref etag) = remote_etag {
            if let Some(ref q) = q {
                let _ = q.record_etag(&session_id, etag);
            }
        }

        info!("pull: pulled {} from {}", session_id, tool.name());
        pulled += 1;
    }

    info!("pull: pulled {pulled} (skipped {skipped} already current)");

    if excluded > 0 && !exclude.project_path_contains.is_empty() {
        println!(
            "excluded {excluded} session{} matching {:?}",
            if excluded == 1 { "" } else { "s" },
            exclude.project_path_contains,
        );
    }

    if !errors.is_empty() {
        anyhow::bail!(
            "{} session(s) failed to pull:\n{}",
            errors.len(),
            errors.join("\n")
        );
    }
    Ok((pulled, skipped))
}

/// Parse a session_id from a storage object key of the form
/// `{tool}/{project_key}/{session_id}.age`.
/// Returns `None` if the key doesn't match the expected layout.
fn parse_session_id(object_key: &str) -> Option<String> {
    // Key shape: "tool/project_key/session_id.age"
    // We split on '/' and take the last segment, then strip ".age".
    let last = object_key.split('/').next_back()?;
    last.strip_suffix(".age").map(|s| s.to_string())
}

// ── Dry-run plan ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum DryRunAction {
    /// Session is already current locally — no download needed.
    Skip,
    /// Session is missing locally or ETag changed — would be downloaded.
    Download,
}

struct DryRunEntry {
    session_id: String,
    action: DryRunAction,
}

struct DryRunPlan {
    entries: Vec<DryRunEntry>,
}

impl DryRunPlan {
    fn download_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.action == DryRunAction::Download)
            .count()
    }

    fn skip_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.action == DryRunAction::Skip)
            .count()
    }
}

/// Build a dry-run plan: classify each remote session without touching storage or the queue.
fn build_dry_run_plan(
    remote_age_index: &HashMap<String, (DateTime<Utc>, Option<String>)>,
    local_mtime_index: &HashMap<&str, DateTime<Utc>>,
    recorded_etags: &HashMap<String, String>,
) -> DryRunPlan {
    let mut entries = Vec::new();

    for (object_key, (remote_mtime, remote_etag)) in remote_age_index {
        let session_id = match parse_session_id(object_key) {
            Some(id) => id,
            None => continue,
        };

        // Skip fork objects in dry-run as well.
        if session_id.contains(".fork-") {
            continue;
        }

        let recorded_etag = recorded_etags.get(&session_id);

        // ETag match → skip.
        let etag_current = match (recorded_etag, remote_etag) {
            (Some(rec), Some(rem)) => rec == rem,
            _ => false,
        };

        if etag_current {
            entries.push(DryRunEntry {
                session_id,
                action: DryRunAction::Skip,
            });
            continue;
        }

        // Local mtime >= remote mtime AND no ETag mismatch → skip.
        let etag_mismatch = match (recorded_etag, remote_etag) {
            (Some(rec), Some(rem)) => rec != rem,
            _ => false,
        };

        if !etag_mismatch {
            if let Some(&local_mtime) = local_mtime_index.get(session_id.as_str()) {
                if local_mtime >= *remote_mtime {
                    entries.push(DryRunEntry {
                        session_id,
                        action: DryRunAction::Skip,
                    });
                    continue;
                }
            }
        }

        // Otherwise → download.
        entries.push(DryRunEntry {
            session_id,
            action: DryRunAction::Download,
        });
    }

    // Sort entries for deterministic output in tests.
    entries.sort_by(|a, b| a.session_id.cmp(&b.session_id));

    DryRunPlan { entries }
}

/// Print the dry-run plan and a summary line (mirrors push --dry-run output style).
fn print_dry_run_plan(plan: &DryRunPlan) {
    for entry in &plan.entries {
        match entry.action {
            DryRunAction::Skip => {
                println!("would skip {} (already current)", entry.session_id);
            }
            DryRunAction::Download => {
                println!("would pull {}", entry.session_id);
            }
        }
    }
    let pull_n = plan.download_count();
    let skip_m = plan.skip_count();
    println!("dry-run summary: would pull {pull_n}, skip {skip_m}");
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::memory::InMemoryStorage;
    use crate::adapter::tool::{LocalSession, ToolAdapter};
    use crate::config::ExcludeConfig;
    use crate::crypto;
    use crate::error::Result as SessyncResult;
    use crate::types::{ProjectKey, SessionId, SessionMeta};
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

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

    /// A mock ToolAdapter that records write_session calls in-memory.
    /// Supports configuring what local sessions are "known" (for list_local_sessions).
    struct MockTool {
        name: &'static str,
        /// Sessions reported as locally known.
        local_sessions: Vec<(SessionMeta, Vec<u8>)>,
        /// Accumulates (session_id, cwd, raw) tuples from write_session calls.
        written: Arc<Mutex<Vec<(String, String, Vec<u8>)>>>,
    }

    impl MockTool {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                local_sessions: Vec::new(),
                written: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_sessions(mut self, sessions: Vec<(SessionMeta, Vec<u8>)>) -> Self {
            self.local_sessions = sessions;
            self
        }

        fn written_count(&self) -> usize {
            self.written.lock().unwrap().len()
        }

        fn written_ids(&self) -> Vec<String> {
            self.written
                .lock()
                .unwrap()
                .iter()
                .map(|(id, _, _)| id.clone())
                .collect()
        }
    }

    #[async_trait]
    impl ToolAdapter for MockTool {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn list_local_sessions(&self) -> SessyncResult<Vec<LocalSession>> {
            let mut out = vec![];
            for (meta, bytes) in &self.local_sessions {
                let path = std::env::temp_dir()
                    .join(format!("sessync_pull_test_{}.jsonl", meta.session_id));
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
            id: &SessionId,
            cwd: &str,
            raw: &[u8],
        ) -> SessyncResult<PathBuf> {
            self.written
                .lock()
                .unwrap()
                .push((id.0.clone(), cwd.to_string(), raw.to_vec()));
            // Return a fake path.
            Ok(PathBuf::from(format!("/tmp/{}.jsonl", id.0)))
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
    }

    /// Encrypt a SessionMeta + raw bytes into storage, simulating what push does.
    /// The storage `last_modified` is set to `Utc::now()`.
    async fn seed_remote(
        storage: &InMemoryStorage,
        tool_name: &str,
        meta: &SessionMeta,
        raw_content: &[u8],
        key: &[u8; 32],
    ) {
        seed_remote_at(storage, tool_name, meta, raw_content, key, Utc::now()).await;
    }

    /// Like `seed_remote` but with an explicit `last_modified` timestamp for
    /// testing the mtime-based skip logic.
    async fn seed_remote_at(
        storage: &InMemoryStorage,
        tool_name: &str,
        meta: &SessionMeta,
        raw_content: &[u8],
        key: &[u8; 32],
        at: DateTime<Utc>,
    ) {
        let object_key = format!(
            "{}/{}/{}.age",
            tool_name, meta.project_key.0, meta.session_id.0
        );
        let meta_key = format!("{}.meta.json", object_key);

        let ct = crypto::encrypt(raw_content, key).unwrap();
        storage.put_at(&object_key, ct, at);

        let meta_json = serde_json::to_vec(meta).unwrap();
        let meta_ct = crypto::encrypt(&meta_json, key).unwrap();
        storage.put_at(&meta_key, meta_ct, at);
    }

    // ── Test 1: empty remote → (0, 0) ─────────────────────────────────────────

    #[tokio::test]
    async fn pull_when_nothing_remote_pulls_nothing() {
        let tool = MockTool::new("mock");
        let storage = InMemoryStorage::new();
        let key = test_key();

        let (pulled, skipped) = pull_all(&tool, &storage, &key, true, None, false, &ExcludeConfig::default())
            .await
            .unwrap();

        assert_eq!(pulled, 0, "no remote sessions → pulled must be 0");
        assert_eq!(skipped, 0, "no remote sessions → skipped must be 0");
        assert_eq!(tool.written_count(), 0, "no write_session calls expected");
    }

    // ── Test 2: 2 remote sessions, local has 0 → pulls both ───────────────────

    #[tokio::test]
    async fn pull_downloads_missing_sessions() {
        let meta_a = make_meta("aaa111", 1000);
        let meta_b = make_meta("bbb222", 2000);
        let key = test_key();

        let storage = InMemoryStorage::new();
        seed_remote(&storage, "mock", &meta_a, b"content-a", &key).await;
        seed_remote(&storage, "mock", &meta_b, b"content-b", &key).await;

        let tool = MockTool::new("mock"); // no local sessions
        let (pulled, skipped) = pull_all(&tool, &storage, &key, true, None, false, &ExcludeConfig::default())
            .await
            .unwrap();

        assert_eq!(pulled, 2, "both sessions should be pulled");
        assert_eq!(skipped, 0, "nothing to skip");
        assert_eq!(tool.written_count(), 2, "write_session called twice");

        let written_ids = tool.written_ids();
        assert!(written_ids.contains(&"aaa111".to_string()));
        assert!(written_ids.contains(&"bbb222".to_string()));
    }

    // ── Test 3: local mtime >= remote mtime → skip ─────────────────────────────

    #[tokio::test]
    async fn pull_skips_locally_newer() {
        // Use a unique session ID to avoid ETag contamination from parallel tests.
        let sid = "pull-skip-local-newer-001";
        let local_ts = Utc.timestamp_opt(2000, 0).unwrap();
        let remote_ts = Utc.timestamp_opt(1000, 0).unwrap(); // older than local
        let meta_a = make_meta(sid, 2000); // local mtime = 2000
        let key = test_key();

        let storage = InMemoryStorage::new();

        // Remote seeded with explicit timestamp older than local.
        let mut remote_meta = meta_a.clone();
        remote_meta.modified_at = remote_ts;
        // seed_remote_at sets storage last_modified = remote_ts (< local_ts).
        seed_remote_at(&storage, "mock", &remote_meta, b"older-content", &key, remote_ts).await;

        // Local has the session at mtime=2000.
        let tool =
            MockTool::new("mock").with_sessions(vec![(meta_a, b"newer-local".to_vec())]);

        // Clear any stale ETags for this session from prior test runs.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag(sid);
        }

        let _ = local_ts; // suppress unused warning

        let (pulled, skipped) = pull_all(&tool, &storage, &key, true, None, false, &ExcludeConfig::default())
            .await
            .unwrap();

        assert_eq!(pulled, 0, "local is newer → should not pull");
        assert_eq!(skipped, 1, "local is newer → should skip");
        assert_eq!(tool.written_count(), 0, "no write_session calls expected");

        // Cleanup.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag(sid);
        }
    }

    // ── Test 4: recorded ETag matches remote → skip ────────────────────────────

    #[tokio::test]
    async fn pull_skips_when_etag_matches_recorded() {
        let sid = "pull-etag-skip-001";
        let meta = make_meta(sid, 1000);
        let key = test_key();

        let storage = InMemoryStorage::new();
        seed_remote(&storage, "mock", &meta, b"some-content", &key).await;

        // Get the ETag the storage computed for the uploaded .age object.
        let object_key = format!("mock/proj1/{}.age", sid);
        let remote_etag = storage.head(&object_key).await.unwrap().etag.unwrap();

        // Record that ETag in the queue — simulates a previous successful pull.
        if let Ok(q) = Queue::open_default() {
            let _ = q.record_etag(sid, &remote_etag);
        }

        let tool = MockTool::new("mock"); // local has nothing (doesn't matter — ETag wins)
        let (pulled, skipped) = pull_all(&tool, &storage, &key, true, None, false, &ExcludeConfig::default())
            .await
            .unwrap();

        assert_eq!(pulled, 0, "ETag matches → should not pull");
        assert_eq!(skipped, 1, "ETag matches → should skip");
        assert_eq!(tool.written_count(), 0);

        // Cleanup.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag(sid);
        }
    }

    // ── Test 5: recorded ETag differs from remote → download ──────────────────

    #[tokio::test]
    async fn pull_downloads_when_etag_differs() {
        let sid = "pull-etag-diff-001";
        let meta = make_meta(sid, 1000);
        let key = test_key();

        let storage = InMemoryStorage::new();
        seed_remote(&storage, "mock", &meta, b"new-content", &key).await;

        // Record a STALE ETag — simulates that remote was updated by another machine.
        if let Ok(q) = Queue::open_default() {
            let _ = q.record_etag(sid, "\"stale-etag\"");
        }

        // Local has the session (would normally cause a skip by mtime), but ETag
        // mismatch forces a download.
        let tool = MockTool::new("mock").with_sessions(vec![(meta.clone(), b"old-local".to_vec())]);

        let (pulled, skipped) = pull_all(&tool, &storage, &key, true, None, false, &ExcludeConfig::default())
            .await
            .unwrap();

        assert_eq!(pulled, 1, "ETag mismatch → should pull");
        assert_eq!(skipped, 0);
        assert_eq!(tool.written_count(), 1);

        // Cleanup.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag(sid);
        }
    }

    // ── Test 6: --tool filter limits scope ────────────────────────────────────

    #[tokio::test]
    async fn pull_with_tool_filter_limits_scope() {
        let meta = make_meta("aaa111", 1000);
        let key = test_key();

        // Seed under "mock" prefix.
        let storage = InMemoryStorage::new();
        seed_remote(&storage, "mock", &meta, b"content", &key).await;

        // Tool adapter named "other" — won't see "mock/" prefix objects.
        let tool = MockTool::new("other");

        let (pulled, skipped) = pull_all(&tool, &storage, &key, true, Some("mock"), false, &ExcludeConfig::default())
            .await
            .unwrap();

        // The tool's own prefix ("other/") has nothing.
        assert_eq!(pulled, 0, "other tool prefix has nothing");
        assert_eq!(skipped, 0);
    }

    // ── Test 7: dry_run does not call write_session ────────────────────────────

    #[tokio::test]
    async fn pull_dry_run_does_not_call_write_session() {
        let meta_a = make_meta("aaa111", 1000);
        let meta_b = make_meta("bbb222", 2000);
        let key = test_key();

        let storage = InMemoryStorage::new();
        seed_remote(&storage, "mock", &meta_a, b"content-a", &key).await;
        seed_remote(&storage, "mock", &meta_b, b"content-b", &key).await;

        // Clear any stale ETags from prior test runs.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag("aaa111");
            let _ = q.delete_etag("bbb222");
        }

        let tool = MockTool::new("mock");
        let (pulled, _skipped) = pull_all(&tool, &storage, &key, true, None, /*dry_run=*/ true, &ExcludeConfig::default())
            .await
            .unwrap();

        assert_eq!(pulled, 0, "dry-run must not actually pull anything");
        assert_eq!(
            tool.written_count(),
            0,
            "dry-run must not call write_session"
        );

        // Cleanup.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag("aaa111");
            let _ = q.delete_etag("bbb222");
        }
    }

    // ── Test 8: ETag recorded after successful download ────────────────────────

    #[tokio::test]
    async fn pull_records_etag_after_successful_download() {
        let sid = "pull-etag-record-001";
        let meta = make_meta(sid, 1000);
        let key = test_key();

        // Clear any stale ETag from prior test runs.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag(sid);
        }

        let storage = InMemoryStorage::new();
        seed_remote(&storage, "mock", &meta, b"content", &key).await;

        let object_key = format!("mock/proj1/{}.age", sid);
        let expected_etag = storage.head(&object_key).await.unwrap().etag.unwrap();

        let tool = MockTool::new("mock");
        pull_all(&tool, &storage, &key, true, None, false, &ExcludeConfig::default())
            .await
            .unwrap();

        // The queue must have recorded the remote ETag.
        let recorded = Queue::open_default()
            .ok()
            .and_then(|q| q.get_etag(sid).ok().flatten());

        assert_eq!(
            recorded.as_deref(),
            Some(expected_etag.as_str()),
            "pull must record the remote ETag after a successful download"
        );

        // Cleanup.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag(sid);
        }
    }

    // ── Test 9: parse_session_id helper ───────────────────────────────────────

    #[test]
    fn parse_session_id_extracts_id_from_well_formed_key() {
        let key = "claude-code/proj1/my-session-id.age";
        assert_eq!(
            parse_session_id(key),
            Some("my-session-id".to_string())
        );
    }

    #[test]
    fn parse_session_id_returns_none_for_meta_json() {
        // .meta.json keys don't end with .age so they'll fail the strip_suffix.
        let key = "claude-code/proj1/my-session.age.meta.json";
        // This key ends with ".json", not ".age" → None.
        assert_eq!(parse_session_id(key), None);
    }

    #[test]
    fn parse_session_id_returns_none_for_malformed_key() {
        assert_eq!(parse_session_id("noslash"), None);
    }

    // ── Test 10: build_dry_run_plan classification ─────────────────────────────

    #[test]
    fn dry_run_plan_classifies_correctly() {
        let remote_age_index: HashMap<String, (DateTime<Utc>, Option<String>)> = {
            let mut m = HashMap::new();
            // "missing" → no local entry and no ETag match → Download
            m.insert(
                "mock/proj1/missing.age".to_string(),
                (Utc.timestamp_opt(1000, 0).unwrap(), Some("\"etag-miss\"".to_string())),
            );
            // "current" → local mtime equal → Skip
            m.insert(
                "mock/proj1/current.age".to_string(),
                (Utc.timestamp_opt(1000, 0).unwrap(), Some("\"etag-curr\"".to_string())),
            );
            // "etag-match" → recorded ETag == remote ETag → Skip
            m.insert(
                "mock/proj1/etag-match.age".to_string(),
                (Utc.timestamp_opt(9999, 0).unwrap(), Some("\"etag-X\"".to_string())),
            );
            m
        };

        let local_mtime_index: HashMap<&str, DateTime<Utc>> = {
            let mut m = HashMap::new();
            // "current" has same mtime as remote → skip by mtime.
            m.insert("current", Utc.timestamp_opt(1000, 0).unwrap());
            // "etag-match" has older mtime than remote (9999), but ETag match → skip.
            m.insert("etag-match", Utc.timestamp_opt(500, 0).unwrap());
            m
        };

        let mut recorded_etags: HashMap<String, String> = HashMap::new();
        recorded_etags.insert("etag-match".to_string(), "\"etag-X\"".to_string());

        let plan = build_dry_run_plan(&remote_age_index, &local_mtime_index, &recorded_etags);

        assert_eq!(plan.download_count(), 1, "only 'missing' should be downloaded");
        assert_eq!(plan.skip_count(), 2, "'current' and 'etag-match' should be skipped");

        // Check that "missing" is marked Download.
        let missing_action = plan
            .entries
            .iter()
            .find(|e| e.session_id == "missing")
            .map(|e| &e.action);
        assert_eq!(missing_action, Some(&DryRunAction::Download));
    }

    // ── Test 11: fork objects are silently skipped ────────────────────────────

    #[tokio::test]
    async fn pull_skips_fork_objects() {
        let key = test_key();
        let storage = InMemoryStorage::new();

        // Seed a fork object directly (not via seed_remote, to control the key).
        let fork_key = "mock/proj1/aaa111.fork-deadbeef.age";
        let ct = crypto::encrypt(b"fork-content", &key).unwrap();
        storage.put(fork_key, ct).await.unwrap();

        let tool = MockTool::new("mock");
        let (pulled, skipped) = pull_all(&tool, &storage, &key, true, None, false, &ExcludeConfig::default())
            .await
            .unwrap();

        assert_eq!(pulled, 0, "fork objects must not be pulled");
        assert_eq!(skipped, 0, "fork objects don't count as skipped");
        assert_eq!(tool.written_count(), 0);
    }

    // ── Exclude filter tests ──────────────────────────────────────────────────

    // pull_exclude_filter_drops_matching_sessions — remote sessions whose
    // source_cwd (stored in the encrypted .meta.json) matches an exclude
    // pattern are not written to disk.
    #[tokio::test]
    async fn pull_exclude_filter_drops_matching_sessions() {
        let key = test_key();
        let storage = InMemoryStorage::new();

        // Seed two sessions:
        // - aaa111: source_cwd in a claude-mem path → excluded
        // - bbb222: source_cwd in a normal path → pulled
        let mut meta_a = make_meta("aaa111", 1000);
        meta_a.source_cwd = "/home/user/.claude-mem/observer-sessions/proj".to_string();
        let meta_b = make_meta("bbb222", 2000); // source_cwd = "/tmp/proj" (normal)

        seed_remote(&storage, "mock", &meta_a, b"content-a", &key).await;
        seed_remote(&storage, "mock", &meta_b, b"content-b", &key).await;

        // Clear ETags so no skip-by-ETag fires.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag("aaa111");
            let _ = q.delete_etag("bbb222");
        }

        let exclude = ExcludeConfig {
            project_path_contains: vec!["claude-mem".to_string()],
        };

        let tool = MockTool::new("mock");
        let (pulled, _skipped) = pull_all(&tool, &storage, &key, true, None, false, &exclude)
            .await
            .unwrap();

        assert_eq!(pulled, 1, "only bbb222 should be pulled");
        let written_ids = tool.written_ids();
        assert!(written_ids.contains(&"bbb222".to_string()), "bbb222 must be written");
        assert!(!written_ids.contains(&"aaa111".to_string()), "aaa111 (excluded) must not be written");

        // Cleanup.
        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag("aaa111");
            let _ = q.delete_etag("bbb222");
        }
    }

    // pull_exclude_no_patterns_pulls_all — empty user patterns + path that
    // doesn't hit the hardcoded plugin blacklist either = no filtering.
    #[tokio::test]
    async fn pull_exclude_no_patterns_pulls_all() {
        let key = test_key();
        let storage = InMemoryStorage::new();

        let mut meta_a = make_meta("aaa111", 1000);
        // Use a normal user project path — not in the hardcoded plugin blacklist.
        meta_a.source_cwd = "/home/user/Project/realproj".to_string();

        seed_remote(&storage, "mock", &meta_a, b"content-a", &key).await;

        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag("aaa111");
        }

        let tool = MockTool::new("mock");
        let (pulled, _) = pull_all(&tool, &storage, &key, true, None, false, &ExcludeConfig::default())
            .await
            .unwrap();

        assert_eq!(pulled, 1, "without exclude patterns all sessions are pulled");

        if let Ok(q) = Queue::open_default() {
            let _ = q.delete_etag("aaa111");
        }
    }
}
