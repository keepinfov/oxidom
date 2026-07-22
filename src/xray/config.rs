use serde_json::{json, Value};

use crate::model::{OutboundSpec, Server, StreamSettings};

/// Generate a full Xray config JSON for `server`, with local SOCKS + HTTP inbounds.
pub fn generate(server: &Server, socks_port: u16, http_port: u16) -> Value {
    json!({
        "log": { "loglevel": "warning" },
        "inbounds": [
            {
                "tag": "socks-in",
                "listen": "127.0.0.1",
                "port": socks_port,
                "protocol": "socks",
                "settings": { "auth": "noauth", "udp": true },
                "sniffing": { "enabled": true, "destOverride": ["http", "tls"] }
            },
            {
                "tag": "http-in",
                "listen": "127.0.0.1",
                "port": http_port,
                "protocol": "http",
                "sniffing": { "enabled": true, "destOverride": ["http", "tls"] }
            }
        ],
        "outbounds": [
            outbound(server),
            { "protocol": "freedom", "tag": "direct" },
            { "protocol": "blackhole", "tag": "block" }
        ],
        "routing": {
            "domainStrategy": "IPIfNonMatch",
            "rules": [
                { "type": "field", "ip": ["geoip:private"], "outboundTag": "direct" }
            ]
        }
    })
}

fn outbound(server: &Server) -> Value {
    let addr = &server.address;
    let port = server.port;
    match &server.spec {
        OutboundSpec::Vless { uuid, encryption, stream } => json!({
            "tag": "proxy",
            "protocol": "vless",
            "settings": { "vnext": [ {
                "address": addr,
                "port": port,
                "users": [ trim_obj(json!({
                    "id": uuid,
                    "encryption": encryption,
                    "flow": stream.flow
                })) ]
            } ] },
            "streamSettings": stream_settings(stream)
        }),
        OutboundSpec::Vmess { uuid, alter_id, security, stream } => json!({
            "tag": "proxy",
            "protocol": "vmess",
            "settings": { "vnext": [ {
                "address": addr,
                "port": port,
                "users": [ { "id": uuid, "alterId": alter_id, "security": security } ]
            } ] },
            "streamSettings": stream_settings(stream)
        }),
        OutboundSpec::Trojan { password, stream } => json!({
            "tag": "proxy",
            "protocol": "trojan",
            "settings": { "servers": [ trim_obj(json!({
                "address": addr,
                "port": port,
                "password": password,
                "flow": stream.flow
            })) ] },
            "streamSettings": stream_settings(stream)
        }),
        OutboundSpec::Shadowsocks { method, password } => json!({
            "tag": "proxy",
            "protocol": "shadowsocks",
            "settings": { "servers": [ {
                "address": addr,
                "port": port,
                "method": method,
                "password": password
            } ] }
        }),
        OutboundSpec::Socks { username, password } => json!({
            "tag": "proxy",
            "protocol": "socks",
            "settings": { "servers": [ socks_http_server(addr, port, username, password) ] }
        }),
        OutboundSpec::Http { username, password } => json!({
            "tag": "proxy",
            "protocol": "http",
            "settings": { "servers": [ socks_http_server(addr, port, username, password) ] }
        }),
    }
}

fn socks_http_server(addr: &str, port: u16, user: &Option<String>, pass: &Option<String>) -> Value {
    let mut server = json!({ "address": addr, "port": port });
    if let (Some(u), Some(p)) = (user, pass) {
        server["users"] = json!([ { "user": u, "pass": p } ]);
    }
    server
}

fn stream_settings(s: &StreamSettings) -> Value {
    let mut v = json!({ "network": if s.network.is_empty() { "tcp" } else { &s.network } });

    match s.network.as_str() {
        "ws" => {
            v["wsSettings"] = trim_obj(json!({
                "path": s.path,
                "headers": s.host.as_ref().map(|h| json!({ "Host": h }))
            }));
        }
        "grpc" => {
            v["grpcSettings"] = trim_obj(json!({
                "serviceName": s.service_name.clone().or_else(|| s.path.clone())
            }));
        }
        "xhttp" | "splithttp" => {
            v["xhttpSettings"] = trim_obj(json!({
                "path": s.path,
                "host": s.host,
                "mode": "auto"
            }));
        }
        "h2" | "http" => {
            v["httpSettings"] = trim_obj(json!({
                "path": s.path,
                "host": s.host.as_ref().map(|h| json!([h]))
            }));
        }
        "tcp" if s.header_type.as_deref() == Some("http") => {
            v["tcpSettings"] = json!({ "header": { "type": "http" } });
        }
        _ => {}
    }

    match s.security.as_str() {
        "tls" => {
            v["security"] = json!("tls");
            v["tlsSettings"] = trim_obj(json!({
                "serverName": s.sni,
                "alpn": s.alpn,
                "fingerprint": s.fingerprint,
                "allowInsecure": s.allow_insecure
            }));
        }
        "reality" => {
            v["security"] = json!("reality");
            v["realitySettings"] = trim_obj(json!({
                "serverName": s.sni,
                "fingerprint": s.fingerprint,
                "publicKey": s.public_key,
                "shortId": s.short_id,
                "spiderX": s.spider_x
            }));
        }
        _ => {}
    }

    v
}

/// Drop null values from a JSON object (Xray rejects some explicit nulls).
fn trim_obj(v: Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, val)| !val.is_null())
                .map(|(k, val)| (k, trim_obj(val)))
                .collect(),
        ),
        other => other,
    }
}
