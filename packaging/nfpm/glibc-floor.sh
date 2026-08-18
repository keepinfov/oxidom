#!/bin/sh
# Assert a binary needs no glibc newer than the package claims.
#
# This is what makes the "installs on Ubuntu 24.04 and Debian 12" claim checkable
# rather than merely intended. Symbol versioning is the mechanism: a binary built
# against glibc 2.41 references GLIBC_2.41 symbols and simply will not start on
# anything older, whatever the package metadata says. Building in an old
# container is what keeps the requirement low; this is what proves it stayed low.
#
#   packaging/nfpm/glibc-floor.sh target/release/oxidom 2.36
set -eu

bin=${1:?usage: glibc-floor.sh <binary> <max-glibc>}
floor=${2:?usage: glibc-floor.sh <binary> <max-glibc>}

need=$(objdump -T "$bin" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
[ -n "$need" ] || { echo "error: no GLIBC symbol versions in $bin — is it dynamically linked?" >&2; exit 1; }

echo "$bin needs glibc $need; the package declares $floor"

highest=$(printf '%s\n%s\n' "$need" "$floor" | sort -V | tail -1)
if [ "$highest" != "$floor" ]; then
    echo "error: $bin needs glibc $need but the package only requires $floor." >&2
    echo "       It would install on systems that cannot run it. Either build in" >&2
    echo "       an older container or raise the declared dependency." >&2
    exit 1
fi
