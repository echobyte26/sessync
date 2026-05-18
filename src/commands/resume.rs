use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::registry::{adapter_by_name, all_adapters, known_tool_names};
use crate::adapter::s3::S3Storage;
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
    include_ghosts: bool,
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
            resume_interactive(&storage, &key, no_launch, restart_app, tool_filter, project_filter, include_ghosts)
                .await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            resume_interactive(&storage, &key, no_launch, restart_app, tool_filter, project_filter, include_ghosts)
                .await
        }
        StorageKind::S3 => {
            let s3cfg = cfg
                .s3
                .as_ref()
                .context("storage_kind = s3 but [s3] section missing")?;
            let storage = S3Storage::new(s3cfg)?;
            resume_interactive(&storage, &key, no_launch, restart_app, tool_filter, project_filter, include_ghosts)
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

/// Session-preview cap (chars) for picker labels. v0.9.0 dropped this from 200
/// to 60 — the old value caused multi-line wraps in the terminal, which (with
/// `dialoguer`'s persistent prompt confirmation lines for each picker step) was
/// pushing the active picker out of view as the user navigated deeper.
const SESSION_PREVIEW_CAP: usize = 60;

// v0.9.0 added `.max_length(15)` to all Select pickers for pagination, but
// dialoguer 0.11's max_length redraw relies on ANSI cursor-up + clear-line
// sequences that don't render cleanly in some terminals when wide chars
// (Chinese previews / project paths) are present — each arrow-key press
// re-prints the picker BELOW the previous frame instead of redrawing in
// place. v0.9.1 dropped the cap: with session previews already capped at 60
// chars (single line), the picker stays compact even without pagination.

/// User's choice from a `pick_with_back` picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// Picked the indexed real item (i.e. **excluding** the synthetic "back"
    /// row when present — caller doesn't need to compensate).
    Pick(usize),
    /// User picked the synthetic "← back" row at the top.
    Back,
    /// User pressed ESC / Ctrl-C to abort.
    Cancel,
}

