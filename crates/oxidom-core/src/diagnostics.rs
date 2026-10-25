//! Bounded, private daemon lifecycle and attributed runtime diagnostics.
//!
//! Only closed enums and numeric/boolean fields can reach this sink. It neither
//! captures stdout/stderr nor logs its own errors; the caller must report a fixed,
//! safe warning through its existing terminal/journal adapter on failure.
//!
//! Each JSONL record is version 1, at most 1 KiB including its newline. The
//! current file and its `.1` backup are each capped at 2 MiB. An already oversized
//! file makes open/write fail with `InvalidData`, without truncating or deleting
//! either generation: an operator must move it aside before retrying.
//!
//! A fresh descriptor takes the stable `.lock` with `try_lock` on every operation;
//! it is never unlinked, including during rotation. Contention returns
//! `WouldBlock`, even between threads sharing one sink. This is synchronous I/O
//! for small lifecycle records, not a guarantee against blocking filesystems or
//! slow disks. Successful writes sync data and directory changes. Rotation is
//! not a cross-file atomic transaction or a power-loss transaction. Failed
//! appends are truncated back where possible; an unrepairable partial tail is
//! repaired to the last newline before a later append.
//!
//! The owned, private directory is pinned once; all leaf operations are relative
//! to that descriptor. This prevents later parent-path replacement from
//! redirecting writes. As with other private state, this does not protect against
//! a hostile process with the same effective uid changing entries concurrently.

use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions, Permissions, TryLockError};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ipc::ProbeDetail;

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Signals,
    Store,
    Recovery,
    CoreResolution,
    DefaultProfile,
    Bus,
    Serving,
    Shutdown,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    StoreBusy,
    PermissionDenied,
    NotFound,
    AlreadyExists,
    InvalidInput,
    InvalidData,
    StorageFull,
    ReadOnlyFilesystem,
    WriteZero,
    Interrupted,
    OtherIo,
    Bus,
    Other,
}

/// Diagnostic-only process-local correlation, never a PID, health counter,
/// profile/store identity, or a value to persist back into application state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub run: u64,
    pub session: u64,
    pub generation: u64,
    pub attempt: Option<u64>,
    pub core: Option<u64>,
}

/// One bounded entropy request per recorder; failure (including zero) disables
/// run correlation rather than falling back to a machine or store identity.
pub fn generate_run_id() -> Option<u64> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0; 8];
    SystemRandom::new().fill(&mut bytes).ok()?;
    let id = u64::from_le_bytes(bytes);
    (id != 0).then_some(id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Explicit,
    Automatic,
    Settings,
    Adopted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Server,
    Pool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Accepted,
    DiscardedStale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationOutcome {
    Reachable,
    Timeout,
    Unreachable,
    NoNetwork,
    Internal,
    InboundNotReady,
    PoolWindowExhausted,
    InterfaceFailed,
    StartFailed,
    MissingSubject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryState {
    Pending,
    Cancelled,
    Recovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticWatchMode {
    Off,
    Conservative,
    Aggressive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchAction {
    Observed,
    Reset,
    DiscardedStale,
    ConservativeRetained,
    RecoveryApplied,
    RecoveryRejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlushReason {
    Batch,
    ContextEnded,
    CauseChanged,
    Capacity,
    Shutdown,
}

/// The original, exact observation key. Recorder-produced summaries always have
/// `action: Observed`; actions and stale results are never aggregated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchObservation {
    pub context: Context,
    pub policy_epoch: u64,
    pub mode: DiagnosticWatchMode,
    pub outcome: ObservationOutcome,
    pub detail: Option<ProbeDetail>,
    pub action: WatchAction,
}

/// No raw error messages, paths, profile names, or other free-form text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Phase {
        phase: Phase,
    },
    Problem {
        phase: Phase,
        kind: FailureKind,
        os_code: Option<i32>,
        fatal: bool,
    },
    CatalogLoadWarnings {
        count: u32,
    },
    SessionOpened {
        context: Context,
    },
    SessionRetired {
        context: Context,
    },
    AttemptStarted {
        context: Context,
        origin: Origin,
        target: TargetKind,
    },
    CoreStarted {
        context: Context,
        adopted: bool,
    },
    AttemptFinished {
        context: Context,
        outcome: ObservationOutcome,
        disposition: Disposition,
        failure: Option<FailureKind>,
        os_code: Option<i32>,
        detail: Option<ProbeDetail>,
    },
    LivenessLost {
        context: Context,
        holding_traffic: bool,
    },
    Retry {
        context: Context,
        state: RetryState,
        ordinal: u32,
        delay_ms: u64,
    },
    Watch {
        context: Context,
        policy_epoch: u64,
        mode: DiagnosticWatchMode,
        outcome: ObservationOutcome,
        detail: Option<ProbeDetail>,
        action: WatchAction,
    },
    WatchSummary {
        key: WatchObservation,
        /// Additional occurrences only, excluding the first visible observation.
        suppressed_count: u64,
        /// Recorder acceptance ordinals of this delta's suppressed occurrences.
        first_ordinal: u64,
        last_ordinal: u64,
        reason: FlushReason,
    },
    /// A failed sync can follow a visible write. These are unconfirmed writes,
    /// not a claim that the corresponding occurrences were definitely lost.
    PersistenceGap {
        unconfirmed_write_attempts: u64,
        unconfirmed_occurrences: u64,
        last_kind: FailureKind,
        last_os_code: Option<i32>,
        saturated: bool,
    },
}

#[derive(Serialize)]
struct Record {
    version: u8,
    unix_ms: u64,
    pid: u32,
    #[serde(flatten)]
    event: Event,
}

fn encode(event: Event, unix_ms: u64, pid: u32) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(&Record {
        version: 1,
        unix_ms,
        pid,
        event,
    })?;
    bytes.push(b'\n');
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "diagnostic record exceeds the byte limit",
        ));
    }
    Ok(bytes)
}

/// A pinned private directory, not a retained log or lock descriptor.
pub struct Diagnostics {
    directory: File,
    current: CString,
    backup: CString,
    lock: CString,
}

