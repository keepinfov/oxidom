//! Shared child-process supervision and log capture.

use std::io::{BufRead, BufReader};
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

/// Stop a recovered child for which no `Child` handle survived the crash.
///
/// Callers must verify the process identity first: unlike `stop_child`, a PID
/// from persistent state may already have been recycled.
pub(crate) fn stop_pid(pid: u32) -> bool {
    let Ok(raw_pid) = i32::try_from(pid) else {
        return false;
    };
    let process = nix::unistd::Pid::from_raw(raw_pid);
    match nix::sys::signal::kill(process, nix::sys::signal::Signal::SIGTERM) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => return true,
        Err(_) => return false,
    }
    if wait_until_gone(process, STOP_GRACE) {
        return true;
    }
    match nix::sys::signal::kill(process, nix::sys::signal::Signal::SIGKILL) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => return true,
        Err(_) => return false,
    }
    wait_until_gone(process, STOP_GRACE)
}

fn wait_until_gone(pid: nix::unistd::Pid, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match nix::sys::signal::kill(pid, None) {
            Err(nix::errno::Errno::ESRCH) => return true,
            Err(_) => return false,
            Ok(()) if Instant::now() >= deadline => return false,
            Ok(()) => thread::sleep(Duration::from_millis(25)),
        }
    }
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
    /// The supervisor's poll reaps an exited core through `try_wait`; a later
    /// disconnect must not signal that pid again — it can already belong to an
    /// unrelated process.
    #[test]
    fn a_reaped_child_is_not_signalled_again() {
        let mut child = std::process::Command::new("/bin/true")
            .spawn()
            .expect("spawning /bin/true");
        // Deterministic reap: `wait` blocks until the exit and caches the
        // status, exactly the state `is_alive` leaves behind.
        child.wait().expect("reaping the child");

        assert_eq!(super::stop_child(&mut child), super::StopOutcome::AlreadyReaped);
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
