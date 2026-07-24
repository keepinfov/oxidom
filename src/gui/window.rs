use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;

use crate::APP_ID;
use crate::config::Config;
use crate::ipc::{self, ProbeState, StatusInfo};
use crate::model::Subscription;
use crate::xray::core::Status;
use crate::{paths, sysproxy};

use super::client::DaemonClient;
use super::operation::{UiOperation, UiOperationKind};
use super::server_card::LatencyState;
use super::sidebar::{Page, Sidebar};
use super::tray::{OxidomTray, TrayCommand};
use super::views::logs::LogsView;
use super::views::servers::ServersView;
use super::views::settings::{SettingsValues, SettingsView};
use super::views::subscriptions::SubscriptionsView;

type SettingsCallback = Rc<dyn Fn(SettingsValues)>;
type ShortcutHandler = Box<dyn Fn(&Rc<Controller>)>;

const SIDEBAR_BREAKPOINT_WIDTH: u32 = 700;

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

/// One round of daemon polling, produced off the main thread.
struct PolledSnapshot {
    status: StatusInfo,
    probe: ProbeState,
    logs: Vec<String>,
}

struct AppState {
    client: DaemonClient,
    subscriptions: Vec<Subscription>,
    /// Card the user is inspecting/expanded. Also the target of the header
    /// Connect button. Distinct from the server that is actually connected.
    selected_id: Option<String>,
    /// Server the tunnel is (optimistically) running for; drives the highlight.
    connected_id: Option<String>,
    latencies: HashMap<String, Option<u32>>,
    /// Last successful measurement of the connected server. Shown in the
    /// status chip even while a periodic re-probe fails, so the reading does
    /// not flicker away; reset when the connection changes.
    last_active_latency: Option<u32>,
    checking: HashSet<String>,
    /// Ids whose failed probe should raise a toast (explicit per-card ping).
    notify_probe: HashSet<String>,
    operation: Option<UiOperation>,
    /// Optimistic status shown while a job is in flight.
    pending_status: Option<Status>,
    /// Latest status reported by the daemon.
    daemon_status: Status,
    /// Last daemon error already shown to the user, so the 500 ms poll does
    /// not re-toast the same failure on every tick.
    notified_error: Option<String>,
}

struct Controller {
    window: adw::ApplicationWindow,
    state: Rc<RefCell<AppState>>,
    split: adw::OverlaySplitView,
    header: adw::HeaderBar,
    stack: gtk::Stack,
    search: gtk::SearchEntry,
    compact_search: gtk::SearchEntry,
    search_bar: gtk::SearchBar,
    search_toggle: gtk::ToggleButton,
    sidebar_toggle: gtk::Button,
    header_status: gtk::Button,
    header_status_icon: gtk::Image,
    header_status_flag: gtk::Box,
    header_status_label: gtk::Label,
    header_status_spinner: gtk::Spinner,
    subscription_actions: gtk::Box,
    settings_actions: gtk::Box,
    compact: Rc<Cell<bool>>,
    search_state: RefCell<SearchState>,
    syncing_search: Cell<bool>,
    sidebar_status: gtk::Button,
    sidebar_status_icon: gtk::Image,
    sidebar_status_label: gtk::Label,
    sidebar_list: gtk::ListBox,
    servers: ServersView,
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
    /// Last (connected, text) pushed to the tray, to skip no-op updates.
    tray_pushed: RefCell<(bool, String)>,
    /// True while this GUI holds the GNOME system proxy applied.
    proxy_applied: Cell<bool>,
    /// Last (active, connecting) pair pushed to the cards, to avoid an
    /// O(cards) pass on every poll tick.
    applied_connection: RefCell<(Option<String>, Option<String>)>,
    poll_in_flight: Arc<AtomicBool>,
    poll_snapshot: Arc<Mutex<Option<PolledSnapshot>>>,
}

