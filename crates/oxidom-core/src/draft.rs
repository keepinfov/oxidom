//! A server described as fields rather than as a share link.
//!
//! The draft is the wire and dialog shape for creating a server by hand: it
//! carries only what a person authors. Everything derived — the id, the
//! transport label, the country, the alias — is computed here, the same way
//! the link parsers compute it, so a hand-made server and a link-imported one
//! are one kind of thing. Field names are serde names on the model types
//! deliberately: the JSON key a dialog labels is the key that reaches the
//! stored server.

use serde::{Deserialize, Serialize};

use crate::model::{
    Hysteria2Settings, OutboundSpec, Protocol, Server, StreamSettings, country_from_name,
    transport_label,
};

/// What a client sends to create (or, resolved, to edit) a server.
///
/// Per-protocol requirements are enforced by [`resolve`], not by the type:
/// a dialog fills the struct incrementally and wants one place that says
/// what is still missing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerDraft {
    /// Display name. Empty falls back to `address:port`, as an unnamed link does.
    #[serde(default)]
    pub name: String,
    pub protocol: Protocol,
    pub address: String,
    pub port: u16,
    /// `id` in the generated outbound — vless and vmess.
    #[serde(default)]
    pub uuid: Option<String>,
    /// vless `encryption`; defaults to `none`.
    #[serde(default)]
    pub encryption: Option<String>,
    /// vmess `alterId`; defaults to 0.
    #[serde(default)]
    pub alter_id: Option<u32>,
    /// vmess user `security`; defaults to `auto`.
    #[serde(default)]
    pub security: Option<String>,
    /// shadowsocks `method`.
    #[serde(default)]
    pub method: Option<String>,
    /// `password` — trojan, shadowsocks, socks, http.
    #[serde(default)]
    pub password: Option<String>,
    /// `user` — socks, http.
    #[serde(default)]
    pub username: Option<String>,
    /// hysteria2 `auth`.
    #[serde(default)]
    pub auth: Option<String>,
    /// Transport and TLS/Reality for vless, vmess and trojan.
    #[serde(default)]
    pub stream: Option<StreamSettings>,
    /// The hysteria2 settings block, for hysteria2.
    #[serde(default)]
    pub hysteria2: Option<Hysteria2Settings>,
    /// Raw JSON merged onto the generated outbound (RFC 7396). The escape
    /// hatch for a core option the form does not model.
    #[serde(default)]
    pub outbound_patch: Option<serde_json::Value>,
}

impl Default for ServerDraft {
    fn default() -> Self {
        ServerDraft {
            name: String::new(),
            protocol: Protocol::Vless,
            address: String::new(),
            port: 0,
            uuid: None,
            encryption: None,
            alter_id: None,
            security: None,
            method: None,
            password: None,
            username: None,
            auth: None,
            stream: None,
            hysteria2: None,
            outbound_patch: None,
        }
    }
}

/// Why a draft creates nothing. Every variant names the field it is about,
/// because the message is what a dialog or a CLI error shows next to it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DraftError {
    #[error("address is empty")]
    EmptyAddress,
    #[error("port is 0")]
    ZeroPort,
    #[error("{0} is required for {1}")]
    MissingField(&'static str, &'static str),
    #[error("outbound_patch is not a JSON object")]
    PatchNotObject,
    #[error("outbound_patch must not set {0:?}; that is what the typed fields are for")]
    PatchSetsReserved(&'static str),
    #[error("the draft does not generate an outbound")]
    DoesNotGenerate,
}

/// The two keys a patch may not touch: they are what the protocol choice and
/// the generator own, and a patch that rewrote them would produce an outbound
/// the rest of the application no longer describes.
const RESERVED_PATCH_KEYS: [&str; 2] = ["tag", "protocol"];

