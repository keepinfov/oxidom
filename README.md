# oxidom

*oxided freedom* — a native Linux desktop **Xray / v2ray client** built with
GTK4 + libadwaita in Rust. Inspired by [Happ] and the GNOME Files (Nautilus)
aesthetic.

oxidom manages subscriptions and standalone servers, drives an **Xray core**
child process (generating its JSON config), and exposes the tunnel as local
SOCKS5 + HTTP proxies — with an optional GNOME system-proxy toggle, optional TUN
interfaces, and per-app routing. The GUI runs fully unprivileged.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screenshots/servers-connected-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="docs/screenshots/servers-connected-light.png">
  <img alt="The server browser with one node connected" src="docs/screenshots/servers-connected-dark.png">
</picture>

## Install it

**Debian, Ubuntu, Mint:**

```sh
curl -fsSL https://keepinfov.github.io/oxidom/KEY.gpg \
  | sudo gpg --dearmor -o /usr/share/keyrings/oxidom.gpg
echo "deb [signed-by=/usr/share/keyrings/oxidom.gpg] https://keepinfov.github.io/oxidom/deb stable main" \
  | sudo tee /etc/apt/sources.list.d/oxidom.list
sudo apt update && sudo apt install oxidom-gui
```

**Fedora, RHEL:**

```sh
sudo curl -fsSL https://keepinfov.github.io/oxidom/oxidom.repo -o /etc/yum.repos.d/oxidom.repo
sudo dnf install oxidom-gui
```

Both are the signed repository, so upgrades arrive with the rest of the system.
On a server, install `oxidom` instead — the daemon and CLI, with no GTK
dependency.

**Or one line**, which detects apt or dnf and runs exactly the commands above:

```sh
curl -fsSL https://keepinfov.github.io/oxidom/install.sh | sh
```

It checks the key it downloads against the fingerprint it was published with —
`05BC 9AA4 B90F F65A CE7F AE1C 74FE 48BE 84CA 2CCF` — and refuses to install if
they disagree. [Read it first](packaging/install.sh); it prints every command
before running it.

