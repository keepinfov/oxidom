//! GUI-only display preferences, persisted independently of the daemon's
//! `config.toml`/`state.toml` (which the GUI never writes directly).

use std::collections::HashSet;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{fsutil, paths};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiPrefs {
    /// Subscription ids whose server-card grid is collapsed on the Servers
    /// page.
    pub collapsed_subscriptions: HashSet<String>,
}

impl GuiPrefs {
    pub fn load() -> GuiPrefs {
        let Ok(path) = paths::gui_prefs_file() else {
            return GuiPrefs::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str(&s) {
                Ok(prefs) => prefs,
                Err(error) => {
                    let moved = fsutil::quarantine(&path);
                    log::warn!("gui_prefs.toml is not valid ({error}); moved aside to {moved:?}");
                    GuiPrefs::default()
                }
            },
            Err(_) => GuiPrefs::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::gui_prefs_file()?;
        let s = toml::to_string_pretty(self).context("serializing gui prefs")?;
        fsutil::write_private_atomic(&path, s.as_bytes()).context("writing gui prefs")?;
        Ok(())
    }
}
