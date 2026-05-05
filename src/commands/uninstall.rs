use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::StorageAdapter;
use crate::cache;
use crate::config::{Config, StorageKind};
use crate::passphrase_store;
use anyhow::{Context, Result};
use dialoguer::Input;
use futures::stream::{self, StreamExt};
use std::path::PathBuf;

/// Everything detected about the current installation.
pub(crate) struct UninstallPlan {
    pub passphrase_set: bool,
    pub config_path: Option<PathBuf>,
    pub mock_store: Option<(PathBuf, usize)>, // (path, file count)
    pub binary_path: PathBuf,
    pub binary_size: u64,
    pub remote_count: Option<usize>,  // None = not purging or not enumerable
    pub remote_label: Option<String>,
}

/// Delete all objects under the storage adapter's root prefix and return the count.
pub async fn purge_remote_objects(storage: &dyn StorageAdapter) -> Result<usize> {
    let objects = storage.list("").await.context("list remote objects")?;
    let count = objects.len();
    if count == 0 {
        return Ok(0);
    }
    let keys: Vec<String> = objects.into_iter().map(|o| o.key).collect();
    let results: Vec<Result<()>> = stream::iter(keys.iter().map(|k| {
        let key = k.clone();
        async move {
            storage.delete(&key).await.with_context(|| format!("delete remote object {key}"))
        }
    }))
    .buffered(8)
    .collect()
    .await;

    for r in results {
        r?;
    }
    Ok(count)
}

/// Format the plan into the human-readable summary string.
pub(crate) fn print_plan(plan: &UninstallPlan, purge_remote: bool) -> String {
    let mut lines = vec![
        "sessync uninstall — remove local installation".to_string(),
        String::new(),
        "Will delete:".to_string(),
    ];

    let mut will_delete_items = vec![];

    if plan.passphrase_set {
        will_delete_items.push("  - sessync passphrase".to_string());
    }
    if let Some(ref cp) = plan.config_path {
        will_delete_items.push(format!("  - {}", cp.display()));
    }
    if let Some((ref mp, count)) = plan.mock_store {
        will_delete_items.push(format!("  - {} ({} files)", mp.display(), count));
    }

    let binary_mb = plan.binary_size as f64 / 1_048_576.0;
    will_delete_items.push(format!(
        "  - {} (this binary, {:.1} MB)",
        plan.binary_path.display(),
        binary_mb,
    ));

    if purge_remote {
        match (&plan.remote_count, &plan.remote_label) {
            (Some(n), Some(label)) => {
                will_delete_items.push(format!("  - {} remote objects under {}", n, label));
                will_delete_items.push(
                    "    (this is irreversible — your passphrase no longer matters once this runs)"
                        .to_string(),
                );
            }
            _ => {
                will_delete_items
                    .push("  (could not enumerate remote, will skip)".to_string());
            }
        }
    }

    lines.extend(will_delete_items);

    lines.push(String::new());
    lines.push("Will NOT touch:".to_string());
    lines.push("  - PATH config in ~/.zshrc / ~/.bashrc".to_string());
    if !purge_remote {
        lines.push(
            "  - Remote backend contents (use --purge-remote to also wipe cloud data)".to_string(),
        );
    }

    lines.join("\n")
}

/// Count the number of files recursively in a directory (non-recursive for simplicity via walkdir-style).
fn count_files_in_dir(path: &std::path::Path) -> usize {
    let mut count = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let ft = entry.file_type();
            if let Ok(ft) = ft {
                if ft.is_dir() {
                    stack.push(entry.path());
                } else if ft.is_file() {
                    count += 1;
                }
            }
        }
    }
    count
}

