#!/bin/sh
# The test harness for packaging/install.sh (plan 2026-09-27-v1-dist, Task 9). POSIX sh, run twice
# by packaging/test_install.py: under bash on the host, and under dash inside the Ubuntu 24.04
# image (Dockerfile.dash), where /bin/sh -- and so the installer's shell -- is dash.
#
#   harness.sh --sh INTERPRETER --scratch DIR [--real-home DIR]
#              [--server-port PORT --server-log FILE] [--only NAME]...
#
#   --sh           the shell every installer run uses (`bash`, or `/bin/sh` = dash in the image)
#   --scratch      an empty or absent directory under ~/.cache (never /tmp); kept on failure
#   --real-home    the real HOME, whose Eitri directories run-in-env.sh guards on every run
#                  (on the host; the image cannot see it, and test_install.py guards around it)
#   --server-port  a fixture server already serving <scratch>/srv on 127.0.0.1, for a hand run
#                  against a server of your own. Without it (how test_install.py runs both halves:
#                  the image has python3, and its run has no network at all) this starts one
#   --only         run only the named test (repeatable)
#
# Every installer run goes through run-in-env.sh. After every test: the stub sudo and package
# managers were never called, the planted ~/.local/bin/nvim and vim are byte-identical, the stub
# claude saw only `--version`, the run's cwd is still empty, and no directory named `~` exists.
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
PKG=$(cd "$HERE/../.." && pwd)
# REAL_INSTALLER is packaging/install.sh as committed, with the owner's release key embedded. Tests
# that read the source (the embedded block, the pinned verify line) and the one test of the real
# key's own behaviour use it. INSTALLER, which every other test runs, copies into a fixture release
# or compares with an installed eitri-setup, is REAL_INSTALLER with the key lines taken out of its
# signers block (setup_keys), so a test that means "an installer with no key" does not depend on
# which key the owner has listed. Until setup_keys has run it is the real file.
REAL_INSTALLER=$PKG/install.sh
INSTALLER=$REAL_INSTALLER
WRAP=$HERE/run-in-env.sh
FIXTURES=$HERE/fixtures
NL='
'

NV_SH=
S=
REAL_HOME=
PORT=
SRV_LOG=
ONLY=
SRV_PID=

while [ $# -gt 0 ]; do
	case $1 in
	--sh) NV_SH=$2; shift ;;
	--scratch) S=$2; shift ;;
	--real-home) REAL_HOME=$2; shift ;;
	--server-port) PORT=$2; shift ;;
	--server-log) SRV_LOG=$2; shift ;;
	--only) ONLY="$ONLY $2 "; shift ;;
	*) echo "harness: unknown option $1" >&2; exit 2 ;;
	esac
	shift
