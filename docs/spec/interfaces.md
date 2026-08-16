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
- fwmark, private table id and rule priority are the same stable value. `default` is `0x6f00`;
  other profiles probe within `0x6f01..=0x6fff`, avoiding the user's `0x1`/`0x2`/`0x3` policy.
- Every enabled interface gets the current default network's link-scope connected routes plus
  `default dev <device>` in its private table, and a matching fwmark rule. The connected routes
  keep LAN and its resolver reachable from `oxidom run`. `routes = "manual"` changes no system
  route; `list` adds only its CIDRs; `default` adds a host route to the server via the old gateway
  plus two half-defaults through the device.
- Bring-up order is persistent TUN, address `/32`, tun2socks spawn, link-up, private route/rule,
  then system routes. The spawn-before-link order and double-dash tun2socks flags are live-tested
  contracts.
- Ordinary `down` stops tun2socks and removes oxidom routes/rule but leaves the persistent device,
  preserving hand-written routes across reconnects. `tun --down` and crash recovery additionally
  delete the device only when oxidom created it.
- Per-process routing uses a transient `systemd --user` scope below
  `oxidom-<profile>.slice`. The daemon atomically owns one `socket cgroupv2` mark rule per session
  in `table inet oxidom`; the CLI verifies `/proc/self/cgroup` inside the scope before `exec`.
  Cleanup removes the profile chain before taking down its routing domain, so traffic is never
  silently released onto the ordinary default route.
