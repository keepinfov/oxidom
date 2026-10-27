//! Read-only journal snapshots. Cursors are untrusted, bounded opaque positions, not
//! authority to open a path. Neither raw journal strings nor parser errors are
//! returned. Unknown fields are discarded by typed deserialization. A gap means
//! the previous position cannot be proved; the number of lost records is unknown.

use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::digest::{SHA256, digest};

/// A bounded oldest-first page of typed diagnostics, never raw journal JSON.
#[derive(Debug, Serialize, Deserialize)]
pub struct Page {
    pub version: u8,
    pub records: Vec<Record>,
    pub next_cursor: Option<String>,
    pub gap: bool,
    pub rejected: u32,
    pub incomplete_tail: bool,
    pub more: bool,
}

impl Page {
    /// Bound and validate a remote page before exposing it to a CLI/exporter.
    pub fn validate(&self) -> io::Result<()> {
        if self.version != 1
            || self.records.len() > 256
            || self
                .next_cursor
                .as_deref()
                .is_some_and(|cursor| Cursor::decode(cursor).is_none())
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        for record in &self.records {
            if record.version != 1 || serde_json::to_vec(record)?.len() >= MAX_RECORD_BYTES {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        Ok(())
    }
}

struct Generation {
    dev: u64,
    ino: u64,
    bytes: Vec<u8>,
    current: bool,
}

// Fixed binary encoding: version, dev, ino, offset, hashed prefix length, SHA256.
// The hash covers consumed bytes only: unread tails may be repaired by a writer.
struct Cursor {
    dev: u64,
    ino: u64,
    offset: usize,
    prefix: usize,
    hash: [u8; 32],
}

impl Cursor {
    fn decode(text: &str) -> Option<Self> {
        if text.len() != 87 {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
        if bytes.len() != 65 || bytes[0] != 1 {
            return None;
        }
        let number = |start| u64::from_be_bytes(bytes[start..start + 8].try_into().unwrap());
        let offset = number(17);
        let prefix = number(25);
        if offset > MAX_FILE_BYTES || prefix > MAX_FILE_BYTES || prefix < offset {
            return None;
        }
        Some(Self {
            dev: number(1),
            ino: number(9),
            offset: offset as usize,
            prefix: prefix as usize,
            hash: bytes[33..].try_into().unwrap(),
        })
    }

    fn matches(&self, generation: &Generation) -> bool {
        self.dev == generation.dev
            && self.ino == generation.ino
            && self.prefix <= generation.bytes.len()
            && (self.offset == 0 || generation.bytes[self.offset - 1] == b'\n'
                // Only a backup's discarded partial tail may end off a newline.
                || (!generation.current && self.offset == generation.bytes.len()))
            && digest(&SHA256, &generation.bytes[..self.prefix]).as_ref() == self.hash
    }

    fn encode(generation: &Generation, offset: usize) -> String {
        let prefix = offset;
        let mut bytes = Vec::with_capacity(65);
        bytes.push(1);
        for value in [generation.dev, generation.ino, offset as u64, prefix as u64] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(digest(&SHA256, &generation.bytes[..prefix]).as_ref());
        URL_SAFE_NO_PAD.encode(bytes)
    }
}

/// Read without creating, repairing, chmodding, or opening any file writable.
/// `limit == 0` means 64 records; larger limits are capped at 256. Malformed or
/// lost cursors reset to the oldest retained byte with `gap = true`. Complete
/// invalid lines count as rejected and advance the cursor. An incomplete current
/// tail does not advance it and sets `incomplete_tail`, not `more`.
///
/// Missing journals/lock return `NotFound`; lock contention returns `WouldBlock`.
/// Errors have fixed text or OS codes, never paths, cursor text, or journal text.
/// At most 4 MiB of journal bytes are snapshotted under the stable shared lock.
/// Like the writer, this cannot defend against a hostile same-uid process that
/// ignores the lock. Hashes prove prefix continuity, not cursor authenticity or
/// changes that restore exactly the same bytes and file identity.
pub fn read(path: &Path, cursor: Option<&str>, limit: u32) -> io::Result<Page> {
    read_inner(path, cursor, limit).map_err(|error| match error.raw_os_error() {
        Some(code) => io::Error::from_raw_os_error(code),
        None => io::Error::new(error.kind(), "diagnostic read failed"),
    })
}

fn open_leaf(directory: &File, name: &CStr) -> io::Result<Option<File>> {
    // SAFETY: directory and NUL-terminated name live throughout openat. No
    // write/create flags are used; O_NONBLOCK prevents FIFO opens from waiting.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // SAFETY: openat returned a new owned fd, transferred exactly once.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(Some(file))
}

fn read_inner(path: &Path, cursor: Option<&str>, limit: u32) -> io::Result<Page> {
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let leaf = |suffix: &[u8]| {
        let mut bytes = name.as_bytes().to_vec();
        bytes.extend_from_slice(suffix);
        CString::new(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    };
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(parent)?;
    let metadata = directory.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let lock = open_leaf(&directory, &leaf(b".lock")?)?.ok_or(io::ErrorKind::NotFound)?;
    lock.try_lock_shared().map_err(|error| match error {
        TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
        TryLockError::Error(error) => error,
    })?;
    let unlock = super::Unlock(&lock);
    #[cfg(test)]
    super::tests::retain_lock(&lock);
    let mut generations = Vec::with_capacity(2);
    for (suffix, current) in [(b".1".as_slice(), false), (b"".as_slice(), true)] {
        if let Some(file) = open_leaf(&directory, &leaf(suffix)?)? {
            let metadata = file.metadata()?;
            if metadata.len() > MAX_FILE_BYTES {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut bytes = vec![0; metadata.len() as usize];
            file.read_exact_at(&mut bytes, 0)?;
            generations.push(Generation {
                dev: metadata.dev(),
                ino: metadata.ino(),
                bytes,
                current,
            });
        }
    }
    if generations.is_empty() {
        return Err(io::ErrorKind::NotFound.into());
    }
    // All raw bytes are now bounded and owned; do not hold up appends while parsing.
    drop(unlock);
    drop(lock);
    let mut page = Page {
        version: 1,
        records: Vec::new(),
        next_cursor: None,
        gap: false,
        rejected: 0,
        incomplete_tail: false,
        more: false,
    };
    let mut index = 0;
    let mut offset = 0;
    if let Some(text) = cursor {
        match Cursor::decode(text).and_then(|position| {
            generations
                .iter()
                .position(|g| position.matches(g))
                .map(|i| (i, position.offset))
        }) {
            Some((i, start)) => {
                index = i;
                offset = start;
            }
            None => page.gap = true,
        }
    }
    let limit = if limit == 0 { 64 } else { limit.min(256) } as usize;
    loop {
        let generation = &generations[index];
        let remaining = &generation.bytes[offset..];
        if remaining.is_empty() && index + 1 < generations.len() {
            index += 1;
            offset = 0;
            continue;
        }
        if page.records.len() == limit {
            page.more =
                remaining.contains(&b'\n') || (!generation.current && !remaining.is_empty());
            page.incomplete_tail = generation.current && !remaining.is_empty() && !page.more;
            break;
        }
        let Some(end) = remaining.iter().position(|&byte| byte == b'\n') else {
            if !remaining.is_empty() {
                if generation.current {
                    page.incomplete_tail = true;
                } else {
                    page.rejected += 1;
                    offset = generation.bytes.len();
                    continue;
                }
            }
            break;
        };
        let line = &remaining[..=end];
        let record = if line.len() <= MAX_RECORD_BYTES {
            serde_json::from_slice::<Record>(line)
                .ok()
                .filter(|record| record.version == 1)
        } else {
            None
        };
        match record {
            Some(record) => page.records.push(record),
            None => page.rejected += 1,
        }
        offset += line.len();
    }
    // An empty initial current can legitimately have no cursor; an incomplete
    // nonempty tail needs an offset-zero cursor so later completion is lossless.
    if cursor.is_some() || index != 0 || offset != 0 || !generations[index].bytes.is_empty() {
        page.next_cursor = Some(Cursor::encode(&generations[index], offset));
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    const ONE: &[u8] =
        b"{\"version\":1,\"unix_ms\":1,\"pid\":42,\"event\":\"phase\",\"phase\":\"starting\"}\n";
    const TWO: &[u8] =
        b"{\"version\":1,\"unix_ms\":2,\"pid\":42,\"event\":\"phase\",\"phase\":\"stopped\"}\n";

    struct Journal(PathBuf);

    impl Journal {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "oxidom-reader-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => {
                        let journal = Self(path);
                        journal.put("journal.lock", b"");
                        return journal;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("creating test directory: {error}"),
                }
            }
        }

        fn path(&self) -> PathBuf {
            self.0.join("journal")
        }

        fn put(&self, name: &str, bytes: &[u8]) {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(self.0.join(name))
                .unwrap();
            file.write_all(bytes).unwrap();
        }

        fn rotate(&self, bytes: &[u8]) {
            fs::rename(self.path(), self.0.join("journal.1")).unwrap();
            self.put("journal", bytes);
        }

        fn read(&self, cursor: Option<&str>, limit: u32) -> Page {
            read(&self.path(), cursor, limit).unwrap()
        }
    }

    impl Drop for Journal {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn pages_serialize_only_literal_typed_fields_and_strip_unknown_secrets() {
        let journal = Journal::new();
        journal.put("journal", b"{\"version\":1,\"unix_ms\":1,\"pid\":42,\"event\":\"phase\",\"phase\":\"starting\",\"secret\":\"invented-private-secret\"}\n");
        let mut page = journal.read(None, 0);
        assert_eq!(
            serde_json::to_vec(&page.records[0]).unwrap(),
            &ONE[..ONE.len() - 1]
        );
        assert!(page.next_cursor.take().is_some());
        let literal = r#"{"version":1,"records":[{"version":1,"unix_ms":1,"pid":42,"event":"phase","phase":"starting"}],"next_cursor":null,"gap":false,"rejected":0,"incomplete_tail":false,"more":false}"#;
        assert_eq!(serde_json::to_string(&page).unwrap(), literal);
        assert_eq!(
            serde_json::from_str::<Page>(literal).unwrap().records.len(),
            1
        );

        journal.put("journal", b"{\"version\":1,\"unix_ms\":1,\"pid\":42,\"event\":\"session_opened\",\"context\":{\"run\":1,\"session\":2,\"generation\":3,\"attempt\":null,\"core\":null,\"secret\":\"invented-private-secret\"}}\n");
        assert!(
            !serde_json::to_string(&journal.read(None, 0))
                .unwrap()
                .contains("invented-private-secret")
        );
    }

    #[test]
    fn pagination_is_oldest_first_without_skips_duplicates_or_false_more() {
        let journal = Journal::new();
        journal.put("journal.1", &[ONE, TWO].concat());
        journal.put("journal", &[ONE, TWO].concat());
        let mut cursor = None;
        let mut seen = Vec::new();
        for expected_more in [true, true, true, false] {
            let page = journal.read(cursor.as_deref(), 1);
            assert!(!page.gap);
            assert_eq!(page.more, expected_more);
            seen.extend(page.records.iter().map(|r| r.unix_ms));
            cursor = page.next_cursor;
        }
        assert_eq!(seen, [1, 2, 1, 2]);
        let end = journal.read(cursor.as_deref(), 1);
        assert!(end.records.is_empty());
        assert!(!end.more);
        assert!(!end.gap);
    }

    #[test]
    fn an_empty_new_current_does_not_replay_the_drained_backup() {
        let journal = Journal::new();
        journal.put("journal.1", ONE);
        journal.put("journal", b"");
        let page = journal.read(None, 1);
        assert_eq!(page.records.len(), 1);
        assert!(!page.more);
        assert!(page.next_cursor.is_some());
        let next = journal.read(page.next_cursor.as_deref(), 1);
        assert!(!next.gap);
        assert!(next.records.is_empty());
        journal.put("journal", TWO);
        let next = journal.read(next.next_cursor.as_deref(), 1);
        assert!(!next.gap);
        assert_eq!(next.records[0].unix_ms, 2);
    }

    #[test]
    fn repair_of_an_unconsumed_tail_does_not_invalidate_a_consumed_prefix() {
        let journal = Journal::new();
        journal.put("journal", &[ONE, b"partial"].concat());
        let page = journal.read(None, 1);
        assert!(page.incomplete_tail);
        journal.put("journal", &[ONE, TWO].concat());
        let page = journal.read(page.next_cursor.as_deref(), 1);
        assert!(!page.gap);
        assert_eq!(page.records[0].unix_ms, 2);
    }

    #[test]
    fn repairing_an_unread_current_tail_does_not_replay_the_consumed_backup() {
        let journal = Journal::new();
        journal.put("journal.1", ONE);
        journal.put("journal", b"incomplete record");
        let page = journal.read(None, 0);
        assert_eq!(page.records.len(), 1);
        assert!(page.incomplete_tail);
        let sink = Diagnostics::open(journal.path()).unwrap();
        sink.append(TWO).unwrap();
        let page = journal.read(page.next_cursor.as_deref(), 0);
        assert!(!page.gap);
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].unix_ms, 2);
    }

    #[test]
    fn remote_pages_cannot_export_free_text_cursors_or_unsupported_records() {
        let journal = Journal::new();
        journal.put("journal", ONE);
        let mut page = journal.read(None, 0);
        page.validate().unwrap();
        page.next_cursor = Some("invented-private-secret".into());
        assert!(page.validate().is_err());
        page.next_cursor = None;
        page.records[0].version = 2;
        assert!(page.validate().is_err());
        page.records.clear();
        page.version = 2;
        assert!(page.validate().is_err());
    }

    #[test]
    fn one_rotation_follows_old_current_and_two_rotations_report_a_gap() {
        let journal = Journal::new();
        journal.put("journal", ONE);
        let cursor = journal.read(None, 1).next_cursor.unwrap();
        let original = File::open(journal.path()).unwrap();
        journal.rotate(TWO);
        let page = journal.read(Some(&cursor), 1);
        assert!(!page.gap);
        assert_eq!(page.records[0].unix_ms, 2);
        assert!(!page.more);
        journal.rotate(ONE);
        let page = journal.read(Some(&cursor), 0);
        assert!(page.gap);
        assert_eq!(
            page.records.iter().map(|r| r.unix_ms).collect::<Vec<_>>(),
            [2, 1]
        );
        drop(original);
    }

    #[test]
    fn restarting_the_writer_preserves_a_cursor_and_appended_records() {
        let journal = Journal::new();
        journal.put("journal", ONE);
        let cursor = journal.read(None, 0).next_cursor.unwrap();
        let sink = Diagnostics::open(journal.path()).unwrap();
        drop(sink);
        let sink = Diagnostics::open(journal.path()).unwrap();
        sink.append(TWO).unwrap();
        let page = journal.read(Some(&cursor), 0);
        assert!(!page.gap);
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].unix_ms, 2);
    }

