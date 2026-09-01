# Xray config generation and supervision

What oxidom emits into the Xray core's JSON, how `[core]` settings change it, and how the core
process is supervised.

## Xray config generation

Emit Xray JSON to a temp file, then spawn the core. Structure below is the **built-in** shape;
`[core]` (see "Advanced core settings") makes each marked line configurable without moving a byte
when nothing is set.
- `log`: `{ loglevel: "warning" }`.
- `inbounds`: a SOCKS inbound on `socks_port` and an HTTP inbound on `http_port`, both bound to
  the session's stable loopback address, `sniffing` enabled (`http`, `tls`).
- `outbounds`: `[ <selected server outbound>, { protocol: "freedom", tag: "direct" },
  { protocol: "blackhole", tag: "block" } ]`.
- `routing`: default rules (v1: everything through proxy; direct for private IPs), preceded by
  whatever the profile's own `routing` block carries. A rules editor is not implemented yet; the
  block is the way to say anything the generator does not.

Generate the protocol-specific `outbounds[0]` from `OutboundSpec` (streamSettings for
tcp/ws/grpc/xhttp; tlsSettings/realitySettings/xtls as needed).

A socks or http outbound emits its `users` entry when the link carried **either** half of the
credential, with the missing half as an empty string — Xray accepts an empty `pass`. Requiring
both halves made a `socks5://user@host` link dial unauthenticated, with nothing anywhere saying
so.

A **pool** session emits the same scaffold plus one outbound per member tagged `s-<alias|id>`, a
`routing.balancers` entry `{ tag: "pool", selector: ["s-"], strategy: { type: <strategy> } }`, a
`burstObservatory` with `subjectSelector: ["s-"]`, and an `api` block with `RoutingService`
reachable through a `dokodemo-door` inbound tagged `api-in` on the session's own address.

**The observatory's destination is `[core] pool_probe_url` (binding).** The balancer puts a node
in rotation only once the burst observatory has reached that address *through* that node, so an
address that cannot be reached from where the user is means every pool on the machine carries
nothing — with a rotation count as the only symptom. It was a constant, which made that outcome
unfixable; it is now a `[core]` key, resolved profile over machine over built-in like every other,
because two pools through two countries need not share a reachable destination. Deliberately not
the settings' `latency_test_url`: that one is only editable while the probe method is HTTP, so
reusing it would drive every pool through an address the interface would not always let the user
change.

**A provider's `pingConfig` is still discarded whole, unconditionally.** Only its *presence* is
read, to decide whether the imported profile wanted an observatory at all. The destination the
generator writes is always oxidom's — built-in or configured — because a destination chosen by
somebody else is a URL the core fetches on a timer through the user's own exits, i.e. a beacon.
Making the overwrite conditional on the value being a default would put that back.

**An unusable configured destination falls back to the built-in.** It is refused where it is
written, so the normal answer is a sentence at `SetSettings` or `SaveProfile`; a value that got
past that — an older file, or one edited by hand — is ignored rather than emitted, because a pool
with no working health check puts nothing in rotation and carries nothing, which is a worse answer
to a bad URL than disregarding it.

**A pool that carried no traffic says whether its health check ever succeeded.** The count on its
own names the symptom rather than the cause, and under `roundRobin` it actively misleads: every
node stays eligible, so the message reads "3 of 3 nodes were in rotation" while nothing works.
Where the core's own log carries failed observatory pings, they outrank the count and the message
names the setting that changes the address. This is read from the core's log rather than folded
into `probe::classify_complaint`, which is shared with the single-server probe path and answers
with a verdict on one server.

**An observatory is not by itself a failover — the strategy decides.** Measured on Xray 26.3.27
with one reachable and one unreachable outbound, twelve requests each:

| Strategy | Sent to the dead node | Notes |
|---|---:|---|
| `roundRobin` | **6 of 12** | observatory logs the failed pings and the balancer ignores them |
| `leastLoad`, `settings.expected: N` | **0** | rotates evenly across the reachable nodes it selected |
| `leastPing` | 0 | settles on one node, which defeats spreading |

`leastLoad` with `expected` is therefore the default: it is the only strategy that both spreads
traffic across exit IPs — the entire point of a pool — and drops nodes that stopped answering.
`expected` above the live count returns exactly the live ones, so "rotate across everything
reachable" is `expected = <pool size>`, which is what `expected = 0` resolves to. The generator
emits `settings` only for `leastLoad`; emitting it elsewhere would imply a filtering that the
other strategies do not perform.

