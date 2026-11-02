//! Authorized failover across real daemon, CLI, core and private-bus processes.

use super::*;
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, atomic::AtomicBool};

const HOSTS: [&str; 5] = [
    "a.failover.invalid",
    "b.failover.invalid",
    "c.failover.invalid",
    "outside.failover.invalid",
    "peer.failover.invalid",
];

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    kind: String,
    target: String,
    pid: u32,
}

struct ProbeGate {
    stream: UnixStream,
    target: String,
    pid: u32,
}

impl ProbeGate {
    fn reply(mut self, success: bool) -> Result<()> {
        self.stream.write_all(if success { b"O" } else { b"R" })?;
        self.stream.shutdown(Shutdown::Write)?;
        Ok(())
    }
}

struct Control {
    listener: UnixListener,
    starts: Vec<(String, u32)>,
    peer_pid: Option<u32>,
}

impl Control {
    fn new(harness: &Harness) -> Result<Self> {
        let listener = UnixListener::bind(harness.root.0.join("failover-control"))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            starts: Vec::new(),
            peer_pid: None,
        })
    }

    fn receive(&mut self) -> Result<Option<ProbeGate>> {
        let (stream, _) = match self.listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut record = String::new();
        BufReader::new(stream.try_clone()?)
            .take(4096)
            .read_line(&mut record)?;
        ensure!(record.ends_with('\n'), "fake control record is incomplete");
        let event: Event = serde_json::from_str(&record)?;
        ensure!(
            HOSTS.contains(&event.target.as_str()),
            "unrecognized fake outbound"
        );
        ensure!(
            event.target != HOSTS[3],
            "an unauthorized catalog member started"
        );
        if event.target == HOSTS[4]
            && let Some(peer_pid) = self.peer_pid
        {
            ensure!(
                event.pid == peer_pid,
                "another incarnation targeted the peer endpoint"
            );
        }
        match event.kind.as_str() {
            "started" => {
                ensure!(
                    event.target != HOSTS[4] || self.count(HOSTS[4]) == 0,
                    "the peer endpoint started more than once"
                );
                self.starts.push((event.target, event.pid));
                Ok(None)
            }
            "probe" => Ok(Some(ProbeGate {
                stream,
                target: event.target,
                pid: event.pid,
            })),
            _ => bail!("unknown fake control event"),
        }
    }

    fn wait_for_probe(&mut self, target: &str) -> Result<ProbeGate> {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(probe) = self.receive()? {
                ensure!(
                    probe.target == target,
                    "unexpected candidate probe: {}",
                    probe.target
                );
                return Ok(probe);
            }
            ensure!(
                Instant::now() < deadline,
                "fake confirmation did not arrive for {target}"
            );
            let mut ready = libc::pollfd {
                fd: self.listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            // SAFETY: poll receives one initialized owned descriptor.
            let result = unsafe {
                libc::poll(
                    &mut ready,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            ensure!(
                result >= 0
                    || std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted,
                "polling fake control failed"
            );
        }
    }

    fn drain_events(&mut self) -> Result<()> {
        loop {
            let mut ready = libc::pollfd {
                fd: self.listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives one initialized owned descriptor.
            let result = unsafe { libc::poll(&mut ready, 1, 0) };
            ensure!(result >= 0, "polling fake control failed");
            if result == 0 {
                return Ok(());
            }
            if let Some(probe) = self.receive()? {
                // Each incarnation reports only its first full SOCKS request.
                // All accepted initial gates are consumed before this drain.
                bail!(
                    "an unexpected incarnation reached confirmation: {} ({})",
                    probe.target,
                    probe.pid
                );
            }
        }
    }

    fn bind_peer(&mut self, pid: u32) -> Result<()> {
        ensure!(self.peer_pid.is_none(), "peer identity was already bound");
        ensure!(
            self.starts
                .iter()
                .filter(|(host, _)| host == HOSTS[4])
                .map(|(_, started)| *started)
                .eq(std::iter::once(pid)),
            "the peer start does not match its pinned core"
        );
        self.peer_pid = Some(pid);
        Ok(())
    }

    fn count(&self, target: &str) -> usize {
        self.starts
            .iter()
            .filter(|(host, _)| host == target)
            .count()
    }

    /// Observe the prior admission deadline using the kernel monotonic timer.
    /// No elapsed-time guess sequences the scenario: the timer and fake-core
    /// control events are polled together and every unexpected start is refused.
    fn past_deadline(&mut self, remaining_ms: u64) -> Result<()> {
        // SAFETY: timerfd_create takes scalar flags and returns a new descriptor.
        let raw = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC) };
        ensure!(raw >= 0, "creating cooldown witness timer");
        // SAFETY: a successful timerfd_create transfers this descriptor.
        let timer = unsafe { OwnedFd::from_raw_fd(raw) };
        let duration = Duration::from_millis(remaining_ms.saturating_add(250));
        let specification = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: duration.as_secs() as libc::time_t,
                tv_nsec: duration.subsec_nanos().into(),
            },
        };
        // SAFETY: timer and initialized timer specification remain live.
        ensure!(
            unsafe {
                libc::timerfd_settime(timer.as_raw_fd(), 0, &specification, std::ptr::null_mut())
            } == 0,
            "arming cooldown witness"
        );
        let deadline = Instant::now() + duration + LIMIT;
        loop {
            let mut descriptors = [
                libc::pollfd {
                    fd: timer.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "cooldown witness did not fire");
            // SAFETY: poll receives two initialized owned descriptors.
            let result = unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if result < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            ensure!(result > 0, "cooldown witness polling failed");
            self.drain_events()?;
            ensure!(
                self.count(HOSTS[2]) == 0,
                "cancelled candidate started before or after its former deadline"
            );
            if descriptors[0].revents & libc::POLLIN != 0 {
                return Ok(());
            }
        }
    }
}

