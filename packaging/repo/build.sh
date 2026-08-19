#!/usr/bin/env bash
# Build an apt and an rpm repository out of the packages already published to
# GitHub Releases, so that installing oxidom is `apt install oxidom-gui` rather
# than downloading two files and naming both on the command line.
#
# The releases are the source of truth and this tree is a derived view: every
# run rebuilds the whole thing from what the releases contain. Nothing is
# carried over between runs, so there is no accumulated state to drift, and a
# deleted or corrected release simply stops appearing.
#
# Pre-releases are skipped. A repository is what a package manager upgrades to
# without being asked, which is not where release candidates belong.
#
#   packaging/repo/build.sh <output-dir>
#
# Signing: apt refuses a repository it cannot verify, so $REPO_SIGNING_KEY must
# hold an ASCII-armoured private key. Without it this exits rather than
# publishing something nobody can install.
set -euo pipefail

out=${1:?usage: build.sh <output-dir>}
repo=${GITHUB_REPOSITORY:-keepinfov/oxidom}
pages_url=${PAGES_URL:-https://keepinfov.github.io/oxidom}

need() { command -v "$1" >/dev/null || { echo "error: $1 is required" >&2; exit 1; }; }
for t in gpg apt-ftparchive createrepo_c rpm; do need "$t"; done
[ -n "${REPO_PKG_DIR:-}" ] || need gh

[ -n "${REPO_SIGNING_KEY:-}" ] || {
    echo "error: REPO_SIGNING_KEY is empty. apt will not use an unsigned" >&2
    echo "       repository, so publishing one would only look like it worked." >&2
    exit 1
}

work=$(mktemp -d)
trap 'rm -rf "$work"; gpgconf --kill gpg-agent 2>/dev/null || true' EXIT

echo "== importing the signing key =="
export GNUPGHOME="$work/gnupg"
mkdir -p "$GNUPGHOME"; chmod 700 "$GNUPGHOME"
printf '%s' "$REPO_SIGNING_KEY" | gpg --batch --quiet --import
key=$(gpg --list-secret-keys --with-colons | awk -F: '/^fpr:/{print $10; exit}')
[ -n "$key" ] || { echo "error: no secret key after import" >&2; exit 1; }
echo "signing as $key"

echo "== collecting packages =="
mkdir -p "$work/pkgs"
if [ -n "${REPO_PKG_DIR:-}" ]; then
    # Used by the test, and by anyone wanting to see what a repository would
    # look like before there is a release to build one from.
    echo "  from $REPO_PKG_DIR"
    cp "$REPO_PKG_DIR"/*.deb "$REPO_PKG_DIR"/*.rpm "$work/pkgs/" 2>/dev/null || true
else
# --exclude-pre-releases keeps candidates out; a draft has no assets to fetch.
tags=$(gh release list --repo "$repo" --exclude-pre-releases --exclude-drafts \
         --limit 100 --json tagName --jq '.[].tagName')
[ -n "$tags" ] || { echo "error: no published releases to build a repository from" >&2; exit 1; }
for tag in $tags; do
    echo "  $tag"
    gh release download "$tag" --repo "$repo" --pattern '*.deb' --dir "$work/pkgs" --clobber 2>/dev/null || true
    gh release download "$tag" --repo "$repo" --pattern '*.rpm' --dir "$work/pkgs" --clobber 2>/dev/null || true
done
fi
find "$work/pkgs" -type f -printf '  found %f\n' | sort

rm -rf "$out"; mkdir -p "$out"
gpg --armor --export "$key" > "$out/KEY.gpg"

echo "== apt =="
# pool/ holds the files; dists/ describes them. `apt-ftparchive packages` writes
# a Filename: relative to the directory it is run from, which is why it runs at
# the repository root rather than beside the pool.
deb_root="$out/deb"
mkdir -p "$deb_root/pool/main/o/oxidom" "$deb_root/dists/stable/main/binary-amd64"
cp "$work"/pkgs/*.deb "$deb_root/pool/main/o/oxidom/" 2>/dev/null || {
    echo "error: no .deb found in any published release" >&2; exit 1; }

( cd "$deb_root"
  apt-ftparchive packages pool > dists/stable/main/binary-amd64/Packages
  gzip -9kf dists/stable/main/binary-amd64/Packages
  apt-ftparchive \
      -o APT::FTPArchive::Release::Origin=oxidom \
      -o APT::FTPArchive::Release::Label=oxidom \
      -o APT::FTPArchive::Release::Suite=stable \
      -o APT::FTPArchive::Release::Codename=stable \
      -o APT::FTPArchive::Release::Architectures=amd64 \
      -o APT::FTPArchive::Release::Components=main \
      -o APT::FTPArchive::Release::Description="oxidom packages" \
      release dists/stable > dists/stable/Release
  # Both forms: InRelease is what modern apt fetches, Release.gpg is the
  # detached signature older clients still look for.
  gpg --batch --yes --default-key "$key" --clearsign -o dists/stable/InRelease dists/stable/Release
  gpg --batch --yes --default-key "$key" -abs -o dists/stable/Release.gpg dists/stable/Release
)

echo "== rpm =="
rpm_root="$out/rpm"
mkdir -p "$rpm_root"
cp "$work"/pkgs/*.rpm "$rpm_root/" 2>/dev/null || {
    echo "error: no .rpm found in any published release" >&2; exit 1; }

# Sign each package, so gpgcheck=1 means something, and then the metadata, so
# repo_gpgcheck=1 does too. Signing only the metadata would still authenticate
# the packages through their checksums, but it makes the .repo file say
# gpgcheck=0, which reads as weaker than it is and invites someone to relax it
# further.
cat > "$work/rpmmacros" <<MACROS
%_gpg_name $key
%__gpg $(command -v gpg)
%_gpg_digest_algo sha256
MACROS
cp "$work/rpmmacros" "$work/.rpmmacros"
for f in "$rpm_root"/*.rpm; do
    HOME="$work" rpm --addsign "$f" > /dev/null
    # rpm reports the file name whether or not a signature was written, so ask
    # the package instead of believing the tool. The tag is RSAHEADER; SIGPGP
    # reads "(none)" on a correctly signed package and would quietly pass a
    # check written against it.
    sig=$(rpm -qp --qf '%{RSAHEADER:pgpsig}' "$f" 2>/dev/null)
    case "$sig" in
        ""|"(none)")
            echo "error: $(basename "$f") came back unsigned" >&2
            exit 1 ;;
    esac
    echo "  signed $(basename "$f") — $sig"
done

createrepo_c --quiet --update "$rpm_root"
gpg --batch --yes --default-key "$key" --detach-sign --armor "$rpm_root/repodata/repomd.xml"

cat > "$out/oxidom.repo" <<REPO
[oxidom]
name=oxidom
baseurl=$pages_url/rpm
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=$pages_url/KEY.gpg
REPO

echo "== landing page =="
# The page a browser gets. It is the same instructions the release notes carry,
# except that from here the package manager does the rest.
cat > "$out/index.html" <<HTML
<!doctype html><meta charset=utf-8><title>oxidom packages</title>
<meta name=viewport content="width=device-width,initial-scale=1">
<style>
 body{max-width:44rem;margin:3rem auto;padding:0 1.2rem;line-height:1.6;
      font-family:system-ui,sans-serif}
 pre{background:#f4f4f5;padding:.9rem 1rem;overflow-x:auto;border-radius:6px}
 code{font-size:.95em} h2{margin-top:2.2rem}
 @media(prefers-color-scheme:dark){body{background:#18181b;color:#e4e4e7}
   pre{background:#27272a} a{color:#7dd3fc}}
</style>
<h1>oxidom packages</h1>
<p>A signed apt and rpm repository, so upgrades arrive with the rest of the
system. Source and releases:
<a href="https://github.com/$repo">github.com/$repo</a>.</p>

<h2>Debian, Ubuntu</h2>
<pre>curl -fsSL $pages_url/KEY.gpg | sudo gpg --dearmor -o /usr/share/keyrings/oxidom.gpg
echo "deb [signed-by=/usr/share/keyrings/oxidom.gpg] $pages_url/deb stable main" \\
  | sudo tee /etc/apt/sources.list.d/oxidom.list
sudo apt update
sudo apt install oxidom-gui        # interface; pulls in the daemon
sudo apt install oxidom            # daemon and CLI only, for a server</pre>

<h2>Fedora, RHEL</h2>
<pre>sudo curl -fsSL $pages_url/oxidom.repo -o /etc/yum.repos.d/oxidom.repo
sudo dnf install oxidom-gui</pre>

<h2>An Xray core is still needed</h2>
<p>No distribution packages one, so neither package can depend on it. oxidom
starts without a core and Settings names the exact download for your machine.
The AppImage on the releases page has one inside it already.</p>

<h2>Interface and daemon</h2>
<p><code>oxidom-gui</code> is a thin client: it holds no privileges and asks
<code>oxidom</code>, which carries the daemon, to do the work. Installing the
interface pulls the daemon in. The daemon alone has no GTK dependency and
installs on releases too old for the interface.</p>
HTML

echo "== done =="
find "$out" -maxdepth 3 -type d | sort