/// Turn a draft into a stored server, or say which field prevents it.
///
/// Validation ends by generating: the resolved server is pushed through the
/// same `outbound_tagged` every connect uses, patch merged, so nothing can be
/// created that the generator would later refuse to describe.
pub fn resolve(draft: &ServerDraft) -> Result<Server, DraftError> {
    let address = draft.address.trim().to_string();
    if address.is_empty() {
        return Err(DraftError::EmptyAddress);
    }
    if draft.port == 0 {
        return Err(DraftError::ZeroPort);
    }
    if let Some(patch) = &draft.outbound_patch {
        let object = patch.as_object().ok_or(DraftError::PatchNotObject)?;
        for key in RESERVED_PATCH_KEYS {
            if object.contains_key(key) {
                return Err(DraftError::PatchSetsReserved(key));
            }
        }
    }

    let required = |value: &Option<String>, field: &'static str, protocol: &'static str| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or(DraftError::MissingField(field, protocol))
    };
    let stream = || normalized_stream(draft.stream.clone().unwrap_or_default());
    let optional = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };

    let spec = match draft.protocol {
        Protocol::Vless => OutboundSpec::Vless {
            uuid: required(&draft.uuid, "uuid", "vless")?,
            encryption: optional(&draft.encryption).unwrap_or_else(|| "none".to_string()),
            stream: stream(),
        },
        Protocol::Vmess => OutboundSpec::Vmess {
            uuid: required(&draft.uuid, "uuid", "vmess")?,
            alter_id: draft.alter_id.unwrap_or(0),
            security: optional(&draft.security).unwrap_or_else(|| "auto".to_string()),
            stream: stream(),
        },
        Protocol::Trojan => OutboundSpec::Trojan {
            password: required(&draft.password, "password", "trojan")?,
            stream: stream(),
        },
        Protocol::Shadowsocks => OutboundSpec::Shadowsocks {
            method: required(&draft.method, "method", "shadowsocks")?,
            password: required(&draft.password, "password", "shadowsocks")?,
        },
        Protocol::Socks => OutboundSpec::Socks {
            username: optional(&draft.username),
            password: optional(&draft.password),
        },
        Protocol::Http => OutboundSpec::Http {
            username: optional(&draft.username),
            password: optional(&draft.password),
        },
        Protocol::Hysteria2 => OutboundSpec::Hysteria2 {
            auth: required(&draft.auth, "auth", "hysteria2")?,
            settings: draft.hysteria2.clone().unwrap_or_default(),
        },
    };

    let name = if draft.name.trim().is_empty() {
        format!("{address}:{}", draft.port)
    } else {
        draft.name.trim().to_string()
    };
    let mut server = Server {
        id: String::new(),
        transport_label: transport_label(draft.protocol, &spec),
        country: country_from_name(&name),
        name,
        protocol: draft.protocol,
        address,
        port: draft.port,
        spec,
        link: None,
        alias: None,
        outbound_patch: draft.outbound_patch.clone(),
        overrides: None,
        latency_ms: None,
    };
    server.id = Server::stable_id(&server.identity_string());

    if crate::xray::config::outbound_tagged(&server, "proxy").is_none() {
        return Err(DraftError::DoesNotGenerate);
    }
    Ok(server)
}

/// A dialog's empty combo and a JSON draft that skipped the block mean the
/// same thing the link query does when it says nothing: plain TCP, no TLS.
fn normalized_stream(mut stream: StreamSettings) -> StreamSettings {
    if stream.network.trim().is_empty() {
        stream.network = "tcp".to_string();
    }
    if stream.security.trim().is_empty() {
        stream.security = "none".to_string();
    }
    stream
}

/// The stored server as the editor shows it: every field a draft carries,
/// read back out of the spec. Editing is this, a dialog, and [`diff`].
pub fn draft_from_server(server: &Server) -> ServerDraft {
    use crate::model::OutboundSpec;
    let (uuid, encryption, alter_id, security, method, password, username, auth, stream, hysteria2) =
        match &server.spec {
            OutboundSpec::Vless {
                uuid,
                encryption,
                stream,
            } => (
                Some(uuid.clone()),
                Some(encryption.clone()),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(stream.clone()),
                None,
            ),
            OutboundSpec::Vmess {
                uuid,
                alter_id,
                security,
                stream,
            } => (
                Some(uuid.clone()),
                None,
                Some(*alter_id),
                Some(security.clone()),
                None,
                None,
                None,
                None,
                Some(stream.clone()),
                None,
            ),
            OutboundSpec::Trojan { password, stream } => (
                None,
                None,
                None,
                None,
                None,
                Some(password.clone()),
                None,
                None,
                Some(stream.clone()),
                None,
            ),
            OutboundSpec::Shadowsocks { method, password } => (
                None,
                None,
                None,
                None,
                Some(method.clone()),
                Some(password.clone()),
                None,
                None,
                None,
                None,
            ),
            OutboundSpec::Socks { username, password }
            | OutboundSpec::Http { username, password } => (
                None,
                None,
                None,
                None,
                None,
                password.clone(),
                username.clone(),
                None,
                None,
                None,
            ),
            OutboundSpec::Hysteria2 { auth, settings } => (
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(auth.clone()),
                None,
                Some(settings.clone()),
            ),
            OutboundSpec::XrayProfile { .. } => {
                (None, None, None, None, None, None, None, None, None, None)
            }
        };
    ServerDraft {
        name: server.name.clone(),
        protocol: server.protocol,
        address: server.address.clone(),
        port: server.port,
        uuid,
        encryption,
        alter_id,
        security,
        method,
        password,
        username,
        auth,
        stream,
        hysteria2,
        outbound_patch: server.outbound_patch.clone(),
    }
}

/// What changed between the draft a dialog was prefilled with and the one
/// it produced: override keys to their new values. Leaves only, the nested
/// blocks dotted — `stream.sni`, `hysteria2.up_mbps` — so an override marks
/// a field, not a whole block. A field the edit removed is recorded as
/// `null`.
pub fn diff(
    before: &ServerDraft,
    after: &ServerDraft,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut changed = std::collections::BTreeMap::new();
    let before = serde_json::to_value(before).unwrap_or(serde_json::Value::Null);
    let after = serde_json::to_value(after).unwrap_or(serde_json::Value::Null);
    diff_objects("", &before, &after, &mut changed);
    changed
}

fn diff_objects(
    prefix: &str,
    before: &serde_json::Value,
    after: &serde_json::Value,
    changed: &mut std::collections::BTreeMap<String, serde_json::Value>,
) {
    let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
        return;
    };
    for (key, value) in after {
        let path = format!("{prefix}{key}");
        match before.get(key) {
            Some(other) if other == value => {}
            Some(other) if other.is_object() && value.is_object() => {
                diff_objects(&format!("{path}."), other, value, changed);
            }
            _ => {
                changed.insert(path, value.clone());
            }
        }
    }
    for key in before.keys() {
        if !after.contains_key(key) {
            changed.insert(format!("{prefix}{key}"), serde_json::Value::Null);
        }
    }
}

