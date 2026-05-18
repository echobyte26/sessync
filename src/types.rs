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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Wire-format version. Bump on breaking changes to this struct's serde shape.
    #[serde(default = "default_meta_version")]
    pub schema_version: u32,
    pub session_id: SessionId,
    pub project_key: ProjectKey,
    /// Original cwd from the source device (for display only — not used to map paths).
    pub source_cwd: String,
    pub source_hostname: String,
    pub modified_at: chrono::DateTime<chrono::Utc>,
    pub byte_size: u64,
    /// First user message, truncated to 80 chars (UI hint for the resume picker).
    pub preview: String,
    /// True if the underlying session contains at least one user message event.
    /// False for "ghost" sessions created as side-effects of Claude Code plugin
    /// hooks (SessionStart progress reports, file-history snapshots, etc.).
    /// Defaults to true on deserialize for backward compatibility — old metas
    /// without this field are treated as real sessions.
    #[serde(default = "default_has_user_message")]
    pub has_user_message: bool,
}

fn default_meta_version() -> u32 {
    1
}

fn default_has_user_message() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Old metas (from sessync < v0.8.1) serialized without the `has_user_message`
    /// field. When deserialized, the serde default must kick in and return `true`
    /// (conservative / backward-compatible: treat unknown as real session).
    #[test]
    fn session_meta_defaults_has_user_message_to_true() {
        // A minimal JSON blob without the has_user_message field — simulates
        // a meta object pushed by sessync v0.8.0 or earlier.
        let json = r#"{
            "schema_version": 1,
            "session_id": "abc-123",
            "project_key": "deadbeef",
            "source_cwd": "/home/user/proj",
            "source_hostname": "laptop",
            "modified_at": "2026-01-01T00:00:00Z",
            "byte_size": 42,
            "preview": "hello"
        }"#;

        let meta: SessionMeta = serde_json::from_str(json)
            .expect("deserialization of legacy meta must not fail");

        assert!(
            meta.has_user_message,
            "legacy meta without has_user_message field must default to true (backward compat)"
        );
    }

    /// A new meta explicitly serialized with has_user_message=false (ghost session)
    /// must deserialize to false — the field must survive a round-trip.
    #[test]
    fn session_meta_preserves_has_user_message_false() {
        let json = r#"{
            "schema_version": 1,
            "session_id": "ghost-999",
            "project_key": "cafebabe",
            "source_cwd": "/home/user/proj",
            "source_hostname": "laptop",
            "modified_at": "2026-01-01T00:00:00Z",
            "byte_size": 0,
            "preview": "",
            "has_user_message": false
        }"#;

        let meta: SessionMeta = serde_json::from_str(json)
            .expect("deserialization of ghost meta must not fail");

        assert!(
            !meta.has_user_message,
            "ghost meta with has_user_message=false must deserialize to false"
        );
    }
}
