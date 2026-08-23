use anyhow::{Result, bail};
use base64::Engine as _;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::link::{Skipped, b64_decode, parse_links_reporting};
use crate::model::{
    Hysteria2Obfs, Hysteria2Settings, OutboundSpec, PortRange, Protocol, Server, StreamSettings,
    country_from_name, normalize_pin_sha256, parse_bandwidth_mbps, transport_label,
};

/// Parse the response formats commonly selected by subscription panels from the
/// User-Agent: share-link lists, full Xray configs, sing-box JSON, and Clash YAML.
pub fn parse(body: &str) -> Result<(Vec<Server>, Skipped)> {
    let text = decode_body(body);
    let (links, skipped) = parse_links_reporting(&text);
    if !links.is_empty() {
        return Ok((links, skipped));
    }

    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        let servers = parse_json(&value);
        if !servers.is_empty() {
            // A config document is all-or-nothing: an outbound this app cannot
            // read is not a line it skipped, it is a document it misread.
            return Ok((servers, Skipped::default()));
        }
    }

    if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&text)
        && let Ok(value) = serde_json::to_value(value)
    {
        let servers = parse_clash(&value);
        if !servers.is_empty() {
            return Ok((servers, Skipped::default()));
        }
    }

    // Never quote the response body in the error: it flows into toasts and
    // logs, and may carry panel tokens or private HTML. Classify it instead.
    let trimmed = text.trim_start();
    if trimmed.starts_with('<') || trimmed.to_ascii_lowercase().contains("<html") {
        bail!(
            "subscription returned no supported servers: the panel sent a web page instead of \
             a server list — it may not recognize this app; try another Client preset in Settings"
        );
    }
    bail!(
        "subscription returned no supported servers (expected a share-link list, Xray or \
         sing-box JSON, or Clash YAML)"
    )
}

/// What a config body carried besides its servers, and which oxidom did not
/// apply.
///
/// The parser takes outbounds and nothing else. A provider that ships routing
/// alongside its nodes — advertising blocked, one country direct, the rest
/// through the proxy — has all of that dropped, and until now the import said
/// only how many servers it found. Silence there reads as "there was nothing
/// else in it", which is how the same subscription behaves differently in
/// oxidom and in another client with nothing to connect the two.
///
/// This counts what was recognised and left. It does **not** apply any of it:
/// deciding whether a provider may choose the routing, or where rule and geo
/// data is fetched from, is a separate question with a security answer
/// attached.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotTaken {
    /// Routing rules the body carried: `route.rules` in sing-box, `rules` in
    /// Clash, `routing.rules` in a full Xray config.
    pub rules: usize,
    /// Named rule sets: `route.rule_set` in sing-box, `rule-providers` in
    /// Clash. Counted apart from `rules` because they are a different kind of
    /// thing — a set is usually a *pointer* to rules held somewhere else.
    pub rule_sets: usize,
    /// The body named where to fetch rule or geo data from: a `geox-url`
    /// block, a `rule_set` entry of `type: "remote"`, a `rule-providers` entry
    /// of `type: http`.
    ///
    /// Kept as its own fact because it is the one with a security answer:
    /// whoever chooses the geo lists chooses which traffic leaves the tunnel,
    /// and that is a larger power than choosing which servers exist.
    pub own_source: bool,
}

impl NotTaken {
    pub fn is_empty(&self) -> bool {
        self.rules == 0 && self.rule_sets == 0 && !self.own_source
    }

    /// What was left, in the one wording the log line and the interface both
    /// use. Empty when there is nothing to say — never "0 rules", which reads
    /// as an import that went wrong rather than as a plain subscription.
    ///
    /// It says **not applied** rather than naming a count and leaving the
    /// reader to guess. A number on its own would read as something that
    /// worked.
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if self.rules > 0 {
            let plural = if self.rules == 1 { "" } else { "s" };
            parts.push(format!("{} routing rule{plural}", self.rules));
        }
        if self.rule_sets > 0 {
            let plural = if self.rule_sets == 1 { "" } else { "s" };
            parts.push(format!("{} rule set{plural}", self.rule_sets));
        }
        let carried = match parts.as_slice() {
            [] => "its own source for rule or geo data".to_string(),
            [one] => one.clone(),
            [one, two] => format!("{one} and {two}"),
            _ => parts.join(", "),
        };
        let source = if self.own_source && !parts.is_empty() {
            ", and its own source for that data"
        } else {
            ""
        };
        Some(format!(
            "carried {carried}{source}, none of which oxidom applied"
        ))
    }
}

/// Read what a subscription body carried besides its servers.
///
/// Separate from [`parse`] rather than another value out of it, because the
/// two answer different questions and only one of them can fail: a body this
/// cannot read at all carries nothing worth reporting, so every path here ends
/// in a count rather than an error. It also means the rules are a pure
/// function over the bytes, which is the only way to hold them to a corpus.
pub fn not_taken(body: &str) -> NotTaken {
    let text = decode_body(body);
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        return not_taken_from(&value);
    }
    if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&text)
        && let Ok(value) = serde_json::to_value(value)
    {
        return not_taken_from(&value);
    }
    // A share-link list carries no routing at all, and neither does anything
    // this cannot read. Nothing to say is said as nothing.
    NotTaken::default()
}

fn not_taken_from(value: &Value) -> NotTaken {
    // An Xray subscription may be a bare array of configs.
    if let Some(configs) = value.as_array() {
        return configs
            .iter()
            .map(not_taken_from)
            .fold(NotTaken::default(), |mut total, one| {
                total.rules += one.rules;
                total.rule_sets += one.rule_sets;
                total.own_source |= one.own_source;
                total
            });
    }
    let count = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    };
    let entries = |pointer: &str| -> &[Value] {
        value
            .pointer(pointer)
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
    };

    // `rule-providers` is a mapping in Clash, not a list.
    let providers = value.get("rule-providers").and_then(Value::as_object);
    let rule_sets = count("/route/rule_set") + providers.map_or(0, |map| map.len());

    let remote_set = entries("/route/rule_set")
        .iter()
        .any(|entry| entry.get("type").and_then(Value::as_str) == Some("remote"));
    let remote_provider = providers.is_some_and(|map| {
        map.values()
            .any(|entry| entry.get("type").and_then(Value::as_str) == Some("http"))
    });
    let own_source = value.get("geox-url").is_some() || remote_set || remote_provider;

    NotTaken {
        // Clash `rules` is a list of strings; sing-box and Xray hold objects.
        // All three are counted the same way, because the count is what is
        // being reported and not the shape.
        rules: count("/route/rules") + count("/rules") + count("/routing/rules"),
        rule_sets,
        own_source,
    }
}

fn decode_body(body: &str) -> String {
    let looks_b64 = body.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '+' | '/' | '=' | '-' | '_')
            || c.is_ascii_whitespace()
    });
    if looks_b64
        && !body.contains("://")
        && let Some(bytes) = b64_decode(body)
        && let Ok(text) = String::from_utf8(bytes)
    {
        let trimmed = text.trim_start();
        if text.contains("://")
            || trimmed.starts_with('{')
            || trimmed.starts_with('[')
            || text.contains("proxies:")
        {
            return text;
        }
    }
    body.to_string()
}

fn parse_json(value: &Value) -> Vec<Server> {
    if let Some(configs) = value.as_array() {
        return parse_xray_configs(configs);
    }
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    if object.get("proxies").and_then(Value::as_array).is_some() {
        return parse_clash(value);
    }
    let Some(outbounds) = object.get("outbounds").and_then(Value::as_array) else {
        return Vec::new();
    };
    if outbounds
        .iter()
        .any(|outbound| outbound.get("protocol").is_some())
    {
        parse_xray_configs(std::slice::from_ref(value))
    } else {
        parse_sing_box(value)
    }
}

