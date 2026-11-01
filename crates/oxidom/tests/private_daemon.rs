//! Real process boundaries on a private bus. The fake core never dials a target.
//! Run with `nix develop -c cargo test -p oxidom --test private_daemon`.

#![cfg(target_os = "linux")]

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use nix::libc;
use oxidom_core::config::{Config, WatchdogMode};
use oxidom_core::failover::FailoverPolicy;
use oxidom_core::ipc::{BUS_NAME, DaemonInfo, INTERFACE, OBJECT_PATH, SessionInfo};
use oxidom_core::profile::{Profile, ProfileProxy};

const LIMIT: Duration = Duration::from_secs(15);

struct Root(PathBuf);

impl Root {
    fn new() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Keep the Unix bus socket below sockaddr_un's path limit even when
        // the build runner supplies a long TMPDIR.
        let path = Path::new("/tmp").join(format!(
            "oxidom-process-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // Refuse to use an existing root, including one left by an earlier run.
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        let root = Self(path);
        for directory in ["home", "config", "data", "cache", "runtime"] {
            private_dir(&root.0.join(directory))?;
        }
        Ok(root)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn private_dir(path: &Path) -> Result<()> {
    oxidom_core::fsutil::create_private_dir(path)
}

/// A pidfd remains tied to this process even if a numeric PID is later reused.
struct ProcessFd(OwnedFd);

impl ProcessFd {
    fn open(pid: u32) -> Result<Self> {
        // SAFETY: scalar syscall arguments; success transfers a new descriptor.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        ensure!(raw >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
        // SAFETY: pidfd_open returned this owned descriptor.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(raw as i32) }))
    }

    fn signal(&self, signal: i32) -> Result<()> {
        // SAFETY: this descriptor is live; null siginfo requests a normal signal.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        ensure!(
            result == 0,
            "pidfd signal: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    fn wait(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let mut descriptor = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives one initialized, owned descriptor.
            let result = unsafe {
                libc::poll(
                    &mut descriptor,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if result > 0 && descriptor.revents & libc::POLLIN != 0 {
                return Ok(());
            }
            if result < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            bail!("process did not exit within {timeout:?}");
        }
    }

    fn exited(&self) -> Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll receives one initialized, owned descriptor.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        ensure!(
            result >= 0,
            "polling owned process: {}",
            std::io::Error::last_os_error()
        );
        Ok(result > 0 && descriptor.revents & libc::POLLIN != 0)
    }

    fn was_reaped_by_daemon(&self) -> Result<()> {
        ensure!(self.exited()?, "fake core is still alive");
        let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: pidfd is owned, siginfo points to initialized storage, and
        // WNOWAIT ensures an adopted orphan remains visible to fallback cleanup.
        let result = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                self.0.as_raw_fd() as libc::id_t,
                information.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        ensure!(
            result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
            "fake core became an unreaped child of the test subreaper"
        );
        Ok(())
    }
}

struct OwnedChild {
    child: Child,
    process: ProcessFd,
    output: PathBuf,
    error: PathBuf,
    reaped: bool,
}

impl OwnedChild {
    fn spawn(command: &mut Command, output: PathBuf) -> Result<Self> {
        let error = output.with_extension("stderr");
        command
            .stdin(Stdio::null())
            .stdout(File::create(&output)?)
            .stderr(File::create(&error)?)
            .process_group(0);
        let mut child = command.spawn()?;
        let process = match ProcessFd::open(child.id()) {
            Ok(process) => process,
            Err(error) => {
                // Keep the unreaped leader as the group identity until all
                // descendants have been signalled, even on setup failure.
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child.id() as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                let _ = child.kill();
                let _ = child.wait();
                Self::reap_group(child.id());
                return Err(error);
            }
        };
        Ok(Self {
            child,
            process,
            output,
            error,
            reaped: false,
        })
    }

    fn wait(&mut self) -> Result<ExitStatus> {
        self.process.wait(LIMIT).with_context(|| self.log())?;
        // The leader is still unreaped, so the PGID cannot name a successor.
        self.kill_group();
        let status = self.child.wait().context("reaping owned child")?;
        self.reaped = true;
        self.reap_adopted();
        Ok(status)
    }

    fn log(&self) -> String {
        format!(
            "stdout:\n{}\nstderr:\n{}",
            Self::read(&self.output),
            Self::read(&self.error)
        )
    }

    fn read(path: &Path) -> String {
        File::open(path)
            .map(|file| {
                let mut bytes = Vec::new();
                let _ = file.take(64 * 1024).read_to_end(&mut bytes);
                String::from_utf8_lossy(&bytes).into_owned()
            })
            .unwrap_or_default()
    }

    fn terminate(&mut self) -> Result<()> {
        self.process.signal(libc::SIGTERM)?;
        self.process.wait(LIMIT).with_context(|| self.log())
    }

    /// Only used after normal cleanup assertions, before fallback group cleanup
    /// can obscure whether the daemon itself reaped its fake cores.
    fn reap_verified_shutdown(&mut self) -> Result<()> {
        let status = self.child.wait()?;
        self.reaped = true;
        ensure!(status.success(), "daemon terminal failure: {}", self.log());
        Ok(())
    }

    fn reap_verified_bus_loss(&mut self) -> Result<()> {
        let status = self.child.wait()?;
        self.reaped = true;
        ensure!(!status.success(), "bus loss must report a terminal failure");
        let log = self.log();
        ensure!(
            log.contains("D-Bus transport closed")
                || log.contains("D-Bus service ownership unavailable"),
            "daemon failed outside the expected bus-loss boundary: {log}"
        );
        Ok(())
    }

    fn kill_group(&self) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.child.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    fn reap_adopted(&self) {
        Self::reap_group(self.child.id());
    }

    fn reap_group(pid: u32) {
        let deadline = Instant::now() + LIMIT;
        loop {
            match nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(-(pid as i32)),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ) {
                Err(nix::errno::Errno::ECHILD) => break,
                Ok(nix::sys::wait::WaitStatus::StillAlive) if Instant::now() < deadline => {
                    std::thread::yield_now()
                }
                Ok(nix::sys::wait::WaitStatus::StillAlive) | Err(_) => {
                    eprintln!("owned process group did not fully reap");
                    break;
                }
                Ok(_) => {}
            }
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            eprintln!(
                "owned child {} cleanup; captured output:\n{}",
                self.child.id(),
                self.log()
            );
            // The unreaped leader still reserves its PID and process-group ID.
            self.kill_group();
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reap_adopted();
        }
    }
}

struct PrivateBus {
    child: Child,
    address: String,
}

impl PrivateBus {
    fn start(root: &Root) -> Result<Self> {
        let config = root.0.join("bus.conf");
        oxidom_core::fsutil::write_private_atomic(&config, format!(
            "<busconfig><type>session</type><listen>unix:path={}</listen><auth>EXTERNAL</auth><policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow own=\"*\"/><allow user=\"*\"/></policy></busconfig>",
            root.0.join("bus").display()
        ).as_bytes())?;
        let mut child = Command::new(
            std::env::var_os("OXIDOM_TEST_DBUS_DAEMON").unwrap_or_else(|| "dbus-daemon".into()),
        )
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.0.join("home"))
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--print-address=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(File::create(root.0.join("bus.log"))?)
        .spawn()?;
        let stdout = child.stdout.take().context("bus stdout")?;
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut address = String::new();
            let result = BufReader::new(stdout)
                .read_line(&mut address)
                .map(|_| address);
            let _ = send.send(result);
        });
        let address = match receive.recv_timeout(LIMIT) {
            Ok(Ok(address)) if !address.trim().is_empty() => address.trim().to_string(),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                bail!("private bus did not publish its address: {result:?}");
            }
        };
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("bus reader panicked"))?;
        Ok(Self { child, address })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Harness {
    // Drop order: bus before root. Every daemon/CLI guard is local to the test.
    bus: PrivateBus,
    root: Root,
    serial: AtomicU64,
}

