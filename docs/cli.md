# CLI reference

The `oxidom` binary is the headless half of the project: it is the daemon, and it
is the client that drives the daemon. The GUI is a separate binary (`oxidom-gui`)
and a peer of this one — anything the GUI does, the CLI can do too.

```
oxidom <COMMAND> [ARGS…]
```

There are **no global flags** beyond `-h`/`--help` and `-V`/`--version`. `--json`
is a per-command flag, not a global one, and exists only on `status`, `list`, and
`profile list`.

## Contents

- [Two ways to name a profile](#two-ways-to-name-a-profile)
- [Handles](#handles)
- [Connecting](#connecting) — `up`, `down`, `connect`
- [Inspecting](#inspecting) — `status`, `ip`, `env`, `list`, `tun`
- [Servers](#servers) — `ping`, `alias`
- [Profiles](#profiles) — `profile list|show|new|edit|rm`
- [Running things](#running-things) — `run`
- [Processes](#processes) — `daemon`, `gui`
- [Exit codes](#exit-codes)
- [Output contract](#output-contract)
- [JSON schemas](#json-schemas)

## Two ways to name a profile

Verb-first is canonical — it is what `oxidom@.service` uses:

```sh
oxidom up work
oxidom status work
```

Profile-first is an accepted synonym, for shell history's sake:

```sh
oxidom work up
oxidom work status
```

The rewrite applies only when the first argument is not itself a command and the
second is one of `up`, `down`, `status`, `ip`, `run`, `env`, `tun`. Because a
profile name could otherwise be mistaken for a command, **command names and
aliases are reserved and cannot be used as profile names**:

```
up  connect-profile  down  disconnect  status  ip  list  ping
alias  profile  connect  daemon  gui  run  env  tun
```

## Handles

Commands that take a `HANDLE` (`connect`, `ping`, `alias`) resolve it in this
order:

1. an exact alias
2. an exact server id
3. a unique case-insensitive substring of an alias or name

If a substring matches more than one server, the command fails with exit code 1
and prints the candidates **to stderr**, so a script reading stdout sees nothing
ambiguous.

An alias is lowercase ASCII letters, digits and hyphens, at most 32 characters,
and may not be exactly 16 hex characters (that shape is reserved for server ids).

## Connecting

### `oxidom up [PROFILE]`

Connect using a named profile. Alias: `connect-profile`.

| Argument | Type | Default |
|---|---|---|
| `PROFILE` | positional | `default` |

This is one of only two commands that will **start a daemon** if none is running.
Warnings (a pool whose members collapse onto fewer real endpoints, a
`routes = "default"` profile that leaves DNS outside the tunnel, ports pinned by
the unit) are printed to stderr; the command still succeeds.

```sh
oxidom up            # the `default` profile
oxidom up work
```

### `oxidom down [PROFILE]`

Disconnect. Alias: `disconnect`.

| Argument | Type | Default |
|---|---|---|
| `PROFILE` | positional, optional | *all sessions* |

Omitting `PROFILE` stops every session regardless of who owns it. The command is
idempotent: stopping a profile that is not up prints a note to stderr and still
exits 0, so `ExecStop=` never fails a unit.

### `oxidom connect <HANDLE>`

Connect to one server without a profile. This is not a separate mode — it is the
`default` session with that server selected.

```sh
oxidom connect ch-trojan
oxidom connect "Zurich"     # unique substring
```

May start a daemon.

## Inspecting

None of these will start a daemon; if none is running they exit 4.

### `oxidom status [PROFILE] [--json]`

With no `PROFILE`, lists every session — identical to `oxidom list sessions`.
With one, prints that session.

```
connected  ch-trojan  Zurich 01  socks 127.0.0.1:10808  84 ms
```

A pool session has no single active server, and says so rather than naming its
first member:

```
connected  socks 127.72.14.1:10808  84 ms
selection: pool "Europe" (leastPing, 6 nodes on 4 exits, now → nl-two)
  ✓ ch-one   Zurich 01
  ✗ de-two   Frankfurt 02
  ? nl-three Amsterdam 01
```

`✓` in rotation, `✗` out of rotation, `?` not yet known.

### `oxidom ip [PROFILE] [--egress] [--fresh]`

| Flag | Meaning |
|---|---|
| *(none)* | the server endpoint address, one per line (a pool prints every member) |
| `--egress` | the public address observed **through** the tunnel |
| `--fresh` | ignore the 60-second egress cache; requires `--egress` |

`PROFILE` defaults to `default`. Pool sessions are never cached, because the exit
rotates.

```sh
oxidom ip                    # where we are pointed
oxidom ip --egress           # where the world sees us
oxidom ip work --egress --fresh
```

### `oxidom env [PROFILE]`

Print shell exports for one session's local proxies. `PROFILE` defaults to
`default`.

```sh
eval "$(oxidom env work)"
curl https://ifconfig.me
```

Emits exactly eight `export` lines — `ALL_PROXY`/`all_proxy` (`socks5h://…`),
`HTTP_PROXY`/`http_proxy`, `HTTPS_PROXY`/`https_proxy` (all three `http://…` on
the HTTP port), and `NO_PROXY`/`no_proxy` set to `localhost,127.0.0.0/8,::1`.

This is the right tool for programs that honour proxy variables. For programs
that do not, see [`oxidom run`](#running-things).

### `oxidom list [TARGET] [--json]`

`TARGET` is one of `servers` (default), `profiles`, `subscriptions`, `sessions`.
Text output is tab-separated, except `sessions`, which is a padded table with
columns `PROFILE STATE SERVER ADDRESS [DEVICE] LATENCY`. `DEVICE` appears only
when at least one session has an interface; missing values render as `—`.

### `oxidom tun [PROFILE] [--down]`

Inspect or remove a session's persistent TUN device. `PROFILE` defaults to
`default`.

```
oxi-work	198.18.9.7/32	mtu 1500	routes manual	table 28449	mark 0x6f01	up
```

`--down` stops interface routing and deletes the device — but **only if oxidom
created it**; a device someone else made is left alone. See
[routing.md](routing.md).

## Servers

### `oxidom ping <HANDLE>`

Measure one server. On success prints **only the integer milliseconds**, so it
drops straight into a script:

```sh
if ms=$(oxidom ping ch-trojan); then echo "alive: ${ms}ms"; fi
```

Polls the daemon for up to 30 seconds. Failures are distinguished, because "the
server is down" and "this laptop has no network" deserve different reactions:
`server is unreachable`, `probe timed out`, `no network connection`, `probe could
not run on this machine`.

### `oxidom alias <HANDLE> <NEW>`

Give a server a stable human handle. Aliases survive subscription refreshes and
are what you should use in profiles and scripts — a server id is stable but
unreadable, and a name can change upstream.

```sh
oxidom alias 3f2a91c4b7e05d16 ch-trojan
```

## Profiles

All require a running daemon. See [profiles-and-pools.md](profiles-and-pools.md)
for the file format.

| Command | Effect |
|---|---|
| `oxidom profile list [--json]` | list profiles |
| `oxidom profile show <NAME>` | print one profile as TOML |
| `oxidom profile new <NAME>` | create an empty profile using the current proxy ports |
| `oxidom profile edit <NAME>` | edit with `$EDITOR`, else `$VISUAL`, else `vi` |
| `oxidom profile rm <NAME>` | remove one profile |

`profile edit` writes a `0600` temporary file, runs the editor, no-ops if nothing
changed, and **validates before saving** — a profile that would fail to come up is
rejected at the editor, not at connect time. The temp file is deleted even if the
editor fails.

Removing a profile deliberately leaves a running session of it alone, so the unit
that started it can still stop it.

## Running things

### `oxidom run [--profile NAME] [-c COMMAND] [-- ARGS…]`

Run one process inside a profile's routing domain — for programs that ignore
proxy environment variables.

| Flag | Type | Default |
|---|---|---|
| `--profile <NAME>` | string | `default` |
| `-c <COMMAND>` | string, split without a shell | — |
| `ARGS…` | trailing, after `--` | required unless `-c` |

```sh
oxidom run -- curl https://ifconfig.me
oxidom run --profile work -- ping 1.1.1.1
oxidom work run -- firefox
oxidom run -c "curl -s https://ifconfig.me"   # split, but no shell is started
```

Requirements — all three, or the command refuses rather than leaking traffic onto
the ordinary route:

- the profile has an interface (`[interface] enable = true`) and it is up
- a `systemd --user` manager is available
- `systemd-run` is on `PATH`

**The exit code is the child's**, so this composes: a failing command inside the
tunnel fails the script outside it. A signal becomes `128 + signum`.

## Processes

### `oxidom daemon [--system] [--socks-port N] [--http-port N]`

Run the headless daemon that owns the tunnel and serves D-Bus.

| Flag | Meaning |
|---|---|
| `--system` | serve on the system bus instead of the session bus |
| `--socks-port <N>` | override `socks_port` from `config.toml` |
| `--http-port <N>` | override `http_port` from `config.toml` |

Passing a port on the command line **pins** it: clients can no longer change it,
and `up` reports `… port is pinned by the unit, profile value ignored`. This is
deliberate — with the daemon on the system bus, an unpinned port is a setting any
client could rewrite out from under everyone else using it.

Default log level is `info` for `daemon` and `warn` for every other subcommand;
`$RUST_LOG` overrides either.

### `oxidom gui [--background] [--debug]`

A compatibility shim that execs `oxidom-gui`. Prefer calling `oxidom-gui`
directly. Both accept the same flags:

| Flag | Meaning |
|---|---|
| `--background` | start without showing the window (for autostart); activating the app again presents it |
| `--debug` | stay in the foreground and log at debug level |

Started from a terminal, `oxidom-gui` forks into the background so closing that
terminal does not take the window and tray with it. Nothing is detached when
stdout is not a terminal, so `oxidom-gui | tee` and the tray unit behave normally.

## Exit codes

| Code | Meaning |
|---:|---|
| `0` | success |
| `1` | command error |
| `3` | not connected — the requested connection-dependent value does not exist |
| `4` | no daemon reachable |
| *child's* | `oxidom run` only: the command's own code, or `128 + signal` |

Exit 3 prints **no message**. It is the "nothing to report" answer, not a failure:

```sh
if ip=$(oxidom ip --egress); then
  echo "$ip"
elif [ $? -eq 3 ]; then
  echo "not connected"
fi
```

## Output contract

- **stdout** carries data, and only data.
- **stderr** carries diagnostics, warnings, and ambiguous-handle candidates.

Read commands never start a daemon. Only `up` and `connect` may.

## JSON schemas

`--json` prints a single line of compact JSON. These shapes are frozen by tests —
they are safe to parse. Subscription URLs, credentials, and custom user-agents are
deliberately excluded from every one of them.

**`status <PROFILE> --json`** — one object:

```json
{"state":"connected","server":{"id":"…","alias":"ch-trojan","name":"Zurich 01",
"address":"203.0.113.7","port":443,"protocol":"trojan"},"socks_port":10808,
"http_port":10809,"latency_ms":84,"error":null,"address":"127.0.0.1"}
```

`server` is `null` for a pool session, and a `selection` object appears instead.

**`status --json`, `list sessions --json`** — an array of session objects, adding
`profile`, `owns_system_proxy`, and `interface` (`null` when the profile has none):

```json
[{"profile":"work","state":"connected","server_id":"…","server_alias":"ch-trojan",
"server_name":"Zurich 01","address":"127.72.14.1","socks_port":10808,
"http_port":10809,"latency_ms":84,"error":null,"owns_system_proxy":true,
"interface":{"device":"oxi-work","address":"198.18.9.7","mtu":1500,
"routes":"manual","table":28449,"mark":28449,"up":true}}]
```

**`list servers --json`** — `id`, `alias`, `name`, `protocol`, `address`, `port`,
`country`, `subscription_id`, `subscription`.

**`list profiles --json`, `profile list --json`** — `name`, `description`,
`server`, `socks_port`, `http_port`.

**`list subscriptions --json`** — `id`, `name`, `description`, `send_hwid`,
`server_count`, `updated_at`, and `userinfo` (`{upload, download, total, expire}`
or `null`).

---

Next: [configuration.md](configuration.md) · [profiles-and-pools.md](profiles-and-pools.md) · [troubleshooting.md](troubleshooting.md)
