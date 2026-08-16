<!--
The rules behind this checklist are in AGENTS.md. Delete any line that does not
apply, but do not tick one you did not do — an honest gap is easier to review
than a wrong claim.
-->

## What this changes

<!-- What is true after this lands, and why it was worth doing. -->

## How it was verified

<!--
Which of these you ran, and what they said. Name anything you could not run.

  nix develop -c cargo fmt --all -- --check
  nix develop -c cargo clippy --all-targets --all-features -- -D warnings
  nix develop -c cargo test --workspace
  nix build
-->

## Checklist

- [ ] The validation suite passes, or the exceptions are named above.
- [ ] A test covers this — a regression test for a fix, new tests for new
      behaviour.
- [ ] `docs/spec/` matches, if the behaviour contract moved.
- [ ] The manual in `docs/` matches ([what to update when](../AGENTS.md#what-to-update-when)).
- [ ] `CHANGELOG.md` has an `[Unreleased]` entry, or this is invisible to users.
- [ ] Commits are signed, and their subjects describe the resulting state.
- [ ] No real subscription URL, share link, UUID, password, server address or
      personal path anywhere in the diff.
