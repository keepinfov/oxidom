# Isolated lifecycle testing

The workspace suite includes pure reducers and in-process daemon fixtures, plus
the Linux process scenarios in
[`private_daemon.rs`](../crates/oxidom/tests/private_daemon.rs). The process
target starts the built `oxidom` binary; calling a service method directly does
not substitute for this boundary.

Run it through the pinned development shell:

```sh
nix develop -c cargo test -p oxidom --test private_daemon -- --nocapture
```

The regular `cargo test --workspace --locked --no-fail-fast` CI job includes this
target. The CLI package's Nix checks also supply the private bus executable via
`OXIDOM_TEST_DBUS_DAEMON`. No new dependency or display is required.

## Isolation

Each scenario creates a private root for HOME and the XDG directories and starts
its own nonforking session bus. Child environments do not inherit the caller's
store, proxy settings or bus addresses. The system bus points to a nonexistent
socket inside that root, and external session-daemon supervision disables
implicit bootstrap. The bus configuration has no activation service directories.

The seeded configuration explicitly selects a fake `xray_binary`, turns desktop
proxy and standby off, and uses proxy-only profiles. An absent environment
override alone would be insufficient: ordinary core resolution can install a
managed Xray. The fake reports the pinned version and implements only loopback
inbounds and canned SOCKS/HTTP responses. It never resolves or dials the requested
destination. All catalog addresses and credentials are invented.

Readiness is based on bus replies and observable process/session events, with
bounded deadlines. A successful spawn alone is insufficient. Child guards own
cleanup, including failure paths; the dedicated integration process temporarily
acts as a child subreaper for orphaned fakes. Each spawned command has its own
process group, whose leader remains unreaped until fallback cleanup can safely
address that group. Pidfds pin the process being observed. The successful path
checks daemon reaping before fallback cleanup, so harness cleanup cannot stand
in for correct shutdown. The test does not configure TUN, nftables or desktop
proxy settings.

## Acceptance boundaries

The process assertions cover store-owner contention without saved-file changes,
independent saved-profile sessions, loss and explicit recovery of one core,
terminal signal shutdown, a new daemon incarnation using the same store and
private-bus transport loss with a live session. Losing or
stopping an unarmed peer must preserve the other session's core and unused
activation grant. The tests inspect both client-visible state and child-process
lifecycle. A saved failover policy surviving restart does not authorize automatic
resumption.

This is one part of tracker #216 P14. The remaining integrated matrix includes
refresh versus Stop, delayed completion, daemon owner replacement, diagnostic
cursor recovery and bounded failover exhaustion across client/process boundaries.
Existing reducer and in-process regressions remain useful evidence for their
own boundaries.

An automated headless GTK smoke scenario is also required before P14 acceptance.
[`headless-bench.sh`](../scripts/headless-bench.sh) is an interactive development
bench; running the workspace suite does not run it or establish GUI smoke
evidence. GUI automation must own and reap its bus, display server, daemon and
GUI, use a private display and memory GSettings, and wait for completed hydration
rather than merely finding a splash window.

Privileged routing and packet-path fault evidence belongs to P15 in disposable
VMs. Canned proxy responses do not demonstrate forwarding, no-leak behavior,
continuous protection or interoperability with a real Xray release. Test and
package CI success does not by itself complete P14 or P15.
