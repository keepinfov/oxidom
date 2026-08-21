//! Core settings that are not the selection.
//!
//! Everything here was hard-coded in the generator until now: the log level,
//! what the inbounds sniff, how domains are resolved, whether outbounds are
//! multiplexed, and whether the TLS hello is fragmented. Each is a knob the
//! competing clients expose, and each is something the core already does — the
//! only thing missing was a way to ask for it.
//!
//! Two levels set them: `config.toml` for the machine and `[core]` in a profile
//! for one tunnel. `None` on a field means "not set at this level", so a profile
//! that mentions one field does not silently reset the rest. With only two
//! levels there is no need for a parallel "where did this come from" structure —
//! [`Origin::of`] derives it from the two `Option`s the caller already holds.
//!
//! Built-in defaults reproduce the config oxidom generated before this module
//! existed, byte for byte. `xray/config.rs` has a golden test that fails if that
//! ever stops being true.

/// Where a pool's balancer sends its health check, unless something says
/// otherwise.
///
/// The observatory pings this through every member and puts a node in rotation
/// only once the ping has come back, so a destination that cannot be reached
/// through an exit is a pool that carries nothing — with "0 of N nodes were in
/// rotation" as the only symptom. It used to be a constant in the generator
/// with no way to change it, which made every pool on a machine dependent on
/// one address being reachable from wherever the user is.
pub const DEFAULT_POOL_PROBE: &str = "https://connectivitycheck.gstatic.com/generate_204";

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Xray takes `concurrency` well past what it documents — 4096 and -2 are both
/// accepted by 26.3.27 — so the useful range is ours to enforce. -1 is the
/// documented "disable", and 1024 the documented ceiling.
const MAX_MUX_CONCURRENCY: i16 = 1024;

/// Everything the generator reads besides the servers themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoreOptions {
    pub log_level: Option<LogLevel>,
    pub domain_strategy: Option<DomainStrategy>,
    #[serde(skip_serializing_if = "SniffingOptions::is_unset")]
    pub sniffing: SniffingOptions,
    #[serde(skip_serializing_if = "MuxOptions::is_unset")]
    pub mux: MuxOptions,
    #[serde(skip_serializing_if = "FragmentOptions::is_unset")]
    pub fragment: FragmentOptions,
    /// `Some(vec![])` is meaningful: it turns off noises a lower level enabled.
    pub noises: Option<Vec<Noise>>,
    #[serde(skip_serializing_if = "DnsOptions::is_unset")]
    pub dns: DnsOptions,
    /// Where a pool's burst observatory sends its health check.
    ///
    /// A `[core]` option rather than a top-level key because the observatory is
    /// core configuration, and because two pools through two countries do not
    /// necessarily share a reachable destination. Deliberately not the settings'
    /// `latency_test_url`: that one is only editable while the probe method is
    /// HTTP, so reusing it would drive every pool through a URL the interface
    /// would not always let the user change.
    pub pool_probe_url: Option<String>,
}

/// An untouched section is left out of the file entirely.
///
/// Without this a profile that never mentions `[core]` still gains four empty
/// tables the moment it is rewritten, and every profile on disk changes shape
/// for a feature its owner did not ask for. The golden TOML test in this crate
/// is what catches that.
macro_rules! is_unset {
    ($type:ty) => {
        impl $type {
            pub fn is_unset(&self) -> bool {
                *self == Self::default()
            }
        }
    };
}

