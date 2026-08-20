use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use oxidom_core::model::Subscription;
use oxidom_core::pool::PoolQuery;

use super::super::group::subscription_description;
use super::super::prefs::{FAVOURITES_ID, GroupKind, GuiPrefs, ServerGroup};
use super::super::reduce::{
    FailureReport, FilterOption, HistoryRow, ServerProfiles, available_countries,
    available_protocols, available_subscriptions, connect_choices, describe_rule,
    excludable_servers, filtered_ids, filters_to_query, group_member_ids, groups_holding,
    moved_in_order, ordered_subscriptions, query_equals_group, toggled_member, upsert_group,
};
use super::super::server_card::{
    CARD_MEASURE_WIDTH, COMPACT_CARD_HEIGHT, CardConnectionState, CardHandlers, LatencyState,
    ServerCard,
};

const CARD_COLUMN_SPACING: i32 = 12;
const CARD_ROW_SPACING: i32 = 12;
const MIN_CARD_WIDTH: i32 = 250;
const MIN_CARD_WIDTH_FOR_THREE_COLUMNS: i32 = 300;
// Deliberately `CARD_MEASURE_WIDTH` rather than another step of the 250 -> 300
// ladder. That is the width a card measures its own expanded height at, so a
// column narrower than this makes the cached height a measurement of a card
// that does not exist — text wraps to more lines than were paid for. Three
// columns already sit below it at 300, which is survivable at that count and is
// not worth widening now; a fourth column is where the gap stops being small,
// so this is where the two numbers are tied together.
const MIN_CARD_WIDTH_FOR_FOUR_COLUMNS: i32 = CARD_MEASURE_WIDTH;
const COLUMN_HYSTERESIS: i32 = 16;
const RESIZE_SETTLE_MS: u64 = 120;

/// What the cards should say about the connection. One value rather than three
/// arguments so the window's "already applied" cache cannot fall out of step
/// with what the cards were last told.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CardConnection {
    /// Compatibility server the header controls.
    pub active: Option<String>,
    /// Connected profiles grouped by the server they carry.
    pub profiles: HashMap<String, ServerProfiles>,
    /// The server an attempt is being built for.
    pub connecting: Option<String>,
    /// The server whose attempt failed, until something replaces it.
    pub failed: Option<String>,
}

/// `select` only inspects/expands a card. `activate` independently connects,
/// switches, or disconnects its server.
#[derive(Clone)]
pub struct CardCallbacks {
    pub select: Rc<dyn Fn(String)>,
    pub activate: Rc<dyn Fn(String)>,
    pub ping: Rc<dyn Fn(String)>,
    /// Look at one server's certificate and decide about it, before anything
    /// has failed.
    pub trust: Rc<dyn Fn(String)>,
    pub recheck: Rc<dyn Fn(Vec<String>)>,
    pub refresh: Rc<dyn Fn(String)>,
    pub set_alias: Rc<dyn Fn(String, String)>,
    pub create_pool: Rc<dyn Fn(PoolQuery)>,
    /// Run the group on screen, now. Writes no profile and confirms nothing —
    /// `create_pool` is where a selection is saved, and it is a separate,
    /// deliberate act. The second argument is the selection already resolved to
    /// server ids, which is how the window recognises a session that is already
    /// running it.
    pub connect_pool: Rc<dyn Fn(PoolQuery, Vec<String>)>,
    /// Show the log page narrowed to one server. Offered from the expanded
    /// card beside a failed check, where what the core printed is the next
    /// thing anybody wants.
    pub show_logs: Rc<dyn Fn(String)>,
    /// Start a problem report about this server, from the log narrowed to it.
    pub report: Rc<dyn Fn(String)>,
}

/// One subscription block. Cards live in independent vertical column boxes
/// (equal widths via the homogeneous horizontal box), so a card growing or
/// shrinking moves only its own column's tail — columns never couple through
/// shared row heights, and a card's slot changes only on repack (rebuild,
/// filter, sort, column-count change), never on selection.
#[derive(Clone)]
struct SubscriptionBlock {
    /// Subscription id. The block's own actions address it by id rather than by
    /// position, because reordering moves the position out from under them.
    id: String,
    root: gtk::Widget,
    columns_box: gtk::Box,
    column_boxes: Rc<RefCell<Vec<gtk::Box>>>,
    cards: Vec<(String, gtk::Widget)>,
    display_order: Rc<RefCell<Vec<String>>>,
    sort_button: gtk::Button,
    /// Held for the same reason `sort_button` is: its icon has to say whether
    /// pressing it starts a sweep or stops one. It used to be built and dropped,
    /// so nothing could ever change it.
    speed_button: gtk::Button,
    /// How many of this block's cards are checking. A count rather than a flag
    /// because cards retire one at a time as the daemon works through the queue,
    /// and the button must stay a stop button until the last of them is done.
    checking: Rc<Cell<usize>>,
    sort_generation: Rc<Cell<u64>>,
}

type BrowseCallback = Rc<RefCell<Option<Box<dyn Fn()>>>>;
/// Set on every rebuild, read long afterwards by the filter popover.
type PoolCallback = Rc<RefCell<Option<Rc<dyn Fn(PoolQuery)>>>>;
/// Like [`PoolCallback`], plus the ids the query resolved to.
type ResolvedPoolCallback = Rc<RefCell<Option<Rc<dyn Fn(PoolQuery, Vec<String>)>>>>;
/// Server id to its checkbox, in the order the picker laid them out.
type PickerChecks = Rc<RefCell<Vec<(String, gtk::CheckButton)>>>;
/// The handles a picker's checkboxes write into. Shared with whoever will save
/// them, so there is one answer to "what is ticked" rather than two.
type Selection = Rc<RefCell<Vec<String>>>;
/// Told what is ticked now, every time a checkbox moves.
type SelectionChanged = Rc<dyn Fn(&[String])>;

/// Which list of hand-picked servers a picker page writes into. The two pages
/// are the same screen over two different fields, so the field travels as a
/// closure rather than as a duplicated page.
type DraftField = Rc<dyn Fn(&mut FilterDraft, Vec<String>)>;

/// Which of the three multi-select filters a checkbox writes into.
///
/// The boxes all look alike and all live in one list, so the field is carried
/// beside each one rather than inferred from which row it happens to be under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilterField {
    Country,
    Protocol,
    Subscription,
}

/// What the selection editor is editing, before Apply writes it to the page.
///
/// A copy rather than the live fields: a modal dialog hides the list it would be
/// changing, so applying each tick was work nobody could watch — and a form that
/// commits once can also be abandoned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FilterDraft {
    countries: Vec<String>,
    protocols: Vec<String>,
    subscriptions: Vec<String>,
    exclude: Vec<String>,
    /// Servers named by hand. Non-empty means the rule is not in force at all —
    /// the same precedence `pool::resolve` gives a list over a rule.
    members: Vec<String>,
}

impl FilterDraft {
    /// How many of the rule's fields are set. The hand-picked members are not
    /// one of them: they are the other answer to the same question, not a
    /// narrower rule.
    fn rule_fields(&self) -> usize {
        [
            self.countries.is_empty(),
            self.protocols.is_empty(),
            self.subscriptions.is_empty(),
            self.exclude.is_empty(),
        ]
        .into_iter()
        .filter(|empty| !empty)
        .count()
    }

    /// Which kind of group this selection would be saved as.
    ///
    /// Derived rather than asked. It used to be a radio, on the reasoning that
    /// "these forty-two, frozen" and "whatever is German, forever" are different
    /// intents only the user knows — which is true, and is exactly what the form
    /// above already says: servers named by hand are frozen because naming them
    /// is what freezing means, and a rule with no members keeps matching because
    /// that is all a rule can do. The radio asked for the answer a second time,
    /// in the vocabulary of how groups are stored, as the first step of making
    /// one.
    fn kind(&self) -> GroupKind {
        if self.members.is_empty() {
            GroupKind::Rule
        } else {
            GroupKind::List
        }
    }

    fn to_query(&self, name: String, kind: GroupKind) -> PoolQuery {
        match kind {
            // Frozen exactly as chosen, in the order the picker shows them.
            GroupKind::List => PoolQuery {
                name,
                members: self.members.clone(),
                ..PoolQuery::default()
            },
            GroupKind::Rule => PoolQuery {
                name,
                countries: self.countries.clone(),
                protocols: self.protocols.clone(),
                subscriptions: self.subscriptions.clone(),
                exclude: self.exclude.clone(),
                ..PoolQuery::default()
            },
        }
    }
}

/// Why the selection editor was opened.
///
/// There is one editor; this decides only what it starts holding and which of
/// its buttons is the obvious one. Three doors used to lead to two dialogs that
/// asked overlapping questions in different words, and which of the two you got
/// decided whether your answer could be saved at all.
enum SelectionIntent {
    /// Narrow what the page is showing.
    Filter,
    /// The same editor, opened by "New group": it starts with the name focused,
    /// because the name is the part the user came here to fill in.
    Name,
    /// Change a group that already exists. Boxed only to keep the three
    /// variants the same size; one of these exists at a time.
    Edit(Box<ServerGroup>),
}

#[derive(Clone)]
pub struct ServersView {
    pub root: gtk::ScrolledWindow,
    content: gtk::Box,
    /// Just the subscription blocks and the "nothing matches" page — the part a
    /// scope change actually replaces, and therefore the only part that fades.
    servers_area: gtk::Box,
    cards: Rc<RefCell<HashMap<String, ServerCard>>>,
    blocks: Rc<RefCell<Vec<SubscriptionBlock>>>,
    /// Which servers are mid-check, so a repeated state for the same card does
    /// not double-count its block. `set_latency_state` is called for every card
    /// on every age sweep, not only when something changed.
    checking: Rc<RefCell<HashSet<String>>>,
    /// Whether the daemon knows how to call a check off. A daemon that does not
    /// must not be given a stop button: it would be a control that says it will
    /// stop something and then does not, which is worse than the second press
    /// being ignored as it was before. Set once at startup from the D-Bus
    /// capability, the way Settings decides whether to offer a geo download.
    can_cancel_probes: Rc<Cell<bool>>,
    subscriptions: Rc<RefCell<Vec<Subscription>>>,
    /// Lowercased "name transport protocol address:port country" per server.
    /// The search matches this, never transient widget text like the
    /// "Connected" badge — otherwise connecting would change search results.
    search_texts: Rc<RefCell<HashMap<String, String>>>,
    query: Rc<RefCell<String>>,
    filter_countries: Rc<RefCell<Vec<String>>>,
    filter_protocols: Rc<RefCell<Vec<String>>>,
    filter_subscriptions: Rc<RefCell<Vec<String>>>,
    /// Servers struck out by hand, by id. Part of the rule like the three above
    /// it — `PoolQuery.exclude` is where a rule says "everything German except
    /// this one", which no combination of the other three can express.
    filter_exclude: Rc<RefCell<Vec<String>>>,
    /// Members of the selected group when that group is a frozen list. Not a
    /// widget's contents — a list has no filter row to live in — so it is
    /// carried here and folded into `current_filter` by `apply_filter`.
    scope_members: Rc<RefCell<Vec<String>>>,
    /// What the visible selection would be saved as: a list of exactly the
    /// visible servers, or the rule that matched them.
    current_filter: Rc<RefCell<PoolQuery>>,
    /// Id of the chip that is selected, `None` for "All".
    active_group: Rc<RefCell<Option<String>>>,
    /// Mirror of `prefs.groups`, so the chip row and the popover read one list.
    saved_groups: Rc<RefCell<Vec<ServerGroup>>>,
    /// The selected chip's widget, so the "modified" mark can be updated on a
    /// keystroke without rebuilding the row under the pointer.
    /// The scope switcher, rebuilt whenever the set of groups changes. Held so
    /// `sync_chip_modified` can relabel one toggle without rebuilding the row
    /// under the pointer.
    scopes: Rc<RefCell<Option<adw::ToggleGroup>>>,
    /// Set while the row is being repopulated, so `active-name` changes made by
    /// the code do not re-enter `select_group` and rebuild the row from inside
    /// its own construction.
    syncing_scopes: Rc<Cell<bool>>,
    /// Set from the moment a scope is chosen until the list has been recomputed
    /// for it. Only [`Self::sync_chip_modified`] reads it, and only to keep quiet
    /// about a difference that is its own doing.
    switching_scope: Rc<Cell<bool>>,
    /// Set on every rebuild; the popover's "Create pool" needs it long after
    /// the callbacks that carried it went out of scope.
    on_create_pool: PoolCallback,
    on_connect_pool: ResolvedPoolCallback,
    /// The row of saved scopes, above the subscription blocks.
    chip_bar: gtk::Box,
    chip_scroll: gtk::ScrolledWindow,
    /// One line under the chip row, shown only until the first group exists.
    chip_hint: gtk::Label,
    /// Shown while a chip is selected, or while the filter is narrowing the
    /// list — never for a bare search, so the page does not gain and lose a
    /// strip on every keystroke.
    connect_bar: gtk::Box,
    connect_title: gtk::Label,
    connect_button: adw::SplitButton,
    /// Offered beside Connect only for a scope that is not saved yet: it is the
    /// answer to "and how do I keep this?", asked exactly where it comes up.
    connect_save: gtk::Button,
    /// Strategy the split button's primary half uses, last chosen from its
    /// menu. Not persisted: it describes this session's intent, and a pool the
    /// user actually kept is written into the profile anyway.
    connect_strategy: Rc<Cell<usize>>,
    /// How many nodes the pool rotates over, `0` meaning every live one. Starts
    /// at [`DEFAULT_POOL_ROTATION`] rather than at "all": a pool over a whole
    /// country is mostly repeats of a handful of hosts, and rotating over all of
    /// them buys no extra spread while costing an observatory ping apiece.
    connect_rotation_value: Rc<Cell<usize>>,
    connect_rotation: gtk::MenuButton,
    /// First thing in the chip row rather than an icon in the header. The
    /// header had six widgets packed before it and this one was the sixth, so
    /// nobody found it; the row of chips is where a scope is chosen, and the
    /// filter is how a new one is made.
    filter_button: gtk::Button,
    /// The pill's text, so `apply_filter` can say how many fields are set
    /// without rebuilding the row under the pointer.
    filter_label: gtk::Label,
    /// Number of card columns; driven by the window width (1, 2, 3, or 4).
    columns: Rc<Cell<usize>>,
    pending_columns: Rc<Cell<usize>>,
    column_update_scheduled: Rc<Cell<bool>>,
    resize_generation: Rc<Cell<u64>>,
    /// Latest completed measurements. This never changes display order by itself.
    latencies: Rc<RefCell<HashMap<String, Option<u32>>>>,
    /// The card whose inline details are open (at most one).
    selected: Rc<RefCell<Option<String>>>,
    requested_selected: Rc<RefCell<Option<String>>>,
    /// Shown when a search hides every card. Kept as one persistent widget
    /// rather than rebuilt, so toggling it costs nothing on each keystroke.
    no_matches: adw::StatusPage,
    /// Invoked by the "no servers yet" page; the window routes it to the
    /// Subscriptions page.
    on_browse_subscriptions: BrowseCallback,
    /// Which subscription blocks are collapsed, persisted to disk so it
    /// survives restarts.
    prefs: Rc<RefCell<GuiPrefs>>,
}

impl ServersView {
    pub fn new(subscriptions: &[Subscription]) -> Self {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 20);
        content.set_hexpand(true);
        content.set_margin_top(16);
        content.set_margin_bottom(20);
        content.set_margin_start(18);
        content.set_margin_end(18);

        let viewport = gtk::Viewport::builder()
            .child(&content)
            .hscroll_policy(gtk::ScrollablePolicy::Minimum)
            .hexpand(true)
            .build();
        let root = gtk::ScrolledWindow::builder()
            .child(&viewport)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_width(false)
            .vexpand(true)
            .build();
        let no_matches = adw::StatusPage::builder()
            .icon_name("system-search-symbolic")
            .title("No matching servers")
            .description("Try a different name, country, or protocol.")
            .vexpand(true)
            .visible(false)
            .build();

