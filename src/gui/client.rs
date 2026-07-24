//! Blocking D-Bus client for the oxidom daemon. All calls can block for the
//! duration of a daemon operation — never call these on the GTK main thread.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::ipc::{ApplySettingsResult, BUS_NAME, INTERFACE, OBJECT_PATH, ProbeState, StatusInfo};
use crate::model::Subscription;

#[derive(Clone)]
pub struct DaemonClient {
    proxy: zbus::blocking::Proxy<'static>,
}

fn friendly(error: zbus::Error) -> anyhow::Error {
    match error {
        zbus::Error::MethodError(_, Some(message), _) => anyhow::anyhow!(message),
        other => anyhow::anyhow!(other),
    }
}

impl DaemonClient {
    fn try_bus(system: bool) -> Result<Self> {
        let connection = if system {
            zbus::blocking::Connection::system()?
        } else {
            zbus::blocking::Connection::session()?
        };
        let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)?;
        // Reject name owners that don't answer: a real daemon replies fast.
        let _: String = proxy.call("Status", &())?;
        Ok(DaemonClient { proxy })
    }

    /// System bus first (the systemd service), then the session bus; as a
    /// last resort spawn a session daemon so the GUI works standalone.
    pub fn connect_any() -> Result<Self> {
        if let Ok(client) = Self::try_bus(true) {
            return Ok(client);
        }
        if let Ok(client) = Self::try_bus(false) {
            return Ok(client);
        }
        let exe = std::env::current_exe().context("locating the oxidom binary")?;
        std::process::Command::new(exe)
            .arg("daemon")
            .spawn()
            .context("spawning a session daemon")?;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(150));
            if let Ok(client) = Self::try_bus(false) {
                return Ok(client);
            }
        }
        bail!("could not reach or start the oxidom daemon")
    }

    pub fn subscriptions(&self) -> Result<Vec<Subscription>> {
        let json: String = self
            .proxy
            .call("ListSubscriptions", &())
            .map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn add_subscription(&self, url: &str, name: Option<&str>, send_hwid: bool) -> Result<()> {
        self.proxy
            .call("AddSubscription", &(url, name.unwrap_or(""), send_hwid))
            .map_err(friendly)
    }

    pub fn remove_subscription(&self, subscription_id: &str) -> Result<bool> {
        self.proxy
            .call("RemoveSubscription", &(subscription_id,))
            .map_err(friendly)
    }

    pub fn refresh(&self, subscription_id: &str) -> Result<()> {
        self.proxy
            .call("Refresh", &(subscription_id,))
            .map_err(friendly)
    }

    pub fn refresh_all(&self) -> Result<()> {
        self.proxy.call("RefreshAll", &()).map_err(friendly)
    }

    pub fn import_links(&self, text: &str) -> Result<(u32, u32)> {
        self.proxy.call("ImportLinks", &(text,)).map_err(friendly)
    }

    pub fn remove_server(&self, server_id: &str) -> Result<bool> {
        self.proxy
            .call("RemoveServer", &(server_id,))
            .map_err(friendly)
    }

    pub fn set_hwid(&self, subscription_id: &str, enabled: bool) -> Result<()> {
        self.proxy
            .call("SetHwid", &(subscription_id, enabled))
            .map_err(friendly)
    }

    pub fn connect_server(&self, server_id: &str) -> Result<()> {
        self.proxy.call("Connect", &(server_id,)).map_err(friendly)
    }

    pub fn disconnect(&self) -> Result<()> {
        self.proxy.call("Disconnect", &()).map_err(friendly)
    }

    pub fn status(&self) -> Result<StatusInfo> {
        let json: String = self.proxy.call("Status", &()).map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn request_probe(&self, server_id: &str) -> Result<()> {
        self.proxy
            .call("RequestProbe", &(server_id,))
            .map_err(friendly)
    }

    pub fn request_probes(&self, server_ids: &[String]) -> Result<()> {
        self.proxy
            .call("RequestProbes", &(server_ids,))
            .map_err(friendly)
    }

    pub fn probe_state(&self) -> Result<ProbeState> {
        let json: String = self.proxy.call("ProbeState", &()).map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn settings(&self) -> Result<Config> {
        let json: String = self.proxy.call("GetSettings", &()).map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn apply_settings(&self, config: &Config) -> Result<ApplySettingsResult> {
        let payload = serde_json::to_string(config)?;
        let json: String = self
            .proxy
            .call("SetSettings", &(payload,))
            .map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn recent_logs(&self) -> Result<Vec<String>> {
        self.proxy.call("RecentLogs", &()).map_err(friendly)
    }

    pub fn clear_logs(&self) -> Result<()> {
        self.proxy.call("ClearLogs", &()).map_err(friendly)
    }
}