    #[test]
    fn truncation_rewrites_replacements_and_untrusted_cursors_report_gaps() {
        let journal = Journal::new();
        for replacement in [b"".as_slice(), TWO, &[TWO, TWO].concat()] {
            journal.put("journal", ONE);
            let cursor = journal.read(None, 0).next_cursor.unwrap();
            let inode = fs::metadata(journal.path()).unwrap().ino();
            journal.put("journal", replacement);
            assert_eq!(fs::metadata(journal.path()).unwrap().ino(), inode);
            assert!(journal.read(Some(&cursor), 0).gap);
        }
        journal.put("journal", ONE);
        let cursor = journal.read(None, 0).next_cursor.unwrap();
        // Keep the old inode alive so replacement identity is deterministic.
        let old = File::open(journal.path()).unwrap();
        fs::remove_file(journal.path()).unwrap();
        journal.put("journal", ONE);
        assert!(journal.read(Some(&cursor), 0).gap);
        drop(old);
        for cursor in ["", "invented-private-secret", &"A".repeat(4096)] {
            let page = journal.read(Some(cursor), 0);
            assert!(page.gap);
            assert_eq!(page.records.len(), 1);
            assert!(
                !serde_json::to_string(&page)
                    .unwrap()
                    .contains("invented-private-secret")
            );
        }
    }

