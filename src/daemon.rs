//! Headless daemon: owns the Engine (config, subscriptions, xray core,
//! probes) and exposes it on D-Bus. The GUI is a thin client; the tunnel
//! survives the GUI, logout only kills it in `--session` mode.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use gtk::glib;
use zbus::fdo;

use crate::config::{Config, LatencyMethod};
use crate::engine::Engine;
use crate::ipc::{
    ApplySettingsResult, BUS_NAME, LatencyReading, OBJECT_PATH, PROBE_STATE_VERSION, ProbeFailure,
    ProbeRoute, ProbeState, RuntimeInfo, StatusInfo,
};
use crate::model::Server;
use crate::probe;
use crate::xray::core::Status;

/// How many servers may be measured at once. This is now a cap on *processes*:
/// an HTTP probe starts a core of its own to make its request through, so a
/// "check all" over a large subscription would otherwise fork one Xray per
/// server and take the machine with it.
const MAX_CONCURRENT_PROBES: usize = 8;
const ACTIVE_PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// The probe pipeline: what is being measured now, and what is waiting for a
/// slot behind `MAX_CONCURRENT_PROBES`.
///
/// One lock rather than the two it replaces. Every interesting question — "is
/// this id already spoken for?", "may another probe start?" — spans both sets,
/// so the old pair carried an unwritten `checking` → `probe_queue` ordering
/// that held only because the two call sites remembered to take them that way.
#[derive(Default)]
struct ProbeQueue {
    running: HashSet<String>,
    queued: VecDeque<String>,
}

impl ProbeQueue {
    /// Take an id unless it is already spoken for. Returns whether it was
    /// newly queued.
    fn enqueue(&mut self, server_id: String) -> bool {
        if self.holds(&server_id) {
            return false;
        }
        self.queued.push_back(server_id);
        true
    }

    fn holds(&self, server_id: &str) -> bool {
        self.running.contains(server_id) || self.queued.iter().any(|id| id == server_id)
    }

    /// Promote the next waiting id, when a slot is free.
    fn start_next(&mut self) -> Option<String> {
        if self.running.len() >= MAX_CONCURRENT_PROBES {
            return None;
        }
        let server_id = self.queued.pop_front()?;
        self.running.insert(server_id.clone());
        Some(server_id)
    }

    /// Run `server_id` now, past the queue and past the cap. The confirmation
    /// after a connect is not a queued measurement: it decides whether the
    /// tunnel the user is watching stays up, and cannot wait behind a bulk
    /// re-check of a whole subscription.
    fn start_now(&mut self, server_id: &str) {
        self.queued.retain(|id| id != server_id);
        self.running.insert(server_id.to_string());
    }

    fn finish(&mut self, server_id: &str) {
        self.running.remove(server_id);
    }

    /// Both sets as the wire wants them.
    fn snapshot(&self) -> (Vec<String>, Vec<String>) {
        (
            self.running.iter().cloned().collect(),
            self.queued.iter().cloned().collect(),
        )
    }

    /// Drop queued ids that are no longer backed by a server. `running` is left
    /// alone on purpose: each of those has a thread that will `finish` it, and
    /// removing the entry early would hand out a slot that is still occupied.
    fn retain_alive(&mut self, alive: &HashSet<String>) {
        self.queued.retain(|id| alive.contains(id));
    }
}

/// A failure the daemon reports in place of the core's own status, together
/// with the server it belongs to.
///
/// The id has to be carried explicitly: every path that sets one of these has
/// already called `engine.disconnect()`, which clears `active_server_id` — so
/// by the time the failure is reportable, the only record of *which* server
/// failed is this struct.
#[derive(Clone)]
struct ErrorOverride {
    status: Status,
    server_id: String,
}

pub struct DaemonOptions {
    pub system_bus: bool,
    pub socks_port: Option<u16>,
    pub http_port: Option<u16>,
}

