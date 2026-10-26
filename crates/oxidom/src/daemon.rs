//! Headless daemon: owns the Engine (config, subscriptions, xray core,
//! probes) and exposes it on D-Bus. The GUI is a thin client; the tunnel
//! survives GUI closure; deployment determines user-manager/logout lifetime.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::sys::signal::{SigSet, Signal};
use zbus::fdo;

use oxidom_core::alias;
use oxidom_core::config::{Config, LatencyMethod, WatchdogMode};
use oxidom_core::diagnostics::{
    Context as DiagnosticContext, DiagnosticWatchMode, Disposition, Event, ObservationOutcome,
    Origin, RetryState, TargetKind, WatchAction,
};
use oxidom_core::engine::{
    Engine, PoolSession, SessionSelection, SubscriptionFetch, SubscriptionOperation,
    pool_fingerprint,
};
use oxidom_core::handle::{self, HandleMatch};
use oxidom_core::ipc::{
    ApplySettingsResult, BUS_NAME, InterfaceInfo, LatencyReading, OBJECT_PATH, PROBE_HISTORY_LIMIT,
    PROBE_STATE_VERSION, PoolMember, ProbeDetail, ProbeFailure, ProbeHistory, ProbeRoute,
    ProbeState, ProfileEntry, RuntimeInfo, SelectionInfo, SessionInfo, StatusInfo, UpResult,
    UpServer,
};
use oxidom_core::model::Server;
use oxidom_core::pool;
use oxidom_core::probe;
use oxidom_core::profile::{self, MAX_POOL_MEMBERS, Profile, RouteMode};
use oxidom_core::xray::core::Status;

mod lifecycle;
mod trace;
use trace::{Facts, Identities};

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
/// The legacy, explicitly aggressive live-core restart threshold.
const TUNNEL_DEATH_STRIKES: u32 = 3;

