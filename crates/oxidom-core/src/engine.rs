//! Daemon-owned registry and per-profile runtime sessions.

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, SocketAddrV4};

use anyhow::{Result, anyhow};

use crate::config::Config;
use crate::model::{Server, Subscription};
use crate::state::{self, SessionState, State, store};
use crate::xray::core::{Status, XrayCore};
use crate::{alias, link, probe, subscription};

/// Fixed id of the local group that holds servers imported by share-link,
/// not tied to any subscription URL. It is a sentinel rather than a hash, so
/// the identity migration must leave it alone.
pub const LOCAL_ID: &str = "local";

/// Persistent configuration and server catalog shared by every session.
pub struct Registry {
    pub config: Config,
    pub subscriptions: Vec<Subscription>,
    /// Non-fatal problems found while loading (e.g. a quarantined corrupt
    /// subscriptions file). The GUI surfaces these once at startup.
    pub load_warnings: Vec<String>,
}

impl Registry {
    pub fn load() -> Self {
        let config = Config::load();
        let (subscriptions, store_warning) = store::load();
        Registry {
            subscriptions,
            config,
            load_warnings: store_warning.into_iter().collect(),
        }
    }

    pub fn migrate_identities(&mut self, state: &mut State) {
        let active_before: Vec<String> = state
            .sessions
            .iter()
            .filter_map(|session| session.server_id.clone())
            .collect();
        let mut server_ids = HashMap::new();
        let mut seen_ids: HashMap<String, (String, String)> = HashMap::new();
        let mut identities_changed = false;
        let mut renamed_servers = 0usize;

        for subscription in &mut self.subscriptions {
            // The local group is keyed by a sentinel, not by its (empty) URL.
            // Rehashing it would orphan every share-link the user imported.
            if subscription.id != LOCAL_ID {
                let new_subscription_id = Server::stable_id(&subscription.url);
                if subscription.id != new_subscription_id {
                    subscription.id = new_subscription_id;
                    identities_changed = true;
                }
            }

            let mut migrated = Vec::with_capacity(subscription.servers.len());
            for mut server in std::mem::take(&mut subscription.servers) {
                let old_id = server.id.clone();
                let new_id = Server::stable_id(&server.identity_string());
                if old_id != new_id {
                    server_ids
                        .entry(old_id.clone())
                        .or_insert_with(|| new_id.clone());
                    server.id.clone_from(&new_id);
                    identities_changed = true;
                    renamed_servers += 1;
                }

                if let Some((first_old_id, first_name)) = seen_ids.get(&new_id)
                    && first_old_id != &old_id
                {
                    log::warn!(
                        "dropping server {:?}: its migrated id collides with earlier server {:?}",
                        server.name,
                        first_name
                    );
                    identities_changed = true;
                    continue;
                }
                seen_ids
                    .entry(new_id)
                    .or_insert_with(|| (old_id, server.name.clone()));
                migrated.push(server);
            }
            subscription.servers = migrated;
        }

        let aliases_complete = self
            .all_servers()
            .all(|server| server.alias.as_ref().is_some());
        if !identities_changed && aliases_complete {
            return;
        }

        for session in &mut state.sessions {
            if let Some(active_id) = session.server_id.as_mut()
                && let Some(new_id) = server_ids.get(active_id)
            {
                active_id.clone_from(new_id);
            }
        }
        alias::assign(&mut self.subscriptions);

        if let Err(error) = store::save(&self.subscriptions) {
            log::warn!("could not persist migrated subscription identities: {error:#}");
        }
        if let Err(error) = state.save() {
            log::warn!("could not persist migrated active server identity: {error:#}");
        }

        let active_after: Vec<&str> = state
            .sessions
            .iter()
            .filter_map(|session| session.server_id.as_deref())
            .collect();
        let active_preserved = active_after.len() == active_before.len()
            && active_after
                .iter()
                .all(|active_id| self.all_servers().any(|server| server.id == *active_id));
        log::info!(
            "identity migration renamed {renamed_servers} servers; active server preserved: \
             {active_preserved}"
        );
    }

