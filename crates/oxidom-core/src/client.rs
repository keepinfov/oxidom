//! Blocking D-Bus client for the oxidom daemon. All calls can block for the
//! duration of a daemon operation — never call these on the GTK main thread.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::ipc::{
    ApplySettingsResult, BUS_NAME, INTERFACE, OBJECT_PATH, ProbeState, ProfileEntry, RuntimeInfo,
    SessionInfo, StatusInfo, UpResult,
};
use crate::model::Subscription;
use crate::profile::Profile;

/// Ceiling on any single daemon call. zbus waits forever by default, and a
/// daemon that stops replying — wedged on its engine lock, killed mid-call —
/// would otherwise pin the UI's operation slot for the rest of the session,
/// refusing every later action with "another operation is still running".
/// Sized past the slowest legitimate call: `RefreshAll` fetches each
/// subscription in turn, each with its own 30s HTTP cap.
const METHOD_TIMEOUT: Duration = Duration::from_secs(300);

/// How long an installed-but-silent system daemon is waited for before the GUI
/// gives up and runs one of its own. The window it covers is the seconds
/// between a desktop session autostarting the GUI and the systemd unit claiming
/// its bus name; a daemon that is genuinely broken is not worth more than this.
const SYSTEM_DAEMON_GRACE: Duration = Duration::from_secs(10);
const SYSTEM_DAEMON_RETRY: Duration = Duration::from_millis(250);

/// Where a D-Bus system policy file for the daemon would be installed. Its
/// presence is what tells "no system daemon on this machine" (nothing to wait
/// for) apart from "the system daemon has not started yet" (wait for it).
const SYSTEM_POLICY_DIRS: [&str; 4] = [
    "/etc/dbus-1/system.d",
    "/usr/share/dbus-1/system.d",
    "/usr/local/share/dbus-1/system.d",
    // NixOS assembles the bus configuration out of the system profile.
    "/run/current-system/sw/share/dbus-1/system.d",
];

/// Which daemon the GUI ended up driving. A session daemon keeps its own
/// database under the user's XDG dirs, so this is not a detail: the two hold
/// different servers, and picking the wrong one silently is what makes a
/// subscription look like it lost half its entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonSource {
    /// The systemd service on the system bus.
    System,
    /// A daemon that was already running on the session bus.
    Session,
    /// A session daemon this process started.
    Spawned,
}

/// What [`DaemonClient::connect_any`] is doing, so the startup window can say
/// it. Text lives in the window; this only names the step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectStage {
    /// Asking the system bus.
    System,
    /// The system daemon is installed but has not claimed its name yet.
    WaitingForSystem,
    /// Asking the session bus.
    Session,
    /// Starting a private session daemon.
    Starting,
}

#[derive(Clone)]
pub struct DaemonClient {
    proxy: zbus::blocking::Proxy<'static>,
    source: DaemonSource,
}

fn friendly(error: zbus::Error) -> anyhow::Error {
    match error {
        zbus::Error::MethodError(_, Some(message), _) => anyhow::anyhow!(message),
        other => anyhow::anyhow!(other),
    }
}

/// `friendly`, plus the one diagnosis it cannot make on its own: a daemon that
/// predates profiles answers `UnknownMethod`, which as raw bus text tells the
/// user nothing about what to do.
fn profiles_unsupported(error: zbus::Error) -> anyhow::Error {
    if let zbus::Error::MethodError(name, _, _) = &error
        && name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod"
    {
        return anyhow::anyhow!(
            "this oxidom daemon is older than profiles and sessions; restart it to upgrade"
        );
    }
    friendly(error)
}

/// Is a system daemon installed at all? Only its D-Bus policy file can say:
/// without one the bus would refuse every call, so a machine that has it has a
/// system daemon, and a machine that does not never will.
fn system_daemon_installed() -> bool {
    policy_installed(&SYSTEM_POLICY_DIRS, BUS_NAME)
}

fn policy_installed(dirs: &[&str], bus_name: &str) -> bool {
    let file = format!("{bus_name}.conf");
    dirs.iter().any(|dir| Path::new(dir).join(&file).exists())
}

