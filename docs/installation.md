# Installation

oxidom ships two binaries:

- **`oxidom`** — the CLI and the daemon. No GTK dependency at all, so it installs
  cleanly on a server.
- **`oxidom-gui`** — the GTK4 / libadwaita interface. Needs a desktop.

oxidom downloads its pinned, verified **Xray core** automatically on first use.

## Contents

- [Verifying a download](#verifying-a-download)
- [From the package repository](#from-the-package-repository)
- [Debian and Ubuntu, from a downloaded file](#debian-and-ubuntu-from-a-downloaded-file)
- [Fedora, RHEL and derivatives](#fedora-rhel-and-derivatives)
- [AppImage](#appimage)
- [NixOS](#nixos)
- [Nix without NixOS](#nix-without-nixos)
- [Arch](#arch)
- [From source](#from-source)
- [Tested distro matrix](#tested-distro-matrix)
- [Getting an Xray core](#getting-an-xray-core)
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

## From the package repository

The least work, and upgrades arrive with the rest of the system.

**Debian, Ubuntu:**

```sh
curl -fsSL https://keepinfov.github.io/oxidom/KEY.gpg \
  | sudo gpg --dearmor -o /usr/share/keyrings/oxidom.gpg
echo "deb [signed-by=/usr/share/keyrings/oxidom.gpg] https://keepinfov.github.io/oxidom/deb stable main" \
  | sudo tee /etc/apt/sources.list.d/oxidom.list
sudo apt update
sudo apt install oxidom-gui        # the interface; pulls in the daemon
```

**Fedora, RHEL:**

```sh
sudo curl -fsSL https://keepinfov.github.io/oxidom/oxidom.repo \
  -o /etc/yum.repos.d/oxidom.repo
sudo dnf install oxidom-gui
```

On a server, install `oxidom` instead — the daemon and CLI, with no GTK
dependency at all.

**Or one line.** It detects apt or dnf and runs exactly the commands above,
printing each one before it does:

```sh
curl -fsSL https://keepinfov.github.io/oxidom/install.sh | sh
curl -fsSL https://keepinfov.github.io/oxidom/install.sh | sh -s -- --server
```

Before trusting it, read it: it is
[`packaging/install.sh`](../packaging/install.sh) in this repository, served from
the same host as the key it verifies. It checks the downloaded key against the
fingerprint pinned in its own source —

```
05BC 9AA4 B90F F65A CE7F  AE1C 74FE 48BE 84CA 2CCF
```

— and refuses to install if they disagree, rather than importing whatever
arrives. The same value is published as
[`KEY.fingerprint`](https://keepinfov.github.io/oxidom/KEY.fingerprint) beside
the key, so the pin can be checked against the repository as well as against
this page. It does **not** enable the system daemon: which daemon runs decides
where the database lives, and that is not a decision an installer gets to make
quietly.

The repository is signed, and the packages in it are the same ones attached to
the release. Only full releases appear; release candidates do not, because a
repository is what a package manager upgrades to without being asked.

## Debian and Ubuntu, from a downloaded file

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
On an older release, install `oxidom` alone and use the CLI, or take the
[AppImage](#appimage), which brings its own GTK stack.

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


## AppImage

For a desktop whose distribution is too old for the `oxidom-gui` package — most
of all **Ubuntu 24.04 LTS** and **Debian 12**, whose libadwaita is 1.5 and 1.2
against a floor of 1.7.

```sh
chmod +x oxidom-*-x86_64.AppImage
./oxidom-*-x86_64.AppImage
```

About 45 MB. It carries its own GTK, libadwaita, icon theme **and glibc**, so
the host's versions do not matter, and it carries an Xray core and the `oxidom`
daemon binary — nothing else to install. It needs no root and no special kernel
permission, which is why it works on Ubuntu 24.04, where a bundle that mounted
itself through a user namespace would not.

Two things to know:

- **There is no system daemon.** An AppImage installs nothing, so it cannot
  place a systemd unit or a D-Bus policy: it runs a session daemon of its own,
  with its own database under `~/.local/share/oxidom`. Local SOCKS and HTTP
  proxies and the GNOME system-proxy toggle work. TUN interfaces and `oxidom
  run` do not — both need `CAP_NET_ADMIN`, which only the system daemon can
  hold. For those, install the `.deb` or `.rpm`.
- **Where the packages install, prefer them.** They are a few megabytes, they
  update with the system, and they can run the daemon at boot.

If it will not start, your system may lack a usable FUSE. Run it without:

```sh
./oxidom-0.1.0-x86_64.AppImage --appimage-extract-and-run
```

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

The first connection downloads oxidom's verified Xray 26.3.27 release without an
AUR package or administrator privileges. An AUR core is useful only as an offline
fallback when it reports that exact version; configure it under Settings → Xray
core or with `$OXIDOM_XRAY_BIN`.

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

oxidom manages its tested Xray release automatically. On first use it downloads
**Xray 26.3.27** into its private data directory, checks the release archive against
a SHA-256 digest pinned in oxidom's source, and extracts the binary plus its geo
data. No package manager or administrator privileges are needed.

The managed release is available on Linux `x86_64` and `aarch64`. An offline
machine may instead use `xray_binary`, `$OXIDOM_XRAY_BIN`, or `xray` on `PATH`, but
oxidom accepts those only when `xray version` reports exactly `Xray 26.3.27`.
This prevents an untested core release from silently changing generated-config
semantics. See [configuration.md](configuration.md#finding-helper-binaries).

### Getting the geo data

The core also needs **`geoip.dat` and `geosite.dat`**. The managed release installs
them beside the binary. They are not optional and they are not only for routing rules you might add later: every configuration
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
