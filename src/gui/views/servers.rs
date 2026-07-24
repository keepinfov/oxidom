use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use crate::model::Subscription;

use super::super::group::subscription_description;
use super::super::server_card::{
    CARD_MEASURE_WIDTH, COMPACT_CARD_HEIGHT, CardConnectionState, LatencyState, ServerCard,
};

const CARD_COLUMN_SPACING: i32 = 12;
const CARD_ROW_SPACING: i32 = 12;
const MIN_CARD_WIDTH: i32 = 320;
const COLUMN_HYSTERESIS: i32 = 16;
const RESIZE_SETTLE_MS: u64 = 120;

/// `select` only inspects/expands a card. `activate` independently connects,
/// switches, or disconnects its server.
#[derive(Clone)]
pub struct CardCallbacks {
    pub select: Rc<dyn Fn(String)>,
    pub activate: Rc<dyn Fn(String)>,
    pub ping: Rc<dyn Fn(String)>,
    pub recheck: Rc<dyn Fn(Vec<String>)>,
    pub refresh: Rc<dyn Fn(String)>,
}

/// One subscription block: the header widget (hidden when filtered empty), the
/// grid that lays out cards, and the cards in source order.
#[derive(Clone)]
struct GroupUi {
    root: gtk::Widget,
    grid: gtk::Grid,
    reserver: gtk::Box,
    cards: Vec<(String, gtk::Widget)>,
    display_order: Rc<RefCell<Vec<String>>>,
    sort_button: gtk::Button,
    sort_generation: Rc<Cell<u64>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridAnchor {
    column: i32,
    row: i32,
}

#[derive(Clone)]
pub struct ServersView {
    pub root: gtk::ScrolledWindow,
    viewport: gtk::Viewport,
    content: gtk::Box,
    cards: Rc<RefCell<HashMap<String, ServerCard>>>,
    groups: Rc<RefCell<Vec<GroupUi>>>,
    /// Lowercased "name transport protocol address:port country" per server.
    /// The search matches this, never transient widget text like the
    /// "Connected" badge — otherwise connecting would change search results.
    search_texts: Rc<RefCell<HashMap<String, String>>>,
    query: Rc<RefCell<String>>,
    /// Number of grid columns; adapts to window width (1, 2, or 3).
    columns: Rc<Cell<usize>>,
    pending_columns: Rc<Cell<usize>>,
    column_update_scheduled: Rc<Cell<bool>>,
    viewport_width: Rc<Cell<i32>>,
    layout_sync_scheduled: Rc<Cell<bool>>,
    resize_generation: Rc<Cell<u64>>,
    /// Latest completed measurements. This never changes display order by itself.
    latencies: Rc<RefCell<HashMap<String, Option<u32>>>>,
    /// The expanded/selected card spans complete grid rows so its content never
    /// overlaps a neighboring card.
    selected: Rc<RefCell<Option<String>>>,
    requested_selected: Rc<RefCell<Option<String>>>,
    selected_anchor: Rc<RefCell<Option<GridAnchor>>>,
    selection_generation: Rc<Cell<u64>>,
}

impl ServersView {
    pub fn new() -> Self {
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
        let view = Self {
            root,
            viewport,
            content,
            cards: Rc::new(RefCell::new(HashMap::new())),
            groups: Rc::new(RefCell::new(Vec::new())),
            search_texts: Rc::new(RefCell::new(HashMap::new())),
            query: Rc::new(RefCell::new(String::new())),
            columns: Rc::new(Cell::new(1)),
            pending_columns: Rc::new(Cell::new(1)),
            column_update_scheduled: Rc::new(Cell::new(false)),
            viewport_width: Rc::new(Cell::new(-1)),
            layout_sync_scheduled: Rc::new(Cell::new(false)),
            resize_generation: Rc::new(Cell::new(0)),
            latencies: Rc::new(RefCell::new(HashMap::new())),
            selected: Rc::new(RefCell::new(None)),
            requested_selected: Rc::new(RefCell::new(None)),
            selected_anchor: Rc::new(RefCell::new(None)),
            selection_generation: Rc::new(Cell::new(0)),
        };

        // Adapt the grid column count (1/2/3) to the viewport width. With the
        // horizontal scrollbar disabled the hadjustment's page-size tracks the
        // visible content width, and it notifies on every resize.
        let hadj = view.root.hadjustment();
        hadj.connect_page_size_notify({
            let view = view.clone();
            move |_| {
                view.update_columns_for_viewport();
            }
        });
        view.root.connect_map({
            let view = view.clone();
            move |_| {
                view.update_columns_for_viewport();
            }
        });

        view
    }

