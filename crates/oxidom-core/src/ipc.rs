//! Shared D-Bus contract between `oxidom daemon` and the GUI client.
//! Complex payloads travel as JSON strings; the structs here are the schema.

use serde::{Deserialize, Serialize};

use crate::config::LatencyMethod;
use crate::core_options::CoreOptions;
use crate::xray::core::Status;
use crate::xray::resolve::XraySource;

/// Distinct from the GUI's GApplication id (`dev.keepinfov.oxidom`), which
/// already owns that name on the session bus for single-instance activation.
pub const BUS_NAME: &str = "dev.keepinfov.oxidom.Daemon";
pub const OBJECT_PATH: &str = "/dev/keepinfov/oxidom/Daemon";
pub const INTERFACE: &str = "dev.keepinfov.oxidom1";

/// Connection state as reported by the daemon.
///
/// `serde(default)` is not cosmetic here: the GUI fetches status, probe state
/// and logs inside one closure and folds them into a single snapshot, so a
/// deserialisation error on any one of them silently freezes *all* of it. A
/// payload from a daemon of a different vintage has to degrade field by field.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StatusInfo {
    /// "disconnected" | "connecting" | "connected" | "error"
    pub state: String,
    pub error: Option<String>,
    /// Id of the server the tunnel runs for, when any.
    pub active_id: Option<String>,
    /// Name of the profile that brought the tunnel up, when it was brought up by
    /// one. `Connect` on a bare server leaves it unset, and so does a daemon
    /// older than profiles — the GUI must read "no profile", not "unknown".
    pub active_profile: Option<String>,
    /// Id of the server a failure belongs to, when the daemon knows it.
    ///
    /// Deliberately not folded into `active_id`: that one means "the tunnel is
    /// carrying X" and drives the connected highlight and the system-proxy
    /// reconciliation, neither of which may treat a server that just failed as
    /// live. The failed server is still worth naming — otherwise the card the
    /// user clicked falls back to looking merely disconnected.
    pub error_id: Option<String>,
    /// Every profile session known to the daemon, in stable profile order.
    ///
    /// The fields above remain a compatibility view of `default`, or of the
    /// first session when `default` is absent, because existing GUIs still
    /// consume only that single-session shape.
    pub sessions: Vec<SessionInfo>,
}

impl StatusInfo {
    pub fn from_status(status: &Status, active_id: Option<String>) -> Self {
        let (state, error) = match status {
            Status::Disconnected => ("disconnected", None),
            Status::Connecting => ("connecting", None),
            Status::Connected => ("connected", None),
            Status::Error(message) => ("error", Some(message.clone())),
        };
        StatusInfo {
            state: state.to_string(),
            error,
            active_id,
            active_profile: None,
            error_id: None,
            sessions: Vec::new(),
        }
    }

    /// Name the profile the tunnel was brought up by. Separate from
    /// [`Self::from_status`] for the same reason as [`Self::with_error_id`]:
    /// the status alone does not know it, and the override path — which
    /// reports a tunnel that is already down — must not claim one.
    pub fn with_active_profile(mut self, active_profile: Option<String>) -> Self {
        self.active_profile = active_profile;
        self
    }

    /// Name the server a failure belongs to. Separate from [`Self::from_status`]
    /// because only the daemon's override path knows it: by the time an error
    /// is reported the tunnel is already down and `active_server_id` cleared.
    pub fn with_error_id(mut self, error_id: Option<String>) -> Self {
        self.error_id = error_id;
        self
    }

    pub fn to_status(&self) -> Status {
        match self.state.as_str() {
            "connecting" => Status::Connecting,
            "connected" => Status::Connected,
            "error" => Status::Error(self.error.clone().unwrap_or_default()),
            _ => Status::Disconnected,
        }
    }
}

