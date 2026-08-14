use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;

use crate::gui::operation::{UiOperation, UiOperationKind};
use oxidom_core::engine::LOCAL_ID;
use oxidom_core::link;
use oxidom_core::model::{Subscription, UserInfo};

use super::super::group::{format_bytes, subscription_description};
use super::{dialog_content, icon_button, set_transient_parent, set_validation, validation_label};

/// Callbacks the subscriptions view invokes.
#[derive(Clone)]
pub struct SubscriptionCallbacks {
    pub add: Rc<dyn Fn(String, Option<String>, bool)>,
    pub import: Rc<dyn Fn(String)>,
    pub refresh: Rc<dyn Fn(String)>,
    pub refresh_all: Rc<dyn Fn()>,
    pub remove: Rc<dyn Fn(String)>,
    pub remove_server: Rc<dyn Fn(String)>,
    pub hwid: Rc<dyn Fn(String, bool)>,
    /// Set one subscription's User-Agent override, or clear it when empty.
    pub user_agent: Rc<dyn Fn(String, String)>,
    /// Names of the groups that would lose a server if it were deleted.
    /// A group is the only thing in this app that holds a server by id, so a
    /// deletion is the one moment a user can be told before it is too late.
    pub groups_holding: Rc<dyn Fn(String) -> Vec<String>>,
    /// The same, for every server of a subscription: `(group name, count)`.
    pub groups_holding_any: Rc<dyn Fn(String) -> Vec<(String, usize)>>,
}

/// "It is also in Favourites and Germany." — appended to a deletion prompt.
fn also_in_groups(groups: &[String]) -> String {
    match groups {
        [] => String::new(),
        [one] => format!(" It will also leave the group “{one}”."),
        many => format!(" It will also leave the groups {}.", quoted_list(many)),
    }
}

