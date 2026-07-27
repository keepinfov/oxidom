use std::error::Error as _;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{Config, LatencyMethod};
use crate::model::{Protocol, Server};
use crate::xray::{config as xray_config, resolve};
use crate::{fsutil, paths};

const TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for a freshly spawned core to bind its SOCKS inbound.
const SOCKS_READY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a throwaway probe core gets to come up. Shorter than the one above:
/// that one waits on the connection the user is watching, this one is holding
/// up a queue of other servers.
const PROBE_CORE_READY_TIMEOUT: Duration = Duration::from_secs(4);
/// Budget for a request through a core that has only just started. Twice
/// [`TIMEOUT`], because the number it produces is not just a request: it pays
/// for DNS, the outbound handshake and the TLS session, none of which a live
/// tunnel repeats. Measured against a healthy REALITY server, a first request
/// lands around 1.5 s, so a 3 s budget reported working servers as dead often
/// enough to see it happen twice in a row.
const REAL_DELAY_TIMEOUT: Duration = Duration::from_secs(6);

/// How a probe should reach the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Straight at the server. The only thing a per-server reading in the
    /// list can mean: every card must be measured on its own merits.
    Direct,
    /// Through the local SOCKS inbound, i.e. through whatever server is
    /// currently active. Only ever valid for that one server.
    Proxied,
}

/// A reading and the method that actually produced it.
///
/// The two halves exist because the second is not always the method that was
/// asked for, and a number whose provenance is thrown away cannot be told apart
/// from one that means what the user expects it to mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    pub ms: u32,
    pub method: LatencyMethod,
}

/// Why a probe produced no number. Everything except `Reachable` used to be
/// `None`, which made a dead network look exactly like a dead server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    Reachable(Measurement),
    /// The server answered the door and then refused, or never answered.
    Unreachable,
    Timeout,
    /// Not the server's fault: nothing on this machine can reach anything.
    NoNetwork,
    /// Not the server's fault either, and not the network's: no core binary,
    /// no free port, no writable data dir.
    Internal(&'static str),
}

/// Measure latency for `server` with the configured method, reporting how it
/// was really measured.
///
/// The HTTP methods measure what the user actually cares about — whether a
/// request gets through, and how slowly — which a TCP handshake cannot answer:
/// a censored server completes the handshake and then drops the session, and
/// the port may not even be the one carrying traffic (hysteria2 is QUIC over
/// UDP). So a direct HTTP probe gives the server a core of its own and makes
/// the request through it, rather than measuring the tunnel some *other*
/// server happens to be running or degrading to a connect() nobody asked for.
///
/// `Route::Proxied` goes through the tunnel already in use, and never falls
/// back to a direct probe: that would report a healthy number for a tunnel
/// carrying nothing.
pub fn measure(server: &Server, config: &Config, route: Route, bind: Ipv4Addr) -> ProbeOutcome {
    let method = config.latency_method;
    match method {
        LatencyMethod::Icmp => icmp_ping(&server.address),
        LatencyMethod::Tcp => direct_ping(server),
        LatencyMethod::HttpHead | LatencyMethod::HttpGet => {
            let verb = if method == LatencyMethod::HttpHead {
                "HEAD"
            } else {
                "GET"
            };
            match route {
                Route::Proxied => http_ping(
                    bind,
                    config.socks_port,
                    &config.latency_test_url,
                    verb,
                    TIMEOUT,
                ),
                Route::Direct => real_delay(server, config, verb),
            }
        }
    }
}

/// Time a request made *through* `server`, over a core started for this probe
/// alone.
///
/// This is the only measurement that exercises the same path traffic will take:
/// the outbound handshake, the TLS/REALITY negotiation, and a request the
/// censor gets to see. Cores are torn down as soon as the number is in, and the
/// daemon's probe queue caps how many run at once.
fn real_delay(server: &Server, config: &Config, verb: &str) -> ProbeOutcome {
    let core = match ProbeCore::start(server, config) {
        Ok(core) => core,
        Err(outcome) => return outcome,
    };
    if !socks_ready(
        Ipv4Addr::LOCALHOST,
        core.socks_port,
        PROBE_CORE_READY_TIMEOUT,
    ) {
        log::debug!(
            "probe core for {} never bound its inbound within {}s",
            server.address,
            PROBE_CORE_READY_TIMEOUT.as_secs()
        );
        return ProbeOutcome::Timeout;
    }
    http_ping(
        Ipv4Addr::LOCALHOST,
        core.socks_port,
        &config.latency_test_url,
        verb,
        REAL_DELAY_TIMEOUT,
    )
}

