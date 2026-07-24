use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;

use crate::APP_ID;
use crate::engine::Engine;
use crate::model::Subscription;
use crate::probe;
use crate::xray::core::Status;

use super::operation::{UiOperation, UiOperationKind};
use super::server_card::LatencyState;
use super::sidebar::{Page, Sidebar};
use super::views::logs::LogsView;
use super::views::servers::ServersView;
use super::views::settings::{SettingsValues, SettingsView};
use super::views::subscriptions::SubscriptionsView;

type SettingsCallback = Rc<dyn Fn(SettingsValues)>;

const SIDEBAR_BREAKPOINT_WIDTH: u32 = 700;
const ACTIVE_PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// Cap for simultaneously running latency probes; "check all" on a large
/// subscription queues the rest instead of spawning hundreds of threads and
/// connections at once.
const MAX_CONCURRENT_PROBES: usize = 8;

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

fn active_probe_is_due(
    now: Instant,
    next_probe: Option<Instant>,
    connected: bool,
    has_active_server: bool,
    active_probe_running: bool,
    engine_available: bool,
    operation_active: bool,
) -> bool {
    connected
        && has_active_server
        && !active_probe_running
        && engine_available
        && !operation_active
        && next_probe.is_some_and(|deadline| now >= deadline)
}

struct AppState {
    engine: Option<Engine>,
    subscriptions: Vec<Subscription>,
    /// Card the user is inspecting/expanded. Also the target of the header
    /// Connect button. Distinct from the server that is actually connected.
    selected_id: Option<String>,
    /// Server the tunnel is (optimistically) running for; drives the highlight.
    connected_id: Option<String>,
    latencies: HashMap<String, Option<u32>>,
    checking: HashSet<String>,
    /// Servers waiting for a probe slot (see MAX_CONCURRENT_PROBES).
    probe_queue: VecDeque<String>,
    next_active_probe: Option<Instant>,
    operation: Option<UiOperation>,
    pending_status: Option<Status>,
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
    servers: ServersView,
    subscriptions: SubscriptionsView,
    settings: SettingsView,
    logs: LogsView,
    toasts: adw::ToastOverlay,
    force_close: Cell<bool>,
    close_after_apply: Cell<bool>,
}

