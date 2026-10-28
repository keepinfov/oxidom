# GUI

The shape of the graphical client: its window layout, the server browser, and how it stays
adaptive.

The GUI is an `adw::Application` (app id `dev.keepinfov.oxidom`). It wires to the core modules and
does not reimplement parsing/Xray logic in the UI layer.

## Startup (binding)

Daemon discovery and the initial subscription, profile, status, settings, runtime and capability
calls run on the same background worker, never on the GTK main thread. The startup window stays
responsive and reports "Loading daemon state..." while that worker hydrates the initial state;
background startup remains hidden. Discovery follows the phase deadlines in `storage.md`.

Deliver the client and owned initial state together, once, before constructing the interactive
window. Preserve the launch notification id and present the real window before dismissing the
splash. Closing the splash abandons delivery and quits; it is not a request to stop the daemon.
No late hydration result may overwrite an interactive action.

Hydration fields fail independently. A failed subscription read is not an authoritative empty
catalog and must not prune saved GUI ordering. A failed initial status produces unknown state,
not proof that tunnels stopped; unavailable state must not clear a desktop proxy. Retain existing
defaults for display, but failed catalog/profile/settings or non-legacy runtime reads leave control
unavailable and start selected recovery. Capability checks read cached handshake facts. This does
not impose a total hydration deadline or move tray/desktop-proxy work
off the main thread.

`OXIDOM_SESSION_DAEMON=external` makes startup a discovery-only client of externally supervised
daemons. On absence, use the existing unavailable dialog; Retry must not create a private daemon
or invoke a service manager. Runtime poll failure uses selected recovery below, never discovery
or a daemon restart loop. Closing the window, Quit and discovery failure are not daemon
stop commands. The opt-in user daemon unit is independent of the tray unit; ordinary standalone
startup does not promise cgroup or logout survival.

## Selected recovery (binding)

A failed authoritative poll or operation follow-up retires the binding before applying operation
outcomes or reconciling desktop proxy. Preserve last-known sessions and proxy as unknown, never
infer stopped from missing replies. Retire callbacks by a binding generation separate from action
epochs; old success, failure and worker-loss results cannot update catalog/settings/status/logs,
retire successor operations or launch follow-up mutations. Each binding owns its poll mailbox and
busy flag. Close daemon-derived dialogs/popovers on retirement, reject stale picker submissions
before selection changes, and stamp queued tray commands when their menu is built.

Run at most one recovery/hydration worker. Retry failed attempts no sooner than two seconds after
completion. Use only `recover_selected` from `storage.md`: no discovery, bus/store fallback,
bootstrap, mutation replay or supervisor invocation. Legacy successors remain refused without
store proof. A candidate is not installed until subscriptions, profiles, status, settings and probe
state (including queued/running phases) have all loaded. Runtime is required except explicit
`UnknownMethod` from a legacy binding. All reads use the same immutable client. This is coherent
identity, not a transactional database snapshot or universal hard hydration deadline.

Install the candidate, state, capabilities, runtime locks and widgets while admission is closed,
then enable control. Clear old optimistic operations, pending closes, probe notifications and
remote log cursor; preserve local logs, validated selection and GUI preferences. Refresh settings
baseline, adopting it for a clean draft while preserving unsubmitted edits in a dirty draft; never
replay Apply. Local Cancel/Discard/Quit remain usable while unavailable. GUI closure still does not
stop daemon sessions. No guarantee is made that an indeterminate mutation did not execute; fresh
state must be inspected before repeating it.

## Layout

### Target And Control Facts (binding)

For each session, show saved selection, pending request and reported selection as separate
labelled facts in expanded Profiles rows. A pending target takes headline precedence without
rewriting the saved selection or relabelling reported runtime. An unavailable snapshot labels
reported facts last-known and cannot authorize mutations. Capture saved starts at admission,
including resolved single-server identity where available; pool requests retain their query for
display and separately resolved IDs for acknowledgement. No pool has a representative server.

Primary card, group-bar and session controls derive their label, availability and semantic action
from the same pure reducer decision. Global usage/highlighting is decoration, not permission to
disconnect another session. Only the selected session determines the action. Matching connected,
connecting or held selections offer Stop subject to admission; a retained failed/disconnected
saved session requires explicit restart sequencing, not an Up that the daemon rejects. Unknown
states authorize no action. Runtime-only sessions permit Stop, not saving or inventing a profile.
Revalidate the displayed semantic action before dispatch; a stale Disconnect must not become
Connect or follow a changed profile. Context-menu revisions also invalidate equal-label actions.

