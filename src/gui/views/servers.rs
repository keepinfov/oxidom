use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;

use crate::model::Subscription;

use super::super::group::subscription_description;
use super::super::server_card::ServerCard;

#[derive(Clone)]
pub struct ServersView {
    pub root: gtk::ScrolledWindow,
    content: gtk::Box,
    cards: Rc<RefCell<HashMap<String, ServerCard>>>,
    groups: Rc<RefCell<Vec<(gtk::Widget, gtk::FlowBox)>>>,
    query: Rc<RefCell<String>>,
    narrow: Rc<Cell<bool>>,
}

impl ServersView {
    pub fn new() -> Self {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 28);
        content.set_margin_top(24);
        content.set_margin_bottom(32);
        content.set_margin_start(28);
        content.set_margin_end(28);

        let root = gtk::ScrolledWindow::builder()
            .child(&content)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        Self {
            root,
            content,
            cards: Rc::new(RefCell::new(HashMap::new())),
            groups: Rc::new(RefCell::new(Vec::new())),
            query: Rc::new(RefCell::new(String::new())),
            narrow: Rc::new(Cell::new(false)),
        }
    }

    pub fn rebuild(
        &self,
        subscriptions: &[Subscription],
        active_id: Option<&str>,
        latencies: &HashMap<String, Option<u32>>,
        on_activate: Rc<dyn Fn(String)>,
    ) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        self.cards.borrow_mut().clear();
        self.groups.borrow_mut().clear();

        if subscriptions.is_empty() {
            let empty = adw::StatusPage::builder()
                .icon_name("network-server-symbolic")
                .title("No servers yet")
                .description("Add a subscription to start browsing servers.")
                .vexpand(true)
                .build();
            self.content.append(&empty);
            return;
        }

        for subscription in subscriptions {
            let heading = gtk::Label::builder()
                .label(&subscription.name)
                .xalign(0.0)
                .css_classes(["title-2"])
                .build();
            let description = gtk::Label::builder()
                .label(subscription_description(subscription))
                .xalign(0.0)
                .wrap(true)
                .css_classes(["dim-label"])
                .build();
            let header = gtk::Box::new(gtk::Orientation::Vertical, 4);
            header.append(&heading);
            header.append(&description);

            let flow = gtk::FlowBox::builder()
                .column_spacing(12)
                .row_spacing(12)
                .homogeneous(true)
                .selection_mode(gtk::SelectionMode::None)
                .min_children_per_line(1)
                .max_children_per_line(if self.narrow.get() { 1 } else { 3 })
                .build();
            for server in &subscription.servers {
                let id = server.id.clone();
                let callback = on_activate.clone();
                let mut display = server.clone();
                if let Some(latency) = latencies.get(&id) {
                    display.latency_ms = *latency;
                }
                let card = ServerCard::new(&display, active_id == Some(id.as_str()), move || {
                    callback(id.clone());
                });
                card.button.set_tooltip_text(Some(&format!(
                    "{} · {}:{} · {} · {}",
                    server.name,
                    server.address,
                    server.port,
                    server.protocol.as_str(),
                    server.country.as_deref().unwrap_or("")
                )));
                flow.insert(&card.button, -1);
                self.cards.borrow_mut().insert(server.id.clone(), card);
            }

            let group = gtk::Box::new(gtk::Orientation::Vertical, 12);
            group.append(&header);
            group.append(&flow);
            let group_widget = group.clone().upcast::<gtk::Widget>();
            self.content.append(&group);
            self.groups.borrow_mut().push((group_widget, flow));
        }
        self.apply_filter();
    }

    pub fn set_query(&self, query: &str) {
        *self.query.borrow_mut() = query.trim().to_lowercase();
        self.apply_filter();
    }

    fn apply_filter(&self) {
        let query = self.query.borrow().clone();
        for (group, flow) in self.groups.borrow().iter() {
            let mut visible = 0;
            let mut child = flow.first_child();
            while let Some(widget) = child {
                let next = widget.next_sibling();
                if let Ok(flow_child) = widget.clone().downcast::<gtk::FlowBoxChild>() {
                    let matches = flow_child
                        .child()
                        .and_then(|child| child.tooltip_text())
                        .map(|text| text.to_lowercase().contains(&query))
                        .unwrap_or(query.is_empty())
                        || flow_child
                            .child()
                            .and_then(|child| child.downcast::<gtk::Button>().ok())
                            .and_then(|button| button.child())
                            .map(|child| widget_text(&child).to_lowercase().contains(&query))
                            .unwrap_or(false);
                    flow_child.set_visible(matches);
                    if matches {
                        visible += 1;
                    }
                }
                child = next;
            }
            group.set_visible(visible > 0);
        }
    }

    pub fn set_latency(&self, server_id: &str, latency: Option<u32>) {
        if let Some(card) = self.cards.borrow().get(server_id) {
            card.set_latency(latency);
        }
    }

    pub fn set_active(&self, server_id: Option<&str>) {
        for (id, card) in self.cards.borrow().iter() {
            card.set_active(server_id == Some(id.as_str()));
        }
    }

    pub fn set_narrow(&self, narrow: bool) {
        self.narrow.set(narrow);
        for (_, flow) in self.groups.borrow().iter() {
            flow.set_max_children_per_line(if narrow { 1 } else { 3 });
        }
    }
}

fn widget_text(widget: &gtk::Widget) -> String {
    let mut text = String::new();
    if let Ok(label) = widget.clone().downcast::<gtk::Label>() {
        text.push_str(&label.text());
        text.push(' ');
    }
    let mut child = widget.first_child();
    while let Some(item) = child {
        text.push_str(&widget_text(&item));
        child = item.next_sibling();
    }
    text
}
