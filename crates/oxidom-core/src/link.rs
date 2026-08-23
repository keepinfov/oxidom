use std::collections::HashMap;

use base64::Engine as _;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::model::{
    Hysteria2Obfs, Hysteria2Settings, OutboundSpec, PortRange, Protocol, Server, StreamSettings,
    country_from_name, normalize_pin_sha256, parse_bandwidth_mbps, transport_label,
};

/// Try several base64 alphabets/paddings and return decoded bytes.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let engines = [
        base64::engine::general_purpose::STANDARD,
        base64::engine::general_purpose::STANDARD_NO_PAD,
        base64::engine::general_purpose::URL_SAFE,
        base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ];
    for eng in engines {
        if let Ok(bytes) = eng.decode(s.as_bytes()) {
            return Some(bytes);
        }
    }
    None
}

/// Strip the brackets `Url` keeps around an IPv6 literal.
///
/// `[2001:db8::1]` is the right spelling inside a URL and the wrong one
/// everywhere downstream: `to_socket_addrs` and `ping` both reject it, so a
/// bracketed address means a server that can never be measured. Xray strips
/// the brackets itself, which is why the tunnel works while the card does not.
fn normalize_host(host: &str) -> String {
    host.strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host)
        .to_string()
}

fn host_of(url: &Url) -> Option<String> {
    Some(normalize_host(url.host_str()?))
}

fn decode_fragment(url: &Url) -> String {
    url.fragment()
        .map(|f| percent_decode_str(f).decode_utf8_lossy().into_owned())
        .unwrap_or_default()
}

fn query_map(url: &Url) -> HashMap<String, String> {
    url.query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn stream_from_query(q: &HashMap<String, String>) -> StreamSettings {
    let get = |k: &str| q.get(k).cloned().filter(|s| !s.is_empty());
    let network = get("type").unwrap_or_else(|| "tcp".to_string());
    let security = get("security").unwrap_or_else(|| "none".to_string());
    StreamSettings {
        network,
        security,
        sni: get("sni").or_else(|| get("peer")),
        alpn: get("alpn").map(|a| a.split(',').map(|s| s.trim().to_string()).collect()),
        fingerprint: get("fp"),
        allow_insecure: q
            .get("allowInsecure")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false),
        pin_sha256: get("pinSHA256")
            .or_else(|| get("pinsha256"))
            .as_deref()
            .and_then(normalize_pin_sha256),
        public_key: get("pbk"),
        short_id: get("sid"),
        spider_x: get("spx"),
        path: get("path"),
        host: get("host"),
        service_name: get("serviceName"),
        header_type: get("headerType"),
        flow: get("flow"),
    }
}

fn finish(
    link: &str,
    name: String,
    protocol: Protocol,
    address: String,
    port: u16,
    spec: OutboundSpec,
) -> Server {
    let country = country_from_name(&name);
    let mut server = Server {
        id: String::new(),
        transport_label: transport_label(protocol, &spec),
        country,
        name,
        protocol,
        address,
        port,
        spec,
        link: Some(link.to_string()),
        alias: None,
        latency_ms: None,
    };
    server.id = Server::stable_id(&server.identity_string());
    server
}

/// Every scheme [`parse_link`] understands, with the human name used in dialogs
/// and error messages. A `None` label marks an alias that needs no separate
/// mention (`socks5` alongside `socks`).
///
/// This is the one list. The import dialog, its validator and the engine's
/// error message all read it, because three hand-maintained copies had already
/// drifted apart from each other and from `parse_link`.
const SCHEMES: &[(&str, Option<&str>)] = &[
    ("vless", Some("vless")),
    ("vmess", Some("vmess")),
    ("trojan", Some("trojan")),
    ("ss", Some("Shadowsocks")),
    ("socks", Some("SOCKS")),
    ("socks5", None),
    ("http", Some("HTTP")),
    ("https", None),
    ("hysteria2", Some("Hysteria2")),
    ("hy2", None),
];

/// Whether a line uses a scheme oxidom can parse. Case-insensitive and
/// BOM-tolerant, to match what [`parse_link`] actually accepts.
pub fn is_supported_scheme(line: &str) -> bool {
    let line = line.trim().trim_start_matches('\u{feff}').trim();
    let Some((scheme, _)) = line.split_once("://") else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    SCHEMES.iter().any(|(name, _)| *name == scheme)
}

