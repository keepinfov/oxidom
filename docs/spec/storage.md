# Files (config & state)

Where oxidom keeps its configuration and state on disk, which daemon owns that store, and the schema of `config.toml`.

Resolve config dir as `$XDG_CONFIG_HOME/oxidom` (`~/.config/oxidom`), data dir as
`$XDG_DATA_HOME/oxidom` (`~/.local/share/oxidom`). Create parent dirs on write; missing files
mean defaults/empty. Invalid or unreadable recovery state instead fails startup closed.

- `~/.config/oxidom/config.toml` — user settings (see schema).
- `~/.config/oxidom/profiles/<name>.toml` — named CLI/systemd connection profiles.
- `~/.local/share/oxidom/subscriptions.json` — cached subscriptions + parsed servers. A server
  may carry `outbound_patch` (a raw JSON object merged onto its generated outbound; see
  `docs/spec/data-model.md`); the key is absent, not null, on every server without one, so
  files predating it round-trip byte-identically.
- `~/.local/share/oxidom/state.toml` — ephemeral `[[sessions]]` records: profile, server id,
  loopback address, fixed SOCKS/HTTP ports, recovery PIDs and the planned interface routes/rule.
  Interface intent is written before persistent kernel application; owned TUN intents use the
  qualified recovery format below, not legacy wildcard cleanup selectors. The legacy flat
  active-server fields are accepted only as migration input.
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
- `~/.local/share/oxidom/store.identity` — private persistent continuity seed, initialized under
  the store lease before recovery; see the read-only identity contract below.
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

## Owned TUN recovery state (binding)

When any session has `interface.ownership`, `state.toml` starts with `format_version = 1`.
Without owned records, the writer keeps the unversioned session shape. The checked loader accepts
normal legacy session and retired flat state, including migration of the old private default
route. It rejects unsupported versions, malformed TOML, invalid ownership and owned records without
the explicit version before recovery or startup mutation. It leaves the file untouched: no
quarantine, defaults or empty replacement. Save also checks the existing file before replacing it;
an invalid or unsupported versioned file must not be overwritten by a later write.

`[sessions.interface.ownership]` contains a random 32-character lowercase hexadecimal `cookie`,
nonzero random route `metric`, optional `gateway_interface` and `device_index`, qualified `routes`,
optional `rule`, and optional `cleanup_dirty` (default false). Ownership and nested route/rule
records reject unknown fields. Each route records `output_interface` and a `route` with required
IPv4 `address`, `prefix` and `table`, plus optional `gateway` or connected-route `interface`.
Prefixes must be canonical networks; the output index and next-hop kind must agree with the
recorded device/gateway identities. The rule records nonzero `mark`, `table` and `priority` in
1..32765. Route intents require a device index and rule identity, are bounded to 512, and belong
only to main table 254 or the recorded private table.

Kernel routes carry protocol **242**, the recorded metric, destination/prefix, table, output
interface and next-hop/scope selectors. Rules carry IPv4 lookup action, protocol **242**, full
`0xffffffff` fwmark mask, mark, table and priority. The mark/table are randomized per ownership
cookie (high bit set), not stable profile marks; rule priority is `10000 + mark % 20000`.
Device identity is its name, recorded ifindex and exact alias
`oxidom-tun-v1:<cookie>`, never its name alone. Protocol 242 is an administrative namespace, not
an unforgeable ownership credential.

An exclusive transient TUN receives its alias and index first. The complete cleanup intent must
be durably saved **before** enabling persistence or adding addresses, routes or rules. A crash
before persistence closes the transient descriptor and lets the kernel remove the device.
Helper replacement PIDs are persisted; failed persistence stops the replacement. Cleanup marks
the owner dirty before destructive work, removes only qualified resources, and retains unresolved
intent on identity, kernel or persistence failure. Dirty cleanup blocks restart/reuse; unresolved
owned recovery blocks new interface activation and replacement of the affected session until
resolved. Ownership failure never falls back to legacy cleanup.

### Downgrade restriction (breaking)

Old binaries **do not refuse** this format: they ignore `format_version` and `ownership`.
For owned interfaces the legacy cleanup fields deliberately stay `created = false`, `routes = []`
and `rule = false`, preventing those readers from applying unqualified device/route/rule deletion.
This is not safe downgrade support: an old writer can discard the only ownership records and
strand resources. The retained `nft_rule` and helper PID are not a replacement for ownership.

