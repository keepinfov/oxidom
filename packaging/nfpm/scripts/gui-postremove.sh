#!/bin/sh
set -e
command -v gtk-update-icon-cache >/dev/null 2>&1 &&
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor >/dev/null 2>&1 || true
command -v update-desktop-database >/dev/null 2>&1 &&
    update-desktop-database -q /usr/share/applications >/dev/null 2>&1 || true
exit 0
