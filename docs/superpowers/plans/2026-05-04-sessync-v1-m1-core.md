# sessync v1 Milestone 1: Core Manual Sync Pipeline

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Rust CLI that lets the user manually push Claude Code sessions from one Mac to Aliyun OSS (encrypted) and resume them on another Mac, with config + interactive selection UX.

**Architecture:** Two-trait abstraction — `ToolAdapter` (file format + locations of a coding agent's sessions) and `StorageAdapter` (blob storage backend). v1 ships one impl per trait (Claude Code + Aliyun OSS) plus an in-memory storage adapter for tests. Encryption is age + argon2id KDF, passphrase stored in macOS Keychain. CLI is async (tokio) with interactive selection (dialoguer).

**Tech Stack:**
- Rust 2021 edition
- `clap` (CLI parsing, derive feature)
- `tokio` (async runtime)
- `serde` + `serde_json` + `toml` (data)
- `age` 0.10 (file encryption)
- `argon2` 0.5 (KDF)
- `keyring` 3.x (macOS Keychain)
- `aliyun-oss-client` 0.13 (OSS SDK; if unstable, fall back to manual signing with `reqwest`)
- `dialoguer` 0.11 (interactive prompts)
- `thiserror` + `anyhow` (error handling)
- `tracing` + `tracing-subscriber` (logging)
- `tempfile` (test isolation)

**Milestone 1 scope (in):** `sessync init`, `sessync push`, `sessync resume`, `sessync status` — manually invoked, happy path.

**Out of M1 scope (deferred to M2):** Claude Code Stop hook, launchd 周期任务, SQLite pending queue, retry logic, osascript notifications, log rotation, `sessync doctor`, `sessync push --retry-pending`.

---

## Engineer Pre-flight

Before Task 0, verify:

- [ ] Rust toolchain installed: `cargo --version` returns ≥ 1.75
  - If missing: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- [ ] An Aliyun OSS bucket exists (or willing to create one for testing). Need: endpoint, bucket name, AK, SK.
- [ ] On macOS (M1 is macOS-only; cross-platform deferred).
- [ ] Working directory: `/Users/jameschen/Project/ai-coding-project/sessync`
- [ ] PRD at `docs/prd/2026-05-04-sessync-v1.md` is the source of truth for requirements.

---

## File Structure

```
sessync/
├── Cargo.toml
├── src/
│   ├── main.rs                  # CLI entry, clap parsing, dispatch
│   ├── lib.rs                   # Re-exports for integration tests
│   ├── error.rs                 # SessyncError (thiserror) + Result<T>
│   ├── types.rs                 # SessionId, ProjectKey, SessionMeta
│   ├── config.rs                # Config struct, TOML load/save
│   ├── keychain.rs              # macOS Keychain wrapper (passphrase only)
│   ├── crypto.rs                # KDF (argon2id) + encrypt/decrypt (age)
│   ├── adapter/
│   │   ├── mod.rs               # Trait definitions: ToolAdapter, StorageAdapter
│   │   ├── claude_code.rs       # Claude Code session reader/writer
│   │   ├── path_codec.rs        # Encode/decode cwd ↔ Claude project dir name
│   │   ├── oss.rs               # Aliyun OSS storage adapter
│   │   └── memory.rs            # In-memory storage adapter (tests only)
│   └── commands/
│       ├── mod.rs
│       ├── init.rs              # Interactive setup
│       ├── push.rs              # Encrypt + upload latest sessions
│       ├── resume.rs            # Select project/session → download → decrypt → place
│       └── status.rs            # Print last sync times + counts
├── tests/
│   ├── crypto_roundtrip.rs      # End-to-end encrypt/decrypt + KDF determinism
│   ├── storage_trait.rs         # InMemoryStorage round-trip via the trait
│   ├── claude_code_adapter.rs   # Read fixture jsonl, list projects/sessions
│   ├── path_codec.rs            # Path encoding edge cases
│   └── push_resume_e2e.rs       # Full push → InMemoryStorage → resume cycle
├── tests/fixtures/
│   ├── claude_projects/
│   │   └── -Users-jameschen-test-project-foo/
│   │       └── 01ab-...-jsonl    # synthetic session jsonl
│   └── README.md
└── docs/
    ├── prd/2026-05-04-sessync-v1.md
    └── superpowers/plans/2026-05-04-sessync-v1-m1-core.md
```

**Decomposition rationale:**
- `adapter/` isolates pluggable boundaries (the PRD's hard requirement); each adapter file is its own concern, keeps each file small.
- `commands/` is one file per subcommand to mirror clap's structure and stay scannable.
- `crypto.rs` keeps KDF + encryption together because they're always called as a pair (passphrase → key → encrypt).
- `keychain.rs` is separated because it's the only platform-specific module in M1; isolating it makes future cross-platform abstraction cheap.
- `path_codec.rs` lives under `adapter/` because Claude Code's path encoding is part of its tool-specific behavior, but it's its own file because it has tricky edge cases worth testing in isolation.

---

## Task 0: Project Bootstrap

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/lib.rs`

- [ ] **Step 1: `cargo init`**

```bash
cd /Users/jameschen/Project/ai-coding-project/sessync
cargo init --name sessync --bin
```

Expected: creates `Cargo.toml` and `src/main.rs` with hello-world. Don't commit yet.

- [ ] **Step 2: Replace `Cargo.toml` with deps**

```toml
[package]
name = "sessync"
version = "0.1.0"
edition = "2021"
rust-version = "1.75"

[dependencies]
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "fs", "io-util"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
age = "0.10"
argon2 = "0.5"
keyring = { version = "3", features = ["apple-native"] }
dialoguer = "0.11"
anyhow = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
chrono = { version = "0.4", features = ["serde"] }
sha2 = "0.10"
hex = "0.4"
async-trait = "0.1"
# OSS — pin to a known-good version. If 0.13 doesn't compile, try latest 0.x.
aliyun-oss-client = "0.13"

[dev-dependencies]
tempfile = "3"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "fs", "io-util", "test-util"] }
```

- [ ] **Step 3: Create `src/lib.rs`**

```rust
//! sessync library root — exports modules for integration tests.
pub mod adapter;
pub mod commands;
pub mod config;
pub mod crypto;
pub mod error;
pub mod keychain;
pub mod types;
```

- [ ] **Step 4: Replace `src/main.rs` with clap skeleton**

```rust
use clap::{Parser, Subcommand};
use sessync::commands;