done
[ -n "$NV_SH" ] && [ -n "$S" ] || { echo "harness: --sh and --scratch are required" >&2; exit 2; }
case $S in /*) ;; *) echo "harness: --scratch must be absolute" >&2; exit 2 ;; esac

# ---------------------------------------------------------------------------------------------
# Fixtures, built once per run

REV_A=aaaaaaa0000000000000000000000000000000a1
REV_B=bbbbbbb0000000000000000000000000000000b2
REV_C=ccccccc0000000000000000000000000000000c3
SIDECAR_LINE='verdandi-claude-sidecar 0.1.0 (protocol 3, node v22.23.2, build 0123456789abcdef)'

setup_stubs() {
	mkdir -p "$S/logs" "$S/stubs" "$S/stubs-root" "$S/stubs-tarfail" "$S/stubs-net" "$S/stubs-wget" \
		"$S/stubs-mvlog" "$S/stubs-oldglibc" "$S/stubs-patheitri" "$S/stubs-mvfail" "$S/stubs-mvterm" \
		"$S/stubs-probeuname" "$S/stubs-swapcat" "$S/stubs-swapsha" "$S/stubs-mvafterswap" \
		"$S/stubs-cargo" "$S/stubs-cpswap" "$S/stubs-fakeowner" \
		"$S/helpers"
	for c in sudo apt apt-get dnf pacman zypper; do cp "$FIXTURES/forbidden" "$S/stubs/$c"; done
	cp "$FIXTURES/claude" "$FIXTURES/nvim" "$S/stubs/"
	cp "$FIXTURES/id-root" "$S/stubs-root/id"
	cp "$FIXTURES/tar-fail" "$S/stubs-tarfail/tar"
	cp "$FIXTURES/curl-mirror" "$S/stubs-net/curl"
	cp "$FIXTURES/wget-mirror" "$S/stubs-wget/wget"
	cp "$FIXTURES/mv-log" "$S/stubs-mvlog/mv"
	cp "$FIXTURES/mv-swap-fails" "$S/stubs-mvfail/mv"
	cp "$FIXTURES/mv-swap-fails" "$S/stubs-mvterm/mv"
	cp "$FIXTURES/getconf-old" "$S/stubs-oldglibc/getconf"
	cp "$FIXTURES/eitri-other" "$S/stubs-patheitri/eitri"
	cp "$FIXTURES/uname-probe" "$S/stubs-probeuname/uname"
	cp "$FIXTURES/cat-swap" "$S/stubs-swapcat/cat"
	cp "$FIXTURES/sha256sum-swap" "$S/stubs-swapsha/sha256sum"
	cp "$FIXTURES/mv-after-swap-term" "$S/stubs-mvafterswap/mv"
	cp "$FIXTURES/cp-swap" "$S/stubs-cpswap/cp"
	cp "$FIXTURES/find-fake-owner" "$S/stubs-fakeowner/find"
	# check_build_tools' whole floor, stubbed (plan Task 11): no test here needs a real
	# Rust/C/pkg-config/protobuf toolchain installed. Only `git` is real (setup_stubs itself does
	# not install it; the harness's own host, or the Ubuntu 24.04 dash image, must have it).
	cp "$FIXTURES/cargo-fromsource" "$S/stubs-cargo/cargo"
	cp "$FIXTURES/rustc-fromsource" "$S/stubs-cargo/rustc"
	cp "$FIXTURES/cc-fromsource" "$S/stubs-cargo/cc"
	cp "$FIXTURES/pkg-config-fromsource" "$S/stubs-cargo/pkg-config"
	cp "$FIXTURES/protoc-fromsource" "$S/stubs-cargo/protoc"
	cp "$FIXTURES/env-nl" "$S/helpers/env-nl"
	chmod 0755 "$S"/stubs*/* "$S"/helpers/*
	# A PATH tail without one tool: every entry of /usr/bin (and /bin when it is its own directory)
	# linked, except the one left out. /usr/bin cannot be on such a PATH, since it holds the tool.
	for tool in ssh-keygen curl cargo; do
		d=$S/path-no-$tool
		mkdir -p "$d"
		for src in /usr/bin /bin; do
			if [ "$src" = /bin ] && [ -L /bin ]; then continue; fi
			for f in "$src"/*; do
				b=${f##*/}
				[ "$b" = "$tool" ] && continue
				[ -e "$d/$b" ] || ln -s "$f" "$d/$b"
			done
		done
	done
}

