//! The `[core]` editor, built once and used at both levels of the merge.
//!
//! `config.toml` and a profile's `[core]` hold the same table, so they get the
//! same rows. What differs is what an unset field means, and that difference is
//! the whole design of this module:
//!
//! - In `config.toml` the only level below is the built-in default, so "unset"
//!   and "the built-in value" describe the same generated config. The editor
//!   stores the shorter of the two — otherwise one Apply on an unrelated
//!   setting would write a full `[core]` table into a file whose owner never
//!   asked for one.
//! - In a profile, "unset" means the machine's value, which is not the built-in
//!   one. So each section is overridden whole or inherited whole — mirroring the
//!   file, where the profile either has a `[core.mux]` table or it does not.
//!
//! Noise packets have no editor here. They are a list of hand-tuned byte
//! patterns with no useful default, and a blind round trip through a GUI that
//! could not show them would be worse than saying so: the row reports how many
//! there are and the value is carried back untouched, exactly as the pool
//! membership is in `profile_dialog.rs`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use oxidom_core::core_options::{
    CoreOptions, DestOverride, DnsOptions, DomainStrategy, FragmentOptions, LogLevel, MuxOptions,
    Noise, QueryStrategy, ResolvedCore, ResolvedMux, SniffingOptions, XudpMode,
};

/// The one listener an editor reports edits to, once someone asks for it.
type ChangeListener = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// Which of the two levels this editor writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreLevel {
    /// `config.toml`.
    Machine,
    /// A profile's `[core]`, layered over the machine's.
    Profile,
}

impl CoreLevel {
    /// A profile's drop-downs carry one extra entry at the top, "Inherited",
    /// which the machine level has no use for: the value it would inherit is
    /// the built-in default, already in the list under its own name.
    fn combo_offset(self) -> u32 {
        match self {
            CoreLevel::Machine => 0,
            CoreLevel::Profile => 1,
        }
    }

    fn is_profile(self) -> bool {
        self == CoreLevel::Profile
    }
}

const LOG_LEVELS: &[LogLevel] = &[
    LogLevel::Debug,
    LogLevel::Info,
    LogLevel::Warning,
    LogLevel::Error,
    LogLevel::Silent,
];
const LOG_LEVEL_LABELS: &[&str] = &["Debug", "Info", "Warning", "Error", "Off"];

const DOMAIN_STRATEGIES: &[DomainStrategy] = &[
    DomainStrategy::AsIs,
    DomainStrategy::IpIfNonMatch,
    DomainStrategy::IpOnDemand,
];
/// The core's own spelling, not a friendlier one: this is what `oxidom core
/// show`, the documentation and the generated JSON all say, and a support
/// conversation goes badly when the screen and the file disagree.
const DOMAIN_STRATEGY_LABELS: &[&str] = &["AsIs", "IPIfNonMatch", "IPOnDemand"];

const XUDP_MODES: &[XudpMode] = &[XudpMode::Reject, XudpMode::Allow, XudpMode::Skip];
const XUDP_LABELS: &[&str] = &["Reject", "Allow", "Skip"];

const QUERY_STRATEGIES: &[QueryStrategy] = &[
    QueryStrategy::UseIp,
    QueryStrategy::UseIpv4,
    QueryStrategy::UseIpv6,
];
const QUERY_LABELS: &[&str] = &["UseIP", "UseIPv4", "UseIPv6"];

const DEST_OVERRIDES: &[DestOverride] =
    &[DestOverride::Http, DestOverride::Tls, DestOverride::Quic];
const DEST_OVERRIDE_LABELS: &[&str] = &["HTTP", "TLS", "QUIC"];

const MUX_POOL_WARNING: &str = "One connection carries everything, so a group stops spreading \
    activity across its exit addresses.";
const SNIFFING_HINT: &str = "Off means domain rules stop matching: the core only ever sees the \
    address the application already resolved.";
const NOISE_HINT: &str = "Edited in the configuration file. Saved from here untouched.";
const FRAGMENT_HINT: &str = "Only the first packets of a connection are split, which is what \
    hides a TLS hello from an inspecting middlebox.";
const DNS_PROFILE_LIMIT: &str = " — a profile can point somewhere else, but cannot take it away";
const FRAGMENT_DEFAULTS: &str = "Empty fields fall back to tlshello, 100-200 and 10-20.";
const DNS_SCOPE: &str = "Used by the core for the names it routes. The desktop's own resolver is \
    left alone.";
const OWNED_BY_PROFILE: &str = "Set by this profile";

