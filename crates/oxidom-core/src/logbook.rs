//! The one place a diagnostic line lands, whoever produced it.
//!
//! Three programs write here: the Xray core and `tun2socks` through their
//! pipes, and oxidom itself through the `log` facade. Keeping them in one
//! ordered book with a `source` on every record is what lets the Logs view
//! answer "was that the core or us?" — a question the old buffer could only
//! answer by the string prefix `"oxidom: "`, which nothing ever parsed back.
//!
//! Every record carries a [`LogRecord::seq`] that is monotonic for the life of
//! the process and never reused, even across eviction. That number is load
//! bearing: a reader asks for what comes *after* its cursor, so a refresh is
//! always an append. The buffer it replaced handed out its whole contents on
//! every poll, and once full, eviction shifted every line — the reader could
//! not tell "two new lines" from "a different log", so it rebuilt, and the
//! rebuild threw the scroll position back to the top twice a second.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::ipc::now_unix_ms;

/// How many records the book holds before the oldest is evicted.
///
/// The buffer this replaced held 500, which a core running at
/// `loglevel = "debug"` fills in seconds — by the time a failure was worth
/// reading about, the reason for it had already been evicted.
pub const CAPACITY: usize = 5000;

/// Records returned by one [`LogBook::since`] call, capped by its `limit`.
pub const DEFAULT_LIMIT: usize = 500;

/// Which program emitted a line.
///
/// `tun2socks` is deliberately its own source rather than being folded in with
/// the core: an interface that never came up and a core that refused its
/// config are different failures with different fixes, and the old code merged
/// both streams into one buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSource {
    /// oxidom's own reasoning: the `log` facade, plus `note`/`fail`.
    Oxidom,
    Xray,
    Tun2socks,
}

impl LogSource {
    pub fn label(self) -> &'static str {
        match self {
            LogSource::Oxidom => "oxidom",
            LogSource::Xray => "xray",
            LogSource::Tun2socks => "tun2socks",
        }
    }
}

/// How serious one line is.
///
/// Deliberately **not** [`crate::core_options::LogLevel`]. That type is a
/// setting — which verbosity to ask the core for — and it has a `Silent`
/// variant that means "emit nothing", which cannot describe a line that
/// exists. Merging the two would make a filter in the UI look like a knob on
/// the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Severity {
    /// True when a record of this severity survives a filter set to
    /// `threshold`. The declared order runs `Error` → `Trace`, so "this
    /// severe or worse" is `self <= threshold`; the method exists so no caller
    /// has to remember which way that reads.
    pub fn at_least(self, threshold: Severity) -> bool {
        self <= threshold
    }

    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "ERROR",
            Severity::Warn => "WARN",
            Severity::Info => "INFO",
            Severity::Debug => "DEBUG",
            Severity::Trace => "TRACE",
        }
    }
}

impl From<log::Level> for Severity {
    fn from(level: log::Level) -> Self {
        match level {
            log::Level::Error => Severity::Error,
            log::Level::Warn => Severity::Warn,
            log::Level::Info => Severity::Info,
            log::Level::Debug => Severity::Debug,
            log::Level::Trace => Severity::Trace,
        }
    }
}

/// One line, with everything needed to filter it without re-parsing text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    pub seq: u64,
    /// When the line *arrived*, not the timestamp the core printed inside it.
    ///
    /// Arrival time is what orders records against oxidom's own, and reading
    /// the core's own stamp would mean parsing a local-time string with no
    /// offset — a date library for a field that would differ by milliseconds.
    /// The printed stamp is stripped from `text` instead of being kept twice.
    pub unix_ms: u64,
    pub source: LogSource,
    pub severity: Severity,
    /// Which profile's session produced it; `None` for anything not tied to one.
    pub profile: Option<String>,
    /// Rust module path for oxidom's own lines, the core's subsystem
    /// (`app/proxyman`) for a line that named one, empty otherwise.
    pub target: String,
    pub text: String,
}