/// "8 of them are in “Europe”, 1 in “Favourites”." — the same warning for a
/// whole subscription, where naming every server would be useless.
fn groups_losing_servers(affected: &[(String, usize)]) -> String {
    if affected.is_empty() {
        return String::new();
    }
    let parts = affected
        .iter()
        .map(|(name, count)| format!("{count} in “{name}”"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(" Servers in your groups will go with them: {parts}.")
}

fn quoted_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("“{value}”"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone)]
struct HeaderControls {
    root: gtk::Box,
    add_subscription: gtk::Button,
    import_server: gtk::Button,
    update_all: gtk::Button,
    update_label: gtk::Label,
    update_spinner: gtk::Spinner,
}

#[derive(Clone)]
struct RowControls {
    row: adw::ActionRow,
    spinner: gtk::Spinner,
}

#[derive(Default)]
struct OperationWidgets {
    subscriptions: HashMap<String, RowControls>,
    local_servers: Option<RowControls>,
}

#[derive(Clone)]
pub struct SubscriptionsView {
    pub root: gtk::ScrolledWindow,
    content: gtk::Box,
    callbacks: Rc<RefCell<Option<SubscriptionCallbacks>>>,
    header: HeaderControls,
    header_embedded: Rc<Cell<bool>>,
    operation: Rc<RefCell<Option<UiOperation>>>,
    operation_widgets: Rc<RefCell<OperationWidgets>>,
}

impl SubscriptionsView {
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
        let callbacks = Rc::new(RefCell::new(None::<SubscriptionCallbacks>));
        let header = make_header_controls(&root, callbacks.clone());

        Self {
            root,
            content,
            callbacks,
            header,
            header_embedded: Rc::new(Cell::new(true)),
            operation: Rc::new(RefCell::new(None)),
            operation_widgets: Rc::new(RefCell::new(OperationWidgets::default())),
        }
    }

    /// Controls intended for the subscriptions page's header bar.
    ///
    /// They are embedded in the list header by default so the page remains
    /// usable before its containing window adopts them. Call
    /// [`Self::set_header_actions_embedded`] with `false` before the first
    /// rebuild when placing this widget in the window header.
    pub fn header_actions(&self) -> gtk::Box {
        self.header.root.clone()
    }

    pub fn set_header_actions_embedded(&self, embedded: bool) {
        self.header_embedded.set(embedded);
    }

    pub fn rebuild(&self, subscriptions: &[Subscription], callbacks: SubscriptionCallbacks) {
        *self.callbacks.borrow_mut() = Some(callbacks.clone());

        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        *self.operation_widgets.borrow_mut() = OperationWidgets::default();

        let list = adw::PreferencesGroup::builder()
            .title("Subscriptions")
            .description("Manage providers and standalone share links")
            .build();
        if self.header_embedded.get() {
            list.set_header_suffix(Some(&self.header.root));
        }

        if subscriptions.is_empty() {
            let empty = adw::ActionRow::builder()
                .title("No subscriptions")
                .subtitle("Use + to add a subscription or import a server")
                .activatable(false)
                .build();
            list.add(&empty);
        }

        for subscription in subscriptions {
            if subscription.id == LOCAL_ID {
                self.add_local_servers_row(&list, subscription, callbacks.clone());
            } else {
                self.add_subscription_row(&list, subscription, callbacks.clone());
            }
        }

        self.content.append(&list);
        self.apply_operation();
    }

    /// Updates row-level progress without requiring a page rebuild.
    pub fn set_operation(&self, operation: Option<UiOperation>) {
        *self.operation.borrow_mut() = operation;
        self.apply_operation();
    }

    fn add_subscription_row(
        &self,
        list: &adw::PreferencesGroup,
        subscription: &Subscription,
        callbacks: SubscriptionCallbacks,
    ) {
        let row = adw::ActionRow::builder()
            .title(&subscription.name)
            .subtitle(subscription_description(subscription))
            .title_lines(1)
            .subtitle_lines(2)
            .activatable(true)
            .build();
        let spinner = gtk::Spinner::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let chevron = gtk::Image::from_icon_name("go-next-symbolic");
        chevron.set_accessible_role(gtk::AccessibleRole::Presentation);
        row.add_suffix(&spinner);
        row.add_suffix(&chevron);

        let snapshot = subscription.clone();
        row.connect_activated(move |row| {
            show_subscription_details(row, snapshot.clone(), callbacks.clone());
        });
        self.operation_widgets.borrow_mut().subscriptions.insert(
            subscription.id.clone(),
            RowControls {
                row: row.clone(),
                spinner,
            },
        );
        list.add(&row);
    }

    fn add_local_servers_row(
        &self,
        list: &adw::PreferencesGroup,
        subscription: &Subscription,
        callbacks: SubscriptionCallbacks,
    ) {
        let count = subscription.servers.len();
        let row = adw::ActionRow::builder()
            .title("Local servers")
            .subtitle(format!(
                "{count} imported {}",
                if count == 1 { "server" } else { "servers" }
            ))
            .activatable(true)
            .build();
        let spinner = gtk::Spinner::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let chevron = gtk::Image::from_icon_name("go-next-symbolic");
        chevron.set_accessible_role(gtk::AccessibleRole::Presentation);
        row.add_suffix(&spinner);
        row.add_suffix(&chevron);

        let snapshot = subscription.clone();
        row.connect_activated(move |row| {
            show_local_servers(row, snapshot.clone(), callbacks.clone());
        });
        self.operation_widgets.borrow_mut().local_servers = Some(RowControls {
            row: row.clone(),
            spinner,
        });
        list.add(&row);
    }

    fn apply_operation(&self) {
        let operation = self.operation.borrow().clone();

        self.header.add_subscription.set_sensitive(!matches!(
            operation.as_ref().map(|operation| operation.kind),
            Some(UiOperationKind::AddSubscription)
        ));
        self.header.import_server.set_sensitive(!matches!(
            operation.as_ref().map(|operation| operation.kind),
            Some(UiOperationKind::ImportServers)
        ));

        let updating_all = matches!(
            operation.as_ref().map(|operation| operation.kind),
            Some(UiOperationKind::UpdateAllSubscriptions)
        );
        self.header.update_all.set_sensitive(!updating_all);
        self.header.update_spinner.set_spinning(updating_all);
        self.header.update_spinner.set_visible(updating_all);

        let widgets = self.operation_widgets.borrow();
        for (id, controls) in &widgets.subscriptions {
            let affected = operation
                .as_ref()
                .and_then(|operation| operation.subscription_id.as_deref())
                .is_some_and(|target| target == id);
            controls.row.set_sensitive(!affected);
            controls.spinner.set_spinning(affected);
            controls.spinner.set_visible(affected);
        }
        if let Some(controls) = widgets.local_servers.as_ref() {
            let affected = matches!(
                operation.as_ref().map(|operation| operation.kind),
                Some(UiOperationKind::ImportServers | UiOperationKind::DeleteServer)
            );
            controls.row.set_sensitive(!affected);
            controls.spinner.set_spinning(affected);
            controls.spinner.set_visible(affected);
        }
    }

    pub fn set_ultra_compact(&self, enabled: bool) {
        self.content.set_spacing(if enabled { 16 } else { 22 });
        self.header.update_label.set_visible(!enabled);
        if enabled {
            self.header.update_all.add_css_class("header-icon-button");
        } else {
            self.header
                .update_all
                .remove_css_class("header-icon-button");
        }
    }
}

