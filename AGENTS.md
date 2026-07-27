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
subscription_user_agent = "v2rayNG/1.9.5"  # panels gate the body on this
xray_binary = ""              # empty: use $OXIDOM_XRAY_BIN, then xray on PATH
tun2socks_binary = ""         # empty: use $OXIDOM_TUN2SOCKS_BIN, then tun2socks on PATH
```

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
Emit Xray JSON to a temp file, then spawn the core. Structure:
- `log`: `{ loglevel: "warning" }`.
- `inbounds`: a SOCKS inbound on `socks_port` and an HTTP inbound on `http_port`, both bound to
  the session's stable loopback address, `sniffing` enabled (`http`, `tls`).
- `outbounds`: `[ <selected server outbound>, { protocol: "freedom", tag: "direct" },
  { protocol: "blackhole", tag: "block" } ]`.
- `routing`: default rules (v1: everything through proxy; direct for private IPs). A full rules
  editor is Phase 3.
Generate the protocol-specific `outbounds[0]` from `OutboundSpec` (streamSettings for
tcp/ws/grpc/xhttp; tlsSettings/realitySettings/xtls as needed).

Two Xray 26.x details that are easy to get wrong and are covered by tests — verify any change
against a real core with `xray run -test -c <file>` rather than against documentation:
- `allowInsecure` was **removed** and makes the core refuse to start when true. Never emit it;
  the replacement is `tlsSettings.pinnedPeerCertSha256`, a bare **hex** string (not an array,
  not base64).
- hysteria2 is `protocol: "hysteria"` with `settings.version == 2`; the credential goes in
  `streamSettings.hysteriaSettings.auth`, and salamander obfuscation is
  `streamSettings.finalmask` — a single object beside `hysteriaSettings`, *not* the `udpmasks`
  array that appears in Xray's protobuf.

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
- `oxidom status [PROFILE] [--json]`, `oxidom ip [PROFILE] [--egress] [--fresh]`,
  `oxidom env [PROFILE]`, `oxidom list [servers|profiles|subscriptions|sessions] [--json]`, and
  `oxidom ping <HANDLE>` are read commands and never spawn a session daemon.
- `oxidom tun [PROFILE] [--down]` inspects the session interface or explicitly removes it.
- `oxidom alias <HANDLE> <NEW>` changes a server alias.
- `oxidom profile {list,show,new,edit,rm}` manages daemon-owned profiles.
- `oxidom daemon [--system --socks-port --http-port]` runs the D-Bus service.
- `oxidom <PROFILE> run -- <cmd>...` is reserved for per-process marking. In phase 4b it still
  refuses safely: a proxy-only profile points to `oxidom env`; an interface profile reports its
  table and fwmark and says process marking arrives in the next step.

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
new writes are refused. `UpProfile` resolves `select.server` as a handle and applies that
profile's ports; unit-pinned ports constrain only `default`. Removing a profile deliberately
leaves its running session intact so the unit can still stop what it started.

Every profile gets a stable `127.<a>.<b>.1` inbound address derived with the same FNV-1a 64 used
for ids; collisions probe forward through the address space. `default` is permanently
`127.0.0.1`, an external contract with consumers such as redsocks. Different addresses let every
profile reuse the same SOCKS/HTTP ports.

### Sessions (binding)

Subscriptions and servers are global sources. A profile is persistent configuration; a session
is the ephemeral running instance of one profile: its Xray process, selected server, loopback
address, ports, optional interface and status. “Connect one server” means the `default` session,
not a separate global mode. Several profiles may run at once, including several profiles on the
same server.

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

- Device names default to `oxi-<profile>` and fit Linux's 15-byte IFNAMSIZ payload; an explicit
  valid `device` is required for longer profile names.
- Device addresses are stable `198.18.<c>.<d>/32` values from the RFC 2544 benchmark block.
  `default` is fixed at `198.18.0.1`; `/32` is binding because it adds no connected route.
- fwmark, private table id and rule priority are the same stable value. `default` is `0x6f00`;
  other profiles probe within `0x6f01..=0x6fff`, avoiding the user's `0x1`/`0x2`/`0x3` policy.
- Every enabled interface gets `default dev <device>` in its private table and a matching fwmark
  rule. `routes = "manual"` changes no system route; `list` adds only its CIDRs; `default` adds a
  host route to the server via the old gateway plus two half-defaults through the device.
- Bring-up order is persistent TUN, address `/32`, tun2socks spawn, link-up, private route/rule,
  then system routes. The spawn-before-link order and double-dash tun2socks flags are live-tested
  contracts.
- Ordinary `down` stops tun2socks and removes oxidom routes/rule but leaves the persistent device,
  preserving hand-written routes across reconnects. `tun --down` and crash recovery additionally
  delete the device only when oxidom created it.

## GUI (Phase 2 — codex brief)
Build with `adw::Application` (app id `dev.keepinfov.oxidom`). Wire to the core modules; do not
reimplement parsing/Xray logic in the UI layer.

Layout (from the mockups + Nautilus feel; dark, rounded, generous spacing):
- **`adw::NavigationSplitView`** (or `OverlaySplitView`) for the adaptive sidebar.
  - **Sidebar:** app/logo area at top; a nav list — at least "General" (server browser) plus
    entries for Subscriptions, Settings, Logs; a bottom action row (e.g. connection status /
    quick connect). Collapses in narrow mode with a small toggle button in the header.
  - **Content:** `adw::HeaderBar` with standard window controls; a **search entry** spanning the
    top that filters servers by name/protocol/country.
- **Server card** (custom widget): country **flag**, server **name**, protocol **subtitle**
  (`transport_label`, e.g. "vless + xhttp + reality"), optional **latency badge** (green when
  low). Whole card is a click target → selects that server. Every server carried by a connected
  profile is visually marked; the one-profile case is unchanged.
- **Server grid:** a top group of "loose"/favorite servers, then per-**subscription groups**:
  each group shows its **title** + **description** (name + quota/expiry from userinfo) followed by
  that subscription's server cards. Multi-column grid in wide mode, single column in narrow
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
      engine.rs                  # Registry + per-profile Session/Sessions facade
      proc.rs                    # shared child supervision and recovered PID inspection
      resolve.rs                 # shared config → env → PATH binary resolver
      link.rs                    # share-link parsers
      subscription.rs            # fetch + decode + userinfo headers + hwid
      xray/
        config.rs                # OutboundSpec -> Xray JSON
        core.rs                  # process supervisor + status
        resolve.rs               # Xray binary preflight
      probe.rs                   # latency probes
      netns.rs                   # phase-B2-safe `oxidom run` refusal
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
- `oxidom run -- <cmd>` routes that process (or, until B3 lands, exits non-zero with the
  profile's actionable interface/table/mark state — current state).
```
