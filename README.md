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
  VMess, Trojan, Shadowsocks (SIP002), SOCKS, HTTP.
- **Server browser**: multi-column card grid with country flags, inline card
  details, search, per-subscription latency check and sort.
- **Latency probes**: ICMP / TCP / HTTP HEAD / HTTP GET (through the tunnel),
  selectable in Settings.
- **Privacy**: HWID device headers are strictly opt-in per subscription; no
  telemetry. State files are written `0600`.
- Crash-safe: an orphaned xray child or a stuck GNOME system proxy left by a
  killed instance is repaired on the next start.

## Architecture

`oxidom daemon` owns the tunnel (subscriptions, Xray core, probes) and serves
D-Bus (`dev.keepinfov.oxidom.Daemon`); the GUI is a thin client with a tray
icon. Closing the window keeps the connection; the daemon can run as a
system service that starts at boot and survives logout. Launching the GUI
without a daemon auto-spawns a session one.

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

**Arch (AUR):** `packaging/aur/PKGBUILD` (`oxidom-git`), then
`systemctl enable --now oxidom.service`.

## Build & run

With nix (recommended — provides GTK, libadwaita, and the Xray core):

```sh
nix develop -c cargo run       # development (auto-spawns a session daemon)
nix build                      # wrapped release binary in ./result/bin
```

Without nix you need GTK4 ≥ 4.14, libadwaita ≥ 1.4, a Rust toolchain, and an
`xray` binary on `PATH` (or point `OXIDOM_XRAY_BIN` at one).

## CLI

```sh
oxidom                     # launch the GUI (default)
oxidom gui --background    # start hidden (tray only)
oxidom daemon [--system]   # headless daemon (session bus by default)
oxidom run -- CMD          # (planned) run one command through the proxy
```

## Status

V1 targets a local-proxy workflow (SOCKS/HTTP inbounds + GNOME system proxy).
TUN system-wide VPN, per-app netns routing, and routing-rule editing are
planned. See `AGENTS.md` for the full spec.

## License

[MIT](LICENSE)

[Happ]: https://happ.su/
