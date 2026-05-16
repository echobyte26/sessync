use super::path_codec;
use super::tool::{LocalSession, ToolAdapter};
use crate::error::{Result, SessyncError};
use crate::types::{ProjectKey, SessionId, SessionMeta};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::io::AsyncBufReadExt;

/// Maximum number of lines to scan in a jsonl file when looking for the `cwd` field.
/// Line 1 is typically `{"type":"permission-mode","sessionId":"…"}` (no cwd).
/// Real cwd usually appears in line 2 or 3. 50 is a generous bound.
const CWD_SCAN_LINES: usize = 50;

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

                // Read the real cwd from the jsonl content (preserves dots and
                // dashes that the dir-name encoding loses).  Falls back to the
                // directory-name decode if no cwd field is found.
                let source_cwd = match cwd_from_jsonl(&path).await {
                    Some(cwd) => cwd,
                    None => {
                        tracing::warn!(
                            path = %path.display(),
                            fallback_cwd = %dir_decoded_cwd,
                            "no cwd field found in first {} lines of jsonl; \
                             falling back to dir-name decode (lossy)",
                            CWD_SCAN_LINES
                        );
                        dir_decoded_cwd.clone()
                    }
                };
                let project_key = path_codec::project_key_for_cwd(&source_cwd);

                let preview = first_user_message_preview(&path).await.unwrap_or_default();

                out.push(LocalSession {
                    meta: SessionMeta {
                        schema_version: 1,
                        session_id,
                        project_key,
                        source_cwd,
                        source_hostname: hostname(),
                        modified_at: metadata
                            .modified()
                            .map(chrono::DateTime::<chrono::Utc>::from)
                            .unwrap_or_else(|_| chrono::Utc::now()),
                        byte_size: metadata.len(),
                        preview,
                    },
                    local_path: path,
                });
            }
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
}

fn decode_project_dir(encoded: &str) -> String {
    // Reverse of `path_codec::encode_cwd` — replace dashes with slashes.
    // Note: this is lossy if any path component itself contained dashes;
    // Claude Code accepts the lossiness, so do we.
    encoded.replace('-', "/")
}

/// Read the first `CWD_SCAN_LINES` lines of a jsonl session file and return the
/// value of the first `"cwd"` field found.
///
/// Claude Code's line 1 is typically `{"type":"permission-mode","sessionId":"…"}`
/// (no cwd). The real cwd appears in line 2+ in `attachment`, `assistant`, `user`,
/// or `system` events.
///
/// Returns `None` when:
/// - the file cannot be opened (I/O error — logged at warn level)
/// - no line in the first `CWD_SCAN_LINES` carries a `"cwd"` string field
///
/// This function is O(first-N-lines) and does not read the whole file, so it has
/// negligible cost even for large session files.
async fn cwd_from_jsonl(path: &Path) -> Option<String> {
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), err = %e, "cannot open jsonl to read cwd");
            return None;
        }
    };

    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut scanned = 0usize;

    while scanned < CWD_SCAN_LINES {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break, // EOF
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "read error while scanning cwd");
                break;
            }
        };
        scanned += 1;

        // Skip lines that are too large to parse cheaply.
        if line.len() > MAX_LINE_BYTES {
            continue;
        }

        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                if !cwd.is_empty() {
                    return Some(cwd.to_string());
                }
            }
        }
    }

    None
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
}
