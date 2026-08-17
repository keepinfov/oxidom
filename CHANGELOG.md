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
