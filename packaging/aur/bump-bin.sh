#!/usr/bin/env bash
# packaging/aur/bump-bin.sh -- fills packaging/aur/neovibe-bin/PKGBUILD's pkgver, _verdandi_source,
# _nodever and sha256sums from a real release directory (RELEASE + SHA256SUMS, plan Task 12's
# release.sh output), then regenerates .SRCINFO. Never runs git -- it only ever *prints* the commit an
# operator would run by hand, because the AUR is public git with visible authorship and the
# global git identity on this machine is private (spec sec 9).
#
# Usage: packaging/aur/bump-bin.sh <release-dir> [--allow-prerelease] [--pkgbuild-dir DIR]
#
#   <release-dir>        holds RELEASE and SHA256SUMS (plan Task 12's release.sh output, e.g.
#                         ~/.cache/neovibe-release/v1.0.0/).
#   --allow-prerelease    Fill the PKGBUILD/.SRCINFO anyway when the release's own NEOVIBE_VERSION
#                         is a prerelease (contains "-rc."). Only ever pass this for
#                         packaging/aur/test-in-container.sh's own use: neovibe-bin is not
#                         published to the AUR for a prerelease version (see the note at the top
#                         of neovibe-bin/PKGBUILD, and
#                         the private review notes #3) -- pacman's
#                         own version comparison sorts a `-rc.N` pkgver ABOVE the final release
#                         that follows it, stranding every rc installer with no upgrade path.
#   --pkgbuild-dir DIR    Operate on DIR instead of the sibling neovibe-bin/ directory (testing
#                         only -- keeps a prerelease bump from dirtying the tracked PKGBUILD).
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PKGBUILD_DIR="$SCRIPT_DIR/neovibe-bin"
ALLOW_PRERELEASE=0
RELEASE_DIR=""

usage() {
	sed -n '2,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
	case "$1" in
	--allow-prerelease)
		ALLOW_PRERELEASE=1
		shift
		;;
	--pkgbuild-dir)
		[ $# -ge 2 ] || {
			echo "bump-bin.sh: --pkgbuild-dir needs a value" >&2
			exit 1
		}
		PKGBUILD_DIR="$2"
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	-*)
		echo "bump-bin.sh: unknown option: $1" >&2
		exit 1
		;;
	*)
		if [ -n "$RELEASE_DIR" ]; then
			echo "bump-bin.sh: unexpected argument: $1 (release dir already given: $RELEASE_DIR)" >&2
			exit 1
		fi
		RELEASE_DIR="$1"
		shift
		;;
	esac
done

if [ -z "$RELEASE_DIR" ]; then
	usage >&2
	exit 1
fi

RELEASE_FILE="$RELEASE_DIR/RELEASE"
SUMS_FILE="$RELEASE_DIR/SHA256SUMS"
PKGBUILD_FILE="$PKGBUILD_DIR/PKGBUILD"
SRCINFO_FILE="$PKGBUILD_DIR/.SRCINFO"

[ -f "$RELEASE_FILE" ] || {
	echo "bump-bin.sh: no RELEASE at $RELEASE_FILE" >&2
	exit 1
}
[ -f "$SUMS_FILE" ] || {
	echo "bump-bin.sh: no SHA256SUMS at $SUMS_FILE" >&2
	exit 1
}
[ -f "$PKGBUILD_FILE" ] || {
	echo "bump-bin.sh: no PKGBUILD at $PKGBUILD_FILE" >&2
	exit 1
}

kv() {
	# kv KEY: one value from RELEASE, or die naming the key.
	local v
	v=$(sed -n "s/^$1=//p" "$RELEASE_FILE")
	[ -n "$v" ] || {
		echo "bump-bin.sh: $RELEASE_FILE has no $1" >&2
		exit 1
	}
	printf '%s\n' "$v"
}

sum_of() {
	# sum_of FILENAME: the sha256 SHA256SUMS records for FILENAME, or die naming it.
	local v
	v=$(awk -v f="$1" '$2==f {print $1; found=1} END{if(!found) exit 1}' "$SUMS_FILE") || {
		echo "bump-bin.sh: $SUMS_FILE has no entry for $1" >&2
		exit 1
	}
	printf '%s\n' "$v"
}

VERSION="$(kv NEOVIBE_VERSION)"
VERDANDI_SOURCE="$(kv VERDANDI_SOURCE)"
NODE_VERSION="$(kv NODE_VERSION)"
NODE_SHA256_X64="$(kv NODE_SHA256_linux_x64)"
# Both go into the PKGBUILD's sed below; refuse anything that is not a plain Node version or hash
# rather than let a malformed RELEASE write a broken (or sed-injected) PKGBUILD.
[[ "$NODE_VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
	echo "bump-bin.sh: $RELEASE_FILE's NODE_VERSION is not a vX.Y.Z version: $NODE_VERSION" >&2
	exit 1
}
[[ "$NODE_SHA256_X64" =~ ^[0-9a-f]{64}$ ]] || {
	echo "bump-bin.sh: $RELEASE_FILE's NODE_SHA256_linux_x64 is not a sha256: $NODE_SHA256_X64" >&2
	exit 1
}

case "$VERSION" in
*-rc.*) IS_PRERELEASE=1 ;;
*) IS_PRERELEASE=0 ;;
esac

if [ "$IS_PRERELEASE" = 1 ] && [ "$ALLOW_PRERELEASE" != 1 ]; then
	cat >&2 <<EOF
