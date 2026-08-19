# Profiles, pools and sessions

What a profile file holds, how a pool selects and ranks its members, and what a running session
is.

## Profiles (binding)

Profiles live in `profiles/<name>.toml` below the daemon's config directory:

```toml
description = "work"

[select]
server = "ch-trojan"

[proxy]
socks_port = 10808
http_port = 10809

[interface]
enable = true
routes = "manual"
```

Names match `^[a-z0-9][a-z0-9_-]{0,31}$`; command names and aliases are reserved so profile-first
argv is never ambiguous. Existing reserved-name files remain readable/listed with a warning, but
new writes are refused. `UpProfile` resolves the profile's selection and applies that profile's
ports; unit-pinned ports constrain only `default`. Removing a profile deliberately leaves its
running session intact so the unit can still stop what it started.

A profile selects **either** one server **or** a pool, never both:

```toml
[select.pool]
name           = "Europe"         # label only; never selects anything
strategy       = "leastLoad"      # leastLoad | roundRobin | random | leastPing
members        = ["ch-one", "de-two"]   # a *list*; mutually exclusive with the filters below
subscriptions  = ["main"]         # empty = every group, including "My servers"
countries      = ["ch", "de", "nl"]
protocols      = ["vless", "trojan"]
exclude        = ["ch-trojan-3"]
max            = 8                # 0 = uncapped
probe_interval = "5m"
```

A pool is made either of a **list** (`members` non-empty, `PoolKind::List`) or of a **rule**
(the filters, `PoolKind::Rule`). The distinction is the user's, not an implementation detail: a
rule cannot be looked at and it *grows* — a server added by tomorrow's refresh joins on its own —
while a list can be counted and never gains a member without being edited. Losing one is
expected and is just a server going away. Freezing a list as "no filters plus everything else
excluded" looks equivalent and is not: a server that did not exist when the list was frozen is
in nobody's exclusions, so it would silently join. `Profile::validate` **rejects** a pool that
sets both, rather than letting the file claim one thing and the tunnel do another; `resolve`, the
pure function underneath, lets the list win so a config that slipped through cannot half-apply.

`name` is carried into `SelectionInfo` and printed by `oxidom status` (`pool "Europe" (…)`). It
takes no part in selection, and `engine::pool_fingerprint` hashes resolved members, so renaming a
pool never makes a running session stale. Both `name` and `members` are `skip_serializing_if`
empty, so every pool profile written before lists existed still round-trips to the same bytes.

The same `PoolQuery` drives the balancer and the GUI's server filter: a filter is a pool
constructor, not a second search. A **group** in the GUI is a saved `PoolQuery` under a name, so
connecting one writes it straight into `select.pool` — the daemon never learns a new noun. Group
membership is therefore edited only where the servers are, in the Selection dialog on the Servers
page; the profile editor reports the pool and edits only `strategy`, `max`, `expected` and
`probe_interval`, carrying everything about *which* servers through untouched. The window says
**group** for all of this; `pool` stays the word in the TOML, the CLI and the IPC payload, and one
line in the profile editor says the two are the same thing.

**One dialog says what a selection is; a name is what saves it.** `present_selection_dialog`
(`SelectionIntent::{Filter, Name, Edit}`) is the only editor: optional `Name` and `Icon` at the
top, then the matching rows — three `AdwExpanderRow`s (Country, Protocol, Subscription) holding a
checkbox per choice, `Except` as an `AdwActionRow` with a picker of its own because it is a search
over a provider's two hundred nodes — then the hand-picked list, then a summary line. `Apply`
shows the selection without saving it, `Save` needs a name. The `Filter` pill, `New group` and
`⋮ → Edit…` all open this one dialog, differing only in what it opens with.

