use super::*;

use std::collections::BTreeSet;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;

use oxidom_core::client::DaemonClient;
use oxidom_core::diagnostics::Page;

const ACTOR_SOCKET: &str = "OXIDOM_TEST_OWNER_SOCKET";

fn protocol_line(reader: &mut impl BufRead, cap: usize) -> Result<Option<String>> {
    let mut line = String::new();
    let count = reader.take((cap + 1) as u64).read_line(&mut line)?;
    if count == 0 {
        return Ok(None);
    }
    ensure!(
        count <= cap && line.ends_with('\n'),
        "private protocol record exceeded its bound or ended early"
    );
    Ok(Some(line))
}

struct Actor {
    child: OwnedChild,
    control: BufReader<UnixStream>,
}

impl Actor {
    fn start(harness: &Harness, mode: &str, launcher: Option<&Path>) -> Result<Self> {
        let path = harness.root.0.join(format!("actor-{mode}"));
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let mut command = harness.test_command()?;
        command
            .args(["--exact", "owner::public_client_actor", "--nocapture"])
            .env(ACTOR_SOCKET, &path)
            .env("OXIDOM_TEST_OWNER_MODE", mode);
        if let Some(launcher) = launcher {
            command
                .env("OXIDOM_SESSION_DAEMON", "standalone")
                .env("OXIDOM_BIN", launcher)
                .env(
                    "OXIDOM_TEST_BOOTSTRAP_SOCKET",
                    harness.root.0.join("bootstrap"),
                );
        }
        let child = OwnedChild::spawn(&mut command, harness.root.0.join(format!("{mode}.log")))?;
        let mut accepted = None;
        wait_until(|| {
            ensure!(
                !child.process.exited()?,
                "client actor exited: {}",
                child.log()
            );
            match listener.accept() {
                Ok((stream, _)) => {
                    accepted = Some(stream);
                    Ok(true)
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
                Err(error) => Err(error.into()),
            }
        })?;
        let stream = accepted.context("actor control")?;
        stream.set_read_timeout(Some(LIMIT * 3))?;
        stream.set_write_timeout(Some(LIMIT))?;
        let mut actor = Self {
            child,
            control: BufReader::new(stream),
        };
        ensure!(
            actor.receive()?["event"] == "ready",
            "actor ready handshake"
        );
        Ok(actor)
    }

    fn send(&mut self, command: &str) -> Result<()> {
        writeln!(self.control.get_mut(), "{command}")?;
        Ok(())
    }

    fn receive(&mut self) -> Result<serde_json::Value> {
        let Some(line) = protocol_line(&mut self.control, 4096)? else {
            // Keep the leader unreaped, but let libtest finish writing the
            // actual helper failure before collecting its diagnostic output.
            let _ = self.child.process.wait(LIMIT);
            bail!("actor closed control: {}", self.child.log());
        };
        Ok(serde_json::from_str(&line)?)
    }

    fn ask(&mut self, command: &str) -> Result<serde_json::Value> {
        self.send(command)?;
        self.receive()
    }

    fn finish(&mut self) -> Result<()> {
        ensure!(self.ask("done")?["event"] == "done", "actor completion");
        ensure!(
            self.child.wait()?.success(),
            "client actor failed: {}",
            self.child.log()
        );
        Ok(())
    }
}

fn reply(stream: &mut UnixStream, value: serde_json::Value) -> Result<()> {
    serde_json::to_writer(&mut *stream, &value)?;
    stream.write_all(b"\n")?;
    Ok(())
}

fn hydrate(client: &DaemonClient) -> Result<serde_json::Value> {
    let subscriptions = client.subscriptions()?;
    let profiles = client.list_profiles()?;
    let status = client.status()?;
    client.settings_with_watchdog_support()?;
    client.probe_state()?;
    client
        .runtime_info_optional()?
        .context("current daemon runtime hydration")?;
    client.validate_binding()?;
    Ok(serde_json::json!({
        "event": "hydrated", "subscriptions": subscriptions.len(),
        "profiles": profiles.len(), "profile_names": profiles.iter().map(|profile| profile.name.as_str()).collect::<Vec<_>>(),
        "sessions": status.sessions,
    }))
}

/// Reexecuted with a private environment; never changes parent-process env.
#[test]
fn public_client_actor() -> Result<()> {
    let Some(path) = std::env::var_os(ACTOR_SOCKET) else {
        return Ok(());
    };
    let mode = std::env::var("OXIDOM_TEST_OWNER_MODE")?;
    let mut control = BufReader::new(UnixStream::connect(path)?);
    control.get_ref().set_read_timeout(Some(LIMIT * 3))?;
    control.get_ref().set_write_timeout(Some(LIMIT))?;
    reply(control.get_mut(), serde_json::json!({"event": "ready"}))?;
    let line = protocol_line(&mut control, 64)?.context("explicit actor connect command")?;
    ensure!(line.trim() == "connect", "explicit actor connect");
    let original = if mode.starts_with("bootstrap") {
        DaemonClient::connect_any(|_| {})?
    } else {
        DaemonClient::connect_existing()?
    };
    let clone = original.clone();
    let stamp = original.binding_stamp();
    ensure!(
        clone.binding_stamp() == stamp,
        "clones retain the original binding"
    );
    reply(control.get_mut(), hydrate(&original)?)?;
    let mut recovered = None;
    loop {
        let line = protocol_line(&mut control, 64)?.context("actor command channel closed")?;
        match line.trim() {
            "observe" => {
                reply(control.get_mut(), hydrate(&original)?)?;
            }
            "stale" => {
                ensure!(
                    original.validate_binding().is_err() && clone.validate_binding().is_err(),
                    "old clones refuse a replaced owner"
                );
                ensure!(
                    original.down("work").is_err() && clone.down("work").is_err(),
                    "stale mutations must not be replayed"
                );
                ensure!(
                    original.binding_stamp() == stamp && clone.binding_stamp() == stamp,
                    "old clients cannot silently retarget"
                );
                reply(
                    control.get_mut(),
                    serde_json::json!({"event": "stale-refused"}),
                )?;
            }
            "recover" => {
                let candidate = original.recover_selected()?;
                ensure!(
                    candidate.binding_stamp() != stamp,
                    "recovery creates a new binding"
                );
                reply(control.get_mut(), hydrate(&candidate)?)?;
                recovered = Some(candidate);
            }
            "foreign" => {
                let candidate = recovered.as_ref().context("previous recovered client")?;
                ensure!(
                    candidate.validate_binding().is_err(),
                    "the recovered binding also retires"
                );
                ensure!(
                    original.recover_selected().is_err() && candidate.recover_selected().is_err(),
                    "another store at the selected endpoint is refused"
                );
                ensure!(
                    candidate.down("work").is_err(),
                    "old mutation cannot reach the foreign store"
                );
                reply(
                    control.get_mut(),
                    serde_json::json!({"event": "foreign-refused"}),
                )?;
            }
            "done" => {
                reply(control.get_mut(), serde_json::json!({"event": "done"}))?;
                return Ok(());
            }
            _ => bail!("unknown private actor command"),
        }
    }
}

fn import_work(proxy: &zbus::blocking::Proxy<'_>) -> Result<Vec<String>> {
    let imported: (u32, u32) = proxy.call("ImportLinks", &("trojan://invented-owner-a@owner-a.example.invalid:443#OwnerA\ntrojan://invented-owner-b@owner-b.example.invalid:443#OwnerB",))?;
    ensure!(imported.0 == 2, "private owner catalog");
    let subscriptions: Vec<oxidom_core::model::Subscription> =
        json_call(proxy, "ListSubscriptions")?;
    let ids = subscriptions
        .iter()
        .flat_map(|subscription| &subscription.servers)
        .map(|server| server.id.clone())
        .collect::<Vec<_>>();
    ensure!(ids.len() == 2, "two private canonical IDs");
    let literal: String = proxy.call("GetProfile", &("default",))?;
    let mut profile: Profile = serde_json::from_str(&literal)?;
    profile.select.server = ids[0].clone();
    profile.failover = Some(FailoverPolicy {
        enabled: true,
        members: ids.clone(),
        max_attempts: 1,
        cooldown_secs: 1,
    });
    let _: () = proxy.call("SaveProfile", &("work", serde_json::to_string(&profile)?))?;
    Ok(ids)
}

fn diagnostics(harness: &Harness, cursor: Option<&str>) -> Result<Page> {
    let mut arguments = vec!["core", "diagnostics", "--limit", "256"];
    if let Some(cursor) = cursor {
        arguments.extend(["--cursor", cursor]);
    }
    Ok(serde_json::from_str(&harness.cli(&arguments)?)?)
}

fn reap_orphan(pid: u32, process: &ProcessFd) -> Result<()> {
    process.wait(LIMIT)?;
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: owned pidfd and initialized output storage. Unlike replacement
    // daemons, the test subreaper is the orphan's actual parent.
    let result = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            process.0.as_raw_fd() as libc::id_t,
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG,
        )
    };
    ensure!(
        result == 0,
        "reaping adopted fixture: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: waitid initialized the siginfo after returning success.
    let information = unsafe { information.assume_init() };
    ensure!(
        unsafe { information.si_pid() } == pid as i32,
        "the pinned orphan was reaped"
    );
    Ok(())
}

