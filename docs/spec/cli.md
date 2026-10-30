# CLI (clap derive)

The command surface of the headless binary: its verbs, its output and exit codes, and how a
server handle resolves.

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
  Explicit Stop/`down` also deletes an owned failover TUN after qualified cleanup; only legacy
  non-owned TUNs retain their persistent device on ordinary disconnect. Failed cleanup retains
  ownership and blocks reuse, rather than reporting a clean stop.
- `oxidom alias <HANDLE> <NEW>` changes a server alias.
- `oxidom server add [--protocol P] [--file PATH|-]` creates a server by hand. Without
  `--file` it opens `$EDITOR`/`$VISUAL`/vi on a commented TOML template for the protocol;
  with it, the draft is read whole from a file or stdin. The body is a `ServerDraft`
  (`docs/spec/data-model.md`), and validation is the daemon's: the CLI sends the draft and
  prints the daemon's error, so the dialog and the command line reject with one voice. An
  unchanged template creates nothing. `server` is a reserved profile name.
- `oxidom profile {list,show,new,edit,rm}` manages daemon-owned profiles.
- `oxidom daemon [--system --socks-port --http-port]` runs the D-Bus service.
  SIGINT/SIGTERM returns success only after successful terminal cleanup; bus/name loss,
  startup/wait failure or failed cleanup returns failure. Terminal request rejection and
  draining follow `storage.md`; closing the GUI is not a daemon stop request.
- `oxidom <PROFILE> run -- <cmd>...` and `oxidom <PROFILE> run -c "<cmd>"` run one command
  inside the profile's routing domain. The `-c` string is split with shell-word rules but never
  passed to a shell. A proxy-only profile refuses safely and points to `oxidom env`.

Only `up` and `connect` may spawn a private session daemon; every other control command requires
an existing daemon.

Discovery, including bus connection/authentication and readiness, follows the monotonic phase
budgets in `storage.md`: existing-only commands allow 2 seconds per bus; `up` and `connect` allow
10 seconds for system discovery, 2 seconds for an existing session daemon and 6 seconds for a
spawned one. Timeout/`NoReply` fails rather than silently selecting another database. A spawned
daemon's early exit is reported after a bounded same-session readiness grace for a competing
starter. These are discovery I/O budgets, not total command or ordinary RPC deadlines; no command
syntax, D-Bus signature or exit-code mapping changes.

`OXIDOM_SESSION_DAEMON=external` also forbids `up` and `connect` from privately bootstrapping an
unavailable session daemon. Existing system/session daemons remain usable in the normal order;
absence reports daemon-unavailable (exit 4), not a service-manager invocation. Unset, empty or
`standalone` retains private bootstrap. Invalid values fail before discovery without echoing the
value. The policy does not daemonize foreground `oxidom daemon` or change existing-only commands.

The canonical order is verb first (`oxidom up work`). For profile-scoped commands, profile first
is an argv-normalized synonym (`oxidom work up`); a real subcommand in the first position always
wins. `oxidom env` prints POSIX `export` statements for both SOCKS and HTTP endpoints.

Data goes only to stdout; warnings, errors, and ambiguous-handle candidates go to stderr. JSON
uses the fixed DTOs in `oxidom-core/src/cli_json.rs`. `SessionOutput` carries `holding_traffic`
beside `state`: a session whose core exited while it kept its routes is in `error` and dropping
traffic, which is a different claim from `error` alone. The sessions table keeps `STATE` as the
legacy lifecycle and shows holding separately, never as a replacement lifecycle.

### Runtime session facts (binding)

`SessionInfo` has an additive `runtime: Option<RuntimeSessionInfo>`, serde-defaulted and omitted
when absent. Both CLI JSON DTOs forward it unchanged. Old payloads retain their shape when it is
absent, unknown fields are ignored, and all string vocabularies below are open: clients must
degrade gracefully for unknown values. No D-Bus method signature, legacy `state`/`error`, or exit
code changes. Profile-specific status retains its existing connected-session requirement.