fn watchdog_policy(mode: WatchdogMode) -> Option<(Duration, Duration, u32)> {
    match mode {
        WatchdogMode::Off => None,
        WatchdogMode::Conservative => Some((Duration::from_secs(60), Duration::from_secs(8), 5)),
        WatchdogMode::Aggressive => Some((
            ACTIVE_PROBE_INTERVAL,
            Duration::from_secs(3),
            TUNNEL_DEATH_STRIKES,
        )),
    }
}

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
    /// The connect attempt a proxied job set out to measure, captured when the
    /// job was created. A reading may only land while this is still the
    /// profile's current generation; `None` never lands.
    generation: Option<u64>,
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
    fn enqueue(&mut self, target: ProbeTarget, server_id: String, generation: Option<u64>) -> bool {
        if self.holds(&target) {
            return false;
        }
        let job = self.job(target, server_id, generation);
        self.queued.push_back(job);
        true
    }

    fn holds(&self, target: &ProbeTarget) -> bool {
        self.running.values().any(|job| &job.target == target)
            || self.queued.iter().any(|job| &job.target == target)
    }

    fn job(&mut self, target: ProbeTarget, server_id: String, generation: Option<u64>) -> ProbeJob {
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
                    generation,
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
    fn start_now(&mut self, profile: &str, server_id: &str, generation: Option<u64>) -> ProbeJob {
        let target = ProbeTarget::Proxied(profile.to_string());
        self.queued.retain(|job| job.target != target);
        // A superseded confirmation may still be unwinding. Give the new one
        // its own token so the old worker cannot release the new worker's slot.
        let job = self.job(target, server_id.to_string(), generation);
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

    /// Drop queued direct probes for these servers, and report which ones went.
    ///
    /// `running` is left alone for the same reason `retain_alive` leaves it
    /// alone: each running job owns a thread that will `finish` it, and taking
    /// the entry out early hands a slot to a second worker while the first is
    /// still in it. A probe already measuring therefore runs to its end — at
    /// most eight of them, about ten seconds — while everything behind it is
    /// dropped at once. That is where the minutes are.
    ///
    /// Only `Direct` jobs are touched. A `Proxied` job is the confirmation that
    /// decides whether a live tunnel stays up, it is keyed by profile rather
    /// than by the server the user is looking at, and the active-probe loop
    /// would re-enqueue it within thirty seconds anyway. Cancelling one because
    /// its server happened to be named would be a different act than the one
    /// the button offers.
    fn cancel_direct(&mut self, servers: &HashSet<String>) -> Vec<ProbeJob> {
        let mut cancelled = Vec::new();
        self.queued.retain(|job| {
            let drop =
                matches!(&job.target, ProbeTarget::Direct(_)) && servers.contains(&job.server_id);
            if drop {
                cancelled.push(job.clone());
            }
            !drop
        });
        cancelled
    }

    /// Drop queued ids that are no longer backed by a server. `running` is left
    /// alone on purpose: each of those has a thread that will `finish` it, and
    /// removing the entry early would hand out a slot that is still occupied.
    fn retain_alive(&mut self, servers: &HashSet<String>, profiles: &HashSet<String>) {
        self.queued.retain(|job| match &job.target {
            ProbeTarget::Direct(_) => servers.contains(&job.server_id),
            // A pool's job is labelled `pool:<profile>` — a name no server
            // list will ever contain — so it lives and dies with its profile.
            ProbeTarget::Proxied(profile) => {
                profiles.contains(profile)
                    && (job.server_id == format!("pool:{profile}")
                        || servers.contains(&job.server_id))
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct WatchdogAttempt {
    generation: u64,
    spawn_seq: u64,
    core: Option<u64>,
    policy_epoch: u64,
    context: Option<DiagnosticContext>,
}

#[derive(Clone, Copy)]
struct Confirmation {
    generation: u64,
    core: Option<u64>,
    spawn_seq: u64,
    context: Option<DiagnosticContext>,
    inbound: Option<(Ipv4Addr, u16, LatencyMethod)>,
}

type PendingConfirmation = (mpsc::Receiver<bool>, Confirmation);
type PoolConfirmation = (Option<u32>, Option<probe::ProbeOutcome>, ConfirmationEnd);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfirmationEnd {
    Measured,
    InboundNotReady,
    Superseded,
    CoreStopped,
    PoolWindowExhausted,
}

impl WatchdogAttempt {
    fn subject(self) -> Confirmation {
        Confirmation {
            generation: self.generation,
            core: self.core,
            spawn_seq: self.spawn_seq,
            context: self.context,
            inbound: None,
        }
    }
}

struct TunnelStreak {
    attempt: WatchdogAttempt,
    mode: WatchdogMode,
    count: u32,
}

#[derive(Clone)]
pub(crate) struct Shared {
    lifecycle: lifecycle::Lifecycle,
    terminal_cleanup: Arc<Mutex<Option<Result<(), String>>>>,
    engine: Arc<Mutex<Engine>>,
    recorder: crate::diagnostics::Recorder,
    identities: Arc<Mutex<Identities>>,
    /// Couples application transitions to identity capture, never to persistence.
    /// Acquire before the engine; release before emission, which may reorder facts.
    diagnostic_order: Arc<Mutex<()>>,
    /// Queue network updates without occupying the engine or blocking executor threads.
    subscription_work: Arc<tokio::sync::Mutex<()>>,
    readings: Arc<Mutex<HashMap<String, LatencyReading>>>,
    /// The last few readings per server, newest first, for the one card that
    /// is open. Kept beside `readings` rather than inside it because the two
    /// are read on completely different schedules: `readings` crosses the wire
    /// twice a second for every server, this crosses it once when a card is
    /// expanded.
    history: Arc<Mutex<HashMap<String, Vec<LatencyReading>>>>,
    proxied: Arc<Mutex<HashMap<String, LatencyReading>>>,
    probes: Arc<Mutex<ProbeQueue>>,
    /// Layered over the core status, e.g. when the confirming probe after a
    /// connect fails and the daemon shuts the tunnel back down.
    override_status: Arc<Mutex<HashMap<String, ErrorOverride>>>,
    /// A failure streak belongs to one attempt and policy, never its replacement.
    tunnel_failures: Arc<Mutex<HashMap<String, TunnelStreak>>>,
    watchdog_epoch: Arc<AtomicU64>,
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
    /// Everything about the core's geo data: the cached verdict, and whatever a
    /// running download has reported so far.
    geo: Arc<Mutex<GeoState>>,
    /// What the core answered when asked its version, cached against the
    /// binary that answered.
    core_version: Arc<Mutex<CoreVersion>>,
    /// Set to stop a running download. Outside the mutex on purpose: `Cancel`
    /// must be reachable while the download thread holds the lock to publish
    /// progress, or cancelling would wait for the very loop it is stopping.
    geo_cancel: Arc<oxidom_core::xray::assets::Cancel>,
    /// Direct SOCKS5/HTTP listener when enabled and no session is up or holding
    /// traffic. Lock order is engine, override_status, standby.
    standby: Arc<Mutex<Option<oxidom_core::standby::StandbyProxy>>>,
    // Every service/worker clone keeps the store leased until its engine Arc
    // has been released. In particular, zbus may retain a completed task.
    store_lease: Option<Arc<crate::store_lock::StoreLock>>,
}

/// The daemon's memory of the geo data question.
///
/// The verdict is cached against the binary it was measured for: answering it
/// costs a subprocess, and `RuntimeInfo` is fetched on every Settings open.
#[derive(Default)]
pub(crate) struct GeoState {
    /// Which core the verdict below belongs to. A different one invalidates it.
    probed_for: Option<std::path::PathBuf>,
    usable: Option<bool>,
    error: Option<String>,
    downloading: bool,
    progress: oxidom_core::xray::assets::Progress,
    last_error: Option<String>,
    cancelled: bool,
}

/// The daemon's memory of the core's version.
///
/// Cached against the binary it was read from for the same reason the geo
/// verdict is: answering costs a subprocess, and `RuntimeInfo` is fetched
/// every time Settings opens. A core that could not be run is cached too —
/// `version: None` against the path that failed — so a broken binary is
/// spawned once rather than on every open.
#[derive(Default)]
pub(crate) struct CoreVersion {
    read_for: Option<std::path::PathBuf>,
    version: Option<String>,
}

impl Shared {
    fn admit(&self) -> fdo::Result<lifecycle::Work> {
        self.lifecycle
            .enter()
            .ok_or_else(|| failed("daemon is stopping"))
    }

    fn ensure_open(&self) -> fdo::Result<()> {
        if self.lifecycle.is_closed() {
            return Err(failed("daemon is stopping"));
        }
        Ok(())
    }

    fn spawn(&self, work: impl FnOnce() + Send + 'static) -> bool {
        let Some(ticket) = self.lifecycle.enter() else {
            return false;
        };
        let lifecycle = self.lifecycle.clone();
        std::thread::spawn(move || {
            let _ticket = ticket;
            if !lifecycle.is_closed() {
                work();
            }
        });
        true
    }

    fn begin_terminal(&self) {
        // Engine transitions take diagnostic_order before engine. Closure uses
        // that same order, then no pending attempt may become current again.
        {
            let _order = oxidom_core::sync::lock(&self.diagnostic_order);
            let _engine = oxidom_core::sync::lock(&self.engine);
            self.lifecycle.close();
            self.invalidate_all_generations();
            self.geo_cancel.cancel();
            oxidom_core::sync::lock(&self.probes).queued.clear();
        }
    }

    fn terminate(&self) -> Result<()> {
        self.begin_terminal();
        self.lifecycle.drain();
        let mut cleanup = oxidom_core::sync::lock(&self.terminal_cleanup);
        if let Some(result) = &*cleanup {
            return result.clone().map_err(anyhow::Error::msg);
        }
        self.stop_standby();
        let mut probes = oxidom_core::sync::lock(&self.probes);
        probes.queued.clear();
        probes.running.clear();
        drop(probes);
        let result = oxidom_core::sync::lock(&self.engine)
            .shutdown()
            .map_err(|error| format!("{error:#}"));
        *cleanup = Some(result.clone());
        result.map_err(anyhow::Error::msg)
    }

    async fn update_subscriptions(
        &self,
        operation: SubscriptionOperation,
        fetch: impl FnMut(
            &SubscriptionFetch,
            Option<&str>,
        ) -> Result<oxidom_core::subscription::FetchResult>
        + Send
        + 'static,
    ) -> Result<()> {
        let ticket = self.admit().map_err(anyhow::Error::new)?;
        let guard = self.subscription_work.clone().lock_owned().await;
        let shared = self.clone();
        tokio::task::spawn_blocking(move || {
            let _ticket = ticket;
            let _guard = guard;
            let update = {
                let engine = oxidom_core::sync::lock(&shared.engine);
                shared.ensure_open().map_err(anyhow::Error::new)?;
                engine.prepare_subscription_update(operation)?
            };
            let results = update.fetch(
                |target| {
                    shared.ensure_open().map_err(anyhow::Error::new)?;
                    oxidom_core::sync::lock(&shared.engine).validate_subscription_fetch(target)
                },
                fetch,
            );
            let result = {
                let order = oxidom_core::sync::lock(&shared.diagnostic_order);
                let mut engine = oxidom_core::sync::lock(&shared.engine);
                let commit = shared.lifecycle.commit().context("daemon is stopping")?;
                let before = Identities::selections(&engine);
                let (result, stopped) = engine.apply_subscription_update(results);
                for profile in stopped {
                    shared.invalidate_generation(&profile);
                    shared.clear_override(&profile);
                }
                let mut facts = Facts::default();
                shared.capture_teardown(&before, &engine, &mut facts);
                drop(engine);
                drop(order);
                drop(commit);
                facts.emit(&shared.recorder);
                result
            };
            shared.prune_readings();
            shared.reconcile_standby();
            result
        })
        .await
        .map_err(|_| anyhow::anyhow!("subscription worker failed"))?
    }

    #[cfg(test)]
    pub(crate) fn new(
        engine: Engine,
        socks_port_locked: bool,
        http_port_locked: bool,
        system_bus: bool,
    ) -> Self {
        Self::new_with_recorder(
            engine,
            socks_port_locked,
            http_port_locked,
            system_bus,
            crate::diagnostics::Recorder::for_test(),
        )
    }

    pub(crate) fn new_with_recorder(
        engine: Engine,
        socks_port_locked: bool,
        http_port_locked: bool,
        system_bus: bool,
        recorder: crate::diagnostics::Recorder,
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
            .collect::<HashMap<_, _>>();
        let mut identities = Identities::default();
        let mut facts = Facts::default();
        for (profile, generation) in &current_generations {
            let session = engine.sessions.get(profile).expect("seeded session");
            if let Some(core) = session.core.incarnation() {
                let target = if session.server_id().is_some() {
                    TargetKind::Server
                } else {
                    TargetKind::Pool
                };
                let context = identities.start(
                    &recorder,
                    profile,
                    *generation,
                    Origin::Adopted,
                    target,
                    &mut facts,
                );
                identities.started(profile, context, Some(core), true, &mut facts);
            }
        }
        let shared = Shared {
            lifecycle: lifecycle::Lifecycle::default(),
            terminal_cleanup: Arc::new(Mutex::new(None)),
            engine: Arc::new(Mutex::new(engine)),
            recorder,
            identities: Arc::new(Mutex::new(identities)),
            diagnostic_order: Arc::new(Mutex::new(())),
            subscription_work: Arc::new(tokio::sync::Mutex::new(())),
            readings: Arc::new(Mutex::new(HashMap::new())),
            history: Arc::new(Mutex::new(HashMap::new())),
            proxied: Arc::new(Mutex::new(HashMap::new())),
            probes: Arc::new(Mutex::new(ProbeQueue::default())),
            override_status: Arc::new(Mutex::new(HashMap::new())),
            tunnel_failures: Arc::new(Mutex::new(HashMap::new())),
            watchdog_epoch: Arc::new(AtomicU64::new(0)),
            // An adopted pool core is already a live attempt. Giving it a
            // generation makes its first post-recovery death reconnectable
            // while retaining the same cancellation rules as a fresh `up`.
            connect_generation: Arc::new(AtomicU64::new(generation)),
            current_generations: Arc::new(Mutex::new(current_generations)),
            socks_port_locked,
            http_port_locked,
            system_bus,
            geo: Arc::new(Mutex::new(GeoState::default())),
            core_version: Arc::new(Mutex::new(CoreVersion::default())),
            geo_cancel: Arc::new(oxidom_core::xray::assets::Cancel::default()),
            standby: Arc::new(Mutex::new(None)),
            store_lease: None,
        };
        facts.emit(&shared.recorder);
        shared
    }

    pub(crate) fn stop_standby(&self) {
        if let Some(mut proxy) = oxidom_core::sync::lock(&self.standby).take() {
            proxy.stop();
        }
    }

    pub(crate) fn reconcile_standby(&self) {
        // Keep the engine locked through the listener change. Session startup
        // stops standby under the same lock, so a stale reconcile cannot undo it.
        let engine = oxidom_core::sync::lock(&self.engine);
        if self.lifecycle.is_closed() {
            return;
        }
        let overrides = oxidom_core::sync::lock(&self.override_status);
        let wanted = {
            if !engine.registry.config.standby_proxy {
                None
            } else {
                let busy = engine.sessions.iter().any(|(profile, session)| {
                    let status = session.status();
                    // A released core can die before begin_reconnect publishes
                    // its override. Keep direct ports closed across that gap and
                    // across failed attempts while the retry remains current.
                    let retry_pending = resilient(&engine, profile)
                        && self.current_generation(profile).is_some()
                        && (matches!(&status, Status::Error(message) if message == CORE_EXITED_MESSAGE)
                            || overrides.get(profile).is_some_and(|failure| {
                                matches!(&failure.status, Status::Error(message) if message == RECONNECTING_MESSAGE)
                            }));
                    matches!(status, Status::Connected | Status::Connecting)
                        || session.holding_traffic
                        || retry_pending
                }) || overrides
                    .values()
                    .any(|status| matches!(status.status, Status::Connected | Status::Connecting));
                (!busy).then_some((
                    engine.registry.config.socks_port,
                    engine.registry.config.http_port,
                ))
            }
        };
        let mut guard = oxidom_core::sync::lock(&self.standby);
        match wanted {
            None => {
                if let Some(mut proxy) = guard.take() {
                    proxy.stop();
                }
            }
            Some((socks_port, http_port)) => {
                if guard
                    .as_ref()
                    .is_some_and(|proxy| proxy.ports() == (socks_port, http_port))
                {
                    return;
                }
                if let Some(mut proxy) = guard.take() {
                    proxy.stop();
                }
                match oxidom_core::standby::StandbyProxy::start(socks_port, http_port) {
                    Ok(proxy) => *guard = Some(proxy),
                    Err(error) => log::warn!("could not start standby proxy: {error:#}"),
                }
            }
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

    /// Called only after fallible preflight, with ownership of the engine.
    fn claim_generation(&self, profile: &str, facts: &mut Facts) -> u64 {
        let generation = self.next_connect_generation(profile);
        self.clear_override(profile);
        oxidom_core::sync::lock(&self.identities).supersede(profile, facts);
        generation
    }

    fn generation_is_current(&self, profile: &str, generation: u64) -> bool {
        !self.lifecycle.is_closed()
            && oxidom_core::sync::lock(&self.current_generations).get(profile) == Some(&generation)
    }

    fn current_generation(&self, profile: &str) -> Option<u64> {
        if self.lifecycle.is_closed() {
            return None;
        }
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

    fn capture_confirmation(
        &self,
        engine: &Engine,
        profile: &str,
        generation: u64,
        context: Option<DiagnosticContext>,
    ) -> Confirmation {
        let session = engine.sessions.get(profile);
        Confirmation {
            generation,
            core: session.and_then(|session| session.core.incarnation()),
            spawn_seq: session.map_or(0, |session| session.core.spawn_seq()),
            context,
            inbound: session.map(|session| {
                (
                    session.address,
                    session.socks_port,
                    engine.registry.config.latency_method,
                )
            }),
        }
    }

    fn confirmation_is_current(
        &self,
        engine: &Engine,
        profile: &str,
        snapshot: Confirmation,
    ) -> bool {
        self.generation_is_current(profile, snapshot.generation)
            && engine.sessions.get(profile).is_some_and(|session| {
                session.selection.is_some()
                    && session.core.incarnation() == snapshot.core
                    && session.core.spawn_seq() == snapshot.spawn_seq
            })
    }

    fn record_finished(
        &self,
        snapshot: Confirmation,
        outcome: ObservationOutcome,
        current: bool,
        error: Option<&anyhow::Error>,
        detail: Option<ProbeDetail>,
    ) {
        if let Some(event) = trace::finished(snapshot.context, outcome, current, error, detail) {
            self.recorder.record(event);
        }
    }

    /// Return every session whose core has just been found dead, once.
    ///
    /// The sweep is deliberately per profile: two sessions may carry the same
    /// server, and the health of one process says nothing about the other.
    #[cfg(test)]
    fn note_core_deaths(&self) -> Vec<(String, ConnectionTarget)> {
        self.capture_core_deaths()
            .into_iter()
            .map(|(profile, target, _)| (profile, target))
            .collect()
    }

    fn capture_core_deaths(&self) -> Vec<(String, ConnectionTarget, Confirmation)> {
        let order = oxidom_core::sync::lock(&self.diagnostic_order);
        let mut engine = oxidom_core::sync::lock(&self.engine);
        if self.lifecycle.is_closed() {
            return Vec::new();
        }
        let mut facts = Facts::default();
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
            let generation = self.current_generation(&profile).unwrap_or(0);
            let core = engine
                .sessions
                .get(&profile)
                .and_then(|session| session.core.incarnation());
            let context = oxidom_core::sync::lock(&self.identities).observed_core(&profile, core);
            let snapshot = self.capture_confirmation(&engine, &profile, generation, context);
            dead.push((profile, target, snapshot));
        }
        for (profile, _, snapshot) in &dead {
            // Not a teardown. A core exiting is not a person asking for their
            // ordinary connection back, and removing the routes before the
            // retry sends every application straight out with the machine's own
            // address for as long as the reconnect takes. `note_core_exit`
            // holds by default and releases only where the profile asked for it.
            engine.note_core_exit(profile);
            if let Some(context) = snapshot.context {
                facts.push(Event::LivenessLost {
                    context,
                    holding_traffic: engine
                        .sessions
                        .get(profile)
                        .is_some_and(|session| session.holding_traffic),
                });
            }
        }
        drop(engine);
        drop(order);
        facts.emit(&self.recorder);
        dead
    }

    fn begin_reconnect(&self, profile: String, target: ConnectionTarget, snapshot: Confirmation) {
        if !self.claim_reconnect(&profile, &target, snapshot) {
            return;
        }
        let shared = self.clone();
        self.spawn(move || {
            shared.reconnect(profile, target, snapshot.generation, snapshot.context)
        });
    }

    fn claim_reconnect(
        &self,
        profile: &str,
        target: &ConnectionTarget,
        snapshot: Confirmation,
    ) -> bool {
        let generation = snapshot.generation;
        if !self.generation_is_current(profile, generation) {
            return false;
        }
        let order = oxidom_core::sync::lock(&self.diagnostic_order);
        let engine = oxidom_core::sync::lock(&self.engine);
        let death_is_current = self.confirmation_is_current(&engine, profile, snapshot)
            && resilient(&engine, profile)
            && engine.sessions.get(profile).is_some_and(|session| {
                session.selection.as_ref() == Some(&target.selection())
                    && match target {
                        ConnectionTarget::Server(_) => true,
                        ConnectionTarget::Pool { api_port, .. } => session.api_port == *api_port,
                    }
            })
            && matches!(
                engine.sessions.get(profile).map(|session| session.status()),
                Some(Status::Error(message)) if message == CORE_EXITED_MESSAGE
            );
        if !death_is_current || !self.generation_is_current(profile, generation) {
            return false;
        }
        let already_reconnecting = matches!(
            oxidom_core::sync::lock(&self.override_status).get(profile),
            Some(ErrorOverride {
                status: Status::Error(message),
                selection,
            }) if message == RECONNECTING_MESSAGE && selection == &target.selection()
        );
        if already_reconnecting {
            return false;
        }

        oxidom_core::sync::lock(&self.override_status).insert(
            profile.to_string(),
            ErrorOverride {
                status: Status::Error(RECONNECTING_MESSAGE.to_string()),
                selection: target.selection(),
            },
        );
        let mut facts = Facts::default();
        oxidom_core::sync::lock(&self.identities).retry(
            profile,
            snapshot.context,
            RetryState::Pending,
            0,
            &mut facts,
        );
        drop(engine);
        drop(order);
        facts.emit(&self.recorder);
        true
    }

    fn retry_fact(
        &self,
        profile: &str,
        trigger: Option<DiagnosticContext>,
        state: RetryState,
        ordinal: u32,
    ) {
        let order = oxidom_core::sync::lock(&self.diagnostic_order);
        let mut facts = Facts::default();
        {
            let engine = oxidom_core::sync::lock(&self.engine);
            let mut identities = oxidom_core::sync::lock(&self.identities);
            if identities.retry(profile, trigger, state, ordinal, &mut facts)
                && state == RetryState::Cancelled
            {
                identities.prune(&engine, &mut facts);
            }
        }
        drop(order);
        facts.emit(&self.recorder);
    }

    fn complete_reconnect(
        &self,
        profile: &str,
        target: &ConnectionTarget,
        snapshot: Confirmation,
        trigger: Option<DiagnosticContext>,
        ordinal: u32,
    ) -> bool {
        let order = oxidom_core::sync::lock(&self.diagnostic_order);
        if !self.reconnect_is_pending(profile, target, snapshot.generation) {
            return false;
        }
        let mut engine = oxidom_core::sync::lock(&self.engine);
        if !self.confirmation_is_current(&engine, profile, snapshot)
            || !engine.sessions.get_mut(profile).is_some_and(|session| {
                session.selection.as_ref() == Some(&target.selection())
                    && session.reported_status() == Status::Connected
                    && session.is_alive()
            })
        {
            return false;
        }
        self.clear_reconnect_override(profile, target);
        let mut facts = Facts::default();
        oxidom_core::sync::lock(&self.identities).retry(
            profile,
            trigger,
            RetryState::Recovered,
            ordinal,
            &mut facts,
        );
        drop(engine);
        drop(order);
        facts.emit(&self.recorder);
        true
    }

    fn reconnect(
        &self,
        profile: String,
        target: ConnectionTarget,
        generation: u64,
        trigger: Option<DiagnosticContext>,
    ) {
        let mut attempt = 0;
        loop {
            if !self.reconnect_is_pending(&profile, &target, generation) {
                self.retry_fact(&profile, trigger, RetryState::Cancelled, attempt);
                return;
            }
            if !self.lifecycle.wait(reconnect_delay(attempt)) {
                return;
            }
            if !self.reconnect_is_pending(&profile, &target, generation) {
                self.retry_fact(&profile, trigger, RetryState::Cancelled, attempt);
                return;
            }

            let Some(result) = self.start_reconnect_attempt(&profile, &target, generation) else {
                self.retry_fact(&profile, trigger, RetryState::Cancelled, attempt);
                return;
            };
            match result {
                Ok((confirmed, snapshot)) => {
                    if confirmed.recv().unwrap_or(false)
                        && self.complete_reconnect(&profile, &target, snapshot, trigger, attempt)
                    {
                        return;
                    }
                }
                Err(error) => {
                    log::warn!("automatic reconnect of profile {profile:?} failed: {error:#}");
                }
            }
            attempt = attempt.saturating_add(1);
            if self.reconnect_is_pending(&profile, &target, generation) {
                self.retry_fact(&profile, trigger, RetryState::Pending, attempt);
            }
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

    fn supervise_once(&self) {
        let dead = self.capture_core_deaths();
        if dead.is_empty() {
            return;
        }
        for (profile, target, snapshot) in dead {
            self.begin_reconnect(profile, target, snapshot);
        }
        self.reconcile_standby();
    }

    fn status_info(&self) -> StatusInfo {
        self.supervise_once();
        // Take one coherent snapshot in the same lock order as confirmation
        // and standby handoff: engine, override_status, standby.
        let engine = oxidom_core::sync::lock(&self.engine);
        let overrides = oxidom_core::sync::lock(&self.override_status);
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
            status.standby_proxy = oxidom_core::sync::lock(&self.standby).is_some();
            return status;
        };
        let profile = session.profile.as_str();
        let mut status = if let Some(failure) = overrides.get(profile) {
            StatusInfo::from_status(&failure.status, None)
                .with_error_id(failure.selection.server_id().map(str::to_string))
        } else {
            StatusInfo::from_status(
                &session.reported_status(),
                session.server_id().map(str::to_string),
            )
            .with_active_profile(session.selection.as_ref().map(|_| profile.to_string()))
        };
        status.sessions = sessions;
        status.standby_proxy = oxidom_core::sync::lock(&self.standby).is_some();
        status
    }

    fn list_session_infos(&self) -> Vec<SessionInfo> {
        let engine = oxidom_core::sync::lock(&self.engine);
        let overrides = oxidom_core::sync::lock(&self.override_status);
        engine
            .sessions
            .iter()
            .map(|(profile, session)| {
                session_info(&engine, profile, session, overrides.get(profile))
            })
            .collect()
    }

    fn session_info(&self, profile: &str) -> Option<SessionInfo> {
        let engine = oxidom_core::sync::lock(&self.engine);
        let overrides = oxidom_core::sync::lock(&self.override_status);
        let session = engine.sessions.get(profile)?;
        Some(session_info(
            &engine,
            profile,
            session,
            overrides.get(profile),
        ))
    }

    fn runtime_info(&self) -> RuntimeInfo {
        // Resolution and geo/version checks may spawn subprocesses. Snapshot
        // settings first so terminal closure never waits on this engine lock.
        let engine = oxidom_core::sync::lock(&self.engine);
        let configured = engine.registry.config.xray_binary.clone();
        let socks_port = engine.registry.config.socks_port;
        let http_port = engine.registry.config.http_port;
        drop(engine);
        let resolved = self.resolve_xray(&configured);
        let (resolved_path, xray_path, xray_error, xray_source) = match resolved {
            Ok(resolved) => (
                Some(resolved.path.clone()),
                Some(resolved.path.display().to_string()),
                None,
                (!oxidom_core::xray::managed::is_managed(&resolved.path))
                    .then_some(resolved.source),
            ),
            Err(error) => (None, None, Some(format!("{error:#}")), None),
        };
        RuntimeInfo {
            xray_path,
            xray_error,
            xray_source,
            socks_port_locked: self.socks_port_locked,
            http_port_locked: self.http_port_locked,
            socks_port,
            http_port,
            // Same rule `set_settings` applies below, published so the GUI can
            // grey the rows instead of accepting text the daemon will revert.
            binary_paths_locked: self.system_bus,
            geo: self.geo_assets(resolved_path.as_deref()),
            daemon_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            core_version: self.read_core_version(resolved_path.as_deref()),
        }
    }

    /// What the resolved core calls itself.
    ///
    /// `None` when none resolved, or when the binary that did could not be run
    /// — a file that is not executable, an architecture this machine cannot
    /// load. Why it failed is already in `xray_error` where resolution is what
    /// went wrong, and where it is not, a version the daemon cannot obtain is
    /// better reported as absent than as the error text, which would end up in
    /// a row that a reader takes for a version string.
    ///
    /// The lock is held across the spawn, as `geo_assets` below holds its own
    /// across `xray run -test`: two callers arriving together then produce one
    /// subprocess rather than two, and `xray version` is the lighter of the
    /// two questions.
    fn read_core_version(&self, xray: Option<&std::path::Path>) -> Option<String> {
        let xray = xray?;
        let mut cached = oxidom_core::sync::lock(&self.core_version);
        if cached.read_for.as_deref() == Some(xray) {
            return cached.version.clone();
        }
        let version = oxidom_core::proc::output_timeout(
            std::process::Command::new(xray).arg("version"),
            Duration::from_secs(5),
        )
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            oxidom_core::versions::core_version(&String::from_utf8_lossy(&output.stdout))
        });
        cached.read_for = Some(xray.to_path_buf());
        cached.version = version.clone();
        version
    }

    /// What this daemon can say about the core's geo data.
    ///
    /// The verdict is cached against the binary it was measured for, because
    /// answering it spawns a process and this is fetched every time Settings
    /// opens. A download in flight is reported from the same lock, so a caller
    /// polling this sees progress without a second method.
    fn geo_assets(&self, xray: Option<&std::path::Path>) -> oxidom_core::ipc::GeoAssets {
        use oxidom_core::xray::assets;

        let dir = assets::own_dir().unwrap_or_default();
        let size = |asset: assets::GeoAsset| {
            std::fs::metadata(dir.join(asset.installed_name()))
                .map(|meta| meta.len())
                .unwrap_or(0)
        };
        let mut state = oxidom_core::sync::lock(&self.geo);
        // Only ask the core when there is one and the answer is not already
        // about it. A download completing clears `probed_for`, which is what
        // makes the next read re-measure.
        if let Some(xray) = xray
            && state.probed_for.as_deref() != Some(xray)
            && !state.downloading
        {
            match Self::geo_scratch() {
                Ok(scratch) => {
                    let verdict = assets::probe(xray, None, &scratch);
                    state.probed_for = Some(xray.to_path_buf());
                    match verdict {
                        Ok(()) => {
                            state.usable = Some(true);
                            state.error = None;
                        }
                        Err(error) => {
                            log::warn!("the Xray core cannot load its geo data: {error}");
                            state.usable = Some(false);
                            state.error = Some(error);
                        }
                    }
                }
                Err(error) => log::debug!("cannot stage a geo data check: {error:#}"),
            }
        }
        if xray.is_none() {
            // Nothing to ask. Undetermined rather than false: a missing core is
            // its own message, and reporting missing data beside it would put
            // two banners on one cause.
            state.usable = None;
            state.error = None;
            state.probed_for = None;
        }
        oxidom_core::ipc::GeoAssets {
            dir: dir.display().to_string(),
            usable: state.usable,
            error: state.error.clone(),
            geoip_bytes: size(assets::GeoAsset::GeoIp),
            geosite_bytes: size(assets::GeoAsset::GeoSite),
            writable: Self::geo_dir_writable(&dir),
            downloading: state.downloading,
            current_file: state.progress.file.clone(),
            done_bytes: state.progress.done_bytes,
            total_bytes: state.progress.total_bytes,
            last_error: state.last_error.clone(),
            cancelled: state.cancelled,
        }
    }

    /// Start fetching both lists on a worker thread.
    ///
    /// Returns as soon as the thread is running. That is not a convenience: the
    /// daemon serves D-Bus on a blocking connection whose method tasks share one
    /// executor, so a method that sat in the download would stall `Status`,
    /// `ProbeState` and `LogsSince` for its whole duration — and the log stream
    /// is where the progress lines the user reads come from.
    fn start_geo_download(&self, through_tunnel: bool) -> Result<(), String> {
        use oxidom_core::xray::assets;

        let proxy = through_tunnel.then(|| self.local_socks_proxy()).flatten();
        let _commit = self
            .lifecycle
            .commit()
            .ok_or_else(|| "daemon is stopping".to_string())?;
        let dir = assets::own_dir().map_err(|error| format!("{error:#}"))?;
        {
            let mut state = oxidom_core::sync::lock(&self.geo);
            if state.downloading {
                return Err("a geo data download is already running".to_string());
            }
            state.downloading = true;
            state.cancelled = false;
            state.last_error = None;
            state.progress = assets::Progress::default();
        }
        self.geo_cancel.reset();

        // Chosen here rather than taken from the caller: the daemon knows its
        // own inbounds, and an address supplied over the bus would make this a
        // "send my traffic wherever you like" primitive.
        // `Shared` is a bundle of `Arc`s, so the clone is the same state.
        let shared = self.clone();
        self.spawn(move || {
            let outcome = shared.run_geo_download(&dir, proxy.as_deref());
            let mut state = oxidom_core::sync::lock(&shared.geo);
            state.downloading = false;
            state.progress = assets::Progress::default();
            match outcome {
                Ok(()) => {
                    state.last_error = None;
                    // Force the next read to ask the core again: the answer it
                    // has cached was measured before these files existed.
                    state.probed_for = None;
                }
                Err(error) if error.is::<assets::Cancelled>() => {
                    state.cancelled = true;
                    log::info!("the geo data download was cancelled");
                }
                Err(error) => {
                    let text = format!("{error:#}");
                    log::error!("could not install the geo data: {text}");
                    state.last_error = Some(text);
                }
            }
        });
        Ok(())
    }

    fn run_geo_download(&self, dir: &std::path::Path, proxy: Option<&str>) -> anyhow::Result<()> {
        use oxidom_core::xray::assets;

        struct Reporter<'a> {
            shared: &'a Shared,
        }
        impl assets::Sink for Reporter<'_> {
            fn progress(&self, progress: &assets::Progress) {
                oxidom_core::sync::lock(&self.shared.geo).progress = progress.clone();
            }
            fn cancelled(&self) -> bool {
                self.shared.lifecycle.is_closed() || self.shared.geo_cancel.is_cancelled()
            }
            fn install(&self, target: &std::path::Path, bytes: &[u8]) -> Result<()> {
                let _commit = self.shared.lifecycle.commit().ok_or(assets::Cancelled)?;
                if self.cancelled() {
                    return Err(assets::Cancelled.into());
                }
                oxidom_core::fsutil::write_private_atomic(target, bytes)
            }
        }
        // Safety of the borrow: the thread joins on nothing, but `Reporter`
        // lives only for this call, which the thread owns.
        let sink = Reporter { shared: self };

        let agent = assets::agent(proxy)?;
        if let Some(proxy) = proxy {
            log::info!("fetching the geo data through {proxy}");
        }
        // Read once, before the first fetch: the two lists must come from the
        // pair the settings held when the download was asked for, not from
        // whatever a settings save between them left behind.
        let urls: Vec<(assets::GeoAsset, String)> = {
            let engine = oxidom_core::sync::lock(&self.engine);
            assets::GeoAsset::ALL
                .into_iter()
                .map(|asset| (asset, engine.registry.config.geo_url(asset).to_string()))
                .collect()
        };
        for (asset, url) in urls {
            log::info!("fetching {} from {url}", asset.installed_name());
            let path = assets::download(asset, &url, dir, &agent, &sink)?;
            log::info!("installed {}", path.display());
        }
        Ok(())
    }

    /// The proxy URL of a tunnel that is actually up, if one is.
    fn local_socks_proxy(&self) -> Option<String> {
        let engine = oxidom_core::sync::lock(&self.engine);
        engine
            .sessions
            .iter()
            .find(|(_, session)| session.status() == Status::Connected)
            // ureq 2.12 rejects the curl-style `socks5h` scheme, but its SOCKS5
            // transport hands the proxy a domain target rather than resolving
            // locally -- the same note `egress::request_address` carries.
            .map(|(_, session)| format!("socks5://{}", session.socks_endpoint()))
    }

    /// The core this daemon would run, when there is one.
    fn resolved_xray(&self) -> Option<std::path::PathBuf> {
        let engine = oxidom_core::sync::lock(&self.engine);
        let configured = engine.registry.config.xray_binary.clone();
        drop(engine);
        self.resolve_xray(&configured)
            .ok()
            .map(|resolved| resolved.path)
    }

    fn resolve_xray(&self, configured: &str) -> Result<oxidom_core::xray::resolve::ResolvedXray> {
        self.ensure_open().map_err(anyhow::Error::new)?;
        oxidom_core::xray::resolve::resolve_with_commit(configured, || {
            self.lifecycle.commit().context("daemon is stopping")
        })
    }

    fn geo_scratch() -> anyhow::Result<std::path::PathBuf> {
        Ok(oxidom_core::paths::data_dir()?.join("geo-check.json"))
    }

    /// Whether a download could actually be written, asked by creating the
    /// directory rather than by inspecting a mode: the daemon may be any user,
    /// and a read-only state directory is a real deployment.
    fn geo_dir_writable(dir: &std::path::Path) -> bool {
        if std::fs::create_dir_all(dir).is_err() {
            return false;
        }
        let probe = dir.join(".writable");
        match std::fs::write(&probe, b"") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
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
        let generation = match &target {
            ProbeTarget::Proxied(profile) => self.current_generation(profile),
            ProbeTarget::Direct(_) => None,
        };
        let Some(commit) = self.lifecycle.commit() else {
            return;
        };
        if !oxidom_core::sync::lock(&self.probes).enqueue(target, server_id, generation) {
            return;
        }
        drop(commit);
        self.pump_probes();
    }

    /// Call off queued checks for these servers, and report how many went.
    ///
    /// Every cancelled id leaves a reading, exactly as a measured one does. The
    /// latency contract is that an id which enters the queue leaves it with an
    /// entry, because a client retires its spinner on the id leaving
    /// `running ∪ queued` and a silent departure leaves a card checking for
    /// ever. The entry says `Cancelled`, which is neither the server's fault
    /// nor this machine's, so the card reports a check that was stopped instead
    /// of presenting a stale number as though it had just been refreshed.
    ///
    /// The method is `LatencyMethod::default()` because a queued job never read
    /// the config and so never learned which method it would have used — the
    /// same reason, and the same answer, as a server removed between the
    /// request and its slot.
    fn cancel_probes(&self, server_ids: &[String]) -> usize {
        let Some(_commit) = self.lifecycle.commit() else {
            return 0;
        };
        let wanted: HashSet<String> = server_ids.iter().cloned().collect();
        let cancelled = oxidom_core::sync::lock(&self.probes).cancel_direct(&wanted);
        if cancelled.is_empty() {
            return 0;
        }
        let mut readings = oxidom_core::sync::lock(&self.readings);
        for job in &cancelled {
            readings.insert(
                job.server_id.clone(),
                LatencyReading::failed_locally(
                    ProbeFailure::Unknown,
                    ProbeDetail::Cancelled,
                    ProbeRoute::Direct,
                    LatencyMethod::default(),
                ),
            );
        }
        cancelled.len()
    }

    fn enqueue_session_probe(&self, profile: String, label: String) {
        let generation = self.current_generation(&profile);
        let Some(commit) = self.lifecycle.commit() else {
            return;
        };
        if !oxidom_core::sync::lock(&self.probes).enqueue(
            ProbeTarget::Proxied(profile),
            label,
            generation,
        ) {
            return;
        }
        drop(commit);
        self.pump_probes();
    }

    fn pump_probes(&self) {
        loop {
            if self.lifecycle.is_closed() {
                return;
            }
            let Some(next) = oxidom_core::sync::lock(&self.probes).start_next() else {
                return;
            };
            let shared = self.clone();
            if !self.spawn(move || {
                shared.run_probe(&next);
                oxidom_core::sync::lock(&shared.probes).finish(next.token);
                shared.pump_probes();
            }) {
                return;
            }
        }
    }

    /// Probe one server and record the outcome in the map belonging to the
    /// job's route. Direct jobs complete in `readings`; live connection jobs
    /// complete in `proxied`.
    fn run_probe(&self, job: &ProbeJob) -> Option<u32> {
        match &job.target {
            ProbeTarget::Direct(_) => self.run_direct_probe(&job.server_id),
            ProbeTarget::Proxied(profile) => self.run_session_probe(profile, job.generation),
        }
    }

    fn run_direct_probe(&self, server_id: &str) -> Option<u32> {
        if self.lifecycle.is_closed() {
            return None;
        }
        let target = {
            let engine = oxidom_core::sync::lock(&self.engine);
            engine
                .find_server(server_id)
                .map(|server| (server, engine.registry.config.clone()))
        };
        // `measured` is what separates the two maps below. A probe that ran
        // says something about the server whether or not it produced a number —
        // a server that times out half the time is exactly what the history is
        // for — while the arm that never ran says only that this daemon lost
        // track of the server, which belongs in the current reading and nowhere
        // else.
        let (reading, measured) = match target {
            Some((server, config)) => {
                let method = config.latency_method;
                let reading = match probe::measure_with_commit(
                    &server,
                    &config,
                    probe::Route::Direct,
                    Ipv4Addr::LOCALHOST,
                    Duration::from_secs(3),
                    || self.lifecycle.commit().context("daemon is stopping"),
                ) {
                    // The reading carries the method that produced it, not the
                    // one the config asked for: a hysteria2 server may answer
                    // only ICMP. The card says which it was rather than
                    // passing a handshake off as the user's chosen probe.
                    probe::ProbeOutcome::Reachable(measured) => {
                        LatencyReading::ok(measured.ms, ProbeRoute::Direct, measured.method)
                    }
                    outcome => wire_reading(&outcome, ProbeRoute::Direct, method),
                };
                (reading, true)
            }
            // The server was removed between the request and its slot. Nothing
            // was measured and nothing about it is known — including which
            // method would have been used, since that read the config we never
            // got to.
            None => (
                LatencyReading::failed(
                    ProbeFailure::Unknown,
                    ProbeRoute::Direct,
                    LatencyMethod::default(),
                ),
                false,
            ),
        };
        let _commit = self.lifecycle.commit()?;
        let value = reading.value;
        oxidom_core::sync::lock(&self.readings).insert(server_id.to_string(), reading);
        if measured {
            self.record_history(server_id, reading);
        }
        value
    }

    /// Push one measured reading onto a server's history, newest first.
    ///
    /// Taken after the `readings` lock is released, never with both held: the
    /// two maps are written in this order everywhere, and the D-Bus methods
    /// that read them take one at a time.
    fn record_history(&self, server_id: &str, reading: LatencyReading) {
        let mut history = oxidom_core::sync::lock(&self.history);
        let entries = history.entry(server_id.to_string()).or_default();
        entries.insert(0, reading);
        entries.truncate(PROBE_HISTORY_LIMIT);
    }

    /// Probe the tunnel owned by one profile, keeping the raw outcome.
    ///
    /// Split out from `run_session_probe` so the watchdog can tell a server that
    /// stopped answering (`Timeout`/`Unreachable`) from one that is merely slow
    /// or from this machine losing its own uplink — a distinction the collapsed
    /// `Option<u32>` throws away. Returns the method actually attempted alongside
    /// the outcome, since a pool measures through a different verb than a single
    /// server, and even a single-server session configured for `icmp` or `tcp`
    /// confirms over HTTP through its SOCKS inbound. `None` means there was
    /// nothing to measure.
    fn session_probe_outcome_with_timeout(
        &self,
        profile: &str,
        timeout: Duration,
    ) -> Option<(probe::ProbeOutcome, LatencyMethod)> {
        self.session_probe_outcome_for(profile, timeout, None)
    }

    fn session_probe_outcome_for(
        &self,
        profile: &str,
        timeout: Duration,
        expected: Option<Confirmation>,
    ) -> Option<(probe::ProbeOutcome, LatencyMethod)> {
        if self.lifecycle.is_closed() {
            return None;
        }
        let (server, config, address, selection) = {
            let engine = oxidom_core::sync::lock(&self.engine);
            if expected
                .is_some_and(|snapshot| !self.confirmation_is_current(&engine, profile, snapshot))
            {
                return None;
            }
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
        // Every proxied probe — single server or pool — makes an HTTP request
        // through the local SOCKS inbound, so the attempted method on a failed
        // reading is always HTTP (`HttpHead` when asked for, `HttpGet`
        // otherwise), never `Icmp` or `Tcp`.
        let (verb, attempted_method) = pool_probe_method(requested_method);
        Some(match (selection, server) {
            (Some(SessionSelection::Server(_)), Some(server)) => (
                probe::measure_with_timeout(
                    &server,
                    &config,
                    probe::Route::Proxied,
                    address,
                    timeout,
                ),
                attempted_method,
            ),
            (Some(SessionSelection::Pool { .. }), _) => {
                // ICMP/TCP name one member and therefore cannot measure a
                // rotating session. HTTP through the live SOCKS inbound measures
                // exactly what the balancer carries.
                (
                    probe::http_ping(
                        address,
                        config.socks_port,
                        &config.latency_test_url,
                        verb,
                        timeout,
                    ),
                    attempted_method,
                )
            }
            _ => (
                probe::ProbeOutcome::Internal(oxidom_core::ipc::ProbeDetail::Other),
                attempted_method,
            ),
        })
    }

    /// Measure the tunnel owned by one profile, recording the reading the GUI
    /// shows and returning the latency in ms (or `None` on any failure).
    ///
    /// `generation` is the connect attempt this probe set out to measure. The
    /// reading lands only while that attempt is still the profile's current
    /// one: a session replaced mid-probe passes the Connected check below for
    /// the *new* core, and a timeout measured across the restart would
    /// overwrite the confirmation's good reading. A probe that captured no
    /// generation measured no attributable attempt, and discards likewise.
    fn run_session_probe(&self, profile: &str, generation: Option<u64>) -> Option<u32> {
        self.run_session_probe_with_timeout(profile, generation, Duration::from_secs(3))
            .0
    }

    fn run_session_probe_with_timeout(
        &self,
        profile: &str,
        generation: Option<u64>,
        timeout: Duration,
    ) -> (Option<u32>, Option<probe::ProbeOutcome>) {
        self.run_session_probe_for(profile, generation, timeout, None)
    }

    fn run_session_probe_for(
        &self,
        profile: &str,
        generation: Option<u64>,
        timeout: Duration,
        expected: Option<Confirmation>,
    ) -> (Option<u32>, Option<probe::ProbeOutcome>) {
        let outcome = if let Some(expected) = expected {
            self.session_probe_outcome_for(profile, timeout, Some(expected))
        } else {
            self.session_probe_outcome_with_timeout(profile, timeout)
        };
        let reading = match &outcome {
            Some((probe::ProbeOutcome::Reachable(measured), _)) => {
                LatencyReading::ok(measured.ms, ProbeRoute::Proxied, measured.method)
            }
            Some((outcome, attempted_method)) => {
                wire_reading(outcome, ProbeRoute::Proxied, *attempted_method)
            }
            None => LatencyReading::failed(
                ProbeFailure::Unknown,
                ProbeRoute::Proxied,
                LatencyMethod::default(),
            ),
        };
        let value = reading.value;
        let engine = oxidom_core::sync::lock(&self.engine);
        let still_current = {
            engine.sessions.get(profile).is_some_and(|session| {
                session.status() == Status::Connected && session.selection.is_some()
            }) && generation
                .is_some_and(|generation| self.generation_is_current(profile, generation))
                && expected
                    .is_none_or(|snapshot| self.confirmation_is_current(&engine, profile, snapshot))
        };
        if still_current {
            oxidom_core::sync::lock(&self.proxied).insert(profile.to_string(), reading);
        }
        (value, outcome.map(|(outcome, _)| outcome))
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
        self.start_connection_target_captured(profile, target, generation, origin)
            .map(|started| started.map(|(confirmation, _)| confirmation))
    }

    fn start_connection_target_captured(
        &self,
        profile: &str,
        target: &ConnectionTarget,
        generation: u64,
        origin: ConnectionOrigin,
    ) -> Result<Option<PendingConfirmation>> {
        if !self.generation_is_current(profile, generation) {
            if origin == ConnectionOrigin::Explicit {
                anyhow::bail!("connection attempt was superseded before its core started");
            }
            return Ok(None);
        }
        oxidom_core::sync::lock(&self.proxied).remove(profile);
        // A fresh attempt starts the death streak over, so a couple of misses
        // remembered from before an explicit reconnect cannot cut the new
        // tunnel's grace short.
        oxidom_core::sync::lock(&self.tunnel_failures).remove(profile);
        let probe_label = target.probe_label(profile);
        let probe_job = oxidom_core::sync::lock(&self.probes).start_now(
            profile,
            &probe_label,
            Some(generation),
        );

        let order = oxidom_core::sync::lock(&self.diagnostic_order);
        let mut facts = Facts::default();
        let mut snapshot = None;
        let connect_result = {
            let mut engine = oxidom_core::sync::lock(&self.engine);
            if !self.generation_is_current(profile, generation) {
                None
            } else {
                self.stop_standby();
                let context = oxidom_core::sync::lock(&self.identities).start(
                    &self.recorder,
                    profile,
                    generation,
                    if origin == ConnectionOrigin::Explicit {
                        Origin::Explicit
                    } else {
                        Origin::Automatic
                    },
                    if target.server_id().is_some() {
                        TargetKind::Server
                    } else {
                        TargetKind::Pool
                    },
                    &mut facts,
                );
                let result = match target {
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
                };
                let context = if result.is_ok() {
                    let core = engine
                        .sessions
                        .get(profile)
                        .and_then(|session| session.core.incarnation());
                    oxidom_core::sync::lock(&self.identities)
                        .started(profile, context, core, false, &mut facts)
                } else {
                    context
                };
                // On failure, context is still the pre-spawn context (core None),
                // even if the engine retains a previous core incarnation.
                snapshot = Some(self.capture_confirmation(&engine, profile, generation, context));
                if let Err(error) = &result {
                    if let Some(event) = trace::finished(
                        context,
                        ObservationOutcome::StartFailed,
                        self.generation_is_current(profile, generation),
                        Some(error),
                        None,
                    ) {
                        facts.push(event);
                    }
                    // Finish the failed operation before diagnostics can block:
                    // no delayed cleanup may touch a replacement's reading or proxy.
                    oxidom_core::sync::lock(&self.proxied).insert(
                        profile.to_string(),
                        LatencyReading::failed(
                            ProbeFailure::Unknown,
                            ProbeRoute::Proxied,
                            LatencyMethod::default(),
                        ),
                    );
                    oxidom_core::sync::lock(&self.probes).finish(probe_job.token);
                    engine.sessions.release_system_proxy(profile);
                }
                Some(result)
            }
        };
        if connect_result.is_none() {
            oxidom_core::sync::lock(&self.probes).finish(probe_job.token);
        }
        drop(order);
        if matches!(&connect_result, Some(Err(_))) {
            self.reconcile_standby();
        }
        facts.emit(&self.recorder);
        match connect_result {
            None => {
                if origin == ConnectionOrigin::Explicit {
                    Err(anyhow::anyhow!(
                        "connection attempt was superseded before its core started"
                    ))
                } else {
                    Ok(None)
                }
            }
            Some(Err(error)) => Err(error),
            Some(Ok(())) => {
                let snapshot = snapshot.expect("successful start snapshot");
                Ok(Some((
                    self.confirm_connection(
                        profile.to_string(),
                        target.clone(),
                        generation,
                        origin,
                        probe_job.token,
                        snapshot,
                    ),
                    snapshot,
                )))
            }
        }
    }

    /// `None` means an explicit operation has already superseded this retry,
    /// so no Xray start was attempted.
    fn start_reconnect_attempt<T: Into<ConnectionTarget>>(
        &self,
        profile: &str,
        target: T,
        generation: u64,
    ) -> Option<Result<PendingConfirmation>> {
        let target = target.into();
        if !self.generation_is_current(profile, generation) {
            return None;
        }
        match self.start_connection_target_captured(
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
    fn confirm_pool(&self, profile: &str, snapshot: Confirmation) -> PoolConfirmation {
        let deadline = Instant::now() + POOL_CONFIRM_WINDOW;
        loop {
            let measured = self.run_session_probe_for(
                profile,
                Some(snapshot.generation),
                probe::REAL_DELAY_TIMEOUT,
                Some(snapshot),
            );
            if let Some(completion) =
                self.pool_probe_completion(profile, snapshot, measured, deadline, Instant::now)
            {
                return completion;
            }
            if !self.lifecycle.wait(POOL_CONFIRM_RETRY_GAP) {
                return (None, None, ConfirmationEnd::Superseded);
            }
        }
    }

    fn pool_probe_completion(
        &self,
        profile: &str,
        snapshot: Confirmation,
        measured: (Option<u32>, Option<probe::ProbeOutcome>),
        deadline: Instant,
        now: impl FnOnce() -> Instant,
    ) -> Option<PoolConfirmation> {
        let (latency, outcome) = measured;
        let end = if latency.is_some() {
            ConfirmationEnd::Measured
        } else {
            // Keep the last actual observation even when ownership or liveness
            // ends the window early. Neither is evidence of deadline exhaustion.
            let engine = oxidom_core::sync::lock(&self.engine);
            let current = self.confirmation_is_current(&engine, profile, snapshot);
            let running = engine
                .sessions
                .get(profile)
                .is_some_and(|session| session.status() == Status::Connected);
            drop(engine);
            if !current {
                ConfirmationEnd::Superseded
            } else if !running {
                ConfirmationEnd::CoreStopped
            } else if now() + POOL_CONFIRM_RETRY_GAP >= deadline {
                ConfirmationEnd::PoolWindowExhausted
            } else {
                return None;
            }
        };
        Some((latency, outcome, end))
    }

    fn confirm_connection(
        &self,
        profile: String,
        target: ConnectionTarget,
        generation: u64,
        origin: ConnectionOrigin,
        probe_token: u64,
        snapshot: Confirmation,
    ) -> mpsc::Receiver<bool> {
        let (confirmed, confirmation) = mpsc::channel();
        let shared = self.clone();
        self.spawn(move || {
            let (address, socks_port, method) = {
                let Some(inbound) = snapshot.inbound else {
                    let Some(commit) = shared.lifecycle.commit() else {
                        return;
                    };
                    oxidom_core::sync::lock(&shared.proxied).insert(
                        profile.clone(),
                        LatencyReading::failed(
                            ProbeFailure::Unknown,
                            ProbeRoute::Proxied,
                            LatencyMethod::default(),
                        ),
                    );
                    oxidom_core::sync::lock(&shared.probes).finish(probe_token);
                    drop(commit);
                    shared.record_finished(
                        snapshot,
                        ObservationOutcome::MissingSubject,
                        false,
                        None,
                        None,
                    );
                    let _ = confirmed.send(false);
                    return;
                };
                inbound
            };

            // The core being alive proves nothing: readiness is the inbound
            // accepting connections. Waiting here is also what keeps the probe
            // below from racing a core that simply has not bound yet.
            let ready = probe::wait_for_socks(address, socks_port);

            let (latency, probe_outcome, end) = if ready && target.server_id().is_none() {
                shared.confirm_pool(&profile, snapshot)
            } else if ready {
                let (latency, outcome) = shared.run_session_probe_for(
                    &profile,
                    Some(generation),
                    probe::REAL_DELAY_TIMEOUT,
                    Some(snapshot),
                );
                (latency, outcome, ConfirmationEnd::Measured)
            } else {
                // Nothing could be measured, but the GUI is waiting on this id
                // and would otherwise see the spinner retire onto whatever the
                // map still held. Record the failure it actually is.
                let (_, attempted_method) = pool_probe_method(method);
                let engine = oxidom_core::sync::lock(&shared.engine);
                if shared.confirmation_is_current(&engine, &profile, snapshot) {
                    oxidom_core::sync::lock(&shared.proxied).insert(
                        profile.clone(),
                        LatencyReading::failed(
                            ProbeFailure::Timeout,
                            ProbeRoute::Proxied,
                            attempted_method,
                        ),
                    );
                }
                (
                    None,
                    Some(probe::ProbeOutcome::Timeout),
                    ConfirmationEnd::InboundNotReady,
                )
            };
            oxidom_core::sync::lock(&shared.probes).finish(probe_token);
            if ready && latency.is_some() {
                let current =
                    shared.apply_confirmation_success(&profile, &target, origin, snapshot);
                let _ = confirmed.send(current);
                return;
            }
            // The core writes its refusal as the outbound attempt fails, and
            // reading the logbook the instant `http_ping` returns loses the
            // race with the pipe reader thread often enough to miss the line —
            // the same race `ProbeCore::complaint` pays a short sleep for on
            // the failing path.
            if !shared.lifecycle.wait(Duration::from_millis(200)) {
                return;
            }
            shared.apply_confirmation_failure(
                &profile,
                &target,
                origin,
                snapshot,
                end,
                probe_outcome.as_ref(),
            );
            let _ = confirmed.send(false);
        });
        confirmation
    }

    fn apply_confirmation_success(
        &self,
        profile: &str,
        target: &ConnectionTarget,
        origin: ConnectionOrigin,
        snapshot: Confirmation,
    ) -> bool {
        let order = oxidom_core::sync::lock(&self.diagnostic_order);
        let mut engine = oxidom_core::sync::lock(&self.engine);
        // Currentness and interface/traffic release form one engine operation.
        let current = self.confirmation_is_current(&engine, profile, snapshot)
            && engine.sessions.get(profile).is_some_and(|session| {
                session.status() == Status::Connected
                    && session.selection.as_ref() == Some(&target.selection())
            });
        if current {
            if let Err(error) = engine.start_interface(profile) {
                let reason = format!("could not bring up the profile interface: {error:#}");
                engine.confirmation_failed(profile, &reason);
                if origin == ConnectionOrigin::Explicit {
                    oxidom_core::sync::lock(&self.override_status).insert(
                        profile.to_string(),
                        ErrorOverride {
                            status: Status::Error(reason),
                            selection: target.selection(),
                        },
                    );
                }
                let mut facts = Facts::default();
                if origin == ConnectionOrigin::Explicit {
                    oxidom_core::sync::lock(&self.identities).prune(&engine, &mut facts);
                }
                drop(engine);
                drop(order);
                self.reconcile_standby();
                self.record_finished(
                    snapshot,
                    ObservationOutcome::InterfaceFailed,
                    true,
                    Some(&error),
                    None,
                );
                facts.emit(&self.recorder);
                return false;
            }
            engine.confirm_traffic(profile);
            if engine.registry.config.system_proxy
                && let Err(error) = engine.sessions.claim_system_proxy(profile)
            {
                log::warn!("profile {profile:?} reconnected without the system proxy: {error:#}");
            }
        }
        drop(engine);
        drop(order);
        self.record_finished(snapshot, ObservationOutcome::Reachable, current, None, None);
        current
    }

    fn apply_confirmation_failure(
        &self,
        profile: &str,
        target: &ConnectionTarget,
        origin: ConnectionOrigin,
        snapshot: Confirmation,
        end: ConfirmationEnd,
        probe_outcome: Option<&probe::ProbeOutcome>,
    ) {
        let order = oxidom_core::sync::lock(&self.diagnostic_order);
        let mut engine = oxidom_core::sync::lock(&self.engine);
        // Bail out if another connect/disconnect superseded this attempt:
        // the tunnel now running is not the one this thread was confirming.
        let ready = end != ConfirmationEnd::InboundNotReady;
        let (outcome, detail) = if !ready {
            (ObservationOutcome::InboundNotReady, None)
        } else if end == ConfirmationEnd::PoolWindowExhausted {
            (
                ObservationOutcome::PoolWindowExhausted,
                trace::observation(probe_outcome).1,
            )
        } else {
            trace::observation(probe_outcome)
        };
        if !self.confirmation_is_current(&engine, profile, snapshot) {
            drop(engine);
            drop(order);
            self.record_finished(snapshot, outcome, false, None, detail);
            return;
        }
        // A core that rejected the config exited at once, so both the dead
        // inbound and the failed probe are symptoms. Say the actual cause.
        let logs = engine
            .sessions
            .get(profile)
            .map(|session| session.current_run_logs())
            .unwrap_or_default();
        let rotating = engine
            .sessions
            .get(profile)
            .and_then(|session| session.balancer_info.as_ref())
            .map(|info| info.principle.len());
        let reason = confirmation_failure(
            target,
            ready,
            &logs,
            rotating,
            probe_outcome,
            snapshot
                .inbound
                .map(|(_, _, method)| method)
                .unwrap_or_default(),
        );
        let still_active = engine.sessions.get(profile).is_some_and(|session| {
            session.selection.as_ref() == Some(&target.selection())
                && session.status() == Status::Connected
        });
        if still_active {
            engine.confirmation_failed(profile, &reason);
            if origin == ConnectionOrigin::Explicit {
                oxidom_core::sync::lock(&self.override_status).insert(
                    profile.to_string(),
                    ErrorOverride {
                        status: Status::Error(reason),
                        selection: target.selection(),
                    },
                );
            }
        }
        let mut facts = Facts::default();
        if origin == ConnectionOrigin::Explicit {
            oxidom_core::sync::lock(&self.identities).prune(&engine, &mut facts);
        }
        drop(engine);
        drop(order);
        self.reconcile_standby();
        self.record_finished(snapshot, outcome, true, None, detail);
        facts.emit(&self.recorder);
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
        self.retire_stopped_contexts();
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
        oxidom_core::sync::lock(&self.history).retain(|id, _| alive_servers.contains(id));
        oxidom_core::sync::lock(&self.proxied)
            .retain(|profile, _| alive_profiles.contains(profile));
        oxidom_core::sync::lock(&self.tunnel_failures)
            .retain(|profile, _| alive_profiles.contains(profile));
        oxidom_core::sync::lock(&self.probes).retain_alive(&alive_servers, &alive_profiles);
    }

    fn retire_stopped_contexts(&self) {
        let order = oxidom_core::sync::lock(&self.diagnostic_order);
        let mut facts = Facts::default();
        {
            let engine = oxidom_core::sync::lock(&self.engine);
            oxidom_core::sync::lock(&self.identities).prune(&engine, &mut facts);
        }
        drop(order);
        facts.emit(&self.recorder);
    }

    fn capture_teardown(&self, before: &HashSet<String>, engine: &Engine, facts: &mut Facts) {
        let alive = engine
            .all_servers()
            .map(|server| server.id.as_str())
            .collect::<HashSet<_>>();
        let pending = oxidom_core::sync::lock(&self.override_status)
            .iter()
            .filter(|(profile, failure)| {
                // A failed retry can have no live selection. Its override is
                // then the remaining source of target ownership. Never cancel
                // a different live selection because of an obsolete override.
                let same_subject = engine.sessions.get(profile).is_none_or(|session| {
                    session
                        .selection
                        .as_ref()
                        .is_none_or(|selection| selection == &failure.selection)
                });
                same_subject
                    && match &failure.selection {
                        SessionSelection::Server(server) => !alive.contains(server.as_str()),
                        SessionSelection::Pool { members, .. } => members
                            .iter()
                            .any(|server| !alive.contains(server.as_str())),
                    }
            })
            .map(|(profile, _)| profile.clone())
            .collect::<Vec<_>>();
        for profile in pending {
            self.invalidate_generation(&profile);
            self.clear_override(&profile);
            oxidom_core::sync::lock(&self.identities).supersede(&profile, facts);
        }
        for profile in before {
            if engine
                .sessions
                .get(profile)
                .is_none_or(|session| session.selection.is_none())
            {
                self.invalidate_generation(profile);
                self.clear_override(profile);
            }
        }
        oxidom_core::sync::lock(&self.identities).after_teardown(before, engine, facts);
    }

    fn watchdog_attempt(&self, engine: &Engine, profile: &str) -> Option<WatchdogAttempt> {
        let generation = self.current_generation(profile)?;
        let session = engine.sessions.get(profile)?;
        let core = session.core.incarnation();
        let context = oxidom_core::sync::lock(&self.identities).observed_core(profile, core);
        Some(WatchdogAttempt {
            generation,
            spawn_seq: session.core.spawn_seq(),
            core,
            policy_epoch: self.watchdog_epoch.load(Ordering::SeqCst),
            context,
        })
    }

    fn watchdog_is_current(
        &self,
        engine: &Engine,
        profile: &str,
        attempt: WatchdogAttempt,
    ) -> bool {
        // Diagnostic availability must never become an application guard.
        self.confirmation_is_current(engine, profile, attempt.subject())
            && self.watchdog_epoch.load(Ordering::SeqCst) == attempt.policy_epoch
    }

    fn record_watch(
        &self,
        attempt: WatchdogAttempt,
        mode: WatchdogMode,
        outcome: Option<&probe::ProbeOutcome>,
        action: WatchAction,
    ) {
        if let Some(context) = attempt.context {
            let (outcome, detail) = trace::observation(outcome);
            self.recorder.record(Event::Watch {
                context,
                policy_epoch: attempt.policy_epoch,
                mode: trace::watch_mode(mode),
                outcome,
                detail,
                action,
            });
        }
    }

    /// Report a streak of attributable negative probes. Missing measurements and
    /// local outages break the streak; neither proves the server is dead.
    fn note_tunnel_probe(
        &self,
        profile: &str,
        attempt: WatchdogAttempt,
        mode: WatchdogMode,
        outcome: Option<&probe::ProbeOutcome>,
    ) -> bool {
        let order = oxidom_core::sync::lock(&self.diagnostic_order);
        let engine = oxidom_core::sync::lock(&self.engine);
        if !self.watchdog_is_current(&engine, profile, attempt)
            || engine.registry.config.watchdog != mode
        {
            drop(engine);
            drop(order);
            self.record_watch(attempt, mode, outcome, WatchAction::DiscardedStale);
            return false;
        }
        let Some((_, _, threshold)) = watchdog_policy(mode) else {
            return false;
        };
        let mut failures = oxidom_core::sync::lock(&self.tunnel_failures);
        let had_streak = failures.contains_key(profile);
        let negative = matches!(
            outcome,
            Some(probe::ProbeOutcome::Timeout | probe::ProbeOutcome::Unreachable)
        );
        let threshold_reached = match outcome {
            Some(probe::ProbeOutcome::Timeout | probe::ProbeOutcome::Unreachable) => {
                let streak = failures.entry(profile.to_string()).or_insert(TunnelStreak {
                    attempt,
                    mode,
                    count: 0,
                });
                if (
                    streak.attempt.generation,
                    streak.attempt.core,
                    streak.attempt.spawn_seq,
                    streak.attempt.policy_epoch,
                    streak.mode,
                ) != (
                    attempt.generation,
                    attempt.core,
                    attempt.spawn_seq,
                    attempt.policy_epoch,
                    mode,
                ) {
                    *streak = TunnelStreak {
                        attempt,
                        mode,
                        count: 0,
                    };
                }
                streak.count = streak.count.saturating_add(1);
                if streak.count >= threshold {
                    failures.remove(profile);
                    true
                } else {
                    false
                }
            }
            _ => {
                failures.remove(profile);
                false
            }
        };
        drop(failures);
        drop(engine);
        drop(order);
        self.record_watch(
            attempt,
            mode,
            outcome,
            if !negative && had_streak {
                WatchAction::Reset
            } else {
                WatchAction::Observed
            },
        );
        threshold_reached
    }

    /// Revive a session whose server went silent while its core kept running.
    ///
    /// The liveness sweep notices a dead core or helper, not a silent server
    /// whose core remains alive with its inbound still bound. This kills the
    /// wedged core, marks it failed, and holds or releases its routes according
    /// to the session's own setting — then hands off to the ordinary reconnect
    /// loop, which retries with backoff until the server is back.
    #[cfg(test)]
    fn recover_dead_tunnel(
        &self,
        profile: &str,
        attempt: WatchdogAttempt,
    ) -> Option<ConnectionTarget> {
        self.recover_dead_tunnel_for(profile, attempt, Some(&probe::ProbeOutcome::Timeout))
            .map(|(target, _)| target)
    }

    fn recover_dead_tunnel_for(
        &self,
        profile: &str,
        attempt: WatchdogAttempt,
        outcome: Option<&probe::ProbeOutcome>,
    ) -> Option<(ConnectionTarget, Confirmation)> {
        let order = oxidom_core::sync::lock(&self.diagnostic_order);
        let target = (|| {
            let mut engine = oxidom_core::sync::lock(&self.engine);
            if !self.watchdog_is_current(&engine, profile, attempt)
                || engine.registry.config.watchdog != WatchdogMode::Aggressive
                || !resilient(&engine, profile)
            {
                return None;
            }
            let session = engine.sessions.get_mut(profile)?;
            if session.reported_status() != Status::Connected {
                return None;
            }
            let target = reconnect_target(session)?;
            // The core still holds the profile's SOCKS port; a reconnect rebinds
            // the same port and `ensure_ports_free` refuses while the old inbound
            // lingers. Kill it before failing so the port is free, and so
            // `fail` — which leaves the selection intact — is what `begin_reconnect`
            // then sees.
            session.core.disconnect();
            session.core.fail(CORE_EXITED_MESSAGE);
            session.balancer_info = None;
            // Same reasoning as the core-death sweep: the tunnel stopped
            // answering, which is a reason to stop passing traffic through it,
            // not a reason to pass that traffic in the clear instead.
            engine.note_core_exit(profile);
            let snapshot =
                self.capture_confirmation(&engine, profile, attempt.generation, attempt.context);
            Some((target, snapshot))
        })();
        drop(order);
        self.record_watch(
            attempt,
            WatchdogMode::Aggressive,
            outcome,
            if target.is_some() {
                WatchAction::RecoveryApplied
            } else {
                WatchAction::RecoveryRejected
            },
        );
        target
    }

    fn retain_conservative_tunnel(
        &self,
        profile: &str,
        attempt: WatchdogAttempt,
        outcome: Option<&probe::ProbeOutcome>,
    ) -> bool {
        let order = oxidom_core::sync::lock(&self.diagnostic_order);
        let mut engine = oxidom_core::sync::lock(&self.engine);
        let current = self.watchdog_is_current(&engine, profile, attempt)
            && engine.registry.config.watchdog == WatchdogMode::Conservative
            && engine.sessions.get_mut(profile).is_some_and(|session| {
                session.reported_status() == Status::Connected && session.is_alive()
            });
        drop(engine);
        drop(order);
        self.record_watch(
            attempt,
            WatchdogMode::Conservative,
            outcome,
            if current {
                WatchAction::ConservativeRetained
            } else {
                WatchAction::DiscardedStale
            },
        );
        current
    }

    /// Periodic re-probe of the active server; keeps the latency reading
    /// fresh but never tears an established connection down.
    fn spawn_active_probe_loop(&self) {
        let shared = self.clone();
        self.spawn(move || {
            loop {
                if !shared.lifecycle.wait(ACTIVE_PROBE_INTERVAL) {
                    return;
                }
                let active = {
                    let mut engine = oxidom_core::sync::lock(&shared.engine);
                    if shared.lifecycle.is_closed() {
                        return;
                    }
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

    /// Conservative checks diagnose silence without treating one synthetic
    /// destination as proof that a live core must be killed.
    fn spawn_tunnel_watchdog(&self) {
        let shared = self.clone();
        self.spawn(move || {
            loop {
                let mode = oxidom_core::sync::lock(&shared.engine)
                    .registry
                    .config
                    .watchdog;
                let interval = watchdog_policy(mode)
                    .map(|(interval, _, _)| interval)
                    .unwrap_or(Duration::from_secs(60));
                if !shared.lifecycle.wait(interval) {
                    return;
                }
                let (mode, watched) = {
                    let mut engine = oxidom_core::sync::lock(&shared.engine);
                    if shared.lifecycle.is_closed() {
                        return;
                    }
                    let mode = engine.registry.config.watchdog;
                    if mode == WatchdogMode::Off {
                        oxidom_core::sync::lock(&shared.tunnel_failures).clear();
                        continue;
                    }
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
                        if session.reported_status() == Status::Connected
                            && session.is_alive()
                            && (reconnect_all || session_has_run_cgroup(session))
                            && let Some(attempt) = shared.watchdog_attempt(&engine, &profile)
                        {
                            watched.push((profile, attempt));
                        }
                    }
                    (mode, watched)
                };
                let Some((_, timeout, _)) = watchdog_policy(mode) else {
                    continue;
                };
                for (profile, attempt) in watched {
                    let outcome = shared
                        .session_probe_outcome_for(&profile, timeout, Some(attempt.subject()))
                        .map(|(outcome, _)| outcome);
                    if shared.note_tunnel_probe(&profile, attempt, mode, outcome.as_ref()) {
                        if mode == WatchdogMode::Aggressive {
                            if let Some((target, snapshot)) =
                                shared.recover_dead_tunnel_for(&profile, attempt, outcome.as_ref())
                            {
                                log::warn!(
                                    "profile {profile:?} failed aggressive tunnel checks; reviving it"
                                );
                                shared.begin_reconnect(profile, target, snapshot);
                            }
                        } else if shared.retain_conservative_tunnel(
                            &profile,
                            attempt,
                            outcome.as_ref(),
                        ) {
                            log::warn!(
                                "profile {profile:?} failed conservative tunnel checks; keeping its live core"
                            );
                        }
                    }
                }
            }
        });
    }

    fn spawn_pool_status_loop(&self) {
        let shared = self.clone();
        self.spawn(move || {
            loop {
                if !shared.lifecycle.wait(POOL_POLL_INTERVAL) {
                    return;
                }
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
                        if shared.lifecycle.is_closed() {
                            return;
                        }
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
                    let result = shared.resolve_xray(&xray_binary).and_then(|resolved| {
                        oxidom_core::xray::api::balancer_info(
                            &resolved.path,
                            api,
                            "pool",
                            BALANCER_API_TIMEOUT,
                        )
                    });
                    let mut engine = oxidom_core::sync::lock(&shared.engine);
                    if shared.lifecycle.is_closed() {
                        return;
                    }
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

fn fetch_subscription(
    request: &SubscriptionFetch,
    hwid: Option<&str>,
) -> Result<oxidom_core::subscription::FetchResult> {
    oxidom_core::subscription::fetch(&request.url, &request.user_agent, request.send_hwid, hwid)
}

pub(crate) struct Service {
    pub(crate) shared: Shared,
}

impl Service {
    fn stop_profile(&self, profile: &str) -> fdo::Result<bool> {
        let order = oxidom_core::sync::lock(&self.shared.diagnostic_order);
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        self.shared.ensure_open()?;
        self.shared.clear_override(profile);
        self.shared.invalidate_generation(profile);
        let mut facts = Facts::default();
        oxidom_core::sync::lock(&self.shared.identities).supersede(profile, &mut facts);
        let stopped = engine.remove_session(profile);
        oxidom_core::sync::lock(&self.shared.identities).prune(&engine, &mut facts);
        drop(engine);
        drop(order);
        facts.emit(&self.shared.recorder);
        let stopped = stopped.map_err(failed)?;
        self.shared.prune_readings();
        self.shared.reconcile_standby();
        Ok(stopped)
    }

    fn stop_all_profiles(&self) -> fdo::Result<()> {
        let order = oxidom_core::sync::lock(&self.shared.diagnostic_order);
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        self.shared.ensure_open()?;
        self.shared.clear_all_overrides();
        self.shared.invalidate_all_generations();
        let mut facts = Facts::default();
        for (profile, _) in engine.sessions.iter() {
            oxidom_core::sync::lock(&self.shared.identities).supersede(profile, &mut facts);
        }
        let stopped = engine.disconnect_all();
        oxidom_core::sync::lock(&self.shared.identities).prune(&engine, &mut facts);
        drop(engine);
        drop(order);
        facts.emit(&self.shared.recorder);
        stopped.map_err(failed)?;
        self.shared.prune_readings();
        self.shared.reconcile_standby();
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

/// Turn a pool query into the target a connection is started against.
///
/// Shared by `UpProfile`, where the query came out of a profile file, and by
/// `ConnectPool`, where it came straight off the Servers page and no file is
/// involved at all. The warnings below are the reason this is one function: they
/// are said once, where somebody can act on them, rather than by `pool::resolve`,
/// which the interface calls on every keystroke — and a second copy of them would
/// drift.
///
/// `label` is what the warnings call this pool: a profile names itself, an ad-hoc
/// selection has no name to give.
fn resolve_pool_target(
    engine: &Engine,
    readings: &HashMap<String, oxidom_core::ipc::LatencyReading>,
    label: &str,
    query: &oxidom_core::pool::PoolQuery,
    address: Ipv4Addr,
    socks_port: u16,
    http_port: u16,
) -> anyhow::Result<(ConnectionTarget, Vec<oxidom_core::model::Server>)> {
    let composites = oxidom_core::pool::excluded_composites(query, &engine.registry.subscriptions);
    if !composites.is_empty() {
        log::warn!(
            "{label} pool omitted composite Xray profiles: {}",
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
    let missing = oxidom_core::pool::missing_members(query, &engine.registry.subscriptions);
    if !missing.is_empty() {
        log::warn!(
            "{label} pool lists {} server(s) no subscription holds any more: {}",
            missing.len(),
            missing.join(", ")
        );
    }
    // Ranked, not merely resolved: `max` truncates, and the first
    // member is the exit the balancer uses until the observatory
    // reports. Subscription order gave both jobs to whichever
    // entry the provider happened to list first.
    let resolved =
        oxidom_core::pool::resolve_ranked(query, &engine.registry.subscriptions, |server| {
            known_state(readings, &server.id)
        })?;
    if resolved.len() > MAX_POOL_MEMBERS {
        return Err(anyhow::anyhow!(
            "{label} pool resolves to {} nodes; set [select.pool] max to \
                     {MAX_POOL_MEMBERS} or less",
            resolved.len()
        ));
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
            "{label} pool has {} member(s) on only {hosts} exit address(es); \
                     traffic can spread no wider than that",
            resolved.len()
        );
    }
    let servers = resolved.into_iter().cloned().collect::<Vec<_>>();
    let members = servers
        .iter()
        .map(|server| server.id.clone())
        .collect::<Vec<_>>();
    let api_port = oxidom_core::engine::free_port(address, &[socks_port, http_port])?;
    let expected = query.expected_or_all(members.len());
    Ok((
        ConnectionTarget::Pool {
            members,
            name: query.name.clone(),
            strategy: query.strategy.as_xray().to_string(),
            expected,
            probe_interval: query.probe_interval_or_default().to_string(),
            api_port,
        },
        servers,
    ))
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
            let status = session.reported_status();
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
        holding_traffic: session.holding_traffic,
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
    let (
        Some(query),
        Some(SessionSelection::Pool {
            members: active_members,
            fingerprint,
            ..
        }),
    ) = (query, selection)
    else {
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
            if pool_fingerprint(&ids) == *fingerprint {
                return false;
            }
            if !query.members.is_empty() {
                return true;
            }
            let candidate_set: HashSet<&str> =
                members.iter().map(|server| server.id.as_str()).collect();
            let expected_len = if query.max == 0 {
                members.len()
            } else {
                query.max.min(members.len())
            };
            active_members.len() != expected_len
                || !active_members
                    .iter()
                    .all(|id| candidate_set.contains(id.as_str()))
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
        let _request = self.shared.admit()?;
        let engine = oxidom_core::sync::lock(&self.shared.engine);
        json(&engine.registry.subscriptions)
    }

    async fn add_subscription(
        &self,
        url: String,
        name: String,
        send_hwid: bool,
    ) -> fdo::Result<()> {
        let _request = self.shared.admit()?;
        let name = (!name.is_empty()).then_some(name);
        self.shared
            .update_subscriptions(
                SubscriptionOperation::Add {
                    url,
                    name,
                    send_hwid,
                },
                fetch_subscription,
            )
            .await
            .map_err(failed)
    }

    fn remove_subscription(&self, subscription_id: String) -> fdo::Result<bool> {
        let _request = self.shared.admit()?;
        let order = oxidom_core::sync::lock(&self.shared.diagnostic_order);
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        self.shared.ensure_open()?;
        let before = Identities::selections(&engine);
        let result = engine.remove_subscription(&subscription_id);
        let mut facts = Facts::default();
        self.shared.capture_teardown(&before, &engine, &mut facts);
        drop(engine);
        drop(order);
        facts.emit(&self.shared.recorder);
        self.shared.prune_readings();
        result.map_err(failed)
    }

    async fn refresh(&self, subscription_id: String) -> fdo::Result<()> {
        let _request = self.shared.admit()?;
        self.shared
            .update_subscriptions(
                SubscriptionOperation::Refresh(subscription_id),
                fetch_subscription,
            )
            .await
            .map_err(failed)
    }

    async fn refresh_all(&self) -> fdo::Result<()> {
        let _request = self.shared.admit()?;
        self.shared
            .update_subscriptions(SubscriptionOperation::RefreshAll, fetch_subscription)
            .await
            .map_err(failed)
    }

    fn import_links(&self, text: String) -> fdo::Result<(u32, u32)> {
        let _request = self.shared.admit()?;
        let result = {
            let mut engine = oxidom_core::sync::lock(&self.shared.engine);
            self.shared.ensure_open()?;
            engine.import_links(&text)
        };
        self.shared.prune_readings();
        let (added, unsupported) = result.map_err(failed)?;
        Ok((added as u32, unsupported as u32))
    }

    fn remove_server(&self, server_id: String) -> fdo::Result<bool> {
        let _request = self.shared.admit()?;
        let order = oxidom_core::sync::lock(&self.shared.diagnostic_order);
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        self.shared.ensure_open()?;
        let before = Identities::selections(&engine);
        let result = engine.remove_server(&server_id);
        let mut facts = Facts::default();
        self.shared.capture_teardown(&before, &engine, &mut facts);
        drop(engine);
        drop(order);
        facts.emit(&self.shared.recorder);
        self.shared.prune_readings();
        result.map_err(failed)
    }

    fn set_server_alias(&self, server_id: String, alias: String) -> fdo::Result<()> {
        let _request = self.shared.admit()?;
        if !alias::is_valid(&alias) {
            return Err(failed(
                "alias must be 1-32 lowercase letters, digits, or hyphens, start with a \
                 letter or digit, and not be exactly 16 hexadecimal characters",
            ));
        }
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        self.shared.ensure_open()?;
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

    /// Create a server described as fields, validated through the same path
    /// that generates an Xray outbound; nothing is created when validation
    /// fails. The draft arrives as JSON so the method can grow fields without
    /// the wire signature moving.
    fn create_server(&self, draft: String) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        let draft: oxidom_core::draft::ServerDraft = serde_json::from_str(&draft)
            .map_err(|error| failed(format!("the draft does not parse: {error}")))?;
        let server = oxidom_core::draft::resolve(&draft).map_err(failed)?;
        let stored = {
            let mut engine = oxidom_core::sync::lock(&self.shared.engine);
            self.shared.ensure_open()?;
            engine.add_server(server).map_err(failed)?
        };
        serde_json::to_string(&oxidom_core::ipc::CreatedServer {
            id: stored.id,
            name: stored.name,
            alias: stored.alias,
        })
        .map_err(failed)
    }

    /// Edit one stored server, validated through the same path that
    /// generates an Xray outbound; the id never changes. The draft arrives
    /// as JSON, as with `CreateServer`, so the wire signature stays put
    /// while the draft grows fields. A subscription's server records the
    /// changed fields as overrides; a hand-made one is simply rewritten.
    fn update_server(&self, server_id: String, draft: String) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        let draft: oxidom_core::draft::ServerDraft = serde_json::from_str(&draft)
            .map_err(|error| failed(format!("the draft does not parse: {error}")))?;
        let stored = {
            let mut engine = oxidom_core::sync::lock(&self.shared.engine);
            self.shared.ensure_open()?;
            engine.update_server(&server_id, &draft).map_err(failed)?
        };
        serde_json::to_string(&oxidom_core::ipc::CreatedServer {
            id: stored.id,
            name: stored.name,
            alias: stored.alias,
        })
        .map_err(failed)
    }

    /// Drop one override from a subscription's server and take back what
    /// the provider last sent for the field.
    fn drop_override(&self, server_id: String, field: String) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        let stored = {
            let mut engine = oxidom_core::sync::lock(&self.shared.engine);
            self.shared.ensure_open()?;
            engine
                .drop_server_override(&server_id, &field)
                .map_err(failed)?
        };
        serde_json::to_string(&oxidom_core::ipc::CreatedServer {
            id: stored.id,
            name: stored.name,
            alias: stored.alias,
        })
        .map_err(failed)
    }

    /// Read the certificate a server presents, without judging it.
    ///
    /// Two steps rather than one — inspect, then trust — so the value stored is
    /// the one the user was shown. Fetching again inside a `trust` call would
    /// pin whatever the second handshake returned, which is precisely the
    /// substitution a pin exists to prevent.
    fn inspect_certificate(&self, server_id: String) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
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
                .tls_sni()
                .map(str::to_string)
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
        let _request = self.shared.admit()?;
        let pin = sha256.trim().to_ascii_lowercase();
        if pin.len() != 64 || !pin.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(failed(
                "a certificate pin is 64 hexadecimal characters (SHA-256 of the certificate)",
            ));
        }
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        self.shared.ensure_open()?;
        let server = engine
            .registry
            .subscriptions
            .iter_mut()
            .flat_map(|subscription| subscription.servers.iter_mut())
            .find(|server| server.id == server_id)
            .ok_or_else(|| failed("server not found"))?;
        if !server.spec.set_pin_sha256(Some(pin)) {
            return Err(failed(
                "this server has no TLS settings to pin a certificate on",
            ));
        }
        engine.save().map_err(failed)
    }

    fn set_hwid(&self, subscription_id: String, enabled: bool) -> fdo::Result<()> {
        let _request = self.shared.admit()?;
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        self.shared.ensure_open()?;
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
        let _request = self.shared.admit()?;
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        self.shared.ensure_open()?;
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
        let _request = self.shared.admit()?;
        // Bare Connect predates profiles and must keep working for a user who
        // has no `default.toml`: it is read here only to learn whether an
        // interface was requested.
        let default_profile = profile::load_or_default("default").map_err(failed)?;
        default_profile.validate("default").map_err(failed)?;
        let default_routing = default_profile.routing_block().map_err(failed)?;
        let order = oxidom_core::sync::lock(&self.shared.diagnostic_order);
        let mut facts = Facts::default();
        let generation = {
            let mut engine = oxidom_core::sync::lock(&self.shared.engine);
            self.shared.ensure_open()?;
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
            engine.configure_core("default", &default_profile.core, default_routing);
            engine.configure_hold_traffic("default", default_profile.on_core_exit);
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
            self.shared.claim_generation("default", &mut facts)
        };
        drop(order);
        facts.emit(&self.shared.recorder);

        // Claimed only now that every fallible preparation is behind us, the
        // same order as `connect_pool` and `up_profile`: burning the generation
        // or the override on an attempt that then fails to start would cancel a
        // pending automatic reconnect and leave the session in Error for good.
        let result = self.shared.start_connection(
            "default",
            &server_id,
            generation,
            ConnectionOrigin::Explicit,
        );
        result.map(|_| ()).map_err(failed)
    }

    /// Run a selection now, without writing it anywhere.
    ///
    /// The pool counterpart of [`Service::connect`], and it works the same way:
    /// the session is named on the query (falling back to `default` for a client
    /// that predates the field), `<session>.toml` is read when present for the
    /// settings a session needs — its ports, its interface, its `[core]`, its
    /// routing block and what it does when a core exits — and **no profile file
    /// is written or modified**.
    ///
    /// Saving a selection is still a separate, deliberate act, and repointing a
    /// saved profile still goes through `SaveProfile` and still asks first.
    fn connect_pool(&self, query_json: String) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        let query: oxidom_core::pool::PoolQuery =
            serde_json::from_str(&query_json).map_err(failed)?;
        let session_name = query.session_or_default().to_string();
        if !profile::valid_name(&session_name) {
            return Err(failed(format!("invalid session name {session_name:?}")));
        }
        let session_profile = profile::load_or_default(&session_name).map_err(failed)?;
        session_profile.validate(&session_name).map_err(failed)?;
        let session_routing = session_profile.routing_block().map_err(failed)?;

        // Taken before the engine lock, and in this order everywhere: the two
        // are wanted together only here and in `up_profile`, and taking them the
        // other way round is how a deadlock gets written.
        let readings = oxidom_core::sync::lock(&self.shared.readings).clone();

        let order = oxidom_core::sync::lock(&self.shared.diagnostic_order);
        let mut facts = Facts::default();
        let (target, generation) = {
            let mut engine = oxidom_core::sync::lock(&self.shared.engine);
            self.shared.ensure_open()?;
            let (socks_port, http_port, _) = if session_name == "default" {
                (
                    engine.registry.config.socks_port,
                    engine.registry.config.http_port,
                    Vec::new(),
                )
            } else {
                self.shared.reconcile_profile_ports(
                    &engine.registry.config,
                    &session_name,
                    &session_profile,
                )
            };
            if socks_port == http_port {
                return Err(failed(
                    "the profile's SOCKS and HTTP inbounds would share a port",
                ));
            }
            if engine.registry.config.system_proxy
                && let Some(owner) = engine.sessions.owner_of_system_proxy()
                && owner != session_name
            {
                return Err(failed(format!(
                    "the system proxy is already held by profile {owner:?}"
                )));
            }
            let existed = engine.sessions.get(&session_name).is_some();
            let address = match engine.sessions.get(&session_name) {
                Some(existing) => existing.address,
                None => oxidom_core::bind::address_for(
                    &session_name,
                    &engine.sessions.taken_addresses(),
                )
                .ok_or_else(|| failed("no free profile loopback addresses remain"))?,
            };
            let (target, servers) = resolve_pool_target(
                &engine,
                &readings,
                "the selected group",
                &query,
                address,
                socks_port,
                http_port,
            )
            .map_err(failed)?;
            engine
                .prepare_session(&session_name, address, socks_port, http_port)
                .map_err(failed)?;
            engine.configure_core(&session_name, &session_profile.core, session_routing);
            engine.configure_hold_traffic(&session_name, session_profile.on_core_exit);
            if let Err(error) =
                engine.configure_interface(&session_name, &session_profile.interface, &servers)
            {
                if !existed {
                    engine.sessions.remove(&session_name);
                }
                return Err(failed(error));
            }
            if engine.registry.config.system_proxy {
                engine
                    .sessions
                    .claim_system_proxy(&session_name)
                    .map_err(failed)?;
            }
            let generation = self.shared.claim_generation(&session_name, &mut facts);
            (target, generation)
        };
        drop(order);
        facts.emit(&self.shared.recorder);
        let result = self.shared.start_connection_target(
            &session_name,
            &target,
            generation,
            ConnectionOrigin::Explicit,
        );
        result.map_err(failed)?;
        Ok(session_name)
    }

    fn disconnect(&self) -> fdo::Result<()> {
        let _request = self.shared.admit()?;
        self.stop_profile("default")?;
        Ok(())
    }

    fn list_profiles(&self) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
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
                    on_core_exit: profile.on_core_exit,
                    routing: profile.routing,
                }),
                Err(error) => log::warn!("skipping profile {name:?}: {error:#}"),
            }
        }
        json(&entries)
    }

    fn get_profile(&self, name: String) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        json(&profile::load(&name).map_err(failed)?)
    }

    fn save_profile(&self, name: String, profile_json: String) -> fdo::Result<()> {
        let _request = self.shared.admit()?;
        let profile: Profile = serde_json::from_str(&profile_json).map_err(failed)?;
        // `save` re-checks both the name and the ports. The daemon owns the
        // profiles directory — on the system bus it is root's — so validation
        // that only ran in the caller would be no validation at all.
        let _commit = self
            .shared
            .lifecycle
            .commit()
            .ok_or_else(|| failed("daemon is stopping"))?;
        profile::save(&name, &profile).map_err(failed)
    }

    fn remove_profile(&self, name: String) -> fdo::Result<bool> {
        let _request = self.shared.admit()?;
        let _commit = self
            .shared
            .lifecycle
            .commit()
            .ok_or_else(|| failed("daemon is stopping"))?;
        // The session/profile association deliberately survives the file: a
        // tunnel this profile brought up is still its tunnel, and `oxidom down
        // <profile>` (i.e. the unit's ExecStop) has to keep taking it down.
        profile::remove(&name).map_err(failed)
    }

    fn up_profile(&self, name: String) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        let profile = profile::load(&name).map_err(failed)?;
        // Only files written through `SaveProfile` were validated on the way
        // in; this one may have been edited by hand since.
        profile.validate(&name).map_err(failed)?;
        let profile_routing = profile.routing_block().map_err(failed)?;
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
        let order = oxidom_core::sync::lock(&self.shared.diagnostic_order);
        let mut facts = Facts::default();
        let (target, up_server, ignored_ports, generation) = {
            let mut engine = oxidom_core::sync::lock(&self.shared.engine);
            self.shared.ensure_open()?;
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
                let (target, servers) = resolve_pool_target(
                    &engine,
                    &readings,
                    &format!("profile {name:?}"),
                    query,
                    address,
                    socks_port,
                    http_port,
                )
                .map_err(failed)?;
                (target, servers, UpServer::default())
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
            engine.configure_core(&name, &profile.core, profile_routing);
            engine.configure_hold_traffic(&name, profile.on_core_exit);
            if let Err(error) = engine.configure_interface(&name, &profile.interface, &servers) {
                engine.sessions.remove(&name);
                return Err(failed(error));
            }
            if wants_system_proxy {
                engine.sessions.claim_system_proxy(&name).map_err(failed)?;
            }
            let generation = self.shared.claim_generation(&name, &mut facts);
            (target, up_server, ignored_ports, generation)
        };
        drop(order);
        facts.emit(&self.shared.recorder);
        let confirmation = self.shared.start_connection_target(
            &name,
            &target,
            generation,
            ConnectionOrigin::Explicit,
        );
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
        let _request = self.shared.admit()?;
        if profile.is_empty() {
            // The old single-session method remains unconditional. With
            // sessions that means bringing every profile down.
            self.stop_all_profiles()?;
            return Ok(true);
        }
        self.stop_profile(&profile)
    }

    fn list_sessions(&self) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        json(&self.shared.list_session_infos())
    }

    fn session_status(&self, profile: String) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        let session = self
            .shared
            .session_info(&profile)
            .ok_or_else(|| failed(oxidom_core::ipc::profile_not_up_message(&profile)))?;
        json(&session)
    }

    fn down_profile(&self, profile: String) -> fdo::Result<bool> {
        let _request = self.shared.admit()?;
        self.stop_profile(&profile)
    }

    fn delete_interface(&self, profile: String) -> fdo::Result<bool> {
        let _request = self.shared.admit()?;
        let order = oxidom_core::sync::lock(&self.shared.diagnostic_order);
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        self.shared.ensure_open()?;
        let result = engine.delete_interface(&profile);
        let mut facts = Facts::default();
        oxidom_core::sync::lock(&self.shared.identities).prune(&engine, &mut facts);
        drop(engine);
        drop(order);
        facts.emit(&self.shared.recorder);
        result.map_err(failed)
    }

    fn mark_cgroup(&self, profile: String, uid: u32) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        let slice = {
            let mut engine = oxidom_core::sync::lock(&self.shared.engine);
            self.shared.ensure_open()?;
            engine.mark_cgroup(&profile, uid).map_err(failed)?
        };
        json(&slice)
    }

    fn status(&self) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        json(&self.shared.status_info())
    }

    fn request_probe(&self, server_id: String) -> fdo::Result<()> {
        let _request = self.shared.admit()?;
        self.shared.enqueue_probe(server_id);
        Ok(())
    }

    fn request_probes(&self, server_ids: Vec<String>) -> fdo::Result<()> {
        let _request = self.shared.admit()?;
        for id in server_ids {
            self.shared.enqueue_probe(id);
        }
        Ok(())
    }

    /// Call off queued checks. Idempotent: asking twice, or for a server
    /// nothing is queued for, is not an error.
    ///
    /// Answers with how many were dropped, so a client can tell "there was
    /// nothing left to stop" from "the daemon does not know this method" —
    /// which is what the empty call is for, since an older daemon replies
    /// `UnknownMethod` to it and a current one replies zero.
    ///
    /// A check already measuring is not interrupted; at most eight are, and
    /// they end within about ten seconds. What this removes is the queue behind
    /// them, which on a large subscription is the minutes.
    fn cancel_probes(&self, server_ids: Vec<String>) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        let cancelled = self.shared.cancel_probes(&server_ids);
        json(&serde_json::json!({ "cancelled": cancelled }))
    }

    fn probe_state(&self) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
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

    /// What one server's recent checks measured, newest first.
    ///
    /// One server per call, and not folded into `ProbeState`, because that one
    /// is polled twice a second for every server a client knows about: ten
    /// readings each would multiply a standing broadcast to serve a panel only
    /// the expanded card draws. A server with no measured check yet answers an
    /// empty list rather than an error — there is nothing wrong with a server
    /// that has never been checked.
    fn probe_history(&self, server_id: String) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        let readings = oxidom_core::sync::lock(&self.shared.history)
            .get(&server_id)
            .cloned()
            .unwrap_or_default();
        json(&ProbeHistory { readings })
    }

    fn get_settings(&self) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        json(&oxidom_core::sync::lock(&self.shared.engine).registry.config)
    }

    fn set_settings(&self, config_json: String) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        let raw: serde_json::Value = serde_json::from_str(&config_json).map_err(failed)?;
        let mut config: Config = serde_json::from_value(raw.clone()).map_err(failed)?;
        let order = oxidom_core::sync::lock(&self.shared.diagnostic_order);
        let mut engine = oxidom_core::sync::lock(&self.shared.engine);
        self.shared.ensure_open()?;

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
        if raw.get("standby_proxy").is_none() {
            config.standby_proxy = engine.registry.config.standby_proxy;
        }
        if raw.get("reconnect").is_none() {
            config.reconnect = engine.registry.config.reconnect;
        }
        if raw.get("watchdog").is_none() {
            config.watchdog = engine.registry.config.watchdog;
        }
        if raw.get("on_core_exit").is_none() {
            config.on_core_exit = engine.registry.config.on_core_exit;
        }
        if raw.get("geoip_url").is_none() {
            config.geoip_url = engine.registry.config.geoip_url.clone();
        }
        if raw.get("geosite_url").is_none() {
            config.geosite_url = engine.registry.config.geosite_url.clone();
        }
        // Same, for the whole `[core]` section. A client that edits it always
        // sends the key, empty section included; one that does not know the
        // section omits it, and overwriting from that absence would erase a
        // hand-written fragment setting on the next Apply click.
        if raw.get("core").is_none() {
            config.core = engine.registry.config.core.clone();
        } else if raw.pointer("/core/pool_probe_url").is_none() {
            // And the same one key deeper. A client that knows `[core]` but not
            // this key sends the section without it, and serde reads that
            // absence as `None` — which would clear a pool health-check address
            // the user had set, on the next Apply from an older window.
            config.core.pool_probe_url = engine.registry.config.core.pool_probe_url.clone();
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
        let mut facts = Facts::default();
        if config.watchdog != engine.registry.config.watchdog {
            self.shared.watchdog_epoch.fetch_add(1, Ordering::SeqCst);
            oxidom_core::sync::lock(&self.shared.tunnel_failures).clear();
            oxidom_core::sync::lock(&self.shared.identities).flush_all(&mut facts);
        }
        engine.registry.config = config;
        let socks_port = engine.registry.config.socks_port;
        let http_port = engine.registry.config.http_port;
        let xray_binary = engine.registry.config.xray_binary.clone();
        engine.set_default_ports(socks_port, http_port);
        if let Some(session) = engine.sessions.get_mut("default") {
            session.core.xray_binary = xray_binary;
        }
        if let Err(error) = engine.save() {
            drop(engine);
            drop(order);
            facts.emit(&self.shared.recorder);
            return Err(failed(error));
        }
        let mut reconnect_error = None;
        let mut confirmation = None;
        if ports_changed && engine.status() == Status::Connected {
            self.shared.stop_standby();
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
                let context = oxidom_core::sync::lock(&self.shared.identities).start(
                    &self.shared.recorder,
                    "default",
                    generation,
                    Origin::Settings,
                    if target.server_id().is_some() {
                        TargetKind::Server
                    } else {
                        TargetKind::Pool
                    },
                    &mut facts,
                );
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
                    Ok(()) => {
                        let core = engine
                            .sessions
                            .get("default")
                            .and_then(|session| session.core.incarnation());
                        let context = oxidom_core::sync::lock(&self.shared.identities)
                            .started("default", context, core, false, &mut facts);
                        let snapshot = self
                            .shared
                            .capture_confirmation(&engine, "default", generation, context);
                        let probe_label = target.probe_label("default");
                        let probe_token = oxidom_core::sync::lock(&self.shared.probes)
                            .start_now("default", &probe_label, Some(generation))
                            .token;
                        confirmation = Some((target, generation, snapshot, probe_token));
                    }
                    Err(error) => {
                        if let Some(event) = trace::finished(
                            context,
                            ObservationOutcome::StartFailed,
                            true,
                            Some(&error),
                            None,
                        ) {
                            facts.push(event);
                        }
                        reconnect_error = Some(format!("{error:#}"));
                    }
                }
            }
        }
        drop(engine);
        drop(order);
        facts.emit(&self.shared.recorder);
        if let Some((target, generation, snapshot, probe_token)) = confirmation {
            self.shared.confirm_connection(
                "default".to_string(),
                target,
                generation,
                ConnectionOrigin::Explicit,
                probe_token,
                snapshot,
            );
        } else {
            self.shared.reconcile_standby();
        }
        json(&ApplySettingsResult {
            reconnect_error,
            ignored_ports,
            ignored_paths,
        })
    }

    fn runtime_info(&self) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        json(&self.shared.runtime_info())
    }

    /// Start installing the geo data the core needs.
    ///
    /// Returns as soon as the worker is running rather than when the files are
    /// there — 23 MB on the sort of link this program exists for outlives any
    /// single-call timeout, and a method that blocked would stall every other
    /// caller on this connection, including the log reads that carry the
    /// progress a user is watching. Poll [`Self::runtime_info`] for how it is
    /// getting on.
    ///
    /// `through_tunnel` asks for the fetch to go over a tunnel that is already
    /// up, which is the answer when the release host is blocked on the network
    /// the user is trying to escape. The *daemon* picks the proxy address from
    /// its own sessions; a caller-supplied one would be a request to send
    /// traffic anywhere it liked.
    ///
    /// The URLs are constants in `oxidom_core::xray::assets`, not parameters:
    /// a privileged daemon that fetched whatever address a caller named would
    /// be a very general tool.
    ///
    /// No privilege check beyond the bus policy, which already admits only
    /// root, `wheel` and `oxidom`. Unlike a binary path — which names something
    /// the daemon will *execute*, and is refused on a system daemon for that
    /// reason — this installs data from a fixed address, verified against a
    /// digest published beside it, that the caller cannot influence.
    fn download_geo_assets(&self, through_tunnel: bool) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        self.shared
            .start_geo_download(through_tunnel)
            .map_err(fdo::Error::Failed)?;
        json(&serde_json::json!({ "started": true }))
    }

    /// Stop a running download. Idempotent: asking twice, or when none is
    /// running, is not an error.
    fn cancel_geo_download(&self) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        self.shared.geo_cancel.cancel();
        json(&serde_json::json!({ "cancelled": true }))
    }

    /// Geo data directories on this machine whose contents the core accepts.
    ///
    /// Offered before a 23 MB download because many machines already carry
    /// these files from another client or an unrelated package. Every directory
    /// returned has been handed to the core and accepted, so this reports what
    /// is *usable* rather than what merely exists — a truncated list is
    /// rejected here exactly as it would be at connect time.
    fn find_geo_assets(&self) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        use oxidom_core::xray::assets;

        let Some(xray) = self.shared.resolved_xray() else {
            return Err(fdo::Error::Failed(
                "there is no Xray core to judge the files with".to_string(),
            ));
        };
        let scratch =
            Shared::geo_scratch().map_err(|error| fdo::Error::Failed(format!("{error:#}")))?;
        let dirs = assets::candidate_dirs_here(Some(&xray));
        json(&assets::usable_candidates(&xray, &dirs, &scratch))
    }

    /// Install geo data already on this machine, instead of downloading it.
    ///
    /// The directory must be one [`Self::find_geo_assets`] offered, re-checked
    /// here rather than trusted: this is a path from a caller, and a system
    /// daemon takes them from other users.
    fn adopt_geo_assets(&self, dir: String) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        use oxidom_core::xray::assets;

        let Some(xray) = self.shared.resolved_xray() else {
            return Err(fdo::Error::Failed(
                "there is no Xray core to judge the files with".to_string(),
            ));
        };
        let source = std::path::PathBuf::from(&dir);
        let scratch =
            Shared::geo_scratch().map_err(|error| fdo::Error::Failed(format!("{error:#}")))?;
        if assets::usable_candidates(&xray, std::slice::from_ref(&source), &scratch).is_empty() {
            return Err(fdo::Error::Failed(format!(
                "{dir} does not hold geo data this core will load"
            )));
        }
        let into = assets::own_dir().map_err(|error| fdo::Error::Failed(format!("{error:#}")))?;
        let commit = self
            .shared
            .lifecycle
            .commit()
            .ok_or_else(|| failed("daemon is stopping"))?;
        assets::adopt(&source, &into).map_err(|error| fdo::Error::Failed(format!("{error:#}")))?;
        drop(commit);
        log::info!("installed the geo data found in {dir}");
        // Measured before these files existed, so ask again on the next read.
        oxidom_core::sync::lock(&self.shared.geo).probed_for = None;
        json(&serde_json::json!({ "installed": true }))
    }

    /// Kept for clients older than [`Self::logs_since`].
    ///
    /// Now served from the process-wide book, so it finally answers for **every**
    /// session. It used to read `default_session()` alone, which meant the logs
    /// of any other profile were unreachable by the CLI, the GUI and the bus
    /// alike — the one place a failure explains itself, blank.
    fn recent_logs(&self) -> fdo::Result<Vec<String>> {
        let _request = self.shared.admit()?;
        Ok(oxidom_core::logbook::global().legacy_lines())
    }

    /// Structured log records after `after_seq`, at most `limit` of them.
    ///
    /// The cursor is the point: [`Self::recent_logs`] hands over the whole
    /// buffer every time it is called, so a reader cannot tell new lines from a
    /// reshuffled log and has to redraw. Passing what it has already seen makes
    /// a refresh an append.
    fn logs_since(&self, after_seq: u64, limit: u32) -> fdo::Result<String> {
        let _request = self.shared.admit()?;
        let limit = if limit == 0 {
            oxidom_core::logbook::DEFAULT_LIMIT
        } else {
            (limit as usize).min(oxidom_core::logbook::CAPACITY)
        };
        json(&oxidom_core::logbook::global().since(after_seq, limit))
    }

    fn clear_logs(&self) -> fdo::Result<()> {
        let _request = self.shared.admit()?;
        let _commit = self
            .shared
            .lifecycle
            .commit()
            .ok_or_else(|| failed("daemon is stopping"))?;
        oxidom_core::logbook::global().clear();
        Ok(())
    }

    /// Read the selected daemon's journal without taking application locks or
    /// occupying the D-Bus executor with filesystem I/O. No caller path is used.
    async fn diagnostics_since(&self, cursor: String, limit: u32) -> fdo::Result<String> {
        let ticket = self.shared.admit()?;
        tokio::task::spawn_blocking(move || {
            let _ticket = ticket;
            let path = oxidom_core::paths::daemon_log_file()
                .map_err(|_| fdo::Error::Failed("diagnostic path unavailable".into()))?;
            let page = oxidom_core::diagnostics::read(
                &path,
                (!cursor.is_empty()).then_some(cursor.as_str()),
                limit,
            )
            .map_err(|error| fdo::Error::Failed(format!("diagnostic read failed: {error}")))?;
            json(&page)
        })
        .await
        .map_err(|_| fdo::Error::Failed("diagnostic reader worker failed".into()))?
    }
}

