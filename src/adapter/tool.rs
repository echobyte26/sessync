use crate::error::Result;
use crate::types::{ProjectKey, SessionId, SessionMeta};
use async_trait::async_trait;
use std::path::PathBuf;

/// A coding agent's session storage adapter.
/// v1 has one impl: `ClaudeCodeAdapter`.
#[async_trait]
pub trait ToolAdapter: Send + Sync {
    /// Tool short name, used as part of OSS key prefix.
    fn name(&self) -> &'static str;

    /// Discover all local sessions across all projects.
    async fn list_local_sessions(&self) -> Result<Vec<LocalSession>>;

    /// Read the raw session file (for upload). Engineer should NOT parse — preserve bytes.
    async fn read_session(&self, session_id: &SessionId) -> Result<Vec<u8>>;

    /// Write a session into the local store, mapped to `target_cwd`.
    /// Implementation handles tool-specific path encoding so `claude --resume` finds it.
    async fn write_session(&self, session_id: &SessionId, target_cwd: &str, raw: &[u8]) -> Result<PathBuf>;

    /// Compute the project key (stable across devices) for a given cwd.
    fn project_key_for(&self, cwd: &str) -> ProjectKey;
}

#[derive(Debug, Clone)]
pub struct LocalSession {
    pub meta: SessionMeta,
    pub local_path: PathBuf,
}
