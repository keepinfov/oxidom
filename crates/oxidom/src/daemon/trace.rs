//! Private runtime correlation. Capture under the engine lock; emit only after
//! releasing application locks. Profile keys never leave this in-memory map.
//! Immutable facts can reach persistence in a different order than capture.

use super::*;
use crate::diagnostics::Recorder;

#[derive(Default)]
pub(super) struct Facts {
    events: Vec<Event>,
    retired: Vec<DiagnosticContext>,
}

impl Facts {
    pub(super) fn push(&mut self, event: Event) {
        self.events.push(event);
    }

    pub(super) fn flush(&mut self, context: DiagnosticContext) {
        self.retired.push(context);
    }

    pub(super) fn emit(self, recorder: &Recorder) {
        for context in self.retired {
            recorder.retire(context);
        }
        for event in self.events {
            recorder.record(event);
        }
    }
}

struct Runtime {
    context: DiagnosticContext,
    core_context: Option<DiagnosticContext>,
    retry: Option<(DiagnosticContext, u32)>,
    retry_allowed: bool,
}

#[derive(Default)]
pub(super) struct Identities {
    sessions: HashMap<String, Runtime>,
}

impl Identities {
    #[cfg(test)]
    pub(super) fn context(&self, profile: &str) -> Option<DiagnosticContext> {
        let context = self.sessions.get(profile)?.context;
        context.attempt.map(|_| context)
    }

    pub(super) fn observed_core(
        &self,
        profile: &str,
        core: Option<u64>,
    ) -> Option<DiagnosticContext> {
        let context = self.sessions.get(profile)?.core_context?;
        (core.is_some() && context.core == core).then_some(context)
    }

    pub(super) fn start(
        &mut self,
        recorder: &Recorder,
        profile: &str,
        generation: u64,
        origin: Origin,
        target: TargetKind,
        facts: &mut Facts,
    ) -> Option<DiagnosticContext> {
        let run = recorder.run_id()?;
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.sessions.entry(profile.to_string())
        {
            let context = DiagnosticContext {
                run,
                session: recorder.next_id()?,
                generation,
                attempt: None,
                core: None,
            };
            entry.insert(Runtime {
                context,
                core_context: None,
                retry: None,
                retry_allowed: true,
            });
            facts.push(Event::SessionOpened { context });
        }
        let runtime = self.sessions.get_mut(profile)?;
        facts.flush(runtime.context);
        if runtime.context.generation != generation {
            Self::cancel(runtime, facts);
        }
        let attempt = recorder.next_id();
        runtime.retry_allowed = true;
        runtime.context = DiagnosticContext {
            generation,
            attempt,
            core: None,
            ..runtime.context
        };
        // Exhaustion disables attribution for this attempt, never the connect.
        attempt?;
        facts.push(Event::AttemptStarted {
            context: runtime.context,
            origin,
            target,
        });
        Some(runtime.context)
    }

    pub(super) fn started(
        &mut self,
        profile: &str,
        context: Option<DiagnosticContext>,
        core: Option<u64>,
        adopted: bool,
        facts: &mut Facts,
    ) -> Option<DiagnosticContext> {
        let mut context = context?;
        context.core = core;
        let runtime = self.sessions.get_mut(profile)?;
        if let Some(previous) = runtime.core_context
            && previous != context
        {
            facts.flush(previous);
        }
        runtime.context = context;
        runtime.core_context = core.map(|_| context);
        if core.is_some() {
            facts.push(Event::CoreStarted { context, adopted });
        }
        Some(context)
    }

    pub(super) fn retry(
        &mut self,
        profile: &str,
        trigger: Option<DiagnosticContext>,
        state: RetryState,
        ordinal: u32,
        facts: &mut Facts,
    ) -> bool {
        let Some(trigger) = trigger else { return false };
        let Some(runtime) = self.sessions.get_mut(profile) else {
            return false;
        };
        match state {
            RetryState::Pending => {
                if !runtime.retry_allowed
                    || (runtime.context.generation != trigger.generation
                        && runtime.core_context != Some(trigger)
                        && runtime.retry.map(|(old, _)| old) != Some(trigger))
                    || runtime.retry.is_some_and(|(old, _)| old != trigger)
                {
                    return false;
                }
                runtime.retry = Some((trigger, ordinal));
            }
            RetryState::Cancelled | RetryState::Recovered => {
                if runtime.retry.map(|(old, _)| old) != Some(trigger) {
                    return false;
                }
                runtime.retry = None;
            }
        }
        facts.push(Event::Retry {
            context: trigger,
            state,
            ordinal,
            delay_ms: if state == RetryState::Pending {
                reconnect_delay(ordinal).as_millis() as u64
            } else {
                0
            },
        });
        true
    }

