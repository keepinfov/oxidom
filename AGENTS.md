# oxidom — working agreement

This file governs how work is done in this repository. It applies to everyone
who changes it: maintainers, outside contributors, and coding agents alike.
Where it says "you", that includes an agent acting on someone's behalf.

Two other bodies of writing sit next to it, and neither replaces it:

- [`docs/spec/`](docs/spec/) — the binding implementation contract. What the
  software must *do*: on-disk formats, generated Xray configuration, the CLI
  surface, probe semantics, interface handling. Change behaviour and you change
  that text in the same commit.
- [`docs/`](docs/) — the manual for people who *use* oxidom.

A directory may carry its own `AGENTS.md` that tightens these rules for its own
files. None may loosen them.

## Ground rules

- **Do not put real credentials anywhere.** Subscription URLs, share links,
  UUIDs, passwords and server addresses are live secrets. They do not belong in
  commits, tests, fixtures, issues, log excerpts, or screenshots — not even
  partially redacted. Use invented values; the test suite already does.
- **Leave unrelated work alone.** Do not reformat, refactor, re-order, or
  "clean up" code your task does not touch. A diff should be readable as one
  idea.
- **Do not weaken a check to get past it.** Deleting a test, loosening an
  assertion, or adding an `#[allow]` is a change that needs its own
  justification in the commit body.
- **Say what actually happened.** If a check fails, report the failure and its
  output. Never describe a gate as passing that you did not run, and never
  present an unverified claim as measured.
- **Ask before anything irreversible**: force-pushing, rewriting published
  history, deleting branches or tags, touching a user's live database under
  `~/.local/share/oxidom` or `/var/lib/oxidom`.
- **No AI or agent attribution in commits.** No `Co-Authored-By`,
  `Generated-by`, or `Assisted-by` trailers, in any form.

## Repository layout

| Path | What lives there |
|---|---|
| `crates/oxidom-core/` | The shared library: tunnel engine, subscription and link parsing, Xray config generation, probes, paths, D-Bus client. Most of the logic and most of the tests. |
| `crates/oxidom/` | The `oxidom` binary: CLI and the daemon that owns the tunnel and serves D-Bus. |
| `crates/oxidom-gui/` | The `oxidom-gui` binary: the GTK4/libadwaita client. UI state lives in reducers so it can be tested without a display. |
| `data/` | Desktop entry, icons, metainfo, D-Bus policy and service files. |
| `nix/` | The NixOS module. |
| `packaging/aur/` | The Arch package: `PKGBUILD`, `.SRCINFO`, systemd units, sysusers. |
| `docs/`, `docs/spec/` | User manual; binding implementation contract. |

Parsing, Xray control and routing belong in `oxidom-core`. The GUI must not
reimplement them.

## Environment

Everything is built through the flake's dev shell. This is not a preference:
the workspace does not build outside it. `pkg-config` and the GTK/libadwaita
headers come from the shell, and the toolchain it pins (currently rustc 1.96)
is the one the gates are green on — a newer host nightly reports warnings this
project has not fixed.

```sh
nix develop                       # the shell everything below assumes
nix develop -c cargo run -p oxidom-gui
nix develop -c cargo run -p oxidom -- status
nix build                         # both wrapped binaries, as installed
nix build .#oxidom-cli            # headless package on its own
nix build .#oxidom-gui            # graphical package on its own
```

Entering the shell points `core.hooksPath` at [`.githooks/`](.githooks), which
formats what you stage. To enable it by hand:

```sh
git config core.hooksPath .githooks
```

## The validation suite

Run these before asking anyone — or any CI — to look at your work. Every one of
them passes on `master` today; a failure is yours to explain.

```sh
nix develop -c cargo fmt --all -- --check                                # ~2s
nix develop -c cargo clippy --all-targets --all-features -- -D warnings  # ~1min
nix develop -c cargo test --workspace                                    # ~90s cold, ~7s warm
nix run nixpkgs#alejandra -- --check .                                   # ~3s, Nix files
nix build                                                                # ~6min cold
```

`nix flake check` realises the same two derivations as `nix build`; run one,
not both. On a machine you are also using, prefix the slow ones with
`nice -n 19 ionice -c3` and cap them with `-j8`.

**Documentation-only changes** — anything touching just `docs/`, `README.md`,
`CHANGELOG.md` or this file — need only a careful read and
`git diff --check`. The Rust suite is not required, and CI skips it for you.

A failing check blocks the merge. If the failure is demonstrably older than
your change, say so in the pull request with the evidence, and fix it in a
separate commit rather than folding it into yours.

## Testing obligations

The suite is 417 tests and takes seven seconds warm. There is no excuse for
sending an untested change.

- **Every bug fix ships with a regression test that fails without the fix.**
  Write the test first and watch it fail; a test that passes before your change
  is not testing your change.