        let chip_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["group-chip-bar"])
            .build();
        // Horizontal scroll rather than wrapping: the row must not grow taller
        // as groups accumulate, and it must not become the page's minimum
        // width in a narrow window.
        let chip_scroll = gtk::ScrolledWindow::builder()
            .child(&chip_bar)
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .build();

        let chip_hint = gtk::Label::builder()
            .label("Servers you use together can be saved as a group — then one click connects to the whole set.")
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(1)
            .visible(false)
            .css_classes(["dim-label", "caption", "group-chip-hint"])
            .build();

        let connect_title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .build();
        let connect_button = adw::SplitButton::builder()
            .label("Connect")
            .css_classes(["suggested-action"])
            .build();
        let connect_save = gtk::Button::builder()
            .label("Save as group")
            .tooltip_text("Keep this selection as a chip you can come back to")
            .visible(false)
            .css_classes(["flat"])
            .build();
        // How wide the rotation is belongs beside Connect, because that is where
        // the pool is made. Leaving it to the profile editor meant every pool
        // the UI built rotated over everything it matched — forty-two nodes for
        // a country with nine distinct hosts.
        let connect_rotation = gtk::MenuButton::builder()
            .tooltip_text("How many nodes carry traffic at once")
            .css_classes(["flat"])
            .build();
        let connect_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .visible(false)
            .css_classes(["group-connect-bar"])
            .build();
        connect_bar.append(&connect_title);
        connect_bar.append(&connect_rotation);
        connect_bar.append(&connect_save);
        connect_bar.append(&connect_button);

        // A word, not a lone glyph. Testing found nobody guessing that an icon
        // in the header was the way to build a group, and the icon it used
        // (`funnel-symbolic`) is not an Adwaita name at all, so it drew as a
        // broken square. The funnel now travels with the app, and the label
        // carries the meaning even where the icon does not load.
        //
        // No `pan-down-symbolic` any more: that arrow promises a menu dropping
        // out of the button, and this opens a dialog. It was also a third widget
        // of width in the first thing on the row, next to a scope switcher that
        // needs the space.
        let filter_label = gtk::Label::new(Some("Filter"));
        let filter_content = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        filter_content.append(&gtk::Image::from_icon_name("oxidom-funnel-symbolic"));
        filter_content.append(&filter_label);
        let filter_button = gtk::Button::builder()
            .tooltip_text("Narrow the list by country, protocol or subscription")
            .css_classes(["pill", "group-chip", "filter-pill"])
            .build();
        filter_button.set_child(Some(&filter_content));
        filter_button.update_property(&[gtk::accessible::Property::Label("Filter servers")]);
        filter_button.update_property(&[gtk::accessible::Property::HasPopup(true)]);
        let servers_area = gtk::Box::new(gtk::Orientation::Vertical, 20);
        servers_area.set_hexpand(true);

        let view = Self {
            root,
            content,
            servers_area,
            no_matches,
            checking: Rc::new(RefCell::new(HashSet::new())),
            // Assumed absent until told otherwise: an unasked question must not
            // paint a control that cannot work.
            can_cancel_probes: Rc::new(Cell::new(false)),
            chip_bar,
            chip_scroll,
            chip_hint,
            connect_bar,
            connect_title,
            connect_button,
            connect_save,
            connect_strategy: Rc::new(Cell::new(0)),
            connect_rotation_value: Rc::new(Cell::new(oxidom_core::pool::DEFAULT_POOL_ROTATION)),
            connect_rotation,
            filter_button,
            filter_label,
            scope_members: Rc::new(RefCell::new(Vec::new())),
            active_group: Rc::new(RefCell::new(None)),
            saved_groups: Rc::new(RefCell::new(Vec::new())),
            scopes: Rc::new(RefCell::new(None)),
            syncing_scopes: Rc::new(Cell::new(false)),
            switching_scope: Rc::new(Cell::new(false)),
            on_create_pool: Rc::new(RefCell::new(None)),
            on_connect_pool: Rc::new(RefCell::new(None)),
            on_browse_subscriptions: Rc::new(RefCell::new(None)),
            cards: Rc::new(RefCell::new(HashMap::new())),
            blocks: Rc::new(RefCell::new(Vec::new())),
            subscriptions: Rc::new(RefCell::new(subscriptions.to_vec())),
            search_texts: Rc::new(RefCell::new(HashMap::new())),
            query: Rc::new(RefCell::new(String::new())),
            filter_countries: Rc::new(RefCell::new(Vec::new())),
            filter_protocols: Rc::new(RefCell::new(Vec::new())),
            filter_subscriptions: Rc::new(RefCell::new(Vec::new())),
            filter_exclude: Rc::new(RefCell::new(Vec::new())),
            current_filter: Rc::new(RefCell::new(PoolQuery::default())),
            columns: Rc::new(Cell::new(1)),
            pending_columns: Rc::new(Cell::new(1)),
            column_update_scheduled: Rc::new(Cell::new(false)),
            resize_generation: Rc::new(Cell::new(0)),
            latencies: Rc::new(RefCell::new(HashMap::new())),
            selected: Rc::new(RefCell::new(None)),
            requested_selected: Rc::new(RefCell::new(None)),
            prefs: Rc::new(RefCell::new(GuiPrefs::load(subscriptions))),
        };
        // Once, not per rebuild: the split button outlives every rebuild, and
        // re-connecting its handlers would make the Nth click perform N
        // connections. The filter pill is the same widget for the same reason —
        // it is only unparented and re-appended when the row is rebuilt.
        view.build_connect_button();
        view.filter_button.connect_clicked({
            let view = view.clone();
            move |_| view.present_selection_dialog(SelectionIntent::Filter)
        });
        view
    }

    /// Pick the column count from the width the window gives this view.
    /// Driven from window.rs — deriving it from our own allocation would form
    /// a feedback loop with the content's minimum width and deadlock the
    /// window's ability to shrink.
    /// The one loaded copy of `gui_prefs.toml`, shared rather than re-read.
    ///
    /// Every writer saves the whole struct, so a second copy loaded elsewhere
    /// would silently overwrite anything the first changed after loading.
    pub fn prefs(&self) -> Rc<RefCell<GuiPrefs>> {
        self.prefs.clone()
    }

    pub fn set_available_width(&self, width: i32) {
        let usable = width
            .saturating_sub(self.content.margin_start())
            .saturating_sub(self.content.margin_end());
        let columns = columns_for_width_with_hysteresis(
            usable,
            self.pending_columns.get(),
            COLUMN_HYSTERESIS,
        );
        if columns != self.columns.get() {
            self.schedule_columns(columns);
        }
        self.schedule_expanded_remeasure();
    }

    fn schedule_columns(&self, count: usize) {
        self.pending_columns.set(count);
        if self.column_update_scheduled.replace(true) {
            return;
        }
        let view = self.clone();
        glib::idle_add_local_once(move || {
            view.column_update_scheduled.set(false);
            view.set_columns(view.pending_columns.get());
        });
    }

    fn set_columns(&self, count: usize) {
        let count = count.clamp(1, 4);
        if self.columns.get() == count {
            return;
        }
        self.columns.set(count);
        self.pending_columns.set(count);
        for block in self.blocks.borrow().iter() {
            repack_block(block, count);
        }
        self.refresh_expanded_height();
        self.schedule_expanded_remeasure();
    }

    /// `latency_states` carries everything a badge needs, already decided by
    /// `reduce`: ids it does not mention have nothing measured. The view used to
    /// re-derive that from a latency map plus a set of in-flight ids, which is
    /// how a rebuilt card and a live one could end up disagreeing.
    pub fn rebuild(
        &self,
        subscriptions: &[Subscription],
        connected_id: Option<&str>,
        connected_profiles: &HashMap<String, ServerProfiles>,
        selected_id: Option<&str>,
        latency_states: &HashMap<String, LatencyState>,
        callbacks: CardCallbacks,
    ) {
        // The daemon's order is the subscriptions' creation order; the user's
        // arrangement is a display preference layered on top of it, so every
        // rebuild re-applies it rather than the widget order being the only
        // record of it.
        let ordered = ordered_subscriptions(subscriptions, &self.prefs.borrow().subscription_order);
        let subscriptions = &ordered[..];
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        // The area survives the page rebuild as a widget, so its opacity — which
        // an interrupted fade could have left below 1.0 — is reset rather than
        // inherited.
        while let Some(child) = self.servers_area.first_child() {
            self.servers_area.remove(&child);
        }
        self.servers_area.set_opacity(1.0);
        self.cards.borrow_mut().clear();
        self.blocks.borrow_mut().clear();
        *self.subscriptions.borrow_mut() = subscriptions.to_vec();
        self.search_texts.borrow_mut().clear();
        *self.on_create_pool.borrow_mut() = Some(callbacks.create_pool.clone());
        *self.on_connect_pool.borrow_mut() = Some(callbacks.connect_pool.clone());
        *self.latencies.borrow_mut() = latency_states
            .iter()
            .filter_map(|(id, state)| sort_value(*state).map(|value| (id.clone(), value)))
            .collect();
        *self.selected.borrow_mut() = selected_id.map(str::to_string);
        *self.requested_selected.borrow_mut() = selected_id.map(str::to_string);

        if subscriptions.is_empty() {
            let empty = adw::StatusPage::builder()
                .icon_name("network-server-symbolic")
                .title("No servers yet")
                .description("Add a subscription to start browsing servers.")
                .vexpand(true)
                .build();
            // Without this the user has to discover the Subscriptions page
            // unaided; an empty state that names the next step should offer it.
            let action = gtk::Button::builder()
                .label("Add a subscription")
                .halign(gtk::Align::Center)
                .css_classes(["suggested-action", "pill"])
                .build();
            action.connect_clicked({
                let callback = self.on_browse_subscriptions.clone();
                move |_| {
                    if let Some(callback) = callback.borrow().as_ref() {
                        callback();
                    }
                }
            });
            empty.set_child(Some(&action));
            self.content.append(&empty);
            return;
        }

        self.content.append(&self.chip_scroll);
        self.content.append(&self.chip_hint);
        self.content.append(&self.connect_bar);
        // The cards live in their own box so a scope change can cross-fade *them*
        // and nothing else. Fading `content` faded the scope row, the filter pill
        // and the Connect bar along with them, which read as the whole window
        // blinking — and the controls that did not change are exactly the ones
        // that must stay put while the thing they control is replaced.
        self.content.append(&self.servers_area);
        self.build_chip_bar();
        let favourites: HashSet<String> = self
            .prefs
            .borrow()
            .groups
            .iter()
            .find(|group| group.id == FAVOURITES_ID)
            .map(|group| group.query.members.iter().cloned().collect())
            .unwrap_or_default();

        for subscription in subscriptions {
            let heading = gtk::Label::builder()
                .label(&subscription.name)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .single_line_mode(true)
                .css_classes(["title-2"])
                .build();

            let ids: Vec<String> = subscription.servers.iter().map(|s| s.id.clone()).collect();
            // Update = re-fetch the subscription; speed = re-measure every server.
            // Icon buttons sit right beside the name, Happ-style.
            let update = gtk::Button::builder()
                .icon_name("view-refresh-symbolic")
                .tooltip_text("Update subscription")
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular", "server-action"])
                .build();
            update.update_property(&[gtk::accessible::Property::Label("Update subscription")]);
            // The local "My servers" subscription has no URL to re-fetch.
            update.set_visible(!subscription.url.is_empty());
            update.connect_clicked({
                let cb = callbacks.refresh.clone();
                let id = subscription.id.clone();
                move |_| cb(id.clone())
            });
            let speed = gtk::Button::builder()
                .icon_name(sweep_icon(false))
                .tooltip_text(sweep_label(false))
                .valign(gtk::Align::Center)
                .sensitive(!ids.is_empty())
                .css_classes(["flat", "circular", "server-action"])
                .build();
            speed.update_property(&[gtk::accessible::Property::Label(sweep_label(false))]);
            speed.connect_clicked({
                let cb = callbacks.recheck.clone();
                let ids = ids.clone();
                move |_| cb(ids.clone())
            });
            // Sort is a pure view action, so wire it straight to the view.
            let sort = gtk::Button::builder()
                .icon_name("view-sort-ascending-symbolic")
                .tooltip_text("Sort by latency")
                .valign(gtk::Align::Center)
                .sensitive(!ids.is_empty())
                .css_classes(["flat", "circular", "server-action"])
                .build();
            sort.update_property(&[gtk::accessible::Property::Label("Sort by latency")]);
            sort.connect_clicked({
                let view = self.clone();
                let subscription_id = subscription.id.clone();
                move |_| view.sort_subscription(&subscription_id)
            });
            let reorder = gtk::MenuButton::builder()
                .icon_name("view-more-symbolic")
                .tooltip_text("Move subscription")
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular", "server-action"])
                .build();
            reorder.update_property(&[gtk::accessible::Property::Label("Move subscription")]);
            reorder.set_popover(Some(&self.move_subscription_menu(&subscription.id)));
            let collapsed = Rc::new(Cell::new(
                self.prefs
                    .borrow()
                    .collapsed_subscriptions
                    .contains(&subscription.id),
            ));
            let collapse_toggle = gtk::Button::builder()
                .icon_name(collapse_icon(collapsed.get()))
                .tooltip_text(collapse_tooltip(collapsed.get()))
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular", "server-action"])
                .build();
            collapse_toggle.update_property(&[gtk::accessible::Property::Label(collapse_tooltip(
                collapsed.get(),
            ))]);

            let description = gtk::Label::builder()
                .label(subscription_description(subscription))
                .xalign(0.0)
                .max_width_chars(1)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .css_classes(["dim-label"])
                .build();
            let title_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
            title_box.set_hexpand(true);
            title_box.append(&heading);
            title_box.append(&description);
            // The name and the quota line are the block's whole width minus four
            // icon buttons, and hitting a 24px chevron to fold a subscription away
            // was the only way to do it. They become the expander instead; the
            // chevron stays because it is what says the block folds at all.
            let title_toggle = gtk::Button::builder()
                .child(&title_box)
                .hexpand(true)
                .css_classes(["flat", "subscription-toggle"])
                .build();

            let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            actions.set_valign(gtk::Align::Center);
            actions.append(&update);
            actions.append(&speed);
            actions.append(&sort);
            actions.append(&reorder);
            actions.append(&collapse_toggle);

            let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            header.set_hexpand(true);
            header.append(&title_toggle);
            header.append(&actions);

            let columns_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(CARD_COLUMN_SPACING)
                .homogeneous(true)
                .hexpand(true)
                .visible(!collapsed.get())
                .build();
            // One toggle behind two widgets, so the chevron and the title can
            // never disagree about whether the block is folded.
            let toggle: Rc<dyn Fn()> = {
                let collapsed = collapsed.clone();
                let columns_box = columns_box.clone();
                let collapse_toggle = collapse_toggle.clone();
                let title_toggle = title_toggle.clone();
                let prefs = self.prefs.clone();
                let subscription_id = subscription.id.clone();
                Rc::new(move || {
                    let now_collapsed = !collapsed.get();
                    collapsed.set(now_collapsed);
                    columns_box.set_visible(!now_collapsed);
                    collapse_toggle.set_icon_name(collapse_icon(now_collapsed));
                    collapse_toggle.set_tooltip_text(Some(collapse_tooltip(now_collapsed)));
                    collapse_toggle.update_property(&[gtk::accessible::Property::Label(
                        collapse_tooltip(now_collapsed),
                    )]);
                    title_toggle
                        .update_state(&[gtk::accessible::State::Expanded(Some(!now_collapsed))]);
                    let mut prefs = prefs.borrow_mut();
                    if now_collapsed {
                        prefs
                            .collapsed_subscriptions
                            .insert(subscription_id.clone());
                    } else {
                        prefs.collapsed_subscriptions.remove(&subscription_id);
                    }
                    if let Err(error) = prefs.save() {
                        log::warn!("could not save gui prefs: {error:#}");
                    }
                })
            };
            title_toggle.update_state(&[gtk::accessible::State::Expanded(Some(!collapsed.get()))]);
            title_toggle.connect_clicked({
                let toggle = toggle.clone();
                move |_| toggle()
            });
            collapse_toggle.connect_clicked(move |_| toggle());

            let mut block_cards: Vec<(String, gtk::Widget)> = Vec::new();
            for server in &subscription.servers {
                let id = server.id.clone();
                let latency_state = latency_states
                    .get(&id)
                    .copied()
                    .unwrap_or(LatencyState::Unmeasured);
                let on_select = {
                    let cb = callbacks.select.clone();
                    let id = id.clone();
                    move || cb(id.clone())
                };
                let on_ping = {
                    let cb = callbacks.ping.clone();
                    let id = id.clone();
                    move || cb(id.clone())
                };
                let on_trust = {
                    let cb = callbacks.trust.clone();
                    let id = id.clone();
                    move || cb(id.clone())
                };
                let on_activate = {
                    let cb = callbacks.activate.clone();
                    let id = id.clone();
                    move || cb(id.clone())
                };
                let on_set_alias = {
                    let cb = callbacks.set_alias.clone();
                    let id = id.clone();
                    move |alias| cb(id.clone(), alias)
                };
                let on_show_logs = {
                    let cb = callbacks.show_logs.clone();
                    let id = id.clone();
                    move || cb(id.clone())
                };
                let on_report = {
                    let cb = callbacks.report.clone();
                    let id = id.clone();
                    move || cb(id.clone())
                };
                let connection_state = match (connected_profiles.get(&id), connected_id) {
                    (Some(profiles), _) if !profiles.connected.is_empty() => {
                        CardConnectionState::ConnectedHere
                    }
                    (Some(profiles), _) if !profiles.in_pool.is_empty() => {
                        CardConnectionState::InPool
                    }
                    (None, Some(connected_id)) if connected_id == id => {
                        CardConnectionState::ConnectedHere
                    }
                    (_, Some(_)) => CardConnectionState::ConnectedElsewhere,
                    _ => CardConnectionState::Disconnected,
                };
                let card = ServerCard::new(
                    server,
                    connection_state,
                    latency_state,
                    favourites.contains(&id),
                    CardHandlers {
                        select: Rc::new(on_select),
                        activate: Rc::new(on_activate),
                        ping: Rc::new(on_ping),
                        trust: Rc::new(on_trust),
                        set_alias: Rc::new(on_set_alias),
                        toggle_favourite: {
                            let view = self.clone();
                            let id = id.clone();
                            Rc::new(move || view.toggle_favourite(&id))
                        },
                        show_logs: Rc::new(on_show_logs),
                        report: Rc::new(on_report),
                    },
                );
                let mut tooltip = format!(
                    "{} · {}:{} · {}",
                    server.name,
                    server.address,
                    server.port,
                    server.protocol.as_str(),
                );
                if let Some(country) = server.country.as_deref() {
                    tooltip.push_str(" · ");
                    tooltip.push_str(country);
                }
                card.root.set_tooltip_text(Some(&tooltip));
                self.search_texts.borrow_mut().insert(
                    id.clone(),
                    format!(
                        "{} {} {} {}:{} {}",
                        server.name,
                        server.transport_label,
                        server.protocol.as_str(),
                        server.address,
                        server.port,
                        server.country.as_deref().unwrap_or(""),
                    )
                    .to_lowercase(),
                );
                block_cards.push((id.clone(), card.root.clone().upcast::<gtk::Widget>()));
                self.cards.borrow_mut().insert(server.id.clone(), card);
            }

            let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
            root.set_hexpand(true);
            root.append(&header);
            root.append(&columns_box);
            self.servers_area.append(&root);
            let block = SubscriptionBlock {
                id: subscription.id.clone(),
                root: root.upcast::<gtk::Widget>(),
                columns_box,
                column_boxes: Rc::new(RefCell::new(Vec::new())),
                display_order: Rc::new(RefCell::new(
                    block_cards.iter().map(|(id, _)| id.clone()).collect(),
                )),
                cards: block_cards,
                sort_button: sort,
                speed_button: speed,
                checking: Rc::new(Cell::new(0)),
                sort_generation: Rc::new(Cell::new(0)),
            };
            repack_block(&block, self.columns.get());
            self.blocks.borrow_mut().push(block);
        }
        self.sync_probing(latency_states);
        // Last child, so it sits below the blocks it stands in for.
        self.servers_area.append(&self.no_matches);
        self.apply_filter();
        if let Some(server_id) = selected_id {
            self.set_selected_immediately(server_id);
        }
        self.schedule_expanded_remeasure();
    }

    /// The row of saved scopes: Filter, All, one chip per group, then "+".
    ///
    /// A group is a *scope*, not a place servers live. Rendering it as another
    /// block of cards would show the same server two or three times over and
    /// leave no way to tell which card was the real one; narrowing the single
    /// list keeps every server in exactly one place — its subscription.
    fn build_chip_bar(&self) {
        self.syncing_scopes.set(true);
        while let Some(child) = self.chip_bar.first_child() {
            self.chip_bar.remove(&child);
        }
        let active = self.active_group.borrow().clone();
        let saved = self.prefs.borrow().groups.clone();
        *self.saved_groups.borrow_mut() = saved.clone();
        self.scopes.borrow_mut().take();

        // The filter sits at the head of the row it feeds: everything to its
        // right is a scope that was once made with it. Unparented rather than
        // rebuilt, because it is one long-lived widget carrying the field count.
        self.filter_button.unparent();
        self.chip_bar.append(&self.filter_button);
        self.chip_bar
            .append(&gtk::Separator::new(gtk::Orientation::Vertical));

        // One `AdwToggleGroup` rather than a row of `GtkToggleButton`s.
        //
        // Three things come with it that the loose row had to fake or went
        // without: the radio behaviour (a group always has exactly one active
        // toggle, so "clicking the active chip must not clear the row" stops
        // being a special case), arrow-key navigation between scopes, and an
        // indicator that *slides* from the old scope to the new one — which is
        // the animation this row wanted, at no cost in code.
        let scopes = adw::ToggleGroup::new();
        scopes.add(
            adw::Toggle::builder()
                .name("")
                .label("All")
                .tooltip("Every server from every subscription")
                .build(),
        );
        for group in &saved {
            let count = group_member_ids(group, &self.subscriptions.borrow()).len();
            scopes.add(
                adw::Toggle::builder()
                    .name(&group.id)
                    .label(group.label())
                    .tooltip(group_chip_tooltip(group, count))
                    .build(),
            );
        }
        scopes.set_active_name(Some(active.as_deref().unwrap_or("")));
        scopes.connect_active_name_notify({
            let view = self.clone();
            move |scopes| {
                if view.syncing_scopes.get() {
                    return;
                }
                let name = scopes.active_name().unwrap_or_default();
                view.select_group(Some(name.as_str()).filter(|name| !name.is_empty()));
            }
        });
        *self.scopes.borrow_mut() = Some(scopes.clone());
        self.chip_bar.append(&scopes);

        // The `⋮` used to be a second half welded onto the selected chip. A
        // toggle inside an `AdwToggleGroup` cannot carry one, and it turns out
        // not to want to: one menu beside the group, acting on whatever is
        // selected, is a target that stays in the same place instead of moving
        // with the selection — and it removes the rule that only the selected
        // chip has a menu, which was itself only there to stop five identical
        // buttons appearing in a row.
        let manage = gtk::MenuButton::builder()
            .tooltip_text("What to do with the selection on screen")
            .css_classes(["flat", "group-chip-menu"])
            .build();
        manage.set_child(Some(&gtk::Image::from_icon_name("view-more-symbolic")));
        manage.update_property(&[gtk::accessible::Property::Label("Selection actions")]);
        // Never insensitive, because it no longer means "the selected group": it
        // acts on the scope that is on screen, saved or not, and making a profile
        // out of that is exactly what somebody looking at an unsaved filter wants.
        // The items that need a saved group disable themselves instead.
        let position = active
            .as_deref()
            .and_then(|id| saved.iter().position(|group| group.id == id));
        manage.set_popover(Some(&self.scope_menu(
            position.map(|index| (saved[index].clone(), index, saved.len())),
        )));
        self.chip_bar.append(&manage);
        self.sync_chip_modified();

        // Labelled, not a bare "+": a circle with a plus in it was read as
        // "add a subscription" by everyone who was asked, and the word is the
        // only thing that says what gets added.
        let add_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        add_content.append(&gtk::Image::from_icon_name("list-add-symbolic"));
        add_content.append(&gtk::Label::new(Some("New group")));
        let add = gtk::Button::builder()
            .child(&add_content)
            .tooltip_text("Pick servers you use together and give them a name")
            .css_classes(["pill", "group-chip", "group-chip-add"])
            .build();
        add.update_property(&[gtk::accessible::Property::Label("New group")]);
        add.connect_clicked({
            let view = self.clone();
            move |_| view.present_selection_dialog(SelectionIntent::Name)
        });
        self.chip_bar.append(&add);

        // Until the first group exists, the row is two chips whose purpose is
        // not obvious from their labels alone. One line says what the row is
        // for; it goes away as soon as the answer is on screen.
        self.chip_hint
            .set_visible(saved.iter().all(|group| group.id == FAVOURITES_ID));
        self.syncing_scopes.set(false);
    }

    /// Everything that can be done to one group, hung off its own chip.
    ///
    /// This used to be a row inside the filter popover, on the reasoning that
    /// the popover is where a scope is worked on and a chip is already a click
    /// target. Both halves turned out to be wrong: the popover was behind a
    /// header button nobody found, and "already a click target" only rules out
    /// a second gesture on the same target — a menu button beside it is its own
    /// target and cannot be hit by accident.
    ///
    /// `group` is `None` for "All" and for an unsaved filter: the menu is still
    /// offered, because "make a profile out of what I am looking at" is the one
    /// action that does not need the scope to have a name. It used to be a third
    /// button in the filter popover's footer, where three buttons had turned that
    /// footer into a menu pretending to be an action bar.
    fn scope_menu(&self, group: Option<(ServerGroup, usize, usize)>) -> gtk::Popover {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        list.set_margin_top(6);
        list.set_margin_bottom(6);
        list.set_margin_start(6);
        list.set_margin_end(6);
        let popover = gtk::Popover::builder().child(&list).build();

        // A `Button`'s own label centres itself, which reads as a row of
        // headings rather than a menu; an explicit child left-aligns it.
        let item = |label: &str, tooltip: &str| {
            gtk::Button::builder()
                .child(&gtk::Label::builder().label(label).xalign(0.0).build())
                .tooltip_text(tooltip)
                .css_classes(["flat"])
                .build()
        };

        // Named for what it makes. "Create pool…" was a third word for a thing
        // that already had two — a group is a pool, and connecting one runs it —
        // and the only thing that actually distinguished it was that it produces
        // a *profile*, which is what it now says.
        let create_pool = item(
            "New profile from this…",
            "Create a connection profile whose servers are the visible selection. Connect \
             runs the selection without saving anything; this is how it is kept.",
        );
        create_pool.connect_clicked({
            let view = self.clone();
            let popover = popover.clone();
            move |_| {
                popover.popdown();
                let query = view.current_filter.borrow().clone();
                if let Some(callback) = view.on_create_pool.borrow().as_ref() {
                    callback(query);
                }
            }
        });
        list.append(&create_pool);

        let Some((group, index, total)) = group else {
            // Nothing else in this menu means anything without a saved group,
            // and a row of five insensitive items is worse than a short menu.
            return popover;
        };

        list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let edit = item(
            "Edit…",
            "Rename it, change its icon, or pick different servers",
        );
        edit.connect_clicked({
            let view = self.clone();
            let group = group.clone();
            let popover = popover.clone();
            move |_| {
                popover.popdown();
                view.present_selection_dialog(SelectionIntent::Edit(Box::new(group.clone())));
            }
        });
        list.append(&edit);

        // Only useful while the chip carries its "·": otherwise it would
        // overwrite the group with itself. Decided when the menu opens rather
        // than when the row is built, because the search box moves this without
        // rebuilding the row.
        let update = item(
            "Update to what's shown",
            "Replace this group's servers with the ones on screen",
        );
        update.connect_clicked({
            let view = self.clone();
            let group = group.clone();
            let popover = popover.clone();
            move |_| {
                popover.popdown();
                // A list takes the servers on screen; a rule takes the filters
                // that put them there, because a rule that froze them would stop
                // being a rule.
                let query = match group.kind {
                    GroupKind::List => PoolQuery {
                        name: group.name.clone(),
                        members: filtered_ids(
                            &view.current_filter.borrow(),
                            &view.subscriptions.borrow(),
                        ),
                        ..PoolQuery::default()
                    },
                    GroupKind::Rule => view.rule_from_filters(group.name.clone()),
                };
                view.save_group(
                    Some(group.clone()),
                    group.name.clone(),
                    group.icon.clone(),
                    group.kind,
                    query,
                );
            }
        });
        popover.connect_show({
            let view = self.clone();
            let group = group.clone();
            let update = update.clone();
            move |_| update.set_sensitive(view.group_is_modified(&group))
        });
        list.append(&update);

        list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        for (label, tooltip, delta, enabled) in [
            (
                "Move left",
                "Move this chip towards the start",
                -1_isize,
                index > 0,
            ),
            (
                "Move right",
                "Move this chip towards the end",
                1,
                index + 1 < total,
            ),
        ] {
            let move_item = item(label, tooltip);
            move_item.set_sensitive(enabled);
            move_item.connect_clicked({
                let view = self.clone();
                let id = group.id.clone();
                let popover = popover.clone();
                move |_| {
                    popover.popdown();
                    view.move_chip(&id, delta);
                }
            });
            list.append(&move_item);
        }

        list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let delete = item(
            "Remove",
            // Favourites is where the star puts things, so removing it would
            // leave the star with nowhere to go. Emptying it is always
            // possible, and is what "delete" would have meant.
            if group.id == FAVOURITES_ID {
                "Favourites is built in. Unstar its servers to empty it."
            } else {
                "Remove this group. The servers in it are not touched."
            },
        );
        delete.add_css_class("destructive-action");
        delete.set_sensitive(group.id != FAVOURITES_ID);
        delete.connect_clicked({
            let view = self.clone();
            let group = group.clone();
            let popover = popover.clone();
            move |_| {
                popover.popdown();
                view.delete_group_dialog(group.clone());
            }
        });
        list.append(&delete);

        popover
    }

    /// Whether the visible selection has drifted from the group as saved.
    ///
    /// A list is compared against what it currently resolves to, not against
    /// the handles it stores: a member whose server went away is not an edit
    /// the user made, and counting it would blame them for a refresh.
    fn group_is_modified(&self, group: &ServerGroup) -> bool {
        let mut basis = group.clone();
        if basis.kind == GroupKind::List {
            basis.query.members = group_member_ids(group, &self.subscriptions.borrow());
        }
        !query_equals_group(&self.current_filter.borrow(), &basis)
    }

    /// Wire the split button: a click connects with the strategy last chosen,
    /// the arrow offers the others.
    ///
    /// This is the bar the whole redesign exists for. Before it, the only way
    /// to run a pool was to open the profile editor, switch Selection to Pool
    /// and fill in eight fields — an expert path for a request that is simply
    /// "don't make me pick a server".
    fn build_connect_button(&self) {
        self.connect_button.connect_clicked({
            let view = self.clone();
            move |_| view.connect_active_group()
        });
        self.connect_save.connect_clicked({
            let view = self.clone();
            move |_| view.present_selection_dialog(SelectionIntent::Name)
        });
        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        list.set_margin_top(6);
        list.set_margin_bottom(6);
        list.set_margin_start(6);
        list.set_margin_end(6);
        let popover = gtk::Popover::builder().child(&list).build();
        for (index, choice) in connect_choices().into_iter().enumerate() {
            let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
            text.append(
                &gtk::Label::builder()
                    .label(choice.label)
                    .xalign(0.0)
                    .build(),
            );
            text.append(
                &gtk::Label::builder()
                    .label(choice.detail)
                    .xalign(0.0)
                    .max_width_chars(34)
                    .wrap(true)
                    .css_classes(["dim-label", "caption"])
                    .build(),
            );
            let button = gtk::Button::builder()
                .child(&text)
                .css_classes(["flat"])
                .build();
            button.connect_clicked({
                let view = self.clone();
                let popover = popover.clone();
                move |_| {
                    popover.popdown();
                    // Picking from the menu both chooses and acts, so the arrow
                    // is never a settings menu the user has to press twice.
                    view.connect_strategy.set(index);
                    view.connect_active_group();
                }
            });
            list.append(&button);
        }
        self.connect_button.set_popover(Some(&popover));
        self.build_rotation_menu();
    }

    /// The rotation picker beside Connect.
    ///
    /// Built once, like the strategy menu: the bar outlives every rebuild, and
    /// hooking it up per rebuild is how the Nth click came to fire N times.
    fn build_rotation_menu(&self) {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        list.set_margin_top(6);
        list.set_margin_bottom(6);
        list.set_margin_start(6);
        list.set_margin_end(6);
        let popover = gtk::Popover::builder().child(&list).build();
        for value in ROTATION_CHOICES {
            let button = gtk::Button::builder()
                .label(rotation_label(value))
                .css_classes(["flat"])
                .build();
            button.set_tooltip_text(Some(&rotation_detail(value)));
            button.connect_clicked({
                let view = self.clone();
                let popover = popover.clone();
                move |_| {
                    popover.popdown();
                    view.connect_rotation_value.set(value);
                    view.sync_connect_bar();
                }
            });
            list.append(&button);
        }
        self.connect_rotation.set_popover(Some(&popover));
    }

    /// Hand the visible selection to the window as a pool.
    fn connect_active_group(&self) {
        let Some(query) = self.connect_query() else {
            return;
        };
        // Resolved here, where the subscriptions already are, and for the same
        // reason the bar's count is: what Connect runs is what the page shows.
        let members = filtered_ids(&query, &self.subscriptions.borrow());
        if let Some(callback) = self.on_connect_pool.borrow().as_ref() {
            callback(query, members);
        }
    }

    /// What Connect would run: exactly what is on screen, under a name and the
    /// chosen strategy.
    ///
    /// Deliberately the *visible* selection rather than the group as saved. The
    /// chip already marks itself when the two differ, and connecting something
    /// other than what the page is showing would make that mark a lie. With no
    /// group selected the name comes from the rule, and `PoolQuery.name` is a
    /// label only — it takes no part in selection and does not make a session
    /// stale — so an unsaved scope connects on the same footing as a saved one.
    fn connect_query(&self) -> Option<PoolQuery> {
        let name = match self.active_group_value() {
            Some(group) => group.name,
            None if self.active_filter_fields() > 0 => {
                suggested_group_name(&self.rule_from_filters(String::new()))
            }
            None => return None,
        };
        let mut query = self.current_filter.borrow().clone();
        if filtered_ids(&query, &self.subscriptions.borrow()).is_empty() {
            return None;
        }
        query.name = name;
        query.strategy = connect_choices()
            .get(self.connect_strategy.get())
            .map(|choice| choice.strategy)
            .unwrap_or_default();
        query.expected = self.connect_rotation_value.get();
        Some(query)
    }

    /// The bar appears for a saved chip, and for a filter the user set by hand.
    ///
    /// Not for a bare search: the filter fields are a deliberate act that stays
    /// put, while search text changes on every keystroke, and a strip that
    /// appears and vanishes under the pointer moves the cards out from under
    /// the click that was aimed at them. Hence `active_filter_fields`, which
    /// does not consult the search box.
    fn sync_connect_bar(&self) {
        let subscriptions = self.subscriptions.borrow().clone();
        let visible = filtered_ids(&self.current_filter.borrow(), &subscriptions).len();
        let unsaved = self.active_group_value().is_none();
        let title = match self.active_group_value() {
            Some(group) => {
                let saved = group_member_ids(&group, &subscriptions).len();
                // "8 of 12" rather than a bare "8" whenever the search box is
                // hiding part of the group: Connect acts on the eight, and the
                // difference is the only warning that it will.
                let count = if visible == saved {
                    format!("{visible} server{}", if visible == 1 { "" } else { "s" })
                } else {
                    format!("{visible} of {saved} shown")
                };
                format!("{} · {count}", group.label())
            }
            None if self.active_filter_fields() > 0 => format!(
                "{} · {visible} server{}",
                describe_rule(&self.rule_from_filters(String::new())),
                if visible == 1 { "" } else { "s" }
            ),
            None => {
                self.connect_bar.set_visible(false);
                return;
            }
        };
        self.connect_title.set_label(&title);
        let rotation = self.connect_rotation_value.get();
        self.connect_rotation
            .set_label(&rotation_summary(rotation, visible));
        // Only worth choosing when there is something to choose between: over
        // one or two nodes every width is the same rotation.
        self.connect_rotation.set_sensitive(visible > 2);
        let choice = connect_choices()
            .into_iter()
            .nth(self.connect_strategy.get());
        self.connect_button.set_sensitive(visible > 0);
        self.connect_button
            .set_tooltip_text(Some(&match (visible, choice.as_ref()) {
                (0, _) => "Nothing to connect: this selection shows no servers.".to_string(),
                // No profile is named because none is touched. The strategy's
                // own sentence ends in a full stop; the one before it has to as
                // well, or the two run together into a line with no punctuation
                // between them.
                (_, Some(choice)) => format!(
                    "Run these {visible} servers now, without saving anything. {}: {}",
                    choice.label, choice.detail
                ),
                (_, None) => format!("Run these {visible} servers now, without saving anything."),
            }));
        self.connect_save.set_visible(unsaved);
        self.connect_save.set_sensitive(visible > 0);
        self.connect_bar.set_visible(true);
    }

    /// One editor for what is selected, whether or not it ends up with a name.
    ///
    /// There used to be two dialogs. "Filter" said which servers to show, in the
    /// language of a rule plus a hand-picked list; "Save as group" said the same
    /// thing again with its own second picker, plus a name, an icon and a
    /// List/Rule radio. Which door you came through decided which half of the
    /// vocabulary you got: picking five servers by hand in the filter left them
    /// unsaveable, and naming a group meant first finding a control that did
    /// something else. They are now the same form, and a name is simply an
    /// optional field on it.
    ///
    /// It is a dialog rather than a popover because the `Except` picker inside
    /// it was a second popover parented to the first. GTK4 gives an autohide
    /// popover a grab; opening another one inside it takes that grab away and
    /// leaves the outer popover believing it is still shown, so its button
    /// silently refuses to open it ever again. Measured behaviour — and as a
    /// dialog the pickers become pushed pages with a slide animation instead.
    ///
    /// The dialog edits a **draft** and commits on Apply or Save. Nothing behind
    /// a modal dialog can be watched changing, so applying each tick was work the
    /// user could not see; a form with one commit can also be abandoned.
    fn present_selection_dialog(&self, intent: SelectionIntent) {
        let subscriptions = self.subscriptions.borrow().clone();
        let editing = match &intent {
            SelectionIntent::Edit(group) => Some((**group).clone()),
            _ => None,
        };
        // Favourites is where the star puts servers, so it can hold nothing but
        // a list. The radio never excluded it: converting it to a rule left the
        // star writing members into a group that also had filters, which
        // `Profile::validate` refuses — so connecting to it failed afterwards.
        let list_only = editing
            .as_ref()
            .is_some_and(|group| group.id == FAVOURITES_ID);

        // Hand-picked servers are part of the draft, not a separate mode: this is
        // the whole selection — a rule, or a list, exactly as `PoolQuery` puts it.
        let draft = Rc::new(RefCell::new(match editing.as_ref() {
            // A rule's members are whatever it happens to match today, so
            // loading them would turn every edit of a rule into a freeze.
            Some(group) => FilterDraft {
                countries: group.query.countries.clone(),
                protocols: group.query.protocols.clone(),
                subscriptions: group.query.subscriptions.clone(),
                exclude: group.query.exclude.clone(),
                members: match group.kind {
                    GroupKind::List => group_member_ids(group, &subscriptions),
                    GroupKind::Rule => Vec::new(),
                },
            },
            None => FilterDraft {
                countries: self.filter_countries.borrow().clone(),
                protocols: self.filter_protocols.borrow().clone(),
                subscriptions: self.filter_subscriptions.borrow().clone(),
                exclude: self.filter_exclude.borrow().clone(),
                members: self.starting_members(&intent, &subscriptions),
            },
        }));

        let dialog = adw::Dialog::builder()
            .title(match editing.as_ref() {
                Some(group) => group.name.as_str(),
                None => "Selection",
            })
            .content_width(420)
            .content_height(620)
            .build();
        let navigation = adw::NavigationView::new();

        // Save is the obvious button where a group is being changed, Apply where
        // the page is. Both are always present: the whole point of one editor is
        // that naming a selection is not a different errand.
        let suggest = |wanted: bool| -> Vec<&str> {
            if wanted {
                vec!["suggested-action"]
            } else {
                Vec::new()
            }
        };
        let editing_group = editing.is_some();
        let apply = gtk::Button::builder()
            .label("Apply")
            .tooltip_text("Show this selection on the page, without saving it")
            .css_classes(suggest(!editing_group))
            .build();
        let save = gtk::Button::builder()
            .label("Save")
            .css_classes(suggest(editing_group))
            .build();
        let cancel = gtk::Button::with_label("Cancel");
        let header = adw::HeaderBar::builder()
            .show_end_title_buttons(false)
            .show_start_title_buttons(false)
            .build();
        header.pack_start(&cancel);
        header.pack_end(&save);
        header.pack_end(&apply);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
        body.set_margin_top(12);
        body.set_margin_bottom(12);
        body.set_margin_start(12);
        body.set_margin_end(12);

        // Two lists, because they are two answers to one question and only one of
        // them can be in force: a rule that keeps matching, or the servers named
        // by hand. `pool::resolve` already gives the named ones precedence, so the
        // dialog disables the rule rather than letting the user write something
        // that would be silently ignored.
        let rows = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        // One line where the match count and the List/Rule radio used to be two
        // controls saying overlapping things. It reports what is selected *and*
        // what saving it would mean, which is the answer the radio was asking
        // the user to supply.
        let matches = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .max_width_chars(44)
            .css_classes(["dim-label", "caption"])
            .build();

        // Optional, and first: a selection with a name is a group, and one
        // without is a filter. That is the whole difference, so it is one field
        // rather than a second dialog.
        let name_row = adw::EntryRow::builder().title("Name (optional)").build();
        // Just "Icon": the six presets take most of the row's width, so a longer
        // title ellipsises — and "optional" is already established by the field
        // above it.
        let icon_row = adw::EntryRow::builder().title("Icon").build();
        // A chip row of five identically shaped words is hard to aim at. One
        // glyph in front makes each one findable at a glance, which is the only
        // job a chip has. Free text, not a fixed palette: the presets are a
        // shortcut, not the vocabulary.
        let presets = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        presets.set_valign(gtk::Align::Center);
        for preset in ["★", "⚡", "🌍", "🏠", "🔒", "🎬"] {
            let button = gtk::Button::builder()
                .label(preset)
                .tooltip_text(format!("Use {preset}"))
                .css_classes(["flat", "circular"])
                .build();
            button.connect_clicked({
                let icon_row = icon_row.clone();
                // Pressing the one already chosen clears it, so a group can get
                // back to having no icon without selecting the text by hand.
                move |_| {
                    let next = if icon_row.text() == preset {
                        ""
                    } else {
                        preset
                    };
                    icon_row.set_text(next);
                }
            });
            presets.append(&button);
        }
        icon_row.add_suffix(&presets);
        match editing.as_ref() {
            Some(group) => {
                name_row.set_text(&group.name);
                icon_row.set_text(&group.icon);
            }
            // Only where the user asked for a group: prefilling a name on the
            // filter would offer to save something nobody set out to save.
            None if matches!(intent, SelectionIntent::Name) => {
                name_row.set_text(&suggested_group_name(
                    &draft.borrow().to_query(String::new(), GroupKind::Rule),
                ));
            }
            None => {}
        }

        let checks: Rc<RefCell<Vec<(FilterField, String, gtk::CheckButton)>>> =
            Rc::new(RefCell::new(Vec::new()));
        let field_rows: Rc<RefCell<Vec<(FilterField, adw::ExpanderRow)>>> =
            Rc::new(RefCell::new(Vec::new()));
        let step_row = |title: &str, subtitle: &str| {
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .activatable(true)
                .build();
            row.add_suffix(
                &gtk::Image::builder()
                    .icon_name("go-next-symbolic")
                    .css_classes(["dim-label"])
                    .build(),
            );
            row
        };
        let exclude_row = step_row("Except", &exclude_label(&draft.borrow().exclude));
        // The other half of "pick specific servers": `Except` says which ones to
        // drop out of a rule, and this says which ones are the whole selection.
        // Until now the only way to name servers by hand was the New group dialog,
        // so choosing five nodes meant first inventing a name for them.
        let only_row = step_row(
            "Only these servers",
            &picked_label(draft.borrow().members.len()),
        );
        let picked = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        picked.append(&only_row);
        // Set while the boxes are written from the draft: GTK emits `toggled` for
        // a programmatic `set_active` exactly as for a click, so without this each
        // write would run the read-back once per checkbox.
        let syncing = Rc::new(Cell::new(false));

        // One closure both directions of the sync go through, so the subtitles,
        // the summary, the draft and whether Save means anything cannot describe
        // different things.
        let refresh: Rc<dyn Fn()> = Rc::new({
            let checks = checks.clone();
            let field_rows = field_rows.clone();
            let exclude_row = exclude_row.clone();
            let only_row = only_row.clone();
            let rows = rows.clone();
            let matches = matches.clone();
            let draft = draft.clone();
            let subscriptions = subscriptions.clone();
            let view = self.clone();
            let name_row = name_row.clone();
            let save = save.clone();
            move || {
                let borrowed = checks.borrow();
                for (field, row) in field_rows.borrow().iter() {
                    let mut chosen = 0;
                    let mut total = 0;
                    for (candidate, _, check) in borrowed.iter() {
                        if candidate != field {
                            continue;
                        }
                        total += 1;
                        chosen += usize::from(check.is_active());
                    }
                    // A row that has to be opened to find out whether it is doing
                    // anything gets opened again and again.
                    row.set_subtitle(&match (chosen, total) {
                        (0, 0) => "None available".to_string(),
                        (0, total) => format!("Any of {total}"),
                        (chosen, total) => format!("{chosen} of {total}"),
                    });
                }
                drop(borrowed);
                let draft = draft.borrow();
                exclude_row.set_subtitle(&exclude_label(&draft.exclude));
                only_row.set_subtitle(&picked_label(draft.members.len()));
                // Named servers win over a rule wherever this is resolved, so the
                // rule is disabled rather than left writable and ignored.
                let by_hand = !draft.members.is_empty();
                rows.set_sensitive(!by_hand && !list_only);
                rows.set_tooltip_text(if list_only {
                    Some("Favourites holds the servers you star, so it has no rule.")
                } else {
                    by_hand.then_some(
                        "These servers are named by hand, so no filter applies. Clear them to \
                         go back to a rule.",
                    )
                });
                // The same computation the page will do on Apply, so the number
                // promised here is the number that appears.
                let count = if by_hand {
                    filtered_ids(
                        &PoolQuery {
                            members: draft.members.clone(),
                            ..PoolQuery::default()
                        },
                        &subscriptions,
                    )
                    .len()
                } else {
                    let query = filters_to_query(
                        &subscriptions,
                        &draft.subscriptions,
                        &draft.countries,
                        &draft.protocols,
                        &draft.exclude,
                        &view.search_texts.borrow(),
                        "",
                    );
                    filtered_ids(&query, &subscriptions).len()
                };
                matches.set_label(&selection_summary(&draft, count, list_only));
                // A greyed-out Save says "not ready yet"; a live one that
                // silently does nothing says the app is broken. An unnamed
                // selection is a filter, and an empty rule would mean every
                // server on the machine — which the "All" chip already is.
                // Favourites is exempt: it exists whether or not anything is
                // starred, so its icon and name are editable at any time.
                let named = !name_row.text().trim().is_empty();
                let anything = by_hand || draft.rule_fields() > 0 || list_only;
                save.set_sensitive(named && anything);
                save.set_tooltip_text(Some(match (named, anything) {
                    (false, _) => "Give the selection a name to keep it as a group",
                    (_, false) => "Pick some servers or set a filter first",
                    _ => "Keep this selection as a group in the chip row",
                }));
            }
        });
        name_row.connect_changed({
            let refresh = refresh.clone();
            move |_| refresh()
        });

        // Read every box rather than tracking the one that moved: the boxes are
        // the truth on screen, and rebuilding the draft from them cannot drift
        // from what the user sees.
        let read_back: Rc<dyn Fn()> = Rc::new({
            let checks = checks.clone();
            let draft = draft.clone();
            let syncing = syncing.clone();
            let refresh = refresh.clone();
            move || {
                if syncing.get() {
                    return;
                }
                {
                    let mut draft = draft.borrow_mut();
                    draft.countries.clear();
                    draft.protocols.clear();
                    draft.subscriptions.clear();
                    for (field, value, check) in checks.borrow().iter() {
                        if !check.is_active() {
                            continue;
                        }
                        match field {
                            FilterField::Country => draft.countries.push(value.clone()),
                            FilterField::Protocol => draft.protocols.push(value.clone()),
                            FilterField::Subscription => draft.subscriptions.push(value.clone()),
                        }
                    }
                }
                refresh();
            }
        });

        let countries = available_countries(&subscriptions)
            .into_iter()
            .map(|value| FilterOption {
                label: value.to_ascii_uppercase(),
                value,
            })
            .collect::<Vec<_>>();
        let protocols = available_protocols(&subscriptions)
            .into_iter()
            .map(|value| FilterOption {
                label: value.clone(),
                value,
            })
            .collect::<Vec<_>>();
        let groups = available_subscriptions(&subscriptions);

        // One boxed list instead of four buttons that each opened a popover of
        // their own. An expander row shows its choices in place.
        for (field, title, options) in [
            (FilterField::Country, "Country", &countries),
            (FilterField::Protocol, "Protocol", &protocols),
            (FilterField::Subscription, "Subscription", &groups),
        ] {
            let row = adw::ExpanderRow::builder()
                .title(title)
                .sensitive(!options.is_empty())
                .build();
            let chosen = {
                let draft = draft.borrow();
                match field {
                    FilterField::Country => draft.countries.clone(),
                    FilterField::Protocol => draft.protocols.clone(),
                    FilterField::Subscription => draft.subscriptions.clone(),
                }
            };
            for option in options {
                let check = gtk::CheckButton::new();
                check.set_active(chosen.contains(&option.value));
                check.connect_toggled({
                    let read_back = read_back.clone();
                    move |_| read_back()
                });
                let inner = adw::ActionRow::builder()
                    .title(&option.label)
                    .activatable_widget(&check)
                    .build();
                inner.add_prefix(&check);
                row.add_row(&inner);
                checks
                    .borrow_mut()
                    .push((field, option.value.clone(), check));
            }
            // Opened when it already has something in it: the row is then a
            // report on the filter, and closing it would hide the answer.
            row.set_expanded(!chosen.is_empty());
            rows.append(&row);
            field_rows.borrow_mut().push((field, row));
        }

        let naming = adw::PreferencesGroup::new();
        naming.add(&name_row);
        naming.add(&icon_row);
        body.append(&naming);
        body.append(&section_title("Matching"));
        body.append(&rows);
        body.append(&section_title("Or name them"));
        body.append(&picked);

        let reset = gtk::Button::builder()
            .label("Reset")
            .css_classes(["flat"])
            .build();
        reset.connect_clicked({
            let draft = draft.clone();
            let checks = checks.clone();
            let syncing = syncing.clone();
            let refresh = refresh.clone();
            move |_| {
                *draft.borrow_mut() = FilterDraft::default();
                syncing.set(true);
                for (_, _, check) in checks.borrow().iter() {
                    check.set_active(false);
                }
                syncing.set(false);
                refresh();
            }
        });
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        footer.set_margin_top(6);
        footer.set_margin_bottom(6);
        footer.set_margin_start(12);
        footer.set_margin_end(12);
        footer.append(&matches);
        footer.append(&reset);

        // Both server pickers push a page instead of opening a popover: each is a
        // checkbox per server with a search over two hundred of them, which is a
        // screen, and `AdwNavigationView` slides between screens on its own.
        let picker_page = |title: &str, tag: &str, initial: Vec<String>, write: DraftField| {
            let picker = server_picker(
                &subscriptions,
                &self.search_texts.borrow(),
                Rc::new(RefCell::new(initial)),
                Rc::new({
                    let draft = draft.clone();
                    let refresh = refresh.clone();
                    move |values: &[String]| {
                        write(&mut draft.borrow_mut(), values.to_vec());
                        refresh();
                    }
                }),
            );
            let page_view = adw::ToolbarView::builder().content(&picker.root).build();
            page_view.add_top_bar(&adw::HeaderBar::new());
            let page = adw::NavigationPage::builder()
                .title(title)
                .tag(tag)
                .child(&page_view)
                .build();
            // Built empty and filled before the push rather than on `shown`: two
            // hundred rows arriving after the slide finished made the page jump —
            // and with `hscrollbar-policy: never` above them their width demand
            // reached the dialog, so it jumped sideways too. Still lazy, because
            // nothing is built until the row is activated at all.
            (page, picker.fill)
        };

        let (except_page, fill_except) = picker_page(
            "Except",
            "except",
            draft.borrow().exclude.clone(),
            Rc::new(|draft: &mut FilterDraft, values| draft.exclude = values),
        );
        exclude_row.connect_activated({
            let navigation = navigation.clone();
            move |_| {
                fill_except();
                navigation.push(&except_page);
            }
        });
        rows.append(&exclude_row);

        let (only_page, fill_only) = picker_page(
            "Only these servers",
            "only",
            draft.borrow().members.clone(),
            Rc::new(|draft: &mut FilterDraft, values| draft.members = values),
        );
        only_row.connect_activated({
            let navigation = navigation.clone();
            move |_| {
                fill_only();
                navigation.push(&only_page);
            }
        });

        let scroll = gtk::ScrolledWindow::builder()
            .child(&body)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let toolbar = adw::ToolbarView::builder().content(&scroll).build();
        toolbar.add_top_bar(&header);
        toolbar.add_bottom_bar(&footer);
        navigation.add(
            &adw::NavigationPage::builder()
                .title(dialog.title().as_str())
                .tag("selection")
                .child(&toolbar)
                .build(),
        );
        dialog.set_child(Some(&navigation));

        cancel.connect_clicked({
            let dialog = dialog.clone();
            move |_| {
                dialog.close();
            }
        });
        apply.connect_clicked({
            let view = self.clone();
            let dialog = dialog.clone();
            let draft = draft.clone();
            move |_| {
                dialog.close();
                view.show_selection(&draft.borrow());
            }
        });
        save.connect_clicked({
            let view = self.clone();
            let dialog = dialog.clone();
            let draft = draft.clone();
            let name_row = name_row.clone();
            let icon_row = icon_row.clone();
            move |_| {
                let name = name_row.text().trim().to_string();
                if name.is_empty() {
                    return;
                }
                dialog.close();
                let draft = draft.borrow();
                let kind = if list_only {
                    GroupKind::List
                } else {
                    draft.kind()
                };
                view.save_group(
                    editing.clone(),
                    name.clone(),
                    icon_row.text().trim().to_string(),
                    kind,
                    draft.to_query(name, kind),
                );
            }
        });
        (*refresh)();
        dialog.present(Some(&self.root));
        // The name is what "New group" came for; everything else on the form
        // already holds what the page was showing.
        if matches!(intent, SelectionIntent::Name) {
            name_row.grab_focus();
        }
    }

    /// What a fresh selection starts holding when it is not editing a group.
    ///
    /// A search box has no equivalent in a rule, so a selection made with one in
    /// force is frozen into a list instead — otherwise a group saved from
    /// "de" on screen would quietly be every German node *and* every node whose
    /// name happens to contain those letters, forever. This is the honest half
    /// of what the List/Rule radio used to convey by disabling itself.
    fn starting_members(
        &self,
        intent: &SelectionIntent,
        subscriptions: &[Subscription],
    ) -> Vec<String> {
        if matches!(intent, SelectionIntent::Name) && !self.query.borrow().is_empty() {
            return filtered_ids(&self.current_filter.borrow(), subscriptions);
        }
        self.scope_members.borrow().clone()
    }

    /// Put a selection on the page without saving it anywhere.
    fn show_selection(&self, draft: &FilterDraft) {
        self.filter_countries
            .borrow_mut()
            .clone_from(&draft.countries);
        self.filter_protocols
            .borrow_mut()
            .clone_from(&draft.protocols);
        self.filter_subscriptions
            .borrow_mut()
            .clone_from(&draft.subscriptions);
        self.filter_exclude.borrow_mut().clone_from(&draft.exclude);
        self.scope_members.borrow_mut().clone_from(&draft.members);
        // The saved scope is kept, not cleared: narrowing "Germany" is a
        // narrowed view of Germany, which the chip marks with a "·" and
        // offers to save.
        self.apply_filter();
    }

    /// Load a saved scope, or clear back to "All".
    fn select_group(&self, id: Option<&str>) {
        // Cleared by `apply_filter`, at the far end of the cross-fade.
        self.switching_scope.set(true);
        let group = id.and_then(|id| {
            self.saved_groups
                .borrow()
                .iter()
                .find(|group| group.id == id)
                .cloned()
        });
        *self.active_group.borrow_mut() = group.as_ref().map(|group| group.id.clone());
        match group {
            // A list has no filter rows to load, so the filters are cleared and
            // the members are carried separately.
            Some(group) if group.kind == GroupKind::List => {
                *self.scope_members.borrow_mut() = group.query.members.clone();
                self.set_filter_values(&PoolQuery::default());
            }
            Some(group) => {
                self.scope_members.borrow_mut().clear();
                self.set_filter_values(&group.query);
            }
            None => {
                self.scope_members.borrow_mut().clear();
                self.set_filter_values(&PoolQuery::default());
            }
        }
        self.build_chip_bar();
        self.fade_through(|view| view.apply_filter());
    }

    /// Swap what the list shows behind a short cross-fade.
    ///
    /// Switching scope replaces most of the grid at once, and a hard cut reads
    /// as a flicker rather than as a change of view. Fired only from
    /// `select_group` — deliberately not from `apply_filter`, which runs on
    /// every keystroke in the search box, where a fade would be a strobe.
    ///
    /// The swap happens at the far end of the fade-out, so nothing moves while
    /// anything is still legible; cards are already collapsed by `apply_filter`
    /// before it repacks, so this never fights `server_card`'s own height
    /// animation.
    fn fade_through(&self, swap: impl Fn(&Self) + 'static) {
        let target = self.servers_area.clone();
        let out = adw::TimedAnimation::builder()
            .widget(&target)
            .value_from(1.0)
            .value_to(0.35)
            .duration(90)
            .target(&adw::PropertyAnimationTarget::new(&target, "opacity"))
            .build();
        let view = self.clone();
        out.connect_done(move |_| {
            swap(&view);
            let target = view.servers_area.clone();
            adw::TimedAnimation::builder()
                .widget(&target)
                .value_from(0.35)
                .value_to(1.0)
                .duration(130)
                .target(&adw::PropertyAnimationTarget::new(&target, "opacity"))
                .build()
                .play();
        });
        out.play();
    }

    /// Write a query into the filter widgets' backing state, then into the
    /// widgets themselves, so the two cannot drift.
    fn set_filter_values(&self, query: &PoolQuery) {
        self.filter_countries
            .borrow_mut()
            .clone_from(&query.countries);
        self.filter_protocols
            .borrow_mut()
            .clone_from(&query.protocols);
        self.filter_subscriptions
            .borrow_mut()
            .clone_from(&query.subscriptions);
        self.filter_exclude.borrow_mut().clone_from(&query.exclude);
    }

    /// Names of the groups that currently hold `server_id`. The subscriptions
    /// page asks before it deletes anything; groups live here, so the answer
    /// does too.
    pub fn groups_holding(&self, server_id: &str) -> Vec<String> {
        groups_holding(
            &self.prefs.borrow().groups,
            &self.subscriptions.borrow(),
            server_id,
        )
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    /// `(group name, how many of this subscription's servers it holds)`.
    pub fn groups_holding_any(&self, subscription_id: &str) -> Vec<(String, usize)> {
        let subscriptions = self.subscriptions.borrow();
        let Some(subscription) = subscriptions
            .iter()
            .find(|group| group.id == subscription_id)
        else {
            return Vec::new();
        };
        self.prefs
            .borrow()
            .groups
            .iter()
            .filter_map(|group| {
                let held = group_member_ids(group, &subscriptions);
                let count = subscription
                    .servers
                    .iter()
                    .filter(|server| held.contains(&server.id))
                    .count();
                (count > 0).then(|| (group.name.clone(), count))
            })
            .collect()
    }

    fn active_group_value(&self) -> Option<ServerGroup> {
        let id = self.active_group.borrow().clone()?;
        self.saved_groups
            .borrow()
            .iter()
            .find(|group| group.id == id)
            .cloned()
    }

    fn delete_group_dialog(&self, group: ServerGroup) {
        let parent = self.root.root().and_downcast::<gtk::Window>();
        let dialog = adw::AlertDialog::new(
            Some("Remove group?"),
            Some(&format!(
                "“{}” will be removed. The servers in it stay where they are.",
                group.name
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Remove")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, {
            let view = self.clone();
            move |dialog, response| {
                dialog.close();
                if response != "delete" {
                    return;
                }
                {
                    let mut prefs = view.prefs.borrow_mut();
                    prefs.groups.retain(|saved| saved.id != group.id);
                    if let Err(error) = prefs.save() {
                        log::warn!("could not save gui prefs: {error:#}");
                    }
                }
                *view.saved_groups.borrow_mut() = view.prefs.borrow().groups.clone();
                view.select_group(None);
            }
        });
        dialog.present(parent.as_ref());
    }

    /// Star or unstar one server. Handled here rather than routed through the
    /// window because Favourites is a display preference this view already
    /// owns; the daemon has no opinion about it.
    fn toggle_favourite(&self, server_id: &str) {
        {
            let mut prefs = self.prefs.borrow_mut();
            let favourites = prefs
                .groups
                .iter()
                .find(|group| group.id == FAVOURITES_ID)
                .cloned()
                .unwrap_or_else(ServerGroup::favourites);
            prefs.groups = upsert_group(&prefs.groups, toggled_member(&favourites, server_id));
            if let Err(error) = prefs.save() {
                log::warn!("could not save gui prefs: {error:#}");
            }
        }
        // The chip's count changed, and if Favourites is the active scope the
        // card the user just starred has to appear or leave right now.
        self.build_chip_bar();
        if self.active_group.borrow().as_deref() == Some(FAVOURITES_ID) {
            self.select_group(Some(FAVOURITES_ID));
        }
    }

    /// The advanced rows read back as a rule. One place rather than three, so
    /// adding a filter cannot reach the screen without reaching what is saved.
    fn rule_from_filters(&self, name: String) -> PoolQuery {
        PoolQuery {
            name,
            countries: self.filter_countries.borrow().clone(),
            protocols: self.filter_protocols.borrow().clone(),
            subscriptions: self.filter_subscriptions.borrow().clone(),
            exclude: self.filter_exclude.borrow().clone(),
            ..PoolQuery::default()
        }
    }

    /// `query` is passed in rather than re-derived from the page: the editor
    /// commits a draft that the page has not necessarily been shown, so reading
    /// the live filters here would save something the dialog never displayed.
    ///
    /// A group stores *which servers*, and nothing else. `expected` is
    /// deliberately absent even though the Connect bar sets it: the rotation
    /// width is an intent about the run, chosen where Connect is pressed and
    /// visible at that moment, and giving the group a second copy of it is how
    /// the two come to disagree — the same reason the profile dialog stopped
    /// carrying a pool editor.
    fn save_group(
        &self,
        editing: Option<ServerGroup>,
        name: String,
        icon: String,
        kind: GroupKind,
        query: PoolQuery,
    ) {
        let group = ServerGroup {
            id: editing
                .map(|group| group.id)
                .unwrap_or_else(|| free_group_id(&name, &self.prefs.borrow().groups)),
            name,
            icon,
            kind,
            query,
        };
        let id = group.id.clone();
        {
            let mut prefs = self.prefs.borrow_mut();
            prefs.groups = upsert_group(&prefs.groups, group);
            if let Err(error) = prefs.save() {
                log::warn!("could not save gui prefs: {error:#}");
            }
        }
        *self.saved_groups.borrow_mut() = self.prefs.borrow().groups.clone();
        self.select_group(Some(&id));
    }

    /// Mark the selected chip when the filter no longer shows what that group
    /// holds — so a narrowed view is never mistaken for the group itself, and
    /// "Save as group…" is understood as saving the change rather than the
    /// group as it was.
    fn sync_chip_modified(&self) {
        // A scope change writes the new filters, rebuilds the row, and only then
        // recomputes what is on screen. Asked in between, `group_is_modified`
        // compares the *incoming* group against the *outgoing* query and says
        // yes, so the new chip flashed a "·" that vanished a moment later — a
        // warning about an edit nobody made.
        if self.switching_scope.get() {
            return;
        }
        let Some(scopes) = self.scopes.borrow().clone() else {
            return;
        };
        let Some(id) = self.active_group.borrow().clone() else {
            return;
        };
        let group = {
            let saved = self.saved_groups.borrow();
            let Some(group) = saved.iter().find(|group| group.id == id) else {
                return;
            };
            group.clone()
        };
        // The toggle is addressed by name rather than by index: the row is
        // rebuilt whenever a group is added, removed or moved, and an index
        // captured before that would relabel somebody else's scope.
        let Some(toggle) = scopes.toggle_by_name(&id) else {
            return;
        };
        let modified = self.group_is_modified(&group);
        toggle.set_label(Some(&if modified {
            format!("{} ·", group.label())
        } else {
            group.label()
        }));
        toggle.set_tooltip(&if modified {
            format!(
                "Showing a narrowed view of “{}”. Save it to keep the change.",
                group.name
            )
        } else {
            group_chip_tooltip(
                &group,
                group_member_ids(&group, &self.subscriptions.borrow()).len(),
            )
        });
    }

    /// How many filter fields are set, on the pill itself. A popover that has
    /// to be opened to find out whether it is doing anything is a popover the
    /// user will open again and again.
    fn sync_filter_pill(&self) {
        let active = self.active_filter_fields();
        self.filter_label.set_label(&if active == 0 {
            "Filter".to_string()
        } else {
            format!("Filter · {active}")
        });
        if active == 0 {
            self.filter_button.remove_css_class("group-chip-modified");
        } else {
            self.filter_button.add_css_class("group-chip-modified");
        }
    }

    /// Filter fields the user has set, ignoring the search box. The search is
    /// deliberately not counted: it is transient text, and a control that
    /// lights up on every keystroke stops meaning anything.
    fn active_filter_fields(&self) -> usize {
        [
            self.filter_countries.borrow().is_empty(),
            self.filter_protocols.borrow().is_empty(),
            self.filter_subscriptions.borrow().is_empty(),
            self.filter_exclude.borrow().is_empty(),
            // Servers named by hand count as a field too, now that the dialog can
            // write them: without this a hand-picked selection left the pill
            // reading "Filter" and, worse, gave the Connect bar no reason to
            // appear — so there was no way to run what had just been chosen.
            self.scope_members.borrow().is_empty(),
        ]
        .into_iter()
        .filter(|empty| !empty)
        .count()
    }

    pub fn set_query(&self, query: &str) {
        *self.query.borrow_mut() = query.trim().to_lowercase();
        self.apply_filter();
    }

    /// Wires the "no servers yet" page's action button.
    pub fn connect_browse_subscriptions(&self, callback: impl Fn() + 'static) {
        *self.on_browse_subscriptions.borrow_mut() = Some(Box::new(callback));
    }

    fn apply_filter(&self) {
        // Whatever is on screen is exactly what "save" and "create pool" would
        // store, so both are derived here from one computation rather than
        // rebuilt separately and left to drift.
        let subscriptions = self.subscriptions.borrow().clone();
        let members = self.scope_members.borrow().clone();
        let query = if members.is_empty() {
            filters_to_query(
                &subscriptions,
                &self.filter_subscriptions.borrow(),
                &self.filter_countries.borrow(),
                &self.filter_protocols.borrow(),
                &self.filter_exclude.borrow(),
                &self.search_texts.borrow(),
                &self.query.borrow(),
            )
        } else {
            // A frozen list narrowed by the search box is still a list: there
            // is no filter to intersect with, only members to drop. Building it
            // through `filtered_ids` keeps the resolved order and silently
            // loses handles whose server is gone, which is what a list is for.
            let text = self.query.borrow().clone();
            let texts = self.search_texts.borrow();
            let listed = PoolQuery {
                members,
                ..PoolQuery::default()
            };
            PoolQuery {
                members: filtered_ids(&listed, &subscriptions)
                    .into_iter()
                    .filter(|id| {
                        text.is_empty()
                            || texts
                                .get(id)
                                .is_some_and(|haystack| haystack.contains(&text))
                    })
                    .collect(),
                ..PoolQuery::default()
            }
        };
        let filtered = filtered_ids(&query, &subscriptions)
            .into_iter()
            .collect::<HashSet<_>>();
        *self.current_filter.borrow_mut() = query;
        // The query now describes what is on screen, so the "modified" marker can
        // be trusted again.
        self.switching_scope.set(false);
        self.sync_filter_pill();
        self.sync_chip_modified();
        self.sync_connect_bar();
        let selected = self.selected.borrow().clone();
        let mut total_visible = 0usize;
        {
            for block in self.blocks.borrow().iter() {
                let mut visible = 0;
                for (id, card) in &block.cards {
                    let matches = filtered.contains(id);
                    card.set_visible(matches);
                    if matches {
                        visible += 1;
                    }
                }
                block.root.set_visible(visible > 0);
                total_visible += visible;
            }
        }
        // A query that matches nothing used to leave a blank page. An empty
        // Favourites is the ordinary version of that, so it says the thing to
        // do about it rather than blaming the search.
        let empty_list = self.active_group.borrow().is_some()
            && self.scope_members.borrow().is_empty()
            && self.saved_groups.borrow().iter().any(|group| {
                Some(group.id.as_str()) == self.active_group.borrow().as_deref()
                    && group.kind == GroupKind::List
            });
        if total_visible == 0 && empty_list {
            self.no_matches.set_title("This group is empty");
            self.no_matches
                .set_description(Some("Star a server to put it here."));
        } else {
            self.no_matches.set_title("No matching servers");
            self.no_matches
                .set_description(Some("Try a different name, country, or protocol."));
        }
        self.no_matches.set_visible(total_visible == 0);
        // Cards mid-collapse from a recent selection switch reflow anyway —
        // snap them closed before repacking.
        for (id, card) in self.cards.borrow().iter() {
            if Some(id.as_str()) != selected.as_deref() && card.is_expanded() {
                card.collapse_immediately();
            }
        }
        for block in self.blocks.borrow().iter() {
            repack_block(block, self.columns.get());
        }
        // Keep the selected card's expansion in sync with its visibility, so a
        // filtered-out card doesn't stay tall and highlighted off-grid, and
        // reappears expanded when the query clears.
        if let Some(selected_id) = selected.as_deref() {
            let card = self.cards.borrow().get(selected_id).cloned();
            if let Some(card) = card {
                if !card.root.get_visible() {
                    card.collapse_immediately();
                } else if !card.is_expanded()
                    && let Some(height) = self.expanded_target_height(selected_id)
                {
                    card.set_expanded_immediately(height);
                }
            }
        }
        self.schedule_expanded_remeasure();
    }

    /// Re-derive every stop control from the states a rebuild was handed.
    ///
    /// A rebuild builds cards and headers from scratch, so their buttons start
    /// at "check" no matter what is in flight, and it does not go through
    /// [`Self::set_latency_state`] — the state arrives as a constructor argument
    /// instead. Without this, sorting or filtering during a sweep left a row of
    /// fresh buttons offering to start a sweep that was already running.
    ///
    /// The passed states are the authority rather than the set this view keeps,
    /// which is why the set is rebuilt from them here too.
    fn sync_probing(&self, latency_states: &HashMap<String, LatencyState>) {
        let mut checking = self.checking.borrow_mut();
        checking.clear();
        for (id, state) in latency_states {
            if *state == LatencyState::Checking {
                checking.insert(id.clone());
            }
        }
        if !self.can_cancel_probes.get() {
            return;
        }
        let cards = self.cards.borrow();
        for block in self.blocks.borrow().iter() {
            let count = block
                .cards
                .iter()
                .filter(|(id, _)| checking.contains(id))
                .count();
            block.checking.set(count);
            let probing = count > 0;
            block.speed_button.set_icon_name(sweep_icon(probing));
            block
                .speed_button
                .set_tooltip_text(Some(sweep_label(probing)));
            block
                .speed_button
                .update_property(&[gtk::accessible::Property::Label(sweep_label(probing))]);
            for (id, _) in &block.cards {
                if let Some(card) = cards.get(id) {
                    card.set_probing(checking.contains(id));
                }
            }
        }
    }

    /// Record whether the daemon can call a check off, which decides whether
    /// either latency control is allowed to offer a stop.
    pub fn set_probe_cancel_supported(&self, supported: bool) {
        self.can_cancel_probes.set(supported);
    }

    /// Keep each block's sweep button pointed at the right action.
    ///
    /// The count is per block rather than global because two subscriptions can
    /// be swept independently, and a stop button on the block that is idle would
    /// stop the wrong thing. Only a crossing of zero touches a widget: a sweep
    /// of six hundred servers calls through here twice per card, and repainting
    /// the header on each of those would be twelve hundred no-op set_icon_names.
    fn track_checking(&self, server_id: &str, checking: bool) {
        let was = self.checking.borrow().contains(server_id);
        if was == checking {
            return;
        }
        if checking {
            self.checking.borrow_mut().insert(server_id.to_string());
        } else {
            self.checking.borrow_mut().remove(server_id);
        }
        if !self.can_cancel_probes.get() {
            return;
        }
        if let Some(card) = self.cards.borrow().get(server_id) {
            card.set_probing(checking);
        }
        for block in self.blocks.borrow().iter() {
            if !block.cards.iter().any(|(id, _)| id == server_id) {
                continue;
            }
            let count = block.checking.get();
            let count = if checking {
                count + 1
            } else {
                count.saturating_sub(1)
            };
            block.checking.set(count);
            // Only a crossing of zero, which is the only time the button's
            // meaning actually changes.
            if (checking && count == 1) || (!checking && count == 0) {
                let probing = count > 0;
                block.speed_button.set_icon_name(sweep_icon(probing));
                block
                    .speed_button
                    .set_tooltip_text(Some(sweep_label(probing)));
                block
                    .speed_button
                    .update_property(&[gtk::accessible::Property::Label(sweep_label(probing))]);
            }
            break;
        }
    }

    /// Put the diagnosis for a failed check on one card, or take it away.
    ///
    /// Only the expanded card shows one, so this is a lookup rather than a
    /// pass over the grid — and the expansion is re-measured after it, because
    /// the block appearing is the one content change that happens while a card
    /// is already open at a fixed height.
    pub fn set_failure_report(&self, server_id: &str, report: Option<&FailureReport>) {
        let changed = match self.cards.borrow().get(server_id) {
            Some(card) => {
                card.set_failure_report(report);
                card.is_expanded()
            }
            None => false,
        };
        if changed {
            self.refresh_expanded_height();
        }
    }

    /// Put the recent checks on one card, or take them away.
    ///
    /// A lookup and a re-measure for the same reasons as
    /// [`Self::set_failure_report`]: only the expanded card carries a history,
    /// and the list growing a row is a content change under a fixed height.
    pub fn set_history(&self, server_id: &str, rows: &[HistoryRow]) {
        let changed = match self.cards.borrow().get(server_id) {
            Some(card) => {
                card.set_history(rows);
                card.is_expanded()
            }
            None => false,
        };
        if changed {
            self.refresh_expanded_height();
        }
    }

    pub fn set_latency_state(&self, server_id: &str, state: LatencyState) {
        if let Some(card) = self.cards.borrow().get(server_id) {
            card.set_latency_state(state);
        }
        self.track_checking(server_id, state == LatencyState::Checking);
        // `Checking` says nothing about where the card belongs in a latency
        // sort, so it leaves the previous key alone rather than clearing it.
        match sort_value(state) {
            Some(value) => {
                self.latencies
                    .borrow_mut()
                    .insert(server_id.to_string(), value);
            }
            None if state != LatencyState::Checking => {
                self.latencies.borrow_mut().remove(server_id);
            }
            None => {}
        }
    }

    /// Reflect the connection on every card in one pass: a `connecting` server
    /// wins, otherwise `active` decides Connected/Elsewhere/Disconnected.
    pub fn set_connection(&self, connection: &CardConnection) {
        for (id, card) in self.cards.borrow().iter() {
            let state = match (&connection.connecting, &connection.failed) {
                // An attempt in flight outranks everything: no other card may
                // claim `default` while one is being built. Sessions belonging
                // to other profiles remain real and stay highlighted.
                (Some(connecting), _) if connecting == id => CardConnectionState::Connecting,
                (Some(_), _)
                    if connection
                        .profiles
                        .get(id)
                        .is_some_and(|profiles| !profiles.connected.is_empty()) =>
                {
                    CardConnectionState::ConnectedHere
                }
                (Some(_), _)
                    if connection
                        .profiles
                        .get(id)
                        .is_some_and(|profiles| !profiles.in_pool.is_empty()) =>
                {
                    CardConnectionState::InPool
                }
                (Some(_), _) => CardConnectionState::Disconnected,
                (None, Some(_))
                    if connection
                        .profiles
                        .get(id)
                        .is_some_and(|profiles| !profiles.connected.is_empty()) =>
                {
                    CardConnectionState::ConnectedHere
                }
                (None, Some(_))
                    if connection
                        .profiles
                        .get(id)
                        .is_some_and(|profiles| !profiles.in_pool.is_empty()) =>
                {
                    CardConnectionState::InPool
                }
                (None, Some(failed)) if failed == id => CardConnectionState::Failed,
                _ if connection
                    .profiles
                    .get(id)
                    .is_some_and(|profiles| !profiles.connected.is_empty()) =>
                {
                    CardConnectionState::ConnectedHere
                }
                _ if connection
                    .profiles
                    .get(id)
                    .is_some_and(|profiles| !profiles.in_pool.is_empty()) =>
                {
                    CardConnectionState::InPool
                }
                _ if connection.active.as_deref() == Some(id) => CardConnectionState::ConnectedHere,
                _ if !connection.profiles.is_empty() || connection.active.is_some() => {
                    CardConnectionState::ConnectedElsewhere
                }
                _ => CardConnectionState::Disconnected,
            };
            card.set_connection_state(state);
        }
    }

    pub fn set_selected(&self, server_id: Option<&str>) {
        if self.requested_selected.borrow().as_deref() == server_id {
            return;
        }
        let next = server_id.map(str::to_string);
        *self.requested_selected.borrow_mut() = next.clone();
        let current = self.selected.borrow().clone();

        if current == next {
            if let Some(server_id) = current {
                let card = self.cards.borrow().get(&server_id).cloned();
                let target_height = self.expanded_target_height(&server_id);
                if let (Some(card), Some(target_height)) = (card, target_height) {
                    card.expand(target_height, None);
                }
            }
            return;
        }

        *self.selected.borrow_mut() = next.clone();

        // The clicked card expands strictly in place — its slot never changes
        // on selection (only an explicit sort moves cards), so its header
        // stays put and a follow-up double-click always lands on it.
        if let Some(id) = next.as_deref() {
            let card = self.cards.borrow().get(id).cloned();
            let height = self.expanded_target_height(id);
            if let (Some(card), Some(height)) = (card, height) {
                card.expand(height, None);
            }
        }
        self.collapse_others(next.as_deref());
    }

    /// Collapse every expanded card except `keep`, immediately. When a
    /// collapsing card sits above the kept card (earlier group, or same
    /// column and earlier slot in the same group), compensate the scroll
    /// position frame-by-frame so the kept card stays pinned on screen.
    fn collapse_others(&self, keep: Option<&str>) {
        let keep_position = keep.and_then(|id| self.position_of(id));
        let vadjustment = self.root.vadjustment();
        let columns = self.columns.get();
        for (id, card) in self.cards.borrow().iter() {
            if Some(id.as_str()) == keep || !card.is_expanded() {
                continue;
            }
            let shifts_kept_card = match (self.position_of(id), keep_position) {
                (Some(prev), Some(next)) => collapse_would_shift(prev, next, columns),
                _ => false,
            };
            if shifts_kept_card {
                let vadjustment = vadjustment.clone();
                card.collapse(Some(Rc::new(move |delta: i32| {
                    let value = (vadjustment.value() - f64::from(delta)).max(0.0);
                    vadjustment.set_value(value);
                })));
            } else {
                card.collapse(None);
            }
        }
    }

    /// (group index, index within the group's ordered visible cards).
    fn position_of(&self, server_id: &str) -> Option<(usize, usize)> {
        let blocks = self.blocks.borrow();
        for (block_index, block) in blocks.iter().enumerate() {
            let display_order = block.display_order.borrow();
            let mut visible_index = 0;
            for id in display_order.iter() {
                let Some((_, widget)) = block.cards.iter().find(|(card_id, _)| card_id == id)
                else {
                    continue;
                };
                if !widget.get_visible() {
                    continue;
                }
                if id == server_id {
                    return Some((block_index, visible_index));
                }
                visible_index += 1;
            }
        }
        None
    }

    fn set_selected_immediately(&self, server_id: &str) {
        let height = self.expanded_target_height(server_id);
        if let Some(height) = height
            && let Some(card) = self.cards.borrow().get(server_id)
        {
            card.set_expanded_immediately(height);
        }
    }

    /// Natural expanded height at the current column width.
    fn expanded_target_height(&self, server_id: &str) -> Option<i32> {
        let cards = self.cards.borrow();
        let card = cards.get(server_id)?;
        let allocated = card.root.allocated_width();
        let width = if allocated > 0 {
            allocated
        } else {
            self.column_fallback_width().unwrap_or(CARD_MEASURE_WIDTH)
        };
        Some(card.expanded_natural_height(width).max(COMPACT_CARD_HEIGHT))
    }

    fn column_fallback_width(&self) -> Option<i32> {
        let blocks = self.blocks.borrow();
        let total = blocks
            .iter()
            .map(|block| block.columns_box.allocated_width())
            .find(|width| *width > 0)?;
        let columns = self.columns.get().max(1) as i32;
        Some(
            total
                .saturating_sub(CARD_COLUMN_SPACING.saturating_mul(columns - 1))
                .checked_div(columns)
                .unwrap_or(total),
        )
    }

    fn refresh_expanded_height(&self) {
        let Some(server_id) = self.selected.borrow().clone() else {
            return;
        };
        let Some(height) = self.expanded_target_height(&server_id) else {
            return;
        };
        if let Some(card) = self.cards.borrow().get(&server_id) {
            card.resize_expanded(height);
        }
    }

    /// Re-measure the expanded card once the layout settles after a width or
    /// column change; coalesces bursts of resize events.
    fn schedule_expanded_remeasure(&self) {
        let generation = self.resize_generation.get().wrapping_add(1);
        self.resize_generation.set(generation);
        let view = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(RESIZE_SETTLE_MS), move || {
            if view.resize_generation.get() == generation {
                view.refresh_expanded_height();
            }
        });
    }

    /// Move a subscription group one slot up (`-1`) or down (`1`).
    ///
    /// Reorders the widgets in place instead of rebuilding: a rebuild would
    /// throw away every card's expansion, latency badge and running animation
    /// to express a change that is three `reorder_child_after` calls.
    fn move_subscription(&self, subscription_id: &str, delta: isize) {
        let visible: Vec<String> = self
            .blocks
            .borrow()
            .iter()
            .map(|block| block.id.clone())
            .collect();
        let order = moved_in_order(&visible, subscription_id, delta);
        if order == visible {
            return;
        }

        {
            let mut blocks = self.blocks.borrow_mut();
            blocks.sort_by_key(|block| {
                order
                    .iter()
                    .position(|id| id == &block.id)
                    .unwrap_or(usize::MAX)
            });
            // `content` also holds the chip row and the connect bar above the
            // blocks and the "no matches" page below them. Re-seating starts
            // after the connect bar by name rather than at `first_child`, so
            // the first block cannot slip in front of it.
            let mut previous = Some(self.connect_bar.clone().upcast::<gtk::Widget>());
            for block in blocks.iter() {
                self.content
                    .reorder_child_after(&block.root, previous.as_ref());
                previous = Some(block.root.clone());
            }
        }

        let mut prefs = self.prefs.borrow_mut();
        prefs.subscription_order = order;
        if let Err(error) = prefs.save() {
            log::warn!("could not save gui prefs: {error:#}");
        }
    }

    /// Move a group chip one slot along the row.
    ///
    /// Rebuilding the row rather than re-seating widgets: a chip carries no
    /// animation or expansion to lose, and the row is small enough that the
    /// simpler path costs nothing.
    fn move_chip(&self, group_id: &str, delta: isize) {
        let visible: Vec<String> = self
            .saved_groups
            .borrow()
            .iter()
            .map(|group| group.id.clone())
            .collect();
        let order = moved_in_order(&visible, group_id, delta);
        if order == visible {
            return;
        }
        {
            let mut prefs = self.prefs.borrow_mut();
            prefs.groups.sort_by_key(|group| {
                order
                    .iter()
                    .position(|id| id == &group.id)
                    .unwrap_or(usize::MAX)
            });
            if let Err(error) = prefs.save() {
                log::warn!("could not save gui prefs: {error:#}");
            }
        }
        *self.saved_groups.borrow_mut() = self.prefs.borrow().groups.clone();
        self.build_chip_bar();
    }

    fn move_subscription_menu(&self, subscription_id: &str) -> gtk::Popover {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        list.set_margin_top(6);
        list.set_margin_bottom(6);
        list.set_margin_start(6);
        list.set_margin_end(6);
        let popover = gtk::Popover::builder().child(&list).build();
        for (label, icon, delta) in [
            ("Move up", "go-up-symbolic", -1_isize),
            ("Move down", "go-down-symbolic", 1),
        ] {
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            content.append(&gtk::Image::from_icon_name(icon));
            content.append(&gtk::Label::new(Some(label)));
            let button = gtk::Button::builder()
                .child(&content)
                .css_classes(["flat"])
                .build();
            button.connect_clicked({
                let view = self.clone();
                let subscription_id = subscription_id.to_string();
                let popover = popover.clone();
                move |_| {
                    popover.popdown();
                    view.move_subscription(&subscription_id, delta);
                }
            });
            list.append(&button);
        }
        popover
    }

    /// Manually capture and apply a latency order. Later measurements update only
    /// their badges until the user presses sort again.
    pub fn sort_subscription(&self, subscription_id: &str) {
        let Some(block) = self
            .blocks
            .borrow()
            .iter()
            .find(|block| block.id == subscription_id)
            .cloned()
        else {
            return;
        };
        let sorted = sorted_by_latency(&block.display_order.borrow(), &self.latencies.borrow());
        let generation = block.sort_generation.get().wrapping_add(1);
        block.sort_generation.set(generation);
        block.sort_button.set_sensitive(false);

        if !adw::is_animations_enabled(&block.columns_box) {
            *block.display_order.borrow_mut() = sorted;
            repack_block(&block, self.columns.get());
            block.sort_button.set_sensitive(true);
            return;
        }

        let target = adw::CallbackAnimationTarget::new({
            let columns_box = block.columns_box.clone();
            move |value| columns_box.set_opacity(value)
        });
        let animation = adw::TimedAnimation::new(
            &block.columns_box,
            block.columns_box.opacity(),
            0.0,
            90,
            target,
        );
        animation.set_easing(adw::Easing::EaseInCubic);
        animation.connect_done({
            let view = self.clone();
            let block = block.clone();
            move |_| {
                if block.sort_generation.get() != generation {
                    return;
                }
                *block.display_order.borrow_mut() = sorted.clone();
                repack_block(&block, view.columns.get());

                let target = adw::CallbackAnimationTarget::new({
                    let columns_box = block.columns_box.clone();
                    move |value| columns_box.set_opacity(value)
                });
                let fade_in = adw::TimedAnimation::new(&block.columns_box, 0.0, 1.0, 130, target);
                fade_in.set_easing(adw::Easing::EaseOutCubic);
                fade_in.connect_done({
                    let block = block.clone();
                    move |_| {
                        if block.sort_generation.get() == generation {
                            block.sort_button.set_sensitive(true);
                        }
                    }
                });
                fade_in.play();
            }
        });
        animation.play();
    }
}

