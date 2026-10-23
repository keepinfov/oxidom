# Files (config & state)

Where oxidom keeps its configuration and state on disk, which daemon owns that store, and the schema of `config.toml`.

Resolve config dir as `$XDG_CONFIG_HOME/oxidom` (`~/.config/oxidom`), data dir as
`$XDG_DATA_HOME/oxidom` (`~/.local/share/oxidom`). Create parent dirs on write; never panic on
missing files (treat as defaults/empty).

- `~/.config/oxidom/config.toml` — user settings (see schema).
- `~/.config/oxidom/profiles/<name>.toml` — named CLI/systemd connection profiles.
- `~/.local/share/oxidom/subscriptions.json` — cached subscriptions + parsed servers. A server
  may carry `outbound_patch` (a raw JSON object merged onto its generated outbound; see
  `docs/spec/data-model.md`); the key is absent, not null, on every server without one, so
  files predating it round-trip byte-identically.
- `~/.local/share/oxidom/state.toml` — ephemeral `[[sessions]]` records: profile, server id,
  loopback address, fixed SOCKS/HTTP ports, recovery PIDs and the planned interface routes/rule.
  Interface intent is written before kernel application so crash cleanup may safely over-delete
  idempotent records but can never forget an applied route. The legacy flat active-server fields
  are accepted only as migration input.
  `[[run_cgroups]]` records are the same file's other half: the profile, cgroup path and ancestor
  level of an `oxidom run` scope whose traffic must not reach the ordinary path while no tunnel
  carries it. They are deliberately not part of `[[sessions]]` — `down` removes the session while
  the scope keeps running — and a restart rebuilds the block from them. A record whose cgroup no
  longer exists is dropped, with its chain, on the next start or teardown.
- `~/.local/share/oxidom/hwid` — random per-install id (only generated/used if a sub opts in).
- `~/.local/share/oxidom/store.lock` — the daemon's exclusive store lock, held from before
  loading or recovering state until exit. The file is private and must never be unlinked to
  release the lock: every contender must lock the same inode. A second owner fails without
  running recovery, regardless of which bus it intended to serve.
- `~/.local/share/oxidom/oxidom-gui.log` — the graphical client's own log, `0600`, rotated at 2MB
  with one `.log.1` kept. Written only by the GUI, which detaches and sends stderr to `/dev/null`
  and so has no journal.
- `~/.local/share/oxidom/oxidom-daemon.log`, `.log.1` and `.log.lock` — bounded structured daemon
  lifecycle diagnostics, independent of stderr/journal and distinct from raw logbook records.
  The exact format, limits and failure policy are specified below.
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

State and configuration replacements use a unique, exclusively created `0600` temporary file
in the destination directory. Write, permission and file-sync errors are returned before rename;
only that writer's temporary file is cleaned on failure. Rename is followed by directory fsync.
New `0700` directory components and their entries in their parents are also synchronized.
A failure after rename means the replacement may already be visible but its durability was not
confirmed; it must not be reported as a successful save. This is per-file durability, not a
transaction across the store's separate files.

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

Discovery uses monotonic phase deadlines covering transport setup, authentication/Hello, proxy
construction and the readiness `Status` call, not merely the wait for a method reply:

- `connect_any`: the whole system phase has 10 seconds, including its first attempt and any
  installed-policy grace retries; the existing session phase has 2 seconds; starting and reaching
  a session daemon has 6 seconds. Quick confirmed absence or access denial advances immediately
  unless installed-system grace applies. Retries and sleeps share their phase's remaining budget.
- `connect_existing`: at most 2 seconds per bus, without installed-system grace or an explicit
  session-daemon spawn. Ordinary D-Bus system-service activation is still possible.
- A timeout, `NoReply`, or other indeterminate readiness failure returns an error. It is not
  evidence that the system name is unowned and must not select another database. Confirmed
  absence, a missing/refusing bus socket, or access denial can permit fallback.
- Check the spawned child's exit while waiting. After exit, allow up to 2 seconds of same-session
  readiness within the original 6-second budget: another starter may have won the store lock.
  If no daemon answers, report the original exit status. Never kill a daemon on discovery timeout.

