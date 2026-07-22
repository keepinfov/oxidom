use std::path::PathBuf;

use anyhow::{anyhow, Result};

pub fn config_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .ok_or_else(|| anyhow!("no XDG config dir"))?
        .join("oxidom"))
}

pub fn data_dir() -> Result<PathBuf> {
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
