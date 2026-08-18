# oxidom

A native Xray / v2ray client. This package carries the command line and the
daemon; `oxidom-gui` carries the GTK4 interface.

## An Xray core is required, and is not packaged here

No Debian, Ubuntu, Fedora or RHEL repository contains an Xray core, so this
package cannot install one for you. oxidom starts without one and says so;
connecting and checking latency both need it.

`oxidom status` names the exact download for this machine. By hand:

    curl -LO https://github.com/XTLS/Xray-core/releases/latest/download/Xray-linux-64.zip
    unzip Xray-linux-64.zip xray
    sudo install -Dm755 xray /usr/local/bin/xray

Use 26.1 or newer if you have Hysteria2 servers — that is where the native
outbound landed.

The core also needs `geoip.dat` and `geosite.dat`, which the release zip does
not contain. Every configuration oxidom generates references `geoip:private`
and `geosite:private`, and a core that cannot load them refuses to start at
all. oxidom can install them for you — see Settings, or `oxidom status`.

If the binary lives somewhere unusual, point oxidom at it with the
`xray_binary` setting or `$OXIDOM_XRAY_BIN`.

## The system daemon is not enabled by default

Installing this package does not start anything. That is deliberate: the system
daemon keeps its database in `/var/lib/oxidom` rather than in your home
directory, so enabling it changes which database is authoritative, and its
D-Bus policy restricts it to root, `wheel` and the `oxidom` group. A desktop
client does not need it — it starts a session daemon of its own, with its own
database under `~/.local/share/oxidom`.

To run it at boot, for every user on the machine:

    sudo systemctl enable --now oxidom.service
    sudo gpasswd -a alice oxidom        # let a non-admin drive it

The unit does not pin the proxy ports, so they come from
`/var/lib/oxidom/config.toml` and Settings can change them. On a machine where
several people drive the same daemon, pin them with `systemctl edit oxidom` —
moving an inbound moves it for everyone.

TUN interfaces additionally need `CAP_NET_ADMIN`, which this unit does not
grant. Add it with `systemctl edit oxidom.service`:

    [Service]
    AmbientCapabilities=CAP_NET_ADMIN
    CapabilityBoundingSet=CAP_NET_ADMIN

## Optional programs

| Program | Needed for |
|---|---|
| `tun2socks` | TUN interfaces |
| `nft` | `oxidom run` (per-app routing) |
| `gsettings` | the GNOME system-proxy toggle |
| `ping` | ICMP latency probes |
| `systemd-run` | `oxidom run` |

## Uninstalling

Removing the package leaves `/var/lib/oxidom` and the `oxidom` account behind,
on purge as much as on remove — that directory holds your subscriptions and
pinned certificates. Delete it by hand if you want it gone.

Full documentation: <https://github.com/keepinfov/oxidom>