    #[test]
    fn malformed_unsupported_and_oversize_lines_are_consumed_without_exporting_text() {
        let journal = Journal::new();
        let unsupported = b"{\"version\":2,\"unix_ms\":1,\"pid\":42,\"event\":\"phase\",\"phase\":\"starting\"}\n";
        let mut exact = ONE[..ONE.len() - 1].to_vec();
        exact.resize(1023, b' ');
        exact.push(b'\n');
        let mut oversize = exact.clone();
        oversize.insert(0, b' ');
        journal.put(
            "journal",
            &[
                b"invented-private-secret\n".as_slice(),
                unsupported,
                b"\xff\n",
                &oversize,
                &exact,
                TWO,
            ]
            .concat(),
        );
        let page = journal.read(None, 1);
        assert_eq!(page.rejected, 4);
        assert_eq!(page.records.len(), 1);
        assert!(page.more);
        let page = journal.read(page.next_cursor.as_deref(), 1);
        assert_eq!(page.rejected, 0);
        assert_eq!(page.records[0].unix_ms, 2);
        assert!(!page.more);
    }

    #[test]
    fn current_partial_tails_wait_without_consumption_and_backup_tails_are_rejected() {
        let journal = Journal::new();
        journal.put("journal.1", b"partial backup");
        journal.put("journal", &[ONE, &TWO[..TWO.len() - 1]].concat());
        let page = journal.read(None, 0);
        assert_eq!(page.rejected, 1);
        assert_eq!(page.records.len(), 1);
        assert!(page.incomplete_tail);
        assert!(!page.more);
        let waiting = journal.read(page.next_cursor.as_deref(), 0);
        assert!(!waiting.gap);
        assert!(waiting.records.is_empty());
        assert_eq!(waiting.next_cursor, page.next_cursor);
        assert!(waiting.incomplete_tail);
        assert!(!waiting.more);
        journal.put("journal", &[ONE, TWO].concat());
        let complete = journal.read(waiting.next_cursor.as_deref(), 0);
        assert!(!complete.gap);
        assert_eq!(complete.records[0].unix_ms, 2);
        assert!(!complete.incomplete_tail);

        journal.put("journal", &TWO[..TWO.len() - 1]);
        fs::remove_file(journal.0.join("journal.1")).unwrap();
        let initial = journal.read(None, 0);
        assert!(initial.next_cursor.is_some());
        journal.put("journal", TWO);
        let completed = journal.read(initial.next_cursor.as_deref(), 0);
        assert!(!completed.gap);
        assert_eq!(completed.records.len(), 1);
    }

