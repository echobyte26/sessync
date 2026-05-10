//! Integration tests for the launchd plist install/uninstall/status helpers.
//!
//! These tests exercise only filesystem operations — no real `launchctl` calls
//! are made (enable_launchctl = false / write_plist_at only).

#[cfg(target_os = "macos")]
mod tests {
    use sessync::commands::launchd;
    use std::path::Path;

    // ── helpers ────────────────────────────────────────────────────────────────

    fn fake_binary(tmp: &Path) -> std::path::PathBuf {
        let p = tmp.join("fake-sessync");
        std::fs::write(&p, b"#!/bin/sh\n").unwrap();
        p
    }

    // ── tests ──────────────────────────────────────────────────────────────────

    #[test]
    fn install_at_writes_plist_with_binary_path() {
        let tmp = tempfile::tempdir().unwrap();
        let plist_path = tmp.path().join("com.sessync.push.plist");
        let binary = fake_binary(tmp.path());
        let log_dir = tmp.path().join("Logs").join("sessync");

        // Pass enable_launchctl=false so we don't shell out in CI.
        launchd::install_at(&plist_path, &binary, &log_dir, false).unwrap();

        let content = std::fs::read_to_string(&plist_path).unwrap();

        // Binary path must appear verbatim.
        assert!(
            content.contains(binary.to_str().unwrap()),
            "plist must contain the binary path, got:\n{content}"
        );

        // StartInterval must be 1800.
        assert!(
            content.contains("<integer>1800</integer>"),
            "plist must set StartInterval to 1800, got:\n{content}"
        );

        // Log paths must appear.
        assert!(
            content.contains("launchd.out.log"),
            "plist must reference stdout log, got:\n{content}"
        );
        assert!(
            content.contains("launchd.err.log"),
            "plist must reference stderr log, got:\n{content}"
        );

        // RunAtLoad must be false.
        assert!(
            content.contains("<false/>"),
            "plist must set RunAtLoad to false, got:\n{content}"
        );

        // Label must be correct.
        assert!(
            content.contains("com.sessync.push"),
            "plist must have label com.sessync.push, got:\n{content}"
        );

        // push + --quiet must appear as separate arguments.
        assert!(
            content.contains("<string>push</string>"),
            "plist must have push argument, got:\n{content}"
        );
        assert!(
            content.contains("<string>--quiet</string>"),
            "plist must have --quiet argument, got:\n{content}"
        );
    }

    #[test]
    fn install_at_creates_log_dir_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let plist_path = tmp.path().join("agents").join("com.sessync.push.plist");
        let binary = fake_binary(tmp.path());
        let log_dir = tmp.path().join("deeply").join("nested").join("logs");

        assert!(!log_dir.exists(), "log dir should not exist before install");

        launchd::install_at(&plist_path, &binary, &log_dir, false).unwrap();