is_unset!(CoreOptions);
is_unset!(SniffingOptions);
is_unset!(MuxOptions);
is_unset!(FragmentOptions);
is_unset!(DnsOptions);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SniffingOptions {
    pub enabled: Option<bool>,
    pub dest_override: Option<Vec<DestOverride>>,
    /// Sniff to pick a route but hand the original address to the outbound.
    pub route_only: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MuxOptions {
    pub enabled: Option<bool>,
    pub concurrency: Option<i16>,
    pub xudp_concurrency: Option<i16>,
    pub xudp_proxy_udp_443: Option<XudpMode>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FragmentOptions {
    pub enabled: Option<bool>,
    /// `tlshello`, a count such as `1-3`, or a plain count.
    pub packets: Option<String>,
    /// Bytes per fragment: a plain number or a range. Unlike `packets` this one
    /// refuses `tlshello` — the core rejects it there.
    pub length: Option<String>,
    /// Milliseconds between fragments.
    pub interval: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsOptions {
    /// Resolver for everything the tunnel carries. Plain address or a DoH URL.
    pub server: Option<String>,
    /// Resolver consulted first for names the machine answers locally. It is
    /// keyed to `geosite:private`, so today it covers LAN names and nothing
    /// else: oxidom routes everything but private addresses through the proxy,
    /// which leaves no other class of "direct" name to key it to.
    pub direct_server: Option<String>,
    pub query_strategy: Option<QueryStrategy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Noise {
    pub kind: NoiseKind,
    pub packet: String,
    pub delay: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    #[default]
    Warning,
    Error,
    /// Named `Silent` rather than `None` so that matching on it does not read
    /// like an absent `Option`; it is still `none` on the wire and in TOML.
    #[serde(rename = "none")]
    Silent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainStrategy {
    AsIs,
    #[default]
    IpIfNonMatch,
    IpOnDemand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestOverride {
    Http,
    Tls,
    Quic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XudpMode {
    #[default]
    Reject,
    Allow,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryStrategy {
    #[default]
    UseIp,
    UseIpv4,
    UseIpv6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseKind {
    Rand,
    Str,
    Base64,
}

/// The spelling Xray reads.
///
/// Deliberately not the serde representation: TOML keys in this project are
/// snake_case throughout, while the core wants `IPIfNonMatch` and `UseIPv4`.
/// Tying the two together would make a cosmetic change to the file format a
/// silent change to the generated config — and the core accepts an unknown
/// `domainStrategy` or `queryStrategy` without a word, so nothing downstream
/// would catch it.
impl LogLevel {
    pub fn as_xray(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warning => "warning",
            LogLevel::Error => "error",
            LogLevel::Silent => "none",
        }
    }
}

impl DomainStrategy {
    pub fn as_xray(self) -> &'static str {
        match self {
            DomainStrategy::AsIs => "AsIs",
            DomainStrategy::IpIfNonMatch => "IPIfNonMatch",
            DomainStrategy::IpOnDemand => "IPOnDemand",
        }
    }
}

impl DestOverride {
    pub fn as_xray(self) -> &'static str {
        match self {
            DestOverride::Http => "http",
            DestOverride::Tls => "tls",
            DestOverride::Quic => "quic",
        }
    }
}

impl XudpMode {
    pub fn as_xray(self) -> &'static str {
        match self {
            XudpMode::Reject => "reject",
            XudpMode::Allow => "allow",
            XudpMode::Skip => "skip",
        }
    }
}

impl QueryStrategy {
    pub fn as_xray(self) -> &'static str {
        match self {
            QueryStrategy::UseIp => "UseIP",
            QueryStrategy::UseIpv4 => "UseIPv4",
            QueryStrategy::UseIpv6 => "UseIPv6",
        }
    }
}

impl NoiseKind {
    pub fn as_xray(self) -> &'static str {
        match self {
            NoiseKind::Rand => "rand",
            NoiseKind::Str => "str",
            NoiseKind::Base64 => "base64",
        }
    }
}

/// Which level a resolved value came from. Reported by `oxidom core show`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Origin {
    BuiltIn,
    Global,
    Profile,
}

impl Origin {
    pub fn of<T>(global: Option<&T>, profile: Option<&T>) -> Origin {
        match (global, profile) {
            (_, Some(_)) => Origin::Profile,
            (Some(_), None) => Origin::Global,
            (None, None) => Origin::BuiltIn,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Origin::BuiltIn => "built-in",
            Origin::Global => "global",
            Origin::Profile => "profile",
        }
    }
}

/// Core settings with every level already applied — what the generator sees.
///
/// The `Option`s that survive mean "do not emit this at all", not "unset": the
/// generated config has to stay byte-identical to the pre-`[core]` one when
/// nothing is configured, and an emitted-but-default block would break that.
///
/// `Eq` is not derived: `routing` holds arbitrary JSON, and `serde_json::Value`
/// is only `PartialEq` because it can hold a float. Nothing compares two of
/// these for equality.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCore {
    pub log_level: LogLevel,
    pub domain_strategy: DomainStrategy,
    pub sniffing: ResolvedSniffing,
    pub mux: Option<ResolvedMux>,
    pub dialer: Option<ResolvedDialer>,
    pub dns: Option<ResolvedDns>,
    /// A profile's own `routing` block, already checked by
    /// [`crate::xray::routing::validate`].
    ///
    /// [`CoreOptions::resolve`] always leaves this `None`, and that is what
    /// keeps it away from probes: a probe resolves the machine-wide `[core]`
    /// with no profile, so the only way this is ever set is
    /// `Engine::configure_core`, which a probe does not call. Do not give
    /// `CoreOptions` a `routing` field — a probe routed by the user's rules
    /// stops measuring the server it claims to.
    pub routing: Option<serde_json::Value>,
    /// Where a pool's burst observatory pings. [`DEFAULT_POOL_PROBE`] unless a
    /// level set one.
    pub pool_probe: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSniffing {
    pub enabled: bool,
    pub dest_override: Vec<DestOverride>,
    pub route_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMux {
    pub concurrency: Option<i16>,
    pub xudp_concurrency: Option<i16>,
    pub xudp_proxy_udp_443: Option<XudpMode>,
}

/// The `freedom` outbound proxy outbounds dial through.
///
/// One outbound carries both fragmentation and noises, and either one alone is
/// enough to need it — which is why it is tagged `dialer` rather than
/// `fragment`. Naming it after one of its two jobs would be a lie in the other
/// half of the cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDialer {
    pub fragment: Option<ResolvedFragment>,
    pub noises: Vec<Noise>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFragment {
    pub packets: String,
    pub length: String,
    pub interval: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDns {
    pub server: String,
    pub direct_server: Option<String>,
    pub query_strategy: QueryStrategy,
}

/// Defaults for the three fragment fields, applied when fragmentation is on but
/// a field was left out. They are the values the circumvention clients ship.
const DEFAULT_FRAGMENT_PACKETS: &str = "tlshello";
const DEFAULT_FRAGMENT_LENGTH: &str = "100-200";
const DEFAULT_FRAGMENT_INTERVAL: &str = "10-20";

/// The built-in settings, i.e. what oxidom generated before `[core]` existed.
impl Default for ResolvedCore {
    fn default() -> Self {
        CoreOptions::resolve(&CoreOptions::default(), &CoreOptions::default())
    }
}

impl CoreOptions {
    /// Profile over global over built-in.
    pub fn resolve(global: &CoreOptions, profile: &CoreOptions) -> ResolvedCore {
        let pick = |g: Option<&str>, p: Option<&str>, fallback: &str| -> String {
            p.or(g).unwrap_or(fallback).to_string()
        };

        let fragment_on = profile
            .fragment
            .enabled
            .or(global.fragment.enabled)
            .unwrap_or(false);
        let fragment = fragment_on.then(|| ResolvedFragment {
            packets: pick(
                global.fragment.packets.as_deref(),
                profile.fragment.packets.as_deref(),
                DEFAULT_FRAGMENT_PACKETS,
            ),
            length: pick(
                global.fragment.length.as_deref(),
                profile.fragment.length.as_deref(),
                DEFAULT_FRAGMENT_LENGTH,
            ),
            interval: pick(
                global.fragment.interval.as_deref(),
                profile.fragment.interval.as_deref(),
                DEFAULT_FRAGMENT_INTERVAL,
            ),
        });

        let noises = profile
            .noises
            .as_ref()
            .or(global.noises.as_ref())
            .cloned()
            .unwrap_or_default();

        // Either half is reason enough to dial through the freedom outbound;
        // neither means no outbound and no `dialerProxy` anywhere.
        let dialer = (fragment.is_some() || !noises.is_empty())
            .then_some(ResolvedDialer { fragment, noises });

        let mux_on = profile.mux.enabled.or(global.mux.enabled).unwrap_or(false);
        let mux = mux_on.then(|| ResolvedMux {
            concurrency: profile.mux.concurrency.or(global.mux.concurrency),
            xudp_concurrency: profile.mux.xudp_concurrency.or(global.mux.xudp_concurrency),
            xudp_proxy_udp_443: profile
                .mux
                .xudp_proxy_udp_443
                .or(global.mux.xudp_proxy_udp_443),
        });

        // A `direct_server` on its own would leave every non-private name
        // unresolved, so the block hangs off `server` being set.
        let dns = profile
            .dns
            .server
            .as_ref()
            .or(global.dns.server.as_ref())
            .map(|server| ResolvedDns {
                server: server.clone(),
                direct_server: profile
                    .dns
                    .direct_server
                    .as_ref()
                    .or(global.dns.direct_server.as_ref())
                    .cloned(),
                query_strategy: profile
                    .dns
                    .query_strategy
                    .or(global.dns.query_strategy)
                    .unwrap_or_default(),
            });

        ResolvedCore {
            // Profile over machine over built-in, like every other field here.
            // A value that got past `validate` — an older file, or one edited by
            // hand — falls back rather than disabling the observatory: a pool
            // with no health check puts nothing in rotation at all, which is a
            // worse answer to a bad URL than ignoring it.
            pool_probe: profile
                .pool_probe_url
                .as_deref()
                .or(global.pool_probe_url.as_deref())
                .map(str::trim)
                .filter(|url| usable_pool_probe(url))
                .unwrap_or(DEFAULT_POOL_PROBE)
                .to_string(),
            log_level: profile.log_level.or(global.log_level).unwrap_or_default(),
            domain_strategy: profile
                .domain_strategy
                .or(global.domain_strategy)
                .unwrap_or_default(),
            sniffing: ResolvedSniffing {
                enabled: profile
                    .sniffing
                    .enabled
                    .or(global.sniffing.enabled)
                    .unwrap_or(true),
                dest_override: profile
                    .sniffing
                    .dest_override
                    .as_ref()
                    .or(global.sniffing.dest_override.as_ref())
                    .cloned()
                    .unwrap_or_else(|| vec![DestOverride::Http, DestOverride::Tls]),
                route_only: profile
                    .sniffing
                    .route_only
                    .or(global.sniffing.route_only)
                    .unwrap_or(false),
            },
            mux,
            dialer,
            dns,
            // Never from here: see the field's own comment. `Engine::configure_core`
            // is the only writer, and a probe does not call it.
            routing: None,
        }
    }

    /// Reject what the core would take and then ignore, or take and then
    /// misbehave on. Measured on Xray 26.3.27 rather than read from docs:
    /// the core catches a zero minimum in a range but happily accepts a
    /// reversed one, and puts no ceiling on `concurrency` at all.
    pub fn validate(&self, section: &str) -> Result<()> {
        if let Some(concurrency) = self.mux.concurrency {
            check_concurrency(concurrency, section, "concurrency")?;
        }
        if let Some(concurrency) = self.mux.xudp_concurrency {
            check_concurrency(concurrency, section, "xudp_concurrency")?;
        }

        if let Some(packets) = &self.fragment.packets
            && packets != DEFAULT_FRAGMENT_PACKETS
        {
            check_range(packets, section, "fragment packets")?;
        }
        if let Some(length) = &self.fragment.length {
            check_range(length, section, "fragment length")?;
        }
        if let Some(interval) = &self.fragment.interval {
            check_range(interval, section, "fragment interval")?;
        }

        for noise in self.noises.iter().flatten() {
            if noise.packet.is_empty() {
                bail!("[{section}] a noise packet cannot be empty");
            }
            check_range(&noise.delay, section, "noise delay")?;
        }

        if let Some(server) = &self.dns.server
            && server.trim().is_empty()
        {
            bail!("[{section}] dns server cannot be blank");
        }
        if let Some(url) = &self.pool_probe_url
            && !url.trim().is_empty()
            && !usable_pool_probe(url.trim())
        {
            bail!(
                "[{section}] pool_probe_url must be an http or https address with a host and no \
                 credentials — the core fetches it through every pool member on a timer"
            );
        }
        if let Some(direct) = &self.dns.direct_server {
            if direct.trim().is_empty() {
                bail!("[{section}] dns direct_server cannot be blank");
            }
            if self.dns.server.is_none() {
                bail!(
                    "[{section}] dns direct_server needs dns server as well — on its own it \
                     would leave every name outside the local network unresolved"
                );
            }
        }
        Ok(())
    }
}

/// Whether a string is something the core can be told to ping.
///
/// http or https, a host, and no credentials — the last because the address is
/// written into the generated config, which is on disk and in a problem report.
fn usable_pool_probe(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
        && parsed.host().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
}

fn check_concurrency(value: i16, section: &str, field: &str) -> Result<()> {
    if value == -1 || (1..=MAX_MUX_CONCURRENCY).contains(&value) {
        return Ok(());
    }
    bail!(
        "[{section}] mux {field} must be -1 (disabled) or between 1 and {MAX_MUX_CONCURRENCY}, \
         got {value}"
    );
}

/// A plain number or `min-max`. The core rejects a zero minimum itself but
/// accepts `200-100` without complaint, and then fragments nothing.
fn check_range(value: &str, section: &str, field: &str) -> Result<()> {
    let parse = |part: &str| -> Result<u32> {
        part.parse::<u32>().map_err(|_| {
            anyhow::anyhow!(
                "[{section}] {field} must be a number or a range like \
                 \"10-20\", got {value:?}"
            )
        })
    };
    match value.split_once('-') {
        None => {
            parse(value)?;
        }
        Some((min, max)) => {
            let (min, max) = (parse(min)?, parse(max)?);
            if min > max {
                bail!(
                    "[{section}] {field} range {value:?} runs backwards — the core accepts it and then does nothing"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global_log(level: LogLevel) -> CoreOptions {
        CoreOptions {
            log_level: Some(level),
            ..CoreOptions::default()
        }
    }

    #[test]
    fn nothing_configured_resolves_to_the_config_oxidom_already_generated() {
        let resolved = CoreOptions::resolve(&CoreOptions::default(), &CoreOptions::default());

        assert_eq!(resolved.log_level, LogLevel::Warning);
        assert_eq!(resolved.domain_strategy, DomainStrategy::IpIfNonMatch);
        assert!(resolved.sniffing.enabled);
        assert_eq!(
            resolved.sniffing.dest_override,
            [DestOverride::Http, DestOverride::Tls]
        );
        assert!(!resolved.sniffing.route_only);
        // The three that must stay absent, not default: emitting any of them
        // would move bytes in a config nothing asked to change.
        assert_eq!(resolved.mux, None);
        assert_eq!(resolved.dialer, None);
        assert_eq!(resolved.dns, None);
    }

    #[test]
    fn a_profile_field_wins_and_leaves_its_neighbours_alone() {
        let global = CoreOptions {
            log_level: Some(LogLevel::Error),
            domain_strategy: Some(DomainStrategy::AsIs),
            ..CoreOptions::default()
        };
        let profile = global_log(LogLevel::Debug);

        let resolved = CoreOptions::resolve(&global, &profile);

        assert_eq!(resolved.log_level, LogLevel::Debug);
        // The profile said nothing about the strategy, so the global one stands
        // rather than falling back to built-in.
        assert_eq!(resolved.domain_strategy, DomainStrategy::AsIs);
    }

    #[test]
    fn an_empty_noise_list_in_a_profile_turns_off_what_the_machine_enabled() {
        let global = CoreOptions {
            noises: Some(vec![Noise {
                kind: NoiseKind::Rand,
                packet: "10-20".to_string(),
                delay: "10-16".to_string(),
            }]),
            ..CoreOptions::default()
        };
        let profile = CoreOptions {
            noises: Some(Vec::new()),
            ..CoreOptions::default()
        };

        // Nothing else asks for the dialer outbound, so it disappears with them.
        assert_eq!(CoreOptions::resolve(&global, &profile).dialer, None);
        assert!(
            CoreOptions::resolve(&global, &CoreOptions::default())
                .dialer
                .is_some()
        );
    }

    #[test]
    fn fragmentation_alone_is_enough_to_need_a_dialer() {
        let profile = CoreOptions {
            fragment: FragmentOptions {
                enabled: Some(true),
                ..FragmentOptions::default()
            },
            ..CoreOptions::default()
        };

        let dialer = CoreOptions::resolve(&CoreOptions::default(), &profile)
            .dialer
            .expect("fragmentation needs an outbound to dial through");
        let fragment = dialer.fragment.expect("fragment was enabled");
        assert_eq!(fragment.packets, "tlshello");
        assert!(dialer.noises.is_empty());
    }

    #[test]
    fn disabling_fragmentation_in_a_profile_drops_the_globally_set_fields() {
        let global = CoreOptions {
            fragment: FragmentOptions {
                enabled: Some(true),
                length: Some("40-60".to_string()),
                ..FragmentOptions::default()
            },
            ..CoreOptions::default()
        };
        let profile = CoreOptions {
            fragment: FragmentOptions {
                enabled: Some(false),
                ..FragmentOptions::default()
            },
            ..CoreOptions::default()
        };

        assert_eq!(CoreOptions::resolve(&global, &profile).dialer, None);
    }

    #[test]
    fn a_direct_resolver_rides_along_with_the_main_one() {
        let global = CoreOptions {
            dns: DnsOptions {
                server: Some("1.1.1.1".to_string()),
                direct_server: Some("localhost".to_string()),
                query_strategy: Some(QueryStrategy::UseIpv4),
            },
            ..CoreOptions::default()
        };

        let dns = CoreOptions::resolve(&global, &CoreOptions::default())
            .dns
            .expect("a server was set");
        assert_eq!(dns.server, "1.1.1.1");
        assert_eq!(dns.direct_server.as_deref(), Some("localhost"));
        assert_eq!(dns.query_strategy, QueryStrategy::UseIpv4);
    }

    /// The core reads these spellings and silently ignores anything else, so a
    /// rename in the TOML representation must never reach the wire.
    #[test]
    fn the_wire_spellings_are_the_ones_the_core_actually_reads() {
        assert_eq!(LogLevel::Silent.as_xray(), "none");
        assert_eq!(LogLevel::Warning.as_xray(), "warning");
        assert_eq!(DomainStrategy::IpIfNonMatch.as_xray(), "IPIfNonMatch");
        assert_eq!(DomainStrategy::IpOnDemand.as_xray(), "IPOnDemand");
        assert_eq!(DomainStrategy::AsIs.as_xray(), "AsIs");
        assert_eq!(QueryStrategy::UseIp.as_xray(), "UseIP");
        assert_eq!(QueryStrategy::UseIpv4.as_xray(), "UseIPv4");
        assert_eq!(QueryStrategy::UseIpv6.as_xray(), "UseIPv6");
        assert_eq!(DestOverride::Quic.as_xray(), "quic");
        assert_eq!(XudpMode::Reject.as_xray(), "reject");
        assert_eq!(NoiseKind::Base64.as_xray(), "base64");
    }

    /// TOML stays snake_case even where the wire does not.
    #[test]
    fn the_file_representation_is_snake_case() {
        let options = CoreOptions {
            log_level: Some(LogLevel::Silent),
            domain_strategy: Some(DomainStrategy::IpIfNonMatch),
            dns: DnsOptions {
                server: Some("1.1.1.1".to_string()),
                query_strategy: Some(QueryStrategy::UseIpv4),
                ..DnsOptions::default()
            },
            ..CoreOptions::default()
        };

        let toml = toml::to_string(&options).unwrap();
        assert!(toml.contains(r#"log_level = "none""#), "{toml}");
        assert!(
            toml.contains(r#"domain_strategy = "ip_if_non_match""#),
            "{toml}"
        );
        assert!(toml.contains(r#"query_strategy = "use_ipv4""#), "{toml}");

        let round_tripped: CoreOptions = toml::from_str(&toml).unwrap();
        assert_eq!(round_tripped, options);
    }

    #[test]
    fn an_unknown_level_is_refused_here_because_the_core_would_not_refuse_it() {
        // `xray run -test` accepts `loglevel: "loud"` and then logs at warning.
        assert!(toml::from_str::<CoreOptions>(r#"log_level = "loud""#).is_err());
        assert!(toml::from_str::<CoreOptions>(r#"domain_strategy = "whatever""#).is_err());
        assert!(
            toml::from_str::<CoreOptions>(
                r#"[dns]
query_strategy = "use_nothing""#
            )
            .is_err()
        );
    }

    #[test]
    fn a_backwards_range_is_refused_although_the_core_takes_it() {
        let options = CoreOptions {
            fragment: FragmentOptions {
                enabled: Some(true),
                length: Some("200-100".to_string()),
                ..FragmentOptions::default()
            },
            ..CoreOptions::default()
        };

        let error = options.validate("core").unwrap_err().to_string();
        assert!(error.contains("runs backwards"), "{error}");
    }

    #[test]
    fn concurrency_is_bounded_here_because_the_core_bounds_it_nowhere() {
        let bad = |value: i16| {
            CoreOptions {
                mux: MuxOptions {
                    concurrency: Some(value),
                    ..MuxOptions::default()
                },
                ..CoreOptions::default()
            }
            .validate("core")
            .is_err()
        };

        assert!(bad(0));
        assert!(bad(4096));
        assert!(bad(-2));
        assert!(!bad(-1));
        assert!(!bad(1));
        assert!(!bad(1024));
    }

    #[test]
    fn a_direct_resolver_without_a_main_one_is_refused() {
        let options = CoreOptions {
            dns: DnsOptions {
                direct_server: Some("localhost".to_string()),
                ..DnsOptions::default()
            },
            ..CoreOptions::default()
        };

        let error = options.validate("core").unwrap_err().to_string();
        assert!(error.contains("needs dns server"), "{error}");
    }

    #[test]
    fn tlshello_is_a_packet_setting_and_not_a_length() {
        let with_packets = CoreOptions {
            fragment: FragmentOptions {
                packets: Some("tlshello".to_string()),
                ..FragmentOptions::default()
            },
            ..CoreOptions::default()
        };
        assert!(with_packets.validate("core").is_ok());

        let with_length = CoreOptions {
            fragment: FragmentOptions {
                length: Some("tlshello".to_string()),
                ..FragmentOptions::default()
            },
            ..CoreOptions::default()
        };
        assert!(with_length.validate("core").is_err());
    }

    #[test]
    fn the_origin_of_a_value_needs_no_structure_of_its_own() {
        assert_eq!(Origin::of::<u8>(None, None), Origin::BuiltIn);
        assert_eq!(Origin::of(Some(&1), None), Origin::Global);
        assert_eq!(Origin::of(Some(&1), Some(&2)), Origin::Profile);
        assert_eq!(Origin::of(None, Some(&2)), Origin::Profile);
    }

    /// Two pools through two countries do not necessarily share a reachable
    /// destination, so a profile overrides the machine, like every other field.
    #[test]
    fn a_profile_chooses_its_own_pool_health_check() {
        let machine = CoreOptions {
            pool_probe_url: Some("https://machine.example/generate_204".to_string()),
            ..CoreOptions::default()
        };
        let profile = CoreOptions {
            pool_probe_url: Some("https://profile.example/generate_204".to_string()),
            ..CoreOptions::default()
        };

        assert_eq!(
            CoreOptions::resolve(&machine, &CoreOptions::default()).pool_probe,
            "https://machine.example/generate_204"
        );
        assert_eq!(
            CoreOptions::resolve(&machine, &profile).pool_probe,
            "https://profile.example/generate_204"
        );
        assert_eq!(
            CoreOptions::resolve(&CoreOptions::default(), &CoreOptions::default()).pool_probe,
            DEFAULT_POOL_PROBE,
            "unset must reproduce what the generator emitted before this was settable"
        );
    }

    /// A pool with no working health check puts nothing in rotation and carries
    /// nothing, so an unusable value is worse than no value: it falls back
    /// rather than reaching the core as written.
    #[test]
    fn an_unusable_pool_health_check_falls_back_instead_of_breaking_every_pool() {
        for bad in [
            "ftp://files.example/thing",
            "not a url at all",
            "https://user:secret@host.example/generate_204",
            "",
            "   ",
        ] {
            let options = CoreOptions {
                pool_probe_url: Some(bad.to_string()),
                ..CoreOptions::default()
            };
            assert_eq!(
                CoreOptions::resolve(&options, &CoreOptions::default()).pool_probe,
                DEFAULT_POOL_PROBE,
                "{bad:?} reached the core"
            );
        }
    }

    /// And it is refused when it is written, so the answer is a sentence rather
    /// than a setting that silently did nothing.
    #[test]
    fn a_pool_health_check_that_names_no_host_is_refused_where_it_is_written() {
        let mut options = CoreOptions {
            pool_probe_url: Some("ftp://files.example/thing".to_string()),
            ..CoreOptions::default()
        };
        let error = options.validate("core").expect_err("ftp is not fetchable");
        assert!(
            error.to_string().contains("pool_probe_url"),
            "the message does not name the field: {error}"
        );

        options.pool_probe_url = Some("https://user:secret@host.example/x".to_string());
        assert!(
            options.validate("core").is_err(),
            "credentials would be written into the generated config"
        );

        // Empty is how a level says "not this one", not a mistake.
        options.pool_probe_url = Some(String::new());
        assert!(options.validate("core").is_ok());
    }
}
