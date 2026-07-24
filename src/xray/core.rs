use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, bail};

use crate::model::Server;
use crate::xray::config;
use crate::xray::resolve::{self, ResolvedXray};
use crate::{fsutil, paths};

const LOG_CAP: usize = 500;

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
        self.status.lock().unwrap().clone()
    }

    fn set_status(&self, s: Status) {
        *self.status.lock().unwrap() = s;
    }

    fn config_path() -> Result<PathBuf> {
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
        self.logs.lock().unwrap().clear();
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

    fn try_connect(&mut self, server: &Server) -> Result<()> {
        // Resolve before checking ports: a busy port must not mask a missing core.
        let xray = self.resolve_binary()?;
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
            let _ = child.kill();
            let _ = child.wait();
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
        self.logs.lock().unwrap().clone()
    }

    pub fn clear_logs(&self) {
        self.logs.lock().unwrap().clear();
    }
}

impl Drop for XrayCore {
    fn drop(&mut self) {
        self.disconnect();
    }
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
    let mut logs = logs.lock().unwrap();
    if logs.len() >= LOG_CAP {
        logs.remove(0);
    }
    logs.push(line);
}
