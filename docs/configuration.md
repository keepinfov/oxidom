# Configuration

oxidom has one settings file, `config.toml`. Connection-specific settings live in
[profiles](profiles-and-pools.md) instead, so that several tunnels can differ
without fighting over one file.

## Contents

- [Where the files are](#where-the-files-are)
- [The two databases](#the-two-databases) — the single most common surprise
- [`config.toml`](#configtoml)
- [Advanced core settings](#advanced-core-settings) — fragmentation, mux, sniffing, DNS
- [Finding helper binaries](#finding-helper-binaries)
- [Environment variables](#environment-variables)
- [File handling](#file-handling)

## Where the files are

Under an ordinary user daemon:

| Path | Contents |
|---|---|
| `~/.config/oxidom/config.toml` | settings, below |
| `~/.config/oxidom/profiles/<name>.toml` | one file per profile |
| `~/.config/oxidom/gui_prefs.toml` | GUI-only display state — collapsed groups, ordering, saved groups |
| `~/.local/share/oxidom/subscriptions.json` | cached subscriptions and their parsed servers |
| `~/.local/share/oxidom/state.toml` | live session records, for crash recovery |
| `~/.local/share/oxidom/hwid` | per-install id, **only created if a subscription opts in** |
| `~/.local/share/oxidom/current-config-<profile>.json` | the generated Xray config in use |
| `~/.cache/oxidom/egress.json` | 60-second cache for `oxidom ip --egress` |

`$XDG_CONFIG_HOME`, `$XDG_DATA_HOME` and `$XDG_CACHE_HOME` are honoured.

## The two databases

When oxidom runs as a **system daemon**, systemd sets `StateDirectory=oxidom`, and
oxidom then keeps *both* its config and its data in `/var/lib/oxidom` — flat, not
in XDG subdirectories:

```
/var/lib/oxidom/config.toml
/var/lib/oxidom/profiles/work.toml
/var/lib/oxidom/subscriptions.json
/var/lib/oxidom/state.toml
```

The cache directory deliberately does **not** move, because `oxidom ip --egress`
runs as the calling user.

**So the choice of daemon is the choice of database.** A session daemon reads
`~/.config/oxidom`; the system daemon reads `/var/lib/oxidom`. They do not share
servers, subscriptions or profiles.

This is why the client waits for an installed system daemon rather than racing it.
If a GUI at login won the race against the systemd unit, it would quietly bind to
a *different* database — and the only visible symptom would be that all your
servers had vanished. If you ever see that, you are talking to the other daemon;
see [troubleshooting.md](troubleshooting.md#my-servers-vanished).

## `config.toml`

Every key is optional; unknown keys are ignored. A file that fails to parse is
moved aside (see [File handling](#file-handling)) and defaults are used.

```toml
socks_port = 10808
http_port  = 10809
system_proxy = false
reconnect = false
latency_method = "http_get"
latency_test_url = "https://www.gstatic.com/generate_204"
subscription_user_agent = "v2rayN/6.45"
xray_binary = ""
tun2socks_binary = ""
nft_binary = ""
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `socks_port` | `u16` | `10808` | Local SOCKS5 inbound port. Must be non-zero and differ from `http_port`. |
| `http_port` | `u16` | `10809` | Local HTTP inbound port. |
| `system_proxy` | bool | `false` | Whether a connected session takes over the GNOME system proxy. See [routing.md](routing.md#gnome-system-proxy). |
| `reconnect` | bool | `false` | Redial when the core exits **unexpectedly** — never after you asked for `down`. Opt-in on purpose: a tunnel that silently comes back is a tunnel you cannot turn off. |
| `latency_method` | enum | `"http_get"` | One of `icmp`, `tcp`, `http_head`, `http_get`. |
| `latency_test_url` | string | `https://www.gstatic.com/generate_204` | Target for the HTTP probe methods. |
| `subscription_user_agent` | string | `"v2rayN/6.45"` | Sent when fetching subscriptions. Many panels serve a different body — or a different *format*, or a web page — depending on it. See [the format it selects](subscriptions-and-protocols.md#the-user-agent-decides-the-format). |
| `xray_binary` | path | `""` | Empty falls through to the env var, then `PATH`. |
| `tun2socks_binary` | path | `""` | Same. Needed only for TUN mode. |
| `nft_binary` | path | `""` | Same. Needed only for `oxidom run`. |

Ports must be between 1 and 65535, and the SOCKS and HTTP inbounds cannot share
one.

### Settings the daemon will refuse to change

- **Pinned ports.** If the unit passes `--socks-port`/`--http-port`, clients cannot
  change them, and the GUI locks those rows and says why.
- **The three `*_binary` paths, over the system bus.** Setting a binary path on a
  privileged daemon is a remote-execution primitive, so a system daemon ignores
  those keys from clients entirely. Edit the file, or use `systemctl edit`.

### User-Agent presets

The GUI offers these, and the free-text field remains the source of truth:
`v2rayN/6.45`, `v2rayNG/1.9.5`, `Happ/3.13.0`, `Streisand`, `Hiddify/2.0.5`,
`NekoBox/1.3.5`, `Shadowrocket/2.2.9`, `clash-verge/1.7.7`, `SFA/1.10.0`.

## Advanced core settings

`[core]` controls what the generated Xray config says about logging, sniffing,
DNS, multiplexing and TLS fragmentation. Everything in it is optional and off by
default: **with an empty `[core]` the generated config is exactly what oxidom
produced before these settings existed.**

Two levels set them, and they merge field by field:

1. `[core]` in `config.toml` — the machine.
2. `[core]` in a [profile](profiles-and-pools.md) — one tunnel; wins where it
   says anything at all.

To see what a session would actually be built with, and which of the two levels
decided each value:

```console
$ oxidom core show work
log_level               warning        built-in
domain_strategy         IPIfNonMatch   built-in
sniffing.enabled        true           built-in
sniffing.dest_override  http,tls       built-in
sniffing.route_only     false          built-in
mux.enabled             true           global
mux.concurrency         16             profile
fragment.enabled        false          built-in
```

A row appears exactly when the key reaches the generated config, so the table
reads as the config rather than as a list of everything settable. `--json` prints
the same thing as a stable schema.

### `[core]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `log_level` | enum | `"warning"` | `debug`, `info`, `warning`, `error`, `none`. `debug` is loud enough to matter — see the note below. |
| `domain_strategy` | enum | `"ip_if_non_match"` | `as_is`, `ip_if_non_match`, `ip_on_demand`. How routing resolves domains before matching. |

### `[core.sniffing]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `true` | Read the destination host out of the connection. Turning it off means domain-based routing stops seeing domains. |
| `dest_override` | list | `["http", "tls"]` | Any of `http`, `tls`, `quic`. |
| `route_only` | bool | `false` | Use the sniffed name to pick a route, but hand the original address to the outbound. |

### `[core.mux]`

Multiplexes several streams over one connection.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `false` | |
| `concurrency` | int | unset | `-1` disables, otherwise 1–1024. |
| `xudp_concurrency` | int | unset | Same range. |
| `xudp_proxy_udp_443` | enum | unset | `reject`, `allow`, `skip`. |

In Xray, mux is a property of an *outbound*, so a [pool](profiles-and-pools.md#pools)
applies it to every member. Note that holding one connection open works against
spreading activity across exit addresses, which is the reason pools exist — think
before combining the two.

### `[core.fragment]` and `noises`

Splits the TLS hello across packets, and optionally sends decoy traffic. This is
the setting people come to advanced options for.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `false` | |
| `packets` | string | `"tlshello"` | `tlshello`, a count, or a range like `"1-3"`. |
| `length` | string | `"100-200"` | Bytes per fragment: a number or a range. Unlike `packets`, this one does not accept `tlshello`. |
| `interval` | string | `"10-20"` | Milliseconds between fragments. |

```toml
[core.fragment]
enabled = true
packets = "tlshello"
length = "100-200"
interval = "10-20"

[[core.noises]]
kind = "rand"      # rand | str | base64
packet = "10-20"
delay = "10-16"
```

Both are carried by one extra `freedom` outbound, tagged `dialer`, that the proxy
outbounds dial through. It exists only when one of the two is configured.

### `[core.dns]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `server` | string | unset | Resolver for what the tunnel carries. A plain address or a DoH URL. Nothing else in this section takes effect without it. |
| `direct_server` | string | unset | Consulted first, and only for names the local network answers. |
| `query_strategy` | enum | `"use_ip"` | `use_ip`, `use_ipv4`, `use_ipv6` — the "IPv6 mode" other clients expose. |

```toml
[core.dns]
server = "https://1.1.1.1/dns-query"
direct_server = "localhost"
query_strategy = "use_ipv4"
```

`direct_server` is scoped to private names, so today it covers your LAN and
nothing more: oxidom currently routes everything except private addresses through
the proxy, which leaves no other class of "direct" name to point it at.

### In the GUI

Settings → **Core behaviour** edits the machine's `[core]`; the profile editor
carries the same rows under the same heading.

The two differ in what "unset" means, so they behave differently on purpose:

- In Settings, a row set to the built-in value is stored as *nothing at all*.
  Turning multiplexing on adds `[core.mux] enabled = true` and not one key more;
  turning it off again removes the table. The file keeps naming only what you
  chose.
- In the profile editor, each section is a switch: off inherits the machine's
  value, and the row underneath says what that value is, so a section can be
  read without being switched on. Turning it on hands that whole section to the
  profile — from then on it stops following `config.toml`, which is exactly what
  a `[core.mux]` table in a profile file means.

`noises` has no rows. A list of hand-tuned byte patterns has no sensible default
to offer, so the GUI reports how many there are and writes back what it was
given; edit them in the file.

One thing the GUI cannot express, because the file cannot either: a profile can
point `dns.server` somewhere else, but it cannot *remove* a resolver set for the
machine. An unset field means "inherit", and there is no third state.

### Two things worth knowing

**A setting the core accepts is not a setting the core honours.** Xray 26.3.27
takes an unknown key, an unknown `loglevel`, and a range written backwards
(`"200-100"`) without a word of complaint — and then does nothing with them.
oxidom therefore rejects those itself when the file is read, rather than letting
a typo look like a working setting. If a value is refused here, that is why.

**The log level moves both ends of the same dial.** At `debug` a busy tunnel
pushes interesting lines out of the bounded log buffer quickly — turn it on to
answer a question, then turn it back off. At `none` the core prints nothing at
all, not even its startup banner, so the Logs view stays empty and a failure
leaves no trace to read afterwards. Connecting still works either way: readiness
is decided by probing the local SOCKS port, not by watching the log.

## Finding helper binaries

Three external programs may be needed. Each resolves in the same order:

**config key → environment variable → `PATH`**

| Program | Needed for | Config key | Environment variable |
|---|---|---|---|
| `xray` | everything — it is the core | `xray_binary` | `OXIDOM_XRAY_BIN` |
| `tun2socks` | TUN mode only | `tun2socks_binary` | `OXIDOM_TUN2SOCKS_BIN` |
| `nft` | `oxidom run` only | `nft_binary` | `OXIDOM_NFT_BIN` |

Resolution happens **before** the core is spawned, so a missing or unusable binary
is reported as an actionable error naming both the path tried and where that path
came from — not as a core that mysteriously fails to start.

Three more are invoked by bare name, with no override: `gsettings` (GNOME proxy),
`ping` (ICMP probes), `systemd-run` (`oxidom run`).

## Environment variables

| Variable | Effect |
|---|---|
| `RUST_LOG` | Log level. Overrides the default (`info` for `daemon`, `warn` otherwise). |
| `OXIDOM_XRAY_BIN` | Xray core path, when `xray_binary` is empty. |
| `OXIDOM_TUN2SOCKS_BIN` | tun2socks path. |
| `OXIDOM_NFT_BIN` | `nft` path. |
| `OXIDOM_BIN` | Which `oxidom` binary the GUI spawns as a session daemon. Set by the Nix wrapper. |
| `OXIDOM_GUI_BIN` | Which `oxidom-gui` binary `oxidom gui` execs. |
| `OXIDOM_EGRESS_URL` | Override `https://api.ipify.org` for `oxidom ip --egress`. |
| `EDITOR`, `VISUAL` | Editor for `oxidom profile edit` (then `vi`). |
| `STATE_DIRECTORY` | Set by systemd. Redirects config **and** data. See [above](#the-two-databases). |
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` | Base directories. |
| `XDG_RUNTIME_DIR` | Where `oxidom run` looks for the systemd user socket. |

## File handling

- Every write is **atomic** — a temp file and a rename, so a crash mid-write cannot
  truncate your subscriptions.
- Files are created `0600` and their parent directories `0700`. Generated Xray
  configs contain credentials and are no exception.
- A file that cannot be parsed is **never overwritten**. It is renamed to
  `<name>.corrupt-<timestamp>` and oxidom continues with defaults, telling you
  where the original went. Nothing you had is destroyed by a parse bug.
- `config.toml` and `state.toml` are **owned by the daemon**. Hand-editing them
  while a daemon is running will be overwritten; stop it first, or go through the
  GUI or `oxidom profile edit`.

---

Next: [profiles-and-pools.md](profiles-and-pools.md) · [routing.md](routing.md) · [cli.md](cli.md)
