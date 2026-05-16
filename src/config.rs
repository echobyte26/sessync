use crate::error::{Result, SessyncError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageKind {
    #[default]
    Oss,
    LocalFs,
}

/// Patterns that prevent sessions from being synced.
///
/// Applied in push (before upload) and in pull/ls (after decrypting the sidecar
/// meta). Case-sensitive substring matching against `source_cwd`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ExcludeConfig {
    /// Skip sessions whose `source_cwd` contains ANY of these substrings.
    /// Case-sensitive. Examples: `"claude-mem"`, `".claude-mem"`, `"plugins/marketplace"`.
    /// These are USER-ADDED patterns; built-in defaults below are always applied
    /// on top of these.
    #[serde(default)]
    pub project_path_contains: Vec<String>,
}

/// Always-on plugin-path blacklist. These three are the only paths where no
/// legitimate user-session use case exists — they're plugin / SDK / hook
/// machinery dirs. Users cannot disable these; they can however ADD more
/// custom patterns via `[exclude] project_path_contains` if they need.
///
/// Chosen narrow on purpose: paths like `/tmp/` or `node_modules/` are NOT
/// here because users sometimes legitimately use Claude/Codex from those.
pub const HARDCODED_PLUGIN_PATHS: &[&str] = &[
    "/.claude/plugins/",   // all Claude Code marketplace plugins
    "/.claude-mem/",       // claude-mem plugin's data dir
    "/.codex/plugins/",    // all Codex plugins
];

impl ExcludeConfig {
    /// Returns `true` if `source_cwd` matches any exclusion pattern —
    /// either a user-configured one or a hardcoded plugin path.
    pub fn matches(&self, source_cwd: &str) -> bool {
        HARDCODED_PLUGIN_PATHS
            .iter()
            .any(|pat| source_cwd.contains(pat))
            || self
                .project_path_contains
                .iter()
                .any(|pat| source_cwd.contains(pat.as_str()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Which storage backend to use. Defaults to `oss` when absent
    /// (preserves config compat with v1 installs that predate this field).
    #[serde(default)]
    pub storage_kind: StorageKind,
    /// Required when storage_kind = Oss. Optional otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oss: Option<OssConfig>,
    /// Required when storage_kind = LocalFs. Optional otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_fs: Option<LocalFsConfig>,
    pub device: DeviceConfig,
    /// Salt for argon2 KDF. 16 random bytes generated at `init` time, hex-encoded.
    pub kdf_salt_hex: String,
    /// Optional session exclusion rules.
    #[serde(default, skip_serializing_if = "is_default_exclude")]
    pub exclude: ExcludeConfig,
}

/// Helper so that a default (empty) `[exclude]` section is omitted from
/// serialized TOML (keeps existing config files unchanged).
fn is_default_exclude(e: &ExcludeConfig) -> bool {
    e.project_path_contains.is_empty()
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

fn default_prefix() -> String {
    "sessync/".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFsConfig {
    /// Filesystem directory acting as the "bucket". Created on init.
    pub root: PathBuf,
}

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
        let text = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SessyncError::Config(format!(
                    "config not found at {} — run `sessync init`",
                    path.display()
                ))
            } else {
                SessyncError::Config(format!("read {}: {e}", path.display()))
            }
        })?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| SessyncError::Config(format!("parse: {e}")))?;
        cfg.validate_backend()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| SessyncError::Config(format!("serialize: {e}")))?;
        std::fs::write(path, text)?;
        // Restrict to owner-only — the file holds OSS AccessKeySecret in plaintext
        // (and the local-fs backend's root is benign, but keep the policy uniform).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Confirm the storage_kind matches the populated backend section.
    fn validate_backend(&self) -> Result<()> {
        match self.storage_kind {
            StorageKind::Oss => {
                if self.oss.is_none() {
                    return Err(SessyncError::Config(
                        "storage_kind = oss but [oss] section is missing".into(),
                    ));
                }
            }
            StorageKind::LocalFs => {
                if self.local_fs.is_none() {
                    return Err(SessyncError::Config(
                        "storage_kind = local-fs but [local_fs] section is missing".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_oss_config() -> Config {
        Config {
            storage_kind: StorageKind::Oss,
            oss: Some(OssConfig {
                endpoint: "oss-cn-hangzhou.aliyuncs.com".into(),
                bucket: "my-sessync".into(),
                access_key_id: "AKIDxxx".into(),
                access_key_secret: "secretxxx".into(),
                prefix: "sessync/".into(),
            }),
            local_fs: None,
            device: DeviceConfig {
                device_id: "11111111-1111-1111-1111-111111111111".into(),
                hostname: "test-mac.local".into(),
            },
            kdf_salt_hex: "00".repeat(16),
            exclude: ExcludeConfig::default(),
        }
    }

    fn sample_local_fs_config(root: PathBuf) -> Config {
        Config {
            storage_kind: StorageKind::LocalFs,
            oss: None,
            local_fs: Some(LocalFsConfig { root }),
            device: DeviceConfig {
                device_id: "22222222-2222-2222-2222-222222222222".into(),
                hostname: "dev-mac.local".into(),
            },
            kdf_salt_hex: "11".repeat(16),
            exclude: ExcludeConfig::default(),
        }
    }

    #[test]
    fn save_then_load_roundtrips_oss() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let c = sample_oss_config();
        c.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.storage_kind, StorageKind::Oss);
        assert_eq!(loaded.oss.unwrap().bucket, "my-sessync");
        assert_eq!(loaded.device.hostname, "test-mac.local");
    }

    #[test]
    fn save_then_load_roundtrips_local_fs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let store = dir.path().join("store");
        let c = sample_local_fs_config(store.clone());
        c.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.storage_kind, StorageKind::LocalFs);
        assert_eq!(loaded.local_fs.unwrap().root, store);
    }

    #[test]
    fn save_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/deep/config.toml");
        sample_oss_config().save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn load_missing_config_suggests_init() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.toml");
        let err = Config::load(&path).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("sessync init"), "got: {msg}");
    }

    #[test]
    fn load_supplies_default_prefix_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let toml = r#"
