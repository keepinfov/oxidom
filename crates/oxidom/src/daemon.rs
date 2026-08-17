//! Headless daemon: owns the Engine (config, subscriptions, xray core,
//! probes) and exposes it on D-Bus. The GUI is a thin client; the tunnel
//! survives the GUI, logout only kills it in `--session` mode.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::sys::signal::{SigSet, Signal};
use zbus::fdo;

use oxidom_core::alias;
use oxidom_core::config::{Config, LatencyMethod};
use oxidom_core::engine::{Engine, PoolSession, SessionSelection, pool_fingerprint};
use oxidom_core::handle::{self, HandleMatch};
use oxidom_core::ipc::{
    ApplySettingsResult, BUS_NAME, InterfaceInfo, LatencyReading, OBJECT_PATH, PROBE_STATE_VERSION,
    PoolMember, ProbeFailure, ProbeRoute, ProbeState, ProfileEntry, RuntimeInfo, SelectionInfo,
    SessionInfo, StatusInfo, UpResult, UpServer,
};
use oxidom_core::model::Server;
use oxidom_core::pool;
use oxidom_core::probe;
use oxidom_core::profile::{self, MAX_POOL_MEMBERS, Profile, RouteMode};
use oxidom_core::xray::core::Status;

/// How many servers may be measured at once. This is now a cap on *processes*:
/// an HTTP probe starts a core of its own to make its request through, so a
/// "check all" over a large subscription would otherwise fork one Xray per
/// server and take the machine with it.
const MAX_CONCURRENT_PROBES: usize = 8;
const ACTIVE_PROBE_INTERVAL: Duration = Duration::from_secs(30);
const CORE_WATCH_INTERVAL: Duration = Duration::from_secs(1);
const POOL_POLL_INTERVAL: Duration = Duration::from_millis(500);
const BALANCER_INFO_TTL: Duration = Duration::from_secs(2);
const BALANCER_API_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a *pool* gets to prove itself before the session is torn down.
///
/// A single server is ready the moment its inbound binds; a pool is not. Until
/// `burstObservatory` returns its first round the balancer has no ranking and
/// hands the request to the first outbound in config order, so one shot at a
/// freshly started pool measures member #1 and nothing else. Measured on Xray
/// 26.3.27 with eight members of which six were dead: dead member first failed
/// 6 of 6 attempts, the same eight with a live member first connected 3 of 3.
/// That is a race, not a verdict on the pool.
const POOL_CONFIRM_WINDOW: Duration = Duration::from_secs(20);
/// Gap between confirmation attempts inside that window. Long enough that the
/// observatory makes progress between tries, short enough that a healthy pool
/// still comes up promptly.
const POOL_CONFIRM_RETRY_GAP: Duration = Duration::from_secs(2);
const CORE_EXITED_MESSAGE: &str = "Xray exited unexpectedly";
const RECONNECTING_MESSAGE: &str = "Xray exited unexpectedly — reconnecting";
/// Consecutive failed liveness probes before a still-running tunnel is declared
/// dead and revived. At `ACTIVE_PROBE_INTERVAL` this is ~90s of unbroken
/// failure — long enough that one slow round, or a server merely slower than
/// the probe budget, never trips it, since any reachable answer resets the run.
const TUNNEL_DEATH_STRIKES: u32 = 3;

fn reconnect_delay(attempt: u32) -> Duration {
    Duration::from_secs((1_u64 << attempt.min(5)).min(30))
}

/// True while a session is actively carrying an `oxidom run` child — the packet
/// mark that routes that child's cgroup into the tunnel is installed. Using
/// `run` is itself the opt-in to resilient recovery, so this outlives a core
/// death: `cleanup_live_interface` keeps `cgroup` set so the mark is reinstated
/// when the tunnel comes back.
fn session_has_run_cgroup(session: &oxidom_core::engine::Session) -> bool {
    session
        .interface
        .as_ref()
        .and_then(|interface| interface.cgroup.as_ref())
        .is_some()
}

/// Whether a profile's tunnel should bring itself back after an unexpected
/// death. Off by default and opt-in through `reconnect`, except for a session
/// running a program under `oxidom run`, which always recovers so the child it
/// carries is not stranded when its server blinks.
fn resilient(engine: &Engine, profile: &str) -> bool {
    engine.registry.config.reconnect
        || engine
            .sessions
            .get(profile)
            .is_some_and(session_has_run_cgroup)
}

/// The probe pipeline: what is being measured now, and what is waiting for a
/// slot behind `MAX_CONCURRENT_PROBES`.
///
/// One lock rather than the two it replaces. Every interesting question — "is
/// this id already spoken for?", "may another probe start?" — spans both sets,
/// so the old pair carried an unwritten `checking` → `probe_queue` ordering
/// that held only because the two call sites remembered to take them that way.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ProbeTarget {
    Direct(String),
    Proxied(String),
}

#[derive(Clone, Debug)]
struct ProbeJob {
    token: u64,
    target: ProbeTarget,
    server_id: String,
}

#[derive(Default)]
struct ProbeQueue {
    next_token: u64,
    running: HashMap<u64, ProbeJob>,
    queued: VecDeque<ProbeJob>,
}

impl ProbeQueue {
    /// Take a logical target unless it is already spoken for. Direct probes
    /// deduplicate by server; proxied probes deduplicate by profile, because
    /// two profiles on one server are two independent connections.
    fn enqueue(&mut self, target: ProbeTarget, server_id: String) -> bool {
        if self.holds(&target) {
            return false;
        }
        let job = self.job(target, server_id);
        self.queued.push_back(job);
        true
    }

    fn holds(&self, target: &ProbeTarget) -> bool {
        self.running.values().any(|job| &job.target == target)
            || self.queued.iter().any(|job| &job.target == target)
    }

    fn job(&mut self, target: ProbeTarget, server_id: String) -> ProbeJob {
        loop {
            self.next_token = self.next_token.wrapping_add(1);
            if self.next_token != 0
                && !self.running.contains_key(&self.next_token)
                && !self.queued.iter().any(|job| job.token == self.next_token)
            {
                return ProbeJob {
                    token: self.next_token,
                    target,
                    server_id,
                };
            }
        }
    }

    /// Promote the next waiting job, when a slot is free.
    fn start_next(&mut self) -> Option<ProbeJob> {
        if self.running.len() >= MAX_CONCURRENT_PROBES {
            return None;
        }
        let job = self.queued.pop_front()?;
        self.running.insert(job.token, job.clone());
        Some(job)
    }

    /// Run this profile now, past the queue and past the cap. The confirmation
    /// after a connect is not a queued measurement: it decides whether the
    /// tunnel the user is watching stays up, and cannot wait behind a bulk
    /// re-check of a whole subscription.
    fn start_now(&mut self, profile: &str, server_id: &str) -> ProbeJob {
        let target = ProbeTarget::Proxied(profile.to_string());
        self.queued.retain(|job| job.target != target);
        // A superseded confirmation may still be unwinding. Give the new one
        // its own token so the old worker cannot release the new worker's slot.
        let job = self.job(target, server_id.to_string());
        self.running.insert(job.token, job.clone());
        job
    }

    fn finish(&mut self, token: u64) {
        self.running.remove(&token);
    }

    /// Both sets as the wire wants them.
    fn snapshot(&self) -> (Vec<String>, Vec<String>) {
        (
            self.running
                .values()
                .map(|job| job.server_id.clone())
                .collect(),
            self.queued
                .iter()
                .map(|job| job.server_id.clone())
                .collect(),
        )
    }

    /// Drop queued ids that are no longer backed by a server. `running` is left
    /// alone on purpose: each of those has a thread that will `finish` it, and
    /// removing the entry early would hand out a slot that is still occupied.
    fn retain_alive(&mut self, servers: &HashSet<String>, profiles: &HashSet<String>) {
        self.queued.retain(|job| {
            servers.contains(&job.server_id)
                && match &job.target {
                    ProbeTarget::Direct(_) => true,
                    ProbeTarget::Proxied(profile) => profiles.contains(profile),
                }
        });
    }
}

/// A failure the daemon reports in place of the core's own status, together
/// with the server it belongs to.
///
/// The id has to be carried explicitly: every path that sets one of these has
/// already stopped its session, which clears the session's server id — so by
/// the time the failure is reportable, the only record of *which* server
/// failed is this struct.
#[derive(Clone)]
struct ErrorOverride {
    status: Status,
    selection: SessionSelection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ConnectionTarget {
    Server(String),
    Pool {
        members: Vec<String>,
        /// The pool's label, carried so a reconnect does not lose it.
        name: String,
        strategy: String,
        /// How many members `leastLoad` keeps rotating; 0 means all of them.
        expected: usize,
        probe_interval: String,
        api_port: u16,
    },
}

impl ConnectionTarget {
    fn selection(&self) -> SessionSelection {
        match self {
            Self::Server(server_id) => SessionSelection::Server(server_id.clone()),
            Self::Pool {
                members, strategy, ..
            } => SessionSelection::pool(members.clone(), strategy.clone()),
        }
    }

    fn server_id(&self) -> Option<&str> {
        match self {
            Self::Server(server_id) => Some(server_id),
            Self::Pool { .. } => None,
        }
    }

    fn probe_label(&self, profile: &str) -> String {
        self.server_id()
            .map(str::to_string)
            .unwrap_or_else(|| format!("pool:{profile}"))
    }
}

/// Rebuild the reconnect target from a live session, exactly as the death sweep
/// does, so a hand-driven revival lands on the same `ConnectionTarget` that
/// `begin_reconnect` will accept as still current — pool members and label
/// included.
fn reconnect_target(session: &oxidom_core::engine::Session) -> Option<ConnectionTarget> {
    Some(match session.selection.clone()? {
        SessionSelection::Server(server_id) => ConnectionTarget::Server(server_id),
        SessionSelection::Pool {
            members, strategy, ..
        } => ConnectionTarget::Pool {
            members,
            name: session.pool_name.clone(),
            strategy,
            expected: session.pool_expected,
            probe_interval: session.pool_probe_interval.clone(),
            api_port: session.api_port,
        },
    })
}

impl From<&str> for ConnectionTarget {
    fn from(server_id: &str) -> Self {
        Self::Server(server_id.to_string())
    }
}

impl From<&ConnectionTarget> for ConnectionTarget {
    fn from(target: &ConnectionTarget) -> Self {
        target.clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnectionOrigin {
    Explicit,
    Reconnect,
}

pub struct DaemonOptions {
    pub system_bus: bool,
    pub socks_port: Option<u16>,
    pub http_port: Option<u16>,
}

#[derive(Clone)]
pub(crate) struct Shared {
    engine: Arc<Mutex<Engine>>,
    readings: Arc<Mutex<HashMap<String, LatencyReading>>>,
    proxied: Arc<Mutex<HashMap<String, LatencyReading>>>,
    probes: Arc<Mutex<ProbeQueue>>,
    /// Layered over the core status, e.g. when the confirming probe after a
    /// connect fails and the daemon shuts the tunnel back down.
    override_status: Arc<Mutex<HashMap<String, ErrorOverride>>>,
    /// Consecutive dead-tunnel probes per profile, for the watchdog that revives
    /// a resilient session whose server stops answering while its core keeps
    /// running (so `note_core_deaths` never sees it). Reset by any reachable
    /// probe, when a session leaves `Connected`, and on prune.
    tunnel_failures: Arc<Mutex<HashMap<String, u32>>>,
    /// Generates attempt ids; `current_generations` keeps one cancellation
    /// domain per profile so bringing `work` up cannot invalidate `default`'s
    /// in-flight confirmation.
    connect_generation: Arc<AtomicU64>,
    current_generations: Arc<Mutex<HashMap<String, u64>>>,
    /// Ports pinned by `--socks-port`/`--http-port`, i.e. by the service unit.
    socks_port_locked: bool,
    http_port_locked: bool,
    /// True when serving the system bus, where callers are other users rather
    /// than the person who started the daemon.
    system_bus: bool,
}

impl Shared {
    pub(crate) fn new(
        engine: Engine,
        socks_port_locked: bool,
        http_port_locked: bool,
        system_bus: bool,
    ) -> Self {
        let mut generation = 0;
        let current_generations = engine
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.selection.is_some() && session.status() == Status::Connected
            })
            .map(|(profile, _)| {
                generation += 1;
                (profile.to_string(), generation)
            })
            .collect();
        Shared {
            engine: Arc::new(Mutex::new(engine)),
            readings: Arc::new(Mutex::new(HashMap::new())),
            proxied: Arc::new(Mutex::new(HashMap::new())),
            probes: Arc::new(Mutex::new(ProbeQueue::default())),
            override_status: Arc::new(Mutex::new(HashMap::new())),
            tunnel_failures: Arc::new(Mutex::new(HashMap::new())),
            // An adopted pool core is already a live attempt. Giving it a
            // generation makes its first post-recovery death reconnectable
            // while retaining the same cancellation rules as a fresh `up`.
            connect_generation: Arc::new(AtomicU64::new(generation)),
            current_generations: Arc::new(Mutex::new(current_generations)),
            socks_port_locked,
            http_port_locked,
            system_bus,
        }
    }

    /// The ports a profile actually gets, and the names of the ones it asked
    /// for and did not. Unit-pinned ports describe the primary tunnel and
    /// therefore constrain only `default`; every other profile keeps its own
    /// ports on its own loopback address.
    fn reconcile_profile_ports(
        &self,
        config: &Config,
        profile_name: &str,
        profile: &Profile,
    ) -> (u16, u16, Vec<String>) {
        let mut ignored = Vec::new();
        let mut socks_port = profile.proxy.socks_port;
        let mut http_port = profile.proxy.http_port;
        if profile_name == "default" && self.socks_port_locked && socks_port != config.socks_port {
            socks_port = config.socks_port;
            ignored.push("SOCKS port".to_string());
        }
        if profile_name == "default" && self.http_port_locked && http_port != config.http_port {
            http_port = config.http_port;
            ignored.push("HTTP port".to_string());
        }
        (socks_port, http_port, ignored)
    }

    /// Invalidate any in-flight connect confirmation and return the id of the
    /// attempt starting now.
    fn next_connect_generation(&self, profile: &str) -> u64 {
        let generation = self.connect_generation.fetch_add(1, Ordering::SeqCst) + 1;
        oxidom_core::sync::lock(&self.current_generations).insert(profile.to_string(), generation);
        generation
    }

    fn generation_is_current(&self, profile: &str, generation: u64) -> bool {
        oxidom_core::sync::lock(&self.current_generations).get(profile) == Some(&generation)
    }

    fn current_generation(&self, profile: &str) -> Option<u64> {
        oxidom_core::sync::lock(&self.current_generations)
            .get(profile)
            .copied()
    }

    fn invalidate_generation(&self, profile: &str) {
        oxidom_core::sync::lock(&self.current_generations).remove(profile);
    }

    fn invalidate_all_generations(&self) {
        oxidom_core::sync::lock(&self.current_generations).clear();
    }

    /// Return every session whose core has just been found dead, once.
    ///
    /// The sweep is deliberately per profile: two sessions may carry the same
    /// server, and the health of one process says nothing about the other.
    fn note_core_deaths(&self) -> Vec<(String, ConnectionTarget)> {
        let mut engine = oxidom_core::sync::lock(&self.engine);
        let profiles = engine
            .sessions
            .iter()
            .map(|(profile, _)| profile.to_string())
            .collect::<Vec<_>>();
        let mut dead = Vec::new();
        for profile in profiles {
            let Some(session) = engine.sessions.get_mut(&profile) else {
                continue;
            };
            let alive = session.is_alive();
            if session.status() != Status::Connected || alive {
                continue;
            }
            let Some(target) = reconnect_target(session) else {
                continue;
            };
            session.core.fail(CORE_EXITED_MESSAGE);
            session.balancer_info = None;
            dead.push((profile, target));
        }
        for (profile, _) in &dead {
            if let Err(error) = engine.stop_interface(profile) {
                log::warn!(
                    "could not clean the interface after profile {profile:?}'s core exited: \
                     {error:#}"
                );
            }
            engine.sessions.release_system_proxy(profile);
        }
        dead
    }