fn make_header_controls(
    root: &gtk::ScrolledWindow,
    callbacks: Rc<RefCell<Option<SubscriptionCallbacks>>>,
) -> HeaderControls {
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_valign(gtk::Align::Center);

    let add_menu = gtk::MenuButton::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add subscription or server")
        .focusable(true)
        .build();
    add_menu.add_css_class("flat");
    add_menu.add_css_class("header-icon-button");
    add_menu.update_property(&[gtk::accessible::Property::Label(
        "Add subscription or server",
    )]);

    let menu_content = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu_content.set_margin_top(6);
    menu_content.set_margin_bottom(6);
    menu_content.set_margin_start(6);
    menu_content.set_margin_end(6);
    let add_subscription = gtk::Button::builder()
        .label("Add subscription")
        .halign(gtk::Align::Fill)
        .focusable(true)
        .build();
    add_subscription.add_css_class("flat");
    let import_server = gtk::Button::builder()
        .label("Import server")
        .halign(gtk::Align::Fill)
        .focusable(true)
        .build();
    import_server.add_css_class("flat");
    menu_content.append(&add_subscription);
    menu_content.append(&import_server);
    let popover = gtk::Popover::builder().child(&menu_content).build();
    add_menu.set_popover(Some(&popover));

    let update_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let update_spinner = gtk::Spinner::builder().visible(false).build();
    let update_icon = gtk::Image::from_icon_name("view-refresh-symbolic");
    let update_label = gtk::Label::new(Some("Update all"));
    update_content.append(&update_spinner);
    update_content.append(&update_icon);
    update_content.append(&update_label);
    let update_all = gtk::Button::builder()
        .child(&update_content)
        .tooltip_text("Update all subscriptions")
        .focusable(true)
        .build();
    update_all.add_css_class("flat");
    update_all.update_property(&[gtk::accessible::Property::Label("Update all subscriptions")]);

    let root_weak = root.downgrade();
    let callbacks_for_add = callbacks.clone();
    let popover_for_add = popover.clone();
    add_subscription.connect_clicked(move |_| {
        popover_for_add.popdown();
        let Some(root) = root_weak.upgrade() else {
            return;
        };
        let Some(callbacks) = callbacks_for_add.borrow().clone() else {
            return;
        };
        show_add_subscription(&root, callbacks);
    });

    let root_weak = root.downgrade();
    let callbacks_for_import = callbacks.clone();
    let popover_for_import = popover.clone();
    import_server.connect_clicked(move |_| {
        popover_for_import.popdown();
        let Some(root) = root_weak.upgrade() else {
            return;
        };
        let Some(callbacks) = callbacks_for_import.borrow().clone() else {
            return;
        };
        show_import_servers(&root, callbacks);
    });

    update_all.connect_clicked(move |_| {
        if let Some(callbacks) = callbacks.borrow().as_ref() {
            (callbacks.refresh_all)();
        }
    });

    actions.append(&add_menu);
    actions.append(&update_all);
    HeaderControls {
        root: actions,
        add_subscription,
        import_server,
        update_all,
        update_label,
        update_spinner,
    }
}