    pub fn save(&self) -> Result<()> {
        self.config.save()?;
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
        alias::assign(&mut self.subscriptions);
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
        alias::assign(&mut self.subscriptions);
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
        alias::assign(&mut self.subscriptions);
        store::save(&self.subscriptions)?;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(errors.join("; ")))
        }
    }

    pub fn remove_subscription(&mut self, sub_id: &str) -> Result<()> {
        self.subscriptions.retain(|s| s.id != sub_id);
        store::save(&self.subscriptions)?;
        Ok(())
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
        alias::assign(&mut self.subscriptions);
        store::save(&self.subscriptions)?;
        Ok((added, unsupported))
    }

    /// Remove a single server from the local group, dropping the group when it
    /// becomes empty. Subscription servers would just reappear on refresh.
    pub fn remove_server(&mut self, server_id: &str) -> Result<()> {
        if let Some(sub) = self.subscriptions.iter_mut().find(|s| s.id == LOCAL_ID) {
            sub.servers.retain(|s| s.id != server_id);
        }
        self.subscriptions
            .retain(|s| !(s.id == LOCAL_ID && s.servers.is_empty()));
        store::save(&self.subscriptions)?;
        Ok(())
    }
}

/// One running profile and the Xray process that carries it.
pub struct Session {
    /// Name of the profile that started this session. This is the runtime key.
    pub profile: String,
    pub core: XrayCore,
    pub address: Ipv4Addr,
    pub socks_port: u16,
    pub http_port: u16,
    pub server_id: Option<String>,
}

impl Session {
    pub fn new(
        profile: String,
        address: Ipv4Addr,
        socks_port: u16,
        http_port: u16,
        xray_binary: String,
    ) -> Self {
        Self {
            profile,
            core: XrayCore::new(socks_port, http_port, xray_binary),
            address,
            socks_port,
            http_port,
            server_id: None,
        }
    }

    pub fn connect(&mut self, server: &Server) -> Result<()> {
        self.core.connect(server, self.address, &self.profile)?;
        self.server_id = Some(server.id.clone());
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.core.disconnect();
        self.server_id = None;
    }

    pub fn status(&self) -> Status {
        self.core.status()
    }

    pub fn is_alive(&mut self) -> bool {
        self.core.is_alive()
    }

    pub fn recent_logs(&self) -> Vec<String> {
        self.core.recent_logs()
    }

    pub fn clear_logs(&self) {
        self.core.clear_logs();
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.core.child_pid()
    }

    pub fn socks_endpoint(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.address, self.socks_port)
    }

    pub fn http_endpoint(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.address, self.http_port)
    }

    fn set_ports(&mut self, socks_port: u16, http_port: u16) {
        self.socks_port = socks_port;
        self.http_port = http_port;
        self.core.socks_port = socks_port;
        self.core.http_port = http_port;
    }

    fn state(&self) -> SessionState {
        SessionState {
            profile: self.profile.clone(),
            server_id: self.server_id.clone(),
            address: self.address,
            socks_port: self.socks_port,
            http_port: self.http_port,
            xray_pid: self.child_pid(),
        }
    }
}

#[derive(Default)]
pub struct Sessions {
    inner: BTreeMap<String, Session>,
    system_proxy_owner: Option<String>,
}

impl Sessions {
    pub fn get(&self, profile: &str) -> Option<&Session> {
        self.inner.get(profile)
    }

    pub fn get_mut(&mut self, profile: &str) -> Option<&mut Session> {
        self.inner.get_mut(profile)
    }

    pub fn insert(&mut self, session: Session) -> Option<Session> {
        self.inner.insert(session.profile.clone(), session)
    }

