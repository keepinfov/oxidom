# Changelog

All notable changes to this project are recorded here, following
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html); the release policy,
including what a `0.x` bump means, is in [AGENTS.md](AGENTS.md#versioning-and-releases).

`0.1.0` is the first release. Changes made before this file existed are in the
git history.

Each entry says what changed for someone using oxidom, not which function was
edited. Anything that changes behaviour, configuration, on-disk files, the D-Bus
surface, packaging, or the CLI belongs here.

## [Unreleased]

### Added
- **A pool's health check is no longer one hard-coded address.** A pool's balancer puts a node into
  rotation only once it has reached a ping destination *through* that node. That destination was a
  constant with no setting behind it, so wherever it was blocked, throttled or simply slow through
  an exit, every pool on the machine reported that nothing was in rotation and carried no traffic —
  with a count of nodes as the only clue, and nothing saying a health check was the reason.

  **`[core] pool_probe_url`** now sets it, in `config.toml` for the machine and in a profile for one
  tunnel: two pools through two countries need not share a reachable destination. It appears in
  **Settings › Core behaviour › Pools** and in the profile editor's matching section. Unset means
  the built-in address, so a file written before this generates exactly what it generated before.

  It is deliberately not the existing latency check URL, which is only editable while the check
  method is HTTP — reusing it would drive every pool through an address the interface would not
  always let you change.

  Whatever a subscription supplies for this is still discarded, unconditionally. A destination
  chosen by somebody else is a URL your core would fetch on a timer, through your own exits.

- **A one-line installer.** `curl -fsSL https://keepinfov.github.io/oxidom/install.sh | sh` detects
  apt or dnf and runs exactly the commands the documentation lists, printing each before it runs.
  It checks the repository key it downloads against a fingerprint pinned in its own source and
  refuses to install if they disagree — a fetch that does not check the key is trust-on-first-use
  dressed up as verification. The fingerprint is now published beside the key so the pin can be
  checked against the repository too. The script is in the repository, so it can be read before it
  is trusted, and it deliberately does not enable the system daemon.

### Changed
- **A Debian or Ubuntu user can install oxidom from the README.** `## Install` was the third
  heading, opened with eight lines of Nix, gave Arch two lines of prose, and for everything else
  named three build prerequisites and no commands at all — while a signed apt and rpm repository
  had been documented in `docs/installation.md` all along. Someone on Ubuntu read the top of the
  file and concluded they had to build from source.

  The apt and dnf lines now sit above the feature list, with the AppImage for distributions too
  old for the packages and one line saying an Xray core is required and not bundled. `## Install`
  runs deb and rpm first, Arch next, Nix last, and `## Try it` leads with the installed binaries
  rather than `nix run`. The README also claimed oxidom was on the AUR, which
  `docs/installation.md` says it is not; that is corrected.

### Fixed
- **A pool that carried no traffic says whether its health check ever succeeded.** The message gave
  a count and no cause, and under round-robin rotation the count actively misled: every node stays
  eligible, so it read "3 of 3 nodes were in rotation" while nothing worked. Where the core's own
  log shows the health check failing, that is now what is reported, and it names the setting that
  changes the address.

- **A problem report no longer names the provider.** Server aliases survived a report in full,
  inside every access line the core writes: `[socks-in >> s-nl-soda-vpn]`. That tag is `s-` plus
  the server's alias, and the alias is derived from the server's name and its country — so it named
  the provider and usually the exit country, which is the one thing the bug form asks a reporter
  not to include. No rule reached it: as a token it has no dot, so the host rule never saw it.

  The report is now built with the server list the daemon already holds, so aliases, display names
  and server addresses are taken out by name rather than by shape. The `s-` namespace stays,
  because it names nobody and the line is about a pool member. A tag naming a server the report
  does not know still survives: taking it out would mean redacting on the strength of a
  two-character prefix.

- **A problem report says which redaction is which.** Every removal carried the same word, so two
  different hosts read identically and one host appearing twice could not be told from two:

      error ping https://[redacted] with ...: Head "https://[redacted] context deadline exceeded

  Both of those were the same URL, and nothing said so. Marks are now numbered per report —
  `[host 1]`, `[host 2]`, `[address 1]`, `[node 1]` — so the same value carries the same number
  wherever it appears, and a failure followed through a log cannot be read as a sequence of events
  that never happened. A server's alias, name and address share one number, because they are one
  server. Credentials are deliberately not numbered.

- **A problem report states what it kept, and why.** The footer said what had been removed, in four
  categories, and named none of the marks — `[machine]` and `[user]` appeared in reports and were
  documented nowhere. It said nothing about what was kept on purpose, so a reader seeing
  `127.0.0.1:1080` and `geoip.dat` intact beside a redaction could not tell a decision from a miss.

  It now names every mark a report can contain and what each stood for, says that loopback, the
  unspecified address, ports and oxidom's own names are kept deliberately, and says that the rules
  read shapes rather than meanings and are best-effort. The report ends by asking the reporter to
  read it through; that is only actionable if they know what the rules were meant to catch. The
  wording lives in one table now, so the footer and the marks cannot drift apart.

- **The Connect bar says which session it runs in.** Connecting a group while a profile was
  selected in the header raised the pool in the `default` session. That is the design and not a
  defect — no profile file is read, written or confirmed — but four things on screen implied
  otherwise: the header kept the profile visibly selected, the Connect tooltip said "without saving
  anything" and named no session, the header went on reporting that profile as idle afterwards, and
  a banner announced "1 more profile is running". The connection a user had just started read as
  having happened somewhere else.

  The difference is not cosmetic. Which session runs decides which ports are opened, which
  interface is configured and which routing applies — `default.toml`'s, not the shown profile's.

  The bar now names the session it will use, and says on its face, not only in a tooltip, when the
  profile shown above it is not the one used. A `default` session carrying a group is reported as a
  group rather than counted as another profile.

  Raising a pool in a *named* session is a separate change and is not this one.

- **A stop is offered where it lands, and says what it stopped.** Pressing stop on a
  subscription's latency sweep produced no visible sign that anything had stopped. Whether the
  press landed could only be worked out by watching spinners disappear over the following seconds.
  The daemon answers a cancel with how many checks it dropped, and that number was being thrown
  away. It is now said — and "there was nothing left to stop" is said differently from a stop that
  dropped something. The sidebar's activity indicator settles at the press rather than at the next
  poll.

  A card being checked also offered a stop whether or not the check could be stopped. Cancelling
  drops the queue and nothing else: the checks already measuring each own a thread and run to their
  end, and the check for the server currently carrying the tunnel is made through the tunnel, which
  a cancel never drops. For those, the press did nothing at all and the button stayed a stop button
  that had been pressed. A check that cannot be stopped now keeps its spinner and offers no stop.

  A check that *was* stopped no longer looks like a server that could not be reached or a machine
  with no Xray core — three conditions that drew the same mark in the same colour, one of them the
  user's own doing.


## [0.2.0] - 2026-08-21

### Added
- **An import says what it did not take.** Providers ship routing alongside their nodes —
  advertising blocked, one country direct, the rest through the proxy. oxidom reads the
  servers and nothing else, which is deliberate, and said nothing about the rest, which was
  not: silence reads as "there was nothing else in the body", and the only way to find out
  otherwise was to fetch the subscription by hand.

  Opening a subscription now shows a **Routing** row whenever one arrived with rules of its
  own — how many rules and rule sets, whether it named its own source for rule or geo data,
  and that **none of it was applied**. The same sentence goes in the log at import. Nothing
  carried is still said as nothing: there is no "0 rules" row on a plain subscription.

  It is kept apart from **Skipped**, which is about servers this build could not read. One is
  a failure to understand and the other a deliberate refusal, and a reader needs to tell them
  apart. Where traffic goes stays decided by this application's settings — whoever chooses the
  routing chooses which of your traffic goes around the tunnel.


- **The application says which versions it is running.** A menu button on the right of the
  header — the window's first primary menu — opens an About dialog carrying the version of
  the interface, of the daemon it is talking to, and of the Xray core. The three are separate
  programs with separate lifetimes: a package upgrade replaces the binaries and restarts
  nothing, so a window can spend a whole session driving the daemon it started the morning
  with, and until now the only symptom of that was a control quietly missing from the
  interface. The dialog now says it in a sentence instead, and says nothing at all when the
  two agree.

  Its Troubleshooting page carries the block the bug form asks for — version, how oxidom was
  installed, which daemon answered, what `xray version` says, and the distribution and
  desktop — with the Copy and Save buttons libadwaita provides. Every one of those five was
  something the machine already knew and a reporter was being asked to go and look up.

  A daemon that is too old to name itself is reported as too old, not as blank: `RuntimeInfo`
  gained the two version fields additively, so an older daemon still answers and an older
  client still reads a newer daemon's reply.

- **An expanded card says why the last check failed.** A failed check left one dash and one
  sentence, and "the server did not answer" covers a refused handshake, a wrong TLS
  parameter and a dead network alike. Telling those apart is the whole diagnosis, and it
  meant scrolling the log page with every other source on the machine mixed into it.

  Opening a card now shows the reason the daemon gave, how the check was actually made —
  the method really used, which is not always the one configured, and whether it went
  through the tunnel — and how long ago. A check the user stopped is reported as stopped
  rather than as a fault. A check in flight shows nothing, because the reason it would show
  is about the measurement being replaced.

  Beside it, one button opens the log page narrowed to that server, so the rest of what
  happened is one press away instead of a search away. Nothing here is new information from
  the daemon: the reading has carried the method, the route, the time and the detail all
  along, and the card threw four of them away on the way to a badge.

- **Where the geo data comes from is a setting.** The IP and domain lists the core reads to
  tell your local network apart from the tunnel were fetched from one hardcoded source. The
  published lists differ in what they cover, and for some countries a regional one is the
  difference between routing that works and routing that does not — there was no way to say so
  short of installing the files by hand.

  **Settings › Xray core › Where the geo data comes from** now offers three sources by name —
  v2fly (the default and unchanged), Loyalsoldier, and runetfreedom for Russia — and accepts any
  address that publishes the same shape. The two lists are chosen separately, and `config.toml`
  gains `geoip_url` and `geosite_url`; empty means the built-in source, so a file written before
  this fetches exactly what it fetched before.

  Two rules hold whatever you point at. **Only `https`**, refused before anything is fetched:
  the list and the SHA-256 that vouches for it come down the same connection, so over plain HTTP
  whoever sits between the two machines rewrites both and the check still passes. And **a digest
  or nothing** — always the `.sha256sum` published beside the file named, so a source offering
  none is refused rather than installed unverified.

  The confirmation before a download now names the host it will actually contact, and quotes the
  file sizes only for the built-in pair; it used to say "GitHub" and give two fixed sizes, both
  of which would have been confident lies pointed anywhere else. The copyable recipe offered to
  a daemon that cannot install the files itself quotes the configured addresses for the same
  reason.

- **A problem report is assembled from the log page, with nothing identifying in it.**
  Reporting a bug meant copying log lines by hand and hoping nothing in them was a live
  credential, then going and looking up five things the application already knew. The bug
  form asks the reporter to guarantee that no share link, UUID, password or server address
  is in what they send, and the only way to keep that promise was to read every line.

  Select lines on the Logs page and press **Report a problem**. The report carries the
  version block the About dialog shows, what the connection is made of and the subscription
  User-Agent, then the lines with every address, host name, account id, share link,
  subscription URL, password, machine name and account name taken out. Each removal is
  **marked where it stood** — `[address]`, `[host]`, `[uuid]`, `[share link]`, `[redacted]`,
  `[machine]`, `[user]` — so a bracket reads as a redaction rather than as an absence.

  Over-redaction was treated as a failure too, because a report reading `[host] [address]`
  on every line helps nobody: loopback addresses and port numbers stay, a private address is
  marked as private rather than blanked, and `geoip.dat` is still `geoip.dat`. The rules are
  in `oxidom-core`, so a report the CLI writes will remove the same things, and they are
  pinned in both directions by a corpus — shapes that must not survive, and lines that must
  survive byte for byte.

  The report goes on the clipboard and offers itself a file. **No browser is opened and
  nothing is sent anywhere**: a prefilled issue URL would carry the log through a third
  party's address bar and would submit it before the reporter had read it.

  An expanded card whose last check failed offers the same action beside **Show in logs**,
  narrowing the page to that server first.

- **A card can show more than the newest reading.** One measurement said whether a server
  answered once. It could not say whether it answers reliably, which is the actual question
  behind choosing between two hundred of them: a server that is fast half the time and one
  that is steady looked identical.

  The daemon now keeps the last ten checks for each server, and an expanded card lists them
  newest first with the method each was taken by and how long ago. Checks that ran and failed
  keep their place in the list — a server that times out every other attempt is exactly what
  the list is for, and hiding those rows would make it look steady. Checks called off before
  they ran leave no row, so stopping a large sweep does not push a server's real record out
  of a ten-deep list.

  The list is fetched for the one card that is open, over a new `ProbeHistory` method on the
  bus. It is deliberately not part of the snapshot the interface polls twice a second for
  every server, and nothing about that snapshot changed: a client that has never heard of
  the history still reads a current daemon exactly as before, and a daemon too old to keep
  one is reported as having no checks rather than as an error.

- **A profile can carry its own routing rules.** Everything oxidom generated said the same
  two things about where traffic goes — private addresses direct, and for a pool the rest
  to the balancer — and there was no way to say anything else short of not using oxidom.
  A profile file now takes a `routing` block, written as Xray writes it, and its rules go
  **ahead** of the two above, so a rule you write wins over the built-in one below it:

  ```toml
  routing = '''
  { "rules": [ { "domain": ["geosite:category-ads-all"], "outboundTag": "block" } ] }
  '''
  ```

  Traffic can be sent to `direct`, `block`, or — on a profile that selects a single server —
  `proxy`. A pool has no `proxy` outbound, because its members are reached through the
  balancer, and a rule aimed at one there is refused by name rather than becoming a core
  that will not start. Balancers, a `balancerTag`, and `domainStrategy` are refused too:
  the first two are oxidom's to build, and the third already has a home in `[core]`.
  Everything is checked when the profile is saved and again when it is brought up, so the
  answer is a sentence rather than an exit code.

  There is no editor for it. The profile dialog reports how many rules the block holds and
  writes it back untouched, the way it already treats a group's membership — so editing a
  profile from the interface cannot lose them. It is a profile key rather than a machine
  one on purpose: a rule set machine-wide would also reach the short-lived cores that
  measure latency, where a rule sending the measurement out direct would report a dead
  server as fast.

  **A subscription's own routing is still discarded at import.** Carrying a provider's
  block means mapping its outbound tags onto oxidom's; this is the half that does not need
  that, and it is what lets someone write the same rules by hand today.

- **A latency check can be called off.** The daemon gained `CancelProbes`, which drops
  every check still waiting in the queue. The at-most-eight already measuring finish on
  their own — each holds a slot a thread will release, and taking it early would hand it
  to a second worker — so cancelling a 600-server sweep returns the daemon to idle in
  about ten seconds instead of thirteen minutes. Before this there was no way to stop one
  short of killing the daemon, and quitting the interface did not help because the daemon
  owns the work.

  A cancelled server reports that its check was stopped, rather than going on showing the
  number from last time as though it had just been refreshed. Older clients that have
  never heard of the new reason read it as an ordinary local one instead of failing to
  parse, and a check confirming a live tunnel is never cancelled: it decides whether that
  tunnel stays up.

  **In the interface, the button that started a check now stops it.** It becomes a stop
  icon while a check runs, and switches the moment it is pressed rather than when the
  daemon answers — on a queue of several hundred that answer is seconds away, and a
  control that waits for it looks like one that missed the press. On a collapsed card the
  button is hidden, so the right-click menu carries the same item. Pressing check a second
  time used to be silently ignored, twice over: once by the interface and once by the
  daemon.

  `oxidom ping` still has no way to call one off.

- **A wide window carries a fourth column of cards.** The Servers page stopped at three
  columns, reached at 924 px of content, and every pixel past that went into making the
  same three cards wider. Maximised on a wide screen this meant three fat columns where
  four comfortable ones fit, and the page exists to compare servers against each other,
  so how many are on screen at once is most of what makes the comparison possible.

  A fourth column now appears at 1316 px of content — four cards at 320 px plus the
  spacing between them. That 320 is the width a card already measures its own expanded
  height at, so a column is never narrower than the measurement the layout is holding for
  it. The count still moves with the same hysteresis as the other breakpoints, so a window
  parked on the threshold keeps the grid it has rather than flickering between three and
  four.

- **A signed package repository**, so that installing oxidom is
  `apt install oxidom-gui` and upgrades arrive with the rest of the system
  rather than two files downloaded from a release page. Debian and Ubuntu add a
  source list; Fedora and RHEL drop in a `.repo` file. Only full releases appear
  in it — a repository is what a package manager upgrades to without being
  asked, which is not where release candidates belong.

### Changed

- **Connecting a group needs no profile, and rewrites none.** Connect on the Servers page
  wrote the visible selection into whichever profile happened to be selected, asked the user
  to confirm replacing what that profile held, and — with no profile selected — refused
  outright and told them to go and make one on another page. "Connect me to one of these" is
  the commonest thing the page is asked, and none of that was part of the request.

  It now runs the selection immediately: nothing is written, nothing is confirmed, and no
  profile has to exist. The session is the daemon's `default`, the same one connecting a
  single server uses, over a new `ConnectPool` method; an older daemon says so plainly
  instead of failing with a bus error. Pressing Connect again stops the session running that
  selection — matched on the servers rather than on the name, so a saved group and the same
  servers picked by hand are one run, and the ranking moving between latency checks does not
  make the button forget what is up.

  Saving stays available and stays deliberate: **Save as group** keeps the selection as a
  chip, and **New profile from this…** makes a profile out of it. Connecting a *profile*,
  from the Profiles page, still repoints it and still asks first.

- **The documentation now says which panel answers with which format.** oxidom reads
  share-link lists, Xray JSON, sing-box JSON and Clash YAML, and the panel picks between
  them from the client string it is sent — but nothing said which panel does what, so a
  subscription that came back empty gave no way to tell an unsupported shape from a broken
  one. Marzban, Marzneshin, Remnawave, 3x-ui, Hiddify Manager and V2Board/XBoard now have a
  row in the manual and a test case named after them, including the one where a panel
  answers with a web page because it did not recognise the client. None of the cases has
  been tried against a live panel, and the table says so: each is written from the format
  that panel is documented to serve, so the claim is no wider than what was tested.

- **One system of quotation marks, and one case for a label.** Profile names were quoted with
  guillemets while groups, subscriptions and servers used curly quotes, so a single confirmation
  could ask `Connect «work» to “Europe”?` — two conventions in one sentence. Everything is quoted
  the one way now. A handful of labels were in Title Case while the rest of the interface is
  sentence case; they have joined it, which also means one action no longer answers to three
  differently capitalised names.
- **One description per failed check.** The CLI, the server card and the window each wrote their
  own words for the same four conditions, so the same failure was described differently depending
  on where it was read. There is now one wording per condition, beside the type that carries it.
- **One verb for measuring latency.** The same operation was called check, measure, probe, ping
  and test depending on where you looked: a button said Check, the badge beside it said "not
  measured", the profile dialog said Probe interval and Settings said Latency test URL. It is
  check throughout now. Settings also names the methods the way results do — pick "TCP
  handshake" and the badge reports a TCP handshake, where before you picked "TCP" and were told
  something with a different name.
- **One word for a failure.** A server card's badge read Failed while the description a screen
  reader announced for that same badge said Connection failed, and everywhere else in the
  interface the word is error. The badge and its description now both say error, so the label
  and the spoken text agree.
- **One word for a tunnel that is not up.** The same state was called Ready in the sidebar,
  Disconnected on a card and in the header, and Stopped in the session list — three names a user
  could see at once, none of which meant anything different. It is Disconnected everywhere, which
  is what the daemon has always called it.

- **One word for removing a thing.** The interface said Delete in some places and Remove in
  others, and the two collided: a section headed Remove held a button reading Delete
  Subscription, and every "Delete X?" question explained itself with the word *removed*. It is
  Remove throughout now — headings, buttons, menu items, confirmations and progress lines. The
  word matches the CLI, which only ever said remove.

### Fixed
- **The server grid opens at the width it has.** Every launch laid the cards out in a single
  column down the left of a wide window, and the only way to discover the application could
  do better was to resize it for some unrelated reason. The column arithmetic was right all
  along; the width was pushed to it exactly once, before the window had its final size, and
  nothing ever ran it again.

  The width is now pushed at three moments — when the surface is created, on the first turn
  of the main loop after the window is mapped, and on every later resize — into a setter that
  does nothing unless the count actually changes. No single one of them has to be right.

  The first count also applies immediately instead of waiting for an idle turn. Deferring it
  put the opening frame on screen at the starting value of one and repacked afterwards, so a
  single column flashed on every launch. That was there all along, hidden behind the larger
  defect while the count never changed at all.


- **A tunnel whose core died now holds its traffic instead of releasing it.** When the Xray
  process of a session exited by itself, oxidom removed that session's TUN routes, its
  fwmark rule and its hold on the desktop proxy setting *before* it started retrying — and
  the retry backs off, so for the whole of that window every application fell back to the
  ordinary default route and left with the machine's own address. Nothing in the interface
  said so. The tunnel appeared to be reconnecting while a remote service was already seeing
  the real address and country, which is the one outcome a tunnel exists to prevent.

  A session now keeps its routes and its rule until it is either reconnected or explicitly
  taken down, so traffic aimed at the tunnel is dropped rather than released. The same holds
  for a core that is alive but has stopped answering. An explicit Disconnect is unchanged:
  asking for the ordinary connection back is the one case where falling back to it is what
  was meant.

  This is the shipped default. `on_core_exit = "release"` in `config.toml` — **Settings ›
  Hold traffic if Xray exits**, turned off — restores the old behaviour, and a profile can
  answer for itself. The answer is fixed when the session comes up, so editing a profile
  cannot change what happens to a tunnel already running.

  Because the difference matters, it is visible: the Sessions page marks a held session
  **holding traffic** and says the routes stay until it reconnects or is stopped, and
  `oxidom status` prints its state as `holding` with a `holding_traffic` field in `--json`.
  A network that is deliberately dead must not look like one that is broken.

  A reconnect under a held interface adds nothing — the routes are already there — except
  tun2socks, which is restarted if it did not survive the outage. A device left up with
  nothing behind it would black-hole the tunnel for good, which is worse than the leak.

- **A log that could not be saved says so.** Saving to a place that refuses the write — a
  directory without permission, most often — closed the file chooser exactly as a success
  does and left no file and no message. The only trace was a line in the very log the user
  was trying to save. It now reports the failure, naming the file and the system's own
  reason.

- **Reading back through the log no longer grows without limit.** The Logs page dropped its
  oldest lines only while you were sitting at the bottom of it, which is the one position where
  it hardly matters. Scroll up — to read what happened while a core was talking at debug level,
  the reason anyone scrolls up — and nothing bounded what the page held. It is now trimmed
  wherever you are reading, and your place on the page does not move when it happens.

- **A clock that jumps no longer grows the log view's memory.** Lines wait a fraction of a
  second before being shown, so that a line the daemon wrote first but handed over late
  still appears in the right place. That wait ends on a timestamp, so a clock stepped
  backwards — or one line stamped in the future — could hold the queue shut for as long as
  the discrepancy lasted, and nothing limited what piled up behind it. Past a ceiling the
  oldest waiting lines are now shown anyway, their order unproven. Nothing is discarded:
  two lines in the wrong order are a smaller lie than a gap nobody can see.

## [0.1.0] - 2026-08-18

### Added

- **An AppImage**, for desktops whose distribution is too old for the
  `oxidom-gui` package — above all Ubuntu 24.04 LTS and Debian 12, whose
  libadwaita is 1.5 and 1.2 against a floor of 1.7. It carries its own GTK,
  libadwaita, icon theme and glibc, plus an Xray core and the daemon binary, so
  nothing else needs installing, and it needs no root and no special kernel
  permission to start. Being an installed-nothing bundle it runs a session
  daemon only: local proxies and the GNOME system-proxy toggle work, TUN and
  `oxidom run` need the `.deb` or `.rpm`.

- **A tag publishes a release.** Pushing `vX.Y.Z` builds the packages and the
  AppImage, checks the tag against the manifest and the changelog, and drafts a
  GitHub release with every asset, a `SHA256SUMS`, and notes taken from the
  changelog section for that version. It is left as a draft for a person to
  publish. Every asset carries a build attestation, so `gh attestation verify
  <file> --repo keepinfov/oxidom` says whether a download really came from this
  repository.

- **`.deb` and `.rpm` packages**, in the same two-package split the project has
  always had: `oxidom` is the CLI and daemon with no GTK dependency at all, and
  `oxidom-gui` is the interface, depending on `oxidom` at exactly the same
  version. Installing does not enable the system daemon — that moves which
  database is authoritative, so it stays the administrator's decision — and
  removing, or purging, leaves `/var/lib/oxidom` alone.

  The daemon package is built against glibc 2.36 (`.deb`) and 2.34 (`.rpm`), so
  **it installs on Ubuntu 24.04 LTS, Debian 12 and RHEL 9**, none of which can
  build oxidom from their own repositories. The interface still needs
  libadwaita 1.7 and therefore Debian 13, Ubuntu 25.04 or Fedora 42.

- **The "Install a core" hint names the exact download for this machine.** Where a
  distribution packages a core it still gives the one command — and now covers Alpine,
  Gentoo's GURU overlay and Homebrew alongside Arch and Nix. Where it does not, which is
  Debian, Ubuntu, Fedora, openSUSE and RHEL, the hint used to be the releases page: eighty
  assets, and no indication which of them runs on your machine or what to do with it. It is
  now the archive built for your architecture, with the commands that end in a working
  `xray version`, a **Copy** button that takes all of them and an **Open** button that
  visits the download. An architecture upstream publishes no build for is told so rather
  than sent to a broken link.
- **Settings offers to install the geo data when the core cannot load it.** The row says whether
  the core can read `geoip.dat` and `geosite.dat`, and offers to fix it when it cannot: by copying
  a set already on this machine where one exists, or by downloading. The confirmation names both
  addresses and both destination paths before anything is fetched, and says plainly that GitHub is
  blocked on some networks — where a tunnel is already up, the download can go through it. A
  progress bar reports the transfer and it can be cancelled part-way. A daemon too old to install
  anything is given the commands to run instead of a button that could not have helped it.
- **oxidom installs the geo data its core needs.** `geoip.dat` and `geosite.dat` are a
  requirement of every connection, not of some optional routing feature, and the Xray
  release ships neither — so anyone who installed a core by hand had a client that could
  not connect. The daemon can now fetch both, verify each against the SHA-256 published
  beside it, and point the core at them. It reports progress by the byte and can be
  stopped part-way.

  Files already on the machine are preferred to a download: a set installed by a
  distribution package or another client is used where it lies. Whether any of it is
  usable is settled by asking the core itself (`xray run -test`) rather than by looking
  for filenames — which is also how a **corrupt** list is told from a missing one, and
  the only method that works where the core is a wrapper that supplies the location
  itself, as on NixOS.

  The environment the core is spawned with is left untouched unless oxidom holds both
  files and nothing else has chosen a location, so a machine that works today is
  unaffected.
- **The Logs page tells the three programs apart.** The Xray core, the network
  interface helper and oxidom itself now each tag their own lines, so an
  interface that never came up no longer reads exactly like a core that refused
  its config. Filter by source, hide anything below a chosen severity, or search
  the text; **Save** writes what is on screen to a file.
- **oxidom's own reasoning is finally in the app.** Everything the program says
  about itself used to go to stderr and nowhere else — which for the graphical
  client meant nowhere at all, since it detaches and sends stderr to
  `/dev/null`. It is now in the Logs page beside the core's output.
- **The graphical client keeps a log on disk**, at
  `~/.local/share/oxidom/oxidom-gui.log` (`0600`, rotated at 2MB), so a crash
  leaves something to read. The daemon writes no file: its stderr already
  reaches the journal.
- `LogsSince` on D-Bus, returning only what follows the caller's cursor.
  `RecentLogs` and `ClearLogs` are unchanged, so older clients keep working, and
  `RecentLogs` now answers for every session instead of only `default` — the
  logs of any other profile were previously unreachable by CLI, GUI and bus
  alike.
- **Trust certificate…** on a server card's context menu, for deciding before
  anything fails or after a certificate has changed. Shown only for servers
  using ordinary TLS.
- **The GUI asks about a certificate rather than failing.** A connection that
  fails because the server's certificate cannot be verified now opens a dialog
  showing the fingerprint, and accepting it pins the certificate and reconnects
  — instead of an error that named the server as the problem. Asked once per
  server; a server that is already pinned and still fails does not ask again.
- **Trusting a server's certificate.** Xray 26 removed `allowInsecure`, so a
  server with a self-signed certificate became unreachable no matter what its
  share link said — and failed with an error that blamed the server. `oxidom
  trust <server>` now shows the certificate's SHA-256 fingerprint, and
  `--trust` pins it. A pin accepts one certificate rather than any
  certificate, which is why it replaces `allowInsecure` instead of restoring
  it. Pins survive subscription refreshes, like aliases.

- An **Appearance** setting: follow the desktop, or pin the window to light or
  dark. It applies as it is picked and survives a restart. Until now the app
  followed the system scheme and offered no way to say otherwise, which leaves
  nothing to say it on a desktop that has no such setting.
- **Ctrl+V imports what is on the clipboard.** A subscription URL opens Add
  Subscription, share links open Import Server, both already filled in; opening
  either dialog by hand fills an empty field the same way. Copying a link and
  then having to find the right dialog and paste again was the step nobody
  wanted.
- A working agreement for contributors and agents ([AGENTS.md](AGENTS.md)), the
  binding implementation contract split into [docs/spec/](docs/spec/), a
  contributor guide, this changelog, a security policy, continuous integration,
  and a formatting hook installed by the dev shell.
- A screenshot of the connected server browser in the README, in both desktop
  colour schemes.

### Changed

- **A release lists the commits it contains.** The notes carry the changelog section for the
  version, which says what changed for someone using oxidom, and now also the commit subjects
  since the previous tag — generated, not written. The two are deliberately different lists: a
  release of refactors and CI work has little to say in the first and plenty in the second.

- `flake.nix` reads the version out of `Cargo.toml` instead of repeating it.
  The manifest is now the only place it is written, and `packaging/version.sh`
  checks that every other file naming a version agrees — which CI runs on every
  pull request, rather than leaving it to be discovered while cutting a release.

- The systemd units and the sysusers file live in `packaging/systemd/` and are
  installed straight out of the checkout. They were never Arch-specific, and the
  Arch package carried a second copy of each with a checksum beside it — a
  checksum that once went stale and stopped `makepkg` at the validity check
  before it reached a compiler. Anyone installing the assets by hand should read
  the new paths from `docs/installation.md`.

- The Arch service unit no longer pins the SOCKS and HTTP ports. A pinned port
  is one the daemon refuses to change, which left Settings showing a locked row
  and no way for a desktop user to move their own proxy off 10808. Ports now
  come from `config.toml`, the file the GUI edits. Pinning remains the right
  answer where several people drive one daemon, and `systemctl edit oxidom`
  still does it — `docs/installation.md` says how.

### Fixed

- **The packaged service unit now activates and dies the way the documentation
  says it does.** `docs/spec/interfaces.md` states `KillMode=process` as binding
  and `docs/architecture.md` says D-Bus activation closes the login race, but
  both were describing the NixOS module: the unit installed by the Arch package
  set neither. So a daemon restart killed every running core with it, leaving
  nothing for `recover()` to adopt, and a graphical client autostarting at login
  could still win the race against the unit, fall back to a session daemon and
  bind to a different database — whose only symptom is that the servers have
  vanished. The unit now sets `Type=dbus`, `BusName=` and `KillMode=process`.

- **A core with no geo data now says so, instead of blaming the config.** Xray
  reports a missing `geoip.dat` as `invalid field rule` under "failed to build
  routing configuration", naming neither the file nor the asset directory — so
  oxidom repeated it as "the core refused the generated config", which reads as
  a configuration this program built wrongly and sends the reader to inspect a
  server that was never at fault. Every latency check and every connection now
  reports the real cause and offers the Settings page. A corrupt or truncated
  list is caught the same way: the core rejects it with a different message,
  and both are recognised.
- **The manual no longer claims the geo data is optional.** `installation.md`
  said no `geoip.dat` or `geosite.dat` was needed, on the reasoning that a
  modern core resolves `geoip:private` by itself. It does not: every
  configuration oxidom generates carries that rule plus `geosite:private`, and a
  core that cannot load the lists refuses to start at all, blaming an "invalid
  field rule" rather than the missing file. Anyone who installed a core by hand
  followed that sentence into a client that could not connect — the Xray release
  zip ships the binary alone. Installing the two files is now documented where
  the core is, the real error text is in the troubleshooting guide so a search
  finds it, and both explain that upstream publishes `geosite.dat` as `dlc.dat`.
- **Reading or pinning a server's certificate no longer starts a daemon of its
  own.** `oxidom trust` was the one command outside `up` and `connect` that
  would start a private session daemon when none was running. That daemon keeps
  its own database, so the handle was resolved against a different set of
  servers than the pin was meant for: the server appeared not to exist, or the
  pin was written where nothing else would ever read it. `trust` now requires a
  daemon that is already running and exits 4 without one, like every other
  command that is not bringing the tunnel up.
- **Disconnecting while a connection is still proving itself no longer leaves
  an interface behind.** A tunnel is confirmed on its own thread, and that
  thread asked "is this attempt still the current one?" *before* taking the
  lock it needed to act on the answer. A disconnect landing in the gap was
  answered "still current", so the interface came up anyway — device, routing
  table, nft rule — for a connection that had already been called off, and the
  machine went on sending traffic through something the user had stopped. The
  question is now asked under the same lock that does the work, as the failing
  path in the same function already did.
- **A background task that dies no longer takes Settings with it.** If a worker
  ended without reporting — a panic, or a daemon connection dropped underneath
  it — the operation was never completed. For **Apply** that meant its spinner
  stayed up and Apply and Reset stayed insensitive for the rest of the session,
  with no way back but restarting the app; and if the apply had been asked to
  close the window afterwards, that request stayed armed and shut the window on
  some later save instead. The loss is now reported as a failure of whatever was
  asked for, and says what it does and does not mean: nothing was cancelled, but
  what is on screen may be out of date.
- **The certificate dialogs say when they cannot answer.** Reading a
  certificate and pinning one each wait on a worker, and both read "the worker
  has gone" as "the worker has not answered yet" — so a failure produced no
  message at all, and left a timer polling every 50ms for the life of the
  process.
- **A card checking for a long time stops flickering, and giving up on one
  works.** Sweeping a large subscription runs eight checks at a time, so a card
  near the end of the queue can legitimately wait longer than the five-minute
  backstop. When the backstop fired it forgot the card was waiting at all — and
  the daemon was still naming that check, so the very next poll read it as a new
  one, put the spinner back and restarted the clock. The card blinked once every
  five minutes rather than settling, and a daemon that had genuinely lost track
  of a check kept its card spinning anyway, which is the one thing the backstop
  is for.
- **A stopped profile can be started again from its own row.** The Profiles page
  was waiting for a word the daemon has never sent. It reads four states off the
  wire, and the one meaning "stopped" is spelled `disconnected` — so a session
  the daemon was holding but not running fell through to "Unknown", and an
  unknown state deliberately greys out the row's switch, on the reasoning that a
  switch cannot assert a position it does not understand. The reasoning was
  sound; the state was perfectly well understood. Reachable without any newer
  daemon: a reconnect that fails to confirm leaves exactly such a session.
- **The Filter button draws its funnel on Arch.** The Arch package installed the
  two application icons and not the one action icon the application ships,
  because Adwaita has no filter glyph under any name. On a release build the
  pill at the head of the chip row was therefore an empty square — the
  development builds hid this, since a `cargo run` installs the icon into the
  user's data directory on startup and a packaged build has no such step.
- **The Logs page no longer throws you back to the top.** Scrolling up to read
  something used to last only until the next line arrived. The daemon handed
  over its whole buffer twice a second and the view worked out the difference by
  comparing text — which stopped working once the 500-line buffer filled, because
  from then on every new line shifted every other one. The view rebuilt itself
  instead, and a rebuild resets the scroll position. It broke, in other words,
  exactly when there was finally enough output to be worth reading. The view now
  receives only what it has not seen and appends it, and trims old lines only
  while you are at the bottom.
- A server the core could speak to perfectly well could be reported as one whose
  protocol it does not support. oxidom's own warning about an unrecognised
  obfuscation type was written into the same buffer that was then searched for
  the core's failure markers, and it contains the words that marker matches.
- Output from the network interface helper was lost entirely when no session
  existed to redirect it into, and was indistinguishable from the core's
  otherwise.
- The log buffer no longer holds only the last 500 lines, which a core at
  `debug` filled in seconds — often discarding the reason for a failure before
  anyone could read it.
- A failed latency check now says what went wrong instead of blaming the
  server. The probe core's log was discarded, so a core that refused to talk to
  a server — most often because it would not accept the server's certificate —
  reached the user as "server is unreachable". The core's own words are now
  read on the failing path and, when they name a condition, reported as one:
  the certificate was rejected, the server asks for unverified TLS that Xray 26
  removed, or the core refused the generated config. Probe cores run at `info`
  for this, because at `warning` the same refusal is reported on one transport
  and dropped on the next.

- A machine with no Xray core no longer reports every server as unreachable.
  The default latency check measures through a core it starts for the purpose,
  so without one it failed for every server at once — and said so as "server is
  unreachable", which blames working nodes for a missing program. Such a
  failure now reads as a check that could not run, a whole-subscription check
  says once that nothing was measured, and a banner names the cause while the
  core is missing.
- Country flags now appear for providers that spell the country in plain
  letters. Detection accepted only a leading flag emoji, so `DE-2 HYSTERIA2`
  read as no country at all and every such card showed a globe. A leading
  two-letter token is now read when it is a real ISO code — only the first
  token, and only a real code, so `second-ws-stas` does not become Samoa.
  Names carrying no country still show no flag; nothing is guessed from the
  address.
- The Arch package builds again: the recorded checksum for `oxidom.service` no
  longer matched the file, which stopped `makepkg` at the validity check before
  it reached a compiler. The package now also declares the libraries it links
  and ships the `.SRCINFO` the AUR requires.

[Unreleased]: https://github.com/keepinfov/oxidom/commits/master