/// One running profile as exposed by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionInfo {
    pub profile: String,
    /// "disconnected" | "connecting" | "connected" | "error"
    pub state: String,
    pub error: Option<String>,
    pub server_id: Option<String>,
    pub server_alias: Option<String>,
    pub server_name: Option<String>,
    /// Loopback address shared by this session's inbounds.
    pub address: String,
    pub socks_port: u16,
    pub http_port: u16,
    /// Whether this session is the logical owner of the desktop system proxy.
    pub owns_system_proxy: bool,
    pub interface: Option<InterfaceInfo>,
    /// Additive selection detail. Legacy server fields above keep their exact
    /// meaning; a pool never fills them with an arbitrary member.
    pub selection: SelectionInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SelectionInfo {
    /// "server" | "pool"
    pub kind: String,
    /// What the user calls this pool, empty when it has no name or the
    /// selection is a single server. Cosmetic: it never selects anything.
    pub name: String,
    pub strategy: String,
    pub members: Vec<PoolMember>,
    /// How many distinct exit `address:port` pairs the members cover.
    ///
    /// A provider lists one host many times, so a pool of 42 entries is
    /// routinely nine exits counted 42 times, and "42 nodes" alone promises a
    /// spread it will not produce. Zero means "not reported" — an older daemon
    /// answers without the field — which is why nothing renders it as a count.
    pub endpoints: usize,
    pub selecting: Option<String>,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PoolMember {
    pub server_id: String,
    pub alias: Option<String>,
    pub name: String,
    pub tag: String,
    /// Whether the balancer is currently rotating through this node.
    ///
    /// Deliberately not called `healthy`. `leastLoad` filters by reachability
    /// *and* by cost, so a node missing here may be perfectly alive and merely
    /// unselected; `roundRobin` keeps unreachable nodes in the rotation, so a
    /// node present here may be dead. "In rotation" is the one thing the core
    /// actually tells us, and it is true for every strategy.
    ///
    /// There is deliberately no per-node delay beside it: the wire schema
    /// carries none, and a fabricated number would read like a measurement.
    pub in_rotation: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct InterfaceInfo {
    pub device: String,
    pub address: String,
    pub mtu: u16,
    /// "manual" | "list" | "default"
    pub routes: String,
    pub table: u32,
    pub mark: u32,
    pub up: bool,
}

/// How a probe reached the server. Mirrors `probe::Route` rather than reusing
/// it: that enum is the prober's own vocabulary and has no business growing
/// serde derives and a wire-stable spelling because the GUI wants to say where
/// a number came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeRoute {
    /// Straight at the server — what a per-server reading in the list means.
    #[default]
    Direct,
    /// Through the tunnel. Only ever valid for the active server, and the only
    /// reading that says anything about the connection the user is using.
    Proxied,
}

/// Why a probe produced no number. Carried whole so the GUI can eventually
/// distinguish "this server is down" from "this machine is offline"; phase 1
/// only ever reports [`ProbeFailure::Unreachable`] and [`ProbeFailure::Unknown`],
/// because `probe::measure` still collapses every failure into `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeFailure {
    /// The server did not answer.
    Unreachable,
    /// The attempt ran out of time.
    Timeout,
    /// The probe never left this machine.
    NoNetwork,
    /// Something went wrong that the prober could not classify.
    Unknown,
}

/// One measurement, with everything needed to judge it.
///
/// A bare `Option<u32>` said only "41" — not when, not how, not through what.
/// The GUI could then show a number taken before a reconnect as if it described
/// the tunnel now running, which is the dishonesty this type exists to end.
///
/// Invariant, upheld by the constructors: `failure.is_some()` exactly when
/// `value.is_none()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LatencyReading {
    /// Round-trip in milliseconds, when the probe produced one.
    pub value: Option<u32>,
    /// Wall clock, not `Instant`: this crosses a process boundary, where a
    /// monotonic clock reading from another process means nothing.
    pub measured_at_unix_ms: u64,
    pub route: ProbeRoute,
    /// How the number was *taken*, which is not always the method that was
    /// configured — a hysteria2 server that refuses TCP is measured by ICMP.
    /// Recording the intent here instead would make a handshake time
    /// indistinguishable from the web request the user asked for.
    pub method: LatencyMethod,
    /// Why there is no `value`. `None` exactly when `value` is `Some`.
    pub failure: Option<ProbeFailure>,
    /// What went wrong here, when this machine is what went wrong.
    ///
    /// Additive and serde-defaulted, so a daemon that sends it and a client
    /// that has never heard of it still understand each other — which is why
    /// this is a field rather than a fifth `ProbeFailure`, an enum with no
    /// `#[serde(other)]` that older clients would fail to parse outright.
    #[serde(default)]
    pub detail: Option<ProbeDetail>,
}

