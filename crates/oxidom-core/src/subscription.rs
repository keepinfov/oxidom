use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::link::b64_decode;
use crate::model::{Server, Subscription, UserInfo};
use crate::subscription_format;

/// Fallback User-Agent when neither the subscription nor config specify one.
/// Panels commonly gate the response on a recognized client string.
const DEFAULT_USER_AGENT: &str = "v2rayNG/1.9.5";

/// Overall cap on a subscription fetch. ureq's default agent has *no* read
/// timeout, so a panel that completes the handshake and then goes quiet would
/// block this thread forever — and the daemon holds its engine lock across
/// this call, which would take the whole tunnel down with it.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of fetching + parsing a subscription URL.
#[derive(Debug)]
pub struct FetchResult {
    pub servers: Vec<Server>,
    pub userinfo: Option<UserInfo>,
    pub title: Option<String>,
    pub update_interval: Option<u64>,
}

/// Fetch and parse a subscription.
///
/// `user_agent` matters: many panels (Remnawave, Marzban, Happ) return an
/// "app not supported" page instead of configs when the client is unknown, so
/// callers should pass a recognized client identifier.
///
/// `hwid` is only sent when `send_hwid` is true, and uses the Happ header set
/// (`x-hwid` + optional device metadata) that Remnawave-style panels expect.
pub fn fetch(
    url: &str,
    user_agent: &str,
    send_hwid: bool,
    hwid: Option<&str>,
) -> Result<FetchResult> {
    let ua = if user_agent.trim().is_empty() {
        DEFAULT_USER_AGENT
    } else {
        user_agent
    };
    if send_hwid && hwid.is_none() {
        bail!("Send HWID is enabled, but the per-install identifier is unavailable");
    }
    require_https(url)?;
    let agent = ureq::AgentBuilder::new().timeout(FETCH_TIMEOUT).build();
    let mut req = agent.get(url).set("User-Agent", ua);
    if send_hwid && let Some(id) = hwid {
        // Happ/Remnawave device identification headers. Only x-hwid is
        // required; the rest help the panel label the device. Sent solely
        // when the user opts in per subscription.
        req = req
            .set("x-hwid", id)
            .set("x-device-os", "Linux")
            .set("x-ver-os", std::env::consts::ARCH)
            .set("x-device-model", "oxidom");
    }
    // Keep the subscription URL out of user-visible errors — for most panels
    // the URL itself is the access token. The full error goes to the debug log.
    let resp = req.call().map_err(|error| {
        log::debug!("subscription fetch failed: {error}");
        match error {
            ureq::Error::Status(code, _) => {
                anyhow::anyhow!("fetching subscription: the server responded with HTTP {code}")
            }
            ureq::Error::Transport(transport) => {
                anyhow::anyhow!("fetching subscription: {}", transport.kind())
            }
        }
    })?;

    let userinfo = resp
        .header("subscription-userinfo")
        .and_then(parse_userinfo);
    let title = resp
        .header("profile-title")
        .map(decode_maybe_b64_header)
        .filter(|s| !s.is_empty());
    let update_interval = resp
        .header("profile-update-interval")
        .and_then(|s| s.trim().parse::<u64>().ok());
    let hwid_required = resp
        .header("x-hwid-not-supported")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    if !send_hwid && hwid_required {
        bail!(
            "this subscription requires a device identifier; enable Advanced > Send HWID \
             and add it again"
        );
    }

    let body = resp.into_string().context("reading subscription body")?;
    let servers = subscription_format::parse(&body)
        .with_context(|| format!("parsing subscription response for User-Agent \"{ua}\""))?;

    Ok(FetchResult {
        servers,
        userinfo,
        title,
        update_interval,
    })
}

