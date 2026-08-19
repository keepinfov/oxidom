use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;

use oxidom_core::core_options::CoreOptions;
use oxidom_core::ipc::ProfileEntry;
use oxidom_core::model::Subscription;
use oxidom_core::pool::{PoolKind, PoolQuery, Strategy};
use oxidom_core::profile::{
    self, Profile, ProfileInterface, ProfileProxy, ProfileSelect, RouteMode,
};

use super::super::reduce::describe_pool;
use super::core_editor::{CoreEditor, CoreLevel};
use super::{dialog_content, set_transient_parent, set_validation, validation_label};

const PROFILE_NAME_ERROR: &str = "Use lowercase letters, digits, '_' and '-'; up to 32 \
    characters. The name is also the systemd instance name (oxidom@<name>).";
const PROFILE_NAME_TAKEN: &str = "A profile with this name already exists.";
const PORTS_ERROR: &str = "The SOCKS and HTTP inbounds cannot share a port.";
const SERVER_ERROR: &str = "Choose the server this profile connects to.";
const MISSING_SERVER_HINT: &str = "This handle matches no server the daemon knows.";
const NO_SERVER_LABEL: &str = "Choose a server…";
const DNS_LEAK_WARNING: &str = "All traffic will use the tunnel, but DNS is not routed through \
    it in this release. The system resolver will continue outside the tunnel.";
const HEALTH_BLIND_WARNING: &str = "This strategy keeps unreachable nodes in the rotation. \
    Measured on Xray 26.3.27: with one live and one dead node, half of the requests went into \
    the dead one. Use leastLoad to rotate only across nodes the core can still reach.";
const LEAST_PING_WARNING: &str = "leastPing concentrates traffic on one node and works against \
    spreading activity across IPs.";
const NEW_CONNECTIONS_HINT: &str = "Switching between the group's servers affects only new \
    connections; existing connections do not migrate.";
const POOL_FROM_GROUPS_HINT: &str = "To run several servers at once, save a group on the Servers \
    page and press Connect on it.";
const LIST_MEMBERSHIP_HINT: &str = "A fixed list. New servers do not join it on their own. Change \
    which servers are in it from the group chips on the Servers page.";
const RULE_MEMBERSHIP_HINT: &str = "A rule, so servers a future refresh adds can join it. Change \
    what it matches from the group chips on the Servers page.";

/// One server the profile's picker can point at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerChoice {
    /// What gets written to `select.server`: the alias when the server has
    /// one, its id otherwise.
    pub handle: String,
    /// What the user reads in the drop-down.
    pub label: String,
}

/// Index of a stored handle in the current choices, or how it should degrade.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preselect {
    /// `choices[index]` is the stored handle.
    Found { index: usize },
    /// The stored handle matches no server. Show it anyway, first and marked,
    /// so opening a profile and pressing Save cannot silently repoint it at
    /// whatever happened to be at the top of the list.
    Missing { label: String },
    /// The profile has no server yet (a freshly created one).
    Empty,
}

/// What the dialog does with what the user typed. Separate from the page's
/// own callbacks: the page no longer owns the dialog.
#[derive(Clone)]
pub struct ProfileDialogCallbacks {
    /// `(name, profile)` — both for editing and creating.
    pub save: Rc<dyn Fn(String, Profile)>,
    pub remove: Rc<dyn Fn(String)>,
}

pub enum ProfileDialog<'a> {
    /// Editing `name`, whose current contents are `entry`.
    Edit {
        name: &'a str,
        entry: &'a ProfileEntry,
    },
    New {
        pool: Option<PoolQuery>,
    },
}

