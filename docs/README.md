# oxidom documentation

*oxided freedom* — a native Linux desktop **Xray / v2ray client** built with GTK4
and libadwaita in Rust.

New here? Start with [installation.md](installation.md), then
[quickstart.md](quickstart.md).

## The pages

| Page | What it covers |
|---|---|
| [installation.md](installation.md) | NixOS and the flake, Arch, building from source, the tested distro matrix |
| [quickstart.md](quickstart.md) | Zero to a verified tunnel, in the GUI and on the command line |
| [gui.md](gui.md) | The graphical client: the five pages, filters and groups, the tray |
| [cli.md](cli.md) | Every command, flag, exit code and JSON schema |
| [configuration.md](configuration.md) | `config.toml`, file locations, environment variables |
| [profiles-and-pools.md](profiles-and-pools.md) | Profile files, pools, balancing strategies, systemd units |
| [subscriptions-and-protocols.md](subscriptions-and-protocols.md) | Supported protocols and link schemes, subscription formats, HWID and privacy |
| [routing.md](routing.md) | What actually carries traffic: local proxies, the GNOME system proxy, TUN, per-app routing |
| [architecture.md](architecture.md) | Crates, the daemon, the process model, the security model |
| [troubleshooting.md](troubleshooting.md) | Organised by symptom, with the real error text |

## I want to…

| | |
|---|---|
| install it on NixOS | [installation.md § NixOS](installation.md#nixos) |
| install it on another distro | [installation.md § From source](installation.md#from-source) |
| add my subscription | [quickstart.md](quickstart.md) |
| find out why my servers disappeared | [troubleshooting.md](troubleshooting.md#my-servers-vanished) |
| send my whole desktop through the tunnel | [routing.md § GNOME system proxy](routing.md#gnome-system-proxy) |
| send *one program* through the tunnel | [routing.md § Per-process routing](routing.md#per-process-routing) |
| run several tunnels at once | [profiles-and-pools.md](profiles-and-pools.md) |
| spread traffic across many servers | [profiles-and-pools.md § Pools](profiles-and-pools.md#pools) |
| connect at boot, without logging in | [profiles-and-pools.md § systemd](profiles-and-pools.md#systemd) |
| script against it | [cli.md § Exit codes](cli.md#exit-codes), [§ JSON schemas](cli.md#json-schemas) |
| change ports or the probe method | [configuration.md](configuration.md#configtoml) |
| understand how it is built | [architecture.md](architecture.md) |

## Two things worth knowing early

**The choice of daemon is the choice of database.** A session daemon keeps its
servers and profiles in your home directory; the system daemon keeps them in
`/var/lib/oxidom`. They are separate. This is behind almost every "my servers
vanished" report — see
[configuration.md § The two databases](configuration.md#the-two-databases).

**Enabling a TUN interface is not by itself a VPN.** The default,
`routes = "manual"`, deliberately changes no system route; only
[`oxidom run`](routing.md#per-process-routing) traffic is carried. For a
system-wide tunnel set `routes = "default"` — see
[routing.md](routing.md#what-routes-actually-does).

## For contributors

[`AGENTS.md`](../AGENTS.md) in the repository root is the authoritative
implementation spec — binding contracts and the reasoning behind them. These docs
are the user-facing view of the same system.