These are cooperative I/O deadlines, not hard real-time limits on synchronous callbacks,
filesystem lookup or process spawning. On expiry, drop pending async setup; zbus's Unix connect
can already be running as uncancellable blocking work, so the persistent discovery runtime caps
such blocking work at one thread. Normal RPCs retain a separate 300-second per-reply limit, not
an aggregate startup, hydration or subscription-update deadline. No mutating request is retried
because of a client timeout.

### Session-daemon supervision ownership (binding)

Clients discover or bootstrap; they are not restart supervisors. `OXIDOM_SESSION_DAEMON` is
a client launch policy, not a `config.toml` key, store selector or proof of the serving process's
supervisor. Unset, empty and `standalone` preserve one-shot private bootstrap. `external` keeps
system-first discovery but forbids a private spawn when no existing daemon is available. Return
an externally-managed-unavailable error instead; never run `systemctl`, retry a mutation, kill an
owner or race the configured supervisor. Invalid/non-Unicode values fail before discovery and
must not be echoed in errors. Existing-only commands still never bootstrap.

Automatic standalone startup keeps the env/sibling/PATH binary resolution and caller
environment, opens null stdin, creates a new POSIX session and resets/unblocks SIGINT/SIGTERM
before exec. Preserve stdout/stderr so startup diagnostics are not discarded. Provision one
child-specific waiter before spawning, retain exclusive `Child` ownership through discovery,
then transfer it on success, failure or unwind. No discovery outcome kills or restarts the child.
The wait owner lasts only as long as its launching process; after that normal OS adoption applies.
This is reaping, not crash restart, cgroup isolation, logout survival or full stream isolation.
Foreground `oxidom daemon` keeps its existing service/terminal behavior.

The opt-in `oxidom-session.service` user unit owns managed starts/restarts in a separate cgroup
from the GUI/tray. It is `Type=dbus`, uses the same session-bus name and ordinary user XDG roots,
has no capabilities or `StateDirectory`, and follows the user manager's `default.target`, not
`graphical-session.target`. Restart only on failure, after 2 seconds, limited to 3 starts per
60 seconds. Allow 180 seconds for unit startup, including first core installation.
`KillMode=process` preserves children for existing recovery; only eligible pool cores
are adopted, not every session. Installation must neither enable this unit nor enable lingering.
Stopping the service does not authorize clients in external mode to bootstrap a substitute.

NixOS enables the mode only with both `programs.oxidom.enable` and
`programs.oxidom.sessionDaemon.enable`; the latter defaults to false. Set external mode in login
session variables and explicitly in the tray, and order the tray after the daemon with `Wants`,
not reverse `PartOf`/`Requires` coupling. Custom XDG/binary settings belong in the user manager's
environment, not only in a client shell. Service enablement alone cannot constrain clients that
lack the policy, and older clients do not honor it. Never take over a pre-existing standalone
owner; switching owners is an explicit operator action. D-Bus signatures and on-disk schemas
remain unchanged. Bus-transport reconnection and generalized shutdown quiescence are not supplied
by this launch-ownership contract.

## Persistent Diagnostics (binding)

Only daemon execution opens `data_dir()/oxidom-daemon.log`, including foreground, managed and
automatically bootstrapped session daemons. Resolution uses the test override, then nonempty
`STATE_DIRECTORY`, then user XDG data, like the store. No separate root or setting is introduced.
GUI and ordinary CLI commands do not open it. Initialize before stop-signal setup, store locking
or recovery, without starting threads. Never discard supplied stdout/stderr or change launch,
store, routing or exit semantics because diagnostics failed.

The journal records safe lifecycle checkpoints and typed failure categories, not a tee of the
log facade, error chains, raw core output or configuration. Records contain only closed enums,
fixed-width numbers and booleans: no paths, names, server addresses, links, credentials, arbitrary
messages or environment values. Unrecognized error types become `other`, not formatted text.
This does not sanitize or replay legacy logbook/terminal output.

