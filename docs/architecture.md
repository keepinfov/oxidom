# Architecture

How oxidom is put together, and why. For the binding implementation contracts and
the reasoning behind them, see [`spec/`](spec/) — this page is the readable
summary.

## Contents

- [Three crates](#three-crates)
- [The daemon owns the tunnel](#the-daemon-owns-the-tunnel)
- [Finding the daemon](#finding-the-daemon)
- [Running the core](#running-the-core)
- [Sessions](#sessions)
- [Latency probes](#latency-probes)
- [Security model](#security-model)

## Three crates

```
crates/
  oxidom-core/    library: subscriptions, links, Xray config generation,
                  probes, TUN, nftables, D-Bus client, state
  oxidom/         binary `oxidom`: the CLI, and the daemon
  oxidom-gui/     binary `oxidom-gui`: the GTK4/libadwaita interface
```

The GUI is a **thin client**. It does not parse links, generate Xray configs, or
supervise processes — it asks the daemon. Anything the GUI can do, the CLI can do,
because both are clients of the same interface.

The headless package builds without any GTK dependency at all, which is what makes
a server install reasonable.

## The daemon owns the tunnel

`oxidom daemon` holds the engine: the Xray processes, the sessions, the
subscriptions, the probe scheduler. Clients talk to it over **D-Bus**:

| | |
|---|---|
| Bus name | `dev.keepinfov.oxidom.Daemon` |
| Object path | `/dev/keepinfov/oxidom/Daemon` |
| Interface | `dev.keepinfov.oxidom1` |

(The GUI's application id, `dev.keepinfov.oxidom`, is a different thing that
happens to look similar.)

It can run two ways:

- **Session daemon** — on the session bus, as you. Started automatically by the GUI
  or by `oxidom up` if nothing else is running. Its database is in your XDG
  directories.
- **System daemon** — on the system bus, as the `oxidom` user, started at boot.
  Survives logout, and is the only one that can hold `CAP_NET_ADMIN` for TUN. Its
  database is `/var/lib/oxidom`.

Consequently **the choice of daemon is the choice of database**; see
[configuration.md](configuration.md#the-two-databases).

Closing the GUI window does not disconnect anything. The daemon is still there.

## Finding the daemon

A client tries, in order: the system bus, then the session bus, then — for
commands that may start one — spawning a private session daemon.

The subtlety is a race. A GUI autostarting at login and a systemd unit starting the
system daemon can finish in either order; if the GUI got there first and fell back
to a session daemon, it would bind to a *different database*, and the only symptom
would be that your servers had disappeared.

Two things prevent that:

- The system daemon is **D-Bus activatable**, so asking for the name starts it and
  waits, rather than racing it.
- A client that finds the name unowned but sees an installed system-daemon policy
  file waits out a grace period (10 s) before falling back — and only if the bus
  says *nobody* owns the name. `AccessDenied` is a final answer: that user is not
  permitted to drive the system daemon, and a session daemon of their own is the
  correct outcome for them.

Falling back is logged, and the GUI shows which step it is on rather than a blank
window.

Discovery deadlines cover connecting and authenticating to the bus as well as
waiting for `Status`. GUI startup and commands that may start a daemon allow
10 seconds for the whole system phase, 2 seconds for an existing session daemon,
and 6 seconds for a newly started session daemon. Read/control commands that
require an existing daemon allow 2 seconds per bus without the system grace.
Quick absence or access denial can finish a phase sooner. A timeout or `NoReply`
reports an error rather than silently choosing another database.

A spawned daemon that exits early is reported with its exit status, after a
short same-session readiness grace for a competing starter that won the store
lock. Discovery failure does not kill a daemon. These are cooperative I/O
deadlines, not guarantees against stalled filesystem or process-spawn work;
ordinary calls and initial GUI hydration have separate timeout semantics.

## Who supervises the session daemon

By default, the GUI or `up`/`connect` bootstraps one standalone session daemon
without a restart loop. The launcher reaps its child eventually but never kills
or restarts it because discovery failed or the GUI closed. Automatic startup
creates a new POSIX session and null stdin; stdout/stderr remain available for
diagnosis. This does not escape a launcher's cgroup or promise logout survival.

Managed installations may opt into a separate **user** `oxidom-session.service`.
The user manager alone owns its failure restarts, independently of the tray's
cgroup. Clients must receive `OXIDOM_SESSION_DAEMON=external`, which preserves
normal discovery but forbids private bootstrap during absence or a restart gap.
There is no automatic takeover of an existing standalone owner. The system
daemon remains a distinct service/store and retains discovery priority.

The user unit uses normal user XDG directories without capabilities or an
implicit lingering setting. A crash restart is not universal tunnel resumption:
only eligible pool cores are adopted; other recorded cores and interfaces follow
existing cleanup. [Installation](installation.md#managed-session-daemon) explains
the explicit opt-in and migration boundaries.

## Running the core

Each session writes its own `current-config-<profile>.json` (mode `0600` — it holds
credentials) and runs `xray run -c <that file>`. Output goes into a 5000-record ring
buffer, which is what the GUI's Logs page shows. The core gets no log file of its own:
managed services send the daemon's stderr to the journal; standalone launches retain the
supplied streams without that guarantee. The GUI, which detaches and has no
journal, keeps its own `oxidom-gui.log` — both described in
[`spec/storage.md`](spec/storage.md).

Before spawning, oxidom **resolves the binary first and then checks the ports** —
in that order, so a busy port can never mask a missing core. Stopping is SIGTERM,
a 2-second grace, then SIGKILL.

Connecting remains `connecting` in public status until confirmation proves the SOCKS path
carries traffic and the requested interface is ready. The internal process state is available
to probes and supervision throughout. Failed confirmation reports its cause rather than
publishing an unverified connection as connected.

Synthetic tunnel checks are conservative by default: they warn without killing a live core.
Aggressive live-core recovery is explicit; process-exit supervision is independent. Results
belong to one core incarnation and policy epoch, not merely a profile name.

**Crash recovery.** The daemon records what it did before doing it, so a crash can
be cleaned up: orphaned Xray children, a stuck GNOME system proxy, leftover routes,
rules and TUN devices are all repaired on the next start. A recovered process is
adopted only after checking it is really ours — the cmdline must name our config
file, and a recovered tun2socks must name our device. The binary is
user-configurable, so its name proves nothing.
An exclusive store lock is acquired before loading or recovering anything, so another daemon
cannot enter recovery against a live owner's store.

The system unit deliberately uses `KillMode=process`: the cores, not the daemon,
carry the traffic, so a daemon crash should not drop every tunnel with it. A clean
stop still tears them down.

## Sessions

A session is the running instance of one profile — its core, its selection, its
loopback address, its ports, and optionally its interface. Several may run at once.
A profile that is already up cannot be brought up twice.

A session's selection is a single server **or** a [pool](profiles-and-pools.md#pools).
A pool session deliberately has *no* active server: nothing is allowed to quietly
pick the first member and call it the exit. Everything meaning "the tunnel is
carrying server X" — the connected highlight, the proxied latency reading, the
egress cache — is keyed by session, and a pool reports its live exit from the
core's own observatory instead.

Runtime state is written to `state.toml` **before** it is applied to the kernel, so
crash cleanup may safely over-delete an idempotent record but can never forget one
that was applied.

## Latency probes

Four methods, selectable in Settings: `icmp` (shells out to `ping` — no raw
sockets), `tcp`, `http_head`, `http_get`.

A probe runs one of two ways:

- **Proxied** — through the live session, measuring the tunnel you are using.
- **Direct** — for a server that is not connected, by starting a throwaway core for
  that one server on OS-allocated ports and making one request through it. The
  temporary core and its config are removed when the measurement ends.

At most 8 probes run at once, because each HTTP probe is a process.

A reading records the method **actually used**, not the one configured — a TCP
probe of a Hysteria2 server falls back to ICMP, since Hysteria2 is QUIC over UDP
and has no TCP port to open.

"Unreachable" and "you have no network" are distinguished by checking for any
usable default route, so a local outage is never laundered into a verdict about
somebody's server.

## Security model

- **The GUI and CLI run unprivileged.** oxidom never escalates on its own. The only
  privileged component is the opt-in system daemon, and only when TUN is enabled,
  and only `CAP_NET_ADMIN`.
- **The system daemon's D-Bus policy is not an "any local user" surface.** Its
  methods rewrite the machine's proxy configuration, spawn the core, and edit
  subscriptions. Only `root`, group `wheel`, and group `oxidom` may send to it;
  everyone else is denied. An unprivileged service account on the same machine
  cannot redirect your traffic.
- **Binary paths cannot be set remotely on a privileged daemon.** Pointing a root
  daemon at an arbitrary executable is a remote-execution primitive, so a system
  daemon ignores `xray_binary`, `tun2socks_binary` and `nft_binary` from clients.
- **Ports pinned on the unit command line cannot be changed by clients**, so one
  client cannot move an inbound out from under everyone else.
- **No telemetry.** HWID is opt-in per subscription and the identifier is not even
  generated until something opts in.
- **Subscription URLs are credentials** and are excluded from error messages and
  from `--json` output.
- All state files are `0600`, their directories `0700`, and every write is atomic.
- An unparseable file is quarantined, never overwritten.

---

Next: [routing.md](routing.md) · [configuration.md](configuration.md) · [`spec/`](spec/)