    pub fn remove(&mut self, profile: &str) -> Option<Session> {
        let removed = self.inner.remove(profile);
        if removed.is_some() && self.system_proxy_owner.as_deref() == Some(profile) {
            self.system_proxy_owner = None;
        }
        removed
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Session)> {
        self.inner
            .iter()
            .map(|(profile, session)| (profile.as_str(), session))
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn taken_addresses(&self) -> Vec<Ipv4Addr> {
        self.inner.values().map(|session| session.address).collect()
    }

    pub fn owner_of_system_proxy(&self) -> Option<&str> {
        self.system_proxy_owner.as_deref()
    }

    pub fn claim_system_proxy(&mut self, profile: &str) -> Result<()> {
        if let Some(owner) = self.owner_of_system_proxy()
            && owner != profile
        {
            return Err(anyhow!(
                "the system proxy is already held by profile {owner:?}"
            ));
        }
        if !self.inner.contains_key(profile) {
            return Err(anyhow!("profile {profile:?} has no session"));
        }
        self.system_proxy_owner = Some(profile.to_string());
        Ok(())
    }

    pub fn release_system_proxy(&mut self, profile: &str) {
        if self.owner_of_system_proxy() == Some(profile) {
            self.system_proxy_owner = None;
        }
    }

    fn from_state(state: &State, config: &Config) -> Self {
        let mut sessions = Sessions::default();
        for saved in &state.sessions {
            let mut session = Session::new(
                saved.profile.clone(),
                saved.address,
                saved.socks_port,
                saved.http_port,
                config.xray_binary.clone(),
            );
            session.server_id.clone_from(&saved.server_id);
            sessions.insert(session);
        }
        sessions
    }
}

/// Compatibility facade for the still-single-session daemon surface.
pub struct Engine {
    pub registry: Registry,
    pub sessions: Sessions,
    pub state: State,
}

impl Engine {
    pub fn load() -> Self {
        let mut registry = Registry::load();
        let mut state = State::load(&registry.config);
        registry.migrate_identities(&mut state);
        let sessions = Sessions::from_state(&state, &registry.config);
        let mut engine = Engine {
            registry,
            sessions,
            state,
        };
        engine.recover();
        engine
    }

    /// Undo each resource a crashed previous instance could have left behind.
    fn recover(&mut self) {
        self.recover_stale_cores();
        // Phase 4b adds the TUN device and the routes we added here.
    }

    fn recover_stale_cores(&mut self) {
        let stale_sessions = self
            .state
            .sessions
            .iter()
            .filter_map(|session| session.xray_pid.map(|pid| (session.profile.clone(), pid)))
            .collect::<Vec<_>>();
        if stale_sessions.is_empty() {
            return;
        }
        let mut cleaned_profiles = Vec::new();
        for (profile, pid) in &stale_sessions {
            if kill_stale_xray(*pid, profile) {
                log::info!(
                    "stopped orphaned xray process {pid} for profile {:?} from a previous run",
                    profile
                );
                cleaned_profiles.push(profile.clone());
            } else {
                log::warn!(
                    "could not confirm that orphaned xray process {pid} for profile {:?} stopped",
                    profile
                );
            }
        }
        if cleaned_profiles.is_empty() {
            return;
        }
        // A session is a running profile, not a remembered connection. Once
        // an inherited child has been reaped there is no runtime left to
        // restore, and keeping its entry would make the next `up` reject it as
        // a phantom session. A child we could not stop stays recorded so the
        // next daemon start can try again rather than orphaning it forever.
        for profile in &cleaned_profiles {
            self.sessions.remove(profile);
        }
        self.state
            .sessions
            .retain(|session| !cleaned_profiles.contains(&session.profile));
        if let Err(error) = self.state.save() {
            log::warn!("could not persist stale-core recovery state: {error:#}");
        }
    }

    pub fn save(&self) -> Result<()> {
        self.registry.save()?;
        self.state.save()?;
        Ok(())
    }

    pub fn all_servers(&self) -> impl Iterator<Item = &Server> {
        self.registry.all_servers()
    }

    pub fn find_server(&self, id: &str) -> Option<Server> {
        self.registry.find_server(id)
    }

    pub fn add_subscription(
        &mut self,
        url: String,
        name: Option<String>,
        send_hwid: bool,
    ) -> Result<()> {
        self.registry.add_subscription(url, name, send_hwid)
    }

    pub fn refresh(&mut self, sub_id: &str) -> Result<()> {
        self.registry.refresh(sub_id)?;
        self.disconnect_if_active_gone();
        Ok(())
    }

