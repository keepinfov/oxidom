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
Timeout | NoNetwork | Internal(ProbeDetail)`. The distinction is the point — a failure that is
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
- **The probe core's own log is read, not discarded (binding).** A core says why it will not work
  — `tls: failed to verify certificate: x509: …`, or `allowInsecure has been removed` — and it
  says it on **stdout**, which is where Xray writes its whole log (`xray run -test 2>/dev/null`
  still prints the error; `1>/dev/null` prints nothing). The probe core is therefore run at
  `log_level = info` regardless of the machine-wide `[core]`: at `warning` the same rejected
  certificate is reported on one transport and silently dropped on the next. The log is read only
  when the probe failed, capped, and never stored.
- A recognised complaint becomes a `ProbeDetail` on the wire —
  `certificate_rejected`, `insecure_tls_unsupported`, `config_refused`, `geo_assets_missing`,
  `no_core`, `cancelled`, `other` — set
  on `LatencyReading.detail` beside `ProbeFailure::Unknown`. It is a serde-defaulted field with a
  `#[serde(other)]` fallback, so a daemon that sends a reason and a client that has never heard of
  it still understand each other; a fifth `ProbeFailure` variant would have made older clients
  fail to parse the whole snapshot. An **unrecognised** complaint changes nothing: the verdict
  stays whatever the measurement said, because guessing at a core's wording is how a wrong
  explanation gets shown with confidence.
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
- **Every direct id that leaves `running ∪ queued` leaves a `readings` entry behind**, including
  ids that no longer resolve and ids that were cancelled; a job for a still-current session
  leaves its result in `proxied`. A session probe's reading lands only while the connect attempt
  it set out to measure is still the profile's current one: the Connected check alone is true for
  a session replaced mid-probe, and a timeout measured across the restart would overwrite the new
  core's confirmation. The invariant is stated over the *departure* rather than over
  running to completion, because the GUI retires its spinner on the id leaving that union: a
  silent early return, by any route, leaves a card checking forever.
- **`queued ≠ finished`.** `ProbeState` reports `running` and `queued` separately; a card waiting
  for a slot still carries its *previous* number and must not present it as this measurement's.
- **`ProbeState.version`** is bumped for incompatible semantic changes. The additive,
  serde-defaulted `proxied` map does not bump it. A GUI seeing a lower required version reports
  everything as unmeasured and says why, rather than guessing.
- `ProbeState.readings` contains direct measurements keyed by server id.
  `ProbeState.proxied` contains connection measurements keyed by profile; two profiles on one
  server must never overwrite each other. Readings are pruned with their server or session.
- **`ProbeState` carries the newest reading and nothing older.** It is polled every 500 ms for
  every server a client knows about, so anything added to it is a standing broadcast. That is why
  the history below is a second payload and not a field here — and why `PROBE_STATE_VERSION` does
  **not** move for it: nothing in this struct changed, so a client that has never heard of the
  history parses a current daemon's snapshot exactly as before.

Freshness is the GUI's job: `gui::reduce::latency_state` is the **single** mapper from a reading to
a `LatencyState`, and ages are bucketed to whole minutes so the badge repaints on a bucket change
rather than once a second.

## What a failed check says (binding)

The badge answers a glance across the whole grid; the **expanded card** answers the diagnosis. They
are different questions and have two mappers, `gui::reduce::latency_state` and
`gui::reduce::failure_report`, but they must read **the same reading** — `SnapshotState::card_state`
and `SnapshotState::card_failure` both take it from `shown_reading`, so a card cannot show a dash
for this check beside the reason from the one before it.

- **The reason is the daemon's, not the card's.** `ProbeFailure::message_with` is the one place the
  wording lives, so the CLI, the badge and the card cannot describe one condition three ways. The
  card promotes the fragment to a sentence and adds nothing.
- **A reason travels with how the check was made and when.** The method actually used and the route
  are what decide whether the reason is about the server at all: a refusal measured through a
  tunnel that has since gone down describes the tunnel. A reading that cannot be dated says so
  rather than reading as fresh.
- **A check in flight carries no reason.** The previous one describes a measurement being replaced,
  and under a spinner it reads as why the spinner is spinning.
- **A stopped check still gives a reason.** The card owes an answer for having no number, and
  `Cancelled` is that answer. It is reported as what happened, not as a fault — which is why the
  block is ruled off rather than coloured as an error.
- **One action leads to the rest of what happened**, which is in the process log mixed with every
  other source: the card opens the log page narrowed to the server's **address**, because that is
  what the prober and the core write. A name is the user's word for a server and appears in no log
  line. The narrowing is the log page's own search entry, so it is visible and can be widened.
