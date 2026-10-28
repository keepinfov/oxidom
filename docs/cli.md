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

Daemon discovery bounds bus setup and readiness, not just individual replies.
Commands requiring an existing daemon allow 2 seconds per bus. `up` and
`connect` allow 10 seconds for the system phase, 2 seconds for an existing session
daemon and 6 seconds for a newly started one. A timeout reports an error rather
than silently selecting another database. An early daemon exit includes its exit
status, allowing a short same-session grace for a competing starter. These are
discovery I/O budgets, not total command deadlines; a slow first daemon/core
installation may require retrying later. See [architecture.md](architecture.md#finding-the-daemon).

With an externally supervised session daemon, export
`OXIDOM_SESSION_DAEMON=external` to every GUI/CLI launcher. `up` and `connect`
then report daemon-unavailable (exit 4) rather than start a competing private
process. This does not force the session bus: a reachable system daemon still
wins. Unset, empty or `standalone` keeps default one-shot startup without a daemon
crash-restart loop. Foreground `oxidom daemon` is unchanged.

## Contents

- [Two ways to name a profile](#two-ways-to-name-a-profile)
- [Handles](#handles)
- [Connecting](#connecting) — `up`, `down`, `connect`
- [Inspecting](#inspecting) — `status`, `ip`, `env`, `list`, `tun`
- [Servers](#servers) — `ping`, `alias`
- [Profiles](#profiles) — `profile list|show|new|edit|rm`
- [Core settings](#core-settings) — `core show`
- [Diagnostic export](#diagnostic-export) — `core diagnostics`, `core report`
- [Running things](#running-things) — `run`
- [Processes](#processes) — `daemon`, `gui`
- [Exit codes](#exit-codes)
- [Output contract](#output-contract)
- [JSON schemas](#json-schemas)

## Diagnostic export

```sh
oxidom core diagnostics --limit 64
oxidom core diagnostics --cursor 'TOKEN_FROM_NEXT_CURSOR'
oxidom core report --output ./oxidom-report.txt
# A daemon that failed before D-Bus readiness: explicitly read its local journal.
oxidom core report --file ~/.local/share/oxidom/oxidom-daemon.log --output ./oxidom-report.txt
```

These commands never start a daemon. Without `--file`, they read the selected existing
daemon over D-Bus; an older daemon reports unsupported, not substituted memory logs.
With `--file`, no daemon is contacted and only that private journal plus `.1` and `.lock`
are read. Files are not repaired or chmodded; unsafe files, missing files and contention
are errors. A user's CLI cannot directly open a different uid's private system journal.

`core diagnostics` always prints JSON. Pages contain `records`, `next_cursor`, `gap`,
`rejected`, `incomplete_tail` and `more`, plus schema `version: 1`. Default limit is 64,
capped at 256; 0 selects the default. Read oldest first and pass back `next_cursor`
unchanged. `more` means another page is available. A restart preserves the position;
rotation can eventually evict it. `gap: true` means continuity could not be proved and
reading reset to the oldest retained data, possibly repeating records; lost count is
unknown. Invalid complete lines are counted in `rejected` without copying their text.
An incomplete current tail waits for a later read. This cursor is unrelated to memory
log sequence numbers and runtime IDs.

`core report` accepts the same `--cursor`, `--limit`, `--file` options. It formats **one
bounded page**, not a complete or atomic archive. Without `--output` the report goes to
stdout; with it the CLI atomically writes a `0600` file, creating missing directories
privately. The report includes the next cursor and explains gaps and omissions.
Daemon/core versions, connection settings, raw logs and server identities are not
collected. Shared best-effort redaction is applied; **review the whole report before
sharing**. Nothing is uploaded automatically. Successful reads with explicit gaps or
rejected lines still exit 0; storage/export/unsupported-method errors exit 1, absent
daemon in default mode exits 4. No local-file fallback happens implicitly.

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

When [direct standby](configuration.md#direct-standby) is actually serving, the
unscoped listing explicitly shows `default` as `direct` on `127.0.0.1` at the
configured ports, even if a failed default session remains recorded. That means
ordinary, unprotected traffic, not a connected tunnel. A session holding traffic
prevents standby in every profile; its lifecycle remains visible alongside separate holding facts.

`oxidom status default` also reports this as `direct`, naming both local ports. Any previous
default-session failure remains in the JSON `error` field and is printed as `previous error`
in the single-session text output. Direct standby does not satisfy the connected-tunnel
requirement of `ip`, `env` or `run`.

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

`connecting` includes verification: the process must carry an HTTP request through SOCKS,
and any requested interface must be ready, before status becomes `connected`.

`STATE` always retains lifecycle, including `error` for a failed core. A daemon reporting runtime
facts adds `PHASE HEALTH PROTECTION RETRY`. `PHASE` separates process startup (`starting`),
SOCKS-path confirmation (`verifying`), confirmed operation (`running`), failure (`failed`) and
stopped state (`stopped`). It is independent of legacy error wording and route holding.

`HEALTH` is the latest accepted session observation as outcome/source/time, or `unknown` until
measured. Its UTC timestamp is evidence of when it was observed, not a guarantee of reachability
now. Direct server-list latency and diagnostic history never supply this value.

`PROTECTION` labels the snapshotted exit policy, recorded hold, interface-up flag, route mode and
desktop-proxy ownership. These describe this session's routing scope, not protection of every
application or all traffic. A manual interface or proxy-only session does not cover the entire
machine. See [when the core dies](routing.md#when-the-core-dies). Older daemons without runtime
facts get a separate `HOLDING` column when needed, without replacing `STATE`.
Missing or null protection booleans mean unknown and are printed as `unknown`, not `false`.

`RETRY` shows `idle`, `backoff`, `attempting` or `exhausted`, a zero-based ordinal, and the remaining
monotonic backoff milliseconds when waiting, rounded up (zero once the deadline is reached).
The first pending and first admitted automatic attempt both have ordinal zero. `exhausted`
means saved-profile failover reached its budget or candidate limit; it is independent of phase
and does not fall through to infinite same-server reconnect. Replacement or confirmed recovery
clears the episode. Revocation prevents further starts but may let admitted confirmation finish.
For ordinary same-target reconnect, disabling automatic recovery cancels its pending episode;
enabling it again does not restart an old waiter. An attempt that already started may finish
verification, but will not retry again if
it fails, even if recovery is re-enabled before it finishes. Bring the profile up explicitly to
begin a fresh connection after cancellation.

No dedicated failover command or flag is added. Configure the optional `[failover]` table using
`oxidom profile edit <name>`, then explicitly `oxidom up <name>` to authorize that saved profile.
Temporary `connect` or pool activation, `oxidom run`, and global `reconnect` do not authorize
alternate servers. See [Sequential failover](profiles-and-pools.md#sequential-failover) for an
invented-ID TOML example. This P12 stage is proxy-only; enabled interfaces are refused and safe
TUN failover is still mandatory unfinished work. Dedicated CLI/GUI controls remain P13.

`STATE` reads `direct` for active standby service on the configured local ports.
It has no remote server, tunnel latency or interface, and does not own the system
proxy. An enabled setting alone does not make a `direct` row: the daemon must
actually be serving, with no connected, connecting or holding session anywhere.

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

The daemon does the measuring; `ping` asks for it and then polls for up to 30
seconds. Because a request is only queued, not started, that budget covers the
wait for a free slot as well as the measurement — on a busy queue a probe can
still be running when the command gives up.

Direct HTTP checks and the initial connection confirmation each allow **6 seconds**
for the cold request, including DNS, TCP and TLS/REALITY handshakes. The temporary
core's startup has a separate readiness wait; periodic checks through an established
session retain a **3-second** request budget. Confirmation uses HTTP through SOCKS
even if the configured direct method is ICMP or TCP. A passing host ping or TCP
connect does not prove the proxy outbound carries requests; a failure reports a
recognised core cause (handshake timeout, reset, refusal or DNS failure), otherwise
the measured timeout, closed connection or local failure.

Failures are distinguished, because "the server is down" and "this laptop has no
network" deserve different reactions. Everything below goes to stderr, and every
line naming a server is prefixed with its handle:

| Message | Meaning |
|---|---|
| `the server did not answer` | The server never answered, or answered and refused. |
| `the check ran out of time` | The measurement itself ran out of time. |
| `this machine has no network` | This machine has no usable route. Claimed only on evidence — see [spec/latency.md](spec/latency.md). |
| `the check could not run on this machine` | Something local stopped the check, and the daemon did not say what. |

When the daemon *does* say what stopped it, that reason is printed instead of the
last line — these come from the probe core's own complaint, so they name a
condition rather than shrugging:

| Message | Meaning |
|---|---|
| `no Xray core, so nothing could be measured` | No core binary was found. `oxidom core show` says where it looked. |
| `the server's certificate was rejected` | TLS verification failed. `oxidom trust <HANDLE>` shows the certificate. |
| `the server asks for unverified TLS, which this core removed` | The link wants `allowInsecure`, which Xray 26.x dropped. A pin is the only way through. |
| `the core refused the generated config` | The core would not start on this server's settings. |
| `the core has no geo data (geoip.dat, geosite.dat), so it refused the routing rules` | The core cannot load `geoip.dat`/`geosite.dat`, which every generated config needs. |
| `the check could not run on this machine` | A local fault the core did not name — the same line as in the table above, because it is the same condition. |

Four more are about the exchange rather than the server: `probe for <HANDLE> did
not finish within 30 seconds`, `daemon finished the probe for <HANDLE> without a
reading`, `daemon returned an invalid probe reading` (a reading that is neither a
number nor a failure — a daemon bug), and `daemon probe schema N is older than
required version M`, which means the running daemon predates this binary and
should be restarted.

`oxidom ping` offers no way to call a check off: Ctrl-C stops the waiting, not the
measuring. The daemon does accept `CancelProbes`, which drops what is still queued,
but no command here sends it.

### `oxidom server add [--protocol P] [--file PATH|-]`

Create a server by hand, field by field — the way to enter one whose parameters
did not arrive as a share link. Without `--file`, your editor opens on a
commented TOML template for the chosen protocol (default `vless`); every field
names the JSON key it fills, and the reference is
[spec/data-model.md](spec/data-model.md). With `--file` the draft is read whole
from a file, or from stdin with `-`:

```sh
oxidom server add --protocol trojan          # edit a template
oxidom server add --file server.toml         # scripted
```

The daemon validates the draft through the same path that generates the Xray
outbound and creates nothing when validation fails; the error names the field.
The created server appears under **My servers**, next to pasted links, where a
subscription refresh cannot touch it. A `[outbound_patch]` table is merged into
the generated outbound verbatim, for core options the fields do not model.

### `oxidom trust <HANDLE> [--trust]`

Show the certificate a server presents, and accept it.

Xray 26 removed `allowInsecure`, so a server with a self-signed or otherwise
untrusted certificate cannot be reached at all until its certificate is
**pinned**. Providers still put `allowInsecure=1` in share links, where it now
does nothing.

```console
$ oxidom trust home-relay
relay.example.org:443
SHA-256 55:de:d3:8f:1e:6c:6b:4a:…:ae:a6:c1:0f
Run again with --trust to accept this certificate for this server.

$ oxidom trust home-relay --trust
Pinned. Only this certificate will be accepted for this server.
```

Without `--trust` it only looks. Compare the fingerprint against the server's
own certificate before accepting it — that comparison is the entire security of
this feature:

```sh
openssl x509 -in cert.pem -outform der | openssl dgst -sha256
```

A pin accepts **one** certificate rather than any certificate, which is why
this replaced `allowInsecure` rather than restoring it. Pins survive
subscription refreshes, like aliases.

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

## Core settings

### `oxidom core show [PROFILE] [--json]`

Prints the advanced Xray core settings a session would be built with, one per
line, together with the level that decided each: `built-in`, `global`
(`config.toml`), or `profile`. `PROFILE` defaults to `default`.

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

Fields are tab-separated, as everywhere else in this CLI.

A row exists exactly when the key will reach the generated config, so a disabled
section shows only its switch — listing `mux.concurrency` under a `mux` that is
off would describe a value the core never sees. There is no `core set`: these are
edited in `config.toml` or with `oxidom profile edit`, and fifteen fields would
mean fifteen verbs. See
[configuration.md](configuration.md#advanced-core-settings) for what each one
does.

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

SIGINT/SIGTERM closes request admission, cancels pending work and drains active workers before
session cleanup. A successful stop exits 0; failed cleanup, service setup failure or loss of the
D-Bus connection/name exits 1. There is no automatic bus reconnection or standalone restart.
Closing the GUI leaves the daemon running. See
[terminal lifecycle troubleshooting](troubleshooting.md#the-daemon-stops-after-bus-loss) for
shutdown delays and retained recovery records.

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

Daemon execution additionally keeps bounded structured lifecycle facts in its
data directory, independently of `RUST_LOG`. This does not copy arbitrary stderr
or change existing command exit codes/output. Only explicit diagnostic `--file`
mode reads that journal directly; default diagnostic commands use D-Bus.
See [persistent daemon diagnostics](troubleshooting.md#persistent-daemon-diagnostics).

The structured journal now correlates runtime attempts and summarizes repeated
watch observations, but its persistent cursor is not the process-local D-Bus log
cursor. See [diagnostic export](#diagnostic-export). A late confirmation cannot end recovery for an
already-dead incarnation; deleting a target cancels its pending retry ownership.

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

Both status and session-list JSON may additionally contain a `runtime` object:

```json
{"phase":"running","health":{"outcome":"reachable","source":"active","measured_at":"2026-10-28T01:02:03Z"},"protection":{"on_core_exit":"hold","holding":false,"interface_up":false,"route_mode":null,"desktop_proxy_owner":true},"retry":{"state":"idle","ordinal":0,"remaining_ms":null}}
```

`health` is null until this generation/core has an accepted observation, including after adopting
an existing core. Replacing the core clears old health. `source` is `confirmation`, `active` or
`watchdog`; outcomes distinguish `reachable`, `unreachable`, `timeout`, `no_network` and `internal`.
Unknown string values must be tolerated. Older daemons omit `runtime`, and synthetic direct
standby rows omit it too. Existing `state`, `error`, latency fields and exit codes are unchanged;
profile-specific status still requires a connected session. Text status appends runtime facts
when available.

**`status --json`, `list sessions --json`** — an array of session objects, adding
`profile`, `owns_system_proxy`, and `interface` (`null` when the profile has none):

```json
[{"profile":"work","state":"connected","server_id":"…","server_alias":"ch-trojan",
"server_name":"Zurich 01","address":"127.72.14.1","socks_port":10808,
"http_port":10809,"latency_ms":84,"error":null,"owns_system_proxy":true,
"interface":{"device":"oxi-work","address":"198.18.9.7","mtu":1500,
"routes":"manual","table":28449,"mark":28449,"up":true}}]
```

While standby is active, the array includes a `default` entry with `state` equal
to `"direct"`, address `"127.0.0.1"` and the configured ports. `server_id`,
`server_alias`, `server_name`, `latency_ms` and `interface` are `null`;
`owns_system_proxy` and `holding_traffic` are `false`, and `selection` is absent.
This service remains reported even when a failed default session is inspectable;
it is not an additional connected tunnel.

**`list servers --json`** — `id`, `alias`, `name`, `protocol`, `address`, `port`,
`country`, `subscription_id`, `subscription`.

**`list profiles --json`, `profile list --json`** — `name`, `description`,
`server`, `socks_port`, `http_port`.

**`list subscriptions --json`** — `id`, `name`, `description`, `send_hwid`,
`server_count`, `updated_at`, and `userinfo` (`{upload, download, total, expire}`
or `null`).

**`core show --json`** — the profile, and one object per setting that reaches the
generated config:

```json
{"profile":"work","settings":[{"setting":"log_level","value":"warning",
"origin":"built-in"},{"setting":"mux.concurrency","value":"16",
"origin":"profile"}]}
```

`value` is always a string, including for numbers and booleans: the settings have
several different types and a stable schema is worth more here than a faithful
one. `origin` is `built-in`, `global`, or `profile`.

---

Next: [configuration.md](configuration.md) · [profiles-and-pools.md](profiles-and-pools.md) · [troubleshooting.md](troubleshooting.md)
