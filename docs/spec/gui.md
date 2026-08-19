# GUI

The shape of the graphical client: its window layout, the server browser, and how it stays
adaptive.

The GUI is an `adw::Application` (app id `dev.keepinfov.oxidom`). It wires to the core modules and
does not reimplement parsing/Xray logic in the UI layer.

Layout (from the mockups + Nautilus feel; dark, rounded, generous spacing):
- **`adw::NavigationSplitView`** (or `OverlaySplitView`) for the adaptive sidebar.
  - **Sidebar:** app/logo area at top; a nav list — at least "Servers" (the server browser) plus
    entries for Subscriptions, Settings, Logs; a bottom action row (e.g. connection status /
    quick connect). Collapses in narrow mode with a small toggle button in the header.
  - **Content:** `adw::HeaderBar` with standard window controls; a **search entry** spanning the
    top that filters servers by name/protocol/country.
- **Latency controls are two-state.** The card's check button and the subscription header's
  sweep button each show whether pressing them starts a check or stops one, and they switch on
  press rather than on the daemon's acknowledgement — on a full queue that answer is seconds
  away, and a control that waits for it reads as one that missed the press. The card's button
  lives in the region a collapsed card hides, so the context menu borrows the same button and
  therefore shows the same label. Stopping is not a failure and raises no error. Neither
  control offers a stop unless the daemon answered `CancelProbes` at startup: a button that
  says it will stop something and then cannot is worse than the second press being ignored.
- **Server card** (custom widget): country **flag**, server **name**, protocol **subtitle**
  (`transport_label`, e.g. "vless + xhttp + reality"), optional **latency badge** (green when
  low). Whole card is a click target → selects that server. Every server carried by a connected
  profile is visually marked; the one-profile case is unchanged.
- **Groups:** a chip row above the grid — `All`, one chip per saved group, `+`. A group narrows
  **the one list**, and is never a second block of cards: rendering it as its own block
  shows the same server two or three times and leaves no way to tell which card is real, and
  "show it only in its highest-priority group" makes a starred server vanish from its
  subscription. Cards stay in their subscription; the chip narrows what is shown. Selecting a
  chip reveals a Connect bar that **runs the visible selection immediately**: no profile is read,
  written or confirmed, and the session is the daemon's `default` — the same one a bare `Connect`
  on a single server uses, over `ConnectPool`, its pool counterpart. Pressing it again stops the
  session running that selection, matched on the server ids rather than on the query, so a saved
  group and the same servers arrived at by hand are one session. Saving stays explicit and stays
  separate: **New profile from this…**. Repointing a saved profile still rewrites a file and still
  asks first, which is what connecting a *profile* means. Favourites is a built-in list; the card's star is what fills it, and it
  cannot be deleted because the star would have nowhere to put things.
- **Server grid:** a top block of "loose"/favorite servers, then one **block per subscription**:
  each block shows its **title** + **description** (name + quota/expiry from userinfo) followed by
  that subscription's server cards. In the code a block is `SubscriptionBlock` — never a "group",
  which the window uses only for the saved selections in the chip row. Multi-column grid in wide mode, single column in narrow
  (use `adw::WrapBox`/`FlowBox` with a breakpoint via `adw::Breakpoint`).
- **Connect control:** a single primary **Connect/Disconnect** toggle in the header for the
  compatibility `default` session. It shows its live status (Connecting/Connected/Error) and
  active latency. If other sessions run, a persistent banner reports their count and points to
  `oxidom status`.
- **Subscriptions view:** add (URL + optional name), update-now, delete; per-sub **"send HWID"**
  switch (default OFF) with a privacy hint.
- **Settings view:** ports, system-proxy toggle, latency method + test URL. The Xray core group also
  reports whether the core can load its geo data and offers to install it. What the rows offer is a
  pure decision (`reduce::geo_offer`), not a widget-level one, because the awkward cases are the
  point: a daemon that predates the download, or one that cannot write its own asset directory,
  must be given a copyable command rather than a button that fails when pressed. A **system**
  daemon too old to install gets no download button at all — it runs as `oxidom` with
  `ProtectHome=true`, so nothing this process writes to the user's home could ever be read by it.
  The confirmation names the release host and says it may be blocked, and offers to fetch through
  a tunnel that is already up. Progress is polled off the status tick, only while a download runs.
- **Logs view:** the process log book, filtered by source (all / oxidom / Xray / interface),
  minimum severity, and a text search. Records arrive by cursor (`LogsSince`), so a refresh
  appends and **never rebuilds** — rebuilding is what threw the reader back to the top of the log.
  A rebuild happens only on a filter change, a clear, or a daemon restart. The buffer is trimmed
  whenever it has outgrown what one text view should lay out, and **never past the first visible
  line** — so nothing being read is ever inside the deleted range. Trimming does not move the
  reading position: the height removed from above the viewport is measured before the delete and
  taken off the scroll offset after it, which is what leaves a reader who has scrolled up alone.
  That promise is about the position, not about following; gating the trim on following instead
  bounded the buffer only for the reader sitting at the bottom, who needed it least. Lines the
  daemon could not hand over are announced in place, not silently dropped.

Responsiveness: a breakpoint (~700px) collapses the split view and switches the grid to a single
column, exposing the small sidebar toggle button (as annotated in the narrow mockup).