```json
{"phase":"verifying","health":{"outcome":"timeout","source":"confirmation","measured_at":"2026-10-28T01:02:03Z"},"protection":{"on_core_exit":"hold","holding":true,"interface_up":true,"route_mode":"manual","desktop_proxy_owner":false},"retry":{"state":"attempting","ordinal":2,"remaining_ms":null}}
```

Every runtime struct is serde-defaulted. `phase` is derived only from raw process lifecycle and
confirmation pending: disconnected -> `stopped`, connecting -> `starting`, connected/pending ->
`verifying`, connected/confirmed -> `running`, error -> `failed`. Legacy error overrides do not
infer phase. Holding and retry do not replace lifecycle.

`health` is null until an attributable application-owned session observation is accepted; it is
never copied from profile latency or diagnostic records. It contains `outcome` (`reachable`,
`unreachable`, `timeout`, `no_network`, `internal`), `source` (`confirmation`, `active`, `watchdog`)
and RFC3339 UTC `measured_at`. Its generation, core incarnation and spawn watermark must match
the current subject. Replacement resets it; adoption has no health until measured. An observation
is evidence at its timestamp, not a perpetual claim of current reachability.

`protection` reports only recorded scope: snapshotted `on_core_exit` (`hold`/`release`), `holding`,
`interface_up`, optional `route_mode` (`manual`/`list`/`default`) and `desktop_proxy_owner`. These
facts do not assert that all machine traffic is protected, nor verify external routing state.
The `holding`, `interface_up` and `desktop_proxy_owner` fields are optional booleans: missing or
null means unknown, never false. Current daemon projections always emit a measured/recorded
boolean (`Some(true)` or `Some(false)`); a defaulted `runtime: {}` asserts no negative facts.

