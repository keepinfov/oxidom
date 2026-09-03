//! Pure pool selection shared by profile activation and the GUI filter.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::{OutboundSpec, Server, Subscription};

/// How many nodes a pool built through the UI rotates over unless told
/// otherwise.
///
/// Applied where a `PoolQuery` is *constructed*, never in `Deserialize` and
/// never in [`PoolQuery::expected_or_all`]: `expected = 0` is a written-down
/// answer meaning "all of them", and giving it a new meaning would silently
/// retune every profile already on disk. Six because a pool exists to spread
/// activity over exit addresses while staying steerable — rotating over all
/// forty-two entries of a subscription buys no more spread than its handful of
/// distinct hosts, and costs an observatory ping per entry.
pub const DEFAULT_POOL_ROTATION: usize = 6;

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

/// What a pool is made of: either an explicit list of servers, or a rule that
/// keeps matching as subscriptions change.
///
/// The distinction is the user's, not an implementation detail. A rule cannot
/// be looked at — to know what is in "Europe" you have to run it in your head
/// against every subscription — and it *grows*: a server added by tomorrow's
/// refresh joins on its own. A list can be counted, and it never gains a member
/// without being edited. Losing one is fine and expected; that is just a server
/// going away.
///
/// Freezing a list as "every filter off, plus every other server excluded"
/// looks equivalent and is not: a server that did not exist when the list was
/// frozen is in nobody's exclusions, so it would silently join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
    List,
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PoolQuery {
    /// What the user calls this pool. Carried so `oxidom status` can say
    /// `pool "Europe"` instead of six anonymous nodes; it names the pool and
    /// never selects anything, so renaming one does not make a running session
    /// stale (see `engine::pool_fingerprint`, which hashes resolved members).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default)]
    pub strategy: Strategy,
    /// Exact handles — server id or alias — making up a frozen list. Non-empty
    /// means this pool *is* these servers: the filters below are not consulted,
    /// and [`Profile::validate`](crate::profile::Profile::validate) rejects a
    /// pool that sets both rather than quietly ignoring half of it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
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
    pub fn kind(&self) -> PoolKind {
        if self.members.is_empty() {
            PoolKind::Rule
        } else {
            PoolKind::List
        }
    }

    /// Whether any filter is set. Only meaningful together with [`Self::kind`]:
    /// a list that also carries filters is a contradiction, not a refinement.
    pub fn has_filters(&self) -> bool {
        !self.subscriptions.is_empty()
            || !self.countries.is_empty()
            || !self.protocols.is_empty()
            || !self.exclude.is_empty()
    }

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

/// Resolve a pool without I/O.
///
/// A rule preserves group and server order; a list preserves the order the user
/// arranged it in, which is why `max` truncating it is still meaningful.
pub fn resolve<'a>(query: &PoolQuery, groups: &'a [Subscription]) -> Result<Vec<&'a Server>> {
    let mut matches: Vec<&Server> = Vec::new();

    if query.kind() == PoolKind::List {
        for handle in &query.members {
            let Some(server) = find_member(handle, groups) else {
                // Silent for the same reason as below. `missing_members` is the
                // companion that says it once, where a user can act on it.
                continue;
            };
            if matches!(&server.spec, OutboundSpec::XrayProfile { .. }) {
                continue;
            }
            // A handle can be listed twice — an alias and the id behind it are
            // two spellings of one server, and two outbounds would collide on
            // the `s-<handle>` tag.
            if matches.iter().any(|kept| kept.id == server.id) {
                continue;
            }
            matches.push(server);
        }
    } else {
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
    }

    if query.max != 0 {
        matches.truncate(query.max);
    }
    if matches.is_empty() {
        bail!("{}", empty_pool_message(query));
    }
    Ok(matches)
}

/// What the caller knows about a candidate. Ordering only — nothing here
/// decides membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Known {
    /// Answered a probe, in milliseconds.
    Reachable(u32),
    /// Never measured. Ranked ahead of [`Known::Unreachable`] because a node
    /// nobody asked might work, while one that already refused is the worst
    /// possible opening bid.
    Unmeasured,
    Unreachable,
}

impl Known {
    fn order(self) -> (u8, u32) {
        match self {
            Self::Reachable(ms) => (0, ms),
            Self::Unmeasured => (1, 0),
            Self::Unreachable => (2, 0),
        }
    }
}

