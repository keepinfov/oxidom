use std::path::PathBuf;

use anyhow::{Result, anyhow};

/// When running as a systemd system service, all state lives in the unit's
/// StateDirectory (e.g. /var/lib/oxidom) instead of the user's XDG dirs.
fn state_directory() -> Option<PathBuf> {
    std::env::var_os("STATE_DIRECTORY")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = state_directory() {
        return Ok(dir);
    }
    Ok(dirs::config_dir()
        .ok_or_else(|| anyhow!("no XDG config dir"))?
        .join("oxidom"))
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = state_directory() {
        return Ok(dir);
    }
    Ok(dirs::data_dir()
        .ok_or_else(|| anyhow!("no XDG data dir"))?
        .join("oxidom"))
}

pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn subscriptions_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("subscriptions.json"))
}

pub fn state_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("state.toml"))
}

pub fn hwid_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("hwid"))
}
