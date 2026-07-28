use std::collections::HashSet;
use std::net::Ipv4Addr;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::model::{Hysteria2Settings, OutboundSpec, Server, StreamSettings};

// The pool generator has no Config parameter, and an observatory needs one
// stable HTTP target regardless of the user's direct/active probe method.
const POOL_PROBE_DESTINATION: &str = "https://connectivitycheck.gstatic.com/generate_204";

/// Generate a full Xray config JSON for `server`, with local SOCKS + HTTP inbounds.
pub fn generate(server: &Server, bind: Ipv4Addr, socks_port: u16, http_port: u16) -> Value {
    match &server.spec {
        OutboundSpec::XrayProfile {
            proxy_outbounds,
            balancers,
            burst_observatory,
            balancer_tag,
        } => {
            let mut config = scaffold(bind, socks_port, http_port, proxy_outbounds.clone());
            config["routing"]["rules"] = json!([
                { "type": "field", "ip": ["geoip:private"], "outboundTag": "direct" },
                { "type": "field", "network": "tcp,udp", "balancerTag": balancer_tag }
            ]);
            config["routing"]["balancers"] = Value::Array(balancers.clone());
            if let Some(observatory) = burst_observatory {
                config["burstObservatory"] = observatory.clone();
            }
            config
        }
        _ => {
            let mut config = scaffold(bind, socks_port, http_port, vec![outbound(server)]);
            config["routing"]["rules"] = json!([
                { "type": "field", "ip": ["geoip:private"], "outboundTag": "direct" }
            ]);
            config
        }
    }
}

/// Everything about a pool that the generated config depends on. Grouped
/// because these four always travel together from the profile down to the
/// core, and passing them one by one made every hop an eight-argument call.
pub struct PoolSpec<'a> {
    pub members: &'a [&'a Server],
    pub strategy: &'a str,
    /// How many reachable nodes `leastLoad` keeps rotating; see `pool::Strategy`.
    pub expected: usize,
    pub probe_interval: &'a str,
}

pub fn generate_pool(
    spec: &PoolSpec<'_>,
    bind: Ipv4Addr,
    socks_port: u16,
    http_port: u16,
    api_port: u16,
) -> Result<Value> {
    let PoolSpec {
        members,
        strategy,
        expected,
        probe_interval,
    } = *spec;
    if members.is_empty() {
        bail!("cannot generate an Xray pool with no members");
    }

    let mut seen_tags = HashSet::with_capacity(members.len());
    let mut outbounds = Vec::with_capacity(members.len());
    for server in members {
        let handle = server.alias.as_deref().unwrap_or(&server.id);
        let tag = format!("s-{handle}");
        if !seen_tags.insert(tag.clone()) {
            bail!("duplicate Xray pool outbound tag {tag:?}");
        }
        let outbound = outbound_tagged(server, &tag).with_context(|| {
            format!(
                "server {:?} is a composite Xray profile and cannot be a pool member",
                server.name
            )
        })?;
        outbounds.push(outbound);
    }

    let mut config = scaffold(bind, socks_port, http_port, outbounds);
    config["inbounds"]
        .as_array_mut()
        .expect("the shared scaffold always has inbounds")
        .push(json!({
            "tag": "api-in",
            "listen": bind.to_string(),
            "port": api_port,
            "protocol": "dokodemo-door",
            "settings": { "address": "127.0.0.1" }
        }));
    // `leastLoad` is the only strategy that reads `expected`, and it is what
    // makes a pool both spread and survive: the core keeps that many of the
    // nodes it can still reach and rotates across them. Emitting it for the
    // health-blind strategies would suggest they filter, which they do not.
    let mut strategy_value = json!({ "type": strategy });
    if strategy == "leastLoad" {
        strategy_value["settings"] = json!({ "expected": expected.max(1) });
    }
    config["routing"]["balancers"] = json!([{
        "tag": "pool",
        "selector": ["s-"],
        "strategy": strategy_value
    }]);
    // This rule must precede the catch-all balancer rule or `xray api bi`
    // routes its own request into the pool and waits until it times out.
    config["routing"]["rules"] = json!([
        { "type": "field", "inboundTag": ["api-in"], "outboundTag": "api" },
        { "type": "field", "ip": ["geoip:private"], "outboundTag": "direct" },
        { "type": "field", "network": "tcp,udp", "balancerTag": "pool" }
    ]);
    config["burstObservatory"] = json!({
        "subjectSelector": ["s-"],
        "pingConfig": {
            "destination": POOL_PROBE_DESTINATION,
            "interval": probe_interval,
            "timeout": "3s",
            "sampling": 3
        }
    });
    config["api"] = json!({
        "tag": "api",
        "services": ["RoutingService"]
    });
    Ok(config)
}

