//! GUI-only display preferences, persisted independently of the daemon's
//! `config.toml`/`state.toml` (which the GUI never writes directly).

use std::collections::HashSet;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use oxidom_core::model::Subscription;
use oxidom_core::pool::PoolQuery;
use oxidom_core::{fsutil, paths};

/// The built-in group every install starts with. Its membership is explicit,
/// so starring a card is the only thing that puts a server in it.
pub const FAVOURITES_ID: &str = "favourites";

/// Whether a group is a frozen list or a live rule.
///
/// Stored rather than inferred from `members` being empty, which is what
/// `PoolKind` does. A profile with an empty pool cannot exist — `resolve`
/// refuses it — so inference is safe there; a *group* can sit empty for as long
/// as the user has not starred anything yet, and an empty list inferred as an
/// unfiltered rule would silently mean "every server on the machine".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GroupKind {
    #[default]
    List,
    Rule,
}

/// A named set of servers: what the user picks a scope from, and what a pool
/// connects to.
///
/// Deliberately a GUI preference rather than daemon state. A group *is* a
/// `PoolQuery`, and `select.pool` already expresses one completely — so
/// connecting materialises the group into the profile and the daemon never
/// needs to learn a new noun. The cost is that editing a group does not reach a
/// session that is already up, which is exactly what the existing `stale` mark
/// on a session already says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerGroup {
    pub id: String,
    pub name: String,
    /// Emoji shown before the name on the chip. Empty is fine.
    pub icon: String,
    pub kind: GroupKind,
    /// A list uses `members`; a rule uses the filters. The two halves are
    /// mutually exclusive, which `Profile::validate` enforces once the group
    /// reaches a profile.
    pub query: PoolQuery,
}

impl ServerGroup {
    pub fn favourites() -> Self {
        Self {
            id: FAVOURITES_ID.to_string(),
            name: "Favourites".to_string(),
            icon: "★".to_string(),
            kind: GroupKind::List,
            query: PoolQuery {
                name: "Favourites".to_string(),
                ..PoolQuery::default()
            },
        }
    }

    pub fn label(&self) -> String {
        if self.icon.is_empty() {
            self.name.clone()
        } else {
            format!("{} {}", self.icon, self.name)
        }
    }
}

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
    /// Saved scopes, in chip order. Never pruned against the daemon's servers:
    /// a list is *meant* to shrink when a server goes away, and a rule matches
    /// whatever exists at the time it runs.
    pub groups: Vec<ServerGroup>,
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
        // Not written back: an install that has never starred anything should
        // not acquire a prefs file just for showing the chip.
        if !prefs.groups.iter().any(|group| group.id == FAVOURITES_ID) {
            prefs.groups.insert(0, ServerGroup::favourites());
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
