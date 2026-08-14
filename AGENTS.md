# oxidom — agent implementation spec

`oxidom` ("oxided freedom") is a native Linux desktop **Xray/v2ray client**, built with
**GTK4 + libadwaita** in Rust. It manages subscriptions and servers, drives the **Xray core**
(a child process it configures via JSON), and routes traffic through a local proxy — with a
per-process launcher for routing individual apps. Visual inspiration: **Happ** and the
**Nautilus** (GNOME Files) aesthetic.

This file is the authoritative spec. Build workflow is **hybrid**: the core Rust (Xray control,
subscription/link parsing, routing, CLI) is written directly; the **GTK UI** (Phase 2) is
delegated to headless codex using the "GUI" section below verbatim as its brief. Keep everything
idiomatic, small, and robust. Do not add features beyond this spec without noting them in
`.notes/IDEAS.md`.

## Non-negotiable constraints
- The **GUI and CLI run unprivileged** (no root). Only the opt-in system daemon receives
  `CAP_NET_ADMIN`; oxidom never escalates on its own.
- **No secret exfiltration:** HWID/device identifiers are **never** sent unless the user opts in
  per-subscription. No telemetry.
- **Commits:** Conventional Commits, **no AI/Co-Authored-By trailer** (strict). Commit only when
  the user asks.
- Must build with `cargo build` against `Cargo.toml`. Fix all warnings. Keep `Cargo.lock`.
- Xray binary path resolves in priority order: the `xray_binary` config key, then the
  `OXIDOM_XRAY_BIN` env var (set by the nix wrapper/devShell), then `xray` on `PATH`. Resolution
  is a preflight (`xray::resolve`) that runs before spawning, so a missing core is reported as
  an actionable error naming both the path tried and where it came from.

## Build / dev
- `nix develop` gives a shell with gtk4/libadwaita/glib + rust toolchain + `xray` +
  `tun2socks`.
- `nix develop -c cargo build` builds the workspace; use
  `nix develop -c cargo run -p oxidom-gui` for the GUI and
  `nix develop -c cargo run -p oxidom -- <command>` for the CLI/daemon.
- `nix build` produces both wrapped binaries; `nix build .#oxidom-cli` and
  `nix build .#oxidom-gui` build the headless and graphical packages separately.

## Files (config & state)
Resolve config dir as `$XDG_CONFIG_HOME/oxidom` (`~/.config/oxidom`), data dir as
`$XDG_DATA_HOME/oxidom` (`~/.local/share/oxidom`). Create parent dirs on write; never panic on
missing files (treat as defaults/empty).

- `~/.config/oxidom/config.toml` — user settings (see schema).
- `~/.config/oxidom/profiles/<name>.toml` — named CLI/systemd connection profiles.
- `~/.local/share/oxidom/subscriptions.json` — cached subscriptions + parsed servers.
- `~/.local/share/oxidom/state.toml` — ephemeral `[[sessions]]` records: profile, server id,
  loopback address, fixed SOCKS/HTTP ports, recovery PIDs and the planned interface routes/rule.
  Interface intent is written before kernel application so crash cleanup may safely over-delete
  idempotent records but can never forget an applied route. The legacy flat active-server fields
  are accepted only as migration input.
- `~/.local/share/oxidom/hwid` — random per-install id (only generated/used if a sub opts in).
- `~/.cache/oxidom/egress.json` — user-owned 60-second cache for `oxidom ip --egress`, keyed by
  profile and server id.

### Which daemon owns the store (binding)
A system daemon run from the NixOS module keeps all of the above in its `StateDirectory`
(`/var/lib/oxidom`) instead of the user's XDG dirs, so **the choice of daemon is the choice of
database**. The GUI must not make that choice by accident:

- The system daemon is D-Bus **activatable** (`share/dbus-1/system-services/…`, unit `Type=dbus`),
  so a client that asks for the name starts it and waits instead of racing it.
- `DaemonClient::connect_any` falls back to a session daemon only after waiting out an installed
  system daemon (`SYSTEM_DAEMON_GRACE`), and only when the bus says *nobody owns the name*.
  `AccessDenied` is a final answer — that user is not allowed to drive the system daemon, and a
  session daemon of their own is the correct answer for them.
