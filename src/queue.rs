//! Persistent push queue backed by SQLite.
//!
//! Keeps two tables:
//!   - `pending_pushes`: sessions that failed to upload and need retry.
//!   - `push_outcomes`: bounded log of recent push results (for A4 streak
//!     counting and the future `sessync logs` command).
//!
//! No async, no connection pool, no migration system — just CREATE IF NOT EXISTS.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

const DB_FILE: &str = "queue.db";
/// Maximum rows retained in `push_outcomes` after each write.
const OUTCOME_CAP: usize = 100;

// ── public structs ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct PendingItem {
    pub session_id: String,
    pub enqueued_at: i64,
    pub last_attempt_at: Option<i64>,
    pub attempt_count: i64,
    pub last_error: Option<String>,
}

#[derive(Debug)]
pub struct Outcome {
    pub id: i64,
    pub at: i64,
    pub success: bool,
    pub summary: String,
}

// ── Queue ─────────────────────────────────────────────────────────────────────

pub struct Queue {
    conn: Connection,
}

impl Queue {
    /// Open (or create) the queue at `~/.local/share/sessync/queue.db`.
    pub fn open_default() -> Result<Self> {
        let path = default_queue_path()?;
        Self::open_at(&path)
    }

    /// Open (or create) the queue at an explicit path — used by tests.
    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create queue dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open sqlite db {}", path.display()))?;
        let q = Self { conn };
        q.init_schema()?;
        Ok(q)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pending_pushes (
                session_id      TEXT PRIMARY KEY,
                enqueued_at     INTEGER NOT NULL,
                last_attempt_at INTEGER,
                attempt_count   INTEGER NOT NULL DEFAULT 0,
                last_error      TEXT
            );
            CREATE TABLE IF NOT EXISTS push_outcomes (
                id      INTEGER PRIMARY KEY AUTOINCREMENT,
                at      INTEGER NOT NULL,
                success INTEGER NOT NULL,
                summary TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS session_etags (
                session_id  TEXT PRIMARY KEY,
                etag        TEXT NOT NULL,
                recorded_at INTEGER NOT NULL
            );
            -- v0.9.0: track how many plaintext bytes of each session this device
            -- has already pushed. push.rs uses this to decide between a delta
            -- (local_size > last_pushed_size, append-only assumption) and a full
            -- base rewrite (local_size < last_pushed_size, i.e., truncation).
            CREATE TABLE IF NOT EXISTS session_state (
                session_id       TEXT PRIMARY KEY,
                last_pushed_size INTEGER NOT NULL,
                updated_at       INTEGER NOT NULL
            );
            -- v0.9.3: cache the authoritative source_cwd for each session_id so
            -- list_local_sessions doesn't depend on Claude Code's lossy dir-name
            -- encoding (e.g., `-Users-foo-ai-coding-project-azoth` ambiguously
            -- decodes to either `/Users/foo/ai-coding-project/azoth` or
            -- `/Users/foo/ai/coding/project/azoth`). Populated on pull from the
            -- received meta.source_cwd, and on push when the jsonl scan finds
            -- a `cwd` field. NOT populated from the dir-name fallback path.
            CREATE TABLE IF NOT EXISTS session_cwd (
                session_id  TEXT PRIMARY KEY,
                source_cwd  TEXT NOT NULL,
                updated_at  INTEGER NOT NULL
            );
            -- v0.13.0: separate the etag recorded by PULL (last successful download
            -- snapshot) from the one recorded by PUSH (last successful upload).
            -- Pre-v0.13 both wrote to `session_etags`, so a successful push by THIS
            -- device updated the etag — then the next pull saw 'recorded == remote'
            -- (because our push WAS the remote) and skipped, missing peer content
            -- that landed at an even-later OSS state.  Pull now has its own column.
            CREATE TABLE IF NOT EXISTS session_pull_etag (
                session_id  TEXT PRIMARY KEY,
                etag        TEXT NOT NULL,
                recorded_at INTEGER NOT NULL
            );
            -- v0.13.0: per-(session, device) incremental-pull tracking.  Pull
            -- records the highest delta seq it has reconstructed into local for
            -- each device that contributed to a session.  Next pull lists OSS,
            -- finds deltas with seq > my recorded last_seq for each device, and
            -- only downloads + appends those.  Base etag is tracked separately so
            -- a compaction (base replaced on OSS, deltas folded in) forces a full
            -- reconstruct instead of incremental append.
            CREATE TABLE IF NOT EXISTS session_pull_state (
                session_id     TEXT NOT NULL,
                device_id      TEXT NOT NULL,
                last_seq       INTEGER NOT NULL,
                base_etag      TEXT,
                updated_at     INTEGER NOT NULL,
                PRIMARY KEY (session_id, device_id)
            );",
        )?;
        Ok(())
    }

    /// Add `session_id` to the pending queue. Idempotent: if already present,
    /// does not reset `attempt_count` or `enqueued_at`.
    pub fn enqueue(&self, session_id: &str) -> Result<()> {
        let now = now_epoch();
        self.conn.execute(
            "INSERT INTO pending_pushes (session_id, enqueued_at)
             VALUES (?1, ?2)
             ON CONFLICT(session_id) DO NOTHING",
            params![session_id, now],
        )?;
        Ok(())
    }

    /// Remove `session_id` from the pending queue. Idempotent.
    pub fn dequeue(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM pending_pushes WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Return all pending sessions, ordered by `enqueued_at` ascending (oldest first).
    pub fn list_pending(&self) -> Result<Vec<PendingItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, enqueued_at, last_attempt_at, attempt_count, last_error
             FROM pending_pushes
             ORDER BY enqueued_at ASC",
        )?;
        let items = stmt
            .query_map([], |row| {
                Ok(PendingItem {
                    session_id: row.get(0)?,
                    enqueued_at: row.get(1)?,
                    last_attempt_at: row.get(2)?,
                    attempt_count: row.get(3)?,
                    last_error: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// Bump `attempt_count`, set `last_attempt_at` to now, and record `error`.
    /// `error = None` means the attempt succeeded (caller is responsible for
    /// calling `dequeue` separately on success).
    pub fn record_attempt(&self, session_id: &str, error: Option<&str>) -> Result<()> {
        let now = now_epoch();
        self.conn.execute(
            "UPDATE pending_pushes
             SET attempt_count   = attempt_count + 1,
                 last_attempt_at = ?1,
                 last_error      = ?2
             WHERE session_id = ?3",
            params![now, error, session_id],
        )?;
        Ok(())
    }

    /// Append a push outcome and trim the log to at most `OUTCOME_CAP` rows.
    pub fn record_outcome(&self, success: bool, summary: &str) -> Result<()> {
        let now = now_epoch();
        self.conn.execute(
            "INSERT INTO push_outcomes (at, success, summary) VALUES (?1, ?2, ?3)",
            params![now, success as i64, summary],
        )?;
        // Trim: delete all but the most recent OUTCOME_CAP rows.
        self.conn.execute(
            &format!(
                "DELETE FROM push_outcomes
                 WHERE id NOT IN (
                     SELECT id FROM push_outcomes ORDER BY id DESC LIMIT {OUTCOME_CAP}
                 )"
            ),
            [],
        )?;
        Ok(())
    }

    /// Return the `limit` most-recent outcomes, newest first.
    pub fn recent_outcomes(&self, limit: usize) -> Result<Vec<Outcome>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, at, success, summary
             FROM push_outcomes
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let items = stmt
            .query_map(params![limit as i64], |row| {
                Ok(Outcome {
                    id: row.get(0)?,
                    at: row.get(1)?,
                    success: row.get::<_, i64>(2)? != 0,
                    summary: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// Count consecutive failures scanning backwards from the most-recent outcome.
    /// Stops at the first success (or when rows are exhausted).
    pub fn consecutive_failures(&self) -> Result<u32> {
        // Fetch outcomes newest-first; stop at the first success.
        let mut stmt = self.conn.prepare(
            "SELECT success FROM push_outcomes ORDER BY id DESC",
        )?;
        let mut count = 0u32;
        for row in stmt.query_map([], |row| row.get::<_, i64>(0))? {
            let success = row? != 0;
            if success {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    // ── ETag tracking (C-etag) ────────────────────────────────────────────────

    /// Upsert the last-known remote ETag for `session_id`.
    ///
    /// Called after every successful PUT so subsequent pushes can compare the
    /// recorded ETag against the current remote ETag to detect cross-machine writes.
    pub fn record_etag(&self, session_id: &str, etag: &str) -> Result<()> {
        let now = now_epoch();
        self.conn.execute(
            "INSERT INTO session_etags (session_id, etag, recorded_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET etag = excluded.etag,
                                                   recorded_at = excluded.recorded_at",
            params![session_id, etag, now],
        )?;
        Ok(())
    }

    /// Return the last-recorded ETag for `session_id`, or `None` if never pushed
    /// from this machine (or if the etag was deleted).
    pub fn get_etag(&self, session_id: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT etag FROM session_etags WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query(params![session_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// Remove the ETag record for `session_id` — e.g. when a session is deleted
    /// locally and we no longer want to track it.
    pub fn delete_etag(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM session_etags WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Return all recorded ETags as a map from session_id → etag.
    ///
    /// Used by dry-run to read the full ETag snapshot without engaging any write path.
    pub fn all_etags(&self) -> Result<std::collections::HashMap<String, String>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, etag FROM session_etags",
        )?;
        let map = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<std::result::Result<std::collections::HashMap<_, _>, _>>()?;
        Ok(map)
    }

    // ── v0.13.0: pull-side etag (separate from push-side `session_etags`) ──────

    /// Record the OSS etag of the latest object pulled for a session.  Pull's
    /// "skip A" check compares this to the current remote etag — NOT to whatever
    /// the push side wrote.  This is the v0.13.0 fix for the pull-skip-too-often
    /// bug where our own push updated `session_etags` and the next pull saw a
    /// match (against our own upload) and missed peer-pushed updates.
    pub fn record_pull_etag(&self, session_id: &str, etag: &str) -> Result<()> {
        let now = now_epoch();
        self.conn.execute(
            "INSERT INTO session_pull_etag (session_id, etag, recorded_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET etag = excluded.etag,
                                                   recorded_at = excluded.recorded_at",
            params![session_id, etag, now],
        )?;
        Ok(())
    }

    pub fn get_pull_etag(&self, session_id: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT etag FROM session_pull_etag WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query(params![session_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub fn all_pull_etags(&self) -> Result<std::collections::HashMap<String, String>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, etag FROM session_pull_etag",
        )?;
        let map = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<std::result::Result<std::collections::HashMap<_, _>, _>>()?;
        Ok(map)
    }

    // ── v0.13.0: incremental-pull state ───────────────────────────────────────

    /// Highest delta seq that pull has reconstructed into local for a given
    /// (session_id, device_id) pair.  Pull uses this to know which deltas it
    /// already has and only download the new ones.  Returns `None` if never
    /// pulled this device's content (next pull treats it as full reconstruct).
    pub fn get_pull_seq(
        &self,
        session_id: &str,
        device_id: &str,
    ) -> Result<Option<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT last_seq FROM session_pull_state
             WHERE session_id = ?1 AND device_id = ?2",
        )?;
        let mut rows = stmt.query(params![session_id, device_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get::<_, i64>(0)? as u32))
        } else {
            Ok(None)
        }
    }

    /// Record that pull has successfully consumed deltas up to `last_seq` from
    /// `device_id` for this session.  `base_etag` is the etag of the session's
    /// base.age object at the time of this pull — used to detect compaction
    /// (peer rewrote base) on subsequent pulls.
    pub fn record_pull_seq(
        &self,
        session_id: &str,
        device_id: &str,
        last_seq: u32,
        base_etag: Option<&str>,
    ) -> Result<()> {
        let now = now_epoch();
        self.conn.execute(
            "INSERT INTO session_pull_state
             (session_id, device_id, last_seq, base_etag, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id, device_id) DO UPDATE SET
                last_seq   = excluded.last_seq,
                base_etag  = excluded.base_etag,
                updated_at = excluded.updated_at",
            params![session_id, device_id, last_seq as i64, base_etag, now],
        )?;
        Ok(())
    }

    /// Return the recorded last_seq for every device that has contributed to
    /// this session.  Used by pull's classify_pull_plan to decide whether an
    /// incremental append is safe.  Empty map means "never pulled this
    /// session" (fall through to full reconstruct).
    pub fn get_pull_seqs_for_session(
        &self,
        session_id: &str,
    ) -> Result<std::collections::HashMap<String, u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT device_id, last_seq FROM session_pull_state
             WHERE session_id = ?1",
        )?;
        let map = stmt
            .query_map(params![session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32))
            })?
            .collect::<std::result::Result<std::collections::HashMap<_, _>, _>>()?;
        Ok(map)
    }

    /// Return the base etag this device last saw when reconstructing the
    /// session.  Used to detect compaction (base etag changed → base content
    /// rewritten → previously-accumulated deltas no longer apply → must do
    /// a full reconstruct from scratch).  We share one base_etag across all
    /// devices for a session (it's the same base.age file), so we read the
    /// first row for this session_id.
    pub fn get_pull_base_etag(&self, session_id: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT base_etag FROM session_pull_state
             WHERE session_id = ?1 AND base_etag IS NOT NULL
             LIMIT 1",
        )?;
        let mut rows = stmt.query(params![session_id])?;
        if let Some(row) = rows.next()? {
            Ok(row.get::<_, Option<String>>(0)?)
        } else {
            Ok(None)
        }
    }

    /// Drop all pull state for a session — used when we force a full
    /// reconstruct (e.g. on compaction detection) so the next pull starts
    /// fresh.
    pub fn clear_pull_state(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM session_pull_state WHERE session_id = ?1",
            params![session_id],
        )?;
        self.conn.execute(
            "DELETE FROM session_pull_etag WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    // ── Delta-sync state tracking (v0.9.0) ────────────────────────────────────

    /// Return how many plaintext bytes of `session_id` this device has already
    /// pushed (as base + accumulated deltas).
    ///
    /// `None` means "never pushed from this device" — push will treat this as a
    /// first-time full base upload.
    pub fn get_session_state(&self, session_id: &str) -> Result<Option<SessionState>> {
        let mut stmt = self.conn.prepare(
            "SELECT last_pushed_size FROM session_state WHERE session_id = ?1",
        )?;
        let row = stmt
            .query_row(params![session_id], |r| {
                let size: i64 = r.get(0)?;
                Ok(SessionState {
                    last_pushed_size: size.max(0) as u64,
                })
            })
            .ok();
        Ok(row)
    }

    /// Upsert delta-sync state. Called after every successful base or delta push.
    pub fn record_session_state(&self, session_id: &str, last_pushed_size: u64) -> Result<()> {
        let now = now_epoch();
        self.conn.execute(
            "INSERT INTO session_state (session_id, last_pushed_size, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                 last_pushed_size = excluded.last_pushed_size,
                 updated_at       = excluded.updated_at",
            params![session_id, last_pushed_size as i64, now],
        )?;
        Ok(())
    }

    /// Forget delta-sync state for `session_id`. Used when a base is rewritten
    /// (compaction or stale-overwrite) so the next push starts fresh.
    pub fn delete_session_state(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM session_state WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    // ── Authoritative source_cwd cache (v0.9.3) ───────────────────────────────

    /// Look up the cached authoritative source_cwd for `session_id`.
    ///
    /// Returns `None` if no entry exists — caller should fall back to scanning
    /// the jsonl content (which is what list_local_sessions does today).
    pub fn get_session_cwd(&self, session_id: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_cwd FROM session_cwd WHERE session_id = ?1",
        )?;
        let row = stmt
            .query_row(params![session_id], |r| r.get::<_, String>(0))
            .ok();
        Ok(row)
    }

    /// Upsert the authoritative source_cwd for `session_id`. Called from:
    ///   - `pull.rs` on successful pull (cwd from received meta — propagated
    ///     from whichever device originally produced the meta correctly)
    ///   - `claude_code.rs` when `scan_jsonl` extracts a real cwd field
    ///
    /// MUST NOT be called from the dir-name decode fallback path — that's the
    /// lossy form we're trying to override, recording it here would lock us
    /// into the wrong answer forever.
    pub fn record_session_cwd(&self, session_id: &str, source_cwd: &str) -> Result<()> {
        let now = now_epoch();
        self.conn.execute(
            "INSERT INTO session_cwd (session_id, source_cwd, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                 source_cwd = excluded.source_cwd,
                 updated_at = excluded.updated_at",
            params![session_id, source_cwd, now],
        )?;
        Ok(())
    }
}

/// Per-session push state for delta sync. See `Queue::get_session_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionState {
    pub last_pushed_size: u64,
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn default_queue_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("$HOME not set"))?;
    Ok(PathBuf::from(home)
        .join(".local/share/sessync")
        .join(DB_FILE))
}