/// The endpoint a server actually exits through, which is what a pool spreads
/// over. Two subscription entries sharing one of these are one exit IP, however
/// differently they are spelled.
fn endpoint(server: &Server) -> (&str, u16) {
    (server.address.as_str(), server.port)
}

/// Resolve a pool for activation: the same membership as [`resolve`], ordered so
/// that the pool is worth having, and only then cut to `max`.
///
/// Two things make the order matter, and neither is visible to the pure
/// `resolve` the GUI calls on every keystroke:
///
/// - **`max` truncates.** On a real subscription 26 of 42 German entries share
///   one `address:port`; taking "the first 6" took six spellings of one host,
///   which is one exit IP — the opposite of what a pool is for. So distinct
///   endpoints are dealt out one apiece before any endpoint gets a second.
/// - **The first member is the pool's opening exit.** Until `burstObservatory`
///   reports, the balancer routes to the first outbound in config order (see
///   `AGENTS.md`), so putting a known-reachable node there is the difference
///   between connecting at once and waiting out the confirmation window.
///
/// Membership is unchanged — this only decides order, and truncation after it.
pub fn resolve_ranked<'a>(
    query: &PoolQuery,
    groups: &'a [Subscription],
    known: impl Fn(&Server) -> Known,
) -> Result<Vec<&'a Server>> {
    let mut uncapped = query.clone();
    uncapped.max = 0;
    let matches = resolve(&uncapped, groups)?;

    // Group by exit endpoint, keeping first-seen order so the result stays
    // deterministic for two candidates nothing is known about.
    let mut buckets: Vec<Vec<&'a Server>> = Vec::new();
    let mut seen: Vec<(&str, u16)> = Vec::new();
    for server in matches {
        match seen.iter().position(|known| *known == endpoint(server)) {
            Some(index) => buckets[index].push(server),
            None => {
                seen.push(endpoint(server));
                buckets.push(vec![server]);
            }
        }
    }
    for bucket in &mut buckets {
        bucket.sort_by_key(|server| known(server).order());
    }
    // Best host first, so member #1 is the best opening exit available.
    buckets.sort_by_key(|bucket| known(bucket[0]).order());

    let mut ranked = Vec::new();
    for round in 0..buckets.iter().map(Vec::len).max().unwrap_or(0) {
        for bucket in &buckets {
            if let Some(server) = bucket.get(round) {
                ranked.push(*server);
            }
        }
    }
    if query.max != 0 {
        ranked.truncate(query.max);
    }
    Ok(ranked)
}

/// How many distinct exit endpoints a resolved pool actually covers.
///
/// A pool of 42 entries over 9 hosts spreads across 9 addresses, not 42, and
/// saying "42 nodes" without this is the pool's least honest number.
pub fn distinct_endpoints(members: &[&Server]) -> usize {
    let mut seen: Vec<(&str, u16)> = Vec::new();
    for server in members {
        if !seen.contains(&endpoint(server)) {
            seen.push(endpoint(server));
        }
    }
    seen.len()
}

/// Composite profiles that the pool names but cannot turn into outbounds.
///
/// `resolve` stays silent because the GUI calls it continuously. Activation
/// uses this companion once to make the omission visible in the daemon log.
pub fn excluded_composites<'a>(query: &PoolQuery, groups: &'a [Subscription]) -> Vec<&'a Server> {
    if query.kind() == PoolKind::List {
        return query
            .members
            .iter()
            .filter_map(|handle| find_member(handle, groups))
            .filter(|server| matches!(&server.spec, OutboundSpec::XrayProfile { .. }))
            .collect();
    }
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

/// Handles a frozen list names that no subscription holds any more.
///
/// A list is expected to shrink — that is a server going away, not a broken
/// profile — so this is reported once at activation rather than failing the
/// pool. Empty for a rule, which has no handles to lose.
pub fn missing_members<'a>(query: &'a PoolQuery, groups: &[Subscription]) -> Vec<&'a str> {
    if query.kind() == PoolKind::Rule {
        return Vec::new();
    }
    query
        .members
        .iter()
        .filter(|handle| find_member(handle, groups).is_none())
        .map(String::as_str)
        .collect()
}

