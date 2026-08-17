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

/// Which colour scheme the window asks libadwaita for.
///
/// The app followed the desktop and nothing else until this existed, which is
/// fine on GNOME and useless everywhere a desktop has no such setting to
/// follow — or where someone simply wants this one window light while the rest
/// of the session is dark.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorScheme {
    /// Follow the desktop, as before.
    #[default]
    System,
    Light,
    Dark,
}

impl ColorScheme {
    /// `ForceLight`/`ForceDark`, not the `Prefer…` pair. The preferring pair
    /// yields to the desktop — `PreferLight` on a desktop set to dark stays
    /// dark — which makes the setting look broken for the very people who want
    /// it: someone on a dark desktop asking this one window to be light.
    pub fn to_adw(self) -> adw::ColorScheme {
        match self {
            ColorScheme::System => adw::ColorScheme::Default,
            ColorScheme::Light => adw::ColorScheme::ForceLight,
            ColorScheme::Dark => adw::ColorScheme::ForceDark,
        }
    }

    /// Position in the Settings combo, and back. The order is the widget's
    /// contract, so both directions live here rather than at the call site.
    pub fn from_position(position: u32) -> ColorScheme {
        match position {
            1 => ColorScheme::Light,
            2 => ColorScheme::Dark,
            _ => ColorScheme::System,
        }
    }

    pub fn position(self) -> u32 {
        match self {
            ColorScheme::System => 0,
            ColorScheme::Light => 1,
            ColorScheme::Dark => 2,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiPrefs {
    /// Light, dark, or whatever the desktop says. Kept here rather than in the
    /// daemon's `config.toml` because it is a property of this window, not of
    /// the tunnel: a headless daemon has no use for it, and two GUIs on one
    /// system daemon may reasonably disagree.
    pub color_scheme: ColorScheme,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The combo's positions are a contract between two files; a round trip
    /// through them is what keeps the row from showing one thing and applying
    /// another.
    #[test]
    fn every_scheme_survives_the_combo_position_it_is_shown_at() {
        for scheme in [ColorScheme::System, ColorScheme::Light, ColorScheme::Dark] {
            assert_eq!(ColorScheme::from_position(scheme.position()), scheme);
        }
    }

    /// A position the model never produces must land on "follow the system"
    /// rather than on whichever variant happens to be written first.
    #[test]
    fn an_unknown_position_follows_the_system() {
        assert_eq!(ColorScheme::from_position(7), ColorScheme::System);
    }

    /// A chosen scheme has to override the desktop, or the setting does
    /// nothing for the person most likely to reach for it: someone on a dark
    /// desktop who wants this one window light. libadwaita's `Prefer…` pair
    /// yields to the desktop and looks broken here.
    #[test]
    fn choosing_a_scheme_overrides_the_desktop() {
        assert_eq!(ColorScheme::Light.to_adw(), adw::ColorScheme::ForceLight);
        assert_eq!(ColorScheme::Dark.to_adw(), adw::ColorScheme::ForceDark);
        assert_eq!(ColorScheme::System.to_adw(), adw::ColorScheme::Default);
    }

    /// Prefs written before this setting existed carry no key for it, and must
    /// load as "follow the system" — the behaviour those users already had.
    #[test]
    fn prefs_without_a_scheme_keep_following_the_system() {
        let prefs: GuiPrefs = toml::from_str("collapsed_subscriptions = []\n").expect("parses");
        assert_eq!(prefs.color_scheme, ColorScheme::System);
    }

    #[test]
    fn a_saved_scheme_is_read_back() {
        let prefs = GuiPrefs {
            color_scheme: ColorScheme::Dark,
            ..GuiPrefs::default()
        };
        let text = toml::to_string_pretty(&prefs).expect("serializes");
        assert!(text.contains("color_scheme = \"dark\""), "{text}");
        let parsed: GuiPrefs = toml::from_str(&text).expect("parses");
        assert_eq!(parsed.color_scheme, ColorScheme::Dark);
    }
}
