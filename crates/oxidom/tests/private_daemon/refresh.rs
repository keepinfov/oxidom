use super::*;

use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread::JoinHandle;

use oxidom_core::model::Subscription;

const ORIGINAL: &str = "trojan://invented-alpha@alpha.example.invalid:443#Alpha\ntrojan://invented-beta@beta.example.invalid:443#Beta\n";
const UPDATED: &str = "trojan://invented-alpha@alpha.example.invalid:443#Alpha\ntrojan://invented-beta@beta.example.invalid:443#Beta\ntrojan://invented-gamma@gamma.example.invalid:443#Gamma\n";
const REPLACED: &str = "trojan://invented-alpha@alpha.example.invalid:443#Alpha\ntrojan://invented-beta@beta.example.invalid:443#Beta\ntrojan://invented-gamma@gamma.example.invalid:443#Gamma\ntrojan://invented-delta@delta.example.invalid:443#Delta\n";

enum Reply {
    Body(&'static str),
    Abort,
}

/// A real HTTP provider, with its response held until control assertions finish.
/// Shutdown wakes both an idle accept and a request already waiting at the gate.
struct Provider {
    url: String,
    commands: mpsc::Sender<Reply>,
    entered: mpsc::Receiver<usize>,
    cancel: UnixStream,
    worker: Option<JoinHandle<Result<()>>>,
}

impl Provider {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}/invented-subscription", listener.local_addr()?);
        let (cancel, waiting) = UnixStream::pair()?;
        let (commands, receive) = mpsc::channel();
        let (send, entered) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            for index in 0..3 {
                let Some(mut stream) = Self::accept(&listener, &waiting)? else {
                    return Ok(());
                };
                Self::request(&mut stream)?;
                let body = if index == 0 {
                    ORIGINAL
                } else {
                    send.send(index)?;
                    // Longer than the CLI wait budget: an autonomous provider
                    // timeout must not make a blocked Stop appear responsive.
                    match receive.recv_timeout(LIMIT * 4)? {
                        Reply::Body(body) => body,
                        Reply::Abort => return Ok(()),
                    }
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )?;
            }
            Ok(())
        });
        Ok(Self {
            url,
            commands,
            entered,
            cancel,
            worker: Some(worker),
        })
    }

    fn accept(listener: &TcpListener, cancel: &UnixStream) -> Result<Option<TcpStream>> {
        let deadline = Instant::now() + LIMIT * 4;
        loop {
            let mut descriptors = [
                libc::pollfd {
                    fd: listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: cancel.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let remaining = deadline.saturating_duration_since(Instant::now());
            // SAFETY: poll receives two initialized descriptors that remain owned.
            let result = unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    2,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if result < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            ensure!(result > 0, "private provider accept timed out");
            if descriptors[1].revents != 0 {
                return Ok(None);
            }
            if descriptors[0].revents & libc::POLLIN != 0 {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        ensure!(
                            peer.ip().is_loopback(),
                            "provider accepted a non-loopback peer"
                        );
                        return Ok(Some(stream));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }

    fn request(stream: &mut TcpStream) -> Result<()> {
        stream.set_write_timeout(Some(LIMIT))?;
        let deadline = Instant::now() + LIMIT;
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            ensure!(
                bytes.len() < 8192,
                "private provider request exceeded its bound"
            );
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "private provider request timed out");
            stream.set_read_timeout(Some(remaining))?;
            let mut byte = [0];
            stream.read_exact(&mut byte)?;
            bytes.push(byte[0]);
        }
        let headers = std::str::from_utf8(&bytes)?;
        ensure!(
            headers.starts_with("GET /invented-subscription HTTP/1.1\r\n"),
            "unexpected private provider request"
        );
        ensure!(
            !headers.to_ascii_lowercase().contains("\r\nx-hwid:"),
            "disabled HWID reached the provider"
        );
        Ok(())
    }

    fn wait_request(&self, index: usize) -> Result<()> {
        ensure!(
            self.entered.recv_timeout(LIMIT)? == index,
            "provider request order changed"
        );
        Ok(())
    }

    fn respond(&self, body: &'static str) -> Result<()> {
        self.commands.send(Reply::Body(body))?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.worker
            .take()
            .context("provider already joined")?
            .join()
            .map_err(|_| anyhow::anyhow!("provider worker panicked"))?
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.commands.send(Reply::Abort);
        let _ = self.cancel.write_all(&[1]);
        if let Some(worker) = self.worker.take() {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("private provider cleanup: {error:#}"),
                Err(_) => eprintln!("private provider worker panicked during cleanup"),
            }
        }
    }
}