impl Harness {
    fn new() -> Result<Self> {
        let root = Root::new()?;
        let bus = PrivateBus::start(&root)?;
        let harness = Self {
            bus,
            root,
            serial: AtomicU64::new(0),
        };
        harness.seed()?;
        Ok(harness)
    }

    fn config_dir(&self) -> PathBuf {
        self.root.0.join("config/oxidom")
    }
    fn data_dir(&self) -> PathBuf {
        self.root.0.join("data/oxidom")
    }

    fn seed(&self) -> Result<()> {
        private_dir(&self.config_dir().join("profiles"))?;
        private_dir(&self.data_dir())?;
        let executable = std::env::current_exe()?;
        let quoted = format!(
            "'{}'",
            executable
                .to_str()
                .context("test executable")?
                .replace('\'', "'\\''")
        );
        let fake = self.root.0.join("fake-xray");
        oxidom_core::fsutil::write_private_atomic(&fake, format!(
            "#!/bin/sh\ncase \"$1\" in\nversion) printf 'Xray 26.3.27 (offline process fixture)\\n';;\nrun) export OXIDOM_TEST_CORE_CONFIG=\"$3\"; exec {quoted} --exact fake_core_answers_without_dialing --nocapture;;\n*) exit 1;;\nesac\n"
        ).as_bytes())?;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700))?;
        // Keep both reservations while selecting; production necessarily hands
        // released ports to the core rather than receiving listeners from us.
        let socks = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let config = Config {
            socks_port: socks.local_addr()?.port(),
            http_port: http.local_addr()?.port(),
            xray_binary: fake.display().to_string(),
            latency_test_url: "http://offline.example.invalid/confirm".into(),
            watchdog: WatchdogMode::Off,
            system_proxy: false,
            standby_proxy: false,
            reconnect: false,
            ..Config::default()
        };
        oxidom_core::fsutil::write_private_atomic(
            &self.config_dir().join("config.toml"),
            toml::to_string(&config)?.as_bytes(),
        )?;
        let profile = Profile {
            description: "unchanged saved profile sentinel".into(),
            proxy: ProfileProxy {
                socks_port: config.socks_port,
                http_port: config.http_port,
            },
            ..Profile::default()
        };
        oxidom_core::fsutil::write_private_atomic(
            &self.config_dir().join("profiles/default.toml"),
            toml::to_string(&profile)?.as_bytes(),
        )?;
        oxidom_core::fsutil::write_private_atomic(
            &self.data_dir().join("state.toml"),
            b"sessions = []\n",
        )?;
        oxidom_core::fsutil::write_private_atomic(
            &self.data_dir().join("subscriptions.json"),
            b"[]\n",
        )?;
        Ok(())
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oxidom"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.0.join("home"))
            .env("XDG_CONFIG_HOME", self.root.0.join("config"))
            .env("XDG_DATA_HOME", self.root.0.join("data"))
            .env("XDG_CACHE_HOME", self.root.0.join("cache"))
            .env("XDG_STATE_HOME", self.root.0.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.0.join("runtime"))
            .env("DBUS_SESSION_BUS_ADDRESS", &self.bus.address)
            .env(
                "DBUS_SYSTEM_BUS_ADDRESS",
                format!("unix:path={}", self.root.0.join("no-system-bus").display()),
            )
            .env("OXIDOM_SESSION_DAEMON", "external")
            .env("GSETTINGS_BACKEND", "memory")
            .current_dir(&self.root.0);
        command
    }

    fn spawn(&self, arguments: &[&str]) -> Result<OwnedChild> {
        let serial = self.serial.fetch_add(1, Ordering::Relaxed);
        OwnedChild::spawn(
            self.command().args(arguments),
            self.root.0.join(format!("child-{serial}.log")),
        )
    }

    fn cli(&self, arguments: &[&str]) -> Result<String> {
        let mut child = self.spawn(arguments)?;
        let status = child.wait()?;
        ensure!(
            status.success(),
            "CLI {arguments:?} failed: {}",
            child.log()
        );
        Ok(OwnedChild::read(&child.output))
    }

    fn connection(&self) -> Result<zbus::blocking::Connection> {
        Ok(
            zbus::blocking::connection::Builder::address(self.bus.address.as_str())?
                .method_timeout(Duration::from_secs(1))
                .build()?,
        )
    }
}