/// `Select`-with-back: prepends "← 返回上一步" when `allow_back` is true, with
/// the cursor defaulting to the first real item so pressing Enter immediately
/// doesn't accidentally jump back. Returns a `Choice` with the back/cancel
/// distinction lifted out of the index space.
fn pick_with_back(prompt: &str, labels: &[String], allow_back: bool) -> Result<Choice> {
    let mut items: Vec<String> = Vec::with_capacity(labels.len() + 1);
    let back_label = "← 返回上一步";
    if allow_back {
        items.push(back_label.to_string());
    }
    items.extend(labels.iter().cloned());

    let default = if allow_back { 1 } else { 0 };
    let raw = Select::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .items(&items)
        .default(default)
        .interact_opt()?;

    Ok(match raw {
        None => Choice::Cancel,
        Some(0) if allow_back => Choice::Back,
        Some(i) => Choice::Pick(if allow_back { i - 1 } else { i }),
    })
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
    include_ghosts: bool,
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

    // ── State machine driving Tool → Project → Session picks ─────────────────
    // Each step can be re-entered when the user picks "← back" in the next.
    // We cache the per-tool projects map so re-entering the project step after
    // back-from-session doesn't re-decrypt the tool's meta objects.

    #[derive(Debug, Clone)]
    enum Phase {
        Tool,
        Project { tool_idx: usize },
        Session {
            tool_idx: usize,
            project_key: String,
            /// `true` iff this Session was entered from a SHOWN project picker
            /// (not auto-skipped). Determines whether back from Session goes to
            /// Project (recheck) or Tool.
            project_picker_was_shown: bool,
        },
    }

    // Cached per-tool view: project list + by_project bucketing. Built lazily
    // on first entry to that tool's Project phase, reused on re-entry.
    let mut project_data_cache: HashMap<
        usize,
        (
            BTreeMap<String, Vec<(StorageObject, SessionMeta)>>,
            Vec<(ProjectKey, String, usize, DateTime<Utc>)>,
        ),
    > = HashMap::new();

    let mut phase = if all.len() > 1 {
        Phase::Tool
    } else {
        Phase::Project { tool_idx: 0 }
    };

    let (chosen_tool_index, chosen_pk, chosen_meta_owned): (usize, String, SessionMeta) = loop {
        match phase.clone() {
            // ── Phase 1: pick tool (no back — this is the entry) ───────────
            Phase::Tool => {
                let tool_labels = build_tool_labels(&tool_stats);
                match pick_with_back("Pick a tool", &tool_labels, /*allow_back=*/ false)? {
                    Choice::Cancel | Choice::Back => {
                        println!("Cancelled.");
                        save_cache(&cache_path, &mut meta_cache, key);
                        return Ok(());
                    }
                    Choice::Pick(i) => {
                        phase = Phase::Project { tool_idx: i };
                    }
                }
            }

            // ── Phase 2: pick project ──────────────────────────────────────
            Phase::Project { tool_idx } => {
                let chosen_adapter = &all[tool_idx];
                let ts = &per_tool[tool_idx];
                if ts.meta_objects.is_empty() {
                    println!(
                        "No remote sessions found for tool '{}'.",
                        chosen_adapter.name()
                    );
                    if all.len() > 1 {
                        phase = Phase::Tool;
                        continue;
                    }
                    save_cache(&cache_path, &mut meta_cache, key);
                    return Ok(());
                }

                // Lazily decrypt cache misses + bucket by project (cached per tool).
                if !project_data_cache.contains_key(&tool_idx) {
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

                    let mut by_project: BTreeMap<
                        String,
                        Vec<(StorageObject, SessionMeta)>,
                    > = BTreeMap::new();
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

                    let mut projects: Vec<(ProjectKey, String, usize, DateTime<Utc>)> =
                        by_project
                            .iter()
                            .map(|(pk_str, entries)| {
                                let max_mtime = entries
                                    .iter()
                                    .map(|(_, m)| m.modified_at)
                                    .max()
                                    .unwrap_or(Utc::now());
                                let source_cwd = entries
                                    .iter()
                                    .max_by_key(|(_, m)| m.modified_at)
                                    .map(|(_, m)| m.source_cwd.clone())
                                    .unwrap_or_default();
                                (
                                    ProjectKey(pk_str.clone()),
                                    source_cwd,
                                    entries.len(),
                                    max_mtime,
                                )
                            })
                            .collect();
                    projects.sort_by_key(|(_, _, _, mtime)| Reverse(*mtime));

                    project_data_cache.insert(tool_idx, (by_project, projects));
                }
                let (_by_project_ref, projects_ref) = &project_data_cache[&tool_idx];
                let projects = projects_ref.clone();

                // Apply --project filter to narrow the choices.
                let filtered_projects: Vec<(ProjectKey, String, usize, DateTime<Utc>)> =
                    if let Some(ref pf) = project_filter {
                        let matches: Vec<_> = projects
                            .iter()
                            .filter(|(pk, _, _, _)| {
                                pk.0 == *pf || pk.0.starts_with(pf.as_str())
                            })
                            .cloned()
                            .collect();
                        if matches.is_empty() {
                            anyhow::bail!(
                                "no project '{}' found in {} remote",
                                pf,
                                chosen_adapter.name()
                            );
                        }
                        matches
                    } else {
                        projects.clone()
                    };

                // Auto-pick when there's only one choice — no picker to show.
                if filtered_projects.len() == 1 {
                    phase = Phase::Session {
                        tool_idx,
                        project_key: filtered_projects[0].0 .0.clone(),
                        project_picker_was_shown: false,
                    };
                    continue;
                }

                let labels = build_project_labels(&filtered_projects);
                let allow_back = all.len() > 1;
                match pick_with_back("Pick a project", &labels, allow_back)? {
                    Choice::Cancel => {
                        println!("Cancelled.");
                        save_cache(&cache_path, &mut meta_cache, key);
                        return Ok(());
                    }
                    Choice::Back => {
                        phase = Phase::Tool;
                    }
                    Choice::Pick(i) => {
                        phase = Phase::Session {
                            tool_idx,
                            project_key: filtered_projects[i].0 .0.clone(),
                            project_picker_was_shown: true,
                        };
                    }
                }
            }

            // ── Phase 3: pick session within chosen tool+project ───────────
            Phase::Session {
                tool_idx,
                project_key,
                project_picker_was_shown,
            } => {
                let chosen_adapter = &all[tool_idx];
                let session_prefix = format!("{}/{}/", chosen_adapter.name(), project_key);
                let session_objects_all = storage.list(&session_prefix).await?;

                // v0.9.0: filter to base `.age` only — skip deltas (multiple per
                // session) when displaying the picker. We still reconstruct from
                // base+deltas on download via delta::reconstruct.
                let mut session_objects: Vec<StorageObject> = session_objects_all
                    .iter()
                    .filter(|o| {
                        crate::delta::is_base_key(&o.key)
                    })
                    .cloned()
                    .collect();
                session_objects.sort_by_key(|o| Reverse(o.last_modified));

                let mut session_obj_index: HashMap<String, (DateTime<Utc>, u64)> =
                    HashMap::new();
                for o in &session_objects {
                    session_obj_index.insert(o.key.clone(), (o.last_modified, o.size));
                }

                let session_misses: Vec<(String, DateTime<Utc>, u64)> = session_objects
                    .iter()
                    .filter_map(|obj| {
                        let (mtime, size) = *session_obj_index
                            .get(&obj.key)
                            .unwrap_or(&(obj.last_modified, obj.size));
                        // meta sidecar is at `{base}.meta.json`
                        let meta_k = format!("{}.meta.json", obj.key);
                        if meta_cache.get_if_fresh(&meta_k, mtime, size).is_some() {
                            None
                        } else {
                            Some((meta_k, mtime, size))
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
                    .filter_map(|obj| {
                        let (mtime, size) = *session_obj_index
                            .get(&obj.key)
                            .unwrap_or(&(obj.last_modified, obj.size));
                        let meta_k = format!("{}.meta.json", obj.key);
                        let meta = meta_cache
                            .get_if_fresh(&meta_k, mtime, size)?
                            .clone();

                        if !include_ghosts
                            && (!meta.has_user_message || meta.preview.trim().is_empty())
                        {
                            return None;
                        }

                        let label = format!(
                            "[{}] {}  — {}",
                            meta.modified_at
                                .with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M"),
                            truncate(&meta.preview, SESSION_PREVIEW_CAP),
                            meta.source_hostname,
                        );
                        Some((label, meta))
                    })
                    .collect();

                if pairs.is_empty() {
                    println!(
                        "No sessions to show (all hidden as ghosts). \
                         Use --include-ghosts to see them."
                    );
                    // Allow going back if either upstream picker was shown.
                    if project_picker_was_shown {
                        phase = Phase::Project { tool_idx };
                        continue;
                    }
                    if all.len() > 1 {
                        phase = Phase::Tool;
                        continue;
                    }
                    save_cache(&cache_path, &mut meta_cache, key);
                    return Ok(());
                }

                let (session_labels, session_metas): (Vec<_>, Vec<_>) =
                    pairs.into_iter().unzip();

                let allow_back = project_picker_was_shown || all.len() > 1;
                match pick_with_back("Pick a session", &session_labels, allow_back)? {
                    Choice::Cancel => {
                        println!("Cancelled.");
                        save_cache(&cache_path, &mut meta_cache, key);
                        return Ok(());
                    }
                    Choice::Back => {
                        phase = if project_picker_was_shown {
                            Phase::Project { tool_idx }
                        } else {
                            Phase::Tool
                        };
                    }
                    Choice::Pick(i) => {
                        break (tool_idx, project_key, session_metas[i].clone());
                    }
                }
            }
        }
    };

    let chosen_adapter = &all[chosen_tool_index];
    let chosen_meta = &chosen_meta_owned;

    // ── Reconstruct session content from base + deltas ──────────────────────
    // v0.9.0: a session is stored as a base `.age` plus zero or more
    // `.delta-{seq}-{device}.age` chunks. `delta::reconstruct` lists the layout
    // from the all-tool prefix listing, downloads each part, decrypts, and
    // gunzips (transparent for old uncompressed-base objects).
    let resume_prefix = format!("{}/", chosen_adapter.name());
    let all_for_tool = storage.list(&resume_prefix).await?;
    let layout = crate::delta::find_session_layout(
        &all_for_tool,
        chosen_adapter.name(),
        &chosen_pk,
        &chosen_meta.session_id.0,
    );
    let pt = crate::delta::reconstruct(storage, key, &layout).await?;

    // ── Write into target cwd via the chosen adapter ──────────────────────────
    let target_cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let written = chosen_adapter
        .write_session(&chosen_meta.session_id, &target_cwd, &pt, chosen_meta.modified_at)
        .await?;

    println!("\nSession dropped at: {}", written.display());
    println!(
        "Run: {} --resume {}",
        chosen_adapter.launch_binary_name(),
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
                chosen_adapter.launch_binary_name()
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
