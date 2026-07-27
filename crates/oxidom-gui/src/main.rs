mod gui;

use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "oxidom-gui",
    version,
    about = "Graphical interface for the oxidom Xray client"
)]
struct Cli {
    /// Start without showing the window.
    #[arg(long)]
    background: bool,
    /// Stay in the foreground and log at debug level. $RUST_LOG still wins,
    /// so `RUST_LOG=warn oxidom-gui --debug` is a quiet foreground run.
    #[arg(long)]
    debug: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let default_level = if cli.debug { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .init();
    if !cli.debug {
        detach();
    }
    gui::run(cli.background)
}

/// Fork into the background, returning only in the child, so that closing the
/// terminal the window was started from does not take the tray with it.
///
/// Skipped when stdout is not a terminal, because then something is already
/// supervising this process: the tray unit runs `oxidom-gui --background` as
/// `Type=simple`, and a main process that forks and exits reads to systemd as
/// a service that died during startup. A pipe gets the same treatment for the
/// same reason — whoever is reading it wants this process, not a stub of it.
fn detach() {
    if !std::io::stdout().is_terminal() {
        return;
    }
    // SAFETY: no thread has been created yet — fork carries only the calling
    // thread over, and GTK, zbus and the poll timer all come later. This is
    // also why the fork cannot be deferred: after `gui::run` there is no safe
    // moment left.
    unsafe {
        match libc::fork() {
            // Nothing was lost that a foreground run does not also give: say
            // so and carry on attached rather than refusing to start.
            -1 => {
                log::warn!("could not detach from the terminal, staying in the foreground");
                return;
            }
            0 => {}
            _ => std::process::exit(0),
        }
        // A single fork is enough. The second one in the classic recipe exists
        // to make re-acquiring a controlling terminal impossible, and a GUI
        // never opens one to begin with.
        if libc::setsid() == -1 {
            log::warn!("could not start a new session after detaching");
        }
        silence_stdio();
    }
}

/// Point stdio at /dev/null once detached: the shell has its prompt back, and
/// a background process still writing to that terminal scribbles over it.
///
/// # Safety
///
/// Must be called from a single-threaded process — `dup2` retargets file
/// descriptors every thread shares.
unsafe fn silence_stdio() {
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null < 0 {
            return;
        }
        for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            libc::dup2(null, target);
        }
        if null > libc::STDERR_FILENO {
            libc::close(null);
        }
    }
}