kdf_salt_hex = "00000000000000000000000000000000"

[oss]
endpoint = "oss-cn-hangzhou.aliyuncs.com"
bucket = "b"
access_key_id = "ak"
access_key_secret = "sk"

[device]
device_id = "d"
hostname = "h"
"#;
        std::fs::write(&path, toml).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.storage_kind, StorageKind::Oss);
        assert_eq!(cfg.oss.as_ref().unwrap().prefix, "sessync/");
    }

    #[test]
    fn load_rejects_kind_without_matching_backend() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let toml = r#"
storage_kind = "local-fs"
kdf_salt_hex = "00000000000000000000000000000000"

[device]
device_id = "d"
hostname = "h"
"#;
        std::fs::write(&path, toml).unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(format!("{err}").contains("local_fs"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        sample_oss_config().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "expected 0600, got {:o}", mode & 0o777);
    }

    // ── ExcludeConfig tests ───────────────────────────────────────────────────

    /// ExcludeConfig defaults to an empty USER pattern list. Hardcoded plugin
    /// blacklist still applies (it can't be disabled by default).
    #[test]
    fn exclude_config_defaults_to_empty() {
        let e = ExcludeConfig::default();
        assert!(e.project_path_contains.is_empty());
        // A normal user path doesn't hit the hardcoded list either.
        assert!(!e.matches("/Users/foo/Project/azoth"), "empty patterns must not match user paths");
    }

    /// Hardcoded plugin paths are always excluded, even with empty user config.
    #[test]
    fn exclude_config_hardcoded_paths_always_match() {
        let e = ExcludeConfig::default();
        assert!(e.matches("/Users/foo/.claude/plugins/marketplaces/something/sub"));
        assert!(e.matches("/Users/foo/.claude-mem/observer-sessions/abc"));
        assert!(e.matches("/Users/foo/.codex/plugins/whatever"));
        // Sanity: paths that don't contain any hardcoded pattern are NOT excluded.
        assert!(!e.matches("/Users/foo/Project/azoth"));
        assert!(!e.matches("/Users/foo/.codex/sessions"));  // codex sessions OK
        assert!(!e.matches("/Users/foo/.claude/projects"));  // claude projects OK
    }

    /// Hardcoded + user patterns are OR'd together.
    #[test]
    fn exclude_config_user_patterns_compose_with_hardcoded() {
        let e = ExcludeConfig {
            project_path_contains: vec!["my-noisy-plugin".to_string()],
        };
        // Hardcoded still works.
        assert!(e.matches("/Users/foo/.claude-mem/sessions"));
        // User-added works.
        assert!(e.matches("/Users/foo/my-noisy-plugin/x"));
        // Both miss → not excluded.
        assert!(!e.matches("/Users/foo/real-project"));
    }

    /// matches() returns true when source_cwd contains any pattern substring.
    #[test]
    fn exclude_config_matches_substring() {
        let e = ExcludeConfig {
            project_path_contains: vec!["claude-mem".to_string(), "plugins/marketplace".to_string()],
        };

        // Matches first pattern.
        assert!(e.matches("/home/user/.claude-mem/observer-sessions/abc"));
        // Matches second pattern.
        assert!(e.matches("/home/user/plugins/marketplace/foo"));
        // No match.
        assert!(!e.matches("/home/user/my-project"));
        // Case-sensitive: uppercase variant does NOT match.
        assert!(!e.matches("/home/user/.CLAUDE-MEM/sessions"));
    }

    /// A config with [exclude] section round-trips through save/load correctly.
    #[test]
    fn exclude_config_roundtrips_through_toml() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = sample_oss_config();
        cfg.exclude = ExcludeConfig {
            project_path_contains: vec!["claude-mem".to_string(), ".hidden-plugin".to_string()],
        };
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(
            loaded.exclude.project_path_contains,
            vec!["claude-mem", ".hidden-plugin"],
        );
    }

    /// A config without an [exclude] section loads with an empty ExcludeConfig.
    #[test]
    fn exclude_config_absent_section_is_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let toml = r#"
