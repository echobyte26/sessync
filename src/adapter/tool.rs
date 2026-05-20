use crate::error::Result;
use crate::types::{ProjectKey, SessionId, SessionMeta};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

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
    ///
    /// `source_modified_at` is the original session's `modified_at` from the source
    /// device's `SessionMeta`. The implementation MUST stamp the receiving file's
    /// mtime (and any tool-specific "updated_at" fields) with this value, NOT the
    /// current wall-clock time. Otherwise the next `sessync push` on this device
    /// sees a phantom "local is newer than remote" condition and re-uploads the
    /// session — triggering an infinite cross-device ping-pong (v0.8.2 fix).
    async fn write_session(
        &self,
        session_id: &SessionId,
        target_cwd: &str,
        raw: &[u8],
        source_modified_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<PathBuf>;

    /// Find every local filesystem path where this session_id currently has a
    /// jsonl/session file. Returns empty Vec if the session has never been
    /// written to this device.
    ///
    /// Claude Code can have the same UUID in MULTIPLE `~/.claude/projects/<dir>/`
    /// directories — created by `claude --resume <id>` in a different cwd,
    /// session-migration skills, or cross-device sessync pulls that landed
    /// under another encoded cwd. Codex's SQLite has a PRIMARY KEY on
    /// session_id so this can never return more than one path there.
    ///
    /// Used by `pull` to mirror incoming content to ALL existing local copies
    /// (v0.9.7), keeping the session consistent across the local Claude Code
    /// project dirs it lives in. If empty, caller falls back to
    /// `write_session(target_cwd, ...)` to create the first local copy.
    async fn find_existing_session_paths(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<PathBuf>>;

    /// Write session content directly to a given filesystem path, preserving
    /// `source_modified_at` as the file's mtime. The path must be one returned
    /// from a prior `find_existing_session_paths` or `write_session` call.
    ///
    /// Used by pull's mirror-to-existing path (v0.9.7) — when a session already
    /// has one or more local copies, we update each in place rather than
    /// creating yet another copy at the cwd-encoded dir.
    ///
    /// Codex adapters should return an error (this method is meaningless for
    /// SQLite-backed tools).
    async fn write_session_at_path(
        &self,
        path: &Path,
        raw: &[u8],
        source_modified_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()>;

    /// Compute the project key (stable across devices) for a given cwd.
    fn project_key_for(&self, cwd: &str) -> ProjectKey;

    /// Spawn the tool's CLI to resume the given session.
    /// Returns the spawned child for the caller to wait on (or take stdio over).
    fn launch_resume(&self, session_id: &SessionId) -> std::io::Result<std::process::Child>;

    /// Whether the launch binary is on PATH (for `--no-launch` fallback).
    fn launch_binary_on_path(&self) -> bool;

    /// The actual binary name to invoke (e.g. "claude", "codex").
    /// Used in the "Run: <binary> --resume <id>" hint printed by `sessync resume`.
    /// Distinct from `name()` which is the tool namespace identifier ("claude-code").
    fn launch_binary_name(&self) -> &'static str;
}

#[derive(Debug, Clone)]
pub struct LocalSession {
    pub meta: SessionMeta,
    pub local_path: PathBuf,
}
