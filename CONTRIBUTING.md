# Contributing to oxidom

Thanks for looking. This page gets you from a clone to a reviewable pull
request. The rules that decide whether a change can be merged live in
[AGENTS.md](AGENTS.md) — this is the short path through them.

## Get it building

Everything goes through the Nix dev shell. The workspace does not build without
it: the GTK4 and libadwaita versions this project needs are newer than most
distributions ship, and the shell also pins the Rust toolchain the checks are
green on.

```sh
git clone https://github.com/keepinfov/oxidom
cd oxidom
nix develop                        # first entry downloads a fair amount
git config core.hooksPath .githooks # entering the shell also does this
```

Then:

```sh
nix develop -c cargo run -p oxidom-gui        # the app
nix develop -c cargo run -p oxidom -- status  # the CLI
nix develop -c cargo test --workspace         # ~7s once warm
```

You do not need a real VPN subscription to develop. Nothing in the test suite
talks to a network.

## Find something to do

- Bugs and features: the [issue tracker](https://github.com/keepinfov/oxidom/issues).
  Open one before writing a large feature, so the design can be discussed
  before the code exists.
- Both kinds go through a form, and the questions are not ceremony. Which
  daemon answered decides which database is authoritative; how oxidom was
  installed decides which GTK it links against; the core's version decides what
  a config may contain. A report without them usually cannot be acted on.
- Security problems: **not** an issue — see [SECURITY.md](SECURITY.md).
- Not sure how a subsystem works? [`docs/spec/`](docs/spec/) is the
  implementation contract, written with the reasoning that produced it. It is
  the fastest way into this codebase.

## Make the change

```sh
git switch -c fix/short-description master
```

Work in small, readable commits. Before you push:

```sh
nix develop -c cargo fmt --all -- --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --workspace
```

Three things reviewers will look for immediately:

1. **A test.** A fix comes with a test that fails without it. A feature comes
   with tests for what it now does.
2. **The docs that go with it.** AGENTS.md has a
   [table of what to update when](AGENTS.md#what-to-update-when).
3. **A changelog entry**, under `[Unreleased]`, if a user would notice.

Commit subjects here describe the state after the change rather than the act of
changing it — `fix(gui): a stopped group profile says which group`, not
`fix group label`. Commits must be signed; see
[AGENTS.md § Commits](AGENTS.md#commits).

## Open the pull request

Draft while it settles, ready when the checks pass. Say what you changed, why,
and how you verified it. If you could not run part of the suite — no Nix, no
Linux, no hardware — say which part; that is useful information, not a failing.

CI runs formatting, clippy, the tests, a full `nix build`, and packaging
checks. Documentation-only changes skip the Rust jobs automatically.

A maintainer reviews, and the branch is squash-merged.

## A few house rules worth repeating

- **Never paste a real subscription URL, share link, UUID, password or server
  address** into an issue, a PR, a log excerpt, or a screenshot. They are live
  credentials. Invent values instead — the tests do.
- Do not reformat or refactor code your change does not touch.
- Do not delete or weaken a test to make something pass. If a test is wrong,
  say why in the commit.
- **Do not point at anything a clone does not contain** — your own notes
  directory, a local planning file, a chat log, a document only you can open.
  Whoever reads your commit or pull request cannot follow the link, so write
  the substance out in full, or commit the document alongside the change.

## Licence

oxidom is [MIT](LICENSE). By contributing you agree your work is released under
it.
