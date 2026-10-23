//! Blocking D-Bus client for the oxidom daemon. All calls can block for the
//! duration of a daemon operation — never call these on the GTK main thread.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::time::Instant;

use crate::config::Config;
use crate::ipc::{
    ApplySettingsResult, BUS_NAME, CreatedServer, INTERFACE, OBJECT_PATH, ProbeHistory, ProbeState,
    ProfileEntry, RuntimeInfo, SessionInfo, StatusInfo, UpResult,
};
use crate::model::Subscription;
use crate::profile::Profile;
use crate::run::CgroupSlice;

mod session_daemon;

/// Ordinary per-reply limit, separate from discovery's end-to-end budgets.
/// This is not a total operation deadline: RefreshAll can queue and fetch many
/// subscriptions sequentially, and even sending a call precedes its reply timer.
const METHOD_TIMEOUT: Duration = Duration::from_secs(300);

/// Discovery includes connection, authentication, Hello, proxy setup and Status.
/// An unanswered attempt is indeterminate, never permission to switch databases.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
/// The entire patient system phase, including its first attempt and retries.
const SYSTEM_DAEMON_GRACE: Duration = Duration::from_secs(10);
const SYSTEM_DAEMON_RETRY: Duration = Duration::from_millis(250);
const SESSION_STARTUP: Duration = Duration::from_secs(6);
const SESSION_RETRY: Duration = Duration::from_millis(150);
const SESSION_EXIT_GRACE: Duration = Duration::from_secs(2);

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
                | "org.freedesktop.DBus.Error.Spawn.ChildSignaled"
                | "org.freedesktop.DBus.Error.Spawn.ChildExited"
        ),
        zbus::Error::FDO(error) => matches!(
            error.as_ref(),
            zbus::fdo::Error::ServiceUnknown(_)
                | zbus::fdo::Error::NameHasNoOwner(_)
                | zbus::fdo::Error::SpawnChildSignaled(_)
                | zbus::fdo::Error::SpawnChildExited(_)
        ),
        _ => false,
    }
}

fn bus_unavailable(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, ..) => {
            name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"
        }
        zbus::Error::FDO(error) => matches!(error.as_ref(), zbus::fdo::Error::AccessDenied(_)),
        zbus::Error::InputOutput(error) => matches!(
            error.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::PermissionDenied
        ),
        _ => false,
    }
}

fn block_discovery<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    // Connections retain reactor tasks after discovery returns. A temporary
    // runtime would strand them; avoid relying on zbus's hidden block_on API.
    static RUNTIME: OnceLock<std::io::Result<tokio::runtime::Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                // zbus 5.18 uses a blocking Unix connect that may outlive our
                // cancelled future (e.g. a full socket backlog). Its Task drop
                // aborts queued work, but cannot stop a running connect. Bound
                // those uncancellable connects to one on this persistent runtime.
                .max_blocking_threads(1)
                .enable_all()
                .build()
        })
        .as_ref()
        .map_err(|_| anyhow!("could not create the daemon discovery runtime"))?
        .block_on(future)
}

async fn before_deadline<T>(
    deadline: Instant,
    future: impl Future<Output = zbus::Result<T>>,
) -> zbus::Result<T> {
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(deadline) => {
            Err(zbus::fdo::Error::TimedOut("daemon discovery deadline expired".into()).into())
        }
        result = future, if Instant::now() < deadline => result,
    }
}

async fn find_existing<T, F, Fut>(
    progress: &impl Fn(ConnectStage),
    patient: bool,
    installed: bool,
    attempt: &mut F,
) -> Result<Option<(T, DaemonSource)>>
where
    F: FnMut(DaemonSource) -> Fut,
    Fut: Future<Output = zbus::Result<T>>,
{
    let deadline = Instant::now()
        + if patient {
            SYSTEM_DAEMON_GRACE
        } else {
            DISCOVERY_TIMEOUT
        };
    progress(ConnectStage::System);
    let mut waiting = false;
    loop {
        if Instant::now() >= deadline {
            if !waiting {
                bail!("system daemon discovery timed out; refusing to switch databases");
            }
            // A pending attempt expires as an error below. Only confirmed
            // absence permits fallback at the end of installed grace.
            log::warn!(
                "the installed system daemon remained absent through its startup grace; \
                 falling back to a session daemon, which keeps its own subscriptions"
            );
            break;
        }
        match before_deadline(deadline, attempt(DaemonSource::System)).await {
            Ok(client) => return Ok(Some((client, DaemonSource::System))),
            Err(error) if name_unowned(&error) => {
                if !patient || !installed {
                    break;
                }
                if !waiting {
                    waiting = true;
                    progress(ConnectStage::WaitingForSystem);
                }
                let now = Instant::now();
                if now < deadline {
                    tokio::time::sleep_until((now + SYSTEM_DAEMON_RETRY).min(deadline)).await;
                }
            }
            Err(error) if bus_unavailable(&error) => break,
            Err(error) => {
                return Err(anyhow!(error).context(
                    "could not determine system daemon readiness; refusing to switch databases",
                ));
            }
        }
    }
    progress(ConnectStage::Session);
    match before_deadline(
        Instant::now() + DISCOVERY_TIMEOUT,
        attempt(DaemonSource::Session),
    )
    .await
    {
        Ok(client) => Ok(Some((client, DaemonSource::Session))),
        Err(error) if name_unowned(&error) || bus_unavailable(&error) => Ok(None),
        Err(error) => Err(anyhow!(error).context("could not determine session daemon readiness")),
    }
}