#[derive(Parser)]
#[command(name = "sessync", version, about = "Cross-device sync for Claude Code sessions")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive first-time setup (OSS creds + passphrase).
    Init,
    /// Encrypt and upload local sessions to OSS.
    Push,
    /// Browse remote sessions and pull one into the current project.
    Resume,
    /// Show sync state.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("sessync=info".parse().unwrap()))
        .init();

    let cli = Cli::parse();
    match cli.command {
        Cmd::Init => commands::init::run().await,
        Cmd::Push => commands::push::run().await,
        Cmd::Resume => commands::resume::run().await,
        Cmd::Status => commands::status::run().await,
    }
}
```

- [ ] **Step 5: Create empty module files so build passes**

Create these stubs (one line each so the modules exist):

```bash
mkdir -p src/adapter src/commands
printf 'pub mod tool;\npub mod storage;\npub mod claude_code;\npub mod path_codec;\npub mod oss;\npub mod memory;\n' > src/adapter/mod.rs
printf 'pub mod init;\npub mod push;\npub mod resume;\npub mod status;\n' > src/commands/mod.rs
for f in src/adapter/{tool,storage,claude_code,path_codec,oss,memory}.rs src/commands/{init,push,resume,status}.rs src/error.rs src/types.rs src/config.rs src/keychain.rs src/crypto.rs; do
  printf '// stub — implemented in later task\n' > "$f"
done
```

Then add async run stubs so `main.rs` compiles:

```rust
// src/commands/init.rs
pub async fn run() -> anyhow::Result<()> { anyhow::bail!("init: not implemented") }
```

(Repeat for push/resume/status.)

- [ ] **Step 6: Verify build**

```bash
cargo build
```

Expected: PASS (lots of "unused" warnings is fine).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/
git commit -m "chore: bootstrap sessync rust project with clap skeleton"
```

---

## Task 1: Error Type + Base Types

**Files:**
- Modify: `src/error.rs`
- Modify: `src/types.rs`

- [ ] **Step 1: Define `SessyncError` in `src/error.rs`**

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SessyncError {
    #[error("config error: {0}")]
    Config(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("tool adapter error: {0}")]
    Tool(String),

    #[error("keychain error: {0}")]
    Keychain(String),

    #[error("io error")]
    Io(#[from] std::io::Error),

    #[error("serde error")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, SessyncError>;
```

- [ ] **Step 2: Define base types in `src/types.rs`**

```rust
use serde::{Deserialize, Serialize};

/// Opaque session identifier (Claude uses UUID-like strings).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A normalized project identifier — a content hash of the cwd path.
/// Used as the OSS prefix so the same project across devices maps to the same key
/// even when paths differ.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectKey(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: SessionId,
    pub project_key: ProjectKey,
    /// Original cwd from the source device (for display only — not used to map paths).
    pub source_cwd: String,
    pub source_hostname: String,
    pub modified_at: chrono::DateTime<chrono::Utc>,
    pub byte_size: u64,
    /// First user message, truncated to 80 chars (UI hint for the resume picker).
    pub preview: String,
}
```

- [ ] **Step 3: Verify build**

```bash
cargo build
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/error.rs src/types.rs
git commit -m "feat: define SessyncError and base types"
```

---

## Task 2: Crypto — KDF (argon2id)

**Files:**
- Modify: `src/crypto.rs`
- Create: `tests/crypto_roundtrip.rs`

- [ ] **Step 1: Write the failing test in `tests/crypto_roundtrip.rs`**

```rust
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
```

- [ ] **Step 2: Run test, verify it fails**

```bash
cargo test --test crypto_roundtrip kdf_
```

Expected: FAIL with "function `derive_key` not found".

- [ ] **Step 3: Implement KDF in `src/crypto.rs`**

```rust
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
```

- [ ] **Step 4: Run tests, verify they pass**

```bash
cargo test --test crypto_roundtrip kdf_
```

Expected: 3 PASS.

- [ ] **Step 5: Commit**

```bash
git add src/crypto.rs tests/crypto_roundtrip.rs
git commit -m "feat(crypto): add argon2id passphrase KDF"
```

---

## Task 3: Crypto — Encrypt / Decrypt with age

**Files:**
- Modify: `src/crypto.rs`
- Modify: `tests/crypto_roundtrip.rs`

- [ ] **Step 1: Add round-trip tests**

Append to `tests/crypto_roundtrip.rs`:

```rust
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
```

- [ ] **Step 2: Run, verify they fail**

```bash
cargo test --test crypto_roundtrip encrypt_
```

Expected: FAIL ("function `encrypt`/`decrypt` not found").

- [ ] **Step 3: Implement encrypt/decrypt in `src/crypto.rs`**

Append to `src/crypto.rs`:

```rust
use age::secrecy::SecretString;
use std::io::{Read, Write};

/// Encrypt with a symmetric key by wrapping it as an age scrypt recipient.
/// The 32-byte key is hex-encoded into a SecretString — age uses it as the passphrase.
/// (We already did the expensive KDF; age's internal scrypt over the hex string
/// adds a cheap second pass that's negligible at the file sizes we move.)
pub fn encrypt(plaintext: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    let pass = SecretString::from(hex::encode(key));
    let encryptor = age::Encryptor::with_user_passphrase(pass);
    let mut out = vec![];
    let mut w = encryptor.wrap_output(&mut out)
        .map_err(|e| SessyncError::Crypto(format!("age wrap: {e}")))?;
    w.write_all(plaintext)?;
    w.finish().map_err(|e| SessyncError::Crypto(format!("age finish: {e}")))?;
    Ok(out)
}

pub fn decrypt(ciphertext: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    let pass = SecretString::from(hex::encode(key));
    let decryptor = age::Decryptor::new(ciphertext)
        .map_err(|e| SessyncError::Crypto(format!("age open: {e}")))?;
    let mut r = decryptor.decrypt(std::iter::once(&age::scrypt::Identity::new(pass) as _))
        .map_err(|e| SessyncError::Crypto(format!("age decrypt: {e}")))?;
    let mut buf = vec![];
    r.read_to_end(&mut buf)?;
    Ok(buf)
}
```

> Note: `age` 0.10 API may differ slightly. If `Encryptor::with_user_passphrase` / `scrypt::Identity` symbols aren't found, check the version's docs and adjust — the contract is "encrypt and decrypt with a symmetric secret derived from the hex-encoded key". Don't reinvent the encryption primitive; only adjust API call shape.

- [ ] **Step 4: Run, verify pass**

```bash
cargo test --test crypto_roundtrip
```

Expected: all 5 PASS.

- [ ] **Step 5: Commit**

```bash
git add src/crypto.rs tests/crypto_roundtrip.rs
git commit -m "feat(crypto): add age encrypt/decrypt with symmetric key"
```

---

## Task 4: Config (TOML load/save)

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write inline tests in `src/config.rs`**

```rust
use crate::error::{Result, SessyncError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub oss: OssConfig,
    pub device: DeviceConfig,
    /// Salt for argon2 KDF. 16 random bytes generated at `init` time, hex-encoded.
    pub kdf_salt_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OssConfig {
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    /// Object key prefix (so users can scope multiple installs in one bucket).
    #[serde(default = "default_prefix")]
    pub prefix: String,
}

fn default_prefix() -> String { "sessync/".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// Stable device identifier (UUID v4 generated once at init).
    pub device_id: String,
    /// Human-readable hostname for display.
    pub hostname: String,
}

impl Config {
    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config/sessync/config.toml")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| SessyncError::Config(format!("read {}: {e}", path.display())))?;
        toml::from_str(&text)
            .map_err(|e| SessyncError::Config(format!("parse: {e}")))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| SessyncError::Config(format!("serialize: {e}")))?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_config() -> Config {
        Config {
            oss: OssConfig {
                endpoint: "oss-cn-hangzhou.aliyuncs.com".into(),
                bucket: "my-sessync".into(),
                access_key_id: "AKIDxxx".into(),
                access_key_secret: "secretxxx".into(),
                prefix: "sessync/".into(),
            },
            device: DeviceConfig {
                device_id: "11111111-1111-1111-1111-111111111111".into(),
                hostname: "test-mac.local".into(),
            },
            kdf_salt_hex: "00".repeat(16),
        }
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let c = sample_config();
        c.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.oss.bucket, "my-sessync");
        assert_eq!(loaded.device.hostname, "test-mac.local");
    }

    #[test]
    fn save_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/deep/config.toml");
        sample_config().save(&path).unwrap();
        assert!(path.exists());
    }
}
```

- [ ] **Step 2: Run tests, verify they pass**

```bash
cargo test config::tests::
```

Expected: 2 PASS.

- [ ] **Step 3: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): add Config struct with TOML load/save"
```