/// A stable id for a new block: a slug of its name, suffixed until free.
///
/// The id outlives renames, so it cannot simply *be* the name; and it is only
/// ever compared against this machine's own list, so it does not need to be a
/// hash of anything.
fn free_group_id(name: &str, existing: &[ServerGroup]) -> String {
    let slug = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let slug = slug.trim_matches('-').to_string();
    let base = if slug.is_empty() {
        "group".to_string()
    } else {
        slug
    };
    let taken = |candidate: &str| existing.iter().any(|group| group.id == candidate);
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or(base)
}

fn section_title(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["dim-label", "caption-heading"])
        .build()
}

fn group_chip_tooltip(group: &ServerGroup, matched: usize) -> String {
    match group.kind {
        GroupKind::List => format!(
            "{}: {matched} server{}, frozen. New servers do not join on their own.",
            group.name,
            if matched == 1 { "" } else { "s" }
        ),
        GroupKind::Rule => format!(
            "{}: {}. Matches {matched} right now, and servers added later can join.",
            group.name,
            describe_rule(&group.query)
        ),
    }
}

/// What the form on screen adds up to, in one line.
///
/// This is what became of the List/Rule radio. The radio asked the user to
/// classify their own selection before they had made it, in words about how
/// groups are stored; the same fact is readable off the form, so it is reported
/// instead — including the part the radio only ever conveyed by greying itself
/// out, which is that a frozen list does not grow and a rule does.
fn selection_summary(draft: &FilterDraft, matched: usize, list_only: bool) -> String {
    let plural = |count: usize| if count == 1 { "" } else { "s" };
    if !draft.members.is_empty() {
        let picked = draft.members.len();
        return format!(
            "{picked} server{} chosen by hand — frozen, so servers added later do not join.",
            plural(picked)
        );
    }
    if list_only {
        // A group that can only ever hold a list is empty, not universal. The
        // rule branch below would have called it "every server", which is the
        // one thing an empty Favourites is not.
        return "No servers yet — star one on a card, or pick some here.".to_string();
    }
    if draft.rule_fields() == 0 {
        return format!("Every server — {matched} right now.");
    }
    format!(
        "{}: {matched} server{} right now, and matching servers added later join on their own.",
        describe_rule(&draft.to_query(String::new(), GroupKind::Rule)),
        plural(matched)
    )
}

