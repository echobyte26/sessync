//! Integration tests for `CodexAdapter`.
//!
//! These tests **never touch the real `~/.codex/`**. Each test builds a fresh
//! temporary directory via `make_temp_codex_root()`, creates a minimal SQLite
//! schema, and passes the temp path to `CodexAdapter::at(...)`.
//!
//! Schema note: the real `state_5.sqlite` has several NOT NULL columns that need
//! defaults. Our minimal schema uses `DEFAULT ''` or `DEFAULT 0` on all of them
//! so that we can INSERT rows without supplying every column in test setup.

use rusqlite::Connection;
use sessync::adapter::codex::CodexAdapter;
use sessync::adapter::tool::ToolAdapter;
use sessync::types::SessionId;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared fixture helpers
// ---------------------------------------------------------------------------

/// Create a temp directory with a `.codex/` subdirectory and a minimal
/// `state_5.sqlite` (schema matches what `CodexAdapter` expects).
fn make_temp_codex_root() -> TempDir {
    let td = TempDir::new().unwrap();
    let codex = td.path().join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    let conn = Connection::open(codex.join("state_5.sqlite")).unwrap();
    // Minimal schema — mirrors the real schema's required columns but adds
    // DEFAULT values so test INSERTs don't need to supply every column.
    conn.execute_batch(
        r#"
        CREATE TABLE threads (
            id                TEXT PRIMARY KEY,
            rollout_path      TEXT NOT NULL,
            created_at        INTEGER NOT NULL DEFAULT 0,
            updated_at        INTEGER NOT NULL DEFAULT 0,
            source            TEXT NOT NULL DEFAULT 'local',
            model_provider    TEXT NOT NULL DEFAULT 'unknown',
            cwd               TEXT NOT NULL,
            title             TEXT NOT NULL DEFAULT '',
            sandbox_policy    TEXT NOT NULL DEFAULT 'default',
            approval_mode     TEXT NOT NULL DEFAULT 'suggest',
            tokens_used       INTEGER NOT NULL DEFAULT 0,
            has_user_event    INTEGER NOT NULL DEFAULT 0,
            archived          INTEGER NOT NULL DEFAULT 0,
            archived_at       INTEGER,
            git_sha           TEXT,
            git_branch        TEXT,
            git_origin_url    TEXT,
            cli_version       TEXT NOT NULL DEFAULT '',
            first_user_message TEXT NOT NULL DEFAULT '',
            agent_nickname    TEXT,
            agent_role        TEXT,
            memory_mode       TEXT NOT NULL DEFAULT 'enabled',
            model             TEXT,
            reasoning_effort  TEXT,
            agent_path        TEXT,
            created_at_ms     INTEGER,
            updated_at_ms     INTEGER,
            thread_source     TEXT
        );
        "#,
    )
    .unwrap();
    td
}

/// Insert a session row into the `threads` table.
fn insert_thread(conn: &Connection, id: &str, cwd: &str, rollout_path: &str, preview: &str) {
    conn.execute(
        "INSERT INTO threads (id, rollout_path, cwd, first_user_message, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, rollout_path, cwd, preview, 1_700_000_000_000_i64],
    )
    .unwrap();
}

