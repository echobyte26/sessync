use super::path_codec;
use super::tool::{LocalSession, ToolAdapter};
use crate::error::{Result, SessyncError};
use crate::types::{ProjectKey, SessionId, SessionMeta};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::io::AsyncBufReadExt;

/// Maximum number of lines to scan in a jsonl file when looking for the `cwd`
/// field and for ghost-session detection (presence of a `type: user` event).
///
/// Real cwd usually appears in line 2-3 (attachment / first user event). 50
/// covers the vast majority. v0.9.3 bumped to 500: some sessions log many
/// `progress` / `file-history-snapshot` events at the start before the first
/// user message attaches cwd; 50 was too conservative and triggered the
/// lossy dir-name decode fallback for those sessions, splitting one logical
/// project into two project_keys in the picker. Cost: ~5 KB extra read per
/// jsonl in the unlucky cases (most files fall out at the first cwd hit
/// within the first 10 lines).
const CWD_SCAN_LINES: usize = 500;

static HOSTNAME: OnceLock<String> = OnceLock::new();

pub struct ClaudeCodeAdapter {
    /// Root directory of Claude Code projects (default `~/.claude/projects`).
    root: PathBuf,
}

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeCodeAdapter {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Self {
            root: PathBuf::from(home).join(".claude/projects"),
        }
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl ToolAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    async fn list_local_sessions(&self) -> Result<Vec<LocalSession>> {
        let mut out = vec![];
        if !self.root.exists() {
            return Ok(out);
        }

        // Layer 1: iterate project dirs — skip any we can't read.
        let mut project_dirs = match tokio::fs::read_dir(&self.root).await {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(path = %self.root.display(), err = %e, "cannot read Claude projects root; returning empty session list");
                return Ok(out);
            }
        };

        loop {
            let entry = match project_dirs.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(err = %e, "error iterating project dirs entry; skipping");
                    continue;
                }
            };

            // Skip non-directories (files, symlinks to non-dirs, etc.) gracefully.
            let is_dir = match entry.file_type().await {
                Ok(ft) => ft.is_dir(),
                Err(e) => {
                    tracing::warn!(path = %entry.path().display(), err = %e, "cannot stat project dir entry; skipping");
                    continue;
                }
            };
            if !is_dir {
                continue;
            }

            let project_dir_name = entry.file_name().to_string_lossy().into_owned();
            // source_cwd and project_key are determined per-session below (after
            // reading the cwd field from the jsonl content). The dir-decode is kept
            // as a fallback for session files that have no cwd field in their first
            // CWD_SCAN_LINES lines.
            let dir_decoded_cwd = decode_project_dir(&project_dir_name);

            // Layer 2: iterate jsonl files within the project dir — skip unreadable ones.
            let mut files = match tokio::fs::read_dir(entry.path()).await {
                Ok(rd) => rd,
                Err(e) => {
                    tracing::warn!(path = %entry.path().display(), err = %e, "cannot read project dir; skipping");
                    continue;
                }
            };

            // v0.9.4: dir-canonical-from-sibling.
            // Same `~/.claude/projects/-Users-foo-bar/` directory always
            // corresponds to the same real cwd. If even ONE jsonl in this
            // directory has a `cwd` field within CWD_SCAN_LINES, that value
            // is the canonical cwd for the WHOLE directory — all other
            // sessions in the same dir inherit it, regardless of whether
            // their own scan succeeded. This collapses what v0.7.3 would
            // split into two project_keys (correct cwd vs lossy dir-decode)
            // into a single picker entry.
            //
            // Pass 1: collect every session's partial info + accumulate
            // canonical_cwd from the first scan hit.
            // Pass 2: assign canonical_cwd (or dir-decode fallback if no
            // hit) and emit LocalSession entries.
            struct PendingSession {
                path: std::path::PathBuf,
                session_id: SessionId,
                metadata: std::fs::Metadata,
                scanned_cwd: Option<String>,
                has_user_message: bool,
                preview: String,
            }
            let mut pending: Vec<PendingSession> = Vec::new();
            let mut dir_canonical_cwd: Option<String> = None;

            loop {
                let f = match files.next_entry().await {
                    Ok(Some(e)) => e,
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(err = %e, "error iterating project dir files entry; skipping");
                        continue;
                    }
                };

                let path = f.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }

                let session_id = match path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| SessionId(s.to_string()))
                {
                    Some(id) => id,
                    None => {
                        tracing::warn!(path = %path.display(), "bad jsonl filename; skipping");
                        continue;
                    }
                };

                let metadata = match tokio::fs::metadata(&path).await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(path = %path.display(), err = %e, "cannot stat session file; skipping");
                        continue;
                    }
                };

                let ScanResult { cwd: scanned_cwd, has_user_message } =
                    scan_jsonl(&path).await;

                if dir_canonical_cwd.is_none() {
                    if let Some(ref cwd) = scanned_cwd {
                        dir_canonical_cwd = Some(cwd.clone());
                    }
                }

                let preview = first_user_message_preview(&path).await.unwrap_or_default();

                pending.push(PendingSession {
                    path,
                    session_id,
                    metadata,
                    scanned_cwd,
                    has_user_message,
                    preview,
                });
            }

            // Single fallback warning per directory (instead of per-session
            // spam in the old code path).
            if dir_canonical_cwd.is_none() && !pending.is_empty() {
                tracing::warn!(
                    dir = %project_dir_name,
                    fallback_cwd = %dir_decoded_cwd,
                    pending_sessions = pending.len(),
                    "no jsonl in this project dir has a cwd field in first {} lines; \
                     using lossy dir-name decode for all {} session(s)",
                    CWD_SCAN_LINES,
                    pending.len()
                );
            }

            for p in pending {
                // Priority: this session's own scan (most direct), else the
                // canonical from any sibling in the same dir, else the lossy
                // dir-decode. Sibling and self should always agree when both
                // succeed — they refer to the same real project — but we
                // prefer self for the rare case where a single dir was
                // assembled from sessions belonging to different projects
                // (which shouldn't happen but be conservative).
                let source_cwd = p
                    .scanned_cwd
                    .clone()
                    .or_else(|| dir_canonical_cwd.clone())
                    .unwrap_or_else(|| dir_decoded_cwd.clone());
                let project_key = path_codec::project_key_for_cwd(&source_cwd);

                out.push(LocalSession {
                    meta: SessionMeta {
                        schema_version: 1,
                        session_id: p.session_id,
                        project_key,
                        source_cwd,
                        source_hostname: hostname(),
                        modified_at: p.metadata
                            .modified()
                            .map(chrono::DateTime::<chrono::Utc>::from)
                            .unwrap_or_else(|_| chrono::Utc::now()),
                        byte_size: p.metadata.len(),
                        preview: p.preview,
                        has_user_message: p.has_user_message,
                    },
                    local_path: p.path,
                });
            }
        }

        // v0.9.5: dedupe local sessions by session_id. The same UUID can show up
        // in multiple `~/.claude/projects/<dir>/` directories — usually because
        // a past pull with a wrong cwd wrote the session into a different
        // project dir, leaving the original in place. Without dedupe, push
        // uploads each copy under a separate project_key (massive OSS dup +
        // bandwidth bloat: user logs showed same UUID pushed 3x in one cycle
        // at 250KB / 8MB / 12MB sizes). Keep the freshest mtime — that's the
        // copy most likely reflecting current activity.
        out.sort_by(|a, b| {
            a.meta
                .session_id
                .0
                .cmp(&b.meta.session_id.0)
                .then(b.meta.modified_at.cmp(&a.meta.modified_at))
        });
        let before = out.len();
        out.dedup_by(|a, b| a.meta.session_id.0 == b.meta.session_id.0);
        if out.len() < before {
            tracing::warn!(
                "deduped {} duplicate local jsonl(s) (same session_id in multiple project dirs); \
                 kept the freshest mtime per UUID. \
                 To clean the older copies from disk, identify them with `find ~/.claude/projects -name '<uuid>.jsonl'` and rm the stale ones.",
                before - out.len()
            );
        }
        Ok(out)
    }

    async fn read_session(&self, session_id: &SessionId) -> Result<Vec<u8>> {
        // Walk all project dirs; pick first match, warn if duplicates exist.
        let mut found: Option<(PathBuf, Vec<u8>)> = None;
        let mut project_dirs = tokio::fs::read_dir(&self.root).await?;
        while let Some(pd) = project_dirs.next_entry().await? {
            let candidate = pd.path().join(format!("{}.jsonl", session_id.0));
            match tokio::fs::read(&candidate).await {
                Ok(bytes) => {
                    if let Some((prev_path, _)) = &found {
                        tracing::warn!(
                            session_id = %session_id,
                            kept = %prev_path.display(),
                            ignored = %candidate.display(),
                            "duplicate session file in another project dir; keeping first match",
                        );
                    } else {
                        found = Some((candidate, bytes));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }
        found
            .map(|(_, b)| b)
            .ok_or_else(|| SessyncError::Tool(format!("session not found locally: {session_id}")))
    }

    async fn write_session(
        &self,
        session_id: &SessionId,
        target_cwd: &str,
        raw: &[u8],
        source_modified_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<PathBuf> {
        let dir_name = path_codec::encode_cwd(target_cwd);
        let dir = self.root.join(dir_name);
        tokio::fs::create_dir_all(&dir).await?;
        let final_path = dir.join(format!("{}.jsonl", session_id.0));

        // Atomic write: tmp + rename. POSIX rename is atomic on the same filesystem,
        // so a crash mid-write leaves either the old file or the new one — never a
        // truncated jsonl that would confuse `claude --resume`.
        let tmp_path = dir.join(format!("{}.jsonl.tmp", session_id.0));
        tokio::fs::write(&tmp_path, raw).await?;
        tokio::fs::rename(&tmp_path, &final_path).await?;

        // Stamp the file's mtime to the source's modified_at. Without this, the
        // file's mtime defaults to "now", and on the next `sessync push` this
        // device looks "newer than remote" → A5 skip fails → re-uploads on every
        // cycle, kicking off a cross-device ping-pong (v0.8.2 fix).
        let ft = filetime::FileTime::from_system_time(source_modified_at.into());
        filetime::set_file_mtime(&final_path, ft).map_err(SessyncError::Io)?;

        Ok(final_path)
    }

    fn project_key_for(&self, cwd: &str) -> ProjectKey {
        path_codec::project_key_for_cwd(cwd)
    }

    fn launch_resume(&self, session_id: &SessionId) -> std::io::Result<std::process::Child> {
        std::process::Command::new("claude")
            .arg("--resume")
            .arg(&session_id.0)
            .spawn()
    }

    fn launch_binary_on_path(&self) -> bool {
        std::process::Command::new("claude")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok()
    }

    fn launch_binary_name(&self) -> &'static str {
        "claude"
    }
}

fn decode_project_dir(encoded: &str) -> String {
    // Reverse of `path_codec::encode_cwd` — replace dashes with slashes.
    // Note: this is lossy if any path component itself contained dashes;
    // Claude Code accepts the lossiness, so do we.
    encoded.replace('-', "/")
}

/// Bytes to read from the tail of the jsonl when looking for the most recent cwd.
/// 64 KB is enough to capture several full events (typical event ~1-4 KB) while
/// keeping the I/O bounded.
const CWD_TAIL_SCAN_BYTES: u64 = 64 * 1024;

/// Result of scanning a jsonl session file.
struct ScanResult {
    /// Best-effort source cwd. Priority:
    ///   1. The cwd from the LAST cwd-bearing line (reflects current activity —
    ///      handles migrated sessions where head and tail disagree).
    ///   2. The cwd from the FIRST cwd-bearing line in the head window.
    ///   3. `None` if neither was found.
    cwd: Option<String>,
    /// `true` if at least one line has `"type":"user"`; `false` for ghost sessions
    /// (plugin hook side-effects that contain only `type:progress` / `type:file-history-snapshot`).
    has_user_message: bool,
}

/// Scan a jsonl session file and extract:
///
/// - The session's current source_cwd (tail-preferred, head-fallback — see `ScanResult::cwd`).
/// - Whether any line has `"type":"user"` (for ghost-session detection).
///
/// Claude Code's line 1 is typically `{"type":"permission-mode","sessionId":"…"}`
/// (no cwd). The real cwd appears in line 2+ in `attachment`, `assistant`, `user`,
/// or `system` events.
///
/// **Tail bias (v0.9.6)**: A session can be resumed in a different cwd via
/// `claude --resume <id>`. Claude Code then COPIES the jsonl into the new cwd's
/// project dir but **keeps the original cwd value in the existing lines** — only
/// NEW events written after the resume carry the new cwd. The old head-only scan
/// reported the creation cwd forever, breaking cross-device sync (e.g. mini's
/// session at sessync/ would push to OSS under azoth/ because head still said azoth).
/// We now scan the tail and prefer its cwd, so the session syncs under its
/// **current** project.
///
/// Ghost sessions (created by plugin hooks like `superpowers@claude-plugins-official`)
/// contain only `type:progress` and `type:file-history-snapshot` lines — no user
/// message events — so `has_user_message` will be `false` for them.
///
/// Cost: O(first-N-lines) head scan + one seek + 64KB read for tail. Bounded.
async fn scan_jsonl(path: &Path) -> ScanResult {
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), err = %e, "cannot open jsonl to scan");
            return ScanResult { cwd: None, has_user_message: false };
        }
    };

    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut scanned = 0usize;
    let mut head_cwd: Option<String> = None;
    let mut has_user_message = false;

    while scanned < CWD_SCAN_LINES {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break, // EOF
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "read error while scanning jsonl");
                break;
            }
        };
        scanned += 1;

        // Skip lines that are too large to parse cheaply.
        if line.len() > MAX_LINE_BYTES {
            continue;
        }

        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            // Extract cwd from the first line that has it.
            if head_cwd.is_none() {
                if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                    if !cwd.is_empty() {
                        head_cwd = Some(cwd.to_string());
                    }
                }
            }

            // Ghost detection: any `"type":"user"` event makes this a real session.
            if !has_user_message
                && v.get("type").and_then(|t| t.as_str()) == Some("user")
            {
                has_user_message = true;
            }

            // Early exit once we have everything we need within the scan window.
            if head_cwd.is_some() && has_user_message {
                break;
            }
        }
    }

    // Tail scan — find the most recent cwd. Prefer over head_cwd.
    let tail_cwd = tail_cwd_scan(path).await;
    let cwd = tail_cwd.or(head_cwd);

    ScanResult { cwd, has_user_message }
}

