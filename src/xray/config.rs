use serde_json::{Value, json};

use crate::model::{Hysteria2Settings, OutboundSpec, Server, StreamSettings};

/// Generate a full Xray config JSON for `server`, with local SOCKS + HTTP inbounds.
pub fn generate(server: &Server, socks_port: u16, http_port: u16) -> Value {
    let mut config = json!({
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
    });

    match &server.spec {
        OutboundSpec::XrayProfile {
            proxy_outbounds,
            balancers,
            burst_observatory,
            balancer_tag,
        } => {
            let mut outbounds = proxy_outbounds.clone();
            outbounds.push(json!({ "protocol": "freedom", "tag": "direct" }));
            outbounds.push(json!({ "protocol": "blackhole", "tag": "block" }));
            config["outbounds"] = Value::Array(outbounds);
            config["routing"] = json!({
                "domainStrategy": "IPIfNonMatch",
                "rules": [
                    { "type": "field", "ip": ["geoip:private"], "outboundTag": "direct" },
                    { "type": "field", "network": "tcp,udp", "balancerTag": balancer_tag }
                ],
                "balancers": balancers
            });
            if let Some(observatory) = burst_observatory {
                config["burstObservatory"] = observatory.clone();
            }
        }
        _ => {
            config["outbounds"] = json!([
                outbound(server),
                { "protocol": "freedom", "tag": "direct" },
                { "protocol": "blackhole", "tag": "block" }
            ]);
            config["routing"] = json!({
                "domainStrategy": "IPIfNonMatch",
                "rules": [
                    { "type": "field", "ip": ["geoip:private"], "outboundTag": "direct" }
                ]
            });
        }
    }

    config
}