    fn update_columns_for_viewport(&self) {
        let width = self.viewport.width();
        if width <= 0 || self.viewport_width.replace(width) == width {
            return;
        }
        let usable_width = width
            .saturating_sub(self.content.margin_start())
            .saturating_sub(self.content.margin_end());
        let columns = columns_for_width_with_hysteresis(
            usable_width,
            self.pending_columns.get(),
            COLUMN_HYSTERESIS,
        );
        if columns != self.columns.get() {
            self.schedule_columns(columns);
        } else if columns == 1 {
            self.schedule_single_column_resize();
        }
    }

    fn invalidate_layout(&self) {
        self.viewport_width.set(-1);
        if self.layout_sync_scheduled.replace(true) {
            return;
        }
        let view = self.clone();
        glib::idle_add_local_once(move || {
            view.layout_sync_scheduled.set(false);
            view.update_columns_for_viewport();
        });
    }

    fn schedule_single_column_resize(&self) {
        let generation = self.resize_generation.get().wrapping_add(1);
        self.resize_generation.set(generation);
        let view = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(RESIZE_SETTLE_MS), move || {
            if view.resize_generation.get() == generation && view.columns.get() == 1 {
                view.refresh_single_column_geometry();
            }
        });
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

    pub fn rebuild(
        &self,
        subscriptions: &[Subscription],
        connected_id: Option<&str>,
        selected_id: Option<&str>,
        latencies: &HashMap<String, Option<u32>>,
        checking: &HashSet<String>,
        callbacks: CardCallbacks,
    ) {
        self.selection_generation
            .set(self.selection_generation.get().wrapping_add(1));
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        self.cards.borrow_mut().clear();
        self.groups.borrow_mut().clear();
        self.search_texts.borrow_mut().clear();
        *self.latencies.borrow_mut() = latencies.clone();
        *self.selected.borrow_mut() = selected_id.map(str::to_string);
        *self.requested_selected.borrow_mut() = selected_id.map(str::to_string);
        *self.selected_anchor.borrow_mut() = None;

        if subscriptions.is_empty() {
            let empty = adw::StatusPage::builder()
                .icon_name("network-server-symbolic")
                .title("No servers yet")
                .description("Add a subscription to start browsing servers.")
                .vexpand(true)
                .build();
            self.content.append(&empty);
            self.invalidate_layout();
            return;
        }

        for (index, subscription) in subscriptions.iter().enumerate() {
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
                move |_| view.sort_group(index)
            });
            let name_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            name_row.set_hexpand(true);
            name_row.append(&heading);
            name_row.append(&update);
            name_row.append(&speed);
            name_row.append(&sort);

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
            title_box.append(&name_row);
            title_box.append(&description);

            let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            header.set_hexpand(true);
            header.append(&title_box);

            let grid = gtk::Grid::builder()
                .column_homogeneous(true)
                .row_homogeneous(false)
                .column_spacing(CARD_COLUMN_SPACING)
                .row_spacing(CARD_ROW_SPACING)
                .hexpand(true)
                .build();
            let reserver = gtk::Box::new(gtk::Orientation::Vertical, 0);
            reserver.set_can_target(false);
            reserver.set_focusable(false);
            reserver.set_opacity(0.0);
            reserver.set_visible(false);
            reserver.set_valign(gtk::Align::Start);
            grid.attach(&reserver, 0, 0, 1, 1);