setup_keys() {
	mkdir -p "$S/keys"
	ssh-keygen -q -t ed25519 -N '' -C '' -f "$S/keys/release"
	ssh-keygen -q -t ed25519 -N '' -C '' -f "$S/keys/other"
	# allowed_signers lines with no comment field, as packaging/release-signers specifies.
	for k in release other; do
		awk '{ print "release@eitri namespaces=\"eitri-release\" " $1 " " $2 }' "$S/keys/$k.pub" >"$S/keys/signers-$k"
	done
	SIGNERS=$S/keys/signers-release
	# The installer with no key embedded (a fixture of this harness, never shipped): install.sh with
	# every key line of its embedded signers block removed, so that "no key built in" stays
	# deterministic now that the real file lists the owner's release key. Only key lines may go.
	mkdir -p "$S/unkeyed"
	awk '
		/^[[:space:]]*cat <<.EITRI_RELEASE_SIGNERS.$/ { inside = 1; print; next }
		/^EITRI_RELEASE_SIGNERS$/ { inside = 0 }
		inside && /^[[:space:]]*[^#[:space:]]/ { next }
		{ print }' "$REAL_INSTALLER" >"$S/unkeyed/install.sh"
	chmod 0755 "$S/unkeyed/install.sh"
	_sk_keys=$(grep -c -E '^[[:space:]]*[^#[:space:]]' "$PKG/release-signers")
	_sk_removed=$(diff "$REAL_INSTALLER" "$S/unkeyed/install.sh" | grep -c '^<')
	_sk_added=$(diff "$REAL_INSTALLER" "$S/unkeyed/install.sh" | grep -c '^>')
	if [ "$_sk_removed" != "$_sk_keys" ] || [ "$_sk_added" != 0 ] ||
		diff "$REAL_INSTALLER" "$S/unkeyed/install.sh" | grep '^<' | grep -v -E '^< [^#[:space:]]' >/dev/null; then
		echo "harness: the unkeyed installer is not install.sh minus the $_sk_keys key line(s) of release-signers" >&2
		exit 2
	fi
	INSTALLER=$S/unkeyed/install.sh
	# The installer with the test key embedded in its signers block, as every final release's
	# installer carries one (spec §6.4, D13): the branch that runs with no --release-signers. The
	# unkeyed copy plus one line, so it carries the test key and never the owner's.
	awk -v key="$(cat "$SIGNERS")" '/^EITRI_RELEASE_SIGNERS$/ { print key } { print }' \
		"$INSTALLER" >"$S/install-keyed.sh"
	KEYED_INSTALLER=$S/install-keyed.sh
	if [ "$(diff "$INSTALLER" "$KEYED_INSTALLER" | grep -c '^[<>]')" != 1 ]; then
		echo "harness: the keyed installer is not the unkeyed installer plus one key line" >&2
		exit 2
	fi
}

setup_libs() {
	# ok: GTK 4.14.5 and WebKit, found through a fake `ldconfig -p`.
	d=$S/libs/ok
	mkdir -p "$d"
	: >"$d/libgtk-4.so.1.1405.5"
	ln -s libgtk-4.so.1.1405.5 "$d/libgtk-4.so.1"
	: >"$d/libwebkitgtk-6.0.so.4"
	printf '3 libs found in cache `/etc/ld.so.cache'"'"'\n\tlibgtk-4.so.1 (libc6,x86-64) => %s/libgtk-4.so.1\n\tlibwebkitgtk-6.0.so.4 (libc6,x86-64) => %s/libwebkitgtk-6.0.so.4\n' \
		"$d" "$d" >"$d/ldconfig-p.txt"
	# gtk412: GTK 4.12 and WebKit, found by the directory scan (no ldconfig output).
	d=$S/libs/gtk412
	mkdir -p "$d"
	: >"$d/libgtk-4.so.1.1200.3"
	ln -s libgtk-4.so.1.1200.3 "$d/libgtk-4.so.1"
	: >"$d/libwebkitgtk-6.0.so.4"
	# nowebkit: GTK 4.14 only.
	d=$S/libs/nowebkit
	mkdir -p "$d"
	: >"$d/libgtk-4.so.1.1405.5"
	ln -s libgtk-4.so.1.1405.5 "$d/libgtk-4.so.1"
	mkdir -p "$S/osrel"
	printf 'NAME="Ubuntu"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID="24.04"\n' >"$S/osrel/ubuntu-24.04"
	printf 'NAME="Ubuntu"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID="22.04"\n' >"$S/osrel/ubuntu-22.04"
	printf 'NAME="Fedora Linux"\nID=fedora\nVERSION_ID=41\n' >"$S/osrel/fedora-41"
	mkdir -p "$S/system"
}

# setup_sidecar_fixtures: the sidecar build's own inputs (plan 2026-09-27-v1-dist, Task 10, spec
# §5.3) -- built once, shared by every mk_release call and by test_sidecar.sh directly. A real
# `bin/npm` (fixtures/npm-sidecar) and `bin/node` (a stub -- nothing here ever runs it, sidecar_build_core
# only checks it exists and is executable) inside a real, xz-compressed tarball shaped like the real
# nodejs.org one, so tar's own extraction is exercised for real. NODE_FIXTURE_SHA256_X64 is this
# fixture's own real hash, embedded by mk_release into RELEASE -- never the real pinned Node's hash,
# since no test unpacks the real Node. NODE_FIXTURE_SHA256_ARM64 is a matching real fixture too
# (installer-codex-4/5's own aarch64 tests): every other test's own `uname -m` is the real one
# (x86_64 on this host), so nothing but those ever downloads or checks it.
setup_sidecar_fixtures() {
	NODE_FIXTURE_VERSION=v22.23.2
	rm -rf "$S/fix/node-build"
	mkdir -p "$S/fix" "$S/srv/dist/$NODE_FIXTURE_VERSION"
	for _sf_arch in x64 arm64; do
		_nd=node-$NODE_FIXTURE_VERSION-linux-$_sf_arch
		mkdir -p "$S/fix/node-build/$_nd/bin"
		printf '#!/bin/sh\necho "node-stub: this is a test double, not real Node" >&2\nexit 1\n' \
			>"$S/fix/node-build/$_nd/bin/node"
		cp "$FIXTURES/npm-sidecar" "$S/fix/node-build/$_nd/bin/npm"
		chmod 0755 "$S/fix/node-build/$_nd/bin/node" "$S/fix/node-build/$_nd/bin/npm"
		printf 'Test double Node LICENSE, from the fake tarball built for packaging/tests/install; not the real one.\n' \
			>"$S/fix/node-build/$_nd/LICENSE"
		tar -C "$S/fix/node-build" -cJf "$S/fix/$_nd.tar.xz" "$_nd"
		cp "$S/fix/$_nd.tar.xz" "$S/srv/dist/$NODE_FIXTURE_VERSION/$_nd.tar.xz"
	done
	NODE_FIXTURE_SHA256_X64=$(sha256sum <"$S/fix/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz" | cut -d' ' -f1)
	NODE_FIXTURE_SHA256_ARM64=$(sha256sum <"$S/fix/node-$NODE_FIXTURE_VERSION-linux-arm64.tar.xz" | cut -d' ' -f1)
	NODE_FIXTURE_TARBALL=$S/fix/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz
}

# setup_skia_fixture: --from-source's own pinned Skia input (installer-claude-4, plan 2026-09-28
# v1-dist review2 fixes Task 3) -- fetch_skia_binaries checks its download against a sha256, never
# skia-bindings' own unauthenticated one, so a test double only needs to be a real, checksummable
# file; nothing here ever unpacks or links it. SKIA_FIXTURE_URL is a fixed, made-up upstream URL
# (never actually fetched: fetch_skia_binaries only ever reads its basename in test mode and fetches
# that basename from EITRI_INSTALL_TEST_SKIA_BASE_URL instead) that fs_scaffold writes into every
# from-source fixture's own pins.env as SKIA_BINARIES_URL_UPSTREAM.
SKIA_FIXTURE_URL='https://example.invalid/skia-binaries-test.tar.gz'
setup_skia_fixture() {
	mkdir -p "$S/fix" "$S/srv/skia"
	printf 'test fixture Skia archive, from packaging/tests/install/harness.sh; not the real thing.\n' \
		>"$S/fix/skia-binaries-test.tar.gz"
	cp "$S/fix/skia-binaries-test.tar.gz" "$S/srv/skia/skia-binaries-test.tar.gz"
	SKIA_FIXTURE_SHA256=$(sha256sum <"$S/fix/skia-binaries-test.tar.gz" | cut -d' ' -f1)
}

# setup_nvim_fixtures: the nvim offer's own input (plan 2026-09-27-v1-dist, Task 11, spec §7) -- a
# real tar.gz shaped exactly like the official nvim-linux-x86_64.tar.gz (top directory
# nvim-linux-x86_64/, bin/nvim inside it), served at the same releases/download/vX.Y.Z/ layout
# github.com/neovim/neovim uses, at $S/srv/nvim-releases/. NVIM_FIXTURE_SHA256 is this fixture's own
# real hash, embedded by mk_release into RELEASE -- never the real pinned nvim's hash, since no test
# unpacks the real nvim release.
setup_nvim_fixtures() {
	NVIM_FIXTURE_VERSION=0.11.2
	_nv_top=nvim-linux-x86_64
	rm -rf "$S/fix/nvim-build"
	mkdir -p "$S/fix/nvim-build/$_nv_top/bin"
	{
		printf '#!/bin/sh\n'
		# shellcheck disable=SC2016 # the literal line, variables and all
		printf 'case $1 in\n'
		printf -- '--version) echo "NVIM v%s" ;;\n' "$NVIM_FIXTURE_VERSION"
		# shellcheck disable=SC2016 # the literal line, variables and all
		printf '*) echo "nvim fixture: unexpected argv: $*" >&2; exit 1 ;;\n'
		printf 'esac\n'
	} >"$S/fix/nvim-build/$_nv_top/bin/nvim"
	chmod 0755 "$S/fix/nvim-build/$_nv_top/bin/nvim"
	mkdir -p "$S/fix" "$S/srv/nvim-releases/v$NVIM_FIXTURE_VERSION"
	tar -C "$S/fix/nvim-build" -czf "$S/fix/$_nv_top.tar.gz" "$_nv_top"
	cp "$S/fix/$_nv_top.tar.gz" "$S/srv/nvim-releases/v$NVIM_FIXTURE_VERSION/$_nv_top.tar.gz"
	NVIM_FIXTURE_SHA256=$(sha256sum <"$S/fix/$_nv_top.tar.gz" | cut -d' ' -f1)
}

# mk_verdandi_source REV7 DEST: a minimal, real Verdandi source tree (just enough for
# sidecar_build_core's own sanity check) tarred to DEST with no wrapper directory, matching a
# wrapper-less `git archive`. Sets MVS_SHA256.
mk_verdandi_source() {
	_mv_src=$S/fix/verdandi-src-$1
	rm -rf "$_mv_src"
	mkdir -p "$_mv_src/apps/claude-sidecar/scripts"
	printf '{\n  "name": "verdandi",\n  "private": true\n}\n' >"$_mv_src/package.json"
	printf '{\n  "name": "@verdandi/claude-sidecar",\n  "version": "0.1.0"\n}\n' \
		>"$_mv_src/apps/claude-sidecar/package.json"
	printf '// test fixture: never run (npm itself is a stub in these tests)\n' \
		>"$_mv_src/apps/claude-sidecar/scripts/buildBinary.mjs"
	rm -f "$2"
	tar -C "$_mv_src" -czf "$2" .
	MVS_SHA256=$(sha256sum <"$2" | cut -d' ' -f1)
}

# mk_release VERSION REV: a release directory $S/fix/v$VERSION laid out as spec §4.3, from stub
# binaries, the real launcher and desktop file, this installer as eitri-setup, and a real (fake)
# Verdandi source asset + the shared fake Node's pins, so every release's RELEASE is one a sidecar
# build can actually be attempted against (plan Task 10).
mk_release() {
	v=$1
	rev=$2
	rev7=$(echo "$rev" | cut -c1-7)
	top=eitri-$v-x86_64-linux
	b=$S/fix/build-$v/$top
	rm -rf "$S/fix/build-$v" "$S/fix/v$v"
	mkdir -p "$b/bin" "$b/lib/eitri" "$b/share/applications" "$b/share/licenses/eitri" "$S/fix/v$v"
	cp "$PKG/eitri.launcher.sh" "$b/bin/eitri"
	chmod 0755 "$b/bin/eitri"
	for bin in shell eitri-supervisor eitri-tmux-shim eitri-claude-handoff; do
		printf '#!/bin/sh\necho "stub %s %s"\n' "$bin" "$v" >"$b/lib/eitri/$bin"
		chmod 0755 "$b/lib/eitri/$bin"
	done
	# The fixture release's eitri-setup and install.sh are the unkeyed copy, not the real file: the
	# tests run them (the tarball's eitri-setup after an install, `eitri setup`) and compare the
	# installed eitri-setup with $INSTALLER, and none of them may depend on the owner's real key.
	# Nothing here stands in for what the real release ships; t_real_installer_* test that file.
	cp "$INSTALLER" "$b/lib/eitri/eitri-setup"
	chmod 0755 "$b/lib/eitri/eitri-setup"
	mk_verdandi_source "$rev7" "$S/fix/v$v/verdandi-$rev7-source.tar.gz"
	{
		echo "EITRI_VERSION=$v"
		echo "EITRI_COMMIT=1111111111111111111111111111111111111111"
		echo "NEOVIDE_FORK_COMMIT=2222222222222222222222222222222222222222"
		echo "VERDANDI_REV=$rev"
		echo "VERDANDI_SOURCE=verdandi-$rev7-source.tar.gz"
		echo "VERDANDI_SOURCE_SHA256=$MVS_SHA256"
		echo "NODE_VERSION=$NODE_FIXTURE_VERSION"
		echo "NODE_SHA256_linux_x64=$NODE_FIXTURE_SHA256_X64"
		echo "NODE_SHA256_linux_arm64=$NODE_FIXTURE_SHA256_ARM64"
		echo "NVIM_VERSION=$NVIM_FIXTURE_VERSION"
		echo "NVIM_SHA256_linux_x86_64=$NVIM_FIXTURE_SHA256"
		echo "GTK_FLOOR=4.14"
	} >"$b/lib/eitri/RELEASE"
	cp "$PKG/eitri.desktop" "$b/share/applications/eitri.desktop"
	for f in LICENSE THIRD-PARTY-LICENSES SOURCE; do
		echo "$f for $v" >"$b/share/licenses/eitri/$f"
	done
	tar -C "$S/fix/build-$v" -czf "$S/fix/v$v/$top.tar.gz" "$top"
	cp "$b/lib/eitri/RELEASE" "$S/fix/v$v/RELEASE"
	cp "$INSTALLER" "$S/fix/v$v/install.sh"
	resign "$S/fix/v$v"
}

# resign DIR [KEY]: SHA256SUMS over DIR's release files, and SHA256SUMS.sig by KEY (release).
resign() {
	(
		cd "$1" || exit 1
		rm -f SHA256SUMS SHA256SUMS.sig
		sha256sum -- *.tar.gz install.sh RELEASE >SHA256SUMS
	)
	sign_sums "$1" "${2:-release}"
}

sign_sums() {
	rm -f "$1/SHA256SUMS.sig"
	ssh-keygen -q -Y sign -f "$S/keys/$2" -n eitri-release "$1/SHA256SUMS" 2>/dev/null
}

setup_releases() {
	mk_release 1.0.0 "$REV_A"
	mk_release 1.1.0 "$REV_B"
	mk_release 1.2.0 "$REV_C"
	mk_release 1.0.0-rc.1 "$REV_A"
	mkdir -p "$S/srv"
}

# serve VERSION... : the served tree holds exactly these releases; `latest` is the first named.
serve() {
	rm -rf "$S/srv/releases"
	mkdir -p "$S/srv/releases/latest/download" "$S/srv/releases/download"
	cp -R "$S/fix/v$1/." "$S/srv/releases/latest/download/"
	for v do
		cp -R "$S/fix/v$v" "$S/srv/releases/download/v$v"
	done
}

# served VERSION: the directory a release is served from (also the one `latest` mirrors).
served() { printf '%s\n' "$S/srv/releases/download/v$1"; }

# relatest VERSION: after editing served VERSION, make `latest` a copy of it again.
relatest() {
	rm -rf "$S/srv/releases/latest/download"
	mkdir -p "$S/srv/releases/latest/download"
	cp -R "$(served "$1")/." "$S/srv/releases/latest/download/"
}

start_server() {
	if [ -n "$PORT" ]; then
		[ -n "$SRV_LOG" ] || SRV_LOG=$S/srv.log
		return 0
	fi
	SRV_LOG=$S/srv.log
	python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$S/srv" >"$S/srv.out" 2>"$SRV_LOG" &
	SRV_PID=$!
	i=0
	while [ $i -lt 100 ]; do
		PORT=$(sed -n 's/.*port \([0-9][0-9]*\).*/\1/p' "$S/srv.out" | head -n 1)
		[ -n "$PORT" ] && break
		sleep 0.1
		i=$((i + 1))
	done
	[ -n "$PORT" ] || { echo "harness: the fixture server did not start" >&2; exit 2; }
}

stop_server() {
	if [ -n "$SRV_PID" ]; then
		kill "$SRV_PID" 2>/dev/null
		wait "$SRV_PID" 2>/dev/null
		SRV_PID=
	fi
}

# ---------------------------------------------------------------------------------------------
# Per-test helpers. A test runs in a subshell: T is its directory, TH its HOME, OUT the last
# installer run's output, RC its exit status.

FAILS=0
fail() {
	FAILS=$((FAILS + 1))
	printf '    FAIL: %s\n' "$*"
}

# inst [WRAPPER-OPTION]... [--] [INSTALLER-ARG]... -- options before `--` go to run-in-env.sh. It
# runs $INSTALLER_UNDER_TEST when a test sets it (the keyed copy), else install.sh itself.
inst() {
	_i_has=0
	for _i_a do
		if [ "$_i_a" = -- ]; then _i_has=1; fi
	done
	_i_seen=0
	if [ "$_i_has" = 0 ]; then
		set -- -- "$@"
	fi
	for _i_a do
		shift
		if [ "$_i_seen" = 0 ] && [ "$_i_a" = -- ]; then
			_i_seen=1
			set -- "$@" -- "$NV_SH" "${INSTALLER_UNDER_TEST:-$INSTALLER}"
			continue
		fi
		set -- "$@" "$_i_a"
	done
	if [ -n "$REAL_HOME" ]; then set -- --guard-real-home "$REAL_HOME" "$@"; fi
	# PRE_STUBS (plan Task 11): a directory searched BEFORE $S/stubs -- for a test that needs its own
	# nvim/cargo/etc. to shadow the fixtures every other test in this suite relies on ($S/stubs's own
	# nvim, NVIM v0.11.4, already adequate; forbidden/claude stay reachable from $S/stubs right after
	# it). Unset for every caller that does not set it, so this is a no-op everywhere else.
	if [ -n "${PRE_STUBS:-}" ]; then
		"$WRAP" --home "$TH" --cwd "$T/cwd" --stubs "$PRE_STUBS" --stubs "$S/stubs" \
			--set EITRI_INSTALL_TEST=1 \
			--set "EITRI_INSTALL_TEST_LIBDIRS=$S/libs/ok" \
			--set "EITRI_INSTALL_TEST_SYSTEM_RELEASE=$S/system/RELEASE" \
			--set "EITRI_INSTALL_TEST_OS_RELEASE=$S/osrel/ubuntu-24.04" \
			"$@" </dev/null >"$OUT" 2>&1
	else
		"$WRAP" --home "$TH" --cwd "$T/cwd" --stubs "$S/stubs" \
			--set EITRI_INSTALL_TEST=1 \
			--set "EITRI_INSTALL_TEST_LIBDIRS=$S/libs/ok" \
			--set "EITRI_INSTALL_TEST_SYSTEM_RELEASE=$S/system/RELEASE" \
			--set "EITRI_INSTALL_TEST_OS_RELEASE=$S/osrel/ubuntu-24.04" \
			"$@" </dev/null >"$OUT" 2>&1
	fi
	RC=$?
	RUNS=$((RUNS + 1))
	cp "$OUT" "$T/out.$RUNS"
	case $RC in 96 | 97 | 98) fail "run-in-env.sh refused or its guard fired (exit $RC): $(cat "$OUT")" ;; esac
}

