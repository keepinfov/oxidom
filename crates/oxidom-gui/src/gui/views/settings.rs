use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use oxidom_core::config::{Config, LatencyMethod, OnCoreExit};
use oxidom_core::core_options::CoreOptions;
use oxidom_core::ipc::RuntimeInfo;
// Recognized subscription client identifiers: picking one fills the editable
// User-Agent field, which stays the source of truth so a value not listed here
// can still be typed. Shared with a subscription's own override, so the two
// scopes of one choice cannot drift apart.
use oxidom_core::subscription::CLIENT_PRESETS as UA_PRESETS;
use oxidom_core::xray::assets::{self, GEO_PRESETS};

use oxidom_core::client::DaemonSource;

use super::core_editor::{CoreEditor, CoreLevel};
use super::icon_button;
use crate::gui::prefs::ColorScheme;
use crate::gui::reduce::{self, GeoOffer};

/// Restored by [`SettingsView::set_system_proxy_failure`], so the row has one
/// place that owns its normal wording.
const SYSTEM_PROXY_SUBTITLE: &str =
    "Send the whole desktop's traffic through oxidom while connected (GNOME)";

/// Says what the *other* setting would do, because "hold" only means something
/// against the alternative — and the alternative is the one with a consequence
/// worth spelling out.
const HOLD_TRAFFIC_SUBTITLE: &str = "Traffic is dropped until the tunnel reconnects. Turned off, apps fall back to your \
     ordinary connection — with your own address — until it does";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsValues {
    pub socks_port: u16,
    pub http_port: u16,
    pub system_proxy: bool,
    pub reconnect: bool,
    /// What a session does with its routes when its core exits by itself. The
    /// machine's answer; a profile may override it.
    pub hold_traffic: bool,
    pub latency_method: LatencyMethod,
    pub latency_test_url: String,
    pub subscription_user_agent: String,
    /// Where the geo lists are fetched from. Empty means the built-in source,
    /// which is what clearing the field back to the default looks like.
    pub geoip_url: String,
    pub geosite_url: String,
    /// Empty means "use the daemon's managed, pinned Xray core".
    pub xray_binary: String,
    /// Kept in the draft so applying unrelated GUI settings cannot erase a
    /// path configured outside the GUI.
    pub tun2socks_binary: String,
    /// Kept for the same reason as tun2socks.
    pub nft_binary: String,
    /// The machine-wide `[core]`. Every profile that does not override a
    /// section inherits it, and so do latency probes.
    pub core: CoreOptions,
}