/// Write a minimal JSONL rollout file at `path` with some content.
fn write_fake_rollout(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// When `~/.codex/` does not exist at all, `list_local_sessions` returns an
/// empty vec without error — Codex simply isn't installed.
#[tokio::test]
async fn list_returns_empty_when_no_codex_install() {
    let td = TempDir::new().unwrap();
    let missing_root = td.path().join("no_codex_here");
    // Do NOT create missing_root — it must not exist.

    let adapter = CodexAdapter::at(&missing_root);
    let sessions = adapter.list_local_sessions().await.unwrap();
    assert!(
        sessions.is_empty(),
        "expected empty list when codex root is absent, got {sessions:#?}"
    );
}

/// A SQLite DB with one row and a corresponding rollout file should produce
/// exactly one `LocalSession` with all fields populated.
#[tokio::test]
async fn list_returns_sessions_from_sqlite() {
    let td = make_temp_codex_root();
    let codex = td.path().join(".codex");

    let uuid = "aaaabbbb-cccc-dddd-eeee-111122223333";
    let rollout = codex
        .join("sessions/2026/01/01")
        .join(format!("rollout-2026-01-01T00-00-00-{uuid}.jsonl"));
    write_fake_rollout(&rollout, r#"{"item":{"session_meta":{}}}"#);

    {
        let conn = Connection::open(codex.join("state_5.sqlite")).unwrap();
        insert_thread(
            &conn,
            uuid,
            "/home/user/myproject",
            rollout.to_str().unwrap(),
            "hello codex",
        );
    }

    let adapter = CodexAdapter::at(&codex);
    let sessions = adapter.list_local_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1, "expected exactly 1 session");

    let s = &sessions[0];
    assert_eq!(s.meta.session_id.0, uuid);
    assert_eq!(s.meta.source_cwd, "/home/user/myproject");
    assert_eq!(s.meta.preview, "hello codex");
    assert_eq!(s.local_path, rollout);
    assert_eq!(s.meta.schema_version, 1);
}

/// A row pointing at a nonexistent rollout file should be silently skipped;
/// other valid rows must still be returned.
#[tokio::test]
async fn list_skips_rows_with_missing_rollout_file() {
    let td = make_temp_codex_root();
    let codex = td.path().join(".codex");

    let good_uuid = "00000000-0000-0000-0000-000000000001";
    let bad_uuid = "00000000-0000-0000-0000-000000000002";

    let good_rollout = codex
        .join("sessions/2026/01/01")
        .join(format!("rollout-2026-01-01T00-00-00-{good_uuid}.jsonl"));
    write_fake_rollout(&good_rollout, "{}");

    let missing_rollout = codex
        .join("sessions/2026/01/01")
        .join(format!("rollout-2026-01-01T00-00-01-{bad_uuid}.jsonl"));
    // intentionally NOT created

    {
        let conn = Connection::open(codex.join("state_5.sqlite")).unwrap();
        insert_thread(
            &conn,
            good_uuid,
            "/proj/good",
            good_rollout.to_str().unwrap(),
            "good session",
        );
        insert_thread(
            &conn,
            bad_uuid,
            "/proj/bad",
            missing_rollout.to_str().unwrap(),
            "bad session",
        );
    }

    let adapter = CodexAdapter::at(&codex);
    let sessions = adapter.list_local_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1, "only the session with an existing rollout file should be returned");
    assert_eq!(sessions[0].meta.session_id.0, good_uuid);
}

/// `read_session` should return the raw bytes from the rollout file.
#[tokio::test]
async fn read_session_returns_jsonl_bytes() {
    let td = make_temp_codex_root();
    let codex = td.path().join(".codex");

    let uuid = "read-0000-0000-0000-000000000001";
    let rollout = codex
        .join("sessions/2026/01/01")
        .join(format!("rollout-2026-01-01T00-00-00-{uuid}.jsonl"));
    let expected = b"the raw jsonl content\n";
    write_fake_rollout(&rollout, std::str::from_utf8(expected).unwrap());

    {
        let conn = Connection::open(codex.join("state_5.sqlite")).unwrap();
        insert_thread(
            &conn,
            uuid,
            "/some/cwd",
            rollout.to_str().unwrap(),
            "preview",
        );
    }

    let adapter = CodexAdapter::at(&codex);
    let bytes = adapter.read_session(&SessionId(uuid.into())).await.unwrap();
    assert_eq!(bytes, expected);
}

/// `read_session` on an unknown session ID returns an error (not a panic).
#[tokio::test]
async fn read_session_not_found_returns_error() {
    let td = make_temp_codex_root();
    let codex = td.path().join(".codex");

    let adapter = CodexAdapter::at(&codex);
    let result = adapter
        .read_session(&SessionId("nonexistent-uuid".into()))
        .await;
    assert!(result.is_err(), "expected error for unknown session_id");
}