- Losing this race is invisible in the UI except as servers that "vanished", so the fallback is
  logged, and the connection runs off the main loop behind a startup window that says which step
  it is on. Never make the user stare at nothing while a daemon is being reached.

### `config.toml` schema (serde)
```toml
socks_port = 10808            # local SOCKS inbound
http_port  = 10809            # local HTTP inbound
system_proxy = false          # toggle GNOME/env system proxy on connect
reconnect = false             # reconnect after an unexpected core exit; explicit opt-in
latency_method = "http_get"   # one of: icmp | tcp | http_head | http_get
latency_test_url = "https://www.gstatic.com/generate_204"
subscription_user_agent = "v2rayN/6.45"    # panels gate the body *and its format* on this
xray_binary = ""              # empty: use $OXIDOM_XRAY_BIN, then xray on PATH
tun2socks_binary = ""         # empty: use $OXIDOM_TUN2SOCKS_BIN, then tun2socks on PATH
nft_binary = ""               # empty: use $OXIDOM_NFT_BIN, then nft on PATH

[core]                        # machine-wide Xray core settings; a profile's [core] overrides
log_level = "warning"         # debug | info | warning | error | none
domain_strategy = "ip_if_non_match"
noises = []
[core.sniffing]               # enabled | dest_override (http/tls/quic) | route_only
[core.mux]                    # enabled | concurrency | xudp_concurrency | xudp_proxy_udp_443
[core.fragment]               # enabled | packets | length | interval
[core.dns]                    # server | direct_server | query_strategy
```

Every `[core]` key is optional at both levels, and an untouched section is not written to the file
at all. See "Advanced core settings" below for what each one generates.

## Data model
```rust
enum Protocol { Vless, Vmess, Trojan, Shadowsocks, Socks, Http, Hysteria2 }

struct Server {
    id: String,            // stable hash of the link
    name: String,          // remark / tag
    protocol: Protocol,
    address: String,
    port: u16,
    // Transport/security summary for the card subtitle, e.g. "vless + xhttp + reality".
    transport_label: String,
    country: Option<String>, // ISO code, for the flag; parsed from name if present
    raw: OutboundSpec,       // everything needed to emit Xray outbound JSON
    latency_ms: Option<u32>, // last probe result (runtime only)
}

struct Subscription {
    id: String,
    name: String,             // from profile-title header, else user-given
    url: String,
    description: Option<String>,
    userinfo: Option<UserInfo>, // upload/download/total/expire
    send_hwid: bool,            // OPT-IN, default false
    servers: Vec<Server>,
    updated_at: Option<i64>,
}

struct UserInfo { upload: u64, download: u64, total: u64, expire: Option<i64> }
```

## Subscription fetch & parse
1. HTTP GET the subscription URL with `ureq`. Send a normal browser-ish `User-Agent`.
   If `send_hwid` is true for that sub, add the HWID header (Happ uses an `x-hwid`-style header —
   send `Hwid: <id>` and `User-Agent` including the app; **only when opted in**). Otherwise send
   nothing identifying.
2. Read response headers: `subscription-userinfo` (`upload=..; download=..; total=..; expire=..`),
   `profile-title` (may be base64 with `base64:` prefix), `profile-update-interval`.
3. Body may be base64-encoded; if it decodes to text lines, use that, else use raw text.
4. Split into lines; parse each non-empty line as a share link (below). Skip unparseable lines.

### Share-link parsers → `Server`
- `vless://uuid@host:port?<params>#name` — params: `type` (tcp/ws/grpc/xhttp/splithttp),
  `security` (none/tls/reality), `sni`, `pbk`, `sid`, `fp`, `flow` (xtls-rprx-vision),
  `path`, `host`, `serviceName`, `alpn`, `encryption`. Build `transport_label` like
  `"vless + xhttp + reality"`.
- `vmess://<base64 json>` — JSON with `add/port/id/aid/net/tls/host/path/sni/scy/ps`.
- `trojan://password@host:port?<params>#name` — tls params like vless.
- `ss://` — SIP002: `ss://base64(method:password)@host:port#name` or fully-base64 form.
- `socks://` / `http://` — optional userinfo auth.
- `hysteria2://` (alias `hy2://`) — `auth@host:port[,ranges]?<params>#name`. Params: `obfs`
  (only `salamander`), `obfs-password`, `sni`, `insecure`, `pinSHA256`, `alpn`, `up`, `down`,
  `hopInterval`, `congestion`. The port defaults to 443, the auth string is opaque and may
  contain `:`, and the comma-separated port-hopping ranges must come off the authority before
  `Url::parse` will accept the link. Settings live in `Hysteria2Settings`, not `StreamSettings`.
  Bare `hysteria://` is **v1** and stays unsupported.