    #[test]
    fn limits_are_defaulted_and_capped_and_empty_journals_do_not_invent_records() {
        let journal = Journal::new();
        journal.put("journal", b"");
        let empty = journal.read(None, 0);
        assert!(empty.next_cursor.is_none());
        assert!(empty.records.is_empty());
        assert!(!empty.more);
        journal.put("journal", &ONE.repeat(300));
        assert_eq!(journal.read(None, 0).records.len(), 64);
        assert_eq!(journal.read(None, u32::MAX).records.len(), 256);
        assert!(journal.read(None, u32::MAX).more);
    }

    #[test]
    fn two_full_generations_are_bounded_and_invalid_lines_still_make_progress() {
        let journal = Journal::new();
        let mut bytes = vec![b'x'; MAX_FILE_BYTES as usize];
        *bytes.last_mut().unwrap() = b'\n';
        journal.put("journal.1", &bytes);
        journal.put("journal", &bytes);
        let page = journal.read(None, 1);
        assert_eq!(page.rejected, 2);
        assert!(page.records.is_empty());
        assert!(!page.more);
        let next = journal.read(page.next_cursor.as_deref(), 1);
        assert_eq!(next.rejected, 0);
        assert!(!next.gap);
    }

    #[test]
    fn a_symlinked_parent_is_not_followed_or_repaired() {
        let journal = Journal::new();
        journal.put("journal", ONE);
        let outer = Journal::new();
        let link = outer.0.join("linked");
        symlink(&journal.0, &link).unwrap();
        assert!(read(&link.join("journal"), None, 0).is_err());
        assert_eq!(fs::read(journal.path()).unwrap(), ONE);
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }

