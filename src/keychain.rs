use crate::error::{Result, SessyncError};
use keyring::Entry;

const SERVICE: &str = "sessync";
const ACCOUNT: &str = "passphrase";

fn entry(service: &str, account: &str) -> Result<Entry> {
    Entry::new(service, account).map_err(|e| {
        SessyncError::Keychain(format!("open keychain entry {service}/{account}: {e}"))
    })
}

/// Store the user's passphrase in the macOS Keychain.
///
/// Overwrites any existing entry. Caller is responsible for confirming intent —
/// silently overwriting orphans every previously-encrypted remote session.
pub fn store_passphrase(passphrase: &str) -> Result<()> {
    entry(SERVICE, ACCOUNT)?
        .set_password(passphrase)
        .map_err(|e| SessyncError::Keychain(format!("store passphrase in keychain: {e}")))
}

/// Retrieve the passphrase.
///
/// Returns `Err` if the entry is not set, the Keychain is locked, or any other
/// platform error occurred. Use `passphrase_is_set` to test for presence without
/// triggering an auth prompt on locked keychains.
pub fn load_passphrase() -> Result<String> {
    entry(SERVICE, ACCOUNT)?
        .get_password()
        .map_err(|e| SessyncError::Keychain(format!("load passphrase from keychain: {e}")))
}

/// Returns true if a passphrase entry exists.
///
/// Distinguishes "no entry" (false) from "platform error / locked" (Err).
/// Used by `sessync status` to report configuration state.
pub fn passphrase_is_set() -> Result<bool> {
    match entry(SERVICE, ACCOUNT)?.get_password() {
        Ok(_) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(SessyncError::Keychain(format!(
            "probe passphrase in keychain: {e}"
        ))),
    }
}

/// Delete the passphrase entry. Idempotent — `NoEntry` is treated as success.
///
/// Available for tests and a future interactive `sessync reset` command.
pub fn delete_passphrase() -> Result<()> {
    match entry(SERVICE, ACCOUNT)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(SessyncError::Keychain(format!(
            "delete passphrase from keychain: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SERVICE: &str = "sessync-test-do-not-use";
    const TEST_ACCOUNT: &str = "passphrase";

    /// IGNORED by default — touches the real Keychain (under a test-only service
    /// name to avoid clobbering a real `sessync/passphrase` entry).
    /// Run manually with:
    /// `cargo test --lib keychain::tests::roundtrip -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn roundtrip() {
        let e = entry(TEST_SERVICE, TEST_ACCOUNT).unwrap();
        let _ = e.delete_credential();

        let e = entry(TEST_SERVICE, TEST_ACCOUNT).unwrap();
        e.set_password("test-passphrase-do-not-use").unwrap();

        let e = entry(TEST_SERVICE, TEST_ACCOUNT).unwrap();
        assert_eq!(e.get_password().unwrap(), "test-passphrase-do-not-use");

        let e = entry(TEST_SERVICE, TEST_ACCOUNT).unwrap();
        e.delete_credential().unwrap();
    }
}