fn spawn_core_supervisor(shared: Shared) {
    let lifecycle = shared.clone();
    lifecycle.spawn(move || {
        loop {
            if !shared.lifecycle.wait(CORE_WATCH_INTERVAL) {
                return;
            }
            shared.supervise_once();
        }
    });
}

fn check_bus(connection: &zbus::blocking::Connection) -> Result<()> {
    if connection.is_closed() {
        anyhow::bail!("D-Bus transport closed");
    }
    let owner = zbus::blocking::fdo::DBusProxy::new(connection)
        .ok()
        .and_then(|bus| {
            bus.get_name_owner(
                zbus::names::BusName::from_static_str(BUS_NAME).expect("static bus name"),
            )
            .ok()
        });
    match owner {
        Some(owner)
            if connection
                .unique_name()
                .is_some_and(|ours| ours.as_str() == owner.as_str()) =>
        {
            Ok(())
        }
        Some(_) => anyhow::bail!("D-Bus service name lost"),
        None => anyhow::bail!("D-Bus service ownership unavailable"),
    }
}

fn serve_until_terminal(
    shared: &Shared,
    setup: Result<zbus::blocking::Connection>,
    mut stop_requested: impl FnMut() -> Result<bool>,
    serving: impl FnOnce(&zbus::blocking::Connection),
) -> Result<()> {
    let setup = setup.and_then(|connection| {
        check_bus(&connection)?;
        Ok(connection)
    });
    let terminal = match &setup {
        Ok(connection) => {
            shared.reconcile_standby();
            shared.spawn_active_probe_loop();
            shared.spawn_tunnel_watchdog();
            shared.spawn_pool_status_loop();
            spawn_core_supervisor(shared.clone());
            serving(connection);
            loop {
                match stop_requested() {
                    Ok(true) => break Ok(()),
                    Ok(false) => {}
                    Err(error) => break Err(error),
                }
                if let Err(error) = check_bus(connection) {
                    break Err(error);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Err(_) => Ok(()),
    };
    let shutdown = shared.terminate();
    let terminal = setup.and(terminal);
    match (terminal, shutdown) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("terminal cleanup failed: {cleanup:#}")))
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

pub(super) fn run(
    options: DaemonOptions,
    diagnostics: &mut crate::diagnostics::DaemonLog,
) -> Result<()> {
    use oxidom_core::diagnostics::Phase;

    // Block SIGINT/SIGTERM before anything spawns a thread: zbus serves on its own
    // workers, and only threads created after the mask is set inherit it.
    // signalfd lets the serving thread observe shutdown without a waiter thread.
    diagnostics.phase(Phase::Signals);
    let mut stop = SigSet::empty();
    stop.add(Signal::SIGINT);
    stop.add(Signal::SIGTERM);
    stop.thread_block()?;
    let signals = nix::sys::signalfd::SignalFd::with_flags(
        &stop,
        nix::sys::signalfd::SfdFlags::SFD_NONBLOCK | nix::sys::signalfd::SfdFlags::SFD_CLOEXEC,
    )?;

    diagnostics.phase(Phase::Store);
    let store_lock = Arc::new(crate::store_lock::StoreLock::acquire(
        &oxidom_core::paths::data_dir()?,
    )?);
    diagnostics.phase(Phase::Recovery);
    let mut engine = Engine::load();
    diagnostics.catalog_load_warnings(engine.registry.load_warnings.len());
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
    diagnostics.phase(Phase::CoreResolution);
    let resolved = oxidom_core::xray::resolve::resolve(&engine.registry.config.xray_binary);
    match resolved {
        Ok(resolved) => log::info!(
            "using the Xray core at {} (from {})",
            resolved.path.display(),
            resolved.source.label(&oxidom_core::xray::resolve::XRAY)
        ),
        Err(error) => {
            diagnostics.problem(&error, false);
            log::warn!("no usable Xray core: {error:#}");
        }
    }

    // Seeded from the running config, so `oxidom up` works on a fresh install
    // without the user first writing a profile by hand. Never fatal: a daemon
    // that refuses to start because it could not write an example profile is
    // worse than one with no profiles.
    diagnostics.phase(Phase::DefaultProfile);
    if let Err(error) = profile::ensure_default(&engine) {
        diagnostics.problem(&error, false);
        log::warn!("could not create the default profile: {error:#}");
    }

    let mut shared = Shared::new_with_recorder(
        engine,
        options.socks_port.is_some(),
        options.http_port.is_some(),
        options.system_bus,
        diagnostics.recorder(),
    );
    shared.store_lease = Some(store_lock);

    diagnostics.phase(Phase::Bus);
    let setup = (|| -> Result<zbus::blocking::Connection> {
        let service = Service {
            shared: shared.clone(),
        };
        let builder = if options.system_bus {
            zbus::blocking::connection::Builder::system().context("connecting to the system bus")?
        } else {
            zbus::blocking::connection::Builder::session()
                .context("connecting to the session bus")?
        };
        let connection = builder
            .method_timeout(Duration::from_secs(1))
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .name(BUS_NAME)
            .context("claiming the bus name (is another oxidom daemon running?)")?
            .serve_at(OBJECT_PATH, service)
            .context("registering the service object")?
            .build()
            .context("starting the D-Bus service")?;
        Ok(connection)
    })();
    let shutdown = serve_until_terminal(
        &shared,
        setup,
        || match signals.read_signal() {
            Ok(signal) => Ok(signal.is_some()),
            Err(nix::errno::Errno::EINTR) => Ok(false),
            Err(error) => Err(anyhow::Error::new(error).context("reading shutdown signal")),
        },
        |_| {
            diagnostics.phase(Phase::Serving);
            log::info!(
                "oxidom daemon serving {BUS_NAME} on the {} bus",
                if options.system_bus {
                    "system"
                } else {
                    "session"
                }
            );
        },
    );
    diagnostics.close_runtime();
    diagnostics.phase(Phase::Shutdown);
    if let Err(error) = &shutdown {
        diagnostics.problem(error, false);
        log::warn!("could not persist the clean daemon shutdown: {error:#}");
    }
    diagnostics.phase(Phase::Stopped);
    log::info!("oxidom daemon stopped");
    shutdown
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
    probe_outcome: Option<&probe::ProbeOutcome>,
    configured_method: LatencyMethod,
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
        return single_server_confirmation_failure(logs, probe_outcome, configured_method);
    };
    // A count on its own names the symptom and not the cause, and under
    // roundRobin it is actively misleading: every node stays eligible, so the
    // message reads "3 of 3 nodes were in rotation" while nothing works. The
    // balancer puts a node in rotation only once the observatory's ping has
    // come back through it, so the core's own failed pings outrank the count —
    // and they point at the one thing a user can change.
    let failed = observatory_failures(logs);
    let seen = if failed > 0 {
        format!(
            "its health check could not be reached through {failed} of {} nodes — the address is \
             [core] pool_probe_url",
            members.len()
        )
    } else {
        match rotating {
            Some(live) => format!("{live} of {} nodes were in rotation", members.len()),
            None => format!("none of its {} nodes answered", members.len()),
        }
    };
    format!(
        "the pool carried no traffic within {}s — {seen}",
        POOL_CONFIRM_WINDOW.as_secs()
    )
}

/// Explain why a single-server session's confirmation probe failed.
///
/// The old wording — "active server did not pass its latency check" — said
/// nothing about whether the probe timed out, the server reset the connection,
/// the machine had no network, or a direct ICMP/TCP ping had only tested the
/// host rather than the proxy outbound.
fn single_server_confirmation_failure(
    logs: &[String],
    probe_outcome: Option<&probe::ProbeOutcome>,
    configured_method: LatencyMethod,
) -> String {
    if let Some(reason) = core_outbound_failure(logs) {
        return format!("active server did not pass its latency check — {reason}");
    }
    let non_http_note = match configured_method {
        LatencyMethod::Icmp => {
            " (ICMP checks only whether the host answers ping, not whether the proxy carries traffic)"
        }
        LatencyMethod::Tcp => {
            " (TCP checks only whether the port accepts a connection, not whether the proxy carries traffic)"
        }
        LatencyMethod::HttpHead | LatencyMethod::HttpGet => "",
    };
    match probe_outcome {
        Some(probe::ProbeOutcome::NoNetwork) => {
            format!(
                "active server did not pass its latency check — {}",
                ProbeFailure::NoNetwork.message()
            )
        }
        Some(probe::ProbeOutcome::Internal(detail)) => {
            format!(
                "active server did not pass its latency check — {}",
                detail.message()
            )
        }
        Some(probe::ProbeOutcome::Timeout) => {
            format!(
                "active server did not pass its latency check — the request through the tunnel timed out after {}s{non_http_note}",
                probe::REAL_DELAY_TIMEOUT.as_secs()
            )
        }
        Some(probe::ProbeOutcome::Unreachable) => {
            format!(
                "active server did not pass its latency check — the tunnel closed the connection without carrying the request{non_http_note}"
            )
        }
        _ => "active server did not pass its latency check".to_string(),
    }
}

/// Extract a human-readable outbound failure from Xray's own log lines when
/// the error is not one of the local `ProbeDetail` variants.
fn core_outbound_failure(logs: &[String]) -> Option<String> {
    for line in logs.iter().rev() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("connection reset by peer") {
            return Some("the server reset the connection during the handshake".to_string());
        }
        if lower.contains("connection refused") {
            return Some("the server refused the connection".to_string());
        }
        if lower.contains("handshake timeout") || lower.contains("tls handshake") {
            return Some("the TLS/REALITY handshake timed out".to_string());
        }
        if lower.contains("context deadline exceeded") || lower.contains("i/o timeout") {
            return Some("the outbound connection to the server timed out".to_string());
        }
        if lower.contains("no such host") || lower.contains("Temporary failure in name resolution")
        {
            return Some("the server's hostname could not be resolved".to_string());
        }
        if lower.contains("failed to process outbound traffic")
            || lower.contains("failed to dial")
            || lower.contains("failed to handler mux client")
        {
            // An arbitrary core complaint may contain credentials or endpoints;
            // only the classified, fixed wording above is safe to repeat.
            return Some("the core could not establish an outbound connection".to_string());
        }
    }
    None
}