impl From<&Config> for SettingsValues {
    fn from(config: &Config) -> Self {
        Self {
            socks_port: config.socks_port,
            http_port: config.http_port,
            system_proxy: config.system_proxy,
            reconnect: config.reconnect,
            hold_traffic: config.on_core_exit == OnCoreExit::Hold,
            latency_method: config.latency_method,
            latency_test_url: config.latency_test_url.clone(),
            subscription_user_agent: config.subscription_user_agent.clone(),
            geoip_url: config.geoip_url.clone(),
            geosite_url: config.geosite_url.clone(),
            xray_binary: config.xray_binary.clone(),
            tun2socks_binary: config.tun2socks_binary.clone(),
            nft_binary: config.nft_binary.clone(),
            core: config.core.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsState {
    pub dirty: bool,
    pub valid: bool,
    pub applying: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsValidation {
    pub ports: Option<&'static str>,
    pub latency_url: Option<&'static str>,
    pub xray_binary: Option<&'static str>,
    /// Not a `&'static str` like its neighbours: this one names the offending
    /// value, because "must be a number or a range" without saying which field
    /// or what it read is no help among a dozen core rows.
    pub core: Option<String>,
}

impl SettingsValidation {
    pub fn is_valid(&self) -> bool {
        self.ports.is_none()
            && self.latency_url.is_none()
            && self.xray_binary.is_none()
            && self.core.is_none()
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
    hold_traffic: adw::SwitchRow,
    method: adw::ComboRow,
    test_url: adw::EntryRow,
    user_agent: adw::EntryRow,
    geoip_url: adw::EntryRow,
    geosite_url: adw::EntryRow,
    geo_preset: adw::ComboRow,
    ua_preset: adw::ComboRow,
    xray_binary: adw::EntryRow,
    tun2socks_binary: adw::EntryRow,
    nft_binary: adw::EntryRow,
    xray_effective: adw::ActionRow,
    /// "Install a core", with the command for this distribution. Hidden while
    /// a core resolves, because then there is nothing to install.
    install_hint: adw::ActionRow,
    /// What the row's copy button puts on the clipboard: the whole recipe
    /// where there is one, since a bare URL leaves the work undone.
    install_command: Rc<RefCell<String>>,
    /// What its Open button visits, when the answer is a download.
    install_link: Rc<RefCell<String>>,
    open_install: gtk::Button,
    /// What the core says about its geo data, filled from `RuntimeInfo` like
    /// `xray_effective` and never worked out here: the daemon is a different
    /// process, often a different user, and on NixOS the location comes from
    /// inside a wrapper this process cannot see.
    geo_status: adw::ActionRow,
    /// Offers the fix: a download, an adoption of files already present, or a
    /// command when neither can help.
    geo_action: adw::ActionRow,
    geo_button: gtk::Button,
    geo_cancel: gtk::Button,
    geo_progress: gtk::ProgressBar,
    /// The manual recipe, for a daemon too old to install anything itself.
    geo_command: Rc<RefCell<String>>,
    geo_copy: gtk::Button,
    core: CoreEditor,
    ports_error: gtk::Label,
    url_error: gtk::Label,
    xray_error: gtk::Label,
    core_error: gtk::Label,
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
            hold_traffic: self.hold_traffic.is_active(),
            latency_method: match self.method.selected() {
                0 => LatencyMethod::Icmp,
                1 => LatencyMethod::Tcp,
                2 => LatencyMethod::HttpHead,
                _ => LatencyMethod::HttpGet,
            },
            latency_test_url: self.test_url.text().to_string(),
            subscription_user_agent: self.user_agent.text().to_string(),
            geoip_url: self.geoip_url.text().to_string(),
            geosite_url: self.geosite_url.text().to_string(),
            // Trimmed here so trailing whitespace never counts as an edit and
            // never reaches the daemon's path resolution.
            xray_binary: self.xray_binary.text().trim().to_string(),
            tun2socks_binary: self.tun2socks_binary.text().trim().to_string(),
            nft_binary: self.nft_binary.text().trim().to_string(),
            core: self.core.values(),
        }
    }

    fn set_values(&self, values: &SettingsValues) {
        self.socks.set_value(f64::from(values.socks_port));
        self.http.set_value(f64::from(values.http_port));
        self.system_proxy.set_active(values.system_proxy);
        self.reconnect.set_active(values.reconnect);
        self.hold_traffic.set_active(values.hold_traffic);
        self.method.set_selected(match values.latency_method {
            LatencyMethod::Icmp => 0,
            LatencyMethod::Tcp => 1,
            LatencyMethod::HttpHead => 2,
            LatencyMethod::HttpGet => 3,
        });
        self.test_url.set_text(&values.latency_test_url);
        self.user_agent.set_text(&values.subscription_user_agent);
        self.geoip_url.set_text(&values.geoip_url);
        self.geosite_url.set_text(&values.geosite_url);
        self.xray_binary.set_text(&values.xray_binary);
        self.tun2socks_binary.set_text(&values.tun2socks_binary);
        self.nft_binary.set_text(&values.nft_binary);
        // Nothing sits below `config.toml`, so the editor inherits from the
        // built-in defaults — an untouched table.
        self.core.set_values(&CoreOptions::default(), &values.core);
    }
}

#[derive(Clone)]
pub struct SettingsView {
    pub root: adw::PreferencesPage,
    widgets: SettingsWidgets,
    model: Rc<RefCell<SettingsModel>>,
    updating_widgets: Rc<Cell<bool>>,
    /// Deliberately outside [`SettingsWidgets`]: everything in there edits the
    /// daemon's config behind Apply/Reset, while this one is a property of the
    /// window that takes effect the moment it is picked and has nothing to
    /// apply.
    appearance: adw::ComboRow,
    /// Set while the row is being written to programmatically, so restoring
    /// the saved choice does not read as the user making it.
    updating_appearance: Rc<Cell<bool>>,
    /// Whether the daemon answering this window knows how to install geo data,
    /// and which daemon it is. Both come from the window, which owns the
    /// client; together they decide whether a button can help at all.
    geo_download_supported: Rc<Cell<bool>>,
    daemon_source: Rc<Cell<DaemonSource>>,
}

impl SettingsView {
    /// Builds a settings editor. `on_apply` is called only after the user
    /// explicitly activates Apply; editing never persists configuration.
    pub fn new(config: &Config, on_apply: impl Fn(SettingsValues) + 'static) -> Self {
        let mut applied = SettingsValues::from(config);
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
            .subtitle(SYSTEM_PROXY_SUBTITLE)
            .active(applied.system_proxy)
            .build();
        let reconnect = adw::SwitchRow::builder()
            .title("Reconnect automatically")
            .subtitle("Reconnect only when Xray exits unexpectedly, never after Disconnect")
            .active(applied.reconnect)
            .build();
        let hold_traffic = adw::SwitchRow::builder()
            .title("Hold traffic if Xray exits")
            .subtitle(HOLD_TRAFFIC_SUBTITLE)
            // Two lines, and said so: the default is one, and the second line of
            // this subtitle is clipped by the row without it.
            .subtitle_lines(2)
            .active(applied.hold_traffic)
            .build();

        let methods =
            gtk::StringList::new(&["ICMP ping", "TCP handshake", "HTTP HEAD", "HTTP GET"]);
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
            .title("Latency check URL")
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

        // Left blank rather than pre-filled with the built-in address: a field
        // showing the default is a field the user has to recognise as the
        // default before they dare clear it, and clearing is how you go back.
        let geoip_url = adw::EntryRow::builder()
            .title("GeoIP list")
            .text(&applied.geoip_url)
            .build();
        let geosite_url = adw::EntryRow::builder()
            .title("Geosite list")
            .text(&applied.geosite_url)
            .build();
        let geo_labels: Vec<&str> = std::iter::once("Custom")
            .chain(GEO_PRESETS.iter().map(|preset| preset.label))
            .collect();
        let geo_preset = adw::ComboRow::builder()
            .title("Source")
            .subtitle("Fills both addresses below")
            .model(&gtk::StringList::new(&geo_labels))
            .selected(preset_for_geo(&applied.geoip_url, &applied.geosite_url))
            .build();
        let geo_source = adw::ExpanderRow::builder()
            .title("Where the geo data comes from")
            .subtitle(
                "The lists differ in what they cover; each is checked against the \
                       SHA-256 published beside it",
            )
            .build();
        geo_source.add_row(&geo_preset);
        geo_source.add_row(&geoip_url);
        geo_source.add_row(&geosite_url);

        let xray_binary = adw::EntryRow::builder()
            .title("Xray binary override")
            .text(&applied.xray_binary)
            .build();
        let tun2socks_binary = adw::EntryRow::builder()
            .title("tun2socks binary")
            .text(&applied.tun2socks_binary)
            .build();
        let nft_binary = adw::EntryRow::builder()
            .title("nft binary")
            .text(&applied.nft_binary)
            .build();
        // Filled from the daemon over D-Bus, never computed here: the daemon
        // is a separate process, usually a different user, with its own $PATH.
        let xray_effective = adw::ActionRow::builder()
            .title("In use by the daemon")
            .subtitle("Checking…")
            .subtitle_selectable(true)
            .build();
        xray_effective.add_css_class("property");

        let core = CoreEditor::new(CoreLevel::Machine, &CoreOptions::default(), &applied.core);
        // A hand-written `[core] log_level = "warning"` says the same thing as
        // no `[core]` at all, and the editor stores the shorter of the two. The
        // baseline has to agree, or the page would open already dirty over a
        // difference nobody made and Apply could not remove.
        applied.core = core.values();

        let ports_error = validation_label();
        let url_error = validation_label();
        let xray_error = validation_label();
        let core_error = validation_label();
        core.group.add(&core_error);

        let proxy_group = adw::PreferencesGroup::builder()
            .title("Local proxy")
            .build();
        proxy_group.add(&socks);
        proxy_group.add(&http);
        proxy_group.add(&ports_error);
        proxy_group.add(&system_proxy);
        proxy_group.add(&reconnect);
        proxy_group.add(&hold_traffic);
        let xray_group = adw::PreferencesGroup::builder()
            .title("Xray core")
            .description(
                "Leave empty to install and use the pinned managed core. An override must report \
                 the same version; a system-wide service cannot read paths under /home.",
            )
            .build();
        // Shown only when the verified managed download failed, so an offline
        // machine still has a copyable, checksum-verifying fallback.
        let install_hint = adw::ActionRow::builder()
            .title("Install the pinned core manually")
            .subtitle_selectable(true)
            .visible(false)
            .build();
        let install_command = Rc::new(RefCell::new(String::new()));
        let copy_install = icon_button("edit-copy-symbolic", "Copy");
        copy_install.set_tooltip_text(Some("Copy"));
        copy_install.connect_clicked({
            let install_command = install_command.clone();
            move |button| {
                button
                    .clipboard()
                    .set_text(install_command.borrow().as_str());
            }
        });
        // Shown only where there is a download to open. A package-manager
        // answer has no page worth visiting, and a button that opens nothing
        // is a button that lies.
        let install_link = Rc::new(RefCell::new(String::new()));
        // `web-browser-symbolic`, not `external-link-symbolic`: Adwaita ships
        // no icon under the latter name, and a missing icon draws an empty
        // square -- the same way the Filter pill did before the funnel started
        // travelling with the application.
        let open_install = icon_button("web-browser-symbolic", "Open");
        open_install.set_tooltip_text(Some("Open the download page"));
        open_install.set_visible(false);
        open_install.connect_clicked({
            let install_link = install_link.clone();
            move |_| {
                let uri = install_link.borrow().clone();
                if uri.is_empty() {
                    return;
                }
                // `gtk::UriLauncher` would read better but arrives with the
                // `v4_10` feature, and the workspace pins `v4_8`; raising it
                // for one button is not this change's business.
                if let Err(error) = gtk::gio::AppInfo::launch_default_for_uri(
                    &uri,
                    gtk::gio::AppLaunchContext::NONE,
                ) {
                    log::debug!("could not open the download page: {error}");
                }
            }
        });
        install_hint.add_suffix(&open_install);
        install_hint.add_suffix(&copy_install);

        // Between what the daemon resolved and how to install a core: the
        // order reads as what you asked for, what is in use, what it is
        // missing, how to get it.
        let geo_status = adw::ActionRow::builder()
            .title("Geo data")
            .subtitle_selectable(true)
            .visible(false)
            .build();
        geo_status.add_css_class("property");

        let geo_action = adw::ActionRow::builder()
            .title("Install the geo data")
            .subtitle_selectable(true)
            .visible(false)
            .build();
        let geo_button = gtk::Button::with_label("Download");
        geo_button.add_css_class("suggested-action");
        geo_button.set_valign(gtk::Align::Center);
        let geo_cancel = gtk::Button::with_label("Cancel");
        geo_cancel.add_css_class("destructive-action");
        geo_cancel.set_valign(gtk::Align::Center);
        geo_cancel.set_visible(false);
        let geo_command = Rc::new(RefCell::new(String::new()));
        let geo_copy = icon_button("edit-copy-symbolic", "Copy");
        geo_copy.set_tooltip_text(Some("Copy"));
        geo_copy.set_visible(false);
        geo_copy.connect_clicked({
            let geo_command = geo_command.clone();
            move |button| {
                button.clipboard().set_text(geo_command.borrow().as_str());
            }
        });
        geo_action.add_suffix(&geo_copy);
        geo_action.add_suffix(&geo_cancel);
        geo_action.add_suffix(&geo_button);

        // The first progress bar in the application. Twenty-three megabytes is
        // long enough on a slow link that a spinner alone reads as a hang.
        let geo_progress = gtk::ProgressBar::builder()
            .visible(false)
            .show_text(false)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(8)
            .build();

        xray_group.add(&xray_binary);
        xray_group.add(&xray_error);
        xray_group.add(&xray_effective);
        xray_group.add(&geo_status);
        xray_group.add(&geo_action);
        xray_group.add(&geo_progress);
        xray_group.add(&install_hint);
        xray_group.add(&geo_source);
        let latency_group = adw::PreferencesGroup::builder()
            .title("Latency")
            .description("HTTP checks use the active local SOCKS proxy")
            .build();
        latency_group.add(&method);
        latency_group.add(&test_url);
        latency_group.add(&url_error);

        let advanced = adw::ExpanderRow::builder()
            .title("Advanced")
            .subtitle("Core paths and subscription compatibility settings")
            .build();
        advanced.add_row(&tun2socks_binary);
        advanced.add_row(&nft_binary);
        advanced.add_row(&ua_preset);
        advanced.add_row(&user_agent);
        let advanced_group = adw::PreferencesGroup::new();
        advanced_group.add(&advanced);

        let appearance = adw::ComboRow::builder()
            .title("Appearance")
            .subtitle("Follow the desktop, or pin this window to one scheme")
            .model(&gtk::StringList::new(&[
                "Follow the system",
                "Light",
                "Dark",
            ]))
            .selected(ColorScheme::default().position())
            .build();
        let appearance_group = adw::PreferencesGroup::new();
        appearance_group.add(&appearance);

        let root = adw::PreferencesPage::new();
        // First, and alone in its group: it is the one row here that takes
        // effect as it is clicked, and grouping it with the daemon's settings
        // would promise it the same Apply.
        root.add(&appearance_group);
        root.add(&proxy_group);
        root.add(&xray_group);
        root.add(&latency_group);
        // Below Latency, above the paths: these change what the tunnel does,
        // which puts them above the rows that only say where binaries live.
        root.add(&core.group);
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
            hold_traffic,
            method,
            test_url,
            user_agent,
            geoip_url,
            geosite_url,
            geo_preset,
            ua_preset,
            xray_binary,
            tun2socks_binary,
            nft_binary,
            xray_effective,
            install_hint,
            geo_status,
            geo_action,
            geo_button,
            geo_cancel,
            geo_progress,
            geo_command,
            geo_copy,
            install_command,
            install_link,
            open_install,
            core,
            ports_error,
            url_error,
            xray_error,
            core_error,
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
            appearance,
            updating_appearance: Rc::new(Cell::new(false)),
            // Assumed absent until the window says otherwise, so a daemon that
            // turns out to be too old is never briefly offered a button.
            geo_download_supported: Rc::new(Cell::new(false)),
            daemon_source: Rc::new(Cell::new(DaemonSource::Session)),
        }
    }

    /// Show the saved choice without reporting it back as a new one.
    pub fn set_color_scheme(&self, scheme: ColorScheme) {
        self.updating_appearance.set(true);
        self.appearance.set_selected(scheme.position());
        self.updating_appearance.set(false);
    }

    /// Called as the user picks, not on Apply: there is nothing to apply, and
    /// a theme that waited for a button would look broken.
    pub fn connect_color_scheme_changed(&self, on_change: impl Fn(ColorScheme) + 'static) {
        let updating = self.updating_appearance.clone();
        self.appearance.connect_selected_notify(move |row| {
            if updating.get() {
                return;
            }
            on_change(ColorScheme::from_position(row.selected()));
        });
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

    /// Reports whether the desktop proxy could actually be installed. The
    /// switch used to stay on and look applied on a session where `gsettings`
    /// is not there to apply it, which made this the one setting that could be
    /// on and doing nothing with no way to tell.
    pub fn set_system_proxy_failure(&self, reason: Option<&str>) {
        let row = &self.widgets.system_proxy;
        match reason {
            Some(reason) => {
                row.set_subtitle(reason);
                row.add_css_class("error");
            }
            None => {
                row.set_subtitle(SYSTEM_PROXY_SUBTITLE);
                row.remove_css_class("error");
            }
        }
    }

    /// Completes an apply with what the daemon **stored**, which is not always
    /// what was sent: a system daemon reverts binary paths, and a service unit
    /// pins ports. Marking the submitted draft applied instead left the page
    /// clean while holding values that exist nowhere.
    ///
    /// A field the daemon refused is snapped back in the widget too, so the row
    /// shows the truth rather than an edit that will never take. Fields it
    /// accepted are left alone, which keeps the promise of [`Self::mark_applied`]:
    /// an edit made *while* the apply was in flight stays dirty.
    pub fn adopt_applied(&self, submitted: &SettingsValues, effective: SettingsValues) {
        let draft = {
            let mut model = self.model.borrow_mut();
            let mut draft = model.draft.clone();
            refuse(submitted, &effective, &mut draft);
            model.draft = draft.clone();
            model.mark_applied(effective);
            draft
        };
        self.updating_widgets.set(true);
        self.widgets.set_values(&draft);
        sync_preset(&self.widgets);
        self.updating_widgets.set(false);
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
    /// Tell the page which daemon it is talking to, and whether that daemon can
    /// install geo data at all. Both are the window's to know: it owns the
    /// client. Called before the first `set_runtime_info`.
    pub fn set_daemon_capabilities(&self, source: DaemonSource, can_download: bool) {
        self.daemon_source.set(source);
        self.geo_download_supported.set(can_download);
    }

    /// The manual recipe, for a daemon that cannot install anything itself.
    ///
    /// `/usr/local/share/xray` rather than oxidom's own directory on purpose:
    /// it is in the core's built-in search path, so it needs no environment
    /// variable and works for a daemon of any age, including the old one that
    /// prompted the advice.
    ///
    /// The addresses are the configured ones, not the built-in pair: a recipe
    /// that fetched a different list from the one the settings name would
    /// install the wrong lists by hand and give no sign of it.
    fn manual_install_command(widgets: &SettingsWidgets) -> String {
        use oxidom_core::xray::assets::GeoAsset;
        format!(
            "curl -Lo geoip.dat   {}\ncurl -Lo geosite.dat {}\n\
             sudo install -Dm644 geoip.dat   /usr/local/share/xray/geoip.dat\n\
             sudo install -Dm644 geosite.dat /usr/local/share/xray/geosite.dat",
            assets::resolve_url(GeoAsset::GeoIp, &widgets.geoip_url.text()),
            assets::resolve_url(GeoAsset::GeoSite, &widgets.geosite_url.text()),
        )
    }

    /// Paint the geo rows from a decision the reducer already made.
    ///
    /// Nothing is decided here — every branch is a `GeoOffer` variant, which is
    /// what makes the awkward cases (a system daemon too old to help, a
    /// directory it cannot write) testable without a display.
    fn apply_geo_offer(&self, offer: &GeoOffer) {
        let widgets = &self.widgets;
        let show = |status: bool, action: bool, progress: bool| {
            widgets.geo_status.set_visible(status);
            widgets.geo_action.set_visible(action);
            widgets.geo_progress.set_visible(progress);
        };
        match offer {
            GeoOffer::Silent => show(false, false, false),
            GeoOffer::Working => {
                widgets
                    .geo_status
                    .set_subtitle("In use — the core loads geoip.dat and geosite.dat");
                widgets.geo_status.remove_css_class("error");
                show(true, false, false);
            }
            GeoOffer::Download => {
                widgets
                    .geo_status
                    .set_subtitle("Missing — the core refuses every connection without it");
                widgets.geo_status.add_css_class("error");
                widgets
                    .geo_action
                    .set_subtitle("Look for a copy on this machine, or download it from GitHub");
                widgets.geo_button.set_label("Install…");
                widgets.geo_button.set_visible(true);
                widgets.geo_button.set_sensitive(true);
                widgets.geo_cancel.set_visible(false);
                widgets.geo_copy.set_visible(false);
                show(true, true, false);
            }
            GeoOffer::Running { file, done, total } => {
                widgets.geo_status.set_subtitle("Installing…");
                widgets.geo_status.remove_css_class("error");
                widgets
                    .geo_action
                    .set_subtitle(&reduce::geo_progress_text(file, *done, *total));
                widgets.geo_button.set_visible(false);
                widgets.geo_cancel.set_visible(true);
                widgets.geo_copy.set_visible(false);
                if *total > 0 {
                    widgets
                        .geo_progress
                        .set_fraction(*done as f64 / *total as f64);
                } else {
                    // No Content-Length, so there is nothing to divide by. A
                    // pulse still distinguishes working from wedged.
                    widgets.geo_progress.pulse();
                }
                show(true, true, true);
            }
            GeoOffer::Unwritable { dir } => {
                widgets
                    .geo_status
                    .set_subtitle("Missing — the core refuses every connection without it");
                widgets.geo_status.add_css_class("error");
                widgets.geo_action.set_subtitle(&format!(
                    "The daemon cannot write {dir}, so it cannot install this for you"
                ));
                widgets.geo_button.set_visible(false);
                widgets.geo_cancel.set_visible(false);
                *widgets.geo_command.borrow_mut() = Self::manual_install_command(widgets);
                widgets.geo_copy.set_visible(true);
                show(true, true, false);
            }
            GeoOffer::CommandOnly { session_fallback } => {
                widgets
                    .geo_status
                    .set_subtitle("Missing — the core refuses every connection without it");
                widgets.geo_status.add_css_class("error");
                widgets.geo_action.set_subtitle(if *session_fallback {
                    "This daemon is older than the app and cannot install it. Copy the \
                     commands, or update oxidom."
                } else {
                    // The system service runs as `oxidom` with ProtectHome, so
                    // nothing this GUI could download would ever be readable by
                    // it. Only a command helps.
                    "The system service is older than the app and cannot install it. \
                     Copy the commands, or update oxidom."
                });
                widgets.geo_button.set_visible(false);
                widgets.geo_cancel.set_visible(false);
                *widgets.geo_command.borrow_mut() = Self::manual_install_command(widgets);
                widgets.geo_copy.set_visible(true);
                show(true, true, false);
            }
        }
    }

    /// The button that starts an install, for the window to connect to.
    pub fn geo_install_button(&self) -> gtk::Button {
        self.widgets.geo_button.clone()
    }

    /// The button that stops one.
    pub fn geo_cancel_button(&self) -> gtk::Button {
        self.widgets.geo_cancel.clone()
    }

    pub fn set_runtime_info(&self, info: Option<&RuntimeInfo>) {
        let widgets = &self.widgets;
        let Some(info) = info else {
            widgets
                .xray_effective
                .set_subtitle("Unavailable — this daemon is older than the app");
            widgets.xray_effective.remove_css_class("error");
            self.apply_geo_offer(&GeoOffer::Silent);
            return;
        };

        match (&info.xray_path, &info.xray_error) {
            (Some(path), _) => {
                let source = info
                    .xray_source
                    .map(|source| {
                        format!(
                            " (from {})",
                            source.label(&oxidom_core::xray::resolve::XRAY)
                        )
                    })
                    .unwrap_or_default();
                widgets
                    .xray_effective
                    .set_subtitle(&format!("{path}{source}"));
                widgets.xray_effective.remove_css_class("error");
                widgets.install_hint.set_visible(false);
            }
            (None, Some(error)) => {
                widgets.xray_effective.set_subtitle(error);
                widgets.xray_effective.add_css_class("error");
                // A distribution that packages a core gets its command; the
                // rest get the release built for *this* machine, because the
                // releases page carries eighty assets and choosing between
                // them is where people came unstuck. Inventing an `apt install
                // xray` that fails would be trusted over the documentation, so
                // no distribution is given a command it does not have.
                let install = oxidom_core::distro::xray_install_here();
                widgets.install_hint.set_subtitle(&install.summary());
                *widgets.install_command.borrow_mut() = install.clipboard();
                match install.link() {
                    Some(url) => {
                        *widgets.install_link.borrow_mut() = url.to_string();
                        widgets.open_install.set_visible(true);
                    }
                    None => {
                        widgets.install_link.borrow_mut().clear();
                        widgets.open_install.set_visible(false);
                    }
                }
                widgets.install_hint.set_visible(true);
            }
            // Nothing resolved and nothing reported: an older daemon that
            // answers neither. Offering an install command would be a guess.
            (None, None) => {
                widgets.xray_effective.set_subtitle("Unknown");
                widgets.xray_effective.remove_css_class("error");
                widgets.install_hint.set_visible(false);
            }
        }

        self.apply_geo_offer(&reduce::geo_offer(
            Some(info),
            self.geo_download_supported.get(),
            self.daemon_source.get(),
        ));

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

        // A system daemon reverts these three to its own values, so leaving
        // them editable meant typing into a field whose text the next Apply
        // would silently drop — and the page would still call itself applied.
        const PATH_LOCKED: &str =
            "Set by the system service — a privileged daemon does not run a path chosen here";
        for row in [
            &widgets.xray_binary,
            &widgets.tun2socks_binary,
            &widgets.nft_binary,
        ] {
            row.set_sensitive(!info.binary_paths_locked);
        }
        widgets
            .xray_binary
            .set_tooltip_text(info.binary_paths_locked.then_some(PATH_LOCKED));

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

/// Copies into `draft` every field the daemon changed on the way in — that is,
/// every field where what came back differs from what was sent. Compared field
/// by field rather than by the labels in `ApplySettingsResult`, so a refusal the
/// daemon forgets to name still shows up, and an unrelated edit made while the
/// apply was in flight survives.
fn refuse(submitted: &SettingsValues, effective: &SettingsValues, draft: &mut SettingsValues) {
    macro_rules! adopt {
        ($($field:ident),+ $(,)?) => {$(
            if submitted.$field != effective.$field {
                draft.$field = effective.$field.clone();
            }
        )+};
    }
    adopt!(
        socks_port,
        http_port,
        system_proxy,
        reconnect,
        latency_method,
        latency_test_url,
        subscription_user_agent,
        geoip_url,
        geosite_url,
        xray_binary,
        tun2socks_binary,
        nft_binary,
        core,
    );
}

/// Which named source both addresses currently describe, or `Custom`.
///
/// Empty counts as the built-in one, because that is what empty means
/// everywhere else. A pair that matches no preset — or two halves from
/// different presets, which is legitimate — reads as Custom rather than as
/// whichever half happened to match.
fn preset_for_geo(geoip: &str, geosite: &str) -> u32 {
    let geoip = assets::resolve_url(assets::GeoAsset::GeoIp, geoip);
    let geosite = assets::resolve_url(assets::GeoAsset::GeoSite, geosite);
    GEO_PRESETS
        .iter()
        .position(|preset| preset.geoip == geoip && preset.geosite == geosite)
        .map(|index| index as u32 + 1)
        .unwrap_or(0)
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
    let selected = preset_for_geo(&widgets.geoip_url.text(), &widgets.geosite_url.text());
    if widgets.geo_preset.selected() != selected {
        widgets.geo_preset.set_selected(selected);
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
    widgets.core.connect_changed({
        let update = update.clone();
        move || update()
    });
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
    widgets.hold_traffic.connect_active_notify({
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
    widgets.tun2socks_binary.connect_changed({
        let update = update.clone();
        move |_| update()
    });
    widgets.nft_binary.connect_changed({
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
    set_validation_message(&widgets.core_error, validation.core.as_deref());
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
        0 => "Straight to the server, outside the tunnel",
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
    // The same check the daemon and `oxidom profile edit` run, borrowed rather
    // than restated: the core takes a reversed range or an out-of-band
    // concurrency without a word and then quietly does nothing with it, so
    // there is no later moment at which a wrong value announces itself.
    let core = values
        .core
        .validate("core")
        .err()
        .map(|error| error.to_string().trim_start_matches("[core] ").to_string());
    SettingsValidation {
        ports,
        latency_url,
        xray_binary,
        core,
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
            hold_traffic: true,
            latency_method: LatencyMethod::HttpGet,
            latency_test_url: "https://www.gstatic.com/generate_204".into(),
            subscription_user_agent: "oxidom/test".into(),
            geoip_url: String::new(),
            geosite_url: String::new(),
            xray_binary: String::new(),
            tun2socks_binary: String::new(),
            nft_binary: String::new(),
            core: CoreOptions::default(),
        }
    }

    /// Empty is the built-in source, so it must read as that preset rather
    /// than as Custom — otherwise the row would tell a user who has changed
    /// nothing that they have.
    #[test]
    fn an_unset_geo_source_reads_as_the_built_in_preset() {
        assert_eq!(preset_for_geo("", ""), 1);
        let second = &GEO_PRESETS[1];
        assert_eq!(preset_for_geo(second.geoip, second.geosite), 2);
        assert_eq!(
            preset_for_geo(second.geoip, ""),
            0,
            "one half from one source and one from another is nobody's preset"
        );
        assert_eq!(preset_for_geo("https://example.invalid/geoip.dat", ""), 0);
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

    /// The page used to mark the *request* applied, so a system daemon that
    /// reverted a binary path left the row clean holding a path that existed
    /// nowhere. It now adopts what came back — but only for the fields the
    /// daemon actually changed, so an edit made while the apply was in flight
    /// is not thrown away.
    #[test]
    fn a_refused_field_snaps_back_and_an_unrelated_edit_survives() {
        let submitted = SettingsValues {
            xray_binary: "/home/me/xray".into(),
            ..values()
        };
        let effective = SettingsValues {
            // The daemon kept its own core and its unit-pinned port.
            xray_binary: "/nix/store/xray/bin/xray".into(),
            socks_port: 20172,
            ..submitted.clone()
        };
        // Typed after Apply was pressed, before the answer arrived.
        let mut draft = SettingsValues {
            latency_test_url: "https://example.test/204".into(),
            ..submitted.clone()
        };

        refuse(&submitted, &effective, &mut draft);

        assert_eq!(draft.xray_binary, "/nix/store/xray/bin/xray");
        assert_eq!(draft.socks_port, 20172);
        assert_eq!(draft.latency_test_url, "https://example.test/204");
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

    /// The core takes a reversed range without a word and then fragments
    /// nothing, so Apply is the last moment at which a wrong value can be
    /// pointed at — nothing downstream will ever complain about it.
    #[test]
    fn a_core_value_the_xray_core_would_swallow_blocks_apply() {
        let mut draft = values();
        draft.core.fragment.length = Some("200-100".into());

        let validation = validate(&draft);
        assert!(!validation.is_valid());
        let message = validation.core.expect("the range runs backwards");
        assert!(message.contains("runs backwards"), "{message}");
        // The section prefix belongs in a file, not on a row that is already
        // inside the group it names.
        assert!(!message.starts_with("[core]"), "{message}");

        draft.core.fragment.length = Some("100-200".into());
        assert!(validate(&draft).is_valid());
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
