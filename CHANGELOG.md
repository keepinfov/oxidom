# Changelog

All notable changes to this project are recorded here, following
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html); the release policy,
including what a `0.x` bump means, is in [AGENTS.md](AGENTS.md#versioning-and-releases).

Nothing has been released yet: there is no `v*` tag, and the `0.1.0` in
`Cargo.toml` is a placeholder rather than a published version. Everything below
is therefore unreleased. Changes made before this file existed are in the git
history.

Each entry says what changed for someone using oxidom, not which function was
edited. Anything that changes behaviour, configuration, on-disk files, the D-Bus
surface, packaging, or the CLI belongs here.

## [Unreleased]

### Added

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

- The Arch service unit no longer pins the SOCKS and HTTP ports. A pinned port
  is one the daemon refuses to change, which left Settings showing a locked row
  and no way for a desktop user to move their own proxy off 10808. Ports now
  come from `config.toml`, the file the GUI edits. Pinning remains the right
  answer where several people drive one daemon, and `systemctl edit oxidom`
  still does it — `docs/installation.md` says how.

### Fixed

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
