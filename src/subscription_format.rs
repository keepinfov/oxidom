use anyhow::{Result, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use url::Url;

use crate::link::{b64_decode, parse_links};
use crate::model::{
    OutboundSpec, Protocol, Server, StreamSettings, country_from_name, normalize_pin_sha256,
    transport_label,
};

/// Parse the response formats commonly selected by subscription panels from the
/// User-Agent: share-link lists, full Xray configs, sing-box JSON, and Clash YAML.
pub fn parse(body: &str) -> Result<Vec<Server>> {
    let text = decode_body(body);
    let links = parse_links(&text);
    if !links.is_empty() {
        return Ok(links);
    }

    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        let servers = parse_json(&value);
        if !servers.is_empty() {
            return Ok(servers);
        }
    }

    if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&text)
        && let Ok(value) = serde_json::to_value(value)
    {
        let servers = parse_clash(&value);
        if !servers.is_empty() {
            return Ok(servers);
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
            .filter(|outbound| {
                outbound
                    .get("protocol")
                    .and_then(Value::as_str)
                    .and_then(protocol_from_name)
                    .is_some()
            })
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
            if let Some(tag) = balancer_tag
                && let Some(server) =
                    xray_profile_server(config, &name, proxy_outbounds, balancers, tag)
            {
                servers.push(server);
            }
            continue;
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
    let identity = serde_json::to_string(&json!({
        "name": name,
        "proxy_outbounds": proxy_outbounds,
        "balancers": balancers,
        "burst_observatory": burst_observatory,
        "balancer_tag": balancer_tag,
    }))
    .ok()?;
    let count = proxy_outbounds.len();
    Some(Server {
        id: Server::stable_id(&identity),
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
        latency_ms: None,
    })
}

fn server_from_xray_outbound(name: &str, outbound: &Value) -> Option<Server> {
    let protocol_name = outbound.get("protocol")?.as_str()?;
    let protocol = protocol_from_name(protocol_name)?;
    let mut stream = stream_from_xray(outbound.get("streamSettings"));
    match protocol {
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
    let port = u16_value(outbound.get("server_port")?)?;
    let stream = stream_from_sing_box(outbound);
    let spec = match protocol {
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
    let port = u16_value(proxy.get("port")?)?;
    let stream = stream_from_clash(proxy);
    let spec = match protocol {
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

fn stream_from_clash(proxy: &Value) -> StreamSettings {
    let reality = proxy.get("reality-opts").unwrap_or(&Value::Null);
    let network = string(proxy, "network").unwrap_or_else(|| "tcp".to_string());
    let tls = bool_value(proxy.get("tls")).unwrap_or(false);
    let network_options = match network.as_str() {
        "ws" => proxy.get("ws-opts"),
        "grpc" => proxy.get("grpc-opts"),
        "h2" | "http" => proxy.get("h2-opts"),
        _ => None,
    }
    .unwrap_or(&Value::Null);
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
        path: string(network_options, "path"),
        host: network_options
            .pointer("/headers/Host")
            .and_then(Value::as_str)
            .map(str::to_string),
        service_name: string(network_options, "grpc-service-name"),
        header_type: None,
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
        latency_ms: None,
    };
    server.link = canonical_share_link(&server);
    let identity = server
        .link
        .clone()
        .or_else(|| serde_json::to_string(&server.spec).ok())
        .unwrap_or_else(|| format!("{}:{}:{}", server.address, server.port, server.name));
    server.id = Server::stable_id(&identity);
    server
}

fn canonical_share_link(server: &Server) -> Option<String> {
    match &server.spec {
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
            let credentials = base64::engine::general_purpose::STANDARD_NO_PAD
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
    url.set_host(Some(address)).ok()?;
    url.set_port(Some(port)).ok()?;
    Some(url)
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
        _ => None,
    }
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
    use super::parse;
    use crate::model::OutboundSpec;

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
        let servers = parse(body).unwrap();
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
        let servers = parse(body).unwrap();
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
        let servers = parse(body).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].address, "example.com");
    }

    #[test]
    fn rejects_zero_server_documents_even_when_they_contain_urls() {
        let error = parse(r#"{"dns":["https://example.com"]}"#).unwrap_err();
        assert!(error.to_string().contains("no supported servers"));
    }
}
