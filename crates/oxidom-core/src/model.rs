use serde::{Deserialize, Serialize};

use crate::link::Skipped;
use crate::subscription_format::NotTaken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Vless,
    Vmess,
    Trojan,
    Shadowsocks,
    Socks,
    Http,
    Hysteria2,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Vless => "vless",
            Protocol::Vmess => "vmess",
            Protocol::Trojan => "trojan",
            Protocol::Shadowsocks => "shadowsocks",
            Protocol::Socks => "socks",
            Protocol::Http => "http",
            Protocol::Hysteria2 => "hysteria2",
        }
    }
}

/// An inclusive port range, for hysteria2 port hopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    /// Parse one range. Share links and Clash write `5000-6000`, sing-box
    /// writes `5000:6000`, and a bare `5000` is a range of one.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let (start, end) = match raw.split_once(['-', ':']) {
            Some((start, end)) => (start.trim().parse().ok()?, end.trim().parse().ok()?),
            None => {
                let only: u16 = raw.parse().ok()?;
                (only, only)
            }
        };
        (start <= end && start != 0).then_some(PortRange { start, end })
    }

    /// The `5000-6000` spelling Xray's `udpHop.ports` expects.
    pub fn to_xray(self) -> String {
        if self.start == self.end {
            self.start.to_string()
        } else {
            format!("{}-{}", self.start, self.end)
        }
    }
}

/// Hysteria2 obfuscation. Only `salamander` exists today, but the type is
/// carried verbatim so an unknown one can be reported rather than guessed at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hysteria2Obfs {
    pub kind: String,
    pub password: String,
}

/// Settings for a hysteria2 outbound.
///
/// Deliberately not a [`StreamSettings`]: hysteria2 is QUIC, so all but three
/// of that struct's fields (reality keys, websocket path, grpc service name,
/// vless flow…) are meaningless here, and its `network`/`security` pair does
/// not describe anything a user typed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hysteria2Settings {
    pub sni: Option<String>,
    pub alpn: Option<Vec<String>>,
    /// Xray 26.x removed `allowInsecure`; kept only to drive a warning.
    #[serde(default)]
    pub allow_insecure: bool,
    /// Hex SHA-256 of the peer certificate, normalized by [`normalize_pin_sha256`].
    pub pin_sha256: Option<String>,
    pub obfs: Option<Hysteria2Obfs>,
    /// Bandwidth hints, normalized to whole mbps so that the same server
    /// written as `100`, `"100 Mbps"` and `"100mbps"` stays one server.
    pub up_mbps: Option<u32>,
    pub down_mbps: Option<u32>,
    /// Extra ranges for port hopping. The primary port lives on `Server.port`.
    #[serde(default)]
    pub port_hop: Vec<PortRange>,
    pub hop_interval_secs: Option<u32>,
    pub congestion: Option<String>,
    pub udp_idle_timeout_secs: Option<u32>,
}

/// Normalize a bandwidth hint to whole mbps.
///
/// Providers spell the same number as `100`, `"100 Mbps"`, `"100mbps"` or
/// `"1 gbps"`. Storing the text verbatim would make two identical servers
/// compare unequal and orphan the user's saved node on the next refresh.
pub fn parse_bandwidth_mbps(raw: &str) -> Option<u32> {
    let raw = raw.trim().to_ascii_lowercase();
    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    let value: u64 = digits.parse().ok()?;
    let unit = raw[digits.len()..].trim();
    let mbps = match unit {
        "g" | "gbps" | "gb" | "gbit" => value * 1000,
        "k" | "kbps" | "kb" | "kbit" => value.div_ceil(1000),
        // Bare numbers are mbps by convention in every format we import.
        _ => value,
    };
    // A zero would read as "unlimited" to hysteria; treat a rounded-down
    // sub-mbps value as the smallest meaningful hint instead.
    Some(mbps.clamp(1, u32::MAX as u64) as u32)
}

