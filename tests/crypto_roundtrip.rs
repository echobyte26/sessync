use sessync::crypto;

// ── KDF tests (unchanged from v0.4.0) ────────────────────────────────────────

#[test]
fn kdf_is_deterministic_for_same_passphrase_and_salt() {
    let salt = [0u8; 16];
    let key1 = crypto::derive_key("hunter2", &salt).unwrap();
    let key2 = crypto::derive_key("hunter2", &salt).unwrap();
    assert_eq!(key1, key2);
    assert_eq!(key1.len(), 32);
}

#[test]
fn kdf_differs_for_different_passphrases() {
    let salt = [0u8; 16];
    let k1 = crypto::derive_key("hunter2", &salt).unwrap();
    let k2 = crypto::derive_key("hunter3", &salt).unwrap();
    assert_ne!(k1, k2);
}

#[test]
fn kdf_differs_for_different_salts() {
    let k1 = crypto::derive_key("hunter2", &[0u8; 16]).unwrap();
    let k2 = crypto::derive_key("hunter2", &[1u8; 16]).unwrap();
    assert_ne!(k1, k2);
}

// ── Encrypt/decrypt roundtrip (new SSC1 format) ───────────────────────────────

#[test]
fn encrypt_then_decrypt_recovers_plaintext() {
    let plaintext = b"hello sessync, this is a session jsonl line";
    let key = [42u8; 32];
    let ciphertext = crypto::encrypt(plaintext, &key).unwrap();
    assert_ne!(&ciphertext[..], plaintext);
    let decrypted = crypto::decrypt(&ciphertext, &key).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn decrypt_with_wrong_key_fails() {
    let plaintext = b"top secret";
    let key1 = [1u8; 32];
    let key2 = [2u8; 32];
    let ct = crypto::encrypt(plaintext, &key1).unwrap();
    assert!(crypto::decrypt(&ct, &key2).is_err());
}

// ── New v0.5.0 tests — SSC1 format properties ────────────────────────────────

/// encrypt() output must begin with the 8-byte SSC1 magic so the dispatcher
/// in decrypt() can reliably select the new code path.
#[test]
fn encrypt_output_starts_with_ssc1_magic() {
    let key = [0u8; 32];
    let ct = crypto::encrypt(b"test payload", &key).unwrap();
    assert!(ct.len() >= 8, "ciphertext too short to contain magic");
    assert_eq!(&ct[..8], b"SSC1\0\0\0\0", "first 8 bytes must be SSC1 magic");
}

/// Empty plaintext must survive the roundtrip (edge case: 0-byte message).
#[test]
fn roundtrip_empty_plaintext() {
    let key = [0xABu8; 32];
    let ct = crypto::encrypt(b"", &key).unwrap();
    let pt = crypto::decrypt(&ct, &key).unwrap();
    assert_eq!(pt, b"");
}

/// 5 MB plaintext — ensures there is no streaming/allocation cliff in the new path.
#[test]
fn roundtrip_large_plaintext() {
    let key = [0xCDu8; 32];
    let plaintext: Vec<u8> = (0u32..5 * 1024 * 1024)
        .map(|i| (i % 251) as u8)
        .collect();
    let ct = crypto::encrypt(&plaintext, &key).unwrap();
    let pt = crypto::decrypt(&ct, &key).unwrap();
    assert_eq!(pt, plaintext);
}

/// Flip one byte in the ciphertext body — authentication must fail.
#[test]
fn tampered_ciphertext_fails() {
    let key = [0x55u8; 32];
    let mut ct = crypto::encrypt(b"tamper me", &key).unwrap();
    // Byte 33 is inside the ciphertext body (after 8-byte magic + 24-byte nonce).
    ct[33] ^= 0xFF;
    assert!(
        crypto::decrypt(&ct, &key).is_err(),
        "tampered ciphertext must not decrypt"
    );
}

/// Wrong key must produce a decryption error (not a panic or silent mismatch).
#[test]
fn wrong_key_fails() {
    let key1 = [0x11u8; 32];
    let key2 = [0x22u8; 32];
    let ct = crypto::encrypt(b"secret data", &key1).unwrap();
    assert!(
        crypto::decrypt(&ct, &key2).is_err(),
        "wrong key must not decrypt successfully"
    );
}

// ── Backward compatibility: v0.1.0–v0.4.0 age format ────────────────────────

/// Construct an age-encrypted blob using the exact same key-encoding that
/// v0.4.0's `encrypt()` used (hex(key) → age passphrase), then verify that
/// the v0.5.0 `decrypt()` can transparently fall through to the legacy path.
///
/// This exercises the real backward-compat constraint: any file that was on
/// disk or in OSS before upgrading to v0.5.0 must still decrypt correctly.
#[test]
fn decrypt_legacy_age_format() {
    use age::secrecy::SecretString;
    use std::io::Write;

    let key = [0x42u8; 32];
    let plaintext = b"legacy v0.4.0 age-encrypted data";

    // Reproduce the old encrypt() exactly.
    let pass = SecretString::from(hex::encode(key));
    let encryptor = age::Encryptor::with_user_passphrase(pass);
    let mut age_blob: Vec<u8> = vec![];
    let mut w = encryptor.wrap_output(&mut age_blob).unwrap();
    w.write_all(plaintext).unwrap();
    w.finish().unwrap();

    // The age blob must NOT start with SSC1 magic (detector precondition).
    assert!(
        age_blob.len() < 8 || &age_blob[..8] != b"SSC1\0\0\0\0",
        "age blob must not start with SSC1 magic"
    );

    // v0.5.0 decrypt() must handle it transparently.
    let recovered = crypto::decrypt(&age_blob, &key)
        .expect("decrypt() must handle legacy age format for backward compat");
    assert_eq!(recovered, plaintext);
}