impl Diagnostics {
    /// Create/validate the directory and both generations without recording an
    /// event. This diagnostic lock is independent of the daemon's store lock;
    /// callers can open it first and record a failed attempt to acquire the store.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic path has no file name",
            )
        })?;
        let name = name.as_bytes();
        let leaf = |suffix: &[u8]| {
            let mut bytes = name.to_vec();
            bytes.extend_from_slice(suffix);
            CString::new(bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "diagnostic path contains NUL")
            })
        };
        let current = leaf(b"")?;
        let backup = leaf(b".1")?;
        let lock = leaf(b".lock")?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        crate::fsutil::create_private_dir(parent).map_err(|error| {
            error.downcast::<io::Error>().unwrap_or_else(|_| {
                io::Error::other("creating private diagnostic directory failed")
            })
        })?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(parent)?;
        let metadata = directory.metadata()?;
        // SAFETY: geteuid has no preconditions or pointer arguments.
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "diagnostic directory is not owned by the effective uid",
            ));
        }
        private_mode(&directory, 0o700)?;
        let sink = Self {
            directory,
            current,
            backup,
            lock,
        };
        sink.access(None)?;
        Ok(sink)
    }

    /// Append one structured record with the current Unix milliseconds and PID.
    /// No logging occurs here, including on failure.
    pub fn write(&self, event: Event) -> io::Result<()> {
        self.append(&encode(
            event,
            crate::ipc::now_unix_ms(),
            std::process::id(),
        )?)
    }

    // Private byte seam: there is deliberately no public arbitrary-text writer.
    fn append(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty()
            || bytes.len() > MAX_RECORD_BYTES
            || bytes.last() != Some(&b'\n')
            || bytes[..bytes.len() - 1].contains(&b'\n')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic record must be one bounded newline-terminated record",
            ));
        }
        self.access(Some(bytes))
    }

    fn access(&self, bytes: Option<&[u8]>) -> io::Result<()> {
        let lock = self
            .open_leaf(&self.lock, true)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "diagnostic lock is missing"))?;
        lock.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                "diagnostic writer is already active",
            ),
            TryLockError::Error(error) => error,
        })?;

        // Validate both generations before chmod, tail repair, or rotation.
        // In particular, an unsafe/oversized backup is not an expendable target.
        let current = self.open_leaf(&self.current, false)?;
        let backup = self.open_leaf(&self.backup, false)?;
        for file in [&current, &backup].into_iter().flatten() {
            if file.metadata()?.len() > MAX_FILE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "existing diagnostic generation exceeds the byte limit",
                ));
            }
        }
        private_mode(&lock, 0o600)?;
        if let Some(backup) = backup {
            private_mode(&backup, 0o600)?;
        }
        let mut current = match current {
            Some(file) => file,
            None => self.create_current()?,
        };
        private_mode(&current, 0o600)?;
        let mut length = repair_tail(&current)?;
        let Some(bytes) = bytes else {
            return Ok(());
        };
        if length + bytes.len() as u64 > MAX_FILE_BYTES {
            #[cfg(test)]
            tests::fail_at(tests::Stage::Rename)?;
            // SAFETY: the owned directory fd remains live, and both leaf names
            // are valid C strings. No path lookup is relative to the process cwd.
            if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    self.current.as_ptr(),
                    self.directory.as_raw_fd(),
                    self.backup.as_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            // If sync or creation fails, do not append to the now-backup fd.
            self.sync_directory()?;
            current = self.create_current()?;
            length = 0;
        }

        let result = (|| {
            let mut remaining = bytes;
            while !remaining.is_empty() {
                #[cfg(test)]
                let written = tests::write(&mut current, remaining);
                #[cfg(not(test))]
                let written = current.write(remaining);
                match written {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(count) => remaining = &remaining[count..],
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            sync_data(&current)?;
            // Also persist entries left by an earlier failed creation/sync.
            self.sync_directory()
        })();
        // Best effort only: ENOSPC/EIO can also prevent rollback or its sync.
        // The next writer still repairs an incomplete tail under the lock.
        if result.is_err() && current.set_len(length).is_ok() {
            let _ = sync_data(&current);
        }
        result
    }

    fn create_current(&self) -> io::Result<File> {
        #[cfg(test)]
        tests::fail_at(tests::Stage::CreateCurrent)?;
        // O_EXCL prevents truncating any leaf introduced since validation.
        let file = self.open_at(&self.current, libc::O_CREAT | libc::O_EXCL)?;
        private_mode(&file, 0o600)?;
        self.sync_directory()?;
        Ok(file)
    }

    fn open_leaf(&self, name: &CStr, create: bool) -> io::Result<Option<File>> {
        match self.open_at(name, 0) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                let file = match self.open_at(name, libc::O_CREAT | libc::O_EXCL) {
                    Ok(file) => file,
                    // Another starter may have created the stable lock meanwhile.
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        return self.open_at(name, 0).map(Some);
                    }
                    Err(error) => return Err(error),
                };
                sync_data(&file)?;
                self.sync_directory()?;
                Ok(Some(file))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn open_at(&self, name: &CStr, flags: i32) -> io::Result<File> {
        // O_NONBLOCK is essential before fstat: opening an injected FIFO must
        // not wait for a peer. No truncation or chmod occurs before validation.
        // SAFETY: directory and name remain valid throughout openat. A successful
        // fd is transferred exactly once to File, including on validation errors.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags
                    | libc::O_RDWR
                    | libc::O_APPEND
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK,
                // C varargs require promotion even when mode_t is only 16 bits.
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat just returned this uniquely owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no preconditions or pointer arguments.
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "diagnostic leaf must be an owned regular file with one link",
            ));
        }
        Ok(file)
    }

    fn sync_directory(&self) -> io::Result<()> {
        #[cfg(test)]
        tests::fail_at(tests::Stage::DirectorySync)?;
        self.directory.sync_all()
    }
}

fn private_mode(file: &File, mode: u32) -> io::Result<()> {
    if file.metadata()?.mode() & 0o7777 != mode {
        file.set_permissions(Permissions::from_mode(mode))?;
    }
    // A previous chmod may have succeeded while its sync failed. Visible mode
    // alone cannot establish durability, including for the lock and backup.
    #[cfg(test)]
    tests::before_metadata_sync(file)?;
    file.sync_all()
}

fn sync_data(file: &File) -> io::Result<()> {
    #[cfg(test)]
    tests::fail_at(tests::Stage::FileSync)?;
    file.sync_data()
}

