//! Cross-platform machine-bound passphrase storage.
//!
//! Replaces macOS Keychain. Stores the user's passphrase at
//! ~/.config/sessync/passphrase.enc (chmod 0600 on Unix). The file is
//! encrypted under a key derived from the machine's stable identifier:
//!   key = HMAC-SHA256(machine_id, "sessync-passphrase-v1")
//!
//! Properties:
//!   - install never prompts (it's a file write, no OS dialog)
//!   - upgrade never prompts (key derivation is binary-independent)
//!   - cross-platform (machine_id resolved per-OS by `machine-uid` crate)
//!   - leaked file is useless without same machine_id

use crate::error::{Result, SessyncError};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "passphrase.enc";
const KEY_DERIVATION_DOMAIN: &[u8] = b"sessync-passphrase-v1";

pub fn default_passphrase_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| SessyncError::Config("$HOME not set".into()))?;
    Ok(PathBuf::from(home).join(".config/sessync").join(FILE_NAME))
}

/// Compute the machine-bound encryption key. Always 32 bytes.
fn derive_machine_key() -> Result<[u8; 32]> {
    let machine_id = machine_uid::get()
        .map_err(|e| SessyncError::Config(format!("read machine id: {e}")))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(machine_id.as_bytes())
        .map_err(|e| SessyncError::Crypto(format!("hmac key: {e}")))?;
    mac.update(KEY_DERIVATION_DOMAIN);
    let result = mac.finalize().into_bytes();
    let mut key = [0u8; 32];
    key.copy_from_slice(&result);
    Ok(key)
}

/// Store the passphrase. Overwrites existing.
pub fn store_passphrase(passphrase: &str) -> Result<()> {
    let path = default_passphrase_path()?;
    store_passphrase_at(passphrase, &path)
}

pub fn store_passphrase_at(passphrase: &str, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SessyncError::Config(format!("mkdir {}: {e}", parent.display())))?;
    }
    let key = derive_machine_key()?;
    let ciphertext = crate::crypto::encrypt(passphrase.as_bytes(), &key)?;
    let tmp = path.with_extension("enc.tmp");
    std::fs::write(&tmp, &ciphertext)
        .map_err(|e| SessyncError::Config(format!("write tmp {}: {e}", tmp.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| SessyncError::Config(format!("chmod tmp: {e}")))?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| SessyncError::Config(format!("rename {} -> {}: {e}", tmp.display(), path.display())))?;
    Ok(())
}

pub fn load_passphrase() -> Result<String> {
    let path = default_passphrase_path()?;

    // Fast path: file already exists.
    if path.exists() {
        return load_passphrase_at(&path);
    }

    // v0.1.x → v0.2.x migration: try the deprecated macOS Keychain.
    // If found, copy it to the new file and clear the keychain entry,
    // so subsequent runs hit the fast path with no migration cost.
    #[cfg(target_os = "macos")]
    {
        if let Ok(p) = crate::keychain::load_passphrase() {
            tracing::info!(
                "migrated passphrase from macOS Keychain to {}",
                path.display()
            );
            store_passphrase_at(&p, &path)?;
            // Best-effort cleanup — keep going even if the keychain delete fails
            // (the entry will just be ignored on next run since the file exists).
            let _ = crate::keychain::delete_passphrase();
            return Ok(p);
        }
    }

    // No file, no migration source — return the standard "run init" error.
    load_passphrase_at(&path)
}

pub fn load_passphrase_at(path: &Path) -> Result<String> {
    if !path.exists() {
        return Err(SessyncError::Config(format!(
            "passphrase not found at {} — run `sessync init`", path.display()
        )));
    }
    let key = derive_machine_key()?;
    let ciphertext = std::fs::read(path)
        .map_err(|e| SessyncError::Config(format!("read {}: {e}", path.display())))?;
    let plaintext = crate::crypto::decrypt(&ciphertext, &key)
        .map_err(|_| SessyncError::Crypto(
            "passphrase file unreadable — likely created on a different machine".into()
        ))?;
    String::from_utf8(plaintext)
        .map_err(|e| SessyncError::Config(format!("passphrase not valid UTF-8: {e}")))
}

/// Returns whether a passphrase is set on this machine. No prompt, no error
/// surface — used by status and the data-loss guard.
///
/// Also reports `true` if a v0.1.x macOS Keychain entry exists (which would
/// be migrated on next `load_passphrase`), so status correctly shows "set"
/// for users who haven't yet run any command that triggers the migration.
pub fn passphrase_is_set() -> bool {
    let file_exists = default_passphrase_path()
        .map(|p| p.exists())
        .unwrap_or(false);
    if file_exists {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        if crate::keychain::passphrase_is_set().unwrap_or(false) {
            return true;
        }
    }
    false
}

/// Delete the passphrase file. Idempotent.
pub fn delete_passphrase() -> Result<()> {
    let path = default_passphrase_path()?;
    delete_passphrase_at(&path)
}

pub fn delete_passphrase_at(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SessyncError::Config(format!("delete {}: {e}", path.display()))),
    }
}