The only supported downgrade is to explicitly stop every owned TUN session using the new binary,
complete all cleanup and durable state writes, verify that no owned recovery records remain,
then stop the new daemon and install/start the older binary. Never downgrade with outstanding
owned records, including dirty or crash-recovery records. Do not delete or hand-clear state to
make an older daemon start. Upgrading a normal legacy state remains supported.

## Which daemon owns the store (binding)

State and configuration replacements use a unique, exclusively created `0600` temporary file
in the destination directory. Write, permission and file-sync errors are returned before rename;
only that writer's temporary file is cleaned on failure. Rename is followed by directory fsync.
New `0700` directory components and their entries in their parents are also synchronized.
A failure after rename means the replacement may already be visible but its durability was not
confirmed; it must not be reported as a successful save. This is per-file durability, not a
transaction across the store's separate files.

The last logical store-lease owner explicitly unlocks before closing its descriptor. Diagnostic
writer and bounded reader-snapshot scopes likewise explicitly unlock on success and error, before
reader parsing. A duplicated/inherited descriptor is not a logical lease owner and must not extend
these scopes through pre-exec inheritance. Independent contenders retain their own locks; stable
lock files are never unlinked. Daemon service/worker `Arc` references still retain the actual store
lease through engine destruction, not merely until terminal cleanup returns.

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

### Read-only daemon identity (binding)

The additive `DaemonInfo() -> String` method on the existing interface returns JSON:

```json
{"version":1,"incarnation":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","store_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","capabilities":["probe_cancel","geo_download"]}
```

Both tokens are exactly 64 lowercase hexadecimal characters. Incarnation is a fresh random
256-bit value for each daemon startup, immutable throughout serving. The method reads immutable
facts only: no engine lock, subprocess, network work or cancellation. Capability names describe
implemented methods, not authorization or current readiness. Unknown capabilities are ignorable;
missing required fields, invalid tokens and unsupported versions are not verified identity.
Terminal admission closure rejects this request just like existing requests.

After acquiring `store.lock`, before loading/recovering the engine, create `store.identity` if
absent: 64 random lowercase hexadecimal characters followed by one LF, atomically persisted
and synchronized as `0600`. Existing identity must be a regular file owned by the daemon user,
with one link, exact `0600` mode and exactly those 65 bytes. Do not follow symlinks, repair,
quarantine or regenerate invalid/unreadable identity. Identity persistence/validation failure
fails startup before recovery. Re-sync an existing seed and its directory before advertising it.

Compute store identity as SHA-256 of the literal bytes `oxidom-store-v1` followed by NUL, the
65 seed bytes, then the canonical data-root path and canonical config-root path in that order.
Each path is encoded as its raw Unix bytes prefixed with its byte length as an unsigned 64-bit
big-endian integer. Hex-encode the digest. This covers both roots without publishing paths.
Changing either canonical root, replacing/deleting the seed or independently initializing another
store changes continuity. A symlink alias of the same roots does not. Relocation intentionally
requires a new explicit client selection. Copying/restoring the seed at the same paths can retain
identity; this is not content equality, rollback detection, cryptographic authentication or proof
against a malicious owner. It is independent of diagnostics run IDs, core IDs and logbook IDs.

All existing D-Bus methods and payloads remain compatible. Only explicit `UnknownMethod` selects
legacy/unverified identity; timeout, access denial, malformed JSON or unsupported version does not.

### Client owner binding and selected recovery (binding)

Initial discovery retains its system-first budgets and activation policy. Resolve the well-known
name to a unique owner, then perform `DaemonInfo` and decode readiness `Status` on that unique
destination. Confirm that the well-known name still belongs to this owner before returning.
When initially absent, a read-only well-known `Status` may trigger normal D-Bus activation;
its reply is not identity evidence. Capture the actual selected address, bus instance and owner.
Every operational call, fallback and clone stays pinned to this immutable unique destination on
its original transport; owner replacement cannot redirect a multi-call operation to its successor.

