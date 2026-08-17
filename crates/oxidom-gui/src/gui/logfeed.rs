//! Turning two independent log streams into one ordered one.
//!
//! The GUI reads two books that know nothing of each other: the daemon's, over
//! D-Bus, and its own, in this process. Their sequence numbers are unrelated,
//! so ordering has to come from the arrival timestamps — and those cannot be
//! trusted the instant a record shows up. The daemon is polled twice a second,
//! so one of its records may reach us half a second after it was written, by
//! which time a GUI record stamped *later* has already been shown. Appending as
//! they arrive would leave the two permanently interleaved wrong, and the only
//! repair would be the full redraw this whole change exists to remove.
//!
//! So a record is held briefly before it is released. Anything older than
//! [`REORDER_WINDOW_MS`] is past the point where a straggler can still arrive to
//! sit in front of it, and can be appended in timestamp order for good.

use oxidom_core::logbook::{LEGACY_BOOK_ID, LogRecord, LogSlice};

/// How long a record waits before it is considered safely ordered.
///
/// Comfortably longer than the 500 ms poll interval that sets how late a
/// daemon record can be. The cost is that the newest lines appear that much
/// after the fact, which at this size reads as live.
pub const REORDER_WINDOW_MS: u64 = 600;

/// What one tick produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FeedBatch {
    /// Records to append, oldest first.
    pub records: Vec<LogRecord>,
    /// Discard what is on screen before appending: the daemon restarted, or it
    /// is too old to support a cursor and re-sent everything.
    pub reset: bool,
    /// Records the daemon could not hand over, accumulated since the last tick.
    pub skipped: u64,
}

/// Merges the daemon's log with the GUI's own, in order.
#[derive(Debug, Default)]
pub struct LogFeed {
    remote_cursor: u64,
    remote_book: Option<u64>,
    local_cursor: u64,
    /// Records that have arrived but are not yet old enough to be ordered.
    holding: Vec<LogRecord>,
}

impl LogFeed {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cursor to send with the next request to the daemon.
    pub fn remote_cursor(&self) -> u64 {
        self.remote_cursor
    }

    /// The cursor to read this process's own book from.
    pub fn local_cursor(&self) -> u64 {
        self.local_cursor
    }

