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
    /// These are USER-ADDED patterns; they compose with the built-in zero-config
    /// heuristic (`is_plugin_cwd_under_home`) applied inside `matches()`.
    #[serde(default)]
    pub project_path_contains: Vec<String>,
}

/// Returns `true` if `cwd` looks like a plugin/tool data directory rather than
/// a user project. Heuristic: `$HOME/.<anything>/...`.
///
/// Covers claude-mem, Claude Code plugins, Codex plugins, and any future
/// plugin that follows the Unix dotfile convention — without maintaining a
/// per-plugin allowlist.
///
/// Examples:
///   /Users/X/.claude-mem/observer/abc       → true (plugin)
///   /Users/X/.claude/plugins/foo            → true (plugin)
///   /Users/X/.codex/plugins/bar             → true (plugin)
///   /Users/X/.future-plugin-v3/whatever     → true (plugin) ← future-proof
///   /Users/X/Project/foo                    → false (user)
///   /Users/X/code/.git/internal             → false (user — .git is mid-path)
///   /Users/X/Documents/work/baz             → false (user)
pub fn is_plugin_cwd_under_home(cwd: &str, home: &str) -> bool {
    let prefix = format!("{home}/");
    let Some(rest) = cwd.strip_prefix(&prefix) else {
        return false;
    };
    // First path component starts with '.' → dotfile dir under home → plugin
    rest.starts_with('.')
}

