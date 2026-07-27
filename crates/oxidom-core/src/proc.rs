//! Shared child-process supervision and bounded log capture.

use std::io::{BufRead, BufReader};
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const LOG_CAP: usize = 500;
/// How long a managed child gets to exit on SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// Stop a child gracefully when possible, then ensure it has been reaped.
///
/// `Child::kill` alone is an unconditional SIGKILL, which needlessly severs
/// in-flight traffic during an ordinary disconnect.
pub(crate) fn stop_child(child: &mut Child) {
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
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(_) => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
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

pub(crate) fn spawn_reader<R: std::io::Read + Send + 'static>(
    reader: R,
    logs: Arc<Mutex<Vec<String>>>,
) {
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines().map_while(Result::ok) {
            push_log(&logs, line);
        }
    });
}

pub(crate) fn push_log(logs: &Arc<Mutex<Vec<String>>>, line: String) {
    let mut logs = crate::sync::lock(logs);
    if logs.len() >= LOG_CAP {
        logs.remove(0);
    }
    logs.push(line);
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
