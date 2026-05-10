//! `sessync upgrade` — update sessync via Homebrew.
//!
//! Runs `brew update` then `brew upgrade sessync`, streaming both commands'
//! output live.  If brew is not on PATH the user gets a clear install link.

use anyhow::{bail, Result};
use tokio::process::Command;

/// Run `brew update && brew upgrade sessync`, streaming output in real time.
pub async fn run() -> Result<()> {
    println!("Current version: {}", env!("CARGO_PKG_VERSION"));

    run_brew(&["update"]).await?;
    run_brew(&["upgrade", "sessync"]).await?;

    println!(
        "Done. Run `sessync --version` to confirm. \
        See CHANGELOG: https://github.com/echobyte26/sessync/blob/main/CHANGELOG.md"
    );
    Ok(())
}

/// Spawn `brew <args>` with inherited stdio and wait for it to finish.
///
/// On `NotFound` (brew not in PATH) we emit a helpful message and return an
/// error rather than bubbling up a raw OS error.
async fn run_brew(args: &[&str]) -> Result<()> {
    let mut child = match Command::new("brew").args(args).spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "brew not found in PATH — sessync upgrade requires Homebrew \
                (https://brew.sh)"
            );
        }
        Err(e) => return Err(e.into()),
    };

    let status = child.wait().await?;
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        bail!("`brew {}` exited with status {code}", args.join(" "));
    }
    Ok(())
}
