use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::registry::{adapter_by_name, all_adapters, known_tool_names};
use crate::adapter::storage::{StorageAdapter, StorageObject};
use crate::adapter::tool::ToolAdapter;
use crate::cache::{self, MetaCache};
use crate::config::{Config, StorageKind};
use crate::crypto;
use crate::passphrase_store;
use crate::types::SessionMeta;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use dialoguer::{theme::ColorfulTheme, Select};
use futures::stream::{self, StreamExt, TryStreamExt};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

pub async fn run(no_launch: bool, tool_filter: Option<String>) -> Result<()> {
    let cfg = Config::load(&Config::default_path()).context("load config")?;
    let passphrase = passphrase_store::load_passphrase()?;
    let salt = crypto::decode_salt_hex(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;

    // Resolve which adapters to show sessions from.
    let adapters: Vec<Box<dyn ToolAdapter>> = if let Some(ref name) = tool_filter {
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

    match cfg.storage_kind {
        StorageKind::Oss => {
            let oss = cfg
                .oss
                .as_ref()
                .context("storage_kind = oss but [oss] section missing")?;
            let storage = OssStorage::new(oss)?;
            resume_interactive(&adapters, &storage, &key, no_launch).await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            resume_interactive(&adapters, &storage, &key, no_launch).await
        }
    }
}

/// A session entry in the resume picker, with its originating adapter.
struct ResumeEntry {
    /// Index into `adapters` slice so we can retrieve the right adapter after pick.
    adapter_index: usize,
    /// The adapter name, used for the multi-tool prefix in the picker label.
    adapter_name: &'static str,
    /// Project key (second segment of the OSS key).
    project_key: String,
    /// Decoded session metadata.
    meta: SessionMeta,
    /// Storage object key for the `.age.meta.json` sidecar (used for cache).
    meta_key: String,
}

pub async fn resume_interactive<S: StorageAdapter>(
    adapters: &[Box<dyn ToolAdapter>],
    storage: &S,
    key: &[u8; 32],
    no_launch: bool,
) -> Result<()> {
    // Load cache once — shared across all adapters.
    let cache_path: Option<PathBuf> = cache::default_cache_path().ok();
    // We use "all" as a synthetic tool name for the shared cache namespace.
    let mut meta_cache = match &cache_path {
        Some(p) => MetaCache::load_or_empty(p, key, "all"),
        None => {
            tracing::debug!("HOME not set — running without meta cache");
            MetaCache::empty("all")
        }
    };

    // Gather all .meta.json objects from each adapter's prefix, merged into one list.
    let mut all_entries: Vec<ResumeEntry> = Vec::new();
    // Unified object_index: key → (mtime, size) across all tools.
    let mut object_index: HashMap<String, (DateTime<Utc>, u64)> = HashMap::new();

    for (adapter_index, adapter) in adapters.iter().enumerate() {
        let prefix = format!("{}/", adapter.name());
        let objects = storage.list(&prefix).await?;

        for o in &objects {
            object_index.insert(o.key.clone(), (o.last_modified, o.size));
        }

        // Tombstone stale cache entries for keys no longer present.
        let present: HashSet<&str> = object_index.keys().map(|k| k.as_str()).collect();
        meta_cache.retain_only(&present);

        // Group .meta.json objects by project key within this adapter.
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

        // For each project, take the most-recent .meta.json as the representative entry.
        for (project_key, mut objs) in by_project {
            objs.sort_by_key(|o| Reverse(o.last_modified));
            for obj in objs {
                // We'll fill meta later after fetching; placeholder for now.
                all_entries.push(ResumeEntry {
                    adapter_index,
                    adapter_name: adapter.name(),
                    project_key: project_key.clone(),
                    meta: SessionMeta {
                        schema_version: 0,
                        session_id: crate::types::SessionId(String::new()),
                        project_key: crate::types::ProjectKey(project_key.clone()),
                        source_cwd: String::new(),
                        source_hostname: String::new(),
                        modified_at: obj.last_modified,
                        byte_size: 0,
                        preview: String::new(),
                    },
                    meta_key: obj.key.clone(),
                });
            }
        }
    }

    if all_entries.is_empty() {
        println!("No remote sessions found.");
        return Ok(());
    }

    // Sort all entries globally by mtime DESC.
    all_entries.sort_by_key(|e| Reverse(e.meta.modified_at));

    // Fetch all cache misses concurrently.
    let misses: Vec<(usize, String, DateTime<Utc>, u64)> = all_entries
        .iter()
        .enumerate()
        .filter_map(|(idx, entry)| {
            let (mtime, size) = object_index[&entry.meta_key];
            if meta_cache.get_if_fresh(&entry.meta_key, mtime, size).is_some() {
                None
            } else {
                Some((idx, entry.meta_key.clone(), mtime, size))
            }
        })
        .collect();

    let fetched: Vec<(String, SessionMeta, DateTime<Utc>, u64)> =
        stream::iter(misses.into_iter().map(|(_, mk, mtime, size)| (mk, mtime, size)))
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

    // Populate real metas from cache into all_entries.
    for entry in &mut all_entries {
        let (mtime, size) = object_index[&entry.meta_key];
        if let Some(m) = meta_cache.get_if_fresh(&entry.meta_key, mtime, size) {
            entry.meta = m.clone();
        }
    }

    let multi_tool = adapters.len() > 1;

    // Build project-level picker labels (one per unique project-key+adapter combination).
    // Deduplicate: show only the most-recent entry per (adapter, project_key).
    let mut seen: std::collections::HashSet<(usize, String)> = std::collections::HashSet::new();
    let mut project_entries: Vec<&ResumeEntry> = Vec::new();
    for entry in &all_entries {
        let key = (entry.adapter_index, entry.project_key.clone());
        if seen.insert(key) {
            project_entries.push(entry);
        }
    }

    let project_labels: Vec<String> = project_entries
        .iter()
        .map(|entry| {
            let cwd = &entry.meta.source_cwd;
            let pk_abbrev = &entry.project_key[..entry.project_key.len().min(8)];
            if multi_tool {
                format!("[{}] {}  ({}…)", entry.adapter_name, cwd, pk_abbrev)
            } else {
                format!("{}  ({}…)", cwd, pk_abbrev)
            }
        })
        .collect();

    let Some(project_pick) = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Pick a project")
        .items(&project_labels)
        .default(0)
        .interact_opt()?
    else {
        println!("Cancelled.");
        if let Some(p) = &cache_path {
            if let Err(e) = meta_cache.save(p, key) {
                tracing::debug!("meta-cache save failed (non-fatal): {e}");
            }
        }
        return Ok(());
    };

    let chosen_project = project_entries[project_pick];
    let chosen_adapter = &adapters[chosen_project.adapter_index];
    let chosen_pk = &chosen_project.project_key;

    // ── Step 2: pick a session within the chosen project ─────────────────────
    // Fetch all sessions for this adapter/project.
    let session_prefix = format!("{}/{}/", chosen_adapter.name(), chosen_pk);
    let session_objects_all = storage.list(&session_prefix).await?;

    let mut session_objects: Vec<StorageObject> = session_objects_all
        .into_iter()
        .filter(|o| o.key.ends_with(".meta.json"))
        .collect();
    session_objects.sort_by_key(|o| Reverse(o.last_modified));

    // Fetch any session-level cache misses.
    let session_misses: Vec<(String, DateTime<Utc>, u64)> = session_objects
        .iter()
        .filter_map(|obj| {
            let (mtime, size) = *object_index.get(&obj.key).unwrap_or(&(obj.last_modified, obj.size));
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
            let (mtime, size) = *object_index.get(&obj.key).unwrap_or(&(obj.last_modified, obj.size));
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
        if let Some(p) = &cache_path {
            if let Err(e) = meta_cache.save(p, key) {
                tracing::debug!("meta-cache save failed (non-fatal): {e}");
            }
        }
        return Ok(());
    };
    let chosen_meta = &session_metas[session_pick];

    // ── Step 3: download + decrypt the actual session bytes ───────────────────
    let session_key = format!(
        "{}/{}/{}.age",
        chosen_adapter.name(),
        chosen_pk,
        chosen_meta.session_id.0
    );
    let ct = storage.get(&session_key).await?;
    let pt = crypto::decrypt(&ct, key)?;

    // ── Step 4: write into target cwd via the chosen adapter ──────────────────
    let target_cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let written = chosen_adapter
        .write_session(&chosen_meta.session_id, &target_cwd, &pt)
        .await?;

    println!("\nSession dropped at: {}", written.display());
    println!("Run: {} --resume {}", chosen_adapter.name(), chosen_meta.session_id);

    // Best-effort cache save — never fail resume because of this.
    if let Some(p) = &cache_path {
        if let Err(e) = meta_cache.save(p, key) {
            tracing::debug!("meta-cache save failed (non-fatal): {e}");
        }
    }

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

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}
