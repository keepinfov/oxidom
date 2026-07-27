use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;

use crate::gui::operation::UiOperation;
use oxidom_core::ipc::ProfileEntry;
use oxidom_core::model::Subscription;
use oxidom_core::profile::{self, Profile, ProfileProxy, ProfileSelect};

use super::{dialog_content, icon_button, set_transient_parent, set_validation, validation_label};

const PROFILE_NAME_ERROR: &str = "Use lowercase letters, digits, '_' and '-'; up to 32 \
    characters. The name is also the systemd instance name (oxidom@<name>).";
const PROFILE_NAME_TAKEN: &str = "A profile with this name already exists.";
const PORTS_ERROR: &str = "The SOCKS and HTTP inbounds cannot share a port.";
const SERVER_ERROR: &str = "Choose the server this profile connects to.";
const MISSING_SERVER_HINT: &str = "This handle matches no server the daemon knows.";

#[derive(Clone)]
pub struct ProfileCallbacks {
    pub up: Rc<dyn Fn(String)>,
    pub down: Rc<dyn Fn(String)>,
    /// `(name, profile)` — both for editing and creating.
    pub save: Rc<dyn Fn(String, Profile)>,
    pub remove: Rc<dyn Fn(String)>,
}

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

#[derive(Clone)]
struct HeaderControls {
    root: gtk::Box,
    add: gtk::Button,
}

#[derive(Clone)]
struct RowControls {
    row: adw::ActionRow,
    spinner: gtk::Spinner,
}

#[derive(Clone, Default)]
struct DialogData {
    profiles: Vec<ProfileEntry>,
    choices: Vec<ServerChoice>,
}

#[derive(Clone)]
pub struct ProfilesView {
    pub root: gtk::ScrolledWindow,
    content: gtk::Box,
    callbacks: Rc<RefCell<Option<ProfileCallbacks>>>,
    dialog_data: Rc<RefCell<DialogData>>,
    header: HeaderControls,
    header_embedded: Rc<Cell<bool>>,
    operation: Rc<RefCell<Option<UiOperation>>>,
    operation_widgets: Rc<RefCell<HashMap<String, RowControls>>>,
}

impl ProfilesView {
    pub fn new() -> Self {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 22);
        content.set_hexpand(true);
        content.set_margin_top(24);
        content.set_margin_bottom(32);
        content.set_margin_start(28);
        content.set_margin_end(28);
        let root = gtk::ScrolledWindow::builder()
            .child(&content)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let callbacks = Rc::new(RefCell::new(None::<ProfileCallbacks>));
        let dialog_data = Rc::new(RefCell::new(DialogData::default()));
        let header = make_header_controls(&root, callbacks.clone(), dialog_data.clone());