#[derive(Clone)]
struct PickerEntry {
    handle: String,
    label: String,
    missing: bool,
    /// Not a server: the "nothing chosen yet" entry. `AdwComboRow` wraps a
    /// `GtkSingleSelection` that autoselects, so it refuses to hold
    /// `INVALID_LIST_POSITION` and snaps to item 0 instead. Without a real
    /// item to land on, opening a profile that has no server and pressing
    /// Save would point it at whatever happened to sort first.
    placeholder: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DialogValues {
    description: String,
    server: String,
    pool: Option<PoolQuery>,
    socks_port: u16,
    http_port: u16,
    interface_enable: bool,
    interface_device: String,
    /// C2 has no address row, but a save must still preserve the loaded value.
    interface_address: String,
    interface_mtu: u16,
    interface_routes: RouteMode,
    interface_list: String,
    /// Only the sections this profile overrides; the rest is inherited from
    /// `config.toml` and never written here.
    core: CoreOptions,
}

impl DialogValues {
    fn new(entry: Option<&ProfileEntry>) -> Self {
        let proxy = entry.map_or_else(ProfileProxy::default, |entry| ProfileProxy {
            socks_port: entry.socks_port,
            http_port: entry.http_port,
        });
        let interface = entry
            .map(|entry| entry.interface.clone())
            .unwrap_or_default();
        Self {
            description: entry
                .map(|entry| entry.description.clone())
                .unwrap_or_default(),
            server: entry.map(|entry| entry.server.clone()).unwrap_or_default(),
            pool: entry.and_then(|entry| entry.pool.clone()),
            socks_port: proxy.socks_port,
            http_port: proxy.http_port,
            interface_enable: interface.enable,
            interface_device: interface.device,
            interface_address: interface.address,
            interface_mtu: interface.mtu,
            interface_routes: interface.routes,
            interface_list: interface.list.join(", "),
            core: entry.map(|entry| entry.core.clone()).unwrap_or_default(),
        }
    }
}

fn profile_from_dialog(values: DialogValues) -> Profile {
    Profile {
        description: values.description,
        select: ProfileSelect {
            server: values.server,
            pool: values.pool,
        },
        proxy: ProfileProxy {
            socks_port: values.socks_port,
            http_port: values.http_port,
        },
        interface: ProfileInterface {
            enable: values.interface_enable,
            device: values.interface_device,
            address: values.interface_address,
            mtu: values.interface_mtu,
            routes: values.interface_routes,
            list: parse_subnets(&values.interface_list),
        },
        core: values.core,
    }
}

pub fn show_profile_dialog(
    parent: &impl IsA<gtk::Widget>,
    mode: ProfileDialog<'_>,
    profiles: &[ProfileEntry],
    choices: &[ServerChoice],
    // The machine's `[core]`, so every section this profile does not override
    // can show what it inherits instead of standing blank.
    machine_core: &CoreOptions,
    callbacks: ProfileDialogCallbacks,
) {
    let (title, edit_name, initial) = match mode {
        ProfileDialog::Edit { name, entry } => (
            format!("Edit {name}"),
            Some(name.to_string()),
            DialogValues::new(Some(entry)),
        ),
        ProfileDialog::New { pool } => {
            let mut initial = DialogValues::new(None);
            initial.pool = pool;
            ("New profile".to_string(), None, initial)
        }
    };

    let window = adw::Window::builder()
        .title(&title)
        .modal(true)
        .default_width(520)
        .default_height(680)
        .build();
    set_transient_parent(&window, parent);

    let header = adw::HeaderBar::new();
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    save.set_sensitive(false);
    header.pack_start(&cancel);
    header.pack_end(&save);

    let profile_group = adw::PreferencesGroup::builder().title("Profile").build();
    let name_entry = edit_name.is_none().then(|| {
        let entry = adw::EntryRow::builder()
            .title("Name")
            .activates_default(true)
            .build();
        profile_group.add(&entry);
        entry
    });
    let description_entry = adw::EntryRow::builder()
        .title("Description")
        .text(&initial.description)
        .activates_default(true)
        .build();
    profile_group.add(&description_entry);

    // "Group" is offered only to a profile that already has one. An empty one
    // is not a starting point the user can fill in here any more — it is an
    // unfiltered rule, i.e. every server on the machine — so the entry point
    // for making one is the group chips, where the servers actually are.
    let has_pool = initial.pool.is_some();
    let selection_labels = if has_pool {
        gtk::StringList::new(&["Single server", "Group"])
    } else {
        gtk::StringList::new(&["Single server"])
    };
    let selection_mode = adw::ComboRow::builder()
        .title("Selection")
        .subtitle(if has_pool { "" } else { POOL_FROM_GROUPS_HINT })
        .model(&selection_labels)
        .selected(u32::from(has_pool))
        .build();
    profile_group.add(&selection_mode);

    let (picker_entries, selected) = picker_entries(choices, &initial.server);
    let picker_entries = Rc::new(picker_entries);
    let labels: Vec<&str> = picker_entries
        .iter()
        .map(|entry| entry.label.as_str())
        .collect();
    let servers = gtk::StringList::new(&labels);
    let server = adw::ComboRow::builder()
        .title("Server")
        .model(&servers)
        // `enable-search` is inert on its own: AdwComboRow filters through the
        // expression, and with none set the search entry appears and matches
        // nothing. For a GtkStringList the string to match on is the item's
        // own `string` property.
        .expression(gtk::PropertyExpression::new(
            gtk::StringObject::static_type(),
            None::<gtk::Expression>,
            "string",
        ))
        .enable_search(true)
        .build();
    // After the model, not with it: the selection is a position *into* the
    // model, and a builder that happened to apply the two in the other order
    // would drop it.
    server.set_selected(selected);
    server.set_visible(initial.pool.is_none());
    profile_group.add(&server);

    let socks = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    socks.set_title("SOCKS port");
    socks.set_value(f64::from(initial.socks_port));
    profile_group.add(&socks);
    let http = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    http.set_title("HTTP port");
    http.set_value(f64::from(initial.http_port));
    profile_group.add(&http);

    let initial_pool = initial.pool.clone().unwrap_or_default();
    let pool_group = adw::PreferencesGroup::builder()
        .title("Group")
        // The one place the stored name is worth saying: somebody reading the
        // profile file or the CLI meets `pool` there and has to know it is this.
        .description("Written as `pool` in the profile file, and called that by the CLI")
        .visible(initial.pool.is_some())
        .build();
    let strategy_labels = gtk::StringList::new(&["leastLoad", "roundRobin", "random", "leastPing"]);
    let strategy = adw::ComboRow::builder()
        .title("Strategy")
        .subtitle(strategy_hint(initial_pool.strategy))
        .model(&strategy_labels)
        .selected(strategy_index(initial_pool.strategy))
        .build();
    pool_group.add(&strategy);

    // Which servers a pool holds is chosen where the servers are, on the
    // Servers page, with real country and protocol pickers. Four comma-separated
    // text fields were the only way to say it before that existed; keeping them
    // now would be a second, blind editor for the same thing — and a save from
    // one of them could silently disagree with the group the pool came from.
    // The membership is carried through untouched and reported here instead.
    let membership = adw::ActionRow::builder()
        .title(if initial_pool.name.is_empty() {
            describe_pool(&initial_pool)
        } else {
            format!("{} — {}", initial_pool.name, describe_pool(&initial_pool))
        })
        .subtitle(match initial_pool.kind() {
            PoolKind::List => LIST_MEMBERSHIP_HINT,
            PoolKind::Rule => RULE_MEMBERSHIP_HINT,
        })
        .subtitle_lines(3)
        .activatable(false)
        .build();
    pool_group.add(&membership);

    let pool_max = adw::SpinRow::with_range(0.0, profile::MAX_POOL_MEMBERS as f64, 1.0);
    pool_max.set_title("Maximum nodes");
    pool_max.set_subtitle("0 means no query limit; activation still caps a group at 64 nodes");
    pool_max.set_value(initial_pool.max as f64);
    pool_group.add(&pool_max);
    let pool_expected = adw::SpinRow::with_range(0.0, profile::MAX_POOL_MEMBERS as f64, 1.0);
    pool_expected.set_title("Nodes in rotation");
    pool_expected.set_subtitle(
        "How many reachable nodes leastLoad rotates across; 0 means all of them. Ignored by the \
         other strategies.",
    );
    pool_expected.set_value(initial_pool.expected as f64);
    pool_group.add(&pool_expected);
    let pool_probe_interval = adw::EntryRow::builder()
        .title("Probe interval")
        .text(&initial_pool.probe_interval)
        .build();
    pool_group.add(&pool_probe_interval);
    let switching_hint = adw::ActionRow::builder()
        .title("Existing connections stay on their current exit")
        .subtitle(NEW_CONNECTIONS_HINT)
        .subtitle_lines(2)
        .activatable(false)
        .build();
    switching_hint.add_prefix(&gtk::Image::from_icon_name("dialog-information-symbolic"));
    pool_group.add(&switching_hint);

    let interface_group = adw::PreferencesGroup::builder().title("Interface").build();
    let interface_enable = adw::SwitchRow::builder()
        .title("Enable interface")
        .subtitle("Off means this profile exposes only local SOCKS and HTTP proxies")
        .active(initial.interface_enable)
        .build();
    interface_group.add(&interface_enable);

    let route_labels = gtk::StringList::new(&["manual", "list", "default"]);
    let routes = adw::ComboRow::builder()
        .title("Routes")
        .model(&route_labels)
        .selected(route_mode_index(initial.interface_routes))
        .build();
    interface_group.add(&routes);

    // A row of the group rather than an `adw::Banner`: a banner is meant to
    // span a window edge to edge and keeps square corners, which inside a
    // rounded dialog reads as a rendering fault. As a row it also sits
    // directly under the setting it is about — and never last, so that a
    // hidden trailing row cannot leave the real last row square-cornered.
    let dns_warning = adw::ActionRow::builder()
        .title("DNS stays outside the tunnel")
        .subtitle(DNS_LEAK_WARNING)
        .subtitle_lines(3)
        .activatable(false)
        .visible(dns_leak(initial.interface_enable, initial.interface_routes))
        .build();
    let dns_icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
    dns_icon.add_css_class("dns-leak-icon");
    dns_warning.add_prefix(&dns_icon);
    dns_warning.add_css_class("dns-leak-row");
    interface_group.add(&dns_warning);

    let device = adw::EntryRow::builder()
        .title("Device")
        .text(&initial.interface_device)
        .build();
    let device_hint = gtk::Label::builder()
        .css_classes(["dim-label"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(22)
        .build();
    device.add_suffix(&device_hint);
    interface_group.add(&device);

    let mtu = adw::SpinRow::with_range(0.0, 65535.0, 1.0);
    mtu.set_title("MTU");
    mtu.set_subtitle("0 selects the default MTU (1500)");
    mtu.set_value(f64::from(initial.interface_mtu));
    interface_group.add(&mtu);

    let routed_subnets = adw::EntryRow::builder()
        .title("Routed subnets")
        .text(&initial.interface_list)
        .visible(initial.interface_routes == RouteMode::List)
        .build();
    interface_group.add(&routed_subnets);

    let core_editor = CoreEditor::new(CoreLevel::Profile, machine_core, &initial.core);

    let groups = gtk::Box::new(gtk::Orientation::Vertical, 24);
    groups.append(&profile_group);
    groups.append(&pool_group);
    groups.append(&interface_group);
    groups.append(&core_editor.group);
    if let Some(name) = edit_name.as_deref() {
        let remove_group = adw::PreferencesGroup::builder().title("Remove").build();
        let delete = gtk::Button::with_label("Remove profile");
        delete.set_halign(gtk::Align::Start);
        delete.add_css_class("destructive-action");
        remove_group.add(&delete);
        groups.append(&remove_group);

        let name = name.to_string();
        let remove = callbacks.remove.clone();
        let profile_window = window.clone();
        delete.connect_clicked(move |_| {
            let dialog = adw::AlertDialog::new(
                Some("Remove profile?"),
                Some(&format!(
                    "“{name}” will be removed. The tunnel it started, if any, keeps running."
                )),
            );
            dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Remove")]);
            dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            dialog.connect_response(None, {
                let name = name.clone();
                let remove = remove.clone();
                let profile_window = profile_window.clone();
                move |dialog, response| {
                    dialog.close();
                    if response == "delete" {
                        remove(name.clone());
                        profile_window.close();
                    }
                }
            });
            dialog.present(Some(&profile_window));
        });
    }

    let validation = validation_label();
    let content = dialog_content(&groups, &validation);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&header);
    page.append(&content);
    window.set_content(Some(&page));
    window.set_default_widget(Some(&save));

    let existing_names = Rc::new(
        profiles
            .iter()
            .map(|profile| profile.name.clone())
            .collect::<Vec<_>>(),
    );
    let interface_address = initial.interface_address.clone();
    let collect_profile: Rc<dyn Fn() -> Option<(String, Profile)>> = Rc::new({
        let core_editor = core_editor.clone();
        let name_entry = name_entry.clone();
        let edit_name = edit_name.clone();
        let description_entry = description_entry.clone();
        let selection_mode = selection_mode.clone();
        let server = server.clone();
        let picker_entries = picker_entries.clone();
        let strategy = strategy.clone();
        let carried = initial_pool.clone();
        let pool_max = pool_max.clone();
        let pool_expected = pool_expected.clone();
        let pool_probe_interval = pool_probe_interval.clone();
        let socks = socks.clone();
        let http = http.clone();
        let interface_enable = interface_enable.clone();
        let device = device.clone();
        let mtu = mtu.clone();
        let routes = routes.clone();
        let routed_subnets = routed_subnets.clone();
        move || {
            let name = dialog_name(name_entry.as_ref(), edit_name.as_deref());
            let pool_mode = selection_mode.selected() == 1;
            let selected = (!pool_mode)
                .then(|| picker_entries.get(server.selected() as usize))
                .flatten();
            if selected.is_some_and(|entry| entry.placeholder) || (!pool_mode && selected.is_none())
            {
                return None;
            }
            // Everything about *which* servers is carried, not rebuilt: this
            // dialog only reports it, and saving is not allowed to erase what
            // it did not offer to edit. What is read back is the three runtime
            // knobs, which have no home anywhere else.
            let pool = pool_mode.then(|| PoolQuery {
                strategy: strategy_from_index(strategy.selected()),
                max: pool_max.value() as usize,
                expected: pool_expected.value() as usize,
                probe_interval: pool_probe_interval.text().trim().to_string(),
                ..carried.clone()
            });
            let values = DialogValues {
                description: description_entry.text().to_string(),
                server: selected
                    .map(|entry| entry.handle.clone())
                    .unwrap_or_default(),
                pool,
                socks_port: socks.value() as u16,
                http_port: http.value() as u16,
                interface_enable: interface_enable.is_active(),
                interface_device: device.text().to_string(),
                interface_address: interface_address.clone(),
                interface_mtu: mtu.value() as u16,
                interface_routes: route_mode(routes.selected()),
                interface_list: routed_subnets.text().to_string(),
                core: core_editor.values(),
            };
            Some((name, profile_from_dialog(values)))
        }
    });

    let update_validation: Rc<dyn Fn()> = Rc::new({
        let name_entry = name_entry.clone();
        let edit_name = edit_name.clone();
        let existing_names = existing_names.clone();
        let selection_mode = selection_mode.clone();
        let server = server.clone();
        let picker_entries = picker_entries.clone();
        let pool_group = pool_group.clone();
        let strategy = strategy.clone();
        let socks = socks.clone();
        let http = http.clone();
        let routes = routes.clone();
        let routed_subnets = routed_subnets.clone();
        let dns_warning = dns_warning.clone();
        let device_hint = device_hint.clone();
        let save = save.clone();
        let validation = validation.clone();
        let collect_profile = collect_profile.clone();
        let interface_enable = interface_enable.clone();
        move || {
            let name = dialog_name(name_entry.as_ref(), edit_name.as_deref());
            set_device_hint(&device_hint, &name);
            let name_issue = name_entry
                .as_ref()
                .and_then(|entry| profile_name_validation(entry.text().as_str(), &existing_names));
            let pool_mode = selection_mode.selected() == 1;
            let selected = picker_entries.get(server.selected() as usize);
            server.set_visible(!pool_mode);
            pool_group.set_visible(pool_mode);
            strategy.set_subtitle(strategy_hint(strategy_from_index(strategy.selected())));
            server.set_subtitle(if selected.is_some_and(|entry| entry.missing) {
                MISSING_SERVER_HINT
            } else {
                ""
            });
            let route_mode = route_mode(routes.selected());
            routed_subnets.set_visible(route_mode == RouteMode::List);
            dns_warning.set_visible(dns_leak(interface_enable.is_active(), route_mode));

            let issue = name_issue
                .map(str::to_string)
                .or_else(|| {
                    (socks.value() as u16 == http.value() as u16).then(|| PORTS_ERROR.to_string())
                })
                .or_else(|| {
                    (!pool_mode && selected.is_none_or(|entry| entry.placeholder))
                        .then(|| SERVER_ERROR.to_string())
                })
                .or_else(|| {
                    collect_profile().and_then(|(_, profile)| {
                        profile.validate(&name).err().map(|error| error.to_string())
                    })
                });
            save.set_sensitive(issue.is_none());
            set_validation(&validation, issue.as_deref());
        }
    });
    if let Some(entry) = name_entry.as_ref() {
        entry.connect_changed({
            let update_validation = update_validation.clone();
            move |_| update_validation()
        });
    }
    description_entry.connect_changed({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    selection_mode.connect_selected_notify({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    server.connect_selected_notify({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    strategy.connect_selected_notify({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    pool_probe_interval.connect_changed({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    pool_max.connect_value_notify({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    pool_expected.connect_value_notify({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    socks.connect_value_notify({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    http.connect_value_notify({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    interface_enable.connect_active_notify({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    // `default` is the one choice here that silently sends every DNS lookup
    // around the tunnel it just routed everything into. A row can be scrolled
    // past, so choosing it asks — and cancelling puts the combo back where it
    // was, which is why the previous index is remembered.
    let previous_route = Rc::new(Cell::new(routes.selected()));
    // Putting the combo back must not look like a fresh choice to this very
    // handler, or cancelling would ask again about the mode it just left.
    let reverting_routes = Rc::new(Cell::new(false));
    routes.connect_selected_notify({
        let update_validation = update_validation.clone();
        let window = window.clone();
        let previous_route = previous_route.clone();
        let reverting_routes = reverting_routes.clone();
        move |routes| {
            update_validation();
            if reverting_routes.get() {
                previous_route.set(routes.selected());
                return;
            }
            if route_mode(routes.selected()) != RouteMode::Default {
                previous_route.set(routes.selected());
                return;
            }
            confirm_dns_leak(
                &window,
                routes,
                previous_route.get(),
                reverting_routes.clone(),
                update_validation.clone(),
            );
            previous_route.set(routes.selected());
        }
    });
    device.connect_changed({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    mtu.connect_value_notify({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    routed_subnets.connect_changed({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    // `[core]` is validated by `Profile::validate`, which `update_validation`
    // already runs — the editor only has to say when something changed.
    core_editor.connect_changed({
        let update_validation = update_validation.clone();
        move || update_validation()
    });
    update_validation();

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    let window_for_save = window.clone();
    save.connect_clicked(move |button| {
        if !button.is_sensitive() {
            return;
        }
        let Some((name, profile)) = collect_profile() else {
            return;
        };
        (callbacks.save)(name, profile);
        window_for_save.close();
    });
    window.present();
}

fn dialog_name(name_entry: Option<&adw::EntryRow>, edit_name: Option<&str>) -> String {
    name_entry
        .map(|entry| entry.text().to_string())
        .or_else(|| edit_name.map(str::to_string))
        .unwrap_or_default()
}

fn set_device_hint(label: &gtk::Label, name: &str) {
    match oxidom_core::bind::device_name(name) {
        Ok(device) => label.set_label(&format!("Auto: {device}")),
        Err(_) => label.set_label("Auto name does not fit"),
    }
}

/// Does this combination actually send DNS outside the tunnel? Only a routed
/// interface can; `routes = "default"` on a disabled interface is a stored
/// intention, not a leak, and warning about it would be noise.
fn dns_leak(enable: bool, routes: RouteMode) -> bool {
    enable && routes == RouteMode::Default
}

fn confirm_dns_leak(
    parent: &adw::Window,
    routes: &adw::ComboRow,
    previous: u32,
    reverting: Rc<std::cell::Cell<bool>>,
    update_validation: Rc<dyn Fn()>,
) {
    let dialog = adw::AlertDialog::new(
        Some("Route everything, but not DNS?"),
        Some(DNS_LEAK_WARNING),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("accept", "Route everything")]);
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, {
        let routes = routes.clone();
        move |dialog, response| {
            dialog.close();
            if response == "accept" {
                return;
            }
            reverting.set(true);
            routes.set_selected(previous);
            reverting.set(false);
            update_validation();
        }
    });
    dialog.present(Some(parent));
}

fn route_mode_index(mode: RouteMode) -> u32 {
    match mode {
        RouteMode::Manual => 0,
        RouteMode::List => 1,
        RouteMode::Default => 2,
    }
}

fn strategy_index(strategy: Strategy) -> u32 {
    match strategy {
        Strategy::LeastLoad => 0,
        Strategy::RoundRobin => 1,
        Strategy::Random => 2,
        Strategy::LeastPing => 3,
    }
}

fn strategy_from_index(index: u32) -> Strategy {
    match index {
        1 => Strategy::RoundRobin,
        2 => Strategy::Random,
        3 => Strategy::LeastPing,
        _ => Strategy::LeastLoad,
    }
}

/// Both hints describe measured behaviour, not preference. A user who picks a
/// health-blind strategy has to learn it here rather than from a pool that
/// quietly swallows part of its traffic.
fn strategy_hint(strategy: Strategy) -> &'static str {
    if strategy.picks_one() {
        LEAST_PING_WARNING
    } else if strategy.keeps_dead_nodes() {
        HEALTH_BLIND_WARNING
    } else {
        ""
    }
}

fn parse_values(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn route_mode(index: u32) -> RouteMode {
    match index {
        1 => RouteMode::List,
        2 => RouteMode::Default,
        _ => RouteMode::Manual,
    }
}

fn parse_subnets(text: &str) -> Vec<String> {
    parse_values(text)
}

fn picker_entries(choices: &[ServerChoice], stored: &str) -> (Vec<PickerEntry>, u32) {
    let mut entries: Vec<PickerEntry> = choices
        .iter()
        .map(|choice| PickerEntry {
            handle: choice.handle.clone(),
            label: choice.label.clone(),
            missing: false,
            placeholder: false,
        })
        .collect();
    match preselect(choices, stored) {
        Preselect::Found { index } => (entries, index as u32),
        Preselect::Missing { label } => {
            entries.insert(
                0,
                PickerEntry {
                    handle: stored.to_string(),
                    label,
                    missing: true,
                    placeholder: false,
                },
            );
            (entries, 0)
        }
        Preselect::Empty => {
            entries.insert(
                0,
                PickerEntry {
                    handle: String::new(),
                    label: NO_SERVER_LABEL.to_string(),
                    missing: false,
                    placeholder: true,
                },
            );
            (entries, 0)
        }
    }
}

/// Every server the user can point a profile at, in the order the drop-down
/// shows them.
pub fn server_choices(subscriptions: &[Subscription]) -> Vec<ServerChoice> {
    let mut used = HashSet::new();
    let mut choices = Vec::new();
    for server in subscriptions
        .iter()
        .flat_map(|subscription| subscription.servers.iter())
    {
        let handle = server.alias.clone().unwrap_or_else(|| server.id.clone());
        if !used.insert(handle.clone()) {
            continue;
        }
        choices.push(ServerChoice {
            label: format!(
                "{handle}  ·  {}",
                oxidom_core::model::name_without_flag(&server.name)
            ),
            handle,
        });
    }
    choices.sort_by_key(|choice| choice.label.to_lowercase());
    choices
}

/// Index of `stored` in `choices`, and the entry to prepend when it is not
/// there at all.
pub fn preselect(choices: &[ServerChoice], stored: &str) -> Preselect {
    if stored.is_empty() {
        return Preselect::Empty;
    }
    choices
        .iter()
        .position(|choice| choice.handle == stored)
        .map_or_else(
            || Preselect::Missing {
                label: format!("{stored} — not found"),
            },
            |index| Preselect::Found { index },
        )
}

pub fn profile_name_validation(name: &str, existing_names: &[String]) -> Option<&'static str> {
    if !profile::valid_name(name) {
        Some(PROFILE_NAME_ERROR)
    } else if existing_names.iter().any(|existing| existing == name) {
        Some(PROFILE_NAME_TAKEN)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use oxidom_core::core_options::{FragmentOptions, LogLevel};

    use super::*;
    use oxidom_core::link::parse_link;

    fn subscription(servers: Vec<oxidom_core::model::Server>) -> Subscription {
        let mut subscription = Subscription::new(
            "https://example.test/sub".to_string(),
            Some("Test".to_string()),
        );
        subscription.servers = servers;
        subscription
    }

    #[test]
    fn choices_use_alias_or_id_drop_flags_deduplicate_and_sort() {
        let mut by_alias =
            parse_link("trojan://secret@z.example:443#Zulu").expect("valid test link");
        by_alias.id = "id-zulu".to_string();
        by_alias.alias = Some("alpha".to_string());
        let mut by_id =
            parse_link("trojan://secret@a.example:443#%F0%9F%87%A8%F0%9F%87%AD%20Alpine")
                .expect("valid test link");
        by_id.id = "bravo".to_string();
        by_id.alias = None;
        let mut duplicate = by_alias.clone();
        duplicate.name = "Ignored duplicate".to_string();

        let choices = server_choices(&[subscription(vec![by_id, by_alias, duplicate])]);
        assert_eq!(
            choices,
            vec![
                ServerChoice {
                    handle: "alpha".to_string(),
                    label: "alpha  ·  Zulu".to_string(),
                },
                ServerChoice {
                    handle: "bravo".to_string(),
                    label: "bravo  ·  Alpine".to_string(),
                },
            ]
        );
        assert!(choices.iter().all(|choice| !choice.label.contains('🇨')));
    }

    #[test]
    fn preselection_never_silently_repoints_a_missing_handle() {
        let choices = vec![ServerChoice {
            handle: "known".to_string(),
            label: "known  ·  Server".to_string(),
        }];
        assert_eq!(preselect(&choices, "known"), Preselect::Found { index: 0 });
        assert_eq!(
            preselect(&choices, "gone"),
            Preselect::Missing {
                label: "gone — not found".to_string()
            }
        );
        assert_eq!(preselect(&choices, ""), Preselect::Empty);
    }

    /// A profile with no server must land on something the dialog refuses to
    /// save, because the combo will not stay unselected: without an entry of
    /// its own it silently picks the first real server in the list.
    #[test]
    fn a_profile_without_a_server_gets_an_entry_that_is_not_a_server() {
        let choices = vec![
            ServerChoice {
                handle: "alpha".to_string(),
                label: "alpha  ·  Alpha".to_string(),
            },
            ServerChoice {
                handle: "bravo".to_string(),
                label: "bravo  ·  Bravo".to_string(),
            },
        ];

        let (entries, selected) = picker_entries(&choices, "");
        assert_eq!(selected, 0);
        assert!(entries[0].placeholder);
        assert!(entries[0].handle.is_empty());
        assert_eq!(entries[1].handle, "alpha");

        // A stored handle still selects its own server, and nothing is added.
        let (entries, selected) = picker_entries(&choices, "bravo");
        assert_eq!(selected, 1);
        assert!(entries.iter().all(|entry| !entry.placeholder));
    }

    #[test]
    fn profile_name_validation_covers_format_and_collisions() {
        let existing = vec!["work".to_string()];
        for valid in ["home", "work_2", "a"] {
            assert_eq!(profile_name_validation(valid, &existing), None, "{valid:?}");
        }
        for invalid in [
            "",
            "Home",
            "home.office",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert_eq!(
                profile_name_validation(invalid, &existing),
                Some(PROFILE_NAME_ERROR),
                "{invalid:?}"
            );
        }
        assert_eq!(
            profile_name_validation("work", &existing),
            Some(PROFILE_NAME_TAKEN)
        );
    }

    /// `routes = "default"` on a profile whose interface is off routes
    /// nothing, so warning about its DNS would train the user to dismiss the
    /// warning that matters.
    #[test]
    fn only_a_routed_interface_counts_as_a_dns_leak() {
        assert!(dns_leak(true, RouteMode::Default));
        assert!(!dns_leak(false, RouteMode::Default));
        assert!(!dns_leak(true, RouteMode::Manual));
        assert!(!dns_leak(true, RouteMode::List));
    }

    #[test]
    fn what_the_dialog_never_shows_survives_an_unchanged_round_trip() {
        let entry = ProfileEntry {
            name: "work".to_string(),
            description: "Office tunnel".to_string(),
            server: String::new(),
            socks_port: 12080,
            http_port: 12081,
            interface: ProfileInterface {
                enable: true,
                device: "corp0".to_string(),
                address: "198.18.7.1".to_string(),
                mtu: 1400,
                routes: RouteMode::List,
                list: vec!["10.0.0.0/8".to_string(), "172.16.0.0/12".to_string()],
            },
            pool: Some(PoolQuery {
                // Neither of these has a widget in the dialog, which is
                // exactly why they belong in this test.
                name: "Europe".to_string(),
                members: Vec::new(),
                strategy: Strategy::LeastPing,
                subscriptions: vec!["main".to_string()],
                countries: vec!["ch".to_string(), "de".to_string()],
                protocols: vec!["vless".to_string()],
                exclude: vec!["slow".to_string()],
                max: 8,
                expected: 4,
                probe_interval: "30s".to_string(),
            }),
            // `[core]` does have rows now, but it reaches them through the
            // same carrier, and dropping it on the way would quietly
            // un-fragment a profile that only connects because of it.
            core: CoreOptions {
                log_level: Some(LogLevel::Debug),
                fragment: FragmentOptions {
                    enabled: Some(true),
                    length: Some("40-60".to_string()),
                    ..FragmentOptions::default()
                },
                ..CoreOptions::default()
            },
        };

        let saved = profile_from_dialog(DialogValues::new(Some(&entry)));
        assert_eq!(saved.interface, entry.interface);
        assert_eq!(saved.core, entry.core);
        assert_eq!(saved.description, entry.description);
        assert_eq!(saved.select.server, entry.server);
        assert_eq!(saved.select.pool, entry.pool);
        assert_eq!(saved.proxy.socks_port, entry.socks_port);
        assert_eq!(saved.proxy.http_port, entry.http_port);
        saved.validate(&entry.name).unwrap();
    }

    #[test]
    fn strategy_help_states_the_ip_spreading_tradeoff() {
        assert_eq!(strategy_hint(Strategy::LeastPing), LEAST_PING_WARNING);
        // Both ways of failing the user's goal are called out, and the default
        // — the one that both spreads and drops dead nodes — needs no warning.
        assert_eq!(strategy_hint(Strategy::RoundRobin), HEALTH_BLIND_WARNING);
        assert_eq!(strategy_hint(Strategy::Random), HEALTH_BLIND_WARNING);
        assert_eq!(strategy_hint(Strategy::LeastLoad), "");

        for strategy in [
            Strategy::LeastLoad,
            Strategy::RoundRobin,
            Strategy::Random,
            Strategy::LeastPing,
        ] {
            assert_eq!(strategy_from_index(strategy_index(strategy)), strategy);
        }
        // The picker opens on the default rather than on a health-blind sweep.
        assert_eq!(strategy_index(Strategy::default()), 0);
    }
}
