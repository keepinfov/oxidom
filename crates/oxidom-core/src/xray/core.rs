use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use crate::model::{OutboundSpec, Server};
use crate::proc::{push_log, spawn_reader, stop_child};
use crate::xray::config;
use crate::xray::resolve::{self, ResolvedXray};
use crate::{fsutil, paths};

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
    /// A pool core may outlive a killed daemon and be adopted by its next
    /// instance. `Child` cannot be reconstructed from a PID, so this is the
    /// equally supervised recovered form.
    recovered: Option<(u32, String)>,
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
            recovered: None,
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

    pub(crate) fn config_path(profile: &str) -> Result<PathBuf> {
        if !crate::profile::valid_name(profile) {
            bail!("refusing to build an Xray config path from invalid profile {profile:?}");
        }
        Ok(paths::data_dir()?.join(format!("current-config-{profile}.json")))
    }

    /// Refuse to start when a local inbound port is already taken; otherwise
    /// Xray exits instantly and the only symptom would be a failed probe.
    fn ensure_ports_free(&self, bind: Ipv4Addr, api_port: Option<u16>) -> Result<()> {
        let mut ports = vec![(self.socks_port, "SOCKS"), (self.http_port, "HTTP")];
        if let Some(api_port) = api_port {
            ports.push((api_port, "API"));
        }
        for (index, (port, label)) in ports.iter().enumerate() {
            if let Some((_, other_label)) = ports[..index]
                .iter()
                .find(|(other_port, _)| other_port == port)
            {
                bail!("local {label} and {other_label} endpoints cannot share port {port}");
            }
        }
        for (port, label) in ports {
            if std::net::TcpListener::bind((bind, port)).is_err() {
                if label == "API" {
                    bail!(
                        "local API endpoint {bind}:{port} was taken before Xray could bind it — \
                         retry the connection to allocate another port"
                    );
                }
                bail!(
                    "local {label} endpoint {bind}:{port} is already in use — pick a different \
                     port in Settings"
                );
            }
        }
        Ok(())
    }

    /// Start (or restart) the core for `server`.
    pub fn connect(&mut self, server: &Server, bind: Ipv4Addr, profile: &str) -> Result<()> {
        self.disconnect();
        self.set_status(Status::Connecting);
        crate::sync::lock(&self.logs).clear();
        match self.try_connect(server, bind, profile) {
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

    /// Start (or restart) the core for a resolved pool.
    pub fn connect_pool(
        &mut self,
        spec: &config::PoolSpec<'_>,
        bind: Ipv4Addr,
        api_port: u16,
        profile: &str,
    ) -> Result<()> {
        self.disconnect();
        self.set_status(Status::Connecting);
        crate::sync::lock(&self.logs).clear();
        match self.try_connect_pool(spec, bind, api_port, profile) {
            Ok(()) => Ok(()),
            Err(error) => {
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

    fn try_connect(&mut self, server: &Server, bind: Ipv4Addr, profile: &str) -> Result<()> {
        // Resolve before checking ports: a busy port must not mask a missing core.
        let xray = self.resolve_binary()?;
        self.preflight_notes(server);
        self.ensure_ports_free(bind, None)?;
        let cfg = config::generate(server, bind, self.socks_port, self.http_port);
        self.spawn_config(&xray, &cfg, profile)?;
        self.active = Some(server.clone());
        Ok(())
    }

    fn try_connect_pool(
        &mut self,
        spec: &config::PoolSpec<'_>,
        bind: Ipv4Addr,
        api_port: u16,
        profile: &str,
    ) -> Result<()> {
        let xray = self.resolve_binary()?;
        for member in spec.members {
            self.preflight_notes(member);
        }
        self.ensure_ports_free(bind, Some(api_port))?;
        let cfg = config::generate_pool(spec, bind, self.socks_port, self.http_port, api_port)?;
        self.spawn_config(&xray, &cfg, profile)?;
        self.active = None;
        Ok(())
    }

    fn spawn_config(
        &mut self,
        xray: &ResolvedXray,
        cfg: &serde_json::Value,
        profile: &str,
    ) -> Result<()> {
        let path = Self::config_path(profile)?;
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
        // Process launched. Readiness is confirmed by a latency probe from the caller;
        // mark Connected optimistically and let the caller downgrade on probe failure.
        self.set_status(Status::Connected);
        Ok(())
    }

    pub(crate) fn adopt(&mut self, pid: u32, profile: &str) {
        self.child = None;
        self.recovered = Some((pid, profile.to_string()));
        self.active = None;
        self.set_status(Status::Connected);
    }

    /// PID of the running xray child, persisted so a crashed instance's tunnel
    /// can be reaped on the next start.
    pub fn child_pid(&self) -> Option<u32> {
        self.child
            .as_ref()
            .map(Child::id)
            .or_else(|| self.recovered.as_ref().map(|(pid, _)| *pid))
    }

    pub fn disconnect(&mut self) {
        if let Some(mut child) = self.child.take() {
            stop_child(&mut child);
        }
        if let Some((pid, _)) = self.recovered.take() {
            let _ = crate::proc::stop_pid(pid);
        }
        self.active = None;
        self.set_status(Status::Disconnected);
    }

    /// True if the child is still running.
    pub fn is_alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => self.recovered.as_ref().is_some_and(|(pid, profile)| {
                let Ok(config) = Self::config_path(profile) else {
                    return false;
                };
                crate::proc::cmdline(*pid).is_some_and(|arguments| {
                    arguments
                        .iter()
                        .any(|argument| std::path::Path::new(argument) == config)
                })
            }),
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

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::XrayCore;

    #[test]
    fn config_paths_are_profile_scoped_and_validate_the_name() -> Result<()> {
        let work = XrayCore::config_path("work")?;
        let home = XrayCore::config_path("home")?;
        assert_ne!(work, home);
        assert_eq!(
            work.file_name().and_then(|name| name.to_str()),
            Some("current-config-work.json")
        );
        assert!(XrayCore::config_path("../outside").is_err());
        Ok(())
    }

    #[test]
    fn pool_api_port_cannot_alias_a_proxy_inbound() {
        let core = XrayCore::new(10808, 10809, String::new());

        let error = core
            .ensure_ports_free("127.77.1.1".parse().unwrap(), Some(10808))
            .unwrap_err()
            .to_string();

        assert!(error.contains("API and SOCKS endpoints cannot share port 10808"));
    }
}
