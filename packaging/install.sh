#!/bin/sh
# Install oxidom from the signed package repository, on a distribution that has
# apt or dnf.
#
# Served from the same host as the key it verifies, so that fetching this script
# and fetching the key are one act of trust rather than two. Everything it does
# is in docs/installation.md as commands you can run yourself; this exists so
# that a Debian or Fedora user has one line to run, not so that anything happens
# out of sight. It prints each step before taking it.
#
# It verifies the key it downloads against the fingerprint pinned below, and
# refuses to install if that check cannot be made. A fetch that does not check
# the key is TOFU dressed as verification: whoever can rewrite the repository can
# rewrite the key that vouches for it.
#
# It deliberately does not enable the system daemon. Which daemon runs decides
# where the database lives, and moving somebody's database is not something an
# installer gets to do quietly. `packaging/nfpm/scripts/postinstall.sh` prints
# what to do next, and this leaves that message alone.
set -eu

FINGERPRINT='05BC9AA4B90FF65ACE7FAE1C74FE48BE84CA2CCF'
PAGES='https://keepinfov.github.io/oxidom'
PACKAGE='oxidom-gui'

say() { printf '%s\n' "$*"; }
die() {
	printf 'oxidom install: %s\n' "$*" >&2
	exit 1
}

run() {
	say "  \$ $*"
	"$@"
}

need() {
	command -v "$1" >/dev/null 2>&1 || die "$1 is needed and was not found"
}

while [ $# -gt 0 ]; do
	case "$1" in
	--server | --cli) PACKAGE='oxidom' ;;
	-h | --help)
		say "usage: install.sh [--server]"
		say
		say "  --server   install oxidom (daemon and CLI, no GTK) instead of oxidom-gui"
		exit 0
		;;
	*) die "unknown option: $1" ;;
	esac
	shift
done

need curl
need gpg

# The pinned fingerprint is the whole point of this script, so an empty one is a
# packaging mistake and must stop the run rather than degrade it silently.
case "$FINGERPRINT" in
[0-9A-F][0-9A-F]*) ;;
*) die 'no key fingerprint is pinned in this script — refusing to install unverified' ;;
esac

if command -v apt-get >/dev/null 2>&1; then
	manager='apt'
elif command -v dnf >/dev/null 2>&1; then
	manager='dnf'
else
	die "no apt or dnf here — see docs/installation.md for AppImage, Nix, Arch and source"
fi

sudo=''
if [ "$(id -u)" -ne 0 ]; then
	need sudo
	sudo='sudo'
fi

key="$(mktemp)"
trap 'rm -f "$key"' EXIT INT TERM

say "Fetching the repository key from $PAGES"
curl -fsSL "$PAGES/KEY.gpg" -o "$key" || die 'could not fetch the repository key'

say "Checking it against the fingerprint this script was published with"
seen="$(gpg --show-keys --with-colons --with-fingerprint "$key" 2>/dev/null |
	awk -F: '/^fpr:/ { print $10; exit }')"
[ -n "$seen" ] || die 'the downloaded key could not be read'
if [ "$seen" != "$FINGERPRINT" ]; then
	say "  expected $FINGERPRINT"
	say "  received $seen"
	die 'the key does not match — not installing. Report this: it should never happen.'
fi
say "  ok: $seen"

case "$manager" in
apt)
	run $sudo install -d -m 0755 /usr/share/keyrings
	run $sudo gpg --batch --yes --dearmor -o /usr/share/keyrings/oxidom.gpg "$key"
	say "  \$ echo \"deb [signed-by=/usr/share/keyrings/oxidom.gpg] $PAGES/deb stable main\" | $sudo tee /etc/apt/sources.list.d/oxidom.list"
	echo "deb [signed-by=/usr/share/keyrings/oxidom.gpg] $PAGES/deb stable main" |
		$sudo tee /etc/apt/sources.list.d/oxidom.list >/dev/null
	run $sudo apt-get update
	run $sudo apt-get install -y "$PACKAGE"
	;;
dnf)
	run $sudo curl -fsSL "$PAGES/oxidom.repo" -o /etc/yum.repos.d/oxidom.repo
	# The repo file sets gpgcheck and repo_gpgcheck, so dnf verifies every
	# package and the metadata against this key from here on.
	run $sudo rpm --import "$key"
	run $sudo dnf install -y "$PACKAGE"
	;;
esac

say ''
say "Installed $PACKAGE from the signed repository. Upgrades now arrive with the rest of the system."
# shellcheck disable=SC2016 # `oxidom status` is a command to type, not one to run
say 'oxidom also needs an Xray core, which no distribution packages. Run `oxidom status` — it names'
say 'the exact download for this machine. The system daemon is deliberately not enabled: see'
say "$PAGES for what that decides, or docs/installation.md in the repository."