#[test]
fn crash_recovery_preserves_the_store_without_replaying_clients_or_resetting_cursors() -> Result<()>
{
    let _serial = serial();
    let _subreaper = Subreaper::install()?;
    let harness = Harness::new()?;
    let (mut original, proxy, identity) = harness.daemon()?;
    let ids = import_work(&proxy)?;
    harness.cli(&["up", "work"])?;
    ready_sessions(&proxy, &["work"])?;
    let (old_core_pid, old_core) = harness.core("work")?;
    let expected_path = harness.data_dir().join("current-config-work.json");
    let cmdline = fs::read(format!("/proc/{old_core_pid}/cmdline"))?;
    ensure!(
        cmdline
            .split(|byte| *byte == 0)
            .any(|argument| argument == expected_path.as_os_str().as_encoded_bytes()),
        "fake identity contains the exact recovery config argument"
    );
    let mut actor = Actor::start(&harness, "recovery", None)?;
    ensure!(
        actor.ask("connect")?["event"] == "hydrated",
        "initial public client hydration"
    );
    let prefix = diagnostics(&harness, None)?;
    ensure!(
        !prefix.gap && !prefix.more && !prefix.records.is_empty(),
        "bounded initial persistent journal"
    );
    let cursor = prefix.next_cursor.as_deref().context("persistent cursor")?;

    original.process.signal(libc::SIGKILL)?;
    original.process.wait(LIMIT)?;
    ensure!(
        !old_core.exited()?,
        "only the daemon was killed before recovery"
    );
    let (mut replacement, next, successor) = harness.daemon()?;
    ensure!(
        successor.store_id == identity.store_id && successor.incarnation != identity.incarnation,
        "same-store new-incarnation crash replacement"
    );
    old_core.wait(LIMIT)?;
    reap_orphan(old_core_pid, &old_core)?;
    ensure!(
        sessions(&next)?.is_empty() && harness.state()?.sessions.is_empty(),
        "replacement cleans a non-pool orphan without resuming a grant"
    );
    let saved: String = next.call("GetProfile", &("work",))?;
    let saved: Profile = serde_json::from_str(&saved)?;
    ensure!(
        saved
            .failover
            .is_some_and(|policy| policy.enabled && policy.members == ids),
        "saved permission survives without runtime resumption"
    );
    harness.cli(&["up", "work"])?;
    ready_sessions(&next, &["work"])?;
    let (replacement_pid, replacement_core) = harness.core("work")?;
    ensure!(
        actor.ask("stale")?["event"] == "stale-refused",
        "stale public clients rejected"
    );
    ensure!(
        !replacement_core.exited()? && harness.core("work")?.0 == replacement_pid,
        "stale Down did not reach the replacement core"
    );
    let hydrated = actor.ask("recover")?;
    ensure!(
        hydrated["event"] == "hydrated"
            && hydrated["subscriptions"]
                .as_u64()
                .is_some_and(|count| count > 0)
            && hydrated["profile_names"]
                .as_array()
                .is_some_and(|names| names.iter().any(|name| name == "work"))
            && hydrated["sessions"].as_array().is_some_and(|sessions| {
                sessions.iter().any(|session| {
                    session["profile"] == "work"
                        && session["state"] == "connected"
                        && session["server_id"] == ids[0]
                })
            }),
        "recovered candidate fully hydrates the replacement"
    );
    let suffix = diagnostics(&harness, Some(cursor))?;
    ensure!(
        !suffix.gap && !suffix.more && !suffix.records.is_empty(),
        "restart follows the durable cursor without a gap"
    );
    let full = diagnostics(&harness, None)?;
    ensure!(!full.gap && !full.more, "bounded authoritative journal");
    let combined = prefix
        .records
        .iter()
        .chain(&suffix.records)
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let authoritative = full
        .records
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        authoritative.starts_with(&combined),
        "cursor prefix and suffix have no missing or duplicate records; later appends may follow"
    );
    ensure!(
        diagnostics(&harness, Some("invented-invalid-cursor"))?.gap,
        "malformed cursor reports an explicit gap"
    );

    replacement.terminate()?;
    replacement_core.wait(LIMIT)?;
    replacement_core.was_reaped_by_daemon()?;
    replacement.reap_verified_shutdown()?;
    let foreign = Harness::new()?;
    let (mut other, foreign_proxy, other_identity) = foreign.daemon_at(&harness.bus.address)?;
    ensure!(
        other_identity.store_id != identity.store_id,
        "foreign selected endpoint really uses another private store"
    );
    import_work(&foreign_proxy)?;
    foreign.cli_at(&harness.bus.address, &["up", "work"])?;
    ready_sessions(&foreign_proxy, &["work"])?;
    let (foreign_pid, foreign_core) = foreign.core("work")?;
    ensure!(
        actor.ask("foreign")?["event"] == "foreign-refused",
        "public recovery refuses foreign-store substitution"
    );
    ensure!(
        !foreign_core.exited()? && foreign.core("work")?.0 == foreign_pid,
        "foreign core survives stale mutation attempts"
    );
    actor.finish()?;
    other.terminate()?;
    foreign_core.wait(LIMIT)?;
    foreign_core.was_reaped_by_daemon()?;
    other.reap_verified_shutdown()?;
    let status = original.child.wait()?;
    original.reaped = true;
    ensure!(
        status.signal() == Some(libc::SIGKILL),
        "the original daemon ended only through the injected crash"
    );
    Ok(())
}

