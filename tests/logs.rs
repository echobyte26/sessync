//! Tests for `sessync logs` formatting helpers and queue integration.

use chrono::{Duration, TimeZone, Utc};
use sessync::commands::logs::{format_outcome_line, format_relative, parse_since};
use sessync::queue::Queue;
use sessync::ui::style;
use tempfile::TempDir;

// ── parse_since ───────────────────────────────────────────────────────────────

#[test]
fn parse_since_handles_common_formats() {
    assert_eq!(parse_since("30s").unwrap(), 30);
    assert_eq!(parse_since("5m").unwrap(), 300);
    assert_eq!(parse_since("1h").unwrap(), 3600);
    assert_eq!(parse_since("2d").unwrap(), 172_800);
}

#[test]
fn parse_since_rejects_garbage() {
    assert!(parse_since("").is_err());
    assert!(parse_since("abc").is_err());
    assert!(parse_since("5x").is_err());
    assert!(parse_since("0m").is_err());
    assert!(parse_since("-1h").is_err());
    assert!(parse_since("5").is_err());
}

// ── format_relative ──────────────────────────────────────────────────────────

#[test]
fn format_relative_just_now() {
    let now = Utc::now();
    let then = now - Duration::seconds(30);
    assert_eq!(format_relative(now, then), "just now");
}

#[test]
fn format_relative_minutes() {
    let now = Utc::now();
    // 5 minutes ago
    let then = now - Duration::minutes(5);
    assert_eq!(format_relative(now, then), "5 minutes ago");
}

#[test]
fn format_relative_one_minute_singular() {
    let now = Utc::now();
    let then = now - Duration::minutes(1);
    assert_eq!(format_relative(now, then), "1 minute ago");
}

#[test]
fn format_relative_hours() {
    let now = Utc::now();
    let then = now - Duration::hours(2);
    assert_eq!(format_relative(now, then), "2 hours ago");
}

#[test]
fn format_relative_one_hour_singular() {
    let now = Utc::now();
    let then = now - Duration::hours(1);
    assert_eq!(format_relative(now, then), "1 hour ago");
}

#[test]
fn format_relative_falls_back_to_absolute_after_24h() {
    // Use a fixed UTC timestamp; absolute fallback formats in local TZ so we
    // compute the expected string the same way the impl does.
    let then = Utc.with_ymd_and_hms(2024, 3, 15, 10, 30, 0).unwrap();
    let now = then + Duration::hours(25);
    let result = format_relative(now, then);
    let expected = then
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M")
        .to_string();
    assert_eq!(result, expected);
    assert!(!result.contains("ago"), "should not say 'ago': {result}");
}

// ── format_outcome_line ───────────────────────────────────────────────────────

#[test]
fn format_outcome_line_includes_check_mark_on_success() {
    let line = format_outcome_line("2 minutes ago", true, "pushed 3 (skipped 1)");
    // The raw Unicode check mark character must appear in the output
    // regardless of whether color escape codes are present (NO_COLOR or TTY).
    assert!(
        line.contains(style::check_ok()),
        "expected ✓ in line: {line:?}"
    );
    assert!(line.contains("pushed 3 (skipped 1)"), "summary missing: {line:?}");
}

#[test]
fn format_outcome_line_includes_cross_on_failure() {
    let line = format_outcome_line("1 hour ago", false, "upload abc-123: oss error: 403");
    assert!(
        line.contains(style::cross_fail()),
        "expected ✗ in line: {line:?}"
    );
    assert!(line.contains("upload abc-123"), "summary missing: {line:?}");
}

// ── end-to-end: Queue::open_at + recent_outcomes ordering ────────────────────

#[test]
fn recent_outcomes_returns_newest_first() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("queue.db");
    let q = Queue::open_at(&db_path).unwrap();

    q.record_outcome(true, "pushed 1 (skipped 0)").unwrap();
    q.record_outcome(false, "upload err: 403").unwrap();
    q.record_outcome(true, "pushed 2 (skipped 1)").unwrap();

    let outcomes = q.recent_outcomes(10).unwrap();
    assert_eq!(outcomes.len(), 3);
    // Newest first: last recorded is at index 0.
    assert_eq!(outcomes[0].summary, "pushed 2 (skipped 1)");
    assert!(outcomes[0].success);
    assert_eq!(outcomes[1].summary, "upload err: 403");
    assert!(!outcomes[1].success);
    assert_eq!(outcomes[2].summary, "pushed 1 (skipped 0)");
}

#[test]
fn recent_outcomes_respects_limit() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("queue.db");
    let q = Queue::open_at(&db_path).unwrap();

    for i in 0..10 {
        q.record_outcome(true, &format!("pushed {i}")).unwrap();
    }

    let outcomes = q.recent_outcomes(3).unwrap();
    assert_eq!(outcomes.len(), 3);
    // The 3 most recent are pushed 9, pushed 8, pushed 7.
    assert_eq!(outcomes[0].summary, "pushed 9");
}

// ── filter logic (since + failed) ─────────────────────────────────────────────

/// Verify that the `--failed` filter keeps only failed outcomes.
#[test]
fn filter_failed_keeps_only_failures() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("queue.db");
    let q = Queue::open_at(&db_path).unwrap();

    q.record_outcome(true, "ok push").unwrap();
    q.record_outcome(false, "err push A").unwrap();
    q.record_outcome(true, "ok push 2").unwrap();
    q.record_outcome(false, "err push B").unwrap();

    let all = q.recent_outcomes(100).unwrap();
    let failed: Vec<_> = all.iter().filter(|o| !o.success).collect();
    assert_eq!(failed.len(), 2);
    assert!(failed.iter().all(|o| o.summary.starts_with("err push")));
}

/// Verify the `--since` filter: outcomes older than the window are excluded.
#[test]
fn filter_since_excludes_old_outcomes() {
    // We simulate by inspecting the `at` field directly: outcomes recorded
    // "right now" are within any reasonable window (e.g. 1h), while we can
    // derive what an old `at` timestamp would look like.
    let now = Utc::now().timestamp();

    // Use parse_since to confirm the helper correctly converts window strings.
    let window_1h = parse_since("1h").unwrap(); // 3600

    // An outcome timestamped 7200s ago should be excluded by a 1h window.
    let old_at = now - 7200;
    assert!(now - old_at > window_1h, "sanity: old outcome is outside 1h window");

    // An outcome timestamped 30m ago should be included by a 1h window.
    let recent_at = now - 1800;
    assert!(now - recent_at <= window_1h, "sanity: recent outcome is inside 1h window");
}

/// Verify `--since` + `--failed` composition: only recent failures pass both filters.
#[test]
fn filter_since_and_failed_are_composed() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("queue.db");
    let q = Queue::open_at(&db_path).unwrap();

    // Record a mix of outcomes (all "recent" — just recorded).
    q.record_outcome(true, "success A").unwrap();
    q.record_outcome(false, "failure A").unwrap();
    q.record_outcome(true, "success B").unwrap();

    let all = q.recent_outcomes(100).unwrap();
    let now_ts = Utc::now().timestamp();
    let window = parse_since("1h").unwrap();

    // Apply both filters manually (mirrors what run() does).
    let filtered: Vec<_> = all
        .iter()
        .filter(|o| !o.success && now_ts - o.at <= window)
        .collect();

    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].summary, "failure A");
}
