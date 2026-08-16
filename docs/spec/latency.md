# Latency probes (`latency_method`)

How a latency number is measured, what a failed probe means, and the contract a measurement
travels under.

- `icmp` — spawn `ping -c1 -W1 <host>` and parse (avoids raw-socket privileges).
- `tcp` — time a raw TCP connect to `host:port`.
- `http_head` — HEAD `latency_test_url` **through the active SOCKS inbound**.
- `http_get` — GET `latency_test_url` through SOCKS (Happ-style; expect 204).

List view may use a cheap method across servers concurrently (bounded thread pool); the active
connection uses the configured method.

## Probe outcomes (binding)

`probe::measure` returns `ProbeOutcome`, never `Option`: `Reachable(Measurement) | Unreachable |
Timeout | NoNetwork | Internal(&'static str)`. The distinction is the point — a failure that is
*not the server's fault* must never be drawn as a dead server.

- **`NoNetwork`** is claimed only on evidence: a kernel error that says so
  (`NetworkUnreachable`/`NetworkDown`/`HostUnreachable`, or `ENETUNREACH`/`ENETDOWN`/`EHOSTUNREACH`
  by errno), or an ambiguous DNS failure *plus* no default route in `/proc/net/route` or
  `/proc/net/ipv6_route`. getaddrinfo reports NXDOMAIN and "no resolver" identically, so DNS alone
  never proves it. An unreadable `/proc` means "no evidence", not "offline".
- **`Internal`** is this machine's fault — no core binary, no free port, unwritable data dir. It
  crosses the wire as `ProbeFailure::Unknown` plus a `warn` line; blaming the server would be a
  lie, and inventing a fifth wire variant would break older GUIs (`ProbeFailure` has no
  `#[serde(other)]`).
- A client must keep that distinction on screen (binding). `Unknown` says the check never reached
  the server, so it may not be drawn as an unresponsive one: a machine with no core fails every
  probe at once, and rendering that as a subscription of dead servers sends people to replace
  nodes that work. It renders as its own state, and a whole-subscription sweep reports it once —
  a sweep stays quiet about a single silent server, whose own card already says so.
- The hysteria2 ICMP fallback retries only `Unreachable`/`Timeout`. Retrying `NoNetwork` would
  launder it into "server is dead".
- An HTTP response with an error status still proves the server carried the request: `Reachable`.

## The reading contract (binding)

A latency number without its context is a lie waiting to be told, so a measurement crosses D-Bus
as `ipc::LatencyReading { value, measured_at_unix_ms, route, method, failure }` — never as a bare
`Option<u32>`, and never in `Server`.

- **`route`** is `Direct` or `Proxied`. Only the server the tunnel is *currently* carrying may be
  measured `Proxied` (`daemon::Shared::probe_target`), and only a `Proxied` reading may be shown
  as the connection's latency. A reading whose route no longer applies is `Superseded` on the
  card — shown as `—`, never as a number.
- **`failure.is_some()` ⟺ `value.is_none()`**, upheld by `LatencyReading::ok`/`failed`. Build them
  through those constructors.
- **Every direct id that enters `ProbeQueue::running` leaves with a `readings` entry**, including
  ids that no longer resolve; a job for a still-current session leaves its result in `proxied`.
  The GUI retires its spinner on the id leaving `running ∪ queued`, so a silent early return
  leaves a card checking forever.
- **`queued ≠ finished`.** `ProbeState` reports `running` and `queued` separately; a card waiting
  for a slot still carries its *previous* number and must not present it as this measurement's.
- **`ProbeState.version`** is bumped for incompatible semantic changes. The additive,
  serde-defaulted `proxied` map does not bump it. A GUI seeing a lower required version reports
  everything as unmeasured and says why, rather than guessing.
- `ProbeState.readings` contains direct measurements keyed by server id.
  `ProbeState.proxied` contains connection measurements keyed by profile; two profiles on one
  server must never overwrite each other. Readings are pruned with their server or session.

Freshness is the GUI's job: `gui::reduce::latency_state` is the **single** mapper from a reading to
a `LatencyState`, and ages are bucketed to whole minutes so the badge repaints on a bucket change
rather than once a second.