/// A name the user will probably keep, so saving a scope is one click and a
/// confirmation rather than a blank field to invent something for.
fn suggested_group_name(rule: &PoolQuery) -> String {
    match (rule.countries.as_slice(), rule.protocols.as_slice()) {
        ([country], []) => country.to_uppercase(),
        ([], [protocol]) => protocol.clone(),
        ([country], [protocol]) => format!("{} {protocol}", country.to_uppercase()),
        _ => String::new(),
    }
}

/// Widths the rotation picker offers. `0` is "every live node", which is what
/// `expected` has always meant on disk.
const ROTATION_CHOICES: [usize; 6] = [2, 4, 6, 8, 12, 0];

fn rotation_label(value: usize) -> String {
    match value {
        0 => "All nodes".to_string(),
        count => format!("{count} nodes"),
    }
}

fn rotation_detail(value: usize) -> String {
    match value {
        0 => "Every node that still answers carries traffic. Widest spread, and \
              one reachability check per node."
            .to_string(),
        count => format!(
            "The {count} fastest reachable nodes carry traffic; a node that stops \
             answering is replaced from the rest of the pool."
        ),
    }
}

/// What the bar says the rotation will be, against what the pool actually has.
///
/// A pool of three cannot rotate over six, and printing "6 nodes" over three
/// would promise a spread the core will not produce.
fn rotation_summary(value: usize, available: usize) -> String {
    match value {
        0 => "All nodes".to_string(),
        count if count >= available => format!(
            "All {available} node{}",
            if available == 1 { "" } else { "s" }
        ),
        count => format!("{count} of {available} active"),
    }
}

