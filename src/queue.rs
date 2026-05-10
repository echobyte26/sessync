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
