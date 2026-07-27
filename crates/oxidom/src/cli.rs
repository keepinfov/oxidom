use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
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
    /// Connect using a named profile.
    #[command(visible_alias = "connect-profile")]
    Up {
        /// Profile name.
        #[arg(default_value = "default")]
        profile: String,
    },
    /// Disconnect the active tunnel.
    #[command(visible_alias = "disconnect")]
    Down {
        /// Profile to stop. Omitted, the tunnel is stopped whoever owns it.
        profile: Option<String>,
    },
    /// Connect directly to one server, without a profile.
    Connect {
        /// Exact alias/id or a unique alias/name substring.
        handle: String,
    },
    /// Show the active connection.
    Status {
        /// Print the stable machine-readable schema.
        #[arg(long)]
        json: bool,
    },
    /// Print the active server endpoint or observed public egress address.
    Ip {
        /// Observe the public address through the active SOCKS tunnel.
        #[arg(long)]
        egress: bool,
        /// Ignore the 60-second egress cache.
        #[arg(long, requires = "egress")]
        fresh: bool,
    },
    /// List daemon objects.
    List {
        /// Object type to list.
        #[arg(value_enum, default_value_t = ListTarget::Servers)]
        target: ListTarget,
        /// Print the stable machine-readable schema.
        #[arg(long)]
        json: bool,
    },
    /// Measure one server and print milliseconds.
    Ping {
        /// Exact alias/id or a unique alias/name substring.
        handle: String,
    },
    /// Assign a stable human handle to a server.
    Alias {
        /// Existing exact alias/id or a unique alias/name substring.
        handle: String,
        /// New globally unique alias.
        new: String,
    },
    /// Manage named connection profiles.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Launch the graphical interface.
    Gui {
        /// Start without showing the window (for autostart; the tray/daemon
        /// keep working, activating the app again presents the window).
        #[arg(long)]
        background: bool,
        /// Stay in the foreground and log at debug level.
        #[arg(long)]
        debug: bool,
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

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ListTarget {
    Servers,
    Profiles,
    Subscriptions,
}

#[derive(Subcommand, Debug)]
pub enum ProfileCommand {
    /// List profiles.
    List {
        /// Print the stable machine-readable schema.
        #[arg(long)]
        json: bool,
    },
    /// Print one profile as TOML.
    Show { name: String },
    /// Create an empty profile using the current proxy ports.
    New { name: String },
    /// Edit one profile with $EDITOR, $VISUAL, or vi.
    Edit { name: String },
    /// Remove one profile.
    Rm { name: String },
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

pub fn run_gui(background: bool, debug: bool) -> Result<()> {
    let executable = gui_binary();
    let mut command = std::process::Command::new(&executable);
    if background {
        command.arg("--background");
    }
    if debug {
        command.arg("--debug");
    }
    let error = command.exec();
    Err(error).with_context(|| {
        format!(
            "launching the graphical interface with {} (set OXIDOM_GUI_BIN to override it)",
            executable.display()
        )
    })
}
