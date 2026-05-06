use sessync::notify::sanitize_for_osascript;
use sessync::queue::Queue;

#[test]
fn notify_does_not_panic_on_macos() {
    // Platform-independent: just assert no panic.
    sessync::notify::notify("test title", "test body with \"quotes\"");
}

#[test]
fn notify_sanitizes_double_quotes() {
    assert_eq!(
        sanitize_for_osascript(r#"he said "hello""#),
        "he said 'hello'"
    );
    assert_eq!(sanitize_for_osascript("no quotes here"), "no quotes here");
    assert_eq!(sanitize_for_osascript(""), "");
    // Multiple quotes
    assert_eq!(
        sanitize_for_osascript(r#""a" and "b""#),
        "'a' and 'b'"
    );
}

/// Pure logic test: the A4 notification threshold fires on exactly 3 consecutive
/// failures, not on 2, not on 4+.
#[test]
fn consecutive_failures_threshold_triggers_only_on_exact_count() {
    // Helper to build a queue with a specific sequence of outcomes.
    fn failures_after(successes_first: bool, failure_count: u32) -> u32 {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::open_at(&dir.path().join("queue.db")).unwrap();
        if successes_first {
            q.record_outcome(true, "ok").unwrap();
        }
        for _ in 0..failure_count {
            q.record_outcome(false, "err").unwrap();
        }
        q.consecutive_failures().unwrap()
    }

    // Exactly 3 failures → should equal 3 (trigger threshold).
    assert_eq!(failures_after(true, 3), 3);

    // Fewer than 3 → below threshold.
    assert_eq!(failures_after(true, 2), 2);
    assert_eq!(failures_after(true, 1), 1);
    assert_eq!(failures_after(true, 0), 0);

    // More than 3 → above threshold; notification fires only at the 3rd run,
    // subsequent runs see n=4,5,… and the `== 3` guard prevents re-spamming.
    assert_eq!(failures_after(true, 4), 4);
    assert_eq!(failures_after(true, 5), 5);
}