/// The supported schemes as prose, e.g. "vless, vmess, …, HTTP".
pub fn supported_scheme_list() -> String {
    SCHEMES
        .iter()
        .filter_map(|(_, label)| *label)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse any supported share link into a Server. Returns None if unrecognized.
pub fn parse_link(link: &str) -> Option<Server> {
    // A UTF-8 BOM survives `trim` (it is not whitespace) and would turn the
    // first line of a subscription into an unknown "\u{feff}vless" scheme.
    let link = link.trim().trim_start_matches('\u{feff}').trim();
    let scheme = link.split("://").next()?.to_lowercase();
    match scheme.as_str() {
        "vless" => parse_vless(link),
        "vmess" => parse_vmess(link),
        "trojan" => parse_trojan(link),
        "ss" => parse_ss(link),
        "socks" | "socks5" => parse_socks(link),
        "http" | "https" => parse_http(link),
        "hysteria2" | "hy2" => parse_hysteria2(link),
        _ => None,
    }
}

fn parse_vless(link: &str) -> Option<Server> {
    let url = Url::parse(link).ok()?;
    let uuid = percent_decode_str(url.username())
        .decode_utf8_lossy()
        .into_owned();
    if uuid.is_empty() {
        return None;
    }
    let host = host_of(&url)?;
    let port = url.port()?;
    let q = query_map(&url);
    let stream = stream_from_query(&q);
    let name = {
        let f = decode_fragment(&url);
        if f.is_empty() { host.clone() } else { f }
    };
    let encryption = q
        .get("encryption")
        .cloned()
        .unwrap_or_else(|| "none".to_string());
    let spec = OutboundSpec::Vless {
        uuid,
        encryption,
        stream,
    };
    Some(finish(link, name, Protocol::Vless, host, port, spec))
}

fn parse_trojan(link: &str) -> Option<Server> {
    let url = Url::parse(link).ok()?;
    let password = percent_decode_str(url.username())
        .decode_utf8_lossy()
        .into_owned();
    if password.is_empty() {
        return None;
    }
    let host = host_of(&url)?;
    let port = url.port()?;
    let q = query_map(&url);
    let mut stream = stream_from_query(&q);
    // Trojan is TLS by default.
    if stream.security == "none" {
        stream.security = "tls".to_string();
    }
    let name = {
        let f = decode_fragment(&url);
        if f.is_empty() { host.clone() } else { f }
    };
    let spec = OutboundSpec::Trojan { password, stream };
    Some(finish(link, name, Protocol::Trojan, host, port, spec))
}

fn parse_vmess(link: &str) -> Option<Server> {
    let payload = link.strip_prefix("vmess://")?;
    let bytes = b64_decode(payload)?;
    let json: Value = serde_json::from_slice(&bytes).ok()?;

    let s = |k: &str| json.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let num = |k: &str| -> Option<u64> {
        json.get(k).and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
    };

    let host = normalize_host(&s("add")?);
    // `as u16` silently wraps: a panel emitting 65536 would produce port 0 and
    // a config xray refuses to load, with nothing on screen to explain it.
    let port = u16::try_from(num("port")?).ok()?;
    let uuid = s("id")?;
    // The same wrap as the port above: `as u32` would turn an oversized
    // AlterID into a different one and the handshake would fail unexplained.
    let alter_id = match num("aid") {
        Some(aid) => u32::try_from(aid).ok()?,
        None => 0,
    };
    let security = s("scy").unwrap_or_else(|| "auto".to_string());
    let net = s("net").unwrap_or_else(|| "tcp".to_string());
    let tls = s("tls").unwrap_or_default();
    let name = s("ps").unwrap_or_else(|| host.clone());

    let stream = StreamSettings {
        network: net,
        security: if tls.is_empty() {
            "none".to_string()
        } else {
            tls
        },
        sni: s("sni").or_else(|| s("host")),
        alpn: s("alpn").map(|a| a.split(',').map(|x| x.trim().to_string()).collect()),
        fingerprint: s("fp"),
        allow_insecure: false,
        pin_sha256: None,
        public_key: None,
        short_id: None,
        spider_x: None,
        path: s("path"),
        host: s("host"),
        service_name: s("path"),
        header_type: s("type"),
        flow: None,
    };
    let spec = OutboundSpec::Vmess {
        uuid,
        alter_id,
        security,
        stream,
    };
    Some(finish(link, name, Protocol::Vmess, host, port, spec))
}

fn parse_ss(link: &str) -> Option<Server> {
    // SIP002: ss://base64(method:password)@host:port#name
    // Legacy:  ss://base64(method:password@host:port)#name
    let rest = link.strip_prefix("ss://")?;
    let (main, frag) = match rest.split_once('#') {
        Some((m, f)) => (m, Some(f)),
        None => (rest, None),
    };
    // Drop any plugin query — Xray has no SIP003 plugin support, so an
    // obfs/v2ray-plugin server will connect but not actually work.
    let (main, query) = match main.split_once('?') {
        Some((main, query)) => (main, Some(query)),
        None => (main, None),
    };
    if query.is_some_and(|query| query.contains("plugin=")) {
        log::warn!(
            "shadowsocks link requests a SIP003 plugin, which is not supported; \
             the server will likely not work"
        );
    }
    let name = frag
        .map(|f| percent_decode_str(f).decode_utf8_lossy().into_owned())
        .unwrap_or_default();

    let (method, password, host, port) = if let Some((userinfo, hostport)) = main.rsplit_once('@') {
        // SIP002: userinfo is base64(method:password), hostport is host:port.
        // Percent-decode first — a URL-safe base64 blob still gets its `-`/`_`
        // (and any `/` from the standard alphabet) escaped by generators, and
        // `%2F` is not in any base64 alphabet, so decoding it as-is fails and
        // the whole userinfo would be mistaken for a plaintext method.
        let raw = percent_decode_str(userinfo)
            .decode_utf8_lossy()
            .into_owned();
        let decoded = b64_decode(&raw)
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or(raw);
        let (method, password) = decoded.split_once(':')?;
        let (host, port) = split_host_port(hostport)?;
        (method.to_string(), password.to_string(), host, port)
    } else {
        // Legacy fully base64.
        let decoded = String::from_utf8(b64_decode(main)?).ok()?;
        let (creds, hostport) = decoded.rsplit_once('@')?;
        let (method, password) = creds.split_once(':')?;
        let (host, port) = split_host_port(hostport)?;
        (method.to_string(), password.to_string(), host, port)
    };

    let name = if name.is_empty() { host.clone() } else { name };
    let spec = OutboundSpec::Shadowsocks { method, password };
    Some(finish(link, name, Protocol::Shadowsocks, host, port, spec))
}

/// Split a hysteria2 port-hopping suffix out of the authority.
///
/// `hysteria2://pw@host:443,5000-6000` is a legal share link but not a legal
/// URL — `Url::parse` rejects the comma with `InvalidPort` — so the ranges have
/// to come off before parsing. Returns the URL-parseable link and the extra
/// ranges; a link without a suffix comes back unchanged.
fn split_port_hop(link: &str) -> (String, Vec<PortRange>) {
    let Some(scheme_end) = link.find("://") else {
        return (link.to_string(), Vec::new());
    };
    let start = scheme_end + 3;
    let end = link[start..]
        .find(['/', '?', '#'])
        .map(|i| start + i)
        .unwrap_or(link.len());
    let authority = &link[start..end];

    // The userinfo may itself contain '@', so the host starts after the last one.
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(at) => (&authority[..=at], &authority[at + 1..]),
        None => ("", authority),
    };
    // A comma cannot occur inside a bracketed IPv6 literal or a port number,
    // so one split is enough and stays correct for `[::1]:443,5000-6000`.
    let Some((hostport, extra)) = hostport.split_once(',') else {
        return (link.to_string(), Vec::new());
    };

    let ranges = extra.split(',').filter_map(PortRange::parse).collect();
    let sanitized = format!("{}{userinfo}{hostport}{}", &link[..start], &link[end..]);
    (sanitized, ranges)
}

