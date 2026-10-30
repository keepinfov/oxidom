//! Saved-profile grants and their private, nonpersistent recovery progress.

use super::*;
use oxidom_core::failover::{Activation, Decision, FailoverPolicy};
use oxidom_core::ipc::RuntimeRetry;
use oxidom_core::profile::ProfileSelect;

pub(super) fn local_complaint(logs: &[String]) -> bool {
    core_rejected_the_protocol(logs)
        || logs
            .iter()
            .any(|line| probe::classify_complaint(line).is_some())
}

pub(super) fn eligible_confirmation(
    end: ConfirmationEnd,
    outcome: Option<&probe::ProbeOutcome>,
    logs: &[String],
) -> bool {
    end == ConfirmationEnd::Measured
        && matches!(
            outcome,
            Some(probe::ProbeOutcome::Timeout | probe::ProbeOutcome::Unreachable)
        )
        && !local_complaint(logs)
}

fn remaining_ms(deadline: Instant, now: Instant) -> u64 {
    u64::try_from(
        deadline
            .saturating_duration_since(now)
            .as_nanos()
            .div_ceil(1_000_000),
    )
    .unwrap_or(u64::MAX)
}

pub(super) struct Snapshot {
    initial: String,
    select: ProfileSelect,
    policy: FailoverPolicy,
    activation: Activation,
}

impl Snapshot {
    pub(super) fn new(
        engine: &Engine,
        profile: &Profile,
        target: &ConnectionTarget,
    ) -> Result<Option<Self>> {
        let Some(policy) = profile.failover.as_ref().filter(|policy| policy.enabled) else {
            return Ok(None);
        };
        anyhow::ensure!(
            !profile.interface.enable && profile.select.pool.is_none(),
            "failover requires a proxy-only single-server profile"
        );
        policy.validate_candidates(engine.all_servers())?;
        let initial = target
            .server_id()
            .context("failover requires a single server")?;
        Ok(Some(Self {
            initial: initial.into(),
            select: profile.select.clone(),
            policy: policy.clone(),
            activation: Activation::new(policy.clone(), initial)?,
        }))
    }

    fn matches(&self, profile: &Profile) -> bool {
        profile.select == self.select
            && profile.failover.as_ref() == Some(&self.policy)
            && !profile.interface.enable
    }
}

struct Entry {
    generation: u64,
    snapshot: Snapshot,
    subject: Option<Confirmation>,
    current: String,
    automatic: bool,
    worker: bool,
    ordinal: u32,
    backoff: Instant,
    disarmed: bool,
    confirming: bool,
}

#[derive(Default)]
pub(super) struct Activations(Mutex<HashMap<String, Entry>>);

impl Activations {
    pub(super) fn install(&self, profile: &str, generation: u64, snapshot: Snapshot) {
        let current = snapshot.initial.clone();
        oxidom_core::sync::lock(&self.0).insert(
            profile.into(),
            Entry {
                generation,
                snapshot,
                subject: None,
                current,
                automatic: false,
                worker: false,
                ordinal: 0,
                backoff: Instant::now(),
                disarmed: false,
                confirming: false,
            },
        );
    }

