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
  low). The **expanded** card additionally draws the recent checks as a chart, with the range on
  its heading and the failures under it, and says beneath that why the last check produced no
  number —
  `docs/spec/latency.md` governs both, and the record is fetched per server rather than polled
  with the grid. Whole card is a click target →
  selects that server. Every server carried by a connected profile is visually marked; the
  one-profile case is unchanged.
  - **An expansion owns the card until it finishes (binding).** The revealed region fades in
    while the card grows, and the two are guarded separately: a fresh measurement — of the
    content, or of a new column width — aims the height animation somewhere else rather than
    replacing it. Guarding both on one generation meant that the pushes which feed an open
    card, arriving in the same main-loop iteration as the click and one poll later, cancelled
    the fade; a fade with no terminal write then left the region at the opacity it was built
    with, and every expanded card in the application was blank while measuring its real
    contents. Every animation that can be superseded writes its end state on completion, and
    every push says whether what it draws changed rather than being re-measured for having
    landed on an open card.
  - **A region that is not drawn takes no clicks.** Opacity alone does not stop a widget being
    targeted, so the detail region becomes targetable when the fade finishes and stops being
    targetable when the card collapses.
  - **The context menu covers the whole card.** The gesture is carried by the box holding the
    header and the detail region, not by the header, which is a button of fixed height and the
    detail region's sibling — a menu carried by the header reaches only the top of an open card
    while holding actions that exist nowhere else at that moment. The popover points at the press.
  - **Selectable text keeps its own menu.** The gesture bubbles rather than captures, so a
    metadata label that is selectable answers a right-click with the text menu and the card menu
    covers everything else. Those labels are selectable so an address or a failure reason can be
    copied out; a card menu taking the press first would remove the only way to do it.
- **Groups:** a chip row above the grid — `All`, one chip per saved group, `+`. A group narrows
  **the one list**, and is never a second block of cards: rendering it as its own block
  shows the same server two or three times and leaves no way to tell which card is real, and
  "show it only in its highest-priority group" makes a starred server vanish from its
  subscription. Cards stay in their subscription; the chip narrows what is shown. Selecting a
  chip reveals a Connect bar that **runs the visible selection immediately**: no profile is read,
  written or confirmed, and the session is the daemon's `default` — the same one a bare `Connect`
  on a single server uses, over `ConnectPool`, its pool counterpart. Pressing it again stops the
  session running that selection, matched on the server ids rather than on the query, so a saved
  group and the same servers arrived at by hand are one session.
  - **The bar names the session (binding).** "Writing nothing" is not "touching nothing":
    `default.toml`'s ports, its interface, its `[core]` and its routing block are the ones that
    apply, and which session runs decides which ports are opened and which routing holds. The
    button says which session it will use, and where the header is showing a different profile the
    bar says on its face — not only in a tooltip — that the shown profile is not the one used.
  - **The header's selection governs the single-server click and not this one.** A card click
    honours the selection and offers to repoint the profile; a group Connect does not read it at
    all. Two adjacent controls reading one selection two ways is allowed, and stated here, but not
    left to be discovered: the header goes on reporting the selected profile's own status, which is
    true, and the bar carries the difference.
  - **A `default` session running a pool is reported as a group, not as another profile.** The
    banner exists to name what is out of sight; a pool raised by the bar on screen is not, and
    `default` is not a profile anybody named. Reported by count, it read as the connection having
    happened somewhere else.

  Saving stays explicit and stays separate: **New profile from this…**. Repointing a saved profile still rewrites a file and still
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
  **An import says what it did not take (binding).** The parser reads outbounds and nothing
  else, so a body carrying routing — `route.rules`/`route.rule_set` in sing-box, `rules`/
  `rule-providers`/`geox-url` in Clash, `routing.rules` in a full Xray config — has all of it
  dropped. That is correct, and staying silent about it is not: silence reads as "there was
  nothing else in the body", which is how the same subscription behaves differently here and
  in another client with nothing to connect the two. `subscription_format::not_taken` counts
  what was recognised and left, `NotTaken::summary` is the single wording, and it is said in
  **two** places — the log line at import, and a **Routing** row beside the quota, which is
  where the question is asked long after the toast would have gone. Kept apart from
  **Skipped**, which is about servers this build could not read: one is a failure to
  understand, the other a deliberate refusal, and merging them would make both unreadable.
  Nothing carried is said as nothing — never "0 rules", which reads as an import that went
  wrong. Whether any of it may ever be *applied*, and whether a provider may choose where
  rule or geo data is fetched from, is a separate question and not answered here.
  - **A server can be typed in (binding).** *Create server* offers the fields per protocol —
    vless, vmess, trojan, shadowsocks, hysteria2 — plus transport and TLS/Reality, and a raw
    JSON field merged into the generated outbound, so a core option the form does not model is
    still reachable. Row titles are the draft's JSON keys, deliberately: the dialog, the CLI
    template and the stored server must read as one thing, and a prettier label would be a
    third name for the same field. Passwords, UUIDs and pre-shared keys are masked with a
    reveal. Validation is `oxidom_core::draft::resolve` — the same validator the daemon runs —
    so the dialog and the daemon reject with one sentence, and the sentence names the field;
    an untouched dialog is incomplete, not wrong, and shows no error. The created server joins
    **My servers**, where pasted links live and no refresh reaches.
