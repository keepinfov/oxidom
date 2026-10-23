//! Launch policy and lifetime ownership for automatically started session daemons.

use std::ffi::OsStr;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{SyncSender, sync_channel};

use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LaunchOwner {
    Standalone,
    External,
}

impl LaunchOwner {
    pub(super) fn parse(value: Option<&OsStr>) -> Result<Self> {
        match value {
            None => Ok(Self::Standalone),
            Some(value) => match value.to_str() {
                Some("" | "standalone") => Ok(Self::Standalone),
                Some("external") => Ok(Self::External),
                _ => {
                    bail!("invalid OXIDOM_SESSION_DAEMON; expected empty, standalone, or external")
                }
            },
        }
    }
}

pub(super) fn connect_or_launch<T>(
    existing: Option<T>,
    owner: LaunchOwner,
    launch: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if let Some(client) = existing {
        return Ok(client);
    }
    if owner == LaunchOwner::External {
        bail!(
            "the externally managed oxidom session daemon is not available; \
             start it with its external supervisor (OXIDOM_SESSION_DAEMON=external)"
        );
    }
    launch()
}

pub(super) struct SessionDaemon {
    // Discovery may still inspect early exit with try_wait before handoff.
    pub(super) child: Option<Child>,
    reap: SyncSender<Child>,
    #[cfg(test)]
    finished: Option<std::sync::mpsc::Receiver<Option<std::process::ExitStatus>>>,
}

impl SessionDaemon {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Self> {
        // Keep inherited or explicitly provided diagnostic streams intact.
        command.stdin(Stdio::null());
        // SAFETY: only async-signal-safe POSIX calls and stack-local data are
        // used between fork and exec. No allocator, locks, logging or runtime.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                let mut stop = std::mem::zeroed();
                libc::sigemptyset(&mut stop);
                // Reset only the daemon's stop signals, not unrelated masks.
                for signal in [libc::SIGINT, libc::SIGTERM] {
                    libc::sigaddset(&mut stop, signal);
                    let mut action: libc::sigaction = std::mem::zeroed();
                    action.sa_sigaction = libc::SIG_DFL;
                    libc::sigemptyset(&mut action.sa_mask);
                    if libc::sigaction(signal, &action, std::ptr::null_mut()) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if libc::sigprocmask(libc::SIG_UNBLOCK, &stop, std::ptr::null_mut()) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        // Provision the only waiter before a process exists. A buffered one-slot
        // channel lets Drop hand over ownership without waiting for child exit.
        let (reap, receiver) = sync_channel::<Child>(1);
        #[cfg(test)]
        let (finished_tx, finished) = sync_channel(1);
        let _waiter = std::thread::Builder::new()
            .name("oxidom-session-reaper".into())
            .spawn(move || {
                let _status = receiver
                    .recv()
                    .ok()
                    .and_then(|mut child| reap_child(&mut child));
                #[cfg(test)]
                let _ = finished_tx.send(_status);
            })?;
        let mut daemon = Self {
            child: None,
            reap,
            #[cfg(test)]
            finished: Some(finished),
        };
        daemon.child = Some(command.spawn()?);
        Ok(daemon)
    }
}

impl Drop for SessionDaemon {
    fn drop(&mut self) {
        if let Some(child) = self.child.take()
            && let Err(mut error) = self.reap.send(child)
        {
            // The receiver normally cannot disappear before this send. If it
            // does, retain the sole child-specific wait owner without spawning.
            let _ = reap_child(&mut error.0);
        }
        // Never join: timeout, discovery failure and GUI close must not stop a
        // daemon. This does not escape cgroups or provide autorestart.
    }
}

fn reap_child(child: &mut Child) -> Option<std::process::ExitStatus> {
    loop {
        match child.wait() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                log::warn!("could not reap the session daemon: {error}");
                return None;
            }
            Ok(status) => return Some(status),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::ffi::{OsStr, OsString};
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Command, ExitStatus, Stdio};
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    use super::{LaunchOwner, SessionDaemon, connect_or_launch};

