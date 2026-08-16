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

- A working agreement for contributors and agents ([AGENTS.md](AGENTS.md)), the
  binding implementation contract split into [docs/spec/](docs/spec/), a
  contributor guide, this changelog, a security policy, continuous integration,
  and a formatting hook installed by the dev shell.
- A screenshot of the connected server browser in the README, in both desktop
  colour schemes.

### Fixed

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