Temporary pool connection changes runtime only. Default single-server Connect is temporary.
Named single-server card retargeting retains the existing explicit Repoint confirmation, naming
the saved-file write and runtime interruption; it is never a silent temporary-operation emulation.
Runtime-only named entries without a saved profile cannot repoint through that path. Explicit
Save/Profile Repoint remains the only selection-persistence operation. This does not add a new
named temporary-server RPC or alter existing D-Bus signatures. Target equality is not proof of a
new attempt incarnation.

### Runtime Dimensions (binding)

Profiles, header, sidebar and tray share a pure presentation of independent runtime dimensions.
Phase is stopped, starting, verifying, running or failed. Running is accepted lifecycle state,
not a guarantee of continuous reachability. Local Start/Stop intent has summary and tone
precedence; reported phase remains a separate labelled detail. Unknown wire phase and unavailable
polls remain unknown, including when the last snapshot reported direct standby.

Health is the last attributable confirmation, active or watchdog observation, with outcome,
source and UTC timestamp. Missing health means no observation reported, not healthy. Negative
synthetic observations do not themselves change phase or authorize switching targets. Pool
rotation and profile-keyed latency are not substitutes for current-core health attribution.

Protection shows recorded exit policy, scoped traffic hold, interface/up/route mode and logical
desktop-proxy ownership independently. Neither logical ownership nor interface bookkeeping
proves desktop-setting application or whole-device protection. Missing fields mean not reported;
held traffic concerns this session's assigned traffic, not every application on the machine.

Retry is idle, backoff or attempting, independently of phase and hold. Backoff uses the daemon's
remaining monotonic delay; the changing countdown must not rebuild expanded selectable details.
Idle means no active retry episode, not disabled policy. Older daemons retain known legacy phase
and hold but do not fabricate health, policy or retry facts. Existing semantic control admission,
saved-target checks and binding-generation fences remain in force.

Single-server optimistic starts retain their requested server ID independently of reported
runtime identity and saved profile configuration. A current poll reporting another server as
Connecting/Connected cannot acknowledge the request; matching target in the pinned profile,
terminal error, or the existing pin deadline may retire it. Foreground operation/deadline guards
remain unchanged. Named-profile reconciliation uses that profile's runtime server ID. Pools do
not invent a representative server. Admission rollback restores the latest accepted reported
identity immediately, not the optimistic replacement. Target equality alone does not prove a
distinct attempt/core incarnation; this is local reconciliation, not new wire attribution.

Profiles rows and the session switcher share an inventory: saved-profile order followed by
runtime-only session keys in sorted, deduplicated order. Runtime-only entries say "No saved
profile", retain Disconnect/Stop (including held traffic), and expose neither Edit nor saved
Start. A pending Stop turns the switch off; unknown state cannot authorize a tray toggle. When
a selected runtime-only session disappears, select a remaining default/first entry rather than
leaving controls bound to an invisible missing key. Recovery retains selection if either saved
profile or runtime session still exists. Inventory placeholders are never saved as profiles.

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
  control offers a stop unless the read-only handshake advertises probe cancellation: a button that
  says it will stop something and then cannot is worse than the second press being ignored.
 - **Server card** (custom widget): country **flag**, server **name**, protocol **subtitle**
   (`transport_label`, e.g. "vless + xhttp + reality"), optional **latency badge** (green when
   low). The **expanded** card shows the server's identity, any trust or local-edit marks, the
   recent checks as a chart with the range on its heading and the failures under it, and why the
   last check produced no number, while the complete parameter listing (`draft::parameters`) opens
   in its own details dialog from the card's action row or context menu.
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
   - **Editing opens the creation dialog prefilled** (`show_server_edit_dialog`), and a
     subscription's server has its protocol row locked. In the parameter listing, an overridden
     field is marked and carries a per-row action that drops the override and takes the
     provider's value back — the drop is `DropOverride`, not a local repaint, so the store and
     the card cannot disagree. A composite profile shows no edit action: it has no fields.
   - **The copy action hands over a link and names its limits.** A server without a link of its
     own — typed by hand — gets the canonical form of its stored fields. When the link cannot
     carry something the server has, the action says so after copying, in the field names the
     editor uses; a composite profile's button is insensitive and its tooltip says why.
  - **Selectable text keeps its own menu.** The gesture bubbles rather than captures, so a
    metadata label that is selectable answers a right-click with the text menu and the card menu
    covers everything else. Those labels are selectable so an address or a failure reason can be
    copied out; a card menu taking the press first would remove the only way to do it.
  - **The expanded card states what identifies a server; reading it in full happens on its own
    surface (binding).** The card carries identity — flag, name, alias, `protocol ·
    address:port`, the transport summary (`transport_label`) — the trust marks that change what
    a connection means (`allow_insecure`, a pinned certificate), and the latency record with its
    failure reason. The complete field-by-field listing of the stored server, under the JSON key
    names the editor uses, belongs to a details surface opened from the card, never to the card
    itself. A card is glanced at among two hundred neighbours; a server is inspected one at a
    time, and the two readings earn different surfaces. What the card deliberately leaves
    elsewhere: which profiles or pools carry the server stays the mark above and the profiles
    page; which subscription it came from, and when that was refreshed, stays the subscriptions
    page. The card answers "what is this server", not "what is it doing".
  - **The card reads; changing a server happens in a dialog.** An action on the action row may
    open an editing surface, but nothing on the card edits in place.
  - **Nothing added to the card may grow it past its measured height (binding).** The expanded
    height is measured from real content and animated once; content without a small upper bound
    scrolls inside the card's own scroller instead of resizing the grid around it. The
    measurement targets a height the late-arriving pushes then correct, so every block added to
    the card either has a fixed height or scrolls — a block that grows with its data re-opens
    that correction visibly.
  - **A secret is masked wherever it is shown.** A password, UUID or pre-shared key renders
    masked with an explicit reveal, and a revealed value never reaches a log line.
