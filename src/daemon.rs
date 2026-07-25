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

use crate::config::Config;
use crate::engine::Engine;
use crate::ipc::{ApplySettingsResult, BUS_NAME, OBJECT_PATH, ProbeState, RuntimeInfo, StatusInfo};
use crate::model::Server;
use crate::probe;
use crate::xray::core::Status;

const MAX_CONCURRENT_PROBES: usize = 8;
const ACTIVE_PROBE_INTERVAL: Duration = Duration::from_secs(30);

pub struct DaemonOptions {
    pub system_bus: bool,
    pub socks_port: Option<u16>,
    pub http_port: Option<u16>,
}

#[derive(Clone)]
struct Shared {
    engine: Arc<Mutex<Engine>>,
    latencies: Arc<Mutex<HashMap<String, Option<u32>>>>,
    checking: Arc<Mutex<HashSet<String>>>,
    probe_queue: Arc<Mutex<VecDeque<String>>>,
    /// Layered over the core status, e.g. when the confirming probe after a
    /// connect fails and the daemon shuts the tunnel back down.
    override_status: Arc<Mutex<Option<Status>>>,
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
            latencies: Arc::new(Mutex::new(HashMap::new())),
            checking: Arc::new(Mutex::new(HashSet::new())),
            probe_queue: Arc::new(Mutex::new(VecDeque::new())),
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
        if let Some(status) = self.override_status.lock().unwrap().clone() {
            return StatusInfo::from_status(&status, None);
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
        {
            let checking = self.checking.lock().unwrap();
            let mut queue = self.probe_queue.lock().unwrap();
            if checking.contains(&server_id) || queue.contains(&server_id) {
                return;
            }
            queue.push_back(server_id);
        }
        self.pump_probes();
    }

    fn pump_probes(&self) {
        loop {
            let next = {
                let mut checking = self.checking.lock().unwrap();
                if checking.len() >= MAX_CONCURRENT_PROBES {
                    return;
                }
                let Some(id) = self.probe_queue.lock().unwrap().pop_front() else {
                    return;
                };
                checking.insert(id.clone());
                id
            };
            let shared = self.clone();
            std::thread::spawn(move || {
                shared.run_probe(&next);
                shared.checking.lock().unwrap().remove(&next);
                shared.pump_probes();
            });
        }
    }

    /// Probe one server and record the outcome. Every id that enters
    /// `checking` leaves with a `latencies` entry, including ids that no longer
    /// resolve — the GUI keys its spinner off that entry appearing, so a silent
    /// early return would leave the card checking forever.
    fn run_probe(&self, server_id: &str) -> Option<u32> {
        let latency = self
            .probe_target(server_id)
            .and_then(|(server, config, route)| {
                probe::measure(
                    &server,
                    config.latency_method,
                    config.socks_port,
                    &config.latency_test_url,
                    route,
                )
            });
        self.latencies
            .lock()
            .unwrap()
            .insert(server_id.to_string(), latency);
        latency
    }

    /// After a connect: confirm the tunnel actually works; tear it down and
    /// surface an error when it does not.
    ///
    /// `generation` identifies the connect attempt this confirmation belongs
    /// to. Without it a slow probe from a superseded attempt would tear down
    /// the healthy connection that replaced it — the server id alone does not
    /// distinguish two connects to the same server.
    fn confirm_connection(&self, server_id: String, generation: u64) {
        let shared = self.clone();
        std::thread::spawn(move || {
            let socks_port = { shared.engine.lock().unwrap().config.socks_port };

            // The core being alive proves nothing: readiness is the inbound
            // accepting connections. Waiting here is also what keeps the probe
            // below from racing a core that simply has not bound yet.
            let ready = probe::wait_for_socks(socks_port);

            shared.checking.lock().unwrap().insert(server_id.clone());
            let latency = if ready {
                shared.run_probe(&server_id)
            } else {
                None
            };
            shared.checking.lock().unwrap().remove(&server_id);
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
                *shared.override_status.lock().unwrap() = Some(Status::Error(reason));
            }
        });
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
                    let already = shared.checking.lock().unwrap().contains(&id);
                    if !already {
                        shared.enqueue_probe(id);
                    }
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
        self.shared
            .engine
            .lock()
            .unwrap()
            .add_subscription(url, name, send_hwid)
            .map_err(failed)
    }

    fn remove_subscription(&self, subscription_id: String) -> fdo::Result<bool> {
        self.shared
            .engine
            .lock()
            .unwrap()
            .remove_subscription(&subscription_id)
            .map_err(failed)
    }

    fn refresh(&self, subscription_id: String) -> fdo::Result<()> {
        self.shared
            .engine
            .lock()
            .unwrap()
            .refresh(&subscription_id)
            .map_err(failed)
    }

    fn refresh_all(&self) -> fdo::Result<()> {
        self.shared
            .engine
            .lock()
            .unwrap()
            .refresh_all()
            .map_err(failed)
    }

    fn import_links(&self, text: String) -> fdo::Result<(u32, u32)> {
        let (added, unsupported) = self
            .shared
            .engine
            .lock()
            .unwrap()
            .import_links(&text)
            .map_err(failed)?;
        Ok((added as u32, unsupported as u32))
    }

    fn remove_server(&self, server_id: String) -> fdo::Result<bool> {
        self.shared
            .engine
            .lock()
            .unwrap()
            .remove_server(&server_id)
            .map_err(failed)
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
        self.shared
            .engine
            .lock()
            .unwrap()
            .connect(&server_id)
            .map_err(failed)?;
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
        let state = ProbeState {
            checking: self
                .shared
                .checking
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect(),
            latencies: self.shared.latencies.lock().unwrap().clone(),
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