kdf_salt_hex = "00000000000000000000000000000000"

[oss]
endpoint = "oss-cn-hangzhou.aliyuncs.com"
bucket = "b"
access_key_id = "ak"
access_key_secret = "sk"

[device]
device_id = "d"
hostname = "h"
"#;
        std::fs::write(&path, toml).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.exclude.project_path_contains.is_empty());
        assert!(!cfg.exclude.matches("/some/path"));
    }

    /// A default ExcludeConfig does not appear in the serialised TOML
    /// (to keep existing config files clean).
    #[test]
    fn empty_exclude_config_not_serialised() {
        let cfg = sample_oss_config(); // exclude is default (empty)
        let toml_str = toml::to_string_pretty(&cfg).unwrap();
        assert!(
            !toml_str.contains("[exclude]"),
            "empty [exclude] should not appear in TOML: {toml_str}"
        );
    }

    /// The pure `matches_exclude` helper semantics — case-sensitive substring.
    #[test]
    fn matches_exclude_helper_semantics() {
        fn matches_exclude(source_cwd: &str, patterns: &[String]) -> bool {
            patterns.iter().any(|p| source_cwd.contains(p.as_str()))
        }

        let pats: Vec<String> = vec!["claude-mem".to_string()];
        assert!(matches_exclude("/home/.claude-mem/sessions", &pats));
        assert!(!matches_exclude("/home/claude/project", &pats));
        assert!(!matches_exclude("/home/.CLAUDE-MEM/sessions", &pats)); // case-sensitive
    }
}