pub fn build(app: &adw::Application, background: bool) -> Option<adw::ApplicationWindow> {
    install_css();
    #[cfg(debug_assertions)]
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display)
            .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data"));
    }
    gtk::Window::set_default_icon_name(APP_ID);

    let client = match DaemonClient::connect_any() {
        Ok(client) => client,
        Err(error) => {
            let dialog = adw::MessageDialog::new(
                None::<&gtk::Window>,
                Some("oxidom daemon unavailable"),
                Some(&format!("{error:#}")),
            );
            dialog.add_responses(&[("quit", "Quit"), ("retry", "Try Again")]);
            dialog.set_response_appearance("retry", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("retry"));
            // Escape must quit, not dismiss: the hold guard is already taken
            // in `gui::run`, so a dismissed dialog would otherwise leave a
            // held process with no window and no tray to close it with.
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
            return None;
        }
    };
    let subscriptions_snapshot = client.subscriptions().unwrap_or_default();
    let initial_status = client.status().unwrap_or_default();
    let initial_config = client.settings().unwrap_or_default();
    // A daemon older than RuntimeInfo answers UnknownMethod; `None` just
    // leaves the settings rows unlocked and the effective path unknown.
    let initial_runtime = client.runtime_info().ok();
    let selected_id = initial_status.active_id.clone();
    let state = Rc::new(RefCell::new(AppState {
        client,
        subscriptions: subscriptions_snapshot,
        selected_id,
        connected_id: initial_status.active_id.clone(),
        latencies: HashMap::new(),
        last_active_latency: None,
        checking: HashSet::new(),
        notify_probe: HashSet::new(),
        operation: None,
        pending_status: None,
        daemon_status: initial_status.to_status(),
        // Left unset on purpose: a GUI opening onto an already-broken daemon
        // should toast once, on its first poll.
        notified_error: None,
    }));

    let servers = ServersView::new();
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
    stack.add_named(&servers.root, Some(Page::General.stack_name()));
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
    settings.set_runtime_info(initial_runtime.as_ref());
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
        .tooltip_text("Connection status")
        .visible(false)
        .css_classes(["header-status"])
        .build();
    header_status.update_property(&[gtk::accessible::Property::Label("Connection status")]);

    let header = adw::HeaderBar::new();
    header.pack_start(&sidebar_toggle);
    header.pack_start(&search_toggle);
    header.pack_start(&header_status);
    header.pack_start(&search);
    header.pack_end(&subscription_actions);
    header.pack_end(&settings_actions);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&search_bar);
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
            connected: false,
            status_text: "Disconnected".to_string(),
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

    let controller = Rc::new(Controller {
        window: window.clone(),
        state,
        split,
        header,
        stack,
        search,
        compact_search,
        search_bar,
        search_toggle,
        sidebar_toggle,
        header_status,
        header_status_icon,
        header_status_flag,
        header_status_label,
        header_status_spinner,
        subscription_actions,
        settings_actions,
        compact: Rc::new(Cell::new(false)),
        search_state: RefCell::new(SearchState::default()),
        syncing_search: Cell::new(false),
        sidebar_status: sidebar.status_button,
        sidebar_status_icon: sidebar.status_icon,
        sidebar_status_label: sidebar.status_label,
        sidebar_list: sidebar.list,
        servers,
        subscriptions,
        settings,
        logs,
        toasts,
        close_after_apply: Cell::new(false),
        quit_after_close: Cell::new(false),
        tray: RefCell::new(tray_handle),
        tray_commands,
        tray_pushed: RefCell::new((false, String::new())),
        proxy_applied: Cell::new(gui_proxy_marker_exists()),
        applied_connection: RefCell::new((None, None)),
        poll_in_flight: Arc::new(AtomicBool::new(false)),
        poll_snapshot: Arc::new(Mutex::new(None)),
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
    controller.rebuild_views();
    controller.refresh_status();
    controller.add_breakpoint();
    controller.start_timer();

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
                    move |_| {
                        if let Some(controller) = weak.upgrade() {
                            controller.push_servers_width();
                        }
                    }
                });
            }
            controller.push_servers_width();
        }
    });

    if !background {
        window.present();
    }

    // Repair a system proxy left over from a previous GUI run and reflect
    // the daemon's current connection on the cards.
    controller.reconcile_system_proxy();
    controller.sync_connection_cards();
    Some(window)
}

