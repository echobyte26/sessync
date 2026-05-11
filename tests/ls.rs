//! Integration-level tests for `sessync ls` formatters.
//!
//! The `run()` function requires a live config + storage + keychain, so we
//! test the two pure formatter functions (`format_text_output` and
//! `format_json_output`) directly. They contain all the interesting logic.

use sessync::commands::ls::{format_json_output, format_text_output};
use sessync::types::{ProjectKey, SessionId, SessionMeta};
use chrono::TimeZone;

// ── helpers ───────────────────────────────────────────────────────────────────

fn ts(y: i32, mo: u32, d: u32, h: u32, m: u32) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc
        .with_ymd_and_hms(y, mo, d, h, m, 0)
        .single()
        .unwrap()
}

fn make_meta(
    session_id: &str,
    project_key: &str,
    source_cwd: &str,
    source_hostname: &str,
    preview: &str,
    modified_at: chrono::DateTime<chrono::Utc>,
) -> SessionMeta {
    SessionMeta {
        schema_version: 1,
        session_id: SessionId(session_id.to_string()),
        project_key: ProjectKey(project_key.to_string()),
        source_cwd: source_cwd.to_string(),
        source_hostname: source_hostname.to_string(),
        modified_at,
        byte_size: 512,
        preview: preview.to_string(),
    }
}

// ── text output tests ─────────────────────────────────────────────────────────

#[test]
fn text_output_handles_empty_input() {
    let out = format_text_output(&[]);
    assert_eq!(out.trim(), "No sessions found.");
}

#[test]
fn text_output_groups_by_project_with_indented_sessions() {
    let metas = vec![
        (
            "a3f9abcd".to_string(),
            vec![
                make_meta(
                    "s1",
                    "a3f9abcd",
                    "/Users/foo/projectA",
                    "Mac mini",
                    "running cargo build with full test suite, hits OOM",
                    ts(2026, 5, 6, 14, 23),
                ),
                make_meta(
                    "s2",
                    "a3f9abcd",
                    "/Users/foo/projectA",
                    "Mac pro",
                    "tweak the readme intro line",
                    ts(2026, 5, 6, 9, 11),
                ),
            ],
        ),
        (
            "e7c2bcde".to_string(),
            vec![make_meta(
                "s3",
                "e7c2bcde",
                "/Users/bar/projectB",
                "Mac pro",
                "explore the new clap derive style",
                ts(2026, 5, 6, 11, 0),
            )],
        ),
    ];

    let out = format_text_output(&metas);

    // Project headers present.
    assert!(out.contains("/Users/foo/projectA"), "proj A header: {out}");
    assert!(out.contains("/Users/bar/projectB"), "proj B header: {out}");

    // Session lines present.
    assert!(
        out.contains("running cargo build"),
        "session preview: {out}"
    );
    assert!(out.contains("tweak the readme"), "session preview: {out}");
    assert!(
        out.contains("explore the new clap"),
        "session preview: {out}"
    );

    // Hostnames present.
    assert!(out.contains("Mac mini"), "hostname: {out}");
    assert!(out.contains("Mac pro"), "hostname: {out}");

    // Sessions are indented (each session line starts with spaces).
    let session_lines: Vec<&str> = out
        .lines()
        .filter(|l| l.contains("2026-05-06"))
        .collect();
    assert!(!session_lines.is_empty(), "expected session lines with timestamp");
    for line in &session_lines {
        assert!(
            line.starts_with("    "),
            "session line should be indented: {line:?}"
        );
    }
}

#[test]
fn text_output_includes_project_header_count() {
    let metas = vec![(
        "proj-x".to_string(),
        vec![
            make_meta("s1", "proj-x", "/cwd", "h", "p1", ts(2026, 5, 6, 3, 0)),
            make_meta("s2", "proj-x", "/cwd", "h", "p2", ts(2026, 5, 5, 3, 0)),
            make_meta("s3", "proj-x", "/cwd", "h", "p3", ts(2026, 5, 4, 3, 0)),
        ],
    )];

    let out = format_text_output(&metas);
    assert!(out.contains("3 sessions"), "project session count: {out}");
    assert!(out.contains("1 project"), "top-level project count: {out}");
    assert!(out.contains("3 sessions"), "top-level session count: {out}");
}

