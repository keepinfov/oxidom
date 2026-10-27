//! Binding freshness is independent of ordering actions within one daemon.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct RetiredWork {
    pub live: Arc<AtomicBool>,
    pub changed: Arc<AtomicBool>,
}

impl Drop for RetiredWork {
    fn drop(&mut self) {
        if !self.live.load(Ordering::SeqCst) {
            self.changed.store(true, Ordering::SeqCst);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Generation(u64);

pub(super) struct RecoveryState {
    generation: Generation,
    ready: bool,
    in_flight: bool,
}

impl RecoveryState {
    pub fn new(ready: bool) -> Self {
        Self {
            generation: Generation(0),
            ready,
            in_flight: false,
        }
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn ready(&self) -> bool {
        self.ready
    }

    pub fn accepts(&self, generation: Generation) -> bool {
        self.ready && self.generation == generation
    }

    pub fn begin(&mut self) -> Option<Generation> {
        if self.in_flight {
            return None;
        }
        self.generation = Generation(self.generation.0.checked_add(1)?);
        self.ready = false;
        self.in_flight = true;
        Some(self.generation)
    }

    pub fn failed(&mut self, generation: Generation) {
        if self.generation == generation {
            self.in_flight = false;
        }
    }

    pub fn finish(&mut self, generation: Generation) -> bool {
        if self.generation != generation || !self.in_flight {
            return false;
        }
        self.in_flight = false;
        self.ready = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_work_requests_fresh_reads_without_accepting_its_old_outcome() {
        let live = Arc::new(AtomicBool::new(true));
        let changed = Arc::new(AtomicBool::new(false));
        let pending = RetiredWork {
            live: live.clone(),
            changed: changed.clone(),
        };
        live.store(false, Ordering::SeqCst);
        assert!(!changed.load(Ordering::SeqCst));
        drop(pending);
        assert!(changed.swap(false, Ordering::SeqCst));
        let current = RetiredWork {
            live: Arc::new(AtomicBool::new(true)),
            changed: changed.clone(),
        };
        drop(current);
        assert!(!changed.load(Ordering::SeqCst));
    }

    #[test]
    fn retirement_rejects_all_old_payloads_before_a_successor_is_ready() {
        let mut state = RecoveryState::new(true);
        let old = state.generation();
        assert!(state.accepts(old));
        let pending = state.begin().unwrap();
        assert!(!state.accepts(old));
        assert!(!state.accepts(pending));
        assert!(!state.ready());
        assert!(state.begin().is_none(), "only one hydration may run");
        assert!(state.finish(pending));
        assert!(state.accepts(pending));
        assert!(!state.accepts(old));
        assert!(!state.finish(pending), "completion is consumed once");
    }

    #[test]
    fn failed_or_superseded_hydration_never_restores_control() {
        let mut state = RecoveryState::new(false);
        let first = state.begin().unwrap();
        state.failed(first);
        assert!(!state.ready());
        let second = state.begin().unwrap();
        assert!(!state.finish(first));
        state.failed(first);
        assert!(
            state.begin().is_none(),
            "obsolete failure cannot release a new worker"
        );
        assert!(state.finish(second));
        assert!(!state.accepts(first));
        assert!(state.accepts(second));
    }

    #[test]
    fn stale_operation_results_cannot_retire_an_identical_successor_operation() {
        let mut state = RecoveryState::new(true);
        let old = state.generation();
        let next = state.begin().unwrap();
        state.finish(next);
        let mut completed = false;
        let mut new_operation = Some("apply settings");
        if state.accepts(old) {
            new_operation.take();
            completed = true;
        }
        assert!(!completed);
        assert_eq!(new_operation, Some("apply settings"));
    }
}