# inst_net ARGS: inst against the fixture server, checking signatures with the test key.
inst_net() {
	inst "$@" --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
}

expect_rc() {
	if [ "$RC" != "$1" ]; then fail "exit status $RC, expected $1${2:+ ($2)}; output:$NL$(sed 's/^/      | /' "$OUT")"; fi
}
expect_fail() {
	if [ "$RC" = 0 ]; then fail "exit status 0, expected a failure${1:+ ($1)}; output:$NL$(sed 's/^/      | /' "$OUT")"; fi
}
expect_out() {
	if ! grep -F -e "$1" "$OUT" >/dev/null; then fail "output lacks \"$1\"; output:$NL$(sed 's/^/      | /' "$OUT")"; fi
}
expect_no_out() {
	if grep -F -e "$1" "$OUT" >/dev/null; then fail "output contains \"$1\""; fi
}
expect_count() {
	_ec=$(grep -c -F -e "$1" "$OUT")
	if [ "$_ec" != "$2" ]; then fail "\"$1\" appears $_ec times in the output, expected $2"; fi
}
expect_file() {
	if [ ! -f "$1" ]; then fail "missing file $1"; fi
}
expect_exec() {
	if [ ! -f "$1" ] || [ ! -x "$1" ]; then fail "missing executable $1"; fi
}
expect_absent() {
	if [ -e "$1" ] || [ -L "$1" ]; then fail "$1 exists, and should not"; fi
}
expect_dir() {
	if [ ! -d "$1" ]; then fail "missing directory $1"; fi
}
expect_eq() {
	if [ "$1" != "$2" ]; then fail "${3:-values differ}: got [$1], expected [$2]"; fi
}