struct Catalog {
    ids: Vec<String>,
    links: Vec<String>,
}

fn catalog(
    harness: &Harness,
    proxy: &zbus::blocking::Proxy<'_>,
    members: usize,
    budget: usize,
    cooldown: u64,
) -> Result<Catalog> {
    let links = HOSTS
        .iter()
        .enumerate()
        .map(|(index, host)| format!("trojan://invented-{index}@{host}:443#Fixture-{index}"))
        .collect::<Vec<_>>();
    let imported: (u32, u32) = proxy.call("ImportLinks", &(links.join("\n"),))?;
    ensure!(
        imported == (5, 0),
        "five invented catalog members were imported"
    );
    let stored: Vec<oxidom_core::cli_json::ServerOutput> =
        serde_json::from_str(&harness.cli(&["list", "servers", "--json"])?)?;
    let ids = HOSTS
        .iter()
        .map(|host| {
            stored
                .iter()
                .find(|server| server.address == *host)
                .map(|server| server.id.clone())
                .context("invented catalog ID")
        })
        .collect::<Result<Vec<_>>>()?;
    let json: String = proxy.call("GetProfile", &("default",))?;
    let mut profile: Profile = serde_json::from_str(&json)?;
    profile.select.server = ids[0].clone();
    profile.failover = None;
    let _: () = proxy.call("SaveProfile", &("work", serde_json::to_string(&profile)?))?;
    profile.select.server = ids[4].clone();
    let _: () = proxy.call("SaveProfile", &("peer", serde_json::to_string(&profile)?))?;
    harness.cli(&[
        "profile",
        "failover",
        "work",
        "--enable",
        "--members",
        &ids[..members].join(","),
        "--max-attempts",
        &budget.to_string(),
        "--cooldown-secs",
        &cooldown.to_string(),
        "--json",
    ])?;
    Ok(Catalog { ids, links })
}

fn progress(proxy: &zbus::blocking::Proxy<'_>) -> Result<oxidom_core::ipc::RuntimeFailover> {
    sessions(proxy)?
        .into_iter()
        .find(|session| session.profile == "work")
        .and_then(|session| session.runtime)
        .and_then(|runtime| runtime.failover)
        .context("work activation progress")
}

fn peer_unchanged(
    harness: &Harness,
    proxy: &zbus::blocking::Proxy<'_>,
    catalog: &Catalog,
    peer: &(u32, ProcessFd),
) -> Result<()> {
    let current = sessions(proxy)?
        .into_iter()
        .find(|session| session.profile == "peer")
        .context("independent peer")?;
    ensure!(
        current.state == "connected" && current.server_id.as_ref() == Some(&catalog.ids[4]),
        "peer target or health changed"
    );
    ensure!(
        current
            .runtime
            .and_then(|runtime| runtime.failover)
            .is_none(),
        "peer inherited work authorization"
    );
    ensure!(!peer.1.exited()?, "original peer core exited");
    ensure!(
        harness
            .state()?
            .sessions
            .iter()
            .find(|session| session.profile == "peer")
            .and_then(|session| session.xray_pid)
            == Some(peer.0),
        "peer core was replaced"
    );
    Ok(())
}