/// Why a probe never reached the server.
///
/// A closed set rather than free text: the wording belongs to whichever client
/// is showing it, and a string neither translates nor matches. `Other` keeps a
/// client from choking on a reason a newer daemon learned to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeDetail {
    /// No Xray binary, so nothing could be measured through anything.
    NoCore,
    /// The core would not accept the server's certificate.
    CertificateRejected,
    /// The server asks for unverified TLS, which Xray 26 removed. Only a
    /// pinned certificate can accept such a server now.
    InsecureTlsUnsupported,
    /// The core refused the configuration generated for this server.
    ConfigRefused,
    /// Something else local: no free port, nowhere to stage a config.
    #[serde(other)]
    Other,
}

impl ProbeDetail {
    /// One line, in the words a user needs. Kept beside the enum so the CLI
    /// and the GUI cannot drift into describing the same condition
    /// differently.
    pub fn message(self) -> &'static str {
        match self {
            ProbeDetail::NoCore => "no Xray core, so nothing could be measured",
            ProbeDetail::CertificateRejected => "the server's certificate was rejected",
            ProbeDetail::InsecureTlsUnsupported => {
                "the server asks for unverified TLS, which this core removed"
            }
            ProbeDetail::ConfigRefused => "the core refused the generated config",
            ProbeDetail::Other => "the check could not run on this machine",
        }
    }
}

impl LatencyReading {
    pub fn ok(value: u32, route: ProbeRoute, method: LatencyMethod) -> Self {
        LatencyReading {
            value: Some(value),
            measured_at_unix_ms: now_unix_ms(),
            route,
            method,
            failure: None,
            detail: None,
        }
    }

    pub fn failed(failure: ProbeFailure, route: ProbeRoute, method: LatencyMethod) -> Self {
        LatencyReading {
            value: None,
            measured_at_unix_ms: now_unix_ms(),
            route,
            method,
            failure: Some(failure),
            detail: None,
        }
    }

    /// The same, naming what this machine got wrong. Only failures carry one:
    /// a number needs no excuse.
    pub fn failed_locally(
        failure: ProbeFailure,
        detail: ProbeDetail,
        route: ProbeRoute,
        method: LatencyMethod,
    ) -> Self {
        LatencyReading {
            detail: Some(detail),
            ..LatencyReading::failed(failure, route, method)
        }
    }
}

/// The clock [`LatencyReading::measured_at_unix_ms`] is stamped from. Exported
/// so the GUI dates a reading against the same clock the daemon used.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

/// Current [`ProbeState`] shape. A payload carrying anything lower comes from a
/// daemon that predates the honest-reading contract, whose numbers cannot be
/// dated or attributed — see [`ProbeState::version`].
pub const PROBE_STATE_VERSION: u8 = 1;

/// Snapshot of the daemon's probe machinery for the GUI to mirror.
///
/// Running and queued are reported separately because the GUI cannot tell them
/// apart otherwise: a card waiting behind `MAX_CONCURRENT_PROBES` is not being
/// measured yet, and treating it as finished is what made a bulk re-check drop
/// every spinner on the first tick and pass the old numbers off as fresh.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProbeState {
    /// [`PROBE_STATE_VERSION`] as of the daemon that produced this. Defaults to
    /// 0, which is precisely the case worth catching: an older daemon sends the
    /// pre-contract `latencies` map, `readings` deserializes empty, and without
    /// this field the GUI would report a whole server list as unmeasured with no
    /// hint that the daemon, not the network, is the reason.
    pub version: u8,
    /// Ids being measured right now.
    pub running: Vec<String>,
    /// Ids accepted but still waiting for a slot.
    pub queued: Vec<String>,
    /// Direct measurements keyed by server id.
    pub readings: std::collections::HashMap<String, LatencyReading>,
    /// Measurements through running sessions, keyed by profile name.
    ///
    /// Kept separate from `readings`: two profiles may carry the same server,
    /// but their tunnels are still independent connections.
    pub proxied: std::collections::HashMap<String, LatencyReading>,
}

