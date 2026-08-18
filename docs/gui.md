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
| **Logs** | The Xray core, the interface helper and oxidom itself, filterable by source and severity. |

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

### Latency

Each card carries a badge. It is deliberately small, so its whole vocabulary is
here:

| Badge | Means |
|---|---|
| `84 ms` | A direct measurement. Dimmed, with the age in its tooltip, once it is over a minute old. |
| `84 ms`, tunnel colour | Measured **through the running tunnel**, not around it. Only the server the tunnel is carrying can read this way. |
| spinner | Being checked, or waiting for a free slot — eight run at once. |
| `—` | No number. The tooltip says which kind of nothing: never measured, measured in a context that no longer applies, or the server did not answer. Only the last is drawn in the error colour. |
| `⊘` | The check never ran. This machine's problem, not the server's — no network, or no core to measure with. The tooltip names the condition when the daemon knows it. |

A dash rather than a cross for an unreachable server is on purpose: a cross reads
as a verdict, and a failed check leaves no number in exactly the way an unmeasured
server does. The same reasoning keeps `⊘` in the amber "this machine" colour
rather than the red one — see [spec/latency.md](spec/latency.md).

The tooltip on a number also names the method actually used, which is not always
the one configured: a TCP check of a Hysteria2 server falls back to ICMP, because
Hysteria2 is QUIC over UDP and has no TCP port to open.

Two controls start a check. The card's own ⟳ **Re-check latency** sits in the
expanded card and in its right-click menu; the subscription header's ⚡ **Check
latency of all servers** sweeps the whole block. Both are fire-and-forget —
**there is no way to stop a check once it has started**, and closing the window
does not stop it either. A sweep of a large subscription can run for minutes.

The method itself is chosen in Settings, once, for the whole application.

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

### A certificate you have to decide about

Xray 26 removed the setting that skipped certificate verification, so a server
with a self-signed certificate cannot connect on its own — links that still
carry `allowInsecure=1` get nothing from it. The first time such a connection
fails, oxidom shows what the server presented and asks:

> **Trust this certificate?** … SHA-256 97:89:79:cf:… Accepting pins this one
> certificate for this server. Any other certificate, including a replacement,
> will be refused until you look again.

Accepting stores the fingerprint and reconnects. It is asked once per server,
survives subscription refreshes, and can also be done from the command line with
[`oxidom trust`](cli.md#oxidom-trust-handle---trust) — where you can compare the
fingerprint against the server's own certificate first.

If a server that is already pinned fails the same way, the dialog does not
reappear: pinning did not fix it, and asking again in a loop would not either.

The same dialog is on a server card's context menu as **Trust certificate…**,
for deciding before anything fails — and for a server whose certificate has
since changed. It appears only for servers using ordinary TLS: REALITY
authenticates by public key rather than by a certificate chain, and a plain
protocol presents none at all.

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
| **Geo data** | Whether the core can load `geoip.dat` and `geosite.dat`. Decided by asking the core, not by looking for the files — which is also how a corrupt list is told from a missing one. Silent until the daemon has an answer, so a daemon older than the check accuses nothing. |
| **Install the geo data** | Shown only when the core cannot load them. Looks for a copy already on this machine first and offers to use it; otherwise offers a download, behind a confirmation naming both URLs and both destination paths. A progress bar reports the transfer and **Cancel** stops it. Where the daemon is too old to install anything, the row carries the manual commands instead of a button that could not help. |

## Logs

Three programs report here, and every line says which one it was:

| Source | What it is |
|---|---|
| **oxidom** | What the app itself decided, and why — including the reasons a connection was refused before the core ever started. |
| **Xray** | What the core printed. Its own severity and subsystem are read out of the line. |
| **Interface** | The `tun2socks` helper. Kept separate from the core because an interface that never came up and a core that refused its config are different problems. |

Narrow the stream with the source switcher, the severity list and the search box;
under a narrow window the last two fold into a menu. **Copy** and **Save** both
take what is on screen, filters included. **Clear** empties the view and the
daemon's buffer.

The view follows new output only while you are at the bottom. Scroll up and it
stops, so lines arriving below never move what you are reading; the button at the
bottom right returns you to live. If output arrives faster than it can be
collected, the missing count is stated in place rather than passed over.

Raise what the core itself reports with **Settings › Core behaviour › Log level**.
Raise what oxidom reports about itself with `RUST_LOG`, or `oxidom-gui --debug`.

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