    fn cancel(runtime: &mut Runtime, facts: &mut Facts) {
        if let Some((context, ordinal)) = runtime.retry.take() {
            facts.push(Event::Retry {
                context,
                state: RetryState::Cancelled,
                ordinal,
                delay_ms: 0,
            });
        }
    }

    pub(super) fn supersede(&mut self, profile: &str, facts: &mut Facts) {
        if let Some(runtime) = self.sessions.get_mut(profile) {
            runtime.retry_allowed = false;
            facts.flush(runtime.context);
            Self::cancel(runtime, facts);
        }
    }

    pub(super) fn retire(&mut self, profile: &str, facts: &mut Facts) {
        if let Some(mut runtime) = self.sessions.remove(profile) {
            Self::cancel(&mut runtime, facts);
            if let Some(core) = runtime.core_context {
                facts.flush(core);
            }
            facts.push(Event::SessionRetired {
                context: runtime.context,
            });
        }
    }

    pub(super) fn flush_all(&self, facts: &mut Facts) {
        for runtime in self.sessions.values() {
            facts.flush(runtime.context);
            if let Some(core) = runtime.core_context
                && core != runtime.context
            {
                facts.flush(core);
            }
        }
    }

    pub(super) fn selections(engine: &Engine) -> HashSet<String> {
        engine
            .sessions
            .iter()
            .filter(|(_, session)| session.selection.is_some())
            .map(|(profile, _)| profile.to_string())
            .collect()
    }

    pub(super) fn after_teardown(
        &mut self,
        before: &HashSet<String>,
        engine: &Engine,
        facts: &mut Facts,
    ) {
        for profile in before {
            if engine
                .sessions
                .get(profile)
                .is_none_or(|session| session.selection.is_none())
            {
                self.supersede(profile, facts);
            }
        }
        self.prune(engine, facts);
    }

    /// An inspectable disconnected entry is not an accepted runtime lifetime.
    /// Held/failed retry sessions keep their identity until actually stopped.
    pub(super) fn prune(&mut self, engine: &Engine, facts: &mut Facts) {
        let retired = self
            .sessions
            .iter()
            .filter(|(profile, runtime)| {
                engine.sessions.get(profile).is_none_or(|session| {
                    session.selection.is_none()
                        && runtime.retry.is_none()
                        && !session.holding_traffic
                        && session.status() == Status::Disconnected
                        && session.interface.as_ref().is_none_or(|interface| {
                            !interface.up && interface.tun2socks.child_pid().is_none()
                        })
                })
            })
            .map(|(profile, _)| profile.clone())
            .collect::<Vec<_>>();
        for profile in retired {
            self.retire(&profile, facts);
        }
    }
}

pub(super) fn observation(
    outcome: Option<&probe::ProbeOutcome>,
) -> (ObservationOutcome, Option<ProbeDetail>) {
    match outcome {
        Some(probe::ProbeOutcome::Reachable(_)) => (ObservationOutcome::Reachable, None),
        Some(probe::ProbeOutcome::Timeout) => (ObservationOutcome::Timeout, None),
        Some(probe::ProbeOutcome::Unreachable) => (ObservationOutcome::Unreachable, None),
        Some(probe::ProbeOutcome::NoNetwork) => (ObservationOutcome::NoNetwork, None),
        Some(probe::ProbeOutcome::Internal(detail)) => {
            (ObservationOutcome::Internal, Some(*detail))
        }
        None => (ObservationOutcome::MissingSubject, None),
    }
}

pub(super) fn watch_mode(mode: WatchdogMode) -> DiagnosticWatchMode {
    match mode {
        WatchdogMode::Off => DiagnosticWatchMode::Off,
        WatchdogMode::Conservative => DiagnosticWatchMode::Conservative,
        WatchdogMode::Aggressive => DiagnosticWatchMode::Aggressive,
    }
}

