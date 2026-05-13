use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::registry::{adapter_by_name, all_adapters, known_tool_names};
use crate::adapter::storage::{StorageAdapter, StorageObject};
use crate::adapter::tool::ToolAdapter;
use crate::cache::{self, MetaCache};
use crate::config::{Config, StorageKind};
use crate::crypto;
use crate::passphrase_store;
use crate::types::{ProjectKey, SessionMeta};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use dialoguer::{theme::ColorfulTheme, Select};
use futures::stream::{self, StreamExt, TryStreamExt};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

pub async fn run(
    no_launch: bool,
    restart_app: bool,
    tool_filter: Option<String>,
    project_filter: Option<String>,
) -> Result<()> {
    let cfg = Config::load(&Config::default_path()).context("load config")?;
    let passphrase = passphrase_store::load_passphrase()?;
    let salt = crypto::decode_salt_hex(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;

    match cfg.storage_kind {
        StorageKind::Oss => {
            let oss = cfg
                .oss
                .as_ref()
                .context("storage_kind = oss but [oss] section missing")?;
            let storage = OssStorage::new(oss)?;
            resume_interactive(&storage, &key, no_launch, restart_app, tool_filter, project_filter)
                .await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            resume_interactive(&storage, &key, no_launch, restart_app, tool_filter, project_filter)
                .await
        }
    }
}

// ── Pure helpers (exported for unit tests) ────────────────────────────────────

/// Build tool-picker labels from per-tool stats.
///
/// `per_tool`: `(tool_name, project_count, session_count)` already in display order.
///
/// Output example: `"Claude Code  (45 sessions across 8 projects)"`
pub fn build_tool_labels(per_tool: &[(String, usize, usize)]) -> Vec<String> {
    per_tool
        .iter()
        .map(|(name, project_count, session_count)| {
            let sess_word = if *session_count == 1 { "session" } else { "sessions" };
            let proj_word = if *project_count == 1 { "project" } else { "projects" };
            format!("{name}  ({session_count} {sess_word} across {project_count} {proj_word})")
        })
        .collect()
}

/// Build project-picker labels from per-project stats.
///
/// `projects`: `(project_key, source_cwd, session_count, latest_mtime)` already sorted by recency.
///
/// Output example: `"/Users/foo/azoth  (a3f9…)  12 sessions, latest 2 min ago"`
pub fn build_project_labels(projects: &[(ProjectKey, String, usize, DateTime<Utc>)]) -> Vec<String> {
    let now = Utc::now();
    projects
        .iter()
        .map(|(pk, cwd, count, mtime)| {
            let pk_abbrev = &pk.0[..pk.0.len().min(4)];
            let sess_word = if *count == 1 { "session" } else { "sessions" };
            let age = relative_time(now - *mtime);
            // Truncate very long cwd to 60 chars from the right (keep filename end).
            let cwd_display = truncate_cwd(cwd, 60);
            format!("{cwd_display}  ({pk_abbrev}…)  {count} {sess_word}, latest {age}")
        })
        .collect()
}

/// Express a chrono Duration as a human-readable "2 min ago" / "3 days ago" string.
fn relative_time(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins} min ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        let w = if hours == 1 { "hour" } else { "hours" };
        return format!("{hours} {w} ago");
    }
    let days = hours / 24;
    let w = if days == 1 { "day" } else { "days" };
    format!("{days} {w} ago")
}

