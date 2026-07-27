//! Supervision of one tun2socks process.

use std::net::SocketAddrV4;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::proc::{push_log, spawn_reader, stop_child};
use crate::tun::resolve::{self, Resolved};

pub struct Tun2socks {
    child: Option<Child>,
    pub logs: Arc<Mutex<Vec<String>>>,
    /// Configured path; empty falls back to the environment and then `PATH`.
    pub binary: String,
}

impl Tun2socks {
    pub fn new(binary: String) -> Self {
        Self {
            child: None,
            logs: Arc::new(Mutex::new(Vec::new())),
            binary,
        }
    }

    pub fn resolve_binary(&self) -> Result<Resolved> {
        resolve::resolve(&self.binary)
    }

    pub fn start(&mut self, device: &str, proxy: SocketAddrV4, mtu: u16) -> Result<()> {
        self.stop();
        crate::sync::lock(&self.logs).clear();
        if let Err(error) = self.try_start(device, proxy, mtu) {
            let message = format!("{error:#}");
            self.note(&message);
            return Err(error);
        }
        Ok(())
    }

    fn try_start(&mut self, device: &str, proxy: SocketAddrV4, mtu: u16) -> Result<()> {
        let resolved = self.resolve_binary()?;
        let mut child = Command::new(&resolved.path)
            .args(arguments(device, proxy, mtu))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning tun2socks ({})", resolved.path.display()))?;

        if let Some(out) = child.stdout.take() {
            spawn_reader(out, self.logs.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, self.logs.clone());
        }
        self.child = Some(child);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            stop_child(&mut child);
        }
    }

    pub fn is_alive(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    pub fn recent_logs(&self) -> Vec<String> {
        crate::sync::lock(&self.logs).clone()
    }

    pub fn note(&self, message: &str) {
        push_log(&self.logs, format!("oxidom: {message}"));
    }
}

impl Drop for Tun2socks {
    fn drop(&mut self) {
        self.stop();
    }
}

/// ⚠ tun2socks 2.7 parses its command line with pflag, not Go flag.
/// A single dash is a shortcut cluster: `-device foo` becomes `-d` with the
/// value `evice`, silently creating another device instead of joining ours.
/// This caused three false gate runs on 2026-07-27.
pub(crate) fn arguments(device: &str, proxy: SocketAddrV4, mtu: u16) -> Vec<String> {
    vec![
        "--device".to_string(),
        device.to_string(),
        "--proxy".to_string(),
        format!("socks5://{proxy}"),
        "--mtu".to_string(),
        mtu.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn long_options_always_use_two_dashes() {
        let arguments = super::arguments(
            "oxi-work",
            SocketAddrV4::new(Ipv4Addr::new(127, 91, 37, 1), 10808),
            1500,
        );
        assert!(
            arguments
                .iter()
                .filter(|argument| argument.starts_with('-'))
                .all(|argument| argument.starts_with("--")),
            "{arguments:?}"
        );
    }

    #[test]
    fn arguments_preserve_the_exact_proxy_endpoint() {
        assert_eq!(
            super::arguments(
                "oxi-work",
                SocketAddrV4::new(Ipv4Addr::new(127, 91, 37, 1), 10808),
                1500,
            ),
            [
                "--device",
                "oxi-work",
                "--proxy",
                "socks5://127.91.37.1:10808",
                "--mtu",
                "1500",
            ]
        );
    }
}