pub async fn run(purge_remote: bool, yes: bool) -> Result<()> {
    // ── Phase 1: Detect ──────────────────────────────────────────────────────

    // Self binary path.
    let binary_path = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("couldn't locate self executable: {e}"))?
        .canonicalize()
        .unwrap_or_else(|_| {
            std::env::current_exe().expect("current_exe() succeeded moments ago")
        });
    let binary_size = std::fs::metadata(&binary_path)
        .map(|m| m.len())
        .unwrap_or(0);

    // Config path.
    let config_path_candidate = Config::default_path();
    let config_path: Option<PathBuf> = if config_path_candidate.exists() {
        Some(config_path_candidate.clone())
    } else {
        None
    };

    // Mock store: ~/.sessync
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mock_store_path = PathBuf::from(&home).join(".sessync");
    let mock_store: Option<(PathBuf, usize)> = if mock_store_path.exists() {
        let count = count_files_in_dir(&mock_store_path);
        Some((mock_store_path.clone(), count))
    } else {
        None
    };

    // Passphrase file.
    let passphrase_set = passphrase_store::passphrase_is_set();

    // Remote enumeration (only if --purge-remote and config exists).
    let mut remote_count: Option<usize> = None;
    let mut remote_label: Option<String> = None;

    if purge_remote {
        if let Some(ref cp) = config_path {
            match Config::load(cp) {
                Ok(cfg) => {
                    let result = enumerate_remote(&cfg).await;
                    match result {
                        Ok((count, label)) => {
                            remote_count = Some(count);
                            remote_label = Some(label);
                        }
                        Err(e) => {
                            eprintln!(
                                "(could not enumerate remote, will skip: {e})"
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("(could not load config to enumerate remote, will skip: {e})");
                }
            }
        }
    }

    // ── Phase 2: Print plan ──────────────────────────────────────────────────

    let plan = UninstallPlan {
        passphrase_set,
        config_path: config_path.clone(),
        mock_store: mock_store.clone(),
        binary_path: binary_path.clone(),
        binary_size,
        remote_count,
        remote_label: remote_label.clone(),
    };

    let summary = print_plan(&plan, purge_remote);

    // Check "nothing to uninstall" — only if literally no local artifacts exist
    // (binary always exists since we're running it, so this path is effectively
    // unreachable in practice, but we keep it for spec compliance).
    if summary == "Nothing to uninstall." {
        println!("{}", summary);
        return Ok(());
    }

    println!("{}", summary);
    println!();

    // ── Phase 3: Confirm ─────────────────────────────────────────────────────

    if !yes {
        let prompt = if purge_remote {
            "Proceed with deletion (including remote objects)?"
        } else {
            "Proceed with deletion?"
        };

        let confirmed = dialoguer::Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()
            .context("confirmation prompt")?;

        if !confirmed {
            println!("Aborted.");
            return Ok(());
        }

        // Second confirmation for --purge-remote with OSS: type the bucket name.
        if purge_remote {
            if let Some(ref cp) = config_path {
                if let Ok(cfg) = Config::load(cp) {
                    match cfg.storage_kind {
                        StorageKind::Oss => {
                            let bucket = cfg
                                .oss
                                .as_ref()
                                .map(|o| o.bucket.clone())
                                .unwrap_or_else(|| "<unknown>".into());
                            let typed: String = Input::new()
                                .with_prompt(format!(
                                    "Type the bucket name '{}' to confirm remote purge",
                                    bucket
                                ))
                                .interact_text()
                                .context("bucket name confirmation prompt")?;
                            if typed.trim() != bucket {
                                println!("Bucket name didn't match. Aborted.");
                                return Ok(());
                            }
                        }
                        StorageKind::LocalFs => {
                            // For LocalFs, the first confirm is sufficient.
                        }
                    }
                }
            }
        }
    }

    // ── Phase 4: Execute ─────────────────────────────────────────────────────

    // Step 1: OSS purge (if --purge-remote).
    if purge_remote {
        if let (Some(n), Some(_label)) = (plan.remote_count, &plan.remote_label) {
            println!("  Deleting {} remote objects...", n);
            // Reload config fresh for the actual delete operation.
            if let Some(ref cp) = config_path {
                let cfg = Config::load(cp).context("reload config for remote purge")?;
                let deleted = do_remote_purge(&cfg)
                    .await
                    .context("remote purge failed — aborting to avoid orphaning remote data")?;
                println!("  ✓ {} remote objects deleted", deleted);
            }
        }
        // If remote_count is None (enumerate failed), we already printed a skip notice.
    }

    // Step 2: Passphrase file delete.
    passphrase_store::delete_passphrase().context("delete passphrase file")?;
    println!("  ✓ passphrase file removed");

    // Step 3: Config dir delete.
    if let Some(ref cp) = config_path {
        // Remove the parent directory (e.g. ~/.config/sessync/) entirely.
        if let Some(parent) = cp.parent() {
            if parent.exists() {
                std::fs::remove_dir_all(parent)
                    .with_context(|| format!("remove config dir {}", parent.display()))?;
                println!("  ✓ config removed");
            }
        }
    }

    // Step 4: Mock store delete.
    if let Some((ref mp, _)) = mock_store {
        std::fs::remove_dir_all(mp)
            .with_context(|| format!("remove mock store {}", mp.display()))?;
        println!("  ✓ mock store removed");
    }

    // Step 4b: Meta cache delete (if present). Best-effort — this is a
    // non-essential artifact, don't abort uninstall on failure.
    if let Ok(cache_path) = cache::default_cache_path() {
        if cache_path.exists() {
            if let Err(e) = std::fs::remove_file(&cache_path) {
                eprintln!("  (could not remove meta cache {}: {e})", cache_path.display());
            } else {
                println!("  ✓ meta cache removed");
            }
        }
    }

    // Step 5: Self binary delete.
    std::fs::remove_file(&binary_path)
        .with_context(|| format!("remove binary {}", binary_path.display()))?;
    println!("  ✓ binary removed");

    // ── Phase 5: Final tip ───────────────────────────────────────────────────

    println!();
    println!("Uninstalled.");
    println!();
    println!("If you want to also remove the PATH line you added to ~/.zshrc:");
    println!(r#"  sed -i '' '\|export PATH="$HOME/.local/bin:$PATH"|d' ~/.zshrc"#);

    Ok(())
}

/// Return (count, label) for the remote backend in the given config.
async fn enumerate_remote(cfg: &Config) -> Result<(usize, String)> {
    match cfg.storage_kind {
        StorageKind::Oss => {
            let oss_cfg = cfg
                .oss
                .as_ref()
                .context("storage_kind = oss but [oss] section missing")?;
            let storage = OssStorage::new(oss_cfg).context("init OssStorage")?;
            let objects = storage.list("").await.context("list remote objects")?;
            let label = format!("oss://{}/{}", oss_cfg.bucket, oss_cfg.prefix);
            Ok((objects.len(), label))
        }
        StorageKind::LocalFs => {
            let lf_cfg = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf_cfg.root).context("init LocalFsStorage")?;
            let objects = storage.list("").await.context("list remote objects")?;
            let label = lf_cfg.root.display().to_string();
            Ok((objects.len(), label))
        }
    }
}

/// Actually execute the remote purge, returning number of objects deleted.
async fn do_remote_purge(cfg: &Config) -> Result<usize> {
    match cfg.storage_kind {
        StorageKind::Oss => {
            let oss_cfg = cfg
                .oss
                .as_ref()
                .context("storage_kind = oss but [oss] section missing")?;
            let storage = OssStorage::new(oss_cfg).context("init OssStorage")?;
            purge_remote_objects(&storage).await
        }
        StorageKind::LocalFs => {
            let lf_cfg = cfg
                .local_fs
                .as_ref()
                .context("storage_kind = local-fs but [local_fs] section missing")?;
            let storage = LocalFsStorage::new(&lf_cfg.root).context("init LocalFsStorage")?;
            purge_remote_objects(&storage).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_plan(
        passphrase_set: bool,
        config_path: Option<PathBuf>,
        mock_store: Option<(PathBuf, usize)>,
        remote_count: Option<usize>,
        remote_label: Option<String>,
    ) -> UninstallPlan {
        UninstallPlan {
            passphrase_set,
            config_path,
            mock_store,
            binary_path: PathBuf::from("/home/user/.local/bin/sessync"),
            binary_size: 5_242_880, // 5 MB
            remote_count,
            remote_label,
        }
    }

    #[test]
    fn print_plan_full_with_remote() {
        let plan = make_plan(
            true,
            Some(PathBuf::from("/home/user/.config/sessync/config.toml")),
            Some((PathBuf::from("/home/user/.sessync"), 42)),
            Some(13),
            Some("oss://my-bucket/sessync/".to_string()),
        );
        let output = print_plan(&plan, true);
        assert!(output.contains("sessync uninstall — remove local installation"));
        assert!(output.contains("sessync passphrase"));
        assert!(output.contains("/home/user/.config/sessync/config.toml"));
        assert!(output.contains("/home/user/.sessync (42 files)"));
        assert!(output.contains(".local/bin/sessync (this binary, 5.0 MB)"));
        assert!(output.contains("13 remote objects under oss://my-bucket/sessync/"));
        assert!(output.contains("irreversible"));
        assert!(!output.contains("Remote backend contents"));
    }

    #[test]
    fn print_plan_no_remote_flag() {
        let plan = make_plan(
            true,
            Some(PathBuf::from("/home/user/.config/sessync/config.toml")),
            None,
            None,
            None,
        );
        let output = print_plan(&plan, false);
        assert!(output.contains("Remote backend contents (use --purge-remote"));
        assert!(!output.contains("remote objects under"));
    }

    #[test]
    fn print_plan_no_passphrase_no_mock() {
        let plan = make_plan(false, None, None, None, None);
        let output = print_plan(&plan, false);
        assert!(!output.contains("sessync passphrase"));
        assert!(!output.contains(".sessync"));
        // Binary is always listed.
        assert!(output.contains("this binary"));
    }

    #[test]
    fn print_plan_remote_not_enumerable() {
        let plan = make_plan(true, None, None, None, None);
        let output = print_plan(&plan, true);
        // When purge_remote=true but no remote_count/label, should mention skip.
        assert!(output.contains("could not enumerate remote"));
    }

    #[test]
    fn print_plan_binary_size_formatting() {
        let mut plan = make_plan(false, None, None, None, None);
        plan.binary_size = 2_621_440; // 2.5 MB
        let output = print_plan(&plan, false);
        assert!(output.contains("2.5 MB"));
    }
}