/// Truncate a cwd path to at most `max_chars` characters, keeping the right side.
fn truncate_cwd(cwd: &str, max_chars: usize) -> String {
    if cwd.chars().count() <= max_chars {
        return cwd.to_string();
    }
    // Keep the right end so the project directory name stays visible.
    let suffix: String = cwd.chars().rev().take(max_chars - 1).collect::<String>().chars().rev().collect();
    format!("…{suffix}")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

// ── Internal session data collected per tool ──────────────────────────────────

struct ToolSessions {
    /// All `.meta.json` storage objects for this tool, sorted by mtime DESC.
    meta_objects: Vec<StorageObject>,
    /// Maps object-key → (mtime, size) — used for cache lookups.
    object_index: HashMap<String, (DateTime<Utc>, u64)>,
}

// ── Main interactive flow ─────────────────────────────────────────────────────

pub async fn resume_interactive<S: StorageAdapter>(
    storage: &S,
    key: &[u8; 32],
    no_launch: bool,
    restart_app: bool,
    tool_filter: Option<String>,
    project_filter: Option<String>,
) -> Result<()> {
    // Load shared meta cache once.
    let cache_path: Option<PathBuf> = cache::default_cache_path().ok();
    let mut meta_cache = match &cache_path {
        Some(p) => MetaCache::load_or_empty(p, key, "all"),
        None => {
            tracing::debug!("HOME not set — running without meta cache");
            MetaCache::empty("all")
        }
    };

    // ── Resolve candidate adapters ────────────────────────────────────────────
    let all: Vec<Box<dyn ToolAdapter>> = if let Some(ref name) = tool_filter {
        match adapter_by_name(name) {
            Some(a) => vec![a],
            None => anyhow::bail!(
                "unknown tool '{}'. Known: {}",
                name,
                known_tool_names().join(", ")
            ),
        }
    } else {
        all_adapters()
    };

    // ── Fetch .meta.json listings for each candidate adapter ──────────────────
    let mut per_tool: Vec<ToolSessions> = Vec::new();
    for (adapter_index, adapter) in all.iter().enumerate() {
        let prefix = format!("{}/", adapter.name());
        let objects = storage.list(&prefix).await?;

        let mut object_index: HashMap<String, (DateTime<Utc>, u64)> = HashMap::new();
        for o in &objects {
            object_index.insert(o.key.clone(), (o.last_modified, o.size));
        }

        // Tombstone stale cache entries for this tool's keys.
        let present: HashSet<&str> = object_index.keys().map(|k| k.as_str()).collect();
        meta_cache.retain_only(&present);

        let mut meta_objects: Vec<StorageObject> = objects
            .into_iter()
            .filter(|o| o.key.ends_with(".meta.json"))
            .collect();
        meta_objects.sort_by_key(|o| Reverse(o.last_modified));

        let _ = adapter_index; // kept for clarity; adapter resolved from `all` slice later
        per_tool.push(ToolSessions {
            meta_objects,
            object_index,
        });
    }

    // ── Phase 1: pick tool ────────────────────────────────────────────────────
    // Build per-tool stats for the tool picker label.
    let tool_stats: Vec<(String, usize, usize)> = per_tool
        .iter()
        .zip(all.iter())
        .map(|(ts, adapter)| {
            let session_count = ts.meta_objects.len();
            // Count distinct project keys.
            let project_count = ts
                .meta_objects
                .iter()
                .filter_map(|o| {
                    let parts: Vec<&str> = o.key.splitn(3, '/').collect();
                    if parts.len() >= 3 { Some(parts[1]) } else { None }
                })
                .collect::<HashSet<_>>()
                .len();
            (adapter.name().to_string(), project_count, session_count)
        })
        .collect();

    let chosen_tool_index = if all.len() == 1 {
        // Only one adapter — auto-pick without showing a picker.
        0
    } else {
        // Show tool picker.
        let tool_labels = build_tool_labels(&tool_stats);
        let Some(pick) = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Pick a tool")
            .items(&tool_labels)
            .default(0)
            .interact_opt()?
        else {
            println!("Cancelled.");
            save_cache(&cache_path, &mut meta_cache, key);
            return Ok(());
        };
        pick
    };

    let chosen_adapter = &all[chosen_tool_index];
    let ts = &per_tool[chosen_tool_index];

    if ts.meta_objects.is_empty() {
        println!(
            "No remote sessions found for tool '{}'.",
            chosen_adapter.name()
        );
        save_cache(&cache_path, &mut meta_cache, key);
        return Ok(());
    }

    // ── Fetch cache misses for the chosen tool's meta objects ─────────────────
    let misses: Vec<(String, DateTime<Utc>, u64)> = ts
        .meta_objects
        .iter()
        .filter_map(|o| {
            let (mtime, size) = ts.object_index[&o.key];
            if meta_cache.get_if_fresh(&o.key, mtime, size).is_some() {
                None
            } else {
                Some((o.key.clone(), mtime, size))
            }
        })
        .collect();

    let fetched: Vec<(String, SessionMeta, DateTime<Utc>, u64)> =
        stream::iter(misses)
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

    // Build (project_key → Vec<(obj, meta)>) map for the chosen tool.
    // Sort each bucket by mtime DESC.
    let mut by_project: BTreeMap<String, Vec<(StorageObject, SessionMeta)>> = BTreeMap::new();
    for obj in &ts.meta_objects {
        let parts: Vec<&str> = obj.key.splitn(3, '/').collect();
        if parts.len() < 3 {
            continue;
        }
        let pk_str = parts[1].to_string();
        let (mtime, size) = ts.object_index[&obj.key];
        if let Some(meta) = meta_cache.get_if_fresh(&obj.key, mtime, size) {
            by_project
                .entry(pk_str)
                .or_default()
                .push((obj.clone(), meta.clone()));
        }
    }

    // Build sorted project list: (ProjectKey, source_cwd, session_count, max_mtime).
    let mut projects: Vec<(ProjectKey, String, usize, DateTime<Utc>)> = by_project
        .iter()
        .map(|(pk_str, entries)| {
            let max_mtime = entries.iter().map(|(_, m)| m.modified_at).max().unwrap_or(Utc::now());
            let source_cwd = entries
                .iter()
                .max_by_key(|(_, m)| m.modified_at)
                .map(|(_, m)| m.source_cwd.clone())
                .unwrap_or_default();
            (ProjectKey(pk_str.clone()), source_cwd, entries.len(), max_mtime)
        })
        .collect();
    // Sort by recency DESC.
    projects.sort_by_key(|(_, _, _, mtime)| Reverse(*mtime));

    // ── Phase 2: pick project ─────────────────────────────────────────────────
    let chosen_pk: String = if let Some(ref pf) = project_filter {
        // --project given: filter + validate.
        let matches: Vec<_> = projects
            .iter()
            .filter(|(pk, _, _, _)| pk.0 == *pf || pk.0.starts_with(pf.as_str()))
            .collect();
        match matches.len() {
            0 => anyhow::bail!(
                "no project '{}' found in {} remote",
                pf,
                chosen_adapter.name()
            ),
            1 => matches[0].0 .0.clone(),
            _ => {
                // Multiple prefix matches — show filtered picker.
                let filtered: Vec<(ProjectKey, String, usize, DateTime<Utc>)> =
                    matches.into_iter().cloned().collect();
                let labels = build_project_labels(&filtered);
                let Some(pick) = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Pick a project")
                    .items(&labels)
                    .default(0)
                    .interact_opt()?
                else {
                    println!("Cancelled.");
                    save_cache(&cache_path, &mut meta_cache, key);
                    return Ok(());
                };
                filtered[pick].0 .0.clone()
            }
        }
    } else if projects.len() == 1 {
        // Only one project — auto-pick.
        projects[0].0 .0.clone()
    } else {
        // Show project picker.
        let labels = build_project_labels(&projects);
        let Some(pick) = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Pick a project")
            .items(&labels)
            .default(0)
            .interact_opt()?
        else {
            println!("Cancelled.");
            save_cache(&cache_path, &mut meta_cache, key);
            return Ok(());
        };
        projects[pick].0 .0.clone()
    };

    // ── Phase 3: pick session within chosen project ───────────────────────────
    let session_prefix = format!("{}/{}/", chosen_adapter.name(), chosen_pk);
    let session_objects_all = storage.list(&session_prefix).await?;

    let mut session_objects: Vec<StorageObject> = session_objects_all
        .into_iter()
        .filter(|o| o.key.ends_with(".meta.json"))
        .collect();
    session_objects.sort_by_key(|o| Reverse(o.last_modified));

    // Build a unified object_index for session-level objects (may include new keys
    // not in the tool-level index if storage was updated mid-flow).
    let mut session_obj_index: HashMap<String, (DateTime<Utc>, u64)> = HashMap::new();
    for o in &session_objects {
        session_obj_index.insert(o.key.clone(), (o.last_modified, o.size));
    }

    // Fetch any session-level cache misses.
    let session_misses: Vec<(String, DateTime<Utc>, u64)> = session_objects
        .iter()
        .filter_map(|obj| {
            let (mtime, size) = *session_obj_index
                .get(&obj.key)
                .unwrap_or(&(obj.last_modified, obj.size));
            if meta_cache.get_if_fresh(&obj.key, mtime, size).is_some() {
                None
            } else {
                Some((obj.key.clone(), mtime, size))
            }
        })
        .collect();

    let fetched_session: Vec<(String, SessionMeta, DateTime<Utc>, u64)> =
        stream::iter(session_misses)
            .map(|(mk, mtime, size)| async move {
                let raw = storage.get(&mk).await?;
                let pt = crypto::decrypt(&raw, key)?;
                let meta: SessionMeta = serde_json::from_slice(&pt)?;
                anyhow::Ok((mk, meta, mtime, size))
            })
            .buffered(8)
            .try_collect()
            .await?;

    for (mk, meta, mtime, size) in fetched_session {
        meta_cache.insert(mk, meta, mtime, size);
    }

    let pairs: Vec<(String, SessionMeta)> = session_objects
        .iter()
        .map(|obj| {
            let (mtime, size) = *session_obj_index
                .get(&obj.key)
                .unwrap_or(&(obj.last_modified, obj.size));
            let meta = meta_cache
                .get_if_fresh(&obj.key, mtime, size)
                .expect("just fetched or was already cached")
                .clone();
            let label = format!(
                "[{}] {}  — {}",
                meta.modified_at
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M"),
                truncate(&meta.preview, 200),
                meta.source_hostname,
            );
            (label, meta)
        })
        .collect();

    let (session_labels, session_metas): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();

    let Some(session_pick) = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Pick a session")
        .items(&session_labels)
        .default(0)
        .interact_opt()?
    else {
        println!("Cancelled.");
        save_cache(&cache_path, &mut meta_cache, key);
        return Ok(());
    };
    let chosen_meta = &session_metas[session_pick];

    // ── Download + decrypt the actual session bytes ───────────────────────────
    let session_key = format!(
        "{}/{}/{}.age",
        chosen_adapter.name(),
        chosen_pk,
        chosen_meta.session_id.0
    );
    let ct = storage.get(&session_key).await?;
    let pt = crypto::decrypt(&ct, key)?;

    // ── Write into target cwd via the chosen adapter ──────────────────────────
    let target_cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let written = chosen_adapter
        .write_session(&chosen_meta.session_id, &target_cwd, &pt)
        .await?;

    println!("\nSession dropped at: {}", written.display());
    println!(
        "Run: {} --resume {}",
        chosen_adapter.name(),
        chosen_meta.session_id
    );

    // Best-effort cache save.
    save_cache(&cache_path, &mut meta_cache, key);

    // ── Optionally restart the tool's app (Codex only) ────────────────────────
    if restart_app && chosen_adapter.name() == "codex" {
        eprintln!("Restarting Codex.app to refresh its session list...");
        let _ = std::process::Command::new("killall").arg("Codex").output();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let _ = std::process::Command::new("open")
            .arg("/Applications/Codex.app")
            .output();
        println!("Codex.app restarted to pick up the synced session.");
    }

    // ── Launch resume unless suppressed ──────────────────────────────────────
    if !no_launch {
        if chosen_adapter.launch_binary_on_path() {
            let mut child = chosen_adapter.launch_resume(&chosen_meta.session_id)?;
            let status = child.wait()?;
            std::process::exit(status.code().unwrap_or(0));
        } else {
            println!(
                "({} not found in PATH; run the command above to resume)",
                chosen_adapter.name()
            );
        }
    }

    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn save_cache(cache_path: &Option<PathBuf>, meta_cache: &mut MetaCache, key: &[u8; 32]) {
    if let Some(p) = cache_path {
        if let Err(e) = meta_cache.save(p, key) {
            tracing::debug!("meta-cache save failed (non-fatal): {e}");
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // ── build_tool_labels ─────────────────────────────────────────────────────

    #[test]
    fn tool_labels_singular_vs_plural() {
        let per_tool = vec![
            ("Claude Code".to_string(), 8usize, 45usize),
            ("Codex".to_string(), 1usize, 1usize),
        ];
        let labels = build_tool_labels(&per_tool);
        assert_eq!(labels.len(), 2);
        // Plural
        assert!(
            labels[0].contains("45 sessions"),
            "should say '45 sessions', got: {}",
            labels[0]
        );
        assert!(
            labels[0].contains("8 projects"),
            "should say '8 projects', got: {}",
            labels[0]
        );
        // Singular
        assert!(
            labels[1].contains("1 session"),
            "should say '1 session', got: {}",
            labels[1]
        );
        assert!(
            labels[1].contains("1 project"),
            "should say '1 project', got: {}",
            labels[1]
        );
        // Sanity: singular must NOT contain "sessions" or "projects" (no accidental plural)
        assert!(
            !labels[1].contains("1 sessions"),
            "should not have '1 sessions', got: {}",
            labels[1]
        );
        assert!(
            !labels[1].contains("1 projects"),
            "should not have '1 projects', got: {}",
            labels[1]
        );
    }

    #[test]
    fn tool_labels_zero_sessions() {
        let per_tool = vec![("Claude Code".to_string(), 0usize, 0usize)];
        let labels = build_tool_labels(&per_tool);
        assert!(
            labels[0].contains("0 sessions"),
            "zero sessions label: {}",
            labels[0]
        );
        assert!(
            labels[0].contains("0 projects"),
            "zero projects label: {}",
            labels[0]
        );
    }

    // ── build_project_labels ──────────────────────────────────────────────────

    fn make_project(pk: &str, cwd: &str, count: usize, secs_ago: i64) -> (ProjectKey, String, usize, DateTime<Utc>) {
        let mtime = Utc::now() - chrono::Duration::seconds(secs_ago);
        (ProjectKey(pk.to_string()), cwd.to_string(), count, mtime)
    }

    #[test]
    fn project_labels_basic_format() {
        let projects = vec![
            make_project("a3f9b2c1", "/Users/foo/azoth", 12, 120),
            make_project("deadbeef", "/Users/foo/deepstar", 5, 3700),
        ];
        let labels = build_project_labels(&projects);
        assert_eq!(labels.len(), 2);
        // First entry: azoth, 12 sessions, ~2 min ago
        assert!(labels[0].contains("azoth"), "label[0]: {}", labels[0]);
        assert!(labels[0].contains("12 sessions"), "label[0]: {}", labels[0]);
        assert!(labels[0].contains("a3f9"), "should contain pk prefix, label[0]: {}", labels[0]);
        // Second entry: deepstar, 5 sessions, ~1 hour ago
        assert!(labels[1].contains("deepstar"), "label[1]: {}", labels[1]);
        assert!(labels[1].contains("5 sessions"), "label[1]: {}", labels[1]);
    }

    #[test]
    fn project_labels_singular_session() {
        let projects = vec![make_project("aabbccdd", "/Users/foo/solo", 1, 30)];
        let labels = build_project_labels(&projects);
        assert!(labels[0].contains("1 session"), "label: {}", labels[0]);
        assert!(!labels[0].contains("1 sessions"), "label: {}", labels[0]);
    }

    #[test]
    fn project_labels_sort_recency() {
        // The caller is responsible for sorting; build_project_labels just formats.
        // We pass an already-sorted slice (newest first) and verify order is preserved.
        let projects = vec![
            make_project("aaaa0001", "/Users/foo/newest", 3, 60),   // 1 min ago
            make_project("bbbb0002", "/Users/foo/middle", 2, 3600), // 1 hour ago
            make_project("cccc0003", "/Users/foo/oldest", 1, 86400),// 1 day ago
        ];
        let labels = build_project_labels(&projects);
        assert!(labels[0].contains("newest"), "first should be newest: {}", labels[0]);
        assert!(labels[1].contains("middle"), "second should be middle: {}", labels[1]);
        assert!(labels[2].contains("oldest"), "third should be oldest: {}", labels[2]);
    }

    #[test]
    fn project_labels_truncate_long_cwd() {
        let long_cwd = "/Users/sakuragi/very/deep/path/that/exceeds/the/reasonable/display/width/project/name";
        let projects = vec![make_project("abcd1234", long_cwd, 7, 300)];
        let labels = build_project_labels(&projects);
        // Label should exist and contain the project name at the end.
        assert!(labels[0].contains("name"), "should contain end of path: {}", labels[0]);
        // Should not be longer than 200 chars total (generous bound).
        assert!(
            labels[0].chars().count() < 200,
            "label too long: {} chars",
            labels[0].chars().count()
        );
    }

    // ── relative_time ─────────────────────────────────────────────────────────

    #[test]
    fn relative_time_just_now() {
        assert_eq!(relative_time(chrono::Duration::seconds(0)), "just now");
        assert_eq!(relative_time(chrono::Duration::seconds(59)), "just now");
    }

    #[test]
    fn relative_time_minutes() {
        assert_eq!(relative_time(chrono::Duration::seconds(60)), "1 min ago");
        assert_eq!(relative_time(chrono::Duration::seconds(120)), "2 min ago");
        assert_eq!(relative_time(chrono::Duration::seconds(3599)), "59 min ago");
    }

    #[test]
    fn relative_time_hours() {
        assert_eq!(relative_time(chrono::Duration::seconds(3600)), "1 hour ago");
        assert_eq!(relative_time(chrono::Duration::seconds(7200)), "2 hours ago");
    }

    #[test]
    fn relative_time_days() {
        assert_eq!(relative_time(chrono::Duration::seconds(86400)), "1 day ago");
        assert_eq!(relative_time(chrono::Duration::seconds(86400 * 3)), "3 days ago");
    }

    // ── truncate_cwd ──────────────────────────────────────────────────────────

    #[test]
    fn truncate_cwd_short_passthrough() {
        let s = "/Users/foo/bar";
        assert_eq!(truncate_cwd(s, 60), s);
    }

    #[test]
    fn truncate_cwd_long_keeps_end() {
        let s = "/Users/sakuragi/very/deep/path/that/exceeds/the/display/limit/project";
        let result = truncate_cwd(s, 20);
        assert!(result.starts_with('…'), "should start with ellipsis: {result}");
        assert!(result.ends_with("project"), "should end with directory name: {result}");
        assert!(result.chars().count() <= 20, "should be at most 20 chars: {result}");
    }
}