#[derive(Clone)]
struct Shared {
    engine: Arc<Mutex<Engine>>,
    readings: Arc<Mutex<HashMap<String, LatencyReading>>>,
    probes: Arc<Mutex<ProbeQueue>>,
    /// Layered over the core status, e.g. when the confirming probe after a
    /// connect fails and the daemon shuts the tunnel back down.
    override_status: Arc<Mutex<Option<ErrorOverride>>>,
    /// Bumped by every connect and disconnect, so an in-flight confirmation
    /// can tell whether the tunnel it was checking is still the current one.
    connect_generation: Arc<AtomicU64>,
    /// Ports pinned by `--socks-port`/`--http-port`, i.e. by the service unit.
    socks_port_locked: bool,
    http_port_locked: bool,
    /// True when serving the system bus, where callers are other users rather
    /// than the person who started the daemon.
    system_bus: bool,
}

impl Shared {
    fn new(
        engine: Engine,
        socks_port_locked: bool,
        http_port_locked: bool,
        system_bus: bool,
    ) -> Self {
        Shared {
            engine: Arc::new(Mutex::new(engine)),
            readings: Arc::new(Mutex::new(HashMap::new())),
            probes: Arc::new(Mutex::new(ProbeQueue::default())),
            override_status: Arc::new(Mutex::new(None)),
            connect_generation: Arc::new(AtomicU64::new(0)),
            socks_port_locked,
            http_port_locked,
            system_bus,
        }
    }