fn show_add_subscription(parent: &impl IsA<gtk::Widget>, callbacks: SubscriptionCallbacks) {
    let window = adw::Window::builder()
        .title("Add Subscription")
        .modal(true)
        .default_width(480)
        .default_height(390)
        .build();
    set_transient_parent(&window, parent);

    let header = adw::HeaderBar::new();
    let cancel = gtk::Button::with_label("Cancel");
    let add = gtk::Button::with_label("Add");
    add.add_css_class("suggested-action");
    add.set_sensitive(false);
    header.pack_start(&cancel);
    header.pack_end(&add);

    let group = adw::PreferencesGroup::builder()
        .title("Subscription")
        .description("Device identification remains off unless you enable it.")
        .build();
    let url_entry = adw::EntryRow::builder()
        .title("Subscription URL")
        .activates_default(true)
        .build();
    let name_entry = adw::EntryRow::builder().title("Name (optional)").build();
    let send_hwid = adw::SwitchRow::builder()
        .title("Send HWID")
        .subtitle("Share this install's random identifier only with this provider")
        .subtitle_lines(2)
        .active(false)
        .build();
    group.add(&url_entry);
    group.add(&name_entry);
    group.add(&send_hwid);

    let validation = validation_label();
    let content = dialog_content(&group, &validation);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&header);
    page.append(&content);
    window.set_content(Some(&page));
    window.set_default_widget(Some(&add));

    let validation_for_change = validation.clone();
    let add_for_change = add.clone();
    url_entry.connect_changed(move |entry| {
        let value = entry.text();
        let valid = is_valid_subscription_url(value.trim());
        add_for_change.set_sensitive(valid);
        if value.trim().is_empty() || valid {
            set_validation(&validation_for_change, None);
        } else {
            set_validation(
                &validation_for_change,
                Some("Enter a complete HTTP or HTTPS URL."),
            );
        }
    });

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    let window_for_add = window.clone();
    add.connect_clicked(move |_| {
        let url = url_entry.text().trim().to_string();
        if !is_valid_subscription_url(&url) {
            return;
        }
        let name = name_entry.text().trim().to_string();
        (callbacks.add)(
            url,
            (!name.is_empty()).then_some(name),
            send_hwid.is_active(),
        );
        window_for_add.close();
    });
    window.present();
}

fn show_import_servers(parent: &impl IsA<gtk::Widget>, callbacks: SubscriptionCallbacks) {
    let window = adw::Window::builder()
        .title("Import Server")
        .modal(true)
        .default_width(520)
        .default_height(430)
        .build();
    set_transient_parent(&window, parent);

    let header = adw::HeaderBar::new();
    let cancel = gtk::Button::with_label("Cancel");
    let import = gtk::Button::with_label("Import");
    import.add_css_class("suggested-action");
    import.set_sensitive(false);
    header.pack_start(&cancel);
    header.pack_end(&import);

    let group = adw::PreferencesGroup::builder()
        .title("Share links")
        .description(format!(
            "Paste one {} link per line.",
            link::supported_scheme_list()
        ))
        .build();
    let buffer = gtk::TextBuffer::new(None);
    let editor = gtk::TextView::builder()
        .buffer(&buffer)
        .monospace(true)
        .top_margin(10)
        .bottom_margin(10)
        .left_margin(12)
        .right_margin(12)
        .wrap_mode(gtk::WrapMode::Char)
        .build();
    editor.update_property(&[gtk::accessible::Property::Label("Server share links")]);
    let editor_scroller = gtk::ScrolledWindow::builder()
        .min_content_height(130)
        .max_content_height(240)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&editor)
        .build();
    let frame = gtk::Frame::builder()
        .child(&editor_scroller)
        .css_classes(["card"])
        .build();
    group.add(&frame);

    let validation = validation_label();
    let content = dialog_content(&group, &validation);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&header);
    page.append(&content);
    window.set_content(Some(&page));
    window.set_default_widget(Some(&import));

    let import_for_change = import.clone();
    let validation_for_change = validation.clone();
    buffer.connect_changed(move |buffer| {
        let (start, end) = buffer.bounds();
        let text = buffer.text(&start, &end, false);
        let issue = validate_share_links(&text);
        import_for_change.set_sensitive(!text.trim().is_empty() && issue.is_none());
        set_validation(&validation_for_change, issue);
    });

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    let window_for_import = window.clone();
    import.connect_clicked(move |_| {
        let (start, end) = buffer.bounds();
        let text = buffer.text(&start, &end, false).to_string();
        if text.trim().is_empty() || validate_share_links(&text).is_some() {
            return;
        }
        (callbacks.import)(text);
        window_for_import.close();
    });
    window.present();
}

