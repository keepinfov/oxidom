use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use oxidom_core::logbook::{CAPACITY, LogRecord, LogSource, Severity};

use super::icon_button;
use crate::gui::logfeed::FeedBatch;

type ClearCallback = Rc<dyn Fn()>;

/// Lines kept in the widget before the oldest are dropped.
///
/// Lower than the book's own capacity: the records are still held in full and a
/// filter change redraws from them, so this only bounds what one `GtkTextBuffer`
/// is asked to lay out.
const VIEW_LINES: i32 = 2000;

/// Lines dropped in one go once [`VIEW_LINES`] is passed. Each deletion forces
/// the text view to revalidate its layout, so doing it a line at a time would
/// pay that cost on every arriving line.
const TRIM_CHUNK: i32 = 512;

/// What the user has narrowed the view to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    /// `None` is every source.
    pub source: Option<LogSource>,
    /// Keep records this severe or worse.
    pub severity: Severity,
    pub needle: String,
}

impl Default for Filter {
    fn default() -> Self {
        Filter {
            source: None,
            // Debug rather than Trace: the core at `loglevel = "debug"` is the
            // loudest thing the view ever shows, and trace is ours alone.
            severity: Severity::Debug,
            needle: String::new(),
        }
    }
}

impl Filter {
    pub fn matches(&self, record: &LogRecord) -> bool {
        if self.source.is_some_and(|source| source != record.source) {
            return false;
        }
        if !record.severity.at_least(self.severity) {
            return false;
        }
        if self.needle.is_empty() {
            return true;
        }
        let needle = self.needle.to_lowercase();
        record.text.to_lowercase().contains(&needle)
            || record.target.to_lowercase().contains(&needle)
    }
}

#[derive(Clone)]
pub struct LogsView {
    pub root: gtk::Box,
    text: gtk::TextView,
    buffer: gtk::TextBuffer,
    /// Right-gravity mark kept at the end of the buffer. Scrolling to a mark
    /// lets GtkTextView defer the scroll until it has laid the new text out;
    /// driving the adjustment directly means guessing at a height it has not
    /// computed yet.
    bottom: gtk::TextMark,
    scrolled: gtk::ScrolledWindow,
    stack: gtk::Stack,
    copy: gtk::Button,
    clear: gtk::Button,
    save: gtk::Button,
    follow: gtk::Button,
    /// Level and search, so they can move into `overflow` when the window is
    /// too narrow to hold them in the toolbar.
    filters: gtk::Box,
    filter_slot: gtk::Box,
    overflow: gtk::MenuButton,
    overflow_popover: gtk::Popover,
    compact: Rc<Cell<bool>>,
    following: Rc<Cell<bool>>,
    updating_scroll: Rc<Cell<bool>>,
    /// Every record received, filtered or not, so changing the filter redraws
    /// without asking the daemon again.
    records: Rc<RefCell<Vec<LogRecord>>>,
    filter: Rc<RefCell<Filter>>,
    clear_callbacks: Rc<RefCell<Vec<ClearCallback>>>,
}

impl LogsView {
    pub fn new() -> Self {
        let buffer = gtk::TextBuffer::new(None);
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

        let empty = adw::StatusPage::builder()
            .icon_name("utilities-terminal-symbolic")
            .title("No log output yet")
            .description(
                "The Xray core, the network interface and oxidom itself all report here. \
                 Connect to a server to see what they say.",
            )
            .vexpand(true)
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&scrolled, Some("log"));
        stack.set_visible_child_name("empty");

        install_tags(&buffer);

        let sources = adw::ToggleGroup::new();
        for (name, label, tooltip) in [
            ("", "All", "Every source"),
            ("oxidom", "oxidom", "What oxidom itself decided and why"),
            ("xray", "Xray", "What the Xray core printed"),
            (
                "tun2socks",
                "Interface",
                "What the network interface helper printed",
            ),
        ] {
            sources.add(
                adw::Toggle::builder()
                    .name(name)
                    .label(label)
                    .tooltip(tooltip)
                    .build(),
            );
        }
        sources.set_active_name(Some(""));

        let levels =
            gtk::DropDown::from_strings(&["Errors", "Warnings", "Info", "Debug", "Everything"]);
        levels.set_selected(3);
        levels.set_tooltip_text(Some("Hide anything less serious than this"));

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search")
            .hexpand(true)
            .build();
        search.set_tooltip_text(Some("Show only lines containing this text"));

        let filters = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        filters.append(&levels);
        filters.append(&search);

        let filter_slot = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .hexpand(true)
            .build();
        filter_slot.append(&filters);