- **Parsers, serialisers and config generation are tested against literal
  bytes.** A share-link change means a test with the actual link text; a
  generated-JSON change means a test with the actual JSON. `docs/spec/` records
  behaviour the tests are expected to pin — if you change what it says, change
  the test in the same commit.
- **Test the invariant, not the function.** Test names here are sentences that
  state what must remain true — `a_dead_core_is_noticed_without_a_status_call`,
  `column_hysteresis_prevents_threshold_flapping`. Follow that.
- **Tests are deterministic and offline.** No sleeps to sequence things, no
  network, no reliance on the developer's own machine. A test that needs a real
  network, a live Xray core, root, or someone's real subscription is marked
  `#[ignore = "<why>"]` with the reason and, in a doc comment, the command that
  runs it. Three such tests exist; keep the count honest.
- **GUI logic goes in reducers.** UI state changes belong in pure functions
  under `gui::reduce` and friends, tested headlessly. No test constructs a
  widget or initialises GTK.
- **Filesystem tests redirect the root.** Use `paths::set_test_root` behind
  `sync::lock(&paths::TEST_ROOT_LOCK)` with the RAII `TestRoot` guard pattern
  already in `profile.rs`, `state.rs` and `engine.rs`. The root is
  process-global; taking the lock is not optional.
- **External binaries are faked through their env seam.** `BinarySpec` and the
  `OXIDOM_*_BIN` variables exist so tests can point at a shell stub. Do not
  shell out to a real `xray`, `nft` or `tun2socks` in a default test.

Unit tests live in `#[cfg(test)] mod tests` at the bottom of the file they
cover. The single integration test exists because its behaviour cannot be
exercised in-process; add another only for the same reason.

## Branches

Every change, including a one-line documentation fix, goes on a branch cut from
current `master`.

- Name it `type/short-kebab` — `type` being one of the commit types below.
- Lowercase ASCII, digits, hyphens; two to six words; 48 characters at most.
- Work-in-progress and fixup commits are fine on the branch. They do not
  survive the merge.

## Commits

```text
type(scope)!: what is true after this commit
```

- **Types**: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `build`, `ci`,
  `chore`, `revert`.
- **Scopes**: `core`, `cli`, `gui`, `packaging`, `nix`, `docs`, `ci`, `deps`.
  Ask before inventing one.
- **The subject describes the resulting state, not the action.** This project
  writes `feat(gui): a subscription's User-Agent offers the same presets as the
  global one`, not `add user-agent presets`. Lowercase, no trailing period, 72
  characters at most. It reads as a sentence about the software.
- Wrap the body at 100 characters. Explain why, and what you verified — not
  what the diff already shows.
- `Fixes #N` / `Refs #N` only for an issue that actually exists. No other
  trailers without being asked.
- **Every commit is signed** (SSH or OpenPGP), and signatures are verified
  before a merge. Configure `user.signingkey` and `commit.gpgsign` locally;
  never commit a key or an identity into a tracked file. If you cannot sign,
  say so rather than merging unsigned.

  Signing is not the same as being able to verify. For GitHub to show a
  commit as verified, add the same key to your account as a *signing* key —
  an authentication key does not count. To verify locally, point
  `gpg.ssh.allowedSignersFile` at a file listing the signers you trust; keep
  it out of the tree, for instance in `.git/allowed_signers`:

  ```sh
  echo "you@example.com $(cat ~/.ssh/id_ed25519.pub)" > .git/allowed_signers
  git config gpg.ssh.allowedSignersFile .git/allowed_signers
  git log --format='%h %G? %s' -5   # G = verified
  ```
- Breaking changes need `!`, a `BREAKING CHANGE:` footer saying what breaks and
  what to do about it, and a changelog entry. Get agreement before writing one.

## Pull requests

`master` is only ever updated through a pull request.

Open it as a draft while it settles, mark it ready when the validation suite is
green locally. The description says what changed and why, how you verified it,
and anything a reviewer should distrust. If you could not run part of the
suite, say which part and why.

