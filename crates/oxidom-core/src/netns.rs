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

use anyhow::{Context, Result, bail};

use crate::ipc::InterfaceInfo;

pub fn run(
    profile: &str,
    args: &[String],
    interface: Option<&InterfaceInfo>,
    interface_enabled: bool,
) -> Result<()> {
    if args.is_empty() {
        bail!("no command given to `oxidom run`");
    }
    if !interface_enabled {
        bail!(
            "profile `{profile}` has no network interface. Use `oxidom env {profile}` for \
             programs that honor proxy environment variables"
        );
    }
    let interface = interface.context(
        "the profile asks for an interface, but its running session has none; bring the profile \
         down and up again",
    )?;
    bail!(
        "`oxidom run` process marking arrives in the next implementation step. Profile \
         `{profile}` currently uses routing table {} and fwmark {:#x}; apply that mark yourself \
         to route the command through {}. Command requested: {}",
        interface.table,
        interface.mark,
        interface.device,
        shell_words::join(args)
    );
}
