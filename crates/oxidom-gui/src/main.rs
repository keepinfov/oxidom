mod gui;

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
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    gui::run(cli.background)
}