# snap DIR: everything about what is inside DIR -- names, types, modes, sizes, mtimes, link
# targets and content hashes -- so two snaps are equal only if nothing in it changed at all. DIR's
# own mtime is left out: every locked run creates and removes its cache directory (and so its lock),
# which touches the directory holding it and nothing else.
snap() {
	if [ ! -e "$1" ]; then
		echo absent
		return 0
	fi
	(
		cd "$1" || exit 1
		find . -mindepth 1 -printf '%p %y %m %s %T@ %l\n' | LC_ALL=C sort
		find . -type f -exec sha256sum {} + | LC_ALL=C sort
	)
}

# snap_but_lock DIR: snap, except the mtimes of the directories a locked run creates its lock in
# and removes it from again (<cache>, <cache>/eitri) -- for a HOME whose cache already existed.
snap_but_lock() {
	snap "$1" | sed -E 's#^(\./\.cache(/eitri)? d [0-7]+ [0-9]+) [0-9.]+ #\1 - #'
}

# snap_but_staging DIR: snap, except the mtimes of the directories a fresh install or an upgrade
# that fails creates and removes its own entries in, leaving every file as it was: .local/lib
# (eitri.new, a swap's renames), the three the launcher, desktop entry and licences are staged in
# before the swap, and -- for a run against a HOME with no prior install at all -- .local itself,
# whose own mtime changes the moment unpack_new's mkdir -p makes .local/lib the first time (plan
# Task 10 review: reached once a fatal sidecar-build failure could die there too).
snap_but_staging() {
	snap "$1" | sed -E 's#^(\./\.local(/(lib|bin|share/applications|share/licenses/eitri))? d [0-7]+ [0-9]+) [0-9.]+ #\1 - #'
}

