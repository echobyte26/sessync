// ─────────────────────────────────────────────────────────────────────────────
// Claude Code (JSON / settings.json) tests
// ─────────────────────────────────────────────────────────────────────────────
mod claude_code {
    use sessync::commands::hook;

    #[test]
    fn install_then_uninstall_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");

        hook::install_hook_at(&settings_path).unwrap();
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(
            content.contains("sessync push"),
            "expected 'sessync push' in settings after install, got:\n{content}"
        );

        hook::uninstall_hook_at(&settings_path).unwrap();
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(
            !content.contains("sessync push"),
            "expected no 'sessync push' in settings after uninstall, got:\n{content}"
        );
    }

    #[test]
    fn install_preserves_other_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [{"type":"command","command":"echo other"}]}]
            }
        }"#,
        )
        .unwrap();

        hook::install_hook_at(&settings_path).unwrap();

        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(
            content.contains("echo other"),
            "user's existing hook must be preserved, got:\n{content}"
        );
        assert!(
            content.contains("sessync push"),
            "sessync hook must be added, got:\n{content}"
        );
    }

    #[test]
    fn install_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");

        hook::install_hook_at(&settings_path).unwrap();
        hook::install_hook_at(&settings_path).unwrap(); // second call must no-op

        let content = std::fs::read_to_string(&settings_path).unwrap();
        let count = content.matches("sessync push").count();
        assert_eq!(
            count, 1,
            "should only have one 'sessync push' hook entry, found {count}:\n{content}"
        );
    }

    #[test]
    fn install_into_existing_settings_preserves_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"enabledPlugins": {"my-plugin": true}, "theme": "dark"}"#,
        )
        .unwrap();

        hook::install_hook_at(&settings_path).unwrap();

        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(
            content.contains("enabledPlugins"),
            "top-level keys must be preserved, got:\n{content}"
        );
        assert!(
            content.contains("dark"),
            "theme key must be preserved, got:\n{content}"
        );
        assert!(
            content.contains("sessync push"),
            "sessync hook must be added, got:\n{content}"
        );
    }

    #[test]
    fn uninstall_only_removes_sessync_hook_leaves_others() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{
            "hooks": {
                "Stop": [
                    {"matcher": "", "hooks": [{"type":"command","command":"echo other"}]},
                    {"matcher": "", "hooks": [{"type":"command","command":"sessync push --quiet # sessync-auto-push"}]}
                ]
            }
        }"#,
        )
        .unwrap();

        hook::uninstall_hook_at(&settings_path).unwrap();

        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(
            content.contains("echo other"),
            "other hooks must survive uninstall, got:\n{content}"
        );
        assert!(
            !content.contains("sessync push"),
            "sessync hook must be removed, got:\n{content}"
        );
    }

    #[test]
    fn status_returns_false_when_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");

        let installed = hook::status_hook_at(&settings_path).unwrap();
        assert!(!installed);
    }

    #[test]
    fn status_returns_true_when_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");

        hook::install_hook_at(&settings_path).unwrap();
        let installed = hook::status_hook_at(&settings_path).unwrap();
        assert!(installed);
    }

    /// Test the new tool-agnostic API with "claude-code".
    /// We can't call install_for_tool directly (it resolves $HOME), so we
    /// verify the low-level helpers still compile and work via install_hook_at.
    #[test]
    fn install_for_tool_delegates_to_install_hook_at() {
        // This test verifies the low-level path still works when called
        // from non-tool-aware code (e.g. auto_push::setup legacy path).
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        hook::install_hook_at(&settings_path).unwrap();
        let installed = hook::status_hook_at(&settings_path).unwrap();
        assert!(installed, "install_hook_at should result in status=true");
    }

    /// v0.9.9: migrate_hook_to_absolute_path detects an old bare-name
    /// install (e.g. v0.9.7-style `sessync push --quiet # sessync-auto-push`)
    /// and rewrites it to the absolute-path form, preserving the rest of
    /// settings.json unchanged.  Idempotent — running twice is a no-op.
    #[test]
    fn migrate_bare_name_hook_to_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");

        // Seed an old-style bare-name install + an unrelated user hook.
        std::fs::write(
            &settings_path,
            r#"{
              "hooks": {
                "Stop": [
                  {"hooks": [{"type":"command","command":"echo user-hook"}]},
                  {"matcher": "", "hooks": [{"type":"command","command":"sessync push --quiet # sessync-auto-push"}]}
                ]
              },
              "unrelated_top_level": "preserve me"
            }"#,
        )
        .unwrap();

        // First migration: should rewrite.
        let migrated = hook::migrate_hook_to_absolute_path(&settings_path).unwrap();
        assert!(migrated, "first migration should report change");

        let after = std::fs::read_to_string(&settings_path).unwrap();
        // The sessync entry was rewritten — bare name is gone, an absolute path is in.
        assert!(
            !after.contains("\"sessync push --quiet # sessync-auto-push\""),
            "bare-name command should be removed; got:\n{after}"
        );
        assert!(
            after.contains("/sessync push --quiet # sessync-auto-push"),
            "absolute path form should be present; got:\n{after}"
        );
        // Other hook and unrelated key preserved.
        assert!(after.contains("echo user-hook"), "user hook must be preserved");
        assert!(after.contains("unrelated_top_level"), "unrelated top-level must be preserved");

        // Second migration: idempotent, no rewrite.
        let migrated_again = hook::migrate_hook_to_absolute_path(&settings_path).unwrap();
        assert!(!migrated_again, "second migration should be a no-op");
    }

    /// v0.9.9: migration is a no-op when settings.json doesn't have the
    /// sessync hook installed.
    #[test]
    fn migrate_no_op_when_hook_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"hooks": {"Stop": [{"hooks": [{"type":"command","command":"echo only-user"}]}]}}"#,
        )
        .unwrap();

        let migrated = hook::migrate_hook_to_absolute_path(&settings_path).unwrap();
        assert!(!migrated, "should not migrate when no sessync hook is present");
    }
}
