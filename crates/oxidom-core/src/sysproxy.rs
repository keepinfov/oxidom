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
    update(address, socks_port, http_port, false, set)
}

/// Replace an owned proxy without switching to direct mode, even on failure.
pub fn replace(address: Ipv4Addr, socks_port: u16, http_port: u16) -> Result<()> {
    update(address, socks_port, http_port, true, set)
}

fn update(
    address: Ipv4Addr,
    socks_port: u16,
    http_port: u16,
    preserve: bool,
    mut set: impl FnMut(&str, &str, &str) -> Result<()>,
) -> Result<()> {
    let result = (|| {
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
    })();
    if result.is_err() && !preserve {
        let _ = set("org.gnome.system.proxy", "mode", "none");
    }
    result
}

/// Restore the desktop proxy to off.
pub fn clear() -> Result<()> {
    set("org.gnome.system.proxy", "mode", "none")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_an_owned_proxy_never_switches_to_direct_on_failure() {
        for failed_write in 0..8 {
            let mut writes = Vec::new();
            let result = update(
                Ipv4Addr::LOCALHOST,
                10808,
                10809,
                true,
                |schema, key, value| {
                    let index = writes.len();
                    writes.push((schema.to_string(), key.to_string(), value.to_string()));
                    if index == failed_write {
                        bail!("injected settings failure");
                    }
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert!(
                !writes
                    .iter()
                    .any(|(_, key, value)| key == "mode" && value == "none")
            );
        }
    }

    #[test]
    fn an_initial_proxy_apply_rolls_back_a_partial_configuration() {
        let mut writes = Vec::new();
        assert!(
            update(Ipv4Addr::LOCALHOST, 10808, 10809, false, |_, key, value| {
                writes.push((key.to_string(), value.to_string()));
                if key == "port" {
                    bail!("injected settings failure");
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(writes.last(), Some(&("mode".into(), "none".into())));
    }
}
