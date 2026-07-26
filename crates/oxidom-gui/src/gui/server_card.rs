use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use gtk::subclass::prelude::*;

use oxidom_core::config::LatencyMethod;
use oxidom_core::model::Server;

pub const COMPACT_CARD_HEIGHT: i32 = 64;
pub const CARD_MEASURE_WIDTH: i32 = 320;

const COLLAPSE_DURATION_MS: u32 = 120;
const EXPAND_DURATION_MS: u32 = 160;
const DETAIL_FADE_IN_DURATION_MS: u32 = 120;

type Completion = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

struct HeightTransition {
    from: i32,
    to: i32,
    duration: u32,
    easing: adw::Easing,
}

mod card_frame {
    use super::*;

    #[derive(Default)]
    pub struct CardFrame {
        pub child: RefCell<Option<gtk::Widget>>,
        pub height: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CardFrame {
        const NAME: &'static str = "OxidomCardFrame";
        type Type = super::CardFrame;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for CardFrame {
        fn dispose(&self) {
            if let Some(child) = self.child.borrow_mut().take() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for CardFrame {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Vertical {
                let height = self.height.get().max(0);
                return (height, height, -1, -1);
            }
            self.child
                .borrow()
                .as_ref()
                .map_or((0, 0, -1, -1), |child| child.measure(orientation, for_size))
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.child.borrow().as_ref() {
                let (minimum_height, _, _, _) = child.measure(gtk::Orientation::Vertical, width);
                child.allocate(width, height.max(minimum_height), baseline, None);
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if let Some(child) = self.child.borrow().as_ref() {
                self.obj().snapshot_child(child, snapshot);
            }
        }
    }
}

glib::wrapper! {
    pub struct CardFrame(ObjectSubclass<card_frame::CardFrame>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl CardFrame {
    fn new(child: &impl IsA<gtk::Widget>) -> Self {
        let frame: Self = glib::Object::new();
        let child = child.clone().upcast::<gtk::Widget>();
        child.set_parent(&frame);
        frame.imp().child.replace(Some(child));
        frame.set_animated_height(COMPACT_CARD_HEIGHT);
        frame
    }

    fn set_animated_height(&self, height: i32) {
        let height = height.max(0);
        if self.imp().height.replace(height) != height {
            self.queue_resize();
        }
    }
}

/// How old a reading is, in whole minutes.
///
/// Bucketed rather than exact because the age is refreshed by a background
/// sweep: a value that changed every second would repaint — and re-fade — every
/// badge on every pass, for a number nobody reads to the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatencyAge {
    /// No usable timestamp, so nothing can be said about the age.
    Unknown,
    /// Taken within the last minute.
    Fresh,
    /// Whole minutes since the measurement, saturating.
    Stale(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatencyState {
    /// No probe has ever produced a reading for this server.
    Unmeasured,
    /// A reading exists, but it was taken in a context that is gone — a number
    /// measured through the tunnel, for a server that no longer carries it, or
    /// the other way round. Showing it is the same lie as passing a
    /// pre-connect direct ping off as the tunnel's latency.
    Superseded,
    Checking,
    /// Measured straight at the server: a fact about that server.
    Reachable {
        ms: u32,
        age: LatencyAge,
        /// How the number was really taken — not always what the settings
        /// asked for, since a hysteria2 server that refuses TCP is measured by
        /// ICMP. The tooltip says which, instead of letting the badge imply a
        /// measurement nobody made.
        method: LatencyMethod,
    },
    /// Measured through the tunnel: a fact about the connection in use.
    Tunnel {
        ms: u32,
        age: LatencyAge,
        method: LatencyMethod,
    },
    /// A probe ran and the server did not answer.
    Unreachable,
    /// The probe never left this machine, so the server was not tested at all.
    NoNetwork,
}

/// How a reading was taken, for the badge's tooltip.
///
/// Named after what actually happened on the wire rather than after the
/// setting: a card measured with a TCP handshake says "TCP handshake" even
/// when the user picked HTTP GET, because that is what the number is.
fn method_text(method: LatencyMethod) -> &'static str {
    match method {
        LatencyMethod::Icmp => "ICMP ping",
        LatencyMethod::Tcp => "TCP handshake",
        LatencyMethod::HttpHead => "HTTP HEAD",
        LatencyMethod::HttpGet => "HTTP GET",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardConnectionState {
    Disconnected,
    ConnectedHere,
    ConnectedElsewhere,
    Connecting,
    /// This server's connection attempt failed. Distinct from `Disconnected`
    /// because the two look identical otherwise, and a user who clicked
    /// Connect and got the card they started from has no way to tell that
    /// anything happened at all.
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClickPlan {
    Ignore,
    ToggleDetails,
    Activate,
}

#[derive(Clone)]
pub struct ServerCard {
    pub root: CardFrame,
    detail: gtk::Box,
    detail_region: gtk::Box,
    header: gtk::Button,
    latency_display: gtk::Stack,
    latency: gtk::Label,
    latency_spinner: gtk::Spinner,
    /// The badge-shaped box the spinner sits in; carries the pill's background
    /// and its tooltip, neither of which may live on a rotating widget.
    latency_spinner_pill: gtk::Box,
    status: gtk::Label,
    connect_button: gtk::Button,
    expanded: Rc<Cell<bool>>,
    /// What the badge is currently showing. The age sweep re-pushes a state for
    /// every card every 15 s, so without this the whole grid would re-fade on
    /// each pass for the handful of badges that actually changed.
    last_latency: Rc<Cell<LatencyState>>,
    latency_generation: Rc<Cell<u64>>,
    height_generation: Rc<Cell<u64>>,
}

impl ServerCard {
    pub fn new(
        server: &Server,
        connection_state: CardConnectionState,
        latency_state: LatencyState,
        on_select: impl Fn() + 'static,
        on_activate: impl Fn() + 'static,
        on_ping: impl Fn() + 'static,
    ) -> Self {
        let flag = flag_widget(server.country.as_deref(), 26, 20);

        let name = gtk::Label::builder()
            .label(oxidom_core::model::name_without_flag(&server.name))
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(20)
            .xalign(0.0)
            .css_classes(["server-name"])
            .build();
        let subtitle = gtk::Label::builder()
            .label(&server.transport_label)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(20)
            .xalign(0.0)
            .css_classes(["dim-label", "server-subtitle"])
            .build();
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
        labels.set_hexpand(true);
        labels.append(&name);
        labels.append(&subtitle);

        // `Fill` on both axes, here and on the spinner's pill below: the stack
        // is homogeneous, so two children that fill it are drawn at exactly the
        // same size. Anything else lets the badge change shape when a check
        // starts, since a spinner and a line of 0.75em text have nothing like
        // the same natural height.
        let latency = gtk::Label::builder()
            .css_classes(["latency-badge"])
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::Fill)
            .hexpand(true)
            .xalign(0.5)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(8)
            .build();
        // The pill keeps its size and background while a check runs — the
        // spinner appears *inside* it instead of replacing it, so the row stops
        // twitching every time a card is re-checked. The background belongs to
        // the box, never to the spinner: GTK animates a spinner by rotating its
        // whole node, so a badge drawn on the spinner itself spins along with
        // it, and the pill tumbles across the card.
        let latency_spinner = gtk::Spinner::builder()
            .width_request(16)
            .height_request(16)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .css_classes(["latency-spinner"])
            .build();
        let latency_spinner_pill = gtk::Box::builder()
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::Fill)
            .css_classes(["latency-badge"])
            .build();
        latency_spinner_pill.append(&latency_spinner);
        let latency_display = gtk::Stack::builder()
            .width_request(68)
            .halign(gtk::Align::End)
            .valign(gtk::Align::Center)
            .hhomogeneous(true)
            .vhomogeneous(true)
            .css_classes(["latency-display"])
            .build();
        latency_display.add_named(&latency, Some("label"));
        latency_display.add_named(&latency_spinner_pill, Some("spinner"));
        let status = gtk::Label::builder()
            .css_classes(["status-badge"])
            .valign(gtk::Align::Center)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(10)
            .visible(false)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(10);
        content.set_margin_bottom(10);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.append(&flag);
        content.append(&labels);
        content.append(&status);
        content.append(&latency_display);

        let header = gtk::Button::builder()
            .child(&content)
            .height_request(COMPACT_CARD_HEIGHT)
            .css_classes(["server-card-header"])
            .build();

        // A single click inspects (toggles details); connecting stays an
        // explicit action — the Connect button, or a double-click shortcut.
        let on_select: Rc<dyn Fn()> = Rc::new(on_select);
        let on_activate: Rc<dyn Fn()> = Rc::new(on_activate);
        let expanded = Rc::new(Cell::new(false));
        let primary_click = gtk::GestureClick::new();
        primary_click.set_button(gtk::gdk::BUTTON_PRIMARY);
        primary_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        primary_click.connect_pressed({
            let on_activate = on_activate.clone();
            move |_, n_press, _, _| {
                if click_plan_for_press(gtk::gdk::BUTTON_PRIMARY, n_press) == ClickPlan::Activate {
                    on_activate();
                }
            }
        });
        primary_click.connect_released({
            let on_select = on_select.clone();
            move |_, n_press, _, _| {
                if click_plan_for_press(gtk::gdk::BUTTON_PRIMARY, n_press)
                    == ClickPlan::ToggleDetails
                {
                    on_select();
                }
            }
        });
        header.add_controller(primary_click);

        let secondary_click = gtk::GestureClick::new();
        secondary_click.set_button(gtk::gdk::BUTTON_SECONDARY);
        secondary_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        secondary_click.connect_released({
            let on_select = on_select.clone();
            move |_, n_press, _, _| {
                if click_plan_for_press(gtk::gdk::BUTTON_SECONDARY, n_press)
                    == ClickPlan::ToggleDetails
                {
                    on_select();
                }
            }
        });
        header.add_controller(secondary_click);

        let keyboard = gtk::EventControllerKey::new();
        keyboard.connect_key_pressed({
            let on_select = on_select.clone();
            move |_, key, _, _| {
                if matches!(
                    key,
                    gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::space
                ) {
                    on_select();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        });
        header.add_controller(keyboard);

        let full_name = gtk::Label::builder()
            .label(oxidom_core::model::name_without_flag(&server.name))
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(1)
            .selectable(true)
            .css_classes(["server-detail-name"])
            .build();
        let meta = gtk::Label::builder()
            .label(format!(
                "{}  ·  {}:{}",
                server.protocol.as_str(),
                server.address,
                server.port
            ))
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(1)
            .selectable(true)
            .css_classes(["dim-label", "server-meta"])
            .build();
        let connect_button = gtk::Button::builder()
            .label("Connect")
            .halign(gtk::Align::End)
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        connect_button.connect_clicked({
            let on_activate = on_activate.clone();
            move |_| on_activate()
        });
        let copy_button = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text(if server.link.is_some() {
                "Copy share-link"
            } else {
                "Composite profiles cannot be copied as one share-link"
            })
            .valign(gtk::Align::Center)
            .sensitive(server.link.is_some())
            .css_classes(["flat", "server-action"])
            .build();
        copy_button.update_property(&[gtk::accessible::Property::Label("Copy share-link")]);
        if let Some(link) = server.link.clone() {
            copy_button.connect_clicked(move |button| button.clipboard().set_text(&link));
        }
        let ping_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Re-check latency")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "server-action"])
            .build();
        ping_button.update_property(&[gtk::accessible::Property::Label("Re-check latency")]);
        ping_button.connect_clicked(move |_| on_ping());

        let action_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        action_row.set_hexpand(true);
        action_row.append(&copy_button);
        action_row.append(&ping_button);
        let action_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        action_spacer.set_hexpand(true);
        action_row.append(&action_spacer);
        action_row.append(&connect_button);

        let metadata = gtk::Box::new(gtk::Orientation::Vertical, 6);
        metadata.append(&full_name);
        metadata.append(&meta);
        let metadata_scroller = gtk::ScrolledWindow::builder()
            .child(&metadata)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .vexpand(true)
            .build();

        // Long selectable metadata scrolls inside the fixed shell. Actions stay
        // pinned to its bottom, so connect/disconnect is never clipped.
        let detail = gtk::Box::new(gtk::Orientation::Vertical, 6);
        detail.set_css_classes(&["server-card-detail"]);
        detail.set_vexpand(true);
        detail.append(&metadata_scroller);
        detail.append(&action_row);

        let detail_region = gtk::Box::new(gtk::Orientation::Vertical, 0);
        detail_region.append(&detail);
        detail_region.set_vexpand(true);
        detail_region.set_overflow(gtk::Overflow::Hidden);
        detail_region.set_visible(false);
        detail_region.set_opacity(0.0);
        detail_region.set_can_target(false);

        let shell = gtk::Box::new(gtk::Orientation::Vertical, 0);
        shell.set_hexpand(true);
        shell.append(&header);
        shell.append(&detail_region);

        let root = CardFrame::new(&shell);
        root.set_hexpand(true);
        root.set_valign(gtk::Align::Start);
        root.set_overflow(gtk::Overflow::Hidden);
        root.add_css_class("server-card");

        let card = Self {
            root,
            detail,
            detail_region,
            header,
            latency_display,
            latency,
            latency_spinner,
            latency_spinner_pill,
            status,
            connect_button,
            expanded,
            last_latency: Rc::new(Cell::new(latency_state)),
            latency_generation: Rc::new(Cell::new(0)),
            height_generation: Rc::new(Cell::new(0)),
        };
        card.apply_latency(latency_state);
        card.set_connection_state(connection_state);
        card
    }

    pub fn set_latency_state(&self, state: LatencyState) {
        let previous = self.last_latency.replace(state);
        if previous == state {
            return;
        }
        let generation = self.latency_generation.get().wrapping_add(1);
        self.latency_generation.set(generation);

        // The spinner and the number share one slot, so fading across that
        // boundary reads as the pill blinking out and back rather than as a
        // check starting. Only number-to-number is worth a crossfade.
        if state == LatencyState::Checking
            || previous == LatencyState::Checking
            || self.latency.text().is_empty()
            || !adw::is_animations_enabled(&self.latency_display)
        {
            self.latency_display.set_opacity(1.0);
            self.apply_latency(state);
            return;
        }
        let out_target = adw::CallbackAnimationTarget::new({
            let display = self.latency_display.clone();
            move |value| display.set_opacity(value)
        });
        let out = adw::TimedAnimation::new(
            &self.latency_display,
            self.latency_display.opacity(),
            0.0,
            110,
            out_target,
        );
        out.set_easing(adw::Easing::EaseInCubic);
        out.connect_done({
            let card = self.clone();
            move |_| {
                if card.latency_generation.get() != generation {
                    return;
                }
                card.apply_latency(state);
                let in_target = adw::CallbackAnimationTarget::new({
                    let display = card.latency_display.clone();
                    move |value| display.set_opacity(value)
                });
                let animation =
                    adw::TimedAnimation::new(&card.latency_display, 0.0, 1.0, 150, in_target);
                animation.set_easing(adw::Easing::EaseOutCubic);
                animation.play();
            }
        });
        out.play();
    }

    fn apply_latency(&self, state: LatencyState) {
        self.latency_spinner.set_spinning(false);
        for class in [
            "latency-reachable",
            "latency-tunnel",
            "latency-stale",
            "latency-error",
            "latency-offline",
        ] {
            self.latency.remove_css_class(class);
        }
        match state {
            LatencyState::Reachable { ms, age, method } => {
                self.show_reading(
                    &format!("{ms} ms"),
                    &format!("Latency: {ms} ms · {}", method_text(method)),
                    age,
                );
                self.latency.add_css_class("latency-reachable");
            }
            LatencyState::Tunnel { ms, age, method } => {
                self.show_reading(
                    &format!("{ms} ms"),
                    &format!("Through the tunnel: {ms} ms · {}", method_text(method)),
                    age,
                );
                self.latency.add_css_class("latency-tunnel");
            }
            LatencyState::Unmeasured => {
                self.show_label("—", "Latency has not been measured");
            }
            LatencyState::Superseded => {
                self.show_label("—", "Measured in a different context — needs a fresh check");
            }
            LatencyState::Checking => {
                self.latency_display.set_visible_child_name("spinner");
                self.latency_spinner.set_spinning(true);
                self.latency_spinner_pill
                    .set_tooltip_text(Some("Checking server reachability"));
            }
            // Same dash as an unmeasured server, in the error colour: a failed
            // check still leaves no number, and a cross reads as a verdict on
            // the server rather than as the absence of a reading.
            LatencyState::Unreachable => {
                self.show_label("—", "Server is unreachable or did not respond");
                self.latency.add_css_class("latency-error");
            }
            LatencyState::NoNetwork => {
                self.show_label("⊘", "No network — the server was not checked");
                self.latency.add_css_class("latency-offline");
            }
        }
    }

    fn show_label(&self, text: &str, tooltip: &str) {
        self.latency_display.set_visible_child_name("label");
        self.latency.set_label(text);
        self.latency.set_tooltip_text(Some(tooltip));
    }

    /// A number, dimmed and dated once it is a minute old. The number itself
    /// stays readable: it is still the last thing we know, and hiding it would
    /// trade one dishonesty for another.
    fn show_reading(&self, text: &str, tooltip: &str, age: LatencyAge) {
        match age {
            LatencyAge::Stale(minutes) => {
                let unit = if minutes == 1 { "minute" } else { "minutes" };
                self.show_label(text, &format!("{tooltip} · measured {minutes} {unit} ago"));
                self.latency.add_css_class("latency-stale");
            }
            LatencyAge::Fresh | LatencyAge::Unknown => self.show_label(text, tooltip),
        }
    }

    pub fn set_connection_state(&self, state: CardConnectionState) {
        self.connect_button.remove_css_class("suggested-action");
        self.connect_button.remove_css_class("destructive-action");
        self.status.remove_css_class("status-working");
        self.status.remove_css_class("status-error");
        self.root.remove_css_class("failed-server");
        self.connect_button.set_sensitive(true);
        let accessible_status = match state {
            CardConnectionState::Disconnected | CardConnectionState::ConnectedElsewhere => {
                "Disconnected"
            }
            CardConnectionState::ConnectedHere => "Connected",
            CardConnectionState::Connecting => "Connecting",
            CardConnectionState::Failed => "Connection failed",
        };
        self.header
            .update_property(&[gtk::accessible::Property::Description(accessible_status)]);
        match state {
            CardConnectionState::Disconnected => {
                self.status.set_visible(false);
                self.latency_display.set_visible(true);
                self.connect_button.set_label("Connect");
                self.connect_button.add_css_class("suggested-action");
                self.root.remove_css_class("active-server");
            }
            CardConnectionState::ConnectedHere => {
                // The success-coloured card and the global status already show
                // that this is the active tunnel. Keeping a second text pill
                // here only steals the space the server name needs.
                self.status.set_visible(false);
                // Kept visible: this is the one card whose number describes the
                // connection the user is actually on, and hiding it is why the
                // active server was the only one you could not see a ping for.
                self.latency_display.set_visible(true);
                self.connect_button.set_label("Disconnect");
                self.connect_button.add_css_class("destructive-action");
                self.root.add_css_class("active-server");
            }
            CardConnectionState::ConnectedElsewhere => {
                self.status.set_visible(false);
                self.latency_display.set_visible(true);
                self.connect_button.set_label("Switch");
                self.connect_button.add_css_class("suggested-action");
                self.root.remove_css_class("active-server");
            }
            CardConnectionState::Connecting => {
                self.status.set_label("Connecting");
                self.status.add_css_class("status-working");
                self.status.set_visible(true);
                self.latency_display.set_visible(false);
                self.connect_button.set_label("Connecting…");
                self.connect_button.set_sensitive(false);
                self.root.remove_css_class("active-server");
            }
            CardConnectionState::Failed => {
                self.status.set_label("Failed");
                self.status.add_css_class("status-error");
                self.status.set_visible(true);
                // The badge stays hidden here even though it does for no other
                // state: the only number this card can have is a direct one
                // taken before the attempt, and offering it beside "Failed"
                // reads as "the tunnel is fine, 84 ms" — the exact lie.
                self.latency_display.set_visible(false);
                self.connect_button.set_label("Reconnect");
                self.connect_button.add_css_class("suggested-action");
                self.root.remove_css_class("active-server");
                self.root.add_css_class("failed-server");
            }
        }
    }

    pub fn expanded_natural_height(&self, width: i32) -> i32 {
        let (minimum_width, _, _, _) = self.detail.measure(gtk::Orientation::Horizontal, -1);
        let (minimum, natural, _, _) = self
            .detail
            .measure(gtk::Orientation::Vertical, width.max(minimum_width).max(1));
        COMPACT_CARD_HEIGHT.saturating_add(natural.max(minimum).max(0))
    }

    pub fn resize_expanded(&self, target_height: i32) {
        if self.expanded.get() {
            self.height_generation
                .set(self.height_generation.get().wrapping_add(1));
            self.root.set_animated_height(target_height);
        }
    }

    pub fn set_expanded_immediately(&self, target_height: i32) {
        self.height_generation
            .set(self.height_generation.get().wrapping_add(1));
        self.expanded.set(true);
        self.root.set_valign(gtk::Align::Start);
        self.root.set_animated_height(target_height);
        self.root.add_css_class("selected-server");
        self.detail_region.set_visible(true);
        self.detail_region.set_can_target(true);
        self.detail_region.set_opacity(1.0);
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded.get()
    }

    /// Cancel any in-flight height animation and snap straight to the compact
    /// state. Used when the card leaves the layout (e.g. filtered out).
    pub fn collapse_immediately(&self) {
        self.height_generation
            .set(self.height_generation.get().wrapping_add(1));
        self.finish_collapse();
    }

    pub fn expand(&self, target_height: i32, on_done: Option<Box<dyn FnOnce()>>) {
        let generation = self.height_generation.get().wrapping_add(1);
        self.height_generation.set(generation);
        self.expanded.set(true);
        let current_height = self.root.allocated_height().max(COMPACT_CARD_HEIGHT);

        self.root.set_valign(gtk::Align::Start);
        self.root.set_animated_height(current_height);
        self.root.add_css_class("selected-server");
        self.detail_region.set_visible(true);
        self.detail_region.set_can_target(true);

        if !adw::is_animations_enabled(&self.root) {
            self.root.set_animated_height(target_height);
            self.detail_region.set_opacity(1.0);
            if let Some(on_done) = on_done {
                on_done();
            }
            return;
        }
        self.animate_height(
            generation,
            HeightTransition {
                from: current_height,
                to: target_height,
                duration: EXPAND_DURATION_MS,
                easing: adw::Easing::EaseOutCubic,
            },
            on_done,
        );
        self.animate_detail_opacity(
            generation,
            self.detail_region.opacity(),
            1.0,
            DETAIL_FADE_IN_DURATION_MS,
            adw::Easing::EaseOutCubic,
        );
    }

    /// Collapse back to the compact height. `on_shrink` receives the height
    /// delta of every animation frame; the servers view uses it to compensate
    /// the scroll position so the selected card stays pinned on screen while a
    /// card above it shrinks.
    pub fn collapse(&self, on_shrink: Option<Rc<dyn Fn(i32)>>) {
        let generation = self.height_generation.get().wrapping_add(1);
        self.height_generation.set(generation);
        let current_height = self.root.allocated_height().max(COMPACT_CARD_HEIGHT);

        if !adw::is_animations_enabled(&self.root) {
            self.finish_collapse();
            if let Some(on_shrink) = on_shrink {
                on_shrink(current_height - COMPACT_CARD_HEIGHT);
            }
            return;
        }

        let last_height = Rc::new(Cell::new(current_height));
        let target = adw::CallbackAnimationTarget::new({
            let card = self.clone();
            let last_height = last_height.clone();
            let on_shrink = on_shrink.clone();
            move |value| {
                if card.height_generation.get() != generation {
                    return;
                }
                let height = value.round() as i32;
                card.root.set_animated_height(height);
                if let Some(on_shrink) = &on_shrink {
                    let delta = last_height.replace(height) - height;
                    if delta != 0 {
                        on_shrink(delta);
                    }
                }
            }
        });
        let animation = adw::TimedAnimation::new(
            &self.root,
            f64::from(current_height),
            f64::from(COMPACT_CARD_HEIGHT),
            COLLAPSE_DURATION_MS,
            target,
        );
        animation.set_easing(adw::Easing::EaseInCubic);
        animation.connect_done({
            let card = self.clone();
            move |_| {
                if card.height_generation.get() != generation {
                    return;
                }
                card.finish_collapse();
                if let Some(on_shrink) = &on_shrink {
                    let delta = last_height.replace(COMPACT_CARD_HEIGHT) - COMPACT_CARD_HEIGHT;
                    if delta != 0 {
                        on_shrink(delta);
                    }
                }
            }
        });
        animation.play();
    }

    fn finish_collapse(&self) {
        self.root.set_animated_height(COMPACT_CARD_HEIGHT);
        self.root.remove_css_class("selected-server");
        self.detail_region.set_opacity(0.0);
        self.detail_region.set_can_target(false);
        self.detail_region.set_visible(false);
        self.expanded.set(false);
    }

    fn animate_height(
        &self,
        generation: u64,
        transition: HeightTransition,
        on_done: Option<Box<dyn FnOnce()>>,
    ) {
        let HeightTransition {
            from,
            to,
            duration,
            easing,
        } = transition;
        let target = adw::CallbackAnimationTarget::new({
            let card = self.clone();
            move |value| {
                if card.height_generation.get() == generation {
                    let height = value.round() as i32;
                    card.root.set_animated_height(height);
                }
            }
        });
        let animation =
            adw::TimedAnimation::new(&self.root, f64::from(from), f64::from(to), duration, target);
        animation.set_easing(easing);
        if let Some(on_done) = on_done {
            let completion: Completion = Rc::new(RefCell::new(Some(on_done)));
            animation.connect_done({
                let card = self.clone();
                move |_| {
                    if card.height_generation.get() == generation
                        && let Some(on_done) = completion.borrow_mut().take()
                    {
                        on_done();
                    }
                }
            });
        }
        animation.play();
    }

    fn animate_detail_opacity(
        &self,
        generation: u64,
        from: f64,
        to: f64,
        duration: u32,
        easing: adw::Easing,
    ) {
        let target = adw::CallbackAnimationTarget::new({
            let card = self.clone();
            move |value| {
                if card.height_generation.get() == generation {
                    card.detail_region.set_opacity(value);
                }
            }
        });
        let animation = adw::TimedAnimation::new(&self.detail_region, from, to, duration, target);
        animation.set_easing(easing);
        animation.play();
    }
}

fn click_plan_for_press(button: u32, n_press: i32) -> ClickPlan {
    match (button, n_press) {
        (gtk::gdk::BUTTON_PRIMARY, 1) => ClickPlan::ToggleDetails,
        (gtk::gdk::BUTTON_PRIMARY, 2) => ClickPlan::Activate,
        (gtk::gdk::BUTTON_SECONDARY, 1) => ClickPlan::ToggleDetails,
        _ => ClickPlan::Ignore,
    }
}

thread_local! {
    /// Decoded flags, keyed by normalized country code. `rebuild()` recreates
    /// every card, so without this a subscription with hundreds of servers
    /// decodes hundreds of PNGs on the main thread on every refresh. Textures
    /// are immutable and shareable, and the set is bounded by the ~250
    /// embedded flags. Thread-local because GDK types are not `Send`.
    static FLAG_TEXTURES: RefCell<HashMap<String, Option<gtk::gdk::Texture>>> =
        RefCell::new(HashMap::new());
}

fn flag_texture(country: &str) -> Option<gtk::gdk::Texture> {
    let key = country.trim().to_ascii_lowercase();
    FLAG_TEXTURES.with(|cache| {
        cache
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| {
                super::flags::flag_png(country)
                    .and_then(|bytes| gtk::gdk_pixbuf::Pixbuf::from_read(bytes).ok())
                    .map(|pixbuf| gtk::gdk::Texture::for_pixbuf(&pixbuf))
            })
            .clone()
    })
}

/// A square flag icon for the country, falling back to a globe symbol.
pub(crate) fn flag_widget(country: Option<&str>, flag_size: i32, globe_size: i32) -> gtk::Widget {
    let texture = country.and_then(flag_texture);
    match texture {
        Some(texture) => gtk::Image::builder()
            .paintable(&texture)
            .pixel_size(flag_size)
            .css_classes(["server-flag"])
            .build()
            .upcast::<gtk::Widget>(),
        None => gtk::Image::builder()
            .icon_name("web-browser-symbolic")
            .pixel_size(globe_size)
            .css_classes(["server-globe"])
            .build()
            .upcast::<gtk::Widget>(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CardConnectionState, ClickPlan, LatencyAge, LatencyMethod, LatencyState,
        click_plan_for_press,
    };

    #[test]
    fn primary_click_toggles_and_double_click_activates() {
        assert_eq!(
            click_plan_for_press(gtk::gdk::BUTTON_PRIMARY, 1),
            ClickPlan::ToggleDetails
        );
        assert_eq!(
            click_plan_for_press(gtk::gdk::BUTTON_PRIMARY, 2),
            ClickPlan::Activate
        );
        assert_eq!(
            click_plan_for_press(gtk::gdk::BUTTON_SECONDARY, 1),
            ClickPlan::ToggleDetails
        );
        assert_eq!(
            click_plan_for_press(gtk::gdk::BUTTON_PRIMARY, 3),
            ClickPlan::Ignore
        );
    }

    #[test]
    fn connection_states_cover_each_explicit_card_action() {
        assert_ne!(
            CardConnectionState::Disconnected,
            CardConnectionState::ConnectedHere
        );
        assert_ne!(
            CardConnectionState::ConnectedElsewhere,
            CardConnectionState::Connecting
        );
        // The whole point of the state: a server that failed does not look
        // like one the user never touched.
        assert_ne!(
            CardConnectionState::Failed,
            CardConnectionState::Disconnected
        );
    }

    /// The badge has to compare equal for the age sweep's early return to
    /// work, and a reading that got older has to compare *un*equal for the
    /// sweep to be worth running at all.
    #[test]
    fn a_badge_changes_exactly_when_its_reading_or_its_age_does() {
        let fresh = LatencyState::Reachable {
            ms: 41,
            age: LatencyAge::Fresh,
            method: LatencyMethod::Tcp,
        };
        assert_eq!(
            fresh,
            LatencyState::Reachable {
                ms: 41,
                age: LatencyAge::Fresh,
                method: LatencyMethod::Tcp
            }
        );
        assert_ne!(
            fresh,
            LatencyState::Reachable {
                ms: 41,
                age: LatencyAge::Stale(2),
                method: LatencyMethod::Tcp
            }
        );
        // Same number, different thing measured.
        assert_ne!(
            fresh,
            LatencyState::Tunnel {
                ms: 41,
                age: LatencyAge::Fresh,
                method: LatencyMethod::Tcp
            }
        );
        // Same number, measured a different way: the badge reads the same but
        // its tooltip does not, and the sweep must repaint it.
        assert_ne!(
            fresh,
            LatencyState::Reachable {
                ms: 41,
                age: LatencyAge::Fresh,
                method: LatencyMethod::Icmp
            }
        );
    }
}