Capabilities come only from validated immutable handshake facts and are cached per binding.
Never call `CancelGeoDownload`, `CancelProbes` or another mutation to probe support. Legacy
daemons without `DaemonInfo` retain existing calls but conservatively disable optional geo-download
and probe-cancel controls, even when the methods may exist. Capabilities do not prove permission.

`DaemonClient::recover_selected` is an explicit candidate-construction API, not an automatic retry
or replacement of any active client. One attempt has a cooperative 2-second aggregate setup,
owner/identity/readiness deadline. Reuse only the captured address; do not reread bus-selection
environment, discover another bus, activate an absent service, bootstrap or invoke a supervisor.
Verified selection accepts only another validated identity with the same store ID, including across
bus/owner/incarnation changes; never downgrade to legacy. Legacy selection accepts only the same
unique owner on the same authenticated bus GUID, not a replacement or a matching unique-name text
on another bus. Failures leave the original binding and all its clones unchanged. Each candidate
has a fresh process-local binding stamp; clones share the stamp.

No mutation is retried or replayed, including after a timeout or lost reply: execution may already
have happened. Recovery does not resolve that ambiguity. A consumer must hydrate required state
and reject retired-binding completions before installing a candidate and resuming control. The
GUI follows the selected-recovery and callback-fencing contract in `gui.md`; an unverifiable
successor stays unavailable and requires explicit new selection rather than silent fallback.

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
remain unchanged. Bus-transport reconnection is not supplied by this launch-ownership contract.

### Terminal daemon lifecycle (binding)

SIGINT/SIGTERM, loss of the selected D-Bus transport or well-known name, and fatal service setup
failure after recovery converge on one terminal cleanup path. Loss is terminal, not permission
to reconnect, reclaim the name, switch stores or replay a request. GUI closure is not a terminal
event. Request the name without allowing or attempting replacement; verify primary ownership
before recurring workers start or Serving is published.

Terminal closure is one-way. Serialize closure with engine transitions and final filesystem
commits: reject new requests with a D-Bus failure, invalidate pending generations, cancel retry
and download work, clear queued probes and wake periodic/backoff waits. Previously admitted
work remains accounted for even when its caller disappears. Delayed results cannot start a core
or interface, reopen standby, install downloaded assets or commit catalog changes after closure.
An operation already inside its serialized commit boundary finishes before closure.

Drain admitted requests and stateful workers without holding application locks, then stop standby
and clean sessions. Repeated cleanup returns its first result rather than running again. Fence
runtime diagnostics only after quiescence and cleanup; diagnostic persistence is not a prerequisite
for closing admission. Keep the store leased through the last engine destruction, including
references retained by the service, so no final state write races a replacement store owner.

Attempt cleanup for every session. Retain unresolved resource metadata and failed-persistence
recovery records rather than treating failure as a clean stop. Startup port overrides and engine
destruction must not erase an unrelated live owner's recovery PID. Recovered-process signalling
pins the process with a Linux pidfd before identity validation and signals that descriptor; failure
to establish safe ownership fails closed rather than signalling a numeric replacement PID. This
does not promise adoption of arbitrary cores or successful cleanup on every kernel.

A signal stop succeeds only if cleanup succeeds. Bus/name loss, setup/wait failure and failed
cleanup return failure, allowing the existing external supervisor policy to apply. Standalone
operation gains no automatic restart loop. Serving checks ownership with a one-second outgoing
method timeout and a 100 ms polling interval; these are not an end-to-end shutdown deadline.

Core version queries have a five-second execution timeout. Query capture closes stdin, bounds
each output stream to 1 MiB, and kills/reaps its direct child on timeout or capture failure.
Existing network, DNS, filesystem/kernel and other helper operations can delay closure or drain;
there is no universal hard shutdown bound. Never release the store merely because a drain timer
expired while work can still mutate it. Forced termination still requires ordinary recovery.

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
`RUST_LOG=off`. Core-resolution/default-profile errors, terminal cleanup errors and collected catalog-load
warning counts are retained without their raw text. `catalog_load_warnings` counts only the
subscriptions-store warnings returned by catalog loading, not all config/state/recovery warnings
that other code writes to legacy logs. No guarantee is made for pre-exec failures, argument
parsing, arbitrary panic payloads, abort, SIGKILL, or execution that never reaches the recorder.
Runtime attribution, repeat summaries and persistent cursor reading are specified below.
Local CLI export follows `cli.md`; memory-log cursors remain separate.

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