**`GroupKind` is derived, never asked.** Naming servers by hand is what freezing them means, so a
draft with members saves as `List`; a rule with no members keeps matching, so an empty member list
saves as `Rule`. The List/Rule radio that used to ask this was the user classifying their own
selection in storage vocabulary before they had made it, and it could only tell the truth by
disabling itself. What it conveyed that way is now stated: a selection named while the search box
is non-empty freezes into a list (search has no rule equivalent), and `Save` refuses a nameless-
rule-with-no-fields, which would mean every server. Favourites is `list_only`: the star writes
members, and a Favourites with filters is a pool `Profile::validate` rejects.

**`⋮` acts on the selection on screen, not on the selected group.** It is never insensitive.
`New profile from this…` lives there — it is the only way to make a *new* profile from the visible
selection, and it needs no name for that selection, so it works on an unsaved filter too. The
group-only items (`Edit…`, `Update to what's shown`, `Move left/right`, `Delete`) are simply
absent when no group is selected, rather than present and dead.

**A group stores selection; the Connect bar states rotation width.** The bar carries a rotation
picker defaulting to `pool::DEFAULT_POOL_ROTATION` (6), and `connect_query` writes it into
`expected`. It is deliberately *not* also stored on the group: a group answers "which servers",
the width answers "how many at once for this run", and a second copy is how the two come to
disagree. Consequently `same_pool` compares selection only while `same_rotation` compares
`expected`, and a changed width yields `PoolAction::RetuneAndUp` — neither a no-op (which would
drop the width just chosen) nor `RepointAndUp` (which would ask the user to confirm replacing a
pool with itself). It rewrites without a dialog and says so in a toast. `pool_for_profile` takes
`expected` from what the bar chose and still carries `max` and `probe_interval` through from the
saved profile, because nothing outside the profile editor can express those two.
The default exists because `expected = 0` means "all", and a country-wide pool is mostly repeats
of a handful of hosts: rotating over all 42 buys no more spread than its 9 distinct addresses
while costing an observatory ping per entry.
Two blind editors for one thing is how a saved profile comes to disagree with the group it was
made from. `leastLoad` is the default because the point of a pool is to
spread activity across exit IPs *and keep working*; `roundRobin` was measured on Xray 26.3.27 to
keep unreachable nodes in the rotation, and `leastPing` concentrates every connection on one node.
`server = ""` with `[select.pool]` absent stays valid — that is a freshly created profile, and
only `UpProfile` refuses it.

`pool.resolve` is pure and its order is deterministic because the resolved list becomes both the
config's outbound tags and the session's stored membership: a rule follows subscription order then
server order within a group; a list follows the order the user arranged, which is why `max`
truncating it is still meaningful. `subscriptions` match a group id exactly or a group name
case-insensitively; `members` and `exclude` match an alias or id **exactly** — substring matching
there would silently drop half a pool or enrol a server nobody chose. A handle listed twice
(alias and id) yields one outbound, or the `s-<handle>` tags would collide. Servers whose spec is
`OutboundSpec::XrayProfile` are never pool members: such a server is itself a balancer.

`resolve` stays silent about what it dropped because the GUI calls it on every keystroke. Two
companions report it once, at `up`, where a user can act: `excluded_composites` for balancer
servers that cannot become outbounds, and `missing_members` for handles a list names that no
subscription holds any more. Neither is fatal — only an empty result is.

**Activation resolves through `pool.resolve_ranked`, not `resolve`** (binding). Membership is
identical — a test pins that — but the order is not, and two jobs ride on the order that
subscription order does badly:

- **A pool spreads over exit addresses, not over subscription entries.** Providers list one host
  many times; in one observed provider list, 26 of 42 German entries shared `31.12.75.21:2087`
  and the whole set covered 9 addresses. `max` cutting "the first 6" therefore bought six
  spellings of one exit IP. `resolve_ranked` groups candidates by `address:port` and deals the
  groups out one apiece before any group gets a second, so a capped pool spends its budget on
  different hosts. `distinct_endpoints` is the honest count, and `up` warns when it is below the
  member count.
