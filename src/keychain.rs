use crate::error::{Result, SessyncError};
use keyring::Entry;

const SERVICE: &str = "sessync";
const ACCOUNT: &str = "passphrase";

/// Store the user's passphrase in the macOS Keychain.
pub fn store_passphrase(passphrase: &str) -> Result<()> {
    Entry::new(SERVICE, ACCOUNT)
        .map_err(|e| SessyncError::Keychain(format!("entry: {e}")))?
        .set_password(passphrase)
        .map_err(|e| SessyncError::Keychain(format!("set: {e}")))
}

/// Retrieve the passphrase. Returns Err if not set or Keychain locked.
pub fn load_passphrase() -> Result<String> {
    Entry::new(SERVICE, ACCOUNT)
        .map_err(|e| SessyncError::Keychain(format!("entry: {e}")))?
        .get_password()
        .map_err(|e| SessyncError::Keychain(format!("get: {e}")))
}

/// Delete the passphrase entry. Used in tests / `init` re-runs.
pub fn delete_passphrase() -> Result<()> {
    let entry = Entry::new(SERVICE, ACCOUNT)
        .map_err(|e| SessyncError::Keychain(format!("entry: {e}")))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(SessyncError::Keychain(format!("delete: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IGNORED by default — touches the real Keychain. Run manually with:
    /// `cargo test keychain::tests::roundtrip -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn roundtrip() {
        let _ = delete_passphrase();
        store_passphrase("test-passphrase-do-not-use").unwrap();
        let got = load_passphrase().unwrap();
        assert_eq!(got, "test-passphrase-do-not-use");
        delete_passphrase().unwrap();
    }
}
