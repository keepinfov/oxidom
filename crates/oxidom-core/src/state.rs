use std::net::Ipv4Addr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::{bind, fsutil, paths};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub sessions: Vec<SessionState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    pub profile: String,
    pub server_id: Option<String>,
    pub address: Ipv4Addr,
    pub socks_port: u16,
    pub http_port: u16,
    /// PID of the xray child of the last run, so a crash never leaves an
    /// orphaned tunnel: the next start kills it if it is still an xray process.
    pub xray_pid: Option<u32>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            profile: String::new(),
            server_id: None,
            address: Ipv4Addr::UNSPECIFIED,
            socks_port: 0,
            http_port: 0,
            xray_pid: None,
        }
    }
}

/// The legacy fields are accepted only here. Keeping them out of `State`
/// ensures the next real write cannot recreate the retired on-disk shape.
#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredState {
    sessions: Vec<SessionState>,
    active_server_id: Option<String>,
    active_profile: Option<String>,
    xray_pid: Option<u32>,
}

impl State {
    pub fn load(config: &Config) -> State {
        let Ok(path) = paths::state_file() else {
            return State::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<StoredState>(&s) {
                Ok(stored) => migrate(stored, config),
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

fn migrate(stored: StoredState, config: &Config) -> State {
    if !stored.sessions.is_empty() {
        return State {
            sessions: stored.sessions,
        };
    }
    if stored.active_server_id.is_none()
        && stored.active_profile.is_none()
        && stored.xray_pid.is_none()
    {
        return State::default();
    }

    let profile = stored
        .active_profile
        .unwrap_or_else(|| "default".to_string());
    let Some(address) = bind::address_for(&profile, &[]) else {
        log::warn!("could not allocate a loopback address while migrating state for {profile:?}");
        return State::default();
    };
    let state = State {
        sessions: vec![SessionState {
            profile,
            server_id: stored.active_server_id,
            address,
            socks_port: config.socks_port,
            http_port: config.http_port,
            xray_pid: stored.xray_pid,
        }],
    };
    if let Err(error) = state.save() {
        log::warn!("could not persist the migrated session state: {error:#}");
    }
    state
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    use anyhow::Result;

    use super::*;

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TestRoot {
        path: std::path::PathBuf,
    }

    impl TestRoot {
        fn install(label: &str) -> Self {
            let suffix = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "oxidom-state-test-{label}-{}-{suffix}",
                std::process::id()
            ));
            crate::paths::set_test_root(Some(path.clone()));
            Self { path }
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            crate::paths::set_test_root(None);
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn legacy_state_becomes_one_session_without_losing_the_server() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("legacy-migration");
        let path = crate::paths::state_file()?;
        crate::fsutil::write_private_atomic(
            &path,
            b"active_server_id = \"server-id\"\nactive_profile = \"work\"\nxray_pid = 42\n",
        )?;
        let config = Config {
            socks_port: 21080,
            http_port: 21081,
            ..Config::default()
        };

        let state = State::load(&config);

        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].profile, "work");
        assert_eq!(state.sessions[0].server_id.as_deref(), Some("server-id"));
        assert_eq!(state.sessions[0].socks_port, 21080);
        assert_eq!(state.sessions[0].http_port, 21081);
        assert_eq!(state.sessions[0].xray_pid, Some(42));
        assert_eq!(
            state.sessions[0].address,
            bind::address_for("work", &[]).unwrap()
        );

        let migrated = std::fs::read_to_string(path)?;
        assert!(migrated.contains("[[sessions]]"));
        assert!(!migrated.contains("active_server_id"));
        assert!(!migrated.contains("active_profile"));
        Ok(())
    }

    #[test]
    fn legacy_state_without_a_profile_becomes_default() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("legacy-default");
        let path = crate::paths::state_file()?;
        crate::fsutil::write_private_atomic(&path, b"active_server_id = \"server-id\"\n")?;

        let state = State::load(&Config::default());

        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].profile, "default");
        assert_eq!(state.sessions[0].address, Ipv4Addr::LOCALHOST);
        assert_eq!(state.sessions[0].server_id.as_deref(), Some("server-id"));
        Ok(())
    }

    #[test]
    fn current_state_is_not_rewritten_while_loading() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("no-rewrite");
        let state = State {
            sessions: vec![SessionState {
                profile: "default".to_string(),
                server_id: Some("server-id".to_string()),
                address: Ipv4Addr::LOCALHOST,
                socks_port: 10808,
                http_port: 10809,
                xray_pid: None,
            }],
        };
        state.save()?;
        let path = crate::paths::state_file()?;
        let mixed = format!(
            "active_server_id = \"legacy-id\"\nactive_profile = \"legacy\"\nxray_pid = 9\n\n{}",
            std::fs::read_to_string(&path)?
        );
        crate::fsutil::write_private_atomic(&path, mixed.as_bytes())?;
        let inode = std::fs::metadata(&path)?.ino();

        let loaded = State::load(&Config::default());

        assert_eq!(loaded.sessions, state.sessions);
        assert_eq!(std::fs::metadata(path)?.ino(), inode);
        Ok(())
    }
}
