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
- The **GUI runs unprivileged** (no root). Only the per-app netns helper is privileged.
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
- `nix develop` gives a shell with gtk4/libadwaita/glib + rust toolchain + `xray`.
- `nix develop -c cargo build` / `cargo run`. `nix build` produces the wrapped binary.

## Files (config & state)
Resolve config dir as `$XDG_CONFIG_HOME/oxidom` (`~/.config/oxidom`), data dir as
`$XDG_DATA_HOME/oxidom` (`~/.local/share/oxidom`). Create parent dirs on write; never panic on
missing files (treat as defaults/empty).

- `~/.config/oxidom/config.toml` — user settings (see schema).
- `~/.local/share/oxidom/subscriptions.json` — cached subscriptions + parsed servers.
- `~/.local/share/oxidom/state.toml` — last active server, per-app route memory.
- `~/.local/share/oxidom/hwid` — random per-install id (only generated/used if a sub opts in).

### `config.toml` schema (serde)
```toml
socks_port = 10808            # local SOCKS inbound
http_port  = 10809            # local HTTP inbound
system_proxy = false          # toggle GNOME/env system proxy on connect
latency_method = "http_get"   # one of: icmp | tcp | http_head | http_get
latency_test_url = "https://www.gstatic.com/generate_204"
subscription_user_agent = "v2rayNG/1.9.5"  # panels gate the body on this
xray_binary = ""              # empty: use $OXIDOM_XRAY_BIN, then xray on PATH
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
  `127.0.0.1`, `sniffing` enabled (`http`, `tls`).
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
- Stop cleanly on disconnect/app-exit (SIGTERM, then SIGKILL after timeout).
- Only one core process at a time (single active server).

## Latency probes (`latency_method`)
- `icmp` — spawn `ping -c1 -W1 <host>` and parse (avoids raw-socket privileges).
- `tcp` — time a raw TCP connect to `host:port`.
- `http_head` — HEAD `latency_test_url` **through the active SOCKS inbound**.
- `http_get` — GET `latency_test_url` through SOCKS (Happ-style; expect 204).
List view may use a cheap method across servers concurrently (bounded thread pool); the active
connection uses the configured method.

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
- **Every id that enters `ProbeQueue::running` leaves with a `readings` entry**, including ids that
  no longer resolve. The GUI retires its spinner on the id leaving `running ∪ queued`, so a silent
  early return leaves a card checking forever.
- **`queued ≠ finished`.** `ProbeState` reports `running` and `queued` separately; a card waiting
  for a slot still carries its *previous* number and must not present it as this measurement's.
- **`ProbeState.version`** is bumped whenever the shape changes. A GUI seeing a lower version
  reports everything as unmeasured and says why, rather than guessing.
- Readings are dropped for servers that no longer exist (`Shared::prune_readings`, called by every
  mutating `Service` method) — ids are reissued on subscription refresh.

Freshness is the GUI's job: `gui::reduce::latency_state` is the **single** mapper from a reading to
a `LatencyState`, and ages are bucketed to whole minutes so the badge repaints on a bucket change
rather than once a second.

## CLI (clap derive)
Single binary `oxidom`:
- `oxidom` / `oxidom gui` → launch GUI (default).
- `oxidom run -- <cmd>...` → run one process routed through the active proxy via a **network
  namespace**. Mechanism (Phase 1): create/enter a netns with a veth or `slirp`-style userspace
  path to the local SOCKS inbound, or (simpler first cut) a netns + `redsocks`/`tun2socks`
  bridged to SOCKS; then `exec` the target. Needs a small privileged helper — design the helper
  boundary so the GUI never needs root. Document the chosen privilege model in `.notes/`.
- (Phase 3) `oxidom connect <id> | status | disconnect` → control a background core for scripting.

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
  low). Whole card is a click target → selects that server. The active server is visually marked.
- **Server grid:** a top group of "loose"/favorite servers, then per-**subscription groups**:
  each group shows its **title** + **description** (name + quota/expiry from userinfo) followed by
  that subscription's server cards. Multi-column grid in wide mode, single column in narrow
  (use `adw::WrapBox`/`FlowBox` with a breakpoint via `adw::Breakpoint`).
- **Connect control:** a single primary **Connect/Disconnect** toggle in the header (single active
  server). Show live status (Connecting/Connected/Error) and active latency.
- **Subscriptions view:** add (URL + optional name), update-now, delete; per-sub **"send HWID"**
  switch (default OFF) with a privacy hint.
- **Settings view:** ports, system-proxy toggle, latency method + test URL.
- **Logs view:** the core's ring-buffer output.

Responsiveness: define a breakpoint (~700px) that collapses the split view and switches the grid
to a single column, exposing the small sidebar toggle button (as annotated in the narrow mockup).

## Module layout
```
src/
  main.rs         # entry + dispatch
  cli.rs          # clap defs + `run` subcommand
  config.rs       # config.toml load/save
  state.rs        # state.toml (active server, per-app memory)
  model.rs        # Server/Subscription/OutboundSpec types
  link.rs         # share-link parsers (vless/vmess/trojan/ss/socks/http/hysteria2)
  subscription.rs # fetch + decode + userinfo headers + hwid
  xray/
    config.rs     # OutboundSpec -> Xray JSON
    core.rs       # process supervisor + status
  probe.rs        # latency probes
  netns.rs        # `oxidom run` per-process routing
  gui/            # Phase 2 (codex): app, window, sidebar, cards, groups, views
```

## Definition of done
- `cargo build` clean; `Cargo.lock` present; `nix build` works.
- `oxidom` opens the adaptive window; sidebar collapses under the breakpoint.
- Adding a base64 subscription lists its servers grouped with title/description; userinfo quota
  shows when present.
- Selecting a server + Connect starts Xray, exposes the local SOCKS/HTTP proxy, and shows
  Connected + a real latency; Disconnect stops it cleanly.
- HWID is never sent unless the per-sub switch is on.
- `oxidom run -- <cmd>` routes that process (or, until the helper lands, exits non-zero with a
  clear "not yet implemented" message — current state).
```
