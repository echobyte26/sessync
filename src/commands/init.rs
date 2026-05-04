use crate::config::{Config, DeviceConfig, OssConfig};
use crate::keychain;
use anyhow::Result;
use dialoguer::{Confirm, Input, Password};
use rand::RngCore;

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

pub async fn run() -> Result<()> {
    println!("sessync init — first-time setup\n");

    // DATA LOSS GUARD: detect existing config + keychain entry. Re-running init
    // generates a fresh salt and overwrites the keychain entry, which would
    // orphan every already-encrypted OSS session.
    let existing_config = Config::default_path();
    let has_config = existing_config.exists();
    let has_passphrase = keychain::passphrase_is_set().unwrap_or(false);

    if has_config || has_passphrase {
        eprintln!("WARNING: Existing sessync configuration detected:");
        if has_config {
            eprintln!("   - config file: {}", existing_config.display());
        }
        if has_passphrase {
            eprintln!("   - passphrase in macOS Keychain");
        }
        eprintln!("\nRe-running init will generate a NEW salt and store a NEW passphrase.");
        eprintln!("All sessions previously encrypted with the old passphrase will become");
        eprintln!("UNRECOVERABLE from OSS — even if you remember the old one, the salt is gone.");
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

    let endpoint: String = Input::new()
        .with_prompt("OSS endpoint (e.g. oss-cn-hangzhou.aliyuncs.com)")
        .validate_with(require_nonempty)
        .interact_text()?;
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

    println!("\nPick a strong passphrase. Sessions are encrypted with it before upload.");
    println!("If you forget it, your remote sessions are unrecoverable.\n");
    let passphrase = Password::new()
        .with_prompt("Passphrase")
        .with_confirmation("Confirm passphrase", "Mismatch")
        .interact()?;

    if passphrase.chars().count() < 12 {
        eprintln!(
            "Warning: passphrases under 12 characters are weak against offline attacks once an attacker has any encrypted blob."
        );
    }

    // Generate per-install salt + device id.
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let device_id = uuid::Uuid::new_v4().to_string();
    let hostname = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let cfg = Config {
        oss: OssConfig {
            endpoint,
            bucket,
            access_key_id,
            access_key_secret,
            prefix,
        },
        device: DeviceConfig {
            device_id,
            hostname,
        },
        kdf_salt_hex: hex::encode(salt),
    };

    // Atomic init: write keychain FIRST (more likely to fail — auth prompt,
    // locked keychain), then config. If config save fails, roll back the
    // keychain entry so the next `init` doesn't trip the data-loss guard
    // on an unrecoverable mid-state.
    keychain::store_passphrase(&passphrase)?;
    let path = Config::default_path();
    if let Err(e) = cfg.save(&path) {
        let _ = keychain::delete_passphrase();
        return Err(e.into());
    }

    println!("\nPassphrase stored in macOS Keychain.");
    println!("Config saved to {}", path.display());
    println!("\nDone. Run `sessync status` to verify, then `sessync push` to upload.");
    Ok(())
}
