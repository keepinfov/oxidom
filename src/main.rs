// One parsed subscription response field is reserved for automatic refresh in
// Phase 3. Keep the core-wide allowance until that API is consumed.
#![allow(dead_code)]

mod cli;
mod config;
mod daemon;
mod engine;
mod fsutil;
mod gui;
mod ipc;
mod link;
mod model;
mod netns;
mod paths;
mod probe;
mod state;
mod subscription;
mod subscription_format;
mod sync;
mod sysproxy;
mod xray;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};

pub const APP_ID: &str = "dev.keepinfov.oxidom";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Gui { background: false }) {
        Command::Gui { background } => gui::run(background),
        Command::Daemon {
            system,
            socks_port,
            http_port,
        } => daemon::run(daemon::DaemonOptions {
            system_bus: system,
            socks_port,
            http_port,
        }),
        Command::Run { args } => netns::run(&args),
    }
}