fn parse_xray_configs(configs: &[Value]) -> Vec<Server> {
    let mut servers = Vec::new();
    for config in configs {
        let Some(outbounds) = config.get("outbounds").and_then(Value::as_array) else {
            continue;
        };
        let proxy_outbounds: Vec<Value> = outbounds
            .iter()
            .filter(|outbound| xray_outbound_protocol(outbound).is_some())
            .cloned()
            .collect();
        if proxy_outbounds.is_empty() {
            continue;
        }

        let name = string(config, "remarks")
            .or_else(|| string(config, "name"))
            .unwrap_or_else(|| "Imported Xray profile".to_string());
        let balancers = config
            .pointer("/routing/balancers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let route_balancer_tag = config
            .pointer("/routing/rules")
            .and_then(Value::as_array)
            .and_then(|rules| {
                rules
                    .iter()
                    .rev()
                    .find_map(|rule| string(rule, "balancerTag"))
            });
        let balancer_tag = route_balancer_tag.or_else(|| {
            balancers
                .first()
                .and_then(|balancer| string(balancer, "tag"))
        });

        if proxy_outbounds.len() > 1 && !balancers.is_empty() {
            let profile = balancer_tag.and_then(|tag| {
                xray_profile_server(config, &name, proxy_outbounds.clone(), balancers, tag)
            });
            // Only let the profile swallow the individual outbounds once it has
            // actually materialized. A balancer with no resolvable tag, or one
            // whose first outbound fails to parse, used to drop every server in
            // the config and import the subscription as empty.
            if let Some(server) = profile {
                servers.push(server);
                continue;
            }
        }

        for (index, outbound) in proxy_outbounds.iter().enumerate() {
            let item_name = if proxy_outbounds.len() == 1 {
                name.clone()
            } else {
                let tag = string(outbound, "tag").unwrap_or_else(|| (index + 1).to_string());
                format!("{name} · {tag}")
            };
            if let Some(server) = server_from_xray_outbound(&item_name, outbound) {
                servers.push(server);
            }
        }
    }
    servers
}

fn xray_profile_server(
    config: &Value,
    name: &str,
    proxy_outbounds: Vec<Value>,
    balancers: Vec<Value>,
    balancer_tag: String,
) -> Option<Server> {
    let first = server_from_xray_outbound(name, proxy_outbounds.first()?)?;
    let burst_observatory = config.get("burstObservatory").cloned();
    let count = proxy_outbounds.len();
    let mut server = Server {
        id: String::new(),
        name: name.to_string(),
        protocol: first.protocol,
        address: first.address,
        port: first.port,
        transport_label: format!("xray + balanced ({count})"),
        country: country_from_name(name),
        spec: OutboundSpec::XrayProfile {
            proxy_outbounds,
            balancers,
            burst_observatory,
            balancer_tag,
        },
        link: None,
        alias: None,
        latency_ms: None,
    };
    server.id = Server::stable_id(&server.identity_string());
    Some(server)
}

fn server_from_xray_outbound(name: &str, outbound: &Value) -> Option<Server> {
    let protocol = xray_outbound_protocol(outbound)?;
    if protocol == Protocol::Hysteria2 {
        return hysteria2_from_xray(name, outbound);
    }
    let mut stream = stream_from_xray(outbound.get("streamSettings"));
    match protocol {
        Protocol::Hysteria2 => unreachable!("handled above"),
        Protocol::Vless | Protocol::Vmess => {
            let endpoint = outbound.pointer("/settings/vnext/0")?;
            let user = endpoint.pointer("/users/0")?;
            stream.flow = string(user, "flow");
            let address = string(endpoint, "address")?;
            let port = u16_value(endpoint.get("port")?)?;
            let spec = if protocol == Protocol::Vless {
                OutboundSpec::Vless {
                    uuid: string(user, "id")?,
                    encryption: string(user, "encryption").unwrap_or_else(|| "none".to_string()),
                    stream: stream.clone(),
                }
            } else {
                OutboundSpec::Vmess {
                    uuid: string(user, "id")?,
                    alter_id: u32_value(user.get("alterId")).unwrap_or(0),
                    security: string(user, "security").unwrap_or_else(|| "auto".to_string()),
                    stream: stream.clone(),
                }
            };
            Some(finish_server(name, protocol, address, port, spec))
        }
        Protocol::Trojan | Protocol::Shadowsocks | Protocol::Socks | Protocol::Http => {
            let endpoint = outbound.pointer("/settings/servers/0")?;
            if protocol == Protocol::Trojan {
                stream.flow = string(endpoint, "flow");
            }
            let address = string(endpoint, "address")?;
            let port = u16_value(endpoint.get("port")?)?;
            let spec = match protocol {
                Protocol::Trojan => OutboundSpec::Trojan {
                    password: string(endpoint, "password")?,
                    stream: stream.clone(),
                },
                Protocol::Shadowsocks => OutboundSpec::Shadowsocks {
                    method: string(endpoint, "method")?,
                    password: string(endpoint, "password")?,
                },
                Protocol::Socks | Protocol::Http => {
                    let auth = endpoint.pointer("/users/0");
                    let username = auth.and_then(|value| {
                        string(value, "user").or_else(|| string(value, "username"))
                    });
                    let password = auth.and_then(|value| {
                        string(value, "pass").or_else(|| string(value, "password"))
                    });
                    if protocol == Protocol::Socks {
                        OutboundSpec::Socks { username, password }
                    } else {
                        OutboundSpec::Http { username, password }
                    }
                }
                _ => return None,
            };
            Some(finish_server(name, protocol, address, port, spec))
        }
    }
}

fn stream_from_xray(value: Option<&Value>) -> StreamSettings {
    let Some(value) = value else {
        return StreamSettings {
            network: "tcp".to_string(),
            security: "none".to_string(),
            ..Default::default()
        };
    };
    let network = string(value, "network").unwrap_or_else(|| "tcp".to_string());
    let security = string(value, "security").unwrap_or_else(|| "none".to_string());
    let tls = value
        .get(if security == "reality" {
            "realitySettings"
        } else {
            "tlsSettings"
        })
        .unwrap_or(&Value::Null);
    let network_settings = match network.as_str() {
        "ws" => value.get("wsSettings"),
        "grpc" => value.get("grpcSettings"),
        "xhttp" | "splithttp" => value.get("xhttpSettings"),
        "h2" | "http" => value.get("httpSettings"),
        "tcp" => value.get("tcpSettings"),
        _ => None,
    }
    .unwrap_or(&Value::Null);
    StreamSettings {
        network,
        security,
        sni: string(tls, "serverName"),
        alpn: string_vec(tls.get("alpn")),
        fingerprint: string(tls, "fingerprint"),
        allow_insecure: bool_value(tls.get("allowInsecure")).unwrap_or(false),
        // Xray 26.x takes a bare string here; older configs used an array.
        pin_sha256: string(tls, "pinnedPeerCertSha256")
            .or_else(|| {
                tls.pointer("/pinnedPeerCertSha256/0")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .as_deref()
            .and_then(normalize_pin_sha256),
        public_key: string(tls, "publicKey"),
        short_id: string(tls, "shortId"),
        spider_x: string(tls, "spiderX"),
        path: string(network_settings, "path"),
        host: string(network_settings, "host").or_else(|| {
            network_settings
                .pointer("/headers/Host")
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        service_name: string(network_settings, "serviceName"),
        header_type: network_settings
            .pointer("/header/type")
            .and_then(Value::as_str)
            .map(str::to_string),
        flow: None,
    }
}

fn parse_sing_box(config: &Value) -> Vec<Server> {
    config
        .get("outbounds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(server_from_sing_box)
        .collect()
}

fn server_from_sing_box(outbound: &Value) -> Option<Server> {
    let protocol = protocol_from_name(outbound.get("type")?.as_str()?)?;
    let name = string(outbound, "tag").unwrap_or_else(|| protocol.as_str().to_string());
    let address = string(outbound, "server")?;
    if protocol == Protocol::Hysteria2 {
        return hysteria2_from_sing_box(&name, address, outbound);
    }
    let port = u16_value(outbound.get("server_port")?)?;
    let stream = stream_from_sing_box(outbound);
    let spec = match protocol {
        Protocol::Hysteria2 => unreachable!("handled above"),
        Protocol::Vless => OutboundSpec::Vless {
            uuid: string(outbound, "uuid")?,
            encryption: "none".to_string(),
            stream: stream.clone(),
        },
        Protocol::Vmess => OutboundSpec::Vmess {
            uuid: string(outbound, "uuid")?,
            alter_id: u32_value(outbound.get("alter_id")).unwrap_or(0),
            security: string(outbound, "security").unwrap_or_else(|| "auto".to_string()),
            stream: stream.clone(),
        },
        Protocol::Trojan => OutboundSpec::Trojan {
            password: string(outbound, "password")?,
            stream: stream.clone(),
        },
        Protocol::Shadowsocks => OutboundSpec::Shadowsocks {
            method: string(outbound, "method")?,
            password: string(outbound, "password")?,
        },
        Protocol::Socks | Protocol::Http => {
            let username = string(outbound, "username");
            let password = string(outbound, "password");
            if protocol == Protocol::Socks {
                OutboundSpec::Socks { username, password }
            } else {
                OutboundSpec::Http { username, password }
            }
        }
    };
    Some(finish_server(&name, protocol, address, port, spec))
}

fn stream_from_sing_box(outbound: &Value) -> StreamSettings {
    let tls = outbound.get("tls").unwrap_or(&Value::Null);
    let reality = tls.get("reality").unwrap_or(&Value::Null);
    let transport = outbound.get("transport").unwrap_or(&Value::Null);
    let tls_enabled = bool_value(tls.get("enabled")).unwrap_or(!tls.is_null());
    let security = if !reality.is_null() {
        "reality"
    } else if tls_enabled {
        "tls"
    } else {
        "none"
    };
    StreamSettings {
        network: string(transport, "type").unwrap_or_else(|| "tcp".to_string()),
        security: security.to_string(),
        sni: string(tls, "server_name"),
        alpn: string_vec(tls.get("alpn")),
        fingerprint: tls
            .pointer("/utls/fingerprint")
            .and_then(Value::as_str)
            .map(str::to_string),
        allow_insecure: bool_value(tls.get("insecure")).unwrap_or(false),
        // sing-box pins whole PEM certificates, not digests — nothing to map.
        pin_sha256: None,
        public_key: string(reality, "public_key"),
        short_id: string(reality, "short_id"),
        spider_x: None,
        path: string(transport, "path"),
        host: transport
            .pointer("/headers/Host")
            .and_then(Value::as_str)
            .map(str::to_string),
        service_name: string(transport, "service_name"),
        header_type: None,
        flow: string(outbound, "flow"),
    }
}

fn parse_clash(config: &Value) -> Vec<Server> {
    config
        .get("proxies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(server_from_clash)
        .collect()
}

fn server_from_clash(proxy: &Value) -> Option<Server> {
    let type_name = proxy.get("type")?.as_str()?.to_ascii_lowercase();
    let protocol = protocol_from_name(&type_name)?;
    let name = string(proxy, "name").unwrap_or_else(|| protocol.as_str().to_string());
    let address = string(proxy, "server")?;
    if protocol == Protocol::Hysteria2 {
        return hysteria2_from_clash(&name, address, proxy);
    }
    let port = u16_value(proxy.get("port")?)?;
    let stream = stream_from_clash(proxy);
    let spec = match protocol {
        Protocol::Hysteria2 => unreachable!("handled above"),
        Protocol::Vless => OutboundSpec::Vless {
            uuid: string(proxy, "uuid")?,
            encryption: "none".to_string(),
            stream: stream.clone(),
        },
        Protocol::Vmess => OutboundSpec::Vmess {
            uuid: string(proxy, "uuid")?,
            alter_id: u32_value(proxy.get("alterId").or_else(|| proxy.get("alter-id")))
                .unwrap_or(0),
            security: string(proxy, "cipher").unwrap_or_else(|| "auto".to_string()),
            stream: stream.clone(),
        },
        Protocol::Trojan => OutboundSpec::Trojan {
            password: string(proxy, "password")?,
            stream: stream.clone(),
        },
        Protocol::Shadowsocks => OutboundSpec::Shadowsocks {
            method: string(proxy, "cipher")?,
            password: string(proxy, "password")?,
        },
        Protocol::Socks | Protocol::Http => {
            let username = string(proxy, "username");
            let password = string(proxy, "password");
            if protocol == Protocol::Socks {
                OutboundSpec::Socks { username, password }
            } else {
                OutboundSpec::Http { username, password }
            }
        }
    };
    Some(finish_server(&name, protocol, address, port, spec))
}

/// A Clash option that is a string in one dialect and a one-or-more list in
/// another; either way the first entry is the one the outbound carries.
fn first_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => items.first().and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

fn stream_from_clash(proxy: &Value) -> StreamSettings {
    let reality = proxy.get("reality-opts").unwrap_or(&Value::Null);
    let requested = string(proxy, "network").unwrap_or_else(|| "tcp".to_string());
    let tls = bool_value(proxy.get("tls")).unwrap_or(false);
    // Clash's `network: http` is not a transport: it is TCP with HTTP header
    // camouflage, and its options live in `http-opts`, where both `path` and
    // the header values are lists. `h2` is the real HTTP/2 transport, with its
    // host list under `h2-opts.host` rather than in a headers map.
    let (network, header_type) = match requested.as_str() {
        "http" => ("tcp".to_string(), Some("http".to_string())),
        _ => (requested.clone(), None),
    };
    let network_options = match requested.as_str() {
        "ws" => proxy.get("ws-opts"),
        "grpc" => proxy.get("grpc-opts"),
        "h2" => proxy.get("h2-opts"),
        "http" => proxy.get("http-opts"),
        _ => None,
    }
    .unwrap_or(&Value::Null);
    let (path, host) = match requested.as_str() {
        "http" => (
            network_options.get("path").and_then(first_string),
            network_options
                .pointer("/headers/Host")
                .and_then(first_string),
        ),
        "h2" => (
            string(network_options, "path"),
            network_options
                .get("host")
                .and_then(first_string)
                .or_else(|| {
                    network_options
                        .pointer("/headers/Host")
                        .and_then(first_string)
                }),
        ),
        _ => (
            string(network_options, "path"),
            network_options
                .pointer("/headers/Host")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
    };
    StreamSettings {
        network,
        security: if !reality.is_null() {
            "reality".to_string()
        } else if tls {
            "tls".to_string()
        } else {
            "none".to_string()
        },
        sni: string(proxy, "servername").or_else(|| string(proxy, "sni")),
        alpn: string_vec(proxy.get("alpn")),
        fingerprint: string(proxy, "client-fingerprint"),
        allow_insecure: bool_value(proxy.get("skip-cert-verify")).unwrap_or(false),
        // In Clash `fingerprint` is the certificate digest; the uTLS profile is
        // the separate `client-fingerprint` read above.
        pin_sha256: string(proxy, "fingerprint")
            .as_deref()
            .and_then(normalize_pin_sha256),
        public_key: string(reality, "public-key"),
        short_id: string(reality, "short-id"),
        spider_x: None,
        path,
        host,
        service_name: string(network_options, "grpc-service-name"),
        header_type,
        flow: string(proxy, "flow"),
    }
}

fn finish_server(
    name: &str,
    protocol: Protocol,
    address: String,
    port: u16,
    spec: OutboundSpec,
) -> Server {
    let mut server = Server {
        id: String::new(),
        name: name.to_string(),
        protocol,
        address,
        port,
        transport_label: transport_label(protocol, &spec),
        country: country_from_name(name),
        spec,
        link: None,
        alias: None,
        latency_ms: None,
    };
    server.link = canonical_share_link(&server);
    server.id = Server::stable_id(&server.identity_string());
    server
}

fn canonical_share_link(server: &Server) -> Option<String> {
    match &server.spec {
        OutboundSpec::Hysteria2 { auth, settings } => {
            Some(hysteria2_share_link(server, auth, settings))
        }
        OutboundSpec::Vless {
            uuid,
            encryption,
            stream,
        } => {
            let mut url = endpoint_url("vless", &server.address, server.port)?;
            url.set_username(uuid).ok()?;
            add_stream_query(&mut url, stream);
            url.query_pairs_mut().append_pair("encryption", encryption);
            url.set_fragment(Some(&server.name));
            Some(url.into())
        }
        OutboundSpec::Vmess {
            uuid,
            alter_id,
            security,
            stream,
        } => {
            let payload = json!({
                "v": "2",
                "ps": server.name,
                "add": server.address,
                "port": server.port.to_string(),
                "id": uuid,
                "aid": alter_id.to_string(),
                "scy": security,
                "net": stream.network,
                "tls": if stream.security == "none" { "" } else { &stream.security },
                "host": stream.host,
                "path": stream.path.clone().or_else(|| stream.service_name.clone()),
                "sni": stream.sni,
                "fp": stream.fingerprint,
                "alpn": stream.alpn.as_ref().map(|items| items.join(","))
            });
            let encoded = base64::engine::general_purpose::STANDARD_NO_PAD
                .encode(serde_json::to_vec(&payload).ok()?);
            Some(format!("vmess://{encoded}"))
        }
        OutboundSpec::Trojan { password, stream } => {
            let mut url = endpoint_url("trojan", &server.address, server.port)?;
            url.set_username(password).ok()?;
            add_stream_query(&mut url, stream);
            url.set_fragment(Some(&server.name));
            Some(url.into())
        }
        OutboundSpec::Shadowsocks { method, password } => {
            // SIP002 mandates the URL-safe alphabet here. The standard one can
            // emit `+` and `/`, which `Url::set_username` then percent-escapes
            // into `%2F` — not valid base64 for any decoder on the other side.
            let credentials = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(format!("{method}:{password}"));
            let mut url = endpoint_url("ss", &server.address, server.port)?;
            url.set_username(&credentials).ok()?;
            url.set_fragment(Some(&server.name));
            Some(url.into())
        }
        OutboundSpec::Socks { username, password } => {
            authenticated_url("socks", server, username.as_deref(), password.as_deref())
        }
        OutboundSpec::Http { username, password } => {
            authenticated_url("http", server, username.as_deref(), password.as_deref())
        }
        OutboundSpec::XrayProfile { .. } => None,
    }
}

fn endpoint_url(scheme: &str, address: &str, port: u16) -> Option<Url> {
    let mut url = Url::parse(&format!("{scheme}://placeholder@localhost:1")).ok()?;
    url.set_host(Some(&bracketed(address))).ok()?;
    url.set_port(Some(port)).ok()?;
    Some(url)
}

/// A bare IPv6 address handed to `Url::set_host` is truncated at its first
/// colon — `2001:db8::1` becomes host `2001` — so every emitter brackets it,
/// and `normalize_host` strips the brackets again on parse.
fn bracketed(address: &str) -> String {
    if address.contains(':') && !address.starts_with('[') {
        format!("[{address}]")
    } else {
        address.to_string()
    }
}

fn authenticated_url(
    scheme: &str,
    server: &Server,
    username: Option<&str>,
    password: Option<&str>,
) -> Option<String> {
    let mut url = endpoint_url(scheme, &server.address, server.port)?;
    url.set_username(username.unwrap_or("")).ok()?;
    url.set_password(password).ok()?;
    url.set_fragment(Some(&server.name));
    Some(url.into())
}

fn add_stream_query(url: &mut Url, stream: &StreamSettings) {
    let mut pairs = url.query_pairs_mut();
    pairs.append_pair(
        "type",
        if stream.network.is_empty() {
            "tcp"
        } else {
            &stream.network
        },
    );
    pairs.append_pair(
        "security",
        if stream.security.is_empty() {
            "none"
        } else {
            &stream.security
        },
    );
    for (key, value) in [
        ("sni", stream.sni.as_deref()),
        ("fp", stream.fingerprint.as_deref()),
        ("pbk", stream.public_key.as_deref()),
        ("sid", stream.short_id.as_deref()),
        ("spx", stream.spider_x.as_deref()),
        ("path", stream.path.as_deref()),
        ("host", stream.host.as_deref()),
        ("serviceName", stream.service_name.as_deref()),
        ("headerType", stream.header_type.as_deref()),
        ("flow", stream.flow.as_deref()),
    ] {
        if let Some(value) = value {
            pairs.append_pair(key, value);
        }
    }
    if let Some(alpn) = &stream.alpn {
        pairs.append_pair("alpn", &alpn.join(","));
    }
    if stream.allow_insecure {
        pairs.append_pair("allowInsecure", "1");
    }
}

fn protocol_from_name(name: &str) -> Option<Protocol> {
    match name.to_ascii_lowercase().as_str() {
        "vless" => Some(Protocol::Vless),
        "vmess" => Some(Protocol::Vmess),
        "trojan" => Some(Protocol::Trojan),
        "shadowsocks" | "ss" => Some(Protocol::Shadowsocks),
        "socks" | "socks5" => Some(Protocol::Socks),
        "http" | "https" => Some(Protocol::Http),
        // Deliberately not bare "hysteria": in Clash and sing-box that names
        // v1, a different wire protocol Xray's v2 outbound cannot speak.
        "hysteria2" | "hy2" => Some(Protocol::Hysteria2),
        _ => None,
    }
}

/// The protocol of an Xray outbound.
///
/// Xray calls hysteria2 `"hysteria"` and distinguishes the versions by a field,
/// so unlike the other formats the whole outbound is needed to tell them apart.
fn xray_outbound_protocol(outbound: &Value) -> Option<Protocol> {
    let name = outbound.get("protocol")?.as_str()?.to_ascii_lowercase();
    if name == "hysteria" {
        return (u32_value(outbound.pointer("/settings/version")) == Some(2))
            .then_some(Protocol::Hysteria2);
    }
    protocol_from_name(&name)
}

/// Parse a duration that may be a plain number of seconds or a Go-style
/// `"30s"` string, as sing-box and Clash respectively write `hop_interval`.
fn seconds_value(value: Option<&Value>) -> Option<u32> {
    let value = value?;
    if let Some(number) = u32_value(Some(value)) {
        return Some(number);
    }
    let text = value.as_str()?.trim().trim_end_matches('s');
    text.parse().ok()
}

/// Bandwidth written either as a number of mbps or as `"100 Mbps"`.
fn bandwidth_value(value: Option<&Value>) -> Option<u32> {
    let value = value?;
    if let Some(number) = u32_value(Some(value)) {
        return Some(number.max(1));
    }
    parse_bandwidth_mbps(value.as_str()?)
}

fn port_ranges(raw: Option<&Value>) -> Vec<PortRange> {
    string_vec(raw)
        .unwrap_or_default()
        .iter()
        .flat_map(|item| item.split(','))
        .filter_map(PortRange::parse)
        .collect()
}

fn hysteria2_from_xray(name: &str, outbound: &Value) -> Option<Server> {
    let address = string(outbound.pointer("/settings")?, "address")?;
    let port = u16_value(outbound.pointer("/settings/port")?)?;
    let hy = outbound.pointer("/streamSettings/hysteriaSettings")?;
    let tls = outbound
        .pointer("/streamSettings/tlsSettings")
        .unwrap_or(&Value::Null);
    let mask = outbound
        .pointer("/streamSettings/finalmask")
        .unwrap_or(&Value::Null);

    let settings = Hysteria2Settings {
        sni: string(tls, "serverName"),
        alpn: string_vec(tls.get("alpn")),
        allow_insecure: bool_value(tls.get("allowInsecure")).unwrap_or(false),
        pin_sha256: string(tls, "pinnedPeerCertSha256")
            .as_deref()
            .and_then(normalize_pin_sha256),
        obfs: string(mask, "type").map(|kind| Hysteria2Obfs {
            kind,
            password: mask
                .pointer("/settings/password")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        up_mbps: bandwidth_value(hy.get("up")),
        down_mbps: bandwidth_value(hy.get("down")),
        port_hop: port_ranges(hy.pointer("/udpHop/ports")),
        hop_interval_secs: seconds_value(hy.pointer("/udpHop/interval")),
        congestion: string(hy, "congestion"),
        udp_idle_timeout_secs: u32_value(hy.get("udpIdleTimeout")),
    };
    let spec = OutboundSpec::Hysteria2 {
        auth: string(hy, "auth")?,
        settings,
    };
    Some(finish_server(
        name,
        Protocol::Hysteria2,
        address,
        port,
        spec,
    ))
}

fn hysteria2_from_sing_box(name: &str, address: String, outbound: &Value) -> Option<Server> {
    let tls = outbound.get("tls").unwrap_or(&Value::Null);
    let obfs = outbound.get("obfs").and_then(|obfs| {
        // Either `"obfs": "salamander"` or `{"type": …, "password": …}`.
        let kind = obfs
            .as_str()
            .map(str::to_string)
            .or_else(|| string(obfs, "type"))?;
        Some(Hysteria2Obfs {
            kind,
            password: string(obfs, "password").unwrap_or_default(),
        })
    });

    let settings = Hysteria2Settings {
        sni: string(tls, "server_name"),
        alpn: string_vec(tls.get("alpn")),
        allow_insecure: bool_value(tls.get("insecure")).unwrap_or(false),
        pin_sha256: None,
        obfs,
        up_mbps: bandwidth_value(outbound.get("up_mbps").or_else(|| outbound.get("up"))),
        down_mbps: bandwidth_value(outbound.get("down_mbps").or_else(|| outbound.get("down"))),
        // sing-box separates a range with a colon: "5000:6000".
        port_hop: port_ranges(outbound.get("server_ports")),
        hop_interval_secs: seconds_value(outbound.get("hop_interval")),
        congestion: None,
        udp_idle_timeout_secs: None,
    };
    // A hysteria2 endpoint may advertise only a hopping range.
    let port = u16_value(outbound.get("server_port").unwrap_or(&Value::Null))
        .or_else(|| settings.port_hop.first().map(|range| range.start))
        .unwrap_or(443);
    let spec = OutboundSpec::Hysteria2 {
        auth: string(outbound, "password")?,
        settings,
    };
    Some(finish_server(
        name,
        Protocol::Hysteria2,
        address,
        port,
        spec,
    ))
}

fn hysteria2_from_clash(name: &str, address: String, proxy: &Value) -> Option<Server> {
    let settings = Hysteria2Settings {
        sni: string(proxy, "sni").or_else(|| string(proxy, "servername")),
        alpn: string_vec(proxy.get("alpn")),
        allow_insecure: bool_value(proxy.get("skip-cert-verify")).unwrap_or(false),
        // On hysteria2 `fingerprint` is the certificate digest, not the uTLS
        // profile that `client-fingerprint` names elsewhere in Clash.
        pin_sha256: string(proxy, "fingerprint")
            .as_deref()
            .and_then(normalize_pin_sha256),
        obfs: string(proxy, "obfs").map(|kind| Hysteria2Obfs {
            kind,
            password: string(proxy, "obfs-password").unwrap_or_default(),
        }),
        up_mbps: bandwidth_value(proxy.get("up")),
        down_mbps: bandwidth_value(proxy.get("down")),
        port_hop: port_ranges(proxy.get("ports")),
        hop_interval_secs: seconds_value(proxy.get("hop-interval")),
        congestion: None,
        udp_idle_timeout_secs: None,
    };
    // `port` is optional when `ports` is present; the generic path would drop
    // the whole server here.
    let port = u16_value(proxy.get("port").unwrap_or(&Value::Null))
        .or_else(|| settings.port_hop.first().map(|range| range.start))
        .unwrap_or(443);
    let spec = OutboundSpec::Hysteria2 {
        auth: string(proxy, "password")?,
        settings,
    };
    Some(finish_server(
        name,
        Protocol::Hysteria2,
        address,
        port,
        spec,
    ))
}

/// Everything but the RFC 3986 "unreserved" set is escaped. Keeping `-._~`
/// literal avoids emitting noise like `sni=real%2Eexample`, which is legal but
/// which other clients display verbatim.
const SHARE_LINK_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Build a `hysteria2://` link for an imported server.
///
/// Hand-rolled rather than built with `Url`, which will not accept the comma in
/// a port-hopping authority. **The key order is fixed on purpose**: this link
/// becomes the server's stable id, so reordering it would give every saved
/// hysteria2 server a new identity on the next subscription refresh.
fn hysteria2_share_link(server: &Server, auth: &str, s: &Hysteria2Settings) -> String {
    let encode = |value: &str| utf8_percent_encode(value, SHARE_LINK_VALUE).to_string();

    let mut authority = format!("{}:{}", bracketed(&server.address), server.port);
    for range in &s.port_hop {
        authority.push(',');
        authority.push_str(&range.to_xray());
    }

    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(obfs) = &s.obfs {
        query.push(("obfs", obfs.kind.clone()));
        if !obfs.password.is_empty() {
            query.push(("obfs-password", obfs.password.clone()));
        }
    }
    if let Some(sni) = &s.sni {
        query.push(("sni", sni.clone()));
    }
    if s.allow_insecure {
        query.push(("insecure", "1".to_string()));
    }
    if let Some(pin) = &s.pin_sha256 {
        query.push(("pinSHA256", pin.clone()));
    }
    if let Some(alpn) = &s.alpn {
        query.push(("alpn", alpn.join(",")));
    }
    if let Some(interval) = s.hop_interval_secs {
        query.push(("hopInterval", interval.to_string()));
    }
    if let Some(up) = s.up_mbps {
        query.push(("up", up.to_string()));
    }
    if let Some(down) = s.down_mbps {
        query.push(("down", down.to_string()));
    }
    if let Some(congestion) = &s.congestion {
        query.push(("congestion", congestion.clone()));
    }

    let query: Vec<String> = query
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect();
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    format!(
        "hysteria2://{}@{authority}/{query}#{}",
        encode(auth),
        encode(&server.name)
    )
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}

fn u16_value(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|number| u16::try_from(number).ok())
        .or_else(|| value.as_str()?.parse().ok())
}

fn u32_value(value: Option<&Value>) -> Option<u32> {
    let value = value?;
    value
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .or_else(|| value.as_str()?.parse().ok())
}

fn bool_value(value: Option<&Value>) -> Option<bool> {
    let value = value?;
    value.as_bool().or_else(|| match value.as_str()? {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    })
}

fn string_vec(value: Option<&Value>) -> Option<Vec<String>> {
    let value = value?;
    if let Some(items) = value.as_array() {
        return Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        );
    }
    value.as_str().map(|text| {
        text.split(',')
            .map(|item| item.trim().to_string())
            .collect()
    })
}

#[cfg(test)]
mod tests {
    /// A sing-box body of the shape a panel actually serves: nodes, a rule
    /// list, and named sets — one of which is a pointer to somewhere else.
    /// The servers come through and nothing else does, which is correct; the
    /// count is what makes it say so.
    #[test]
    fn a_sing_box_body_reports_the_routing_it_arrived_with() {
        let body = r#"{
            "outbounds": [
                {"type": "vless", "tag": "de", "server": "203.0.113.9", "server_port": 443,
                 "uuid": "6f8c3d2e-1a4b-4c7d-9e0f-5a6b7c8d9e0f"}
            ],
            "route": {
                "rules": [
                    {"rule_set": "ads", "outbound": "block"},
                    {"domain_suffix": [".example.invalid"], "outbound": "direct"}
                ],
                "rule_set": [
                    {"tag": "ads", "type": "remote", "format": "binary",
                     "url": "https://example.invalid/ads.srs"},
                    {"tag": "local", "type": "local", "format": "binary",
                     "path": "local.srs"}
                ]
            }
        }"#;
        let carried = not_taken(body);
        assert_eq!(carried.rules, 2);
        assert_eq!(carried.rule_sets, 2);
        assert!(
            carried.own_source,
            "a rule set of type remote names a host to fetch from"
        );
        assert!(!carried.is_empty());

        let (servers, _skipped) = parse(body).expect("the servers still parse");
        assert_eq!(servers.len(), 1, "no rule may arrive as a server");
        assert_eq!(servers[0].address, "203.0.113.9");
    }

    /// The same for Clash, where `rule-providers` is a mapping rather than a
    /// list and the rules are bare strings.
    #[test]
    fn a_clash_body_reports_the_routing_it_arrived_with() {
        let body = "proxies:\n\
             \x20 - name: de\n\
             \x20   type: vless\n\
             \x20   server: 203.0.113.9\n\
             \x20   port: 443\n\
             \x20   uuid: 6f8c3d2e-1a4b-4c7d-9e0f-5a6b7c8d9e0f\n\
             rules:\n\
             \x20 - 'RULE-SET,ads,REJECT'\n\
             \x20 - 'GEOIP,LAN,DIRECT'\n\
             \x20 - 'MATCH,de'\n\
             rule-providers:\n\
             \x20 ads:\n\
             \x20   type: http\n\
             \x20   behavior: domain\n\
             \x20   url: 'https://example.invalid/ads.yaml'\n\
             geox-url:\n\
             \x20 geoip: 'https://example.invalid/geoip.dat'\n";
        let carried = not_taken(body);
        assert_eq!(carried.rules, 3);
        assert_eq!(carried.rule_sets, 1);
        assert!(
            carried.own_source,
            "geox-url names where geo data comes from"
        );

        let (servers, _skipped) = parse(body).expect("the servers still parse");
        assert_eq!(servers.len(), 1, "no rule may arrive as a server");
    }

    /// A body that carried nothing but servers must say nothing. "0 rules"
    /// reads as a defect in the import rather than as a plain subscription.
    #[test]
    fn a_body_that_carried_only_servers_says_nothing() {
        let sing_box = r#"{"outbounds": [
            {"type": "vless", "tag": "de", "server": "203.0.113.9", "server_port": 443,
             "uuid": "6f8c3d2e-1a4b-4c7d-9e0f-5a6b7c8d9e0f"}
        ]}"#;
        assert_eq!(not_taken(sing_box), NotTaken::default());
        assert!(not_taken(sing_box).is_empty());

        // A share-link list carries no routing at all, and neither does a body
        // nothing can read.
        assert!(
            not_taken("vless://6f8c3d2e-1a4b-4c7d-9e0f-5a6b7c8d9e0f@203.0.113.9:443#de").is_empty()
        );
        assert!(not_taken("<html>not supported</html>").is_empty());
        assert!(not_taken("").is_empty());
    }

    /// Local sets point at nothing outside the body, so they are counted but
    /// they are not a source anybody chose on the user's behalf. The two facts
    /// are reported apart because only one of them has a security answer.
    #[test]
    fn a_set_that_names_no_host_is_counted_but_is_not_a_source() {
        let body = r#"{
            "outbounds": [{"type": "vless", "tag": "de", "server": "203.0.113.9",
                           "server_port": 443,
                           "uuid": "6f8c3d2e-1a4b-4c7d-9e0f-5a6b7c8d9e0f"}],
            "route": {"rule_set": [{"tag": "local", "type": "local", "path": "local.srs"}]}
        }"#;
        let carried = not_taken(body);
        assert_eq!(carried.rule_sets, 1);
        assert!(!carried.own_source);
        assert!(!carried.is_empty(), "a set is still something not taken");
    }

    /// A full Xray config drops its rules the same way — `parse` reads
    /// `routing` only for the balancer tag — so the same silence applied
    /// there, and the same count ends it.
    #[test]
    fn an_xray_config_reports_the_routing_block_it_arrived_with() {
        let body = r#"[{
            "remarks": "Imported",
            "outbounds": [{"protocol": "vless", "tag": "proxy", "settings": {"vnext": [
                {"address": "203.0.113.9", "port": 443,
                 "users": [{"id": "6f8c3d2e-1a4b-4c7d-9e0f-5a6b7c8d9e0f"}]}]}}],
            "routing": {"rules": [
                {"type": "field", "domain": ["geosite:category-ads-all"], "outboundTag": "block"},
                {"type": "field", "ip": ["geoip:private"], "outboundTag": "direct"}
            ]}
        }]"#;
        let carried = not_taken(body);
        assert_eq!(carried.rules, 2);
        assert_eq!(carried.rule_sets, 0);
        assert!(!carried.own_source);
    }

    use super::{NotTaken, not_taken, parse};
    use crate::model::{OutboundSpec, PortRange, Protocol};

    /// The reported case: a panel lists more servers than this build can read,
    /// and the ones it can read arrive with no hint that the rest existed.
    #[test]
    fn a_half_readable_list_reports_the_half_it_dropped() {
        let body = "vless://id@one.example:443#One\n\
                    tuic://id@two.example:443#Two\n\
                    tuic://id@three.example:443#Three\n\
                    ssh://four.example:22#Four\n";
        let (servers, skipped) = parse(body).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(skipped.lines, 3);
        assert_eq!(skipped.schemes, ["tuic", "ssh"]);
    }

    #[test]
    fn parses_xray_balanced_profile() {
        let body = r#"[{
          "remarks": "Auto",
          "outbounds": [
            {"tag":"proxy","protocol":"vless","settings":{"vnext":[{"address":"one.example","port":443,"users":[{"id":"id-1","encryption":"none","flow":"xtls-rprx-vision"}]}]},"streamSettings":{"network":"tcp","security":"none"}},
            {"tag":"proxy-2","protocol":"vless","settings":{"vnext":[{"address":"two.example","port":443,"users":[{"id":"id-2","encryption":"none"}]}]},"streamSettings":{"network":"tcp","security":"none"}},
            {"tag":"direct","protocol":"freedom"}
          ],
          "routing":{"rules":[{"type":"field","network":"tcp,udp","balancerTag":"balance"}],"balancers":[{"tag":"balance","selector":["proxy"]}]},
          "burstObservatory":{"subjectSelector":["proxy"]}
        }]"#;
        let (servers, _skipped) = parse(body).unwrap();
        assert_eq!(servers.len(), 1);
        assert!(servers[0].link.is_none());
        assert!(matches!(servers[0].spec, OutboundSpec::XrayProfile { .. }));
    }

    #[test]
    fn parses_sing_box_and_creates_share_links() {
        let body = r#"{"outbounds":[
          {"type":"selector","tag":"select","outbounds":["node"]},
          {"type":"vless","tag":"node","server":"example.com","server_port":443,"uuid":"id","tls":{"enabled":true,"server_name":"example.com"}}
        ]}"#;
        let (servers, _skipped) = parse(body).unwrap();
        assert_eq!(servers.len(), 1);
        assert!(
            servers[0]
                .link
                .as_deref()
                .is_some_and(|link| link.starts_with("vless://"))
        );
    }

    #[test]
    fn parses_clash_yaml() {
        let body = r#"
proxies:
  - name: Example
    type: vless
    server: example.com
    port: 443
    uuid: test-id
    tls: true
    servername: example.com
"#;
        let (servers, _skipped) = parse(body).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].address, "example.com");
    }

    #[test]
    fn balancer_without_a_usable_tag_still_imports_its_outbounds() {
        // The profile branch used to `continue` unconditionally, so a balancer
        // it could not turn into a single server took every outbound in the
        // config down with it and the subscription imported as empty.
        let body = r#"[{
          "remarks": "Auto",
          "outbounds": [
            {"tag":"proxy","protocol":"vless","settings":{"vnext":[{"address":"one.example","port":443,"users":[{"id":"id-1","encryption":"none"}]}]},"streamSettings":{"network":"tcp","security":"none"}},
            {"tag":"proxy-2","protocol":"vless","settings":{"vnext":[{"address":"two.example","port":443,"users":[{"id":"id-2","encryption":"none"}]}]},"streamSettings":{"network":"tcp","security":"none"}}
          ],
          "routing": {"balancers": [{"selector": ["proxy"]}]}
        }]"#;
        let (servers, _skipped) = parse(body).unwrap();
        let addresses: Vec<&str> = servers.iter().map(|s| s.address.as_str()).collect();
        assert_eq!(addresses, ["one.example", "two.example"]);
    }

    #[test]
    fn generated_shadowsocks_links_parse_back() {
        // SIP002 wants the URL-safe alphabet: the standard one emits `/`, which
        // `Url` escapes to `%2F` and no base64 decoder accepts.
        let body = r#"
proxies:
  - name: SS
    type: ss
    server: example.com
    port: 8388
    cipher: aes-256-gcm
    password: "pa/ss+word"
"#;
        let (servers, _skipped) = parse(body).unwrap();
        let link = servers[0].link.as_deref().expect("a canonical share link");
        assert!(
            !link.contains('%'),
            "generated link should need no escaping: {link}"
        );
        let reparsed = crate::link::parse_link(link).expect("generated link must parse back");
        assert_eq!(reparsed.address, "example.com");
        assert_eq!(reparsed.port, 8388);
        let OutboundSpec::Shadowsocks { method, password } = &reparsed.spec else {
            panic!("expected a shadowsocks outbound");
        };
        assert_eq!(method, "aes-256-gcm");
        assert_eq!(password, "pa/ss+word");
    }

    #[test]
    fn rejects_zero_server_documents_even_when_they_contain_urls() {
        let error = parse(r#"{"dns":["https://example.com"]}"#).unwrap_err();
        assert!(error.to_string().contains("no supported servers"));
    }

    fn hy2(server: &crate::model::Server) -> (&str, &crate::model::Hysteria2Settings) {
        match &server.spec {
            OutboundSpec::Hysteria2 { auth, settings } => (auth, settings),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_clash_hysteria2_without_an_explicit_port() {
        let yaml = r#"
proxies:
  - name: "HY2"
    type: hysteria2
    server: h.example
    ports: "5000-6000,7000"
    password: secret
    obfs: salamander
    obfs-password: obfspw
    up: 100
    down: "1 gbps"
    sni: real.example
    skip-cert-verify: true
    alpn: [h3]
    hop-interval: 30
    fingerprint: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
"#;
        let (servers, _skipped) = parse(yaml).unwrap();
        assert_eq!(servers.len(), 1);
        let server = &servers[0];
        let (auth, s) = hy2(server);

        assert_eq!(server.protocol, Protocol::Hysteria2);
        assert_eq!(server.address, "h.example");
        // No `port:` key — it must fall back to the first hopping range.
        assert_eq!(server.port, 5000);
        assert_eq!(auth, "secret");
        assert_eq!(s.obfs.as_ref().unwrap().password, "obfspw");
        assert_eq!(s.up_mbps, Some(100));
        assert_eq!(s.down_mbps, Some(1000));
        assert!(s.allow_insecure);
        assert_eq!(s.hop_interval_secs, Some(30));
        // `fingerprint` on hysteria2 is a certificate pin, not a uTLS profile.
        assert!(s.pin_sha256.is_some());
        assert_eq!(server.transport_label, "hysteria2 + salamander");
    }

    #[test]
    fn parses_sing_box_hysteria2() {
        let json = r#"{"outbounds":[{
          "type":"hysteria2","tag":"SB","server":"h.example","server_port":443,
          "password":"secret","up_mbps":100,"down_mbps":50,
          "obfs":{"type":"salamander","password":"obfspw"},
          "server_ports":["5000:6000"],"hop_interval":"30s",
          "tls":{"enabled":true,"server_name":"real.example","insecure":true,"alpn":["h3"]}
        }]}"#;
        let (servers, _skipped) = parse(json).unwrap();
        assert_eq!(servers.len(), 1);
        let (auth, s) = hy2(&servers[0]);

        assert_eq!(auth, "secret");
        assert_eq!(servers[0].port, 443);
        assert_eq!(s.obfs.as_ref().unwrap().kind, "salamander");
        assert_eq!(s.up_mbps, Some(100));
        assert_eq!(s.down_mbps, Some(50));
        // sing-box separates a range with a colon, not a dash.
        assert_eq!(
            s.port_hop,
            vec![PortRange {
                start: 5000,
                end: 6000
            }]
        );
        assert_eq!(s.hop_interval_secs, Some(30), "\"30s\" is a duration");
        assert_eq!(s.sni.as_deref(), Some("real.example"));
    }

    #[test]
    fn sing_box_accepts_a_bare_string_obfs() {
        let json = r#"{"outbounds":[{"type":"hysteria2","tag":"X","server":"h.example",
          "server_port":443,"password":"pw","obfs":"salamander"}]}"#;
        let (servers, _skipped) = parse(json).unwrap();
        assert_eq!(hy2(&servers[0]).1.obfs.as_ref().unwrap().kind, "salamander");
    }

    #[test]
    fn parses_xray_hysteria2_outbound_and_skips_v1() {
        let json = r#"{"outbounds":[{
          "tag":"hy2","protocol":"hysteria",
          "settings":{"version":2,"address":"h.example","port":443},
          "streamSettings":{"network":"hysteria","security":"tls",
            "tlsSettings":{"serverName":"real.example"},
            "hysteriaSettings":{"version":2,"auth":"secret","up":"100 mbps",
                                "udpHop":{"ports":["443","5000-6000"],"interval":30}},
            "finalmask":{"type":"salamander","settings":{"password":"obfspw"}}}
        }]}"#;
        let (servers, _skipped) = parse(json).unwrap();
        assert_eq!(servers.len(), 1);
        let (auth, s) = hy2(&servers[0]);
        assert_eq!(auth, "secret");
        assert_eq!(s.up_mbps, Some(100));
        assert_eq!(s.obfs.as_ref().unwrap().password, "obfspw");
        assert_eq!(s.sni.as_deref(), Some("real.example"));

        // Version 1 is a different wire protocol and must not be imported.
        let v1 = r#"{"outbounds":[{"tag":"hy1","protocol":"hysteria",
          "settings":{"version":1,"address":"h.example","port":443},
          "streamSettings":{"hysteriaSettings":{"auth":"x"}}}]}"#;
        assert!(parse(v1).is_err(), "hysteria v1 must not be imported");
    }

    /// The emitter and the parser must agree: the generated link becomes the
    /// server's stable id, so a mismatch renames every server on each refresh.
    #[test]
    fn an_ipv6_server_emits_a_bracketed_link_that_parses_back() {
        let yaml = r#"
proxies:
  - name: "SIX"
    type: vless
    server: "2001:db8::1"
    port: 443
    uuid: 00000000-0000-0000-0000-000000000000
    tls: true
  - name: "HY6"
    type: hysteria2
    server: "2001:db8::2"
    port: 443
    password: "pw"
"#;
        let imported = parse(yaml).unwrap().0;

        let vless = imported[0]
            .link
            .as_deref()
            .expect("vless links are emitable");
        assert!(vless.contains("@[2001:db8::1]:443"), "{vless}");
        let reparsed = crate::link::parse_link(vless).expect("emitted link must parse");
        assert_eq!(reparsed.address, "2001:db8::1");
        assert_eq!(reparsed.port, 443);

        let hysteria = imported[1]
            .link
            .as_deref()
            .expect("hysteria2 links are emitable");
        assert!(hysteria.contains("@[2001:db8::2]:443"), "{hysteria}");
        let reparsed = crate::link::parse_link(hysteria).expect("emitted link must parse");
        assert_eq!(reparsed.address, "2001:db8::2");
        assert_eq!(reparsed.port, 443);
    }

    #[test]
    fn clash_network_http_imports_as_tcp_camouflage_and_h2_reads_its_host_list() {
        let yaml = r#"
proxies:
  - name: "CAMO"
    type: vmess
    server: c.example
    port: 80
    uuid: 00000000-0000-0000-0000-000000000000
    alterId: 0
    cipher: auto
    network: http
    http-opts:
      path: ["/live"]
      headers:
        Host: ["cdn.example"]
  - name: "REALH2"
    type: vmess
    server: h.example
    port: 443
    uuid: 00000000-0000-0000-0000-000000000000
    alterId: 0
    cipher: auto
    tls: true
    network: h2
    h2-opts:
      host: ["h2.example"]
      path: /h2
"#;
        let imported = parse(yaml).unwrap().0;

        let camo = stream_of(&imported[0]);
        assert_eq!(
            camo.network, "tcp",
            "Clash http is camouflage, not a transport"
        );
        assert_eq!(camo.header_type.as_deref(), Some("http"));
        assert_eq!(camo.path.as_deref(), Some("/live"));
        assert_eq!(camo.host.as_deref(), Some("cdn.example"));

        let h2 = stream_of(&imported[1]);
        assert_eq!(h2.network, "h2");
        assert_eq!(h2.path.as_deref(), Some("/h2"));
        assert_eq!(
            h2.host.as_deref(),
            Some("h2.example"),
            "h2-opts.host is a list"
        );
    }

    fn stream_of(server: &crate::model::Server) -> &crate::model::StreamSettings {
        match &server.spec {
            crate::model::OutboundSpec::Vmess { stream, .. } => stream,
            other => panic!("expected vmess: {other:?}"),
        }
    }

    #[test]
    fn hysteria2_share_link_round_trips() {
        let yaml = r#"
proxies:
  - name: "HY2"
    type: hysteria2
    server: h.example
    port: 443
    ports: "5000-6000"
    password: "se:cr et"
    obfs: salamander
    obfs-password: obfspw
    up: 100
    down: 50
    sni: real.example
    skip-cert-verify: true
    hop-interval: 30
"#;
        let imported = &parse(yaml).unwrap().0[0];
        let link = imported
            .link
            .as_deref()
            .expect("hysteria2 links are emitable");
        assert!(link.starts_with("hysteria2://"), "{link}");
        assert!(link.contains(":443,5000-6000"), "{link}");

        let reparsed = crate::link::parse_link(link).expect("emitted link must parse");
        assert!(
            reparsed.same_connection_as(imported),
            "\n emitted: {:?}\n reparsed: {:?}",
            imported.spec,
            reparsed.spec
        );
    }

    /// The query order is part of the identity; a reorder orphans saved servers.
    #[test]
    fn hysteria2_share_link_key_order_is_stable() {
        let json = r#"{"outbounds":[{"type":"hysteria2","tag":"N","server":"h.example",
          "server_port":443,"password":"pw","obfs":{"type":"salamander","password":"o"},
          "up_mbps":100,"down_mbps":50,
          "tls":{"enabled":true,"server_name":"real.example","insecure":true}}]}"#;
        let server = &parse(json).unwrap().0[0];
        assert_eq!(
            server.link.as_deref().unwrap(),
            "hysteria2://pw@h.example:443/?obfs=salamander&obfs-password=o\
             &sni=real.example&insecure=1&up=100&down=50#N"
        );
    }

    /// One case per panel this application claims to read.
    ///
    /// The parsers were tested against bodies written here by hand, none of
    /// them named after the software that would send it, so nothing recorded
    /// which panel any shape came from. A user whose panel serves a shape
    /// nobody tried sees an empty list, and the application cannot say whether
    /// that shape is unsupported or whether it is broken.
    ///
    /// **Every fixture below is fixture-only.** No live panel was available, so
    /// each body is written from the format that panel is documented to serve
    /// for that client string, with invented credentials throughout. The table
    /// in `docs/subscriptions-and-protocols.md` says the same, so the claim
    /// stays exactly as wide as what was actually tried.
    ///
    /// A `.b64` fixture is stored the way the panel sends it — base64 around
    /// the link list — because that wrapper is a branch of its own
    /// (`decode_body`). Read one with `base64 -d`.
    mod panels {
        use super::*;

        const MARZBAN_LINKS: &str = include_str!("subscription_format/fixtures/marzban-v2rayn.b64");
        const MARZBAN_CLASH: &str = include_str!("subscription_format/fixtures/marzban-clash.yaml");
        const MARZBAN_SING_BOX: &str =
            include_str!("subscription_format/fixtures/marzban-sing-box.json");
        const MARZNESHIN_LINKS: &str =
            include_str!("subscription_format/fixtures/marzneshin-v2rayn.b64");
        const REMNAWAVE_LINKS: &str =
            include_str!("subscription_format/fixtures/remnawave-v2rayn.b64");
        const REMNAWAVE_XRAY: &str =
            include_str!("subscription_format/fixtures/remnawave-v2rayng.json");
        const THREE_X_UI_LINKS: &str =
            include_str!("subscription_format/fixtures/three-x-ui-v2rayn.b64");
        const HIDDIFY_SING_BOX: &str =
            include_str!("subscription_format/fixtures/hiddify-manager-sing-box.json");
        const V2BOARD_CLASH: &str = include_str!("subscription_format/fixtures/v2board-clash.yaml");
        const WEB_PAGE: &str = include_str!("subscription_format/fixtures/panel-web-page.html");

        /// Marzban wraps its share-link list in base64, so the wrapper is part
        /// of the case: a body that decodes to three links must arrive as three
        /// servers, not as one unreadable line.
        #[test]
        fn a_marzban_link_list_arrives_as_one_server_per_link() {
            let (servers, skipped) = parse(MARZBAN_LINKS).unwrap();
            assert!(skipped.is_empty(), "nothing in this body is unsupported");
            let seen: Vec<_> = servers
                .iter()
                .map(|server| (server.protocol, server.address.as_str(), server.port))
                .collect();
            assert_eq!(
                seen,
                [
                    (Protocol::Vless, "nl1.panel.example", 443),
                    (Protocol::Vmess, "de1.panel.example", 8443),
                    (Protocol::Trojan, "fr1.panel.example", 443),
                ]
            );
            assert!(
                servers.iter().all(|server| server.link.is_some()),
                "a link list keeps the link each server arrived on"
            );
        }

        /// The same panel, the same servers, a Clash client string.
        #[test]
        fn a_marzban_clash_body_carries_the_same_three_servers() {
            let (servers, _skipped) = parse(MARZBAN_CLASH).unwrap();
            let seen: Vec<_> = servers
                .iter()
                .map(|server| (server.protocol, server.address.as_str(), server.port))
                .collect();
            assert_eq!(
                seen,
                [
                    (Protocol::Vless, "nl1.panel.example", 443),
                    (Protocol::Vmess, "de1.panel.example", 8443),
                    (Protocol::Trojan, "fr1.panel.example", 443),
                ]
            );
        }

        /// A sing-box body names its nodes in a `selector` outbound that is not
        /// itself a server; counting it would show a node nobody can connect to.
        #[test]
        fn a_marzban_sing_box_body_skips_the_selector_that_names_its_nodes() {
            let (servers, _skipped) = parse(MARZBAN_SING_BOX).unwrap();
            assert_eq!(servers.len(), 2);
            assert_eq!(servers[0].address, "nl1.panel.example");
            assert_eq!(servers[1].address, "de1.panel.example");
        }

        #[test]
        fn a_marzneshin_link_list_arrives_as_one_server_per_link() {
            let (servers, skipped) = parse(MARZNESHIN_LINKS).unwrap();
            assert!(skipped.is_empty());
            assert_eq!(servers.len(), 2);
            assert_eq!(servers[0].address, "a.marzneshin.example");
            assert_eq!(servers[0].port, 2087);
            assert_eq!(servers[1].address, "b.marzneshin.example");
        }

        /// Both halves of the split this application's default client string
        /// exists for: the same Remnawave subscription answers `v2rayN` with a
        /// link list of individual nodes, and `v2rayNG` with whole Xray
        /// configurations that are one balanced server each.
        #[test]
        fn a_remnawave_link_list_is_one_server_per_node() {
            let (servers, _skipped) = parse(REMNAWAVE_LINKS).unwrap();
            assert_eq!(servers.len(), 2);
            assert!(
                servers.iter().all(|server| server.link.is_some()),
                "each node is individually shareable and poolable"
            );
        }

        #[test]
        fn a_remnawave_xray_config_is_one_balanced_server_with_no_link() {
            let (servers, _skipped) = parse(REMNAWAVE_XRAY).unwrap();
            assert_eq!(
                servers.len(),
                1,
                "a balanced configuration is one server, not one per outbound"
            );
            assert!(matches!(servers[0].spec, OutboundSpec::XrayProfile { .. }));
            assert!(
                servers[0].link.is_none(),
                "no share link can express a whole balanced configuration"
            );
        }

        #[test]
        fn a_three_x_ui_link_list_carries_reality_and_shadowsocks_together() {
            let (servers, skipped) = parse(THREE_X_UI_LINKS).unwrap();
            assert!(skipped.is_empty());
            assert_eq!(servers.len(), 2);
            assert_eq!(servers[0].protocol, Protocol::Vless);
            assert_eq!(servers[1].protocol, Protocol::Shadowsocks);
            assert_eq!(servers[1].address, "ss.xui.example");
        }

        /// Hiddify Manager leads with hysteria2, whose parameters live in a
        /// different place from every other protocol's.
        #[test]
        fn a_hiddify_manager_sing_box_body_keeps_its_hysteria2_parameters() {
            let (servers, _skipped) = parse(HIDDIFY_SING_BOX).unwrap();
            assert_eq!(servers.len(), 2);
            assert_eq!(servers[0].protocol, Protocol::Hysteria2);
            assert_eq!(servers[0].address, "hy2.hiddify.example");
            assert_eq!(servers[0].port, 8443);
            assert_eq!(servers[1].protocol, Protocol::Vless);
        }

        #[test]
        fn a_v2board_clash_body_carries_both_of_its_protocols() {
            let (servers, _skipped) = parse(V2BOARD_CLASH).unwrap();
            assert_eq!(servers.len(), 2);
            assert_eq!(servers[0].protocol, Protocol::Shadowsocks);
            assert_eq!(servers[0].address, "la.v2board.example");
            assert_eq!(servers[1].protocol, Protocol::Trojan);
            assert_eq!(servers[1].address, "tokyo.v2board.example");
        }

        /// The commonest failure a user meets, and the only one whose message
        /// names a cure. It had no test: the body that produced it fell into
        /// the generic bail beside it.
        #[test]
        fn a_panel_answering_with_a_web_page_says_so_and_quotes_nothing() {
            let error = parse(WEB_PAGE).unwrap_err().to_string();
            assert!(
                error.contains("web page instead of a server list"),
                "got: {error}"
            );
            assert!(
                error.contains("Client preset"),
                "the message names what to change: {error}"
            );
            assert!(
                !error.contains("<html") && !error.contains("subscription</title>"),
                "the body is classified, never quoted back: {error}"
            );
        }
    }
}
