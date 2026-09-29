#!/usr/bin/env bash
# packaging/tests/e2e/run.sh -- plan 2026-09-27-v1-dist, Task 15: the installer end to end, in
# clean containers (spec sec 6, 5.3, 7, 15). Not part of the pytest suite (it is heavy, needs
# docker and real internet, and builds a real sidecar per distro) -- run it by hand or from a
# workflow, never from `python3 -m pytest packaging`.
#
# What this proves, per distro image, against a real release directory served on the host's own
# 127.0.0.1 (spec sec 6.2's own loopback exception -- the installer accepts plain http only there):
#   - the curl-piped and saved-then-`sh`-run install paths both work, non-interactively, as a
#     non-root user, with `--with-nvim`;
#   - the real `ssh-keygen -Y verify` path runs (codex verdict #5: openssh-client is in every image,
#     and this script greps for the non-degraded "signature on SHA256SUMS: good" line, so a future
#     accidental removal of openssh-client fails loudly instead of silently degrading to
#     checksum-only);
#   - spec sec 6.5's install layout, `neovibe --version`, and the built sidecar's own `--version`
#     (host_cli, protocol 3, the pinned Node);
#   - on Ubuntu's own 0.9.5-nvim variant: neovibe's private nvim is fetched and used, and the
#     distro's /usr/bin/nvim is byte-for-byte untouched;
#   - a second run says "up to date"; `neovibe --legacy` / `NEOVIBE_AGENT_BACKEND=legacy neovibe`
#     both exit 1 naming the reason (plan Task 5); `--uninstall` leaves nothing outside
#     ~/.config/neovibe and the state directory; there is no `sudo` in any image, so a maintainer
#     script that tried to use it would fail loudly, and there is no maintainer script at all (REL-2
#     below);
#   - REL-1: `apt install ./neovibe_*.deb` / `dnf install ./neovibe-*.rpm` as root, then
#     `neovibe setup --yes` as the created non-root user, builds the sidecar reading only
#     /usr/lib/neovibe/RELEASE -- proved behaviourally, not just read off the source: the mirror
#     `neovibe setup` is pointed at carries no SHA256SUMS at all, so a fetch of it would 404 and the
#     run would fail;
#   - REL-2: the installed package ran no maintainer script, and `neovibe setup` refuses as root.
#
# Usage: packaging/tests/e2e/run.sh --release-dir DIR [--release-signers FILE]
#                                   [--only NAME[,NAME...]] [--out DIR]
#   --release-dir      a finished, signed release directory (release.sh's output: RELEASE,
#                      SHA256SUMS, SHA256SUMS.sig and the assets RELEASE lists). Required: there is
#                      no default, so a run always names the release it tested. Only ever read.
#   --release-signers  the allowed_signers file the installer checks SHA256SUMS.sig against
#                      (install.sh --release-signers), bind-mounted read-only into every curl-flow
#                      container. Default: DIR/release-signers. A real release directory carries none
#                      (release.sh never writes one) -- pass the signers file of the key it was
#                      signed with, e.g. a release candidate's release.sh --release-signers FILE.
#   --only             a comma-separated subset of: ubuntu-no-nvim, ubuntu-0.9.5, ubuntu-piped,
#                      ubuntu-pkg, fedora, fedora-pkg, arch (default: all)
#   --out              where logs and the summary go (default ~/.cache/nv-v1dist-t15/results)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"

RELEASE_DIR=""
RELEASE_SIGNERS=""
ONLY=""
OUT="$HOME/.cache/nv-v1dist-t15/results"

while [ $# -gt 0 ]; do
	case "$1" in
	--release-dir)
		RELEASE_DIR="$2"
		shift 2
		;;
	--release-signers)
		RELEASE_SIGNERS="$2"
		shift 2
		;;
	--only)
		ONLY="$2"
		shift 2
		;;
	--out)
		OUT="$2"
		shift 2
		;;
	-h | --help)
		sed -n '2,43p' "$0"
		exit 0
		;;
	*)
		echo "run.sh: unknown option: $1" >&2
		exit 2
		;;
	esac
done

