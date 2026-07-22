// One parsed subscription response field is reserved for automatic refresh in
// Phase 3. Keep the core-wide allowance until that API is consumed.
#![allow(dead_code)]

mod cli;
mod config;
mod engine;
mod gui;
mod link;
mod model;
mod netns;
mod paths;
mod probe;
mod state;
mod subscription;
mod xray;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};

pub const APP_ID: &str = "dev.keepinfov.oxidom";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Gui) {
        Command::Gui => gui::run(),
        Command::Run { args } => netns::run(&args),
    }
}
