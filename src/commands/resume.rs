use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::{StorageAdapter, StorageObject};
use crate::adapter::tool::ToolAdapter;
use crate::cache::{self, MetaCache};
use crate::config::{Config, StorageKind};
use crate::crypto;
use crate::keychain;
use crate::types::SessionMeta;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use dialoguer::{theme::ColorfulTheme, Select};
use futures::stream::{self, StreamExt, TryStreamExt};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

pub async fn run() -> Result<()> {
    let cfg = Config::load(&Config::default_path()).context("load config")?;
    let passphrase = keychain::load_passphrase()?;
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
            resume_interactive(&tool, &storage, &key).await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            resume_interactive(&tool, &storage, &key).await
        }
    }
}

pub async fn resume_interactive<T: ToolAdapter, S: StorageAdapter>(
    tool: &T,
    storage: &S,
    key: &[u8; 32],
) -> Result<()> {
    let prefix = format!("{}/", tool.name());
    // Always list — this is the truth source and our cache-invalidation signal.
    let objects = storage.list(&prefix).await?;

    // Load cache (or empty on any error — cache is purely an optimisation,
    // never block resume on cache problems including HOME unset).
    let cache_path: Option<PathBuf> = cache::default_cache_path().ok();
    let mut meta_cache = match &cache_path {
        Some(p) => MetaCache::load_or_empty(p, key, tool.name()),
        None => {
            tracing::debug!("HOME not set — running without meta cache");
            MetaCache::empty(tool.name())
        }
    };

    // Build an owned index: object_key → (mtime, size).
    // Using String keys avoids lifetime conflicts with `objects`.
    let object_index: HashMap<String, (DateTime<Utc>, u64)> = objects
        .iter()
        .map(|o| (o.key.clone(), (o.last_modified, o.size)))
        .collect();

    // Tombstone: drop cache entries whose key no longer exists in OSS.
    let present_keys: HashSet<&str> = object_index.keys().map(|k| k.as_str()).collect();
    meta_cache.retain_only(&present_keys);

    // Object key layout: {tool}/{project_key}/{session_id}.age (content)
    // and {tool}/{project_key}/{session_id}.age.meta.json (encrypted meta sidecar).
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
        println!("No remote sessions found.");
        return Ok(());
    }

    // ── Step 1: pick a project ────────────────────────────────────────────────
    let project_keys: Vec<String> = by_project.keys().cloned().collect();

    // Collect the (key, mtime, size) triples that are cache misses.
    let project_misses: Vec<(String, DateTime<Utc>, u64)> = project_keys
        .iter()
        .filter_map(|pk| {
            let obj = &by_project[pk][0];
            let (mtime, size) = object_index[&obj.key];
            if meta_cache.get_if_fresh(&obj.key, mtime, size).is_some() {
                None // cache hit — skip
            } else {
                Some((obj.key.clone(), mtime, size))
            }
        })
        .collect();

    // Fetch all project-label misses concurrently.
    let fetched_project: Vec<(String, SessionMeta, DateTime<Utc>, u64)> =
        stream::iter(project_misses)
            .map(|(mk, mtime, size)| async move {
                let raw = storage.get(&mk).await?;
                let pt = crypto::decrypt(&raw, key)?;
                let meta: SessionMeta = serde_json::from_slice(&pt)?;
                anyhow::Ok((mk, meta, mtime, size))
            })
            .buffered(8)
            .try_collect()
            .await?;

    // Insert fetched results into cache.
    for (mk, meta, mtime, size) in fetched_project {
        meta_cache.insert(mk, meta, mtime, size);
    }

    // Build project labels — all representative metas are now in cache.
    let project_labels: Vec<String> = project_keys
        .iter()
        .map(|pk| {
            let obj = &by_project[pk][0];
            let (mtime, size) = object_index[&obj.key];
            let meta = meta_cache
                .get_if_fresh(&obj.key, mtime, size)
                .expect("just fetched or was already cached");
            format!("{}  ({})", meta.source_cwd, pk)
        })
        .collect();

    let Some(pick) = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Pick a project")
        .items(&project_labels)
        .default(0)
        .interact_opt()?
    else {
        println!("Cancelled.");
        // Save cache on cancellation — we already paid for those fetches.
        if let Some(p) = &cache_path {
            if let Err(e) = meta_cache.save(p, key) {
                tracing::debug!("meta-cache save failed (non-fatal): {e}");
            }
        }
        return Ok(());
    };
    let chosen_pk = &project_keys[pick];

    // ── Step 2: pick a session within the project ─────────────────────────────
    let session_objects = &by_project[chosen_pk];

    // Collect session-level misses.
    let session_misses: Vec<(String, DateTime<Utc>, u64)> = session_objects
        .iter()
        .filter_map(|obj| {
            let (mtime, size) = object_index[&obj.key];
            if meta_cache.get_if_fresh(&obj.key, mtime, size).is_some() {
                None
            } else {
                Some((obj.key.clone(), mtime, size))
            }
        })
        .collect();

    // Concurrent fetch for session misses.
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

    // Build (label, meta) pairs — all session metas are now in cache.
    let pairs: Vec<(String, SessionMeta)> = session_objects
        .iter()
        .map(|obj| {
            let (mtime, size) = object_index[&obj.key];
            let meta = meta_cache
                .get_if_fresh(&obj.key, mtime, size)
                .expect("just fetched or was already cached")
                .clone();
            let label = format!(
                "[{}] {}  — {}",
                meta.modified_at.format("%Y-%m-%d %H:%M"),
                truncate(&meta.preview, 50),
                meta.source_hostname,
            );
            (label, meta)
        })
        .collect();

    let (session_labels, session_metas): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();

    let Some(pick) = Select::with_theme(&ColorfulTheme::default())
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
    let chosen_meta = &session_metas[pick];

    // ── Step 3: download + decrypt the actual session bytes ───────────────────
    let session_key = format!(
        "{}/{}/{}.age",
        tool.name(),
        chosen_pk,
        chosen_meta.session_id.0
    );
    let ct = storage.get(&session_key).await?;
    let pt = crypto::decrypt(&ct, key)?;

    // ── Step 4: write into target cwd ─────────────────────────────────────────
    let target_cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let written = tool
        .write_session(&chosen_meta.session_id, &target_cwd, &pt)
        .await?;

    println!("\nSession dropped at: {}", written.display());
    println!("Run: claude --resume {}", chosen_meta.session_id);

    // Best-effort cache save — never fail resume because of this.
    if let Some(p) = &cache_path {
        if let Err(e) = meta_cache.save(p, key) {
            tracing::debug!("meta-cache save failed (non-fatal): {e}");
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