- **The first member is the pool's opening exit**, per the observatory note in
  [xray-config.md](xray-config.md#xray-config-generation). Groups are
  ordered by the best `pool::Known` in each, so a node that last answered opens the pool.
  `known_state` maps the daemon's direct readings onto that; a `Proxied` reading describes a
  tunnel rather than a server, and `NoNetwork`/`Unknown` are this machine's failure, so both rank
  as `Unmeasured` rather than as a verdict on somebody's node. Measured end to end: the same
  41-node German rule took ~25 s to confirm with nothing measured and **70 ms** once two of its
  nodes had been probed.

Ranking never changes who is in the pool, so a list still gets everyone the user named; losing a
named member stays `missing_members`' job.

**The exit count is reported, not only logged.** `SelectionInfo.endpoints` carries it to every
surface, and both `oxidom status` (`… 42 nodes on 9 exits …`) and the Profiles page's `Nodes` row
(`6 of 42 in rotation · 9 exit addresses`) say it — but only when it is *below* the member count,
because on a pool where every node is its own host it is one number printed twice. Zero means an
older daemon did not report it, so nothing renders zero as a count. It is counted inside
`selection_info`, which already looks every member up to name it, rather than snapshotted at `up`:
that loop is the cost, and a `Status` paying it anyway may as well answer the question. Members
that no subscription holds any more contribute no endpoint, so a shrunken pool understates rather
than invents.

**A pool's node count is explained by its strategy, not by its width.** `reduce::rotation_help`
is keyed on the running strategy, deliberately *not* shared with the Connect bar's
`rotation_detail`: that one is one sentence about a width being chosen and describes `leastLoad`,
and `roundRobin` keeps unreachable nodes in the rotation, so the same sentence would be false.
Two facts, two sentences.

Pool membership is resolved **once, at `up`**. A subscription refresh that changes what the
query would match marks the session `stale` and invites a reconnect; it never rewrites the
config under live connections.

Every profile gets a stable `127.<a>.<b>.1` inbound address derived with the same FNV-1a 64 used
for ids; collisions probe forward through the address space. `default` is permanently
`127.0.0.1`, an external contract with consumers such as redsocks. Different addresses let every
profile reuse the same SOCKS/HTTP ports.

## Sessions (binding)

Subscriptions and servers are global sources. A profile is persistent configuration; a session
is the ephemeral running instance of one profile: its Xray process, selection, loopback
address, ports, optional interface and status. “Connect one server” means the `default` session,
not a separate global mode. Several profiles may run at once, including several profiles on the
same server.

A session does not require a profile file. `Connect` runs one server under `default` and reads
`default.toml` only to learn whether an interface was asked for; `ConnectPool` does the same for a
selection, taking the query the interface resolved and writing **nothing** — the profiles directory
is untouched, and connecting a group therefore neither needs a profile nor modifies one. Saving a
selection is a separate act (`SaveProfile`), and repointing a saved profile still rewrites its file.

A session's selection is a single server **or** a pool, and a pool session has no active server:
`Session::server_id()` returns `None` for it, deliberately, so nothing can quietly pick the first
member and call it the exit. Everything that means “the tunnel is carrying server X” — the
connected highlight, the proxied reading, the egress cache — is therefore keyed by session, and
a pool reports its live exit from the observatory instead.

`Sessions` is a `BTreeMap` keyed by profile, so listings are stable. A profile already up cannot
be brought up twice. `DownProfile` removes exactly that runtime; `Down("")` removes all sessions.
The single GNOME system-proxy owner is runtime state in `Sessions`, not persistent configuration:
it is released when its session stops or its core dies. On daemon startup, each recovered Xray
PID is checked against its own `current-config-<profile>.json`, each tun2socks PID must contain
`--device <our-device>`, and recorded interface resources are removed before the state entry is
forgotten.