fn repair_tail(file: &File) -> io::Result<u64> {
    let length = file.metadata()?.len();
    if length > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "existing diagnostic generation exceeds the byte limit",
        ));
    }
    let mut end = length;
    let mut buffer = [0; 4096];
    // At most MAX_FILE_BYTES are inspected; each generation was size-checked
    // before entry. Usually only the last block is needed.
    let repaired = loop {
        if end == 0 {
            break 0;
        }
        let start = end.saturating_sub(buffer.len() as u64);
        let bytes = &mut buffer[..(end - start) as usize];
        file.read_exact_at(bytes, start)?;
        if let Some(index) = bytes.iter().rposition(|&byte| byte == b'\n') {
            break start + index as u64 + 1;
        }
        end = start;
    };
    if repaired != length {
        file.set_len(repaired)?;
        sync_data(file)?;
    }
    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn runtime_records_have_literal_private_version_one_bytes() {
        let context = Context {
            run: 1,
            session: 2,
            generation: 3,
            attempt: Some(4),
            core: Some(5),
        };
        let key = WatchObservation {
            context,
            policy_epoch: 6,
            mode: DiagnosticWatchMode::Conservative,
            outcome: ObservationOutcome::Timeout,
            detail: Some(crate::ipc::ProbeDetail::NoCore),
            action: WatchAction::Observed,
        };
        for (event, literal) in [
            (
                Event::SessionOpened { context },
                r#"{"event":"session_opened","context":{"run":1,"session":2,"generation":3,"attempt":4,"core":5}}"#,
            ),
            (
                Event::SessionRetired { context },
                r#"{"event":"session_retired","context":{"run":1,"session":2,"generation":3,"attempt":4,"core":5}}"#,
            ),
            (
                Event::AttemptStarted {
                    context,
                    origin: Origin::Explicit,
                    target: TargetKind::Pool,
                },
                r#"{"event":"attempt_started","context":{"run":1,"session":2,"generation":3,"attempt":4,"core":5},"origin":"explicit","target":"pool"}"#,
            ),
            (
                Event::CoreStarted {
                    context,
                    adopted: true,
                },
                r#"{"event":"core_started","context":{"run":1,"session":2,"generation":3,"attempt":4,"core":5},"adopted":true}"#,
            ),
            (
                Event::AttemptFinished {
                    context,
                    outcome: ObservationOutcome::StartFailed,
                    disposition: Disposition::DiscardedStale,
                    failure: Some(FailureKind::OtherIo),
                    os_code: Some(-1),
                    detail: None,
                },
                r#"{"event":"attempt_finished","context":{"run":1,"session":2,"generation":3,"attempt":4,"core":5},"outcome":"start_failed","disposition":"discarded_stale","failure":"other_io","os_code":-1,"detail":null}"#,
            ),
            (
                Event::LivenessLost {
                    context,
                    holding_traffic: true,
                },
                r#"{"event":"liveness_lost","context":{"run":1,"session":2,"generation":3,"attempt":4,"core":5},"holding_traffic":true}"#,
            ),
            (
                Event::Retry {
                    context,
                    state: RetryState::Pending,
                    ordinal: 7,
                    delay_ms: 8,
                },
                r#"{"event":"retry","context":{"run":1,"session":2,"generation":3,"attempt":4,"core":5},"state":"pending","ordinal":7,"delay_ms":8}"#,
            ),
            (
                Event::Watch {
                    context,
                    policy_epoch: 6,
                    mode: key.mode,
                    outcome: key.outcome,
                    detail: key.detail,
                    action: key.action,
                },
                r#"{"event":"watch","context":{"run":1,"session":2,"generation":3,"attempt":4,"core":5},"policy_epoch":6,"mode":"conservative","outcome":"timeout","detail":"no_core","action":"observed"}"#,
            ),
            (
                Event::WatchSummary {
                    key,
                    suppressed_count: 16,
                    first_ordinal: 2,
                    last_ordinal: 17,
                    reason: FlushReason::Batch,
                },
                r#"{"event":"watch_summary","key":{"context":{"run":1,"session":2,"generation":3,"attempt":4,"core":5},"policy_epoch":6,"mode":"conservative","outcome":"timeout","detail":"no_core","action":"observed"},"suppressed_count":16,"first_ordinal":2,"last_ordinal":17,"reason":"batch"}"#,
            ),
            (
                Event::PersistenceGap {
                    unconfirmed_write_attempts: 2,
                    unconfirmed_occurrences: 17,
                    last_kind: FailureKind::OtherIo,
                    last_os_code: Some(5),
                    saturated: false,
                },
                r#"{"event":"persistence_gap","unconfirmed_write_attempts":2,"unconfirmed_occurrences":17,"last_kind":"other_io","last_os_code":5,"saturated":false}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&event).unwrap(), literal);
            let expected = format!(
                "{{\"version\":1,\"unix_ms\":0,\"pid\":1,{}\n",
                &literal[1..]
            );
            assert_eq!(encode(event, 0, 1).unwrap(), expected.as_bytes());
            assert_eq!(serde_json::from_str::<Event>(literal).unwrap(), event);
        }
    }

    #[test]
    fn maximal_runtime_records_stay_below_one_kib_including_the_newline() {
        let context = Context {
            run: u64::MAX,
            session: u64::MAX,
            generation: u64::MAX,
            attempt: Some(u64::MAX),
            core: Some(u64::MAX),
        };
        for event in [
            Event::SessionOpened { context },
            Event::SessionRetired { context },
            Event::AttemptStarted {
                context,
                origin: Origin::Automatic,
                target: TargetKind::Server,
            },
            Event::CoreStarted {
                context,
                adopted: false,
            },
            Event::AttemptFinished {
                context,
                outcome: ObservationOutcome::PoolWindowExhausted,
                disposition: Disposition::Accepted,
                failure: Some(FailureKind::ReadOnlyFilesystem),
                os_code: Some(i32::MIN),
                detail: Some(crate::ipc::ProbeDetail::InsecureTlsUnsupported),
            },
            Event::LivenessLost {
                context,
                holding_traffic: false,
            },
            Event::Retry {
                context,
                state: RetryState::Recovered,
                ordinal: u32::MAX,
                delay_ms: u64::MAX,
            },
            Event::Watch {
                context,
                policy_epoch: u64::MAX,
                mode: DiagnosticWatchMode::Aggressive,
                outcome: ObservationOutcome::PoolWindowExhausted,
                detail: Some(crate::ipc::ProbeDetail::InsecureTlsUnsupported),
                action: WatchAction::ConservativeRetained,
            },
            Event::WatchSummary {
                key: WatchObservation {
                    context,
                    policy_epoch: u64::MAX,
                    mode: DiagnosticWatchMode::Conservative,
                    outcome: ObservationOutcome::PoolWindowExhausted,
                    detail: Some(crate::ipc::ProbeDetail::InsecureTlsUnsupported),
                    action: WatchAction::Observed,
                },
                suppressed_count: u64::MAX,
                first_ordinal: u64::MAX,
                last_ordinal: u64::MAX,
                reason: FlushReason::ContextEnded,
            },
            Event::PersistenceGap {
                unconfirmed_write_attempts: u64::MAX,
                unconfirmed_occurrences: u64::MAX,
                last_kind: FailureKind::ReadOnlyFilesystem,
                last_os_code: Some(i32::MIN),
                saturated: true,
            },
        ] {
            assert!(encode(event, u64::MAX, u32::MAX).unwrap().len() <= 1024);
        }
    }

    #[test]
    fn maximum_summary_and_gap_fields_have_literal_bytes_without_truncation() {
        let context = Context {
            run: u64::MAX,
            session: u64::MAX,
            generation: u64::MAX,
            attempt: Some(u64::MAX),
            core: Some(u64::MAX),
        };
        assert_eq!(
            encode(Event::WatchSummary { key: WatchObservation { context, policy_epoch: u64::MAX, mode: DiagnosticWatchMode::Conservative, outcome: ObservationOutcome::PoolWindowExhausted, detail: Some(crate::ipc::ProbeDetail::InsecureTlsUnsupported), action: WatchAction::Observed }, suppressed_count: u64::MAX, first_ordinal: u64::MAX, last_ordinal: u64::MAX, reason: FlushReason::ContextEnded }, u64::MAX, u32::MAX).unwrap(),
            b"{\"version\":1,\"unix_ms\":18446744073709551615,\"pid\":4294967295,\"event\":\"watch_summary\",\"key\":{\"context\":{\"run\":18446744073709551615,\"session\":18446744073709551615,\"generation\":18446744073709551615,\"attempt\":18446744073709551615,\"core\":18446744073709551615},\"policy_epoch\":18446744073709551615,\"mode\":\"conservative\",\"outcome\":\"pool_window_exhausted\",\"detail\":\"insecure_tls_unsupported\",\"action\":\"observed\"},\"suppressed_count\":18446744073709551615,\"first_ordinal\":18446744073709551615,\"last_ordinal\":18446744073709551615,\"reason\":\"context_ended\"}\n"
        );
        assert_eq!(
            encode(Event::PersistenceGap { unconfirmed_write_attempts: u64::MAX, unconfirmed_occurrences: u64::MAX, last_kind: FailureKind::ReadOnlyFilesystem, last_os_code: Some(i32::MIN), saturated: true }, u64::MAX, u32::MAX).unwrap(),
            b"{\"version\":1,\"unix_ms\":18446744073709551615,\"pid\":4294967295,\"event\":\"persistence_gap\",\"unconfirmed_write_attempts\":18446744073709551615,\"unconfirmed_occurrences\":18446744073709551615,\"last_kind\":\"read_only_filesystem\",\"last_os_code\":-2147483648,\"saturated\":true}\n"
        );
        assert_eq!(
            serde_json::to_string(&Context {
                run: 1,
                session: 2,
                generation: 3,
                attempt: None,
                core: None
            })
            .unwrap(),
            r#"{"run":1,"session":2,"generation":3,"attempt":null,"core":null}"#
        );
    }

    #[test]
    fn runtime_enum_spellings_are_closed_and_fixed() {
        macro_rules! fixed {
            ($ty:ty, $($variant:path => $literal:literal),+ $(,)?) => {{
                $(
                    let literal = concat!("\"", $literal, "\"");
                    assert_eq!(serde_json::to_string(&$variant).unwrap(), literal);
                    assert_eq!(serde_json::from_str::<$ty>(literal).unwrap(), $variant);
                )+
                assert!(serde_json::from_str::<$ty>(r#""invented-private-name""#).is_err());
            }};
        }
        fixed!(Origin, Origin::Explicit => "explicit", Origin::Automatic => "automatic", Origin::Settings => "settings", Origin::Adopted => "adopted");
        fixed!(TargetKind, TargetKind::Server => "server", TargetKind::Pool => "pool");
        fixed!(Disposition, Disposition::Accepted => "accepted", Disposition::DiscardedStale => "discarded_stale");
        fixed!(ObservationOutcome,
            ObservationOutcome::Reachable => "reachable", ObservationOutcome::Timeout => "timeout",
            ObservationOutcome::Unreachable => "unreachable", ObservationOutcome::NoNetwork => "no_network",
            ObservationOutcome::Internal => "internal", ObservationOutcome::InboundNotReady => "inbound_not_ready",
            ObservationOutcome::PoolWindowExhausted => "pool_window_exhausted", ObservationOutcome::InterfaceFailed => "interface_failed",
            ObservationOutcome::StartFailed => "start_failed", ObservationOutcome::MissingSubject => "missing_subject",
        );
        fixed!(RetryState, RetryState::Pending => "pending", RetryState::Cancelled => "cancelled", RetryState::Recovered => "recovered");
        fixed!(DiagnosticWatchMode, DiagnosticWatchMode::Off => "off", DiagnosticWatchMode::Conservative => "conservative", DiagnosticWatchMode::Aggressive => "aggressive");
        fixed!(WatchAction,
            WatchAction::Observed => "observed", WatchAction::Reset => "reset", WatchAction::DiscardedStale => "discarded_stale",
            WatchAction::ConservativeRetained => "conservative_retained", WatchAction::RecoveryApplied => "recovery_applied", WatchAction::RecoveryRejected => "recovery_rejected",
        );
        fixed!(FlushReason, FlushReason::Batch => "batch", FlushReason::ContextEnded => "context_ended", FlushReason::CauseChanged => "cause_changed", FlushReason::Capacity => "capacity", FlushReason::Shutdown => "shutdown");
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Stage {
        Write,
        FileSync,
        DirectorySync,
        Rename,
        CreateCurrent,
    }

    #[derive(Clone, Copy)]
    enum Fault {
        Error(Stage, i32),
        PartialWrite(i32),
        ShortWrite,
        WriteZero,
        MetadataSync(u64),
    }

    thread_local! {
        static FAULT: Cell<Option<Fault>> = const { Cell::new(None) };
    }

    struct FaultGuard;

    impl FaultGuard {
        fn install(fault: Fault) -> Self {
            FAULT.set(Some(fault));
            Self
        }
    }

    impl Drop for FaultGuard {
        fn drop(&mut self) {
            FAULT.set(None);
        }
    }

    pub(super) fn fail_at(stage: Stage) -> io::Result<()> {
        if let Some(Fault::Error(expected, code)) = FAULT.get()
            && stage == expected
        {
            FAULT.set(None);
            return Err(io::Error::from_raw_os_error(code));
        }
        Ok(())
    }

    pub(super) fn before_metadata_sync(file: &File) -> io::Result<()> {
        if let Some(Fault::MetadataSync(inode)) = FAULT.get()
            && file.metadata()?.ino() == inode
        {
            FAULT.set(None);
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(())
    }

    pub(super) fn write(file: &mut File, bytes: &[u8]) -> io::Result<usize> {
        fail_at(Stage::Write)?;
        match FAULT.get() {
            Some(Fault::PartialWrite(code)) => {
                FAULT.set(Some(Fault::Error(Stage::Write, code)));
                file.write(&bytes[..bytes.len().min(7)])
            }
            Some(Fault::ShortWrite) => {
                FAULT.set(None);
                file.write(&bytes[..bytes.len().min(7)])
            }
            Some(Fault::WriteZero) => {
                FAULT.set(None);
                Ok(0)
            }
            _ => file.write(bytes),
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "oxidom-diagnostics-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("creating test directory: {error}"),
                }
            }
        }

        fn path(&self) -> PathBuf {
            self.0.join("oxidom-daemon.log")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn event() -> Event {
        Event::Phase {
            phase: Phase::Starting,
        }
    }

    fn record() -> Vec<u8> {
        encode(event(), 1234, 42).unwrap()
    }

    fn full_file(byte: u8) -> Vec<u8> {
        let mut bytes = vec![byte; MAX_FILE_BYTES as usize];
        *bytes.last_mut().unwrap() = b'\n';
        bytes
    }

    fn mode(path: &std::path::Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    #[test]
    fn the_default_daemon_path_uses_the_redirected_data_root_not_the_gui_log() {
        struct TestRoot {
            directory: TestDir,
        }
        impl Drop for TestRoot {
            fn drop(&mut self) {
                crate::paths::set_test_root(None);
            }
        }
        let _guard = crate::sync::lock(&crate::paths::TEST_ROOT_LOCK);
        let root = TestRoot {
            directory: TestDir::new(),
        };
        crate::paths::set_test_root(Some(root.directory.0.clone()));
        assert_eq!(
            crate::paths::daemon_log_file().unwrap(),
            root.directory.path()
        );
        assert_eq!(
            crate::paths::gui_log_file().unwrap(),
            root.directory.0.join("oxidom-gui.log")
        );
    }

    #[test]
    fn records_have_literal_versioned_bytes_and_only_closed_structured_fields() {
        assert_eq!(
            record(),
            b"{\"version\":1,\"unix_ms\":1234,\"pid\":42,\"event\":\"phase\",\"phase\":\"starting\"}\n"
        );
        assert_eq!(
            encode(Event::Problem {
                phase: Phase::Store,
                kind: FailureKind::StoreBusy,
                os_code: Some(11),
                fatal: true,
            }, 0, 1).unwrap(),
            b"{\"version\":1,\"unix_ms\":0,\"pid\":1,\"event\":\"problem\",\"phase\":\"store\",\"kind\":\"store_busy\",\"os_code\":11,\"fatal\":true}\n"
        );
        assert_eq!(
            encode(Event::Problem {
                phase: Phase::Bus,
                kind: FailureKind::Bus,
                os_code: None,
                fatal: false,
            }, 0, 1).unwrap(),
            b"{\"version\":1,\"unix_ms\":0,\"pid\":1,\"event\":\"problem\",\"phase\":\"bus\",\"kind\":\"bus\",\"os_code\":null,\"fatal\":false}\n"
        );
        assert_eq!(
            encode(Event::CatalogLoadWarnings { count: 3 }, 0, 1).unwrap(),
            b"{\"version\":1,\"unix_ms\":0,\"pid\":1,\"event\":\"catalog_load_warnings\",\"count\":3}\n"
        );
        assert!(serde_json::from_str::<Event>(r#"{"event":"arbitrary text"}"#).is_err());
        assert!(serde_json::from_str::<Phase>(r#""arbitrary text""#).is_err());
        assert!(serde_json::from_str::<FailureKind>(r#""arbitrary text""#).is_err());
    }

    #[test]
    fn all_phase_and_failure_spellings_are_fixed_and_round_trip() {
        for (phase, literal) in [
            (Phase::Starting, "starting"),
            (Phase::Signals, "signals"),
            (Phase::Store, "store"),
            (Phase::Recovery, "recovery"),
            (Phase::CoreResolution, "core_resolution"),
            (Phase::DefaultProfile, "default_profile"),
            (Phase::Bus, "bus"),
            (Phase::Serving, "serving"),
            (Phase::Shutdown, "shutdown"),
            (Phase::Stopped, "stopped"),
        ] {
            let bytes = format!("\"{literal}\"");
            assert_eq!(serde_json::to_string(&phase).unwrap(), bytes);
            assert_eq!(serde_json::from_str::<Phase>(&bytes).unwrap(), phase);
        }
        for (kind, literal) in [
            (FailureKind::StoreBusy, "store_busy"),
            (FailureKind::PermissionDenied, "permission_denied"),
            (FailureKind::NotFound, "not_found"),
            (FailureKind::AlreadyExists, "already_exists"),
            (FailureKind::InvalidInput, "invalid_input"),
            (FailureKind::InvalidData, "invalid_data"),
            (FailureKind::StorageFull, "storage_full"),
            (FailureKind::ReadOnlyFilesystem, "read_only_filesystem"),
            (FailureKind::WriteZero, "write_zero"),
            (FailureKind::Interrupted, "interrupted"),
            (FailureKind::OtherIo, "other_io"),
            (FailureKind::Bus, "bus"),
            (FailureKind::Other, "other"),
        ] {
            let bytes = format!("\"{literal}\"");
            assert_eq!(serde_json::to_string(&kind).unwrap(), bytes);
            assert_eq!(serde_json::from_str::<FailureKind>(&bytes).unwrap(), kind);
        }
        for event in [
            event(),
            Event::CatalogLoadWarnings { count: u32::MAX },
            Event::Problem {
                phase: Phase::Stopped,
                kind: FailureKind::Other,
                os_code: Some(i32::MIN),
                fatal: true,
            },
        ] {
            let bytes = encode(event, u64::MAX, u32::MAX).unwrap();
            assert!(bytes.len() <= MAX_RECORD_BYTES);
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(serde_json::from_value::<Event>(value).unwrap(), event);
        }
    }

    #[test]
    fn nested_directories_and_existing_owned_modes_are_private() {
        let dir = TestDir::new();
        let parent = dir.0.join("nested/private");
        let path = parent.join("oxidom-daemon.log");
        let sink = Diagnostics::open(path.clone()).unwrap();
        sink.write(event()).unwrap();
        assert_eq!(mode(&dir.0.join("nested")), 0o700);
        assert_eq!(mode(&parent), 0o700);
        for leaf in [
            "oxidom-daemon.log",
            "oxidom-daemon.log.1",
            "oxidom-daemon.log.lock",
        ] {
            let file = parent.join(leaf);
            if !file.exists() {
                fs::write(&file, b"backup\n").unwrap();
            }
            fs::set_permissions(file, fs::Permissions::from_mode(0o666)).unwrap();
        }
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        Diagnostics::open(path).unwrap().write(event()).unwrap();
        assert_eq!(mode(&parent), 0o700);
        for leaf in [
            "oxidom-daemon.log",
            "oxidom-daemon.log.1",
            "oxidom-daemon.log.lock",
        ] {
            assert_eq!(mode(&parent.join(leaf)), 0o600);
        }
    }

    #[test]
    fn a_retry_syncs_owned_inodes_even_if_the_failed_chmod_sync_left_the_mode_correct() {
        for leaf in [
            "oxidom-daemon.log.lock",
            "oxidom-daemon.log.1",
            "oxidom-daemon.log",
        ] {
            let dir = TestDir::new();
            let sink = Diagnostics::open(dir.path()).unwrap();
            let path = dir.0.join(leaf);
            if !path.exists() {
                fs::write(&path, b"backup\n").unwrap();
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            let inode = fs::metadata(&path).unwrap().ino();
            for _ in 0..2 {
                let _fault = FaultGuard::install(Fault::MetadataSync(inode));
                assert_eq!(
                    sink.write(event()).unwrap_err().raw_os_error(),
                    Some(libc::EIO)
                );
                assert_eq!(mode(&path), 0o600);
            }
            sink.write(event()).unwrap();
        }
    }

    #[test]
    fn an_incomplete_tail_is_repaired_before_a_complete_record_is_appended() {
        for initial in [
            b"complete\npartial".as_slice(),
            b"partial",
            b"complete\n",
            b"",
        ] {
            let dir = TestDir::new();
            fs::write(dir.path(), initial).unwrap();
            let sink = Diagnostics::open(dir.path()).unwrap();
            sink.append(&record()).unwrap();
            let mut expected = if initial.starts_with(b"complete\n") {
                b"complete\n".to_vec()
            } else {
                Vec::new()
            };
            expected.extend(record());
            assert_eq!(fs::read(dir.path()).unwrap(), expected);
        }
    }

    #[test]
    fn tail_repair_is_bounded_even_when_no_newline_is_in_the_last_block() {
        let dir = TestDir::new();
        let mut bytes = b"complete\n".to_vec();
        bytes.resize(MAX_FILE_BYTES as usize, b'x');
        fs::write(dir.path(), bytes).unwrap();
        let sink = Diagnostics::open(dir.path()).unwrap();
        sink.append(&record()).unwrap();
        assert_eq!(
            fs::read(dir.path()).unwrap(),
            [b"complete\n".to_vec(), record()].concat()
        );
        fs::write(dir.path(), vec![b'x'; MAX_FILE_BYTES as usize]).unwrap();
        sink.append(&record()).unwrap();
        assert_eq!(fs::read(dir.path()).unwrap(), record());
    }

    #[test]
    fn exact_fit_stays_current_and_projected_overflow_rotates() {
        let dir = TestDir::new();
        let sink = Diagnostics::open(dir.path()).unwrap();
        let bytes = record();
        let mut prefix = vec![b'x'; MAX_FILE_BYTES as usize - bytes.len()];
        *prefix.last_mut().unwrap() = b'\n';
        fs::write(dir.path(), &prefix).unwrap();
        sink.append(&bytes).unwrap();
        prefix.extend(&bytes);
        assert_eq!(fs::read(dir.path()).unwrap(), prefix);
        assert!(!dir.0.join("oxidom-daemon.log.1").exists());
        sink.append(&bytes).unwrap();
        assert_eq!(fs::read(dir.0.join("oxidom-daemon.log.1")).unwrap(), prefix);
        assert_eq!(fs::read(dir.path()).unwrap(), bytes);
    }

    #[test]
    fn two_writers_at_the_size_boundary_keep_only_complete_bounded_generations() {
        let dir = TestDir::new();
        let sink = Diagnostics::open(dir.path()).unwrap();
        let bytes = record();
        let mut prefix = vec![b'x'; MAX_FILE_BYTES as usize - bytes.len()];
        *prefix.last_mut().unwrap() = b'\n';
        fs::write(dir.path(), &prefix).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let write = || {
                barrier.wait();
                sink.append(&bytes)
            };
            let first = scope.spawn(write);
            let second = scope.spawn(write);
            [first.join().unwrap(), second.join().unwrap()]
        });
        let successes = results.iter().filter(|result| result.is_ok()).count();
        for error in results.into_iter().filter_map(Result::err) {
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        }
        prefix.extend(&bytes);
        let backup = dir.0.join("oxidom-daemon.log.1");
        match successes {
            1 => {
                assert_eq!(fs::read(dir.path()).unwrap(), prefix);
                assert!(!backup.exists());
            }
            2 => {
                assert_eq!(fs::read(backup).unwrap(), prefix);
                assert_eq!(fs::read(dir.path()).unwrap(), bytes);
            }
            _ => panic!("at least one writer must succeed"),
        }
    }

    #[test]
    fn record_limits_count_bytes_including_the_newline_not_unicode_characters() {
        let dir = TestDir::new();
        let sink = Diagnostics::open(dir.path()).unwrap();
        let mut exact = vec![b'x'; MAX_RECORD_BYTES - 1];
        exact.push(b'\n');
        sink.append(&exact).unwrap();
        let oversized = format!("{}\n", "\u{00e9}".repeat(MAX_RECORD_BYTES / 2));
        assert!(oversized.chars().count() < MAX_RECORD_BYTES);
        assert_eq!(
            sink.append(oversized.as_bytes()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read(dir.path()).unwrap(), exact);
        for invalid in [b"".as_slice(), b"no newline", b"two\nlines\n"] {
            assert_eq!(
                sink.append(invalid).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn repeated_rotations_keep_only_two_bounded_private_generations() {
        let dir = TestDir::new();
        let sink = Diagnostics::open(dir.path()).unwrap();
        let lock_inode = fs::metadata(dir.0.join("oxidom-daemon.log.lock"))
            .unwrap()
            .ino();
        for byte in b'a'..=b'd' {
            let previous = full_file(byte);
            fs::write(dir.path(), &previous).unwrap();
            sink.append(&record()).unwrap();
            let backup = dir.0.join("oxidom-daemon.log.1");
            assert_eq!(fs::read(&backup).unwrap(), previous);
            assert_eq!(fs::read(dir.path()).unwrap(), record());
            assert_eq!(mode(&backup), 0o600);
            assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 3);
        }
        assert_eq!(
            fs::metadata(dir.0.join("oxidom-daemon.log.lock"))
                .unwrap()
                .ino(),
            lock_inode
        );
    }

    #[test]
    fn oversized_current_or_backup_is_refused_unchanged_even_after_restart() {
        for oversized_leaf in ["oxidom-daemon.log", "oxidom-daemon.log.1"] {
            let dir = TestDir::new();
            let sink = Diagnostics::open(dir.path()).unwrap();
            let mut oversized = full_file(b'x');
            oversized.push(b'x');
            let path = dir.0.join(oversized_leaf);
            fs::write(&path, &oversized).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(
                sink.write(event()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(
                Diagnostics::open(dir.path()).err().unwrap().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(fs::read(&path).unwrap(), oversized);
            assert_eq!(mode(&path), 0o644);
        }
    }

    #[test]
    fn unsafe_current_backup_and_lock_leaves_never_touch_the_sentinel() {
        for leaf in [
            "oxidom-daemon.log",
            "oxidom-daemon.log.1",
            "oxidom-daemon.log.lock",
        ] {
            for unsafe_kind in ["symlink", "hardlink", "fifo", "directory"] {
                let dir = TestDir::new();
                let sentinel = dir.0.join("sentinel");
                fs::write(&sentinel, b"unrelated sentinel\n").unwrap();
                fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o644)).unwrap();
                let target = dir.0.join(leaf);
                match unsafe_kind {
                    "symlink" => symlink(&sentinel, &target).unwrap(),
                    "hardlink" => fs::hard_link(&sentinel, &target).unwrap(),
                    "fifo" => {
                        let name = CString::new(target.as_os_str().as_bytes()).unwrap();
                        // SAFETY: name is a valid, NUL-terminated local test path.
                        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o644) }, 0);
                    }
                    _ => {
                        fs::create_dir(&target).unwrap();
                        fs::write(target.join("sentinel"), b"directory sentinel").unwrap();
                    }
                }
                let before = fs::symlink_metadata(&target).unwrap();
                assert!(
                    Diagnostics::open(dir.path()).is_err(),
                    "{leaf}: {unsafe_kind}"
                );
                let after = fs::symlink_metadata(&target).unwrap();
                assert_eq!(
                    (after.ino(), after.mode(), after.nlink()),
                    (before.ino(), before.mode(), before.nlink())
                );
                assert_eq!(fs::read(&sentinel).unwrap(), b"unrelated sentinel\n");
                assert_eq!(mode(&sentinel), 0o644);
                if unsafe_kind == "directory" {
                    assert_eq!(
                        fs::read(target.join("sentinel")).unwrap(),
                        b"directory sentinel"
                    );
                }
            }
        }
    }

    #[test]
    fn unsafe_backups_introduced_after_open_are_not_overwritten_on_rotation() {
        let dir = TestDir::new();
        let sink = Diagnostics::open(dir.path()).unwrap();
        let previous = full_file(b'x');
        fs::write(dir.path(), &previous).unwrap();
        let sentinel = dir.0.join("sentinel");
        fs::write(&sentinel, b"preserved").unwrap();
        let backup = dir.0.join("oxidom-daemon.log.1");
        symlink(&sentinel, &backup).unwrap();
        assert!(sink.write(event()).is_err());
        assert_eq!(fs::read(dir.path()).unwrap(), previous);
        assert_eq!(fs::read_link(backup).unwrap(), sentinel);
        assert_eq!(fs::read(sentinel).unwrap(), b"preserved");
    }

    #[test]
    fn a_symlink_directory_is_refused_without_changing_its_target_mode() {
        let dir = TestDir::new();
        let target = dir.0.join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.0.join("link");
        symlink(&target, &link).unwrap();
        assert!(Diagnostics::open(link.join("oxidom-daemon.log")).is_err());
        assert_eq!(mode(&target), 0o755);
        assert_eq!(fs::read_dir(target).unwrap().count(), 0);
    }

    #[test]
    fn replacing_the_parent_path_does_not_redirect_an_open_sink() {
        let dir = TestDir::new();
        let parent = dir.0.join("private");
        let sink = Diagnostics::open(parent.join("oxidom-daemon.log")).unwrap();
        let pinned = dir.0.join("pinned");
        fs::rename(&parent, &pinned).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(parent.join("oxidom-daemon.log"), b"replacement sentinel").unwrap();
        fs::write(pinned.join("oxidom-daemon.log"), full_file(b'x')).unwrap();
        sink.append(&record()).unwrap();
        assert_eq!(
            fs::read(parent.join("oxidom-daemon.log")).unwrap(),
            b"replacement sentinel"
        );
        assert_eq!(
            fs::read(pinned.join("oxidom-daemon.log")).unwrap(),
            record()
        );
        assert_eq!(
            fs::metadata(pinned.join("oxidom-daemon.log.1"))
                .unwrap()
                .len(),
            MAX_FILE_BYTES
        );
    }

    #[test]
    fn local_and_thread_contenders_fail_promptly_on_the_stable_lock() {
        let dir = TestDir::new();
        let sink = Diagnostics::open(dir.path()).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.0.join("oxidom-daemon.log.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert_eq!(
            sink.write(event()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            Diagnostics::open(dir.path()).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    assert_eq!(
                        sink.write(event()).unwrap_err().kind(),
                        io::ErrorKind::WouldBlock
                    );
                })
                .join()
                .unwrap();
        });
        drop(lock);
        sink.write(event()).unwrap();
    }

    fn authenticate_child_fixture(path: &Path, parent_pid: u32) -> io::Result<()> {
        let denied = || io::Error::new(io::ErrorKind::PermissionDenied, "not a lock test fixture");
        // SAFETY: getppid/geteuid have no preconditions or pointer arguments.
        let (actual_parent, uid) = unsafe { (libc::getppid() as u32, libc::geteuid()) };
        if parent_pid != actual_parent || path.file_name() != Some("oxidom-daemon.log".as_ref()) {
            return Err(denied());
        }
        let directory = path.parent().ok_or_else(denied)?;
        let prefix = format!("oxidom-diagnostics-{parent_pid}-");
        let suffix = directory
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(&prefix))
            .ok_or_else(denied)?;
        if directory.parent() != Some(std::env::temp_dir().as_path())
            || suffix.is_empty()
            || !suffix.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(denied());
        }
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
            return Err(denied());
        }
        let marker = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join(".lock-test-child"))?;
        let metadata = marker.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.nlink() != 1
            || metadata.mode() & 0o7777 != 0o600
            || metadata.len() != 4
        {
            return Err(denied());
        }
        let mut bytes = [0; 4];
        marker.read_exact_at(&mut bytes, 0)?;
        if bytes != parent_pid.to_le_bytes() {
            return Err(denied());
        }
        Ok(())
    }

    #[test]
    fn a_non_fixture_child_path_is_rejected_without_mutating_the_sentinel() {
        let dir = TestDir::new();
        fs::write(dir.path(), b"incomplete sentinel").unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: getppid has no preconditions or pointer arguments.
        let parent = unsafe { libc::getppid() as u32 };
        assert!(authenticate_child_fixture(&dir.path(), parent).is_err());
        assert_eq!(fs::read(dir.path()).unwrap(), b"incomplete sentinel");
        assert_eq!(mode(&dir.path()), 0o644);
        assert_eq!(mode(&dir.0), 0o755);
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    // The child authenticates a parent-owned exclusive fixture before the sink
    // can chmod, repair, or create anything. Inherited path variables alone do
    // not select the child branch.
    #[test]
    fn a_process_contender_cannot_bypass_the_stable_lock() {
        const CHILD: &str = "OXIDOM_DIAGNOSTICS_LOCK_TEST_PATH";
        const PARENT: &str = "OXIDOM_DIAGNOSTICS_LOCK_TEST_PARENT";
        const TEST: &str = "diagnostics::tests::a_process_contender_cannot_bypass_the_stable_lock";
        let parent = std::env::var(PARENT)
            .ok()
            .and_then(|value| value.parse::<u32>().ok());
        // SAFETY: getppid has no preconditions or pointer arguments.
        if parent == Some(unsafe { libc::getppid() as u32 })
            && std::env::args().any(|argument| argument == TEST)
        {
            let path = PathBuf::from(std::env::var_os(CHILD).expect("child fixture path"));
            authenticate_child_fixture(&path, parent.unwrap()).unwrap();
            let error = Diagnostics::open(path).err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            return;
        }
        let dir = TestDir::new();
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.0.join(".lock-test-child"))
            .unwrap();
        marker.write_all(&std::process::id().to_le_bytes()).unwrap();
        let sink = Diagnostics::open(dir.path()).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.0.join("oxidom-daemon.log.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, dir.path())
            .env(PARENT, std::process::id().to_string())
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        drop(lock);
        sink.write(event()).unwrap();
    }

    #[test]
    fn injected_write_and_sync_failures_roll_back_the_partial_append() {
        let dir = TestDir::new();
        let sink = Diagnostics::open(dir.path()).unwrap();
        sink.append(&record()).unwrap();
        for (fault, code) in [
            (Fault::Error(Stage::Write, libc::ENOSPC), Some(libc::ENOSPC)),
            (Fault::Error(Stage::Write, libc::EACCES), Some(libc::EACCES)),
            (Fault::PartialWrite(libc::ENOSPC), Some(libc::ENOSPC)),
            (Fault::Error(Stage::FileSync, libc::EIO), Some(libc::EIO)),
            (
                Fault::Error(Stage::DirectorySync, libc::EIO),
                Some(libc::EIO),
            ),
            (Fault::WriteZero, None),
        ] {
            let _fault = FaultGuard::install(fault);
            let error = sink.append(&record()).unwrap_err();
            assert_eq!(error.raw_os_error(), code);
            if code.is_none() {
                assert_eq!(error.kind(), io::ErrorKind::WriteZero);
            }
            assert_eq!(fs::read(dir.path()).unwrap(), record());
        }
        let _fault = FaultGuard::install(Fault::ShortWrite);
        sink.append(&record()).unwrap();
        assert_eq!(fs::read(dir.path()).unwrap(), [record(), record()].concat());
    }

    #[test]
    fn interrupted_writes_retry_without_duplicating_the_record() {
        let dir = TestDir::new();
        let sink = Diagnostics::open(dir.path()).unwrap();
        let _fault = FaultGuard::install(Fault::Error(Stage::Write, libc::EINTR));
        sink.append(&record()).unwrap();
        assert_eq!(fs::read(dir.path()).unwrap(), record());
    }

    #[test]
    fn a_failed_append_after_rotation_never_changes_the_new_backup() {
        let dir = TestDir::new();
        let sink = Diagnostics::open(dir.path()).unwrap();
        let previous = full_file(b'x');
        fs::write(dir.path(), &previous).unwrap();
        {
            let _fault = FaultGuard::install(Fault::PartialWrite(libc::ENOSPC));
            assert_eq!(
                sink.append(&record()).unwrap_err().raw_os_error(),
                Some(libc::ENOSPC)
            );
        }
        assert_eq!(
            fs::read(dir.0.join("oxidom-daemon.log.1")).unwrap(),
            previous
        );
        assert!(fs::read(dir.path()).unwrap().is_empty());
        sink.append(&record()).unwrap();
        assert_eq!(fs::read(dir.path()).unwrap(), record());
    }

    #[test]
    fn failure_to_create_the_new_current_after_rotation_preserves_the_backup() {
        let dir = TestDir::new();
        let sink = Diagnostics::open(dir.path()).unwrap();
        let previous = full_file(b'x');
        fs::write(dir.path(), &previous).unwrap();
        {
            let _fault = FaultGuard::install(Fault::Error(Stage::CreateCurrent, libc::ENOSPC));
            assert_eq!(
                sink.write(event()).unwrap_err().raw_os_error(),
                Some(libc::ENOSPC)
            );
        }
        assert!(!dir.path().exists());
        assert_eq!(
            fs::read(dir.0.join("oxidom-daemon.log.1")).unwrap(),
            previous
        );
        sink.append(&record()).unwrap();
        assert_eq!(fs::read(dir.path()).unwrap(), record());
    }

    #[test]
    fn creation_sync_failures_are_reported_without_unlinking_the_stable_lock() {
        for stage in [Stage::FileSync, Stage::DirectorySync] {
            let dir = TestDir::new();
            {
                let _fault = FaultGuard::install(Fault::Error(stage, libc::EIO));
                assert_eq!(
                    Diagnostics::open(dir.path()).err().unwrap().raw_os_error(),
                    Some(libc::EIO)
                );
            }
            let lock = dir.0.join("oxidom-daemon.log.lock");
            let inode = fs::metadata(&lock).unwrap().ino();
            Diagnostics::open(dir.path())
                .unwrap()
                .write(event())
                .unwrap();
            assert_eq!(fs::metadata(lock).unwrap().ino(), inode);
        }
    }

    #[test]
    fn failed_rotation_never_appends_and_directory_sync_errors_are_reported() {
        let dir = TestDir::new();
        let sink = Diagnostics::open(dir.path()).unwrap();
        let previous = full_file(b'x');
        fs::write(dir.path(), &previous).unwrap();
        let backup = dir.0.join("oxidom-daemon.log.1");
        fs::write(&backup, b"old backup\n").unwrap();
        {
            let _fault = FaultGuard::install(Fault::Error(Stage::Rename, libc::EACCES));
            assert_eq!(
                sink.write(event()).unwrap_err().raw_os_error(),
                Some(libc::EACCES)
            );
        }
        assert_eq!(fs::read(dir.path()).unwrap(), previous);
        assert_eq!(fs::read(&backup).unwrap(), b"old backup\n");
        {
            let _fault = FaultGuard::install(Fault::Error(Stage::DirectorySync, libc::EIO));
            assert_eq!(
                sink.write(event()).unwrap_err().raw_os_error(),
                Some(libc::EIO)
            );
        }
        assert!(!dir.path().exists());
        assert_eq!(fs::read(&backup).unwrap(), previous);
        sink.append(&record()).unwrap();
        assert_eq!(fs::read(dir.path()).unwrap(), record());
    }
}
