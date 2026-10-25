//! Shared typed recorder with bounded, context-specific observation summaries.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use oxidom_core::diagnostics::{
    Context, Diagnostics, Event, FailureKind, FlushReason, Phase, WatchAction, WatchObservation,
    generate_run_id,
};
use oxidom_core::logbook::{LogSource, Severity};
use oxidom_core::sync::lock;

#[derive(Default)]
struct Runtime {
    closed: bool,
    ordinal: u64,
    aggregation_disabled: bool,
    buckets: VecDeque<Bucket>,
}

struct Bucket {
    key: WatchObservation,
    suppressed: u64,
    first: u64,
    last: u64,
}

impl Bucket {
    fn flush(&mut self, reason: FlushReason) -> Option<Event> {
        if self.suppressed == 0 {
            return None;
        }
        let event = Event::WatchSummary {
            key: self.key,
            suppressed_count: self.suppressed,
            first_ordinal: self.first,
            last_ordinal: self.last,
            reason,
        };
        self.suppressed = 0;
        Some(event)
    }
}

impl Runtime {
    fn accept(&mut self, event: Event) -> Vec<Event> {
        let lifecycle = matches!(
            event,
            Event::Phase { .. }
                | Event::Problem { .. }
                | Event::CatalogLoadWarnings { .. }
                | Event::PersistenceGap { .. }
        );
        if self.closed && !lifecycle {
            return Vec::new();
        }
        if self.aggregation_disabled {
            return vec![event];
        }
        // Exhaustion disables aggregation permanently, preserving the events
        // without inventing wrapped or ambiguous acceptance ordinals.
        let Some(ordinal) = self.ordinal.checked_add(1) else {
            self.aggregation_disabled = true;
            let mut output = self.flush_all(FlushReason::Capacity);
            output.push(event);
            return output;
        };
        self.ordinal = ordinal;
        if lifecycle {
            return vec![event];
        }
        if let Event::SessionRetired { context } = event {
            let mut output = self.retire(context);
            output.push(event);
            return output;
        }
        let Event::Watch {
            context,
            policy_epoch,
            mode,
            outcome,
            detail,
            action,
        } = event
        else {
            return vec![event];
        };
        let key = WatchObservation {
            context,
            policy_epoch,
            mode,
            outcome,
            detail,
            action,
        };
        let mut output = Vec::new();
        // A changed cause within the exact context/policy closes the old bucket.
        // Other policies and core incarnations remain independent FIFO entries.
        if let Some(index) = self.buckets.iter().position(|bucket| {
            bucket.key.context == context
                && bucket.key.policy_epoch == policy_epoch
                && bucket.key != key
        }) {
            let mut bucket = self.buckets.remove(index).unwrap();
            output.extend(bucket.flush(FlushReason::CauseChanged));
        }
        if action != WatchAction::Observed {
            output.push(event);
            return output;
        }
        if let Some(bucket) = self.buckets.iter_mut().find(|bucket| bucket.key == key) {
            if bucket.suppressed == 0 {
                bucket.first = ordinal;
            }
            bucket.last = ordinal;
            // Reset at sixteen: this counter can never approach u64 exhaustion.
            bucket.suppressed += 1;
            if bucket.suppressed == 16 {
                output.extend(bucket.flush(FlushReason::Batch));
            }
            return output;
        }
        if self.buckets.len() == 64 {
            output.extend(
                self.buckets
                    .pop_front()
                    .unwrap()
                    .flush(FlushReason::Capacity),
            );
        }
        self.buckets.push_back(Bucket {
            key,
            suppressed: 0,
            first: 0,
            last: 0,
        });
        output.push(event);
        output
    }

    /// Retire the subject generation, including its old attempts/incarnations.
    /// This flush is not a stale-worker fence; the daemon owns subject validity.
    fn retire(&mut self, context: Context) -> Vec<Event> {
        let mut output = Vec::new();
        let mut index = 0;
        while index < self.buckets.len() {
            let old = self.buckets[index].key.context;
            if (old.run, old.session, old.generation)
                == (context.run, context.session, context.generation)
            {
                output.extend(
                    self.buckets
                        .remove(index)
                        .unwrap()
                        .flush(FlushReason::ContextEnded),
                );
            } else {
                index += 1;
            }
        }
        output
    }

    fn flush_all(&mut self, reason: FlushReason) -> Vec<Event> {
        self.buckets
            .drain(..)
            .filter_map(|mut bucket| bucket.flush(reason))
            .collect()
    }

    fn close(&mut self) -> Vec<Event> {
        self.closed = true;
        self.flush_all(FlushReason::Shutdown)
    }
}

#[derive(Default)]
struct Persistence {
    attempts: u64,
    occurrences: u64,
    last: Option<(FailureKind, Option<i32>)>,
    saturated: bool,
}

