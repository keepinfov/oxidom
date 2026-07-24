//! App-facing orchestration API. The GUI (Phase 2) drives everything through
//! this type; it should not call the lower-level modules directly.

use anyhow::{Result, anyhow};

use crate::config::Config;
use crate::model::{Server, Subscription};
use crate::state::{self, State, store};
use crate::sysproxy;
use crate::xray::core::{Status, XrayCore};
use crate::{link, probe, subscription};

/// Fixed id of the local group that holds servers imported by share-link,
/// not tied to any subscription URL.
pub const LOCAL_ID: &str = "local";

pub struct Engine {
    pub config: Config,
    pub state: State,
    pub subscriptions: Vec<Subscription>,
    pub core: XrayCore,
    /// True while we currently hold the desktop system proxy set to our ports.
    proxy_applied: bool,
    /// Non-fatal problems found while loading (e.g. a quarantined corrupt
    /// subscriptions file). The GUI surfaces these once at startup.
    pub load_warnings: Vec<String>,
}

impl Engine {
    pub fn load() -> Self {
        let config = Config::load();
        let core = XrayCore::new(config.socks_port, config.http_port);
        let state = State::load();
        let (subscriptions, store_warning) = store::load();
        let mut engine = Engine {
            state,
            subscriptions,
            core,
            config,
            proxy_applied: false,
            load_warnings: store_warning.into_iter().collect(),
        };
        engine.recover_from_unclean_shutdown();
        engine
    }

    /// Undo whatever a crashed previous instance left behind: an orphaned xray
    /// child keeping the tunnel open, and a desktop proxy pointing at it.
    fn recover_from_unclean_shutdown(&mut self) {
        let mut dirty = false;
        if let Some(pid) = self.state.xray_pid.take() {
            dirty = true;
            if kill_stale_xray(pid) {
                log::info!("stopped orphaned xray process {pid} from a previous run");
            }
        }
        if self.state.system_proxy_applied {
            self.state.system_proxy_applied = false;
            dirty = true;
            if sysproxy::clear().is_ok() {
                log::info!("restored the desktop proxy left enabled by a previous run");
            }
        }
        if dirty {
            self.state.save().ok();
        }
    }

    pub fn save(&self) -> Result<()> {
        self.config.save()?;
        self.state.save()?;
        store::save(&self.subscriptions)?;
        Ok(())
    }

    /// Flat iterator over every known server across all subscriptions.
    pub fn all_servers(&self) -> impl Iterator<Item = &Server> {
        self.subscriptions.iter().flat_map(|s| s.servers.iter())
    }

    pub fn find_server(&self, id: &str) -> Option<Server> {
        self.all_servers().find(|s| s.id == id).cloned()
    }

    pub fn add_subscription(
        &mut self,
        url: String,
        name: Option<String>,
        send_hwid: bool,
    ) -> Result<()> {
        let mut sub = Subscription::new(url, name);
        sub.send_hwid = send_hwid;
        let hwid = if sub.send_hwid {
            state::hwid().ok()
        } else {
            None
        };
        subscription::refresh(
            &mut sub,
            &self.config.subscription_user_agent,
            hwid.as_deref(),
        )?;
        if let Some(existing) = self
            .subscriptions
            .iter_mut()
            .find(|existing| existing.id == sub.id)
        {
            *existing = sub;
        } else {
            self.subscriptions.push(sub);
        }
        store::save(&self.subscriptions)?;
        Ok(())
    }

    pub fn refresh(&mut self, sub_id: &str) -> Result<()> {
        let hwid_val = state::hwid().ok();
        let ua = self.config.subscription_user_agent.clone();
        let sub = self
            .subscriptions
            .iter_mut()
            .find(|s| s.id == sub_id)
            .ok_or_else(|| anyhow!("subscription not found"))?;
        let hwid = if sub.send_hwid {
            hwid_val.as_deref()
        } else {
            None
        };
        subscription::refresh(sub, &ua, hwid)?;
        store::save(&self.subscriptions)?;
        Ok(())
    }

