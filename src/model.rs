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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamSettings {
    /// tcp | ws | grpc | xhttp | splithttp | h2
    pub network: String,
    /// none | tls | reality
    pub security: String,
    pub sni: Option<String>,
    pub alpn: Option<Vec<String>>,
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub allow_insecure: bool,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Original share link (kept for debugging / re-parsing).
    pub link: String,
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
            servers: Vec::new(),
            updated_at: None,
        }
    }
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
