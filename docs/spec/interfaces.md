# Interfaces (binding)

The privileged TUN path: what it takes, what names and addresses it claims, and the order it is
brought up and torn down in.

`[interface] enable = false` is exactly the unprivileged proxy-only behavior. When enabled, the
system daemon (and only it) requires `CAP_NET_ADMIN`; the NixOS module grants it only with
`services.oxidom.tun.enable = true` and keeps `oxi-*` unmanaged by NetworkManager.

The daemon unit sets `KillMode=process` because the Xray cores, not the daemon, carry the
traffic. Under systemd's default the whole cgroup dies with the daemon, so a crash drops every
tunnel and the restarted daemon finds nothing to adopt — which silently made the pool-adoption
path in `recover()` unreachable in production. A clean stop still tears the cores down through
the daemon's own signal handler, and anything that leaks is reaped on the next start.

- Device names default to `oxi-<profile>` and fit Linux's 15-byte IFNAMSIZ payload; an explicit
  valid `device` is required for longer profile names.
- Device addresses are stable `198.18.<c>.<d>/32` values from the RFC 2544 benchmark block.
  `default` is fixed at `198.18.0.1`; `/32` is binding because it adds no connected route.
- Without owned TUN failover, fwmark, private table id and rule priority share a stable value.
  `default` is `0x6f00`;
  other profiles probe within `0x6f01..=0x6fff`, avoiding the user's `0x1`/`0x2`/`0x3` policy.
- Every enabled interface gets the current default network's link-scope connected routes plus
  `default dev <device>` in its private table, and a matching fwmark rule. The connected routes
  keep LAN and its resolver reachable from `oxidom run`. `routes = "manual"` changes no system
  route; legacy `list` adds only its CIDRs; `default` adds server host bypasses via the old
  gateway plus two half-defaults through the device. Owned failover's union is specified below.
- Legacy bring-up order is persistent TUN, address `/32`, tun2socks spawn, link-up, private route/rule,
  then system routes. The spawn-before-link order and double-dash tun2socks flags are live-tested
  contracts.
- Without owned TUN failover, ordinary `down` stops tun2socks and removes oxidom routes/rule
  but leaves the persistent device,
  preserving hand-written routes across reconnects. `tun --down` and crash recovery additionally
  delete the device only when oxidom created it.
- **A bound `oxidom run` cgroup is blocked, not released (binding).** The cgroup a profile's
  `oxidom run` commands belong to is recorded in the state file, independently of the session that
  installed it, and its nftables chain is turned into a drop for every family whenever no tunnel
  carries it: a switch that re-plans the interface, a start that fails halfway, a core that exits
  under `on_core_exit = "hold"`, a failed automatic reconnect, `down`, `tun --down`, and a daemon
  restart that finds the scope still running. The chain becomes a mark again only when a tunnel
  carries the cgroup. It is released when the profile's policy is `on_core_exit = "release"`, when
  the scope has ended, or when the daemon that restarts finds no cgroup at all — so a chain is
  never left blocking a name. `down` removes the session; it does not remove that record.
- **A core exiting is not a `down` (binding).** When the Xray process of a session exits by itself
  — or a live core stops answering and the watchdog revives it — the session keeps its routes, its
  fwmark rule and its ownership of the desktop proxy setting until it is either reconnected or
  explicitly taken down. Traffic aimed at the tunnel is dropped for that window rather than
  released onto the ordinary default route, where it would leave with the machine's own address
  while the interface still showed a tunnel reconnecting. This is the shipped default,
  `on_core_exit = "hold"`; `"release"` reproduces the teardown-first behaviour, and a profile's
  own setting wins over the machine's. The answer is snapshotted when the session comes up, so a
  profile edited mid-session does not change what happens to the tunnel already running.
  The hold ends when a new core has been confirmed to carry traffic — a core that starts and then
  fails its confirmation is not a confirmation, and a failed one leaves the session holding again
  rather than tearing down the routes the hold is made of — or when the session is explicitly
  taken down under a policy of release.
- A reconnect under a held interface **adds nothing**: `start_interface` finds it already up and
  returns. Its tun2socks is the exception — the one part of an interface that is a process rather
  than kernel state — and is restarted alone if it did not survive the outage, leaving every route
  where it was. A device left up with nothing servicing it black-holes the tunnel for good, which
  is worse than the release this holding prevents.
- Per-process routing uses a transient `systemd --user` scope below
  `oxidom-<profile>.slice`. The daemon atomically owns one `socket cgroupv2` mark rule per session
  in `table inet oxidom`; the CLI verifies `/proc/self/cgroup` inside the scope before `exec`.
  Cleanup removes the profile chain before taking down its routing domain, so traffic is never
  silently released onto the ordinary default route.