- **Settings view:** ports, system-proxy toggle, latency method + test URL. The Xray core group also
  reports whether the core can load its geo data and offers to install it. What the rows offer is a
  pure decision (`reduce::geo_offer`), not a widget-level one, because the awkward cases are the
  point: a daemon that predates the download, or one that cannot write its own asset directory,
  must be given a copyable command rather than a button that fails when pressed. A **system**
  daemon too old to install gets no download button at all — it runs as `oxidom` with
  `ProtectHome=true`, so nothing this process writes to the user's home could ever be read by it.
  The confirmation names the release host and says it may be blocked, and offers to fetch through
  a tunnel that is already up. Progress is polled off the status tick, only while a download runs.
  **Where the lists come from is a setting** (`geoip_url`, `geosite_url`), because the published
  lists differ in what they cover and a regional one decides whether routing works at all in some
  countries. Three sources are offered by name and any address is accepted; the two lists are
  chosen separately. Two rules are binding and belong to `oxidom_core::xray::assets` rather than
  to the window: the address must be **`https`**, refused before anything is fetched — the list
  and the digest that vouches for it travel the same connection, so plain HTTP would let whoever
  sits between the two machines rewrite both and the check would still pass — and the digest is
  always the `.sha256sum` published beside the file named, so a source offering none is **refused
  rather than installed unverified**. Because the source is a setting, the confirmation is
  `reduce::geo_download_prompt` rather than a format string at the call site: it names the host
  actually configured, and it quotes the file sizes only for the built-in pair, which is the one
  case where they are known. The manual recipe for a daemon that cannot install quotes the
  configured addresses for the same reason.