fn start_pair(
    harness: &Harness,
    proxy: &zbus::blocking::Proxy<'_>,
    control: &mut Control,
) -> Result<((u32, ProcessFd), (u32, ProcessFd))> {
    harness.cli(&["up", "work"])?;
    let initial = control.wait_for_probe(HOSTS[0])?;
    let work = harness.core("work")?;
    ensure!(
        initial.pid == work.0,
        "work confirmation did not belong to its durable core"
    );
    initial.reply(true)?;
    ready_sessions(proxy, &["work"])?;
    harness.cli(&["up", "peer"])?;
    let initial = control.wait_for_probe(HOSTS[4])?;
    let peer = harness.core("peer")?;
    ensure!(
        initial.pid == peer.0,
        "peer confirmation did not belong to its durable core"
    );
    control.bind_peer(peer.0)?;
    initial.reply(true)?;
    ready_sessions(proxy, &["work", "peer"])?;
    Ok((work, peer))
}

fn shutdown(daemon: &mut OwnedChild, cores: &[(u32, ProcessFd)]) -> Result<()> {
    daemon.terminate()?;
    for (pid, process) in cores {
        process.wait(LIMIT)?;
        process
            .was_reaped_by_daemon()
            .with_context(|| format!("daemon reaping fake core {pid}"))?;
    }
    daemon.reap_verified_shutdown()
}

#[test]
fn authorized_process_recovery_keeps_its_peer_and_revoked_cooldown_cannot_resume() -> Result<()> {
    let _serial = serial();
    for catalog_revocation in [false, true] {
        let _subreaper = Subreaper::install()?;
        let harness = Harness::new()?;
        harness.install_fake("failover_matrix::gated_core")?;
        let mut control = Control::new(&harness)?;
        let (mut daemon, proxy, _) = harness.daemon()?;
        let catalog = catalog(&harness, &proxy, 3, 2, 10)?;
        let saved = fs::read(harness.config_dir().join("profiles/work.toml"))?;
        let (initial, peer) = start_pair(&harness, &proxy, &mut control)?;
        initial.1.signal(libc::SIGKILL)?;
        let alternate = control.wait_for_probe(HOSTS[1])?;
        let pending = progress(&proxy)?;
        ensure!(
            pending.state == "recovering"
                && pending.current == catalog.ids[1]
                && pending.attempts == 1
                && pending.next.is_none(),
            "an admitted confirmation owns exactly one automatic attempt"
        );
        let alternate_core = harness.core("work")?;
        ensure!(
            alternate.pid == alternate_core.0,
            "alternate confirmation did not belong to its durable core"
        );
        alternate.reply(true)?;
        wait_until(|| {
            let current = progress(&proxy)?;
            Ok(current.state == "armed"
                && current.current == catalog.ids[1]
                && current.attempts == 0)
        })?;
        ready_sessions(&proxy, &["work", "peer"])?;
        ensure!(
            fs::read(harness.config_dir().join("profiles/work.toml"))? == saved,
            "automatic recovery rewrote the saved primary or grant"
        );
        peer_unchanged(&harness, &proxy, &catalog, &peer)?;
        alternate_core.1.signal(libc::SIGKILL)?;
        let mut remaining_ms = 0;
        wait_until(|| {
            let current = sessions(&proxy)?
                .into_iter()
                .find(|session| session.profile == "work")
                .context("recovering work")?;
            let runtime = current.runtime.context("recovering runtime")?;
            let failover = runtime.failover.context("recovering grant")?;
            remaining_ms = runtime.retry.remaining_ms.unwrap_or_default();
            Ok(failover.current == catalog.ids[1]
                && failover.next.as_ref() == Some(&catalog.ids[2])
                && runtime.retry.state == "backoff"
                && remaining_ms > 0)
        })?;
        if catalog_revocation {
            let _: bool = proxy.call("RemoveServer", &(&catalog.ids[2],))?;
            ensure!(
                progress(&proxy)?.state == "disarmed",
                "catalog removal did not revoke the waiting grant"
            );
            let imported: (u32, u32) = proxy.call("ImportLinks", &(&catalog.links[2],))?;
            ensure!(imported == (1, 0), "removed candidate was not reimported");
            let stored: Vec<oxidom_core::cli_json::ServerOutput> =
                serde_json::from_str(&harness.cli(&["list", "servers", "--json"])?)?;
            ensure!(
                stored
                    .iter()
                    .any(|server| server.id == catalog.ids[2] && server.address == HOSTS[2]),
                "reimport changed the canonical candidate ID"
            );
            let output: serde_json::Value = serde_json::from_str(&harness.cli(&[
                "profile",
                "failover",
                "work",
                "--enable",
                "--members",
                &catalog.ids[..3].join(","),
                "--json",
            ])?)?;
            ensure!(
                output["saved"]["enabled"] == true && output["runtime"]["state"] == "disarmed",
                "readdition or saving rearmed a revoked activation"
            );
        } else {
            harness.cli(&["down", "work"])?;
            let output: serde_json::Value = serde_json::from_str(
                &harness.cli(&["profile", "failover", "work", "--enable", "--json"])?,
            )?;
            ensure!(
                output["saved"]["enabled"] == true
                    && output
                        .get("runtime")
                        .is_some_and(serde_json::Value::is_null),
                "saving after Stop created an activation"
            );
        }
        control.past_deadline(remaining_ms)?;
        ensure!(
            control.count(HOSTS[0]) == 1
                && control.count(HOSTS[1]) == 1
                && control.count(HOSTS[2]) == 0
                && control.count(HOSTS[4]) == 1,
            "recovery escaped its admitted A-to-B sequence"
        );
        peer_unchanged(&harness, &proxy, &catalog, &peer)?;
        if catalog_revocation {
            ensure!(
                progress(&proxy)?.state == "disarmed",
                "a cancelled waiter rearmed after its deadline"
            );
            harness.cli(&["down", "work"])?;
        }
        harness.cli(&["up", "work"])?;
        let restarted = control.wait_for_probe(HOSTS[0])?;
        let fresh = harness.core("work")?;
        ensure!(
            restarted.pid == fresh.0,
            "explicit rearm confirmation does not belong to its durable core"
        );
        restarted.reply(true)?;
        ready_sessions(&proxy, &["work", "peer"])?;
        let armed = progress(&proxy)?;
        ensure!(
            armed.state == "armed" && armed.current == catalog.ids[0],
            "explicit saved Up did not arm the restored grant"
        );
        peer_unchanged(&harness, &proxy, &catalog, &peer)?;
        shutdown(&mut daemon, &[initial, alternate_core, fresh, peer])?;
    }
    Ok(())
}

