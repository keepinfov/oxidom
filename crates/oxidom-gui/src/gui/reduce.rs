//! The part of the GUI that decides *what* changed, kept apart from the part
//! that pushes pixels.
//!
//! Everything the 500 ms poll folds into the window used to live inside one
//! `borrow_mut()` in `window.rs`, interleaved with widget calls — which made it
//! both unreviewable and untestable, since exercising it needed a display. This
//! module owns that state and the pure transition over it; the widget layer
//! keeps the `Rc`s, the D-Bus client and the toasts, and replays the [`Effect`]s
//! this returns once the borrow is gone.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use oxidom_core::client::DaemonSource;
use oxidom_core::ipc::{
    LatencyReading, PROBE_STATE_VERSION, ProbeDetail, ProbeFailure, ProbeHistory, ProbeRoute,
    ProbeState, ProfileEntry, RuntimeInfo, SelectionInfo, SessionInfo, StatusInfo,
};
use oxidom_core::logbook::LogSlice;
use oxidom_core::model::{OutboundSpec, Subscription};
use oxidom_core::pool::{PoolKind, PoolQuery, Strategy};
use oxidom_core::profile::RouteMode;
use oxidom_core::versions::Versions;
use oxidom_core::xray::core::Status;

use super::operation::{UiOperation, UiOperationKind};
use super::prefs::{GroupKind, ServerGroup};
use super::server_card::{LatencyAge, LatencyState, method_text};

/// One round of daemon polling, produced off the main thread.
pub(super) struct PolledSnapshot {
    pub status: StatusInfo,
    pub probe: ProbeState,
    /// Only what the daemon has logged since the cursor sent with this round.
    /// See [`super::logfeed::LogFeed`].
    pub logs: LogSlice,
    /// [`SnapshotState::state_epoch`] as it stood *before* the first D-Bus read
    /// of this round. A snapshot whose epoch fell behind describes a world the
    /// user has already changed, and applying it is what makes the connection
    /// UI flicker back to its pre-click frame half a second after the click.
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RouteTarget {
    profile: Option<String>,
    server_id: String,
}

/// Everything a poll snapshot reads or writes. No widgets, no `Rc`, no D-Bus —
/// the fields the window keeps beside this one (`client`, `subscriptions`,
/// `selected_id`) are deliberately absent because [`reduce`] never touches them.
pub(super) struct SnapshotState {
    /// Profile the header, the tray and a card click act on. Only the header
    /// switcher writes it, and it starts at `default` on every launch: the
    /// single-profile user must not be shown a selection they never made.
    pub selected_profile: String,
    /// Server the tunnel is (optimistically) running for; drives the highlight.
    pub connected_id: Option<String>,
    /// Profile that brought the tunnel up, as reported by the daemon.
    pub active_profile: Option<String>,
    /// Every runtime session. The compatibility fields above still drive the
    /// header; this list keeps cards and the system-proxy owner honest.
    pub sessions: Vec<SessionInfo>,
    /// Connected profiles and pool memberships grouped by server.
    pub connected_profiles: HashMap<String, ServerProfiles>,
    /// Direct server measurements as last seen.
    pub readings: HashMap<String, LatencyReading>,
    /// Connection measurements keyed by profile.
    pub proxied: HashMap<String, LatencyReading>,
    /// Last successful measurement of the connected server, tagged with the
    /// server id it belongs to. Shown (dimmed) in the status chips whenever
    /// no probe has confirmed a fresh reading for the *current* connection
    /// yet, so the chip never goes blank right after a (re)connect; the id
    /// tag is what keeps a previous server's number from leaking onto a
    /// different one — it is intentionally never reset on disconnect, since
    /// a stale-but-correct reading for the same server is exactly what
    /// should resurface on reconnect.
    last_active_latency: Option<(RouteTarget, u32)>,
    /// Ids whose card is showing a spinner, and what that spinner is waiting
    /// for. Entries appear here before the D-Bus request that creates them even
    /// lands, so the daemon's own sets cannot be mirrored directly.
    pub checking: HashMap<String, ProbeWait>,
    /// Ids whose failed probe should raise a toast (explicit per-card ping).
    pub notify_probe: HashSet<String>,
    /// Ids from a whole-subscription check. These report only failures of this
    /// machine — no core, no network — because those explain every card at
    /// once and are invisible otherwise. A single silent server among fifty is
    /// its card's business, not a toast's.
    pub notify_local: HashSet<String>,
    pub operation: Option<UiOperation>,
    /// Optimistic status shown while a job is in flight. Written only through
    /// [`SnapshotState::pin_status`], so the deadline below always has a start.
    pending_status: Option<Status>,
    /// The profile the optimistic status belongs to. A single operation may be
    /// in flight, but changing the header selection must not move its status to
    /// another session.
    pending_profile: String,
    /// When `pending_status` was pinned. The pin outranks every daemon status
    /// while it stands, so a pin nobody ever retires — a daemon that restarted
    /// mid-connect, an operation whose worker died — would freeze the UI on
    /// "Connecting…" for the rest of the session with no way out.
    pending_since: Instant,
    /// Latest status reported by the daemon.
    pub daemon_status: Status,
    /// The server whose connection attempt failed, while that failure is still
    /// what the UI is reporting. Separate from `connected_id`, which means
    /// "the tunnel is running for this one" and drives the system proxy.
    pub failed_id: Option<String>,
    /// The server the *daemon* would measure through the tunnel right now.
    ///
    /// Deliberately not [`SnapshotState::connected_id`], which may be running
    /// ahead of the daemon optimistically: this is the condition the daemon
    /// itself applies when it picks a probe's route, and judging a reading's
    /// route against anything else would call it superseded on every connect
    /// and re-probe forever.
    route_target: Option<RouteTarget>,
    /// Last daemon error already shown to the user, so the 500 ms poll does
    /// not re-toast the same failure on every tick.
    pub notified_error: Option<String>,
    /// Whether the "your daemon is too old" warning has been raised. Same
    /// reason as above: the condition holds on every tick until the daemon is
    /// restarted, and it is worth saying exactly once.
    notified_outdated: bool,
    /// Bumped by every action that changes what the daemon is about to report.
    /// Polling is asynchronous and takes three blocking D-Bus calls, so without
    /// a generation there is nothing to tell a snapshot from before the user's
    /// click apart from one taken after it.
    pub state_epoch: u64,
}

/// One card's wait for a probe result.
#[derive(Debug, Clone, Copy)]
pub(super) struct ProbeWait {
    /// When the spinner went up.
    pub since: Instant,
    /// Whether the daemon has been seen holding this id — running it or
    /// queueing it. Until then its absence from both sets means nothing: the
    /// GUI raises the spinner optimistically, before the D-Bus request that
    /// asks for the probe has even been sent, so a tick landing in that window
    /// would otherwise retire a probe that has not started.
    pub acked: bool,
    /// Whether the deadline has passed with the daemon still claiming this id.
    /// The spinner is down and the card has been repainted, but the wait is
    /// *kept* so the next tick does not read the id as new and raise the
    /// spinner again. It is dropped once the daemon lets the id go.
    pub given_up: bool,
}

impl ProbeWait {
    pub fn new(since: Instant) -> Self {
        Self {
            since,
            acked: false,
            given_up: false,
        }
    }

    /// Whether this wait should still be drawn as a check in progress.
    fn is_running(&self) -> bool {
        !self.given_up
    }
}

impl SnapshotState {
    pub fn new(status: &StatusInfo) -> Self {
        Self {
            selected_profile: "default".to_string(),
            connected_id: status.active_id.clone(),
            active_profile: status.active_profile.clone(),
            sessions: status.sessions.clone(),
            connected_profiles: connected_profiles(status),
            readings: HashMap::new(),
            proxied: HashMap::new(),
            last_active_latency: None,
            checking: HashMap::new(),
            notify_probe: HashSet::new(),
            notify_local: HashSet::new(),
            operation: None,
            pending_status: None,
            pending_profile: "default".to_string(),
            pending_since: Instant::now(),
            daemon_status: status.to_status(),
            failed_id: None,
            route_target: route_target(status),
            // Left unset on purpose: a GUI opening onto an already-broken
            // daemon should toast once, on its first poll.
            notified_error: None,
            notified_outdated: false,
            state_epoch: 0,
        }
    }

    /// What the UI as a whole is showing: an optimistic transition outranks
    /// the daemon's own view until it is retired.
    pub fn current_status(&self) -> Status {
        self.pending_status_for(&self.selected_profile)
            .cloned()
            .unwrap_or_else(|| self.daemon_status.clone())
    }

    /// Show `status` ahead of the daemon: either optimistically, while the
    /// user's action is in flight, or as the outcome a completion handler
    /// already knows and the daemon has not caught up with yet.
    pub fn pin_status(&mut self, profile: &str, status: Status, now: Instant) {
        self.pending_profile = profile.to_string();
        self.pending_status = Some(status);
        self.pending_since = now;
    }

    fn pending_status_for(&self, profile: &str) -> Option<&Status> {
        if self.pending_profile == profile {
            self.pending_status.as_ref()
        } else {
            None
        }
    }

    #[cfg(test)]
    pub fn is_pinned(&self) -> bool {
        self.pending_status.is_some()
    }

    /// How `id`'s badge should look right now. The one place that decides it,
    /// so a card rebuilt from scratch, a card updated by the poll and a card
    /// swept for age cannot disagree about what the same reading means.
    pub fn card_state(&self, id: &str, now_unix_ms: u64) -> LatencyState {
        let is_active = self.is_active(id);
        latency_state(
            self.shown_reading(id),
            self.is_checking(id),
            is_active,
            now_unix_ms,
        )
    }

    /// Why `id`'s last check produced no number, for the expanded card.
    ///
    /// Reads the same reading the badge above was decided from, deliberately:
    /// two lookups that could pick differently would let a card show a dash
    /// for one check and the reason from another. A check in flight reports
    /// nothing — the previous reason is about a measurement that is being
    /// replaced, and leaving it under a spinner reads as the reason the
    /// spinner is spinning.
    pub fn card_failure(&self, id: &str, now_unix_ms: u64) -> Option<FailureReport> {
        if self.is_checking(id) {
            return None;
        }
        failure_report(self.shown_reading(id), now_unix_ms)
    }

    /// Whether the tunnel is currently carried by this server, i.e. whether a
    /// reading taken through it is still about anything.
    fn is_active(&self, id: &str) -> bool {
        self.route_target
            .as_ref()
            .is_some_and(|target| target.server_id == id)
    }

    /// The reading a card is showing: the proxied one while this server
    /// carries the tunnel, its own otherwise.
    fn shown_reading(&self, id: &str) -> Option<&LatencyReading> {
        if self.is_active(id) {
            self.route_target
                .as_ref()
                .and_then(|target| proxied_reading(self, target))
                // Compatibility with an A2 daemon: it put the active
                // connection reading in the server-id map.
                .or_else(|| {
                    self.readings
                        .get(id)
                        .filter(|reading| reading.route == ProbeRoute::Proxied)
                })
        } else {
            self.readings.get(id)
        }
    }

    /// A card is checking while a wait is live. A wait the deadline has given
    /// up on is still held — to keep the id from reading as new — but it is no
    /// longer a check in progress, and drawing it as one is the thing the
    /// deadline exists to stop.
    pub(super) fn is_checking(&self, id: &str) -> bool {
        self.checking.get(id).is_some_and(ProbeWait::is_running)
    }

    /// Drop the pin without waiting for the daemon to contradict it. Only for
    /// the path where the operation left no outcome at all — a worker that
    /// died — since there the pin describes an action that may never have been
    /// carried out, and holding it would show a transition that never happened.
    pub fn clear_pin(&mut self) {
        self.pending_status = None;
    }
}

/// How long a pinned status may outrank the daemon. Long enough that a slow
/// connect is never cut short, short enough that a user staring at a wrong
/// "Connecting…" gets the truth back without restarting the app.
const PENDING_STATUS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

/// Something the widget layer has to do once the state borrow is released.
/// Returned in the order it must be applied.
pub(super) enum Effect {
    Latency(String, LatencyState),
    /// Ask for a fresh measurement of this id: the one on record was taken
    /// over a route that no longer applies. Bounded by construction — only the
    /// server that just stopped or started carrying the tunnel can be in that
    /// position, so this is at most two ids per snapshot, never a sweep.
    Reprobe(String),
    ToastUnreachable,
    ToastNoNetwork,
    /// The probe never reached the server because something here stopped it.
    /// Carries an action to Settings, since the usual cause — no Xray core —
    /// is fixed there and nowhere else.
    ToastProbeDidNotRun,
    ConnectionError(String),
    /// The daemon is older than the reading contract. Its numbers arrive
    /// undated and unattributed, so the GUI reports nothing rather than
    /// guessing — and says why, since a list that is silently all-unmeasured
    /// otherwise reads as a network problem.
    DaemonOutdated,
}

/// How long a card may wait for a probe result before the spinner is retired
/// as lost. Generous on purpose: a "check all" over a large subscription runs
/// eight at a time, each with its own timeout, so a long *legitimate* wait is
/// normal and cutting it short would misreport a server as unmeasured. An HTTP
/// probe starts a core and makes a request through it — up to ten seconds per
/// server in the worst case — so a hundred-server subscription can legitimately
/// keep the last card waiting for over two minutes. This is the absolute
/// backstop: a daemon that lost track of a probe it still claims to be running
/// would otherwise keep the card spinning for the session.
///
/// Passing it retires the spinner but *keeps* the wait, because the daemon is
/// still naming the id and a forgotten wait is indistinguishable from a new
/// one — the next tick would adopt it and put the spinner straight back, which
/// left the backstop unable to stop anything and the card flickering once a
/// deadline instead of settling.
pub(super) const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// How long an unacknowledged probe keeps its spinner. Covers the window
/// between raising the spinner and the daemon reporting the id back; past it
/// the request is assumed lost.
const PROBE_ACK_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Fold one poll snapshot into the state. `None` means the snapshot predates
/// the user's last action and was dropped whole.
///
/// The check has to gate *all* of `apply_snapshot`, not just the mutations
/// here: a stale round also carries a stale log tail, and — the reason this
/// exists at all — drives `reconcile_system_proxy` into clearing the GNOME
/// proxy it just applied.
///
/// `window_visible` is passed in rather than read from the window: it is the
/// only GTK fact this decision needs, and taking it as an argument is what
/// keeps the whole transition testable without a display.
///
/// `now_unix_ms` is the wall clock the daemon dates its readings against; it is
/// passed alongside the monotonic `now` rather than read here, because only one
/// of the two is meaningful across a process boundary and only the other is
/// safe to measure durations with.
pub(super) fn reduce(
    state: &mut SnapshotState,
    snapshot: &PolledSnapshot,
    now: Instant,
    now_unix_ms: u64,
    window_visible: bool,
) -> Option<Vec<Effect>> {
    if snapshot.epoch < state.state_epoch {
        return None;
    }
    let mut effects = Vec::new();
    let mut toast_unreachable = false;
    let mut toast_no_network = false;
    let mut toast_not_run = false;
    let mut new_error: Option<String> = None;
    state.daemon_status = snapshot.status.to_status();
    state.active_profile = snapshot.status.active_profile.clone();
    state.sessions.clone_from(&snapshot.status.sessions);
    state.connected_profiles = connected_profiles(&snapshot.status);
    // Only the daemon's override path knows which server a failure belongs to;
    // failures that never reached the daemon are named by the connect handler
    // instead, which is why this only ever adds.
    if let Some(id) = &snapshot.status.error_id {
        state.failed_id = Some(id.clone());
    }

    // Nothing below needs a special case for this: an outdated daemon sends no
    // `readings` at all, so every card falls through to "unmeasured" on its
    // own. All that is missing is telling the user why.
    if snapshot.probe.version < PROBE_STATE_VERSION && !state.notified_outdated {
        state.notified_outdated = true;
        effects.push(Effect::DaemonOutdated);
    }

    // Whose readings are proxied changes here, and it changes the meaning of
    // numbers that did not move at all — so the cards it affects have to be
    // recomputed even though nothing about them was polled. At most two: the
    // server that stopped carrying the tunnel, and the one that started.
    let route_target = route_target(&snapshot.status);
    let rerouted: Vec<String> = if route_target == state.route_target {
        Vec::new()
    } else {
        let touched = state
            .route_target
            .iter()
            .chain(route_target.iter())
            .map(|target| target.server_id.clone())
            .collect();
        state.route_target = route_target;
        touched
    };

    // An id the daemon holds — running it, or merely queueing it behind
    // `MAX_CONCURRENT_PROBES` — is a probe that has not produced a number yet.
    let held: HashSet<&String> = snapshot
        .probe
        .running
        .iter()
        .chain(snapshot.probe.queued.iter())
        .collect();

    for (id, reading) in &snapshot.probe.readings {
        if state.readings.get(id) != Some(reading) {
            state.readings.insert(id.clone(), *reading);
            // Record the reading, but do not put it on a card that is waiting
            // for a fresh one: the reading still in the daemon's map belongs to
            // the *previous* measurement, and showing it now is the fake ping.
            // The card gets its number when the probe it is waiting on retires.
            if !held.contains(id) && !state.checking.contains_key(id) {
                let shown = state.card_state(id, now_unix_ms);
                if shown == LatencyState::Superseded {
                    effects.push(Effect::Reprobe(id.clone()));
                }
                effects.push(Effect::Latency(id.clone(), shown));
            }
            // Compatibility with an A2 daemon, which has no `proxied` map.
            if state.route_target.as_ref().is_some_and(|target| {
                target.server_id == *id && target.profile == state.active_profile
            }) && reading.route == ProbeRoute::Proxied
                && let Some(ms) = reading.value
                && let Some(target) = state.route_target.clone()
            {
                state.last_active_latency = Some((target, ms));
            }
            let asked = state.notify_probe.remove(id);
            let swept = state.notify_local.remove(id);
            match probe_toast(reading, asked, swept) {
                Some(ProbeToast::DidNotRun) => toast_not_run = true,
                Some(ProbeToast::NoNetwork) => toast_no_network = true,
                Some(ProbeToast::Unreachable) => toast_unreachable = true,
                None => {}
            }
        }
    }
    for (profile, reading) in &snapshot.probe.proxied {
        if state.proxied.get(profile) == Some(reading) {
            continue;
        }
        state.proxied.insert(profile.clone(), *reading);
        let target_id = state
            .route_target
            .as_ref()
            .filter(|target| target.profile.as_deref() == Some(profile.as_str()))
            .map(|target| target.server_id.clone());
        let Some(id) = target_id else {
            continue;
        };
        if !held.contains(&id) && !state.checking.contains_key(&id) {
            effects.push(Effect::Latency(
                id.clone(),
                state.card_state(&id, now_unix_ms),
            ));
        }
        if let Some(ms) = reading.value
            && let Some(target) = state.route_target.clone()
        {
            state.last_active_latency = Some((target, ms));
        }
        let asked = state.notify_probe.remove(&id);
        let swept = state.notify_local.remove(&id);
        match probe_toast(reading, asked, swept) {
            Some(ProbeToast::DidNotRun) => toast_not_run = true,
            Some(ProbeToast::NoNetwork) => toast_no_network = true,
            Some(ProbeToast::Unreachable) => toast_unreachable = true,
            None => {}
        }
    }
    // The daemon forgets readings for servers that no longer exist; mirroring
    // that keeps a number from outliving the server it belongs to. Only for a
    // daemon that speaks readings at all — an outdated one sends an empty map,
    // which means "I did not say", not "they are gone".
    if snapshot.probe.version >= PROBE_STATE_VERSION {
        state
            .readings
            .retain(|id, _| snapshot.probe.readings.contains_key(id));
        state
            .proxied
            .retain(|profile, _| snapshot.probe.proxied.contains_key(profile));
    }
    for id in &held {
        match state.checking.get_mut(*id) {
            Some(wait) => wait.acked = true,
            None => {
                state.checking.insert(
                    (*id).clone(),
                    ProbeWait {
                        since: now,
                        acked: true,
                        given_up: false,
                    },
                );
                effects.push(Effect::Latency((*id).clone(), LatencyState::Checking));
            }
        }
    }
    // A probe is done when the daemon no longer holds it — *not* when a reading
    // for it exists. Keying off the reading is what made a card show the number
    // from its previous measurement as if it were the result of the one still
    // waiting in the queue.
    //
    // Two different answers, so two lists. An id the daemon has let go is
    // *finished*: forget the wait entirely. An id the daemon still claims past
    // the deadline is *given up on*: take the spinner down but keep the wait,
    // because forgetting it would let the next tick read the id as new and
    // raise the spinner all over again — which is what made the backstop
    // ineffective and the card flicker instead of settling.
    let mut finished: Vec<String> = Vec::new();
    let mut given_up: Vec<String> = Vec::new();
    for (id, wait) in &state.checking {
        if held.contains(id) {
            if wait.is_running() && now.duration_since(wait.since) > PROBE_DEADLINE {
                given_up.push(id.clone());
            }
            continue;
        }
        if wait.acked || now.duration_since(wait.since) > PROBE_ACK_GRACE {
            finished.push(id.clone());
        }
    }
    for id in given_up {
        if let Some(wait) = state.checking.get_mut(&id) {
            wait.given_up = true;
        }
        push_card(&mut effects, state, &id, now_unix_ms);
    }
    for id in finished {
        state.checking.remove(&id);
        // A reading that never arrived leaves the card unmeasured rather than
        // unreachable: nothing was measured, so nothing may be claimed.
        push_card(&mut effects, state, &id, now_unix_ms);
    }
    // Cards whose reading did not change but whose *meaning* did, because the
    // tunnel moved out from under it.
    for id in rerouted {
        if !state.is_checking(&id) {
            push_card(&mut effects, state, &id, now_unix_ms);
        }
    }

    // Retire the pin once the daemon has stopped reporting the world as it was
    // before the action that pinned it. Nothing else does this: the completion
    // handler used to clear the pin unconditionally, which is precisely what
    // let a snapshot taken before the click repaint the pre-click frame.
    //
    // The snapshot is known to be no older than the pin (it passed the epoch
    // check above), so "the daemon still says what it said before" is a
    // statement about the daemon lagging, not about a stale read.
    if state.operation.is_none() {
        let reported_status = reported_status_for(state, &state.pending_profile);
        let contradicted = match &state.pending_status {
            None => false,
            // Optimistic: the user asked to go up. Anything but the old
            // "disconnected" means the daemon has begun to agree.
            Some(Status::Connecting | Status::Connected) => {
                !matches!(reported_status, Status::Disconnected)
            }
            // Terminal: a completion pinned the outcome, or the user asked to
            // go down. Anything but the old "up" means the daemon has caught up.
            Some(Status::Disconnected | Status::Error(_)) => {
                !matches!(reported_status, Status::Connected | Status::Connecting)
            }
        };
        if contradicted || now.duration_since(state.pending_since) > PENDING_STATUS_DEADLINE {
            state.pending_status = None;
        }
    }
    // Key off the daemon's own view, not `current_status`: the latter prefers
    // `pending_status`, which may still hold an optimistic transition the
    // daemon has not confirmed yet.
    let error = match &state.daemon_status {
        Status::Error(message) => Some(message.clone()),
        _ => None,
    };
    if error != state.notified_error {
        state.notified_error = error.clone();
        // Record the transition either way, but stay quiet while hidden:
        // ToastOverlay queues, and `gui --background` would otherwise dump an
        // hour of stale toasts when first opened.
        new_error = error.filter(|_| window_visible);
    }
    // While no optimistic transition is in flight, the daemon's view of the
    // active server wins.
    if state.operation.is_none()
        && state.pending_status_for("default").is_none()
        && snapshot.status.active_id != state.connected_id
    {
        // `last_active_latency` is tagged with the id it belongs to and only
        // ever read when that tag matches `connected_id` (see its doc comment),
        // so it self-invalidates here without needing an explicit reset.
        state.connected_id = snapshot.status.active_id.clone();
    }

    // The failure is over the moment the UI stops reporting one — a reconnect
    // pinning "Connecting…", or the daemon coming back with anything else.
    if !matches!(state.current_status(), Status::Error(_)) {
        state.failed_id = None;
    }

    if toast_unreachable {
        effects.push(Effect::ToastUnreachable);
    }
    if toast_no_network {
        effects.push(Effect::ToastNoNetwork);
    }
    if toast_not_run {
        effects.push(Effect::ToastProbeDidNotRun);
    }
    // Failures the user never triggered — a crashed core, a tunnel torn down by
    // its own latency check — reach the screen only here.
    if let Some(error) = new_error {
        effects.push(Effect::ConnectionError(error));
    }
    Some(effects)
}

/// Queue the card's current appearance, unless this snapshot already decided
/// it, and ask for a fresh measurement when the reading on record was taken
/// over a route that no longer applies.
fn push_card(effects: &mut Vec<Effect>, state: &SnapshotState, id: &str, now_unix_ms: u64) {
    if effects
        .iter()
        .any(|effect| matches!(effect, Effect::Latency(updated, _) if updated == id))
    {
        return;
    }
    let shown = state.card_state(id, now_unix_ms);
    if shown == LatencyState::Superseded {
        effects.push(Effect::Reprobe(id.to_string()));
    }
    effects.push(Effect::Latency(id.to_string(), shown));
}

/// The server the daemon would measure *through the tunnel*, which is exactly
/// the condition `daemon::Shared::probe_target` applies when it picks a route.
/// Mirrored rather than approximated on purpose — see
/// [`SnapshotState::route_target`].
fn route_target(status: &StatusInfo) -> Option<RouteTarget> {
    if !matches!(status.to_status(), Status::Connected) {
        return None;
    }
    Some(RouteTarget {
        profile: status.active_profile.clone(),
        server_id: status.active_id.clone()?,
    })
}

fn proxied_reading<'a>(
    state: &'a SnapshotState,
    target: &RouteTarget,
) -> Option<&'a LatencyReading> {
    target
        .profile
        .as_deref()
        .and_then(|profile| state.proxied.get(profile))
}

