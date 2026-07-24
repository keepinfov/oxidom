use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "oxidom",
    version,
    about = "oxided freedom — a GTK4 Xray client"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch the graphical interface (default).
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
