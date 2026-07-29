//! GUI-only display preferences, persisted independently of the daemon's
//! `config.toml`/`state.toml` (which the GUI never writes directly).

use std::collections::HashSet;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use oxidom_core::model::Subscription;
use oxidom_core::{fsutil, paths};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiPrefs {
    /// Subscription ids whose server-card grid is collapsed on the Servers
    /// page.
    pub collapsed_subscriptions: HashSet<String>,
    /// Subscription ids in the order the user arranged them. Advisory: ids the
    /// daemon no longer knows are ignored, and subscriptions added since are
    /// appended. See `reduce::ordered_subscriptions`.
    pub subscription_order: Vec<String>,
}

impl GuiPrefs {
    pub fn load(subscriptions: &[Subscription]) -> GuiPrefs {
        let Ok(path) = paths::gui_prefs_file() else {
            return GuiPrefs::default();
        };
        let mut prefs = match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str(&s) {
                Ok(prefs) => prefs,
                Err(error) => {
                    let moved = fsutil::quarantine(&path);
                    log::warn!("gui_prefs.toml is not valid ({error}); moved aside to {moved:?}");
                    GuiPrefs::default()
                }
            },
            Err(_) => GuiPrefs::default(),
        };
        let current: HashSet<&str> = subscriptions
            .iter()
            .map(|subscription| subscription.id.as_str())
            .collect();
        let before = prefs.collapsed_subscriptions.len() + prefs.subscription_order.len();
        prefs
            .collapsed_subscriptions
            .retain(|subscription_id| current.contains(subscription_id.as_str()));
        prefs
            .subscription_order
            .retain(|subscription_id| current.contains(subscription_id.as_str()));
        if prefs.collapsed_subscriptions.len() + prefs.subscription_order.len() != before
            && let Err(error) = prefs.save()
        {
            log::warn!("could not discard stale gui prefs: {error:#}");
        }
        prefs
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::gui_prefs_file()?;
        let s = toml::to_string_pretty(self).context("serializing gui prefs")?;
        fsutil::write_private_atomic(&path, s.as_bytes()).context("writing gui prefs")?;
        Ok(())
    }
}