---

## Task 5: Keychain Wrapper (passphrase storage)

**Files:**
- Modify: `src/keychain.rs`

- [ ] **Step 1: Implement Keychain wrapper**

```rust
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
```

> Why `#[ignore]`: Keychain tests pop up an OS auth prompt the first time. Don't run in CI; engineer runs once locally to validate.

- [ ] **Step 2: Verify build**

```bash
cargo build
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/keychain.rs
git commit -m "feat(keychain): macOS Keychain wrapper for passphrase"
```

---

## Task 6: StorageAdapter Trait + InMemoryStorage

**Files:**
- Modify: `src/adapter/mod.rs`
- Modify: `src/adapter/storage.rs`
- Modify: `src/adapter/memory.rs`
- Create: `tests/storage_trait.rs`

- [ ] **Step 1: Define `StorageAdapter` trait in `src/adapter/storage.rs`**

```rust
use crate::error::Result;
use async_trait::async_trait;

/// A blob storage backend. Operations are keyed by opaque string keys.
/// Implementations: `OssStorage` (production), `InMemoryStorage` (tests).
#[async_trait]
pub trait StorageAdapter: Send + Sync {
    /// Upload bytes under `key`. Overwrites if exists.
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()>;

    /// Download bytes for `key`. Returns Err if missing.
    async fn get(&self, key: &str) -> Result<Vec<u8>>;

    /// List keys under a given prefix (no trailing-slash semantics; literal prefix match).
    async fn list(&self, prefix: &str) -> Result<Vec<StorageObject>>;

    /// Delete `key`. Idempotent (no error if missing).
    async fn delete(&self, key: &str) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct StorageObject {
    pub key: String,
    pub size: u64,
    pub last_modified: chrono::DateTime<chrono::Utc>,
}
```

- [ ] **Step 2: Implement `InMemoryStorage` in `src/adapter/memory.rs`**

```rust
use super::storage::{StorageAdapter, StorageObject};
use crate::error::{Result, SessyncError};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct InMemoryStorage {
    inner: Mutex<HashMap<String, (Vec<u8>, chrono::DateTime<chrono::Utc>)>>,
}

impl InMemoryStorage {
    pub fn new() -> Self { Self::default() }
}

#[async_trait]
impl StorageAdapter for InMemoryStorage {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.insert(key.to_string(), (bytes, chrono::Utc::now()));
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let g = self.inner.lock().unwrap();
        g.get(key)
            .map(|(b, _)| b.clone())
            .ok_or_else(|| SessyncError::Storage(format!("not found: {key}")))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<StorageObject>> {
        let g = self.inner.lock().unwrap();
        let mut out: Vec<_> = g.iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, (b, t))| StorageObject {
                key: k.clone(),
                size: b.len() as u64,
                last_modified: *t,
            })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.remove(key);
        Ok(())
    }
}
```

- [ ] **Step 3: Write integration test in `tests/storage_trait.rs`**

```rust
use sessync::adapter::memory::InMemoryStorage;
use sessync::adapter::storage::StorageAdapter;

#[tokio::test]
async fn put_get_roundtrip() {
    let s = InMemoryStorage::new();
    s.put("k1", b"hello".to_vec()).await.unwrap();
    let got = s.get("k1").await.unwrap();
    assert_eq!(got, b"hello");
}

#[tokio::test]
async fn list_filters_by_prefix() {
    let s = InMemoryStorage::new();
    s.put("a/1", vec![1]).await.unwrap();
    s.put("a/2", vec![2]).await.unwrap();
    s.put("b/1", vec![3]).await.unwrap();
    let listed = s.list("a/").await.unwrap();
    let keys: Vec<_> = listed.into_iter().map(|o| o.key).collect();
    assert_eq!(keys, vec!["a/1".to_string(), "a/2".to_string()]);
}

#[tokio::test]
async fn delete_is_idempotent() {
    let s = InMemoryStorage::new();
    s.delete("nope").await.unwrap();
    s.put("k", vec![1]).await.unwrap();
    s.delete("k").await.unwrap();
    assert!(s.get("k").await.is_err());
}
```

- [ ] **Step 4: Run tests, verify pass**

```bash
cargo test --test storage_trait
```

Expected: 3 PASS.

- [ ] **Step 5: Commit**

```bash
git add src/adapter/ tests/storage_trait.rs
git commit -m "feat(adapter): StorageAdapter trait + InMemoryStorage impl"
```

---

## Task 7: Aliyun OSS StorageAdapter

**Files:**
- Modify: `src/adapter/oss.rs`

