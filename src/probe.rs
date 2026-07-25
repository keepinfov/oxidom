use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::LatencyMethod;
use crate::model::{Protocol, Server};

const TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for a freshly spawned core to bind its SOCKS inbound.
const SOCKS_READY_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Measure latency for `server` using `method`.
///
/// `Route::Proxied` is what the HTTP methods were designed for (Happ-style:
/// fetch a test URL through the tunnel), but it measures the *active* server
/// no matter which one is passed in — so the list uses `Route::Direct`, where
/// the HTTP methods degrade to a TCP connect against that specific server.
/// A proxied probe never falls back to a direct one: doing so would report a
/// healthy number for a tunnel that is not carrying traffic.
pub fn measure(
    server: &Server,
    method: LatencyMethod,
    socks_port: u16,
    test_url: &str,
    route: Route,
) -> Option<u32> {
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
                Route::Proxied => http_ping(socks_port, test_url, verb),
                Route::Direct => direct_ping(server),
            }
        }
    }
}

/// Reach the server without going through the tunnel.
///
/// Hysteria2 is QUIC over UDP, so a TCP connect to its port proves nothing and
/// would report every healthy server as unreachable. Try TCP anyway — plenty of
/// deployments co-host a masquerade site on the same port — and fall back to
/// ICMP, which at least distinguishes a dead host from a live one.
fn direct_ping(server: &Server) -> Option<u32> {
    let tcp = tcp_ping(&server.address, server.port);
    if tcp.is_some() || server.protocol != Protocol::Hysteria2 {
        return tcp;
    }
    icmp_ping(&server.address)
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
    let deadline = Instant::now() + SOCKS_READY_TIMEOUT;
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
pub fn http_ping(socks_port: u16, url: &str, method: &str) -> Option<u32> {
    let proxy_url = format!("socks5://127.0.0.1:{socks_port}");
    let proxy = ureq::Proxy::new(&proxy_url).ok()?;
    let agent = ureq::AgentBuilder::new()
        .proxy(proxy)
        .timeout(TIMEOUT)
        .build();
    let start = Instant::now();
    let resp = agent.request(method, url).call().ok()?;
    // Any response (including 204) counts.
    let _ = resp.status();
    Some(start.elapsed().as_millis() as u32)
}
