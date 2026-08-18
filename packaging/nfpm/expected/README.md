# Expected file lists

What each package installs, one path per line, sorted. The `.deb` and the `.rpm`
carry the same set, so there is one list per package rather than one per format.

These exist because a package silently losing a file is invisible until somebody
installs it. It has happened once already — `d121d87 fix(packaging): the filter
icon ships with the Arch package` — and the symptom was an empty square where a
button's icon should be, several steps away from the packaging change that
caused it.

Regenerate after deliberately adding or removing a file:

    dpkg --contents dist/oxidom_*.deb | awk '$1 !~ /^d/ {print $6}' \
      | sed 's|^\./|/|' | sort > packaging/nfpm/expected/oxidom.txt
