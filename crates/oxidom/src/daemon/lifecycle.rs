//! Admission and ownership of work, including work detached from a D-Bus future.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

#[derive(Default)]
struct State {
    closed: bool,
    active: usize,
}

#[derive(Clone, Default)]
pub(super) struct Lifecycle {
    state: Arc<(Mutex<State>, Condvar)>,
    commit: Arc<Mutex<()>>,
}

pub(super) struct Work(Lifecycle);

impl Lifecycle {
    pub(super) fn enter(&self) -> Option<Work> {
        let mut state = oxidom_core::sync::lock(&self.state.0);
        if state.closed {
            return None;
        }
        state.active += 1;
        self.state.1.notify_all();
        Some(Work(self.clone()))
    }

    pub(super) fn is_closed(&self) -> bool {
        oxidom_core::sync::lock(&self.state.0).closed
    }

    /// Serialize the final acceptance of delayed results against closure.
    pub(super) fn commit(&self) -> Option<MutexGuard<'_, ()>> {
        let guard = oxidom_core::sync::lock(&self.commit);
        (!self.is_closed()).then_some(guard)
    }

    pub(super) fn close(&self) {
        let _commit = oxidom_core::sync::lock(&self.commit);
        oxidom_core::sync::lock(&self.state.0).closed = true;
        self.state.1.notify_all();
    }

    /// Return false on closure, including closure before the wait began.
    pub(super) fn wait(&self, duration: Duration) -> bool {
        let state = oxidom_core::sync::lock(&self.state.0);
        let (state, _) = self
            .state
            .1
            .wait_timeout_while(state, duration, |state| !state.closed)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        !state.closed
    }

    pub(super) fn drain(&self) {
        let state = oxidom_core::sync::lock(&self.state.0);
        drop(
            self.state
                .1
                .wait_while(state, |state| state.active != 0)
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
    }

    #[cfg(test)]
    pub(super) fn wait_for_work(&self) {
        let state = oxidom_core::sync::lock(&self.state.0);
        drop(
            self.state
                .1
                .wait_while(state, |state| state.active == 0)
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let mut state = oxidom_core::sync::lock(&self.0.state.0);
        state.active -= 1;
        self.0.state.1.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn terminal_closure_rejects_admission_and_delayed_commits() {
        let lifecycle = Lifecycle::default();
        let work = lifecycle.enter().expect("open");
        lifecycle.close();
        lifecycle.close();
        assert!(lifecycle.enter().is_none());
        assert!(lifecycle.commit().is_none());
        assert!(!lifecycle.wait(Duration::from_secs(60)));
        drop(work);
        lifecycle.drain();
    }

    #[test]
    fn draining_waits_for_detached_work_and_wakes_periodic_workers() {
        let lifecycle = Lifecycle::default();
        let work = lifecycle.enter().expect("open");
        let worker_lifecycle = lifecycle.clone();
        let (started, ready) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _work = work;
            started.send(()).unwrap();
            assert!(!worker_lifecycle.wait(Duration::from_secs(3600)));
        });
        ready.recv().unwrap();
        lifecycle.close();
        lifecycle.drain();
        worker.join().unwrap();
    }

    #[test]
    fn an_accepted_commit_finishes_before_terminal_closure() {
        let lifecycle = Lifecycle::default();
        let commit = lifecycle.commit().expect("open");
        let closing = lifecycle.clone();
        let (started, ready) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            closing.close();
        });
        ready.recv().unwrap();
        assert!(!lifecycle.is_closed());
        drop(commit);
        worker.join().unwrap();
        assert!(lifecycle.commit().is_none());
    }
}
