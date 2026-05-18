//! Codex adapter — maps OpenAI Codex CLI session storage to the `ToolAdapter` trait.
//!
//! ## Storage layout (confirmed via direct inspection of ~/.codex/state_5.sqlite, 2026-05-07)
//!
//! Codex uses a dual-layer store:
//! - **SQLite index**: `~/.codex/state_N.sqlite`, table `threads`.
//! - **JSONL rollout files**: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`.
//!
//! ## Research-doc deviations (see docs/research/codex-adapter.md)
//!
//! The research doc listed a schema that was slightly wrong. Actual NOT-NULL columns are:
//!   `source`, `model_provider`, `title`, `sandbox_policy`, `approval_mode`, `has_user_event`.
//! We supply safe defaults for all of them in `write_session`.
//!
//! The research doc used "sessions table" throughout, but the actual table name is `threads`.
//!
//! `rollout_path` filename uses an ISO8601 timestamp with dashes, not a plain unix timestamp:
//!   `rollout-2025-05-07T17-24-21-<uuid>.jsonl`  (no unix epoch in the name).

use super::path_codec;
use super::tool::{LocalSession, ToolAdapter};
use crate::error::{Result, SessyncError};
use crate::types::{ProjectKey, SessionId, SessionMeta};
use async_trait::async_trait;
use chrono::Utc;
use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static CODEX_HOSTNAME: OnceLock<String> = OnceLock::new();

// ---------------------------------------------------------------------------
// Constructor helpers
// ---------------------------------------------------------------------------

fn default_codex_root() -> PathBuf {
    // Respect CODEX_HOME env var — Codex itself honours this override.
    if let Ok(home) = std::env::var("CODEX_HOME") {
        return PathBuf::from(home);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".codex")
}

fn hostname() -> String {
    CODEX_HOSTNAME
        .get_or_init(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "unknown".into())
        })
        .clone()
}

/// Extract the model_provider from a Codex rollout's first-line session_meta event.
///
/// Codex's session_meta carries a `model_provider` field (e.g. "openai"). If we
/// insert into SQLite with `model_provider = 'unknown'`, Codex refuses to resume
/// the session — fails with "Model provider `unknown` not found". So we have to
/// read the real value from the rollout and use it in the INSERT.
///
/// Returns `Some(provider)` if the first line is a session_meta with a string
/// `payload.model_provider`. Returns `None` on any shape mismatch (caller
/// should fall back to a safe default like `"openai"`).
pub fn extract_rollout_model_provider(raw: &[u8]) -> Option<String> {
    let nl = raw.iter().position(|&b| b == b'\n')?;
    let first = &raw[..nl];
    let v: serde_json::Value = serde_json::from_slice(first).ok()?;
    if v.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    v.get("payload")?
        .get("model_provider")?
        .as_str()
        .map(String::from)
}

/// Rewrite the embedded cwd in a Codex rollout's first-line session_meta event.
///
/// Codex stores the original cwd inside the jsonl rollout file (first line:
/// `{"type":"session_meta","payload":{"cwd":"...","..."}}`). On startup it
/// reads the rollout and uses that cwd to reconcile the SQLite `threads` row,
/// overwriting whatever we INSERTed. So for cross-machine sync we must rewrite
/// the rollout's cwd to the local target_cwd, otherwise the synced session
/// shows up in the Codex.app sidebar under the SOURCE machine's path (which
/// doesn't exist on the destination machine, making the session invisible
/// from the project view).
///
/// Returns `Some(new_bytes)` if the first line was a session_meta with a
/// rewritable cwd. Returns `None` if the file shape doesn't match expectations,
/// in which case the caller should write the original bytes unchanged — we
/// never break unrecognized rollouts.
pub fn rewrite_rollout_cwd(raw: &[u8], target_cwd: &str) -> Option<Vec<u8>> {
    let nl = raw.iter().position(|&b| b == b'\n')?;
    let (first, rest) = raw.split_at(nl);

    let mut v: serde_json::Value = serde_json::from_slice(first).ok()?;

    let is_session_meta = v
        .get("type")
        .and_then(|t| t.as_str())
        .map(|s| s == "session_meta")
        .unwrap_or(false);
    if !is_session_meta {
        return None;
    }

    let cwd_slot = v.get_mut("payload")?.get_mut("cwd")?;
    if !cwd_slot.is_string() {
        return None;
    }
    *cwd_slot = serde_json::Value::String(target_cwd.to_string());

    let mut new_first = serde_json::to_vec(&v).ok()?;
    let mut out = Vec::with_capacity(new_first.len() + rest.len());
    out.append(&mut new_first);
    out.extend_from_slice(rest);
    Some(out)
}

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

