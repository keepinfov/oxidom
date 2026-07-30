# Configuration

oxidom has one settings file, `config.toml`. Connection-specific settings live in
[profiles](profiles-and-pools.md) instead, so that several tunnels can differ
without fighting over one file.

## Contents

- [Where the files are](#where-the-files-are)
- [The two databases](#the-two-databases) — the single most common surprise
- [`config.toml`](#configtoml)
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
subscription_user_agent = "v2rayNG/1.9.5"
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
| `subscription_user_agent` | string | `"v2rayNG/1.9.5"` | Sent when fetching subscriptions. Many panels serve a different body — or a web page — depending on it. |
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
`v2rayNG/1.9.5`, `Happ/3.13.0`, `v2rayN/6.45`, `Streisand`, `Hiddify/2.0.5`,
`NekoBox/1.3.5`, `Shadowrocket/2.2.9`, `clash-verge/1.7.7`, `SFA/1.10.0`.

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
