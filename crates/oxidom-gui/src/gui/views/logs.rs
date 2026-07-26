use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

type ClearCallback = Rc<dyn Fn()>;

#[derive(Clone)]
pub struct LogsView {
    pub root: gtk::Box,
    scrolled: gtk::ScrolledWindow,
    buffer: gtk::TextBuffer,
    copy: gtk::Button,
    clear: gtk::Button,
    follow: gtk::Button,
    following: Rc<Cell<bool>>,
    updating_scroll: Rc<Cell<bool>>,
    source_logs: Rc<RefCell<Vec<String>>>,
    cleared_prefix: Rc<RefCell<Option<Vec<String>>>>,
    visible_text: Rc<RefCell<String>>,
    clear_callbacks: Rc<RefCell<Vec<ClearCallback>>>,
}

impl LogsView {
    pub fn new() -> Self {
        let buffer = gtk::TextBuffer::new(None);
        buffer.set_text("No Xray output yet.");
        let text = gtk::TextView::builder()
            .buffer(&buffer)
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .left_margin(18)
            .right_margin(18)
            .top_margin(18)
            .bottom_margin(18)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        let scrolled = gtk::ScrolledWindow::builder()
            .child(&text)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let copy = icon_button("edit-copy-symbolic", "Copy all logs");
        let clear = icon_button("edit-clear-all-symbolic", "Clear logs");
        let follow = icon_button("go-bottom-symbolic", "Follow live logs");
        copy.set_sensitive(false);
        clear.set_sensitive(false);
        follow.set_visible(false);
        let toolbar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::End)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        toolbar.append(&follow);
        toolbar.append(&copy);
        toolbar.append(&clear);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&toolbar);
        root.append(&scrolled);

        let following = Rc::new(Cell::new(true));
        let updating_scroll = Rc::new(Cell::new(false));
        let source_logs = Rc::new(RefCell::new(Vec::new()));
        let cleared_prefix = Rc::new(RefCell::new(None));
        let visible_text = Rc::new(RefCell::new(String::new()));
        let clear_callbacks: Rc<RefCell<Vec<ClearCallback>>> = Rc::new(RefCell::new(Vec::new()));

        let adjustment = scrolled.vadjustment();
        adjustment.connect_value_changed({
            let following = following.clone();
            let updating_scroll = updating_scroll.clone();
            let follow = follow.clone();
            move |adjustment| {
                if updating_scroll.get() {
                    return;
                }
                let at_bottom = is_at_bottom(adjustment);
                following.set(at_bottom);
                follow.set_visible(!at_bottom);
            }
        });
        follow.connect_clicked({
            let adjustment = adjustment.clone();
            let following = following.clone();
            let updating_scroll = updating_scroll.clone();
            let follow = follow.clone();
            move |_| {
                following.set(true);
                updating_scroll.set(true);
                scroll_to_bottom(&adjustment);
                updating_scroll.set(false);
                follow.set_visible(false);
            }
        });
        copy.connect_clicked({
            let text = text.clone();
            let visible_text = visible_text.clone();
            move |_| {
                let value = visible_text.borrow();
                if !value.is_empty() {
                    text.clipboard().set_text(&value);
                }
            }
        });
        clear.connect_clicked({
            let source_logs = source_logs.clone();
            let cleared_prefix = cleared_prefix.clone();
            let visible_text = visible_text.clone();
            let buffer = buffer.clone();
            let copy = copy.clone();
            let clear = clear.clone();
            let clear_callbacks = clear_callbacks.clone();
            move |_| {
                *cleared_prefix.borrow_mut() = Some(source_logs.borrow().clone());
                visible_text.borrow_mut().clear();
                buffer.set_text("No Xray output yet.");
                copy.set_sensitive(false);
                clear.set_sensitive(false);
                let callbacks = clear_callbacks.borrow().clone();
                for callback in callbacks {
                    callback();
                }
            }
        });

        Self {
            root,
            scrolled,
            buffer,
            copy,
            clear,
            follow,
            following,
            updating_scroll,
            source_logs,
            cleared_prefix,
            visible_text,
            clear_callbacks,
        }
    }

    /// Allows the controller to clear the core ring buffer as well as the
    /// visible log. Local suppression still makes Clear useful without it.
    pub fn connect_clear_requested(&self, callback: impl Fn() + 'static) {
        self.clear_callbacks.borrow_mut().push(Rc::new(callback));
    }

    pub fn set_logs(&self, logs: &[String]) {
        *self.source_logs.borrow_mut() = logs.to_vec();
        let visible = {
            let mut cleared_prefix = self.cleared_prefix.borrow_mut();
            visible_after_clear(logs, &mut cleared_prefix)
        };
        let value = visible.join("\n");
        if *self.visible_text.borrow() == value {
            return;
        }
        *self.visible_text.borrow_mut() = value.clone();
        self.copy.set_sensitive(!value.is_empty());
        self.clear.set_sensitive(!value.is_empty());

        let rendered = if value.is_empty() {
            "No Xray output yet."
        } else {
            &value
        };
        let should_follow = self.following.get();
        self.updating_scroll.set(should_follow);
        self.buffer.set_text(rendered);
        if should_follow {
            let adjustment = self.scrolled.vadjustment();
            let updating_scroll = self.updating_scroll.clone();
            let follow = self.follow.clone();
            glib::idle_add_local_once(move || {
                scroll_to_bottom(&adjustment);
                updating_scroll.set(false);
                follow.set_visible(false);
            });
        }
    }
}

fn icon_button(icon_name: &str, accessible_label: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(accessible_label)
        .focusable(true)
        .css_classes(["flat"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label(accessible_label)]);
    button
}

fn is_at_bottom(adjustment: &gtk::Adjustment) -> bool {
    adjustment.value() + adjustment.page_size() >= adjustment.upper() - 2.0
}

fn scroll_to_bottom(adjustment: &gtk::Adjustment) {
    adjustment.set_value((adjustment.upper() - adjustment.page_size()).max(adjustment.lower()));
}

fn visible_after_clear<'a>(
    logs: &'a [String],
    cleared_prefix: &mut Option<Vec<String>>,
) -> &'a [String] {
    let Some(prefix) = cleared_prefix.as_ref() else {
        return logs;
    };
    if logs.starts_with(prefix) {
        &logs[prefix.len()..]
    } else {
        *cleared_prefix = None;
        logs
    }
}

#[cfg(test)]
mod tests {
    use super::visible_after_clear;

    #[test]
    fn clear_hides_old_lines_and_shows_new_tail() {
        let mut prefix = Some(vec!["old one".into(), "old two".into()]);
        let logs = vec!["old one".into(), "old two".into(), "new".into()];
        assert_eq!(visible_after_clear(&logs, &mut prefix), ["new"]);
    }

    #[test]
    fn rotated_ring_buffer_stops_suppressing() {
        let mut prefix = Some(vec!["old".into()]);
        let logs = vec!["new".into()];
        assert_eq!(visible_after_clear(&logs, &mut prefix), logs);
        assert!(prefix.is_none());
    }
}