            let mut group_cards: Vec<(String, gtk::Widget)> = Vec::new();
            for server in &subscription.servers {
                let id = server.id.clone();
                let latency_state = latency_state(&id, latencies, checking);
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
                let connection_state = match connected_id {
                    Some(connected_id) if connected_id == id => CardConnectionState::ConnectedHere,
                    Some(_) => CardConnectionState::ConnectedElsewhere,
                    None => CardConnectionState::Disconnected,
                };
                let card = ServerCard::new(
                    server,
                    connection_state,
                    latency_state,
                    on_select,
                    on_activate,
                    on_ping,
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
            group.append(&grid);
            self.content.append(&group);
            let group_ui = GroupUi {
                root: group.upcast::<gtk::Widget>(),
                grid,
                reserver,
                display_order: Rc::new(RefCell::new(
                    group_cards.iter().map(|(id, _)| id.clone()).collect(),
                )),
                cards: group_cards,
                sort_button: sort,
                sort_generation: Rc::new(Cell::new(0)),
            };
            fill_grid(&group_ui, self.columns.get(), selected_id, None, None, true);
            self.groups.borrow_mut().push(group_ui);
        }
        self.apply_filter();
        if let Some(server_id) = selected_id {
            self.set_selected_immediately(server_id);
        }
        self.invalidate_layout();
    }

    pub fn set_query(&self, query: &str) {
        *self.query.borrow_mut() = query.trim().to_lowercase();
        self.apply_filter();
    }

    fn apply_filter(&self) {
        let query = self.query.borrow().clone();
        let selected = self.selected.borrow().clone();
        let selected_anchor = *self.selected_anchor.borrow();
        let geometry = selected
            .as_deref()
            .and_then(|id| self.expanded_geometry_for(id));
        let search_texts = self.search_texts.borrow();
        for group in self.groups.borrow().iter() {
            let mut visible = 0;
            for (id, card) in &group.cards {
                let matches = query.is_empty()
                    || search_texts
                        .get(id)
                        .is_some_and(|text| text.contains(&query));
                card.set_visible(matches);
                if matches {
                    visible += 1;
                }
            }
            group.root.set_visible(visible > 0);
            // Re-pack so only the matching cards occupy grid cells; filtered-out
            // cards leave no holes.
            fill_grid(
                group,
                self.columns.get(),
                selected.as_deref(),
                if query.is_empty() {
                    selected_anchor
                } else {
                    None
                },
                geometry,
                true,
            );
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
                    && let Some(geometry) = self.expanded_geometry_for(selected_id)
                {
                    card.set_expanded_immediately(geometry.height);
                }
            }
        }
        self.invalidate_layout();
    }

    pub fn set_latency_state(&self, server_id: &str, state: LatencyState) {
        if let Some(card) = self.cards.borrow().get(server_id) {
            card.set_latency_state(state);
        }
        match state {
            LatencyState::Reachable(ms) => {
                self.latencies
                    .borrow_mut()
                    .insert(server_id.to_string(), Some(ms));
            }
            LatencyState::Unreachable => {
                self.latencies
                    .borrow_mut()
                    .insert(server_id.to_string(), None);
            }
            LatencyState::Unmeasured => {
                self.latencies.borrow_mut().remove(server_id);
            }
            LatencyState::Checking => {}
        }
    }

    /// Reflect the connection on every card in one pass: a `connecting` server
    /// wins, otherwise `active` decides Connected/Elsewhere/Disconnected.
    pub fn set_connection(&self, active: Option<&str>, connecting: Option<&str>) {
        for (id, card) in self.cards.borrow().iter() {
            let state = if let Some(connecting_id) = connecting {
                if connecting_id == id {
                    CardConnectionState::Connecting
                } else {
                    CardConnectionState::Disconnected
                }
            } else {
                match active {
                    Some(active_id) if active_id == id => CardConnectionState::ConnectedHere,
                    Some(_) => CardConnectionState::ConnectedElsewhere,
                    None => CardConnectionState::Disconnected,
                }
            };
            card.set_connection_state(state);
        }
    }

