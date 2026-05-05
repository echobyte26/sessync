use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::StorageAdapter;
use crate::adapter::tool::ToolAdapter;
use crate::config::{Config, StorageKind};
use crate::crypto;
use crate::keychain;
use anyhow::{Context, Result};
use tracing::info;

pub async fn run() -> Result<()> {
    let cfg =
        Config::load(&Config::default_path()).context("load config (run `sessync init` first?)")?;
    let passphrase = keychain::load_passphrase().context("load passphrase from keychain")?;
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
            push_all(&tool, &storage, &key).await
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            push_all(&tool, &storage, &key).await
        }
    }
}

pub async fn push_all<T: ToolAdapter, S: StorageAdapter>(
    tool: &T,
    storage: &S,
    key: &[u8; 32],
) -> Result<()> {
    let sessions = tool.list_local_sessions().await?;
    info!("found {} local sessions", sessions.len());

    let mut pushed = 0usize;
    for s in sessions {
        // Read directly from the resolved local_path — no need to re-walk
        // the project tree via tool.read_session(session_id).
        let raw = tokio::fs::read(&s.local_path).await.map_err(|e| {
            anyhow::anyhow!(
                "read {} ({}): {e}",
                s.meta.session_id,
                s.local_path.display()
            )
        })?;

        // Object key layout: {tool}/{project_key}/{session_id}.age
        let object_key = format!(
            "{}/{}/{}.age",
            tool.name(),
            s.meta.project_key.0,
            s.meta.session_id.0
        );
        let meta_key = format!("{}.meta.json", object_key);

        let ciphertext = crypto::encrypt(&raw, key)
            .map_err(|e| anyhow::anyhow!("encrypt {}: {e}", s.meta.session_id))?;
        let meta_json = serde_json::to_vec(&s.meta)?;
        let meta_ciphertext = crypto::encrypt(&meta_json, key)
            .map_err(|e| anyhow::anyhow!("encrypt meta {}: {e}", s.meta.session_id))?;

        storage
            .put(&object_key, ciphertext)
            .await
            .map_err(|e| anyhow::anyhow!("upload {}: {e}", object_key))?;
        storage
            .put(&meta_key, meta_ciphertext)
            .await
            .map_err(|e| anyhow::anyhow!("upload meta {}: {e}", meta_key))?;

        info!(
            "pushed {} ({} plaintext bytes)",
            s.meta.session_id, s.meta.byte_size
        );
        pushed += 1;
    }
    println!("pushed {pushed} sessions");
    Ok(())
}
