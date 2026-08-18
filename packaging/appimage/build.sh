#!/usr/bin/env bash
# Build the AppImage.
#
# It exists for the desktops that cannot install oxidom-gui: Ubuntu 24.04 LTS and
# Debian 12, whose libadwaita is 1.5 and 1.2 against a floor of 1.7.
#
# Two obstacles, and only the first is obvious.
#
# The first is glibc. GTK 4.18 and libadwaita 1.7 exist only where glibc is 2.41,
# so anything linked against them refuses to start on a 2.39 host. The bundle
# therefore carries its own glibc and its own dynamic loader.
#
# The second cost an earlier attempt. `nix bundle` solved glibc by mounting a
# store through an unprivileged user namespace — and Ubuntu has restricted those
# by default since 23.10, so the result would not start on the exact release it
# was built for. Escaping glibc is not enough; the escape must need no privilege
# the target withholds.
#
# sharun maps the bundled interpreter with userland-execve: no namespaces, no
# mounting, no privileges. lib4bin gathers each binary's libraries, rewrites the
# interpreter and RPATH to relative paths, and sharun sets GTK_PATH,
# GDK_PIXBUF_MODULE_FILE, GIO_MODULE_DIR and GSETTINGS_SCHEMA_DIR relative to
# itself at startup. uruntime then mounts the image through SUID fusermount3
# where it exists — which is the default on the distributions this targets — and
# falls back to a namespace only if it must.
#
# Build this on the oldest distribution that carries libadwaita 1.7, which is
# Debian 13. Anything newer raises the bundled glibc for no gain.
set -euo pipefail

