//! Best-effort macOS desktop notification via osascript.
//! On non-macOS platforms the function is a silent no-op.

/// Replace double quotes so osascript doesn't break its quoted string literals.
pub fn sanitize_for_osascript(s: &str) -> String {
    s.replace('"', "'")
}

/// Fire a macOS notification. Never panics, never returns an error —
/// notification failure is not worth aborting a push for.
pub fn notify(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        let t = sanitize_for_osascript(title);
        let b = sanitize_for_osascript(body);
        let script = format!(r#"display notification "{b}" with title "{t}""#);
        // Best-effort — ignore exit code and stderr.
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .output();
    }
    // On non-macOS: intentional no-op.
    #[cfg(not(target_os = "macos"))]
    let _ = (title, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_does_not_panic_on_macos() {
        // Just verifies no panic — we can't assert the banner appeared.
        notify("test title", "test body");
    }

    #[test]
    fn notify_sanitizes_double_quotes() {
        assert_eq!(
            sanitize_for_osascript(r#"he said "hello""#),
            "he said 'hello'"
        );
        assert_eq!(sanitize_for_osascript("no quotes here"), "no quotes here");
        assert_eq!(sanitize_for_osascript(""), "");
    }
}