Derive `country` from a leading flag emoji or country code in the name when present.

## Xray config generation
Emit Xray JSON to a temp file, then spawn the core. Structure below is the **built-in** shape;
`[core]` (see "Advanced core settings") makes each marked line configurable without moving a byte
when nothing is set.
- `log`: `{ loglevel: "warning" }`.
- `inbounds`: a SOCKS inbound on `socks_port` and an HTTP inbound on `http_port`, both bound to
  the session's stable loopback address, `sniffing` enabled (`http`, `tls`).
- `outbounds`: `[ <selected server outbound>, { protocol: "freedom", tag: "direct" },
  { protocol: "blackhole", tag: "block" } ]`.
- `routing`: default rules (v1: everything through proxy; direct for private IPs). A full rules
  editor is Phase 3.
Generate the protocol-specific `outbounds[0]` from `OutboundSpec` (streamSettings for
tcp/ws/grpc/xhttp; tlsSettings/realitySettings/xtls as needed).

A **pool** session emits the same scaffold plus one outbound per member tagged `s-<alias|id>`, a
`routing.balancers` entry `{ tag: "pool", selector: ["s-"], strategy: { type: <strategy> } }`, a
`burstObservatory` with `subjectSelector: ["s-"]`, and an `api` block with `RoutingService`
reachable through a `dokodemo-door` inbound tagged `api-in` on the session's own address.

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

Three further details are binding:

- The `api-in → api` rule comes **first** in `routing.rules`, ahead of the `balancerTag` rule.
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

### Advanced core settings (cycle 4, phase 6 — binding)

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
core refuses to start rather than quietly not matching. `pkgs.xray` is a wrapper that sets
`XRAY_LOCATION_ASSET`; a hand-set `xray_binary` pointing at an unwrapped core will fail at spawn.

## Xray process supervisor
- Resolve the binary (see above), then spawn `<resolved> run -c <configfile>`. Capture
  stdout/stderr to an in-memory ring buffer surfaced in a "Logs" view; oxidom's own failure
  reasons go into the same buffer prefixed `oxidom:`, so the Logs view explains a failure even
  when xray never started.
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

## Latency probes (`latency_method`)
- `icmp` — spawn `ping -c1 -W1 <host>` and parse (avoids raw-socket privileges).
- `tcp` — time a raw TCP connect to `host:port`.
- `http_head` — HEAD `latency_test_url` **through the active SOCKS inbound**.
- `http_get` — GET `latency_test_url` through SOCKS (Happ-style; expect 204).
List view may use a cheap method across servers concurrently (bounded thread pool); the active
connection uses the configured method.

### Probe outcomes (cycle 4, phase 2 — binding)
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
- The hysteria2 ICMP fallback retries only `Unreachable`/`Timeout`. Retrying `NoNetwork` would
  launder it into "server is dead".
- An HTTP response with an error status still proves the server carried the request: `Reachable`.

### The reading contract (cycle 4, phase 1 — binding)
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

## CLI (clap derive)
`oxidom` is the headless CLI/daemon and `oxidom-gui` is the graphical client.
`oxidom gui` remains a compatibility shim that execs the latter, passing
`--background` and `--debug` through.

`oxidom-gui` detaches from a terminal it was started from: `main` forks, calls
`setsid`, and points stdio at `/dev/null` **before** GTK or any thread exists —
fork carries only the calling thread over, so there is no later moment at which
this is safe. It is skipped when stdout is not a terminal, because then a
supervisor is watching: the tray unit runs `oxidom-gui --background` as
`Type=simple`, and a main process that forks and exits reads to systemd as a
service that died on startup. `--debug` also skips it and defaults the log
level to `debug`; `$RUST_LOG` overrides that default in either mode.

