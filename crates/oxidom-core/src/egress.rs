//! Observe and cache the tunnel's public egress address for the CLI.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::cli_json::EgressCache;
use crate::{fsutil, paths};

pub const DEFAULT_EGRESS_URL: &str = "https://api.ipify.org";
const CACHE_TTL_MS: u64 = 60_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub fn address(server_id: &str, socks_port: u16, fresh: bool) -> Result<IpAddr> {
    let path = paths::cache_dir()?.join("egress.json");
    let now = crate::ipc::now_unix_ms();
    if !fresh
        && let Ok(body) = std::fs::read_to_string(&path)
        && let Ok(cache) = serde_json::from_str::<EgressCache>(&body)
        && cache.server_id == server_id
        && now.saturating_sub(cache.at_unix_ms) < CACHE_TTL_MS
        && let Ok(ip) = cache.ip.parse()
    {
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
    let proxy_url = format!("socks5://127.0.0.1:{socks_port}");
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

    let cache = EgressCache {
        server_id: server_id.to_string(),
        ip: ip.to_string(),
        at_unix_ms: now,
    };
    match serde_json::to_vec(&cache) {
        Ok(body) => {
            if let Err(error) = fsutil::write_private_atomic(&path, &body) {
                log::warn!("could not cache the egress address: {error:#}");
            }
        }
        Err(error) => log::warn!("could not serialize the egress address cache: {error}"),
    }
    Ok(ip)
}
