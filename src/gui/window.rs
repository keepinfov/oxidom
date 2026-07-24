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
use crate::ipc::{ProbeState, StatusInfo};
use crate::model::Subscription;
use crate::xray::core::Status;
use crate::{paths, sysproxy};

use super::client::DaemonClient;
use super::operation::{UiOperation, UiOperationKind};
use super::server_card::LatencyState;
use super::sidebar::{Page, Sidebar};
use super::views::logs::LogsView;
use super::views::servers::ServersView;
use super::views::settings::{SettingsValues, SettingsView};
use super::views::subscriptions::SubscriptionsView;

type SettingsCallback = Rc<dyn Fn(SettingsValues)>;

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
    servers: ServersView,
    subscriptions: SubscriptionsView,
    settings: SettingsView,
    logs: LogsView,
    toasts: adw::ToastOverlay,
    force_close: Cell<bool>,
    close_after_apply: Cell<bool>,
    /// True while this GUI holds the GNOME system proxy applied.
    proxy_applied: Cell<bool>,
    /// Last (active, connecting) pair pushed to the cards, to avoid an
    /// O(cards) pass on every poll tick.
    applied_connection: RefCell<(Option<String>, Option<String>)>,
    poll_in_flight: Arc<AtomicBool>,
    poll_snapshot: Arc<Mutex<Option<PolledSnapshot>>>,
}

pub fn build(app: &adw::Application) {
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
                Some(&error.to_string()),
            );
            dialog.add_response("close", "Close");
            dialog.present();
            return;
        }
    };
    let subscriptions_snapshot = client.subscriptions().unwrap_or_default();
    let initial_status = client.status().unwrap_or_default();
    let initial_config = client.settings().unwrap_or_default();
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
        servers,
        subscriptions,
        settings,
        logs,
        toasts,
        force_close: Cell::new(false),
        close_after_apply: Cell::new(false),
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

    window.present();

    // Repair a system proxy left over from a previous GUI run and reflect
    // the daemon's current connection on the cards.
    controller.reconcile_system_proxy();
    controller.sync_connection_cards();
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
                if controller.force_close.get() || !controller.settings.has_unsaved_changes() {
                    // The daemon owns the tunnel; closing the GUI leaves the
                    // connection (and the system proxy) as they are.
                    return glib::Propagation::Proceed;
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
                    {
                        let mut state = controller.state.borrow_mut();
                        state.pending_status = Some(Status::Error(error.to_string()));
                        state.connected_id = None;
                    }
                    controller.set_cards_connection(None, None);
                    controller.show_message(&format!("Could not connect: {error}"));
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
        };
        self.client_job(
            UiOperation::new(UiOperationKind::ApplySettings),
            move |client| client.apply_settings(&config),
            move |controller, result| {
                match result {
                    Ok(outcome) => {
                        controller.settings.mark_applied(values.clone());
                        if let Some(error) = outcome.reconnect_error {
                            {
                                let mut state = controller.state.borrow_mut();
                                state.pending_status = Some(Status::Error(error.clone()));
                                state.connected_id = None;
                            }
                            controller.set_cards_connection(None, None);
                            controller.show_message(&format!(
                                "Settings saved, but the connection could not restart: {error}"
                            ));
                        }
                        controller.reconcile_system_proxy();
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
            if !controller.window.is_visible() {
                return glib::ControlFlow::Break;
            }
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
    use super::{ResponsiveMode, SearchState, responsive_mode_for_width};

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