- `oxidom up [PROFILE]` (`connect-profile`) connects the `default` profile or the named one.
- `oxidom down [PROFILE]` (`disconnect`) stops the tunnel unconditionally unless a profile
  is named.
- `oxidom connect <HANDLE>` connects one server without a profile.
- With a pool, `status` prints the selection as `pool "Europe" (leastPing, 6 nodes, now →
  ch-trojan-2)` — the name is dropped when the pool has none — plus per-member health, and `ip`
  prints one endpoint per line. `--egress` stays unambiguous —
  one request through the session — and is never cached for a pool, because the exit rotates.
- `oxidom status [PROFILE] [--json]`, `oxidom ip [PROFILE] [--egress] [--fresh]`,
  `oxidom env [PROFILE]`, `oxidom list [servers|profiles|subscriptions|sessions] [--json]`, and
  `oxidom ping <HANDLE>` are read commands and never spawn a session daemon.
- `oxidom tun [PROFILE] [--down]` inspects the session interface or explicitly removes it.
- `oxidom alias <HANDLE> <NEW>` changes a server alias.
- `oxidom profile {list,show,new,edit,rm}` manages daemon-owned profiles.
- `oxidom daemon [--system --socks-port --http-port]` runs the D-Bus service.
- `oxidom <PROFILE> run -- <cmd>...` and `oxidom <PROFILE> run -c "<cmd>"` run one command
  inside the profile's routing domain. The `-c` string is split with shell-word rules but never
  passed to a shell. A proxy-only profile refuses safely and points to `oxidom env`.

Only `up` and `connect` may spawn a private session daemon; every other control command requires
an existing daemon.

The canonical order is verb first (`oxidom up work`). For profile-scoped commands, profile first
is an argv-normalized synonym (`oxidom work up`); a real subcommand in the first position always
wins. `oxidom env` prints POSIX `export` statements for both SOCKS and HTTP endpoints.

Data goes only to stdout; warnings, errors, and ambiguous-handle candidates go to stderr. JSON
uses the fixed DTOs in `oxidom-core/src/cli_json.rs`. Exit codes are binding:

| Code | Meaning |
|---:|---|
| 0 | Success |
| 1 | Command error |
| 3 | No active connection |
| 4 | Daemon unavailable |

### Handles and aliases (binding)

Server ids use hand-written FNV-1a 64 and aliases are globally unique, stable human handles.
`handle::resolve` prefers an exact alias, then an exact id, then a unique case-insensitive
substring of alias or name. No match is an error; multiple substring matches are an error with
the candidates listed in stderr. Aliases are lowercase ASCII letters/digits/hyphens, at most 32
characters, and cannot be exactly 16 hexadecimal characters.

### Profiles (binding)

Profiles live in `profiles/<name>.toml` below the daemon's config directory:

```toml
description = "work"

[select]
server = "ch-trojan"

[proxy]
socks_port = 10808
http_port = 10809

[interface]
enable = true
routes = "manual"
```

Names match `^[a-z0-9][a-z0-9_-]{0,31}$`; command names and aliases are reserved so profile-first
argv is never ambiguous. Existing reserved-name files remain readable/listed with a warning, but
new writes are refused. `UpProfile` resolves the profile's selection and applies that profile's
ports; unit-pinned ports constrain only `default`. Removing a profile deliberately leaves its
running session intact so the unit can still stop what it started.

A profile selects **either** one server **or** a pool, never both:

```toml
[select.pool]
name           = "Europe"         # label only; never selects anything
strategy       = "leastLoad"      # leastLoad | roundRobin | random | leastPing
members        = ["ch-one", "de-two"]   # a *list*; mutually exclusive with the filters below
subscriptions  = ["main"]         # empty = every group, including "My servers"
countries      = ["ch", "de", "nl"]
protocols      = ["vless", "trojan"]
exclude        = ["ch-trojan-3"]
max            = 8                # 0 = uncapped
probe_interval = "5m"
```

A pool is made either of a **list** (`members` non-empty, `PoolKind::List`) or of a **rule**
(the filters, `PoolKind::Rule`). The distinction is the user's, not an implementation detail: a
rule cannot be looked at and it *grows* — a server added by tomorrow's refresh joins on its own —
while a list can be counted and never gains a member without being edited. Losing one is
expected and is just a server going away. Freezing a list as "no filters plus everything else
excluded" looks equivalent and is not: a server that did not exist when the list was frozen is
in nobody's exclusions, so it would silently join. `Profile::validate` **rejects** a pool that
sets both, rather than letting the file claim one thing and the tunnel do another; `resolve`, the
pure function underneath, lets the list win so a config that slipped through cannot half-apply.