    pub fn refresh_all(&mut self) -> Result<()> {
        let result = self.registry.refresh_all();
        self.disconnect_if_active_gone();
        result
    }

    /// Remove a subscription, stopping every session whose server it held.
    pub fn remove_subscription(&mut self, sub_id: &str) -> Result<bool> {
        let disconnected = !self
            .disconnect_if_active_within(|server_id, subscriptions| {
                subscriptions
                    .iter()
                    .find(|subscription| subscription.id == sub_id)
                    .is_some_and(|subscription| {
                        subscription
                            .servers
                            .iter()
                            .any(|server| server.id == server_id)
                    })
            })
            .is_empty();
        self.registry.remove_subscription(sub_id)?;
        Ok(disconnected)
    }

    pub fn import_links(&mut self, text: &str) -> Result<(usize, usize)> {
        self.registry.import_links(text)
    }

    pub fn remove_server(&mut self, server_id: &str) -> Result<bool> {
        let disconnected = !self
            .disconnect_if_active_within(|active_id, _| active_id == server_id)
            .is_empty();
        self.registry.remove_server(server_id)?;
        Ok(disconnected)
    }

    pub fn default_session(&self) -> Option<&Session> {
        if let Some(session) = self.sessions.get("default") {
            return Some(session);
        }
        self.sessions.iter().next().map(|(_, session)| session)
    }

    pub fn default_session_mut(&mut self) -> Option<&mut Session> {
        let profile = self.default_profile()?.to_string();
        self.sessions.get_mut(&profile)
    }

    fn default_profile(&self) -> Option<&str> {
        if self.sessions.get("default").is_some() {
            Some("default")
        } else {
            self.sessions.iter().next().map(|(profile, _)| profile)
        }
    }

    /// Create a session alongside the profiles that are already running.
    ///
    /// Reusing an existing entry is reserved for `Connect`, whose historical
    /// contract is to replace whatever the `default` session is carrying.
    /// `UpProfile` rejects an existing entry before it reaches this method.
    pub fn prepare_session(
        &mut self,
        profile: &str,
        address: Ipv4Addr,
        socks_port: u16,
        http_port: u16,
    ) -> Result<()> {
        if !crate::profile::valid_name(profile) {
            return Err(anyhow!("invalid profile name {profile:?}"));
        }
        if let Some(session) = self.sessions.get_mut(profile) {
            session.address = address;
            session.set_ports(socks_port, http_port);
            return Ok(());
        }

        self.sessions.insert(Session::new(
            profile.to_string(),
            address,
            socks_port,
            http_port,
            self.registry.config.xray_binary.clone(),
        ));
        Ok(())
    }

    pub fn set_default_ports(&mut self, socks_port: u16, http_port: u16) {
        let Some(session) = self.sessions.get_mut("default") else {
            return;
        };
        session.set_ports(socks_port, http_port);
        self.sync_session("default");
    }

    pub fn active_server_id(&self) -> Option<String> {
        self.default_session()?.server_id.clone()
    }

    pub fn active_profile(&self) -> Option<String> {
        let session = self.default_session()?;
        session.server_id.as_ref().map(|_| session.profile.clone())
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
        if !gone.is_empty() {
            for profile in &gone {
                let Some(session) = self.sessions.get(profile) else {
                    continue;
                };
                session
                    .core
                    .note("the active server is no longer in its subscription — disconnected");
            }
        }
        !gone.is_empty()
    }

    /// Disconnect when the tunnel is running and the active server matches
    /// `covers(active_id, &subscriptions)`. Never leave xray proxying through
    /// a server the user just deleted.
    fn disconnect_if_active_within(
        &mut self,
        covers: impl Fn(&str, &[Subscription]) -> bool,
    ) -> Vec<String> {
        // `Error` counts as active: a crashed core still leaves the server
        // recorded as the active one, and deleting it must clear that.
        let affected: Vec<String> = self
            .sessions
            .iter()
            .filter_map(
                |(profile, session)| match (&session.server_id, session.status()) {
                    (Some(id), Status::Connected | Status::Connecting | Status::Error(_))
                        if covers(id, &self.registry.subscriptions) =>
                    {
                        Some(profile.to_string())
                    }
                    _ => None,
                },
            )
            .collect();
        for profile in &affected {
            if let Some(session) = self.sessions.get_mut(profile) {
                session.disconnect();
            }
            self.sessions.release_system_proxy(profile);
            self.sync_session(profile);
        }
        if !affected.is_empty()
            && let Err(error) = self.state.save()
        {
            log::warn!("could not persist the disconnected state: {error:#}");
        }
        affected
    }