pub(super) fn finished(
    context: Option<DiagnosticContext>,
    outcome: ObservationOutcome,
    current: bool,
    error: Option<&anyhow::Error>,
    detail: Option<ProbeDetail>,
) -> Option<Event> {
    let context = context?;
    let (failure, os_code) = error
        .map(crate::diagnostics::classify)
        .map(|(kind, code)| (Some(kind), code))
        .unwrap_or((None, None));
    Some(Event::AttemptFinished {
        context,
        outcome,
        disposition: if current {
            Disposition::Accepted
        } else {
            Disposition::DiscardedStale
        },
        failure,
        os_code,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_a_claimed_retry_uses_its_trigger_and_cannot_reopen_the_episode() {
        let recorder = Recorder::for_test();
        let mut identities = Identities::default();
        let mut facts = Facts::default();
        let first = identities
            .start(
                &recorder,
                "invented-private-profile",
                1,
                Origin::Explicit,
                TargetKind::Server,
                &mut facts,
            )
            .unwrap();
        let first = identities
            .started(
                "invented-private-profile",
                Some(first),
                Some(7),
                false,
                &mut facts,
            )
            .unwrap();
        assert!(!identities.retry(
            "invented-private-profile",
            Some(first),
            RetryState::Cancelled,
            0,
            &mut facts,
        ));
        assert!(identities.retry(
            "invented-private-profile",
            Some(first),
            RetryState::Pending,
            0,
            &mut facts,
        ));
        let second = identities
            .start(
                &recorder,
                "invented-private-profile",
                1,
                Origin::Automatic,
                TargetKind::Server,
                &mut facts,
            )
            .unwrap();
        assert_ne!(first.attempt, second.attempt);
        identities.supersede("invented-private-profile", &mut facts);
        assert!(!identities.retry(
            "invented-private-profile",
            Some(first),
            RetryState::Pending,
            1,
            &mut facts,
        ));
        assert!(!identities.retry(
            "invented-private-profile",
            Some(first),
            RetryState::Recovered,
            1,
            &mut facts,
        ));
        facts.emit(&recorder);
        let retries = recorder
            .events()
            .into_iter()
            .filter(|event| matches!(event, Event::Retry { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            retries,
            vec![
                Event::Retry {
                    context: first,
                    state: RetryState::Pending,
                    ordinal: 0,
                    delay_ms: 1000
                },
                Event::Retry {
                    context: first,
                    state: RetryState::Cancelled,
                    ordinal: 0,
                    delay_ms: 0
                },
            ]
        );
        assert!(
            !serde_json::to_string(&recorder.events())
                .unwrap()
                .contains("invented-private-profile")
        );
    }

    #[test]
    fn failed_attempts_do_not_relabel_an_existing_core() {
        let recorder = Recorder::for_test();
        let mut identities = Identities::default();
        let mut facts = Facts::default();
        let first = identities
            .start(
                &recorder,
                "default",
                1,
                Origin::Explicit,
                TargetKind::Server,
                &mut facts,
            )
            .unwrap();
        let first = identities
            .started("default", Some(first), Some(7), false, &mut facts)
            .unwrap();
        let failed = identities
            .start(
                &recorder,
                "default",
                1,
                Origin::Automatic,
                TargetKind::Server,
                &mut facts,
            )
            .unwrap();
        assert_eq!(failed.core, None);
        assert_eq!(identities.observed_core("default", Some(7)), Some(first));
        assert_eq!(identities.observed_core("default", Some(8)), None);
        let error = anyhow::anyhow!("invented private error text");
        facts.push(
            finished(
                Some(failed),
                ObservationOutcome::StartFailed,
                true,
                Some(&error),
                None,
            )
            .unwrap(),
        );
        facts.emit(&recorder);
        assert!(recorder.events().contains(&Event::AttemptFinished {
            context: failed,
            outcome: ObservationOutcome::StartFailed,
            disposition: Disposition::Accepted,
            failure: Some(oxidom_core::diagnostics::FailureKind::Other),
            os_code: None,
            detail: None,
        }));
        assert!(
            !serde_json::to_string(&recorder.events())
                .unwrap()
                .contains("private error")
        );
    }
}
