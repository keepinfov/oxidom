use std::collections::HashMap;

use base64::Engine as _;
use percent_encoding::percent_decode_str;
use serde_json::Value;
use url::Url;

use crate::model::{
    OutboundSpec, Protocol, Server, StreamSettings, country_from_name, normalize_pin_sha256,
    transport_label,
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
    Server {
        id: Server::stable_id(link),
        transport_label: transport_label(protocol, &spec),
        country,
        name,
        protocol,
        address,
        port,
        spec,
        link: Some(link.to_string()),
        latency_ms: None,
    }
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
    let host = url.host_str()?.to_string();
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
    let host = url.host_str()?.to_string();
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

    let host = s("add")?;
    let port = num("port")? as u16;
    let uuid = s("id")?;
    let alter_id = num("aid").unwrap_or(0) as u32;
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
        // SIP002: userinfo is base64(method:password), hostport is host:port
        let decoded = b64_decode(userinfo)
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_else(|| userinfo.to_string());
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

fn parse_socks(link: &str) -> Option<Server> {
    let url = Url::parse(link).ok()?;
    let host = url.host_str()?.to_string();
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
    let host = url.host_str()?.to_string();
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
    let (host, port) = s.rsplit_once(':')?;
    let port: u16 = port.trim().parse().ok()?;
    Some((host.to_string(), port))
}

/// Parse a newline list of share links into servers, skipping unrecognized lines.
pub fn parse_links(text: &str) -> Vec<Server> {
    parse_links_counting(text).0
}

/// Like `parse_links`, but also counts lines that look like share links yet
/// use an unsupported or malformed scheme (hysteria2://, tuic://, …), so the
/// caller can tell the user instead of dropping servers silently.
pub fn parse_links_counting(text: &str) -> (Vec<Server>, usize) {
    let mut servers = Vec::new();
    let mut unsupported = 0;
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }
        match parse_link(line) {
            Some(server) => servers.push(server),
            None if line.contains("://") => unsupported += 1,
            None => {}
        }
    }
    (servers, unsupported)
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn unsupported_schemes_are_counted_not_dropped_silently() {
        let text = format!(
            "vless://{UUID}@example.com:443\n\
             tuic://x@y:443\n\
             \n\
             not a link at all\n"
        );
        let (servers, unsupported) = parse_links_counting(&text);
        assert_eq!(servers.len(), 1);
        assert_eq!(unsupported, 1, "only the tuic line looks like a link");
    }
}
