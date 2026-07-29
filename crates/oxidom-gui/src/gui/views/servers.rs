use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use oxidom_core::model::Subscription;
use oxidom_core::pool::PoolQuery;

use super::super::group::subscription_description;
use super::super::prefs::GuiPrefs;
use super::super::reduce::{
    FilterOption, ServerProfiles, available_countries, available_protocols,
    available_subscriptions, filtered_ids, filters_to_query, moved_subscription,
    ordered_subscriptions,
};
use super::super::server_card::{
    CARD_MEASURE_WIDTH, COMPACT_CARD_HEIGHT, CardConnectionState, LatencyState, ServerCard,
};

const CARD_COLUMN_SPACING: i32 = 12;
const CARD_ROW_SPACING: i32 = 12;
const MIN_CARD_WIDTH: i32 = 250;
const MIN_CARD_WIDTH_FOR_THREE_COLUMNS: i32 = 300;
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
    pub recheck: Rc<dyn Fn(Vec<String>)>,
    pub refresh: Rc<dyn Fn(String)>,
    pub set_alias: Rc<dyn Fn(String, String)>,
    pub create_pool: Rc<dyn Fn(PoolQuery)>,
}

/// One subscription block. Cards live in independent vertical column boxes
/// (equal widths via the homogeneous horizontal box), so a card growing or
/// shrinking moves only its own column's tail — columns never couple through
/// shared row heights, and a card's slot changes only on repack (rebuild,
/// filter, sort, column-count change), never on selection.
#[derive(Clone)]
struct GroupUi {
    /// Subscription id. The group's own actions address it by id rather than by
    /// position, because reordering moves the position out from under them.
    id: String,
    root: gtk::Widget,
    columns_box: gtk::Box,
    column_boxes: Rc<RefCell<Vec<gtk::Box>>>,
    cards: Vec<(String, gtk::Widget)>,
    display_order: Rc<RefCell<Vec<String>>>,
    sort_button: gtk::Button,
    sort_generation: Rc<Cell<u64>>,
}

type BrowseCallback = Rc<RefCell<Option<Box<dyn Fn()>>>>;

#[derive(Clone)]
pub struct ServersView {
    pub root: gtk::ScrolledWindow,
    content: gtk::Box,
    cards: Rc<RefCell<HashMap<String, ServerCard>>>,
    groups: Rc<RefCell<Vec<GroupUi>>>,
    subscriptions: Rc<RefCell<Vec<Subscription>>>,
    /// Lowercased "name transport protocol address:port country" per server.
    /// The search matches this, never transient widget text like the
    /// "Connected" badge — otherwise connecting would change search results.
    search_texts: Rc<RefCell<HashMap<String, String>>>,
    query: Rc<RefCell<String>>,
    filter_countries: Rc<RefCell<Vec<String>>>,
    filter_protocols: Rc<RefCell<Vec<String>>>,
    filter_subscriptions: Rc<RefCell<Vec<String>>>,
    current_filter: Rc<RefCell<PoolQuery>>,
    create_pool: Rc<RefCell<Option<gtk::Button>>>,
    /// Number of card columns; driven by the window width (1, 2, or 3).
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
    /// Which subscription groups are collapsed, persisted to disk so it
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