        let overflow_popover = gtk::Popover::builder().build();
        let overflow = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Level and search")
            .popover(&overflow_popover)
            .visible(false)
            .css_classes(["flat"])
            .build();

        let copy = icon_button("edit-copy-symbolic", "Copy the visible log");
        let clear = icon_button("edit-clear-all-symbolic", "Clear the log");
        let save = icon_button("document-save-symbolic", "Save the visible log to a file");
        let follow = icon_button("go-bottom-symbolic", "Follow live logs");
        copy.set_sensitive(false);
        clear.set_sensitive(false);
        save.set_sensitive(false);
        follow.set_visible(false);

        let toolbar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            // The 18px the text itself is inset by; a narrower gutter here made
            // the toolbar read as belonging to something other than the log.
            .margin_top(12)
            .margin_bottom(6)
            .margin_start(18)
            .margin_end(18)
            .build();
        toolbar.append(&sources);
        toolbar.append(&filter_slot);
        toolbar.append(&overflow);
        toolbar.append(&follow);
        toolbar.append(&save);
        toolbar.append(&copy);
        toolbar.append(&clear);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&toolbar);
        root.append(&stack);

        let bottom = buffer.create_mark(Some("bottom"), &buffer.end_iter(), false);

        let view = LogsView {
            root,
            text,
            buffer,
            bottom,
            scrolled,
            stack,
            copy,
            clear,
            save,
            follow,
            filters,
            filter_slot,
            overflow,
            overflow_popover,
            compact: Rc::new(Cell::new(false)),
            following: Rc::new(Cell::new(true)),
            updating_scroll: Rc::new(Cell::new(false)),
            records: Rc::new(RefCell::new(Vec::new())),
            filter: Rc::new(RefCell::new(Filter::default())),
            clear_callbacks: Rc::new(RefCell::new(Vec::new())),
        };
        view.connect_signals(&sources, &levels, &search);
        view.follow_colours();
        view
    }

    fn connect_signals(
        &self,
        sources: &adw::ToggleGroup,
        levels: &gtk::DropDown,
        search: &gtk::SearchEntry,
    ) {
        let adjustment = self.scrolled.vadjustment();
        adjustment.connect_value_changed({
            let view = self.clone();
            move |adjustment| {
                if view.updating_scroll.get() {
                    return;
                }
                let at_bottom = is_at_bottom(adjustment);
                view.following.set(at_bottom);
                view.follow.set_visible(!at_bottom);
            }
        });
        self.follow.connect_clicked({
            let view = self.clone();
            move |_| {
                view.following.set(true);
                view.scroll_to_end();
                view.follow.set_visible(false);
            }
        });
        sources.connect_active_name_notify({
            let view = self.clone();
            move |sources| {
                let name = sources.active_name().unwrap_or_default();
                view.filter.borrow_mut().source = match name.as_str() {
                    "oxidom" => Some(LogSource::Oxidom),
                    "xray" => Some(LogSource::Xray),
                    "tun2socks" => Some(LogSource::Tun2socks),
                    _ => None,
                };
                view.redraw();
            }
        });
        levels.connect_selected_notify({
            let view = self.clone();
            move |levels| {
                view.filter.borrow_mut().severity = match levels.selected() {
                    0 => Severity::Error,
                    1 => Severity::Warn,
                    2 => Severity::Info,
                    3 => Severity::Debug,
                    _ => Severity::Trace,
                };
                view.redraw();
            }
        });
        search.connect_search_changed({
            let view = self.clone();
            move |search| {
                view.filter.borrow_mut().needle = search.text().to_string();
                view.redraw();
            }
        });
        self.copy.connect_clicked({
            let view = self.clone();
            move |_| {
                let text = view.visible_text();
                if !text.is_empty() {
                    view.text.clipboard().set_text(&text);
                }
            }
        });
        self.save.connect_clicked({
            let view = self.clone();
            move |_| view.save_to_file()
        });
        self.clear.connect_clicked({
            let view = self.clone();
            move |_| {
                view.records.borrow_mut().clear();
                view.redraw();
                let callbacks = view.clear_callbacks.borrow().clone();
                for callback in callbacks {
                    callback();
                }
            }
        });
        adw::StyleManager::default().connect_dark_notify({
            let view = self.clone();
            move |_| view.follow_colours()
        });
    }

    /// Allows the controller to clear the daemon's book as well as the view.
    pub fn connect_clear_requested(&self, callback: impl Fn() + 'static) {
        self.clear_callbacks.borrow_mut().push(Rc::new(callback));
    }

    /// Take one round of merged log records.
    ///
    /// Appends, and only appends, unless the feed reports that what is on screen
    /// no longer describes the same log. That is the whole point of the cursor:
    /// the view used to be handed the entire buffer every half second and had to
    /// work out for itself whether it had changed — and once the buffer was full
    /// it always had, so it rebuilt, and the rebuild threw the reader back to the
    /// top of the log twice a second.
    pub fn append(&self, batch: &FeedBatch) {
        if batch.reset {
            self.records.borrow_mut().clear();
        }
        if !batch.reset && batch.records.is_empty() && batch.skipped == 0 {
            return;
        }

        {
            let mut records = self.records.borrow_mut();
            records.extend(batch.records.iter().cloned());
            let excess = records.len().saturating_sub(CAPACITY);
            records.drain(..excess);
        }

        if batch.reset {
            self.redraw();
            return;
        }

        let filter = self.filter.borrow().clone();
        let fresh: Vec<&LogRecord> = batch
            .records
            .iter()
            .filter(|record| filter.matches(record))
            .collect();
        if fresh.is_empty() && batch.skipped == 0 {
            self.refresh_controls();
            return;
        }

        self.updating_scroll.set(true);
        if batch.skipped > 0 {
            // Said where it happened rather than tallied somewhere out of the
            // way. A reader who cannot see the gap has been handed a log that
            // reads as continuous and is not.
            self.insert_notice(&gap_notice(batch.skipped));
        }
        for record in fresh {
            self.insert_record(record);
        }
        self.trim();
        if self.following.get() {
            self.scroll_to_mark();
        }
        self.release_scroll_guard();
        self.refresh_controls();
    }

    /// Rebuild the buffer from the records held.
    ///
    /// Only ever in response to something the user did — changing a filter, or
    /// clearing — where a jump is expected. Live output never comes through
    /// here.
    fn redraw(&self) {
        let was_following = self.following.get();
        self.updating_scroll.set(true);
        self.buffer.set_text("");
        let filter = self.filter.borrow().clone();
        for record in self
            .records
            .borrow()
            .iter()
            .filter(|record| filter.matches(record))
        {
            self.insert_record(record);
        }
        if was_following {
            self.scroll_to_mark();
        }
        self.release_scroll_guard();
        self.refresh_controls();
    }

    /// A line about the log itself rather than a line of it.
    fn insert_notice(&self, notice: &str) {
        let start_offset = self.buffer.end_iter().offset();
        self.buffer
            .insert(&mut self.buffer.end_iter(), &format!("{notice}\n"));
        let start = self.buffer.iter_at_offset(start_offset);
        let end = self.buffer.end_iter();
        self.buffer.apply_tag_by_name("meta", &start, &end);
    }

    fn insert_record(&self, record: &LogRecord) {
        let start_offset = self.buffer.end_iter().offset();
        self.buffer
            .insert(&mut self.buffer.end_iter(), &render_line(record));
        if let Some(tag) = tag_for(record.severity) {
            let start = self.buffer.iter_at_offset(start_offset);
            let end = self.buffer.end_iter();
            self.buffer.apply_tag_by_name(tag, &start, &end);
        }
    }

    /// Drop the oldest lines once the buffer has grown past what one text view
    /// should be asked to lay out.
    ///
    /// Two conditions, both load bearing. Only while following, because a reader
    /// who has scrolled up is looking at the old end and must not have it pulled
    /// out from under them. And never past the first line actually on screen,
    /// so that even a mistimed call cannot delete what is being read.
    fn trim(&self) {
        if !self.following.get() {
            return;
        }
        if self.buffer.line_count() <= VIEW_LINES + TRIM_CHUNK {
            return;
        }
        let visible = self.text.visible_rect();
        let (first_visible, _) = self.text.line_at_y(visible.y());
        let cut = TRIM_CHUNK.min(first_visible.line());
        if cut <= 0 {
            return;
        }
        let mut start = self.buffer.start_iter();
        let Some(mut end) = self.buffer.iter_at_line(cut) else {
            return;
        };
        self.buffer.delete(&mut start, &mut end);
    }

    fn scroll_to_mark(&self) {
        self.buffer.move_mark(&self.bottom, &self.buffer.end_iter());
        self.text.scroll_to_mark(&self.bottom, 0.0, true, 0.0, 1.0);
        self.follow.set_visible(false);
    }

    fn scroll_to_end(&self) {
        self.updating_scroll.set(true);
        self.scroll_to_mark();
        self.release_scroll_guard();
    }

    /// Clear the re-entrancy guard one main-loop iteration later, never inline.
    ///
    /// The text view revalidates its layout — and emits the `value_changed` this
    /// guard suppresses — from an idle at `GDK_PRIORITY_REDRAW + 5`, which runs
    /// ahead of the default idle priority used here. Clearing the guard
    /// synchronously would let that signal through and turn following off on
    /// every appended line.
    fn release_scroll_guard(&self) {
        let updating_scroll = self.updating_scroll.clone();
        glib::idle_add_local_once(move || updating_scroll.set(false));
    }

    fn refresh_controls(&self) {
        let any = self.buffer.start_iter() != self.buffer.end_iter();
        self.copy.set_sensitive(any);
        self.save.set_sensitive(any);
        self.clear.set_sensitive(!self.records.borrow().is_empty());
        self.stack
            .set_visible_child_name(if any { "log" } else { "empty" });
    }

    fn visible_text(&self) -> String {
        self.buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), false)
            .to_string()
    }

    /// Write what is on screen — filters included — to a file the user picks.
    ///
    /// `GtkFileChooserNative` rather than `GtkFileDialog`, which arrived in GTK
    /// 4.10 and would mean raising this workspace's feature floor; that in turn
    /// surfaces deprecations in startup code this change has no business
    /// touching.
    fn save_to_file(&self) {
        let window = self.root.root().and_downcast::<gtk::Window>();
        let chooser = gtk::FileChooserNative::new(
            Some("Save the log"),
            window.as_ref(),
            gtk::FileChooserAction::Save,
            Some("Save"),
            Some("Cancel"),
        );
        chooser.set_current_name("oxidom.log");
        let text = self.visible_text();
        // The chooser is native and asynchronous; it must outlive this call or
        // it is destroyed before it can be answered.
        let held = RefCell::new(Some(chooser.clone()));
        chooser.connect_response(move |chooser, response| {
            if response == gtk::ResponseType::Accept
                && let Some(path) = chooser.file().and_then(|file| file.path())
                && let Err(error) = std::fs::write(&path, text.as_bytes())
            {
                log::warn!("could not save the log to {}: {error}", path.display());
            }
            held.borrow_mut().take();
        });
        chooser.show();
    }

    /// Fold the level and search controls into a menu when the window is too
    /// narrow for them. The source switcher stays out: it is the one control
    /// this page exists for.
    pub fn set_ultra_compact(&self, enabled: bool) {
        if self.compact.get() == enabled {
            return;
        }
        self.compact.set(enabled);
        if enabled {
            self.filter_slot.remove(&self.filters);
            self.filters.set_margin_top(6);
            self.filters.set_margin_bottom(6);
            self.filters.set_margin_start(6);
            self.filters.set_margin_end(6);
            self.overflow_popover.set_child(Some(&self.filters));
        } else {
            self.overflow_popover.set_child(gtk::Widget::NONE);
            self.filter_slot.append(&self.filters);
        }
        self.overflow.set_visible(enabled);
        self.filter_slot.set_visible(!enabled);
    }

    /// Re-derive the tag colours for the current light/dark setting.
    ///
    /// `GtkTextTag` takes a colour, not a style class, so nothing recolours it
    /// when the scheme changes; without this the error red picked for a light
    /// window stays unreadable on a dark one.
    fn follow_colours(&self) {
        let dark = adw::StyleManager::default().is_dark();
        let table = self.buffer.tag_table();
        for (name, light, heavy) in [
            ("error", "#c01c28", "#ff938c"),
            ("warn", "#a1670a", "#f8c76b"),
            ("dim", "#5e5c64", "#9a9996"),
            ("meta", "#77767b", "#8e8e8e"),
        ] {
            if let Some(tag) = table.lookup(name) {
                tag.set_foreground(Some(if dark { heavy } else { light }));
            }
        }
    }
}

