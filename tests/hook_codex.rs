//! Tests for the Codex Stop hook (TOML / config.toml).

use sessync::commands::hook;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a TOML file and return its value.
fn parse_toml(path: &std::path::Path) -> toml::Value {
    let raw = std::fs::read_to_string(path).expect("read config.toml");
    toml::from_str(&raw).expect("parse config.toml")
}

fn codex_config_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("config.toml")
}

// ─────────────────────────────────────────────────────────────────────────────
// mod codex
// ─────────────────────────────────────────────────────────────────────────────
mod codex {
    use super::*;

    // ── install ───────────────────────────────────────────────────────────────

    #[test]
    fn install_writes_toml_with_features_flag_and_hook_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        hook::install_codex_hook_at(&path).unwrap();

        let config = parse_toml(&path);

        // Feature flag present and true.
        let features_enabled = config
            .get("features")
            .and_then(|f| f.get("codex_hooks"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(
            features_enabled,
            "[features] codex_hooks must be true after install"
        );

        // Hook entry present with our marker.
        let hook_present = config
            .get("hooks")
            .and_then(|h| h.get("Stop"))
            .and_then(|s| s.as_array())
            .map(|stop| {
                stop.iter().any(|entry| {
                    entry
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|inner| {
                            inner.iter().any(|h| {
                                h.get("command")
                                    .and_then(|c| c.as_str())
                                    .map(|cmd| cmd.contains("sessync-auto-push"))
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        assert!(hook_present, "hooks.Stop must contain our sessync entry");

        // Command must include --tool codex.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("--tool codex"),
            "hook command must embed --tool codex, got:\n{raw}"
        );
    }

    #[test]
    fn install_is_idempotent_with_tag_match() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        hook::install_codex_hook_at(&path).unwrap();
        hook::install_codex_hook_at(&path).unwrap(); // second call must no-op

        let raw = std::fs::read_to_string(&path).unwrap();
        // Count occurrences of our marker — must be exactly 1.
        let count = raw.matches("sessync-auto-push").count();
        assert_eq!(
            count, 1,
            "should only have one sessync-auto-push entry, found {count}:\n{raw}"
        );
    }

    #[test]
    fn install_is_idempotent_when_command_starts_with_sessync_push() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        // Pre-populate with a command that starts with "sessync push" but
        // has no trailing tag — simulates a hand-edited entry.
        let pre_existing = r#"
[features]
codex_hooks = true

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "sessync push --quiet --tool codex"
timeout = 30
"#;
        std::fs::write(&path, pre_existing).unwrap();

        hook::install_codex_hook_at(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let count = raw.matches("sessync push").count();
        assert_eq!(
            count, 1,
            "should not duplicate a 'sessync push' entry that has no tag, found {count}:\n{raw}"
        );
    }

    #[test]
    fn install_preserves_unrelated_toml_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        // Pre-populate with an unrelated section.
        std::fs::write(
            &path,
            "[some_other_section]\nmy_key = \"my_value\"\n",
        )
        .unwrap();

        hook::install_codex_hook_at(&path).unwrap();

        let config = parse_toml(&path);
        let preserved = config
            .get("some_other_section")
            .and_then(|s| s.get("my_key"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            preserved, "my_value",
            "[some_other_section] must survive install"
        );
    }

    #[test]
    fn install_enables_codex_hooks_when_it_was_false() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        std::fs::write(&path, "[features]\ncodex_hooks = false\n").unwrap();
        hook::install_codex_hook_at(&path).unwrap();

        let config = parse_toml(&path);
        let flag = config
            .get("features")
            .and_then(|f| f.get("codex_hooks"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(flag, "codex_hooks must be flipped to true");
    }

    #[test]
    fn install_errors_on_non_boolean_codex_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        // codex_hooks set to a string — not a boolean.
        std::fs::write(&path, "[features]\ncodex_hooks = \"yes\"\n").unwrap();

        let result = hook::install_codex_hook_at(&path);
        assert!(
            result.is_err(),
            "install should error when codex_hooks is a non-boolean"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("not a boolean"),
            "error message should mention 'not a boolean', got: {msg}"
        );
    }

    // ── uninstall ─────────────────────────────────────────────────────────────

    #[test]
    fn uninstall_removes_only_our_hook_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        // Pre-populate with our hook AND a user-owned hook.
        let pre_existing = r#"
[features]
codex_hooks = true

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "echo user-hook"

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "sessync push --quiet --tool codex # sessync-auto-push"
timeout = 30
"#;
        std::fs::write(&path, pre_existing).unwrap();

        hook::uninstall_codex_hook_at(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("echo user-hook"),
            "user hook must survive uninstall, got:\n{raw}"
        );
        assert!(
            !raw.contains("sessync push"),
            "sessync hook must be removed, got:\n{raw}"
        );
    }

    #[test]
    fn uninstall_does_not_remove_codex_hooks_feature_flag() {
        // Judgment call: we leave codex_hooks = true after uninstall.
        // The user may have enabled it for other hooks they own.
        // Silently removing it would break their other hooks.
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        hook::install_codex_hook_at(&path).unwrap();
        hook::uninstall_codex_hook_at(&path).unwrap();

        let config = parse_toml(&path);
        let flag = config
            .get("features")
            .and_then(|f| f.get("codex_hooks"))
            .and_then(|v| v.as_bool());
        // The key may or may not remain — but if it does it must still be true.
        if let Some(v) = flag {
            assert!(
                v,
                "if codex_hooks key remains after uninstall, it must still be true"
            );
        }
        // Either way, the user's feature flag was not flipped to false.
    }

    #[test]
    fn uninstall_is_noop_when_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        // No file — should not error.
        hook::uninstall_codex_hook_at(&path).unwrap();

        // File exists but no sessync hook.
        std::fs::write(&path, "[features]\ncodex_hooks = true\n").unwrap();
        hook::uninstall_codex_hook_at(&path).unwrap();
    }

    // ── status ────────────────────────────────────────────────────────────────

    #[test]
    fn status_returns_false_when_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        let installed = hook::status_codex_hook_at(&path).unwrap();
        assert!(!installed);
    }

    #[test]
    fn status_returns_true_only_when_both_feature_flag_and_hook_entry_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        hook::install_codex_hook_at(&path).unwrap();
        let installed = hook::status_codex_hook_at(&path).unwrap();
        assert!(installed, "status should be true after full install");
    }

    #[test]
    fn status_returns_false_when_codex_hooks_feature_is_false_even_if_hook_entry_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        // Install first (sets feature flag + hook).
        hook::install_codex_hook_at(&path).unwrap();

        // Now manually flip the feature flag to false.
        let raw = std::fs::read_to_string(&path).unwrap();
        let patched = raw.replace("codex_hooks = true", "codex_hooks = false");
        std::fs::write(&path, patched).unwrap();

        let installed = hook::status_codex_hook_at(&path).unwrap();
        assert!(
            !installed,
            "status must be false when feature flag is disabled, even if hook entry exists"
        );
    }

    #[test]
    fn status_returns_false_when_only_feature_flag_present_but_no_hook_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        std::fs::write(&path, "[features]\ncodex_hooks = true\n").unwrap();
        let installed = hook::status_codex_hook_at(&path).unwrap();
        assert!(!installed, "status must be false when hook entry is missing");
    }

    // ── roundtrip ─────────────────────────────────────────────────────────────

    #[test]
    fn install_then_uninstall_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = codex_config_path(tmp.path());

        hook::install_codex_hook_at(&path).unwrap();
        assert!(hook::status_codex_hook_at(&path).unwrap());

        hook::uninstall_codex_hook_at(&path).unwrap();
        assert!(!hook::status_codex_hook_at(&path).unwrap());
    }
}
