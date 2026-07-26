use std::path::PathBuf;
use std::sync::RwLock;

use anyhow::{Result, anyhow};

static TEST_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);

#[doc(hidden)]
pub fn set_test_root(dir: Option<PathBuf>) {
    *TEST_ROOT
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = dir;
}

/// Serialises tests that install a root: it is process-global on purpose, so
/// that worker threads spawned by the daemon see the same one.
#[doc(hidden)]
pub static TEST_ROOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_root() -> Option<PathBuf> {
    TEST_ROOT
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// When running as a systemd system service, all state lives in the unit's
/// StateDirectory (e.g. /var/lib/oxidom) instead of the user's XDG dirs.
fn state_directory() -> Option<PathBuf> {
    std::env::var_os("STATE_DIRECTORY")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = test_root() {
        return Ok(dir);
    }
    if let Some(dir) = state_directory() {
        return Ok(dir);
    }
    Ok(dirs::config_dir()
        .ok_or_else(|| anyhow!("no XDG config dir"))?
        .join("oxidom"))
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = test_root() {
        return Ok(dir);
    }
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

/// GUI-only display preferences (e.g. collapsed subscription groups). Unlike
/// `config.toml`/`state.toml`, this file is owned and written directly by the
/// GUI process itself, never by the daemon.
pub fn gui_prefs_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("gui_prefs.toml"))
}
