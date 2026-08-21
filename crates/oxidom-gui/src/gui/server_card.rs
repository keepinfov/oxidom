use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use gtk::subclass::prelude::*;

use oxidom_core::config::LatencyMethod;
use oxidom_core::ipc::ProbeDetail;
use oxidom_core::model::Server;

use super::reduce::{FailureReport, HistoryRow};
use super::views::{
    dialog_content, icon_button, set_transient_parent, set_validation, validation_label,
};

pub const COMPACT_CARD_HEIGHT: i32 = 64;
pub const CARD_MEASURE_WIDTH: i32 = 320;

const COLLAPSE_DURATION_MS: u32 = 120;
const EXPAND_DURATION_MS: u32 = 160;
const DETAIL_FADE_IN_DURATION_MS: u32 = 120;
const ALIAS_ERROR: &str = "Use lowercase letters, digits and '-'; up to 32 characters, and not \
    16 hex digits (that is what a server id looks like).";

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

    /// The height the frame is drawing at right now.
    ///
    /// `allocated_height` lags this by a layout pass while an animation is
    /// running, and a refresh that arrives mid-expansion has to start from
    /// where the card actually stands or the card jumps backwards.
    fn animated_height(&self) -> i32 {
        self.imp().height.get()
    }
}

/// What a re-measure does to a card that is already open.
///
/// Lifted out of the wiring because it is the whole of the defect: the old
/// code had one branch, [`HeightRefresh::Set`], and took it while an expansion
/// was in flight. `Set` bumps the generation that both the height animation and
/// the fade were guarded by, so the card jumped to its full height with its
/// contents still at zero opacity — a measured, empty card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HeightRefresh {
    /// Nothing to do: the card is not open, or it already stands at — or is
    /// already on its way to — that height.
    Ignore,
    /// Write the height out: nothing is moving.
    Set,
    /// Aim the running animation somewhere else, from where it stands. The
    /// expansion continues; only its destination changes.
    Retarget,
}

/// Decide what a re-measure does. `standing` is where the card is, or where it
/// is already heading — not where it is drawn this frame, or a refresh
/// arriving mid-expansion would read the animation's own progress as a change
/// and restart it on every poll.
pub(super) fn height_refresh(
    open: bool,
    animating: bool,
    standing: i32,
    target: i32,
) -> HeightRefresh {
    if !open || standing == target {
        HeightRefresh::Ignore
    } else if animating {
        HeightRefresh::Retarget
    } else {
        HeightRefresh::Set
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
    /// The check could not run here at all — most often no Xray core, which the
    /// default HTTP probe needs to build the tunnel it measures through. Kept
    /// apart from [`LatencyState::Unreachable`] because it says nothing about
    /// the server: reporting a local failure as an unresponsive server sends
    /// people to replace working nodes.
    NotRun(Option<ProbeDetail>),
}

/// How a reading was taken, for the badge's tooltip.
///
/// Named after what actually happened on the wire rather than after the
/// setting: a card measured with a TCP handshake says "TCP handshake" even
/// when the user picked HTTP GET, because that is what the number is.
pub(super) fn method_text(method: LatencyMethod) -> &'static str {
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
    /// The server is one candidate in a running pool, not "the connection".
    InPool,
    ConnectedElsewhere,
    Connecting,
    /// This server's connection attempt failed. Distinct from `Disconnected`
    /// because the two look identical otherwise, and a user who clicked
    /// Connect and got the card they started from has no way to tell that
    /// anything happened at all.
    Failed,
}

/// What a card can ask the page to do. One struct rather than five closure
/// parameters: `ServerCard::new` was already at seven arguments, and the sixth
/// and seventh would have been two more `impl Fn()` that read identically at
/// the call site.
#[derive(Clone)]
pub struct CardHandlers {
    pub select: Rc<dyn Fn()>,
    pub activate: Rc<dyn Fn()>,
    pub ping: Rc<dyn Fn()>,
    /// Look at the server's certificate and decide about it, before anything
    /// has failed. The failure path opens the same dialog on its own; this is
    /// for the person who already knows their server is self-signed.
    pub trust: Rc<dyn Fn()>,
    pub set_alias: Rc<dyn Fn(String)>,
    pub toggle_favourite: Rc<dyn Fn()>,
    /// Open the log page narrowed to this server. Offered only beside a failed
    /// check, because that is the one place where what the core printed is the
    /// next thing anybody wants and the log is where it went.
    pub show_logs: Rc<dyn Fn()>,
    /// Turn what happened to this server into a problem report. Offered in the
    /// same place and for the same reason as `show_logs`: beside a failed
    /// check is where somebody decides they cannot fix this themselves.
    pub report: Rc<dyn Fn()>,
}

/// What a server that stayed silent is called, wherever it is reported.
///
/// The card's badge and the window's sweep toast carried this verbatim in two
/// places. One of two copies is always the one that stops being updated.
pub const UNREACHABLE_TEXT: &str = "Server is unreachable or did not respond";

