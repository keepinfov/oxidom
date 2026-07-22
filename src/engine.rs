//! App-facing orchestration API. The GUI (Phase 2) drives everything through
//! this type; it should not call the lower-level modules directly.

use anyhow::{anyhow, Result};

use crate::config::Config;
use crate::model::{Server, Subscription};
use crate::state::{self, store, State};
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
}

impl Engine {
    pub fn load() -> Self {
        let config = Config::load();
        let core = XrayCore::new(config.socks_port, config.http_port);
        Engine {
            state: State::load(),
            subscriptions: store::load(),
            core,
            config,
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

    pub fn add_subscription(&mut self, url: String, name: Option<String>) -> Result<()> {
        let mut sub = Subscription::new(url, name);
        let hwid = if sub.send_hwid { state::hwid().ok() } else { None };
        subscription::refresh(&mut sub, &self.config.subscription_user_agent, hwid.as_deref())?;
        self.subscriptions.push(sub);
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
        let hwid = if sub.send_hwid { hwid_val.as_deref() } else { None };
        subscription::refresh(sub, &ua, hwid)?;
        store::save(&self.subscriptions)?;
        Ok(())
    }

    pub fn remove_subscription(&mut self, sub_id: &str) -> Result<()> {
        self.subscriptions.retain(|s| s.id != sub_id);
        store::save(&self.subscriptions)
    }

    /// Import one or more share-links into the local "My servers" group.
    /// Returns how many new servers were added (duplicates are skipped).
    pub fn import_links(&mut self, text: &str) -> Result<usize> {
        let parsed = link::parse_links(text);
        if parsed.is_empty() {
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
            if !self.subscriptions[idx].servers.iter().any(|s| s.id == server.id) {
                self.subscriptions[idx].servers.push(server);
                added += 1;
            }
        }
        store::save(&self.subscriptions)?;
        Ok(added)
    }

    /// Remove a single server from the local group, dropping the group when it
    /// becomes empty. Only local servers are removable; subscription servers
    /// would just reappear on refresh.
    pub fn remove_server(&mut self, server_id: &str) -> Result<()> {
        if let Some(sub) = self.subscriptions.iter_mut().find(|s| s.id == LOCAL_ID) {
            sub.servers.retain(|s| s.id != server_id);
        }
        self.subscriptions
            .retain(|s| !(s.id == LOCAL_ID && s.servers.is_empty()));
        store::save(&self.subscriptions)
    }

    pub fn connect(&mut self, server_id: &str) -> Result<()> {
        let server = self
            .find_server(server_id)
            .ok_or_else(|| anyhow!("server not found"))?;
        self.core.connect(&server)?;
        self.state.active_server_id = Some(server_id.to_string());
        self.state.save().ok();
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.core.disconnect();
        self.state.active_server_id = None;
        self.state.save().ok();
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