    pub fn set_selected(&self, server_id: Option<&str>) {
        if self.requested_selected.borrow().as_deref() == server_id {
            return;
        }

        let generation = self.selection_generation.get().wrapping_add(1);
        self.selection_generation.set(generation);
        let next = server_id.map(str::to_string);
        *self.requested_selected.borrow_mut() = next.clone();
        let next_anchor = server_id.and_then(|id| self.anchor_for(id));
        let current = self.selected.borrow().clone();

        if current == next {
            if let Some(server_id) = current {
                let card = self.cards.borrow().get(&server_id).cloned();
                let target_height = self
                    .expanded_geometry_for(&server_id)
                    .map(|geometry| geometry.height);
                if let (Some(card), Some(target_height)) = (card, target_height) {
                    card.expand(target_height, None);
                }
            }
            return;
        }

        let current_card = current
            .as_deref()
            .and_then(|id| self.cards.borrow().get(id).cloned());
        let geometry = next
            .as_deref()
            .and_then(|id| self.expanded_geometry_for(id));
        *self.selected.borrow_mut() = next.clone();
        *self.selected_anchor.borrow_mut() = next_anchor;

        // Re-plan the grid at the moment of the click, without the reserver:
        // the new card moves into its full-row span and the old one returns to
        // a single row before either height animation starts. Grid rows are
        // shared across columns, so their heights now follow both animations
        // coherently instead of drifting with the stale plan and snapping when
        // the expand finishes.
        for group in self.groups.borrow().iter() {
            fill_grid(
                group,
                self.columns.get(),
                next.as_deref(),
                next_anchor,
                geometry,
                false,
            );
        }

        if let Some(card) = current_card {
            card.collapse(None);
        }

        let expansion = next.as_deref().and_then(|server_id| {
            let card = self.cards.borrow().get(server_id).cloned()?;
            let target_height = geometry.map(|geometry| geometry.height)?;
            Some((card, target_height))
        });
        if let Some((card, target_height)) = expansion {
            let view = self.clone();
            card.expand(
                target_height,
                Some(Box::new(move || {
                    view.commit_selection_layout(generation);
                })),
            );
        } else if next.is_some() {
            self.commit_selection_layout(generation);
        }
    }

    /// Steady-state pass after the expand animation: re-plan from the *current*
    /// selection state and place the reserver. Positions were already applied
    /// when the selection changed, so this is visually a no-op, but re-reading
    /// state keeps it correct if columns or order changed mid-animation.
    fn commit_selection_layout(&self, generation: u64) {
        if self.selection_generation.get() != generation {
            return;
        }
        let selected = self.selected.borrow().clone();
        let anchor = *self.selected_anchor.borrow();
        let geometry = selected
            .as_deref()
            .and_then(|id| self.expanded_geometry_for(id));
        for group in self.groups.borrow().iter() {
            fill_grid(
                group,
                self.columns.get(),
                selected.as_deref(),
                anchor,
                geometry,
                true,
            );
        }
    }

    fn set_selected_immediately(&self, server_id: &str) {
        *self.selected_anchor.borrow_mut() = self.anchor_for(server_id);
        let geometry = self.expanded_geometry_for(server_id);
        let anchor = *self.selected_anchor.borrow();
        for group in self.groups.borrow().iter() {
            fill_grid(
                group,
                self.columns.get(),
                Some(server_id),
                anchor,
                geometry,
                true,
            );
        }
        let card = self.cards.borrow().get(server_id).cloned();
        let target_height = geometry.map(|geometry| geometry.height);
        if let (Some(card), Some(target_height)) = (card, target_height) {
            card.set_expanded_immediately(target_height);
        }
    }