    pub(super) fn remove(&self, profile: &str) {
        oxidom_core::sync::lock(&self.0).remove(profile);
    }
    pub(super) fn clear(&self) {
        oxidom_core::sync::lock(&self.0).clear();
    }
    pub(super) fn owns(&self, profile: &str, generation: u64) -> bool {
        oxidom_core::sync::lock(&self.0)
            .get(profile)
            .is_some_and(|entry| entry.generation == generation)
    }
    pub(super) fn armed(&self, profile: &str) -> bool {
        oxidom_core::sync::lock(&self.0)
            .get(profile)
            .is_some_and(|entry| !entry.disarmed)
    }
    pub(super) fn disarm(&self, profile: &str) {
        if let Some(entry) = oxidom_core::sync::lock(&self.0).get_mut(profile) {
            entry.disarmed = true;
            entry.snapshot.activation.disarm();
        }
    }
    pub(super) fn reconcile_profile(&self, name: &str, profile: &Profile) {
        let mut entries = oxidom_core::sync::lock(&self.0);
        if let Some(entry) = entries.get_mut(name)
            && !entry.snapshot.matches(profile)
        {
            entry.disarmed = true;
            entry.snapshot.activation.disarm();
        }
    }
    pub(super) fn reconcile_catalog(&self, engine: &Engine) {
        for entry in oxidom_core::sync::lock(&self.0).values_mut() {
            if entry
                .snapshot
                .policy
                .validate_candidates(engine.all_servers())
                .is_err()
            {
                entry.disarmed = true;
                entry.snapshot.activation.disarm();
            }
        }
    }
    pub(super) fn bind(&self, profile: &str, target: &ConnectionTarget, subject: Confirmation) {
        if let Some(entry) = oxidom_core::sync::lock(&self.0).get_mut(profile)
            && entry.generation == subject.generation
        {
            entry.subject = Some(subject);
            entry.current = target.server_id().unwrap_or_default().into();
            entry.confirming = true;
        }
    }
    pub(super) fn success(&self, profile: &str, target: &ConnectionTarget, subject: Confirmation) {
        if let Some(entry) = oxidom_core::sync::lock(&self.0).get_mut(profile)
            && entry.matches(target, subject)
        {
            entry.confirming = false;
            if entry.automatic {
                let _ = entry.snapshot.activation.confirm_success(&entry.current);
                entry.automatic = false;
                entry.ordinal = 0;
            }
        }
    }
    pub(super) fn rebind_death(
        &self,
        profile: &str,
        target: &ConnectionTarget,
        subject: Confirmation,
    ) {
        if let Some(entry) = oxidom_core::sync::lock(&self.0).get_mut(profile)
            && entry.generation == subject.generation
            && target.server_id() == Some(entry.current.as_str())
        {
            entry.subject = Some(subject);
        }
    }
    pub(super) fn retry(
        &self,
        profile: &str,
        generation: Option<u64>,
        now: Instant,
    ) -> Option<RuntimeRetry> {
        let entries = oxidom_core::sync::lock(&self.0);
        let entry = entries
            .get(profile)
            .filter(|entry| Some(entry.generation) == generation)?;
        let decision = entry.snapshot.activation.next(now, entry.backoff);
        let ordinal = match &decision {
            Decision::Wait(_) | Decision::Attempt(_) => entry.ordinal,
            Decision::Idle | Decision::Disarmed => 0,
            _ => entry.ordinal.saturating_sub(1),
        };
        let (state, remaining_ms) = match decision {
            Decision::Wait(deadline) => ("backoff", Some(remaining_ms(deadline, now))),
            Decision::Attempt(_) => ("backoff", Some(0)),
            Decision::InFlight => ("attempting", None),
            Decision::Exhausted => ("exhausted", None),
            _ => ("idle", None),
        };
        Some(RuntimeRetry {
            state: state.into(),
            ordinal,
            remaining_ms,
        })
    }
}

impl Entry {
    fn matches(&self, target: &ConnectionTarget, subject: Confirmation) -> bool {
        self.generation == subject.generation
            && target.server_id() == Some(self.current.as_str())
            && self.subject.is_some_and(|captured| {
                (captured.generation, captured.core, captured.spawn_seq)
                    == (subject.generation, subject.core, subject.spawn_seq)
            })
    }
}

