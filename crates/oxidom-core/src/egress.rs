//! Observe and cache the tunnel's public egress address for the CLI.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::cli_json::EgressCache;
use crate::{fsutil, paths};

pub const DEFAULT_EGRESS_URL: &str = "https://api.ipify.org";
const CACHE_TTL_MS: u64 = 60_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub fn address(
    profile: &str,
    server_id: &str,
    bind: Ipv4Addr,
    socks_port: u16,
    fresh: bool,
) -> Result<IpAddr> {
    let path = paths::cache_dir()?.join("egress.json");
    let now = crate::ipc::now_unix_ms();
    let mut caches = std::fs::read_to_string(&path)
        .ok()
        .and_then(|body| serde_json::from_str::<HashMap<String, EgressCache>>(&body).ok())
        .unwrap_or_default();
    if !fresh && let Some(ip) = cached_address(&caches, profile, server_id, now) {
        return Ok(ip);
    }

    let url = std::env::var("OXIDOM_EGRESS_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_EGRESS_URL.to_string());
    // ureq 2.12 rejects the curl-style `socks5h` scheme, but its SOCKS5
    // transport passes a domain `TargetAddr` to the proxy instead of resolving
    // it locally. In other words, `socks5` here has the `socks5h` behavior the
    // CLI contract requires.
    let proxy_url = format!("socks5://{bind}:{socks_port}");
    let proxy = ureq::Proxy::new(&proxy_url).context("building the local SOCKS proxy URL")?;
    let response = ureq::AgentBuilder::new()
        .proxy(proxy)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .get(&url)
        .call()
        .with_context(|| format!("requesting the egress address from {url}"))?;
    let body = response
        .into_string()
        .context("reading the egress address response")?;
    let ip: IpAddr = body.trim().parse().with_context(|| {
        format!(
            "the egress service returned no IP address: {:?}",
            body.trim()
        )
    })?;

    caches.insert(
        profile.to_string(),
        EgressCache {
            server_id: server_id.to_string(),
            ip: ip.to_string(),
            at_unix_ms: now,
        },
    );
    match serde_json::to_vec(&caches) {
        Ok(body) => {
            if let Err(error) = fsutil::write_private_atomic(&path, &body) {
                log::warn!("could not cache the egress address: {error:#}");
            }
        }
        Err(error) => log::warn!("could not serialize the egress address cache: {error}"),
    }
    Ok(ip)
}

fn cached_address(
    caches: &HashMap<String, EgressCache>,
    profile: &str,
    server_id: &str,
    now: u64,
) -> Option<IpAddr> {
    let cache = caches.get(profile)?;
    (cache.server_id == server_id && now.saturating_sub(cache.at_unix_ms) < CACHE_TTL_MS)
        .then(|| cache.ip.parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(server_id: &str, ip: &str, at_unix_ms: u64) -> EgressCache {
        EgressCache {
            server_id: server_id.to_string(),
            ip: ip.to_string(),
            at_unix_ms,
        }
    }

    #[test]
    fn cache_is_scoped_by_profile_and_server() {
        let caches = HashMap::from([
            ("home".to_string(), entry("same", "192.0.2.1", 10_000)),
            ("work".to_string(), entry("same", "192.0.2.2", 10_000)),
        ]);

        assert_eq!(
            cached_address(&caches, "home", "same", 10_001),
            Some("192.0.2.1".parse().unwrap())
        );
        assert_eq!(
            cached_address(&caches, "work", "same", 10_001),
            Some("192.0.2.2".parse().unwrap())
        );
        assert_eq!(cached_address(&caches, "home", "other", 10_001), None);
    }

    #[test]
    fn old_single_entry_cache_is_incompatible_by_design() {
        let old = serde_json::to_string(&entry("server", "192.0.2.1", 10_000)).unwrap();

        assert!(serde_json::from_str::<HashMap<String, EgressCache>>(&old).is_err());
    }

    #[test]
    fn expired_cache_is_not_returned() {
        let caches = HashMap::from([("default".to_string(), entry("server", "192.0.2.1", 10_000))]);

        assert_eq!(
            cached_address(&caches, "default", "server", 10_000 + CACHE_TTL_MS),
            None
        );
    }
}
