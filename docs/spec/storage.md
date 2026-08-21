# Files (config & state)

Where oxidom keeps its configuration and state on disk, which daemon owns that store, and the schema of `config.toml`.

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
- `~/.local/share/oxidom/oxidom-gui.log` — the graphical client's own log, `0600`, rotated at 2MB
  with one `.log.1` kept. Written only by the GUI, which detaches and sends stderr to `/dev/null`
  and so has no journal; the daemon writes no file because its stderr already reaches one.
- `~/.local/share/oxidom/assets/{geoip.dat,geosite.dat}` — the geo data the core needs, when
  oxidom installed it. `0600` in a `0700` directory like every other file here: the core runs as
  the same user as the daemon that spawns it, so nothing wider is required. Written only when the
  core cannot already find the lists for itself, and pointed at with `XRAY_LOCATION_ASSET` only
  when **both** are present — a directory holding one would hide whichever the core would
  otherwise have found. Which daemon owns the store therefore decides which one the download
  helps: a system daemon writes `/var/lib/oxidom/assets` and cannot read a user's home at all.
- `~/.local/share/oxidom/geo-check.json` — transient. The configuration handed to
  `xray run -test` when asking whether the core can load its lists; removed as soon as the core
  answers.
- `~/.cache/oxidom/egress.json` — user-owned 60-second cache for `oxidom ip --egress`, keyed by
  profile and server id.

## Which daemon owns the store (binding)

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

## Reading the log over D-Bus (binding)

`LogsSince(after_seq: u64, limit: u32) -> String` returns a JSON `LogSlice`: the records following
the caller's cursor, plus `book_id`, `first_seq`, `next_seq` and `skipped`. It answers for **every**
session, not just `default`.

`RecentLogs` and `ClearLogs` keep their names and shapes, so a client older than this daemon still
works; `RecentLogs` is served from the same book and keeps the `oxidom: ` prefix those clients
parse. Against a daemon *older* than the client, `logs_since` falls back to `RecentLogs` and marks
the reconstructed slice with `book_id == 0` (`LEGACY_BOOK_ID`), which a real book never takes — the
signal that sequence numbers are synthetic, the whole log arrives every call, and the reader must
replace rather than append.

A reader that sees `book_id` change resets its cursor: the daemon restarted and counts from zero
again, and a cursor left above every number the new book will issue makes the view go silent.

## `config.toml` schema (serde)

```toml
socks_port = 10808            # local SOCKS inbound
http_port  = 10809            # local HTTP inbound
system_proxy = false          # toggle GNOME/env system proxy on connect
reconnect = false             # reconnect after an unexpected core exit; explicit opt-in
on_core_exit = "hold"         # hold | release — a core that exits by itself keeps its routes,
                              # so traffic is dropped rather than released; a profile overrides
latency_method = "http_get"   # one of: icmp | tcp | http_head | http_get
latency_test_url = "https://www.gstatic.com/generate_204"
subscription_user_agent = "v2rayN/6.45"    # panels gate the body *and its format* on this
geoip_url = ""                # empty: the built-in source. https only; digest is <url>.sha256sum
geosite_url = ""              # empty: the built-in source. Chosen separately from geoip_url
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
pool_probe_url = ""           # where a pool's balancer sends its health check; empty: built-in
```

A profile file carries the same `[core]` table plus `description`, `[select]`, `[proxy]`,
`[interface]`, and `routing` — a string holding an Xray `routing` object, normally written as a
TOML multi-line literal. It is spliced ahead of the generated rules; what it may not contain is in
[A profile's own routing block](xray-config.md#a-profiles-own-routing-block-binding). A profile
that carries none gains no key.

Every `[core]` key is optional at both levels, and an untouched section is not written to the file
at all. See [Advanced core settings](xray-config.md#advanced-core-settings-binding) for what each
one generates.
