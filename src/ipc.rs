//! Shared D-Bus contract between `oxidom daemon` and the GUI client.
//! Complex payloads travel as JSON strings; the structs here are the schema.

use serde::{Deserialize, Serialize};

use crate::xray::core::Status;
use crate::xray::resolve::XraySource;

/// Distinct from the GUI's GApplication id (`dev.keepinfov.oxidom`), which
/// already owns that name on the session bus for single-instance activation.
pub const BUS_NAME: &str = "dev.keepinfov.oxidom.Daemon";
pub const OBJECT_PATH: &str = "/dev/keepinfov/oxidom/Daemon";
pub const INTERFACE: &str = "dev.keepinfov.oxidom1";

/// Connection state as reported by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StatusInfo {
    /// "disconnected" | "connecting" | "connected" | "error"
    pub state: String,
    pub error: Option<String>,
    /// Id of the server the tunnel runs for, when any.
    pub active_id: Option<String>,
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
        }
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

/// Snapshot of the daemon's probe machinery for the GUI to mirror.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProbeState {
    pub checking: Vec<String>,
    pub latencies: std::collections::HashMap<String, Option<u32>>,
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
    /// The ports actually in use. Only meaningful when the matching
    /// `*_locked` flag is set — otherwise these default to 0 on an old daemon.
    pub socks_port: u16,
    pub http_port: u16,
}

/// A follow-up the GUI can offer for a failure. Errors cross D-Bus as free
/// text, so the mapping lives here beside the contract rather than being
/// guessed independently on each side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorAction {
    OpenSettings,
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
];

pub fn error_action(message: &str) -> ErrorAction {
    if SETTINGS_HINTS.iter().any(|hint| message.contains(hint)) {
        ErrorAction::OpenSettings
    } else {
        ErrorAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        ] {
            assert_eq!(
                error_action(message),
                ErrorAction::OpenSettings,
                "{message:?}"
            );
        }
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
