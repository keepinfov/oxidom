use std::net::Ipv4Addr;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::tun::plan::{Cidr, RouteSpec, Via};
use crate::{bind, fsutil, paths};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub sessions: Vec<SessionState>,
    /// `oxidom run` cgroups that must not reach the ordinary path while no
    /// tunnel is carrying them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run_cgroups: Vec<RunCgroupRecord>,
}

/// One bound `oxidom run` cgroup, as the state file carries it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunCgroupRecord {
    pub profile: String,
    pub path: String,
    pub level: u32,
}

impl RunCgroupRecord {
    pub fn slice(&self) -> crate::run::CgroupSlice {
        crate::run::CgroupSlice {
            path: self.path.clone(),
            level: self.level,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    pub profile: String,
    pub server_id: Option<String>,
    pub address: Ipv4Addr,
    pub socks_port: u16,
    pub http_port: u16,
    /// PID of the xray child of the last run, so a crash never leaves an
    /// orphaned tunnel: the next start kills it if it is still an xray process.
    pub xray_pid: Option<u32>,
    pub interface: Option<InterfaceState>,
    /// Pool selection is additive: an older daemon sees `server_id = None`
    /// and therefore cannot mistake a pool for one of its members.
    #[serde(default)]
    pub pool_members: Vec<String>,
    #[serde(default)]
    pub pool_strategy: String,
    /// Zero means this is not a pool session.
    #[serde(default)]
    pub api_port: u16,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            profile: String::new(),
            server_id: None,
            address: Ipv4Addr::UNSPECIFIED,
            socks_port: 0,
            http_port: 0,
            xray_pid: None,
            interface: None,
            pool_members: Vec::new(),
            pool_strategy: String::new(),
            api_port: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InterfaceState {
    pub device: String,
    pub address: Ipv4Addr,
    pub mtu: u16,
    pub table: u32,
    pub mark: u32,
    /// Only an interface created by oxidom may be removed during recovery.
    pub created: bool,
    pub tun2socks_pid: Option<u32>,
    /// Planned routes are recorded before application so crash cleanup may
    /// harmlessly over-delete, but can never forget a route already applied.
    /// This is the entire private + system plan, not a reconstruction.
    pub routes: Vec<RouteRecord>,
    pub rule: bool,
    /// The profile's nft chain was planned before application. Cleanup may
    /// harmlessly remove a chain that the atomic transaction never created.
    pub nft_rule: bool,
    /// Versioned failover intents. Legacy resource fields MUST stay empty when
    /// present: old readers cannot safely clean these resources. Downgrade only
    /// after the owning daemon has explicitly removed every owned interface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ownership: Option<InterfaceOwnership>,
}

impl Default for InterfaceState {
    fn default() -> Self {
        Self {
            device: String::new(),
            address: Ipv4Addr::UNSPECIFIED,
            mtu: 0,
            table: 0,
            mark: 0,
            created: false,
            tun2socks_pid: None,
            routes: Vec::new(),
            rule: false,
            nft_rule: false,
            ownership: None,
        }
    }
}

/// Protocol is an administrative namespace, not an unforgeable owner token.
pub const OWNED_ROUTE_PROTOCOL: u8 = 242;
pub const OWNED_STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceOwnership {
    pub cookie: String,
    pub metric: u32,
    pub gateway_interface: Option<u32>,
    pub device_index: Option<u32>,
    pub routes: Vec<OwnedRoute>,
    pub rule: Option<OwnedRule>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cleanup_dirty: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedRoute {
    #[serde(deserialize_with = "deserialize_owned_route_record")]
    pub route: RouteRecord,
    pub output_interface: u32,
}

fn deserialize_owned_route_record<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<RouteRecord, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictRoute {
        address: Ipv4Addr,
        prefix: u8,
        table: u32,
        gateway: Option<Ipv4Addr>,
        interface: Option<u32>,
    }
    let route = StrictRoute::deserialize(deserializer)?;
    Ok(RouteRecord {
        address: route.address,
        prefix: route.prefix,
        table: route.table,
        gateway: route.gateway,
        interface: route.interface,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedRule {
    pub mark: u32,
    pub table: u32,
    pub priority: u32,
}

impl InterfaceOwnership {
    pub fn new(gateway_interface: Option<u32>) -> Result<Self> {
        use ring::rand::{SecureRandom, SystemRandom};
        let mut bytes = [0_u8; 20];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("generating TUN recovery identity"))?;
        Ok(Self {
            cookie: bytes[..16]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            metric: u32::from_be_bytes(bytes[16..].try_into()?).max(1),
            gateway_interface,
            device_index: None,
            routes: Vec::new(),
            rule: None,
            cleanup_dirty: false,
        })
    }

    pub fn alias(&self) -> String {
        format!("oxidom-tun-v1:{}", self.cookie)
    }

    pub fn routing_mark(&self) -> Result<u32> {
        self.validate()?;
        Ok(u32::from_str_radix(&self.cookie[8..16], 16)? | 0x8000_0000)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.cookie.len() == 32
                && self
                    .cookie
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "invalid TUN ownership cookie"
        );
        ensure!(self.metric != 0, "owned routes require a nonzero metric");
        ensure!(
            self.device_index != Some(0) && self.gateway_interface != Some(0),
            "invalid owned interface index"
        );
        ensure!(self.routes.len() <= 512, "too many owned route intents");
        for route in &self.routes {
            ensure!(
                self.device_index.is_some()
                    && route.output_interface != 0
                    && route.route.prefix <= 32,
                "invalid owned route intent"
            );
            let prefix = route.route.prefix;
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            ensure!(
                u32::from(route.route.address) & !mask == 0,
                "owned routes require canonical network prefixes"
            );
            ensure!(
                route.route.gateway.is_none() || route.route.interface.is_none(),
                "ambiguous owned route next hop"
            );
            if route.route.gateway.is_some() {
                ensure!(
                    Some(route.output_interface) == self.gateway_interface
                        && route.route.table == 254,
                    "owned bypass has an invalid output interface or table"
                );
            } else if let Some(index) = route.route.interface {
                ensure!(
                    route.output_interface == index,
                    "owned connected route interface differs from its selector"
                );
            } else {
                ensure!(
                    Some(route.output_interface) == self.device_index,
                    "owned capture route differs from its TUN identity"
                );
            }
        }
        if let Some(rule) = &self.rule {
            ensure!(
                self.device_index.is_some()
                    && rule.mark != 0
                    && rule.table != 0
                    && (1..32766).contains(&rule.priority),
                "invalid owned rule intent"
            );
        }
        ensure!(
            self.routes.is_empty() || self.rule.is_some(),
            "owned route intents require a rule identity"
        );
        if let Some(rule) = &self.rule {
            ensure!(
                self.routes
                    .iter()
                    .all(|route| route.route.table == 254 || route.route.table == rule.table),
                "owned route is outside the recorded routing domain"
            );
        }
        Ok(())
    }
}

fn is_versioned(body: &str) -> bool {
    fn contains_marker(value: &toml::Value) -> bool {
        match value {
            toml::Value::Table(table) => table.iter().any(|(key, value)| {
                key == "format_version" || key == "ownership" || contains_marker(value)
            }),
            toml::Value::Array(array) => array.iter().any(contains_marker),
            _ => false,
        }
    }
    toml::from_str::<toml::Value>(body).is_ok_and(|value| contains_marker(&value))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteRecord {
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub table: u32,
    /// Gateway for a routed next hop; device routes leave this empty.
    pub gateway: Option<Ipv4Addr>,
    /// Existing system interface for a copied connected route. Both `None`
    /// means the profile device, preserving the B2 on-disk representation.
    pub interface: Option<u32>,
}

impl Default for RouteRecord {
    fn default() -> Self {
        Self {
            address: Ipv4Addr::UNSPECIFIED,
            prefix: 0,
            table: 0,
            gateway: None,
            interface: None,
        }
    }
}

impl RouteRecord {
    pub fn from_spec(spec: &RouteSpec) -> Self {
        Self {
            address: spec.destination.address,
            prefix: spec.destination.prefix,
            table: spec.table,
            gateway: match spec.via {
                Via::Device | Via::Interface(_) => None,
                Via::Gateway(gateway) => Some(gateway),
            },
            interface: match spec.via {
                Via::Interface(index) => Some(index),
                Via::Device | Via::Gateway(_) => None,
            },
        }
    }

    pub fn to_spec(&self) -> RouteSpec {
        RouteSpec {
            destination: Cidr {
                address: self.address,
                prefix: self.prefix,
            },
            via: match (self.gateway, self.interface) {
                (Some(gateway), _) => Via::Gateway(gateway),
                (None, Some(index)) => Via::Interface(index),
                (None, None) => Via::Device,
            },
            table: self.table,
        }
    }
}

/// The legacy fields are accepted only here. Keeping them out of `State`
/// ensures the next real write cannot recreate the retired on-disk shape.
#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredState {
    format_version: Option<u32>,
    sessions: Vec<SessionState>,
    run_cgroups: Vec<RunCgroupRecord>,
    active_server_id: Option<String>,
    active_profile: Option<String>,
    xray_pid: Option<u32>,
}

impl State {
    pub fn load(config: &Config) -> State {
        Self::load_checked(config).expect("recovery state cannot be safely read; refusing startup")
    }

    /// Unlike legacy corruption recovery, versioned records are never
    /// quarantined or replaced with an empty state on a parse/version error.
    pub fn load_checked(config: &Config) -> Result<State> {
        let path = paths::state_file()?;
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                if is_versioned(&s) {
                    return Self::decode(&s, config);
                }
                match toml::from_str::<StoredState>(&s) {
                    Ok(stored) => {
                        let needs_migration = stored.sessions.is_empty()
                            && (stored.active_server_id.is_some()
                                || stored.active_profile.is_some()
                                || stored.xray_pid.is_some());
                        let state = migrate(stored, config);
                        if needs_migration {
                            state.save()?;
                        }
                        Ok(state)
                    }
                    Err(error) => {
                        // A malformed document cannot reliably be classified as
                        // legacy. Never discard potentially owned recovery intent.
                        Err(error).context("parsing recovery state; leaving file untouched")
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(error).context("reading recovery state"),
        }
    }

    fn decode(body: &str, config: &Config) -> Result<State> {
        let value: toml::Value = toml::from_str(body).context("parsing recovery state keys")?;
        if is_versioned(body) {
            fn keys(value: &toml::Value, allowed: &[&str]) -> Result<()> {
                let table = value
                    .as_table()
                    .context("versioned recovery record must be a table")?;
                ensure!(
                    table.keys().all(|key| allowed.contains(&key.as_str())),
                    "unknown versioned recovery field"
                );
                Ok(())
            }
            keys(&value, &["format_version", "sessions", "run_cgroups"])?;
            if let Some(sessions) = value.get("sessions") {
                for session in sessions
                    .as_array()
                    .context("versioned sessions must be an array")?
                {
                    keys(
                        session,
                        &[
                            "profile",
                            "server_id",
                            "address",
                            "socks_port",
                            "http_port",
                            "xray_pid",
                            "interface",
                            "pool_members",
                            "pool_strategy",
                            "api_port",
                        ],
                    )?;
                    if let Some(interface) = session.get("interface") {
                        keys(
                            interface,
                            &[
                                "device",
                                "address",
                                "mtu",
                                "table",
                                "mark",
                                "created",
                                "tun2socks_pid",
                                "routes",
                                "rule",
                                "nft_rule",
                                "ownership",
                            ],
                        )?;
                        if let Some(routes) = interface.get("routes") {
                            for route in routes
                                .as_array()
                                .context("versioned routes must be an array")?
                            {
                                keys(
                                    route,
                                    &["address", "prefix", "table", "gateway", "interface"],
                                )?;
                            }
                        }
                    }
                }
            }
            if let Some(cgroups) = value.get("run_cgroups") {
                for cgroup in cgroups
                    .as_array()
                    .context("versioned cgroups must be an array")?
                {
                    keys(cgroup, &["profile", "path", "level"])?;
                }
            }
        }
        let stored: StoredState =
            toml::from_str(body).context("parsing versioned recovery state")?;
        ensure!(
            stored.format_version.is_none() || stored.format_version == Some(OWNED_STATE_VERSION),
            "unsupported recovery state version; leave state untouched and use a compatible daemon"
        );
        if stored.format_version.is_some() {
            ensure!(
                stored.active_server_id.is_none()
                    && stored.active_profile.is_none()
                    && stored.xray_pid.is_none(),
                "versioned recovery cannot contain retired state fields"
            );
        }
        for session in &stored.sessions {
            if let Some(interface) = &session.interface
                && let Some(owner) = &interface.ownership
            {
                ensure!(
                    stored.format_version == Some(OWNED_STATE_VERSION),
                    "owned recovery requires an explicit format version"
                );
                owner.validate()?;
                ensure!(
                    !interface.created && !interface.rule && interface.routes.is_empty(),
                    "owned recovery must not carry legacy cleanup selectors"
                );
                if let Some(rule) = &owner.rule {
                    ensure!(
                        interface.mark == rule.mark && interface.table == rule.table,
                        "owned rule differs from the recorded interface domain"
                    );
                }
            }
        }
        Ok(migrate(stored, config))
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::state_file()?;
        // Refuse overwriting an unknown format even if another writer changed
        // the file after startup. No cleanup may precede the startup guard.
        match std::fs::read_to_string(&path) {
            Ok(body) if is_versioned(&body) => {
                Self::decode(&body, &Config::default())?;
            }
            Ok(body) => {
                toml::from_str::<StoredState>(&body).context("checking existing recovery state")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("checking existing recovery state before write");
            }
        }
        let s = self.encode()?;
        fsutil::write_private_atomic(&path, s.as_bytes()).context("writing state")?;
        Ok(())
    }

    fn encode(&self) -> Result<String> {
        #[derive(Serialize)]
        struct Versioned<'a> {
            format_version: u32,
            #[serde(flatten)]
            state: &'a State,
        }
        let owned = self.sessions.iter().any(|session| {
            session
                .interface
                .as_ref()
                .is_some_and(|interface| interface.ownership.is_some())
        });
        let body = if owned {
            toml::to_string_pretty(&Versioned {
                format_version: OWNED_STATE_VERSION,
                state: self,
            })?
        } else {
            toml::to_string_pretty(self)?
        };
        // Validate before committing intents, not only when recovering them.
        Self::decode(&body, &Config::default())?;
        Ok(body)
    }
}

fn migrate(stored: StoredState, config: &Config) -> State {
    let StoredState {
        format_version: _,
        sessions: stored_sessions,
        run_cgroups,
        active_server_id,
        active_profile,
        xray_pid,
    } = stored;
    if !stored_sessions.is_empty() {
        let mut sessions = stored_sessions;
        // B2 recorded only system routes and reconstructed the private
        // default during recovery. Promote that old on-disk shape once so B3
        // cleanup has one source of truth and an upgrade after a crash cannot
        // strand the pre-B3 private route.
        for session in &mut sessions {
            let Some(interface) = session.interface.as_mut() else {
                continue;
            };
            if interface.ownership.is_none()
                && interface.rule
                && !interface
                    .routes
                    .iter()
                    .any(|route| route.table == interface.table)
            {
                interface.routes.insert(
                    0,
                    RouteRecord {
                        address: Ipv4Addr::UNSPECIFIED,
                        prefix: 0,
                        table: interface.table,
                        gateway: None,
                        interface: None,
                    },
                );
            }
        }
        return State {
            sessions,
            run_cgroups,
        };
    }
    if active_server_id.is_none() && active_profile.is_none() && xray_pid.is_none() {
        return State {
            sessions: Vec::new(),
            run_cgroups,
        };
    }

    let profile = active_profile.unwrap_or_else(|| "default".to_string());
    let Some(address) = bind::address_for(&profile, &[]) else {
        log::warn!("could not allocate a loopback address while migrating state for {profile:?}");
        return State {
            sessions: Vec::new(),
            run_cgroups,
        };
    };
    State {
        sessions: vec![SessionState {
            profile,
            server_id: active_server_id,
            address,
            socks_port: config.socks_port,
            http_port: config.http_port,
            xray_pid,
            interface: None,
            pool_members: Vec::new(),
            pool_strategy: String::new(),
            api_port: 0,
        }],
        run_cgroups,
    }
}

/// Storage for the cached subscriptions list (subscriptions.json).
pub mod store {
    use anyhow::{Context, Result};

    use crate::model::Subscription;
    use crate::{fsutil, paths};

    /// Load the cached subscriptions. On a corrupt file the data is moved
    /// aside instead of being silently replaced; the returned warning is
    /// surfaced to the user by the GUI.
    pub fn load() -> (Vec<Subscription>, Option<String>) {
        let Ok(path) = paths::subscriptions_file() else {
            return (Vec::new(), None);
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(subs) => (subs, None),
                Err(error) => {
                    let moved = fsutil::quarantine(&path);
                    log::warn!("subscriptions.json is not valid ({error}); moved to {moved:?}");
                    let warning = match moved {
                        Some(moved) => format!(
                            "Saved subscriptions could not be read and were moved to {}",
                            moved.display()
                        ),
                        None => "Saved subscriptions could not be read".to_string(),
                    };
                    (Vec::new(), Some(warning))
                }
            },
            Err(_) => (Vec::new(), None),
        }
    }

    pub fn save(subs: &[Subscription]) -> Result<()> {
        let path = paths::subscriptions_file()?;
        let s = serde_json::to_string_pretty(subs).context("serializing subscriptions")?;
        fsutil::write_private_atomic(&path, s.as_bytes()).context("writing subscriptions")?;
        Ok(())
    }
}

/// Return the per-install HWID, generating one if absent. Only *used* when a
/// subscription opts in — generation here does not transmit anything.
pub fn hwid() -> Result<String> {
    let path = paths::hwid_file()?;
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let id = generate_hwid();
    fsutil::write_private_atomic(&path, id.as_bytes()).context("writing hwid")?;
    Ok(id)
}

fn generate_hwid() -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    std::process::id().hash(&mut h);
    format!("{:016x}{:016x}", h.finish(), std::process::id() as u64)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    use anyhow::Result;

    use super::*;

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);

    const OWNED_LITERAL: &str = r#"format_version = 1
[[sessions]]
profile = "work"
address = "127.0.0.1"
[sessions.interface]
device = "oxi-work"
mark = 2147483649
table = 2147483649
created = false
routes = []
rule = false
[sessions.interface.ownership]
cookie = "0123456789abcdef0123456789abcdef"
metric = 1234567
gateway_interface = 7
device_index = 42
[[sessions.interface.ownership.routes]]
output_interface = 7
[sessions.interface.ownership.routes.route]
address = "192.0.2.10"
prefix = 32
table = 254
gateway = "192.0.2.1"
[sessions.interface.ownership.rule]
mark = 2147483649
table = 2147483649
priority = 12000
"#;

    #[test]
    fn owned_route_selectors_are_required_and_unknown_fields_are_refused() {
        for body in [
            OWNED_LITERAL.replace("prefix = 32\n", ""),
            OWNED_LITERAL.replace("address = \"192.0.2.10\"\n", ""),
            OWNED_LITERAL.replace("table = 254\n", ""),
            OWNED_LITERAL.replace("prefix = 32", "prefix = 32\nunknown_selector = 7"),
        ] {
            assert!(State::decode(&body, &Config::default()).is_err());
        }
    }

    #[test]
    fn versioned_outer_records_refuse_unknown_keys_without_restricting_legacy_records() -> Result<()>
    {
        for body in [
            OWNED_LITERAL.replacen(
                "format_version = 1",
                "format_version = 1\nunknown_root = 1",
                1,
            ),
            OWNED_LITERAL.replacen(
                "profile = \"work\"",
                "profile = \"work\"\nunknown_session = 1",
                1,
            ),
            OWNED_LITERAL.replacen(
                "device = \"oxi-work\"",
                "device = \"oxi-work\"\nunknown_interface = 1",
                1,
            ),
            format!(
                "{OWNED_LITERAL}\n[[run_cgroups]]\nprofile = 'work'\npath = 'invented'\nlevel = 1\nunknown_cgroup = 1\n"
            ),
        ] {
            assert!(State::decode(&body, &Config::default()).is_err());
        }
        State::decode(
            "unknown_legacy = 1\n[[sessions]]\nprofile = 'old'\nfuture = true\n",
            &Config::default(),
        )?;
        Ok(())
    }

    #[test]
    fn legacy_values_named_ownership_do_not_trigger_versioned_decoding_or_recursive_writes()
    -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("legacy-marker-value");
        let body = "active_server_id = 'ownership'\nactive_profile = 'ownership'\n";
        assert!(!is_versioned(body));
        let decoded = State::decode(body, &Config::default())?;
        assert_eq!(decoded.sessions[0].profile, "ownership");
        assert!(!crate::paths::state_file()?.exists(), "decoding is pure");
        crate::fsutil::write_private_atomic(&crate::paths::state_file()?, body.as_bytes())?;
        let loaded = State::load_checked(&Config::default())?;
        assert_eq!(loaded.sessions, decoded.sessions);
        assert!(std::fs::read_to_string(crate::paths::state_file()?)?.contains("[[sessions]]"));
        Ok(())
    }

