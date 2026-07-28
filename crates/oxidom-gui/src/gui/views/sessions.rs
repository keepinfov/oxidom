use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;

use crate::gui::operation::UiOperation;
use crate::gui::reduce::{SessionChip, SessionChipKind, SessionRow, SessionRowState};
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
    row: adw::ActionRow,
    spinner: gtk::Spinner,
    state: gtk::Label,
    chips: gtk::FlowBox,
    toggle: gtk::Switch,
    syncing_toggle: Rc<Cell<bool>>,
    /// The model this row is currently showing. `set_rows` runs on every poll,
    /// and rebuilding the chip labels twice a second for rows nothing happened
    /// to is churn the user pays for in a flickering list.
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
            .title("Sessions")
            .description("Run and edit named connection profiles shared with the CLI and systemd")
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

    fn add_session_row(
        &self,
        list: &adw::PreferencesGroup,
        model: &SessionRow,
        callbacks: SessionCallbacks,
    ) {
        let row = adw::ActionRow::builder()
            .title(&model.profile)
            .subtitle(session_subtitle(model))
            .subtitle_lines(2)
            .activatable(true)
            .build();

        let chips = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .column_spacing(6)
            .row_spacing(4)
            .min_children_per_line(1)
            .max_children_per_line(3)
            .build();
        chips.set_valign(gtk::Align::Center);
        row.add_suffix(&chips);

        let state = gtk::Label::builder()
            .valign(gtk::Align::Center)
            .css_classes(["status-badge"])
            .build();
        row.add_suffix(&state);

        let spinner = gtk::Spinner::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        row.add_suffix(&spinner);

        let toggle = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .active(model.toggle_on)
            .build();
        row.add_suffix(&toggle);
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

        let entry_name = model.profile.clone();
        row.connect_activated({
            let edit = callbacks.edit.clone();
            let profile = model.profile.clone();
            move |_| edit(profile.clone())
        });

        let controls = RowControls {
            row: row.clone(),
            spinner,
            state,
            chips,
            toggle,
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
    *controls.applied.borrow_mut() = Some(row.clone());
    controls.row.set_title(&row.profile);
    controls.row.set_subtitle(&session_subtitle(row));
    controls.row.set_sensitive(!row.busy);
    controls.spinner.set_spinning(row.busy);
    controls.spinner.set_visible(row.busy);
    set_state_label(&controls.state, row.state);
    set_chips(&controls.chips, &row.chips);

    // `set_active` emits the same signal as a user's click. Polling must not
    // turn its own repaint into an UpProfile/Down request every 500 ms.
    controls.syncing_toggle.set(true);
    controls.toggle.set_active(row.toggle_on);
    controls.syncing_toggle.set(false);
}

fn session_subtitle(row: &SessionRow) -> String {
    if row.state == SessionRowState::Error {
        return row
            .error
            .clone()
            .unwrap_or_else(|| "The session failed".to_string());
    }
    match (row.server.is_empty(), row.description.is_empty()) {
        (false, false) => format!("{} · {}", row.server, row.description),
        (false, true) => row.server.clone(),
        (true, false) => row.description.clone(),
        (true, true) => String::new(),
    }
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
    };
    label.set_label(text);
    label.add_css_class(class);
}

fn set_chips(container: &gtk::FlowBox, chips: &[SessionChip]) {
    while let Some(child) = container.first_child() {
        let child = child
            .downcast::<gtk::FlowBoxChild>()
            .expect("GtkFlowBox owns FlowBoxChild wrappers");
        container.remove(&child);
    }
    for chip in chips {
        let label = gtk::Label::builder()
            .label(&chip.text)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["session-chip", chip_class(chip.kind)])
            .build();
        container.insert(&label, -1);
    }
    container.set_visible(!chips.is_empty());
}

fn chip_class(kind: SessionChipKind) -> &'static str {
    match kind {
        SessionChipKind::Interface => "session-chip-interface",
        SessionChipKind::Inbound => "session-chip-inbound",
        SessionChipKind::Latency => "session-chip-latency",
        SessionChipKind::SystemProxy => "session-chip-system-proxy",
        SessionChipKind::ProxyOnly => "session-chip-proxy-only",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: SessionRowState) -> SessionRow {
        SessionRow {
            profile: "work".to_string(),
            state,
            server: "ch-trojan".to_string(),
            description: "Office tunnel".to_string(),
            chips: Vec::new(),
            toggle_on: false,
            busy: false,
            error: None,
        }
    }

    #[test]
    fn a_failed_session_says_why_instead_of_which_server_it_was_pointed_at() {
        let mut failed = row(SessionRowState::Error);
        failed.error = Some("tun2socks exited".to_string());
        assert_eq!(session_subtitle(&failed), "tun2socks exited");

        // A daemon that reports the failure without a message still has to
        // leave the row saying something other than "everything is fine".
        failed.error = None;
        assert_eq!(session_subtitle(&failed), "The session failed");
    }

    #[test]
    fn a_running_session_reads_as_server_then_description() {
        let mut running = row(SessionRowState::Connected);
        assert_eq!(session_subtitle(&running), "ch-trojan · Office tunnel");
        running.description.clear();
        assert_eq!(session_subtitle(&running), "ch-trojan");
        running.server.clear();
        running.description = "Office tunnel".to_string();
        assert_eq!(session_subtitle(&running), "Office tunnel");
        running.description.clear();
        assert_eq!(session_subtitle(&running), "");
    }
}
