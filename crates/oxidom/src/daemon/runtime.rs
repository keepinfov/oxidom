//! Application-owned observations and retry episodes. Diagnostics are not a source.

use super::*;
use oxidom_core::ipc::{RuntimeHealth, RuntimeRetry};

#[derive(Default)]
pub(super) struct RuntimeFacts {
    health: HashMap<String, (Confirmation, RuntimeHealth)>,
    retry: HashMap<String, RetryEpisode>,
}

struct RetryEpisode {
    generation: u64,
    ordinal: u32,
    deadline: Option<Instant>,
    retry_allowed: bool,
}

impl RuntimeFacts {
    pub(super) fn reset(&mut self, profile: &str) {
        self.health.remove(profile);
        self.retry.remove(profile);
    }

    pub(super) fn clear(&mut self) {
        self.health.clear();
        self.retry.clear();
    }

    pub(super) fn clear_health(&mut self, profile: &str) {
        self.health.remove(profile);
    }

    pub(super) fn observe(
        &mut self,
        profile: &str,
        subject: Confirmation,
        outcome: &probe::ProbeOutcome,
        source: &str,
        measured_at: String,
    ) {
        let outcome = match outcome {
            probe::ProbeOutcome::Reachable(_) => "reachable",
            probe::ProbeOutcome::Unreachable => "unreachable",
            probe::ProbeOutcome::Timeout => "timeout",
            probe::ProbeOutcome::NoNetwork => "no_network",
            probe::ProbeOutcome::Internal(_) => "internal",
        };
        self.health.insert(
            profile.to_string(),
            (
                subject,
                RuntimeHealth {
                    outcome: outcome.to_string(),
                    source: source.to_string(),
                    measured_at,
                },
            ),
        );
    }

    pub(super) fn health(
        &self,
        profile: &str,
        subject: Option<Confirmation>,
    ) -> Option<RuntimeHealth> {
        let subject = subject?;
        self.health
            .get(profile)
            .filter(|(captured, _)| {
                (captured.generation, captured.core, captured.spawn_seq)
                    == (subject.generation, subject.core, subject.spawn_seq)
            })
            .map(|(_, health)| health.clone())
    }

    pub(super) fn backoff(&mut self, profile: &str, generation: u64, ordinal: u32, now: Instant) {
        self.retry.insert(
            profile.to_string(),
            RetryEpisode {
                generation,
                ordinal,
                deadline: Some(now + reconnect_delay(ordinal)),
                retry_allowed: true,
            },
        );
    }

    pub(super) fn attempting(&mut self, profile: &str, generation: u64) -> bool {
        if let Some(episode) = self.retry.get_mut(profile).filter(|episode| {
            episode.generation == generation && episode.deadline.is_some() && episode.retry_allowed
        }) {
            episode.deadline = None;
            return true;
        }
        false
    }

    pub(super) fn has_retry(&self, profile: &str, generation: u64) -> bool {
        self.retry
            .get(profile)
            .is_some_and(|episode| episode.generation == generation)
    }

    pub(super) fn retry_allowed(&self, profile: &str, generation: u64) -> bool {
        self.retry
            .get(profile)
            .is_some_and(|episode| episode.generation == generation && episode.retry_allowed)
    }

    pub(super) fn disable_attempt(&mut self, profile: &str, generation: u64) -> bool {
        if let Some(episode) = self
            .retry
            .get_mut(profile)
            .filter(|episode| episode.generation == generation && episode.deadline.is_none())
        {
            episode.retry_allowed = false;
            return true;
        }
        false
    }

    pub(super) fn idle(&mut self, profile: &str, generation: u64) {
        if self
            .retry
            .get(profile)
            .is_some_and(|episode| episode.generation == generation)
        {
            self.retry.remove(profile);
        }
    }

    pub(super) fn wait(&self, profile: &str, generation: u64, now: Instant) -> Option<Duration> {
        self.retry
            .get(profile)
            .filter(|episode| episode.generation == generation)
            .and_then(|episode| episode.deadline)
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    pub(super) fn retry(
        &self,
        profile: &str,
        generation: Option<u64>,
        now: Instant,
    ) -> RuntimeRetry {
        match self
            .retry
            .get(profile)
            .filter(|episode| Some(episode.generation) == generation)
        {
            Some(episode) => RuntimeRetry {
                state: if episode.deadline.is_some() {
                    "backoff"
                } else {
                    "attempting"
                }
                .to_string(),
                ordinal: episode.ordinal,
                remaining_ms: episode.deadline.map(|deadline| remaining_ms(deadline, now)),
            },
            None => RuntimeRetry {
                state: "idle".to_string(),
                ..RuntimeRetry::default()
            },
        }
    }
}

fn remaining_ms(deadline: Instant, now: Instant) -> u64 {
    // Round up so a positive sub-millisecond wait is not reported as expired.
    u64::try_from(
        deadline
            .saturating_duration_since(now)
            .as_nanos()
            .div_ceil(1_000_000),
    )
    .unwrap_or(u64::MAX)
}

pub(super) fn phase(status: &Status, confirmation_pending: bool) -> &'static str {
    match status {
        Status::Disconnected => "stopped",
        Status::Connecting => "starting",
        Status::Connected if confirmation_pending => "verifying",
        Status::Connected => "running",
        Status::Error(_) => "failed",
    }
}

