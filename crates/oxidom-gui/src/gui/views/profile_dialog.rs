use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;

use oxidom_core::ipc::ProfileEntry;
use oxidom_core::model::Subscription;
use oxidom_core::profile::{
    self, Profile, ProfileInterface, ProfileProxy, ProfileSelect, RouteMode,
};

use super::sessions::SessionCallbacks;
use super::{dialog_content, set_transient_parent, set_validation, validation_label};

const PROFILE_NAME_ERROR: &str = "Use lowercase letters, digits, '_' and '-'; up to 32 \
    characters. The name is also the systemd instance name (oxidom@<name>).";
const PROFILE_NAME_TAKEN: &str = "A profile with this name already exists.";
const PORTS_ERROR: &str = "The SOCKS and HTTP inbounds cannot share a port.";
const SERVER_ERROR: &str = "Choose the server this profile connects to.";
const MISSING_SERVER_HINT: &str = "This handle matches no server the daemon knows.";
const DNS_LEAK_WARNING: &str = "All traffic will use the tunnel, but DNS is not routed through \
    it in this release. The system resolver will continue outside the tunnel.";

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

pub(super) enum ProfileDialog<'a> {
    /// Editing `name`, whose current contents are `entry`.
    Edit {
        name: &'a str,
        entry: &'a ProfileEntry,
    },
    New,
}

#[derive(Clone)]
struct PickerEntry {
    handle: String,
    label: String,
    missing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DialogValues {
    description: String,
    server: String,
    socks_port: u16,
    http_port: u16,
    interface_enable: bool,
    interface_device: String,
    /// C2 has no address row, but a save must still preserve the loaded value.
    interface_address: String,
    interface_mtu: u16,
    interface_routes: RouteMode,
    interface_list: String,
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
            socks_port: proxy.socks_port,
            http_port: proxy.http_port,
            interface_enable: interface.enable,
            interface_device: interface.device,
            interface_address: interface.address,
            interface_mtu: interface.mtu,
            interface_routes: interface.routes,
            interface_list: interface.list.join(", "),
        }
    }
}