`name` is carried into `SelectionInfo` and printed by `oxidom status` (`pool "Europe" (…)`). It
takes no part in selection, and `engine::pool_fingerprint` hashes resolved members, so renaming a
pool never makes a running session stale. Both `name` and `members` are `skip_serializing_if`
empty, so every pool profile written before lists existed still round-trips to the same bytes.

The same `PoolQuery` drives the balancer and the GUI's server filter: a filter is a pool
constructor, not a second search. A **group** in the GUI is a saved `PoolQuery` under a name, so
connecting one writes it straight into `select.pool` — the daemon never learns a new noun. Group
membership is therefore edited only where the servers are, in the Selection dialog on the Servers
page; the profile editor reports the pool and edits only `strategy`, `max`, `expected` and
`probe_interval`, carrying everything about *which* servers through untouched. The window says
**group** for all of this; `pool` stays the word in the TOML, the CLI and the IPC payload, and one
line in the profile editor says the two are the same thing.

**One dialog says what a selection is; a name is what saves it.** `present_selection_dialog`
(`SelectionIntent::{Filter, Name, Edit}`) is the only editor: optional `Name` and `Icon` at the
top, then the matching rows — three `AdwExpanderRow`s (Country, Protocol, Subscription) holding a
checkbox per choice, `Except` as an `AdwActionRow` with a picker of its own because it is a search
over a provider's two hundred nodes — then the hand-picked list, then a summary line. `Apply`
shows the selection without saving it, `Save` needs a name. The `Filter` pill, `New group` and
`⋮ → Edit…` all open this one dialog, differing only in what it opens with.

**`GroupKind` is derived, never asked.** Naming servers by hand is what freezing them means, so a
draft with members saves as `List`; a rule with no members keeps matching, so an empty member list
saves as `Rule`. The List/Rule radio that used to ask this was the user classifying their own
selection in storage vocabulary before they had made it, and it could only tell the truth by
disabling itself. What it conveyed that way is now stated: a selection named while the search box
is non-empty freezes into a list (search has no rule equivalent), and `Save` refuses a nameless-
rule-with-no-fields, which would mean every server. Favourites is `list_only`: the star writes
members, and a Favourites with filters is a pool `Profile::validate` rejects.

**`⋮` acts on the selection on screen, not on the selected group.** It is never insensitive.
`New profile from this…` lives there — it is the only way to make a *new* profile from the visible
selection, and it needs no name for that selection, so it works on an unsaved filter too. The
group-only items (`Edit…`, `Update to what's shown`, `Move left/right`, `Delete`) are simply
absent when no group is selected, rather than present and dead.

**A group stores selection; the Connect bar states rotation width.** The bar carries a rotation
picker defaulting to `pool::DEFAULT_POOL_ROTATION` (6), and `connect_query` writes it into
`expected`. It is deliberately *not* also stored on the group: a group answers "which servers",
the width answers "how many at once for this run", and a second copy is how the two come to
disagree. Consequently `same_pool` compares selection only while `same_rotation` compares
`expected`, and a changed width yields `PoolAction::RetuneAndUp` — neither a no-op (which would
drop the width just chosen) nor `RepointAndUp` (which would ask the user to confirm replacing a
pool with itself). It rewrites without a dialog and says so in a toast. `pool_for_profile` takes
`expected` from what the bar chose and still carries `max` and `probe_interval` through from the
saved profile, because nothing outside the profile editor can express those two.
The default exists because `expected = 0` means "all", and a country-wide pool is mostly repeats
of a handful of hosts: rotating over all 42 buys no more spread than its 9 distinct addresses
while costing an observatory ping per entry.
Two blind editors for one thing is how a saved profile comes to disagree with the group it was
made from. `leastLoad` is the default because the point of a pool is to
spread activity across exit IPs *and keep working*; `roundRobin` was measured on Xray 26.3.27 to
keep unreachable nodes in the rotation, and `leastPing` concentrates every connection on one node.
`server = ""` with `[select.pool]` absent stays valid — that is a freshly created profile, and
only `UpProfile` refuses it.