/// Where the burst observatory names itself in the core's own log.
const OBSERVATORY_SOURCE: &str = "app/observatory/burst";

/// How many distinct pool members the core failed to reach its health-check
/// destination through.
///
/// Deliberately not folded into `probe::classify_complaint`, which is shared
/// with the single-server probe path and answers with a `ProbeDetail` — a
/// verdict on one server. An observatory failure is a verdict on the
/// destination, and teaching that function about it would make a single-server
/// check report a pool's wording.
///
/// The line is the core's, and reads
/// `WARN xray/app/observatory/burst  error ping <url> with <tag>: ...`.
fn observatory_failures(logs: &[String]) -> usize {
    let mut tags: HashSet<&str> = HashSet::new();
    for line in logs {
        if !line.contains(OBSERVATORY_SOURCE) || !line.contains("error ping") {
            continue;
        }
        // The tag sits between " with " and the colon that opens the Go error.
        // Anything else on the line is the destination, which is already known.
        if let Some(tail) = line.split(" with ").nth(1)
            && let Some(tag) = tail.split(':').next()
            && !tag.trim().is_empty()
        {
            tags.insert(tag.trim());
        }
    }
    tags.len()
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

    struct PrivateBus(std::process::Child);

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn run_private_serving(
        shared: Shared,
        setup: Result<zbus::blocking::Connection>,
        serving: impl FnOnce(&zbus::blocking::Connection) + Send + 'static,
    ) -> Result<()> {
        let (done, result) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let outcome = serve_until_terminal(
                &shared,
                setup,
                || {
                    anyhow::ensure!(
                        Instant::now() < deadline,
                        "private serving deadline exhausted"
                    );
                    Ok(false)
                },
                serving,
            );
            let _ = done.send(outcome);
        });
        let outcome = result.recv_timeout(Duration::from_secs(10))?;
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("private serving worker panicked"))?;
        outcome
    }

    #[test]
    fn private_bus_name_and_transport_loss_close_admission_and_drain_workers() -> Result<()> {
        use std::io::BufRead;
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("terminal-private-bus")?;
        let binary =
            std::env::var_os("OXIDOM_TEST_DBUS_DAEMON").unwrap_or_else(|| "dbus-daemon".into());
        let config = _root.path.join("private-bus.conf");
        std::fs::write(
            &config,
            b"<busconfig><type>session</type><listen>unix:tmpdir=/tmp</listen>\
              <auth>EXTERNAL</auth><policy context=\"default\">\
              <allow send_destination=\"*\"/><allow receive_sender=\"*\"/>\
              <allow own=\"*\"/><allow user=\"*\"/></policy></busconfig>",
        )?;
        let mut bus = PrivateBus(
            std::process::Command::new(binary)
                .arg(format!("--config-file={}", config.display()))
                .args(["--nofork", "--print-address=1"])
                .env("HOME", &_root.path)
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .env_remove("DBUS_SYSTEM_BUS_ADDRESS")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .context("private dbus-daemon is required for terminal regression")?,
        );
        let stdout = bus.0.stdout.take().context("bus stdout")?;
        let address = std::thread::scope(|scope| -> Result<String> {
            let (ready, address) = mpsc::channel();
            scope.spawn(move || {
                let mut line = String::new();
                let result = std::io::BufReader::new(stdout)
                    .read_line(&mut line)
                    .map(|_| line);
                let _ = ready.send(result);
            });
            match address.recv_timeout(Duration::from_secs(5)) {
                Ok(result) => Ok(result?),
                Err(error) => {
                    let _ = bus.0.kill();
                    let _ = bus.0.wait();
                    Err(error.into())
                }
            }
        })?;
        let address = address.trim();
        if address.is_empty() {
            use std::io::Read;
            let status = bus.0.wait()?;
            let mut stderr = String::new();
            bus.0
                .stderr
                .take()
                .context("bus stderr")?
                .read_to_string(&mut stderr)?;
            anyhow::bail!("private bus exited {status}: {stderr}");
        }
        let service = for_test();
        let shared = service.shared.clone();
        let connection = zbus::blocking::connection::Builder::address(address)?
            .method_timeout(Duration::from_secs(1))
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, service)?
            .build()?;
        check_bus(&connection)?;
        let client = zbus::blocking::connection::Builder::address(address)?
            .method_timeout(Duration::from_secs(1))
            .build()?;
        let unique = connection
            .unique_name()
            .context("private service name")?
            .as_str();
        let proxy =
            zbus::blocking::Proxy::new(&client, unique, OBJECT_PATH, "dev.keepinfov.oxidom1")?;
        let _: String = proxy.call("Status", &())?;
        let contender = for_test();
        let rejected = contender.shared.clone();
        let collision = zbus::blocking::connection::Builder::address(address)?
            .method_timeout(Duration::from_secs(1))
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, contender)?
            .build();
        let collision = run_private_serving(
            rejected.clone(),
            collision.map_err(anyhow::Error::new),
            |_| panic!("name collision must not start workers"),
        );
        assert!(collision.is_err());
        assert!(rejected.admit().is_err());
        let loss = run_private_serving(shared.clone(), Ok(connection.clone()), |connection| {
            assert!(connection.release_name(BUS_NAME).unwrap());
        });
        assert!(
            loss.unwrap_err()
                .to_string()
                .contains("ownership unavailable")
        );
        assert!(shared.admit().is_err());
        let rejected_call: zbus::Result<String> = proxy.call("Status", &());
        assert!(rejected_call.is_err());

        let service = for_test();
        let signalled = service.shared.clone();
        let signal_connection = zbus::blocking::connection::Builder::address(address)?
            .method_timeout(Duration::from_secs(1))
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, service)?
            .build()?;
        serve_until_terminal(
            &signalled,
            Ok(signal_connection),
            || Ok(true),
            |connection| {
                assert!(connection.release_name(BUS_NAME).unwrap());
            },
        )?;
        assert!(signalled.admit().is_err());

        let service = for_test();
        let shared = service.shared.clone();
        let connection = zbus::blocking::connection::Builder::address(address)?
            .method_timeout(Duration::from_secs(1))
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, service)?
            .build()?;
        check_bus(&connection)?;
        let loss = run_private_serving(shared.clone(), Ok(connection), move |_| {
            bus.0.kill().unwrap();
            bus.0.wait().unwrap();
        });
        let error = loss.unwrap_err().to_string();
        assert!(
            error.contains("transport closed") || error.contains("ownership unavailable"),
            "{error}"
        );
        assert!(shared.admit().is_err());
        Ok(())
    }

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

    /// Re-executed only by the fake binary below. All SOCKS destinations receive
    /// a local canned response; the child never resolves or dials the target.
    #[test]
    fn fake_core_serves_offline_socks_requests() -> Result<()> {
        use std::io::{Read, Write};
        let Some(path) = std::env::var_os("OXIDOM_TEST_CORE_CONFIG") else {
            return Ok(());
        };
        let config: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        for inbound in config["inbounds"].as_array().context("fake inbounds")? {
            let address = inbound["listen"].as_str().context("fake listen address")?;
            let port = inbound["port"].as_u64().context("fake inbound port")? as u16;
            let listener = std::net::TcpListener::bind((address, port))?;
            let socks = inbound["protocol"] == "socks";
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    std::thread::spawn(move || -> std::io::Result<()> {
                        let mut stream = stream;
                        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                        if !socks {
                            return Ok(());
                        }
                        let mut greeting = [0; 2];
                        stream.read_exact(&mut greeting)?;
                        stream.read_exact(&mut vec![0; usize::from(greeting[1])])?;
                        stream.write_all(&[5, 0])?;
                        let mut request = [0; 4];
                        stream.read_exact(&mut request)?;
                        let address_len = match request[3] {
                            1 => 4,
                            4 => 16,
                            3 => {
                                let mut length = [0];
                                stream.read_exact(&mut length)?;
                                usize::from(length[0])
                            }
                            _ => return Ok(()),
                        };
                        stream.read_exact(&mut vec![0; address_len + 2])?;
                        stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])?;
                        let mut headers = Vec::new();
                        while !headers.ends_with(b"\r\n\r\n") && headers.len() < 4096 {
                            let mut byte = [0];
                            stream.read_exact(&mut byte)?;
                            headers.push(byte[0]);
                        }
                        stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                    });
                }
            });
        }
        loop {
            std::thread::park();
        }
    }

    fn fake_core_binary(root: &TestRoot) -> Result<String> {
        use std::os::unix::fs::PermissionsExt;
        let path = root.path.join("fake-xray");
        let executable = format!(
            "'{}'",
            std::env::current_exe()?
                .to_str()
                .context("test binary path")?
                .replace('\'', "'\\''")
        );
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncase \"$1\" in\nversion) printf 'Xray 26.3.27 (offline test core)\\n';;\nrun) export OXIDOM_TEST_CORE_CONFIG=\"$3\"; exec {executable} --exact daemon::tests::fake_core_serves_offline_socks_requests --nocapture;;\n*) exit 1;;\nesac\n"
            ),
        )?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        Ok(path.display().to_string())
    }

    fn configure_fake_core(service: &Service, root: &TestRoot) -> Result<()> {
        let mut engine = oxidom_core::sync::lock(&service.shared.engine);
        engine.registry.config.xray_binary = fake_core_binary(root)?;
        engine.registry.config.latency_test_url = "http://offline.example.invalid/check".into();
        engine.registry.config.system_proxy = false;
        engine.registry.config.reconnect = false;
        // Reserve arbitrary ports until both have been selected.
        let socks = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let http = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        engine.registry.config.socks_port = socks.local_addr()?.port();
        engine.registry.config.http_port = http.local_addr()?.port();
        for name in ["work", "home"] {
            profile::save(
                name,
                &Profile {
                    proxy: oxidom_core::profile::ProfileProxy {
                        socks_port: engine.registry.config.socks_port,
                        http_port: engine.registry.config.http_port,
                    },
                    ..Default::default()
                },
            )?;
        }
        engine.import_links("trojan://invented-one@one.example.invalid:443#One\ntrojan://invented-two@two.example.invalid:443#Two")?;
        Ok(())
    }

    fn confirm_live_session(service: &Service, name: &str) -> Result<u32> {
        let generation = service
            .shared
            .current_generation(name)
            .context("live generation")?;
        let (target, address, socks_port) = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            let session = engine.sessions.get_mut(name).context("live session")?;
            assert!(session.is_alive(), "a real fake-core child is running");
            (
                reconnect_target(session).context("live selection")?,
                session.address,
                session.socks_port,
            )
        };
        assert!(probe::wait_for_socks(address, socks_port));
        assert!(
            service
                .shared
                .run_session_probe(name, Some(generation))
                .is_some(),
            "fake core actually carries an offline HTTP request"
        );
        let token = oxidom_core::sync::lock(&service.shared.probes)
            .start_now(name, &target.probe_label(name), Some(generation))
            .token;
        let snapshot = {
            let engine = oxidom_core::sync::lock(&service.shared.engine);
            let context = oxidom_core::sync::lock(&service.shared.identities).context(name);
            service
                .shared
                .capture_confirmation(&engine, name, generation, context)
        };
        assert!(
            service
                .shared
                .confirm_connection(
                    name.to_string(),
                    target,
                    generation,
                    ConnectionOrigin::Explicit,
                    token,
                    snapshot,
                )
                .recv_timeout(Duration::from_secs(5))?
        );
        let engine = oxidom_core::sync::lock(&service.shared.engine);
        engine
            .sessions
            .get(name)
            .context("session")?
            .core
            .child_pid()
            .context("fake child pid")
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

    fn start_attributed_fake_session(
        service: &Service,
        profile: &str,
        server: &str,
        retry_generation: Option<u64>,
    ) -> Result<Confirmation> {
        let generation = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            let address = engine
                .sessions
                .get(profile)
                .map(|session| session.address)
                .or_else(|| bind::address_for(profile, &engine.sessions.taken_addresses()))
                .context("fake session address")?;
            let socks = engine.registry.config.socks_port;
            let http = engine.registry.config.http_port;
            engine.prepare_session(profile, address, socks, http)?;
            retry_generation.unwrap_or_else(|| service.shared.next_connect_generation(profile))
        };
        assert!(
            service
                .shared
                .start_connection(
                    profile,
                    server,
                    generation,
                    if retry_generation.is_some() {
                        ConnectionOrigin::Reconnect
                    } else {
                        ConnectionOrigin::Explicit
                    }
                )?
                .context("accepted fake start")?
                .recv_timeout(Duration::from_secs(5))?
        );
        let engine = oxidom_core::sync::lock(&service.shared.engine);
        let context = oxidom_core::sync::lock(&service.shared.identities).context(profile);
        Ok(service
            .shared
            .capture_confirmation(&engine, profile, generation, context))
    }

    #[test]
    fn same_generation_retries_get_new_attempts_and_cores_and_keep_old_completions() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("attributed-retry")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        let a = start_attributed_fake_session(&service, "default", &server, None)?;
        let a_context = a.context.context("A attribution")?;
        let watched = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.watchdog = WatchdogMode::Aggressive;
            service
                .shared
                .watchdog_attempt(&engine, "default")
                .context("A watch")?
        };
        for _ in 0..2 {
            assert!(!service.shared.note_tunnel_probe(
                "default",
                watched,
                WatchdogMode::Aggressive,
                Some(&probe::ProbeOutcome::Timeout)
            ));
        }
        let b = start_attributed_fake_session(&service, "default", &server, Some(a.generation))?;
        let b_context = b.context.context("B attribution")?;
        assert_eq!(a_context.session, b_context.session);
        assert_eq!(a_context.generation, b_context.generation);
        assert_ne!(a_context.attempt, b_context.attempt);
        assert!(a_context.core.is_some());
        assert!(b_context.core.is_some());
        assert_ne!(a_context.core, b_context.core);
        // A captured before B must not clone B's server/config for measurement.
        assert!(
            service
                .shared
                .session_probe_outcome_for("default", Duration::ZERO, Some(a))
                .is_none()
        );
        let target = ConnectionTarget::Server(server);
        assert!(!service.shared.apply_confirmation_success(
            "default",
            &target,
            ConnectionOrigin::Explicit,
            a
        ));
        service.shared.apply_confirmation_failure(
            "default",
            &target,
            ConnectionOrigin::Explicit,
            a,
            ConfirmationEnd::Measured,
            Some(&probe::ProbeOutcome::Timeout),
        );
        for _ in 0..2 {
            assert!(!service.shared.note_tunnel_probe(
                "default",
                watched,
                WatchdogMode::Aggressive,
                Some(&probe::ProbeOutcome::Timeout)
            ));
        }
        let events = service.shared.recorder.events();
        for outcome in [ObservationOutcome::Reachable, ObservationOutcome::Timeout] {
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, Event::AttemptFinished {
                context, outcome: found, disposition: Disposition::DiscardedStale, ..
            } if *context == a_context && *found == outcome))
            );
        }
        assert_eq!(
            service
                .shared
                .session_info("default")
                .context("B session")?
                .state,
            "connected"
        );
        service.stop_all_profiles()?;
        service.shared.recorder.close_runtime();
        assert_eq!(
            service
                .shared
                .recorder
                .events()
                .iter()
                .filter(|event| matches!(event,
            Event::WatchSummary { key, suppressed_count: 1, .. } if key.context == a_context))
                .count(),
            1
        );
        assert_eq!(service.shared.recorder.events().iter().filter(|event| matches!(event,
            Event::Watch { context, action: WatchAction::DiscardedStale, .. } if *context == a_context)).count(), 2);
        Ok(())
    }

    #[test]
    fn liveness_and_retry_transitions_keep_the_observed_subject_after_replacement() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("attributed-liveness-retry")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        let a = start_attributed_fake_session(&service, "default", &server, None)?;
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.reconnect = true;
            let session = engine.sessions.get_mut("default").context("session")?;
            // Reap a verified fake child synchronously, then expose the death
            // sweep's pre-observation status without a timing sleep or fake PID.
            session.core.disconnect();
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
        }
        let mut deaths = service.shared.capture_core_deaths();
        assert_eq!(deaths.len(), 1);
        let (profile, target, observed) = deaths.pop().context("death")?;
        assert_eq!(observed.context, a.context);
        assert!(service.shared.claim_reconnect(&profile, &target, observed));
        assert!(!service.shared.claim_reconnect(&profile, &target, observed));
        let b = start_attributed_fake_session(&service, "default", &server, Some(a.generation))?;
        service
            .shared
            .retry_fact(&profile, observed.context, RetryState::Recovered, 0);
        assert_ne!(a.context, b.context);
        assert!(!service.shared.claim_reconnect(&profile, &target, observed));
        let events = service.shared.recorder.events();
        assert!(
            events.iter().any(|event| matches!(event,
            Event::LivenessLost { context, holding_traffic: true } if Some(*context) == a.context))
        );
        for state in [RetryState::Pending, RetryState::Recovered] {
            assert_eq!(events.iter().filter(|event| matches!(event,
                Event::Retry { context, state: found, .. } if Some(*context) == a.context && *found == state)).count(), 1);
        }
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn typed_fact_collection_does_not_emit_while_application_locks_are_owned() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("attribution-capture-without-io")?;
        let service = for_test();
        let mut facts = Facts::default();
        {
            let engine = oxidom_core::sync::lock(&service.shared.engine);
            let _probes = oxidom_core::sync::lock(&service.shared.probes);
            let _generations = oxidom_core::sync::lock(&service.shared.current_generations);
            let _streaks = oxidom_core::sync::lock(&service.shared.tunnel_failures);
            let mut identities = oxidom_core::sync::lock(&service.shared.identities);
            let context = identities.start(
                &service.shared.recorder,
                "default",
                1,
                Origin::Explicit,
                TargetKind::Server,
                &mut facts,
            );
            assert!(context.is_some());
            identities.flush_all(&mut facts);
            identities.prune(&engine, &mut facts);
            assert!(service.shared.recorder.events().is_empty());
        }
        facts.emit(&service.shared.recorder);
        assert!(
            service
                .shared
                .recorder
                .events()
                .iter()
                .any(|event| matches!(event, Event::SessionOpened { .. }))
        );
        Ok(())
    }

    #[test]
    fn production_emission_releases_application_locks_including_the_diagnostic_gate() -> Result<()>
    {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("diagnostic-emission-lock-observer")?;
        let mut shared = Shared::new(Engine::load(), false, false, false);
        let engine = Arc::downgrade(&shared.engine);
        let probes = Arc::downgrade(&shared.probes);
        let identities = Arc::downgrade(&shared.identities);
        let generations = Arc::downgrade(&shared.current_generations);
        let failures = Arc::downgrade(&shared.tunnel_failures);
        let order = Arc::downgrade(&shared.diagnostic_order);
        let proxied = Arc::downgrade(&shared.proxied);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let writes = observed.clone();
        shared.recorder = crate::diagnostics::Recorder::for_test_with_observer(move |event| {
            assert!(
                order.upgrade().expect("gate exists").try_lock().is_ok(),
                "diagnostic_order is held at emission"
            );
            assert!(
                engine.upgrade().expect("engine exists").try_lock().is_ok(),
                "engine is held at emission"
            );
            assert!(
                probes.upgrade().expect("probes exist").try_lock().is_ok(),
                "probes are held at emission"
            );
            assert!(
                identities
                    .upgrade()
                    .expect("identities exist")
                    .try_lock()
                    .is_ok(),
                "identities are held at emission"
            );
            assert!(
                generations
                    .upgrade()
                    .expect("generations exist")
                    .try_lock()
                    .is_ok(),
                "generations are held at emission"
            );
            assert!(
                failures
                    .upgrade()
                    .expect("streaks exist")
                    .try_lock()
                    .is_ok(),
                "tunnel_failures are held at emission"
            );
            if matches!(
                event,
                Event::AttemptFinished {
                    outcome: ObservationOutcome::StartFailed,
                    ..
                }
            ) {
                let probes = probes.upgrade().expect("probes exist");
                let probes = probes
                    .try_lock()
                    .unwrap_or_else(|_| panic!("probes locked at failed-start emission"));
                assert!(
                    probes.running.is_empty(),
                    "failed probe cleanup precedes emission"
                );
                drop(probes);
                let proxied = proxied.upgrade().expect("readings exist");
                let proxied = proxied
                    .try_lock()
                    .unwrap_or_else(|_| panic!("readings locked at failed-start emission"));
                assert_eq!(proxied.get("default").expect("failed reading").value, None);
            }
            oxidom_core::sync::lock(&writes).push(event);
        });
        let service = Service { shared };
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        let snapshot = start_attributed_fake_session(&service, "default", &server, None)?;
        let watched = {
            let engine = oxidom_core::sync::lock(&service.shared.engine);
            service
                .shared
                .watchdog_attempt(&engine, "default")
                .context("watch subject")?
        };
        let (_, _, threshold) = watchdog_policy(WatchdogMode::Conservative).context("policy")?;
        for strike in 1..=threshold {
            assert_eq!(
                service.shared.note_tunnel_probe(
                    "default",
                    watched,
                    WatchdogMode::Conservative,
                    Some(&probe::ProbeOutcome::Timeout)
                ),
                strike == threshold
            );
        }
        assert!(service.shared.retain_conservative_tunnel(
            "default",
            watched,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        assert!(!service.shared.note_tunnel_probe(
            "default",
            watched,
            WatchdogMode::Conservative,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        assert!(!service.shared.note_tunnel_probe(
            "default",
            watched,
            WatchdogMode::Conservative,
            Some(&probe::ProbeOutcome::NoNetwork)
        ));
        // A partial summary exercises Recorder::retire, not just record().
        for _ in 0..2 {
            assert!(!service.shared.note_tunnel_probe(
                "default",
                watched,
                WatchdogMode::Conservative,
                Some(&probe::ProbeOutcome::Timeout)
            ));
        }
        assert!(
            service
                .shared
                .start_connection(
                    "default",
                    "invented-missing-server",
                    snapshot.generation,
                    ConnectionOrigin::Reconnect
                )
                .is_err()
        );
        assert!(service.stop_profile("default")?);
        assert!(!service.shared.note_tunnel_probe(
            "default",
            watched,
            WatchdogMode::Conservative,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        service.shared.recorder.close_runtime();
        let events = oxidom_core::sync::lock(&observed);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::AttemptStarted { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::CoreStarted { adopted: false, .. }))
        );
        for outcome in [
            ObservationOutcome::Reachable,
            ObservationOutcome::StartFailed,
        ] {
            assert!(events.iter().any(|event| matches!(event, Event::AttemptFinished { outcome: found, .. } if *found == outcome)));
        }
        for expected in [
            WatchAction::Observed,
            WatchAction::Reset,
            WatchAction::ConservativeRetained,
            WatchAction::DiscardedStale,
        ] {
            assert!(
                events.iter().any(
                    |event| matches!(event, Event::Watch { action, .. } if *action == expected)
                )
            );
        }
        assert!(events.iter().any(|event| matches!(
            event,
            Event::WatchSummary {
                reason: oxidom_core::diagnostics::FlushReason::ContextEnded,
                ..
            }
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::SessionRetired { .. }))
        );
        Ok(())
    }

    #[test]
    fn same_server_profiles_and_removed_then_recreated_sessions_have_independent_ids() -> Result<()>
    {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("attributed-session-lifetime")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        let a = start_attributed_fake_session(&service, "work", &server, None)?
            .context
            .context("work context")?;
        let home = start_attributed_fake_session(&service, "home", &server, None)?
            .context
            .context("home context")?;
        assert_ne!(a.session, home.session);
        assert_ne!(a.core, home.core);
        service.stop_profile("work")?;
        let b = start_attributed_fake_session(&service, "work", &server, None)?
            .context
            .context("new work context")?;
        assert_ne!(a.session, b.session);
        assert!(
            service
                .shared
                .recorder
                .events()
                .contains(&Event::SessionRetired { context: a })
        );
        assert_eq!(
            oxidom_core::sync::lock(&service.shared.identities).context("home"),
            Some(home)
        );
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn a_failed_start_never_borrows_the_previous_core_identity() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("attributed-failed-start")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        let a = start_attributed_fake_session(&service, "default", &server, None)?;
        assert!(
            service
                .shared
                .start_connection(
                    "default",
                    "invented-missing-server",
                    a.generation,
                    ConnectionOrigin::Reconnect
                )
                .is_err()
        );
        let context = oxidom_core::sync::lock(&service.shared.identities)
            .context("default")
            .context("failed context")?;
        assert_eq!(context.core, None);
        assert_ne!(context.attempt, a.context.context("old context")?.attempt);
        assert!(
            service
                .shared
                .recorder
                .events()
                .iter()
                .any(|event| matches!(event,
            Event::AttemptFinished { context: found, outcome: ObservationOutcome::StartFailed,
                disposition: Disposition::Accepted, .. } if *found == context))
        );
        assert!(
            !service
                .shared
                .recorder
                .events()
                .iter()
                .any(|event| matches!(event,
            Event::CoreStarted { context: found, .. } if found.attempt == context.attempt))
        );
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn server_removal_retires_each_affected_profile_without_relabelling_a_survivor() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("attributed-server-removal")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let servers = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .map(|server| server.id.clone())
            .collect::<Vec<_>>();
        let work = start_attributed_fake_session(&service, "work", &servers[0], None)?;
        let home = start_attributed_fake_session(&service, "home", &servers[0], None)?;
        let survivor = start_attributed_fake_session(&service, "default", &servers[1], None)?;
        assert!(service.remove_server(servers[0].clone())?);
        let events = service.shared.recorder.events();
        for (profile, snapshot) in [("work", work), ("home", home)] {
            assert!(events.contains(&Event::SessionRetired {
                context: snapshot.context.context("removed context")?
            }));
            assert!(
                oxidom_core::sync::lock(&service.shared.identities)
                    .observed_core(profile, snapshot.core)
                    .is_none()
            );
        }
        assert_eq!(
            oxidom_core::sync::lock(&service.shared.identities).context("default"),
            survivor.context
        );
        assert!(!service.shared.apply_confirmation_success(
            "work",
            &ConnectionTarget::Server(servers[0].clone()),
            ConnectionOrigin::Explicit,
            work
        ));
        assert!(
            service
                .shared
                .recorder
                .events()
                .iter()
                .any(|event| matches!(event,
            Event::AttemptFinished { context, disposition: Disposition::DiscardedStale, .. }
                if Some(*context) == work.context))
        );
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn settings_port_restarts_are_distinct_attributed_attempts() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("attributed-settings-restart")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        let a = start_attributed_fake_session(&service, "default", &server, None)?
            .context
            .context("old context")?;
        let mut config = oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .clone();
        let socks = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let http = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        config.socks_port = socks.local_addr()?.port();
        config.http_port = http.local_addr()?.port();
        drop((socks, http));
        let result: ApplySettingsResult =
            serde_json::from_str(&service.set_settings(serde_json::to_string(&config)?)?)?;
        assert!(result.reconnect_error.is_none());
        confirm_live_session(&service, "default")?;
        let b = oxidom_core::sync::lock(&service.shared.identities)
            .context("default")
            .context("new context")?;
        assert_eq!(a.session, b.session);
        assert_ne!(a.generation, b.generation);
        assert_ne!(a.attempt, b.attempt);
        assert_ne!(a.core, b.core);
        assert!(
            service
                .shared
                .recorder
                .events()
                .iter()
                .any(|event| matches!(event,
            Event::AttemptStarted { context, origin: Origin::Settings, .. }
            if context.attempt == b.attempt && context.core.is_none()))
        );
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn constructor_records_real_preexisting_cores_as_adopted_without_fresh_confirmation()
    -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("attributed-adoption")?;
        let mut engine = Engine::load();
        engine.registry.config.xray_binary = fake_core_binary(&root)?;
        engine.registry.config.system_proxy = false;
        engine.import_links("trojan://invented-adoption@adoption.example.invalid:443#Adopted")?;
        let server = engine.all_servers().next().context("server")?.id.clone();
        let socks = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let http = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        engine.prepare_session(
            "default",
            Ipv4Addr::LOCALHOST,
            socks.local_addr()?.port(),
            http.local_addr()?.port(),
        )?;
        drop((socks, http));
        engine.connect_session("default", &server)?;
        let core = engine
            .sessions
            .get("default")
            .context("session")?
            .core
            .incarnation()
            .context("actual core")?;
        let service = Service {
            shared: Shared::new(engine, false, false, false),
        };
        let events = service.shared.recorder.events();
        assert!(events.iter().any(|event| matches!(
            event,
            Event::AttemptStarted {
                origin: Origin::Adopted,
                ..
            }
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event,
            Event::CoreStarted { context, adopted: true } if context.core == Some(core)))
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::AttemptFinished { .. }))
        );
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn policy_off_on_flushes_old_watch_summaries_and_discards_old_epoch_observations() -> Result<()>
    {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("attributed-policy-epoch")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        start_attributed_fake_session(&service, "default", &server, None)?;
        let mut config = oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .clone();
        config.watchdog = WatchdogMode::Aggressive;
        service.set_settings(serde_json::to_string(&config)?)?;
        let old = service
            .shared
            .watchdog_attempt(&oxidom_core::sync::lock(&service.shared.engine), "default")
            .context("watch")?;
        for _ in 0..2 {
            assert!(!service.shared.note_tunnel_probe(
                "default",
                old,
                config.watchdog,
                Some(&probe::ProbeOutcome::Timeout)
            ));
        }
        for mode in [WatchdogMode::Off, WatchdogMode::Aggressive] {
            config.watchdog = mode;
            service.set_settings(serde_json::to_string(&config)?)?;
        }
        let new = service
            .shared
            .watchdog_attempt(&oxidom_core::sync::lock(&service.shared.engine), "default")
            .context("new watch")?;
        assert_eq!(old.context, new.context);
        assert_ne!(old.policy_epoch, new.policy_epoch);
        assert!(!service.shared.note_tunnel_probe(
            "default",
            old,
            config.watchdog,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        assert!(!service.shared.note_tunnel_probe(
            "default",
            new,
            config.watchdog,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        assert!(!service.shared.note_tunnel_probe(
            "default",
            new,
            config.watchdog,
            Some(&probe::ProbeOutcome::NoNetwork)
        ));
        let events = service.shared.recorder.events();
        assert!(events.iter().any(|event| matches!(event, Event::WatchSummary { key, suppressed_count: 1,
            reason: oxidom_core::diagnostics::FlushReason::ContextEnded, .. } if key.policy_epoch == old.policy_epoch)));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Watch { policy_epoch,
            action: WatchAction::DiscardedStale, .. } if *policy_epoch == old.policy_epoch))
        );
        assert!(events.iter().any(|event| matches!(event, Event::Watch { policy_epoch,
            outcome: ObservationOutcome::NoNetwork, action: WatchAction::Reset, .. } if *policy_epoch == new.policy_epoch)));
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn watch_actions_distinguish_retention_recovery_and_rejection_without_inventing_a_death()
    -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("attributed-watch-actions")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        start_attributed_fake_session(&service, "default", &server, None)?;
        let mut config = oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .clone();
        config.reconnect = true;
        for mode in [WatchdogMode::Conservative, WatchdogMode::Aggressive] {
            config.watchdog = mode;
            service.set_settings(serde_json::to_string(&config)?)?;
            let attempt = service
                .shared
                .watchdog_attempt(&oxidom_core::sync::lock(&service.shared.engine), "default")
                .context("watch attempt")?;
            let (_, _, threshold) = watchdog_policy(mode).context("policy")?;
            for strike in 1..=threshold {
                assert_eq!(
                    service.shared.note_tunnel_probe(
                        "default",
                        attempt,
                        mode,
                        Some(&probe::ProbeOutcome::Unreachable)
                    ),
                    strike == threshold
                );
            }
            if mode == WatchdogMode::Conservative {
                assert!(service.shared.retain_conservative_tunnel(
                    "default",
                    attempt,
                    Some(&probe::ProbeOutcome::Unreachable)
                ));
                assert!(
                    oxidom_core::sync::lock(&service.shared.engine)
                        .sessions
                        .get_mut("default")
                        .context("retained session")?
                        .is_alive()
                );
            } else {
                assert!(
                    service
                        .shared
                        .recover_dead_tunnel_for(
                            "default",
                            attempt,
                            Some(&probe::ProbeOutcome::Unreachable)
                        )
                        .is_some()
                );
                assert!(
                    service
                        .shared
                        .recover_dead_tunnel_for(
                            "default",
                            attempt,
                            Some(&probe::ProbeOutcome::Unreachable)
                        )
                        .is_none()
                );
            }
        }
        let events = service.shared.recorder.events();
        for expected in [
            WatchAction::ConservativeRetained,
            WatchAction::RecoveryApplied,
            WatchAction::RecoveryRejected,
        ] {
            assert!(events.iter().any(|event| matches!(event, Event::Watch { action, outcome: ObservationOutcome::Unreachable, .. }
                if *action == expected)));
        }
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::LivenessLost { .. }))
        );
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn pool_early_termination_keeps_typed_observations_instead_of_inventing_window_exhaustion()
    -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("pool-termination-attribution")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let target = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            let socks = engine.registry.config.socks_port;
            let http = engine.registry.config.http_port;
            engine.prepare_session("default", Ipv4Addr::LOCALHOST, socks, http)?;
            ConnectionTarget::Pool {
                members: engine
                    .all_servers()
                    .map(|server| server.id.clone())
                    .collect(),
                name: "Invented pool".into(),
                strategy: "leastLoad".into(),
                expected: 0,
                probe_interval: "5m".into(),
                api_port: oxidom_core::engine::free_port(Ipv4Addr::LOCALHOST, &[socks, http])?,
            }
        };
        let start_pool = || -> Result<Confirmation> {
            let generation = {
                let _engine = oxidom_core::sync::lock(&service.shared.engine);
                service.shared.next_connect_generation("default")
            };
            assert!(
                service
                    .shared
                    .start_connection_target(
                        "default",
                        &target,
                        generation,
                        ConnectionOrigin::Explicit
                    )?
                    .context("accepted pool")?
                    .recv_timeout(Duration::from_secs(5))?
            );
            let engine = oxidom_core::sync::lock(&service.shared.engine);
            let context = oxidom_core::sync::lock(&service.shared.identities).context("default");
            Ok(service
                .shared
                .capture_confirmation(&engine, "default", generation, context))
        };
        let original = start_pool()?;
        oxidom_core::sync::lock(&service.shared.engine)
            .sessions
            .get_mut("default")
            .context("pool")?
            .core
            .fail(CORE_EXITED_MESSAGE);
        let now = Instant::now();
        let mut current = original;
        for expected_end in [ConfirmationEnd::CoreStopped, ConfirmationEnd::Superseded] {
            if expected_end == ConfirmationEnd::Superseded {
                current = start_pool()?;
            }
            for (observation, expected, detail) in [
                (
                    Some(probe::ProbeOutcome::Internal(ProbeDetail::ConfigRefused)),
                    ObservationOutcome::Internal,
                    Some(ProbeDetail::ConfigRefused),
                ),
                (
                    Some(probe::ProbeOutcome::NoNetwork),
                    ObservationOutcome::NoNetwork,
                    None,
                ),
                (
                    Some(probe::ProbeOutcome::Unreachable),
                    ObservationOutcome::Unreachable,
                    None,
                ),
                (None, ObservationOutcome::MissingSubject, None),
            ] {
                let (latency, observation, end) = service
                    .shared
                    .pool_probe_completion(
                        "default",
                        original,
                        (None, observation),
                        now + POOL_CONFIRM_WINDOW,
                        || now,
                    )
                    .context("terminated pool")?;
                assert_eq!(latency, None);
                assert_eq!(end, expected_end);
                assert_eq!(trace::observation(observation.as_ref()), (expected, detail));
                service.shared.apply_confirmation_failure(
                    "default",
                    &target,
                    ConnectionOrigin::Explicit,
                    original,
                    end,
                    observation.as_ref(),
                );
                assert!(
                    service
                        .shared
                        .recorder
                        .events()
                        .contains(&Event::AttemptFinished {
                            context: original.context.context("original context")?,
                            outcome: expected,
                            disposition: if end == ConfirmationEnd::Superseded {
                                Disposition::DiscardedStale
                            } else {
                                Disposition::Accepted
                            },
                            failure: None,
                            os_code: None,
                            detail,
                        })
                );
            }
        }
        assert!(
            !service
                .shared
                .recorder
                .events()
                .iter()
                .any(|event| matches!(
                    event,
                    Event::AttemptFinished {
                        outcome: ObservationOutcome::PoolWindowExhausted,
                        ..
                    }
                ))
        );
        assert!(
            service
                .shared
                .pool_probe_completion(
                    "default",
                    current,
                    (None, Some(probe::ProbeOutcome::Unreachable)),
                    now + POOL_CONFIRM_WINDOW,
                    || now
                )
                .is_none()
        );
        let (latency, observation, end) = service
            .shared
            .pool_probe_completion(
                "default",
                current,
                (None, Some(probe::ProbeOutcome::Unreachable)),
                now,
                || now,
            )
            .context("expired window")?;
        assert_eq!(latency, None);
        assert_eq!(end, ConfirmationEnd::PoolWindowExhausted);
        assert!(matches!(
            observation.as_ref(),
            Some(probe::ProbeOutcome::Unreachable)
        ));
        service.shared.apply_confirmation_failure(
            "default",
            &target,
            ConnectionOrigin::Explicit,
            current,
            end,
            observation.as_ref(),
        );
        assert!(
            service
                .shared
                .recorder
                .events()
                .iter()
                .any(|event| matches!(event,
            Event::AttemptFinished { context, outcome: ObservationOutcome::PoolWindowExhausted,
                disposition: Disposition::Accepted, .. } if Some(*context) == current.context))
        );
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn completed_recovery_remains_recovered_when_replacement_follows_override_release() -> Result<()>
    {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("ordered-retry-recovery")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        let original = start_attributed_fake_session(&service, "default", &server, None)?;
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.reconnect = true;
            let session = engine.sessions.get_mut("default").context("session")?;
            session.core.disconnect();
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
        }
        let (profile, target, death) = service
            .shared
            .capture_core_deaths()
            .pop()
            .context("death")?;
        assert!(service.shared.claim_reconnect(&profile, &target, death));
        let recovered =
            start_attributed_fake_session(&service, "default", &server, Some(original.generation))?;
        assert!(
            service
                .shared
                .complete_reconnect(&profile, &target, recovered, death.context, 0)
        );
        assert!(
            oxidom_core::sync::lock(&service.shared.override_status)
                .get(&profile)
                .is_none()
        );
        // Replacement can proceed as soon as override release is observable.
        // Its supersession must find the recovered episode already captured.
        let replacement = start_attributed_fake_session(&service, "default", &server, None)?;
        service
            .shared
            .retry_fact(&profile, death.context, RetryState::Cancelled, 0);
        assert!(
            !service
                .shared
                .complete_reconnect(&profile, &target, recovered, death.context, 0)
        );
        let terminal = service
            .shared
            .recorder
            .events()
            .into_iter()
            .filter(|event| {
                matches!(event,
            Event::Retry { context, state: RetryState::Cancelled | RetryState::Recovered, .. }
                if Some(*context) == death.context)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            terminal,
            vec![Event::Retry {
                context: death.context.context("trigger")?,
                state: RetryState::Recovered,
                ordinal: 0,
                delay_ms: 0,
            }]
        );
        assert_eq!(
            oxidom_core::sync::lock(&service.shared.identities).context(&profile),
            replacement.context
        );
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn removing_a_server_invalidates_its_pending_retry_before_control_returns() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("removed-server-cancels-retry")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        service.connect(server.clone())?;
        confirm_live_session(&service, "default")?;
        let generation = service
            .shared
            .current_generation("default")
            .context("generation")?;
        let target = ConnectionTarget::Server(server.clone());
        oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .reconnect = true;
        oxidom_core::sync::lock(&service.shared.override_status).insert(
            "default".into(),
            ErrorOverride {
                status: Status::Error(RECONNECTING_MESSAGE.into()),
                selection: target.selection(),
            },
        );
        assert!(service.remove_server(server)?);
        assert!(!service.shared.generation_is_current("default", generation));
        assert!(
            !service
                .shared
                .reconnect_is_pending("default", &target, generation)
        );
        assert!(
            oxidom_core::sync::lock(&service.shared.override_status)
                .get("default")
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn removing_a_subscription_invalidates_its_pending_retry_before_control_returns() -> Result<()>
    {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("removed-subscription-cancels-retry")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let (subscription, server) = {
            let engine = oxidom_core::sync::lock(&service.shared.engine);
            let subscription = engine
                .registry
                .subscriptions
                .first()
                .context("subscription")?;
            (
                subscription.id.clone(),
                subscription.servers.first().context("server")?.id.clone(),
            )
        };
        service.connect(server.clone())?;
        confirm_live_session(&service, "default")?;
        let generation = service
            .shared
            .current_generation("default")
            .context("generation")?;
        let target = ConnectionTarget::Server(server);
        oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .reconnect = true;
        oxidom_core::sync::lock(&service.shared.override_status).insert(
            "default".into(),
            ErrorOverride {
                status: Status::Error(RECONNECTING_MESSAGE.into()),
                selection: target.selection(),
            },
        );
        assert!(service.remove_subscription(subscription)?);
        assert!(!service.shared.generation_is_current("default", generation));
        assert!(
            !service
                .shared
                .reconnect_is_pending("default", &target, generation)
        );
        assert!(
            oxidom_core::sync::lock(&service.shared.override_status)
                .get("default")
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn deletion_cancels_pending_targets_even_after_a_failed_attempt_cleared_selection() -> Result<()>
    {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        for remove_subscription in [false, true] {
            let root = TestRoot::install(if remove_subscription {
                "empty-selection-subscription-retry"
            } else {
                "empty-selection-server-retry"
            })?;
            let service = for_test();
            configure_fake_core(&service, &root)?;
            let (subscription, server) = {
                let engine = oxidom_core::sync::lock(&service.shared.engine);
                let subscription = engine
                    .registry
                    .subscriptions
                    .first()
                    .context("subscription")?;
                (
                    subscription.id.clone(),
                    subscription.servers.first().context("server")?.id.clone(),
                )
            };
            service.connect(server.clone())?;
            confirm_live_session(&service, "default")?;
            let generation = service
                .shared
                .current_generation("default")
                .context("generation")?;
            let target = ConnectionTarget::Server(server.clone());
            {
                let mut engine = oxidom_core::sync::lock(&service.shared.engine);
                engine.registry.config.reconnect = true;
                let session = engine.sessions.get_mut("default").context("session")?;
                session.core.disconnect();
                session.selection = None;
                *oxidom_core::sync::lock(&session.core.status) =
                    Status::Error("invented retry failure".into());
            }
            oxidom_core::sync::lock(&service.shared.override_status).insert(
                "default".into(),
                ErrorOverride {
                    status: Status::Error(RECONNECTING_MESSAGE.into()),
                    selection: target.selection(),
                },
            );
            if remove_subscription {
                service.remove_subscription(subscription)?;
            } else {
                service.remove_server(server)?;
            }
            assert!(!service.shared.generation_is_current("default", generation));
            assert!(
                !service
                    .shared
                    .reconnect_is_pending("default", &target, generation)
            );
            assert!(
                oxidom_core::sync::lock(&service.shared.override_status)
                    .get("default")
                    .is_none()
            );
            service.stop_all_profiles()?;
        }
        Ok(())
    }

    #[test]
    fn delayed_confirmation_cannot_finish_recovery_after_its_core_dies() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("delayed-recovery-success")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.reconnect = true;
            engine.all_servers().next().context("server")?.id.clone()
        };
        let first = start_attributed_fake_session(&service, "default", &server, None)?;
        let lose_core = || -> Result<(String, ConnectionTarget, Confirmation)> {
            {
                let mut engine = oxidom_core::sync::lock(&service.shared.engine);
                let session = engine.sessions.get_mut("default").context("session")?;
                session.core.disconnect();
                *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
            }
            service.shared.capture_core_deaths().pop().context("death")
        };
        let (profile, target, trigger) = lose_core()?;
        assert!(service.shared.claim_reconnect(&profile, &target, trigger));
        let confirmed =
            start_attributed_fake_session(&service, "default", &server, Some(first.generation))?;
        let (_, _, second_death) = lose_core()?;
        assert!(
            !service
                .shared
                .claim_reconnect(&profile, &target, second_death)
        );
        assert!(!service.shared.complete_reconnect(
            &profile,
            &target,
            confirmed,
            trigger.context,
            0,
        ));
        assert!(
            service
                .shared
                .reconnect_is_pending(&profile, &target, first.generation)
        );
        assert!(
            !service
                .shared
                .recorder
                .events()
                .iter()
                .any(|event| matches!(event,
            Event::Retry { context, state: RetryState::Recovered, .. }
                if Some(*context) == trigger.context))
        );
        let (answer, current) = service
            .shared
            .start_reconnect_attempt(&profile, &target, first.generation)
            .context("retry still owned")??;
        assert!(answer.recv_timeout(Duration::from_secs(5))?);
        assert!(
            service
                .shared
                .complete_reconnect(&profile, &target, current, trigger.context, 1,)
        );
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn failed_new_generation_keeps_old_core_observations_and_actual_retry_events() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("surviving-core-attribution")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.reconnect = true;
            engine.all_servers().next().context("server")?.id.clone()
        };
        let old = start_attributed_fake_session(&service, "default", &server, None)?;
        let generation = service.shared.next_connect_generation("default");
        assert!(
            service
                .shared
                .start_connection(
                    "default",
                    "invented-removed-target",
                    generation,
                    ConnectionOrigin::Explicit,
                )
                .is_err()
        );
        let watched = {
            let engine = oxidom_core::sync::lock(&service.shared.engine);
            service
                .shared
                .watchdog_attempt(&engine, "default")
                .context("surviving core")?
        };
        assert_eq!(watched.context, old.context);
        assert_ne!(watched.generation, old.generation);
        for _ in 0..2 {
            assert!(!service.shared.note_tunnel_probe(
                "default",
                watched,
                WatchdogMode::Conservative,
                Some(&probe::ProbeOutcome::Timeout),
            ));
        }
        let mut config = oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .clone();
        config.watchdog = WatchdogMode::Off;
        service.set_settings(serde_json::to_string(&config)?)?;
        assert!(service.shared.recorder.events().iter().any(|event| matches!(event,
            Event::WatchSummary { key, suppressed_count: 1, reason: oxidom_core::diagnostics::FlushReason::ContextEnded, .. }
                if Some(key.context) == old.context)));
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            let session = engine.sessions.get_mut("default").context("session")?;
            session.core.disconnect();
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
        }
        let (profile, target, death) = service
            .shared
            .capture_core_deaths()
            .pop()
            .context("death")?;
        assert_eq!(death.context, old.context);
        assert_eq!(death.generation, generation);
        assert!(service.shared.claim_reconnect(&profile, &target, death));
        assert!(
            service
                .shared
                .recorder
                .events()
                .iter()
                .any(|event| matches!(event,
            Event::Retry { context, state: RetryState::Pending, .. }
                if Some(*context) == old.context))
        );
        let recovered =
            start_attributed_fake_session(&service, "default", &server, Some(generation))?;
        assert!(
            service
                .shared
                .complete_reconnect(&profile, &target, recovered, old.context, 0)
        );
        assert!(
            service
                .shared
                .recorder
                .events()
                .iter()
                .any(|event| matches!(event,
            Event::Retry { context, state: RetryState::Recovered, .. }
                if Some(*context) == old.context))
        );
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn liveness_loss_before_a_conservative_action_discards_the_retention_claim() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("conservative-action-after-death")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        start_attributed_fake_session(&service, "default", &server, None)?;
        let watched = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.watchdog = WatchdogMode::Conservative;
            engine.registry.config.reconnect = true;
            service
                .shared
                .watchdog_attempt(&engine, "default")
                .context("watch")?
        };
        let (_, _, threshold) = watchdog_policy(WatchdogMode::Conservative).context("policy")?;
        for strike in 1..=threshold {
            assert_eq!(
                service.shared.note_tunnel_probe(
                    "default",
                    watched,
                    WatchdogMode::Conservative,
                    Some(&probe::ProbeOutcome::Timeout)
                ),
                strike == threshold
            );
        }
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            let session = engine.sessions.get_mut("default").context("session")?;
            session.core.disconnect();
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
        }
        assert_eq!(service.shared.capture_core_deaths().len(), 1);
        assert!(service.shared.watchdog_is_current(
            &oxidom_core::sync::lock(&service.shared.engine),
            "default",
            watched
        ));
        assert!(!service.shared.retain_conservative_tunnel(
            "default",
            watched,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        let events = service.shared.recorder.events();
        assert!(events.iter().any(|event| matches!(event,
            Event::LivenessLost { context, holding_traffic: true } if Some(*context) == watched.context)));
        assert!(events.iter().any(|event| matches!(event,
            Event::Watch { context, action: WatchAction::DiscardedStale, .. } if Some(*context) == watched.context)));
        assert!(!events.iter().any(|event| matches!(
            event,
            Event::Watch {
                action: WatchAction::ConservativeRetained,
                ..
            }
        )));
        {
            let engine = oxidom_core::sync::lock(&service.shared.engine);
            let session = engine.sessions.get("default").context("session")?;
            assert!(
                matches!(session.status(), Status::Error(message) if message == CORE_EXITED_MESSAGE)
            );
            assert!(session.holding_traffic);
        }
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn cancelling_a_retry_retires_its_ended_lifetime_before_the_next_connect() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("cancelled-retry-lifetime")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let server = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("server")?
            .id
            .clone();
        let original = start_attributed_fake_session(&service, "default", &server, None)?;
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.reconnect = true;
            let session = engine.sessions.get_mut("default").context("session")?;
            session.core.disconnect();
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
        }
        let (profile, target, death) = service
            .shared
            .capture_core_deaths()
            .pop()
            .context("death")?;
        assert!(service.shared.claim_reconnect(&profile, &target, death));
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            assert!(!engine.confirmation_failed(&profile, "invented failed retry"));
            let session = engine.sessions.get(&profile).context("inspectable entry")?;
            assert_eq!(session.status(), Status::Disconnected);
            assert!(session.selection.is_none());
            assert!(!session.holding_traffic);
            assert!(session.interface.is_none());
            engine.registry.config.reconnect = false;
        }
        assert!(
            !service
                .shared
                .reconnect_is_pending(&profile, &target, death.generation)
        );
        service
            .shared
            .retry_fact(&profile, death.context, RetryState::Cancelled, 0);
        assert!(
            oxidom_core::sync::lock(&service.shared.identities)
                .context(&profile)
                .is_none()
        );
        let events = service.shared.recorder.events();
        let cancelled = events.iter().position(|event| matches!(event,
            Event::Retry { context, state: RetryState::Cancelled, .. } if Some(*context) == death.context)).context("cancel event")?;
        let retired = events
            .iter()
            .position(|event| {
                matches!(event,
            Event::SessionRetired { context } if Some(*context) == original.context)
            })
            .context("retirement")?;
        assert!(cancelled < retired);
        let replacement = start_attributed_fake_session(&service, "default", &server, None)?;
        assert_ne!(
            original.context.context("old context")?.session,
            replacement.context.context("new context")?.session
        );
        service
            .shared
            .retry_fact(&profile, death.context, RetryState::Cancelled, 0);
        assert_eq!(
            oxidom_core::sync::lock(&service.shared.identities).context(&profile),
            replacement.context
        );
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.reconnect = true;
            let session = engine.sessions.get_mut(&profile).context("replacement")?;
            session.core.disconnect();
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
        }
        let (_, target, held_death) = service
            .shared
            .capture_core_deaths()
            .pop()
            .context("held death")?;
        assert!(
            service
                .shared
                .claim_reconnect(&profile, &target, held_death)
        );
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            assert!(!engine.confirmation_failed(&profile, "invented second failed retry"));
            engine
                .sessions
                .get_mut(&profile)
                .context("held entry")?
                .holding_traffic = true;
        }
        service
            .shared
            .retry_fact(&profile, held_death.context, RetryState::Cancelled, 0);
        assert_eq!(
            oxidom_core::sync::lock(&service.shared.identities).context(&profile),
            replacement.context
        );
        assert!(
            !service
                .shared
                .recorder
                .events()
                .iter()
                .any(|event| matches!(event,
            Event::SessionRetired { context } if Some(*context) == replacement.context))
        );
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn a_pool_that_failed_to_confirm_counts_nodes_instead_of_naming_a_server() {
        // `Session::server_id()` returns `None` for a pool on purpose, so a
        // message about "the active server" would describe something that does
        // not exist.
        let target = pool_target(&["one", "two", "three"]);
        assert_eq!(
            confirmation_failure(&target, true, &[], Some(0), None, LatencyMethod::HttpGet),
            "the pool carried no traffic within 20s — 0 of 3 nodes were in rotation"
        );
        assert_eq!(
            confirmation_failure(&target, true, &[], Some(1), None, LatencyMethod::HttpGet),
            "the pool carried no traffic within 20s — 1 of 3 nodes were in rotation"
        );
        // The balancer could not be asked at all — claiming "0 in rotation"
        // would report a measurement nobody took.
        assert_eq!(
            confirmation_failure(&target, true, &[], None, None, LatencyMethod::HttpGet),
            "the pool carried no traffic within 20s — none of its 3 nodes answered"
        );
    }

    /// A count on its own names the symptom and not the cause — and under
    /// roundRobin every node stays eligible, so it reads "3 of 3 nodes were in
    /// rotation" while nothing works. The balancer puts a node in rotation only
    /// once the observatory has reached its destination through it, so a failed
    /// ping is the actual answer and names the setting that changes it.
    ///
    /// The lines are the core's own, as they arrive in the log book.
    #[test]
    fn a_pool_whose_health_check_never_succeeded_says_so_instead_of_counting_rotation() {
        let target = pool_target(&["one", "two", "three"]);
        let logs = vec![
            "19:02:20  WARN   xray/app/observatory/burst  error ping \
             https://probe.example/generate_204 with s-01: Head \
             \"https://probe.example/generate_204\": context deadline exceeded"
                .to_string(),
            // The same member failing twice is one member, not two.
            "19:02:25  WARN   xray/app/observatory/burst  error ping \
             https://probe.example/generate_204 with s-01: Head \
             \"https://probe.example/generate_204\": context deadline exceeded"
                .to_string(),
            "19:02:26  WARN   xray/app/observatory/burst  error ping \
             https://probe.example/generate_204 with s-02: Head \
             \"https://probe.example/generate_204\": context deadline exceeded"
                .to_string(),
        ];

        assert_eq!(
            confirmation_failure(&target, true, &logs, Some(3), None, LatencyMethod::HttpGet),
            "the pool carried no traffic within 20s — its health check could not be reached \
             through 2 of 3 nodes — the address is [core] pool_probe_url",
            "the rotation count outranked the core's own failed pings"
        );
    }

    /// A pool that failed for some other reason keeps the wording it had, and
    /// nothing oxidom writes itself may be read as the core complaining.
    #[test]
    fn a_log_without_an_observatory_complaint_still_reports_the_rotation() {
        let target = pool_target(&["one", "two"]);
        let logs = vec![
            "19:02:20  INFO   oxidom  the pool carried no traffic within 20s".to_string(),
            "19:02:21  INFO   xray  from tcp:127.0.0.1:42204 accepted tcp:example:443".to_string(),
        ];
        assert_eq!(
            confirmation_failure(&target, true, &logs, Some(2), None, LatencyMethod::HttpGet),
            "the pool carried no traffic within 20s — 2 of 2 nodes were in rotation"
        );
    }

    /// Two downloads of the same two files would race on the same temp names
    /// and pull the bytes twice over a link the user may be paying for. The
    /// second request is refused rather than queued: the first is already doing
    /// what the caller asked for.
    #[test]
    fn a_second_geo_download_is_refused_while_one_is_running() {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("geo-busy");
        let shared = Shared::new(Engine::load(), false, false, false);
        oxidom_core::sync::lock(&shared.geo).downloading = true;
        let error = shared
            .start_geo_download(false)
            .expect_err("a second request must be refused");
        assert!(error.contains("already running"), "{error}");
        drop(root);
    }

    /// Cancelling is consulted once per chunk by the download loop, which holds
    /// the state lock to publish progress. The flag therefore lives outside
    /// that lock — otherwise Cancel would block on the very loop it is meant to
    /// stop, and a 23 MB download would be uninterruptible.
    #[test]
    fn a_download_can_be_cancelled_while_it_holds_the_progress_lock() {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("geo-cancel");
        let shared = Shared::new(Engine::load(), false, false, false);
        let held = oxidom_core::sync::lock(&shared.geo);
        shared.geo_cancel.cancel();
        assert!(
            shared.geo_cancel.is_cancelled(),
            "a cancel must not wait for the progress lock"
        );
        drop(held);
        drop(root);
    }

    #[test]
    fn a_dead_inbound_and_a_rejected_protocol_outrank_the_selection() {
        let target = pool_target(&["one"]);
        // Nothing bound, so nothing was measured: the pool count would be a
        // conclusion drawn from an experiment that never ran.
        assert_eq!(
            confirmation_failure(&target, false, &[], None, None, LatencyMethod::HttpGet),
            "the local SOCKS inbound never came up — the core is not carrying traffic"
        );
        let logs = vec![format!(
            "failed to parse outbound: {}",
            oxidom_core::xray::core::UNSUPPORTED_PROTOCOL_MARKERS[0]
        )];
        assert!(
            confirmation_failure(&target, false, &logs, None, None, LatencyMethod::HttpGet)
                .contains("does not support this server's protocol")
        );
        // A single server with no outcome and no log falls back to the bare sentence.
        assert_eq!(
            confirmation_failure(
                &ConnectionTarget::Server("one".to_string()),
                true,
                &[],
                None,
                None,
                LatencyMethod::HttpGet,
            ),
            "active server did not pass its latency check"
        );
    }

    #[test]
    fn an_unclassified_outbound_failure_never_repeats_the_core_log() {
        let logs = vec![
            "failed to dial > trojan://invented-password@private.example.invalid:443?token=invented-token".to_string(),
        ];
        let message = single_server_confirmation_failure(
            &logs,
            Some(&probe::ProbeOutcome::Unreachable),
            LatencyMethod::HttpGet,
        );
        assert_eq!(
            core_outbound_failure(&logs).as_deref(),
            Some("the core could not establish an outbound connection")
        );
        assert!(!message.contains("invented-password"));
        assert!(!message.contains("private.example.invalid"));
        assert!(!message.contains("invented-token"));
    }

    #[test]
    fn a_single_server_confirmation_failure_explains_what_went_wrong() {
        let target = ConnectionTarget::Server("one".to_string());
        assert_eq!(
            confirmation_failure(
                &target,
                true,
                &[],
                None,
                Some(&probe::ProbeOutcome::Timeout),
                LatencyMethod::HttpGet,
            ),
            "active server did not pass its latency check — the request through the tunnel timed out after 6s"
        );
        assert_eq!(
            confirmation_failure(
                &target,
                true,
                &[],
                None,
                Some(&probe::ProbeOutcome::Unreachable),
                LatencyMethod::Icmp,
            ),
            "active server did not pass its latency check — the tunnel closed the connection without carrying the request (ICMP checks only whether the host answers ping, not whether the proxy carries traffic)"
        );
        let reset_logs = vec![
            "app/proxyman/outbound: failed to process outbound traffic > proxy/vless/outbound: \
             failed to find an available destination > common/retry: [read tcp 10.0.0.2:41234->\
             198.51.100.7:443: read: connection reset by peer] > common/retry: all retry attempts failed"
                .to_string(),
        ];
        assert_eq!(
            confirmation_failure(
                &target,
                true,
                &reset_logs,
                None,
                Some(&probe::ProbeOutcome::Unreachable),
                LatencyMethod::HttpGet,
            ),
            "active server did not pass its latency check — the server reset the connection during the handshake"
        );
    }

    /// A connection that failed because the core has no geo data must say so.
    /// The account below is the only one the user gets — the session is torn
    /// down and the core's status goes with it — so "did not pass its latency
    /// check" would send someone to replace a server that was never the
    /// problem. This works without a line of its own here because the check
    /// funnels through `classify_complaint`, which is the point of having one
    /// classifier; this test is what stops that wiring being removed.
    #[test]
    fn a_connection_that_failed_for_want_of_geo_data_says_so() {
        let logs = vec![
            "Failed to start: main: failed to load config files: [current-config.json] > \
             infra/conf: failed to build routing configuration > infra/conf: invalid field \
             rule > infra/conf: failed to load GeoIP: private > infra/conf: failed to open \
             file: geoip.dat > open /var/lib/oxidom/assets/geoip.dat: no such file or directory"
                .to_string(),
        ];
        assert_eq!(
            confirmation_failure(
                &ConnectionTarget::Server("one".to_string()),
                false,
                &logs,
                None,
                None,
                LatencyMethod::HttpGet,
            ),
            oxidom_core::ipc::ProbeDetail::GeoAssetsMissing.message(),
            "the core's own reason outranks the dead inbound it caused"
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
            None,
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

    /// The inverse of the cancellation above: an explicit connect owns the
    /// visible state only once it actually starts something. One that fails
    /// during preparation must not burn the generation a pending automatic
    /// reconnect is waiting on, or clear its override — either loss strands
    /// the session in Error, holding traffic, with no retry left alive.
    #[test]
    fn a_failed_connect_leaves_a_pending_reconnect_intact() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("failed-connect-reconnect")?;
        let service = for_test();
        let generation = service.shared.next_connect_generation("default");
        let selection = SessionSelection::Server("dead-server".to_string());
        oxidom_core::sync::lock(&service.shared.override_status).insert(
            "default".to_string(),
            ErrorOverride {
                status: Status::Error(RECONNECTING_MESSAGE.to_string()),
                selection: selection.clone(),
            },
        );

        assert!(service.connect("no-such-server".to_string()).is_err());

        assert!(
            service.shared.generation_is_current("default", generation),
            "a connect that started nothing must not invalidate the reconnect's generation"
        );
        assert!(
            matches!(
                oxidom_core::sync::lock(&service.shared.override_status).get("default"),
                Some(ErrorOverride {
                    status: Status::Error(message),
                    selection: held,
                }) if message == RECONNECTING_MESSAGE && held == &selection
            ),
            "the reconnecting override survives a connect that started nothing"
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
            engine.registry.config.watchdog = WatchdogMode::Off;
            engine.registry.config.nft_binary = "/configured/nft".to_string();
        }
        let mut raw = serde_json::to_value(Config::default())?;
        let object = raw
            .as_object_mut()
            .context("serialized config is not an object")?;
        object.remove("reconnect");
        object.remove("watchdog");
        object.remove("tun2socks_binary");
        object.remove("nft_binary");

        service.set_settings(raw.to_string())?;

        let engine = oxidom_core::sync::lock(&service.shared.engine);
        assert!(engine.registry.config.reconnect);
        assert_eq!(engine.registry.config.watchdog, WatchdogMode::Off);
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

    fn subscription_response() -> oxidom_core::subscription::FetchResult {
        let (servers, skipped) = oxidom_core::subscription_format::parse(
            "trojan://invented@node.example.invalid:443#Offline",
        )
        .unwrap();
        oxidom_core::subscription::FetchResult {
            servers,
            skipped,
            userinfo: None,
            title: Some("Offline".into()),
            update_interval: None,
            not_taken: Default::default(),
        }
    }

    fn subscription_runtime() -> Result<tokio::runtime::Runtime> {
        Ok(tokio::runtime::Builder::new_current_thread().build()?)
    }

    #[test]
    fn an_admitted_import_waiting_for_the_engine_cannot_write_after_closure() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("terminal-admitted-import")?;
        let service = for_test();
        let shared = service.shared.clone();
        let engine = oxidom_core::sync::lock(&shared.engine);
        let worker = std::thread::spawn(move || {
            service.import_links("trojan://invented@node.example.invalid:443#Offline".into())
        });
        shared.lifecycle.wait_for_work();
        shared.lifecycle.close();
        drop(engine);
        assert!(worker.join().unwrap().is_err());
        shared.terminate()?;
        assert!(
            oxidom_core::sync::lock(&shared.engine)
                .registry
                .subscriptions
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn terminal_closure_rejects_delayed_explicit_retry_and_confirmation_transitions() -> Result<()>
    {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("terminal-attempts")?;
        let service = for_test();
        let generation = service.shared.next_connect_generation("work");
        let snapshot = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            mark_active(&mut engine, "work", "invented")?;
            service
                .shared
                .capture_confirmation(&engine, "work", generation, None)
        };
        let target = ConnectionTarget::Server("invented".into());
        service.shared.begin_terminal();
        assert!(
            service
                .shared
                .start_connection_target_captured(
                    "work",
                    &target,
                    generation,
                    ConnectionOrigin::Explicit
                )
                .is_err()
        );
        assert!(
            service
                .shared
                .start_reconnect_attempt("work", &target, generation)
                .is_none()
        );
        assert!(!service.shared.claim_reconnect("work", &target, snapshot));
        assert!(!service.shared.apply_confirmation_success(
            "work",
            &target,
            ConnectionOrigin::Explicit,
            snapshot
        ));
        service.shared.apply_confirmation_failure(
            "work",
            &target,
            ConnectionOrigin::Explicit,
            snapshot,
            ConfirmationEnd::InboundNotReady,
            None,
        );
        assert!(oxidom_core::sync::lock(&service.shared.override_status).is_empty());
        service.shared.terminate()?;
        service.shared.terminate()?;
        Ok(())
    }

    #[test]
    fn terminal_cleanup_failure_is_retained_across_repeated_calls() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("terminal-cleanup-error")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            mark_active(&mut engine, "work", "invented")?;
            engine
                .state
                .sessions
                .push(oxidom_core::state::SessionState {
                    profile: "work".into(),
                    xray_pid: Some(std::process::id()),
                    ..Default::default()
                });
        }
        let other = service.shared.clone();
        let concurrent = std::thread::spawn(move || other.terminate().unwrap_err().to_string());
        let first = service.shared.terminate().unwrap_err().to_string();
        let second = concurrent.join().unwrap();
        assert_eq!(first, second);
        assert_eq!(first, service.shared.terminate().unwrap_err().to_string());
        assert!(
            oxidom_core::sync::lock(&service.shared.engine)
                .state
                .sessions
                .iter()
                .any(|saved| saved.xray_pid == Some(std::process::id()))
        );
        Ok(())
    }

    #[test]
    fn a_retained_service_clone_keeps_the_store_leased_after_terminal_cleanup() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("terminal-store-lease")?;
        let data = oxidom_core::paths::data_dir()?;
        let mut service = for_test();
        service.shared.store_lease = Some(Arc::new(crate::store_lock::StoreLock::acquire(&data)?));
        let retained = service.shared.clone();
        service.shared.terminate()?;
        drop(service);
        assert!(crate::store_lock::StoreLock::acquire(&data).is_err());
        drop(retained);
        let _next = crate::store_lock::StoreLock::acquire(&data)?;
        Ok(())
    }

    #[test]
    fn terminal_closure_rejects_a_subscription_response_already_in_flight() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("terminal-subscription")?;
        let service = for_test();
        subscription_runtime()?.block_on(async {
            let (entered, waiting) = tokio::sync::oneshot::channel();
            let (release, blocked) = mpsc::channel();
            let worker = service.shared.clone();
            let mut entered = Some(entered);
            let operation = tokio::spawn(async move {
                worker
                    .update_subscriptions(
                        SubscriptionOperation::Add {
                            url: "https://panel.example.invalid/sub".into(),
                            name: Some("invented".into()),
                            send_hwid: false,
                        },
                        move |_, _| {
                            let _ = entered.take().context("one fetch")?.send(());
                            blocked.recv()?;
                            Ok(subscription_response())
                        },
                    )
                    .await
            });
            waiting.await?;
            service.shared.lifecycle.close();
            assert!(service.status().is_err());
            assert!(service.get_settings().is_err());
            assert!(service.runtime_info().is_err());
            release.send(())?;
            assert!(operation.await?.is_err());
            service.shared.terminate()?;
            assert!(
                oxidom_core::sync::lock(&service.shared.engine)
                    .registry
                    .subscriptions
                    .is_empty()
            );
            assert!(service.shared.subscription_work.try_lock().is_ok());
            Ok::<_, anyhow::Error>(())
        })
    }

    #[test]
    fn terminal_shutdown_drains_periodic_workers_and_cannot_revive_standby() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("terminal-periodic-workers")?;
        let service = for_test();
        let generation = service.shared.next_connect_generation("work");
        service.shared.spawn_active_probe_loop();
        service.shared.spawn_tunnel_watchdog();
        service.shared.spawn_pool_status_loop();
        spawn_core_supervisor(service.shared.clone());
        service.shared.terminate()?;
        assert!(!service.shared.generation_is_current("work", generation));
        assert!(!service.shared.spawn(|| panic!("terminal work ran")));
        oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .standby_proxy = true;
        service.shared.reconcile_standby();
        assert!(oxidom_core::sync::lock(&service.shared.standby).is_none());
        assert!(service.request_probe("invented".into()).is_err());
        Ok(())
    }

    #[test]
    fn a_pending_subscription_fetch_leaves_the_engine_and_async_executor_available() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("responsive-subscription-fetch")?;
        let service = for_test();
        let id = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            let mut sub = oxidom_core::model::Subscription::new(
                "https://panel.example.invalid/sub".into(),
                Some("Initial".into()),
            );
            sub.servers = subscription_response().servers;
            let id = sub.id.clone();
            engine.registry.subscriptions.push(sub);
            mark_active(&mut engine, "work", "invented-server")?;
            id
        };
        subscription_runtime()?.block_on(async {
            let (entered, waiting) = tokio::sync::oneshot::channel();
            let (release, blocked) = mpsc::channel();
            let worker = service.shared.clone();
            let probe = worker.clone();
            let mut entered = Some(entered);
            let operation = tokio::spawn(async move {
                worker
                    .update_subscriptions(SubscriptionOperation::Refresh(id), move |_, _| {
                        assert!(
                            probe.engine.try_lock().is_ok(),
                            "network work must not own the engine"
                        );
                        let _ = entered.take().context("one fetch")?.send(());
                        blocked.recv_timeout(Duration::from_secs(10))?;
                        Ok(subscription_response())
                    })
                    .await
            });
            waiting.await?;
            assert!(
                !operation.is_finished(),
                "reply must wait for the fetch and commit"
            );
            assert!(service.shared.subscription_work.try_lock().is_err());
            let _: StatusInfo = serde_json::from_str(&service.status()?)?;
            let _: Vec<oxidom_core::model::Subscription> =
                serde_json::from_str(&service.list_subscriptions()?)?;
            service.stop_profile("work")?;
            {
                let mut engine = service
                    .shared
                    .engine
                    .try_lock()
                    .expect("control remains available");
                engine.registry.subscriptions[0].description = Some("edited during HTTP".into());
            }
            release.send(())?;
            operation.await??;
            assert_eq!(
                oxidom_core::sync::lock(&service.shared.engine)
                    .registry
                    .subscriptions[0]
                    .description
                    .as_deref(),
                Some("edited during HTTP")
            );
            assert!(service.shared.subscription_work.try_lock().is_ok());
            Ok::<_, anyhow::Error>(())
        })
    }

    #[test]
    fn subscription_workers_release_their_queue_after_failure_or_panic() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("subscription-worker-failure")?;
        let service = for_test();
        subscription_runtime()?.block_on(async {
            let add = || SubscriptionOperation::Add {
                url: "https://panel.example.invalid/sub".into(),
                name: Some("Offline".into()),
                send_hwid: false,
            };
            assert!(
                service
                    .shared
                    .update_subscriptions(add(), |_, _| Err(anyhow::anyhow!(
                        "offline fetch failure"
                    )))
                    .await
                    .is_err()
            );
            assert!(service.shared.subscription_work.try_lock().is_ok());
            let error = service
                .shared
                .update_subscriptions(add(), |_, _| panic!("invented worker panic"))
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "subscription worker failed");
            assert!(service.shared.subscription_work.try_lock().is_ok());
            service
                .shared
                .update_subscriptions(add(), |_, _| Ok(subscription_response()))
                .await?;
            assert_eq!(
                oxidom_core::sync::lock(&service.shared.engine)
                    .registry
                    .subscriptions
                    .len(),
                1
            );
            Ok::<_, anyhow::Error>(())
        })
    }

    #[test]
    fn queued_requests_do_not_send_hwid_after_opt_out_or_removal() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("subscription-revoked-hwid")?;
        let service = for_test();
        for remove in [false, true] {
            let id = {
                let mut engine = oxidom_core::sync::lock(&service.shared.engine);
                let first = oxidom_core::model::Subscription::new(
                    "https://first.example.invalid/sub".into(),
                    Some("First".into()),
                );
                let mut second = oxidom_core::model::Subscription::new(
                    "https://second.example.invalid/sub".into(),
                    Some("Second".into()),
                );
                second.send_hwid = true;
                let id = second.id.clone();
                engine.registry.subscriptions = vec![first, second];
                id
            };
            subscription_runtime()?.block_on(async {
                let (entered, waiting) = tokio::sync::oneshot::channel();
                let (release, blocked) = mpsc::channel();
                let shared = service.shared.clone();
                let mut entered = Some(entered);
                let operation = tokio::spawn(async move {
                    shared
                        .update_subscriptions(
                            SubscriptionOperation::RefreshAll,
                            move |request, hwid| {
                                assert_eq!(
                                    request.url, "https://first.example.invalid/sub",
                                    "the revoked request must never start"
                                );
                                assert!(hwid.is_none());
                                let _ = entered.take().context("first request only")?.send(());
                                blocked.recv_timeout(Duration::from_secs(10))?;
                                Ok(subscription_response())
                            },
                        )
                        .await
                });
                waiting.await?;
                if remove {
                    service.remove_subscription(id)?;
                } else {
                    service.set_hwid(id, false)?;
                }
                release.send(())?;
                assert!(
                    operation.await?.is_err(),
                    "the invalidated target is reported"
                );
                assert!(!oxidom_core::paths::hwid_file()?.exists());
                Ok::<_, anyhow::Error>(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn refresh_teardown_invalidates_pending_confirmation_before_unlocking_the_engine() -> Result<()>
    {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("subscription-cancels-attempt")?;
        let service = for_test();
        let id = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            let mut sub = oxidom_core::model::Subscription::new(
                "https://panel.example.invalid/sub".into(),
                Some("Initial".into()),
            );
            sub.servers = subscription_response().servers;
            let server = sub.servers[0].id.clone();
            let id = sub.id.clone();
            engine.registry.subscriptions.push(sub);
            mark_active(&mut engine, "work", &server)?;
            let session = engine.sessions.get_mut("work").context("session")?;
            *oxidom_core::sync::lock(&session.core.status) = Status::Connecting;
            id
        };
        let generation = service.shared.next_connect_generation("work");
        subscription_runtime()?.block_on(service.shared.update_subscriptions(
            SubscriptionOperation::Refresh(id),
            |_, _| {
                let mut response = subscription_response();
                response.servers.clear();
                Ok(response)
            },
        ))?;
        assert!(!service.shared.generation_is_current("work", generation));
        assert!(
            oxidom_core::sync::lock(&service.shared.override_status)
                .get("work")
                .is_none()
        );
        assert!(
            oxidom_core::sync::lock(&service.shared.engine)
                .sessions
                .get("work")
                .context("session")?
                .selection
                .is_none()
        );
        let error = service
            .shared
            .start_connection(
                "work",
                "unaffected-server",
                generation,
                ConnectionOrigin::Explicit,
            )
            .unwrap_err();
        assert!(error.to_string().contains("superseded"));
        Ok(())
    }

    fn watchdog_test_attempt(shared: &Shared, mode: WatchdogMode) -> Result<WatchdogAttempt> {
        if shared.current_generation("default").is_none() {
            shared.next_connect_generation("default");
        }
        let mut engine = oxidom_core::sync::lock(&shared.engine);
        engine.registry.config.watchdog = mode;
        if engine.sessions.get("default").is_none() {
            mark_active(&mut engine, "default", "test-server")?;
        }
        shared
            .watchdog_attempt(&engine, "default")
            .context("watchdog attempt")
    }

    #[test]
    fn a_dead_tunnel_is_declared_only_after_an_unbroken_run_of_failures() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("tunnel-strikes")?;
        let service = for_test();
        let shared = &service.shared;

        let generation = watchdog_test_attempt(shared, WatchdogMode::Aggressive)?;
        let note = |outcome: Option<&probe::ProbeOutcome>| {
            shared.note_tunnel_probe("default", generation, WatchdogMode::Aggressive, outcome)
        };
        // One short of the threshold never fires.
        for _ in 0..TUNNEL_DEATH_STRIKES - 1 {
            assert!(!note(Some(&probe::ProbeOutcome::Timeout)));
        }
        // A single reachable answer — however slow — wipes the streak, so the
        // count has to start over from scratch.
        assert!(!note(Some(&reachable())));
        for _ in 0..TUNNEL_DEATH_STRIKES - 1 {
            assert!(!note(Some(&probe::ProbeOutcome::Unreachable)));
        }
        // The strike that reaches the threshold is the one that declares it dead.
        assert!(note(Some(&probe::ProbeOutcome::Unreachable)));
        // And the counter is cleared on the way out, so the next run starts fresh.
        assert!(!note(Some(&probe::ProbeOutcome::Timeout)));
        Ok(())
    }

    #[test]
    fn neither_a_dead_network_nor_our_own_failure_counts_as_a_dead_server() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("tunnel-not-the-server")?;
        let service = for_test();
        let shared = &service.shared;
        let generation = watchdog_test_attempt(shared, WatchdogMode::Aggressive)?;
        let note = |outcome: Option<&probe::ProbeOutcome>| {
            shared.note_tunnel_probe("default", generation, WatchdogMode::Aggressive, outcome)
        };

        // A machine with no uplink, our own inability to measure, and a vanished
        // session are none of them the server going quiet: a reconnect cannot
        // mend them, so they must never advance toward a teardown, even repeated
        // well past the threshold.
        for _ in 0..TUNNEL_DEATH_STRIKES + 2 {
            assert!(!note(Some(&probe::ProbeOutcome::NoNetwork)));
            assert!(!note(Some(&probe::ProbeOutcome::Internal(
                oxidom_core::ipc::ProbeDetail::NoCore
            ))));
            assert!(!note(None));
        }
        for interruption in [
            None,
            Some(probe::ProbeOutcome::NoNetwork),
            Some(probe::ProbeOutcome::Internal(ProbeDetail::Other)),
        ] {
            for _ in 0..TUNNEL_DEATH_STRIKES - 1 {
                assert!(!note(Some(&probe::ProbeOutcome::Timeout)));
            }
            assert!(!note(interruption.as_ref()));
            for _ in 0..TUNNEL_DEATH_STRIKES - 1 {
                assert!(!note(Some(&probe::ProbeOutcome::Timeout)));
            }
            assert!(note(Some(&probe::ProbeOutcome::Timeout)));
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
    fn only_aggressive_checks_may_restart_a_live_core() -> Result<()> {
        assert_eq!(watchdog_policy(WatchdogMode::Off), None);
        assert_eq!(
            watchdog_policy(WatchdogMode::Conservative),
            Some((Duration::from_secs(60), Duration::from_secs(8), 5))
        );
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("conservative-watchdog")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.reconnect = true;
            mark_active(&mut engine, "default", "one")?;
            *oxidom_core::sync::lock(
                &engine
                    .sessions
                    .get("default")
                    .context("session")?
                    .core
                    .status,
            ) = Status::Connected;
        }
        let generation = watchdog_test_attempt(&service.shared, WatchdogMode::Conservative)?;
        for _ in 0..4 {
            assert!(!service.shared.note_tunnel_probe(
                "default",
                generation,
                WatchdogMode::Conservative,
                Some(&probe::ProbeOutcome::Timeout)
            ));
        }
        assert!(service.shared.note_tunnel_probe(
            "default",
            generation,
            WatchdogMode::Conservative,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        for mode in [WatchdogMode::Conservative, WatchdogMode::Off] {
            oxidom_core::sync::lock(&service.shared.engine)
                .registry
                .config
                .watchdog = mode;
            assert!(
                service
                    .shared
                    .recover_dead_tunnel("default", generation)
                    .is_none()
            );
            assert_eq!(
                oxidom_core::sync::lock(&service.shared.engine)
                    .sessions
                    .get("default")
                    .context("session")?
                    .status(),
                Status::Connected
            );
        }
        Ok(())
    }

    #[test]
    fn a_watchdog_result_cannot_stop_or_add_strikes_to_a_replacement() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("stale-watchdog")?;
        let service = for_test();
        let shared = &service.shared;
        {
            let mut engine = oxidom_core::sync::lock(&shared.engine);
            engine.registry.config.reconnect = true;
            engine.registry.config.watchdog = WatchdogMode::Aggressive;
            mark_active(&mut engine, "default", "one")?;
            *oxidom_core::sync::lock(
                &engine
                    .sessions
                    .get("default")
                    .context("session")?
                    .core
                    .status,
            ) = Status::Connected;
        }
        let stale = watchdog_test_attempt(shared, WatchdogMode::Aggressive)?;
        for _ in 0..TUNNEL_DEATH_STRIKES - 1 {
            assert!(!shared.note_tunnel_probe(
                "default",
                stale,
                WatchdogMode::Aggressive,
                Some(&probe::ProbeOutcome::Timeout)
            ));
        }
        shared.next_connect_generation("default");
        let current = watchdog_test_attempt(shared, WatchdogMode::Aggressive)?;
        assert!(!shared.note_tunnel_probe(
            "default",
            stale,
            WatchdogMode::Aggressive,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        assert!(shared.recover_dead_tunnel("default", stale).is_none());
        assert_eq!(
            oxidom_core::sync::lock(&shared.engine)
                .sessions
                .get("default")
                .context("replacement")?
                .status(),
            Status::Connected
        );
        for _ in 0..TUNNEL_DEATH_STRIKES - 1 {
            assert!(!shared.note_tunnel_probe(
                "default",
                current,
                WatchdogMode::Aggressive,
                Some(&probe::ProbeOutcome::Timeout)
            ));
        }
        assert!(shared.note_tunnel_probe(
            "default",
            current,
            WatchdogMode::Aggressive,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        shared.invalidate_generation("default");
        assert!(shared.recover_dead_tunnel("default", current).is_none());
        Ok(())
    }

    #[test]
    fn changing_watchdog_policy_does_not_inherit_another_policys_strikes() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("watchdog-policy-streak")?;
        let service = for_test();
        let shared = &service.shared;
        let generation = watchdog_test_attempt(shared, WatchdogMode::Conservative)?;
        for _ in 0..4 {
            assert!(!shared.note_tunnel_probe(
                "default",
                generation,
                WatchdogMode::Conservative,
                Some(&probe::ProbeOutcome::Timeout)
            ));
        }
        let current = watchdog_test_attempt(shared, WatchdogMode::Aggressive)?;
        for _ in 0..TUNNEL_DEATH_STRIKES - 1 {
            assert!(!shared.note_tunnel_probe(
                "default",
                current,
                WatchdogMode::Aggressive,
                Some(&probe::ProbeOutcome::Timeout)
            ));
        }
        assert!(shared.note_tunnel_probe(
            "default",
            current,
            WatchdogMode::Aggressive,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        Ok(())
    }

    #[test]
    fn disabling_and_reenabling_watchdog_invalidates_in_flight_checks() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("watchdog-settings-epoch")?;
        let service = for_test();
        let shared = &service.shared;
        let stale = watchdog_test_attempt(shared, WatchdogMode::Aggressive)?;
        for _ in 0..TUNNEL_DEATH_STRIKES - 1 {
            assert!(!shared.note_tunnel_probe(
                "default",
                stale,
                WatchdogMode::Aggressive,
                Some(&probe::ProbeOutcome::Timeout)
            ));
        }
        for mode in [WatchdogMode::Off, WatchdogMode::Aggressive] {
            service.set_settings(serde_json::to_string(&Config {
                watchdog: mode,
                ..Config::default()
            })?)?;
        }
        assert!(!shared.note_tunnel_probe(
            "default",
            stale,
            WatchdogMode::Aggressive,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        assert!(shared.recover_dead_tunnel("default", stale).is_none());
        let current = watchdog_test_attempt(shared, WatchdogMode::Aggressive)?;
        assert_ne!(current.policy_epoch, stale.policy_epoch);
        for _ in 0..TUNNEL_DEATH_STRIKES - 1 {
            assert!(!shared.note_tunnel_probe(
                "default",
                current,
                WatchdogMode::Aggressive,
                Some(&probe::ProbeOutcome::Timeout)
            ));
        }
        assert!(shared.note_tunnel_probe(
            "default",
            current,
            WatchdogMode::Aggressive,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        Ok(())
    }

    #[test]
    fn automatic_retries_cannot_inherit_checks_of_the_previous_core() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("watchdog-core-incarnation")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let id = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            let id = engine.all_servers().next().context("server")?.id.clone();
            let socks = engine.registry.config.socks_port;
            let http = engine.registry.config.http_port;
            engine.prepare_session("default", Ipv4Addr::LOCALHOST, socks, http)?;
            engine.connect_session("default", &id)?;
            engine.confirm_traffic("default");
            id
        };
        let stale = watchdog_test_attempt(&service.shared, WatchdogMode::Aggressive)?;
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.connect_session("default", &id)?;
            engine.confirm_traffic("default");
        }
        let current = watchdog_test_attempt(&service.shared, WatchdogMode::Aggressive)?;
        assert_eq!(current.generation, stale.generation);
        assert_ne!(current.spawn_seq, stale.spawn_seq);
        assert!(!service.shared.note_tunnel_probe(
            "default",
            stale,
            WatchdogMode::Aggressive,
            Some(&probe::ProbeOutcome::Timeout)
        ));
        assert!(
            service
                .shared
                .recover_dead_tunnel("default", stale)
                .is_none()
        );
        assert_eq!(
            oxidom_core::sync::lock(&service.shared.engine)
                .sessions
                .get("default")
                .context("session")?
                .reported_status(),
            Status::Connected
        );
        Ok(())
    }

    #[test]
    fn reviving_a_live_but_dead_tunnel_fails_the_core_and_keeps_the_selection() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("revive-live-tunnel")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            // Recovery returns its target; only the watchdog starts the retry.
            engine.registry.config.reconnect = true;
            engine.registry.config.watchdog = WatchdogMode::Aggressive;
            mark_active(&mut engine, "default", "silent-server")?;
            let session = engine.default_session_mut().context("default session")?;
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
        }

        let generation = watchdog_test_attempt(&service.shared, WatchdogMode::Aggressive)?;
        assert!(
            service
                .shared
                .recover_dead_tunnel("default", generation)
                .is_some()
        );

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

    /// Renamed and rewritten rather than deleted: it pinned the behaviour this
    /// change replaces. A dead core used to release the desktop proxy — and the
    /// TUN routes with it — before anything was retried, which is the leak. The
    /// sweep still notices every dead session; what it no longer does is let
    /// their traffic out.
    #[test]
    fn every_dead_session_is_noticed_and_holds_its_traffic() -> Result<()> {
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
        // The desktop keeps pointing at a port nothing is listening on, which
        // is the point: an application that honours it fails rather than
        // quietly reaching the internet with the machine's own address.
        assert_eq!(engine.sessions.owner_of_system_proxy(), Some("home"));
        assert!(matches!(
            engine.sessions.get("home").map(|session| session.status()),
            Some(Status::Error(message)) if message == CORE_EXITED_MESSAGE
        ));
        assert!(
            engine
                .sessions
                .get("home")
                .is_some_and(|session| session.holding_traffic),
            "a held session says so, or the interface cannot"
        );
        drop(engine);
        assert!(service.shared.note_core_deaths().is_empty());
        Ok(())
    }

    /// The setting is what the old behaviour is now called, and it still works:
    /// a profile that asked to be released is released, on the same sweep that
    /// holds the one beside it.
    #[test]
    fn a_session_set_to_release_gives_up_the_system_proxy_as_before() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("released-core")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            mark_active(&mut engine, "home", "same")?;
            *oxidom_core::sync::lock(
                &engine
                    .sessions
                    .get("home")
                    .context("test session")?
                    .core
                    .status,
            ) = Status::Connected;
            engine.sessions.claim_system_proxy("home")?;
            engine.configure_hold_traffic("home", Some(oxidom_core::config::OnCoreExit::Release));
        }

        assert_eq!(service.shared.note_core_deaths().len(), 1);
        let engine = oxidom_core::sync::lock(&service.shared.engine);
        assert!(engine.sessions.owner_of_system_proxy().is_none());
        assert!(
            engine
                .sessions
                .get("home")
                .is_some_and(|session| !session.holding_traffic),
            "nothing is being held, so nothing should claim to be"
        );
        Ok(())
    }

    /// The machine's answer is what a profile that names none gets, and it is
    /// fixed when the session comes up rather than read when the core dies: a
    /// profile edited mid-session must not change what happens to the tunnel
    /// already running.
    #[test]
    fn a_profile_that_names_no_answer_takes_the_machines_one_at_up() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("inherited-hold")?;
        let service = for_test();
        let mut engine = oxidom_core::sync::lock(&service.shared.engine);
        mark_active(&mut engine, "home", "same")?;

        engine.registry.config.on_core_exit = oxidom_core::config::OnCoreExit::Release;
        engine.configure_hold_traffic("home", None);
        assert!(
            !engine
                .sessions
                .get("home")
                .context("session")?
                .holds_traffic
        );

        engine.registry.config.on_core_exit = oxidom_core::config::OnCoreExit::Hold;
        engine.configure_hold_traffic("home", None);
        assert!(
            engine
                .sessions
                .get("home")
                .context("session")?
                .holds_traffic
        );

        // A profile that answered for itself is not overruled by the machine.
        engine.registry.config.on_core_exit = oxidom_core::config::OnCoreExit::Hold;
        engine.configure_hold_traffic("home", Some(oxidom_core::config::OnCoreExit::Release));
        assert!(
            !engine
                .sessions
                .get("home")
                .context("session")?
                .holds_traffic
        );
        Ok(())
    }

    /// An explicit down is unchanged, and it is the difference the whole setting
    /// rests on: a person asking for their ordinary connection back gets it,
    /// held session or not.
    #[test]
    fn an_explicit_down_releases_a_session_that_was_holding_traffic() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("down-after-hold")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            mark_active(&mut engine, "home", "same")?;
            *oxidom_core::sync::lock(
                &engine
                    .sessions
                    .get("home")
                    .context("test session")?
                    .core
                    .status,
            ) = Status::Connected;
            engine.sessions.claim_system_proxy("home")?;
        }
        assert_eq!(service.shared.note_core_deaths().len(), 1);

        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            assert_eq!(engine.sessions.owner_of_system_proxy(), Some("home"));
            engine.stop_session("home");
            assert!(engine.sessions.owner_of_system_proxy().is_none());
            assert!(
                engine
                    .sessions
                    .get("home")
                    .is_some_and(|session| !session.holding_traffic)
            );
        }
        Ok(())
    }

    /// Reviving a tunnel whose core is alive but silent takes the same route,
    /// and used to release the same things.
    #[test]
    fn reviving_a_silent_tunnel_holds_its_traffic_too() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("silent-tunnel-hold")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.reconnect = true;
            engine.registry.config.watchdog = WatchdogMode::Aggressive;
            mark_active(&mut engine, "default", "silent-server")?;
            let session = engine.default_session_mut().context("default session")?;
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
            engine.sessions.claim_system_proxy("default")?;
        }

        let generation = watchdog_test_attempt(&service.shared, WatchdogMode::Aggressive)?;
        assert!(
            service
                .shared
                .recover_dead_tunnel("default", generation)
                .is_some()
        );

        let engine = oxidom_core::sync::lock(&service.shared.engine);
        assert_eq!(engine.sessions.owner_of_system_proxy(), Some("default"));
        assert!(
            engine
                .sessions
                .get("default")
                .is_some_and(|session| session.holding_traffic)
        );
        Ok(())
    }

    /// The whole point of the verb: it runs a selection and leaves the profiles
    /// directory exactly as it found it. Connecting used to write the selection
    /// into a profile first, and refuse when there was none to write to.
    #[test]
    fn connecting_a_selection_writes_no_profile() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("ad-hoc-pool")?;
        let service = for_test();
        let before = oxidom_core::profile::list()?;

        // No servers are stored, so the connect cannot get as far as spawning a
        // core — which is the point: even the failing path must not write.
        let query = oxidom_core::pool::PoolQuery {
            name: "Europe".to_string(),
            countries: vec!["de".to_string()],
            ..oxidom_core::pool::PoolQuery::default()
        };
        let error = service
            .connect_pool(serde_json::to_string(&query)?)
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains("no profile"),
            "a missing profile is no longer a reason to refuse: {error}"
        );

        assert_eq!(
            oxidom_core::profile::list()?,
            before,
            "connecting a selection creates no profile"
        );
        Ok(())
    }

    /// Two pools in two sessions run at once without colliding on ports, and
    /// an absent session name still means `default`.
    #[test]
    fn a_pool_can_be_raised_in_a_named_session_and_defaults_to_default() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("pool-named-session")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;

        let unnamed = oxidom_core::pool::PoolQuery::default();
        assert_eq!(unnamed.session_or_default(), "default");
        assert_eq!(
            service.connect_pool(serde_json::to_string(&unnamed)?)?,
            "default"
        );

        let named = oxidom_core::pool::PoolQuery {
            name: "Work pool".to_string(),
            session: "work".to_string(),
            ..oxidom_core::pool::PoolQuery::default()
        };
        assert_eq!(
            service.connect_pool(serde_json::to_string(&named)?)?,
            "work"
        );
        let default_pid = confirm_live_session(&service, "default")?;
        let work_pid = confirm_live_session(&service, "work")?;
        assert_ne!(default_pid, work_pid);

        let engine = oxidom_core::sync::lock(&service.shared.engine);
        let default_session = engine.sessions.get("default").context("default session")?;
        let work_session = engine.sessions.get("work").context("work session")?;
        assert_ne!(default_session.address, work_session.address);
        assert_eq!(work_session.socks_port, engine.registry.config.socks_port);
        assert_eq!(work_session.http_port, engine.registry.config.http_port);
        drop(engine);
        service.stop_all_profiles()?;
        Ok(())
    }

    #[test]
    fn two_named_pools_keep_live_independent_cores_when_one_is_stopped() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("two-named-live-pools")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .standby_proxy = true;
        service.shared.reconcile_standby();
        for name in ["work", "home"] {
            let query = oxidom_core::pool::PoolQuery {
                session: name.into(),
                ..Default::default()
            };
            assert_eq!(service.connect_pool(serde_json::to_string(&query)?)?, name);
            confirm_live_session(&service, name)?;
            service.shared.reconcile_standby();
            assert!(oxidom_core::sync::lock(&service.shared.standby).is_none());
        }
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            let work = engine.sessions.get_mut("work").context("work")?;
            assert!(work.is_alive());
            let work_pid = work.core.child_pid();
            let home = engine.sessions.get_mut("home").context("home")?;
            assert!(home.is_alive());
            assert_ne!(work_pid, home.core.child_pid());
        }
        service.stop_profile("work")?;
        assert!(oxidom_core::sync::lock(&service.shared.standby).is_none());
        confirm_live_session(&service, "home")?;
        service.stop_profile("home")?;
        assert!(
            oxidom_core::sync::lock(&service.shared.standby).is_some(),
            "stopping the last named session restores standby"
        );
        service.shared.stop_standby();
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
    fn every_status_surface_waits_for_traffic_confirmation() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("public-confirmation-status")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let id = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            let id = engine
                .all_servers()
                .next()
                .context("test server")?
                .id
                .clone();
            let socks_port = engine.registry.config.socks_port;
            let http_port = engine.registry.config.http_port;
            engine.prepare_session("default", Ipv4Addr::LOCALHOST, socks_port, http_port)?;
            engine.connect_session("default", &id)?;
            id
        };
        service.shared.next_connect_generation("default");
        let status: StatusInfo = serde_json::from_str(&service.status()?)?;
        assert_eq!(status.state, "connecting");
        assert_eq!(status.active_id.as_deref(), Some(id.as_str()));
        assert_eq!(status.sessions[0].state, "connecting");
        let sessions: Vec<SessionInfo> = serde_json::from_str(&service.list_sessions()?)?;
        assert_eq!(sessions[0].state, "connecting");
        let session: SessionInfo =
            serde_json::from_str(&service.session_status("default".into())?)?;
        assert_eq!(session.state, "connecting");

        confirm_live_session(&service, "default")?;
        let status: StatusInfo = serde_json::from_str(&service.status()?)?;
        assert_eq!(status.state, "connected");
        assert_eq!(status.sessions[0].state, "connected");
        assert_eq!(
            service
                .shared
                .session_info("default")
                .context("session")?
                .state,
            "connected"
        );
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
            on_core_exit: None,
            routing: Some(
                r#"{"rules":[{"domain":["geosite:category-ads-all"],"outboundTag":"block"}]}"#
                    .to_string(),
            ),
        };

        service.save_profile("work".to_string(), serde_json::to_string(&profile)?)?;
        let loaded: Profile = serde_json::from_str(&service.get_profile("work".to_string())?)?;
        assert_eq!(loaded, profile);

        let entries: Vec<ProfileEntry> = serde_json::from_str(&service.list_profiles()?)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "work");
        assert_eq!(entries[0].server, "ch-trojan");
        assert_eq!(entries[0].socks_port, 21080);
        // The listing carries the block too: the editor rewrites a profile from
        // this entry, and one that did not carry it would drop the rules on the
        // next save.
        assert_eq!(entries[0].routing, profile.routing);

        // A block the daemon cannot use is refused where the caller still sees
        // why, rather than at spawn where it would be an Xray exit code.
        let unusable = Profile {
            routing: Some(
                r#"{"rules":[{"domain":["x.example"],"outboundTag":"warp"}]}"#.to_string(),
            ),
            ..profile.clone()
        };
        let error = service
            .save_profile("broken".to_string(), serde_json::to_string(&unusable)?)
            .unwrap_err()
            .to_string();
        assert!(error.contains("\"warp\""), "got: {error}");

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

    /// Every id that enters the queue must leave it with a reading: a client
    /// retires its spinner on the id leaving `running ∪ queued`, so a cancel
    /// that wrote nothing would leave a card checking for ever. The reading
    /// says the check was stopped rather than that the server failed, and it
    /// replaces the previous number rather than letting a stale one pass as
    /// freshly measured.
    #[test]
    fn a_cancelled_probe_leaves_a_reading_that_blames_no_server() {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("cancel-probes");
        let shared = Shared::new(Engine::load(), false, false, false);

        oxidom_core::sync::lock(&shared.probes).enqueue(
            ProbeTarget::Direct("waiting".to_string()),
            "waiting".to_string(),
            None,
        );

        assert_eq!(shared.cancel_probes(&["waiting".to_string()]), 1);
        assert_eq!(
            shared.cancel_probes(&["waiting".to_string()]),
            0,
            "asking twice is not an error and cancels nothing the second time"
        );

        let readings = oxidom_core::sync::lock(&shared.readings);
        let reading = readings
            .get("waiting")
            .expect("a cancelled id leaves a reading");
        assert_eq!(reading.failure, Some(ProbeFailure::Unknown));
        assert_eq!(reading.detail, Some(ProbeDetail::Cancelled));
        assert!(
            reading.value.is_none(),
            "a cancelled check measured nothing, and failure implies no value"
        );
        assert_eq!(
            reading.detail.unwrap().message(),
            "the check was stopped before it ran"
        );
        drop(readings);
        drop(root);
    }

    /// Ten deep, newest first. The bound is what makes the history affordable
    /// for a subscription of several hundred servers that have all been swept:
    /// the daemon holds every one of these lists at once.
    #[test]
    fn a_history_keeps_the_last_ten_checks_newest_first() {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("history-bound");
        let shared = Shared::new(Engine::load(), false, false, false);

        for ms in 1..=(PROBE_HISTORY_LIMIT as u32 + 5) {
            shared.record_history(
                "berlin",
                LatencyReading::ok(ms, ProbeRoute::Direct, LatencyMethod::default()),
            );
        }

        let history = oxidom_core::sync::lock(&shared.history);
        let entries = history.get("berlin").expect("a checked server has one");
        assert_eq!(entries.len(), PROBE_HISTORY_LIMIT);
        assert_eq!(
            entries.first().and_then(|reading| reading.value),
            Some(PROBE_HISTORY_LIMIT as u32 + 5),
            "newest first"
        );
        assert_eq!(
            entries.last().and_then(|reading| reading.value),
            Some(6),
            "the oldest five fell off the end rather than the newest five"
        );
        drop(history);
        drop(root);
    }

    /// A check called off before it ran measured nothing about the server, so
    /// it says nothing the history is for. It must also not *displace*
    /// anything: one press of Stop on a 600-server sweep would otherwise push
    /// every server's real record out of a ten-deep list, erasing the history
    /// by filling it.
    #[test]
    fn a_stopped_check_leaves_the_history_alone() {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("history-cancel");
        let shared = Shared::new(Engine::load(), false, false, false);

        shared.record_history(
            "waiting",
            LatencyReading::ok(41, ProbeRoute::Direct, LatencyMethod::default()),
        );
        oxidom_core::sync::lock(&shared.probes).enqueue(
            ProbeTarget::Direct("waiting".to_string()),
            "waiting".to_string(),
            None,
        );
        assert_eq!(shared.cancel_probes(&["waiting".to_string()]), 1);

        assert_eq!(
            oxidom_core::sync::lock(&shared.readings)
                .get("waiting")
                .and_then(|reading| reading.detail),
            Some(ProbeDetail::Cancelled),
            "the current reading still owes the card an answer"
        );
        let history = oxidom_core::sync::lock(&shared.history);
        let entries = history.get("waiting").expect("the measured check is kept");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].value, Some(41));
        drop(history);
        drop(root);
    }

    /// Histories are pruned with their server, for the same reason readings
    /// are: a subscription refresh that drops a server must not leave the
    /// daemon carrying ten readings for it for the rest of the session.
    #[test]
    fn a_history_is_pruned_with_the_server_it_describes() {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("history-prune");
        let shared = Shared::new(Engine::load(), false, false, false);

        shared.record_history(
            "gone",
            LatencyReading::ok(41, ProbeRoute::Direct, LatencyMethod::default()),
        );
        shared.prune_readings();

        assert!(
            oxidom_core::sync::lock(&shared.history).is_empty(),
            "no server by that id is loaded, so nothing should hold its history"
        );
        drop(root);
    }

    /// A server nobody has checked is not an error, and answers a payload the
    /// caller can render as "no checks yet" rather than one it must handle as
    /// a failure.
    #[test]
    fn a_server_never_checked_answers_an_empty_history() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("history-empty");
        let service = for_test();

        let history: ProbeHistory =
            serde_json::from_str(&service.probe_history("never-checked".to_string())?)?;
        assert!(history.readings.is_empty());

        service.shared.record_history(
            "berlin",
            LatencyReading::ok(41, ProbeRoute::Direct, LatencyMethod::default()),
        );
        let history: ProbeHistory =
            serde_json::from_str(&service.probe_history("berlin".to_string())?)?;
        assert_eq!(
            history.readings.iter().map(|r| r.value).collect::<Vec<_>>(),
            vec![Some(41)]
        );
        drop(root);
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
        assert!(queue.enqueue(
            ProbeTarget::Proxied("home".to_string()),
            "same".to_string(),
            None
        ));
        assert!(queue.enqueue(
            ProbeTarget::Proxied("work".to_string()),
            "same".to_string(),
            None
        ));
        assert!(!queue.enqueue(
            ProbeTarget::Proxied("home".to_string()),
            "same".to_string(),
            None
        ));

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
            None,
        );

        queue.start_now("default", "active", None);
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

    /// docs/spec/latency.md: no id leaves `running` (or the queue) without a
    /// reading — "a silent early return, by any route". A connect whose core
    /// never started claimed the slot with `start_now`, so the GUI watched the
    /// id run; releasing it with nothing written retired the spinner onto a
    /// stale number presented as this check's result.
    #[test]
    fn a_connect_that_never_started_leaves_a_reading_behind() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("failed-start-reading")?;
        let service = for_test();
        let server_id = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.xray_binary = "/nonexistent-oxidom-xray".to_string();
            engine.import_links("trojan://pw@one.example.invalid:443#One")?;
            engine.prepare_session(
                "default",
                std::net::Ipv4Addr::new(127, 72, 14, 9),
                12980,
                12981,
            )?;
            engine
                .all_servers()
                .next()
                .context("the imported server")?
                .id
                .clone()
        };
        let generation = service.shared.next_connect_generation("default");

        let result = service.shared.start_connection(
            "default",
            &server_id,
            generation,
            ConnectionOrigin::Explicit,
        );
        assert!(result.is_err(), "the core binary does not exist");

        let (running, queued) = oxidom_core::sync::lock(&service.shared.probes).snapshot();
        assert!(
            running.is_empty() && queued.is_empty(),
            "the slot was released"
        );
        let proxied = oxidom_core::sync::lock(&service.shared.proxied);
        let reading = proxied
            .get("default")
            .context("the departure left a reading, not a silence")?;
        assert!(reading.value.is_none(), "nothing was measured");
        Ok(())
    }

    /// A proxied probe carries the connect generation it set out to measure.
    /// One started before a reconnect finishes after the new core's
    /// confirmation, and a timeout measured across the restart must not
    /// overwrite the good reading — `oxidom status` printed `—` and the GUI
    /// showed a failure for a healthy tunnel until the next sweep.
    #[test]
    fn a_probe_of_a_replaced_session_does_not_overwrite_the_new_reading() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("stale-probe-generation")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            mark_active(&mut engine, "default", "gone")?;
            let session = engine.sessions.get("default").context("test session")?;
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
        }
        let stale = service.shared.next_connect_generation("default");
        service.shared.next_connect_generation("default");
        oxidom_core::sync::lock(&service.shared.proxied).insert(
            "default".to_string(),
            LatencyReading::ok(31, ProbeRoute::Proxied, LatencyMethod::HttpGet),
        );

        service.shared.run_session_probe("default", Some(stale));

        let proxied = oxidom_core::sync::lock(&service.shared.proxied);
        let reading = proxied.get("default").context("the confirmation reading")?;
        assert_eq!(reading.value, Some(31), "the new core keeps its reading");
        Ok(())
    }

    /// The complement: the guard discards only replaced attempts. A probe of
    /// the attempt that is still current lands its reading as before.
    #[test]
    fn a_probe_of_the_current_session_still_lands() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("current-probe-lands")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            mark_active(&mut engine, "default", "gone")?;
            let session = engine.sessions.get("default").context("test session")?;
            *oxidom_core::sync::lock(&session.core.status) = Status::Connected;
        }
        let generation = service.shared.next_connect_generation("default");

        service
            .shared
            .run_session_probe("default", Some(generation));

        let proxied = oxidom_core::sync::lock(&service.shared.proxied);
        assert!(
            proxied.contains_key("default"),
            "the current attempt's probe records its outcome"
        );
        Ok(())
    }

    /// The daemon end of creating a server by hand: a draft arrives as JSON,
    /// is validated through the outbound generator, stored in the local group
    /// with an alias, refused loudly as a duplicate, and removable.
    #[test]
    fn a_server_created_by_hand_is_listed_refused_twice_and_removable() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("create-server-by-hand")?;
        let service = for_test();
        let draft = serde_json::json!({
            "name": "Typed",
            "protocol": "trojan",
            "address": "one.example.invalid",
            "port": 443,
            "password": "invented"
        })
        .to_string();

        let created: oxidom_core::ipc::CreatedServer =
            serde_json::from_str(&service.create_server(draft.clone())?)?;
        assert!(created.alias.is_some(), "an alias was assigned");
        {
            let engine = oxidom_core::sync::lock(&service.shared.engine);
            assert!(
                engine
                    .all_servers()
                    .any(|server| server.id == created.id && server.link.is_none()),
                "the created server is listed, with no share link claimed"
            );
        }

        let error = service
            .create_server(draft)
            .expect_err("duplicates are refused");
        assert!(error.to_string().contains("Typed"), "{error}");

        service.remove_server(created.id.clone())?;
        let engine = oxidom_core::sync::lock(&service.shared.engine);
        assert!(!engine.all_servers().any(|server| server.id == created.id));
        Ok(())
    }

    /// A pool's liveness probe is queued under the synthetic label
    /// `pool:<profile>`, which no server list will ever contain — the job
    /// lives and dies with its profile, not with a server id.
    #[test]
    fn a_prune_spares_the_queued_probe_of_a_living_pool() {
        let mut queue = ProbeQueue::default();
        queue.enqueue(
            ProbeTarget::Proxied("spread".to_string()),
            "pool:spread".to_string(),
            None,
        );

        queue.retain_alive(&HashSet::new(), &HashSet::from(["spread".to_string()]));
        let (_, queued) = queue.snapshot();
        assert_eq!(queued, vec!["pool:spread".to_string()]);

        queue.retain_alive(&HashSet::new(), &HashSet::new());
        let (_, queued) = queue.snapshot();
        assert!(
            queued.is_empty(),
            "the pool's profile went away, so the job goes too"
        );
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

    #[test]
    fn a_cancelled_probe_leaves_the_queue() {
        let mut queue = ProbeQueue::default();
        enqueue_direct(&mut queue, "one");
        enqueue_direct(&mut queue, "two");
        enqueue_direct(&mut queue, "three");

        let cancelled =
            queue.cancel_direct(&HashSet::from(["one".to_string(), "three".to_string()]));

        assert_eq!(
            cancelled
                .iter()
                .map(|job| job.server_id.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "three"],
            "the dropped jobs are reported so their readings can be written"
        );
        assert_eq!(queue.snapshot().1, vec!["two".to_string()]);
    }

    /// The whole point of cancelling is that the queue behind the running
    /// probes goes at once, which is where the minutes are: a sweep costs
    /// `ceil(servers / 8) × 10s` and only the eight in flight are unavoidable.
    #[test]
    fn cancelling_drops_the_queue_but_not_what_is_measuring() {
        let mut queue = ProbeQueue::default();
        for n in 0..MAX_CONCURRENT_PROBES + 5 {
            enqueue_direct(&mut queue, &format!("server-{n}"));
        }
        let running = drain_slots(&mut queue);
        assert_eq!(running.len(), MAX_CONCURRENT_PROBES, "the cap still holds");

        let all: HashSet<String> = (0..MAX_CONCURRENT_PROBES + 5)
            .map(|n| format!("server-{n}"))
            .collect();
        let cancelled = queue.cancel_direct(&all);

        assert_eq!(cancelled.len(), 5, "only the queued five were droppable");
        let (still_running, queued) = queue.snapshot();
        assert!(queued.is_empty(), "nothing is left waiting");
        assert_eq!(
            still_running.len(),
            MAX_CONCURRENT_PROBES,
            "a probe already measuring keeps its slot: its thread will finish it, \
             and releasing the slot early would hand it to a second worker"
        );
    }

    /// A cancel names servers, but a proxied job is keyed by profile and is the
    /// confirmation deciding whether a live tunnel stays up. Naming its server
    /// must not call it off.
    #[test]
    fn cancelling_a_server_does_not_call_off_its_live_connection_check() {
        let mut queue = ProbeQueue::default();
        assert!(queue.enqueue(
            ProbeTarget::Proxied("work".to_string()),
            "shared-server".to_string(),
            None,
        ));

        let cancelled = queue.cancel_direct(&HashSet::from(["shared-server".to_string()]));

        assert!(
            cancelled.is_empty(),
            "the session check is not the user's to cancel here"
        );
        assert_eq!(queue.snapshot().1, vec!["shared-server".to_string()]);
    }

    #[test]
    fn cancelling_a_probe_that_is_not_queued_is_not_an_error() {
        let mut queue = ProbeQueue::default();
        enqueue_direct(&mut queue, "kept");

        assert!(
            queue
                .cancel_direct(&HashSet::from(["absent".to_string()]))
                .is_empty()
        );
        assert!(queue.cancel_direct(&HashSet::new()).is_empty());
        assert_eq!(
            queue.snapshot().1,
            vec!["kept".to_string()],
            "nothing else moved"
        );
    }

    #[test]
    fn status_resumes_standby_after_a_released_core_death_but_never_a_held_one() -> Result<()> {
        dead_core_reconciles_standby(true)
    }

    #[test]
    fn supervision_resumes_standby_after_a_released_core_death_but_never_a_held_one() -> Result<()>
    {
        dead_core_reconciles_standby(false)
    }

    fn established_fake_session(
        service: &Service,
        policy: oxidom_core::config::OnCoreExit,
    ) -> Result<u32> {
        let server_id = {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.standby_proxy = true;
            engine.registry.config.on_core_exit = policy;
            let socks_port = engine.registry.config.socks_port;
            let http_port = engine.registry.config.http_port;
            let address = bind::address_for("work", &engine.sessions.taken_addresses())
                .context("work address")?;
            engine.prepare_session("work", address, socks_port, http_port)?;
            engine.configure_hold_traffic("work", None);
            engine
                .all_servers()
                .next()
                .context("invented server")?
                .id
                .clone()
        };
        service.shared.reconcile_standby();
        let generation = service.shared.next_connect_generation("work");
        let confirmed = service
            .shared
            .start_connection("work", &server_id, generation, ConnectionOrigin::Explicit)?
            .context("current connection")?;
        assert!(confirmed.recv_timeout(Duration::from_secs(5))?);
        let pid = confirm_live_session(service, "work")?;
        assert!(!service.shared.status_info().standby_proxy);
        Ok(pid)
    }

    fn kill_fake_session(service: &Service, pid: u32) -> Result<()> {
        let mut engine = oxidom_core::sync::lock(&service.shared.engine);
        let session = engine
            .sessions
            .get_mut("work")
            .context("established work session")?;
        assert_eq!(session.core.child_pid(), Some(pid));
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGKILL)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while session.is_alive() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            !session.is_alive(),
            "the fake child exited before lifecycle detection"
        );
        assert_eq!(
            session.status(),
            Status::Connected,
            "the session is still established until death is detected"
        );
        Ok(())
    }

    fn dead_core_reconciles_standby(use_status: bool) -> Result<()> {
        use oxidom_core::config::OnCoreExit;
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("standby-established-core-death")?;
        for policy in [OnCoreExit::Hold, OnCoreExit::Release] {
            let service = for_test();
            configure_fake_core(&service, &root)?;
            let pid = established_fake_session(&service, policy)?;
            kill_fake_session(&service, pid)?;
            if use_status {
                let status = service.shared.status_info();
                assert_eq!(
                    status.standby_proxy,
                    policy == OnCoreExit::Release,
                    "status reconciles standby after a core death with {policy:?}"
                );
            } else {
                service.shared.supervise_once();
            }
            assert_eq!(
                oxidom_core::sync::lock(&service.shared.standby).is_some(),
                policy == OnCoreExit::Release,
                "supervision reconciles standby after a core death with {policy:?}"
            );
            let sessions = service.shared.list_session_infos();
            assert_eq!(sessions[0].state, "error");
            assert_eq!(sessions[0].holding_traffic, policy == OnCoreExit::Hold);
            if policy == OnCoreExit::Release {
                let config = oxidom_core::sync::lock(&service.shared.engine)
                    .registry
                    .config
                    .clone();
                assert!(probe::socks_up(Ipv4Addr::LOCALHOST, config.socks_port));
            }
            service.stop_all_profiles()?;
            service.shared.stop_standby();
        }
        Ok(())
    }

    #[test]
    fn standby_never_resumes_between_core_death_detection_and_automatic_reconnect() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("standby-reconnect-gap")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let pid = established_fake_session(&service, oxidom_core::config::OnCoreExit::Release)?;
        oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .reconnect = true;
        kill_fake_session(&service, pid)?;
        let dead = service.shared.note_core_deaths();
        assert_eq!(dead.len(), 1);
        assert!(
            oxidom_core::sync::lock(&service.shared.override_status).is_empty(),
            "reconnect has not begun yet"
        );
        service.shared.reconcile_standby();
        assert!(
            oxidom_core::sync::lock(&service.shared.standby).is_none(),
            "even a concurrent reconcile must not open direct ports before begin_reconnect"
        );
        let (profile, target) = dead.into_iter().next().context("dead target")?;
        oxidom_core::sync::lock(&service.shared.override_status).insert(
            profile.clone(),
            ErrorOverride {
                status: Status::Error(RECONNECTING_MESSAGE.into()),
                selection: target.selection(),
            },
        );
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine
                .sessions
                .get_mut(&profile)
                .context("work")?
                .core
                .fail("retry did not start");
        }
        service.shared.reconcile_standby();
        assert!(
            oxidom_core::sync::lock(&service.shared.standby).is_none(),
            "a pending retry remains protected after a failed attempt"
        );
        oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .reconnect = false;
        service.shared.reconcile_standby();
        assert!(
            oxidom_core::sync::lock(&service.shared.standby).is_some(),
            "turning retry off permits the released session's standby"
        );
        service.stop_all_profiles()?;
        service.shared.stop_standby();
        Ok(())
    }

    #[test]
    fn a_disabled_standby_setting_closes_listeners_and_accepted_clients() -> Result<()> {
        use std::io::{Read, Write};
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("standby-settings-off")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;
        let mut config = oxidom_core::sync::lock(&service.shared.engine)
            .registry
            .config
            .clone();
        config.standby_proxy = true;
        service.set_settings(serde_json::to_string(&config)?)?;
        let mut older = serde_json::to_value(&config)?;
        older
            .as_object_mut()
            .context("settings object")?
            .remove("standby_proxy");
        service.set_settings(older.to_string())?;
        assert!(
            service.shared.status_info().standby_proxy,
            "a client that does not know the key preserves the opt-in"
        );
        let mut accepted = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, config.socks_port))?;
        accepted.set_read_timeout(Some(Duration::from_secs(1)))?;
        accepted.write_all(b"\x05\x01\x00")?;
        accepted.read_exact(&mut [0; 2])?;
        config.standby_proxy = false;
        service.set_settings(serde_json::to_string(&config)?)?;
        assert!(
            !oxidom_core::sync::lock(&service.shared.engine)
                .registry
                .config
                .standby_proxy,
            "the JSON payload disables, rather than preserves, standby"
        );
        assert!(!probe::socks_up(Ipv4Addr::LOCALHOST, config.socks_port));
        assert!(std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, config.http_port)).is_err());
        match accepted.read(&mut [0]) {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("disabled standby kept an accepted client: {other:?}"),
        }
        assert!(!service.shared.status_info().standby_proxy);
        assert!(
            !Config::load().standby_proxy,
            "the off setting is persisted"
        );
        Ok(())
    }

    #[test]
    fn standby_is_disabled_by_every_busy_or_held_profile() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("standby-named-session")?;
        let service = for_test();
        for (state, holding) in [
            (Status::Connected, false),
            (Status::Connecting, false),
            (Status::Error("core exited".into()), true),
        ] {
            {
                let mut engine = oxidom_core::sync::lock(&service.shared.engine);
                engine.registry.config.standby_proxy = true;
                engine.registry.config.socks_port = 0;
                engine.registry.config.http_port = 0;
                mark_active(&mut engine, "work", "gone")?;
                let session = engine.sessions.get_mut("work").context("work session")?;
                *oxidom_core::sync::lock(&session.core.status) = state;
                session.holding_traffic = holding;
            }
            service.shared.reconcile_standby();
            assert!(
                oxidom_core::sync::lock(&service.shared.standby).is_none(),
                "a named busy or held session forbids direct standby"
            );
        }
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            let session = engine.sessions.get_mut("work").context("work session")?;
            *oxidom_core::sync::lock(&session.core.status) = Status::Disconnected;
            session.holding_traffic = false;
        }
        for status in [Status::Connecting, Status::Connected] {
            oxidom_core::sync::lock(&service.shared.override_status).insert(
                "work".to_string(),
                ErrorOverride {
                    status,
                    selection: SessionSelection::Server("gone".into()),
                },
            );
            service.shared.reconcile_standby();
            assert!(
                oxidom_core::sync::lock(&service.shared.standby).is_none(),
                "a busy override also forbids direct standby"
            );
        }
        Ok(())
    }

    #[test]
    fn direct_standby_keeps_a_failed_defaults_error_in_the_daemon_snapshot() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("standby-failed-default")?;
        let service = for_test();
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.standby_proxy = true;
            engine.registry.config.socks_port = 0;
            engine.registry.config.http_port = 0;
            mark_active(&mut engine, "default", "failed-server")?;
        }
        oxidom_core::sync::lock(&service.shared.override_status).insert(
            "default".to_string(),
            ErrorOverride {
                status: Status::Error("the core refused the config".into()),
                selection: SessionSelection::Server("failed-server".into()),
            },
        );
        service.shared.reconcile_standby();
        let status: StatusInfo = serde_json::from_str(&service.status()?)?;
        assert!(status.standby_proxy);
        assert_eq!(status.state, "error");
        assert_eq!(status.error_id.as_deref(), Some("failed-server"));
        assert_eq!(status.error.as_deref(), Some("the core refused the config"));
        assert_eq!(status.sessions[0].error, status.error);
        service.shared.stop_standby();
        Ok(())
    }

    #[test]
    fn standby_proxy_answers_only_when_enabled_and_releases_ports_when_a_session_starts()
    -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let root = TestRoot::install("standby-proxy-lifecycle")?;
        let service = for_test();
        configure_fake_core(&service, &root)?;

        let socks_probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let socks_port = socks_probe.local_addr()?.port();
        drop(socks_probe);
        let http_probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let http_port = http_probe.local_addr()?.port();
        drop(http_probe);

        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.socks_port = socks_port;
            engine.registry.config.http_port = http_port;
            engine.registry.config.standby_proxy = false;
        }
        service.shared.reconcile_standby();
        assert!(!probe::socks_up(Ipv4Addr::LOCALHOST, socks_port));

        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.standby_proxy = true;
        }
        service.shared.reconcile_standby();
        assert!(probe::socks_up(Ipv4Addr::LOCALHOST, socks_port));
        let status: StatusInfo = serde_json::from_str(&service.status()?)?;
        assert!(status.standby_proxy);

        // Exercise the production entrypoint, not the stop helper. A live core
        // must take exactly these ports from standby and carry the confirmation.
        let server_id = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .next()
            .context("test server")?
            .id
            .clone();
        let shared = service.shared.clone();
        let (started, ready) = mpsc::channel();
        let (stop, stopping) = mpsc::channel();
        let reconciler = std::thread::spawn(move || {
            shared.reconcile_standby();
            let _ = started.send(());
            while matches!(stopping.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                shared.reconcile_standby();
                std::thread::yield_now();
            }
        });
        ready.recv()?;
        let connected = service.connect(server_id);
        stop.send(())?;
        reconciler.join().expect("reconciler did not panic");
        connected?;
        confirm_live_session(&service, "default")?;
        assert!(oxidom_core::sync::lock(&service.shared.standby).is_none());
        service.shared.reconcile_standby();
        assert!(oxidom_core::sync::lock(&service.shared.standby).is_none());
        service.stop_profile("default")?;
        assert!(probe::socks_up(Ipv4Addr::LOCALHOST, socks_port));
        service.shared.stop_standby();
        Ok(())
    }
}
