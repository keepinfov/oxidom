use adw::prelude::*;

use crate::APP_ID;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    General,
    Subscriptions,
    Settings,
    Logs,
}

impl Page {
    pub fn stack_name(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Subscriptions => "subscriptions",
            Self::Settings => "settings",
            Self::Logs => "logs",
        }
    }

    /// Row position in the navigation list; inverse of the mapping in
    /// `connect_row_selected`.
    pub fn index(self) -> i32 {
        match self {
            Self::General => 0,
            Self::Subscriptions => 1,
            Self::Settings => 2,
            Self::Logs => 3,
        }
    }
}

pub struct Sidebar {
    pub root: gtk::Box,
    /// Exposed so the window can move the selection programmatically, e.g.
    /// from an error toast offering "Open Settings".
    pub list: gtk::ListBox,
    pub status_button: gtk::Button,
    pub status_icon: gtk::Image,
    pub status_label: gtk::Label,
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
            (Page::General, "network-server-symbolic", "General"),
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
            let page = match row.index() {
                1 => Page::Subscriptions,
                2 => Page::Settings,
                3 => Page::Logs,
                _ => Page::General,
            };
            on_page(page);
        });

        let status_icon = gtk::Image::builder()
            .icon_name("network-vpn-symbolic")
            .pixel_size(18)
            .width_request(18)
            .height_request(18)
            .css_classes(["sidebar-status-icon"])
            .build();
        let status_label = gtk::Label::builder()
            .label("Ready")
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let status_content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        status_content.append(&status_icon);
        status_content.append(&status_label);
        let status_button = gtk::Button::builder()
            .child(&status_content)
            .tooltip_text("Connection status")
            .css_classes(["flat", "sidebar-status"])
            .focus_on_click(true)
            .build();
        status_button.update_property(&[gtk::accessible::Property::Label("Connection status")]);
        status_button.set_margin_top(12);
        status_button.set_margin_bottom(14);
        status_button.set_margin_start(14);
        status_button.set_margin_end(14);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_width_request(230);
        root.add_css_class("sidebar");
        root.append(&brand);
        root.append(&list);
        list.set_vexpand(true);
        root.append(&status_button);

        Self {
            root,
            list,
            status_button,
            status_icon,
            status_label,
        }
    }
}