bump-bin.sh: $VERSION is a prerelease (-rc.N). neovibe-bin is never published to the AUR for a
prerelease version: its pkgver ('-' -> '_') sorts ABOVE the eventual final release under pacman's
own vercmp (vercmp 1.0.0_rc.1-1 1.0.0-1 => 1), stranding every -rc.N installer above 1.0.0 with no
upgrade path (the private review notes #3).

Pass --allow-prerelease only to build and test this version locally
(packaging/aur/test-in-container.sh); the result must never be pushed to the AUR.
EOF
	exit 1
fi

TARBALL_SHA="$(sum_of "neovibe-${VERSION}-x86_64-linux.tar.gz")"
VERDANDI_SHA="$(sum_of "$VERDANDI_SOURCE")"

PKGVER="${VERSION//-/_}"

sed -i \
	-E \
	-e "s/^pkgver=.*/pkgver=$PKGVER/" \
	-e "s/^_realver=.*/_realver=$VERSION/" \
	-e "s/^_verdandi_source=.*/_verdandi_source=$VERDANDI_SOURCE/" \
	-e "s/^_nodever=.*/_nodever=$NODE_VERSION/" \
	"$PKGBUILD_FILE"
# The Node URL and its sha256 must move together (the third sha256sums entry below): a PKGBUILD
# without the _nodever line would keep its old URL beside the new hash, which makepkg only reports
# later as a checksum failure.
grep -qxF "_nodever=$NODE_VERSION" "$PKGBUILD_FILE" || {
	echo "bump-bin.sh: $PKGBUILD_FILE has no _nodever= line to set to $NODE_VERSION (RELEASE's NODE_VERSION)" >&2
	exit 1
}

# The sha256sums array's three entries (tarball, verdandi source, node), matched by position
# within the array, not by content, since a re-run must replace whatever value is already there
# (the placeholder zeros the first time, a previous release's real hashes after that).
python3 - "$PKGBUILD_FILE" "$TARBALL_SHA" "$VERDANDI_SHA" "$NODE_SHA256_X64" <<'PY'
import re
import sys

path, tarball_sha, verdandi_sha, node_sha = sys.argv[1:5]
text = open(path, encoding="utf-8").read()

m = re.search(r"sha256sums=\(\s*'[0-9a-f]{64}'\s*\n\s*'[0-9a-f]{64}'\s*\n\s*'[0-9a-f]{64}'\)", text)
if not m:
    sys.exit(f"bump-bin.sh: could not find a 3-entry sha256sums array in {path}")

lines = m.group(0).splitlines()
assert len(lines) == 3, lines
lines[0] = re.sub(r"'[0-9a-f]{64}'", f"'{tarball_sha}'", lines[0])
lines[1] = re.sub(r"'[0-9a-f]{64}'", f"'{verdandi_sha}'", lines[1])
lines[2] = re.sub(r"'[0-9a-f]{64}'", f"'{node_sha}'", lines[2])
replacement = "\n".join(lines)
open(path, "w", encoding="utf-8").write(text[: m.start()] + replacement + text[m.end() :])
PY

if ! command -v makepkg >/dev/null 2>&1; then
	echo "bump-bin.sh: PKGBUILD/.SRCINFO updated for $VERSION, but no makepkg on PATH: run makepkg --printsrcinfo > $SRCINFO_FILE yourself (in packaging/aur/test-in-container.sh's archlinux container, or a local Arch install)" >&2
	exit 0
fi

(cd "$PKGBUILD_DIR" && makepkg --printsrcinfo >"$SRCINFO_FILE")

if [ "$IS_PRERELEASE" = 1 ]; then
	cat <<EOF
bump-bin.sh: $PKGBUILD_FILE and $SRCINFO_FILE updated for $VERSION (prerelease, --allow-prerelease).
This is for packaging/aur/test-in-container.sh only -- do not commit or push this to the AUR.
EOF
	exit 0
fi

# Identity isolation for the printed commit (never run here): GIT_AUTHOR_*/GIT_COMMITTER_* as an
# env-var prefix, which genuinely overrides an inherited identity (unlike `git -c user.name=...`,
# which environment variables outrank -- reproduced in
# the private review notes #4), plus the same
# hooksPath/gpgsign/template/--no-verify isolation and after-the-fact identity check
# publish/commit.sh already uses for this project's own public-history commits.
cat <<EOF
$PKGBUILD_FILE and $SRCINFO_FILE updated for $VERSION.

To publish, in a clone of ssh://aur@aur.archlinux.org/neovibe-bin.git (a separate git history --
never this repo):

  cp $PKGBUILD_FILE $SRCINFO_FILE <that clone>/
  cd <that clone>
  git add PKGBUILD .SRCINFO
  GIT_AUTHOR_NAME='Hunter Grey' GIT_AUTHOR_EMAIL=71165939+HunterGrey-cyber@users.noreply.github.com \\
  GIT_COMMITTER_NAME='Hunter Grey' GIT_COMMITTER_EMAIL=71165939+HunterGrey-cyber@users.noreply.github.com \\
    git -c core.hooksPath=/dev/null -c commit.gpgsign=false -c commit.template= \\
    commit --no-verify -m "neovibe-bin $VERSION"
  [ "\$(git log -1 --format='%an|%ae|%cn|%ce')" = \\
    'Hunter Grey|71165939+HunterGrey-cyber@users.noreply.github.com|Hunter Grey|71165939+HunterGrey-cyber@users.noreply.github.com' ] \\
    || { echo 'IDENTITY MISMATCH -- do not push' >&2; exit 1; }
  git push origin HEAD:master
EOF