    pub fn connect(&mut self, server_id: &str) -> Result<()> {
        self.connect_session("default", server_id)
    }

    pub fn connect_session(&mut self, profile: &str, server_id: &str) -> Result<()> {
        let server = self
            .find_server(server_id)
            .ok_or_else(|| anyhow!("server not found"))?;
        self.sessions
            .get_mut(profile)
            .ok_or_else(|| anyhow!("profile {profile:?} has no session"))?
            .connect(&server)?;
        self.sync_session(profile);
        if let Err(error) = self.state.save() {
            log::warn!("could not persist the active Xray process: {error:#}");
        }
        Ok(())
    }

    pub fn disconnect(&mut self) {
        if let Err(error) = self.remove_session("default") {
            log::warn!("could not persist the disconnected state: {error:#}");
        }
    }

    /// Stop a core but keep its session entry so a failed connection remains
    /// inspectable until the user explicitly brings the profile down.
    pub fn stop_session(&mut self, profile: &str) {
        if let Some(session) = self.sessions.get_mut(profile) {
            session.disconnect();
        }
        self.sessions.release_system_proxy(profile);
        self.sync_session(profile);
        if let Err(error) = self.state.save() {
            log::warn!("could not persist the disconnected state: {error:#}");
        }
    }

    /// Bring one profile down and forget its ephemeral runtime allocation.
    pub fn remove_session(&mut self, profile: &str) -> Result<bool> {
        let Some(mut session) = self.sessions.remove(profile) else {
            return Ok(false);
        };
        session.disconnect();
        self.state.sessions.retain(|saved| saved.profile != profile);
        self.state.save()?;
        Ok(true)
    }

    /// An unnamed `Down` means all sessions, not whichever one happens to be
    /// visible through the compatibility status fields.
    pub fn disconnect_all(&mut self) -> Result<bool> {
        let had_sessions = !self.sessions.is_empty();
        for session in self.sessions.inner.values_mut() {
            session.disconnect();
        }
        self.sessions.inner.clear();
        self.sessions.system_proxy_owner = None;
        self.state.sessions.clear();
        self.state.save()?;
        Ok(had_sessions)
    }

    pub fn status(&self) -> Status {
        self.default_session()
            .map(Session::status)
            .unwrap_or(Status::Disconnected)
    }

    /// Probe one server with the configured latency method, measured against
    /// that server rather than through the tunnel.
    pub fn probe(&self, server: &Server) -> probe::ProbeOutcome {
        probe::measure(
            server,
            &self.registry.config,
            probe::Route::Direct,
            Ipv4Addr::LOCALHOST,
        )
    }

