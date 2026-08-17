# The graphical client

`oxidom-gui` is a GTK4 / libadwaita application. It is a **thin client** — the
daemon owns the tunnel, so closing the window does not disconnect anything, and
everything here can also be done from the [CLI](cli.md).

```sh
oxidom-gui
oxidom-gui --background      # start hidden, for autostart
oxidom-gui --debug           # stay in the foreground, debug logging
```

Started from a terminal, the GUI forks into the background so closing that
terminal does not take the window and tray with it. `--debug` keeps it attached.
Nothing is detached when stdout is not a terminal, so `oxidom-gui | tee` and the
tray unit behave normally.

## The five pages

| Page | What it is for |
|---|---|
| **Servers** | The server browser: cards, search, filters, groups, and the Connect bar. |
| **Profiles** | Named connections, one row per profile; run them, edit them, set their interface. |
| **Subscriptions** | Add, refresh, reorder and remove subscriptions; quota and expiry. |
| **Settings** | Ports, system proxy, reconnect, probe method, subscription User-Agent, core paths. |
| **Logs** | Recent Xray core output, kept in memory. |

The sidebar collapses under a width breakpoint, so the window is usable narrow.

## Servers

A multi-column card grid with country flags. A card carries the server's name, a
transport summary (`vless + xhttp + reality`), and its latency once measured;
clicking one opens its details inline rather than in a dialog.

Above the grid is a **chip row** — a segmented control over one list, not several
lists. Its head is the `Filter` pill; the rest are your saved groups plus
`Favourites`.

### Filters and groups

There is one dialog behind all of them, and a name is the only difference between
a filter and a group.

It holds an optional **Name** and **Icon**, then **Country**, **Protocol** and
**Subscription** expanders with a checkbox per choice, then **Except** for
excluding individual servers (a search, because a provider may have two hundred
nodes), then a list for picking servers by hand. `Apply` shows the selection
without saving it; `Save` needs a name. The `Filter` pill, `New group` and
`⋮ → Edit…` all open this dialog — they differ only in what it opens with.

You are never asked whether you are making a list or a rule. Servers picked by
hand are frozen, because picking them is what freezing means; a selection made
only of filters keeps matching, because that is all a filter can do. The line at
the bottom of the dialog says which one you have made and what it will do next
week.

A **group is a saved filter**. That is the whole idea: connecting to a group writes
the filter straight into a profile's [pool](profiles-and-pools.md#pools), so the
daemon never learns a second concept.

The `⋮` menu always acts on **the selection currently on screen**, not on some
previously selected group. `New profile from this…` lives there and works on an
unsaved filter too, since it needs no name for the selection. Group-only items
(`Edit…`, `Update to what's shown`, `Move left/right`, `Delete`) are simply absent
when no group is selected, rather than present and dead.

### Connecting

The Connect bar carries a rotation width — how many of a group's servers stay in
rotation at once, default 6. It is deliberately not stored on the group: a group answers
*which* servers, the width answers *how many at once, this run*. Changing it
rewrites without a dialog and says so in a toast.

Per-subscription latency checks and sorting live on this page too.

## Profiles

One row per profile — the same `profiles/*.toml` the CLI and systemd use, running
or not. A row answers one question, is this up and where, and folds the rest away.
Selecting a profile makes the header, the cards and the tray follow it, so
"connected" always means *this* profile.

A profile connected to a group shows its live exit rather than naming the group's
first server, because the first server is not the exit.

Per-profile interface settings are edited from here.

A profile that is up is a **session**; that is the word the CLI, systemd and the
logs use for it.

## Subscriptions

Add by URL, refresh, reorder, collapse, remove. Quota and expiry appear when the
provider sends them.

Two per-subscription settings worth knowing:

- **Send HWID** — off by default, and off is the whole point. See
  [subscriptions-and-protocols.md](subscriptions-and-protocols.md#privacy-and-hwid).
- **Fetching › User-Agent override** — for panels that choose the *response
  format* from the client string, so one provider can need a different value
  than the rest. Empty inherits **Settings › Advanced › Client preset**; the new
  value applies on the next **Update**. See
  [subscriptions-and-protocols.md](subscriptions-and-protocols.md#the-user-agent-decides-the-format).

**Ctrl+V** anywhere outside a text field takes whatever is on the clipboard and
opens the dialog it belongs to, filled in: a subscription URL opens *Add
subscription*, one or more share links open *Import server*. Opening either
dialog by hand does the same for an empty field, so a link copied a moment ago
is already there. Nothing importable on the clipboard is said in a toast rather
than ignored.

Locally pasted share links live in a built-in subscription called **My servers**.

Removing a subscription disconnects any session using one of its servers, and a
refresh that drops the active server disconnects that session rather than silently
repointing it.

## Settings

| Row | Notes |
|---|---|
| **Appearance** | Follow the system, Light, or Dark. Applies as you pick it — there is nothing to Apply, because it belongs to this window rather than to the daemon. A choice other than "follow the system" overrides the desktop, which is the point on a desktop that offers no such setting. Stored in `gui_prefs.toml`. |
| **Local proxy** — SOCKS and HTTP ports | Locked, with an explanation, when the daemon's unit pinned them. |
| **System proxy** | "Send the whole desktop's traffic through oxidom while connected (GNOME)". See [routing.md](routing.md#gnome-system-proxy). |
| **Reconnect automatically** | "Reconnect only when Xray exits unexpectedly, never after Disconnect". |
| **Latency method** | `icmp`, `tcp`, `http_head`, `http_get`. |
| **Latency test URL** | Target for the HTTP methods. |
| **Subscription User-Agent** | Free text, plus a **Client preset** list that fills it. |
| **Advanced › Xray / tun2socks / nft binary** | Ignored by a **system** daemon on purpose — setting a binary path on a privileged daemon would be a remote-execution primitive. |
| **In use by the daemon** | What the daemon actually resolved, which is not always what you typed. |

## Tray

The tray icon needs a **StatusNotifierItem host** on the session bus. On stock
GNOME that means an AppIndicator extension; without one, the window still works
normally — there is simply no icon.

`--background` starts the GUI hidden so the tray and the session daemon exist
before you open a window. Activating the application again presents it. On NixOS:

```nix
programs.oxidom.trayAutostart = true;
```

## Notes

- The GUI runs **fully unprivileged**. TUN interfaces need the system daemon; the
  GUI can configure them, but cannot grant the privilege.
- The GUI owns the GNOME system proxy toggle, not the daemon — it is a per-desktop
  user setting. A proxy left behind by a killed GUI is repaired on the next start.
- If the daemon cannot be reached at all, the GUI says so in a dialog rather than
  showing an empty window.

---

Next: [quickstart.md](quickstart.md) · [profiles-and-pools.md](profiles-and-pools.md) · [troubleshooting.md](troubleshooting.md)
