use crate::types::ProjectKey;
use sha2::{Digest, Sha256};

/// Encode a filesystem path the way Claude Code does for project directory names.
/// `/Users/alice/Project/foo` → `-Users-alice-Project-foo`
///
/// Lossy: literal `-` in path components is indistinguishable from `/` after encoding,
/// matching Claude Code's own behavior. Decoding is best-effort and lives in the adapter.
pub fn encode_cwd(cwd: &str) -> String {
    cwd.replace('/', "-")
}

/// Stable project key = hex(sha256(cwd)), truncated to first 8 bytes.
/// Used as OSS prefix segment.
///
/// Same path on two devices → same key → groups together in selector.
/// Different paths (e.g. /Users/foo vs /home/foo) → different keys → user picks
/// which to pull at resume time, then writes into local cwd.
///
/// 64 bits of hash is sufficient for single-user collision safety (birthday bound ~2^32).
/// Do not reuse this key for multi-tenant scenarios without widening.
/// Inputs are not normalized — `/foo` and `/foo/` produce different keys; pass canonical paths.
pub fn project_key_for_cwd(cwd: &str) -> ProjectKey {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let digest = hasher.finalize();
    ProjectKey(hex::encode(&digest[..8]))
}