    /// Invalidate any in-flight connect confirmation and return the id of the
    /// attempt starting now.
    fn next_connect_generation(&self) -> u64 {
        self.connect_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn status_info(&self) -> StatusInfo {
        // Scoped, not `if let`: in edition 2024 an `if let` scrutinee's
        // temporary lives for the whole body, so holding this guard while
        // touching `engine` below would take the two locks in the opposite
        // order from `confirm_connection` — engine first, then override — and
        // deadlock the daemon the moment the two raced.
        let override_status = self.override_status.lock().unwrap().clone();
        if let Some(failure) = override_status {
            return StatusInfo::from_status(&failure.status, None)
                .with_error_id(Some(failure.server_id));
        }
        let mut engine = self.engine.lock().unwrap();
        if engine.status() == Status::Connected && !engine.core.is_alive() {
            // Record the death once rather than re-deriving it on every poll,
            // so the log keeps one line and the GUI toasts one transition.
            engine.core.fail("Xray exited unexpectedly");
        }
        let status = engine.status();
        let active = engine.state.active_server_id.clone();
        StatusInfo::from_status(&status, active)
    }

    fn runtime_info(&self) -> RuntimeInfo {
        let engine = self.engine.lock().unwrap();
        let (xray_path, xray_error, xray_source) = match engine.core.resolve_binary() {
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
            socks_port: engine.config.socks_port,
            http_port: engine.config.http_port,
        }
    }

    /// The server to probe, the config to probe it with, and how to reach it:
    /// only the server the tunnel is actually carrying may be measured through
    /// the proxy, everything else is measured on its own merits.
    fn probe_target(&self, server_id: &str) -> Option<(Server, Config, probe::Route)> {
        let engine = self.engine.lock().unwrap();
        let server = engine.find_server(server_id)?;
        let active = engine.state.active_server_id.as_deref() == Some(server_id)
            && engine.status() == Status::Connected;
        let route = if active {
            probe::Route::Proxied
        } else {
            probe::Route::Direct
        };
        Some((server, engine.config.clone(), route))
    }

    fn enqueue_probe(&self, server_id: String) {
        if !self.probes.lock().unwrap().enqueue(server_id) {
            return;
        }
        self.pump_probes();
    }

    fn pump_probes(&self) {
        loop {
            let Some(next) = self.probes.lock().unwrap().start_next() else {
                return;
            };
            let shared = self.clone();
            std::thread::spawn(move || {
                shared.run_probe(&next);
                shared.probes.lock().unwrap().finish(&next);
                shared.pump_probes();
            });
        }
    }

    /// Probe one server and record the outcome. Every id that enters the queue
    /// leaves with a `readings` entry, including ids that no longer resolve —
    /// the GUI keys its spinner off that entry appearing, so a silent early
    /// return would leave the card checking forever.
    fn run_probe(&self, server_id: &str) -> Option<u32> {
        let reading = match self.probe_target(server_id) {
            Some((server, config, route)) => {
                let method = config.latency_method;
                let wire = wire_route(route);
                match probe::measure(&server, &config, route) {
                    // The reading carries the method that produced it, not the
                    // one the config asked for: a hysteria2 server may answer
                    // only ICMP. The card says which it was rather than
                    // passing a handshake off as the user's chosen probe.
                    Some(measured) => LatencyReading::ok(measured.ms, wire, measured.method),
                    // `probe::measure` collapses every failure into `None`, so
                    // this is as specific as phase 1 can honestly be. The
                    // method here is the one that was attempted.
                    None => LatencyReading::failed(ProbeFailure::Unreachable, wire, method),
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
        self.readings
            .lock()
            .unwrap()
            .insert(server_id.to_string(), reading);
        value
    }

    /// After a connect: confirm the tunnel actually works; tear it down and
    /// surface an error when it does not.
    ///
    /// `generation` identifies the connect attempt this confirmation belongs
    /// to. Without it a slow probe from a superseded attempt would tear down
    /// the healthy connection that replaced it — the server id alone does not
    /// distinguish two connects to the same server.
    /// The id is already `running` when this starts — `Service::connect` claims
    /// the slot — so this thread owns it and must release it on every path.
    fn confirm_connection(&self, server_id: String, generation: u64) {
        let shared = self.clone();
        std::thread::spawn(move || {
            let (socks_port, method) = {
                let engine = shared.engine.lock().unwrap();
                (engine.config.socks_port, engine.config.latency_method)
            };

            // The core being alive proves nothing: readiness is the inbound
            // accepting connections. Waiting here is also what keeps the probe
            // below from racing a core that simply has not bound yet.
            let ready = probe::wait_for_socks(socks_port);

            let latency = if ready {
                shared.run_probe(&server_id)
            } else {
                // Nothing could be measured, but the GUI is waiting on this id
                // and would otherwise see the spinner retire onto whatever the
                // map still held. Record the failure it actually is.
                shared.readings.lock().unwrap().insert(
                    server_id.clone(),
                    LatencyReading::failed(ProbeFailure::Unreachable, ProbeRoute::Proxied, method),
                );
                None
            };
            shared.probes.lock().unwrap().finish(&server_id);
            if ready && latency.is_some() {
                return;
            }
            let mut engine = shared.engine.lock().unwrap();
            // Bail out if another connect/disconnect superseded this attempt:
            // the tunnel now running is not the one this thread was confirming.
            if shared.connect_generation.load(Ordering::SeqCst) != generation {
                return;
            }
            // A core that rejected the config exited at once, so both the dead
            // inbound and the failed probe are symptoms. Say the actual cause.
            let reason = if core_rejected_the_protocol(&engine.core.recent_logs()) {
                format!(
                    "the core does not support this server's protocol — {}",
                    crate::xray::core::HYSTERIA2_CORE_HINT
                )
            } else if ready {
                "active server did not pass its latency check".to_string()
            } else {
                "the local SOCKS inbound never came up — the core is not carrying traffic"
                    .to_string()
            };
            let still_active = engine.state.active_server_id.as_deref() == Some(&server_id);
            if still_active && engine.status() == Status::Connected {
                // Leave the reason in the log buffer too: the tunnel is
                // torn down below, so the core's own status is lost.
                engine.core.note(&reason);
                engine.disconnect();
                *shared.override_status.lock().unwrap() = Some(ErrorOverride {
                    status: Status::Error(reason),
                    server_id: server_id.clone(),
                });
            }
        });
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
        let alive: HashSet<String> = {
            let engine = self.engine.lock().unwrap();
            engine
                .all_servers()
                .map(|server| server.id.clone())
                .collect()
        };
        self.readings
            .lock()
            .unwrap()
            .retain(|id, _| alive.contains(id));
        self.probes.lock().unwrap().retain_alive(&alive);
    }

    /// Periodic re-probe of the active server; keeps the latency reading
    /// fresh but never tears an established connection down.
    fn spawn_active_probe_loop(&self) {
        let shared = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(ACTIVE_PROBE_INTERVAL);
                let active = {
                    let mut engine = shared.engine.lock().unwrap();
                    if engine.status() != Status::Connected || !engine.core.is_alive() {
                        continue;
                    }
                    engine.state.active_server_id.clone()
                };
                if let Some(id) = active {
                    // `enqueue_probe` already refuses an id that is running or
                    // waiting, so this cannot pile up behind a slow probe.
                    shared.enqueue_probe(id);
                }
            }
        });
    }
}

struct Service {
    shared: Shared,
}

/// `{:#}` keeps anyhow's cause chain; `to_string()` would send only the
/// outermost context ("spawning xray") and drop the reason it failed.
fn failed(error: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(format!("{error:#}"))
}

/// The prober's route as the wire spells it. Kept as an explicit mapping so
/// adding a route to one side is a compile error rather than a silent
/// mislabelling of where a number came from.
fn wire_route(route: probe::Route) -> ProbeRoute {
    match route {
        probe::Route::Direct => ProbeRoute::Direct,
        probe::Route::Proxied => ProbeRoute::Proxied,
    }
}

fn json<T: serde::Serialize>(value: &T) -> fdo::Result<String> {
    serde_json::to_string(value).map_err(failed)
}

#[zbus::interface(name = "dev.keepinfov.oxidom1")]
impl Service {
    fn list_subscriptions(&self) -> fdo::Result<String> {
        let engine = self.shared.engine.lock().unwrap();
        json(&engine.subscriptions)
    }

