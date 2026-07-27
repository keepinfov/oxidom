# oxidom

*oxided freedom* — a native Linux desktop **Xray / v2ray client** built with
GTK4 + libadwaita in Rust. Inspired by [Happ] and the GNOME Files (Nautilus)
aesthetic.

oxidom manages subscriptions and standalone servers, drives an **Xray core**
child process (generating its JSON config), and exposes the tunnel as local
SOCKS5 + HTTP proxies — with an optional GNOME system-proxy toggle. The GUI
runs fully unprivileged.

## Features

- **Subscriptions**: base64 share-link lists, provider-selected Xray JSON,
  sing-box JSON, and Clash YAML responses (including native Xray balanced
  profiles); quota/expiry from `subscription-userinfo` headers.
- **Protocols**: VLESS (Reality / XTLS-Vision / xhttp / ws / grpc / tcp),
  VMess, Trojan, Shadowsocks (SIP002), SOCKS, HTTP, Hysteria2 (obfuscation and
  port hopping; needs Xray 26.1+, which is where the native outbound landed).
- **Server browser**: multi-column card grid with country flags, inline card
  details, search, per-subscription latency check and sort.
- **Latency probes**: ICMP / TCP / HTTP HEAD / HTTP GET (through the tunnel),
  selectable in Settings.
- **Privacy**: HWID device headers are strictly opt-in per subscription; no
  telemetry. State files are written `0600`.
- Crash-safe: an orphaned xray child or a stuck GNOME system proxy left by a
  killed instance is repaired on the next start.

## Architecture

The Cargo workspace is split into three crates: `oxidom-core` contains the
shared tunnel, subscription, probe, and D-Bus client logic; `oxidom` is the
headless CLI/daemon; and `oxidom-gui` is the GTK application. `oxidom daemon`
owns the tunnel and serves D-Bus (`dev.keepinfov.oxidom.Daemon`); the GUI is a
thin client with a tray icon. Closing the window keeps the connection; the
daemon can run as a system service that starts at boot and survives logout.
Launching the GUI without a daemon auto-spawns a session one.

## Install

**NixOS (flake):**

```nix
inputs.oxidom.url = "github:keepinfov/oxidom";
# ...
imports = [ inputs.oxidom.nixosModules.default ];
programs.oxidom.enable = true;          # GUI
programs.oxidom.trayAutostart = true;   # tray at login
services.oxidom.enable = true;          # system daemon at boot
services.oxidom.socksPort = 20172;      # optional
```

The flake exposes separate `oxidom-cli` (headless) and `oxidom-gui` packages;
the default package contains both. The NixOS module installs the GUI and CLI
when `programs.oxidom.enable` is set and uses only the headless package for the
system daemon.

**Arch (AUR):** `packaging/aur/PKGBUILD` (`oxidom-git`) installs both binaries, then
`systemctl enable --now oxidom.service`.

The system daemon rewrites the machine's proxy configuration and runs the
core, so its D-Bus policy admits root, `wheel`, and the `oxidom` group only —
an unprivileged service account on the same machine cannot redirect your
traffic. Administrators need no setup; to let a non-admin account drive it,
add them to the `oxidom` group (`services.oxidom.users = [ "alice" ];` on
NixOS, `gpasswd -a alice oxidom` elsewhere).

## Build & run

With nix (recommended — provides GTK, libadwaita, and the Xray core):

```sh
nix develop -c cargo build                   # all three workspace crates
nix develop -c cargo run -p oxidom-gui       # graphical client
nix develop -c cargo run -p oxidom -- daemon # headless daemon
nix build                                    # both wrapped binaries
nix build .#oxidom-cli                       # headless package only
nix build .#oxidom-gui                       # graphical package only
```

Without nix you need GTK4 ≥ 4.14, libadwaita ≥ 1.4, a Rust toolchain, and an
`xray` binary on `PATH` (or point `OXIDOM_XRAY_BIN` at one).

## CLI

```sh
oxidom up [PROFILE]                  # connect a profile (default: default)
oxidom down [PROFILE]                # stop unconditionally or only its owner
oxidom connect HANDLE                # connect one server without a profile
oxidom status [--json]               # active server, ports, and tunnel latency
oxidom ip [--egress] [--fresh]       # endpoint IP or cached public egress IP
oxidom list [servers|profiles|subscriptions] [--json]
oxidom ping HANDLE                   # print only milliseconds on success
oxidom alias HANDLE NEW
oxidom profile {list,show,new,edit,rm}
oxidom daemon [--system]             # headless daemon (session bus by default)
oxidom gui [--background] [--debug]  # compatibility shim to oxidom-gui
oxidom run -- CMD                    # reserved per-process proxy launcher
```

Started from a terminal, `oxidom-gui` forks into the background so closing that
terminal does not take the window and tray with it. `--debug` keeps it in the
foreground and raises the default log level; `$RUST_LOG` still overrides either
way. Nothing is detached when stdout is not a terminal, so the tray unit and
`oxidom-gui | tee` behave as before.

Structured data is printed only to stdout; diagnostics and ambiguous-handle
candidates go to stderr. Stable exit codes are `0` (success), `1` (command
error), `3` (not connected), and `4` (daemon unavailable). Read commands do
not start a private daemon; only `up` and `connect` may do so.

Profiles are TOML files owned by the daemon. For example, after
`oxidom profile new work`, set `select.server` with `oxidom profile edit work`
and run it directly or as a boot-managed oneshot:

```sh
sudo systemctl start oxidom@work
sudo systemctl enable oxidom@work
```

## Status

V1 targets a local-proxy workflow (SOCKS/HTTP inbounds + GNOME system proxy).
TUN system-wide VPN, per-app netns routing, and routing-rule editing are
planned. See `AGENTS.md` for the full spec.

## License

[MIT](LICENSE)

[Happ]: https://happ.su/