    fn sync_session(&mut self, profile: &str) {
        let Some(saved) = self.sessions.get(profile).map(Session::state) else {
            return;
        };
        if let Some(existing) = self
            .state
            .sessions
            .iter_mut()
            .find(|session| session.profile == profile)
        {
            *existing = saved;
        } else {
            self.state.sessions.push(saved);
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Stop the child before the struct fields drop so the recovery flag
        // can be persisted as clean in the same pass.
        for session in self.sessions.inner.values_mut() {
            session.core.disconnect();
        }
        if self
            .state
            .sessions
            .iter()
            .any(|session| session.xray_pid.is_some())
        {
            for session in &mut self.state.sessions {
                session.xray_pid = None;
            }
            if let Err(error) = self.state.save() {
                log::warn!("could not clear the Xray recovery PID on shutdown: {error:#}");
            }
        }
    }
}

/// Kill a leftover xray process from a previous run, but only after verifying
/// the PID still belongs to our core — PIDs get recycled.
fn kill_stale_xray(pid: u32, profile: &str) -> bool {
    if !is_our_xray(pid, profile) {
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
/// it. A process merely named `xray` is not enough: with multiple sessions
/// that could be another profile's core.
fn is_our_xray(pid: u32, profile: &str) -> bool {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let cmdline = String::from_utf8_lossy(&raw);
    XrayCore::config_path(profile).is_ok_and(|config| {
        cmdline
            .split('\0')
            .any(|arg| !arg.is_empty() && std::path::Path::new(arg) == config)
    })
}

#[cfg(test)]
mod tests {
    use std::hash::{Hash, Hasher};
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use anyhow::{Context, Result, anyhow};

    use super::{Engine, LOCAL_ID, Session, Sessions};
    use crate::bind;
    use crate::link::parse_link;
    use crate::model::{Server, Subscription};
    use crate::state::{SessionState, State, store};

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TestRoot {
        path: std::path::PathBuf,
    }

    impl TestRoot {
        fn install(label: &str) -> Result<Self> {
            let suffix = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "oxidom-core-test-{label}-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path)
                .with_context(|| format!("creating test root {}", path.display()))?;
            crate::paths::set_test_root(Some(path.clone()));
            Ok(Self { path })
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            crate::paths::set_test_root(None);
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn old_stable_id(seed: &str) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        seed.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    fn old_subscription(url: &str, links: &[&str]) -> Subscription {
        let mut subscription = Subscription::new(url.to_string(), Some("Test".to_string()));
        subscription.id = old_stable_id(url);
        subscription.servers = links
            .iter()
            .map(|link| {
                let mut server = parse_link(link).unwrap();
                server.id = old_stable_id(&server.identity_string());
                server.alias = None;
                server
            })
            .collect();
        subscription
    }

    fn saved_session(server_id: Option<String>) -> SessionState {
        SessionState {
            profile: "default".to_string(),
            server_id,
            address: Ipv4Addr::LOCALHOST,
            socks_port: 10808,
            http_port: 10809,
            xray_pid: None,
        }
    }

    fn saved_profile_session(profile: &str, xray_pid: Option<u32>) -> SessionState {
        SessionState {
            profile: profile.to_string(),
            address: bind::address_for(profile, &[]).unwrap(),
            xray_pid,
            ..saved_session(None)
        }
    }

    fn wait_for_process_identity(child: &mut std::process::Child, profile: &str) -> Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            if super::is_our_xray(child.id(), profile) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let pid = child.id();
        let _ = child.kill();
        let _ = child.wait();
        Err(anyhow!(
            "test process {pid} never exposed the {profile} config path"
        ))
    }

    #[test]
    fn sessions_iterate_in_profile_order_and_report_endpoints() {
        let mut sessions = Sessions::default();
        sessions.insert(Session::new(
            "work".to_string(),
            Ipv4Addr::new(127, 72, 14, 1),
            10808,
            10809,
            String::new(),
        ));
        sessions.insert(Session::new(
            "home".to_string(),
            Ipv4Addr::new(127, 31, 8, 1),
            10808,
            10809,
            String::new(),
        ));

        assert_eq!(
            sessions
                .iter()
                .map(|(profile, _)| profile)
                .collect::<Vec<_>>(),
            ["home", "work"]
        );
        assert_eq!(sessions.len(), 2);
        assert!(!sessions.is_empty());
        assert_eq!(sessions.taken_addresses().len(), 2);
        assert_eq!(
            sessions.get("work").unwrap().socks_endpoint().to_string(),
            "127.72.14.1:10808"
        );
        assert_eq!(
            sessions.get("work").unwrap().http_endpoint().to_string(),
            "127.72.14.1:10809"
        );
        assert!(sessions.owner_of_system_proxy().is_none());
        sessions.claim_system_proxy("home").unwrap();
        assert_eq!(sessions.owner_of_system_proxy(), Some("home"));
        assert!(sessions.claim_system_proxy("work").is_err());
        assert!(sessions.remove("home").is_some());
        assert!(sessions.owner_of_system_proxy().is_none());
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn recovery_reaps_every_recorded_session_and_clears_runtime_state() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("recover-all-sessions")?;
        State {
            sessions: vec![
                saved_profile_session("home", Some(4_000_000)),
                saved_profile_session("work", Some(4_000_001)),
            ],
        }
        .save()?;

        let engine = Engine::load();

        assert!(engine.state.sessions.is_empty());
        assert!(engine.sessions.is_empty());
        let persisted = State::load(&engine.registry.config);
        assert!(persisted.sessions.is_empty());
        Ok(())
    }

    #[test]
    fn recovery_matches_each_orphan_against_its_own_config_path() -> Result<()> {
        use std::process::{Command, Stdio};

        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("recover-profile-configs")?;
        let mut saved = Vec::new();
        let mut waiters = Vec::new();
        for profile in ["home", "work"] {
            let config = crate::xray::core::XrayCore::config_path(profile)?;
            std::fs::create_dir_all(config.parent().context("config parent")?)?;
            let mut child = Command::new("/bin/sh")
                .args(["-c", "while :; do sleep 1; done"])
                .arg(&config)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("starting test orphan for {profile}"))?;
            wait_for_process_identity(&mut child, profile)?;
            let pid = child.id();
            waiters.push(std::thread::spawn(move || child.wait()));
            saved.push(saved_profile_session(profile, Some(pid)));
        }
        State { sessions: saved }.save()?;

        let engine = Engine::load();

        assert!(engine.state.sessions.is_empty());
        assert!(engine.sessions.is_empty());
        for waiter in waiters {
            waiter.join().map_err(|_| anyhow!("waiter panicked"))??;
        }
        Ok(())
    }

    #[test]
    fn one_profile_never_claims_another_profiles_core() -> Result<()> {
        use std::process::{Command, Stdio};

        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("recover-profile-identity")?;
        let config = crate::xray::core::XrayCore::config_path("work")?;
        std::fs::create_dir_all(config.parent().context("config parent")?)?;
        let mut child = Command::new("/bin/sh")
            .args(["-c", "while :; do sleep 1; done"])
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        wait_for_process_identity(&mut child, "work")?;
        assert!(super::is_our_xray(child.id(), "work"));
        assert!(!super::is_our_xray(child.id(), "home"));

        child.kill()?;
        child.wait()?;
        Ok(())
    }

    #[test]
    fn recovery_does_not_forget_an_unrelated_live_process() -> Result<()> {
        use std::process::{Command, Stdio};

        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("recover-unrelated-process")?;
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        State {
            sessions: vec![saved_profile_session("work", Some(child.id()))],
        }
        .save()?;

        let engine = Engine::load();

        assert_eq!(engine.state.sessions.len(), 1);
        assert_eq!(engine.state.sessions[0].xray_pid, Some(child.id()));
        assert!(engine.sessions.get("work").is_some());
        child.kill()?;
        child.wait()?;
        Ok(())
    }

    #[test]
    fn migration_leaves_the_local_group_keyed_by_its_sentinel() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("identity-local-group")?;
        let mut local = old_subscription("", &["trojan://one@one.example:443#Imported"]);
        local.id = LOCAL_ID.to_string();
        store::save(&[local])?;
        State::default().save()?;

        let engine = Engine::load();

        let group = engine
            .registry
            .subscriptions
            .first()
            .ok_or_else(|| anyhow!("the local group disappeared"))?;
        assert_eq!(group.id, LOCAL_ID);
        assert_eq!(group.servers.len(), 1);
        Ok(())
    }

    #[test]
    fn migration_preserves_the_active_server_and_server_count() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("identity-migration")?;
        let subscription = old_subscription(
            "https://subscription.example/token",
            &[
                "trojan://one@one.example:443#One",
                "vless://two@two.example:443#Two",
            ],
        );
        let old_active = subscription.servers[1].id.clone();
        let expected_active = Server::stable_id(&subscription.servers[1].identity_string());
        let server_count = subscription.servers.len();
        store::save(&[subscription])?;
        State {
            sessions: vec![saved_session(Some(old_active))],
        }
        .save()?;

        let engine = Engine::load();

        assert_eq!(engine.all_servers().count(), server_count);
        assert_eq!(
            engine.active_server_id().as_deref(),
            Some(expected_active.as_str())
        );
        assert!(
            engine
                .all_servers()
                .all(|server| server.alias.as_ref().is_some_and(|alias| !alias.is_empty()))
        );
        Ok(())
    }

    #[test]
    fn completed_migration_does_not_rewrite_the_store() -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("identity-no-rewrite")?;
        let mut subscription = old_subscription(
            "https://subscription.example/stable",
            &["trojan://one@one.example:443#One"],
        );
        subscription.id = Server::stable_id(&subscription.url);
        subscription.servers[0].id = Server::stable_id(&subscription.servers[0].identity_string());
        subscription.servers[0].alias = Some("one".to_string());
        store::save(&[subscription])?;
        State::default().save()?;
        let subscriptions_path = crate::paths::subscriptions_file()?;
        let state_path = crate::paths::state_file()?;
        let subscriptions_inode = std::fs::metadata(&subscriptions_path)?.ino();
        let state_inode = std::fs::metadata(&state_path)?.ino();

        let _engine = Engine::load();

        assert_eq!(
            std::fs::metadata(&subscriptions_path)?.ino(),
            subscriptions_inode
        );
        assert_eq!(std::fs::metadata(&state_path)?.ino(), state_inode);
        Ok(())
    }

