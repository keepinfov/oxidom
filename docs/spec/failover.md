# Authorized Sequential Failover

## Delivery Boundary

P12 is delivered in two required parts. The first implements proxy-only saved-profile
failover; the second adds safe interface routing. P12 is not complete after the first part.
The TUN part supports an explicitly bounded routing envelope, described below. No routing
protection claim follows from proxy-only operation. Dedicated controls follow the contract below.

## Persisted Authorization

An optional profile `[failover]` section contains `enabled` (default false), `members`
(ordered canonical server IDs), `max_attempts` (default 3, range 1..64), and
`cooldown_secs` (default 30, range 1..3600). Absence preserves legacy behavior and bytes.
Enabled authorization requires 2..64 unique, nonempty canonical IDs, a single-server
selection covered by those IDs and no pool. Alias matching, rule filters,
latency ranking and empty-list expansion cannot determine authorized membership.
Activation resolves IDs exactly against the current catalog and refuses missing or
composite candidates. Authorization is not a subscription or GUI-group reference.

Only explicit saved-profile activation snapshots this authorization. Temporary Connect,
ConnectPool, global reconnect and `oxidom run` do not authorize alternate servers. New
configuration is persisted before a later activation can use it. No automatic attempt
rewrites the saved selection. Candidates use the current configuration of an authorized
ID, not an immutable endpoint snapshot; missing IDs cannot be replaced by aliases.

## Controls And Progress (binding)

`oxidom profile failover NAME` reads saved authorization and the current activation, if reported.
`--enable` and `--disable` are mutually exclusive explicit choices. `--members ID,ID` replaces the
ordered exact-ID list; `--max-attempts` and `--cooldown-secs` replace only those settings. Merely
setting a list or limit never enables an absent/disabled policy. Disable retains the list and
limits. No flags means no mutation; `--json` changes output only. These existing-daemon commands
never bootstrap, stop, start or rearm a session. Repeated enable of the same policy does not
rearm a revoked activation. Explicit Stop/Up is required for a new activation.

The additive `SetProfileFailover(name: String, policy_json: String) -> ()` D-Bus method patches
only the optional policy. Its JSON object has optional `enabled`, `members`, `max_attempts` and
`cooldown_secs`; only present non-null fields are changed. Omitted fields come from the CURRENT
saved policy under the activation/admission lock, never the client snapshot. Thus a limits-only
edit cannot undo a concurrent disable or restore a removed group. Empty `{}` is a no-op,
including keeping an absent section absent; top-level null removes the policy. Unknown patch
fields are refused. Stale CLI reads cannot write back primary, ports, interface, routing or
core options. Validation and durable persistence precede
use by a later explicit activation. Changed authorization revokes the current activation before
persistence; failed writes disarm it. It never removes a held interface or interrupts admitted
confirmation. `SaveProfile` remains available, with unchanged signature and legacy omission/null
semantics. Both writing paths validate enabled authorization against the current catalog,
resolved primary and effective TUN envelope; a disabled policy can retain unavailable IDs.
Activation/admission still revalidate, since the catalog can change after saving.

The profile dialog provides an off-by-default switch, an ordered comma-separated exact-ID list,
a catalog picker with explicit Add, and bounded attempt/cooldown controls. Add appends a canonical
ID, never an alias or implicit group. The list can be reordered/trimmed explicitly. Selecting a
new primary or pool turns switching off and invalidates the prior permission while retaining
the visible list and limits in disabled form; enabling it after that selection is an explicit
new authorization. Pools cannot enable it. Ordinary unchanged
saves preserve policy; untouched new/legacy profiles keep the section absent. Saving never
starts or rearms runtime. The dialog explains temporary-connection exclusion, fixed membership,
revocation and the restricted TUN boundary.

Current GUI and TOML editors send request-only `expected_failover: {select, failover}` in the
existing SaveProfile JSON. The expectation is captured when loading, independently of edited
values. Under the engine lock the daemon compares both fields with the current saved profile
before reconciliation, disarming or writing. A conflict fails explicitly and requires reopening;
no automatic merge/replay or authorization restoration follows an unrelated description/limit
edit. Malformed/null expectation is an error, not an unchecked save. An absent expectation
retains legacy caller behavior. The guard is never serialized into a profile or recovery file.
It guards selection and authorization, not a revision of every profile field.

The read-only daemon handshake advertises `profile_failover`. New clients refuse policy writes
without it; the GUI explains why controls are unavailable instead of silently succeeding against
a daemon that ignores the field. No mutating capability probes are permitted. Older clients
ignore the new capability, method and runtime field; their existing signatures remain valid.

`runtime.failover` is optional, omitted when no activation grant is reported, and never inferred
from saved configuration. It is generation-fenced application state with open-string `state`
(`armed`, `recovering`, `exhausted`, `disarmed`), ordered canonical `members`, `current` ID,
optional `next` ID, episode `attempts`, `max_attempts` and `cooldown_secs`. Waiting reports the
next unvisited authorized candidate without admitting it; in-flight, idle, exhausted and
disarmed states have no next candidate. Attempts count automatic admissions, excluding explicit
Up; success resets the episode. Exhaustion can occur before max_attempts when every alternate
was tried. Revocation retains the activation's original list and progress for explanation;
new saved settings do not relabel it. Stop/replacement clears that old activation. Missing or
unknown facts do not establish disabled policy, authorization, health or protection. GUI details
and CLI text/JSON present these facts separately from phase, health, hold and retry deadlines.