/// What a reader gets back, and enough context to know what it missed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSlice {
    pub records: Vec<LogRecord>,
    /// Identifies the book these sequence numbers belong to.
    ///
    /// Without it a restarted daemon counts from zero again while the GUI's
    /// cursor stays high, so every later request matches nothing and the view
    /// goes quietly dead. A reader that sees this change resets its cursor.
    pub book_id: u64,
    /// Lowest `seq` still held, or `next_seq` when the book is empty.
    pub first_seq: u64,
    /// The `seq` the next pushed record will take.
    pub next_seq: u64,
    /// Records after the reader's cursor that this call could not return —
    /// evicted before it asked, or beyond `limit`. Reported rather than
    /// swallowed: a gap the reader cannot see is a log that lies.
    pub skipped: u64,
}

struct Inner {
    records: VecDeque<LogRecord>,
    next_seq: u64,
}

impl Inner {
    /// Lowest `seq` still held. An empty book reports the seq it *would* hand
    /// out next, so a reader that is fully caught up computes a zero gap.
    fn first_seq(&self) -> u64 {
        self.records
            .front()
            .map(|record| record.seq)
            .unwrap_or(self.next_seq)
    }
}

pub struct LogBook {
    inner: Mutex<Inner>,
    book_id: u64,
}

impl LogBook {
    pub fn new() -> Self {
        LogBook {
            inner: Mutex::new(Inner {
                records: VecDeque::new(),
                next_seq: 1,
            }),
            // Enough to tell one run from the next without pulling in a random
            // number generator: two processes cannot share a pid within the
            // same millisecond.
            book_id: (now_unix_ms() << 16) ^ u64::from(std::process::id()),
        }
    }

    pub fn book_id(&self) -> u64 {
        self.book_id
    }

    /// Record one line.
    ///
    /// Nothing outside this module runs while the lock is held. A callback that
    /// logged would re-enter and deadlock, and the file sink is deliberately
    /// driven from outside for the same reason plus one more: a slow disk must
    /// not stall the thread draining the core's pipe.
    pub fn push(
        &self,
        source: LogSource,
        severity: Severity,
        profile: Option<&str>,
        target: &str,
        text: String,
    ) -> u64 {
        let mut inner = crate::sync::lock(&self.inner);
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.records.push_back(LogRecord {
            seq,
            unix_ms: now_unix_ms(),
            source,
            severity,
            profile: profile.map(str::to_owned),
            target: target.to_string(),
            text,
        });
        while inner.records.len() > CAPACITY {
            inner.records.pop_front();
        }
        seq
    }

    /// Record a line a child process printed, reading its severity out of the
    /// line itself.
    pub fn push_process_line(&self, source: LogSource, profile: Option<&str>, line: &str) -> u64 {
        let parsed = parse_process_line(line);
        self.push(
            source,
            parsed.severity,
            profile,
            parsed.target,
            parsed.text.to_string(),
        )
    }

    /// The seq the next record will take. Callers take this *before* starting a
    /// child so they can later read only that run's output.
    pub fn next_seq(&self) -> u64 {
        crate::sync::lock(&self.inner).next_seq
    }

    /// Everything after `after_seq`, newest-first-priority, at most `limit`.
    ///
    /// When more is waiting than `limit` allows, the **newest** are returned
    /// and the shortfall is reported in [`LogSlice::skipped`]. Returning the
    /// oldest instead would leave a reader that fell behind falling further
    /// behind on every call, and a cold start (`after_seq == 0`) wants the tail
    /// of the log, not its beginning.
    pub fn since(&self, after_seq: u64, limit: usize) -> LogSlice {
        let inner = crate::sync::lock(&self.inner);
        let first_seq = inner.first_seq();
        let matching: Vec<&LogRecord> = inner
            .records
            .iter()
            .filter(|record| record.seq > after_seq)
            .collect();
        let over_limit = matching.len().saturating_sub(limit);
        let records: Vec<LogRecord> = matching
            .into_iter()
            .skip(over_limit)
            .cloned()
            .collect::<Vec<_>>();
        // Sequence numbers between the cursor and the oldest record still held
        // were evicted; that is a real gap and belongs in the count.
        let evicted = first_seq.saturating_sub(after_seq.saturating_add(1));
        LogSlice {
            records,
            book_id: self.book_id,
            first_seq,
            next_seq: inner.next_seq,
            skipped: evicted + over_limit as u64,
        }
    }

