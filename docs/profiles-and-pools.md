# Profiles and pools

A **profile** is persistent configuration: which server to use, on which ports,
with or without a network interface. A **session** is the running instance of one
profile. Several profiles can be up at once, each on its own loopback address —
including several profiles pointing at the same server.

Subscriptions and servers are global. Profiles only *select* from them.

## Contents

- [The file](#the-file)
- [Naming](#naming)
- [`[interface]`](#interface)
- [`[core]`](#core)
- [Pools](#pools)
- [Lists and rules](#lists-and-rules)
- [Strategies](#strategies)
- [Groups in the GUI](#groups-in-the-gui)
- [systemd](#systemd)
- [Validation errors](#validation-errors)

## The file

`~/.config/oxidom/profiles/<name>.toml` (or `/var/lib/oxidom/profiles/` under the
system daemon — see [configuration.md](configuration.md#the-two-databases)).

```toml
description = "work"

[select]
server = "ch-trojan"

[proxy]
socks_port = 10808
http_port  = 10809

[interface]
enable = true
routes = "manual"

[core.fragment]
enabled = true
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `description` | string | `""` | Free text, shown in listings. |
| `select.server` | string | `""` | An alias, a server id, or a unique name substring. |
| `select.pool` | table | — | A pool instead of a single server. [Below](#pools). Mutually exclusive with `select.server`. |
| `proxy.socks_port` | `u16` | `10808` | This profile's SOCKS inbound. |
| `proxy.http_port` | `u16` | `10809` | This profile's HTTP inbound. |
| `interface.*` | table | disabled | [Below](#interface). |
| `core.*` | table | inherited | Overrides the machine-wide Xray core settings. [Below](#core). |

A freshly created profile has `server = ""` and no pool. That is valid to store —
only `oxidom up` refuses it, with `profile "x" does not name a server yet`.

Every profile binds to its own loopback address, so two sessions can both use port
10808 without colliding. `default` is pinned to `127.0.0.1` forever; other profiles
get a stable hashed `127.x.y.1`. Use `oxidom env <profile>` rather than assuming.

Edit profiles with `oxidom profile edit <name>` — it validates before saving, so a
profile that could not come up is rejected at the editor rather than at connect
time.

## Naming

1 to 32 characters, `[a-z0-9_-]`, starting with a letter or digit. They double as
systemd instance names (`oxidom@work.service`), which is where the constraint comes
from.

Command names and aliases are [reserved](cli.md#two-ways-to-name-a-profile) so that
`oxidom <profile> <verb>` is never ambiguous. An existing file with a reserved name
stays readable and listed, with a warning, but new ones are refused.

`default` is special everywhere: it is the session behind `oxidom connect`, it owns
`127.0.0.1`, and it is seeded once on first start and never rewritten afterwards.

## `[interface]`

Gives the profile a TUN device. This is the only part of oxidom that needs
privilege, and it is available **only from the system daemon**, which must hold
`CAP_NET_ADMIN`. oxidom never escalates on its own. See [routing.md](routing.md).

```toml
[interface]
enable  = false
device  = ""          # empty derives "oxi-<profile>"
address = ""          # empty derives a stable address in 198.18.0.0/16
mtu     = 0           # 0 selects 1500
routes  = "manual"    # "manual" | "list" | "default"
list    = []          # IPv4 CIDRs, used only by routes = "list"
```

| Key | Default | Notes |
|---|---|---|
| `enable` | `false` | `false` is exactly the unprivileged, proxy-only behaviour. |
| `device` | `oxi-<profile>` | Must fit Linux's 15-byte interface name limit; a long profile name therefore requires an explicit `device`. |
| `address` | derived | `default` is fixed at `198.18.0.1`. `/32`, so it adds no connected route. |
| `mtu` | `1500` | `0`, or 576–65535. |
| `routes` | `"manual"` | **`manual` captures nothing by itself** — see [routing.md](routing.md#what-routes-actually-does). |
| `list` | `[]` | Required to be non-empty when `routes = "list"`. |

## `[core]`

The advanced Xray settings — fragmentation, noises, mux, sniffing, DNS,
`domainStrategy`, log level — are documented once in
[configuration.md](configuration.md#advanced-core-settings). A profile's `[core]`
is the same table, and it overrides the machine-wide one **field by field**:

```toml
[core.fragment]
enabled = true          # this profile fragments; every other profile does not
```

Mentioning one field leaves the rest inherited, so the snippet above does not
reset the log level or the DNS server set in `config.toml`. The profile editor
works in the same units: a section is switched on — the profile owns it — or
left off, in which case the row says what it inherits instead of standing blank.

To see the merged result and which level decided each value:

```console
$ oxidom core show <profile>
```

Two consequences worth stating plainly:

- A **pool** applies `[core.mux]` and the fragmenter to every member, because in
  Xray both are properties of an outbound rather than of the config.
- Latency **probes** use the machine-wide `[core]` only — a probe belongs to a
  server, not to a profile. So a server that connects only because of a
  profile's fragmentation will still measure as unreachable in the list. If it is
  the machine that needs fragmentation, set it in `config.toml`.

## Pools

A pool selects *several* servers and lets the core balance across them. The point
is to spread activity over exit addresses and keep working when one node dies.

```toml
[select.pool]
name           = "Europe"
strategy       = "leastLoad"
subscriptions  = ["main"]
countries      = ["ch", "de", "nl"]
protocols      = ["vless", "trojan"]
exclude        = ["ch-trojan-3"]
max            = 8
probe_interval = "5m"
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | `""` | A label. Takes **no part** in selection — renaming a pool never makes a running session stale. |
| `strategy` | enum | `"leastLoad"` | [Below](#strategies). |
| `members` | list | `[]` | An explicit list of aliases/ids. Non-empty makes this a *list* pool and forbids the filters. |
| `subscriptions` | list | `[]` | Match a subscription id exactly, or its name case-insensitively. Empty means every group, including `My servers`. |
| `countries` | list | `[]` | ISO 3166 alpha-2, case-insensitive. |
| `protocols` | list | `[]` | `vless`, `trojan`, … |
| `exclude` | list | `[]` | Alias or id, matched **exactly** — a substring here would silently drop half a pool. |
| `max` | int | `0` | Cap on members. `0` is uncapped. At most 64. |
| `expected` | int | `0` | How many nodes to keep in rotation. Only `leastLoad` reads it. `0` means all. |
| `probe_interval` | string | `"5m"` | How often the core re-measures members. `30s`, `5m`, `1h`. |

A pool session has **no active server**. `oxidom status` reports its live exit
from the core's own observatory rather than naming the first member — because the
first member is not the exit, it is just first.

When `max` truncates a pool, members are first dealt one per distinct endpoint, so
`max = 8` cannot come back as eight spellings of the same host. `oxidom up` warns
when a pool's members collapse onto fewer real endpoints than it looks like.

Servers that are themselves balanced Xray profiles are never pool members — such a
server is already a balancer.

## Lists and rules

A pool is made of **either** an explicit list **or** a rule, never both. Setting
both is rejected rather than silently half-applied.

The difference is the user's, not an implementation detail:

- A **rule** (the filters) *grows*. A server added by tomorrow's subscription
  refresh joins on its own. You cannot look at a rule and count it.
- A **list** (`members`) can be counted and never gains a member without being
  edited. Losing one just means a server went away.

Freezing a list as "no filters, plus everything else excluded" looks equivalent and
is not: a server that did not exist when you froze it is in nobody's exclusions, so
it would silently join.

## Strategies

| Strategy | Behaviour |
|---|---|
| `leastLoad` | **Default.** Keeps `expected` nodes in rotation by measured health. The point of a pool is to spread across exits *and keep working*. |
| `roundRobin` | Cycles all members. Measured on Xray 26.3.27 to keep unreachable nodes in the rotation. |
| `random` | Picks at random. |
| `leastPing` | Concentrates every connection on the single fastest node — which is the opposite of spreading. |

## Groups in the GUI

A **group** in the GUI is a saved pool query under a name. Connecting to one writes
it straight into `select.pool`; the daemon never learns a second noun for it.

Because a group *is* a filter, group membership is edited where the servers are —
on the Servers page, in the one Selection dialog the `Filter` pill, `New group` and
`⋮ → Edit…` all open. The profile editor reports the pool and edits only
`strategy`, `max`, `expected` and `probe_interval`, carrying *which* servers
through untouched. Two independent editors for one thing is how a saved profile
comes to disagree with the group it was made from.

The window calls this a **group** everywhere. `pool` is the word in the profile
file, in the CLI and in `oxidom status`, and one line in the profile editor says
so — the two are the same thing seen from either side.

The Connect bar carries a rotation width (default 6), written into `expected`. It
is deliberately not also stored on the group: a group answers "which servers", the
width answers "how many at once, this run".

## systemd

The template unit runs a profile as a boot-managed oneshot:

```sh
sudo systemctl start oxidom@work
sudo systemctl enable oxidom@work
```

On NixOS the template ships without an `[Install]` section, so enabling an instance
is declarative:

```nix
systemd.services."oxidom@work".wantedBy = [ "multi-user.target" ];
```

Ports pinned on the daemon's command line constrain only `default`; other profiles
keep their own.

## Validation errors

All of these are reported when the profile is saved, not when it fails to connect:

| Message | Cause |
|---|---|
| `a profile selects either a server or a pool, not both` | `select.server` and `[select.pool]` both set |
| `[select.pool] lists members, so it cannot also filter by …` | a list pool with filters |
| `[select.pool] members must name at most 64 servers` | over `MAX_POOL_MEMBERS` |
| `max must be 0 (unlimited) or at most 64` | — |
| `[select.pool] probe_interval must be empty or a duration such as "30s", "5m", or "1h"` | — |
| `profile ports must be between 1 and 65535` | — |
| `the profile's SOCKS and HTTP inbounds cannot share a port` | — |
| `interface routes require [interface] enable = true` | — |
| `[interface] routes = "list" requires at least one CIDR in [interface] list` | — |

And at connect time:

| Message | Cause |
|---|---|
| `profile "x" is already up; run 'oxidom down x' first` | one session per profile |
| `profile "x" does not name a server yet; set select.server to an alias or id` | empty selection |
| `"…" matches N servers (…); use an alias or an id` | ambiguous substring |
| `no server matches the pool query (countries: ch; protocols: vless)` | a rule that resolves to nothing |
| `the system proxy is already held by profile "y"` | only one session may own it |

---

Next: [routing.md](routing.md) · [cli.md](cli.md) · [troubleshooting.md](troubleshooting.md)