    fn begin_reconnect(&self, profile: String, target: ConnectionTarget, generation: u64) {
        if !self.generation_is_current(&profile, generation) {
            return;
        }
        let death_is_current = {
            let engine = oxidom_core::sync::lock(&self.engine);
            resilient(&engine, &profile)
                && engine.sessions.get(&profile).is_some_and(|session| {
                    session.selection.as_ref() == Some(&target.selection())
                        && match &target {
                            ConnectionTarget::Server(_) => true,
                            ConnectionTarget::Pool { api_port, .. } => {
                                session.api_port == *api_port
                            }
                        }
                })
                && matches!(
                    engine.sessions.get(&profile).map(|session| session.status()),
                    Some(Status::Error(message)) if message == CORE_EXITED_MESSAGE
                )
        };
        if !death_is_current || !self.generation_is_current(&profile, generation) {
            return;
        }
        let already_reconnecting = matches!(
            oxidom_core::sync::lock(&self.override_status).get(&profile),
            Some(ErrorOverride {
                status: Status::Error(message),
                selection,
            }) if message == RECONNECTING_MESSAGE && selection == &target.selection()
        );
        if already_reconnecting {
            return;
        }

        oxidom_core::sync::lock(&self.override_status).insert(
            profile.clone(),
            ErrorOverride {
                status: Status::Error(RECONNECTING_MESSAGE.to_string()),
                selection: target.selection(),
            },
        );
        let shared = self.clone();
        std::thread::spawn(move || shared.reconnect(profile, target, generation));
    }

    fn reconnect(&self, profile: String, target: ConnectionTarget, generation: u64) {
        let mut attempt = 0;
        loop {
            if !self.reconnect_is_pending(&profile, &target, generation) {
                return;
            }
            std::thread::sleep(reconnect_delay(attempt));
            if !self.reconnect_is_pending(&profile, &target, generation) {
                return;
            }

            let Some(result) = self.start_reconnect_attempt(&profile, &target, generation) else {
                return;
            };
            match result {
                Ok(confirmed) => {
                    if confirmed.recv().unwrap_or(false)
                        && self.reconnect_is_pending(&profile, &target, generation)
                    {
                        self.clear_reconnect_override(&profile, &target);
                        return;
                    }
                }
                Err(error) => {
                    log::warn!("automatic reconnect of profile {profile:?} failed: {error:#}");
                }
            }
            attempt = attempt.saturating_add(1);
        }
    }

    fn reconnect_is_pending(
        &self,
        profile: &str,
        target: &ConnectionTarget,
        generation: u64,
    ) -> bool {
        if !self.generation_is_current(profile, generation) {
            return false;
        }
        let is_resilient = {
            let engine = oxidom_core::sync::lock(&self.engine);
            resilient(&engine, profile)
        };
        if !is_resilient {
            self.clear_reconnect_override(profile, target);
            return false;
        }
        matches!(
            oxidom_core::sync::lock(&self.override_status).get(profile),
            Some(ErrorOverride {
                status: Status::Error(message),
                selection,
            }) if message == RECONNECTING_MESSAGE && selection == &target.selection()
        )
    }

    fn clear_reconnect_override(&self, profile: &str, target: &ConnectionTarget) {
        let mut override_status = oxidom_core::sync::lock(&self.override_status);
        let is_ours = matches!(
            override_status.get(profile),
            Some(ErrorOverride {
                status: Status::Error(message),
                selection,
            }) if message == RECONNECTING_MESSAGE && selection == &target.selection()
        );
        if is_ours {
            override_status.remove(profile);
        }
    }

    fn clear_override(&self, profile: &str) {
        oxidom_core::sync::lock(&self.override_status).remove(profile);
    }

    fn clear_all_overrides(&self) {
        oxidom_core::sync::lock(&self.override_status).clear();
    }

    fn status_info(&self) -> StatusInfo {
        for (profile, target) in self.note_core_deaths() {
            if let Some(generation) = self.current_generation(&profile) {
                self.begin_reconnect(profile, target, generation);
            }
        }
        // Scoped, not `if let`: in edition 2024 an `if let` scrutinee's
        // temporary lives for the whole body, so holding this guard while
        // touching `engine` below would take the two locks in the opposite
        // order from `confirm_connection` — engine first, then override — and
        // deadlock the daemon the moment the two raced.
        let overrides = oxidom_core::sync::lock(&self.override_status).clone();
        let engine = oxidom_core::sync::lock(&self.engine);
        let sessions = engine
            .sessions
            .iter()
            .map(|(profile, session)| {
                session_info(&engine, profile, session, overrides.get(profile))
            })
            .collect::<Vec<_>>();
        let Some(session) = engine.default_session() else {
            let mut status = StatusInfo::from_status(&Status::Disconnected, None);
            status.sessions = sessions;
            return status;
        };
        let profile = session.profile.as_str();
        let mut status = if let Some(failure) = overrides.get(profile) {
            StatusInfo::from_status(&failure.status, None)
                .with_error_id(failure.selection.server_id().map(str::to_string))
        } else {
            StatusInfo::from_status(&session.status(), session.server_id().map(str::to_string))
                .with_active_profile(session.selection.as_ref().map(|_| profile.to_string()))
        };
        status.sessions = sessions;
        status
    }

    fn list_session_infos(&self) -> Vec<SessionInfo> {
        let overrides = oxidom_core::sync::lock(&self.override_status).clone();
        let engine = oxidom_core::sync::lock(&self.engine);
        engine
            .sessions
            .iter()
            .map(|(profile, session)| {
                session_info(&engine, profile, session, overrides.get(profile))
            })
            .collect()
    }

    fn session_info(&self, profile: &str) -> Option<SessionInfo> {
        let override_status = oxidom_core::sync::lock(&self.override_status)
            .get(profile)
            .cloned();
        let engine = oxidom_core::sync::lock(&self.engine);
        let session = engine.sessions.get(profile)?;
        Some(session_info(
            &engine,
            profile,
            session,
            override_status.as_ref(),
        ))
    }

    fn runtime_info(&self) -> RuntimeInfo {
        let engine = oxidom_core::sync::lock(&self.engine);
        let resolved = oxidom_core::xray::resolve::resolve(&engine.registry.config.xray_binary);
        let (xray_path, xray_error, xray_source) = match resolved {
            Ok(resolved) => (
                Some(resolved.path.display().to_string()),
                None,
                Some(resolved.source),
            ),
            Err(error) => (None, Some(format!("{error:#}")), None),
        };
        RuntimeInfo {
            xray_path,
            xray_error,
            xray_source,
            socks_port_locked: self.socks_port_locked,
            http_port_locked: self.http_port_locked,
            socks_port: engine.registry.config.socks_port,
            http_port: engine.registry.config.http_port,
            // Same rule `set_settings` applies below, published so the GUI can
            // grey the rows instead of accepting text the daemon will revert.
            binary_paths_locked: self.system_bus,
        }
    }

    /// Pick the connection a generic server probe describes. A server carried
    /// by a session is measured through the first such profile in stable
    /// profile order; everything else is a direct server measurement.
    fn probe_target(&self, server_id: &str) -> ProbeTarget {
        let engine = oxidom_core::sync::lock(&self.engine);
        engine
            .sessions
            .iter()
            .find(|(_, session)| {
                session.status() == Status::Connected && session.server_id() == Some(server_id)
            })
            .map(|(profile, _)| ProbeTarget::Proxied(profile.to_string()))
            .unwrap_or_else(|| ProbeTarget::Direct(server_id.to_string()))
    }

    fn enqueue_probe(&self, server_id: String) {
        let target = self.probe_target(&server_id);
        if !oxidom_core::sync::lock(&self.probes).enqueue(target, server_id) {
            return;
        }
        self.pump_probes();
    }

    fn enqueue_session_probe(&self, profile: String, label: String) {
        if !oxidom_core::sync::lock(&self.probes).enqueue(ProbeTarget::Proxied(profile), label) {
            return;
        }
        self.pump_probes();
    }

    fn pump_probes(&self) {
        loop {
            let Some(next) = oxidom_core::sync::lock(&self.probes).start_next() else {
                return;
            };
            let shared = self.clone();
            std::thread::spawn(move || {
                shared.run_probe(&next);
                oxidom_core::sync::lock(&shared.probes).finish(next.token);
                shared.pump_probes();
            });
        }
    }

    /// Probe one server and record the outcome in the map belonging to the
    /// job's route. Direct jobs complete in `readings`; live connection jobs
    /// complete in `proxied`.
    fn run_probe(&self, job: &ProbeJob) -> Option<u32> {
        match &job.target {
            ProbeTarget::Direct(_) => self.run_direct_probe(&job.server_id),
            ProbeTarget::Proxied(profile) => self.run_session_probe(profile),
        }
    }

    fn run_direct_probe(&self, server_id: &str) -> Option<u32> {
        let target = {
            let engine = oxidom_core::sync::lock(&self.engine);
            engine
                .find_server(server_id)
                .map(|server| (server, engine.registry.config.clone()))
        };
        let reading = match target {
            Some((server, config)) => {
                let method = config.latency_method;
                match probe::measure(&server, &config, probe::Route::Direct, Ipv4Addr::LOCALHOST) {
                    // The reading carries the method that produced it, not the
                    // one the config asked for: a hysteria2 server may answer
                    // only ICMP. The card says which it was rather than
                    // passing a handshake off as the user's chosen probe.
                    probe::ProbeOutcome::Reachable(measured) => {
                        LatencyReading::ok(measured.ms, ProbeRoute::Direct, measured.method)
                    }
                    outcome => wire_reading(&outcome, ProbeRoute::Direct, method),
                }
            }
            // The server was removed between the request and its slot. Nothing
            // was measured and nothing about it is known — including which
            // method would have been used, since that read the config we never
            // got to.
            None => LatencyReading::failed(
                ProbeFailure::Unknown,
                ProbeRoute::Direct,
                LatencyMethod::default(),
            ),
        };
        let value = reading.value;
        oxidom_core::sync::lock(&self.readings).insert(server_id.to_string(), reading);
        value
    }

    /// Probe the tunnel owned by one profile, keeping the raw outcome.
    ///
    /// Split out from `run_session_probe` so the watchdog can tell a server that
    /// stopped answering (`Timeout`/`Unreachable`) from one that is merely slow
    /// or from this machine losing its own uplink — a distinction the collapsed
    /// `Option<u32>` throws away. Returns the method actually attempted alongside
    /// the outcome, since a pool measures through a different verb than a single
    /// server. `None` means there was nothing to measure.
    fn session_probe_outcome(&self, profile: &str) -> Option<(probe::ProbeOutcome, LatencyMethod)> {
        let (server, config, address, selection) = {
            let engine = oxidom_core::sync::lock(&self.engine);
            match engine.sessions.get(profile) {
                Some(session) if session.status() == Status::Connected => {
                    let mut config = engine.registry.config.clone();
                    config.socks_port = session.socks_port;
                    config.http_port = session.http_port;
                    let server = session
                        .server_id()
                        .and_then(|server_id| engine.find_server(server_id));
                    (server, config, session.address, session.selection.clone())
                }
                _ => return None,
            }
        };
        let requested_method = config.latency_method;
        Some(match (selection, server) {
            (Some(SessionSelection::Server(_)), Some(server)) => (
                probe::measure(&server, &config, probe::Route::Proxied, address),
                requested_method,
            ),
            (Some(SessionSelection::Pool { .. }), _) => {
                // ICMP/TCP name one member and therefore cannot measure a
                // rotating session. HTTP through the live SOCKS inbound measures
                // exactly what the balancer carries.
                let (verb, method) = pool_probe_method(requested_method);
                (
                    probe::http_ping(
                        address,
                        config.socks_port,
                        &config.latency_test_url,
                        verb,
                        Duration::from_secs(3),
                    ),
                    method,
                )
            }
            _ => (
                probe::ProbeOutcome::Internal(oxidom_core::ipc::ProbeDetail::Other),
                requested_method,
            ),
        })
    }

    /// Measure the tunnel owned by one profile, recording the reading the GUI
    /// shows and returning the latency in ms (or `None` on any failure).
    fn run_session_probe(&self, profile: &str) -> Option<u32> {
        let reading = match self.session_probe_outcome(profile) {
            Some((probe::ProbeOutcome::Reachable(measured), _)) => {
                LatencyReading::ok(measured.ms, ProbeRoute::Proxied, measured.method)
            }
            Some((outcome, attempted_method)) => {
                wire_reading(&outcome, ProbeRoute::Proxied, attempted_method)
            }
            None => LatencyReading::failed(
                ProbeFailure::Unknown,
                ProbeRoute::Proxied,
                LatencyMethod::default(),
            ),
        };
        let value = reading.value;
        let still_current = {
            let engine = oxidom_core::sync::lock(&self.engine);
            engine.sessions.get(profile).is_some_and(|session| {
                session.status() == Status::Connected && session.selection.is_some()
            })
        };
        if still_current {
            oxidom_core::sync::lock(&self.proxied).insert(profile.to_string(), reading);
        }
        value
    }

    /// Start every connection through one path, whether the caller is a D-Bus
    /// request or the supervisor. Claiming the probe slot here preserves the
    /// same honest card state while `confirm_connection` proves the tunnel.
    fn start_connection(
        &self,
        profile: &str,
        server_id: &str,
        generation: u64,
        origin: ConnectionOrigin,
    ) -> Result<Option<mpsc::Receiver<bool>>> {
        self.start_connection_target(
            profile,
            &ConnectionTarget::Server(server_id.to_string()),
            generation,
            origin,
        )
    }

    fn start_connection_target(
        &self,
        profile: &str,
        target: &ConnectionTarget,
        generation: u64,
        origin: ConnectionOrigin,
    ) -> Result<Option<mpsc::Receiver<bool>>> {
        if !self.generation_is_current(profile, generation) {
            return Ok(None);
        }
        oxidom_core::sync::lock(&self.proxied).remove(profile);
        // A fresh attempt starts the death streak over, so a couple of misses
        // remembered from before an explicit reconnect cannot cut the new
        // tunnel's grace short.
        oxidom_core::sync::lock(&self.tunnel_failures).remove(profile);
        let probe_label = target.probe_label(profile);
        let probe_job = oxidom_core::sync::lock(&self.probes).start_now(profile, &probe_label);

        let connect_result = {
            let mut engine = oxidom_core::sync::lock(&self.engine);
            if !self.generation_is_current(profile, generation) {
                None
            } else {
                Some(match target {
                    ConnectionTarget::Server(server_id) => {
                        engine.connect_session(profile, server_id)
                    }
                    ConnectionTarget::Pool {
                        members,
                        name,
                        strategy,
                        expected,
                        probe_interval,
                        api_port,
                    } => engine.connect_pool_session(
                        profile,
                        members,
                        &PoolSession {
                            name,
                            strategy,
                            expected: *expected,
                            probe_interval,
                            api_port: *api_port,
                        },
                    ),
                })
            }
        };
        match connect_result {
            None => {
                oxidom_core::sync::lock(&self.probes).finish(probe_job.token);
                Ok(None)
            }
            Some(Err(error)) => {
                oxidom_core::sync::lock(&self.probes).finish(probe_job.token);
                oxidom_core::sync::lock(&self.engine)
                    .sessions
                    .release_system_proxy(profile);
                Err(error)
            }
            Some(Ok(())) => Ok(Some(self.confirm_connection(
                profile.to_string(),
                target.clone(),
                generation,
                origin,
                probe_job.token,
            ))),
        }
    }