fn show_subscription_details(
    parent: &impl IsA<gtk::Widget>,
    subscription: Subscription,
    callbacks: SubscriptionCallbacks,
) {
    let window = adw::Window::builder()
        .title(&subscription.name)
        .modal(true)
        .default_width(560)
        .default_height(560)
        .build();
    set_transient_parent(&window, parent);

    let header = adw::HeaderBar::new();
    let close = gtk::Button::with_label("Close");
    header.pack_start(&close);
    let update = gtk::Button::with_label("Update");
    update.add_css_class("suggested-action");
    header.pack_end(&update);

    let details = adw::PreferencesGroup::builder().title("Details").build();
    let url_row = adw::ActionRow::builder()
        .title("URL")
        .subtitle(&subscription.url)
        .subtitle_selectable(true)
        .subtitle_lines(3)
        .build();
    let copy = icon_button("edit-copy-symbolic", "Copy subscription URL");
    let url_for_copy = subscription.url.clone();
    copy.connect_clicked(move |button| {
        button.display().clipboard().set_text(&url_for_copy);
    });
    url_row.add_suffix(&copy);
    details.add(&url_row);

    let quota = adw::ActionRow::builder()
        .title("Quota")
        .subtitle(format_quota(subscription.userinfo.as_ref()))
        .build();
    details.add(&quota);
    let expiry = adw::ActionRow::builder()
        .title("Expiry")
        .subtitle(
            subscription
                .userinfo
                .as_ref()
                .and_then(|info| info.expire)
                .map(format_timestamp)
                .unwrap_or_else(|| "Not provided".to_string()),
        )
        .build();
    details.add(&expiry);
    let updated = adw::ActionRow::builder()
        .title("Last updated")
        .subtitle(
            subscription
                .updated_at
                .map(format_timestamp)
                .unwrap_or_else(|| "Never".to_string()),
        )
        .build();
    details.add(&updated);
    let server_count = adw::ActionRow::builder()
        .title("Servers")
        .subtitle(subscription.servers.len().to_string())
        .build();
    details.add(&server_count);

    // The User-Agent belongs next to the subscription and not only in Settings,
    // because it selects the *format* the panel answers with, and providers
    // disagree about which client gets what. One that returns a bundled Xray
    // profile per country to `v2rayNG` and a plain share-link list to everything
    // else needs a different value than the rest of your subscriptions — and the
    // global preset can only be right for one of them.
    let fetching = adw::PreferencesGroup::builder()
        .title("Fetching")
        .description(
            "Panels usually pick the response format from the User-Agent. \
             Leave this empty to use the global preset from Settings › Advanced. \
             Changing it takes effect on the next update.",
        )
        .build();
    let user_agent = adw::EntryRow::builder()
        .title("User-Agent override")
        .text(subscription.user_agent.clone().unwrap_or_default())
        .show_apply_button(true)
        .build();
    let ua_id = subscription.id.clone();
    let ua_callback = callbacks.user_agent.clone();
    user_agent.connect_apply(move |row| {
        ua_callback(ua_id.clone(), row.text().to_string());
    });
    fetching.add(&user_agent);

    let privacy = adw::PreferencesGroup::builder()
        .title("Privacy")
        .description("HWID is never sent unless this switch is enabled.")
        .build();
    let send_hwid = adw::SwitchRow::builder()
        .title("Send HWID")
        .subtitle("Share this install's random identifier with this provider")
        .subtitle_lines(2)
        .active(subscription.send_hwid)
        .build();
    let hwid_id = subscription.id.clone();
    let hwid_callback = callbacks.hwid.clone();
    send_hwid.connect_active_notify(move |row| {
        hwid_callback(hwid_id.clone(), row.is_active());
    });
    privacy.add(&send_hwid);

    let danger = adw::PreferencesGroup::builder().title("Remove").build();
    let delete = gtk::Button::with_label("Delete Subscription");
    delete.set_halign(gtk::Align::Start);
    delete.add_css_class("destructive-action");
    danger.add(&delete);

    let groups = gtk::Box::new(gtk::Orientation::Vertical, 24);
    groups.append(&details);
    groups.append(&fetching);
    groups.append(&privacy);
    groups.append(&danger);
    let content = scrollable_dialog_content(&groups);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&header);
    page.append(&content);
    window.set_content(Some(&page));

    let window_for_close = window.clone();
    close.connect_clicked(move |_| window_for_close.close());
    let refresh_id = subscription.id.clone();
    let refresh = callbacks.refresh.clone();
    let window_for_update = window.clone();
    update.connect_clicked(move |_| {
        refresh(refresh_id.clone());
        window_for_update.close();
    });

    let remove_id = subscription.id.clone();
    let remove_name = subscription.name.clone();
    let remove = callbacks.remove.clone();
    let holding_any = callbacks.groups_holding_any.clone();
    let details_window = window.clone();
    delete.connect_clicked(move |_| {
        let affected = holding_any(remove_id.clone());
        let dialog = adw::AlertDialog::new(
            Some("Delete subscription?"),
            Some(&format!(
                "“{remove_name}” and all of its servers will be removed.{}",
                groups_losing_servers(&affected)
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let remove = remove.clone();
        let remove_id = remove_id.clone();
        let closing = details_window.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "delete" {
                remove(remove_id.clone());
                closing.close();
            }
        });
        // An `AdwDialog` is presented *into* a widget rather than parented to a
        // window at construction, so the window is still needed after the
        // handler has taken its own reference.
        dialog.present(Some(&details_window));
    });
    window.present();
}

