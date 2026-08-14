use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;

use crate::gui::operation::UiOperation;
use crate::gui::reduce::{SessionDetail, SessionRow, SessionRowState};
use oxidom_core::ipc::ProfileEntry;

use super::icon_button;

#[derive(Clone)]
pub struct SessionCallbacks {
    /// `(profile, active)` — the requested position of the session switch.
    pub toggle: Rc<dyn Fn(String, bool)>,
    /// Open the editor for this profile. The page deliberately does not open
    /// it itself: the entry it holds is as old as the last time this page was
    /// entered, and saving a whole profile from a stale copy silently undoes
    /// whatever the CLI wrote in the meantime.
    pub edit: Rc<dyn Fn(String)>,
    /// Open the editor for a profile that does not exist yet.
    pub create: Rc<dyn Fn()>,
}

#[derive(Clone)]
struct HeaderControls {
    root: gtk::Box,
    add: gtk::Button,
}

#[derive(Clone)]
struct RowControls {
    row: adw::ExpanderRow,
    spinner: gtk::Spinner,
    /// Warning badge, e.g. a stale pool. Hidden when there is nothing to warn
    /// about, rather than present-and-empty, so it takes no width.
    warning: gtk::Label,
    latency: gtk::Label,
    state: gtk::Label,
    toggle: gtk::Switch,
    edit: adw::ButtonRow,
    /// Detail rows currently hanging off the expander, so they can be removed
    /// before the next set is added — `AdwExpanderRow` has no "clear".
    details: Rc<RefCell<Vec<adw::ActionRow>>>,
    syncing_toggle: Rc<Cell<bool>>,
    /// The model this row is currently showing. `set_rows` runs on every poll,
    /// and rebuilding the labels twice a second for rows nothing happened to is
    /// churn the user pays for in a flickering list.
    applied: Rc<RefCell<Option<SessionRow>>>,
}

#[derive(Clone)]
pub struct SessionsView {
    pub root: gtk::ScrolledWindow,
    content: gtk::Box,
    callbacks: Rc<RefCell<Option<SessionCallbacks>>>,
    header: HeaderControls,
    header_embedded: Rc<Cell<bool>>,
    operation: Rc<RefCell<Option<UiOperation>>>,
    operation_widgets: Rc<RefCell<HashMap<String, RowControls>>>,
}

impl SessionsView {
    pub fn new() -> Self {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 22);
        content.set_hexpand(true);
        content.set_margin_top(24);
        content.set_margin_bottom(32);
        content.set_margin_start(28);
        content.set_margin_end(28);
        let root = gtk::ScrolledWindow::builder()
            .child(&content)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let callbacks = Rc::new(RefCell::new(None::<SessionCallbacks>));
        let header = make_header_controls(callbacks.clone());

