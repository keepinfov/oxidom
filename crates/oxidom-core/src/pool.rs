//! Pure pool selection shared by profile activation and the GUI filter.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::{OutboundSpec, Server, Subscription};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Strategy {
    /// Rotates across the nodes the observatory still reaches. The default
    /// because it is the only strategy that both spreads traffic — the point
    /// of a pool — and drops dead nodes.
    #[default]
    LeastLoad,
    /// Rotates across **every** member, reachable or not. Measured on Xray
    /// 26.3.27: with one live and one unreachable node, half of twelve
    /// requests went into the unreachable one. Kept because an even sweep is
    /// sometimes what is wanted, but it is not a failover.
    RoundRobin,
    /// Same blindness as `roundRobin`, without the even sweep.
    Random,
    /// Concentrates everything on the single fastest node. The opposite of
    /// spreading activity, and offered for "my server got slow, move me".
    LeastPing,
}

impl Strategy {
    pub fn as_xray(&self) -> &'static str {
        match self {
            Strategy::RoundRobin => "roundRobin",
            Strategy::Random => "random",
            Strategy::LeastPing => "leastPing",
            Strategy::LeastLoad => "leastLoad",
        }
    }

    /// Whether the core keeps unreachable members in the rotation. The UI has
    /// to say this out loud: a pool that quietly swallows a third of its
    /// requests looks like an unreliable connection, not like a setting.
    pub fn keeps_dead_nodes(&self) -> bool {
        matches!(self, Strategy::RoundRobin | Strategy::Random)
    }

    /// Whether the balancer settles on one node, so naming a current exit is
    /// meaningful rather than a snapshot of a rotation.
    pub fn picks_one(&self) -> bool {
        matches!(self, Strategy::LeastPing)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PoolQuery {
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default)]
    pub subscriptions: Vec<String>,
    #[serde(default)]
    pub countries: Vec<String>,
    #[serde(default)]
    pub protocols: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub max: usize,
    /// How many members `leastLoad` keeps in rotation. Zero means the whole
    /// pool, which the core reads as "rotate across everything still
    /// reachable" — verified on 26.3.27, where an `expected` above the live
    /// count returns exactly the live ones.
    #[serde(default)]
    pub expected: usize,
    #[serde(default)]
    pub probe_interval: String,
}

impl PoolQuery {
    pub fn probe_interval_or_default(&self) -> &str {
        if self.probe_interval.is_empty() {
            "5m"
        } else {
            &self.probe_interval
        }
    }

    /// `expected` resolved against the pool that was actually built.
    pub fn expected_or_all(&self, members: usize) -> usize {
        if self.expected == 0 || self.expected > members {
            members
        } else {
            self.expected
        }
    }
}

/// Resolve a pool without I/O while preserving group and server order.
pub fn resolve<'a>(query: &PoolQuery, groups: &'a [Subscription]) -> Result<Vec<&'a Server>> {
    let mut matches = Vec::new();

    for group in groups {
        if !group_matches(query, group) {
            continue;
        }

        for server in &group.servers {
            if !server_matches(query, server) {
                continue;
            }
            // Silent on purpose: this runs on every GUI filter keystroke and on
            // every daemon poll, so a `warn!` here would be a log flood. The one
            // place a user can act on it — bringing a pool profile up — says so
            // once, by comparing the resolved list against the group contents.
            if matches!(&server.spec, OutboundSpec::XrayProfile { .. }) {
                continue;
            }
            matches.push(server);
        }
    }

    if query.max != 0 {
        matches.truncate(query.max);
    }
    if matches.is_empty() {
        bail!("{}", empty_pool_message(query));
    }
    Ok(matches)
}

/// Composite profiles that match the query but cannot become pool outbounds.
///
/// `resolve` stays silent because the GUI calls it continuously. Activation
/// uses this companion once to make the omission visible in the daemon log.
pub fn excluded_composites<'a>(query: &PoolQuery, groups: &'a [Subscription]) -> Vec<&'a Server> {
    groups
        .iter()
        .filter(|group| group_matches(query, group))
        .flat_map(|group| group.servers.iter())
        .filter(|server| {
            server_matches(query, server)
                && matches!(&server.spec, OutboundSpec::XrayProfile { .. })
        })
        .collect()
}

fn group_matches(query: &PoolQuery, group: &Subscription) -> bool {
    query.subscriptions.is_empty()
        || query
            .subscriptions
            .iter()
            .any(|selection| selection == &group.id || selection.eq_ignore_ascii_case(&group.name))
}