/// What a row says while it is inheriting. Worked out once from the level
/// below, which is the only thing on the page the reader cannot see for
/// themselves — the point being that a section can be read without being
/// switched on first, and switching one on to look is exactly how a profile
/// ends up pinning a value nobody meant to pin.
#[derive(Clone, Default)]
struct InheritedText {
    log_level: String,
    domain_strategy: String,
    query_strategy: String,
    sniffing: String,
    mux: String,
    fragment: String,
    dns: String,
}

/// The `[core]` rows, ready to be added to a page or a dialog.
#[derive(Clone)]
pub struct CoreEditor {
    pub group: adw::PreferencesGroup,
    level: CoreLevel,
    widgets: CoreWidgets,
    /// Not shown, not editable, and handed straight back on save.
    noises: Rc<RefCell<Option<Vec<Noise>>>>,
    inherited_text: Rc<InheritedText>,
    updating: Rc<Cell<bool>>,
    /// Filled in by [`CoreEditor::connect_changed`]. The signals are wired at
    /// construction because they also keep the explanatory subtitles honest,
    /// which has to happen whether or not anyone is listening for edits.
    changed: ChangeListener,
}

#[derive(Clone)]
struct CoreWidgets {
    log_level: adw::ComboRow,
    domain_strategy: adw::ComboRow,
    sniffing: adw::ExpanderRow,
    sniffing_enabled: adw::SwitchRow,
    dest_override: Vec<gtk::CheckButton>,
    route_only: adw::SwitchRow,
    mux: adw::ExpanderRow,
    mux_enabled: adw::SwitchRow,
    concurrency: adw::SpinRow,
    xudp_concurrency: adw::SpinRow,
    xudp_mode: adw::ComboRow,
    fragment: adw::ExpanderRow,
    fragment_enabled: adw::SwitchRow,
    packets: adw::EntryRow,
    length: adw::EntryRow,
    interval: adw::EntryRow,
    noise_row: adw::ActionRow,
    dns: adw::ExpanderRow,
    dns_server: adw::EntryRow,
    dns_direct: adw::EntryRow,
    query_strategy: adw::ComboRow,
}

