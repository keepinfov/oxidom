//! Lightweight direct SOCKS5 and HTTP proxy listener for when no session is up.
//!
//! When `standby_proxy = true` in `config.toml` and every session is
//! neither connected/connecting nor holding traffic after an unexpected core
//! exit, this listener keeps `socks_port` and `http_port` answering on
//! `127.0.0.1` and passes connections straight out over the machine's ordinary
//! network path. Stopping it closes listeners and accepted connections before
//! a session spawns its Xray core.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: usize = 256;
const MAX_HTTP_HEADERS: u64 = 64 * 1024;

#[derive(Default)]
struct Connections {
    stopped: bool,
    next_id: usize,
    sockets: HashMap<usize, Vec<TcpStream>>,
}

struct Connection {
    id: usize,
    connections: Arc<Mutex<Connections>>,
}

impl Connection {
    fn stopped(&self) -> bool {
        crate::sync::lock(&self.connections).stopped
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        crate::sync::lock(&self.connections)
            .sockets
            .remove(&self.id);
    }
}

pub struct StandbyProxy {
    socks_port: u16,
    http_port: u16,
    connections: Arc<Mutex<Connections>>,
    socks_thread: Option<JoinHandle<Vec<JoinHandle<()>>>>,
    http_thread: Option<JoinHandle<Vec<JoinHandle<()>>>>,
}

impl StandbyProxy {
    pub fn start(socks_port: u16, http_port: u16) -> Result<Self> {
        let socks_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, socks_port))
            .with_context(|| format!("binding standby SOCKS listener on 127.0.0.1:{socks_port}"))?;
        let http_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, http_port))
            .with_context(|| format!("binding standby HTTP listener on 127.0.0.1:{http_port}"))?;
        let socks_port = socks_listener.local_addr()?.port();
        let http_port = http_listener.local_addr()?.port();
        let connections = Arc::new(Mutex::new(Connections::default()));
        let stop_socks = connections.clone();
        let socks_thread =
            thread::spawn(move || accept_loop(socks_listener, stop_socks, handle_socks5));
        let stop_http = connections.clone();
        let http_thread = thread::spawn(move || accept_loop(http_listener, stop_http, handle_http));
        Ok(Self {
            socks_port,
            http_port,
            connections,
            socks_thread: Some(socks_thread),
            http_thread: Some(http_thread),
        })
    }

    pub fn ports(&self) -> (u16, u16) {
        (self.socks_port, self.http_port)
    }

    pub fn stop(&mut self) {
        if self.socks_thread.is_none() && self.http_thread.is_none() {
            return;
        }
        {
            let mut connections = crate::sync::lock(&self.connections);
            connections.stopped = true;
            for sockets in connections.sockets.values() {
                for socket in sockets {
                    let _ = socket.shutdown(Shutdown::Both);
                }
            }
        }
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.socks_port)),
            Duration::from_millis(50),
        );
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.http_port)),
            Duration::from_millis(50),
        );
        for handle in [self.socks_thread.take(), self.http_thread.take()]
            .into_iter()
            .flatten()
        {
            if let Ok(workers) = handle.join() {
                for worker in workers {
                    let _ = worker.join();
                }
            }
        }
    }
}

