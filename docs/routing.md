# Routing: what actually carries your traffic

oxidom has three routing layers. They stack, and each is opt-in above the first.

| Layer | Privilege | What it moves |
|---|---|---|
| [Local SOCKS + HTTP](#local-proxies) | none | whatever you point at it |
| [GNOME system proxy](#gnome-system-proxy) | none | apps that honour the desktop proxy setting |
| [TUN interface](#tun-interface) | `CAP_NET_ADMIN`, system daemon only | depends on `routes` — by default, nothing |
| [Per-process routing](#per-process-routing) | none for the caller | exactly the commands you run under it |

Nothing here ever touches `/etc/resolv.conf`. oxidom does not rewrite your DNS.

Where traffic goes *inside* the tunnel is decided by the core's routing rules;
a profile can carry [rules of your own](#rules-of-your-own).

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
proxy settings for applications that honor them. Applications ignoring those settings are
unaffected. This is not whole-device traffic capture; a TUN interface captures traffic according
to its configured routes, not merely because it exists.

It is applied by the **GUI**, not the daemon — it is a per-desktop user setting,
and a system daemon has no business writing it. Under the hood it is `gsettings`
on `org.gnome.system.proxy`, with `mode = manual` set **last** so the desktop
never sees a half-written configuration; any failure part-way rolls the whole
thing back.

Only one session may own it at a time:

```
the system proxy is already held by profile "work"
```

Disconnecting releases it. After core failure, a held session retains the proxy arrangement
until reconnect or explicit Stop; release policy permits removal. Logical daemon ownership does
not independently prove the desktop setting was applied. If a GUI is killed outright, a
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

Without owned TUN failover, routing identities are derived and stable: device
`oxi-<profile>` (within Linux's 15-byte name limit), a `/32` address from the
RFC 2544 benchmark block `198.18.0.0/16` (`default` is `198.18.0.1`), and a
routing table id, fwmark and rule priority that share one value — `0x6f00` for
`default`, then `0x6f01..0x6fff`, chosen to stay clear of the marks people
actually use. Owned failover uses randomized mark/table identity and a separately derived rule
priority instead; do not depend on the stable values for that mode. Existing resource conflicts
are refused, not proof that a route belongs to oxidom.

The legacy device is **persistent**: an ordinary `down` stops tun2socks and removes
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
| `"default"` | A host route to each server via the old gateway, plus `0.0.0.0/1` and `128.0.0.0/1` via the device: split-default IPv4 capture, subject to more-specific routes and existing direct/private exceptions. Not IPv6 or resolver containment. |

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

### Owned TUN failover

[Saved-profile failover](profiles-and-pools.md#sequential-failover) supports a bounded TUN mode,
not arbitrary endpoint or route changes. It requires effective `on_core_exit = "hold"`, no
custom profile `routing`, and 2..64 exact authorized IDs with supported typed literal unicast
IPv4 dial endpoints. Supported protocols are VLESS, VMess, Trojan, Shadowsocks, SOCKS, HTTP and
Hysteria2. Stream transports are default/TCP, WebSocket, gRPC, XHTTP/splithttp and h2, with
default/none, TLS or REALITY security. Domains, IPv6, loopback/link-local/nonunicast dial
addresses, composite profiles, raw outbound patches, XHTTP extras, ECH and port hopping are
refused before starting the initial candidate. TLS/HTTP names may still be names; they are not
the dial endpoint. Every candidate must remain in the original address/shape envelope.

Only one such TUN may exist, with no other configured TUN in the same engine. Starting failover
beside an existing TUN, or starting another TUN beside retained failover ownership, is refused.
Proxy-only sessions remain allowed. A pre-existing device is refused instead of adopted.

The route modes keep distinct scopes:

- `manual` changes no main-table route. Capture still needs an explicit mark or administrator
  route; merely enabling failover is not a VPN.
- `list` first installs deduplicated `/32` main-table bypasses for every authorized IPv4
  candidate via the original default gateway, then its listed capture networks. It needs a
  default gateway and refuses a capture `/32` equal to a candidate address.
- `default` installs the same candidate union before the two IPv4 half-default captures.
- The private marked-application table gets connected routes and its TUN default, never the
  candidate bypass union. The union must not give `oxidom run` a direct candidate escape.

The daemon exclusively creates a transient device, stamps its alias/index identity and durably
records complete cleanup intent before making it persistent or adding addresses/routes/rules.
Owned routes carry protocol 242 and a random metric; rules carry protocol 242, full mark mask,
randomized mark/table and derived priority. Checks before capture refuse routing conflicts,
but netlink cannot atomically reserve the whole plan against arbitrary privileged writers.
Coordinate other route managers/administrators; do not treat protocol 242 as a security token.

Automatic switches reuse the device, bypass union, private domain and captures. They do not
tear down the hold between candidates. Failed confirmation, exhaustion, revocation and catalog
removal of the current member keep assigned traffic held and direct standby off. Explicit Stop
removes the owned device as well as its routes/rule; legacy ordinary disconnect still leaves
its persistent device. Cleanup failures retain ownership, block reuse and require resolution,
not deleting `state.toml`. [Downgrade only after new-binary cleanup](installation.md#downgrading-after-owned-tun-use).

Helper restoration is local, not a reason to blame or advance a remote server. A new helper
is checked for immediate child liveness and its PID is persisted. One automatic replacement
is allowed; repeated helper failure requires explicit teardown. These checks and HTTP confirmation
through SOCKS are not continuous proof of packet forwarding through TUN. This remains scoped
IPv4 capture with connected/private exceptions, no resolver rewrite and no whole-device promise.
P12 is not complete merely because the source implements this path: P14/P15 validation remains
required, and no VM packet-path or leak measurement is claimed here.

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

### What a switch does to a command that is already running

An `oxidom run` command is inside a scope that outlives the session which
created it, so it cannot be asked when the tunnel is going away. Switching a
profile — or bringing a profile up while its scope is still busy — therefore
does not release it: the daemon turns the profile's chain into a **block**, and
the command's packets are dropped until the new tunnel marks them again.

The same protection covers a tunnel that fails to start, a core that dies
(`on_core_exit = "hold"`, the default), a failed automatic reconnect, and
`oxidom down` while a command is still running. `oxidom status` reports such a
session as holding its traffic rather than carrying it.

The block is not permanent. It ends when a tunnel for the profile carries the
cgroup again, when the command's scope ends, or — for a profile set to
`on_core_exit = "release"` — immediately, since that setting is a profile saying
it wants no protection at all.

### IPv6

`oxidom run` carries IPv4. The mark it sets selects a table, that table holds
IPv4 routes, and the rule pointing at it is an IPv4 rule — so an IPv6 packet
would keep the mark and then be routed exactly like any other, leaving by the
machine's own address while the IPv4 half of the same program went through the
tunnel.

Rather than leave that to be noticed later, the profile's chain refuses IPv6
from a marked cgroup. A program that only speaks IPv6 to a destination fails
inside `oxidom run`, and says so, instead of quietly using your real address.
Link-local and multicast are excepted, so neighbour discovery keeps working, and
the LAN reachability the private table provides is unchanged for IPv4.

`oxidom env` is not affected: it exports IPv4 proxies, and a program that honours
them makes its own choices.

## Rules of your own

The three layers above decide *what* reaches the tunnel. Where a given request
goes once it is inside is the core's routing table, and oxidom writes two rules
into it: private addresses go direct, and, for a pool, everything else goes to
the balancer.

A profile can carry an Xray `routing` block of its own, written into the profile
file and spliced **ahead** of those two — so a rule you write wins over the one
oxidom installs:

```toml
# ~/.config/oxidom/profiles/work.toml
routing = '''
{
  "rules": [
    { "domain": ["geosite:category-ads-all"], "outboundTag": "block" },
    { "ip": ["geoip:ch"], "outboundTag": "direct" }
  ]
}
'''
```

That is advertising blocked and one country direct, with everything else still
going through the tunnel. The syntax is Xray's own, so its documentation is the
reference; `geosite:` and `geoip:` names come from the data files the daemon
installs.

Three outbound tags exist to send traffic to:

| `outboundTag` | Where it goes |
|---|---|
| `proxy` | the tunnel — **only on a profile that selects one server** |
| `direct` | out of the machine normally, not through the tunnel |
| `block` | nowhere |

A profile running a **pool** has no `proxy` outbound: its members are reached
through the balancer, and a rule aimed at `proxy` is refused rather than
producing a core that will not start. Send those to `direct` or `block`, and
leave everything else to fall through to the balancer.

Four things are refused, when the profile is saved and again when it is brought
up, with the reason in the message:

- `balancers`, or a rule with a `balancerTag` — oxidom builds the balancer, and a
  selector from elsewhere can resolve to an outbound that leaves the tunnel;
- `domainStrategy` — set it as `domain_strategy` under `[core]`, which owns it;
- an `outboundTag` naming anything but the three above;
- a rule with no `outboundTag`, which decides nothing.

There is no editor for this in the interface. The profile dialog shows how many
rules the block holds and writes it back untouched, the way it already treats a
group's membership — so editing a profile from the interface cannot lose the
rules, and the file is where you change them.

**A subscription's own routing is still discarded on import.** Keeping a
provider's block means mapping its outbound tags onto oxidom's, and modelling
rules properly is a separate piece of work; until then this block is how you say
the same thing yourself.

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