    /// Text of the records a given session produced, for the callers that
    /// match the core's own words against known failure markers.
    ///
    /// Scoped by both `profile` and `from_seq` because the book is shared: it
    /// holds other sessions' lines and the previous attempt's, and a marker
    /// read out of either would diagnose the wrong thing.
    pub fn texts_for(
        &self,
        source: LogSource,
        profile: Option<&str>,
        from_seq: u64,
    ) -> Vec<String> {
        crate::sync::lock(&self.inner)
            .records
            .iter()
            .filter(|record| record.seq >= from_seq)
            .filter(|record| record.source == source)
            .filter(|record| profile.is_none_or(|name| record.profile.as_deref() == Some(name)))
            .map(|record| record.text.clone())
            .collect()
    }

    /// Every record still held, oldest first, flattened back to the shape the
    /// pre-cursor D-Bus call promised.
    ///
    /// Keeps the `"oxidom: "` prefix that used to be baked into the text, so a
    /// client older than this book still reads its own lines the way it always
    /// did. New readers use [`Self::since`] and filter on the fields instead.
    pub fn legacy_lines(&self) -> Vec<String> {
        crate::sync::lock(&self.inner)
            .records
            .iter()
            .map(|record| match record.source {
                LogSource::Oxidom => format!("oxidom: {}", record.text),
                LogSource::Xray => record.text.clone(),
                LogSource::Tun2socks => format!("tun2socks: {}", record.text),
            })
            .collect()
    }

    /// Forget what is held without rewinding `seq`: the numbers a reader has
    /// already seen must never be handed to a different line.
    pub fn clear(&self) {
        crate::sync::lock(&self.inner).records.clear();
    }
}

impl Default for LogBook {
    fn default() -> Self {
        Self::new()
    }
}

/// This process's book. The GUI and the daemon each have their own — they are
/// separate processes, and the GUI cannot reach the daemon's memory.
pub fn global() -> &'static LogBook {
    static BOOK: OnceLock<LogBook> = OnceLock::new();
    BOOK.get_or_init(LogBook::new)
}

/// A `log` implementation that writes to the terminal *and* to the book.
///
/// Generic over the terminal half so this crate needs no formatter of its own:
/// each binary hands over the `env_logger::Logger` it already built.
struct Tee {
    terminal: Box<dyn log::Log>,
}

impl log::Log for Tee {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        // The book wants everything the global max level lets through, which is
        // wider than the terminal filter; `log` re-checks against that max
        // before calling us, so answering true here costs nothing extra.
        self.terminal.enabled(metadata) || accepted_by_book(metadata)
    }

    fn log(&self, record: &log::Record<'_>) {
        // `env_logger::Logger::log` applies its own filter, so this is not the
        // same as ignoring it.
        self.terminal.log(record);
        if accepted_by_book(record.metadata()) {
            global().push(
                LogSource::Oxidom,
                record.level().into(),
                None,
                record.target(),
                record.args().to_string(),
            );
        }
    }

    fn flush(&self) {
        self.terminal.flush();
    }
}