fn set_window_icon(window: &adw::ApplicationWindow) {
    let icon = include_bytes!("../../data/dev.keepinfov.oxidom.svg");
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
            Page::General,
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

    /// Puts the cursor in whichever search entry the current layout uses.
    fn focus_search(self: &Rc<Self>) {
        self.navigate_to(Page::General);
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
                if !controller.compact.get() || !controller.is_general_page() {
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
                if controller.compact.get() && controller.is_general_page() && !bar.is_search_mode()
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
        self.header_status.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.handle_status_clicked();
                }
            }
        });
        self.sidebar_status.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.handle_status_clicked();
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
        let dialog = adw::MessageDialog::new(
            Some(&self.window),
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
        dialog.present();
    }

    fn show_page(&self, page: Page) {
        if self.is_general_page() {
            self.remember_visible_search();
        }
        self.stack.set_visible_child_name(page.stack_name());
        self.sync_search_chrome();
        if self.split.is_collapsed() {
            self.split.set_show_sidebar(false);
        }
    }

    fn is_general_page(&self) -> bool {
        self.stack.visible_child_name().as_deref() == Some(Page::General.stack_name())
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
        let general = self.is_general_page();
        let subscriptions =
            self.stack.visible_child_name().as_deref() == Some(Page::Subscriptions.stack_name());
        let settings =
            self.stack.visible_child_name().as_deref() == Some(Page::Settings.stack_name());
        self.subscription_actions.set_visible(subscriptions);
        self.settings_actions.set_visible(settings);
        if self.compact.get() {
            self.sync_search_entry(&self.compact_search);
            self.search.set_visible(false);
            self.search_toggle.set_visible(general);
            self.search_bar.set_visible(general);
            self.search_bar
                .set_key_capture_widget(general.then_some(&self.window));
            if general && !self.compact_search.text().is_empty() {
                self.search_bar.set_search_mode(true);
            }
        } else {
            self.sync_search_entry(&self.search);
            self.search.set_visible(general);
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

        self.subscriptions.set_ultra_compact(enabled);
        self.settings.set_ultra_compact(enabled);
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
        let width = self.window.width();
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

    fn rebuild_views(self: &Rc<Self>) {
        let (subscriptions, selected_id, connected_id, latencies, checking, operation) = {
            let state = self.state.borrow();
            (
                state.subscriptions.clone(),
                state.selected_id.clone(),
                state.connected_id.clone(),
                state.latencies.clone(),
                state.checking.clone(),
                state.operation.clone(),
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
        };
        self.servers.rebuild(
            &subscriptions,
            connected_id.as_deref(),
            selected_id.as_deref(),
            &latencies,
            &checking,
            callbacks,
        );

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
        };
        self.subscriptions.rebuild(&subscriptions, sub_callbacks);
        self.subscriptions.set_operation(operation);
    }

    fn activate_server(self: &Rc<Self>, server_id: String) {
        let (status, connected) = {
            let state = self.state.borrow();
            (self.current_status(&state), state.connected_id.clone())
        };
        if matches!(status, Status::Connected | Status::Connecting)
            && connected.as_deref() == Some(&server_id)
        {
            self.disconnect();
        } else {
            self.connect_server(server_id);
        }
    }

    fn disconnect_if_active(self: &Rc<Self>) {
        let status = {
            let state = self.state.borrow();
            self.current_status(&state)
        };
        if matches!(status, Status::Connecting | Status::Connected) {
            self.disconnect();
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
            if state.checking.contains(&server_id) {
                return;
            }
            state.checking.insert(server_id.clone());
            if notify_failure {
                state.notify_probe.insert(server_id.clone());
            }
        }
        self.servers
            .set_latency_state(&server_id, LatencyState::Checking);
        self.refresh_activity_status();
        let client = self.state.borrow().client.clone();
        std::thread::spawn(move || {
            let _ = client.request_probe(&server_id);
        });
    }

    fn enqueue_probes(self: &Rc<Self>, ids: Vec<String>) {
        let new_ids: Vec<String> = {
            let mut state = self.state.borrow_mut();
            ids.into_iter()
                .filter(|id| state.checking.insert(id.clone()))
                .collect()
        };
        if new_ids.is_empty() {
            return;
        }
        for id in &new_ids {
            self.servers.set_latency_state(id, LatencyState::Checking);
        }
        self.refresh_activity_status();
        let client = self.state.borrow().client.clone();
        std::thread::spawn(move || {
            let _ = client.request_probes(&new_ids);
        });
    }

    fn connect_server(self: &Rc<Self>, server_id: String) {
        {
            let mut state = self.state.borrow_mut();
            state.selected_id = Some(server_id.clone());
            state.connected_id = Some(server_id.clone());
            state.pending_status = Some(Status::Connecting);
            state.last_active_latency = None;
        }
        self.set_cards_connection(Some(&server_id), Some(&server_id));
        self.servers.set_selected(Some(&server_id));
        self.refresh_status();
        let work_id = server_id.clone();
        self.client_job(
            UiOperation::for_server(UiOperationKind::Connect, server_id),
            move |client| client.connect_server(&work_id),
            move |controller, result| {
                if let Err(error) = result {
                    let message = format!("{error:#}");
                    {
                        let mut state = controller.state.borrow_mut();
                        state.pending_status = Some(Status::Error(message.clone()));
                        state.connected_id = None;
                    }
                    controller.set_cards_connection(None, None);
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
            state.pending_status = Some(Status::Disconnected);
            state.connected_id = None;
            state.last_active_latency = None;
        }
        self.set_cards_connection(None, None);
        self.refresh_status();
        self.client_job(
            UiOperation::new(UiOperationKind::Disconnect),
            |client| client.disconnect(),
            |controller, result| {
                if let Err(error) = result {
                    controller.show_message(&format!("Could not disconnect: {error}"));
                }
                controller.reconcile_system_proxy();
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
                controller.finish_removal("delete subscription", result);
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
                Err(error) => controller.show_message(&format!("Could not import: {error}")),
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
            self.show_message(&format!("Could not {action}: {error}"));
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
                        state.connected_id = None;
                        state.last_active_latency = None;
                    }
                    self.set_cards_connection(None, None);
                    self.show_message("Disconnected — the active server was removed");
                    self.reconcile_system_proxy();
                    self.refresh_status();
                }
                self.rebuild_views();
            }
            Err(error) => self.show_message(&format!("Could not {action}: {error}")),
        }
    }

    fn set_hwid(self: &Rc<Self>, subscription_id: String, enabled: bool) {
        let work_id = subscription_id;
        self.client_job(
            UiOperation::new(UiOperationKind::ApplySettings),
            move |client| client.set_hwid(&work_id, enabled),
            |controller, result| {
                if let Err(error) = result {
                    controller.show_message(&format!("Could not save HWID preference: {error}"));
                }
                controller.rebuild_views();
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
            latency_method: values.latency_method,
            latency_test_url: values.latency_test_url.clone(),
            subscription_user_agent: values.subscription_user_agent.clone(),
            xray_binary: values.xray_binary.clone(),
        };
        self.client_job(
            UiOperation::new(UiOperationKind::ApplySettings),
            move |client| {
                let outcome = client.apply_settings(&config)?;
                // Applying can move the Xray path or be refused outright; ask
                // the daemon what it ended up with instead of assuming.
                Ok((outcome, client.runtime_info().ok()))
            },
            move |controller, result| {
                match result {
                    Ok((outcome, runtime)) => {
                        controller.settings.mark_applied(values.clone());
                        controller.settings.set_runtime_info(runtime.as_ref());
                        if !outcome.ignored_ports.is_empty() {
                            controller.show_message(&format!(
                                "{} left unchanged — fixed by the system service unit",
                                outcome.ignored_ports.join(" and ")
                            ));
                        }
                        if let Some(error) = outcome.reconnect_error {
                            {
                                let mut state = controller.state.borrow_mut();
                                state.pending_status = Some(Status::Error(error.clone()));
                                state.connected_id = None;
                            }
                            controller.set_cards_connection(None, None);
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
            if state.operation.is_some() {
                drop(state);
                self.show_message("Another operation is still running");
                return;
            }
            state.operation = Some(operation.clone());
        }
        self.subscriptions.set_operation(Some(operation));
        self.refresh_activity_status();

        let client = self.state.borrow().client.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = work(&client);
            let subscriptions = client.subscriptions();
            let _ = sender.send((result, subscriptions));
        });

        let weak = Rc::downgrade(self);
        let mut complete = Some(complete);
        glib::timeout_add_local(Duration::from_millis(40), move || {
            match receiver.try_recv() {
                Ok((result, subscriptions)) => {
                    let Some(controller) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    {
                        let mut state = controller.state.borrow_mut();
                        if let Ok(subscriptions) = subscriptions {
                            state.subscriptions = subscriptions;
                        }
                        state.pending_status = None;
                        state.operation = None;
                    }
                    controller.subscriptions.set_operation(None);
                    controller.refresh_status();
                    if let Some(complete) = complete.take() {
                        complete(&controller, result);
                    }
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(controller) = weak.upgrade() {
                        let mut state = controller.state.borrow_mut();
                        state.pending_status = None;
                        state.operation = None;
                        drop(state);
                        controller.subscriptions.set_operation(None);
                        controller.show_message("Background operation stopped unexpectedly");
                        controller.refresh_status();
                    }
                    glib::ControlFlow::Break
                }
            }
        });
    }

    /// 500ms tick: apply the last poll snapshot, then start the next poll on
    /// a worker thread (never block the UI on D-Bus).
    fn start_timer(self: &Rc<Self>) {
        let controller = self.clone();
        glib::timeout_add_local(Duration::from_millis(500), move || {
            controller.drain_tray_commands();
            if let Some(snapshot) = controller.poll_snapshot.lock().unwrap().take() {
                controller.apply_snapshot(snapshot);
            }
            if !controller.poll_in_flight.swap(true, Ordering::SeqCst) {
                let client = controller.state.borrow().client.clone();
                let slot = controller.poll_snapshot.clone();
                let in_flight = controller.poll_in_flight.clone();
                std::thread::spawn(move || {
                    let snapshot = (|| {
                        Ok::<PolledSnapshot, anyhow::Error>(PolledSnapshot {
                            status: client.status()?,
                            probe: client.probe_state()?,
                            logs: client.recent_logs()?,
                        })
                    })();
                    if let Ok(snapshot) = snapshot {
                        *slot.lock().unwrap() = Some(snapshot);
                    }
                    in_flight.store(false, Ordering::SeqCst);
                });
            }
            glib::ControlFlow::Continue
        });
    }

    fn apply_snapshot(self: &Rc<Self>, snapshot: PolledSnapshot) {
        let mut latency_updates: Vec<(String, LatencyState)> = Vec::new();
        let mut toast_unreachable = false;
        let mut new_error: Option<String> = None;
        {
            let mut state = self.state.borrow_mut();
            for (id, value) in &snapshot.probe.latencies {
                if state.latencies.get(id) != Some(value) {
                    state.latencies.insert(id.clone(), *value);
                    latency_updates.push((
                        id.clone(),
                        match value {
                            Some(ms) => LatencyState::Reachable(*ms),
                            None => LatencyState::Unreachable,
                        },
                    ));
                    if state.connected_id.as_deref() == Some(id)
                        && let Some(ms) = value
                    {
                        state.last_active_latency = Some(*ms);
                    }
                    if state.notify_probe.remove(id) && value.is_none() {
                        toast_unreachable = true;
                    }
                }
            }
            // Mirror the daemon's checking set: new entries show spinners,
            // ids that finished (have a result and are no longer checking)
            // leave the local set.
            let daemon_checking: HashSet<String> =
                snapshot.probe.checking.iter().cloned().collect();
            for id in &daemon_checking {
                if state.checking.insert(id.clone()) {
                    latency_updates.push((id.clone(), LatencyState::Checking));
                }
            }
            let finished: Vec<String> = state
                .checking
                .iter()
                .filter(|id| {
                    !daemon_checking.contains(*id) && snapshot.probe.latencies.contains_key(*id)
                })
                .cloned()
                .collect();
            for id in finished {
                state.checking.remove(&id);
                if !latency_updates.iter().any(|(updated, _)| updated == &id) {
                    let value = snapshot.probe.latencies.get(&id).copied().flatten();
                    latency_updates.push((
                        id,
                        match value {
                            Some(ms) => LatencyState::Reachable(ms),
                            None => LatencyState::Unreachable,
                        },
                    ));
                }
            }
            state.daemon_status = snapshot.status.to_status();
            // Key off the daemon's own view, not `current_status`: the latter
            // prefers `pending_status`, which a failed connect leaves pinned.
            let error = match &state.daemon_status {
                Status::Error(message) => Some(message.clone()),
                _ => None,
            };
            if error != state.notified_error {
                state.notified_error = error.clone();
                // Record the transition either way, but stay quiet while
                // hidden: ToastOverlay queues, and `gui --background` would
                // otherwise dump an hour of stale toasts when first opened.
                new_error = error.filter(|_| self.window.is_visible());
            }
            // While no optimistic transition is in flight, the daemon's view
            // of the active server wins.
            if state.operation.is_none()
                && state.pending_status.is_none()
                && snapshot.status.active_id != state.connected_id
            {
                state.connected_id = snapshot.status.active_id.clone();
                if state.connected_id.is_none() {
                    state.last_active_latency = None;
                }
            }
        }
        for (id, latency_state) in latency_updates {
            self.servers.set_latency_state(&id, latency_state);
        }
        if toast_unreachable {
            self.show_message("Server is unreachable or did not respond");
        }
        // Failures the user never triggered — a crashed core, a tunnel torn
        // down by its own latency check — reach the screen only here.
        if let Some(error) = new_error {
            self.show_error("Connection error", &error);
        }
        self.logs.set_logs(&snapshot.logs);
        self.sync_connection_cards();
        self.reconcile_system_proxy();
        self.refresh_status();
    }

    /// Push the current connection onto the cards, skipping the O(cards)
    /// pass when nothing changed.
    fn sync_connection_cards(&self) {
        let (active, status) = {
            let state = self.state.borrow();
            (state.connected_id.clone(), self.current_status(&state))
        };
        let desired = match status {
            Status::Connecting => (active.clone(), active),
            Status::Connected => (active, None),
            _ => (None, None),
        };
        if *self.applied_connection.borrow() == desired {
            return;
        }
        *self.applied_connection.borrow_mut() = desired.clone();
        self.servers
            .set_connection(desired.0.as_deref(), desired.1.as_deref());
    }

    fn set_cards_connection(&self, active: Option<&str>, connecting: Option<&str>) {
        *self.applied_connection.borrow_mut() =
            (active.map(str::to_string), connecting.map(str::to_string));
        self.servers.set_connection(active, connecting);
    }

    /// The GNOME system proxy is a session concern, so the GUI (not the
    /// daemon, which may run as a system service) applies and clears it. A
    /// marker file survives crashes so the next start can undo a stale proxy.
    fn reconcile_system_proxy(&self) {
        let applied_settings = self.settings.applied();
        let status = {
            let state = self.state.borrow();
            self.current_status(&state)
        };
        let want = applied_settings.system_proxy && status == Status::Connected;
        if want && !self.proxy_applied.get() {
            if sysproxy::apply(applied_settings.socks_port, applied_settings.http_port).is_ok() {
                self.proxy_applied.set(true);
                if let Some(marker) = gui_proxy_marker() {
                    let _ = std::fs::write(marker, b"1");
                }
            }
        } else if !want && self.proxy_applied.get() {
            let _ = sysproxy::clear();
            self.proxy_applied.set(false);
            if let Some(marker) = gui_proxy_marker() {
                let _ = std::fs::remove_file(marker);
            }
        }
    }

    fn drain_tray_commands(self: &Rc<Self>) {
        while let Ok(command) = self.tray_commands.try_recv() {
            match command {
                TrayCommand::ShowWindow => self.window.present(),
                TrayCommand::Disconnect => self.disconnect_if_active(),
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
            if let Some(app) = self.window.application() {
                app.quit();
            }
        } else {
            self.window.set_visible(false);
        }
    }

    /// Mirror the connection into the tray tooltip/menu, skipping no-ops.
    fn update_tray(&self, status: &Status) {
        let Some(handle) = self.tray.borrow().clone() else {
            return;
        };
        let connected = matches!(status, Status::Connected | Status::Connecting);
        let text = match status {
            Status::Disconnected => "Disconnected".to_string(),
            Status::Connecting => "Connecting…".to_string(),
            Status::Connected => {
                let name = self.active_server_name(&self.state.borrow());
                match name {
                    Some(name) => format!("Connected · {name}"),
                    None => "Connected".to_string(),
                }
            }
            Status::Error(error) => format!("Error: {error}"),
        };
        if *self.tray_pushed.borrow() == (connected, text.clone()) {
            return;
        }
        *self.tray_pushed.borrow_mut() = (connected, text.clone());
        handle.update(move |tray| {
            tray.connected = connected;
            tray.status_text = text.clone();
        });
    }

    fn refresh_status(&self) {
        let state = self.state.borrow();
        let status = self.current_status(&state);
        let active_latency = state
            .connected_id
            .as_ref()
            .and_then(|id| state.latencies.get(id))
            .copied()
            .flatten()
            .or(state.last_active_latency);
        drop(state);

        self.update_tray(&status);
        self.update_header_connection_status(&status, active_latency);
        self.refresh_activity_status();
    }

    fn active_server_name(&self, state: &AppState) -> Option<String> {
        self.active_server_display(state).map(|(name, _)| name)
    }

    /// Display name and country of the connected server.
    fn active_server_display(&self, state: &AppState) -> Option<(String, Option<String>)> {
        let active = state.connected_id.as_deref()?;
        state
            .subscriptions
            .iter()
            .flat_map(|subscription| subscription.servers.iter())
            .find(|server| server.id == active)
            .map(|server| {
                (
                    crate::model::name_without_flag(&server.name).to_string(),
                    server.country.clone(),
                )
            })
    }

    /// The compact-mode status chip: as small as possible — a flag plus the
    /// latency reading; everything verbose lives in the tooltip.
    fn update_header_connection_status(&self, status: &Status, active_latency: Option<u32>) {
        self.header_status_spinner.set_spinning(false);
        self.header_status_spinner.set_visible(false);
        self.header_status_icon.set_visible(false);
        while let Some(child) = self.header_status_flag.first_child() {
            self.header_status_flag.remove(&child);
        }
        self.header_status_flag.set_visible(false);
        self.header_status_label.set_label("");
        self.header_status_label.set_visible(false);
        set_status_tone(&self.header_status, StatusTone::Neutral);
        self.header_status
            .set_visible(self.compact.get() && !matches!(status, Status::Disconnected));
        self.header_status.set_sensitive(false);

        let display = self.active_server_display(&self.state.borrow());
        let name = display.as_ref().map(|(name, _)| name.clone());
        let country = display.and_then(|(_, country)| country);
        match status {
            Status::Disconnected => {}
            Status::Connecting => {
                set_status_tone(&self.header_status, StatusTone::Working);
                self.header_status_spinner.set_visible(true);
                self.header_status_spinner.set_spinning(true);
                let tooltip = name.as_deref().map_or_else(
                    || "Connecting…".to_string(),
                    |name| format!("Connecting · {name}"),
                );
                self.header_status.set_tooltip_text(Some(&tooltip));
                self.header_status
                    .update_property(&[gtk::accessible::Property::Label(&tooltip)]);
            }
            Status::Connected => {
                set_status_tone(&self.header_status, StatusTone::Connected);
                self.header_status_flag
                    .append(&super::server_card::flag_widget(country.as_deref(), 16, 14));
                self.header_status_flag.set_visible(true);
                if let Some(ms) = active_latency {
                    self.header_status_label.set_label(&format!("{ms} ms"));
                    self.header_status_label.set_visible(true);
                }
                let tooltip = format!(
                    "{} — click to disconnect",
                    name.as_deref().unwrap_or("Connected")
                );
                self.header_status.set_tooltip_text(Some(&tooltip));
                self.header_status.set_sensitive(true);
                self.header_status
                    .update_property(&[gtk::accessible::Property::Label("Disconnect VPN")]);
            }
            Status::Error(error) => {
                set_status_tone(&self.header_status, StatusTone::Error);
                self.header_status_icon
                    .set_icon_name(Some("dialog-warning-symbolic"));
                self.header_status_icon.set_visible(true);
                self.header_status.set_tooltip_text(Some(error));
                self.header_status
                    .update_property(&[gtk::accessible::Property::Label("Connection error")]);
            }
        }
    }

    fn update_sidebar_connection_status(&self, state: &AppState) {
        let status = self.current_status(state);
        let active_latency = state
            .connected_id
            .as_ref()
            .and_then(|id| state.latencies.get(id))
            .copied()
            .flatten();
        self.sidebar_status.set_sensitive(false);
        match status {
            Status::Disconnected => {
                set_status_tone(&self.sidebar_status, StatusTone::Neutral);
                self.sidebar_status_icon
                    .set_icon_name(Some("network-vpn-symbolic"));
                self.sidebar_status_label.set_label("Ready");
                self.sidebar_status.set_tooltip_text(Some("Ready"));
            }
            Status::Connecting => {
                set_status_tone(&self.sidebar_status, StatusTone::Working);
                self.sidebar_status_icon
                    .set_icon_name(Some("network-transmit-receive-symbolic"));
                self.sidebar_status_label.set_label("Connecting…");
                self.sidebar_status.set_tooltip_text(Some("Connecting"));
            }
            Status::Connected => {
                set_status_tone(&self.sidebar_status, StatusTone::Connected);
                self.sidebar_status_icon
                    .set_icon_name(Some("network-vpn-symbolic"));
                let name = self
                    .active_server_name(state)
                    .unwrap_or_else(|| "Connected".to_string());
                let label = active_latency
                    .map(|ms| format!("{name} · {ms} ms"))
                    .unwrap_or(name);
                self.sidebar_status_label.set_label(&label);
                self.sidebar_status.set_tooltip_text(Some("Disconnect VPN"));
                self.sidebar_status.set_sensitive(true);
                self.sidebar_status
                    .update_property(&[gtk::accessible::Property::Label("Disconnect VPN")]);
            }
            Status::Error(ref error) => {
                set_status_tone(&self.sidebar_status, StatusTone::Error);
                self.sidebar_status_icon
                    .set_icon_name(Some("dialog-warning-symbolic"));
                self.sidebar_status_label.set_label("Connection error");
                self.sidebar_status.set_tooltip_text(Some(error));
            }
        }
    }

    fn refresh_activity_status(&self) {
        let state = self.state.borrow();
        match (state.operation.as_ref(), state.checking.len()) {
            (Some(operation), _) => {
                set_status_tone(&self.sidebar_status, StatusTone::Working);
                self.sidebar_status_icon
                    .set_icon_name(Some("view-refresh-symbolic"));
                self.sidebar_status_label.set_label(operation.label());
            }
            (None, 1) => {
                set_status_tone(&self.sidebar_status, StatusTone::Working);
                self.sidebar_status_icon
                    .set_icon_name(Some("network-transmit-receive-symbolic"));
                self.sidebar_status_label.set_label("Checking latency…");
            }
            (None, count) if count > 1 => {
                set_status_tone(&self.sidebar_status, StatusTone::Working);
                self.sidebar_status_icon
                    .set_icon_name(Some("network-transmit-receive-symbolic"));
                self.sidebar_status_label
                    .set_label(&format!("Checking latency · {count}"));
            }
            (None, _) => self.update_sidebar_connection_status(&state),
        }
    }

    fn current_status(&self, state: &AppState) -> Status {
        state
            .pending_status
            .clone()
            .unwrap_or_else(|| state.daemon_status.clone())
    }

    /// The status buttons double as the only always-visible carrier of a
    /// failure: the detail otherwise lives in a tooltip, which is unreachable
    /// by keyboard and hidden entirely in wide mode.
    fn handle_status_clicked(self: &Rc<Self>) {
        let status = {
            let state = self.state.borrow();
            self.current_status(&state)
        };
        match status {
            Status::Error(error) => self.show_error_details("Connection error", &error),
            _ => self.disconnect_if_active(),
        }
    }

    /// A failure the user did not directly trigger, or one too long for a
    /// toast: one line, the full text behind Details, and a shortcut to the
    /// place that can fix it when the message names one.
    fn show_error(self: &Rc<Self>, title: &str, detail: &str) {
        let toast = adw::Toast::new(&summarize_error(title, detail));
        toast.set_priority(adw::ToastPriority::High);
        toast.set_timeout(8);
        let open_settings = ipc::error_action(detail) == ipc::ErrorAction::OpenSettings;
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

    /// Full error text, selectable and dismissible.
    fn show_error_details(self: &Rc<Self>, title: &str, detail: &str) {
        let dialog = adw::MessageDialog::new(Some(&self.window), Some(title), Some(detail));
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
        dialog.present();
    }

    /// Records an error the caller has already surfaced, so the poll loop does
    /// not toast the identical message a second time. Both paths format the
    /// same anyhow error with `{:#}`, so the strings match exactly.
    fn mark_error_notified(&self, message: &str) {
        self.state.borrow_mut().notified_error = Some(message.to_string());
    }

    fn navigate_to(&self, page: Page) {
        // Drive the sidebar rather than the stack directly, so its selection
        // and the visible page cannot disagree.
        if let Some(row) = self.sidebar_list.row_at_index(page.index()) {
            self.sidebar_list.select_row(Some(&row));
        }
    }

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
        button.flat { min-height: 24px; font-weight: normal; }
        button.pill { font-weight: 500; }
        button.slim-pill { min-height: 24px; padding: 2px 16px; }
        .compact-search { min-height: 28px; }
        .compact-search text { padding-top: 1px; padding-bottom: 1px; }
        .compact-connect { min-height: 24px; padding: 2px 14px; font-weight: 500; }
        .ultra-connect-bar { padding: 8px 12px 10px; border-top: 1px solid alpha(@window_fg_color, 0.08); background: alpha(@window_fg_color, 0.025); }
        .ultra-connect-bar .compact-connect { min-height: 34px; }

        .sidebar { background: alpha(@window_fg_color, 0.035); }
        .sidebar-status { padding: 8px 10px; border-radius: 10px; background: alpha(@window_fg_color, 0.055); }
        .sidebar-status.status-neutral { color: alpha(@window_fg_color, 0.72); }
        .sidebar-status.status-working { color: @accent_color; background: alpha(@accent_color, 0.12); }
        .sidebar-status.status-connected { color: @success_color; background: alpha(@success_color, 0.12); }
        .sidebar-status.status-error { color: @error_color; background: alpha(@error_color, 0.14); }
        .sidebar-status-icon { color: currentColor; -gtk-icon-size: 18px; }

        .server-card {
            border: none;
            border-radius: 12px;
            background: alpha(@window_fg_color, 0.05);
            box-shadow: inset 0 0 0 1px alpha(@window_fg_color, 0.07);
        }
        .server-card:hover { background: alpha(@window_fg_color, 0.09); }
        .server-card.selected-server { box-shadow: inset 0 0 0 1px alpha(@accent_color, 0.65); }
        .server-card.active-server { background: alpha(@accent_color, 0.12); box-shadow: inset 0 0 0 1px @accent_color; }
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
        .latency-badge { border-radius: 999px; padding: 3px 8px; font-size: 0.75em; font-weight: 500; }
        .latency-badge.latency-error { font-size: 1.05em; padding: 1px 8px; font-weight: 700; }
        .latency-spinner { color: @accent_color; }
        .latency-reachable { color: @accent_color; background: alpha(@accent_color, 0.12); }
        .latency-error { color: @error_color; background: alpha(@error_color, 0.13); }
        .status-badge { border-radius: 999px; padding: 3px 8px; font-size: 0.75em; font-weight: 600; }
        .status-badge.status-working { color: @accent_color; background: alpha(@accent_color, 0.13); }
        .status-badge.status-connected { color: @success_color; background: alpha(@success_color, 0.13); }
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
    use super::{ResponsiveMode, SearchState, responsive_mode_for_width, summarize_error};

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
