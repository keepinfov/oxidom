use std::net::{TcpListener, TcpStream, ToSocketAddrs};
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
pub fn measure(server: &Server, config: &Config, route: Route) -> Option<Measurement> {
    let method = config.latency_method;
    match method {
        LatencyMethod::Icmp => icmp_ping(&server.address).map(|ms| Measurement {
            ms,
            method: LatencyMethod::Icmp,
        }),
        LatencyMethod::Tcp => direct_ping(server),
        LatencyMethod::HttpHead | LatencyMethod::HttpGet => {
            let verb = if method == LatencyMethod::HttpHead {
                "HEAD"
            } else {
                "GET"
            };
            let ms = match route {
                Route::Proxied => {
                    http_ping(config.socks_port, &config.latency_test_url, verb, TIMEOUT)
                }
                Route::Direct => real_delay(server, config, verb),
            };
            ms.map(|ms| Measurement { ms, method })
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
fn real_delay(server: &Server, config: &Config, verb: &str) -> Option<u32> {
    let core = ProbeCore::start(server, config)?;
    if !socks_ready(core.socks_port, PROBE_CORE_READY_TIMEOUT) {
        log::debug!(
            "probe core for {} never bound its inbound within {}s",
            server.address,
            PROBE_CORE_READY_TIMEOUT.as_secs()
        );
        return None;
    }
    http_ping(
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
    fn start(server: &Server, config: &Config) -> Option<ProbeCore> {
        let xray = match resolve::resolve(&config.xray_binary) {
            Ok(xray) => xray,
            Err(error) => {
                // Not the server's fault, and worth saying out loud: without a
                // core nothing can be measured and nothing can be connected.
                log::warn!("cannot probe through a server without an Xray core: {error:#}");
                return None;
            }
        };
        // Ports of its own, so a probe never collides with the live tunnel or
        // with another probe. Asking the OS for a free one and then handing it
        // to the core leaves a gap where something else could take it; the core
        // exiting immediately is then indistinguishable from an unreachable
        // server, which is why the gap is kept as short as possible.
        let socks_port = free_port()?;
        let http_port = free_port()?;
        let generated = xray_config::generate(server, socks_port, http_port);
        let config_path = paths::data_dir()
            .ok()?
            .join(format!("probe-{socks_port}.json"));
        let body = serde_json::to_string(&generated).ok()?;
        if let Err(error) = fsutil::write_private_atomic(&config_path, body.as_bytes()) {
            log::warn!("could not write a probe config: {error:#}");
            return None;
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
            Ok(child) => Some(ProbeCore {
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
                None
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
fn free_port() -> Option<u16> {
    let port = TcpListener::bind(("127.0.0.1", 0))
        .ok()?
        .local_addr()
        .ok()?
        .port();
    Some(port)
}

/// Reach the server without going through the tunnel.
///
/// Hysteria2 is QUIC over UDP, so a TCP connect to its port proves nothing and
/// would report every healthy server as unreachable. Try TCP anyway — plenty of
/// deployments co-host a masquerade site on the same port — and fall back to
/// ICMP, which at least distinguishes a dead host from a live one.
fn direct_ping(server: &Server) -> Option<Measurement> {
    let tcp = tcp_ping(&server.address, server.port);
    if tcp.is_some() || server.protocol != Protocol::Hysteria2 {
        return tcp.map(|ms| Measurement {
            ms,
            method: LatencyMethod::Tcp,
        });
    }
    icmp_ping(&server.address).map(|ms| Measurement {
        ms,
        method: LatencyMethod::Icmp,
    })
}

/// Is the local SOCKS inbound accepting connections?
pub fn socks_up(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &(std::net::Ipv4Addr::LOCALHOST, port).into(),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
}

/// Wait for the core to bind its SOCKS inbound after a spawn. The process
/// being alive says nothing about whether it is carrying traffic yet, so this
/// is what "connected" actually rests on.
pub fn wait_for_socks(port: u16) -> bool {
    socks_ready(port, SOCKS_READY_TIMEOUT)
}

fn socks_ready(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if socks_up(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Raw TCP connect latency to host:port.
pub fn tcp_ping(host: &str, port: u16) -> Option<u32> {
    let addr = (host, port).to_socket_addrs().ok()?.next()?;
    let start = Instant::now();
    TcpStream::connect_timeout(&addr, TIMEOUT).ok()?;
    Some(start.elapsed().as_millis() as u32)
}

/// ICMP via the `ping` command (avoids raw-socket privileges).
pub fn icmp_ping(host: &str) -> Option<u32> {
    let out = Command::new("ping")
        .args(["-c", "1", "-W", "1", host])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Parse "time=12.3 ms".
    let idx = text.find("time=")?;
    let rest = &text[idx + 5..];
    let num: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num.parse::<f64>().ok().map(|ms| ms.round() as u32)
}

/// Time an HTTP request to `url` routed through the local SOCKS inbound.
pub fn http_ping(socks_port: u16, url: &str, method: &str, timeout: Duration) -> Option<u32> {
    let proxy_url = format!("socks5://127.0.0.1:{socks_port}");
    let proxy = ureq::Proxy::new(&proxy_url).ok()?;
    let agent = ureq::AgentBuilder::new()
        .proxy(proxy)
        .timeout(timeout)
        .build();
    let start = Instant::now();
    let resp = agent.request(method, url).call().ok()?;
    // Any response (including 204) counts.
    let _ = resp.status();
    Some(start.elapsed().as_millis() as u32)
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
        let measured = measure(&server, &Config::default(), Route::Direct)
            .expect("the server carried the request");
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
