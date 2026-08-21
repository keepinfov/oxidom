use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::link::{Skipped, b64_decode};
use crate::model::{Server, Subscription, UserInfo};
use crate::subscription_format;
use crate::subscription_format::NotTaken;

/// Fallback User-Agent when neither the subscription nor config specify one.
/// Panels commonly gate the response on a recognized client string.
///
/// `v2rayN` rather than `v2rayNG`: both are recognized everywhere, but panels
/// routinely map `v2rayNG` to a client-specific *structured* body — Remnawave
/// answers it with an array of whole Xray configs, one balanced profile per
/// country — while `v2rayN` gets the plain share-link list of the same nodes.
/// The link list is what oxidom does most with: one server per node, each with
/// a share link, poolable and individually measurable.
const DEFAULT_USER_AGENT: &str = "v2rayN/6.45";

/// Client strings panels recognise, as `(label, value)`.
///
/// Domain knowledge rather than UI chrome, and therefore in one place: the
/// global setting and a subscription's own override are the same choice made
/// at two scopes, and a list that existed only beside the global one is why
/// the override was free text you had to know the spelling for.
///
/// The first entry is [`DEFAULT_USER_AGENT`], so "no preference" and "the
/// first preset" are the same client string rather than two.
pub const CLIENT_PRESETS: &[(&str, &str)] = &[
    ("v2rayN", DEFAULT_USER_AGENT),
    ("v2rayNG", "v2rayNG/1.9.5"),
    ("Happ", "Happ/3.13.0"),
    ("Streisand", "Streisand"),
    ("Hiddify", "Hiddify/2.0.5"),
    ("NekoBox", "NekoBox/1.3.5"),
    ("Shadowrocket", "Shadowrocket/2.2.9"),
    ("Clash Meta", "clash-verge/1.7.7"),
    ("sing-box", "SFA/1.10.0"),
];

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
    /// The share links in the response this build cannot parse. A panel lists
    /// what its own clients understand, so this is routinely non-empty and
    /// routinely the answer to "where did half my servers go".
    pub skipped: Skipped,
    /// The routing the response carried and oxidom did not apply. Distinct
    /// from `skipped`, which is about servers this build could not read: this
    /// is about entries it read perfectly well and deliberately left.
    pub not_taken: NotTaken,
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
    let (servers, skipped) = subscription_format::parse(&body)
        .with_context(|| format!("parsing subscription response for User-Agent \"{ua}\""))?;
    let not_taken = subscription_format::not_taken(&body);
    if !not_taken.is_empty() {
        // At info, beside the count of servers, because this is part of what
        // the import did rather than a diagnostic about it. The URL is never
        // named: it is a credential.
        log::info!(
            "this subscription {}",
            not_taken.summary().unwrap_or_default()
        );
    }

    Ok(FetchResult {
        servers,
        userinfo,
        title,
        update_interval,
        skipped,
        not_taken,
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

/// Normalize a per-subscription User-Agent override as a user typed it.
///
/// Blank means "inherit the global setting", which is how the value is spelled
/// everywhere it can be edited: D-Bus has no `Option`, and an emptied entry row
/// in the GUI has to clear the override rather than send a literal empty
/// User-Agent — which would make the panel choose a format on its own.
pub fn user_agent_override(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Fetch into an existing Subscription, updating servers/userinfo/name/timestamp.
pub fn refresh(sub: &mut Subscription, user_agent: &str, hwid: Option<&str>) -> Result<()> {
    let ua = sub.user_agent.as_deref().unwrap_or(user_agent);
    let mut res = fetch(&sub.url, ua, sub.send_hwid, hwid)?;
    preserve_server_identity(&sub.servers, &mut res.servers);
    sub.servers = res.servers;
    sub.skipped = res.skipped;
    sub.not_taken = res.not_taken;
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
        // A pinned certificate is the user's decision about *this* server, not
        // something the provider sends; a refresh that dropped it would make
        // the server unreachable again and ask the question a second time.
        if let Some(pin) = previous[index]
            .spec
            .stream()
            .and_then(|stream| stream.pin_sha256.clone())
            && let Some(stream) = server.spec.stream_mut()
            && stream.pin_sha256.is_none()
        {
            stream.pin_sha256 = Some(pin);
        }
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

    use super::{fetch, preserve_server_identity, require_https, user_agent_override};
    use crate::link::parse_link;

    /// A blank override must clear the field rather than be stored. An empty
    /// string reaching `refresh` would be sent as the literal User-Agent, and a
    /// panel that picks its response format from the header answers a stranger
    /// with whatever it likes — which is the bug this setting exists to fix.
    #[test]
    fn a_blank_user_agent_override_means_inherit_the_global_one() {
        for blank in ["", "   ", "\t", "\n "] {
            assert_eq!(user_agent_override(blank), None, "{blank:?}");
        }
        assert_eq!(
            user_agent_override("  v2rayN/6.45  "),
            Some("v2rayN/6.45".to_string()),
            "surrounding whitespace is an editing artifact, not part of the header"
        );
    }

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

    /// A pinned certificate is the user's answer to "do you trust this
    /// server", not something the provider sends. A refresh that dropped it
    /// would make the server unreachable again and ask a second time — and the
    /// provider's fresh copy of the entry never carries a pin, so the value
    /// has to be carried across deliberately.
    #[test]
    fn a_refresh_keeps_a_trusted_certificate() {
        let mut previous =
            parse_link("vless://test-id@example.com:443?encryption=none&type=tcp&security=tls#Old")
                .unwrap();
        previous
            .spec
            .stream_mut()
            .expect("vless carries a stream")
            .pin_sha256 = Some("a".repeat(64));

        let mut refreshed = vec![
            parse_link("vless://test-id@example.com:443?encryption=none&type=tcp&security=tls#New")
                .unwrap(),
        ];
        assert_eq!(
            refreshed[0]
                .spec
                .stream()
                .and_then(|stream| stream.pin_sha256.clone()),
            None,
            "a link never carries a pin, which is why this must be carried over"
        );

        preserve_server_identity(&[previous], &mut refreshed);

        assert_eq!(
            refreshed[0]
                .spec
                .stream()
                .and_then(|stream| stream.pin_sha256.clone()),
            Some("a".repeat(64))
        );
    }

    /// A provider that starts sending its own pin is answering the same
    /// question with better authority, so it is not overwritten.
    #[test]
    fn a_pin_from_the_provider_wins_over_a_carried_one() {
        let mut previous =
            parse_link("vless://test-id@example.com:443?encryption=none&type=tcp&security=tls#Old")
                .unwrap();
        previous
            .spec
            .stream_mut()
            .expect("vless carries a stream")
            .pin_sha256 = Some("a".repeat(64));

        let mut refreshed = vec![
            parse_link("vless://test-id@example.com:443?encryption=none&type=tcp&security=tls#New")
                .unwrap(),
        ];
        refreshed[0]
            .spec
            .stream_mut()
            .expect("vless carries a stream")
            .pin_sha256 = Some("b".repeat(64));

        preserve_server_identity(&[previous], &mut refreshed);

        assert_eq!(
            refreshed[0]
                .spec
                .stream()
                .and_then(|stream| stream.pin_sha256.clone()),
            Some("b".repeat(64))
        );
    }
}
