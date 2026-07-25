use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::LatencyMethod;
use crate::model::{Protocol, Server};

const TIMEOUT: Duration = Duration::from_secs(3);

/// Measure latency for `server` using `method`. `socks_port` is used by the
/// HTTP methods, which route through the active Xray SOCKS inbound.
pub fn measure(
    server: &Server,
    method: LatencyMethod,
    socks_port: u16,
    test_url: &str,
) -> Option<u32> {
    match method {
        LatencyMethod::Icmp => icmp_ping(&server.address),
        LatencyMethod::Tcp => direct_ping(server),
        LatencyMethod::HttpHead => {
            if socks_up(socks_port) {
                http_ping(socks_port, test_url, "HEAD")
            } else {
                direct_ping(server)
            }
        }
        LatencyMethod::HttpGet => {
            if socks_up(socks_port) {
                http_ping(socks_port, test_url, "GET")
            } else {
                direct_ping(server)
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

fn socks_up(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &(std::net::Ipv4Addr::LOCALHOST, port).into(),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
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
