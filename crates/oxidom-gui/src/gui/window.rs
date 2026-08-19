use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use adw::prelude::*;
use anyhow::{Result, anyhow};
use gtk::glib;

use oxidom_core::APP_ID;
use oxidom_core::config::Config;
use oxidom_core::ipc;
use oxidom_core::ipc::ProfileEntry;
use oxidom_core::logbook::{self, LogSlice};
use oxidom_core::model::Subscription;
use oxidom_core::pool::PoolQuery;
use oxidom_core::profile::{Profile, ProfileProxy, ProfileSelect};
use oxidom_core::xray::core::Status;
use oxidom_core::{paths, sysproxy};

use super::logfeed::LogFeed;
use super::operation::{UiOperation, UiOperationKind};
use super::reduce::{
    CardAction, Effect, PolledSnapshot, PoolAction, ProbeWait, SessionRowState, SnapshotState,
    SwitcherItem, active_latency_for, card_action, human_bytes, latency_states,
    missing_core_message, other_profiles_message, pool_action, pool_for_profile, pool_short_label,
    press_stops, reduce, selected_status, session_for, session_rows, switcher_items,
    switcher_visible,
};
use super::server_card::{self, LatencyState};
use super::sidebar::{Page, Sidebar};
use super::tray::{OxidomTray, TrayCommand};
use super::views::logs::LogsView;
use super::views::profile_dialog::{
    ProfileDialog, ProfileDialogCallbacks, server_choices, show_profile_dialog,
};
use super::views::servers::{CardConnection, ServersView};
use super::views::sessions::{SessionCallbacks, SessionsView};
use super::views::settings::{SettingsValues, SettingsView};
use super::views::subscriptions::{Pasted, SubscriptionsView, with_clipboard};
use oxidom_core::client::{ConnectStage, DaemonClient, DaemonSource};

/// The refusal `client_job` hands to a completion handler when another
/// operation is already in flight. It is a state of this window, not a failure
/// of a server or of the daemon, and it is a distinct type so handlers can tell
/// the difference: dressed as a connect failure it reddened a card and blamed a
/// node the daemon was never asked about.
#[derive(Debug)]
struct Busy;

impl std::fmt::Display for Busy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("another operation is still running")
    }
}

impl std::error::Error for Busy {}

fn is_busy(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Busy>().is_some()
}

/// A worker thread ended without sending its result.
///
/// Reported as an ordinary failure of whatever was asked for, so the completion
/// handler that owns the cleanup runs. It says what is and is not true
/// afterwards, because "it failed" and "it may or may not have happened" call
/// for different reactions.
#[derive(Debug)]
struct WorkerLost;

impl std::fmt::Display for WorkerLost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "the background worker stopped without reporting a result — nothing was cancelled, \
             but what is on screen may be out of date",
        )
    }
}

impl std::error::Error for WorkerLost {}

/// What one poll of a worker's channel means.
///
/// Named because the distinction is easy to lose: `try_recv` reports "nothing
/// yet" and "nobody will ever send" as two different errors, and an `else`
/// branch that treats them alike leaves a timer running for the life of the
/// process and the user waiting for an answer that cannot arrive.
enum WorkerPoll<T> {
    Ready(T),
    Waiting,
    Lost,
}

fn poll_worker<T>(receiver: &mpsc::Receiver<T>) -> WorkerPoll<T> {
    match receiver.try_recv() {
        Ok(value) => WorkerPoll::Ready(value),
        Err(mpsc::TryRecvError::Empty) => WorkerPoll::Waiting,
        Err(mpsc::TryRecvError::Disconnected) => WorkerPoll::Lost,
    }
}

/// The single thing a status strip offers to do about the state it reports.
/// Absent for `Disconnected`, and for `Connecting` — which resolves on its own
/// in seconds and has nothing to stop that has started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusAction {
    Disconnect,
    ShowError,
}

fn status_action_for(status: &Status) -> Option<StatusAction> {
    match status {
        Status::Connected => Some(StatusAction::Disconnect),
        Status::Error(_) => Some(StatusAction::ShowError),
        Status::Disconnected | Status::Connecting => None,
    }
}

/// What was last handed to the tray. The poll tick runs twice a second and
/// most ticks change nothing, so the icon is only woken when one of these does.
#[derive(Clone, Default, PartialEq, Eq)]
struct TrayState {
    text: String,
    sessions: Vec<(String, bool)>,
    failed: bool,
}

type SettingsCallback = Rc<dyn Fn(SettingsValues)>;
type ShortcutHandler = Box<dyn Fn(&Rc<Controller>)>;

const SIDEBAR_BREAKPOINT_WIDTH: u32 = 700;

/// Poll ticks between age sweeps — 30 × 500 ms, i.e. every 15 s. A reading's
/// age is bucketed to whole minutes, so this is four chances to notice each
/// bucket change; sweeping on every tick would be pure waste, and a second
/// timer for it would be a second thing to keep in step with the poll.
const AGE_SWEEP_TICKS: u8 = 30;

/// Log records to take from each book per poll.
///
/// A quiet tick returns none of them; the cap only matters after the view has
/// been unable to keep up, and what it drops is reported rather than hidden.
const LOG_FETCH_LIMIT: u32 = 1000;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponsiveMode {
    Wide,
    Compact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusTone {
    Neutral,
    Working,
    Connected,
    Error,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SearchState {
    text: String,
    cursor: i32,
    selection: Option<(i32, i32)>,
}

impl SearchState {
    fn new(text: String, cursor: i32, selection: Option<(i32, i32)>) -> Self {
        let length = i32::try_from(text.chars().count()).unwrap_or(i32::MAX);
        let cursor = cursor.clamp(0, length);
        let selection = selection
            .map(|(start, end)| (start.clamp(0, length), end.clamp(0, length)))
            .filter(|(start, end)| start != end);
        Self {
            text,
            cursor,
            selection,
        }
    }

    fn capture(entry: &gtk::SearchEntry) -> Self {
        Self::new(
            entry.text().to_string(),
            entry.position(),
            entry.selection_bounds(),
        )
    }

    fn restore(&self, entry: &gtk::SearchEntry) {
        if entry.text().as_str() != self.text {
            entry.set_text(&self.text);
        }
        if let Some((start, end)) = self.selection {
            entry.select_region(start, end);
        } else {
            entry.set_position(self.cursor);
        }
    }
}

#[cfg(test)]
fn responsive_mode_for_width(width: f64) -> ResponsiveMode {
    if width <= f64::from(SIDEBAR_BREAKPOINT_WIDTH) {
        ResponsiveMode::Compact
    } else {
        ResponsiveMode::Wide
    }
}

/// Width the servers view actually gets: in compact mode the sidebar overlays
/// the content, in wide mode OverlaySplitView carves out its fraction
/// (25% clamped to the configured 230..=280 range).
fn servers_available_width(window_width: i32, compact: bool) -> i32 {
    if compact {
        return window_width;
    }
    let sidebar = (window_width / 4).clamp(230, 280);
    window_width.saturating_sub(sidebar)
}

struct AppState {
    client: DaemonClient,
    subscriptions: Vec<Subscription>,
    profiles: Vec<ProfileEntry>,
    /// Card the user is inspecting/expanded. Also the target of the header
    /// Connect button. Distinct from the server that is actually connected.
    selected_id: Option<String>,
    /// Everything the poll snapshot owns, kept apart so the transition over it
    /// can be reviewed and tested without a display. See [`super::reduce`].
    ui: SnapshotState,
}

struct Controller {
    window: adw::ApplicationWindow,
    state: Rc<RefCell<AppState>>,
    /// The daemon's last answer about itself. Held here as well as pushed into
    /// Settings because the geo dialogs quote the daemon's own asset directory,
    /// and guessing one on a system daemon would name the wrong machine's path.
    runtime: RefCell<Option<ipc::RuntimeInfo>>,
    /// Whether a download is being followed, so two clicks cannot start two
    /// timers reading the same state.
    geo_polling: Cell<bool>,
    split: adw::OverlaySplitView,
    header: adw::HeaderBar,
    stack: gtk::Stack,
    search: gtk::SearchEntry,
    compact_search: gtk::SearchEntry,
    search_bar: gtk::SearchBar,
    profiles_banner: adw::Banner,
    /// Shown while the daemon cannot resolve an Xray core.
    core_banner: adw::Banner,
    search_toggle: gtk::ToggleButton,
    sidebar_toggle: gtk::Button,
    header_status: gtk::Button,
    header_status_icon: gtk::Image,
    header_status_flag: gtk::Box,
    header_status_label: gtk::Label,
    header_status_spinner: gtk::Spinner,
    header_status_action: gtk::Button,
    header_status_action_icon: gtk::Image,
    profile_switcher: gtk::MenuButton,
    profile_switcher_label: gtk::Label,
    profile_switcher_popover: gtk::Popover,
    profile_switcher_list: gtk::ListBox,
    /// Items the popover's rows were built from, so a poll that changed
    /// nothing does not tear the list down and put it back.
    profile_switcher_shown: RefCell<Vec<SwitcherItem>>,
    profile_actions: gtk::Box,
    subscription_actions: gtk::Box,
    settings_actions: gtk::Box,
    compact: Rc<Cell<bool>>,
    search_state: RefCell<SearchState>,
    syncing_search: Cell<bool>,
    sidebar_status: gtk::Button,
    sidebar_status_icon: gtk::Image,
    sidebar_status_label: gtk::Label,
    sidebar_status_spinner: gtk::Spinner,
    sidebar_status_action: gtk::Button,
    sidebar_status_action_icon: gtk::Image,
    sidebar_list: gtk::ListBox,
    servers: ServersView,
    sessions: SessionsView,
    subscriptions: SubscriptionsView,
    settings: SettingsView,
    logs: LogsView,
    toasts: adw::ToastOverlay,
    close_after_apply: Cell<bool>,
    /// True when the pending close came from Quit rather than the window
    /// button, so finishing it must end the process instead of hiding.
    quit_after_close: Cell<bool>,
    tray: RefCell<Option<ksni::blocking::Handle<OxidomTray>>>,
    tray_commands: mpsc::Receiver<TrayCommand>,
    /// Last state pushed to the tray, to skip no-op updates.
    tray_pushed: RefCell<TrayState>,
    /// True while this GUI holds the GNOME system proxy applied.
    proxy_applied: Cell<bool>,
    /// Endpoint the applied GNOME proxy points at. `None` with
    /// `proxy_applied == true` means it came from a crash marker and must be
    /// reconciled even if a connection is already up.
    applied_proxy_endpoint: Cell<Option<(std::net::Ipv4Addr, u16, u16)>>,
    /// Why the desktop proxy could not be installed, as last reported.
    /// `reconcile_system_proxy` runs on every poll tick, so the message is kept
    /// here to say it once instead of twice a second.
    system_proxy_failure: RefCell<Option<String>>,
    /// What the status action button currently promises, so pressing it does
    /// what its icon said rather than what the state has since become.
    status_action: Cell<Option<StatusAction>>,
    /// Last (active, connecting) pair pushed to the cards, to avoid an
    /// O(cards) pass on every poll tick.
    applied_connection: RefCell<CardConnection>,
    poll_in_flight: Arc<AtomicBool>,
    poll_snapshot: Arc<Mutex<Option<PolledSnapshot>>>,
    /// Poll ticks since the last age sweep. See [`AGE_SWEEP_TICKS`].
    sweep_tick: Cell<u8>,
    /// Merges the daemon's log with this process's own, and remembers how far
    /// each has been read. See [`LogFeed`].
    log_feed: RefCell<LogFeed>,
}

/// How often the main loop looks in on the connection thread.
const STARTUP_POLL: Duration = Duration::from_millis(60);

/// The small window shown while the daemon is being reached. It exists because
/// the connection is not instant — an installed system daemon is waited for,
/// and D-Bus activation may have to start it — and a launcher click that
/// produces nothing at all for several seconds reads as an application that
/// hung, not as one that is working.
struct Splash {
    window: adw::ApplicationWindow,
    stage: gtk::Label,
    /// Kept so [`Splash::dismiss`] can take the handler off before closing.
    /// Closing a window emits `close-request` exactly as clicking its × does,
    /// and this handler quits the application — so a splash dismissed by
    /// success would take the process down with it, a second before the window
    /// it was waiting for could appear.
    close_handler: glib::SignalHandlerId,
}

impl Splash {
    fn new(app: &adw::Application, cancelled: Rc<Cell<bool>>) -> Self {
        let spinner = gtk::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .build();
        spinner.set_spinning(true);
        let title = gtk::Label::builder()
            .label("oxidom")
            .css_classes(["title-2"])
            .build();
        let stage = gtk::Label::builder()
            .label(stage_text(ConnectStage::System))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_valign(gtk::Align::Center);
        content.set_vexpand(true);
        content.set_margin_start(24);
        content.set_margin_end(24);
        content.set_margin_bottom(24);
        content.append(&spinner);
        content.append(&title);
        content.append(&stage);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::builder().css_classes(["flat"]).build());
        view.set_content(Some(&content));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("oxidom")
            .default_width(340)
            .default_height(220)
            .resizable(false)
            .content(&view)
            .build();
        set_window_icon(&window);
        // Closing the splash by hand is the only way to abandon a connection
        // that is taking too long, and there is no window behind it to fall
        // back to, so it means quit.
        let close_handler = window.connect_close_request({
            let app = app.clone();
            move |_| {
                cancelled.set(true);
                app.quit();
                glib::Propagation::Proceed
            }
        });
        window.present();
        Splash {
            window,
            stage,
            close_handler,
        }
    }

    fn set_stage(&self, stage: ConnectStage) {
        self.stage.set_label(stage_text(stage));
    }

    /// Take the splash down because the connection finished, not because the
    /// user gave up on it.
    fn dismiss(self) {
        self.window.disconnect(self.close_handler);
        self.window.close();
    }
}

fn stage_text(stage: ConnectStage) -> &'static str {
    match stage {
        ConnectStage::System => "Reaching the oxidom daemon…",
        ConnectStage::WaitingForSystem => "Waiting for the system daemon to start…",
        ConnectStage::Session => "Looking for a daemon in this session…",
        ConnectStage::Starting => "Starting a local daemon…",
    }
}

/// Reach the daemon off the main loop, then build the window with it.
///
/// `on_ready` receives the window, or `None` when the daemon could not be
/// reached at all (the error is already on screen by then). Splitting this out
/// of [`build`] is what lets the connection take its time: it may wait out a
/// system daemon that is still starting, and the main loop has to keep running
/// so the splash can be drawn while it does.
pub fn start(
    app: &adw::Application,
    background: bool,
    on_ready: impl Fn(Option<adw::ApplicationWindow>) + 'static,
) {
    install_css();
    #[cfg(debug_assertions)]
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display)
            .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data"));
    }
    gtk::Window::set_default_icon_name(APP_ID);

    // Taken before anything is shown: GTK gives the id to the first window
    // that maps and forgets it, and the first window here is a splash that
    // does not survive. The real window claims it again below.
    let startup_id = gtk::gdk::Display::default().and_then(|display| {
        display
            .startup_notification_id()
            .map(|id| id.as_str().to_string())
    });

    // `--background` shows nothing by definition, so it gets no splash either;
    // its progress goes to the log.
    let cancelled = Rc::new(Cell::new(false));
    // In a cell because dismissing the splash consumes it, and the closure
    // below outlives the tick that does so.
    let splash = RefCell::new((!background).then(|| Splash::new(app, cancelled.clone())));

    let (stage_sender, stage_receiver) = mpsc::channel();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let client = DaemonClient::connect_any(|stage| {
            log::info!("daemon connection: {}", stage_text(stage));
            let _ = stage_sender.send(stage);
        });
        let _ = sender.send(client.map_err(|error| format!("{error:#}")));
    });

    let app = app.clone();
    let on_ready = Rc::new(on_ready);
    let mut startup_id = startup_id;
    glib::timeout_add_local(STARTUP_POLL, move || {
        if cancelled.get() {
            return glib::ControlFlow::Break;
        }
        while let Ok(stage) = stage_receiver.try_recv() {
            if let Some(splash) = splash.borrow().as_ref() {
                splash.set_stage(stage);
            }
        }
        let outcome = match receiver.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Ok(outcome) => outcome,
            // The thread died without answering; treat it as a failure rather
            // than polling a channel nobody will ever write to again.
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("the daemon connection ended without an answer".to_string())
            }
        };
        match outcome {
            Ok(client) => {
                // Built and presented *before* the splash comes down. Taking
                // the splash away first leaves the application with no window
                // at all for a moment, and a desktop that follows a launch by
                // watching for its window sees the launch abandoned rather
                // than finished — which is a busy cursor that never clears.
                let window = build(&app, background, client, startup_id.take());
                if let Some(splash) = splash.borrow_mut().take() {
                    splash.dismiss();
                }
                on_ready(Some(window));
            }
            Err(message) => {
                if let Some(splash) = splash.borrow_mut().take() {
                    splash.dismiss();
                }
                show_daemon_error(&app, &message);
                on_ready(None);
            }
        }
        glib::ControlFlow::Break
    });
}

/// The one dialog that is still an `AdwMessageDialog`, and has to be.
///
/// This runs when the daemon could not be reached, which is *before* there is a
/// window. `AdwDialog` is not a toplevel — it is presented into a widget, and
/// presenting one with no parent shows nothing at all, so the user would be left
/// with a held process, no window and no way to answer. `AdwMessageDialog` is a
/// `GtkWindow` and can stand on its own. Every other dialog in the app moved.
#[allow(deprecated)]
fn show_daemon_error(app: &adw::Application, message: &str) {
    let dialog = adw::MessageDialog::new(
        None::<&gtk::Window>,
        Some("oxidom daemon unavailable"),
        Some(message),
    );
    dialog.add_responses(&[("quit", "Quit"), ("retry", "Try again")]);
    dialog.set_response_appearance("retry", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("retry"));
    // Escape must quit, not dismiss: the hold guard is already taken in
    // `gui::run`, so a dismissed dialog would otherwise leave a held process
    // with no window and no tray to close it with.
    dialog.set_close_response("quit");
    dialog.connect_response(None, {
        let app = app.clone();
        move |dialog, response| {
            dialog.close();
            if response == "retry" {
                app.activate();
            } else {
                app.quit();
            }
        }
    });
    dialog.present();
}

fn refresh_profiles_after<R>(client: &DaemonClient, result: R) -> (R, Vec<ProfileEntry>) {
    // A failed refresh must not mask the outcome the user asked about: the
    // list is a redraw, the operation is the answer.
    (result, client.list_profiles().unwrap_or_default())
}

