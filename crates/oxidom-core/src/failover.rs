//! Explicit authorization and application-neutral sequential recovery.
//!
//! The caller owns persistence, catalog revalidation, eligible failure causes,
//! and generation/core attribution. This module performs no I/O or scheduling.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::config::OnCoreExit;
use crate::model::{OutboundSpec, Server};
use crate::profile::ProfileInterface;

/// Runtime-only routing authorization. Credentials and TLS/HTTP names are not
/// snapshotted: only endpoint identity and the supported outbound shape are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunEnvelope {
    interface: ProfileInterface,
    candidates: Vec<(String, Ipv4Addr, String)>,
}

impl TunEnvelope {
    /// `catalog` must be the exact authorized candidate set, not the full store.
    pub fn new<'a>(
        catalog: impl IntoIterator<Item = &'a Server>,
        requested: &ProfileInterface,
        effective_policy: OnCoreExit,
    ) -> Result<Self> {
        ensure!(
            requested.enable,
            "TUN failover requires an enabled interface"
        );
        ensure!(
            effective_policy == OnCoreExit::Hold,
            "TUN failover requires on_core_exit = hold"
        );
        if !requested.device.is_empty() {
            crate::bind::validate_device_name(&requested.device)?;
        }
        ensure!(
            requested.mtu == 0 || requested.mtu >= 576,
            "TUN failover requires a valid interface MTU"
        );
        if !requested.address.is_empty() {
            ensure!(
                requested.address.parse::<Ipv4Addr>().is_ok(),
                "TUN failover requires an IPv4 interface address"
            );
        }
        let list = requested
            .list
            .iter()
            .map(|entry| entry.parse::<crate::tun::plan::Cidr>())
            .collect::<Result<Vec<_>>>()?;
        if requested.routes == crate::profile::RouteMode::List {
            ensure!(
                !list.is_empty(),
                "TUN failover list routes require capture CIDRs"
            );
        }
        let mut candidates = Vec::new();
        let mut ids = HashSet::new();
        for server in catalog {
            ensure!(
                !server.id.trim().is_empty() && ids.insert(&server.id),
                "TUN failover requires unique exact candidate IDs"
            );
            let (address, shape) = Self::endpoint(server)?;
            candidates.push((server.id.clone(), address, shape));
        }
        ensure!(
            (2..=64).contains(&candidates.len()),
            "TUN failover requires 2..64 candidates"
        );
        if requested.routes == crate::profile::RouteMode::List {
            ensure!(
                !list.iter().any(|route| route.prefix == 32
                    && candidates
                        .iter()
                        .any(|candidate| candidate.1 == route.address)),
                "TUN failover capture /32 conflicts with a candidate bypass"
            );
        }
        Ok(Self {
            interface: requested.clone(),
            candidates,
        })
    }

    fn endpoint(server: &Server) -> Result<(Ipv4Addr, String)> {
        ensure!(
            server.outbound_patch.is_none(),
            "TUN failover refuses raw outbound patches"
        );
        let address = server
            .address
            .parse::<Ipv4Addr>()
            .map_err(|_| anyhow::anyhow!("TUN failover requires literal IPv4 dial addresses"))?;
        ensure!(
            address.octets()[0] != 0
                && address.octets()[0] < 224
                && !address.is_loopback()
                && !address.is_link_local()
                && !address.is_broadcast(),
            "TUN failover requires unicast non-loopback IPv4 dial addresses"
        );
        ensure!(
            server.port != 0,
            "TUN failover requires a nonzero dial port"
        );
        let (kind, stream) = match &server.spec {
            OutboundSpec::Vless { stream, .. } => ("vless", Some(stream)),
            OutboundSpec::Vmess { stream, .. } => ("vmess", Some(stream)),
            OutboundSpec::Trojan { stream, .. } => ("trojan", Some(stream)),
            OutboundSpec::Shadowsocks { .. } => ("shadowsocks", None),
            OutboundSpec::Socks { .. } => ("socks", None),
            OutboundSpec::Http { .. } => ("http", None),
            OutboundSpec::Hysteria2 { settings, .. } => {
                ensure!(
                    settings.port_hop.is_empty(),
                    "TUN failover refuses port hopping"
                );
                ("hysteria2", None)
            }
            OutboundSpec::XrayProfile { .. } => bail!("TUN failover refuses composite outbounds"),
        };
        let shape = if let Some(stream) = stream {
            ensure!(
                stream.xhttp_extra.is_none(),
                "TUN failover refuses XHTTP extensions"
            );
            ensure!(stream.ech.is_none(), "TUN failover refuses ECH extensions");
            ensure!(
                matches!(
                    stream.network.as_str(),
                    "" | "tcp" | "ws" | "grpc" | "xhttp" | "splithttp" | "h2"
                ),
                "TUN failover refuses unsupported transports"
            );
            ensure!(
                matches!(stream.security.as_str(), "" | "none" | "tls" | "reality"),
                "TUN failover refuses unsupported security"
            );
            format!("{kind}/{}/{}", stream.network, stream.security)
        } else {
            kind.to_string()
        };
        Ok((address, shape))
    }

    pub fn validate_candidates<'a>(
        &self,
        catalog: impl IntoIterator<Item = &'a Server>,
    ) -> Result<()> {
        let catalog = catalog.into_iter().collect::<Vec<_>>();
        for (id, address, shape) in &self.candidates {
            let matches = catalog
                .iter()
                .filter(|server| &server.id == id)
                .collect::<Vec<_>>();
            ensure!(
                matches.len() == 1,
                "TUN failover candidate is missing or ambiguous"
            );
            let current = Self::endpoint(matches[0])?;
            ensure!(
                current == (*address, shape.clone()),
                "TUN failover candidate endpoint or shape changed"
            );
        }
        Ok(())
    }

    pub fn validate_candidate(&self, server: &Server) -> Result<()> {
        let (_, address, shape) = self
            .candidates
            .iter()
            .find(|candidate| candidate.0 == server.id)
            .ok_or_else(|| anyhow::anyhow!("server is outside the TUN failover envelope"))?;
        ensure!(
            Self::endpoint(server)? == (*address, shape.clone()),
            "TUN failover candidate endpoint or shape changed"
        );
        Ok(())
    }

    pub fn validate_interface(
        &self,
        requested: &ProfileInterface,
        effective_policy: OnCoreExit,
    ) -> Result<()> {
        ensure!(
            requested == &self.interface && effective_policy == OnCoreExit::Hold,
            "TUN failover interface or hold policy changed"
        );
        Ok(())
    }

    pub fn contains(&self, id: &str) -> bool {
        self.candidates.iter().any(|candidate| candidate.0 == id)
    }

    pub fn addresses(&self) -> Vec<Ipv4Addr> {
        let mut addresses = Vec::new();
        for (_, address, _) in &self.candidates {
            if !addresses.contains(address) {
                addresses.push(*address);
            }
        }
        addresses
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FailoverPolicy {
    pub enabled: bool,
    pub members: Vec<String>,
    pub max_attempts: usize,
    pub cooldown_secs: u64,
}

impl Default for FailoverPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            members: Vec::new(),
            max_attempts: 3,
            cooldown_secs: 30,
        }
    }
}