/// Abort releases the provider before joining, including on assertion unwind.
struct Refresh {
    abort: mpsc::Sender<Reply>,
    done: mpsc::Receiver<()>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl Refresh {
    fn start(harness: &Harness, owner: &str, id: &str, provider: &Provider) -> Result<Self> {
        let connection =
            zbus::blocking::connection::Builder::address(harness.bus.address.as_str())?
                .method_timeout(LIMIT * 3)
                .build()?;
        let proxy = zbus::blocking::Proxy::new_owned(
            connection,
            owner.to_string(),
            OBJECT_PATH,
            INTERFACE,
        )?;
        let id = id.to_string();
        let (send, done) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = proxy
                .call::<_, _, ()>("Refresh", &(id,))
                .map_err(anyhow::Error::new);
            let _ = send.send(());
            result
        });
        Ok(Self {
            abort: provider.commands.clone(),
            done,
            worker: Some(worker),
        })
    }

    fn pending(&self) -> Result<()> {
        ensure!(
            matches!(self.done.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "refresh completed before provider release"
        );
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.done.recv_timeout(LIMIT)?;
        self.worker
            .take()
            .context("refresh already joined")?
            .join()
            .map_err(|_| anyhow::anyhow!("refresh worker panicked"))?
    }
}

impl Drop for Refresh {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.abort.send(Reply::Abort);
            let _ = worker.join();
        }
    }
}

fn catalog(proxy: &zbus::blocking::Proxy<'_>) -> Result<Vec<Subscription>> {
    json_call(proxy, "ListSubscriptions")
}

fn saved(proxy: &zbus::blocking::Proxy<'_>, name: &str) -> Result<Profile> {
    let text: String = proxy.call("GetProfile", &(name,))?;
    Ok(serde_json::from_str(&text)?)
}

fn save(proxy: &zbus::blocking::Proxy<'_>, name: &str, profile: &Profile) -> Result<()> {
    Ok(proxy.call("SaveProfile", &(name, serde_json::to_string(profile)?))?)
}

fn assert_peer(
    harness: &Harness,
    proxy: &zbus::blocking::Proxy<'_>,
    pid: u32,
    core: &ProcessFd,
    id: &str,
) -> Result<()> {
    ensure!(
        !core.exited()?,
        "peer core stopped during another profile's update"
    );
    ensure!(
        harness
            .state()?
            .sessions
            .iter()
            .find(|session| session.profile == "home")
            .and_then(|session| session.xray_pid)
            == Some(pid),
        "refresh changed the peer core incarnation"
    );
    let current = sessions(proxy)?;
    let peer = current
        .iter()
        .find(|session| session.profile == "home")
        .context("peer session")?;
    ensure!(
        peer.state == "connected" && peer.server_id.as_deref() == Some(id),
        "peer facts changed during refresh/Stop"
    );
    ensure!(
        peer.runtime
            .as_ref()
            .context("peer runtime")?
            .failover
            .is_none(),
        "refresh enrolled the unarmed peer in failover"
    );
    Ok(())
}