    fn add_subscription(&self, url: String, name: String, send_hwid: bool) -> fdo::Result<()> {
        let name = (!name.is_empty()).then_some(name);
        let result = self
            .shared
            .engine
            .lock()
            .unwrap()
            .add_subscription(url, name, send_hwid);
        // After the failures too: a refresh that errors part-way through has
        // still replaced some of the list.
        self.shared.prune_readings();
        result.map_err(failed)
    }

    fn remove_subscription(&self, subscription_id: String) -> fdo::Result<bool> {
        let result = self
            .shared
            .engine
            .lock()
            .unwrap()
            .remove_subscription(&subscription_id);
        self.shared.prune_readings();
        result.map_err(failed)
    }

    fn refresh(&self, subscription_id: String) -> fdo::Result<()> {
        let result = self.shared.engine.lock().unwrap().refresh(&subscription_id);
        self.shared.prune_readings();
        result.map_err(failed)
    }

    fn refresh_all(&self) -> fdo::Result<()> {
        let result = self.shared.engine.lock().unwrap().refresh_all();
        self.shared.prune_readings();
        result.map_err(failed)
    }

    fn import_links(&self, text: String) -> fdo::Result<(u32, u32)> {
        let result = self.shared.engine.lock().unwrap().import_links(&text);
        self.shared.prune_readings();
        let (added, unsupported) = result.map_err(failed)?;
        Ok((added as u32, unsupported as u32))
    }

    fn remove_server(&self, server_id: String) -> fdo::Result<bool> {
        let result = self.shared.engine.lock().unwrap().remove_server(&server_id);
        self.shared.prune_readings();
        result.map_err(failed)
    }

    fn set_hwid(&self, subscription_id: String, enabled: bool) -> fdo::Result<()> {
        let mut engine = self.shared.engine.lock().unwrap();
        if let Some(subscription) = engine
            .subscriptions
            .iter_mut()
            .find(|subscription| subscription.id == subscription_id)
        {
            subscription.send_hwid = enabled;
        }
        engine.save().map_err(failed)
    }

    fn connect(&self, server_id: String) -> fdo::Result<()> {
        *self.shared.override_status.lock().unwrap() = None;
        let generation = self.shared.next_connect_generation();

        // Both of these happen before the tunnel comes up, and both are about
        // the same lie. The reading this server already has was taken *directly*
        // — it says nothing about the tunnel now being built — so it is dropped
        // rather than left to resurface as the new connection's ping. And the
        // slot is claimed here, not in the confirmation thread, so there is no
        // window in which the id is in neither set and the card can retire its
        // spinner onto a number nobody measured.
        self.shared.readings.lock().unwrap().remove(&server_id);
        self.shared.probes.lock().unwrap().start_now(&server_id);

        if let Err(error) = self.shared.engine.lock().unwrap().connect(&server_id) {
            // No confirmation will run for an attempt that never started, so
            // the slot has to be given back here or it is lost for good. The
            // id leaves without a reading on purpose: nothing was measured.
            self.shared.probes.lock().unwrap().finish(&server_id);
            return Err(failed(error));
        }
        self.shared.confirm_connection(server_id, generation);
        Ok(())
    }