/// A searchable checkbox per server, grouped by the subscription it came from.
///
/// One implementation behind both pages of the selection editor — "Except" and
/// "Only these servers" — because a second copy of this is exactly how the two
/// would come to disagree about what a server is called or which ones can be
/// pooled at all.
struct ServerPicker {
    root: gtk::Box,
    /// Builds the rows. Safe to call more than once; only the first does work.
    fill: Rc<dyn Fn()>,
}

fn server_picker(
    subscriptions: &[Subscription],
    haystacks: &HashMap<String, String>,
    selected: Selection,
    on_change: SelectionChanged,
) -> ServerPicker {
    let grouped = excludable_servers(subscriptions);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.set_margin_top(8);
    list.set_margin_bottom(8);
    list.set_margin_start(8);
    list.set_margin_end(8);
    // Unbounded by default, so a caller that hands the picker a whole page gets a
    // list that fills it: capped at 320 px it left the rest of the page blank
    // while its own list was still scrolling, which is the worst of both. A
    // caller with no page to spare caps `scroll` itself.
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Find a server")
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.set_margin_top(6);
    root.set_margin_bottom(6);
    root.set_margin_start(6);
    root.set_margin_end(6);
    root.set_vexpand(true);
    root.append(&search);

    let checks: PickerChecks = Rc::new(RefCell::new(Vec::new()));
    // Directly under the search, because they act on what the search left on
    // screen and nothing else: "select all shown" after typing "de" is the
    // shortest honest way to say "every German node", and it is the reason the
    // search is here at all. It lives in the picker rather than beside one
    // caller's copy of it, so both pages get it.
    let bulk = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bulk.set_halign(gtk::Align::End);
    for (label, wanted) in [("Select all shown", true), ("Clear", false)] {
        let button = gtk::Button::builder()
            .label(label)
            .css_classes(["flat"])
            .build();
        button.connect_clicked({
            let checks = checks.clone();
            move |_| {
                for (_, check) in checks.borrow().iter() {
                    if check.get_visible() {
                        check.set_active(wanted);
                    }
                }
            }
        });
        bulk.append(&button);
    }
    root.append(&bulk);
    root.append(&scroll);

    let filled = Rc::new(Cell::new(false));
    // The search matches the same text the page's search box matches — name,
    // transport, protocol, address and country — so typing "de" here finds the
    // German nodes rather than only the ones with "de" in their name.
    let haystacks = haystacks.clone();
    let fill: Rc<dyn Fn()> = Rc::new({
        let checks = checks.clone();
        move || {
            if filled.replace(true) {
                return;
            }
            // Rows are kept so the search can hide them. A provider with two
            // hundred nodes turns this list into a scroll hunt otherwise, and
            // the user asked to pick *specific* servers — which means finding
            // one.
            let mut rows: Vec<(String, gtk::Widget)> = Vec::new();
            for (subscription, servers) in &grouped {
                let heading = section_title(subscription);
                heading.set_margin_top(6);
                list.append(&heading);
                rows.push((subscription.to_lowercase(), heading.upcast::<gtk::Widget>()));
                for server in servers {
                    // An explicit ellipsizing child rather than
                    // `with_label`: a `GtkCheckButton`'s own label demands its
                    // full width, and with `hscrollbar-policy: never` above it
                    // that demand travels all the way out to the dialog — two
                    // hundred provider names made the page jump wider the moment
                    // it was filled. The whole name lives in the tooltip.
                    let label = gtk::Label::builder()
                        .label(&server.label)
                        .ellipsize(gtk::pango::EllipsizeMode::End)
                        .xalign(0.0)
                        .max_width_chars(28)
                        .build();
                    let check = gtk::CheckButton::builder()
                        .child(&label)
                        .tooltip_text(&server.label)
                        .build();
                    check.set_active(selected.borrow().contains(&server.value));
                    check.connect_toggled({
                        let selected = selected.clone();
                        let value = server.value.clone();
                        let on_change = on_change.clone();
                        move |check| {
                            let mut values = selected.borrow_mut();
                            if check.is_active() {
                                if !values.contains(&value) {
                                    values.push(value.clone());
                                }
                            } else {
                                values.retain(|selected| selected != &value);
                            }
                            let snapshot = values.clone();
                            drop(values);
                            on_change(&snapshot);
                        }
                    });
                    list.append(&check);
                    let haystack = haystacks
                        .get(&server.value)
                        .cloned()
                        .unwrap_or_else(|| server.label.to_lowercase());
                    rows.push((haystack, check.clone().upcast::<gtk::Widget>()));
                    checks.borrow_mut().push((server.value.clone(), check));
                }
            }
            search.connect_search_changed(move |entry| {
                let text = entry.text().trim().to_lowercase();
                // A subscription heading is shown when it or any of its servers
                // match, which is why the headings are marked and re-scanned
                // rather than filtered independently.
                let mut heading: Option<(gtk::Widget, bool)> = None;
                for (haystack, row) in &rows {
                    let is_heading = row.is::<gtk::Label>();
                    let matches = text.is_empty() || haystack.contains(&text);
                    if is_heading {
                        if let Some((widget, any)) = heading.take() {
                            widget.set_visible(any);
                        }
                        heading = Some((row.clone(), matches));
                        continue;
                    }
                    row.set_visible(matches);
                    if matches && let Some((_, any)) = heading.as_mut() {
                        *any = true;
                    }
                }
                if let Some((widget, any)) = heading {
                    widget.set_visible(any);
                }
            });
        }
    });

    ServerPicker { root, fill }
}

