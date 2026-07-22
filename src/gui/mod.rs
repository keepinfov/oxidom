use adw::prelude::*;
use anyhow::Result;

use crate::APP_ID;

mod group;
mod server_card;
mod sidebar;
mod views;
mod window;

pub fn run() -> Result<()> {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(window::build);

    // Clap already consumed the process arguments; GTK should not parse them again.
    let empty: [&str; 0] = [];
    let _ = app.run_with_args(&empty);
    Ok(())
}
