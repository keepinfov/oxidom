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

- An **Appearance** setting: follow the desktop, or pin the window to light or
  dark. It applies as it is picked and survives a restart. Until now the app
  followed the system scheme and offered no way to say otherwise, which leaves
  nothing to say it on a desktop that has no such setting.
- A working agreement for contributors and agents ([AGENTS.md](AGENTS.md)), the
  binding implementation contract split into [docs/spec/](docs/spec/), a
  contributor guide, this changelog, a security policy, continuous integration,
  and a formatting hook installed by the dev shell.
- A screenshot of the connected server browser in the README, in both desktop
  colour schemes.

### Fixed

- A machine with no Xray core no longer reports every server as unreachable.
  The default latency check measures through a core it starts for the purpose,
  so without one it failed for every server at once — and said so as "server is
  unreachable", which blames working nodes for a missing program. Such a
  failure now reads as a check that could not run, a whole-subscription check
  says once that nothing was measured, and a banner names the cause while the
  core is missing.
- The Arch package builds again: the recorded checksum for `oxidom.service` no
  longer matched the file, which stopped `makepkg` at the validity check before
  it reached a compiler. The package now also declares the libraries it links
  and ships the `.SRCINFO` the AUR requires.

[Unreleased]: https://github.com/keepinfov/oxidom/commits/master