#[test]
fn text_output_truncates_long_preview() {
    // 201-char preview — must be truncated to 199 chars + ellipsis (≤200 total graphemes).
    let long_preview: String = "a".repeat(201);
    let metas = vec![(
        "p".to_string(),
        vec![make_meta(
            "s1",
            "p",
            "/cwd",
            "host",
            &long_preview,
            ts(2026, 5, 6, 0, 0),
        )],
    )];

    let out = format_text_output(&metas);

    // The full 201-char string should NOT appear verbatim.
    assert!(
        !out.contains(&long_preview),
        "full preview should not appear untruncated: {out}"
    );
    // Ellipsis (U+2026) must appear in its place.
    assert!(out.contains('\u{2026}'), "expected ellipsis: {out}");
}

// ── JSON output tests ─────────────────────────────────────────────────────────

#[test]
fn json_output_is_parseable_back_into_a_struct() {
    let metas = vec![
        (
            "a3f9".to_string(),
            vec![make_meta(
                "sess-uuid-1",
                "a3f9",
                "/Users/foo/projectA",
                "Mac mini",
                "running cargo build",
                ts(2026, 5, 6, 14, 23),
            )],
        ),
        (
            "e7c2".to_string(),
            vec![
                make_meta(
                    "sess-uuid-2",
                    "e7c2",
                    "/Users/bar/projectB",
                    "Mac pro",
                    "explore clap derive",
                    ts(2026, 5, 6, 11, 0),
                ),
                make_meta(
                    "sess-uuid-3",
                    "e7c2",
                    "/Users/bar/projectB",
                    "Mac mini",
                    "starter session",
                    ts(2026, 5, 5, 23, 40),
                ),
            ],
        ),
    ];

    let json_str = format_json_output(&metas);

    // Must be valid JSON.
    let parsed: serde_json::Value =
        serde_json::from_str(&json_str).expect("must produce valid JSON");

    // New shape (v0.6.0): {"tools":[{"name":"claude-code","projects":[…]}]}
    let tools = parsed["tools"].as_array().expect("root must have 'tools' array");
    assert_eq!(tools.len(), 1, "single-tool call wraps in one tool entry");
    assert_eq!(tools[0]["name"], "claude-code");

    let projects = tools[0]["projects"]
        .as_array()
        .expect("tool must have 'projects' array");
    assert_eq!(projects.len(), 2, "expected 2 projects");

    // Project A.
    assert_eq!(projects[0]["key"], "a3f9");
    assert_eq!(projects[0]["source_cwd"], "/Users/foo/projectA");
    let sa = projects[0]["sessions"]
        .as_array()
        .expect("sessions array");
    assert_eq!(sa.len(), 1);
    assert_eq!(sa[0]["id"], "sess-uuid-1");
    assert_eq!(sa[0]["source_hostname"], "Mac mini");
    assert_eq!(sa[0]["preview"], "running cargo build");

    // modified_at is RFC3339.
    let mat = sa[0]["modified_at"].as_str().unwrap();
    assert!(
        mat.starts_with("2026-05-06"),
        "modified_at should be RFC3339: {mat}"
    );

    // Project B — two sessions.
    assert_eq!(projects[1]["key"], "e7c2");
    let sb = projects[1]["sessions"]
        .as_array()
        .expect("sessions array");
    assert_eq!(sb.len(), 2);
    assert_eq!(sb[0]["id"], "sess-uuid-2");
    assert_eq!(sb[1]["id"], "sess-uuid-3");
}

#[test]
fn json_output_empty_projects_has_correct_shape() {
    let out = format_json_output(&[]);
    let parsed: serde_json::Value =
        serde_json::from_str(&out).expect("must be valid JSON");

    // New shape: {"tools":[{"name":"claude-code","projects":[]}]}
    let tools = parsed["tools"].as_array().expect("tools must be array");
    assert_eq!(tools.len(), 1, "even empty output wraps in one tool entry");
    let projects = tools[0]["projects"]
        .as_array()
        .expect("projects must be array");
    assert!(projects.is_empty(), "expected empty projects array");
}
