use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use crate::core_options::ResolvedCore;
use crate::logbook::{self, LogSource, Severity};
use crate::model::{OutboundSpec, Server};
use crate::proc::{spawn_reader, stop_child};
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
    /// Profile whose session this core serves. Every line it files is tagged
    /// with it, because the log book is shared by every session in the process.
    profile: String,
    /// First sequence number belonging to the current run.
    ///
    /// The book is never cleared on connect, so this is what separates this
    /// attempt's output from the previous one's. Callers that read the core's
    /// own words to diagnose a failure must start here: a marker left by an
    /// earlier attempt would otherwise be read as this attempt's reason.
    spawn_seq: u64,
    pub socks_port: u16,
    pub http_port: u16,
    /// Configured path to the Xray binary; empty falls back to the environment
    /// and then `PATH`. See [`crate::xray::resolve`].
    pub xray_binary: String,
    pub active: Option<Server>,
}

impl XrayCore {
    pub fn new(profile: String, socks_port: u16, http_port: u16, xray_binary: String) -> Self {
        XrayCore {
            child: None,
            recovered: None,
            status: Arc::new(Mutex::new(Status::Disconnected)),
            profile,
            spawn_seq: 0,
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

    /// Record an oxidom-side message in the same book as xray's output, so the
    /// Logs view can explain a failure even when xray never started.
    ///
    /// Filed under [`LogSource::Oxidom`] rather than prefixed `"oxidom: "`: the
    /// source is now a field the view filters on, and a note that merely *read*
    /// as ours used to be indistinguishable from a core line that quoted us.
    pub fn note(&self, message: &str) {
        logbook::global().push(
            LogSource::Oxidom,
            Severity::Warn,
            Some(&self.profile),
            "oxidom::xray",
            message.to_string(),
        );
    }

    /// Mark where this attempt's output begins, and say so in the log.
    ///
    /// Called before anything else a connect does — including
    /// [`Self::preflight_notes`], whose warnings are part of what a failure is
    /// later explained by. Taking the watermark any later would leave those
    /// notes attributed to the previous attempt, and taking it any earlier
    /// would fold in the previous attempt's dying words.
    fn open_run(&mut self) {
        self.spawn_seq = logbook::global().next_seq();
        logbook::global().push(
            LogSource::Oxidom,
            Severity::Info,
            Some(&self.profile),
            "oxidom::xray",
            format!("starting the core for profile {}", self.profile),
        );
    }

    /// First sequence number of the current run. See [`Self::spawn_seq`].
    pub fn spawn_seq(&self) -> u64 {
        self.spawn_seq
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
    pub fn connect(
        &mut self,
        server: &Server,
        bind: Ipv4Addr,
        profile: &str,
        core: &ResolvedCore,
    ) -> Result<()> {
        self.disconnect();
        self.set_status(Status::Connecting);
        self.open_run();
        match self.try_connect(server, bind, profile, core) {
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
        core: &ResolvedCore,
    ) -> Result<()> {
        self.disconnect();
        self.set_status(Status::Connecting);
        self.open_run();
        match self.try_connect_pool(spec, bind, api_port, profile, core) {
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

    fn try_connect(
        &mut self,
        server: &Server,
        bind: Ipv4Addr,
        profile: &str,
        core: &ResolvedCore,
    ) -> Result<()> {
        // Resolve before checking ports: a busy port must not mask a missing core.
        let xray = self.resolve_binary()?;
        self.preflight_notes(server);
        self.ensure_ports_free(bind, None)?;
        let cfg = config::generate(server, bind, self.socks_port, self.http_port, core);
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
        core: &ResolvedCore,
    ) -> Result<()> {
        let xray = self.resolve_binary()?;
        for member in spec.members {
            self.preflight_notes(member);
        }
        self.ensure_ports_free(bind, Some(api_port))?;
        let cfg =
            config::generate_pool(spec, bind, self.socks_port, self.http_port, api_port, core)?;
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

        // Pump stdout and stderr into the log book.
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, LogSource::Xray, self.profile.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, LogSource::Xray, self.profile.clone());
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

    /// What the core itself said during **this** run, and nothing else.
    ///
    /// For callers that diagnose a failure by matching the core's own words:
    /// the book is shared, so it also holds other sessions' lines, the previous
    /// attempt's, and oxidom's own notes — one of which quotes an unknown
    /// obfuscation type in wording that [`UNSUPPORTED_PROTOCOL_MARKERS`] would
    /// match. Not a general log reader; the Logs view goes through the book.
    pub fn current_run_logs(&self) -> Vec<String> {
        logbook::global().texts_for(LogSource::Xray, Some(&self.profile), self.spawn_seq)
    }

    pub fn clear_logs(&self) {
        logbook::global().clear();
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
        let core = XrayCore::new("work".to_string(), 10808, 10809, String::new());

        let error = core
            .ensure_ports_free("127.77.1.1".parse().unwrap(), Some(10808))
            .unwrap_err()
            .to_string();

        assert!(error.contains("API and SOCKS endpoints cannot share port 10808"));
    }
}