impl Drop for StandbyProxy {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(
    listener: TcpListener,
    connections: Arc<Mutex<Connections>>,
    handler: fn(TcpStream, &Connection) -> io::Result<()>,
) -> Vec<JoinHandle<()>> {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while let Ok((stream, _)) = listener.accept() {
        let mut state = crate::sync::lock(&connections);
        if state.stopped {
            break;
        }
        workers.retain(|worker| !worker.is_finished());
        if state.sockets.len() >= MAX_CONNECTIONS {
            continue;
        }
        let Ok(socket) = stream.try_clone() else {
            continue;
        };
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        state.sockets.insert(id, vec![socket]);
        let connection = Connection {
            id,
            connections: connections.clone(),
        };
        workers.push(thread::spawn(move || {
            let _ = handler(stream, &connection);
        }));
    }
    workers
}

fn handle_socks5(mut client: TcpStream, connection: &Connection) -> io::Result<()> {
    let _ = client.set_read_timeout(Some(CONNECT_TIMEOUT));
    let _ = client.set_write_timeout(Some(CONNECT_TIMEOUT));
    let mut header = [0_u8; 2];
    client.read_exact(&mut header)?;
    if header[0] != 0x05 {
        return Ok(());
    }
    let nmethods = usize::from(header[1]);
    let mut methods = vec![0_u8; nmethods];
    client.read_exact(&mut methods)?;
    if !methods.contains(&0x00) {
        let _ = client.write_all(&[0x05, 0xFF]);
        return Ok(());
    }
    client.write_all(&[0x05, 0x00])?;

    let mut req = [0_u8; 4];
    client.read_exact(&mut req)?;
    if connection.stopped() {
        return Ok(());
    }
    if req[0] != 0x05 || req[1] != 0x01 {
        let _ = client.write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
        return Ok(());
    }
    let target = match req[3] {
        0x01 => {
            let mut ip = [0_u8; 4];
            let mut port = [0_u8; 2];
            client.read_exact(&mut ip)?;
            client.read_exact(&mut port)?;
            vec![SocketAddr::from((ip, u16::from_be_bytes(port)))]
        }
        0x03 => {
            let mut len = [0_u8; 1];
            client.read_exact(&mut len)?;
            let mut domain = vec![0_u8; usize::from(len[0])];
            let mut port = [0_u8; 2];
            client.read_exact(&mut domain)?;
            client.read_exact(&mut port)?;
            let host = String::from_utf8_lossy(&domain);
            match (host.as_ref(), u16::from_be_bytes(port)).to_socket_addrs() {
                Ok(addrs) => addrs.collect(),
                Err(_) => {
                    let _ = client.write_all(&[0x05, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
                    return Ok(());
                }
            }
        }
        0x04 => {
            let mut ip = [0_u8; 16];
            let mut port = [0_u8; 2];
            client.read_exact(&mut ip)?;
            client.read_exact(&mut port)?;
            vec![SocketAddr::from((ip, u16::from_be_bytes(port)))]
        }
        _ => {
            let _ = client.write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
            return Ok(());
        }
    };

    let Some(mut upstream) = connect_any(&target, connection) else {
        let _ = client.write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
        return Ok(());
    };
    client.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])?;
    let _ = client.set_read_timeout(None);
    let _ = client.set_write_timeout(None);
    relay(&mut client, &mut upstream);
    Ok(())
}

fn handle_http(client: TcpStream, connection: &Connection) -> io::Result<()> {
    let _ = client.set_read_timeout(Some(CONNECT_TIMEOUT));
    let _ = client.set_write_timeout(Some(CONNECT_TIMEOUT));
    let mut reader = BufReader::new(client.try_clone()?).take(MAX_HTTP_HEADERS);
    let mut first_line = String::new();
    if reader.read_line(&mut first_line)? == 0 {
        return Ok(());
    }
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }
    let method = parts[0];
    let target = parts[1];
    let version = parts.get(2).copied().unwrap_or("HTTP/1.1");

    let mut headers = Vec::new();
    let mut host_header = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("host")
        {
            host_header = Some(value.trim().to_string());
        }
        headers.push(line);
    }

    if reader.limit() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "HTTP headers too large",
        ));
    }
    let reader = reader.into_inner();
    let buffered = reader.buffer().to_vec();
    let mut client_write = reader.into_inner();
    if connection.stopped() {
        return Ok(());
    }
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(target, 443);
        let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
            let _ = client_write.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
            return Ok(());
        };
        let Some(mut upstream) = connect_any(&addrs.collect::<Vec<_>>(), connection) else {
            let _ = client_write.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
            return Ok(());
        };
        client_write.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        upstream.write_all(&buffered)?;
        let _ = client_write.set_read_timeout(None);
        let _ = client_write.set_write_timeout(None);
        relay(&mut client_write, &mut upstream);
        return Ok(());
    }

    let (authority, path) = if let Some(rest) = target.strip_prefix("http://") {
        match rest.split_once('/') {
            Some((auth, path)) => (auth.to_string(), format!("/{path}")),
            None => (rest.to_string(), "/".to_string()),
        }
    } else if let Some(host) = host_header {
        (host, target.to_string())
    } else {
        let _ = client_write.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        return Ok(());
    };
    let (host, port) = split_host_port(&authority, 80);
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
        let _ = client_write.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
        return Ok(());
    };
    let Some(mut upstream) = connect_any(&addrs.collect::<Vec<_>>(), connection) else {
        let _ = client_write.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
        return Ok(());
    };
    write!(upstream, "{method} {path} {version}\r\n")?;
    for header in headers {
        if header.split_once(':').is_some_and(|(name, _)| {
            [
                "proxy-connection",
                "proxy-authorization",
                "proxy-authenticate",
                "proxy-authentication-info",
            ]
            .iter()
            .any(|hop_header| name.trim().eq_ignore_ascii_case(hop_header))
        }) {
            continue;
        }
        upstream.write_all(header.as_bytes())?;
    }
    upstream.write_all(b"\r\n")?;
    upstream.write_all(&buffered)?;
    let _ = client_write.set_read_timeout(None);
    let _ = client_write.set_write_timeout(None);
    relay(&mut client_write, &mut upstream);
    Ok(())
}

