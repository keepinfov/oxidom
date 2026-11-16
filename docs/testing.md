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

The `x86_64-linux` flake checks also include the
[isolated routing-fault VM check](routing-vm.md), which uses the installed CLI
and the locked Xray release. It requires a builder with working KVM; the CI
build job checks access to `/dev/kvm` and the KVM API before running the single
`nix flake check --print-build-logs` command. Missing virtualization fails that
job rather than skipping the routing check. The existing CLI/GUI, module and
GTK/AT checks remain in the same invocation and reuse their package derivations.

On a disposable `x86_64-linux` KVM builder, run just these checks with:

```sh
nix build .#checks.x86_64-linux.routing-faults --print-build-logs
nix build .#checks.x86_64-linux.scoped-routing --print-build-logs
```

The first VM driver runs the [routing foundation](routing-vm.md) before the
[real-core fault scenarios](routing-vm-faults.md), then the
[transition and recovery scenarios](routing-vm-recovery.md). The endpoint fault
scenarios observe A-to-B recovery, exhausted and revoked held endpoints,
an independent proxy-only peer,
and explicit daemon shutdown/restart. The recovery continuation adds a finite
active window of fresh unbound IPv4 HTTP sockets across A-to-B switching,
one real helper replacement followed by disarm/hold on repeated failure,
preservation of coordinated foreign qualified routes/rules, dirty-alias cleanup
retry, and daemon SIGKILL orphans observed before explicit replacement cleanup.
The observer records helper admissions; real executable identity and forwarding
verify the initial helper and replacement. The guest administrator installs its
configuration with all sessions and the daemon stopped, preserving the public
binary-path guard.

The second VM driver runs the [scoped routing check](scoped-routing-vm.md),
exercising a persistent `oxidom run` user scope, cgroup nftables marking and
IPv6 refusal, core-loss and Down request windows, same-scope Up recovery, and
departed-scope pruning.

These assertions cover the recorded HTTP/TCP windows and checkpoints. They do
not establish packet-loss bounds, continuous or whole-device protection, or
uninterrupted hold across daemon crash/restart. Direct traffic after explicit
cleanup measures deliberate release. Final release acceptance requires recorded
CI results.

This routing gate is not exposed on the other flake platforms, so its results do
not establish cross-platform support. Its documented packet-path assertions are
separate from the canned proxy responses in the process and GUI fixtures above.
Those responses do not demonstrate forwarding, no-leak behavior, continuous
protection or interoperability with a real Xray release. Tracker #216 records
which P15 cases have actually run and the accepted CI head; adding a check is
not evidence of its execution or completion of that stage.

## Accepted evidence and remaining acceptance

The following merged slices have accepted CI results. The heads identify the
code actually exercised; later source additions do not inherit their evidence.