Version 1 is UTF-8 JSONL. Every new record has `version: 1`, `unix_ms: u64` (realtime, not a
monotonic ordering guarantee), `pid: u32`, and a flattened event. Literal examples including LF:

```jsonl
{"version":1,"unix_ms":1234,"pid":42,"event":"phase","phase":"starting"}
{"version":1,"unix_ms":1235,"pid":42,"event":"problem","phase":"store","kind":"store_busy","os_code":null,"fatal":true}
{"version":1,"unix_ms":1236,"pid":42,"event":"catalog_load_warnings","count":3}
```

`phase` values are `starting`, `signals`, `store`, `recovery`, `core_resolution`, `default_profile`,
`bus`, `serving`, `shutdown`, `stopped`. These describe diagnostic control-flow checkpoints, not
tunnel health or proof that every resource was cleaned. `problem` includes the current phase,
`fatal: bool`, a numeric OS code when available (otherwise null), and `kind`: `store_busy`,
`permission_denied`, `not_found`, `already_exists`, `invalid_input`, `invalid_data`, `storage_full`,
`read_only_filesystem`, `write_zero`, `interrupted`, `other_io`, `bus`, or `other`.

Record checkpoints before operations that may fail or print to a blocked stderr. Record returned
fatal errors before the CLI prints the original error and exits; retain those facts even with
`RUST_LOG=off`. Nonfatal core-resolution/default-profile/shutdown errors and collected catalog-load
warning counts are retained without their raw text. `catalog_load_warnings` counts only the
subscriptions-store warnings returned by catalog loading, not all config/state/recovery warnings
that other code writes to legacy logs. No guarantee is made for pre-exec failures, argument
parsing, arbitrary panic payloads, abort, SIGKILL, or execution that never reaches the recorder.
Per-attempt attribution, repeat summaries, persistent cursors and export are separate work.

Each newly encoded record is at most 1,024 bytes including LF. Current and one backup
`oxidom-daemon.log.1` are each at most 2,097,152 bytes, at most 4 MiB payload combined. A separate
empty stable `oxidom-daemon.log.lock` serializes size checks, tail repair, rotation and append
across threads/processes; never unlink it. Acquisition is nonblocking: contention reports
unavailability rather than waiting for a writer. Rotation uses projected size, and a failed
required rotation never falls through to append. On restart, append within the same budget and
repair an incomplete current tail to its last LF. Already oversized generations are refused
unchanged; opening does not certify externally supplied complete lines as valid/safe records.

New directory components are created private; the final owned directory is pinned and set to
`0700`. Relative opens and rotation stay on that pinned directory even if its pathname changes.
Current, backup and lock are owned regular files with one link, `0600`; reject leaf symlinks,
hardlinks and nonregular files before modification. Reject a symlink final directory. Ancestor
resolution and hostile same-UID entry mutation are outside this guarantee.

Successful appends synchronize file data and directory changes; permission metadata is also
synchronized. This is synchronous I/O for sparse lifecycle events, not a filesystem time bound,
cross-file atomic rotation or a power-loss transaction. An error can follow a visible partial
change. Failed appends attempt rollback; later writes repair incomplete current tails.

Unavailable, contended, unsafe, oversized, full or failing storage never changes the original
daemon result or chooses an alternate destination. Emit at most one fixed safe warning per
process through stderr and the process-local logbook, without recursive logging. If open fails,
file diagnostics stay disabled for that process; an already-open sink attempts later events
again after write failure. Neither successful persistence nor an alternate fallback file may be
claimed when storage failed. No file can be guaranteed when storage and terminal fallback are
both unavailable. Existing D-Bus log cursors and `ClearLogs` remain memory-only and do not read,
replay or delete these generations.

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
standby_proxy = false         # direct SOCKS/HTTP only when every session is idle and none holds traffic
reconnect = false             # reconnect after an unexpected core exit; explicit opt-in
watchdog = "conservative"     # off | conservative | aggressive; independent of process supervision
on_core_exit = "hold"         # hold | release — a core that exits by itself keeps its routes,
                              # so traffic is dropped rather than released; a profile overrides