/// A core running for the length of one probe, on ports nothing else uses.
///
/// Owns its config file as well as its process: the file carries the server's
/// credentials, and a probe that returns early — or panics — must not leave
/// either behind.
struct ProbeCore {
    child: Child,
    config_path: PathBuf,
    socks_port: u16,
}

impl ProbeCore {
    fn start(server: &Server, config: &Config) -> Result<ProbeCore, ProbeOutcome> {
        let xray = match resolve::resolve(&config.xray_binary) {
            Ok(xray) => xray,
            Err(error) => {
                // Not the server's fault, and worth saying out loud: without a
                // core nothing can be measured and nothing can be connected.
                log::warn!("cannot probe through a server without an Xray core: {error:#}");
                return Err(ProbeOutcome::Internal("no xray core"));
            }
        };
        // Ports of its own, so a probe never collides with the live tunnel or
        // with another probe. Asking the OS for a free one and then handing it
        // to the core leaves a gap where something else could take it; the core
        // exiting immediately is then indistinguishable from an unreachable
        // server, which is why the gap is kept as short as possible.
        let socks_port = free_port().map_err(|_| ProbeOutcome::Internal("no free port"))?;
        let http_port = free_port().map_err(|_| ProbeOutcome::Internal("no free port"))?;
        let generated = xray_config::generate(server, Ipv4Addr::LOCALHOST, socks_port, http_port);
        let config_path = paths::data_dir()
            .map_err(|_| ProbeOutcome::Internal("cannot stage a probe config"))?
            .join(format!("probe-{socks_port}.json"));
        let body = serde_json::to_string(&generated)
            .map_err(|_| ProbeOutcome::Internal("cannot stage a probe config"))?;
        if let Err(error) = fsutil::write_private_atomic(&config_path, body.as_bytes()) {
            log::warn!("could not write a probe config: {error:#}");
            return Err(ProbeOutcome::Internal("cannot stage a probe config"));
        }
        match Command::new(&xray.path)
            .arg("run")
            .arg("-c")
            .arg(&config_path)
            // A probe core's log is nobody's business: piping it would mean
            // draining two more pipes per server just to throw the bytes away.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => Ok(ProbeCore {
                child,
                config_path,
                socks_port,
            }),
            Err(error) => {
                log::warn!(
                    "could not start a probe core ({}): {error}",
                    xray.path.display()
                );
                remove_quietly(&config_path);
                Err(ProbeOutcome::Internal("cannot start a probe core"))
            }
        }
    }
}

impl Drop for ProbeCore {
    fn drop(&mut self) {
        // Killed outright rather than asked to stop: a probe core carries no
        // state worth flushing, and the grace period the live core gets would
        // be paid on every server in the list.
        let _ = self.child.kill();
        let _ = self.child.wait();
        remove_quietly(&self.config_path);
    }
}

fn remove_quietly(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::debug!("could not remove {}: {error}", path.display());
    }
}

/// A port the OS says is free right now.
fn free_port() -> io::Result<u16> {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
}

/// Reach the server without going through the tunnel.
///
/// Hysteria2 is QUIC over UDP, so a TCP connect to its port proves nothing and
/// would report every healthy server as unreachable. Try TCP anyway — plenty of
/// deployments co-host a masquerade site on the same port — and fall back to
/// ICMP, which at least distinguishes a dead host from a live one.
fn direct_ping(server: &Server) -> ProbeOutcome {
    let tcp = tcp_ping(&server.address, server.port);
    if server.protocol != Protocol::Hysteria2 {
        return tcp;
    }
    match tcp {
        ProbeOutcome::Unreachable | ProbeOutcome::Timeout => icmp_ping(&server.address),
        outcome => outcome,
    }
}

/// Is the local SOCKS inbound accepting connections?
pub fn socks_up(address: Ipv4Addr, port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &SocketAddrV4::new(address, port).into(),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
}

/// Wait for the core to bind its SOCKS inbound after a spawn. The process
/// being alive says nothing about whether it is carrying traffic yet, so this
/// is what "connected" actually rests on.
pub fn wait_for_socks(address: Ipv4Addr, port: u16) -> bool {
    socks_ready(address, port, SOCKS_READY_TIMEOUT)
}