fn build(
    app: &adw::Application,
    background: bool,
    client: DaemonClient,
    startup_id: Option<String>,
) -> adw::ApplicationWindow {
    if client.source() != DaemonSource::System {
        log::info!(
            "driving a session daemon ({:?}); its subscriptions are stored per-user",
            client.source()
        );
    }
    let subscriptions_snapshot = client.subscriptions().unwrap_or_default();
    let profiles_snapshot = client.list_profiles().unwrap_or_default();
    let initial_status = client.status().unwrap_or_default();
    let initial_config = client.settings().unwrap_or_default();
    // A daemon older than RuntimeInfo answers UnknownMethod; `None` just
    // leaves the settings rows unlocked and the effective path unknown.
    let initial_runtime = client.runtime_info().ok();
    // Asked once, here: whether this daemon can install geo data, and which
    // daemon it is. Together they decide whether the Settings page may offer a
    // button at all — a system service too old to know the method cannot be
    // helped by anything this process downloads into the user's home.
    let daemon_source = client.source();
    let geo_download_supported = client.supports_geo_download();
    // Asked once, here, because the answer decides whether the latency controls
    // may offer a stop at all. A session daemon left running from an older
    // version is the realistic case: the packages pin the daemon to the same
    // version as the interface, but a running process is not upgraded by an
    // install.
    let probe_cancel_supported = client.supports_probe_cancel();
    let selected_id = initial_status.active_id.clone();
    let servers = ServersView::new(&subscriptions_snapshot);
    let state = Rc::new(RefCell::new(AppState {
        client,
        subscriptions: subscriptions_snapshot,
        profiles: profiles_snapshot,
        selected_id,
        ui: SnapshotState::new(&initial_status),
    }));

    let sessions = SessionsView::new();
    sessions.set_header_actions_embedded(false);
    let profile_actions = sessions.header_actions();
    profile_actions.set_visible(false);
    let subscriptions = SubscriptionsView::new();
    subscriptions.set_header_actions_embedded(false);
    let subscription_actions = subscriptions.header_actions();
    subscription_actions.set_visible(false);
    let logs = LogsView::new();
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .hhomogeneous(false)
        .vexpand(true)
        .hexpand(true)
        .build();
    stack.add_named(&servers.root, Some(Page::Servers.stack_name()));
    stack.add_named(&sessions.root, Some(Page::Profiles.stack_name()));
    stack.add_named(&subscriptions.root, Some(Page::Subscriptions.stack_name()));

    let settings_callback: Rc<RefCell<Option<SettingsCallback>>> = Rc::new(RefCell::new(None));
    let settings = SettingsView::new(&initial_config, {
        let callback = settings_callback.clone();
        move |values| {
            if let Some(callback) = callback.borrow().as_ref() {
                callback(values);
            }
        }
    });
    settings.set_daemon_capabilities(daemon_source, geo_download_supported);
    servers.set_probe_cancel_supported(probe_cancel_supported);
    settings.set_runtime_info(initial_runtime.as_ref());

    // Apply the saved scheme before anything is on screen, so a window pinned
    // to light does not flash the desktop's dark one on the way up.
    let gui_prefs = servers.prefs();
    let saved_scheme = gui_prefs.borrow().color_scheme;
    adw::StyleManager::default().set_color_scheme(saved_scheme.to_adw());
    settings.set_color_scheme(saved_scheme);
    settings.connect_color_scheme_changed({
        let gui_prefs = gui_prefs.clone();
        move |scheme| {
            adw::StyleManager::default().set_color_scheme(scheme.to_adw());
            let mut prefs = gui_prefs.borrow_mut();
            prefs.color_scheme = scheme;
            if let Err(error) = prefs.save() {
                log::warn!("could not save the colour scheme: {error:#}");
            }
        }
    });
    let settings_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    settings_actions.append(&settings.reset_button());
    settings_actions.append(&settings.apply_button());
    settings_actions.set_visible(false);
    stack.add_named(&settings.root, Some(Page::Settings.stack_name()));
    stack.add_named(&logs.root, Some(Page::Logs.stack_name()));

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search")
        .max_width_chars(24)
        .css_classes(["compact-search"])
        .build();
    let compact_search = gtk::SearchEntry::builder()
        .placeholder_text("Search")
        .max_width_chars(1)
        .hexpand(true)
        .css_classes(["compact-search"])
        .build();
    let search_bar = gtk::SearchBar::builder().show_close_button(true).build();
    search_bar.connect_entry(&compact_search);
    search_bar.set_child(Some(&compact_search));
    let profiles_banner = adw::Banner::builder()
        .title(
            other_profiles_message(&initial_status.sessions, "default")
                .as_deref()
                .unwrap_or_default(),
        )
        .revealed(other_profiles_message(&initial_status.sessions, "default").is_some())
        .build();
    profiles_banner.set_button_label(Some("Profiles"));
    // A missing core is not a passing condition — nothing connects and nothing
    // is measured until it is fixed — but its only other symptom is a field
    // partway down Settings that nobody visits until something already went
    // wrong.
    let core_banner = adw::Banner::builder()
        .title(
            missing_core_message(initial_runtime.as_ref())
                .as_deref()
                .unwrap_or_default(),
        )
        .revealed(missing_core_message(initial_runtime.as_ref()).is_some())
        .build();
    core_banner.set_button_label(Some("Settings"));
    let search_toggle = gtk::ToggleButton::builder()
        .icon_name("edit-find-symbolic")
        .tooltip_text("Search servers")
        .visible(false)
        .css_classes(["flat", "header-icon-button"])
        .build();
    search_toggle.update_property(&[gtk::accessible::Property::Label("Search servers")]);
    let sidebar_toggle = gtk::Button::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Show sidebar")
        .visible(false)
        .css_classes(["flat", "header-icon-button"])
        .build();
    sidebar_toggle.update_property(&[gtk::accessible::Property::Label("Show sidebar")]);
    let header_status_spinner = gtk::Spinner::new();
    let header_status_icon = gtk::Image::builder()
        .icon_name("network-vpn-symbolic")
        .visible(false)
        .css_classes(["header-status-icon"])
        .build();
    let header_status_flag = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    let header_status_label = gtk::Label::builder()
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(18)
        .xalign(0.0)
        .build();
    let header_status_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    header_status_content.append(&header_status_spinner);
    header_status_content.append(&header_status_flag);
    header_status_content.append(&header_status_icon);
    header_status_content.append(&header_status_label);
    let header_status = gtk::Button::builder()
        .child(&header_status_content)
        .tooltip_text("Show connections")
        .visible(false)
        .css_classes(["header-status"])
        .build();
    header_status.update_property(&[gtk::accessible::Property::Label("Show connections")]);
    // The compact half of the sidebar's status strip, and split the same way:
    // the chip reports, this acts. Exactly one of the two strips is on screen
    // at a time, so the two never disagree about where Disconnect lives.
    let header_status_action_icon = gtk::Image::builder().pixel_size(16).build();
    let header_status_action = gtk::Button::builder()
        .child(&header_status_action_icon)
        .visible(false)
        .css_classes(["flat", "header-icon-button"])
        .build();

    let profile_switcher_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .activate_on_single_click(true)
        .css_classes(["profile-switcher-list"])
        .build();
    let profile_switcher_popover = gtk::Popover::builder()
        .child(&profile_switcher_list)
        .build();
    // A `MenuButton` label is a plain non-ellipsizing GtkLabel, so a profile
    // named anything long became the window's minimum width and the header
    // simply refused to shrink past it. An explicit child gives the name an
    // ellipsis and a ceiling, and the chevron the button would have drawn
    // itself is drawn here instead.
    let profile_switcher_label = gtk::Label::builder()
        .label("default")
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(12)
        .xalign(0.0)
        .build();
    let profile_switcher_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    profile_switcher_content.append(&profile_switcher_label);
    profile_switcher_content.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
    let profile_switcher = gtk::MenuButton::builder()
        .tooltip_text("Choose profile")
        .visible(false)
        .css_classes(["profile-switcher"])
        .build();
    profile_switcher.set_child(Some(&profile_switcher_content));
    profile_switcher.set_popover(Some(&profile_switcher_popover));
    profile_switcher.update_property(&[gtk::accessible::Property::Label(
        "Choose connection profile",
    )]);

    let header = adw::HeaderBar::new();
    header.pack_start(&sidebar_toggle);
    header.pack_start(&search_toggle);
    header.pack_start(&header_status);
    header.pack_start(&header_status_action);
    header.pack_start(&profile_switcher);
    header.pack_start(&search);
    // The filter used to be packed here, sixth in a row of six, on the theory
    // that it belonged beside the search because both are "find the servers I
    // mean". It cost nothing vertically and nobody found it. It now lives at
    // the head of the chip row, where the scopes it builds are.
    header.pack_end(&profile_actions);
    header.pack_end(&subscription_actions);
    header.pack_end(&settings_actions);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&search_bar);
    content.append(&core_banner);
    content.append(&profiles_banner);
    content.append(&stack);

    let controller_holder = Rc::new(RefCell::new(std::rc::Weak::<Controller>::new()));
    let controller_for_sidebar = controller_holder.clone();
    let sidebar = Sidebar::new(move |page| {
        if let Some(controller) = controller_for_sidebar.borrow().upgrade() {
            controller.show_page(page);
        }
    });

    let split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar.root)
        .content(&content)
        .collapsed(false)
        .pin_sidebar(true)
        .show_sidebar(true)
        .min_sidebar_width(230.0)
        .max_sidebar_width(280.0)
        .build();

    let (tray_sender, tray_commands) = mpsc::channel();
    let tray_handle = {
        use ksni::blocking::TrayMethods;
        let tray = OxidomTray {
            sessions: Vec::new(),
            status_text: "Disconnected".to_string(),
            failed: false,
            commands: tray_sender,
        };
        match tray.spawn() {
            Ok(handle) => Some(handle),
            Err(error) => {
                log::warn!("no StatusNotifier tray available: {error}");
                None
            }
        }
    };

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("oxidom")
        .icon_name(APP_ID)
        .default_width(1100)
        .default_height(720)
        .content(&toasts)
        .build();
    set_window_icon(&window);
    drop_focus_on_outside_click(&window);

    let controller = Rc::new(Controller {
        runtime: RefCell::new(initial_runtime.clone()),
        geo_polling: Cell::new(false),
        window: window.clone(),
        state,
        split,
        header,
        stack,
        search,
        compact_search,
        search_bar,
        profiles_banner,
        core_banner,
        search_toggle,
        sidebar_toggle,
        header_status,
        header_status_icon,
        header_status_flag,
        header_status_label,
        header_status_spinner,
        header_status_action,
        header_status_action_icon,
        profile_switcher,
        profile_switcher_label,
        profile_switcher_popover,
        profile_switcher_list,
        profile_switcher_shown: RefCell::new(Vec::new()),
        profile_actions,
        subscription_actions,
        settings_actions,
        compact: Rc::new(Cell::new(false)),
        search_state: RefCell::new(SearchState::default()),
        syncing_search: Cell::new(false),
        sidebar_status: sidebar.status_button,
        sidebar_status_icon: sidebar.status_icon,
        sidebar_status_label: sidebar.status_label,
        sidebar_status_spinner: sidebar.status_spinner,
        sidebar_status_action: sidebar.status_action,
        sidebar_status_action_icon: sidebar.status_action_icon,
        sidebar_list: sidebar.list,
        servers,
        sessions,
        subscriptions,
        settings,
        logs,
        toasts,
        close_after_apply: Cell::new(false),
        quit_after_close: Cell::new(false),
        tray: RefCell::new(tray_handle),
        tray_commands,
        tray_pushed: RefCell::new(TrayState::default()),
        proxy_applied: Cell::new(gui_proxy_marker_exists()),
        system_proxy_failure: RefCell::new(None),
        status_action: Cell::new(None),
        applied_proxy_endpoint: Cell::new(None),
        applied_connection: RefCell::new(CardConnection::default()),
        poll_in_flight: Arc::new(AtomicBool::new(false)),
        sweep_tick: Cell::new(0),
        poll_snapshot: Arc::new(Mutex::new(None)),
        log_feed: RefCell::new(LogFeed::new()),
    });
    *controller_holder.borrow_mut() = Rc::downgrade(&controller);

    *settings_callback.borrow_mut() = Some({
        let weak = Rc::downgrade(&controller);
        Rc::new(move |values| {
            if let Some(controller) = weak.upgrade() {
                controller.save_settings(values);
            }
        })
    });
    controller.wire_actions();
    // The daemon outlives this window, so a download started before it opened
    // -- or by another client -- is already running. Pick it up rather than
    // showing a stale offer to start one.
    if initial_runtime
        .as_ref()
        .is_some_and(|runtime| runtime.geo.downloading)
    {
        controller.poll_geo_progress();
    }
    controller.rebuild_views();
    controller.refresh_status();
    controller.add_breakpoint();
    controller.start_timer();
    controller.watch_termination();

    // Column count follows the window width (see push_servers_width).
    window.connect_realize({
        let weak = Rc::downgrade(&controller);
        move |window| {
            let Some(controller) = weak.upgrade() else {
                return;
            };
            if let Some(surface) = window.surface() {
                surface.connect_width_notify({
                    let weak = weak.clone();
                    move |surface| {
                        if let Some(controller) = weak.upgrade() {
                            controller.push_servers_width_from(surface.width());
                        }
                    }
                });
            }
            controller.push_servers_width();
        }
    });

    if !background {
        // The window the launcher is waiting for is not the first one this
        // process shows. GTK hands the startup id to whatever maps first —
        // here the splash, which is then destroyed — and the desktop is left
        // with a launch that never finished and a busy cursor to match.
        // Adopting the id here has to happen before the window maps; after
        // that it is ignored.
        if let Some(startup_id) = &startup_id {
            window.set_startup_id(startup_id);
        }
        window.present();
    } else if let Some(startup_id) = &startup_id {
        // Nothing will ever map, so nothing would ever end the sequence.
        if let Some(display) = gtk::gdk::Display::default() {
            display.notify_startup_complete(startup_id);
        }
    }

    // Repair a system proxy left over from a previous GUI run and reflect
    // the daemon's current connection on the cards.
    controller.reconcile_system_proxy();
    controller.sync_connection_cards();
    window
}

/// Let a click anywhere outside a text field give up that field's focus.
///
/// GTK keeps an entry focused until something else takes focus, and most of
/// this window is buttons and cards, which take none. So a search box or an
/// alias field stayed lit and kept swallowing the keyboard long after the user
/// had moved on, with no way back other than Escape or Tab.
///
/// Runs in the capture phase and never claims the sequence, so the widget that
/// was actually clicked still gets the same click it always did. Only text
/// widgets are dropped — anything else keeps its focus, or keyboard navigation
/// would be unusable.
fn drop_focus_on_outside_click(window: &adw::ApplicationWindow) {
    let click = gtk::GestureClick::new();
    click.set_button(0);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed({
        let window = window.downgrade();
        move |_, _, x, y| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let Some(focused) = gtk::prelude::GtkWindowExt::focus(&window) else {
                return;
            };
            // `GtkEditable` covers the whole family in one test — GtkText, the
            // entries that delegate to it, and libadwaita's `AdwEntryRow`.
            if !focused.is::<gtk::Editable>() && !focused.is::<gtk::TextView>() {
                return;
            }
            // A click on the entry itself arrives as its inner `GtkText`, and a
            // click on the frame around it as the `GtkEntry` that owns it —
            // hence both directions of the ancestry test.
            let inside = window
                .pick(x, y, gtk::PickFlags::DEFAULT)
                .is_some_and(|hit| {
                    hit == focused || hit.is_ancestor(&focused) || focused.is_ancestor(&hit)
                });
            if !inside {
                gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
            }
        }
    });
    window.add_controller(click);
}

fn set_window_icon(window: &adw::ApplicationWindow) {
    let icon = include_bytes!("../../../../data/dev.keepinfov.oxidom.svg");
    let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(icon.as_slice()))
    else {
        log::warn!("could not decode the window icon");
        return;
    };
    let textures = [16, 24, 32, 48, 64, 128]
        .into_iter()
        .filter_map(|size| {
            pixbuf
                .scale_simple(size, size, gtk::gdk_pixbuf::InterpType::Bilinear)
                .map(|scaled| gtk::gdk::Texture::for_pixbuf(&scaled))
        })
        .collect::<Vec<_>>();

    if textures.is_empty() {
        log::warn!("could not scale the window icon");
        return;
    }

    window.connect_realize(move |window| {
        let Some(surface) = window.surface() else {
            return;
        };
        let Ok(toplevel) = surface.downcast::<gtk::gdk::Toplevel>() else {
            return;
        };
        toplevel.set_icon_list(&textures);
    });
}

/// What trusting a certificate was for. Trusting is never the goal in itself:
/// it is asked for because something failed, and that something is what should
/// happen next.
#[derive(Debug, Clone)]
enum AfterTrust {
    /// The connection that failed, tried again.
    Reconnect(String),
    /// Nothing was connecting; a fresh reading replaces the failed one.
    Measure,
}

impl Controller {
    /// Keyboard access to everything reachable from the header and sidebar.
    /// GTK needs a `GActionMap` on the window plus app-level accelerators;
    /// neither existed before, so every shortcut lives here.
    fn install_shortcuts(self: &Rc<Self>) {
        let Some(app) = self.window.application() else {
            return;
        };
        let actions = gtk::gio::SimpleActionGroup::new();

        let add = |name: &str, accels: &[&str], handler: ShortcutHandler| {
            let action = gtk::gio::SimpleAction::new(name, None);
            action.connect_activate({
                let weak = Rc::downgrade(self);
                move |_, _| {
                    if let Some(controller) = weak.upgrade() {
                        handler(&controller);
                    }
                }
            });
            actions.add_action(&action);
            app.set_accels_for_action(&format!("win.{name}"), accels);
        };

        add(
            "search",
            &["<Control>f"],
            Box::new(|controller| controller.focus_search()),
        );
        add(
            "refresh",
            &["<Control>r", "F5"],
            Box::new(|controller| controller.refresh_all_subscriptions()),
        );
        add(
            "paste",
            &["<Control>v"],
            Box::new(|controller| controller.import_from_clipboard()),
        );
        add(
            "quit",
            &["<Control>q"],
            Box::new(|controller| controller.request_quit()),
        );
        add(
            "close",
            &["<Control>w"],
            Box::new(|controller| {
                controller.window.close();
            }),
        );
        for (index, page) in [
            Page::Servers,
            Page::Profiles,
            Page::Subscriptions,
            Page::Settings,
            Page::Logs,
        ]
        .into_iter()
        .enumerate()
        {
            add(
                &format!("page{}", index + 1),
                &[&format!("<Control>{}", index + 1)],
                Box::new(move |controller| controller.navigate_to(page)),
            );
        }

        self.window.insert_action_group("win", Some(&actions));
    }

    /// Ctrl+V anywhere but a text field: take whatever is on the clipboard and
    /// open the dialog it belongs to, filled in.
    ///
    /// A subscription link is copied from a browser or a chat and then pasted;
    /// the app asking the user to first find the right dialog, and paste there,
    /// is the step being removed. An entry with focus keeps its own Ctrl+V —
    /// GTK gives the focused widget the key first, and this action never sees
    /// it.
    fn import_from_clipboard(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        with_clipboard(&self.window, move |pasted| {
            let Some(controller) = weak.upgrade() else {
                return;
            };
            if controller.subscriptions.open_for_pasted(pasted) == Pasted::Nothing {
                controller.show_message("Nothing on the clipboard to import");
            }
        });
    }

    /// Puts the cursor in whichever search entry the current layout uses.
    fn focus_search(self: &Rc<Self>) {
        self.navigate_to(Page::Servers);
        if self.compact.get() {
            self.search_toggle.set_active(true);
            let search = self.compact_search.clone();
            glib::idle_add_local_once(move || {
                search.grab_focus();
            });
        } else {
            self.search.grab_focus();
        }
    }

