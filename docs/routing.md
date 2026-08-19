# Routing: what actually carries your traffic

oxidom has three routing layers. They stack, and each is opt-in above the first.

| Layer | Privilege | What it moves |
|---|---|---|
| [Local SOCKS + HTTP](#local-proxies) | none | whatever you point at it |
| [GNOME system proxy](#gnome-system-proxy) | none | apps that honour the desktop proxy setting |
| [TUN interface](#tun-interface) | `CAP_NET_ADMIN`, system daemon only | depends on `routes` — by default, nothing |
| [Per-process routing](#per-process-routing) | none for the caller | exactly the commands you run under it |

Nothing here ever touches `/etc/resolv.conf`. oxidom does not rewrite your DNS.

## Local proxies

Every session always gets both a SOCKS5 and an HTTP inbound. No privilege, no
system changes — this is the whole of the unprivileged path.

Each profile binds its own loopback address so ports never collide between
sessions. `default` is pinned to `127.0.0.1` forever; other profiles get a stable
hashed `127.x.y.1`. Do not assume the address — ask:

```sh
oxidom env work
# export ALL_PROXY=socks5h://127.72.14.1:10808
# export all_proxy=socks5h://127.72.14.1:10808
# export HTTP_PROXY=http://127.72.14.1:10809
# …
```

```sh
eval "$(oxidom env work)"
curl https://ifconfig.me
```

Both inbounds sniff HTTP and TLS, UDP is enabled on SOCKS, and traffic to private
address space is routed direct rather than through the tunnel.

## GNOME system proxy

Turning on `system_proxy` makes the connected session take over the desktop's
proxy settings, so ordinary GNOME apps use the tunnel without being told about it.

It is applied by the **GUI**, not the daemon — it is a per-desktop user setting,
and a system daemon has no business writing it. Under the hood it is `gsettings`
on `org.gnome.system.proxy`, with `mode = manual` set **last** so the desktop
never sees a half-written configuration; any failure part-way rolls the whole
thing back.

Only one session may own it at a time:

```
the system proxy is already held by profile "work"
```

Disconnecting, or the core dying, releases it. If a GUI is killed outright, a
marker file lets the next start undo the proxy it left behind — a stuck system
proxy does not survive a restart.

This needs a GNOME session. Elsewhere it fails with
`running gsettings (is this a GNOME session?)`.

## TUN interface

A real network device, `oxi-<profile>`, with tun2socks moving packets into the
session's SOCKS inbound.

This is the **only** privileged part of oxidom, and it is available only from the
system daemon holding `CAP_NET_ADMIN`. oxidom will not escalate on its own:

```
profile 'work' asks for a network interface, but this daemon has no
CAP_NET_ADMIN. Interfaces are only available from the system daemon: enable
services.oxidom.enable together with services.oxidom.tun.enable. oxidom will
not escalate privileges on its own.
```

Enable it per profile:

```toml
[interface]
enable = true
routes = "manual"
```

and on the host — NixOS:

```nix
services.oxidom.enable = true;
services.oxidom.tun.enable = true;
```

Everything is derived and stable, so two profiles never collide: device
`oxi-<profile>` (within Linux's 15-byte name limit), a `/32` address from the
RFC 2544 benchmark block `198.18.0.0/16` (`default` is `198.18.0.1`), and a
routing table id, fwmark and rule priority that share one value — `0x6f00` for
`default`, then `0x6f01..0x6fff`, chosen to stay clear of the marks people
actually use.

The device is **persistent**: an ordinary `down` stops tun2socks and removes
oxidom's routes but leaves the device, so hand-written routes on it survive a
reconnect. `oxidom tun --down` removes it — but only if oxidom created it. A
device someone else made is never deleted:

```
refusing to delete network interface "oxi-work": oxidom did not create it
```

### What `routes` actually does

This is the setting people get wrong, so it is worth being blunt:

**`routes = "manual"`, the default, changes no system route at all.** The device
exists, tun2socks is running, and *nothing* goes through it except traffic
explicitly marked by [`oxidom run`](#per-process-routing). Enabling the interface
is not by itself a VPN.

| `routes` | Effect on the main routing table |
|---|---|
| `"manual"` | Nothing. Only `oxidom run` traffic is carried. |
| `"list"` | Each CIDR in `[interface] list` is routed via the device. |
| `"default"` | A host route to each server via the old gateway, plus `0.0.0.0/1` and `128.0.0.0/1` via the device — the classic split default. Everything goes through the tunnel. |

`routes = "default"` needs a current default IPv4 gateway, and `oxidom up` warns:

```
routes = "default" does not move the system resolver into the tunnel;
DNS may still use the existing network path
```

In all cases the profile's own private table also gets the current network's
connected routes, so your LAN and its resolver stay reachable from inside.

The interface is brought up only **after** the SOCKS inbound has passed its check —
a tunnel that cannot carry traffic never gets to capture your routes. If bring-up
fails, the session is torn back down rather than left half-applied.

## Per-process routing

`oxidom run` sends one command through a profile's interface, leaving every other
program on the ordinary route. It is for programs that ignore proxy environment
variables; for programs that honour them, `oxidom env` is simpler and needs no
interface at all.

```sh
oxidom run -- curl https://ifconfig.me
oxidom run --profile work -- ping 1.1.1.1
oxidom work run -- firefox
```

How it works: the command is launched in a transient `systemd --user` scope under
`oxidom-<profile>.slice`; the daemon owns one nftables rule per session that marks
packets from that cgroup; the mark selects the profile's routing table. A second
chain restores the mark on the reverse path, so reverse-path filtering does not
drop the replies.

Requirements — and if any is missing the command **refuses** rather than quietly
running unrouted:

- The profile has an interface, and it is up:
  `profile 'work' has no network interface. Use 'oxidom env work' for programs that honor proxy environment variables`
- A `systemd --user` manager is available (its socket in `$XDG_RUNTIME_DIR`).
- `systemd-run` and `nft` are on `PATH`.

Before `exec`, the child verifies its own cgroup is really inside the expected
slice. If systemd put it elsewhere, it refuses:

```
systemd put 'oxidom run' in cgroup "…", expected it below "…";
refusing to run the command outside the selected profile
```

The exit code is the child's, so this composes with ordinary shell logic.

Cleanup removes the profile's nftables chain *before* taking down its routing
domain, so traffic is never briefly released onto the ordinary default route on
the way down.

## When the core dies

The Xray process can exit on its own — a crash, an out-of-memory kill, a server
that dropped the connection. oxidom then **keeps** the tunnel's routes, its
fwmark rule and its hold on the desktop proxy setting until the session either
reconnects or you stop it.

That means traffic for the tunnel is *dropped* during the outage, not sent out
some other way. It looks like a dead network, and that is deliberate: the
alternative is every application quietly falling back to your ordinary
connection, with your own address and country, while the interface still shows a
tunnel that is reconnecting. A remote service notices that long before you do.

The session says so — the Sessions page marks it **holding traffic**, and
`oxidom status` shows the state as `holding` — because a network that is
deliberately dead must not be mistaken for a broken one.

Two ways to change it:

- **Settings → Hold traffic if Xray exits**, or `on_core_exit` in
  `config.toml`, sets the machine's answer.
- A profile's own `on_core_exit` overrides it, in the profile editor under
  Interface or in the profile file.

Turning it off restores the older behaviour: routes come down as soon as the
core does, and applications reach the internet directly until the tunnel is
back.

An explicit `down` — the switch, `oxidom down` — always releases. Asking for
your ordinary connection back is the one case where falling back to it is what
you meant.

---

Next: [profiles-and-pools.md](profiles-and-pools.md) · [troubleshooting.md](troubleshooting.md) · [architecture.md](architecture.md)