- [P14 process/GUI slice #261](https://github.com/keepinfov/oxidom/pull/261),
  head `cdee957bfc51b7eb44a7e320e74d223feb642917`, passed
  [Nix/GTK/AT run 41495597588](https://github.com/keepinfov/oxidom/actions/runs/41495597588).
  The [GUI smoke](gui-smoke.md) observed startup main-window mapping,
  selected-store owner recovery and resumed polling, a real AT-SPI bridge/tree,
  navigation dispatch, hide/reactivate and Quit preserving the daemon/session.
  The accessibility observation is a pre-fault main-window tree and fixture
  label; it does not establish recovery announcements, focus order, visual
  geometry or full accessibility conformance. Its fake core does not establish
  real Xray forwarding.
- [Routing foundation #262](https://github.com/keepinfov/oxidom/pull/262),
  head `cc84ceb5968eefcc6b3fd34e530d215e6012ae9b`, passed
  [Nix run 41747002983](https://github.com/keepinfov/oxidom/actions/runs/41747002983).
  The [foundation](routing-vm.md) observed `list` and `default` owned routing,
  real HTTP through candidate A, qualified device/route/rule identity and
  candidate bypasses, then explicit process/resource cleanup and deliberate
  direct release. Positive canary bytes in a finite capture accompany the
  absence of direct TCP to the fixture HTTP sink.
- [Real-core faults #263](https://github.com/keepinfov/oxidom/pull/263),
  head `ae2c43cd0d4243654b5e2ff5b970395f3679029c`, passed
  [Nix run 41812043075](https://github.com/keepinfov/oxidom/actions/runs/41812043075).
  The [endpoint scenarios](routing-vm-faults.md) observed real A-to-B recovery,
  finite held exhaustion, revocation across its cooldown deadline, an
  independent proxy-only peer and SIGTERM cleanup before explicit restart.
  Closed captures required positive controls and zero kernel drops. These
  finite HTTP/TCP captures and endpoint snapshots do not exclude a brief
  release between sampled states.

- [Recovery continuation #264](https://github.com/keepinfov/oxidom/pull/264),
  merged as `fb06e59`, exercised head `5468b2d` passed
  [routing-faults build 43610231920](https://github.com/keepinfov/oxidom/actions/runs/43610231920).
  The [recovery continuation](routing-vm-recovery.md) observed a fresh-socket
  A-to-B transition with the same owned plan and helper, one durable real
  helper replacement followed by repeated-failure disarming with no third
  admission, qualified foreign-resource preservation and dirty-alias cleanup
  retry, and a daemon SIGKILL whose live orphan forwarded before explicit
  replacement cleanup. Closed captures required positive controls and exact
  zero kernel drops. The head above includes the capture-ring fix merged as
  `6a14b9a`: #264 merged while its own VM run still failed on the helper
  window's dropped packets, so that earlier run is not the accepted
  observation. Endpoint snapshots do not exclude a brief release between
  sampled states, and uninterrupted hold through daemon crash/restart is not
  promised.

- [Scoped routing #267](https://github.com/keepinfov/oxidom/pull/267),
  merged as `c914c2d`, exercised head `c298d64` passed
  [scoped-routing build 43709848740](https://github.com/keepinfov/oxidom/actions/runs/43709848740).
  The [scoped routing check](scoped-routing-vm.md) observed a persistent
  `oxidom run` user scope whose PID, unified cgroup and level-4 profile-slice
  inode stayed fixed through core loss, explicit Down and same-scope Up. The
  scoped actor had no effective capabilities. Marked IPv4 requests appeared in
  the candidate's real SOCKS stream and reached the real HTTP service; the JSON
  nftables rules matched the recorded cgroup and current mark; mark,
  conntrack-mark and prerouting-restore counters advanced; and an output rule
  kept a finished flow's kernel-generated reset on the tunnel side. A scoped
  global IPv6 request was refused with no sink receipt or direct data-NIC TCP,
  while an unscoped request and a scoped link-local request succeeded. Bounded
  core-loss and Down windows retained the drop chain, durable record and IPv4
  and IPv6 refusal until Up restored marking, with a live canary in both
  captures before each operation and fresh requests still completing after the
  stable observation; the next public teardown pruned the departed scope's
  record and chains. Closed captures required positive IPv4/IPv6 controls,
  zero kernel drops and no direct TCP to the scoped HTTP endpoints during the
  recorded windows. The check did not validate packaged geo data, multicast,
  neighbour discovery or ambient-capability removal from helper children;
  automatic A-to-B recovery on a retained interface is covered by the separate
  scenario below. Its reverse-path and TUN readings are not strict-filtering
  claims.
- [Automatic scoped recovery #280](https://github.com/keepinfov/oxidom/pull/280),
  merged as `4490d64`, exercised head `65f53fb` passed
  [scoped-routing build 43866895962](https://github.com/keepinfov/oxidom/actions/runs/43866895962).
  The [scoped routing check](scoped-routing-vm.md) now also observes automatic
  A-to-B recovery on the retained owned interface while a live `oxidom run`
  scope keeps marking traffic: the same device, mark, ownership cookie and
  device index; the same actor PID, cgroup and level-4 slice inode; the profile
  chain restored with its four cgroup rules, one connection rule and one
  restore rule; a fresh request through candidate B's real SOCKS stream; and
  one finite capture pair with no direct TCP to either candidate's HTTP port
  and exact zero kernel drops. The intermediate block chain is sampled by the
  existing core-loss window rather than resampled here; daemon SIGKILL or
  restart hold, atomicity, UDP and scoped IPv6 traffic remain outside this
  observation.

P15 acceptance is complete: every mandatory case in the plan now has an
accepted CI result, including automatic A-to-B mark restoration on a retained
interface. The remaining statements above are limits of the recorded
observations, not unrun mandatory cases. The 0.6.1 release validation supplies
the final compatibility, specification, manual, changelog and packaging review.

No accepted result here establishes universal no-leak behavior, atomic route
transitions, packet-loss bounds, UDP containment, resolver containment or
continuous whole-device protection. No uninterrupted hold through daemon
crash/replacement is promised; direct traffic after explicit cleanup is
deliberate release. The VM fixtures' synthetic geo classifications and HTTP/TCP
scope remain part of their boundary.