fn parse_hysteria2(link: &str) -> Option<Server> {
    // Normalize the `hy2://` alias so both spellings of one server produce the
    // same stable id and dedupe against each other.
    let canonical = match link.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("hy2") => format!("hysteria2://{rest}"),
        _ => link.to_string(),
    };
    let (sanitized, mut port_hop) = split_port_hop(&canonical);
    let url = Url::parse(&sanitized).ok()?;
    let q = query_map(&url);
    let get = |k: &str| q.get(k).cloned().filter(|s| !s.is_empty());

    // Hysteria2 auth is one opaque string that may contain ':'. `Url` splits it
    // into username and password at the first colon, so put it back together.
    let user = percent_decode_str(url.username())
        .decode_utf8_lossy()
        .into_owned();
    let auth = match url.password() {
        Some(password) => format!(
            "{user}:{}",
            percent_decode_str(password).decode_utf8_lossy()
        ),
        None => user,
    };
    // Some panels put the credential in the query instead of the userinfo.
    let auth = if auth.is_empty() {
        get("auth").or_else(|| get("password")).unwrap_or_default()
    } else {
        auth
    };
    if auth.is_empty() {
        return None;
    }

    let host = host_of(&url)?;
    // Unlike the other schemes here, hysteria2 defines a default port.
    let port = url.port().unwrap_or(443);

    // Port hopping also travels as a query parameter in some exporters.
    for key in ["mport", "ports"] {
        if let Some(value) = get(key) {
            port_hop.extend(value.split(',').filter_map(PortRange::parse));
        }
    }

    let obfs = get("obfs").map(|kind| Hysteria2Obfs {
        kind,
        password: get("obfs-password").unwrap_or_default(),
    });

    let settings = Hysteria2Settings {
        sni: get("sni").or_else(|| get("peer")),
        alpn: get("alpn").map(|a| a.split(',').map(|s| s.trim().to_string()).collect()),
        allow_insecure: get("insecure")
            .or_else(|| get("allowInsecure"))
            .is_some_and(|v| v == "1" || v == "true"),
        pin_sha256: get("pinSHA256").as_deref().and_then(normalize_pin_sha256),
        obfs,
        up_mbps: get("up")
            .or_else(|| get("upmbps"))
            .as_deref()
            .and_then(parse_bandwidth_mbps),
        down_mbps: get("down")
            .or_else(|| get("downmbps"))
            .as_deref()
            .and_then(parse_bandwidth_mbps),
        port_hop,
        hop_interval_secs: get("hopInterval").and_then(|v| v.parse().ok()),
        congestion: get("congestion"),
        udp_idle_timeout_secs: None,
    };

    let name = {
        let f = decode_fragment(&url);
        if f.is_empty() { host.clone() } else { f }
    };
    let spec = OutboundSpec::Hysteria2 { auth, settings };
    // Store the canonical link, commas and all: it is what round-trips.
    Some(finish(
        &canonical,
        name,
        Protocol::Hysteria2,
        host,
        port,
        spec,
    ))
}