/// One field of the stored server, as override keys spell it.
pub fn field_value(server: &Server, key: &str) -> Option<serde_json::Value> {
    let draft = serde_json::to_value(draft_from_server(server)).ok()?;
    let mut current = &draft;
    for part in key.split('.') {
        current = current.get(part)?;
    }
    Some(current.clone())
}

/// Rebuild a server with override values applied, through the same
/// validator a draft passes — the nested blocks ride along whole, so an
/// XHTTP `extra` the provider sent survives an edit that never mentions it.
///
/// The id, the alias, the link and the last reading are carried from the
/// input: an override changes what the server is, not which server it is.
/// The overrides themselves are the caller's to set.
pub fn apply_overrides(
    server: &Server,
    values: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Option<Server> {
    let mut draft = serde_json::to_value(draft_from_server(server)).ok()?;
    let object = draft.as_object_mut()?;
    for (key, value) in values {
        set_dotted(object, key, value.clone());
    }
    let draft: ServerDraft = serde_json::from_value(draft).ok()?;
    let mut resolved = resolve(&draft).ok()?;
    resolved.id = server.id.clone();
    resolved.alias = server.alias.clone();
    resolved.link = server.link.clone();
    resolved.latency_ms = server.latency_ms;
    Some(resolved)
}

fn set_dotted(
    object: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: serde_json::Value,
) {
    let mut parts = key.split('.');
    let Some(first) = parts.next() else { return };
    let mut rest = parts.peekable();
    if rest.peek().is_none() {
        object.insert(first.to_string(), value);
        return;
    }
    let nested = object
        .entry(first.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !nested.is_object() {
        *nested = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(nested) = nested.as_object_mut() {
        set_dotted(nested, &rest.collect::<Vec<_>>().join("."), value);
    }
}

/// One line of the expanded card's parameter listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    /// The dialog group the field sits in, so the card and the editor read
    /// as one thing rather than as two spellings of the same fields.
    pub group: &'static str,
    /// The JSON key the editor labels the field with.
    pub key: &'static str,
    /// The value as a person reads it.
    pub value: String,
    /// A credential: shown masked until revealed.
    pub secret: bool,
    /// The user overrode this field against what the provider sends.
    pub overridden: bool,
}

/// How a listing row spells as an override key: the same names, save for
/// the two nested blocks. Public for the card, which offers the drop
/// action per row.
pub fn override_key(group: &str, key: &str) -> Option<String> {
    match group {
        SERVER | CREDENTIALS => Some(key.to_string()),
        TRANSPORT => Some(format!("stream.{key}")),
        HYSTERIA2 => Some(format!("hysteria2.{key}")),
        PATCH => Some("outbound_patch".to_string()),
        _ => None,
    }
}

const SERVER: &str = "Server";
const CREDENTIALS: &str = "Credentials";
const TRANSPORT: &str = "Transport and TLS";
const HYSTERIA2: &str = "hysteria2";
const PATCH: &str = "outbound_patch";

/// Every parameter the stored server carries, spelled the way the editor
/// spells them.
///
/// Only what is present is listed — an optional the server does not carry is
/// not a parameter of it. A composite Xray profile is deliberately not
/// listed: it is provider JSON with no fields of its own, and a listing that
/// pretends otherwise would be a third spelling nobody can edit.
pub fn parameters(server: &Server) -> Vec<Parameter> {
    use crate::model::OutboundSpec;

    let overridden_keys: std::collections::BTreeSet<String> = server
        .overrides
        .as_ref()
        .map(|overrides| overrides.values.keys().cloned().collect())
        .unwrap_or_default();
    let mut rows = Vec::new();
    let mut plain = |key: &'static str, value: String| {
        rows.push(Parameter {
            group: SERVER,
            key,
            value,
            secret: false,
            overridden: false,
        });
    };
    plain("protocol", server.protocol.as_str().to_string());
    plain("address", server.address.clone());
    plain("port", server.port.to_string());

    let credential = |key: &'static str, value: String, rows: &mut Vec<Parameter>| {
        rows.push(Parameter {
            group: CREDENTIALS,
            key,
            value,
            secret: true,
            overridden: false,
        });
    };

    match &server.spec {
        OutboundSpec::Vless {
            uuid,
            encryption,
            stream,
        } => {
            credential("uuid", uuid.clone(), &mut rows);
            if encryption != "none" {
                rows.push(Parameter {
                    group: CREDENTIALS,
                    key: "encryption",
                    value: encryption.clone(),
                    secret: false,
                    overridden: false,
                });
            }
            stream_parameters(stream, &mut rows);
        }
        OutboundSpec::Vmess {
            uuid,
            alter_id,
            security,
            stream,
        } => {
            credential("uuid", uuid.clone(), &mut rows);
            if *alter_id != 0 {
                rows.push(Parameter {
                    group: CREDENTIALS,
                    key: "alter_id",
                    value: alter_id.to_string(),
                    secret: false,
                    overridden: false,
                });
            }
            if security != "auto" {
                rows.push(Parameter {
                    group: CREDENTIALS,
                    key: "security",
                    value: security.clone(),
                    secret: false,
                    overridden: false,
                });
            }
            stream_parameters(stream, &mut rows);
        }
        OutboundSpec::Trojan { password, stream } => {
            credential("password", password.clone(), &mut rows);
            stream_parameters(stream, &mut rows);
        }
        OutboundSpec::Shadowsocks { method, password } => {
            rows.push(Parameter {
                group: CREDENTIALS,
                key: "method",
                value: method.clone(),
                secret: false,
                overridden: false,
            });
            credential("password", password.clone(), &mut rows);
        }
        OutboundSpec::Socks { username, password } | OutboundSpec::Http { username, password } => {
            for (key, value) in [("username", username), ("password", password)] {
                if let Some(value) = value {
                    credential(key, value.clone(), &mut rows);
                }
            }
        }
        OutboundSpec::Hysteria2 { auth, settings } => {
            credential("auth", auth.clone(), &mut rows);
            let hysteria_rows = &mut rows;
            let mut field = |key: &'static str, value: String, secret: bool| {
                hysteria_rows.push(Parameter {
                    group: HYSTERIA2,
                    key,
                    value,
                    secret,
                    overridden: false,
                });
            };
            if let Some(sni) = &settings.sni {
                field("sni", sni.clone(), false);
            }
            if let Some(alpn) = &settings.alpn {
                field("alpn", alpn.join(", "), false);
            }
            if let Some(obfs) = &settings.obfs {
                field("obfs.password", obfs.password.clone(), true);
            }
            for (key, value) in [
                ("up_mbps", settings.up_mbps),
                ("down_mbps", settings.down_mbps),
                ("hop_interval_secs", settings.hop_interval_secs),
                ("udp_idle_timeout_secs", settings.udp_idle_timeout_secs),
            ] {
                if let Some(value) = value {
                    field(key, value.to_string(), false);
                }
            }
            if let Some(congestion) = &settings.congestion {
                field("congestion", congestion.clone(), false);
            }
            if !settings.port_hop.is_empty() {
                let ranges = settings
                    .port_hop
                    .iter()
                    .map(|range| {
                        if range.start == range.end {
                            range.start.to_string()
                        } else {
                            format!("{}-{}", range.start, range.end)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                field("port_hop", ranges, false);
            }
            if settings.allow_insecure {
                field("allow_insecure", "true".to_string(), false);
            }
            if let Some(pin) = &settings.pin_sha256 {
                field("pin_sha256", pin.clone(), false);
            }
        }
        OutboundSpec::XrayProfile { .. } => {}
    }

    if let Some(patch) = &server.outbound_patch {
        rows.push(Parameter {
            group: PATCH,
            key: "outbound_patch",
            value: serde_json::to_string_pretty(patch).unwrap_or_default(),
            secret: false,
            overridden: false,
        });
    }
    for row in &mut rows {
        if let Some(path) = override_key(row.group, row.key) {
            row.overridden = overridden_keys.contains(&path);
        }
    }
    rows
}

/// The transport and TLS half, shared by the three stream-carrying
/// protocols. Present fields only; `network` and `security` are always
/// carried, normalized to `tcp` and `none` when a link said nothing.
fn stream_parameters(stream: &StreamSettings, rows: &mut Vec<Parameter>) {
    let mut field = |key: &'static str, value: String| {
        rows.push(Parameter {
            group: TRANSPORT,
            key,
            value,
            secret: false,
            overridden: false,
        });
    };
    field("network", stream.network.clone());
    field("security", stream.security.clone());
    for (key, value) in [
        ("sni", &stream.sni),
        ("fingerprint", &stream.fingerprint),
        ("path", &stream.path),
        ("host", &stream.host),
        ("service_name", &stream.service_name),
        ("xhttp_mode", &stream.xhttp_mode),
        ("grpc_authority", &stream.grpc_authority),
        ("header_type", &stream.header_type),
        ("flow", &stream.flow),
        ("public_key", &stream.public_key),
        ("short_id", &stream.short_id),
        ("spider_x", &stream.spider_x),
        ("pin_sha256", &stream.pin_sha256),
    ] {
        if let Some(value) = value {
            field(key, value.clone());
        }
    }
    if let Some(alpn) = &stream.alpn {
        field("alpn", alpn.join(", "));
    }
    if let Some(extra) = &stream.xhttp_extra {
        field(
            "xhttp_extra",
            serde_json::to_string_pretty(extra).unwrap_or_default(),
        );
    }
    for (key, carried) in [
        ("allow_insecure", stream.allow_insecure),
        ("grpc_multi_mode", stream.grpc_multi_mode),
    ] {
        if carried {
            field(key, "true".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::parse_link;
    use crate::xray::config::outbound_tagged;
    use std::collections::BTreeMap;

    fn base(protocol: Protocol) -> ServerDraft {
        ServerDraft {
            name: "Typed".to_string(),
            protocol,
            address: "server.example.invalid".to_string(),
            port: 443,
            ..ServerDraft::default()
        }
    }

    /// The issue's core requirement: a draft is validated through the same
    /// path that generates an Xray outbound. A draft describing the same
    /// connection as a share link produces the same outbound, byte for byte.
    #[test]
    fn a_draft_resolves_to_the_outbound_its_share_link_twin_generates() {
        let cases: Vec<(&str, ServerDraft)> = vec![
            (
                "vless://11111111-2222-3333-4444-555555555555@server.example.invalid:443?type=ws&security=tls&sni=cover.example.invalid&path=%2Fws#Typed",
                ServerDraft {
                    uuid: Some("11111111-2222-3333-4444-555555555555".to_string()),
                    stream: Some(StreamSettings {
                        network: "ws".to_string(),
                        security: "tls".to_string(),
                        sni: Some("cover.example.invalid".to_string()),
                        path: Some("/ws".to_string()),
                        ..StreamSettings::default()
                    }),
                    ..base(Protocol::Vless)
                },
            ),
            (
                "trojan://invented-password@server.example.invalid:443?security=tls&sni=cover.example.invalid#Typed",
                ServerDraft {
                    password: Some("invented-password".to_string()),
                    stream: Some(StreamSettings {
                        network: "tcp".to_string(),
                        security: "tls".to_string(),
                        sni: Some("cover.example.invalid".to_string()),
                        ..StreamSettings::default()
                    }),
                    ..base(Protocol::Trojan)
                },
            ),
            (
                "ss://YWVzLTI1Ni1nY206aW52ZW50ZWQ=@server.example.invalid:443#Typed",
                ServerDraft {
                    method: Some("aes-256-gcm".to_string()),
                    password: Some("invented".to_string()),
                    ..base(Protocol::Shadowsocks)
                },
            ),
            (
                "hysteria2://invented-auth@server.example.invalid:443?sni=cover.example.invalid#Typed",
                ServerDraft {
                    auth: Some("invented-auth".to_string()),
                    hysteria2: Some(Hysteria2Settings {
                        sni: Some("cover.example.invalid".to_string()),
                        ..Hysteria2Settings::default()
                    }),
                    ..base(Protocol::Hysteria2)
                },
            ),
        ];
        for (link, draft) in cases {
            let twin = parse_link(link).expect(link);
            let resolved = resolve(&draft).expect(link);
            assert!(
                resolved.same_connection_as(&twin),
                "{link}: {:?} vs {:?}",
                resolved.spec,
                twin.spec
            );
            assert_eq!(
                outbound_tagged(&resolved, "proxy"),
                outbound_tagged(&twin, "proxy"),
                "{link}"
            );
            assert_eq!(resolved.transport_label, twin.transport_label, "{link}");
        }
    }

    /// vmess separately: its share link is base64 JSON, so the twin is easier
    /// to state as fields against the parser's own literal.
    #[test]
    fn a_vmess_draft_generates_like_its_parsed_twin() {
        // {"v":"2","ps":"Typed","add":"server.example.invalid","port":"443",
        //  "id":"11111111-2222-3333-4444-555555555555","aid":"0","net":"ws",
        //  "tls":"tls","path":"/ws","host":"cover.example.invalid"}
        let link = "vmess://eyJ2IjoiMiIsInBzIjoiVHlwZWQiLCJhZGQiOiJzZXJ2ZXIuZXhhbXBsZS5pbnZhbGlkIiwicG9ydCI6IjQ0MyIsImlkIjoiMTExMTExMTEtMjIyMi0zMzMzLTQ0NDQtNTU1NTU1NTU1NTU1IiwiYWlkIjoiMCIsIm5ldCI6IndzIiwidGxzIjoidGxzIiwicGF0aCI6Ii93cyIsImhvc3QiOiJjb3Zlci5leGFtcGxlLmludmFsaWQifQ==";
        let twin = parse_link(link).expect("vmess link parses");
        let draft = ServerDraft {
            uuid: Some("11111111-2222-3333-4444-555555555555".to_string()),
            stream: Some(StreamSettings {
                network: "ws".to_string(),
                security: "tls".to_string(),
                path: Some("/ws".to_string()),
                host: Some("cover.example.invalid".to_string()),
                sni: Some("cover.example.invalid".to_string()),
                // The vmess parser mirrors `path` into `service_name`
                // unconditionally; the twin must carry the same mirror.
                service_name: Some("/ws".to_string()),
                ..StreamSettings::default()
            }),
            ..base(Protocol::Vmess)
        };
        let resolved = resolve(&draft).expect("vmess draft resolves");
        assert!(resolved.same_connection_as(&twin));
        assert_eq!(
            outbound_tagged(&resolved, "proxy"),
            outbound_tagged(&twin, "proxy")
        );
    }

    #[test]
    fn a_draft_with_an_empty_address_creates_nothing() {
        let mut draft = base(Protocol::Trojan);
        draft.password = Some("invented".to_string());
        draft.address = "   ".to_string();
        assert_eq!(resolve(&draft).unwrap_err(), DraftError::EmptyAddress);
    }

    #[test]
    fn a_draft_with_port_zero_creates_nothing() {
        let mut draft = base(Protocol::Trojan);
        draft.password = Some("invented".to_string());
        draft.port = 0;
        assert_eq!(resolve(&draft).unwrap_err(), DraftError::ZeroPort);
    }

    /// One rejection per missing credential, and the error names the JSON key
    /// the dialog labels — that sentence is what appears next to the field.
    #[test]
    fn a_missing_credential_is_named_with_its_json_key() {
        let cases: Vec<(ServerDraft, &str)> = vec![
            (base(Protocol::Vless), "uuid is required for vless"),
            (base(Protocol::Vmess), "uuid is required for vmess"),
            (base(Protocol::Trojan), "password is required for trojan"),
            (
                base(Protocol::Shadowsocks),
                "method is required for shadowsocks",
            ),
            (
                {
                    let mut draft = base(Protocol::Shadowsocks);
                    draft.method = Some("aes-256-gcm".to_string());
                    draft
                },
                "password is required for shadowsocks",
            ),
            (base(Protocol::Hysteria2), "auth is required for hysteria2"),
        ];
        for (draft, message) in cases {
            let error = resolve(&draft).expect_err(message);
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn a_patch_that_is_not_an_object_creates_nothing() {
        let mut draft = base(Protocol::Trojan);
        draft.password = Some("invented".to_string());
        draft.outbound_patch = Some(serde_json::json!(["not", "an", "object"]));
        assert_eq!(resolve(&draft).unwrap_err(), DraftError::PatchNotObject);
    }

    #[test]
    fn a_patch_that_sets_the_tag_or_the_protocol_creates_nothing() {
        for key in ["tag", "protocol"] {
            let mut draft = base(Protocol::Trojan);
            draft.password = Some("invented".to_string());
            draft.outbound_patch = Some(serde_json::json!({ key: "stolen" }));
            assert!(matches!(
                resolve(&draft).unwrap_err(),
                DraftError::PatchSetsReserved(reserved) if reserved == key
            ));
        }
    }

    /// The escape hatch works end to end: the fragment lands in the generated
    /// outbound verbatim, merged rather than bolted on.
    #[test]
    fn a_patch_reaches_the_generated_outbound_verbatim() {
        let mut draft = base(Protocol::Trojan);
        draft.password = Some("invented".to_string());
        draft.outbound_patch = Some(serde_json::json!({
            "mux": { "enabled": true, "concurrency": 4 },
            "streamSettings": { "sockopt": { "tcpFastOpen": true } }
        }));
        let server = resolve(&draft).expect("patched draft resolves");
        let outbound = outbound_tagged(&server, "proxy").expect("generates");
        assert_eq!(
            outbound["mux"],
            serde_json::json!({ "enabled": true, "concurrency": 4 })
        );
        assert_eq!(outbound["streamSettings"]["sockopt"]["tcpFastOpen"], true);
        // Merged: what the generator wrote next to the patch is still there.
        assert_eq!(outbound["protocol"], "trojan");
        assert_eq!(outbound["settings"]["servers"][0]["password"], "invented");
    }

    /// Two drafts that differ only in their patch are two servers, not one
    /// server and a duplicate.
    #[test]
    fn a_patch_takes_part_in_the_servers_identity() {
        let mut plain = base(Protocol::Trojan);
        plain.password = Some("invented".to_string());
        let mut patched = plain.clone();
        patched.outbound_patch = Some(serde_json::json!({ "mux": { "enabled": true } }));
        let plain = resolve(&plain).expect("resolves");
        let patched = resolve(&patched).expect("resolves");
        assert_ne!(plain.id, patched.id);
    }

    /// The listing spells a vless server exactly as the editor does, with
    /// the credential masked-worthy and the absent options absent.
    #[test]
    fn a_resolved_vless_server_lists_the_editors_fields() {
        let draft = ServerDraft {
            uuid: Some("11111111-2222-3333-4444-555555555555".to_string()),
            stream: Some(StreamSettings {
                network: "ws".to_string(),
                security: "tls".to_string(),
                sni: Some("cover.example.invalid".to_string()),
                path: Some("/ws".to_string()),
                ..StreamSettings::default()
            }),
            ..base(Protocol::Vless)
        };
        let server = resolve(&draft).expect("resolves");
        assert_eq!(
            parameters(&server)
                .into_iter()
                .map(|parameter| (parameter.group, parameter.key, parameter.secret))
                .collect::<Vec<_>>(),
            vec![
                ("Server", "protocol", false),
                ("Server", "address", false),
                ("Server", "port", false),
                ("Credentials", "uuid", true),
                ("Transport and TLS", "network", false),
                ("Transport and TLS", "security", false),
                ("Transport and TLS", "sni", false),
                ("Transport and TLS", "path", false),
            ]
        );
        let listed = parameters(&server);
        assert_eq!(listed[0].value, "vless");
        assert_eq!(listed[1].value, "server.example.invalid");
        assert_eq!(listed[2].value, "443");
        assert_eq!(listed[3].value, "11111111-2222-3333-4444-555555555555");
    }

    /// A plain trojan carries nothing beyond the basics and the password:
    /// normalized `tcp` and `none`, no absent options.
    #[test]
    fn a_plain_trojan_lists_nothing_it_does_not_carry() {
        let mut draft = base(Protocol::Trojan);
        draft.password = Some("invented".to_string());
        let server = resolve(&draft).expect("resolves");
        assert_eq!(
            parameters(&server)
                .into_iter()
                .map(|parameter| (parameter.group, parameter.key))
                .collect::<Vec<_>>(),
            vec![
                ("Server", "protocol"),
                ("Server", "address"),
                ("Server", "port"),
                ("Credentials", "password"),
                ("Transport and TLS", "network"),
                ("Transport and TLS", "security"),
            ]
        );
    }

    /// The hysteria2 half, including the obfs password as a secret and the
    /// port-hopping ranges a link may carry.
    #[test]
    fn a_hysteria2_server_lists_its_settings_and_marks_the_obfs_secret() {
        let mut draft = base(Protocol::Hysteria2);
        draft.auth = Some("invented-auth".to_string());
        draft.hysteria2 = Some(Hysteria2Settings {
            sni: Some("cover.example.invalid".to_string()),
            obfs: Some(crate::model::Hysteria2Obfs {
                kind: "salamander".to_string(),
                password: "also-invented".to_string(),
            }),
            up_mbps: Some(100),
            down_mbps: Some(500),
            port_hop: vec![
                crate::model::PortRange {
                    start: 40_000,
                    end: 50_000,
                },
                crate::model::PortRange {
                    start: 60_000,
                    end: 60_000,
                },
            ],
            ..Hysteria2Settings::default()
        });
        let server = resolve(&draft).expect("resolves");
        let listed = parameters(&server);
        let pairs = listed
            .iter()
            .map(|parameter| (parameter.key, parameter.secret))
            .collect::<Vec<_>>();
        assert_eq!(
            pairs,
            vec![
                ("protocol", false),
                ("address", false),
                ("port", false),
                ("auth", true),
                ("sni", false),
                ("obfs.password", true),
                ("up_mbps", false),
                ("down_mbps", false),
                ("port_hop", false),
            ]
        );
        let port_hop = listed.iter().find(|p| p.key == "port_hop").expect("listed");
        assert_eq!(port_hop.value, "40000-50000, 60000");
        let up = listed.iter().find(|p| p.key == "up_mbps").expect("listed");
        assert_eq!(up.value, "100");
    }

    /// The patch rides along, pretty-printed, in the group of its own.
    #[test]
    fn a_servers_patch_is_listed_pretty() {
        let mut draft = base(Protocol::Trojan);
        draft.password = Some("invented".to_string());
        draft.outbound_patch = Some(serde_json::json!({ "mux": { "enabled": true } }));
        let server = resolve(&draft).expect("resolves");
        let patch = parameters(&server)
            .into_iter()
            .find(|parameter| parameter.key == "outbound_patch")
            .expect("the patch is carried");
        assert_eq!(patch.group, "outbound_patch");
        assert_eq!(
            patch.value,
            "{\n  \"mux\": {\n    \"enabled\": true\n  }\n}"
        );
    }

    /// The prefill is faithful: reading a resolved server back as a draft
    /// and resolving it again reproduces the server, for every protocol the
    /// editor offers.
    #[test]
    fn a_server_reads_back_as_the_draft_that_made_it() {
        let cases = [
            ServerDraft {
                uuid: Some("11111111-2222-3333-4444-555555555555".to_string()),
                stream: Some(StreamSettings {
                    network: "xhttp".to_string(),
                    security: "reality".to_string(),
                    public_key: Some("invented-public-key".to_string()),
                    short_id: Some("ab".to_string()),
                    spider_x: Some("/".to_string()),
                    xhttp_mode: Some("packet-up".to_string()),
                    ..StreamSettings::default()
                }),
                ..base(Protocol::Vless)
            },
            ServerDraft {
                uuid: Some("11111111-2222-3333-4444-555555555555".to_string()),
                stream: Some(StreamSettings {
                    network: "grpc".to_string(),
                    security: "tls".to_string(),
                    service_name: Some("gun".to_string()),
                    grpc_authority: Some("cover.example.invalid".to_string()),
                    grpc_multi_mode: true,
                    ..StreamSettings::default()
                }),
                ..base(Protocol::Vmess)
            },
            {
                let mut draft = base(Protocol::Trojan);
                draft.password = Some("invented".to_string());
                draft
            },
            {
                let mut draft = base(Protocol::Shadowsocks);
                draft.method = Some("aes-256-gcm".to_string());
                draft.password = Some("invented".to_string());
                draft
            },
            ServerDraft {
                auth: Some("invented-auth".to_string()),
                hysteria2: Some(Hysteria2Settings {
                    sni: Some("cover.example.invalid".to_string()),
                    up_mbps: Some(100),
                    ..Hysteria2Settings::default()
                }),
                ..base(Protocol::Hysteria2)
            },
        ];
        for draft in cases {
            let server = resolve(&draft).expect("resolves");
            let read_back = resolve(&draft_from_server(&server)).expect("reads back");
            assert_eq!(read_back.spec, server.spec, "{:?}", draft.protocol);
            assert_eq!(read_back.name, server.name);
            assert_eq!(read_back.address, server.address);
            assert_eq!(read_back.port, server.port);
            assert_eq!(read_back.transport_label, server.transport_label);
            assert_eq!(read_back.id, server.id);
        }
    }

    /// An edit names the fields it touched, leaves only, the nested blocks
    /// dotted; a cleared field is a `null`, and an untouched field does not
    /// appear even when the block around it changed.
    #[test]
    fn a_diff_names_the_touched_leaves_and_nothing_else() {
        let before = ServerDraft {
            uuid: Some("11111111-2222-3333-4444-555555555555".to_string()),
            stream: Some(StreamSettings {
                network: "ws".to_string(),
                security: "tls".to_string(),
                sni: Some("old-cover.example.invalid".to_string()),
                path: Some("/ws".to_string()),
                ..StreamSettings::default()
            }),
            ..base(Protocol::Vless)
        };
        let after = ServerDraft {
            address: "edited.example.invalid".to_string(),
            port: 8443,
            stream: Some(StreamSettings {
                network: "ws".to_string(),
                security: "tls".to_string(),
                sni: Some("new-cover.example.invalid".to_string()),
                ..StreamSettings::default()
            }),
            ..before.clone()
        };
        assert_eq!(
            diff(&before, &after),
            vec![
                (
                    "address".to_string(),
                    serde_json::json!("edited.example.invalid")
                ),
                ("port".to_string(), serde_json::json!(8443)),
                ("stream.path".to_string(), serde_json::Value::Null),
                (
                    "stream.sni".to_string(),
                    serde_json::json!("new-cover.example.invalid")
                ),
            ]
            .into_iter()
            .collect()
        );
    }

    /// Overrides are applied through the validator, and what the edit never
    /// mentioned rides along: the XHTTP `extra` a provider sent survives a
    /// port override untouched.
    #[test]
    fn an_override_rebuilds_the_server_and_keeps_the_untouched_fields() {
        let mut draft = base(Protocol::Vless);
        draft.uuid = Some("11111111-2222-3333-4444-555555555555".to_string());
        draft.stream = Some(StreamSettings {
            network: "xhttp".to_string(),
            security: "reality".to_string(),
            public_key: Some("invented-public-key".to_string()),
            xhttp_extra: Some(serde_json::json!({ "xmux": { "maxConcurrency": 2 } })),
            ..StreamSettings::default()
        });
        let server = resolve(&draft).expect("resolves");
        let alias = Some("kept".to_string());
        let mut with_alias = server.clone();
        with_alias.alias = alias.clone();

        let edited = apply_overrides(
            &with_alias,
            &vec![
                ("port".to_string(), serde_json::json!(8443)),
                (
                    "stream.sni".to_string(),
                    serde_json::json!("cover.example.invalid"),
                ),
            ]
            .into_iter()
            .collect(),
        )
        .expect("applies");

        assert_eq!(edited.port, 8443);
        assert_eq!(
            edited.spec.stream().and_then(|stream| stream.sni.clone()),
            Some("cover.example.invalid".to_string())
        );
        assert_eq!(
            edited
                .spec
                .stream()
                .and_then(|stream| stream.xhttp_extra.clone()),
            Some(serde_json::json!({ "xmux": { "maxConcurrency": 2 } })),
            "the untouched extra rides along"
        );
        assert_eq!(edited.id, server.id, "the server stays itself");
        assert_eq!(edited.alias, alias);
        assert_eq!(edited.transport_label, server.transport_label);
    }

    /// What a field holds, spelled the way override keys spell it — the
    /// provider base a dropped override falls back to.
    #[test]
    fn a_field_value_reads_the_nested_blocks_dotted() {
        let mut draft = base(Protocol::Hysteria2);
        draft.auth = Some("invented-auth".to_string());
        draft.hysteria2 = Some(Hysteria2Settings {
            up_mbps: Some(100),
            ..Hysteria2Settings::default()
        });
        let server = resolve(&draft).expect("resolves");
        assert_eq!(field_value(&server, "port"), Some(serde_json::json!(443)));
        assert_eq!(
            field_value(&server, "hysteria2.up_mbps"),
            Some(serde_json::json!(100))
        );
        assert_eq!(field_value(&server, "stream.network"), None);
    }

    /// The listing marks what the user overrode, with the override keys'
    /// own spelling — including into the nested blocks.
    #[test]
    fn the_listing_marks_the_overridden_fields() {
        let mut draft = base(Protocol::Vless);
        draft.uuid = Some("11111111-2222-3333-4444-555555555555".to_string());
        draft.stream = Some(StreamSettings {
            network: "ws".to_string(),
            security: "tls".to_string(),
            sni: Some("provider.example.invalid".to_string()),
            ..StreamSettings::default()
        });
        let mut server = resolve(&draft).expect("resolves");
        server.overrides = Some(crate::model::ServerOverrides {
            values: vec![
                ("port".to_string(), serde_json::json!(8443)),
                (
                    "stream.sni".to_string(),
                    serde_json::json!("edited.example.invalid"),
                ),
            ]
            .into_iter()
            .collect(),
            provider: BTreeMap::new(),
        });

        let listed = parameters(&server);
        let port = listed.iter().find(|p| p.key == "port").expect("listed");
        assert!(port.overridden);
        let sni = listed
            .iter()
            .find(|p| p.key == "sni" && p.group == "Transport and TLS")
            .expect("listed");
        assert!(sni.overridden);
        let address = listed.iter().find(|p| p.key == "address").expect("listed");
        assert!(!address.overridden);
        let uuid = listed.iter().find(|p| p.key == "uuid").expect("listed");
        assert!(!uuid.overridden);
    }
}