case "$OUT" in
/tmp | /tmp/*) echo "run.sh: --out must not be under /tmp (a small shared tmpfs)" >&2; exit 2 ;;
esac

log() { printf '[run.sh %(%H:%M:%S)T] %s\n' -1 "$*"; }
die() {
	printf 'run.sh: %s\n' "$*" >&2
	exit 1
}

[ -n "$RELEASE_DIR" ] || die "--release-dir DIR is required (see --help)"
[ -d "$RELEASE_DIR" ] || die "--release-dir $RELEASE_DIR: no such directory"
# Absolute from here on: the mirror below is symlinks into it, and docker --mount needs an absolute src.
RELEASE_DIR="$(cd -- "$RELEASE_DIR" && pwd -P)"
[ -f "$RELEASE_DIR/RELEASE" ] && [ -f "$RELEASE_DIR/SHA256SUMS" ] && [ -f "$RELEASE_DIR/SHA256SUMS.sig" ] ||
	die "--release-dir $RELEASE_DIR: not a finished, signed release directory (needs RELEASE, SHA256SUMS, SHA256SUMS.sig)"
if [ -n "$RELEASE_SIGNERS" ]; then
	[ -f "$RELEASE_SIGNERS" ] || die "--release-signers $RELEASE_SIGNERS: no such file"
else
	RELEASE_SIGNERS="$RELEASE_DIR/release-signers"
	[ -f "$RELEASE_SIGNERS" ] ||
		die "no $RELEASE_SIGNERS: release.sh writes no signers file into a release directory, so pass --release-signers FILE (the allowed_signers file of the key that signed SHA256SUMS)"
fi
RELEASE_SIGNERS="$(cd -- "$(dirname -- "$RELEASE_SIGNERS")" && pwd -P)/$(basename -- "$RELEASE_SIGNERS")"
VERSION="$(sed -n 's/^NEOVIBE_VERSION=//p' "$RELEASE_DIR/RELEASE")"
[ -n "$VERSION" ] || die "$RELEASE_DIR/RELEASE has no NEOVIBE_VERSION"
REV7="$(sed -n 's/^VERDANDI_REV=\(.......\).*/\1/p' "$RELEASE_DIR/RELEASE" | head -n1)"
[ -n "$REV7" ] || die "$RELEASE_DIR/RELEASE has no VERDANDI_REV"

command -v docker >/dev/null 2>&1 || die "docker is not on PATH"
UID_H="$(id -u)"
GID_H="$(id -g)"

should_run() {
	[ -z "$ONLY" ] && return 0
	case ",$ONLY," in *",$1,"*) return 0 ;; esac
	return 1
}

# Never `docker pull`/`--pull`: the three base images must already be on this host.
for img in ubuntu:24.04 fedora:44 archlinux:latest; do
	docker image inspect "$img" >/dev/null 2>&1 || die "base image $img is not present locally, and this script never pulls: build/pull it by hand first, or report the gap instead of pulling"
done

mkdir -p -- "$OUT"
SCRATCH="$HOME/.cache/nv-v1dist-t15/e2e"
rm -rf -- "$SCRATCH"
mkdir -p -- "$SCRATCH/serve/full/releases/latest/download" "$SCRATCH/serve/full/releases/download/v$VERSION" \
	"$SCRATCH/serve/setuponly/releases/download/v$VERSION" "$SCRATCH/homes" "$SCRATCH/scripts"

