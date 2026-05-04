use crate::error::{Result, SessyncError};
use age::secrecy::SecretString;
use argon2::{Algorithm, Argon2, Params, Version};
use std::io::{Read, Write};

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

/// Encrypt with a symmetric key by wrapping it as an age scrypt recipient.
/// The 32-byte key is hex-encoded into a SecretString — age uses it as the passphrase.
pub fn encrypt(plaintext: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    let pass = SecretString::from(hex::encode(key));
    let encryptor = age::Encryptor::with_user_passphrase(pass);
    let mut out = vec![];
    let mut w = encryptor
        .wrap_output(&mut out)
        .map_err(|e| SessyncError::Crypto(format!("age wrap: {e}")))?;
    w.write_all(plaintext)?;
    w.finish()
        .map_err(|e| SessyncError::Crypto(format!("age finish: {e}")))?;
    Ok(out)
}

pub fn decrypt(ciphertext: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    let pass = SecretString::from(hex::encode(key));
    let decryptor = age::Decryptor::new(ciphertext)
        .map_err(|e| SessyncError::Crypto(format!("age open: {e}")))?;
    let mut r = match decryptor {
        age::Decryptor::Passphrase(d) => d
            .decrypt(&pass, None)
            .map_err(|e| SessyncError::Crypto(format!("age decrypt: {e}")))?,
        _ => {
            return Err(SessyncError::Crypto(
                "expected passphrase-encrypted age file".into(),
            ))
        }
    };
    let mut buf = vec![];
    r.read_to_end(&mut buf)?;
    Ok(buf)
}