async fn wait_spawned<T, F, Fut>(
    deadline: Instant,
    attempt: &mut F,
    mut check_child: impl FnMut() -> Result<()>,
) -> Result<T>
where
    F: FnMut(DaemonSource) -> Fut,
    Fut: Future<Output = zbus::Result<T>>,
{
    let readiness = async {
        loop {
            if Instant::now() >= deadline {
                bail!("timed out waiting for the session daemon to become ready");
            }
            match before_deadline(deadline, attempt(DaemonSource::Spawned)).await {
                Ok(client) => return Ok(client),
                Err(error) if name_unowned(&error) => {}
                Err(error) => {
                    return Err(
                        anyhow!(error).context("could not reach the started session daemon")
                    );
                }
            }
            let now = Instant::now();
            if now < deadline {
                tokio::time::sleep_until((now + SESSION_RETRY).min(deadline)).await;
            }
        }
    };
    let child_exit = async {
        loop {
            if let Err(error) = check_child() {
                return error;
            }
            if Instant::now() >= deadline {
                return anyhow!("timed out waiting for the session daemon to become ready");
            }
            let now = Instant::now();
            if now < deadline {
                tokio::time::sleep_until((now + SESSION_RETRY).min(deadline)).await;
            }
        }
    };
    tokio::pin!(readiness);
    let exit_error = tokio::select! {
        biased;
        error = child_exit => error,
        result = &mut readiness => return result,
    };
    // Another client may have won the store lock. Keep the in-flight readiness
    // and absence retries on this same session bus, without spawning again.
    let final_deadline = deadline.min(Instant::now() + SESSION_EXIT_GRACE);
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(final_deadline) => Err(exit_error),
        result = readiness, if Instant::now() < final_deadline => result.or(Err(exit_error)),
    }
}