/// Loopback is exempt from [`require_https`]: there is no on-path position
/// between a process and 127.0.0.1, and a locally hosted panel has nowhere else
/// to live.
fn is_loopback_url(parsed: &url::Url) -> bool {
    match parsed.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Refuse a subscription URL that is not `https` (or plaintext to loopback).
///
/// The response body is authoritative — it supplies every server's address,
/// credentials and transport, and for an imported Xray profile the routing
/// material as well — so over plaintext an on-path attacker owns the whole
/// tunnel. This client exists for people on exactly such networks, and TLS is
/// otherwise fully verified, which makes cleartext the one way around it. The
/// check lives here rather than in a caller because `fetch` is the single
/// network boundary every path reaches.
fn require_https(url: &str) -> Result<()> {
    let Ok(parsed) = url::Url::parse(url) else {
        bail!("this subscription URL cannot be parsed");
    };
    if parsed.scheme() == "https" || (parsed.scheme() == "http" && is_loopback_url(&parsed)) {
        return Ok(());
    }
    bail!(
        "subscriptions must use https; {}:// is an unauthenticated transport that anyone on the \
         network can rewrite",
        parsed.scheme()
    )
}

/// Fetch into an existing Subscription, updating servers/userinfo/name/timestamp.
pub fn refresh(sub: &mut Subscription, user_agent: &str, hwid: Option<&str>) -> Result<()> {
    let ua = sub.user_agent.as_deref().unwrap_or(user_agent);
    let mut res = fetch(&sub.url, ua, sub.send_hwid, hwid)?;
    preserve_server_identity(&sub.servers, &mut res.servers);
    sub.servers = res.servers;
    sub.userinfo = res.userinfo;
    if let Some(t) = res.title {
        sub.name = t;
    }
    sub.updated_at = Some(now_unix());
    Ok(())
}

fn preserve_server_identity(previous: &[Server], refreshed: &mut [Server]) {
    let mut used = vec![false; previous.len()];
    for server in refreshed {
        let previous_index = previous
            .iter()
            .enumerate()
            .find(|(index, old)| !used[*index] && old.id == server.id)
            .or_else(|| {
                previous
                    .iter()
                    .enumerate()
                    .find(|(index, old)| !used[*index] && old.same_connection_as(server))
            })
            .map(|(index, _)| index);

        let Some(index) = previous_index else {
            continue;
        };
        used[index] = true;
        server.id.clone_from(&previous[index].id);
        server.alias.clone_from(&previous[index].alias);
        server.latency_ms = previous[index].latency_ms;
    }
}

fn decode_maybe_b64_header(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("base64:")
        && let Some(bytes) = b64_decode(rest)
        && let Ok(text) = String::from_utf8(bytes)
    {
        return text;
    }
    raw.to_string()
}

/// Parse `upload=..; download=..; total=..; expire=..` header.
fn parse_userinfo(raw: &str) -> Option<UserInfo> {
    let mut info = UserInfo::default();
    let mut any = false;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            let v = v.trim();
            match k.trim() {
                "upload" => {
                    info.upload = v.parse().unwrap_or(0);
                    any = true;
                }
                "download" => {
                    info.download = v.parse().unwrap_or(0);
                    any = true;
                }
                "total" => {
                    info.total = v.parse().unwrap_or(0);
                    any = true;
                }
                "expire" => {
                    info.expire = v.parse().ok();
                    any = true;
                }
                _ => {}
            }
        }
    }
    if any { Some(info) } else { None }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::{fetch, preserve_server_identity, require_https};
    use crate::link::parse_link;

    #[test]
    fn plaintext_subscriptions_are_refused_off_loopback() {
        for rejected in [
            "http://panel.example/sub",
            "HTTP://panel.example/sub",
            "ftp://panel.example/sub",
            "file:///etc/passwd",
            "http://localhost.evil.example/sub",
            "not a url",
        ] {
            assert!(
                require_https(rejected).is_err(),
                "{rejected} must be refused"
            );
        }
        for accepted in [
            "https://panel.example/sub",
            "http://127.0.0.1:8080/sub",
            "http://localhost:8080/sub",
            "http://[::1]:8080/sub",
        ] {
            require_https(accepted).unwrap_or_else(|error| panic!("{accepted}: {error}"));
        }
    }

    fn serve_once(headers: &str, body: &str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let headers = headers.to_string();
        let body = body.to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            sender
                .send(String::from_utf8_lossy(&request).into_owned())
                .unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}\r\n\r\n{}",
                body.len(),
                headers,
                body
            )
            .unwrap();
        });
        (format!("http://{address}/subscription"), receiver)
    }

    #[test]
    fn reports_hwid_gate_without_sending_an_identifier() {
        let (url, request) = serve_once(
            "Content-Type: text/plain\r\nx-hwid-not-supported: true",
            "ignored",
        );
        let error = fetch(&url, "test-client", false, None).unwrap_err();
        assert!(error.to_string().contains("requires a device identifier"));
        assert!(
            !request
                .recv()
                .unwrap()
                .to_ascii_lowercase()
                .contains("x-hwid:")
        );
    }

    #[test]
    fn sends_hwid_only_after_opt_in_and_parses_the_response() {
        let (url, request) = serve_once(
            "Content-Type: text/plain",
            "vless://test-id@example.com:443?encryption=none&type=tcp&security=none#Example",
        );
        let result = fetch(&url, "test-client", true, Some("installation-id")).unwrap();
        assert_eq!(result.servers.len(), 1);
        assert!(
            request
                .recv()
                .unwrap()
                .to_ascii_lowercase()
                .contains("x-hwid: installation-id")
        );
    }

    #[test]
    fn refresh_keeps_identity_when_only_the_server_name_changes() {
        let mut previous = parse_link(
            "vless://test-id@example.com:443?encryption=none&type=tcp&security=none#Old",
        )
        .unwrap();
        previous.latency_ms = Some(42);
        previous.alias = Some("saved-handle".to_string());
        let old_id = previous.id.clone();
        let mut refreshed = vec![
            parse_link(
                "vless://test-id@example.com:443?encryption=none&type=tcp&security=none#New",
            )
            .unwrap(),
        ];
        assert_ne!(old_id, refreshed[0].id);

        preserve_server_identity(&[previous], &mut refreshed);

        assert_eq!(refreshed[0].id, old_id);
        assert_eq!(refreshed[0].alias.as_deref(), Some("saved-handle"));
        assert_eq!(refreshed[0].latency_ms, Some(42));
        assert_eq!(refreshed[0].name, "New");
    }
}