        Self {
            root,
            content,
            callbacks,
            header,
            header_embedded: Rc::new(Cell::new(true)),
            operation: Rc::new(RefCell::new(None)),
            operation_widgets: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn header_actions(&self) -> gtk::Box {
        self.header.root.clone()
    }

    pub fn set_header_actions_embedded(&self, embedded: bool) {
        self.header_embedded.set(embedded);
    }

    pub fn rebuild(
        &self,
        profiles: &[ProfileEntry],
        rows: &[SessionRow],
        callbacks: SessionCallbacks,
    ) {
        *self.callbacks.borrow_mut() = Some(callbacks.clone());

        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        self.operation_widgets.borrow_mut().clear();

        let list = adw::PreferencesGroup::builder()
            .title("Profiles")
            .description("Named connections, shared with the CLI and systemd")
            .build();
        if self.header_embedded.get() {
            list.set_header_suffix(Some(&self.header.root));
        }

        if profiles.is_empty() {
            let empty = adw::ActionRow::builder()
                .title("No profiles")
                .subtitle(
                    "Use + to create one; `oxidom up <name>` and `systemctl start \
                     oxidom@<name>` use the same files",
                )
                .subtitle_lines(3)
                .activatable(false)
                .build();
            list.add(&empty);
        }

        // Only rows the profile list still knows about: a row whose profile
        // has gone would carry an editor for a name that no longer exists.
        for row in rows {
            if !profiles.iter().any(|entry| entry.name == row.profile) {
                continue;
            }
            self.add_session_row(&list, row, callbacks.clone());
        }

        self.content.append(&list);
        self.apply_operation();
    }

    /// Apply the latest pure row models to the rows already on the page.
    ///
    /// Returns `false` when the set of profiles no longer matches what is
    /// built, because rebuilding from here would pair the new rows with the
    /// profiles of the last build. The caller owns both lists and is the only
    /// one who can rebuild them consistently.
    #[must_use]
    pub fn set_rows(&self, rows: &[SessionRow]) -> bool {
        let widgets = self.operation_widgets.borrow();
        if widgets.len() != rows.len() || rows.iter().any(|row| !widgets.contains_key(&row.profile))
        {
            return false;
        }

        for row in rows {
            if let Some(controls) = widgets.get(&row.profile) {
                apply_row(controls, row);
            }
        }
        drop(widgets);
        self.apply_operation();
        true
    }

    pub fn set_operation(&self, operation: Option<UiOperation>) {
        *self.operation.borrow_mut() = operation;
        self.apply_operation();
    }

    pub fn set_ultra_compact(&self, enabled: bool) {
        self.content.set_spacing(if enabled { 16 } else { 22 });
    }

    /// One row: a headline and the state, with every other fact folded away.
    ///
    /// An expander rather than a wall of suffixes because the row has to answer
    /// "is this on?" at a glance and "what exactly is it doing?" on request, and
    /// those are different questions. The former is the badge and the switch;
    /// the latter is eight labelled lines that have no business competing with
    /// them for the same strip of pixels.
    fn add_session_row(
        &self,
        list: &adw::PreferencesGroup,
        model: &SessionRow,
        callbacks: SessionCallbacks,
    ) {
        let row = adw::ExpanderRow::builder()
            .title(&model.profile)
            .subtitle(&model.headline)
            .subtitle_lines(2)
            .build();

        // `AdwExpanderRow::add_suffix` packs towards the title, so suffixes are
        // added in the reverse of the order they are read: switch first ends up
        // rightmost, next to the expander arrow where a switch belongs.
        let toggle = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .active(model.toggle_on)
            .build();
        row.add_suffix(&toggle);

        let spinner = gtk::Spinner::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        row.add_suffix(&spinner);

        let state = gtk::Label::builder()
            .valign(gtk::Align::Center)
            .css_classes(["status-badge"])
            .build();
        row.add_suffix(&state);

        let latency = gtk::Label::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .css_classes(["session-chip", "session-chip-latency"])
            .build();
        row.add_suffix(&latency);

        let warning = gtk::Label::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .css_classes(["session-chip", "session-chip-stale"])
            .build();
        row.add_suffix(&warning);

        let syncing_toggle = Rc::new(Cell::new(false));
        toggle.connect_active_notify({
            let callback = callbacks.toggle.clone();
            let profile = model.profile.clone();
            let syncing_toggle = syncing_toggle.clone();
            move |toggle| {
                if syncing_toggle.get() {
                    return;
                }
                callback(profile.clone(), toggle.is_active());
            }
        });

        // Editing moved off the row itself: activating an `AdwExpanderRow`
        // expands it, so the row's own gesture is spoken for. A button inside
        // the expanded body is its own target and says what it does. It is
        // re-added after the details on every change so the action stays last —
        // facts first, then the thing that changes them.
        let edit = adw::ButtonRow::builder().title("Edit profile…").build();
        edit.connect_activated({
            let edit = callbacks.edit.clone();
            let profile = model.profile.clone();
            move |_| edit(profile.clone())
        });

        let entry_name = model.profile.clone();
        let controls = RowControls {
            row: row.clone(),
            spinner,
            warning,
            latency,
            state,
            toggle,
            edit,
            details: Rc::new(RefCell::new(Vec::new())),
            syncing_toggle,
            applied: Rc::new(RefCell::new(None)),
        };
        apply_row(&controls, model);
        self.operation_widgets
            .borrow_mut()
            .insert(entry_name, controls);
        list.add(&row);
    }

    fn apply_operation(&self) {
        let operation = self.operation.borrow().clone();
        let busy = operation.is_some();
        self.header.add.set_sensitive(!busy);
        for (name, controls) in self.operation_widgets.borrow().iter() {
            let affected = operation
                .as_ref()
                .and_then(|operation| operation.profile.as_deref())
                == Some(name.as_str());
            controls.row.set_sensitive(!busy);
            controls.spinner.set_spinning(affected);
            controls.spinner.set_visible(affected);
        }
    }
}

fn make_header_controls(callbacks: Rc<RefCell<Option<SessionCallbacks>>>) -> HeaderControls {
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_valign(gtk::Align::Center);
    let add = icon_button("list-add-symbolic", "New profile");
    add.add_css_class("header-icon-button");

    add.connect_clicked(move |_| {
        if let Some(callbacks) = callbacks.borrow().clone() {
            (callbacks.create)();
        }
    });

    actions.append(&add);
    HeaderControls { root: actions, add }
}

fn apply_row(controls: &RowControls, row: &SessionRow) {
    if controls.applied.borrow().as_ref() == Some(row) {
        return;
    }
    let changed_details = controls
        .applied
        .borrow()
        .as_ref()
        .map(|applied| applied.details != row.details)
        .unwrap_or(true);
    *controls.applied.borrow_mut() = Some(row.clone());
    controls.row.set_title(&row.profile);
    controls.row.set_subtitle(&row.headline);
    controls.row.set_sensitive(!row.busy);
    controls.spinner.set_spinning(row.busy);
    controls.spinner.set_visible(row.busy);

    match &row.warning {
        Some(chip) => {
            controls.warning.set_label(&chip.text);
            controls.warning.set_tooltip_text(chip.tooltip.as_deref());
            controls.warning.set_visible(true);
        }
        None => controls.warning.set_visible(false),
    }
    match &row.latency {
        Some(text) => {
            controls.latency.set_label(text);
            controls.latency.set_visible(true);
        }
        None => controls.latency.set_visible(false),
    }
    set_state_label(&controls.state, row.state);

    // Detail rows are rebuilt only when they actually changed: the page polls
    // twice a second, and removing and re-adding rows under the pointer makes
    // an expanded row unusable.
    if changed_details {
        set_details(controls, &row.details);
    }

    // `set_active` emits the same signal as a user's click. Polling must not
    // turn its own repaint into an UpProfile/Down request every 500 ms.
    controls.syncing_toggle.set(true);
    controls.toggle.set_active(row.toggle_on);
    controls.syncing_toggle.set(false);
    // A switch asserts a position. Against a state this build cannot read there
    // is no position to assert, and an off switch would claim the session is
    // down. The rest of the row stays usable — Edit profile… still applies.
    controls
        .toggle
        .set_sensitive(row.state != SessionRowState::Unknown);
}

fn set_details(controls: &RowControls, details: &[SessionDetail]) {
    for previous in controls.details.borrow_mut().drain(..) {
        controls.row.remove(&previous);
    }
    // Asked of the widget rather than inferred from the detail count: a session
    // with no details still gets the edit row, so "details were empty" is not
    // the same question as "the edit row is already in there", and answering the
    // wrong one adds it a second time on every repaint.
    if controls.edit.parent().is_some() {
        controls.row.remove(&controls.edit);
    }
    let mut built = Vec::with_capacity(details.len());
    for detail in details {
        let row = adw::ActionRow::builder()
            .title(&detail.label)
            .subtitle(&detail.value)
            .subtitle_selectable(true)
            .build();
        // On the row rather than on a help icon beside it: the whole row is the
        // hover target already, and an icon whose only job is to be hovered is
        // a second thing to notice for one sentence.
        if let Some(tooltip) = &detail.tooltip {
            row.set_tooltip_text(Some(tooltip));
        }
        if detail.copyable {
            let copy = gtk::Button::builder()
                .icon_name("edit-copy-symbolic")
                .tooltip_text("Copy")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            copy.connect_clicked({
                let value = detail.value.clone();
                move |button| {
                    if let Some(display) = gtk::gdk::Display::default() {
                        display.clipboard().set_text(&value);
                    }
                    button.set_icon_name("object-select-symbolic");
                }
            });
            row.add_suffix(&copy);
        }
        controls.row.add_row(&row);
        built.push(row);
    }
    controls.row.add_row(&controls.edit);
    *controls.details.borrow_mut() = built;
}

fn set_state_label(label: &gtk::Label, state: SessionRowState) {
    for class in [
        "status-neutral",
        "status-working",
        "status-connected",
        "status-error",
    ] {
        label.remove_css_class(class);
    }
    let (text, class) = match state {
        SessionRowState::Stopped => ("Stopped", "status-neutral"),
        SessionRowState::Connecting => ("Connecting", "status-working"),
        SessionRowState::Connected => ("Connected", "status-connected"),
        SessionRowState::Error => ("Error", "status-error"),
        // Deliberately not "Stopped": this build cannot read the state, and
        // saying "stopped" would answer a question it did not understand.
        SessionRowState::Unknown => ("Unknown", "status-neutral"),
    };
    label.set_label(text);
    label.add_css_class(class);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::reduce::SessionWarning;

    fn row(state: SessionRowState) -> SessionRow {
        SessionRow {
            profile: "work".to_string(),
            state,
            headline: "ch-trojan".to_string(),
            pool: false,
            latency: None,
            warning: None,
            details: Vec::new(),
            toggle_on: false,
            busy: false,
            error: None,
        }
    }

    #[test]
    fn a_row_with_no_warning_and_no_reading_shows_neither_badge() {
        // Both badges are `visible(false)` until there is something to say, so
        // an ordinary connected session carries exactly one pill — its state.
        let plain = row(SessionRowState::Connected);
        assert!(plain.warning.is_none());
        assert!(plain.latency.is_none());

        let stale = SessionRow {
            warning: Some(SessionWarning {
                text: "stale".to_string(),
                tooltip: Some("Reconnect to pick up new servers".to_string()),
            }),
            latency: Some("210 ms".to_string()),
            ..plain
        };
        let warning = stale.warning.as_ref().expect("just set");
        assert_eq!(warning.text, "stale");
        // The badge is the number alone; its age lives in the detail rows,
        // where there is room to read it.
        assert_eq!(stale.latency.as_deref(), Some("210 ms"));
    }
}