SHARUN_URL=${SHARUN_URL:-https://github.com/VHSgunzo/sharun/releases/download/v0.8.1/sharun-x86_64}
LIB4BIN_URL=${LIB4BIN_URL:-https://raw.githubusercontent.com/VHSgunzo/sharun/v0.8.1/lib4bin}
URUNTIME_URL=${URUNTIME_URL:-https://github.com/VHSgunzo/uruntime/releases/download/v0.5.9/uruntime-appimage-squashfs-lite-x86_64}
XRAY_URL=${XRAY_URL:-https://github.com/XTLS/Xray-core/releases/download/v26.3.27/Xray-linux-64.zip}

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
version=$(packaging/version.sh)
arch=$(uname -m)
out=${1:-dist}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$out"

need() { command -v "$1" >/dev/null || { echo "error: $1 is required" >&2; exit 1; }; }
# lib4bin names its own dependencies when it is missing one, but it does so
# after the build has already run, so check them here where the message costs
# nothing: file binutils patchelf findutils grep sed coreutils.
for t in cargo curl unzip mksquashfs glib-compile-schemas patchelf file strip find; do need "$t"; done

echo "== binaries =="
cargo build --release --locked -p oxidom -p oxidom-gui

echo "== tools =="
curl -sSfL -o "$work/sharun"   "$SHARUN_URL"   && chmod +x "$work/sharun"
curl -sSfL -o "$work/lib4bin"  "$LIB4BIN_URL"  && chmod +x "$work/lib4bin"
curl -sSfL -o "$work/uruntime" "$URUNTIME_URL" && chmod +x "$work/uruntime"
export PATH="$work:$PATH"

echo "== the core, which oxidom drives but does not build =="
curl -sSfL -o "$work/xray.zip" "$XRAY_URL"
unzip -q -o "$work/xray.zip" xray -d "$work"
chmod +x "$work/xray"

APPDIR="$work/AppDir"
mkdir -p "$APPDIR"

echo "== gathering libraries =="
# --with-hooks packs the auxiliary files a library needs at startup, which is
# what makes the gdk-pixbuf loader cache and the GIO module cache usable from a
# directory they were not built for.
SHARUN="$work/sharun" lib4bin \
    --dst-dir "$APPDIR" \
    --with-sharun --hard-links \
    --patch-interpreter --patch-rpath \
    --with-hooks --strip \
    target/release/oxidom-gui \
    target/release/oxidom \
    "$work/xray"

echo "== data GTK reads at runtime =="
# Schemas are consulted by name, and a GTK application that cannot find
# org.gtk.Settings aborts on startup rather than degrading.
install -d "$APPDIR/share/glib-2.0/schemas"
cp -r /usr/share/glib-2.0/schemas/. "$APPDIR/share/glib-2.0/schemas/"
# --strict: a schema that will not compile should stop the build, not produce a
# cache the application aborts on at startup.
glib-compile-schemas --strict "$APPDIR/share/glib-2.0/schemas"

# Adwaita is where every symbolic icon in the interface comes from; hicolor is
# the fallback theme the lookup ends at.
install -d "$APPDIR/share/icons"
cp -r /usr/share/icons/Adwaita "$APPDIR/share/icons/" 2>/dev/null || true
cp -r /usr/share/icons/hicolor "$APPDIR/share/icons/" 2>/dev/null || true

# oxidom's own assets, at the same paths the packages install them to.
install -Dm644 data/dev.keepinfov.oxidom.svg \
    "$APPDIR/share/icons/hicolor/scalable/apps/dev.keepinfov.oxidom.svg"
install -Dm644 data/dev.keepinfov.oxidom-symbolic.svg \
    "$APPDIR/share/icons/hicolor/symbolic/apps/dev.keepinfov.oxidom-symbolic.svg"
# Adwaita ships no filter icon under any name, so the funnel travels with the
# application; without it the Filter pill draws an empty square.
install -Dm644 data/icons/oxidom-funnel-symbolic.svg \
    "$APPDIR/share/icons/hicolor/scalable/actions/oxidom-funnel-symbolic.svg"
install -Dm644 data/dev.keepinfov.oxidom.metainfo.xml \
    "$APPDIR/share/metainfo/dev.keepinfov.oxidom.metainfo.xml"
install -Dm644 data/dev.keepinfov.oxidom.desktop "$APPDIR/dev.keepinfov.oxidom.desktop"
install -Dm644 data/dev.keepinfov.oxidom.svg "$APPDIR/dev.keepinfov.oxidom.svg"
ln -sf dev.keepinfov.oxidom.svg "$APPDIR/.DirIcon"

echo "== entry point =="
# sharun reads .app to know which of the packed binaries is the application, and
# AppRun is a hard link to sharun rather than a script so /proc/self/exe resolves.
echo 'oxidom-gui' > "$APPDIR/.app"
ln -f "$APPDIR/sharun" "$APPDIR/AppRun"

# The interface spawns `oxidom daemon` as a child and drives an Xray core; both
# travel in bin/, and naming them here is what stops the search falling through
# to a host that has neither. ${SHARUN_DIR} is expanded by sharun at startup.
cat > "$APPDIR/.env" <<'ENV'
OXIDOM_BIN=${SHARUN_DIR}/bin/oxidom
OXIDOM_XRAY_BIN=${SHARUN_DIR}/bin/xray
XDG_DATA_DIRS=${SHARUN_DIR}/share:/usr/local/share:/usr/share
ENV

echo "== packing =="
# An AppImage is its runtime followed by a filesystem image, and that is all the
# assembly amounts to. uruntime mounts through SUID fusermount3 where the host
# has one — the default on the distributions this targets — and only falls back
# to a user namespace when it does not, which is what stops a restricted host
# from being a dead end.
target="$out/oxidom-$version-$arch.AppImage"
mksquashfs "$APPDIR" "$work/image.sqfs" \
    -comp zstd -Xcompression-level 19 -root-owned -noappend -no-progress -quiet
cat "$work/uruntime" "$work/image.sqfs" > "$target"
chmod +x "$target"

[ -s "$target" ] || { echo "error: packing produced nothing" >&2; exit 1; }
echo "built $target ($(du -h "$target" | cut -f1))"

# A bundle that cannot start is worse than none, and the failure is silent — it
# runs, prints nothing, exits. Extraction rather than mounting, so this measures
# the bundle and not whether the build host happens to allow FUSE.
echo "== checking it runs =="
"$target" --appimage-extract-and-run --version