`pool.resolve` is pure and its order is deterministic because the resolved list becomes both the
config's outbound tags and the session's stored membership: a rule follows subscription order then
server order within a group; a list follows the order the user arranged, which is why `max`
truncating it is still meaningful. `subscriptions` match a group id exactly or a group name
case-insensitively; `members` and `exclude` match an alias or id **exactly** — substring matching
there would silently drop half a pool or enrol a server nobody chose. A handle listed twice
(alias and id) yields one outbound, or the `s-<handle>` tags would collide. Servers whose spec is
`OutboundSpec::XrayProfile` are never pool members: such a server is itself a balancer.

`resolve` stays silent about what it dropped because the GUI calls it on every keystroke. Two
companions report it once, at `up`, where a user can act: `excluded_composites` for balancer
servers that cannot become outbounds, and `missing_members` for handles a list names that no
subscription holds any more. Neither is fatal — only an empty result is.

**Activation resolves through `pool.resolve_ranked`, not `resolve`** (binding). Membership is
identical — a test pins that — but the order is not, and two jobs ride on the order that
subscription order does badly:

- **A pool spreads over exit addresses, not over subscription entries.** Providers list one host
  many times; on the store this was found on, 26 of 42 German entries shared `31.12.75.21:2087`
  and the whole set covered 9 addresses. `max` cutting "the first 6" therefore bought six
  spellings of one exit IP. `resolve_ranked` groups candidates by `address:port` and deals the
  groups out one apiece before any group gets a second, so a capped pool spends its budget on
  different hosts. `distinct_endpoints` is the honest count, and `up` warns when it is below the
  member count.
- **The first member is the pool's opening exit**, per the observatory note above. Groups are
  ordered by the best `pool::Known` in each, so a node that last answered opens the pool.
  `known_state` maps the daemon's direct readings onto that; a `Proxied` reading describes a
  tunnel rather than a server, and `NoNetwork`/`Unknown` are this machine's failure, so both rank
  as `Unmeasured` rather than as a verdict on somebody's node. Measured end to end: the same
  41-node German rule took ~25 s to confirm with nothing measured and **70 ms** once two of its
  nodes had been probed.

Ranking never changes who is in the pool, so a list still gets everyone the user named; losing a
named member stays `missing_members`' job.

**The exit count is reported, not only logged.** `SelectionInfo.endpoints` carries it to every
surface, and both `oxidom status` (`… 42 nodes on 9 exits …`) and the Profiles page's `Nodes` row
(`6 of 42 in rotation · 9 exit addresses`) say it — but only when it is *below* the member count,
because on a pool where every node is its own host it is one number printed twice. Zero means an
older daemon did not report it, so nothing renders zero as a count. It is counted inside
`selection_info`, which already looks every member up to name it, rather than snapshotted at `up`:
that loop is the cost, and a `Status` paying it anyway may as well answer the question. Members
that no subscription holds any more contribute no endpoint, so a shrunken pool understates rather
than invents.

**A pool's node count is explained by its strategy, not by its width.** `reduce::rotation_help`
is keyed on the running strategy, deliberately *not* shared with the Connect bar's
`rotation_detail`: that one is one sentence about a width being chosen and describes `leastLoad`,
and `roundRobin` keeps unreachable nodes in the rotation, so the same sentence would be false.
Two facts, two sentences.

Pool membership is resolved **once, at `up`**. A subscription refresh that changes what the
query would match marks the session `stale` and invites a reconnect; it never rewrites the
config under live connections.

Every profile gets a stable `127.<a>.<b>.1` inbound address derived with the same FNV-1a 64 used
for ids; collisions probe forward through the address space. `default` is permanently
`127.0.0.1`, an external contract with consumers such as redsocks. Different addresses let every
profile reuse the same SOCKS/HTTP ports.

### Sessions (binding)

Subscriptions and servers are global sources. A profile is persistent configuration; a session
is the ephemeral running instance of one profile: its Xray process, selection, loopback
address, ports, optional interface and status. “Connect one server” means the `default` session,
not a separate global mode. Several profiles may run at once, including several profiles on the
same server.