/// Adapter for the OpenAI Codex CLI / desktop app.
///
/// Instantiate via `CodexAdapter::new()` for production use (defaults to
/// `~/.codex/`). Use `CodexAdapter::at(path)` to redirect to a temp directory
/// in tests — this is the only way to avoid touching the real Codex install.
pub struct CodexAdapter {
    /// The Codex data home directory (`~/.codex/` by default).
    codex_root: PathBuf,
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexAdapter {
    /// Create an adapter pointed at the default `~/.codex/` root.
    pub fn new() -> Self {
        Self::at(default_codex_root())
    }

    /// Create an adapter pointed at an arbitrary root (for testing).
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { codex_root: root.into() }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Glob `<codex_root>/state_*.sqlite` and return the path with the
    /// **highest** version number (e.g. `state_6` beats `state_5`).
    /// Returns `None` if no state DB exists (Codex not installed or never run).
    fn find_state_db(&self) -> Option<PathBuf> {
        // std::fs::read_dir is fine here — this is not hot-path code and
        // we call it from blocking context (rusqlite is also blocking).
        let entries = std::fs::read_dir(&self.codex_root).ok()?;
        let mut best: Option<(u64, PathBuf)> = None;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Match "state_N.sqlite"
            if let Some(rest) = name_str.strip_prefix("state_") {
                if let Some(num_str) = rest.strip_suffix(".sqlite") {
                    if let Ok(num) = num_str.parse::<u64>() {
                        let path = entry.path();
                        match &best {
                            None => best = Some((num, path)),
                            Some((best_num, _)) if num > *best_num => best = Some((num, path)),
                            _ => {}
                        }
                    }
                }
            }
        }
        best.map(|(_, p)| p)
    }

