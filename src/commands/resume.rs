use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::StorageAdapter;
use crate::adapter::tool::ToolAdapter;
use crate::config::Config;
use crate::crypto;
use crate::keychain;
use crate::types::SessionMeta;
use anyhow::{Context, Result};
use dialoguer::{Select, theme::ColorfulTheme};
use std::collections::BTreeMap;

pub async fn run() -> Result<()> {
    let cfg = Config::load(&Config::default_path()).context("load config")?;
    let passphrase = keychain::load_passphrase()?;
    let salt = decode_salt(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;

    let tool = ClaudeCodeAdapter::new();
    let storage = OssStorage::new(&cfg.oss)?;

    resume_interactive(&tool, &storage, &key).await
}

pub async fn resume_interactive<T: ToolAdapter, S: StorageAdapter>(
    tool: &T,
    storage: &S,
    key: &[u8; 32],
) -> Result<()> {
    let prefix = format!("{}/", tool.name());
    let objects = storage.list(&prefix).await?;

    // Object key layout: {tool}/{project_key}/{session_id}.age (content)
    // and {tool}/{project_key}/{session_id}.age.meta.json (encrypted meta sidecar).
    // Both written by push.rs; meta sidecar = encrypted SessionMeta JSON.
    let mut by_project: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for o in &objects {
        if !o.key.ends_with(".meta.json") { continue; }  // index from meta files only
        let parts: Vec<&str> = o.key.splitn(3, '/').collect();
        if parts.len() < 3 { continue; }
        by_project.entry(parts[1].to_string()).or_default().push(o.key.clone());
    }

    if by_project.is_empty() {
        println!("No remote sessions found.");
        return Ok(());
    }

    // Step 1: pick a project, displayed by source_cwd from any one of its sessions' meta.
    let project_keys: Vec<String> = by_project.keys().cloned().collect();
    let mut project_labels: Vec<String> = vec![];
    let mut project_metas_first: Vec<SessionMeta> = vec![];
    for pk in &project_keys {
        let any_meta_key = &by_project[pk][0];
        let raw = storage.get(any_meta_key).await?;
        let pt = crypto::decrypt(&raw, key)?;
        let meta: SessionMeta = serde_json::from_slice(&pt)?;
        project_labels.push(format!("{}  ({})", meta.source_cwd, pk));
        project_metas_first.push(meta);
    }

    let pick = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Pick a project")
        .items(&project_labels)
        .default(0)
        .interact()?;
    let chosen_pk = &project_keys[pick];

    // Step 2: pick a session within the project.
    let session_meta_keys = &by_project[chosen_pk];
    let mut session_labels: Vec<String> = vec![];
    let mut session_metas: Vec<SessionMeta> = vec![];
    for mk in session_meta_keys {
        let raw = storage.get(mk).await?;
        let pt = crypto::decrypt(&raw, key)?;
        let meta: SessionMeta = serde_json::from_slice(&pt)?;
        session_labels.push(format!(
            "[{}] {}  — {}",
            meta.modified_at.format("%Y-%m-%d %H:%M"),
            truncate(&meta.preview, 50),
            meta.source_hostname,
        ));
        session_metas.push(meta);
    }

    let pick = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Pick a session")
        .items(&session_labels)
        .default(0)
        .interact()?;
    let chosen_meta = &session_metas[pick];

    // Step 3: download + decrypt the actual session bytes.
    let session_key = format!("{}/{}/{}.age", tool.name(), chosen_pk, chosen_meta.session_id.0);
    let ct = storage.get(&session_key).await?;
    let pt = crypto::decrypt(&ct, key)?;

    // Step 4: write into target cwd (default = current working dir).
    let target_cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let written = tool.write_session(&chosen_meta.session_id, &target_cwd, &pt).await?;

    println!("\nSession dropped at: {}", written.display());
    println!("Run: claude --resume {}", chosen_meta.session_id);
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() }
    else { s.chars().take(n - 1).collect::<String>() + "…" }
}

fn decode_salt(hex_str: &str) -> Result<[u8; 16]> {
    let bytes = hex::decode(hex_str).context("salt hex decode")?;
    bytes.try_into().map_err(|_| anyhow::anyhow!("salt must be 16 bytes"))
}
