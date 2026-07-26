use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::os::unix::process::CommandExt;

#[derive(Parser, Debug)]
#[command(
    name = "oxidom",
    version,
    about = "oxided freedom — a native Xray client",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch the graphical interface.
    Gui {
        /// Start without showing the window (for autostart; the tray/daemon
        /// keep working, activating the app again presents the window).
        #[arg(long)]
        background: bool,
    },
    /// Run the headless daemon that owns the tunnel and serves D-Bus.
    Daemon {
        /// Serve on the system bus (systemd service) instead of the session bus.
        #[arg(long)]
        system: bool,
        /// Override the local SOCKS inbound port from config.toml.
        #[arg(long)]
        socks_port: Option<u16>,
        /// Override the local HTTP inbound port from config.toml.
        #[arg(long)]
        http_port: Option<u16>,
    },
    /// Run a single process routed through the active proxy (via a network namespace).
    Run {
        /// The command and arguments to run, e.g. `oxidom run -- curl https://ifconfig.me`.
        #[arg(trailing_var_arg = true, required = true)]
        args: Vec<String>,
    },
}

fn gui_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("OXIDOM_GUI_BIN").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(sibling) = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(|parent| parent.join("oxidom-gui")))
        .filter(|path| path.is_file())
    {
        return sibling;
    }
    PathBuf::from("oxidom-gui")
}

pub fn run_gui(background: bool) -> Result<()> {
    let executable = gui_binary();
    let mut command = std::process::Command::new(&executable);
    if background {
        command.arg("--background");
    }
    let error = command.exec();
    Err(error).with_context(|| {
        format!(
            "launching the graphical interface with {} (set OXIDOM_GUI_BIN to override it)",
            executable.display()
        )
    })
}