/// Read the trailing `CWD_TAIL_SCAN_BYTES` of a jsonl file and return the cwd from
/// the LAST line that has one. Returns `None` if no cwd is found in the tail
/// window, or on I/O error.
///
/// Carefully discards partial lines at both ends of the window:
///   - The first line (if offset > 0) is sliced mid-line — skip it.
///   - The last line (if file does not end in `\n`) is mid-write — skip it.
async fn tail_cwd_scan(path: &Path) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut file = tokio::fs::File::open(path).await.ok()?;
    let len = file.metadata().await.ok()?.len();
    if len == 0 {
        return None;
    }

    let to_read = CWD_TAIL_SCAN_BYTES.min(len);
    let offset = len - to_read;
    file.seek(std::io::SeekFrom::Start(offset)).await.ok()?;

    let mut buf = vec![0u8; to_read as usize];
    if file.read_exact(&mut buf).await.is_err() {
        return None;
    }

    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.split('\n').collect();

    // Drop partial first line if we seeked into the middle of one.
    if offset > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    // Drop trailing partial line (mid-write). `split('\n')` always produces
    // a trailing empty element when input ends with '\n' — that's also dropped here.
    if !lines.is_empty() {
        lines.pop();
    }

    for line in lines.iter().rev() {
        if line.len() > MAX_LINE_BYTES {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                if !cwd.is_empty() {
                    return Some(cwd.to_string());
                }
            }
        }
    }
    None
}

