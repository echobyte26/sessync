//! Unit tests for the pure (I/O-free) helpers in `commands::doctor`.
//!
//! We don't test `run()` end-to-end because it touches the real filesystem,
//! network, and system tools. Instead, we test the small classifier functions
//! that contain the decision logic — those are safe to exercise anywhere.

use sessync::commands::doctor::{
    check_codex_binary_reachable, check_codex_dir_exists, check_codex_sqlite_present,
    classify_consecutive_failures, classify_storage_error, CheckResult,
};
use sessync::error::SessyncError;

// ── consecutive_failures classifier ──────────────────────────────────────────

#[test]
fn consecutive_failures_below_3_passes() {
    for n in [0u32, 1, 2] {
        let result = classify_consecutive_failures(n);
        assert!(
            matches!(result, CheckResult::Pass(_)),
            "expected Pass for n={n}, got {result:?}"
        );
    }
}

#[test]
fn consecutive_failures_at_3_fails() {
    let result = classify_consecutive_failures(3);
    assert!(
        matches!(result, CheckResult::Fail { .. }),
        "expected Fail for n=3, got {result:?}"
    );
}

#[test]
fn consecutive_failures_above_3_fails() {
    for n in [4u32, 10, 100] {
        let result = classify_consecutive_failures(n);
        assert!(
            matches!(result, CheckResult::Fail { .. }),
            "expected Fail for n={n}, got {result:?}"
        );
    }
}

#[test]
fn consecutive_failures_fail_contains_count() {
    let result = classify_consecutive_failures(5);
    if let CheckResult::Fail { reason, .. } = result {
        assert!(
            reason.contains('5'),
            "reason should mention the failure count, got: {reason}"
        );
    } else {
        panic!("expected Fail");
    }
}

#[test]
fn consecutive_failures_fail_has_hint() {
    let result = classify_consecutive_failures(3);
    if let CheckResult::Fail { hint, .. } = result {
        assert!(hint.is_some(), "Fail result should carry an actionable hint");
    } else {
        panic!("expected Fail");
    }
}

// ── storage error classifier ──────────────────────────────────────────────────

#[test]
fn storage_auth_error_yields_specific_hint() {
    let err = SessyncError::Storage("storage error: InvalidAccessKeyId".to_string());
    let result = classify_storage_error(&err);
    match result {
        CheckResult::Fail { hint: Some(h), .. } => {
            assert!(
                h.contains("access_key_id") || h.contains("access_key_secret"),
                "auth hint should mention credentials, got: {h}"
            );
        }
        other => panic!("expected Fail with credential hint, got {other:?}"),
    }
}

#[test]
fn storage_signature_error_yields_specific_hint() {
    let err = SessyncError::Storage("storage error: SignatureDoesNotMatch".to_string());
    let result = classify_storage_error(&err);
    match result {
        CheckResult::Fail { hint: Some(h), .. } => {
            assert!(
                h.contains("access_key_id") || h.contains("access_key_secret"),
                "signature hint should mention credentials, got: {h}"
            );
        }
        other => panic!("expected Fail with credential hint, got {other:?}"),
    }
}

#[test]
fn storage_network_error_yields_different_hint() {
    let err = SessyncError::Storage("storage error: dns resolution failed".to_string());
    let result = classify_storage_error(&err);
    match result {
        CheckResult::Fail { hint: Some(h), reason: _ } => {
            assert!(
                h.contains("internet") || h.contains("endpoint") || h.contains("network"),
                "network hint should mention connectivity or endpoint, got: {h}"
            );
        }
        other => panic!("expected Fail with network hint, got {other:?}"),
    }
}

#[test]
fn storage_timeout_error_yields_network_hint() {
    let err = SessyncError::Storage("storage error: connection timed out".to_string());
    let result = classify_storage_error(&err);
    match result {
        CheckResult::Fail { hint: Some(h), .. } => {
            assert!(
                h.contains("internet") || h.contains("endpoint"),
                "timeout hint should mention connectivity, got: {h}"
            );
        }
        other => panic!("expected Fail, got {other:?}"),
    }
}

#[test]
fn storage_auth_and_network_hints_are_different() {
    let auth_err = SessyncError::Storage("storage error: InvalidAccessKeyId".to_string());
    let net_err = SessyncError::Storage("storage error: dns error".to_string());

    let auth_hint = match classify_storage_error(&auth_err) {
        CheckResult::Fail { hint: Some(h), .. } => h,
        other => panic!("expected Fail, got {other:?}"),
    };
    let net_hint = match classify_storage_error(&net_err) {
        CheckResult::Fail { hint: Some(h), .. } => h,
        other => panic!("expected Fail, got {other:?}"),
    };
    assert_ne!(
        auth_hint, net_hint,
        "auth and network hints should be distinct"
    );
}

// ── CheckResult basic properties ──────────────────────────────────────────────

#[test]
fn check_result_pass_equality() {
    assert_eq!(
        CheckResult::Pass("ok".to_string()),
        CheckResult::Pass("ok".to_string())
    );
}

#[test]
fn check_result_fail_equality() {
    assert_eq!(
        CheckResult::Fail {
            reason: "bad".to_string(),
            hint: None
        },
        CheckResult::Fail {
            reason: "bad".to_string(),
            hint: None
        }
    );
}

// ── Codex install checks ──────────────────────────────────────────────────────

/// When `~/.codex/` does not exist (e.g. on a machine without Codex),
/// all three Codex checks must return `Info`, never `Fail`.
/// Codex is optional — users who don't have it installed should not see a
/// red failure banner.
#[test]
fn codex_checks_use_info_not_fail_when_missing() {
    // Point CODEX_HOME at a temp dir that doesn't actually contain ~/.codex/.
    // Use a unique suffix so parallel test runs don't stomp each other.
    let tmp = std::env::temp_dir().join(format!(
        "sessync_doctor_codex_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    // Deliberately do NOT create `tmp` — we want the path to be absent.

    // Override CODEX_HOME so the doctor functions use our non-existent path.
    // Safety: test is single-threaded with respect to this env var since each
    // test binary runs in its own process when using `cargo test`.
    // We restore the original value after the assertions.
    let prev = std::env::var("CODEX_HOME").ok();
    std::env::set_var("CODEX_HOME", &tmp);

    // All three checks must return Info (not Fail) when Codex is absent.
    let dir_result = check_codex_dir_exists();
    assert!(
        matches!(dir_result, CheckResult::Info(_)),
        "codex_dir_exists must be Info when ~/.codex/ absent, got: {dir_result:?}"
    );
    assert!(
        !matches!(dir_result, CheckResult::Fail { .. }),
        "codex_dir_exists must never be Fail — Codex is optional"
    );

    // sqlite and binary checks should also be Info (or skipped-Info) when dir absent.
    let sqlite_result = check_codex_sqlite_present();
    assert!(
        !matches!(sqlite_result, CheckResult::Fail { .. }),
        "codex_sqlite_present must never be Fail — Codex is optional, got: {sqlite_result:?}"
    );

    let binary_result = check_codex_binary_reachable();
    assert!(
        !matches!(binary_result, CheckResult::Fail { .. }),
        "codex_binary_reachable must never be Fail — Codex is optional, got: {binary_result:?}"
    );

    // Restore env.
    match prev {
        Some(v) => std::env::set_var("CODEX_HOME", v),
        None => std::env::remove_var("CODEX_HOME"),
    }
}