    #[test]
    fn only_explicit_external_ownership_disables_the_default_bootstrap() {
        for value in [None, Some(OsStr::new("")), Some(OsStr::new("standalone"))] {
            assert_eq!(LaunchOwner::parse(value).unwrap(), LaunchOwner::Standalone);
        }
        assert_eq!(
            LaunchOwner::parse(Some(OsStr::new("external"))).unwrap(),
            LaunchOwner::External
        );
    }

    #[test]
    fn invalid_ownership_fails_closed_without_echoing_the_value() {
        let non_unicode = OsString::from_vec(vec![0xff]);
        for value in [
            OsStr::new("invented-private-value"),
            OsStr::new("EXTERNAL"),
            OsStr::new(" external"),
            non_unicode.as_os_str(),
        ] {
            let error = LaunchOwner::parse(Some(value)).unwrap_err();
            assert_eq!(
                error.to_string(),
                "invalid OXIDOM_SESSION_DAEMON; expected empty, standalone, or external"
            );
        }
    }

    #[test]
    fn an_existing_client_wins_without_launching_another_daemon() {
        for owner in [LaunchOwner::Standalone, LaunchOwner::External] {
            assert_eq!(
                connect_or_launch(Some("existing"), owner, || {
                    panic!("an existing client must win")
                })
                .unwrap(),
                "existing"
            );
        }
    }

    #[test]
    fn an_unavailable_external_daemon_never_invokes_the_launcher() {
        let error = connect_or_launch::<()>(None, LaunchOwner::External, || {
            panic!("external ownership must never launch a daemon")
        })
        .unwrap_err();
        assert!(error.to_string().contains("externally managed"));
        assert!(error.to_string().contains("not available"));
    }

    #[test]
    fn standalone_bootstrap_invokes_the_launcher_exactly_once_even_on_error() {
        let calls = Cell::new(0);
        let result = connect_or_launch(None, LaunchOwner::Standalone, || {
            calls.set(calls.get() + 1);
            Ok("spawned")
        });
        assert_eq!(result.unwrap(), "spawned");
        assert_eq!(calls.get(), 1);

        let result = connect_or_launch::<()>(None, LaunchOwner::Standalone, || {
            calls.set(calls.get() + 1);
            anyhow::bail!("fake spawn failure")
        });
        assert_eq!(result.unwrap_err().to_string(), "fake spawn failure");
        assert_eq!(calls.get(), 2);
    }

    // Run only this inert test in the child, never the daemon or a real bus.
    const CHILD_TEST: &str = "client::session_daemon::tests::standalone_child_probe";
    const PROBE_FD: &str = "OXIDOM_TEST_SESSION_PROBE_FD";
    const TIMEOUT: Duration = Duration::from_secs(20);

    fn socket_pair() -> (UnixStream, UnixStream) {
        let (parent, child) = UnixStream::pair().unwrap();
        for socket in [&parent, &child] {
            socket.set_read_timeout(Some(TIMEOUT)).unwrap();
            socket.set_write_timeout(Some(TIMEOUT)).unwrap();
        }
        (parent, child)
    }

    fn probe_command() -> (Command, UnixStream, UnixStream, UnixStream) {
        let (parent, child) = socket_pair();
        let (stdout, child_stdout) = socket_pair();
        let (stderr, child_stderr) = socket_pair();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", CHILD_TEST, "--nocapture"]);
        command.env(PROBE_FD, child.as_raw_fd().to_string());
        command.env("OXIDOM_TEST_SESSION_ENV", "preserved");
        command.stdout(Stdio::from(std::os::fd::OwnedFd::from(child_stdout)));
        command.stderr(Stdio::from(std::os::fd::OwnedFd::from(child_stderr)));
        // Keep the socket alive until spawn; only the child's copy loses CLOEXEC.
        // Also simulate a caller that blocks and ignores the daemon's stop signals.
        // SAFETY: this hook uses only async-signal-safe operations on child-local
        // descriptors, signal dispositions and the calling thread's signal mask.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(child.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut stop = std::mem::zeroed();
                libc::sigemptyset(&mut stop);
                for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGUSR1] {
                    libc::sigaddset(&mut stop, signal);
                    let mut action: libc::sigaction = std::mem::zeroed();
                    action.sa_sigaction = libc::SIG_IGN;
                    libc::sigemptyset(&mut action.sa_mask);
                    if libc::sigaction(signal, &action, std::ptr::null_mut()) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if libc::sigprocmask(libc::SIG_BLOCK, &stop, std::ptr::null_mut()) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        (command, parent, stdout, stderr)
    }