impl FailoverPolicy {
    /// Validate persisted shape without resolving aliases or consulting a catalog.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=64).contains(&self.max_attempts),
            "[failover] max_attempts must be between 1 and 64"
        );
        ensure!(
            (1..=3600).contains(&self.cooldown_secs),
            "[failover] cooldown_secs must be between 1 and 3600"
        );
        if self.enabled {
            ensure!(
                (2..=64).contains(&self.members.len()),
                "enabled [failover] requires between 2 and 64 members"
            );
            let mut seen = HashSet::new();
            for id in &self.members {
                ensure!(
                    !id.trim().is_empty() && seen.insert(id),
                    "[failover] members must be nonempty unique canonical server IDs"
                );
            }
        }
        Ok(())
    }

    /// Resolve every authorized member by exact catalog ID, never by alias.
    /// Call at activation and again before admission; any error must disarm an
    /// existing activation. A later catalog readdition must not rearm it.
    pub fn validate_candidates<'a>(
        &self,
        catalog: impl IntoIterator<Item = &'a Server>,
    ) -> Result<()> {
        self.validate()?;
        if !self.enabled {
            return Ok(());
        }
        let catalog: HashMap<_, _> = catalog
            .into_iter()
            .map(|server| (server.id.as_str(), server))
            .collect();
        for id in &self.members {
            let Some(server) = catalog.get(id.as_str()) else {
                bail!("[failover] member is missing from the server catalog");
            };
            ensure!(
                !matches!(&server.spec, OutboundSpec::XrayProfile { .. }),
                "[failover] members cannot be composite Xray profiles"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Idle,
    Wait(Instant),
    Attempt(String),
    InFlight,
    Exhausted,
    Disarmed,
}

/// One activation's private progress. Construct only after exact catalog
/// validation of an explicitly activated saved profile. Never persist it.
#[derive(Debug)]
pub struct Activation {
    policy: FailoverPolicy,
    current: usize,
    visited: Vec<bool>,
    attempts: usize,
    episode: bool,
    in_flight: bool,
    armed: bool,
    cooldown_until: Option<Instant>,
}

impl Activation {
    /// `initial_id` is the resolved canonical primary, not the saved alias.
    /// The explicit initial start does not consume budget or start cooldown.
    pub fn new(policy: FailoverPolicy, initial_id: &str) -> Result<Self> {
        policy.validate()?;
        ensure!(policy.enabled, "failover authorization is disabled");
        let current = policy
            .members
            .iter()
            .position(|id| id == initial_id)
            .ok_or_else(|| anyhow::anyhow!("failover primary is not an authorized member"))?;
        Ok(Self {
            visited: vec![false; policy.members.len()],
            policy,
            current,
            attempts: 0,
            episode: false,
            in_flight: false,
            armed: true,
            cooldown_until: None,
        })
    }

    /// Begin only for an eligible, current failure. An ongoing or exhausted
    /// episode cannot be restarted to replenish its budget.
    pub fn begin_episode(&mut self, failed_id: &str) -> Result<()> {
        ensure!(self.armed, "failover activation is disarmed");
        ensure!(!self.episode, "failover recovery episode is already active");
        ensure!(
            self.policy.members[self.current] == failed_id,
            "failover failure does not belong to the current candidate"
        );
        self.visited[self.current] = true;
        self.episode = true;
        Ok(())
    }

    /// Inspect without consuming budget. `backoff_until` is a fixed monotonic
    /// deadline supplied by the caller, not a duration restarted on each poll.
    /// The caller bounds reconnect backoff according to its reconnect policy.
    pub fn next(&self, now: Instant, backoff_until: Instant) -> Decision {
        if !self.armed {
            return Decision::Disarmed;
        }
        if self.in_flight {
            return Decision::InFlight;
        }
        if !self.episode {
            return Decision::Idle;
        }
        if self.attempts >= self.policy.max_attempts || self.visited.iter().all(|seen| *seen) {
            return Decision::Exhausted;
        }
        let deadline = self
            .cooldown_until
            .map_or(backoff_until, |cooldown| cooldown.max(backoff_until));
        if now < deadline {
            return Decision::Wait(deadline);
        }
        let index = (1..self.policy.members.len())
            .map(|offset| (self.current + offset) % self.policy.members.len())
            .find(|index| !self.visited[*index])
            .expect("an unvisited candidate remains");
        Decision::Attempt(self.policy.members[index].clone())
    }

    /// Admit immediately before starting, after caller-side currentness and
    /// authorization revalidation. Startup failure still consumes this attempt.
    /// At most one admission may remain in flight through traffic confirmation.
    pub fn admit(&mut self, id: &str, now: Instant, backoff_until: Instant) -> Result<()> {
        ensure!(
            self.next(now, backoff_until) == Decision::Attempt(id.to_owned()),
            "failover candidate is not ready for admission"
        );
        self.current = self
            .policy
            .members
            .iter()
            .position(|member| member == id)
            .expect("admitted candidate is authorized");
        self.visited[self.current] = true;
        self.attempts += 1;
        self.in_flight = true;
        self.cooldown_until = Some(now + Duration::from_secs(self.policy.cooldown_secs));
        Ok(())
    }

    /// Release an admitted attempt after an eligible startup/confirmation
    /// failure. Ineligible failures must disarm rather than advance.
    pub fn attempt_failed(&mut self, id: &str) -> Result<()> {
        self.check_in_flight(id)?;
        self.in_flight = false;
        Ok(())
    }

    /// Accept only current traffic-confirmed recovery. Keep activation cooldown
    /// across episodes and do not schedule automatic failback.
    pub fn confirm_success(&mut self, id: &str) -> Result<()> {
        self.check_in_flight(id)?;
        self.in_flight = false;
        self.episode = false;
        self.attempts = 0;
        self.visited.fill(false);
        Ok(())
    }

    fn check_in_flight(&self, id: &str) -> Result<()> {
        ensure!(
            self.in_flight && self.policy.members[self.current] == id,
            "failover completion does not belong to the in-flight candidate"
        );
        Ok(())
    }

    /// Permanent cancellation/revocation. An already admitted confirmation may
    /// finish, but no method can rearm this activation or admit another start.
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tun_candidates() -> Vec<Server> {
        [
            "socks://192.0.2.10:1080#One",
            "trojan://invented@198.51.100.20:443#Two",
        ]
        .map(|link| crate::link::parse_link(link).unwrap())
        .to_vec()
    }

    #[test]
    fn tun_envelopes_pin_exact_ids_and_addresses_but_keep_credentials_current() -> Result<()> {
        let mut candidates = tun_candidates();
        let interface = ProfileInterface {
            enable: true,
            ..Default::default()
        };
        let envelope = TunEnvelope::new(&candidates, &interface, OnCoreExit::Hold)?;
        assert_eq!(
            envelope.addresses(),
            [
                Ipv4Addr::new(192, 0, 2, 10),
                Ipv4Addr::new(198, 51, 100, 20)
            ]
        );
        if let OutboundSpec::Trojan { password, stream } = &mut candidates[1].spec {
            *password = "new-invented-password".into();
            stream.sni = Some("tls.example".into());
            stream.host = Some("transport.example".into());
        }
        envelope.validate_candidates(&candidates)?;
        if let OutboundSpec::Trojan { stream, .. } = &mut candidates[1].spec {
            stream.network = "grpc".into();
        }
        assert!(envelope.validate_candidates(&candidates).is_err());
        if let OutboundSpec::Trojan { stream, .. } = &mut candidates[1].spec {
            stream.network = "tcp".into();
        }
        candidates[0].address = "192.0.2.11".into();
        assert!(envelope.validate_candidates(&candidates).is_err());
        candidates[0].address = "192.0.2.10".into();
        candidates[0].id = "replacement-id".into();
        assert!(envelope.validate_candidates(&candidates).is_err());
        assert!(envelope.validate_candidates(&candidates[1..]).is_err());
        assert!(
            TunEnvelope::new(tun_candidates().iter(), &interface, OnCoreExit::Release).is_err()
        );
        Ok(())
    }

    #[test]
    fn tun_envelopes_refuse_endpoint_escape_and_nonunicast_literals() -> Result<()> {
        let interface = ProfileInterface {
            enable: true,
            ..Default::default()
        };
        for address in [
            "endpoint.example",
            "2001:db8::1",
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.2",
            "169.254.1.1",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
        ] {
            let mut candidates = tun_candidates();
            candidates[0].address = address.into();
            assert!(
                TunEnvelope::new(&candidates, &interface, OnCoreExit::Hold).is_err(),
                "{address}"
            );
        }
        for patch in [serde_json::Value::Null, serde_json::json!({})] {
            let mut candidates = tun_candidates();
            candidates[0].outbound_patch = Some(patch.clone());
            assert!(TunEnvelope::new(&candidates, &interface, OnCoreExit::Hold).is_err());
            candidates[0].outbound_patch = None;
            if let OutboundSpec::Trojan { stream, .. } = &mut candidates[1].spec {
                stream.xhttp_extra = Some(patch);
            }
            assert!(TunEnvelope::new(&candidates, &interface, OnCoreExit::Hold).is_err());
        }
        let candidates = tun_candidates();
        assert!(
            TunEnvelope::new(
                [&candidates[0], &candidates[0]],
                &interface,
                OnCoreExit::Hold
            )
            .is_err()
        );
        Ok(())
    }

    fn policy() -> FailoverPolicy {
        FailoverPolicy {
            enabled: true,
            members: ["a", "b", "c", "d"].map(String::from).to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn persisted_limits_are_bounded_and_enabled_members_are_nonempty_and_unique() -> Result<()> {
        let mut policy = policy();
        for max_attempts in [0, 65] {
            policy.max_attempts = max_attempts;
            assert!(policy.validate().is_err());
        }
        for max_attempts in [1, 64] {
            policy.max_attempts = max_attempts;
            policy.validate()?;
        }
        for cooldown_secs in [0, 3601] {
            policy.cooldown_secs = cooldown_secs;
            assert!(policy.validate().is_err());
        }
        for cooldown_secs in [1, 3600] {
            policy.cooldown_secs = cooldown_secs;
            policy.validate()?;
        }
        for members in [
            vec![],
            vec!["a"],
            vec!["a", ""],
            vec!["a", " "],
            vec!["a", "a"],
        ] {
            policy.members = members.into_iter().map(String::from).collect();
            assert!(policy.validate().is_err());
        }
        policy.members = (0..64).map(|id| format!("server-{id}")).collect();
        policy.validate()?;
        policy.members.push("server-64".into());
        assert!(policy.validate().is_err());
        Ok(())
    }

    #[test]
    fn recovery_wraps_in_saved_order_without_retrying_the_failed_primary() -> Result<()> {
        let now = Instant::now();
        let mut activation = Activation::new(policy(), "c")?;
        assert_eq!(activation.next(now, now), Decision::Idle);
        activation.begin_episode("c")?;
        for (ordinal, id) in ["d", "a", "b"].into_iter().enumerate() {
            let time = now + Duration::from_secs(ordinal as u64 * 30);
            assert_eq!(activation.next(time, now), Decision::Attempt(id.into()));
            // Merely inspecting a candidate never consumes it.
            assert_eq!(activation.next(time, now), Decision::Attempt(id.into()));
            activation.admit(id, time, now)?;
            assert_eq!(activation.next(time, now), Decision::InFlight);
            assert!(activation.admit(id, time, now).is_err());
            activation.attempt_failed(id)?;
        }
        assert_eq!(activation.next(now, now), Decision::Exhausted);
        assert!(activation.begin_episode("b").is_err());
        Ok(())
    }

    #[test]
    fn actual_start_admissions_exhaust_budget_even_when_startup_fails() -> Result<()> {
        let now = Instant::now();
        let mut policy = policy();
        policy.max_attempts = 1;
        let mut activation = Activation::new(policy, "a")?;
        activation.begin_episode("a")?;
        assert!(activation.admit("c", now, now).is_err());
        activation.admit("b", now, now)?;
        activation.attempt_failed("b")?;
        assert_eq!(activation.next(now, now), Decision::Exhausted);
        Ok(())
    }

    #[test]
    fn candidate_exhaustion_stops_before_a_larger_budget() -> Result<()> {
        let now = Instant::now();
        let mut policy = policy();
        policy.members.truncate(2);
        policy.max_attempts = 64;
        let mut activation = Activation::new(policy, "a")?;
        activation.begin_episode("a")?;
        activation.admit("b", now, now)?;
        activation.attempt_failed("b")?;
        assert_eq!(activation.next(now, now), Decision::Exhausted);
        Ok(())
    }

    #[test]
    fn confirmed_recovery_resets_the_episode_but_retains_cooldown() -> Result<()> {
        let now = Instant::now();
        let mut policy = policy();
        policy.max_attempts = 1;
        let mut activation = Activation::new(policy, "a")?;
        activation.begin_episode("a")?;
        activation.admit("b", now, now)?;
        assert!(activation.confirm_success("a").is_err());
        activation.confirm_success("b")?;
        assert_eq!(activation.next(now, now), Decision::Idle);
        activation.begin_episode("b")?;
        let cooldown = now + Duration::from_secs(30);
        assert_eq!(activation.next(now, now), Decision::Wait(cooldown));
        assert!(activation.admit("c", now, now).is_err());
        let backoff = now + Duration::from_secs(50);
        assert_eq!(activation.next(cooldown, backoff), Decision::Wait(backoff));
        activation.admit("c", backoff, backoff)?;
        activation.confirm_success("c")?;
        activation.begin_episode("c")?;
        let time = backoff + Duration::from_secs(30);
        activation.admit("d", time, time)?;
        activation.confirm_success("d")?;
        activation.begin_episode("d")?;
        let time = time + Duration::from_secs(30);
        assert_eq!(activation.next(time, time), Decision::Attempt("a".into()));
        Ok(())
    }

    #[test]
    fn independent_activations_never_share_budget_or_cancellation() -> Result<()> {
        let now = Instant::now();
        let mut first = Activation::new(policy(), "a")?;
        let mut second = Activation::new(policy(), "a")?;
        first.begin_episode("a")?;
        second.begin_episode("a")?;
        first.admit("b", now, now)?;
        first.disarm();
        first.confirm_success("b")?;
        assert_eq!(first.next(now, now), Decision::Disarmed);
        assert!(first.begin_episode("b").is_err());
        assert_eq!(second.next(now, now), Decision::Attempt("b".into()));
        second.admit("b", now, now)?;
        second.attempt_failed("b")?;
        assert!(matches!(second.next(now, now), Decision::Wait(_)));
        second.disarm();
        let later = now + Duration::from_secs(100);
        assert_eq!(second.next(later, later), Decision::Disarmed);
        assert!(second.admit("c", later, later).is_err());
        Ok(())
    }

    #[test]
    fn activation_requires_enabled_authorization_and_canonical_primary_membership() {
        assert!(Activation::new(FailoverPolicy::default(), "a").is_err());
        assert!(Activation::new(policy(), "primary-alias").is_err());
        let mut activation = Activation::new(policy(), "a").unwrap();
        assert!(activation.begin_episode("b").is_err());
    }

    #[test]
    fn catalog_validation_never_substitutes_aliases_or_composite_profiles() -> Result<()> {
        let mut first = crate::link::parse_link("socks://192.0.2.1:1080#First")
            .ok_or_else(|| anyhow::anyhow!("invented SOCKS fixture parses"))?;
        first.id = "a".into();
        first.alias = Some("b".into());
        let mut second = first.clone();
        second.id = "b".into();
        let mut policy = policy();
        policy.members.truncate(2);
        assert!(policy.validate_candidates([&first]).is_err());
        policy.validate_candidates([&first, &second])?;
        second.spec = OutboundSpec::XrayProfile {
            proxy_outbounds: vec![],
            balancers: vec![],
            burst_observatory: None,
            balancer_tag: "composite".into(),
        };
        assert!(policy.validate_candidates([&first, &second]).is_err());
        Ok(())
    }
}