/// Which profiles visibly use each server. Kept pure because this is the
/// multi-session fact every card consumes; widgets must not independently
/// reinterpret the compatibility `active_id`.
pub(super) fn connected_profiles(status: &StatusInfo) -> HashMap<String, ServerProfiles> {
    let mut by_server = HashMap::<String, ServerProfiles>::new();
    for session in &status.sessions {
        if session.state != "connected" {
            continue;
        }
        if session.selection.kind == "pool" {
            for member in &session.selection.members {
                by_server
                    .entry(member.server_id.clone())
                    .or_default()
                    .in_pool
                    .push(session.profile.clone());
            }
            continue;
        }
        let Some(server_id) = &session.server_id else {
            continue;
        };
        by_server
            .entry(server_id.clone())
            .or_default()
            .connected
            .push(session.profile.clone());
    }
    // An older daemon has no session list. Preserve the exact one-tunnel
    // appearance it had before this additive field existed.
    if by_server.is_empty()
        && matches!(status.to_status(), Status::Connected)
        && let Some(server_id) = &status.active_id
    {
        by_server
            .entry(server_id.clone())
            .or_default()
            .connected
            .push(
                status
                    .active_profile
                    .clone()
                    .unwrap_or_else(|| "default".to_string()),
            );
    }
    by_server
}

/// The two visually distinct relationships a server card can have to running
/// profiles. Pool membership is intentionally not folded into `connected`:
/// a rotating pool has no single active server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ServerProfiles {
    pub connected: Vec<String>,
    pub in_pool: Vec<String>,
}

/// One subscription choice shown by the server filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FilterOption {
    pub value: String,
    pub label: String,
}

/// Countries that can contribute an actual pool outbound.
pub(super) fn available_countries(groups: &[Subscription]) -> Vec<String> {
    let mut values = pool_servers(groups)
        .filter_map(|server| server.country.as_deref())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

/// Protocols that can contribute an actual pool outbound.
pub(super) fn available_protocols(groups: &[Subscription]) -> Vec<String> {
    let mut values = pool_servers(groups)
        .map(|server| server.protocol.as_str().to_string())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

/// Every pool-eligible server, grouped under the subscription it belongs to.
///
/// The user asked to exclude "specific servers of a specific subscription", and
/// a flat list of two hundred names does not answer that: two providers reusing
/// the same city name are indistinguishable until the subscription is shown
/// above them. The value written is the server id rather than its alias — an
/// alias is a display name the user can change, and an exclusion that quietly
/// stopped applying after a rename would be worse than no exclusion.
pub(super) fn excludable_servers(groups: &[Subscription]) -> Vec<(String, Vec<FilterOption>)> {
    groups
        .iter()
        .filter_map(|group| {
            let servers: Vec<FilterOption> = group
                .servers
                .iter()
                .filter(|server| !matches!(&server.spec, OutboundSpec::XrayProfile { .. }))
                // The provider's name as it stands, flag and all. Stripping the
                // flag is only right where one is drawn beside the label — on a
                // card — and these are plain checkbox rows, so cutting it here
                // just deleted the one glyph that says where the node is.
                .map(|server| FilterOption {
                    value: server.id.clone(),
                    label: server.name.clone(),
                })
                .collect();
            (!servers.is_empty()).then(|| (group.name.clone(), servers))
        })
        .collect()
}

/// Subscription ids and labels that contain at least one pool-eligible node.
pub(super) fn available_subscriptions(groups: &[Subscription]) -> Vec<FilterOption> {
    groups
        .iter()
        .filter(|group| {
            group
                .servers
                .iter()
                .any(|server| !matches!(&server.spec, OutboundSpec::XrayProfile { .. }))
        })
        .map(|group| FilterOption {
            value: group.id.clone(),
            label: group.name.clone(),
        })
        .collect()
}

/// The rule in the words the filter uses, for a tooltip or a radio label.
pub(super) fn describe_rule(query: &PoolQuery) -> String {
    let mut parts = Vec::new();
    if !query.countries.is_empty() {
        parts.push(query.countries.join(", ").to_uppercase());
    }
    if !query.protocols.is_empty() {
        parts.push(query.protocols.join(", "));
    }
    if !query.subscriptions.is_empty() {
        parts.push(format!("{} subscription(s)", query.subscriptions.len()));
    }
    if !query.exclude.is_empty() {
        parts.push(format!("except {}", query.exclude.len()));
    }
    if parts.is_empty() {
        "every server".to_string()
    } else {
        parts.join(" · ")
    }
}

/// A pool of either kind in one line, for somewhere that only reports.
///
/// Shared with the profile editor so the two cannot describe the same pool
/// differently — which is exactly what happens when a dialog grows its own
/// wording for something the main UI already names.
pub(super) fn describe_pool(query: &PoolQuery) -> String {
    match query.kind() {
        PoolKind::List => format!(
            "{} server{}, chosen by hand",
            query.members.len(),
            if query.members.len() == 1 { "" } else { "s" }
        ),
        PoolKind::Rule => describe_rule(query),
    }
}

/// Whether the filter widgets currently hold exactly what a saved group holds.
///
/// The group's own name is not part of the comparison: renaming a group does
/// not make the view "modified", because nothing about which servers are shown
/// has changed.
pub(super) fn query_equals_group(current: &PoolQuery, group: &ServerGroup) -> bool {
    let mut saved = group.query.clone();
    saved.name = current.name.clone();
    &saved == current
}

/// Add or replace a group by id, keeping chip order stable.
///
/// Replacing in place rather than removing and pushing: saving over "Europe"
/// must not send its chip to the end of the row.
pub(super) fn upsert_group(groups: &[ServerGroup], group: ServerGroup) -> Vec<ServerGroup> {
    let mut groups = groups.to_vec();
    match groups.iter_mut().find(|saved| saved.id == group.id) {
        Some(saved) => *saved = group,
        None => groups.push(group),
    }
    groups
}

/// The group after starring or unstarring one server.
///
/// Membership is by server id, never by alias: an alias is a display name the
/// user can change, and a favourite that fell out of the list because it was
/// renamed would look like data loss.
pub(super) fn toggled_member(group: &ServerGroup, server_id: &str) -> ServerGroup {
    let mut group = group.clone();
    match group
        .query
        .members
        .iter()
        .position(|member| member == server_id)
    {
        Some(index) => {
            group.query.members.remove(index);
        }
        None => group.query.members.push(server_id.to_string()),
    }
    group
}

/// The servers a group currently stands for.
///
/// An empty *list* selects nothing, which is why the kind is stored rather than
/// inferred: `PoolQuery` with no members and no filters is an unfiltered rule,
/// i.e. every server, and a Favourites nobody has starred yet must not mean
/// that.
pub(super) fn group_member_ids(group: &ServerGroup, groups: &[Subscription]) -> Vec<String> {
    if group.kind == GroupKind::List && group.query.members.is_empty() {
        return Vec::new();
    }
    filtered_ids(&group.query, groups)
}

/// Groups a server belongs to right now, by name — for the sentence a deletion
/// confirmation adds so nobody loses a favourite without being told.
pub(super) fn groups_holding<'a>(
    saved: &'a [ServerGroup],
    groups: &[Subscription],
    server_id: &str,
) -> Vec<&'a str> {
    saved
        .iter()
        .filter(|group| {
            group_member_ids(group, groups)
                .iter()
                .any(|id| id == server_id)
        })
        .map(|group| group.name.as_str())
        .collect()
}

/// Apply a saved display order to the daemon's subscription list.
///
/// The saved order is advisory, not authoritative: ids it does not mention keep
/// their natural position *after* the ones it does, and ids it mentions that no
/// longer exist are dropped. A subscription added since the order was saved
/// therefore appears at the end rather than vanishing.
pub(super) fn ordered_subscriptions(
    groups: &[Subscription],
    order: &[String],
) -> Vec<Subscription> {
    let mut remaining: Vec<Option<Subscription>> = groups.iter().cloned().map(Some).collect();
    let mut ordered = Vec::with_capacity(groups.len());
    for id in order {
        if let Some(slot) = remaining
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|group| &group.id == id))
            && let Some(group) = slot.take()
        {
            ordered.push(group);
        }
    }
    ordered.extend(remaining.into_iter().flatten());
    ordered
}

/// The order after moving `id` by `delta` positions within `visible`.
///
/// Takes the currently displayed order rather than the stored one so the result
/// is always a complete, self-consistent order — storing a partial one is how a
/// later move against a stale list would reshuffle entries the user never
/// touched. Out-of-range moves are a no-op, so the callers can wire the buttons
/// unconditionally and let the ends of the list clamp themselves.
///
/// Shared by the subscription blocks and the group chips: both are a row of
/// named things the user arranges, and one of them having its own copy of this
/// is how the two would come to disagree about what "move up" means.
pub(super) fn moved_in_order(visible: &[String], id: &str, delta: isize) -> Vec<String> {
    let mut order = visible.to_vec();
    let Some(from) = order.iter().position(|value| value == id) else {
        return order;
    };
    let Some(to) = from
        .checked_add_signed(delta)
        .filter(|to| *to < order.len())
    else {
        return order;
    };
    let group = order.remove(from);
    order.insert(to, group);
    order
}

/// Turn what the filter widgets show into the exact query used for both the
/// list and a newly created profile.
///
/// `PoolQuery` deliberately has no fuzzy text field. A text search is frozen
/// into exact server-id exclusions, so clicking "Create pool" preserves the
/// visible selection instead of silently broadening it.
///
/// `exclude` are the servers the user struck out by hand. They and the frozen
/// search share one field because `resolve` has one notion of exclusion; the two
/// are unioned rather than one replacing the other, so typing in the search box
/// cannot resurrect a server that was explicitly struck out.
pub(super) fn filters_to_query(
    groups: &[Subscription],
    subscriptions: &[String],
    countries: &[String],
    protocols: &[String],
    exclude: &[String],
    search_texts: &HashMap<String, String>,
    text: &str,
) -> PoolQuery {
    let mut query = PoolQuery {
        subscriptions: normalized(subscriptions, false),
        countries: normalized(countries, true),
        protocols: normalized(protocols, true),
        exclude: normalized(exclude, false),
        ..PoolQuery::default()
    };
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() {
        return query;
    }

    let by_text: Vec<String> = oxidom_core::pool::resolve(&query, groups)
        .unwrap_or_default()
        .into_iter()
        .filter(|server| {
            !search_texts
                .get(&server.id)
                .is_some_and(|haystack| haystack.contains(&text))
        })
        .map(|server| server.id.clone())
        .collect();
    query.exclude.extend(by_text);
    query.exclude = normalized(&query.exclude, false);
    query
}

/// Stable ids selected by a pool query, in subscription/server order.
pub(super) fn filtered_ids(query: &PoolQuery, groups: &[Subscription]) -> Vec<String> {
    oxidom_core::pool::resolve(query, groups)
        .unwrap_or_default()
        .into_iter()
        .map(|server| server.id.clone())
        .collect()
}

fn normalized(values: &[String], lowercase: bool) -> Vec<String> {
    let mut normalized = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| {
            if lowercase {
                value.to_ascii_lowercase()
            } else {
                value.to_string()
            }
        })
        .collect::<Vec<_>>();
    normalized.sort_by_key(|value| value.to_ascii_lowercase());
    normalized.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    normalized
}

fn pool_servers(groups: &[Subscription]) -> impl Iterator<Item = &oxidom_core::model::Server> {
    groups
        .iter()
        .flat_map(|group| group.servers.iter())
        .filter(|server| !matches!(&server.spec, OutboundSpec::XrayProfile { .. }))
}

/// The session of `profile`, when the daemon reports one.
pub(super) fn session_for<'a>(state: &'a SnapshotState, profile: &str) -> Option<&'a SessionInfo> {
    state
        .sessions
        .iter()
        .find(|session| session.profile == profile)
}

fn status_from_session(session: Option<&SessionInfo>) -> Status {
    let Some(session) = session else {
        return Status::Disconnected;
    };
    match session.state.as_str() {
        "connected" => Status::Connected,
        "connecting" => Status::Connecting,
        "error" => Status::Error(session.error.clone().unwrap_or_default()),
        _ => Status::Disconnected,
    }
}

fn reported_status_for(state: &SnapshotState, profile: &str) -> Status {
    if profile == "default" {
        state.daemon_status.clone()
    } else {
        status_from_session(session_for(state, profile))
    }
}

/// What the header shows for the currently selected profile.
///
/// `default` deliberately goes through `current_status()` — the very same code
/// that drove the header before sessions existed — instead of being recomputed
/// from the session list: the single-profile experience must not merely look
/// equivalent, it must be the same path.
pub(super) fn selected_status(state: &SnapshotState) -> Status {
    if state.selected_profile == "default" {
        return state.current_status();
    }
    state
        .pending_status_for(&state.selected_profile)
        .cloned()
        .unwrap_or_else(|| reported_status_for(state, &state.selected_profile))
}

/// One row on the Profiles page.
///
/// The shape is the fix for what the page had become: every fact about a
/// session was a coloured pill in a `GtkFlowBox` suffix, and a flow box gives
/// each child the same column width, so `210 ms` was stretched to the width of
/// `pool · 1 node · now ch-hysteria2` and four pills wrapped into a ragged grid.
/// Beside a `Connected` pill and a switch, that is five competing colours to
/// answer one question — is this on?
///
/// So the row now says one thing at a glance and keeps the rest folded away:
/// a headline, at most one *state* badge plus a latency reading, and every
/// remaining fact as a labelled detail row. A pill is reserved for state and
/// for a warning, which is what a pill is good at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionRow {
    /// Profile name; also the row's stable key.
    pub profile: String,
    pub state: SessionRowState,
    /// The one line under the name: what this session is pointed at, and — for
    /// a pool — how much of it is carrying traffic.
    pub headline: String,
    /// A pool is a selection in its own right, never an active member.
    pub pool: bool,
    /// Round-trip through this session, when there is a current measurement.
    pub latency: Option<String>,
    /// Something the user should see without expanding the row. Only a warning
    /// earns this; ordinary facts are [`Self::details`].
    pub warning: Option<SessionWarning>,
    /// Everything else, shown when the row is expanded.
    pub details: Vec<SessionDetail>,
    /// Where the toggle sits before the user touches it.
    pub toggle_on: bool,
    /// An operation for this profile is in flight; the row is insensitive.
    pub busy: bool,
    pub error: Option<String>,
}

/// A labelled fact about a session, shown as its own row when expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionDetail {
    pub label: String,
    pub value: String,
    /// Worth offering a copy button for — an address someone will paste into
    /// another program.
    pub copyable: bool,
    /// What the row means, when the value is a number whose rule is not
    /// self-evident. Never a restatement of the value: a tooltip that says the
    /// same thing louder is a tooltip nobody reads twice.
    pub tooltip: Option<String>,
}

impl SessionDetail {
    fn new(label: &str, value: impl Into<String>) -> Self {
        Self {
            label: label.to_string(),
            value: value.into(),
            copyable: false,
            tooltip: None,
        }
    }

    fn copyable(mut self) -> Self {
        self.copyable = true;
        self
    }

    fn explained(mut self, tooltip: impl Into<String>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }
}