**Too old for the packages** — Ubuntu 24.04 LTS, Debian 12, whose libadwaita is
1.5 and 1.2 against a floor of 1.7 — take the **AppImage** from the [releases
page](https://github.com/keepinfov/oxidom/releases): `chmod +x` and run it. It
carries its own GTK, libadwaita, glibc *and* an Xray core.

**The Xray core is managed for you.** On first use oxidom downloads the tested
Xray release into its own private data directory and verifies the archive
against a digest pinned in its source — no package manager and no
administrator needed. An offline machine can point `xray_binary` at a core of
the same version instead.

Everything else — NixOS, Nix, Arch, from source, the tested distro matrix — is in
**[docs/installation.md](docs/installation.md)**.

## Features

- **Subscriptions**: base64 share-link lists, provider-selected Xray JSON,
  sing-box JSON, and Clash YAML responses (including native Xray balanced
  profiles); quota/expiry from `subscription-userinfo` headers.
- **Protocols**: VLESS (Reality / XTLS-Vision / xhttp / ws / grpc / tcp),
  VMess, Trojan, Shadowsocks (SIP002), SOCKS, HTTP, Hysteria2 (obfuscation and
  port hopping; needs Xray 26.1+, which is where the native outbound landed).
- **Server browser**: multi-column card grid with country flags, inline card
  details, search, per-subscription latency check and sort.
- **Pools**: select many servers by rule or by list and let the core balance
  across them, so activity spreads over exit addresses and a dead node does not
  end the session.
- **Several tunnels at once**: profiles run side by side, each on its own
  loopback address, optionally each on its own TUN interface.
- **Per-app routing**: `oxidom run -- <cmd>` sends one command through a profile
  and leaves everything else on the ordinary route.
- **Latency probes**: ICMP / TCP / HTTP HEAD / HTTP GET (through the tunnel),
  selectable in Settings.
- **Privacy**: HWID device headers are strictly opt-in per subscription; no
  telemetry. State files are written `0600`.
- Crash-safe: an orphaned xray child, a stuck GNOME system proxy, or leftover
  routes and devices left by a killed instance are repaired on the next start.

## Other ways to install

**`.deb` / `.rpm` on their own:** from the [releases
page](https://github.com/keepinfov/oxidom/releases), if you would rather not add
a repository. Installing does not enable the system daemon; `oxidom status` says
what that decides.

**Arch:** oxidom is **not on the AUR**. Build it from the `PKGBUILD` in this
repository — `cd packaging/aur && makepkg -si` — then
`systemctl enable --now oxidom.service` if you want the system daemon.

**NixOS (flake):**

```nix
inputs.oxidom.url = "github:keepinfov/oxidom";
# ...
imports = [ inputs.oxidom.nixosModules.default ];
programs.oxidom.enable = true;          # GUI
programs.oxidom.trayAutostart = true;   # tray at login
services.oxidom.enable = true;          # system daemon at boot
services.oxidom.tun.enable = true;      # allow TUN interfaces
```

**From source:** a Rust toolchain (1.85+), GTK4 and libadwaita development
packages. The Xray core needs no installing first — it is managed, above.

Full instructions, the tested distro matrix, and the from-source asset list are in
**[docs/installation.md](docs/installation.md)**.

## Try it

Once installed:

```sh
oxidom-gui                           # the interface
oxidom status                        # what is running, and what is missing
oxidom connect ch-trojan
oxidom ip --egress
```

Without installing anything, if you have Nix:

```sh
nix run github:keepinfov/oxidom      # the GUI
```

or, in a clone:

```sh
nix develop -c cargo run -p oxidom-gui       # graphical client
nix develop -c cargo run -p oxidom -- daemon # headless daemon
nix build                                    # both wrapped binaries
```

## Documentation

| | |
|---|---|
| [Installation](docs/installation.md) | NixOS, Arch, from source, tested distro matrix |
| [Quickstart](docs/quickstart.md) | Subscription → connected → verified |
| [GUI guide](docs/gui.md) | The five pages, filters and groups, the tray |
| [CLI reference](docs/cli.md) | Every command, flag, exit code, JSON schema |
| [Configuration](docs/configuration.md) | `config.toml`, paths, environment variables |
| [Profiles and pools](docs/profiles-and-pools.md) | Profile files, balancing, systemd units |
| [Subscriptions and protocols](docs/subscriptions-and-protocols.md) | Link schemes, formats, HWID and privacy |
| [Routing](docs/routing.md) | Local proxies, system proxy, TUN, per-app routing |
| [Architecture](docs/architecture.md) | Crates, the daemon, the security model |
| [Troubleshooting](docs/troubleshooting.md) | By symptom, with the real error text |

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) takes you from a clone to a pull request;
[AGENTS.md](AGENTS.md) is the working agreement every change is held to, and
[docs/spec/](docs/spec/) is the implementation contract behind the behaviour.
Security problems go through [SECURITY.md](SECURITY.md) rather than the issue
tracker.

## Architecture in one paragraph

The Cargo workspace is split into three crates: `oxidom-core` contains the shared
tunnel, subscription, probe, and D-Bus client logic; `oxidom` is the headless
CLI/daemon; and `oxidom-gui` is the GTK application. `oxidom daemon` owns the
tunnel and serves D-Bus (`dev.keepinfov.oxidom.Daemon`); the GUI is a thin client
with a tray icon. Closing the window keeps the connection; the daemon can run as a
system service that starts at boot and survives logout. Launching the GUI without
a daemon auto-spawns a session one. See
[docs/architecture.md](docs/architecture.md), and
[docs/spec/](docs/spec/) for the full implementation contract.

## Status

The local-proxy workflow (SOCKS/HTTP inbounds + GNOME system proxy), TUN
interfaces, pools and per-app routing are implemented. Routing-rule editing is
still planned.

## License

[MIT](LICENSE)

[Happ]: https://happ.su/