/// Likewise for this machine having no network at all.
pub const NO_NETWORK_TEXT: &str = "No network connection";

/// What the check button offers, given whether a check is already running.
///
/// Two states, one function, following `collapse_icon` in the servers view. The
/// label is what the context menu shows: `sync_context_labels` falls back to a
/// button's tooltip when it has no text of its own, and this button has none.
fn ping_icon(probing: bool) -> &'static str {
    if probing {
        "media-playback-stop-symbolic"
    } else {
        "view-refresh-symbolic"
    }
}

fn ping_label(probing: bool) -> &'static str {
    if probing {
        "Stop checking latency"
    } else {
        "Re-check latency"
    }
}

fn favourite_icon(favourite: bool) -> &'static str {
    if favourite {
        "starred-symbolic"
    } else {
        "non-starred-symbolic"
    }
}

fn favourite_tooltip(favourite: bool) -> &'static str {
    if favourite {
        "Remove from Favourites"
    } else {
        "Add to Favourites"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClickPlan {
    Ignore,
    ToggleDetails,
    Activate,
    /// The card's actions, as a list. Right-click used to be a third spelling
    /// of "expand", which made it the one pointer gesture that did nothing a
    /// left click did not already do.
    ContextMenu,
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
    /// Held so its icon can say whether pressing it starts a check or stops
    /// one. Built and dropped before this change, which is why the button
    /// looked identical in every state while all the feedback lived in the
    /// badge. The context menu borrows this very button, so relabelling it
    /// relabels the menu entry too — which is the only route to the control on
    /// a collapsed card, where the action row is hidden.
    ping_button: gtk::Button,
    /// The block explaining a check that produced no number, and its two
    /// lines. Hidden whole while there is nothing to explain: an empty row in
    /// the expanded card reads as a defect in the card rather than as a fact
    /// about the server.
    failure: gtk::Box,
    failure_reason: gtk::Label,
    failure_attempt: gtk::Label,
    history: gtk::Box,
    history_list: gtk::Box,
    expanded: Rc<Cell<bool>>,
    /// What the badge is currently showing. The age sweep re-pushes a state for
    /// every card every 15 s, so without this the whole grid would re-fade on
    /// each pass for the handful of badges that actually changed.
    last_latency: Rc<Cell<LatencyState>>,
    last_connection: Rc<Cell<CardConnectionState>>,
    /// Whether the badge is hidden because the number predates a failed connect
    /// attempt. Cleared by the next measurement, not by time — see
    /// [`ServerCard::set_latency_state`].
    latency_predates_failure: Rc<Cell<bool>>,
    latency_generation: Rc<Cell<u64>>,
    height_generation: Rc<Cell<u64>>,
    /// Guards the fade separately from the height. One generation for both
    /// meant that re-measuring the card — which happens on every selection,
    /// because the history arrives a poll later — cancelled the fade that had
    /// just started, and nothing ever wrote the opacity again.
    opacity_generation: Rc<Cell<u64>>,
    /// Whether a height animation is running, so a re-measure can aim it
    /// somewhere else instead of cancelling it.
    height_animating: Rc<Cell<bool>>,
    /// Where the card stands, or where it is heading. Compared against a fresh
    /// measurement to tell a real content change from the animation's own
    /// progress.
    height_target: Rc<Cell<i32>>,
    /// What the failure block and the recent-checks list are drawing. The
    /// re-measure used to be gated on `is_expanded()` alone, so opening a card
    /// re-measured it whether or not anything had changed.
    failure_shown: Rc<RefCell<Option<FailureReport>>>,
    history_shown: Rc<RefCell<Vec<HistoryRow>>>,
}

impl ServerCard {
    pub fn new(
        server: &Server,
        connection_state: CardConnectionState,
        latency_state: LatencyState,
        favourite: bool,
        handlers: CardHandlers,
    ) -> Self {
        let CardHandlers {
            select: on_select,
            activate: on_activate,
            ping: on_ping,
            trust: on_trust,
            set_alias: on_set_alias,
            toggle_favourite,
            show_logs: on_show_logs,
            report: on_report,
        } = handlers;
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
        let alias = gtk::Label::builder()
            // Labelled, unlike the two lines above it: an address is
            // self-evident, a bare lowercase word under one is not.
            .label(server.alias.as_deref().map_or_else(
                || "no alias".to_string(),
                |alias| format!("alias  ·  {alias}"),
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
        let edit_alias = icon_button("document-edit-symbolic", "Set alias");
        edit_alias.add_css_class("server-action");
        let current_alias = server.alias.clone();
        edit_alias.connect_clicked(move |button| {
            show_alias_dialog(button, current_alias.as_deref(), on_set_alias.clone());
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
            .icon_name(ping_icon(false))
            .tooltip_text(ping_label(false))
            .valign(gtk::Align::Center)
            .css_classes(["flat", "server-action"])
            .build();
        ping_button.update_property(&[gtk::accessible::Property::Label(ping_label(false))]);
        ping_button.connect_clicked(move |_| on_ping());
        // Never shown on the card itself: it exists so the context menu has a
        // button to borrow, the way every other item there does.
        let trust_button = gtk::Button::builder().visible(false).build();
        trust_button.connect_clicked(move |_| on_trust());
        // Only ordinary TLS has a certificate to pin. REALITY authenticates by
        // public key and presents a borrowed chain nobody should pin, and a
        // plain protocol presents nothing — offering the item there would open
        // a dialog that can only fail.
        let pinnable = server
            .spec
            .stream()
            .is_some_and(|stream| stream.security == "tls");
        // Starring is the one way into the Favourites group, and Favourites is
        // the answer to "the four servers I actually use are somewhere in six
        // hundred".
        let favourite_button = gtk::ToggleButton::builder()
            .icon_name(favourite_icon(favourite))
            .tooltip_text(favourite_tooltip(favourite))
            .active(favourite)
            .valign(gtk::Align::Center)
            .css_classes(["flat", "server-action"])
            .build();
        favourite_button.update_property(&[gtk::accessible::Property::Label(favourite_tooltip(
            favourite,
        ))]);
        favourite_button.connect_clicked(move |button| {
            let now = button.is_active();
            button.set_icon_name(favourite_icon(now));
            button.set_tooltip_text(Some(favourite_tooltip(now)));
            button.update_property(&[gtk::accessible::Property::Label(favourite_tooltip(now))]);
            toggle_favourite();
        });

        let action_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        action_row.set_hexpand(true);
        action_row.append(&favourite_button);
        action_row.append(&edit_alias);
        action_row.append(&copy_button);
        action_row.append(&ping_button);
        let action_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        action_spacer.set_hexpand(true);
        action_row.append(&action_spacer);
        action_row.append(&connect_button);

        // Why the last check produced no number, which the badge cannot say: it
        // has a glyph, a tooltip and a pill to fit in, and "the server did not
        // answer" covers a refused handshake, a wrong TLS parameter and a dead
        // network alike. Hidden entirely while there is nothing to explain,
        // rather than left as an empty row that reads as a defect.
        let failure_reason = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(1)
            .selectable(true)
            .css_classes(["server-meta", "server-failure-reason"])
            .build();
        let failure_attempt = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(1)
            .selectable(true)
            .css_classes(["dim-label", "server-meta"])
            .build();
        // The rest of what happened is in the log, mixed with every other
        // source on the machine. This is the only way to it that arrives
        // already narrowed to the server being asked about.
        // `flat` alone, without `server-action`: that class squares a button
        // down to an icon's footprint, and this one carries words.
        let failure_logs = gtk::Button::builder()
            .label("Show in logs")
            .halign(gtk::Align::Start)
            .css_classes(["flat"])
            .build();
        failure_logs.connect_clicked(move |_| on_show_logs());
        // The step after reading the log, in the place somebody reaches it
        // from. What it produces has every address, host name, account id and
        // credential taken out and marked, so it can be pasted into a public
        // issue without being read line by line first.
        let failure_report = gtk::Button::builder()
            .label("Report a problem")
            .halign(gtk::Align::Start)
            .css_classes(["flat"])
            .build();
        failure_report.connect_clicked(move |_| on_report());
        let failure_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        failure_actions.append(&failure_logs);
        failure_actions.append(&failure_report);
        let failure = gtk::Box::new(gtk::Orientation::Vertical, 4);
        failure.set_visible(false);
        failure.set_css_classes(&["server-failure"]);
        failure.append(&failure_reason);
        failure.append(&failure_attempt);
        failure.append(&failure_actions);

        // The record behind the badge. One number cannot tell a steady server
        // from one that is fast half the time, and choosing between servers is
        // what the page is for. Rebuilt rather than reused row by row: the list
        // is at most `PROBE_HISTORY_LIMIT` long and changes only when a check
        // finishes on the one card that is open.
        let history_title = gtk::Label::builder()
            .xalign(0.0)
            .label("Recent checks")
            .css_classes(["server-meta", "server-history-title"])
            .build();
        let history_list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let history = gtk::Box::new(gtk::Orientation::Vertical, 4);
        history.set_visible(false);
        history.set_css_classes(&["server-history"]);
        history.append(&history_title);
        history.append(&history_list);

        let metadata = gtk::Box::new(gtk::Orientation::Vertical, 6);
        metadata.append(&full_name);
        metadata.append(&meta);
        metadata.append(&alias);
        metadata.append(&failure);
        metadata.append(&history);
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

        // Right-click used to be a second way to expand the card, which made it
        // the only pointer gesture in the app that did nothing a left click did
        // not already do. The actions it now offers are the buttons above,
        // driven by `emit_clicked` rather than re-implemented: a menu that
        // copied their logic would be a second place for it to drift, and this
        // way each item inherits its source button's sensitivity for free.
        //
        // The gesture sits on `shell` rather than on `header`. `header` is a
        // button fixed at `COMPACT_CARD_HEIGHT` and is `detail_region`'s
        // sibling, so it is never on the event path for a press that lands in
        // the detail region — and the menu carries actions that exist nowhere
        // else while the card is open. `shell` is the parent of both, so it is
        // on the path for the whole card at either height.
        //
        // Bubble, not capture: the seven metadata labels below are selectable
        // and install GTK's own text menu, which is the one that must win over
        // text. They are selectable so an address or a failure reason can be
        // copied out, and a card menu that took the press first would remove
        // the only way to do it. Bubble runs target to root, so text keeps its
        // menu and everything else gets the card's.
        let context_menu: Rc<RefCell<Option<gtk::Popover>>> = Rc::new(RefCell::new(None));
        let secondary_click = gtk::GestureClick::new();
        secondary_click.set_button(gtk::gdk::BUTTON_SECONDARY);
        secondary_click.set_propagation_phase(gtk::PropagationPhase::Bubble);
        secondary_click.connect_pressed({
            let context_menu = context_menu.clone();
            let shell = shell.clone();
            let connect_button = connect_button.clone();
            let favourite_button = favourite_button.clone();
            let edit_alias = edit_alias.clone();
            let copy_button = copy_button.clone();
            let ping_button = ping_button.clone();
            move |_, n_press, x, y| {
                if click_plan_for_press(gtk::gdk::BUTTON_SECONDARY, n_press)
                    != ClickPlan::ContextMenu
                {
                    return;
                }
                let mut slot = context_menu.borrow_mut();
                let popover = slot.get_or_insert_with(|| {
                    // Built on first use: a card is recreated on every rebuild,
                    // and a subscription of six hundred servers should not pay
                    // for six hundred popovers nobody opens.
                    let popover = context_popover(&items(
                        &connect_button,
                        favourite_button.upcast_ref(),
                        &edit_alias,
                        &copy_button,
                        &ping_button,
                        pinnable.then_some(&trust_button),
                    ));
                    popover.set_parent(&shell);
                    popover
                });
                sync_context_labels(
                    popover,
                    &items(
                        &connect_button,
                        favourite_button.upcast_ref(),
                        &edit_alias,
                        &copy_button,
                        &ping_button,
                        pinnable.then_some(&trust_button),
                    ),
                );
                popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.popup();
            }
        });
        shell.add_controller(secondary_click);

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
            ping_button,
            failure,
            failure_reason,
            failure_attempt,
            history,
            history_list,
            expanded,
            last_latency: Rc::new(Cell::new(latency_state)),
            last_connection: Rc::new(Cell::new(CardConnectionState::Disconnected)),
            latency_predates_failure: Rc::new(Cell::new(false)),
            latency_generation: Rc::new(Cell::new(0)),
            height_generation: Rc::new(Cell::new(0)),
            opacity_generation: Rc::new(Cell::new(0)),
            height_animating: Rc::new(Cell::new(false)),
            height_target: Rc::new(Cell::new(COMPACT_CARD_HEIGHT)),
            failure_shown: Rc::new(RefCell::new(None)),
            history_shown: Rc::new(RefCell::new(Vec::new())),
        };
        card.apply_latency(latency_state);
        card.set_connection_state(connection_state);
        card
    }

    /// Point the check button at starting a check or at stopping one.
    ///
    /// Called by the servers view rather than from inside
    /// [`Self::set_latency_state`], because whether a stop is offerable at all
    /// depends on the daemon knowing how to stop — a fact a card has no way to
    /// learn. The view owns that flag and the same decision for the block's
    /// sweep button, so both live in one place.
    pub fn set_probing(&self, probing: bool) {
        self.ping_button.set_icon_name(ping_icon(probing));
        self.ping_button.set_tooltip_text(Some(ping_label(probing)));
        self.ping_button
            .update_property(&[gtk::accessible::Property::Label(ping_label(probing))]);
    }

    pub fn set_latency_state(&self, state: LatencyState) {
        // A card left over from a failed attempt hides its badge, because the
        // only number it had was taken before the attempt. Anything arriving
        // now was taken after it, so the reason to hide it is gone — and
        // re-checking a failed server is the one way to find out it recovered.
        if self.last_connection.get() == CardConnectionState::Failed
            && !matches!(state, LatencyState::Unmeasured | LatencyState::Superseded)
        {
            self.latency_predates_failure.set(false);
            self.latency_display.set_visible(true);
        }
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

    /// Show, or stop showing, why the last check produced no number.
    ///
    /// Separate from [`Self::set_latency_state`] because the two answer
    /// different questions for different readers: the badge is a glance at
    /// every card in the grid, and this is the diagnosis on the one card
    /// somebody opened. Pushing it through the badge's channel would have put
    /// a `String` in a `Copy` state that the grid keeps one of per card.
    ///
    /// Returns whether what is drawn changed, which is what decides a
    /// re-measure. The caller used to re-measure whenever the card was open,
    /// so every selection disturbed the expansion it had just started.
    pub fn set_failure_report(&self, report: Option<&FailureReport>) -> bool {
        if self.failure_shown.borrow().as_ref() == report {
            return false;
        }
        self.failure_shown.replace(report.cloned());
        match report {
            Some(report) => {
                self.failure_reason.set_label(&report.reason);
                self.failure_attempt.set_label(&report.attempt);
                self.failure.set_visible(true);
            }
            None => {
                self.failure.set_visible(false);
                // Cleared rather than left standing behind the hidden box: the
                // labels are selectable, and a reason from two checks ago is
                // worse than none at all if anything ever reveals them again.
                self.failure_reason.set_label("");
                self.failure_attempt.set_label("");
            }
        }
        true
    }

    /// Show, or stop showing, what the recent checks measured.
    ///
    /// Pushed only for the card that is open, like the failure block above it
    /// and for a sharper version of the same reason: this one is fetched from
    /// the daemon by a call of its own rather than taken from the snapshot the
    /// grid polls, so asking for every card would be one D-Bus round trip per
    /// card twice a second.
    ///
    /// Returns whether what is drawn changed, for the same reason as
    /// [`Self::set_failure_report`] — and with more force here, since the poll
    /// that feeds this list runs twice a second and rebuilding the rows would
    /// otherwise re-measure the card on every tick.
    pub fn set_history(&self, rows: &[HistoryRow]) -> bool {
        if self.history_shown.borrow().as_slice() == rows {
            return false;
        }
        self.history_shown.replace(rows.to_vec());
        while let Some(child) = self.history_list.first_child() {
            self.history_list.remove(&child);
        }
        if rows.is_empty() {
            self.history.set_visible(false);
            return true;
        }
        for row in rows {
            // Fixed width on the number so the column lines up: "8 ms" and
            // "1204 ms" in the same list otherwise leave the methods ragged,
            // and the point of the list is comparing down it.
            let value = gtk::Label::builder()
                .xalign(0.0)
                .label(&row.value)
                .width_chars(8)
                .selectable(true)
                .css_classes(["server-meta"])
                .build();
            let taken = gtk::Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .label(&row.taken)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .max_width_chars(1)
                .selectable(true)
                .css_classes(["dim-label", "server-meta"])
                .build();
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            line.append(&value);
            line.append(&taken);
            self.history_list.append(&line);
        }
        self.history.set_visible(true);
        true
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
                self.show_label("—", "Latency has not been checked");
            }
            LatencyState::Superseded => {
                self.show_label("—", "Measured in a different context — needs a fresh check");
            }
            LatencyState::Checking => {
                self.latency_display.set_visible_child_name("spinner");
                self.latency_spinner.set_spinning(true);
                self.latency_spinner_pill
                    .set_tooltip_text(Some("Checking latency"));
            }
            // Same dash as an unmeasured server, in the error colour: a failed
            // check still leaves no number, and a cross reads as a verdict on
            // the server rather than as the absence of a reading.
            LatencyState::Unreachable => {
                self.show_label("—", UNREACHABLE_TEXT);
                self.latency.add_css_class("latency-error");
            }
            LatencyState::NoNetwork => {
                self.show_label("⊘", "No network — the server was not checked");
                self.latency.add_css_class("latency-offline");
            }
            // The offline colour, not the error one: nothing here is the
            // server's doing, and the amber says "this machine" the way the
            // no-network state already does.
            // The daemon names the condition when it knows it — a rejected
            // certificate is not "see Settings › Xray core", and telling
            // someone to look at a core that is present and working is worse
            // than saying nothing.
            LatencyState::NotRun(detail) => {
                let tooltip = match detail {
                    Some(detail) => format!("Not measured: {}", detail.message()),
                    None => "The check could not run on this machine — see Settings › Xray core"
                        .to_string(),
                };
                self.show_label("⊘", &tooltip);
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
                let ago = super::reduce::minutes_ago(minutes);
                self.show_label(text, &format!("{tooltip} · measured {ago}"));
                self.latency.add_css_class("latency-stale");
            }
            LatencyAge::Fresh | LatencyAge::Unknown => self.show_label(text, tooltip),
        }
    }

    pub fn set_connection_state(&self, state: CardConnectionState) {
        // Entering the failed state is what makes the current number stale, not
        // being in it: a re-check while the card still says "Error" clears
        // this again, and the poll re-asserting the same state must not undo
        // that.
        let previous = self.last_connection.replace(state);
        if state != CardConnectionState::Failed {
            self.latency_predates_failure.set(false);
        } else if previous != CardConnectionState::Failed {
            self.latency_predates_failure.set(true);
        }
        self.connect_button.remove_css_class("suggested-action");
        self.connect_button.remove_css_class("destructive-action");
        self.status.remove_css_class("status-working");
        self.status.remove_css_class("status-neutral");
        self.status.remove_css_class("status-error");
        self.root.remove_css_class("failed-server");
        self.connect_button.set_sensitive(true);
        let accessible_status = match state {
            CardConnectionState::Disconnected | CardConnectionState::ConnectedElsewhere => {
                "Disconnected"
            }
            CardConnectionState::ConnectedHere => "Connected",
            CardConnectionState::InPool => "One of several servers in use",
            CardConnectionState::Connecting => "Connecting",
            CardConnectionState::Failed => "Connection error",
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
            CardConnectionState::InPool => {
                self.status.set_label("In use");
                self.status.add_css_class("status-neutral");
                self.status.set_visible(true);
                self.latency_display.set_visible(true);
                self.connect_button.set_label("Use alone");
                self.connect_button.add_css_class("suggested-action");
                self.root.remove_css_class("active-server");
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
                self.status.set_label("Error");
                self.status.add_css_class("status-error");
                self.status.set_visible(true);
                // A number taken *before* the attempt reads beside "Error" as
                // "the tunnel is fine, 84 ms" — the exact lie — so it stays
                // hidden. A number taken after it is the opposite: it is how
                // the user finds out the server came back, so re-checking
                // brings the badge straight back.
                self.latency_display
                    .set_visible(!self.latency_predates_failure.get());
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

    /// Take a fresh measurement of an open card.
    ///
    /// Called both when the content under the header changed and when the
    /// column width did, and in either case it may land in the middle of the
    /// expansion that is drawing the card. It must not cancel that expansion:
    /// the fade the expansion started is what makes the region visible at all.
    pub fn resize_expanded(&self, target_height: i32) {
        match height_refresh(
            self.expanded.get(),
            self.height_animating.get(),
            self.height_target.get(),
            target_height,
        ) {
            HeightRefresh::Ignore => {}
            HeightRefresh::Set => {
                self.height_generation
                    .set(self.height_generation.get().wrapping_add(1));
                self.height_animating.set(false);
                self.height_target.set(target_height);
                self.root.set_animated_height(target_height);
            }
            HeightRefresh::Retarget => {
                let generation = self.height_generation.get().wrapping_add(1);
                self.height_generation.set(generation);
                self.height_target.set(target_height);
                self.animate_height(
                    generation,
                    HeightTransition {
                        from: self.root.animated_height(),
                        to: target_height,
                        duration: EXPAND_DURATION_MS,
                        easing: adw::Easing::EaseOutCubic,
                    },
                    None,
                );
            }
        }
    }

    pub fn set_expanded_immediately(&self, target_height: i32) {
        self.bump_generations();
        self.height_animating.set(false);
        self.height_target.set(target_height);
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
        self.bump_generations();
        self.finish_collapse();
    }

    /// Invalidate every animation in flight on this card at once. Used where
    /// the card is put into a state directly, so that neither a height frame
    /// nor a fade frame can arrive afterwards and undo it.
    fn bump_generations(&self) {
        self.height_generation
            .set(self.height_generation.get().wrapping_add(1));
        self.opacity_generation
            .set(self.opacity_generation.get().wrapping_add(1));
    }

    pub fn expand(&self, target_height: i32, on_done: Option<Box<dyn FnOnce()>>) {
        self.bump_generations();
        let generation = self.height_generation.get();
        self.expanded.set(true);
        self.height_target.set(target_height);
        let current_height = self.root.animated_height().max(COMPACT_CARD_HEIGHT);

        self.root.set_valign(gtk::Align::Start);
        self.root.set_animated_height(current_height);
        self.root.add_css_class("selected-server");
        self.detail_region.set_visible(true);
        // Not targetable until it is drawn. A region at zero opacity still
        // takes a click, so the buttons under it were reachable through what
        // looked like blank space.
        self.detail_region.set_can_target(false);

        if !adw::is_animations_enabled(&self.root) {
            self.height_animating.set(false);
            self.root.set_animated_height(target_height);
            self.show_detail_region();
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
            self.opacity_generation.get(),
            self.detail_region.opacity(),
            1.0,
            DETAIL_FADE_IN_DURATION_MS,
            adw::Easing::EaseOutCubic,
        );
    }

    /// The end state of a fade in, written whole. Reached from the animation's
    /// completion and from the path that has no animations to run, so that
    /// there is exactly one description of what a shown detail region is.
    fn show_detail_region(&self) {
        self.detail_region.set_opacity(1.0);
        self.detail_region.set_can_target(true);
    }

    /// Collapse back to the compact height. `on_shrink` receives the height
    /// delta of every animation frame; the servers view uses it to compensate
    /// the scroll position so the selected card stays pinned on screen while a
    /// card above it shrinks.
    pub fn collapse(&self, on_shrink: Option<Rc<dyn Fn(i32)>>) {
        self.bump_generations();
        let generation = self.height_generation.get();
        self.height_animating.set(false);
        self.height_target.set(COMPACT_CARD_HEIGHT);
        let current_height = self.root.animated_height().max(COMPACT_CARD_HEIGHT);

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
        self.height_animating.set(false);
        self.height_target.set(COMPACT_CARD_HEIGHT);
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
        let completion: Completion = Rc::new(RefCell::new(on_done));
        animation.connect_done({
            let card = self.clone();
            move |_| {
                // A superseded animation still reports itself done, so the
                // flag is cleared only by the animation that is still the
                // current one — otherwise a retarget would immediately be
                // treated as finished.
                if card.height_generation.get() != generation {
                    return;
                }
                card.height_animating.set(false);
                // The end value, written out: a timed animation's last frame
                // is whatever the frame clock happened to land on, and the
                // card must stand exactly at the height that was measured.
                card.root.set_animated_height(to);
                if let Some(on_done) = completion.borrow_mut().take() {
                    on_done();
                }
            }
        });
        self.height_animating.set(true);
        animation.play();
    }

    /// Fade the detail region, guarded by [`Self::opacity_generation`] and
    /// **finished by a terminal write**.
    ///
    /// Both halves are the defect. Guarding the fade on the height generation
    /// meant a re-measure cancelled it; having no `connect_done` meant a fade
    /// that was cancelled before its first frame-clock tick left the region at
    /// the opacity it was built with, which is zero. Either alone is a blank
    /// card, and the card measured its real contents the whole time.
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
                if card.opacity_generation.get() == generation {
                    card.detail_region.set_opacity(value);
                }
            }
        });
        let animation = adw::TimedAnimation::new(&self.detail_region, from, to, duration, target);
        animation.set_easing(easing);
        animation.connect_done({
            let card = self.clone();
            move |_| {
                if card.opacity_generation.get() != generation {
                    return;
                }
                if to >= 1.0 {
                    card.show_detail_region();
                } else {
                    card.detail_region.set_opacity(to);
                }
            }
        });
        animation.play();
    }
}

fn show_alias_dialog(
    parent: &impl IsA<gtk::Widget>,
    current: Option<&str>,
    on_save: Rc<dyn Fn(String)>,
) {
    let window = adw::Window::builder()
        .title("Set alias")
        .modal(true)
        .default_width(420)
        .build();
    set_transient_parent(&window, parent);

    let header = adw::HeaderBar::new();
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    save.set_sensitive(false);
    header.pack_start(&cancel);
    header.pack_end(&save);

    let group = adw::PreferencesGroup::new();
    let entry = adw::EntryRow::builder()
        .title("Alias")
        .text(current.unwrap_or_default())
        .activates_default(true)
        .build();
    group.add(&entry);
    let validation = validation_label();
    let content = dialog_content(&group, &validation);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&header);
    page.append(&content);
    window.set_content(Some(&page));
    window.set_default_widget(Some(&save));

    let update_validation: Rc<dyn Fn()> = Rc::new({
        let entry = entry.clone();
        let save = save.clone();
        let validation = validation.clone();
        move || {
            let issue = alias_validation(entry.text().as_str());
            save.set_sensitive(issue.is_none());
            set_validation(&validation, issue);
        }
    });
    entry.connect_changed({
        let update_validation = update_validation.clone();
        move |_| update_validation()
    });
    update_validation();

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    let window_for_save = window.clone();
    save.connect_clicked(move |button| {
        let alias = entry.text().to_string();
        if !button.is_sensitive() || alias_validation(&alias).is_some() {
            return;
        }
        on_save(alias);
        window_for_save.close();
    });
    window.present();
}

pub fn alias_validation(alias: &str) -> Option<&'static str> {
    (!oxidom_core::alias::is_valid(alias)).then_some(ALIAS_ERROR)
}

