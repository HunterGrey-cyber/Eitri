#!/bin/sh
# Puts the agent sidecar where Eitri finds it on macOS (Apple silicon).
#
# `packaging/install.sh` builds the sidecar for Linux only: Verdandi's `buildBinary.mjs` refuses
# darwin, and grpc-tools ships an x86_64-only `protoc`. This script is the macOS version of that one
# step. It installs to the same per-user place `agent::user_sidecar_path` computes,
#   $XDG_DATA_HOME/eitri/sidecar/<rev>/verdandi-claude-sidecar   (+ BUILD)
# where an unset, empty or relative XDG_DATA_HOME means $HOME/.local/share, and <rev> is the 7-character
# `agent::BUNDLED_SIDECAR_REVISION`.
set -eu

NODE_VERSION=v22.23.2
# From https://nodejs.org/dist/v22.23.2/SHASUMS256.txt (node-v22.23.2-darwin-arm64.tar.xz). Verdandi's
# pin (apps/claude-sidecar/node-pin.json) has no darwin entry, so this value is the script's, not Verdandi's.
NODE_DARWIN_ARM64_SHA256=5eff7a9011895aae3f29d06f167b84a62b028a591370c7cafb59103559fd26e1
PROTOCOL_MAJOR=3

usage() {
	cat <<'EOF'
usage: setup-sidecar.sh --artifact FILE [--verdandi-rev REV] [options]
       setup-sidecar.sh --verdandi-checkout DIR [options]

Installs the sidecar from this client's bundled source revision (agent::BUNDLED_SIDECAR_REVISION).

  --artifact FILE            an already built verdandi-claude-sidecar for darwin-arm64. Its revision is the
                             git HEAD of the directory it sits in, or --verdandi-rev when it is not in a
                             checkout. Nothing is built.
  --verdandi-checkout DIR    build in this Verdandi checkout (needs node >= 22, npm, curl, protoc, codesign)
                             and install the result. The checkout's tracked files are never edited: the
                             build runs a patched COPY of scripts/buildBinary.mjs (removed afterwards).
  --verdandi-rev REV         the revision the artifact was built from (artifact mode, outside a checkout): 7 to 40
                             lowercase hex digits
  An artifact inside a checkout is taken to be built from that checkout's HEAD: nothing in the artifact names the
  revision it was built from. An untracked build output that outlived a checkout is not detected, so prefer
  --verdandi-checkout, which builds from the revision it checks.
  --allow-dirty              build or accept a checkout with modified tracked files
  --skip-pressure-check      build even when macOS reports memory pressure (the build peaks near 0.9 GiB)
  -h, --help                 this text

Refuses when the revision is not the expected one, when the artifact does not report protocol 3 on Node
22.23.2 serving host_cli only, or when the platform is not darwin-arm64.

What the build needs on macOS, and does about it, all outside tracked files:
  * a darwin-arm64 checksum for the pinned Node tarball (built into this script, taken from nodejs.org);
    the tarball is downloaded to build/node-cache, verified, and unpacked there;
  * the SEA blob is made by that pinned Node (a blob is specific to the Node version that makes it);
  * Mach-O injection: codesign --remove-signature, postject --macho-segment-name NODE_SEA, then an ad-hoc
    codesign --sign - (no strip);
  * grpc-tools' bin/protoc is x86_64-only: node_modules/grpc-tools/bin/protoc is replaced by a link to the
    protoc on PATH (e.g. brew install protoc); the original is kept as protoc.x86_64.orig.

The data directory is chosen as Eitri chooses it: XDG_DATA_HOME if absolute, else $HOME/.local/share.
EOF
}

die() {
	printf 'setup-sidecar: %s\n' "$*" >&2
	exit 1
}
say() { printf 'setup-sidecar: %s\n' "$*" >&2; }

ARTIFACT=
CHECKOUT=
REV_OPT=
ALLOW_DIRTY=0
SKIP_PRESSURE=0
while [ $# -gt 0 ]; do
	case $1 in
	--artifact | --verdandi-checkout | --verdandi-rev)
		[ $# -ge 2 ] || die "$1 needs a value"
		case $1 in
		--artifact) ARTIFACT=$2 ;;
		--verdandi-checkout) CHECKOUT=$2 ;;
		--verdandi-rev) REV_OPT=$2 ;;
		esac
		shift
		;;
	--allow-dirty) ALLOW_DIRTY=1 ;;
	--skip-pressure-check) SKIP_PRESSURE=1 ;;
	-h | --help)
		usage
		exit 0
		;;
	*) die "unknown argument $1 (see --help)" ;;
	esac
	shift
