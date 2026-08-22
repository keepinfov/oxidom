use adw::prelude::*;

use oxidom_core::APP_ID;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Servers,
    Profiles,
    Subscriptions,
    Settings,
    Logs,
}

impl Page {
    pub fn stack_name(self) -> &'static str {
        match self {
            Self::Servers => "servers",
            Self::Profiles => "profiles",
            Self::Subscriptions => "subscriptions",
            Self::Settings => "settings",
            Self::Logs => "logs",
        }
    }

    /// Row position in the navigation list.
    pub fn index(self) -> i32 {
        match self {
            Self::Servers => 0,
            Self::Profiles => 1,
            Self::Subscriptions => 2,
            Self::Settings => 3,
            Self::Logs => 4,
        }
    }

    /// The page a row position names; the inverse of [`Page::index`].
    ///
    /// This used to be written out a second time inside `connect_row_selected`,
    /// where nothing compared the two tables. Anything reading the selection
    /// back — which is how the window answers "which page is showing" — went
    /// through that copy.
    pub fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Profiles,
            2 => Self::Subscriptions,
            3 => Self::Settings,
            4 => Self::Logs,
            _ => Self::Servers,
        }
    }
}

/// The status strip at the foot of the sidebar.
///
/// It is two targets, not one. `status_button` carries the text and always does
/// the same thing — open the page that owns connections. `status_action` is the
/// one thing there is to *do* about the current state, and it only exists when
/// there is one.
///
/// It used to be a single button whose meaning changed with the state:
/// connected meant disconnect, failed meant show the error, anything else meant
/// nothing. Worse, background work rewrote its label without touching what the
/// click did, so a strip reading "Checking latency…" disconnected the VPN.
pub struct Sidebar {
    pub root: gtk::Box,
    /// Exposed so the window can move the selection programmatically, e.g.
    /// from an error toast offering "Open Settings".
    pub list: gtk::ListBox,
    pub status_button: gtk::Button,
    pub status_icon: gtk::Image,
    pub status_label: gtk::Label,
    /// Spins beside the text while something runs. Deliberately not a second
    /// label: the strip answers "am I connected", and the work that is running
    /// is shown on the page doing it.
    pub status_spinner: gtk::Spinner,
    pub status_action: gtk::Button,
    pub status_action_icon: gtk::Image,
}

impl Sidebar {
    pub fn new(on_page: impl Fn(Page) + Clone + 'static) -> Self {
        let title_icon_name = format!("{APP_ID}-symbolic");
        let title_icon = gtk::Image::builder()
            .icon_name(&title_icon_name)
            .pixel_size(28)
            .build();
        let title = gtk::Label::builder()
            .label("oxidom")
            .css_classes(["title-2"])
            .build();
        let brand = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        brand.set_margin_top(18);
        brand.set_margin_bottom(14);
        brand.set_margin_start(18);
        brand.set_margin_end(18);
        brand.append(&title_icon);
        brand.append(&title);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build();
        list.set_margin_start(8);
        list.set_margin_end(8);
        let pages = [
            (Page::Servers, "network-server-symbolic", "Servers"),
            (Page::Profiles, "network-vpn-symbolic", "Profiles"),
            (
                Page::Subscriptions,
                "x-office-address-book-symbolic",
                "Subscriptions",
            ),
            (Page::Settings, "preferences-system-symbolic", "Settings"),
            (Page::Logs, "utilities-terminal-symbolic", "Logs"),
        ];
        for (_, icon, title) in pages {
            let row = adw::ActionRow::builder()
                .title(title)
                .activatable(true)
                .css_classes(["nav-row"])
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            list.append(&row);
        }
        list.select_row(list.row_at_index(0).as_ref());
        list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            on_page(Page::from_index(row.index()));
        });

        let status_icon = gtk::Image::builder()
            .icon_name("network-vpn-symbolic")
            .pixel_size(18)
            .width_request(18)
            .height_request(18)
            .css_classes(["sidebar-status-icon"])
            .build();
        let status_label = gtk::Label::builder()
            .label("Disconnected")
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let status_spinner = gtk::Spinner::new();
        status_spinner.set_visible(false);
        let status_content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        status_content.append(&status_icon);
        status_label.set_hexpand(true);
        status_content.append(&status_label);
        status_content.append(&status_spinner);
        let status_button = gtk::Button::builder()
            .child(&status_content)
            .hexpand(true)
            .tooltip_text("Show connections")
            .css_classes(["flat", "sidebar-status"])
            .focus_on_click(true)
            .build();
        status_button.update_property(&[gtk::accessible::Property::Label("Show connections")]);

        let status_action_icon = gtk::Image::builder().pixel_size(16).build();
        let status_action = gtk::Button::builder()
            .child(&status_action_icon)
            .css_classes(["flat", "circular", "sidebar-status-action"])
            .visible(false)
            .build();

        let status_strip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        status_strip.append(&status_button);
        status_strip.append(&status_action);
        status_strip.set_margin_top(12);
        status_strip.set_margin_bottom(14);
        status_strip.set_margin_start(14);
        status_strip.set_margin_end(14);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_width_request(230);
        root.add_css_class("sidebar");
        root.append(&brand);
        root.append(&list);
        list.set_vexpand(true);
        root.append(&status_strip);

        Self {
            root,
            list,
            status_button,
            status_icon,
            status_label,
            status_spinner,
            status_action,
            status_action_icon,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two tables used to be written out separately, in two files, with
    /// nothing comparing them. A page that navigated to row 3 and read row 3
    /// back as a different page would have been a silent disagreement.
    #[test]
    fn a_row_position_and_the_page_it_names_are_inverses() {
        for page in [
            Page::Servers,
            Page::Profiles,
            Page::Subscriptions,
            Page::Settings,
            Page::Logs,
        ] {
            assert_eq!(
                Page::from_index(page.index()),
                page,
                "{page:?} did not survive the round trip through its row index"
            );
        }
    }

    /// A list with no selection reports -1, and the sidebar opens on Servers.
    #[test]
    fn a_row_position_that_names_no_page_reads_as_the_first_one() {
        assert_eq!(Page::from_index(-1), Page::Servers);
        assert_eq!(Page::from_index(9), Page::Servers);
    }
}