/// A source button, and the wording its menu item should carry.
///
/// `None` takes the wording from the button itself, which is what dynamic items
/// need: Connect becomes Disconnect, the star becomes an unstar. A fixed label
/// is for buttons whose tooltip explains a *refusal* rather than naming the
/// action — a disabled menu row must still say what it would have done.
type ContextItem<'a> = (&'a gtk::Button, Option<&'static str>);

/// One menu item per source button, activating that very button.
///
/// The items carry no behaviour of their own: a context menu that re-implemented
/// Connect or Copy would be a second copy of rules that already live on the card
/// — including which of them are available at all, which the items inherit by
/// mirroring `sensitive`.
/// The menu's contents, in one place: it is built on first use and its labels
/// are re-synced on every open, and the two lists drifting apart would show one
/// item and act on another.
fn items<'a>(
    connect: &'a gtk::Button,
    favourite: &'a gtk::Button,
    alias: &'a gtk::Button,
    copy: &'a gtk::Button,
    ping: &'a gtk::Button,
    trust: Option<&'a gtk::Button>,
) -> Vec<ContextItem<'a>> {
    let mut items = vec![
        (connect, None),
        (favourite, None),
        (alias, None),
        (copy, Some("Copy share-link")),
        (ping, None),
    ];
    items.extend(trust.map(|trust| (trust, Some("Trust certificate…"))));
    items
}

