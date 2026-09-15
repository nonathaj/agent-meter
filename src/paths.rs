//! Locations of agent-meter's own files.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// Environment variable that overrides the data directory.
pub const DATA_DIR_ENV: &str = "AGENT_METER_DIR";

/// Resolves agent-meter's data directory.
///
/// `AGENT_METER_DIR` wins; otherwise the platform's local data directory is
/// used (`%LOCALAPPDATA%\agent-meter`, `~/Library/Application Support/agent-meter`,
/// `$XDG_DATA_HOME/agent-meter`).
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            bail!("{DATA_DIR_ENV} must be an absolute path, got {}", dir.display());
        }
        return Ok(dir);
    }
    let base = dirs::data_local_dir().context("could not determine the local data directory")?;
    Ok(base.join("agent-meter"))
}

/// The user's home directory.
pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("could not determine the home directory")
}