/// Result of applying settings daemon-side.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ApplySettingsResult {
    /// Set when ports changed while connected and the reconnect failed.
    pub reconnect_error: Option<String>,
    /// Human labels of ports the daemon refused to change because its service
    /// unit pins them on the command line.
    pub ignored_ports: Vec<String>,
    /// Human labels of binary paths a **system** daemon refused to change. Kept
    /// apart from `ignored_ports` because the reason is different and the GUI
    /// used to report both as "fixed by the service unit", which is true of the
    /// ports and false of the paths: a path names what the privileged daemon
    /// executes, so no caller gets to choose it. An older daemon leaves this
    /// empty and reports paths in `ignored_ports` — mislabelled, but not lost.
    pub ignored_paths: Vec<String>,
}

/// Daemon-side facts the GUI cannot work out locally: the two run in separate
/// processes, usually as different users, with different environments.
///
/// Fetched on demand (startup, after Apply) and deliberately *not* part of
/// [`StatusInfo`], which is polled twice a second — resolving the Xray binary
/// walks `$PATH` and has no business running on that tick.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RuntimeInfo {
    /// Absolute path the daemon resolved, when resolution succeeded.
    pub xray_path: Option<String>,
    /// Why resolution failed, when it did.
    pub xray_error: Option<String>,
    pub xray_source: Option<XraySource>,
    /// Ports fixed on the daemon's command line by its service unit. Config
    /// edits to them would be silently reverted on the next restart, so the
    /// daemon refuses the write and the GUI locks the row.
    pub socks_port_locked: bool,
    pub http_port_locked: bool,
    /// Set by a **system** daemon, which resolves its own core binaries. The
    /// GUI locks those rows for the same reason it locks a pinned port: an
    /// editable field that the daemon silently reverts is a field that lies.
    /// Defaults to `false`, so a session daemon and an older one both stay
    /// editable — which is correct for the session daemon and unchanged
    /// behaviour for the old one.
    pub binary_paths_locked: bool,
    /// The ports actually in use. Only meaningful when the matching
    /// `*_locked` flag is set — otherwise these default to 0 on an old daemon.
    pub socks_port: u16,
    pub http_port: u16,
}

/// One profile in a listing, flattened for CLI and other D-Bus clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProfileEntry {
    pub name: String,
    pub description: String,
    pub server: String,
    pub socks_port: u16,
    pub http_port: u16,
    /// The profile's interface section, so an editor that rewrites the profile
    /// can put back what it did not show. Without it a GUI save silently
    /// downgrades a TUN profile to proxy-only.
    pub interface: crate::profile::ProfileInterface,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<crate::pool::PoolQuery>,
    /// The profile's `[core]` section, carried for the same reason as
    /// `interface`: the GUI does not show these settings yet, and a save that
    /// dropped them would quietly un-fragment a profile that needed it.
    #[serde(default, skip_serializing_if = "CoreOptions::is_unset")]
    pub core: CoreOptions,
}

/// The selected server returned after bringing a profile up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UpServer {
    pub id: String,
    pub alias: Option<String>,
    pub name: String,
}

/// Result of applying and connecting a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UpResult {
    pub server: UpServer,
    /// Profile port names ignored because the daemon unit pins them.
    pub ignored_ports: Vec<String>,
    pub warnings: Vec<String>,
}

/// A follow-up the GUI can offer for a failure. Errors cross D-Bus as free
/// text, so the mapping lives here beside the contract rather than being
/// guessed independently on each side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorAction {
    OpenSettings,
    /// The server presented a certificate this build will not accept. The
    /// follow-up is to look at it and decide, which is a different act from
    /// editing a setting — nothing in Settings can fix this one.
    TrustCertificate,
    None,
}

