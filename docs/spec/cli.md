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
- `oxidom alias <HANDLE> <NEW>` changes a server alias.
- `oxidom profile {list,show,new,edit,rm}` manages daemon-owned profiles.
- `oxidom daemon [--system --socks-port --http-port]` runs the D-Bus service.
- `oxidom <PROFILE> run -- <cmd>...` and `oxidom <PROFILE> run -c "<cmd>"` run one command
  inside the profile's routing domain. The `-c` string is split with shell-word rules but never
  passed to a shell. A proxy-only profile refuses safely and points to `oxidom env`.

Only `up` and `connect` may spawn a private session daemon; every other control command requires
an existing daemon.

The canonical order is verb first (`oxidom up work`). For profile-scoped commands, profile first
is an argv-normalized synonym (`oxidom work up`); a real subcommand in the first position always
wins. `oxidom env` prints POSIX `export` statements for both SOCKS and HTTP endpoints.

Data goes only to stdout; warnings, errors, and ambiguous-handle candidates go to stderr. JSON
uses the fixed DTOs in `oxidom-core/src/cli_json.rs`. `SessionOutput` carries `holding_traffic`
beside `state`: a session whose core exited while it kept its routes is in `error` and dropping
traffic, which is a different claim from `error` alone, and the sessions table prints its `STATE`
as `holding`. Exit codes are binding:

| Code | Meaning |
|---:|---|
| 0 | Success |
| 1 | Command error |
| 3 | No active connection |
| 4 | Daemon unavailable |

## Handles and aliases (binding)

Server ids use hand-written FNV-1a 64 and aliases are globally unique, stable human handles.
`handle::resolve` prefers an exact alias, then an exact id, then a unique case-insensitive
substring of alias or name. No match is an error; multiple substring matches are an error with
the candidates listed in stderr. Aliases are lowercase ASCII letters/digits/hyphens, at most 32
characters, and cannot be exactly 16 hexadecimal characters.
