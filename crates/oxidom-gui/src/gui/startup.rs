use std::sync::mpsc;

use anyhow::Result;
use oxidom_core::client::DaemonClient;
use oxidom_core::config::Config;
use oxidom_core::ipc::{ProbeState, ProfileEntry, RuntimeInfo, StatusInfo};
use oxidom_core::model::Subscription;

use super::reduce::{ProbePhase, ProbeWait, SnapshotState};

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
    pub fn load_recovered(client: &DaemonClient) -> Result<Self> {
        let runtime = client.runtime_info_optional()?;
        let mut data = Self::recovered(
            InitialResults {
                subscriptions: client.subscriptions(),
                profiles: client.list_profiles(),
                status: client.status(),
                settings: client.settings_with_watchdog_support(),
                runtime: Ok(runtime.clone().unwrap_or_default()),
                geo_download_supported: client.supports_geo_download(),
                probe_cancel_supported: client.supports_probe_cancel(),
            },
            client.probe_state(),
        )?;
        data.runtime = runtime;
        Ok(data)
    }

    fn recovered(results: InitialResults, probe: Result<ProbeState>) -> Result<Self> {
        // Do not publish defaults as authoritative replacement state. All these
        // reads belong to the same immutable unique-owner client.
        results
            .subscriptions
            .as_ref()
            .map_err(|_| anyhow::anyhow!("catalog hydration failed"))?;
        results
            .profiles
            .as_ref()
            .map_err(|_| anyhow::anyhow!("profile hydration failed"))?;
        results
            .status
            .as_ref()
            .map_err(|_| anyhow::anyhow!("status hydration failed"))?;
        results
            .settings
            .as_ref()
            .map_err(|_| anyhow::anyhow!("settings hydration failed"))?;
        results
            .runtime
            .as_ref()
            .map_err(|_| anyhow::anyhow!("runtime hydration failed"))?;
        let probe = probe?;
        let mut data = Self::from(results);
        data.ui.readings = probe.readings;
        data.ui.proxied = probe.proxied;
        let now = std::time::Instant::now();
        for (ids, phase) in [
            (probe.queued, ProbePhase::Queued),
            (probe.running, ProbePhase::Running),
        ] {
            for id in ids {
                let mut wait = ProbeWait::new(now);
                wait.acked = true;
                wait.phase = phase;
                data.ui.checking.insert(id, wait);
            }
        }
        Ok(data)
    }

    pub fn load(client: &DaemonClient) -> Self {
        let runtime = client.runtime_info_optional();
        let runtime_error = runtime
            .as_ref()
            .err()
            .map(|_| "runtime unavailable".to_string());
        let runtime = runtime.ok().flatten();
        // Capability checks read cached handshake facts and never cancel work.
        let mut data = Self::from(InitialResults {
            subscriptions: client.subscriptions(),
            profiles: client.list_profiles(),
            status: client.status(),
            settings: client.settings_with_watchdog_support(),
            runtime: Ok(runtime.clone().unwrap_or_default()),
            geo_download_supported: client.supports_geo_download(),
            probe_cancel_supported: client.supports_probe_cancel(),
        });
        data.runtime = runtime;
        if let Some(error) = runtime_error {
            data.ui.poll_failed(0, error);
        }
        data
    }
}

impl From<InitialResults> for InitialData {
    fn from(results: InitialResults) -> Self {
        let poll_error = results
            .status
            .as_ref()
            .err()
            .map(|error| format!("{error:#}"))
            .or_else(|| {
                results
                    .subscriptions
                    .as_ref()
                    .err()
                    .map(|_| "catalog unavailable".into())
            })
            .or_else(|| {
                results
                    .profiles
                    .as_ref()
                    .err()
                    .map(|_| "profiles unavailable".into())
            })
            .or_else(|| {
                results
                    .settings
                    .as_ref()
                    .err()
                    .map(|_| "settings unavailable".into())
            });
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

    #[test]
    fn successor_hydration_rejects_any_missing_authoritative_field() {
        for missing in 0..6 {
            let mut fetched = results();
            fetched.runtime = Ok(RuntimeInfo::default());
            let mut probe = Ok(ProbeState::default());
            match missing {
                0 => fetched.subscriptions = Err(anyhow::anyhow!("offline")),
                1 => fetched.profiles = Err(anyhow::anyhow!("offline")),
                2 => fetched.status = Err(anyhow::anyhow!("offline")),
                3 => fetched.settings = Err(anyhow::anyhow!("offline")),
                4 => fetched.runtime = Err(anyhow::anyhow!("offline")),
                _ => probe = Err(anyhow::anyhow!("offline")),
            }
            assert!(InitialData::recovered(fetched, probe).is_err());
        }
        let mut fetched = results();
        fetched.runtime = Ok(RuntimeInfo::default());
        let probe = ProbeState {
            running: vec!["running".into()],
            queued: vec!["queued".into()],
            ..ProbeState::default()
        };
        let data = InitialData::recovered(fetched, Ok(probe)).unwrap();
        assert_eq!(data.ui.checking["running"].phase, ProbePhase::Running);
        assert_eq!(data.ui.checking["queued"].phase, ProbePhase::Queued);
        assert!(data.ui.checking.values().all(|wait| wait.acked));
    }

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