# tree DIR: names and types only, excluding the planted editors.
tree() {
	(
		cd "$1" || exit 1
		find . -mindepth 1 -printf '%p %y\n' | LC_ALL=C sort |
			grep -v -e '^\./\.local/bin/nvim f$' -e '^\./\.local/bin/vim f$'
	)
}

# use_home DIR: make DIR (created) this test's HOME, with the planted nvim and vim.
use_home() {
	TH=$1
	mkdir -p "$TH/.local/bin"
	printf '#!/bin/sh\necho "planted nvim, never to be touched"\n' >"$TH/.local/bin/nvim"
	printf '#!/bin/sh\necho "planted vim, never to be touched"\n' >"$TH/.local/bin/vim"
	chmod 0755 "$TH/.local/bin/nvim" "$TH/.local/bin/vim"
	sha256sum "$TH/.local/bin/nvim" "$TH/.local/bin/vim" >>"$T/planted.sha256"
}

data_of() { printf '%s\n' "$TH/.local/share"; }

# plant_sidecar REV7 [DATA]: a sidecar that is *present* (spec §5.3 step 6) for REV7.
plant_sidecar() {
	_ps_dir=${2:-$TH/.local/share}/eitri/sidecar/$1
	mkdir -p "$_ps_dir"
	printf '#!/bin/sh\nprintf "%%s\\n" "%s" "claude-agent-sdk 0.3.252 (bundled claude code 2.1.252)" "supported claude code CLI: >=2.1.252 <3.0.0" "executable sources served: host_cli"\n' \
		"$SIDECAR_LINE" >"$_ps_dir/verdandi-claude-sidecar"
	chmod 0755 "$_ps_dir/verdandi-claude-sidecar"
	printf 'VERDANDI_REV=%s\nSIDECAR_VERSION_LINE=%s\nBUILT_AT=2026-09-27T00:00:00Z\n' "$1" "$SIDECAR_LINE" >"$_ps_dir/BUILD"
}