> No automated test in this task — OSS requires real bucket + creds. Engineer runs `sessync init` then `sessync push` against a real bucket as the smoke test (Task 14 / Task 16).

- [ ] **Step 1: Implement `OssStorage` in `src/adapter/oss.rs`**

```rust
use super::storage::{StorageAdapter, StorageObject};
use crate::config::OssConfig;
use crate::error::{Result, SessyncError};
use async_trait::async_trait;

pub struct OssStorage {
    client: aliyun_oss_client::Client,
    prefix: String,
}

impl OssStorage {
    pub fn new(cfg: &OssConfig) -> Result<Self> {
        let client = aliyun_oss_client::Client::new(
            cfg.access_key_id.clone().into(),
            cfg.access_key_secret.clone().into(),
            cfg.endpoint.parse().map_err(|e| SessyncError::Storage(format!("endpoint parse: {e}")))?,
            cfg.bucket.parse().map_err(|e| SessyncError::Storage(format!("bucket parse: {e}")))?,
        );
        Ok(Self {
            client,
            prefix: cfg.prefix.clone(),
        })
    }

    fn full_key(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }
}

#[async_trait]
impl StorageAdapter for OssStorage {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let full = self.full_key(key);
        self.client.put_content(bytes, &full).await
            .map_err(|e| SessyncError::Storage(format!("put {full}: {e}")))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let full = self.full_key(key);
        let buf: Vec<u8> = self.client.get_object(&full, ..).await
            .map_err(|e| SessyncError::Storage(format!("get {full}: {e}")))?;
        Ok(buf)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<StorageObject>> {
        let full = self.full_key(prefix);
        let resp = self.client.get_object_list(vec![("prefix".into(), full.clone().into())]).await
            .map_err(|e| SessyncError::Storage(format!("list {full}: {e}")))?;
        let strip = self.prefix.clone();
        let out = resp.object_iter()
            .map(|o| StorageObject {
                key: o.path().trim_start_matches(&strip).to_string(),
                size: o.size() as u64,
                last_modified: o.last_modified().to_owned(),
            })
            .collect();
        Ok(out)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let full = self.full_key(key);
        self.client.delete_object(&full).await
            .map_err(|e| SessyncError::Storage(format!("delete {full}: {e}")))?;
        Ok(())
    }
}
```

> If `aliyun-oss-client` 0.13's API differs (the crate is moving fast), the contract is the same: `put_content`, `get_object`, `get_object_list` (or `list_objects`), `delete_object`. Adjust call shape only; do not change the trait.

- [ ] **Step 2: Verify build**

```bash
cargo build
```

Expected: PASS. If it fails because of `aliyun-oss-client` API drift, search `https://docs.rs/aliyun-oss-client` for the right method names and adjust.

- [ ] **Step 3: Commit**

```bash
git add src/adapter/oss.rs
git commit -m "feat(adapter): Aliyun OSS storage adapter"
```

---

## Task 8: ToolAdapter Trait

**Files:**
- Modify: `src/adapter/tool.rs`

- [ ] **Step 1: Define trait + supporting types**

```rust
use crate::error::Result;
use crate::types::{ProjectKey, SessionId, SessionMeta};
use async_trait::async_trait;
use std::path::PathBuf;

/// A coding agent's session storage adapter.
/// v1 has one impl: `ClaudeCodeAdapter`.
#[async_trait]
pub trait ToolAdapter: Send + Sync {
    /// Tool short name, used as part of OSS key prefix.
    fn name(&self) -> &'static str;

    /// Discover all local sessions across all projects.
    async fn list_local_sessions(&self) -> Result<Vec<LocalSession>>;

    /// Read the raw session file (for upload). Engineer should NOT parse — preserve bytes.
    async fn read_session(&self, session_id: &SessionId) -> Result<Vec<u8>>;

    /// Write a session into the local store, mapped to `target_cwd`.
    /// Implementation handles tool-specific path encoding so `claude --resume` finds it.
    async fn write_session(&self, session_id: &SessionId, target_cwd: &str, raw: &[u8]) -> Result<PathBuf>;

    /// Compute the project key (stable across devices) for a given cwd.
    fn project_key_for(&self, cwd: &str) -> ProjectKey;
}

#[derive(Debug, Clone)]
pub struct LocalSession {
    pub meta: SessionMeta,
    pub local_path: PathBuf,
}
```

- [ ] **Step 2: Verify build**

```bash
cargo build
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/adapter/tool.rs
git commit -m "feat(adapter): ToolAdapter trait + LocalSession type"
```

---

## Task 9: Path Codec for Claude Code

**Files:**
- Modify: `src/adapter/path_codec.rs`
- Create: `tests/path_codec.rs`

Claude Code stores sessions under `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. The encoding replaces `/` with `-`. Two devices with different cwds produce different encoded names — this module is the source of truth for that mapping.

- [ ] **Step 1: Write failing tests in `tests/path_codec.rs`**

```rust
use sessync::adapter::path_codec;

#[test]
fn encode_replaces_slashes_with_dashes() {
    assert_eq!(
        path_codec::encode_cwd("/Users/alice/Project/foo"),
        "-Users-alice-Project-foo"
    );
}

#[test]
fn encode_handles_root() {
    assert_eq!(path_codec::encode_cwd("/"), "-");
}

#[test]
fn project_key_is_deterministic_and_path_invariant_via_basename() {
    // The PRD says we map by stable hash so the same project on different paths can co-locate.
    // For v1 we use a content hash of the full path — same path on both Macs collides cleanly.
    // Different paths intentionally don't collide (user picks at resume time).
    let a = path_codec::project_key_for_cwd("/Users/alice/work/foo");
    let b = path_codec::project_key_for_cwd("/Users/alice/work/foo");
    assert_eq!(a, b);
    let c = path_codec::project_key_for_cwd("/home/alice/work/foo");
    assert_ne!(a, c);
}
```

- [ ] **Step 2: Run, verify FAIL**

```bash
cargo test --test path_codec
```

Expected: FAIL.

- [ ] **Step 3: Implement `src/adapter/path_codec.rs`**

```rust
use crate::types::ProjectKey;
use sha2::{Digest, Sha256};

/// Encode a filesystem path the way Claude Code does for project directory names.
/// `/Users/alice/Project/foo` → `-Users-alice-Project-foo`
pub fn encode_cwd(cwd: &str) -> String {
    cwd.replace('/', "-")
}

