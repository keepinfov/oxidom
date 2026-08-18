#!/bin/sh
# The project version, and the check that every file claiming to know it agrees.
#
# `Cargo.toml` is the source of truth. Everything else either derives the value
# (flake.nix reads the manifest) or repeats it in a format that cannot be
# derived — the AppStream release entry needs hand-written notes, the changelog
# needs a hand-written section — and those are what this checks.
#
# Deliberately POSIX sh with grep and sed only: it runs in a bare CI image and in
# an archlinux:base-devel container, neither of which has jq, yq or xmllint.
#
#   packaging/version.sh                    print the version
#   packaging/version.sh --check            invariants that hold at every commit
#   packaging/version.sh --check-release v1.2.3   the above, plus release-only ones
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

version() {
    # The same expression packaging/aur/PKGBUILD uses in pkgver().
    grep -m1 '^version' "$root/Cargo.toml" | cut -d'"' -f2
}

fail() {
    echo "error: $1" >&2
    status=1
}

check() {
    v=$1
    status=0

    [ -n "$v" ] || { echo "error: no version in Cargo.toml" >&2; return 1; }

    # The AppStream release entry. GNOME Software believes this, so a version
    # that was never released is not a cosmetic mistake.
    meta="$root/data/dev.keepinfov.oxidom.metainfo.xml"
    meta_version=$(grep -m1 '<release version=' "$meta" | sed 's/.*version="\([^"]*\)".*/\1/')
    [ "$meta_version" = "$v" ] || fail \
        "metainfo names release $meta_version, Cargo.toml says $v"

    # A <release> with no date, or a placeholder one, is worse than none.
    grep -m1 '<release version=' "$meta" | grep -q 'date="[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]"' \
        || fail "the newest metainfo <release> has no well-formed date"

    # flake.nix must derive the version rather than repeat it.
    ! grep -q 'version = "[0-9]' "$root/flake.nix" \
        || fail "flake.nix hardcodes a version; it should read Cargo.toml"

    return $status
}

check_release() {
    v=$1
    tag=$2
    status=0

    check "$v" || status=1

    [ "$tag" = "v$v" ] || fail "tag $tag does not match version $v"

    # The changelog must have a dated section for it: releasing with everything
    # still under [Unreleased] is how a release ends up with no notes.
    grep -q "^## \[$v\] - [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]" "$root/CHANGELOG.md" \
        || fail "CHANGELOG.md has no dated [$v] section"

    return $status
}

case "${1-}" in
    "")               version ;;
    --check)          check "$(version)" && echo "version $(version): consistent" ;;
    --check-release)  [ $# -eq 2 ] || { echo "usage: $0 --check-release vX.Y.Z" >&2; exit 2; }
                      check_release "$(version)" "$2" && echo "release $2: consistent" ;;
    *)                echo "usage: $0 [--check | --check-release vX.Y.Z]" >&2; exit 2 ;;
esac