pub fn build(app: &adw::Application) {
    install_css();
    #[cfg(debug_assertions)]
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display)
            .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data"));
    }
    gtk::Window::set_default_icon_name(APP_ID);

    let engine = Engine::load();
    let subscriptions_snapshot = engine.subscriptions.clone();
    let selected_id = engine.state.active_server_id.clone();
    let initial_config = engine.config.clone();
    let state = Rc::new(RefCell::new(AppState {
        engine: Some(engine),
        subscriptions: subscriptions_snapshot,
        selected_id,
        connected_id: None,
        latencies: HashMap::new(),
        checking: HashSet::new(),
        probe_queue: VecDeque::new(),
        next_active_probe: None,
        operation: None,
        pending_status: None,
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
    let header_status_label = gtk::Label::builder()
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(18)
        .xalign(0.0)
        .build();
    let header_status_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    header_status_content.append(&header_status_spinner);
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
        servers,
        subscriptions,
        settings,
        logs,
        toasts,
        force_close: Cell::new(false),
        close_after_apply: Cell::new(false),
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

    // Shut the tunnel down and restore the desktop proxy on SIGINT/SIGTERM,
    // not just on a clean window close.
    for signal in [libc::SIGINT, libc::SIGTERM] {
        glib::unix_signal_add_local(signal, {
            let weak = Rc::downgrade(&controller);
            move || {
                if let Some(controller) = weak.upgrade() {
                    controller.shutdown_engine();
                    controller.force_close.set(true);
                    controller.window.close();
                }
                glib::ControlFlow::Break
            }
        });
    }

    window.present();

    let warnings = controller
        .state
        .borrow_mut()
        .engine
        .as_mut()
        .map(|engine| std::mem::take(&mut engine.load_warnings))
        .unwrap_or_default();
    for warning in warnings {
        controller.show_message(&warning);
    }
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
    fn wire_actions(self: &Rc<Self>) {
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
                    controller.disconnect_if_active();
                }
            }
        });
        self.sidebar_status.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.disconnect_if_active();
                }
            }
        });
        self.logs.connect_clear_requested({
            let weak = Rc::downgrade(self);
            move || {
                if let Some(controller) = weak.upgrade()
                    && let Some(engine) = controller.state.borrow().engine.as_ref()
                {
                    engine.core.clear_logs();
                }
            }
        });
        self.window.connect_close_request({
            let weak = Rc::downgrade(self);
            move |_| {
                let Some(controller) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if controller.force_close.get() || !controller.settings.has_unsaved_changes() {
                    // Dropping the engine here (not at some uncertain point of
                    // Rc teardown) guarantees the xray child is killed and the
                    // desktop proxy restored before the process exits.
                    controller.shutdown_engine();
                    return glib::Propagation::Proceed;
                }
                controller.confirm_close_with_unsaved_settings();
                glib::Propagation::Stop
            }
        });
    }

    /// Take and drop the engine: kills the xray child and restores the system
    /// proxy. When a worker thread currently owns the engine there is nothing
    /// safe to do here — the persisted recovery flags repair things on the
    /// next start instead.
    fn shutdown_engine(&self) {
        let engine = self.state.borrow_mut().engine.take();
        if engine.is_none() {
            log::warn!("exiting while a background operation owns the engine");
        }
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
                        controller.force_close.set(true);
                        controller.window.close();
                    }
                    _ => {}
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

    /// Queue a batch of latency probes, running at most MAX_CONCURRENT_PROBES
    /// at a time.
    fn enqueue_probes(self: &Rc<Self>, ids: Vec<String>) {
        {
            let mut state = self.state.borrow_mut();
            for id in ids {
                if !state.checking.contains(&id) && !state.probe_queue.contains(&id) {
                    state.probe_queue.push_back(id);
                }
            }
        }
        self.pump_probe_queue();
    }

    fn pump_probe_queue(self: &Rc<Self>) {
        loop {
            let next = {
                let mut state = self.state.borrow_mut();
                if state.checking.len() >= MAX_CONCURRENT_PROBES {
                    return;
                }
                state.probe_queue.pop_front()
            };
            let Some(id) = next else {
                return;
            };
            self.probe_one(id, false);
        }
    }

    fn probe_one(self: &Rc<Self>, server_id: String, notify_failure: bool) {
        if self.state.borrow().checking.contains(&server_id) {
            return;
        }
        let server = self
            .state
            .borrow()
            .subscriptions
            .iter()
            .flat_map(|subscription| subscription.servers.iter())
            .find(|server| server.id == server_id)
            .cloned();
        let Some(server) = server else {
            return;
        };
        let (method, socks_port, test_url) = {
            let state = self.state.borrow();
            match state.engine.as_ref() {
                Some(engine) => (
                    engine.config.latency_method,
                    engine.config.socks_port,
                    engine.config.latency_test_url.clone(),
                ),
                None => return,
            }
        };
        self.state.borrow_mut().checking.insert(server_id.clone());
        self.servers
            .set_latency_state(&server_id, LatencyState::Checking);
        self.refresh_activity_status();

        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(probe::measure(&server, method, socks_port, &test_url));
        });
        let weak = Rc::downgrade(self);
        let id = server_id;
        glib::timeout_add_local(Duration::from_millis(50), move || {
            match receiver.try_recv() {
                Ok(latency) => {
                    if let Some(controller) = weak.upgrade() {
                        controller.finish_probe(&id, latency, notify_failure);
                    }
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(controller) = weak.upgrade() {
                        controller.finish_probe(&id, None, notify_failure);
                    }
                    glib::ControlFlow::Break
                }
            }
        });
    }

    fn finish_probe(self: &Rc<Self>, server_id: &str, latency: Option<u32>, notify_failure: bool) {
        let mut connection_failed = false;
        {
            let mut state = self.state.borrow_mut();
            state.checking.remove(server_id);
            state.latencies.insert(server_id.to_string(), latency);
            let confirming_connection = state.connected_id.as_deref() == Some(server_id)
                && state.pending_status == Some(Status::Connecting);
            if let Some(engine) = state.engine.as_mut() {
                if let Some(server) = engine
                    .subscriptions
                    .iter_mut()
                    .flat_map(|subscription| subscription.servers.iter_mut())
                    .find(|server| server.id == server_id)
                {
                    server.latency_ms = latency;
                }
                if confirming_connection && latency.is_none() {
                    engine.disconnect();
                }
            }
            if confirming_connection {
                if latency.is_some() {
                    state.pending_status = None;
                } else {
                    state.pending_status = Some(Status::Error(
                        "active server did not pass its latency check".into(),
                    ));
                    state.connected_id = None;
                    state.next_active_probe = None;
                    connection_failed = true;
                }
            }
            if !connection_failed && state.connected_id.as_deref() == Some(server_id) {
                state.next_active_probe = Some(Instant::now() + ACTIVE_PROBE_INTERVAL);
            }
        }
        self.servers.set_latency_state(
            server_id,
            match latency {
                Some(ms) => LatencyState::Reachable(ms),
                None => LatencyState::Unreachable,
            },
        );
        if connection_failed {
            self.servers.set_connection(None, None);
            self.show_message("Connection failed: the active server did not respond");
        } else if self.state.borrow().pending_status.is_none()
            && self.state.borrow().connected_id.as_deref() == Some(server_id)
        {
            self.servers.set_connection(Some(server_id), None);
        } else if notify_failure && latency.is_none() {
            self.show_message("Server is unreachable or did not respond");
        }
        self.refresh_status();
        self.pump_probe_queue();
    }

    fn connect_server(self: &Rc<Self>, server_id: String) {
        if self.state.borrow().engine.is_none() {
            self.show_message("Another operation is still running");
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            state.selected_id = Some(server_id.clone());
            state.connected_id = Some(server_id.clone());
            state.pending_status = Some(Status::Connecting);
            state.next_active_probe = None;
        }
        self.servers
            .set_connection(Some(&server_id), Some(&server_id));
        self.servers.set_selected(Some(&server_id));
        self.refresh_status();
        let work_id = server_id.clone();
        self.engine_job(
            UiOperation::for_server(UiOperationKind::Connect, server_id.clone()),
            move |engine| engine.connect(&work_id),
            move |controller, result| {
                if let Err(error) = result {
                    {
                        let mut state = controller.state.borrow_mut();
                        state.pending_status = Some(Status::Error(error.to_string()));
                        state.connected_id = None;
                        state.next_active_probe = None;
                    }
                    controller.servers.set_connection(None, None);
                    controller.show_message(&format!("Could not connect: {error}"));
                } else {
                    controller.state.borrow_mut().pending_status = Some(Status::Connecting);
                    controller.probe_one(server_id.clone(), false);
                }
                controller.refresh_status();
            },
        );
    }

    fn disconnect(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            state.pending_status = Some(Status::Disconnected);
            state.connected_id = None;
            state.next_active_probe = None;
        }
        self.servers.set_connection(None, None);
        self.refresh_status();
        self.engine_job(
            UiOperation::new(UiOperationKind::Disconnect),
            |engine| {
                engine.disconnect();
                Ok(())
            },
            |controller, result| {
                if let Err(error) = result {
                    controller.show_message(&format!("Could not disconnect: {error}"));
                }
            },
        );
    }

    fn add_subscription(self: &Rc<Self>, url: String, name: Option<String>, send_hwid: bool) {
        self.engine_job(
            UiOperation::new(UiOperationKind::AddSubscription),
            move |engine| engine.add_subscription(url, name, send_hwid),
            |controller, result| controller.finish_subscription_change("add subscription", result),
        );
    }

    fn refresh_subscription(self: &Rc<Self>, subscription_id: String) {
        let work_id = subscription_id.clone();
        self.engine_job(
            UiOperation::for_subscription(UiOperationKind::UpdateSubscription, subscription_id),
            move |engine| engine.refresh(&work_id),
            |controller, result| {
                controller.finish_subscription_change("update subscription", result)
            },
        );
    }

    fn refresh_all_subscriptions(self: &Rc<Self>) {
        self.engine_job(
            UiOperation::new(UiOperationKind::UpdateAllSubscriptions),
            |engine| engine.refresh_all(),
            |controller, result| {
                controller.finish_subscription_change("update subscriptions", result)
            },
        );
    }

    fn remove_subscription(self: &Rc<Self>, subscription_id: String) {
        let work_id = subscription_id.clone();
        self.engine_job(
            UiOperation::for_subscription(UiOperationKind::DeleteSubscription, subscription_id),
            move |engine| engine.remove_subscription(&work_id),
            |controller, result| {
                controller.finish_removal("delete subscription", result);
            },
        );
    }

    fn import_servers(self: &Rc<Self>, text: String) {
        self.engine_job(
            UiOperation::new(UiOperationKind::ImportServers),
            move |engine| engine.import_links(&text),
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
        self.engine_job(
            UiOperation::for_server(UiOperationKind::DeleteServer, server_id),
            move |engine| engine.remove_server(&work_id),
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
                        state.next_active_probe = None;
                    }
                    self.servers.set_connection(None, None);
                    self.show_message("Disconnected — the active server was removed");
                    self.refresh_status();
                }
                self.rebuild_views();
            }
            Err(error) => self.show_message(&format!("Could not {action}: {error}")),
        }
    }

    fn set_hwid(self: &Rc<Self>, subscription_id: String, enabled: bool) {
        let result = {
            let mut state = self.state.borrow_mut();
            let Some(engine) = state.engine.as_mut() else {
                drop(state);
                self.show_message("Another operation is still running");
                self.rebuild_views();
                return;
            };
            if let Some(subscription) = engine
                .subscriptions
                .iter_mut()
                .find(|subscription| subscription.id == subscription_id)
            {
                subscription.send_hwid = enabled;
            }
            let result = engine.save();
            state.subscriptions = engine.subscriptions.clone();
            result
        };
        if let Err(error) = result {
            self.show_message(&format!("Could not save HWID preference: {error}"));
        }
    }

    fn save_settings(self: &Rc<Self>, values: SettingsValues) {
        let validation = self.settings.validation();
        if !validation.is_valid() {
            self.settings.set_apply_in_progress(false);
            return;
        }
        let applied = self.settings.applied();
        let ports_changed =
            applied.socks_port != values.socks_port || applied.http_port != values.http_port;
        let active_id = self.state.borrow().connected_id.clone();
        let work_values = values.clone();
        let work_active = active_id.clone();
        self.engine_job(
            UiOperation::new(UiOperationKind::ApplySettings),
            move |engine| {
                engine.config.socks_port = work_values.socks_port;
                engine.config.http_port = work_values.http_port;
                engine.config.system_proxy = work_values.system_proxy;
                engine.config.latency_method = work_values.latency_method;
                engine.config.latency_test_url = work_values.latency_test_url;
                engine.config.subscription_user_agent = work_values.subscription_user_agent;
                engine.core.socks_port = work_values.socks_port;
                engine.core.http_port = work_values.http_port;
                engine.save()?;
                let reconnect_error = if ports_changed && engine.core.status() == Status::Connected
                {
                    work_active
                        .as_deref()
                        .and_then(|active| engine.connect(active).err())
                        .map(|error| error.to_string())
                } else {
                    None
                };
                engine.reconcile_system_proxy();
                Ok(reconnect_error)
            },
            move |controller, result| {
                match result {
                    Ok(reconnect_error) => {
                        controller.settings.mark_applied(values.clone());
                        if !ports_changed && active_id.is_some() {
                            controller.state.borrow_mut().next_active_probe =
                                Some(Instant::now() + ACTIVE_PROBE_INTERVAL);
                        }
                        if let Some(error) = reconnect_error {
                            {
                                let mut state = controller.state.borrow_mut();
                                state.pending_status = Some(Status::Error(error.clone()));
                                state.connected_id = None;
                                state.next_active_probe = None;
                            }
                            controller.servers.set_connection(None, None);
                            controller.show_message(&format!(
                                "Settings saved, but the connection could not restart: {error}"
                            ));
                        } else if ports_changed && let Some(active) = active_id.clone() {
                            let mut state = controller.state.borrow_mut();
                            state.pending_status = Some(Status::Connecting);
                            state.next_active_probe = None;
                            drop(state);
                            controller.servers.set_connection(None, Some(&active));
                            controller.probe_one(active, false);
                        }
                        if controller.close_after_apply.replace(false) {
                            controller.force_close.set(true);
                            controller.window.close();
                        }
                    }
                    Err(error) => {
                        controller.settings.set_apply_in_progress(false);
                        controller.close_after_apply.set(false);
                        controller.show_message(&format!("Could not save settings: {error}"));
                    }
                }
                controller.refresh_status();
            },
        );
    }

    fn engine_job<R, Work, Complete>(
        self: &Rc<Self>,
        operation: UiOperation,
        work: Work,
        complete: Complete,
    ) where
        R: Send + 'static,
        Work: FnOnce(&mut Engine) -> Result<R> + Send + 'static,
        Complete: FnOnce(&Rc<Self>, Result<R>) + 'static,
    {
        let displayed_operation = operation.clone();
        let mut engine = {
            let mut state = self.state.borrow_mut();
            let previous_status = self.current_status(&state);
            let Some(engine) = state.engine.take() else {
                drop(state);
                self.show_message("Another operation is still running");
                return;
            };
            if state.pending_status.is_none() {
                state.pending_status = Some(previous_status.clone());
            }
            state.operation = Some(operation);
            engine
        };
        self.subscriptions.set_operation(Some(displayed_operation));
        self.refresh_activity_status();

        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = work(&mut engine);
            let _ = sender.send((engine, result));
        });

        let weak = Rc::downgrade(self);
        let mut complete = Some(complete);
        glib::timeout_add_local(Duration::from_millis(40), move || {
            match receiver.try_recv() {
                Ok((engine, result)) => {
                    let Some(controller) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    {
                        let mut state = controller.state.borrow_mut();
                        state.subscriptions = engine.subscriptions.clone();
                        state.engine = Some(engine);
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
                        state.engine = Some(Engine::load());
                        state.pending_status = Some(Status::Error(
                            "background operation stopped unexpectedly".to_string(),
                        ));
                        state.operation = None;
                        drop(state);
                        controller.subscriptions.set_operation(None);
                        controller.refresh_status();
                    }
                    glib::ControlFlow::Break
                }
            }
        });
    }

    fn maybe_probe_active(self: &Rc<Self>) {
        let now = Instant::now();
        let server_id = {
            let mut state = self.state.borrow_mut();
            let status = state
                .pending_status
                .clone()
                .or_else(|| state.engine.as_ref().map(Engine::status))
                .unwrap_or(Status::Disconnected);
            let active_id = state.connected_id.clone();
            let active_probe_running = active_id
                .as_ref()
                .is_some_and(|id| state.checking.contains(id));
            let due = active_probe_is_due(
                now,
                state.next_active_probe,
                status == Status::Connected,
                active_id.is_some(),
                active_probe_running,
                state.engine.is_some(),
                state.operation.is_some(),
            );
            if !matches!(status, Status::Connected | Status::Connecting) {
                state.next_active_probe = None;
            }
            if due {
                state.next_active_probe = Some(now + ACTIVE_PROBE_INTERVAL);
                active_id
            } else {
                None
            }
        };
        if let Some(server_id) = server_id {
            self.probe_one(server_id, false);
        }
    }

    fn start_timer(self: &Rc<Self>) {
        let controller = self.clone();
        glib::timeout_add_local(Duration::from_millis(500), move || {
            if !controller.window.is_visible() {
                return glib::ControlFlow::Break;
            }
            controller.refresh_status();
            controller.maybe_probe_active();
            let logs = controller
                .state
                .borrow()
                .engine
                .as_ref()
                .map(|engine| engine.core.recent_logs())
                .unwrap_or_default();
            controller.logs.set_logs(&logs);
            glib::ControlFlow::Continue
        });
    }

    fn refresh_status(&self) {
        let mut state = self.state.borrow_mut();
        let mut status = self.current_status(&state);
        if status == Status::Connected
            && state
                .engine
                .as_mut()
                .is_some_and(|engine| !engine.core.is_alive())
        {
            status = Status::Error("Xray exited unexpectedly".to_string());
            state.pending_status = Some(status.clone());
        }
        let active_latency = state
            .connected_id
            .as_ref()
            .and_then(|id| state.latencies.get(id))
            .copied()
            .flatten();
        drop(state);

        self.update_header_connection_status(&status, active_latency);
        self.refresh_activity_status();
    }

    fn active_server_name(&self, state: &AppState) -> Option<String> {
        let active = state.connected_id.as_deref()?;
        state
            .subscriptions
            .iter()
            .flat_map(|subscription| subscription.servers.iter())
            .find(|server| server.id == active)
            .map(|server| crate::model::name_without_flag(&server.name).to_string())
    }

    fn update_header_connection_status(&self, status: &Status, active_latency: Option<u32>) {
        self.header_status_spinner.set_spinning(false);
        self.header_status_spinner.set_visible(false);
        self.header_status_icon.set_visible(false);
        self.header_status_label.set_label("");
        set_status_tone(&self.header_status, StatusTone::Neutral);
        self.header_status
            .set_visible(self.compact.get() && !matches!(status, Status::Disconnected));
        self.header_status.set_sensitive(false);

        let name = self.active_server_name(&self.state.borrow());
        match status {
            Status::Disconnected => {}
            Status::Connecting => {
                set_status_tone(&self.header_status, StatusTone::Working);
                self.header_status_spinner.set_visible(true);
                self.header_status_spinner.set_spinning(true);
                let label = name.as_deref().map_or_else(
                    || "Connecting…".to_string(),
                    |name| format!("Connecting · {name}"),
                );
                self.header_status_label.set_label(&label);
                self.header_status.set_tooltip_text(Some(&label));
                self.header_status
                    .update_property(&[gtk::accessible::Property::Label(&label)]);
            }
            Status::Connected => {
                set_status_tone(&self.header_status, StatusTone::Connected);
                let label = match (name.as_deref(), active_latency) {
                    (Some(name), Some(ms)) => format!("{name} · {ms} ms"),
                    (Some(name), None) => name.to_string(),
                    (None, Some(ms)) => format!("Connected · {ms} ms"),
                    (None, None) => "Connected".to_string(),
                };
                self.header_status_icon
                    .set_icon_name(Some("network-vpn-symbolic"));
                self.header_status_icon.set_visible(true);
                self.header_status_label.set_label(&label);
                self.header_status
                    .set_tooltip_text(Some(&format!("{label} — click to disconnect")));
                self.header_status.set_sensitive(true);
                self.header_status
                    .update_property(&[gtk::accessible::Property::Label("Disconnect VPN")]);
            }
            Status::Error(error) => {
                set_status_tone(&self.header_status, StatusTone::Error);
                self.header_status_icon
                    .set_icon_name(Some("dialog-warning-symbolic"));
                self.header_status_icon.set_visible(true);
                self.header_status_label.set_label("Connection error");
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
            .or_else(|| state.engine.as_ref().map(Engine::status))
            .unwrap_or(Status::Disconnected)
    }

    fn show_message(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
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
            min-height: 32px;
            padding: 4px 10px;
            border-radius: 10px;
            box-shadow: none;
            opacity: 1;
            font-weight: 500;
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
    use std::time::{Duration, Instant};

    use super::{ResponsiveMode, SearchState, active_probe_is_due, responsive_mode_for_width};

    #[test]
    fn responsive_modes_include_their_upper_boundaries() {
        assert_eq!(responsive_mode_for_width(320.0), ResponsiveMode::Compact);
        assert_eq!(responsive_mode_for_width(680.0), ResponsiveMode::Compact);
        assert_eq!(responsive_mode_for_width(700.0), ResponsiveMode::Compact);
        assert_eq!(responsive_mode_for_width(701.0), ResponsiveMode::Wide);
    }

    #[test]
    fn periodic_probe_requires_one_due_active_server() {
        let now = Instant::now();
        let due = Some(now - Duration::from_secs(1));
        assert!(active_probe_is_due(
            now, due, true, true, false, true, false
        ));
        assert!(!active_probe_is_due(
            now, due, true, true, true, true, false
        ));
        assert!(!active_probe_is_due(
            now, due, true, false, false, true, false
        ));
        assert!(!active_probe_is_due(
            now, due, true, true, false, true, true
        ));
        assert!(!active_probe_is_due(
            now,
            Some(now + Duration::from_secs(1)),
            true,
            true,
            false,
            true,
            false
        ));
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