fn socks_ready(address: Ipv4Addr, port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if socks_up(address, port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Raw TCP connect latency to host:port.
pub fn tcp_ping(host: &str, port: u16) -> ProbeOutcome {
    let address = match (host, port).to_socket_addrs() {
        Ok(mut addresses) => match addresses.next() {
            Some(address) => address,
            None => return dns_failure(),
        },
        Err(_) => return dns_failure(),
    };
    let start = Instant::now();
    match TcpStream::connect_timeout(&address, TIMEOUT) {
        Ok(_) => ProbeOutcome::Reachable(Measurement {
            ms: start.elapsed().as_millis() as u32,
            method: LatencyMethod::Tcp,
        }),
        Err(error) => classify_io_error(error.kind(), error.raw_os_error()),
    }
}

/// ICMP via the `ping` command (avoids raw-socket privileges).
pub fn icmp_ping(host: &str) -> ProbeOutcome {
    let output = match Command::new("ping")
        .args(["-c", "1", "-W", "1", host])
        .output()
    {
        Ok(output) => output,
        Err(_) => return ProbeOutcome::Internal("ping is not installed"),
    };
    match output.status.code() {
        Some(1) => return ProbeOutcome::Unreachable,
        Some(2) => return ProbeOutcome::NoNetwork,
        Some(0) => {}
        _ => return ProbeOutcome::Unreachable,
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Parse "time=12.3 ms".
    let Some(idx) = text.find("time=") else {
        return ProbeOutcome::Unreachable;
    };
    let rest = &text[idx + 5..];
    let num: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    match num.parse::<f64>() {
        Ok(ms) => ProbeOutcome::Reachable(Measurement {
            ms: ms.round() as u32,
            method: LatencyMethod::Icmp,
        }),
        Err(_) => ProbeOutcome::Unreachable,
    }
}

/// Time an HTTP request to `url` routed through the local SOCKS inbound.
pub fn http_ping(
    address: Ipv4Addr,
    socks_port: u16,
    url: &str,
    method: &str,
    timeout: Duration,
) -> ProbeOutcome {
    let proxy_url = format!("socks5://{address}:{socks_port}");
    let proxy = match ureq::Proxy::new(&proxy_url) {
        Ok(proxy) => proxy,
        Err(_) => return ProbeOutcome::Internal("bad proxy url"),
    };
    let agent = ureq::AgentBuilder::new()
        .proxy(proxy)
        .timeout(timeout)
        .build();
    let start = Instant::now();
    match agent.request(method, url).call() {
        // Any response (including an HTTP error status) counts.
        Ok(_) | Err(ureq::Error::Status(_, _)) => ProbeOutcome::Reachable(Measurement {
            ms: start.elapsed().as_millis() as u32,
            method: http_method(method),
        }),
        Err(ureq::Error::Transport(transport)) => classify_http_transport(&transport),
    }
}

fn http_method(method: &str) -> LatencyMethod {
    if method == "HEAD" {
        LatencyMethod::HttpHead
    } else {
        LatencyMethod::HttpGet
    }
}

/// Classify connect failures without performing I/O, so the OS-specific
/// `Uncategorized` fallback is covered by deterministic tests.
fn classify_io_error(kind: io::ErrorKind, raw_os_error: Option<i32>) -> ProbeOutcome {
    match kind {
        io::ErrorKind::TimedOut => ProbeOutcome::Timeout,
        io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::NetworkDown
        | io::ErrorKind::HostUnreachable => ProbeOutcome::NoNetwork,
        io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted => ProbeOutcome::Unreachable,
        _ if matches!(
            raw_os_error,
            Some(libc::ENETUNREACH | libc::ENETDOWN | libc::EHOSTUNREACH)
        ) =>
        {
            ProbeOutcome::NoNetwork
        }
        _ => ProbeOutcome::Unreachable,
    }
}

fn classify_http_transport(transport: &ureq::Transport) -> ProbeOutcome {
    match transport.kind() {
        ureq::ErrorKind::Dns => dns_failure(),
        ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::ProxyConnect => {
            ProbeOutcome::Unreachable
        }
        ureq::ErrorKind::Io if transport_timed_out(transport) => ProbeOutcome::Timeout,
        _ => ProbeOutcome::Unreachable,
    }
}

fn transport_timed_out(transport: &ureq::Transport) -> bool {
    let mut source = transport.source();
    while let Some(error) = source {
        if error
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::TimedOut)
        {
            return true;
        }
        source = error.source();
    }
    false
}

fn dns_failure() -> ProbeOutcome {
    if default_route_present() {
        ProbeOutcome::Unreachable
    } else {
        ProbeOutcome::NoNetwork
    }
}

/// Is there any default route at all? A probe failure means something very
/// different when the machine has no way out in the first place, and asking
/// the routing table costs one file read — no packets, no resolver.
fn default_route_present() -> bool {
    let tables = [
        std::fs::read_to_string("/proc/net/route"),
        std::fs::read_to_string("/proc/net/ipv6_route"),
    ];
    let mut read_any = false;
    for table in tables.into_iter().flatten() {
        read_any = true;
        if has_default_route(&table) {
            return true;
        }
    }
    // An unreadable /proc tells us nothing. Calling that "offline" would put
    // blame on the machine without evidence, which is worse than missing it.
    !read_any
}

fn has_default_route(proc_net_route: &str) -> bool {
    proc_net_route.lines().any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        let (destination, flags) = match fields.as_slice() {
            // /proc/net/route: Iface Destination Gateway Flags ...
            [_, destination, _, flags, ..] if *destination == "00000000" => (*destination, *flags),
            // /proc/net/ipv6_route: Destination Prefix Source Prefix ...
            [destination, prefix, _, _, _, _, _, _, flags, ..]
                if destination.len() == 32
                    && destination.bytes().all(|byte| byte == b'0')
                    && *prefix == "00" =>
            {
                (*destination, *flags)
            }
            _ => return false,
        };
        !destination.is_empty()
            && u32::from_str_radix(flags, 16)
                .is_ok_and(|flags| flags & u32::from(libc::RTF_UP) != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_ports_are_bindable_and_distinct() {
        let first = free_port().expect("the OS has a spare port");
        let second = free_port().expect("the OS has a second spare port");
        assert_ne!(
            first, second,
            "two probes running at once must not be handed the same inbound"
        );
        // The core is going to bind it a moment later; if we cannot, neither
        // can it, and every probe would report an unreachable server.
        TcpListener::bind(("127.0.0.1", first)).expect("the port we picked is free");
    }

    #[test]
    fn a_missing_default_route_reads_as_no_network() {
        let without_default = "\
Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT
eth0 0000FEA9 00000000 0001 0 0 100 00FFFFFF 0 0 0
";
        let with_default = "\
Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT
eth0 00000000 0100A8C0 0003 0 0 100 00000000 0 0 0
";
        assert!(!has_default_route(without_default));
        assert!(has_default_route(with_default));
    }

    #[test]
    fn a_refused_connection_is_the_servers_fault() -> io::Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);

        assert_eq!(tcp_ping("127.0.0.1", port), ProbeOutcome::Unreachable);
        Ok(())
    }

    #[test]
    fn an_unroutable_address_is_not_the_servers_fault() {
        assert_eq!(
            classify_io_error(io::ErrorKind::Other, Some(libc::ENETUNREACH)),
            ProbeOutcome::NoNetwork
        );
        assert_eq!(
            classify_io_error(io::ErrorKind::ConnectionRefused, Some(libc::ECONNREFUSED)),
            ProbeOutcome::Unreachable
        );
    }

    /// The measurement the HTTP methods exist for: a request carried *by the
    /// server*, which is the only thing that catches one that finishes a
    /// handshake and then passes no traffic.
    ///
    /// Needs a real server, a core and a network, so it is opt-in:
    ///
    /// ```text
    /// OXIDOM_TEST_LINK='vless://…' cargo test -- --ignored real_delay
    /// ```
    ///
    /// `OXIDOM_TEST_SERVER_JSON` takes one entry out of `subscriptions.json`
    /// instead, which is the only way to test a server whose share link cannot
    /// be reconstructed by hand without losing half its settings.
    #[test]
    #[ignore = "requires a server in OXIDOM_TEST_LINK, an xray binary and a network"]
    fn a_real_delay_probe_measures_through_the_server() {
        let server = match std::env::var("OXIDOM_TEST_SERVER_JSON") {
            Ok(json) => serde_json::from_str(&json).expect("the server parses"),
            Err(_) => {
                let link = std::env::var("OXIDOM_TEST_LINK").expect("OXIDOM_TEST_LINK");
                crate::link::parse_link(&link).expect("the link parses")
            }
        };
        let ProbeOutcome::Reachable(measured) = measure(
            &server,
            &Config::default(),
            Route::Direct,
            Ipv4Addr::LOCALHOST,
        ) else {
            panic!("the server did not carry the request");
        };
        assert_eq!(
            measured.method,
            LatencyMethod::HttpGet,
            "a direct HTTP probe must report the method it was asked for, \
             not a handshake standing in for it"
        );
        assert!(measured.ms > 0, "a request through a server takes time");
        // Printed because the number is the point of running this by hand.
        println!("{} answered in {} ms", server.address, measured.ms);
    }
}
