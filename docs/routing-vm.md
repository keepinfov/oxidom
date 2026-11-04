# Isolated routing VM checks

The Linux x86-64 `routing-faults` flake check boots two disposable NixOS guests.
It uses the installed CLI, the repository's system-daemon module, real Xray
26.3.27 and real tun2socks. It is separate from fake-core lifecycle tests and
from the session-daemon module evaluation check.

Run it in an isolated CI runner with KVM available:

```sh
nix build .#checks.x86_64-linux.routing-faults --print-build-logs
```

The normal build workflow also includes it through `nix flake check`. A runner
without KVM fails the prerequisite rather than silently skipping the test.
Builds and VM execution for tracker #216 run in CI; source inspection and
formatting can run locally.

## Topology and isolation

The client is `10.42.0.2/24`, with its only IPv4 default through the peer at
`10.42.0.1`. The peer terminates SOCKS candidate endpoints `192.0.2.10` and
`192.0.2.11`, and an HTTP sink at `203.0.113.10`; all three are off the client's
connected subnet. Neither guest obtains a management-network DHCP address, and
the peer has no default route or IP forwarding. The VM driver's serial backdoor
does not carry the HTTP packets being tested. Addresses and imported entries
are invented; no real subscription, provider or Internet endpoint is used.

`CAP_NET_ADMIN` is granted to the system service, whose daemon's effective set
is checked. CLI and public D-Bus mutations run as an ordinary guest user with
no effective capabilities, admitted by the packaged group policy. This fixture
does not claim capability removal from the daemon's helper children. The peer's
real Xray and HTTP service have no capabilities. The daemon explicitly selects
the pinned Nix Xray/tun2socks paths; no runtime core or geo download, desktop
proxy or direct standby is enabled. The roots, devices, buses and routes belong
to disposable guests, never to the developer's machine.

Documentation addresses may be classified as private by an installed geo list.
That could route the sink through Xray's `direct` outbound while an HTTP-only
test reported success. This fixture therefore supplies an explicit tiny
protobuf geo dataset: `PRIVATE` contains only `10.0.0.0/8`, and its geosite entry
is the invented `p15-lan.invalid`. The production `geoip:private` rule is kept
unchanged, and a positive LAN request through the same Xray verifies its direct
behavior. The fixture tests routing mechanics with that declared classification;
it does **not** validate the packaged geo dataset or its geographic coverage.

## Foundation assertions

For each of `routes = "list"` and `routes = "default"`, the fixture explicitly
saves and activates an owned failover envelope with both canonical candidates.
It checks the interface alias/index, qualified protocol-242 route/rule identity,
durable selectors and the full candidate bypass union in the main table. Marked
candidate traffic must resolve through the private TUN table without those
main-table bypasses.

A direct baseline proves that the sink is reachable over the data NIC before
capture. After capture, a fresh HTTP canary must reach the real sink through
candidate A. The same closed packet capture must contain its bytes in A's SOCKS
stream and contain no direct TCP packets to the sink HTTP endpoint. The peer's
actual inbound access log and HTTP receipt are additional evidence. The capture
is observed ready before the canary. Immediate packet delivery and per-packet
pcap flushing are enabled; a bounded gate observes that same canary in the file
before capture is stopped. The closed, flushed file is then checked again for
both assertions. An empty capture cannot satisfy the positive requirement.

The capture filter and negative assertion cover TCP to that HTTP endpoint;
they do not establish UDP or ICMP containment.

Explicit Stop must remove the owned device, protocol-qualified routes/rule and
recovery session, and terminate both core and helper before VM teardown. A new
direct request then demonstrates the deliberate release boundary of Stop.

This is the first P15 slice. It does not establish core/helper/daemon fault
behavior, nft/cgroup enforcement, IPv6 containment, ownership collision recovery,
exhaustion, revocation, packet-loss bounds, continuous or whole-device protection.
Those require the remaining isolated fault matrix and its recorded CI results.