fn scaffold(
    bind: Ipv4Addr,
    socks_port: u16,
    http_port: u16,
    mut proxy_outbounds: Vec<Value>,
) -> Value {
    proxy_outbounds.push(json!({ "protocol": "freedom", "tag": "direct" }));
    proxy_outbounds.push(json!({ "protocol": "blackhole", "tag": "block" }));
    json!({
        "log": { "loglevel": "warning" },
        "inbounds": [
            {
                "tag": "socks-in",
                "listen": bind.to_string(),
                "port": socks_port,
                "protocol": "socks",
                "settings": { "auth": "noauth", "udp": true },
                "sniffing": { "enabled": true, "destOverride": ["http", "tls"] }
            },
            {
                "tag": "http-in",
                "listen": bind.to_string(),
                "port": http_port,
                "protocol": "http",
                "sniffing": { "enabled": true, "destOverride": ["http", "tls"] }
            }
        ],
        "outbounds": proxy_outbounds,
        "routing": { "domainStrategy": "IPIfNonMatch" }
    })
}

fn outbound(server: &Server) -> Value {
    outbound_tagged(server, "proxy")
        .unwrap_or_else(|| unreachable!("composite profiles are generated by generate"))
}

fn outbound_tagged(server: &Server, tag: &str) -> Option<Value> {
    let addr = &server.address;
    let port = server.port;
    Some(match &server.spec {
        OutboundSpec::Vless {
            uuid,
            encryption,
            stream,
        } => json!({
            "tag": tag,
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
            "tag": tag,
            "protocol": "vmess",
            "settings": { "vnext": [ {
                "address": addr,
                "port": port,
                "users": [ { "id": uuid, "alterId": alter_id, "security": security } ]
            } ] },
            "streamSettings": stream_settings(stream)
        }),
        OutboundSpec::Trojan { password, stream } => json!({
            "tag": tag,
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
            "tag": tag,
            "protocol": "shadowsocks",
            "settings": { "servers": [ {
                "address": addr,
                "port": port,
                "method": method,
                "password": password
            } ] }
        }),
        OutboundSpec::Socks { username, password } => json!({
            "tag": tag,
            "protocol": "socks",
            "settings": { "servers": [ socks_http_server(addr, port, username, password) ] }
        }),
        OutboundSpec::Http { username, password } => json!({
            "tag": tag,
            "protocol": "http",
            "settings": { "servers": [ socks_http_server(addr, port, username, password) ] }
        }),
        OutboundSpec::Hysteria2 { auth, settings } => json!({
            "tag": tag,
            // Xray names the protocol "hysteria" and selects v2 by version.
            "protocol": "hysteria",
            "settings": { "version": 2, "address": addr, "port": port },
            "streamSettings": hysteria2_stream(auth, settings, port)
        }),
        OutboundSpec::XrayProfile { .. } => return None,
    })
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
    use std::net::Ipv4Addr;

    use serde_json::json;

    use super::{PoolSpec, generate, generate_pool};
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
            alias: None,
            latency_ms: None,
        };

        let config = generate(&server, Ipv4Addr::LOCALHOST, 10808, 10809);
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
            alias: None,
            latency_ms: None,
        }
    }

    fn socks_server() -> Server {
        Server {
            id: "socks".to_string(),
            name: "SOCKS".to_string(),
            protocol: Protocol::Socks,
            address: "proxy.example".to_string(),
            port: 1080,
            transport_label: "socks".to_string(),
            country: None,
            spec: OutboundSpec::Socks {
                username: None,
                password: None,
            },
            link: None,
            alias: None,
            latency_ms: None,
        }
    }

    #[test]
    fn default_bind_keeps_the_legacy_config_bytes() {
        let generated = generate(&socks_server(), Ipv4Addr::LOCALHOST, 10808, 10809);
        let legacy = json!({
            "log": { "loglevel": "warning" },
            "inbounds": [
                {
                    "tag": "socks-in",
                    "listen": "127.0.0.1",
                    "port": 10808,
                    "protocol": "socks",
                    "settings": { "auth": "noauth", "udp": true },
                    "sniffing": { "enabled": true, "destOverride": ["http", "tls"] }
                },
                {
                    "tag": "http-in",
                    "listen": "127.0.0.1",
                    "port": 10809,
                    "protocol": "http",
                    "sniffing": { "enabled": true, "destOverride": ["http", "tls"] }
                }
            ],
            "outbounds": [
                {
                    "tag": "proxy",
                    "protocol": "socks",
                    "settings": { "servers": [{ "address": "proxy.example", "port": 1080 }] }
                },
                { "protocol": "freedom", "tag": "direct" },
                { "protocol": "blackhole", "tag": "block" }
            ],
            "routing": {
                "domainStrategy": "IPIfNonMatch",
                "rules": [
                    { "type": "field", "ip": ["geoip:private"], "outboundTag": "direct" }
                ]
            }
        });

        assert_eq!(
            serde_json::to_vec(&generated).unwrap(),
            serde_json::to_vec(&legacy).unwrap()
        );
    }

    #[test]
    fn pool_config_has_member_tags_api_first_routing_and_observatory() {
        let mut socks = socks_server();
        socks.alias = Some("socks-node".to_string());
        let mut vless = tls_vless(false, None);
        vless.id = "vless-id".to_string();
        let bind = Ipv4Addr::new(127, 72, 14, 1);

        let config = generate_pool(
            &PoolSpec {
                members: &[&socks, &vless],
                strategy: "leastLoad",
                expected: 2,
                probe_interval: "5m",
            },
            bind,
            10808,
            10809,
            10810,
        )
        .unwrap();

        assert_eq!(config["inbounds"][2]["tag"], "api-in");
        assert_eq!(config["inbounds"][2]["listen"], "127.72.14.1");
        assert_eq!(config["inbounds"][2]["port"], 10810);
        assert_eq!(config["outbounds"][0]["tag"], "s-socks-node");
        assert_eq!(config["outbounds"][1]["tag"], "s-vless-id");
        assert_eq!(config["outbounds"][2]["tag"], "direct");
        assert_eq!(config["outbounds"][3]["tag"], "block");

        let routing = &config["routing"];
        assert_eq!(routing["balancers"][0]["tag"], "pool");
        assert_eq!(routing["balancers"][0]["selector"], json!(["s-"]));
        assert_eq!(routing["balancers"][0]["strategy"]["type"], "leastLoad");
        // Without `expected` the core would settle on one node, which is the
        // opposite of spreading traffic across exits.
        assert_eq!(
            routing["balancers"][0]["strategy"]["settings"]["expected"],
            2
        );
        assert_eq!(routing["rules"][0]["inboundTag"], json!(["api-in"]));
        assert_eq!(routing["rules"][0]["outboundTag"], "api");
        assert_eq!(routing["rules"][1]["outboundTag"], "direct");
        assert_eq!(routing["rules"][2]["balancerTag"], "pool");
        assert_eq!(config["burstObservatory"]["subjectSelector"], json!(["s-"]));
        assert_eq!(config["burstObservatory"]["pingConfig"]["interval"], "5m");
        assert_eq!(config["api"]["services"], json!(["RoutingService"]));
    }

    #[test]
    fn duplicate_pool_tags_are_rejected() {
        let mut by_alias = socks_server();
        by_alias.alias = Some("collision".to_string());
        let mut by_id = tls_vless(false, None);
        by_id.id = "collision".to_string();

        let error = generate_pool(
            &PoolSpec {
                members: &[&by_alias, &by_id],
                strategy: "leastLoad",
                expected: 2,
                probe_interval: "5m",
            },
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            10810,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(error, "duplicate Xray pool outbound tag \"s-collision\"");
    }

    #[test]
    fn both_inbounds_use_the_session_address() {
        let bind = Ipv4Addr::new(127, 72, 14, 1);
        let config = generate(&socks_server(), bind, 10808, 10809);
        assert_eq!(config["inbounds"][0]["listen"], "127.72.14.1");
        assert_eq!(config["inbounds"][1]["listen"], "127.72.14.1");
    }

    /// Xray 26.x rejects the whole config when `allowInsecure` is true, so a
    /// server that asks for it must still produce a config the core will start.
    #[test]
    fn allow_insecure_is_never_emitted() {
        for insecure in [true, false] {
            let config = generate(
                &tls_vless(insecure, None),
                Ipv4Addr::LOCALHOST,
                10808,
                10809,
            );
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
        let config = generate(
            &tls_vless(true, Some(pin)),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
        );
        let tls = &config["outbounds"][0]["streamSettings"]["tlsSettings"];
        // A bare string, not an array: Xray 26.x fails to decode the array form.
        assert_eq!(tls["pinnedPeerCertSha256"], json!(pin));
    }

    #[test]
    fn absent_pin_leaves_the_key_out() {
        let config = generate(&tls_vless(false, None), Ipv4Addr::LOCALHOST, 10808, 10809);
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
            alias: None,
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
            Ipv4Addr::LOCALHOST,
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
            Ipv4Addr::LOCALHOST,
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
            Ipv4Addr::LOCALHOST,
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
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
        );
        let hop = &config["outbounds"][0]["streamSettings"]["hysteriaSettings"]["udpHop"];
        assert_eq!(hop["ports"], json!(["443", "5000-6000", "7000"]));
        assert_eq!(hop["interval"], 30);
    }

    #[test]
    fn unset_hysteria2_options_are_omitted_entirely() {
        let config = generate(
            &hysteria2(Hysteria2Settings::default()),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
        );
        let stream = &config["outbounds"][0]["streamSettings"];
        let hy = &stream["hysteriaSettings"];
        for key in ["up", "down", "congestion", "udpIdleTimeout", "udpHop"] {
            assert!(hy.get(key).is_none(), "{key} should be absent: {hy}");
        }
        assert!(stream["tlsSettings"].get("serverName").is_none());
    }

    #[test]
    fn vless_outbound_shape() {
        let config = generate(&tls_vless(false, None), Ipv4Addr::LOCALHOST, 10808, 10809);
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
        // Start from real share links too, so the whole path from what a user
        // pastes to what the core reads is covered, not just the generator.
        let from_links = [
            "hysteria2://pa%3Ass@h.example:443,5000-6000/?obfs=salamander&obfs-password=o\
             &sni=real.example&insecure=1&up=100%20mbps&down=1%20gbps&hopInterval=30#HY2",
            "hy2://pw@h.example/",
            "vless://b831381d-6324-4d53-ad4f-8cda48b30811@example.com:443\
             ?type=ws&security=tls&sni=cdn.example&path=%2Fws&allowInsecure=1#WS",
        ]
        .into_iter()
        .map(|link| crate::link::parse_link(link).expect("sample link should parse"));

        let servers: Vec<Server> = from_links
            .chain([
                hysteria2(Hysteria2Settings {
                    sni: Some("h.example".to_string()),
                    alpn: Some(vec!["h3".to_string()]),
                    pin_sha256: Some(
                        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                            .to_string(),
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
            ])
            .collect();

        let mut configs = Vec::new();
        for (index, server) in servers.iter().enumerate() {
            let bind = if index == 0 {
                Ipv4Addr::new(127, 72, 14, 1)
            } else {
                Ipv4Addr::LOCALHOST
            };
            configs.push((
                server.protocol.as_str().to_string(),
                generate(server, bind, 10808, 10809),
            ));
        }
        configs.push((
            "pool".to_string(),
            generate_pool(
                &PoolSpec {
                    members: &[&servers[0], &servers[2]],
                    strategy: "leastLoad",
                    expected: 2,
                    probe_interval: "5m",
                },
                Ipv4Addr::new(127, 72, 14, 1),
                10808,
                10809,
                10810,
            )
            .expect("sample pool should generate"),
        ));

        for (index, (label, config)) in configs.into_iter().enumerate() {
            let path = std::env::temp_dir().join(format!(
                "oxidom-cfg-{}-{index}-{label}.json",
                std::process::id(),
            ));
            std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
            let out = std::process::Command::new(&xray)
                .args(["run", "-test", "-c"])
                .arg(&path)
                .output()
                .expect("xray should be runnable");
            assert!(
                out.status.success(),
                "xray rejected the {label} config:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            eprintln!(
                "xray accepted {label}: stdout={:?}, stderr={:?}",
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            std::fs::remove_file(&path).ok();
        }
    }
}