A pull request is mergeable when: CI is green, every commit is signed, the
[definition of done](#definition-of-done) is satisfied, and a maintainer has
approved it. It merges as a squash, and the branch is deleted.

Nothing is pushed to `master` directly, nothing is force-pushed, and no
published history is rewritten.

## Definition of done

- [ ] The validation suite passes, or the exceptions are named in the PR.
- [ ] A regression test covers the bug, or new tests cover the new behaviour.
- [ ] `docs/spec/` matches the behaviour, if the behaviour contract moved.
- [ ] The user manual in `docs/` matches, per the table below.
- [ ] `CHANGELOG.md` has an entry under `[Unreleased]`, unless the change is
      invisible to users (a refactor with no behavioural effect, a test, CI).
- [ ] `Cargo.lock` is committed if dependencies moved, and nothing unrelated
      was bumped.
- [ ] No secret, no real server, no personal path anywhere in the diff.

## What to update when

| If you change… | Update |
|---|---|
| A CLI command, flag or exit code | `docs/spec/cli.md`, `docs/cli.md` |
| A `config.toml` key or a state file | `docs/spec/storage.md`, `docs/configuration.md` |
| Link parsing or a subscription format | `docs/spec/data-model.md`, `docs/subscriptions-and-protocols.md` |
| Generated Xray JSON or core options | `docs/spec/xray-config.md`, `docs/configuration.md` |
| Profiles, pools or session semantics | `docs/spec/profiles-and-pools.md`, `docs/profiles-and-pools.md` |
| Probes or the latency reading contract | `docs/spec/latency.md`, and the wording in `docs/cli.md`, `docs/gui.md`, `docs/troubleshooting.md` |
| TUN, routing or per-app routing | `docs/spec/interfaces.md`, `docs/routing.md` |
| A GUI page, dialog or flow | `docs/spec/gui.md`, `docs/gui.md` |
| The D-Bus surface | `docs/spec/`, and say so in the PR: old clients must keep working |
| Packaging or install steps | `packaging/`, `docs/installation.md` |
| Anything a user would notice | `CHANGELOG.md` |

## Versioning and releases

The project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Nothing has been released yet: there is no `v*` tag, and the `0.1.0` in
`Cargo.toml` is a placeholder.

Never change a version, write a release commit, or create a tag without being
asked to. Recommend a release; do not perform one unbidden.

While the project is below `1.0.0`:

- patch — a compatible fix;
- minor — a backward-compatible feature;
- minor — also a breaking change, called out in the changelog.

At `1.0.0` and after, ordinary major/minor/patch semantics apply, and the D-Bus
interface and on-disk formats become compatibility surfaces in their own right.

A release touches more places than the manifest, and missing one has broken the
package before:

1. Cut `release/vX.Y.Z` from `master`.
2. Set the version in `Cargo.toml` (`[workspace.package]`) and in the two
   `version = ` strings in `flake.nix`; refresh `Cargo.lock`.
3. Move the `[Unreleased]` entries into a dated `[X.Y.Z]` section.
4. Update `packaging/aur/PKGBUILD` and regenerate `.SRCINFO` with
   `makepkg --printsrcinfo > .SRCINFO`.
5. Run the whole validation suite.
6. Commit as `chore(release): release vX.Y.Z`, signed, and verify the
   signature.
7. Merge, then create a signed annotated tag `vX.Y.Z` on the merge commit.

Pushing the tag publishes the release. Confirm the remote and the tag before
pushing it.

## Dependencies and toolchain

- The toolchain is whatever `flake.lock` pins. There is no separate MSRV claim,
  because no other toolchain is tested. Do not add one without adding the CI
  job that proves it.
- `Cargo.lock` is committed and must stay in sync with the manifests: the Arch
  package builds `--frozen` and fails outright on drift.
- A new dependency needs a reason in the pull request. This program handles
  other people's credentials and traffic; every crate added is supply chain
  someone inherits. Prefer the standard library, then something already in the
  tree.
- Do not bump unrelated dependencies in a feature change.

## Safeguards

These hold regardless of what a task asks for. If a change would break one,
stop and raise it.

- The GUI and CLI run unprivileged. Only the system daemon may hold
  `CAP_NET_ADMIN`, and only for interface work.
- Configuration and state are written `0600` by atomic temp-and-rename, into
  directories created `0700`.
- A hardware id is sent to a provider only for a subscription that has opted
  in. There is no telemetry of any kind.
- Secrets never reach a log line. Logs are expected to be safe to paste into a
  bug report.
- The daemon owns the database. The GUI reads and writes it only over D-Bus.
- Xray resolution order is `config.toml`'s `xray_binary`, then
  `$OXIDOM_XRAY_BIN`, then `xray` on `PATH`. The same shape applies to
  `tun2socks` and `nft`.
- Vulnerabilities go through [SECURITY.md](SECURITY.md), never a public issue.

## For agents

Everything above applies to you. In addition:

- **Plan before a large change.** A task is large when it touches three or more
  subsystems, mixes a format change with a behaviour change, or would sensibly
  land as more than one commit. Present the plan — goals, order, what each step
  must satisfy — before writing code.
- **One worktree per writing agent.** Parallel agents never share a working
  tree. Give each its own branch and a disjoint set of files.
- **Do not act on the user's live system.** Their daemon, their tunnel and
  their subscriptions are not test fixtures. When exercising the app, isolate
  it: a throwaway `HOME`, its own ports, and no path to the system bus.
- **Verify before claiming.** Run the command, read the output, quote it. "It
  should work" is not a result.
- **Propose features in an issue**, not in a private file. Anything the
  repository does not contain does not exist for the next contributor.
