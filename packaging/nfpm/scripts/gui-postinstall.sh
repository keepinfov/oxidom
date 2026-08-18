#!/bin/sh
# On Debian these are also driven by dpkg triggers from hicolor-icon-theme and
# desktop-file-utils; on Fedora nothing does them for you.
set -e
case "${1-}" in
    configure|1|2) ;;
    *) exit 0 ;;
esac
command -v gtk-update-icon-cache >/dev/null 2>&1 &&
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor >/dev/null 2>&1 || true
command -v update-desktop-database >/dev/null 2>&1 &&
    update-desktop-database -q /usr/share/applications >/dev/null 2>&1 || true
exit 0