/// Transport + security settings shared by vless/vmess/trojan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSettings {
    /// tcp | ws | grpc | xhttp | splithttp | h2
    pub network: String,
    /// none | tls | reality
    pub security: String,
    pub sni: Option<String>,
    pub alpn: Option<Vec<String>>,
    pub fingerprint: Option<String>,
    /// The link asked to skip certificate verification. Xray 26.x **removed**
    /// `allowInsecure` (it is now a hard startup failure), so this can no longer
    /// be honored — it only drives a warning. See [`crate::xray::config`].
    #[serde(default)]
    pub allow_insecure: bool,
    /// Hex SHA-256 of the peer certificate. The only escape hatch Xray still
    /// offers for a certificate it would otherwise reject.
    #[serde(default)]
    pub pin_sha256: Option<String>,
    // reality
    pub public_key: Option<String>,
    pub short_id: Option<String>,
    pub spider_x: Option<String>,
    // transport specifics
    pub path: Option<String>,
    pub host: Option<String>,
    pub service_name: Option<String>,
    /// XHTTP request mode. Absent means Xray's `auto` default.
    #[serde(default)]
    pub xhttp_mode: Option<String>,
    /// gRPC `:authority` override.
    #[serde(default)]
    pub grpc_authority: Option<String>,
    /// Whether the gRPC transport uses Xray's multi-mode framing.
    #[serde(default)]
    pub grpc_multi_mode: bool,
    pub header_type: Option<String>,
    /// vless flow, e.g. xtls-rprx-vision
    pub flow: Option<String>,
}

/// Everything needed to emit an Xray outbound for one server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum OutboundSpec {
    Vless {
        uuid: String,
        #[serde(default = "default_none_encryption")]
        encryption: String,
        stream: StreamSettings,
    },
    Vmess {
        uuid: String,
        #[serde(default)]
        alter_id: u32,
        #[serde(default = "default_auto")]
        security: String,
        stream: StreamSettings,
    },
    Trojan {
        password: String,
        stream: StreamSettings,
    },
    Shadowsocks {
        method: String,
        password: String,
    },
    Socks {
        username: Option<String>,
        password: Option<String>,
    },
    Http {
        username: Option<String>,
        password: Option<String>,
    },
    Hysteria2 {
        auth: String,
        settings: Hysteria2Settings,
    },
    /// A provider-supplied Xray profile that needs multiple proxy outbounds and
    /// an Xray balancer. Local inbounds and safe routing remain owned by oxidom.
    XrayProfile {
        proxy_outbounds: Vec<serde_json::Value>,
        balancers: Vec<serde_json::Value>,
        burst_observatory: Option<serde_json::Value>,
        balancer_tag: String,
    },
}

impl OutboundSpec {
    /// Whether these describe the same connection, ignoring marks the user
    /// made locally.
    ///
    /// A pinned certificate is one such mark: the provider never sends it, so
    /// comparing it would mean a server stopped matching its own refreshed
    /// entry the moment someone trusted its certificate — taking the alias and
    /// the stable id down with the pin. The clone happens only when the cheap
    /// comparison already failed, and these lists are hundreds of entries at
    /// most.
    pub fn same_connection_as(&self, other: &Self) -> bool {
        if self == other {
            return true;
        }
        let mut left = self.clone();
        let mut right = other.clone();
        for spec in [&mut left, &mut right] {
            if let Some(stream) = spec.stream_mut() {
                stream.pin_sha256 = None;
            }
        }
        left == right
    }

    /// The same block, to write to. Only one thing writes to it — pinning a
    /// certificate the user chose to trust — and that is deliberately a
    /// separate act from parsing a link.
    pub fn stream_mut(&mut self) -> Option<&mut StreamSettings> {
        match self {
            OutboundSpec::Vless { stream, .. }
            | OutboundSpec::Vmess { stream, .. }
            | OutboundSpec::Trojan { stream, .. } => Some(stream),
            OutboundSpec::Shadowsocks { .. }
            | OutboundSpec::Socks { .. }
            | OutboundSpec::Http { .. }
            | OutboundSpec::Hysteria2 { .. }
            | OutboundSpec::XrayProfile { .. } => None,
        }
    }

    /// The transport/security block, for the variants that have one. Lets
    /// callers ask about TLS without matching every variant themselves.
    pub fn stream(&self) -> Option<&StreamSettings> {
        match self {
            OutboundSpec::Vless { stream, .. }
            | OutboundSpec::Vmess { stream, .. }
            | OutboundSpec::Trojan { stream, .. } => Some(stream),
            OutboundSpec::Shadowsocks { .. }
            | OutboundSpec::Socks { .. }
            | OutboundSpec::Http { .. }
            // Hysteria2 carries its own settings type; see `Hysteria2Settings`.
            | OutboundSpec::Hysteria2 { .. }
            | OutboundSpec::XrayProfile { .. } => None,
        }
    }
}