    struct Probe {
        daemon: Option<SessionDaemon>,
        pidfd: std::os::fd::OwnedFd,
        socket: UnixStream,
        stdout: UnixStream,
        stderr: UnixStream,
    }

    impl Probe {
        fn spawn() -> Self {
            let (mut command, socket, stdout, stderr) = probe_command();
            let mut daemon = SessionDaemon::spawn(&mut command).unwrap();
            let child = daemon.child.as_mut().unwrap();
            // SAFETY: the guard still owns an unreaped Child, so its PID cannot
            // be reused before pidfd_open pins that process's identity.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
            if fd == -1 {
                let error = std::io::Error::last_os_error();
                // No wait or handoff has happened: Child::kill cannot hit a
                // reused PID. Dropping daemon leaves the existing waiter to reap.
                let _ = child.kill();
                panic!("opening the test child's pidfd: {error}");
            }
            let probe = Self {
                daemon: Some(daemon),
                // SAFETY: pidfd_open returned a new descriptor owned by this guard.
                pidfd: unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as libc::c_int) },
                socket,
                stdout,
                stderr,
            };
            drop(command);
            probe
        }

        fn ready(&mut self) {
            let mut ready = [0; 5];
            self.socket.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready");
        }

        fn handoff(&mut self) -> (SessionDaemon, Receiver<Option<ExitStatus>>) {
            let mut daemon = self.daemon.take().unwrap();
            let finished = daemon.finished.take().unwrap();
            (daemon, finished)
        }

        fn release(&mut self) {
            self.socket.write_all(b"x").unwrap();
            let mut done = [0; 4];
            self.socket.read_exact(&mut done).unwrap();
            assert_eq!(&done, b"done");
        }

        fn finished(&mut self, finished: Receiver<Option<ExitStatus>>) {
            let status = finished
                .recv_timeout(TIMEOUT)
                .expect("session waiter did not finish")
                .expect("session waiter failed to reap the child");
            assert!(status.success());
            // Use the pinned identity even after reaping: never waitpid a PID
            // that might now refer to an unrelated child of another test.
            let mut status = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
            // SAFETY: waitid receives a valid output buffer and this guard's pidfd.
            let result = unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    self.pidfd.as_raw_fd() as libc::id_t,
                    status.as_mut_ptr(),
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            assert_eq!(result, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            for (stream, expected) in [
                (&mut self.stdout, "stdout probe diagnostic"),
                (&mut self.stderr, "stderr probe diagnostic"),
            ] {
                let mut output = String::new();
                stream.take(16384).read_to_string(&mut output).unwrap();
                assert!(
                    output.contains(expected),
                    "missing provided diagnostic stream"
                );
            }
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            // SAFETY: pidfd_send_signal can target only the pinned process, even
            // if it has been reaped and its numeric PID has since been reused.
            // Cleanup never waits; SessionDaemon or its waiter remains the owner.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }

    #[test]
    fn standalone_child_probe() {
        let Some(fd) = std::env::var_os(PROBE_FD) else {
            return;
        };
        let fd = fd.to_str().unwrap().parse().unwrap();
        // SAFETY: the parent gives this process a dedicated inherited socket.
        let mut socket = unsafe { UnixStream::from_raw_fd(fd) };
        assert_eq!(
            std::env::var("OXIDOM_TEST_SESSION_ENV").unwrap(),
            "preserved"
        );
        // SAFETY: these queries access only valid local buffers or process state.
        unsafe {
            assert_eq!(libc::getsid(0), libc::getpid());
            assert_eq!(libc::getpgrp(), libc::getpid());
            let null = std::fs::File::open("/dev/null").unwrap();
            let mut expected: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(null.as_raw_fd(), &mut expected), 0);
            let mut actual: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(libc::STDIN_FILENO, &mut actual), 0);
            assert_eq!(actual.st_dev, expected.st_dev);
            assert_eq!(actual.st_ino, expected.st_ino);
            assert_eq!(actual.st_rdev, expected.st_rdev);
            let mut mask = std::mem::zeroed();
            assert_eq!(
                libc::sigprocmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask),
                0
            );
            for signal in [libc::SIGINT, libc::SIGTERM] {
                assert_eq!(libc::sigismember(&mask, signal), 0);
                let mut action: libc::sigaction = std::mem::zeroed();
                assert_eq!(libc::sigaction(signal, std::ptr::null(), &mut action), 0);
                assert_eq!(action.sa_sigaction, libc::SIG_DFL);
            }
            assert_eq!(libc::sigismember(&mask, libc::SIGUSR1), 1);
            let mut action: libc::sigaction = std::mem::zeroed();
            assert_eq!(
                libc::sigaction(libc::SIGUSR1, std::ptr::null(), &mut action),
                0
            );
            assert_eq!(action.sa_sigaction, libc::SIG_IGN);
        }
        writeln!(std::io::stdout(), "stdout probe diagnostic").unwrap();
        writeln!(std::io::stderr(), "stderr probe diagnostic").unwrap();
        socket.write_all(b"ready").unwrap();
        let mut instruction = [0];
        if socket.read_exact(&mut instruction).is_ok() {
            assert_eq!(instruction, [b'x']);
            socket.write_all(b"done").unwrap();
        }
    }

    #[test]
    fn provided_diagnostics_and_null_stdin_survive_detached_discovery() {
        let mut probe = Probe::spawn();
        probe.ready();
        let (mut daemon, finished) = probe.handoff();
        assert!(daemon.child.as_mut().unwrap().try_wait().unwrap().is_none());
        drop(daemon);
        // A successful exchange after Drop proves handoff did not kill the child.
        probe.release();
        probe.finished(finished);
    }

    #[test]
    fn cleanup_stops_the_owned_probe_and_lets_its_waiter_finish() {
        let mut probe = Probe::spawn();
        probe.ready();
        let (daemon, finished) = probe.handoff();
        drop(daemon);
        drop(probe);
        let status = finished
            .recv_timeout(TIMEOUT)
            .expect("cleanup did not release the waiter")
            .expect("cleanup failed to reap the child");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn discovery_error_and_unwind_still_transfer_wait_ownership() {
        for unwind in [false, true] {
            let mut probe = Probe::spawn();
            probe.ready();
            let (daemon, finished) = probe.handoff();
            let discovery = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _daemon = daemon;
                if unwind {
                    panic!("fake discovery unwind");
                }
                anyhow::bail!("fake discovery timeout")
            }));
            if unwind {
                assert!(discovery.is_err());
            } else {
                let result: anyhow::Result<()> = discovery.unwrap();
                assert_eq!(result.unwrap_err().to_string(), "fake discovery timeout");
            }
            probe.release();
            probe.finished(finished);
        }
    }

    #[test]
    fn an_already_observed_child_exit_is_safe_to_hand_to_the_waiter() {
        let mut probe = Probe::spawn();
        probe.ready();
        probe.release();
        let mut event = libc::pollfd {
            fd: probe.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll gets one valid descriptor and a bounded timeout.
        assert_eq!(
            unsafe { libc::poll(&mut event, 1, TIMEOUT.as_millis() as i32) },
            1
        );
        assert_ne!(event.revents & libc::POLLIN, 0);
        let (mut daemon, finished) = probe.handoff();
        let child = daemon.child.as_mut().unwrap();
        let status = child.try_wait().unwrap().expect("child did not exit");
        assert!(status.success());
        assert_eq!(child.try_wait().unwrap(), Some(status));
        drop(daemon);
        probe.finished(finished);
    }
}
