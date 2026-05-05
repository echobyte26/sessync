use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Pure helper — takes both source and target explicitly so tests can drive it
/// without touching `current_exe()`.
pub(crate) fn install_binary(source: &Path, target: &Path, no_codesign: bool) -> Result<()> {
    // mkdir -p target.parent()
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("couldn't create directory {}", parent.display()))?;
    }

    // Copy binary (works cross-filesystem; rename would not).
    std::fs::copy(source, target)
        .with_context(|| format!("couldn't copy {} → {}", source.display(), target.display()))?;

    // chmod +x on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(target)
            .with_context(|| format!("couldn't read metadata for {}", target.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(target, perms)
            .with_context(|| format!("couldn't set permissions on {}", target.display()))?;
    }

    // Drop quarantine attribute on macOS (silently — attr may not exist).
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(target)
            .status();
    }

    // Ad-hoc codesign on macOS unless --no-codesign.
    #[cfg(target_os = "macos")]
    if !no_codesign {
        let status = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(target)
            .status()
            .map_err(|e| anyhow::anyhow!("codesign spawn failed: {e}\nHint: install Xcode Command Line Tools with `xcode-select --install`"))?;
        if !status.success() {
            anyhow::bail!("codesign exited {}", status);
        }
        println!("✓ codesign (ad-hoc)");
    }

    // Suppress unused-variable warning on non-macOS.
    #[cfg(not(target_os = "macos"))]
    let _ = no_codesign;

    Ok(())
}

/// Returns true if `target_parent` is listed in `path_env` (colon-separated).
pub(crate) fn is_target_dir_in_path(target_parent: &Path, path_env: &str) -> bool {
    path_env
        .split(':')
        .any(|entry| Path::new(entry) == target_parent)
}

pub async fn run(target: Option<PathBuf>, no_codesign: bool) -> Result<()> {
    // 1. Resolve current binary path.
    let source = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("couldn't locate self executable: {e}"))?;

    // 2. Resolve target path.
    let target = target.unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| {
            eprintln!("WARNING: $HOME is unset, installing to current directory");
            ".".into()
        });
        PathBuf::from(home).join(".local/bin/sessync")
    });

    // 3. Refuse to install over self.
    let source_canonical = source.canonicalize().ok();
    let target_canonical = target.canonicalize().ok();
    if source_canonical.is_some() && source_canonical == target_canonical {
        println!("Already installed at {} — nothing to do.", target.display());
        return Ok(());
    }

    // 4-8. Copy, chmod, quarantine-drop, codesign.
    install_binary(&source, &target, no_codesign)?;

    // 9. PATH check.
    let path_env = std::env::var("PATH").unwrap_or_default();
    if let Some(target_parent) = target.parent() {
        if is_target_dir_in_path(target_parent, &path_env) {
            println!("✓ {} is in PATH", target_parent.display());
        } else {
            let shell = std::env::var("SHELL").unwrap_or_default();
            if shell.ends_with("/zsh") {
                println!(
                    "! Add this to ~/.zshrc and reopen terminal:\n    export PATH=\"$HOME/.local/bin:$PATH\""
                );
            } else if shell.ends_with("/bash") {
                println!(
                    "! Add this to ~/.bashrc and reopen terminal:\n    export PATH=\"$HOME/.local/bin:$PATH\""
                );
            } else {
                println!(
                    "! {} is not in PATH. Add it to your shell's config file:\n    export PATH=\"{}:$PATH\"",
                    target_parent.display(),
                    target_parent.display()
                );
            }
        }
    }

    // 10. Final tip.
    println!(
        "\nInstalled: {}\n\nFirst run will trigger a macOS Keychain prompt. Click \"Always Allow\" so\nsubsequent commands don't re-prompt.\n\nTry: sessync --version",
        target.display()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Create a dummy source binary (a non-empty file) in a tempdir.
    fn make_dummy_binary(dir: &TempDir) -> PathBuf {
        let src = dir.path().join("sessync_src");
        std::fs::write(&src, b"#!/bin/sh\necho hi\n").unwrap();
        src
    }

    #[test]
    fn test_install_binary_copies_and_chmods() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        let source = make_dummy_binary(&src_dir);
        // Nested target dir that doesn't exist yet.
        let target = dst_dir.path().join("bin/sessync");

        install_binary(&source, &target, true).unwrap();

        assert!(target.exists(), "target file must exist after install");

        // Verify chmod +x (mode 0755).
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        // Check owner-execute bit at minimum.
        assert_eq!(mode & 0o111, 0o111, "target must be executable (mode & 0o111)");
    }

    #[test]
    fn test_install_binary_content_matches() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        let source = make_dummy_binary(&src_dir);
        let target = dst_dir.path().join("sessync");

        install_binary(&source, &target, true).unwrap();

        let src_bytes = std::fs::read(&source).unwrap();
        let dst_bytes = std::fs::read(&target).unwrap();
        assert_eq!(src_bytes, dst_bytes, "copied content must match source");
    }

    #[test]
    fn test_is_target_dir_in_path_found() {
        let path_env = "/usr/bin:/usr/local/bin:/home/user/.local/bin";
        let target = Path::new("/home/user/.local/bin");
        assert!(is_target_dir_in_path(target, path_env));
    }

    #[test]
    fn test_is_target_dir_in_path_not_found() {
        let path_env = "/usr/bin:/usr/local/bin";
        let target = Path::new("/home/user/.local/bin");
        assert!(!is_target_dir_in_path(target, path_env));
    }

    #[test]
    fn test_is_target_dir_in_path_empty() {
        assert!(!is_target_dir_in_path(Path::new("/usr/bin"), ""));
    }
}