fn show_local_servers(
    parent: &impl IsA<gtk::Widget>,
    subscription: Subscription,
    callbacks: SubscriptionCallbacks,
) {
    let window = adw::Window::builder()
        .title("Local Servers")
        .modal(true)
        .default_width(540)
        .default_height(500)
        .build();
    set_transient_parent(&window, parent);

    let header = adw::HeaderBar::new();
    let close = gtk::Button::with_label("Close");
    header.pack_start(&close);

    let servers = adw::PreferencesGroup::builder()
        .title("Imported servers")
        .description("These entries are stored locally and do not update automatically.")
        .build();
    if subscription.servers.is_empty() {
        let empty = adw::ActionRow::builder()
            .title("No local servers")
            .activatable(false)
            .build();
        servers.add(&empty);
    }
    for server in subscription.servers {
        let row = adw::ActionRow::builder()
            .title(&server.name)
            .subtitle(format!(
                "{}:{} · {}",
                server.address,
                server.port,
                server.protocol.as_str()
            ))
            .title_lines(1)
            .subtitle_lines(2)
            .build();
        let remove = icon_button("user-trash-symbolic", "Remove server");
        remove.add_css_class("destructive-action");
        let server_id = server.id;
        let server_name = server.name.clone();
        let callback = callbacks.remove_server.clone();
        let holding = callbacks.groups_holding.clone();
        let window_for_remove = window.clone();
        remove.connect_clicked(move |_| {
            // Deleting is irreversible (there is no undo), so mirror the
            // subscription-delete confirmation.
            let dialog = adw::AlertDialog::new(
                Some("Remove server?"),
                Some(&format!(
                    "“{server_name}” will be removed permanently.{}",
                    also_in_groups(&holding(server_id.clone()))
                )),
            );
            dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
            dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            dialog.connect_response(None, {
                let callback = callback.clone();
                let server_id = server_id.clone();
                let window_for_remove = window_for_remove.clone();
                move |dialog, response| {
                    dialog.close();
                    if response == "remove" {
                        callback(server_id.clone());
                        window_for_remove.close();
                    }
                }
            });
            dialog.present(Some(&window_for_remove));
        });
        row.add_suffix(&remove);
        servers.add(&row);
    }

    let content = scrollable_dialog_content(&servers);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&header);
    page.append(&content);
    window.set_content(Some(&page));
    let window_for_close = window.clone();
    close.connect_clicked(move |_| window_for_close.close());
    window.present();
}