    /// `None` means an explicit operation has already superseded this retry,
    /// so no Xray start was attempted.
    fn start_reconnect_attempt<T: Into<ConnectionTarget>>(
        &self,
        profile: &str,
        target: T,
        generation: u64,
    ) -> Option<Result<mpsc::Receiver<bool>>> {
        let target = target.into();
        if !self.generation_is_current(profile, generation) {
            return None;
        }
        match self.start_connection_target(
            profile,
            &target,
            generation,
            ConnectionOrigin::Reconnect,
        ) {
            Ok(Some(confirmed)) => Some(Ok(confirmed)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    }

    /// After a connect: confirm the tunnel actually works; tear it down and
    /// surface an error when it does not.
    ///
    /// `generation` identifies the connect attempt this confirmation belongs
    /// to. Without it a slow probe from a superseded attempt would tear down
    /// the healthy connection that replaced it — the server id alone does not
    /// distinguish two connects to the same server.
    /// The id is already `running` when this starts — `start_connection` claims
    /// the slot — so this thread owns it and must release it on every path.
    /// Probe a freshly started pool until it carries a request or its window
    /// runs out.
    ///
    /// The retry is the whole point: a pool that has not been observed yet
    /// routes to its first member, so a single shot reports on member #1 rather
    /// than on the pool. Retrying costs nothing when the pool is healthy — the
    /// first attempt succeeds — and is the difference between "this pool is
    /// dead" and "this pool had not been measured yet".
    fn confirm_pool(&self, profile: &str, generation: u64) -> Option<u32> {
        let deadline = Instant::now() + POOL_CONFIRM_WINDOW;
        loop {
            if let Some(ms) = self.run_session_probe(profile) {
                return Some(ms);
            }
            // A superseded attempt or a core that already exited has nothing to
            // wait for; retrying would only delay the report of what happened.
            if !self.generation_is_current(profile, generation) {
                return None;
            }
            let running = {
                let engine = oxidom_core::sync::lock(&self.engine);
                engine
                    .sessions
                    .get(profile)
                    .is_some_and(|session| session.status() == Status::Connected)
            };
            if !running || Instant::now() + POOL_CONFIRM_RETRY_GAP >= deadline {
                return None;
            }
            std::thread::sleep(POOL_CONFIRM_RETRY_GAP);
        }
    }

    fn confirm_connection(
        &self,
        profile: String,
        target: ConnectionTarget,
        generation: u64,
        origin: ConnectionOrigin,
        probe_token: u64,
    ) -> mpsc::Receiver<bool> {
        let (confirmed, confirmation) = mpsc::channel();
        let shared = self.clone();
        std::thread::spawn(move || {
            let (address, socks_port, method) = {
                let engine = oxidom_core::sync::lock(&shared.engine);
                let Some(session) = engine.sessions.get(&profile) else {
                    oxidom_core::sync::lock(&shared.proxied).insert(
                        profile.clone(),
                        LatencyReading::failed(
                            ProbeFailure::Unknown,
                            ProbeRoute::Proxied,
                            LatencyMethod::default(),
                        ),
                    );
                    oxidom_core::sync::lock(&shared.probes).finish(probe_token);
                    let _ = confirmed.send(false);
                    return;
                };
                (
                    session.address,
                    session.socks_port,
                    engine.registry.config.latency_method,
                )
            };

            // The core being alive proves nothing: readiness is the inbound
            // accepting connections. Waiting here is also what keeps the probe
            // below from racing a core that simply has not bound yet.
            let ready = probe::wait_for_socks(address, socks_port);

            let latency = if ready && target.server_id().is_none() {
                shared.confirm_pool(&profile, generation)
            } else if ready {
                shared.run_session_probe(&profile)
            } else {
                // Nothing could be measured, but the GUI is waiting on this id
                // and would otherwise see the spinner retire onto whatever the
                // map still held. Record the failure it actually is.
                oxidom_core::sync::lock(&shared.proxied).insert(
                    profile.clone(),
                    LatencyReading::failed(ProbeFailure::Timeout, ProbeRoute::Proxied, method),
                );
                None
            };
            oxidom_core::sync::lock(&shared.probes).finish(probe_token);
            if ready && latency.is_some() {
                let current = shared.generation_is_current(&profile, generation);
                if current {
                    let mut engine = oxidom_core::sync::lock(&shared.engine);
                    if let Err(error) = engine.start_interface(&profile) {
                        let reason = format!("could not bring up the profile interface: {error:#}");
                        if let Some(session) = engine.sessions.get(&profile) {
                            session.core.note(&reason);
                        }
                        engine.stop_session(&profile);
                        if origin == ConnectionOrigin::Explicit {
                            oxidom_core::sync::lock(&shared.override_status).insert(
                                profile.clone(),
                                ErrorOverride {
                                    status: Status::Error(reason),
                                    selection: target.selection(),
                                },
                            );
                        }
                        let _ = confirmed.send(false);
                        return;
                    }
                    if engine.registry.config.system_proxy
                        && let Err(error) = engine.sessions.claim_system_proxy(&profile)
                    {
                        log::warn!(
                            "profile {profile:?} reconnected without the system proxy: {error:#}"
                        );
                    }
                }
                let _ = confirmed.send(current);
                return;
            }
            let mut engine = oxidom_core::sync::lock(&shared.engine);
            // Bail out if another connect/disconnect superseded this attempt:
            // the tunnel now running is not the one this thread was confirming.
            if !shared.generation_is_current(&profile, generation) {
                let _ = confirmed.send(false);
                return;
            }
            // A core that rejected the config exited at once, so both the dead
            // inbound and the failed probe are symptoms. Say the actual cause.
            let logs = engine
                .sessions
                .get(&profile)
                .map(|session| session.current_run_logs())
                .unwrap_or_default();
            let rotating = engine
                .sessions
                .get(&profile)
                .and_then(|session| session.balancer_info.as_ref())
                .map(|info| info.principle.len());
            let reason = confirmation_failure(&target, ready, &logs, rotating);
            let still_active = engine.sessions.get(&profile).is_some_and(|session| {
                session.selection.as_ref() == Some(&target.selection())
                    && session.status() == Status::Connected
            });
            if still_active {
                // Leave the reason in the log buffer too: the tunnel is
                // torn down below, so the core's own status is lost.
                if let Some(session) = engine.sessions.get(&profile) {
                    session.core.note(&reason);
                }
                engine.stop_session(&profile);
                if origin == ConnectionOrigin::Explicit {
                    oxidom_core::sync::lock(&shared.override_status).insert(
                        profile.clone(),
                        ErrorOverride {
                            status: Status::Error(reason),
                            selection: target.selection(),
                        },
                    );
                }
            }
            let _ = confirmed.send(false);
        });
        confirmation
    }

    /// Forget everything remembered about servers that no longer exist.
    ///
    /// A reading outlives its server otherwise: subscriptions issue fresh ids
    /// on every refresh, so the map would grow for the life of the daemon and —
    /// worse — an id reused by a later refresh would inherit a number measured
    /// against a different endpoint.
    ///
    /// The queue is pruned in the same pass, and not as a nicety: a removed id
    /// still waiting for a slot would come round moments later and have
    /// `run_probe` write a fresh `failed(Unknown, ..)` entry, undoing the clean
    /// that just ran.
    fn prune_readings(&self) {
        // engine → readings, the same order `run_probe` takes them in, and the
        // engine lock is dropped before either of the others is touched.
        let (alive_servers, alive_profiles): (HashSet<String>, HashSet<String>) = {
            let engine = oxidom_core::sync::lock(&self.engine);
            (
                engine
                    .all_servers()
                    .map(|server| server.id.clone())
                    .collect(),
                engine
                    .sessions
                    .iter()
                    .map(|(profile, _)| profile.to_string())
                    .collect(),
            )
        };
        oxidom_core::sync::lock(&self.readings).retain(|id, _| alive_servers.contains(id));
        oxidom_core::sync::lock(&self.proxied)
            .retain(|profile, _| alive_profiles.contains(profile));
        oxidom_core::sync::lock(&self.tunnel_failures)
            .retain(|profile, _| alive_profiles.contains(profile));
        oxidom_core::sync::lock(&self.probes).retain_alive(&alive_servers, &alive_profiles);
    }

    /// Fold one liveness probe of a running tunnel into its failure streak and
    /// report whether the tunnel should now be considered dead.
    ///
    /// Only the server's own silence advances the count. A `Reachable` answer —
    /// however slow — wipes the streak, so a server merely slower than the probe
    /// budget never trips this. `NoNetwork` is this machine losing its uplink,
    /// not the server dropping, and a reconnect cannot mend it; `Internal`/`None`
    /// are our own failure to measure. None of those touch the count.
    fn note_tunnel_probe(&self, profile: &str, outcome: Option<&probe::ProbeOutcome>) -> bool {
        let mut failures = oxidom_core::sync::lock(&self.tunnel_failures);
        match outcome {
            Some(probe::ProbeOutcome::Timeout | probe::ProbeOutcome::Unreachable) => {
                let count = failures.entry(profile.to_string()).or_insert(0);
                *count += 1;
                if *count >= TUNNEL_DEATH_STRIKES {
                    failures.remove(profile);
                    true
                } else {
                    false
                }
            }
            Some(probe::ProbeOutcome::Reachable(_)) => {
                failures.remove(profile);
                false
            }
            _ => false,
        }
    }

    /// Revive a session whose server went silent while its core kept running.
    ///
    /// `note_core_deaths` only fires when the Xray *process* exits; a server that
    /// simply stops answering leaves the core alive with its inbound still bound,
    /// so nothing there would ever notice. This walks the same path by hand — kill
    /// the wedged core, mark it failed, drop the interface — then hands off to the
    /// ordinary reconnect loop, which retries with backoff until the server is
    /// back and `start_interface` re-marks the still-running child's cgroup.
    fn recover_dead_tunnel(&self, profile: &str) {
        let target = {
            let mut engine = oxidom_core::sync::lock(&self.engine);
            let Some(session) = engine.sessions.get_mut(profile) else {
                return;
            };
            let Some(target) = reconnect_target(session) else {
                return;
            };
            // The core still holds the profile's SOCKS port; a reconnect rebinds
            // the same port and `ensure_ports_free` refuses while the old inbound
            // lingers. Kill it before failing so the port is free, and so
            // `fail` — which leaves the selection intact — is what `begin_reconnect`
            // then sees.
            session.core.disconnect();
            session.core.fail(CORE_EXITED_MESSAGE);
            session.balancer_info = None;
            if let Err(error) = engine.stop_interface(profile) {
                log::warn!(
                    "could not clean the interface while reviving profile {profile:?}: {error:#}"
                );
            }
            engine.sessions.release_system_proxy(profile);
            target
        };
        if let Some(generation) = self.current_generation(profile) {
            self.begin_reconnect(profile.to_string(), target, generation);
        }
    }

    /// Periodic re-probe of the active server; keeps the latency reading
    /// fresh but never tears an established connection down.
    fn spawn_active_probe_loop(&self) {
        let shared = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(ACTIVE_PROBE_INTERVAL);
                let active = {
                    let mut engine = oxidom_core::sync::lock(&shared.engine);
                    let profiles = engine
                        .sessions
                        .iter()
                        .map(|(profile, _)| profile.to_string())
                        .collect::<Vec<_>>();
                    let mut active = Vec::new();
                    for profile in profiles {
                        let Some(session) = engine.sessions.get_mut(&profile) else {
                            continue;
                        };
                        if session.status() == Status::Connected && session.is_alive() {
                            let label = session
                                .server_id()
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("pool:{profile}"));
                            active.push((profile, label));
                        }
                    }
                    active
                };
                for (profile, label) in active {
                    // The queue refuses a profile already running or waiting,
                    // so a slow session cannot pile up copies of itself.
                    shared.enqueue_session_probe(profile, label);
                }
            }
        });
    }

    /// Counterpart to `spawn_active_probe_loop`: it refuses to tear a connection
    /// down, this one does — but only for a resilient session (an `oxidom run`
    /// cgroup, or the global `reconnect`), and only after `TUNNEL_DEATH_STRIKES`
    /// unbroken failures prove the server is gone rather than slow.
    fn spawn_tunnel_watchdog(&self) {
        let shared = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(ACTIVE_PROBE_INTERVAL);
                let watched = {
                    let mut engine = oxidom_core::sync::lock(&shared.engine);
                    let reconnect_all = engine.registry.config.reconnect;
                    let profiles = engine
                        .sessions
                        .iter()
                        .map(|(profile, _)| profile.to_string())
                        .collect::<Vec<_>>();
                    let mut watched = Vec::new();
                    for profile in profiles {
                        let Some(session) = engine.sessions.get_mut(&profile) else {
                            continue;
                        };
                        if session.status() == Status::Connected
                            && session.is_alive()
                            && (reconnect_all || session_has_run_cgroup(session))
                        {
                            watched.push(profile);
                        }
                    }
                    watched
                };
                for profile in watched {
                    let outcome = shared
                        .session_probe_outcome(&profile)
                        .map(|(outcome, _)| outcome);
                    if shared.note_tunnel_probe(&profile, outcome.as_ref()) {
                        log::warn!(
                            "profile {profile:?} stopped answering while its core is alive; \
                             reviving it"
                        );
                        shared.recover_dead_tunnel(&profile);
                    }
                }
            }
        });
    }

    fn spawn_pool_status_loop(&self) {
        let shared = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POOL_POLL_INTERVAL);
                let profiles = {
                    let engine = oxidom_core::sync::lock(&shared.engine);
                    engine
                        .sessions
                        .iter()
                        .filter(|(_, session)| {
                            matches!(&session.selection, Some(SessionSelection::Pool { .. }))
                        })
                        .map(|(profile, _)| profile.to_string())
                        .collect::<Vec<_>>()
                };

                for profile_name in profiles {
                    let profile_query = profile::load(&profile_name)
                        .ok()
                        .and_then(|profile| profile.select.pool);
                    let api_task = {
                        let mut engine = oxidom_core::sync::lock(&shared.engine);
                        let selection = engine
                            .sessions
                            .get(&profile_name)
                            .and_then(|session| session.selection.as_ref());
                        let stale = pool_membership_stale(
                            profile_query.as_ref(),
                            selection,
                            &engine.registry.subscriptions,
                        );

                        let xray_binary = engine.registry.config.xray_binary.clone();
                        let Some(session) = engine.sessions.get_mut(&profile_name) else {
                            continue;
                        };
                        session.pool_stale = stale;
                        let due = session.status() == Status::Connected
                            && session.api_port != 0
                            && session
                                .balancer_polled_at
                                .is_none_or(|last| last.elapsed() >= BALANCER_INFO_TTL);
                        if !due {
                            None
                        } else {
                            session.balancer_polled_at = Some(Instant::now());
                            Some((
                                session.selection.clone(),
                                xray_binary,
                                SocketAddrV4::new(session.address, session.api_port),
                                session.api_port,
                            ))
                        }
                    };
                    let Some((selection, xray_binary, api, api_port)) = api_task else {
                        continue;
                    };
                    let result =
                        oxidom_core::xray::resolve::resolve(&xray_binary).and_then(|resolved| {
                            oxidom_core::xray::api::balancer_info(
                                &resolved.path,
                                api,
                                "pool",
                                BALANCER_API_TIMEOUT,
                            )
                        });
                    let mut engine = oxidom_core::sync::lock(&shared.engine);
                    let Some(session) = engine.sessions.get_mut(&profile_name) else {
                        continue;
                    };
                    if session.selection == selection && session.api_port == api_port {
                        session.balancer_info = result.ok();
                    }
                }
            }
        });
    }
}

pub(crate) struct Service {
    pub(crate) shared: Shared,
}

