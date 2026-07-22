use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::LatencyMethod;
use crate::model::Server;

const TIMEOUT: Duration = Duration::from_secs(3);

/// Measure latency for `server` using `method`. `socks_port` is used by the
/// HTTP methods, which route through the active Xray SOCKS inbound.
pub fn measure(server: &Server, method: LatencyMethod, socks_port: u16, test_url: &str) -> Option<u32> {
    match method {
        LatencyMethod::Icmp => icmp_ping(&server.address),
        LatencyMethod::Tcp => tcp_ping(&server.address, server.port),
        LatencyMethod::HttpHead => http_ping(socks_port, test_url, "HEAD"),
        LatencyMethod::HttpGet => http_ping(socks_port, test_url, "GET"),
    }
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
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
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
