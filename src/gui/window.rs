use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;

use crate::engine::Engine;
use crate::model::{Server, Subscription};
use crate::xray::core::Status;

use super::sidebar::{Page, Sidebar};
use super::views::logs::LogsView;
use super::views::servers::ServersView;
use super::views::settings::{SettingsValues, SettingsView};
use super::views::subscriptions::SubscriptionsView;

type SettingsCallback = Rc<dyn Fn(SettingsValues)>;

struct AppState {
    engine: Option<Engine>,
    subscriptions: Vec<Subscription>,
    selected_id: Option<String>,
    latencies: HashMap<String, Option<u32>>,
    pending_status: Option<Status>,
}

struct Controller {
    window: adw::ApplicationWindow,
    state: Rc<RefCell<AppState>>,
    split: adw::OverlaySplitView,
    search: gtk::SearchEntry,
    sidebar_toggle: gtk::Button,
    connect: gtk::Button,
    connect_label: gtk::Label,
    connect_spinner: gtk::Spinner,
    sidebar_status_icon: gtk::Image,
    sidebar_status_label: gtk::Label,
    servers: ServersView,
    subscriptions: SubscriptionsView,
    logs: LogsView,
    toasts: adw::ToastOverlay,
    probe_generation: Cell<u64>,
}

pub fn build(app: &adw::Application) {
    install_css();

    let engine = Engine::load();
    let subscriptions_snapshot = engine.subscriptions.clone();
    let selected_id = engine.state.active_server_id.clone();
    let initial_config = engine.config.clone();
    let state = Rc::new(RefCell::new(AppState {
        engine: Some(engine),
        subscriptions: subscriptions_snapshot,
        selected_id,
        latencies: HashMap::new(),
        pending_status: None,
    }));

    let servers = ServersView::new();
    let subscriptions = SubscriptionsView::new();
    let logs = LogsView::new();
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
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
    stack.add_named(&settings.root, Some(Page::Settings.stack_name()));
    stack.add_named(&logs.root, Some(Page::Logs.stack_name()));

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search servers")
        .hexpand(true)
        .max_width_chars(48)
        .build();
    let sidebar_toggle = gtk::Button::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Show sidebar")
        .visible(false)
        .build();
    let connect_spinner = gtk::Spinner::new();
    let connect_label = gtk::Label::new(Some("Connect"));
    let connect_content = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    connect_content.append(&connect_spinner);
    connect_content.append(&connect_label);
    let connect = gtk::Button::builder()
        .child(&connect_content)
        .css_classes(["suggested-action", "pill"])
        .build();

    let header = adw::HeaderBar::new();
    header.pack_start(&sidebar_toggle);
    header.set_title_widget(Some(&search));
    header.pack_end(&connect);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&stack);

    let stack_for_sidebar = stack.clone();
    let search_for_sidebar = search.clone();
    let split_holder: Rc<RefCell<Option<adw::OverlaySplitView>>> = Rc::new(RefCell::new(None));
    let split_for_sidebar = split_holder.clone();
    let sidebar = Sidebar::new(move |page| {
        stack_for_sidebar.set_visible_child_name(page.stack_name());
        search_for_sidebar.set_visible(page == Page::General);
        if let Some(split) = split_for_sidebar.borrow().as_ref() {
            if split.is_collapsed() {
                split.set_show_sidebar(false);
            }
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
    *split_holder.borrow_mut() = Some(split.clone());

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("oxidom")
        .default_width(1100)
        .default_height(720)
        .content(&toasts)
        .build();

    let controller = Rc::new(Controller {
        window: window.clone(),
        state,
        split,
        search,
        sidebar_toggle,
        connect,
        connect_label,
        connect_spinner,
        sidebar_status_icon: sidebar.status_icon,
        sidebar_status_label: sidebar.status_label,
        servers,
        subscriptions,
        logs,
        toasts,
        probe_generation: Cell::new(0),
    });

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
    controller.start_probes();
    controller.add_breakpoint();
    controller.start_timer();
    window.present();
}

impl Controller {
    fn wire_actions(self: &Rc<Self>) {
        self.search.connect_search_changed({
            let weak = Rc::downgrade(self);
            move |entry| {
                if let Some(controller) = weak.upgrade() {
                    controller.servers.set_query(&entry.text());
                }
            }
        });
        self.sidebar_toggle.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.split.set_show_sidebar(true);
                }
            }
        });
        self.connect.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.toggle_connection();
                }
            }
        });
    }

    fn add_breakpoint(self: &Rc<Self>) {
        let Ok(condition) = adw::BreakpointCondition::parse("max-width: 700px") else {
            return;
        };
        let breakpoint = adw::Breakpoint::new(condition);
        breakpoint.connect_apply({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.split.set_pin_sidebar(false);
                    controller.split.set_collapsed(true);
                    controller.split.set_show_sidebar(false);
                    controller.sidebar_toggle.set_visible(true);
                    controller.servers.set_narrow(true);
                }
            }
        });
        breakpoint.connect_unapply({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.split.set_collapsed(false);
                    controller.split.set_pin_sidebar(true);
                    controller.split.set_show_sidebar(true);
                    controller.sidebar_toggle.set_visible(false);
                    controller.servers.set_narrow(false);
                }
            }
        });
        self.window.add_breakpoint(breakpoint);
    }

    fn rebuild_views(self: &Rc<Self>) {
        let (subscriptions, selected_id, latencies) = {
            let state = self.state.borrow();
            (
                state.subscriptions.clone(),
                state.selected_id.clone(),
                state.latencies.clone(),
            )
        };
        self.servers
            .rebuild(&subscriptions, selected_id.as_deref(), &latencies, {
                let weak = Rc::downgrade(self);
                Rc::new(move |id| {
                    if let Some(controller) = weak.upgrade() {
                        controller.connect_server(id);
                    }
                })
            });

        let add = {
            let weak = Rc::downgrade(self);
            Rc::new(move |url, name| {
                if let Some(controller) = weak.upgrade() {
                    controller.add_subscription(url, name);
                }
            }) as Rc<dyn Fn(String, Option<String>)>
        };
        let refresh = {
            let weak = Rc::downgrade(self);
            Rc::new(move |id| {
                if let Some(controller) = weak.upgrade() {
                    controller.refresh_subscription(id);
                }
            }) as Rc<dyn Fn(String)>
        };
        let remove = {
            let weak = Rc::downgrade(self);
            Rc::new(move |id| {
                if let Some(controller) = weak.upgrade() {
                    controller.remove_subscription(id);
                }
            }) as Rc<dyn Fn(String)>
        };
        let hwid = {
            let weak = Rc::downgrade(self);
            Rc::new(move |id, enabled| {
                if let Some(controller) = weak.upgrade() {
                    controller.set_hwid(id, enabled);
                }
            }) as Rc<dyn Fn(String, bool)>
        };
        self.subscriptions
            .rebuild(&subscriptions, add, refresh, remove, hwid);
    }

    fn toggle_connection(self: &Rc<Self>) {
        let (status, selected) = {
            let state = self.state.borrow();
            (self.current_status(&state), state.selected_id.clone())
        };
        match status {
            Status::Connected | Status::Connecting => self.disconnect(),
            Status::Disconnected | Status::Error(_) => {
                if let Some(id) = selected {
                    self.connect_server(id);
                } else {
                    self.show_message("Select a server first");
                }
            }
        }
    }

    fn connect_server(self: &Rc<Self>, server_id: String) {
        if self.state.borrow().engine.is_none() {
            self.show_message("Another operation is still running");
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            state.selected_id = Some(server_id.clone());
            state.pending_status = Some(Status::Connecting);
        }
        self.servers.set_active(Some(&server_id));
        self.refresh_status();
        self.engine_job(
            move |engine| engine.connect(&server_id),
            move |controller, result| {
                if let Err(error) = result {
                    controller.state.borrow_mut().pending_status =
                        Some(Status::Error(error.to_string()));
                    controller.show_message(&format!("Could not connect: {error}"));
                } else {
                    controller.state.borrow_mut().pending_status = Some(Status::Connecting);
                    controller.start_probes();
                }
                controller.refresh_status();
            },
        );
    }

    fn disconnect(self: &Rc<Self>) {
        self.state.borrow_mut().pending_status = Some(Status::Disconnected);
        self.refresh_status();
        self.engine_job(
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

    fn add_subscription(self: &Rc<Self>, url: String, name: Option<String>) {
        self.engine_job(
            move |engine| engine.add_subscription(url, name),
            |controller, result| controller.finish_subscription_change("add subscription", result),
        );
    }

    fn refresh_subscription(self: &Rc<Self>, subscription_id: String) {
        self.engine_job(
            move |engine| engine.refresh(&subscription_id),
            |controller, result| {
                controller.finish_subscription_change("update subscription", result)
            },
        );
    }

    fn remove_subscription(self: &Rc<Self>, subscription_id: String) {
        self.engine_job(
            move |engine| engine.remove_subscription(&subscription_id),
            |controller, result| {
                controller.finish_subscription_change("delete subscription", result)
            },
        );
    }

    fn finish_subscription_change(self: &Rc<Self>, action: &str, result: Result<()>) {
        if let Err(error) = result {
            self.show_message(&format!("Could not {action}: {error}"));
            return;
        }
        self.rebuild_views();
        self.start_probes();
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

    fn save_settings(&self, values: SettingsValues) {
        let result = {
            let mut state = self.state.borrow_mut();
            let Some(engine) = state.engine.as_mut() else {
                return;
            };
            engine.config.socks_port = values.socks_port;
            engine.config.http_port = values.http_port;
            engine.config.system_proxy = values.system_proxy;
            engine.config.latency_method = values.latency_method;
            engine.config.latency_test_url = values.latency_test_url;
            engine.config.subscription_user_agent = values.subscription_user_agent;
            engine.core.socks_port = values.socks_port;
            engine.core.http_port = values.http_port;
            engine.save()
        };
        if let Err(error) = result {
            self.show_message(&format!("Could not save settings: {error}"));
        }
    }

    fn engine_job<R, Work, Complete>(self: &Rc<Self>, work: Work, complete: Complete)
    where
        R: Send + 'static,
        Work: FnOnce(&mut Engine) -> Result<R> + Send + 'static,
        Complete: FnOnce(&Rc<Self>, Result<R>) + 'static,
    {
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
            engine
        };
        self.connect.set_sensitive(false);

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
                    }
                    controller.connect.set_sensitive(true);
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
                        drop(state);
                        controller.connect.set_sensitive(true);
                        controller.refresh_status();
                    }
                    glib::ControlFlow::Break
                }
            }
        });
    }

    fn start_probes(self: &Rc<Self>) {
        let mut servers: Vec<Server> = self
            .state
            .borrow()
            .subscriptions
            .iter()
            .flat_map(|subscription| subscription.servers.iter().cloned())
            .collect();
        if servers.is_empty() {
            return;
        }
        let selected_id = self.state.borrow().selected_id.clone();
        servers.sort_by_key(|server| selected_id.as_deref() != Some(server.id.as_str()));
        let generation = self.probe_generation.get().wrapping_add(1);
        self.probe_generation.set(generation);

        let queue = Arc::new(Mutex::new(VecDeque::from(servers)));
        let (sender, receiver) = mpsc::channel();
        let workers = queue.lock().map(|queue| queue.len().min(4)).unwrap_or(0);
        for _ in 0..workers {
            let queue = queue.clone();
            let sender = sender.clone();
            std::thread::spawn(move || {
                let engine = Engine::load();
                loop {
                    let server = queue.lock().ok().and_then(|mut queue| queue.pop_front());
                    let Some(server) = server else { break };
                    let latency = engine.probe(&server);
                    if sender.send((server.id, latency)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);

        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let Some(controller) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if controller.probe_generation.get() != generation {
                return glib::ControlFlow::Break;
            }
            loop {
                match receiver.try_recv() {
                    Ok((id, latency)) => {
                        let mut state = controller.state.borrow_mut();
                        state.latencies.insert(id.clone(), latency);
                        if let Some(engine) = state.engine.as_mut() {
                            for server in engine
                                .subscriptions
                                .iter_mut()
                                .flat_map(|subscription| subscription.servers.iter_mut())
                            {
                                if server.id == id {
                                    server.latency_ms = latency;
                                    break;
                                }
                            }
                        }
                        if state.selected_id.as_deref() == Some(id.as_str())
                            && matches!(state.pending_status, Some(Status::Connecting))
                        {
                            state.pending_status = Some(match latency {
                                Some(_) => state
                                    .engine
                                    .as_ref()
                                    .map(Engine::status)
                                    .unwrap_or(Status::Disconnected),
                                None => Status::Error("Latency probe failed".to_string()),
                            });
                        }
                        drop(state);
                        controller.servers.set_latency(&id, latency);
                        controller.refresh_status();
                    }
                    Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        controller.refresh_status();
                        return glib::ControlFlow::Break;
                    }
                }
            }
        });
    }

    fn start_timer(self: &Rc<Self>) {
        let controller = self.clone();
        glib::timeout_add_local(Duration::from_millis(500), move || {
            if !controller.window.is_visible() {
                return glib::ControlFlow::Break;
            }
            controller.refresh_status();
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
            .selected_id
            .as_ref()
            .and_then(|id| state.latencies.get(id))
            .copied()
            .flatten();
        drop(state);

        self.connect_spinner.set_spinning(false);
        self.connect_spinner.set_visible(false);
        self.connect.remove_css_class("destructive-action");
        self.connect.remove_css_class("suggested-action");
        match status {
            Status::Disconnected => {
                self.connect.add_css_class("suggested-action");
                self.connect_label.set_label("Connect");
                self.sidebar_status_icon
                    .set_icon_name(Some("network-offline-symbolic"));
                self.sidebar_status_label.set_label("Disconnected");
            }
            Status::Connecting => {
                self.connect_spinner.set_visible(true);
                self.connect_spinner.set_spinning(true);
                self.connect_label.set_label("Connecting…");
                self.sidebar_status_icon
                    .set_icon_name(Some("network-transmit-receive-symbolic"));
                self.sidebar_status_label.set_label("Connecting…");
            }
            Status::Connected => {
                self.connect.add_css_class("destructive-action");
                let connect_text = active_latency
                    .map(|ms| format!("Disconnect · {ms} ms"))
                    .unwrap_or_else(|| "Disconnect".to_string());
                self.connect_label.set_label(&connect_text);
                self.sidebar_status_icon
                    .set_icon_name(Some("network-vpn-symbolic"));
                let status_text = active_latency
                    .map(|ms| format!("Connected · {ms} ms"))
                    .unwrap_or_else(|| "Connected".to_string());
                self.sidebar_status_label.set_label(&status_text);
            }
            Status::Error(error) => {
                self.connect.add_css_class("suggested-action");
                self.connect_label.set_label("Retry");
                self.sidebar_status_icon
                    .set_icon_name(Some("dialog-error-symbolic"));
                self.sidebar_status_label.set_label(&error);
            }
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

fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        r#"
        .sidebar { background: alpha(@window_fg_color, 0.035); }
        .sidebar-status { padding: 10px; border-radius: 12px; background: alpha(@window_fg_color, 0.055); }
        .server-card { border-radius: 14px; background: alpha(@window_fg_color, 0.06); border: 1px solid alpha(@window_fg_color, 0.08); }
        .server-card:hover { background: alpha(@window_fg_color, 0.10); }
        .server-card.active-server { border: 2px solid @accent_color; background: alpha(@accent_color, 0.13); }
        .server-flag { font-size: 28px; }
        .server-globe { min-width: 34px; min-height: 34px; }
        .server-name { font-weight: 700; font-size: 1.05em; }
        .server-subtitle { font-size: 0.9em; }
        .latency-badge { border-radius: 999px; padding: 4px 8px; font-size: 0.85em; font-weight: 700; }
        .latency-good { color: #57e389; background: alpha(#57e389, 0.14); }
        .latency-slow { color: #f8e45c; background: alpha(#f8e45c, 0.14); }
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
