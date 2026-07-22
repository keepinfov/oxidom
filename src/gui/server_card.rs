use std::rc::Rc;

use adw::prelude::*;

use crate::model::Server;

#[derive(Clone)]
pub struct ServerCard {
    pub root: gtk::Box,
    latency: gtk::Label,
    detail_latency: gtk::Label,
    revealer: gtk::Revealer,
}

impl ServerCard {
    pub fn new(
        server: &Server,
        connected: bool,
        selected: bool,
        on_select: impl Fn() + 'static,
        on_connect: impl Fn() + 'static,
        on_ping: impl Fn() + 'static,
    ) -> Self {
        let on_connect = Rc::new(on_connect);

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
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
        labels.set_hexpand(true);
        labels.append(&name);
        labels.append(&subtitle);

        let latency = gtk::Label::builder()
            .css_classes(["latency-badge"])
            .visible(false)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        content.set_margin_top(10);
        content.set_margin_bottom(10);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.append(&flag);
        content.append(&labels);
        content.append(&latency);

        // Single click selects/expands; the header stays a Button for hover +
        // keyboard activation, but connection is deliberately kept off it.
        let header = gtk::Button::builder()
            .child(&content)
            .css_classes(["server-card-header"])
            .build();
        header.connect_clicked(move |_| on_select());

        // Double click connects. Capture phase so we see the press before the
        // button's own gesture consumes it.
        let double = gtk::GestureClick::new();
        double.set_button(gtk::gdk::BUTTON_PRIMARY);
        double.set_propagation_phase(gtk::PropagationPhase::Capture);
        double.connect_pressed({
            let on_connect = on_connect.clone();
            move |_, n_press, _, _| {
                if n_press == 2 {
                    on_connect();
                }
            }
        });
        header.add_controller(double);

        // Expanded detail: address, per-server latency, and explicit actions.
        let meta = gtk::Label::builder()
            .label(format!(
                "{}  ·  {}:{}",
                server.protocol.as_str(),
                server.address,
                server.port
            ))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["dim-label", "server-meta"])
            .build();
        let detail_latency = gtk::Label::builder()
            .label("Not measured")
            .xalign(0.0)
            .css_classes(["dim-label", "server-meta"])
            .build();
        let connect_button = gtk::Button::builder()
            .label("Connect")
            .css_classes(["suggested-action"])
            .build();
        connect_button.connect_clicked({
            let on_connect = on_connect.clone();
            move |_| on_connect()
        });
        let ping_button = gtk::Button::builder()
            .icon_name("emblem-synchronizing-symbolic")
            .tooltip_text("Test latency")
            .css_classes(["flat"])
            .build();
        ping_button.connect_clicked(move |_| on_ping());
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.append(&connect_button);
        actions.append(&ping_button);
        let actions_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        actions_spacer.set_hexpand(true);
        actions.append(&actions_spacer);
        actions.append(&detail_latency);

        let detail = gtk::Box::new(gtk::Orientation::Vertical, 8);
        detail.set_css_classes(&["server-card-detail"]);
        detail.append(&meta);
        detail.append(&actions);
        let revealer = gtk::Revealer::builder()
            .child(&detail)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(selected)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.add_css_class("server-card");
        root.append(&header);
        root.append(&revealer);
        if connected {
            root.add_css_class("active-server");
        }
        if selected {
            root.add_css_class("selected-server");
        }

        let card = Self { root, latency, detail_latency, revealer };
        card.set_latency(server.latency_ms);
        card
    }

    pub fn set_latency(&self, latency_ms: Option<u32>) {
        self.latency.remove_css_class("latency-good");
        self.latency.remove_css_class("latency-slow");
        match latency_ms {
            Some(ms) => {
                let text = format!("{ms} ms");
                self.latency.set_label(&text);
                self.latency.add_css_class(if ms < 300 {
                    "latency-good"
                } else {
                    "latency-slow"
                });
                self.latency.set_visible(true);
                self.detail_latency.set_label(&format!("Latency: {ms} ms"));
            }
            None => {
                self.latency.set_visible(false);
                self.detail_latency.set_label("Not measured");
            }
        }
    }

    pub fn set_active(&self, active: bool) {
        if active {
            self.root.add_css_class("active-server");
        } else {
            self.root.remove_css_class("active-server");
        }
    }

    pub fn set_selected(&self, selected: bool) {
        self.revealer.set_reveal_child(selected);
        if selected {
            self.root.add_css_class("selected-server");
        } else {
            self.root.remove_css_class("selected-server");
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