impl Shared {
    /// Called with engine ownership, never from diagnostics or catalog refresh.
    pub(super) fn failover_failure_locked(
        &self,
        _engine: &Engine,
        profile: &str,
        target: &ConnectionTarget,
        subject: Confirmation,
        eligible: bool,
        core_death: bool,
    ) -> bool {
        let mut entries = oxidom_core::sync::lock(&self.failover.0);
        let Some(entry) = entries
            .get_mut(profile)
            .filter(|entry| entry.generation == subject.generation)
        else {
            return false;
        };
        // Owning even a revoked/exhausted grant suppresses legacy infinite retries.
        if !self.generation_is_current(profile, subject.generation)
            || !entry.matches(target, subject)
            || entry.disarmed
        {
            return true;
        }
        if core_death && entry.confirming {
            // A dead child does not cancel its outstanding measurement. Wait
            // for that confirmation to finish before admitting another start.
            return true;
        }
        entry.confirming = false;
        if !eligible {
            entry.disarmed = true;
            entry.snapshot.activation.disarm();
            return true;
        }
        if entry.automatic {
            if entry
                .snapshot
                .activation
                .attempt_failed(&entry.current)
                .is_err()
            {
                return true;
            }
            entry.automatic = false;
        } else if entry
            .snapshot
            .activation
            .begin_episode(&entry.current)
            .is_err()
        {
            return true;
        }
        entry.backoff = Instant::now() + reconnect_delay(entry.ordinal);
        if entry.worker {
            return true;
        }
        entry.worker = true;
        let generation = entry.generation;
        drop(entries);
        let shared = self.clone();
        let name = profile.to_string();
        // The worker rechecks persisted authorization under the same engine lock
        // immediately before every admission; this observation itself grants none.
        self.spawn(move || shared.run_failover(name, generation));
        true
    }

    pub(super) fn admit_failover_locked(
        &self,
        engine: &Engine,
        profile: &str,
        target: &ConnectionTarget,
        generation: u64,
    ) -> bool {
        let mut entries = oxidom_core::sync::lock(&self.failover.0);
        let Some(entry) = entries
            .get_mut(profile)
            .filter(|entry| entry.generation == generation && !entry.disarmed)
        else {
            return false;
        };
        let valid = profile::load(profile).is_ok_and(|profile| {
            profile.validate("activation").is_ok() && entry.snapshot.matches(&profile)
        }) && entry
            .snapshot
            .policy
            .validate_candidates(engine.all_servers())
            .is_ok();
        if !valid {
            entry.disarmed = true;
            entry.snapshot.activation.disarm();
            return false;
        }
        let Some(id) = target.server_id() else {
            return false;
        };
        if entry
            .snapshot
            .activation
            .admit(id, Instant::now(), entry.backoff)
            .is_err()
        {
            return false;
        }
        entry.automatic = true;
        entry.current = id.into();
        entry.subject = None;
        entry.ordinal += 1;
        true
    }