fn default_none_encryption() -> String {
    "none".to_string()
}
fn default_auto() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub address: String,
    pub port: u16,
    /// Human subtitle for the card, e.g. "vless + xhttp + reality".
    pub transport_label: String,
    /// ISO 3166-1 alpha-2 code parsed from a leading flag emoji / name, if any.
    pub country: Option<String>,
    pub spec: OutboundSpec,
    /// Original or normalized share link. Composite profiles cannot be expressed
    /// as one share link and therefore store `None`.
    #[serde(default)]
    pub link: Option<String>,
    /// Human handle for the CLI and for `oxidom@<name>` units. Assigned on load and
    /// carried across subscription refreshes, so a unit name never moves.
    #[serde(default)]
    pub alias: Option<String>,
    /// Last latency probe result (runtime only, not persisted).
    #[serde(skip)]
    pub latency_ms: Option<u32>,
}

impl Server {
    /// FNV-1a. `DefaultHasher` is explicitly not stable across Rust releases, and a
    /// server id that changes with the toolchain silently orphans the active server,
    /// every profile and every unit named after it.
    pub fn stable_id(seed: &str) -> String {
        format!("{:016x}", stable_hash(seed))
    }

    /// The string a server's id is derived from. Kept in one place so that the id
    /// of an already-stored server can be recomputed exactly during migration,
    /// instead of being guessed by comparing fields.
    pub fn identity_string(&self) -> String {
        if let Some(link) = &self.link {
            return link.clone();
        }
        if let OutboundSpec::XrayProfile {
            proxy_outbounds,
            balancers,
            burst_observatory,
            balancer_tag,
        } = &self.spec
            && let Ok(identity) = serde_json::to_string(&serde_json::json!({
                "name": self.name,
                "proxy_outbounds": proxy_outbounds,
                "balancers": balancers,
                "burst_observatory": burst_observatory,
                "balancer_tag": balancer_tag,
            }))
        {
            return identity;
        }
        self.identity_from_serialized_spec(serde_json::to_string(&self.spec).ok())
    }

    fn identity_from_serialized_spec(&self, serialized: Option<String>) -> String {
        serialized.unwrap_or_else(|| format!("{}:{}:{}", self.address, self.port, self.name))
    }

    /// Whether two subscription entries describe the same Xray connection.
    /// Display names and share-link formatting are deliberately ignored.
    pub fn same_connection_as(&self, other: &Self) -> bool {
        self.protocol == other.protocol
            && self.address == other.address
            && self.port == other.port
            && self.spec.same_connection_as(&other.spec)
    }
}

/// The stable 64-bit value behind persisted ids and profile bind addresses.
/// Keeping both consumers on one implementation prevents a toolchain update
/// from moving either identity unexpectedly.
pub(crate) fn stable_hash(seed: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserInfo {
    pub upload: u64,
    pub download: u64,
    pub total: u64,
    /// Unix timestamp of expiry, if provided.
    pub expire: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub name: String,
    pub url: String,
    pub description: Option<String>,
    pub userinfo: Option<UserInfo>,
    /// OPT-IN device identifier on fetch. Default false (max privacy).
    #[serde(default)]
    pub send_hwid: bool,
    /// Per-subscription User-Agent override. When `None` the global config
    /// value is used. Lets a single provider that expects a specific client
    /// (e.g. `Happ/3.13.0`) work without changing the global default.
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub servers: Vec<Server>,
    /// What the last refresh could not parse. Stored rather than reported once
    /// and forgotten: the question it answers ("why are there ten servers
    /// here?") is asked long after the refresh, and a toast is gone by then.
    #[serde(default, skip_serializing_if = "Skipped::is_empty")]
    pub skipped: Skipped,
    /// What the last refresh read and deliberately did not apply. Stored for
    /// the same reason `skipped` is: the question it answers is asked long
    /// after the refresh that would have toasted it.
    #[serde(default, skip_serializing_if = "NotTaken::is_empty")]
    pub not_taken: NotTaken,
    pub updated_at: Option<i64>,
}

impl Subscription {
    pub fn new(url: String, name: Option<String>) -> Self {
        Subscription {
            id: Server::stable_id(&url),
            name: name.unwrap_or_else(|| url.clone()),
            url,
            description: None,
            userinfo: None,
            send_hwid: false,
            user_agent: None,
            servers: Vec::new(),
            skipped: Skipped::default(),
            not_taken: NotTaken::default(),
            updated_at: None,
        }
    }
}

/// Normalize a certificate pin to the lowercase hex Xray expects.
///
/// Share links spell the same digest several ways — bare hex, colon-separated
/// hex (the `openssl x509 -fingerprint` output), and occasionally base64. Xray
/// parses the value as hex and rejects anything that is not exactly 32 bytes,
/// so normalize here and drop what cannot be represented rather than handing
/// the core a config it will refuse to start with.
pub fn normalize_pin_sha256(raw: &str) -> Option<String> {
    let hex: String = raw
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-'))
        .collect::<String>()
        .to_ascii_lowercase();
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(hex);
    }
    // Some panels emit the digest base64-encoded; convert it to hex.
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .ok()?;
    (decoded.len() == 32).then(|| {
        decoded
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    })
}

