mod cli;
mod daemon;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    match cli.command {
        Command::Gui { background } => cli::run_gui(background),
        Command::Daemon {
            system,
            socks_port,
            http_port,
        } => daemon::run(daemon::DaemonOptions {
            system_bus: system,
            socks_port,
            http_port,
        }),
        Command::Run { args } => oxidom_core::netns::run(&args),
    }
}
