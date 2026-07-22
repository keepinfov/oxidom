use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Last active server id.
    pub active_server_id: Option<String>,
    /// Remembered per-app route choices (desktop-id/binary -> server id).
    pub app_routes: HashMap<String, String>,
}

impl State {
    pub fn load() -> State {
        let Ok(path) = paths::state_file() else {
            return State::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_default(),
            Err(_) => State::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::state_file()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let s = toml::to_string_pretty(self).context("serializing state")?;
        std::fs::write(&path, s).context("writing state")?;
        Ok(())
    }
}

/// Storage for the cached subscriptions list (subscriptions.json).
pub mod store {
    use anyhow::{Context, Result};

    use crate::model::Subscription;
    use crate::paths;

    pub fn load() -> Vec<Subscription> {
        let Ok(path) = paths::subscriptions_file() else {
            return Vec::new();
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    pub fn save(subs: &[Subscription]) -> Result<()> {
        let path = paths::subscriptions_file()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let s = serde_json::to_string_pretty(subs).context("serializing subscriptions")?;
        std::fs::write(&path, s).context("writing subscriptions")?;
        Ok(())
    }
}

/// Return the per-install HWID, generating one if absent. Only *used* when a
/// subscription opts in — generation here does not transmit anything.
pub fn hwid() -> Result<String> {
    let path = paths::hwid_file()?;
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let id = generate_hwid();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, &id).context("writing hwid")?;
    Ok(id)
}

fn generate_hwid() -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    std::process::id().hash(&mut h);
    format!("{:016x}{:016x}", h.finish(), std::process::id() as u64)
}