### Runtime attribution (binding)

Runtime records add a closed `context` object to version 1 without changing existing lifecycle
record shapes: `{run: u64, session: u64, generation: u64, attempt: u64|null, core: u64|null}`.
`run` comes from one eight-byte random request per recorder; unavailable entropy or zero disables
runtime correlation without an identity-derived fallback. It is a diagnostic namespace, not
proof of daemon/store continuity. Tests inject fixed IDs. Session and attempt IDs are checked,
shared monotonic allocations; exhaustion disables affected attribution, never process control.
No raw-name dictionary or name-derived hash is persisted.

A session ID covers one accepted runtime lifetime, surviving replacement and retries until
actual removal or complete disconnection. Held sessions, pending retries and failed cleanup
retain identity. Recreating an ended runtime, even with the same profile name/server, gets a new
ID. Each accepted start, including automatic retry and settings-driven port restart, gets a new
attempt ID; a cancellation generation alone is not an attempt identifier.
Catalog deletion invalidates affected application generations and reconnect overrides before
recording cancellation/retirement; diagnostic cancellation cannot substitute for actual control
cancellation or allow an old retry to resurrect the deleted target.

Core IDs are separately checked process-local allocations made only after successful OS spawn
or adoption. Opening a log run, failed resolution/configuration/spawn, a PID and the log watermark
are not core IDs. A failed attempt never borrows a surviving core's identity. An existing core
keeps its original context even when a newer-generation start fails before replacing it;
watch/recovery observations retain that trigger rather than restamping its generation.

Capture immutable context and the applicable generation/incarnation/watermark at the engine
ownership boundary and pass it with background work. Validate the subject when taking a probe
snapshot and consuming its result. Stale success/failure records keep the old context and say
`discarded_stale`. Emit accepted confirmation only after interface bring-up and traffic
confirmation. A delayed recovery-success receipt must revalidate that confirmed core before
clearing the retry override. Settings restarts claim their probe job before diagnostic emission,
so delayed persistence cannot steal a newer job.

Additive runtime events and their fields are:

- `session_opened`, `session_retired`: `context`. Retirement reflects observed runtime cleanup,
  not merely a Stop request or successful diagnostic write.
- `attempt_started`: `context`, `origin` (`explicit`, `automatic`, `settings`, `adopted`),
  `target` (`server`, `pool`). Never include target contents.
- `core_started`: `context`, `adopted: bool`. Adoption is not fresh confirmation.
- `attempt_finished`: `context`, `outcome`, `disposition` (`accepted`, `discarded_stale`),
  nullable `failure` (the existing closed failure categories), `os_code`, and closed `detail`.
- `liveness_lost`: `context`, `holding_traffic: bool`. This does not invent a process exit status:
  failed supervision can include an interface-helper failure with Xray still alive.
- `retry`: original trigger `context`, `state` (`pending`, `cancelled`, `recovered`), numeric
  `ordinal: u32` and `delay_ms: u64`. Each actual retry still has a separate attempt ID.
- `watch`: `context`, `policy_epoch: u64`, `mode` (`off`, `conservative`, `aggressive`),
  `outcome`, nullable closed `detail`, and `action` (`observed`, `reset`, `discarded_stale`,
  `conservative_retained`, `recovery_applied`, `recovery_rejected`). Evidence, threshold and
  recovery action are distinct facts; an overtaken conservative action cannot claim a live core.

`outcome` is one of `reachable`, `timeout`, `unreachable`, `no_network`, `internal`,
`inbound_not_ready`, `pool_window_exhausted`, `interface_failed`, `start_failed`, `missing_subject`.
`detail` uses the closed `ProbeDetail` values, never formatted core text. A pool's early termination
keeps its typed observation/detail; only the window cutoff produces `pool_window_exhausted`.

```jsonl
{"version":1,"unix_ms":1234,"pid":42,"event":"attempt_started","context":{"run":1,"session":2,"generation":3,"attempt":4,"core":null},"origin":"explicit","target":"server"}
```