fn json_call<T: serde::de::DeserializeOwned>(
    proxy: &zbus::blocking::Proxy<'_>,
    method: &str,
) -> Result<T> {
    let json: String = proxy.call(method, &())?;
    Ok(serde_json::from_str(&json)?)
}

fn wait_until(mut predicate: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + LIMIT;
    loop {
        if predicate()? {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "condition did not become true within {LIMIT:?}"
        );
        // Yield between observations; only the observed protocol/state event
        // advances the test, never an elapsed sequencing delay.
        std::thread::yield_now();
    }
}

fn sessions(proxy: &zbus::blocking::Proxy<'_>) -> Result<Vec<SessionInfo>> {
    json_call(proxy, "ListSessions")
}

struct Subreaper(i32);

impl Subreaper {
    fn install() -> Result<Self> {
        let mut previous = 0_i32;
        // SAFETY: prctl receives a valid initialized integer output pointer.
        ensure!(
            unsafe {
                libc::prctl(
                    libc::PR_GET_CHILD_SUBREAPER,
                    &mut previous as *mut i32,
                    0,
                    0,
                    0,
                )
            } == 0,
            "reading test subreaper policy"
        );
        // SAFETY: scalar prctl arguments. This dedicated integration binary
        // owns every process it creates; failed daemons must not orphan fakes.
        ensure!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
            "installing test subreaper"
        );
        Ok(Self(previous))
    }
}

