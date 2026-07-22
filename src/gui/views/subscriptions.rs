use std::rc::Rc;

use adw::prelude::*;

use crate::engine::LOCAL_ID;
use crate::model::Subscription;

use super::super::group::subscription_description;

/// Callbacks the subscriptions view invokes.
#[derive(Clone)]
pub struct SubscriptionCallbacks {
    pub add: Rc<dyn Fn(String, Option<String>)>,
    pub import: Rc<dyn Fn(String)>,
    pub refresh: Rc<dyn Fn(String)>,
    pub remove: Rc<dyn Fn(String)>,
    pub remove_server: Rc<dyn Fn(String)>,
    pub hwid: Rc<dyn Fn(String, bool)>,
}

#[derive(Clone)]
pub struct SubscriptionsView {
    pub root: gtk::ScrolledWindow,
    content: gtk::Box,
}

impl SubscriptionsView {
    pub fn new() -> Self {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 22);
        content.set_margin_top(24);
        content.set_margin_bottom(32);
        content.set_margin_start(28);
        content.set_margin_end(28);
        let root = gtk::ScrolledWindow::builder()
            .child(&content)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        Self { root, content }
    }

    pub fn rebuild(&self, subscriptions: &[Subscription], callbacks: SubscriptionCallbacks) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }

        let add_group = adw::PreferencesGroup::builder()
            .title("Add subscription")
            .description("Paste a subscription URL. Device identification stays off by default.")
            .build();
        let url = adw::EntryRow::builder().title("Subscription URL").build();
        let name = adw::EntryRow::builder().title("Name (optional)").build();
        let add = gtk::Button::builder()
            .label("Add")
            .halign(gtk::Align::End)
            .css_classes(["suggested-action", "pill"])
            .build();
        add_group.add(&url);
        add_group.add(&name);
        add_group.add(&add);
        let url_for_add = url.clone();
        let name_for_add = name.clone();
        let on_add = callbacks.add.clone();
        add.connect_clicked(move |_| {
            let value = url_for_add.text().trim().to_string();
            if value.is_empty() {
                return;
            }
            let title = name_for_add.text().trim().to_string();
            on_add(value, (!title.is_empty()).then_some(title));
            url_for_add.set_text("");
            name_for_add.set_text("");
        });
        self.content.append(&add_group);

        // Standalone servers: paste one or more share-links, no subscription.
        let server_group = adw::PreferencesGroup::builder()
            .title("Add server")
            .description("Paste share-links (vless://, vmess://, trojan://, ss://), one per line.")
            .build();
        let buffer = gtk::TextBuffer::new(None);
        let editor = gtk::TextView::builder()
            .buffer(&buffer)
            .monospace(true)
            .top_margin(6)
            .bottom_margin(6)
            .left_margin(8)
            .right_margin(8)
            .wrap_mode(gtk::WrapMode::Char)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .min_content_height(76)
            .max_content_height(180)
            .propagate_natural_height(true)
            .child(&editor)
            .build();
        let frame = gtk::Frame::builder()
            .child(&scroller)
            .css_classes(["card"])
            .build();
        let import = gtk::Button::builder()
            .label("Import")
            .halign(gtk::Align::End)
            .css_classes(["suggested-action", "pill"])
            .build();
        server_group.add(&frame);
        server_group.add(&import);
        let on_import = callbacks.import.clone();
        import.connect_clicked(move |_| {
            let (start, end) = buffer.bounds();
            let text = buffer.text(&start, &end, false).to_string();
            if text.trim().is_empty() {
                return;
            }
            on_import(text);
            buffer.set_text("");
        });
        self.content.append(&server_group);

        let list = adw::PreferencesGroup::builder()
            .title("Subscriptions")
            .build();
        for subscription in subscriptions {
            let expander = adw::ExpanderRow::builder()
                .title(&subscription.name)
                .subtitle(subscription_description(subscription))
                .build();

            if subscription.id == LOCAL_ID {
                for server in &subscription.servers {
                    let row = adw::ActionRow::builder()
                        .title(&server.name)
                        .subtitle(format!(
                            "{}:{} · {}",
                            server.address,
                            server.port,
                            server.protocol.as_str()
                        ))
                        .build();
                    let trash = gtk::Button::builder()
                        .icon_name("user-trash-symbolic")
                        .tooltip_text("Remove server")
                        .valign(gtk::Align::Center)
                        .css_classes(["flat"])
                        .build();
                    let id = server.id.clone();
                    let remove_server = callbacks.remove_server.clone();
                    trash.connect_clicked(move |_| remove_server(id.clone()));
                    row.add_suffix(&trash);
                    expander.add_row(&row);
                }
                list.add(&expander);
                continue;
            }

            let privacy = adw::SwitchRow::builder()
                .title("Send HWID")
                .subtitle("Only sends this install's identifier to this subscription when enabled")
                .active(subscription.send_hwid)
                .build();
            let id = subscription.id.clone();
            let callback = callbacks.hwid.clone();
            privacy.connect_active_notify(move |row| callback(id.clone(), row.is_active()));
            expander.add_row(&privacy);

            let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            actions.set_margin_top(8);
            actions.set_margin_bottom(8);
            actions.set_margin_start(12);
            actions.set_margin_end(12);
            actions.set_halign(gtk::Align::End);
            let update = gtk::Button::with_label("Update");
            let delete = gtk::Button::with_label("Delete");
            delete.add_css_class("destructive-action");
            let refresh_id = subscription.id.clone();
            let refresh = callbacks.refresh.clone();
            update.connect_clicked(move |_| refresh(refresh_id.clone()));
            let remove_id = subscription.id.clone();
            let remove = callbacks.remove.clone();
            delete.connect_clicked(move |_| remove(remove_id.clone()));
            actions.append(&update);
            actions.append(&delete);
            expander.add_row(&actions);
            list.add(&expander);
        }
        self.content.append(&list);
    }
}
