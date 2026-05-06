use sessync::queue::Queue;
use tempfile::TempDir;

fn open_temp_queue() -> (Queue, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let q = Queue::open_at(&dir.path().join("queue.db")).unwrap();
    (q, dir)
}

#[test]
fn enqueue_dequeue_roundtrip() {
    let (q, _dir) = open_temp_queue();

    q.enqueue("session-abc").unwrap();
    let pending = q.list_pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].session_id, "session-abc");

    q.dequeue("session-abc").unwrap();
    let pending = q.list_pending().unwrap();
    assert!(pending.is_empty(), "queue should be empty after dequeue");
}

#[test]
fn enqueue_is_idempotent() {
    let (q, _dir) = open_temp_queue();

    q.enqueue("session-xyz").unwrap();
    q.enqueue("session-xyz").unwrap(); // second call — must not bump count or reset enqueued_at

    let pending = q.list_pending().unwrap();
    assert_eq!(pending.len(), 1, "should still be exactly 1 row");
    assert_eq!(
        pending[0].attempt_count, 0,
        "enqueue alone must not bump attempt_count"
    );
}

#[test]
fn record_attempt_increments_count_and_updates_error() {
    let (q, _dir) = open_temp_queue();

    q.enqueue("session-err").unwrap();

    q.record_attempt("session-err", Some("network timeout")).unwrap();
    let pending = q.list_pending().unwrap();
    assert_eq!(pending[0].attempt_count, 1);
    assert_eq!(pending[0].last_error.as_deref(), Some("network timeout"));
    assert!(pending[0].last_attempt_at.is_some());

    q.record_attempt("session-err", Some("auth expired")).unwrap();
    let pending = q.list_pending().unwrap();
    assert_eq!(pending[0].attempt_count, 2);
    assert_eq!(pending[0].last_error.as_deref(), Some("auth expired"));
}

#[test]
fn consecutive_failures_counts_back_from_latest() {
    let (q, _dir) = open_temp_queue();

    // S F F F → 3 consecutive failures
    q.record_outcome(true, "pushed 1 (skipped 0)").unwrap();
    q.record_outcome(false, "upload failed: timeout").unwrap();
    q.record_outcome(false, "upload failed: timeout").unwrap();
    q.record_outcome(false, "upload failed: timeout").unwrap();
    assert_eq!(q.consecutive_failures().unwrap(), 3);

    let (q2, _dir2) = open_temp_queue();
    // F S F F → 2 consecutive failures
    q2.record_outcome(false, "err").unwrap();
    q2.record_outcome(true, "ok").unwrap();
    q2.record_outcome(false, "err").unwrap();
    q2.record_outcome(false, "err").unwrap();
    assert_eq!(q2.consecutive_failures().unwrap(), 2);

    let (q3, _dir3) = open_temp_queue();
    // All successes → 0 consecutive failures
    q3.record_outcome(true, "ok").unwrap();
    q3.record_outcome(true, "ok").unwrap();
    assert_eq!(q3.consecutive_failures().unwrap(), 0);
}

#[test]
fn recent_outcomes_orders_newest_first() {
    let (q, _dir) = open_temp_queue();

    q.record_outcome(true, "first").unwrap();
    q.record_outcome(false, "second").unwrap();
    q.record_outcome(true, "third").unwrap();

    let outcomes = q.recent_outcomes(10).unwrap();
    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0].summary, "third", "newest must come first");
    assert_eq!(outcomes[2].summary, "first", "oldest must come last");
}

#[test]
fn recent_outcomes_respects_limit_and_truncates_old_rows() {
    let (q, _dir) = open_temp_queue();

    // Insert 150 rows — trim cap is 100.
    for i in 0..150usize {
        q.record_outcome(i % 2 == 0, &format!("outcome-{i}")).unwrap();
    }

    // The table must hold at most ~100 rows after trimming.
    let all = q.recent_outcomes(200).unwrap();
    assert!(
        all.len() <= 100,
        "expected at most 100 rows retained, got {}",
        all.len()
    );

    // The most-recent row should be outcome-149 (the last inserted).
    assert_eq!(all[0].summary, "outcome-149");

    // recent_outcomes(5) should return only 5.
    let limited = q.recent_outcomes(5).unwrap();
    assert_eq!(limited.len(), 5);
}