impl DaemonClient {
    async fn try_bus(source: DaemonSource) -> zbus::Result<zbus::Proxy<'static>> {
        let builder = if source == DaemonSource::System {
            zbus::connection::Builder::system()?
        } else {
            zbus::connection::Builder::session()?
        };
        let connection = builder.method_timeout(METHOD_TIMEOUT).build().await?;
        let proxy = zbus::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE).await?;
        // Reject name owners that don't answer: a real daemon replies fast.
        let _: String = proxy.call("Status", &()).await?;
        Ok(proxy)
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
    /// `OXIDOM_SESSION_DAEMON=external` leaves launch ownership to an external
    /// supervisor; unset, empty and `standalone` retain private bootstrap.
    pub fn connect_any(progress: impl Fn(ConnectStage)) -> Result<Self> {
        let owner = session_daemon::LaunchOwner::parse(
            std::env::var_os("OXIDOM_SESSION_DAEMON").as_deref(),
        )?;
        let existing = block_discovery(find_existing(
            &progress,
            true,
            system_daemon_installed(),
            &mut Self::try_bus,
        ))?;
        let (proxy, source) = session_daemon::connect_or_launch(existing, owner, || {
            block_discovery(async {
                progress(ConnectStage::Starting);
                let deadline = Instant::now() + SESSION_STARTUP;
                let executable = daemon_binary();
                let mut daemon = session_daemon::SessionDaemon::spawn(
                    std::process::Command::new(&executable).arg("daemon"),
                )
                .context("spawning a session daemon")?;
                let proxy = wait_spawned(deadline, &mut Self::try_bus, || {
                    if let Some(status) = daemon
                        .child
                        .as_mut()
                        .context("session daemon child is missing")?
                        .try_wait()
                        .context("checking the session daemon")?
                    {
                        bail!("the session daemon exited before becoming ready ({status})");
                    }
                    Ok(())
                })
                .await?;
                Ok((proxy, DaemonSource::Spawned))
            })
        })?;
        Ok(Self {
            proxy: proxy.into(),
            source,
        })
    }

    /// Reach an already running daemon without starting a private session
    /// daemon. Read-only CLI commands use this so a status check never changes
    /// machine state merely by asking.
    pub fn connect_existing() -> Result<Self> {
        let (proxy, source) =
            block_discovery(find_existing(&|_| {}, false, false, &mut Self::try_bus))?
                .ok_or_else(|| anyhow!("the oxidom daemon is not available"))?;
        Ok(Self {
            proxy: proxy.into(),
            source,
        })
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

    /// Create a server described as fields rather than as a share link. The
    /// draft travels as JSON; the daemon validates and answers with what it
    /// stored. A daemon older than this method answers `UnknownMethod`, which
    /// as raw bus text tells the user nothing about what to do.
    pub fn create_server(&self, draft: &crate::draft::ServerDraft) -> Result<CreatedServer> {
        let draft_json = serde_json::to_string(draft)?;
        let json: String = self
            .proxy
            .call("CreateServer", &(draft_json.as_str(),))
            .map_err(|error| unknown_method(error, "create servers by hand"))?;
        serde_json::from_str(&json).map_err(Into::into)
    }

    /// Edit one stored server through the same validator a draft passes.
    /// The draft travels as JSON, like `CreateServer`; a daemon older than
    /// the method answers `UnknownMethod`.
    pub fn update_server(
        &self,
        server_id: &str,
        draft: &crate::draft::ServerDraft,
    ) -> Result<CreatedServer> {
        let draft_json = serde_json::to_string(draft)?;
        let json: String = self
            .proxy
            .call("UpdateServer", &(server_id, draft_json.as_str()))
            .map_err(|error| unknown_method(error, "edit servers"))?;
        serde_json::from_str(&json).map_err(Into::into)
    }

    /// Drop one override and take back what the provider last sent.
    pub fn drop_override(&self, server_id: &str, field: &str) -> Result<CreatedServer> {
        let json: String = self
            .proxy
            .call("DropOverride", &(server_id, field))
            .map_err(|error| unknown_method(error, "edit servers"))?;
        serde_json::from_str(&json).map_err(Into::into)
    }

    /// The certificate this server presents, as a SHA-256 hex string. Reads
    /// it; decides nothing.
    pub fn inspect_certificate(&self, server_id: &str) -> Result<String> {
        self.proxy
            .call("InspectCertificate", &(server_id,))
            .map_err(friendly)
    }

    /// Accept exactly this certificate for this server from now on. Pass the
    /// fingerprint that was shown, not one fetched again — the difference is
    /// the whole point of a pin.
    pub fn trust_certificate(&self, server_id: &str, sha256: &str) -> Result<()> {
        self.proxy
            .call("TrustCertificate", &(server_id, sha256))
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

    pub fn delete_interface(&self, name: &str) -> Result<bool> {
        self.proxy
            .call("DeleteInterface", &(name,))
            .map_err(profiles_unsupported)
    }

    pub fn mark_cgroup(&self, name: &str, uid: u32) -> Result<CgroupSlice> {
        let json: String = self
            .proxy
            .call("MarkCgroup", &(name, uid))
            .map_err(profiles_unsupported)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn set_hwid(&self, subscription_id: &str, enabled: bool) -> Result<()> {
        self.proxy
            .call("SetHwid", &(subscription_id, enabled))
            .map_err(friendly)
    }

    /// Override the User-Agent for one subscription. An empty `user_agent`
    /// clears the override so the global config value applies again — D-Bus has
    /// no `Option`, and "empty means inherit" is how the rest of the config
    /// spells the same thing.
    pub fn set_subscription_user_agent(
        &self,
        subscription_id: &str,
        user_agent: &str,
    ) -> Result<()> {
        self.proxy
            .call("SetSubscriptionUserAgent", &(subscription_id, user_agent))
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

    /// Call off queued checks for these servers, and report how many were
    /// dropped. Not an error when nothing is queued for them.
    ///
    /// A check already measuring is not interrupted — at most eight are, and
    /// they end within about ten seconds. What this drops is the queue behind
    /// them. Each cancelled server leaves a reading marked
    /// [`ProbeDetail::Cancelled`], so a client retires its spinner the same way
    /// it does for a measured one.
    ///
    /// A daemon older than this method answers `UnknownMethod`; callers must
    /// degrade rather than treat it as fatal.
    ///
    /// [`ProbeDetail::Cancelled`]: crate::ipc::ProbeDetail::Cancelled
    pub fn cancel_probes(&self, server_ids: &[String]) -> Result<usize> {
        let json: String = self
            .proxy
            .call("CancelProbes", &(server_ids,))
            .map_err(friendly)?;
        let answer: serde_json::Value = serde_json::from_str(&json)?;
        Ok(answer
            .get("cancelled")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize)
    }

    /// Whether this daemon knows how to call off a check at all.
    ///
    /// Distinguishes "asked and there was nothing queued" from "too old to
    /// ask", which a client needs in order to offer a stop control rather than
    /// one that cannot work. Asked with an empty list, so the question costs
    /// nothing and cancels nothing.
    pub fn supports_probe_cancel(&self) -> bool {
        let empty: Vec<String> = Vec::new();
        match self.proxy.call::<_, _, String>("CancelProbes", &(&empty,)) {
            Ok(_) => true,
            Err(error) => !is_unknown_method(&error),
        }
    }

    /// What this server's recent checks measured, newest first.
    ///
    /// Asked for one server at a time and only when something is about to show
    /// it, because this is the half of the probe state that does not belong on
    /// the poll: `ProbeState` carries one reading for every server twice a
    /// second, and the history is ten deep.
    ///
    /// A daemon too old to know the method answers `UnknownMethod`, which is
    /// reported here as an empty history rather than as an error. That is the
    /// truth from the caller's side — such a daemon kept no history to give —
    /// and it lets the card fall back to the single reading it already has
    /// instead of raising a failure over a panel.
    pub fn probe_history(&self, server_id: &str) -> Result<ProbeHistory> {
        let json: String = match self.proxy.call("ProbeHistory", &(server_id,)) {
            Ok(json) => json,
            Err(error) if is_unknown_method(&error) => return Ok(ProbeHistory::default()),
            Err(error) => return Err(friendly(error)),
        };
        Ok(serde_json::from_str(&json)?)
    }

    /// Run a selection now, writing no profile. Returns the session's name.
    ///
    /// A daemon too old to know the verb answers `UnknownMethod`, which is not
    /// a sentence anyone can act on — so it is translated here, next to the
    /// method it belongs to, the way `profiles_unsupported` already is.
    pub fn connect_pool(&self, query: &crate::pool::PoolQuery) -> Result<String> {
        let payload = serde_json::to_string(query)?;
        self.proxy
            .call("ConnectPool", &(payload.as_str(),))
            .map_err(|error| {
                if is_unknown_method(&error) {
                    anyhow!(
                        "this daemon is too old to run a group without saving it first; \
                         update oxidom, or save the group to a profile and connect that"
                    )
                } else {
                    friendly(error)
                }
            })
    }

    pub fn connect_pool_in_session(
        &self,
        session: &str,
        query: &crate::pool::PoolQuery,
    ) -> Result<String> {
        let mut query = query.clone();
        query.session = session.to_string();
        self.connect_pool(&query)
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

    /// Ask the daemon to install the geo data its core needs.
    ///
    /// Returns when the download has *started*, not when it has finished: the
    /// files are tens of megabytes and the daemon answers other callers on the
    /// same connection meanwhile. Watch [`Self::runtime_info`] for progress —
    /// its `geo` block carries `downloading`, the current file and the byte
    /// counts — and call [`Self::cancel_geo_download`] to stop.
    ///
    /// `through_tunnel` asks for the fetch to go over a tunnel that is already
    /// up, for a network where the release host is blocked. The daemon chooses
    /// the proxy from its own sessions.
    ///
    /// A daemon older than this method answers `UnknownMethod`; callers must
    /// degrade rather than treat it as fatal.
    pub fn download_geo_assets(&self, through_tunnel: bool) -> Result<()> {
        self.proxy
            .call::<_, _, String>("DownloadGeoAssets", &(through_tunnel,))
            .map_err(friendly)?;
        Ok(())
    }

    /// Stop a running geo data download. Not an error when none is running.
    pub fn cancel_geo_download(&self) -> Result<()> {
        self.proxy
            .call::<_, _, String>("CancelGeoDownload", &())
            .map_err(friendly)?;
        Ok(())
    }

    /// Directories on the daemon's machine already holding geo data its core
    /// accepts. Every entry has been offered to the core and taken, so this is
    /// a list of what is usable rather than of what exists.
    pub fn find_geo_assets(&self) -> Result<Vec<crate::xray::assets::Candidate>> {
        let json: String = self.proxy.call("FindGeoAssets", &()).map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    /// Install the geo data in `dir` instead of downloading it. The daemon
    /// re-checks the directory before copying anything out of it.
    pub fn adopt_geo_assets(&self, dir: &str) -> Result<()> {
        self.proxy
            .call::<_, _, String>("AdoptGeoAssets", &(dir,))
            .map_err(friendly)?;
        Ok(())
    }

    /// Whether this daemon knows how to install geo data at all.
    ///
    /// Distinguishes "asked and refused" from "too old to ask", which the GUI
    /// needs in order to offer a manual recipe rather than a button that cannot
    /// work.
    pub fn supports_geo_download(&self) -> bool {
        match self.proxy.call::<_, _, String>("CancelGeoDownload", &()) {
            Ok(_) => true,
            Err(error) => !is_unknown_method(&error),
        }
    }

    pub fn settings(&self) -> Result<Config> {
        self.settings_with_watchdog_support()
            .map(|(config, _)| config)
    }

    /// Old daemons implement aggressive checks, not the new on-disk default.
    pub fn settings_with_watchdog_support(&self) -> Result<(Config, bool)> {
        let json: String = self.proxy.call("GetSettings", &()).map_err(friendly)?;
        Self::decode_settings(&json)
    }

    fn decode_settings(json: &str) -> Result<(Config, bool)> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        let supported = value.get("watchdog").is_some();
        let mut config: Config = serde_json::from_value(value)?;
        if !supported {
            config.watchdog = crate::config::WatchdogMode::Aggressive;
        }
        Ok((config, supported))
    }

    pub fn apply_settings(&self, config: &Config) -> Result<ApplySettingsResult> {
        let mut value = serde_json::to_value(config)?;
        // An untouched `[core]` is left out of the file, and that same
        // `skip_serializing_if` leaves it out of this payload — where the
        // daemon reads an absent key as "keep what you have", so that a client
        // too old to know the section cannot erase it. A client that does edit
        // the section has to say so even when it ends up empty, or turning the
        // last core setting off would never reach the daemon.
        if let Some(object) = value.as_object_mut() {
            object
                .entry("core")
                .or_insert_with(|| serde_json::json!({}));
        }
        let payload = serde_json::to_string(&value)?;
        let json: String = self
            .proxy
            .call("SetSettings", &(payload,))
            .map_err(friendly)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn recent_logs(&self) -> Result<Vec<String>> {
        self.proxy.call("RecentLogs", &()).map_err(friendly)
    }

    /// Structured records after `after_seq`.
    ///
    /// Falls back to `RecentLogs` against a daemon that predates the cursor,
    /// synthesising a slice from the flat lines it returns. That daemon reports
    /// no sequence numbers, so the fallback carries `book_id: 0` and a
    /// `next_seq` of zero, which keeps the caller redrawing exactly as it had to
    /// before — degraded, but never blank.
    pub fn logs_since(&self, after_seq: u64, limit: u32) -> Result<crate::logbook::LogSlice> {
        match self
            .proxy
            .call::<_, _, String>("LogsSince", &(after_seq, limit))
        {
            Ok(json) => Ok(serde_json::from_str(&json)?),
            Err(error) if is_unknown_method(&error) => {
                let lines = self.recent_logs()?;
                Ok(crate::logbook::LogSlice::from_legacy_lines(lines))
            }
            Err(error) => Err(friendly(error)),
        }
    }

    pub fn clear_logs(&self) -> Result<()> {
        self.proxy.call("ClearLogs", &()).map_err(friendly)
    }
}

/// Does this daemon simply not have the method we called? Turned into a
/// sentence that says what to do about it, for the methods a daemon older
/// than the feature cannot answer.
fn unknown_method(error: zbus::Error, what: &str) -> anyhow::Error {
    if let zbus::Error::MethodError(name, _, _) = &error
        && name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod"
    {
        return anyhow::anyhow!("this oxidom daemon is too old to {what}; restart it to upgrade");
    }
    friendly(error)
}

/// Does this daemon simply not have the method we called?
fn is_unknown_method(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::MethodError(name, _, _)
        if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod")
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_unanswered_status_does_not_authorize_database_fallback() {
        let call = zbus::Message::method_call(super::OBJECT_PATH, "Status")
            .unwrap()
            .build(&())
            .unwrap();
        let error = zbus::Message::error(&call.header(), "org.freedesktop.DBus.Error.NoReply")
            .unwrap()
            .build(&"invented unanswered daemon")
            .unwrap()
            .into();
        assert!(!super::name_unowned(&error));
    }

    use std::cell::{Cell, RefCell};
    use std::future::{pending, ready};

    use super::{
        ConnectStage, DISCOVERY_TIMEOUT, DaemonSource, SESSION_EXIT_GRACE, SESSION_RETRY,
        SESSION_STARTUP, SYSTEM_DAEMON_GRACE, before_deadline, find_existing, name_unowned,
        wait_spawned,
    };
    use tokio::time::Instant;

    fn absent() -> zbus::Error {
        zbus::fdo::Error::ServiceUnknown("invented missing daemon".into()).into()
    }

    fn method_error(name: &str) -> zbus::Error {
        let call = zbus::Message::method_call(super::OBJECT_PATH, "Status")
            .unwrap()
            .build(&())
            .unwrap();
        zbus::Message::error(&call.header(), name)
            .unwrap()
            .build(&"invented discovery error")
            .unwrap()
            .into()
    }

    #[tokio::test(start_paused = true)]
    async fn the_outer_deadline_drops_a_pending_async_setup_future() {
        struct OnDrop<'a>(&'a Cell<bool>);
        impl Drop for OnDrop<'_> {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let dropped = Cell::new(false);
        let start = Instant::now();
        let guard = OnDrop(&dropped);
        let result = before_deadline(start + DISCOVERY_TIMEOUT, async move {
            let _guard = guard;
            pending::<zbus::Result<()>>().await
        })
        .await;
        assert!(result.is_err());
        assert!(dropped.get());
        assert_eq!(start.elapsed(), DISCOVERY_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_deadline_never_polls_another_setup_future() {
        let polled = Cell::new(false);
        let result = before_deadline(Instant::now(), async {
            polled.set(true);
            Ok(())
        })
        .await;
        assert!(result.is_err());
        assert!(!polled.get());
    }

    #[tokio::test(start_paused = true)]
    async fn system_retries_share_the_grace_with_the_first_attempt() {
        let start = Instant::now();
        let calls = Cell::new(0);
        let stages = RefCell::new(Vec::new());
        let result = find_existing(
            &|stage| stages.borrow_mut().push(stage),
            true,
            true,
            &mut |source| {
                let call = calls.get();
                calls.set(call + 1);
                async move {
                    if source == DaemonSource::Session {
                        return Ok(());
                    }
                    assert!(Instant::now() < start + SYSTEM_DAEMON_GRACE);
                    if call == 0 {
                        tokio::time::advance(std::time::Duration::from_secs(7)).await;
                    }
                    Err(absent())
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(result, Some(((), DaemonSource::Session)));
        assert_eq!(start.elapsed(), SYSTEM_DAEMON_GRACE);
        assert_eq!(calls.get(), 13);
        assert_eq!(
            *stages.borrow(),
            [
                ConnectStage::System,
                ConnectStage::WaitingForSystem,
                ConnectStage::Session
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_last_system_attempt_that_times_out_never_selects_another_database() {
        let calls = Cell::new(0);
        let start = Instant::now();
        let result = find_existing(&|_| {}, true, true, &mut |source| {
            assert_eq!(source, DaemonSource::System);
            let call = calls.get();
            calls.set(call + 1);
            async move {
                if call == 0 {
                    Err(absent())
                } else {
                    pending::<zbus::Result<()>>().await
                }
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.get(), 2);
        assert_eq!(start.elapsed(), SYSTEM_DAEMON_GRACE);
    }

    #[tokio::test(start_paused = true)]
    async fn no_reply_and_bus_timeouts_are_not_evidence_of_absence() {
        let errors: [zbus::Error; 5] = [
            method_error("org.freedesktop.DBus.Error.NoReply"),
            zbus::fdo::Error::NoReply("invented timeout".into()).into(),
            zbus::fdo::Error::Timeout("invented timeout".into()).into(),
            zbus::fdo::Error::TimedOut("invented timeout".into()).into(),
            std::io::Error::from(std::io::ErrorKind::TimedOut).into(),
        ];
        for error in errors {
            assert!(!name_unowned(&error));
            let result = find_existing(&|_| {}, true, true, &mut |source| {
                assert_eq!(source, DaemonSource::System);
                ready(Err::<(), _>(error.clone()))
            })
            .await;
            assert!(result.is_err());

            let result = find_existing(&|_| {}, false, false, &mut |source| {
                ready(Err::<(), _>(if source == DaemonSource::System {
                    absent()
                } else {
                    error.clone()
                }))
            })
            .await;
            assert!(result.is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn read_only_discovery_bounds_each_bus_without_entering_the_spawn_phase() {
        let start = Instant::now();
        let stages = RefCell::new(Vec::new());
        let result = find_existing(
            &|stage| stages.borrow_mut().push(stage),
            false,
            true,
            &mut |_| pending::<zbus::Result<()>>(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(start.elapsed(), DISCOVERY_TIMEOUT);
        assert_eq!(*stages.borrow(), [ConnectStage::System]);

        let start = Instant::now();
        let result = find_existing(&|_| {}, false, true, &mut |source| async move {
            if source == DaemonSource::System {
                Err(absent())
            } else {
                pending::<zbus::Result<()>>().await
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(start.elapsed(), DISCOVERY_TIMEOUT);

        let result = find_existing(
            &|stage| assert_ne!(stage, ConnectStage::Starting),
            false,
            true,
            &mut |_| ready(Err::<(), _>(absent())),
        )
        .await
        .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_responsive_system_daemon_is_always_preferred() {
        let result = find_existing(&|_| {}, true, true, &mut |source| {
            assert_eq!(source, DaemonSource::System);
            ready(Ok(()))
        })
        .await
        .unwrap();
        assert_eq!(result, Some(((), DaemonSource::System)));
    }

    #[tokio::test(start_paused = true)]
    async fn access_denied_and_an_absent_bus_allow_session_fallback_without_grace() {
        let errors: [zbus::Error; 4] = [
            method_error("org.freedesktop.DBus.Error.AccessDenied"),
            zbus::fdo::Error::AccessDenied("invented denial".into()).into(),
            std::io::Error::from(std::io::ErrorKind::NotFound).into(),
            std::io::Error::from(std::io::ErrorKind::ConnectionRefused).into(),
        ];
        for error in errors {
            let start = Instant::now();
            let result = find_existing(&|_| {}, true, true, &mut |source| {
                ready(if source == DaemonSource::System {
                    Err(error.clone())
                } else {
                    Ok(())
                })
            })
            .await
            .unwrap();
            assert_eq!(result, Some(((), DaemonSource::Session)));
            assert_eq!(start.elapsed(), std::time::Duration::ZERO);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn activation_child_failures_can_recover_within_installed_system_grace() {
        let errors: [zbus::Error; 6] = [
            method_error("org.freedesktop.DBus.Error.ServiceUnknown"),
            method_error("org.freedesktop.DBus.Error.NameHasNoOwner"),
            method_error("org.freedesktop.DBus.Error.Spawn.ChildExited"),
            method_error("org.freedesktop.DBus.Error.Spawn.ChildSignaled"),
            zbus::fdo::Error::SpawnChildExited("invented exit".into()).into(),
            zbus::fdo::Error::SpawnChildSignaled("invented signal".into()).into(),
        ];
        for error in errors {
            let calls = Cell::new(0);
            let result = find_existing(&|_| {}, true, true, &mut |source| {
                assert_eq!(source, DaemonSource::System);
                calls.set(calls.get() + 1);
                ready(if calls.get() == 1 {
                    Err(error.clone())
                } else {
                    Ok(())
                })
            })
            .await
            .unwrap();
            assert_eq!(result, Some(((), DaemonSource::System)));
            assert_eq!(calls.get(), 2);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn spawned_session_retries_never_start_at_or_after_the_startup_deadline() {
        let start = Instant::now();
        let calls = Cell::new(0);
        let result = wait_spawned(
            start + SESSION_STARTUP,
            &mut |source| {
                assert_eq!(source, DaemonSource::Spawned);
                assert!(Instant::now() < start + SESSION_STARTUP);
                calls.set(calls.get() + 1);
                ready(Err::<(), _>(absent()))
            },
            || Ok(()),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(start.elapsed(), SESSION_STARTUP);
        assert_eq!(calls.get(), 40);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_spawned_session_setup_expires_within_the_startup_budget() {
        let start = Instant::now();
        let result = wait_spawned(
            start + SESSION_STARTUP,
            &mut |_| pending::<zbus::Result<()>>(),
            || Ok(()),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(start.elapsed(), SESSION_STARTUP);
    }

    #[tokio::test(start_paused = true)]
    async fn a_spawned_child_exit_is_reported_after_a_short_same_bus_readiness_grace() {
        let start = Instant::now();
        let checks = Cell::new(0);
        let result = wait_spawned(
            start + SESSION_STARTUP,
            &mut |_| pending::<zbus::Result<()>>(),
            || {
                checks.set(checks.get() + 1);
                if checks.get() == 2 {
                    anyhow::bail!(
                        "the session daemon exited before becoming ready (exit status: 23)"
                    );
                }
                Ok(())
            },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("exit status: 23"));
        assert_eq!(checks.get(), 2);
        assert_eq!(start.elapsed(), SESSION_RETRY + SESSION_EXIT_GRACE);
    }

    #[tokio::test(start_paused = true)]
    async fn a_losing_child_exit_does_not_override_pending_winner_readiness() {
        let start = Instant::now();
        let checks = Cell::new(0);
        let attempts = Cell::new(0);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut sender = Some(sender);
        let mut receiver = Some(receiver);
        let result = wait_spawned(
            start + SESSION_STARTUP,
            &mut |source| {
                assert_eq!(source, DaemonSource::Spawned);
                attempts.set(attempts.get() + 1);
                let receiver = receiver.take().unwrap();
                async move {
                    receiver.await.unwrap();
                    Ok(())
                }
            },
            || {
                checks.set(checks.get() + 1);
                if checks.get() == 2 {
                    sender.take().unwrap().send(()).unwrap();
                    anyhow::bail!("the losing session daemon exited (exit status: 23)");
                }
                Ok(())
            },
        )
        .await;
        result.unwrap();
        assert_eq!(attempts.get(), 1);
        assert_eq!(checks.get(), 2);
        assert_eq!(start.elapsed(), SESSION_RETRY);
    }

    #[tokio::test(start_paused = true)]
    async fn a_losing_child_keeps_retrying_the_same_session_bus_for_the_winner() {
        let start = Instant::now();
        let attempts = Cell::new(0);
        let result = wait_spawned(
            start + SESSION_STARTUP,
            &mut |source| {
                assert_eq!(source, DaemonSource::Spawned);
                attempts.set(attempts.get() + 1);
                ready(if attempts.get() == 1 {
                    Err(absent())
                } else {
                    Ok(())
                })
            },
            || anyhow::bail!("the losing session daemon exited (exit status: 23)"),
        )
        .await;
        result.unwrap();
        assert_eq!(attempts.get(), 2);
        assert_eq!(start.elapsed(), SESSION_RETRY);
    }

    #[tokio::test(start_paused = true)]
    async fn child_exit_grace_never_extends_the_original_startup_deadline() {
        let start = Instant::now();
        let checks = Cell::new(0);
        let remaining = SESSION_RETRY * 2;
        let result = wait_spawned(
            start + remaining,
            &mut |_| pending::<zbus::Result<()>>(),
            || {
                checks.set(checks.get() + 1);
                if checks.get() == 2 {
                    anyhow::bail!("the session daemon exited (exit status: 23)");
                }
                Ok(())
            },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("exit status: 23"));
        assert_eq!(checks.get(), 2);
        assert_eq!(start.elapsed(), remaining);
    }

    #[tokio::test(start_paused = true)]
    async fn an_exit_during_the_last_poll_interval_keeps_its_exit_status() {
        let deadline = Instant::now() + SESSION_STARTUP;
        let result = wait_spawned(deadline, &mut |_| pending::<zbus::Result<()>>(), || {
            if Instant::now() >= deadline {
                anyhow::bail!("the session daemon exited (exit status: 23)");
            }
            Ok(())
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("exit status: 23"));
        assert_eq!(Instant::now(), deadline);
    }

    #[test]
    fn an_older_daemon_does_not_claim_to_support_conservative_checks() {
        use crate::config::WatchdogMode;

        let (old, supported) =
            super::DaemonClient::decode_settings(r#"{"reconnect":true}"#).unwrap();
        assert!(!supported);
        assert_eq!(old.watchdog, WatchdogMode::Aggressive);
        let (new, supported) =
            super::DaemonClient::decode_settings(r#"{"watchdog":"off"}"#).unwrap();
        assert!(supported);
        assert_eq!(new.watchdog, WatchdogMode::Off);
    }
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