    fn wire_actions(self: &Rc<Self>) {
        self.install_shortcuts();
        self.settings.geo_install_button().connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.present_geo_install();
                }
            }
        });
        self.settings.geo_cancel_button().connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.cancel_geo_download();
                }
            }
        });
        self.servers.connect_browse_subscriptions({
            let weak = Rc::downgrade(self);
            move || {
                if let Some(controller) = weak.upgrade() {
                    controller.navigate_to(Page::Subscriptions);
                }
            }
        });
        self.search.connect_search_changed({
            let weak = Rc::downgrade(self);
            move |entry| {
                if let Some(controller) = weak.upgrade() {
                    controller.handle_search_changed(entry);
                }
            }
        });
        self.compact_search.connect_search_changed({
            let weak = Rc::downgrade(self);
            move |entry| {
                if let Some(controller) = weak.upgrade() {
                    controller.handle_search_changed(entry);
                }
            }
        });
        self.search_toggle.connect_toggled({
            let weak = Rc::downgrade(self);
            move |toggle| {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                if !controller.compact.get() || !controller.is_servers_page() {
                    return;
                }
                controller.search_bar.set_search_mode(toggle.is_active());
                if toggle.is_active() {
                    let search = controller.compact_search.clone();
                    glib::idle_add_local_once(move || {
                        search.grab_focus();
                    });
                }
            }
        });
        self.search_bar.connect_search_mode_enabled_notify({
            let weak = Rc::downgrade(self);
            move |bar| {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                if controller.search_toggle.is_active() != bar.is_search_mode() {
                    controller.search_toggle.set_active(bar.is_search_mode());
                }
                if controller.compact.get() && controller.is_servers_page() && !bar.is_search_mode()
                {
                    controller.clear_search();
                }
            }
        });
        self.sidebar_toggle.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    let shown = controller.split.shows_sidebar();
                    controller.split.set_show_sidebar(!shown);
                    controller.refresh_status();
                }
            }
        });
        self.split.connect_show_sidebar_notify({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.refresh_status();
                }
            }
        });
        // Both status strips do the same one thing, in every state: show the
        // page that owns connections. What there is to *do* about the state
        // lives on the action button beside them.
        for strip in [&self.header_status, &self.sidebar_status] {
            strip.connect_clicked({
                let weak = Rc::downgrade(self);
                move |_| {
                    if let Some(controller) = weak.upgrade() {
                        controller.navigate_to(Page::Profiles);
                    }
                }
            });
        }
        for action in [&self.header_status_action, &self.sidebar_status_action] {
            action.connect_clicked({
                let weak = Rc::downgrade(self);
                move |_| {
                    if let Some(controller) = weak.upgrade() {
                        controller.handle_status_action();
                    }
                }
            });
        }
        // On the list, not on each row: `GtkListBoxRow::activate` is the
        // keyboard action signal and a mouse click never emits it — the row
        // would highlight and nothing else would happen. `row-activated` is
        // also the only one of the two that a programmatic `select_row` does
        // not raise, so rebuilding the popover cannot switch the profile.
        self.profile_switcher_list.connect_row_activated({
            let weak = Rc::downgrade(self);
            move |_, row| {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                let profile = controller
                    .profile_switcher_shown
                    .borrow()
                    .get(row.index() as usize)
                    .map(|item| item.profile.clone());
                if let Some(profile) = profile {
                    controller.select_profile(profile);
                }
            }
        });
        self.profiles_banner.connect_button_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.navigate_to(Page::Profiles);
                }
            }
        });
        self.core_banner.connect_button_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.navigate_to(Page::Settings);
                }
            }
        });
        self.logs.connect_clear_requested({
            let weak = Rc::downgrade(self);
            move || {
                if let Some(controller) = weak.upgrade() {
                    let client = controller.state.borrow().client.clone();
                    std::thread::spawn(move || {
                        let _ = client.clear_logs();
                    });
                }
            }
        });
        self.logs.connect_save_failed({
            let weak = Rc::downgrade(self);
            move |detail: String| {
                if let Some(controller) = weak.upgrade() {
                    // The failure path rather than the neutral one: this carries
                    // text from the system, and `show_error` is what keeps that
                    // text reachable in full behind Details instead of truncating
                    // it into a one-line toast.
                    controller.show_error("Could not save the log", &detail);
                }
            }
        });
        self.window.connect_close_request({
            let weak = Rc::downgrade(self);
            move |_| {
                let Some(controller) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if !controller.settings.has_unsaved_changes() {
                    // Closing hides the window; the process stays for the
                    // tray and the daemon keeps the tunnel. Quit lives in
                    // the tray menu.
                    controller.window.set_visible(false);
                    return glib::Propagation::Stop;
                }
                controller.confirm_close_with_unsaved_settings();
                glib::Propagation::Stop
            }
        });
    }

    fn confirm_close_with_unsaved_settings(self: &Rc<Self>) {
        let state = self.settings.state();
        if state.applying {
            self.show_message("Settings are still being applied");
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Apply settings before closing?"),
            Some("Your settings draft has not been applied."),
        );
        dialog.add_responses(&[
            ("cancel", "Cancel"),
            ("discard", "Discard"),
            ("apply", "Apply"),
        ]);
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("apply", state.valid);
        dialog.set_default_response(Some(if state.valid { "apply" } else { "cancel" }));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, {
            let weak = Rc::downgrade(self);
            move |dialog, response| {
                dialog.close();
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                match response {
                    "apply" => {
                        controller.close_after_apply.set(true);
                        controller.settings.request_apply();
                    }
                    "discard" => {
                        controller.settings.reset_draft();
                        controller.finish_close();
                    }
                    // Cancelling abandons a Quit as well as a close.
                    _ => controller.quit_after_close.set(false),
                }
            }
        });
        dialog.present(Some(&self.window));
    }

    fn show_page(self: &Rc<Self>, page: Page) {
        if self.is_servers_page() {
            self.remember_visible_search();
        }
        self.stack.set_visible_child_name(page.stack_name());
        self.sync_search_chrome();
        // Whether the banner belongs on screen is now a function of the page,
        // so leaving it to the next poll would show it for half a second on the
        // very page it is redundant on.
        let sessions = self.state.borrow().ui.sessions.clone();
        self.update_profiles_banner(&sessions);
        if page == Page::Profiles {
            self.refresh_profiles_from_daemon();
        }
        if self.split.is_collapsed() {
            self.split.set_show_sidebar(false);
        }
    }

    /// Profiles are daemon-owned files and can change through the CLI while
    /// the window stays open, so entering Profiles takes a fresh snapshot
    /// without making the GTK main loop wait on D-Bus.
    fn refresh_profiles_from_daemon(self: &Rc<Self>) {
        self.with_fresh_profiles(|_| {});
    }

    /// Take a fresh profile list from the daemon, then run `then`.
    ///
    /// The daemon owns `profiles/*.toml` and the CLI writes them behind the
    /// window's back, so anything that is about to *rewrite* a whole profile
    /// has to start from what is on disk now — not from the copy this page was
    /// built with, which may be arbitrarily old.
    fn with_fresh_profiles(self: &Rc<Self>, then: impl FnOnce(&Rc<Self>) + 'static) {
        let client = self.state.borrow().client.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = client.list_profiles().map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
        });

        let weak = Rc::downgrade(self);
        let mut then = Some(then);
        glib::timeout_add_local(Duration::from_millis(40), move || {
            match receiver.try_recv() {
                Ok(Ok(profiles)) => {
                    if let Some(controller) = weak.upgrade() {
                        controller.state.borrow_mut().profiles = profiles;
                        controller.rebuild_sessions();
                        if let Some(then) = then.take() {
                            then(&controller);
                        }
                    }
                    glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    if let Some(controller) = weak.upgrade() {
                        controller.show_error("Could not refresh profiles", &format!("{error:#}"));
                    }
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(controller) = weak.upgrade() {
                        controller.show_error(
                            "Profile refresh stopped unexpectedly",
                            "The worker ended without reporting a result. The profile list on \
                             screen may be out of date.",
                        );
                    }
                    glib::ControlFlow::Break
                }
            }
        });
    }

    fn is_servers_page(&self) -> bool {
        self.stack.visible_child_name().as_deref() == Some(Page::Servers.stack_name())
    }

    fn handle_search_changed(&self, entry: &gtk::SearchEntry) {
        if self.syncing_search.get() {
            return;
        }
        let next = SearchState::capture(entry);
        let text_changed = self.search_state.borrow().text != next.text;
        *self.search_state.borrow_mut() = next;
        if text_changed {
            self.servers.set_query(&entry.text());
        }
    }

    fn remember_visible_search(&self) {
        let entry = if self.compact.get() {
            &self.compact_search
        } else {
            &self.search
        };
        self.handle_search_changed(entry);
    }

    fn sync_search_entry(&self, entry: &gtk::SearchEntry) {
        self.syncing_search.set(true);
        self.search_state.borrow().restore(entry);
        self.syncing_search.set(false);
    }

    fn clear_search(&self) {
        if self.search_state.borrow().text.is_empty()
            && self.search.text().is_empty()
            && self.compact_search.text().is_empty()
        {
            return;
        }
        *self.search_state.borrow_mut() = SearchState::default();
        self.syncing_search.set(true);
        self.search.set_text("");
        self.compact_search.set_text("");
        self.syncing_search.set(false);
        self.servers.set_query("");
    }

    fn sync_search_chrome(&self) {
        let on_servers = self.is_servers_page();
        let profiles =
            self.stack.visible_child_name().as_deref() == Some(Page::Profiles.stack_name());
        let subscriptions =
            self.stack.visible_child_name().as_deref() == Some(Page::Subscriptions.stack_name());
        let settings =
            self.stack.visible_child_name().as_deref() == Some(Page::Settings.stack_name());
        self.profile_actions.set_visible(profiles);
        self.subscription_actions.set_visible(subscriptions);
        self.settings_actions.set_visible(settings);
        if self.compact.get() {
            self.sync_search_entry(&self.compact_search);
            self.search.set_visible(false);
            self.search_toggle.set_visible(on_servers);
            self.search_bar.set_visible(on_servers);
            self.search_bar
                .set_key_capture_widget(on_servers.then_some(&self.window));
            if on_servers && !self.compact_search.text().is_empty() {
                self.search_bar.set_search_mode(true);
            }
        } else {
            self.sync_search_entry(&self.search);
            self.search.set_visible(on_servers);
            self.search_toggle.set_visible(false);
            self.search_bar.set_visible(false);
            self.search_bar
                .set_key_capture_widget(Option::<&gtk::Widget>::None);
        }
    }

    fn set_compact(&self, enabled: bool) {
        let previous = self.compact.get();
        if previous == enabled {
            return;
        }
        self.remember_visible_search();
        self.compact.set(enabled);

        if enabled {
            self.sidebar_toggle.set_visible(true);
            self.split.set_pin_sidebar(false);
            self.split.set_collapsed(true);
            self.split.set_show_sidebar(false);
        } else {
            self.search_bar.set_search_mode(false);
            self.search_toggle.set_active(false);
            self.split.set_pin_sidebar(true);
            self.split.set_collapsed(false);
            self.split.set_show_sidebar(true);
            self.sidebar_toggle.set_visible(false);
        }

        self.sessions.set_ultra_compact(enabled);
        self.subscriptions.set_ultra_compact(enabled);
        self.settings.set_ultra_compact(enabled);
        self.logs.set_ultra_compact(enabled);
        self.header.set_show_title(!enabled);
        self.sync_search_chrome();
        self.refresh_status();
        // Sidebar visibility changed the width the servers view gets.
        self.push_servers_width();
    }

    /// Feed the servers view the width it can use. Driven from the window
    /// geometry — never from the view's own allocation, which cannot shrink
    /// below the current column count's minimum and would deadlock.
    fn push_servers_width(&self) {
        self.push_servers_width_from(self.window.width());
    }

    /// Like `push_servers_width`, but takes an already-known-fresh width
    /// instead of reading `window.width()`. The widget's own width lags one
    /// or more main-loop turns behind the `GdkSurface`'s width on some
    /// compositors (observed on niri): a single discrete resize like
    /// maximizing fires exactly one `surface` width-notify, and if
    /// `window.width()` hadn't caught up yet at that instant, the column
    /// count would compute from a stale, narrower width and never get
    /// corrected — nothing re-triggers it until the next resize. Passing the
    /// surface's own width (which is definitionally current, since it is
    /// what just changed) avoids that race entirely.
    fn push_servers_width_from(&self, width: i32) {
        let width = if width > 0 {
            width
        } else {
            self.window.default_width()
        };
        self.servers
            .set_available_width(servers_available_width(width, self.compact.get()));
    }

    fn add_breakpoint(self: &Rc<Self>) {
        let sidebar_condition = format!("max-width: {SIDEBAR_BREAKPOINT_WIDTH}px");
        let Ok(condition) = adw::BreakpointCondition::parse(&sidebar_condition) else {
            return;
        };
        let breakpoint = adw::Breakpoint::new(condition);
        breakpoint.connect_apply({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.set_compact(true);
                }
            }
        });
        breakpoint.connect_unapply({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.set_compact(false);
                }
            }
        });
        self.window.add_breakpoint(breakpoint);
    }

    fn rebuild_sessions(self: &Rc<Self>) {
        let (profiles, rows, operation) = {
            let state = self.state.borrow();
            (
                state.profiles.clone(),
                session_rows(&state.profiles, &state.ui, ipc::now_unix_ms()),
                state.ui.operation.clone(),
            )
        };
        let callbacks = SessionCallbacks {
            toggle: {
                let weak = Rc::downgrade(self);
                Rc::new(move |name, active| {
                    if let Some(controller) = weak.upgrade() {
                        if active {
                            controller.up_profile(name);
                        } else {
                            controller.down_profile(name);
                        }
                    }
                })
            },
            edit: {
                let weak = Rc::downgrade(self);
                Rc::new(move |name| {
                    if let Some(controller) = weak.upgrade() {
                        controller.edit_profile(name);
                    }
                })
            },
            create: {
                let weak = Rc::downgrade(self);
                Rc::new(move || {
                    if let Some(controller) = weak.upgrade() {
                        controller.open_profile_dialog(None, None);
                    }
                })
            },
        };
        self.sessions.rebuild(&profiles, &rows, callbacks);
        self.sessions.set_operation(operation);
        self.sync_profile_switcher();
    }

    /// Reread the profile from the daemon before showing its editor. The
    /// dialog saves the profile whole, so opening it from the page's cached
    /// copy would quietly revert anything the CLI wrote since this page was
    /// last entered.
    fn edit_profile(self: &Rc<Self>, name: String) {
        self.with_fresh_profiles(move |controller| {
            controller.open_profile_dialog(Some(name), None)
        });
    }

    /// `None` opens the editor for a profile that does not exist yet.
    fn open_profile_dialog(self: &Rc<Self>, name: Option<String>, pool: Option<PoolQuery>) {
        let (profiles, choices) = {
            let state = self.state.borrow();
            (state.profiles.clone(), server_choices(&state.subscriptions))
        };
        let callbacks = ProfileDialogCallbacks {
            save: {
                let weak = Rc::downgrade(self);
                Rc::new(move |name, profile| {
                    if let Some(controller) = weak.upgrade() {
                        controller.save_profile(name, profile);
                    }
                })
            },
            remove: {
                let weak = Rc::downgrade(self);
                Rc::new(move |name| {
                    if let Some(controller) = weak.upgrade() {
                        controller.remove_profile(name);
                    }
                })
            },
        };
        let mode = match name.as_deref() {
            None => ProfileDialog::New { pool },
            Some(name) => {
                let Some(entry) = profiles.iter().find(|entry| entry.name == name) else {
                    // Removed through the CLI between the click and the reread.
                    self.show_message(&format!("Profile “{name}” no longer exists"));
                    return;
                };
                ProfileDialog::Edit { name, entry }
            }
        };
        // The last settings the daemon accepted, not the current draft: an
        // "inherited" row must name what the profile would really get, and an
        // unapplied edit on the Settings page is not that yet.
        let machine_core = self.settings.applied().core;
        show_profile_dialog(
            &self.sessions.root,
            mode,
            &profiles,
            &choices,
            &machine_core,
            callbacks,
        );
    }

    fn sync_session_rows(self: &Rc<Self>) {
        let rows = {
            let state = self.state.borrow();
            session_rows(&state.profiles, &state.ui, ipc::now_unix_ms())
        };
        // The profile list itself changed under the page — only a full rebuild
        // can hand each row the profile its dialog edits.
        if !self.sessions.set_rows(&rows) {
            self.rebuild_sessions();
        }
    }

    fn sync_profile_switcher(self: &Rc<Self>) {
        let (visible, selected_profile, items) = {
            let state = self.state.borrow();
            (
                switcher_visible(&state.profiles),
                state.ui.selected_profile.clone(),
                switcher_items(&state.profiles, &state.ui),
            )
        };
        self.profile_switcher.set_visible(visible);
        self.profile_switcher_label.set_label(&selected_profile);
        // The name can be elided down to an ellipsis, so the full one has to be
        // reachable somewhere.
        self.profile_switcher
            .set_tooltip_text(Some(&format!("Profile: {selected_profile}")));

        // The poll calls this twice a second. Rebuilding the popover's rows
        // while the user has it open destroys the row they are reaching for,
        // so an open menu keeps the list it was opened with until it closes.
        if *self.profile_switcher_shown.borrow() == items {
            return;
        }
        if self.profile_switcher_popover.is_visible() {
            return;
        }
        self.profile_switcher_shown.replace(items.clone());

        while let Some(child) = self.profile_switcher_list.first_child() {
            self.profile_switcher_list.remove(&child);
        }
        for item in items {
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            content.set_margin_top(6);
            content.set_margin_bottom(6);
            content.set_margin_start(10);
            content.set_margin_end(10);

            let dot = gtk::Label::builder()
                .label("●")
                .css_classes(["profile-switcher-dot"])
                .build();
            set_status_tone(&dot, session_row_tone(item.state));
            let name = gtk::Label::builder()
                .label(&item.profile)
                .hexpand(true)
                .xalign(0.0)
                .build();
            content.append(&dot);
            content.append(&name);

            let row = gtk::ListBoxRow::builder()
                .child(&content)
                .activatable(true)
                .selectable(true)
                .tooltip_text(session_row_state_label(item.state))
                .build();
            self.profile_switcher_list.append(&row);
            if item.selected {
                self.profile_switcher_list.select_row(Some(&row));
            }
        }
    }

    fn select_profile(self: &Rc<Self>, profile: String) {
        {
            let mut state = self.state.borrow_mut();
            if state.ui.selected_profile == profile {
                self.profile_switcher_popover.popdown();
                return;
            }
            state.ui.selected_profile = profile.clone();
        }
        self.profile_switcher.set_label(&profile);
        self.profile_switcher_popover.popdown();
        self.bump_epoch();
        self.refresh_status();
        self.sync_connection_cards();
        // The banner counts sessions *other than* the selected one, so it has
        // to be recounted here rather than waiting for the next poll.
        let sessions = self.state.borrow().ui.sessions.clone();
        self.update_profiles_banner(&sessions);
    }

    fn rebuild_views(self: &Rc<Self>) {
        let (
            subscriptions,
            selected_id,
            connected_id,
            connected_profiles,
            latency_states,
            operation,
        ) = {
            let state = self.state.borrow();
            (
                state.subscriptions.clone(),
                state.selected_id.clone(),
                state.ui.connected_id.clone(),
                state.ui.connected_profiles.clone(),
                latency_states(&state.ui, ipc::now_unix_ms()),
                state.ui.operation.clone(),
            )
        };
        let callbacks = super::views::servers::CardCallbacks {
            select: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id| {
                    if let Some(controller) = weak.upgrade() {
                        controller.select_server(id);
                    }
                })
            },
            activate: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id| {
                    if let Some(controller) = weak.upgrade() {
                        controller.activate_server(id);
                    }
                })
            },
            ping: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id| {
                    if let Some(controller) = weak.upgrade() {
                        controller.probe_one(id, true);
                    }
                })
            },
            trust: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id: String| {
                    if let Some(controller) = weak.upgrade() {
                        let name = controller.server_label(&id);
                        // Nothing is connecting: this was reached from the menu
                        // rather than from a failure, so a fresh reading is
                        // what shows the pin took effect.
                        controller.present_trust_dialog(id, name, AfterTrust::Measure);
                    }
                })
            },
            recheck: {
                let weak = Rc::downgrade(self);
                Rc::new(move |ids: Vec<String>| {
                    if let Some(controller) = weak.upgrade() {
                        controller.enqueue_probes(ids);
                    }
                })
            },
            refresh: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id: String| {
                    if let Some(controller) = weak.upgrade() {
                        controller.refresh_subscription(id);
                    }
                })
            },
            set_alias: {
                let weak = Rc::downgrade(self);
                Rc::new(move |server_id, alias| {
                    if let Some(controller) = weak.upgrade() {
                        controller.set_alias(server_id, alias);
                    }
                })
            },
            create_pool: {
                let weak = Rc::downgrade(self);
                Rc::new(move |query| {
                    if let Some(controller) = weak.upgrade() {
                        controller.open_profile_dialog(None, Some(query));
                    }
                })
            },
            connect_pool: {
                let weak = Rc::downgrade(self);
                Rc::new(move |query| {
                    if let Some(controller) = weak.upgrade() {
                        controller.connect_pool(query);
                    }
                })
            },
        };
        self.servers.rebuild(
            &subscriptions,
            connected_id.as_deref(),
            &connected_profiles,
            selected_id.as_deref(),
            &latency_states,
            callbacks,
        );
        self.rebuild_sessions();

        let sub_callbacks = super::views::subscriptions::SubscriptionCallbacks {
            add: {
                let weak = Rc::downgrade(self);
                Rc::new(move |url, name, send_hwid| {
                    if let Some(controller) = weak.upgrade() {
                        controller.add_subscription(url, name, send_hwid);
                    }
                })
            },
            import: {
                let weak = Rc::downgrade(self);
                Rc::new(move |text| {
                    if let Some(controller) = weak.upgrade() {
                        controller.import_servers(text);
                    }
                })
            },
            refresh: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id| {
                    if let Some(controller) = weak.upgrade() {
                        controller.refresh_subscription(id);
                    }
                })
            },
            refresh_all: {
                let weak = Rc::downgrade(self);
                Rc::new(move || {
                    if let Some(controller) = weak.upgrade() {
                        controller.refresh_all_subscriptions();
                    }
                })
            },
            remove: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id| {
                    if let Some(controller) = weak.upgrade() {
                        controller.remove_subscription(id);
                    }
                })
            },
            remove_server: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id| {
                    if let Some(controller) = weak.upgrade() {
                        controller.remove_server(id);
                    }
                })
            },
            hwid: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id, enabled| {
                    if let Some(controller) = weak.upgrade() {
                        controller.set_hwid(id, enabled);
                    }
                })
            },
            user_agent: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id, user_agent| {
                    if let Some(controller) = weak.upgrade() {
                        controller.set_subscription_user_agent(id, user_agent);
                    }
                })
            },
            groups_holding: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id: String| {
                    weak.upgrade()
                        .map(|controller| controller.servers.groups_holding(&id))
                        .unwrap_or_default()
                })
            },
            groups_holding_any: {
                let weak = Rc::downgrade(self);
                Rc::new(move |id: String| {
                    weak.upgrade()
                        .map(|controller| controller.servers.groups_holding_any(&id))
                        .unwrap_or_default()
                })
            },
        };
        self.subscriptions.rebuild(&subscriptions, sub_callbacks);
        self.subscriptions.set_operation(operation);

        // The fresh cards were built from `connected_id` alone, which is not
        // the same thing as the connection state the cache says is on screen:
        // a rebuild while connecting, or while a stale id is still recorded,
        // would leave a card claiming "Connected" that `sync_connection_cards`
        // then refuses to repair because its cache still matches. Record what
        // the rebuild actually painted, then let the sync reconcile it.
        *self.applied_connection.borrow_mut() = CardConnection {
            active: connected_id,
            profiles: connected_profiles,
            ..CardConnection::default()
        };
        self.sync_connection_cards();
    }

    fn activate_server(self: &Rc<Self>, server_id: String) {
        let action = {
            let state = self.state.borrow();
            card_action(&state.profiles, &state.ui, &server_id)
        };
        match action {
            CardAction::Connect(id) => self.connect_server(id),
            CardAction::Disconnect => self.disconnect(),
            CardAction::UpProfile(name) => self.up_profile(name),
            CardAction::DownProfile(name) => self.down_profile(name),
            CardAction::RepointAndUp {
                profile,
                server_id,
                replaces_pool,
            } => self.confirm_repoint_and_up(profile, server_id, replaces_pool),
        }
    }

    fn confirm_repoint_and_up(
        self: &Rc<Self>,
        profile_name: String,
        server_id: String,
        replaces_pool: bool,
    ) {
        let (profile, server_name) = {
            let state = self.state.borrow();
            let Some(entry) = state
                .profiles
                .iter()
                .find(|entry| entry.name == profile_name)
            else {
                drop(state);
                self.show_message(&format!("Profile “{profile_name}” no longer exists"));
                return;
            };
            let server = state
                .subscriptions
                .iter()
                .flat_map(|subscription| subscription.servers.iter())
                .find(|server| server.id == server_id);
            let server_name = server
                .map(|server| oxidom_core::model::name_without_flag(&server.name).to_string())
                .unwrap_or_else(|| server_id.clone());
            // The alias when there is one, exactly as `server_choices` builds
            // the dialog's handles. Storing the raw id of an aliased server
            // would make the picker report it as a server that does not exist.
            let handle = server
                .and_then(|server| server.alias.clone())
                .unwrap_or_else(|| server_id.clone());
            (
                Profile {
                    description: entry.description.clone(),
                    // Retargeting at one server replaces whatever the profile
                    // selected, a pool included: that is what clicking a card
                    // means. The confirmation dialog below is what keeps it
                    // from happening silently.
                    select: ProfileSelect {
                        server: handle,
                        pool: None,
                    },
                    proxy: ProfileProxy {
                        socks_port: entry.socks_port,
                        http_port: entry.http_port,
                    },
                    interface: entry.interface.clone(),
                    // Retargeting changes the selection, nothing else: the
                    // advanced core settings have no row in this flow and must
                    // survive it untouched. So must the routing block, which has
                    // no row anywhere.
                    core: entry.core.clone(),
                    routing: entry.routing.clone(),
                },
                server_name,
            )
        };

        let title = if replaces_pool {
            format!("Replace the group in “{profile_name}” with {server_name}?")
        } else {
            format!("Point “{profile_name}” at {server_name}?")
        };
        let body = if replaces_pool {
            "This replaces the saved group with one server. The running connection will \
             reconnect and existing connections will close."
                .to_string()
        } else {
            format!(
                "This will rewrite the saved server selection for “{profile_name}” and connect it."
            )
        };
        let dialog = adw::AlertDialog::new(Some(title.as_str()), Some(body.as_str()));
        dialog.add_responses(&[
            ("cancel", "Cancel"),
            (
                "repoint",
                if replaces_pool {
                    "Replace group and connect"
                } else {
                    "Repoint and connect"
                },
            ),
        ]);
        dialog.set_response_appearance("repoint", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, {
            let weak = Rc::downgrade(self);
            move |dialog, response| {
                dialog.close();
                if response != "repoint" {
                    return;
                }
                if let Some(controller) = weak.upgrade() {
                    controller.repoint_and_up(profile_name.clone(), profile.clone());
                }
            }
        });
        dialog.present(Some(&self.window));
    }

    /// Run a group as the selected profile's pool.
    ///
    /// The same rule a card click follows: the selected profile is what gets
    /// pointed somewhere. A group is not a second kind of connection — it is
    /// what a profile selects — so this reuses `repoint_and_up` rather than
    /// inventing a parallel path that could disagree with it.
    fn connect_pool(self: &Rc<Self>, query: PoolQuery) {
        let action = {
            let state = self.state.borrow();
            pool_action(&state.profiles, &state.ui, &query)
        };
        let (profile_name, replaces_server, replaces_pool) = match action {
            PoolAction::UpProfile(name) => {
                self.up_profile(name);
                return;
            }
            PoolAction::DownProfile(name) => {
                self.down_profile(name);
                return;
            }
            PoolAction::NoProfile(name) => {
                self.show_message(&format!(
                    "“{name}” has no profile to write. Create one on the Profiles page first."
                ));
                return;
            }
            // Same servers, different rotation width. Nothing is replaced, so
            // there is nothing to confirm — but the write is still a write, and
            // a toast is what keeps it from being silent.
            PoolAction::RetuneAndUp { profile, expected } => {
                if let Some(rewritten) = self.profile_with_pool(&profile, query.clone()) {
                    self.show_message(&match expected {
                        0 => format!("“{profile}” now rotates over every live node"),
                        count => format!("“{profile}” now rotates over {count} nodes"),
                    });
                    self.repoint_and_up(profile, rewritten);
                }
                return;
            }
            PoolAction::RepointAndUp {
                profile,
                replaces_server,
                replaces_pool,
            } => (profile, replaces_server, replaces_pool),
        };

        let Some(profile) = self.profile_with_pool(&profile_name, query.clone()) else {
            return;
        };
        let group = if query.name.is_empty() {
            "this group".to_string()
        } else {
            format!("“{}”", query.name)
        };
        let nodes = {
            let state = self.state.borrow();
            oxidom_core::pool::resolve(&query, &state.subscriptions)
                .map(|members| members.len())
                .unwrap_or_default()
        };
        let body = match (replaces_pool, replaces_server.as_deref()) {
            (true, _) => format!(
                "“{profile_name}” will run {group} across {nodes} servers instead of its current \
                 pool. It will reconnect, and existing connections will close."
            ),
            (false, Some(server)) => format!(
                "“{profile_name}” points at {server}. Connecting {group} replaces that with a \
                 pool of {nodes} servers, and rewrites the saved selection."
            ),
            (false, None) => format!(
                "“{profile_name}” will run {group} across {nodes} servers, and the saved \
                 selection is rewritten."
            ),
        };
        let title = format!("Connect “{profile_name}” to {group}?");
        let dialog = adw::AlertDialog::new(Some(title.as_str()), Some(body.as_str()));
        dialog.add_responses(&[("cancel", "Cancel"), ("connect", "Connect")]);
        dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, {
            let weak = Rc::downgrade(self);
            move |dialog, response| {
                dialog.close();
                if response != "connect" {
                    return;
                }
                if let Some(controller) = weak.upgrade() {
                    controller.repoint_and_up(profile_name.clone(), profile.clone());
                }
            }
        });
        dialog.present(Some(&self.window));
    }

    /// The named profile with its selection replaced by `query`, everything
    /// else — ports, description, interface — carried through untouched.
    fn profile_with_pool(self: &Rc<Self>, name: &str, query: PoolQuery) -> Option<Profile> {
        let state = self.state.borrow();
        let Some(entry) = state.profiles.iter().find(|entry| entry.name == name) else {
            drop(state);
            self.show_message(&format!("Profile “{name}” no longer exists"));
            return None;
        };
        Some(Profile {
            description: entry.description.clone(),
            // A pool and a server are the two halves of one choice, so taking
            // the pool means clearing the server; leaving both set is the one
            // shape `Profile::validate` refuses.
            select: ProfileSelect {
                server: String::new(),
                // The bar chooses membership and strategy; the pool's tuning —
                // node cap, rotation width, probe interval — is the profile
                // editor's, and connecting a group must not quietly reset it.
                pool: Some(pool_for_profile(Some(entry), query)),
            },
            proxy: ProfileProxy {
                socks_port: entry.socks_port,
                http_port: entry.http_port,
            },
            interface: entry.interface.clone(),
            // As with the pool's tuning above: connecting a group changes the
            // membership, not the core settings or the routing block the profile
            // was given.
            core: entry.core.clone(),
            routing: entry.routing.clone(),
        })
    }

    fn disconnect_if_active(self: &Rc<Self>) {
        let (profile, status) = {
            let state = self.state.borrow();
            (
                state.ui.selected_profile.clone(),
                selected_status(&state.ui),
            )
        };
        // A connect job still in flight owns the operation slot, so a Disconnect
        // sent now is refused as busy — and the refusal used to arrive looking
        // like the tunnel itself had failed. There is nothing to stop yet.
        if matches!(status, Status::Connecting) && self.state.borrow().ui.operation.is_some() {
            self.show_message("Still connecting — wait for it to finish or fail");
            return;
        }
        if matches!(status, Status::Connecting | Status::Connected) {
            if profile == "default" {
                self.disconnect();
            } else {
                self.down_profile(profile);
            }
        }
    }

    fn select_server(self: &Rc<Self>, server_id: String) {
        let selected_id = {
            let mut state = self.state.borrow_mut();
            state.selected_id = if state.selected_id.as_deref() == Some(server_id.as_str()) {
                None
            } else {
                Some(server_id)
            };
            state.selected_id.clone()
        };
        self.servers.set_selected(selected_id.as_deref());
        self.refresh_status();
    }

    /// Mark a card as checking and ask the daemon for a probe. Results come
    /// back through the poll snapshot; the daemon caps concurrency.
    fn probe_one(self: &Rc<Self>, server_id: String, notify_failure: bool) {
        {
            let mut state = self.state.borrow_mut();
            // A press while this card is checking means stop, not "ignore me".
            // Answering the same gesture with silence, twice over — the daemon
            // drops the duplicate too — is what made the control feel dead.
            if state.ui.checking.contains_key(&server_id) {
                drop(state);
                self.cancel_probes(vec![server_id]);
                return;
            }
            state
                .ui
                .checking
                .insert(server_id.clone(), ProbeWait::new(Instant::now()));
            if notify_failure {
                state.ui.notify_probe.insert(server_id.clone());
            }
        }
        self.servers
            .set_latency_state(&server_id, LatencyState::Checking);
        self.refresh_activity_status();
        self.request_probes(vec![server_id]);
    }

    fn enqueue_probes(self: &Rc<Self>, ids: Vec<String>) {
        if press_stops(&ids, &self.state.borrow().ui.checking) {
            self.cancel_probes(ids);
            return;
        }
        let new_ids: Vec<String> = {
            let mut state = self.state.borrow_mut();
            let now = Instant::now();
            let mut new_ids = Vec::new();
            for id in ids {
                state.ui.checking.insert(id.clone(), ProbeWait::new(now));
                // A sweep says nothing about one silent server, but it must say
                // when nothing was measured at all: without this, a machine with
                // no Xray core marks every card and explains none of them.
                state.ui.notify_local.insert(id.clone());
                new_ids.push(id);
            }
            new_ids
        };
        if new_ids.is_empty() {
            return;
        }
        for id in &new_ids {
            self.servers.set_latency_state(id, LatencyState::Checking);
        }
        self.refresh_activity_status();
        self.request_probes(new_ids);
    }

    /// Ask the daemon to call off checks.
    ///
    /// Nothing is repainted here, and that is deliberate rather than lazy. The
    /// daemon leaves a `Cancelled` reading for every id it drops, so those ids
    /// leave `running ∪ queued` and the reducer retires their spinners on the
    /// next poll through the path it already uses for a finished check. Anything
    /// already measuring is not called off — only the queue is dropped — and its
    /// card retires when its own reading lands. Deciding here which of the two a
    /// card is would be a second copy of a rule the reducer already owns, and
    /// the copy would be the one that is wrong.
    fn cancel_probes(self: &Rc<Self>, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let client = self.state.borrow().client.clone();
        std::thread::spawn(move || {
            // Fire and forget, with the failure in the log rather than a toast:
            // the checks carry on, every card still says so, and interrupting
            // the user to report that stopping failed is worse than letting the
            // spinners speak for themselves.
            if let Err(error) = client.cancel_probes(&ids) {
                log::warn!("could not call off the latency checks: {error:#}");
            }
        });
    }

    /// Ask the daemon to probe `ids` on a worker thread, and put the cards
    /// back if the request never lands. The poll only ever clears ids the
    /// daemon itself acknowledges, so a dropped request — daemon restarting,
    /// bus call refused — would otherwise leave the spinner turning for the
    /// rest of the session with no way to retry.
    fn request_probes(self: &Rc<Self>, ids: Vec<String>) {
        let client = self.state.borrow().client.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let work_ids = ids.clone();
        std::thread::spawn(move || {
            let result = match work_ids.as_slice() {
                [single] => client.request_probe(single),
                many => client.request_probes(many),
            };
            let _ = sender.send(result.err().map(|error| format!("{error:#}")));
        });

        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(40), move || {
            let failure = match receiver.try_recv() {
                Ok(None) => return glib::ControlFlow::Break,
                Ok(Some(error)) => error,
                Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    "the probe request did not complete".to_string()
                }
            };
            if let Some(controller) = weak.upgrade() {
                controller.abandon_probes(&ids, &failure);
            }
            glib::ControlFlow::Break
        });
    }

    /// Give up on probes the daemon never accepted: clear the spinners and
    /// restore whatever reading the cards had before.
    fn abandon_probes(self: &Rc<Self>, ids: &[String], error: &str) {
        let restored: Vec<(String, LatencyState)> = {
            let mut state = self.state.borrow_mut();
            let now_unix_ms = ipc::now_unix_ms();
            let mut restored = Vec::new();
            for id in ids {
                if state.ui.checking.remove(id).is_some() {
                    state.ui.notify_probe.remove(id);
                    state.ui.notify_local.remove(id);
                    // Read after the removal above, so the card falls back to
                    // what it knew rather than to the spinner it is leaving.
                    let latency = state.ui.card_state(id, now_unix_ms);
                    restored.push((id.clone(), latency));
                }
            }
            restored
        };
        if restored.is_empty() {
            return;
        }
        for (id, latency_state) in restored {
            self.servers.set_latency_state(&id, latency_state);
        }
        self.refresh_activity_status();
        self.show_error("Could not check latency", &format!("{error:#}"));
    }

    fn connect_server(self: &Rc<Self>, server_id: String) {
        {
            let mut state = self.state.borrow_mut();
            state.selected_id = Some(server_id.clone());
            state.ui.connected_id = Some(server_id.clone());
            // Whatever failed before, this click supersedes it — including a
            // retry of the very server that failed.
            state.ui.failed_id = None;
            // `Connect` is the daemon's method for the default session and no
            // other, so that is the profile this pin describes — regardless of
            // which profile the header happens to be showing.
            state
                .ui
                .pin_status("default", Status::Connecting, Instant::now());
        }
        self.bump_epoch();
        self.set_cards_connection(CardConnection {
            active: Some(server_id.clone()),
            profiles: self.state.borrow().ui.connected_profiles.clone(),
            connecting: Some(server_id.clone()),
            failed: None,
        });
        self.servers.set_selected(Some(&server_id));
        self.rebuild_sessions();
        self.refresh_status();
        let work_id = server_id.clone();
        let failed_id = server_id.clone();
        self.client_job(
            UiOperation::for_server(UiOperationKind::Connect, server_id),
            move |client| client.connect_server(&work_id),
            move |controller, result| {
                if let Err(error) = result {
                    // Refused before the click reached the daemon, so the
                    // server never answered and must not be shown as failing.
                    // Dropping the optimistic pin is the whole rollback: the
                    // next poll restores the daemon's own view of what is up.
                    if is_busy(&error) {
                        controller.state.borrow_mut().ui.clear_pin();
                        controller.set_cards_connection(CardConnection {
                            active: controller.state.borrow().ui.connected_id.clone(),
                            profiles: controller.state.borrow().ui.connected_profiles.clone(),
                            connecting: None,
                            failed: None,
                        });
                        controller.rebuild_sessions();
                        controller.refresh_status();
                        controller.show_message(&format!("Still busy — {error}"));
                        return;
                    }
                    let message = format!("{error:#}");
                    {
                        let mut state = controller.state.borrow_mut();
                        state.ui.pin_status(
                            "default",
                            Status::Error(message.clone()),
                            Instant::now(),
                        );
                        state.ui.connected_id = None;
                        // Named here rather than left to the daemon: a refused
                        // bus call never reached it, so it has no failure to
                        // report.
                        state.ui.failed_id = Some(failed_id.clone());
                    }
                    controller.set_cards_connection(CardConnection {
                        active: None,
                        profiles: controller.state.borrow().ui.connected_profiles.clone(),
                        connecting: None,
                        failed: Some(failed_id.clone()),
                    });
                    controller.rebuild_sessions();
                    // The daemon reports the same failure on its next poll;
                    // claim it now so it is not toasted twice.
                    controller.mark_error_notified(&message);
                    controller.show_error("Could not connect", &message);
                }
                controller.reconcile_system_proxy();
                controller.refresh_status();
            },
        );
    }

    fn disconnect(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            // Symmetrically with `connect_server`: `Disconnect` stops the
            // default session, so that is who the pin belongs to.
            state
                .ui
                .pin_status("default", Status::Disconnected, Instant::now());
            state.ui.connected_id = None;
            state.ui.failed_id = None;
        }
        self.bump_epoch();
        self.set_cards_connection(CardConnection::default());
        self.rebuild_sessions();
        self.refresh_status();
        self.client_job(
            UiOperation::new(UiOperationKind::Disconnect),
            |client| client.disconnect(),
            |controller, result| {
                if let Err(error) = result {
                    controller.show_error("Could not disconnect", &format!("{error:#}"));
                }
                controller.reconcile_system_proxy();
            },
        );
    }

    fn save_profile(self: &Rc<Self>, name: String, profile: Profile) {
        let work_name = name.clone();
        self.client_job(
            UiOperation::for_profile(UiOperationKind::SaveProfile, name),
            move |client| {
                Ok(refresh_profiles_after(
                    client,
                    client.save_profile(&work_name, &profile),
                ))
            },
            |controller, result| match result {
                Ok((operation, profiles)) => {
                    controller.state.borrow_mut().profiles = profiles;
                    controller.rebuild_views();
                    if let Err(error) = operation {
                        controller.show_error("Could not save profile", &format!("{error:#}"));
                    }
                }
                Err(error) => {
                    controller.show_error("Could not save profile", &format!("{error:#}"));
                }
            },
        );
    }

    fn remove_profile(self: &Rc<Self>, name: String) {
        let work_name = name.clone();
        self.client_job(
            UiOperation::for_profile(UiOperationKind::RemoveProfile, name),
            move |client| {
                Ok(refresh_profiles_after(
                    client,
                    client.remove_profile(&work_name),
                ))
            },
            |controller, result| match result {
                Ok((operation, profiles)) => {
                    controller.state.borrow_mut().profiles = profiles;
                    controller.rebuild_views();
                    if let Err(error) = operation {
                        controller.show_error("Could not remove profile", &format!("{error:#}"));
                    }
                }
                Err(error) => {
                    controller.show_error("Could not remove profile", &format!("{error:#}"));
                }
            },
        );
    }

    fn up_profile(self: &Rc<Self>, name: String) {
        let work_name = name.clone();
        let finish_name = name.clone();
        self.client_job(
            UiOperation::for_profile(UiOperationKind::UpProfile, name),
            move |client| {
                Ok(refresh_profiles_after(
                    client,
                    client.up_profile(&work_name),
                ))
            },
            move |controller, result| {
                controller.finish_up_profile(finish_name, result);
            },
        );
    }

    /// Stop and start one profile, for the case where its session is alive but
    /// failed and the reason has just been fixed.
    fn reconnect_profile(self: &Rc<Self>, name: String) {
        let work_name = name.clone();
        let finish_name = name.clone();
        self.client_job(
            UiOperation::for_profile(UiOperationKind::UpProfile, name),
            move |client| {
                // The failure this follows is not a reason to stop: a profile
                // that was never up is exactly what `up` wants.
                let _ = client.down_profile(&work_name);
                Ok(refresh_profiles_after(
                    client,
                    client.up_profile(&work_name),
                ))
            },
            move |controller, result| {
                controller.finish_up_profile(finish_name, result);
            },
        );
    }

    fn repoint_and_up(self: &Rc<Self>, name: String, profile: Profile) {
        let work_name = name.clone();
        let finish_name = name.clone();
        self.client_job(
            UiOperation::for_profile(UiOperationKind::UpProfile, name),
            move |client| {
                let operation = client
                    .save_profile(&work_name, &profile)
                    // A running profile cannot be brought up twice. The user
                    // explicitly confirmed replacing its selection, so close
                    // that routing domain before starting the saved one.
                    .and_then(|()| client.down(&work_name).map(|_| ()))
                    .and_then(|()| client.up_profile(&work_name));
                Ok(refresh_profiles_after(client, operation))
            },
            move |controller, result| {
                controller.finish_up_profile(finish_name, result);
            },
        );
    }

    fn finish_up_profile(
        self: &Rc<Self>,
        name: String,
        result: Result<(Result<ipc::UpResult>, Vec<ProfileEntry>)>,
    ) {
        let operation = match result {
            Ok((operation, profiles)) => {
                self.state.borrow_mut().profiles = profiles;
                operation
            }
            Err(error) => Err(error),
        };
        match operation {
            Ok(result) => {
                let is_pool = self
                    .state
                    .borrow()
                    .profiles
                    .iter()
                    .find(|profile| profile.name == name)
                    .is_some_and(|profile| profile.pool.is_some());
                let server_id = (!is_pool).then_some(result.server.id);
                {
                    let mut state = self.state.borrow_mut();
                    if let Some(server_id) = &server_id {
                        state.selected_id = Some(server_id.clone());
                        state.ui.connected_id = Some(server_id.clone());
                    } else if name == "default" {
                        // A pool has no active member. Keeping the previous
                        // default server here would light up its card and put
                        // its name back into the header until the next poll.
                        state.ui.connected_id = None;
                    }
                    state.ui.failed_id = None;
                    // The pin belongs to the profile that was brought up, not
                    // to whichever one the header shows: a switch away must
                    // not carry this transition along.
                    state
                        .ui
                        .pin_status(&name, Status::Connecting, Instant::now());
                }
                self.bump_epoch();
                if let Some(server_id) = server_id {
                    self.set_cards_connection(CardConnection {
                        active: Some(server_id.clone()),
                        profiles: self.state.borrow().ui.connected_profiles.clone(),
                        connecting: Some(server_id.clone()),
                        failed: None,
                    });
                    self.servers.set_selected(Some(&server_id));
                } else {
                    self.sync_connection_cards();
                }
                self.rebuild_sessions();
                if !result.ignored_ports.is_empty() {
                    self.show_message(&format!(
                        "{} left unchanged — fixed by the system service unit",
                        result.ignored_ports.join(" and ")
                    ));
                }
            }
            Err(error) => {
                // Deliberately neither pinned nor cleared. `UpProfile` refuses
                // before it touches the tunnel whenever the profile is
                // unreadable or its handle resolves to nothing or to several
                // servers, and calling that "disconnected" would blank a
                // connection that is still carrying traffic. The status this
                // worker read after the call lands immediately after this
                // handler and paints whichever of the two it actually is.
                self.mark_error_notified(&format!("{error:#}"));
                self.show_error(
                    &format!("Could not bring up “{name}”"),
                    &format!("{error:#}"),
                );
            }
        }
        self.reconcile_system_proxy();
        self.refresh_status();
    }

    fn down_profile(self: &Rc<Self>, name: String) {
        let work_name = name.clone();
        let message_name = name.clone();
        let pinned_name = name.clone();
        self.client_job(
            UiOperation::for_profile(UiOperationKind::DownProfile, name),
            move |client| Ok(refresh_profiles_after(client, client.down(&work_name))),
            move |controller, result| {
                let operation = match result {
                    Ok((operation, profiles)) => {
                        controller.state.borrow_mut().profiles = profiles;
                        operation
                    }
                    Err(error) => Err(error),
                };
                match operation {
                    Ok(false) => {
                        controller.rebuild_sessions();
                        controller.show_message(&format!(
                            "“{message_name}” is not the profile running the tunnel"
                        ));
                    }
                    Ok(true) => {
                        {
                            let mut state = controller.state.borrow_mut();
                            state
                                .ui
                                .pin_status(&pinned_name, Status::Disconnected, Instant::now());
                            state.ui.connected_id = None;
                            state.ui.failed_id = None;
                        }
                        controller.bump_epoch();
                        controller.set_cards_connection(CardConnection::default());
                        controller.rebuild_sessions();
                        controller.refresh_status();
                        controller.reconcile_system_proxy();
                    }
                    Err(error) => {
                        controller.rebuild_sessions();
                        controller.show_error(
                            &format!("Could not disconnect “{message_name}”"),
                            &format!("{error:#}"),
                        );
                    }
                }
            },
        );
    }

    fn add_subscription(self: &Rc<Self>, url: String, name: Option<String>, send_hwid: bool) {
        self.client_job(
            UiOperation::new(UiOperationKind::AddSubscription),
            move |client| client.add_subscription(&url, name.as_deref(), send_hwid),
            |controller, result| controller.finish_subscription_change("add subscription", result),
        );
    }

    fn refresh_subscription(self: &Rc<Self>, subscription_id: String) {
        let work_id = subscription_id.clone();
        self.client_job(
            UiOperation::for_subscription(UiOperationKind::UpdateSubscription, subscription_id),
            move |client| client.refresh(&work_id),
            |controller, result| {
                controller.finish_subscription_change("update subscription", result)
            },
        );
    }

    fn refresh_all_subscriptions(self: &Rc<Self>) {
        self.client_job(
            UiOperation::new(UiOperationKind::UpdateAllSubscriptions),
            |client| client.refresh_all(),
            |controller, result| {
                controller.finish_subscription_change("update subscriptions", result)
            },
        );
    }

    fn remove_subscription(self: &Rc<Self>, subscription_id: String) {
        let work_id = subscription_id.clone();
        self.client_job(
            UiOperation::for_subscription(UiOperationKind::DeleteSubscription, subscription_id),
            move |client| client.remove_subscription(&work_id),
            |controller, result| {
                controller.finish_removal("remove subscription", result);
            },
        );
    }

    fn import_servers(self: &Rc<Self>, text: String) {
        self.client_job(
            UiOperation::new(UiOperationKind::ImportServers),
            move |client| client.import_links(&text),
            |controller, result| match result {
                Ok((count, unsupported)) => {
                    controller.rebuild_views();
                    let mut message = match count {
                        0 => "No new servers (already imported)".to_string(),
                        1 => "Imported 1 server".to_string(),
                        n => format!("Imported {n} servers"),
                    };
                    if unsupported > 0 {
                        message.push_str(&format!(" · {unsupported} unsupported links skipped"));
                    }
                    controller.show_message(&message);
                }
                Err(error) => controller.show_error("Could not import", &format!("{error:#}")),
            },
        );
    }

    fn remove_server(self: &Rc<Self>, server_id: String) {
        let work_id = server_id.clone();
        self.client_job(
            UiOperation::for_server(UiOperationKind::DeleteServer, server_id),
            move |client| client.remove_server(&work_id),
            |controller, result| controller.finish_removal("remove server", result),
        );
    }

    fn finish_subscription_change(self: &Rc<Self>, action: &str, result: Result<()>) {
        if let Err(error) = result {
            self.show_error(&format!("Could not {action}"), &format!("{error:#}"));
            return;
        }
        self.rebuild_views();
    }

    /// Like finish_subscription_change, but the removal may have taken the
    /// active server down with it — reflect that in the connection UI.
    fn finish_removal(self: &Rc<Self>, action: &str, result: Result<bool>) {
        match result {
            Ok(disconnected) => {
                if disconnected {
                    {
                        let mut state = self.state.borrow_mut();
                        state.ui.connected_id = None;
                        state.ui.failed_id = None;
                    }
                    // No failure to report: the server is simply gone, and
                    // naming it as failed would point at a card that no longer
                    // exists.
                    self.set_cards_connection(CardConnection::default());
                    self.show_message("Disconnected — the active server was removed");
                    self.reconcile_system_proxy();
                    self.refresh_status();
                }
                self.rebuild_views();
            }
            Err(error) => self.show_error(&format!("Could not {action}"), &format!("{error:#}")),
        }
    }

    fn set_hwid(self: &Rc<Self>, subscription_id: String, enabled: bool) {
        let work_id = subscription_id;
        self.client_job(
            UiOperation::new(UiOperationKind::ApplySettings),
            move |client| client.set_hwid(&work_id, enabled),
            |controller, result| {
                if let Err(error) = result {
                    controller.show_error("Could not save HWID preference", &format!("{error:#}"));
                }
                controller.rebuild_views();
            },
        );
    }

    fn set_subscription_user_agent(self: &Rc<Self>, subscription_id: String, user_agent: String) {
        let work_id = subscription_id;
        self.client_job(
            UiOperation::new(UiOperationKind::ApplySettings),
            move |client| client.set_subscription_user_agent(&work_id, &user_agent),
            |controller, result| {
                if let Err(error) = result {
                    controller.show_error("Could not save the User-Agent", &format!("{error:#}"));
                }
                controller.rebuild_views();
            },
        );
    }

    fn set_alias(self: &Rc<Self>, server_id: String, alias: String) {
        self.client_job(
            UiOperation::new(UiOperationKind::ApplySettings),
            move |client| client.set_server_alias(&server_id, &alias),
            |controller, result| match result {
                Ok(()) => controller.rebuild_views(),
                Err(error) => {
                    controller.show_error("Could not set alias", &format!("{error:#}"));
                }
            },
        );
    }

    fn save_settings(self: &Rc<Self>, values: SettingsValues) {
        let validation = self.settings.validation();
        if !validation.is_valid() {
            self.settings.set_apply_in_progress(false);
            return;
        }
        let config = Config {
            socks_port: values.socks_port,
            http_port: values.http_port,
            system_proxy: values.system_proxy,
            reconnect: values.reconnect,
            latency_method: values.latency_method,
            latency_test_url: values.latency_test_url.clone(),
            subscription_user_agent: values.subscription_user_agent.clone(),
            xray_binary: values.xray_binary.clone(),
            tun2socks_binary: values.tun2socks_binary.clone(),
            nft_binary: values.nft_binary.clone(),
            core: values.core.clone(),
        };
        self.client_job(
            UiOperation::new(UiOperationKind::ApplySettings),
            move |client| {
                let outcome = client.apply_settings(&config)?;
                // Applying can move the Xray path or be refused outright; ask
                // the daemon what it ended up with instead of assuming. The
                // stored config is the authority on what was accepted — the
                // page used to mark the *request* applied and then sat clean
                // holding values the daemon had reverted.
                Ok((outcome, client.settings().ok(), client.runtime_info().ok()))
            },
            move |controller, result| {
                match result {
                    Ok((outcome, stored, runtime)) => {
                        match stored.as_ref() {
                            Some(config) => controller
                                .settings
                                .adopt_applied(&values, SettingsValues::from(config)),
                            // An older daemon has no GetSettings; accepting the
                            // request is what this code always did, and is still
                            // better than leaving the page permanently dirty.
                            None => controller.settings.mark_applied(values.clone()),
                        }
                        controller.set_runtime_info(runtime.clone());
                        // Pointing Settings at a core that exists is exactly
                        // when the banner should go away.
                        controller.update_core_banner(runtime.as_ref());
                        if !outcome.ignored_ports.is_empty() {
                            controller.show_message(&format!(
                                "{} left unchanged — fixed by the system service unit",
                                outcome.ignored_ports.join(" and ")
                            ));
                        }
                        if !outcome.ignored_paths.is_empty() {
                            // Not the service unit: a privileged daemon spawns
                            // what these name, so it resolves them itself.
                            controller.show_message(&format!(
                                "{} left unchanged — the system service chooses its own core binaries",
                                outcome.ignored_paths.join(" and ")
                            ));
                        }
                        if let Some(error) = outcome.reconnect_error {
                            // The port change took the tunnel down and it did
                            // not come back: that is a failure of the server it
                            // was running for, and the card should say so.
                            let failed = {
                                let mut state = controller.state.borrow_mut();
                                // Only the default session is restarted after a
                                // port change, so only it can have failed here.
                                state.ui.pin_status(
                                    "default",
                                    Status::Error(error.clone()),
                                    Instant::now(),
                                );
                                let failed = state.ui.connected_id.take();
                                state.ui.failed_id = failed.clone();
                                failed
                            };
                            controller.set_cards_connection(CardConnection {
                                active: None,
                                profiles: controller.state.borrow().ui.connected_profiles.clone(),
                                connecting: None,
                                failed,
                            });
                            controller.mark_error_notified(&error);
                            controller.show_error(
                                "Settings saved, but the connection could not restart",
                                &error,
                            );
                        }
                        controller.reconcile_system_proxy();
                        if controller.close_after_apply.replace(false) {
                            controller.finish_close();
                        }
                    }
                    Err(error) => {
                        controller.settings.set_apply_in_progress(false);
                        controller.close_after_apply.set(false);
                        controller.show_error("Could not save settings", &format!("{error:#}"));
                    }
                }
                controller.refresh_status();
            },
        );
    }

    /// Run one daemon call on a worker thread with the operation spinner up.
    /// The fresh subscriptions snapshot is fetched on the same thread right
    /// after the call, so completions see consistent state.
    fn client_job<R, Work, Complete>(
        self: &Rc<Self>,
        operation: UiOperation,
        work: Work,
        complete: Complete,
    ) where
        R: Send + 'static,
        Work: FnOnce(&DaemonClient) -> Result<R> + Send + 'static,
        Complete: FnOnce(&Rc<Self>, Result<R>) + 'static,
    {
        {
            let mut state = self.state.borrow_mut();
            if state.ui.operation.is_some() {
                drop(state);
                // Hand the refusal to the completion handler instead of
                // returning silently. It owns the per-call cleanup — clearing
                // the settings spinner, dropping a queued close, rolling back
                // the optimistic "connecting" card — and skipping it leaves
                // the UI stuck in a state the user cannot undo.
                complete(self, Err(anyhow!(Busy)));
                return;
            }
            state.ui.operation = Some(operation.clone());
        }
        self.sessions.set_operation(Some(operation.clone()));
        self.subscriptions.set_operation(Some(operation));
        self.refresh_activity_status();

        // Stamped on the main thread, before the worker exists: `AppState` is
        // not `Send`, and an epoch read after the D-Bus calls would certify
        // exactly the staleness it is supposed to catch.
        let epoch = self.bump_epoch();
        let client = self.state.borrow().client.clone();
        let log_cursor = self.log_feed.borrow().remote_cursor();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = work(&client);
            let subscriptions = client.subscriptions();
            // Fetch a fresh status/probe snapshot on the same thread right
            // after the call, so the completion handler can apply it
            // immediately instead of waiting for the next 500ms poll tick —
            // that gap is what let the header/sidebar/card flash a stale
            // state right after an operation the user is watching finished.
            let snapshot = (|| {
                Ok::<PolledSnapshot, anyhow::Error>(PolledSnapshot {
                    status: client.status()?,
                    probe: client.probe_state()?,
                    logs: client.logs_since(log_cursor, LOG_FETCH_LIMIT)?,
                    epoch,
                })
            })();
            let _ = sender.send((result, subscriptions, snapshot));
        });

        let weak = Rc::downgrade(self);
        let mut complete = Some(complete);
        glib::timeout_add_local(Duration::from_millis(40), move || {
            match receiver.try_recv() {
                Ok((result, subscriptions, snapshot)) => {
                    let Some(controller) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    {
                        let mut state = controller.state.borrow_mut();
                        if let Ok(subscriptions) = subscriptions {
                            state.subscriptions = subscriptions;
                        }
                        // The pin is deliberately left standing: retiring it
                        // here is what let a snapshot older than the click
                        // repaint the pre-click frame. `reduce` drops it once
                        // the daemon stops reporting the old world, or after
                        // its deadline.
                        state.ui.operation = None;
                    }
                    controller.sessions.set_operation(None);
                    controller.subscriptions.set_operation(None);
                    // The handler runs *before* the snapshot is applied: it is
                    // where a failed connect pins its outcome, and applying
                    // first would paint one frame of the pre-failure state and
                    // then leave the pin to be picked up half a second later.
                    if let Some(complete) = complete.take() {
                        complete(&controller, result);
                    }
                    // Whatever the handler just decided outranks the reads this
                    // worker made — but its own reads happened after the daemon
                    // returned from the operation, so they are authoritative by
                    // construction and get re-stamped rather than dropped.
                    let epoch = controller.bump_epoch();
                    match snapshot {
                        Ok(mut snapshot) => {
                            snapshot.epoch = epoch;
                            controller.apply_snapshot(snapshot);
                        }
                        Err(_) => controller.refresh_status(),
                    }
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(controller) = weak.upgrade() {
                        let mut state = controller.state.borrow_mut();
                        state.ui.clear_pin();
                        state.ui.operation = None;
                        drop(state);
                        controller.bump_epoch();
                        controller.sessions.set_operation(None);
                        controller.subscriptions.set_operation(None);
                        // Hand the loss to the completion handler, for the same
                        // reason the `Busy` refusal above does: it owns the
                        // per-call cleanup. Skipping it here left the settings
                        // spinner up for the rest of the session — `Apply` and
                        // `Reset` are insensitive until the handler lowers it —
                        // and left a queued close armed, so a later save shut
                        // the window without being asked to.
                        if let Some(complete) = complete.take() {
                            complete(&controller, Err(anyhow!(WorkerLost)));
                        }
                        controller.refresh_status();
                    }
                    glib::ControlFlow::Break
                }
            }
        });
    }

    /// Undo the session proxy when the process is asked to stop rather than
    /// closed from the UI — `systemctl --user stop oxidom-tray`, a session
    /// logout, Ctrl+C on a terminal run. Without this the proxy survives us
    /// and every app on the desktop keeps pointing at a dead port.
    fn watch_termination(self: &Rc<Self>) {
        for signal in [libc::SIGTERM, libc::SIGINT] {
            let weak = Rc::downgrade(self);
            glib::unix_signal_add_local(signal, move || {
                if let Some(controller) = weak.upgrade() {
                    controller.clear_system_proxy();
                    controller.quit_after_close.set(true);
                    controller.finish_close();
                }
                glib::ControlFlow::Break
            });
        }
    }

    /// 500ms tick: apply the last poll snapshot, then start the next poll on
    /// a worker thread (never block the UI on D-Bus).
    fn start_timer(self: &Rc<Self>) {
        let controller = self.clone();
        glib::timeout_add_local(Duration::from_millis(500), move || {
            controller.drain_tray_commands();
            if let Some(snapshot) = oxidom_core::sync::lock(&controller.poll_snapshot).take() {
                controller.apply_snapshot(snapshot);
            }
            let tick = controller.sweep_tick.get().wrapping_add(1);
            controller.sweep_tick.set(tick % AGE_SWEEP_TICKS);
            if controller.sweep_tick.get() == 0 {
                controller.sweep_latency_ages();
            }
            if !controller.poll_in_flight.swap(true, Ordering::SeqCst) {
                let state = controller.state.borrow();
                let client = state.client.clone();
                // Read before the reads, on the main thread: this round can
                // only speak for the world as it stood when it started.
                let epoch = state.ui.state_epoch;
                drop(state);
                let slot = controller.poll_snapshot.clone();
                let in_flight = controller.poll_in_flight.clone();
                // Likewise read here: the cursor says what this round has
                // already seen, and it only moves on the main thread.
                let log_cursor = controller.log_feed.borrow().remote_cursor();
                std::thread::spawn(move || {
                    let snapshot = (|| {
                        Ok::<PolledSnapshot, anyhow::Error>(PolledSnapshot {
                            status: client.status()?,
                            probe: client.probe_state()?,
                            logs: client.logs_since(log_cursor, LOG_FETCH_LIMIT)?,
                            epoch,
                        })
                    })();
                    if let Ok(snapshot) = snapshot {
                        *oxidom_core::sync::lock(&slot) = Some(snapshot);
                    }
                    in_flight.store(false, Ordering::SeqCst);
                });
            }
            glib::ControlFlow::Continue
        });
    }

    /// Everything polled before this call describes a world the user has
    /// already changed. Bumping invalidates the rounds still in flight, and
    /// dropping the slot discards the one that already landed but has not been
    /// applied yet. Returns the new epoch for callers that own an authoritative
    /// snapshot of their own.
    fn bump_epoch(&self) -> u64 {
        let mut state = self.state.borrow_mut();
        state.ui.state_epoch += 1;
        *oxidom_core::sync::lock(&self.poll_snapshot) = None;
        state.ui.state_epoch
    }

    fn apply_snapshot(self: &Rc<Self>, snapshot: PolledSnapshot) {
        let effects = {
            let mut state = self.state.borrow_mut();
            reduce(
                &mut state.ui,
                &snapshot,
                Instant::now(),
                ipc::now_unix_ms(),
                self.window.is_visible(),
            )
        };
        // A round that predates the user's last action is dropped whole — logs,
        // cards and the system-proxy reconciliation included.
        let Some(effects) = effects else {
            return;
        };
        self.update_profiles_banner(&snapshot.status.sessions);
        // Collected rather than issued inline: `probe_one` borrows the state the
        // effects were just produced from.
        let mut reprobe = Vec::new();
        for effect in effects {
            match effect {
                Effect::Latency(id, latency_state) => {
                    self.servers.set_latency_state(&id, latency_state)
                }
                Effect::Reprobe(id) => reprobe.push(id),
                Effect::ToastUnreachable => self.show_message(server_card::UNREACHABLE_TEXT),
                Effect::ToastNoNetwork => self.show_message(server_card::NO_NETWORK_TEXT),
                Effect::ToastProbeDidNotRun => self.show_error(
                    "Latency could not be checked",
                    "The check needs an Xray core and could not start one on this machine. \
                     The servers themselves were never contacted.",
                ),
                Effect::ConnectionError(error) => self.show_error("Connection error", &error),
                Effect::DaemonOutdated => self.show_message(
                    "The oxidom daemon is older than this app — latency readings are unavailable \
                     until it is restarted",
                ),
            }
        }
        for id in reprobe {
            // `probe_one` is a no-op for an id already being checked, which is
            // what keeps a reading the daemon keeps re-taking over the same
            // wrong route from turning into a request loop.
            self.probe_one(id, false);
        }
        self.absorb_logs(snapshot.logs);
        self.sync_session_rows();
        self.sync_profile_switcher();
        self.sync_connection_cards();
        self.reconcile_system_proxy();
        self.refresh_status();
    }

    /// Fold one round of the daemon's log into the view, together with whatever
    /// this process has logged about itself since the last round.
    ///
    /// The GUI's own book is read here rather than in the worker because it is
    /// in this process: there is nothing to wait for, and reading it beside the
    /// daemon's keeps both streams entering the reorder window together.
    fn absorb_logs(&self, remote: LogSlice) {
        let local = logbook::global().since(
            self.log_feed.borrow().local_cursor(),
            LOG_FETCH_LIMIT as usize,
        );
        let batch = self
            .log_feed
            .borrow_mut()
            .absorb(remote, local, ipc::now_unix_ms());
        self.logs.append(&batch);
    }

    /// The banner exists to say that something is running out of sight, and its
    /// button goes to the page that shows it. On that page it is neither: the
    /// rows it points at are already on screen, and the button leads where the
    /// user already is.
    fn update_profiles_banner(&self, sessions: &[ipc::SessionInfo]) {
        let selected_profile = self.state.borrow().ui.selected_profile.clone();
        let elsewhere =
            self.stack.visible_child_name().as_deref() != Some(Page::Profiles.stack_name());
        match other_profiles_message(sessions, &selected_profile).filter(|_| elsewhere) {
            Some(message) => {
                self.profiles_banner.set_title(&message);
                self.profiles_banner.set_revealed(true);
            }
            None => self.profiles_banner.set_revealed(false),
        }
    }

    /// Say once, at the top, what every card would otherwise imply nine times
    /// over: the checks are not failing because the servers are bad.
    fn update_core_banner(&self, runtime: Option<&ipc::RuntimeInfo>) {
        match missing_core_message(runtime) {
            Some(message) => {
                self.core_banner.set_title(&message);
                self.core_banner.set_revealed(true);
            }
            None => self.core_banner.set_revealed(false),
        }
    }

    /// Re-date every badge. Readings do not change when they get older, so
    /// nothing in the poll would ever notice a number crossing from "just
    /// measured" into "measured 3 minutes ago"; the card compares against what
    /// it is showing and ignores the rest, so this costs a lookup per card.
    fn sweep_latency_ages(self: &Rc<Self>) {
        let states = {
            let state = self.state.borrow();
            latency_states(&state.ui, ipc::now_unix_ms())
        };
        for (id, latency) in states {
            self.servers.set_latency_state(&id, latency);
        }
    }

    /// Push the current connection onto the cards, skipping the O(cards)
    /// pass when nothing changed.
    fn sync_connection_cards(&self) {
        let (active, profiles, failed, status) = {
            let state = self.state.borrow();
            (
                state.ui.connected_id.clone(),
                state.ui.connected_profiles.clone(),
                state.ui.failed_id.clone(),
                state.ui.current_status(),
            )
        };
        let desired = match status {
            Status::Connecting => CardConnection {
                active: active.clone(),
                profiles,
                connecting: active,
                failed: None,
            },
            Status::Connected => CardConnection {
                active,
                profiles,
                connecting: None,
                failed: None,
            },
            // A failure keeps naming its server; a plain disconnect names none.
            Status::Error(_) => CardConnection {
                active: None,
                profiles,
                connecting: None,
                failed,
            },
            Status::Disconnected if profiles.is_empty() => CardConnection::default(),
            Status::Disconnected => CardConnection {
                profiles,
                ..CardConnection::default()
            },
        };
        if *self.applied_connection.borrow() == desired {
            return;
        }
        self.set_cards_connection(desired);
    }

    fn set_cards_connection(&self, connection: CardConnection) {
        self.servers.set_connection(&connection);
        *self.applied_connection.borrow_mut() = connection;
    }

    /// The GNOME system proxy is a session concern, so the GUI (not the
    /// daemon, which may run as a system service) applies and clears it. A
    /// marker file survives crashes so the next start can undo a stale proxy.
    fn reconcile_system_proxy(self: &Rc<Self>) {
        let applied_settings = self.settings.applied();
        let desired = {
            let state = self.state.borrow();
            desired_system_proxy_endpoint(
                applied_settings.system_proxy,
                &state.ui.current_status(),
                &state.ui.sessions,
                applied_settings.socks_port,
                applied_settings.http_port,
            )
        };
        if let Some((address, socks_port, http_port)) = desired {
            let endpoint = (address, socks_port, http_port);
            if self.proxy_applied.get() && self.applied_proxy_endpoint.get() != Some(endpoint) {
                // Never leave the previous owner's dead inbound installed if
                // replacing it fails partway through.
                self.clear_system_proxy();
            }
            if !self.proxy_applied.get() {
                // The one setting that could be on and doing nothing: this ran
                // twice a second and dropped the error every time, so a session
                // without `gsettings` left the switch looking applied.
                match sysproxy::apply(address, socks_port, http_port) {
                    Ok(()) => {
                        self.proxy_applied.set(true);
                        self.applied_proxy_endpoint.set(Some(endpoint));
                        if let Some(marker) = gui_proxy_marker() {
                            let _ = std::fs::write(marker, b"1");
                        }
                        self.report_system_proxy(None);
                    }
                    Err(error) => self.report_system_proxy(Some(format!("{error:#}"))),
                }
            }
        } else {
            self.clear_system_proxy();
            // Turning it off, or disconnecting, makes any earlier complaint
            // moot — the row should not keep accusing a switch that is idle.
            self.report_system_proxy(None);
        }
    }

    /// Says once, not on every tick, that the desktop proxy could not be
    /// installed. Keyed on the message so a changing failure still speaks, and
    /// so recovering clears the row rather than leaving a stale red subtitle.
    fn report_system_proxy(self: &Rc<Self>, failure: Option<String>) {
        if *self.system_proxy_failure.borrow() == failure {
            return;
        }
        match failure.as_deref() {
            Some(error) => {
                self.settings.set_system_proxy_failure(Some(
                    "Could not be applied on this desktop — traffic is not going through it",
                ));
                self.show_error("Could not set the system proxy", error);
            }
            None => self.settings.set_system_proxy_failure(None),
        }
        *self.system_proxy_failure.borrow_mut() = failure;
    }

    /// Point the session back at a direct connection. Idempotent, and safe to
    /// call from a signal handler.
    fn clear_system_proxy(&self) {
        if !self.proxy_applied.get() {
            return;
        }
        let _ = sysproxy::clear();
        self.proxy_applied.set(false);
        self.applied_proxy_endpoint.set(None);
        if let Some(marker) = gui_proxy_marker() {
            let _ = std::fs::remove_file(marker);
        }
    }

    fn drain_tray_commands(self: &Rc<Self>) {
        while let Ok(command) = self.tray_commands.try_recv() {
            match command {
                TrayCommand::ShowWindow => self.window.present(),
                TrayCommand::Toggle(profile) => {
                    // The same rule the page's switch follows, so a checkmark
                    // and a switch for one profile can never disagree.
                    let running = self
                        .tray_sessions()
                        .into_iter()
                        .any(|(name, running)| name == profile && running);
                    if running {
                        self.down_profile(profile);
                    } else {
                        self.up_profile(profile);
                    }
                }
                // Quitting from the tray must respect the same unsaved-settings
                // guard as closing the window, or a draft is silently lost.
                TrayCommand::Quit => self.request_quit(),
            }
        }
    }

    /// Quit, confirming first when a settings draft would be lost. Shared by
    /// the tray menu and the Ctrl+Q accelerator.
    fn request_quit(self: &Rc<Self>) {
        self.quit_after_close.set(true);
        if !self.settings.has_unsaved_changes() {
            self.finish_close();
            return;
        }
        self.window.present();
        self.confirm_close_with_unsaved_settings();
    }

    /// Completes a close: quit when it started as Quit, otherwise just hide —
    /// the tray keeps the process and the daemon keeps the tunnel.
    fn finish_close(&self) {
        if self.quit_after_close.replace(false) {
            // Nothing else will undo the GNOME proxy: the daemon may keep the
            // tunnel up after we exit, but it does not own this setting, and
            // the marker file only repairs it on the *next* GUI start. Leaving
            // it set points the whole session at a port that may be gone.
            self.clear_system_proxy();
            if let Some(app) = self.window.application() {
                app.quit();
            }
        } else {
            self.window.set_visible(false);
        }
    }

    /// One `(profile, running)` pair per profile, taken from the very rows the
    /// Profiles page draws so the tray cannot describe a session differently
    /// from the window.
    fn tray_sessions(&self) -> Vec<(String, bool)> {
        let state = self.state.borrow();
        session_rows(&state.profiles, &state.ui, ipc::now_unix_ms())
            .into_iter()
            .map(|row| (row.profile, row.toggle_on))
            .collect()
    }

    /// Mirror the connection into the tray tooltip/menu, skipping no-ops.
    fn update_tray(&self, status: &Status) {
        let Some(handle) = self.tray.borrow().clone() else {
            return;
        };
        let text = {
            let state = self.state.borrow();
            match status {
                Status::Disconnected => "Disconnected".to_string(),
                Status::Connecting => "Connecting…".to_string(),
                Status::Connected => {
                    let name = self
                        .active_pool_name(&state)
                        .or_else(|| self.active_server_name(&state));
                    match name {
                        Some(name) => format!("Connected · {name}"),
                        None => "Connected".to_string(),
                    }
                }
                Status::Error(error) => format!("Error: {error}"),
            }
        };
        let sessions = self.tray_sessions();
        // Any session in error, not just the selected profile's: the icon is
        // the only channel a hidden window has, and "one of your tunnels is
        // down" is worth an attention state whichever one it is.
        let failed = matches!(status, Status::Error(_))
            || self
                .state
                .borrow()
                .ui
                .sessions
                .iter()
                .any(|session| session.state == "error");
        let pushed = TrayState {
            text: text.clone(),
            sessions: sessions.clone(),
            failed,
        };
        if *self.tray_pushed.borrow() == pushed {
            return;
        }
        *self.tray_pushed.borrow_mut() = pushed;
        handle.update(move |tray| {
            tray.status_text = text.clone();
            tray.sessions.clone_from(&sessions);
            tray.failed = failed;
        });
    }

    fn refresh_status(&self) {
        let state = self.state.borrow();
        let status = selected_status(&state.ui);
        let (active_latency, latency_stale) = active_latency_for(&state.ui);
        drop(state);

        self.update_tray(&status);
        self.update_header_connection_status(&status, active_latency, latency_stale);
        self.refresh_activity_status();
    }

    fn active_server_name(&self, state: &AppState) -> Option<String> {
        self.active_server_display(state).map(|(name, _)| name)
    }

    /// The pool label comes from one reducer shared by the header, the sidebar
    /// and the tray, so no surface can accidentally name one member as active.
    fn active_pool_name(&self, state: &AppState) -> Option<String> {
        let session = session_for(&state.ui, &state.ui.selected_profile)?;
        if session.selection.kind != "pool" {
            return None;
        }
        Some(pool_short_label(&session.selection))
    }

    /// Display name and country of the connected server.
    fn active_server_display(&self, state: &AppState) -> Option<(String, Option<String>)> {
        let active = if state.ui.selected_profile == "default" {
            state.ui.connected_id.as_deref()
        } else {
            session_for(&state.ui, &state.ui.selected_profile)
                .and_then(|session| session.server_id.as_deref())
        }?;
        state
            .subscriptions
            .iter()
            .flat_map(|subscription| subscription.servers.iter())
            .find(|server| server.id == active)
            .map(|server| {
                (
                    oxidom_core::model::name_without_flag(&server.name).to_string(),
                    server.country.clone(),
                )
            })
    }

    /// The compact-mode status chip: as small as possible — a flag plus the
    /// latency reading; everything verbose lives in the tooltip.
    fn update_header_connection_status(
        &self,
        status: &Status,
        active_latency: Option<u32>,
        latency_stale: bool,
    ) {
        self.header_status_spinner.set_spinning(false);
        self.header_status_icon.set_visible(false);
        while let Some(child) = self.header_status_flag.first_child() {
            self.header_status_flag.remove(&child);
        }
        self.header_status_flag.set_visible(false);
        self.header_status_label.set_label("");
        self.header_status_label.set_visible(false);
        self.header_status_label.remove_css_class("latency-stale");
        set_status_tone(&self.header_status, StatusTone::Neutral);
        self.header_status
            .set_visible(self.compact.get() && !matches!(status, Status::Disconnected));

        let state = self.state.borrow();
        let pool_name = self.active_pool_name(&state);
        let display = self.active_server_display(&state);
        drop(state);
        let name = display.as_ref().map(|(name, _)| name.clone());
        let country = display.and_then(|(_, country)| country);
        match status {
            Status::Disconnected => {}
            Status::Connecting => {
                set_status_tone(&self.header_status, StatusTone::Working);
                self.header_status_icon
                    .set_icon_name(Some("network-transmit-receive-symbolic"));
                self.header_status_icon.set_visible(true);
                let summary = pool_name.as_deref().or(name.as_deref()).map_or_else(
                    || "Connecting…".to_string(),
                    |name| format!("Connecting · {name}"),
                );
                self.header_status
                    .set_tooltip_text(Some(&format!("{summary} — show connections")));
            }
            Status::Connected => {
                set_status_tone(&self.header_status, StatusTone::Connected);
                if pool_name.is_some() {
                    self.header_status_icon
                        .set_icon_name(Some("network-vpn-symbolic"));
                    self.header_status_icon.set_visible(true);
                } else {
                    self.header_status_flag
                        .append(&super::server_card::flag_widget(country.as_deref(), 16, 14));
                    self.header_status_flag.set_visible(true);
                }
                if let Some(ms) = active_latency {
                    self.header_status_label.set_label(&format!("{ms} ms"));
                    self.header_status_label.set_visible(true);
                    if latency_stale {
                        self.header_status_label.add_css_class("latency-stale");
                    }
                }
                // Names the destination, not the state: Disconnect is the
                // button to the right, and a chip that promised it was how one
                // click came to mean three different things.
                let summary = pool_name
                    .as_deref()
                    .or(name.as_deref())
                    .unwrap_or("Connected");
                self.header_status
                    .set_tooltip_text(Some(&format!("{summary} — show connections")));
            }
            // The text is not put in the tooltip any more: it was the only
            // place it lived, and a tooltip is unreachable by keyboard. The
            // action button beside the chip opens it in full.
            Status::Error(_) => {
                set_status_tone(&self.header_status, StatusTone::Error);
                self.header_status_icon
                    .set_icon_name(Some("dialog-warning-symbolic"));
                self.header_status_icon.set_visible(true);
                self.header_status
                    .set_tooltip_text(Some("Connection error — show connections"));
            }
        }
    }

    fn update_sidebar_connection_status(&self, state: &AppState) {
        let status = selected_status(&state.ui);
        let (active_latency, latency_stale) = active_latency_for(&state.ui);
        self.sidebar_status_label.remove_css_class("latency-stale");
        // The strip is always sensitive because it always does the same thing.
        // It used to be insensitive except when connected, which is how the
        // "show the failure" branch below became unreachable: the one state
        // that needed the click was the one that could not be clicked.
        let summary = match status {
            Status::Disconnected => {
                set_status_tone(&self.sidebar_status, StatusTone::Neutral);
                self.sidebar_status_icon
                    .set_icon_name(Some("network-vpn-symbolic"));
                self.sidebar_status_label.set_label("Disconnected");
                "Disconnected".to_string()
            }
            Status::Connecting => {
                set_status_tone(&self.sidebar_status, StatusTone::Working);
                self.sidebar_status_icon
                    .set_icon_name(Some("network-transmit-receive-symbolic"));
                self.sidebar_status_label.set_label("Connecting…");
                "Connecting".to_string()
            }
            Status::Connected => {
                set_status_tone(&self.sidebar_status, StatusTone::Connected);
                self.sidebar_status_icon
                    .set_icon_name(Some("network-vpn-symbolic"));
                let name = self
                    .active_pool_name(state)
                    .or_else(|| self.active_server_name(state))
                    .unwrap_or_else(|| "Connected".to_string());
                let label = active_latency
                    .map(|ms| format!("{name} · {ms} ms"))
                    .unwrap_or(name);
                self.sidebar_status_label.set_label(&label);
                if active_latency.is_some() && latency_stale {
                    self.sidebar_status_label.add_css_class("latency-stale");
                }
                label
            }
            Status::Error(_) => {
                set_status_tone(&self.sidebar_status, StatusTone::Error);
                self.sidebar_status_icon
                    .set_icon_name(Some("dialog-warning-symbolic"));
                self.sidebar_status_label.set_label("Connection error");
                "Connection error".to_string()
            }
        };
        // Names the destination, not the state, because that is what the click
        // does. The state is already the label an inch to the left.
        self.sidebar_status
            .set_tooltip_text(Some(&format!("{summary} — show connections")));
        self.set_status_action(status_action_for(&status));
    }

    /// The one thing there is to do about the current state, on both strips.
    /// `None` hides the button entirely rather than greying it: a dead control
    /// is still a control to read past.
    fn set_status_action(&self, action: Option<StatusAction>) {
        self.status_action.set(action);
        let Some(action) = action else {
            self.sidebar_status_action.set_visible(false);
            self.header_status_action.set_visible(false);
            return;
        };
        // Both checked against the shipped theme by eye, not by name: Adwaita 50
        // draws `dialog-information-symbolic` as a *light bulb* — a hint, not a
        // message — while `help-about-symbolic` is the circled "i" this wants.
        let (icon, tooltip) = match action {
            StatusAction::Disconnect => ("system-shutdown-symbolic", "Disconnect"),
            StatusAction::ShowError => ("help-about-symbolic", "Show the full error"),
        };
        for (button, image) in [
            (
                &self.sidebar_status_action,
                &self.sidebar_status_action_icon,
            ),
            (&self.header_status_action, &self.header_status_action_icon),
        ] {
            image.set_icon_name(Some(icon));
            button.set_tooltip_text(Some(tooltip));
            button.update_property(&[gtk::accessible::Property::Label(tooltip)]);
            button.set_visible(true);
        }
        // Only ever one strip on screen; the header half follows the same rule
        // as the chip beside it.
        self.header_status_action.set_visible(self.compact.get());
        self.sidebar_status_action.set_visible(!self.compact.get());
    }

    /// Background work spins beside the status; it no longer rewrites it.
    ///
    /// The label used to be replaced with "Checking latency · 12" for as long
    /// as the sweep ran — minutes, on a large subscription — while the click
    /// still disconnected. The strip now keeps answering the one question it
    /// exists for, and the running work says so with a spinner and its tooltip.
    fn refresh_activity_status(&self) {
        let state = self.state.borrow();
        // Waits the deadline has given up on are still held, so that the next
        // tick does not read their ids as new — but their spinners are down,
        // and counting them here would have the strip announce checks no card
        // is showing.
        let checking = state
            .ui
            .checking
            .keys()
            .filter(|id| state.ui.is_checking(id))
            .count();
        let activity = match (state.ui.operation.as_ref(), checking) {
            (Some(operation), _) => Some(operation.label().to_string()),
            (None, 0) => None,
            (None, 1) => Some("Checking latency…".to_string()),
            (None, count) => Some(format!("Checking latency · {count}")),
        };
        for spinner in [&self.sidebar_status_spinner, &self.header_status_spinner] {
            spinner.set_visible(activity.is_some());
            spinner.set_spinning(activity.is_some());
            spinner.set_tooltip_text(activity.as_deref());
        }
        self.update_sidebar_connection_status(&state);
    }

    /// Does what the button's icon promised when it was painted, rather than
    /// re-deriving an action from a state that may have moved since.
    fn handle_status_action(self: &Rc<Self>) {
        match self.status_action.get() {
            Some(StatusAction::Disconnect) => self.disconnect_if_active(),
            Some(StatusAction::ShowError) => {
                let error = {
                    let state = self.state.borrow();
                    match selected_status(&state.ui) {
                        Status::Error(error) => error,
                        _ => return,
                    }
                };
                self.show_error_details("Connection error", &error);
            }
            None => {}
        }
    }

    /// **Every failure that carries text from the daemon or the system.** One
    /// line, the full text behind Details, and a shortcut to the place that can
    /// fix it when the message names one.
    ///
    /// The rule matters because the alternative used to be arbitrary: a failed
    /// `Connect` got this treatment while a failed `up` on the same profile got
    /// a plain toast, so whether the full error was reachable at all depended on
    /// which button had been pressed.
    fn show_error(self: &Rc<Self>, title: &str, detail: &str) {
        let toast = adw::Toast::new(&summarize_error(title, detail));
        toast.set_priority(adw::ToastPriority::High);
        toast.set_timeout(8);
        let action = ipc::error_action(detail);
        // A rejected certificate is the one failure that asks the user a
        // question, and a question does not belong in a toast that leaves after
        // eight seconds taking the only way to answer it with it. The dialog
        // opens instead of the toast.
        // Only the *first* time: a server that already carries a pin and still
        // fails was not fixed by pinning, and re-opening the dialog on every
        // failed attempt would loop. The offer stays reachable on the toast.
        let unpinned = |controller: &Self, server_id: &str| {
            controller
                .state
                .borrow()
                .subscriptions
                .iter()
                .flat_map(|subscription| subscription.servers.iter())
                .find(|server| server.id == server_id)
                .and_then(|server| server.spec.stream())
                .is_none_or(|stream| stream.pin_sha256.is_none())
        };
        if action == ipc::ErrorAction::TrustCertificate
            && let Some(server_id) = self.state.borrow().ui.failed_id.clone()
            && unpinned(self, &server_id)
        {
            let name = self.server_label(&server_id);
            // The profile whose connection just failed is the thing to retry;
            // without one this was a bare measurement, so measure again.
            let after = match self.state.borrow().ui.selected_profile.clone() {
                profile if !profile.is_empty() => AfterTrust::Reconnect(profile),
                _ => AfterTrust::Measure,
            };
            self.present_trust_dialog(server_id, name, after);
            return;
        }
        let open_settings = action == ipc::ErrorAction::OpenSettings;
        toast.set_button_label(Some(if open_settings {
            "Open Settings"
        } else {
            "Details"
        }));
        // `set_action_name` would need a GActionMap the window does not
        // register; a direct handler keeps this self-contained.
        toast.connect_button_clicked({
            let weak = Rc::downgrade(self);
            let title = title.to_string();
            let detail = detail.to_string();
            move |_| {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                if open_settings {
                    controller.navigate_to(Page::Settings);
                } else {
                    controller.show_error_details(&title, &detail);
                }
            }
        });
        self.toasts.add_toast(toast);
    }

    /// Remember what the daemon said about itself and show it.
    fn set_runtime_info(&self, runtime: Option<ipc::RuntimeInfo>) {
        self.settings.set_runtime_info(runtime.as_ref());
        self.update_core_banner(runtime.as_ref());
        *self.runtime.borrow_mut() = runtime;
    }

    /// Follow a running download until it stops.
    ///
    /// `RuntimeInfo` is deliberately not on the 500 ms status tick — resolving
    /// the core walks `$PATH` and asking about the geo data spawns a process,
    /// neither of which belongs on that clock. So progress is followed on its
    /// own slower timer, and only while something is actually downloading.
    fn poll_geo_progress(self: &Rc<Self>) {
        if self.geo_polling.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        let inflight = Rc::new(Cell::new(false));
        glib::timeout_add_local(Duration::from_millis(700), move || {
            let Some(controller) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            // One read at a time: a slow answer must not queue up behind
            // itself and leave the bar jumping backwards.
            if inflight.get() {
                return glib::ControlFlow::Continue;
            }
            inflight.set(true);
            let client = controller.state.borrow().client.clone();
            let (sender, receiver) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let _ = sender.send(client.runtime_info().ok());
            });
            let weak = weak.clone();
            let inflight = inflight.clone();
            glib::timeout_add_local(Duration::from_millis(40), move || {
                match receiver.try_recv() {
                    Ok(runtime) => {
                        inflight.set(false);
                        let Some(controller) = weak.upgrade() else {
                            return glib::ControlFlow::Break;
                        };
                        let finished = runtime
                            .as_ref()
                            .map(|runtime| !runtime.geo.downloading)
                            .unwrap_or(true);
                        let outcome = runtime
                            .as_ref()
                            .map(|runtime| (runtime.geo.cancelled, runtime.geo.last_error.clone()));
                        controller.set_runtime_info(runtime);
                        if finished {
                            controller.geo_polling.set(false);
                            controller.report_geo_outcome(outcome);
                        }
                        glib::ControlFlow::Break
                    }
                    Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        inflight.set(false);
                        glib::ControlFlow::Break
                    }
                }
            });
            if controller.geo_polling.get() {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }

    /// Say how a finished download ended, once.
    ///
    /// A cancel is not a failure and does not get an error toast; a failure
    /// carries the daemon's own words, which name both digests when a checksum
    /// did not match.
    fn report_geo_outcome(self: &Rc<Self>, outcome: Option<(bool, Option<String>)>) {
        match outcome {
            Some((true, _)) => self.show_message("The geo data download was cancelled"),
            Some((false, Some(error))) => {
                self.show_error("Could not download the geo data", &error)
            }
            Some((false, None)) => {
                self.show_message("Geo data installed — the core can load its lists")
            }
            None => {}
        }
    }

    /// Re-read it after something that would have changed the answer.
    fn refresh_runtime_info(self: &Rc<Self>) {
        let client = self.state.borrow().client.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = sender.send(client.runtime_info().ok());
        });
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(40), move || {
            match receiver.try_recv() {
                Ok(runtime) => {
                    if let Some(controller) = weak.upgrade() {
                        controller.set_runtime_info(runtime);
                    }
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
            }
        });
    }

    /// Offer to install the geo data, saying who will be contacted before
    /// anyone is.
    ///
    /// Looks for a copy already on this machine first — that is faster, needs
    /// no network, and many machines have one from another client. Only if
    /// there is none does it offer the download, and then the dialog names both
    /// URLs and both destinations verbatim, because that is the last point at
    /// which someone can decline.
    fn present_geo_install(self: &Rc<Self>) {
        self.client_job(
            UiOperation::new(UiOperationKind::FindGeoAssets),
            |client| client.find_geo_assets(),
            |controller, result| match result {
                Ok(found) if !found.is_empty() => controller.present_geo_adopt(found),
                // A daemon that cannot look is not a reason not to offer the
                // download; it only means we cannot save the user the bytes.
                Ok(_) => controller.present_geo_download(),
                Err(error) if is_busy(&error) => {}
                Err(error) => {
                    log::debug!("could not look for geo data: {error:#}");
                    controller.present_geo_download();
                }
            },
        );
    }

    /// Offer files already on this machine, which beats fetching 23 MB.
    fn present_geo_adopt(self: &Rc<Self>, found: Vec<oxidom_core::xray::assets::Candidate>) {
        let first = found[0].clone();
        let body = format!(
            "The Xray core accepts the geo data already installed here:\n\n\
             \u{2003}{}\n\
             \u{2003}geoip.dat {}\u{2003}geosite.dat {}\n\n\
             oxidom will copy both files into its own directory, so that a package \
             upgrade or another program cannot take them away. Nothing is downloaded.",
            first.dir,
            human_bytes(first.geoip_bytes),
            human_bytes(first.geosite_bytes),
        );
        let dialog = adw::AlertDialog::new(Some("Use the geo data on this machine?"), Some(&body));
        dialog.add_responses(&[
            ("cancel", "Cancel"),
            ("download", "Download instead"),
            ("use", "Use these"),
        ]);
        dialog.set_response_appearance("use", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("use"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, {
            let weak = Rc::downgrade(self);
            move |dialog, response| {
                dialog.close();
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                match response {
                    "use" => controller.adopt_geo_assets(first.dir.clone()),
                    "download" => controller.present_geo_download(),
                    _ => {}
                }
            }
        });
        dialog.present(Some(&self.window));
    }

    /// The confirmation before anything leaves this machine.
    ///
    /// It names GitHub explicitly and says it may be blocked. That is not
    /// boilerplate for this program in particular: its users are behind exactly
    /// the filtering that blocks these hosts, and a spinner that fails after
    /// twenty seconds explains nothing. Where a tunnel is already up, the fetch
    /// can go through it, which turns "blocked" from a dead end into a
    /// checkbox.
    fn present_geo_download(self: &Rc<Self>) {
        use oxidom_core::xray::assets::GeoAsset;

        let dir = self
            .runtime
            .borrow()
            .as_ref()
            .map(|runtime| runtime.geo.dir.clone())
            .filter(|dir| !dir.is_empty())
            .unwrap_or_else(|| "the daemon's data directory".to_string());
        // Only offer the tunnel when there is one: a checkbox that cannot work
        // is worse than no checkbox.
        let connected = self
            .state
            .borrow()
            .ui
            .sessions
            .iter()
            .any(|session| session.state == "connected");

        let body = format!(
            "oxidom will contact GitHub over HTTPS and fetch two files:\n\n\
             \u{2003}{}\n\u{2003}\u{2192} {dir}/geoip.dat\u{2003}about 22 MB\n\n\
             \u{2003}{}\n\u{2003}\u{2192} {dir}/geosite.dat\u{2003}about 2 MB\n\n\
             These are the IP and domain lists the core reads to tell your local network \
             apart from the tunnel. Every configuration oxidom generates needs them.\n\n\
             GitHub is blocked on some networks and in some countries. If this fails, that \
             is the likely reason{}\n\n\
             Each file is checked against the SHA-256 published beside it. That catches a \
             broken download, not a compromised release \u{2014} there is no signature to \
             verify. Nothing is executed.",
            GeoAsset::GeoIp.url(),
            GeoAsset::GeoSite.url(),
            if connected {
                " \u{2014} \"Through the tunnel\" below sends the request over your \
                 connection instead."
            } else {
                ", and connecting first would let oxidom fetch them through the tunnel."
            },
        );

        let dialog = adw::AlertDialog::new(Some("Download the geo data?"), Some(&body));
        dialog.add_responses(&[("cancel", "Cancel"), ("download", "Download")]);
        if connected {
            dialog.add_response("tunnel", "Through the Tunnel");
            dialog.set_response_appearance("tunnel", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("tunnel"));
        } else {
            dialog.set_response_appearance("download", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("download"));
        }
        dialog.set_close_response("cancel");
        dialog.connect_response(None, {
            let weak = Rc::downgrade(self);
            move |dialog, response| {
                dialog.close();
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                match response {
                    "download" => controller.start_geo_download(false),
                    "tunnel" => controller.start_geo_download(true),
                    _ => {}
                }
            }
        });
        dialog.present(Some(&self.window));
    }

    /// Start the download. The daemon returns at once and reports its progress
    /// through the poll, so this holds no operation slot — Cancel has to stay
    /// clickable while 23 MB comes down.
    fn start_geo_download(self: &Rc<Self>, through_tunnel: bool) {
        let client = self.state.borrow().client.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = sender.send(client.download_geo_assets(through_tunnel));
        });
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(40), move || {
            match receiver.try_recv() {
                Ok(result) => {
                    let Some(controller) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    match result {
                        Ok(()) => controller.poll_geo_progress(),
                        Err(error) => controller
                            .show_error("Could not download the geo data", &format!("{error:#}")),
                    }
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
            }
        });
    }

    /// Stop one.
    fn cancel_geo_download(self: &Rc<Self>) {
        let client = self.state.borrow().client.clone();
        std::thread::spawn(move || {
            if let Err(error) = client.cancel_geo_download() {
                log::debug!("could not cancel the geo download: {error:#}");
            }
        });
    }

    /// Install files already on this machine.
    fn adopt_geo_assets(self: &Rc<Self>, dir: String) {
        self.client_job(
            UiOperation::new(UiOperationKind::InstallGeoAssets),
            move |client| client.adopt_geo_assets(&dir),
            |controller, result| match result {
                Ok(()) => {
                    controller.show_message("Geo data installed — the core can load its lists");
                    controller.refresh_runtime_info();
                }
                Err(error) if is_busy(&error) => {}
                Err(error) => {
                    controller.show_error("Could not install the geo data", &format!("{error:#}"))
                }
            },
        );
    }

    /// What to call a server in a sentence. Falls back to the id, which is at
    /// least something the user can match against a card.
    fn server_label(&self, server_id: &str) -> String {
        self.state
            .borrow()
            .subscriptions
            .iter()
            .flat_map(|subscription| subscription.servers.iter())
            .find(|server| server.id == server_id)
            .map(|server| format!("{} ({}:{})", server.name, server.address, server.port))
            .unwrap_or_else(|| server_id.to_string())
    }

    /// Show what a server presents and offer to accept it.
    ///
    /// Two round trips to the daemon, on a worker thread each: read the
    /// certificate, then — only if the user says so — pin the value that was
    /// read. The second call passes the fingerprint that was *displayed*, never
    /// a fresh one, because pinning whatever a later handshake returns is the
    /// substitution a pin exists to prevent.
    fn present_trust_dialog(
        self: &Rc<Self>,
        server_id: String,
        server_name: String,
        after: AfterTrust,
    ) {
        let client = self.state.borrow().client.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let id = server_id.clone();
        std::thread::spawn(move || {
            let _ = sender.send(client.inspect_certificate(&id));
        });
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let result = match poll_worker(&receiver) {
                WorkerPoll::Waiting => return glib::ControlFlow::Continue,
                WorkerPoll::Ready(result) => result,
                WorkerPoll::Lost => Err(anyhow!(WorkerLost)),
            };
            let Some(controller) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            match result {
                Ok(sha256) => {
                    controller.ask_to_trust(&server_id, &server_name, &sha256, after.clone())
                }
                // Reading the certificate is itself a connection, and it fails
                // for all the ordinary reasons. Saying so beats a dialog with
                // an empty fingerprint in it.
                Err(error) => {
                    controller.show_error("Could not read the certificate", &format!("{error:#}"))
                }
            }
            glib::ControlFlow::Break
        });
    }

    fn ask_to_trust(
        self: &Rc<Self>,
        server_id: &str,
        server_name: &str,
        sha256: &str,
        after: AfterTrust,
    ) {
        let readable = sha256
            .as_bytes()
            .chunks(16)
            .map(|line| {
                line.chunks(2)
                    .map(|pair| String::from_utf8_lossy(pair).to_string())
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let dialog = adw::AlertDialog::new(
            Some("Trust this certificate?"),
            Some(&format!(
                "{server_name} presents a certificate this build will not accept on its own — \
                 Xray 26 removed the setting that skipped verification.\n\n\
                 SHA-256\n{readable}\n\n\
                 Accepting pins this one certificate for this server. Any other certificate, \
                 including a replacement, will be refused until you look again.",
            )),
        );
        dialog.set_body_use_markup(false);
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("trust", "Trust");
        // Not `Suggested`: accepting a certificate nobody checked is the thing
        // this dialog exists to slow down, so the button does not invite a
        // reflex.
        dialog.set_response_appearance("trust", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, {
            let weak = Rc::downgrade(self);
            let server_id = server_id.to_string();
            let sha256 = sha256.to_string();
            move |dialog, response| {
                dialog.close();
                if response != "trust" {
                    return;
                }
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                controller.trust_certificate(&server_id, &sha256, after.clone());
            }
        });
        dialog.present(Some(&self.window));
    }

    fn trust_certificate(self: &Rc<Self>, server_id: &str, sha256: &str, after: AfterTrust) {
        let client = self.state.borrow().client.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let id = server_id.to_string();
        let pin = sha256.to_string();
        std::thread::spawn(move || {
            let _ = sender.send(client.trust_certificate(&id, &pin));
        });
        let weak = Rc::downgrade(self);
        let server_id = server_id.to_string();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let result = match poll_worker(&receiver) {
                WorkerPoll::Waiting => return glib::ControlFlow::Continue,
                WorkerPoll::Ready(result) => result,
                WorkerPoll::Lost => Err(anyhow!(WorkerLost)),
            };
            let Some(controller) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            match result {
                Ok(()) => {
                    controller.show_message("Certificate trusted");
                    // Trusting is never the goal in itself: it was asked for
                    // because something failed, and leaving that failure on
                    // screen makes the user repeat the action they already
                    // took.
                    match &after {
                        // Down first: a session that failed is still *up*, and
                        // `up` on a live profile refuses rather than redialling
                        // — which would report the pin as not having helped.
                        AfterTrust::Reconnect(profile) => {
                            controller.reconnect_profile(profile.clone())
                        }
                        AfterTrust::Measure => controller.probe_one(server_id.clone(), false),
                    }
                }
                Err(error) => {
                    controller.show_error("Could not trust the certificate", &format!("{error:#}"))
                }
            }
            glib::ControlFlow::Break
        });
    }

    /// Full error text, selectable and dismissible.
    fn show_error_details(self: &Rc<Self>, title: &str, detail: &str) {
        let dialog = adw::AlertDialog::new(Some(title), Some(detail));
        dialog.set_body_use_markup(false);
        dialog.add_response("close", "Close");
        if ipc::error_action(detail) == ipc::ErrorAction::OpenSettings {
            dialog.add_response("settings", "Open Settings");
            dialog.set_response_appearance("settings", adw::ResponseAppearance::Suggested);
        }
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        dialog.connect_response(None, {
            let weak = Rc::downgrade(self);
            move |dialog, response| {
                dialog.close();
                if response == "settings"
                    && let Some(controller) = weak.upgrade()
                {
                    controller.navigate_to(Page::Settings);
                }
            }
        });
        dialog.present(Some(&self.window));
    }

    /// Records an error the caller has already surfaced, so the poll loop does
    /// not toast the identical message a second time. Both paths format the
    /// same anyhow error with `{:#}`, so the strings match exactly.
    fn mark_error_notified(&self, message: &str) {
        self.state.borrow_mut().ui.notified_error = Some(message.to_string());
    }

    fn navigate_to(&self, page: Page) {
        // Drive the sidebar rather than the stack directly, so its selection
        // and the visible page cannot disagree.
        if let Some(row) = self.sidebar_list.row_at_index(page.index()) {
            self.sidebar_list.select_row(Some(&row));
        }
    }

    /// Neutral news: a success, a refusal already stated in full, a fact the
    /// user should notice. Never a failure carrying an error string — that is
    /// [`Self::show_error`], which keeps the untruncated text reachable.
    fn show_message(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }
}