fn picked_label(count: usize) -> String {
    match count {
        0 => "Nothing picked yet".to_string(),
        1 => "1 server picked".to_string(),
        count => format!("{count} servers picked"),
    }
}

fn exclude_label(selected: &[String]) -> String {
    match selected.len() {
        0 => "Except: nothing".to_string(),
        1 => "Except: 1 server".to_string(),
        count => format!("Except: {count} servers"),
    }
}

/// Round-robin distribution: card i goes to column i % n, keeping the visual
/// reading order row-major because compact cards share one height.
fn distribute_columns(count: usize, columns: usize) -> Vec<Vec<usize>> {
    let columns = columns.max(1);
    let mut assignment = vec![Vec::new(); columns];
    for item in 0..count {
        assignment[item % columns].push(item);
    }
    assignment
}

/// Whether collapsing the card at `prev` moves the card at `next` upward:
/// only when it occupies vertical space above it — an earlier group, or an
/// earlier slot of the same column within the same group.
fn collapse_would_shift(prev: (usize, usize), next: (usize, usize), columns: usize) -> bool {
    let columns = columns.max(1);
    prev.0 < next.0 || (prev.0 == next.0 && prev.1 % columns == next.1 % columns && prev.1 < next.1)
}

/// Lay the group's visible cards out into its column boxes. Skips all widget
/// churn when the assignment already matches, so running height animations
/// and keyboard focus survive unrelated calls.
fn repack_block(block: &SubscriptionBlock, columns: usize) {
    let columns = columns.max(1);
    {
        let mut boxes = block.column_boxes.borrow_mut();
        if boxes.len() != columns {
            for (_, card) in &block.cards {
                if let Some(parent) = card.parent().and_downcast::<gtk::Box>() {
                    parent.remove(card);
                }
            }
            while let Some(child) = block.columns_box.first_child() {
                block.columns_box.remove(&child);
            }
            boxes.clear();
            for _ in 0..columns {
                let column = gtk::Box::new(gtk::Orientation::Vertical, CARD_ROW_SPACING);
                column.set_hexpand(true);
                block.columns_box.append(&column);
                boxes.push(column);
            }
        }
    }

    let boxes = block.column_boxes.borrow();
    let display_order = block.display_order.borrow();
    // Index once: this runs on every resize and every expand/collapse, and a
    // linear scan per id turns a large subscription into O(cards²) work in
    // the middle of an animation.
    let by_id: HashMap<&str, &gtk::Widget> = block
        .cards
        .iter()
        .map(|(id, card)| (id.as_str(), card))
        .collect();
    let ordered: Vec<&gtk::Widget> = display_order
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .filter(|card| card.get_visible())
        .collect();
    let desired = distribute_columns(ordered.len(), columns);

    let unchanged = desired.iter().enumerate().all(|(column, items)| {
        let mut child = boxes[column].first_child();
        for &item in items {
            match child {
                Some(ref widget) if widget == ordered[item] => child = widget.next_sibling(),
                _ => return false,
            }
        }
        child.is_none()
    });
    if unchanged {
        return;
    }

    for (_, card) in &block.cards {
        if let Some(parent) = card.parent().and_downcast::<gtk::Box>() {
            parent.remove(card);
        }
    }
    for (column, items) in desired.iter().enumerate() {
        for &item in items {
            boxes[column].append(ordered[item]);
        }
    }
}

