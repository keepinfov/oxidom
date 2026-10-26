//! Shared child-process supervision and log capture.

use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

use crate::logbook::{self, LogSource};

/// How long a managed child gets to exit on SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// What `stop_child` actually did, so the reap guard is a fact a test can
/// state rather than an absence it cannot observe.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    /// The child had already been waited for; its pid may belong to somebody
    /// else by now, and no signal was sent.
    AlreadyReaped,
    /// SIGTERM (and, past the grace, SIGKILL) went to a child this handle
    /// still owned.
    Stopped,
}

/// Stop a child gracefully when possible, then ensure it has been reaped.
///
/// `Child::kill` alone is an unconditional SIGKILL, which needlessly severs
/// in-flight traffic during an ordinary disconnect.
pub(crate) fn stop_child(child: &mut Child) -> StopOutcome {
    // `is_alive` reaps an exited child through `try_wait`, and std caches the
    // status — so a handle that answers `Ok(Some(_))` here names a pid that
    // may already be somebody else's (pids recycle; `kill_stale_xray` guards
    // the same hazard). Signalling it would terminate an unrelated process,
    // which is why std's own `Child::kill` refuses a reaped child.
    if matches!(child.try_wait(), Ok(Some(_))) {
        return StopOutcome::AlreadyReaped;
    }
    let signalled = i32::try_from(child.id()).is_ok_and(|pid| {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        )
        .is_ok()
    });
    if signalled {
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return StopOutcome::Stopped,
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(_) => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    StopOutcome::Stopped
}

/// Pin the process before validating its identity. Signals and exit polling
/// then refer to that process, never a later owner of the numeric PID.
pub(crate) fn stop_verified_pid(pid: u32, validate: impl FnOnce() -> bool) -> bool {
    let Ok(raw_pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: pidfd_open takes scalar arguments and returns a new descriptor.
    let fd = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, raw_pid, 0) };
    if fd < 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(nix::libc::ESRCH);
    }
    // SAFETY: a successful pidfd_open transfers ownership of this descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let exited = |timeout: Duration| {
        let mut poll = nix::libc::pollfd {
            fd: fd.as_raw_fd(),
            events: nix::libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll points to one initialized descriptor for this call.
        unsafe {
            nix::libc::poll(
                &mut poll,
                1,
                timeout.as_millis().min(i32::MAX as u128) as i32,
            ) > 0
                && poll.revents & nix::libc::POLLIN != 0
        }
    };
    if exited(Duration::ZERO) {
        return true;
    }
    if !validate() {
        return exited(Duration::ZERO);
    }
    for signal in [nix::libc::SIGTERM, nix::libc::SIGKILL] {
        // SAFETY: the descriptor remains owned, and a null siginfo requests the
        // ordinary signal. pidfd_send_signal cannot signal a reused PID.
        let result = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                signal,
                std::ptr::null::<nix::libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            return std::io::Error::last_os_error().raw_os_error() == Some(nix::libc::ESRCH);
        }
        if exited(STOP_GRACE) {
            return true;
        }
    }
    false
}