Capture ordering and persistence are separate. No file I/O or fallback stderr occurs while
holding engine, probe, generation, streak, identity or lifecycle-ordering mutexes. The recorder
releases its reducer-state mutex before I/O; a recorder-specific emission mutex serializes its
own writes. Slow storage is still possible and no network/control behavior depends on diagnostic
availability. Immutable facts may reach disk after later application transitions: `accepted`
describes the decision when captured, not currentness at file-write time. Neither append order
nor realtime timestamps establish total application order. Raw child-log lines remain outside
this attribution contract.

### Repeated observations (binding)

Summarize only `watch` events with `action=observed` and an identical complete observation key:
`context`, policy epoch/mode, outcome/detail and action. The first observation emits immediately;
each 16 additional identical observations emits a delta. Keep at most 64 FIFO buckets, with no
timer, idle deadline or extra thread. Cause/action change within a context/epoch flushes its old
bucket. Explicit retirement, replacement of a surviving older core, policy change and capacity
eviction flush pending deltas; closing runtime recording flushes all remaining buckets.
Different sessions, attempts, cores and policy epochs never share a counter. Lifecycle events,
attempt results, retry transitions, recovery/retention actions and stale observations are not
suppressed. Routine healthy checks need not produce records unless they reset a failure streak.

`watch_summary` contains `key` (the original full watch object), `suppressed_count: u64`,
`first_ordinal: u64`, `last_ordinal: u64` and `reason` (`batch`, `context_ended`, `cause_changed`,
`capacity`, `shutdown`). Count means additional occurrences only, excluding the first detail;
each summary is a delta, not a cumulative total. Ordinals are recorder acceptance positions,
not durable cursors, wall times or file offsets; their range may include other observations.
Ordinal exhaustion flushes pending deltas and disables aggregation rather than wrapping.

Failed writes are distinct from suppression. `persistence_gap` reports numeric
`unconfirmed_write_attempts`, `unconfirmed_occurrences`, the safe `last_kind`/nullable
`last_os_code`, and `saturated: bool`. A failed summary contributes its whole delta; an ordinary
failed emitted record contributes one occurrence. A failed gap report updates the safe error
category but never counts itself, preventing recursive amplification. Try one pending gap before
each later emitted record; acknowledge its snapshot only after a successful write. Counters
saturate explicitly. Because write errors can follow visible changes, these are unconfirmed
occurrences, not proof that records are absent. Open failure still follows the disabled-sink
policy above; no delivery claim is made for it.

Before shutdown lifecycle checkpoints, close runtime acceptance and flush pending summaries plus
one bounded gap-only attempt, even if there were no summaries or the final summary failed.
Later runtime events cannot reopen buckets; repeated closure drains no summary twice and can
retry a pending gap once. After the final lifecycle/fatal record, make a final bounded gap drain.
P05 lifecycle/fatal records remain enabled after the runtime fence. This fences diagnostics,
not background workers; the terminal lifecycle separately drains those workers. The fence alone does not guarantee final
evidence on blocked/failing storage or abrupt process termination. Existing D-Bus memory cursors,
logbook records and `ClearLogs` are unchanged.

## Persistent diagnostic reading (binding)

`DiagnosticsSince(cursor: String, limit: u32) -> String` is an additive D-Bus method on
`dev.keepinfov.oxidom1`. It reads only the selected daemon's diagnostic journal, never a caller
path or a different database. Empty cursor starts at the oldest retained bytes. `limit = 0`
means 64 valid records; limits above 256 are capped. Filesystem I/O runs off the D-Bus executor
without holding engine, session or diagnostic-recorder locks. Storage/lock failures are errors,
not empty history. Old clients and `RecentLogs`, `LogsSince`, `ClearLogs` remain unchanged.
New clients report UnknownMethod as unsupported, without substituting memory logs or local files.

The JSON page is version 1 with fields `version`, `records`, nullable `next_cursor`, `gap`,
`rejected`, `incomplete_tail`, `more`. Records use the existing version-1 typed record schema.
Read backup before current, oldest first, consuming only through the returned page position;
pagination never selects a newest tail. `more` means consumable snapshot bytes remain, including
invalid complete lines. Follow `next_cursor` to continue; it is not a logbook sequence, runtime
ordinal, timestamp or authorization token. Pages are separate snapshots, not one atomic export.

