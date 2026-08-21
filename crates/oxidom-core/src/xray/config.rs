use std::collections::HashSet;
use std::net::Ipv4Addr;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::core_options::{ResolvedCore, ResolvedDialer, ResolvedDns, ResolvedMux};
use crate::model::{Hysteria2Settings, OutboundSpec, Server, StreamSettings};

/// Namespace every balancer-selectable outbound tag shares.
///
/// Xray resolves a balancer `selector` by prefix-matching outbound tags, and
/// `scaffold` always appends `direct` and `block`. Keeping the selectable
/// outbounds under one prefix oxidom owns is what stops a selector from
/// resolving to either of those.
const SELECTABLE_TAG_PREFIX: &str = "s-";

/// Balancer tag oxidom's own catch-all routing rule dispatches to.
const BALANCER_TAG: &str = "pool";

/// The `freedom` outbound that proxy outbounds dial through when fragmentation
/// or noises are configured.
///
/// Named for the job rather than for fragmentation, because noises can be asked
/// for without it. Note that it does **not** start with [`SELECTABLE_TAG_PREFIX`]:
/// a balancer selector must never be able to resolve to it, or a pool would send
/// traffic straight out through freedom while the UI still said Connected.
const DIALER_TAG: &str = "dialer";

/// The balancer strategies Xray accepts. Anything else is a provider typo or an
/// injection attempt, and both should land on the same safe default.
const BALANCER_STRATEGIES: [&str; 4] = ["random", "roundRobin", "leastPing", "leastLoad"];