- **IPv6 is not tunnelled, and a marked cgroup cannot reach it (binding).** The fwmark rule and
  the private table are IPv4: `plan_routes` plans `Ipv4Addr` routes and `Net::rule_add` adds a
  `.v4()` rule, so an IPv6 packet from a marked cgroup would keep its mark and be routed by the
  ordinary table. The profile's own chain therefore refuses IPv6 from that cgroup — in the same
  atomic transaction as the mark, and after accepting `fe80::/10` and `ff00::/8`, which carry
  neighbour discovery and no traffic anywhere. A connection from `oxidom run` to an IPv6-only
  destination fails rather than leaving the machine directly; `oxidom env` is unaffected.

## Owned TUN failover (binding)

Explicit saved-profile failover may use an enabled interface only with effective
`on_core_exit = "hold"`, no custom profile `routing`, and 2..64 exact authorized candidates all
inside one bounded envelope. The entire set is checked before the initial start, not just the
first candidate. Supported typed outbounds are VLESS, VMess, Trojan, Shadowsocks, SOCKS, HTTP and
Hysteria2 with a nonzero port and a literal unicast IPv4 dial address. Refuse domains, IPv6,
0/8, loopback, link-local, multicast/reserved high ranges and the limited broadcast address.
Refuse composite Xray profiles, raw outbound patches, Hysteria2 port hopping, XHTTP extras and
ECH. Stream transports are empty/default, `tcp`, `ws`, `grpc`, `xhttp`, `splithttp` or `h2`;
security is empty/default, `none`, `tls` or `reality`. TLS/HTTP names are not dial addresses.

The envelope pins each ID's IPv4 address and protocol/transport/security shape, and the requested
interface configuration. Credentials, TLS/HTTP names and other supported current settings are
read from the current catalog, not frozen copies. Revalidate the whole catalog envelope and
effective hold policy before automatic admission. Missing/ambiguous IDs, address/shape changes,
interface changes or release policy revoke that activation; restoring them does not rearm it.

Only one TUN failover interface may exist in the engine. Admission refuses any other configured
TUN, and conversely refuses another TUN while the envelope is retained, including held or revoked
state. Proxy-only sessions may coexist. Refuse an existing device rather than attaching to it;
an existing same-profile interface requires explicit deletion before activating this mode.

`manual` installs no main-table routes. `list` installs a deduplicated `/32` bypass union for
all authorized IPv4 addresses via the original default gateway, then the capture CIDRs in main
table 254; it therefore requires that gateway. A capture `/32` equal to a candidate address is
refused. `default` installs that union before the two half-default captures. All bypasses precede
the private-table rule as well as main captures. The private table contains connected routes and
its TUN default, **not** the candidate bypass union: marked application traffic must not acquire
a direct candidate escape. Existing connected-LAN exceptions remain scoped as above.

Owned interfaces use random mark/private-table identity and a separately derived priority, not
the stable profile routing values above. Routes use protocol 242 plus a random nonzero metric;
rules use protocol 242 and a full fwmark mask. Device deletion requires matching name, alias and
ifindex. The exact durable intent and downgrade rules are in
[storage.md](storage.md#owned-tun-recovery-state-binding). Creation is kernel-exclusive and
initially transient. Persist complete qualified intent before persistence, address or routing
mutation; then configure address, spawn/persist helper, bring link up, install bypasses, private
routes/rule and captures. Existing resources are never adopted as proof of this activation's
ownership. Preflight and final checks immediately before capture refuse colliding prefixes,
private tables, marks and priorities, exempting only this owner's exact intents. Netlink has no
atomic multi-object transaction: arbitrary privileged writers must coordinate administratively;
these checks are not atomic exclusion against another administrator.

Automatic alternate starts keep the same device, route plan, bypass union and routing identity.
Do not tear down or re-plan between attempts. The hold remains across failed confirmation,
switches, exhaustion, revocation and removal of the current catalog member; no direct standby or
legacy infinite retry may take over. Explicit Stop removes the owned device as well as its routes
and rule, unlike legacy persistent-device retention. Partial cleanup or failed durable forgetting
retains dirty ownership and blocks restart/reuse until cleanup succeeds.

A local helper failure is not remote-candidate evidence. Restore only tun2socks, persist its PID
and check immediate child liveness. This is not a sustained-readiness guarantee.
One replacement is allowed after the initial helper; repeated failure requires
explicit teardown. Local failure disarms recovery while retaining the hold. Neither this bounded
process observation nor HTTP through SOCKS continuously proves TUN forwarding. Protection is only
the configured IPv4 route/cgroup scope, not all device traffic or DNS/IPv6 containment.

Source implementation does not mark P12 complete. P14/P15 validation remains required; no VM
packet-path, crash-recovery or leak measurement is established by this contract text.