- **Groups:** a chip row above the grid — `All`, one chip per saved group, `+`. A group narrows
  **the one list**, and is never a second block of cards: rendering it as its own block
  shows the same server two or three times and leaves no way to tell which card is real, and
  "show it only in its highest-priority group" makes a starred server vanish from its
  subscription. Cards stay in their subscription; the chip narrows what is shown. Selecting a
  chip reveals a Connect bar that **runs the visible selection immediately**: no
  profile is written or confirmed, and the session is the selected profile's (`default` when none
  was chosen) over `ConnectPool`. Pressing it again stops the session running that selection,
  matched on the server ids rather than on the query **within the selected session only**, so a
  saved group and the same servers arrived at by hand are one selection. A matching pool in a
  different session must never be disconnected by this bar.
  - **The bar names the session (binding).** "Writing nothing" is not "touching nothing":
    `<session>.toml`'s interface, `[core]` and routing block apply, along with its ports for a named
    session (`default` uses the daemon's configured ports). Which session runs decides which
    endpoints are opened and which routing holds. The button says which session it will use,
    and where the header is showing a non-default profile
    the bar says on its face — not only in a tooltip — that it runs in that session.
  - **The header's selection governs both the single-server click and the group Connect.** A card
    click honours the selection and offers to repoint the profile; a group Connect raises the pool
    in the selected profile's session without writing its `.toml` file, and an absent session name
    on `ConnectPool` still means `default` so older clients keep working.
  - **A `default` session running a pool is reported as a group, not as another profile.** The
    banner exists to name what is out of sight; a pool raised by the bar on screen is not, and
    `default` is not a profile anybody named. Reported by count, it read as the connection having
    happened somewhere else.

  Saving stays explicit and separate: the picker's **Save** stores a group chip; the selection
  menu's **New profile from this…** stores a profile and refuses an empty frozen selection.
  Repointing a saved profile still rewrites a file and still
  asks first, which is what connecting a *profile* means. Favourites is a built-in list; the card's star is what fills it, and it
  cannot be deleted because the star would have nowhere to put things.
- **Connection picker (binding):** the `Filter` pill, header menu's **Pick connection…**,
  `Ctrl+K` and tray's **Pick connection…** open the same window, not separate pickers or a small
  popover. Saved groups, country/protocol/subscription filters, exclusions, hand-picked members,
  `Alive only`, a latency ceiling and rotation width share a matching-server preview and count.
  Rotation is a local requested-width draft, committed to page/run settings only by valid,
  current-binding Apply/Save/Connect. Cancel/dismissal and rejected submissions do not commit.
  Preview capping changes effective width, not the remembered request; groups persist membership,
  not run rotation. Strategy-menu selection configures the next explicit primary action and
  refreshes presentation only: no Connect, Disconnect or profile saving, and no live replacement.
  Choices within one metadata field are alternatives; the different fields and health constraints
  intersect. Health filtering reads only the daemon's direct `ProbeState.readings`, never the
  displayed badge when it comes from `proxied`. Unknown and failed readings are excluded by
  either health constraint; a ceiling includes values equal to its maximum. Without a health
  constraint they stay eligible. **Connect** runs the query previewed in this window, without
  silently intersecting it with the Servers page's search text. Preview, count and activation
  use the same eligibility rules; new direct readings may change the next result. Rotation
  defaults to 6, offers bounded widths and an explicit **All**, and is clamped to the result size.
  **Apply** shows the draft on the page; **Save** stores a named group. Profile creation remains
  in the selection menu, separate from Connect; connecting never saves a profile.
- **Server grid:** a top block of "loose"/favorite servers, then one **block per subscription**:
  each block shows its **title** + **description** (name + quota/expiry from userinfo) followed by
  that subscription's server cards. In the code a block is `SubscriptionBlock` — never a "group",
  which the window uses only for the saved selections in the chip row. Multi-column grid in wide mode, single column in narrow
  (use `adw::WrapBox`/`FlowBox` with a breakpoint via `adw::Breakpoint`).
- **Connect control:** a single primary **Connect/Disconnect** toggle in the header for the
  selected profile's session (`default` initially). It shows its live status (Connecting/Connected/Error) and
  active latency. If other sessions run, a persistent banner reports their count and points to
  `oxidom status`.
- **Unavailable daemon snapshot (binding):** a current poll failure marks tunnel state unknown,
  not disconnected. The window, session rows, profile switcher and tray stop claiming a fresh
  connected state; last-known sessions and proxy ownership are retained. Repeated identical
  failures are not logged on every tick. A stale success or failure cannot change a newer UI
  epoch, and a current success clears the warning. Polling retries the existing client and store;
  it must not rediscover another daemon/database as recovery. Local log lines continue draining
  using the last remote book when one has been obtained.
- **Desktop proxy recovery (binding):** an owner that is holding traffic, or replacing its core
  while `connecting`, retains its endpoint. Replacing or reconciling an owned proxy never switches
  to direct mode on failure. Private recovery intent is written before changing desktop settings;
  a failed clear or marker removal retains the recovery record for retry. An unreadable daemon
  snapshot is not permission to clear the proxy. Explicit stop/release still relinquishes it.
- **Tunnel watchdog:** Settings offers Off, Conservative and Aggressive, behind Apply/Reset.
  Conservative is the default and reports failed synthetic checks without killing a live core.
  Against a daemon whose settings omit the key, the control is disabled and says the daemon must
  be upgraded/restarted rather than claiming the new policy works.
- **Subscriptions view:** add (URL + optional name), update-now, delete; per-sub **"send HWID"**
  switch (default OFF) with a privacy hint.
  **Stop remains available during fetch (binding).** One Disconnect/DownProfile may overlap a
  subscription Add/Update/Update All. Other mutations and a second Stop remain serialized.
  Each worker owns its completion record; finishing or losing either never clears the other's
  spinner or Stop pin. A superseded fetch snapshot is discarded, not re-stamped as current after
  Stop. The stopping session alone is busy; activity reports both workers while they overlap.
  Stop does not claim to cancel the subscription request: its result still arrives normally.
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
  reports the pinned managed Xray path (or a matching-version override) and whether the core can
  load its geo data and offers to install it. What the rows offer is a
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
- **Direct standby is explicit (binding).** **Answer on local ports while disconnected** is off
  by default. When enabled, only `127.0.0.1` on the daemon's configured SOCKS/HTTP ports answers
  directly, and only while no session in any profile is connected, connecting or holding traffic.
  Starting any session first stops the listeners and closes accepted direct connections. The GUI
  reports **Direct proxy** while standby is actually active, even if a failed `default` session
  remains inspectable; it must not imply a tunnel or hide direct service behind that error.
  Standby never takes automatic ownership of the GNOME system proxy. A session **holding traffic**
  is fail-closed, not direct standby, and suppresses standby until the hold is released.
- **Primary menu and About:** one menu button on the right of the header, outermost so that it
  does not move when a page's own menus appear beside it. It is the only header control that is
  not about the current view, and it carries Pick connection, Quit and About.
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
  daemon could not hand over are announced in place, not silently dropped — on a replacement
  round too, after the view is rebuilt. A daemon too old to keep a cursor re-sends everything
  and the view is replaced each round; that replacement re-feeds the GUI's own lines rather than
  erasing this process's book along with the daemon's.
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
