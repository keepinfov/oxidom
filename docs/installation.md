# Installation

oxidom ships two binaries:

- **`oxidom`** — the CLI and the daemon. No GTK dependency at all, so it installs
  cleanly on a server.
- **`oxidom-gui`** — the GTK4 / libadwaita interface. Needs a desktop.

You also need an **Xray core**. oxidom drives it as a child process; it does not
bundle one.

## Contents

- [Verifying a download](#verifying-a-download)
- [Debian and Ubuntu](#debian-and-ubuntu)
- [Fedora, RHEL and derivatives](#fedora-rhel-and-derivatives)
- [NixOS](#nixos)
- [Nix without NixOS](#nix-without-nixos)
- [Arch](#arch)
- [From source](#from-source)
- [Tested distro matrix](#tested-distro-matrix)
- [Getting an Xray core](#getting-an-xray-core)
  - [Getting the geo data](#getting-the-geo-data)
- [Optional runtime dependencies](#optional-runtime-dependencies)
- [Installing the assets by hand](#installing-the-assets-by-hand)
- [Who may drive the system daemon](#who-may-drive-the-system-daemon)

## Verifying a download

Every published `.deb`, `.rpm` and AppImage carries a build attestation, so you
can check an asset really came from this repository and from the commit it
claims, rather than trusting the file name:

```sh
gh attestation verify oxidom_0.1.0-1_amd64.deb --repo keepinfov/oxidom
```

Each release also has a `SHA256SUMS` beside the assets.

For a program that carries your traffic this is worth the extra command.

## Debian and Ubuntu

Two packages, from the [releases page]:

```sh
sudo apt install ./oxidom_0.1.0-1_amd64.deb ./oxidom-gui_0.1.0-1_amd64.deb
```

`oxidom` is the CLI and daemon; `oxidom-gui` is the interface and depends on it
at exactly the same version. Install only the first on a server — it has no GTK
dependency at all.

**The daemon package installs on older releases than the interface does.** It is
built against glibc 2.36, so it works on Debian 12 and Ubuntu 22.04 onwards. The
interface links against the distribution's own GTK and needs libadwaita 1.7,
which lands in Debian 13 and Ubuntu 25.04 — see the [matrix](#tested-distro-matrix).
On an older release, install `oxidom` alone and use the CLI, or use the
[Nix package](#nix-without-nixos), which brings its own GTK stack.

Installing does **not** enable the system daemon. That is deliberate and
explained under [the two databases](configuration.md#the-two-databases): enabling
it moves your servers from `~/.local/share/oxidom` to `/var/lib/oxidom` and
limits access to `root`, `wheel` and the `oxidom` group. A desktop client does
not need it — it starts a session daemon of its own. To run it at boot anyway:

```sh
sudo systemctl enable --now oxidom.service
```

Removing the packages leaves `/var/lib/oxidom` and the `oxidom` account behind,
on `purge` as much as on `remove`. That directory is your subscription database.

## Fedora, RHEL and derivatives

```sh
sudo dnf install ./oxidom-0.1.0-1.x86_64.rpm ./oxidom-gui-0.1.0-1.x86_64.rpm
```

Everything above applies unchanged. The daemon package is built against glibc
2.34, so it installs on RHEL 9 and derivatives as well as on Fedora; the
interface needs Fedora 42 or newer for libadwaita 1.7.

[releases page]: https://github.com/keepinfov/oxidom/releases

## NixOS

```nix
{
  inputs.oxidom.url = "github:keepinfov/oxidom";

  # in your system configuration:
  imports = [ inputs.oxidom.nixosModules.default ];

  programs.oxidom.enable = true;          # installs GUI + CLI
  programs.oxidom.trayAutostart = true;   # tray at login

  services.oxidom.enable = true;          # system daemon at boot
  services.oxidom.tun.enable = true;      # allow TUN interfaces
  services.oxidom.users = [ "alice" ];    # non-admins allowed to drive it
}
```

### `programs.oxidom` — the desktop side

| Option | Type | Default | Meaning |
|---|---|---|---|
| `programs.oxidom.enable` | bool | `false` | Installs **both** the GUI and CLI packages. |
| `programs.oxidom.package` | package | `oxidom-gui` | Which GUI package to install. |
| `programs.oxidom.trayAutostart` | bool | `false` | Adds a user unit running `oxidom-gui --background` with the graphical session, so the tray and the GNOME proxy toggle exist before a window is opened. |

### `services.oxidom` — the system daemon

| Option | Type | Default | Meaning |
|---|---|---|---|
| `services.oxidom.enable` | bool | `false` | Runs the daemon at boot, independent of any GUI session, on the system bus. |
| `services.oxidom.package` | package | `oxidom-cli` | The headless package the daemon runs from. |
| `services.oxidom.socksPort` | port | `10808` | Local SOCKS5 inbound. Passed on the command line, so it is **pinned** — clients cannot change it. |
| `services.oxidom.httpPort` | port | `10809` | Local HTTP inbound. Also pinned. |
| `services.oxidom.users` | list of string | `[]` | Accounts added to the `oxidom` group, i.e. allowed to drive the daemon. |
| `services.oxidom.tun.enable` | bool | `false` | Grants **only** the daemon `CAP_NET_ADMIN` and keeps NetworkManager away from `oxi-*` devices. Required for TUN and `oxidom run`. |

The unit is `Type=dbus`, so a client asking for the name starts it rather than
racing it, and `KillMode=process`, so a daemon crash does not drop every tunnel
with it.

### Running a profile at boot

The `oxidom@` template unit deliberately ships **without** an `[Install]` section,
so enable instances declaratively:

```nix
systemd.services."oxidom@work".wantedBy = [ "multi-user.target" ];
```

Remember the system daemon keeps its database in `/var/lib/oxidom`, not in your
home directory — see
[configuration.md § The two databases](configuration.md#the-two-databases).

## Nix without NixOS

```sh
nix run github:keepinfov/oxidom            # launch the GUI
nix profile install github:keepinfov/oxidom
```

In a clone:

```sh
nix build                  # packages.default — both binaries
nix build .#oxidom-cli     # headless only
nix build .#oxidom-gui     # graphical only
nix run .                  # the GUI
nix develop                # dev shell: GTK, rust, xray, tun2socks, nft
nix flake check            # builds and tests both packages
```

The flake exposes `packages.{default,oxidom,oxidom-cli,oxidom-gui}`,
`apps.default`, `checks.{cli,gui}`, `devShells.default`, `formatter`, and
`nixosModules.default`.

The Nix packages wrap the binaries so the core is found without any `PATH` setup:
the CLI gets `OXIDOM_XRAY_BIN`, `OXIDOM_TUN2SOCKS_BIN` and `OXIDOM_NFT_BIN`; the
GUI gets `OXIDOM_XRAY_BIN` and `OXIDOM_BIN`.

Supported systems are `x86_64-linux` and `aarch64-linux`. The flake also declares
`aarch64-darwin`, but oxidom is Linux-only in practice — it uses netlink, TUN
ioctls, nftables and systemd — so treat that attribute as vestigial.

## Arch

oxidom is **not on the AUR yet**, so build it from the `PKGBUILD` in this
repository:

```sh
git clone https://github.com/keepinfov/oxidom
cd oxidom/packaging/aur
makepkg -si
sudo systemctl enable --now oxidom.service
```

Then install an **Xray core**, which oxidom needs to connect but does not
install for you:

```sh
yay -S xray-bin        # or xray, or xray-git — any AUR provider will do
```

No AUR helper? Build one the same way:

```sh
git clone https://aur.archlinux.org/xray-bin.git && cd xray-bin && makepkg -si
```

Every provider of an Xray core lives in the AUR rather than the official
repositories, which is why it is an optional dependency here. Were it a hard
one, `makepkg -si` would stop at `target not found: xray` and — because pacman
cancels a transaction whole — install none of gtk4 or libadwaita either. oxidom
starts without a core and says so in a banner across the top; connecting and
checking latency both need one, since a direct latency check measures through a
core it starts for the purpose. Point oxidom at a core you installed by hand
under Settings → Xray core, or with `$OXIDOM_XRAY_BIN`.

The unit does not pin the proxy ports, so they come from
`/var/lib/oxidom/config.toml` and Settings can change them. On a machine where
several people drive the same daemon, pin them instead — moving an inbound moves
it for everyone:

```sh
sudo systemctl edit oxidom
```
```ini
[Service]
ExecStart=
ExecStart=/usr/bin/oxidom daemon --system --socks-port 10808 --http-port 10809
```

One more thing the package does not do for you:

- **TUN mode needs `tun2socks` and per-app routing needs `nftables`.** Both are
  listed as optional dependencies; install them if you use those features.
- **The Arch unit does not grant `CAP_NET_ADMIN`.** For TUN:

  ```sh
  sudo systemctl edit oxidom.service
  ```
  ```ini
  [Service]
  AmbientCapabilities=CAP_NET_ADMIN
  CapabilityBoundingSet=CAP_NET_ADMIN
  ```

To let a non-admin account drive the daemon: `sudo gpasswd -a alice oxidom`, then
log out and back in.

## From source

Requirements:

- **Rust 1.85 or newer.** The workspace is edition 2024 with resolver 3. There is
  no `rust-toolchain.toml`, so your system toolchain is used; if it is older than
  1.85, install [rustup](https://rustup.rs).
- **`pkg-config`** and a C toolchain.
- For the GUI only: **GTK 4** and **libadwaita ≥ 1.7** development packages.
  libadwaita 1.7 is the binding floor — it is where `AdwToggleGroup` arrives — and
  it in turn requires GTK ≥ 4.18.

```sh
git clone https://github.com/keepinfov/oxidom
cd oxidom

cargo build --release -p oxidom      # CLI + daemon only, no GTK needed
cargo build --release                # everything
```

Binaries land in `target/release/`. To install them properly, see
[Installing the assets by hand](#installing-the-assets-by-hand).

### Distro packages

**Debian 13 / Ubuntu 25.04+**

```sh
sudo apt install build-essential pkg-config rustc cargo \
                 libgtk-4-dev libadwaita-1-dev
```

**Fedora**

```sh
sudo dnf install gcc pkgconf-pkg-config rust cargo \
                 gtk4-devel libadwaita-devel
```

**Arch**

```sh
sudo pacman -S base-devel rust gtk4 libadwaita
```

## Tested distro matrix

What each distribution ships in its own repositories, measured in containers.
Building from source needs Rust ≥ 1.85 for the CLI and libadwaita ≥ 1.7 for the
GUI. The **package** columns are a different question: those binaries are built
elsewhere, so what matters is only whether the distribution can run them.

| Distribution | Rust | GTK4 | libadwaita | build CLI | build GUI | `oxidom` pkg | `oxidom-gui` pkg |
|---|---|---|---|:--:|:--:|:--:|:--:|
| Debian 13 (trixie) | 1.85.0 | 4.18.6 | 1.7.6 | ✅ | ✅ | ✅ | ✅ |
| Ubuntu 25.10 | 1.85.1 | 4.20.1 | 1.8.0 | ✅ | ✅ | ✅ | ✅ |
| Fedora 42 | not measured | | | | | ✅ | ✅ |
| Ubuntu 24.04 LTS | 1.75.0 | 4.14.5 | 1.5.0 | ❌ | ❌ | ✅ | ❌ |
| Debian 12 (bookworm) | 1.63.0 | 4.8.3 | 1.2.2 | ❌ | ❌ | ✅ | ❌ |
| AlmaLinux 9 / RHEL 9 | not measured | | | | | ✅ | ❌ |

The **build** columns are what the distribution ships in its own repositories.
The **pkg** columns are whether the published package installs and runs, which
is a weaker requirement and therefore true in more places: those binaries are
compiled against glibc 2.36 (`.deb`) and 2.34 (`.rpm`) with a rustup toolchain,
and glibc compatibility runs forward only. Every ✅ in the pkg columns is
checked on each change to the packaging, by installing the package in the
container that row names. The ❌ are not tested for failure; they follow from
the declared dependency on libadwaita 1.7, which those releases cannot satisfy
and the package manager therefore refuses.

The build columns for Fedora and the RHEL derivatives were never measured, and
the `oxidom-gui` package is built on Fedora 42, so anything older is untested
rather than known-bad. Arch is rolling and always current. Check yours before
filing a bug:

```sh
rustc --version
pkg-config --modversion libadwaita-1
```

**Debian 13 is exactly on the line** — it ships Rust 1.85.0, the minimum. Anything
older needs a newer toolchain.

### If your distribution is too old

The blocker is almost always Rust, and that is easy to fix:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup default stable
cargo build --release -p oxidom      # CLI, no GTK needed
```

That is enough for the **CLI and daemon** on any of the distributions above,
including Ubuntu 24.04 and Debian 12 — the headless build has no GTK dependency
at all.

The **GUI** additionally needs libadwaita ≥ 1.7, which rustup cannot supply. On
Ubuntu 24.04 LTS or Debian 12 your options are to upgrade the distribution, or to
use the [Nix package](#nix-without-nixos), which brings its own GTK stack and
works on any distribution.

## Getting an Xray core

oxidom needs an `xray` binary and will not start without one. **Most distributions
do not package it** — Nix and the AUR are the exceptions.

A few distributions do package one — Alpine (edge), Arch (via the AUR), Nix, and
Gentoo's GURU overlay — and Homebrew packages it on macOS. **Debian, Ubuntu,
Fedora, openSUSE and RHEL do not**, so most people download a release.

Settings › Xray core names whichever of these applies to your machine, including
the exact archive for your architecture, with buttons to copy the commands or open
the page.

By hand, on `x86_64`:

```sh
curl -LO https://github.com/XTLS/Xray-core/releases/latest/download/Xray-linux-64.zip
unzip -o Xray-linux-64.zip xray
sudo install -Dm755 xray /usr/local/bin/xray
xray version
```

On `aarch64` the archive is `Xray-linux-arm64-v8a.zip`; the release also publishes
32-bit, riscv64, ppc64le, mips and s390x builds, and `Xray-macos-64.zip` /
`Xray-macos-arm64-v8a.zip` for macOS. Pick the one matching `uname -m` — the
archive holds the binary alone, which is why the geo data below is a separate
step.

**Use 26.1 or newer** if you have any Hysteria2 servers — that is where the native
outbound landed, and an older core exits immediately rather than connecting.

If the binary is somewhere unusual, point oxidom at it with the `xray_binary`
setting or `$OXIDOM_XRAY_BIN` — see
[configuration.md](configuration.md#finding-helper-binaries).

### Getting the geo data

The core also needs **`geoip.dat` and `geosite.dat`**, and the release above does
not contain them — the Xray zip ships the binary alone. They are not optional and
they are not only for routing rules you might add later: every configuration
oxidom generates carries the built-in `geoip:private` and `geosite:private`
references, and a core that cannot load the lists **refuses to start at all**:

```
Failed to start: main: failed to load config files: [...]
  > infra/conf: failed to build routing configuration
  > infra/conf: invalid field rule
  > infra/conf: failed to load GeoIP: private
  > infra/conf: failed to open file: geoip.dat
```

Nix and the AUR's `xray-bin` both supply the files, so this bites exactly the
people who installed a core by hand. Install them where every Xray build looks,
which needs no environment variable:

```sh
curl -LO https://github.com/v2fly/geoip/releases/latest/download/geoip.dat
curl -Lo geosite.dat https://github.com/v2fly/domain-list-community/releases/latest/download/dlc.dat
sudo install -Dm644 geoip.dat   /usr/local/share/xray/geoip.dat
sudo install -Dm644 geosite.dat /usr/local/share/xray/geosite.dat
```

`geosite.dat` is published under the name `dlc.dat`; the core looks for it by the
former, so rename it as the command above does. Both releases publish a
`.sha256sum` beside the file if you want to check the download.

The core searches `$XRAY_LOCATION_ASSET`, then its own directory, then
`/usr/local/share/xray` and `/usr/share/xray`. To confirm a core can find its
data before trusting it with a connection:

```sh
printf '%s' '{"outbounds":[{"protocol":"freedom","tag":"direct"}],
  "routing":{"rules":[{"type":"field","ip":["geoip:private"],"outboundTag":"direct"}]}}' > /tmp/geo-test.json
xray run -test -c /tmp/geo-test.json     # "Configuration OK." means the data loaded
```

## Optional runtime dependencies

| Program | Needed for | Package |
|---|---|---|
| `tun2socks` | TUN interfaces | `tun2socks` |
| `nft` | `oxidom run` (per-app routing) | `nftables` |
| `gsettings` | the GNOME system-proxy toggle | `glib2` + `gsettings-desktop-schemas` |
| `ping` | ICMP latency probes | `iputils` |
| `systemd-run` | `oxidom run` | `systemd` |

The tray icon needs a **StatusNotifierItem host**. On stock GNOME that means an
AppIndicator extension; without one the window still works, there is just no icon.

## Installing the assets by hand

`cargo build` produces binaries only. A release build does **not** install its
icons — debug builds drop them into `$XDG_DATA_HOME` for convenience, release
builds do not. The full list, mirroring what the packages do:

| Source | Destination |
|---|---|
| `target/release/oxidom` | `/usr/bin/oxidom` (0755) |
| `target/release/oxidom-gui` | `/usr/bin/oxidom-gui` (0755) |
| `data/dev.keepinfov.oxidom.desktop` | `/usr/share/applications/` |
| `data/dev.keepinfov.oxidom.svg` | `/usr/share/icons/hicolor/scalable/apps/` |
| `data/dev.keepinfov.oxidom-symbolic.svg` | `/usr/share/icons/hicolor/symbolic/apps/` |
| `data/icons/oxidom-funnel-symbolic.svg` | `/usr/share/icons/hicolor/scalable/actions/` |
| `data/dev.keepinfov.oxidom.metainfo.xml` | `/usr/share/metainfo/` |
| `data/dev.keepinfov.oxidom.Daemon.conf` | `/usr/share/dbus-1/system.d/` |
| `data/dev.keepinfov.oxidom.Daemon.service` | `/usr/share/dbus-1/system-services/` |
| `packaging/systemd/oxidom.service` | `/usr/lib/systemd/system/` |
| `packaging/systemd/oxidom@.service` | `/usr/lib/systemd/system/` |
| `packaging/systemd/oxidom.sysusers` | `/usr/lib/sysusers.d/oxidom.conf` |

Country flags need no install step — they are compiled into the GUI binary. No
GSettings schema is shipped either; oxidom only *writes* to the stock
`org.gnome.system.proxy` schemas.

For the system daemon you also need the `oxidom` user, which the sysusers file
creates:

```sh
sudo systemd-sysusers
sudo systemctl daemon-reload
sudo systemctl enable --now oxidom.service
```

## Who may drive the system daemon

The system daemon rewrites the machine's proxy configuration and runs the core, so
its D-Bus policy is not an "any local user" surface. Only these may send to it:

- `root`
- members of `wheel`
- members of `oxidom`

Everyone else is denied — an unprivileged service account on the same machine
cannot redirect your traffic. Administrators need no setup. To let a non-admin
account drive it:

```nix
services.oxidom.users = [ "alice" ];   # NixOS
```

```sh
sudo gpasswd -a alice oxidom           # elsewhere
```

A user who is denied is not left broken: oxidom gives them a session daemon of
their own instead, with its own database.

---

Next: [quickstart.md](quickstart.md) · [configuration.md](configuration.md) · [troubleshooting.md](troubleshooting.md)