- **Primary menu and About:** one menu button on the right of the header, outermost so that it
  does not move when a page's own menus appear beside it. It is the only header control that is
  not about the current view, and it carries Quit and About.
  The About dialog is where the interface says **which versions are running**, which is three
  programs and not one: this window, the daemon it is talking to, and the core the daemon
  resolved. The daemon's and the core's arrive over `RuntimeInfo` (`daemon_version` and
  `core_version`, both `Option<String>` under the struct's existing `serde(default)`, so a daemon
  that predates them still parses and is reported as unknown rather than blank). The core's is
  read by running `xray version` once and caching it against the binary that answered, for the
  same reason the geo verdict is cached: `RuntimeInfo` is fetched every time Settings opens.
  Where the daemon is **not this build**, the dialog says so in a sentence rather than leaving the
  user to infer it from a control that is missing — which is the symptom version skew produces
  today, because every `supports_*` check answers "too old" by hiding something. A daemon that
  cannot name its version at all is older than the release that added the field, so silence is
  read as "older", not as "unknown". Matching versions draw no sentence: a dialog that warns every
  time it opens is one whose warnings stop being read.
  The block the bug form asks for — version, install method, which daemon, core, distribution and
  desktop — is the dialog's own debug information page, which libadwaita already gives a Copy and
  a Save button. Assembling it is `oxidom_core::versions`, not the window, so that anything else
  needing the same header (a problem report, the CLI) produces the same bytes. The install method
  is judged from the path the binary was started as and the two variables the sandboxed formats
  set; `.deb`, `.rpm` and the AUR are indistinguishable once installed and are reported as one
  answer rather than as a guess between three.
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
- **A problem report is assembled here (binding).** One action turns the **selected** lines —
  or, when nothing is selected, everything visible — into a report carrying the same version
  block the About dialog shows (`oxidom_core::versions`), what the connection is made of, the
  subscription User-Agent, and those lines with everything identifying removed.
  - **Removing is `oxidom_core::redact`, never the window.** The CLI must produce the same
    report from the same rules, and a rule that lived in a widget could not be tested against a
    corpus. It has no regular-expression engine: addresses are recognised by `std`'s own
    parsers, the rest by hand, the way `link` parses links.
  - **Every removal is marked in place, and the marks are numbered.** `[host N]`,
    `[address N]`, `[private address N]`, `[uuid N]`, `[node N]`, `[share link]`, `[redacted]`,
    `[machine]`, `[user]`. A line that never named an address and a line whose address was taken
    out must not read the same, or a reader cannot tell a redaction from an absence. Nor may two
    different hosts read alike: numbering is per kind and per report, so the same value carries the
    same number wherever it appears and one host appearing twice can be told from two. Without it a
    redacted report can be read as a different sequence of events from the one that happened.
    Credentials are not numbered — whether the same password recurs is not something a reader needs
    and is the one class where the correlation is itself sensitive.
  - **An outbound tag names the provider, and is taken out by name (binding).** A tag is `s-`
    plus a server's alias, and `alias::suggest` derives that alias from the server's name and its
    country, so it names the provider and usually the exit country — in every access line and
    every observatory line. It has no dot, so the host rule's two-label guard rejects it, and no
    shape separates it from an ordinary word. The report is therefore built with the daemon's
    server list: `Redactor::for_servers` takes aliases, display names and addresses as literals.
    The `s-` namespace is kept and only the handle replaced, because the prefix names nobody and
    the line is about a pool member. A tag whose handle is not a known server survives, and the
    corpus asserts that — redacting it anyway would mean redacting on the strength of a
    two-character prefix. A server's alias, display name and address share one number: they are
    one server. The CLI builds the same list from the same D-Bus call, which is what keeps the two
    reports identical.
  - **The footer states the rules, not only that there were some (binding).** It names every mark
    the report can contain, says what is kept and why — loopback, the unspecified address, ports,
    and oxidom's own names — and says the rules are shape-based and best-effort. The report ends by
    telling the reporter to read it through, and that instruction is only actionable if they know
    what the rules were meant to catch: a reader who sees `127.0.0.1:1080` and `geoip.dat` intact
    beside a redaction cannot otherwise tell a decision from a miss. The wording lives in one
    table, read both by the code that emits the marks and by the footer, and a test asserts the
    footer names every entry.
  - **Over-redaction is a failure too.** A report reading `[host] [address] [redacted]` on every
    line is as useless as one that leaks. Loopback, the unspecified address and ports stay,
    because they name nobody and are usually the point; a private address is marked
    `[private address]` rather than `[address]`, since which side of the tunnel it was on is the
    difference between a routing bug and a server bug; oxidom's own dotted names — the
    application id, bus names, `geoip.dat` — are not hostnames however much they look like one,
    and neither is a dotted identifier from a library below oxidom (`Client.Timeout`, `io.EOF`),
    which is told apart by case since DNS is written in lower case. The hosts oxidom itself
    reaches for — the release downloads, the pool observatory's default probe destination, the default
    latency target — are kept with or without a scheme in front of them.
    Both directions are pinned by one corpus: shapes that must not survive, and lines that must
    survive byte for byte.
  - **The corpus is built from lines the log holds (binding).** Xray's access log writes
    `network:host:port` on both sides of `accepted` on every line, and a rule that reads the
    address without taking the network off first passes it through whole; its observatory writes
    a quoted URL inside a sentence, and punctuation after a URL belongs to the sentence. A shape
    the log emits constantly and the corpus does not carry is where the next leak is.
  - **No browser is opened.** The report goes on the clipboard and is offered a file. A
    prefilled issue URL would carry the log through a third party's address bar and would submit
    it before the reporter had read it, which is the opposite of what the redaction is for.

Responsiveness: a breakpoint (~700px) collapses the split view and switches the grid to a single
column, exposing the small sidebar toggle button (as annotated in the narrow mockup).
