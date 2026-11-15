# Isolated scoped routing VM check

The `scoped-routing` check uses two disposable Linux guests, real Xray 26.3.27,
tun2socks and nftables. It exercises the installed CLI and system-daemon module
with a real systemd user manager. Source readiness is separate from execution:
the tracker and pull request must record the accepted CI head and actual result.

The check is intended to be imported with `self`, `pkgs`, `lib` and
`oxidom-cli`, alongside the existing x86-64 Linux routing check. Its standalone
command on a disposable KVM builder is:

```sh
nix build .#checks.x86_64-linux.scoped-routing --print-build-logs
```

Tracker #216 builds and runs this only in CI. Adding the source does not claim
that the command, user scope, kernel expressions or packet assertions passed.

## Topology and scope

The client has `10.42.0.2/24` and `2001:db8:42::2/64`; the peer has the matching
`.1` and `::1` addresses. Only the client has an IPv4 default route, through the
peer. An explicit IPv6 host route reaches the peer's
`2001:db8:113::10` HTTP endpoint. IPv6 is enabled and is proven usable by an
unscoped request. Management DHCP, router advertisements and autoconfiguration
are disabled; QEMU user-network forwarding is restricted. The peer has neither
an external default nor IP forwarding.

The peer also terminates invented SOCKS endpoints `192.0.2.10` and
`192.0.2.11` and the IPv4 HTTP endpoint `203.0.113.10`. These addresses are
outside the client's connected subnet. The real upstream Xray forwards the
requests; its HTTP service reports the actual socket peer and unique path.
Explicit loopback-address service dependencies precede both listeners.
HTTP requests keep the literal target address and port in their Host header
(with brackets for IPv6), so production HTTP sniffing does not introduce a
hostname or require fixture DNS.

As in the routing foundation, a declared tiny protobuf geo fixture classifies
only `10.0.0.0/8` as private and names the invented private geosite
`p15-lan.invalid`. The generated private-address rule is preserved. The check
does not validate packaged geo datasets or geographic classifications.

The profile uses `routes = "manual"`, so ordinary requests remain on the
ordinary route. Marked traffic must use the private TUN table, including a
request directed at an authorized candidate's HTTP port. The main-table
candidate route cannot become a marked application's escape.

Only the system service is granted `CAP_NET_ADMIN`; the test checks the scoped
actor's UID and zero effective capabilities. It does not claim that the service
removes ambient capabilities from all of its helper children. The client uses
strict reverse-path defaults. The check separately reads the TUN-specific
`src_valid_mark = 1` and `rp_filter = 2` values configured by the engine; it does
not describe that TUN policy as strict filtering.

## One persistent application scope

A real `tester` user manager and private runtime directory are available before
the launcher starts. A transient **user** service invokes the installed
`oxidom run --profile scoped -- ...`. The CLI creates its ordinary user scope,
validates it, registers the cgroup through the public daemon and executes the
actor. A private Unix channel controls the actor; no scope is recreated merely
to make a later check pass.

The actor reports its PID, UID, effective capabilities and actual unified
cgroup. The observer independently reads that cgroup and its filesystem inode.
The durable record must identify the exact escaped profile slice at level 4.
The PID, cgroup and slice inode remain the same through core loss, Down and Up.

The saved owned envelope is explicitly disabled after each confirmed Up. That
retains its installed routing while giving core-death assertions a stable hold
checkpoint; neither an alternate attempt nor legacy reconnect may race that
observation. Only a later explicit enable followed by Up starts a new
activation.

Recovery in this check deliberately uses Down followed by Up, which creates a
new interface while the application scope remains alive. It does not establish
mark restoration during automatic A-to-B recovery on a retained interface;
that bound-scope transition needs its own P15 evidence.

## Packet and nftables evidence

Fresh IPv4 sockets from the scope must appear inside the expected candidate's
real SOCKS TCP stream and reach the real HTTP service. Ordinary IPv4 controls
demonstrate the independent direct path. The actual JSON nftables rules must
match the recorded cgroup and current mark; mark, conntrack-mark and prerouting
restore counters must advance with the scoped request. The profile chain also
carries a rule that restores the same exact conntrack mark on output, so a
kernel-generated reset for a flow whose socket has gone still leaves through
the tunnel instead of the ordinary route.

An unscoped global IPv6 request succeeds. The same scoped request must fail,
advance the profile's IPv6 reject counter and produce neither a sink receipt
nor direct TCP to that endpoint on the data NIC. A distinct scoped link-local
request succeeds and advances the exception counter without advancing the
global IPv6 rejection counter. Multicast and neighbour-discovery behavior are
not separately measured by this check.

Core loss and explicit Down each run while the same actor produces a finite,
bounded series of fresh IPv4 sockets. Explicit Unix-channel gates and guest
monotonic timestamps bracket the operation and a later stable observation.
The initial gate requires a successful unique window canary, whose payload must
already appear in the candidate SOCKS stream in both live captures before the
operation. The producer must still complete fresh requests started after the
stable observation; an
already-finished sequence cannot stand in for transition traffic. The stable
chain must drop the cgroup, preserve its durable record and reject both IPv4 and
IPv6. Later Up restores marking and forwarding for the original actor.

Both data-NIC and all-interface packet captures require distinct positive
IPv4/IPv6 controls. Capture uses immediate mode, and positive canary bytes must
be readable from the live pcap before it is stopped. Closed captures are
analysed again; successful termination and zero kernel drops are required.
The NIC capture rejects every direct TCP packet to the scoped HTTP endpoints,
including during the recorded request windows. nft drop counters and actor
attempts provide source evidence when packets are discarded before interface
capture.

After the actor exits, its launcher and empty profile slice are stopped through
the user manager. The next public teardown must prune the departed scope's
record and chains. No immediate automatic pruning guarantee is inferred.

This is bounded evidence for the observed HTTP/TCP windows and recorded rules.
It does not prove universal atomicity, packet-loss limits, all-device coverage,
UDP containment, helper restoration, foreign-resource cleanup or uninterrupted
protection through daemon SIGKILL/restart. Those require their own P15 evidence.