/// Direct children can belong to any libtest/zbus thread. Enumerating every
/// task also finds descendants adopted after an escaped daemon's parent exits.
fn direct_children() -> Result<BTreeSet<u32>> {
    let mut children = BTreeSet::new();
    for task in fs::read_dir("/proc/self/task")? {
        let path = task?.path().join("children");
        match fs::read_to_string(path) {
            Ok(text) => {
                for pid in text.split_ascii_whitespace() {
                    children.insert(pid.parse()?);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(children)
}

/// Standalone launch deliberately escapes the client's process group. Kernel
/// parentage, not a name/PGID guess, owns orphan cleanup after client guards drop.
/// The baseline's private-bus Child stays unreaped for this guard's lifetime,
/// reserving every excluded PID against reuse.
struct AdoptedChildren<'a> {
    baseline: BTreeSet<u32>,
    _bus: &'a Child,
}

impl<'a> AdoptedChildren<'a> {
    fn new(bus: &'a PrivateBus) -> Result<Self> {
        let baseline = direct_children()?;
        ensure!(
            baseline == BTreeSet::from([bus.child.id()]),
            "the private bus is the only preexisting fixture child"
        );
        Ok(Self {
            baseline,
            _bus: &bus.child,
        })
    }

    fn new_children(&self) -> Result<Vec<u32>> {
        Ok(direct_children()?
            .difference(&self.baseline)
            .copied()
            .collect())
    }

    fn require_drained(&self, registered: &[u32]) -> Result<()> {
        for pid in self.new_children()? {
            ensure!(
                registered.contains(&pid),
                "no unregistered standalone child was created"
            );
            let process = ProcessFd::open(pid)?;
            ensure!(
                process.exited()?,
                "normal cleanup cannot kill a surviving standalone child"
            );
            // The losing client's sole waiter may finish just before or after
            // client exit. If its already-dead child is adopted, reap it here.
            reap_orphan(pid, &process)?;
        }
        ensure!(
            self.new_children()?.is_empty(),
            "standalone children must be explicitly reaped before fallback cleanup"
        );
        Ok(())
    }
}

impl Drop for AdoptedChildren<'_> {
    fn drop(&mut self) {
        let deadline = Instant::now() + LIMIT * 3;
        loop {
            let Ok(children) = self.new_children() else {
                eprintln!("could not enumerate owned adopted children");
                return;
            };
            if children.is_empty() {
                return;
            }
            for pid in children {
                // An unreaped kernel child reserves its numeric PID until this
                // process reaps it; only the pinned descriptor sends signals.
                match ProcessFd::open(pid) {
                    Ok(process) => {
                        if !process.exited().unwrap_or(false) {
                            let _ = process.signal(libc::SIGKILL);
                        }
                        if process
                            .wait(deadline.saturating_duration_since(Instant::now()))
                            .is_ok()
                        {
                            let _ = nix::sys::wait::waitpid(
                                nix::unistd::Pid::from_raw(pid as i32),
                                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                            );
                        }
                    }
                    Err(error) => eprintln!("could not pin adopted child: {error:#}"),
                }
            }
            if Instant::now() >= deadline {
                eprintln!("owned adopted children did not drain within the deadline");
                return;
            }
            std::thread::yield_now();
        }
    }
}

fn launcher_file(harness: &Harness) -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let quoted = format!(
        "'{}'",
        executable
            .to_str()
            .context("integration executable")?
            .replace('\'', "'\\''")
    );
    let path = harness.root.0.join("bootstrap-launcher");
    oxidom_core::fsutil::write_private_atomic(
        &path,
        format!("#!/bin/sh\nexec {quoted} --exact owner::bootstrap_launcher --nocapture\n")
            .as_bytes(),
    )?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

/// The normal daemon executable seam lets the fixture register ownership before
/// exec, while production connect_any still performs the real launch/race.
#[test]
fn bootstrap_launcher() -> Result<()> {
    let Some(path) = std::env::var_os("OXIDOM_TEST_BOOTSTRAP_SOCKET") else {
        return Ok(());
    };
    let mut control = UnixStream::connect(path)?;
    control.set_read_timeout(Some(LIMIT * 3))?;
    control.set_write_timeout(Some(LIMIT))?;
    writeln!(control, "{}", std::process::id())?;
    let mut release = [0];
    control.read_exact(&mut release)?;
    ensure!(
        release == [1],
        "launcher requires parent ownership acknowledgement"
    );
    drop(control);
    let error = Command::new(env!("CARGO_BIN_EXE_oxidom"))
        .arg("daemon")
        .exec();
    Err(error).context("exec real standalone daemon")
}

fn registered_launcher(
    listener: &UnixListener,
    actors: &[u32],
) -> Result<(u32, u32, ProcessFd, UnixStream)> {
    let mut accepted = None;
    wait_until(|| match listener.accept() {
        Ok((stream, _)) => {
            accepted = Some(stream);
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error.into()),
    })?;
    let stream = accepted.context("bootstrap registration")?;
    stream.set_read_timeout(Some(LIMIT))?;
    stream.set_write_timeout(Some(LIMIT))?;
    let mut reader = BufReader::new(stream);
    let line = protocol_line(&mut reader, 32)?.context("launcher registration ended early")?;
    let pid: u32 = line.trim().parse()?;
    let process = ProcessFd::open(pid)?;
    ensure!(
        !process.exited()?,
        "registered launcher is alive before release"
    );
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let parent: u32 = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .context("launcher parent")?
        .trim()
        .parse()?;
    ensure!(
        actors.contains(&parent),
        "registered launcher is a child of an owned client"
    );
    // SAFETY: getsid only observes the still-pinned fixture process.
    ensure!(
        unsafe { libc::getsid(pid as i32) } == pid as i32,
        "production bootstrap creates an independent POSIX session"
    );
    Ok((pid, parent, process, reader.into_inner()))
}

#[test]
fn competing_portable_clients_leave_one_standalone_owner_after_both_clients_exit() -> Result<()> {
    let _serial = serial();
    let _subreaper = Subreaper::install()?;
    let harness = Harness::new()?;
    let adoption = AdoptedChildren::new(&harness.bus)?;
    let launcher = launcher_file(&harness)?;
    let listener = UnixListener::bind(harness.root.0.join("bootstrap"))?;
    listener.set_nonblocking(true)?;
    let mut first = Actor::start(&harness, "bootstrap-first", Some(&launcher))?;
    let mut second = Actor::start(&harness, "bootstrap-second", Some(&launcher))?;
    // Both clients see an absent service and reach the real bootstrap path;
    // neither launcher can own the bus before both processes are registered.
    first.send("connect")?;
    second.send("connect")?;
    let actors = [first.child.child.id(), second.child.child.id()];
    let (first_pid, first_parent, first_process, mut first_gate) =
        registered_launcher(&listener, &actors)?;
    let (second_pid, second_parent, second_process, mut second_gate) =
        registered_launcher(&listener, &actors)?;
    ensure!(
        first_pid != second_pid,
        "two distinct bootstrap attempts reached the gate"
    );
    ensure!(
        first_parent != second_parent,
        "each portable client starts exactly one gated daemon"
    );
    first_gate.write_all(&[1])?;
    second_gate.write_all(&[1])?;
    drop(first_gate);
    drop(second_gate);
    ensure!(
        first.receive()?["event"] == "hydrated",
        "first portable client reaches the winning owner"
    );
    ensure!(
        second.receive()?["event"] == "hydrated",
        "losing starter discovers the winning owner"
    );
    let connection = harness.connection()?;
    let bus = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )?;
    let owner: String = bus.call("GetNameOwner", &(BUS_NAME,))?;
    let owner_pid: u32 = bus.call("GetConnectionUnixProcessID", &(owner.as_str(),))?;
    let proxy = zbus::blocking::Proxy::new_owned(
        connection.clone(),
        owner.clone(),
        OBJECT_PATH,
        INTERFACE,
    )?;
    let identity: DaemonInfo = json_call(&proxy, "DaemonInfo")?;
    identity.validate()?;
    let (winner, loser) = if owner_pid == first_pid {
        (&first_process, &second_process)
    } else {
        ensure!(
            owner_pid == second_pid,
            "the winner is one of the registered launchers"
        );
        (&second_process, &first_process)
    };
    loser.wait(LIMIT)?;
    ensure!(
        !winner.exited()?,
        "only the winning standalone daemon remains live"
    );
    let members = import_work(&proxy)?;
    harness.cli(&["up", "work"])?;
    ready_sessions(&proxy, &["work"])?;
    let (core_pid, core) = harness.core("work")?;
    for actor in [&mut first, &mut second] {
        let observed = actor.ask("observe")?;
        ensure!(
            observed["event"] == "hydrated"
                && observed["subscriptions"]
                    .as_u64()
                    .is_some_and(|count| count > 0)
                && observed["profile_names"]
                    .as_array()
                    .is_some_and(|names| names.iter().any(|name| name == "work"))
                && observed["sessions"]
                    .as_array()
                    .is_some_and(|sessions| sessions
                        .iter()
                        .any(|session| session["profile"] == "work"
                            && session["state"] == "connected"
                            && session["server_id"] == members[0])),
            "each original public client observes the winning owner's live session and catalog"
        );
    }
    first.finish()?;
    second.finish()?;
    ensure!(
        !winner.exited()?,
        "closing both clients does not terminate their standalone daemon"
    );
    let current_owner: String = bus.call("GetNameOwner", &(BUS_NAME,))?;
    ensure!(
        current_owner == owner && json_call::<DaemonInfo>(&proxy, "DaemonInfo")? == identity,
        "client exit causes no owner replacement or automatic respawn"
    );
    ensure!(
        !core.exited()? && harness.core("work")?.0 == core_pid,
        "both client exits preserve the original core"
    );
    ready_sessions(&proxy, &["work"])?;
    winner.signal(libc::SIGTERM)?;
    winner.wait(LIMIT)?;
    core.wait(LIMIT)?;
    core.was_reaped_by_daemon()?;
    reap_orphan(owner_pid, winner)?;
    adoption.require_drained(&[first_pid, second_pid])?;
    Ok(())
}
