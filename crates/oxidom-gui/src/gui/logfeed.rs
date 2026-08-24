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

use oxidom_core::logbook::{CAPACITY, LEGACY_BOOK_ID, LogRecord, LogSlice};

/// How long a record waits before it is considered safely ordered.
///
/// Comfortably longer than the 500 ms poll interval that sets how late a
/// daemon record can be. The cost is that the newest lines appear that much
/// after the fact, which at this size reads as live.
pub const REORDER_WINDOW_MS: u64 = 600;

/// How many records may wait at once before the window is given up on.
///
/// The window releases on a timestamp, which assumes timestamps advance. A clock
/// stepped backwards, or one record stamped in the future, moves the cutoff
/// behind everything held and holds it there for the whole size of the
/// discrepancy — and `holding` had no ceiling, so an NTP correction was enough to
/// grow it without end.
///
/// The number is the log book's own capacity rather than a figure of its own:
/// holding more records than the book keeps cannot help anybody, because the
/// view discards the excess on arrival regardless.
const HOLD_CAPACITY: usize = CAPACITY;

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

    /// How many records are waiting for the window to open. Exists so a test can
    /// state the ceiling as an invariant rather than reaching into the field.
    #[cfg(test)]
    fn held(&self) -> usize {
        self.holding.len()
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
            // The reset replaces the view, and the caller reads this process's
            // own book *before* absorbing — so the local cursor has to stay
            // rewound for the next round to re-send the lines the GUI itself
            // wrote. Without the re-feed they survive one round of a legacy
            // daemon and are gone from the view for good.
            self.local_cursor = 0;
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
        // Under a legacy book the cursor stays at zero (see above); advancing
        // it here would undo the rewind before the caller ever read from it.
        if remote.book_id != LEGACY_BOOK_ID {
            if let Some(last) = local.records.last() {
                self.local_cursor = last.seq;
            }
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
        let mut records: Vec<LogRecord> = self.holding.drain(..settled).collect();

        // Over the ceiling, the oldest of what is still held goes out anyway,
        // ordering unproven. Releasing early is the right way to lose this
        // argument: the alternative is dropping records, and a gap the reader
        // cannot see is worse than two lines in the wrong order. Nothing is
        // lost, so nothing is announced as skipped.
        //
        // `holding` is sorted ascending and every settled record was below the
        // cutoff, so these are still the oldest remaining and appending them
        // after keeps the batch in order.
        if self.holding.len() > HOLD_CAPACITY {
            let excess = self.holding.len() - HOLD_CAPACITY;
            records.extend(self.holding.drain(..excess));
        }

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

    /// The window releases on a timestamp, so a stamp from the future — or a
    /// clock stepped backwards — puts the cutoff behind everything held and keeps
    /// it there for the size of the discrepancy. `holding` had no ceiling, so a
    /// single NTP correction was enough to grow it for as long as the window
    /// stayed shut.
    #[test]
    fn a_record_stamped_in_the_future_does_not_grow_the_hold_without_end() {
        let mut feed = LogFeed::new();
        let now_ms = 1_000_000;
        // One record stamped a day ahead. Nothing that arrives afterwards can
        // reach the cutoff while it is held, so the window never opens.
        let future = now_ms + 86_400_000;
        let mut seq = 0;
        let mut released = 0;
        for _ in 0..4 {
            let batch = feed.absorb(
                slice(
                    DAEMON,
                    (0..2_000)
                        .map(|_| {
                            seq += 1;
                            record(seq, future, LogSource::Xray, "from the future")
                        })
                        .collect(),
                ),
                empty(LOCAL),
                now_ms,
            );
            released += batch.records.len();
            assert!(
                feed.held() <= HOLD_CAPACITY,
                "the hold must stay bounded even while the window cannot open: {} held",
                feed.held()
            );
        }

        assert_eq!(
            released + feed.held(),
            seq as usize,
            "every record either went out or is still held — none were dropped"
        );
    }

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

    /// A legacy daemon's reset replaces the view, and the GUI's own lines must
    /// come back with it: the caller reads the local book before absorbing, so
    /// only a cursor that stays rewound makes the next round re-send them.
    #[test]
    fn a_reset_re_feeds_the_guis_own_lines() {
        let mut feed = LogFeed::new();
        let remote_lines = vec![record(1, 0, LogSource::Xray, "whole log")];
        let local_line = vec![record(5, 1_000, LogSource::Oxidom, "own line")];

        let first = feed.absorb(
            slice(LEGACY_BOOK_ID, remote_lines.clone()),
            slice(LOCAL, local_line.clone()),
            9_000,
        );
        assert!(first.reset);
        assert_eq!(texts(&first), ["whole log", "own line"]);
        assert_eq!(feed.local_cursor(), 0, "the next read starts over");

        let second = feed.absorb(
            slice(LEGACY_BOOK_ID, remote_lines),
            slice(LOCAL, local_line),
            9_500,
        );
        assert!(second.reset);
        assert_eq!(texts(&second), ["whole log", "own line"]);
    }

    /// A reset does not swallow the gap either book reported: the batch carries
    /// both, and the view announces the gap after replacing itself.
    #[test]
    fn a_reset_still_announces_the_lines_the_daemon_could_not_hand_over() {
        let mut feed = LogFeed::new();
        let mut remote = slice(LEGACY_BOOK_ID, vec![record(1, 0, LogSource::Xray, "kept")]);
        remote.skipped = 7;

        let batch = feed.absorb(remote, empty(LOCAL), 9_000);

        assert!(batch.reset);
        assert_eq!(batch.skipped, 7);
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
