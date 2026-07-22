use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "oxidom", version, about = "oxided freedom — a GTK4 Xray client")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch the graphical interface (default).
    Gui,
    /// Run a single process routed through the active proxy (via a network namespace).
    Run {
        /// The command and arguments to run, e.g. `oxidom run -- curl https://ifconfig.me`.
        #[arg(trailing_var_arg = true, required = true)]
        args: Vec<String>,
    },
}