impl Persistence {
    /// One gap-only attempt, including when no other event is available. Failed
    /// gap writes update the safe cause but never count themselves as new loss.
    fn flush_gap(&mut self, mut write: impl FnMut(Event) -> io::Result<()>) -> bool {
        let Some((last_kind, last_os_code)) = self.last else {
            return false;
        };
        let gap = Event::PersistenceGap {
            unconfirmed_write_attempts: self.attempts,
            unconfirmed_occurrences: self.occurrences,
            last_kind,
            last_os_code,
            saturated: self.saturated,
        };
        match write(gap) {
            Ok(()) => {
                *self = Self::default();
                false
            }
            Err(error) => {
                self.last = Some(classify_io(&error));
                true
            }
        }
    }

    /// At most one gap attempt and one event attempt, without recursive loss.
    fn emit(&mut self, event: Event, mut write: impl FnMut(Event) -> io::Result<()>) -> bool {
        let mut failed = self.flush_gap(&mut write);
        if let Err(error) = write(event) {
            let occurrences = match event {
                Event::WatchSummary {
                    suppressed_count, ..
                } => suppressed_count,
                _ => 1,
            };
            match (
                self.attempts.checked_add(1),
                self.occurrences.checked_add(occurrences),
            ) {
                (Some(attempts), Some(occurrences)) => {
                    self.attempts = attempts;
                    self.occurrences = occurrences;
                }
                (attempts, occurrences) => {
                    self.attempts = attempts.unwrap_or(u64::MAX);
                    self.occurrences = occurrences.unwrap_or(u64::MAX);
                    self.saturated = true;
                }
            }
            self.last = Some(classify_io(&error));
            failed = true;
        }
        failed
    }
}

/// Clones share the opened sink, checked IDs, runtime fence and once-only warning.
/// No profile names or other application identities are retained here.
#[derive(Clone)]
pub(crate) struct Recorder {
    inner: Arc<RecorderInner>,
}

struct RecorderInner {
    sink: Option<Diagnostics>,
    runtime: Mutex<Runtime>,
    // Serializes reduction + emission order without holding runtime state at I/O.
    emission: Mutex<Persistence>,
    next_id: AtomicU64,
    run_id: Option<u64>,
    warned: AtomicBool,
    warn_enabled: bool,
    #[cfg(test)]
    capture: Option<Mutex<Vec<Event>>>,
    #[cfg(test)]
    write_results: Mutex<VecDeque<io::Result<()>>>,
    #[cfg(test)]
    observer: Option<Arc<dyn Fn(Event) + Send + Sync>>,
}

impl Recorder {
    fn new(sink: Option<Diagnostics>, warn_enabled: bool) -> Self {
        Self::with_run_id(sink, warn_enabled, generate_run_id())
    }

    fn with_run_id(sink: Option<Diagnostics>, warn_enabled: bool, run_id: Option<u64>) -> Self {
        Self {
            inner: Arc::new(RecorderInner {
                sink,
                runtime: Mutex::new(Runtime::default()),
                emission: Mutex::new(Persistence::default()),
                next_id: AtomicU64::new(0),
                run_id,
                warned: AtomicBool::new(false),
                warn_enabled,
                #[cfg(test)]
                capture: None,
                #[cfg(test)]
                write_results: Mutex::new(VecDeque::new()),
                #[cfg(test)]
                observer: None,
            }),
        }
    }

    /// No files and no fallback warning; IDs and runtime semantics remain usable.
    #[cfg(test)]
    pub(crate) fn silent() -> Self {
        Self::with_run_id(None, false, Some(1))
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let mut recorder = Self::silent();
        Arc::get_mut(&mut recorder.inner).unwrap().capture = Some(Mutex::new(Vec::new()));
        recorder
    }

    /// Observes each attempted write before failure injection or capture, with
    /// reducer/capture locks released. Emission serialization may still be held.
    #[cfg(test)]
    pub(crate) fn for_test_with_observer(observer: impl Fn(Event) + Send + Sync + 'static) -> Self {
        let mut recorder = Self::for_test();
        Arc::get_mut(&mut recorder.inner).unwrap().observer = Some(Arc::new(observer));
        recorder
    }

    #[cfg(test)]
    pub(crate) fn events(&self) -> Vec<Event> {
        self.inner
            .capture
            .as_ref()
            .map(|events| lock(events).clone())
            .unwrap_or_default()
    }

    pub(crate) fn run_id(&self) -> Option<u64> {
        self.inner.run_id
    }

    /// Shared session/attempt namespace. Exhaustion permanently returns None.
    pub(crate) fn next_id(&self) -> Option<u64> {
        self.inner
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()
            .and_then(|id| id.checked_add(1))
    }

    pub(crate) fn record(&self, event: Event) {
        self.reduce_and_emit(|runtime| runtime.accept(event), false);
    }

    /// Flushes pending deltas for run/session/generation; does not emit a second
    /// SessionRetired event or retain an unbounded set of retired identities.
    pub(crate) fn retire(&self, context: Context) {
        self.reduce_and_emit(|runtime| runtime.retire(context), false);
    }

    /// Fences runtime acceptance and drains summaries once, then makes at most
    /// one gap-only attempt. Repeated calls can retry pending gaps, never reopen
    /// runtime acceptance. P05 phase/problem/catalog events remain enabled.
    pub(crate) fn close_runtime(&self) {
        self.reduce_and_emit(Runtime::close, true);
    }