- **The block's two actions are icons on the reason's own line.** Two labelled buttons on a row of
  their own made this the tallest thing on an expanded card once the recent checks stopped being
  it. Every icon here carries a tooltip *and* an accessible label, which is the whole of what keeps
  an icon from being a guess — and the report's icon is a warning rather than a send, because
  nothing leaves the machine and an arrow would promise a transmission that does not happen.
- **It sits under the record, not over it.** The count of failures and the reason for the newest
  one are both about checks, and either side of the chart they read as two blocks that happen to
  share a subject. Together they read as one statement: this many of the last ten failed, and the
  last of them was tried this way, this recently.

## What the recent checks say (binding)

One number is the weakest possible basis for choosing between servers: a server that is fast half
the time and one that is steady are indistinguishable through their newest reading alone. The
daemon therefore keeps the last `ipc::PROBE_HISTORY_LIMIT` readings per server, and the expanded
card **draws** them as a chart with the numbers, the direction and the failures written underneath.

- **The history is fetched, never polled.** `ProbeHistory` is asked for one server at a time, when
  a card opens and when something about that server changes, the way `RuntimeInfo` is asked for
  when Settings opens. Folding it into `ProbeState` would multiply a twice-a-second payload for
  every server by ten to feed a block only the one open card can show.
- **It records checks that ran.** A check called off before it got a slot measured nothing about
  the server, so it leaves a `readings` entry — the card still owes an answer for having no
  number — and no history entry. Recording those would let one press of Stop on a large sweep push
  every server's real record out of a ten-deep list, erasing the history by way of filling it.
- **A check that ran and failed keeps its place.** A server that times out every other attempt is
  exactly what the block exists to expose, and dropping those readings would make it look steady.
  The reason is `ProbeFailure::message_with` again, not a second wording.
- **It is direct measurements only**, like `readings`. A `Proxied` reading describes a tunnel, and
  belongs to the profile carrying it rather than to the server.
- **Histories are pruned with their server**, for the same reason readings are.
- **The age is worded by one function.** `gui::reduce::when_text` writes "just now", "3 minutes
  ago" and "at an unrecorded time" for both the reason above the chart and every column of it,
  because the two sit on the same card and two spellings there read as two different facts.

### The shape of the block (binding)

Spread, outliers and failures are shape rather than text, and a list of ten lines is not how a
comparison between servers is made. The block is a chart, and these are the rules that keep it
honest — a chart says things a list cannot, and it can also imply things a list never could.

- **Three marks, not two.** A check that ran and failed, and a slot no check has filled, are drawn
  differently. A gap would be both at once, and would report a server that fails every other
  attempt as one nobody has got round to testing — the distinction the whole block exists for.
- **A failure is drawn full height and striped.** It is not a measurement, so no height would be
  the honest drawing and a gap the result. Full height cannot be mistaken for a gap, and the
  stripes are what keep it from being read as a very slow reading by someone who cannot separate
  the two colours. The mark carries the difference; the colour only reinforces it.
- **Every slot the daemon keeps is drawn**, filled or not, so `PROBE_HISTORY_LIMIT` is visible on
  the card. The list this replaced stopped at ten and said nothing about it, so it read as
  unbounded.
- **Time runs left to right**, which is the opposite of the newest-first order the daemon answers
  in and of the failure block above. `gui::reduce::history_legend` says which, because both
  directions are ordinary and the picture cannot show that it chose one.
- **The heights are shares of the tallest reading on that chart alone.** Two servers' charts are
  therefore *not* comparable with each other, and a steady 5 ms server and a steady 500 ms one draw
  the same picture. This is why the range is stated and is not optional.
- **A reading is a label; a legend is a tooltip.** The range, how many checks are behind it, and
  the reasons for the failures describe the *server*: they are labels on the card, because a chart
  whose content is reachable only by pointing at it says nothing to a screen reader and nothing to
  anyone who does not think to hover — and the failures are the case the block exists for. The
  direction and the bound describe the *drawing*, are identical on every card of every server, and
  are the heading's tooltip. They were a caption once and cost two lines of every open card, on a
  block whose whole reason for replacing a list was height.
- **The range rides on the heading.** It sits beside "Recent checks" rather than under the chart,
  so the numbers the heights cannot state cost no line of their own. Its shortness is a
  requirement, not a style: a summary long enough to wrap has given back what the chart saved.
- **The reasons are grouped, not listed.** Six timeouts in a row spend one line, not six, and the
  commonest reason leads because that is the one describing the server.
- **The geometry is a pure function.** `gui::reduce::chart_columns` turns the marks and a
  rectangle into integer columns, and `history_chart` turns a `ProbeHistory` into marks and
  sentences. Neither touches a widget, because no test here may construct one, and the column
  arithmetic is the part that fails invisibly: a width rounded down ten times leaves a strip on
  the right that reads as the chart being clipped.
