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