    fn reduce_and_emit(&self, reduce: impl FnOnce(&mut Runtime) -> Vec<Event>, flush_gap: bool) {
        let mut emission = lock(&self.inner.emission);
        let output = {
            let mut runtime = lock(&self.inner.runtime);
            reduce(&mut runtime)
        };
        let mut failed = false;
        for event in output {
            failed |= emission.emit(event, |event| self.write(event));
        }
        if flush_gap {
            failed |= emission.flush_gap(|event| self.write(event));
        }
        drop(emission);
        if failed {
            self.unavailable();
        }
    }

    fn write(&self, event: Event) -> io::Result<()> {
        #[cfg(test)]
        {
            if let Some(observer) = &self.inner.observer {
                observer(event);
            }
            if let Some(result) = lock(&self.inner.write_results).pop_front() {
                result?;
            }
            if let Some(events) = &self.inner.capture {
                lock(events).push(event);
            }
        }
        match &self.inner.sink {
            Some(sink) => sink.write(event),
            None => Ok(()),
        }
    }

    fn unavailable(&self) {
        if !self.inner.warn_enabled || self.inner.warned.swap(true, Ordering::Relaxed) {
            return;
        }
        const WARNING: &str =
            "persistent daemon diagnostics unavailable; original stderr remains enabled";
        // Neither path re-enters the sink or depends on RUST_LOG. A broken
        // fallback stream must not replace the daemon's original outcome.
        oxidom_core::logbook::global().push(
            LogSource::Oxidom,
            Severity::Warn,
            None,
            "oxidom::diagnostics",
            WARNING.to_string(),
        );
        let _ = writeln!(io::stderr().lock(), "oxidom: {WARNING}");
    }
}

/// Record returned startup failures before the CLI prints its original error.
/// Pre-exec failures, argument parsing, panic payloads and abrupt termination
/// are deliberately not copied into this structured journal.
pub(super) fn run(options: crate::daemon::DaemonOptions) -> Result<()> {
    let mut diagnostics = DaemonLog::new();
    diagnostics.phase(Phase::Starting);
    let result = crate::daemon::run(options, &mut diagnostics);
    diagnostics.close_runtime();
    if let Err(error) = &result {
        diagnostics.problem(error, true);
    }
    // The final lifecycle write can fail after runtime acceptance was fenced.
    // Re-closing only retries its pending gap; summaries stay drained.
    diagnostics.close_runtime();
    result
}

pub(super) struct DaemonLog {
    recorder: Recorder,
    phase: Phase,
}

impl DaemonLog {
    fn new() -> Self {
        let sink = oxidom_core::paths::daemon_log_file()
            .ok()
            .and_then(|path| Diagnostics::open(path).ok());
        let diagnostics = Self {
            recorder: Recorder::new(sink, true),
            phase: Phase::Starting,
        };
        if diagnostics.recorder.inner.sink.is_none() {
            diagnostics.recorder.unavailable();
        }
        diagnostics
    }

    pub(super) fn phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.recorder.record(Event::Phase { phase });
    }

    pub(super) fn problem(&mut self, error: &anyhow::Error, fatal: bool) {
        let (kind, os_code) = classify(error);
        self.recorder.record(Event::Problem {
            phase: self.phase,
            kind,
            os_code,
            fatal,
        });
    }

    pub(super) fn catalog_load_warnings(&mut self, count: usize) {
        if count != 0 {
            self.recorder.record(Event::CatalogLoadWarnings {
                count: u32::try_from(count).unwrap_or(u32::MAX),
            });
        }
    }

    pub(crate) fn recorder(&self) -> Recorder {
        self.recorder.clone()
    }

    pub(crate) fn close_runtime(&self) {
        self.recorder.close_runtime();
    }
}

pub(crate) fn classify(error: &anyhow::Error) -> (FailureKind, Option<i32>) {
    for cause in error.chain() {
        if matches!(
            cause.downcast_ref::<std::fs::TryLockError>(),
            Some(std::fs::TryLockError::WouldBlock)
        ) {
            return (FailureKind::StoreBusy, None);
        }
        if let Some(error) = cause.downcast_ref::<io::Error>() {
            return classify_io(error);
        }
        if let Some(error) = cause.downcast_ref::<nix::errno::Errno>() {
            return classify_io(&io::Error::from_raw_os_error(*error as i32));
        }
        if let Some(error) = cause.downcast_ref::<zbus::Error>() {
            return match error {
                zbus::Error::InputOutput(error) => classify_io(error),
                _ => (FailureKind::Bus, None),
            };
        }
    }
    (FailureKind::Other, None)
}