impl Drop for Subreaper {
    fn drop(&mut self) {
        // SAFETY: scalar prctl arguments; owned children have already dropped.
        unsafe {
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, self.0, 0, 0, 0);
        }
    }
}

fn ready_sessions(proxy: &zbus::blocking::Proxy<'_>, names: &[&str]) -> Result<()> {
    wait_until(|| {
        let sessions = sessions(proxy)?;
        Ok(names.iter().all(|name| {
            sessions
                .iter()
                .any(|session| session.profile == *name && session.state == "connected")
        }))
    })
}

#[test]
fn one_store_owner_reaps_independent_sessions_and_restarts_without_permission_resumption()
-> Result<()> {
    let _subreaper = Subreaper::install()?;
    let mut harness = Harness::new()?;
    let sentinels = [
        harness.config_dir().join("config.toml"),
        harness.config_dir().join("profiles/default.toml"),
        harness.data_dir().join("state.toml"),
        harness.data_dir().join("subscriptions.json"),
    ];
    let before = sentinels
        .iter()
        .map(fs::read)
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut first = harness.spawn(&["daemon"])?;
    let mut second = harness.spawn(&["daemon"])?;
    let connection = harness.connection()?;
    let bus = zbus::blocking::fdo::DBusProxy::new(&connection)?;
    let mut owner = None;
    wait_until(|| {
        ensure!(
            !first.process.exited()? || !second.process.exited()?,
            "both daemon starters exited: {} {}",
            first.log(),
            second.log()
        );
        if let Ok(name) = bus.get_name_owner(BUS_NAME.try_into()?) {
            owner = Some(name.to_string());
            return Ok(true);
        }
        Ok(false)
    })?;
    drop(bus);
    let proxy = zbus::blocking::Proxy::new_owned(
        connection,
        owner.context("daemon owner")?,
        OBJECT_PATH,
        INTERFACE,
    )?;
    let info: DaemonInfo = json_call(&proxy, "DaemonInfo")?;
    info.validate()?;
    // The losing contender must fail, rather than wait for store release.
    wait_until(|| Ok(first.process.exited()? || second.process.exited()?))?;
    let (winner, loser) = if first.process.exited()? {
        (&mut second, &mut first)
    } else {
        (&mut first, &mut second)
    };
    ensure!(!loser.wait()?.success(), "both daemon starters succeeded");
    ensure!(
        loser.log().contains("another daemon may already own"),
        "loser failed outside the store lease: {}",
        loser.log()
    );
    ensure!(!winner.process.exited()?, "winning daemon exited");
    for (path, bytes) in sentinels.iter().zip(before) {
        ensure!(fs::read(path)? == bytes, "starter changed a saved sentinel");
    }

    let imported: (u32, u32) = proxy.call("ImportLinks", &("trojan://invented-one@one.example.invalid:443#One\ntrojan://invented-two@two.example.invalid:443#Two",))?;
    ensure!(imported.0 == 2, "two invented servers were imported");
    let servers: Vec<oxidom_core::cli_json::ServerOutput> =
        serde_json::from_str(&harness.cli(&["list", "servers", "--json"])?)?;
    ensure!(servers.len() == 2, "catalog is private and complete");
    let ids = servers
        .iter()
        .map(|server| server.id.clone())
        .collect::<Vec<_>>();
    let default_json: String = proxy.call("GetProfile", &("default",))?;
    let mut profile: Profile = serde_json::from_str(&default_json)?;
    profile.select.server = ids[0].clone();
    profile.failover = Some(FailoverPolicy {
        enabled: true,
        members: ids.clone(),
        max_attempts: 1,
        cooldown_secs: 1,
    });
    let _: () = proxy.call("SaveProfile", &("work", serde_json::to_string(&profile)?))?;
    profile.select.server = ids[1].clone();
    profile.failover = None;
    let _: () = proxy.call("SaveProfile", &("home", serde_json::to_string(&profile)?))?;
    harness.cli(&["up", "work"])?;
    harness.cli(&["up", "home"])?;
    ready_sessions(&proxy, &["work", "home"])?;
    let active = sessions(&proxy)?;
    ensure!(active.len() == 2, "two independent proxy-only sessions");
    let work = active
        .iter()
        .find(|session| session.profile == "work")
        .context("work session")?;
    let home = active
        .iter()
        .find(|session| session.profile == "home")
        .context("home session")?;
    ensure!(
        work.address != home.address,
        "profiles retain independent inbounds"
    );
    ensure!(
        active
            .iter()
            .all(|session| session.interface.is_none() && !session.owns_system_proxy),
        "no privileged interface or desktop proxy"
    );
    ensure!(
        work.runtime
            .as_ref()
            .and_then(|runtime| runtime.failover.as_ref())
            .is_some_and(|activation| activation.state == "armed"),
        "explicit saved Up arms the work grant"
    );
    ensure!(
        home.runtime
            .as_ref()
            .and_then(|runtime| runtime.failover.as_ref())
            .is_none(),
        "the other session has no failover grant"
    );
    let state: oxidom_core::state::State =
        toml::from_str(&fs::read_to_string(harness.data_dir().join("state.toml"))?)?;
    let mut cores = state
        .sessions
        .iter()
        .map(|session| {
            let pid = session.xray_pid.context("durable fake core PID")?;
            Ok((pid, ProcessFd::open(pid)?))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(cores.len() == 2, "both fake core PIDs are durable");

    // A dead unarmed peer must not disturb work or borrow its failover grant.
    let home_pid = state
        .sessions
        .iter()
        .find(|session| session.profile == "home")
        .and_then(|session| session.xray_pid)
        .context("home core PID")?;
    cores
        .iter()
        .find(|(pid, _)| *pid == home_pid)
        .context("pinned home core")?
        .1
        .signal(libc::SIGKILL)?;
    wait_until(|| {
        let current = sessions(&proxy)?;
        Ok(current
            .iter()
            .any(|session| session.profile == "home" && session.state == "error")
            && current
                .iter()
                .any(|session| session.profile == "work" && session.state == "connected"))
    })?;
    let still_active = sessions(&proxy)?;
    let work_after_loss = still_active
        .iter()
        .find(|session| session.profile == "work")
        .context("work after peer loss")?;
    ensure!(
        work_after_loss
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.failover.as_ref())
            .is_some_and(|activation| activation.state == "armed"
                && activation.attempts == 0
                && activation.current == ids[0]),
        "peer loss cannot consume work's authorization"
    );
    ensure!(
        still_active
            .iter()
            .find(|session| session.profile == "home")
            .and_then(|session| session.runtime.as_ref())
            .and_then(|runtime| runtime.failover.as_ref())
            .is_none(),
        "peer loss does not grant automatic switching"
    );

    // Stop only home. Work's activation and actual core must remain independent.
    let work_pid = state
        .sessions
        .iter()
        .find(|session| session.profile == "work")
        .and_then(|session| session.xray_pid)
        .context("work core PID")?;
    harness.cli(&["down", "home"])?;
    ready_sessions(&proxy, &["work"])?;
    let remaining = sessions(&proxy)?;
    ensure!(
        remaining.len() == 1 && remaining[0].profile == "work",
        "named Stop leaves only its peer"
    );
    ensure!(
        !cores
            .iter()
            .find(|(pid, _)| *pid == work_pid)
            .context("pinned work core")?
            .1
            .exited()?,
        "peer core survives named Stop"
    );
    harness.cli(&["up", "home"])?;
    ready_sessions(&proxy, &["work", "home"])?;
    let restored: oxidom_core::state::State =
        toml::from_str(&fs::read_to_string(harness.data_dir().join("state.toml"))?)?;
    let restored_home = restored
        .sessions
        .iter()
        .find(|session| session.profile == "home")
        .and_then(|session| session.xray_pid)
        .context("explicitly restored home core")?;
    cores.push((restored_home, ProcessFd::open(restored_home)?));
    ensure!(
        restored
            .sessions
            .iter()
            .find(|session| session.profile == "work")
            .and_then(|session| session.xray_pid)
            == Some(work_pid),
        "peer recovery preserves the original work core"
    );

    winner.terminate()?;
    for (pid, process) in cores {
        process.wait(LIMIT)?;
        process
            .was_reaped_by_daemon()
            .with_context(|| format!("daemon reaping fake core {pid}"))?;
    }
    winner.reap_verified_shutdown()?;
    ensure!(
        proxy.call::<_, _, String>("DaemonInfo", &()).is_err(),
        "the old owner is terminal"
    );
    let clean: oxidom_core::state::State =
        toml::from_str(&fs::read_to_string(harness.data_dir().join("state.toml"))?)?;
    ensure!(
        clean.sessions.is_empty(),
        "clean shutdown clears recovery sessions"
    );
    let mut restarted = harness.spawn(&["daemon"])?;
    let connection = harness.connection()?;
    let bus = zbus::blocking::fdo::DBusProxy::new(&connection)?;
    let mut owner = None;
    wait_until(|| {
        ensure!(
            !restarted.process.exited()?,
            "restart failed: {}",
            restarted.log()
        );
        if let Ok(name) = bus.get_name_owner(BUS_NAME.try_into()?) {
            owner = Some(name.to_string());
            return Ok(true);
        }
        Ok(false)
    })?;
    drop(bus);
    let next = zbus::blocking::Proxy::new_owned(
        connection,
        owner.context("replacement owner")?,
        OBJECT_PATH,
        INTERFACE,
    )?;
    let new_info: DaemonInfo = json_call(&next, "DaemonInfo")?;
    ensure!(
        info.store_id == new_info.store_id,
        "store continuity survives daemon replacement"
    );
    ensure!(
        info.incarnation != new_info.incarnation,
        "replacement has a new incarnation"
    );
    ensure!(
        sessions(&next)?.is_empty(),
        "restart resumes neither sessions nor automatic grants"
    );
    let saved_json: String = next.call("GetProfile", &("work",))?;
    let saved: Profile = serde_json::from_str(&saved_json)?;
    ensure!(
        saved
            .failover
            .is_some_and(|policy| policy.enabled && policy.members == ids),
        "persisted intent survives without arming runtime"
    );
    let output: serde_json::Value =
        serde_json::from_str(&harness.cli(&["profile", "failover", "work", "--json"])?)?;
    ensure!(
        output
            .get("runtime")
            .is_some_and(serde_json::Value::is_null),
        "CLI reports no resumed activation"
    );
    // A transport loss is also terminal. A fresh explicit Up creates one core;
    // killing only our private bus must make the daemon drain it by itself.
    harness.cli(&["up", "home"])?;
    ready_sessions(&next, &["home"])?;
    let before_loss: oxidom_core::state::State =
        toml::from_str(&fs::read_to_string(harness.data_dir().join("state.toml"))?)?;
    ensure!(
        before_loss.sessions.len() == 1,
        "only the explicit Up resumed"
    );
    let core = ProcessFd::open(
        before_loss.sessions[0]
            .xray_pid
            .context("core before private bus loss")?,
    )?;
    harness.bus.child.kill()?;
    harness.bus.child.wait()?;
    restarted
        .process
        .wait(LIMIT)
        .with_context(|| restarted.log())?;
    core.wait(LIMIT)?;
    core.was_reaped_by_daemon()?;
    restarted.reap_verified_bus_loss()?;
    let after_loss: oxidom_core::state::State =
        toml::from_str(&fs::read_to_string(harness.data_dir().join("state.toml"))?)?;
    ensure!(
        after_loss.sessions.is_empty(),
        "bus loss clears recovery sessions"
    );
    Ok(())
}

/// Reexecuted by the fake binary, not an extra top-level network scenario.
#[test]
fn fake_core_answers_without_dialing() -> Result<()> {
    let Some(config) = std::env::var_os("OXIDOM_TEST_CORE_CONFIG") else {
        return Ok(());
    };
    // The daemon blocks terminal signals for signalfd before spawning. A real
    // core owns its signal handling; this fake uses ordinary SIGTERM semantics.
    let empty = nix::sys::signal::SigSet::empty();
    nix::sys::signal::pthread_sigmask(
        nix::sys::signal::SigmaskHow::SIG_SETMASK,
        Some(&empty),
        None,
    )?;
    let config: serde_json::Value = serde_json::from_slice(&fs::read(config)?)?;
    for inbound in config["inbounds"].as_array().context("fake inbounds")? {
        let address: Ipv4Addr = inbound["listen"]
            .as_str()
            .context("fake listen address")?
            .parse()?;
        ensure!(
            address.is_loopback(),
            "fake core only accepts loopback inbounds"
        );
        let port = u16::try_from(inbound["port"].as_u64().context("fake inbound port")?)?;
        let listener = TcpListener::bind((address, port))?;
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
                    let length = match request[3] {
                        1 => 4,
                        4 => 16,
                        3 => {
                            let mut length = [0];
                            stream.read_exact(&mut length)?;
                            usize::from(length[0])
                        }
                        _ => return Ok(()),
                    };
                    stream.read_exact(&mut vec![0; length + 2])?;
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
