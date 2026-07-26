use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use crate::config::{Config, LatencyMethod};
use crate::ipc::RuntimeInfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsValues {
    pub socks_port: u16,
    pub http_port: u16,
    pub system_proxy: bool,
    pub reconnect: bool,
    pub latency_method: LatencyMethod,
    pub latency_test_url: String,
    pub subscription_user_agent: String,
    /// Empty means "let the daemon fall back to $OXIDOM_XRAY_BIN, then $PATH".
    pub xray_binary: String,
}

impl From<&Config> for SettingsValues {
    fn from(config: &Config) -> Self {
        Self {
            socks_port: config.socks_port,
            http_port: config.http_port,
            system_proxy: config.system_proxy,
            reconnect: config.reconnect,
            latency_method: config.latency_method,
            latency_test_url: config.latency_test_url.clone(),
            subscription_user_agent: config.subscription_user_agent.clone(),
            xray_binary: config.xray_binary.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsState {
    pub dirty: bool,
    pub valid: bool,
    pub applying: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettingsValidation {
    pub ports: Option<&'static str>,
    pub latency_url: Option<&'static str>,
    pub xray_binary: Option<&'static str>,
}

impl SettingsValidation {
    pub fn is_valid(self) -> bool {
        self.ports.is_none() && self.latency_url.is_none() && self.xray_binary.is_none()
    }
}

#[derive(Debug)]
struct SettingsModel {
    applied: SettingsValues,
    draft: SettingsValues,
    applying: bool,
}

impl SettingsModel {
    fn new(applied: SettingsValues) -> Self {
        Self {
            draft: applied.clone(),
            applied,
            applying: false,
        }
    }

    fn state(&self) -> SettingsState {
        SettingsState {
            dirty: self.draft != self.applied,
            valid: validate(&self.draft).is_valid(),
            applying: self.applying,
        }
    }

    fn reset(&mut self) {
        self.draft = self.applied.clone();
    }

    fn mark_applied(&mut self, values: SettingsValues) {
        self.applied = values;
        self.applying = false;
    }
}

#[derive(Clone)]
struct SettingsWidgets {
    socks: adw::SpinRow,
    http: adw::SpinRow,
    system_proxy: adw::SwitchRow,
    reconnect: adw::SwitchRow,
    method: adw::ComboRow,
    test_url: adw::EntryRow,
    user_agent: adw::EntryRow,
    ua_preset: adw::ComboRow,
    xray_binary: adw::EntryRow,
    xray_effective: adw::ActionRow,
    ports_error: gtk::Label,
    url_error: gtk::Label,
    xray_error: gtk::Label,
    apply: gtk::Button,
    reset: gtk::Button,
}

impl SettingsWidgets {
    fn values(&self) -> SettingsValues {
        SettingsValues {
            socks_port: self.socks.value() as u16,
            http_port: self.http.value() as u16,
            system_proxy: self.system_proxy.is_active(),
            reconnect: self.reconnect.is_active(),
            latency_method: match self.method.selected() {
                0 => LatencyMethod::Icmp,
                1 => LatencyMethod::Tcp,
                2 => LatencyMethod::HttpHead,
                _ => LatencyMethod::HttpGet,
            },
            latency_test_url: self.test_url.text().to_string(),
            subscription_user_agent: self.user_agent.text().to_string(),
            // Trimmed here so trailing whitespace never counts as an edit and
            // never reaches the daemon's path resolution.
            xray_binary: self.xray_binary.text().trim().to_string(),
        }
    }

    fn set_values(&self, values: &SettingsValues) {
        self.socks.set_value(f64::from(values.socks_port));
        self.http.set_value(f64::from(values.http_port));
        self.system_proxy.set_active(values.system_proxy);
        self.reconnect.set_active(values.reconnect);
        self.method.set_selected(match values.latency_method {
            LatencyMethod::Icmp => 0,
            LatencyMethod::Tcp => 1,
            LatencyMethod::HttpHead => 2,
            LatencyMethod::HttpGet => 3,
        });
        self.test_url.set_text(&values.latency_test_url);
        self.user_agent.set_text(&values.subscription_user_agent);
        self.xray_binary.set_text(&values.xray_binary);
    }
}

#[derive(Clone)]
pub struct SettingsView {
    pub root: adw::PreferencesPage,
    widgets: SettingsWidgets,
    model: Rc<RefCell<SettingsModel>>,
    updating_widgets: Rc<Cell<bool>>,
}

/// Recognized subscription client identifiers. Picking one fills the editable
/// User-Agent field; the field itself stays the source of truth so users can
/// still type a value not listed here.
const UA_PRESETS: &[(&str, &str)] = &[
    ("v2rayNG", "v2rayNG/1.9.5"),
    ("Happ", "Happ/3.13.0"),
    ("v2rayN", "v2rayN/6.45"),
    ("Streisand", "Streisand"),
    ("Hiddify", "Hiddify/2.0.5"),
    ("NekoBox", "NekoBox/1.3.5"),
    ("Shadowrocket", "Shadowrocket/2.2.9"),
    ("Clash Meta", "clash-verge/1.7.7"),
    ("sing-box", "SFA/1.10.0"),
];

impl SettingsView {
    /// Builds a settings editor. `on_apply` is called only after the user
    /// explicitly activates Apply; editing never persists configuration.
    pub fn new(config: &Config, on_apply: impl Fn(SettingsValues) + 'static) -> Self {
        let applied = SettingsValues::from(config);
        let socks = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
        socks.set_title("SOCKS port");
        socks.set_subtitle("Local port other apps can use as a SOCKS5 proxy");
        socks.set_value(f64::from(applied.socks_port));
        let http = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
        http.set_title("HTTP port");
        http.set_subtitle("Local port other apps can use as an HTTP proxy");
        http.set_value(f64::from(applied.http_port));
        let system_proxy = adw::SwitchRow::builder()
            .title("System proxy")
            .subtitle("Send the whole desktop's traffic through oxidom while connected (GNOME)")
            .active(applied.system_proxy)
            .build();
        let reconnect = adw::SwitchRow::builder()
            .title("Reconnect automatically")
            .subtitle("Reconnect only when Xray exits unexpectedly, never after Disconnect")
            .active(applied.reconnect)
            .build();

        let methods = gtk::StringList::new(&["ICMP", "TCP", "HTTP HEAD", "HTTP GET"]);
        let initial_method = match applied.latency_method {
            LatencyMethod::Icmp => 0,
            LatencyMethod::Tcp => 1,
            LatencyMethod::HttpHead => 2,
            LatencyMethod::HttpGet => 3,
        };
        let method = adw::ComboRow::builder()
            .title("Latency method")
            .subtitle(method_subtitle(initial_method))
            .model(&methods)
            .selected(initial_method)
            .build();
        let test_url = adw::EntryRow::builder()
            .title("Latency test URL")
            .text(&applied.latency_test_url)
            // Only the HTTP methods request a URL; keep the row visibly
            // inert otherwise instead of silently ignoring edits.
            .sensitive(initial_method >= 2)
            .build();
        method.connect_selected_notify({
            let test_url = test_url.clone();
            move |row| {
                row.set_subtitle(method_subtitle(row.selected()));
                test_url.set_sensitive(row.selected() >= 2);
            }
        });
        let user_agent = adw::EntryRow::builder()
            .title("Subscription User-Agent")
            .text(&applied.subscription_user_agent)
            .build();
        let preset_labels: Vec<&str> = std::iter::once("Custom")
            .chain(UA_PRESETS.iter().map(|(label, _)| *label))
            .collect();
        let presets = gtk::StringList::new(&preset_labels);
        let selected_preset = preset_for_user_agent(&applied.subscription_user_agent);
        let ua_preset = adw::ComboRow::builder()
            .title("Client preset")
            .subtitle("Fills the User-Agent below")
            .model(&presets)
            .selected(selected_preset)
            .build();

        let xray_binary = adw::EntryRow::builder()
            .title("Xray binary")
            .text(&applied.xray_binary)
            .build();
        // Filled from the daemon over D-Bus, never computed here: the daemon
        // is a separate process, usually a different user, with its own $PATH.
        let xray_effective = adw::ActionRow::builder()
            .title("In use by the daemon")
            .subtitle("Checking…")
            .subtitle_selectable(true)
            .build();
        xray_effective.add_css_class("property");

        let ports_error = validation_label();
        let url_error = validation_label();
        let xray_error = validation_label();

        let proxy_group = adw::PreferencesGroup::builder()
            .title("Local proxy")
            .build();
        proxy_group.add(&socks);
        proxy_group.add(&http);
        proxy_group.add(&ports_error);
        proxy_group.add(&system_proxy);
        proxy_group.add(&reconnect);
        let xray_group = adw::PreferencesGroup::builder()
            .title("Xray core")
            .description(
                "Leave empty to use $OXIDOM_XRAY_BIN, then the first xray on PATH. \
                 A system-wide oxidom service cannot read paths under /home.",
            )
            .build();
        xray_group.add(&xray_binary);
        xray_group.add(&xray_error);
        xray_group.add(&xray_effective);
        let latency_group = adw::PreferencesGroup::builder()
            .title("Latency")
            .description("HTTP checks use the active local SOCKS proxy")
            .build();
        latency_group.add(&method);
        latency_group.add(&test_url);
        latency_group.add(&url_error);

        let advanced = adw::ExpanderRow::builder()
            .title("Advanced")
            .subtitle("Technical subscription compatibility settings")
            .build();
        advanced.add_row(&ua_preset);
        advanced.add_row(&user_agent);
        let advanced_group = adw::PreferencesGroup::new();
        advanced_group.add(&advanced);

        let root = adw::PreferencesPage::new();
        root.add(&proxy_group);
        root.add(&xray_group);
        root.add(&latency_group);
        root.add(&advanced_group);

        let apply = gtk::Button::builder()
            .label("Apply")
            .tooltip_text("Apply settings")
            .sensitive(false)
            .css_classes(["suggested-action"])
            .build();
        let reset = gtk::Button::builder()
            .label("Reset")
            .tooltip_text("Reset unsaved settings")
            .sensitive(false)
            .build();
        apply.update_property(&[gtk::accessible::Property::Label("Apply settings")]);
        reset.update_property(&[gtk::accessible::Property::Label("Reset unsaved settings")]);
        let widgets = SettingsWidgets {
            socks,
            http,
            system_proxy,
            reconnect,
            method,
            test_url,
            user_agent,
            ua_preset,
            xray_binary,
            xray_effective,
            ports_error,
            url_error,
            xray_error,
            apply,
            reset,
        };
        let model = Rc::new(RefCell::new(SettingsModel::new(applied)));
        let updating_widgets = Rc::new(Cell::new(false));

        connect_draft_signals(&widgets, &model, &updating_widgets);
        widgets.apply.connect_clicked({
            let model = model.clone();
            let widgets = widgets.clone();
            move |_| {
                let draft = {
                    let mut model = model.borrow_mut();
                    let state = model.state();
                    if !state.dirty || !state.valid || state.applying {
                        return;
                    }
                    model.applying = true;
                    model.draft.clone()
                };
                refresh_state(&widgets, &model);
                on_apply(draft);
            }
        });
        widgets.reset.connect_clicked({
            let model = model.clone();
            let widgets = widgets.clone();
            let updating_widgets = updating_widgets.clone();
            move |_| {
                let values = {
                    let mut model = model.borrow_mut();
                    if model.applying {
                        return;
                    }
                    model.reset();
                    model.draft.clone()
                };
                updating_widgets.set(true);
                widgets.set_values(&values);
                sync_preset(&widgets);
                updating_widgets.set(false);
                refresh_state(&widgets, &model);
            }
        });
        refresh_state(&widgets, &model);

        Self {
            root,
            widgets,
            model,
            updating_widgets,
        }
    }

    /// A header-bar-ready Apply button. The returned GTK object is a clone
    /// referring to the same widget owned by this view.
    pub fn apply_button(&self) -> gtk::Button {
        self.widgets.apply.clone()
    }

    /// A header-bar-ready Reset button.
    pub fn reset_button(&self) -> gtk::Button {
        self.widgets.reset.clone()
    }

    pub fn set_ultra_compact(&self, enabled: bool) {
        if enabled {
            self.widgets.apply.set_icon_name("object-select-symbolic");
            self.widgets.reset.set_icon_name("edit-undo-symbolic");
            self.widgets.apply.add_css_class("header-icon-button");
            self.widgets.reset.add_css_class("header-icon-button");
        } else {
            self.widgets.apply.set_label("Apply");
            self.widgets.reset.set_label("Reset");
            self.widgets.apply.remove_css_class("header-icon-button");
            self.widgets.reset.remove_css_class("header-icon-button");
        }
    }

    pub fn state(&self) -> SettingsState {
        self.model.borrow().state()
    }

    pub fn draft(&self) -> SettingsValues {
        self.model.borrow().draft.clone()
    }

    pub fn applied(&self) -> SettingsValues {
        self.model.borrow().applied.clone()
    }

    pub fn validation(&self) -> SettingsValidation {
        validate(&self.model.borrow().draft)
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.state().dirty
    }

    /// Requests Apply through the same guarded path as the header button.
    /// This is useful for the Apply branch of an unsaved-changes dialog.
    pub fn request_apply(&self) {
        self.widgets.apply.emit_clicked();
    }

    /// Completes a successful asynchronous apply. If the user edited another
    /// value while the operation was running, that newer draft remains dirty.
    pub fn mark_applied(&self, values: SettingsValues) {
        self.model.borrow_mut().mark_applied(values);
        refresh_state(&self.widgets, &self.model);
    }

    /// Completes a failed/cancelled asynchronous apply without accepting the
    /// draft, allowing the user to try again.
    pub fn set_apply_in_progress(&self, applying: bool) {
        self.model.borrow_mut().applying = applying;
        refresh_state(&self.widgets, &self.model);
    }

    /// Adopts what the daemon reports about itself: the Xray path it actually
    /// resolved, and any ports pinned by its service unit. Locked rows go
    /// insensitive and snap to the daemon's values, so a field can never claim
    /// something the daemon would silently override.
    ///
    /// `None` means the daemon predates this call — leave everything editable
    /// rather than guessing.
    pub fn set_runtime_info(&self, info: Option<&RuntimeInfo>) {
        let widgets = &self.widgets;
        let Some(info) = info else {
            widgets
                .xray_effective
                .set_subtitle("Unavailable — this daemon is older than the app");
            widgets.xray_effective.remove_css_class("error");
            return;
        };

        match (&info.xray_path, &info.xray_error) {
            (Some(path), _) => {
                let source = info
                    .xray_source
                    .map(|source| format!(" (from {})", source.label()))
                    .unwrap_or_default();
                widgets
                    .xray_effective
                    .set_subtitle(&format!("{path}{source}"));
                widgets.xray_effective.remove_css_class("error");
            }
            (None, Some(error)) => {
                widgets.xray_effective.set_subtitle(error);
                widgets.xray_effective.add_css_class("error");
            }
            (None, None) => {
                widgets.xray_effective.set_subtitle("Unknown");
                widgets.xray_effective.remove_css_class("error");
            }
        }

        const LOCKED: &str = "Fixed by the system service unit";
        for (locked, port, row, editable) in [
            (
                info.socks_port_locked,
                info.socks_port,
                &widgets.socks,
                "Local port other apps can use as a SOCKS5 proxy",
            ),
            (
                info.http_port_locked,
                info.http_port,
                &widgets.http,
                "Local port other apps can use as an HTTP proxy",
            ),
        ] {
            row.set_sensitive(!locked);
            row.set_subtitle(if locked { LOCKED } else { editable });
            if locked && port != 0 {
                self.updating_widgets.set(true);
                row.set_value(f64::from(port));
                self.updating_widgets.set(false);
            }
        }

        if info.socks_port_locked || info.http_port_locked {
            let mut model = self.model.borrow_mut();
            let SettingsModel { applied, draft, .. } = &mut *model;
            for values in [applied, draft] {
                if info.socks_port_locked && info.socks_port != 0 {
                    values.socks_port = info.socks_port;
                }
                if info.http_port_locked && info.http_port != 0 {
                    values.http_port = info.http_port;
                }
            }
        }
        refresh_state(&self.widgets, &self.model);
    }

    /// Discards the draft and restores the last successfully applied values.
    pub fn reset_draft(&self) {
        if self.model.borrow().applying {
            return;
        }
        let values = {
            let mut model = self.model.borrow_mut();
            model.reset();
            model.draft.clone()
        };
        self.updating_widgets.set(true);
        self.widgets.set_values(&values);
        sync_preset(&self.widgets);
        self.updating_widgets.set(false);
        refresh_state(&self.widgets, &self.model);
    }
}

fn validation_label() -> gtk::Label {
    gtk::Label::builder()
        .halign(gtk::Align::Start)
        .wrap(true)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["error"])
        .visible(false)
        .build()
}

fn preset_for_user_agent(user_agent: &str) -> u32 {
    UA_PRESETS
        .iter()
        .position(|(_, ua)| *ua == user_agent)
        .map(|index| index as u32 + 1)
        .unwrap_or(0)
}

fn sync_preset(widgets: &SettingsWidgets) {
    let selected = preset_for_user_agent(&widgets.user_agent.text());
    if widgets.ua_preset.selected() != selected {
        widgets.ua_preset.set_selected(selected);
    }
}

fn connect_draft_signals(
    widgets: &SettingsWidgets,
    model: &Rc<RefCell<SettingsModel>>,
    updating_widgets: &Rc<Cell<bool>>,
) {
    let update = {
        let widgets = widgets.clone();
        let model = model.clone();
        let updating_widgets = updating_widgets.clone();
        Rc::new(move || {
            if updating_widgets.get() {
                return;
            }
            model.borrow_mut().draft = widgets.values();
            refresh_state(&widgets, &model);
        })
    };
    widgets.socks.connect_value_notify({
        let update = update.clone();
        move |_| update()
    });
    widgets.http.connect_value_notify({
        let update = update.clone();
        move |_| update()
    });
    widgets.system_proxy.connect_active_notify({
        let update = update.clone();
        move |_| update()
    });
    widgets.reconnect.connect_active_notify({
        let update = update.clone();
        move |_| update()
    });
    widgets.method.connect_selected_notify({
        let update = update.clone();
        move |_| update()
    });
    widgets.test_url.connect_changed({
        let update = update.clone();
        move |_| update()
    });
    widgets.ua_preset.connect_selected_notify({
        let user_agent = widgets.user_agent.clone();
        move |row| {
            if let Some((_, value)) = UA_PRESETS.get(row.selected().wrapping_sub(1) as usize)
                && user_agent.text() != *value
            {
                user_agent.set_text(value);
            }
        }
    });
    widgets.xray_binary.connect_changed({
        let update = update.clone();
        move |_| update()
    });
    widgets.user_agent.connect_changed({
        let widgets = widgets.clone();
        move |_| {
            sync_preset(&widgets);
            update();
        }
    });
}

fn refresh_state(widgets: &SettingsWidgets, model: &Rc<RefCell<SettingsModel>>) {
    let model = model.borrow();
    let validation = validate(&model.draft);
    let state = model.state();
    drop(model);

    set_validation_message(&widgets.ports_error, validation.ports);
    set_validation_message(&widgets.url_error, validation.latency_url);
    set_validation_message(&widgets.xray_error, validation.xray_binary);
    widgets
        .apply
        .set_sensitive(state.dirty && state.valid && !state.applying);
    widgets.reset.set_sensitive(state.dirty && !state.applying);
}

fn set_validation_message(label: &gtk::Label, message: Option<&str>) {
    label.set_label(message.unwrap_or_default());
    label.set_visible(message.is_some());
}

fn method_subtitle(index: u32) -> &'static str {
    match index {
        0 => "Regular ping straight to the server, outside the tunnel",
        1 => "Time to open a direct connection to the server, outside the tunnel",
        // Say that the request goes *through the server*: that is the whole
        // difference from the two above, and the only thing that catches a
        // server which completes a handshake and then carries nothing.
        2 => "Small web request through each server; slower, and starts a core per check",
        _ => "Full web request through each server — the most realistic check",
    }
}

fn validate(values: &SettingsValues) -> SettingsValidation {
    // The SpinRows only allow 1..=65535, but a hand-edited config.toml can
    // still carry 0 into the draft.
    let ports = if values.socks_port == 0 || values.http_port == 0 {
        Some("Ports must be between 1 and 65535")
    } else if values.socks_port == values.http_port {
        Some("SOCKS and HTTP ports must be different")
    } else {
        None
    };
    // The URL only matters for the HTTP methods; don't block Apply on it when
    // the selected method never uses it.
    let url_in_use = matches!(
        values.latency_method,
        LatencyMethod::HttpHead | LatencyMethod::HttpGet
    );
    let latency_url = match url::Url::parse(&values.latency_test_url) {
        _ if !url_in_use => None,
        Ok(url)
            if matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none() =>
        {
            None
        }
        _ => Some("Enter a valid HTTP or HTTPS URL"),
    };
    // Syntax only. Whether the file exists and runs is the daemon's verdict —
    // it may be another user with another $PATH and no access to /home.
    let path = values.xray_binary.trim();
    let xray_binary = if path.starts_with('~') {
        Some("Use a full path — ~ is not expanded")
    } else if path.contains('/') && !path.starts_with('/') {
        Some("Enter an absolute path, or a bare command name to search PATH")
    } else {
        None
    };
    SettingsValidation {
        ports,
        latency_url,
        xray_binary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> SettingsValues {
        SettingsValues {
            socks_port: 10808,
            http_port: 10809,
            system_proxy: false,
            reconnect: false,
            latency_method: LatencyMethod::HttpGet,
            latency_test_url: "https://www.gstatic.com/generate_204".into(),
            subscription_user_agent: "oxidom/test".into(),
            xray_binary: String::new(),
        }
    }

    #[test]
    fn settings_values_dirty_state_and_reset() {
        let mut model = SettingsModel::new(values());
        assert_eq!(
            model.state(),
            SettingsState {
                dirty: false,
                valid: true,
                applying: false,
            }
        );

        model.draft.system_proxy = true;
        assert!(model.state().dirty);
        model.reset();
        assert_eq!(model.draft, model.applied);
        assert!(!model.state().dirty);
    }

    #[test]
    fn settings_validation_rejects_equal_or_zero_ports() {
        let mut draft = values();
        draft.http_port = draft.socks_port;
        assert_eq!(
            validate(&draft).ports,
            Some("SOCKS and HTTP ports must be different")
        );

        draft.http_port = 0;
        assert_eq!(
            validate(&draft).ports,
            Some("Ports must be between 1 and 65535")
        );
    }

    #[test]
    fn settings_validation_requires_http_url_without_credentials() {
        for invalid in [
            "",
            "example.com/probe",
            "ftp://example.com/probe",
            "https://",
            "https://user:secret@example.com/probe",
        ] {
            let mut draft = values();
            draft.latency_test_url = invalid.into();
            assert!(
                validate(&draft).latency_url.is_some(),
                "{invalid:?} should be rejected"
            );
        }

        for valid in ["http://example.com", "https://example.com/probe?q=1"] {
            let mut draft = values();
            draft.latency_test_url = valid.into();
            assert!(
                validate(&draft).latency_url.is_none(),
                "{valid:?} should be accepted"
            );
        }
    }

    #[test]
    fn settings_validation_checks_xray_path_syntax_only() {
        // Existence is the daemon's call; only unusable syntax is rejected here.
        for invalid in ["~/bin/xray", "bin/xray", "./xray"] {
            let mut draft = values();
            draft.xray_binary = invalid.into();
            assert!(
                validate(&draft).xray_binary.is_some(),
                "{invalid:?} should be rejected"
            );
        }

        for valid in ["", "xray", "/nix/store/abc-xray/bin/xray", "/usr/bin/xray"] {
            let mut draft = values();
            draft.xray_binary = valid.into();
            assert!(
                validate(&draft).xray_binary.is_none(),
                "{valid:?} should be accepted"
            );
        }
    }

    #[test]
    fn successful_apply_moves_baseline_without_losing_newer_draft() {
        let original = values();
        let mut model = SettingsModel::new(original.clone());
        let mut submitted = original;
        submitted.system_proxy = true;
        model.draft = submitted.clone();
        model.applying = true;

        model.draft.subscription_user_agent = "newer edit".into();
        model.mark_applied(submitted);

        assert!(!model.state().applying);
        assert!(model.state().dirty);
        assert_eq!(model.draft.subscription_user_agent, "newer edit");
    }
}