/// Pick a masonry column count from the available content width so cards keep a
/// comfortable minimum size: 4 on a wide screen, then 3, 2, 1 when cramped.
fn columns_for_width(width: i32) -> usize {
    let four_columns = MIN_CARD_WIDTH_FOR_FOUR_COLUMNS
        .saturating_mul(4)
        .saturating_add(CARD_COLUMN_SPACING.saturating_mul(3));
    let three_columns = MIN_CARD_WIDTH_FOR_THREE_COLUMNS
        .saturating_mul(3)
        .saturating_add(CARD_COLUMN_SPACING.saturating_mul(2));
    let two_columns = MIN_CARD_WIDTH
        .saturating_mul(2)
        .saturating_add(CARD_COLUMN_SPACING);
    if width >= four_columns {
        4
    } else if width >= three_columns {
        3
    } else if width >= two_columns {
        2
    } else {
        1
    }
}

fn columns_for_width_with_hysteresis(width: i32, current: usize, hysteresis: i32) -> usize {
    let hysteresis = hysteresis.max(0);
    let two_columns = MIN_CARD_WIDTH
        .saturating_mul(2)
        .saturating_add(CARD_COLUMN_SPACING);
    let three_columns = MIN_CARD_WIDTH_FOR_THREE_COLUMNS
        .saturating_mul(3)
        .saturating_add(CARD_COLUMN_SPACING.saturating_mul(2));
    let four_columns = MIN_CARD_WIDTH_FOR_FOUR_COLUMNS
        .saturating_mul(4)
        .saturating_add(CARD_COLUMN_SPACING.saturating_mul(3));
    // One arm per transition rather than a computed count, so that widening the
    // window and narrowing it are written down separately: a step up needs the
    // hysteresis added, a step down needs it subtracted, and a single formula
    // would have to pick one.
    match current.clamp(1, 4) {
        1 if width >= four_columns.saturating_add(hysteresis) => 4,
        1 if width >= three_columns.saturating_add(hysteresis) => 3,
        1 if width >= two_columns.saturating_add(hysteresis) => 2,
        1 => 1,
        2 if width < two_columns.saturating_sub(hysteresis) => 1,
        2 if width >= four_columns.saturating_add(hysteresis) => 4,
        2 if width >= three_columns.saturating_add(hysteresis) => 3,
        2 => 2,
        3 if width < two_columns.saturating_sub(hysteresis) => 1,
        3 if width < three_columns.saturating_sub(hysteresis) => 2,
        3 if width >= four_columns.saturating_add(hysteresis) => 4,
        3 => 3,
        4 if width < two_columns.saturating_sub(hysteresis) => 1,
        4 if width < three_columns.saturating_sub(hysteresis) => 2,
        4 if width < four_columns.saturating_sub(hysteresis) => 3,
        4 => 4,
        _ => columns_for_width(width),
    }
}