/// Read the first `CWD_SCAN_LINES` lines of a jsonl session file and return the
/// value of the first `"cwd"` field found.
///
/// Kept for backward compat with tests; delegates to `scan_jsonl`.
#[cfg(test)]
async fn cwd_from_jsonl(path: &Path) -> Option<String> {
    scan_jsonl(path).await.cwd
}

fn hostname() -> String {
    HOSTNAME
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

/// Lines larger than this are skipped without JSON parsing.
/// Prevents memory spikes when a user pasted a huge blob into Claude.
const MAX_LINE_BYTES: usize = 1_048_576; // 1 MiB

async fn first_user_message_preview(path: &Path) -> Result<String> {
    let f = tokio::fs::File::open(path).await?;
    let mut lines = tokio::io::BufReader::new(f).lines();
    while let Some(line) = lines.next_line().await? {
        if line.len() > MAX_LINE_BYTES {
            // Skip lines that are too large to be human-pasted prose.
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("user") {
                if let Some(content) = v.pointer("/message/content").and_then(|c| c.as_str()) {
                    let mut s = content.to_string();
                    if s.chars().count() > 200 {
                        s = s.chars().take(197).collect::<String>() + "...";
                    }
                    return Ok(s);
                }
            }
        }
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Helper: write lines to a temp jsonl file and return it.
    fn make_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        for line in lines {
            writeln!(f, "{line}").expect("write");
        }
        f
    }

    // ── scan_jsonl ghost-detection tests ─────────────────────────────────────

    /// A jsonl file with at least one `"type":"user"` event → has_user_message=true.
    #[tokio::test]
    async fn list_local_sessions_marks_jsonl_with_user_message_as_real() {
        let f = make_jsonl(&[
            r#"{"type":"permission-mode","sessionId":"sess-real"}"#,
            r#"{"type":"attachment","cwd":"/tmp/proj","sessionId":"sess-real"}"#,
            r#"{"type":"user","message":{"role":"user","content":"hello Claude"},"cwd":"/tmp/proj"}"#,
        ]);
        let result = scan_jsonl(f.path()).await;
        assert!(
            result.has_user_message,
            "jsonl with a type:user line must be marked as a real session (has_user_message=true)"
        );
    }

    /// A jsonl file with only plugin-hook events (progress / file-history-snapshot) and
    /// no `"type":"user"` event → has_user_message=false (ghost session).
    #[tokio::test]
    async fn list_local_sessions_marks_jsonl_with_only_hooks_as_ghost() {
        let f = make_jsonl(&[
            r#"{"type":"permission-mode","sessionId":"sess-ghost"}"#,
            r#"{"type":"progress","message":"SessionStart hook fired","sessionId":"sess-ghost"}"#,
            r#"{"type":"file-history-snapshot","files":[],"sessionId":"sess-ghost"}"#,
        ]);
        let result = scan_jsonl(f.path()).await;
        assert!(
            !result.has_user_message,
            "jsonl with only plugin-hook events (no type:user) must be marked as ghost (has_user_message=false)"
        );
    }

    // ── cwd_from_jsonl tests ───────────────────────────────────────────────────

    /// Line 1 has no cwd (permission-mode), line 2 is an attachment with cwd →
    /// source_cwd must equal the cwd value from line 2.
    #[tokio::test]
    async fn source_cwd_read_from_jsonl_cwd_field() {
        let f = make_jsonl(&[
            r#"{"type":"permission-mode","sessionId":"abc-123"}"#,
            r#"{"type":"attachment","cwd":"/Users/sakuragi/Project/LLMProjects/dify-1.11.4","sessionId":"abc-123"}"#,
        ]);
        let result = cwd_from_jsonl(f.path()).await;
        assert_eq!(
            result.as_deref(),
            Some("/Users/sakuragi/Project/LLMProjects/dify-1.11.4"),
            "cwd must be read from line 2 attachment event"
        );
    }

    /// When no cwd field appears in the first CWD_SCAN_LINES lines, cwd_from_jsonl
    /// returns None (fallback to dir-name decode in the caller).
    #[tokio::test]
    async fn source_cwd_falls_back_when_no_cwd_in_jsonl() {
        // All lines have no cwd field.
        let lines: Vec<String> = (0..CWD_SCAN_LINES + 5)
            .map(|i| format!(r#"{{"type":"system","index":{i}}}"#))
            .collect();
        let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let f = make_jsonl(&line_refs);

        let result = cwd_from_jsonl(f.path()).await;
        assert!(
            result.is_none(),
            "cwd_from_jsonl must return None when no cwd in first {CWD_SCAN_LINES} lines"
        );
    }

    /// The cwd value read from the jsonl preserves dots and dashes in the
    /// original path — unlike the lossy dir-name decode.
    #[tokio::test]
    async fn source_cwd_preserves_dots_and_dashes_in_original_path() {
        // Path has both dots (in .claude-mem) and dashes (in bar-baz).
        let original_cwd = "/Users/alice/.foo-plugin/bar-baz/my.project";
        let line = format!(r#"{{"type":"attachment","cwd":"{original_cwd}"}}"#);
        let f = make_jsonl(&[
            r#"{"type":"permission-mode","sessionId":"sess-1"}"#,
            &line,
        ]);

        let result = cwd_from_jsonl(f.path()).await;
        assert_eq!(
            result.as_deref(),
            Some(original_cwd),
            "dots and dashes in cwd must be preserved exactly"
        );
    }

    /// cwd_from_jsonl returns None for a file that cannot be opened.
    #[tokio::test]
    async fn source_cwd_returns_none_for_missing_file() {
        let result = cwd_from_jsonl(std::path::Path::new("/nonexistent/no/such/file.jsonl")).await;
        assert!(result.is_none(), "missing file must produce None, not a panic");
    }

    /// cwd_from_jsonl skips lines that have no cwd and picks the first one that does.
    #[tokio::test]
    async fn source_cwd_skips_lines_without_cwd_field() {
        let f = make_jsonl(&[
            r#"{"type":"system","msg":"no cwd here"}"#,
            r#"{"type":"assistant","text":"also no cwd"}"#,
            r#"{"type":"user","cwd":"/Users/bob/projects/thing","msg":"finally"}"#,
        ]);

        let result = cwd_from_jsonl(f.path()).await;
        assert_eq!(
            result.as_deref(),
            Some("/Users/bob/projects/thing"),
            "must pick first line with a cwd field"
        );
    }

    // ── tail-cwd-wins-over-head tests (v0.9.6) ─────────────────────────────────

    /// Migrated session: jsonl was created in project A, then the user did
    /// `claude --resume <id>` in project B, after which Claude Code appended
    /// new lines with cwd=B but kept the original head lines with cwd=A.
    /// scan_jsonl must report cwd=B (current), not cwd=A (creation).
    ///
    /// This is the bug that caused the v0.9.5 user complaint "current session
    /// is missing on pro when looking under sessync project" — the session
    /// was on OSS under the OLD project_key (cwd=A) because head said A.
    #[tokio::test]
    async fn source_cwd_prefers_tail_for_migrated_session() {
        let f = make_jsonl(&[
            r#"{"type":"permission-mode","sessionId":"migrated"}"#,
            r#"{"type":"attachment","cwd":"/Users/jameschen/Project/ai-coding-project/azoth","sessionId":"migrated"}"#,
            r#"{"type":"user","cwd":"/Users/jameschen/Project/ai-coding-project/azoth","msg":"start in azoth"}"#,
            // ... session ran in azoth for a while ...
            // Then user did `claude --resume migrated` in sessync repo.
            // From here on, new lines carry cwd=sessync.
            r#"{"type":"user","cwd":"/Users/jameschen/Project/ai-coding-project/sessync","msg":"continued in sessync"}"#,
            r#"{"type":"assistant","cwd":"/Users/jameschen/Project/ai-coding-project/sessync","text":"hi from sessync"}"#,
        ]);

        let result = scan_jsonl(f.path()).await;
        assert_eq!(
            result.cwd.as_deref(),
            Some("/Users/jameschen/Project/ai-coding-project/sessync"),
            "scan_jsonl must report the TAIL cwd (current project) for a migrated session, not the head cwd"
        );
        assert!(result.has_user_message, "real session must be marked has_user_message=true");
    }

    /// Non-migrated session: every line has the same cwd. Tail and head agree.
    /// Result must equal that single cwd. (Sanity check — tail bias must not
    /// regress the common case.)
    #[tokio::test]
    async fn source_cwd_unchanged_when_head_and_tail_agree() {
        let f = make_jsonl(&[
            r#"{"type":"permission-mode","sessionId":"normal"}"#,
            r#"{"type":"user","cwd":"/Users/alice/repo","msg":"x"}"#,
            r#"{"type":"assistant","cwd":"/Users/alice/repo","text":"y"}"#,
            r#"{"type":"user","cwd":"/Users/alice/repo","msg":"z"}"#,
        ]);
        let result = scan_jsonl(f.path()).await;
        assert_eq!(result.cwd.as_deref(), Some("/Users/alice/repo"));
    }

    /// File ends mid-line (no trailing newline — Claude Code is still writing).
    /// The incomplete last line must be discarded by tail_cwd_scan so we don't
    /// try to parse a truncated JSON object. Tail must skip the partial line
    /// and use the last COMPLETE line.
    #[tokio::test]
    async fn source_cwd_tail_skips_mid_write_partial_line() {
        // Manually build a file where the last line is a truncated JSON fragment.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut content = String::new();
        content.push_str(r#"{"type":"permission-mode","sessionId":"midwrite"}"#);
        content.push('\n');
        content.push_str(r#"{"type":"user","cwd":"/Users/alice/old","msg":"head"}"#);
        content.push('\n');
        content.push_str(r#"{"type":"user","cwd":"/Users/alice/new","msg":"complete tail"}"#);
        content.push('\n');
        // Append a partial line — simulates Claude Code mid-write. No trailing \n.
        content.push_str(r#"{"type":"assistant","cwd":"/Users/alice/should-be-ignored","text":"truncated"#);
        // (deliberately no closing quote / brace / newline)
        std::fs::write(tmp.path(), &content).unwrap();

        let result = scan_jsonl(tmp.path()).await;
        assert_eq!(
            result.cwd.as_deref(),
            Some("/Users/alice/new"),
            "tail scan must skip mid-write partial line and use the last complete line's cwd"
        );
    }

    /// File has cwd in head only — no cwd in the tail window (file is short
    /// enough that everything fits, OR tail just doesn't have cwd events).
    /// Fall back to head cwd.
    #[tokio::test]
    async fn source_cwd_falls_back_to_head_when_tail_has_no_cwd() {
        let f = make_jsonl(&[
            r#"{"type":"permission-mode","sessionId":"head-only"}"#,
            r#"{"type":"user","cwd":"/Users/alice/onlyplace","msg":"with cwd"}"#,
            r#"{"type":"progress","sessionId":"head-only","step":"finalize"}"#,
            r#"{"type":"file-history-snapshot","sessionId":"head-only","files":[]}"#,
        ]);
        let result = scan_jsonl(f.path()).await;
        assert_eq!(
            result.cwd.as_deref(),
            Some("/Users/alice/onlyplace"),
            "when tail has no cwd-bearing line, must fall back to head cwd"
        );
    }

    /// Empty file (zero bytes): both head and tail report None. No panic.
    #[tokio::test]
    async fn source_cwd_handles_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // (file exists but is zero bytes)
        let result = scan_jsonl(tmp.path()).await;
        assert!(result.cwd.is_none(), "empty file must yield None cwd, not panic");
        assert!(!result.has_user_message);
    }

    /// Large file (tail-scan window doesn't cover the whole file): head cwd
    /// is in the first lines, tail cwd is in lines beyond the head-scan
    /// window. Tail must still win.
    #[tokio::test]
    async fn source_cwd_tail_wins_when_far_past_head_window() {
        let mut lines = vec![
            r#"{"type":"permission-mode","sessionId":"big"}"#.to_string(),
            r#"{"type":"user","cwd":"/Users/alice/origin","msg":"head"}"#.to_string(),
        ];
        // Fill with cwd-less lines past the head scan window. Each line ~80 bytes.
        // We want enough total content that the head scan finishes (500 lines)
        // and the tail-cwd line is in the trailing 64 KB.
        for i in 0..600 {
            lines.push(format!(
                r#"{{"type":"system","index":{i},"pad":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}"#
            ));
        }
        // Tail event in last few KB of file.
        lines.push(r#"{"type":"user","cwd":"/Users/alice/latest","msg":"tail"}"#.to_string());
        let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let f = make_jsonl(&line_refs);

        let result = scan_jsonl(f.path()).await;
        assert_eq!(
            result.cwd.as_deref(),
            Some("/Users/alice/latest"),
            "tail cwd must win even when head scan also found a cwd, regardless of distance between them"
        );
    }
}
