use std::sync::mpsc;

use anyhow::Result;
use oxidom_core::client::DaemonClient;
use oxidom_core::config::Config;
use oxidom_core::ipc::{ProfileEntry, RuntimeInfo, StatusInfo};
use oxidom_core::model::Subscription;

use super::reduce::SnapshotState;

/// Owned startup state, delivered once before the window accepts any actions.
pub(super) struct InitialData {
    pub subscriptions: Option<Vec<Subscription>>,
    pub profiles: Vec<ProfileEntry>,
    pub status: StatusInfo,
    pub ui: SnapshotState,
    pub config: Config,
    pub watchdog_supported: bool,
    pub runtime: Option<RuntimeInfo>,
    pub geo_download_supported: bool,
    pub probe_cancel_supported: bool,
}

struct InitialResults {
    subscriptions: Result<Vec<Subscription>>,
    profiles: Result<Vec<ProfileEntry>>,
    status: Result<StatusInfo>,
    settings: Result<(Config, bool)>,
    runtime: Result<RuntimeInfo>,
    geo_download_supported: bool,
    probe_cancel_supported: bool,
}

impl InitialData {
    pub fn load(client: &DaemonClient) -> Self {
        // Capability checks read cached handshake facts and never cancel work.
        InitialResults {
            subscriptions: client.subscriptions(),
            profiles: client.list_profiles(),
            status: client.status(),
            settings: client.settings_with_watchdog_support(),
            runtime: client.runtime_info(),
            geo_download_supported: client.supports_geo_download(),
            probe_cancel_supported: client.supports_probe_cancel(),
        }
        .into()
    }
}

impl From<InitialResults> for InitialData {
    fn from(results: InitialResults) -> Self {
        let poll_error = results
            .status
            .as_ref()
            .err()
            .map(|error| format!("{error:#}"));
        let status = results.status.unwrap_or_default();
        let mut ui = SnapshotState::new(&status);
        if let Some(error) = poll_error {
            ui.poll_failed(0, error);
        }
        let (config, watchdog_supported) = results.settings.unwrap_or_default();
        Self {
            // None must not prune GuiPrefs; Some(empty) is authoritative.
            subscriptions: results.subscriptions.ok(),
            profiles: results.profiles.unwrap_or_default(),
            status,
            ui,
            config,
            watchdog_supported,
            // Older daemons may not implement RuntimeInfo.
            runtime: results.runtime.ok(),
            geo_download_supported: results.geo_download_supported,
            probe_cancel_supported: results.probe_cancel_supported,
        }
    }
}

/// Both closures run on the worker; only a connected, hydrated pair is delivered.
pub(super) fn spawn<C: Send + 'static, D: Send + 'static>(
    connect: impl FnOnce() -> Result<C, String> + Send + 'static,
    hydrate: impl FnOnce(&C) -> D + Send + 'static,
) -> mpsc::Receiver<Result<(C, D), String>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = connect().map(|client| {
            let data = hydrate(&client);
            (client, data)
        });
        let _ = sender.send(result);
    });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use super::super::reduce::{SessionRowState, session_rows};

    fn results() -> InitialResults {
        InitialResults {
            subscriptions: Ok(Vec::new()),
            profiles: Ok(vec![ProfileEntry {
                name: "default".into(),
                ..ProfileEntry::default()
            }]),
            status: Ok(StatusInfo {
                state: "disconnected".into(),
                ..StatusInfo::default()
            }),
            settings: Ok((Config::default(), true)),
            runtime: Err(anyhow::anyhow!("older daemon")),
            geo_download_supported: true,
            probe_cancel_supported: false,
        }
    }

    #[test]
    fn connection_and_hydration_run_on_the_same_worker_before_delivery() {
        let caller = thread::current().id();
        let (connected, connection) = mpsc::channel();
        let (hydrated, hydration) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let receiver = spawn(
            move || {
                let worker = thread::current().id();
                connected.send(worker).unwrap();
                Ok(worker)
            },
            move |client| {
                hydrated.send((thread::current().id(), *client)).unwrap();
                wait.recv().unwrap();
                InitialData::from(results())
            },
        );

        let connect_thread = connection.recv().unwrap();
        let (hydrate_thread, client) = hydration.recv().unwrap();
        assert_ne!(connect_thread, caller);
        assert_ne!(hydrate_thread, caller);
        assert_eq!(hydrate_thread, connect_thread);
        assert_eq!(client, connect_thread);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        let (client, data) = receiver.recv().unwrap().unwrap();
        assert_eq!(client, connect_thread);
        assert!(data.subscriptions.as_ref().unwrap().is_empty());
        assert_eq!(data.profiles[0].name, "default");
        assert!(receiver.recv().is_err(), "startup delivers only once");
    }

    #[test]
    fn a_connection_failure_skips_hydration() {
        let hydrated = Arc::new(AtomicBool::new(false));
        let receiver = spawn(|| Err::<(), _>("connection failed".to_string()), {
            let hydrated = hydrated.clone();
            move |_| hydrated.store(true, Ordering::SeqCst)
        });
        assert_eq!(receiver.recv().unwrap().unwrap_err(), "connection failed");
        assert!(!hydrated.load(Ordering::SeqCst));
    }

    #[test]
    fn partial_failures_keep_the_catalog_and_tunnel_state_unknown() {
        let mut fetched = results();
        fetched.subscriptions = Err(anyhow::anyhow!("catalog unavailable"));
        fetched.status = Err(anyhow::anyhow!("offline").context("status failed"));
        fetched.settings = Err(anyhow::anyhow!("settings unavailable"));
        let data = InitialData::from(fetched);

        assert!(data.subscriptions.is_none(), "preferences must not prune");
        assert_eq!(data.profiles[0].name, "default");
        assert_eq!(
            data.ui.poll_error.as_deref(),
            Some("status failed: offline")
        );
        assert_eq!(data.ui.state_epoch, 0);
        let rows = session_rows(&data.profiles, &data.ui, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, SessionRowState::Unknown);
        assert!(rows[0].busy);
        assert_eq!(
            serde_json::to_value(&data.config).unwrap(),
            serde_json::to_value(Config::default()).unwrap()
        );
        assert!(!data.watchdog_supported);
        assert!(data.runtime.is_none());
        assert!(data.geo_download_supported);
        assert!(!data.probe_cancel_supported);
    }

    #[test]
    fn an_authoritative_empty_catalog_and_a_known_stopped_tunnel_stay_known() {
        let data = InitialData::from(results());
        assert!(data.subscriptions.as_ref().unwrap().is_empty());
        assert!(data.ui.poll_error.is_none());
        assert_eq!(data.status.state, "disconnected");
        let rows = session_rows(&data.profiles, &data.ui, 0);
        assert_eq!(rows[0].state, SessionRowState::Stopped);
        assert!(data.watchdog_supported);

        let mut fetched = results();
        fetched.profiles = Err(anyhow::anyhow!("profiles unavailable"));
        assert!(InitialData::from(fetched).profiles.is_empty());
    }
}