    /// Open the Codex state DB in WAL mode.
    /// Returns `Err` with a user-facing message on schema mismatch or open failure.
    fn open_db(path: &Path) -> Result<rusqlite::Connection> {
        let conn = rusqlite::Connection::open(path).map_err(|e| {
            SessyncError::Tool(format!(
                "failed to open Codex state DB at {}: {e}",
                path.display()
            ))
        })?;
        // Enable WAL so concurrent Codex readers are not blocked by our read.
        conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| {
            SessyncError::Tool(format!("failed to set WAL mode on Codex DB: {e}"))
        })?;
        Ok(conn)
    }

    /// Verify that the `threads` table has the columns we depend on.
    /// Returns a clear error if any required column is absent (schema migration guard).
    fn verify_schema(conn: &rusqlite::Connection) -> Result<()> {
        // PRAGMA table_info returns one row per column.
        let mut stmt = conn
            .prepare("PRAGMA table_info(threads)")
            .map_err(|e| SessyncError::Tool(format!("PRAGMA table_info failed: {e}")))?;

        let existing_cols: std::collections::HashSet<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| SessyncError::Tool(format!("table_info query failed: {e}")))?
            .filter_map(|r| r.ok())
            .collect();

        // Columns we actually SELECT in list_local_sessions / read_session.
        const REQUIRED: &[&str] = &[
            "id",
            "cwd",
            "rollout_path",
            "first_user_message",
            "updated_at_ms",
        ];

        for col in REQUIRED {
            if !existing_cols.contains(*col) {
                return Err(SessyncError::Tool(format!(
                    "Codex version not supported by sessync. \
                     Required column `{col}` is missing from the `threads` table. \
                     Skipping Codex sessions."
                )));
            }
        }
        Ok(())
    }

    /// Rotate backups: keep only the `keep` most recent, delete older ones.
    fn rotate_backups(db_path: &Path, keep: usize) -> std::io::Result<()> {
        let parent = db_path.parent().unwrap_or(Path::new("."));
        let stem = db_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let prefix = format!("{}.sessync-backup-", stem);

        let mut backups: Vec<(u64, PathBuf)> = std::fs::read_dir(parent)?
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(ts_str) = name.strip_prefix(&prefix) {
                    ts_str.parse::<u64>().ok().map(|ts| (ts, e.path()))
                } else {
                    None
                }
            })
            .collect();

        // Sort oldest-first so we can remove from the front.
        backups.sort_by_key(|(ts, _)| *ts);

        // Remove oldest backups beyond the retention limit.
        // (We haven't written the new backup yet, so the target count is `keep - 1`.)
        while backups.len() >= keep {
            let (_, oldest) = backups.remove(0);
            if let Err(e) = std::fs::remove_file(&oldest) {
                tracing::warn!(path = %oldest.display(), err = %e, "failed to remove old Codex DB backup");
            }
        }
        Ok(())
    }

    /// Copy `db_path` → `db_path.sessync-backup-<unix_ms>` (keep last 3).
    fn backup_db(db_path: &Path) -> Result<()> {
        // Rotate first (keep 2 existing so after our new write we have 3 total).
        Self::rotate_backups(db_path, 3).map_err(|e| {
            SessyncError::Tool(format!(
                "failed to rotate Codex DB backups at {}: {e}",
                db_path.display()
            ))
        })?;

        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let backup_path = db_path.with_file_name(format!(
            "{}.sessync-backup-{}",
            db_path.file_name().unwrap_or_default().to_string_lossy(),
            ts
        ));

        std::fs::copy(db_path, &backup_path).map_err(|e| {
            SessyncError::Tool(format!(
                "failed to backup Codex DB to {}: {e}",
                backup_path.display()
            ))
        })?;

        tracing::debug!(backup = %backup_path.display(), "Codex DB backed up");
        Ok(())
    }

    /// Build the rollout file path used when writing a new session.
    ///
    /// Format: `<codex_root>/sessions/YYYY/MM/DD/rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl`
    ///
    /// This matches what the Codex source (`codex-rs/rollout/src/recorder.rs`) generates,
    /// so that `codex resume` can find the file via its normal scanning logic.
    fn rollout_path_for(&self, session_id: &SessionId, now: &chrono::DateTime<Utc>) -> PathBuf {
        let date_dir = now.format("%Y/%m/%d").to_string();
        let timestamp = now.format("%Y-%m-%dT%H-%M-%S").to_string();
        let filename = format!("rollout-{}-{}.jsonl", timestamp, session_id.0);
        self.codex_root.join("sessions").join(date_dir).join(filename)
    }

    /// Extract the first user message from raw JSONL bytes.
    ///
    /// Codex JSONL format: each line is `{ "item": <RolloutItem> }` where `RolloutItem`
    /// has a type discriminant. The first line is `session_meta`; user messages appear in
    /// `response_item` or `event_msg` lines. For our purposes we scan for any line whose
    /// JSON contains `"role": "user"` with a non-empty content string.
    ///
    /// Capped at 200 chars to match the preview field contract.
    fn extract_first_user_message(raw: &[u8]) -> String {
        let text = match std::str::from_utf8(raw) {
            Ok(t) => t,
            Err(_) => return String::new(),
        };
        for line in text.lines() {
            if line.len() > 1_048_576 {
                continue; // skip implausibly large lines
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                // Codex rollout format: {"item": {"session_meta": ...}} or
                // {"item": {"response_item": {"item": {"role": "user", "content": [...]}}}}
                // Try a few common paths.
                let content = v
                    .pointer("/item/response_item/item/content/0/text")
                    .or_else(|| v.pointer("/item/response_item/item/content"))
                    .and_then(|c| c.as_str());

                // Also check role
                let role = v
                    .pointer("/item/response_item/item/role")
                    .and_then(|r| r.as_str());

                if role == Some("user") {
                    if let Some(c) = content {
                        if !c.is_empty() {
                            let mut s = c.to_string();
                            if s.chars().count() > 200 {
                                s = s.chars().take(197).collect::<String>() + "...";
                            }
                            return s;
                        }
                    }
                }
            }
        }
        String::new()
    }
}

// ---------------------------------------------------------------------------
// ToolAdapter impl
// ---------------------------------------------------------------------------

#[async_trait]
impl ToolAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    async fn list_local_sessions(&self) -> Result<Vec<LocalSession>> {
        // If codex_root doesn't exist at all, Codex is not installed — return empty silently.
        if !self.codex_root.exists() {
            return Ok(vec![]);
        }

        let db_path = match self.find_state_db() {
            Some(p) => p,
            None => {
                tracing::debug!(root = %self.codex_root.display(), "no state_*.sqlite found; treating as no Codex sessions");
                return Ok(vec![]);
            }
        };

