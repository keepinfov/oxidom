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
        }
    }
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
pub fn transport_label(protocol: Protocol, stream: Option<&StreamSettings>) -> String {
    let mut parts = vec![protocol.as_str().to_string()];
    if let Some(s) = stream {
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
    use super::{Server, StreamSettings, normalize_pin_sha256};

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