### A profile's own routing block (binding)

`profile.routing` holds an Xray `routing` object as written. Its rules are spliced into
`routing.rules` **ahead of the ones oxidom installs** — a user rule about a private address wins
over the built-in `geoip:private` rule below it, which is the entire point of carrying one — and
anything else in the block (`domainMatcher`, say) is copied onto the routing object verbatim. The
two positions below keep theirs regardless.

`xray::routing::validate` refuses four things, at `SaveProfile` and again at `UpProfile`, so the
reason is a sentence rather than an Xray exit code:

- **`balancers`, and any rule with a `balancerTag`.** Balancing is oxidom's. A selector is a prefix
  match over outbound tags, and one that resolved to `direct` would send the tunnel out in the
  clear while the interface said Connected — the same reason imported outbounds are re-tagged.
- **`domainStrategy`**, which `[core] domain_strategy` already owns at two levels with a defined
  precedence. A second spelling that silently won would make the editor lie.
- **An `outboundTag` naming an outbound that will not exist.** The tags oxidom emits are `direct`,
  `block`, and — for a single-server profile only — `proxy`; a pool reaches its members through the
  balancer, so a rule aimed at `proxy` there is refused naming that as the reason.
- **A rule with no `outboundTag`**, which decides nothing.

The block is **not** a `[core]` key, and `CoreOptions::resolve` never fills
`ResolvedCore::routing`. That is what keeps it away from probes: a probe folds the machine-wide
`[core]` with no profile, and a rule that reached one could send the measurement out `direct` and
report a dead server as fast. `Engine::configure_core` is the only writer.

The interface has no editor for it. The profile dialog reports how many rules it holds and writes
back what it loaded, the same call as the pool membership beside it and the noise packets below it.

Three further details are binding:

- The `api-in → api` rule comes **first** in `routing.rules`, ahead of the `balancerTag` rule and
  ahead of the profile's own rules: a user rule matching that inbound would hang `xray api bi`, and
  nothing a user writes is about it.
  Otherwise API traffic falls into the balancer and `xray api bi` hangs.
- `selector: ["s-"]` is a prefix match, so no other outbound may start with `s-`. That is the
  whole reason for the tag scheme.
- The api port is allocated on the session address and persisted in `state.toml`; after a
  `kill -9` the daemon re-adopts the running core and would otherwise lose the way to ask it
  anything.

A single-server session emits none of this — its config is byte-identical to what it was before
pools existed, and a test pins that.

**A pool is not ready when its inbound binds** (binding, measured 2026-07-30 on Xray 26.3.27).
Until `burstObservatory` returns its first round the balancer has no ranking and hands the
request to the **first outbound in config order** — which is `pool::resolve` order, i.e.
subscription order. A single confirmation probe therefore measures member #1 and nothing else.
Measured with eight members of which six were dead: dead member first failed **6 of 6**
attempts, the same eight with a live member first connected **3 of 3**. Real subscriptions
always carry dead nodes, so this made pools over them fail outright — the reported symptom was
"a big German pool never comes up", where 40 of 42 nodes were dead.
`Shared::confirm_pool` therefore retries within `POOL_CONFIRM_WINDOW` (20 s, gap
`POOL_CONFIRM_RETRY_GAP`) instead of firing once, and stops early when the attempt is superseded
or the core has already exited. It costs a healthy pool nothing — the first attempt succeeds,
measured at 47 ms — and a single-server session keeps the single shot, because it *is* ready
when its inbound binds. A pool that still fails reports counts (`0 of 3 nodes were in rotation`)
via `confirmation_failure`; it must never speak of "the active server", which a pool has not got
by construction.

The daemon reads balancer state by running `xray api bi --json` against that inbound (no gRPC
client, no new dependency) from its background loop, never from `Status`: the GUI polls `Status`
twice a second and it must not block on a subprocess.

That command's help promises health, strategy and selecting; the wire schema of Xray 26.3.27
carries none of them. What comes back is `override.target` and `principleTarget.tag`, and
**`principleTarget` answers a different question per strategy** — verified live, not read from
documentation:

| Strategy | `principleTarget` holds | A missing tag means |
|---|---|---|
| `roundRobin`, `random` | every node still considered eligible | the observatory dropped it — this *is* the health signal |
| `leastPing`, `leastLoad` | the one node it picked | nothing; the others merely lost |