/// Extract an ISO 3166-1 alpha-2 code from a server name.
///
/// Two spellings, in this order: a leading flag emoji, and a leading two-letter
/// token — `🇩🇪 Frankfurt` and `DE-2 HYSTERIA2` both read as `DE`. Only the
/// first token is considered, and only when it is an assigned code: `IS`, `IT`,
/// `NO`, `ME` and `AT` are all countries *and* ordinary words, so matching them
/// anywhere in a name would decorate half a provider's list with the wrong
/// flags. `second-ws-stas` keeps its `second`, not Samoa.
///
/// A name that says nothing about its country stays `None` rather than being
/// guessed at. Whether the address could be geolocated instead is a separate
/// question, and a heavier one.
pub fn country_from_name(name: &str) -> Option<String> {
    let trimmed = name.trim_start();
    let mut chars = trimmed.chars();
    if let (Some(a), Some(b)) = (chars.next(), chars.next())
        && let (Some(ai), Some(bi)) = (
            regional_indicator_to_letter(a),
            regional_indicator_to_letter(b),
        )
    {
        return Some(format!("{ai}{bi}"));
    }
    let token = trimmed
        .split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | '|' | '·' | '.' | '[' | '('))
        .find(|part| !part.is_empty())?;
    crate::country::is_alpha2(token).then(|| token.to_ascii_uppercase())
}

/// Drop a leading flag emoji (and the whitespace after it) from a display name;
/// the flag is now shown as a separate icon. Returns the name unchanged when it
/// has no leading flag.
pub fn name_without_flag(name: &str) -> &str {
    let trimmed = name.trim_start();
    let mut chars = trimmed.chars();
    if let (Some(a), Some(b)) = (chars.next(), chars.next())
        && regional_indicator_to_letter(a).is_some()
        && regional_indicator_to_letter(b).is_some()
    {
        return chars.as_str().trim_start();
    }
    name
}

fn regional_indicator_to_letter(c: char) -> Option<char> {
    let cp = c as u32;
    if (0x1F1E6..=0x1F1FF).contains(&cp) {
        let letter = (b'A' + (cp - 0x1F1E6) as u8) as char;
        Some(letter)
    } else {
        None
    }
}

