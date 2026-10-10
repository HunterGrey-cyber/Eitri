#!/bin/sh
# Builds Eitri.app: the eitri-mac binary as Contents/MacOS/Eitri, the `eitri` command in Contents/Resources/bin,
# the icon and its licence notice, an Info.plist from mac/bundle/Info.plist, all signed ad hoc (codesign -s -).
set -eu

usage() {
	cat <<'EOF'
usage: bundle.sh [--out DIR] [--bundle-id ID] [--acceptance] [--debug] [--skip-pressure-check]

  --out DIR              where Eitri.app goes (default: <repo>/target/eitri-app). /Applications, ~/Applications
                         and the system's own directories are refused: installing is a copy the user makes. So is
                         everything under /private and /var, which takes in /tmp and $TMPDIR (both resolve there).
  --bundle-id ID         CFBundleIdentifier (default cn.huntergrey.eitri); a fresh one gives a first-ever start,
                         as a new install has, for checks of macOS's own prompts
  --acceptance           an acceptance build (cargo feature `acceptance`: a diagnostic socket, links recorded and
                         never opened). Without this flag a binary carrying it is refused.
  --debug                bundle the debug build instead of the release build
  --skip-pressure-check  build even when macOS reports memory pressure
  -h, --help             this text

Refuses while any process runs from the bundle it would replace.
EOF
}

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
out=$repo/target/eitri-app
bundle_id=cn.huntergrey.eitri
acceptance=no
profile=release
pressure_check=yes
while [ $# -gt 0 ]; do
	case $1 in
	--out) out=${2:?--out needs a directory}; shift 2 ;;
	--bundle-id) bundle_id=${2:?--bundle-id needs an id}; shift 2 ;;
	--acceptance) acceptance=yes; shift ;;
	--debug) profile=debug; shift ;;
	--skip-pressure-check) pressure_check=no; shift ;;
	-h | --help) usage; exit 0 ;;
	*) echo "bundle.sh: unknown argument $1" >&2; usage >&2; exit 2 ;;
	esac
done

fail() {
	echo "bundle.sh: $*" >&2
	exit 1
}