impl Service {
    fn stop_profile(&self, profile: &str) -> fdo::Result<bool> {
        self.shared.clear_override(profile);
        self.shared.invalidate_generation(profile);
        let stopped = oxidom_core::sync::lock(&self.shared.engine)
            .remove_session(profile)
            .map_err(failed)?;
        self.shared.prune_readings();
        Ok(stopped)
    }

    fn stop_all_profiles(&self) -> fdo::Result<()> {
        self.shared.clear_all_overrides();
        self.shared.invalidate_all_generations();
        oxidom_core::sync::lock(&self.shared.engine)
            .disconnect_all()
            .map(|_| ())
            .map_err(failed)?;
        self.shared.prune_readings();
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn for_test() -> Service {
    Service {
        shared: Shared::new(Engine::load(), false, false, false),
    }
}

/// A daemon started the way the service unit starts it: with both inbound
/// ports fixed on the command line.
#[cfg(test)]
pub(crate) fn for_test_with_pinned_ports() -> Service {
    Service {
        shared: Shared::new(Engine::load(), true, true, false),
    }
}

/// `{:#}` keeps anyhow's cause chain; `to_string()` would send only the
/// outermost context ("spawning xray") and drop the reason it failed.
fn failed(error: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(format!("{error:#}"))
}

/// Translate the prober's richer local outcome into the stable wire failure.
/// A failed reading that carries the local reason, when there is one. A
/// server that simply did not answer has nothing to add; a machine that could
/// not run the check does, and that is the difference the card shows.
fn wire_reading(
    outcome: &probe::ProbeOutcome,
    route: ProbeRoute,
    method: LatencyMethod,
) -> LatencyReading {
    match outcome {
        probe::ProbeOutcome::Internal(detail) => {
            LatencyReading::failed_locally(wire_failure(outcome), *detail, route, method)
        }
        _ => LatencyReading::failed(wire_failure(outcome), route, method),
    }
}

fn wire_failure(outcome: &probe::ProbeOutcome) -> ProbeFailure {
    match outcome {
        probe::ProbeOutcome::Unreachable => ProbeFailure::Unreachable,
        probe::ProbeOutcome::Timeout => ProbeFailure::Timeout,
        probe::ProbeOutcome::NoNetwork => ProbeFailure::NoNetwork,
        probe::ProbeOutcome::Internal(detail) => {
            log::warn!("probe could not run: {}", detail.message());
            ProbeFailure::Unknown
        }
        // Only failed outcomes are passed here. Keep this total so an
        // accidental misuse degrades to an honest unknown instead of taking
        // the daemon down.
        probe::ProbeOutcome::Reachable(_) => ProbeFailure::Unknown,
    }
}

fn pool_probe_method(requested: LatencyMethod) -> (&'static str, LatencyMethod) {
    if requested == LatencyMethod::HttpHead {
        ("HEAD", LatencyMethod::HttpHead)
    } else {
        ("GET", LatencyMethod::HttpGet)
    }
}

fn json<T: serde::Serialize>(value: &T) -> fdo::Result<String> {
    serde_json::to_string(value).map_err(failed)
}

fn session_info(
    engine: &Engine,
    profile: &str,
    session: &oxidom_core::engine::Session,
    override_status: Option<&ErrorOverride>,
) -> SessionInfo {
    let (status, selection) = match override_status {
        Some(failure) => (&failure.status, Some(failure.selection.clone())),
        None => {
            let status = session.status();
            return session_info_with_status(engine, profile, session, &status, None);
        }
    };
    session_info_with_status(engine, profile, session, status, selection)
}

fn session_info_with_status(
    engine: &Engine,
    profile: &str,
    session: &oxidom_core::engine::Session,
    status: &Status,
    override_selection: Option<SessionSelection>,
) -> SessionInfo {
    let (state, error) = match status {
        Status::Disconnected => ("disconnected", None),
        Status::Connecting => ("connecting", None),
        Status::Connected => ("connected", None),
        Status::Error(message) => ("error", Some(message.clone())),
    };
    let selection = override_selection.or_else(|| session.selection.clone());
    let server_id = selection
        .as_ref()
        .and_then(SessionSelection::server_id)
        .map(str::to_string);
    let server = server_id
        .as_deref()
        .and_then(|server_id| engine.all_servers().find(|server| server.id == server_id));
    SessionInfo {
        profile: profile.to_string(),
        state: state.to_string(),
        error,
        server_id,
        server_alias: server.and_then(|server| server.alias.clone()),
        server_name: server.map(|server| server.name.clone()),
        address: session.address.to_string(),
        socks_port: session.socks_port,
        http_port: session.http_port,
        owns_system_proxy: engine.sessions.owner_of_system_proxy() == Some(profile),
        interface: session.interface.as_ref().map(|interface| InterfaceInfo {
            device: interface.device.clone(),
            address: interface.address.to_string(),
            mtu: interface.mtu,
            routes: interface.route_mode().to_string(),
            table: interface.table,
            mark: interface.mark,
            up: interface.up,
        }),
        selection: selection_info(engine, session, selection.as_ref()),
    }
}

fn selection_info(
    engine: &Engine,
    session: &oxidom_core::engine::Session,
    selection: Option<&SessionSelection>,
) -> SelectionInfo {
    match selection {
        Some(SessionSelection::Server(_)) => SelectionInfo {
            kind: "server".to_string(),
            ..SelectionInfo::default()
        },
        Some(SessionSelection::Pool {
            members, strategy, ..
        }) => {
            let live = session.balancer_info.as_ref();
            // `principleTarget` is the set the balancer is rotating through
            // right now, and that is all it is. Measured on Xray 26.3.27:
            // `roundRobin` lists every member including unreachable ones,
            // `leastLoad` lists the reachable ones it selected, `leastPing`
            // lists its single winner. So this maps to "in rotation" for every
            // strategy — and only a strategy that settles on one node may have
            // that node called the current exit.
            let picks_one = strategy == "leastPing";
            // Counted here rather than snapshotted at `up` precisely because
            // this loop already looks every member up: the lookup is the cost,
            // and a `Status` that pays it anyway may as well answer the pool's
            // least honest number. Members that no subscription holds any more
            // contribute no endpoint, so a shrunken pool understates rather
            // than invents.
            let mut exits: Vec<(String, u16)> = Vec::new();
            let pool_members = members
                .iter()
                .map(|server_id| {
                    let server = engine.find_server(server_id);
                    if let Some(server) = server.as_ref() {
                        let exit = (server.address.clone(), server.port);
                        if !exits.contains(&exit) {
                            exits.push(exit);
                        }
                    }
                    let alias = server.as_ref().and_then(|server| server.alias.clone());
                    let tag = format!("s-{}", alias.as_deref().unwrap_or(server_id.as_str()));
                    let in_rotation = match live {
                        Some(info) if info.override_target.is_none() => {
                            Some(info.principle.iter().any(|live_tag| live_tag == &tag))
                        }
                        // An override pins one target and says nothing about
                        // what the strategy would otherwise be rotating.
                        _ => None,
                    };
                    PoolMember {
                        server_id: server_id.clone(),
                        alias,
                        name: server
                            .as_ref()
                            .map(|server| server.name.clone())
                            .unwrap_or_else(|| server_id.clone()),
                        tag,
                        in_rotation,
                    }
                })
                .collect::<Vec<_>>();
            // A single current exit exists only when something picked one: an
            // override, or a picking strategy. Naming one node "now" under
            // roundRobin would describe a rotation that never stopped.
            let current_tag = live.and_then(|info| match &info.override_target {
                Some(target) => Some(target.as_str()),
                None if !picks_one => None,
                None => (info.principle.len() == 1).then(|| info.principle[0].as_str()),
            });
            let selecting = current_tag.and_then(|tag| {
                pool_members
                    .iter()
                    .find(|member| member.tag == tag)
                    .map(|member| {
                        member
                            .alias
                            .clone()
                            .unwrap_or_else(|| member.server_id.clone())
                    })
            });
            SelectionInfo {
                kind: "pool".to_string(),
                name: session.pool_name.clone(),
                strategy: strategy.clone(),
                members: pool_members,
                endpoints: exits.len(),
                selecting,
                stale: session.pool_stale,
            }
        }
        None => SelectionInfo::default(),
    }
}

fn pool_membership_stale(
    query: Option<&oxidom_core::pool::PoolQuery>,
    selection: Option<&SessionSelection>,
    subscriptions: &[oxidom_core::model::Subscription],
) -> bool {
    let (Some(query), Some(SessionSelection::Pool { fingerprint, .. })) = (query, selection) else {
        // A running session deliberately survives its profile file or a later
        // switch to single-server selection.
        return false;
    };
    oxidom_core::pool::resolve(query, subscriptions)
        .map(|members| {
            let ids = members
                .iter()
                .map(|server| server.id.clone())
                .collect::<Vec<_>>();
            pool_fingerprint(&ids) != *fingerprint
        })
        // An empty current result differs from the non-empty membership that
        // was accepted at `up`, even though `resolve` reports it as an error.
        .unwrap_or(true)
}

/// How each candidate for an ambiguous handle should be spelled back at the
/// user: the alias they can retype, or the id when the server has none yet.
fn candidate_list(candidates: &[&Server]) -> String {
    candidates
        .iter()
        .map(|server| server.alias.as_deref().unwrap_or(server.id.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[zbus::interface(name = "dev.keepinfov.oxidom1")]
impl Service {
    fn list_subscriptions(&self) -> fdo::Result<String> {
        let engine = oxidom_core::sync::lock(&self.shared.engine);
        json(&engine.registry.subscriptions)
    }

    fn add_subscription(&self, url: String, name: String, send_hwid: bool) -> fdo::Result<()> {
        let name = (!name.is_empty()).then_some(name);
        let result =
            oxidom_core::sync::lock(&self.shared.engine).add_subscription(url, name, send_hwid);
        // After the failures too: a refresh that errors part-way through has
        // still replaced some of the list.
        self.shared.prune_readings();
        result.map_err(failed)
    }

    fn remove_subscription(&self, subscription_id: String) -> fdo::Result<bool> {
        let result =
            oxidom_core::sync::lock(&self.shared.engine).remove_subscription(&subscription_id);
        self.shared.prune_readings();
        result.map_err(failed)
    }

    fn refresh(&self, subscription_id: String) -> fdo::Result<()> {
        let result = oxidom_core::sync::lock(&self.shared.engine).refresh(&subscription_id);
        self.shared.prune_readings();
        result.map_err(failed)
    }

    fn refresh_all(&self) -> fdo::Result<()> {
        let result = oxidom_core::sync::lock(&self.shared.engine).refresh_all();
        self.shared.prune_readings();
        result.map_err(failed)
    }

    fn import_links(&self, text: String) -> fdo::Result<(u32, u32)> {
        let result = oxidom_core::sync::lock(&self.shared.engine).import_links(&text);
        self.shared.prune_readings();
        let (added, unsupported) = result.map_err(failed)?;
        Ok((added as u32, unsupported as u32))
    }

    fn remove_server(&self, server_id: String) -> fdo::Result<bool> {
        let result = oxidom_core::sync::lock(&self.shared.engine).remove_server(&server_id);
        self.shared.prune_readings();
        result.map_err(failed)
    }

    fn set_server_alias(&self, server_id: String, alias: String) -> fdo::Result<()> {
        if !alias::is_valid(&alias) {
            return Err(failed(
                "alias must be 1-32 lowercase letters, digits, or hyphens, start with a \
                 letter or digit, and not be exactly 16 hexadecimal characters",
            ));
        }
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        let target = engine
            .registry
            .subscriptions
            .iter()
            .enumerate()
            .find_map(|(subscription_index, subscription)| {
                subscription
                    .servers
                    .iter()
                    .position(|server| server.id == server_id)
                    .map(|server_index| (subscription_index, server_index))
            })
            .ok_or_else(|| failed("server not found"))?;
        let alias_taken = engine
            .registry
            .subscriptions
            .iter()
            .enumerate()
            .flat_map(|(subscription_index, subscription)| {
                subscription
                    .servers
                    .iter()
                    .enumerate()
                    .map(move |(server_index, server)| ((subscription_index, server_index), server))
            })
            .any(|(position, server)| {
                position != target && server.alias.as_deref() == Some(alias.as_str())
            });
        if alias_taken {
            return Err(failed(format!("alias {alias:?} is already in use")));
        }
        engine.registry.subscriptions[target.0].servers[target.1].alias = Some(alias);
        engine.save().map_err(failed)
    }

    /// Read the certificate a server presents, without judging it.
    ///
    /// Two steps rather than one — inspect, then trust — so the value stored is
    /// the one the user was shown. Fetching again inside a `trust` call would
    /// pin whatever the second handshake returned, which is precisely the
    /// substitution a pin exists to prevent.
    fn inspect_certificate(&self, server_id: String) -> fdo::Result<String> {
        let (address, port, sni) = {
            let engine = oxidom_core::sync::lock(&self.shared.engine);
            let server = engine
                .registry
                .subscriptions
                .iter()
                .flat_map(|subscription| subscription.servers.iter())
                .find(|server| server.id == server_id)
                .ok_or_else(|| failed("server not found"))?;
            let sni = server
                .spec
                .stream()
                .and_then(|stream| stream.sni.clone())
                .filter(|sni| !sni.is_empty());
            (server.address.clone(), server.port, sni)
        };
        // The engine lock is released first: this is a network round trip, and
        // holding it here would stall every other client for its duration.
        let presented =
            oxidom_core::tls_pin::present(&address, port, sni.as_deref()).map_err(failed)?;
        Ok(presented.sha256)
    }

    /// Pin `sha256` for this server, so its certificate is accepted from now on
    /// and no other is.
    ///
    /// This is what replaces `allowInsecure`, which Xray 26 removed: that
    /// accepted any certificate on every connection, while a pin accepts one
    /// certificate the user looked at.
    fn trust_certificate(&self, server_id: String, sha256: String) -> fdo::Result<()> {
        let pin = sha256.trim().to_ascii_lowercase();
        if pin.len() != 64 || !pin.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(failed(
                "a certificate pin is 64 hexadecimal characters (SHA-256 of the certificate)",
            ));
        }
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        let server = engine
            .registry
            .subscriptions
            .iter_mut()
            .flat_map(|subscription| subscription.servers.iter_mut())
            .find(|server| server.id == server_id)
            .ok_or_else(|| failed("server not found"))?;
        let stream = server
            .spec
            .stream_mut()
            .ok_or_else(|| failed("this server has no TLS settings to pin a certificate on"))?;
        stream.pin_sha256 = Some(pin);
        engine.save().map_err(failed)
    }

    fn set_hwid(&self, subscription_id: String, enabled: bool) -> fdo::Result<()> {
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        if let Some(subscription) = engine
            .registry
            .subscriptions
            .iter_mut()
            .find(|subscription| subscription.id == subscription_id)
        {
            subscription.send_hwid = enabled;
        }
        engine.save().map_err(failed)
    }

    /// Set or clear one subscription's User-Agent override. An empty string
    /// clears it, so the global `subscription_user_agent` applies again.
    ///
    /// This does not refetch: a panel keys its response format on the
    /// User-Agent, so the new value has to be in place *before* the next
    /// refresh, and refreshing here would hide which of the two actions failed.
    fn set_subscription_user_agent(
        &self,
        subscription_id: String,
        user_agent: String,
    ) -> fdo::Result<()> {
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        let Some(subscription) = engine
            .registry
            .subscriptions
            .iter_mut()
            .find(|subscription| subscription.id == subscription_id)
        else {
            return Err(failed(format!("unknown subscription {subscription_id:?}")));
        };
        subscription.user_agent = oxidom_core::subscription::user_agent_override(&user_agent);
        engine.save().map_err(failed)
    }

    fn connect(&self, server_id: String) -> fdo::Result<()> {
        // Bare Connect predates profiles and must keep working for a user who
        // has no `default.toml`: it is read here only to learn whether an
        // interface was requested.
        let default_profile = profile::load_or_default("default").map_err(failed)?;
        default_profile.validate("default").map_err(failed)?;
        self.shared.clear_override("default");
        let generation = self.shared.next_connect_generation("default");
        {
            let mut engine = oxidom_core::sync::lock(&self.shared.engine);
            let socks_port = engine.registry.config.socks_port;
            let http_port = engine.registry.config.http_port;
            if engine.registry.config.system_proxy
                && let Some(owner) = engine.sessions.owner_of_system_proxy()
                && owner != "default"
            {
                return Err(failed(format!(
                    "the system proxy is already held by profile {owner:?}"
                )));
            }
            let existed = engine.sessions.get("default").is_some();
            // Resolved before the session is prepared so an unknown id cannot
            // leave a half-built session behind.
            let server = engine
                .find_server(&server_id)
                .ok_or_else(|| failed("server not found"))?;
            engine
                .prepare_session("default", Ipv4Addr::LOCALHOST, socks_port, http_port)
                .map_err(failed)?;
            engine.configure_core("default", &default_profile.core);
            if let Err(error) = engine.configure_interface(
                "default",
                &default_profile.interface,
                std::slice::from_ref(&server),
            ) {
                if !existed {
                    engine.sessions.remove("default");
                }
                return Err(failed(error));
            }
            if engine.registry.config.system_proxy {
                engine
                    .sessions
                    .claim_system_proxy("default")
                    .map_err(failed)?;
            }
        }

        let result = self.shared.start_connection(
            "default",
            &server_id,
            generation,
            ConnectionOrigin::Explicit,
        );
        // A death noticed just before this explicit operation may have queued
        // its reconnect override. The user's action owns the visible state.
        self.shared.clear_override("default");
        result.map(|_| ()).map_err(failed)
    }

    fn disconnect(&self) -> fdo::Result<()> {
        self.stop_profile("default")?;
        Ok(())
    }

    fn list_profiles(&self) -> fdo::Result<String> {
        let names = profile::list().map_err(failed)?;
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            // One unreadable file must not hide every other profile: the CLI
            // uses this to tell the user what they can bring up.
            match profile::load(&name) {
                Ok(profile) => entries.push(ProfileEntry {
                    name,
                    description: profile.description,
                    server: profile.select.server,
                    socks_port: profile.proxy.socks_port,
                    http_port: profile.proxy.http_port,
                    interface: profile.interface,
                    pool: profile.select.pool,
                    core: profile.core,
                }),
                Err(error) => log::warn!("skipping profile {name:?}: {error:#}"),
            }
        }
        json(&entries)
    }

    fn get_profile(&self, name: String) -> fdo::Result<String> {
        json(&profile::load(&name).map_err(failed)?)
    }

    fn save_profile(&self, name: String, profile_json: String) -> fdo::Result<()> {
        let profile: Profile = serde_json::from_str(&profile_json).map_err(failed)?;
        // `save` re-checks both the name and the ports. The daemon owns the
        // profiles directory — on the system bus it is root's — so validation
        // that only ran in the caller would be no validation at all.
        profile::save(&name, &profile).map_err(failed)
    }

    fn remove_profile(&self, name: String) -> fdo::Result<bool> {
        // The session/profile association deliberately survives the file: a
        // tunnel this profile brought up is still its tunnel, and `oxidom down
        // <profile>` (i.e. the unit's ExecStop) has to keep taking it down.
        profile::remove(&name).map_err(failed)
    }

    fn up_profile(&self, name: String) -> fdo::Result<String> {
        let profile = profile::load(&name).map_err(failed)?;
        // Only files written through `SaveProfile` were validated on the way
        // in; this one may have been edited by hand since.
        profile.validate(&name).map_err(failed)?;
        if profile.select.server.is_empty() && profile.select.pool.is_none() {
            return Err(failed(format!(
                "profile {name:?} does not name a server yet; set select.server to an alias or id"
            )));
        }

        // Snapshotted before the engine lock rather than under it: this is the
        // only place the two are wanted together, and taking them in one order
        // here and the other elsewhere is how a deadlock gets written.
        let readings = oxidom_core::sync::lock(&self.shared.readings).clone();

        // Resolve and apply everything that needs the engine, then drop the
        // guard: `start_connection` takes the same lock itself.
        let (target, up_server, ignored_ports) = {
            let mut engine = oxidom_core::sync::lock(&self.shared.engine);
            if engine.sessions.get(&name).is_some() {
                return Err(failed(format!(
                    "profile {name:?} is already up; run `oxidom down {name}` first"
                )));
            }
            let (socks_port, http_port, ignored_ports) =
                self.shared
                    .reconcile_profile_ports(&engine.registry.config, &name, &profile);
            if socks_port == http_port {
                return Err(failed(
                    "the profile's SOCKS and HTTP inbounds would share a port",
                ));
            }

            let wants_system_proxy = engine.registry.config.system_proxy;
            if wants_system_proxy
                && let Some(owner) = engine.sessions.owner_of_system_proxy()
                && owner != name
            {
                return Err(failed(format!(
                    "the system proxy is already held by profile {owner:?}"
                )));
            }
            let address = oxidom_core::bind::address_for(&name, &engine.sessions.taken_addresses())
                .ok_or_else(|| failed("no free profile loopback addresses remain"))?;

            let (target, servers, up_server) = if let Some(query) = &profile.select.pool {
                let composites =
                    oxidom_core::pool::excluded_composites(query, &engine.registry.subscriptions);
                if !composites.is_empty() {
                    log::warn!(
                        "profile {name:?} pool omitted composite Xray profiles: {}",
                        composites
                            .iter()
                            .map(|server| format!(
                                "{} ({})",
                                server.name,
                                server.alias.as_deref().unwrap_or(&server.id)
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                // A frozen list is expected to shrink as servers go away, so a
                // handle nobody holds any more is a note, not a failure. Said
                // once here, where a user can act on it, rather than by
                // `resolve`, which the GUI calls on every keystroke.
                let missing =
                    oxidom_core::pool::missing_members(query, &engine.registry.subscriptions);
                if !missing.is_empty() {
                    log::warn!(
                        "profile {name:?} pool lists {} server(s) no subscription holds any more: {}",
                        missing.len(),
                        missing.join(", ")
                    );
                }
                // Ranked, not merely resolved: `max` truncates, and the first
                // member is the exit the balancer uses until the observatory
                // reports. Subscription order gave both jobs to whichever
                // entry the provider happened to list first.
                let resolved = oxidom_core::pool::resolve_ranked(
                    query,
                    &engine.registry.subscriptions,
                    |server| known_state(&readings, &server.id),
                )
                .map_err(failed)?;
                if resolved.len() > MAX_POOL_MEMBERS {
                    return Err(failed(format!(
                        "profile {name:?} pool resolves to {} nodes; set [select.pool] max to \
                         {MAX_POOL_MEMBERS} or less",
                        resolved.len()
                    )));
                }
                // A pool spreads traffic over exit addresses, not over
                // subscription entries. Providers list the same host many times
                // — 26 of 42 German entries shared one `address:port` on the
                // store this was found on — and a pool that says "42 nodes"
                // while owning 9 addresses overstates itself nearly fivefold.
                // Reported here, once, rather than by `resolve`, which the GUI
                // calls on every keystroke.
                let hosts = oxidom_core::pool::distinct_endpoints(&resolved);
                if hosts < resolved.len() {
                    log::warn!(
                        "profile {name:?} pool has {} member(s) on only {hosts} exit address(es); \
                         traffic can spread no wider than that",
                        resolved.len()
                    );
                }
                let servers = resolved.into_iter().cloned().collect::<Vec<_>>();
                let members = servers
                    .iter()
                    .map(|server| server.id.clone())
                    .collect::<Vec<_>>();
                let api_port = oxidom_core::engine::free_port(address, &[socks_port, http_port])
                    .map_err(failed)?;
                let expected = query.expected_or_all(members.len());
                (
                    ConnectionTarget::Pool {
                        members,
                        name: query.name.clone(),
                        strategy: query.strategy.as_xray().to_string(),
                        expected,
                        probe_interval: query.probe_interval_or_default().to_string(),
                        api_port,
                    },
                    servers,
                    UpServer::default(),
                )
            } else {
                let server = match handle::resolve(engine.all_servers(), &profile.select.server) {
                    HandleMatch::One(server) => server.clone(),
                    HandleMatch::None => {
                        return Err(failed(format!(
                            "profile {name:?} names no server this daemon knows: {:?}",
                            profile.select.server
                        )));
                    }
                    HandleMatch::Ambiguous(candidates) => {
                        return Err(failed(format!(
                            "{:?} matches {} servers ({}); use an alias or an id",
                            profile.select.server,
                            candidates.len(),
                            candidate_list(&candidates)
                        )));
                    }
                };
                (
                    ConnectionTarget::Server(server.id.clone()),
                    vec![server.clone()],
                    UpServer {
                        id: server.id,
                        alias: server.alias,
                        name: server.name,
                    },
                )
            };

            if name == "default"
                && (engine.registry.config.socks_port != socks_port
                    || engine.registry.config.http_port != http_port)
            {
                engine.registry.config.socks_port = socks_port;
                engine.registry.config.http_port = http_port;
                engine.save().map_err(failed)?;
            }
            engine
                .prepare_session(&name, address, socks_port, http_port)
                .map_err(failed)?;
            engine.configure_core(&name, &profile.core);
            if let Err(error) = engine.configure_interface(&name, &profile.interface, &servers) {
                engine.sessions.remove(&name);
                return Err(failed(error));
            }
            if wants_system_proxy {
                engine.sessions.claim_system_proxy(&name).map_err(failed)?;
            }
            (target, up_server, ignored_ports)
        };

        self.shared.clear_override(&name);
        let generation = self.shared.next_connect_generation(&name);
        let confirmation = self.shared.start_connection_target(
            &name,
            &target,
            generation,
            ConnectionOrigin::Explicit,
        );
        self.shared.clear_override(&name);
        let confirmation = confirmation.map_err(failed)?;
        if profile.interface.enable
            && !confirmation.is_some_and(|confirmation| confirmation.recv().unwrap_or(false))
        {
            let message = self
                .shared
                .session_info(&name)
                .and_then(|session| session.error)
                .unwrap_or_else(|| {
                    format!("profile {name:?} did not finish bringing up its interface")
                });
            return Err(failed(message));
        }

        json(&UpResult {
            server: up_server,
            ignored_ports,
            warnings: (profile.interface.routes == RouteMode::Default)
                .then(|| {
                    "routes = \"default\" does not move the system resolver into the tunnel; DNS \
                     may still use the existing network path"
                        .to_string()
                })
                .into_iter()
                .collect(),
        })
    }

    fn down(&self, profile: String) -> fdo::Result<bool> {
        if profile.is_empty() {
            // The old single-session method remains unconditional. With
            // sessions that means bringing every profile down.
            self.stop_all_profiles()?;
            return Ok(true);
        }
        self.stop_profile(&profile)
    }

    fn list_sessions(&self) -> fdo::Result<String> {
        json(&self.shared.list_session_infos())
    }

    fn session_status(&self, profile: String) -> fdo::Result<String> {
        let session = self
            .shared
            .session_info(&profile)
            .ok_or_else(|| failed(format!("profile {profile:?} is not up")))?;
        json(&session)
    }

    fn down_profile(&self, profile: String) -> fdo::Result<bool> {
        self.stop_profile(&profile)
    }

    fn delete_interface(&self, profile: String) -> fdo::Result<bool> {
        oxidom_core::sync::lock(&self.shared.engine)
            .delete_interface(&profile)
            .map_err(failed)
    }

    fn mark_cgroup(&self, profile: String, uid: u32) -> fdo::Result<String> {
        let slice = oxidom_core::sync::lock(&self.shared.engine)
            .mark_cgroup(&profile, uid)
            .map_err(failed)?;
        json(&slice)
    }

    fn status(&self) -> fdo::Result<String> {
        json(&self.shared.status_info())
    }

    fn request_probe(&self, server_id: String) -> fdo::Result<()> {
        self.shared.enqueue_probe(server_id);
        Ok(())
    }

    fn request_probes(&self, server_ids: Vec<String>) -> fdo::Result<()> {
        for id in server_ids {
            self.shared.enqueue_probe(id);
        }
        Ok(())
    }

    fn probe_state(&self) -> fdo::Result<String> {
        let (running, queued) = oxidom_core::sync::lock(&self.shared.probes).snapshot();
        let state = ProbeState {
            version: PROBE_STATE_VERSION,
            running,
            queued,
            readings: oxidom_core::sync::lock(&self.shared.readings).clone(),
            proxied: oxidom_core::sync::lock(&self.shared.proxied).clone(),
        };
        json(&state)
    }

    fn get_settings(&self) -> fdo::Result<String> {
        json(&oxidom_core::sync::lock(&self.shared.engine).registry.config)
    }

    fn set_settings(&self, config_json: String) -> fdo::Result<String> {
        let raw: serde_json::Value = serde_json::from_str(&config_json).map_err(failed)?;
        let mut config: Config = serde_json::from_value(raw.clone()).map_err(failed)?;
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);

        // A GUI older than this key sends a payload without it; treat that as
        // "leave it alone" rather than clearing the path the daemon may need
        // to start xray at all.
        //
        // On the system bus the same key is a remote-exec primitive: the
        // daemon runs as its own user and spawns whatever path this names, so
        // callers do not get to choose it. There the core comes from the unit's
        // environment or the on-disk config, both of which need root to touch.
        // Two lists, because the two refusals have different reasons and the
        // GUI has to name the right one: a port is pinned by the service unit,
        // a path is refused outright on the system bus.
        let mut ignored_ports = Vec::new();
        let mut ignored_paths = Vec::new();
        if raw.get("xray_binary").is_none() || self.shared.system_bus {
            if self.shared.system_bus && config.xray_binary != engine.registry.config.xray_binary {
                ignored_paths.push("Xray binary path".to_string());
            }
            config.xray_binary = engine.registry.config.xray_binary.clone();
        }
        // Preserve an opt-in made directly in config.toml when an older GUI,
        // which has no checkbox for this key, applies its settings payload.
        if raw.get("reconnect").is_none() {
            config.reconnect = engine.registry.config.reconnect;
        }
        // Same, for the whole `[core]` section. A client that edits it always
        // sends the key, empty section included; one that does not know the
        // section omits it, and overwriting from that absence would erase a
        // hand-written fragment setting on the next Apply click.
        if raw.get("core").is_none() {
            config.core = engine.registry.config.core.clone();
        }
        if raw.get("tun2socks_binary").is_none() || self.shared.system_bus {
            if self.shared.system_bus
                && config.tun2socks_binary != engine.registry.config.tun2socks_binary
            {
                ignored_paths.push("tun2socks binary path".to_string());
            }
            config.tun2socks_binary = engine.registry.config.tun2socks_binary.clone();
        }
        if raw.get("nft_binary").is_none() || self.shared.system_bus {
            if self.shared.system_bus && config.nft_binary != engine.registry.config.nft_binary {
                ignored_paths.push("nft binary path".to_string());
            }
            config.nft_binary = engine.registry.config.nft_binary.clone();
        }

        // Rejected here as well as in the GUI: any D-Bus client can send a
        // config, and a zero or colliding port produces a core that fails to
        // start with a far less obvious error than this one.
        if config.socks_port == 0 || config.http_port == 0 {
            return Err(failed("ports must be between 1 and 65535"));
        }
        if config.socks_port == config.http_port {
            return Err(failed("the SOCKS and HTTP inbounds cannot share a port"));
        }
        // Same reasoning one level down: the core takes a reversed fragment
        // range or an out-of-band mux concurrency without a word and then does
        // nothing with it, so a bad value would look like a working setting.
        if let Err(error) = config.core.validate("core") {
            return Err(failed(format!("{error:#}")));
        }

        // Ports fixed on the command line by the service unit are refused
        // here, not just greyed out in the GUI: accepting the write would move
        // the inbound until the next restart silently put it back, breaking
        // anything pointed at the old port in the meantime.
        if self.shared.socks_port_locked && config.socks_port != engine.registry.config.socks_port {
            config.socks_port = engine.registry.config.socks_port;
            ignored_ports.push("SOCKS port".to_string());
        }
        if self.shared.http_port_locked && config.http_port != engine.registry.config.http_port {
            config.http_port = engine.registry.config.http_port;
            ignored_ports.push("HTTP port".to_string());
        }

        let ports_changed = engine.registry.config.socks_port != config.socks_port
            || engine.registry.config.http_port != config.http_port;
        let compatibility_profile = engine.default_session().and_then(|session| {
            (session.status() == Status::Connected).then(|| session.profile.clone())
        });
        if config.system_proxy {
            if let Some(profile) = compatibility_profile.as_deref() {
                engine
                    .sessions
                    .claim_system_proxy(profile)
                    .map_err(failed)?;
            }
        } else if let Some(owner) = engine.sessions.owner_of_system_proxy().map(str::to_string) {
            engine.sessions.release_system_proxy(&owner);
        }
        engine.registry.config = config;
        let socks_port = engine.registry.config.socks_port;
        let http_port = engine.registry.config.http_port;
        let xray_binary = engine.registry.config.xray_binary.clone();
        engine.set_default_ports(socks_port, http_port);
        if let Some(session) = engine.default_session_mut() {
            session.core.xray_binary = xray_binary;
        }
        engine.save().map_err(failed)?;
        let mut reconnect_error = None;
        let mut confirmation = None;
        if ports_changed && engine.status() == Status::Connected {
            // Same treatment as a user-driven connect: the restarted core has
            // to prove the new inbound is up before this counts as connected.
            let target = engine.sessions.get("default").and_then(|session| {
                match session.selection.as_ref()? {
                    SessionSelection::Server(server_id) => {
                        Some(ConnectionTarget::Server(server_id.clone()))
                    }
                    SessionSelection::Pool {
                        members, strategy, ..
                    } => Some(ConnectionTarget::Pool {
                        members: members.clone(),
                        name: session.pool_name.clone(),
                        strategy: strategy.clone(),
                        expected: session.pool_expected,
                        probe_interval: session.pool_probe_interval.clone(),
                        api_port: session.api_port,
                    }),
                }
            });
            if let Some(target) = target {
                let generation = self.shared.next_connect_generation("default");
                let reconnect = match &target {
                    ConnectionTarget::Server(server_id) => {
                        engine.connect_session("default", server_id)
                    }
                    ConnectionTarget::Pool {
                        members,
                        name,
                        strategy,
                        expected,
                        probe_interval,
                        api_port,
                    } => engine.connect_pool_session(
                        "default",
                        members,
                        &PoolSession {
                            name,
                            strategy,
                            expected: *expected,
                            probe_interval,
                            api_port: *api_port,
                        },
                    ),
                };
                match reconnect {
                    Ok(()) => confirmation = Some((target, generation)),
                    Err(error) => reconnect_error = Some(format!("{error:#}")),
                }
            }
        }
        drop(engine);
        if let Some((target, generation)) = confirmation {
            let probe_label = target.probe_label("default");
            let probe_token = oxidom_core::sync::lock(&self.shared.probes)
                .start_now("default", &probe_label)
                .token;
            self.shared.confirm_connection(
                "default".to_string(),
                target,
                generation,
                ConnectionOrigin::Explicit,
                probe_token,
            );
        }
        json(&ApplySettingsResult {
            reconnect_error,
            ignored_ports,
            ignored_paths,
        })
    }

    fn runtime_info(&self) -> fdo::Result<String> {
        json(&self.shared.runtime_info())
    }

    /// Kept for clients older than [`Self::logs_since`].
    ///
    /// Now served from the process-wide book, so it finally answers for **every**
    /// session. It used to read `default_session()` alone, which meant the logs
    /// of any other profile were unreachable by the CLI, the GUI and the bus
    /// alike — the one place a failure explains itself, blank.
    fn recent_logs(&self) -> fdo::Result<Vec<String>> {
        Ok(oxidom_core::logbook::global().legacy_lines())
    }

    /// Structured log records after `after_seq`, at most `limit` of them.
    ///
    /// The cursor is the point: [`Self::recent_logs`] hands over the whole
    /// buffer every time it is called, so a reader cannot tell new lines from a
    /// reshuffled log and has to redraw. Passing what it has already seen makes
    /// a refresh an append.
    fn logs_since(&self, after_seq: u64, limit: u32) -> fdo::Result<String> {
        let limit = if limit == 0 {
            oxidom_core::logbook::DEFAULT_LIMIT
        } else {
            (limit as usize).min(oxidom_core::logbook::CAPACITY)
        };
        json(&oxidom_core::logbook::global().since(after_seq, limit))
    }

    fn clear_logs(&self) -> fdo::Result<()> {
        oxidom_core::logbook::global().clear();
        Ok(())
    }
}

fn spawn_core_supervisor(shared: Shared) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(CORE_WATCH_INTERVAL);
            for (profile, target) in shared.note_core_deaths() {
                if let Some(generation) = shared.current_generation(&profile) {
                    shared.begin_reconnect(profile, target, generation);
                }
            }
        }
    });
}

pub fn run(options: DaemonOptions) -> Result<()> {
    // Block SIGINT/SIGTERM before anything spawns a thread: zbus serves on its own
    // workers, and only threads created after the mask is set inherit it. Waiting
    // with sigwait then costs no main loop at all.
    let mut stop = SigSet::empty();
    stop.add(Signal::SIGINT);
    stop.add(Signal::SIGTERM);
    stop.thread_block()?;

    let mut engine = Engine::load();
    for warning in engine.registry.load_warnings.drain(..) {
        log::warn!("{warning}");
    }
    if let Some(port) = options.socks_port {
        engine.registry.config.socks_port = port;
    }
    if let Some(port) = options.http_port {
        engine.registry.config.http_port = port;
    }
    engine.set_default_ports(
        engine.registry.config.socks_port,
        engine.registry.config.http_port,
    );

    // Report the core up front: `journalctl -u oxidom` should show a missing
    // binary before anyone clicks Connect and wonders why it failed.
    let resolved = oxidom_core::xray::resolve::resolve(&engine.registry.config.xray_binary);
    match resolved {
        Ok(resolved) => log::info!(
            "using the Xray core at {} (from {})",
            resolved.path.display(),
            resolved.source.label(&oxidom_core::xray::resolve::XRAY)
        ),
        Err(error) => log::warn!("no usable Xray core: {error:#}"),
    }

    // Seeded from the running config, so `oxidom up` works on a fresh install
    // without the user first writing a profile by hand. Never fatal: a daemon
    // that refuses to start because it could not write an example profile is
    // worse than one with no profiles.
    if let Err(error) = profile::ensure_default(&engine) {
        log::warn!("could not create the default profile: {error:#}");
    }

    let shared = Shared::new(
        engine,
        options.socks_port.is_some(),
        options.http_port.is_some(),
        options.system_bus,
    );
    shared.spawn_active_probe_loop();
    shared.spawn_tunnel_watchdog();
    shared.spawn_pool_status_loop();
    spawn_core_supervisor(shared.clone());

    let service = Service {
        shared: shared.clone(),
    };
    let builder = if options.system_bus {
        zbus::blocking::connection::Builder::system().context("connecting to the system bus")?
    } else {
        zbus::blocking::connection::Builder::session().context("connecting to the session bus")?
    };
    let _connection = builder
        .name(BUS_NAME)
        .context("claiming the bus name (is another oxidom daemon running?)")?
        .serve_at(OBJECT_PATH, service)
        .context("registering the service object")?
        .build()
        .context("starting the D-Bus service")?;
    log::info!(
        "oxidom daemon serving {BUS_NAME} on the {} bus",
        if options.system_bus {
            "system"
        } else {
            "session"
        }
    );

    loop {
        match stop.wait() {
            Ok(Signal::SIGINT | Signal::SIGTERM) => break,
            Ok(_) => continue,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => {
                log::error!("sigwait failed: {error}");
                break;
            }
        }
    }

    if let Err(error) = oxidom_core::sync::lock(&shared.engine).disconnect_all() {
        log::warn!("could not persist the clean daemon shutdown: {error:#}");
    }
    log::info!("oxidom daemon stopped");
    Ok(())
}

/// What the last direct probe says about a candidate, for pool ordering.
///
/// Only a `Direct` reading describes the *server*; a `Proxied` one describes a
/// tunnel that happened to be carrying it. And a probe that never left this
/// machine (`NoNetwork`) or that the prober could not classify (`Unknown`) is
/// this machine's failure, so ranking the server below one nobody measured
/// would launder our own outage into a verdict on somebody's node.
fn known_state(readings: &HashMap<String, LatencyReading>, server_id: &str) -> pool::Known {
    let Some(reading) = readings.get(server_id) else {
        return pool::Known::Unmeasured;
    };
    if reading.route != ProbeRoute::Direct {
        return pool::Known::Unmeasured;
    }
    match (reading.value, reading.failure) {
        (Some(ms), _) => pool::Known::Reachable(ms),
        (None, Some(ProbeFailure::Unreachable | ProbeFailure::Timeout)) => pool::Known::Unreachable,
        (None, _) => pool::Known::Unmeasured,
    }
}

/// Why a connection that started could not be confirmed.
///
/// `rotating` is how many members the balancer had in rotation when the window
/// closed, when that could be read at all.
///
/// Kept pure and separate because this string is the only account the user gets
/// of a session that was torn down: the core's own status goes with it.
fn confirmation_failure(
    target: &ConnectionTarget,
    inbound_ready: bool,
    logs: &[String],
    rotating: Option<usize>,
) -> String {
    // A core that rejected the config exited at once, so both the dead inbound
    // and the failed probe are symptoms. Say the actual cause.
    if core_rejected_the_protocol(logs) {
        return format!(
            "the core does not support this server's protocol — {}",
            oxidom_core::xray::core::HYSTERIA2_CORE_HINT
        );
    }
    // The core says why it would not carry traffic, in the same words it uses
    // when a probe asks. Without this the user is told the server "did not pass
    // its latency check" — true, and useless, when the cause is a certificate
    // this build was never going to accept.
    if let Some(detail) = logs
        .iter()
        .find_map(|line| oxidom_core::probe::classify_complaint(line))
    {
        return detail.message().to_string();
    }
    if !inbound_ready {
        return "the local SOCKS inbound never came up — the core is not carrying traffic"
            .to_string();
    }
    // A pool has no active server by construction, so naming one would describe
    // a thing that does not exist. Report what was tried and how much of it the
    // balancer had in rotation.
    let ConnectionTarget::Pool { members, .. } = target else {
        return "active server did not pass its latency check".to_string();
    };
    let seen = match rotating {
        Some(live) => format!("{live} of {} nodes were in rotation", members.len()),
        None => format!("none of its {} nodes answered", members.len()),
    };
    format!(
        "the pool carried no traffic within {}s — {seen}",
        POOL_CONFIRM_WINDOW.as_secs()
    )
}

/// Whether the core's own output says it could not build the outbound at all —
/// which is what an Xray older than the hysteria2 support does.
fn core_rejected_the_protocol(logs: &[String]) -> bool {
    logs.iter().any(|line| {
        oxidom_core::xray::core::UNSUPPORTED_PROTOCOL_MARKERS
            .iter()
            .any(|marker| line.contains(marker))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidom_core::bind;

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TestRoot {
        path: std::path::PathBuf,
    }

    impl TestRoot {
        fn install(label: &str) -> Result<Self> {
            let suffix = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "oxidom-test-{label}-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path)
                .with_context(|| format!("creating test root {}", path.display()))?;
            oxidom_core::paths::set_test_root(Some(path.clone()));
            Ok(Self { path })
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            oxidom_core::paths::set_test_root(None);
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn pool_target(members: &[&str]) -> ConnectionTarget {
        ConnectionTarget::Pool {
            members: members.iter().map(|name| name.to_string()).collect(),
            name: "Germany".to_string(),
            strategy: "leastLoad".to_string(),
            expected: 0,
            probe_interval: "5m".to_string(),
            api_port: 18080,
        }
    }

    #[test]
    fn a_pool_that_failed_to_confirm_counts_nodes_instead_of_naming_a_server() {
        // `Session::server_id()` returns `None` for a pool on purpose, so a
        // message about "the active server" would describe something that does
        // not exist.
        let target = pool_target(&["one", "two", "three"]);
        assert_eq!(
            confirmation_failure(&target, true, &[], Some(0)),
            "the pool carried no traffic within 20s — 0 of 3 nodes were in rotation"
        );
        assert_eq!(
            confirmation_failure(&target, true, &[], Some(1)),
            "the pool carried no traffic within 20s — 1 of 3 nodes were in rotation"
        );
        // The balancer could not be asked at all — claiming "0 in rotation"
        // would report a measurement nobody took.
        assert_eq!(
            confirmation_failure(&target, true, &[], None),
            "the pool carried no traffic within 20s — none of its 3 nodes answered"
        );
    }

    #[test]
    fn a_dead_inbound_and_a_rejected_protocol_outrank_the_selection() {
        let target = pool_target(&["one"]);
        // Nothing bound, so nothing was measured: the pool count would be a
        // conclusion drawn from an experiment that never ran.
        assert_eq!(
            confirmation_failure(&target, false, &[], None),
            "the local SOCKS inbound never came up — the core is not carrying traffic"
        );
        let logs = vec![format!(
            "failed to parse outbound: {}",
            oxidom_core::xray::core::UNSUPPORTED_PROTOCOL_MARKERS[0]
        )];
        assert!(
            confirmation_failure(&target, false, &logs, None)
                .contains("does not support this server's protocol")
        );
        // A single server keeps the wording it had before pools existed.
        assert_eq!(
            confirmation_failure(
                &ConnectionTarget::Server("one".to_string()),
                true,
                &[],
                None
            ),
            "active server did not pass its latency check"
        );
    }

    /// oxidom's own warning about an unrecognised obfuscation type contains the
    /// words `unknown` and `protocol`, and the hysteria2 hint it is emitted
    /// beside names the protocol too. Neither is the core refusing anything —
    /// but both land in the same book as the core's output, so the lines fed to
    /// this check come from [`Session::current_run_logs`], which admits only
    /// `LogSource::Xray` from the current run. Were that filter dropped, a
    /// perfectly good server would be reported as one the core cannot speak.
    #[test]
    fn oxidoms_own_notes_are_never_mistaken_for_the_core_refusing_a_protocol() {
        let ours = vec![
            "ignoring unknown \"quux\" obfuscation — Xray only implements salamander, and an \
             unknown type stops the core from starting"
                .to_string(),
            oxidom_core::xray::core::HYSTERIA2_CORE_HINT.to_string(),
            "starting the core for profile work".to_string(),
        ];
        assert!(
            !core_rejected_the_protocol(&ours),
            "an oxidom note must not read as the core rejecting the outbound"
        );

        // The same check still fires on what the core itself says.
        let theirs = vec![format!(
            "failed to parse outbound: {}",
            oxidom_core::xray::core::UNSUPPORTED_PROTOCOL_MARKERS[0]
        )];
        assert!(core_rejected_the_protocol(&theirs));
    }

    fn poison<T: Send + 'static>(target: Arc<Mutex<T>>) {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::thread::spawn(move || {
            let _guard = oxidom_core::sync::lock(&target);
            panic!("intentional mutex poison");
        })
        .join();
        std::panic::set_hook(previous_hook);
        assert!(result.is_err());
    }

    fn enqueue_direct(queue: &mut ProbeQueue, server_id: &str) -> bool {
        queue.enqueue(
            ProbeTarget::Direct(server_id.to_string()),
            server_id.to_string(),
        )
    }

    fn drain_slots(queue: &mut ProbeQueue) -> Vec<ProbeJob> {
        std::iter::from_fn(|| queue.start_next()).collect()
    }

    fn mark_active(engine: &mut Engine, profile: &str, server_id: &str) -> Result<()> {
        let address = bind::address_for(profile, &engine.sessions.taken_addresses())
            .context("allocating test address")?;
        let socks_port = engine.registry.config.socks_port;
        let http_port = engine.registry.config.http_port;
        engine.prepare_session(profile, address, socks_port, http_port)?;
        engine
            .sessions
            .get_mut(profile)
            .context("test session is absent")?
            .selection = Some(SessionSelection::Server(server_id.to_string()));
        Ok(())
    }

    fn mark_pool(engine: &mut Engine, profile: &str, members: &[&str]) -> Result<()> {
        let address = bind::address_for(profile, &engine.sessions.taken_addresses())
            .context("allocating test address")?;
        let socks_port = engine.registry.config.socks_port;
        let http_port = engine.registry.config.http_port;
        engine.prepare_session(profile, address, socks_port, http_port)?;
        let session = engine
            .sessions
            .get_mut(profile)
            .context("test session is absent")?;
        session.selection = Some(SessionSelection::pool(
            members.iter().map(|member| (*member).to_string()).collect(),
            "roundRobin".to_string(),
        ));
        session.api_port = 18082;
        session.pool_probe_interval = "5m".to_string();
        Ok(())
    }

    #[test]
    fn backoff_climbs_and_caps_at_thirty_seconds() {
        let delays: Vec<u64> = (0..=6)
            .map(|attempt| reconnect_delay(attempt).as_secs())
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
    }

    #[test]
    fn pool_probe_reports_the_http_method_it_actually_uses() {
        assert_eq!(
            pool_probe_method(LatencyMethod::HttpHead),
            ("HEAD", LatencyMethod::HttpHead)
        );
        for requested in [
            LatencyMethod::HttpGet,
            LatencyMethod::Tcp,
            LatencyMethod::Icmp,
        ] {
            assert_eq!(
                pool_probe_method(requested),
                ("GET", LatencyMethod::HttpGet)
            );
        }
    }

    #[test]
    fn a_newer_generation_cancels_a_pending_reconnect() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("cancel-reconnect")?;
        let service = for_test();
        let stale_generation = service.shared.next_connect_generation("default");
        service.shared.next_connect_generation("default");

        assert!(
            service
                .shared
                .start_reconnect_attempt("default", "never-started", stale_generation)
                .is_none()
        );
        assert_eq!(
            oxidom_core::sync::lock(&service.shared.engine).status(),
            Status::Disconnected
        );
        Ok(())
    }

    #[test]
    fn an_old_settings_payload_keeps_newer_daemon_only_values() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("old-settings-client")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.reconnect = true;
            engine.registry.config.tun2socks_binary = "/configured/tun2socks".to_string();
            engine.registry.config.nft_binary = "/configured/nft".to_string();
        }
        let mut raw = serde_json::to_value(Config::default())?;
        let object = raw
            .as_object_mut()
            .context("serialized config is not an object")?;
        object.remove("reconnect");
        object.remove("tun2socks_binary");
        object.remove("nft_binary");

        service.set_settings(raw.to_string())?;

        let engine = oxidom_core::sync::lock(&service.shared.engine);
        assert!(engine.registry.config.reconnect);
        assert_eq!(
            engine.registry.config.tun2socks_binary,
            "/configured/tun2socks"
        );
        assert_eq!(engine.registry.config.nft_binary, "/configured/nft");
        Ok(())
    }

