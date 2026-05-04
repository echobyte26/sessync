use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::StorageAdapter;
use crate::adapter::tool::ToolAdapter;
use crate::config::Config;
use anyhow::{Context, Result};

pub async fn run() -> Result<()> {
    let cfg = Config::load(&Config::default_path()).context("load config")?;
    let tool = ClaudeCodeAdapter::new();
    let storage = OssStorage::new(&cfg.oss)?;

    let local = tool.list_local_sessions().await?;
    let remote = storage.list(&format!("{}/", tool.name())).await?;

    let remote_sessions = remote
        .iter()
        .filter(|o| o.key.ends_with(".age") && !o.key.contains(".meta."))
        .count();
    let last_remote = remote.iter().map(|o| o.last_modified).max();

    println!(
        "device:       {} ({})",
        cfg.device.hostname, cfg.device.device_id
    );
    println!("local sessions:  {}", local.len());
    println!("remote sessions: {}", remote_sessions);
    if let Some(t) = last_remote {
        println!("last remote upload: {}", t.format("%Y-%m-%d %H:%M:%S UTC"));
    }
    let passphrase_state = match crate::keychain::passphrase_is_set() {
        Ok(true) => "set",
        Ok(false) => "MISSING (run `sessync init`)",
        Err(_) => "ERROR (Keychain locked or unreadable)",
    };
    println!("passphrase:   {}", passphrase_state);
    println!(
        "storage:      oss://{} (prefix {})",
        cfg.oss.bucket, cfg.oss.prefix
    );
    Ok(())
}