A session's selection is a single server **or** a pool, and a pool session has no active server:
`Session::server_id()` returns `None` for it, deliberately, so nothing can quietly pick the first
member and call it the exit. Everything that means “the tunnel is carrying server X” — the
connected highlight, the proxied reading, the egress cache — is therefore keyed by session, and
a pool reports its live exit from the observatory instead.

`Sessions` is a `BTreeMap` keyed by profile, so listings are stable. A profile already up cannot
be brought up twice. `DownProfile` removes exactly that runtime; `Down("")` removes all sessions.
The single GNOME system-proxy owner is runtime state in `Sessions`, not persistent configuration:
it is released when its session stops or its core dies. On daemon startup, each recovered Xray
PID is checked against its own `current-config-<profile>.json`, each tun2socks PID must contain
`--device <our-device>`, and recorded interface resources are removed before the state entry is
forgotten.

## Interfaces (binding)

`[interface] enable = false` is exactly the unprivileged proxy-only behavior. When enabled, the
system daemon (and only it) requires `CAP_NET_ADMIN`; the NixOS module grants it only with
`services.oxidom.tun.enable = true` and keeps `oxi-*` unmanaged by NetworkManager.

The daemon unit sets `KillMode=process` because the Xray cores, not the daemon, carry the
traffic. Under systemd's default the whole cgroup dies with the daemon, so a crash drops every
tunnel and the restarted daemon finds nothing to adopt — which silently made the pool-adoption
path in `recover()` unreachable in production. A clean stop still tears the cores down through
the daemon's own signal handler, and anything that leaks is reaped on the next start.

- Device names default to `oxi-<profile>` and fit Linux's 15-byte IFNAMSIZ payload; an explicit
  valid `device` is required for longer profile names.
- Device addresses are stable `198.18.<c>.<d>/32` values from the RFC 2544 benchmark block.
  `default` is fixed at `198.18.0.1`; `/32` is binding because it adds no connected route.
- fwmark, private table id and rule priority are the same stable value. `default` is `0x6f00`;
  other profiles probe within `0x6f01..=0x6fff`, avoiding the user's `0x1`/`0x2`/`0x3` policy.
- Every enabled interface gets the current default network's link-scope connected routes plus
  `default dev <device>` in its private table, and a matching fwmark rule. The connected routes
  keep LAN and its resolver reachable from `oxidom run`. `routes = "manual"` changes no system
  route; `list` adds only its CIDRs; `default` adds a host route to the server via the old gateway
  plus two half-defaults through the device.
- Bring-up order is persistent TUN, address `/32`, tun2socks spawn, link-up, private route/rule,
  then system routes. The spawn-before-link order and double-dash tun2socks flags are live-tested
  contracts.
- Ordinary `down` stops tun2socks and removes oxidom routes/rule but leaves the persistent device,
  preserving hand-written routes across reconnects. `tun --down` and crash recovery additionally
  delete the device only when oxidom created it.
- Per-process routing uses a transient `systemd --user` scope below
  `oxidom-<profile>.slice`. The daemon atomically owns one `socket cgroupv2` mark rule per session
  in `table inet oxidom`; the CLI verifies `/proc/self/cgroup` inside the scope before `exec`.
  Cleanup removes the profile chain before taking down its routing domain, so traffic is never
  silently released onto the ordinary default route.

## GUI (Phase 2 — codex brief)
Build with `adw::Application` (app id `dev.keepinfov.oxidom`). Wire to the core modules; do not
reimplement parsing/Xray logic in the UI layer.

Layout (from the mockups + Nautilus feel; dark, rounded, generous spacing):
- **`adw::NavigationSplitView`** (or `OverlaySplitView`) for the adaptive sidebar.
  - **Sidebar:** app/logo area at top; a nav list — at least "Servers" (the server browser) plus
    entries for Subscriptions, Settings, Logs; a bottom action row (e.g. connection status /
    quick connect). Collapses in narrow mode with a small toggle button in the header.
  - **Content:** `adw::HeaderBar` with standard window controls; a **search entry** spanning the
    top that filters servers by name/protocol/country.
- **Server card** (custom widget): country **flag**, server **name**, protocol **subtitle**
  (`transport_label`, e.g. "vless + xhttp + reality"), optional **latency badge** (green when
  low). Whole card is a click target → selects that server. Every server carried by a connected
  profile is visually marked; the one-profile case is unchanged.