    /// Refresh every URL-backed subscription (skips the local share-link group,
    /// which has an empty URL). Collects per-subscription errors and still saves
    /// whatever succeeded; returns an error summarizing any failures.
    pub fn refresh_all(&mut self) -> Result<()> {
        let hwid_val = state::hwid().ok();
        let ua = self.config.subscription_user_agent.clone();
        let ids: Vec<String> = self
            .subscriptions
            .iter()
            .filter(|s| !s.url.is_empty())
            .map(|s| s.id.clone())
            .collect();
        let mut errors = Vec::new();
        for id in ids {
            if let Some(sub) = self.subscriptions.iter_mut().find(|s| s.id == id) {
                let hwid = if sub.send_hwid {
                    hwid_val.as_deref()
                } else {
                    None
                };
                if let Err(error) = subscription::refresh(sub, &ua, hwid) {
                    errors.push(format!("{}: {error}", sub.name));
                }
            }
        }
        store::save(&self.subscriptions)?;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(errors.join("; ")))
        }
    }

    /// Remove a subscription. Returns true when the removal took the active
    /// server with it and the tunnel was therefore shut down.
    pub fn remove_subscription(&mut self, sub_id: &str) -> Result<bool> {
        let disconnected = self.disconnect_if_active_within(|server_id, subs| {
            subs.iter()
                .find(|s| s.id == sub_id)
                .is_some_and(|s| s.servers.iter().any(|server| server.id == server_id))
        });
        self.subscriptions.retain(|s| s.id != sub_id);
        store::save(&self.subscriptions)?;
        Ok(disconnected)
    }

    /// Import one or more share-links into the local "My servers" group.
    /// Returns how many new servers were added (duplicates are skipped) and
    /// how many lines used an unsupported scheme.
    pub fn import_links(&mut self, text: &str) -> Result<(usize, usize)> {
        let (parsed, unsupported) = link::parse_links_counting(text);
        if parsed.is_empty() {
            if unsupported > 0 {
                return Err(anyhow!(
                    "none of the links use a supported scheme (vless, vmess, trojan, ss, socks, http)"
                ));
            }
            return Err(anyhow!("no valid share-links found"));
        }
        let idx = match self.subscriptions.iter().position(|s| s.id == LOCAL_ID) {
            Some(idx) => idx,
            None => {
                let mut sub = Subscription::new(String::new(), Some("My servers".to_string()));
                sub.id = LOCAL_ID.to_string();
                self.subscriptions.insert(0, sub);
                0
            }
        };
        let mut added = 0;
        for server in parsed {
            if !self.subscriptions[idx]
                .servers
                .iter()
                .any(|s| s.id == server.id)
            {
                self.subscriptions[idx].servers.push(server);
                added += 1;
            }
        }
        store::save(&self.subscriptions)?;
        Ok((added, unsupported))
    }

    /// Remove a single server from the local group, dropping the group when it
    /// becomes empty. Only local servers are removable; subscription servers
    /// would just reappear on refresh. Returns true when the removed server
    /// was the active one and the tunnel was shut down.
    pub fn remove_server(&mut self, server_id: &str) -> Result<bool> {
        let disconnected = self.disconnect_if_active_within(|active_id, _| active_id == server_id);
        if let Some(sub) = self.subscriptions.iter_mut().find(|s| s.id == LOCAL_ID) {
            sub.servers.retain(|s| s.id != server_id);
        }
        self.subscriptions
            .retain(|s| !(s.id == LOCAL_ID && s.servers.is_empty()));
        store::save(&self.subscriptions)?;
        Ok(disconnected)
    }

    /// Disconnect when the tunnel is running and the active server matches
    /// `covers(active_id, &subscriptions)`. Never leave xray proxying through
    /// a server the user just deleted.
    fn disconnect_if_active_within(
        &mut self,
        covers: impl Fn(&str, &[Subscription]) -> bool,
    ) -> bool {
        let active = match (&self.state.active_server_id, self.core.status()) {
            (Some(id), Status::Connected | Status::Connecting) => id.clone(),
            _ => return false,
        };
        if covers(&active, &self.subscriptions) {
            self.disconnect();
            true
        } else {
            false
        }
    }

    pub fn connect(&mut self, server_id: &str) -> Result<()> {
        let server = self
            .find_server(server_id)
            .ok_or_else(|| anyhow!("server not found"))?;
        self.core.connect(&server)?;
        self.state.active_server_id = Some(server_id.to_string());
        self.state.xray_pid = self.core.child_pid();
        self.state.save().ok();
        self.reconcile_system_proxy();
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.core.disconnect();
        self.state.active_server_id = None;
        self.state.xray_pid = None;
        self.state.save().ok();
        self.reconcile_system_proxy();
    }

    /// Make the desktop system proxy match desired state: on only when the user
    /// enabled it AND the tunnel is connected. Best-effort; failures (e.g. a
    /// non-GNOME session) are ignored so they never block connect/disconnect.
    /// The applied flag is persisted so a crash can be repaired on restart.
    pub fn reconcile_system_proxy(&mut self) {
        let want = self.config.system_proxy && self.core.status() == Status::Connected;
        if want && !self.proxy_applied {
            if sysproxy::apply(self.config.socks_port, self.config.http_port).is_ok() {
                self.proxy_applied = true;
                self.state.system_proxy_applied = true;
                self.state.save().ok();
            }
        } else if !want && self.proxy_applied {
            let _ = sysproxy::clear();
            self.proxy_applied = false;
            self.state.system_proxy_applied = false;
            self.state.save().ok();
        }
    }

    pub fn status(&self) -> Status {
        self.core.status()
    }

    /// Probe one server with the configured latency method.
    pub fn probe(&self, server: &Server) -> Option<u32> {
        probe::measure(
            server,
            self.config.latency_method,
            self.config.socks_port,
            &self.config.latency_test_url,
        )
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Stop the child before the struct fields drop so the recovery flags
        // can be persisted as clean in the same pass.
        self.core.disconnect();
        if self.proxy_applied {
            let _ = sysproxy::clear();
            self.proxy_applied = false;
        }
        if self.state.xray_pid.is_some() || self.state.system_proxy_applied {
            self.state.xray_pid = None;
            self.state.system_proxy_applied = false;
            self.state.save().ok();
        }
    }
}

/// Kill a leftover xray process from a previous run, but only after verifying
/// the PID still belongs to an xray binary — PIDs get recycled.
fn kill_stale_xray(pid: u32) -> bool {
    let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
        return false;
    };
    if comm.trim() != "xray" {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .is_ok()
}
