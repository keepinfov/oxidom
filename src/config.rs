use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{fsutil, paths};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub socks_port: u16,
    pub http_port: u16,
    pub system_proxy: bool,
    pub latency_method: LatencyMethod,
    pub latency_test_url: String,
    /// User-Agent sent when fetching subscriptions. Many panels (Remnawave,
    /// Marzban, Happ) gate the response body on this string and reply with an
    /// "app not supported" page to unknown clients, so we default to a widely
    /// recognized client identifier rather than our own.
    pub subscription_user_agent: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyMethod {
    Icmp,
    Tcp,
    HttpHead,
    HttpGet,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            socks_port: 10808,
            http_port: 10809,
            system_proxy: false,
            latency_method: LatencyMethod::HttpGet,
            latency_test_url: "https://www.gstatic.com/generate_204".to_string(),
            subscription_user_agent: "v2rayNG/1.9.5".to_string(),
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