/// Which of oxidom's own records the book keeps.
///
/// Anything worth a warning is kept whatever wrote it, because a warning from a
/// dependency is usually about us. Below that, only oxidom's own modules: a
/// `RUST_LOG=debug` run otherwise fills the Logs view with zbus and rustls
/// chatter, and the view exists to explain oxidom.
fn accepted_by_book(metadata: &log::Metadata<'_>) -> bool {
    metadata.level() <= log::Level::Info || metadata.target().starts_with("oxidom")
}

/// Send this process's `log` output to the terminal and the book both.
///
/// `terminal` is the logger that would otherwise have been installed alone, and
/// `max_level` is the level it resolved to — passing anything higher would make
/// every `debug!` in the tree format its arguments before either half could
/// reject it, since `log`'s macros check the level and build the message before
/// any `Log` method is reached.
///
/// Idempotent, and quiet about it. `log` permits exactly one logger per process
/// and `cargo test` runs many tests in one, so a second call must be a no-op
/// rather than an error that only shows up under a full test run.
pub fn install_logger(terminal: Box<dyn log::Log>, max_level: log::LevelFilter) {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    let mut installed = false;
    INSTALLED.get_or_init(|| {
        installed = true;
    });
    if !installed {
        return;
    }
    if log::set_boxed_logger(Box::new(Tee { terminal })).is_ok() {
        log::set_max_level(max_level);
    }
}

/// What a child process's line said about itself.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedLine<'a> {
    pub severity: Severity,
    pub target: &'a str,
    pub text: &'a str,
}

/// Read severity and subsystem out of a core log line.
///
/// Xray writes `2006/01/02 15:04:05 [Warning] app/proxyman: message`. Anything
/// that does not match keeps its whole text and is filed as `Info`: an
/// unrecognised line is still evidence, and guessing a severity for it would
/// hide it behind a filter.
pub fn parse_process_line(line: &str) -> ParsedLine<'_> {
    let rest = strip_timestamp(line.trim_end());
    let Some((severity, rest)) = strip_severity(rest) else {
        return ParsedLine {
            severity: Severity::Info,
            target: "",
            text: rest,
        };
    };
    let (target, text) = split_target(rest);
    ParsedLine {
        severity,
        target,
        text,
    }
}

/// Drop a leading `2006/01/02 15:04:05` (with optional fractional seconds).
///
/// Matched by shape rather than parsed into a time: the core prints local time
/// with no offset, so the only honest thing to do with it is to leave it out
/// and keep the arrival time the book already stamps.
fn strip_timestamp(line: &str) -> &str {
    let mut parts = line.splitn(3, ' ');
    let (Some(date), Some(time), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
        return line;
    };
    let dated = date.len() == 10
        && date.split('/').count() == 3
        && date.chars().all(|c| c.is_ascii_digit() || c == '/');
    let timed = time.len() >= 8
        && time.split(':').count() == 3
        && time
            .chars()
            .all(|c| c.is_ascii_digit() || c == ':' || c == '.');
    if dated && timed {
        rest.trim_start()
    } else {
        line
    }
}

fn strip_severity(line: &str) -> Option<(Severity, &str)> {
    let rest = line.strip_prefix('[')?;
    let (name, rest) = rest.split_once(']')?;
    let severity = match name.to_ascii_lowercase().as_str() {
        "error" | "fatal" | "err" => Severity::Error,
        "warning" | "warn" => Severity::Warn,
        "info" => Severity::Info,
        "debug" => Severity::Debug,
        // A bracketed word that is not a level is part of the message, not a
        // level spelled wrong — `[Observatory]` must not become `Info` with the
        // bracket eaten.
        _ => return None,
    };
    Some((severity, rest.trim_start()))
}

