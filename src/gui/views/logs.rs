use adw::prelude::*;

#[derive(Clone)]
pub struct LogsView {
    pub root: gtk::ScrolledWindow,
    buffer: gtk::TextBuffer,
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
        let root = gtk::ScrolledWindow::builder()
            .child(&text)
            .vexpand(true)
            .build();
        Self { root, buffer }
    }

    pub fn set_logs(&self, logs: &[String]) {
        let value = if logs.is_empty() {
            "No Xray output yet.".to_string()
        } else {
            logs.join("\n")
        };
        if self
            .buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), false)
            != value
        {
            self.buffer.set_text(&value);
        }
    }
}
