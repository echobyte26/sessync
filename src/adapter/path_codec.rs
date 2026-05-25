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

/// v0.12.0: extract the project's display name from its full cwd path.
/// Same-name projects across devices share one picker entry — `/Users/X/A/sessync`
/// and `/Users/Y/B/sessync` both display as `sessync`.
pub fn basename_for_cwd(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches('/');
    if let Some(last) = trimmed.rsplit('/').next() {
        if !last.is_empty() {
            return last.to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_for_cwd_strips_trailing_slash() {
        assert_eq!(basename_for_cwd("/foo/bar/baz"), "baz");
        assert_eq!(basename_for_cwd("/foo/bar/baz/"), "baz");
    }

    #[test]
    fn basename_for_cwd_handles_root() {
        // Root path is an edge case; we return empty string (after stripping
        // trailing /).  Callers should treat empty basename as "unknown".
        assert_eq!(basename_for_cwd("/"), "");
    }

    #[test]
    fn basename_for_cwd_same_name_across_paths() {
        // The whole point of v0.12 grouping: same basename collapses regardless of path
        assert_eq!(
            basename_for_cwd("/Users/jameschen/Project/ai-coding-project/sessync"),
            basename_for_cwd("/Users/sakuragi/Project/VibeCodingProjects/sessync"),
        );
        assert_eq!(
            basename_for_cwd("/Users/jameschen/Project/ai-coding-project/sessync"),
            "sessync"
        );
    }

    #[test]
    fn basename_for_cwd_empty_string() {
        assert_eq!(basename_for_cwd(""), "");
    }
}