fn gui_proxy_marker() -> Option<std::path::PathBuf> {
    paths::data_dir()
        .ok()
        .map(|dir| dir.join("gui-proxy-applied"))
}

fn gui_proxy_marker_exists() -> bool {
    gui_proxy_marker().is_some_and(|marker| marker.exists())
}

fn desired_system_proxy_endpoint(
    enabled: bool,
    compatibility_status: &Status,
    sessions: &[ipc::SessionInfo],
    compatibility_socks_port: u16,
    compatibility_http_port: u16,
) -> Option<(std::net::Ipv4Addr, u16, u16)> {
    if !enabled {
        return None;
    }
    if sessions.is_empty() {
        // Compatibility with a daemon that predates the additive session list.
        return (*compatibility_status == Status::Connected).then_some((
            std::net::Ipv4Addr::LOCALHOST,
            compatibility_socks_port,
            compatibility_http_port,
        ));
    }
    sessions
        .iter()
        .find(|session| session.owns_system_proxy && session.state == "connected")
        .and_then(|session| {
            session
                .address
                .parse()
                .ok()
                .map(|address| (address, session.socks_port, session.http_port))
        })
}

/// Toasts are one line; anyhow cause chains are not. Keep the headline and
/// enough of the detail to recognize the failure, leaving the rest to Details.
fn summarize_error(title: &str, detail: &str) -> String {
    const MAX_DETAIL: usize = 110;
    let detail = detail.trim();
    if detail.is_empty() {
        return title.to_string();
    }
    if detail.chars().count() <= MAX_DETAIL {
        return format!("{title}: {detail}");
    }
    // Truncate on a char boundary — error text carries non-ASCII (e.g. the
    // "›" in "Settings › Xray binary", and em dashes from our own messages).
    let cut: String = detail.chars().take(MAX_DETAIL).collect();
    let cut = cut.trim_end();
    format!("{title}: {cut}…")
}