latency_method = "http_get"   # one of: icmp | tcp | http_head | http_get
latency_test_url = "https://www.gstatic.com/generate_204"
subscription_user_agent = "v2rayN/6.45"    # panels gate the body *and its format* on this
geoip_url = ""                # empty: the built-in source. https only; digest is <url>.sha256sum
geosite_url = ""              # empty: the built-in source. Chosen separately from geoip_url
xray_binary = ""              # empty: install/use the pinned managed Xray; env/PATH are matching-version fallbacks
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

### Tunnel watchdog and settings compatibility (binding)

`watchdog` defaults to `conservative` in old on-disk files. It controls synthetic checks for
resilient sessions (`reconnect = true` or a bound `oxidom run` cgroup), not process-exit
supervision. Budgets and recovery semantics are specified in [latency.md](latency.md).
`GetSettings` and `SetSettings` retain their signatures. Omitting `watchdog` from a settings
payload preserves the current daemon value; a valid explicit value replaces it. A policy
change invalidates in-flight checks as well as stored failure streaks.

Absence of the key in a **daemon response** is different from an old file: that daemon implements
the previous aggressive policy and cannot configure it. Clients must mark the control unsupported
rather than claiming the new conservative default is active.

### Geo source and update ownership (binding)

Only the user's `geoip_url` and `geosite_url` settings choose download sources. Subscription
rules, geo URLs and remote rule-set URLs are reported as not applied; importing or refreshing a
subscription grants no permission to fetch or adopt them. Accepting provider rules and accepting
a provider's data source are separate trust decisions, neither implicitly enables the other.

The daemon alone writes its private `assets` copies, on explicit install/adopt requests. It
checks HTTPS and the published sidecar digest for downloads. Distribution-provided assets remain
owned by their package; adoption copies them rather than updating them in place. There is no
scheduled updater or Xray `geodata` block on the pinned 26.3.27 core, and no second writer is
started as a fallback. Updating private copies requires an explicit action and reconnection to
load them. A future scheduler or hot reload requires a new ownership and compatibility contract;
an old daemon/core must continue to report manual updates, never claim unsupported hot reload.

### Direct standby (binding)

`standby_proxy` is opt-in, defaults to false, and is serialized explicitly so disabling it also
travels through `SetSettings`. When enabled, it
serves direct SOCKS5 and HTTP on **`127.0.0.1` only**, at the daemon's configured `socks_port`
and `http_port`, not each profile's loopback address or ports. Traffic uses the machine's
ordinary connection and is not protected by a tunnel.

Standby may run only when **no session in any profile** is connected, connecting or holding
traffic. Starting any session must stop both listeners and close already accepted direct
connections before the session starts; closing only the listening sockets would leave direct
traffic alive alongside the tunnel. A held route suppresses standby even if its core is gone.
`on_core_exit = "hold"` remains fail-closed, never an implicit direct fallback. Once all sessions
are idle and no hold remains, enabled standby can resume. It never automatically claims the
GNOME system proxy.

After an unexpected core exit with `on_core_exit = "release"` and reconnect disabled, standby
resumes once the death is noticed. An automatic reconnect pending or retrying suppresses direct
standby until that lifecycle ends; it must not open a direct window between attempts.

### Standby over D-Bus (binding)

On `dev.keepinfov.oxidom1`, `GetSettings() -> s json` and `SetSettings(s config_json) -> s json`
retain their signatures. `Config.standby_proxy` is an additive boolean, defaulting to false when
loading old files or settings. A `SetSettings` payload **omitting** it preserves the daemon's
current value so an old client cannot clear an opt-in. Explicit `false` disables it.

`Status() -> s json` adds `StatusInfo.standby_proxy`, defaulting to false for older daemons. It
reports **actual direct service**, not merely the configured preference. Existing `state` values
(`disconnected`, `connecting`, `connected`, `error`) and the session list keep their shapes; no
new D-Bus state enum variant is needed. A failed default session may remain in `sessions` while
standby is active, so clients must inspect the flag independently of that session's error.
The CLI's `direct` presentation is described in [cli.md](cli.md), not a new tunnel session.