/// Split `app/proxyman: message` into its subsystem and the message.
///
/// A subsystem never contains a space, so anything with one is an ordinary
/// message that happens to hold a colon.
fn split_target(line: &str) -> (&str, &str) {
    match line.split_once(": ") {
        Some((head, tail)) if !head.is_empty() && !head.contains(' ') => (head, tail),
        _ => ("", line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> LogBook {
        LogBook::new()
    }

    fn push(book: &LogBook, text: &str) -> u64 {
        book.push(LogSource::Oxidom, Severity::Info, None, "test", text.into())
    }

    #[test]
    fn a_reader_only_ever_receives_what_follows_its_cursor() {
        let book = book();
        push(&book, "one");
        push(&book, "two");

        let first = book.since(0, DEFAULT_LIMIT);
        assert_eq!(texts(&first), ["one", "two"]);
        assert_eq!(first.skipped, 0);

        let cursor = first.next_seq - 1;
        push(&book, "three");
        let second = book.since(cursor, DEFAULT_LIMIT);
        assert_eq!(texts(&second), ["three"]);
        assert_eq!(second.skipped, 0);
    }

    #[test]
    fn a_caught_up_reader_receives_nothing_rather_than_a_repeat() {
        let book = book();
        push(&book, "one");
        let slice = book.since(0, DEFAULT_LIMIT);
        let cursor = slice.next_seq - 1;

        let again = book.since(cursor, DEFAULT_LIMIT);
        assert!(again.records.is_empty());
        assert_eq!(again.skipped, 0);
    }

    #[test]
    fn sequence_numbers_survive_eviction_without_being_reused() {
        let book = book();
        for index in 0..CAPACITY + 10 {
            push(&book, &format!("line {index}"));
        }
        let slice = book.since(0, CAPACITY);

        assert_eq!(slice.records.len(), CAPACITY);
        assert_eq!(slice.first_seq, 11, "the first ten must have been evicted");
        assert_eq!(slice.next_seq, CAPACITY as u64 + 11);
        // Strictly increasing, with no number handed out twice.
        assert!(
            slice
                .records
                .windows(2)
                .all(|pair| pair[1].seq == pair[0].seq + 1)
        );
    }

    #[test]
    fn a_gap_the_reader_cannot_see_is_counted_for_it() {
        let book = book();
        for index in 0..CAPACITY + 10 {
            push(&book, &format!("line {index}"));
        }
        // A cursor from before the eviction: ten records are gone for good.
        let slice = book.since(5, CAPACITY);
        assert_eq!(slice.skipped, 5, "seqs 6..=10 were evicted");
    }

    #[test]
    fn a_reader_behind_by_more_than_its_limit_gets_the_newest_lines() {
        let book = book();
        for index in 0..10 {
            push(&book, &format!("line {index}"));
        }
        let slice = book.since(0, 3);

        assert_eq!(texts(&slice), ["line 7", "line 8", "line 9"]);
        assert_eq!(slice.skipped, 7);
    }

    #[test]
    fn clearing_forgets_the_lines_but_not_the_count() {
        let book = book();
        push(&book, "one");
        push(&book, "two");
        book.clear();

        let slice = book.since(0, DEFAULT_LIMIT);
        assert!(slice.records.is_empty());
        assert_eq!(slice.next_seq, 3, "seq must not rewind over a cleared line");
        push(&book, "three");
        assert_eq!(texts(&book.since(2, DEFAULT_LIMIT)), ["three"]);
    }

    #[test]
    fn text_lookup_ignores_other_sources_other_profiles_and_earlier_runs() {
        let book = book();
        book.push(
            LogSource::Xray,
            Severity::Error,
            Some("work"),
            "",
            "old attempt".into(),
        );
        let watermark = book.next_seq();
        book.push(
            LogSource::Xray,
            Severity::Error,
            Some("work"),
            "",
            "this attempt".into(),
        );
        book.push(
            LogSource::Xray,
            Severity::Error,
            Some("home"),
            "",
            "other profile".into(),
        );
        book.push(
            LogSource::Oxidom,
            Severity::Warn,
            Some("work"),
            "",
            "our own note".into(),
        );

        assert_eq!(
            book.texts_for(LogSource::Xray, Some("work"), watermark),
            ["this attempt"]
        );
    }

    #[test]
    fn a_core_line_yields_its_level_and_subsystem_without_its_timestamp() {
        let parsed =
            parse_process_line("2026/08/17 10:48:03 [Warning] app/proxyman: bad handshake");
        assert_eq!(parsed.severity, Severity::Warn);
        assert_eq!(parsed.target, "app/proxyman");
        assert_eq!(parsed.text, "bad handshake");
    }

    #[test]
    fn fractional_seconds_and_every_spelling_of_a_level_are_understood() {
        let parsed = parse_process_line("2026/08/17 10:48:03.123 [Error] core: gone");
        assert_eq!(parsed.severity, Severity::Error);
        assert_eq!(parsed.text, "gone");
        for (name, expected) in [
            ("Debug", Severity::Debug),
            ("Info", Severity::Info),
            ("Warning", Severity::Warn),
            ("Error", Severity::Error),
        ] {
            let line = format!("2026/08/17 10:48:03 [{name}] app: hello");
            assert_eq!(parse_process_line(&line).severity, expected, "{name}");
        }
    }

    #[test]
    fn an_unrecognised_line_keeps_all_of_its_text() {
        for line in [
            "tun2socks started on tun0",
            "[Observatory] not a level at all",
            "2026/08/17 10:48:03 no level here",
        ] {
            let parsed = parse_process_line(line);
            assert_eq!(parsed.severity, Severity::Info, "{line}");
            assert!(
                parsed.text.contains(line.split(' ').next_back().unwrap()),
                "{line} lost text: {}",
                parsed.text
            );
        }
    }

    /// The markers the daemon matches on live in the message, so stripping the
    /// timestamp and subsystem must not disturb them.
    #[test]
    fn a_rejected_protocol_marker_survives_parsing() {
        let parsed = parse_process_line(
            "2026/08/17 10:48:03 [Error] app/proxyman/outbound: failed to build \
             outbound: unknown protocol",
        );
        assert!(parsed.text.contains("unknown protocol"), "{}", parsed.text);
    }

    /// A message whose own text holds a colon must not have half of it read as
    /// a subsystem.
    #[test]
    fn a_message_containing_a_colon_keeps_it() {
        let parsed = parse_process_line("2026/08/17 10:48:03 [Info] failed to dial: timeout");
        assert_eq!(parsed.target, "");
        assert_eq!(parsed.text, "failed to dial: timeout");
    }

    /// A `RUST_LOG=debug` run must not bury oxidom's own reasoning under the
    /// debug output of every crate it links, but a warning from any of them is
    /// usually a warning about us and is kept.
    #[test]
    fn the_book_keeps_our_own_chatter_and_everyone_elses_warnings() {
        let accepted = |target: &str, level: log::Level| {
            accepted_by_book(&log::Metadata::builder().target(target).level(level).build())
        };

        assert!(accepted("oxidom_core::engine", log::Level::Debug));
        assert!(accepted("oxidom_gui::gui::window", log::Level::Trace));
        assert!(accepted("zbus::connection", log::Level::Warn));
        assert!(accepted("rustls::client", log::Level::Info));
        assert!(!accepted("zbus::connection", log::Level::Debug));
        assert!(!accepted("rustls::client", log::Level::Trace));
    }

    #[test]
    fn a_severity_filter_admits_everything_at_least_as_serious() {
        assert!(Severity::Error.at_least(Severity::Warn));
        assert!(Severity::Warn.at_least(Severity::Warn));
        assert!(!Severity::Info.at_least(Severity::Warn));
        assert!(!Severity::Debug.at_least(Severity::Info));
        assert!(Severity::Trace.at_least(Severity::Trace));
    }

    fn texts(slice: &LogSlice) -> Vec<&str> {
        slice
            .records
            .iter()
            .map(|record| record.text.as_str())
            .collect()
    }
}