log "preparing the release mirror from $RELEASE_DIR"
# releases/latest/download/ + releases/download/v<X>/ (GitHub's own layout, spec sec 6.2's
# --base-url doc) plus a bare install.sh for the piped example -- symlinks, so the ~150MB source
# asset is never copied.
for f in "$RELEASE_DIR"/*; do
	name="$(basename "$f")"
	ln -s -- "$f" "$SCRATCH/serve/full/releases/download/v$VERSION/$name"
done
for f in RELEASE SHA256SUMS SHA256SUMS.sig "neovibe-$VERSION-x86_64-linux.tar.gz" install.sh; do
	ln -s -- "$RELEASE_DIR/$f" "$SCRATCH/serve/full/releases/latest/download/$f"
done
ln -s -- "$RELEASE_DIR/install.sh" "$SCRATCH/serve/full/install.sh"
# The REL-1 mirror: only the Verdandi source asset, deliberately no SHA256SUMS anywhere under it --
# `neovibe setup` succeeding against this mirror is the behavioural proof that it never fetches one.
ln -s -- "$RELEASE_DIR/verdandi-$REV7-source.tar.gz" \
	"$SCRATCH/serve/setuponly/releases/download/v$VERSION/verdandi-$REV7-source.tar.gz"

PORT=$((20000 + (UID_H % 20000)))
log "serving $SCRATCH/serve on 127.0.0.1:$PORT"
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$SCRATCH/serve" >"$OUT/http-server.log" 2>&1 &
HTTPD_PID=$!
cleanup() { kill "$HTTPD_PID" 2>/dev/null || true; }
trap cleanup EXIT
for _ in $(seq 1 50); do
	curl -fsS "http://127.0.0.1:$PORT/full/releases/latest/download/RELEASE" >/dev/null 2>&1 && break
	sleep 0.2
done
curl -fsS "http://127.0.0.1:$PORT/full/releases/latest/download/RELEASE" >/dev/null ||
	die "the local release mirror never came up on 127.0.0.1:$PORT"

BASE_URL="http://127.0.0.1:$PORT/full"
SETUP_BASE_URL="http://127.0.0.1:$PORT/setuponly"
SIGNERS_HOST="$RELEASE_SIGNERS"
log "release $RELEASE_DIR, signers $SIGNERS_HOST"

EXPECT_VERSION_LINE="neovibe $VERSION (commit $(sed -n 's/^NEOVIBE_COMMIT=//p' "$RELEASE_DIR/RELEASE" | cut -c1-12), neovide fork $(sed -n 's/^NEOVIDE_FORK_COMMIT=//p' "$RELEASE_DIR/RELEASE" | cut -c1-7), verdandi $REV7)"

PASS=0
FAIL=0
FAILED_NAMES=""
RESULTS="$OUT/summary.tsv"
: >"$RESULTS"

check() {
	# check LABEL COND-DESCRIPTION -- CMD...  (CMD's exit status is the verdict)
	local label="$1"
	shift
	if "$@" >/dev/null 2>&1; then
		printf 'ok\n'
		return 0
	fi
	printf 'FAIL: %s\n' "$label" >&2
	return 1
}

record() {
	local scenario="$1" ok="$2"
	if [ "$ok" = 1 ]; then
		PASS=$((PASS + 1))
		printf '%s\tPASS\n' "$scenario" >>"$RESULTS"
		log "PASS  $scenario"
	else
		FAIL=$((FAIL + 1))
		FAILED_NAMES="$FAILED_NAMES $scenario"
		printf '%s\tFAIL\n' "$scenario" >>"$RESULTS"
		log "FAIL  $scenario"
	fi
}

# ================================================================================================
# The shared curl-flow driver, written once into $SCRATCH/scripts and bind-mounted read-only into
# every curl-flow container (ubuntu-no-nvim, ubuntu-0.9.5, ubuntu-piped, fedora, arch). It never
# takes the base URL, signers path etc. as literals baked into an image -- all of it arrives as env
# vars the container run sets, so one script covers every distro and both Ubuntu nvim variants.
cat >"$SCRATCH/scripts/curl-flow.sh" <<'DRIVER'
#!/bin/sh
# Env in: BASE_URL SIGNERS VERSION EXPECT_VERSION_LINE REV7 PIPED(0/1) EXPECT_DISTRO_NVIM(0/1)
set -eu
ok=1
say() { printf '[driver] %s\n' "$*"; }
fail() {
	printf '[driver] ASSERT FAILED: %s\n' "$*"
	ok=0
}

say "id: $(id); uname: $(uname -srm)"
[ -f /etc/os-release ] && say "os-release: $(sed -n '1,3p' /etc/os-release | tr '\n' ' ')"
if command -v sudo >/dev/null 2>&1; then fail "sudo is on PATH in this image; it must not be"; else say "sudo: absent (ok)"; fi

XDG_DATA_HOME=${XDG_DATA_HOME:-$HOME/.local/share}
SIDECAR_BIN="$XDG_DATA_HOME/neovibe/sidecar/$REV7/verdandi-claude-sidecar"
LAUNCHER="$HOME/.local/bin/neovibe"

if [ "$EXPECT_DISTRO_NVIM" = 1 ]; then
	command -v nvim >/dev/null 2>&1 || fail "the 0.9.5 variant has no distro nvim on PATH at all"
	DISTRO_NVIM=$(command -v nvim)
	DISTRO_NVIM_SHA_BEFORE=$(sha256sum "$DISTRO_NVIM" | awk '{print $1}')
	say "distro nvim: $DISTRO_NVIM ($("$DISTRO_NVIM" --version | head -n1))"
fi

say "=== first install ==="
# Always fetch a local copy too, PIPED or not: it proves nothing about the piped run itself (spec
# sec 6.1's SH-3, stdin carries the whole script) and is only for every step after this one, which
# needs a script on disk regardless of how the FIRST install ran.
curl -fsS "$BASE_URL/install.sh" -o install.sh
if [ "$PIPED" = 1 ]; then
	if ! curl -fsS "$BASE_URL/install.sh" | sh -s -- --base-url "$BASE_URL" \
		--release-signers "$SIGNERS" --yes --with-nvim >first-install.log 2>&1; then
		fail "the piped first install exited non-zero"
	fi
else
	if ! sh install.sh --base-url "$BASE_URL" --release-signers "$SIGNERS" --yes --with-nvim \
		>first-install.log 2>&1; then
		fail "the first install exited non-zero"
	fi
fi
cat first-install.log
if ! grep -qF 'signature on SHA256SUMS: good (release@neovibe)' first-install.log; then
	fail "no 'signature on SHA256SUMS: good' line: the real ssh-keygen -Y verify path did not run cleanly (codex verdict #5)"
fi
if grep -qi 'ssh-keygen was not found' first-install.log; then
	fail "install.sh degraded to checksum-only (ssh-keygen missing) -- openssh-client is not doing its job"
fi

say "disk usage after the first install (sidecar + nvim included, before --uninstall reclaims it): $(du -sh "$HOME" 2>/dev/null | awk '{print $1}')"

say "=== spec sec 6.5 layout ==="
for p in shell neovibe-supervisor neovibe-tmux-shim neovibe-claude-handoff neovibe-setup RELEASE; do
	[ -f "$HOME/.local/lib/neovibe/$p" ] || fail "missing $HOME/.local/lib/neovibe/$p"
done
[ -f "$LAUNCHER" ] || fail "missing launcher $LAUNCHER"
grep -qF '# neovibe-launcher v1' "$LAUNCHER" || fail "$LAUNCHER carries no marker line"
[ -f "$HOME/.local/share/applications/neovibe.desktop" ] || fail "missing the desktop entry"
for p in LICENSE THIRD-PARTY-LICENSES SOURCE; do
	[ -f "$HOME/.local/share/licenses/neovibe/$p" ] || fail "missing licences/$p"
done
[ -x "$SIDECAR_BIN" ] || fail "missing sidecar binary $SIDECAR_BIN"
[ -f "$XDG_DATA_HOME/neovibe/sidecar/$REV7/BUILD" ] || fail "missing sidecar BUILD file"

say "=== neovibe --version ==="
GOT_VERSION=$("$LAUNCHER" --version)
say "got: $GOT_VERSION"
[ "$GOT_VERSION" = "$EXPECT_VERSION_LINE" ] || fail "neovibe --version = '$GOT_VERSION', expected '$EXPECT_VERSION_LINE'"

say "=== sidecar --version ==="
SC_VERSION=$("$SIDECAR_BIN" --version)
say "$SC_VERSION"
printf '%s\n' "$SC_VERSION" | sed -n 1p | grep -qF "(protocol 3, node v22.23.2," ||
	fail "sidecar --version's first line does not name protocol 3, node v22.23.2"
printf '%s\n' "$SC_VERSION" | sed -n 4p | grep -qFx 'executable sources served: host_cli' ||
	fail "sidecar --version's 4th line is not 'executable sources served: host_cli'"

say "=== nvim offer ==="
NVIM_DIR="$XDG_DATA_HOME/neovibe/nvim/0.11.2"
[ -x "$NVIM_DIR/bin/nvim" ] || fail "the pinned nvim 0.11.2 was not fetched into $NVIM_DIR"
if [ -x "$NVIM_DIR/bin/nvim" ]; then
	NVOUT=$("$NVIM_DIR/bin/nvim" --version 2>&1 | head -n1)
	say "private nvim --version: $NVOUT"
	printf '%s\n' "$NVOUT" | grep -qF 'NVIM v0.11.2' || fail "the private nvim reports '$NVOUT', not NVIM v0.11.2"
	if ! ldd "$NVIM_DIR/bin/nvim" >ldd-nvim.log 2>&1; then
		fail "ldd against the private nvim failed on this distro's glibc"
	fi
	if grep -qi 'not found' ldd-nvim.log; then
		fail "the private nvim has an unresolved shared library on this distro: $(cat ldd-nvim.log)"
	fi
fi
if [ "$EXPECT_DISTRO_NVIM" = 1 ]; then
	DISTRO_NVIM_SHA_AFTER=$(sha256sum "$DISTRO_NVIM" | awk '{print $1}')
	[ "$DISTRO_NVIM_SHA_BEFORE" = "$DISTRO_NVIM_SHA_AFTER" ] || fail "$DISTRO_NVIM changed sha256 during install: neovibe touched the distro nvim"
	command -v nvim >/dev/null 2>&1 && [ "$(command -v nvim)" = "$DISTRO_NVIM" ] || fail "nvim no longer resolves to the distro copy on PATH"
fi

say "=== second run: up to date ==="
if ! sh install.sh --base-url "$BASE_URL" --release-signers "$SIGNERS" --yes --with-nvim \
	>second-install.log 2>&1; then
	fail "the second install exited non-zero"
fi
cat second-install.log
grep -qF "neovibe $VERSION is up to date" second-install.log || fail "the second run did not say 'up to date'"

say "=== neovibe --legacy / NEOVIBE_AGENT_BACKEND=legacy (plan Task 5) ==="
set +e
"$LAUNCHER" --legacy --quiet "$HOME" >legacy-flag.log 2>&1
rc1=$?
NEOVIBE_AGENT_BACKEND=legacy "$LAUNCHER" --quiet "$HOME" >legacy-env.log 2>&1
rc2=$?
set -e
[ "$rc1" = 1 ] || fail "neovibe --legacy exited $rc1, not 1"
grep -qF 'the legacy backend is not in this build' legacy-flag.log || fail "--legacy's output does not name 'the legacy backend is not in this build'"
[ "$rc2" = 1 ] || fail "NEOVIBE_AGENT_BACKEND=legacy exited $rc2, not 1"
grep -qF 'the legacy backend is not in this build' legacy-env.log || fail "the env-var path's output does not name 'the legacy backend is not in this build'"

say "=== uninstall ==="
if ! sh install.sh --uninstall >uninstall.log 2>&1; then
	fail "--uninstall exited non-zero"
fi
cat uninstall.log
# Spec sec 6.6's own removal list, checked by name (never claims uninstall also prunes a now-empty
# parent such as ~/.local/bin or ~/.local/share/applications, which it is not asked to and does not).
for p in .local/lib/neovibe .local/bin/neovibe .local/share/applications/neovibe.desktop \
	.local/share/licenses/neovibe .local/share/neovibe; do
	if [ -e "$HOME/$p" ] || [ -L "$HOME/$p" ]; then fail "uninstall left $HOME/$p behind"; fi
done
LEFT=$(find "$HOME" -mindepth 1 -type f \
	! -path "$HOME/.config/*" ! -path "$HOME/.local/state/*" \
	! -name 'install.sh' ! -name '*.log' ! -name '*.part' 2>/dev/null || true)
if [ -n "$LEFT" ]; then
	fail "stray files remain outside ~/.config and the state dir after --uninstall: $LEFT"
fi

if [ "$ok" = 1 ]; then
	echo DRIVER_RESULT_PASS
else
	echo DRIVER_RESULT_FAIL
fi
DRIVER
chmod +x "$SCRATCH/scripts/curl-flow.sh"

run_curl_flow() {
	local name="$1" image="$2" piped="$3" expect_distro_nvim="$4"
	should_run "$name" || return 0
	log "=== $name ($image) ==="
	local home="$SCRATCH/homes/$name"
	mkdir -p -- "$home"
	local t0 t1
	t0=$(date +%s)
	local logf="$OUT/$name.log"
	if docker run --rm --user "$UID_H:$GID_H" --network host \
		--mount "type=bind,src=$home,dst=$home" \
		--mount "type=bind,src=$SCRATCH/scripts/curl-flow.sh,dst=/curl-flow.sh,readonly" \
		--mount "type=bind,src=$SIGNERS_HOST,dst=/release-signers,readonly" \
		-e "HOME=$home" -e "XDG_DATA_HOME=$home/.local/share" -e "XDG_CACHE_HOME=$home/.cache" \
		-e "XDG_STATE_HOME=$home/.local/state" -e "BASE_URL=$BASE_URL" -e "SIGNERS=/release-signers" \
		-e "VERSION=$VERSION" -e "EXPECT_VERSION_LINE=$EXPECT_VERSION_LINE" -e "REV7=$REV7" \
		-e "PIPED=$piped" -e "EXPECT_DISTRO_NVIM=$expect_distro_nvim" -e "PORT=$PORT" \
		-w "$home" "$image" /bin/sh /curl-flow.sh >"$logf" 2>&1; then
		:
	fi
	t1=$(date +%s)
	local du
	du=$(sed -n 's/^.*disk usage after the first install.*: //p' "$logf" | tail -n1)
	[ -n "$du" ] || du="(unknown: the driver never reached that line)"
	# npm's default verbosity does not print grpc-tools' install-script download URL, so the direct
	# grep is a bonus, not the only evidence: `grpc_tools_node_protoc` actually running (the
	# `generate` step's own build log line) only follows a successful prebuilt fetch, since spec
	# sec 15/5.3 states there is no compile fallback at all.
	local grpc=no
	if grep -q 'node-precompiled-binaries.grpc.io' "$logf" 2>/dev/null; then
		grpc="yes (seen: node-precompiled-binaries.grpc.io)"
	elif grep -q 'grpc_tools_node_protoc --plugin=' "$logf" 2>/dev/null; then
		grpc="yes (inferred: grpc_tools_node_protoc ran; spec sec 15 says there is no compile fallback)"
	fi
	log "$name: sidecar-build wall time ~$((t1 - t0))s, disk $du, grpc-tools prebuilt protoc downloaded: $grpc"
	printf '%s\twall_s=%s\tdisk=%s\tgrpc_protoc_downloaded=%s\n' "$name" "$((t1 - t0))" "$du" "$grpc" >>"$OUT/measurements.tsv"
	if grep -q '^DRIVER_RESULT_PASS$' "$logf" && ! grep -q 'ASSERT FAILED' "$logf"; then
		record "$name" 1
	else
		record "$name" 0
		log "  see $logf"
	fi
}

# ================================================================================================
# The pkg-flow driver: `apt`/`dnf` install a local package as root, confirm no maintainer script ran
# and that `neovibe setup` refuses as root, then drop to the created non-root user and run
# `neovibe setup --yes` against the SHA256SUMS-less mirror (REL-1's behavioural proof).
cat >"$SCRATCH/scripts/pkg-flow.sh" <<'DRIVER'
#!/bin/sh
# Env in: PKG(deb|rpm) PKGFILE PKGNAME SETUP_BASE_URL REV7. Runs as root throughout; drops to the
# image's own non-root `tester` user (`su -`, so $HOME is really /home/tester, bind-mounted by the
# caller) only for the one step that must not run as root.
set -eu
ok=1
say() { printf '[driver] %s\n' "$*"; }
fail() {
	printf '[driver] ASSERT FAILED: %s\n' "$*"
	ok=0
}

say "installing $PKGFILE as $(id)"
if [ "$PKG" = deb ]; then
	apt-get update >apt-update.log 2>&1 || fail "apt-get update failed"
	if ! DEBIAN_FRONTEND=noninteractive apt-get install -y "$PKGFILE" >pkg-install.log 2>&1; then
		fail "apt install $PKGFILE failed"
	fi
	cat pkg-install.log
	SCRIPTS=""
	for f in preinst postinst prerm postrm; do
		[ -f "/var/lib/dpkg/info/$PKGNAME.$f" ] && SCRIPTS="$SCRIPTS $f"
	done
	if [ -n "$SCRIPTS" ]; then fail "dpkg ran a maintainer script for $PKGNAME:$SCRIPTS"; fi
	dpkg -s "$PKGNAME" | grep -q '^Status: install ok installed' || fail "dpkg -s $PKGNAME does not report installed"
else
	if ! dnf install -y "$PKGFILE" >pkg-install.log 2>&1; then
		fail "dnf install $PKGFILE failed"
	fi
	cat pkg-install.log
	SC=$(rpm -q --scripts "$PKGNAME" 2>&1 || true)
	if [ -n "$SC" ] && [ "$SC" != "(none)" ]; then fail "rpm -q --scripts reports a script: $SC"; fi
fi

say "=== neovibe setup as root refuses ==="
set +e
neovibe setup --yes --base-url "$SETUP_BASE_URL" >root-setup.log 2>&1
rc=$?
set -e
cat root-setup.log
[ "$rc" != 0 ] || fail "neovibe setup as root did not refuse"
grep -qiE 'root' root-setup.log || fail "the root refusal message does not mention root"

say "=== neovibe setup --yes as the non-root user (REL-1) ==="
# The redirection is inside the -c string, so tester's own shell creates the log: redirected out
# here, root's shell would create it first, leaving a root-owned file on the host's bind mount
# (spec sec 2.4's hygiene).
su - tester -c "neovibe setup --yes --base-url '$SETUP_BASE_URL' >/home/tester/user-setup.log 2>&1" ||
	fail "neovibe setup --yes as tester failed"
cat /home/tester/user-setup.log
SIDECAR="/home/tester/.local/share/neovibe/sidecar/$REV7/verdandi-claude-sidecar"
[ -x "$SIDECAR" ] || fail "REL-1: the sidecar did not land at $SIDECAR"

if [ "$ok" = 1 ]; then
	echo DRIVER_RESULT_PASS
else
	echo DRIVER_RESULT_FAIL
fi
DRIVER
chmod +x "$SCRATCH/scripts/pkg-flow.sh"

run_pkg_flow() {
	local name="$1" image="$2" pkg="$3" pkgfile="$4" pkgname="$5"
	should_run "$name" || return 0
	log "=== $name ($image, $pkg) ==="
	local home="$SCRATCH/homes/$name-tester"
	mkdir -p -- "$home"
	local logf="$OUT/$name.log"
	# --user is deliberately omitted (root, for apt/dnf); the mount lands at /home/tester, the
	# image's own non-root user's real $HOME, so `su - tester` needs no HOME override and the
	# sidecar it builds is visible on the host afterward.
	docker run --rm --network host \
		--mount "type=bind,src=$home,dst=/home/tester" \
		--mount "type=bind,src=$SCRATCH/scripts/pkg-flow.sh,dst=/pkg-flow.sh,readonly" \
		--mount "type=bind,src=$pkgfile,dst=/pkg.$pkg,readonly" \
		-e "PKG=$pkg" -e "PKGFILE=/pkg.$pkg" -e "PKGNAME=$pkgname" -e "SETUP_BASE_URL=$SETUP_BASE_URL" \
		-e "REV7=$REV7" \
		"$image" /bin/sh /pkg-flow.sh >"$logf" 2>&1 || true
	if grep -q '^DRIVER_RESULT_PASS$' "$logf" && ! grep -q 'ASSERT FAILED' "$logf"; then
		record "$name" 1
	else
		record "$name" 0
		log "  see $logf"
	fi
}

: >"$OUT/measurements.tsv"

log "building the e2e images (never --pull)"
docker build -q --build-arg UID="$UID_H" --build-arg GID="$GID_H" --build-arg WITH_DISTRO_NVIM=0 \
	-t neovibe-e2e-ubuntu-no-nvim -f "$HERE/ubuntu-24.04.Dockerfile" "$HERE" >/dev/null
docker build -q --build-arg UID="$UID_H" --build-arg GID="$GID_H" --build-arg WITH_DISTRO_NVIM=1 \
	-t neovibe-e2e-ubuntu-0.9.5 -f "$HERE/ubuntu-24.04.Dockerfile" "$HERE" >/dev/null
docker build -q --build-arg UID="$UID_H" --build-arg GID="$GID_H" \
	-t neovibe-e2e-fedora -f "$HERE/fedora-44.Dockerfile" "$HERE" >/dev/null
docker build -q --build-arg UID="$UID_H" --build-arg GID="$GID_H" \
	-t neovibe-e2e-arch -f "$HERE/arch.Dockerfile" "$HERE" >/dev/null

run_curl_flow ubuntu-no-nvim neovibe-e2e-ubuntu-no-nvim 0 0
run_curl_flow ubuntu-0.9.5 neovibe-e2e-ubuntu-0.9.5 0 1
run_curl_flow ubuntu-piped neovibe-e2e-ubuntu-no-nvim 1 0
run_curl_flow fedora neovibe-e2e-fedora 0 0
run_curl_flow arch neovibe-e2e-arch 0 0

run_pkg_flow ubuntu-pkg neovibe-e2e-ubuntu-no-nvim deb "$RELEASE_DIR/neovibe_${VERSION}_amd64.deb" neovibe
run_pkg_flow fedora-pkg neovibe-e2e-fedora rpm "$RELEASE_DIR/neovibe-${VERSION}-1.x86_64.rpm" neovibe

log "=== summary: $PASS passed, $FAIL failed ==="
cat "$RESULTS"
if [ "$FAIL" -gt 0 ]; then
	log "failed:$FAILED_NAMES"
	exit 1
fi