- **Groups:** a chip row above the grid — `All`, one chip per saved group, `+`. A group narrows
  **the one list**, and is never a second block of cards: rendering it as its own block
  shows the same server two or three times and leaves no way to tell which card is real, and
  "show it only in its highest-priority group" makes a starred server vanish from its
  subscription. Cards stay in their subscription; the chip narrows what is shown. Selecting a
  chip reveals a Connect bar that points the **selected profile** at that group — the same rule a
  card click follows. Favourites is a built-in list; the card's star is what fills it, and it
  cannot be deleted because the star would have nowhere to put things.
- **Server grid:** a top block of "loose"/favorite servers, then one **block per subscription**:
  each block shows its **title** + **description** (name + quota/expiry from userinfo) followed by
  that subscription's server cards. In the code a block is `SubscriptionBlock` — never a "group",
  which the window uses only for the saved selections in the chip row. Multi-column grid in wide mode, single column in narrow
  (use `adw::WrapBox`/`FlowBox` with a breakpoint via `adw::Breakpoint`).
- **Connect control:** a single primary **Connect/Disconnect** toggle in the header for the
  compatibility `default` session. Show its live status (Connecting/Connected/Error) and active
  latency. If other sessions run, a persistent banner reports their count and points to
  `oxidom status`.
- **Subscriptions view:** add (URL + optional name), update-now, delete; per-sub **"send HWID"**
  switch (default OFF) with a privacy hint.
- **Settings view:** ports, system-proxy toggle, latency method + test URL.
- **Logs view:** the core's ring-buffer output.

Responsiveness: define a breakpoint (~700px) that collapses the split view and switches the grid
to a single column, exposing the small sidebar toggle button (as annotated in the narrow mockup).

## Module layout
```
Cargo.toml                       # virtual workspace manifest
crates/
  oxidom-core/
    src/
      lib.rs
      bind.rs                    # stable inbound/interface identities and routing marks
      client.rs                  # blocking D-Bus client, shared by GUI and CLI
      config.rs                  # config.toml load/save
      state.rs                   # state.toml
      model.rs                   # Server/Subscription/OutboundSpec types
      pool.rs                    # PoolQuery (list or rule) + pure membership resolution
      engine.rs                  # Registry + per-profile Session/Sessions facade
      proc.rs                    # shared child supervision and recovered PID inspection
      resolve.rs                 # shared config → env → PATH binary resolver
      run.rs                     # systemd user scope + cgroup verification/exec
      nft.rs                     # atomic per-profile cgroup mark rules
      nft/
        resolve.rs               # nft binary spec
      link.rs                    # share-link parsers
      subscription.rs            # fetch + decode + userinfo headers + hwid
      xray/
        api.rs                   # `xray api bi` — live balancer selection and health
        config.rs                # OutboundSpec -> Xray JSON
        core.rs                  # process supervisor + status
        resolve.rs               # Xray binary preflight
      probe.rs                   # latency probes
      tun.rs
      tun/
        caps.rs                  # CAP_NET_ADMIN preflight
        core.rs                  # tun2socks process supervisor
        device.rs                # persistent TUN ioctls
        net.rs                   # blocking rtnetlink facade
        plan.rs                  # pure route planning
        resolve.rs               # tun2socks binary spec
  oxidom/
    src/
      main.rs                    # CLI entry + dispatch
      cli.rs                     # clap definitions + GUI binary shim
      daemon.rs                  # headless D-Bus service
  oxidom-gui/
    src/
      main.rs                    # GTK application entry
      gui/                       # window, sidebar, cards, groups, views
    data/flags/
```

## Definition of done
- `cargo build` clean; `Cargo.lock` present; `nix build` works.
- `oxidom` opens the adaptive window; sidebar collapses under the breakpoint.
- Adding a base64 subscription lists its servers grouped with title/description; userinfo quota
  shows when present.
- Selecting a server + Connect starts Xray, exposes the local SOCKS/HTTP proxy, and shows
  Connected + a real latency; Disconnect stops it cleanly.
- HWID is never sent unless the per-sub switch is on.
- `oxidom <profile> run -- <cmd>` routes only that command through the profile while ordinary
  neighboring commands keep their existing route.
```
