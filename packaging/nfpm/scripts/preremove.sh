#!/bin/sh
# dpkg: `prerm remove|upgrade|deconfigure|...`. rpm: `%preun <0|1>`, 0 being the
# last copy going away and 1 an upgrade. Stop the daemon only when it is really
# leaving; an upgrade restarts it below.
set -e
case "${1-}" in
    remove|purge|0) ;;
    *) exit 0 ;;
esac

if [ -d /run/systemd/system ]; then
    systemctl --no-reload disable --now oxidom.service >/dev/null 2>&1 || true
    # Instances of the template unit, if any profile was enabled at boot.
    for unit in $(systemctl list-units --plain --no-legend 'oxidom@*.service' 2>/dev/null | awk '{print $1}'); do
        systemctl --no-reload disable --now "$unit" >/dev/null 2>&1 || true
    done
fi
exit 0
