# Real-core routing fault scenarios

This script continues the same disposable NixOS guests after the
[routing foundation](routing-vm.md). It is a separate P15 acceptance slice;
adding its source does not establish that it ran. The tracker and pull request
record the accepted CI head and actual results.

The Nix VM `testScript` concatenates `scripts/routing-vm.py`,
`scripts/routing-vm-faults.py` and `scripts/routing-vm-recovery.py`, with a newline
between each script. The [transition and recovery continuation](routing-vm-recovery.md)
runs after the scenarios below. No separate package build,
developer daemon, host tunnel or external provider is a fixture.

## Separate real upstream processes

After the foundation has stopped both owned sessions and flushed its captures,
the script stops the original aggregate upstream service. Guest-only runtime
systemd units then run the same real pinned Xray as independent candidate A,
candidate B, unauthorized C and an independent LAN proxy. Each has a private
state directory, a dynamic user and no capabilities. Their invented no-auth
listener configurations live only under the disposable guest's `/run`.
Runtime unit files remain loadable across the deliberate B Stop/Start; the
test does not depend on transient-unit garbage collection timing.

C is imported into the catalogue at `192.0.2.12` but is absent from the fixed
authorization `[A, B]`. The independent profile's endpoint is the peer's LAN
address, `10.42.0.1:1082`. An off-LAN endpoint outside the authorized bypass union
would itself be captured by default TUN routing and would not demonstrate the
intended independence. Its local SOCKS/HTTP ports are separate from the owned
TUN session's ports.

Client mutations use the installed CLI or public D-Bus as the ordinary guest
user. The VM observer alone reads kernel/private state and injects faults.
Before each signal, it opens a pidfd and verifies the process start time, UID,
parent and exact generated configuration argument. The core-death SIGKILL is
sent through that descriptor, never to an unverified saved numeric PID.

## Assertions

1. **A core dies and B confirms real traffic.** After the client A core dies,
   runtime must attribute the confirmed replacement to B. The peer profile
   retains its original core identity and serves unique canaries before and
   after recovery. A separate unique canary must appear inside B's actual
   SOCKS TCP stream. The pre-fault and confirmed-replacement snapshots of the
   device, helper PID and qualified durable envelope match; the saved
   primary/policy bytes do not change. Confirmed success resets the episode.
2. **An unavailable B reaches finite held exhaustion.** B's separate service
   is stopped before A's core is killed; A's upstream and the LAN peer remain
   available. A real B dial must be visible. One admitted automatic attempt
   reaches exhausted state without retrying A or visiting C. The exhausted
   snapshot retains the device and installed plan, held HTTP fails without a
   sink receipt, and the original peer core still serves real traffic. Explicit
   Stop subsequently removes the owned plan.
3. **Revocation survives reenable and the old cooldown deadline.** Following
   confirmed A-to-B recovery, B's core dies and the test observes a positive
   application-owned remaining cooldown. A unique peer canary is bracketed by
   backoff observations, proving that the unchanged peer serves traffic while
   the owned session is waiting and holding. This stable cooldown checkpoint
   avoids assuming that multiple RPCs can fit inside a confirmation timeout.
   Disable followed by enable leaves the current activation disarmed. A
   timerfd crosses that observed deadline;
   held HTTP still fails and no new A or C dial appears. Only explicit
   Down/Up establishes a new activation and working path.
4. **SIGTERM cleanup precedes a deliberate restart.** Stopping the shipped
   system service removes owned routes/rule/device, recovery sessions and the
   recorded core/helper processes before VM teardown. A direct request then
   measures the intentional release boundary. No oxidom/busctl request targets
   the stopped daemon, avoiding accidental D-Bus activation. Explicit
   restart preserves store identity, changes daemon incarnation, and restores
   neither sessions nor a runtime grant. A later explicit Up works again.

Both closed packet captures require the same unique independent-peer controls
around negative canaries, not merely absence from an empty capture. Physical
data-NIC captures reject direct TCP to the HTTP sink. Each blocked curl uses a
distinct fixed source port below the guest's observed ephemeral range, and the
all-interface capture must positively show
that canary's SYN to the sink while its route still names the TUN. The separate
all-interface capture also rejects C dials even if a forbidden attempt were
trapped inside the TUN. The no-A-revisit interval starts inclusively at a
timestamp sampled immediately before the fault signal, covering the entire
post-injection interval rather than a later process-exit observation. Both
capture processes must finish successfully and report zero kernel drops.

Each tcpdump uses immediate mode for kernel delivery and `-U` for pcap output
flushes. Before sending SIGINT, bounded live-file checks require every mandatory
unique peer/candidate control in both captures, plus the correlated blocked
canary SYN in the all-interface capture when applicable. Exhaustion also waits
for B's attempted connection. No fixed sleep substitutes for these observations.
Both captures are stopped even if a live observation fails; successful gates
are followed by the existing closed-file positive and negative assertions.

These are endpoint observations before faults and after confirmed recovery or
stable held states. Although capture spans a transition, this slice does not
issue a continuous sequence of fresh TUN requests or monitor every kernel route
change during that transition. It therefore does not exclude a brief release
window between the observed states. The subsequent
[transition and recovery continuation](routing-vm-recovery.md) adds a finite
active window of fresh unbound IPv4 HTTP sockets across A-to-B switching, with
positive packet controls. Its assertion is limited to that recorded window.

The foundation's declared synthetic geo classification remains in use. These
checks cover observed HTTP/TCP paths, not packaged geo data, UDP/ICMP or IPv6.
This endpoint slice does not establish helper restoration, foreign-owned
resource collision recovery or daemon SIGKILL recovery. The continuation adds
one real helper exec replacement followed by repeated-failure disarm/hold,
coordinated foreign qualified route/rule preservation, dirty-alias cleanup retry,
and identifiable orphans before explicit replacement cleanup. Its helper exec
observer is an offline guest-administrator fixture; public binary-path guards
remain in force.

The cumulative check does not establish scoped cgroup/nft enforcement, IPv6
containment, packet-loss bounds or continuous/whole-device protection. The
daemon restart contract cleans non-pool owned TUN records; direct traffic after
that explicit recovery is deliberate release, and no uninterrupted hold through
daemon crash/restart is claimed. Remaining P15 cases and final release acceptance
need separate evidence and recorded CI results.