`retry` is an application-owned generation-scoped episode, independent of optional diagnostics:
`state` is `idle`, `backoff`, `attempting` or `exhausted`; `ordinal` is the zero-based attempt ordinal;
`remaining_ms` is present only in backoff, computed from the actual monotonic wait deadline,
saturating at zero and rounded up to milliseconds. Attempting begins at the actual start
transition, not when a timer expires. Cancellation, replacement and confirmed recovery clear the
episode to idle (ordinal zero, remaining null). Bounded saved-profile failover reports
`exhausted` at budget/candidate exhaustion with remaining null, independently of phase or hold;
it never authorizes infinite reconnect underneath. Waiting for the first automatic attempt and
that admitted attempt both use ordinal zero; positive fractional milliseconds round up just as
for ordinary reconnect. Revocation prevents further admissions, not an already-admitted
confirmation. Authorization and its lifecycle follow [failover.md](failover.md).
For ordinary same-target reconnect, disabling resilience cancels only an owned retry episode,
invalidating its generation and
removing its reconnect override together if it is still waiting; manual generations are unaffected.
An already-started automatic attempt retains ownership through confirmation, but further retries
for that episode are permanently disabled. Its live confirmed success can clear the reconnect
override without resilience still enabled; failure uses existing cleanup and retires the episode.
Re-enabling does not revive a cancelled waiter or rearm a disabled in-flight episode.
A new explicit connection claims its own generation. Automatic
starts validate current resilience, generation, matching reconnect override and live backoff
episode under the engine lock before changing the episode to attempting or starting a core.
Saved-profile failover instead revalidates its persisted grant, exact catalog IDs and current
generation at admission. Global reconnect does not opt in to alternates or rearm a revoked grant.
No dedicated failover flag or command is introduced: edit `[failover]` with the existing
`oxidom profile edit <name>` and explicitly activate the saved profile with `oxidom up <name>`.
Enabled interfaces require the bounded owned-TUN contract in [interfaces.md](interfaces.md#owned-tun-failover-binding):
effective hold, literal unicast IPv4 typed candidates, no custom routing and exclusive TUN
admission. Proxy-only sessions may coexist. Exhaustion/revocation does not release held routing;
explicit Stop is required. P13 owns dedicated controls; source implementation does not establish
P12 completion or substitute for required P14/P15 validation.

The text session table adds `PHASE HEALTH PROTECTION RETRY` when any row has runtime facts.
Health includes outcome/source/timestamp, protection labels each recorded fact, and retry includes
state/ordinal/countdown. Missing runtime rows render unknown rather than inventing facts. Older
payloads with holding add a separate `HOLDING` column. Successful profile-specific text status
appends the same runtime facts. Synthetic direct standby rows carry no runtime object.

`status` (without a profile) and `list sessions`, including their JSON forms, report active
standby service as a `default` row with `state = "direct"`, address `127.0.0.1`, and the daemon's
configured SOCKS/HTTP ports. This is direct service, not a tunnel or a new persisted session.
It must remain visible even if a failed default session is still recorded. Its server fields,
latency and interface are null, `owns_system_proxy` and `holding_traffic` are false, and no pool
selection is attached. A session holding routes instead remains `error` with
`holding_traffic = true` in JSON and separate holding facts in the table; direct standby is forbidden while
any session holds traffic, connects or is connected. The D-Bus status flag underlying this
presentation is specified in [storage.md](storage.md#standby-over-d-bus-binding).

`status default` reports the same direct service using the single-session `StatusOutput` shape,
with no selected server or latency. The `error` field retains any previous default-session
failure; text output appends it as `previous error`. In the unscoped listing the direct row
replaces the failed default row and preserves that error in JSON, without hiding other profiles.

Exit codes are binding:

| Code | Meaning |
|---:|---|
| 0 | Success |
| 1 | Command error |
| 3 | No active connection |
| 4 | Daemon unavailable |

## Persistent diagnostics and reports (binding)

`oxidom core diagnostics [--cursor TOKEN] [--limit N] [--file PATH]` prints one version-1
persistent diagnostic page as JSON, with the schema and continuation/gap semantics in
`storage.md`. It does not accept `--json` because JSON is its only format. No daemon is started.
Without `--file`, require the existing selected daemon and its additive `DiagnosticsSince`
method; no fallback to another store or memory-only logs is permitted. With `--file`, read exactly
that journal plus its `.1`/`.lock` locally without contacting D-Bus, creating files or changing
permissions. This explicit path supports diagnosing failures before readiness. It does not grant
access to another uid's private journal; system-daemon reads normally use D-Bus.

`oxidom core report [--cursor TOKEN] [--limit N] [--file PATH] [--output PATH]` reads the same
single bounded page and formats a local report using the shared report/redaction implementation.
Default output is stdout; `--output` atomically replaces a file with mode `0600`, creating missing
directories privately. The CLI, never the privileged daemon, opens the output path. No upload or
clipboard operation occurs. Versions/install/distribution/desktop are local-client facts; daemon
and core versions, connection transport and subscription User-Agent are explicitly not collected.
No raw memory logs, server identities, config or journal path are included. Environment-derived
text headings are redacted as well as record lines. The report states page limits, gap/rejection/
partial-tail facts and review requirements, and includes the generated next cursor unchanged.
It is not a complete or atomic diagnostic archive; `more` requires another page request.
Redaction is best-effort, not a guarantee that arbitrary raw logs are safe to share.

No new top-level profile word is reserved. Exit codes remain 0 on a successfully read/exported
page (including explicit gaps/rejections), 1 on storage/unsupported-method/export errors, and 4
when the default mode cannot reach an existing daemon. Data stays on stdout, errors on stderr.

## Handles and aliases (binding)

Server ids use hand-written FNV-1a 64 and aliases are globally unique, stable human handles.
`handle::resolve` prefers an exact alias, then an exact id, then a unique case-insensitive
substring of alias or name. No match is an error; multiple substring matches are an error with
the candidates listed in stderr. Aliases are lowercase ASCII letters/digits/hyphens, at most 32
characters, and cannot be exactly 16 hexadecimal characters.