impl CoreEditor {
    /// `inherited` is the level below — an untouched [`CoreOptions`] for the
    /// machine editor, the machine's own settings for a profile's.
    pub fn new(level: CoreLevel, inherited: &CoreOptions, values: &CoreOptions) -> Self {
        let resolved = CoreOptions::resolve(inherited, &CoreOptions::default());
        let profile = level.is_profile();

        // Not "Xray core": on the Settings page that name already belongs to
        // the group that says *which binary* runs. These rows are about what it
        // does once it is running.
        let group = adw::PreferencesGroup::builder()
            .title("Core behaviour")
            .description(if profile {
                "Settings this profile keeps to itself. A section left off follows the machine."
            } else {
                "Applies to every profile that does not override it, and to latency probes."
            })
            .build();

        // The drop-down entry says only "Inherited"; the value it stands for
        // goes in the subtitle, where it has room to be read. A combo button is
        // the narrowest thing on the row and "Inherited (IPIfNonMatch)" arrives
        // there as "Inherited (IPIfNonM…".
        let inherit_entry = profile.then_some("Inherited");
        let log_level = combo("Log level", LOG_LEVEL_LABELS, inherit_entry);
        group.add(&log_level);
        let domain_strategy = combo("Domain resolution", DOMAIN_STRATEGY_LABELS, inherit_entry);
        group.add(&domain_strategy);

        let sniffing = section("Sniffing", profile);
        let sniffing_enabled = adw::SwitchRow::builder()
            .title("Read the destination from traffic")
            .subtitle(SNIFFING_HINT)
            .build();
        sniffing.add_row(&sniffing_enabled);
        let dest_row = adw::ActionRow::builder()
            .title("Protocols")
            .subtitle("Which handshakes the core reads a hostname out of")
            .activatable(false)
            .build();
        let dest_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        dest_box.set_valign(gtk::Align::Center);
        let dest_override: Vec<gtk::CheckButton> = DEST_OVERRIDE_LABELS
            .iter()
            .map(|label| {
                let button = gtk::CheckButton::with_label(label);
                dest_box.append(&button);
                button
            })
            .collect();
        dest_row.add_suffix(&dest_box);
        sniffing.add_row(&dest_row);
        let route_only = adw::SwitchRow::builder()
            .title("Route only")
            .subtitle("Use the sniffed name to pick a route, but still connect to the address the application asked for")
            .build();
        sniffing.add_row(&route_only);
        group.add(&sniffing);

        let mux = section("Multiplexing", profile);
        let mux_enabled = adw::SwitchRow::builder()
            .title("Multiplex connections")
            .subtitle(MUX_POOL_WARNING)
            .build();
        mux.add_row(&mux_enabled);
        let concurrency = adw::SpinRow::with_range(0.0, 1024.0, 1.0);
        concurrency.set_title("Concurrency");
        concurrency.set_subtitle("Streams per connection. 0 leaves it to the core.");
        mux.add_row(&concurrency);
        let xudp_concurrency = adw::SpinRow::with_range(0.0, 1024.0, 1.0);
        xudp_concurrency.set_title("XUDP concurrency");
        xudp_concurrency
            .set_subtitle("Same, for UDP carried over the tunnel. 0 leaves it to the core.");
        mux.add_row(&xudp_concurrency);
        // Always carries the extra entry, at both levels: unlike the others
        // this key has no oxidom default at all — leaving it out is a real
        // third choice, "let the core decide".
        let xudp_mode = combo(
            "UDP over port 443",
            XUDP_LABELS,
            Some(inherit_entry.unwrap_or("Core default")),
        );
        xudp_mode.set_subtitle(
            "QUIC and HTTP/3 ride on it; multiplexing them usually costs more than it saves",
        );
        mux.add_row(&xudp_mode);
        group.add(&mux);

        let fragment = section("Fragmentation", profile);
        let fragment_enabled = adw::SwitchRow::builder()
            .title("Fragment outgoing packets")
            .subtitle(FRAGMENT_HINT)
            .build();
        fragment.add_row(&fragment_enabled);
        let packets = entry("Packets", "tlshello, a count, or a range such as 1-3");
        fragment.add_row(&packets);
        let length = entry("Length (bytes)", "A number or a range, e.g. 100-200");
        fragment.add_row(&length);
        let interval = entry("Interval (ms)", "A number or a range, e.g. 10-20");
        fragment.add_row(&interval);
        let noise_row = adw::ActionRow::builder()
            .title("Noise packets")
            .subtitle(NOISE_HINT)
            .activatable(false)
            .build();
        fragment.add_row(&noise_row);
        group.add(&fragment);

        let dns = section("DNS", profile);
        let dns_server = entry(
            "Resolver",
            "An address or a DoH URL. Empty means the core resolves as it always did.",
        );
        dns.add_row(&dns_server);
        let dns_direct = entry(
            "Local resolver",
            "Asked first, and only about names on the local network",
        );
        dns.add_row(&dns_direct);
        let query_strategy = combo("Address family", QUERY_LABELS, inherit_entry);
        dns.add_row(&query_strategy);
        group.add(&dns);

        let widgets = CoreWidgets {
            log_level,
            domain_strategy,
            sniffing,
            sniffing_enabled,
            dest_override,
            route_only,
            mux,
            mux_enabled,
            concurrency,
            xudp_concurrency,
            xudp_mode,
            fragment,
            fragment_enabled,
            packets,
            length,
            interval,
            noise_row,
            dns,
            dns_server,
            dns_direct,
            query_strategy,
        };

        let editor = Self {
            group,
            level,
            widgets,
            noises: Rc::new(RefCell::new(None)),
            inherited_text: Rc::new(inherited_text(level, &resolved)),
            updating: Rc::new(Cell::new(false)),
            changed: Rc::new(RefCell::new(None)),
        };
        editor.set_values(inherited, values);
        editor.connect_signals();
        editor
    }

    /// Reports an edit made by the user. Changes this module makes itself —
    /// [`CoreEditor::set_values`] — deliberately do not reach `on_change`, so a
    /// reset cannot come back looking like a fresh edit.
    pub fn connect_changed(&self, on_change: impl Fn() + 'static) {
        *self.changed.borrow_mut() = Some(Rc::new(on_change));
    }

