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
use dialoguer::{console::Term, theme::ColorfulTheme, FuzzySelect};
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
        .map(|(pk, _cwd, count, mtime)| {
            // v0.12.0: pk is now the project basename (e.g. "sessync"),
            // not the hash; display it directly as the project name.
            // Drop the (pk_abbrev…) suffix that used to disambiguate same-cwd
            // collisions — by-basename grouping makes pk unique per row.
            let basename = &pk.0;
            let sess_word = if *count == 1 { "session" } else { "sessions" };
            let age = relative_time(now - *mtime);
            format!("{basename}  {count} {sess_word}, latest {age}")
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

/// Visible-rows cap for `FuzzySelect`. With fuzzy filtering active the user
/// usually narrows the list to 1-5 items by typing, so the cap just bounds the
/// initial empty-filter view. 15 fits comfortably in a typical terminal viewport
/// (alongside the breadcrumb of prior phase confirmations) without spilling.
const PICKER_MAX_LENGTH: usize = 15;

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

/// FuzzySelect-with-back: prepends a back row when `allow_back` is true, with
/// the cursor defaulting to the first real item so pressing Enter immediately
/// doesn't accidentally jump back. Returns a `Choice` with back/cancel lifted
/// out of the index space.
///
/// v0.9.2 switched from `Select` to `FuzzySelect` because Select's incremental
/// redraw drifted one row per keypress on long lists with wide chars (Chinese
/// previews / `← 返回上一步`), pushing the breadcrumb out of viewport as the
/// user navigated. FuzzySelect's render path is bounded by `max_length` and the
/// user-typed filter usually narrows the list to a handful of items, so the
/// picker frame stays compact and the breadcrumb above it stays visible.
///
/// The back row uses ASCII `[back]` instead of `← 返回上一步` to side-step any
/// residual wide-char width-measurement quirk in the row that's always at the
/// top of the picker.
fn pick_with_back(prompt: &str, labels: &[String], allow_back: bool) -> Result<Choice> {
    let mut items: Vec<String> = Vec::with_capacity(labels.len() + 1);
    let back_label = "[back] 返回上一步";
    if allow_back {
        items.push(back_label.to_string());
    }
    items.extend(labels.iter().cloned());

    let default = if allow_back { 1 } else { 0 };
    // v0.9.3: `.report(false)` suppresses dialoguer's auto-printed `✓ <prompt>
    // <choice>` confirmation line. We render our own breadcrumb at the top of
    // each phase entry (after clear_screen) so back-and-pick-different doesn't
    // leave stale `✓ Pick a project` lines stacking on screen.
    let raw = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .items(&items)
        .default(default)
        .max_length(PICKER_MAX_LENGTH)
        .report(false)
        .interact_opt()?;

    Ok(match raw {
        None => Choice::Cancel,
        Some(0) if allow_back => Choice::Back,
        Some(i) => Choice::Pick(if allow_back { i - 1 } else { i }),
    })
}

/// Wipe the terminal before each phase entry. No breadcrumb — the picker's
/// own prompt (`? Pick a tool` / `Pick a project` / `Pick a session`) tells
/// the user which level they're at. Eliminates the stale `✓ Pick a project`
/// stacking that back-and-pick-different produced under dialoguer's default
/// `.report(true)` behavior.
fn clear_for_phase() {
    let _ = Term::stdout().clear_screen();
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
            .filter(|o| crate::delta::is_meta_key(&o.key))
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
            // v0.10.0: project_count is no longer derivable from OSS keys
            // (project_key segment removed from path).  Computing the real
            // count would require decrypting every meta to read source_cwd
            // — too expensive at tool-picker preflight.  Display 0 as a
            // placeholder; the per-tool drill-down still groups projects
            // correctly via meta.source_cwd.
            let project_count = 0usize;
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
                clear_for_phase();
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
                clear_for_phase();
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

                    // v0.10.0: project_key is no longer in the OSS key path
                    // (`<tool>/<sid>.meta.json` instead of `<tool>/<pk>/<sid>.age.meta.json`).
                    // Derive a virtual project_key from meta.source_cwd so the
                    // picker still groups by cwd context.  Same source_cwd =
                    // same virtual pk = one picker entry.
                    let mut by_project: BTreeMap<
                        String,
                        Vec<(StorageObject, SessionMeta)>,
                    > = BTreeMap::new();
                    for obj in &ts.meta_objects {
                        let (mtime, size) = ts.object_index[&obj.key];
                        if let Some(meta) = meta_cache.get_if_fresh(&obj.key, mtime, size) {
                            // v0.12.0: group by basename — see ls.rs
                            let pk_str = crate::adapter::path_codec::basename_for_cwd(
                                &meta.source_cwd,
                            );
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
                clear_for_phase();
                let chosen_adapter = &all[tool_idx];
                // v0.10.0: OSS layout dropped project_key, so we list the
                // whole tool prefix and then filter to only the sessions
                // that belong to the user's chosen virtual project_key
                // (derived from meta.source_cwd in the Phase::Project step).
                let tool_prefix = format!("{}/", chosen_adapter.name());
                let session_objects_all = storage.list(&tool_prefix).await?;
                // Build the set of session_ids that belong to the chosen project,
                // re-reading the cached by_project map for this tool (it was
                // populated in Phase::Project for the same tool_idx).
                let chosen_sids: HashSet<String> = project_data_cache
                    .get(&tool_idx)
                    .and_then(|(by_project_map, _)| by_project_map.get(&project_key))
                    .map(|pairs| {
                        pairs
                            .iter()
                            .map(|(_obj, meta)| meta.session_id.0.clone())
                            .collect()
                    })
                    .unwrap_or_default();

                // v0.9.0: filter to base `.age` only — skip deltas (multiple per
                // session) when displaying the picker. We still reconstruct from
                // base+deltas on download via delta::reconstruct.
                // v0.10.0: additional filter by chosen project's session_id set
                // (since we now list the whole tool prefix, not a pk-scoped prefix).
                let mut session_objects: Vec<StorageObject> = session_objects_all
                    .iter()
                    .filter(|o| crate::delta::is_base_key(&o.key))
                    .filter(|o| {
                        // v0.11.0: route via delta:: helper. base key may be
                        // either v0.11 (<tool>/<sid>/base.age) or v0.10
                        // legacy (<tool>/<sid>.age); session_id_from_base_key
                        // handles both.
                        match crate::delta::session_id_from_base_key(
                            chosen_adapter.name(), &o.key)
                        {
                            Some(sid) => chosen_sids.contains(&sid),
                            None => false,
                        }
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
                        // v0.11.0: derive sid from base key, build meta key via delta::meta_key
                        // (no more inline format! — these are what caused the v0.10 hotfix chain).
                        let meta_k = match crate::delta::session_id_from_base_key(
                            chosen_adapter.name(), &obj.key) {
                            Some(sid) => crate::delta::meta_key(chosen_adapter.name(), &sid),
                            None => return None, // unparseable; skip
                        };
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
                        // v0.11.0: same delta:: helper as cache-miss path above.
                        let meta_k = match crate::delta::session_id_from_base_key(
                            chosen_adapter.name(), &obj.key) {
                            Some(sid) => crate::delta::meta_key(chosen_adapter.name(), &sid),
                            None => return None,
                        };
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
    let _ = &chosen_pk; // v0.10: project_key dropped from OSS path
    let layout = crate::delta::find_session_layout(
        &all_for_tool,
        chosen_adapter.name(),
        &chosen_meta.session_id.0,
    );
    let pt = crate::delta::reconstruct(storage, key, &layout).await?;

    // ── v0.12.0 Phase::WriteTarget: choose where to land this session ────────
    // After the user picks a session, ask where to write it locally.
    // Default = a local project dir whose basename matches the chosen project.
    // Alternative = the cwd where `sessync resume` was invoked.
    // If only one viable option exists, no prompt — just use it.
    let target_cwd = choose_target_cwd(chosen_adapter.name(), &chosen_pk)?;

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
            let mut child = chosen_adapter.launch_resume(
                &chosen_meta.session_id,
                Some(std::path::Path::new(&target_cwd)),
            )?;
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

/// v0.12.0: ask the user where to land a resumed session locally.
///
/// 1. Scan `~/.claude/projects/` (or codex equivalent) for dirs whose decoded
///    path basename equals the picked project basename (e.g. user picked
///    "sessync" → look for any `-Users-*-sessync` dir).
/// 2. Always include the current cwd as an option.
/// 3. If only one option exists (or all options resolve to the same path),
///    use it silently — no prompt.
/// 4. Otherwise show a FuzzySelect picker with the matching local dir(s) on
///    top (default) and current cwd below.
fn choose_target_cwd(tool_name: &str, project_basename: &str) -> Result<String> {
    let current = std::env::current_dir()?.to_string_lossy().to_string();

    // Tool-specific project dir scanning.  For Claude Code, decode each
    // `~/.claude/projects/<encoded>` entry and check basename.  For Codex,
    // there's no equivalent local dir convention — we skip auto-detect.
    let local_matches: Vec<String> = if tool_name == "claude-code" {
        scan_claude_dirs_by_basename(project_basename)
    } else {
        Vec::new()
    };

    // Build option list: local matches first (preserve order), then current
    // cwd if it isn't already in the list.
    let mut options: Vec<String> = local_matches.clone();
    if !options.iter().any(|p| p == &current) {
        options.push(current.clone());
    }

    // Zero options shouldn't happen (current is always added), but defensive.
    if options.is_empty() {
        return Ok(current);
    }
    // One option = no prompt.
    if options.len() == 1 {
        return Ok(options.into_iter().next().unwrap());
    }

    // Multi-option: show FuzzySelect.  Labels indicate which is the local match
    // vs the invocation cwd.
    let labels: Vec<String> = options
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i < local_matches.len() {
                format!("{p}    [local match]")
            } else {
                format!("{p}    [current cwd]")
            }
        })
        .collect();
    println!();
    let idx = dialoguer::FuzzySelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Pick target directory")
        .items(&labels)
        .default(0)
        .max_length(PICKER_MAX_LENGTH)
        .interact_on(&dialoguer::console::Term::stderr())
        .context("target directory pick cancelled")?;
    Ok(options[idx].clone())
}

/// Scan `~/.claude/projects/<encoded>` entries; decode each name (replace `-`
/// with `/`) and return those whose basename (last `/`-segment of decode)
/// equals `target_basename`.
fn scan_claude_dirs_by_basename(target_basename: &str) -> Vec<String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let projects_dir = std::path::Path::new(&home).join(".claude").join("projects");
    let Ok(read_dir) = std::fs::read_dir(&projects_dir) else {
        return Vec::new();
    };
    let mut matches = Vec::new();
    for entry in read_dir.flatten() {
        let Ok(file_type) = entry.file_type() else { continue };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // Decode: `-Users-foo-bar-baz` → `/Users/foo/bar/baz` (lossy, but the
        // basename comparison is robust to ambiguity since we just check the
        // last segment).
        let decoded = name.replace('-', "/");
        let bn = crate::adapter::path_codec::basename_for_cwd(&decoded);
        if bn != target_basename {
            continue;
        }
        // v0.12.2 fix: a Claude Code project dir may exist locally (created
        // by sessync pull mirroring content) without the decoded cwd
        // actually existing on this device's filesystem.  Example: pro has
        // `~/.claude/projects/-Users-jameschen-.../sessync/` (mirror of
        // mini's content) but `/Users/jameschen/.../sessync` doesn't exist
        // on pro (no jameschen user).  If we hand that bogus cwd to
        // `Command::current_dir().spawn()` for `claude --resume`, it fails
        // with "No such file or directory".  Skip any decoded cwd whose
        // path doesn't resolve on the local filesystem.
        if !std::path::Path::new(&decoded).exists() {
            continue;
        }
        matches.push(decoded);
    }
    // Stable order: alphabetical
    matches.sort();
    matches
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

    // v0.12.0: pk is now the project basename (e.g. "azoth"), and the cwd
    // field is no longer displayed.  Same-named projects across cwds collapse
    // to a single picker entry — only basename + count + age shows.

    #[test]
    fn project_labels_basic_format() {
        let projects = vec![
            make_project("azoth", "/Users/foo/azoth", 12, 120),
            make_project("deepstar", "/Users/foo/deepstar", 5, 3700),
        ];
        let labels = build_project_labels(&projects);
        assert_eq!(labels.len(), 2);
        assert!(labels[0].contains("azoth"), "label[0]: {}", labels[0]);
        assert!(labels[0].contains("12 sessions"), "label[0]: {}", labels[0]);
        assert!(labels[1].contains("deepstar"), "label[1]: {}", labels[1]);
        assert!(labels[1].contains("5 sessions"), "label[1]: {}", labels[1]);
    }

    #[test]
    fn project_labels_singular_session() {
        let projects = vec![make_project("solo", "/Users/foo/solo", 1, 30)];
        let labels = build_project_labels(&projects);
        assert!(labels[0].contains("1 session"), "label: {}", labels[0]);
        assert!(!labels[0].contains("1 sessions"), "label: {}", labels[0]);
    }

    #[test]
    fn project_labels_sort_recency() {
        let projects = vec![
            make_project("newest", "/Users/foo/newest", 3, 60),
            make_project("middle", "/Users/foo/middle", 2, 3600),
            make_project("oldest", "/Users/foo/oldest", 1, 86400),
        ];
        let labels = build_project_labels(&projects);
        assert!(labels[0].contains("newest"), "first should be newest: {}", labels[0]);
        assert!(labels[1].contains("middle"), "second should be middle: {}", labels[1]);
        assert!(labels[2].contains("oldest"), "third should be oldest: {}", labels[2]);
    }

    #[test]
    fn project_labels_show_only_basename_not_full_path() {
        // The whole point of v0.12: label is just the basename, no full path.
        let projects = vec![make_project(
            "sessync",
            "/Users/sakuragi/very/deep/path/that/exceeds/the/reasonable/display/width/sessync",
            7,
            300,
        )];
        let labels = build_project_labels(&projects);
        assert!(labels[0].contains("sessync"));
        assert!(!labels[0].contains("/Users/"), "label must not contain full path: {}", labels[0]);
        assert!(!labels[0].contains("sakuragi"), "label must not contain user dir: {}", labels[0]);
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