    #[test]
    fn migration_keeps_the_first_server_when_old_ids_collapse() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("identity-collision")?;
        let mut subscription = old_subscription(
            "https://subscription.example/collision",
            &["trojan://one@one.example:443#First"],
        );
        let mut duplicate = subscription.servers[0].clone();
        duplicate.id = "different-old-id".to_string();
        duplicate.name = "Second".to_string();
        subscription.servers.push(duplicate);
        store::save(&[subscription])?;
        State {
            sessions: vec![saved_session(Some("different-old-id".to_string()))],
        }
        .save()?;

        let engine = Engine::load();

        assert_eq!(engine.all_servers().count(), 1);
        assert_eq!(
            engine
                .all_servers()
                .next()
                .map(|server| server.name.as_str()),
            Some("First")
        );
        assert_eq!(
            engine.active_server_id(),
            engine.all_servers().next().map(|server| server.id.clone())
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires OXIDOM_TEST_SUBSCRIPTIONS pointing at a real cache"]
    fn migrates_a_real_subscription_cache_without_losing_state() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let source = std::env::var_os("OXIDOM_TEST_SUBSCRIPTIONS")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow!("OXIDOM_TEST_SUBSCRIPTIONS is not set"))?;
        let subscriptions: Vec<Subscription> = serde_json::from_str(
            &std::fs::read_to_string(&source)
                .with_context(|| format!("reading {}", source.display()))?,
        )
        .with_context(|| format!("parsing {}", source.display()))?;
        let state_path = source
            .parent()
            .ok_or_else(|| anyhow!("subscription path has no parent"))?
            .join("state.toml");
        let state_body = std::fs::read_to_string(&state_path)
            .with_context(|| format!("reading {}", state_path.display()))?;
        let state_value: toml::Value = toml::from_str(&state_body)
            .with_context(|| format!("parsing {}", state_path.display()))?;
        let active_before = state_value
            .get("active_server_id")
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("the real state has no active server"))?;
        if !subscriptions
            .iter()
            .flat_map(|subscription| subscription.servers.iter())
            .any(|server| server.id == active_before)
        {
            return Err(anyhow!("the real active server is absent before migration"));
        }
        let server_count = subscriptions
            .iter()
            .map(|subscription| subscription.servers.len())
            .sum::<usize>();

        let _root = TestRoot::install("real-identity-migration")?;
        store::save(&subscriptions)?;
        crate::fsutil::write_private_atomic(&crate::paths::state_file()?, state_body.as_bytes())?;
        let engine = Engine::load();

        assert_eq!(engine.all_servers().count(), server_count);
        let active_after = engine
            .active_server_id()
            .ok_or_else(|| anyhow!("migration cleared the active server"))?;
        assert!(
            engine.all_servers().any(|server| server.id == active_after),
            "migrated active server is absent"
        );
        Ok(())
    }
}
