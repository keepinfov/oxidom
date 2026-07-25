use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

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
    /// Last latency probe result (runtime only, not persisted).
    #[serde(skip)]
    pub latency_ms: Option<u32>,
}

impl Server {
    pub fn stable_id(link: &str) -> String {
        let mut h = DefaultHasher::new();
        link.hash(&mut h);
        format!("{:016x}", h.finish())
    }

    /// Whether two subscription entries describe the same Xray connection.
    /// Display names and share-link formatting are deliberately ignored.
    pub fn same_connection_as(&self, other: &Self) -> bool {
        self.protocol == other.protocol
            && self.address == other.address
            && self.port == other.port
            && self.spec == other.spec
    }
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

/// Extract an ISO 3166-1 alpha-2 code from a leading flag emoji, if present.
pub fn country_from_name(name: &str) -> Option<String> {
    let mut chars = name.chars().peekable();
    // Skip leading whitespace.
    while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
        chars.next();
    }
    let a = chars.next()?;
    let b = chars.next()?;
    let ai = regional_indicator_to_letter(a)?;
    let bi = regional_indicator_to_letter(b)?;
    Some(format!("{ai}{bi}"))
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
