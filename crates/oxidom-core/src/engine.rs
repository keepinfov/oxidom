//! Daemon-owned registry and per-profile runtime sessions.

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, SocketAddrV4, ToSocketAddrs};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::Config;
use crate::model::{Server, Subscription};
use crate::nft::Nft;
use crate::profile::{ProfileInterface, RouteMode};
use crate::run::CgroupSlice;
use crate::state::{self, InterfaceState, RouteRecord, SessionState, State, store};
use crate::tun::core::Tun2socks;
use crate::tun::plan::{Cidr, PlanInput, RoutePlan, Via, plan_routes};
use crate::xray::api::BalancerInfo;
use crate::xray::core::{Status, XrayCore};
use crate::{alias, bind, link, probe, subscription};

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
            for member in &mut session.pool_members {
                if let Some(new_id) = server_ids.get(member) {
                    member.clone_from(new_id);
                }
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

/// Runtime half of a profile interface. Its plan remains available while the
/// interface is stopped so an opted-in reconnect can restore the same routing
/// domain after the new SOCKS inbound has proved ready.
pub struct Interface {
    pub profile: String,
    pub device: String,
    pub address: Ipv4Addr,
    pub mtu: u16,
    pub table: u32,
    pub mark: u32,
    pub routes: RouteMode,
    pub created: bool,
    pub tun2socks: Tun2socks,
    pub nft_binary: String,
    pub cgroup: Option<CgroupSlice>,
    nft_active: bool,
    pub plan: RoutePlan,
    pub up: bool,
    fresh: bool,
}

impl Interface {
    fn state(&self) -> InterfaceState {
        InterfaceState {
            device: self.device.clone(),
            address: self.address,
            mtu: self.mtu,
            table: self.table,
            mark: self.mark,
            created: self.created,
            tun2socks_pid: self.tun2socks.child_pid(),
            routes: self
                .plan
                .private
                .iter()
                .chain(&self.plan.system)
                .map(RouteRecord::from_spec)
                .collect(),
            // Like routes, the rule is recorded as planned before it is
            // applied. Idempotent netlink cleanup makes that the safe side.
            rule: true,
            nft_rule: self.cgroup.is_some(),
        }
    }

    pub fn route_mode(&self) -> &'static str {
        match self.routes {
            RouteMode::Manual => "manual",
            RouteMode::List => "list",
            RouteMode::Default => "default",
        }
    }
}

/// One running profile and the Xray process that carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSelection {
    Server(String),
    Pool {
        members: Vec<String>,
        strategy: String,
        fingerprint: u64,
    },
}

impl SessionSelection {
    pub fn pool(members: Vec<String>, strategy: String) -> Self {
        let fingerprint = pool_fingerprint(&members);
        Self::Pool {
            members,
            strategy,
            fingerprint,
        }
    }

    pub fn server_id(&self) -> Option<&str> {
        match self {
            Self::Server(server_id) => Some(server_id),
            Self::Pool { .. } => None,
        }
    }
}

/// Everything a pool session runs with besides its members.
///
/// One struct rather than five positional arguments threaded through two
/// layers: `name` was the fifth, and adding it flat would have pushed both
/// `Session::connect_pool` and `Engine::connect_pool_session` past the point
/// where the reader has to count commas to know which `u16` is the api port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolSession<'a> {
    /// What the user calls this pool. Purely a label — it names the pool in
    /// `oxidom status` and never takes part in selecting anything.
    pub name: &'a str,
    pub strategy: &'a str,
    pub expected: usize,
    pub probe_interval: &'a str,
    pub api_port: u16,
}

pub fn pool_fingerprint(members: &[String]) -> u64 {
    // Server ids are fixed-width hexadecimal strings, so a NUL separator
    // makes the ordered sequence unambiguous while reusing the project's one
    // stable FNV-1a implementation.
    crate::model::stable_hash(&members.join("\0"))
}