#[test]
fn failed_process_recovery_exhausts_its_budget_or_members_without_revisits() -> Result<()> {
    let _serial = serial();
    for (members, budget) in [(3, 1), (2, 64)] {
        let _subreaper = Subreaper::install()?;
        let harness = Harness::new()?;
        harness.install_fake("failover_matrix::gated_core")?;
        let mut control = Control::new(&harness)?;
        let (mut daemon, proxy, _) = harness.daemon()?;
        let catalog = catalog(&harness, &proxy, members, budget, 1)?;
        let saved = fs::read(harness.config_dir().join("profiles/work.toml"))?;
        let (initial, peer) = start_pair(&harness, &proxy, &mut control)?;
        initial.1.signal(libc::SIGKILL)?;
        let alternate = control.wait_for_probe(HOSTS[1])?;
        let pending = progress(&proxy)?;
        ensure!(
            pending.attempts == 1 && pending.current == catalog.ids[1] && pending.next.is_none(),
            "automatic admission was not attributed before confirmation"
        );
        let alternate_core = harness.core("work")?;
        ensure!(
            alternate.pid == alternate_core.0,
            "failed confirmation did not belong to its durable core"
        );
        alternate.reply(false)?;
        wait_until(|| Ok(progress(&proxy)?.state == "exhausted"))?;
        let exhausted = progress(&proxy)?;
        ensure!(
            exhausted.attempts == 1
                && exhausted.max_attempts == budget
                && exhausted.members == catalog.ids[..members]
                && exhausted.current == catalog.ids[1]
                && exhausted.next.is_none(),
            "exhaustion facts lost the finite episode"
        );
        let output: serde_json::Value =
            serde_json::from_str(&harness.cli(&["profile", "failover", "work", "--json"])?)?;
        ensure!(
            output["runtime"]["state"] == "exhausted"
                && output["runtime"]["attempts"] == 1
                && output["saved"]["enabled"] == true,
            "CLI conflated saved permission and exhausted activation"
        );
        control.drain_events()?;
        ensure!(
            control.count(HOSTS[0]) == 1
                && control.count(HOSTS[1]) == 1
                && control.count(HOSTS[2]) == 0
                && control.count(HOSTS[4]) == 1,
            "finite episode revisited or admitted an extra candidate"
        );
        ensure!(
            fs::read(harness.config_dir().join("profiles/work.toml"))? == saved,
            "exhaustion changed saved configuration"
        );
        peer_unchanged(&harness, &proxy, &catalog, &peer)?;
        shutdown(&mut daemon, &[initial, alternate_core, peer])?;
    }
    Ok(())
}

