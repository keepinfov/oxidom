#!/bin/sh
set -e

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
fi

# The `oxidom` account and /var/lib/oxidom are deliberately left behind, on
# purge as much as on remove. That directory is the user's subscription
# database and their pinned certificates; a package manager is the wrong thing
# to delete it, and reinstalling would otherwise lose everything silently.
case "${1-}" in
    purge|0)
        [ -d /var/lib/oxidom ] && cat <<'EOF'

oxidom's database is left in /var/lib/oxidom — it holds your subscriptions and
pinned certificates. Remove it by hand if you want it gone.

EOF
        ;;
esac
exit 0
