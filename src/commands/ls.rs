//! `sessync ls` — non-interactive listing of remote sessions.
//!
//! Text mode: groups sessions by project, sorted by recency (same order as
//! the resume project picker). Within each project sessions are sorted by
//! mtime DESC. Accepts an optional `--project <key>` filter and a `--json`
//! flag for machine-readable output.

use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::{StorageAdapter, StorageObject};
use crate::adapter::tool::ToolAdapter;
use crate::cache::{self, MetaCache};
use crate::config::{Config, StorageKind};
use crate::crypto;
use crate::passphrase_store;
use crate::types::SessionMeta;
use crate::ui::style;
use anyhow::{Context, Result};
use chrono::DateTime;
use futures::stream::{self, StreamExt, TryStreamExt};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

// ── JSON output types ────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct JsonOutput {
    pub projects: Vec<JsonProject>,
}

#[derive(Debug, Serialize)]
pub struct JsonProject {
    pub key: String,
    pub source_cwd: String,
    pub sessions: Vec<JsonSession>,
}

#[derive(Debug, Serialize)]
pub struct JsonSession {
    pub id: String,
    pub modified_at: String,
    pub preview: String,
    pub source_hostname: String,
}

// ── entry point ───────────────────────────────────────────────────────────────

pub async fn run(project: Option<String>, json: bool) -> Result<()> {
    let cfg = Config::load(&Config::default_path()).context("load config")?;
    let passphrase = passphrase_store::load_passphrase()?;
    let salt = crypto::decode_salt_hex(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;

    let tool = ClaudeCodeAdapter::new();

    match cfg.storage_kind {
        StorageKind::Oss => {
            let oss = cfg
                .oss
                .as_ref()
                .context("storage_kind = oss but [oss] section missing")?;
            let storage = OssStorage::new(oss)?;
            list_sessions(&tool, &storage, &key, project, json).await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            list_sessions(&tool, &storage, &key, project, json).await
        }
    }
}

// ── core listing logic ────────────────────────────────────────────────────────

pub async fn list_sessions<T: ToolAdapter, S: StorageAdapter>(
    tool: &T,
    storage: &S,
    key: &[u8; 32],
    project_filter: Option<String>,
    json: bool,
) -> Result<()> {
    let prefix = format!("{}/", tool.name());
    let objects = storage.list(&prefix).await?;

    // Load cache (same as resume — reuse any already-cached entries).
    let cache_path: Option<PathBuf> = cache::default_cache_path().ok();
    let mut meta_cache = match &cache_path {
        Some(p) => MetaCache::load_or_empty(p, key, tool.name()),
        None => {
            tracing::debug!("HOME not set — running without meta cache");
            MetaCache::empty(tool.name())
        }
    };

    // Build an owned object index: key → (mtime, size).
    let object_index: HashMap<String, (DateTime<chrono::Utc>, u64)> = objects
        .iter()
        .map(|o| (o.key.clone(), (o.last_modified, o.size)))
        .collect();

    // Tombstone: drop cache entries whose key no longer exists remotely.
    let present_keys: HashSet<&str> = object_index.keys().map(|k| k.as_str()).collect();
    meta_cache.retain_only(&present_keys);

    // Group .meta.json objects by project key.
    let mut by_project: BTreeMap<String, Vec<StorageObject>> = BTreeMap::new();
    for o in &objects {
        if !o.key.ends_with(".meta.json") {
            continue;
        }
        let parts: Vec<&str> = o.key.splitn(3, '/').collect();
        if parts.len() < 3 {
            continue;
        }
        by_project
            .entry(parts[1].to_string())
            .or_default()
            .push(o.clone());
    }

    if by_project.is_empty() {
        if json {
            println!("{{\"projects\":[]}}");
        } else {
            println!("No sessions found.");
        }
        return Ok(());
    }

    // Sort projects by recency (latest mtime DESC) — same as resume.
    let mut projects_sorted: Vec<(String, Vec<StorageObject>)> = by_project.into_iter().collect();
    projects_sorted.sort_by(|(_, a_objs), (_, b_objs)| {
        let epoch = DateTime::from_timestamp(0, 0).unwrap_or(chrono::Utc::now());
        let a_max = a_objs
            .iter()
            .map(|o| o.last_modified)
            .max()
            .unwrap_or(epoch);
        let b_max = b_objs
            .iter()
            .map(|o| o.last_modified)
            .max()
            .unwrap_or(epoch);
        b_max.cmp(&a_max)
    });

    // Apply project filter if requested.
    if let Some(ref filter) = project_filter {
        projects_sorted.retain(|(pk, _)| pk == filter);
        if projects_sorted.is_empty() {
            if json {
                println!("{{\"projects\":[]}}");
            } else {
                println!("No sessions found for project '{filter}'.");
            }
            return Ok(());
        }
    }

    // Collect all .meta.json keys that are cache misses across all projects.
    let all_misses: Vec<(String, DateTime<chrono::Utc>, u64)> = projects_sorted
        .iter()
        .flat_map(|(_, objs)| objs.iter())
        .filter_map(|obj| {
            let (mtime, size) = object_index[&obj.key];
            if meta_cache.get_if_fresh(&obj.key, mtime, size).is_some() {
                None
            } else {
                Some((obj.key.clone(), mtime, size))
            }
        })
        .collect();

    // Fetch all misses concurrently (buffered, like resume).
    let fetched: Vec<(String, SessionMeta, DateTime<chrono::Utc>, u64)> =
        stream::iter(all_misses)
            .map(|(mk, mtime, size)| async move {
                let raw = storage.get(&mk).await?;
                let pt = crypto::decrypt(&raw, key)?;
                let meta: SessionMeta = serde_json::from_slice(&pt)?;
                anyhow::Ok((mk, meta, mtime, size))
            })
            .buffered(8)
            .try_collect()
            .await?;

    for (mk, meta, mtime, size) in fetched {
        meta_cache.insert(mk, meta, mtime, size);
    }

    // Build the sorted (project_key, Vec<SessionMeta>) structure.
    let metas_by_project: Vec<(String, Vec<SessionMeta>)> = projects_sorted
        .iter()
        .map(|(pk, objs)| {
            // Sort sessions DESC by mtime.
            let mut session_objs: Vec<&StorageObject> = objs.iter().collect();
            session_objs.sort_by_key(|o| Reverse(o.last_modified));

            let metas: Vec<SessionMeta> = session_objs
                .iter()
                .map(|obj| {
                    let (mtime, size) = object_index[&obj.key];
                    meta_cache
                        .get_if_fresh(&obj.key, mtime, size)
                        .expect("just fetched or was already cached")
                        .clone()
                })
                .collect();
            (pk.clone(), metas)
        })
        .collect();

    // Save cache — best-effort, don't fail on error.
    if let Some(ref p) = cache_path {
        if let Err(e) = meta_cache.save(p, key) {
            tracing::debug!("meta-cache save failed (non-fatal): {e}");
        }
    }

    if json {
        println!("{}", format_json_output(&metas_by_project));
    } else {
        print!("{}", format_text_output(&metas_by_project));
    }

    Ok(())
}

// ── pure formatters (also used by tests) ─────────────────────────────────────

/// Render the full text listing.  Returns a `String` (newline-terminated).
pub fn format_text_output(metas_by_project: &[(String, Vec<SessionMeta>)]) -> String {
    if metas_by_project.is_empty() {
        return "No sessions found.\n".to_string();
    }

    let total_projects = metas_by_project.len();
    let total_sessions: usize = metas_by_project.iter().map(|(_, s)| s.len()).sum();

    let mut out = String::new();
    out.push_str(&style::header(&format!(
        "Remote sessions  \u{2500}\u{2500} total: {total_projects} project{}, {total_sessions} session{}\n",
        if total_projects == 1 { "" } else { "s" },
        if total_sessions == 1 { "" } else { "s" },
    )));
    out.push('\n');

    for (project_key, sessions) in metas_by_project {
        let count = sessions.len();
        // Use source_cwd from the first (most recent) session as the display label.
        let cwd = sessions
            .first()
            .map(|m| m.source_cwd.as_str())
            .unwrap_or("?");
        // Abbreviate the project key for display.
        let key_abbrev = &project_key[..project_key.len().min(8)];

        out.push_str(&format!(
            "{} {}  {}  {} session{}\n",
            style::value("\u{25b8}"),
            style::header(cwd),
            style::dim(&format!("({key_abbrev}…)")),
            style::value(&count.to_string()),
            if count == 1 { "" } else { "s" },
        ));

        for meta in sessions {
            let ts = meta
                .modified_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string();
            let preview = truncate(&meta.preview, 200);
            out.push_str(&format!(
                "    {}  {}  {} {}\n",
                style::dim(&format!("[{ts}]")),
                preview,
                style::dim("\u{2014}"),
                style::key(&meta.source_hostname),
            ));
        }
        out.push('\n');
    }

    out
}

/// Render machine-readable JSON.  Returns a pretty-printed `String`.
pub fn format_json_output(metas_by_project: &[(String, Vec<SessionMeta>)]) -> String {
    let projects: Vec<JsonProject> = metas_by_project
        .iter()
        .map(|(project_key, sessions)| {
            let source_cwd = sessions
                .first()
                .map(|m| m.source_cwd.clone())
                .unwrap_or_default();

            let json_sessions: Vec<JsonSession> = sessions
                .iter()
                .map(|m| JsonSession {
                    id: m.session_id.0.clone(),
                    modified_at: m.modified_at.to_rfc3339(),
                    preview: m.preview.clone(),
                    source_hostname: m.source_hostname.clone(),
                })
                .collect();

            JsonProject {
                key: project_key.clone(),
                source_cwd,
                sessions: json_sessions,
            }
        })
        .collect();

    let output = JsonOutput { projects };
    serde_json::to_string_pretty(&output).expect("JsonOutput is always serializable")
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "\u{2026}"
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ProjectKey, SessionId};
    use chrono::TimeZone;

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
            byte_size: 100,
            preview: preview.to_string(),
        }
    }

    fn ts(y: i32, mo: u32, d: u32, h: u32, m: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(y, mo, d, h, m, 0)
            .single()
            .unwrap()
    }

    // ── text output tests ─────────────────────────────────────────────────────

    #[test]
    fn text_output_handles_empty_input() {
        let output = format_text_output(&[]);
        assert_eq!(output.trim(), "No sessions found.");
    }

    #[test]
    fn text_output_groups_by_project_with_indented_sessions() {
        let metas = vec![
            (
                "proj-a".to_string(),
                vec![make_meta(
                    "s1",
                    "proj-a",
                    "/home/alice/a",
                    "Laptop",
                    "first session",
                    ts(2026, 5, 6, 14, 0),
                )],
            ),
            (
                "proj-b".to_string(),
                vec![make_meta(
                    "s2",
                    "proj-b",
                    "/home/alice/b",
                    "Desktop",
                    "second session",
                    ts(2026, 5, 5, 10, 0),
                )],
            ),
        ];

        let out = format_text_output(&metas);

        // Both project headers present.
        assert!(out.contains("/home/alice/a"), "expected proj-a cwd: {out}");
        assert!(out.contains("/home/alice/b"), "expected proj-b cwd: {out}");

        // Sessions indented under their project.
        assert!(out.contains("first session"), "expected session preview: {out}");
        assert!(out.contains("second session"), "expected session preview: {out}");
        assert!(out.contains("Laptop"), "expected hostname: {out}");
        assert!(out.contains("Desktop"), "expected hostname: {out}");
    }

    #[test]
    fn text_output_includes_project_header_count() {
        let metas = vec![(
            "proj-x".to_string(),
            vec![
                make_meta("s1", "proj-x", "/foo", "h1", "p1", ts(2026, 5, 6, 1, 0)),
                make_meta("s2", "proj-x", "/foo", "h1", "p2", ts(2026, 5, 5, 1, 0)),
                make_meta("s3", "proj-x", "/foo", "h1", "p3", ts(2026, 5, 4, 1, 0)),
            ],
        )];

        let out = format_text_output(&metas);
        // Header line should report "3 sessions" for this project.
        assert!(out.contains("3 sessions"), "expected session count: {out}");
        // Top-level summary should say "1 project".
        assert!(out.contains("1 project"), "expected project count: {out}");
    }

    #[test]
    fn text_output_truncates_long_preview() {
        // 201-char preview — should be truncated to 200 chars + ellipsis.
        let long_preview: String = "x".repeat(201);
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
        // The original 201-char string should NOT appear verbatim.
        assert!(
            !out.contains(&long_preview),
            "preview should have been truncated: {out}"
        );
        // The ellipsis character should be present.
        assert!(out.contains('\u{2026}'), "expected ellipsis in output: {out}");
    }

    // ── JSON output tests ─────────────────────────────────────────────────────

    #[test]
    fn json_output_is_parseable_back_into_a_struct() {
        let metas = vec![
            (
                "proj-a".to_string(),
                vec![make_meta(
                    "sess-1",
                    "proj-a",
                    "/home/alice/projectA",
                    "Mac mini",
                    "running cargo build",
                    ts(2026, 5, 6, 14, 23),
                )],
            ),
            (
                "proj-b".to_string(),
                vec![
                    make_meta(
                        "sess-2",
                        "proj-b",
                        "/home/alice/projectB",
                        "Mac pro",
                        "explore clap derive style",
                        ts(2026, 5, 6, 11, 0),
                    ),
                    make_meta(
                        "sess-3",
                        "proj-b",
                        "/home/alice/projectB",
                        "Mac mini",
                        "starter session",
                        ts(2026, 5, 5, 23, 40),
                    ),
                ],
            ),
        ];

        let json_str = format_json_output(&metas);

        // Must parse without error.
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("JSON must be valid");

        let projects = parsed["projects"].as_array().expect("projects must be array");
        assert_eq!(projects.len(), 2, "expected 2 projects");

        // First project.
        assert_eq!(projects[0]["key"], "proj-a");
        assert_eq!(projects[0]["source_cwd"], "/home/alice/projectA");
        let sessions_a = projects[0]["sessions"]
            .as_array()
            .expect("sessions must be array");
        assert_eq!(sessions_a.len(), 1);
        assert_eq!(sessions_a[0]["id"], "sess-1");
        assert_eq!(sessions_a[0]["source_hostname"], "Mac mini");
        assert!(
            sessions_a[0]["modified_at"]
                .as_str()
                .unwrap()
                .starts_with("2026-05-06"),
            "modified_at should be RFC3339"
        );

        // Second project — two sessions.
        assert_eq!(projects[1]["key"], "proj-b");
        let sessions_b = projects[1]["sessions"]
            .as_array()
            .expect("sessions must be array");
        assert_eq!(sessions_b.len(), 2);
        assert_eq!(sessions_b[0]["id"], "sess-2");
        assert_eq!(sessions_b[1]["id"], "sess-3");
    }
}
