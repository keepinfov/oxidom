use adw::prelude::*;

pub mod core_editor;
pub mod logs;
pub mod profile_dialog;
pub mod server_dialog;
pub mod servers;
pub mod sessions;
pub mod settings;
pub mod subscriptions;

pub(crate) fn dialog_content(
    group: &impl IsA<gtk::Widget>,
    validation: &gtk::Label,
) -> gtk::ScrolledWindow {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.append(group);
    content.append(validation);
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build()
}

pub(crate) fn validation_label() -> gtk::Label {
    let label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .wrap(true)
        .visible(false)
        .build();
    label.add_css_class("error");
    label
}

pub(crate) fn set_validation(label: &gtk::Label, message: Option<&str>) {
    label.set_label(message.unwrap_or_default());
    label.set_visible(message.is_some());
}

pub(crate) fn set_transient_parent(window: &adw::Window, parent: &impl IsA<gtk::Widget>) {
    if let Some(parent_window) = parent.root().and_downcast::<gtk::Window>() {
        window.set_transient_for(Some(&parent_window));
    }
}

pub(crate) fn icon_button(icon_name: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(label)
        .focusable(true)
        .build();
    button.add_css_class("flat");
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}
