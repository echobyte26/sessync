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
}

fn default_meta_version() -> u32 {
    1
}