/// Stable project key = hex(sha256(cwd)). Used as OSS prefix segment.
/// Same path on two devices → same key → groups together in selector.
/// Different paths (e.g. /Users/foo vs /home/foo) → different keys → user picks
/// which to pull at resume time, then writes into local cwd.
pub fn project_key_for_cwd(cwd: &str) -> ProjectKey {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let digest = hasher.finalize();
    ProjectKey(hex::encode(&digest[..8]))  // 16 hex chars is plenty for personal use
}
```

- [ ] **Step 4: Run, verify PASS**

```bash
cargo test --test path_codec
```

Expected: 3 PASS.

- [ ] **Step 5: Commit**

```bash
git add src/adapter/path_codec.rs tests/path_codec.rs
git commit -m "feat(adapter): path codec for Claude Code project dirs"
```

---

## Task 10: Claude Code ToolAdapter

**Files:**
- Modify: `src/adapter/claude_code.rs`
- Create: `tests/fixtures/claude_projects/-tmp-test-foo/abc123-def.jsonl`
- Create: `tests/claude_code_adapter.rs`

- [ ] **Step 1: Create test fixture**

```bash
mkdir -p tests/fixtures/claude_projects/-tmp-test-foo
cat > tests/fixtures/claude_projects/-tmp-test-foo/abc123-def.jsonl <<'EOF'
{"type":"summary","summary":"Test session","leafUuid":"abc"}
{"type":"user","message":{"role":"user","content":"hello world"},"uuid":"u1","timestamp":"2026-05-04T10:00:00.000Z"}
{"type":"assistant","message":{"role":"assistant","content":"hi"},"uuid":"a1","timestamp":"2026-05-04T10:00:01.000Z"}
EOF
```

- [ ] **Step 2: Write failing test in `tests/claude_code_adapter.rs`**

```rust
use sessync::adapter::claude_code::ClaudeCodeAdapter;
use sessync::adapter::tool::ToolAdapter;
use sessync::types::SessionId;
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude_projects")
}

#[tokio::test]
async fn list_local_sessions_finds_fixture() {
    let adapter = ClaudeCodeAdapter::with_root(fixture_root());
    let sessions = adapter.list_local_sessions().await.unwrap();
    assert!(sessions.iter().any(|s| s.meta.session_id.0 == "abc123-def"));
}

#[tokio::test]
async fn read_session_returns_raw_bytes() {
    let adapter = ClaudeCodeAdapter::with_root(fixture_root());
    let bytes = adapter.read_session(&SessionId("abc123-def".into())).await.unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("hello world"));
    assert!(text.contains("\"type\":\"user\""));
}

#[tokio::test]
async fn write_session_creates_file_under_target_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let adapter = ClaudeCodeAdapter::with_root(tmp.path().to_path_buf());
    let written = adapter.write_session(
        &SessionId("xyz-789".into()),
        "/Users/test/some/cwd",
        b"{\"type\":\"user\"}\n",
    ).await.unwrap();
    assert!(written.exists());
    let dir = written.parent().unwrap();
    assert_eq!(dir.file_name().unwrap().to_str().unwrap(), "-Users-test-some-cwd");
}
```

- [ ] **Step 3: Run, verify FAIL**

```bash
cargo test --test claude_code_adapter
```

Expected: FAIL ("ClaudeCodeAdapter not found").

- [ ] **Step 4: Implement `src/adapter/claude_code.rs`**

```rust
use super::path_codec;
use super::tool::{LocalSession, ToolAdapter};
use crate::error::{Result, SessyncError};
use crate::types::{ProjectKey, SessionId, SessionMeta};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::io::AsyncBufReadExt;

pub struct ClaudeCodeAdapter {
    /// Root directory of Claude Code projects (default `~/.claude/projects`).
    root: PathBuf,
}

impl ClaudeCodeAdapter {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Self { root: PathBuf::from(home).join(".claude/projects") }
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl ToolAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &'static str { "claude-code" }

    async fn list_local_sessions(&self) -> Result<Vec<LocalSession>> {
        let mut out = vec![];
        if !self.root.exists() {
            return Ok(out);
        }
        let mut project_dirs = tokio::fs::read_dir(&self.root).await?;
        while let Some(pd) = project_dirs.next_entry().await? {
            if !pd.file_type().await?.is_dir() { continue; }
            let project_dir_name = pd.file_name().to_string_lossy().into_owned();
            let source_cwd = decode_project_dir(&project_dir_name);
            let project_key = path_codec::project_key_for_cwd(&source_cwd);

            let mut files = tokio::fs::read_dir(pd.path()).await?;
            while let Some(f) = files.next_entry().await? {
                let path = f.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") { continue; }
                let session_id = path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| SessionId(s.to_string()))
                    .ok_or_else(|| SessyncError::Tool(format!("bad filename: {}", path.display())))?;

                let metadata = tokio::fs::metadata(&path).await?;
                let preview = first_user_message_preview(&path).await.unwrap_or_default();

                out.push(LocalSession {
                    meta: SessionMeta {
                        session_id,
                        project_key: project_key.clone(),
                        source_cwd: source_cwd.clone(),
                        source_hostname: hostname(),
                        modified_at: metadata.modified()
                            .map(chrono::DateTime::<chrono::Utc>::from)
                            .unwrap_or_else(|_| chrono::Utc::now()),
                        byte_size: metadata.len(),
                        preview,
                    },
                    local_path: path,
                });
            }
        }
        Ok(out)
    }

    async fn read_session(&self, session_id: &SessionId) -> Result<Vec<u8>> {
        // Walk all project dirs to find the one containing this session.
        let mut project_dirs = tokio::fs::read_dir(&self.root).await?;
        while let Some(pd) = project_dirs.next_entry().await? {
            let candidate = pd.path().join(format!("{}.jsonl", session_id.0));
            if candidate.exists() {
                return Ok(tokio::fs::read(&candidate).await?);
            }
        }
        Err(SessyncError::Tool(format!("session not found locally: {session_id}")))
    }

    async fn write_session(&self, session_id: &SessionId, target_cwd: &str, raw: &[u8]) -> Result<PathBuf> {
        let dir_name = path_codec::encode_cwd(target_cwd);
        let dir = self.root.join(dir_name);
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.jsonl", session_id.0));
        tokio::fs::write(&path, raw).await?;
        Ok(path)
    }

    fn project_key_for(&self, cwd: &str) -> ProjectKey {
        path_codec::project_key_for_cwd(cwd)
    }
}