fn parse_socks(link: &str) -> Option<Server> {
    let url = Url::parse(link).ok()?;
    let host = host_of(&url)?;
    let port = url.port()?;
    let username = opt(url.username());
    let password = url.password().map(|p| p.to_string());
    let name = {
        let f = decode_fragment(&url);
        if f.is_empty() { host.clone() } else { f }
    };
    let spec = OutboundSpec::Socks { username, password };
    Some(finish(link, name, Protocol::Socks, host, port, spec))
}

fn parse_http(link: &str) -> Option<Server> {
    let url = Url::parse(link).ok()?;
    let host = host_of(&url)?;
    let port = url
        .port()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    let username = opt(url.username());
    let password = url.password().map(|p| p.to_string());
    let name = {
        let f = decode_fragment(&url);
        if f.is_empty() { host.clone() } else { f }
    };
    let spec = OutboundSpec::Http { username, password };
    Some(finish(link, name, Protocol::Http, host, port, spec))
}

fn opt(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(percent_decode_str(s).decode_utf8_lossy().into_owned())
    }
}

fn split_host_port(s: &str) -> Option<(String, u16)> {
    let (host, port) = match s.trim().strip_prefix('[') {
        // Bracketed IPv6: the port separator is the colon after the closing
        // bracket, not the last colon in the address.
        Some(rest) => {
            let (host, tail) = rest.split_once(']')?;
            (host, tail.strip_prefix(':')?)
        }
        None => s.rsplit_once(':')?,
    };
    let port: u16 = port.trim().parse().ok()?;
    Some((normalize_host(host), port))
}

/// Parse a newline list of share links into servers, skipping unrecognized lines.
pub fn parse_links(text: &str) -> Vec<Server> {
    parse_links_reporting(text).0
}