    #[test]
    fn a_dead_core_is_noticed_without_a_status_call() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("dead-core")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            mark_active(&mut engine, "default", "dead-server")?;
            let session = engine.default_session_mut().context("default session")?;
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
            assert!(!session.core.is_alive());
        }

        assert_eq!(
            service.shared.note_core_deaths(),
            vec![(
                "default".to_string(),
                ConnectionTarget::Server("dead-server".to_string())
            )]
        );
        assert!(matches!(
            oxidom_core::sync::lock(&service.shared.engine).status(),
            Status::Error(message) if message == "Xray exited unexpectedly"
        ));
        assert!(service.shared.note_core_deaths().is_empty());
        Ok(())
    }

    #[test]
    fn a_dead_pool_keeps_its_full_reconnect_target() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("dead-pool")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            mark_pool(&mut engine, "spread", &["one", "two"])?;
            let session = engine.sessions.get("spread").context("pool session")?;
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
        }

        let deaths = service.shared.note_core_deaths();
        assert!(matches!(
            deaths.as_slice(),
            [(
                profile,
                ConnectionTarget::Pool {
                    members,
                    name: _,
                    strategy,
                    expected: _,
                    probe_interval,
                    api_port: 18082,
                },
            )] if profile == "spread"
                && members == &["one".to_string(), "two".to_string()]
                && strategy == "roundRobin"
                && probe_interval == "5m"
        ));
        assert!(matches!(
            oxidom_core::sync::lock(&service.shared.engine)
                .sessions
                .get("spread")
                .map(|session| session.status()),
            Some(Status::Error(message)) if message == CORE_EXITED_MESSAGE
        ));
        Ok(())
    }

    fn reachable() -> probe::ProbeOutcome {
        probe::ProbeOutcome::Reachable(probe::Measurement {
            ms: 42,
            method: LatencyMethod::HttpGet,
        })
    }

    #[test]
    fn a_dead_tunnel_is_declared_only_after_an_unbroken_run_of_failures() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("tunnel-strikes")?;
        let service = for_test();
        let shared = &service.shared;

        // One short of the threshold never fires.
        for _ in 0..TUNNEL_DEATH_STRIKES - 1 {
            assert!(!shared.note_tunnel_probe("default", Some(&probe::ProbeOutcome::Timeout)));
        }
        // A single reachable answer — however slow — wipes the streak, so the
        // count has to start over from scratch.
        assert!(!shared.note_tunnel_probe("default", Some(&reachable())));
        for _ in 0..TUNNEL_DEATH_STRIKES - 1 {
            assert!(!shared.note_tunnel_probe("default", Some(&probe::ProbeOutcome::Unreachable)));
        }
        // The strike that reaches the threshold is the one that declares it dead.
        assert!(shared.note_tunnel_probe("default", Some(&probe::ProbeOutcome::Unreachable)));
        // And the counter is cleared on the way out, so the next run starts fresh.
        assert!(!shared.note_tunnel_probe("default", Some(&probe::ProbeOutcome::Timeout)));
        Ok(())
    }

    #[test]
    fn neither_a_dead_network_nor_our_own_failure_counts_as_a_dead_server() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("tunnel-not-the-server")?;
        let service = for_test();
        let shared = &service.shared;

        // A machine with no uplink, our own inability to measure, and a vanished
        // session are none of them the server going quiet: a reconnect cannot
        // mend them, so they must never advance toward a teardown, even repeated
        // well past the threshold.
        for _ in 0..TUNNEL_DEATH_STRIKES + 2 {
            assert!(!shared.note_tunnel_probe("default", Some(&probe::ProbeOutcome::NoNetwork)));
            assert!(!shared.note_tunnel_probe(
                "default",
                Some(&probe::ProbeOutcome::Internal(
                    oxidom_core::ipc::ProbeDetail::NoCore
                ))
            ));
            assert!(!shared.note_tunnel_probe("default", None));
        }
        Ok(())
    }

    #[test]
    fn an_oxidom_run_session_is_resilient_even_with_reconnect_off() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("run-is-resilient")?;
        let service = for_test();
        let mut engine = oxidom_core::sync::lock(&service.shared.engine);
        engine.registry.config.reconnect = false;

        mark_active(&mut engine, "runner", "srv")?;
        // No cgroup yet: a plain connect keeps the deliberate off-by-default.
        assert!(!resilient(&engine, "runner"));

        engine.attach_run_cgroup_for_test("runner", 1000)?;
        // Carrying a run child is itself the opt-in.
        assert!(resilient(&engine, "runner"));

        // The global flag still turns it on for everyone else.
        mark_active(&mut engine, "plain", "srv")?;
        assert!(!resilient(&engine, "plain"));
        engine.registry.config.reconnect = true;
        assert!(resilient(&engine, "plain"));
        Ok(())
    }

    #[test]
    fn reviving_a_live_but_dead_tunnel_fails_the_core_and_keeps_the_selection() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("revive-live-tunnel")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            // reconnect off and no cgroup, so the handoff to begin_reconnect is a
            // no-op and the test stays hermetic — we assert only the local half.
            engine.registry.config.reconnect = false;
            mark_active(&mut engine, "default", "silent-server")?;
            let session = engine.default_session_mut().context("default session")?;
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
        }

        service.shared.recover_dead_tunnel("default");

        let engine = oxidom_core::sync::lock(&service.shared.engine);
        let session = engine.sessions.get("default").context("default session")?;
        assert!(matches!(
            session.status(),
            Status::Error(message) if message == CORE_EXITED_MESSAGE
        ));
        // The selection has to survive so begin_reconnect can aim at the same
        // server once it is reachable again.
        assert_eq!(
            session.selection,
            Some(SessionSelection::Server("silent-server".to_string()))
        );
        Ok(())
    }

    #[test]
    fn an_adopted_session_starts_inside_a_reconnect_generation() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("adopted-generation")?;
        let mut engine = Engine::load();
        mark_pool(&mut engine, "spread", &["one", "two"])?;
        let session = engine.sessions.get("spread").context("pool session")?;
        *oxidom_core::sync::lock(&session.core.status) = Status::Connected;

        let shared = Shared::new(engine, false, false, false);

        assert!(shared.current_generation("spread").is_some());
        Ok(())
    }

    #[test]
    fn every_dead_session_is_noticed_and_releases_the_system_proxy() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("all-dead-cores")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            for (profile, server) in [("home", "same"), ("work", "same")] {
                mark_active(&mut engine, profile, server)?;
                *oxidom_core::sync::lock(
                    &engine
                        .sessions
                        .get(profile)
                        .context("test session")?
                        .core
                        .status,
                ) = Status::Connected;
            }
            engine.sessions.claim_system_proxy("home")?;
        }

        assert_eq!(
            service.shared.note_core_deaths(),
            vec![
                (
                    "home".to_string(),
                    ConnectionTarget::Server("same".to_string())
                ),
                (
                    "work".to_string(),
                    ConnectionTarget::Server("same".to_string())
                ),
            ]
        );
        let engine = oxidom_core::sync::lock(&service.shared.engine);
        assert!(engine.sessions.owner_of_system_proxy().is_none());
        assert!(matches!(
            engine.sessions.get("home").map(|session| session.status()),
            Some(Status::Error(message)) if message == CORE_EXITED_MESSAGE
        ));
        drop(engine);
        assert!(service.shared.note_core_deaths().is_empty());
        Ok(())
    }

    #[test]
    fn a_panicking_worker_leaves_the_daemon_answering() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("poisoned-probes")?;
        let service = for_test();

        poison(service.shared.probes.clone());

        service.probe_state()?;
        service.status()?;
        service.list_subscriptions()?;
        let queue = oxidom_core::sync::lock(&service.shared.probes);
        let _ = queue.snapshot();
        Ok(())
    }

    #[test]
    fn a_poisoned_engine_lock_still_serves_status() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("poisoned-engine")?;
        let service = for_test();

        poison(service.shared.engine.clone());

        service.status()?;
        Ok(())
    }

    #[test]
    fn an_empty_root_gives_an_empty_but_valid_surface() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("empty-surface")?;
        let service = for_test();

        let status: StatusInfo = serde_json::from_str(&service.status()?)?;
        assert_eq!(status.state, "disconnected");
        let probes: ProbeState = serde_json::from_str(&service.probe_state()?)?;
        assert_eq!(probes.version, PROBE_STATE_VERSION);
        Ok(())
    }

    #[test]
    fn server_aliases_are_validated_and_globally_unique() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("server-alias")?;
        let service = for_test();
        let first = oxidom_core::link::parse_link("trojan://one@one.example:443#One")
            .context("parsing first test server")?;
        let first_id = first.id.clone();
        let second = oxidom_core::link::parse_link("trojan://two@two.example:443#Two")
            .context("parsing second test server")?;
        let second_id = second.id.clone();
        let mut subscription = oxidom_core::model::Subscription::new(
            "https://subscription.example".to_string(),
            Some("Test".to_string()),
        );
        subscription.servers = vec![first, second];
        oxidom_core::alias::assign(std::slice::from_mut(&mut subscription));
        oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .subscriptions
            .push(subscription);

        service.set_server_alias(first_id, "chosen".to_string())?;
        assert!(
            service
                .set_server_alias(second_id.clone(), "chosen".to_string())
                .is_err()
        );
        assert!(
            service
                .set_server_alias(second_id, "NOT-PORTABLE".to_string())
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn a_pinned_port_outranks_the_profile_and_is_reported() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("profile-pinned-ports")?;
        let service = for_test_with_pinned_ports();
        let config = oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .clone();
        let profile = Profile {
            proxy: oxidom_core::profile::ProfileProxy {
                socks_port: config.socks_port + 1000,
                http_port: config.http_port + 1000,
            },
            ..Profile::default()
        };

        let (socks_port, http_port, ignored) = service
            .shared
            .reconcile_profile_ports(&config, "default", &profile);

        assert_eq!(socks_port, config.socks_port);
        assert_eq!(http_port, config.http_port);
        assert_eq!(ignored, vec!["SOCKS port", "HTTP port"]);
        // The refusal is reported, not written: the running config keeps the
        // ports the unit gave it.
        let after = oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .clone();
        assert_eq!(after.socks_port, config.socks_port);
        assert_eq!(after.http_port, config.http_port);
        Ok(())
    }

    #[test]
    fn an_unpinned_daemon_takes_the_profile_ports_verbatim() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("profile-free-ports")?;
        let service = for_test();
        let config = oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .clone();
        let profile = Profile {
            proxy: oxidom_core::profile::ProfileProxy {
                socks_port: 21080,
                http_port: 21081,
            },
            ..Profile::default()
        };

        let (socks_port, http_port, ignored) = service
            .shared
            .reconcile_profile_ports(&config, "work", &profile);

        assert_eq!((socks_port, http_port), (21080, 21081));
        assert!(ignored.is_empty());
        Ok(())
    }

    #[test]
    fn unit_pins_do_not_override_a_non_default_profile() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("profile-non-default-pins")?;
        let service = for_test_with_pinned_ports();
        let config = oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .clone();
        let profile = Profile {
            proxy: oxidom_core::profile::ProfileProxy {
                socks_port: 21080,
                http_port: 21081,
            },
            ..Profile::default()
        };

        let (socks_port, http_port, ignored) = service
            .shared
            .reconcile_profile_ports(&config, "work", &profile);

        assert_eq!((socks_port, http_port), (21080, 21081));
        assert!(ignored.is_empty());
        Ok(())
    }

    #[test]
    fn sessions_are_listed_in_order_and_stopped_independently() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("sessions-list-down")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            mark_active(&mut engine, "work", "work-server")?;
            mark_active(&mut engine, "home", "home-server")?;
            *oxidom_core::sync::lock(
                &engine
                    .sessions
                    .get("work")
                    .context("work session")?
                    .core
                    .status,
            ) = Status::Connected;
            *oxidom_core::sync::lock(
                &engine
                    .sessions
                    .get("home")
                    .context("home session")?
                    .core
                    .status,
            ) = Status::Connected;
            engine.sessions.claim_system_proxy("home")?;
        }

        let sessions: Vec<SessionInfo> = serde_json::from_str(&service.list_sessions()?)?;
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.profile.as_str())
                .collect::<Vec<_>>(),
            ["home", "work"]
        );
        assert!(sessions[0].owns_system_proxy);
        assert!(!sessions[1].owns_system_proxy);
        assert!(service.down_profile("work".to_string())?);
        assert!(
            oxidom_core::sync::lock(&service.shared.engine)
                .sessions
                .get("home")
                .is_some()
        );
        assert!(
            oxidom_core::sync::lock(&service.shared.engine)
                .sessions
                .get("work")
                .is_none()
        );
        assert!(service.down_profile("home".to_string())?);
        assert!(
            oxidom_core::sync::lock(&service.shared.engine)
                .sessions
                .owner_of_system_proxy()
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn status_keeps_the_legacy_view_and_adds_all_sessions() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("sessions-status-compat")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            mark_active(&mut engine, "work", "work-server")?;
            mark_active(&mut engine, "home", "home-server")?;
            *oxidom_core::sync::lock(
                &engine
                    .sessions
                    .get("home")
                    .context("home session")?
                    .core
                    .status,
            ) = Status::Connecting;
        }

        let status: StatusInfo = serde_json::from_str(&service.status()?)?;
        assert_eq!(status.state, "connecting");
        assert_eq!(status.active_profile.as_deref(), Some("home"));
        assert_eq!(status.active_id.as_deref(), Some("home-server"));
        assert_eq!(status.sessions.len(), 2);
        Ok(())
    }

    #[test]
    fn pool_session_info_never_fills_the_legacy_server_fields() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("pool-session-info")?;
        let service = for_test();
        let mut subscription = oxidom_core::model::Subscription::new(
            "https://subscription.example".to_string(),
            Some("Test".to_string()),
        );
        subscription.servers = [
            "trojan://one@one.example:443#One",
            "vless://two@two.example:443#Two",
        ]
        .iter()
        .map(|link| oxidom_core::link::parse_link(link).context("parsing test server"))
        .collect::<Result<Vec<_>>>()?;
        oxidom_core::alias::assign(std::slice::from_mut(&mut subscription));
        let ids = subscription
            .servers
            .iter()
            .map(|server| server.id.clone())
            .collect::<Vec<_>>();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.subscriptions.push(subscription);
            mark_pool(&mut engine, "spread", &[&ids[0], &ids[1]])?;
            let first_tag = format!(
                "s-{}",
                engine
                    .find_server(&ids[0])
                    .and_then(|server| server.alias)
                    .context("first alias")?
            );
            let session = engine.sessions.get_mut("spread").context("pool session")?;
            session.balancer_info = Some(oxidom_core::xray::api::BalancerInfo {
                tag: "pool".to_string(),
                principle: vec![first_tag],
                override_target: None,
            });
        }

        let session: SessionInfo =
            serde_json::from_str(&service.session_status("spread".to_string())?)?;
        assert!(session.server_id.is_none());
        assert!(session.server_alias.is_none());
        assert!(session.server_name.is_none());
        assert_eq!(session.selection.kind, "pool");
        assert_eq!(session.selection.strategy, "roundRobin");
        assert_eq!(session.selection.members.len(), 2);
        // roundRobin keeps rotating, so there is no "now" node to name. The
        // rotation membership is still reported — but note it says nothing
        // about reachability under this strategy, which is why the field is
        // called `in_rotation`.
        assert_eq!(session.selection.selecting, None);
        assert_eq!(session.selection.members[0].in_rotation, Some(true));
        assert_eq!(session.selection.members[1].in_rotation, Some(false));
        Ok(())
    }

    /// A picking strategy answers with its winner. Naming that winner is the
    /// whole point; the others are simply out of rotation, not dead.
    #[test]
    fn a_least_ping_pool_names_its_pick_without_condemning_the_rest() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("pool-least-ping")?;
        let service = for_test();
        let mut subscription = oxidom_core::model::Subscription::new(
            "https://subscription.example".to_string(),
            Some("Test".to_string()),
        );
        subscription.servers = [
            "trojan://one@one.example:443#One",
            "vless://two@two.example:443#Two",
        ]
        .iter()
        .map(|link| oxidom_core::link::parse_link(link).context("parsing test server"))
        .collect::<Result<Vec<_>>>()?;
        oxidom_core::alias::assign(std::slice::from_mut(&mut subscription));
        let ids = subscription
            .servers
            .iter()
            .map(|server| server.id.clone())
            .collect::<Vec<_>>();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.subscriptions.push(subscription);
            mark_pool(&mut engine, "spread", &[&ids[0], &ids[1]])?;
            let second_tag = format!(
                "s-{}",
                engine
                    .find_server(&ids[1])
                    .and_then(|server| server.alias)
                    .context("second alias")?
            );
            let session = engine.sessions.get_mut("spread").context("pool session")?;
            if let Some(SessionSelection::Pool { strategy, .. }) = session.selection.as_mut() {
                "leastPing".clone_into(strategy);
            }
            session.balancer_info = Some(oxidom_core::xray::api::BalancerInfo {
                tag: "pool".to_string(),
                principle: vec![second_tag],
                override_target: None,
            });
        }

        let session: SessionInfo =
            serde_json::from_str(&service.session_status("spread".to_string())?)?;
        assert_eq!(session.selection.strategy, "leastPing");
        assert_eq!(
            session.selection.selecting,
            session.selection.members[1].alias
        );
        assert_eq!(session.selection.members[0].in_rotation, Some(false));
        assert_eq!(session.selection.members[1].in_rotation, Some(true));
        Ok(())
    }

    #[test]
    fn stale_uses_the_resolved_order_and_ignores_a_missing_profile() -> Result<()> {
        let mut subscription = oxidom_core::model::Subscription::new(
            "https://subscription.example".to_string(),
            Some("Test".to_string()),
        );
        subscription.servers = [
            "trojan://one@one.example:443#One",
            "vless://two@two.example:443#Two",
        ]
        .iter()
        .map(|link| oxidom_core::link::parse_link(link).context("parsing test server"))
        .collect::<Result<Vec<_>>>()?;
        let ids = subscription
            .servers
            .iter()
            .map(|server| server.id.clone())
            .collect::<Vec<_>>();
        let selection = SessionSelection::pool(ids.clone(), "roundRobin".to_string());
        let groups = [subscription];
        let query = oxidom_core::pool::PoolQuery::default();

        assert!(!pool_membership_stale(
            Some(&query),
            Some(&selection),
            &groups
        ));
        let changed = oxidom_core::pool::PoolQuery {
            exclude: vec![ids[0].clone()],
            ..query
        };
        assert!(pool_membership_stale(
            Some(&changed),
            Some(&selection),
            &groups
        ));
        assert!(!pool_membership_stale(None, Some(&selection), &groups));
        Ok(())
    }

    #[test]
    fn an_existing_profile_session_is_refused_before_starting_another_core() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("profile-already-up")?;
        let service = for_test();
        let profile = Profile {
            select: oxidom_core::profile::ProfileSelect {
                server: "anything".to_string(),
                pool: None,
            },
            ..Profile::default()
        };
        profile::save("work", &profile)?;
        mark_active(
            &mut oxidom_core::sync::lock(&service.shared.engine),
            "work",
            "server",
        )?;

        let error = service.up_profile("work".to_string()).unwrap_err();
        assert!(error.to_string().contains("profile \"work\" is already up"));
        Ok(())
    }

    #[test]
    fn a_second_system_proxy_owner_is_refused() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("profile-system-proxy-owner")?;
        let service = for_test();
        let server = oxidom_core::link::parse_link("trojan://secret@example.com:443#Example")
            .context("parsing server")?;
        let server_id = server.id.clone();
        let mut subscription = oxidom_core::model::Subscription::new(
            "https://subscription.example".to_string(),
            Some("Test".to_string()),
        );
        subscription.servers.push(server);
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.subscriptions.push(subscription);
            engine.registry.config.system_proxy = true;
            mark_active(&mut engine, "home", "home-server")?;
            engine.sessions.claim_system_proxy("home")?;
        }
        profile::save(
            "work",
            &Profile {
                select: oxidom_core::profile::ProfileSelect {
                    server: server_id,
                    pool: None,
                },
                ..Profile::default()
            },
        )?;

        let error = service.up_profile("work".to_string()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the system proxy is already held by profile \"home\"")
        );
        Ok(())
    }

    #[test]
    fn stopping_another_profile_leaves_this_one_running() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("profile-down-mismatch")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            mark_active(&mut engine, "home", "1111111111111111")?;
        }

        assert!(!service.down("work".to_string())?);

        // `systemctl stop oxidom@work` must be a no-op while `home` owns the
        // tunnel, so neither the profile nor the server may have been cleared.
        let engine = oxidom_core::sync::lock(&service.shared.engine);
        assert_eq!(engine.active_profile().as_deref(), Some("home"));
        assert_eq!(
            engine.active_server_id().as_deref(),
            Some("1111111111111111")
        );
        Ok(())
    }

    #[test]
    fn stopping_the_owning_profile_clears_it() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("profile-down-match")?;
        let service = for_test();
        mark_active(
            &mut oxidom_core::sync::lock(&service.shared.engine),
            "home",
            "1111111111111111",
        )?;

        assert!(service.down("home".to_string())?);

        assert!(
            oxidom_core::sync::lock(&service.shared.engine)
                .active_profile()
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn an_unnamed_stop_is_unconditional() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("profile-down-any")?;
        let service = for_test();
        mark_active(
            &mut oxidom_core::sync::lock(&service.shared.engine),
            "home",
            "1111111111111111",
        )?;

        assert!(service.down(String::new())?);

        assert!(
            oxidom_core::sync::lock(&service.shared.engine)
                .active_profile()
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn profiles_round_trip_through_the_daemon() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("profile-crud")?;
        let service = for_test();
        let profile = Profile {
            description: "работа".to_string(),
            select: oxidom_core::profile::ProfileSelect {
                server: "ch-trojan".to_string(),
                pool: None,
            },
            proxy: oxidom_core::profile::ProfileProxy {
                socks_port: 21080,
                http_port: 21081,
            },
            interface: Default::default(),
            core: Default::default(),
        };

        service.save_profile("work".to_string(), serde_json::to_string(&profile)?)?;
        let loaded: Profile = serde_json::from_str(&service.get_profile("work".to_string())?)?;
        assert_eq!(loaded, profile);

        let entries: Vec<ProfileEntry> = serde_json::from_str(&service.list_profiles()?)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "work");
        assert_eq!(entries[0].server, "ch-trojan");
        assert_eq!(entries[0].socks_port, 21080);

        // A name that would escape the profiles directory is refused by the
        // daemon, not merely by whoever called it.
        assert!(
            service
                .save_profile("../evil".to_string(), serde_json::to_string(&profile)?)
                .is_err()
        );
        assert!(
            service
                .save_profile("Work".to_string(), serde_json::to_string(&profile)?)
                .is_err()
        );

        assert!(service.remove_profile("work".to_string())?);
        assert!(!service.remove_profile("work".to_string())?);
        assert!(service.list_profiles()?.contains("[]"));
        Ok(())
    }

    #[test]
    fn a_local_fault_never_blames_the_server() {
        assert_eq!(
            wire_failure(&probe::ProbeOutcome::Internal(
                oxidom_core::ipc::ProbeDetail::Other
            )),
            ProbeFailure::Unknown
        );
    }

    /// The cap is what the GUI's queued spinners exist for: everything past it
    /// has been accepted but not measured, and must not read as finished.
    #[test]
    fn probes_past_the_cap_stay_queued() {
        let mut queue = ProbeQueue::default();
        for index in 0..MAX_CONCURRENT_PROBES + 3 {
            assert!(enqueue_direct(&mut queue, &format!("s{index}")));
        }
        let started = drain_slots(&mut queue);
        assert_eq!(started.len(), MAX_CONCURRENT_PROBES);

        let (running, queued) = queue.snapshot();
        assert_eq!(running.len(), MAX_CONCURRENT_PROBES);
        assert_eq!(queued.len(), 3);

        // A slot freeing up lets exactly one more through.
        queue.finish(started[0].token);
        assert!(queue.start_next().is_some());
        assert_eq!(queue.snapshot().1.len(), 2);
    }

    #[test]
    fn an_id_is_never_queued_twice() {
        let mut queue = ProbeQueue::default();
        assert!(enqueue_direct(&mut queue, "a"));
        assert!(!enqueue_direct(&mut queue, "a"), "already waiting");
        queue.start_next();
        assert!(!enqueue_direct(&mut queue, "a"), "already running");
        let token = queue.running.values().next().unwrap().token;
        queue.finish(token);
        assert!(enqueue_direct(&mut queue, "a"), "free to measure again");
    }

    #[test]
    fn two_profiles_on_one_server_get_independent_probe_jobs() {
        let mut queue = ProbeQueue::default();
        assert!(queue.enqueue(ProbeTarget::Proxied("home".to_string()), "same".to_string(),));
        assert!(queue.enqueue(ProbeTarget::Proxied("work".to_string()), "same".to_string(),));
        assert!(!queue.enqueue(ProbeTarget::Proxied("home".to_string()), "same".to_string(),));

        let jobs = drain_slots(&mut queue);
        assert_eq!(jobs.len(), 2);
        assert_ne!(jobs[0].target, jobs[1].target);
    }

    #[test]
    fn proxied_readings_cross_the_wire_independently_by_profile() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("proxied-reading-keys")?;
        let service = for_test();
        oxidom_core::sync::lock(&service.shared.proxied).extend([
            (
                "home".to_string(),
                LatencyReading::ok(41, ProbeRoute::Proxied, LatencyMethod::HttpGet),
            ),
            (
                "work".to_string(),
                LatencyReading::ok(83, ProbeRoute::Proxied, LatencyMethod::HttpGet),
            ),
        ]);

        let state: ProbeState = serde_json::from_str(&service.probe_state()?)?;

        assert_eq!(
            state.proxied.get("home").and_then(|reading| reading.value),
            Some(41)
        );
        assert_eq!(
            state.proxied.get("work").and_then(|reading| reading.value),
            Some(83)
        );
        assert!(state.readings.is_empty());
        Ok(())
    }

    /// The confirmation after a connect decides whether the tunnel the user is
    /// watching stays up, so it cannot wait behind a bulk re-check.
    #[test]
    fn a_confirmation_probe_jumps_the_queue_and_the_cap() {
        let mut queue = ProbeQueue::default();
        for index in 0..MAX_CONCURRENT_PROBES {
            enqueue_direct(&mut queue, &format!("s{index}"));
        }
        drain_slots(&mut queue);
        queue.enqueue(
            ProbeTarget::Proxied("default".to_string()),
            "active".to_string(),
        );

        queue.start_now("default", "active");
        let (running, queued) = queue.snapshot();
        assert!(running.contains(&"active".to_string()));
        assert!(!queued.contains(&"active".to_string()), "no double start");
    }

    /// A server deleted mid-sweep must not come back out of the queue a moment
    /// later and leave a reading for something that no longer exists.
    #[test]
    fn removed_servers_leave_the_queue() {
        let mut queue = ProbeQueue::default();
        enqueue_direct(&mut queue, "gone");
        enqueue_direct(&mut queue, "kept");
        queue.start_next();

        queue.retain_alive(&HashSet::from(["kept".to_string()]), &HashSet::new());
        let (_, queued) = queue.snapshot();
        assert_eq!(queued, vec!["kept".to_string()]);
    }

    /// A running probe owns a slot until its thread reports back; forgetting it
    /// early would hand the slot out twice.
    #[test]
    fn a_running_probe_keeps_its_slot_through_a_prune() {
        let mut queue = ProbeQueue::default();
        enqueue_direct(&mut queue, "gone");
        let started = queue.start_next().unwrap();

        queue.retain_alive(&HashSet::new(), &HashSet::new());
        assert_eq!(queue.snapshot().0, vec![started.server_id]);
    }
}