    fn run_failover(&self, profile: String, generation: u64) {
        loop {
            let decision = {
                let _engine = oxidom_core::sync::lock(&self.engine);
                let mut entries = oxidom_core::sync::lock(&self.failover.0);
                let Some(entry) = entries
                    .get_mut(&profile)
                    .filter(|entry| entry.generation == generation)
                else {
                    return;
                };
                if !self.generation_is_current(&profile, generation) {
                    return;
                }
                let decision = entry
                    .snapshot
                    .activation
                    .next(Instant::now(), entry.backoff);
                if matches!(
                    decision,
                    Decision::Idle | Decision::Exhausted | Decision::Disarmed
                ) {
                    entry.worker = false;
                    return;
                }
                decision
            };
            match decision {
                Decision::Attempt(id) => {
                    match self.start_connection_target_captured(
                        &profile,
                        &ConnectionTarget::Server(id),
                        generation,
                        ConnectionOrigin::Failover,
                    ) {
                        Ok(Some((receipt, _))) => {
                            drop(receipt);
                        }
                        Ok(None) => {
                            return;
                        }
                        Err(_) => {
                            // Startup errors are untyped local/configuration failures.
                            // Their admission consumed budget, but cannot advance it.
                            let _engine = oxidom_core::sync::lock(&self.engine);
                            if self.generation_is_current(&profile, generation) {
                                self.failover.disarm(&profile);
                            }
                        }
                    }
                }
                Decision::Wait(_) | Decision::InFlight => {
                    // Bound cancellation latency without holding application locks.
                    if !self.lifecycle.wait(Duration::from_millis(100)) {
                        return;
                    }
                }
                _ => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_activation(
        automatic: bool,
    ) -> Result<(
        super::super::tests::TestRoot,
        Service,
        ConnectionTarget,
        Confirmation,
    )> {
        let root = super::super::tests::TestRoot::install("failover-confirmation-seam")?;
        let service = super::super::for_test();
        super::super::tests::configure_fake_core(&service, &root)?;
        let ids = oxidom_core::sync::lock(&service.shared.engine)
            .all_servers()
            .map(|server| server.id.clone())
            .collect::<Vec<_>>();
        let profile = Profile {
            select: ProfileSelect {
                server: ids[0].clone(),
                pool: None,
            },
            failover: Some(FailoverPolicy {
                enabled: true,
                members: ids.clone(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let id = &ids[usize::from(automatic)];
        let subject =
            super::super::tests::start_attributed_fake_session(&service, "default", id, None)?;
        let target = ConnectionTarget::Server(id.clone());
        let engine = oxidom_core::sync::lock(&service.shared.engine);
        let snapshot =
            Snapshot::new(&engine, &profile, &ConnectionTarget::Server(ids[0].clone()))?.unwrap();
        service
            .shared
            .failover
            .install("default", subject.generation, snapshot);
        {
            let mut entries = oxidom_core::sync::lock(&service.shared.failover.0);
            let entry = entries.get_mut("default").unwrap();
            // Keep scheduling deterministic: tests drive admissions themselves.
            entry.worker = true;
            if automatic {
                let now = Instant::now();
                entry.snapshot.activation.begin_episode(&ids[0])?;
                entry.snapshot.activation.admit(id, now, now)?;
                entry.automatic = true;
                entry.ordinal = 1;
            }
        }
        service.shared.failover.bind("default", &target, subject);
        drop(engine);
        Ok((root, service, target, subject))
    }

    fn kill_current(service: &Service) -> Result<()> {
        let mut engine = oxidom_core::sync::lock(&service.shared.engine);
        let session = engine
            .sessions
            .get_mut("default")
            .context("default session")?;
        let pid = session.core.child_pid().context("fake child")?;
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGKILL)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while session.is_alive() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        anyhow::ensure!(!session.is_alive(), "fake child must have exited");
        Ok(())
    }

    #[test]
    fn a_reachable_receipt_with_a_dead_core_recovers_before_or_after_the_death_sweep() -> Result<()>
    {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        for automatic in [false, true] {
            for sweep_first in [false, true] {
                let (_root, service, target, subject) = live_activation(automatic)?;
                kill_current(&service)?;
                if sweep_first {
                    service.shared.supervise_once();
                }
                assert!(!service.shared.apply_confirmation_success(
                    "default",
                    &target,
                    ConnectionOrigin::Failover,
                    subject
                ));
                if !sweep_first {
                    service.shared.supervise_once();
                }
                assert!(service.shared.failover.armed("default"));
                let entries = oxidom_core::sync::lock(&service.shared.failover.0);
                let entry = entries.get("default").unwrap();
                assert!(!entry.confirming);
                assert!(!entry.automatic);
                assert!(matches!(
                    entry
                        .snapshot
                        .activation
                        .next(Instant::now(), entry.backoff),
                    Decision::Wait(_) | Decision::Exhausted
                ));
                drop(entries);
                service.shared.terminate()?;
            }
        }
        Ok(())
    }

    #[test]
    fn local_confirmation_evidence_wins_over_death_detection_in_either_order() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let cases = [
            (
                ConfirmationEnd::InboundNotReady,
                probe::ProbeOutcome::Timeout,
                None,
            ),
            (
                ConfirmationEnd::Measured,
                probe::ProbeOutcome::NoNetwork,
                None,
            ),
            (
                ConfirmationEnd::Measured,
                probe::ProbeOutcome::Internal(ProbeDetail::Other),
                None,
            ),
            (
                ConfirmationEnd::Measured,
                probe::ProbeOutcome::Timeout,
                Some("failed to build configuration"),
            ),
            (
                ConfirmationEnd::Measured,
                probe::ProbeOutcome::Unreachable,
                Some("failed to open file: geoip.dat"),
            ),
            (
                ConfirmationEnd::Measured,
                probe::ProbeOutcome::Timeout,
                Some("x509: certificate rejected"),
            ),
            (
                ConfirmationEnd::Measured,
                probe::ProbeOutcome::Timeout,
                Some("unknown protocol"),
            ),
        ];
        for (end, outcome, complaint) in cases {
            for sweep_first in [false, true] {
                let (_root, service, target, subject) = live_activation(false)?;
                if let Some(line) = complaint {
                    oxidom_core::logbook::global().push(
                        oxidom_core::logbook::LogSource::Xray,
                        oxidom_core::logbook::Severity::Error,
                        Some("default"),
                        "test",
                        line.to_string(),
                    );
                }
                kill_current(&service)?;
                if sweep_first {
                    service.shared.supervise_once();
                }
                service.shared.apply_confirmation_failure(
                    "default",
                    &target,
                    ConnectionOrigin::Explicit,
                    subject,
                    end,
                    Some(&outcome),
                );
                if !sweep_first {
                    service.shared.supervise_once();
                }
                assert!(!service.shared.failover.armed("default"));
                assert_eq!(
                    service
                        .shared
                        .failover
                        .retry("default", Some(subject.generation), Instant::now())
                        .unwrap()
                        .ordinal,
                    0
                );
                service.shared.terminate()?;
            }
        }
        Ok(())
    }

    #[test]
    fn released_default_failover_never_opens_direct_standby_until_explicit_stop() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let (_root, service, target, subject) = live_activation(false)?;
        {
            let mut engine = oxidom_core::sync::lock(&service.shared.engine);
            engine.registry.config.standby_proxy = true;
            engine.registry.config.reconnect = false;
            engine
                .configure_hold_traffic("default", Some(oxidom_core::config::OnCoreExit::Release));
        }
        kill_current(&service)?;
        service.shared.supervise_once();
        service.shared.apply_confirmation_failure(
            "default",
            &target,
            ConnectionOrigin::Explicit,
            subject,
            ConfirmationEnd::Measured,
            Some(&probe::ProbeOutcome::Timeout),
        );
        service.shared.reconcile_standby();
        assert!(oxidom_core::sync::lock(&service.shared.standby).is_none());
        {
            let mut entries = oxidom_core::sync::lock(&service.shared.failover.0);
            let entry = entries.get_mut("default").unwrap();
            let now = Instant::now() + Duration::from_secs(3600);
            let Decision::Attempt(id) = entry.snapshot.activation.next(now, now) else {
                anyhow::bail!("alternate must remain");
            };
            entry.snapshot.activation.admit(&id, now, now)?;
        }
        service.shared.reconcile_standby();
        assert!(oxidom_core::sync::lock(&service.shared.standby).is_none());
        {
            let mut entries = oxidom_core::sync::lock(&service.shared.failover.0);
            let entry = entries.get_mut("default").unwrap();
            let id = entry
                .snapshot
                .policy
                .members
                .iter()
                .find(|id| **id != entry.current)
                .unwrap()
                .clone();
            entry.snapshot.activation.attempt_failed(&id)?;
            assert_eq!(
                entry
                    .snapshot
                    .activation
                    .next(Instant::now(), entry.backoff),
                Decision::Exhausted
            );
        }
        service.shared.reconcile_standby();
        assert!(oxidom_core::sync::lock(&service.shared.standby).is_none());
        service.stop_profile("default")?;
        assert!(oxidom_core::sync::lock(&service.shared.standby).is_some());
        service.shared.terminate()?;
        Ok(())
    }

    #[test]
    fn retry_ordinals_are_zero_based_and_positive_fractional_milliseconds_round_up() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("failover-retry-projection");
        let (engine, profile) = fixture()?;
        let activations = Activations::default();
        activations.install(
            "work",
            1,
            Snapshot::new(
                &engine,
                &profile,
                &ConnectionTarget::Server(profile.select.server.clone()),
            )?
            .unwrap(),
        );
        let now = Instant::now();
        {
            let mut entries = oxidom_core::sync::lock(&activations.0);
            let entry = entries.get_mut("work").unwrap();
            entry.snapshot.activation.begin_episode(&entry.current)?;
            entry.backoff = now + Duration::from_nanos(1);
        }
        let retry = activations.retry("work", Some(1), now).unwrap();
        assert_eq!(retry.ordinal, 0);
        assert_eq!(retry.remaining_ms, Some(1));
        {
            let mut entries = oxidom_core::sync::lock(&activations.0);
            let entry = entries.get_mut("work").unwrap();
            let id = entry.snapshot.policy.members[1].clone();
            entry
                .snapshot
                .activation
                .admit(&id, now + Duration::from_nanos(1), entry.backoff)?;
            entry.ordinal = 1;
        }
        assert_eq!(activations.retry("work", Some(1), now).unwrap().ordinal, 0);
        {
            let mut entries = oxidom_core::sync::lock(&activations.0);
            let entry = entries.get_mut("work").unwrap();
            // Projection of later admitted attempts uses their zero-based index.
            entry.ordinal = 2;
        }
        assert_eq!(activations.retry("work", Some(1), now).unwrap().ordinal, 1);
        {
            let mut entries = oxidom_core::sync::lock(&activations.0);
            let entry = entries.get_mut("work").unwrap();
            let id = entry.snapshot.policy.members[1].clone();
            entry.snapshot.activation.attempt_failed(&id)?;
        }
        let exhausted = activations.retry("work", Some(1), now).unwrap();
        assert_eq!(exhausted.state, "exhausted");
        assert_eq!(exhausted.ordinal, 1);
        activations.disarm("work");
        let idle = activations.retry("work", Some(1), now).unwrap();
        assert_eq!(idle.state, "idle");
        assert_eq!(idle.ordinal, 0);
        assert_eq!(idle.remaining_ms, None);
        assert_eq!(remaining_ms(now, now), 0);
        assert_eq!(remaining_ms(now + Duration::from_nanos(1_000_001), now), 2);
        Ok(())
    }

    struct TestRoot(std::path::PathBuf);

    impl TestRoot {
        fn install(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "oxidom-daemon-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            oxidom_core::paths::set_test_root(Some(path.clone()));
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            oxidom_core::paths::set_test_root(None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn subject(generation: u64, core: u64) -> Confirmation {
        Confirmation {
            generation,
            core: Some(core),
            spawn_seq: core,
            context: None,
            inbound: None,
        }
    }

    fn fixture() -> Result<(Engine, Profile)> {
        let mut engine = Engine::load();
        engine.import_links("socks://192.0.2.1:1080#First\nsocks://192.0.2.2:1080#Second")?;
        let ids = engine
            .all_servers()
            .map(|server| server.id.clone())
            .collect::<Vec<_>>();
        let profile = Profile {
            select: ProfileSelect {
                server: ids[0].clone(),
                pool: None,
            },
            failover: Some(FailoverPolicy {
                enabled: true,
                members: ids,
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok((engine, profile))
    }

    #[test]
    fn absent_or_disabled_authorization_never_creates_an_activation() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("failover-no-grant");
        let (engine, mut profile) = fixture()?;
        let target = ConnectionTarget::Server(profile.select.server.clone());
        profile.failover = None;
        assert!(Snapshot::new(&engine, &profile, &target)?.is_none());
        profile.failover = Some(FailoverPolicy::default());
        assert!(Snapshot::new(&engine, &profile, &target)?.is_none());
        Ok(())
    }

    #[test]
    fn revoked_or_removed_catalog_members_never_rearm_after_readdition() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("failover-readdition");
        let (mut engine, profile) = fixture()?;
        let target = ConnectionTarget::Server(profile.select.server.clone());
        let activations = Activations::default();
        activations.install(
            "work",
            1,
            Snapshot::new(&engine, &profile, &target)?.unwrap(),
        );
        let subscriptions = engine.registry.subscriptions.clone();
        engine.registry.subscriptions.clear();
        activations.reconcile_catalog(&engine);
        engine.registry.subscriptions = subscriptions;
        activations.reconcile_catalog(&engine);
        assert!(!activations.armed("work"));
        activations.reconcile_profile("work", &profile);
        assert!(!activations.armed("work"));
        assert!(activations.owns("work", 1));
        Ok(())
    }

    #[test]
    fn only_the_current_core_can_reset_an_automatic_episode() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("failover-current-core");
        let (engine, profile) = fixture()?;
        let ids = &profile.failover.as_ref().unwrap().members;
        let activations = Activations::default();
        let primary = ConnectionTarget::Server(ids[0].clone());
        let alternate = ConnectionTarget::Server(ids[1].clone());
        activations.install(
            "work",
            2,
            Snapshot::new(&engine, &profile, &primary)?.unwrap(),
        );
        let now = Instant::now();
        {
            let mut entries = oxidom_core::sync::lock(&activations.0);
            let entry = entries.get_mut("work").unwrap();
            entry.snapshot.activation.begin_episode(&ids[0])?;
            entry.snapshot.activation.admit(&ids[1], now, now)?;
            entry.automatic = true;
        }
        activations.bind("work", &alternate, subject(2, 3));
        activations.success("work", &alternate, subject(1, 3));
        activations.success("work", &alternate, subject(2, 2));
        assert_eq!(
            activations.retry("work", Some(2), now).unwrap().state,
            "attempting"
        );
        activations.success("work", &alternate, subject(2, 3));
        assert_eq!(
            activations.retry("work", Some(2), now).unwrap().state,
            "idle"
        );
        Ok(())
    }

    #[test]
    fn external_revocation_blocks_admission_and_temporary_replacement_drops_the_grant() -> Result<()>
    {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("failover-external-revoke");
        let (engine, profile) = fixture()?;
        profile::save("work", &profile)?;
        let primary = ConnectionTarget::Server(profile.select.server.clone());
        let alternate =
            ConnectionTarget::Server(profile.failover.as_ref().unwrap().members[1].clone());
        let snapshot = Snapshot::new(&engine, &profile, &primary)?.unwrap();
        let shared = Shared::new(engine, false, false, false);
        let generation = shared.next_connect_generation("work");
        shared.failover.install("work", generation, snapshot);
        {
            let mut entries = oxidom_core::sync::lock(&shared.failover.0);
            let entry = entries.get_mut("work").unwrap();
            entry.snapshot.activation.begin_episode(&entry.current)?;
            entry.backoff = Instant::now();
        }
        let mut revoked = profile.clone();
        revoked.failover = None;
        profile::save("work", &revoked)?;
        {
            let engine = oxidom_core::sync::lock(&shared.engine);
            assert!(!shared.admit_failover_locked(&engine, "work", &alternate, generation));
        }
        profile::save("work", &profile)?;
        assert!(!shared.failover.armed("work"));
        shared.next_connect_generation("work");
        assert!(!shared.failover.owns("work", generation));
        assert!(!shared.failover.armed("work"));
        Ok(())
    }

    #[test]
    fn legacy_saves_preserve_only_unchanged_selection_and_null_revokes() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("failover-legacy-save");
        let (engine, profile) = fixture()?;
        profile::save("work", &profile)?;
        let service = Service {
            shared: Shared::new(engine, false, false, false),
        };
        let mut raw = serde_json::to_value(&profile)?;
        raw.as_object_mut().unwrap().remove("failover");
        service.save_profile("work".into(), raw.to_string())?;
        assert_eq!(profile::load("work")?.failover, profile.failover);
        raw["select"]["server"] = "different-canonical-id".into();
        service.save_profile("work".into(), raw.to_string())?;
        assert!(profile::load("work")?.failover.is_none());
        profile::save("work", &profile)?;
        raw = serde_json::to_value(&profile)?;
        raw["failover"] = serde_json::Value::Null;
        service.save_profile("work".into(), raw.to_string())?;
        assert!(profile::load("work")?.failover.is_none());
        Ok(())
    }

    #[test]
    fn local_confirmation_failures_disarm_without_admitting_an_alternate() -> Result<()> {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("failover-local-failure");
        let (engine, profile) = fixture()?;
        let target = ConnectionTarget::Server(profile.select.server.clone());
        let snapshot = Snapshot::new(&engine, &profile, &target)?.unwrap();
        let shared = Shared::new(engine, false, false, false);
        let generation = shared.next_connect_generation("work");
        let subject = subject(generation, 1);
        {
            let engine = oxidom_core::sync::lock(&shared.engine);
            assert!(
                !shared.failover_failure_locked(&engine, "work", &target, subject, true, false)
            );
            shared.failover.install("work", generation, snapshot);
            shared.failover.bind("work", &target, subject);
            assert!(
                shared.failover_failure_locked(&engine, "work", &target, subject, false, false)
            );
        }
        assert!(!shared.failover.armed("work"));
        assert_eq!(
            shared
                .failover
                .retry("work", Some(generation), Instant::now())
                .unwrap()
                .ordinal,
            0
        );
        Ok(())
    }

    #[test]
    fn automatic_admission_is_single_flight_and_cancellation_survives_a_late_success() -> Result<()>
    {
        let _guard = oxidom_core::sync::lock(&oxidom_core::paths::TEST_ROOT_LOCK);
        let _root = TestRoot::install("failover-single-flight");
        let (engine, profile) = fixture()?;
        profile::save("work", &profile)?;
        let primary = ConnectionTarget::Server(profile.select.server.clone());
        let alternate =
            ConnectionTarget::Server(profile.failover.as_ref().unwrap().members[1].clone());
        let snapshot = Snapshot::new(&engine, &profile, &primary)?.unwrap();
        let shared = Shared::new(engine, false, false, false);
        let generation = shared.next_connect_generation("work");
        shared.failover.install("work", generation, snapshot);
        {
            let mut entries = oxidom_core::sync::lock(&shared.failover.0);
            let entry = entries.get_mut("work").unwrap();
            entry.snapshot.activation.begin_episode(&entry.current)?;
            entry.backoff = Instant::now();
        }
        {
            let engine = oxidom_core::sync::lock(&shared.engine);
            assert!(shared.admit_failover_locked(&engine, "work", &alternate, generation));
            assert!(!shared.admit_failover_locked(&engine, "work", &alternate, generation));
        }
        let subject = subject(generation, 4);
        shared.failover.bind("work", &alternate, subject);
        shared.failover.disarm("work");
        shared.failover.success("work", &alternate, subject);
        assert!(!shared.failover.armed("work"));
        {
            let engine = oxidom_core::sync::lock(&shared.engine);
            assert!(!shared.admit_failover_locked(&engine, "work", &primary, generation));
        }
        shared.invalidate_generation("work");
        assert!(!shared.failover.owns("work", generation));
        Ok(())
    }
}
