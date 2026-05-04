use super::path_codec;
use super::tool::{LocalSession, ToolAdapter};
use crate::error::{Result, SessyncError};
use crate::types::{ProjectKey, SessionId, SessionMeta};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::io::AsyncBufReadExt;

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
    fn name(&self) -> &'static str { "claude-code" }

    async fn list_local_sessions(&self) -> Result<Vec<LocalSession>> {
        let mut out = vec![];
        if !self.root.exists() {
            return Ok(out);
        }
        let mut project_dirs = tokio::fs::read_dir(&self.root).await?;
        while let Some(pd) = project_dirs.next_entry().await? {
            if !pd.file_type().await?.is_dir() { continue; }
            let project_dir_name = pd.file_name().to_string_lossy().into_owned();
            let source_cwd = decode_project_dir(&project_dir_name);
            let project_key = path_codec::project_key_for_cwd(&source_cwd);

            let mut files = tokio::fs::read_dir(pd.path()).await?;
            while let Some(f) = files.next_entry().await? {
                let path = f.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") { continue; }
                let session_id = path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| SessionId(s.to_string()))
                    .ok_or_else(|| SessyncError::Tool(format!("bad filename: {}", path.display())))?;

                let metadata = tokio::fs::metadata(&path).await?;
                let preview = first_user_message_preview(&path).await.unwrap_or_default();

                out.push(LocalSession {
                    meta: SessionMeta {
                        schema_version: 1,
                        session_id,
                        project_key: project_key.clone(),
                        source_cwd: source_cwd.clone(),
                        source_hostname: hostname(),
                        modified_at: metadata.modified()
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
}

fn decode_project_dir(encoded: &str) -> String {
    // Reverse of `path_codec::encode_cwd` — replace dashes with slashes.
    // Note: this is lossy if any path component itself contained dashes;
    // Claude Code accepts the lossiness, so do we.
    encoded.replace('-', "/")
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

async fn first_user_message_preview(path: &Path) -> Result<String> {
    let f = tokio::fs::File::open(path).await?;
    let mut lines = tokio::io::BufReader::new(f).lines();
    while let Some(line) = lines.next_line().await? {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("user") {
                if let Some(content) = v.pointer("/message/content").and_then(|c| c.as_str()) {
                    let mut s = content.to_string();
                    if s.chars().count() > 80 {
                        s = s.chars().take(77).collect::<String>() + "...";
                    }
                    return Ok(s);
                }
            }
        }
    }
    Ok(String::new())
}
