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
    /// Where the geo lists are fetched from.
    ///
    /// A setting rather than a constant because the published lists differ in
    /// what they cover, and for some users a regional list is the difference
    /// between routing that works and routing that does not. The digest is
    /// always the `.sha256sum` sidecar beside whichever file is named, so a
    /// source that publishes none is refused rather than trusted.
    ///
    /// Empty means the built-in default — which is how a config file written
    /// before these existed reads, and how the settings page clears a custom
    /// address back to the default without needing a second flag.
    pub geoip_url: String,
    pub geosite_url: String,
    /// Matching-version override for the managed Xray core. Empty installs the
    /// pinned release; `$OXIDOM_XRAY_BIN` and `xray` on `$PATH` are offline
    /// fallbacks only when they report the same version.
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
            geoip_url: String::new(),
            geosite_url: String::new(),
            xray_binary: String::new(),
            tun2socks_binary: String::new(),
            nft_binary: String::new(),
            core: CoreOptions::default(),
        }
    }
}

impl Config {
    /// Where one geo list is fetched from: the configured address, or the
    /// built-in default when the setting is empty.
    ///
    /// One reader for both, so the daemon that downloads and the dialog that
    /// says what will be contacted cannot name different hosts.
    pub fn geo_url(&self, asset: crate::xray::assets::GeoAsset) -> &str {
        let configured = match asset {
            crate::xray::assets::GeoAsset::GeoIp => &self.geoip_url,
            crate::xray::assets::GeoAsset::GeoSite => &self.geosite_url,
        };
        crate::xray::assets::resolve_url(asset, configured)
    }

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

    /// Empty means the built-in default rather than "no source at all", which
    /// is what makes a config file written before the setting existed behave
    /// exactly as it did — and what lets the settings page clear a custom
    /// address back to the default without a second flag to mean "unset".
    #[test]
    fn an_unset_geo_source_is_the_built_in_one() {
        use crate::xray::assets::GeoAsset;
        let mut config = Config::default();
        assert_eq!(
            config.geo_url(GeoAsset::GeoIp),
            GeoAsset::GeoIp.default_url()
        );
        assert_eq!(
            config.geo_url(GeoAsset::GeoSite),
            GeoAsset::GeoSite.default_url()
        );

        config.geoip_url = "   ".to_string();
        assert_eq!(
            config.geo_url(GeoAsset::GeoIp),
            GeoAsset::GeoIp.default_url(),
            "an address of nothing but spaces is not an address"
        );

        config.geoip_url = "https://example.invalid/geoip.dat".to_string();
        assert_eq!(
            config.geo_url(GeoAsset::GeoIp),
            "https://example.invalid/geoip.dat"
        );
        assert_eq!(
            config.geo_url(GeoAsset::GeoSite),
            GeoAsset::GeoSite.default_url(),
            "the two lists are chosen separately"
        );
    }

    /// A file written before these existed must still load, and must load as
    /// the source it was actually using.
    #[test]
    fn a_config_written_before_the_geo_source_existed_still_loads() {
        let older = r#"
            socks_port = 10808
            http_port = 10809
            subscription_user_agent = "v2rayN/6.45"
        "#;
        let config: Config = toml::from_str(older).expect("parses");
        assert_eq!(config.socks_port, 10808);
        assert!(config.geoip_url.is_empty());
        assert_eq!(
            config.geo_url(crate::xray::assets::GeoAsset::GeoIp),
            crate::xray::assets::GeoAsset::GeoIp.default_url()
        );
    }
}