The opaque cursor is versioned URL-safe unpadded base64, bounded to 87 characters. It encodes
device/inode, consumed byte offset, prefix length and SHA-256 of consumed bytes. Restarting the
writer does not reset a cursor. One rotation follows the old current into backup then the new
current. Eviction, replacement, truncation or changed consumed bytes invalidate continuity;
an invalid/unrecognized cursor also does so. Return `gap = true` and restart at the oldest
retained position, with unknown lost count. This reset can repeat previously delivered records;
consumers must surface the gap rather than silently append as continuous history. An offset-zero
cursor hashes no unread bytes: legitimate partial-tail repair must not replay an already drained
backup. Exact restoration of the same file identity and consumed bytes is indistinguishable
from continuity; cursors are not authenticated and do not detect every same-uid modification.

Each complete line is bounded to 1 KiB including LF and deserialized into the closed typed schema.
Malformed, oversized or unsupported-version records increment `rejected` and are consumed, but
their text and parser errors are never exported. Unknown fields are discarded, not forwarded.
An incomplete current tail is not consumed and sets `incomplete_tail = true`, not `more` by itself.
An incomplete backup tail is rejected and consumed so traversal can reach current. An initially
empty journal can have null cursor. Remote pages are validated for version, record count/size
and cursor encoding before CLI/report use.

Reading is non-mutating: no creation, chmod, repair or writable open, including for the stable
lock. Pin an owned `0700` directory and open relative leaves with nofollow/nonblock. Require owned,
single-link regular `0600` leaves, each at most 2 MiB, including the lock; refuse unsafe or oversized
entries. A nonblocking shared lock excludes the writer's exclusive lock and permits parallel
readers, including readonly locking on NFS. Snapshot at most 4 MiB and release the lock before
parsing. Missing journal/lock returns NotFound, contention WouldBlock. No wall-clock I/O deadline
is promised; hostile same-uid processes ignoring the lock remain outside the protection boundary.
Existing retention and writer behavior are unchanged; reads do not clear anything.

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

### Saved failover authorization (binding)

An optional profile `[failover]` table stores `enabled` (default `false`), ordered `members`
(default empty), `max_attempts` (default 3, inclusive range 1..64), and `cooldown_secs` (default
30, inclusive range 1..3600). An absent table remains absent on serialization, preserving legacy
bytes and behavior. Enabled policy requires 2..64 unique nonempty canonical server IDs, a
single-server selection resolving to one of those IDs and no pool. An enabled interface additionally
requires the bounded literal-IPv4 TUN envelope, effective hold policy and no custom profile routing
specified in [interfaces.md](interfaces.md#owned-tun-failover-binding).
Activation and automatic admission refuse missing or composite candidates. This is profile
authorization, not a global reconnect setting; see [failover.md](failover.md).

Only authorization is persisted. Episode budget, visited candidates, cooldown deadlines and
generation/core attribution are private runtime state, never resumed after daemon restart.
`SaveProfile` omission preserves existing authorization only for unchanged selection; explicit
JSON `null` removes it. Current clients round-trip the policy and send explicit null when removing
it, rather than conflating removal with a legacy client's omission. Changed selection with an
omitted field revokes authorization. Changed selection or policy disarms the active snapshot;
unchanged saves preserve it. Failed or uncertain persistence cannot arm a grant. External profile
file changes are reloaded and validated before automatic admission, not instantaneously observed.

Every `[core]` key is optional at both levels, and an untouched section is not written to the file
at all. See [Advanced core settings](xray-config.md#advanced-core-settings-binding) for what each
one generates.

### Tunnel watchdog and settings compatibility (binding)

`watchdog` defaults to `conservative` in old on-disk files. It controls synthetic checks for
resilient sessions (`reconnect = true`, a bound `oxidom run` cgroup, or armed saved-profile
failover), not process-exit supervision. Budgets and recovery semantics are specified in
[latency.md](latency.md).
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
are idle, no hold remains and no failover activation owns a generation, enabled standby can
resume. It never automatically claims the GNOME system proxy.

Without a failover-owned activation, an unexpected core exit with `on_core_exit = "release"`
and reconnect disabled allows standby to resume once death is noticed. An automatic reconnect
pending or retrying suppresses direct
standby until that lifecycle ends; it must not open a direct window between attempts.
Failover ownership suppresses standby even after exhaustion or revocation, until explicit Stop
or replacement removes that ownership. No direct window may open between failover attempts.

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