    /// Take one round of both books and return what is now safe to display.
    pub fn absorb(&mut self, remote: LogSlice, local: LogSlice, now_ms: u64) -> FeedBatch {
        let mut reset = false;
        let mut skipped = remote.skipped;

        // A daemon with no cursor re-sends its whole log every time, so the only
        // correct move is to replace what is shown. This is the pre-cursor
        // behaviour, kept deliberately rather than allowed to drift into
        // duplicated lines.
        if remote.book_id == LEGACY_BOOK_ID {
            reset = true;
            self.holding.clear();
            self.remote_cursor = 0;
        } else if self.remote_book != Some(remote.book_id) {
            // First round, or the daemon restarted and is counting from zero
            // again. Without this the cursor stays above every sequence number
            // the new book will ever issue and the view goes quietly dead.
            reset = self.remote_book.is_some();
            self.remote_book = Some(remote.book_id);
            self.holding.clear();
            self.remote_cursor = 0;
        }

        if let Some(last) = remote.records.last() {
            self.remote_cursor = last.seq;
        }
        self.holding.extend(remote.records);

        skipped += local.skipped;
        if let Some(last) = local.records.last() {
            self.local_cursor = last.seq;
        }
        self.holding.extend(local.records);

        // A record with no usable timestamp — a reconstructed one from an older
        // daemon — sorts first and is released at once; there is nothing to
        // order it against.
        self.holding.sort_by_key(|record| record.unix_ms);
        let cutoff = now_ms.saturating_sub(REORDER_WINDOW_MS);
        let settled = self
            .holding
            .iter()
            .take_while(|record| record.unix_ms <= cutoff)
            .count();
        let records: Vec<LogRecord> = self.holding.drain(..settled).collect();

        FeedBatch {
            records,
            reset,
            skipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidom_core::logbook::{LogSource, Severity};

    fn record(seq: u64, unix_ms: u64, source: LogSource, text: &str) -> LogRecord {
        LogRecord {
            seq,
            unix_ms,
            source,
            severity: Severity::Info,
            profile: None,
            target: String::new(),
            text: text.to_string(),
        }
    }

    fn slice(book_id: u64, records: Vec<LogRecord>) -> LogSlice {
        let next_seq = records.last().map(|r| r.seq + 1).unwrap_or(1);
        LogSlice {
            records,
            book_id,
            first_seq: 1,
            next_seq,
            skipped: 0,
        }
    }

    fn empty(book_id: u64) -> LogSlice {
        slice(book_id, Vec::new())
    }

    const DAEMON: u64 = 77;
    const LOCAL: u64 = 99;

    #[test]
    fn a_record_is_held_until_no_straggler_can_still_precede_it() {
        let mut feed = LogFeed::new();
        let batch = feed.absorb(
            slice(DAEMON, vec![record(1, 10_000, LogSource::Xray, "fresh")]),
            empty(LOCAL),
            10_100,
        );
        assert!(
            batch.records.is_empty(),
            "a record 100ms old is still inside the window"
        );

        let batch = feed.absorb(empty(DAEMON), empty(LOCAL), 10_000 + REORDER_WINDOW_MS);
        assert_eq!(texts(&batch), ["fresh"]);
    }

    /// The case the window exists for: the daemon's record is written first but
    /// arrives a poll later, after a GUI record with a higher timestamp has
    /// already been handed over.
    #[test]
    fn a_late_daemon_record_still_lands_before_the_gui_record_it_predates() {
        let mut feed = LogFeed::new();
        feed.absorb(
            empty(DAEMON),
            slice(
                LOCAL,
                vec![record(1, 5_200, LogSource::Oxidom, "from the gui")],
            ),
            5_300,
        );
        let batch = feed.absorb(
            slice(
                DAEMON,
                vec![record(1, 5_100, LogSource::Xray, "from the core")],
            ),
            empty(LOCAL),
            9_000,
        );

        assert_eq!(texts(&batch), ["from the core", "from the gui"]);
    }

    #[test]
    fn the_cursor_advances_to_the_last_record_of_each_book() {
        let mut feed = LogFeed::new();
        feed.absorb(
            slice(
                DAEMON,
                vec![
                    record(4, 1_000, LogSource::Xray, "a"),
                    record(5, 1_000, LogSource::Xray, "b"),
                ],
            ),
            slice(LOCAL, vec![record(9, 1_000, LogSource::Oxidom, "c")]),
            9_000,
        );

        assert_eq!(feed.remote_cursor(), 5);
        assert_eq!(feed.local_cursor(), 9);
    }

    #[test]
    fn a_restarted_daemon_resets_the_cursor_instead_of_going_silent() {
        let mut feed = LogFeed::new();
        feed.absorb(
            slice(DAEMON, vec![record(900, 1_000, LogSource::Xray, "old run")]),
            empty(LOCAL),
            9_000,
        );
        assert_eq!(feed.remote_cursor(), 900);

        // A new process: a different book, counting from one again.
        let batch = feed.absorb(
            slice(
                DAEMON + 1,
                vec![record(1, 2_000, LogSource::Xray, "new run")],
            ),
            empty(LOCAL),
            9_000,
        );

        assert!(batch.reset, "the view must drop the dead run's lines");
        assert_eq!(texts(&batch), ["new run"]);
        assert_eq!(feed.remote_cursor(), 1);
    }

    /// The very first round is not a restart — there is nothing on screen to
    /// throw away, and reporting one would make the view flicker at startup.
    #[test]
    fn the_first_round_is_not_reported_as_a_restart() {
        let mut feed = LogFeed::new();
        let batch = feed.absorb(
            slice(DAEMON, vec![record(1, 1_000, LogSource::Xray, "hello")]),
            empty(LOCAL),
            9_000,
        );
        assert!(!batch.reset);
        assert_eq!(texts(&batch), ["hello"]);
    }

    #[test]
    fn a_daemon_without_a_cursor_replaces_the_view_every_round() {
        let mut feed = LogFeed::new();
        let lines = vec![record(1, 0, LogSource::Xray, "whole log")];

        let first = feed.absorb(slice(LEGACY_BOOK_ID, lines.clone()), empty(LOCAL), 9_000);
        assert!(first.reset);
        assert_eq!(texts(&first), ["whole log"]);

        // It re-sends everything, so replacing again is what keeps the view from
        // showing each line twice.
        let second = feed.absorb(slice(LEGACY_BOOK_ID, lines), empty(LOCAL), 9_500);
        assert!(second.reset);
        assert_eq!(texts(&second), ["whole log"]);
    }

    #[test]
    fn a_gap_reported_by_either_book_reaches_the_caller() {
        let mut feed = LogFeed::new();
        let mut remote = empty(DAEMON);
        remote.skipped = 12;
        let mut local = empty(LOCAL);
        local.skipped = 3;

        assert_eq!(feed.absorb(remote, local, 9_000).skipped, 15);
    }

    fn texts(batch: &FeedBatch) -> Vec<&str> {
        batch
            .records
            .iter()
            .map(|record| record.text.as_str())
            .collect()
    }
}