    fn disconnect(&self) -> fdo::Result<()> {
        *self.shared.override_status.lock().unwrap() = None;
        self.shared.next_connect_generation();
        self.shared.engine.lock().unwrap().disconnect();
        Ok(())
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
        let (running, queued) = self.shared.probes.lock().unwrap().snapshot();
        let state = ProbeState {
            version: PROBE_STATE_VERSION,
            running,
            queued,
            readings: self.shared.readings.lock().unwrap().clone(),
        };
        json(&state)
    }

    fn get_settings(&self) -> fdo::Result<String> {
        json(&self.shared.engine.lock().unwrap().config)
    }

    fn set_settings(&self, config_json: String) -> fdo::Result<String> {
        let raw: serde_json::Value = serde_json::from_str(&config_json).map_err(failed)?;
        let mut config: Config = serde_json::from_value(raw.clone()).map_err(failed)?;
        let mut engine = self.shared.engine.lock().unwrap();

        // A GUI older than this key sends a payload without it; treat that as
        // "leave it alone" rather than clearing the path the daemon may need
        // to start xray at all.
        //
        // On the system bus the same key is a remote-exec primitive: the
        // daemon runs as its own user and spawns whatever path this names, so
        // callers do not get to choose it. There the core comes from the unit's
        // environment or the on-disk config, both of which need root to touch.
        let mut ignored_settings = Vec::new();
        if raw.get("xray_binary").is_none() || self.shared.system_bus {
            if self.shared.system_bus && config.xray_binary != engine.config.xray_binary {
                ignored_settings.push("Xray binary path".to_string());
            }
            config.xray_binary = engine.config.xray_binary.clone();
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

        // Ports fixed on the command line by the service unit are refused
        // here, not just greyed out in the GUI: accepting the write would move
        // the inbound until the next restart silently put it back, breaking
        // anything pointed at the old port in the meantime.
        if self.shared.socks_port_locked && config.socks_port != engine.config.socks_port {
            config.socks_port = engine.config.socks_port;
            ignored_settings.push("SOCKS port".to_string());
        }
        if self.shared.http_port_locked && config.http_port != engine.config.http_port {
            config.http_port = engine.config.http_port;
            ignored_settings.push("HTTP port".to_string());
        }

        let ports_changed = engine.config.socks_port != config.socks_port
            || engine.config.http_port != config.http_port;
        engine.config = config;
        engine.core.socks_port = engine.config.socks_port;
        engine.core.http_port = engine.config.http_port;
        engine.core.xray_binary = engine.config.xray_binary.clone();
        engine.save().map_err(failed)?;
        let mut reconnect_error = None;
        if ports_changed && engine.status() == Status::Connected {
            // Same treatment as a user-driven connect: the restarted core has
            // to prove the new inbound is up before this counts as connected.
            if let Some(active) = engine.state.active_server_id.clone() {
                let generation = self.shared.next_connect_generation();
                match engine.connect(&active) {
                    Ok(()) => self.shared.confirm_connection(active, generation),
                    Err(error) => reconnect_error = Some(format!("{error:#}")),
                }
            }
        }
        json(&ApplySettingsResult {
            reconnect_error,
            ignored_ports: ignored_settings,
        })
    }

    fn runtime_info(&self) -> fdo::Result<String> {
        json(&self.shared.runtime_info())
    }

    fn recent_logs(&self) -> fdo::Result<Vec<String>> {
        Ok(self.shared.engine.lock().unwrap().core.recent_logs())
    }

    fn clear_logs(&self) -> fdo::Result<()> {
        self.shared.engine.lock().unwrap().core.clear_logs();
        Ok(())
    }
}

pub fn run(options: DaemonOptions) -> Result<()> {
    let mut engine = Engine::load();
    for warning in engine.load_warnings.drain(..) {
        log::warn!("{warning}");
    }
    if let Some(port) = options.socks_port {
        engine.config.socks_port = port;
        engine.core.socks_port = port;
    }
    if let Some(port) = options.http_port {
        engine.config.http_port = port;
        engine.core.http_port = port;
    }

    // Report the core up front: `journalctl -u oxidom` should show a missing
    // binary before anyone clicks Connect and wonders why it failed.
    match engine.core.resolve_binary() {
        Ok(resolved) => log::info!(
            "using the Xray core at {} (from {})",
            resolved.path.display(),
            resolved.source.label()
        ),
        Err(error) => log::warn!("no usable Xray core: {error:#}"),
    }

    let shared = Shared::new(
        engine,
        options.socks_port.is_some(),
        options.http_port.is_some(),
        options.system_bus,
    );
    shared.spawn_active_probe_loop();

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

    // Serve until SIGINT/SIGTERM, then shut the tunnel down cleanly.
    let main_loop = glib::MainLoop::new(None, false);
    for signal in [libc::SIGINT, libc::SIGTERM] {
        glib::unix_signal_add(signal, {
            let main_loop = main_loop.clone();
            move || {
                main_loop.quit();
                glib::ControlFlow::Break
            }
        });
    }
    main_loop.run();

    shared.engine.lock().unwrap().disconnect();
    log::info!("oxidom daemon stopped");
    Ok(())
}

/// Whether the core's own output says it could not build the outbound at all —
/// which is what an Xray older than the hysteria2 support does.
fn core_rejected_the_protocol(logs: &[String]) -> bool {
    logs.iter().any(|line| {
        crate::xray::core::UNSUPPORTED_PROTOCOL_MARKERS
            .iter()
            .any(|marker| line.contains(marker))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_slots(queue: &mut ProbeQueue) -> Vec<String> {
        std::iter::from_fn(|| queue.start_next()).collect()
    }

    /// The cap is what the GUI's queued spinners exist for: everything past it
    /// has been accepted but not measured, and must not read as finished.
    #[test]
    fn probes_past_the_cap_stay_queued() {
        let mut queue = ProbeQueue::default();
        for index in 0..MAX_CONCURRENT_PROBES + 3 {
            assert!(queue.enqueue(format!("s{index}")));
        }
        let started = drain_slots(&mut queue);
        assert_eq!(started.len(), MAX_CONCURRENT_PROBES);

        let (running, queued) = queue.snapshot();
        assert_eq!(running.len(), MAX_CONCURRENT_PROBES);
        assert_eq!(queued.len(), 3);

        // A slot freeing up lets exactly one more through.
        queue.finish(&started[0]);
        assert!(queue.start_next().is_some());
        assert_eq!(queue.snapshot().1.len(), 2);
    }

    #[test]
    fn an_id_is_never_queued_twice() {
        let mut queue = ProbeQueue::default();
        assert!(queue.enqueue("a".into()));
        assert!(!queue.enqueue("a".into()), "already waiting");
        queue.start_next();
        assert!(!queue.enqueue("a".into()), "already running");
        queue.finish("a");
        assert!(queue.enqueue("a".into()), "free to measure again");
    }

    /// The confirmation after a connect decides whether the tunnel the user is
    /// watching stays up, so it cannot wait behind a bulk re-check.
    #[test]
    fn a_confirmation_probe_jumps_the_queue_and_the_cap() {
        let mut queue = ProbeQueue::default();
        for index in 0..MAX_CONCURRENT_PROBES {
            queue.enqueue(format!("s{index}"));
        }
        drain_slots(&mut queue);
        queue.enqueue("active".into());

        queue.start_now("active");
        let (running, queued) = queue.snapshot();
        assert!(running.contains(&"active".to_string()));
        assert!(!queued.contains(&"active".to_string()), "no double start");
    }

    /// A server deleted mid-sweep must not come back out of the queue a moment
    /// later and leave a reading for something that no longer exists.
    #[test]
    fn removed_servers_leave_the_queue() {
        let mut queue = ProbeQueue::default();
        queue.enqueue("gone".into());
        queue.enqueue("kept".into());
        queue.start_next();

        queue.retain_alive(&HashSet::from(["kept".to_string()]));
        let (_, queued) = queue.snapshot();
        assert_eq!(queued, vec!["kept".to_string()]);
    }

    /// A running probe owns a slot until its thread reports back; forgetting it
    /// early would hand the slot out twice.
    #[test]
    fn a_running_probe_keeps_its_slot_through_a_prune() {
        let mut queue = ProbeQueue::default();
        queue.enqueue("gone".into());
        let started = queue.start_next().unwrap();

        queue.retain_alive(&HashSet::new());
        assert_eq!(queue.snapshot().0, vec![started]);
    }
}