fn install_tags(buffer: &gtk::TextBuffer) {
    let table = buffer.tag_table();
    let error = gtk::TextTag::builder().name("error").weight(700).build();
    table.add(&error);
    for name in ["warn", "dim", "meta"] {
        table.add(&gtk::TextTag::builder().name(name).build());
    }
}

fn tag_for(severity: Severity) -> Option<&'static str> {
    match severity {
        Severity::Error => Some("error"),
        Severity::Warn => Some("warn"),
        Severity::Info => None,
        Severity::Debug | Severity::Trace => Some("dim"),
    }
}

/// How a gap in the log is announced where it happened.
fn gap_notice(skipped: u64) -> String {
    if skipped == 1 {
        "… 1 earlier line was dropped before it could be shown".to_string()
    } else {
        format!("… {skipped} earlier lines were dropped before they could be shown")
    }
}

/// One line as it is shown: when, how serious, who said it, and what was said.
fn render_line(record: &LogRecord) -> String {
    let origin = if record.target.is_empty() {
        record.source.label().to_string()
    } else {
        format!("{}/{}", record.source.label(), record.target)
    };
    format!(
        "{}  {:<5}  {}  {}\n",
        clock_time(record.unix_ms),
        record.severity.label(),
        origin,
        record.text
    )
}