/// `write_session` should create the rollout file at the expected date-partitioned
/// path and insert a corresponding row into the `threads` table.
#[tokio::test]
async fn write_session_creates_rollout_and_db_row() {
    let td = make_temp_codex_root();
    let codex = td.path().join(".codex");

    let uuid = "write-000-0000-0000-000000000001";
    let raw = b"first line\nsecond line\n";

    let adapter = CodexAdapter::at(&codex);
    let returned_path = adapter
        .write_session(&SessionId(uuid.into()), "/target/cwd", raw)
        .await
        .unwrap();

    // The rollout file should exist and contain our bytes.
    assert!(returned_path.exists(), "rollout file must exist after write_session");
    let on_disk = std::fs::read(&returned_path).unwrap();
    assert_eq!(on_disk, raw);

    // The DB row should be present.
    let conn = Connection::open(codex.join("state_5.sqlite")).unwrap();
    let (cwd, rollout_path_db, source): (String, String, String) = conn
        .query_row(
            "SELECT cwd, rollout_path, source FROM threads WHERE id = ?1",
            [uuid],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    assert_eq!(cwd, "/target/cwd");
    assert_eq!(rollout_path_db, returned_path.to_str().unwrap());
    assert_eq!(source, "sessync");
}

/// Calling `write_session` twice with the same UUID should UPDATE (upsert) rather
/// than fail with a UNIQUE constraint violation.
#[tokio::test]
async fn write_session_idempotent_on_uuid_collision() {
    let td = make_temp_codex_root();
    let codex = td.path().join(".codex");

    let uuid = "idem-0000-0000-0000-000000000001";
    let adapter = CodexAdapter::at(&codex);

    adapter
        .write_session(&SessionId(uuid.into()), "/cwd/v1", b"version one\n")
        .await
        .unwrap();
    adapter
        .write_session(&SessionId(uuid.into()), "/cwd/v2", b"version two\n")
        .await
        .unwrap();

    // Only one row should exist.
    let conn = Connection::open(codex.join("state_5.sqlite")).unwrap();
    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM threads WHERE id = ?1", [uuid], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(row_count, 1, "idempotent write must produce exactly one row");

    // The cwd should reflect the second write.
    let cwd: String = conn
        .query_row(
            "SELECT cwd FROM threads WHERE id = ?1",
            [uuid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cwd, "/cwd/v2");
}

/// `write_session` must create a backup of the SQLite file before mutating it.
#[tokio::test]
async fn write_session_backs_up_sqlite_first() {
    let td = make_temp_codex_root();
    let codex = td.path().join(".codex");

    let uuid = "backup-00-0000-0000-000000000001";
    let adapter = CodexAdapter::at(&codex);

    adapter
        .write_session(&SessionId(uuid.into()), "/some/cwd", b"payload\n")
        .await
        .unwrap();

    // There should be exactly one backup file.
    let db_path = codex.join("state_5.sqlite");
    let backups: Vec<_> = std::fs::read_dir(&codex)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("state_5.sqlite.sessync-backup-")
        })
        .collect();

    assert_eq!(backups.len(), 1, "expected exactly one backup after first write");
    // The backup should be a valid copy (non-empty, same as db).
    let db_size = std::fs::metadata(&db_path).unwrap().len();
    let backup_size = std::fs::metadata(backups[0].path()).unwrap().len();
    assert_eq!(backup_size, db_size, "backup should be same size as DB");
}

/// When multiple state_*.sqlite files exist, the adapter should pick the one
/// with the highest version number.
#[tokio::test]
async fn glob_picks_highest_state_version() {
    let td = TempDir::new().unwrap();
    let codex = td.path().join(".codex");
    std::fs::create_dir_all(&codex).unwrap();

    // Create state_5.sqlite and state_6.sqlite.
    let minimal_schema = r#"
        CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            rollout_path TEXT NOT NULL,
            cwd TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL DEFAULT 0,
            source TEXT NOT NULL DEFAULT 'local',
            model_provider TEXT NOT NULL DEFAULT 'unknown',
            title TEXT NOT NULL DEFAULT '',
            sandbox_policy TEXT NOT NULL DEFAULT 'default',
            approval_mode TEXT NOT NULL DEFAULT 'suggest',
            tokens_used INTEGER NOT NULL DEFAULT 0,
            has_user_event INTEGER NOT NULL DEFAULT 0,
            archived INTEGER NOT NULL DEFAULT 0,
            cli_version TEXT NOT NULL DEFAULT '',
            first_user_message TEXT NOT NULL DEFAULT '',
            memory_mode TEXT NOT NULL DEFAULT 'enabled',
            created_at_ms INTEGER,
            updated_at_ms INTEGER
        );
    "#;

    for version in [5u64, 6u64] {
        let path = codex.join(format!("state_{version}.sqlite"));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(minimal_schema).unwrap();
    }

    // Insert a row only in state_6 so we can verify which DB was opened.
    let uuid6 = "6666-0000-0000-0000-000000000006";
    let rollout6 = codex.join("sessions/2026/01/01/rollout-2026-01-01T00-00-00-v6.jsonl");
    write_fake_rollout(&rollout6, "{}");
    {
        let conn = Connection::open(codex.join("state_6.sqlite")).unwrap();
        insert_thread(
            &conn,
            uuid6,
            "/proj/v6",
            rollout6.to_str().unwrap(),
            "from v6",
        );
    }

    // state_5 has no rows.
    let adapter = CodexAdapter::at(&codex);
    let sessions = adapter.list_local_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1, "should pick state_6 (has a row) not state_5");
    assert_eq!(sessions[0].meta.session_id.0, uuid6);
}

