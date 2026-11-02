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

Scenario modules are
[`refresh.rs`](../crates/oxidom/tests/private_daemon/refresh.rs),
[`owner.rs`](../crates/oxidom/tests/private_daemon/owner.rs) and
[`failover_matrix.rs`](../crates/oxidom/tests/private_daemon/failover_matrix.rs).
Their helper entrypoints reexecute the same test binary with explicit filters;
they are not additional independent network scenarios.

## Isolation

Each scenario creates a private root for HOME and the XDG directories and starts
its own nonforking session bus. Child environments do not inherit the caller's
store, proxy settings or bus addresses. The system bus points to a nonexistent
socket inside that root, and external session-daemon supervision disables
implicit bootstrap. The portable-bootstrap scenario explicitly selects standalone
policy and registers both launcher children before either may exec the daemon.
The bus configuration has no activation service directories.

The seeded configuration explicitly selects a fake `xray_binary`, turns desktop
proxy and standby off, and uses proxy-only profiles. An absent environment
override alone would be insufficient: ordinary core resolution can install a
managed Xray. The fake reports the pinned version and implements only loopback
inbounds and canned SOCKS/HTTP responses. It never resolves or dials the requested
destination. All catalog addresses and credentials are invented.

Failover scenarios control individual SOCKS requests through private Unix
channels. A SOCKS refusal after the inbound is ready supplies a typed unreachable
result. An HTTP error status is still a successful measurement, and a local
inbound/configuration failure cannot authorize an alternate. Subscription refresh
uses a loopback-only HTTP provider with responses held at explicit channel gates.
No gate is sequenced by a sleep.

Readiness is based on bus replies and observable process/session events, with
bounded deadlines. A successful spawn alone is insufficient. Child guards own
cleanup, including failure paths; the dedicated integration process temporarily
acts as a child subreaper for orphaned fakes. Each spawned command has its own
process group, whose leader remains unreaped until fallback cleanup can safely
address that group. Process scenarios hold one mutex across the full lifetime of
this process-global subreaper setting, restoring its previous value before
releasing the mutex. Helper reexecutions do not change the parent setting.

Pidfds pin the process being observed. Clean shutdown checks daemon reaping
before fallback cleanup, so harness cleanup cannot stand in for correct shutdown.
Crash recovery instead observes a surviving orphan before replacement, requires
the replacement to stop it, and reaps it through the test subreaper that is now
its actual parent. The fake carries the exact config-path argument used by
production recovery identity checks; this is not universal process adoption.

Portable launch creates a new POSIX session outside its client's process group.
The scenario validates registered launcher parentage and session identity, then
checks that one owner survives both clients. Failure cleanup enumerates newly
adopted kernel children across all test-process threads, excluding the still
owned baseline bus child. It signals pinned children rather than guessing a
possibly reused process-group number. Normal assertions precede this fallback.
The target does not configure TUN, nftables or desktop proxy settings.

## Acceptance boundaries

The assertions cross real child-process and private-bus boundaries:

| Scenario | Required observations |
|---|---|
| Explicit store contenders | One owner, rejected loser and unchanged saved-file sentinels |
| Portable competing clients | Both reach the real bootstrap path; one daemon retains the bus/store after both clients exit |
| Independent sessions | Peer core, target and unused grant survive another core's loss and named Stop/Up |
| Terminal signal or bus loss | Owned cores exit and are reaped before harness fallback; completed recovery records are cleared |
| Daemon crash/replacement | The orphan is alive before replacement, then stopped; store ID persists, incarnation changes, and a saved policy does not resume a runtime grant |
| Delayed refresh | CLI Stop/core exit complete before HTTP release; late catalog replies neither resurrect stopped work nor replace a newer activation or enroll added servers |
| Public-client recovery | Old clients and clones refuse stale mutations; a new binding fully reads subscriptions, profiles, status, settings, probes and runtime; a foreign store at the same endpoint is refused |
| Persistent diagnostics | Cursor prefix plus restart suffix matches the authoritative journal prefix without loss or duplication; malformed cursors report a gap, and later appends may follow |
| Authorized failover | Gated confirmation exposes the admitted alternate before success resets the episode; cooldown cancellation, revocation/re-add/save and finite budget/visit exhaustion retain group confinement and peer independence |

These assertions are one part of tracker #216 P14. Existing reducer and
in-process regressions establish their own boundaries; the PR and tracker record
the actual CI heads/results rather than embedding a stale test count here.

The Linux flake also has an [isolated headless GUI check](gui-smoke.md).
[`headless-bench.sh`](../scripts/headless-bench.sh) remains an interactive
development bench. The Rust workspace suite does not run it or construct GTK
widgets. Main-window mapping is distinct from successful hydration of every
fact, and X11 shortcut dispatch is distinct from verifying rendered page contents.
The GUI check also requires the real AT-SPI bridge to expose a visible main
frame/window and the invented fixture label in its tree. Full screen-reader
behavior, accessibility conformance, layout and focus need their own evidence.

Privileged routing and packet-path fault evidence belongs to P15 in disposable
VMs. Canned proxy responses do not demonstrate forwarding, no-leak behavior,
continuous protection or interoperability with a real Xray release. Test and
package CI success does not by itself establish privileged packet-path behavior.
