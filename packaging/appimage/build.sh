#!/usr/bin/env bash
# Build the AppImage.
#
# Why this is a `nix bundle` rather than linuxdeploy. The AppImage exists for the
# distributions that cannot install oxidom-gui — Ubuntu 24.04 LTS and Debian 12,
# whose libadwaita is 1.5 and 1.2 against a floor of 1.7. But GTK 4.18 and
# libadwaita 1.7 only exist on distributions with glibc 2.41, so a conventional
# AppImage built anywhere new enough to *link* against them requires a glibc
# newer than the systems it was built for. It would run only where the .deb
# already works, which is nowhere useful.
#
# `nix bundle` packages the derivation's entire closure — GTK, libadwaita, the
# gdk-pixbuf loaders, the GIO modules, the GSettings schemas, the Adwaita icon
# theme, and glibc with its own dynamic loader. The host's glibc stops mattering.
#
# It also reuses the flake, which already gets the runtime environment right:
# wrapGAppsHook4 for the icon theme and loaders, and OXIDOM_XRAY_BIN /
# OXIDOM_BIN / OXIDOM_TUN2SOCKS_BIN pointing at absolute store paths, so the core
# and the daemon are found with no PATH setup and are pulled into the closure.
#
# The `.#oxidom` attribute is the join of both binaries, which is what the
# AppImage should carry: the interface is a thin client and needs `oxidom daemon`
# beside it.
set -euo pipefail

# Pinned rather than floating: an AppImage that silently changes its runtime
# between releases is not something anyone can debug afterwards.
BUNDLER=${BUNDLER:-github:ralismark/nix-appimage/7946addbc0d97e358a6d7aefe5e82310f0fe6b18}

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

version=$(packaging/version.sh)
arch=$(uname -m)
out=${1:-dist}
mkdir -p "$out"

echo "bundling .#oxidom $version ($arch) with $BUNDLER"
nix bundle --bundler "$BUNDLER" .#oxidom -o result-appimage

target="$out/oxidom-$version-$arch.AppImage"
cp -L result-appimage "$target"
chmod +x "$target"
rm -f result-appimage

size=$(du -h "$target" | cut -f1)
echo "built $target ($size)"

# A bundle that cannot start is worse than none, and the failure mode is silent
# — the AppImage runs, prints nothing and exits. Ask it something harmless.
echo "checking it runs"
"$target" --appimage-extract-and-run --version || {
    echo "error: the AppImage did not answer --version" >&2
    exit 1
}