fn outbound(server: &Server) -> Value {
    let addr = &server.address;
    let port = server.port;
    match &server.spec {
        OutboundSpec::Vless {
            uuid,
            encryption,
            stream,
        } => json!({
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
        OutboundSpec::Vmess {
            uuid,
            alter_id,
            security,
            stream,
        } => json!({
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
        OutboundSpec::Hysteria2 { auth, settings } => json!({
            "tag": "proxy",
            // Xray names the protocol "hysteria" and selects v2 by version.
            "protocol": "hysteria",
            "settings": { "version": 2, "address": addr, "port": port },
            "streamSettings": hysteria2_stream(auth, settings, port)
        }),
        OutboundSpec::XrayProfile { .. } => {
            unreachable!("composite profiles are generated by generate")
        }
    }
}

/// Stream settings for a hysteria2 outbound.
///
/// Separate from [`stream_settings`] because the shapes have nothing in common:
/// the credentials live in the *transport* block here, and the obfuscation is a
/// sibling of it rather than a field inside it. Verified against xray 26.3.27
/// with `xray run -test`.
fn hysteria2_stream(auth: &str, s: &Hysteria2Settings, port: u16) -> Value {
    let udp_hop = (!s.port_hop.is_empty()).then(|| {
        // The advertised port must stay in the rotation, and first.
        let mut ports = vec![port.to_string()];
        ports.extend(s.port_hop.iter().map(|range| range.to_xray()));
        trim_obj(json!({ "ports": ports, "interval": s.hop_interval_secs }))
    });

    let mut v = json!({
        "network": "hysteria",
        "security": "tls",
        "tlsSettings": trim_obj(json!({
            "serverName": s.sni,
            "alpn": s.alpn,
            // `allowInsecure` is removed in Xray 26.x; see `stream_settings`.
            "pinnedPeerCertSha256": s.pin_sha256
        })),
        "hysteriaSettings": trim_obj(json!({
            "version": 2,
            "auth": auth,
            // Xray parses these as strings and rejects bare integers.
            "up": s.up_mbps.map(|n| format!("{n} mbps")),
            "down": s.down_mbps.map(|n| format!("{n} mbps")),
            "congestion": s.congestion,
            "udpIdleTimeout": s.udp_idle_timeout_secs,
            "udpHop": udp_hop
        }))
    });

    // Only emit an obfuscator Xray actually implements: an unknown `type` makes
    // it refuse to start, which is a worse failure than connecting without it.
    if let Some(obfs) = &s.obfs
        && obfs.kind.eq_ignore_ascii_case("salamander")
    {
        v["finalmask"] = json!({
            "type": "salamander",
            "settings": { "password": obfs.password }
        });
    }
    v
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
            // `allowInsecure` is deliberately not emitted: Xray 26.x removed it
            // ("has been removed and migrated to pinnedPeerCertSha256") and
            // rejects the whole config when it is true, so emitting it turns an
            // insecure-TLS server into a core that refuses to start. A pin is
            // the only remaining way to accept an otherwise-untrusted cert.
            v["tlsSettings"] = trim_obj(json!({
                "serverName": s.sni,
                "alpn": s.alpn,
                "fingerprint": s.fingerprint,
                "pinnedPeerCertSha256": s.pin_sha256
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::generate;
    use crate::model::{
        Hysteria2Obfs, Hysteria2Settings, OutboundSpec, PortRange, Protocol, Server, StreamSettings,
    };

    #[test]
    fn balanced_profile_keeps_local_inbounds_and_safe_routing() {
        let server = Server {
            id: "profile".to_string(),
            name: "Auto".to_string(),
            protocol: Protocol::Vless,
            address: "one.example".to_string(),
            port: 443,
            transport_label: "xray + balanced (2)".to_string(),
            country: None,
            spec: OutboundSpec::XrayProfile {
                proxy_outbounds: vec![
                    json!({"tag":"proxy","protocol":"vless","settings":{}}),
                    json!({"tag":"proxy-2","protocol":"vless","settings":{}}),
                ],
                balancers: vec![json!({"tag":"balance","selector":["proxy"]})],
                burst_observatory: Some(json!({"subjectSelector":["proxy"]})),
                balancer_tag: "balance".to_string(),
            },
            link: None,
            latency_ms: None,
        };

        let config = generate(&server, 10808, 10809);
        assert_eq!(config["inbounds"][0]["port"], 10808);
        assert_eq!(config["outbounds"].as_array().map(Vec::len), Some(4));
        assert_eq!(config["routing"]["rules"][0]["ip"][0], "geoip:private");
        assert_eq!(config["routing"]["rules"][1]["balancerTag"], "balance");
        assert_eq!(config["burstObservatory"]["subjectSelector"][0], "proxy");
    }

    fn tls_vless(allow_insecure: bool, pin: Option<&str>) -> Server {
        let stream = StreamSettings {
            network: "tcp".to_string(),
            security: "tls".to_string(),
            sni: Some("example.com".to_string()),
            allow_insecure,
            pin_sha256: pin.map(str::to_string),
            ..Default::default()
        };
        Server {
            id: "s".to_string(),
            name: "S".to_string(),
            protocol: Protocol::Vless,
            address: "example.com".to_string(),
            port: 443,
            transport_label: "vless + tls".to_string(),
            country: None,
            spec: OutboundSpec::Vless {
                uuid: "b831381d-6324-4d53-ad4f-8cda48b30811".to_string(),
                encryption: "none".to_string(),
                stream,
            },
            link: None,
            latency_ms: None,
        }
    }

    /// Xray 26.x rejects the whole config when `allowInsecure` is true, so a
    /// server that asks for it must still produce a config the core will start.
    #[test]
    fn allow_insecure_is_never_emitted() {
        for insecure in [true, false] {
            let config = generate(&tls_vless(insecure, None), 10808, 10809);
            let tls = &config["outbounds"][0]["streamSettings"]["tlsSettings"];
            assert!(
                tls.get("allowInsecure").is_none(),
                "allowInsecure leaked with insecure={insecure}: {tls}"
            );
            assert_eq!(tls["serverName"], "example.com");
        }
    }

    #[test]
    fn certificate_pin_is_emitted_as_a_bare_hex_string() {
        let pin = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let config = generate(&tls_vless(true, Some(pin)), 10808, 10809);
        let tls = &config["outbounds"][0]["streamSettings"]["tlsSettings"];
        // A bare string, not an array: Xray 26.x fails to decode the array form.
        assert_eq!(tls["pinnedPeerCertSha256"], json!(pin));
    }

    #[test]
    fn absent_pin_leaves_the_key_out() {
        let config = generate(&tls_vless(false, None), 10808, 10809);
        let tls = &config["outbounds"][0]["streamSettings"]["tlsSettings"];
        assert!(tls.get("pinnedPeerCertSha256").is_none(), "{tls}");
    }

    fn hysteria2(settings: Hysteria2Settings) -> Server {
        Server {
            id: "h".to_string(),
            name: "H".to_string(),
            protocol: Protocol::Hysteria2,
            address: "h.example".to_string(),
            port: 443,
            transport_label: "hysteria2".to_string(),
            country: None,
            spec: OutboundSpec::Hysteria2 {
                auth: "secret".to_string(),
                settings,
            },
            link: None,
            latency_ms: None,
        }
    }

    #[test]
    fn hysteria2_outbound_shape() {
        let config = generate(
            &hysteria2(Hysteria2Settings {
                sni: Some("h.example".to_string()),
                up_mbps: Some(100),
                down_mbps: Some(300),
                ..Default::default()
            }),
            10808,
            10809,
        );
        let out = &config["outbounds"][0];
        assert_eq!(out["protocol"], "hysteria");
        assert_eq!(out["settings"]["version"], 2);
        assert_eq!(out["settings"]["address"], "h.example");
        assert_eq!(out["settings"]["port"], 443);

        let stream = &out["streamSettings"];
        assert_eq!(stream["network"], "hysteria");
        assert_eq!(stream["security"], "tls");
        assert_eq!(stream["tlsSettings"]["serverName"], "h.example");

        let hy = &stream["hysteriaSettings"];
        assert_eq!(hy["version"], 2);
        // The credential lives in the transport block, not in `settings`.
        assert_eq!(hy["auth"], "secret");
        assert!(out["settings"].get("auth").is_none());
        // Xray rejects bare integers for these.
        assert_eq!(hy["up"], "100 mbps");
        assert_eq!(hy["down"], "300 mbps");
    }

    /// The obfuscator is a sibling of `hysteriaSettings`, not a field inside
    /// it, and the key is `finalmask` — `udpmasks` is only Xray's internal
    /// protobuf name and is silently ignored when used in JSON.
    #[test]
    fn salamander_is_a_stream_level_finalmask() {
        let config = generate(
            &hysteria2(Hysteria2Settings {
                obfs: Some(Hysteria2Obfs {
                    kind: "salamander".to_string(),
                    password: "obfspw".to_string(),
                }),
                ..Default::default()
            }),
            10808,
            10809,
        );
        let stream = &config["outbounds"][0]["streamSettings"];
        assert_eq!(stream["finalmask"]["type"], "salamander");
        assert_eq!(stream["finalmask"]["settings"]["password"], "obfspw");
        assert!(stream["hysteriaSettings"].get("finalmask").is_none());
        assert!(stream.get("udpmasks").is_none());
    }

    /// An obfuscator Xray does not implement must be dropped: an unknown type
    /// stops the core from starting at all.
    #[test]
    fn unknown_obfuscation_is_not_emitted() {
        let config = generate(
            &hysteria2(Hysteria2Settings {
                obfs: Some(Hysteria2Obfs {
                    kind: "quack".to_string(),
                    password: "x".to_string(),
                }),
                ..Default::default()
            }),
            10808,
            10809,
        );
        assert!(
            config["outbounds"][0]["streamSettings"]
                .get("finalmask")
                .is_none()
        );
    }

    #[test]
    fn port_hopping_keeps_the_primary_port_first() {
        let config = generate(
            &hysteria2(Hysteria2Settings {
                port_hop: vec![
                    PortRange {
                        start: 5000,
                        end: 6000,
                    },
                    PortRange {
                        start: 7000,
                        end: 7000,
                    },
                ],
                hop_interval_secs: Some(30),
                ..Default::default()
            }),
            10808,
            10809,
        );
        let hop = &config["outbounds"][0]["streamSettings"]["hysteriaSettings"]["udpHop"];
        assert_eq!(hop["ports"], json!(["443", "5000-6000", "7000"]));
        assert_eq!(hop["interval"], 30);
    }

    #[test]
    fn unset_hysteria2_options_are_omitted_entirely() {
        let config = generate(&hysteria2(Hysteria2Settings::default()), 10808, 10809);
        let stream = &config["outbounds"][0]["streamSettings"];
        let hy = &stream["hysteriaSettings"];
        for key in ["up", "down", "congestion", "udpIdleTimeout", "udpHop"] {
            assert!(hy.get(key).is_none(), "{key} should be absent: {hy}");
        }
        assert!(stream["tlsSettings"].get("serverName").is_none());
    }

    #[test]
    fn vless_outbound_shape() {
        let config = generate(&tls_vless(false, None), 10808, 10809);
        let out = &config["outbounds"][0];
        assert_eq!(out["protocol"], "vless");
        assert_eq!(out["settings"]["vnext"][0]["address"], "example.com");
        assert_eq!(out["settings"]["vnext"][0]["port"], 443);
        assert_eq!(
            out["settings"]["vnext"][0]["users"][0]["id"],
            "b831381d-6324-4d53-ad4f-8cda48b30811"
        );
        assert_eq!(out["streamSettings"]["network"], "tcp");
        assert_eq!(out["streamSettings"]["security"], "tls");
    }

    /// The only check that proves the generated JSON against a real core.
    /// Ignored by default because it needs an xray binary; run with
    /// `cargo test -- --ignored` inside `nix develop`.
    #[test]
    #[ignore = "requires an xray binary"]
    fn generated_configs_are_accepted_by_xray() {
        let xray = std::env::var("OXIDOM_XRAY_BIN").unwrap_or_else(|_| "xray".to_string());
        let servers = [
            hysteria2(Hysteria2Settings {
                sni: Some("h.example".to_string()),
                alpn: Some(vec!["h3".to_string()]),
                pin_sha256: Some(
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
                ),
                obfs: Some(Hysteria2Obfs {
                    kind: "salamander".to_string(),
                    password: "obfspw".to_string(),
                }),
                up_mbps: Some(100),
                down_mbps: Some(300),
                port_hop: vec![PortRange {
                    start: 5000,
                    end: 6000,
                }],
                hop_interval_secs: Some(30),
                congestion: Some("bbr".to_string()),
                udp_idle_timeout_secs: Some(60),
                allow_insecure: true,
            }),
            tls_vless(true, None),
        ];

        for server in &servers {
            let config = generate(server, 10808, 10809);
            let path = std::env::temp_dir().join(format!(
                "oxidom-cfg-{}-{}.json",
                std::process::id(),
                server.id
            ));
            std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
            let out = std::process::Command::new(&xray)
                .args(["run", "-test", "-c"])
                .arg(&path)
                .output()
                .expect("xray should be runnable");
            assert!(
                out.status.success(),
                "xray rejected the {} config:\n{}",
                server.protocol.as_str(),
                String::from_utf8_lossy(&out.stderr)
            );
            std::fs::remove_file(&path).ok();
        }
    }
}
