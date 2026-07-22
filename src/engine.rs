//! App-facing orchestration API. The GUI (Phase 2) drives everything through
//! this type; it should not call the lower-level modules directly.

use anyhow::{anyhow, Result};

use crate::config::Config;
use crate::model::{Server, Subscription};
use crate::state::{self, store, State};
use crate::xray::core::{Status, XrayCore};
use crate::{probe, subscription};

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
        subscription::refresh(&mut sub, hwid.as_deref())?;
        self.subscriptions.push(sub);
        store::save(&self.subscriptions)?;
        Ok(())
    }

    pub fn refresh(&mut self, sub_id: &str) -> Result<()> {
        let hwid_val = state::hwid().ok();
        let sub = self
            .subscriptions
            .iter_mut()
            .find(|s| s.id == sub_id)
            .ok_or_else(|| anyhow!("subscription not found"))?;
        let hwid = if sub.send_hwid { hwid_val.as_deref() } else { None };
        subscription::refresh(sub, hwid)?;
        store::save(&self.subscriptions)?;
        Ok(())
    }

    pub fn remove_subscription(&mut self, sub_id: &str) -> Result<()> {
        self.subscriptions.retain(|s| s.id != sub_id);
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