fn session_row_tone(state: SessionRowState) -> StatusTone {
    match state {
        SessionRowState::Stopped => StatusTone::Neutral,
        SessionRowState::Connecting => StatusTone::Working,
        SessionRowState::Connected => StatusTone::Connected,
        SessionRowState::Error => StatusTone::Error,
        SessionRowState::Unknown => StatusTone::Neutral,
    }
}

fn session_row_state_label(state: SessionRowState) -> &'static str {
    match state {
        SessionRowState::Stopped => "Disconnected",
        SessionRowState::Connecting => "Connecting",
        SessionRowState::Connected => "Connected",
        SessionRowState::Error => "Error",
        SessionRowState::Unknown => "Unknown",
    }
}

fn set_status_tone<W: IsA<gtk::Widget>>(widget: &W, tone: StatusTone) {
    let widget = widget.as_ref();
    for class in [
        "status-neutral",
        "status-working",
        "status-connected",
        "status-error",
    ] {
        widget.remove_css_class(class);
    }
    widget.add_css_class(match tone {
        StatusTone::Neutral => "status-neutral",
        StatusTone::Working => "status-working",
        StatusTone::Connected => "status-connected",
        StatusTone::Error => "status-error",
    });
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        r#"
        headerbar { min-height: 34px; padding: 0 4px; }
        headerbar button { min-height: 26px; min-width: 26px; padding: 2px 6px; }
        headerbar button.header-icon-button,
        headerbar menubutton.header-icon-button > button {
            min-width: 32px;
            min-height: 32px;
            padding: 0;
            margin: 0;
        }
        headerbar button.header-icon-button image,
        headerbar menubutton.header-icon-button > button image {
            -gtk-icon-size: 16px;
            padding: 0;
            margin: 0;
        }
        headerbar button.header-status,
        headerbar button.header-status:disabled {
            min-width: 0;
            min-height: 24px;
            padding: 2px 8px;
            border-radius: 8px;
            box-shadow: none;
            opacity: 1;
            font-weight: 500;
            font-size: 0.85em;
        }
        headerbar button.header-status .server-flag,
        headerbar button.header-status .server-globe {
            min-width: 0;
            min-height: 0;
            border-radius: 3px;
        }
        headerbar button.header-status.status-working { color: @accent_color; background: alpha(@accent_color, 0.16); }
        headerbar button.header-status.status-connected { color: @success_color; background: alpha(@success_color, 0.16); }
        headerbar button.header-status.status-connected:hover { background: alpha(@success_color, 0.22); }
        headerbar button.header-status.status-connected:active { background: alpha(@success_color, 0.28); }
        headerbar button.header-status.status-error { color: @error_color; background: alpha(@error_color, 0.17); }
        .header-status-icon { color: currentColor; -gtk-icon-size: 16px; }
        headerbar button.header-status label { padding: 0; margin: 0; }
        headerbar button.header-status label.latency-stale,
        .sidebar-status label.latency-stale { opacity: 0.6; }
        headerbar menubutton.profile-switcher > button {
            min-width: 0;
            min-height: 24px;
            padding: 2px 8px;
            border-radius: 8px;
            box-shadow: none;
            font-weight: 500;
            font-size: 0.85em;
        }
        /* A `listbox` node carries an opaque view background of its own. Left
           alone it paints a square over the popover's rounded corners, which
           is the classic "why is my menu clipped" artefact — the popover is
           round, the sheet on top of it is not. */
        .profile-switcher-list { background: transparent; padding: 4px; min-width: 150px; }
        .profile-switcher-list > row { border-radius: 8px; }
        .profile-switcher-dot { font-size: 0.72em; }
        .profile-switcher-dot.status-neutral { color: alpha(@window_fg_color, 0.46); }
        .profile-switcher-dot.status-working { color: @accent_color; }
        .profile-switcher-dot.status-connected { color: @success_color; }
        .profile-switcher-dot.status-error { color: @error_color; }
        button.flat { min-height: 24px; font-weight: normal; }
        button.pill { font-weight: 500; }
        /* The group title doubles as the expander, but it is a heading first: a
           full-width slab lighting up under the pointer reads as a selection the
           user did not make, and the chevron beside it already says the group
           folds. So it keeps the click and loses the paint. The negative margin
           puts the text back on the same vertical line as the cards underneath
           it; the focus ring stays, or the keyboard would lose the title
           entirely. */
        button.subscription-toggle {
            padding: 2px 8px;
            margin-left: -8px;
            border-radius: 10px;
            min-height: 0;
            background: none;
            box-shadow: none;
        }
        button.subscription-toggle:hover,
        button.subscription-toggle:active { background: none; box-shadow: none; }
        button.subscription-toggle:focus-visible {
            outline: 2px solid @accent_color;
            outline-offset: -2px;
        }
        .group-chip-bar { padding: 0 2px 2px; }
        .group-chip { min-height: 28px; padding: 2px 14px; font-weight: 500; }
        menubutton.group-chip > button { min-height: 28px; padding: 2px 14px; font-weight: 500; border-radius: 999px; }
        .group-chip-bar > separator { margin: 4px 4px; }
        .filter-pill image { -gtk-icon-size: 14px; }
        /* The dot in the label already says "modified"; the accent makes it
           visible without reading, and both survive a theme that ignores one. */
        .group-chip-modified { color: @accent_color; }
        .group-chip-add image { -gtk-icon-size: 14px; }
        /* The scope switcher is an AdwToggleGroup, which brings its own frame
           and its own sliding indicator; it only needs to match the height of
           the pills either side of it. */
        .group-chip-bar > toggle-group { min-height: 30px; }
        .group-chip-bar > toggle-group toggle { padding: 2px 12px; font-weight: 500; }
        /* One menu for whichever scope is selected, so it keeps its place in the
           row instead of travelling with the selection. */
        menubutton.group-chip-menu > button {
            min-height: 28px;
            min-width: 28px;
            padding: 2px 4px;
            border-radius: 999px;
        }
        menubutton.group-chip-menu > button image { -gtk-icon-size: 16px; }
        .group-chip-hint { padding: 0 4px 2px; }
        /* A card of its own rather than a loose row: it speaks for the scope
           above it, not for the subscription block that follows. */
        .group-connect-bar { padding: 10px 14px; border-radius: 12px; background: alpha(@window_fg_color, 0.04); }
        .group-connect-bar > label { font-weight: 500; }
        .compact-search { min-height: 28px; }
        .compact-search text { padding-top: 1px; padding-bottom: 1px; }

        .sidebar { background: alpha(@window_fg_color, 0.035); }
        .sidebar-status { padding: 8px 10px; border-radius: 10px; background: alpha(@window_fg_color, 0.055); }
        .sidebar-status.status-neutral { color: alpha(@window_fg_color, 0.72); }
        .sidebar-status.status-working { color: @accent_color; background: alpha(@accent_color, 0.12); }
        .sidebar-status.status-connected { color: @success_color; background: alpha(@success_color, 0.12); }
        .sidebar-status.status-error { color: @error_color; background: alpha(@error_color, 0.14); }
        .sidebar-status-icon { color: currentColor; -gtk-icon-size: 18px; }
        /* Its own target beside the strip, not part of it: a hover that lit the
           whole strip would say the text and the action were one button, which
           is the confusion the two-target split exists to end. */
        .sidebar-status-action { padding: 6px; min-width: 28px; min-height: 28px; }

        /* Menu rows, not toolbar buttons: full width, text left, so the popover
           reads as a list of actions rather than a strip of pills. */
        .server-context-item { padding: 6px 12px; min-height: 32px; }

        .server-card {
            border: none;
            border-radius: 12px;
            background: alpha(@window_fg_color, 0.05);
            box-shadow: inset 0 0 0 1px alpha(@window_fg_color, 0.07);
        }
        .server-card:hover { background: alpha(@window_fg_color, 0.09); }
        .server-card.selected-server { box-shadow: inset 0 0 0 1px alpha(@accent_color, 0.65); }
        .server-card.active-server { background: alpha(@success_color, 0.08); box-shadow: inset 0 0 0 1px alpha(@success_color, 0.75); }
        .server-card-header { min-height: 56px; padding: 0; background: transparent; border: none; box-shadow: none; }
        .server-card-header:hover { background: transparent; }
        .server-card-detail { padding: 4px 12px 8px; }
        .server-card-detail button { min-height: 22px; min-width: 22px; padding: 2px 8px; font-weight: normal; }
        .server-card-detail button.server-action { min-height: 28px; min-width: 28px; padding: 0; }
        .server-action image { -gtk-icon-size: 18px; }
        .server-meta { font-size: 0.85em; }
        .server-detail-name { font-weight: 600; font-size: 0.9em; }

        .server-flag { min-width: 28px; min-height: 28px; border-radius: 4px; }
        .server-globe { min-width: 28px; min-height: 28px; }
        .server-name { font-weight: 600; font-size: 0.98em; }
        .server-subtitle { font-size: 0.82em; }
        /* The pill is always there, including while a check runs: the spinner
           sits in a box carrying the same class, so it appears inside the badge
           instead of the badge vanishing and the row twitching on every
           re-check. The class must stay on the box — a spinner is animated by
           rotating its whole node, background included. */
        .latency-badge { border-radius: 999px; padding: 3px 8px; font-size: 0.75em; font-weight: 500; background: alpha(@window_fg_color, 0.07); }
        .latency-badge.latency-offline { font-size: 1.05em; padding: 1px 8px; font-weight: 700; }
        .latency-spinner { color: @accent_color; }
        .latency-reachable { color: @accent_color; background: alpha(@accent_color, 0.12); }
        /* Measured through the tunnel: a fact about the connection in use, not
           about the server on its own. Worth its own colour. */
        .latency-tunnel { color: @success_color; background: alpha(@success_color, 0.14); }
        /* Scoped to the card badge on purpose — the .latency-stale rule above
           is scoped to the headerbar and the sidebar and does not reach here. */
        .latency-badge.latency-stale { opacity: 0.55; }
        .latency-error { color: @error_color; background: alpha(@error_color, 0.13); }
        .latency-offline { color: @warning_color; background: alpha(@warning_color, 0.13); }
        .status-badge { border-radius: 999px; padding: 3px 8px; font-size: 0.75em; font-weight: 600; }
        .status-badge.status-neutral { color: alpha(@window_fg_color, 0.68); background: alpha(@window_fg_color, 0.07); }
        .status-badge.status-working { color: @accent_color; background: alpha(@accent_color, 0.13); }
        .status-badge.status-connected { color: @success_color; background: alpha(@success_color, 0.14); }
        .status-badge.status-error { color: @error_color; background: alpha(@error_color, 0.13); }
        /* Two kinds left, and both earn their colour: a warning and a reading.
           The five that went — interface, inbound, system proxy, "proxy only",
           pool — were facts painted like alerts, and a healthy session read as
           four things going wrong. They are labelled rows inside the expander
           now. */
        .session-chip { border-radius: 999px; padding: 3px 8px; font-size: 0.75em; background: alpha(@window_fg_color, 0.07); }
        .session-chip-stale { color: @warning_color; background: alpha(@warning_color, 0.13); }
        .session-chip-latency { color: @success_color; background: alpha(@success_color, 0.14); }
        .dns-leak-row { background: alpha(@warning_color, 0.10); }
        .dns-leak-row > box > box > label.title { color: @warning_color; }
        .dns-leak-icon { color: @warning_color; }
        /* An inset ring rather than a border: `.server-card` is a fixed-height
           frame with overflow hidden, and a real border would eat 2px of the
           content it clips. */
        .server-card.failed-server { box-shadow: inset 0 0 0 1px alpha(@error_color, 0.55); }
        "#,
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use oxidom_core::ipc::SessionInfo;
    use oxidom_core::xray::core::Status;

    use super::{
        ResponsiveMode, SearchState, StatusAction, WorkerPoll, desired_system_proxy_endpoint,
        poll_worker, responsive_mode_for_width, status_action_for, summarize_error,
    };

    /// `try_recv` reports "nothing yet" and "nobody will ever send" as two
    /// different errors, and three timers here treated them alike — `let Ok(..)
    /// else { Continue }` reads as "wait for it", which for a worker that has
    /// gone is forever. The timer stayed on the main loop for the life of the
    /// process, and the operation it belonged to was never completed: the
    /// settings spinner stayed up, leaving Apply and Reset insensitive for the
    /// rest of the session.
    #[test]
    fn a_worker_that_will_never_answer_is_not_something_to_wait_for() {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<u8>(1);
        assert!(matches!(poll_worker(&receiver), WorkerPoll::Waiting));

        sender.send(7).expect("the receiver is alive");
        assert!(matches!(poll_worker(&receiver), WorkerPoll::Ready(7)));

        // Still alive, still empty: waiting is the honest answer.
        assert!(matches!(poll_worker(&receiver), WorkerPoll::Waiting));

        // The worker ends — a return, or a panic — and drops its sender.
        drop(sender);
        assert!(matches!(poll_worker(&receiver), WorkerPoll::Lost));
        // And stays lost, so a caller cannot poll its way back to hope.
        assert!(matches!(poll_worker(&receiver), WorkerPoll::Lost));
    }

    /// A value already in the channel outlives the sender, and must be read
    /// rather than mistaken for the loss that follows it.
    #[test]
    fn a_result_sent_before_the_worker_ended_is_still_delivered() {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<u8>(1);
        sender.send(3).expect("the receiver is alive");
        drop(sender);
        assert!(matches!(poll_worker(&receiver), WorkerPoll::Ready(3)));
        assert!(matches!(poll_worker(&receiver), WorkerPoll::Lost));
    }

    #[test]
    fn system_proxy_follows_only_a_live_owner() {
        let sessions = [
            SessionInfo {
                profile: "home".to_string(),
                state: "connected".to_string(),
                address: "127.31.8.1".to_string(),
                socks_port: 10808,
                http_port: 10809,
                owns_system_proxy: true,
                ..SessionInfo::default()
            },
            SessionInfo {
                profile: "work".to_string(),
                state: "connected".to_string(),
                address: "127.72.14.1".to_string(),
                socks_port: 20808,
                http_port: 20809,
                ..SessionInfo::default()
            },
        ];

        assert_eq!(
            desired_system_proxy_endpoint(true, &Status::Disconnected, &sessions, 1, 2),
            Some(("127.31.8.1".parse().unwrap(), 10808, 10809))
        );

        let mut dead = sessions;
        dead[0].state = "error".to_string();
        assert_eq!(
            desired_system_proxy_endpoint(true, &Status::Connected, &dead, 1, 2),
            None
        );
    }

    /// The strip reports and the button acts, so the button exists only where
    /// there is an act. `Connecting` deliberately offers nothing: the connect
    /// job holds the operation slot, and a Disconnect sent into it is refused
    /// and used to come back looking like the tunnel had failed.
    #[test]
    fn a_status_offers_an_action_only_where_there_is_one() {
        assert_eq!(status_action_for(&Status::Disconnected), None);
        assert_eq!(status_action_for(&Status::Connecting), None);
        assert_eq!(
            status_action_for(&Status::Connected),
            Some(StatusAction::Disconnect)
        );
        assert_eq!(
            status_action_for(&Status::Error("no such file".into())),
            Some(StatusAction::ShowError)
        );
    }

    #[test]
    fn summarize_error_truncates_on_char_boundaries() {
        assert_eq!(summarize_error("Failed", ""), "Failed");
        assert_eq!(summarize_error("Failed", "  short  "), "Failed: short");

        // Multi-byte throughout: a byte-based cut would panic here.
        let long = "путь ".repeat(40);
        let summary = summarize_error("Could not connect", &long);
        assert!(summary.starts_with("Could not connect: путь"), "{summary}");
        assert!(summary.ends_with('…'), "{summary}");
        assert!(summary.chars().count() < long.chars().count(), "{summary}");
    }

    #[test]
    fn responsive_modes_include_their_upper_boundaries() {
        assert_eq!(responsive_mode_for_width(320.0), ResponsiveMode::Compact);
        assert_eq!(responsive_mode_for_width(680.0), ResponsiveMode::Compact);
        assert_eq!(responsive_mode_for_width(700.0), ResponsiveMode::Compact);
        assert_eq!(responsive_mode_for_width(701.0), ResponsiveMode::Wide);
    }

    #[test]
    fn search_state_clamps_cursor_and_selection_to_unicode_length() {
        assert_eq!(
            SearchState::new("Eesti 🇪🇪".to_string(), 99, Some((-4, 99))),
            SearchState {
                text: "Eesti 🇪🇪".to_string(),
                cursor: 8,
                selection: Some((0, 8)),
            }
        );
    }

    #[test]
    fn search_state_drops_empty_selection_without_moving_cursor() {
        assert_eq!(
            SearchState::new("server".to_string(), 3, Some((3, 3))),
            SearchState {
                text: "server".to_string(),
                cursor: 3,
                selection: None,
            }
        );
    }
}