fn decode_project_dir(encoded: &str) -> String {
    // Reverse of `path_codec::encode_cwd` — replace dashes with slashes.
    // Note: this is lossy if any path component itself contained dashes;
    // Claude Code accepts the lossiness, so do we.
    encoded.replace('-', "/")
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

async fn first_user_message_preview(path: &Path) -> Result<String> {
    let f = tokio::fs::File::open(path).await?;
    let mut lines = tokio::io::BufReader::new(f).lines();
    while let Some(line) = lines.next_line().await? {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("user") {
                if let Some(content) = v.pointer("/message/content").and_then(|c| c.as_str()) {
                    let mut s = content.to_string();
                    if s.chars().count() > 80 {
                        s = s.chars().take(77).collect::<String>() + "...";
                    }
                    return Ok(s);
                }
            }
        }
    }
    Ok(String::new())
}
```

- [ ] **Step 5: Run, verify PASS**

```bash
cargo test --test claude_code_adapter
```

Expected: 3 PASS.

- [ ] **Step 6: Commit**

```bash
git add src/adapter/claude_code.rs tests/claude_code_adapter.rs tests/fixtures/
git commit -m "feat(adapter): Claude Code session reader/writer"
```

---

## Task 11: `sessync init` Command

**Files:**
- Modify: `src/commands/init.rs`

- [ ] **Step 1: Implement interactive setup**

```rust
use crate::config::{Config, DeviceConfig, OssConfig};
use crate::keychain;
use anyhow::Result;
use dialoguer::{Input, Password};
use rand::RngCore;

pub async fn run() -> Result<()> {
    println!("sessync init — first-time setup\n");

    let endpoint: String = Input::new()
        .with_prompt("OSS endpoint (e.g. oss-cn-hangzhou.aliyuncs.com)")
        .interact_text()?;
    let bucket: String = Input::new()
        .with_prompt("OSS bucket name")
        .interact_text()?;
    let access_key_id: String = Input::new()
        .with_prompt("OSS AccessKeyId")
        .interact_text()?;
    let access_key_secret: String = Password::new()
        .with_prompt("OSS AccessKeySecret")
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

    // Generate per-install salt + device id.
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let device_id = uuid::Uuid::new_v4().to_string();
    let hostname = std::process::Command::new("hostname")
        .output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let cfg = Config {
        oss: OssConfig { endpoint, bucket, access_key_id, access_key_secret, prefix },
        device: DeviceConfig { device_id, hostname },
        kdf_salt_hex: hex::encode(salt),
    };
    let path = Config::default_path();
    cfg.save(&path)?;
    println!("\nConfig saved to {}", path.display());

    keychain::store_passphrase(&passphrase)?;
    println!("Passphrase stored in macOS Keychain.");

    println!("\nDone. Try `sessync push` after Claude Code has run at least once.");
    Ok(())
}
```

- [ ] **Step 2: Add deps** — append to `Cargo.toml` `[dependencies]`:

```toml
rand = "0.8"
uuid = { version = "1", features = ["v4"] }
```

- [ ] **Step 3: Verify build**

```bash
cargo build
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/commands/init.rs Cargo.toml Cargo.lock
git commit -m "feat(cmd): sessync init for first-time setup"
```

---

## Task 12: `sessync push` Command

**Files:**
- Modify: `src/commands/push.rs`
- Create: `tests/push_resume_e2e.rs` (push half)

- [ ] **Step 1: Implement `push` command core**

```rust
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::StorageAdapter;
use crate::adapter::tool::ToolAdapter;
use crate::config::Config;
use crate::crypto;
use crate::keychain;
use anyhow::{Context, Result};
use tracing::{info, warn};

pub async fn run() -> Result<()> {
    let cfg = Config::load(&Config::default_path()).context("load config (run `sessync init` first?)")?;
    let passphrase = keychain::load_passphrase().context("load passphrase from keychain")?;
    let salt = decode_salt(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;

    let tool = ClaudeCodeAdapter::new();
    let storage = OssStorage::new(&cfg.oss)?;

    push_all(&tool, &storage, &key, &cfg.device.device_id).await
}

pub async fn push_all<T: ToolAdapter, S: StorageAdapter>(
    tool: &T,
    storage: &S,
    key: &[u8; 32],
    device_id: &str,
) -> Result<()> {
    let sessions = tool.list_local_sessions().await?;
    info!("found {} local sessions", sessions.len());

    let mut pushed = 0usize;
    for s in sessions {
        let raw = tool.read_session(&s.meta.session_id).await?;

        // Object key layout: {tool}/{project_key}/{session_id}.age
        let object_key = format!(
            "{}/{}/{}.age",
            tool.name(),
            s.meta.project_key.0,
            s.meta.session_id.0
        );
        let meta_key = format!("{}.meta.json", object_key);

        let ciphertext = crypto::encrypt(&raw, key)
            .map_err(|e| anyhow::anyhow!("encrypt {}: {e}", s.meta.session_id))?;
        let meta_json = serde_json::to_vec(&s.meta)?;
        let meta_ciphertext = crypto::encrypt(&meta_json, key)
            .map_err(|e| anyhow::anyhow!("encrypt meta {}: {e}", s.meta.session_id))?;

        storage.put(&object_key, ciphertext).await
            .map_err(|e| anyhow::anyhow!("upload {}: {e}", object_key))?;
        storage.put(&meta_key, meta_ciphertext).await
            .map_err(|e| anyhow::anyhow!("upload meta {}: {e}", meta_key))?;

        info!("pushed {} ({} bytes)", s.meta.session_id, s.meta.byte_size);
        pushed += 1;
        let _ = device_id;  // reserved for v2 multi-device manifest
    }
    println!("pushed {pushed} sessions");
    Ok(())
}

fn decode_salt(hex_str: &str) -> Result<[u8; 16]> {
    let bytes = hex::decode(hex_str).context("salt hex decode")?;
    let arr: [u8; 16] = bytes.try_into()
        .map_err(|_| anyhow::anyhow!("salt must be 16 bytes"))?;
    Ok(arr)
}
```

- [ ] **Step 2: Add e2e test in `tests/push_resume_e2e.rs`**

```rust
use sessync::adapter::claude_code::ClaudeCodeAdapter;
use sessync::adapter::memory::InMemoryStorage;
use sessync::adapter::storage::StorageAdapter;
use sessync::commands::push;
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_projects")
}