fn context_popover(sources: &[ContextItem<'_>]) -> gtk::Popover {
    let items = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for (source, _) in sources {
        let item = gtk::Button::builder()
            .css_classes(["flat", "server-context-item"])
            .child(&gtk::Label::builder().xalign(0.0).build())
            .build();
        item.connect_clicked({
            let source = (*source).clone();
            move |item| {
                if let Some(popover) = item
                    .ancestor(gtk::Popover::static_type())
                    .and_downcast::<gtk::Popover>()
                {
                    popover.popdown();
                }
                source.emit_clicked();
            }
        });
        items.append(&item);
    }
    gtk::Popover::builder()
        .child(&items)
        .has_arrow(false)
        .css_classes(["menu"])
        .build()
}

/// A card's buttons change wording with its state — Connect becomes Disconnect,
/// the star becomes an unstar — so the menu takes its text at open time rather
/// than at build time.
fn sync_context_labels(popover: &gtk::Popover, sources: &[ContextItem<'_>]) {
    let Some(items) = popover.child().and_downcast::<gtk::Box>() else {
        return;
    };
    let mut item = items.first_child();
    for (source, fixed) in sources {
        let Some(current) = item else { return };
        if let Some(label) = current.first_child().and_downcast::<gtk::Label>() {
            let text = fixed.map(str::to_string).unwrap_or_else(|| {
                source
                    .label()
                    .filter(|label| !label.is_empty())
                    .or_else(|| source.tooltip_text())
                    .unwrap_or_default()
                    .to_string()
            });
            label.set_label(&text);
        }
        // Carries the refusal when there is one: the row says what it would do,
        // the tooltip says why it will not.
        current.set_tooltip_text(source.tooltip_text().as_deref());
        current.set_sensitive(source.is_sensitive());
        item = current.next_sibling();
    }
}

fn click_plan_for_press(button: u32, n_press: i32) -> ClickPlan {
    match (button, n_press) {
        (gtk::gdk::BUTTON_PRIMARY, 1) => ClickPlan::ToggleDetails,
        (gtk::gdk::BUTTON_PRIMARY, 2) => ClickPlan::Activate,
        (gtk::gdk::BUTTON_SECONDARY, 1) => ClickPlan::ContextMenu,
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
        ALIAS_ERROR, COMPACT_CARD_HEIGHT, CardConnectionState, ClickPlan, HeightRefresh,
        LatencyAge, LatencyMethod, LatencyState, alias_validation, click_plan_for_press,
        height_refresh,
    };

    /// A re-measure that arrives while the card is opening must aim the
    /// expansion somewhere else, never replace it.
    ///
    /// This is the defect in one line. The old code took the `Set` branch
    /// unconditionally, and `Set` bumps the generation that guarded the fade as
    /// well as the height — so the card jumped to its measured height with its
    /// contents still at the opacity they were built with, which is zero. Both
    /// pushes that feed an open card land in exactly this window: the failure
    /// block in the same main-loop iteration as the click, the recent checks a
    /// poll later.
    #[test]
    fn a_measurement_that_lands_mid_expansion_aims_it_rather_than_replacing_it() {
        assert_eq!(
            height_refresh(true, true, 280, 340),
            HeightRefresh::Retarget
        );
        assert_eq!(height_refresh(true, false, 280, 340), HeightRefresh::Set);
    }

    /// Nothing is disturbed by a measurement that agrees with where the card
    /// already stands, or is already going. The comparison is against the
    /// destination and not against the height being drawn this frame: an
    /// expansion passes through every height between the two, and comparing
    /// against the drawn one would read the animation's own progress as a
    /// change and restart it on every poll.
    #[test]
    fn a_measurement_that_changes_nothing_disturbs_nothing() {
        assert_eq!(height_refresh(true, true, 340, 340), HeightRefresh::Ignore);
        assert_eq!(height_refresh(true, false, 340, 340), HeightRefresh::Ignore);
        assert_eq!(
            height_refresh(false, false, COMPACT_CARD_HEIGHT, 340),
            HeightRefresh::Ignore,
            "a card that is not open has no expanded height to refresh"
        );
        assert_eq!(
            height_refresh(false, true, COMPACT_CARD_HEIGHT, 340),
            HeightRefresh::Ignore,
            "nor while it is collapsing"
        );
    }

    #[test]
    fn primary_click_toggles_double_click_activates_and_secondary_opens_the_menu() {
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
            ClickPlan::ContextMenu
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
        assert_ne!(
            CardConnectionState::InPool,
            CardConnectionState::ConnectedHere
        );
        // The whole point of the state: a server that failed does not look
        // like one the user never touched.
        assert_ne!(
            CardConnectionState::Failed,
            CardConnectionState::Disconnected
        );
    }

    #[test]
    fn alias_validation_uses_the_core_rules() {
        for valid in ["home", "ch-trojan", "a"] {
            assert_eq!(alias_validation(valid), None, "{valid:?}");
        }
        for invalid in [
            "",
            "Home",
            "home.office",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "deadbeefcafe1234",
        ] {
            assert_eq!(alias_validation(invalid), Some(ALIAS_ERROR), "{invalid:?}");
        }
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