    #[test]
    fn owned_literal_round_trips_without_enrolling_legacy_cleanup_or_b2_promotion() -> Result<()> {
        let state = State::decode(OWNED_LITERAL, &Config::default())?;
        let interface = state.sessions[0].interface.as_ref().unwrap();
        assert!(!interface.created && !interface.rule && interface.routes.is_empty());
        let owner = interface.ownership.as_ref().unwrap();
        assert_eq!(owner.metric, 1234567);
        assert_eq!(owner.routes[0].output_interface, 7);
        assert_eq!(
            owner.routes[0].route.gateway,
            Some(Ipv4Addr::new(192, 0, 2, 1))
        );
        assert_eq!(owner.rule.as_ref().unwrap().priority, 12000);
        let encoded = state.encode()?;
        assert!(encoded.starts_with("format_version = 1\n"));
        assert_eq!(
            State::decode(&encoded, &Config::default())?.sessions,
            state.sessions
        );
        // This is what old readers actually do: ignore unknown ownership,
        // NOT refuse startup. They cannot clean these resources safely.
        #[derive(Deserialize)]
        struct OldInterface {
            created: bool,
            routes: Vec<RouteRecord>,
            rule: bool,
        }
        let value: toml::Value = toml::from_str(&encoded)?;
        let old: OldInterface = value["sessions"][0]["interface"].clone().try_into()?;
        assert!(!old.created && !old.rule && old.routes.is_empty());
        let legacy = State::decode(
            "[[sessions]]\nprofile = 'old'\n[sessions.interface]\ndevice = 'oxi-old'\nrule = true\ntable = 12345\n",
            &Config::default(),
        )?;
        assert!(
            legacy.sessions[0]
                .interface
                .as_ref()
                .unwrap()
                .ownership
                .is_none()
        );
        assert_eq!(
            legacy.sessions[0].interface.as_ref().unwrap().routes.len(),
            1
        );
        assert!(!legacy.encode()?.contains("format_version"));
        Ok(())
    }