fn classify_io(error: &io::Error) -> (FailureKind, Option<i32>) {
    let kind = match error.kind() {
        io::ErrorKind::PermissionDenied => FailureKind::PermissionDenied,
        io::ErrorKind::NotFound => FailureKind::NotFound,
        io::ErrorKind::AlreadyExists => FailureKind::AlreadyExists,
        io::ErrorKind::InvalidInput => FailureKind::InvalidInput,
        io::ErrorKind::InvalidData => FailureKind::InvalidData,
        io::ErrorKind::StorageFull => FailureKind::StorageFull,
        io::ErrorKind::ReadOnlyFilesystem => FailureKind::ReadOnlyFilesystem,
        io::ErrorKind::WriteZero => FailureKind::WriteZero,
        io::ErrorKind::Interrupted => FailureKind::Interrupted,
        _ => FailureKind::OtherIo,
    };
    (kind, error.raw_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidom_core::diagnostics::{
        DiagnosticWatchMode, Disposition, ObservationOutcome, Origin, RetryState, TargetKind,
    };

    fn context() -> Context {
        Context {
            run: 1,
            session: 2,
            generation: 3,
            attempt: Some(4),
            core: Some(5),
        }
    }

    fn watch(context: Context, policy_epoch: u64, outcome: ObservationOutcome) -> Event {
        Event::Watch {
            context,
            policy_epoch,
            mode: DiagnosticWatchMode::Conservative,
            outcome,
            detail: None,
            action: WatchAction::Observed,
        }
    }

    #[test]
    fn the_first_observation_is_visible_and_each_sixteen_repeats_is_a_delta() {
        let mut state = Runtime::default();
        let event = watch(context(), 1, ObservationOutcome::Reachable);
        assert_eq!(state.accept(event), vec![event]);
        for _ in 0..15 {
            assert!(state.accept(event).is_empty());
        }
        let summary = state.accept(event);
        assert!(matches!(
            summary.as_slice(),
            [Event::WatchSummary {
                suppressed_count: 16,
                first_ordinal: 2,
                last_ordinal: 17,
                reason: FlushReason::Batch,
                ..
            }]
        ));
        for _ in 0..15 {
            assert!(state.accept(event).is_empty());
        }
        assert!(matches!(
            state.accept(event).as_slice(),
            [Event::WatchSummary {
                suppressed_count: 16,
                first_ordinal: 18,
                last_ordinal: 33,
                reason: FlushReason::Batch,
                ..
            }]
        ));
        assert!(state.accept(event).is_empty());
        assert!(matches!(
            state.close().as_slice(),
            [Event::WatchSummary {
                suppressed_count: 1,
                first_ordinal: 34,
                last_ordinal: 34,
                reason: FlushReason::Shutdown,
                ..
            }]
        ));
        assert!(state.close().is_empty());
        assert!(state.accept(event).is_empty());
        assert_eq!(
            state.accept(Event::Phase {
                phase: Phase::Stopped
            }),
            vec![Event::Phase {
                phase: Phase::Stopped
            }]
        );
    }

    #[test]
    fn summary_ordinals_count_accepted_events_not_just_matching_observations() {
        let mut state = Runtime::default();
        let event = watch(context(), 1, ObservationOutcome::Timeout);
        state.accept(event);
        state.accept(event);
        state.accept(Event::Phase {
            phase: Phase::Serving,
        });
        state.accept(event);
        assert!(matches!(
            state.close().as_slice(),
            [Event::WatchSummary {
                suppressed_count: 2,
                first_ordinal: 2,
                last_ordinal: 4,
                ..
            }]
        ));
    }

    #[test]
    fn changed_causes_flush_but_policy_generation_and_core_remain_separate() {
        let mut state = Runtime::default();
        let first = watch(context(), 1, ObservationOutcome::Timeout);
        state.accept(first);
        state.accept(first);
        for (ctx, epoch) in [
            (context(), 2),
            (
                Context {
                    generation: 9,
                    ..context()
                },
                1,
            ),
            (
                Context {
                    core: Some(9),
                    ..context()
                },
                1,
            ),
        ] {
            assert_eq!(
                state
                    .accept(watch(ctx, epoch, ObservationOutcome::Timeout))
                    .len(),
                1
            );
        }
        let changed = watch(context(), 1, ObservationOutcome::Unreachable);
        assert!(matches!(
            state.accept(changed).as_slice(),
            [
                Event::WatchSummary {
                    suppressed_count: 1,
                    reason: FlushReason::CauseChanged,
                    ..
                },
                Event::Watch {
                    outcome: ObservationOutcome::Unreachable,
                    ..
                }
            ]
        ));
        assert_eq!(state.buckets.len(), 4);
    }

    #[test]
    fn capacity_is_fifo_and_retirement_flushes_partial_subject_buckets() {
        let mut state = Runtime::default();
        for session in 1..=64 {
            let event = watch(
                Context {
                    session,
                    ..context()
                },
                1,
                ObservationOutcome::Timeout,
            );
            state.accept(event);
            state.accept(event);
        }
        // Re-access does not refresh FIFO insertion order into an LRU order.
        assert!(
            state
                .accept(watch(
                    Context {
                        session: 1,
                        ..context()
                    },
                    1,
                    ObservationOutcome::Timeout
                ))
                .is_empty()
        );
        let extra = watch(
            Context {
                session: 65,
                ..context()
            },
            1,
            ObservationOutcome::Timeout,
        );
        assert!(matches!(
            state.accept(extra).as_slice(),
            [
                Event::WatchSummary {
                    key: WatchObservation {
                        context: Context { session: 1, .. },
                        ..
                    },
                    suppressed_count: 2,
                    reason: FlushReason::Capacity,
                    ..
                },
                Event::Watch { .. }
            ]
        ));
        assert_eq!(state.buckets.len(), 64);
        assert!(matches!(
            state
                .retire(Context {
                    session: 2,
                    core: None,
                    ..context()
                })
                .as_slice(),
            [Event::WatchSummary {
                suppressed_count: 1,
                reason: FlushReason::ContextEnded,
                ..
            }]
        ));
        assert!(state.retire(context()).is_empty());
    }

    #[test]
    fn actions_stale_results_and_lifecycle_events_are_never_suppressed() {
        let mut state = Runtime::default();
        for action in [
            WatchAction::DiscardedStale,
            WatchAction::Reset,
            WatchAction::ConservativeRetained,
            WatchAction::RecoveryApplied,
            WatchAction::RecoveryRejected,
        ] {
            let event = Event::Watch {
                context: context(),
                policy_epoch: 1,
                mode: DiagnosticWatchMode::Off,
                outcome: ObservationOutcome::Internal,
                detail: None,
                action,
            };
            assert_eq!(state.accept(event), vec![event]);
            assert_eq!(state.accept(event), vec![event]);
        }
        for event in [
            Event::SessionOpened { context: context() },
            Event::SessionRetired { context: context() },
            Event::AttemptStarted {
                context: context(),
                origin: Origin::Settings,
                target: TargetKind::Server,
            },
            Event::CoreStarted {
                context: context(),
                adopted: true,
            },
            Event::LivenessLost {
                context: context(),
                holding_traffic: true,
            },
            Event::AttemptFinished {
                context: context(),
                outcome: ObservationOutcome::Reachable,
                disposition: Disposition::DiscardedStale,
                failure: None,
                os_code: None,
                detail: None,
            },
            Event::Retry {
                context: context(),
                state: RetryState::Cancelled,
                ordinal: 1,
                delay_ms: 0,
            },
        ] {
            assert_eq!(state.accept(event), vec![event]);
            assert_eq!(state.accept(event), vec![event]);
        }
    }

    #[test]
    fn a_watch_action_flushes_the_prior_cause_and_is_always_visible() {
        let mut state = Runtime::default();
        let observed = watch(context(), 1, ObservationOutcome::Timeout);
        state.accept(observed);
        state.accept(observed);
        let action = Event::Watch {
            context: context(),
            policy_epoch: 1,
            mode: DiagnosticWatchMode::Conservative,
            outcome: ObservationOutcome::Timeout,
            detail: None,
            action: WatchAction::RecoveryApplied,
        };
        assert!(matches!(
            state.accept(action).as_slice(),
            [
                Event::WatchSummary {
                    key: WatchObservation {
                        action: WatchAction::Observed,
                        ..
                    },
                    suppressed_count: 1,
                    reason: FlushReason::CauseChanged,
                    ..
                },
                Event::Watch {
                    action: WatchAction::RecoveryApplied,
                    ..
                }
            ]
        ));
        assert_eq!(state.accept(action), vec![action]);
        assert_eq!(state.accept(observed), vec![observed]);
    }

    #[test]
    fn cloned_recorders_share_ids_capture_and_the_runtime_close_fence() {
        let recorder = Recorder::for_test();
        let clone = recorder.clone();
        assert_eq!(recorder.run_id(), clone.run_id());
        assert_eq!(recorder.next_id(), Some(1));
        assert_eq!(clone.next_id(), Some(2));
        let event = watch(context(), 1, ObservationOutcome::Reachable);
        clone.record(event);
        recorder.record(event);
        clone.close_runtime();
        recorder.close_runtime();
        recorder.record(event);
        recorder.record(Event::Phase {
            phase: Phase::Shutdown,
        });
        assert_eq!(recorder.events().len(), 3);
        recorder
            .inner
            .next_id
            .store(u64::MAX - 1, Ordering::Relaxed);
        assert_eq!(clone.next_id(), Some(u64::MAX));
        assert_eq!(recorder.next_id(), None);
        assert_eq!(clone.next_id(), None);
        assert_eq!(Recorder::silent().next_id(), Some(1));
    }

    #[test]
    fn details_and_modes_change_causes_without_coalescing_old_deltas() {
        let mut state = Runtime::default();
        let first = watch(context(), 1, ObservationOutcome::Internal);
        state.accept(first);
        state.accept(first);
        let detailed = Event::Watch {
            context: context(),
            policy_epoch: 1,
            mode: DiagnosticWatchMode::Conservative,
            outcome: ObservationOutcome::Internal,
            detail: Some(oxidom_core::ipc::ProbeDetail::ConfigRefused),
            action: WatchAction::Observed,
        };
        assert!(matches!(
            state.accept(detailed).as_slice(),
            [
                Event::WatchSummary {
                    key: WatchObservation { detail: None, .. },
                    reason: FlushReason::CauseChanged,
                    ..
                },
                Event::Watch { .. }
            ]
        ));
        state.accept(detailed);
        let changed_mode = Event::Watch {
            context: context(),
            policy_epoch: 1,
            mode: DiagnosticWatchMode::Aggressive,
            outcome: ObservationOutcome::Internal,
            detail: Some(oxidom_core::ipc::ProbeDetail::ConfigRefused),
            action: WatchAction::Observed,
        };
        assert!(matches!(
            state.accept(changed_mode).as_slice(),
            [
                Event::WatchSummary {
                    key: WatchObservation {
                        mode: DiagnosticWatchMode::Conservative,
                        ..
                    },
                    reason: FlushReason::CauseChanged,
                    ..
                },
                Event::Watch {
                    mode: DiagnosticWatchMode::Aggressive,
                    ..
                }
            ]
        ));
        assert_eq!(state.buckets.len(), 1);
    }

    #[test]
    fn retiring_one_generation_flushes_all_its_incarnations_and_no_other_subject() {
        let mut state = Runtime::default();
        for ctx in [
            context(),
            Context {
                attempt: Some(8),
                core: Some(9),
                ..context()
            },
            Context {
                generation: 8,
                ..context()
            },
            Context {
                run: 8,
                ..context()
            },
            Context {
                session: 8,
                ..context()
            },
        ] {
            let event = watch(ctx, 1, ObservationOutcome::Timeout);
            state.accept(event);
            state.accept(event);
        }
        let retired = Event::SessionRetired { context: context() };
        assert!(matches!(
            state.accept(retired).as_slice(),
            [
                Event::WatchSummary {
                    reason: FlushReason::ContextEnded,
                    ..
                },
                Event::WatchSummary {
                    reason: FlushReason::ContextEnded,
                    ..
                },
                Event::SessionRetired { .. }
            ]
        ));
        assert_eq!(state.buckets.len(), 3);
        assert_eq!(state.accept(retired), vec![retired]);
    }

    #[test]
    fn the_runtime_fence_preserves_phase_problem_and_catalog_records() {
        let recorder = Recorder::for_test();
        let diagnostics = DaemonLog {
            recorder: recorder.clone(),
            phase: Phase::Store,
        };
        let clone = diagnostics.recorder();
        let observed = watch(context(), 1, ObservationOutcome::Timeout);
        clone.record(observed);
        clone.record(observed);
        clone.retire(context());
        clone.record(observed);
        clone.record(observed);
        diagnostics.close_runtime();
        clone.record(observed);
        clone.record(Event::SessionOpened { context: context() });
        clone.record(Event::SessionRetired { context: context() });
        clone.record(Event::Phase {
            phase: Phase::Stopped,
        });
        clone.record(Event::Problem {
            phase: Phase::Store,
            kind: FailureKind::Other,
            os_code: None,
            fatal: true,
        });
        clone.record(Event::CatalogLoadWarnings { count: 1 });
        assert_eq!(recorder.events().len(), 7);
        assert!(matches!(
            recorder.events().as_slice(),
            [
                Event::Watch { .. },
                Event::WatchSummary {
                    reason: FlushReason::ContextEnded,
                    ..
                },
                Event::Watch { .. },
                Event::WatchSummary {
                    reason: FlushReason::Shutdown,
                    ..
                },
                Event::Phase { .. },
                Event::Problem { fatal: true, .. },
                Event::CatalogLoadWarnings { .. }
            ]
        ));
        assert!(!recorder.inner.warned.load(Ordering::Relaxed));
    }

    #[test]
    fn ordinal_exhaustion_flushes_and_disables_aggregation_without_wrapping() {
        let mut state = Runtime::default();
        let event = watch(context(), 1, ObservationOutcome::Timeout);
        state.accept(event);
        state.accept(event);
        state.ordinal = u64::MAX;
        assert!(matches!(
            state.accept(event).as_slice(),
            [
                Event::WatchSummary {
                    suppressed_count: 1,
                    reason: FlushReason::Capacity,
                    ..
                },
                Event::Watch { .. }
            ]
        ));
        assert_eq!(state.accept(event), vec![event]);
        assert_eq!(state.ordinal, u64::MAX);
    }

    #[test]
    fn failed_summaries_count_occurrences_and_gap_failures_do_not_amplify() {
        let mut persistence = Persistence::default();
        let event = Event::WatchSummary {
            key: WatchObservation {
                context: context(),
                policy_epoch: 1,
                mode: DiagnosticWatchMode::Off,
                outcome: ObservationOutcome::Timeout,
                detail: None,
                action: WatchAction::Observed,
            },
            suppressed_count: 16,
            first_ordinal: 2,
            last_ordinal: 17,
            reason: FlushReason::Batch,
        };
        assert!(persistence.emit(event, |_| Err(io::Error::from_raw_os_error(5))));
        assert_eq!((persistence.attempts, persistence.occurrences), (1, 16));
        let mut calls = 0;
        persistence.emit(
            Event::Phase {
                phase: Phase::Serving,
            },
            |_| {
                calls += 1;
                Err(io::Error::from_raw_os_error(28))
            },
        );
        assert_eq!(calls, 2);
        assert_eq!((persistence.attempts, persistence.occurrences), (2, 17));
        let mut written = Vec::new();
        persistence.emit(
            Event::Phase {
                phase: Phase::Stopped,
            },
            |event| {
                written.push(event);
                Ok(())
            },
        );
        assert!(matches!(
            written.as_slice(),
            [
                Event::PersistenceGap {
                    unconfirmed_write_attempts: 2,
                    unconfirmed_occurrences: 17,
                    ..
                },
                Event::Phase { .. }
            ]
        ));
        assert_eq!((persistence.attempts, persistence.occurrences), (0, 0));
        persistence.attempts = u64::MAX;
        persistence.occurrences = u64::MAX;
        persistence.emit(event, |_| Err(io::ErrorKind::WriteZero.into()));
        assert!(persistence.saturated);
        assert_eq!(
            (persistence.attempts, persistence.occurrences),
            (u64::MAX, u64::MAX)
        );
    }

    #[test]
    fn only_a_successful_gap_write_acknowledges_the_pending_snapshot() {
        let mut persistence = Persistence::default();
        let phase = Event::Phase {
            phase: Phase::Serving,
        };
        persistence.emit(phase, |_| Err(io::ErrorKind::WriteZero.into()));
        let mut calls = 0;
        persistence.emit(phase, |_| {
            calls += 1;
            if calls == 1 {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(())
            }
        });
        assert_eq!((persistence.attempts, persistence.occurrences), (1, 1));
        assert_eq!(
            persistence.last,
            Some((FailureKind::PermissionDenied, None))
        );
        calls = 0;
        persistence.emit(phase, |event| {
            calls += 1;
            if calls == 1 {
                assert!(matches!(
                    event,
                    Event::PersistenceGap {
                        unconfirmed_write_attempts: 1,
                        ..
                    }
                ));
                Ok(())
            } else {
                Err(io::ErrorKind::StorageFull.into())
            }
        });
        assert_eq!((persistence.attempts, persistence.occurrences), (1, 1));
        assert_eq!(persistence.last, Some((FailureKind::StorageFull, None)));
    }

    #[test]
    fn closing_without_suppressed_observations_flushes_a_recovered_persistence_gap() {
        let recorder = Recorder::for_test();
        lock(&recorder.inner.write_results).push_back(Err(io::ErrorKind::StorageFull.into()));
        recorder.record(Event::Watch {
            context: context(),
            policy_epoch: 1,
            mode: DiagnosticWatchMode::Conservative,
            outcome: ObservationOutcome::Internal,
            detail: Some(oxidom_core::ipc::ProbeDetail::ConfigRefused),
            action: WatchAction::Observed,
        });
        assert!(recorder.events().is_empty());
        recorder.close_runtime();
        assert!(matches!(
            recorder.events().as_slice(),
            [Event::PersistenceGap {
                unconfirmed_write_attempts: 1,
                unconfirmed_occurrences: 1,
                last_kind: FailureKind::StorageFull,
                ..
            }]
        ));
        {
            let persistence = lock(&recorder.inner.emission);
            assert!(persistence.last.is_none());
            assert_eq!((persistence.attempts, persistence.occurrences), (0, 0));
            assert!(!persistence.saturated);
        }
        recorder.close_runtime();
        assert_eq!(recorder.events().len(), 1);
    }

    #[test]
    fn the_test_observer_sees_every_attempt_without_capture_or_reducer_locks() {
        let target = Arc::new(Mutex::new(None::<std::sync::Weak<RecorderInner>>));
        let attempted = Arc::new(Mutex::new(Vec::new()));
        let observer_target = target.clone();
        let observer_attempted = attempted.clone();
        let recorder = Recorder::for_test_with_observer(move |event| {
            let inner = lock(&observer_target).as_ref().unwrap().upgrade().unwrap();
            assert!(inner.runtime.try_lock().is_ok());
            assert!(inner.capture.as_ref().unwrap().try_lock().is_ok());
            assert!(inner.write_results.try_lock().is_ok());
            lock(&observer_attempted).push(event);
        });
        *lock(&target) = Some(Arc::downgrade(&recorder.inner));
        lock(&recorder.inner.write_results).extend([
            Err(io::ErrorKind::StorageFull.into()),
            Err(io::ErrorKind::PermissionDenied.into()),
            Err(io::ErrorKind::StorageFull.into()),
            Ok(()),
        ]);
        let clone = recorder.clone();
        let observed = watch(context(), 1, ObservationOutcome::Timeout);
        clone.record(observed);
        assert!(recorder.events().is_empty());
        clone.record(observed);
        assert_eq!(lock(&attempted).as_slice(), &[observed]);
        clone.close_runtime();
        recorder.close_runtime();
        recorder.record(observed);
        recorder.record(Event::Phase {
            phase: Phase::Stopped,
        });
        assert!(matches!(
            lock(&attempted).as_slice(),
            [
                Event::Watch { .. },
                Event::PersistenceGap {
                    unconfirmed_write_attempts: 1,
                    unconfirmed_occurrences: 1,
                    ..
                },
                Event::WatchSummary {
                    suppressed_count: 1,
                    reason: FlushReason::Shutdown,
                    ..
                },
                Event::PersistenceGap {
                    unconfirmed_write_attempts: 2,
                    unconfirmed_occurrences: 2,
                    ..
                },
                Event::Phase {
                    phase: Phase::Stopped
                },
            ]
        ));
        assert!(matches!(
            recorder.events().as_slice(),
            [
                Event::PersistenceGap {
                    unconfirmed_write_attempts: 2,
                    unconfirmed_occurrences: 2,
                    ..
                },
                Event::Phase {
                    phase: Phase::Stopped
                },
            ]
        ));
        assert!(lock(&recorder.inner.write_results).is_empty());
    }

    #[test]
    fn the_last_failed_shutdown_summary_gets_one_gap_only_attempt_after_draining() {
        let recorder = Recorder::for_test();
        let observed = watch(context(), 1, ObservationOutcome::Timeout);
        recorder.record(observed);
        recorder.record(observed);
        recorder.record(observed);
        lock(&recorder.inner.write_results)
            .extend([Err(io::ErrorKind::StorageFull.into()), Ok(())]);
        recorder.close_runtime();
        assert!(lock(&recorder.inner.write_results).is_empty());
        assert!(matches!(
            recorder.events().as_slice(),
            [
                Event::Watch { .. },
                Event::PersistenceGap {
                    unconfirmed_write_attempts: 1,
                    unconfirmed_occurrences: 2,
                    last_kind: FailureKind::StorageFull,
                    ..
                }
            ]
        ));
        assert!(lock(&recorder.inner.emission).last.is_none());
        recorder.close_runtime();
        assert_eq!(recorder.events().len(), 2);
    }

    #[test]
    fn repeated_closes_try_one_pending_gap_each_without_amplifying_its_counters() {
        let recorder = Recorder::for_test();
        let observed = watch(context(), 1, ObservationOutcome::Timeout);
        lock(&recorder.inner.write_results).push_back(Err(io::ErrorKind::StorageFull.into()));
        recorder.record(observed);
        lock(&recorder.inner.write_results)
            .extend((0..3).map(|_| Err(io::ErrorKind::PermissionDenied.into())));
        for remaining in (0..3).rev() {
            recorder.close_runtime();
            assert_eq!(lock(&recorder.inner.write_results).len(), remaining);
            let persistence = lock(&recorder.inner.emission);
            assert_eq!((persistence.attempts, persistence.occurrences), (1, 1));
            assert_eq!(
                persistence.last,
                Some((FailureKind::PermissionDenied, None))
            );
            assert!(!persistence.saturated);
        }
        assert!(recorder.events().is_empty());
        recorder.record(observed);
        assert!(recorder.events().is_empty());
        recorder.close_runtime();
        assert!(matches!(
            recorder.events().as_slice(),
            [Event::PersistenceGap {
                unconfirmed_write_attempts: 1,
                unconfirmed_occurrences: 1,
                ..
            }]
        ));
    }

    #[test]
    fn a_post_closure_fatal_write_failure_gets_a_final_persistence_drain() {
        let recorder = Recorder::for_test();
        let mut diagnostics = DaemonLog {
            recorder: recorder.clone(),
            phase: Phase::Store,
        };
        diagnostics.close_runtime();
        lock(&recorder.inner.write_results).push_back(Err(io::ErrorKind::StorageFull.into()));
        diagnostics.problem(&anyhow::anyhow!("invented private failure context"), true);
        diagnostics.close_runtime();
        assert!(matches!(
            recorder.events().as_slice(),
            [Event::PersistenceGap {
                unconfirmed_write_attempts: 1,
                unconfirmed_occurrences: 1,
                ..
            }]
        ));
        assert!(lock(&recorder.inner.emission).last.is_none());
        recorder.record(Event::SessionOpened { context: context() });
        assert_eq!(recorder.events().len(), 1);
    }

    #[test]
    fn diagnostic_categories_never_copy_error_text_or_context() {
        let error = anyhow::anyhow!("password = invented-opaque-value")
            .context("https://provider.invalid/invented-subscription-token");
        let (kind, os_code) = classify(&error);
        let record = serde_json::to_string(&Event::Problem {
            phase: Phase::Store,
            kind,
            os_code,
            fatal: true,
        })
        .unwrap();
        assert_eq!(
            record,
            r#"{"event":"problem","phase":"store","kind":"other","os_code":null,"fatal":true}"#
        );
        let io_error = anyhow::Error::new(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "uuid=11111111-1111-4111-8111-111111111111",
        ))
        .context("another invented private context");
        assert_eq!(classify(&io_error), (FailureKind::PermissionDenied, None));
    }

    #[test]
    fn typed_store_and_bus_errors_keep_safe_causes_without_formatting() {
        let busy = anyhow::Error::new(std::fs::TryLockError::WouldBlock)
            .context("invented store location");
        assert_eq!(classify(&busy), (FailureKind::StoreBusy, None));
        let bus = anyhow::Error::new(zbus::Error::Address("invented private address".into()));
        assert_eq!(classify(&bus), (FailureKind::Bus, None));
        let full = anyhow::Error::new(nix::errno::Errno::ENOSPC);
        assert_eq!(
            classify(&full),
            (FailureKind::StorageFull, Some(nix::libc::ENOSPC))
        );
        let readonly = anyhow::Error::new(io::Error::from_raw_os_error(nix::libc::EROFS));
        assert_eq!(
            classify(&readonly),
            (FailureKind::ReadOnlyFilesystem, Some(nix::libc::EROFS))
        );
    }
}