fn scrollable_dialog_content(content: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let clamp = adw::Clamp::builder()
        .maximum_size(640)
        .tightening_threshold(500)
        .child(content)
        .build();
    clamp.set_margin_top(24);
    clamp.set_margin_bottom(24);
    clamp.set_margin_start(24);
    clamp.set_margin_end(24);
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build()
}

/// Whether the Add button may light up for this URL.
///
/// `https`, plus plaintext to loopback for a locally hosted panel — the same
/// rule the daemon applies in `subscription::require_https`. The daemon refuses
/// plaintext regardless, since D-Bus clients never reach this function, but
/// keeping the two in step turns a doomed fetch into a button that never enables.
fn is_valid_subscription_url(value: &str) -> bool {
    let Ok(parsed) = url::Url::parse(value) else {
        return false;
    };
    let loopback = match parsed.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => return false,
    };
    parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback)
}

/// Refuse a paste the importer would silently drop lines from. Ask the real
/// parser rather than matching prefixes: a hand-kept list drifts from what
/// `link::parse_link` accepts, and this one already rejected `socks5://` links
/// and anything whose scheme was not spelled in lower case — both of which
/// import perfectly well.
fn validate_share_links(text: &str) -> Option<&'static str> {
    // Validate by parsing rather than by scheme prefix: a `vless://` with no
    // uuid uses a supported scheme but is still not an importable server.
    let lines = text.lines().filter(|line| !line.trim().is_empty()).count();
    let (parsed, _) = link::parse_links_reporting(text);
    if parsed.len() == lines {
        None
    } else {
        Some("One or more lines is not a supported server share link.")
    }
}

fn format_quota(info: Option<&UserInfo>) -> String {
    let Some(info) = info else {
        return "Not provided".to_string();
    };
    let used = info.upload.saturating_add(info.download);
    if info.total > 0 {
        format!(
            "{} used of {}",
            format_bytes(used),
            format_bytes(info.total)
        )
    } else if used > 0 {
        format!("{} used", format_bytes(used))
    } else {
        "Not provided".to_string()
    }
}

fn format_timestamp(timestamp: i64) -> String {
    gtk::glib::DateTime::from_unix_local(timestamp)
        .and_then(|value| value.format("%c"))
        .map(|value| value.to_string())
        .unwrap_or_else(|_| timestamp.to_string())
}

#[cfg(test)]
mod tests {
    use super::{also_in_groups, groups_losing_servers};

    #[test]
    fn a_deletion_says_which_groups_would_lose_the_server() {
        // Nothing to say is said as nothing, not as an empty clause dangling
        // off the end of the sentence.
        assert_eq!(also_in_groups(&[]), "");
        assert_eq!(
            also_in_groups(&["Favourites".to_string()]),
            " It will also leave the group “Favourites”."
        );
        assert_eq!(
            also_in_groups(&["Favourites".to_string(), "Germany".to_string()]),
            " It will also leave the groups “Favourites”, “Germany”."
        );
    }

    #[test]
    fn deleting_a_subscription_counts_instead_of_naming_every_server() {
        assert_eq!(groups_losing_servers(&[]), "");
        assert_eq!(
            groups_losing_servers(&[("Europe".to_string(), 8), ("Favourites".to_string(), 1)]),
            " Servers in your groups will go with them: 8 in “Europe”, 1 in “Favourites”."
        );
    }
}
