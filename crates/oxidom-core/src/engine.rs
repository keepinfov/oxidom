//! App-facing orchestration API. The GUI (Phase 2) drives everything through
//! this type; it should not call the lower-level modules directly.

use anyhow::{Result, anyhow};

use crate::config::Config;
use crate::model::{Server, Subscription};
use crate::state::{self, State, store};
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
    /// Non-fatal problems found while loading (e.g. a quarantined corrupt
    /// subscriptions file). The GUI surfaces these once at startup.
    pub load_warnings: Vec<String>,
}

impl Engine {
    pub fn load() -> Self {
        let config = Config::load();
        let core = XrayCore::new(
            config.socks_port,
            config.http_port,
            config.xray_binary.clone(),
        );
        let state = State::load();
        let (subscriptions, store_warning) = store::load();
        let mut engine = Engine {
            state,
            subscriptions,
            core,
            config,
            load_warnings: store_warning.into_iter().collect(),
        };
        engine.recover();
        engine
    }

    /// Undo each resource a crashed previous instance could have left behind.
    fn recover(&mut self) {
        self.recover_stale_core();
        // Phase 4 adds the TUN device and the routes we added here.
    }

    fn recover_stale_core(&mut self) {
        if let Some(pid) = self.state.xray_pid.take() {
            if kill_stale_xray(pid) {
                log::info!("stopped orphaned xray process {pid} from a previous run");
            } else {
                log::warn!("could not confirm that orphaned xray process {pid} stopped");
            }
            if let Err(error) = self.state.save() {
                log::warn!("could not persist stale-core recovery state: {error:#}");
            }
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
        let ua = self.config.subscription_user_agent.clone();
        let sub = self
            .subscriptions
            .iter_mut()
            .find(|s| s.id == sub_id)
            .ok_or_else(|| anyhow!("subscription not found"))?;
        // Generate the device id only for a subscription that opted in: the
        // file is itself a per-install identifier, so an opt-out user must not
        // end up with one sitting on disk.
        let hwid = if sub.send_hwid {
            state::hwid().ok()
        } else {
            None
        };
        subscription::refresh(sub, &ua, hwid.as_deref())?;
        self.disconnect_if_active_gone();
        store::save(&self.subscriptions)?;
        Ok(())
    }

    /// Refresh every URL-backed subscription (skips the local share-link group,
    /// which has an empty URL). Collects per-subscription errors and still saves
    /// whatever succeeded; returns an error summarizing any failures.
    pub fn refresh_all(&mut self) -> Result<()> {
        // Only touch the hwid file when something actually opted in; reading it
        // creates it. See the note in `refresh`.
        let hwid_val = self
            .subscriptions
            .iter()
            .any(|s| s.send_hwid && !s.url.is_empty())
            .then(|| state::hwid().ok())
            .flatten();
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
                    errors.push(format!("{}: {error:#}", sub.name));
                }
            }
        }
        self.disconnect_if_active_gone();
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
                    "none of the links use a supported scheme ({})",
                    link::supported_scheme_list()
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

    /// Disconnect when a refresh took the active server away with it — the
    /// panel rotated its credentials, renumbered it, or dropped it entirely.
    /// Same invariant as a deletion: the tunnel must not keep running through
    /// a server the user can no longer see, select or manage.
    fn disconnect_if_active_gone(&mut self) -> bool {
        let gone = self.disconnect_if_active_within(|active_id, subs| {
            !subs
                .iter()
                .any(|s| s.servers.iter().any(|server| server.id == active_id))
        });
        if gone {
            self.core
                .note("the active server is no longer in its subscription — disconnected");
        }
        gone
    }

    /// Disconnect when the tunnel is running and the active server matches
    /// `covers(active_id, &subscriptions)`. Never leave xray proxying through
    /// a server the user just deleted.
    fn disconnect_if_active_within(
        &mut self,
        covers: impl Fn(&str, &[Subscription]) -> bool,
    ) -> bool {
        // `Error` counts as active: a crashed core still leaves the server
        // recorded as the active one, and deleting it must clear that.
        let active = match (&self.state.active_server_id, self.core.status()) {
            (Some(id), Status::Connected | Status::Connecting | Status::Error(_)) => id.clone(),
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
        if let Err(error) = self.state.save() {
            log::warn!("could not persist the active Xray process: {error:#}");
        }
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.core.disconnect();
        self.state.active_server_id = None;
        self.state.xray_pid = None;
        if let Err(error) = self.state.save() {
            log::warn!("could not persist the disconnected state: {error:#}");
        }
    }

    pub fn status(&self) -> Status {
        self.core.status()
    }

    /// Probe one server with the configured latency method, measured against
    /// that server rather than through the tunnel.
    pub fn probe(&self, server: &Server) -> probe::ProbeOutcome {
        probe::measure(server, &self.config, probe::Route::Direct)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Stop the child before the struct fields drop so the recovery flag
        // can be persisted as clean in the same pass.
        self.core.disconnect();
        if self.state.xray_pid.is_some() {
            self.state.xray_pid = None;
            if let Err(error) = self.state.save() {
                log::warn!("could not clear the Xray recovery PID on shutdown: {error:#}");
            }
        }
    }
}

/// Kill a leftover xray process from a previous run, but only after verifying
/// the PID still belongs to our core — PIDs get recycled.
fn kill_stale_xray(pid: u32) -> bool {
    if !is_our_xray(pid) {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        log::warn!("refusing to signal stale PID {pid}: it is not an oxidom Xray core");
        return false;
    }
    let Ok(raw_pid) = i32::try_from(pid) else {
        log::warn!("stale Xray PID {pid} is outside the platform PID range");
        return false;
    };
    let process = nix::unistd::Pid::from_raw(raw_pid);
    match nix::sys::signal::kill(process, nix::sys::signal::Signal::SIGTERM) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => return true,
        Err(error) => {
            log::warn!("could not send SIGTERM to stale Xray PID {pid}: {error}");
            return false;
        }
    }
    if wait_until_gone(process, std::time::Duration::from_secs(2)) {
        return true;
    }

    log::warn!("stale Xray PID {pid} ignored SIGTERM; sending SIGKILL");
    match nix::sys::signal::kill(process, nix::sys::signal::Signal::SIGKILL) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => return true,
        Err(error) => {
            log::warn!("could not send SIGKILL to stale Xray PID {pid}: {error}");
            return false;
        }
    }
    wait_until_gone(process, std::time::Duration::from_secs(2))
}

fn wait_until_gone(pid: nix::unistd::Pid, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match nix::sys::signal::kill(pid, None) {
            Err(nix::errno::Errno::ESRCH) => return true,
            Err(error) => {
                log::warn!("could not inspect stale Xray PID {pid}: {error}");
                return false;
            }
            Ok(()) if std::time::Instant::now() >= deadline => return false,
            Ok(()) => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
}

/// Does this PID belong to a core oxidom started?
///
/// The binary name is user-configurable (`xray_binary`, `$OXIDOM_XRAY_BIN`), so
/// insisting on `comm == "xray"` would skip a core installed as, say,
/// `xray-linux-amd64` and leave its tunnel up with no way to stop it. The
/// generated config path is the reliable marker: nothing else is run against
/// it. The name check stays as a fallback for when the data dir has moved.
fn is_our_xray(pid: u32) -> bool {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let cmdline = String::from_utf8_lossy(&raw);
    if let Ok(config) = XrayCore::config_path()
        && cmdline
            .split('\0')
            .any(|arg| !arg.is_empty() && std::path::Path::new(arg) == config)
    {
        return true;
    }
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|comm| comm.trim().starts_with("xray"))
}