    fn expanded_geometry_for(&self, server_id: &str) -> Option<ExpandedGeometry> {
        let groups = self.groups.borrow();
        let group = groups
            .iter()
            .find(|group| group.cards.iter().any(|(id, _)| id == server_id))?;
        let cards = self.cards.borrow();
        let card = cards.get(server_id)?;
        if self.columns.get() == 1 {
            let allocated_width =
                grid_column_width(&group.grid, 1).max(card.root.allocated_width());
            let width = if allocated_width > 0 {
                allocated_width
            } else {
                CARD_MEASURE_WIDTH
            };
            return Some(ExpandedGeometry {
                row_span: 1,
                height: card.expanded_natural_height(width).max(COMPACT_CARD_HEIGHT),
            });
        }
        let row_spacing = i32::try_from(group.grid.row_spacing()).unwrap_or(CARD_ROW_SPACING);
        let row_span = card.expanded_span(row_spacing);
        Some(ExpandedGeometry {
            row_span,
            height: exact_expanded_height(row_span, row_spacing),
        })
    }

    fn anchor_for(&self, server_id: &str) -> Option<GridAnchor> {
        let groups = self.groups.borrow();
        let group = groups
            .iter()
            .find(|group| group.cards.iter().any(|(id, _)| id == server_id))?;
        let card = group
            .cards
            .iter()
            .find(|(id, _)| id == server_id)
            .map(|(_, card)| card)?;
        let (column, row, _, _) = group.grid.query_child(card);
        Some(GridAnchor { column, row })
    }

    /// Set the grid column count (clamped 1..=3). No-op if unchanged so a
    /// stream of size-allocations doesn't reflow the cards on every pixel.
    pub fn set_columns(&self, count: usize) {
        let count = count.clamp(1, 3);
        if self.columns.get() == count {
            return;
        }
        self.columns.set(count);
        self.pending_columns.set(count);
        let selected = self.selected.borrow().clone();
        let geometry = selected
            .as_deref()
            .and_then(|id| self.expanded_geometry_for(id));
        for group in self.groups.borrow().iter() {
            fill_grid(group, count, selected.as_deref(), None, geometry, true);
        }
        *self.selected_anchor.borrow_mut() = self
            .selected
            .borrow()
            .as_deref()
            .and_then(|id| self.anchor_for(id));
        if let (Some(server_id), Some(geometry)) = (selected, geometry)
            && let Some(card) = self.cards.borrow().get(&server_id)
        {
            card.resize_expanded(geometry.height);
        }
    }

    fn refresh_single_column_geometry(&self) {
        if self.columns.get() != 1 {
            return;
        }
        let Some(server_id) = self.selected.borrow().clone() else {
            return;
        };
        let Some(geometry) = self.expanded_geometry_for(&server_id) else {
            return;
        };
        if let Some(card) = self.cards.borrow().get(&server_id) {
            card.resize_expanded(geometry.height);
        }
    }