[ "$(uname -s)" = Darwin ] || fail "builds a macOS app; this is $(uname -s)"
case $bundle_id in
*[!A-Za-z0-9.-]* | "" | .* | *.) fail "the bundle id $bundle_id is not of letters, digits, '-' and '.'" ;;
esac
# Never into an Applications folder or a system directory, by any spelling: no `.` or `..` component (its
# directory may not exist yet, so it could not be resolved), the existing part resolved through symlinks, and the
# comparison case-folded (an APFS volume is case-insensitive by default, and `pwd -P` keeps the case it was given).
while :; do
	case $out in
	./*) out=${out#./} ;;
	*) break ;;
	esac
done
case /$out/ in
*/./* | */../*) fail "--out $out has a . or .. component: name the directory plainly" ;;
esac
lower() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }
refuse_place() {
	for refused in /Applications "${HOME:-/nonexistent}/Applications" /System /Library /usr /bin /sbin /private /etc \
		/var; do
		case $(lower "$1/") in
		"$(lower "$refused")"/*) fail "--out $out is inside $refused: bundle elsewhere and copy the app yourself" ;;
		esac
	done
}
case $out in /*) ;; *) out=$(pwd -P)/$out ;; esac
probe=$out
while [ ! -d "$probe" ]; do probe=$(dirname -- "$probe"); done
refuse_place "$(CDPATH= cd -- "$probe" && pwd -P)${out#"$probe"}"
mkdir -p -- "$out"
out=$(CDPATH= cd -- "$out" && pwd -P)
# Again on the directory as it now exists, before anything is written into it.
refuse_place "$out"
app=$out/Eitri.app

# What a run leaves on any exit: cargo's messages and the half-made bundle and icon set, never the bundle.
messages=
stage=
iconset=
cleanup() {
	[ -z "$messages" ] || rm -f -- "$messages"
	[ -z "$stage" ] || rm -rf -- "$stage"
	[ -z "$iconset" ] || rm -rf -- "$iconset"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# A process started from the bundle would keep running the old binary while its files are replaced under it.
# `comm` is the executable's own path, so this matches what runs from the bundle and not a command line that
# merely names it (this check's own included). Checked before the build, and again just before the swap.
refuse_if_running() {
	running=$(ps -axww -o pid= -o comm= | awk -v prefix="$app/" '{ pid = $1; sub(/^ *[0-9]+ +/, ""); if (index($0, prefix) == 1) print pid }')
	[ -z "$running" ] || fail "processes run from $app (pids: $(echo $running)); quit them first"
}
refuse_if_running

if [ "$pressure_check" = yes ]; then
	level=$(sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null || echo 1)
	[ "$level" = 1 ] || fail "macOS reports memory pressure (level $level); wait, or pass --skip-pressure-check"
fi

version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n 1)
[ -n "$version" ] || fail "no [workspace.package] version in $repo/Cargo.toml"
# The bundle's version keys take one to three numbers: `0.2.2-rc.1` is `0.2.2` there, and the full version is
# kept in a key of Eitri's own.
numeric=${version%%[-+]*}
case $numeric in
*[!0-9.]* | "" | .* | *. | *..*) fail "the version $version does not start with MAJOR.MINOR.PATCH" ;;
esac

set -- build -p eitri-mac
[ "$profile" = release ] && set -- "$@" --release
[ "$acceptance" = yes ] && set -- "$@" --features acceptance
echo "bundle.sh: cargo $*"
# Cargo names the binary it built (its target directory and target triple may be set anywhere).
messages=$(mktemp -t eitri-cargo-messages)
(cd "$repo" && cargo "$@" --message-format=json-render-diagnostics) >"$messages"
binary=$(/usr/bin/python3 -c '
import json, sys
for line in open(sys.argv[1]):
    try:
        m = json.loads(line)
    except ValueError:
        continue
    t = m.get("target") or {}
    if m.get("reason") == "compiler-artifact" and t.get("name") == "eitri-mac" and "bin" in t.get("kind", []) and m.get("executable"):
        print(m["executable"])
' "$messages" | tail -n 1)
[ -n "$binary" ] && [ -x "$binary" ] || fail "cargo named no eitri-mac executable"

# The marker only an acceptance build carries (pinned by tests/acceptance_marker.rs); it must match what was asked.
if LC_ALL=C grep -a -q 'eitri-acceptance-build-marker-1' "$binary"; then
	[ "$acceptance" = yes ] || fail "$binary carries the acceptance feature; pass --acceptance to bundle it"
else
	[ "$acceptance" = no ] || fail "$binary lacks the acceptance feature that --acceptance asks for"
fi

stage=$out/.Eitri.app.building
rm -rf -- "$stage"
mkdir -p -- "$stage/Contents/MacOS" "$stage/Contents/Resources/bin"
cp -- "$binary" "$stage/Contents/MacOS/Eitri"
cp -- "$repo/mac/bundle/eitri" "$stage/Contents/Resources/bin/eitri"
chmod 755 "$stage/Contents/MacOS/Eitri" "$stage/Contents/Resources/bin/eitri"
sed -e "s/@BUNDLE_ID@/$bundle_id/g" -e "s/@VERSION@/$version/g" -e "s/@NUMERIC_VERSION@/$numeric/g" \
	"$repo/mac/bundle/Info.plist" >"$stage/Contents/Info.plist"
# The copy is the build cargo named: the same Mach-O UUID (signing below rewrites bytes, never the UUID).
uuid=$(dwarfdump --uuid "$binary" | awk '{ print $2 }')
[ -n "$uuid" ] || fail "$binary has no Mach-O UUID"
[ "$uuid" = "$(dwarfdump --uuid "$stage/Contents/MacOS/Eitri" | awk '{ print $2 }')" ] ||
	fail "the copied binary is not the one cargo built"
plutil -lint "$stage/Contents/Info.plist" >/dev/null || fail "the Info.plist is not valid"

# The icon from the committed PNGs (packaging/icons); there is no 1024 px one, so the 512@2x slot stays empty.
icons=$repo/packaging/icons/hicolor
iconset=$out/.Eitri.iconset
rm -rf -- "$iconset"
mkdir -p -- "$iconset"
for pair in 16:icon_16x16 32:icon_16x16@2x 32:icon_32x32 64:icon_32x32@2x 128:icon_128x128 256:icon_128x128@2x \
	256:icon_256x256 512:icon_256x256@2x 512:icon_512x512; do
	size=${pair%%:*}
	cp -- "$icons/${size}x${size}/apps/cn.huntergrey.eitri.png" "$iconset/${pair#*:}.png"
done
iconutil -c icns -o "$stage/Contents/Resources/Eitri.icns" "$iconset"
rm -rf -- "$iconset"
cp -- "$repo/packaging/icons/LICENSE" "$stage/Contents/Resources/LICENSE-icon.txt"

codesign --force --sign - --timestamp=none "$stage"
codesign --verify --strict "$stage"

refuse_if_running
rm -rf -- "$app"
mv -- "$stage" "$app"
echo "bundle.sh: $app ($bundle_id $version, $profile$( [ "$acceptance" = yes ] && echo ', acceptance'))"