        // All SQLite work is blocking — run it in a blocking thread.
        let codex_root = self.codex_root.clone();
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String, String, String, i64)>> {
            let conn = match Self::open_db(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(err = %e, "cannot open Codex state DB; returning empty session list");
                    return Ok(vec![]);
                }
            };

            if let Err(e) = Self::verify_schema(&conn) {
                tracing::warn!(err = %e, "Codex schema not supported; returning empty session list");
                return Ok(vec![]);
            }

            let mut stmt = conn
                .prepare(
                    "SELECT id, cwd, rollout_path, first_user_message, updated_at_ms \
                     FROM threads \
                     ORDER BY updated_at_ms DESC",
                )
                .map_err(|e| SessyncError::Tool(format!("failed to prepare list query: {e}")))?;

            let rows: Vec<(String, String, String, String, i64)> = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4).unwrap_or(0),
                    ))
                })
                .map_err(|e| SessyncError::Tool(format!("failed to query threads: {e}")))?
                .filter_map(|r| {
                    r.map_err(|e| {
                        tracing::warn!(err = %e, "error reading thread row; skipping");
                        e
                    })
                    .ok()
                })
                .collect();

            let _ = codex_root; // keep codex_root alive until here
            Ok(rows)
        })
        .await
        .map_err(|e| SessyncError::Tool(format!("blocking task panicked: {e}")))??;

        // Now async-stat each rollout file and build LocalSession entries.
        let host = hostname();
        let mut out = Vec::with_capacity(rows.len());
        for (id, cwd, rollout_path_str, preview_raw, updated_at_ms) in rows {
            let rollout_path = PathBuf::from(&rollout_path_str);

            // Skip sessions whose rollout file is missing — don't crash.
            let metadata = match tokio::fs::metadata(&rollout_path).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        session_id = %id,
                        path = %rollout_path.display(),
                        err = %e,
                        "rollout file missing or unreadable; skipping session"
                    );
                    continue;
                }
            };

            let modified_at = if updated_at_ms != 0 {
                chrono::DateTime::<Utc>::from_timestamp_millis(updated_at_ms)
                    .unwrap_or_else(Utc::now)
            } else {
                metadata
                    .modified()
                    .map(chrono::DateTime::<Utc>::from)
                    .unwrap_or_else(|_| Utc::now())
            };

            // Cap preview to 200 chars.
            let preview = if preview_raw.chars().count() > 200 {
                preview_raw.chars().take(197).collect::<String>() + "..."
            } else {
                preview_raw
            };

            let project_key = path_codec::project_key_for_cwd(&cwd);

            out.push(LocalSession {
                meta: SessionMeta {
                    schema_version: 1,
                    session_id: SessionId(id),
                    project_key,
                    source_cwd: cwd,
                    source_hostname: host.clone(),
                    modified_at,
                    byte_size: metadata.len(),
                    preview,
                    has_user_message: true,
                },
                local_path: rollout_path,
            });
        }

        Ok(out)
    }

    async fn read_session(&self, session_id: &SessionId) -> Result<Vec<u8>> {
        let db_path = self.find_state_db().ok_or_else(|| {
            SessyncError::Tool(format!(
                "Codex state DB not found in {}; cannot read session {}",
                self.codex_root.display(),
                session_id
            ))
        })?;

        let session_id_str = session_id.0.clone();
        let rollout_path_str = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
            let conn = Self::open_db(&db_path)?;
            Self::verify_schema(&conn)?;
            let path: Option<String> = conn
                .query_row(
                    "SELECT rollout_path FROM threads WHERE id = ?1",
                    [&session_id_str],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| {
                    SessyncError::Tool(format!("failed to query rollout_path: {e}"))
                })?;
            Ok(path)
        })
        .await
        .map_err(|e| SessyncError::Tool(format!("blocking task panicked: {e}")))??;

        let rollout_path = rollout_path_str.ok_or_else(|| {
            SessyncError::Tool(format!("session not found in Codex DB: {session_id}"))
        })?;

        tokio::fs::read(&rollout_path).await.map_err(|e| {
            SessyncError::Tool(format!(
                "failed to read rollout file {rollout_path}: {e}"
            ))
        })
    }

    async fn write_session(
        &self,
        session_id: &SessionId,
        target_cwd: &str,
        raw: &[u8],
    ) -> Result<PathBuf> {
        // --- Step 1: find (or fail gracefully on) the state DB ---
        let db_path = self.find_state_db().ok_or_else(|| {
            SessyncError::Tool(format!(
                "Codex state DB not found in {}; cannot write session",
                self.codex_root.display()
            ))
        })?;

        // --- Step 2: backup before ANY mutation ---
        Self::backup_db(&db_path)?;

        // --- Step 3: compute rollout path ---
        let now = Utc::now();
        let rollout_path = self.rollout_path_for(session_id, &now);

        // --- Step 4: write JSONL atomically ---
        // Codex's rollout file embeds the original cwd in the first line's
        // session_meta event. Codex.app/CLI reads the rollout file at startup
        // and reconciles SQLite from it — meaning whatever we INSERT into
        // SQLite gets overwritten by the rollout's cwd. So we have to rewrite
        // the rollout's session_meta.payload.cwd to match target_cwd, otherwise
        // the synced session shows up under the original (cross-machine) path
        // group in the Codex.app sidebar (where it's invisible because that
        // path doesn't exist on this Mac).
        let rewritten = rewrite_rollout_cwd(raw, target_cwd);
        let raw_to_write: &[u8] = rewritten.as_deref().unwrap_or(raw);

        let rollout_dir = rollout_path.parent().expect("rollout path always has parent");
        tokio::fs::create_dir_all(rollout_dir).await.map_err(|e| {
            SessyncError::Tool(format!(
                "failed to create sessions dir {}: {e}",
                rollout_dir.display()
            ))
        })?;

        let tmp_path = rollout_path.with_extension("jsonl.tmp");
        tokio::fs::write(&tmp_path, raw_to_write).await.map_err(|e| {
            SessyncError::Tool(format!("failed to write tmp rollout file: {e}"))
        })?;
        tokio::fs::rename(&tmp_path, &rollout_path).await.map_err(|e| {
            SessyncError::Tool(format!("failed to rename rollout file into place: {e}"))
        })?;

        // --- Step 5: upsert the threads row ---
        let first_user_message = Self::extract_first_user_message(raw);
        let first_100: String = first_user_message.chars().take(100).collect();

        // Codex rejects sessions whose model_provider it doesn't recognise
        // ("Model provider `unknown` not found"). Read the real value from
        // the rollout's session_meta payload. Fall back to "openai" — the
        // default provider for all current Codex deployments. We never write
        // "unknown" again.
        let model_provider = extract_rollout_model_provider(raw)
            .unwrap_or_else(|| "openai".to_string());

        let session_id_str = session_id.0.clone();
        let rollout_path_str = rollout_path.to_string_lossy().into_owned();
        let cwd = target_cwd.to_string();
        let now_secs = now.timestamp();
        let now_ms = now.timestamp_millis();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = Self::open_db(&db_path)?;
            Self::verify_schema(&conn)?;

            // Use INSERT OR REPLACE to be idempotent on UUID collision.
            // We provide defaults for all NOT NULL columns that Codex requires.
            conn.execute(
                "INSERT INTO threads (
                    id, rollout_path, created_at, updated_at,
                    source, model_provider,
                    cwd, title,
                    sandbox_policy, approval_mode,
                    tokens_used, has_user_event,
                    first_user_message,
                    archived,
                    created_at_ms, updated_at_ms
                ) VALUES (
                    ?1, ?2, ?3, ?3,
                    'sessync', ?8,
                    ?4, ?5,
                    'default', 'suggest',
                    0, 0,
                    ?6,
                    0,
                    ?7, ?7
                )
                ON CONFLICT(id) DO UPDATE SET
                    rollout_path   = excluded.rollout_path,
                    updated_at     = excluded.updated_at,
                    updated_at_ms  = excluded.updated_at_ms,
                    cwd            = excluded.cwd,
                    first_user_message = excluded.first_user_message,
                    source         = excluded.source,
                    model_provider = excluded.model_provider",
                rusqlite::params![
                    session_id_str,
                    rollout_path_str,
                    now_secs,
                    cwd,
                    first_100,
                    first_user_message,
                    now_ms,
                    model_provider,
                ],
            )
            .map_err(|e| SessyncError::Tool(format!("failed to upsert thread row: {e}")))?;

            Ok(())
        })
        .await
        .map_err(|e| SessyncError::Tool(format!("blocking task panicked: {e}")))??;

        Ok(rollout_path)
    }

    fn project_key_for(&self, cwd: &str) -> ProjectKey {
        path_codec::project_key_for_cwd(cwd)
    }

    fn launch_resume(&self, session_id: &SessionId) -> std::io::Result<std::process::Child> {
        // Per research: `codex resume <uuid>` (no `--`, positional arg).
        // Also try the macOS desktop app binary path as a fallback.
        let binary = self.codex_binary_path();
        std::process::Command::new(binary)
            .arg("resume")
            .arg(&session_id.0)
            .spawn()
    }

    fn launch_binary_on_path(&self) -> bool {
        // Returns true if either `codex` is on PATH or the macOS desktop app bundle exists.
        std::process::Command::new("codex")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .map_or(false, |o| o.status.success())
            || std::path::Path::new("/Applications/Codex.app/Contents/Resources/codex").exists()
    }

    fn launch_binary_name(&self) -> &'static str {
        "codex"
    }
}