- **The palette stays in CSS.** Every column is a child widget carrying a style class rather than
  a rectangle drawn from an `RGBA` held in Rust, because nothing else in this application picks its
  own colours and a chart that did would be the one thing on the card that ignored the theme.

## The D-Bus surface (binding)

Five methods on `dev.keepinfov.oxidom1` carry probing. They are listed here because a client that
cannot see the interface has no other place to read what it may call.

| Method | Signature | What it does |
|---|---|---|
| `RequestProbe` | `(s server_id) → ()` | Enqueue one server. Returns as soon as it is queued, not when it is measured. |
| `RequestProbes` | `(as server_ids) → ()` | The same, for a list. One call, so a sweep does not cost one round trip per server. |
| `CancelProbes` | `(as server_ids) → (s json)` | Drop queued direct probes for these servers. Answers `{"cancelled": N}`. |
| `ProbeState` | `() → (s json)` | The whole `ProbeState` as JSON. Polled; there is no signal. |
| `ProbeHistory` | `(s server_id) → (s json)` | One server's recent checks as a `ProbeHistory`, newest first. Fetched on demand, never polled. |

**A missing history is not an error.** A server nobody has checked answers an empty list, and a
daemon too old to know the method answers `UnknownMethod` — which `Client::probe_history` also
reports as an empty history, because that is the truth from the caller's side: no such daemon kept
one. The card falls back to the single reading it already has rather than raising a failure over a
panel.

**Requesting is idempotent, not additive.** `ProbeQueue::holds` drops a request for a target already
running or queued, so pressing a check twice measures once. A client must not treat the second call
as a second measurement, and must not present it as one.

**Cancelling stops the queue, not the measurement (binding).** The per-server budget is about ten
seconds for the default HTTP method and eight run at once, so a sweep costs roughly
`ceil(servers / 8) × 10s`. `CancelProbes` drops everything still queued; the at-most-eight already
measuring run to their end, because each owns a thread that will `finish` it and releasing a slot
early would hand it to a second worker while the first is still in it. Cancelling a 600-server
sweep therefore returns the daemon to idle in about ten seconds rather than thirteen minutes.

**Cancelling is idempotent, and answers with a count.** Asking twice, or for a server nothing is
queued for, is not an error. The count is what lets a client tell "there was nothing left to stop"
from "this daemon is too old to ask": called with an empty list, a current daemon answers zero and
an older one answers `UnknownMethod`, which is how `Client::supports_probe_cancel` decides whether
to offer a stop control at all.

**A stop is offered only where a cancel reaches (binding).** The two preceding paragraphs are
what a client has to draw from, not merely obey: a check that is already measuring, and a check for
the server carrying the tunnel, are both beyond a cancel, so a control offering to stop either is a
control that will be pressed and do nothing. The reading of `ProbeState` therefore keeps `running`
and `queued` apart — folding them into "the daemon holds this id" answers whether a number is
pending, which is a different question — and a card whose check cannot be stopped keeps its spinner
and offers no stop. This is the same rule already stated for a daemon too old to know the verb,
applied to the check rather than to the daemon. Before any snapshot has mentioned an id there is
nothing to read the phase from, and the control switches on the press rather than waiting a poll;
the route is known from the first frame, so the connected server's check never offers a stop at all.

**A stop says what it stopped.** The count above is answered so a client can use it, and a client
that discards it leaves the user watching spinners to work out whether the press landed — about ten
seconds, on a sweep. It is reported as news and never as an error, and zero is its own sentence
rather than a number, because "there was nothing left to stop" is a different fact from a stop that
dropped nothing. The activity indicator is refreshed at the press, not at whichever later poll
happens to shrink.

**A check that was stopped is distinguishable from one that failed.** A cancelled reading, a
machine with no network and a machine with no core are three conditions and must not share one
appearance: the first is the user's own doing and the other two are not, and telling them apart is
the whole of what a card is asked right after a stop. The stopped one is ruled off rather than
coloured as a fault.

**A cancel only ever drops `Direct` jobs (binding).** A `Proxied` job is the confirmation deciding
whether a live tunnel stays up; it is keyed by profile rather than by the server a user is looking
at, and `spawn_active_probe_loop` re-enqueues it every thirty seconds regardless. Calling one off
because its server was named in a list is a different act from the one the control offers, so it
does not happen.

**A cancelled id leaves a reading, marked `ProbeDetail::Cancelled`.** This is required by the
departure invariant above, and it is also the honest answer: a card whose check was called off must
not go on presenting its previous number as though that were this measurement's. The reading carries
`ProbeFailure::Unknown`, because nothing about the server was learned, and
`LatencyMethod::default()`, because a queued job never read the config and so never learned which
method it would have used — the same answer as a server removed between the request and its slot. A
cancel is not a failure and a client must not report it as one: a cancelled sweep raises no error.
