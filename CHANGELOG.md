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

- **A signed package repository**, so that installing oxidom is
  `apt install oxidom-gui` and upgrades arrive with the rest of the system
  rather than two files downloaded from a release page. Debian and Ubuntu add a
  source list; Fedora and RHEL drop in a `.repo` file. Only full releases appear
  in it — a repository is what a package manager upgrades to without being
  asked, which is not where release candidates belong.

### Changed

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