/// Build the "vless + xhttp + reality"-style subtitle.
///
/// Takes the whole spec rather than a stream: not every protocol has a
/// [`StreamSettings`], and those that don't still have something to say.
pub fn transport_label(protocol: Protocol, spec: &OutboundSpec) -> String {
    let mut parts = vec![protocol.as_str().to_string()];
    if let OutboundSpec::Hysteria2 { settings, .. } = spec {
        // Not "+ tls" (always true, so it carries no information) and not the
        // Xray-internal transport name "hysteria" — but the obfuscation is
        // worth surfacing, since it is the usual reason a server won't connect.
        if let Some(obfs) = &settings.obfs {
            parts.push(obfs.kind.clone());
        }
        return parts.join(" + ");
    }
    if let Some(s) = spec.stream() {
        if !s.network.is_empty() && s.network != "tcp" {
            parts.push(s.network.clone());
        }
        match s.security.as_str() {
            "" | "none" => {}
            other => parts.push(other.to_string()),
        }
    }
    parts.join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// The reported case: a provider whose names spell the country in ASCII.
    /// Every one of these read as no country at all, so every card in the list
    /// showed a globe.
    #[test]
    fn a_leading_country_code_is_read_from_an_ordinary_name() {
        assert_eq!(country_from_name("DE-2 HYSTERIA2").as_deref(), Some("DE"));
        assert_eq!(country_from_name("DE-2 WS").as_deref(), Some("DE"));
        assert_eq!(country_from_name("fi_helsinki").as_deref(), Some("FI"));
        assert_eq!(country_from_name("nl · amsterdam").as_deref(), Some("NL"));
    }

    #[test]
    fn a_flag_emoji_still_wins_and_still_reads_uppercase() {
        assert_eq!(country_from_name("🇳🇱 Node").as_deref(), Some("NL"));
        assert_eq!(country_from_name("  🇨🇭 Trojan").as_deref(), Some("CH"));
    }

    /// Two-letter words that are also countries — `IS`, `IT`, `NO`, `ME`, `AT`,
    /// `WS` — are why only the first token counts. Matching anywhere would put
    /// Samoa on `second-ws-stas` and Italy on anything mentioning it.
    #[test]
    fn a_country_code_elsewhere_in_the_name_is_not_a_country() {
        for name in [
            "second-ws-stas",
            "basa-stas",
            "petros-main",
            "jellyfin-hysteria2",
            "vaultwarden-xray-ws",
            "node-it-01",
            "backup no 2",
        ] {
            assert_eq!(country_from_name(name), None, "{name}");
        }
    }

    /// A leading pair of letters that is not an assigned code stays unknown
    /// rather than becoming a country nobody can point to on a map.
    #[test]
    fn a_leading_non_country_stays_unknown() {
        for name in ["XX-1 node", "AA gateway", "ab-relay", "zz"] {
            assert_eq!(country_from_name(name), None, "{name}");
        }
    }

    fn identity_server(spec: OutboundSpec) -> Server {
        Server {
            id: String::new(),
            name: "Node".to_string(),
            protocol: Protocol::Socks,
            address: "example.com".to_string(),
            port: 1080,
            transport_label: "socks".to_string(),
            country: None,
            spec,
            link: None,
            alias: None,
            latency_ms: None,
        }
    }

    #[test]
    fn stable_id_is_frozen() {
        // The whole point of the phase: this must fail loudly if the algorithm ever
        // changes, because on-disk ids and systemd unit names depend on it.
        assert_eq!(
            Server::stable_id("vless://uuid@example.com:443#node"),
            "e113e764d060247a"
        );
    }

    #[test]
    fn identity_string_covers_every_source() {
        let mut linked = identity_server(OutboundSpec::Socks {
            username: None,
            password: None,
        });
        linked.link = Some("socks://example.com:1080#Node".to_string());
        assert_eq!(linked.identity_string(), "socks://example.com:1080#Node");

        let profile = identity_server(OutboundSpec::XrayProfile {
            proxy_outbounds: vec![serde_json::json!({"tag": "proxy"})],
            balancers: vec![serde_json::json!({"tag": "balance"})],
            burst_observatory: None,
            balancer_tag: "balance".to_string(),
        });
        assert_eq!(
            profile.identity_string(),
            r#"{"balancer_tag":"balance","balancers":[{"tag":"balance"}],"burst_observatory":null,"name":"Node","proxy_outbounds":[{"tag":"proxy"}]}"#
        );

        let serialized = identity_server(OutboundSpec::Socks {
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
        });
        assert_eq!(
            serialized.identity_string(),
            r#"{"kind":"socks","username":"user","password":"pass"}"#
        );

        // Every current OutboundSpec serializes to JSON. Exercise the final
        // compatibility fallback directly so it cannot silently drift if a future
        // variant introduces a fallible value.
        assert_eq!(
            serialized.identity_from_serialized_spec(None),
            "example.com:1080:Node"
        );
    }

    #[test]
    fn pin_accepts_every_spelling_of_the_same_digest() {
        let colons = "E3:B0:C4:42:98:FC:1C:14:9A:FB:F4:C8:99:6F:B9:24:\
                      27:AE:41:E4:64:9B:93:4C:A4:95:99:1B:78:52:B8:55";
        let base64 = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        for raw in [DIGEST, &DIGEST.to_uppercase(), colons, base64] {
            assert_eq!(normalize_pin_sha256(raw).as_deref(), Some(DIGEST), "{raw}");
        }
    }

    /// Xray rejects a pin that is not exactly 32 bytes, so a bad value must be
    /// dropped here rather than becoming a core that refuses to start.
    #[test]
    fn pin_rejects_values_xray_would_refuse() {
        for raw in [
            "",
            "e3b0c442",
            "not-a-digest",
            &DIGEST[..63],
            &format!("{DIGEST}ff"),
        ] {
            assert_eq!(normalize_pin_sha256(raw), None, "{raw}");
        }
    }

    /// Servers cached before `pin_sha256` existed must still load; a failure
    /// here quarantines the user's whole subscription cache.
    #[test]
    fn stream_settings_cached_without_a_pin_still_loads() {
        let json = r#"{"network":"tcp","security":"tls","sni":null,"alpn":null,
          "fingerprint":null,"allow_insecure":true,"public_key":null,"short_id":null,
          "spider_x":null,"path":null,"host":null,"service_name":null,"header_type":null,
          "flow":null}"#;
        let stream: StreamSettings = serde_json::from_str(json).unwrap();
        assert!(stream.allow_insecure);
        assert_eq!(stream.pin_sha256, None);
    }

    #[test]
    fn port_ranges_accept_every_separator_in_use() {
        // `-` in share links and Clash, `:` in sing-box, bare = a range of one.
        assert_eq!(
            PortRange::parse("5000-6000"),
            Some(PortRange {
                start: 5000,
                end: 6000
            })
        );
        assert_eq!(
            PortRange::parse(" 5000 : 6000 "),
            Some(PortRange {
                start: 5000,
                end: 6000
            })
        );
        assert_eq!(
            PortRange::parse("7000"),
            Some(PortRange {
                start: 7000,
                end: 7000
            })
        );
        for bad in ["", "abc", "6000-5000", "0-100", "70000"] {
            assert_eq!(PortRange::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn port_ranges_render_the_way_xray_reads_them() {
        assert_eq!(
            PortRange {
                start: 5000,
                end: 6000
            }
            .to_xray(),
            "5000-6000"
        );
        assert_eq!(
            PortRange {
                start: 443,
                end: 443
            }
            .to_xray(),
            "443"
        );
    }

    /// The same bandwidth written four ways must land on one value, or two
    /// identical servers stop comparing equal and the saved one is orphaned.
    #[test]
    fn bandwidth_normalizes_to_whole_mbps() {
        for raw in ["100", "100mbps", "100 Mbps", " 100 MBPS "] {
            assert_eq!(parse_bandwidth_mbps(raw), Some(100), "{raw}");
        }
        assert_eq!(parse_bandwidth_mbps("1 gbps"), Some(1000));
        // Sub-mbps rounds up: zero would read as "unlimited" to hysteria.
        assert_eq!(parse_bandwidth_mbps("512 kbps"), Some(1));
        assert_eq!(parse_bandwidth_mbps("0"), Some(1));
        assert_eq!(parse_bandwidth_mbps("fast"), None);
    }

    #[test]
    fn hysteria2_label_names_the_obfuscation_and_nothing_else() {
        let plain = OutboundSpec::Hysteria2 {
            auth: "pw".to_string(),
            settings: Hysteria2Settings::default(),
        };
        assert_eq!(transport_label(Protocol::Hysteria2, &plain), "hysteria2");

        let obfuscated = OutboundSpec::Hysteria2 {
            auth: "pw".to_string(),
            settings: Hysteria2Settings {
                obfs: Some(Hysteria2Obfs {
                    kind: "salamander".to_string(),
                    password: "o".to_string(),
                }),
                ..Default::default()
            },
        };
        assert_eq!(
            transport_label(Protocol::Hysteria2, &obfuscated),
            "hysteria2 + salamander"
        );
    }

    #[test]
    fn hysteria2_spec_round_trips_through_serde() {
        let spec = OutboundSpec::Hysteria2 {
            auth: "pw".to_string(),
            settings: Hysteria2Settings {
                sni: Some("h.example".to_string()),
                port_hop: vec![PortRange {
                    start: 5000,
                    end: 6000,
                }],
                up_mbps: Some(100),
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains(r#""kind":"hysteria2""#), "{json}");
        assert_eq!(serde_json::from_str::<OutboundSpec>(&json).unwrap(), spec);
    }

    #[test]
    fn old_cached_string_link_deserializes_as_some() {
        let json = r#"{
          "id":"server","name":"Example","protocol":"vless","address":"example.com","port":443,
          "transport_label":"vless","country":null,
          "spec":{"kind":"vless","uuid":"id","encryption":"none","stream":{"network":"tcp","security":"none","sni":null,"alpn":null,"fingerprint":null,"allow_insecure":false,"public_key":null,"short_id":null,"spider_x":null,"path":null,"host":null,"service_name":null,"header_type":null,"flow":null}},
          "link":"vless://id@example.com:443"
        }"#;
        let server: Server = serde_json::from_str(json).unwrap();
        assert_eq!(server.link.as_deref(), Some("vless://id@example.com:443"));
    }
}