fn split_host_port(authority: &str, default_port: u16) -> (String, u16) {
    if let Some(rest) = authority.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(default_port);
        return (host.to_string(), port);
    }
    if let Some((host, port)) = authority.rsplit_once(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return (host.to_string(), port);
    }
    (authority.to_string(), default_port)
}

fn connect_any(addrs: &[SocketAddr], connection: &Connection) -> Option<TcpStream> {
    use nix::sys::socket::{
        AddressFamily, SockFlag, SockType, SockaddrIn, SockaddrIn6, connect, socket,
    };

    for addr in addrs {
        let family = if addr.is_ipv4() {
            AddressFamily::Inet
        } else {
            AddressFamily::Inet6
        };
        let fd = socket(
            family,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            None,
        )
        .ok()?;
        let stream = TcpStream::from(fd);
        // Registration and the nonblocking dial share stop's lock: no worker
        // may initiate a new upstream after cancellation, even after DNS returns.
        let result = {
            let mut state = crate::sync::lock(&connection.connections);
            if state.stopped {
                return None;
            }
            state
                .sockets
                .get_mut(&connection.id)?
                .push(stream.try_clone().ok()?);
            match addr {
                SocketAddr::V4(addr) => connect(stream.as_raw_fd(), &SockaddrIn::from(*addr)),
                SocketAddr::V6(addr) => connect(stream.as_raw_fd(), &SockaddrIn6::from(*addr)),
            }
        };
        let started = std::time::Instant::now();
        if result.is_ok() || result == Err(nix::errno::Errno::EINPROGRESS) {
            while !connection.stopped() && started.elapsed() < CONNECT_TIMEOUT {
                let mut poll = libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // The descriptor is owned by stream for the whole poll call.
                let ready = unsafe { libc::poll(&mut poll, 1, 50) };
                if ready > 0 {
                    if stream.take_error().ok()?.is_none()
                        && stream.peer_addr().is_ok()
                        && !connection.stopped()
                    {
                        stream.set_nonblocking(false).ok()?;
                        stream.set_write_timeout(Some(CONNECT_TIMEOUT)).ok()?;
                        return Some(stream);
                    }
                    break;
                }
                if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
        crate::sync::lock(&connection.connections)
            .sockets
            .get_mut(&connection.id)?
            .truncate(1);
    }
    None
}

fn relay(left: &mut TcpStream, right: &mut TcpStream) {
    let _ = right.set_write_timeout(None);
    let Ok(mut left_read) = left.try_clone() else {
        return;
    };
    let Ok(mut right_write) = right.try_clone() else {
        return;
    };
    let forward = thread::spawn(move || {
        if io::copy(&mut left_read, &mut right_write).is_err() {
            let _ = left_read.shutdown(Shutdown::Both);
            let _ = right_write.shutdown(Shutdown::Both);
        } else {
            let _ = right_write.shutdown(Shutdown::Write);
        }
    });
    if io::copy(right, left).is_err() {
        let _ = right.shutdown(Shutdown::Both);
        let _ = left.shutdown(Shutdown::Both);
    } else {
        let _ = left.shutdown(Shutdown::Write);
    }
    let _ = forward.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_proxy() -> Result<StandbyProxy> {
        StandbyProxy::start(0, 0)
    }

    fn assert_closed(stream: &mut TcpStream) -> io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                ) =>
            {
                Ok(())
            }
            result => panic!("stopped standby left a socket open: {result:?}"),
        }
    }

    #[test]
    fn stopping_standby_closes_accepted_handshakes_before_they_can_dial() -> Result<()> {
        let mut proxy = test_proxy()?;
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        target.set_nonblocking(true)?;
        let mut socks = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.ports().0))?;
        socks.write_all(b"\x05\x01\x00")?;
        socks.read_exact(&mut [0; 2])?;
        let mut http = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.ports().1))?;
        http.write_all(b"CONNECT ")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while crate::sync::lock(&proxy.connections).sockets.len() != 2
            && std::time::Instant::now() < deadline
        {
            thread::yield_now();
        }
        assert_eq!(
            crate::sync::lock(&proxy.connections).sockets.len(),
            2,
            "both handshake sockets have been accepted"
        );
        proxy.stop();
        let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
        request.extend_from_slice(&target.local_addr()?.port().to_be_bytes());
        let _ = socks.write_all(&request);
        assert_closed(&mut socks)?;
        assert_closed(&mut http)?;
        assert_eq!(
            target.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        Ok(())
    }

    #[test]
    fn stopping_standby_closes_live_socks_connect_and_http_connections() -> Result<()> {
        for protocol in ["socks", "connect", "http"] {
            let mut proxy = test_proxy()?;
            let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
            let port = target.local_addr()?.port();
            let proxy_port = if protocol == "socks" {
                proxy.ports().0
            } else {
                proxy.ports().1
            };
            let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy_port))?;
            client.set_read_timeout(Some(Duration::from_secs(1)))?;
            match protocol {
                "socks" => {
                    client.write_all(b"\x05\x01\x00")?;
                    client.read_exact(&mut [0; 2])?;
                    let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
                    request.extend_from_slice(&port.to_be_bytes());
                    client.write_all(&request)?;
                    client.read_exact(&mut [0; 10])?;
                }
                "connect" => {
                    write!(client, "CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n")?;
                    client.read_exact(&mut [0; 39])?;
                }
                _ => write!(client, "GET http://127.0.0.1:{port}/ HTTP/1.1\r\n\r\n")?,
            }
            let (mut upstream, _) = target.accept()?;
            if protocol == "http" {
                upstream.read_exact(&mut [0; 18])?;
            }
            proxy.stop();
            assert_closed(&mut client)?;
            assert_closed(&mut upstream)?;
            assert!(
                crate::sync::lock(&proxy.connections).sockets.is_empty(),
                "stop joins workers and releases every tracked socket"
            );
        }
        Ok(())
    }

    #[test]
    fn ordinary_http_never_forwards_proxy_authentication_headers_to_the_origin() -> Result<()> {
        let mut proxy = test_proxy()?;
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.ports().1))?;
        write!(
            client,
            "POST http://127.0.0.1:{}/submit HTTP/1.1\r\nHost: origin.example.invalid\r\nProxy-Authorization: Basic invented-basic-token\r\npRoXy-AuThOrIzAtIoN : Bearer invented-proxy-token\r\nPrOxY-AuThEnTiCaTe: Basic realm=\"invented\"\r\nProxy-Authentication-Info: nextnonce=\"invented-nonce\"\r\npRoXy-CoNnEcTiOn: keep-alive\r\nAuthorization: Bearer invented-origin-token\r\nContent-Length: 4\r\n\r\nbody",
            target.local_addr()?.port()
        )?;
        client.shutdown(Shutdown::Write)?;
        let (mut upstream, _) = target.accept()?;
        upstream.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut received = Vec::new();
        upstream.read_to_end(&mut received)?;
        assert_eq!(received, b"POST /submit HTTP/1.1\r\nHost: origin.example.invalid\r\nAuthorization: Bearer invented-origin-token\r\nContent-Length: 4\r\n\r\nbody");
        proxy.stop();
        Ok(())
    }

    #[test]
    fn http_post_preserves_body_bytes_buffered_with_the_headers() -> Result<()> {
        let mut proxy = test_proxy()?;
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.ports().1))?;
        write!(
            client,
            "POST http://127.0.0.1:{}/submit HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody",
            target.local_addr()?.port()
        )?;
        let (mut upstream, _) = target.accept()?;
        upstream.set_read_timeout(Some(Duration::from_secs(1)))?;
        let expected = b"POST /submit HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody";
        let mut bytes = vec![0; expected.len()];
        upstream.read_exact(&mut bytes)?;
        assert_eq!(bytes, expected);
        proxy.stop();
        Ok(())
    }

    #[test]
    fn http_connect_preserves_tunnel_bytes_pipelined_with_the_headers() -> Result<()> {
        let mut proxy = test_proxy()?;
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.ports().1))?;
        write!(
            client,
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\nping",
            target.local_addr()?.port()
        )?;
        let (mut upstream, _) = target.accept()?;
        upstream.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut bytes = [0; 4];
        upstream.read_exact(&mut bytes)?;
        assert_eq!(&bytes, b"ping");
        proxy.stop();
        Ok(())
    }

    #[test]
    fn oversized_http_headers_are_refused_before_any_upstream_connection() -> Result<()> {
        let mut proxy = test_proxy()?;
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        target.set_nonblocking(true)?;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.ports().1))?;
        let request = format!(
            "GET http://127.0.0.1:{}/ HTTP/1.1\r\nX-Pad: {}\r\n\r\n",
            target.local_addr()?.port(),
            "a".repeat(64 * 1024)
        );
        let _ = client.write_all(request.as_bytes());
        let _ = client.shutdown(Shutdown::Write);
        assert_closed(&mut client)?;
        assert_eq!(
            target.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        proxy.stop();
        Ok(())
    }

    #[test]
    fn a_reset_client_releases_its_worker_even_when_the_upstream_stays_open() -> Result<()> {
        let mut proxy = test_proxy()?;
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.ports().0))?;
        client.write_all(b"\x05\x01\x00")?;
        client.read_exact(&mut [0; 2])?;
        let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
        request.extend_from_slice(&target.local_addr()?.port().to_be_bytes());
        client.write_all(&request)?;
        client.read_exact(&mut [0; 10])?;
        let (mut upstream, _) = target.accept()?;
        nix::sys::socket::setsockopt(
            &client,
            nix::sys::socket::sockopt::Linger,
            &libc::linger {
                l_onoff: 1,
                l_linger: 0,
            },
        )?;
        drop(client);
        assert_closed(&mut upstream)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !crate::sync::lock(&proxy.connections).sockets.is_empty()
            && std::time::Instant::now() < deadline
        {
            thread::yield_now();
        }
        assert!(
            crate::sync::lock(&proxy.connections).sockets.is_empty(),
            "an aborted relay must not leak a worker waiting on the other socket"
        );
        proxy.stop();
        Ok(())
    }

    #[test]
    fn standby_proxy_answers_socks5_and_frees_ports_when_stopped() -> Result<()> {
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let target_port = target.local_addr()?.port();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = target.accept() {
                let mut buf = [0_u8; 4];
                let _ = stream.read_exact(&mut buf);
                let _ = stream.write_all(b"pong");
            }
        });

        let socks_probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let socks_port = socks_probe.local_addr()?.port();
        drop(socks_probe);
        let http_probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let http_port = http_probe.local_addr()?.port();
        drop(http_probe);

        let mut proxy = StandbyProxy::start(socks_port, http_port)?;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, socks_port))?;
        client.write_all(&[0x05, 0x01, 0x00])?;
        let mut greeting = [0_u8; 2];
        client.read_exact(&mut greeting)?;
        assert_eq!(greeting, [0x05, 0x00]);

        let mut req = vec![0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1];
        req.extend_from_slice(&target_port.to_be_bytes());
        client.write_all(&req)?;
        let mut reply = [0_u8; 10];
        client.read_exact(&mut reply)?;
        assert_eq!(reply[1], 0x00);

        client.write_all(b"ping")?;
        let mut out = [0_u8; 4];
        client.read_exact(&mut out)?;
        assert_eq!(&out, b"pong");
        drop(client);

        proxy.stop();
        // Stopping frees both ports immediately so a session can bind them.
        TcpListener::bind((Ipv4Addr::LOCALHOST, socks_port))?;
        TcpListener::bind((Ipv4Addr::LOCALHOST, http_port))?;
        Ok(())
    }
}
