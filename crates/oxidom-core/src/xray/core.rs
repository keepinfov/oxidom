use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::model::{OutboundSpec, Server};
use crate::xray::config;
use crate::xray::resolve::{self, ResolvedXray};
use crate::{fsutil, paths};

const LOG_CAP: usize = 500;
/// How long the core gets to exit on SIGTERM before it is killed outright.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// Said before every hysteria2 connect. Cheaper and more reliable than probing
/// the core's version: a git build or a fork can report anything, and the cost
/// would be an extra process spawn on every connect.
pub const HYSTERIA2_CORE_HINT: &str = "hysteria2 needs Xray 26.1 or newer, which is where the native \"hysteria\" protocol \
     landed; an older core exits immediately instead of connecting";

/// What an Xray too old for hysteria2 says on the way out. Used to turn a
/// failed latency check into an explanation.
pub const UNSUPPORTED_PROTOCOL_MARKERS: &[&str] = &[
    "unknown protocol",
    "Failed to build Hysteria config",
    "unknown transport protocol",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

/// Supervises a single Xray core process (one active server at a time).
pub struct XrayCore {
    child: Option<Child>,
    pub status: Arc<Mutex<Status>>,
    pub logs: Arc<Mutex<Vec<String>>>,
    pub socks_port: u16,
    pub http_port: u16,
    /// Configured path to the Xray binary; empty falls back to the environment
    /// and then `PATH`. See [`crate::xray::resolve`].
    pub xray_binary: String,
    pub active: Option<Server>,
}

impl XrayCore {
    pub fn new(socks_port: u16, http_port: u16, xray_binary: String) -> Self {
        XrayCore {
            child: None,
            status: Arc::new(Mutex::new(Status::Disconnected)),
            logs: Arc::new(Mutex::new(Vec::new())),
            socks_port,
            http_port,
            xray_binary,
            active: None,
        }
    }

    /// Locate the Xray binary without starting it. Used as a preflight before
    /// spawning, for the daemon's startup log, and to report the effective
    /// path to the GUI — which runs in a different process and environment and
    /// therefore cannot work this out for itself.
    pub fn resolve_binary(&self) -> Result<ResolvedXray> {
        resolve::resolve(&self.xray_binary)
    }

    /// Record an oxidom-side message in the same ring buffer as xray's output,
    /// so the Logs view can explain a failure even when xray never started.
    pub fn note(&self, message: &str) {
        push_log(&self.logs, format!("oxidom: {message}"));
    }

    /// Move to a failed state once, recording why. Callers that detect the
    /// failure on a poll must not re-derive it on every tick.
    pub fn fail(&mut self, message: &str) {
        self.note(message);
        self.set_status(Status::Error(message.to_string()));
    }

    pub fn status(&self) -> Status {
        crate::sync::lock(&self.status).clone()
    }

    fn set_status(&self, s: Status) {
        *crate::sync::lock(&self.status) = s;
    }

    pub(crate) fn config_path() -> Result<PathBuf> {
        Ok(paths::data_dir()?.join("current-config.json"))
    }

    /// Refuse to start when a local inbound port is already taken; otherwise
    /// Xray exits instantly and the only symptom would be a failed probe.
    fn ensure_ports_free(&self) -> Result<()> {
        for (port, label) in [(self.socks_port, "SOCKS"), (self.http_port, "HTTP")] {
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
                bail!(
                    "local {label} port {port} is already in use — pick a different port in Settings"
                );
            }
        }
        Ok(())
    }

    /// Start (or restart) the core for `server`.
    pub fn connect(&mut self, server: &Server) -> Result<()> {
        self.disconnect();
        self.set_status(Status::Connecting);
        crate::sync::lock(&self.logs).clear();
        match self.try_connect(server) {
            Ok(()) => Ok(()),
            Err(error) => {
                // `{:#}` keeps the anyhow cause chain: the outermost context
                // alone ("spawning xray") never says *why* it failed.
                let message = format!("{error:#}");
                self.note(&message);
                self.set_status(Status::Error(message));
                Err(error)
            }
        }
    }

    /// Record settings the link asked for that oxidom cannot pass on to the
    /// core. None of these stop the connection, but each one changes what
    /// "connected" will mean and the user has no other way to find out.
    fn preflight_notes(&self, server: &Server) {
        let (insecure, pinned) = match &server.spec {
            OutboundSpec::Hysteria2 { settings, .. } => {
                self.note(HYSTERIA2_CORE_HINT);
                if let Some(obfs) = &settings.obfs {
                    if obfs.kind.eq_ignore_ascii_case("salamander") {
                        self.note(
                            "this server uses salamander obfuscation; Xray's implementation is \
                             not known to interoperate with every hysteria2 server, so if the \
                             handshake times out ask the provider for a plain endpoint",
                        );
                    } else {
                        self.note(&format!(
                            "ignoring unknown \"{}\" obfuscation — Xray only implements \
                             salamander, and an unknown type stops the core from starting",
                            obfs.kind
                        ));
                    }
                }
                (settings.allow_insecure, settings.pin_sha256.is_some())
            }
            spec => match spec.stream() {
                Some(stream) => (stream.allow_insecure, stream.pin_sha256.is_some()),
                None => (false, false),
            },
        };

        if insecure && !pinned {
            self.note(
                "this link asks to skip certificate verification, but Xray 26.x removed \
                 \"allowInsecure\" — the certificate will be verified normally, so expect a TLS \
                 failure if the server presents a self-signed certificate",
            );
        }
    }

    fn try_connect(&mut self, server: &Server) -> Result<()> {
        // Resolve before checking ports: a busy port must not mask a missing core.
        let xray = self.resolve_binary()?;
        self.preflight_notes(server);
        self.ensure_ports_free()?;
        let cfg = config::generate(server, self.socks_port, self.http_port);
        let path = Self::config_path()?;
        // The generated config embeds the server credentials — keep it private.
        fsutil::write_private_atomic(&path, serde_json::to_string_pretty(&cfg)?.as_bytes())
            .context("writing xray config")?;

        let mut child = Command::new(&xray.path)
            .arg("run")
            .arg("-c")
            .arg(&path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning xray ({})", xray.path.display()))?;

        // Pump stdout and stderr into the ring buffer.
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, self.logs.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, self.logs.clone());
        }

        self.child = Some(child);
        self.active = Some(server.clone());
        // Process launched. Readiness is confirmed by a latency probe from the caller;
        // mark Connected optimistically and let the caller downgrade on probe failure.
        self.set_status(Status::Connected);
        Ok(())
    }

    /// PID of the running xray child, persisted so a crashed instance's tunnel
    /// can be reaped on the next start.
    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    pub fn disconnect(&mut self) {
        if let Some(mut child) = self.child.take() {
            stop_child(&mut child);
        }
        self.active = None;
        self.set_status(Status::Disconnected);
    }

    /// True if the child is still running.
    pub fn is_alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    pub fn recent_logs(&self) -> Vec<String> {
        crate::sync::lock(&self.logs).clone()
    }

    pub fn clear_logs(&self) {
        crate::sync::lock(&self.logs).clear();
    }
}

impl Drop for XrayCore {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Stop the core the way the spec requires: SIGTERM, then SIGKILL once the
/// grace period is up. `Child::kill` alone is an unconditional SIGKILL, which
/// severs every in-flight connection instead of letting xray close them.
fn stop_child(child: &mut Child) {
    let signalled = i32::try_from(child.id()).is_ok_and(|pid| {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        )
        .is_ok()
    });
    if signalled {
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(_) => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_reader<R: std::io::Read + Send + 'static>(reader: R, logs: Arc<Mutex<Vec<String>>>) {
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines().map_while(Result::ok) {
            push_log(&logs, line);
        }
    });
}

fn push_log(logs: &Arc<Mutex<Vec<String>>>, line: String) {
    let mut logs = crate::sync::lock(logs);
    if logs.len() >= LOG_CAP {
        logs.remove(0);
    }
    logs.push(line);
}