impl ExcludeConfig {
    /// Returns `true` if `source_cwd` matches any exclusion pattern.
    ///
    /// Layer 1: zero-config "dotfile under $HOME" heuristic — catches all Unix
    /// dotfile tool dirs (`~/.claude-mem/`, `~/.claude/plugins/`, `~/.codex/plugins/`,
    /// and any future plugin following the convention) without a hardcoded list.
    ///
    /// Layer 2: user-configured substring patterns — legacy support and escape hatch
    /// for unusual paths the heuristic doesn't catch.
    pub fn matches(&self, source_cwd: &str) -> bool {
        // Layer 1: zero-config "dotfile under home" heuristic (universal)
        if let Ok(home) = std::env::var("HOME") {
            if is_plugin_cwd_under_home(source_cwd, &home) {
                return true;
            }
        }
        // Layer 2: user-configured substring patterns (legacy support)
        // Still useful for: catching garbled cwds from old sessync versions,
        // or for unusual paths the heuristic doesn't catch.
        self.project_path_contains
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

    /// ExcludeConfig defaults to an empty USER pattern list. The zero-config
    /// heuristic still applies via $HOME env var; normal user paths are not excluded.
    #[test]
    fn exclude_config_defaults_to_empty() {
        let e = ExcludeConfig::default();
        assert!(e.project_path_contains.is_empty());
        // A normal user path doesn't hit the heuristic either (not a dotfile dir).
        // Use a path that couldn't possibly start with $HOME/. to be env-independent.
        assert!(!e.matches("/NONEXISTENT_HOME_XYZ/Project/azoth"), "empty patterns must not match user paths");
    }

    /// The zero-config heuristic excludes all dotfile dirs under $HOME, even with
    /// empty user config — covering claude-mem, claude plugins, codex plugins, and
    /// any future plugin following the Unix dotfile convention.
    #[test]
    fn exclude_config_heuristic_catches_dotfile_dirs_under_home() {
        let home = std::env::var("HOME").expect("HOME must be set for this test");
        let e = ExcludeConfig::default();

        // All dotfile dirs under $HOME should be excluded.
        assert!(e.matches(&format!("{home}/.claude/plugins/marketplaces/something/sub")));
        assert!(e.matches(&format!("{home}/.claude-mem/observer-sessions/abc")));
        assert!(e.matches(&format!("{home}/.codex/plugins/whatever")));
        assert!(e.matches(&format!("{home}/.future-plugin-v3/whatever")));

        // Normal user dirs should NOT be excluded by the heuristic.
        assert!(!e.matches(&format!("{home}/Project/azoth")));
        assert!(!e.matches(&format!("{home}/code/myproject")));
        // Dotfile mid-path (e.g. .git) should NOT be excluded.
        assert!(!e.matches(&format!("{home}/Project/foo/.git/internal")));
    }

    /// Zero-config heuristic + user patterns are OR'd together.
    #[test]
    fn exclude_config_user_patterns_compose_with_heuristic() {
        let home = std::env::var("HOME").expect("HOME must be set for this test");
        let e = ExcludeConfig {
            project_path_contains: vec!["my-noisy-plugin".to_string()],
        };
        // Heuristic still works (dotfile under $HOME).
        assert!(e.matches(&format!("{home}/.claude-mem/sessions")));
        // User-added pattern works (not a dotfile dir under $HOME).
        assert!(e.matches(&format!("{home}/code/my-noisy-plugin/x")));
        // Both miss → not excluded.
        assert!(!e.matches(&format!("{home}/real-project")));
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

    // ── is_plugin_cwd_under_home unit tests ──────────────────────────────────

    /// Dotfile dirs directly under $HOME are identified as plugin paths.
    #[test]
    fn is_plugin_cwd_under_home_catches_dotfile_under_home() {
        let home = "/Users/X";
        assert!(is_plugin_cwd_under_home("/Users/X/.claude-mem/observer/abc", home));
        assert!(is_plugin_cwd_under_home("/Users/X/.claude/plugins/foo", home));
        assert!(is_plugin_cwd_under_home("/Users/X/.codex/plugins/bar", home));
        assert!(is_plugin_cwd_under_home("/Users/X/.foo-helper/data", home));
        assert!(is_plugin_cwd_under_home("/Users/X/.future-plugin-v2/whatever", home));
    }

    /// Normal user project dirs are NOT identified as plugin paths.
    #[test]
    fn is_plugin_cwd_under_home_misses_user_dirs() {
        let home = "/Users/X";
        assert!(!is_plugin_cwd_under_home("/Users/X/Project/foo", home));
        assert!(!is_plugin_cwd_under_home("/Users/X/code/bar", home));
        assert!(!is_plugin_cwd_under_home("/Users/X/Documents/baz", home));
    }

    /// A dotfile mid-path (e.g. .git inside a project) is NOT treated as plugin.
    #[test]
    fn is_plugin_cwd_under_home_misses_dotfile_mid_path() {
        let home = "/Users/X";
        // The first component after $HOME/ is "Project" (no dot), so not a plugin.
        assert!(!is_plugin_cwd_under_home("/Users/X/Project/foo/.git/internal", home));
        assert!(!is_plugin_cwd_under_home("/Users/X/code/bar/.hidden/stuff", home));
    }

    /// A path outside $HOME is never identified as a plugin path.
    #[test]
    fn is_plugin_cwd_under_home_handles_no_match() {
        let home = "/Users/X";
        // Completely different root — strip_prefix fails.
        assert!(!is_plugin_cwd_under_home("/tmp/.plugin/data", home));
        assert!(!is_plugin_cwd_under_home("/var/lib/.something", home));
        // Path IS $HOME itself but no trailing slash component.
        assert!(!is_plugin_cwd_under_home("/Users/X", home));
        // Another user's home.
        assert!(!is_plugin_cwd_under_home("/Users/Y/.claude-mem/sessions", home));
    }

    /// ExcludeConfig with empty user patterns excludes claude-mem and other
    /// dotfile dirs under $HOME via the zero-config heuristic alone.
    #[test]
    fn exclude_matches_uses_heuristic_with_empty_user_patterns() {
        let home = std::env::var("HOME").expect("HOME must be set for this test");
        let e = ExcludeConfig::default();

        // All dotfile dirs under $HOME → excluded by heuristic, no user patterns needed.
        assert!(
            e.matches(&format!("{home}/.claude-mem/observer/some-session")),
            "claude-mem under $HOME must be excluded by default heuristic"
        );
        assert!(
            e.matches(&format!("{home}/.claude/plugins/marketplace/foo")),
            "claude plugins under $HOME must be excluded by default heuristic"
        );
        assert!(
            e.matches(&format!("{home}/.codex/plugins/bar")),
            "codex plugins under $HOME must be excluded by default heuristic"
        );
        assert!(
            e.matches(&format!("{home}/.new-ai-tool-2027/sessions/xyz")),
            "any future dotfile plugin under $HOME must be excluded by default heuristic"
        );

        // Normal user project dirs → not excluded.
        assert!(
            !e.matches(&format!("{home}/Project/myapp")),
            "user project dirs must not be excluded"
        );
    }
}
