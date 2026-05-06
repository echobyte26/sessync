//! Tests for `sessync logs` formatting helpers and queue integration.

use chrono::{Duration, TimeZone, Utc};
use sessync::commands::logs::{format_outcome_line, format_relative};
use sessync::queue::Queue;
use sessync::ui::style;
use tempfile::TempDir;

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
    // Use a fixed timestamp so the test is deterministic.
    let then = Utc.with_ymd_and_hms(2024, 3, 15, 10, 30, 0).unwrap();
    let now = then + Duration::hours(25);
    let result = format_relative(now, then);
    assert_eq!(result, "2024-03-15 10:30");
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