        Self {
            root,
            content,
            callbacks,
            dialog_data,
            header,
            header_embedded: Rc::new(Cell::new(true)),
            operation: Rc::new(RefCell::new(None)),
            operation_widgets: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn header_actions(&self) -> gtk::Box {
        self.header.root.clone()
    }

    pub fn set_header_actions_embedded(&self, embedded: bool) {
        self.header_embedded.set(embedded);
    }

    pub fn rebuild(
        &self,
        profiles: &[ProfileEntry],
        choices: &[ServerChoice],
        active: Option<&str>,
        connected: bool,
        callbacks: ProfileCallbacks,
    ) {
        *self.callbacks.borrow_mut() = Some(callbacks.clone());
        *self.dialog_data.borrow_mut() = DialogData {
            profiles: profiles.to_vec(),
            choices: choices.to_vec(),
        };

        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        self.operation_widgets.borrow_mut().clear();

        let list = adw::PreferencesGroup::builder()
            .title("Profiles")
            .description("Named connection settings, shared with the CLI and systemd")
            .build();
        if self.header_embedded.get() {
            list.set_header_suffix(Some(&self.header.root));
        }

        if profiles.is_empty() {
            let empty = adw::ActionRow::builder()
                .title("No profiles")
                .subtitle(
                    "Use + to create one; `oxidom up <name>` and `systemctl start \
                     oxidom@<name>` use the same files",
                )
                .subtitle_lines(3)
                .activatable(false)
                .build();
            list.add(&empty);
        }

        for entry in profiles {
            self.add_profile_row(
                &list,
                entry,
                choices,
                active == Some(entry.name.as_str()) && connected,
                callbacks.clone(),
            );
        }

        self.content.append(&list);
        self.apply_operation();
    }

    pub fn set_operation(&self, operation: Option<UiOperation>) {
        *self.operation.borrow_mut() = operation;
        self.apply_operation();
    }

    pub fn set_ultra_compact(&self, enabled: bool) {
        self.content.set_spacing(if enabled { 16 } else { 22 });
    }

    fn add_profile_row(
        &self,
        list: &adw::PreferencesGroup,
        entry: &ProfileEntry,
        choices: &[ServerChoice],
        active: bool,
        callbacks: ProfileCallbacks,
    ) {
        let row = adw::ActionRow::builder()
            .title(&entry.name)
            .subtitle(row_subtitle(entry))
            .subtitle_lines(2)
            .activatable(true)
            .build();
        let spinner = gtk::Spinner::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        row.add_suffix(&spinner);
        if active {
            let selected = gtk::Image::from_icon_name("object-select-symbolic");
            selected.set_accessible_role(gtk::AccessibleRole::Presentation);
            row.add_suffix(&selected);
        }

        let action = gtk::Button::with_label(if active { "Disconnect" } else { "Connect" });
        action.set_valign(gtk::Align::Center);
        action.add_css_class(if active { "flat" } else { "suggested-action" });
        if active {
            let callback = callbacks.down.clone();
            let name = entry.name.clone();
            action.connect_clicked(move |_| callback(name.clone()));
        } else {
            let callback = callbacks.up.clone();
            let name = entry.name.clone();
            action.connect_clicked(move |_| callback(name.clone()));
        }
        row.add_suffix(&action);

        let entry = entry.clone();
        let entry_name = entry.name.clone();
        let profiles = self.dialog_data.borrow().profiles.clone();
        let choices = choices.to_vec();
        row.connect_activated(move |row| {
            show_profile_dialog(
                row,
                ProfileDialog::Edit {
                    name: &entry.name,
                    entry: &entry,
                },
                &profiles,
                &choices,
                callbacks.clone(),
            );
        });

        self.operation_widgets.borrow_mut().insert(
            entry_name,
            RowControls {
                row: row.clone(),
                spinner,
            },
        );
        list.add(&row);
    }

    fn apply_operation(&self) {
        let operation = self.operation.borrow().clone();
        let busy = operation.is_some();
        self.header.add.set_sensitive(!busy);
        for (name, controls) in self.operation_widgets.borrow().iter() {
            let affected = operation
                .as_ref()
                .and_then(|operation| operation.profile.as_deref())
                == Some(name.as_str());
            controls.row.set_sensitive(!busy);
            controls.spinner.set_spinning(affected);
            controls.spinner.set_visible(affected);
        }
    }
}

fn make_header_controls(
    root: &gtk::ScrolledWindow,
    callbacks: Rc<RefCell<Option<ProfileCallbacks>>>,
    dialog_data: Rc<RefCell<DialogData>>,
) -> HeaderControls {
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_valign(gtk::Align::Center);
    let add = icon_button("list-add-symbolic", "New profile");
    add.add_css_class("header-icon-button");

    let root = root.downgrade();
    add.connect_clicked(move |_| {
        let Some(root) = root.upgrade() else {
            return;
        };
        let Some(callbacks) = callbacks.borrow().clone() else {
            return;
        };
        let data = dialog_data.borrow().clone();
        show_profile_dialog(
            &root,
            ProfileDialog::New,
            &data.profiles,
            &data.choices,
            callbacks,
        );
    });

    actions.append(&add);
    HeaderControls { root: actions, add }
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

pub fn row_subtitle(entry: &ProfileEntry) -> String {
    if !entry.description.is_empty() {
        entry.description.clone()
    } else {
        format!(
            "{} · socks {} · http {}",
            entry.server, entry.socks_port, entry.http_port
        )
    }
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

enum ProfileDialog<'a> {
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

fn show_profile_dialog(
    parent: &impl IsA<gtk::Widget>,
    mode: ProfileDialog<'_>,
    profiles: &[ProfileEntry],
    choices: &[ServerChoice],
    callbacks: ProfileCallbacks,
) {
    let (title, edit_name, description, stored_server, socks_port, http_port) = match mode {
        ProfileDialog::Edit { name, entry } => (
            format!("Edit {name}"),
            Some(name.to_string()),
            entry.description.clone(),
            entry.server.clone(),
            entry.socks_port,
            entry.http_port,
        ),
        ProfileDialog::New => {
            let defaults = ProfileProxy::default();
            (
                "New Profile".to_string(),
                None,
                String::new(),
                String::new(),
                defaults.socks_port,
                defaults.http_port,
            )
        }
    };

    let window = adw::Window::builder()
        .title(&title)
        .modal(true)
        .default_width(520)
        .default_height(520)
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
        .text(&description)
        .activates_default(true)
        .build();
    profile_group.add(&description_entry);

    let (picker_entries, selected) = picker_entries(choices, &stored_server);
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
    socks.set_value(f64::from(socks_port));
    profile_group.add(&socks);
    let http = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    http.set_title("HTTP port");
    http.set_value(f64::from(http_port));
    profile_group.add(&http);

    let groups = gtk::Box::new(gtk::Orientation::Vertical, 24);
    groups.append(&profile_group);
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
    let update_validation: Rc<dyn Fn()> = Rc::new({
        let name_entry = name_entry.clone();
        let existing_names = existing_names.clone();
        let server = server.clone();
        let picker_entries = picker_entries.clone();
        let socks = socks.clone();
        let http = http.clone();
        let save = save.clone();
        let validation = validation.clone();
        move || {
            let name_issue = name_entry
                .as_ref()
                .and_then(|entry| profile_name_validation(entry.text().as_str(), &existing_names));
            let selected = picker_entries.get(server.selected() as usize);
            server.set_subtitle(if selected.is_some_and(|entry| entry.missing) {
                MISSING_SERVER_HINT
            } else {
                ""
            });
            let issue = name_issue.or_else(|| {
                (socks.value() as u16 == http.value() as u16)
                    .then_some(PORTS_ERROR)
                    .or_else(|| selected.is_none().then_some(SERVER_ERROR))
            });
            save.set_sensitive(issue.is_none());
            set_validation(&validation, issue);
        }
    });
    if let Some(entry) = name_entry.as_ref() {
        entry.connect_changed({
            let update_validation = update_validation.clone();
            move |_| update_validation()
        });
    }
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
    update_validation();

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    let window_for_save = window.clone();
    save.connect_clicked(move |button| {
        if !button.is_sensitive() {
            return;
        }
        let name = name_entry
            .as_ref()
            .map(|entry| entry.text().to_string())
            .or_else(|| edit_name.clone())
            .unwrap_or_default();
        let Some(selected) = picker_entries.get(server.selected() as usize) else {
            return;
        };
        let profile = Profile {
            description: description_entry.text().to_string(),
            select: ProfileSelect {
                server: selected.handle.clone(),
            },
            proxy: ProfileProxy {
                socks_port: socks.value() as u16,
                http_port: http.value() as u16,
            },
        };
        (callbacks.save)(name, profile);
        window_for_save.close();
    });
    window.present();
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
    fn profile_rows_prefer_descriptions_and_fall_back_to_connection_details() {
        let mut entry = ProfileEntry {
            name: "work".to_string(),
            description: "Office tunnel".to_string(),
            server: "ch-trojan".to_string(),
            socks_port: 12080,
            http_port: 12081,
        };
        assert_eq!(row_subtitle(&entry), "Office tunnel");
        entry.description.clear();
        assert_eq!(row_subtitle(&entry), "ch-trojan · socks 12080 · http 12081");
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
}