/// Capture a short-lived core query without pipe-reader threads or an unbounded
/// wait_with_output. Descendants retaining a pipe cannot keep the caller alive.
pub fn output_timeout(
    command: &mut std::process::Command,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let result = (|| {
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            // SAFETY: both pipe descriptors remain owned for this operation.
            if unsafe { nix::libc::fcntl(fd, nix::libc::F_SETFL, nix::libc::O_NONBLOCK) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut buffer = [0; 8192];
        let deadline = Instant::now() + timeout;
        let mut finished = None;
        loop {
            let mut drained = true;
            // Bounded batches prevent a writer from starving the deadline.
            for (reader, bytes) in [
                (&mut stdout as &mut dyn Read, &mut out),
                (&mut stderr as &mut dyn Read, &mut err),
            ] {
                for _ in 0..16 {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            if bytes.len() + count > 1024 * 1024 {
                                return Err(std::io::Error::other(
                                    "core query output exceeds 1 MiB",
                                ));
                            }
                            bytes.extend_from_slice(&buffer[..count]);
                            drained = false;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    }
                }
            }
            if let Some(status) = finished.filter(|_| drained) {
                return Ok(std::process::Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            if finished.is_none()
                && let Some(status) = child.try_wait()?
            {
                // Drain until both pipes would block or reach EOF, still under
                // the deadline and cap if descendants keep writing.
                finished = Some(status);
                continue;
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "core query timed out",
                ));
            }
            if finished.is_none() {
                thread::sleep(Duration::from_millis(10));
            }
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

/// Drain one of a child's pipes into the process log book, tagging every line
/// with the program that wrote it and the profile whose session owns it.
pub(crate) fn spawn_reader<R: std::io::Read + Send + 'static>(
    reader: R,
    source: LogSource,
    profile: String,
) {
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines().map_while(Result::ok) {
            logbook::global().push_process_line(source, Some(&profile), &line);
        }
    });
}

/// Read `/proc/<pid>/cmdline` as arguments. Returns `None` when the process is
/// gone, procfs is unavailable, or the kernel exposes no usable arguments.
pub fn cmdline(pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let arguments: Vec<String> = bytes
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8_lossy(argument).into_owned())
        .collect();
    (!arguments.is_empty()).then_some(arguments)
}

#[cfg(test)]
mod tests {
    #[test]
    fn query_capture_preserves_output_larger_than_one_drain_batch() {
        let output = super::output_timeout(
            std::process::Command::new("/bin/sh").args([
                "-c",
                "i=0; while [ $i -lt 32768 ]; do printf 0123456789abcdef; i=$((i+1)); done",
            ]),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"0123456789abcdef".repeat(32768));
    }
    #[test]
    fn a_query_reading_stdin_receives_eof() {
        let output = super::output_timeout(
            std::process::Command::new("/bin/sh").args([
                "-c",
                "if read line; then printf unexpected; else printf eof; fi",
            ]),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"eof");
    }
    #[test]
    fn a_timed_out_query_is_killed_and_reaped_without_pipe_reader_threads() {
        let error = super::output_timeout(
            std::process::Command::new("/bin/sh").args(["-c", "while :; do :; done"]),
            std::time::Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let output = super::output_timeout(
            std::process::Command::new("/bin/sh").args(["-c", "printf invented; printf error >&2"]),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"invented");
        assert_eq!(output.stderr, b"error");
    }

    #[test]
    fn validation_happens_after_pinning_and_rejection_never_signals_the_process() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "read line"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let validated = std::cell::Cell::new(false);
        assert!(!super::stop_verified_pid(child.id(), || {
            validated.set(true);
            false
        }));
        assert!(validated.get());
        assert!(child.try_wait().unwrap().is_none());
        drop(child.stdin.take());
        child.wait().unwrap();
    }

    #[test]
    fn a_process_exiting_during_validation_cannot_redirect_pidfd_signals() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "read line"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        assert!(super::stop_verified_pid(child.id(), || {
            drop(child.stdin.take());
            child.wait().unwrap();
            true
        }));
    }
    /// The supervisor's poll reaps an exited core through `try_wait`; a later
    /// disconnect must not signal that pid again — it can already belong to an
    /// unrelated process.
    #[test]
    fn a_reaped_child_is_not_signalled_again() {
        let test_binary = std::env::current_exe().expect("locating this test binary");
        let mut child = std::process::Command::new(test_binary)
            // The test harness exits after printing help, without recursively
            // running tests. Unlike /bin/true, it exists inside a Nix sandbox.
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawning this test binary");
        // Deterministic reap: `wait` blocks until the exit and caches the
        // status, exactly the state `is_alive` leaves behind.
        child.wait().expect("reaping the child");

        assert_eq!(
            super::stop_child(&mut child),
            super::StopOutcome::AlreadyReaped
        );
    }

    #[test]
    fn cmdline_identifies_this_test_process_and_missing_processes() {
        let arguments = super::cmdline(std::process::id()).expect("this test process must exist");
        assert!(
            arguments
                .first()
                .is_some_and(|argument| argument.contains("oxidom_core")),
            "{arguments:?}"
        );
        assert_eq!(super::cmdline(u32::MAX), None);
    }
}
