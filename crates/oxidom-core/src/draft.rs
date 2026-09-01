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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::parse_link;
    use crate::xray::config::outbound_tagged;

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
}
