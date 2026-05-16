use crate::adapter::local_fs::LocalFsStorage;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::StorageAdapter;
use crate::config::{Config, DeviceConfig, LocalFsConfig, OssConfig, StorageKind};
use crate::error::SessyncError;
use crate::passphrase_store;
use crate::ui::style;
use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};
use rand::RngCore;
use std::path::PathBuf;

/// Well-known backend key for the shared KDF salt.
///
/// The salt is stored top-level (under the configured prefix) so all tools
/// sharing one bucket share one salt — matching a single passphrase to a
/// single key regardless of how many `sessync init` calls were run across
/// devices.  The leading `.` keeps it out of normal session listing prefixes
/// which all start with `claude-code/`.
const SHARED_SALT_KEY: &str = ".sessync-salt";

// dialoguer's validate_with callback requires `&String` (it carries Sized + Clone
// bounds in the Input type), so the ptr_arg lint is a false positive here.
#[allow(clippy::ptr_arg)]
fn require_nonempty(s: &String) -> std::result::Result<(), &'static str> {
    if s.trim().is_empty() {
        Err("required (cannot be empty)")
    } else {
        Ok(())
    }
}

fn default_local_fs_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".sessync/local-store")
}

/// Load an existing shared salt from the backend, or generate and upload a
/// fresh one.
///
/// # Error handling
///
/// - **NotFound** (first `init` on this backend): generate a fresh salt and
///   upload it.  Both `OssStorage::get` and `LocalFsStorage::get` embed the
///   string `"not found"` in their `SessyncError::Storage` payload when the
///   object is absent — we own both adapters, so this string-matching is safe
///   and explicit (not a fragile external contract).
/// - **Wrong size**: the stored object exists but has != 16 bytes.  This is
///   unrecoverable without manual intervention; we error rather than silently
///   generating a new salt, which would produce a different key and orphan
///   everything already encrypted.
/// - **Any other error** (network failure, permission denied, …): bubble up.
///   We must never silently fall back to local-only generation — that would
///   re-introduce the very bug this function fixes.
pub async fn load_or_create_salt<S: StorageAdapter>(storage: &S) -> anyhow::Result<[u8; 16]> {
    match storage.get(SHARED_SALT_KEY).await {
        Ok(bytes) => {
            let arr: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
                anyhow::anyhow!(
                    "shared salt at backend key '{}' has wrong size: expected 16 bytes, got {}",
                    SHARED_SALT_KEY,
                    bytes.len()
                )
            })?;
            println!("Found existing salt on backend — your passphrase must match the original.");
            Ok(arr)
        }
        // Both OssStorage and LocalFsStorage emit "not found: <key>" in their
        // NotFound branch.  InMemoryStorage does the same.  We own all three
        // adapters, so matching on this substring is stable.
        Err(SessyncError::Storage(msg)) if msg.contains("not found") => {
            let mut salt = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut salt);
            storage.put(SHARED_SALT_KEY, salt.to_vec()).await?;
            println!(
                "Initialized new salt on backend — record your passphrase carefully \
                 (it's the only way to decrypt these sessions)."
            );
            Ok(salt)
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn run(mock: bool) -> Result<()> {
    println!(
        "{}",
        style::header("sessync init — first-time setup")
    );

    // DATA LOSS GUARD: detect existing config + passphrase file. Re-running init
    // generates a fresh salt and overwrites the passphrase file, which would
    // orphan every already-encrypted session.
    let existing_config = Config::default_path();
    let has_config = existing_config.exists();
    let has_passphrase = passphrase_store::passphrase_is_set();

    if has_config || has_passphrase {
        eprintln!("{}", style::warn("WARNING: Existing sessync configuration detected:"));
        if has_config {
            eprintln!("   - config file: {}", existing_config.display());
        }
        if has_passphrase {
            eprintln!("   - passphrase file at ~/.config/sessync/passphrase.enc");
        }
        eprintln!("\nRe-running init will generate a NEW salt and store a NEW passphrase.");
        eprintln!("All sessions previously encrypted with the old passphrase will become");
        eprintln!("UNRECOVERABLE — even if you remember the old one, the salt is gone.");
        let proceed = Confirm::new()
            .with_prompt(
                "Overwrite existing configuration? (Type 'y' only if you understand the consequences)",
            )
            .default(false)
            .interact()?;
        if !proceed {
            println!("Aborted. Existing configuration left intact.");
            return Ok(());
        }
    }

    let (storage_kind, oss, local_fs) = if mock {
        println!("\n{}", style::header("Local-fs Backend (mock mode)"));
        let root_default = default_local_fs_root();
        let root_input: String = Input::new()
            .with_prompt("Local-fs store root")
            .default(root_default.to_string_lossy().to_string())
            .interact_text()?;
        (
            StorageKind::LocalFs,
            None,
            Some(LocalFsConfig {
                root: PathBuf::from(root_input),
            }),
        )
    } else {
        println!("\n{}", style::header("OSS Backend"));

        let endpoints = [
            "oss-cn-hangzhou.aliyuncs.com (杭州)",
            "oss-cn-shanghai.aliyuncs.com (上海)",
            "oss-cn-beijing.aliyuncs.com (北京)",
            "oss-cn-shenzhen.aliyuncs.com (深圳)",
            "oss-cn-guangzhou.aliyuncs.com (广州)",
            "oss-cn-hongkong.aliyuncs.com (香港)",
            "Custom...",
        ];
        let pick = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("OSS endpoint")
            .items(&endpoints)
            .default(0)
            .interact()?;
        let endpoint = if pick == endpoints.len() - 1 {
            Input::<String>::new()
                .with_prompt("Custom endpoint")
                .interact_text()?
        } else {
            // Strip the "(xx)" suffix — take only the first whitespace-delimited token.
            endpoints[pick]
                .split_whitespace()
                .next()
                .unwrap()
                .to_string()
        };

        let bucket: String = Input::new()
            .with_prompt("OSS bucket name")
            .validate_with(require_nonempty)
            .interact_text()?;
        let access_key_id: String = Input::new()
            .with_prompt("OSS AccessKeyId")
            .validate_with(require_nonempty)
            .interact_text()?;
        let access_key_secret: String = Password::new()
            .with_prompt("OSS AccessKeySecret")
            .validate_with(|s: &String| require_nonempty(s))
            .interact()?;
        let prefix: String = Input::new()
            .with_prompt("Object key prefix")
            .default("sessync/".into())
            .interact_text()?;
        (
            StorageKind::Oss,
            Some(OssConfig {
                endpoint,
                bucket,
                access_key_id,
                access_key_secret,
                prefix,
            }),
            None,
        )
    };

    println!("\n{}", style::header("Encryption"));
    println!("Pick a strong passphrase. Sessions are encrypted with it before storage.");
    println!("If you forget it, your sessions are unrecoverable.\n");
    let passphrase = Password::new()
        .with_prompt("Passphrase")
        .with_confirmation("Confirm passphrase", "Mismatch")
        .interact()?;

    if passphrase.chars().count() < 12 {
        eprintln!(
            "{}",
            style::warn(
                "Warning: passphrases under 12 characters are weak against offline attacks \
                 once an attacker has any encrypted blob."
            )
        );
    }

    // Build the storage adapter so we can check for / upload a shared salt.
    // This MUST come before we derive the KDF key — the salt determines the key.
    // If both devices don't share the same salt they'll produce different keys
    // and neither can decrypt the other's sessions (B1 bug, 2026-05-05).
    let salt = match &storage_kind {
        StorageKind::Oss => {
            let storage =
                OssStorage::new(oss.as_ref().expect("oss is Some when kind is Oss"))?;
            load_or_create_salt(&storage).await?
        }
        StorageKind::LocalFs => {
            let storage = LocalFsStorage::new(
                &local_fs
                    .as_ref()
                    .expect("local_fs is Some when kind is LocalFs")
                    .root,
            )?;
            load_or_create_salt(&storage).await?
        }
    };

    let device_id = uuid::Uuid::new_v4().to_string();
    let hostname = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let cfg = Config {
        storage_kind,
        oss,
        local_fs,
        device: DeviceConfig {
            device_id,
            hostname,
        },
        kdf_salt_hex: hex::encode(salt),
        exclude: crate::config::ExcludeConfig::default(),
    };

    // Atomic init: write passphrase file first, then config. If config save
    // fails, roll back the passphrase file so the next `init` doesn't trip
    // the data-loss guard on an unrecoverable mid-state.
    passphrase_store::store_passphrase(&passphrase)?;
    let path = Config::default_path();
    if let Err(e) = cfg.save(&path) {
        let _ = passphrase_store::delete_passphrase();
        return Err(e.into());
    }

    println!(
        "\n{} Passphrase stored",
        style::success(style::check_ok())
    );
    println!(
        "{} Config saved to {}",
        style::success(style::check_ok()),
        style::value(&path.display().to_string())
    );
    println!(
        "\n{}",
        style::hint("Run `sessync status` to verify, then `sessync push` to upload.")
    );
    Ok(())
}
