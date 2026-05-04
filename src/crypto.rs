use crate::error::{Result, SessyncError};
use argon2::{Algorithm, Argon2, Params, Version};

/// Derive a 32-byte symmetric key from a passphrase using argon2id.
/// Parameters: m=64MiB, t=3, p=4. These are deliberately strong — the user
/// only types the passphrase at config-load time, so the cost is acceptable.
pub fn derive_key(passphrase: &str, salt: &[u8; 16]) -> Result<[u8; 32]> {
    let params = Params::new(
        65536,  // 64 MiB
        3,      // 3 iterations
        4,      // 4 lanes
        Some(32),
    ).map_err(|e| SessyncError::Crypto(format!("argon2 params: {e}")))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    argon2.hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|e| SessyncError::Crypto(format!("argon2 hash: {e}")))?;
    Ok(out)
}
