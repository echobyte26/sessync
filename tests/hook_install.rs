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