/// What the sweep button offers, given whether its block is already checking.
fn sweep_icon(probing: bool) -> &'static str {
    if probing {
        "media-playback-stop-symbolic"
    } else {
        "power-profile-performance-symbolic"
    }
}

fn sweep_label(probing: bool) -> &'static str {
    if probing {
        "Stop checking latency"
    } else {
        "Check latency of all servers"
    }
}

fn collapse_icon(collapsed: bool) -> &'static str {
    if collapsed {
        "pan-end-symbolic"
    } else {
        "pan-down-symbolic"
    }
}

fn collapse_tooltip(collapsed: bool) -> &'static str {
    if collapsed { "Expand" } else { "Collapse" }
}

/// The sort key a badge contributes, if any: a number, a definite failure, or
/// nothing to say. Only [`sorted_by_latency`] consumes this — the badge itself
/// renders from the [`LatencyState`] directly.
fn sort_value(state: LatencyState) -> Option<Option<u32>> {
    match state {
        LatencyState::Reachable { ms, .. } | LatencyState::Tunnel { ms, .. } => Some(Some(ms)),
        LatencyState::Unreachable | LatencyState::NoNetwork => Some(None),
        // A check that never ran is not a failure to sort last — it is the
        // absence of a reading, exactly like a server nobody has measured.
        LatencyState::Unmeasured
        | LatencyState::Superseded
        | LatencyState::Checking
        | LatencyState::NotRun(_) => None,
    }
}

fn sorted_by_latency(current: &[String], latencies: &HashMap<String, Option<u32>>) -> Vec<String> {
    let mut sorted = current.to_vec();
    sorted.sort_by_key(|id| match latencies.get(id) {
        Some(Some(ms)) => (0, *ms),
        Some(None) => (1, 0),
        None => (2, 0),
    });
    sorted
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        FilterDraft, GroupKind, ROTATION_CHOICES, collapse_would_shift, columns_for_width,
        columns_for_width_with_hysteresis, distribute_columns, rotation_summary, selection_summary,
        sorted_by_latency,
    };

    /// The List/Rule radio is gone, so this is the whole of what replaced it:
    /// naming servers by hand freezes the selection, and leaving them unnamed
    /// keeps the rule matching. Nothing else decides it, which is why no dialog
    /// asks.
    #[test]
    fn the_kind_of_a_group_follows_from_the_form_rather_than_a_radio() {
        let by_hand = FilterDraft {
            countries: vec!["de".into()],
            members: vec!["a".into(), "b".into()],
            ..FilterDraft::default()
        };
        assert_eq!(by_hand.kind(), GroupKind::List);
        // Hand-picked servers win over the rule wherever this is resolved, so
        // the saved query must carry only them — a query with both is what
        // `Profile::validate` refuses.
        let query = by_hand.to_query("Mine".into(), by_hand.kind());
        assert_eq!(query.members, vec!["a".to_string(), "b".to_string()]);
        assert!(query.countries.is_empty());

        let rule = FilterDraft {
            countries: vec!["de".into()],
            ..FilterDraft::default()
        };
        assert_eq!(rule.kind(), GroupKind::Rule);
        let query = rule.to_query("DE".into(), rule.kind());
        assert!(query.members.is_empty());
        assert_eq!(query.countries, vec!["de".to_string()]);
    }

    /// The line under the form is the only place the difference is stated now,
    /// so it has to state it: frozen does not grow, a rule does.
    #[test]
    fn the_summary_says_whether_the_selection_will_keep_growing() {
        let by_hand = FilterDraft {
            members: vec!["a".into()],
            ..FilterDraft::default()
        };
        let text = selection_summary(&by_hand, 1, false);
        assert!(text.starts_with("1 server chosen by hand"), "{text}");
        assert!(text.contains("do not join"), "{text}");

        let rule = FilterDraft {
            countries: vec!["de".into(), "nl".into()],
            ..FilterDraft::default()
        };
        let text = selection_summary(&rule, 12, false);
        assert!(text.starts_with("DE, NL: 12 servers"), "{text}");
        assert!(text.contains("join on their own"), "{text}");

        // An empty rule would mean every server on the machine. Saving it is
        // refused, but the line still has to say what is on screen.
        let text = selection_summary(&FilterDraft::default(), 42, false);
        assert_eq!(text, "Every server — 42 right now.");

        // Favourites with nothing starred holds nothing — the one group for
        // which "every server" would be exactly backwards.
        let text = selection_summary(&FilterDraft::default(), 42, true);
        assert!(text.starts_with("No servers yet"), "{text}");
    }

    #[test]
    fn the_rotation_label_never_promises_more_nodes_than_the_pool_has() {
        assert_eq!(rotation_summary(6, 42), "6 of 42 active");
        // Six over three would advertise a spread the core cannot produce:
        // `expected` above the live count returns exactly the live ones.
        assert_eq!(rotation_summary(6, 3), "All 3 nodes");
        assert_eq!(rotation_summary(6, 6), "All 6 nodes");
        // A Favourites group with one server in it reads as a sentence.
        assert_eq!(rotation_summary(6, 1), "All 1 node");
        // 0 is what `expected` has always meant on disk: every live node.
        assert_eq!(rotation_summary(0, 42), "All nodes");
    }

    #[test]
    fn the_rotation_picker_offers_all_nodes_exactly_once() {
        assert_eq!(
            ROTATION_CHOICES.iter().filter(|value| **value == 0).count(),
            1
        );
        // Ascending, with "all" last: the list reads as widening.
        let capped = &ROTATION_CHOICES[..ROTATION_CHOICES.len() - 1];
        assert!(capped.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(capped.contains(&oxidom_core::pool::DEFAULT_POOL_ROTATION));
    }

    #[test]
    fn distribution_reads_row_major_and_keeps_column_order() {
        assert_eq!(
            distribute_columns(7, 3),
            vec![vec![0, 3, 6], vec![1, 4], vec![2, 5]]
        );
        assert_eq!(distribute_columns(3, 1), vec![vec![0, 1, 2]]);
        assert_eq!(distribute_columns(0, 2), vec![Vec::<usize>::new(); 2]);
    }

    #[test]
    fn collapse_shifts_only_cards_above_in_the_same_column_or_earlier_groups() {
        // Earlier group always sits above.
        assert!(collapse_would_shift((0, 5), (1, 0), 3));
        // Same group, same column (0 and 3 with n=3), earlier slot.
        assert!(collapse_would_shift((1, 0), (1, 3), 3));
        // Same group, different column.
        assert!(!collapse_would_shift((1, 1), (1, 3), 3));
        // Same column but below.
        assert!(!collapse_would_shift((1, 3), (1, 0), 3));
        // Later group never shifts an earlier one.
        assert!(!collapse_would_shift((2, 0), (1, 0), 3));
    }

    #[test]
    fn manual_latency_sort_is_stable_and_puts_failures_last() {
        let current = vec![
            "unmeasured-a".to_string(),
            "slow-a".to_string(),
            "unreachable".to_string(),
            "fast".to_string(),
            "slow-b".to_string(),
            "unmeasured-b".to_string(),
        ];
        let latencies = HashMap::from([
            ("slow-a".to_string(), Some(420)),
            ("unreachable".to_string(), None),
            ("fast".to_string(), Some(35)),
            ("slow-b".to_string(), Some(420)),
        ]);

        assert_eq!(
            sorted_by_latency(&current, &latencies),
            vec![
                "fast",
                "slow-a",
                "slow-b",
                "unreachable",
                "unmeasured-a",
                "unmeasured-b",
            ]
        );
    }

    #[test]
    fn three_columns_start_only_after_the_wider_breakpoint() {
        assert_eq!(columns_for_width(511), 1);
        assert_eq!(columns_for_width(512), 2);
        assert_eq!(columns_for_width(923), 2);
        assert_eq!(columns_for_width(924), 3);
    }

    #[test]
    fn column_hysteresis_prevents_threshold_flapping() {
        assert_eq!(columns_for_width_with_hysteresis(527, 1, 16), 1);
        assert_eq!(columns_for_width_with_hysteresis(528, 1, 16), 2);
        assert_eq!(columns_for_width_with_hysteresis(511, 2, 16), 2);
        assert_eq!(columns_for_width_with_hysteresis(495, 2, 16), 1);
        assert_eq!(columns_for_width_with_hysteresis(939, 2, 16), 2);
        assert_eq!(columns_for_width_with_hysteresis(940, 2, 16), 3);
        assert_eq!(columns_for_width_with_hysteresis(909, 3, 16), 3);
        assert_eq!(columns_for_width_with_hysteresis(907, 3, 16), 2);
        // 1316 = MIN_CARD_WIDTH_FOR_FOUR_COLUMNS * 4 + CARD_COLUMN_SPACING * 3,
        // so widening asks for 1332 and narrowing gives up below 1300.
        assert_eq!(columns_for_width_with_hysteresis(1331, 3, 16), 3);
        assert_eq!(columns_for_width_with_hysteresis(1332, 3, 16), 4);
        assert_eq!(columns_for_width_with_hysteresis(1300, 4, 16), 4);
        assert_eq!(columns_for_width_with_hysteresis(1299, 4, 16), 3);
        // A hard narrow from four skips the arms in between rather than
        // stopping at the neighbouring one.
        assert_eq!(columns_for_width_with_hysteresis(900, 4, 16), 2);
        assert_eq!(columns_for_width_with_hysteresis(495, 4, 16), 1);
    }

    #[test]
    fn four_columns_start_only_after_the_widest_breakpoint() {
        assert_eq!(columns_for_width(1315), 3);
        assert_eq!(columns_for_width(1316), 4);
    }

    #[test]
    fn a_window_parked_between_three_and_four_columns_keeps_what_it_has() {
        // The raw breakpoint sits inside the hysteresis band on both sides, so
        // a window resting exactly on it must be answered with whatever is
        // already packed rather than with the count the width alone implies.
        // Answering 4 here regardless is what makes a grid flicker.
        for width in 1300..=1331 {
            assert_eq!(columns_for_width_with_hysteresis(width, 3, 16), 3);
            assert_eq!(columns_for_width_with_hysteresis(width, 4, 16), 4);
        }
    }
}