impl CodexAdapter {
    /// Resolve the codex binary to invoke.
    ///
    /// On macOS, Codex ships as a desktop app at
    /// `/Applications/Codex.app/Contents/Resources/codex`.
    /// If `codex` is not on PATH, fall back to the desktop app bundle location.
    fn codex_binary_path(&self) -> &'static str {
        const MACOS_PATH: &str = "/Applications/Codex.app/Contents/Resources/codex";
        if std::path::Path::new(MACOS_PATH).exists()
            && !std::process::Command::new("codex")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .map_or(false, |o| o.status.success())
        {
            return MACOS_PATH;
        }
        "codex"
    }
}

#[cfg(test)]
mod rewrite_tests {
    use super::rewrite_rollout_cwd;

    #[test]
    fn rewrites_cwd_in_session_meta_first_line() {
        let raw = br#"{"type":"session_meta","payload":{"id":"abc","cwd":"/old/path","other":"keep"}}
{"timestamp":"x","type":"event_msg"}
"#;
        let out = rewrite_rollout_cwd(raw, "/new/path").unwrap();
        // The cwd should be the new value.
        let nl = out.iter().position(|&b| b == b'\n').unwrap();
        let first = std::str::from_utf8(&out[..nl]).unwrap();
        assert!(first.contains(r#""cwd":"/new/path""#), "got: {first}");
        // Other fields should survive.
        assert!(first.contains(r#""id":"abc""#));
        assert!(first.contains(r#""other":"keep""#));
        // Rest of the file should be byte-identical.
        let body_start = raw.iter().position(|&b| b == b'\n').unwrap();
        assert_eq!(&out[nl..], &raw[body_start..]);
    }

    #[test]
    fn returns_none_when_not_session_meta() {
        let raw = br#"{"type":"event_msg","payload":{"x":1}}
"#;
        assert!(rewrite_rollout_cwd(raw, "/new").is_none());
    }

    #[test]
    fn returns_none_when_no_cwd_field() {
        let raw = br#"{"type":"session_meta","payload":{"id":"x"}}
{"a":1}"#;
        assert!(rewrite_rollout_cwd(raw, "/new").is_none());
    }

    #[test]
    fn returns_none_on_malformed_first_line() {
        let raw = b"not json\nrest";
        assert!(rewrite_rollout_cwd(raw, "/new").is_none());
    }

    #[test]
    fn returns_none_on_no_newline() {
        let raw = br#"{"type":"session_meta","payload":{"cwd":"/x"}}"#;
        assert!(rewrite_rollout_cwd(raw, "/new").is_none());
    }
}

#[cfg(test)]
mod extract_meta_tests {
    use super::extract_rollout_model_provider;

    #[test]
    fn extracts_model_provider_from_session_meta() {
        let raw = br#"{"type":"session_meta","payload":{"id":"abc","cwd":"/x","model_provider":"openai"}}
rest"#;
        assert_eq!(extract_rollout_model_provider(raw).unwrap(), "openai");
    }

    #[test]
    fn extracts_model_provider_when_other_provider() {
        let raw = br#"{"type":"session_meta","payload":{"model_provider":"anthropic","cwd":"/x"}}
"#;
        assert_eq!(extract_rollout_model_provider(raw).unwrap(), "anthropic");
    }

    #[test]
    fn returns_none_when_field_missing() {
        let raw = br#"{"type":"session_meta","payload":{"id":"abc","cwd":"/x"}}
"#;
        assert!(extract_rollout_model_provider(raw).is_none());
    }

    #[test]
    fn returns_none_when_not_session_meta() {
        let raw = br#"{"type":"event_msg","payload":{"model_provider":"openai"}}
"#;
        assert!(extract_rollout_model_provider(raw).is_none());
    }

    #[test]
    fn returns_none_when_no_newline() {
        let raw = br#"{"type":"session_meta","payload":{"model_provider":"openai"}}"#;
        assert!(extract_rollout_model_provider(raw).is_none());
    }
}
