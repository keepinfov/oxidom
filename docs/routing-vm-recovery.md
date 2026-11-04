# Routing transition and recovery checks

This continuation runs after the [routing foundation](routing-vm.md) and
[endpoint fault scenarios](routing-vm-faults.md) inside the same disposable
NixOS guests. The Nix VM `testScript` concatenates `scripts/routing-vm.py`,
`scripts/routing-vm-faults.py` and `scripts/routing-vm-recovery.py`, with a newline
between each script, in that order.
Adding these files does not establish a runtime result; the tracker and PR
record the actual accepted CI head.

The guest operator restarts the real aggregate upstream, because the preceding
fault slice stops its individual upstream services. Candidate addresses, the
HTTP sink and the explicitly synthetic geo classification remain unchanged.
There is no provider, real subscription or external destination.

## Active transition window

A dedicated ordinary guest-user process opens fresh literal-IP HTTP sockets
through the default TUN capture. It is not an oxidom run scope and has no
capabilities. Its private Unix channel starts a finite request window only
after the first unique request completed successfully. The same payload must
be visible in both live captures before the fault is injected.

The producer remains active across client-core A death and real B
confirmation. The record must contain a fresh request started after fault
injection and before the stable checkpoint, plus a completed request started
after that checkpoint. Each request and the whole series have bounds; network
errors are recorded, while an observed direct sink response fails the check.

Both closed captures retain the positive A/B payloads and LAN controls. The
physical data-NIC capture must contain no direct TCP to the sink HTTP endpoint,
and the all-interface capture rejects an unauthorized candidate dial.
Immediate delivery, live-file gates before Stop and exact zero kernel-drop
statistics prevent an empty or undrained capture from standing in for this
observation. This proves only the recorded finite HTTP/TCP window, not
universal atomicity, packet-loss bounds or continuous whole-device protection.

## Real helper admission observation

With all application sessions stopped, the guest administrator stops the
daemon and installs a private exec observer around the original real
tun2socks path. Only the binary-path setting is replaced through private
atomic temporary-file/rename/fsync handling; all other parsed configuration
keys and ownership are checked unchanged. Public system-bus binary-path
guards are not bypassed by a client request.

For an actual device invocation, the observer appends one bounded, locked,
fsynced PID/argv record to its private journal, then execs the same real helper
with the same arguments and PID. Version probes are not counted. The fixture
then explicitly starts the daemon; all later application mutations again use
the ordinary unprivileged CLI or public bus.

The initial helper and its one automatic replacement must match their
recorded PIDs, real executable identity and actual HTTP forwarding. The
replacement PID must already be durable in daemon state. A second fault must
disarm automatic advancement and stop the core while retaining the routing
plan. A finite active-request window and unchanged exec journal reject a third
helper admission; candidate B traffic is separately absent. This is not a
sustained-readiness promise inferred from spawn.

## Qualified cleanup and retry

A coordinated administrative fixture inserts protocol-242 foreign resources:
a route through eth1 with a different metric, plus a distinct marked table
and a same-priority rule. No application traffic is generated while the
deliberate competing route is present. Explicit own cleanup must preserve
those exact foreign kernel records; the observer later removes only its own
fixture selectors. This does not model an uncoordinated privileged attacker.

A separate fixture changes only the owned device alias. Failed Down must
retain qualified dirty intent and kernel routing; subsequent Up is refused.
Process and packet checkpoints are observed explicitly: failed Down is not a
successful Stop, and metadata alone does not demonstrate holding. The observer
restores the captured original alias, then public Down retry must complete
cleanup and allow the deliberate direct route again.

## Daemon crash boundary

A guest-only Restart=no drop-in prevents supervision from erasing the
orphan-before-replacement checkpoint; KillMode remains process. After an
identity-checked pidfd SIGKILL, the real core, helper and owned TUN must still
exist and forward a unique request. No daemon RPC is used while it is dead,
because that could activate the packaged service.

Only then does the fixture explicitly start the replacement. Non-pool owned
recovery must stop the recorded orphan processes and clean their routing
resources. Store identity remains, daemon incarnation changes, and saved
policy does not resume runtime authorization. A direct request after that
cleanup measures the intentional release boundary. No uninterrupted hold
through daemon crash/restart is promised. A later explicit Up creates a new
owned activation, and the fixture restores the original restart policy.

The remaining scoped nft/cgroup, IPv6 and final release acceptance are separate.