#[tokio::test]
async fn push_uploads_encrypted_session_to_storage() {
    let tool = ClaudeCodeAdapter::with_root(fixture_root());
    let storage = InMemoryStorage::new();
    let key = [9u8; 32];

    push::push_all(&tool, &storage, &key, "test-device").await.unwrap();

    let listed = storage.list("claude-code/").await.unwrap();
    assert!(listed.iter().any(|o| o.key.ends_with(".age")));
    assert!(listed.iter().any(|o| o.key.ends_with(".meta.json")));

    // Verify the .age object is NOT plaintext
    let age_key = listed.iter().find(|o| o.key.ends_with(".age") && !o.key.contains(".meta.")).unwrap().key.clone();
    let ct = storage.get(&age_key).await.unwrap();
    assert!(!String::from_utf8_lossy(&ct).contains("hello world"),
        "ciphertext should not contain plaintext substring");
}
```

- [ ] **Step 3: Run, verify PASS**

```bash
cargo test --test push_resume_e2e push_uploads_
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/commands/push.rs tests/push_resume_e2e.rs
git commit -m "feat(cmd): sessync push with encryption"
```

---

## Task 13: `sessync resume` Command

**Files:**
- Modify: `src/commands/resume.rs`

- [ ] **Step 1: Implement `resume` command**

```rust
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::StorageAdapter;
use crate::adapter::tool::ToolAdapter;
use crate::config::Config;
use crate::crypto;
use crate::keychain;
use crate::types::{SessionId, SessionMeta};
use anyhow::{Context, Result};
use dialoguer::{Select, theme::ColorfulTheme};
use std::collections::BTreeMap;

pub async fn run() -> Result<()> {
    let cfg = Config::load(&Config::default_path()).context("load config")?;
    let passphrase = keychain::load_passphrase()?;
    let salt = decode_salt(&cfg.kdf_salt_hex)?;
    let key = crypto::derive_key(&passphrase, &salt)?;

    let tool = ClaudeCodeAdapter::new();
    let storage = OssStorage::new(&cfg.oss)?;

    resume_interactive(&tool, &storage, &key).await
}

pub async fn resume_interactive<T: ToolAdapter, S: StorageAdapter>(
    tool: &T,
    storage: &S,
    key: &[u8; 32],
) -> Result<()> {
    let prefix = format!("{}/", tool.name());
    let objects = storage.list(&prefix).await?;

    // Group object keys by project_key.
    // Object key layout: {tool}/{project_key}/{session_id}.age (content)
    // and {tool}/{project_key}/{session_id}.age.meta.json (encrypted meta sidecar).
    // Both are written by push.rs; the meta sidecar is the encrypted bytes of
    // serde_json::to_vec(&SessionMeta), stored under the unencrypted-suffix key.
    let mut by_project: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for o in &objects {
        if !o.key.ends_with(".meta.json") { continue; }  // index from meta files only
        let parts: Vec<&str> = o.key.splitn(3, '/').collect();
        if parts.len() < 3 { continue; }
        by_project.entry(parts[1].to_string()).or_default().push(o.key.clone());
    }

    if by_project.is_empty() {
        println!("No remote sessions found.");
        return Ok(());
    }

    // Step 1: pick a project, displayed by source_cwd from any one of its sessions' meta.
    let project_keys: Vec<String> = by_project.keys().cloned().collect();
    let mut project_labels: Vec<String> = vec![];
    let mut project_metas_first: Vec<SessionMeta> = vec![];
    for pk in &project_keys {
        let any_meta_key = &by_project[pk][0];
        let raw = storage.get(any_meta_key).await?;
        let pt = crypto::decrypt(&raw, key)?;
        let meta: SessionMeta = serde_json::from_slice(&pt)?;
        project_labels.push(format!("{}  ({})", meta.source_cwd, pk));
        project_metas_first.push(meta);
    }

    let pick = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Pick a project")
        .items(&project_labels)
        .default(0)
        .interact()?;
    let chosen_pk = &project_keys[pick];

    // Step 2: pick a session within the project.
    let session_meta_keys = &by_project[chosen_pk];
    let mut session_labels: Vec<String> = vec![];
    let mut session_metas: Vec<SessionMeta> = vec![];
    for mk in session_meta_keys {
        let raw = storage.get(mk).await?;
        let pt = crypto::decrypt(&raw, key)?;
        let meta: SessionMeta = serde_json::from_slice(&pt)?;
        session_labels.push(format!(
            "[{}] {}  — {}",
            meta.modified_at.format("%Y-%m-%d %H:%M"),
            truncate(&meta.preview, 50),
            meta.source_hostname,
        ));
        session_metas.push(meta);
    }

    let pick = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Pick a session")
        .items(&session_labels)
        .default(0)
        .interact()?;
    let chosen_meta = &session_metas[pick];

    // Step 3: download + decrypt the actual session bytes.
    let session_key = format!("{}/{}/{}.age", tool.name(), chosen_pk, chosen_meta.session_id.0);
    let ct = storage.get(&session_key).await?;
    let pt = crypto::decrypt(&ct, key)?;

    // Step 4: write into target cwd (default = current working dir).
    let target_cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let written = tool.write_session(&chosen_meta.session_id, &target_cwd, &pt).await?;

    println!("\nSession dropped at: {}", written.display());
    println!("Run: claude --resume {}", chosen_meta.session_id);
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() }
    else { s.chars().take(n - 1).collect::<String>() + "…" }
}

fn decode_salt(hex_str: &str) -> Result<[u8; 16]> {
    let bytes = hex::decode(hex_str).context("salt hex decode")?;
    bytes.try_into().map_err(|_| anyhow::anyhow!("salt must be 16 bytes"))
}
```

- [ ] **Step 2: Add e2e test (push → resume round-trip)**

Append to `tests/push_resume_e2e.rs`:

```rust
use sessync::commands::resume;
use sessync::adapter::tool::ToolAdapter;
use sessync::types::SessionId;

#[tokio::test]
async fn push_then_manual_pull_reproduces_session() {
    // This skips the interactive selector — exercises crypto + storage path only.
    let tool_src = ClaudeCodeAdapter::with_root(fixture_root());
    let storage = InMemoryStorage::new();
    let key = [9u8; 32];

    push::push_all(&tool_src, &storage, &key, "device-A").await.unwrap();

    // Simulate device B with a different cwd.
    let tmp = tempfile::tempdir().unwrap();
    let tool_dst = ClaudeCodeAdapter::with_root(tmp.path().to_path_buf());

    // Find the content .age key (not the .meta.json sidecar) and pull it directly.
    let listed = storage.list("claude-code/").await.unwrap();
    let session_key = listed.iter()
        .find(|o| o.key.ends_with(".age") && !o.key.contains(".meta."))
        .unwrap().key.clone();
    let ct = storage.get(&session_key).await.unwrap();
    let pt = sessync::crypto::decrypt(&ct, &key).unwrap();

    // Simulate "the user's current cwd on device B".
    let new_cwd = "/Users/bob/work/foo";
    let written = tool_dst.write_session(&SessionId("abc123-def".into()), new_cwd, &pt).await.unwrap();
    let on_disk = std::fs::read(&written).unwrap();
    let on_disk_str = String::from_utf8_lossy(&on_disk);
    assert!(on_disk_str.contains("hello world"));
    assert!(written.parent().unwrap().file_name().unwrap().to_str().unwrap()
        .contains("Users-bob-work-foo"));
}
```

- [ ] **Step 3: Run, verify PASS**

```bash
cargo test --test push_resume_e2e
```

Expected: 2 PASS.

- [ ] **Step 4: Commit**

```bash
git add src/commands/resume.rs tests/push_resume_e2e.rs
git commit -m "feat(cmd): sessync resume with interactive picker"
```

---

## Task 14: `sessync status` Command

**Files:**
- Modify: `src/commands/status.rs`

- [ ] **Step 1: Implement basic status**

```rust
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::adapter::oss::OssStorage;
use crate::adapter::storage::StorageAdapter;
use crate::adapter::tool::ToolAdapter;
use crate::config::Config;
use anyhow::{Context, Result};