/// The CLI/daemon binary, which after the binary split is no longer this
/// process. `$OXIDOM_BIN` (set by the nix wrapper) wins, then a sibling named
/// `oxidom` next to the current executable, then `$PATH`.
fn daemon_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("OXIDOM_BIN").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(sibling) = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(|parent| parent.join("oxidom")))
        .filter(|path| path.is_file())
    {
        return sibling;
    }
    PathBuf::from("oxidom")
}

/// Does this error mean "nobody is on that name (yet)", as opposed to a final
/// answer? A daemon that is starting is worth waiting for; a bus that refuses
/// this user is not — for them a session daemon *is* the right daemon.
fn name_unowned(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, ..) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
                | "org.freedesktop.DBus.Error.NoReply"
                | "org.freedesktop.DBus.Error.Spawn.ChildSignaled"
                | "org.freedesktop.DBus.Error.Spawn.ChildExited"
        ),
        _ => false,
    }
}

impl DaemonClient {
    fn try_bus(system: bool, source: DaemonSource) -> zbus::Result<Self> {
        let builder = if system {
            zbus::blocking::connection::Builder::system()?
        } else {
            zbus::blocking::connection::Builder::session()?
        };
        let connection = builder.method_timeout(METHOD_TIMEOUT).build()?;
        let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)?;
        // Reject name owners that don't answer: a real daemon replies fast.
        let _: String = proxy.call("Status", &())?;
        Ok(DaemonClient { proxy, source })
    }

    /// Which daemon this client is driving.
    pub fn source(&self) -> DaemonSource {
        self.source
    }

    /// System bus first (the systemd service), then the session bus; as a
    /// last resort spawn a session daemon so the GUI works standalone.
    ///
    /// An installed system daemon is *waited for* rather than raced. The GUI is
    /// autostarted by the desktop session at the same moment systemd starts the
    /// unit, and losing that race by a second used to bind the whole session to
    /// a session daemon with a different database — the user's servers appeared
    /// to vanish, with nothing on screen saying which daemon was answering.
    /// (The packaged unit is D-Bus activatable, which closes the window on its
    /// own; this covers installations whose unit is not.)
    ///
    /// `progress` is called on the calling thread before each step, so a caller
    /// that runs this off the main loop can report what it is waiting on.
    pub fn connect_any(progress: impl Fn(ConnectStage)) -> Result<Self> {
        if let Some(client) = Self::find_existing(&progress, true) {
            return Ok(client);
        }

        progress(ConnectStage::Starting);
        let executable = daemon_binary();
        std::process::Command::new(&executable)
            .arg("daemon")
            .spawn()
            .with_context(|| format!("spawning a session daemon with {}", executable.display()))?;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(150));
            if let Ok(client) = Self::try_bus(false, DaemonSource::Spawned) {
                return Ok(client);
            }
        }
        bail!("could not reach or start the oxidom daemon")
    }

    /// Reach an already running daemon without starting a private session
    /// daemon. Read-only CLI commands use this so a status check never changes
    /// machine state merely by asking.
    pub fn connect_existing() -> Result<Self> {
        Self::find_existing(&|_| {}, false)
            .ok_or_else(|| anyhow::anyhow!("the oxidom daemon is not available"))
    }

    /// `patient` decides whether an installed-but-silent system daemon is worth
    /// waiting [`SYSTEM_DAEMON_GRACE`] for. Only a caller that would otherwise
    /// start a daemon of its own has that race to lose; a read-only CLI command
    /// must answer "nothing is running" now, not in ten seconds.
    fn find_existing(progress: &impl Fn(ConnectStage), patient: bool) -> Option<DaemonClient> {
        progress(ConnectStage::System);
        match Self::try_bus(true, DaemonSource::System) {
            Ok(client) => return Some(client),
            Err(error) if patient && name_unowned(&error) && system_daemon_installed() => {
                progress(ConnectStage::WaitingForSystem);
                let deadline = Instant::now() + SYSTEM_DAEMON_GRACE;
                while Instant::now() < deadline {
                    std::thread::sleep(SYSTEM_DAEMON_RETRY);
                    if let Ok(client) = Self::try_bus(true, DaemonSource::System) {
                        return Some(client);
                    }
                }
                log::warn!(
                    "the system daemon is installed but did not answer within {}s; \
                     falling back to a session daemon, which keeps its own subscriptions",
                    SYSTEM_DAEMON_GRACE.as_secs()
                );
            }
            Err(error) if patient => {
                log::info!("no daemon on the system bus ({error})");
            }
            Err(_) => {}
        }
        progress(ConnectStage::Session);
        Self::try_bus(false, DaemonSource::Session).ok()
    }

    pub fn subscriptions(&self) -> Result<Vec<Subscription>> {
        let json: String = self
            .proxy
            .call("ListSubscriptions", &())
            .map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn add_subscription(&self, url: &str, name: Option<&str>, send_hwid: bool) -> Result<()> {
        self.proxy
            .call("AddSubscription", &(url, name.unwrap_or(""), send_hwid))
            .map_err(friendly)
    }

    pub fn remove_subscription(&self, subscription_id: &str) -> Result<bool> {
        self.proxy
            .call("RemoveSubscription", &(subscription_id,))
            .map_err(friendly)
    }

    pub fn refresh(&self, subscription_id: &str) -> Result<()> {
        self.proxy
            .call("Refresh", &(subscription_id,))
            .map_err(friendly)
    }

    pub fn refresh_all(&self) -> Result<()> {
        self.proxy.call("RefreshAll", &()).map_err(friendly)
    }

    pub fn import_links(&self, text: &str) -> Result<(u32, u32)> {
        self.proxy.call("ImportLinks", &(text,)).map_err(friendly)
    }

    pub fn remove_server(&self, server_id: &str) -> Result<bool> {
        self.proxy
            .call("RemoveServer", &(server_id,))
            .map_err(friendly)
    }

    pub fn set_server_alias(&self, server_id: &str, alias: &str) -> Result<()> {
        self.proxy
            .call("SetServerAlias", &(server_id, alias))
            .map_err(friendly)
    }

    /// Profiles arrived after the first daemons shipped, so every call below
    /// can meet an `UnknownMethod` from a daemon that predates them. Say so in
    /// those words rather than passing the bus error through.
    pub fn list_profiles(&self) -> Result<Vec<ProfileEntry>> {
        let json: String = self
            .proxy
            .call("ListProfiles", &())
            .map_err(profiles_unsupported)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn profile(&self, name: &str) -> Result<Profile> {
        let json: String = self
            .proxy
            .call("GetProfile", &(name,))
            .map_err(profiles_unsupported)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn save_profile(&self, name: &str, profile: &Profile) -> Result<()> {
        let payload = serde_json::to_string(profile)?;
        self.proxy
            .call("SaveProfile", &(name, payload))
            .map_err(profiles_unsupported)
    }

    pub fn remove_profile(&self, name: &str) -> Result<bool> {
        self.proxy
            .call("RemoveProfile", &(name,))
            .map_err(profiles_unsupported)
    }

    pub fn up_profile(&self, name: &str) -> Result<UpResult> {
        let json: String = self
            .proxy
            .call("UpProfile", &(name,))
            .map_err(profiles_unsupported)?;
        Ok(serde_json::from_str(&json)?)
    }

    /// Bring the tunnel down. An empty `profile` stops it unconditionally;
    /// otherwise it stops only if that profile is the one that started it, and
    /// returns false when it is not.
    pub fn down(&self, profile: &str) -> Result<bool> {
        self.proxy
            .call("Down", &(profile,))
            .map_err(profiles_unsupported)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let json: String = self
            .proxy
            .call("ListSessions", &())
            .map_err(profiles_unsupported)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn session_status(&self, name: &str) -> Result<SessionInfo> {
        let json: String = self
            .proxy
            .call("SessionStatus", &(name,))
            .map_err(profiles_unsupported)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn down_profile(&self, name: &str) -> Result<bool> {
        self.proxy
            .call("DownProfile", &(name,))
            .map_err(profiles_unsupported)
    }

    pub fn set_hwid(&self, subscription_id: &str, enabled: bool) -> Result<()> {
        self.proxy
            .call("SetHwid", &(subscription_id, enabled))
            .map_err(friendly)
    }

    pub fn connect_server(&self, server_id: &str) -> Result<()> {
        self.proxy.call("Connect", &(server_id,)).map_err(friendly)
    }

    pub fn disconnect(&self) -> Result<()> {
        self.proxy.call("Disconnect", &()).map_err(friendly)
    }

    pub fn status(&self) -> Result<StatusInfo> {
        let json: String = self.proxy.call("Status", &()).map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn request_probe(&self, server_id: &str) -> Result<()> {
        self.proxy
            .call("RequestProbe", &(server_id,))
            .map_err(friendly)
    }

    pub fn request_probes(&self, server_ids: &[String]) -> Result<()> {
        self.proxy
            .call("RequestProbes", &(server_ids,))
            .map_err(friendly)
    }

    pub fn probe_state(&self) -> Result<ProbeState> {
        let json: String = self.proxy.call("ProbeState", &()).map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    /// Daemon-side facts (resolved Xray path, unit-pinned ports). A daemon
    /// older than this method answers `UnknownMethod`; callers must degrade
    /// gracefully rather than treat that as fatal.
    pub fn runtime_info(&self) -> Result<RuntimeInfo> {
        let json: String = self.proxy.call("RuntimeInfo", &()).map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn settings(&self) -> Result<Config> {
        let json: String = self.proxy.call("GetSettings", &()).map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn apply_settings(&self, config: &Config) -> Result<ApplySettingsResult> {
        let payload = serde_json::to_string(config)?;
        let json: String = self
            .proxy
            .call("SetSettings", &(payload,))
            .map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn recent_logs(&self) -> Result<Vec<String>> {
        self.proxy.call("RecentLogs", &()).map_err(friendly)
    }

    pub fn clear_logs(&self) -> Result<()> {
        self.proxy.call("ClearLogs", &()).map_err(friendly)
    }
}

#[cfg(test)]
mod tests {
    use super::{BUS_NAME, daemon_binary, policy_installed};

    #[test]
    fn daemon_binary_prefers_the_environment() {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let previous = std::env::var_os("OXIDOM_BIN");
        let configured = std::path::PathBuf::from("/configured/oxidom");
        // SAFETY: all tests that mutate oxidom's process environment use the
        // same mutex, so no other such test can read or write it concurrently.
        unsafe {
            std::env::set_var("OXIDOM_BIN", &configured);
        }
        let resolved = daemon_binary();
        // SAFETY: the same process-wide test mutex remains held.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("OXIDOM_BIN", value),
                None => std::env::remove_var("OXIDOM_BIN"),
            }
        }

        assert_eq!(resolved, configured);
    }

    /// The predicate that decides whether an unanswered system bus is worth
    /// waiting out. Getting it wrong either way is a real cost: a false
    /// positive stalls every start on a machine with no system daemon, a false
    /// negative puts the GUI back on a private database whenever it wins the
    /// race with the unit.
    #[test]
    fn only_an_installed_policy_makes_the_system_daemon_worth_waiting_for() {
        let dir = std::env::temp_dir().join(format!("oxidom-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dirs = [dir.to_str().unwrap()];

        assert!(!policy_installed(&dirs, BUS_NAME));
        // A policy for some *other* bus name in the same directory is not ours.
        std::fs::write(dir.join("org.example.Other.conf"), b"").unwrap();
        assert!(!policy_installed(&dirs, BUS_NAME));

        std::fs::write(dir.join(format!("{BUS_NAME}.conf")), b"").unwrap();
        assert!(policy_installed(&dirs, BUS_NAME));
        // A directory that does not exist at all must not panic or match.
        assert!(!policy_installed(
            &["/nonexistent/dbus-1/system.d"],
            BUS_NAME
        ));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