/// Generate a full Xray config JSON for `server`, with local SOCKS + HTTP inbounds.
pub fn generate(
    server: &Server,
    bind: Ipv4Addr,
    socks_port: u16,
    http_port: u16,
    core: &ResolvedCore,
) -> Value {
    match &server.spec {
        OutboundSpec::XrayProfile {
            proxy_outbounds,
            balancers,
            burst_observatory,
            balancer_tag,
        } => {
            // Everything below is provider-supplied, so none of it may reach the
            // core as written. A balancer selector is prefix-matched against
            // outbound tags, and `scaffold` always appends `direct` (freedom):
            // an imported selector of ["direct"] — or [""], which matches every
            // tag — would route the whole tunnel out in the clear while the UI
            // still reported Connected. Re-tag the imported outbounds into a
            // namespace we own and rebuild the balancer around that prefix, so
            // the selector can only ever resolve to a proxy outbound.
            let namespaced = namespace_outbounds(proxy_outbounds);
            let mut config = scaffold(bind, socks_port, http_port, namespaced, core);
            install_rules(
                &mut config,
                core,
                None,
                vec![json!({ "type": "field", "ip": ["geoip:private"], "outboundTag": "direct" })],
                Some(json!({ "type": "field", "network": "tcp,udp", "balancerTag": BALANCER_TAG })),
            );
            config["routing"]["balancers"] = json!([{
                "tag": BALANCER_TAG,
                "selector": [SELECTABLE_TAG_PREFIX],
                "strategy": import_strategy(balancers, balancer_tag)
            }]);
            if burst_observatory.is_some() {
                // Keep the observatory the leastPing/leastLoad strategies need,
                // but not the provider's `pingConfig.destination`: that is a URL
                // the core would fetch on a timer, i.e. a beacon from the host.
                config["burstObservatory"] = json!({
                    "subjectSelector": [SELECTABLE_TAG_PREFIX],
                    "pingConfig": {
                        "destination": core.pool_probe,
                        "interval": "5m",
                        "timeout": "3s",
                        "sampling": 3
                    }
                });
            }
            config
        }
        _ => {
            let mut config = scaffold(bind, socks_port, http_port, vec![outbound(server)], core);
            install_rules(
                &mut config,
                core,
                None,
                vec![json!({ "type": "field", "ip": ["geoip:private"], "outboundTag": "direct" })],
                None,
            );
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
    core: &ResolvedCore,
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
        let tag = format!("{SELECTABLE_TAG_PREFIX}{handle}");
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

    let mut config = scaffold(bind, socks_port, http_port, outbounds, core);
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
        "tag": BALANCER_TAG,
        "selector": [SELECTABLE_TAG_PREFIX],
        "strategy": strategy_value
    }]);
    // The api rule must precede the catch-all balancer rule or `xray api bi`
    // routes its own request into the pool and waits until it times out. It also
    // precedes the profile's own rules: a user rule matching the api request
    // would break the same call, and nothing a user writes is about that inbound.
    install_rules(
        &mut config,
        core,
        Some(json!({ "type": "field", "inboundTag": ["api-in"], "outboundTag": "api" })),
        vec![json!({ "type": "field", "ip": ["geoip:private"], "outboundTag": "direct" })],
        Some(json!({ "type": "field", "network": "tcp,udp", "balancerTag": BALANCER_TAG })),
    );
    config["burstObservatory"] = json!({
        "subjectSelector": [SELECTABLE_TAG_PREFIX],
        "pingConfig": {
            "destination": core.pool_probe,
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

/// Re-tag imported proxy outbounds into the [`SELECTABLE_TAG_PREFIX`] namespace.
///
/// The imported tags are provider-supplied and nothing in the generated config
/// refers to them — oxidom writes its own routing rules — so overwriting them
/// costs nothing and makes the balancer selector exact.
fn namespace_outbounds(proxy_outbounds: &[Value]) -> Vec<Value> {
    proxy_outbounds
        .iter()
        .enumerate()
        .map(|(index, outbound)| {
            let mut outbound = outbound.clone();
            if let Some(object) = outbound.as_object_mut() {
                object.insert(
                    "tag".to_string(),
                    Value::String(format!("{SELECTABLE_TAG_PREFIX}{index}")),
                );
            }
            outbound
        })
        .collect()
}

/// The imported balancer's strategy, reduced to the fields Xray reads.
///
/// `balancer_tag` names the balancer the provider's own routing rule pointed at,
/// so its strategy is the part of the import worth honouring. Everything else
/// about the balancer — above all its `selector` — is rebuilt by the caller.
fn import_strategy(balancers: &[Value], balancer_tag: &str) -> Value {
    let chosen = balancers
        .iter()
        .find(|balancer| balancer.get("tag").and_then(Value::as_str) == Some(balancer_tag))
        .or_else(|| balancers.first());
    let name = chosen
        .and_then(|balancer| balancer.pointer("/strategy/type"))
        .and_then(Value::as_str)
        .filter(|name| BALANCER_STRATEGIES.contains(name))
        .unwrap_or("random");
    let mut strategy = json!({ "type": name });
    if name == "leastLoad" {
        let expected = chosen
            .and_then(|balancer| balancer.pointer("/strategy/settings/expected"))
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1);
        strategy["settings"] = json!({ "expected": expected });
    }
    strategy
}

/// Assemble `routing.rules`, with the profile's own block spliced in.
///
/// Order is the whole of this function: the api rule first when there is one,
/// then whatever the profile carries, then the rules oxidom installs, then the
/// balancer's catch-all last. A user's rule therefore wins over the
/// private-address rule below it — which is the point of carrying one — while
/// the two positions `docs/spec/xray-config.md` calls binding keep theirs.
///
/// Anything else in the block (`domainMatcher`, say) is copied onto the routing
/// object as written. `crate::xray::routing::validate` has already refused the
/// keys that must not survive the trip.
fn install_rules(
    config: &mut Value,
    core: &ResolvedCore,
    api: Option<Value>,
    built_in: Vec<Value>,
    balancer: Option<Value>,
) {
    let (profile_rules, rest) = core
        .routing
        .as_ref()
        .map(crate::xray::routing::parts)
        .unwrap_or_default();
    for (key, value) in rest {
        config["routing"][key] = value;
    }
    let mut rules = Vec::new();
    rules.extend(api);
    rules.extend(profile_rules);
    rules.extend(built_in);
    rules.extend(balancer);
    config["routing"]["rules"] = Value::Array(rules);
}

fn scaffold(
    bind: Ipv4Addr,
    socks_port: u16,
    http_port: u16,
    mut proxy_outbounds: Vec<Value>,
    core: &ResolvedCore,
) -> Value {
    // Only the proxy outbounds are multiplexed and dialed through the dialer.
    // `direct` and `block` are appended below and must stay untouched — and the
    // dialer must never be told to dial through itself.
    for outbound in &mut proxy_outbounds {
        apply_outbound_core(outbound, core);
    }
    if let Some(dialer) = &core.dialer {
        proxy_outbounds.push(dialer_outbound(dialer));
    }
    proxy_outbounds.push(json!({ "protocol": "freedom", "tag": "direct" }));
    proxy_outbounds.push(json!({ "protocol": "blackhole", "tag": "block" }));

    let sniffing = sniffing_block(core);
    let mut config = json!({
        "log": { "loglevel": core.log_level.as_xray() },
        "inbounds": [
            {
                "tag": "socks-in",
                "listen": bind.to_string(),
                "port": socks_port,
                "protocol": "socks",
                "settings": { "auth": "noauth", "udp": true },
                "sniffing": sniffing
            },
            {
                "tag": "http-in",
                "listen": bind.to_string(),
                "port": http_port,
                "protocol": "http",
                "sniffing": sniffing
            }
        ],
        "outbounds": proxy_outbounds,
        "routing": { "domainStrategy": core.domain_strategy.as_xray() }
    });
    if let Some(dns) = &core.dns {
        config["dns"] = dns_block(dns);
    }
    config
}

/// `routeOnly` is emitted only when asked for, because `false` is the core's own
/// default and writing it would move bytes in a config nobody changed.
fn sniffing_block(core: &ResolvedCore) -> Value {
    let dest_override = core
        .sniffing
        .dest_override
        .iter()
        .map(|kind| kind.as_xray())
        .collect::<Vec<_>>();
    let mut sniffing = json!({
        "enabled": core.sniffing.enabled,
        "destOverride": dest_override
    });
    if core.sniffing.route_only {
        sniffing["routeOnly"] = json!(true);
    }
    sniffing
}

/// The direct resolver comes first and is scoped to `geosite:private`, so a name
/// the local network answers is not sent to the resolver behind the tunnel.
/// `geosite:private` is a real list — the core rejects an unknown one outright,
/// which is what makes this safe to emit unconditionally alongside a server.
fn dns_block(dns: &ResolvedDns) -> Value {
    let mut servers = Vec::with_capacity(2);
    if let Some(direct) = &dns.direct_server {
        servers.push(json!({
            "address": direct,
            "domains": ["geosite:private"],
            "skipFallback": true
        }));
    }
    servers.push(json!(dns.server));
    json!({
        "servers": servers,
        "queryStrategy": dns.query_strategy.as_xray()
    })
}

/// Attach the per-outbound half of the core settings.
///
/// Done here rather than inside [`outbound_tagged`] because `mux` and `sockopt`
/// are the same two keys on all eight protocol shapes; threading them through
/// the match would repeat the logic once per arm and let the arms drift.
fn apply_outbound_core(outbound: &mut Value, core: &ResolvedCore) {
    if let Some(mux) = &core.mux {
        outbound["mux"] = mux_block(mux);
    }
    if core.dialer.is_some() {
        // `sockopt` hangs off `streamSettings`, which the plain protocols
        // (shadowsocks, socks, http) do not otherwise emit at all; indexing
        // creates the objects on the way down. Only `dialerProxy` is written,
        // so an imported profile's own sockopt keys survive.
        outbound["streamSettings"]["sockopt"]["dialerProxy"] = json!(DIALER_TAG);
    }
}

fn mux_block(mux: &ResolvedMux) -> Value {
    trim_obj(json!({
        "enabled": true,
        "concurrency": mux.concurrency,
        "xudpConcurrency": mux.xudp_concurrency,
        "xudpProxyUDP443": mux.xudp_proxy_udp_443.map(|mode| mode.as_xray())
    }))
}

fn dialer_outbound(dialer: &ResolvedDialer) -> Value {
    let noises = dialer
        .noises
        .iter()
        .map(|noise| {
            json!({
                "type": noise.kind.as_xray(),
                "packet": noise.packet,
                "delay": noise.delay
            })
        })
        .collect::<Vec<_>>();
    let fragment = dialer.fragment.as_ref().map(|fragment| {
        json!({
            "packets": fragment.packets,
            "length": fragment.length,
            "interval": fragment.interval
        })
    });

    json!({
        "tag": DIALER_TAG,
        "protocol": "freedom",
        "settings": trim_obj(json!({
            "fragment": fragment,
            "noises": (!noises.is_empty()).then_some(noises)
        }))
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

    use super::{
        BALANCER_TAG, DIALER_TAG, PoolSpec, SELECTABLE_TAG_PREFIX, generate, generate_pool,
    };
    use crate::core_options::{
        CoreOptions, DestOverride, DnsOptions, DomainStrategy, FragmentOptions, LogLevel,
        MuxOptions, Noise, NoiseKind, QueryStrategy, ResolvedCore, SniffingOptions, XudpMode,
    };
    use crate::model::{
        Hysteria2Obfs, Hysteria2Settings, OutboundSpec, PortRange, Protocol, Server, StreamSettings,
    };
    use crate::xray::routing;

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

        let config = generate(
            &server,
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &ResolvedCore::default(),
        );
        assert_eq!(config["inbounds"][0]["port"], 10808);
        assert_eq!(config["outbounds"].as_array().map(Vec::len), Some(4));
        assert_eq!(config["routing"]["rules"][0]["ip"][0], "geoip:private");
        // The imported tags are replaced by oxidom's own namespace, so the rule,
        // the balancer and the observatory all speak the `s-` prefix rather than
        // anything the provider chose.
        assert_eq!(config["routing"]["rules"][1]["balancerTag"], "pool");
        assert_eq!(config["routing"]["balancers"][0]["tag"], "pool");
        assert_eq!(config["routing"]["balancers"][0]["selector"][0], "s-");
        assert_eq!(config["outbounds"][0]["tag"], "s-0");
        assert_eq!(config["outbounds"][1]["tag"], "s-1");
        assert_eq!(config["burstObservatory"]["subjectSelector"][0], "s-");
    }

    /// The balancer is the one way a subscription could reach the built-in
    /// `freedom` outbound: Xray prefix-matches a selector against outbound tags,
    /// and `scaffold` always appends `direct`. A selector of ["direct"] — or [""],
    /// which matches every tag — would put the whole tunnel in the clear while
    /// the UI still said Connected.
    #[test]
    fn an_imported_balancer_cannot_select_the_direct_outbound() {
        for hostile in [json!(["direct"]), json!([""]), json!(["block"])] {
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
                        json!({"tag":"direct","protocol":"vless","settings":{}}),
                        json!({"tag":"proxy-2","protocol":"vless","settings":{}}),
                    ],
                    balancers: vec![json!({
                        "tag": "b",
                        "selector": hostile,
                        "strategy": {"type": "leastPing"}
                    })],
                    burst_observatory: None,
                    balancer_tag: "b".to_string(),
                },
                link: None,
                alias: None,
                latency_ms: None,
            };

            let config = generate(
                &server,
                Ipv4Addr::LOCALHOST,
                10808,
                10809,
                &ResolvedCore::default(),
            );
            let balancers = config["routing"]["balancers"].as_array().unwrap();
            assert_eq!(balancers.len(), 1, "{hostile}");
            assert_eq!(balancers[0]["selector"], json!(["s-"]), "{hostile}");
            // The strategy is the only part of the import that survives.
            assert_eq!(balancers[0]["strategy"]["type"], "leastPing", "{hostile}");
            // An imported outbound cannot squat on a built-in tag either.
            let tags: Vec<&str> = config["outbounds"]
                .as_array()
                .unwrap()
                .iter()
                .map(|outbound| outbound["tag"].as_str().unwrap())
                .collect();
            assert_eq!(tags, ["s-0", "s-1", "direct", "block"], "{hostile}");
        }
    }

    /// A provider-chosen observatory destination would be a beacon the core
    /// fetches on a timer.
    #[test]
    fn an_imported_observatory_destination_is_replaced() {
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
                    json!({"tag":"a","protocol":"vless","settings":{}}),
                    json!({"tag":"b","protocol":"vless","settings":{}}),
                ],
                balancers: vec![json!({"tag":"b","selector":["a"]})],
                burst_observatory: Some(json!({
                    "subjectSelector": ["a"],
                    "pingConfig": {"destination": "https://tracker.example/beacon"}
                })),
                balancer_tag: "b".to_string(),
            },
            link: None,
            alias: None,
            latency_ms: None,
        };

        let config = generate(
            &server,
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &ResolvedCore::default(),
        );
        assert_eq!(
            config["burstObservatory"]["pingConfig"]["destination"],
            crate::core_options::DEFAULT_POOL_PROBE,
            "the provider's beacon survived, or the built-in destination moved"
        );
        assert_eq!(config["burstObservatory"]["subjectSelector"], json!(["s-"]));

        // The overwrite is unconditional; only what it writes is now settable.
        // A configured destination must replace the provider's just as the
        // built-in one does — this is the half that would silently regress if
        // somebody made the overwrite conditional on the value being a default.
        let options = CoreOptions {
            pool_probe_url: Some("https://reachable.example/generate_204".to_string()),
            ..CoreOptions::default()
        };
        let configured = generate(
            &server,
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &CoreOptions::resolve(&options, &CoreOptions::default()),
        );
        assert_eq!(
            configured["burstObservatory"]["pingConfig"]["destination"],
            "https://reachable.example/generate_204"
        );
        // An unrecognised strategy falls back to a safe default rather than
        // reaching the core as written.
        assert_eq!(
            config["routing"]["balancers"][0]["strategy"]["type"],
            "random"
        );
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
        let generated = generate(
            &socks_server(),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &ResolvedCore::default(),
        );
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
            &ResolvedCore::default(),
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

    /// A pool the size a country rule actually produces, with the shape a real
    /// subscription has: many entries over few hosts.
    ///
    /// Every earlier pool test used two members, so nothing pinned that 42
    /// outbounds, one balancer and one observatory still come out as one config
    /// the core will take. Paired with the `--ignored` check below, which runs
    /// this very shape through `xray run -test`.
    fn country_sized_pool() -> Vec<Server> {
        (0..42)
            .map(|index| {
                let mut server = tls_vless(false, None);
                // Distinct handles: `s-<handle>` tags collide otherwise, which
                // `generate_pool` rejects outright.
                server.id = format!("de-{index}");
                server.name = format!("Germany {index}");
                // Nine hosts for 42 entries — the store this was measured on had
                // 26 of its 42 German entries on one `address:port`.
                server.address = format!("de{}.example", index % 9);
                server
            })
            .collect()
    }

    /// Core settings as they arrive from a profile: everything unset globally.
    fn from_profile(options: CoreOptions) -> ResolvedCore {
        CoreOptions::resolve(&CoreOptions::default(), &options)
    }

    fn fragmenting() -> ResolvedCore {
        from_profile(CoreOptions {
            fragment: FragmentOptions {
                enabled: Some(true),
                ..FragmentOptions::default()
            },
            ..CoreOptions::default()
        })
    }

    #[test]
    fn without_fragmentation_or_noises_nothing_dials_through_anything() {
        let config = generate(
            &socks_server(),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &ResolvedCore::default(),
        );

        let outbounds = config["outbounds"].as_array().unwrap();
        assert!(!outbounds.iter().any(|out| out["tag"] == DIALER_TAG));
        // Not merely absent from the tags — the proxy outbound must carry no
        // reference to it either. The core accepts a dangling `dialerProxy`
        // without a word, so only this assertion catches the pairing breaking.
        assert_eq!(outbounds[0]["streamSettings"]["sockopt"], json!(null));
    }

    #[test]
    fn fragmentation_adds_a_dialer_and_points_the_proxy_at_it() {
        let config = generate(
            &socks_server(),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &fragmenting(),
        );

        let outbounds = config["outbounds"].as_array().unwrap();
        // The proxy stays first: on a pool the core hands the very first
        // request to whatever leads the list, and that ordering is load-bearing.
        assert_eq!(outbounds[0]["tag"], "proxy");
        assert_eq!(
            outbounds[0]["streamSettings"]["sockopt"]["dialerProxy"],
            DIALER_TAG
        );
        assert_eq!(outbounds[1]["tag"], DIALER_TAG);
        assert_eq!(outbounds[1]["protocol"], "freedom");
        assert_eq!(outbounds[1]["settings"]["fragment"]["packets"], "tlshello");
        assert_eq!(outbounds[1]["settings"]["noises"], json!(null));

        // `direct` and `block` must not be dialed through the fragmenter: they
        // exist precisely to leave the tunnel.
        assert_eq!(outbounds[2]["tag"], "direct");
        assert_eq!(outbounds[2]["streamSettings"], json!(null));
        assert_eq!(outbounds[3]["tag"], "block");
        assert_eq!(outbounds[3]["streamSettings"], json!(null));
    }

    /// Noises without fragmentation still need the outbound to hang off.
    #[test]
    fn noises_alone_are_enough_to_raise_a_dialer() {
        let core = from_profile(CoreOptions {
            noises: Some(vec![Noise {
                kind: NoiseKind::Base64,
                packet: "aGVsbG8=".to_string(),
                delay: "10-16".to_string(),
            }]),
            ..CoreOptions::default()
        });

        let config = generate(&socks_server(), Ipv4Addr::LOCALHOST, 10808, 10809, &core);

        let dialer = &config["outbounds"][1];
        assert_eq!(dialer["tag"], DIALER_TAG);
        assert_eq!(dialer["settings"]["fragment"], json!(null));
        assert_eq!(dialer["settings"]["noises"][0]["type"], "base64");
    }

    /// The whole reason the tag is `dialer` and not `s-dialer`: a balancer
    /// selector is a prefix match, and picking the fragmenter would send the
    /// pool's traffic out through plain freedom.
    #[test]
    fn a_pool_dials_every_member_through_one_dialer_it_can_never_select() {
        let mut first = socks_server();
        first.alias = Some("node-a".to_string());
        let mut second = tls_vless(false, None);
        second.id = "node-b".to_string();

        let config = generate_pool(
            &PoolSpec {
                members: &[&first, &second],
                strategy: "leastLoad",
                expected: 2,
                probe_interval: "5m",
            },
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            10810,
            &fragmenting(),
        )
        .unwrap();

        assert!(!DIALER_TAG.starts_with(SELECTABLE_TAG_PREFIX));
        let outbounds = config["outbounds"].as_array().unwrap();
        for (index, member) in outbounds.iter().take(2).enumerate() {
            assert_eq!(
                member["streamSettings"]["sockopt"]["dialerProxy"], DIALER_TAG,
                "member {index} was left dialing directly"
            );
        }
        assert_eq!(outbounds[2]["tag"], DIALER_TAG);
        // One fragmenter for the whole pool, not one per member.
        assert_eq!(
            outbounds
                .iter()
                .filter(|out| out["tag"] == DIALER_TAG)
                .count(),
            1
        );
        assert_eq!(config["routing"]["balancers"][0]["selector"], json!(["s-"]));
    }

    /// Shadowsocks, socks and http outbounds carry no `streamSettings` of their
    /// own, so the dialer has to bring the object into being.
    #[test]
    fn a_plain_protocol_gains_stream_settings_only_to_hold_the_dialer() {
        let plain = generate(
            &socks_server(),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &ResolvedCore::default(),
        );
        assert_eq!(plain["outbounds"][0]["streamSettings"], json!(null));

        let fragmented = generate(
            &socks_server(),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &fragmenting(),
        );
        assert_eq!(
            fragmented["outbounds"][0]["streamSettings"],
            json!({ "sockopt": { "dialerProxy": DIALER_TAG } })
        );
    }

    #[test]
    fn mux_rides_on_the_proxy_outbounds_and_nowhere_else() {
        let core = from_profile(CoreOptions {
            mux: MuxOptions {
                enabled: Some(true),
                concurrency: Some(8),
                xudp_proxy_udp_443: Some(XudpMode::Skip),
                ..MuxOptions::default()
            },
            ..CoreOptions::default()
        });

        let config = generate(&socks_server(), Ipv4Addr::LOCALHOST, 10808, 10809, &core);

        let outbounds = config["outbounds"].as_array().unwrap();
        assert_eq!(
            outbounds[0]["mux"],
            json!({ "enabled": true, "concurrency": 8, "xudpProxyUDP443": "skip" })
        );
        // Unset knobs stay out rather than being written as the core's own
        // defaults, so the config keeps saying only what was asked for.
        assert_eq!(outbounds[0]["mux"]["xudpConcurrency"], json!(null));
        assert_eq!(outbounds[1]["mux"], json!(null));
        assert_eq!(outbounds[2]["mux"], json!(null));
    }

    #[test]
    fn sniffing_says_route_only_just_when_it_was_asked_for() {
        let quiet = generate(
            &socks_server(),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &ResolvedCore::default(),
        );
        assert_eq!(quiet["inbounds"][0]["sniffing"]["routeOnly"], json!(null));

        let core = from_profile(CoreOptions {
            sniffing: SniffingOptions {
                enabled: Some(true),
                dest_override: Some(vec![
                    DestOverride::Http,
                    DestOverride::Tls,
                    DestOverride::Quic,
                ]),
                route_only: Some(true),
            },
            ..CoreOptions::default()
        });
        let config = generate(&socks_server(), Ipv4Addr::LOCALHOST, 10808, 10809, &core);

        // Both inbounds, not just the SOCKS one people test through.
        for index in 0..2 {
            let sniffing = &config["inbounds"][index]["sniffing"];
            assert_eq!(sniffing["routeOnly"], json!(true));
            assert_eq!(sniffing["destOverride"], json!(["http", "tls", "quic"]));
        }
    }

    #[test]
    fn turning_sniffing_off_reaches_both_inbounds() {
        let core = from_profile(CoreOptions {
            sniffing: SniffingOptions {
                enabled: Some(false),
                ..SniffingOptions::default()
            },
            ..CoreOptions::default()
        });

        let config = generate(&socks_server(), Ipv4Addr::LOCALHOST, 10808, 10809, &core);

        assert_eq!(config["inbounds"][0]["sniffing"]["enabled"], json!(false));
        assert_eq!(config["inbounds"][1]["sniffing"]["enabled"], json!(false));
    }

    #[test]
    fn the_local_resolver_is_asked_first_and_only_about_local_names() {
        let core = from_profile(CoreOptions {
            dns: DnsOptions {
                server: Some("https://1.1.1.1/dns-query".to_string()),
                direct_server: Some("localhost".to_string()),
                query_strategy: Some(QueryStrategy::UseIpv4),
            },
            ..CoreOptions::default()
        });

        let config = generate(&socks_server(), Ipv4Addr::LOCALHOST, 10808, 10809, &core);

        assert_eq!(
            config["dns"],
            json!({
                "servers": [
                    {
                        "address": "localhost",
                        "domains": ["geosite:private"],
                        "skipFallback": true
                    },
                    "https://1.1.1.1/dns-query"
                ],
                "queryStrategy": "UseIPv4"
            })
        );
    }

    #[test]
    fn no_dns_server_means_no_dns_block_at_all() {
        let config = generate(
            &socks_server(),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &ResolvedCore::default(),
        );

        assert_eq!(config["dns"], json!(null));
    }

    #[test]
    fn the_log_level_and_domain_strategy_reach_the_config_as_the_core_spells_them() {
        let core = from_profile(CoreOptions {
            log_level: Some(LogLevel::Silent),
            domain_strategy: Some(DomainStrategy::IpOnDemand),
            ..CoreOptions::default()
        });

        let config = generate(&socks_server(), Ipv4Addr::LOCALHOST, 10808, 10809, &core);

        assert_eq!(config["log"]["loglevel"], "none");
        assert_eq!(config["routing"]["domainStrategy"], "IPOnDemand");
    }

    #[test]
    fn a_country_sized_pool_generates_one_outbound_per_member() {
        let servers = country_sized_pool();
        let members = servers.iter().collect::<Vec<_>>();
        let config = generate_pool(
            &PoolSpec {
                members: &members,
                strategy: "leastLoad",
                expected: 6,
                probe_interval: "5m",
            },
            Ipv4Addr::new(127, 72, 14, 1),
            10808,
            10809,
            10810,
            &ResolvedCore::default(),
        )
        .unwrap();

        let outbounds = config["outbounds"].as_array().unwrap();
        // 42 members, then the two fixed outbounds every config carries.
        assert_eq!(outbounds.len(), 44);
        assert_eq!(outbounds[0]["tag"], "s-de-0");
        assert_eq!(outbounds[41]["tag"], "s-de-41");
        assert_eq!(outbounds[42]["tag"], "direct");
        assert_eq!(outbounds[43]["tag"], "block");
        // The rotation width the Connect bar defaults to, against a pool that
        // has seven times as many members: `expected` is what keeps the core
        // from pinging all 42 into rotation.
        assert_eq!(
            config["routing"]["balancers"][0]["strategy"]["settings"]["expected"],
            6
        );
        assert_eq!(config["routing"]["balancers"][0]["selector"], json!(["s-"]));
        assert_eq!(
            crate::pool::distinct_endpoints(&members),
            9,
            "the fixture must keep the many-entries-few-hosts shape it is for"
        );
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
            &ResolvedCore::default(),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(error, "duplicate Xray pool outbound tag \"s-collision\"");
    }

    #[test]
    fn both_inbounds_use_the_session_address() {
        let bind = Ipv4Addr::new(127, 72, 14, 1);
        let config = generate(
            &socks_server(),
            bind,
            10808,
            10809,
            &ResolvedCore::default(),
        );
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
                &ResolvedCore::default(),
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
            &ResolvedCore::default(),
        );
        let tls = &config["outbounds"][0]["streamSettings"]["tlsSettings"];
        // A bare string, not an array: Xray 26.x fails to decode the array form.
        assert_eq!(tls["pinnedPeerCertSha256"], json!(pin));
    }

    #[test]
    fn absent_pin_leaves_the_key_out() {
        let config = generate(
            &tls_vless(false, None),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &ResolvedCore::default(),
        );
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
            &ResolvedCore::default(),
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
            &ResolvedCore::default(),
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
            &ResolvedCore::default(),
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
            &ResolvedCore::default(),
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
            &ResolvedCore::default(),
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
        let config = generate(
            &tls_vless(false, None),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &ResolvedCore::default(),
        );
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

    /// A profile's own rules go ahead of the ones oxidom installs, so a rule the
    /// user wrote about a private address wins over the built-in one below it —
    /// which is the whole reason for carrying a block.
    #[test]
    fn a_profile_routing_block_is_spliced_ahead_of_the_generated_rules() {
        let core = with_routing(
            r#"{"rules":[
                 {"domain":["geosite:category-ads-all"],"outboundTag":"block"},
                 {"ip":["geoip:ch"],"outboundTag":"direct"}
               ]}"#,
            routing::Shape::SingleServer,
        );
        let config = generate(&socks_server(), Ipv4Addr::LOCALHOST, 10808, 10809, &core);
        let rules = config["routing"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0]["domain"][0], "geosite:category-ads-all");
        assert_eq!(rules[1]["ip"][0], "geoip:ch");
        assert_eq!(
            rules[2]["ip"][0], "geoip:private",
            "the built-in rule stays, and stays below the profile's own"
        );
    }

    /// The two binding positions survive a block: the api rule cannot move off
    /// the front — `xray api bi` hangs the moment its request falls into the
    /// balancer — and the balancer's catch-all cannot move off the back.
    #[test]
    fn a_pool_keeps_its_api_rule_first_and_its_balancer_rule_last() {
        let core = with_routing(
            r#"{"rules":[{"domain":["example.com"],"outboundTag":"block"}]}"#,
            routing::Shape::Pool,
        );
        let members = country_sized_pool();
        let refs: Vec<&Server> = members.iter().collect();
        let config = generate_pool(
            &PoolSpec {
                members: &refs,
                strategy: "leastLoad",
                expected: 0,
                probe_interval: "5m",
            },
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            10810,
            &core,
        )
        .unwrap();
        let rules = config["routing"]["rules"].as_array().unwrap();
        assert_eq!(rules[0]["inboundTag"][0], "api-in");
        assert_eq!(rules[1]["domain"][0], "example.com");
        assert_eq!(rules[2]["ip"][0], "geoip:private");
        assert_eq!(rules.last().unwrap()["balancerTag"], BALANCER_TAG);
    }

    /// Anything in the block that is not a rule lands on the routing object as
    /// written: the block is carried, not modelled.
    #[test]
    fn the_rest_of_a_routing_block_reaches_the_config_as_written() {
        let core = with_routing(
            r#"{"domainMatcher":"hybrid","rules":[]}"#,
            routing::Shape::SingleServer,
        );
        let config = generate(&socks_server(), Ipv4Addr::LOCALHOST, 10808, 10809, &core);
        assert_eq!(config["routing"]["domainMatcher"], "hybrid");
        assert_eq!(
            config["routing"]["domainStrategy"], "IPIfNonMatch",
            "the [core] setting still owns the key the block may not spell"
        );
    }

    /// A probe measures a server, not a route. `CoreOptions::resolve` is the
    /// only thing a probe builds its core from, and it leaves `routing` unset —
    /// a rule that reached one could send the measurement out direct and report
    /// a dead server as fast.
    #[test]
    fn a_resolved_core_never_carries_routing_so_a_probe_cannot_inherit_one() {
        let resolved = CoreOptions::resolve(
            &CoreOptions {
                log_level: Some(LogLevel::Debug),
                ..CoreOptions::default()
            },
            &CoreOptions::default(),
        );
        assert!(resolved.routing.is_none());
        let config = generate(
            &socks_server(),
            Ipv4Addr::LOCALHOST,
            10808,
            10809,
            &resolved,
        );
        let rules = config["routing"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["ip"][0], "geoip:private");
    }

    fn with_routing(raw: &str, shape: routing::Shape) -> ResolvedCore {
        ResolvedCore {
            routing: Some(routing::validate(raw, shape).unwrap()),
            ..ResolvedCore::default()
        }
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
                generate(server, bind, 10808, 10809, &ResolvedCore::default()),
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
                &ResolvedCore::default(),
            )
            .expect("sample pool should generate"),
        ));
        // A pool the size a country rule produces. Two members proved the shape;
        // this proves the core still parses one when the shape is repeated 42
        // times and `expected` is well below the member count.
        let big = country_sized_pool();
        let big_members = big.iter().collect::<Vec<_>>();
        configs.push((
            "pool-42".to_string(),
            generate_pool(
                &PoolSpec {
                    members: &big_members,
                    strategy: "leastLoad",
                    expected: 6,
                    probe_interval: "5m",
                },
                Ipv4Addr::new(127, 72, 14, 1),
                10808,
                10809,
                10810,
                &ResolvedCore::default(),
            )
            .expect("a country-sized pool should generate"),
        ));

        // Phase 6. Every one of these was accepted by 26.3.27 by hand before
        // being written down — and `-test` accepting a config proves only that
        // it parses: the core takes an unknown key, an unknown `loglevel` and a
        // dangling `dialerProxy` without a word. What these cases pin is the
        // half the core *does* police: `destOverride`, `xudpProxyUDP443`,
        // `noises[].type`, `sockopt.domainStrategy`, a zero-minimum range, and
        // whether `geosite:private` resolves at all.
        let everything = CoreOptions {
            pool_probe_url: None,
            log_level: Some(LogLevel::Debug),
            domain_strategy: Some(DomainStrategy::IpOnDemand),
            sniffing: SniffingOptions {
                enabled: Some(true),
                dest_override: Some(vec![
                    DestOverride::Http,
                    DestOverride::Tls,
                    DestOverride::Quic,
                ]),
                route_only: Some(true),
            },
            mux: MuxOptions {
                enabled: Some(true),
                concurrency: Some(8),
                xudp_concurrency: Some(16),
                xudp_proxy_udp_443: Some(XudpMode::Reject),
            },
            fragment: FragmentOptions {
                enabled: Some(true),
                packets: Some("tlshello".to_string()),
                length: Some("100-200".to_string()),
                interval: Some("10-20".to_string()),
            },
            noises: Some(vec![
                Noise {
                    kind: NoiseKind::Rand,
                    packet: "10-20".to_string(),
                    delay: "10-16".to_string(),
                },
                Noise {
                    kind: NoiseKind::Base64,
                    packet: "aGVsbG8=".to_string(),
                    delay: "5".to_string(),
                },
            ]),
            dns: DnsOptions {
                server: Some("https://1.1.1.1/dns-query".to_string()),
                direct_server: Some("localhost".to_string()),
                query_strategy: Some(QueryStrategy::UseIpv4),
            },
        };
        let everything = CoreOptions::resolve(&CoreOptions::default(), &everything);

        // A plain protocol, which has no `streamSettings` of its own until the
        // dialer gives it one, and a vless one, which already has some.
        configs.push((
            "core-plain".to_string(),
            generate(
                &socks_server(),
                Ipv4Addr::LOCALHOST,
                10808,
                10809,
                &everything,
            ),
        ));
        configs.push((
            "core-vless".to_string(),
            generate(&servers[2], Ipv4Addr::LOCALHOST, 10808, 10809, &everything),
        ));
        // The one place phase 5 and phase 6 meet: `mux` and `dialerProxy` land
        // on every member, and the balancer selector must still not resolve to
        // the dialer.
        configs.push((
            "core-pool".to_string(),
            generate_pool(
                &PoolSpec {
                    members: &big_members,
                    strategy: "leastLoad",
                    expected: 6,
                    probe_interval: "5m",
                },
                Ipv4Addr::new(127, 72, 14, 1),
                10808,
                10809,
                10810,
                &everything,
            )
            .expect("a fragmented pool should generate"),
        ));
        // Sniffing off with the log silenced: the opposite corner of the same
        // settings, since `enabled: false` travels a different branch.
        let quiet = CoreOptions::resolve(
            &CoreOptions::default(),
            &CoreOptions {
                log_level: Some(LogLevel::Silent),
                domain_strategy: Some(DomainStrategy::AsIs),
                sniffing: SniffingOptions {
                    enabled: Some(false),
                    ..SniffingOptions::default()
                },
                ..CoreOptions::default()
            },
        );
        configs.push((
            "core-quiet".to_string(),
            generate(&socks_server(), Ipv4Addr::LOCALHOST, 10808, 10809, &quiet),
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