/// What a parse dropped on the floor.
///
/// A provider lists what its own clients understand, so a list that oxidom
/// only half understands is normal — and the half that vanished has to be
/// reported, or the answer to "why are there ten servers here and twenty in
/// the app on my phone" is nowhere.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    /// How many lines were dropped.
    pub lines: usize,
    /// The distinct schemes those lines used, lowercased, in the order first
    /// met. Naming them is what makes the count actionable: `tuic` is a
    /// protocol oxidom does not speak, while `vless` here would mean a
    /// malformed link and therefore a bug worth reporting.
    pub schemes: Vec<String>,
}

impl Skipped {
    pub fn is_empty(&self) -> bool {
        self.lines == 0
    }
}

/// Like `parse_links`, but also reports the lines that look like share links
/// yet use an unsupported or malformed scheme (tuic://, ssh://, …), so the
/// caller can tell the user instead of dropping servers silently.
pub fn parse_links_reporting(text: &str) -> (Vec<Server>, Skipped) {
    let mut servers = Vec::new();
    let mut skipped = Skipped::default();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }
        match parse_link(line) {
            Some(server) => servers.push(server),
            // Only a line that names a scheme is a dropped *server*. Anything
            // else is a comment, a stray header or a fragment of HTML, and
            // counting those would report a loss that never happened.
            None if line.contains("://") => {
                skipped.lines += 1;
                let scheme = line
                    .split_once("://")
                    .map(|(scheme, _)| scheme.trim().to_ascii_lowercase())
                    .unwrap_or_default();
                if !scheme.is_empty() && !skipped.schemes.contains(&scheme) {
                    skipped.schemes.push(scheme);
                }
            }
            None => {}
        }
    }
    (servers, skipped)
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::*;

    const UUID: &str = "b831381d-6324-4d53-ad4f-8cda48b30811";

    fn stream_of(server: &Server) -> &StreamSettings {
        server.spec.stream().expect("spec should carry a stream")
    }

    #[test]
    fn vless_reads_transport_and_reality_parameters() {
        let link = format!(
            "vless://{UUID}@example.com:443?type=ws&security=reality&sni=cdn.example\
             &pbk=key&sid=ab&spx=%2F&path=%2Fws&host=cdn.example&fp=chrome\
             &flow=xtls-rprx-vision#%F0%9F%87%B3%F0%9F%87%B1%20Node"
        );
        let server = parse_link(&link).unwrap();

        assert_eq!(server.protocol, Protocol::Vless);
        assert_eq!(server.address, "example.com");
        assert_eq!(server.port, 443);
        assert_eq!(server.name, "🇳🇱 Node");
        assert_eq!(server.country.as_deref(), Some("NL"));
        assert_eq!(server.transport_label, "vless + ws + reality");

        let stream = stream_of(&server);
        assert_eq!(stream.network, "ws");
        assert_eq!(stream.security, "reality");
        assert_eq!(stream.sni.as_deref(), Some("cdn.example"));
        assert_eq!(stream.public_key.as_deref(), Some("key"));
        assert_eq!(stream.short_id.as_deref(), Some("ab"));
        assert_eq!(stream.spider_x.as_deref(), Some("/"));
        assert_eq!(stream.path.as_deref(), Some("/ws"));
        assert_eq!(stream.fingerprint.as_deref(), Some("chrome"));
        assert_eq!(stream.flow.as_deref(), Some("xtls-rprx-vision"));
    }

    /// Trojan is TLS even when the link says nothing about security.
    #[test]
    fn trojan_defaults_to_tls() {
        let server = parse_link("trojan://pw@example.com:443#T").unwrap();
        assert_eq!(server.protocol, Protocol::Trojan);
        assert_eq!(stream_of(&server).security, "tls");
        assert_eq!(server.transport_label, "trojan + tls");
        match &server.spec {
            OutboundSpec::Trojan { password, .. } => assert_eq!(password, "pw"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn vmess_reads_its_base64_json_payload() {
        let payload = serde_json::json!({
            "add": "example.com", "port": "443", "id": UUID, "aid": "0",
            "scy": "auto", "net": "ws", "tls": "tls", "ps": "VM", "path": "/p",
        })
        .to_string();
        let link = format!(
            "vmess://{}",
            base64::engine::general_purpose::STANDARD.encode(payload)
        );
        let server = parse_link(&link).unwrap();

        assert_eq!(server.protocol, Protocol::Vmess);
        assert_eq!(server.address, "example.com");
        assert_eq!(server.port, 443);
        assert_eq!(server.name, "VM");
        assert_eq!(server.transport_label, "vmess + ws + tls");
        assert_eq!(stream_of(&server).path.as_deref(), Some("/p"));
    }

    #[test]
    fn shadowsocks_accepts_both_sip002_and_legacy_encodings() {
        let creds = base64::engine::general_purpose::STANDARD.encode("aes-256-gcm:secret");
        let sip002 = parse_link(&format!("ss://{creds}@example.com:8388#SS")).unwrap();

        let legacy_body =
            base64::engine::general_purpose::STANDARD.encode("aes-256-gcm:secret@example.com:8388");
        let legacy = parse_link(&format!("ss://{legacy_body}#SS")).unwrap();

        for server in [&sip002, &legacy] {
            assert_eq!(server.protocol, Protocol::Shadowsocks);
            assert_eq!(server.address, "example.com");
            assert_eq!(server.port, 8388);
            assert_eq!(server.transport_label, "shadowsocks");
            match &server.spec {
                OutboundSpec::Shadowsocks { method, password } => {
                    assert_eq!(method, "aes-256-gcm");
                    assert_eq!(password, "secret");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn socks_and_http_carry_optional_credentials() {
        let socks = parse_link("socks5://user:pass@example.com:1080#S").unwrap();
        assert_eq!(socks.protocol, Protocol::Socks);
        assert_eq!(socks.port, 1080);
        match &socks.spec {
            OutboundSpec::Socks { username, password } => {
                assert_eq!(username.as_deref(), Some("user"));
                assert_eq!(password.as_deref(), Some("pass"));
            }
            other => panic!("{other:?}"),
        }

        // A bare https link has no port; the scheme supplies the default.
        let http = parse_link("https://example.com#H").unwrap();
        assert_eq!(http.protocol, Protocol::Http);
        assert_eq!(http.port, 443);
        assert!(matches!(
            &http.spec,
            OutboundSpec::Http {
                username: None,
                password: None
            }
        ));
    }

    #[test]
    fn links_without_credentials_are_rejected() {
        assert!(parse_link("vless://@example.com:443").is_none());
        assert!(parse_link("trojan://example.com:443").is_none());
    }

    /// A BOM or stray whitespace on the first line of a subscription must not
    /// turn a valid scheme into an unknown one.
    #[test]
    fn leading_bom_and_whitespace_are_tolerated() {
        let link = format!("\u{feff}  vless://{UUID}@example.com:443  ");
        assert!(parse_link(&link).is_some());
    }

    /// The scheme table and `parse_link`'s dispatch must not drift apart: a
    /// scheme advertised in the import dialog has to actually parse.
    #[test]
    fn every_advertised_scheme_parses() {
        let creds = base64::engine::general_purpose::STANDARD.encode("aes-256-gcm:secret");
        for (scheme, _) in SCHEMES {
            let sample = match *scheme {
                "ss" => format!("ss://{creds}@example.com:8388"),
                "vmess" => format!(
                    "vmess://{}",
                    base64::engine::general_purpose::STANDARD.encode(
                        serde_json::json!({"add":"example.com","port":"443","id":UUID}).to_string()
                    )
                ),
                "http" | "https" => format!("{scheme}://example.com:8080"),
                other => format!("{other}://{UUID}@example.com:443"),
            };
            assert!(
                is_supported_scheme(&sample),
                "{scheme} missing from is_supported_scheme"
            );
            assert!(parse_link(&sample).is_some(), "{scheme} failed to parse");
        }
    }

    /// `parse_link` lowercases the scheme, so the dialog's check must too —
    /// otherwise the GUI rejects a link the engine would have accepted.
    #[test]
    fn scheme_matching_is_case_insensitive() {
        let link = format!("VLESS://{UUID}@example.com:443");
        assert!(is_supported_scheme(&link));
        assert!(parse_link(&link).is_some());
    }

    #[test]
    fn unrelated_lines_are_not_mistaken_for_links() {
        for line in ["", "hello", "example.com", "tuic://x@y:443"] {
            assert!(!is_supported_scheme(line), "{line:?}");
        }
    }

    fn hy2_settings(server: &Server) -> &Hysteria2Settings {
        match &server.spec {
            OutboundSpec::Hysteria2 { settings, .. } => settings,
            other => panic!("{other:?}"),
        }
    }

    fn hy2_auth(server: &Server) -> &str {
        match &server.spec {
            OutboundSpec::Hysteria2 { auth, .. } => auth,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hysteria2_minimal_link_defaults_to_port_443() {
        let server = parse_link("hysteria2://pw@h.example/").unwrap();
        assert_eq!(server.protocol, Protocol::Hysteria2);
        assert_eq!(server.address, "h.example");
        assert_eq!(server.port, 443, "hysteria2 defines a default port");
        assert_eq!(hy2_auth(&server), "pw");
        assert_eq!(server.transport_label, "hysteria2");
    }

    /// Both spellings name the same server, so they must collapse to one entry
    /// rather than showing up twice after an import.
    #[test]
    fn hy2_alias_normalizes_to_hysteria2() {
        let short = parse_link("hy2://pw@h.example:8443#Node").unwrap();
        let long = parse_link("hysteria2://pw@h.example:8443#Node").unwrap();
        assert_eq!(short.id, long.id);
        assert!(short.link.as_deref().unwrap().starts_with("hysteria2://"));
        assert!(short.same_connection_as(&long));
    }

    /// The auth string is opaque and may contain ':' — which `Url` would
    /// otherwise split off as a password and silently truncate.
    #[test]
    fn auth_survives_colons_and_percent_encoding() {
        let server = parse_link("hysteria2://us%3Aer%40x%2Fy@h.example:443").unwrap();
        assert_eq!(hy2_auth(&server), "us:er@x/y");

        let split = parse_link("hysteria2://user:pass@h.example:443").unwrap();
        assert_eq!(hy2_auth(&split), "user:pass");
    }

    #[test]
    fn port_hopping_suffix_is_parsed_and_preserved() {
        let link = "hysteria2://pw@h.example:443,5000-6000,7000/?hopInterval=30";
        let server = parse_link(link).unwrap();

        assert_eq!(
            server.port, 443,
            "the primary port stays the advertised one"
        );
        assert_eq!(
            hy2_settings(&server).port_hop,
            vec![
                PortRange {
                    start: 5000,
                    end: 6000
                },
                PortRange {
                    start: 7000,
                    end: 7000
                },
            ]
        );
        assert_eq!(hy2_settings(&server).hop_interval_secs, Some(30));
        // The stored link must still round-trip, commas and all.
        assert_eq!(server.link.as_deref(), Some(link));
    }

    #[test]
    fn ipv6_literals_lose_their_brackets() {
        let server = parse_link("hysteria2://pw@[2001:db8::1]:443,5000-6000/").unwrap();
        assert_eq!(server.address, "2001:db8::1");
        assert_eq!(server.port, 443);
        assert_eq!(hy2_settings(&server).port_hop.len(), 1);
    }

    #[test]
    fn hysteria2_query_parameters_map_to_settings() {
        let link = "hysteria2://pw@h.example:443/?obfs=salamander&obfs-password=o\
                    &sni=real.example&insecure=1&alpn=h3\
                    &pinSHA256=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\
                    &up=100%20mbps&down=1%20gbps&congestion=bbr#Obfuscated";
        let server = parse_link(link).unwrap();
        let s = hy2_settings(&server);

        assert_eq!(server.name, "Obfuscated");
        assert_eq!(server.transport_label, "hysteria2 + salamander");
        let obfs = s.obfs.as_ref().unwrap();
        assert_eq!(obfs.kind, "salamander");
        assert_eq!(obfs.password, "o");
        assert_eq!(s.sni.as_deref(), Some("real.example"));
        assert!(s.allow_insecure);
        assert_eq!(s.alpn.as_deref(), Some(["h3".to_string()].as_slice()));
        assert_eq!(
            s.pin_sha256.as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(s.up_mbps, Some(100));
        assert_eq!(s.down_mbps, Some(1000));
        assert_eq!(s.congestion.as_deref(), Some("bbr"));
    }

    #[test]
    fn hysteria2_without_credentials_is_rejected() {
        assert!(parse_link("hysteria2://h.example:443").is_none());
    }

    /// Hysteria v1 is a different wire protocol that Xray's version-2 outbound
    /// cannot speak, so it must stay unsupported rather than be mis-served.
    #[test]
    fn hysteria_v1_is_not_treated_as_hysteria2() {
        assert!(parse_link("hysteria://pw@h.example:443").is_none());
        let (servers, skipped) = parse_links_reporting("hysteria://pw@h.example:443");
        assert!(servers.is_empty());
        assert_eq!(skipped.lines, 1);
        assert_eq!(skipped.schemes, ["hysteria"]);
    }

    #[test]
    fn unsupported_schemes_are_counted_not_dropped_silently() {
        let text = format!(
            "vless://{UUID}@example.com:443\n\
             tuic://x@y:443\n\
             \n\
             not a link at all\n"
        );
        let (servers, skipped) = parse_links_reporting(&text);
        assert_eq!(servers.len(), 1);
        assert_eq!(skipped.lines, 1, "only the tuic line looks like a link");
        // Naming it is the difference between "some servers are missing" and
        // "your provider offers a protocol this app does not speak".
        assert_eq!(skipped.schemes, ["tuic"]);
    }

    fn vmess_link(json: &str) -> String {
        let payload = base64::engine::general_purpose::STANDARD_NO_PAD.encode(json.as_bytes());
        format!("vmess://{payload}")
    }

    #[test]
    fn ipv6_literals_lose_their_url_brackets() {
        // `to_socket_addrs` and `ping` both reject the bracketed spelling, so a
        // server that kept them could connect but never be measured.
        for (link, port) in [
            (
                "vless://uuid@[2001:db8::1]:443?encryption=none&type=tcp#Six",
                443,
            ),
            ("trojan://secret@[2001:db8::1]:443#Six", 443),
            ("socks://[2001:db8::1]:1080#Six", 1080),
            ("ss://YWVzLTI1Ni1nY206cGFzcw@[2001:db8::1]:8388#Six", 8388),
        ] {
            let server = parse_link(link).unwrap_or_else(|| panic!("failed to parse {link}"));
            assert_eq!(server.address, "2001:db8::1", "for {link}");
            assert_eq!(server.port, port, "for {link}");
        }
    }

    #[test]
    fn shadowsocks_userinfo_survives_percent_encoding() {
        // Generators percent-escape the base64 blob; `%2F` and `%3D` are in no
        // base64 alphabet, so decoding before unescaping mistakes the whole
        // thing for a plaintext method and drops the server.
        let plain = parse_link("ss://YWVzLTI1Ni1nY206cGEvc3M@example.com:8388#SS").unwrap();
        let escaped = parse_link("ss://YWVzLTI1Ni1nY206cGEvc3M%3D@example.com:8388#SS").unwrap();
        for server in [&plain, &escaped] {
            let OutboundSpec::Shadowsocks { method, password } = &server.spec else {
                panic!("expected a shadowsocks outbound");
            };
            assert_eq!(method, "aes-256-gcm");
            assert_eq!(password, "pa/ss");
        }
    }

    #[test]
    fn vmess_rejects_a_port_that_does_not_fit() {
        // `as u16` used to wrap 65536 to 0 and hand xray a config it refuses.
        let link = vmess_link(r#"{"add":"example.com","port":"65536","id":"uuid","ps":"Wrapped"}"#);
        assert!(parse_link(&link).is_none());
    }

    #[test]
    fn vmess_rejects_an_alter_id_that_does_not_fit() {
        // `as u32` used to wrap 4294967296 to 0, silently changing the
        // AlterID the handshake is made with.
        let link = vmess_link(
            r#"{"add":"example.com","port":"443","id":"uuid","aid":"4294967296","ps":"Wide"}"#,
        );
        assert!(parse_link(&link).is_none());
    }

    #[test]
    fn vmess_hosts_are_normalized_like_every_other_scheme() {
        let link = vmess_link(r#"{"add":"[2001:db8::1]","port":"443","id":"uuid","ps":"Six"}"#);
        let server = parse_link(&link).unwrap();
        assert_eq!(server.address, "2001:db8::1");
    }

    #[test]
    fn socks5_and_mixed_case_schemes_are_accepted() {
        let servers = parse_links("socks5://example.com:1080#One\nVLESS://u@example.com:443#Two");
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].protocol, Protocol::Socks);
        assert_eq!(servers[1].protocol, Protocol::Vless);
    }
}