    /// Everything the user typed, expressed the way the file stores it.
    pub fn values(&self) -> CoreOptions {
        let widgets = &self.widgets;
        let options = CoreOptions {
            log_level: enum_value(&widgets.log_level, self.level, LOG_LEVELS),
            domain_strategy: enum_value(&widgets.domain_strategy, self.level, DOMAIN_STRATEGIES),
            sniffing: if widgets.sniffing.enables_expansion() {
                SniffingOptions {
                    enabled: Some(widgets.sniffing_enabled.is_active()),
                    dest_override: Some(
                        widgets
                            .dest_override
                            .iter()
                            .enumerate()
                            .filter(|(_, button)| button.is_active())
                            .map(|(index, _)| DEST_OVERRIDES[index])
                            .collect(),
                    ),
                    route_only: Some(widgets.route_only.is_active()),
                }
            } else {
                SniffingOptions::default()
            },
            mux: if widgets.mux.enables_expansion() {
                MuxOptions {
                    enabled: Some(widgets.mux_enabled.is_active()),
                    concurrency: spin_value(&widgets.concurrency),
                    xudp_concurrency: spin_value(&widgets.xudp_concurrency),
                    // This drop-down always carries an "unset" entry, so it is
                    // read as a plain optional rather than through the level.
                    xudp_proxy_udp_443: optional_enum(&widgets.xudp_mode, XUDP_MODES),
                }
            } else {
                MuxOptions::default()
            },
            fragment: if widgets.fragment.enables_expansion() {
                FragmentOptions {
                    enabled: Some(widgets.fragment_enabled.is_active()),
                    packets: entry_value(&widgets.packets),
                    length: entry_value(&widgets.length),
                    interval: entry_value(&widgets.interval),
                }
            } else {
                FragmentOptions::default()
            },
            noises: self.noises.borrow().clone(),
            dns: if widgets.dns.enables_expansion() {
                DnsOptions {
                    server: entry_value(&widgets.dns_server),
                    direct_server: entry_value(&widgets.dns_direct),
                    query_strategy: enum_value(
                        &widgets.query_strategy,
                        self.level,
                        QUERY_STRATEGIES,
                    ),
                }
            } else {
                DnsOptions::default()
            },
        };
        match self.level {
            CoreLevel::Machine => drop_built_ins(options),
            CoreLevel::Profile => options,
        }
    }

    /// Puts the rows back to `values`, showing `inherited` wherever `values`
    /// says nothing.
    pub fn set_values(&self, inherited: &CoreOptions, values: &CoreOptions) {
        let resolved = CoreOptions::resolve(inherited, &CoreOptions::default());
        let widgets = &self.widgets;
        let profile = self.level.is_profile();
        self.updating.set(true);

        set_enum(
            &widgets.log_level,
            self.level,
            LOG_LEVELS,
            values.log_level,
            resolved.log_level,
        );
        set_enum(
            &widgets.domain_strategy,
            self.level,
            DOMAIN_STRATEGIES,
            values.domain_strategy,
            resolved.domain_strategy,
        );

        widgets
            .sniffing
            .set_enable_expansion(!profile || !values.sniffing.is_unset());
        widgets
            .sniffing_enabled
            .set_active(values.sniffing.enabled.unwrap_or(resolved.sniffing.enabled));
        let dest = values
            .sniffing
            .dest_override
            .clone()
            .unwrap_or(resolved.sniffing.dest_override);
        for (index, button) in widgets.dest_override.iter().enumerate() {
            button.set_active(dest.contains(&DEST_OVERRIDES[index]));
        }
        widgets.route_only.set_active(
            values
                .sniffing
                .route_only
                .unwrap_or(resolved.sniffing.route_only),
        );

        widgets
            .mux
            .set_enable_expansion(!profile || !values.mux.is_unset());
        widgets
            .mux_enabled
            .set_active(values.mux.enabled.unwrap_or(resolved.mux.is_some()));
        let inherited_mux = resolved.mux.clone().unwrap_or(ResolvedMux {
            concurrency: None,
            xudp_concurrency: None,
            xudp_proxy_udp_443: None,
        });
        set_spin(
            &widgets.concurrency,
            values.mux.concurrency.or(inherited_mux.concurrency),
        );
        set_spin(
            &widgets.xudp_concurrency,
            values
                .mux
                .xudp_concurrency
                .or(inherited_mux.xudp_concurrency),
        );
        set_optional_enum(
            &widgets.xudp_mode,
            XUDP_MODES,
            values
                .mux
                .xudp_proxy_udp_443
                .or(inherited_mux.xudp_proxy_udp_443),
        );

        widgets
            .fragment
            .set_enable_expansion(!profile || !values.fragment.is_unset());
        let inherited_fragment = resolved
            .dialer
            .as_ref()
            .and_then(|dialer| dialer.fragment.clone());
        widgets.fragment_enabled.set_active(
            values
                .fragment
                .enabled
                .unwrap_or(inherited_fragment.is_some()),
        );
        widgets.packets.set_text(&pick(
            values.fragment.packets.as_deref(),
            inherited_fragment.as_ref().map(|f| f.packets.as_str()),
        ));
        widgets.length.set_text(&pick(
            values.fragment.length.as_deref(),
            inherited_fragment.as_ref().map(|f| f.length.as_str()),
        ));
        widgets.interval.set_text(&pick(
            values.fragment.interval.as_deref(),
            inherited_fragment.as_ref().map(|f| f.interval.as_str()),
        ));

        let noises = values.noises.clone();
        widgets
            .noise_row
            .set_title(&describe_noises(noises.as_deref()));
        *self.noises.borrow_mut() = noises;

        widgets
            .dns
            .set_enable_expansion(!profile || !values.dns.is_unset());
        let inherited_dns = resolved.dns.clone();
        widgets.dns_server.set_text(&pick(
            values.dns.server.as_deref(),
            inherited_dns.as_ref().map(|dns| dns.server.as_str()),
        ));
        widgets.dns_direct.set_text(&pick(
            values.dns.direct_server.as_deref(),
            inherited_dns
                .as_ref()
                .and_then(|dns| dns.direct_server.as_deref()),
        ));
        set_enum(
            &widgets.query_strategy,
            self.level,
            QUERY_STRATEGIES,
            values.dns.query_strategy,
            inherited_dns.map_or_else(QueryStrategy::default, |dns| dns.query_strategy),
        );

        self.updating.set(false);
        refresh_hints(&self.widgets, self.level, &self.inherited_text);
    }