installed_version() {
	sed -n 's/^EITRI_VERSION=//p' "$TH/.local/lib/eitri/RELEASE" 2>/dev/null
}

# srv_mark / srv_paths: the request paths the fixture server logged since the mark.
srv_mark() { SRV_MARK=$(wc -l <"$SRV_LOG"); }
srv_paths() {
	tail -n +"$((SRV_MARK + 1))" "$SRV_LOG" | sed -n 's/.*"GET \([^ ]*\) HTTP.*/\1/p'
}

after_each() {
	if [ -s "$S/logs/forbidden.log" ]; then fail "a forbidden command ran: $(cat "$S/logs/forbidden.log")"; fi
	if [ -s "$T/planted.sha256" ] && ! sha256sum -c --quiet "$T/planted.sha256" >/dev/null 2>&1; then
		fail "a planted nvim/vim changed or vanished"
	fi
	if [ -s "$S/logs/claude.log" ] && grep -v -x 'claude-stub --version' "$S/logs/claude.log" >/dev/null; then
		fail "claude was called with something other than --version: $(cat "$S/logs/claude.log")"
	fi
	if [ -n "$(ls -A "$T/cwd" 2>/dev/null)" ]; then fail "the installer wrote into its cwd: $(ls -A "$T/cwd")"; fi
	if [ -n "$(find "$T" -name '~' 2>/dev/null)" ]; then fail "a path named ~ was created: $(find "$T" -name '~')"; fi
}

