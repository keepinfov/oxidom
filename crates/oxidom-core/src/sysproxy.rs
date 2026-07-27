//! Best-effort desktop proxy toggle via GNOME `gsettings`. Errors are the
//! caller's to surface; a non-GNOME session simply means it can't be applied.

use std::net::Ipv4Addr;
use std::process::Command;

use anyhow::{Context, Result, bail};

fn set(schema: &str, key: &str, value: &str) -> Result<()> {
    let status = Command::new("gsettings")
        .args(["set", schema, key, value])
        .status()
        .context("running gsettings (is this a GNOME session?)")?;
    if !status.success() {
        bail!("gsettings set {schema} {key} failed");
    }
    Ok(())
}

/// Point the desktop proxy at the local Xray inbounds. If any key fails
/// midway the whole change is rolled back so the desktop is never left with a
/// half-configured proxy that `clear()` would not know to undo.
pub fn apply(address: Ipv4Addr, socks_port: u16, http_port: u16) -> Result<()> {
    let result = try_apply(address, socks_port, http_port);
    if result.is_err() {
        let _ = clear();
    }
    result
}

fn try_apply(address: Ipv4Addr, socks_port: u16, http_port: u16) -> Result<()> {
    // Local and loopback traffic must keep bypassing the proxy, or the probe
    // requests (and the proxy itself) would loop through the tunnel.
    set(
        "org.gnome.system.proxy",
        "ignore-hosts",
        "['localhost', '127.0.0.0/8', '::1']",
    )?;
    let address = address.to_string();
    set("org.gnome.system.proxy.socks", "host", &address)?;
    set(
        "org.gnome.system.proxy.socks",
        "port",
        &socks_port.to_string(),
    )?;
    set("org.gnome.system.proxy.http", "host", &address)?;
    set(
        "org.gnome.system.proxy.http",
        "port",
        &http_port.to_string(),
    )?;
    set("org.gnome.system.proxy.https", "host", &address)?;
    set(
        "org.gnome.system.proxy.https",
        "port",
        &http_port.to_string(),
    )?;
    // Flip the mode last so the desktop only ever sees a fully-formed config.
    set("org.gnome.system.proxy", "mode", "manual")?;
    Ok(())
}

/// Restore the desktop proxy to off.
pub fn clear() -> Result<()> {
    set("org.gnome.system.proxy", "mode", "none")
}
