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
- **Server card** (custom widget): country **flag**, server **name**, protocol **subtitle**
  (`transport_label`, e.g. "vless + xhttp + reality"), optional **latency badge** (green when
  low). Whole card is a click target → selects that server. Every server carried by a connected
  profile is visually marked; the one-profile case is unchanged.
- **Groups:** a chip row above the grid — `All`, one chip per saved group, `+`. A group narrows
  **the one list**, and is never a second block of cards: rendering it as its own block
  shows the same server two or three times and leaves no way to tell which card is real, and
  "show it only in its highest-priority group" makes a starred server vanish from its
  subscription. Cards stay in their subscription; the chip narrows what is shown. Selecting a
  chip reveals a Connect bar that points the **selected profile** at that group — the same rule a
  card click follows. Favourites is a built-in list; the card's star is what fills it, and it
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
- **Settings view:** ports, system-proxy toggle, latency method + test URL.
- **Logs view:** the core's ring-buffer output.

Responsiveness: a breakpoint (~700px) collapses the split view and switches the grid to a single
column, exposing the small sidebar toggle button (as annotated in the narrow mockup).