/// Exact id or alias, never a substring: a list is what the user picked, and a
/// prefix match could quietly enrol a server they never chose.
fn find_member<'a>(handle: &str, groups: &'a [Subscription]) -> Option<&'a Server> {
    groups
        .iter()
        .flat_map(|group| group.servers.iter())
        .find(|server| server.id == handle || server.alias.as_deref() == Some(handle))
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
    if query.kind() == PoolKind::List {
        return format!(
            "no server is left of the pool's list ({})",
            query.members.join(", ")
        );
    }
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

    use super::{
        Known, PoolKind, PoolQuery, Strategy, distinct_endpoints, excluded_composites,
        missing_members, resolve, resolve_ranked,
    };
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
            outbound_patch: None,
            overrides: None,
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
            outbound_patch: None,
            overrides: None,
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

    /// A server pinned to a shared exit endpoint, as a provider's duplicate
    /// entries are.
    fn at(id: &str, host: &str) -> Server {
        let mut server = server(id, Protocol::Vless, Some("de"), Some(id));
        server.address = host.to_string();
        server
    }

    /// The German set as it actually arrives: many entries, few hosts, and the
    /// live ones buried.
    fn duplicated_hosts() -> Vec<Subscription> {
        vec![group(
            "main",
            "Main",
            vec![
                at("dead-1", "31.12.75.21"),
                at("dead-2", "31.12.75.21"),
                at("dead-3", "31.12.75.21"),
                at("unknown-1", "194.93.0.28"),
                at("unknown-2", "194.93.0.28"),
                at("live-1", "meadow.example"),
                at("live-2", "de10.example"),
            ],
        )]
    }

    fn known_for(server: &Server) -> Known {
        match server.id.as_str() {
            "live-1" => Known::Reachable(333),
            "live-2" => Known::Reachable(336),
            id if id.starts_with("dead") => Known::Unreachable,
            _ => Known::Unmeasured,
        }
    }

    #[test]
    fn ranking_deals_out_distinct_hosts_before_a_second_entry_of_any_host() {
        let groups = duplicated_hosts();
        let query = PoolQuery {
            countries: vec!["de".to_string()],
            ..PoolQuery::default()
        };

        // Plain resolve keeps subscription order, so `max` would take three
        // spellings of one dead host — one exit IP, and a dead one.
        let plain = PoolQuery {
            max: 3,
            ..query.clone()
        };
        assert_eq!(
            ids(resolve(&plain, &groups).unwrap()),
            ["dead-1", "dead-2", "dead-3"]
        );

        // Ranked: the reachable hosts open, then the unmeasured host, then the
        // dead one, and no host repeats until every host has been dealt.
        let ranked = resolve_ranked(&query, &groups, known_for).unwrap();
        assert_eq!(
            ids(ranked),
            [
                "live-1",
                "live-2",
                "unknown-1",
                "dead-1",
                "unknown-2",
                "dead-2",
                "dead-3"
            ]
        );
    }

    #[test]
    fn a_capped_ranked_pool_spends_its_budget_on_different_hosts() {
        let groups = duplicated_hosts();
        let query = PoolQuery {
            countries: vec!["de".to_string()],
            max: 4,
            ..PoolQuery::default()
        };
        let ranked = resolve_ranked(&query, &groups, known_for).unwrap();
        assert_eq!(
            ids(ranked.clone()),
            ["live-1", "live-2", "unknown-1", "dead-1"]
        );
        // Four members, four exit addresses. The whole point of the pool.
        assert_eq!(distinct_endpoints(&ranked), 4);
    }

    #[test]
    fn distinct_endpoints_counts_hosts_and_not_entries() {
        let groups = duplicated_hosts();
        let query = PoolQuery {
            countries: vec!["de".to_string()],
            ..PoolQuery::default()
        };
        let members = resolve(&query, &groups).unwrap();
        assert_eq!(members.len(), 7);
        // Seven entries, four addresses: "7 nodes" alone would overstate the
        // spread by nearly two to one.
        assert_eq!(distinct_endpoints(&members), 4);
    }

    #[test]
    fn ranking_never_changes_who_is_in_the_pool() {
        let groups = duplicated_hosts();
        let query = PoolQuery {
            countries: vec!["de".to_string()],
            ..PoolQuery::default()
        };
        let mut plain = ids(resolve(&query, &groups).unwrap());
        let mut ranked = ids(resolve_ranked(&query, &groups, known_for).unwrap());
        plain.sort_unstable();
        ranked.sort_unstable();
        assert_eq!(plain, ranked);
    }

    #[test]
    fn a_list_keeps_its_own_hosts_dealt_the_same_way() {
        let groups = duplicated_hosts();
        // A list is what the user picked, so ranking may reorder it but must
        // not drop anyone — losing a named member is `missing_members`' job.
        let query = PoolQuery {
            members: vec![
                "dead-1".to_string(),
                "dead-2".to_string(),
                "live-1".to_string(),
            ],
            ..PoolQuery::default()
        };
        assert_eq!(
            ids(resolve_ranked(&query, &groups, known_for).unwrap()),
            ["live-1", "dead-1", "dead-2"]
        );
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
    fn a_listed_pool_is_exactly_its_members_in_the_order_they_were_listed() {
        let groups = vec![
            group(
                "main",
                "Main",
                vec![
                    server("id-a", Protocol::Vless, Some("de"), Some("berlin")),
                    server("id-b", Protocol::Trojan, Some("ch"), None),
                ],
            ),
            group(
                "backup",
                "Backup",
                vec![server("id-c", Protocol::Socks, Some("nl"), None)],
            ),
        ];
        // Handles are ids or aliases, and the pool follows the user's order
        // rather than the subscriptions' — a list is a list.
        let query = PoolQuery {
            members: vec![
                "id-c".to_string(),
                "berlin".to_string(),
                // The same server spelled twice: one outbound, not two, or the
                // `s-<handle>` tags would collide.
                "id-a".to_string(),
                // Gone from every subscription: dropped, not fatal.
                "id-vanished".to_string(),
            ],
            ..PoolQuery::default()
        };

        assert_eq!(query.kind(), PoolKind::List);
        assert_eq!(ids(resolve(&query, &groups).unwrap()), ["id-c", "id-a"]);
        assert_eq!(missing_members(&query, &groups), ["id-vanished"]);
        // A rule has no handles to lose.
        assert!(missing_members(&PoolQuery::default(), &groups).is_empty());
    }

    #[test]
    fn a_list_ignores_the_filters_and_a_rule_ignores_nothing() {
        let groups = vec![group(
            "main",
            "Main",
            vec![
                server("de", Protocol::Vless, Some("de"), None),
                server("ch", Protocol::Vless, Some("ch"), None),
            ],
        )];
        // `Profile::validate` rejects this combination outright; `resolve` is
        // the pure function underneath and simply lets the list win, so a
        // config that slipped through cannot half-apply.
        let contradictory = PoolQuery {
            members: vec!["de".to_string()],
            countries: vec!["ch".to_string()],
            ..PoolQuery::default()
        };
        assert!(contradictory.has_filters());
        assert_eq!(ids(resolve(&contradictory, &groups).unwrap()), ["de"]);

        let rule = PoolQuery {
            countries: vec!["ch".to_string()],
            ..PoolQuery::default()
        };
        assert_eq!(rule.kind(), PoolKind::Rule);
        assert!(!PoolQuery::default().has_filters());
        assert_eq!(ids(resolve(&rule, &groups).unwrap()), ["ch"]);
    }

    #[test]
    fn a_listed_composite_is_skipped_like_a_matched_one() {
        let groups = vec![group(
            "all",
            "All",
            vec![
                composite("composite"),
                server("plain", Protocol::Vless, Some("ch"), None),
            ],
        )];
        let query = PoolQuery {
            members: vec!["composite".to_string(), "plain".to_string()],
            ..PoolQuery::default()
        };

        assert_eq!(ids(resolve(&query, &groups).unwrap()), ["plain"]);
        assert_eq!(ids(excluded_composites(&query, &groups)), ["composite"]);
        // Named explicitly and still present, so it is not "missing" — it is
        // omitted, which is a different sentence in the log.
        assert!(missing_members(&query, &groups).is_empty());
    }

    #[test]
    fn an_empty_list_names_the_handles_that_are_gone() {
        let groups = vec![group(
            "all",
            "All",
            vec![server("still-here", Protocol::Socks, None, None)],
        )];
        let query = PoolQuery {
            members: vec!["gone-one".to_string(), "gone-two".to_string()],
            ..PoolQuery::default()
        };

        assert_eq!(
            resolve(&query, &groups).unwrap_err().to_string(),
            "no server is left of the pool's list (gone-one, gone-two)"
        );
    }

    #[test]
    fn a_name_is_a_label_and_never_selects_anything() {
        let groups = vec![group(
            "all",
            "All",
            vec![server("one", Protocol::Socks, Some("ch"), None)],
        )];
        let named = PoolQuery {
            name: "Europe".to_string(),
            ..PoolQuery::default()
        };

        assert_eq!(named.kind(), PoolKind::Rule);
        assert!(!named.has_filters());
        assert_eq!(
            ids(resolve(&named, &groups).unwrap()),
            ids(resolve(&PoolQuery::default(), &groups).unwrap())
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
