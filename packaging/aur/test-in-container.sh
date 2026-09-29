#!/usr/bin/env bash
# Builds, checks and installs the binary and source packages in clean archlinux:latest
# containers, then runs --version. Overrides source= with local test inputs rather than
# requiring published release assets and tags.
#
# The binary package's release tarball and source archive are served from --release-dir by
# `python3 -m http.server` on 127.0.0.1. --network host lets the container reach that server.
# The runtime archive still comes from its public download URL.
#
# The source package's three git sources are local repositories mounted read-only at
# /mirrors/, with source= rewritten to git+file:///mirrors/.... The application checkout
# gets --tag in a temporary clone. The editor mirror must contain the integration branch
# with the checkout's submodule revision, and the agent mirror must contain the revision
# pinned in agent/Cargo.toml. Both mirrors are checked before the container starts and
# need no tags. Runtime and pinned crate downloads still use their public URLs.
#
# Use --release-dir for the release assets and pass the application checkout, editor mirror
# and agent mirror directories. Optional flags are --tag TAG, --work DIR, --only bin|git
# and --jobs N. For another release, use its assets and matching checkout and mirrors.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

RELEASE_DIR=""
NEOVIBE_CHECKOUT=""
NEOVIDE_MIRROR=""
VERDANDI_MIRROR=""
TAG=""
WORK="${HOME}/.cache/nv-v1dist-aur"
ONLY=""
JOBS="4"
IMAGE="archlinux:latest"

die() {
	echo "test-in-container.sh: $*" >&2
	exit 1
}

while [ $# -gt 0 ]; do
	case "$1" in
	--release-dir)
		RELEASE_DIR=$2
		shift 2
		;;
	--neovibe-checkout)
		NEOVIBE_CHECKOUT=$2
		shift 2
		;;
	--neovide-mirror)
		NEOVIDE_MIRROR=$2
		shift 2
		;;
	--verdandi-mirror)
		VERDANDI_MIRROR=$2
		shift 2
		;;
	--tag)
		TAG=$2
		shift 2
		;;
	--work)
		WORK=$2
		shift 2
		;;
	--only)
		ONLY=$2
		shift 2
		;;
	--jobs)
		JOBS=$2
		shift 2
		;;
	*)
		die "unknown argument: $1"
		;;
	esac
done

[ -n "$RELEASE_DIR" ] || die "--release-dir is required"
[ -f "$RELEASE_DIR/RELEASE" ] || die "$RELEASE_DIR has no RELEASE"
[ -f "$RELEASE_DIR/SHA256SUMS" ] || die "$RELEASE_DIR has no SHA256SUMS"