/// If the SQLite file is missing required columns, the adapter must return a
/// clear error string — it must NOT panic and must NOT corrupt the DB.
#[tokio::test]
async fn unknown_schema_fails_gracefully_on_list() {
    let td = TempDir::new().unwrap();
    let codex = td.path().join(".codex");
    std::fs::create_dir_all(&codex).unwrap();

    // Create a DB that is missing the required `updated_at_ms` column.
    let conn = Connection::open(codex.join("state_5.sqlite")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            rollout_path TEXT NOT NULL,
            cwd TEXT NOT NULL
            -- deliberately omitting first_user_message, updated_at_ms, etc.
        );
        "#,
    )
    .unwrap();
    drop(conn);

    let adapter = CodexAdapter::at(&codex);
    // list_local_sessions must return Ok(empty) — it logs a warn but does not propagate the error.
    let sessions = adapter.list_local_sessions().await.unwrap();
    assert!(
        sessions.is_empty(),
        "schema mismatch should yield empty list, not a panic"
    );
}

/// `write_session` with a bad schema (missing required columns) returns an error
/// but does NOT corrupt the DB — the backup should still exist and the DB content
/// unchanged (because the backup was made before the failed write).
#[tokio::test]
async fn unknown_schema_fails_gracefully_on_write() {
    let td = TempDir::new().unwrap();
    let codex = td.path().join(".codex");
    std::fs::create_dir_all(&codex).unwrap();

    // Schema missing `first_user_message` → verify_schema will fail on write.
    let conn = Connection::open(codex.join("state_5.sqlite")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            rollout_path TEXT NOT NULL,
            cwd TEXT NOT NULL
        );
        "#,
    )
    .unwrap();
    drop(conn);

    let adapter = CodexAdapter::at(&codex);
    let result = adapter
        .write_session(
            &SessionId("schema-bad-0000-0000-000000000001".into()),
            "/some/cwd",
            b"data",
        )
        .await;

    // Must return an error, not panic.
    assert!(result.is_err(), "write_session with bad schema must return Err");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not supported") || err_msg.contains("missing"),
        "error message should mention schema issue, got: {err_msg}"
    );
}

/// `project_key_for` is deterministic: same cwd → same key.
#[test]
fn project_key_for_is_deterministic() {
    let adapter = CodexAdapter::at("/nonexistent");
    let k1 = adapter.project_key_for("/home/user/myproject");
    let k2 = adapter.project_key_for("/home/user/myproject");
    assert_eq!(k1, k2);
}

/// `project_key_for` produces the same result as calling `path_codec` directly,
/// so Claude Code and Codex can share cross-tool project grouping.
#[test]
fn project_key_matches_path_codec() {
    use sessync::adapter::path_codec;

    let adapter = CodexAdapter::at("/nonexistent");
    let cwd = "/Users/james/Project/myapp";
    let from_adapter = adapter.project_key_for(cwd);
    let from_codec = path_codec::project_key_for_cwd(cwd);
    assert_eq!(from_adapter, from_codec);
}

/// `name()` must return `"codex"`.
#[test]
fn name_returns_codex() {
    let adapter = CodexAdapter::at("/nonexistent");
    assert_eq!(adapter.name(), "codex");
}