#[test]
fn delayed_provider_replies_cannot_restore_a_stopped_or_superseded_activation() -> Result<()> {
    let _serial = serial();
    let _subreaper = Subreaper::install()?;
    let harness = Harness::new()?;
    let (mut daemon, proxy, _) = harness.daemon()?;
    let mut provider = Provider::start()?;
    let _: () = proxy.call(
        "AddSubscription",
        &(&provider.url, "Invented provider", false),
    )?;
    let initial = catalog(&proxy)?;
    let subscription = initial
        .iter()
        .find(|subscription| subscription.url == provider.url)
        .context("private subscription")?;
    let id = subscription.id.clone();
    let members = subscription
        .servers
        .iter()
        .map(|server| server.id.clone())
        .collect::<Vec<_>>();
    ensure!(members.len() == 2, "initial provider list is fixed");
    let imported: (u32, u32) = proxy.call(
        "ImportLinks",
        &("trojan://invented-peer@peer.example.invalid:443#Peer",),
    )?;
    ensure!(imported.0 == 1, "one independent peer was imported");
    let peer = catalog(&proxy)?
        .into_iter()
        .flat_map(|subscription| subscription.servers)
        .find(|server| server.name == "Peer")
        .context("independent peer")?
        .id;
    let mut work = saved(&proxy, "default")?;
    work.select.server = members[0].clone();
    work.failover = Some(FailoverPolicy {
        enabled: true,
        members: members.clone(),
        max_attempts: 1,
        cooldown_secs: 1,
    });
    save(&proxy, "work", &work)?;
    let mut home = work.clone();
    home.select.server = peer.clone();
    home.failover = None;
    save(&proxy, "home", &home)?;
    harness.cli(&["up", "work"])?;
    harness.cli(&["up", "home"])?;
    ready_sessions(&proxy, &["work", "home"])?;
    let (peer_pid, peer_core) = harness.core("home")?;
    let (_, first_core) = harness.core("work")?;
    let owner = proxy.destination().as_str().to_string();

    let refresh = Refresh::start(&harness, &owner, &id, &provider)?;
    provider.wait_request(1)?;
    refresh.pending()?;
    harness.cli(&["down", "work"])?;
    ensure!(
        sessions(&proxy)?
            .iter()
            .all(|session| session.profile != "work"),
        "Stop must complete while HTTP remains blocked"
    );
    first_core.wait(LIMIT)?;
    assert_peer(&harness, &proxy, peer_pid, &peer_core, &peer)?;
    refresh.pending()?;
    provider.respond(UPDATED)?;
    refresh.finish()?;
    ensure!(
        catalog(&proxy)?
            .iter()
            .find(|subscription| subscription.id == id)
            .context("updated provider")?
            .servers
            .len()
            == 3,
        "late response must update the catalog"
    );
    ensure!(
        sessions(&proxy)?
            .iter()
            .all(|session| session.profile != "work"),
        "late completion resurrected stopped work"
    );
    ensure!(
        saved(&proxy, "work")? == work,
        "refresh cannot rewrite saved policy or enroll Gamma"
    );
    assert_peer(&harness, &proxy, peer_pid, &peer_core, &peer)?;

    // A second response crosses an explicit replacement. The new activation
    // must keep its own core and original grant when the old HTTP work commits.
    harness.cli(&["up", "work"])?;
    ready_sessions(&proxy, &["work", "home"])?;
    let (_, superseded_core) = harness.core("work")?;
    let refresh = Refresh::start(&harness, &owner, &id, &provider)?;
    provider.wait_request(2)?;
    harness.cli(&["down", "work"])?;
    superseded_core.wait(LIMIT)?;
    harness.cli(&["up", "work"])?;
    ready_sessions(&proxy, &["work", "home"])?;
    let (replacement_pid, replacement_core) = harness.core("work")?;
    refresh.pending()?;
    provider.respond(REPLACED)?;
    refresh.finish()?;
    provider.finish()?;
    ensure!(
        catalog(&proxy)?
            .iter()
            .find(|subscription| subscription.id == id)
            .context("replaced provider")?
            .servers
            .len()
            == 4,
        "second late response must commit"
    );
    ensure!(
        !replacement_core.exited()?,
        "late refresh stopped the replacement core"
    );
    ensure!(
        harness
            .state()?
            .sessions
            .iter()
            .find(|session| session.profile == "work")
            .and_then(|session| session.xray_pid)
            == Some(replacement_pid),
        "late refresh changed the replacement incarnation"
    );
    let current = sessions(&proxy)?;
    let activation = current
        .iter()
        .find(|session| session.profile == "work" && session.state == "connected")
        .and_then(|session| session.runtime.as_ref())
        .and_then(|runtime| runtime.failover.as_ref())
        .context("replacement activation")?;
    ensure!(
        activation.state == "armed"
            && activation.members == members
            && activation.current == members[0]
            && activation.attempts == 0,
        "refresh changed the fixed grant or consumed an automatic attempt"
    );
    ensure!(
        saved(&proxy, "work")? == work,
        "provider additions cannot expand saved authorization"
    );
    assert_peer(&harness, &proxy, peer_pid, &peer_core, &peer)?;
    daemon.terminate()?;
    for core in [&first_core, &superseded_core, &replacement_core, &peer_core] {
        core.wait(LIMIT)?;
        core.was_reaped_by_daemon()?;
    }
    daemon.reap_verified_shutdown()?;
    ensure!(
        harness.state()?.sessions.is_empty(),
        "terminal cleanup retained completed sessions"
    );
    Ok(())
}