fn send_event(root: &Path, kind: &str, target: &str) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(root.join("failover-control"))?;
    stream.set_read_timeout(Some(LIMIT))?;
    stream.set_write_timeout(Some(LIMIT))?;
    serde_json::to_writer(
        &mut stream,
        &Event {
            kind: kind.into(),
            target: target.into(),
            pid: std::process::id(),
        },
    )?;
    stream.write_all(b"\n")?;
    Ok(stream)
}

fn fake_request(
    mut stream: TcpStream,
    socks: bool,
    root: &Path,
    target: &str,
    first_probe: &AtomicBool,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    if !socks {
        return Ok(());
    }
    let mut greeting = [0; 2];
    // A readiness-only connection intentionally sends no greeting.
    if stream.read_exact(&mut greeting).is_err() {
        return Ok(());
    }
    ensure!(greeting[0] == 5, "fake only accepts SOCKS5");
    stream.read_exact(&mut vec![0; usize::from(greeting[1])])?;
    stream.write_all(&[5, 0])?;
    let mut request = [0; 4];
    stream.read_exact(&mut request)?;
    let length = match request[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut size = [0];
            stream.read_exact(&mut size)?;
            usize::from(size[0])
        }
        _ => bail!("unsupported fake SOCKS address"),
    };
    stream.read_exact(&mut vec![0; length + 2])?;
    // Only the first complete request of this incarnation is confirmation.
    // Later periodic observations receive the same canned success directly,
    // so a long fault window cannot accidentally stall the independent peer.
    let refuse = if first_probe.swap(false, Ordering::SeqCst) {
        let mut control = send_event(root, "probe", target)?;
        let mut response = [0];
        control.read_exact(&mut response)?;
        ensure!(
            matches!(response[0], b'O' | b'R'),
            "invalid fake response command"
        );
        response[0] == b'R'
    } else {
        false
    };
    if refuse {
        // A complete, measured SOCKS refusal is remote Unreachable evidence;
        // HTTP error status is deliberately not used because it is success.
        stream.write_all(&[5, 5, 0, 1, 127, 0, 0, 1, 0, 0])?;
        return Ok(());
    }
    stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])?;
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") && headers.len() < 4096 {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        headers.push(byte[0]);
    }
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
    Ok(())
}

/// Reexecuted only by the configured fake binary; no extra top-level scenario.
#[test]
fn gated_core() -> Result<()> {
    let Some(config) = std::env::var_os("OXIDOM_TEST_CORE_CONFIG") else {
        return Ok(());
    };
    let root = PathBuf::from(std::env::var_os("OXIDOM_TEST_ROOT").context("fake root")?);
    nix::sys::signal::pthread_sigmask(
        nix::sys::signal::SigmaskHow::SIG_SETMASK,
        Some(&nix::sys::signal::SigSet::empty()),
        None,
    )?;
    let config: serde_json::Value = serde_json::from_slice(&fs::read(config)?)?;
    let target = config["outbounds"]
        .as_array()
        .context("fake outbounds")?
        .iter()
        .find(|outbound| outbound["protocol"] == "trojan")
        .and_then(|outbound| outbound["settings"]["servers"][0]["address"].as_str())
        .context("fake single Trojan target")?
        .to_string();
    ensure!(HOSTS.contains(&target.as_str()), "fake target is invented");
    let mut listeners = Vec::new();
    for inbound in config["inbounds"].as_array().context("fake inbounds")? {
        let address: Ipv4Addr = inbound["listen"]
            .as_str()
            .context("fake listen address")?
            .parse()?;
        ensure!(address.is_loopback(), "fake accepts only loopback inbounds");
        let port = u16::try_from(inbound["port"].as_u64().context("fake port")?)?;
        let listener = TcpListener::bind((address, port))?;
        let socks = inbound["protocol"] == "socks";
        listeners.push((listener, socks));
    }
    drop(send_event(&root, "started", &target)?);
    let first_probe = Arc::new(AtomicBool::new(true));
    for (listener, socks) in listeners {
        let root = root.clone();
        let target = target.clone();
        let first_probe = first_probe.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let root = root.clone();
                let target = target.clone();
                let first_probe = first_probe.clone();
                std::thread::spawn(move || {
                    if let Err(error) = fake_request(stream, socks, &root, &target, &first_probe) {
                        eprintln!("offline fake request failed: {error:#}");
                    }
                });
            }
        });
    }
    loop {
        std::thread::park();
    }
}