    #[test]
    fn reader_snapshot_release_does_not_wait_for_retained_descriptors() {
        use super::super::tests::{RETAIN_LOCK, RETAINED_LOCK};
        for unsafe_leaf in [false, true] {
            let journal = Journal::new();
            if unsafe_leaf {
                std::os::unix::fs::symlink(journal.0.join("missing"), journal.path()).unwrap();
            } else {
                journal.put("journal", ONE);
            }
            RETAIN_LOCK.set(true);
            let result = read(&journal.path(), None, 0);
            if unsafe_leaf {
                assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ELOOP));
            } else {
                assert_eq!(result.unwrap().records.len(), 1);
            }
            let retained = RETAINED_LOCK.with_borrow_mut(Option::take).unwrap();
            let contender = File::open(journal.0.join("journal.lock")).unwrap();
            contender
                .try_lock()
                .expect("snapshot release must not depend on retained shared descriptors");
            contender.unlock().unwrap();
            drop(retained);
        }
    }

    #[test]
    fn missing_files_are_not_created_and_contention_is_nonblocking() {
        let journal = Journal::new();
        assert_eq!(
            read(&journal.path(), None, 0).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(!journal.path().exists());
        fs::remove_file(journal.0.join("journal.lock")).unwrap();
        journal.put("journal", ONE);
        assert_eq!(
            read(&journal.path(), None, 0).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(!journal.0.join("journal.lock").exists());
        journal.put("journal.lock", b"");
        let lock = File::open(journal.0.join("journal.lock")).unwrap();
        lock.try_lock().unwrap();
        assert_eq!(
            read(&journal.path(), None, 0).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        lock.unlock().unwrap();
        drop(lock);
        assert_eq!(journal.read(None, 0).records.len(), 1);
    }

    #[test]
    fn unsafe_leaves_and_modes_are_refused_without_repair_or_mutation() {
        for leaf in ["journal", "journal.1", "journal.lock"] {
            for kind in [
                "symlink",
                "hardlink",
                "fifo",
                "directory",
                "mode",
                "oversize",
            ] {
                let journal = Journal::new();
                journal.put("journal", ONE);
                journal.put("sentinel", b"invented sentinel");
                let target = journal.0.join(leaf);
                if target.exists() {
                    fs::remove_file(&target).unwrap();
                }
                match kind {
                    "symlink" => symlink(journal.0.join("sentinel"), &target).unwrap(),
                    "hardlink" => fs::hard_link(journal.0.join("sentinel"), &target).unwrap(),
                    "fifo" => {
                        let name = CString::new(target.as_os_str().as_bytes()).unwrap();
                        // SAFETY: a valid NUL-terminated throwaway test path.
                        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                    }
                    "directory" => fs::create_dir(&target).unwrap(),
                    "mode" => {
                        journal.put(leaf, ONE);
                        fs::set_permissions(&target, Permissions::from_mode(0o644)).unwrap();
                    }
                    _ => {
                        journal.put(leaf, b"");
                        OpenOptions::new()
                            .write(true)
                            .open(&target)
                            .unwrap()
                            .set_len(MAX_FILE_BYTES + 1)
                            .unwrap();
                    }
                }
                let before = fs::symlink_metadata(&target).unwrap();
                assert!(read(&journal.path(), None, 0).is_err(), "{leaf}: {kind}");
                let after = fs::symlink_metadata(&target).unwrap();
                assert_eq!(
                    (before.ino(), before.mode(), before.len(), before.nlink()),
                    (after.ino(), after.mode(), after.len(), after.nlink())
                );
                assert_eq!(
                    fs::read(journal.0.join("sentinel")).unwrap(),
                    b"invented sentinel"
                );
            }
        }
        let journal = Journal::new();
        journal.put("journal", b"partial");
        fs::set_permissions(&journal.0, Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            read(&journal.path(), None, 0).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::metadata(&journal.0).unwrap().mode() & 0o7777, 0o755);
        fs::set_permissions(&journal.0, Permissions::from_mode(0o700)).unwrap();
        assert!(journal.read(None, 0).incomplete_tail);
        assert_eq!(fs::read(journal.path()).unwrap(), b"partial");
    }
}
