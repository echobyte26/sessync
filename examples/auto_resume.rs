//! Smoke-test helper: runs the non-interactive parts of `sessync resume`
//! against the configured backend. Picks the most recently uploaded session
//! and drops it into the current cwd. Use this to validate the end-to-end
//! crypto + storage + write_session path without going through the dialoguer
//! picker (which can't be driven from a non-TTY environment).
//!
//! Run with:
//!   cd /tmp/sessync-resume-test
//!   cargo run --release --example auto_resume --manifest-path /Users/jameschen/Project/ai-coding-project/sessync/Cargo.toml

use sessync::adapter::claude_code::ClaudeCodeAdapter;
use sessync::adapter::local_fs::LocalFsStorage;
use sessync::adapter::oss::OssStorage;
use sessync::adapter::storage::StorageAdapter;
use sessync::adapter::tool::ToolAdapter;
use sessync::config::{Config, StorageKind};
use sessync::types::SessionMeta;
use sessync::{crypto, keychain};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("sessync=info,auto_resume=info")
        .init();

    let cfg = Config::load(&Config::default_path())?;
    let passphrase = keychain::load_passphrase()?;
    let salt = crypto::decode_salt_hex(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;

    let tool = ClaudeCodeAdapter::new();

    // Build a boxed StorageAdapter so the rest of the function is backend-agnostic.
    let storage: Box<dyn StorageAdapter> = match cfg.storage_kind {
        StorageKind::Oss => {
            let oss = cfg.oss.as_ref().expect("storage_kind = oss but [oss] missing");
            Box::new(OssStorage::new(oss)?)
        }
        StorageKind::LocalFs => {
            let lf = cfg.local_fs.as_ref().expect("storage_kind = local-fs but [local_fs] missing");
            Box::new(LocalFsStorage::new(&lf.root)?)
        }
    };

    let prefix = format!("{}/", tool.name());
    let mut objects = storage.list(&prefix).await?;
    println!("[auto_resume] backend listed {} objects", objects.len());

    // Pick the most recently uploaded .meta.json sidecar.
    objects.retain(|o| o.key.ends_with(".meta.json"));
    if objects.is_empty() {
        anyhow::bail!("no remote sessions found");
    }
    objects.sort_by_key(|b| std::cmp::Reverse(b.last_modified)); // newest first
    let chosen_meta_key = objects[0].key.clone();
    println!("[auto_resume] chosen meta key: {chosen_meta_key}");

    // Decrypt the meta to learn session_id + source_cwd.
    let meta_ct = storage.get(&chosen_meta_key).await?;
    let meta_pt = crypto::decrypt(&meta_ct, &key)?;
    let meta: SessionMeta = serde_json::from_slice(&meta_pt)?;
    println!(
        "[auto_resume] picked session {} from {} ({} bytes plaintext)",
        meta.session_id, meta.source_cwd, meta.byte_size
    );

    // Compute the content key and pull it.
    let parts: Vec<&str> = chosen_meta_key.splitn(3, '/').collect();
    if parts.len() < 3 {
        anyhow::bail!("malformed meta key: {chosen_meta_key}");
    }
    let pk = parts[1];
    let content_key = format!("{}/{}/{}.age", tool.name(), pk, meta.session_id.0);
    let ct = storage.get(&content_key).await?;
    let pt = crypto::decrypt(&ct, &key)?;
    println!(
        "[auto_resume] decrypted content: {} bytes, first line: {}",
        pt.len(),
        std::str::from_utf8(&pt)
            .unwrap_or("<not utf-8>")
            .lines()
            .next()
            .unwrap_or("<empty>")
            .chars()
            .take(80)
            .collect::<String>()
    );

    // Write into the *current* cwd via tool.write_session.
    let target_cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let written = tool
        .write_session(&meta.session_id, &target_cwd, &pt)
        .await?;

    println!();
    println!("Session dropped at: {}", written.display());
    println!("Run: claude --resume {}", meta.session_id);
    println!();
    println!("(target_cwd = {})", target_cwd);

    Ok(())
}
