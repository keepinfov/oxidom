#!/bin/sh
# nfpm gives deb and rpm the same script, and they do not agree on arguments:
# dpkg calls `postinst configure <old-version>`, rpm calls `%post <1|2>` where 1
# is an install and 2 an upgrade. Anything else — an abort-upgrade, a triggered
# run — is not ours to act on.
set -e
case "${1-}" in
    configure|1|2) ;;
    *) exit 0 ;;
esac

# The daemon runs as its own unprivileged account. sysusers.d is the right way
# to ask for it, but Debian 12 does not guarantee systemd-sysusers is present.
if command -v systemd-sysusers >/dev/null 2>&1; then
    systemd-sysusers /usr/lib/sysusers.d/oxidom.conf || true
else
    getent group oxidom >/dev/null 2>&1 || \
        groupadd --system oxidom 2>/dev/null || true
    getent passwd oxidom >/dev/null 2>&1 || \
        useradd --system --no-create-home --gid oxidom \
                --shell /usr/sbin/nologin \
                --comment "oxidom tunnel daemon" oxidom 2>/dev/null || true
fi

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
    # Picks up the new file in /usr/share/dbus-1/system.d without restarting the
    # bus, which would drop every session on the machine.
    systemctl reload dbus.service >/dev/null 2>&1 ||
        dbus-send --system --type=method_call \
            --dest=org.freedesktop.DBus / org.freedesktop.DBus.ReloadConfig \
            >/dev/null 2>&1 || true
fi

# Only on a first install. An upgrade printing this every time is noise.
first_install=no
case "${1-}" in
    1) first_install=yes ;;                      # rpm
    configure) [ -z "${2-}" ] && first_install=yes ;;   # dpkg: no old version
esac
[ "$first_install" = yes ] || exit 0

# Deliberately NOT `systemctl enable --now`, though Debian policy would normally
# have a daemon started on install. The system daemon keeps its database in
# /var/lib/oxidom rather than the user's home, so enabling it silently moves
# which database is authoritative and then denies access to anyone outside the
# wheel and oxidom groups. A desktop install does not need it at all: the client
# starts a session daemon of its own. Turning this on is the administrator's
# decision, so it is printed rather than taken.
cat <<'EOF'

oxidom is installed. The system daemon is not enabled, because enabling it
changes which database your servers live in (/var/lib/oxidom, not your home
directory) and restricts access to root, wheel and the oxidom group. A desktop
client does not need it — it starts a session daemon of its own.

To run it at boot for every user on this machine:

    sudo systemctl enable --now oxidom.service
    sudo gpasswd -a "$USER" oxidom     # then log out and back in

oxidom also needs an Xray core, which no distribution packages. See
/usr/share/doc/oxidom/README.md, or run `oxidom status` — it names the exact
download for this machine.

EOF
exit 0