    /// Manually capture and apply a latency order. Later measurements update only
    /// their badges until the user presses sort again.
    pub fn sort_group(&self, index: usize) {
        let Some(group) = self.groups.borrow().get(index).cloned() else {
            return;
        };
        let sorted = sorted_by_latency(&group.display_order.borrow(), &self.latencies.borrow());
        let generation = group.sort_generation.get().wrapping_add(1);
        group.sort_generation.set(generation);
        group.sort_button.set_sensitive(false);

        if !adw::is_animations_enabled(&group.grid) {
            *group.display_order.borrow_mut() = sorted;
            let selected = self.selected.borrow();
            let geometry = selected
                .as_deref()
                .and_then(|id| self.expanded_geometry_for(id));
            fill_grid(
                &group,
                self.columns.get(),
                selected.as_deref(),
                *self.selected_anchor.borrow(),
                geometry,
                true,
            );
            group.sort_button.set_sensitive(true);
            return;
        }

        let target = adw::CallbackAnimationTarget::new({
            let grid = group.grid.clone();
            move |value| grid.set_opacity(value)
        });
        let animation =
            adw::TimedAnimation::new(&group.grid, group.grid.opacity(), 0.0, 90, target);
        animation.set_easing(adw::Easing::EaseInCubic);
        animation.connect_done({
            let view = self.clone();
            let group = group.clone();
            move |_| {
                if group.sort_generation.get() != generation {
                    return;
                }
                *group.display_order.borrow_mut() = sorted.clone();
                let selected = view.selected.borrow();
                let selected_anchor = *view.selected_anchor.borrow();
                let geometry = selected
                    .as_deref()
                    .and_then(|id| view.expanded_geometry_for(id));
                fill_grid(
                    &group,
                    view.columns.get(),
                    selected.as_deref(),
                    selected_anchor,
                    geometry,
                    true,
                );
                drop(selected);

                let target = adw::CallbackAnimationTarget::new({
                    let grid = group.grid.clone();
                    move |value| grid.set_opacity(value)
                });
                let fade_in = adw::TimedAnimation::new(&group.grid, 0.0, 1.0, 130, target);
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

const MIN_EXPANDED_SPAN: i32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExpandedGeometry {
    row_span: i32,
    height: i32,
}

/// One child's position in the grid. Keeping the planner independent from GTK
/// makes the hole-skipping behavior deterministic and easy to test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GridCell {
    item: usize,
    column: i32,
    row: i32,
    row_span: i32,
}

fn exact_expanded_height(row_span: i32, row_spacing: i32) -> i32 {
    let row_span = row_span.clamp(2, 3);
    COMPACT_CARD_HEIGHT
        .saturating_mul(row_span)
        .saturating_add(row_spacing.max(0).saturating_mul(row_span - 1))
}

fn grid_column_width(grid: &gtk::Grid, columns: usize) -> i32 {
    let columns = columns.max(1) as i32;
    let spacing = i32::try_from(grid.column_spacing()).unwrap_or(i32::MAX);
    let gaps = spacing.saturating_mul(columns - 1);
    grid.allocated_width()
        .saturating_sub(gaps)
        .checked_div(columns)
        .unwrap_or(0)
}

fn plan_grid(
    item_count: usize,
    columns: usize,
    expanded_item: Option<usize>,
    expanded_anchor: Option<GridAnchor>,
    expanded_span: i32,
) -> Vec<GridCell> {
    let columns = columns.max(1) as i32;
    let expanded_span = expanded_span.max(1);
    let mut reserved: HashSet<(i32, i32)> = HashSet::new();
    let mut cell = 0i32;
    let mut placements = Vec::with_capacity(item_count);
    let anchored = expanded_item.zip(expanded_anchor).map(|(item, anchor)| {
        let anchor = GridAnchor {
            column: anchor.column.clamp(0, columns - 1),
            row: anchor.row.max(0),
        };
        reserved.insert((anchor.row, anchor.column));
        for extra in 1..expanded_span {
            reserved.insert((anchor.row + extra, anchor.column));
        }
        (item, anchor)
    });

    for item in 0..item_count {
        if let Some((anchored_item, anchor)) = anchored
            && item == anchored_item
        {
            placements.push(GridCell {
                item,
                column: anchor.column,
                row: anchor.row,
                row_span: expanded_span,
            });
            continue;
        }

        let (mut row, mut column) = (cell / columns, cell % columns);
        while reserved.contains(&(row, column)) {
            cell += 1;
            row = cell / columns;
            column = cell % columns;
        }
        let row_span = if Some(item) == expanded_item {
            expanded_span
        } else {
            1
        };
        placements.push(GridCell {
            item,
            column,
            row,
            row_span,
        });
        for extra in 1..row_span {
            reserved.insert((row + extra, column));
        }
        cell += 1;
    }

    placements
}

/// Lay cards out in an N-column grid in the group's explicit display order. The single
/// expanded card reserves every cell covered by its whole-row span.
/// Existing children move through GridLayoutChild properties and are never
/// detached, preserving card state and its animation.
fn fill_grid(
    group: &GroupUi,
    n: usize,
    expanded_id: Option<&str>,
    expanded_anchor: Option<GridAnchor>,
    geometry: Option<ExpandedGeometry>,
    with_reserver: bool,
) {
    // Only visible (unfiltered) cards take a cell, so a search never leaves holes.
    let display_order = group.display_order.borrow();
    let ordered: Vec<&(String, gtk::Widget)> = display_order
        .iter()
        .filter_map(|id| group.cards.iter().find(|(card_id, _)| card_id == id))
        .filter(|(_, card)| card.get_visible())
        .collect();
    let expanded_item =
        expanded_id.and_then(|id| ordered.iter().position(|(item_id, _)| item_id == id));
    let expanded_span = geometry.map_or(MIN_EXPANDED_SPAN, |geometry| geometry.row_span);
    let placements = plan_grid(
        ordered.len(),
        n,
        expanded_item,
        expanded_anchor,
        expanded_span,
    );
    for placement in &placements {
        let (_, card) = ordered[placement.item];
        set_grid_cell(
            &group.grid,
            card,
            placement.column,
            placement.row,
            placement.row_span,
        );
    }

    // While a selection transition is animating (`with_reserver == false`) the
    // expanded card's own animated measure holds its rows; the reserver is only
    // needed as a steady-state stabilizer once the animation has committed.
    let reserved = expanded_item
        .zip(geometry)
        .filter(|_| n > 1 && with_reserver)
        .and_then(|(item, geometry)| {
            placements
                .iter()
                .find(|placement| placement.item == item)
                .map(|placement| (*placement, geometry))
        });
    if let Some((placement, geometry)) = reserved {
        group.reserver.set_height_request(geometry.height);
        set_grid_cell(
            &group.grid,
            group.reserver.upcast_ref(),
            placement.column,
            placement.row,
            placement.row_span,
        );
        group.reserver.set_visible(true);
    } else {
        group.reserver.set_visible(false);
        group.reserver.set_height_request(-1);
    }
}

fn set_grid_cell(grid: &gtk::Grid, card: &gtk::Widget, column: i32, row: i32, row_span: i32) {
    if card.parent().is_none() {
        grid.attach(card, column, row, 1, row_span);
        return;
    }

    let Some(manager) = grid.layout_manager() else {
        return;
    };
    let Ok(layout_child) = manager
        .layout_child(card)
        .downcast::<gtk::GridLayoutChild>()
    else {
        return;
    };
    layout_child.set_column(column);
    layout_child.set_row(row);
    layout_child.set_column_span(1);
    layout_child.set_row_span(row_span);
}

/// Pick a masonry column count from the available content width so cards keep a
/// comfortable minimum size: 3 wide, 2 mid, 1 when cramped.
fn columns_for_width(width: i32) -> usize {
    let three_columns = MIN_CARD_WIDTH
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
    let three_columns = MIN_CARD_WIDTH
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

fn latency_state(
    id: &str,
    latencies: &HashMap<String, Option<u32>>,
    checking: &HashSet<String>,
) -> LatencyState {
    if checking.contains(id) {
        return LatencyState::Checking;
    }
    match latencies.get(id) {
        Some(Some(ms)) => LatencyState::Reachable(*ms),
        Some(None) => LatencyState::Unreachable,
        None => LatencyState::Unmeasured,
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
        GridAnchor, GridCell, columns_for_width, columns_for_width_with_hysteresis,
        exact_expanded_height, plan_grid, sorted_by_latency,
    };

    #[test]
    fn expanded_card_reserves_the_cell_below_it() {
        assert_eq!(
            plan_grid(7, 3, Some(2), None, 2),
            vec![
                GridCell {
                    item: 0,
                    column: 0,
                    row: 0,
                    row_span: 1
                },
                GridCell {
                    item: 1,
                    column: 1,
                    row: 0,
                    row_span: 1
                },
                GridCell {
                    item: 2,
                    column: 2,
                    row: 0,
                    row_span: 2
                },
                GridCell {
                    item: 3,
                    column: 0,
                    row: 1,
                    row_span: 1
                },
                GridCell {
                    item: 4,
                    column: 1,
                    row: 1,
                    row_span: 1
                },
                GridCell {
                    item: 5,
                    column: 0,
                    row: 2,
                    row_span: 1
                },
                GridCell {
                    item: 6,
                    column: 1,
                    row: 2,
                    row_span: 1
                },
            ]
        );
    }

    #[test]
    fn layouts_stay_dense_for_each_column_count() {
        assert_eq!(
            plan_grid(3, 1, None, None, 2).last().map(|cell| cell.row),
            Some(2)
        );
        assert_eq!(
            plan_grid(4, 2, None, None, 2).last().map(|cell| cell.row),
            Some(1)
        );
        assert_eq!(
            plan_grid(6, 3, None, None, 2).last().map(|cell| cell.row),
            Some(1)
        );
    }

    #[test]
    fn expanded_card_keeps_its_clicked_cell() {
        let cells = plan_grid(7, 3, Some(4), Some(GridAnchor { column: 2, row: 1 }), 2);
        assert_eq!(
            cells[4],
            GridCell {
                item: 4,
                column: 2,
                row: 1,
                row_span: 2,
            }
        );
        assert!(
            cells
                .iter()
                .enumerate()
                .all(|(index, cell)| index == 4 || (cell.column, cell.row) != (2, 1))
        );
    }

    #[test]
    fn expanded_height_uses_exact_64_pixel_rows_and_12_pixel_gaps() {
        assert_eq!(exact_expanded_height(2, 12), 140);
        assert_eq!(exact_expanded_height(3, 12), 216);
    }

    #[test]
    fn single_column_grid_does_not_reserve_artificial_rows() {
        assert_eq!(plan_grid(3, 1, Some(1), None, 1)[1].row_span, 1);
    }

    #[test]
    fn taller_expanded_card_reserves_each_covered_cell() {
        let cells = plan_grid(7, 3, Some(2), None, 3);
        assert_eq!(cells[2].row_span, 3);
        assert_eq!((cells[3].column, cells[3].row), (0, 1));
        assert_eq!((cells[4].column, cells[4].row), (1, 1));
        assert_eq!((cells[5].column, cells[5].row), (0, 2));
        assert_eq!((cells[6].column, cells[6].row), (1, 2));
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
        assert_eq!(columns_for_width(651), 1);
        assert_eq!(columns_for_width(652), 2);
        assert_eq!(columns_for_width(983), 2);
        assert_eq!(columns_for_width(984), 3);
    }

    #[test]
    fn column_hysteresis_prevents_threshold_flapping() {
        assert_eq!(columns_for_width_with_hysteresis(667, 1, 16), 1);
        assert_eq!(columns_for_width_with_hysteresis(668, 1, 16), 2);
        assert_eq!(columns_for_width_with_hysteresis(651, 2, 16), 2);
        assert_eq!(columns_for_width_with_hysteresis(635, 2, 16), 1);
        assert_eq!(columns_for_width_with_hysteresis(999, 2, 16), 2);
        assert_eq!(columns_for_width_with_hysteresis(1000, 2, 16), 3);
        assert_eq!(columns_for_width_with_hysteresis(969, 3, 16), 3);
        assert_eq!(columns_for_width_with_hysteresis(967, 3, 16), 2);
    }

    #[test]
    fn filtering_and_rebuild_keep_source_order_dense() {
        let filtered = plan_grid(4, 2, Some(1), Some(GridAnchor { column: 1, row: 0 }), 2);
        assert_eq!(
            filtered
                .iter()
                .map(|cell| (cell.item, cell.column, cell.row))
                .collect::<Vec<_>>(),
            vec![(0, 0, 0), (1, 1, 0), (2, 0, 1), (3, 0, 2)]
        );
        assert_eq!(
            plan_grid(4, 2, None, None, 1),
            plan_grid(4, 2, None, None, 1)
        );
    }
}
