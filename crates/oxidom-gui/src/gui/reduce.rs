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

use oxidom_core::ipc::{
    LatencyReading, PROBE_STATE_VERSION, ProbeFailure, ProbeRoute, ProbeState, ProfileEntry,
    SessionInfo, StatusInfo,
};
use oxidom_core::profile::RouteMode;
use oxidom_core::xray::core::Status;

use super::operation::{UiOperation, UiOperationKind};
use super::server_card::{LatencyAge, LatencyState};

/// One round of daemon polling, produced off the main thread.
pub(super) struct PolledSnapshot {
    pub status: StatusInfo,
    pub probe: ProbeState,
    pub logs: Vec<String>,
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
    /// Connected profiles grouped by the server they carry.
    pub connected_profiles: HashMap<String, Vec<String>>,
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
}

impl ProbeWait {
    pub fn new(since: Instant) -> Self {
        Self {
            since,
            acked: false,
        }
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
        let is_active = self
            .route_target
            .as_ref()
            .is_some_and(|target| target.server_id == id);
        let reading = if is_active {
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
        };
        latency_state(
            reading,
            self.checking.contains_key(id),
            is_active,
            now_unix_ms,
        )
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
            if state.notify_probe.remove(id) && reading.value.is_none() {
                if reading.failure == Some(ProbeFailure::NoNetwork) {
                    toast_no_network = true;
                } else {
                    toast_unreachable = true;
                }
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
        if state.notify_probe.remove(&id) && reading.value.is_none() {
            if reading.failure == Some(ProbeFailure::NoNetwork) {
                toast_no_network = true;
            } else {
                toast_unreachable = true;
            }
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
    let finished: Vec<String> = state
        .checking
        .iter()
        .filter(|(id, wait)| {
            if now.duration_since(wait.since) > PROBE_DEADLINE {
                return true;
            }
            if held.contains(*id) {
                return false;
            }
            wait.acked || now.duration_since(wait.since) > PROBE_ACK_GRACE
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in finished {
        state.checking.remove(&id);
        // A reading that never arrived leaves the card unmeasured rather than
        // unreachable: nothing was measured, so nothing may be claimed.
        push_card(&mut effects, state, &id, now_unix_ms);
    }
    // Cards whose reading did not change but whose *meaning* did, because the
    // tunnel moved out from under it.
    for id in rerouted {
        if !state.checking.contains_key(&id) {
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
pub(super) fn connected_profiles(status: &StatusInfo) -> HashMap<String, Vec<String>> {
    let mut by_server = HashMap::<String, Vec<String>>::new();
    for session in &status.sessions {
        if session.state != "connected" {
            continue;
        }
        let Some(server_id) = &session.server_id else {
            continue;
        };
        by_server
            .entry(server_id.clone())
            .or_default()
            .push(session.profile.clone());
    }
    // An older daemon has no session list. Preserve the exact one-tunnel
    // appearance it had before this additive field existed.
    if by_server.is_empty()
        && matches!(status.to_status(), Status::Connected)
        && let Some(server_id) = &status.active_id
    {
        by_server.insert(
            server_id.clone(),
            vec![
                status
                    .active_profile
                    .clone()
                    .unwrap_or_else(|| "default".to_string()),
            ],
        );
    }
    by_server
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionRow {
    /// Profile name; also the row's stable key.
    pub profile: String,
    pub state: SessionRowState,
    /// Server as the user reads it: the session's alias or name while it runs,
    /// the profile's stored handle when it does not.
    pub server: String,
    pub description: String,
    pub chips: Vec<SessionChip>,
    /// Where the toggle sits before the user touches it.
    pub toggle_on: bool,
    /// An operation for this profile is in flight; the row is insensitive.
    pub busy: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionRowState {
    Stopped,
    Connecting,
    Connected,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionChip {
    pub text: String,
    pub kind: SessionChipKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionChipKind {
    Interface,
    Inbound,
    Latency,
    SystemProxy,
    ProxyOnly,
}

fn session_row_state(session: Option<&SessionInfo>) -> SessionRowState {
    match session.map(|session| session.state.as_str()) {
        Some("connected") => SessionRowState::Connected,
        Some("connecting") => SessionRowState::Connecting,
        Some("error") => SessionRowState::Error,
        _ => SessionRowState::Stopped,
    }
}

fn route_mode_label(mode: RouteMode) -> &'static str {
    match mode {
        RouteMode::Manual => "manual",
        RouteMode::List => "list",
        RouteMode::Default => "default",
    }
}

fn latency_chip(reading: Option<&LatencyReading>, now_unix_ms: u64) -> Option<SessionChip> {
    let LatencyState::Tunnel { ms, age, .. } = latency_state(reading, false, true, now_unix_ms)
    else {
        return None;
    };
    let text = match age {
        LatencyAge::Stale(minutes) => format!("{ms} ms · {minutes} min ago"),
        _ => format!("{ms} ms"),
    };
    Some(SessionChip {
        text,
        kind: SessionChipKind::Latency,
    })
}

/// The operation currently running for `profile`, if any.
fn operation_kind_for(state: &SnapshotState, profile: &str) -> Option<UiOperationKind> {
    let operation = state.operation.as_ref()?;
    (operation.profile.as_deref() == Some(profile)).then_some(operation.kind)
}

/// Build every row on the Sessions page without asking a widget to interpret
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
            let mut chips = Vec::new();

            if let Some(interface) = session.and_then(|session| session.interface.as_ref()) {
                let mut text = format!(
                    "{} · {} · {}",
                    interface.device, interface.address, interface.routes
                );
                if !interface.up {
                    text.push_str(" · down");
                }
                chips.push(SessionChip {
                    text,
                    kind: SessionChipKind::Interface,
                });
            } else if session.is_none() && entry.interface.enable {
                let device = if entry.interface.device.is_empty() {
                    oxidom_core::bind::device_name(&entry.name).ok()
                } else {
                    Some(entry.interface.device.clone())
                };
                if let Some(device) = device {
                    chips.push(SessionChip {
                        text: format!("{device} · {}", route_mode_label(entry.interface.routes)),
                        kind: SessionChipKind::Interface,
                    });
                }
            } else if !entry.interface.enable {
                chips.push(SessionChip {
                    text: "proxy only".to_string(),
                    kind: SessionChipKind::ProxyOnly,
                });
            }

            if let Some(session) = session {
                chips.push(SessionChip {
                    text: format!("{}:{}", session.address, session.socks_port),
                    kind: SessionChipKind::Inbound,
                });
            }
            if let Some(chip) = latency_chip(state.proxied.get(&entry.name), now_unix_ms) {
                chips.push(chip);
            }
            if session.is_some_and(|session| session.owns_system_proxy) {
                chips.push(SessionChip {
                    text: "system proxy".to_string(),
                    kind: SessionChipKind::SystemProxy,
                });
            }

            SessionRow {
                profile: entry.name.clone(),
                state: row_state,
                server: session
                    .and_then(|session| {
                        session
                            .server_alias
                            .as_ref()
                            .or(session.server_name.as_ref())
                    })
                    .cloned()
                    .unwrap_or_else(|| entry.server.clone()),
                description: entry.description.clone(),
                chips,
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
    if state.selected_profile == "default" {
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

    let selected = state.selected_profile.clone();
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
    }
}

pub(super) fn other_sessions_message(
    sessions: &[SessionInfo],
    selected_profile: &str,
) -> Option<String> {
    let count = sessions
        .iter()
        .filter(|session| session.profile != selected_profile)
        .count();
    match count {
        0 => None,
        1 => Some("1 more session is running".to_string()),
        count => Some(format!("{count} more sessions are running")),
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
            _ => LatencyState::Unreachable,
        },
    }
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
    use oxidom_core::ipc::{InterfaceInfo, ProbeFailure, ProbeRoute};
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

    fn snapshot(status: StatusInfo, probe: ProbeState) -> PolledSnapshot {
        PolledSnapshot {
            status,
            probe,
            logs: Vec::new(),
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
        assert_eq!(rows[0].server, "shared");
        assert_eq!(rows[0].description, "Office");
        assert!(rows[0].toggle_on);
        assert!(!rows[0].busy);
        assert_eq!(
            rows[0].chips,
            vec![
                SessionChip {
                    text: "oxi-work · 198.18.7.1 · manual".to_string(),
                    kind: SessionChipKind::Interface,
                },
                SessionChip {
                    text: "127.91.37.1:10808".to_string(),
                    kind: SessionChipKind::Inbound,
                },
                SessionChip {
                    text: "71 ms · 3 min ago".to_string(),
                    kind: SessionChipKind::Latency,
                },
                SessionChip {
                    text: "system proxy".to_string(),
                    kind: SessionChipKind::SystemProxy,
                },
            ]
        );
        assert_eq!(rows[1].state, SessionRowState::Connecting);
        assert_eq!(rows[1].server, "Shared server");
        assert_eq!(rows[1].description, "Personal");
        assert!(rows[1].toggle_on);
        assert!(rows[1].busy);
        assert_eq!(
            rows[1].chips,
            vec![
                SessionChip {
                    text: "proxy only".to_string(),
                    kind: SessionChipKind::ProxyOnly,
                },
                SessionChip {
                    text: "127.92.38.1:10808".to_string(),
                    kind: SessionChipKind::Inbound,
                },
                SessionChip {
                    text: "83 ms".to_string(),
                    kind: SessionChipKind::Latency,
                },
            ]
        );
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
            rows[0].chips,
            vec![SessionChip {
                text: "oxi-work · list".to_string(),
                kind: SessionChipKind::Interface,
            }]
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
            }
        );
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
            Some(&vec!["home".to_string(), "work".to_string()])
        );
        assert_eq!(
            other_sessions_message(&status.sessions, "home").as_deref(),
            Some("2 more sessions are running")
        );
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
        assert!(!state.checking.contains_key("a"));
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
    /// say "Failed" instead of falling back to looking merely disconnected.
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
}