# ---------------------------------------------------------------------------------------------

TESTS=
. "$HERE/test_layout.sh"
. "$HERE/test_recovery.sh"
. "$HERE/test_verify.sh"
. "$HERE/test_net.sh"
. "$HERE/test_system.sh"
. "$HERE/test_uninstall.sh"
. "$HERE/test_sidecar.sh"
. "$HERE/test_nvim.sh"
. "$HERE/test_from_source.sh"
. "$HERE/test_apparmor.sh"

# The scratch must be new: only the fixture server's own files may already be there.
for e in "$S"/* "$S"/.[!.]*; do
	[ -e "$e" ] || continue
	case ${e##*/} in
	srv | srv.log | srv.out) ;;
	*)
		echo "harness: --scratch $S is not empty" >&2
		exit 2
		;;
	esac
done
mkdir -p "$S"
trap 'stop_server' EXIT
trap 'exit 130' INT TERM

printf '# installer shell: %s (%s)\n' "$NV_SH" "$(readlink -f "$(command -v "$NV_SH")")"
setup_stubs
setup_keys
setup_libs
setup_sidecar_fixtures
setup_skia_fixture
setup_nvim_fixtures
setup_releases
start_server

N=0
FAILED=0
FAILED_NAMES=
for t in $TESTS; do
	if [ -n "$ONLY" ]; then
		case $ONLY in *" $t "*) ;; *) continue ;; esac
	fi
	N=$((N + 1))
	T=$S/t/$t
	mkdir -p "$T/cwd"
	for l in forbidden claude curl wget mv swap cargo swap-verdandi; do : >"$S/logs/$l.log"; done
	rm -f "$S/logs/swap" "$S/logs/swap-verdandi" "$S/logs/fake-owner-path"
	(
		FAILS=0
		RUNS=0
		OUT=$T/out
		use_home "$T/home"
		"$t"
		after_each
		exit "$FAILS"
	) >"$T/log" 2>&1
	rc=$?
	if [ "$rc" = 0 ]; then
		printf 'ok %d - %s\n' "$N" "$t"
	else
		FAILED=$((FAILED + 1))
		FAILED_NAMES="$FAILED_NAMES $t"
		printf 'not ok %d - %s\n' "$N" "$t"
		sed 's/^/  /' "$T/log"
	fi
done
printf '# %d tests, %d failed%s\n' "$N" "$FAILED" "${FAILED_NAMES:+:$FAILED_NAMES}"
[ "$N" -gt 0 ] || { echo "harness: no test ran" >&2; exit 2; }
[ "$FAILED" = 0 ]
