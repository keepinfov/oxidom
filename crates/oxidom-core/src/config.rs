use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core_options::CoreOptions;
use crate::{fsutil, paths};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub socks_port: u16,
    pub http_port: u16,
    pub system_proxy: bool,
    /// Bring the tunnel back up by ourselves when the core dies unexpectedly.
    /// Off by default: a client that silently redials is a client that hides a
    /// server going bad, and the user asked for explicit modes throughout.
    pub reconnect: bool,
    /// What happens to a session's routes when its core exits by itself.
    /// A profile's own setting wins where it has one.
    pub on_core_exit: OnCoreExit,
    pub latency_method: LatencyMethod,
    pub latency_test_url: String,
    /// User-Agent sent when fetching subscriptions. Many panels (Remnawave,
    /// Marzban, Happ) gate the response body on this string and reply with an
    /// "app not supported" page to unknown clients, so we default to a widely
    /// recognized client identifier rather than our own.
    pub subscription_user_agent: String,
    /// Path (or bare command name) of the Xray core. Empty falls back to
    /// `$OXIDOM_XRAY_BIN` — set by the nix wrapper — and then `xray` on `$PATH`.
    pub xray_binary: String,
    /// Path (or bare command name) of tun2socks. Empty falls back to
    /// `$OXIDOM_TUN2SOCKS_BIN` — set by the nix wrapper — and then `PATH`.
    pub tun2socks_binary: String,
    /// Path (or bare command name) of nft. Empty falls back to
    /// `$OXIDOM_NFT_BIN` — set by the nix wrapper — and then `PATH`.
    pub nft_binary: String,
    /// Machine-wide defaults for the generated Xray config. A profile's
    /// `[core]` overrides these field by field; see [`crate::core_options`].
    #[serde(skip_serializing_if = "CoreOptions::is_unset")]
    pub core: CoreOptions,
}

/// What a session does with its routes when the core carrying its traffic exits
/// on its own — a crash, an OOM kill, a server that dropped the connection.
///
/// This is not what an explicit `down` does. `down` is a person saying they want
/// their ordinary connection back, and it releases everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnCoreExit {
    /// Keep the TUN routes, the fwmark rule and the desktop proxy setting until
    /// the session is either reconnected or explicitly taken down. Traffic that
    /// would have used the tunnel is dropped instead.
    ///
    /// The default, and the only safe one. Releasing sends every application
    /// straight back to the ordinary default route with the machine's own
    /// address — silently, and while the interface still shows a tunnel that is
    /// reconnecting. A remote service then sees the real address and country,
    /// which is the exact outcome a tunnel exists to prevent.
    #[default]
    Hold,
    /// Tear the routes down immediately, so applications fall back to the
    /// ordinary route while the session is down. What oxidom did before the
    /// choice existed.
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyMethod {
    Icmp,
    Tcp,
    HttpHead,
    /// Matches [`Config::default`]; also what a latency reading falls back to
    /// when it records a probe that never got as far as reading the config.
    #[default]
    HttpGet,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            socks_port: 10808,
            http_port: 10809,
            system_proxy: false,
            reconnect: false,
            on_core_exit: OnCoreExit::default(),
            latency_method: LatencyMethod::HttpGet,
            latency_test_url: "https://www.gstatic.com/generate_204".to_string(),
            subscription_user_agent: "v2rayN/6.45".to_string(),
            xray_binary: String::new(),
            tun2socks_binary: String::new(),
            nft_binary: String::new(),
            core: CoreOptions::default(),
        }
    }
}

impl Config {
    pub fn load() -> Config {
        let Ok(path) = paths::config_file() else {
            return Config::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str(&s) {
                Ok(config) => config,
                Err(error) => {
                    let moved = fsutil::quarantine(&path);
                    log::warn!("config.toml is not valid ({error}); moved aside to {moved:?}");
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::config_file()?;
        let s = toml::to_string_pretty(self).context("serializing config")?;
        fsutil::write_private_atomic(&path, s.as_bytes()).context("writing config")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_off_by_default() {
        assert!(!Config::default().reconnect);
    }
}