/// What "in rotation" means for the strategy actually running.
///
/// Deliberately *not* shared with `rotation_detail` in the Connect bar, which
/// reads as one sentence about a width being chosen and describes `leastLoad`
/// because that is the default the bar is about to write. Here the strategy is
/// already decided and may be one that keeps dead nodes in the rotation, so the
/// same sentence would be false. Two sentences, two different facts.
fn rotation_help(strategy: &str) -> &'static str {
    match strategy {
        "leastLoad" => {
            "The fastest reachable nodes carry traffic. A node that stops answering \
             leaves the rotation and one of the rest takes over."
        }
        "roundRobin" | "random" => {
            "Every member takes turns, including ones that are not answering — this \
             strategy does not check reachability."
        }
        "leastPing" => {
            "One node carries traffic: whichever answers fastest. It changes when \
             another node becomes faster."
        }
        _ => {
            "How many of the group's servers the core is currently willing to send \
             traffic through."
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionRowState {
    Stopped,
    Connecting,
    Connected,
    Error,
    /// A state string this build does not know. Kept apart from `Stopped`
    /// because it is a different claim: "stopped" with the switch off is a
    /// definite answer, and a newer daemon would have been given it wrongly.
    Unknown,
}

/// Something about a session the user must see without expanding the row.
///
/// There used to be a `SessionChipKind` enum with seven variants, one per fact:
/// interface, inbound address, latency, system proxy, "proxy only". Each was
/// painted in accent or warning colour, so a session with nothing wrong still
/// looked like four alerts. A device name and a loopback address are *facts*,
/// and facts are [`SessionDetail`]s. What is left is a warning, which is the one
/// thing a coloured pill is genuinely good for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionWarning {
    pub text: String,
    pub tooltip: Option<String>,
}

fn session_row_state(session: Option<&SessionInfo>) -> SessionRowState {
    match session.map(|session| session.state.as_str()) {
        Some("connected") => SessionRowState::Connected,
        Some("connecting") => SessionRowState::Connecting,
        Some("error") => SessionRowState::Error,
        // Two honest stops: the daemon holding a session it is not running, and
        // the daemon holding none at all. The wire word is "disconnected" —
        // the only one `SessionInfo::state` is documented to carry for this,
        // and the only one the daemon has ever sent. A session carrying a word
        // this build never heard of is not stopped, it is unread.
        Some("disconnected") | None => SessionRowState::Stopped,
        Some(_) => SessionRowState::Unknown,
    }
}

fn route_mode_label(mode: RouteMode) -> &'static str {
    match mode {
        RouteMode::Manual => "manual",
        RouteMode::List => "list",
        RouteMode::Default => "default",
    }
}

/// The reading badge, and the fuller line that goes with it when expanded.
///
/// The badge stays short — a number is scanned, not read — while the age moves
/// into the detail. Showing "210 ms · 3 min ago" in a pill next to a `Connected`
/// pill and a switch was three widths of text where one was wanted.
fn latency_parts(
    reading: Option<&LatencyReading>,
    now_unix_ms: u64,
) -> (Option<String>, Option<SessionDetail>) {
    let LatencyState::Tunnel { ms, age, .. } = latency_state(reading, false, true, now_unix_ms)
    else {
        return (None, None);
    };
    let detail = match age {
        LatencyAge::Stale(minutes) => format!("{ms} ms · measured {minutes} min ago"),
        _ => format!("{ms} ms"),
    };
    (
        Some(format!("{ms} ms")),
        Some(SessionDetail::new("Latency", detail)),
    )
}

/// How many of a pool's members the balancer is rotating through, when that is
/// knowable at all.
///
/// "In rotation", never "healthy": under `roundRobin` a node in the rotation may
/// well be unreachable, and under `leastLoad` a node out of it may be alive and
/// merely unselected. `None` when the balancer could not be asked or an override
/// pins one target, because a count invented there would read as a measurement.
fn rotation_count(selection: &SelectionInfo) -> Option<usize> {
    let known = selection
        .members
        .iter()
        .filter_map(|member| member.in_rotation)
        .collect::<Vec<_>>();
    (known.len() == selection.members.len() && !known.is_empty())
        .then(|| known.into_iter().filter(|value| *value).count())
}

/// The pool in as few characters as it can honestly be put.
///
/// For the header chip, the sidebar strip and the tray, where the full headline
/// would be truncated to nothing useful. It still never names a member as "the"
/// server — a pool has no active server — so the surfaces that show it cannot
/// accidentally claim one.
pub(super) fn pool_short_label(selection: &SelectionInfo) -> String {
    let count = selection.members.len();
    let inner = match rotation_count(selection) {
        Some(live) if live != count => format!("{live}/{count}"),
        _ => count.to_string(),
    };
    if selection.name.is_empty() {
        format!("group ({inner})")
    } else {
        format!("{} ({inner})", selection.name)
    }
}

/// The line under a pool session's name.
///
/// Says what the pool is and how much of it is working, in that order, because
/// "6 of 42 active" is the answer to the question a pool raises and "pool · 42
/// nodes · now ch-hysteria2" was three facts competing to be first.
fn pool_headline(selection: &SelectionInfo) -> String {
    let count = selection.members.len();
    let nodes = if count == 1 { "node" } else { "nodes" };
    let name = if selection.name.is_empty() {
        "Group".to_string()
    } else {
        format!("Group “{}”", selection.name)
    };
    if let Some(selecting) = selection.selecting.as_deref() {
        // Only a strategy that settles on one node, or an explicit override,
        // has a current exit worth naming.
        return format!("{name} · {count} {nodes} · now {selecting}");
    }
    match rotation_count(selection) {
        Some(live) => format!("{name} · {live} of {count} active"),
        None => format!("{name} · {count} {nodes}"),
    }
}

/// The single line under a session's name.
///
/// A failed session says why. Everything else says what it is pointed at — and
/// nothing else, because the row's remaining space belongs to the state badge
/// and the switch, and a subtitle that also carried the description used to push
/// the profile name into an ellipsis at 680 px.
fn session_headline(
    state: SessionRowState,
    selection: &str,
    session: Option<&SessionInfo>,
) -> String {
    if state == SessionRowState::Error {
        return session
            .and_then(|session| session.error.clone())
            .unwrap_or_else(|| "The connection failed".to_string());
    }
    if selection.is_empty() {
        return "No server selected yet".to_string();
    }
    selection.to_string()
}

/// The operation currently running for `profile`, if any.
fn operation_kind_for(state: &SnapshotState, profile: &str) -> Option<UiOperationKind> {
    let operation = state.operation.as_ref()?;
    (operation.profile.as_deref() == Some(profile)).then_some(operation.kind)
}

/// Build every row on the Profiles page without asking a widget to interpret
/// daemon state.
pub(super) fn session_rows(
    profiles: &[ProfileEntry],
    state: &SnapshotState,
    now_unix_ms: u64,
) -> Vec<SessionRow> {
    profiles
        .iter()
        .map(|entry| {
            let session = session_for(state, &entry.name);
            let in_flight = operation_kind_for(state, &entry.name);
            let row_state = match in_flight {
                // The daemon has been asked but has not answered yet. Reporting
                // the world it described half a second ago would put the row —
                // and its switch — back where the user just moved it away from.
                Some(UiOperationKind::UpProfile) => SessionRowState::Connecting,
                Some(UiOperationKind::DownProfile) => SessionRowState::Stopped,
                _ => session_row_state(session),
            };
            let running_pool = session
                .filter(|session| session.selection.kind == "pool")
                .map(|session| &session.selection);
            let is_pool = running_pool.is_some() || entry.pool.is_some();

            let holding = session.is_some_and(|session| session.holding_traffic);

            // The warnings that have to survive the row being collapsed. Holding
            // comes first and displaces the other: a stale pool is a tunnel
            // carrying traffic it could carry better, while a held one is
            // carrying none at all, and a user reading one pill should be given
            // the more consequential fact.
            let warning = if holding {
                Some(SessionWarning {
                    text: "holding traffic".to_string(),
                    tooltip: Some(
                        "The core exited. This tunnel's routes are still in place, so its \
                         traffic is dropped instead of leaving with your own address. \
                         Reconnect, or stop the session to release it."
                            .to_string(),
                    ),
                })
            } else {
                running_pool
                    .filter(|selection| selection.stale)
                    .map(|_| SessionWarning {
                        text: "stale".to_string(),
                        tooltip: Some("Reconnect to pick up new servers".to_string()),
                    })
            };

            let selection = match running_pool {
                Some(selection) => pool_headline(selection),
                // Stopped, so there is no live membership to count — but the
                // saved query still knows what it is pointed at, and a bare
                // "Group" made every stopped group profile look like the same
                // one.
                None if is_pool => match entry.pool.as_ref() {
                    Some(pool) if !pool.name.is_empty() => format!("Group “{}”", pool.name),
                    Some(pool) => format!("Group · {}", describe_pool(pool)),
                    None => "Group".to_string(),
                },
                None => session
                    .and_then(|session| {
                        session
                            .server_alias
                            .as_ref()
                            .or(session.server_name.as_ref())
                    })
                    .cloned()
                    .unwrap_or_else(|| entry.server.clone()),
            };

            let (latency, latency_detail) =
                latency_parts(state.proxied.get(&entry.name), now_unix_ms);

            let mut details = Vec::new();
            // First, because a failure is what the row was expanded for. The
            // headline carries the same text, but a subtitle ellipsises and
            // cannot be selected — so the one thing worth pasting into a bug
            // report was the one thing unreachable.
            if let Some(error) = session.and_then(|session| session.error.as_ref()) {
                details.push(SessionDetail::new("Error", error.clone()).copyable());
            }
            // Directly below the failure, because it is the half of the failure
            // that decides whether anything is leaving this machine unprotected.
            if holding {
                details.push(SessionDetail::new(
                    "Traffic",
                    "Held — the routes stay until this reconnects or is stopped",
                ));
            }
            if let Some(session) = session {
                details.push(
                    SessionDetail::new(
                        "Proxy",
                        format!("{}:{}", session.address, session.socks_port),
                    )
                    .copyable(),
                );
            }
            if let Some(interface) = session.and_then(|session| session.interface.as_ref()) {
                let mut value = format!(
                    "{} · {} · {}",
                    interface.device, interface.address, interface.routes
                );
                if !interface.up {
                    value.push_str(" · down");
                }
                details.push(SessionDetail::new("Interface", value));
            } else if session.is_none() && entry.interface.enable {
                let device = if entry.interface.device.is_empty() {
                    oxidom_core::bind::device_name(&entry.name).ok()
                } else {
                    Some(entry.interface.device.clone())
                };
                if let Some(device) = device {
                    details.push(SessionDetail::new(
                        "Interface",
                        format!("{device} · {}", route_mode_label(entry.interface.routes)),
                    ));
                }
            } else if !entry.interface.enable {
                // "Proxy only" is the answer to "does this capture my whole
                // machine?", which is worth stating rather than implying by the
                // absence of an interface row.
                details.push(SessionDetail::new("Routing", "Proxy only — no interface"));
            }
            if let Some(selection) = running_pool {
                let count = selection.members.len();
                let mut nodes = match rotation_count(selection) {
                    Some(live) => format!("{live} of {count} in rotation"),
                    None => format!("{count} in the group"),
                };
                // The pool's least honest number, and only worth a line when it
                // is smaller than the node count: a provider that lists one host
                // 26 times gives 42 nodes and 9 places to leave from. Zero means
                // an older daemon did not report it, not "no exits".
                if selection.endpoints > 0 && selection.endpoints < count {
                    nodes.push_str(&format!(
                        " · {} exit address{}",
                        selection.endpoints,
                        if selection.endpoints == 1 { "" } else { "es" }
                    ));
                }
                details.push(
                    SessionDetail::new("Nodes", nodes)
                        .explained(rotation_help(&selection.strategy)),
                );
                details.push(SessionDetail::new("Strategy", selection.strategy.clone()));
            }
            details.extend(latency_detail);
            if session.is_some_and(|session| session.owns_system_proxy) {
                details.push(SessionDetail::new("System proxy", "Set by this connection"));
            }
            if !entry.description.is_empty() {
                details.push(SessionDetail::new("Description", entry.description.clone()));
            }

            SessionRow {
                profile: entry.name.clone(),
                state: row_state,
                headline: session_headline(row_state, &selection, session),
                pool: is_pool,
                latency,
                warning,
                details,
                toggle_on: matches!(
                    row_state,
                    SessionRowState::Connected | SessionRowState::Connecting
                ),
                busy: in_flight.is_some(),
                error: session.and_then(|session| session.error.clone()),
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SwitcherItem {
    pub profile: String,
    pub state: SessionRowState,
    pub selected: bool,
}

/// Whether the header shows a profile switcher at all.
///
/// One profile means the switcher does not exist — not that it exists with a
/// single entry. A user who never made a profile must see the header they saw
/// before this phase.
pub(super) fn switcher_visible(profiles: &[ProfileEntry]) -> bool {
    profiles.len() > 1
}

pub(super) fn switcher_items(
    profiles: &[ProfileEntry],
    state: &SnapshotState,
) -> Vec<SwitcherItem> {
    profiles
        .iter()
        .map(|entry| SwitcherItem {
            profile: entry.name.clone(),
            state: session_row_state(session_for(state, &entry.name)),
            selected: entry.name == state.selected_profile,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CardAction {
    /// The pre-sessions path, unchanged: `ConnectServer` on the daemon.
    Connect(String),
    /// The pre-sessions path, unchanged: `Disconnect` on the daemon.
    Disconnect,
    /// The selected profile already points here; just bring it up/down.
    UpProfile(String),
    DownProfile(String),
    /// The selected profile points somewhere else. The widget layer must ask
    /// before doing this: it rewrites `profiles/<name>.toml`.
    RepointAndUp {
        profile: String,
        server_id: String,
        replaces_pool: bool,
    },
}

fn profile_points_to_server(entry: &ProfileEntry, state: &SnapshotState, server_id: &str) -> bool {
    entry.server == server_id
        || state.sessions.iter().any(|session| {
            session.server_id.as_deref() == Some(server_id)
                && session.server_alias.as_deref() == Some(entry.server.as_str())
        })
}

/// Decide what activating a server card means before the widget layer performs
/// any daemon or filesystem operation.
pub(super) fn card_action(
    profiles: &[ProfileEntry],
    state: &SnapshotState,
    server_id: &str,
) -> CardAction {
    let selected = state.selected_profile.clone();
    let selected_entry = profiles.iter().find(|entry| entry.name == selected);
    let replaces_pool = selected_entry.is_some_and(|entry| entry.pool.is_some())
        || session_for(state, &selected).is_some_and(|session| session.selection.kind == "pool");

    if selected == "default" && !replaces_pool {
        // This is a literal move of the pre-sessions branch from
        // `Controller::activate_server`: default must keep the same path, not a
        // newly equivalent interpretation of the session list.
        if matches!(
            state.current_status(),
            Status::Connected | Status::Connecting
        ) && state.connected_id.as_deref() == Some(server_id)
        {
            return CardAction::Disconnect;
        }
        return CardAction::Connect(server_id.to_string());
    }

    if session_for(state, &selected).and_then(|session| session.server_id.as_deref())
        == Some(server_id)
    {
        return CardAction::DownProfile(selected);
    }
    if profiles
        .iter()
        .find(|entry| entry.name == selected)
        .is_some_and(|entry| profile_points_to_server(entry, state, server_id))
    {
        return CardAction::UpProfile(selected);
    }
    CardAction::RepointAndUp {
        profile: selected,
        server_id: server_id.to_string(),
        replaces_pool,
    }
}

/// One entry of the Connect button's menu.
///
/// The wording leads with what the user gets and follows with the cost, because
/// every one of these has a cost: the two health-blind strategies keep dead
/// nodes in the rotation, and the fast one defeats the whole reason pools exist
/// here — spreading activity over several exit addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ConnectChoice {
    pub label: &'static str,
    pub detail: &'static str,
    pub strategy: Strategy,
}

/// The strategies offered where a group is connected. `random` is deliberately
/// absent: it is `roundRobin` with a worse guarantee, and the profile editor
/// still reaches it for anyone who wants it.
pub(super) fn connect_choices() -> Vec<ConnectChoice> {
    vec![
        ConnectChoice {
            label: "Spread across nodes",
            detail: "Rotates over the nodes the core can still reach.",
            strategy: Strategy::LeastLoad,
        },
        ConnectChoice {
            label: "Every node in turn",
            detail: "Strict rotation, including nodes that stopped answering.",
            strategy: Strategy::RoundRobin,
        },
        ConnectChoice {
            label: "Fastest node",
            detail: "One node carries everything, so activity stops spreading.",
            strategy: Strategy::LeastPing,
        },
    ]
}

/// What pressing Connect on a group's bar does.
///
/// Two answers, and neither writes a file. "Connect me to one of these" is the
/// commonest thing the Servers page is asked, and it used to cost a write into
/// the selected profile plus a dialog about a concept the request never
/// mentioned — and with no profile selected it refused outright and sent the
/// user to another page before anything could connect at all.
///
/// Repointing a saved profile still exists, still rewrites a file, and still
/// asks first: that is what connecting a *profile* means. This is not that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PoolAction {
    /// Run the visible selection now. No profile is read, written or confirmed.
    ConnectSelection,
    /// A session is already running exactly this selection, so the button that
    /// started it stops it. Carries that session's profile name.
    Stop(String),
}

/// Decide what connecting a group does. Nothing here writes anything.
///
/// `members` is the selection as the page resolved it — the server ids the query
/// matches right now. Comparing those rather than the query is what makes the
/// button honest: a saved group and the same servers arrived at by hand are the
/// same session, and a session started from one is stopped by the other.
pub(super) fn pool_action(state: &SnapshotState, members: &[String]) -> PoolAction {
    let running = state.sessions.iter().find(|session| {
        session.selection.kind == "pool" && same_members(&session.selection, members)
    });
    match running {
        Some(session) => PoolAction::Stop(session.profile.clone()),
        None => PoolAction::ConnectSelection,
    }
}

/// Whether a running pool holds exactly these servers.
///
/// Order-insensitive: a pool's member order is the ranking it was started with,
/// which moves with the latency readings, and a comparison that counted it would
/// report "a different selection" every time a check ran.
fn same_members(selection: &SelectionInfo, members: &[String]) -> bool {
    if selection.members.len() != members.len() {
        return false;
    }
    let mut running = selection
        .members
        .iter()
        .map(|member| member.server_id.as_str())
        .collect::<Vec<_>>();
    let mut wanted = members.iter().map(String::as_str).collect::<Vec<_>>();
    running.sort_unstable();
    wanted.sort_unstable();
    running == wanted
}

/// The banner over every page but Profiles.
///
/// Counts the running sessions that are not the selected profile's — the ones
/// the rest of the window says nothing about. It is phrased in profiles because
/// that is the only name the user gave any of them; `session` is the daemon's
/// word for one that happens to be up, and it stays in the CLI and the logs.
/// What the banner says when the daemon could not resolve an Xray core.
///
/// Without one nothing connects and nothing is measured, yet the only symptom
/// on the Servers page is that every card refuses to produce a number — which
/// reads as a dead subscription rather than as a missing program. A daemon too
/// old to answer `RuntimeInfo` reports `None` here, and stays silent rather
/// than accusing a core that may well be present.
pub(super) fn missing_core_message(runtime: Option<&RuntimeInfo>) -> Option<String> {
    let runtime = runtime?;
    // Strict precedence, one banner. A core that could not be resolved was
    // never asked about its geo data, so reporting both would be two lines
    // about one cause — and the second would be a guess.
    if runtime.xray_error.is_some() {
        return Some(
            "No Xray core found — nothing can connect or be measured until one is installed".into(),
        );
    }
    // `Some(false)` is the daemon having asked the core and been refused.
    // `None` is nobody having asked, which an older daemon also reports, and
    // silence is the only honest answer to that.
    if runtime.geo.usable == Some(false) {
        return Some(
            "The Xray core cannot load its geo data — nothing will connect until geoip.dat \
             and geosite.dat are installed"
                .into(),
        );
    }
    None
}

/// The summary the About window carries under the application's name.
///
/// `AdwAboutDialog` holds one version as a property, and this window runs
/// three programs: itself, the daemon and the core. The other two go here,
/// where they are read on the page that opens rather than behind a
/// Troubleshooting link — the whole complaint the About window answers is that
/// finding out what is running takes going somewhere else.
///
/// The skew sentence goes last and only when there is one. A window that warns
/// about something every time it opens is a window whose warnings stop being
/// read.
pub(super) fn about_comments(versions: &Versions) -> String {
    let daemon = match versions.daemon.as_deref() {
        Some(version) => format!("Daemon {version} — {}", versions.daemon_kind()),
        None => format!("Daemon version unknown — {}", versions.daemon_kind()),
    };
    let core = match versions.core.as_deref() {
        Some(core) => core.to_string(),
        None => "No Xray core".to_string(),
    };
    let mut text = format!("{APP_SUMMARY}\n\n{daemon}\n{core}");
    if let Some(skew) = versions.skew() {
        text.push_str("\n\n");
        text.push_str(&skew);
    }
    text
}

/// The same sentence the AppStream metadata carries as `<summary>`. Kept
/// identical on purpose: a user meets it in a software centre before install
/// and in this window afterwards, and two wordings would read as two programs.
pub(super) const APP_SUMMARY: &str = "Xray client for the GNOME desktop";

/// What Settings can offer for the geo data, which is not the same question as
/// whether the data is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GeoOffer {
    /// Say nothing: no core to ask, or a daemon too old to have been asked.
    Silent,
    /// The core loads both lists, from wherever it found them.
    Working,
    /// The daemon can install them itself.
    Download,
    /// A download is under way. `total` is zero when the server sent no
    /// length, and the bar must then show motion without a denominator.
    Running { file: String, done: u64, total: u64 },
    /// The daemon cannot write its own asset directory, so a button would only
    /// fail on click.
    Unwritable { dir: String },
    /// The daemon predates the download, so the only thing that helps is a
    /// command the user runs. Offering a button here would be offering a fix
    /// that cannot reach the daemon doing the work.
    CommandOnly { session_fallback: bool },
}

/// Decide what the geo rows offer.
///
/// `daemon_can_download` is whether the daemon knows the method at all, and
/// `source` is which daemon answered. The pair matters: files a *session*
/// fallback download writes into the user's home are invisible to a system
/// service, which runs as `oxidom` with `ProtectHome=true` — so offering that
/// button there would be offering something that provably cannot work.
pub(super) fn geo_offer(
    runtime: Option<&RuntimeInfo>,
    daemon_can_download: bool,
    source: DaemonSource,
) -> GeoOffer {
    let Some(runtime) = runtime else {
        return GeoOffer::Silent;
    };
    if runtime.xray_error.is_some() {
        return GeoOffer::Silent;
    }
    if runtime.geo.downloading {
        return GeoOffer::Running {
            file: runtime
                .geo
                .current_file
                .clone()
                .unwrap_or_else(|| "geo data".to_string()),
            done: runtime.geo.done_bytes,
            total: runtime.geo.total_bytes,
        };
    }
    match runtime.geo.usable {
        None => GeoOffer::Silent,
        Some(true) => GeoOffer::Working,
        Some(false) if !daemon_can_download => GeoOffer::CommandOnly {
            session_fallback: source != DaemonSource::System,
        },
        Some(false) if !runtime.geo.writable => GeoOffer::Unwritable {
            dir: runtime.geo.dir.clone(),
        },
        Some(false) => GeoOffer::Download,
    }
}

/// A byte count as a person reads it, for the line under the progress bar.
pub(super) fn human_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{:.0} kB", bytes as f64 / 1024.0)
    }
}

/// What the progress line says while a download runs.
///
/// Without a total there is still something true to say — the bytes so far —
/// and saying it beats a bar that only pulses.
pub(super) fn geo_progress_text(file: &str, done: u64, total: u64) -> String {
    if total > 0 {
        format!("{file} — {} of {}", human_bytes(done), human_bytes(total))
    } else {
        format!("{file} — {}", human_bytes(done))
    }
}

pub(super) fn other_profiles_message(
    sessions: &[SessionInfo],
    selected_profile: &str,
) -> Option<String> {
    let count = sessions
        .iter()
        .filter(|session| session.profile != selected_profile)
        .count();
    match count {
        0 => None,
        1 => Some("1 more profile is running".to_string()),
        count => Some(format!("{count} more profiles are running")),
    }
}

/// Whether pressing a latency control means stop rather than start.
///
/// True when *any* of the ids is mid-check, because that is exactly when the
/// button is showing a stop icon. The tempting rule — "stop if there is nothing
/// new to start" — reads the press backwards halfway through a sweep: cards
/// retire one at a time, so once a few have finished, a press on a button
/// showing a stop would start fresh checks on the finished ones.
pub(super) fn press_stops(ids: &[String], checking: &HashMap<String, ProbeWait>) -> bool {
    ids.iter().any(|id| checking.contains_key(id))
}

/// Which toast a reading earns, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeToast {
    Unreachable,
    NoNetwork,
    DidNotRun,
}

/// The single rule for whether a failed reading is worth a toast.
///
/// Kept in one place because it was written twice — once for direct readings
/// and once, verbatim, for proxied ones — and two copies of a rule is how one
/// of them stops being the rule.
///
/// `asked` means the user pressed check on this card, `swept` that it came from
/// a whole-subscription sweep. A sweep stays quiet about a single silent server,
/// whose own card already says so, but does report a machine that could not
/// measure anything: that fails every server at once, and cards alone would
/// read as a subscription of dead servers.
///
/// A cancelled check earns nothing. The user stopped it, so reporting it back as
/// a failure would be telling them their own decision went wrong — and on a
/// cancelled sweep of a large subscription it would be an error toast for an
/// action that worked.
pub(super) fn probe_toast(
    reading: &LatencyReading,
    asked: bool,
    swept: bool,
) -> Option<ProbeToast> {
    if reading.value.is_some() {
        return None;
    }
    if reading.detail == Some(ProbeDetail::Cancelled) {
        return None;
    }
    match reading.failure {
        Some(ProbeFailure::Unknown) if asked || swept => Some(ProbeToast::DidNotRun),
        Some(ProbeFailure::NoNetwork) if asked || swept => Some(ProbeToast::NoNetwork),
        _ if asked => Some(ProbeToast::Unreachable),
        _ => None,
    }
}

/// The single mapper from "what the daemon told us" to "what the card shows".
///
/// `is_active` is whether the tunnel is currently carried by this server, i.e.
/// whether a proxied reading for it is still about anything. A reading whose
/// route disagrees with that is [`LatencyState::Superseded`]: the context it
/// was taken in is gone, and the number is about a different thing than the
/// card claims to show.
pub(super) fn latency_state(
    reading: Option<&LatencyReading>,
    is_checking: bool,
    is_active: bool,
    now_unix_ms: u64,
) -> LatencyState {
    if is_checking {
        return LatencyState::Checking;
    }
    let Some(reading) = reading else {
        return LatencyState::Unmeasured;
    };
    if (reading.route == ProbeRoute::Proxied) != is_active {
        return LatencyState::Superseded;
    }
    let age = age_of(reading, now_unix_ms);
    match reading.value {
        Some(ms) if reading.route == ProbeRoute::Proxied => LatencyState::Tunnel {
            ms,
            age,
            method: reading.method,
        },
        Some(ms) => LatencyState::Reachable {
            ms,
            age,
            method: reading.method,
        },
        None => match reading.failure {
            Some(ProbeFailure::NoNetwork) => LatencyState::NoNetwork,
            // `Unknown` is what the daemon sends when the probe never got as
            // far as the server — no Xray core to build the measuring tunnel
            // with, no free port, no place to stage its config. Folding it in
            // with a server that stayed silent is how a machine with no core
            // reports nine healthy servers as dead.
            Some(ProbeFailure::Unknown) => LatencyState::NotRun(reading.detail),
            // Named rather than caught by a rest pattern. A timeout and a refusal
            // both mean the server did not answer, so they share a state — but
            // saying so is what makes a fifth variant a compile error here
            // instead of silently becoming "unreachable".
            Some(ProbeFailure::Unreachable | ProbeFailure::Timeout) => LatencyState::Unreachable,
            // `failure.is_some()` and `value.is_none()` are the same case by
            // contract, so this is a daemon that broke it.
            None => LatencyState::Unmeasured,
        },
    }
}

/// What the expanded card says about a check that produced no number.
///
/// Separate from [`LatencyState`], which is the badge's business: a badge has a
/// glyph, a tooltip and a pill to fit in, and the diagnosis fits in none of
/// them. "The server did not answer" covers a refused handshake, a wrong TLS
/// parameter and a dead network, and telling those apart is the whole
/// diagnosis — which until now meant scrolling a log shared with every other
/// source on the machine.
///
/// Nothing here is new information from the daemon. `LatencyReading` has
/// carried the method, the route, the time and the detail all along; the card
/// threw four of them away on the way to a pill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FailureReport {
    /// What went wrong, in the words the CLI and the badge already use.
    pub reason: String,
    /// How the check was made and when. The pair is what decides whether the
    /// reason describes the server or describes this machine's last five
    /// minutes — a refusal measured through a tunnel that has since gone down
    /// says nothing about the server at all.
    pub attempt: String,
}

