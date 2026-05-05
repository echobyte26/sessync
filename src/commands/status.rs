use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::{StorageAdapter, StorageObject};
use crate::adapter::tool::ToolAdapter;
use crate::config::{Config, StorageKind};
use anyhow::{Context, Result};

pub async fn run() -> Result<()> {
    let cfg = Config::load(&Config::default_path()).context("load config")?;
    let tool = ClaudeCodeAdapter::new();

    let local = tool.list_local_sessions().await?;
    let prefix = format!("{}/", tool.name());
    let (remote, storage_label) = match cfg.storage_kind {
        StorageKind::Oss => {
            let oss = cfg
                .oss
                .as_ref()
                .context("storage_kind = oss but [oss] section missing")?;
            let storage = OssStorage::new(oss)?;
            let listed = storage.list(&prefix).await?;
            (
                listed,
                format!("oss://{} (prefix {})", oss.bucket, oss.prefix),
            )
        }
        StorageKind::LocalFs => {
            let lf = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf.root)?;
            let listed = storage.list(&prefix).await?;
            (listed, format!("local-fs://{}", lf.root.display()))
        }
    };

    let remote_sessions = remote
        .iter()
        .filter(|o| o.key.ends_with(".age") && !o.key.contains(".meta."))
        .count();
    let last_remote = remote.iter().map(|o: &StorageObject| o.last_modified).max();

    println!(
        "device:       {} ({})",
        cfg.device.hostname, cfg.device.device_id
    );
    println!("backend:      {:?}", cfg.storage_kind);
    println!("local sessions:  {}", local.len());
    println!("remote sessions: {}", remote_sessions);
    if let Some(t) = last_remote {
        println!("last remote upload: {}", t.format("%Y-%m-%d %H:%M:%S UTC"));
    }
    let passphrase_state = if crate::passphrase_store::passphrase_is_set() {
        "set"
    } else {
        "MISSING (run `sessync init`)"
    };
    println!("passphrase:   {}", passphrase_state);
    println!("storage:      {}", storage_label);
    Ok(())
}