case "$WORK" in
/tmp | /tmp/*) die "--work must not be under /tmp (a small shared tmpfs)" ;;
esac

VERSION="$(sed -n 's/^NEOVIBE_VERSION=//p' "$RELEASE_DIR/RELEASE")"
[ -n "$VERSION" ] || die "$RELEASE_DIR/RELEASE has no NEOVIBE_VERSION"
[ -n "$TAG" ] || TAG="v$VERSION"

rm -rf -- "$WORK"
mkdir -p -- "$WORK"

HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

# The background `python3 -m http.server` test_bin starts, reaped by one EXIT trap installed here
# for the whole script. HTTP_PID is a plain global and must stay one, never `local` inside
# test_bin: when `set -e` exits the script from inside a function (run_arch failing, the likely
# first-run outcome), bash has already torn that function's local variables down by the time the
# EXIT trap runs at top level, so a `local` pid reads as unset there -- reproduced against this
# script with a stub `docker` whose `run` fails: "http_pid: unbound variable", and the server left
# bound to its port for the next run to trip over. A RETURN trap is no
# better: it does not fire at all on a `set -e` unwind.
HTTP_PID=""
stop_http_server() {
	if [ -n "$HTTP_PID" ]; then
		kill "$HTTP_PID" 2>/dev/null || true
		wait "$HTTP_PID" 2>/dev/null || true
		HTTP_PID=""
	fi
}
trap stop_http_server EXIT

# Reuse pacman's package cache across runs in a named Docker volume.
# This avoids leaving root-owned cache files in a bind-mounted host directory.
docker volume create nv-aur-pacman-cache >/dev/null

run_arch() {
	# run_arch NAME LOG DOCKER_ARGS... -- SCRIPT: one --rm archlinux:latest container, root by
	# default (pacman -Syu and pacman -U need it), with the pinned rc.1-window PKGBUILD dir
	# bind-mounted rw at /build and the pacman cache volume mounted. DOCKER_ARGS are extra
	# `docker run` arguments (test_bin's --network host, test_git's read-only source mounts).
	# SCRIPT runs as bash -c.
	local name="$1" log="$2"
	shift 2
	local docker_args=()
	while [ "$1" != -- ]; do docker_args+=("$1"); shift; done
	shift
	local script="$1"
	docker run --rm --init --name "$name" \
		--mount "type=bind,src=$WORK/build,dst=/build" \
		--mount "type=volume,src=nv-aur-pacman-cache,dst=/var/cache/pacman/pkg" \
		-e "HOST_UID=$HOST_UID" -e "HOST_GID=$HOST_GID" -e "CARGO_BUILD_JOBS=$JOBS" \
		"${docker_args[@]}" "$IMAGE" bash -c "$script" 2>&1 | tee "$log"
}

# The container is started as root (below, plain `docker run`, no --user): pacman -Syu and the
# eventual `pacman -U` both need it, and makepkg -s escalates through sudo to install missing
# makedepends/depends as it goes. The actual build itself never runs as root -- every `makepkg`/
# `namcap` invocation below is `runuser -u builder`, a user created here at the caller's own host
# uid/gid, so nothing this container writes onto the bind-mounted /build is root-owned (spec sec
# 2.4's hygiene intent, met through runuser rather than through `docker run --user`, since a single
# container needs to be root for pacman -Syu/-U and non-root for makepkg in the same run).
# shellcheck disable=SC2016 # expanded by the container's bash -c, not here
ARCH_SETUP='
set -euo pipefail
pacman -Syu --noconfirm --needed base-devel namcap sudo
getent group "$HOST_GID" >/dev/null || groupadd -g "$HOST_GID" builder
getent passwd "$HOST_UID" >/dev/null || useradd -u "$HOST_UID" -g "$HOST_GID" -m -d /home/builder builder
echo "builder ALL=(ALL) NOPASSWD: ALL" >/etc/sudoers.d/builder
chmod 0440 /etc/sudoers.d/builder
chown -R "$HOST_UID:$HOST_GID" /build
'

# build_and_check PKGDIR LOG_PREFIX: makepkg --printsrcinfo|diff, namcap PKGBUILD, makepkg -s,
# namcap the built package, all as the host-uid builder (makepkg refuses root outright).
BUILD_AND_CHECK='
set -euo pipefail
cd /build/PKGDIR
echo "--- makepkg --printsrcinfo vs .SRCINFO ---"
runuser -u builder -- bash -c "cd /build/PKGDIR && makepkg --printsrcinfo" | diff - /build/PKGDIR/.SRCINFO
echo "--- namcap PKGBUILD ---"
runuser -u builder -- bash -c "cd /build/PKGDIR && namcap PKGBUILD"
echo "--- makepkg -s ---"
runuser -u builder -- bash -c "cd /build/PKGDIR && makepkg -s --noconfirm"
echo "--- namcap *.pkg.tar.zst ---"
runuser -u builder -- bash -c "cd /build/PKGDIR && namcap ./*.pkg.tar.zst"
echo "--- pacman -U ---"
pacman -U --noconfirm /build/PKGDIR/*.pkg.tar.zst
echo "--- neovibe --version ---"
neovibe --version
echo "--- verdandi-claude-sidecar --version ---"
/usr/lib/neovibe/verdandi-claude-sidecar --version
echo "--- licence directory ---"
ls -la /usr/share/licenses/PKGNAME/
test -f /usr/share/licenses/PKGNAME/LICENSE.md
'

test_bin() {
	echo "== neovibe-bin =="
	mkdir -p -- "$WORK/build/neovibe-bin"
	cp -- "$SCRIPT_DIR/neovibe-bin/PKGBUILD" "$WORK/build/neovibe-bin/PKGBUILD"
	"$SCRIPT_DIR/bump-bin.sh" "$RELEASE_DIR" --allow-prerelease --pkgbuild-dir "$WORK/build/neovibe-bin"

	# Test-only source= override: the two neovibe.git assets come from a local HTTP server over
	# the release dir (read-only: never modifies --release-dir); Node is still the real nodejs.org.
	local port=18080
	sed -i -E "s#https://github.com/HunterGrey-cyber/neovibe/releases/download/v[^/]+/#http://127.0.0.1:$port/#g" \
		"$WORK/build/neovibe-bin/PKGBUILD"
	(cd "$WORK/build/neovibe-bin" && makepkg --printsrcinfo >.SRCINFO)

	python3 -m http.server "$port" --bind 127.0.0.1 --directory "$RELEASE_DIR" >"$WORK/http-server-bin.log" 2>&1 &
	# The global, not a local: see stop_http_server's comment -- the EXIT trap is what reaps this
	# server when run_arch below fails.
	HTTP_PID=$!

	local script="$ARCH_SETUP"$'\n'"${BUILD_AND_CHECK//PKGDIR/neovibe-bin}"
	script="${script//PKGNAME/neovibe-bin}"
	run_arch "nv-aur-bin-$$" "$WORK/log-bin.log" --network host -- "$script"

	stop_http_server
	echo "neovibe-bin: OK"
}

test_git() {
	echo "== neovibe-git =="
	[ -n "$NEOVIBE_CHECKOUT" ] || die "--neovibe-checkout is required for --only git (or the default, both)"
	[ -n "$NEOVIDE_MIRROR" ] || die "--neovide-mirror is required for --only git (or the default, both)"
	[ -n "$VERDANDI_MIRROR" ] || die "--verdandi-mirror is required for --only git (or the default, both)"
	NEOVIDE_MIRROR="$(realpath -- "$NEOVIDE_MIRROR")"
	VERDANDI_MIRROR="$(realpath -- "$VERDANDI_MIRROR")"

	# Tag a throwaway clone so pkgver()'s git describe has a tag, including for prerelease tests.
	# Leave the input checkout untouched. Preserve an existing $TAG only if it names the
	# commit being built.
	rm -rf -- "$WORK/neovibe-tagged.git"
	git clone --quiet --bare "$NEOVIBE_CHECKOUT" "$WORK/neovibe-tagged.git"
	local head tagged
	head="$(git -C "$WORK/neovibe-tagged.git" rev-parse HEAD)"
	if tagged="$(git -C "$WORK/neovibe-tagged.git" rev-parse -q --verify "refs/tags/$TAG^{commit}")"; then
		[ "$tagged" = "$head" ] || die "--neovibe-checkout already has $TAG at $tagged, not at its HEAD $head"
	else
		git -C "$WORK/neovibe-tagged.git" tag "$TAG" "$head"
	fi

	# Fail here, in seconds, rather than an hour into the container build: the two mirrors must
	# hold what this neovibe commit pins (prepare() checks both out by commit, and makepkg checks
	# the neovide mirror out by the PKGBUILD's #branch=neovibe-integration).
	local fork_commit verdandi_rev
	fork_commit="$(git -C "$WORK/neovibe-tagged.git" rev-parse "$head:neovide")"
	verdandi_rev="$(git -C "$WORK/neovibe-tagged.git" show "$head:agent/Cargo.toml" |
		sed -n 's/^claude-runtime-protocol[[:space:]]*=.*rev[[:space:]]*=[[:space:]]*"\([0-9a-f]*\)".*/\1/p')"
	[ -n "$verdandi_rev" ] || die "$NEOVIBE_CHECKOUT's agent/Cargo.toml pins no claude-runtime-protocol rev"
	git -C "$NEOVIDE_MIRROR" rev-parse --verify --quiet refs/heads/neovibe-integration >/dev/null ||
		die "--neovide-mirror $NEOVIDE_MIRROR has no neovibe-integration branch (the PKGBUILD's #branch=)"
	git -C "$NEOVIDE_MIRROR" merge-base --is-ancestor "$fork_commit" refs/heads/neovibe-integration ||
		die "--neovide-mirror $NEOVIDE_MIRROR: submodule commit $fork_commit is not on neovibe-integration"
	git -C "$VERDANDI_MIRROR" cat-file -e "$verdandi_rev^{commit}" 2>/dev/null ||
		die "--verdandi-mirror $VERDANDI_MIRROR does not have the pinned Verdandi $verdandi_rev"
	if [ "$head" != "$(sed -n 's/^NEOVIBE_COMMIT=//p' "$RELEASE_DIR/RELEASE")" ]; then
		echo "note: --neovibe-checkout HEAD $head is not $RELEASE_DIR's NEOVIBE_COMMIT" >&2
	fi

	# Mount the three git sources read-only and rewrite source= to their container paths.
	# makepkg runs inside the container and cannot clone from unmounted paths on the machine
	# running this script.
	mkdir -p -- "$WORK/build/neovibe-git"
	cp -- "$SCRIPT_DIR/neovibe-git/PKGBUILD" "$WORK/build/neovibe-git/PKGBUILD"
	sed -i \
		-e "s#git+https://github.com/HunterGrey-cyber/neovibe.git#git+file:///mirrors/neovibe.git#" \
		-e "s#git+https://github.com/HunterGrey-cyber/neovide.git#git+file:///mirrors/neovide.git#" \
		-e "s#git+https://github.com/HunterGrey-cyber/verdandi.git#git+file:///mirrors/verdandi.git#" \
		"$WORK/build/neovibe-git/PKGBUILD"
	if grep -n 'git+https://' "$WORK/build/neovibe-git/PKGBUILD"; then
		die "a git source= was not rewritten to a local mirror (above)"
	fi
	(cd "$WORK/build/neovibe-git" && makepkg --printsrcinfo >.SRCINFO)

	local script="$ARCH_SETUP"$'\n'"${BUILD_AND_CHECK//PKGDIR/neovibe-git}"
	script="${script//PKGNAME/neovibe-git}"
	# Use the default bridge network for cargo, npm, the Skia archive and Node downloads.
	# The git+file:// source mounts need no network access and no host network mode.
	run_arch "nv-aur-git-$$" "$WORK/log-git.log" \
		--mount "type=bind,src=$WORK/neovibe-tagged.git,dst=/mirrors/neovibe.git,readonly" \
		--mount "type=bind,src=$NEOVIDE_MIRROR,dst=/mirrors/neovide.git,readonly" \
		--mount "type=bind,src=$VERDANDI_MIRROR,dst=/mirrors/verdandi.git,readonly" \
		-- "$script"
	echo "neovibe-git: OK"
}

case "$ONLY" in
bin) test_bin ;;
git) test_git ;;
"")
	test_bin
	test_git
	;;
*) die "--only must be bin or git" ;;
esac

echo "namcap output: $WORK/log-bin.log, $WORK/log-git.log"