/// The report for the last reading, or `None` when there is nothing to explain.
pub(super) fn failure_report(
    reading: Option<&LatencyReading>,
    now_unix_ms: u64,
) -> Option<FailureReport> {
    let reading = reading?;
    // A number needs no excuse, and a card showing one must not also carry a
    // reason left over from the check before it.
    if reading.value.is_some() {
        return None;
    }
    // `failure.is_some()` exactly when `value.is_none()` by the type's own
    // contract, so a reading with neither comes from a daemon that broke it.
    // Nothing to report beats something invented.
    let failure = reading.failure?;
    let reason = sentence(failure.message_with(reading.detail));
    let how = match reading.route {
        ProbeRoute::Direct => format!("Tried by {}", method_text(reading.method)),
        ProbeRoute::Proxied => {
            format!(
                "Tried through the tunnel by {}",
                method_text(reading.method)
            )
        }
    };
    Some(FailureReport {
        reason,
        attempt: format!("{how} · {}", when_text(reading, now_unix_ms)),
    })
}

/// When a reading was taken, in the one wording the interface uses for it.
///
/// Shared by the reason under a failed check and by every row of the history,
/// because the two sit one above the other on the same card: two spellings of
/// "three minutes ago" there would read as two different facts.
fn when_text(reading: &LatencyReading, now_unix_ms: u64) -> String {
    match age_of(reading, now_unix_ms) {
        LatencyAge::Fresh => "just now".to_string(),
        LatencyAge::Stale(minutes) => minutes_ago(minutes),
        // A daemon that predates the timestamp, or two clocks that disagree.
        // "Just now" would be the flattering answer rather than the true one,
        // and how old the reading is decides whether it still describes
        // anything.
        LatencyAge::Unknown => "at an unrecorded time".to_string(),
    }
}

/// One past check, as the expanded card states it.
pub(super) struct HistoryRow {
    /// `"41 ms"`, or an em dash when the check produced no number. Never blank:
    /// a row with nothing in this column reads as a rendering fault rather than
    /// as a check that failed.
    pub value: String,
    /// How it was taken, when, and — where there is no number — why:
    /// `"HTTP · 3 minutes ago"`, or `"HTTP · 3 minutes ago · the server did not
    /// answer"`. The reason stays a fragment here, unlike the one standing on
    /// its own above, because it is the tail of a line.
    pub taken: String,
}

/// The recent checks, newest first, as rows.
///
/// A single sample is the weakest possible basis for choosing between servers:
/// one that is fast half the time and one that is steady look identical through
/// their newest number alone. These rows are what tells them apart.
///
/// Every row is a check that *ran*. The daemon records no history for a check
/// it called off, so nothing here needs to distinguish "measured badly" from
/// "never measured" — the failures shown are the server's or this machine's.
pub(super) fn history_rows(history: &ProbeHistory, now_unix_ms: u64) -> Vec<HistoryRow> {
    history
        .readings
        .iter()
        .map(|reading| {
            let mut taken = format!(
                "{} · {}",
                method_text(reading.method),
                when_text(reading, now_unix_ms)
            );
            // `failure.is_some()` exactly when `value.is_none()`, so a reading
            // with neither comes from a daemon that broke the contract. The row
            // still owes the column something, and a dash with no reason is the
            // honest form of "it did not say".
            if let (None, Some(failure)) = (reading.value, reading.failure) {
                taken.push_str(" · ");
                taken.push_str(failure.message_with(reading.detail));
            }
            HistoryRow {
                value: match reading.value {
                    Some(ms) => format!("{ms} ms"),
                    None => "—".to_string(),
                },
                taken,
            }
        })
        .collect()
}