    fn connect_signals(&self) {
        let notify: Rc<dyn Fn()> = {
            let updating = self.updating.clone();
            let widgets = self.widgets.clone();
            let changed = self.changed.clone();
            let text = self.inherited_text.clone();
            let level = self.level;
            Rc::new(move || {
                refresh_hints(&widgets, level, &text);
                if updating.get() {
                    return;
                }
                // Cloned out before calling: the listener reads back through
                // `values()`, and a borrow held across that would be live when
                // it does.
                let listener = changed.borrow().clone();
                if let Some(listener) = listener {
                    listener();
                }
            })
        };
        let widgets = &self.widgets;
        for row in [
            &widgets.log_level,
            &widgets.domain_strategy,
            &widgets.xudp_mode,
            &widgets.query_strategy,
        ] {
            row.connect_selected_notify({
                let notify = notify.clone();
                move |_| notify()
            });
        }
        for row in [
            &widgets.sniffing_enabled,
            &widgets.route_only,
            &widgets.mux_enabled,
            &widgets.fragment_enabled,
        ] {
            row.connect_active_notify({
                let notify = notify.clone();
                move |_| notify()
            });
        }
        for button in &widgets.dest_override {
            button.connect_toggled({
                let notify = notify.clone();
                move |_| notify()
            });
        }
        for row in [&widgets.concurrency, &widgets.xudp_concurrency] {
            row.connect_value_notify({
                let notify = notify.clone();
                move |_| notify()
            });
        }
        for row in [
            &widgets.packets,
            &widgets.length,
            &widgets.interval,
            &widgets.dns_server,
            &widgets.dns_direct,
        ] {
            row.connect_changed({
                let notify = notify.clone();
                move |_| notify()
            });
        }
        for row in [
            &widgets.sniffing,
            &widgets.mux,
            &widgets.fragment,
            &widgets.dns,
        ] {
            row.connect_enable_expansion_notify({
                let notify = notify.clone();
                move |_| notify()
            });
        }
    }
}

/// A section header: an expander everywhere, and one that can be switched off
/// as a whole only where "off" has a meaning other than the built-in default.
fn section(title: &str, profile: bool) -> adw::ExpanderRow {
    let row = adw::ExpanderRow::builder().title(title).build();
    row.set_show_enable_switch(profile);
    row
}

fn combo(title: &str, labels: &[&str], unset: Option<&str>) -> adw::ComboRow {
    let mut all: Vec<&str> = Vec::with_capacity(labels.len() + 1);
    if let Some(unset) = unset {
        all.push(unset);
    }
    all.extend_from_slice(labels);
    adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(&all))
        .build()
}

fn entry(title: &str, hint: &str) -> adw::EntryRow {
    let row = adw::EntryRow::builder().title(title).build();
    row.set_tooltip_text(Some(hint));
    row
}

fn index_of<T: PartialEq>(values: &[T], value: T) -> usize {
    values.iter().position(|item| *item == value).unwrap_or(0)
}

fn pick(value: Option<&str>, inherited: Option<&str>) -> String {
    value.or(inherited).unwrap_or_default().to_string()
}

/// A drop-down whose first entry is "Inherited" only in a profile.
fn enum_value<T: Copy>(row: &adw::ComboRow, level: CoreLevel, values: &[T]) -> Option<T> {
    let offset = level.combo_offset();
    row.selected()
        .checked_sub(offset)
        .and_then(|index| values.get(index as usize))
        .copied()
}