        Self {
            root,
            content,
            no_matches,
            on_browse_subscriptions: Rc::new(RefCell::new(None)),
            cards: Rc::new(RefCell::new(HashMap::new())),
            groups: Rc::new(RefCell::new(Vec::new())),
            subscriptions: Rc::new(RefCell::new(subscriptions.to_vec())),
            search_texts: Rc::new(RefCell::new(HashMap::new())),
            query: Rc::new(RefCell::new(String::new())),
            filter_countries: Rc::new(RefCell::new(Vec::new())),
            filter_protocols: Rc::new(RefCell::new(Vec::new())),
            filter_subscriptions: Rc::new(RefCell::new(Vec::new())),
            current_filter: Rc::new(RefCell::new(PoolQuery::default())),
            create_pool: Rc::new(RefCell::new(None)),
            columns: Rc::new(Cell::new(1)),
            pending_columns: Rc::new(Cell::new(1)),
            column_update_scheduled: Rc::new(Cell::new(false)),
            resize_generation: Rc::new(Cell::new(0)),
            latencies: Rc::new(RefCell::new(HashMap::new())),
            selected: Rc::new(RefCell::new(None)),
            requested_selected: Rc::new(RefCell::new(None)),
            prefs: Rc::new(RefCell::new(GuiPrefs::load(subscriptions))),
        }
    }

    /// Pick the column count from the width the window gives this view.
    /// Driven from window.rs — deriving it from our own allocation would form
    /// a feedback loop with the content's minimum width and deadlock the
    /// window's ability to shrink.
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
        let count = count.clamp(1, 3);
        if self.columns.get() == count {
            return;
        }
        self.columns.set(count);
        self.pending_columns.set(count);
        for group in self.groups.borrow().iter() {
            repack_group(group, count);
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
        self.cards.borrow_mut().clear();
        self.groups.borrow_mut().clear();
        *self.subscriptions.borrow_mut() = subscriptions.to_vec();
        self.search_texts.borrow_mut().clear();
        self.create_pool.borrow_mut().take();
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

        self.content
            .append(&self.filter_bar(subscriptions, callbacks.create_pool.clone()));

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
            // The local "My servers" group has no URL to re-fetch.
            update.set_visible(!subscription.url.is_empty());
            update.connect_clicked({
                let cb = callbacks.refresh.clone();
                let id = subscription.id.clone();
                move |_| cb(id.clone())
            });
            let speed = gtk::Button::builder()
                .icon_name("power-profile-performance-symbolic")
                .tooltip_text("Check latency of all servers")
                .valign(gtk::Align::Center)
                .sensitive(!ids.is_empty())
                .css_classes(["flat", "circular", "server-action"])
                .build();
            speed.update_property(&[gtk::accessible::Property::Label(
                "Check latency of all servers",
            )]);
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
                move |_| view.sort_group(&subscription_id)
            });
            let reorder = gtk::MenuButton::builder()
                .icon_name("view-more-symbolic")
                .tooltip_text("Move this group")
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular", "server-action"])
                .build();
            reorder.update_property(&[gtk::accessible::Property::Label("Move this group")]);
            reorder.set_popover(Some(&self.reorder_popover(&subscription.id)));
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
            // The name and the quota line are the group's whole width minus four
            // icon buttons, and hitting a 24px chevron to fold a group away was
            // the only way to do it. They become the expander instead; the
            // chevron stays because it is what says the group folds at all.
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
            // never disagree about whether the group is folded.
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

            let mut group_cards: Vec<(String, gtk::Widget)> = Vec::new();
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
                    on_select,
                    on_activate,
                    on_ping,
                    on_set_alias,
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
                group_cards.push((id.clone(), card.root.clone().upcast::<gtk::Widget>()));
                self.cards.borrow_mut().insert(server.id.clone(), card);
            }

            let group = gtk::Box::new(gtk::Orientation::Vertical, 12);
            group.set_hexpand(true);
            group.append(&header);
            group.append(&columns_box);
            self.content.append(&group);
            let group_ui = GroupUi {
                id: subscription.id.clone(),
                root: group.upcast::<gtk::Widget>(),
                columns_box,
                column_boxes: Rc::new(RefCell::new(Vec::new())),
                display_order: Rc::new(RefCell::new(
                    group_cards.iter().map(|(id, _)| id.clone()).collect(),
                )),
                cards: group_cards,
                sort_button: sort,
                sort_generation: Rc::new(Cell::new(0)),
            };
            repack_group(&group_ui, self.columns.get());
            self.groups.borrow_mut().push(group_ui);
        }
        // Last child, so it sits below the groups it stands in for.
        self.content.append(&self.no_matches);
        self.apply_filter();
        if let Some(server_id) = selected_id {
            self.set_selected_immediately(server_id);
        }
        self.schedule_expanded_remeasure();
    }

    fn filter_bar(
        &self,
        subscriptions: &[Subscription],
        create_pool: Rc<dyn Fn(PoolQuery)>,
    ) -> gtk::Widget {
        let countries = available_countries(subscriptions)
            .into_iter()
            .map(|value| FilterOption {
                label: value.to_ascii_uppercase(),
                value,
            })
            .collect::<Vec<_>>();
        let protocols = available_protocols(subscriptions)
            .into_iter()
            .map(|value| FilterOption {
                label: value.clone(),
                value,
            })
            .collect::<Vec<_>>();
        let subscriptions = available_subscriptions(subscriptions);

        let flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .column_spacing(8)
            .row_spacing(8)
            .max_children_per_line(4)
            .min_children_per_line(1)
            .css_classes(["server-filter-bar"])
            .build();
        flow.insert(
            &filter_menu(
                "Country",
                &countries,
                self.filter_countries.clone(),
                self.clone(),
            ),
            -1,
        );
        flow.insert(
            &filter_menu(
                "Protocol",
                &protocols,
                self.filter_protocols.clone(),
                self.clone(),
            ),
            -1,
        );
        flow.insert(
            &filter_menu(
                "Subscription",
                &subscriptions,
                self.filter_subscriptions.clone(),
                self.clone(),
            ),
            -1,
        );

        let button = gtk::Button::builder()
            .label("Create pool from this filter")
            // A pool is a stored query, not a stored list. Country, protocol
            // and subscription keep matching after a refresh, so a server
            // added later joins the pool; the text box has no equivalent and
            // is frozen into exclusions instead. Saying so here is cheaper
            // than a user discovering it from a stale badge.
            .tooltip_text(
                "Create a profile whose pool matches the visible servers. Country, protocol \
                 and subscription keep matching later, so servers added by a future refresh \
                 can join; the search text is frozen as exclusions and does not.",
            )
            .css_classes(["suggested-action", "pill"])
            .build();
        button.connect_clicked({
            let current_filter = self.current_filter.clone();
            move |_| create_pool(current_filter.borrow().clone())
        });
        flow.insert(&button, -1);
        *self.create_pool.borrow_mut() = Some(button);
        flow.upcast()
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
        let query = filters_to_query(
            &self.subscriptions.borrow(),
            &self.filter_subscriptions.borrow(),
            &self.filter_countries.borrow(),
            &self.filter_protocols.borrow(),
            &self.search_texts.borrow(),
            &self.query.borrow(),
        );
        let filtered = filtered_ids(&query, &self.subscriptions.borrow())
            .into_iter()
            .collect::<HashSet<_>>();
        *self.current_filter.borrow_mut() = query;
        let selected = self.selected.borrow().clone();
        let mut total_visible = 0usize;
        {
            for group in self.groups.borrow().iter() {
                let mut visible = 0;
                for (id, card) in &group.cards {
                    let matches = filtered.contains(id);
                    card.set_visible(matches);
                    if matches {
                        visible += 1;
                    }
                }
                group.root.set_visible(visible > 0);
                total_visible += visible;
            }
        }
        // A query that matches nothing used to leave a blank page.
        self.no_matches.set_visible(total_visible == 0);
        if let Some(button) = self.create_pool.borrow().as_ref() {
            button.set_sensitive(total_visible > 0);
        }
        // Cards mid-collapse from a recent selection switch reflow anyway —
        // snap them closed before repacking.
        for (id, card) in self.cards.borrow().iter() {
            if Some(id.as_str()) != selected.as_deref() && card.is_expanded() {
                card.collapse_immediately();
            }
        }
        for group in self.groups.borrow().iter() {
            repack_group(group, self.columns.get());
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

    pub fn set_latency_state(&self, server_id: &str, state: LatencyState) {
        if let Some(card) = self.cards.borrow().get(server_id) {
            card.set_latency_state(state);
        }
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
        let groups = self.groups.borrow();
        for (group_index, group) in groups.iter().enumerate() {
            let display_order = group.display_order.borrow();
            let mut visible_index = 0;
            for id in display_order.iter() {
                let Some((_, widget)) = group.cards.iter().find(|(card_id, _)| card_id == id)
                else {
                    continue;
                };
                if !widget.get_visible() {
                    continue;
                }
                if id == server_id {
                    return Some((group_index, visible_index));
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
        let groups = self.groups.borrow();
        let total = groups
            .iter()
            .map(|group| group.columns_box.allocated_width())
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
    fn move_group(&self, subscription_id: &str, delta: isize) {
        let visible: Vec<String> = self
            .groups
            .borrow()
            .iter()
            .map(|group| group.id.clone())
            .collect();
        let order = moved_subscription(&visible, subscription_id, delta);
        if order == visible {
            return;
        }

        {
            let mut groups = self.groups.borrow_mut();
            groups.sort_by_key(|group| {
                order
                    .iter()
                    .position(|id| id == &group.id)
                    .unwrap_or(usize::MAX)
            });
            // `content` also holds the filter bar (first) and the "no matches"
            // page (last); re-seating the groups after the filter bar in order
            // leaves both where they belong.
            let mut previous = self.content.first_child();
            for group in groups.iter() {
                self.content
                    .reorder_child_after(&group.root, previous.as_ref());
                previous = Some(group.root.clone());
            }
        }

        let mut prefs = self.prefs.borrow_mut();
        prefs.subscription_order = order;
        if let Err(error) = prefs.save() {
            log::warn!("could not save gui prefs: {error:#}");
        }
    }

    fn reorder_popover(&self, subscription_id: &str) -> gtk::Popover {
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
                    view.move_group(&subscription_id, delta);
                }
            });
            list.append(&button);
        }
        popover
    }

    /// Manually capture and apply a latency order. Later measurements update only
    /// their badges until the user presses sort again.
    pub fn sort_group(&self, subscription_id: &str) {
        let Some(group) = self
            .groups
            .borrow()
            .iter()
            .find(|group| group.id == subscription_id)
            .cloned()
        else {
            return;
        };
        let sorted = sorted_by_latency(&group.display_order.borrow(), &self.latencies.borrow());
        let generation = group.sort_generation.get().wrapping_add(1);
        group.sort_generation.set(generation);
        group.sort_button.set_sensitive(false);

        if !adw::is_animations_enabled(&group.columns_box) {
            *group.display_order.borrow_mut() = sorted;
            repack_group(&group, self.columns.get());
            group.sort_button.set_sensitive(true);
            return;
        }

        let target = adw::CallbackAnimationTarget::new({
            let columns_box = group.columns_box.clone();
            move |value| columns_box.set_opacity(value)
        });
        let animation = adw::TimedAnimation::new(
            &group.columns_box,
            group.columns_box.opacity(),
            0.0,
            90,
            target,
        );
        animation.set_easing(adw::Easing::EaseInCubic);
        animation.connect_done({
            let view = self.clone();
            let group = group.clone();
            move |_| {
                if group.sort_generation.get() != generation {
                    return;
                }
                *group.display_order.borrow_mut() = sorted.clone();
                repack_group(&group, view.columns.get());

                let target = adw::CallbackAnimationTarget::new({
                    let columns_box = group.columns_box.clone();
                    move |value| columns_box.set_opacity(value)
                });
                let fade_in = adw::TimedAnimation::new(&group.columns_box, 0.0, 1.0, 130, target);
                fade_in.set_easing(adw::Easing::EaseOutCubic);
                fade_in.connect_done({
                    let group = group.clone();
                    move |_| {
                        if group.sort_generation.get() == generation {
                            group.sort_button.set_sensitive(true);
                        }
                    }
                });
                fade_in.play();
            }
        });
        animation.play();
    }
}

fn filter_menu(
    title: &str,
    options: &[FilterOption],
    selected: Rc<RefCell<Vec<String>>>,
    view: ServersView,
) -> gtk::MenuButton {
    let button = gtk::MenuButton::builder()
        .label(filter_label(title, options, &selected.borrow()))
        .sensitive(!options.is_empty())
        .css_classes(["flat", "filter-menu"])
        .build();
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.set_margin_top(8);
    list.set_margin_bottom(8);
    list.set_margin_start(8);
    list.set_margin_end(8);
    for option in options {
        let check = gtk::CheckButton::with_label(&option.label);
        check.set_active(selected.borrow().contains(&option.value));
        check.connect_toggled({
            let selected = selected.clone();
            let value = option.value.clone();
            let options = options.to_vec();
            let title = title.to_string();
            let button = button.clone();
            let view = view.clone();
            move |check| {
                let mut values = selected.borrow_mut();
                if check.is_active() {
                    if !values.contains(&value) {
                        values.push(value.clone());
                    }
                } else {
                    values.retain(|selected| selected != &value);
                }
                button.set_label(&filter_label(&title, &options, &values));
                drop(values);
                view.apply_filter();
            }
        });
        list.append(&check);
    }
    let popover = gtk::Popover::builder().child(&list).build();
    button.set_popover(Some(&popover));
    button
}

fn filter_label(title: &str, options: &[FilterOption], selected: &[String]) -> String {
    match selected {
        [] => format!("{title}: All"),
        [only] => options
            .iter()
            .find(|option| option.value == *only)
            .map_or_else(|| format!("{title}: {only}"), |option| option.label.clone()),
        many => format!("{title}: {}", many.len()),
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
fn repack_group(group: &GroupUi, columns: usize) {
    let columns = columns.max(1);
    {
        let mut boxes = group.column_boxes.borrow_mut();
        if boxes.len() != columns {
            for (_, card) in &group.cards {
                if let Some(parent) = card.parent().and_downcast::<gtk::Box>() {
                    parent.remove(card);
                }
            }
            while let Some(child) = group.columns_box.first_child() {
                group.columns_box.remove(&child);
            }
            boxes.clear();
            for _ in 0..columns {
                let column = gtk::Box::new(gtk::Orientation::Vertical, CARD_ROW_SPACING);
                column.set_hexpand(true);
                group.columns_box.append(&column);
                boxes.push(column);
            }
        }
    }

    let boxes = group.column_boxes.borrow();
    let display_order = group.display_order.borrow();
    // Index once: this runs on every resize and every expand/collapse, and a
    // linear scan per id turns a large subscription into O(cards²) work in
    // the middle of an animation.
    let by_id: HashMap<&str, &gtk::Widget> = group
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

    for (_, card) in &group.cards {
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
/// comfortable minimum size: 3 wide, 2 mid, 1 when cramped.
fn columns_for_width(width: i32) -> usize {
    let three_columns = MIN_CARD_WIDTH_FOR_THREE_COLUMNS
        .saturating_mul(3)
        .saturating_add(CARD_COLUMN_SPACING.saturating_mul(2));
    let two_columns = MIN_CARD_WIDTH
        .saturating_mul(2)
        .saturating_add(CARD_COLUMN_SPACING);
    if width >= three_columns {
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
    match current.clamp(1, 3) {
        1 if width >= three_columns.saturating_add(hysteresis) => 3,
        1 if width >= two_columns.saturating_add(hysteresis) => 2,
        1 => 1,
        2 if width < two_columns.saturating_sub(hysteresis) => 1,
        2 if width >= three_columns.saturating_add(hysteresis) => 3,
        2 => 2,
        3 if width < two_columns.saturating_sub(hysteresis) => 1,
        3 if width < three_columns.saturating_sub(hysteresis) => 2,
        3 => 3,
        _ => columns_for_width(width),
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
        LatencyState::Unmeasured | LatencyState::Superseded | LatencyState::Checking => None,
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
        collapse_would_shift, columns_for_width, columns_for_width_with_hysteresis,
        distribute_columns, sorted_by_latency,
    };

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
    }
}