done

# Whatever a run makes and does not finish is removed on any exit, a refusal included: the unpack directory, the
# patched build script, the copy being checked, and the sidecar directory if it is still empty.
UNPACK=
COPY=
TMP=
DEST=
cleanup() {
	[ -z "$UNPACK" ] || rm -rf -- "$UNPACK"
	[ -z "$COPY" ] || rm -f -- "$COPY"
	[ -z "$TMP" ] || rm -f -- "$TMP"
	[ -z "$DEST" ] || rmdir -- "$DEST" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

[ -n "$ARTIFACT" ] || [ -n "$CHECKOUT" ] || {
	usage >&2
	exit 2
}
[ -z "$ARTIFACT" ] || [ -z "$CHECKOUT" ] || die "give --artifact or --verdandi-checkout, not both"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || die "this is the darwin-arm64 step; on Linux use packaging/install.sh"

# The revision this client expects, read from the source that defines it.
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
SPAWN_RS=$HERE/../../agent/src/providers/claude_sidecar/spawn.rs
[ -f "$SPAWN_RS" ] || die "cannot find $SPAWN_RS: run this from an Eitri checkout"
EXPECTED=$(sed -n 's/^pub const BUNDLED_SIDECAR_REVISION: &str = "\([0-9a-f]\{7,40\}\)";$/\1/p' "$SPAWN_RS")
[ -n "$EXPECTED" ] || die "cannot read BUNDLED_SIDECAR_REVISION from $SPAWN_RS"

# Where Eitri looks (agent::user_sidecar_path): an absolute XDG_DATA_HOME wins, else $HOME/.local/share.
case ${XDG_DATA_HOME-} in
*'
'*) die "XDG_DATA_HOME holds a newline" ;;
/*) DATA=$XDG_DATA_HOME ;;
*)
	case ${HOME-} in /*) ;; *) die "HOME is not an absolute path" ;; esac
	DATA=$HOME/.local/share
	;;
esac

# rev_matches ACTUAL: the same prefix rule the client uses for a checkout's revision.
rev_matches() {
	case $1 in
	"$EXPECTED"*) return 0 ;;
	esac
	case $EXPECTED in
	"$1"*) [ -n "$1" ] && return 0 ;;
	esac
	return 1
}

# The checkout the artifact comes from, when there is one.
if [ -n "$ARTIFACT" ]; then
	[ -f "$ARTIFACT" ] || die "$ARTIFACT is not a file"
	SRC_DIR=$(CDPATH= cd -- "$(dirname -- "$ARTIFACT")" && pwd -P)
	ARTIFACT=$SRC_DIR/$(basename -- "$ARTIFACT")
	if git -C "$SRC_DIR" rev-parse --show-toplevel >/dev/null 2>&1; then
		GIT_DIR_FOR_REV=$SRC_DIR
	else
		GIT_DIR_FOR_REV=
	fi
else
	[ -d "$CHECKOUT" ] || die "$CHECKOUT is not a directory"
	CHECKOUT=$(CDPATH= cd -- "$CHECKOUT" && pwd -P)
	[ -f "$CHECKOUT/apps/claude-sidecar/package.json" ] || die "$CHECKOUT has no apps/claude-sidecar/package.json: not a Verdandi checkout"
	GIT_DIR_FOR_REV=$CHECKOUT
fi

ACTUAL=
if [ -n "$GIT_DIR_FOR_REV" ]; then
	ACTUAL=$(git -C "$GIT_DIR_FOR_REV" rev-parse HEAD 2>/dev/null) || ACTUAL=
	if [ "$ALLOW_DIRTY" != 1 ] && [ -n "$(git -C "$GIT_DIR_FOR_REV" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
		die "$GIT_DIR_FOR_REV has modified tracked files, so its revision does not describe what it builds (--allow-dirty to go on)"
	fi
fi
if [ -n "$REV_OPT" ]; then
	case $REV_OPT in
	*[!0-9a-f]* | "") die "--verdandi-rev $REV_OPT is not a revision (7 to 40 lowercase hex digits)" ;;
	esac
	[ ${#REV_OPT} -ge 7 ] && [ ${#REV_OPT} -le 40 ] || die "--verdandi-rev $REV_OPT is not 7 to 40 hex digits"
fi
if [ -z "$ACTUAL" ]; then
	[ -n "$REV_OPT" ] || die "cannot tell which Verdandi revision $ARTIFACT was built from: it is not in a git checkout, so pass --verdandi-rev"
	ACTUAL=$REV_OPT
elif [ -n "$REV_OPT" ] && [ "$REV_OPT" != "$ACTUAL" ] && ! rev_matches "$REV_OPT"; then
	die "--verdandi-rev $REV_OPT disagrees with the checkout's HEAD $ACTUAL"
fi
rev_matches "$ACTUAL" || die "Verdandi revision $ACTUAL is not the one this Eitri expects ($EXPECTED): refusing to install it"
say "revision $ACTUAL matches the expected $EXPECTED"

if [ -n "$CHECKOUT" ]; then
	command -v node >/dev/null 2>&1 || die "node is not on PATH"
	command -v npm >/dev/null 2>&1 || die "npm is not on PATH"
	command -v curl >/dev/null 2>&1 || die "curl is not on PATH"
	command -v codesign >/dev/null 2>&1 || die "codesign is not on PATH"
	if [ "$SKIP_PRESSURE" != 1 ]; then
		level=$(sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null || echo 1)
		[ "$level" = 1 ] || die "memory pressure level is $level, not 1: free some memory and re-run (the build peaks near 0.9 GiB), or --skip-pressure-check"
	fi
	SC=$CHECKOUT/apps/claude-sidecar
	if [ ! -d "$CHECKOUT/node_modules" ]; then
		say "npm ci"
		(cd -- "$CHECKOUT" && npm ci --no-audit --no-fund) || die "npm ci failed"
	fi
	# grpc-tools' protoc is x86_64-only and there is no Rosetta here: link the one on PATH.
	PROTOC_LINK=$CHECKOUT/node_modules/grpc-tools/bin/protoc
	if [ -e "$PROTOC_LINK" ] || [ -L "$PROTOC_LINK" ]; then
		if ! "$PROTOC_LINK" --version >/dev/null 2>&1; then
			command -v protoc >/dev/null 2>&1 || die "grpc-tools' protoc cannot run on this Mac and there is no protoc on PATH (brew install protoc)"
			[ -L "$PROTOC_LINK" ] || mv -- "$PROTOC_LINK" "$PROTOC_LINK.x86_64.orig"
			ln -sfn "$(command -v protoc)" "$PROTOC_LINK"
			say "grpc-tools' protoc replaced by a link to $(command -v protoc) (original kept as protoc.x86_64.orig)"
		fi
	fi
	# The pinned Node, verified, unpacked where buildBinary looks for it.
	CACHE=$SC/build/node-cache
	TARBALL=node-$NODE_VERSION-darwin-arm64.tar.xz
	mkdir -p -- "$CACHE"
	if [ ! -f "$CACHE/$TARBALL" ]; then
		say "downloading $TARBALL"
		curl -fsSL -o "$CACHE/$TARBALL.part" "https://nodejs.org/dist/$NODE_VERSION/$TARBALL" || die "could not download $TARBALL"
		mv -- "$CACHE/$TARBALL.part" "$CACHE/$TARBALL"
	fi
	have=$(shasum -a 256 "$CACHE/$TARBALL" | awk '{ print $1 }')
	if [ "$have" != "$NODE_DARWIN_ARM64_SHA256" ]; then
		rm -f -- "$CACHE/$TARBALL"
		die "$TARBALL sha256 is $have, not $NODE_DARWIN_ARM64_SHA256: removed it, refusing to build around an unverified Node"
	fi
	# Unpacked fresh from the verified tarball every time: a tree left from an earlier run is not what was verified.
	NODE_TREE=$CACHE/node-$NODE_VERSION-darwin-arm64
	UNPACK=$CACHE/.unpack.$$
	rm -rf -- "$UNPACK"
	mkdir -p -- "$UNPACK"
	tar xf "$CACHE/$TARBALL" -C "$UNPACK" || die "could not unpack $TARBALL"
	[ -x "$UNPACK/node-$NODE_VERSION-darwin-arm64/bin/node" ] || die "$TARBALL holds no bin/node"
	rm -rf -- "$NODE_TREE"
	mv -- "$UNPACK/node-$NODE_VERSION-darwin-arm64" "$NODE_TREE"
	rm -rf -- "$UNPACK"

	# The checksum above is for this Node only: a pin that names another version needs a new one here.
	PIN=$SC/node-pin.json
	[ -f "$PIN" ] || die "$PIN is missing: the build script's Node pin moved"
	pinned=$(node -e 'process.stdout.write(String(JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).version))' "$PIN") ||
		die "cannot read the version from $PIN"
	[ "$pinned" = "$NODE_VERSION" ] || die "$PIN pins Node $pinned, but this script's darwin-arm64 checksum is for $NODE_VERSION"

	# A patched copy of the build script, next to the original (it resolves paths from its own place).
	ORIG=$SC/scripts/buildBinary.mjs
	COPY=$SC/scripts/buildBinary.darwin.mjs
	[ -f "$ORIG" ] || die "$ORIG is missing"
	SHA=$NODE_DARWIN_ARM64_SHA256 perl -pe '
		s/^(const \{ version: NODE_VERSION, tarballSha256: NODE_TARBALL_SHA256 \} = loadNodePin\(workspaceRoot\);)$/$1\nNODE_TARBALL_SHA256[\x27darwin-arm64\x27] = \x27$ENV{SHA}\x27;/;
		s/^run\(process\.execPath, \[.--experimental-sea-config., seaConfigPath\]\);/run(join(cacheDir, `node-\${NODE_VERSION}-\${platform}`, \x27bin\x27, \x27node\x27), [\x27--experimental-sea-config\x27, seaConfigPath]);/;
		s/^  run\(.strip., \[outPath\]\);/  run(\x27codesign\x27, [\x27--remove-signature\x27, outPath]);/;
		s/^(run\(.npx., \[.postject., outPath, .NODE_SEA_BLOB., blobPath, .--sentinel-fuse., SEA_FUSE)\]\);/$1, \x27--macho-segment-name\x27, \x27NODE_SEA\x27]);\nrun(\x27codesign\x27, [\x27--sign\x27, \x27-\x27, outPath]);/;
	' "$ORIG" >"$COPY"
	# Every edit must have landed: a build script that moved leaves a half-patched copy.
	grep -q "^NODE_TARBALL_SHA256\['darwin-arm64'\] = '$NODE_DARWIN_ARM64_SHA256';\$" "$COPY" || die "the build script's Node pin moved: cannot add darwin-arm64"
	grep -q "node-\${NODE_VERSION}-\${platform}\`, 'bin', 'node'), \['--experimental-sea-config'" "$COPY" || die "the build script's SEA blob step moved"
	grep -q "run('codesign', \['--remove-signature', outPath\]);" "$COPY" || die "the build script's strip step moved"
	grep -q -- "--macho-segment-name" "$COPY" || die "the build script's inject step moved"
	grep -q "run('codesign', \['--sign', '-', outPath\]);" "$COPY" || die "the build script's inject step moved"

	say "building (a minute or less)"
	started=$(date +%s)
	(cd -- "$CHECKOUT" && node "$COPY") || die "the sidecar build failed"
	say "built in $(($(date +%s) - started)) s"
	ARTIFACT=$(ls -t "$SC"/dist-bin/verdandi-claude-sidecar-*-darwin-arm64 2>/dev/null | sed -n 1p)
	[ -n "$ARTIFACT" ] || die "the build left no darwin-arm64 artifact in $SC/dist-bin"
fi

# The copy that will be installed is the one checked: copied first, beside its final place.
REV7=$(printf '%s' "$EXPECTED" | cut -c1-7)
DEST=$DATA/eitri/sidecar/$REV7
mkdir -p -- "$DEST"
TMP=$DEST/.verdandi-claude-sidecar.tmp.$$
cp -- "$ARTIFACT" "$TMP"
chmod 0755 "$TMP"

# Acceptance: the copy's own --version, as install.sh checks the artifact.
out=$("$TMP" --version 2>&1) || die "the artifact ($ARTIFACT) does not run: $out"
line1=$(printf '%s\n' "$out" | sed -n 1p)
line4=$(printf '%s\n' "$out" | sed -n 4p)
case $line1 in
*"(protocol $PROTOCOL_MAJOR, node $NODE_VERSION,"*) ;;
*) die "the artifact reports \"$line1\", not protocol $PROTOCOL_MAJOR on node $NODE_VERSION: refusing to install it" ;;
esac
[ "$line4" = 'executable sources served: host_cli' ] || die "the artifact serves \"$line4\", not host_cli only: refusing to install it"

# Install: the directory is keyed by the expected revision, BUILD goes last.
mv -- "$TMP" "$DEST/verdandi-claude-sidecar"
printf 'VERDANDI_REV=%s\nSIDECAR_VERSION_LINE=%s\nBUILT_AT=%s\n' "$ACTUAL" "$line1" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$DEST/BUILD"
say "installed $DEST/verdandi-claude-sidecar ($line1)"
printf '%s\n' "$DEST/verdandi-claude-sidecar"