fn set_enum<T: Copy + PartialEq>(
    row: &adw::ComboRow,
    level: CoreLevel,
    values: &[T],
    value: Option<T>,
    inherited: T,
) {
    let offset = level.combo_offset();
    match value {
        Some(value) => row.set_selected(index_of(values, value) as u32 + offset),
        // Unset shows as the inherit entry where there is one, and as the value
        // that would be used where there is not.
        None if offset == 1 => row.set_selected(0),
        None => row.set_selected(index_of(values, inherited) as u32),
    }
}

/// A drop-down whose first entry always means "say nothing about this".
fn optional_enum<T: Copy>(row: &adw::ComboRow, values: &[T]) -> Option<T> {
    row.selected()
        .checked_sub(1)
        .and_then(|index| values.get(index as usize))
        .copied()
}

fn set_optional_enum<T: Copy + PartialEq>(row: &adw::ComboRow, values: &[T], value: Option<T>) {
    row.set_selected(value.map_or(0, |value| index_of(values, value) as u32 + 1));
}

/// Zero is not a concurrency the core would accept, so the spin button uses it
/// for "unset" rather than growing a checkbox beside it.
fn spin_value(row: &adw::SpinRow) -> Option<i16> {
    let value = row.value() as i16;
    (value > 0).then_some(value)
}

fn set_spin(row: &adw::SpinRow, value: Option<i16>) {
    row.set_value(f64::from(value.unwrap_or(0)));
}