/// Local wall-clock time of a record, or blanks when it has none — a line
/// reconstructed from a daemon too old to timestamp it.
fn clock_time(unix_ms: u64) -> String {
    if unix_ms == 0 {
        return "        ".to_string();
    }
    glib::DateTime::from_unix_local(unix_ms as i64 / 1000)
        .and_then(|time| time.format("%H:%M:%S"))
        .map(|formatted| formatted.to_string())
        .unwrap_or_else(|_| "        ".to_string())
}

fn is_at_bottom(adjustment: &gtk::Adjustment) -> bool {
    adjustment.value() + adjustment.page_size() >= adjustment.upper() - 2.0
}

#[cfg(test)]
mod tests {
    use super::{Filter, gap_notice, render_line};
    use oxidom_core::logbook::{LogRecord, LogSource, Severity};

    fn record(source: LogSource, severity: Severity, target: &str, text: &str) -> LogRecord {
        LogRecord {
            seq: 1,
            unix_ms: 0,
            source,
            severity,
            profile: None,
            target: target.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn a_source_filter_admits_only_that_source() {
        let filter = Filter {
            source: Some(LogSource::Oxidom),
            ..Filter::default()
        };
        assert!(filter.matches(&record(LogSource::Oxidom, Severity::Info, "", "ours")));
        assert!(!filter.matches(&record(LogSource::Xray, Severity::Info, "", "theirs")));
        assert!(!filter.matches(&record(
            LogSource::Tun2socks,
            Severity::Info,
            "",
            "interface"
        )));
    }

    #[test]
    fn a_level_filter_hides_the_chatter_and_keeps_the_failures() {
        let filter = Filter {
            severity: Severity::Warn,
            ..Filter::default()
        };
        assert!(filter.matches(&record(LogSource::Xray, Severity::Error, "", "gone")));
        assert!(filter.matches(&record(LogSource::Xray, Severity::Warn, "", "odd")));
        assert!(!filter.matches(&record(LogSource::Xray, Severity::Info, "", "fine")));
        assert!(!filter.matches(&record(LogSource::Xray, Severity::Debug, "", "noise")));
    }

    /// Searching by the subsystem is how a reader follows one part of the core,
    /// and it is not in the message text.
    #[test]
    fn a_search_matches_the_message_or_the_subsystem_ignoring_case() {
        let filter = |needle: &str| Filter {
            needle: needle.to_string(),
            ..Filter::default()
        };
        let line = record(
            LogSource::Xray,
            Severity::Info,
            "app/proxyman",
            "Rejected the handshake",
        );

        assert!(filter("handshake").matches(&line));
        assert!(filter("HANDSHAKE").matches(&line));
        assert!(filter("proxyman").matches(&line));
        assert!(!filter("balancer").matches(&line));
    }

    #[test]
    fn the_default_filter_shows_everything_the_core_can_say() {
        let filter = Filter::default();
        for severity in [
            Severity::Error,
            Severity::Warn,
            Severity::Info,
            Severity::Debug,
        ] {
            assert!(
                filter.matches(&record(LogSource::Xray, severity, "", "line")),
                "{severity:?} must be visible by default"
            );
        }
    }

    /// A dropped line is stated, not tallied out of sight: a log that reads as
    /// continuous when it is not is worse than one that admits a hole.
    #[test]
    fn a_dropped_line_is_announced_and_counted_in_plain_words() {
        assert_eq!(
            gap_notice(1),
            "… 1 earlier line was dropped before it could be shown"
        );
        assert_eq!(
            gap_notice(42),
            "… 42 earlier lines were dropped before they could be shown"
        );
    }

    #[test]
    fn a_line_says_who_spoke_and_names_the_subsystem_when_there_is_one() {
        let with = render_line(&record(
            LogSource::Xray,
            Severity::Error,
            "app/proxyman",
            "refused",
        ));
        assert!(with.contains("xray/app/proxyman"), "{with}");
        assert!(with.contains("ERROR"), "{with}");
        assert!(with.ends_with('\n'), "{with:?}");

        let without = render_line(&record(LogSource::Oxidom, Severity::Info, "", "starting"));
        assert!(without.contains("  oxidom  starting"), "{without}");
    }
}