/// A message written to sit mid-sentence, promoted to standing on its own.
///
/// The wording lives on `ProbeFailure` and `ProbeDetail` so that the CLI, the
/// badge and this card cannot describe one condition three ways. It is phrased
/// as a fragment there because most of its readers append it to something;
/// this is the one reader that does not.
fn sentence(message: &str) -> String {
    let mut chars = message.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// How long ago, in the one wording the interface uses for it.
pub(super) fn minutes_ago(minutes: u16) -> String {
    let unit = if minutes == 1 { "minute" } else { "minutes" };
    format!("{minutes} {unit} ago")
}

fn age_of(reading: &LatencyReading, now_unix_ms: u64) -> LatencyAge {
    // A payload that predates the field, or a clock that moved backwards
    // between the two processes. Either way the age is not knowable, and
    // guessing "fresh" would be the flattering answer rather than the true one.
    if reading.measured_at_unix_ms == 0 {
        return LatencyAge::Unknown;
    }
    let Some(elapsed) = now_unix_ms.checked_sub(reading.measured_at_unix_ms) else {
        return LatencyAge::Unknown;
    };
    match elapsed / 60_000 {
        0 => LatencyAge::Fresh,
        minutes => LatencyAge::Stale(minutes.min(u64::from(u16::MAX)) as u16),
    }
}

/// Every card the GUI has anything to say about. Ids absent from this are
/// [`LatencyState::Unmeasured`] by construction, so the view can default them.
pub(super) fn latency_states(
    state: &SnapshotState,
    now_unix_ms: u64,
) -> HashMap<String, LatencyState> {
    let mut ids = state
        .readings
        .keys()
        .chain(state.checking.keys())
        .cloned()
        .collect::<HashSet<_>>();
    if let Some(target) = &state.route_target
        && (proxied_reading(state, target).is_some()
            || state
                .readings
                .get(&target.server_id)
                .is_some_and(|reading| reading.route == ProbeRoute::Proxied))
    {
        ids.insert(target.server_id.clone());
    }
    ids.into_iter()
        .map(|id| (id.clone(), state.card_state(&id, now_unix_ms)))
        .collect()
}

/// Latency to show for the connected server, and whether it's a carried-over
/// fallback (no probe has confirmed a fresh reading for this connection yet)
/// rather than a live reading.
pub(super) fn active_latency_for(state: &SnapshotState) -> (Option<u32>, bool) {
    if session_for(state, &state.selected_profile)
        .is_some_and(|session| session.selection.kind == "pool")
    {
        return state
            .proxied
            .get(&state.selected_profile)
            .filter(|reading| reading.route == ProbeRoute::Proxied)
            .and_then(|reading| reading.value)
            .map_or((None, false), |ms| (Some(ms), false));
    }

    let (display_target, allow_legacy_reading) = if state.selected_profile == "default" {
        let Some(id) = state.connected_id.as_deref() else {
            return (None, false);
        };
        (
            RouteTarget {
                profile: state.active_profile.clone(),
                server_id: id.to_string(),
            },
            true,
        )
    } else {
        let Some(session) = session_for(state, &state.selected_profile) else {
            return (None, false);
        };
        let Some(server_id) = session.server_id.clone() else {
            return (None, false);
        };
        (
            RouteTarget {
                profile: Some(state.selected_profile.clone()),
                server_id,
            },
            false,
        )
    };
    // Only a proxied reading describes the connection. A direct one for the
    // same server measures the server, not the tunnel through it, and putting
    // that in the header is the original lie this phase set out to remove.
    if let Some(ms) = proxied_reading(state, &display_target)
        .or_else(|| {
            allow_legacy_reading
                .then(|| state.readings.get(&display_target.server_id))
                .flatten()
                .filter(|reading| reading.route == ProbeRoute::Proxied)
        })
        .filter(|reading| reading.route == ProbeRoute::Proxied)
        .and_then(|reading| reading.value)
    {
        return (Some(ms), false);
    }
    match &state.last_active_latency {
        Some((last_target, ms)) if last_target == &display_target => (Some(*ms), true),
        _ => (None, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidom_core::config::LatencyMethod;
    use oxidom_core::ipc::{GeoAssets, InterfaceInfo, PoolMember, ProbeFailure, ProbeRoute};
    use oxidom_core::link::parse_link;
    use oxidom_core::model::Subscription;
    use oxidom_core::profile::ProfileInterface;

    fn state() -> SnapshotState {
        SnapshotState::new(&StatusInfo::default())
    }

    fn profile(name: &str, server: &str) -> ProfileEntry {
        ProfileEntry {
            name: name.to_string(),
            description: String::new(),
            server: server.to_string(),
            socks_port: 10808,
            http_port: 10809,
            interface: ProfileInterface::default(),
            pool: None,
            core: Default::default(),
            on_core_exit: None,
            routing: None,
        }
    }

    fn session(profile: &str, state: &str, server_id: &str) -> SessionInfo {
        SessionInfo {
            profile: profile.to_string(),
            state: state.to_string(),
            server_id: Some(server_id.to_string()),
            address: format!("127.{}.0.1", if profile == "work" { 91 } else { 92 }),
            socks_port: 10808,
            http_port: 10809,
            ..SessionInfo::default()
        }
    }

    fn filter_group(id: &str, name: &str, links: &[(&str, &str, Option<&str>)]) -> Subscription {
        let mut group =
            Subscription::new(format!("https://{id}.example/sub"), Some(name.to_string()));
        group.id = id.to_string();
        group.servers = links
            .iter()
            .map(|(server_id, link, country)| {
                let mut server = parse_link(link).expect("valid filter fixture");
                server.id = (*server_id).to_string();
                server.country = country.map(str::to_string);
                server
            })
            .collect();
        group
    }

    fn snapshot(status: StatusInfo, probe: ProbeState) -> PolledSnapshot {
        PolledSnapshot {
            status,
            probe,
            logs: LogSlice::from_legacy_lines(Vec::new()),
            epoch: 0,
        }
    }

    /// `reduce` for the cases where the snapshot is current by construction.
    fn fold(
        state: &mut SnapshotState,
        snapshot: &PolledSnapshot,
        now: Instant,
        window_visible: bool,
    ) -> Vec<Effect> {
        reduce(state, snapshot, now, NOW_MS, window_visible).expect("snapshot is current")
    }

    /// A fixed wall clock, so a reading's age is decided by the test rather
    /// than by how long the suite took to get here.
    const NOW_MS: u64 = 1_700_000_000_000;

    /// The badge a just-taken direct reading produces. The method is whatever
    /// [`reading`] stamped on it: the state repeats how the number was taken,
    /// it does not decide it.
    fn fresh(ms: u32) -> LatencyState {
        LatencyState::Reachable {
            ms,
            age: LatencyAge::Fresh,
            method: LatencyMethod::HttpGet,
        }
    }

    fn probe(running: &[&str], readings: &[(&str, Option<u32>)]) -> ProbeState {
        ProbeState {
            version: PROBE_STATE_VERSION,
            running: running.iter().map(|id| id.to_string()).collect(),
            queued: Vec::new(),
            readings: readings
                .iter()
                .map(|(id, value)| (id.to_string(), reading(*value)))
                .collect(),
            proxied: HashMap::new(),
        }
    }

    fn reading(value: Option<u32>) -> LatencyReading {
        let mut reading = match value {
            Some(ms) => LatencyReading::ok(ms, ProbeRoute::Direct, LatencyMethod::HttpGet),
            None => LatencyReading::failed(
                ProbeFailure::Unreachable,
                ProbeRoute::Direct,
                LatencyMethod::HttpGet,
            ),
        };
        reading.measured_at_unix_ms = NOW_MS;
        reading
    }

    fn proxied(ms: u32, measured_at_unix_ms: u64) -> LatencyReading {
        let mut reading = LatencyReading::ok(ms, ProbeRoute::Proxied, LatencyMethod::HttpGet);
        reading.measured_at_unix_ms = measured_at_unix_ms;
        reading
    }

    /// `ProbeState::default()` is version 0, i.e. an outdated daemon — not what
    /// the tests below mean when they say "nothing is going on".
    fn idle() -> ProbeState {
        probe(&[], &[])
    }

    fn connected_to(id: &str) -> StatusInfo {
        StatusInfo::from_status(&Status::Connected, Some(id.to_string()))
    }

    /// One reading taken *through* the tunnel, i.e. the only kind that says
    /// anything about the connection in use.
    fn tunnel_probe(id: &str, ms: u32) -> ProbeState {
        let mut reading = reading(Some(ms));
        reading.route = ProbeRoute::Proxied;
        let mut state = idle();
        state.readings.insert(id.to_string(), reading);
        state
    }

    fn latency(effects: &[Effect], id: &str) -> Option<LatencyState> {
        effects.iter().find_map(|effect| match effect {
            Effect::Latency(updated, state) if updated == id => Some(*state),
            _ => None,
        })
    }

    /// The compatibility selection must call the old status and latency paths,
    /// and must preserve both branches of the old card activation decision.
    #[test]
    fn default_selection_preserves_the_pre_sessions_paths() {
        let status = connected_to("same").with_active_profile(Some("default".to_string()));
        let mut state = SnapshotState::new(&status);
        state
            .proxied
            .insert("default".to_string(), proxied(41, NOW_MS));
        let profiles = vec![profile("default", "same")];

        assert_eq!(selected_status(&state), state.current_status());
        assert_eq!(active_latency_for(&state), (Some(41), false));
        assert_eq!(
            card_action(&profiles, &state, "same"),
            CardAction::Disconnect
        );
        assert_eq!(
            card_action(&profiles, &state, "other"),
            CardAction::Connect("other".to_string())
        );

        state.daemon_status = Status::Disconnected;
        assert_eq!(
            card_action(&profiles, &state, "same"),
            CardAction::Connect("same".to_string())
        );
    }

    #[test]
    fn filter_query_and_visible_ids_are_the_same_pool_selection() {
        let groups = vec![
            filter_group(
                "main",
                "Main",
                &[
                    (
                        "alpine",
                        "vless://b831381d-6324-4d53-ad4f-8cda48b30811@a.example:443#Alpine",
                        Some("CH"),
                    ),
                    (
                        "zurich",
                        "vless://b831381d-6324-4d53-ad4f-8cda48b30811@z.example:443#Zurich",
                        Some("ch"),
                    ),
                    (
                        "berlin",
                        "trojan://secret@de.example:443#Berlin",
                        Some("de"),
                    ),
                ],
            ),
            filter_group(
                "backup",
                "Backup",
                &[("other", "trojan://secret@nl.example:443#Other", Some("nl"))],
            ),
        ];
        let search_texts = HashMap::from([
            (
                "alpine".to_string(),
                "alpine vless a.example:443 ch".to_string(),
            ),
            (
                "zurich".to_string(),
                "zurich vless z.example:443 ch".to_string(),
            ),
            (
                "berlin".to_string(),
                "berlin trojan de.example:443 de".to_string(),
            ),
            (
                "other".to_string(),
                "other trojan nl.example:443 nl".to_string(),
            ),
        ]);
        let query = filters_to_query(
            &groups,
            &["main".to_string()],
            &["CH".to_string()],
            &["VLESS".to_string()],
            &[],
            &search_texts,
            "alp",
        );

        assert_eq!(query.subscriptions, ["main"]);
        assert_eq!(query.countries, ["ch"]);
        assert_eq!(query.protocols, ["vless"]);
        assert_eq!(query.exclude, ["zurich"]);
        assert_eq!(filtered_ids(&query, &groups), ["alpine"]);

        // A server struck out by hand stays struck out while the search box is
        // narrowing to it: the two exclusions are one field, and the later one
        // must not overwrite the earlier.
        let by_hand = filters_to_query(
            &groups,
            &["main".to_string()],
            &["CH".to_string()],
            &["VLESS".to_string()],
            &["alpine".to_string()],
            &search_texts,
            "alp",
        );
        assert_eq!(by_hand.exclude, ["alpine", "zurich"]);
        assert!(filtered_ids(&by_hand, &groups).is_empty());

        // And it applies on its own, with no search text at all.
        let struck = filters_to_query(
            &groups,
            &[],
            &[],
            &[],
            &["zurich".to_string()],
            &[].into(),
            "",
        );
        assert_eq!(
            filtered_ids(&struck, &groups),
            ["alpine", "berlin", "other"]
        );
        assert_eq!(
            excludable_servers(&groups)
                .iter()
                .map(|(name, servers)| (name.as_str(), servers.len()))
                .collect::<Vec<_>>(),
            [("Main", 3), ("Backup", 1)]
        );
        assert_eq!(available_countries(&groups), ["ch", "de", "nl"]);
        assert_eq!(
            available_protocols(&groups),
            ["trojan".to_string(), "vless".to_string()]
        );
        assert_eq!(
            available_subscriptions(&groups),
            vec![
                FilterOption {
                    value: "main".to_string(),
                    label: "Main".to_string(),
                },
                FilterOption {
                    value: "backup".to_string(),
                    label: "Backup".to_string(),
                },
            ]
        );
    }

    fn group(id: &str, kind: GroupKind, query: PoolQuery) -> ServerGroup {
        ServerGroup {
            id: id.to_string(),
            name: id.to_string(),
            icon: String::new(),
            kind,
            query,
        }
    }

    #[test]
    fn a_group_matches_the_filter_regardless_of_what_the_pool_is_called() {
        let saved = group(
            "eu",
            GroupKind::Rule,
            PoolQuery {
                name: "Europe".to_string(),
                countries: vec!["de".to_string()],
                ..PoolQuery::default()
            },
        );
        let same = PoolQuery {
            name: "something else entirely".to_string(),
            countries: vec!["de".to_string()],
            ..PoolQuery::default()
        };
        let different = PoolQuery {
            name: "Europe".to_string(),
            countries: vec!["ch".to_string()],
            ..PoolQuery::default()
        };

        assert!(query_equals_group(&same, &saved));
        assert!(!query_equals_group(&different, &saved));
    }

    #[test]
    fn saving_over_a_group_keeps_its_place_in_the_row() {
        let groups = vec![
            group("favourites", GroupKind::List, PoolQuery::default()),
            group("eu", GroupKind::Rule, PoolQuery::default()),
            group("asia", GroupKind::Rule, PoolQuery::default()),
        ];
        let mut replacement = group("eu", GroupKind::Rule, PoolQuery::default());
        replacement.name = "Europe (wider)".to_string();

        let saved = upsert_group(&groups, replacement);
        assert_eq!(
            saved.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(),
            ["favourites", "eu", "asia"]
        );
        assert_eq!(saved[1].name, "Europe (wider)");

        let added = upsert_group(&groups, group("new", GroupKind::Rule, PoolQuery::default()));
        assert_eq!(
            added.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(),
            ["favourites", "eu", "asia", "new"]
        );
    }

    #[test]
    fn starring_a_server_toggles_it_in_and_out_of_the_list() {
        let empty = group("favourites", GroupKind::List, PoolQuery::default());

        let starred = toggled_member(&empty, "id-a");
        assert_eq!(starred.query.members, ["id-a"]);
        let both = toggled_member(&starred, "id-b");
        assert_eq!(both.query.members, ["id-a", "id-b"]);
        // Toggling the first one back leaves the second where it was.
        assert_eq!(toggled_member(&both, "id-a").query.members, ["id-b"]);
    }

    #[test]
    fn a_deletion_can_name_every_group_that_would_lose_the_server() {
        let groups = vec![filter_group(
            "main",
            "Main",
            &[
                (
                    "berlin",
                    "vless://b831381d-6324-4d53-ad4f-8cda48b30811@a.example:443#Berlin",
                    Some("de"),
                ),
                ("zurich", "trojan://secret@z.example:443#Zurich", Some("ch")),
            ],
        )];
        let mut favourites = group(
            "favourites",
            GroupKind::List,
            PoolQuery {
                members: vec!["berlin".to_string()],
                ..PoolQuery::default()
            },
        );
        favourites.name = "Favourites".to_string();
        let mut germany = group(
            "de",
            GroupKind::Rule,
            PoolQuery {
                countries: vec!["de".to_string()],
                ..PoolQuery::default()
            },
        );
        germany.name = "Germany".to_string();
        let saved = vec![favourites, germany];

        // A rule counts too: deleting Berlin empties Germany just as surely as
        // it empties a list that names it.
        assert_eq!(
            groups_holding(&saved, &groups, "berlin"),
            ["Favourites", "Germany"]
        );
        assert!(groups_holding(&saved, &groups, "zurich").is_empty());

        // An empty list stands for nothing. Inferred from `PoolQuery` alone it
        // would be an unfiltered rule, i.e. every server on the machine, and a
        // Favourites nobody has starred yet would claim all of them.
        let untouched = vec![group("favourites", GroupKind::List, PoolQuery::default())];
        assert!(group_member_ids(&untouched[0], &groups).is_empty());
        assert!(groups_holding(&untouched, &groups, "berlin").is_empty());
        // An empty *rule* is still "everything", which is what "All" means.
        let all = group("all", GroupKind::Rule, PoolQuery::default());
        assert_eq!(group_member_ids(&all, &groups).len(), 2);
    }

    #[test]
    fn saved_order_leads_and_unknown_subscriptions_keep_their_natural_place() {
        let groups = vec![
            filter_group("main", "Main", &[]),
            filter_group("backup", "Backup", &[]),
            filter_group("fresh", "Fresh", &[]),
        ];
        let ids = |groups: &[Subscription]| {
            groups
                .iter()
                .map(|group| group.id.clone())
                .collect::<Vec<_>>()
        };

        // A saved order that predates "fresh" still ranks the two it knows, and
        // leaves the newcomer at the end instead of dropping it.
        assert_eq!(
            ids(&ordered_subscriptions(
                &groups,
                &["backup".to_string(), "main".to_string()]
            )),
            ["backup", "main", "fresh"]
        );
        // An id for a subscription that has since been removed is ignored.
        assert_eq!(
            ids(&ordered_subscriptions(
                &groups,
                &["gone".to_string(), "fresh".to_string()]
            )),
            ["fresh", "main", "backup"]
        );
        assert_eq!(ids(&ordered_subscriptions(&groups, &[])), ids(&groups));
    }

    /// The complaint this answers: connecting a group cost a profile write and a
    /// dialog about profiles, and with none selected it refused and pointed at
    /// another page. None of that is a thing the request mentioned.
    #[test]
    fn connecting_a_group_runs_the_selection_and_writes_no_profile() {
        let members = vec!["berlin".to_string(), "munich".to_string()];
        let mut state = state();

        // No profile is selected, none exists, and it connects anyway.
        state.selected_profile = String::new();
        assert_eq!(pool_action(&state, &members), PoolAction::ConnectSelection);

        // A profile pointing somewhere else is not consulted and not rewritten.
        state.selected_profile = "work".to_string();
        state.sessions = vec![session("work", "connected", "berlin")];
        assert_eq!(
            pool_action(&state, &members),
            PoolAction::ConnectSelection,
            "a single-server session is not this selection running"
        );
    }

    /// Started from the bar, stopped from the bar — whichever session is
    /// carrying it, and whatever profile happens to be selected now.
    #[test]
    fn a_running_selection_is_stopped_by_the_button_that_started_it() {
        let members = vec!["berlin".to_string(), "munich".to_string()];
        let mut state = state();
        state.selected_profile = "home".to_string();

        let mut running = session("default", "connected", "berlin");
        running.selection = SelectionInfo {
            kind: "pool".to_string(),
            members: vec![
                PoolMember {
                    server_id: "munich".to_string(),
                    ..PoolMember::default()
                },
                PoolMember {
                    server_id: "berlin".to_string(),
                    ..PoolMember::default()
                },
            ],
            ..SelectionInfo::default()
        };
        state.sessions = vec![running];

        // Order is the ranking the pool started with, and it moves with every
        // latency check. Counting it would make the button flip to Connect for
        // a session that is plainly running.
        assert_eq!(
            pool_action(&state, &members),
            PoolAction::Stop("default".to_string())
        );

        // One server more is a different selection, so it connects rather than
        // stopping something the user did not ask about.
        let wider = vec![
            "berlin".to_string(),
            "munich".to_string(),
            "hamburg".to_string(),
        ];
        assert_eq!(pool_action(&state, &wider), PoolAction::ConnectSelection);
    }

    #[test]
    fn moving_an_entry_clamps_at_both_ends() {
        let visible = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        assert_eq!(moved_in_order(&visible, "c", -1), ["a", "c", "b"]);
        assert_eq!(moved_in_order(&visible, "a", 1), ["b", "a", "c"]);
        // Already first/last, and an id that is not in the list at all.
        assert_eq!(moved_in_order(&visible, "a", -1), ["a", "b", "c"]);
        assert_eq!(moved_in_order(&visible, "c", 1), ["a", "b", "c"]);
        assert_eq!(moved_in_order(&visible, "d", -1), ["a", "b", "c"]);
    }

    #[test]
    fn selected_latency_is_scoped_by_profile_on_the_same_server() {
        let mut state = state();
        state.selected_profile = "work".to_string();
        state.sessions = vec![
            session("default", "connected", "same"),
            session("work", "connected", "same"),
        ];
        state
            .proxied
            .insert("default".to_string(), proxied(41, NOW_MS));
        state
            .proxied
            .insert("work".to_string(), proxied(83, NOW_MS));

        assert_eq!(active_latency_for(&state), (Some(83), false));
    }

    #[test]
    fn pool_latency_is_profile_scoped_without_pretending_one_member_is_active() {
        let mut state = state();
        state.selected_profile = "work".to_string();
        state.sessions = vec![SessionInfo {
            profile: "work".to_string(),
            state: "connected".to_string(),
            selection: SelectionInfo {
                kind: "pool".to_string(),
                members: vec![PoolMember {
                    server_id: "one".to_string(),
                    ..PoolMember::default()
                }],
                ..SelectionInfo::default()
            },
            ..SessionInfo::default()
        }];
        state
            .proxied
            .insert("work".to_string(), proxied(57, NOW_MS));

        assert_eq!(active_latency_for(&state), (Some(57), false));
        assert!(state.sessions[0].server_id.is_none());
    }

    #[test]
    fn a_pin_for_work_is_invisible_while_default_is_selected() {
        let mut state = state();
        let now = Instant::now();
        state.sessions = vec![SessionInfo {
            error: Some("work failed".to_string()),
            ..session("work", "error", "same")
        }];
        state.pin_status("work", Status::Connecting, now);

        assert!(matches!(state.current_status(), Status::Disconnected));
        assert!(matches!(selected_status(&state), Status::Disconnected));

        state.selected_profile = "work".to_string();
        assert!(matches!(selected_status(&state), Status::Connecting));
        state.clear_pin();
        assert_eq!(
            selected_status(&state),
            Status::Error("work failed".to_string())
        );
    }

    #[test]
    fn a_nondefault_pin_retires_against_its_own_session() {
        let mut state = state();
        let now = Instant::now();
        state.pin_status("work", Status::Connecting, now);
        let mut status = connected_to("default");
        status.sessions = vec![session("work", "disconnected", "same")];

        fold(&mut state, &snapshot(status, idle()), now, true);
        assert!(state.is_pinned(), "default must not retire work's pin");

        state.selected_profile = "work".to_string();
        assert!(matches!(selected_status(&state), Status::Connecting));
    }

    #[test]
    fn session_rows_keep_profile_order_and_profile_scoped_facts() {
        let mut work = profile("work", "shared");
        work.description = "Office".to_string();
        work.interface.enable = true;
        let mut home = profile("home", "shared");
        home.description = "Personal".to_string();

        let mut work_session = session("work", "connected", "same");
        work_session.server_alias = Some("shared".to_string());
        work_session.address = "127.91.37.1".to_string();
        work_session.owns_system_proxy = true;
        work_session.interface = Some(InterfaceInfo {
            device: "oxi-work".to_string(),
            address: "198.18.7.1".to_string(),
            mtu: 1500,
            routes: "manual".to_string(),
            table: 28_449,
            mark: 28_449,
            up: true,
        });
        let mut home_session = session("home", "connecting", "same");
        home_session.server_name = Some("Shared server".to_string());
        home_session.address = "127.92.38.1".to_string();

        let mut state = state();
        // Deliberately opposite to the profile order: profiles own row order.
        state.sessions = vec![home_session, work_session];
        state
            .proxied
            .insert("work".to_string(), proxied(71, NOW_MS - 185_000));
        state
            .proxied
            .insert("home".to_string(), proxied(83, NOW_MS));
        state.operation = Some(UiOperation::for_profile(
            super::super::operation::UiOperationKind::UpProfile,
            "home",
        ));

        let rows = session_rows(&[work, home], &state, NOW_MS);
        assert_eq!(
            rows.iter()
                .map(|row| row.profile.as_str())
                .collect::<Vec<_>>(),
            vec!["work", "home"]
        );
        assert_eq!(rows[0].state, SessionRowState::Connected);
        assert_eq!(rows[0].headline, "shared");
        assert!(rows[0].toggle_on);
        assert!(!rows[0].busy);
        // One badge, not four: the number is scanned beside the state, and its
        // age is spelled out below where there is room for the words.
        assert_eq!(rows[0].latency.as_deref(), Some("71 ms"));
        assert!(rows[0].warning.is_none());
        assert_eq!(
            rows[0]
                .details
                .iter()
                .map(|detail| (detail.label.as_str(), detail.value.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Proxy", "127.91.37.1:10808"),
                ("Interface", "oxi-work · 198.18.7.1 · manual"),
                ("Latency", "71 ms · measured 3 min ago"),
                ("System proxy", "Set by this connection"),
                ("Description", "Office"),
            ]
        );
        // The address is the one thing here someone pastes elsewhere.
        assert!(rows[0].details[0].copyable);
        assert!(rows[0].details[1..].iter().all(|detail| !detail.copyable));

        assert_eq!(rows[1].state, SessionRowState::Connecting);
        assert_eq!(rows[1].headline, "Shared server");
        assert!(rows[1].toggle_on);
        assert!(rows[1].busy);
        assert_eq!(rows[1].latency.as_deref(), Some("83 ms"));
        assert_eq!(
            rows[1]
                .details
                .iter()
                .map(|detail| (detail.label.as_str(), detail.value.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Proxy", "127.92.38.1:10808"),
                ("Routing", "Proxy only — no interface"),
                ("Latency", "83 ms"),
                ("Description", "Personal"),
            ]
        );
    }

    #[test]
    fn pool_rows_report_only_health_the_strategy_actually_supplies() {
        let mut work = profile("work", "");
        work.pool = Some(PoolQuery::default());
        let mut state = state();
        state.sessions = vec![SessionInfo {
            profile: "work".to_string(),
            state: "connected".to_string(),
            address: "127.91.37.1".to_string(),
            socks_port: 10808,
            selection: SelectionInfo {
                kind: "pool".to_string(),
                name: String::new(),
                strategy: "roundRobin".to_string(),
                members: vec![
                    PoolMember {
                        server_id: "one".to_string(),
                        alias: Some("one".to_string()),
                        name: "One".to_string(),
                        tag: "s-one".to_string(),
                        in_rotation: Some(true),
                    },
                    PoolMember {
                        server_id: "two".to_string(),
                        alias: Some("two".to_string()),
                        name: "Two".to_string(),
                        tag: "s-two".to_string(),
                        in_rotation: Some(false),
                    },
                ],
                // Two nodes on two hosts: the exit count repeats the node count
                // and the row must not say it twice.
                endpoints: 2,
                selecting: None,
                stale: true,
            },
            ..SessionInfo::default()
        }];

        let rows = session_rows(&[work.clone()], &state, NOW_MS);
        assert!(rows[0].pool);
        assert_eq!(rows[0].headline, "Group · 1 of 2 active");
        assert_eq!(
            pool_short_label(&state.sessions[0].selection),
            "group (1/2)"
        );
        let warning = rows[0].warning.as_ref().expect("a stale pool warns");
        assert_eq!(warning.text, "stale");
        assert_eq!(
            warning.tooltip.as_deref(),
            Some("Reconnect to pick up new servers")
        );
        assert!(
            rows[0]
                .details
                .iter()
                .any(|detail| detail.label == "Nodes" && detail.value == "1 of 2 in rotation")
        );
        assert!(
            rows[0]
                .details
                .iter()
                .any(|detail| detail.label == "Strategy" && detail.value == "roundRobin")
        );
        assert!(
            !rows[0]
                .details
                .iter()
                .any(|detail| detail.value.contains("exit address")),
            "a pool whose nodes are all on their own host says nothing about exits"
        );
        // The explanation belongs to the strategy, not to the count: roundRobin
        // must not be described as keeping only the nodes that answer.
        let nodes = |rows: &[SessionRow]| {
            rows[0]
                .details
                .iter()
                .find(|detail| detail.label == "Nodes")
                .and_then(|detail| detail.tooltip.clone())
                .expect("the node count is explained")
        };
        assert!(nodes(&rows).contains("takes turns"));

        state.sessions[0].selection.strategy = "leastPing".to_string();
        state.sessions[0].selection.selecting = Some("two".to_string());
        for member in &mut state.sessions[0].selection.members {
            member.in_rotation = None;
        }
        let rows = session_rows(&[work], &state, NOW_MS);
        assert_eq!(rows[0].headline, "Group · 2 nodes · now two");
        assert!(
            !rows[0].headline.contains("active"),
            "unknown health under a picking strategy must not become dead nodes"
        );
        // …and the short label cannot invent a rotation it was not told about.
        assert_eq!(pool_short_label(&state.sessions[0].selection), "group (2)");
        assert!(
            rows[0]
                .details
                .iter()
                .any(|detail| detail.label == "Nodes" && detail.value == "2 in the group")
        );
        assert!(nodes(&rows).contains("One node carries traffic"));
    }

    #[test]
    fn a_named_pool_is_called_by_its_name_everywhere_it_appears() {
        let mut work = profile("work", "");
        work.pool = Some(PoolQuery::default());
        let mut state = state();
        state.sessions = vec![SessionInfo {
            profile: "work".to_string(),
            state: "connected".to_string(),
            selection: SelectionInfo {
                kind: "pool".to_string(),
                name: "Germany".to_string(),
                strategy: "leastLoad".to_string(),
                members: (0..6)
                    .map(|index| PoolMember {
                        server_id: index.to_string(),
                        alias: Some(index.to_string()),
                        name: index.to_string(),
                        tag: format!("s-{index}"),
                        in_rotation: Some(index < 4),
                    })
                    .collect(),
                // The real German case in miniature: the provider spelled two
                // hosts six times.
                endpoints: 2,
                selecting: None,
                stale: false,
            },
            ..SessionInfo::default()
        }];
        let rows = session_rows(&[work], &state, NOW_MS);
        assert_eq!(rows[0].headline, "Group “Germany” · 4 of 6 active");
        assert_eq!(
            pool_short_label(&state.sessions[0].selection),
            "Germany (4/6)"
        );
        // The headline counts nodes because that is what the rotation is over;
        // the expanded row is where the spread the pool actually buys is said.
        assert!(rows[0].details.iter().any(|detail| detail.label == "Nodes"
            && detail.value == "4 of 6 in rotation · 2 exit addresses"));
    }

    /// Two stopped group profiles both read "Group" before this, which is the
    /// one state where the row has nothing else to tell them apart by.
    #[test]
    fn a_stopped_group_profile_still_says_which_group() {
        let mut named = profile("eu", "");
        named.pool = Some(PoolQuery {
            name: "Europe".to_string(),
            countries: vec!["DE".to_string(), "NL".to_string()],
            ..PoolQuery::default()
        });
        let mut nameless = profile("ad-hoc", "");
        nameless.pool = Some(PoolQuery {
            countries: vec!["JP".to_string()],
            ..PoolQuery::default()
        });
        let rows = session_rows(&[named, nameless], &state(), NOW_MS);
        assert_eq!(rows[0].headline, "Group “Europe”");
        assert_eq!(rows[1].headline, "Group · JP");
    }

    #[test]
    fn a_stopped_interface_row_uses_the_shared_device_derivation() {
        let mut work = profile("work", "shared");
        work.interface.enable = true;
        work.interface.routes = RouteMode::List;

        let rows = session_rows(&[work], &state(), NOW_MS);
        assert_eq!(rows[0].state, SessionRowState::Stopped);
        assert!(!rows[0].toggle_on);
        assert_eq!(
            rows[0]
                .details
                .iter()
                .map(|detail| (detail.label.as_str(), detail.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("Interface", "oxi-work · list")]
        );
    }

    /// The window repaints the page twice a second off the daemon's status. If
    /// the row ignored the request in flight, the switch the user just moved
    /// would be dragged back by the first tick and forward again by the answer.
    #[test]
    fn a_row_shows_the_operation_in_flight_rather_than_the_daemon_it_is_waiting_on() {
        let work = profile("work", "shared");
        let mut state = state();
        state.operation = Some(UiOperation::for_profile(UiOperationKind::UpProfile, "work"));

        let rows = session_rows(std::slice::from_ref(&work), &state, NOW_MS);
        assert_eq!(rows[0].state, SessionRowState::Connecting);
        assert!(rows[0].toggle_on);
        assert!(rows[0].busy);

        state.sessions = vec![session("work", "connected", "same")];
        state.operation = Some(UiOperation::for_profile(
            UiOperationKind::DownProfile,
            "work",
        ));
        let rows = session_rows(std::slice::from_ref(&work), &state, NOW_MS);
        assert_eq!(rows[0].state, SessionRowState::Stopped);
        assert!(!rows[0].toggle_on);

        // An operation on another profile leaves this row on the daemon's word.
        state.operation = Some(UiOperation::for_profile(
            UiOperationKind::DownProfile,
            "home",
        ));
        let rows = session_rows(&[work], &state, NOW_MS);
        assert_eq!(rows[0].state, SessionRowState::Connected);
        assert!(rows[0].toggle_on);
        assert!(!rows[0].busy);
    }

    /// A newer daemon may name a state this build has never heard of. Folding
    /// it into `Stopped` answered a question the build did not understand, and
    /// the switch then claimed the session was down.
    #[test]
    fn an_unreadable_session_state_is_unknown_rather_than_stopped() {
        let work = profile("work", "shared");
        let mut state = state();

        state.sessions = vec![session("work", "suspended", "shared")];
        let rows = session_rows(std::slice::from_ref(&work), &state, NOW_MS);
        assert_eq!(rows[0].state, SessionRowState::Unknown);
        assert!(!rows[0].toggle_on);

        // The two honest stops still read as stopped: the daemon saying so,
        // and the daemon carrying no session for this profile at all.
        state.sessions = vec![session("work", "disconnected", "shared")];
        let rows = session_rows(std::slice::from_ref(&work), &state, NOW_MS);
        assert_eq!(rows[0].state, SessionRowState::Stopped);

        state.sessions = Vec::new();
        let rows = session_rows(&[work], &state, NOW_MS);
        assert_eq!(rows[0].state, SessionRowState::Stopped);
    }

    /// `SessionInfo::state` documents four words and the daemon sends exactly
    /// those four; `"stopped"` is not among them and never has been. Waiting
    /// for it meant the one word the daemon *does* send for a stopped session
    /// fell through to `Unknown`, which the Profiles page draws as "Unknown"
    /// and, worse, uses to insensitise the row's switch — so a profile the
    /// daemon had stopped could not be started again from its own row.
    ///
    /// Reachable without a newer daemon: a confirmation that fails on a
    /// non-`Explicit` origin calls `stop_session`, which keeps the session and
    /// marks it disconnected, and installs no error override to rename it.
    #[test]
    fn every_state_the_daemon_sends_is_a_state_this_build_reads() {
        let work = profile("work", "shared");
        let mut state = state();

        for (wire, expected) in [
            ("disconnected", SessionRowState::Stopped),
            ("connecting", SessionRowState::Connecting),
            ("connected", SessionRowState::Connected),
            ("error", SessionRowState::Error),
        ] {
            state.sessions = vec![session("work", wire, "shared")];
            let rows = session_rows(std::slice::from_ref(&work), &state, NOW_MS);
            assert_eq!(
                rows[0].state, expected,
                "the daemon sends {wire:?}; no state it sends may read as Unknown"
            );
        }
    }

    /// The headline carries the same text, but a subtitle ellipsises and cannot
    /// be selected, so the failure was the one thing a bug report could not
    /// quote.
    /// A network that is deliberately dead must not look like one that is
    /// broken. The row says which it is without being expanded, because the
    /// question it answers — is anything leaving this machine unprotected? — is
    /// not one to make somebody go looking for.
    #[test]
    fn a_session_holding_traffic_says_so_on_the_collapsed_row() {
        let work = profile("work", "shared");
        let mut state = state();
        let mut held = session("work", "error", "shared");
        held.error = Some("Xray exited unexpectedly".to_string());
        held.holding_traffic = true;
        state.sessions = vec![held];

        let rows = session_rows(&[work], &state, NOW_MS);
        let warning = rows[0]
            .warning
            .as_ref()
            .expect("holding is a warning, not a detail nobody sees");
        assert_eq!(warning.text, "holding traffic");
        assert!(
            warning
                .tooltip
                .as_deref()
                .is_some_and(|tooltip| tooltip.contains("dropped")),
            "the pill has to say what holding does, not only that it happens"
        );
        assert!(
            rows[0]
                .details
                .iter()
                .any(|detail| detail.label == "Traffic"),
            "and the expanded row explains it"
        );
    }

    /// The failure it is paired with still reads as before: a session that lost
    /// its core but had nothing installed to hold is an ordinary failed one.
    #[test]
    fn a_failed_session_that_holds_nothing_gains_no_warning() {
        let work = profile("work", "shared");
        let mut state = state();
        let mut failed = session("work", "error", "shared");
        failed.error = Some("Xray exited unexpectedly".to_string());
        state.sessions = vec![failed];

        let rows = session_rows(&[work], &state, NOW_MS);
        assert!(rows[0].warning.is_none());
        assert!(
            rows[0]
                .details
                .iter()
                .all(|detail| detail.label != "Traffic")
        );
    }

    #[test]
    fn a_failed_session_offers_its_error_as_a_copyable_detail() {
        let work = profile("work", "shared");
        let mut state = state();
        let mut failed = session("work", "error", "shared");
        failed.error = Some("spawning xray (xray): No such file or directory".to_string());
        state.sessions = vec![failed];

        let rows = session_rows(&[work], &state, NOW_MS);
        let error = rows[0]
            .details
            .iter()
            .find(|detail| detail.label == "Error")
            .expect("the failure is a detail row, not only a subtitle");
        assert_eq!(
            error.value,
            "spawning xray (xray): No such file or directory"
        );
        assert!(error.copyable);
    }

    #[test]
    fn switcher_is_absent_for_one_profile_and_reuses_row_states() {
        let profiles = vec![profile("default", "same"), profile("work", "same")];
        let mut state = state();
        state.sessions = vec![session("work", "connected", "same")];

        assert!(!switcher_visible(&profiles[..1]));
        assert!(switcher_visible(&profiles));
        assert_eq!(
            switcher_items(&profiles, &state),
            vec![
                SwitcherItem {
                    profile: "default".to_string(),
                    state: SessionRowState::Stopped,
                    selected: true,
                },
                SwitcherItem {
                    profile: "work".to_string(),
                    state: SessionRowState::Connected,
                    selected: false,
                },
            ]
        );
    }

    #[test]
    fn nondefault_card_actions_follow_runtime_then_stored_handle() {
        let mut state = state();
        state.selected_profile = "work".to_string();
        state.sessions = vec![session("work", "connected", "same")];
        let mut work = profile("work", "shared");

        assert_eq!(
            card_action(&[work.clone()], &state, "same"),
            CardAction::DownProfile("work".to_string())
        );

        let mut alias_source = session("default", "connected", "same");
        alias_source.server_alias = Some("shared".to_string());
        state.sessions = vec![alias_source];
        assert_eq!(
            card_action(&[work.clone()], &state, "same"),
            CardAction::UpProfile("work".to_string())
        );

        work.server = "other".to_string();
        assert_eq!(
            card_action(&[work], &state, "same"),
            CardAction::RepointAndUp {
                profile: "work".to_string(),
                server_id: "same".to_string(),
                replaces_pool: false,
            }
        );
    }

    #[test]
    fn clicking_a_member_replaces_a_pool_only_after_confirmation() {
        for selected in ["default", "work"] {
            let mut state = state();
            state.selected_profile = selected.to_string();
            state.sessions = vec![SessionInfo {
                profile: selected.to_string(),
                state: "connected".to_string(),
                selection: SelectionInfo {
                    kind: "pool".to_string(),
                    members: vec![PoolMember {
                        server_id: "member".to_string(),
                        ..PoolMember::default()
                    }],
                    ..SelectionInfo::default()
                },
                ..SessionInfo::default()
            }];
            let mut entry = profile(selected, "");
            entry.pool = Some(PoolQuery::default());

            assert_eq!(
                card_action(&[entry], &state, "member"),
                CardAction::RepointAndUp {
                    profile: selected.to_string(),
                    server_id: "member".to_string(),
                    replaces_pool: true,
                }
            );
        }
    }

    /// The optimistic "Connecting…" has to survive the daemon still reporting
    /// the world as it was before the click — that lag is the whole reason the
    /// pin exists.
    #[test]
    fn an_optimistic_status_outlives_a_daemon_that_has_not_caught_up() {
        let mut state = state();
        let now = Instant::now();
        state.pin_status("default", Status::Connecting, now);
        fold(
            &mut state,
            &snapshot(StatusInfo::from_status(&Status::Disconnected, None), idle()),
            now,
            true,
        );
        assert!(matches!(state.current_status(), Status::Connecting));
    }

    #[test]
    fn an_optimistic_status_is_retired_once_the_daemon_agrees() {
        let mut state = state();
        let now = Instant::now();
        state.pin_status("default", Status::Connecting, now);
        fold(
            &mut state,
            &snapshot(
                StatusInfo::from_status(&Status::Connected, Some("a".into())),
                idle(),
            ),
            now,
            true,
        );
        assert!(!state.is_pinned());
        assert!(matches!(state.current_status(), Status::Connected));
        // ...and only then may the daemon's active server land.
        assert_eq!(state.connected_id.as_deref(), Some("a"));
    }

    /// The mirror image: "Disconnected" pinned by the click must not be undone
    /// by a daemon that is still tearing the tunnel down.
    #[test]
    fn an_optimistic_disconnect_outlives_a_tunnel_still_going_down() {
        let mut state = state();
        let now = Instant::now();
        state.connected_id = None;
        state.pin_status("default", Status::Disconnected, now);
        fold(
            &mut state,
            &snapshot(
                StatusInfo::from_status(&Status::Connected, Some("a".into())),
                idle(),
            ),
            now,
            true,
        );
        assert!(matches!(state.current_status(), Status::Disconnected));
        assert_eq!(state.connected_id, None);
    }

    /// The completion handler pins the failure it just showed; the snapshot the
    /// same handler carries must not wipe it before the user can read it.
    #[test]
    fn a_pinned_failure_survives_its_own_completion_snapshot() {
        let mut state = state();
        let now = Instant::now();
        state.pin_status("default", Status::Error("no route to host".into()), now);
        fold(
            &mut state,
            &snapshot(
                StatusInfo::from_status(&Status::Connecting, Some("a".into())),
                idle(),
            ),
            now,
            true,
        );
        assert!(matches!(state.current_status(), Status::Error(_)));
    }

    /// A daemon that restarted mid-connect never contradicts the pin, so
    /// without the deadline the UI would say "Connecting…" forever.
    #[test]
    fn a_pin_does_not_outlive_its_deadline() {
        let mut state = state();
        let now = Instant::now();
        state.pin_status("default", Status::Connecting, now);
        fold(
            &mut state,
            &snapshot(StatusInfo::from_status(&Status::Disconnected, None), idle()),
            now + PENDING_STATUS_DEADLINE + std::time::Duration::from_secs(1),
            true,
        );
        assert!(!state.is_pinned());
    }

    /// While the user's own operation is in flight there is nothing to arbitrate:
    /// the pin is the UI's only description of what is happening.
    #[test]
    fn an_in_flight_operation_holds_the_pin_unconditionally() {
        let mut state = state();
        let now = Instant::now();
        state.operation = Some(UiOperation::new(
            super::super::operation::UiOperationKind::Connect,
        ));
        state.pin_status("default", Status::Connecting, now);
        fold(
            &mut state,
            &snapshot(
                StatusInfo::from_status(&Status::Connected, Some("a".into())),
                idle(),
            ),
            now + PENDING_STATUS_DEADLINE + std::time::Duration::from_secs(1),
            true,
        );
        assert!(state.is_pinned());
    }

    #[test]
    fn active_profile_is_initialized_and_updated_from_current_snapshots() {
        let initial = StatusInfo::default().with_active_profile(Some("home".to_string()));
        let mut state = SnapshotState::new(&initial);
        assert_eq!(state.active_profile.as_deref(), Some("home"));

        let current = StatusInfo::default().with_active_profile(Some("work".to_string()));
        fold(&mut state, &snapshot(current, idle()), Instant::now(), true);
        assert_eq!(state.active_profile.as_deref(), Some("work"));
    }

    #[test]
    fn connected_profiles_group_two_sessions_on_the_same_server() {
        let status = StatusInfo {
            state: "connected".to_string(),
            active_id: Some("same".to_string()),
            active_profile: Some("home".to_string()),
            sessions: vec![
                SessionInfo {
                    profile: "home".to_string(),
                    state: "connected".to_string(),
                    server_id: Some("same".to_string()),
                    ..SessionInfo::default()
                },
                SessionInfo {
                    profile: "work".to_string(),
                    state: "connected".to_string(),
                    server_id: Some("same".to_string()),
                    ..SessionInfo::default()
                },
                SessionInfo {
                    profile: "broken".to_string(),
                    state: "error".to_string(),
                    server_id: Some("same".to_string()),
                    ..SessionInfo::default()
                },
            ],
            ..StatusInfo::default()
        };

        assert_eq!(
            connected_profiles(&status).get("same"),
            Some(&ServerProfiles {
                connected: vec!["home".to_string(), "work".to_string()],
                in_pool: Vec::new(),
            })
        );
        assert_eq!(
            other_profiles_message(&status.sessions, "home").as_deref(),
            Some("2 more profiles are running")
        );
    }

    #[test]
    fn pool_members_are_marked_in_pool_not_connected_or_dead() {
        let status = StatusInfo {
            sessions: vec![SessionInfo {
                profile: "spread".to_string(),
                state: "connected".to_string(),
                selection: SelectionInfo {
                    kind: "pool".to_string(),
                    strategy: "leastPing".to_string(),
                    members: vec![
                        PoolMember {
                            server_id: "one".to_string(),
                            in_rotation: None,
                            ..PoolMember::default()
                        },
                        PoolMember {
                            server_id: "two".to_string(),
                            in_rotation: None,
                            ..PoolMember::default()
                        },
                    ],
                    selecting: Some("one".to_string()),
                    ..SelectionInfo::default()
                },
                ..SessionInfo::default()
            }],
            ..StatusInfo::default()
        };

        let profiles = connected_profiles(&status);
        for id in ["one", "two"] {
            assert_eq!(
                profiles.get(id),
                Some(&ServerProfiles {
                    connected: Vec::new(),
                    in_pool: vec!["spread".to_string()],
                })
            );
        }
    }

    #[test]
    fn proxied_latency_is_selected_by_profile_even_on_the_same_server() {
        let mut status = connected_to("same");
        status.active_profile = Some("home".to_string());
        status.sessions = vec![
            SessionInfo {
                profile: "home".to_string(),
                state: "connected".to_string(),
                server_id: Some("same".to_string()),
                ..SessionInfo::default()
            },
            SessionInfo {
                profile: "work".to_string(),
                state: "connected".to_string(),
                server_id: Some("same".to_string()),
                ..SessionInfo::default()
            },
        ];
        let mut probes = idle();
        probes.proxied.insert(
            "home".to_string(),
            LatencyReading::ok(41, ProbeRoute::Proxied, LatencyMethod::HttpGet),
        );
        probes.proxied.insert(
            "work".to_string(),
            LatencyReading::ok(83, ProbeRoute::Proxied, LatencyMethod::HttpGet),
        );
        let mut state = SnapshotState::new(&status);

        fold(
            &mut state,
            &snapshot(status.clone(), probes.clone()),
            Instant::now(),
            true,
        );
        assert_eq!(active_latency_for(&state), (Some(41), false));

        status.active_profile = Some("work".to_string());
        fold(&mut state, &snapshot(status, probes), Instant::now(), true);
        assert_eq!(active_latency_for(&state), (Some(83), false));
    }

    /// The flicker in one test: a round that started before the click reports
    /// the pre-click world, and must not be allowed to describe it.
    #[test]
    fn a_snapshot_from_before_the_last_action_is_dropped_whole() {
        let mut state = state();
        state.state_epoch = 1;
        state.connected_id = Some("a".to_string());
        state.active_profile = Some("home".to_string());
        state.pin_status("default", Status::Connecting, Instant::now());

        let stale = snapshot(
            StatusInfo::from_status(&Status::Disconnected, None)
                .with_active_profile(Some("work".to_string())),
            probe(&[], &[("a", Some(41))]),
        );
        assert!(reduce(&mut state, &stale, Instant::now(), NOW_MS, true).is_none());
        assert_eq!(state.connected_id.as_deref(), Some("a"));
        assert_eq!(state.active_profile.as_deref(), Some("home"));
        assert!(state.readings.is_empty());
        assert!(matches!(state.current_status(), Status::Connecting));
    }

    /// The completion handler of an operation re-stamps its own snapshot with
    /// the epoch it just bumped, because those reads happened *after* the
    /// daemon returned. Equality has to pass, not just "greater than".
    #[test]
    fn a_snapshot_stamped_with_the_current_epoch_is_applied() {
        let mut state = state();
        state.state_epoch = 3;
        let mut current = snapshot(StatusInfo::default(), probe(&[], &[("a", Some(41))]));
        current.epoch = 3;
        assert!(reduce(&mut state, &current, Instant::now(), NOW_MS, true).is_some());
        assert_eq!(state.readings.get("a").map(|r| r.value), Some(Some(41)));
    }

    #[test]
    fn a_probe_the_daemon_started_raises_a_spinner() {
        let mut state = state();
        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&["a"], &[])),
            Instant::now(),
            true,
        );
        assert_eq!(latency(&effects, "a"), Some(LatencyState::Checking));
        assert!(state.checking.contains_key("a"));
    }

    #[test]
    fn a_finished_probe_retires_its_spinner_with_the_reading() {
        let mut state = state();
        let now = Instant::now();
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&["a"], &[])),
            now,
            true,
        );
        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&[], &[("a", Some(41))])),
            now,
            true,
        );
        assert_eq!(latency(&effects, "a"), Some(fresh(41)));
        assert!(!state.checking.contains_key("a"));
    }

    /// The fake ping in one test: a server waiting for its slot still carries
    /// the number from its *previous* measurement, and a queued probe must not
    /// let that number pass for the result of this one.
    #[test]
    fn a_queued_probe_is_not_a_finished_probe() {
        let mut state = state();
        let now = Instant::now();
        let mut waiting = probe(&[], &[("a", Some(41))]);
        waiting.queued = vec!["a".to_string()];

        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), waiting),
            now,
            true,
        );
        assert_eq!(latency(&effects, "a"), Some(LatencyState::Checking));
        assert!(state.checking.contains_key("a"));
    }

    /// The spinner goes up before the D-Bus request is even sent, so the tick
    /// that lands in between finds the id in neither set — and must wait.
    #[test]
    fn an_unacknowledged_probe_keeps_its_spinner_through_the_grace() {
        let mut state = state();
        let now = Instant::now();
        state.checking.insert("a".to_string(), ProbeWait::new(now));

        fold(
            &mut state,
            &snapshot(StatusInfo::default(), idle()),
            now + std::time::Duration::from_millis(500),
            true,
        );
        assert!(state.checking.contains_key("a"), "still within the grace");

        // Past it, the request is assumed lost rather than left spinning.
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), idle()),
            now + PROBE_ACK_GRACE + std::time::Duration::from_secs(1),
            true,
        );
        assert!(!state.checking.contains_key("a"));
    }

    /// Once the daemon has confirmed it holds the id, its dropping out of both
    /// sets is the result arriving — no grace needed.
    #[test]
    fn an_acknowledged_probe_retires_as_soon_as_the_daemon_lets_go() {
        let mut state = state();
        let now = Instant::now();
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&["a"], &[])),
            now,
            true,
        );
        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&[], &[("a", Some(41))])),
            now,
            true,
        );
        assert_eq!(latency(&effects, "a"), Some(fresh(41)));
        assert!(!state.checking.contains_key("a"));
    }

    /// A daemon that lost track of a probe it still claims to be running would
    /// otherwise keep the card spinning for the rest of the session.
    #[test]
    fn a_probe_the_daemon_never_finishes_gives_up_at_the_deadline() {
        let mut state = state();
        let now = Instant::now();
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&["a"], &[])),
            now,
            true,
        );
        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&["a"], &[])),
            now + PROBE_DEADLINE + std::time::Duration::from_secs(1),
            true,
        );
        // No reading was ever taken, so the card must not claim "unreachable".
        assert_eq!(latency(&effects, "a"), Some(LatencyState::Unmeasured));
        assert!(!state.is_checking("a"));
    }

    /// The backstop could not stop anything. It removed the wait outright, and
    /// the daemon was still naming the id — so the very next tick read it as a
    /// probe it had not seen, raised the spinner and restarted the clock. The
    /// card flickered once a deadline forever instead of settling, and a daemon
    /// that had genuinely lost a probe kept its card spinning regardless, which
    /// is the one thing the deadline exists to prevent.
    #[test]
    fn a_probe_given_up_on_does_not_get_its_spinner_back() {
        let mut state = state();
        let now = Instant::now();
        let held = snapshot(StatusInfo::default(), probe(&["a"], &[]));
        fold(&mut state, &held, now, true);
        assert!(state.is_checking("a"));

        let past = now + PROBE_DEADLINE + std::time::Duration::from_secs(1);
        assert_eq!(
            latency(&fold(&mut state, &held, past, true), "a"),
            Some(LatencyState::Unmeasured)
        );

        // The daemon still holds it, and goes on holding it. The spinner must
        // stay down through every one of those ticks.
        for tick in 1..=3 {
            let later = past + std::time::Duration::from_millis(500 * tick);
            let effects = fold(&mut state, &held, later, true);
            assert_ne!(
                latency(&effects, "a"),
                Some(LatencyState::Checking),
                "tick {tick} raised the spinner again"
            );
            assert!(!state.is_checking("a"), "tick {tick} restarted the wait");
        }

        // Letting go drops the wait, so a later check starts clean.
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&[], &[])),
            past + std::time::Duration::from_secs(4),
            true,
        );
        assert!(!state.checking.contains_key("a"));
    }

    /// A wait the deadline gave up on is still in the map, and the strip
    /// counts that map. Counting it would have the header announce a check
    /// while every card shows none.
    #[test]
    fn a_probe_given_up_on_is_not_counted_as_running() {
        let mut state = state();
        let now = Instant::now();
        let held = snapshot(StatusInfo::default(), probe(&["a", "b"], &[]));
        fold(&mut state, &held, now, true);
        assert_eq!(
            state
                .checking
                .keys()
                .filter(|id| state.is_checking(id))
                .count(),
            2
        );

        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&["a"], &[])),
            now + PROBE_DEADLINE + std::time::Duration::from_secs(1),
            true,
        );
        // "b" was let go and forgotten; "a" is held but given up on. Neither is
        // a check in progress, and the map still remembers one of them.
        assert!(state.checking.contains_key("a"));
        assert_eq!(
            state
                .checking
                .keys()
                .filter(|id| state.is_checking(id))
                .count(),
            0
        );
    }

    /// The card the user explicitly pinged is the only one allowed to raise a
    /// toast; a background sweep failing must stay silent.
    #[test]
    fn only_an_explicit_ping_toasts_its_failure() {
        let mut state = state();
        state.notify_probe.insert("a".to_string());
        let effects = fold(
            &mut state,
            &snapshot(
                StatusInfo::default(),
                probe(&[], &[("a", None), ("b", None)]),
            ),
            Instant::now(),
            true,
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::ToastUnreachable))
                .count(),
            1
        );
    }

    #[test]
    fn no_network_toasts_once_and_does_not_redden_cards() {
        let mut state = state();
        state.notify_probe.insert("a".to_string());
        let mut no_network = idle();
        let mut reading = LatencyReading::failed(
            ProbeFailure::NoNetwork,
            ProbeRoute::Direct,
            LatencyMethod::HttpGet,
        );
        reading.measured_at_unix_ms = NOW_MS;
        no_network.readings.insert("a".to_string(), reading);

        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), no_network),
            Instant::now(),
            true,
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::ToastNoNetwork))
                .count(),
            1
        );
        assert_eq!(
            latency(&effects, "a"),
            Some(LatencyState::NoNetwork),
            "an offline machine is not a dead server"
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::ToastUnreachable))
        );
    }

    /// The failure a machine with no Xray core produces for every server at
    /// once. Reporting it as an unresponsive server is how nine working nodes
    /// come to look dead.
    fn no_core(id: &str) -> ProbeState {
        let mut state = idle();
        let mut reading = LatencyReading::failed(
            ProbeFailure::Unknown,
            ProbeRoute::Direct,
            LatencyMethod::HttpGet,
        );
        reading.measured_at_unix_ms = NOW_MS;
        state.readings.insert(id.to_string(), reading);
        state
    }

    #[test]
    fn a_probe_that_never_ran_is_not_a_verdict_on_the_server() {
        let mut state = state();
        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), no_core("a")),
            Instant::now(),
            true,
        );
        assert_eq!(
            latency(&effects, "a"),
            Some(LatencyState::NotRun(None)),
            "a check that never left this machine says nothing about the server"
        );
    }

    /// A sweep is silent about one quiet server, because its card says so
    /// already — but a machine that measured nothing at all has to speak,
    /// since every card looks the same and none of them explains why.
    #[test]
    fn a_sweep_reports_a_machine_that_could_not_measure_anything() {
        let mut state = state();
        state.notify_local.insert("a".to_string());
        state.notify_local.insert("b".to_string());
        let mut probes = no_core("a");
        let mut second = LatencyReading::failed(
            ProbeFailure::Unknown,
            ProbeRoute::Direct,
            LatencyMethod::HttpGet,
        );
        second.measured_at_unix_ms = NOW_MS;
        probes.readings.insert("b".to_string(), second);

        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), probes),
            Instant::now(),
            true,
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::ToastProbeDidNotRun))
                .count(),
            1,
            "two failures of the same machine are one piece of news"
        );
    }

    /// The case a naive rule gets backwards. Cards retire one at a time, so for
    /// most of a sweep's life the set is partly checked and partly done — and
    /// throughout that, the button says stop and must mean it.
    #[test]
    fn a_press_part_way_through_a_sweep_still_means_stop() {
        let now = Instant::now();
        let mut checking = HashMap::new();
        checking.insert("still-going".to_string(), ProbeWait::new(now));
        let swept = vec![
            "done".to_string(),
            "still-going".to_string(),
            "also-done".to_string(),
        ];

        assert!(
            press_stops(&swept, &checking),
            "two of the three have finished, but the sweep has not"
        );
        assert!(
            !press_stops(&swept, &HashMap::new()),
            "with nothing checking, the same press starts a sweep"
        );
        assert!(
            !press_stops(&[], &checking),
            "an empty block offers nothing to stop"
        );
    }

    /// The interface sends a cancel and repaints nothing, on the strength of
    /// this: the daemon leaves a `Cancelled` reading, the id leaves
    /// `running ∪ queued`, and the spinner retires through the same path a
    /// finished check uses. If that stopped holding, every cancelled card would
    /// spin until the five-minute deadline, and the fix would be a second copy
    /// of this rule in the widget layer.
    #[test]
    fn a_cancelled_probe_retires_its_spinner_and_says_why() {
        let mut state = state();
        let now = Instant::now();
        // The daemon reports it queued: the card adopts the spinner and the
        // wait is acknowledged.
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&["a"], &[])),
            now,
            true,
        );
        assert!(state.checking.contains_key("a"), "the spinner is up");

        let mut probes = idle();
        let mut reading = LatencyReading::failed_locally(
            ProbeFailure::Unknown,
            ProbeDetail::Cancelled,
            ProbeRoute::Direct,
            LatencyMethod::default(),
        );
        reading.measured_at_unix_ms = NOW_MS;
        probes.readings.insert("a".to_string(), reading);

        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), probes),
            now,
            true,
        );

        assert!(
            !state.checking.contains_key("a"),
            "the id left the queue, so the spinner retires"
        );
        assert_eq!(
            latency(&effects, "a"),
            Some(LatencyState::NotRun(Some(ProbeDetail::Cancelled))),
            "and the card says the check was stopped, not that the server failed"
        );
    }

    /// The whole reason a cancel carries its own reason on the wire. Without
    /// this, stopping a sweep of a large subscription answers the user with a
    /// red toast about an action that did exactly what they asked.
    #[test]
    fn a_cancelled_sweep_does_not_toast_a_failure() {
        let mut state = state();
        let mut probes = idle();
        for id in ["a", "b", "c"] {
            state.notify_local.insert(id.to_string());
            let mut reading = LatencyReading::failed_locally(
                ProbeFailure::Unknown,
                ProbeDetail::Cancelled,
                ProbeRoute::Direct,
                LatencyMethod::default(),
            );
            reading.measured_at_unix_ms = NOW_MS;
            probes.readings.insert(id.to_string(), reading);
        }

        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), probes),
            Instant::now(),
            true,
        );

        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                Effect::ToastProbeDidNotRun | Effect::ToastUnreachable | Effect::ToastNoNetwork
            )),
            "a cancel is not a failure"
        );
    }

    /// A cancel that the user asked for on one card is just as much not a
    /// failure as a cancelled sweep. `asked` is the stronger of the two flags —
    /// it toasts conditions a sweep stays quiet about — so it is worth pinning
    /// separately rather than trusting the sweep case to cover it.
    #[test]
    fn cancelling_one_card_does_not_toast_a_failure() {
        let mut state = state();
        state.notify_probe.insert("a".to_string());
        let mut probes = idle();
        let mut reading = LatencyReading::failed_locally(
            ProbeFailure::Unknown,
            ProbeDetail::Cancelled,
            ProbeRoute::Direct,
            LatencyMethod::default(),
        );
        reading.measured_at_unix_ms = NOW_MS;
        probes.readings.insert("a".to_string(), reading);

        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), probes),
            Instant::now(),
            true,
        );

        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                Effect::ToastProbeDidNotRun | Effect::ToastUnreachable | Effect::ToastNoNetwork
            )),
            "the user stopped it on purpose"
        );
    }

    /// The guard is for `Cancelled` specifically and must not have muted the
    /// local faults the sweep exists to report — a machine with no core fails
    /// every server at once, and the cards alone read as a dead subscription.
    #[test]
    fn a_local_fault_still_toasts_now_that_a_cancel_does_not() {
        assert_eq!(
            probe_toast(
                &LatencyReading::failed_locally(
                    ProbeFailure::Unknown,
                    ProbeDetail::NoCore,
                    ProbeRoute::Direct,
                    LatencyMethod::default(),
                ),
                false,
                true,
            ),
            Some(ProbeToast::DidNotRun)
        );
        assert_eq!(
            probe_toast(
                &LatencyReading::failed_locally(
                    ProbeFailure::Unknown,
                    ProbeDetail::Cancelled,
                    ProbeRoute::Direct,
                    LatencyMethod::default(),
                ),
                false,
                true,
            ),
            None,
            "same failure, same flags — only the reason differs"
        );
    }

    #[test]
    fn a_sweep_stays_quiet_about_a_server_that_did_not_answer() {
        let mut state = state();
        state.notify_local.insert("a".to_string());
        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&[], &[("a", None)])),
            Instant::now(),
            true,
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::ToastUnreachable)),
            "the card already shows it; a sweep does not narrate every silent node"
        );
    }

    #[test]
    fn the_core_banner_speaks_only_when_the_daemon_found_no_core() {
        assert_eq!(
            missing_core_message(None),
            None,
            "an older daemon says nothing"
        );
        let mut runtime = RuntimeInfo::default();
        assert_eq!(
            missing_core_message(Some(&runtime)),
            None,
            "a resolved core is not news"
        );
        runtime.xray_error = Some("`xray` was not found on $PATH".to_string());
        assert!(missing_core_message(Some(&runtime)).is_some());
    }

    /// One banner, and the missing core outranks the missing data: a core that
    /// could not be resolved was never asked about its geo data, so saying both
    /// would be two lines about one cause and the second would be invented.
    #[test]
    fn the_banner_reports_a_missing_core_before_it_reports_missing_geo_data() {
        let mut runtime = RuntimeInfo {
            xray_error: Some("`xray` was not found on $PATH".to_string()),
            geo: GeoAssets {
                usable: Some(false),
                ..GeoAssets::default()
            },
            ..RuntimeInfo::default()
        };
        let message = missing_core_message(Some(&runtime)).expect("a banner");
        assert!(message.contains("No Xray core"), "{message}");
        assert!(!message.contains("geo"), "one cause, one line: {message}");

        runtime.xray_error = None;
        let message = missing_core_message(Some(&runtime)).expect("a banner");
        assert!(message.contains("geoip.dat"), "{message}");
    }

    /// `None` is nobody having asked — an older daemon, or no core to ask — and
    /// the only honest response is silence. Reporting it as missing data would
    /// accuse every machine running a daemon that predates the check.
    #[test]
    fn the_banner_says_nothing_until_the_daemon_has_determined_whether_the_lists_load() {
        let mut runtime = RuntimeInfo::default();
        assert_eq!(runtime.geo.usable, None, "the default is undetermined");
        assert_eq!(missing_core_message(Some(&runtime)), None);
        runtime.geo.usable = Some(true);
        assert_eq!(missing_core_message(Some(&runtime)), None);
    }

    /// The offer follows the same rule, and adds the one the banner does not
    /// have to make: whether a button would actually achieve anything.
    #[test]
    fn the_download_is_offered_only_when_the_daemon_says_the_lists_will_not_load() {
        let usable = RuntimeInfo::default();
        assert_eq!(
            geo_offer(Some(&usable), true, DaemonSource::Session),
            GeoOffer::Silent,
            "undetermined offers nothing"
        );

        let mut working = RuntimeInfo::default();
        working.geo.usable = Some(true);
        assert_eq!(
            geo_offer(Some(&working), true, DaemonSource::Session),
            GeoOffer::Working
        );

        let mut missing = RuntimeInfo::default();
        missing.geo.usable = Some(false);
        missing.geo.writable = true;
        assert_eq!(
            geo_offer(Some(&missing), true, DaemonSource::Session),
            GeoOffer::Download
        );

        // No core resolved: the missing core is the news, and this row is not.
        let mut coreless = missing.clone();
        coreless.xray_error = Some("`xray` was not found".to_string());
        assert_eq!(
            geo_offer(Some(&coreless), true, DaemonSource::Session),
            GeoOffer::Silent
        );
    }

    /// A daemon that cannot write its own asset directory gets no button. A
    /// control that fails the moment it is pressed is worse than its absence,
    /// and the directory is named so the reader can fix it.
    #[test]
    fn a_daemon_that_cannot_write_its_directory_is_not_offered_a_button() {
        let mut runtime = RuntimeInfo::default();
        runtime.geo.usable = Some(false);
        runtime.geo.writable = false;
        runtime.geo.dir = "/var/lib/oxidom/assets".to_string();
        assert_eq!(
            geo_offer(Some(&runtime), true, DaemonSource::System),
            GeoOffer::Unwritable {
                dir: "/var/lib/oxidom/assets".to_string()
            }
        );
    }

    /// The case that has to be got right, and the reason this is a reducer at
    /// all. A daemon too old to download is also too old to set
    /// `XRAY_LOCATION_ASSET` on the core it spawns, so files the GUI writes
    /// into the user's home help nobody — and against the *system* service they
    /// are unreadable twice over, since it runs as `oxidom` with
    /// `ProtectHome=true`. There, only a command helps.
    #[test]
    fn a_system_daemon_too_old_to_download_is_offered_a_command_and_not_a_button() {
        let mut runtime = RuntimeInfo::default();
        runtime.geo.usable = Some(false);
        runtime.geo.writable = true;

        assert_eq!(
            geo_offer(Some(&runtime), false, DaemonSource::System),
            GeoOffer::CommandOnly {
                session_fallback: false
            },
            "a system service cannot read the home directory the GUI would write to"
        );
        for source in [DaemonSource::Session, DaemonSource::Spawned] {
            assert_eq!(
                geo_offer(Some(&runtime), false, source),
                GeoOffer::CommandOnly {
                    session_fallback: true
                },
                "a daemon running as this user could at least read them"
            );
        }
    }

    /// A running download outranks every other state, including one that says
    /// the lists still will not load — which is exactly what the daemon reports
    /// mid-download, because the verdict it has cached predates the files.
    #[test]
    fn a_running_download_is_reported_as_progress_rather_than_as_a_fresh_offer() {
        let mut runtime = RuntimeInfo::default();
        runtime.geo.usable = Some(false);
        runtime.geo.writable = true;
        runtime.geo.downloading = true;
        runtime.geo.current_file = Some("geoip.dat".to_string());
        runtime.geo.done_bytes = 8 * 1024 * 1024;
        runtime.geo.total_bytes = 22 * 1024 * 1024;
        assert_eq!(
            geo_offer(Some(&runtime), true, DaemonSource::Session),
            GeoOffer::Running {
                file: "geoip.dat".to_string(),
                done: 8 * 1024 * 1024,
                total: 22 * 1024 * 1024,
            }
        );
    }

    /// A server that sends no `Content-Length` leaves nothing to divide by, and
    /// the bar then pulses — but the bytes so far are still true and still
    /// worth printing, because a pulsing bar alone cannot be told from a stall.
    #[test]
    fn progress_without_a_total_still_says_how_far_it_has_got() {
        assert_eq!(
            geo_progress_text("geoip.dat", 8 * 1024 * 1024, 22 * 1024 * 1024),
            "geoip.dat — 8.0 MB of 22.0 MB"
        );
        assert_eq!(
            geo_progress_text("geoip.dat", 8 * 1024 * 1024, 0),
            "geoip.dat — 8.0 MB"
        );
        // Below a megabyte the unit changes rather than printing "0.0 MB".
        assert_eq!(human_bytes(4096), "4 kB");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MB");
    }

    /// An outdated daemon sends numbers that cannot be dated or attributed, so
    /// the GUI reports none of them — but the resulting all-unmeasured list
    /// reads as a network problem unless it says why, once.
    #[test]
    fn an_outdated_daemon_is_reported_once() {
        let mut state = state();
        let outdated = || snapshot(StatusInfo::default(), ProbeState::default());

        let effects = fold(&mut state, &outdated(), Instant::now(), true);
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::DaemonOutdated))
                .count(),
            1
        );

        let effects = fold(&mut state, &outdated(), Instant::now(), true);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::DaemonOutdated))
        );
    }

    #[test]
    fn a_daemon_error_is_toasted_once() {
        let mut state = state();
        let broken = StatusInfo::from_status(&Status::Error("core died".into()), None);
        let effects = fold(
            &mut state,
            &snapshot(broken.clone(), idle()),
            Instant::now(),
            true,
        );
        assert!(effects.iter().any(
            |effect| matches!(effect, Effect::ConnectionError(message) if message == "core died")
        ));
        let effects = fold(&mut state, &snapshot(broken, idle()), Instant::now(), true);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::ConnectionError(_)))
        );
    }

    /// A failure that names its server is what lets the card the user clicked
    /// say "Error" instead of falling back to looking merely disconnected.
    #[test]
    fn a_failure_keeps_naming_its_server_until_something_replaces_it() {
        let mut state = state();
        let mut broken = StatusInfo::from_status(&Status::Error("core died".into()), None);
        broken.error_id = Some("a".to_string());

        fold(&mut state, &snapshot(broken, idle()), Instant::now(), true);
        assert_eq!(state.failed_id.as_deref(), Some("a"));

        // The daemon stops reporting the failure: so does the card.
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), idle()),
            Instant::now(),
            true,
        );
        assert_eq!(state.failed_id, None);
    }

    /// A reconnect pins "Connecting…" ahead of a daemon still reporting the
    /// old failure; the card must follow the click, not the lag.
    #[test]
    fn a_retry_clears_the_failure_before_the_daemon_agrees() {
        let mut state = state();
        let now = Instant::now();
        let mut broken = StatusInfo::from_status(&Status::Error("core died".into()), None);
        broken.error_id = Some("a".to_string());
        fold(&mut state, &snapshot(broken.clone(), idle()), now, true);

        state.pin_status("default", Status::Connecting, now);
        state.operation = Some(UiOperation::new(
            super::super::operation::UiOperationKind::Connect,
        ));
        fold(&mut state, &snapshot(broken, idle()), now, true);
        assert_eq!(state.failed_id, None);
    }

    /// A hidden window must not queue toasts it will replay in a batch hours
    /// later, but it still has to remember it saw the failure.
    #[test]
    fn a_hidden_window_records_the_error_without_toasting() {
        let mut state = state();
        let effects = fold(
            &mut state,
            &snapshot(
                StatusInfo::from_status(&Status::Error("core died".into()), None),
                idle(),
            ),
            Instant::now(),
            false,
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::ConnectionError(_)))
        );
        assert_eq!(state.notified_error.as_deref(), Some("core died"));
    }

    #[test]
    fn the_daemons_active_server_wins_when_nothing_is_in_flight() {
        let mut state = state();
        fold(
            &mut state,
            &snapshot(
                StatusInfo::from_status(&Status::Connected, Some("a".into())),
                idle(),
            ),
            Instant::now(),
            true,
        );
        assert_eq!(state.connected_id.as_deref(), Some("a"));
    }

    #[test]
    fn an_in_flight_operation_freezes_the_active_server() {
        let mut state = state();
        state.operation = Some(UiOperation::new(
            super::super::operation::UiOperationKind::Connect,
        ));
        state.connected_id = Some("b".to_string());
        fold(
            &mut state,
            &snapshot(
                StatusInfo::from_status(&Status::Connected, Some("a".into())),
                idle(),
            ),
            Instant::now(),
            true,
        );
        assert_eq!(state.connected_id.as_deref(), Some("b"));
    }

    #[test]
    fn the_connected_servers_reading_is_carried_over_until_a_fresh_one_lands() {
        let mut state = state();
        state.connected_id = Some("a".to_string());
        fold(
            &mut state,
            &snapshot(connected_to("a"), tunnel_probe("a", 41)),
            Instant::now(),
            true,
        );
        assert_eq!(active_latency_for(&state), (Some(41), false));

        state.readings.remove("a");
        assert_eq!(active_latency_for(&state), (Some(41), true));

        // A different server must never inherit the number.
        state.connected_id = Some("b".to_string());
        assert_eq!(active_latency_for(&state), (None, false));
    }

    /// The original dishonesty in one test: a ping taken straight at the
    /// server, before the tunnel existed, is not the tunnel's latency and must
    /// never reach the chip that claims to show it.
    #[test]
    fn a_direct_reading_is_never_the_connected_servers_latency() {
        let mut state = state();
        state.connected_id = Some("a".to_string());
        fold(
            &mut state,
            &snapshot(connected_to("a"), probe(&[], &[("a", Some(41))])),
            Instant::now(),
            true,
        );
        assert_eq!(active_latency_for(&state), (None, false));
        assert_eq!(state.last_active_latency, None);
    }

    /// The number measured through the tunnel is a fact about the connection,
    /// and says so — the card must not present it as the server's own ping.
    #[test]
    fn a_proxied_reading_is_labelled_as_the_tunnels() {
        let mut state = state();
        let effects = fold(
            &mut state,
            &snapshot(connected_to("a"), tunnel_probe("a", 41)),
            Instant::now(),
            true,
        );
        assert_eq!(
            latency(&effects, "a"),
            Some(LatencyState::Tunnel {
                ms: 41,
                age: LatencyAge::Fresh,
                method: LatencyMethod::HttpGet
            })
        );
    }

    /// Disconnecting does not change the reading — it changes what the reading
    /// is *about*. The card has to stop showing it, and say so, without waiting
    /// for a probe nobody asked for.
    #[test]
    fn a_tunnel_reading_is_superseded_when_the_tunnel_moves() {
        let mut state = state();
        fold(
            &mut state,
            &snapshot(connected_to("a"), tunnel_probe("a", 41)),
            Instant::now(),
            true,
        );

        let mut down = tunnel_probe("a", 41);
        down.readings = state.readings.clone();
        let effects = fold(
            &mut state,
            &snapshot(StatusInfo::default(), down),
            Instant::now(),
            true,
        );
        assert_eq!(latency(&effects, "a"), Some(LatencyState::Superseded));
        // ...and exactly one re-check is asked for, not a sweep.
        let reprobed: Vec<&String> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Reprobe(id) => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(reprobed, vec!["a"]);
    }

    /// A number nobody has re-taken in a while is still shown — it is the last
    /// thing known — but it stops claiming to be current.
    #[test]
    fn a_reading_that_stops_being_current_says_how_old_it_is() {
        let mut state = state();
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&[], &[("a", Some(41))])),
            Instant::now(),
            true,
        );
        assert_eq!(state.card_state("a", NOW_MS), fresh(41));
        assert_eq!(
            state.card_state("a", NOW_MS + 59_000),
            fresh(41),
            "under a minute is still fresh"
        );
        assert_eq!(
            state.card_state("a", NOW_MS + 185_000),
            LatencyState::Reachable {
                ms: 41,
                age: LatencyAge::Stale(3),
                method: LatencyMethod::HttpGet
            }
        );
        // A clock that disagrees between the two processes is not an excuse to
        // invent an age.
        assert_eq!(
            state.card_state("a", NOW_MS - 1),
            LatencyState::Reachable {
                ms: 41,
                age: LatencyAge::Unknown,
                method: LatencyMethod::HttpGet
            }
        );
    }

    /// The daemon drops readings for servers it no longer has; a GUI that kept
    /// them would re-attach a number to an id a later refresh reused.
    #[test]
    fn a_reading_the_daemon_forgot_is_forgotten_here_too() {
        let mut state = state();
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&[], &[("a", Some(41))])),
            Instant::now(),
            true,
        );
        assert!(state.readings.contains_key("a"));

        fold(
            &mut state,
            &snapshot(StatusInfo::default(), idle()),
            Instant::now(),
            true,
        );
        assert!(state.readings.is_empty());

        // But an outdated daemon says nothing at all, which is not the same as
        // saying they are gone.
        state.readings.insert("a".to_string(), reading(Some(41)));
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), ProbeState::default()),
            Instant::now(),
            true,
        );
        assert!(state.readings.contains_key("a"));
    }

    fn failed_at(
        failure: ProbeFailure,
        detail: Option<ProbeDetail>,
        route: ProbeRoute,
        method: LatencyMethod,
        measured_at_unix_ms: u64,
    ) -> LatencyReading {
        let mut reading = match detail {
            Some(detail) => LatencyReading::failed_locally(failure, detail, route, method),
            None => LatencyReading::failed(failure, route, method),
        };
        reading.measured_at_unix_ms = measured_at_unix_ms;
        reading
    }

    /// The four facts the card threw away. Every one of them is in the reading
    /// the daemon already sends, and the badge kept none of them.
    #[test]
    fn a_failed_check_is_reported_with_how_it_was_made_and_when() {
        let report = failure_report(
            Some(&failed_at(
                ProbeFailure::Unreachable,
                None,
                ProbeRoute::Direct,
                LatencyMethod::Tcp,
                NOW_MS,
            )),
            NOW_MS,
        )
        .expect("a failure has a report");
        assert_eq!(report.reason, "The server did not answer");
        assert_eq!(report.attempt, "Tried by TCP handshake · just now");
    }

    /// A refusal, a wrong TLS parameter and a dead network all read as "the
    /// server did not answer" on the badge. The detail is the sentence that
    /// tells them apart, and it is what the daemon sent.
    #[test]
    fn a_local_failure_is_reported_by_its_detail_rather_than_by_its_category() {
        let report = failure_report(
            Some(&failed_at(
                ProbeFailure::Unknown,
                Some(ProbeDetail::CertificateRejected),
                ProbeRoute::Direct,
                LatencyMethod::HttpGet,
                NOW_MS,
            )),
            NOW_MS,
        )
        .expect("a failure has a report");
        assert_eq!(report.reason, "The server's certificate was rejected");
    }

    /// The route decides whether the reason is about the server at all: a
    /// refusal measured through a tunnel describes the tunnel.
    #[test]
    fn a_check_made_through_the_tunnel_says_so() {
        let report = failure_report(
            Some(&failed_at(
                ProbeFailure::Timeout,
                None,
                ProbeRoute::Proxied,
                LatencyMethod::HttpHead,
                NOW_MS - 4 * 60_000,
            )),
            NOW_MS,
        )
        .expect("a failure has a report");
        assert_eq!(report.reason, "The check ran out of time");
        assert_eq!(
            report.attempt,
            "Tried through the tunnel by HTTP HEAD · 4 minutes ago"
        );
    }

    #[test]
    fn one_minute_is_singular_and_the_rest_are_not() {
        let ago = |minutes: u64| {
            failure_report(
                Some(&failed_at(
                    ProbeFailure::Unreachable,
                    None,
                    ProbeRoute::Direct,
                    LatencyMethod::Icmp,
                    NOW_MS - minutes * 60_000,
                )),
                NOW_MS,
            )
            .expect("a failure has a report")
            .attempt
        };
        assert!(ago(1).ends_with("· 1 minute ago"), "{}", ago(1));
        assert!(ago(2).ends_with("· 2 minutes ago"), "{}", ago(2));
        assert!(ago(0).ends_with("· just now"), "{}", ago(0));
    }

    /// A daemon that predates the timestamp, or two clocks that disagree.
    /// "Just now" would be the flattering answer rather than the true one.
    #[test]
    fn a_reading_that_cannot_be_dated_is_not_reported_as_fresh() {
        let report = failure_report(
            Some(&failed_at(
                ProbeFailure::Unreachable,
                None,
                ProbeRoute::Direct,
                LatencyMethod::Tcp,
                0,
            )),
            NOW_MS,
        )
        .expect("a failure has a report");
        assert!(
            report.attempt.ends_with("· at an unrecorded time"),
            "{}",
            report.attempt
        );
    }

    /// A number needs no excuse. A card showing one must not also carry the
    /// reason the check before it gave.
    #[test]
    fn a_reading_with_a_number_has_nothing_to_explain() {
        assert_eq!(failure_report(Some(&reading(Some(41))), NOW_MS), None);
        assert_eq!(failure_report(None, NOW_MS), None);
    }

    /// Stopping a check is the one reason here the user chose. It is still
    /// reported — the card owes an answer for having no number — but as what
    /// happened rather than as something that went wrong.
    #[test]
    fn a_check_the_user_stopped_is_reported_as_stopped() {
        let report = failure_report(
            Some(&failed_at(
                ProbeFailure::Unknown,
                Some(ProbeDetail::Cancelled),
                ProbeRoute::Direct,
                LatencyMethod::HttpGet,
                NOW_MS,
            )),
            NOW_MS,
        )
        .expect("a stopped check still has to say why there is no number");
        assert_eq!(report.reason, "The check was stopped before it ran");
    }

    /// Every reason the daemon can send has to stand on its own as a line of
    /// the card, which means none of them may come out empty or lower-case.
    /// The wording lives on the enums so the CLI and the card cannot drift;
    /// this is what proves the promotion to a sentence holds for all of it.
    #[test]
    fn every_reason_the_daemon_can_send_stands_on_its_own() {
        let details = [
            None,
            Some(ProbeDetail::NoCore),
            Some(ProbeDetail::CertificateRejected),
            Some(ProbeDetail::InsecureTlsUnsupported),
            Some(ProbeDetail::ConfigRefused),
            Some(ProbeDetail::GeoAssetsMissing),
            Some(ProbeDetail::Cancelled),
            Some(ProbeDetail::Other),
        ];
        let failures = [
            ProbeFailure::Unreachable,
            ProbeFailure::Timeout,
            ProbeFailure::NoNetwork,
            ProbeFailure::Unknown,
        ];
        for failure in failures {
            for detail in details {
                let report = failure_report(
                    Some(&failed_at(
                        failure,
                        detail,
                        ProbeRoute::Direct,
                        LatencyMethod::Tcp,
                        NOW_MS,
                    )),
                    NOW_MS,
                )
                .expect("every failure has a report");
                let first = report.reason.chars().next().expect("a non-empty reason");
                assert!(
                    first.is_uppercase(),
                    "{failure:?}/{detail:?} reads as a fragment: {}",
                    report.reason
                );
                assert!(!report.attempt.is_empty());
            }
        }
    }

    /// The card's two answers about one check must come from one reading. Two
    /// lookups that could pick differently would let a card show a dash for
    /// this check and the reason from the one before it.
    #[test]
    fn the_reason_on_a_card_is_about_the_reading_its_badge_is_showing() {
        let mut state = state();
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&[], &[("a", None)])),
            Instant::now(),
            true,
        );
        assert_eq!(state.card_state("a", NOW_MS), LatencyState::Unreachable);
        let report = state
            .card_failure("a", NOW_MS)
            .expect("a card with no number owes a reason");
        assert_eq!(report.reason, "The server did not answer");
        assert_eq!(report.attempt, "Tried by HTTP GET · just now");

        // And once a number arrives, the reason goes with the dash.
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&[], &[("a", Some(41))])),
            Instant::now(),
            true,
        );
        assert_eq!(state.card_failure("a", NOW_MS), None);
    }

    /// A check in flight is replacing the measurement the reason describes.
    /// Left standing under a spinner, the reason reads as why the spinner is
    /// spinning.
    #[test]
    fn a_check_in_flight_carries_no_reason_from_the_check_before_it() {
        let mut state = state();
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&[], &[("a", None)])),
            Instant::now(),
            true,
        );
        assert!(state.card_failure("a", NOW_MS).is_some());
        fold(
            &mut state,
            &snapshot(StatusInfo::default(), probe(&["a"], &[("a", None)])),
            Instant::now(),
            true,
        );
        assert_eq!(state.card_state("a", NOW_MS), LatencyState::Checking);
        assert_eq!(state.card_failure("a", NOW_MS), None);
    }

    /// A server nobody has checked has nothing to explain — it is not a
    /// failure, and saying anything here would invent one.
    #[test]
    fn a_server_that_was_never_checked_is_not_reported_as_having_failed() {
        assert_eq!(state().card_failure("a", NOW_MS), None);
    }

    fn versions(daemon: Option<&str>, core: Option<&str>) -> Versions {
        Versions {
            app: "0.2.0".to_string(),
            daemon: daemon.map(str::to_string),
            core: core.map(str::to_string),
            source: Some(DaemonSource::System),
            install: oxidom_core::versions::Install::Package,
            distribution: Some("Fedora Linux 42".to_string()),
            desktop: Some("GNOME, wayland".to_string()),
        }
    }

    /// The dialog carries this window's own version as a property. The two
    /// programs it has no property for are the two a reporter is asked about
    /// and cannot see, so they are the ones this text exists to name.
    #[test]
    fn the_about_summary_names_the_daemon_and_the_core() {
        assert_eq!(
            about_comments(&versions(Some("0.2.0"), Some("Xray 26.3.27"))),
            "Xray client for the GNOME desktop\n\n\
             Daemon 0.2.0 — the system daemon\n\
             Xray 26.3.27"
        );
    }

    /// A window that warns every time it opens is a window whose warnings are
    /// not read, so a daemon of this build gets no sentence at all.
    #[test]
    fn a_daemon_of_this_build_draws_no_warning() {
        let text = about_comments(&versions(Some("0.2.0"), Some("Xray 26.3.27")));
        assert!(!text.contains("older"), "{text}");
        assert!(!text.contains("restarted"), "{text}");
    }

    #[test]
    fn a_daemon_from_another_build_is_named_as_such_at_the_end() {
        let text = about_comments(&versions(Some("0.1.0"), Some("Xray 26.3.27")));
        assert!(
            text.ends_with("Some controls will be missing until it is restarted."),
            "{text}"
        );
        assert!(text.contains("Daemon 0.1.0 — the system daemon"), "{text}");
    }

    /// Both unknowns say what is not known rather than leaving the line out.
    /// A summary with a line missing reads as a defect in the window; one that
    /// says "unknown" is a fact about the machine, and it is the fact the
    /// reporter needs.
    #[test]
    fn nothing_unknown_is_left_out_of_the_about_summary() {
        let text = about_comments(&versions(None, None));
        assert!(
            text.contains("Daemon version unknown — the system daemon"),
            "{text}"
        );
        assert!(text.contains("No Xray core"), "{text}");
        assert!(text.contains("older than this window"), "{text}");
    }

    fn measured_at(ms: u32, method: LatencyMethod, measured_at_unix_ms: u64) -> LatencyReading {
        let mut reading = LatencyReading::ok(ms, ProbeRoute::Direct, method);
        reading.measured_at_unix_ms = measured_at_unix_ms;
        reading
    }

    /// The point of the panel: two servers whose newest number is the same can
    /// have completely different records behind it, and the rows are what say
    /// so. Newest first, because that is the order the daemon keeps and the
    /// order the number above the list belongs to.
    #[test]
    fn the_history_reads_newest_first_with_the_method_and_the_age_of_each_check() {
        let history = ProbeHistory {
            readings: vec![
                measured_at(41, LatencyMethod::HttpGet, NOW_MS),
                measured_at(870, LatencyMethod::Tcp, NOW_MS - 3 * 60_000),
            ],
        };
        let rows = history_rows(&history, NOW_MS);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].value, "41 ms");
        assert_eq!(rows[0].taken, "HTTP GET · just now");
        assert_eq!(rows[1].value, "870 ms");
        assert_eq!(rows[1].taken, "TCP handshake · 3 minutes ago");
    }

    /// A check that ran and failed is part of the record — a server that times
    /// out every other attempt is exactly what this panel exists to expose, and
    /// dropping those rows would make it look steady. The reason is the
    /// daemon's own wording, the same words the block above the list uses.
    #[test]
    fn a_failed_check_keeps_its_place_in_the_history_and_says_why() {
        let history = ProbeHistory {
            readings: vec![failed_at(
                ProbeFailure::Timeout,
                None,
                ProbeRoute::Direct,
                LatencyMethod::HttpGet,
                NOW_MS - 60_000,
            )],
        };
        let rows = history_rows(&history, NOW_MS);
        assert_eq!(rows[0].value, "—", "a failed check still fills its column");
        assert_eq!(
            rows[0].taken,
            "HTTP GET · 1 minute ago · the check ran out of time"
        );
    }

    /// A daemon too old to keep a history answers an empty one, and a server
    /// nobody has checked answers the same. Neither is an error and neither
    /// draws a row; the card falls back to the single reading it already has.
    #[test]
    fn a_daemon_with_no_history_to_give_draws_no_rows() {
        assert!(history_rows(&ProbeHistory::default(), NOW_MS).is_empty());
    }

    /// A reading that cannot be dated says so rather than reading as fresh,
    /// which is the same promise the reason above the list makes — and it is
    /// one function making it, so the two cannot drift apart.
    #[test]
    fn an_undated_reading_in_the_history_is_not_passed_off_as_recent() {
        let history = ProbeHistory {
            readings: vec![measured_at(41, LatencyMethod::HttpGet, 0)],
        };
        let rows = history_rows(&history, NOW_MS);
        assert_eq!(rows[0].taken, "HTTP GET · at an unrecorded time");
    }
}