fn entry_value(row: &adw::EntryRow) -> Option<String> {
    let text = row.text().trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn describe_noises(noises: Option<&[Noise]>) -> String {
    match noises {
        None => "Noise packets".to_string(),
        Some([]) => "Noise packets — none, overriding the machine".to_string(),
        Some([_]) => "Noise packets — 1".to_string(),
        Some(noises) => format!("Noise packets — {}", noises.len()),
    }
}

/// Subtitles that depend on what is currently selected. Called on every change
/// including the ones this module makes itself, so a reset re-explains too.
fn refresh_hints(widgets: &CoreWidgets, level: CoreLevel, text: &InheritedText) {
    widgets
        .log_level
        .set_subtitle(match enum_value(&widgets.log_level, level, LOG_LEVELS) {
            // Both ends of the dial cost something, and neither says so on its
            // own. Measured: `none` silences even the startup banner, which is
            // what the Logs page shows first.
            Some(LogLevel::Debug) => {
                "Every connection is logged; the Logs page keeps only the newest lines"
            }
            Some(LogLevel::Silent) => "Silences the core completely, including its startup banner",
            Some(_) => "",
            None => &text.log_level,
        });
    widgets.domain_strategy.set_subtitle(
        match enum_value(&widgets.domain_strategy, level, DOMAIN_STRATEGIES) {
            Some(DomainStrategy::AsIs) => {
                "Never resolves a name before routing, so IP rules cannot match one"
            }
            Some(DomainStrategy::IpIfNonMatch) => {
                "Resolves a name only when no domain rule matched it"
            }
            Some(DomainStrategy::IpOnDemand) => "Resolves a name before any rule is tried",
            None => &text.domain_strategy,
        },
    );
    widgets.query_strategy.set_subtitle(
        match enum_value(&widgets.query_strategy, level, QUERY_STRATEGIES) {
            Some(QueryStrategy::UseIp) => "Both families, whichever the name resolves to",
            Some(QueryStrategy::UseIpv4) => "IPv4 only",
            Some(QueryStrategy::UseIpv6) => "IPv6 only",
            None => &text.query_strategy,
        },
    );

    // A section that is inherited says what it inherits; one the profile owns
    // says so, because from that point the machine's value stops reaching it.
    for (row, inherited) in [
        (&widgets.sniffing, &text.sniffing),
        (&widgets.mux, &text.mux),
        (&widgets.fragment, &text.fragment),
        (&widgets.dns, &text.dns),
    ] {
        row.set_subtitle(if level.is_profile() && row.enables_expansion() {
            OWNED_BY_PROFILE
        } else {
            inherited
        });
    }
}

/// What every inheriting row should read.
///
/// In `config.toml` nothing is inherited — the drop-downs there name the
/// built-in value outright — so the section lines carry the format notes the
/// rows themselves have no room for instead.
fn inherited_text(level: CoreLevel, resolved: &ResolvedCore) -> InheritedText {
    if level == CoreLevel::Machine {
        return InheritedText {
            fragment: FRAGMENT_DEFAULTS.to_string(),
            dns: DNS_SCOPE.to_string(),
            ..InheritedText::default()
        };
    }

    let follows = |value: &str| format!("Follows the machine: {value}");
    let sniffing = if resolved.sniffing.enabled {
        let protocols: Vec<&str> = resolved
            .sniffing
            .dest_override
            .iter()
            .map(|value| DEST_OVERRIDE_LABELS[index_of(DEST_OVERRIDES, *value)])
            .collect();
        let route_only = if resolved.sniffing.route_only {
            ", route only"
        } else {
            ""
        };
        format!("Machine: on, {}{route_only}", protocols.join(" + "))
    } else {
        "Machine: off".to_string()
    };
    let mux = match &resolved.mux {
        None => "Machine: off".to_string(),
        Some(mux) => match mux.concurrency {
            Some(concurrency) => format!("Machine: on, {concurrency} streams"),
            None => "Machine: on".to_string(),
        },
    };
    let fragment = match resolved
        .dialer
        .as_ref()
        .and_then(|dialer| dialer.fragment.as_ref())
    {
        None => "Machine: off".to_string(),
        Some(fragment) => format!(
            "Machine: {} · {} · {}",
            fragment.packets, fragment.length, fragment.interval
        ),
    };
    // The one place where "inherited" and "off" are not both reachable: with no
    // `enabled` flag of its own, an unset resolver in a profile means "use the
    // machine's", never "use none".
    let dns = match &resolved.dns {
        None => "Machine: no resolver set".to_string(),
        Some(dns) => format!("Machine: {}{DNS_PROFILE_LIMIT}", dns.server),
    };

    InheritedText {
        log_level: follows(LOG_LEVEL_LABELS[index_of(LOG_LEVELS, resolved.log_level)]),
        domain_strategy: follows(
            DOMAIN_STRATEGY_LABELS[index_of(DOMAIN_STRATEGIES, resolved.domain_strategy)],
        ),
        query_strategy: follows(
            QUERY_LABELS[index_of(
                QUERY_STRATEGIES,
                resolved
                    .dns
                    .as_ref()
                    .map_or_else(QueryStrategy::default, |dns| dns.query_strategy),
            )],
        ),
        sniffing,
        mux,
        fragment,
        dns,
    }
}

/// The `[core]` that reproduces the built-in defaults field for field.
///
/// Kept beside [`drop_built_ins`] because it only exists to be compared
/// against; the test below is what stops it from drifting away from the
/// resolver it mirrors.
fn built_in_options() -> CoreOptions {
    CoreOptions {
        log_level: Some(LogLevel::default()),
        domain_strategy: Some(DomainStrategy::default()),
        sniffing: SniffingOptions {
            enabled: Some(true),
            dest_override: Some(vec![DestOverride::Http, DestOverride::Tls]),
            route_only: Some(false),
        },
        mux: MuxOptions {
            enabled: Some(false),
            ..MuxOptions::default()
        },
        fragment: FragmentOptions {
            enabled: Some(false),
            ..FragmentOptions::default()
        },
        noises: None,
        dns: DnsOptions {
            query_strategy: Some(QueryStrategy::default()),
            ..DnsOptions::default()
        },
    }
}

/// At the machine level a field that says exactly what the built-in default
/// already says is stored as unset.
///
/// Field by field, not section by section: with nothing below but the built-in
/// values, dropping one field can never change what the rest of its table
/// means, and the file that comes out names only what its owner actually chose.
/// Turning on multiplexing should not also write down a DNS address family
/// nobody picked.
fn drop_built_ins(mut options: CoreOptions) -> CoreOptions {
    fn clear<T: PartialEq>(value: &mut Option<T>, built_in: Option<T>) {
        if *value == built_in {
            *value = None;
        }
    }

    let built_in = built_in_options();
    clear(&mut options.log_level, built_in.log_level);
    clear(&mut options.domain_strategy, built_in.domain_strategy);
    clear(&mut options.sniffing.enabled, built_in.sniffing.enabled);
    clear(
        &mut options.sniffing.dest_override,
        built_in.sniffing.dest_override,
    );
    clear(
        &mut options.sniffing.route_only,
        built_in.sniffing.route_only,
    );
    clear(&mut options.mux.enabled, built_in.mux.enabled);
    clear(&mut options.fragment.enabled, built_in.fragment.enabled);
    clear(&mut options.dns.query_strategy, built_in.dns.query_strategy);
    options
}

#[cfg(test)]
mod tests {
    use oxidom_core::core_options::NoiseKind;

    use super::*;

    /// The whole machine-level story rests on these two describing the same
    /// generated config, and nothing else would notice them parting ways.
    #[test]
    fn the_built_in_table_generates_what_an_empty_one_generates() {
        assert_eq!(
            CoreOptions::resolve(&built_in_options(), &CoreOptions::default()),
            ResolvedCore::default()
        );
    }

    #[test]
    fn spelling_out_the_built_in_defaults_stores_nothing() {
        // What the machine editor reads back off untouched widgets.
        assert_eq!(drop_built_ins(built_in_options()), CoreOptions::default());
        assert!(drop_built_ins(built_in_options()).is_unset());
    }

    #[test]
    fn one_changed_field_keeps_only_its_own_section() {
        let mut edited = built_in_options();
        edited.fragment.enabled = Some(true);

        let stored = drop_built_ins(edited);
        assert_eq!(stored.fragment.enabled, Some(true));
        assert_eq!(stored.log_level, None);
        assert!(stored.sniffing.is_unset());
        assert!(stored.mux.is_unset());
        assert!(stored.dns.is_unset());
    }

    /// The file should name what its owner chose and nothing else — the
    /// address family the drop-down happened to be resting on is not a choice.
    #[test]
    fn a_configured_resolver_does_not_drag_its_neighbours_into_the_file() {
        let mut edited = built_in_options();
        edited.dns.server = Some("1.1.1.1".to_string());

        let stored = drop_built_ins(edited);
        assert_eq!(stored.dns.server.as_deref(), Some("1.1.1.1"));
        assert_eq!(stored.dns.query_strategy, None);
    }

    #[test]
    fn a_profile_drop_down_reserves_its_first_entry_for_inheriting() {
        assert_eq!(CoreLevel::Machine.combo_offset(), 0);
        assert_eq!(CoreLevel::Profile.combo_offset(), 1);
        assert_eq!(index_of(LOG_LEVELS, LogLevel::Warning), 2);
        assert_eq!(LOG_LEVELS.len(), LOG_LEVEL_LABELS.len());
        assert_eq!(DOMAIN_STRATEGIES.len(), DOMAIN_STRATEGY_LABELS.len());
        assert_eq!(XUDP_MODES.len(), XUDP_LABELS.len());
        assert_eq!(QUERY_STRATEGIES.len(), QUERY_LABELS.len());
        assert_eq!(DEST_OVERRIDES.len(), DEST_OVERRIDE_LABELS.len());
    }

    /// A profile is read far more often than it is edited, and the reader has
    /// no other way to see what a section it does not override actually does.
    #[test]
    fn an_inherited_section_says_what_it_inherits() {
        let machine = CoreOptions {
            fragment: FragmentOptions {
                enabled: Some(true),
                length: Some("40-60".to_string()),
                ..FragmentOptions::default()
            },
            dns: DnsOptions {
                server: Some("1.1.1.1".to_string()),
                query_strategy: Some(QueryStrategy::UseIpv4),
                ..DnsOptions::default()
            },
            ..CoreOptions::default()
        };
        let resolved = CoreOptions::resolve(&machine, &CoreOptions::default());

        let text = inherited_text(CoreLevel::Profile, &resolved);
        assert_eq!(text.log_level, "Follows the machine: Warning");
        assert_eq!(text.query_strategy, "Follows the machine: UseIPv4");
        assert_eq!(text.sniffing, "Machine: on, HTTP + TLS");
        assert_eq!(text.mux, "Machine: off");
        // The two fields the machine left out are still shown, because the
        // profile inherits the defaults that fill them, not the blanks.
        assert_eq!(text.fragment, "Machine: tlshello · 40-60 · 10-20");
        assert!(text.dns.starts_with("Machine: 1.1.1.1"), "{}", text.dns);

        // Nothing is inherited in `config.toml`, so the same lines carry the
        // format notes the rows have no room for instead.
        let machine_text = inherited_text(CoreLevel::Machine, &ResolvedCore::default());
        assert!(machine_text.log_level.is_empty());
        assert_eq!(machine_text.fragment, FRAGMENT_DEFAULTS);
    }

    /// An empty list is a decision — it turns off noises the machine set — and
    /// the row has to read differently from having none at all.
    #[test]
    fn the_noise_row_distinguishes_none_from_deliberately_empty() {
        assert_eq!(describe_noises(None), "Noise packets");
        assert!(describe_noises(Some(&[])).contains("overriding the machine"));
        assert!(
            describe_noises(Some(&[
                Noise {
                    kind: NoiseKind::Rand,
                    packet: "10-20".to_string(),
                    delay: "10-16".to_string(),
                },
                Noise {
                    kind: NoiseKind::Base64,
                    packet: "aGk=".to_string(),
                    delay: "5".to_string(),
                },
            ]))
            .ends_with("2")
        );
    }
}
