use std::rc::Rc;

use adw::prelude::*;

use crate::model::Subscription;

use super::super::group::subscription_description;

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

    pub fn rebuild(
        &self,
        subscriptions: &[Subscription],
        on_add: Rc<dyn Fn(String, Option<String>)>,
        on_refresh: Rc<dyn Fn(String)>,
        on_remove: Rc<dyn Fn(String)>,
        on_hwid: Rc<dyn Fn(String, bool)>,
    ) {
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

        let list = adw::PreferencesGroup::builder()
            .title("Subscriptions")
            .build();
        for subscription in subscriptions {
            let expander = adw::ExpanderRow::builder()
                .title(&subscription.name)
                .subtitle(subscription_description(subscription))
                .build();

            let privacy = adw::SwitchRow::builder()
                .title("Send HWID")
                .subtitle("Only sends this install's identifier to this subscription when enabled")
                .active(subscription.send_hwid)
                .build();
            let id = subscription.id.clone();
            let callback = on_hwid.clone();
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
            let refresh = on_refresh.clone();
            update.connect_clicked(move |_| refresh(refresh_id.clone()));
            let remove_id = subscription.id.clone();
            let remove = on_remove.clone();
            delete.connect_clicked(move |_| remove(remove_id.clone()));
            actions.append(&update);
            actions.append(&delete);
            expander.add_row(&actions);
            list.add(&expander);
        }
        self.content.append(&list);
    }
}