So `xray::api` reports the raw set and refuses to interpret it, and the daemon — which knows the
strategy — decides. A rotating pool has no single current exit to name and reports how much of
the rotation survives; a picking pool names its node and claims nothing about the rest. An
override pins one target under any strategy. No per-node delay is reported anywhere, because
none is available and an invented one would read as a measurement.

Two Xray 26.x details that are easy to get wrong and are covered by tests — verify any change
against a real core with `xray run -test -c <file>` rather than against documentation:
- `allowInsecure` was **removed** and makes the core refuse to start when true. Never emit it;
  the replacement is `tlsSettings.pinnedPeerCertSha256`, a bare **hex** string (not an array,
  not base64).
- hysteria2 is `protocol: "hysteria"` with `settings.version == 2`; the credential goes in
  `streamSettings.hysteriaSettings.auth`, and salamander obfuscation is
  `streamSettings.finalmask` — a single object beside `hysteriaSettings`, *not* the `udpmasks`
  array that appears in Xray's protobuf.

### Advanced core settings (binding)

`core_options.rs` owns them. Two levels: `[core]` in `config.toml` for the machine, `[core]` in a
profile for one tunnel. `None` means "not set at this level", so a profile that mentions one field
does not reset the rest; `CoreOptions::resolve` folds profile over global over built-in, and
`Origin::of` derives which of the three won without a structure of its own.

| Setting | Where it lands |
|---|---|
| `log_level` | `log.loglevel` |
| `domain_strategy` | `routing.domainStrategy` |
| `sniffing.{enabled,dest_override,route_only}` | both inbounds |
| `dns.{server,direct_server,query_strategy}` | top-level `dns`, absent unless `server` is set |
| `mux.*` | **every** proxy outbound — in Xray this is an outbound field, so a pool carries it per member |
| `fragment.*`, `noises` | a `freedom` outbound tagged `dialer`, plus `sockopt.dialerProxy` on every proxy outbound |

Three invariants:

- **`dialer` does not start with `s-`.** That is the whole point of the tag scheme
  (`SELECTABLE_TAG_PREFIX`): a balancer selector is a prefix match, and one that resolved to the
  fragmenter would send a pool's traffic out through plain freedom while the UI said Connected.
- **The `dialer` outbound exists exactly when fragmentation or noises do**, and nothing points at
  it otherwise. The core accepts a dangling `dialerProxy` silently, so only oxidom's own test
  catches the pairing breaking.
- **Unset means absent, not defaulted.** `routeOnly: false`, an empty `mux`, an empty `dns` are all
  left out; `default_bind_keeps_the_legacy_config_bytes` fails if that stops being true.

**`xray run -test` is necessary and not sufficient.** Measured on 26.3.27: the core rejects a bad
`destOverride`, `mux.xudpProxyUDP443`, `noises[].type`, `sockopt.domainStrategy` and a zero-minimum
range — but *silently accepts* `loglevel: "loud"`, `domainStrategy: "Whatever"`,
`queryStrategy: "UseNothing"`, a reversed range like `"200-100"` (and then fragments nothing), any
`mux.concurrency` including 4096 and -2, a `dialerProxy` naming no outbound, and **any unknown key
anywhere**. Everything in that second list is validated by `CoreOptions::validate`, because nothing
downstream would.

The TOML spellings are snake_case; the wire spellings are not (`IPIfNonMatch`, `UseIPv4`). They are
deliberately separate — `as_xray()`, not the serde representation — so renaming a key in the file
format cannot silently change the generated config.

**The GUI** puts the same rows on both levels, from one widget group —
`gui/views/core_editor.rs`, parameterised by `CoreLevel`. The levels differ only in what "unset"
means, and that difference is the module:

- `Machine`: nothing below but the built-in values, so a field equal to the built-in is stored as
  `None` (`drop_built_ins`, field by field). Without it the first Apply on an unrelated setting
  would write a whole `[core]` table into a file nobody configured.
- `Profile`: sections carry an enable switch and are owned or inherited whole, which is exactly
  what a `[core.mux]` table in a profile file means. An inheriting section shows what it inherits
  in its subtitle, so nobody has to switch one on to look — switching one on to look is how a
  profile ends up pinning a value nobody meant to pin.

Two things this required elsewhere. `Config.core` is `skip_serializing_if`, which also drops it
from the D-Bus payload, and the daemon reads an absent `core` as "keep what you have" so an older
client cannot erase it; `DaemonClient::apply_settings` therefore re-inserts the key explicitly,
otherwise turning the *last* core setting off would never reach the daemon. And `noises` has no
editor at all: a list of byte patterns with no useful default is carried through untouched and
reported by count, the same call as the pool membership in `profile_dialog.rs`.

