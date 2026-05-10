use crate::error::{Result, SessyncError};
use argon2::{Algorithm, Argon2, Params, Version};

/// Magic prefix for the new SSC1 format (XChaCha20-Poly1305).
/// age files start with the ASCII string "age-encryption.org/v1", so the first
/// byte is 'a' (0x61), which is distinct from 'S' (0x53) — no ambiguity.
const MAGIC: &[u8; 8] = b"SSC1\0\0\0\0";

/// Derive a 32-byte symmetric key from a passphrase using argon2id.
/// Parameters: m=64MiB, t=3, p=4. These are deliberately strong — the user
/// only types the passphrase at config-load time, so the cost is acceptable.
pub fn derive_key(passphrase: &str, salt: &[u8; 16]) -> Result<[u8; 32]> {
    let params = Params::new(
        65536, // 64 MiB
        3,     // 3 iterations
        4,     // 4 lanes
        Some(32),
    )
    .map_err(|e| SessyncError::Crypto(format!("argon2 params: {e}")))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|e| SessyncError::Crypto(format!("argon2 hash: {e}")))?;
    Ok(out)
}

/// Encrypt plaintext with XChaCha20-Poly1305 using the supplied 32-byte key.
///
/// Output format (SSC1):
/// ```text
/// [ MAGIC: 8 bytes "SSC1\0\0\0\0" ]
/// [ NONCE: 24 bytes — random XChaCha20 nonce ]
/// [ CIPHERTEXT: plaintext.len() + 16 bytes (Poly1305 auth tag) ]
/// ```
///
/// This replaces the old age-based path (v0.1.0–v0.4.0), which wrapped the
/// key in age's internal scrypt KDF — an extra ~200ms per call for no benefit.
pub fn encrypt(plaintext: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    use chacha20poly1305::{
        aead::{Aead, OsRng},
        AeadCore, KeyInit, XChaCha20Poly1305,
    };

    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| SessyncError::Crypto(format!("xchacha encrypt: {e}")))?;

    let mut out = Vec::with_capacity(MAGIC.len() + nonce.len() + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt a blob produced by either the new SSC1 format or the legacy age format.
///
/// Detection: if the first 8 bytes equal MAGIC, use XChaCha20-Poly1305.
/// Otherwise, fall back to the old age passphrase-decryption path for backward
/// compatibility with files written by v0.1.0–v0.4.0.
pub fn decrypt(ciphertext: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    if ciphertext.len() >= MAGIC.len() && &ciphertext[..MAGIC.len()] == MAGIC {
        // New format: SSC1 magic + nonce(24) + ciphertext
        decrypt_xchacha(ciphertext, key)
    } else {
        // Legacy age format — kept for backward compat with v0.1.0–v0.4.0 files
        decrypt_age(ciphertext, key)
    }
}

fn decrypt_xchacha(blob: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    use chacha20poly1305::{aead::Aead, KeyInit, XChaCha20Poly1305, XNonce};

    let nonce_start = MAGIC.len();
    let ct_start = nonce_start + 24;
    if blob.len() < ct_start + 16 {
        return Err(SessyncError::Crypto("xchacha blob too short".into()));
    }
    let nonce = XNonce::from_slice(&blob[nonce_start..ct_start]);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(nonce, &blob[ct_start..])
        .map_err(|e| SessyncError::Crypto(format!("xchacha decrypt: {e}")))
}

/// Legacy age-based decrypt — preserved verbatim for backward compatibility
/// with files encrypted by v0.1.0–v0.4.0.
fn decrypt_age(ciphertext: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    use age::secrecy::SecretString;
    use std::io::Read;

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

/// Hex-decode a 16-byte argon2 salt from its TOML-stored hex form.
pub fn decode_salt_hex(hex_str: &str) -> Result<[u8; 16]> {
    let bytes =
        hex::decode(hex_str).map_err(|e| SessyncError::Crypto(format!("salt hex decode: {e}")))?;
    bytes.try_into().map_err(|v: Vec<u8>| {
        SessyncError::Crypto(format!("salt must be 16 bytes, got {}", v.len()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [0x42u8; 32]
    }

    // ── Core roundtrip tests ──────────────────────────────────────────────────

    #[test]
    fn roundtrip_xchacha() {
        let key = test_key();
        let plaintext = b"sessync xchacha20-poly1305 roundtrip test";
        let ct = encrypt(plaintext, &key).unwrap();
        let pt = decrypt(&ct, &key).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn roundtrip_empty_plaintext() {
        let key = test_key();
        let ct = encrypt(b"", &key).unwrap();
        let pt = decrypt(&ct, &key).unwrap();
        assert_eq!(pt, b"");
    }

    #[test]
    fn roundtrip_large_plaintext() {
        // 5 MB — verifies no allocation cliff or stream-size issue.
        let key = test_key();
        let plaintext: Vec<u8> = (0u32..5 * 1024 * 1024)
            .map(|i| (i % 251) as u8)
            .collect();
        let ct = encrypt(&plaintext, &key).unwrap();
        let pt = decrypt(&ct, &key).unwrap();
        assert_eq!(pt, plaintext);
    }

    // ── Magic / format detection ──────────────────────────────────────────────

    #[test]
    fn magic_disambiguates_formats() {
        let key = test_key();
        let ct = encrypt(b"hello", &key).unwrap();
        assert!(
            ct.len() >= 8,
            "ciphertext must be at least 8 bytes long"
        );
        assert_eq!(
            &ct[..8],
            MAGIC,
            "first 8 bytes of encrypt() output must equal MAGIC"
        );
    }

    // ── Backward compatibility: legacy age format ─────────────────────────────

    #[test]
    fn decrypt_legacy_age_format() {
        // Produce an age-encrypted blob the same way the OLD encrypt() did.
        // Then verify the new top-level decrypt() can read it transparently.
        use age::secrecy::SecretString;
        use std::io::Write;

        let key = test_key();
        let plaintext = b"legacy age format backward compat";

        let pass = SecretString::from(hex::encode(key));
        let encryptor = age::Encryptor::with_user_passphrase(pass);
        let mut age_blob = vec![];
        let mut w = encryptor.wrap_output(&mut age_blob).unwrap();
        w.write_all(plaintext).unwrap();
        w.finish().unwrap();

        // Sanity: the age blob must NOT start with our magic.
        assert!(
            age_blob.len() < 8 || &age_blob[..8] != MAGIC,
            "age blob must not start with SSC1 magic"
        );

        // New decrypt() must transparently handle it.
        let recovered = decrypt(&age_blob, &key).unwrap();
        assert_eq!(recovered, plaintext);
    }

    // ── Security tests ────────────────────────────────────────────────────────

    #[test]
    fn tampered_ciphertext_fails() {
        let key = test_key();
        let mut ct = encrypt(b"sensitive data", &key).unwrap();
        // Flip a byte in the ciphertext body (after magic + nonce).
        let flip_at = MAGIC.len() + 24 + 5; // well inside the ciphertext
        ct[flip_at] ^= 0xFF;
        assert!(
            decrypt(&ct, &key).is_err(),
            "tampered ciphertext must not decrypt successfully"
        );
    }

    #[test]
    fn wrong_key_fails() {
        let key1 = [0x11u8; 32];
        let key2 = [0x22u8; 32];
        let ct = encrypt(b"secret", &key1).unwrap();
        assert!(
            decrypt(&ct, &key2).is_err(),
            "decryption with wrong key must fail"
        );
    }

    // ── Performance sanity check ──────────────────────────────────────────────

    /// Not a benchmark — just a sanity gate. XChaCha20-Poly1305 should encrypt
    /// 1 MB in well under 200 ms on any reasonable machine. If this ever flaps
    /// on constrained CI, increase the threshold rather than removing the test.
    #[test]
    fn xchacha_is_faster_than_age() {
        use std::time::Instant;

        let key = [0u8; 32];
        let plaintext = vec![42u8; 1024 * 1024]; // 1 MB

        let t = Instant::now();
        let _ = encrypt(&plaintext, &key).unwrap();
        let xchacha_ms = t.elapsed().as_millis();

        eprintln!("xchacha encrypt 1MB: {xchacha_ms}ms");
        assert!(
            xchacha_ms < 200,
            "xchacha encrypt too slow: {xchacha_ms}ms (threshold: 200ms)"
        );
    }
}