fn server_matches(query: &PoolQuery, server: &Server) -> bool {
    if !query.countries.is_empty()
        && !server.country.as_ref().is_some_and(|country| {
            query
                .countries
                .iter()
                .any(|selection| selection.eq_ignore_ascii_case(country))
        })
    {
        return false;
    }
    if !query.protocols.is_empty()
        && !query
            .protocols
            .iter()
            .any(|selection| selection.eq_ignore_ascii_case(server.protocol.as_str()))
    {
        return false;
    }
    // Exclusions deliberately do not use `handle::resolve`: substring
    // matching here could silently remove many members from a pool.
    !query
        .exclude
        .iter()
        .any(|selection| selection == &server.id || server.alias.as_ref() == Some(selection))
}

fn empty_pool_message(query: &PoolQuery) -> String {
    let mut filters = Vec::new();
    if !query.subscriptions.is_empty() {
        filters.push(format!("subscriptions: {}", query.subscriptions.join(", ")));
    }
    if !query.countries.is_empty() {
        filters.push(format!("countries: {}", query.countries.join(", ")));
    }
    if !query.protocols.is_empty() {
        filters.push(format!("protocols: {}", query.protocols.join(", ")));
    }
    if !query.exclude.is_empty() {
        filters.push(format!("exclude: {}", query.exclude.join(", ")));
    }
    if filters.is_empty() {
        filters.push("no filters".to_string());
    }
    format!("no server matches the pool query ({})", filters.join("; "))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{PoolQuery, Strategy, resolve};
    use crate::model::{OutboundSpec, Protocol, Server, Subscription};

    fn server(id: &str, protocol: Protocol, country: Option<&str>, alias: Option<&str>) -> Server {
        Server {
            id: id.to_string(),
            name: format!("Server {id}"),
            protocol,
            address: format!("{id}.example"),
            port: 1080,
            transport_label: protocol.as_str().to_string(),
            country: country.map(str::to_string),
            spec: OutboundSpec::Socks {
                username: None,
                password: None,
            },
            link: None,
            alias: alias.map(str::to_string),
            latency_ms: None,
        }
    }

    fn composite(id: &str) -> Server {
        Server {
            id: id.to_string(),
            name: "Provider balance".to_string(),
            protocol: Protocol::Vless,
            address: "profile.example".to_string(),
            port: 443,
            transport_label: "xray + balanced".to_string(),
            country: Some("ch".to_string()),
            spec: OutboundSpec::XrayProfile {
                proxy_outbounds: vec![json!({"tag": "proxy", "protocol": "vless"})],
                balancers: vec![json!({"tag": "balance", "selector": ["proxy"]})],
                burst_observatory: None,
                balancer_tag: "balance".to_string(),
            },
            link: None,
            alias: Some("provider-balance".to_string()),
            latency_ms: None,
        }
    }

    fn group(id: &str, name: &str, servers: Vec<Server>) -> Subscription {
        let mut group =
            Subscription::new(format!("https://{id}.example/sub"), Some(name.to_string()));
        group.id = id.to_string();
        group.servers = servers;
        group
    }

    fn ids(servers: Vec<&Server>) -> Vec<&str> {
        servers
            .into_iter()
            .map(|server| server.id.as_str())
            .collect()
    }

    #[test]
    fn defaults_rotate_across_live_nodes_with_a_five_minute_probe() {
        let query = PoolQuery::default();
        // Measured, not assumed: `roundRobin` keeps unreachable members in the
        // rotation, so it cannot be the default for a feature whose purpose is
        // to keep working while spreading traffic.
        assert_eq!(query.strategy, Strategy::LeastLoad);
        assert_eq!(query.strategy.as_xray(), "leastLoad");
        assert!(!query.strategy.keeps_dead_nodes());
        assert!(!query.strategy.picks_one());
        assert_eq!(query.probe_interval_or_default(), "5m");

        assert!(Strategy::RoundRobin.keeps_dead_nodes());
        assert!(Strategy::Random.keeps_dead_nodes());
        assert!(Strategy::LeastPing.picks_one());

        let explicit = PoolQuery {
            probe_interval: "30s".to_string(),
            ..PoolQuery::default()
        };
        assert_eq!(explicit.probe_interval_or_default(), "30s");
    }

    #[test]
    fn expected_falls_back_to_the_whole_pool() {
        let unset = PoolQuery::default();
        assert_eq!(unset.expected_or_all(6), 6);

        let narrow = PoolQuery {
            expected: 2,
            ..PoolQuery::default()
        };
        assert_eq!(narrow.expected_or_all(6), 2);

        // Asking for more than the pool holds is not an error: the core
        // answers with whatever is live, which is exactly the intent.
        let wide = PoolQuery {
            expected: 40,
            ..PoolQuery::default()
        };
        assert_eq!(wide.expected_or_all(6), 6);
    }

    #[test]
    fn resolution_preserves_group_and_server_order() {
        let groups = vec![
            group(
                "first",
                "First",
                vec![
                    server("z", Protocol::Socks, Some("ch"), None),
                    server("a", Protocol::Vless, Some("de"), None),
                ],
            ),
            group(
                "second",
                "Second",
                vec![server("m", Protocol::Trojan, Some("nl"), None)],
            ),
        ];

        assert_eq!(
            ids(resolve(&PoolQuery::default(), &groups).unwrap()),
            ["z", "a", "m"]
        );
    }

    #[test]
    fn max_truncates_after_stable_ordering() {
        let groups = vec![group(
            "all",
            "All",
            vec![
                server("one", Protocol::Socks, Some("ch"), None),
                server("two", Protocol::Socks, Some("ch"), None),
                server("three", Protocol::Socks, Some("ch"), None),
            ],
        )];
        let query = PoolQuery {
            max: 2,
            ..PoolQuery::default()
        };

        assert_eq!(ids(resolve(&query, &groups).unwrap()), ["one", "two"]);
    }

    #[test]
    fn subscription_filter_matches_exact_id_or_case_insensitive_name() {
        let groups = vec![
            group(
                "alpha-id",
                "Alpha",
                vec![server("alpha", Protocol::Socks, None, None)],
            ),
            group(
                "beta-id",
                "Beta Group",
                vec![server("beta", Protocol::Socks, None, None)],
            ),
        ];
        let by_id = PoolQuery {
            subscriptions: vec!["alpha-id".to_string()],
            ..PoolQuery::default()
        };
        let by_name = PoolQuery {
            subscriptions: vec!["BETA GROUP".to_string()],
            ..PoolQuery::default()
        };

        assert_eq!(ids(resolve(&by_id, &groups).unwrap()), ["alpha"]);
        assert_eq!(ids(resolve(&by_name, &groups).unwrap()), ["beta"]);
    }

    #[test]
    fn country_filter_is_case_insensitive() {
        let groups = vec![group(
            "all",
            "All",
            vec![
                server("ch", Protocol::Socks, Some("CH"), None),
                server("de", Protocol::Socks, Some("de"), None),
            ],
        )];
        let query = PoolQuery {
            countries: vec!["ch".to_string()],
            ..PoolQuery::default()
        };

        assert_eq!(ids(resolve(&query, &groups).unwrap()), ["ch"]);
    }

    #[test]
    fn protocol_filter_uses_protocol_names_case_insensitively() {
        let groups = vec![group(
            "all",
            "All",
            vec![
                server("vless", Protocol::Vless, None, None),
                server("trojan", Protocol::Trojan, None, None),
            ],
        )];
        let query = PoolQuery {
            protocols: vec!["VLESS".to_string()],
            ..PoolQuery::default()
        };

        assert_eq!(ids(resolve(&query, &groups).unwrap()), ["vless"]);
    }

    #[test]
    fn exclusions_match_only_an_exact_alias_or_id() {
        let groups = vec![group(
            "all",
            "All",
            vec![
                server("id-one", Protocol::Socks, None, Some("ch-one")),
                server("id-two", Protocol::Socks, None, Some("ch-two")),
                server("id-three", Protocol::Socks, None, Some("de-three")),
            ],
        )];
        let substring = PoolQuery {
            exclude: vec!["ch".to_string()],
            ..PoolQuery::default()
        };
        let exact = PoolQuery {
            exclude: vec!["ch-one".to_string(), "id-three".to_string()],
            ..PoolQuery::default()
        };

        assert_eq!(
            ids(resolve(&substring, &groups).unwrap()),
            ["id-one", "id-two", "id-three"]
        );
        assert_eq!(ids(resolve(&exact, &groups).unwrap()), ["id-two"]);
    }

    #[test]
    fn a_server_without_country_does_not_match_a_country_filter() {
        let groups = vec![group(
            "all",
            "All",
            vec![
                server("unknown", Protocol::Socks, None, None),
                server("known", Protocol::Socks, Some("ch"), None),
            ],
        )];
        let query = PoolQuery {
            countries: vec!["ch".to_string()],
            ..PoolQuery::default()
        };

        assert_eq!(ids(resolve(&query, &groups).unwrap()), ["known"]);
    }

    #[test]
    fn composite_xray_profiles_are_never_pool_members() {
        let groups = vec![group(
            "all",
            "All",
            vec![
                composite("composite"),
                server("plain", Protocol::Vless, Some("ch"), None),
            ],
        )];

        assert_eq!(
            ids(resolve(&PoolQuery::default(), &groups).unwrap()),
            ["plain"]
        );
    }

    #[test]
    fn an_empty_result_names_the_active_filters() {
        let groups = vec![group(
            "all",
            "All",
            vec![server("de-trojan", Protocol::Trojan, Some("de"), None)],
        )];
        let query = PoolQuery {
            countries: vec!["ch".to_string(), "de".to_string()],
            protocols: vec!["vless".to_string()],
            ..PoolQuery::default()
        };

        assert_eq!(
            resolve(&query, &groups).unwrap_err().to_string(),
            "no server matches the pool query (countries: ch, de; protocols: vless)"
        );
    }
}