Geo data is a runtime dependency of **every** config oxidom generates, not just of future routing
rules: `geoip:private` has been in the default rule set from the start, and without `geoip.dat` the
core refuses to start rather than quietly not matching. The managed Xray install carries both lists
beside its binary; an explicit `xray_binary` must provide usable lists itself.

**Whether the data is usable is decided by the core, never by looking at the filesystem.** oxidom
asks with `xray run -test` over a configuration carrying exactly the two references above and
nothing else — no inbound, so nothing is bound and nothing leaves the machine. This is not
fastidiousness: that same `pkgs.xray` wrapper exports `XRAY_LOCATION_ASSET` *inside itself*, from a
store path unrelated to the binary's directory, so read from oxidom's own environment the variable
is unset and no conventional directory exists. A filesystem check answers "missing" on the platform
where this has always worked. Asking the core also tells a **corrupt** list from an absent one: a
truncated file is refused as `code not found in geoip.dat: PRIVATE`.

oxidom keeps its own copy in `data_dir()/assets` and sets `XRAY_LOCATION_ASSET` on the core it
spawns — both the live one and a probe's — under two conditions, either of which alone would break
a working machine:

- nothing has already chosen a location. The wrapper reads `${XRAY_LOCATION_ASSET-<store path>}`,
  so anything oxidom exports *wins*, and overriding a deliberate choice is not oxidom's to make;
- oxidom holds **both** files. The variable names one directory, so exporting a half-populated one
  hides whichever list the core would have found for itself.

Otherwise the child's environment is left exactly as it was, and a machine that works today is
unaffected. Files already present elsewhere — from a distribution package or another client — are
used where they lie; oxidom offers to copy them into its own directory only because the variable
names a single directory and another program's may change under it.

## Xray process supervisor

- Resolve the **pinned Xray 26.3.27** binary before spawning. With an empty `xray_binary`, oxidom
  installs the matching official Linux archive into `data_dir()/xray/26.3.27`, verifying its
  source-pinned SHA-256 before extraction; that private copy is used thereafter. If the install is
  unavailable (for example, offline), `$OXIDOM_XRAY_BIN` and then `xray` on `PATH` are accepted only
  when `xray version` reports exactly `Xray 26.3.27`. An explicit `xray_binary` is likewise a
  matching-version override. A differently-versioned core is refused rather than silently given a
  configuration it was not tested against. Then spawn `<resolved> run -c <configfile>`. Capture
  stdout/stderr into the process log book
  (`oxidom_core::logbook`), tagged `xray`, parsed for the `[Level]` and subsystem the core prints.
  `tun2socks` writes to the same book tagged `tun2socks`, and oxidom's own reasoning — the `log`
  facade as well as `note`/`fail` — is tagged `oxidom`. The Logs view therefore explains a failure
  even when xray never started, and says which of the three said so.
- **The book is never cleared on connect (binding).** It is shared by every session, so wiping it
  for one erases the others. Each run's boundary is a `spawn_seq` watermark taken at the top of
  `connect`, and callers that diagnose a failure from the core's own words read only records at or
  after it **and** tagged `xray`. Both halves are required: without the watermark a marker from a
  previous attempt is read as this attempt's reason, and without the source filter oxidom's own
  note about an unrecognised obfuscation type matches `UNSUPPORTED_PROTOCOL_MARKERS`.
- Track state: `Disconnected | Connecting | Connected | Error(msg)`. Consider "Connected" once
  the process is up and a latency probe through the SOCKS inbound succeeds.
- Stop cleanly on disconnect/app-exit (SIGTERM, then SIGKILL after timeout). The same escalation
  applies to an orphan inherited from a crashed run (`engine::kill_stale_xray`), which verifies
  the process is gone rather than assuming SIGTERM worked.
- One core process per running profile; `Sessions` owns them in stable profile-name order.
- **An unexpected exit is noticed by the daemon itself** (`daemon::spawn_core_supervisor`, 1 s
  tick), for every session and not only while a GUI polls `Status()`. With `reconnect = true` the
  supervisor redials that profile's server with a 1 s→30 s backoff, cancelled by an explicit
  operation in the same profile's generation domain. Default is off: silently redialling hides a
  server going bad.
