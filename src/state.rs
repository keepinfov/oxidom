use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{fsutil, paths};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Last active server id.
    pub active_server_id: Option<String>,
    /// Remembered per-app route choices (desktop-id/binary -> server id).
    pub app_routes: HashMap<String, String>,
    /// PID of the xray child of the last run, so a crash never leaves an
    /// orphaned tunnel: the next start kills it if it is still an xray process.
    pub xray_pid: Option<u32>,
}

impl State {
    pub fn load() -> State {
        let Ok(path) = paths::state_file() else {
            return State::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str(&s) {
                Ok(state) => state,
                Err(error) => {
                    let moved = fsutil::quarantine(&path);
                    log::warn!(
                        "state.toml is not valid ({error}); moved aside to {:?}",
                        moved
                    );
                    State::default()
                }
            },
            Err(_) => State::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::state_file()?;
        let s = toml::to_string_pretty(self).context("serializing state")?;
        fsutil::write_private_atomic(&path, s.as_bytes()).context("writing state")?;
        Ok(())
    }
}

/// Storage for the cached subscriptions list (subscriptions.json).
pub mod store {
    use anyhow::{Context, Result};

    use crate::model::Subscription;
    use crate::{fsutil, paths};

    /// Load the cached subscriptions. On a corrupt file the data is moved
    /// aside instead of being silently replaced; the returned warning is
    /// surfaced to the user by the GUI.
    pub fn load() -> (Vec<Subscription>, Option<String>) {
        let Ok(path) = paths::subscriptions_file() else {
            return (Vec::new(), None);
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(subs) => (subs, None),
                Err(error) => {
                    let moved = fsutil::quarantine(&path);
                    log::warn!("subscriptions.json is not valid ({error}); moved to {moved:?}");
                    let warning = match moved {
                        Some(moved) => format!(
                            "Saved subscriptions could not be read and were moved to {}",
                            moved.display()
                        ),
                        None => "Saved subscriptions could not be read".to_string(),
                    };
                    (Vec::new(), Some(warning))
                }
            },
            Err(_) => (Vec::new(), None),
        }
    }

    pub fn save(subs: &[Subscription]) -> Result<()> {
        let path = paths::subscriptions_file()?;
        let s = serde_json::to_string_pretty(subs).context("serializing subscriptions")?;
        fsutil::write_private_atomic(&path, s.as_bytes()).context("writing subscriptions")?;
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
    fsutil::write_private_atomic(&path, id.as_bytes()).context("writing hwid")?;
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
