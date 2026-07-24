use adw::prelude::*;
use anyhow::Result;

use crate::APP_ID;

mod flags;
mod group;
mod operation;
mod server_card;
mod sidebar;
mod views;
mod window;

pub fn run() -> Result<()> {
    gtk::glib::set_prgname(Some(APP_ID));
    gtk::glib::set_application_name("oxidom");
    #[cfg(debug_assertions)]
    install_development_desktop_integration();

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(window::build);

    // Clap already consumed the process arguments; keep only argv[0]. GLib
    // requires it to activate the application reliably.
    let args = ["oxidom"];
    let _ = app.run_with_args(&args);
    Ok(())
}

#[cfg(debug_assertions)]
fn install_development_desktop_integration() {
    use std::path::{Path, PathBuf};

    let Some(data_home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/share"))
        })
    else {
        log::warn!("could not locate the user data directory for the application icon");
        return;
    };
    let Ok(executable) = std::env::current_exe() else {
        log::warn!("could not locate the oxidom executable for desktop integration");
        return;
    };
    let executable = executable
        .canonicalize()
        .unwrap_or(executable)
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let desktop = include_str!("../../data/dev.keepinfov.oxidom.desktop")
        .replace("Exec=oxidom", &format!("Exec=\"{executable}\""));

    let files = [
        (
            data_home
                .join("applications")
                .join(format!("{APP_ID}.desktop")),
            desktop.as_bytes(),
        ),
        (
            data_home
                .join("icons/hicolor/scalable/apps")
                .join(format!("{APP_ID}.svg")),
            include_bytes!("../../data/dev.keepinfov.oxidom.svg").as_slice(),
        ),
        (
            data_home
                .join("icons/hicolor/symbolic/apps")
                .join(format!("{APP_ID}-symbolic.svg")),
            include_bytes!("../../data/dev.keepinfov.oxidom-symbolic.svg").as_slice(),
        ),
    ];

    for (path, contents) in files {
        if let Err(error) = write_if_changed(&path, contents) {
            log::warn!("could not install {}: {error}", path.display());
        }
    }

    fn write_if_changed(path: &Path, contents: &[u8]) -> std::io::Result<()> {
        if std::fs::read(path).is_ok_and(|current| current == contents) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)
    }
}
