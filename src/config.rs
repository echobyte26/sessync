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
}