## Recovery Episode

Unexpected core death or a current confirmation timeout/unreachable outcome may begin
bounded recovery. Conservative watchdog observations alone never switch a live core;
aggressive watchdog retains its existing threshold. NoNetwork, Internal, cancellation,
interface/configuration failures do not authorize advancing candidates.
Malformed or unsupported HTTP probe URLs/methods are local failures, not remote evidence.
Recognized current-core configuration, protocol, geo-data and certificate complaints also
prevent candidate advancement. Untyped startup failures consume an admitted attempt but
conservatively disarm recovery rather than guessing that another server would help.

The explicit initial start is not an automatic attempt. Recovery traverses subsequent
members cyclically from the failed member, trying each at most once per episode, never
the initial failed member again in that episode. It stops at the earlier of the finite
attempt budget or candidate exhaustion. One start and confirmation run at a time per
profile. Admitted automatic starts consume budget even if startup fails; no busy loop
or infinite same-target reconnect may run underneath exhaustion. Success requires current
core traffic confirmation and does not trigger automatic failback.

Cooldown is a monotonic minimum interval between automatic start admissions within the
activation, combined with bounded reconnect backoff by taking the later deadline. Waiting
is cancellable. Budget and visited candidates reset after confirmed recovery; cooldown
does not. Runtime progress is never persisted or resumed across daemon restart.

## Revocation And Currentness

Stop, replacement, shutdown and authorization revocation prevent future starts. Each
attempt retains generation/core attribution; stale completion cannot change a replacement.
Daemon-mediated profile changes serialize with activation and automatic admission. A
changed selection or authorization disarms the current snapshot rather than enrolling
new candidates. Profile removal disarms failover without implicitly stopping its runtime.
Uncertain persistence cannot arm authorization. An already-admitted attempt may finish
confirmation, but cannot schedule another after revocation.

Catalog removal of any authorized ID disarms the activation; readdition never rearms it.
Subscription refresh cannot add members or resurrect an invalidated candidate. Direct
external file changes are revalidated before automatic admission; they cannot be claimed
as instantaneous daemon-mediated revocations.

Older SaveProfile clients omitting `failover` preserve existing authorization only when
selection is unchanged. Explicit null removes authorization. Current editors round-trip
the field. Changed selection with omitted authorization revokes rather than transferring
permission. Legacy clients retain existing D-Bus signatures and status fields. Runtime
retry details may add open-string states for bounded recovery/exhaustion without changing
their legacy projection. No diagnostic event is an application authorization source.

## TUN Routing Envelope

TUN failover requires effective `on_core_exit = "hold"`. Every candidate must have a
literal unicast IPv4 dial address and a supported typed single-endpoint outbound. Hostnames,
IPv6, raw outbound patches, composite profiles and transport extensions that can introduce
additional endpoints are refused before activation. TLS names and transport host fields are
not rewritten; catalog credentials remain current. No new runtime DNS lookup is required.

The activation snapshots the exact ID-to-dial-address envelope, interface settings and
effective hold policy. Before every automatic start, current candidate shapes and addresses
must still match it. Address changes disarm rather than expand routes. All candidate bypasses
are prepared before system capture and retained with the existing recovery route records.
Marked application traffic must not receive those main-table bypasses in its private table.

Only one oxidom TUN interface may coexist with a TUN-failover envelope. Conflicting later
interface activation is refused as well; proxy-only sessions remain independent of this
interface admission restriction. Unsupported or conflicting route/device ownership must fail
before capture rather than treating an existing route as owned. Automatic switching never
calls interface reconfiguration or removes an established hold. Exhaustion, revocation and
local helper/start failures retain the installed plan until explicit teardown. Initial
verification precedes route installation and does not promise protection from the instant Up
is requested. Its RPC receipt concerns the initial attempt; bounded recovery can continue
asynchronously after an initial verification failure.

Xray death and tun2socks readiness are distinct. A local helper failure disarms alternate
advancement rather than blaming the remote candidate. Explicit interface deletion revokes
the envelope. Settings-driven endpoint restarts revoke failover; any surviving helper must
be restarted for a changed SOCKS endpoint. No universal IPv6 or whole-device protection is
claimed. Literal route-plan and fake-process tests do not replace the isolated VM packet
evidence required by P15.

## Owned Recovery Format

Owned TUN resources use version-1 recovery records with route protocol/metric and output
interface selectors, rule protocol/full mark mask/table/priority, and device alias/index.
Durable qualified intent precedes persistence and routing mutations. Unknown versions or
malformed owned selectors stop the new daemon before recovery, without quarantine or rewrite.
Legacy state remains readable, but legacy cleanup selectors are empty for owned resources.

Old binaries do not understand or reject the new owned format and may discard its metadata.
Downgrade is supported only after the new binary has stopped sessions and completed cleanup;
outstanding owned records must be resolved with the new binary first. Ownership tags provide
administrative identity, not protection against privileged writers deliberately reusing them.
Concurrent independent routing changes require administrative coordination. Partial cleanup
marks the interface dirty and blocks reconnect until explicit teardown resolves the intent.