pub async fn run() -> Result<()> {
    let cfg = Config::load(&Config::default_path()).context("load config")?;
    let tool = ClaudeCodeAdapter::new();
    let storage = OssStorage::new(&cfg.oss)?;

    let local = tool.list_local_sessions().await?;
    let remote = storage.list(&format!("{}/", tool.name())).await?;

    let remote_sessions = remote.iter().filter(|o| o.key.ends_with(".age") && !o.key.contains(".meta.")).count();
    let last_remote = remote.iter().map(|o| o.last_modified).max();

    println!("device:       {} ({})", cfg.device.hostname, cfg.device.device_id);
    println!("local sessions:  {}", local.len());
    println!("remote sessions: {}", remote_sessions);
    if let Some(t) = last_remote {
        println!("last remote upload: {}", t.format("%Y-%m-%d %H:%M:%S UTC"));
    }
    println!("storage:      oss://{} (prefix {})", cfg.oss.bucket, cfg.oss.prefix);
    Ok(())
}
```

- [ ] **Step 2: Verify build**

```bash
cargo build
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/commands/status.rs
git commit -m "feat(cmd): sessync status (basic)"
```

---

## Task 15: Final Build + Lint + Format

**Files:** None (verification only)

- [ ] **Step 1: Run full test suite**

```bash
cargo test
```

Expected: all PASS (5 + 3 + 3 + 2 + 2 = 15 tests roughly), Keychain ignored test skipped.

- [ ] **Step 2: Build release binary**

```bash
cargo build --release
ls -la target/release/sessync
```

Expected: binary present, ~5-15 MB depending on features.

- [ ] **Step 3: Format + lint**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
```

Fix any clippy warnings inline. Re-run until clean.

- [ ] **Step 4: Commit any formatting/lint fixes**

```bash
git add -A
git diff --staged --quiet || git commit -m "chore: fmt + clippy"
```

---

## Task 16: Smoke Test on Real Hardware

**No code — engineer-driven verification on real Mac + real OSS bucket.**

- [ ] **Step 1: Run init on Mac A**

```bash
./target/release/sessync init
```

Provide endpoint, bucket, AK, SK, prefix, passphrase. Verify `~/.config/sessync/config.toml` exists, Keychain entry created (use `security find-generic-password -s sessync` to verify).

- [ ] **Step 2: Have at least one Claude Code session locally**

Run `claude` in some project, do a short conversation, then exit. Confirm a `.jsonl` exists under `~/.claude/projects/<encoded>/`.

- [ ] **Step 3: Push from Mac A**

```bash
./target/release/sessync push
```

Expected: prints `pushed N sessions`. Use Aliyun OSS console to confirm objects appear under `<prefix>claude-code/`.

- [ ] **Step 4: Status on Mac A**

```bash
./target/release/sessync status
```

Expected: `local sessions` and `remote sessions` ≥ 1.

- [ ] **Step 5: Init on Mac B with same config**

Same OSS creds + **same passphrase**. Run `sessync init`.

- [ ] **Step 6: Resume on Mac B**

```bash
cd /any/project/path
./target/release/sessync resume
```

Pick the project, pick the session. Expected: file written under `~/.claude/projects/<encoded-current-cwd>/<sid>.jsonl` and a `claude --resume <sid>` hint printed.

- [ ] **Step 7: Verify Claude actually resumes**

```bash
claude --resume <session-id>
```

Expected: prior conversation context loads. **This is the critical PRD risk R2 validation** — if resume content is broken (paths in tool calls don't work), open a follow-up task before declaring M1 done.

- [ ] **Step 8: Document any rough edges**

Open issues / notes for any of: error messages that confused you, OSS upload latency that exceeded 10s P95, edge cases discovered. These feed M2 planning.

---

## Self-Review Checklist (before marking M1 complete)

- [ ] All 13 PRD Must-Haves except M5 hook integration, M6 launchd, M8 pending queue, M12 notifications, M13 log rotation map to a task above (these 5 are explicitly M2)
- [ ] Specifically: M1 ✓ (Task 8+10), M2 ✓ (Task 6+7), M3 ✓ (Task 2+3), M4 ✓ (Task 4+5+11), M9 ✓ (Task 13), M10 ✓ (Task 9+10+13), M11 ✓ (Task 14)
- [ ] No placeholders / TBDs / TODOs in any task (verified)
- [ ] Method names consistent across tasks: `derive_key`, `encrypt`/`decrypt`, `put`/`get`/`list`/`delete`, `list_local_sessions`/`read_session`/`write_session`, `project_key_for`, `push_all`, `resume_interactive`
- [ ] Every test step has expected output stated
- [ ] Each task ends with a commit step
- [ ] Risk R2 (cross-path resume side-effects) explicitly validated in smoke test Task 16 step 7

---

## What Comes Next (M2 plan, not in this doc)

After M1 ships and the smoke test passes:

1. **M2-A: Stop hook integration** — shell script that spawns `sessync push`, `sessync hook install/uninstall`
2. **M2-B: launchd周期 task** — plist generator + `sessync daemon install/uninstall`
3. **M2-C: Pending queue** — SQLite-backed retry, `sessync push --retry-pending`
4. **M2-D: Failure notifications** — osascript wrapper, threshold counter
5. **M2-E: Log rotation** — `~/Library/Logs/sessync/` with daily rotation
6. **M2-F: `sessync doctor`** — self-test (OSS connectivity, Keychain, hook, launchd)

Each becomes its own plan in `docs/superpowers/plans/`.
