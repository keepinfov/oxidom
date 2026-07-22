//! Per-process routing for `oxidom run -- <cmd>`.
//!
//! Design (see .notes/DECISIONS.md, AGENTS.md): run the target process inside a
//! network namespace whose only egress is the active Xray SOCKS inbound. The
//! namespace setup (veth pair + a userspace tun2socks/redsocks bridge to
//! 127.0.0.1:<socks_port>, or a slirp-style path) requires elevated privileges,
//! so it must be performed by a small **privileged helper** — the GUI/CLI
//! frontend never runs as root.
//!
//! The helper's privilege model (setuid vs polkit action vs a NixOS-module-
//! installed helper) is not yet finalized. Until it lands, this returns a clear
//! error instead of silently running the process un-proxied (which would leak
//! traffic).

use anyhow::{bail, Result};

use crate::config::Config;

pub fn run(args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("no command given to `oxidom run`");
    }
    let cfg = Config::load();
    bail!(
        "`oxidom run` is not yet available: per-process routing needs the privileged \
         netns helper, which is not installed. The active proxy would be \
         socks5://127.0.0.1:{}. Command requested: {}",
        cfg.socks_port,
        shell_words::join(args)
    );
}