fn profile_from_dialog(values: DialogValues) -> Profile {
    Profile {
        description: values.description,
        select: ProfileSelect {
            server: values.server,
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
    }
}

pub(super) fn show_profile_dialog(
    parent: &impl IsA<gtk::Widget>,
    mode: ProfileDialog<'_>,
    profiles: &[ProfileEntry],
    choices: &[ServerChoice],
    callbacks: SessionCallbacks,
) {
    let (title, edit_name, initial) = match mode {
        ProfileDialog::Edit { name, entry } => (
            format!("Edit {name}"),
            Some(name.to_string()),
            DialogValues::new(Some(entry)),
        ),
        ProfileDialog::New => ("New Profile".to_string(), None, DialogValues::new(None)),
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
    profile_group.add(&server);

    let socks = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    socks.set_title("SOCKS port");
    socks.set_value(f64::from(initial.socks_port));
    profile_group.add(&socks);
    let http = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    http.set_title("HTTP port");
    http.set_value(f64::from(initial.http_port));
    profile_group.add(&http);

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

    let dns_warning = adw::Banner::builder()
        .title(DNS_LEAK_WARNING)
        .revealed(initial.interface_routes == RouteMode::Default)
        .build();
    dns_warning.add_css_class("error");
    dns_warning.add_css_class("dns-leak-banner");

    let groups = gtk::Box::new(gtk::Orientation::Vertical, 24);
    groups.append(&profile_group);
    groups.append(&interface_group);
    groups.append(&dns_warning);
    if let Some(name) = edit_name.as_deref() {
        let remove_group = adw::PreferencesGroup::builder().title("Remove").build();
        let delete = gtk::Button::with_label("Delete Profile");
        delete.set_halign(gtk::Align::Start);
        delete.add_css_class("destructive-action");
        remove_group.add(&delete);
        groups.append(&remove_group);

        let name = name.to_string();
        let remove = callbacks.remove.clone();
        let profile_window = window.clone();
        delete.connect_clicked(move |_| {
            let dialog = adw::MessageDialog::new(
                Some(&profile_window),
                Some("Delete profile?"),
                Some(&format!(
                    "«{name}» will be removed. The tunnel it started, if any, keeps running."
                )),
            );
            dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
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
            dialog.present();
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
        let name_entry = name_entry.clone();
        let edit_name = edit_name.clone();
        let description_entry = description_entry.clone();
        let server = server.clone();
        let picker_entries = picker_entries.clone();
        let socks = socks.clone();
        let http = http.clone();
        let interface_enable = interface_enable.clone();
        let device = device.clone();
        let mtu = mtu.clone();
        let routes = routes.clone();
        let routed_subnets = routed_subnets.clone();
        move || {
            let name = dialog_name(name_entry.as_ref(), edit_name.as_deref());
            let selected = picker_entries.get(server.selected() as usize)?;
            let values = DialogValues {
                description: description_entry.text().to_string(),
                server: selected.handle.clone(),
                socks_port: socks.value() as u16,
                http_port: http.value() as u16,
                interface_enable: interface_enable.is_active(),
                interface_device: device.text().to_string(),
                interface_address: interface_address.clone(),
                interface_mtu: mtu.value() as u16,
                interface_routes: route_mode(routes.selected()),
                interface_list: routed_subnets.text().to_string(),
            };
            Some((name, profile_from_dialog(values)))
        }
    });

    let update_validation: Rc<dyn Fn()> = Rc::new({
        let name_entry = name_entry.clone();
        let edit_name = edit_name.clone();
        let existing_names = existing_names.clone();
        let server = server.clone();
        let picker_entries = picker_entries.clone();
        let socks = socks.clone();
        let http = http.clone();
        let routes = routes.clone();
        let routed_subnets = routed_subnets.clone();
        let dns_warning = dns_warning.clone();
        let device_hint = device_hint.clone();
        let save = save.clone();
        let validation = validation.clone();
        let collect_profile = collect_profile.clone();
        move || {
            let name = dialog_name(name_entry.as_ref(), edit_name.as_deref());
            set_device_hint(&device_hint, &name);
            let name_issue = name_entry
                .as_ref()
                .and_then(|entry| profile_name_validation(entry.text().as_str(), &existing_names));
            let selected = picker_entries.get(server.selected() as usize);
            server.set_subtitle(if selected.is_some_and(|entry| entry.missing) {
                MISSING_SERVER_HINT
            } else {
                ""
            });
            let route_mode = route_mode(routes.selected());
            routed_subnets.set_visible(route_mode == RouteMode::List);
            dns_warning.set_revealed(route_mode == RouteMode::Default);

            let issue = name_issue
                .map(str::to_string)
                .or_else(|| {
                    (socks.value() as u16 == http.value() as u16).then(|| PORTS_ERROR.to_string())
                })
                .or_else(|| selected.is_none().then(|| SERVER_ERROR.to_string()))
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
    server.connect_selected_notify({
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
    routes.connect_selected_notify({
        let update_validation = update_validation.clone();
        move |_| update_validation()
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

fn route_mode_index(mode: RouteMode) -> u32 {
    match mode {
        RouteMode::Manual => 0,
        RouteMode::List => 1,
        RouteMode::Default => 2,
    }
}

fn route_mode(index: u32) -> RouteMode {
    match index {
        1 => RouteMode::List,
        2 => RouteMode::Default,
        _ => RouteMode::Manual,
    }
}

fn parse_subnets(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn picker_entries(choices: &[ServerChoice], stored: &str) -> (Vec<PickerEntry>, u32) {
    let mut entries: Vec<PickerEntry> = choices
        .iter()
        .map(|choice| PickerEntry {
            handle: choice.handle.clone(),
            label: choice.label.clone(),
            missing: false,
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
                },
            );
            (entries, 0)
        }
        Preselect::Empty => (entries, gtk::INVALID_LIST_POSITION),
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

    #[test]
    fn interface_section_survives_an_unchanged_dialog_round_trip() {
        let entry = ProfileEntry {
            name: "work".to_string(),
            description: "Office tunnel".to_string(),
            server: "ch-trojan".to_string(),
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
        };

        let saved = profile_from_dialog(DialogValues::new(Some(&entry)));
        assert_eq!(saved.interface, entry.interface);
        assert_eq!(saved.description, entry.description);
        assert_eq!(saved.select.server, entry.server);
        assert_eq!(saved.proxy.socks_port, entry.socks_port);
        assert_eq!(saved.proxy.http_port, entry.http_port);
    }
}