    #[test]
    fn unsupported_and_malformed_owned_state_is_never_quarantined_or_overwritten() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("owned-format-guard");
        let path = crate::paths::state_file()?;
        for body in [
            OWNED_LITERAL.replace("format_version = 1", "format_version = 2"),
            OWNED_LITERAL.replace("format_version = 1\n", ""),
            OWNED_LITERAL.replace("metric = 1234567", "metric = 0"),
            OWNED_LITERAL.replace("created = false", "created = true"),
            OWNED_LITERAL.replace("device_index = 42", "device_index = 0"),
            "format_version = 'unknown'\n".into(),
        ] {
            crate::fsutil::write_private_atomic(&path, body.as_bytes())?;
            assert!(State::load_checked(&Config::default()).is_err());
            assert!(State::default().save().is_err());
            assert_eq!(std::fs::read_to_string(&path)?, body);
        }
        Ok(())
    }

    struct TestRoot {
        path: std::path::PathBuf,
    }

    impl TestRoot {
        fn install(label: &str) -> Self {
            let suffix = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "oxidom-state-test-{label}-{}-{suffix}",
                std::process::id()
            ));
            crate::paths::set_test_root(Some(path.clone()));
            Self { path }
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            crate::paths::set_test_root(None);
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn legacy_state_becomes_one_session_without_losing_the_server() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("legacy-migration");
        let path = crate::paths::state_file()?;
        crate::fsutil::write_private_atomic(
            &path,
            b"active_server_id = \"server-id\"\nactive_profile = \"work\"\nxray_pid = 42\n",
        )?;
        let config = Config {
            socks_port: 21080,
            http_port: 21081,
            ..Config::default()
        };

        let state = State::load(&config);

        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].profile, "work");
        assert_eq!(state.sessions[0].server_id.as_deref(), Some("server-id"));
        assert_eq!(state.sessions[0].socks_port, 21080);
        assert_eq!(state.sessions[0].http_port, 21081);
        assert_eq!(state.sessions[0].xray_pid, Some(42));
        assert_eq!(
            state.sessions[0].address,
            bind::address_for("work", &[]).unwrap()
        );

        let migrated = std::fs::read_to_string(path)?;
        assert!(migrated.contains("[[sessions]]"));
        assert!(!migrated.contains("active_server_id"));
        assert!(!migrated.contains("active_profile"));
        Ok(())
    }

    #[test]
    fn legacy_state_without_a_profile_becomes_default() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("legacy-default");
        let path = crate::paths::state_file()?;
        crate::fsutil::write_private_atomic(&path, b"active_server_id = \"server-id\"\n")?;

        let state = State::load(&Config::default());

        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].profile, "default");
        assert_eq!(state.sessions[0].address, Ipv4Addr::LOCALHOST);
        assert_eq!(state.sessions[0].server_id.as_deref(), Some("server-id"));
        Ok(())
    }

    #[test]
    fn current_state_is_not_rewritten_while_loading() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("no-rewrite");
        let state = State {
            sessions: vec![SessionState {
                profile: "default".to_string(),
                server_id: Some("server-id".to_string()),
                address: Ipv4Addr::LOCALHOST,
                socks_port: 10808,
                http_port: 10809,
                xray_pid: None,
                interface: None,
                pool_members: Vec::new(),
                pool_strategy: String::new(),
                api_port: 0,
            }],
            run_cgroups: Vec::new(),
        };
        state.save()?;
        let path = crate::paths::state_file()?;
        let mixed = format!(
            "active_server_id = \"legacy-id\"\nactive_profile = \"legacy\"\nxray_pid = 9\n\n{}",
            std::fs::read_to_string(&path)?
        );
        crate::fsutil::write_private_atomic(&path, mixed.as_bytes())?;
        let inode = std::fs::metadata(&path)?.ino();

        let loaded = State::load(&Config::default());

        assert_eq!(loaded.sessions, state.sessions);
        assert_eq!(std::fs::metadata(path)?.ino(), inode);
        Ok(())
    }

    #[test]
    fn pool_session_round_trips_without_claiming_one_active_server() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("pool-session");
        let state = State {
            sessions: vec![SessionState {
                profile: "spread".to_string(),
                server_id: None,
                address: Ipv4Addr::new(127, 91, 37, 1),
                socks_port: 10808,
                http_port: 10809,
                xray_pid: Some(42),
                interface: None,
                pool_members: vec!["one".to_string(), "two".to_string()],
                pool_strategy: "roundRobin".to_string(),
                api_port: 18082,
            }],
            run_cgroups: Vec::new(),
        };

        state.save()?;
        let loaded = State::load(&Config::default());

        assert_eq!(loaded.sessions, state.sessions);
        assert!(loaded.sessions[0].server_id.is_none());
        assert_eq!(loaded.sessions[0].pool_members, ["one", "two"]);
        assert_eq!(loaded.sessions[0].api_port, 18082);
        Ok(())
    }

    #[test]
    fn b2_interface_state_is_promoted_to_a_complete_route_plan() -> Result<()> {
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("b2-interface-plan");
        let path = crate::paths::state_file()?;
        crate::fsutil::write_private_atomic(
            &path,
            b"[[sessions]]\n\
              profile = \"work\"\n\
              address = \"127.91.37.1\"\n\
              socks_port = 10808\n\
              http_port = 10809\n\
              \n\
              [sessions.interface]\n\
              device = \"oxi-work\"\n\
              address = \"198.18.9.7\"\n\
              mtu = 1500\n\
              table = 28449\n\
              mark = 28449\n\
              rule = true\n\
              \n\
              [[sessions.interface.routes]]\n\
              address = \"10.0.0.0\"\n\
              prefix = 8\n\
              table = 254\n",
        )?;

        let state = State::load(&Config::default());
        let routes = &state.sessions[0].interface.as_ref().unwrap().routes;

        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].address, Ipv4Addr::UNSPECIFIED);
        assert_eq!(routes[0].prefix, 0);
        assert_eq!(routes[0].table, 28449);
        assert_eq!(routes[1].table, 254);
        Ok(())
    }
}