pub struct Session {
    /// Name of the profile that started this session. This is the runtime key.
    pub profile: String,
    pub core: XrayCore,
    pub address: Ipv4Addr,
    pub socks_port: u16,
    pub http_port: u16,
    pub selection: Option<SessionSelection>,
    pub api_port: u16,
    /// The pool's label, empty for a single-server session. Snapshotted at `up`
    /// like the member list, so a session keeps saying what it was brought up
    /// as even after the profile file is edited.
    pub pool_name: String,
    pub pool_expected: usize,
    pub pool_probe_interval: String,
    pub balancer_info: Option<BalancerInfo>,
    pub balancer_polled_at: Option<Instant>,
    pub pool_stale: bool,
    pub interface: Option<Interface>,
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
            selection: None,
            api_port: 0,
            pool_name: String::new(),
            pool_expected: 0,
            pool_probe_interval: String::new(),
            balancer_info: None,
            balancer_polled_at: None,
            pool_stale: false,
            interface: None,
        }
    }

    pub fn connect(&mut self, server: &Server) -> Result<()> {
        self.selection = None;
        self.api_port = 0;
        self.balancer_info = None;
        self.core.connect(server, self.address, &self.profile)?;
        self.selection = Some(SessionSelection::Server(server.id.clone()));
        self.api_port = 0;
        self.pool_name.clear();
        self.pool_expected = 0;
        self.pool_probe_interval.clear();
        self.balancer_info = None;
        self.balancer_polled_at = None;
        self.pool_stale = false;
        Ok(())
    }

    pub fn connect_pool(&mut self, members: &[Server], pool: &PoolSession<'_>) -> Result<()> {
        self.selection = None;
        self.api_port = 0;
        self.balancer_info = None;
        let member_refs = members.iter().collect::<Vec<_>>();
        self.core.connect_pool(
            &crate::xray::config::PoolSpec {
                members: &member_refs,
                strategy: pool.strategy,
                expected: pool.expected,
                probe_interval: pool.probe_interval,
            },
            self.address,
            pool.api_port,
            &self.profile,
        )?;
        self.selection = Some(SessionSelection::pool(
            members.iter().map(|server| server.id.clone()).collect(),
            pool.strategy.to_string(),
        ));
        self.api_port = pool.api_port;
        self.pool_name = pool.name.to_string();
        self.pool_expected = pool.expected;
        self.pool_probe_interval = pool.probe_interval.to_string();
        self.balancer_info = None;
        self.balancer_polled_at = None;
        self.pool_stale = false;
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.core.disconnect();
        self.selection = None;
        self.api_port = 0;
        self.pool_name.clear();
        self.pool_expected = 0;
        self.pool_probe_interval.clear();
        self.balancer_info = None;
        self.balancer_polled_at = None;
        self.pool_stale = false;
    }

    pub fn server_id(&self) -> Option<&str> {
        self.selection
            .as_ref()
            .and_then(SessionSelection::server_id)
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
        let (server_id, pool_members, pool_strategy) = match &self.selection {
            Some(SessionSelection::Server(server_id)) => {
                (Some(server_id.clone()), Vec::new(), String::new())
            }
            Some(SessionSelection::Pool {
                members, strategy, ..
            }) => (None, members.clone(), strategy.clone()),
            None => (None, Vec::new(), String::new()),
        };
        SessionState {
            profile: self.profile.clone(),
            server_id,
            address: self.address,
            socks_port: self.socks_port,
            http_port: self.http_port,
            xray_pid: self.child_pid(),
            interface: self.interface.as_ref().map(Interface::state),
            pool_members,
            pool_strategy,
            api_port: self.api_port,
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

    pub fn taken_device_addresses(&self) -> Vec<Ipv4Addr> {
        self.inner
            .values()
            .filter_map(|session| {
                session
                    .interface
                    .as_ref()
                    .map(|interface| interface.address)
            })
            .collect()
    }

    pub fn taken_routing_marks(&self) -> Vec<u32> {
        self.inner
            .values()
            .filter_map(|session| session.interface.as_ref().map(|interface| interface.mark))
            .collect()
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
            session.selection = saved
                .server_id
                .clone()
                .map(SessionSelection::Server)
                .or_else(|| {
                    (!saved.pool_members.is_empty()).then(|| {
                        SessionSelection::pool(
                            saved.pool_members.clone(),
                            saved.pool_strategy.clone(),
                        )
                    })
                });
            session.api_port = saved.api_port;
            if matches!(&session.selection, Some(SessionSelection::Pool { .. })) {
                let query = crate::profile::load(&saved.profile)
                    .ok()
                    .and_then(|profile| profile.select.pool);
                session.pool_probe_interval = query
                    .as_ref()
                    .map(|query| query.probe_interval_or_default().to_string())
                    .unwrap_or_else(|| "5m".to_string());
                // The label is not persisted in `state.toml` — it is cosmetic,
                // and the profile that owns it is right there on disk.
                session.pool_name = query.map(|query| query.name).unwrap_or_default();
            }
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
        let stale_sessions = self.state.sessions.clone();
        if stale_sessions.is_empty() {
            return;
        }
        let mut cleaned_profiles = Vec::new();
        let mut state_changed = false;
        for saved in &stale_sessions {
            let adopted_pool = saved.xray_pid.filter(|pid| {
                !saved.pool_members.is_empty()
                    && saved.api_port != 0
                    && is_our_xray(*pid, &saved.profile)
            });
            if let Some(pid) = adopted_pool
                && let Some(session) = self.sessions.get_mut(&saved.profile)
            {
                session.core.adopt(pid, &saved.profile);
                log::info!(
                    "adopted running pool Xray process {pid} for profile {:?}",
                    saved.profile
                );
            }
            let core_clean = adopted_pool.is_some()
                || saved.xray_pid.is_none_or(|pid| {
                    if kill_stale_xray(pid, &saved.profile) {
                        // A PID that is simply gone also counts as clean, and
                        // claiming to have stopped it sent one debugging session
                        // looking for a killer that never existed.
                        if std::path::Path::new(&format!("/proc/{pid}")).exists() {
                            log::info!(
                                "stopped orphaned xray process {pid} for profile {:?} from a \
                             previous run",
                                saved.profile
                            );
                        } else {
                            log::info!(
                                "xray process {pid} for profile {:?} was already gone",
                                saved.profile
                            );
                        }
                        true
                    } else {
                        log::warn!(
                            "could not confirm that orphaned xray process {pid} for profile {:?} \
                         stopped",
                            saved.profile
                        );
                        false
                    }
                });
            let interface_clean = saved.interface.as_ref().is_none_or(|interface| {
                match recover_interface(&saved.profile, interface, &self.registry.config.nft_binary)
                {
                    Ok(()) => true,
                    Err(error) => {
                        log::warn!(
                            "could not clean the recovered interface for profile {:?}: {error:#}",
                            saved.profile
                        );
                        false
                    }
                }
            });
            if adopted_pool.is_some() {
                // A recovered core can keep proxying, but the privileged TUN
                // helper cannot be turned back into a `Child`. Clean its
                // recorded kernel domain and retain the adopted proxy session.
                if interface_clean
                    && let Some(current) = self
                        .state
                        .sessions
                        .iter_mut()
                        .find(|session| session.profile == saved.profile)
                    && current.interface.take().is_some()
                {
                    state_changed = true;
                }
                continue;
            }
            if core_clean
                && interface_clean
                && (saved.xray_pid.is_some() || saved.interface.is_some())
            {
                cleaned_profiles.push(saved.profile.clone());
            }
        }
        if cleaned_profiles.is_empty() && !state_changed {
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
        state_changed |= !cleaned_profiles.is_empty();
        if state_changed && let Err(error) = self.state.save() {
            log::warn!("could not persist recovered session state: {error:#}");
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

    /// Attach the profile's requested interface plan to an existing session.
    ///
    /// This is a preflight only: the actual device is created after the Xray
    /// SOCKS inbound has passed its connection probe.
    pub fn configure_interface(
        &mut self,
        profile: &str,
        requested: &ProfileInterface,
        servers: &[Server],
    ) -> Result<()> {
        if !requested.enable {
            if self
                .sessions
                .get(profile)
                .and_then(|session| session.interface.as_ref())
                .is_some()
            {
                self.stop_interface(profile)?;
            }
            if let Some(session) = self.sessions.get_mut(profile) {
                session.interface = None;
            }
            return Ok(());
        }
        if !crate::tun::caps::has_net_admin() {
            bail!("{}", crate::tun::caps::missing_capability_error(profile));
        }

        let device = if requested.device.is_empty() {
            bind::device_name(profile)?
        } else {
            requested.device.clone()
        };
        let address = if requested.address.is_empty() {
            bind::device_address_for(profile, &self.sessions.taken_device_addresses())
                .context("no free profile interface addresses remain")?
        } else {
            requested
                .address
                .parse()
                .context("parsing [interface] address")?
        };
        let mark = bind::routing_mark(profile, &self.sessions.taken_routing_marks())
            .context("no free profile routing marks remain")?;
        let mtu = if requested.mtu == 0 {
            1500
        } else {
            requested.mtu
        };
        let list = requested
            .list
            .iter()
            .map(|entry| entry.parse::<Cidr>())
            .collect::<Result<Vec<_>>>()?;
        // Copied LAN routes are a convenience for marked processes, so a box
        // with no default route at this moment still gets its interface. Only
        // `routes = "default"` genuinely cannot be planned without a gateway.
        let network = crate::tun::net::Net::new()?.default_network()?;
        let connected = network
            .as_ref()
            .map(|network| network.connected.as_slice())
            .unwrap_or_default();
        let (server_addresses, default_gateway) = if requested.routes == RouteMode::Default {
            // A full-tunnel pool pays one DNS lookup per member during `up`.
            // Caching it would make route lifetime and DNS lifetime diverge;
            // phase 5 deliberately resolves the fixed membership once here.
            let server_addresses = resolve_servers_ipv4(servers)?;
            let gateway = network
                .as_ref()
                .and_then(|network| network.gateway)
                .context("routes = \"default\" requires the current default IPv4 gateway")?;
            (server_addresses, Some(gateway))
        } else {
            (Vec::new(), None)
        };
        let plan = plan_routes(&PlanInput {
            table: mark,
            mark,
            mode: requested.routes,
            list: &list,
            server_addresses,
            default_gateway,
            connected,
        })?;
        let mut tun2socks = Tun2socks::new(self.registry.config.tun2socks_binary.clone());
        tun2socks.resolve_binary()?;
        let previous_created = self
            .sessions
            .get(profile)
            .and_then(|session| session.interface.as_ref())
            .is_some_and(|interface| interface.device == device && interface.created);
        if self
            .sessions
            .get(profile)
            .and_then(|session| session.interface.as_ref())
            .is_some()
        {
            self.stop_interface(profile)?;
        }
        // Keep interface diagnostics beside the core output exposed by Logs.
        if let Some(session) = self.sessions.get(profile) {
            tun2socks.logs = session.core.logs.clone();
        }
        let interface = Interface {
            profile: profile.to_string(),
            device: device.clone(),
            address,
            mtu,
            table: mark,
            mark,
            routes: requested.routes,
            created: previous_created || !crate::tun::device::exists(&device),
            tun2socks,
            nft_binary: self.registry.config.nft_binary.clone(),
            cgroup: None,
            nft_active: false,
            plan,
            up: false,
            fresh: false,
        };
        self.sessions
            .get_mut(profile)
            .context("cannot configure an interface before creating its session")?
            .interface = Some(interface);
        Ok(())
    }

    /// Install the one cgroup mark rule belonging to this live session. The
    /// intent reaches state before nftables for the same reason routes do:
    /// cleanup may over-delete safely, but must never forget an applied rule.
    pub fn mark_cgroup(&mut self, profile: &str, uid: u32) -> Result<CgroupSlice> {
        let slice = crate::run::user_slice(profile, uid)?;
        let Some(mut interface) = self
            .sessions
            .get_mut(profile)
            .and_then(|session| session.interface.take())
        else {
            bail!(
                "profile `{profile}` has no network interface. Use `oxidom env {profile}` for \
                 programs that honor proxy environment variables"
            );
        };
        if !interface.up {
            self.sessions
                .get_mut(profile)
                .expect("the session existed above")
                .interface = Some(interface);
            bail!("profile {profile:?} has no live interface to mark a cgroup for");
        }
        if let Some(existing) = interface.cgroup.as_ref()
            && existing != &slice
        {
            let existing_path = existing.path.clone();
            self.sessions
                .get_mut(profile)
                .expect("the session existed above")
                .interface = Some(interface);
            bail!(
                "profile {profile:?} is already bound to cgroup {:?}; bring it down before using \
                 it from another uid",
                existing_path
            );
        }
        if interface.cgroup.as_ref() == Some(&slice) && interface.nft_active {
            self.sessions
                .get_mut(profile)
                .expect("the session existed above")
                .interface = Some(interface);
            return Ok(slice);
        }
        interface.cgroup = Some(slice.clone());
        if let Err(error) = self.persist_interface_state(profile, &interface) {
            self.sessions
                .get_mut(profile)
                .expect("the session existed above")
                .interface = Some(interface);
            return Err(error);
        }
        let result =
            Nft::new(interface.nft_binary.clone()).install(profile, &slice, interface.mark);
        interface.nft_active = result.is_ok();
        self.sessions
            .get_mut(profile)
            .expect("the session existed above")
            .interface = Some(interface);
        result?;
        Ok(slice)
    }

    /// Apply an already planned interface after the session's SOCKS inbound
    /// has proved it can carry traffic.
    pub fn start_interface(&mut self, profile: &str) -> Result<()> {
        let Some(mut interface) = self
            .sessions
            .get_mut(profile)
            .and_then(|session| session.interface.take())
        else {
            return Ok(());
        };
        if interface.up {
            self.sessions
                .get_mut(profile)
                .expect("the session existed above")
                .interface = Some(interface);
            return Ok(());
        }
        if !crate::tun::caps::has_net_admin() {
            let error = anyhow!(crate::tun::caps::missing_capability_error(profile));
            self.sessions
                .get_mut(profile)
                .expect("the session existed above")
                .interface = Some(interface);
            return Err(error);
        }
        let proxy = self
            .sessions
            .get(profile)
            .map(Session::socks_endpoint)
            .expect("the session existed above");

        // The state is intentionally a superset of what reached the kernel:
        // every cleanup operation is idempotent, while an unrecorded route
        // would survive a crash forever.
        if let Err(error) = self.persist_interface_state(profile, &interface) {
            self.sessions
                .get_mut(profile)
                .expect("the session existed above")
                .interface = Some(interface);
            return Err(error);
        }
        let result = start_interface_steps(&mut interface, proxy, |interface| {
            self.persist_interface_state(profile, interface)
        });
        if let Err(error) = result {
            let delete_fresh_device = interface.fresh;
            let rollback = cleanup_live_interface(&mut interface, delete_fresh_device);
            self.sessions
                .get_mut(profile)
                .expect("the session existed above")
                .interface = Some(interface);
            self.sync_session(profile);
            let _ = self.state.save();
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback) => Err(anyhow!(
                    "{error:#}; additionally, rolling the interface back failed: {rollback:#}"
                )),
            };
        }
        self.sessions
            .get_mut(profile)
            .expect("the session existed above")
            .interface = Some(interface);
        self.sync_session(profile);
        self.state.save()?;
        Ok(())
    }

    /// Stop routing through the interface but keep its persistent device and
    /// plan so reconnect can reuse hand-written routes and the same identity.
    pub fn stop_interface(&mut self, profile: &str) -> Result<()> {
        let Some(mut interface) = self
            .sessions
            .get_mut(profile)
            .and_then(|session| session.interface.take())
        else {
            return Ok(());
        };
        let result = cleanup_live_interface(&mut interface, false);
        self.sessions
            .get_mut(profile)
            .expect("the session existed above")
            .interface = Some(interface);
        self.sync_session(profile);
        self.state.save()?;
        result
    }

    /// Explicit `oxidom tun --down`: unlike a profile disconnect, this removes
    /// a device oxidom created and forgets the plan from the live session.
    pub fn delete_interface(&mut self, profile: &str) -> Result<bool> {
        let Some(mut interface) = self
            .sessions
            .get_mut(profile)
            .and_then(|session| session.interface.take())
        else {
            return Ok(false);
        };
        if let Err(error) = cleanup_live_interface(&mut interface, true) {
            self.sessions
                .get_mut(profile)
                .expect("the session existed above")
                .interface = Some(interface);
            return Err(error);
        }
        self.sync_session(profile);
        self.state.save()?;
        Ok(true)
    }

    fn persist_interface_state(&mut self, profile: &str, interface: &Interface) -> Result<()> {
        let saved = self
            .state
            .sessions
            .iter_mut()
            .find(|session| session.profile == profile)
            .with_context(|| format!("session {profile:?} is missing from recovery state"))?;
        saved.interface = Some(interface.state());
        self.state.save()
    }

    pub fn set_default_ports(&mut self, socks_port: u16, http_port: u16) {
        let Some(session) = self.sessions.get_mut("default") else {
            return;
        };
        session.set_ports(socks_port, http_port);
        self.sync_session("default");
    }

    pub fn active_server_id(&self) -> Option<String> {
        self.default_session()?.server_id().map(str::to_string)
    }

    pub fn active_profile(&self) -> Option<String> {
        let session = self.default_session()?;
        session.selection.as_ref().map(|_| session.profile.clone())
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
                |(profile, session)| match (session.server_id(), session.status()) {
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
            self.stop_session(profile);
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
        if self
            .sessions
            .get(profile)
            .and_then(|session| session.interface.as_ref())
            .is_some_and(|interface| interface.up || interface.tun2socks.child_pid().is_some())
        {
            self.stop_interface(profile)?;
        }
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

    pub fn connect_pool_session(
        &mut self,
        profile: &str,
        member_ids: &[String],
        pool: &PoolSession<'_>,
    ) -> Result<()> {
        let members = member_ids
            .iter()
            .map(|id| {
                self.find_server(id)
                    .with_context(|| format!("pool member {id} is no longer in the daemon store"))
            })
            .collect::<Result<Vec<_>>>()?;
        if self
            .sessions
            .get(profile)
            .and_then(|session| session.interface.as_ref())
            .is_some_and(|interface| interface.up || interface.tun2socks.child_pid().is_some())
        {
            self.stop_interface(profile)?;
        }
        self.sessions
            .get_mut(profile)
            .ok_or_else(|| anyhow!("profile {profile:?} has no session"))?
            .connect_pool(&members, pool)?;
        self.sync_session(profile);
        if let Err(error) = self.state.save() {
            log::warn!("could not persist the active Xray pool process: {error:#}");
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
        if let Err(error) = self.stop_interface(profile) {
            log::warn!("could not clean interface for profile {profile:?}: {error:#}");
        }
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
        if self.sessions.get(profile).is_none() {
            return Ok(false);
        }
        self.stop_interface(profile)?;
        let mut session = self
            .sessions
            .remove(profile)
            .expect("the session was checked above");
        session.disconnect();
        self.state.sessions.retain(|saved| saved.profile != profile);
        self.state.save()?;
        Ok(true)
    }

    /// An unnamed `Down` means all sessions, not whichever one happens to be
    /// visible through the compatibility status fields.
    pub fn disconnect_all(&mut self) -> Result<bool> {
        let had_sessions = !self.sessions.is_empty();
        let profiles = self
            .sessions
            .iter()
            .map(|(profile, _)| profile.to_string())
            .collect::<Vec<_>>();
        let mut first_error = None;
        for profile in profiles {
            if let Err(error) = self.remove_session(&profile)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
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

fn resolve_server_ipv4(server: &Server) -> Result<Ipv4Addr> {
    if let Ok(address) = server.address.parse::<Ipv4Addr>() {
        return Ok(address);
    }
    (server.address.as_str(), server.port)
        .to_socket_addrs()
        .with_context(|| {
            format!(
                "resolving server endpoint {}:{} for routes = \"default\"",
                server.address, server.port
            )
        })?
        .find_map(|address| match address.ip() {
            std::net::IpAddr::V4(address) => Some(address),
            std::net::IpAddr::V6(_) => None,
        })
        .context("routes = \"default\" requires the server IPv4 address")
}

/// Ask the OS for a port on this session address, then release it for Xray.
///
/// There is necessarily a short bind-release-bind race, the same one used by
/// probe cores. `ensure_ports_free` closes the diagnostic gap immediately
/// before spawn, while binding permanently here would prevent Xray taking it.
pub fn free_port(bind: Ipv4Addr, excluded: &[u16]) -> Result<u16> {
    for _ in 0..16 {
        let port = std::net::TcpListener::bind((bind, 0))
            .and_then(|listener| listener.local_addr())
            .map(|address| address.port())
            .context("allocating a free port on the session address")?;
        if !excluded.contains(&port) {
            return Ok(port);
        }
    }
    bail!("the OS repeatedly allocated a reserved session port")
}

fn resolve_servers_ipv4(servers: &[Server]) -> Result<Vec<Ipv4Addr>> {
    servers
        .iter()
        .map(|server| {
            resolve_server_ipv4(server).with_context(|| {
                format!(
                    "pool member {:?} ({}) needs a host route to avoid a full-tunnel loop",
                    server.name,
                    server.alias.as_deref().unwrap_or(&server.id)
                )
            })
        })
        .collect()
}

fn start_interface_steps(
    interface: &mut Interface,
    proxy: SocketAddrV4,
    mut persist: impl FnMut(&Interface) -> Result<()>,
) -> Result<()> {
    let created = crate::tun::device::create_persistent(
        &interface.device,
        nix::unistd::Uid::effective().as_raw(),
    )?;
    if created == crate::tun::device::Created::Fresh {
        interface.created = true;
        interface.fresh = true;
    }
    persist(interface)?;

    let net = crate::tun::net::Net::new()?;
    let index = net.link_index(&interface.device)?;
    net.address_add(index, interface.address, 32)?;
    interface
        .tun2socks
        .start(&interface.device, proxy, interface.mtu)?;
    // Persist the recovery PID before another kernel mutation. If the daemon
    // dies between spawn and link-up, the next start can still identify and
    // reap only this tun2socks process.
    persist(interface)?;

    // Gate 4 proved this order with tun2socks 2.7: the helper must attach
    // before administrative link-up. The reverse order was not validated and
    // risks exposing a route before anything can consume its packets.
    net.link_up(index)?;
    for route in &interface.plan.private {
        net.route_add(route, index)?;
    }
    net.rule_add(
        interface.plan.rule.mark,
        interface.plan.rule.table,
        interface.plan.rule.priority,
    )?;
    for route in &interface.plan.system {
        net.route_add(route, index)?;
    }
    if let Some(slice) = interface.cgroup.as_ref() {
        Nft::new(interface.nft_binary.clone()).install(
            &interface.profile,
            slice,
            interface.mark,
        )?;
        interface.nft_active = true;
    }
    interface.up = true;
    interface.fresh = false;
    Ok(())
}

fn cleanup_live_interface(interface: &mut Interface, delete_device: bool) -> Result<()> {
    // Stop assigning the mark before removing its rule/table routes. If nft
    // cannot do that atomically, leave the routing domain intact rather than
    // silently releasing marked traffic onto the ordinary default route.
    if interface.cgroup.is_some() {
        let removed = Nft::new(interface.nft_binary.clone()).remove(&interface.profile);
        // Refusing teardown only makes sense while a mark rule is actually
        // assigning traffic. A profile whose install failed has no rule to
        // release, and treating that as fatal strands its device: the removal
        // fails again on every `down`, so nothing can ever bring it back.
        if interface.nft_active {
            removed?;
        }
        interface.nft_active = false;
    }
    interface.tun2socks.stop();
    interface.up = false;
    let device_exists = crate::tun::device::exists(&interface.device);
    let mut errors = Vec::new();
    match crate::tun::net::Net::new().and_then(|net| {
        let index = device_exists
            .then(|| net.link_index(&interface.device))
            .transpose()?;
        for route in interface.plan.system.iter().rev() {
            if (matches!(route.via, Via::Gateway(_) | Via::Interface(_)) || index.is_some())
                && let Err(error) = net.route_del(route, index.unwrap_or(0))
            {
                errors.push(format!("{error:#}"));
            }
        }
        if let Err(error) = net.rule_del(
            interface.plan.rule.mark,
            interface.plan.rule.table,
            interface.plan.rule.priority,
        ) {
            errors.push(format!("{error:#}"));
        }
        for route in interface.plan.private.iter().rev() {
            if (matches!(route.via, Via::Gateway(_) | Via::Interface(_)) || index.is_some())
                && let Err(error) = net.route_del(route, index.unwrap_or(0))
            {
                errors.push(format!("{error:#}"));
            }
        }
        Ok(())
    }) {
        Ok(()) => {}
        Err(error) => errors.push(format!("{error:#}")),
    }
    if delete_device && device_exists {
        // `created` only knows about this session. A persistent device outlives
        // its session by design, and `down` drops the record that remembered
        // making it, so after the first reconnect the kernel's own notion of
        // ownership is the only one left. Refusing is still a real outcome and
        // is reported rather than passed off as a removal.
        let ours = interface.created
            || crate::tun::device::owned_by(
                &interface.device,
                nix::unistd::Uid::effective().as_raw(),
            );
        if !ours {
            errors.push(format!(
                "refusing to delete network interface {:?}: oxidom did not create it",
                interface.device
            ));
        } else if let Err(error) = crate::tun::device::delete(&interface.device) {
            errors.push(format!("{error:#}"));
        }
    }
    interface.fresh = false;
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

fn recover_interface(profile: &str, interface: &InterfaceState, nft_binary: &str) -> Result<()> {
    if interface.nft_rule {
        Nft::new(nft_binary.to_string()).remove(profile)?;
    }
    if let Some(pid) = interface.tun2socks_pid
        && !kill_stale_tun2socks(pid, &interface.device)
    {
        bail!(
            "could not confirm that recovered tun2socks PID {pid} for device {:?} stopped",
            interface.device
        );
    }
    let device_exists = crate::tun::device::exists(&interface.device);
    let net = crate::tun::net::Net::new()?;
    let index = device_exists
        .then(|| net.link_index(&interface.device))
        .transpose()?;
    for route in interface
        .routes
        .iter()
        .rev()
        .filter(|route| route.table != interface.table)
    {
        let route = route.to_spec();
        if matches!(route.via, Via::Gateway(_) | Via::Interface(_)) || index.is_some() {
            net.route_del(&route, index.unwrap_or(0))?;
        }
    }
    if interface.rule {
        net.rule_del(interface.mark, interface.table, interface.mark)?;
    }
    for route in interface
        .routes
        .iter()
        .rev()
        .filter(|route| route.table == interface.table)
    {
        let route = route.to_spec();
        if matches!(route.via, Via::Interface(_)) || index.is_some() {
            net.route_del(&route, index.unwrap_or(0))?;
        }
    }
    if device_exists && interface.created {
        crate::tun::device::delete(&interface.device)?;
    }
    Ok(())
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
    crate::proc::stop_pid(pid)
}

fn kill_stale_tun2socks(pid: u32, device: &str) -> bool {
    if !is_our_tun2socks(pid, device) {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        log::warn!(
            "refusing to signal stale PID {pid}: it is not oxidom's tun2socks for device \
             {device:?}"
        );
        return false;
    }
    crate::proc::stop_pid(pid)
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
    let Some(cmdline) = crate::proc::cmdline(pid) else {
        return false;
    };
    XrayCore::config_path(profile).is_ok_and(|config| {
        cmdline
            .iter()
            .any(|argument| std::path::Path::new(argument) == config)
    })
}

fn is_our_tun2socks(pid: u32, device: &str) -> bool {
    crate::proc::cmdline(pid).is_some_and(|arguments| arguments_name_tun2socks(&arguments, device))
}

fn arguments_name_tun2socks(arguments: &[String], device: &str) -> bool {
    arguments
        .windows(2)
        .any(|pair| pair[0] == "--device" && pair[1] == device)
}

#[cfg(test)]
mod tests {
    use std::hash::{Hash, Hasher};
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use anyhow::{Context, Result, anyhow};

    use super::{Engine, Interface, LOCAL_ID, Session, Sessions};
    use crate::bind;
    use crate::link::parse_link;
    use crate::model::{Server, Subscription};
    use crate::profile::RouteMode;
    use crate::state::{RouteRecord, SessionState, State, store};
    use crate::tun::core::Tun2socks;
    use crate::tun::plan::{Cidr, RoutePlan, RouteSpec, RuleSpec, Via};

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
            interface: None,
            ..SessionState::default()
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
    fn pool_selection_has_no_server_id_and_hashes_member_order() {
        let first = vec!["one".to_string(), "two".to_string()];
        let reversed = vec!["two".to_string(), "one".to_string()];
        let selection = super::SessionSelection::pool(first.clone(), "roundRobin".to_string());

        assert!(selection.server_id().is_none());
        assert_ne!(
            super::pool_fingerprint(&first),
            super::pool_fingerprint(&reversed)
        );
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
    fn recovery_adopts_a_pool_core_and_keeps_its_api_port() -> Result<()> {
        use std::process::{Command, Stdio};

        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("recover-pool")?;
        let config = crate::xray::core::XrayCore::config_path("spread")?;
        std::fs::create_dir_all(config.parent().context("config parent")?)?;
        let mut child = Command::new("/bin/sh")
            .args(["-c", "while :; do sleep 1; done"])
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        wait_for_process_identity(&mut child, "spread")?;
        let mut saved = saved_profile_session("spread", Some(child.id()));
        saved.pool_members = vec!["one".to_string(), "two".to_string()];
        saved.pool_strategy = "roundRobin".to_string();
        saved.api_port = 18082;
        crate::profile::save(
            "spread",
            &crate::profile::Profile {
                select: crate::profile::ProfileSelect {
                    server: String::new(),
                    pool: Some(crate::pool::PoolQuery {
                        probe_interval: "17s".to_string(),
                        ..crate::pool::PoolQuery::default()
                    }),
                },
                ..crate::profile::Profile::default()
            },
        )?;
        State {
            sessions: vec![saved],
        }
        .save()?;

        let engine = Engine::load();
        let session = engine.sessions.get("spread").context("adopted session")?;
        assert_eq!(session.child_pid(), Some(child.id()));
        assert_eq!(session.api_port, 18082);
        assert_eq!(session.pool_probe_interval, "17s");
        assert!(matches!(
            &session.selection,
            Some(super::SessionSelection::Pool { members, .. })
                if members == &["one".to_string(), "two".to_string()]
        ));
        assert_eq!(session.status(), crate::xray::core::Status::Connected);

        drop(engine);
        child.wait()?;
        Ok(())
    }

    #[test]
    fn recovery_state_records_every_planned_interface_resource() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("recover-interface-record")?;
        let mut saved = saved_profile_session("work", None);
        let planned = Interface {
            profile: "work".to_string(),
            device: "oxi-b2-recover".to_string(),
            address: Ipv4Addr::new(198, 18, 9, 7),
            mtu: 1500,
            table: 0x6f21,
            mark: 0x6f21,
            routes: RouteMode::List,
            created: true,
            tun2socks: Tun2socks::new(String::new()),
            nft_binary: String::new(),
            cgroup: Some(crate::run::user_slice("work", 1000)?),
            nft_active: false,
            plan: RoutePlan {
                private: vec![
                    RouteSpec {
                        destination: Cidr {
                            address: Ipv4Addr::new(192, 168, 1, 0),
                            prefix: 24,
                        },
                        via: Via::Interface(4),
                        table: 0x6f21,
                    },
                    RouteSpec {
                        destination: Cidr {
                            address: Ipv4Addr::UNSPECIFIED,
                            prefix: 0,
                        },
                        via: Via::Device,
                        table: 0x6f21,
                    },
                ],
                system: vec![RouteSpec {
                    destination: Cidr {
                        address: Ipv4Addr::new(10, 0, 0, 0),
                        prefix: 8,
                    },
                    via: Via::Device,
                    table: 254,
                }],
                rule: RuleSpec {
                    mark: 0x6f21,
                    table: 0x6f21,
                    priority: 0x6f21,
                },
            },
            up: false,
            fresh: false,
        };
        let interface = planned.state();
        assert_eq!(
            interface.routes,
            [
                RouteRecord {
                    address: Ipv4Addr::new(192, 168, 1, 0),
                    prefix: 24,
                    table: 0x6f21,
                    gateway: None,
                    interface: Some(4),
                },
                RouteRecord {
                    address: Ipv4Addr::UNSPECIFIED,
                    prefix: 0,
                    table: 0x6f21,
                    gateway: None,
                    interface: None,
                },
                RouteRecord {
                    address: Ipv4Addr::new(10, 0, 0, 0),
                    prefix: 8,
                    table: 254,
                    gateway: None,
                    interface: None,
                },
            ]
        );
        assert!(interface.nft_rule);
        saved.interface = Some(interface.clone());
        State {
            sessions: vec![saved],
        }
        .save()?;

        let loaded = State::load(&crate::config::Config::default());

        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].interface.as_ref(), Some(&interface));
        Ok(())
    }

    #[test]
    fn recovered_tun2socks_identity_requires_our_exact_long_option_pair() {
        let ours =
            ["tun2socks", "--device", "oxi-work", "--proxy", "socks5://x"].map(str::to_string);
        assert!(super::arguments_name_tun2socks(&ours, "oxi-work"));
        let wrong_device = ["tun2socks", "--device", "oxi-home"].map(str::to_string);
        assert!(!super::arguments_name_tun2socks(&wrong_device, "oxi-work"));
        let broken_single_dash = ["tun2socks", "-device", "oxi-work"].map(str::to_string);
        assert!(!super::arguments_name_tun2socks(
            &broken_single_dash,
            "oxi-work"
        ));
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