        assert!(log_dir.exists(), "install_at must create the log directory");
        assert!(plist_path.exists(), "install_at must create the plist file");
    }

    #[test]
    fn uninstall_at_removes_plist() {
        let tmp = tempfile::tempdir().unwrap();
        let plist_path = tmp.path().join("com.sessync.push.plist");
        let binary = fake_binary(tmp.path());
        let log_dir = tmp.path().join("logs");

        launchd::install_at(&plist_path, &binary, &log_dir, false).unwrap();
        assert!(plist_path.exists(), "plist must exist after install");

        launchd::uninstall_at(&plist_path).unwrap();
        assert!(!plist_path.exists(), "plist must be removed after uninstall");
    }

    #[test]
    fn uninstall_at_is_idempotent_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let plist_path = tmp.path().join("com.sessync.push.plist");

        // Should succeed even though the file was never created.
        launchd::uninstall_at(&plist_path).unwrap();
        // Calling it again is still fine.
        launchd::uninstall_at(&plist_path).unwrap();
    }

    #[test]
    fn status_at_returns_true_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let plist_path = tmp.path().join("com.sessync.push.plist");
        let binary = fake_binary(tmp.path());
        let log_dir = tmp.path().join("logs");

        launchd::install_at(&plist_path, &binary, &log_dir, false).unwrap();
        let present = launchd::status_at(&plist_path).unwrap();
        assert!(present, "status_at must return true when plist exists");
    }

    #[test]
    fn status_at_returns_false_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let plist_path = tmp.path().join("com.sessync.push.plist");

        let present = launchd::status_at(&plist_path).unwrap();
        assert!(!present, "status_at must return false when plist is absent");
    }

    #[test]
    fn install_at_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let plist_path = tmp.path().join("com.sessync.push.plist");
        let binary = fake_binary(tmp.path());
        let log_dir = tmp.path().join("logs");

        launchd::install_at(&plist_path, &binary, &log_dir, false).unwrap();
        launchd::install_at(&plist_path, &binary, &log_dir, false).unwrap();

        // Second call must simply overwrite — the plist is still a single valid file.
        assert!(plist_path.exists(), "plist must still exist after second install");
        let content = std::fs::read_to_string(&plist_path).unwrap();
        // Exactly one StartInterval key — not doubled.
        assert_eq!(
            content.matches("<key>StartInterval</key>").count(),
            1,
            "plist must have exactly one StartInterval key after idempotent reinstall"
        );
    }

    #[test]
    fn write_plist_at_produces_valid_xml_prologue() {
        let tmp = tempfile::tempdir().unwrap();
        let plist_path = tmp.path().join("com.sessync.push.plist");
        let binary = fake_binary(tmp.path());
        let log_dir = tmp.path().join("logs");

        launchd::write_plist_at(&plist_path, &binary, &log_dir).unwrap();

        let content = std::fs::read_to_string(&plist_path).unwrap();
        assert!(
            content.starts_with("<?xml version=\"1.0\""),
            "plist must start with XML declaration, got:\n{content}"
        );
        assert!(
            content.contains("<!DOCTYPE plist"),
            "plist must contain DOCTYPE, got:\n{content}"
        );
    }

    // ── Task A: bootstrap/bootout arg-shape tests ──────────────────────────────

    #[test]
    fn bootstrap_args_contains_bootstrap_and_gui_uid_and_plist() {
        let plist = std::path::Path::new("/tmp/com.sessync.push.plist");
        let args = launchd::bootstrap_args(501, plist);

        assert_eq!(args[0], "bootstrap", "first arg must be 'bootstrap'");
        assert_eq!(args[1], "gui/501", "second arg must be 'gui/<uid>'");
        assert_eq!(
            args[2],
            plist.display().to_string(),
            "third arg must be the plist path"
        );
        assert_eq!(args.len(), 3, "bootstrap takes exactly 3 args");
    }

    #[test]
    fn bootout_args_contains_bootout_and_service_identifier() {
        let args = launchd::bootout_args(501);

        assert_eq!(args[0], "bootout", "first arg must be 'bootout'");
        assert_eq!(
            args[1], "gui/501/com.sessync.push",
            "second arg must be the full service identifier 'gui/<uid>/<label>'"
        );
        assert_eq!(args.len(), 2, "bootout takes exactly 2 args (no plist path)");
    }

    #[test]
    fn bootstrap_args_uses_provided_uid() {
        let plist = std::path::Path::new("/tmp/test.plist");
        let args_0 = launchd::bootstrap_args(0, plist);
        let args_1000 = launchd::bootstrap_args(1000, plist);

        assert!(args_0[1].contains("/0"), "uid 0 must appear in domain");
        assert!(args_1000[1].contains("/1000"), "uid 1000 must appear in domain");
    }

    // ── Task B: resolve_binary_path fallback test ──────────────────────────────

    #[test]
    fn resolve_binary_path_falls_back_to_current_exe_when_no_brew() {
        // In CI / test environment neither /opt/homebrew/bin/sessync nor
        // /usr/local/bin/sessync exist (we are running from a test binary), and
        // $HOME/.local/bin/sessync likely doesn't exist either.  The function
        // must still succeed by falling back to current_exe().
        let result = launchd::resolve_binary_path();
        assert!(
            result.is_ok(),
            "resolve_binary_path must not fail even without brew install: {:?}",
            result.err()
        );
        let path = result.unwrap();
        assert!(path.is_absolute(), "resolved binary path must be absolute, got: {path:?}");
    }
}