/// Phrases produced by `xray::resolve` and `XrayCore::ensure_ports_free`, both
/// of which describe something the user fixes in Settings.
const SETTINGS_HINTS: &[&str] = &[
    "already in use",
    "Xray binary",
    "not found on $PATH",
    "$PATH is empty",
    // A core too old for the server's protocol is fixed by pointing oxidom at
    // a newer one, which is also a Settings field.
    "Xray 26.1",
    // No core at all: the latency check reports this without ever naming a
    // path, since it never got as far as resolving one.
    "Xray core",
];

pub fn error_action(message: &str) -> ErrorAction {
    // Checked first: "certificate" appears in a message that also names TLS,
    // and offering Settings there would send someone to a page that cannot
    // help.
    if message.contains(ProbeDetail::CertificateRejected.message())
        || message.contains(ProbeDetail::InsecureTlsUnsupported.message())
    {
        return ErrorAction::TrustCertificate;
    }
    if SETTINGS_HINTS.iter().any(|hint| message.contains(hint)) {
        ErrorAction::OpenSettings
    } else {
        ErrorAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon that predates the field sends readings without it, and those
    /// must still parse — the whole snapshot is folded in one closure, so one
    /// failed field freezes the latency of every server at once.
    #[test]
    fn a_reading_without_a_detail_still_parses() {
        let older = r#"{"value":null,"measured_at_unix_ms":1,"route":"direct",
            "method":"http_get","failure":"timeout"}"#;
        let reading: LatencyReading = serde_json::from_str(older).expect("parses");
        assert_eq!(reading.detail, None);
        assert_eq!(reading.failure, Some(ProbeFailure::Timeout));
    }

    /// And a client that predates a *reason* must not choke on one it has
    /// never heard of.
    #[test]
    fn an_unknown_detail_reads_as_something_else_local() {
        let newer = r#"{"value":null,"measured_at_unix_ms":1,"route":"direct",
            "method":"http_get","failure":"unknown","detail":"invented_later"}"#;
        let reading: LatencyReading = serde_json::from_str(newer).expect("parses");
        assert_eq!(reading.detail, Some(ProbeDetail::Other));
    }

    #[test]
    fn a_local_failure_carries_its_reason() {
        let reading = LatencyReading::failed_locally(
            ProbeFailure::Unknown,
            ProbeDetail::CertificateRejected,
            ProbeRoute::Direct,
            LatencyMethod::HttpGet,
        );
        assert_eq!(reading.value, None, "a reason is not a measurement");
        assert_eq!(reading.detail, Some(ProbeDetail::CertificateRejected));
        let round_trip: LatencyReading =
            serde_json::from_str(&serde_json::to_string(&reading).expect("serializes"))
                .expect("parses");
        assert_eq!(round_trip.detail, reading.detail);
    }

    #[test]
    fn actionable_errors_point_at_settings() {
        // Literals as produced by xray::core and xray::resolve.
        for message in [
            "local SOCKS port 10808 is already in use — pick a different port in Settings",
            "the Xray binary from Settings › Xray binary does not exist: /opt/xray",
            "the Xray binary from $OXIDOM_XRAY_BIN is not executable (mode 0644): /opt/xray",
            "`xray` was not found on $PATH (looked in /bin, /usr/bin) — install xray, or set \
             its full path in Settings",
            "cannot look up `xray` because $PATH is empty — set an absolute path in Settings",
            // The latency check, which fails before it can name a path.
            "The check needs an Xray core and could not start one on this machine.",
        ] {
            assert_eq!(
                error_action(message),
                ErrorAction::OpenSettings,
                "{message:?}"
            );
        }
    }

    /// A certificate problem has an answer, and it is not in Settings: nothing
    /// on that page accepts a certificate. Built from the messages the daemon
    /// actually sends, so rewording one cannot silently send people to a page
    /// that cannot help them.
    #[test]
    fn a_certificate_problem_offers_to_trust_rather_than_to_open_settings() {
        for detail in [
            ProbeDetail::CertificateRejected,
            ProbeDetail::InsecureTlsUnsupported,
        ] {
            assert_eq!(
                error_action(detail.message()),
                ErrorAction::TrustCertificate,
                "{}",
                detail.message()
            );
        }
        // And the reverse: a missing core is still a Settings matter, even
        // though its message names Xray too.
        assert_eq!(
            error_action("the Xray binary from Settings › Xray binary does not exist: /opt/xray"),
            ErrorAction::OpenSettings
        );
    }

    /// Built from the constant the daemon actually uses, so rewording the hint
    /// cannot quietly drop the Settings shortcut it depends on.
    #[test]
    fn an_outdated_core_points_at_settings() {
        let message = format!(
            "the core does not support this server's protocol — {}",
            crate::xray::core::HYSTERIA2_CORE_HINT
        );
        assert_eq!(
            error_action(&message),
            ErrorAction::OpenSettings,
            "{message}"
        );
    }

    /// A daemon that predates a field must not cost us the fields it does send:
    /// one missing key would otherwise fail the whole snapshot, not just itself.
    #[test]
    fn a_partial_status_payload_still_deserializes() {
        let info: StatusInfo = serde_json::from_str(r#"{"state":"connected"}"#).unwrap();
        assert!(matches!(info.to_status(), Status::Connected));
        assert_eq!(info.active_id, None);

        let empty: StatusInfo = serde_json::from_str("{}").unwrap();
        assert!(matches!(empty.to_status(), Status::Disconnected));
    }

    #[test]
    fn an_old_profile_entry_defaults_the_interface_section() {
        let entry: ProfileEntry = serde_json::from_str(
            r#"{"name":"work","server":"ch","socks_port":10808,"http_port":10809}"#,
        )
        .unwrap();
        assert_eq!(entry.interface, crate::profile::ProfileInterface::default());
        assert!(entry.pool.is_none());
    }

    /// A daemon that predates the reading contract sends `latencies` and no
    /// version. It must parse — and be recognisable as too old, since its
    /// numbers cannot be dated.
    #[test]
    fn a_pre_contract_probe_payload_is_recognisably_outdated() {
        let state: ProbeState =
            serde_json::from_str(r#"{"checking":[],"latencies":{"a":41}}"#).unwrap();
        assert_eq!(state.version, 0);
        assert!(state.readings.is_empty());
        assert!(state.proxied.is_empty());
        assert!(state.running.is_empty());
        assert!(state.queued.is_empty());

        let empty: ProbeState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.version, 0);
        assert!(empty.readings.is_empty());
        assert!(empty.proxied.is_empty());
    }

    /// A reading that predates a field still describes its measurement; only
    /// the unknown parts fall back.
    #[test]
    fn a_partial_reading_still_deserializes() {
        let reading: LatencyReading =
            serde_json::from_str(r#"{"value":41,"measured_at_unix_ms":1700000000000}"#).unwrap();
        assert_eq!(reading.value, Some(41));
        assert_eq!(reading.route, ProbeRoute::Direct);
        assert_eq!(reading.failure, None);
    }

    /// The one thing every consumer may assume: a number and a reason are
    /// mutually exclusive, so no caller has to decide which of the two it
    /// believes.
    #[test]
    fn a_reading_never_carries_both_a_number_and_a_reason() {
        let ok = LatencyReading::ok(41, ProbeRoute::Proxied, LatencyMethod::Tcp);
        assert_eq!(ok.value, Some(41));
        assert_eq!(ok.failure, None);
        assert!(ok.measured_at_unix_ms > 0, "stamped at construction");

        let failed = LatencyReading::failed(
            ProbeFailure::Unreachable,
            ProbeRoute::Direct,
            LatencyMethod::Icmp,
        );
        assert_eq!(failed.value, None);
        assert_eq!(failed.failure, Some(ProbeFailure::Unreachable));
    }

    #[test]
    fn unrelated_errors_offer_no_shortcut() {
        for message in [
            "server not found",
            "active server did not pass its latency check",
            "Xray exited unexpectedly",
        ] {
            assert_eq!(error_action(message), ErrorAction::None, "{message:?}");
        }
    }
}
