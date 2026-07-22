use adw::prelude::*;

use crate::model::Server;

#[derive(Clone)]
pub struct ServerCard {
    pub button: gtk::Button,
    latency: gtk::Label,
}

impl ServerCard {
    pub fn new(server: &Server, active: bool, on_activate: impl Fn() + 'static) -> Self {
        let flag = match server.country.as_deref().and_then(country_flag) {
            Some(flag) => gtk::Label::builder()
                .label(flag)
                .css_classes(["server-flag"])
                .build()
                .upcast::<gtk::Widget>(),
            None => gtk::Image::builder()
                .icon_name("web-browser-symbolic")
                .css_classes(["server-globe"])
                .build()
                .upcast::<gtk::Widget>(),
        };

        let name = gtk::Label::builder()
            .label(&server.name)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(0.0)
            .css_classes(["server-name"])
            .build();
        let subtitle = gtk::Label::builder()
            .label(&server.transport_label)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(0.0)
            .css_classes(["dim-label", "server-subtitle"])
            .build();
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 3);
        labels.set_hexpand(true);
        labels.append(&name);
        labels.append(&subtitle);

        let latency = gtk::Label::builder()
            .css_classes(["latency-badge"])
            .visible(false)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        content.set_margin_top(14);
        content.set_margin_bottom(14);
        content.set_margin_start(14);
        content.set_margin_end(14);
        content.append(&flag);
        content.append(&labels);
        content.append(&latency);

        let button = gtk::Button::builder()
            .child(&content)
            .hexpand(true)
            .css_classes(["server-card"])
            .build();
        if active {
            button.add_css_class("active-server");
        }
        button.connect_clicked(move |_| on_activate());

        let card = Self { button, latency };
        card.set_latency(server.latency_ms);
        card
    }

    pub fn set_latency(&self, latency_ms: Option<u32>) {
        self.latency.remove_css_class("latency-good");
        self.latency.remove_css_class("latency-slow");
        match latency_ms {
            Some(ms) => {
                self.latency.set_label(&format!("{ms} ms"));
                self.latency.add_css_class(if ms < 300 {
                    "latency-good"
                } else {
                    "latency-slow"
                });
                self.latency.set_visible(true);
            }
            None => self.latency.set_visible(false),
        }
    }

    pub fn set_active(&self, active: bool) {
        if active {
            self.button.add_css_class("active-server");
        } else {
            self.button.remove_css_class("active-server");
        }
    }
}

fn country_flag(country: &str) -> Option<String> {
    let code = country.trim().to_ascii_uppercase();
    let bytes = code.as_bytes();
    if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_alphabetic) {
        return None;
    }
    let first = char::from_u32(0x1f1e6 + u32::from(bytes[0] - b'A'))?;
    let second = char::from_u32(0x1f1e6 + u32::from(bytes[1] - b'A'))?;
    Some(format!("{first}{second}"))
}
