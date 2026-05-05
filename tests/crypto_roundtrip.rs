use sessync::crypto;

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