pub(super) fn measured_at() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_utc(seconds)
}

fn format_utc(seconds: u64) -> String {
    // Gregorian civil date conversion, with the same UTC convention as logbook.
    let z = (seconds / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    let rest = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(generation: u64, core: u64) -> Confirmation {
        Confirmation {
            generation,
            core: Some(core),
            spawn_seq: core,
            context: None,
            inbound: None,
        }
    }

    #[test]
    fn disabling_an_attempt_allows_its_receipt_but_never_rearms_it() {
        let mut facts = RuntimeFacts::default();
        let now = Instant::now();
        facts.backoff("work", 1, 0, now);
        assert!(facts.attempting("work", 1));
        assert!(facts.disable_attempt("work", 1));
        assert!(facts.has_retry("work", 1));
        assert!(!facts.retry_allowed("work", 1));
        assert!(!facts.attempting("work", 1));
        assert!(!facts.disable_attempt("work", 2));
        facts.idle("work", 1);
        assert!(!facts.has_retry("work", 1));
        facts.backoff("work", 2, 0, now);
        assert!(facts.retry_allowed("work", 2));
    }

    #[test]
    fn a_cancelled_or_already_claimed_episode_refuses_an_attempt() {
        let mut facts = RuntimeFacts::default();
        let now = Instant::now();
        assert!(!facts.attempting("work", 1));
        facts.backoff("work", 1, 0, now);
        assert!(!facts.attempting("work", 2));
        assert!(facts.attempting("work", 1));
        assert!(!facts.attempting("work", 1));
        facts.idle("work", 1);
        assert!(!facts.attempting("work", 1));
    }

    #[test]
    fn health_belongs_to_one_generation_and_core_without_diagnostics() {
        let mut facts = RuntimeFacts::default();
        let captured = subject(1, 2);
        facts.observe(
            "work",
            captured,
            &probe::ProbeOutcome::Timeout,
            "active",
            "2026-10-28T00:00:00Z".into(),
        );
        assert_eq!(
            facts.health("work", Some(captured)).unwrap().outcome,
            "timeout"
        );
        assert!(facts.health("work", Some(subject(2, 2))).is_none());
        assert!(facts.health("work", Some(subject(1, 3))).is_none());
        assert!(facts.health("other", Some(captured)).is_none());
        assert!(facts.health("work", None).is_none());
        facts.reset("work");
        assert!(facts.health("work", Some(captured)).is_none());
    }

    #[test]
    fn retry_deadlines_are_monotonic_and_stale_cancellation_cannot_cancel_a_replacement() {
        let mut facts = RuntimeFacts::default();
        let now = Instant::now();
        facts.backoff("work", 2, 1, now);
        assert_eq!(
            facts.retry("work", Some(2), now).remaining_ms,
            Some(reconnect_delay(1).as_millis() as u64)
        );
        assert_eq!(facts.retry("work", Some(1), now).state, "idle");
        assert_eq!(facts.wait("work", 2, now), Some(reconnect_delay(1)));
        assert_eq!(facts.wait("work", 1, now), None);
        assert_eq!(
            facts
                .retry("work", Some(2), now + reconnect_delay(1))
                .remaining_ms,
            Some(0)
        );
        assert_eq!(
            facts.retry("work", Some(2), now + reconnect_delay(1)).state,
            "backoff"
        );
        facts.idle("work", 1);
        facts.attempting("work", 1);
        assert_eq!(facts.retry("work", Some(2), now).state, "backoff");
        facts.attempting("work", 2);
        assert_eq!(facts.retry("work", Some(2), now).state, "attempting");
        assert_eq!(facts.retry("work", Some(2), now).remaining_ms, None);
        facts.idle("work", 2);
        assert_eq!(facts.retry("work", Some(2), now).state, "idle");
        assert_eq!(remaining_ms(now, now + Duration::from_secs(1)), 0);
        assert_eq!(remaining_ms(now + Duration::from_nanos(1), now), 1);
    }

    #[test]
    fn phase_uses_raw_lifecycle_and_pending_confirmation_only() {
        for (status, pending, expected) in [
            (Status::Disconnected, false, "stopped"),
            (Status::Connecting, false, "starting"),
            (Status::Connected, true, "verifying"),
            (Status::Connected, false, "running"),
            (Status::Error("reconnecting".into()), true, "failed"),
        ] {
            assert_eq!(phase(&status, pending), expected);
        }
    }

    #[test]
    fn health_timestamps_are_literal_rfc3339_utc_including_leap_days() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_827_696), "2000-02-29T12:34:56Z");
        assert_eq!(format_utc(86_400), "1970-01-02T00:00:00Z");
    }
}
