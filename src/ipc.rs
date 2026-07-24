//! Shared D-Bus contract between `oxidom daemon` and the GUI client.
//! Complex payloads travel as JSON strings; the structs here are the schema.

use serde::{Deserialize, Serialize};

use crate::xray::core::Status;

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
pub struct ApplySettingsResult {
    /// Set when ports changed while connected and the reconnect failed.
    pub reconnect_error: Option<String>,
}
