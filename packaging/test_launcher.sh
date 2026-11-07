#!/usr/bin/env bash
# Tests for packaging/eitri.launcher.sh. Standalone: `bash packaging/test_launcher.sh`.
# Builds a fake install tree under ~/.cache/nv-v1dist-t8/launcher-test-XXXXXX/ (never /tmp), with a
# `bin/eitri` copy of the REAL launcher script and a stub `lib/eitri/shell` that records its
# own argv and the environment this suite cares about, then runs the real launcher against it.
#
# Never touches the real $HOME: every invocation below runs with HOME pointed at a scratch
# directory this script creates, so the launcher's own `$HOME`-relative defaults (the XDG_DATA_HOME
# fallback) resolve inside the scratch tree rather than the real user's.
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAUNCHER="$HERE/eitri.launcher.sh"

# A name no other run can share: a process id is not one (two runs in separate PID namespaces, or
# a later run after a pid wrapped, can have the same $$ and would delete each other's scratch).
mkdir -p "$HOME/.cache/nv-v1dist-t8"
SCRATCH_ROOT=$(mktemp -d "$HOME/.cache/nv-v1dist-t8/launcher-test-XXXXXX")
trap 'rm -rf "$SCRATCH_ROOT"' EXIT

ROOT="$SCRATCH_ROOT/install"
FAKE_HOME="$SCRATCH_ROOT/home"
PROJECT_DIR="$SCRATCH_ROOT/project"
mkdir -p "$ROOT/bin" "$ROOT/lib/eitri" "$FAKE_HOME" "$PROJECT_DIR"
# `$ROOT/bin/eitri` is a real COPY of the launcher, the way a `.deb`/tarball install actually
# lays it out (nfpm's `contents` entry copies the tracked file; it never symlinks it) -- so LIBDIR
# for every ordinary test below is simply `$ROOT/bin/eitri`'s own sibling `../lib/eitri`.
cp "$LAUNCHER" "$ROOT/bin/eitri"
chmod +x "$ROOT/bin/eitri"

cat >"$ROOT/lib/eitri/shell" <<'STUB'
#!/usr/bin/env bash
# Test double for the real `shell` binary: records what it was invoked with rather than opening a
# window, so packaging/test_launcher.sh can assert on the launcher's own decisions.
echo "STUB_SELF:$0"
if [[ "${1:-}" == --version ]]; then
	echo "STUB_VERSION:eitri-stub 0.0.0-test"
	exit 0
fi
for a in "$@"; do
	echo "STUB_ARG:$a"
done
STUB
chmod +x "$ROOT/lib/eitri/shell"

cat >"$ROOT/lib/eitri/eitri-setup" <<'STUB'
#!/bin/sh
# Test double for packaging/install.sh acting as eitri-setup: records each invocation on its own
# (a plain `eitri setup` runs it up to twice -- --sidecar-only, then --nvim-offer), so
# packaging/test_launcher.sh can assert on how many times the launcher ran it and with what argv.
echo "STUB_SETUP_CALL"
for a in "$@"; do
	echo "STUB_SETUP_ARG:$a"
done
STUB
chmod +x "$ROOT/lib/eitri/eitri-setup"

FAILURES=0
CHECKS=0

# assert_contains OUTPUT NEEDLE DESCRIPTION
assert_contains() {
	CHECKS=$((CHECKS + 1))
	if [[ "$1" == *"$2"* ]]; then
		echo "ok - $3"
	else
		echo "FAIL - $3"
		echo "  expected to find: $2"
		echo "  in: $1"
		FAILURES=$((FAILURES + 1))
	fi
}

# assert_not_contains OUTPUT NEEDLE DESCRIPTION
assert_not_contains() {
	CHECKS=$((CHECKS + 1))
	if [[ "$1" != *"$2"* ]]; then
		echo "ok - $3"
	else
		echo "FAIL - $3"
		echo "  expected NOT to find: $2"
		echo "  in: $1"
		FAILURES=$((FAILURES + 1))
	fi
}

assert_eq() {
	CHECKS=$((CHECKS + 1))
	if [[ "$1" == "$2" ]]; then
		echo "ok - $3"
	else
		echo "FAIL - $3 (got $1, want $2)"
		FAILURES=$((FAILURES + 1))
	fi
}

# assert_count OUTPUT NEEDLE COUNT DESCRIPTION
assert_count() {
	CHECKS=$((CHECKS + 1))
	local have
	have="$(grep -Fc -- "$2" <<<"$1")"
	if [[ "$have" == "$3" ]]; then
		echo "ok - $4"
	else
		echo "FAIL - $4 (found $2 $have time(s), want $3)"
		echo "  in: $1"
		FAILURES=$((FAILURES + 1))
	fi
}

# Reset the sidecar-discoverable state the hint checks: no RELEASE, no packaged sibling, no
# per-user rev-keyed sidecar, and a fresh (empty) fake $HOME so no earlier case's files leak into
# the next one.
reset_state() {
	rm -f "$ROOT/lib/eitri/RELEASE" "$ROOT/lib/eitri/verdandi-claude-sidecar"
	rm -rf "$FAKE_HOME"
	mkdir -p "$FAKE_HOME"
}

run_launcher() {
	# A clean environment for every run: only what the launcher and its stub need, so a variable
	# left exported by the harness (or by a previous case) never leaks in.
	env -i HOME="$FAKE_HOME" PATH="$PATH" "${EXTRA_ENV[@]}" "$ROOT/bin/eitri" "$@" 2>&1
}

echo "== LIBDIR resolves through a symlinked bin/eitri =="
# A real bin/eitri is a plain copy, but users or distro alternatives can point a separate
# entry at it (~/bin/eitri -> ~/.local/bin/eitri, for example). Follow the launcher's file
# symlinks so LIBDIR is the target copy's sibling lib/eitri, not the symlink entry's sibling.
reset_state
SYMLINK_ENTRY_DIR="$SCRATCH_ROOT/other-bin"
mkdir -p "$SYMLINK_ENTRY_DIR"
ln -s "$ROOT/bin/eitri" "$SYMLINK_ENTRY_DIR/eitri"
EXTRA_ENV=()
out="$(env -i HOME="$FAKE_HOME" PATH="$PATH" "$SYMLINK_ENTRY_DIR/eitri" --version 2>&1)"
assert_contains "$out" "STUB_SELF:$ROOT/lib/eitri/shell" "invoked through a symlink, LIBDIR still resolves to the real script's own lib/eitri"
assert_contains "$out" "STUB_VERSION:" "--version reached the stub through the symlink"

echo "== LIBDIR resolves through a RELATIVE symlink target (GNU stow's own shape) =="
# Exercise a relative symlink target, as GNU stow uses ~/bin/eitri -> ../.local/bin/eitri.
# The absolute-target case above does not exercise resolve_launcher_path's relative branch.
reset_state
RELSYM_ENTRY_DIR="$SCRATCH_ROOT/relsym-bin"
mkdir -p "$RELSYM_ENTRY_DIR"
(cd "$RELSYM_ENTRY_DIR" && ln -s ../install/bin/eitri eitri)
EXTRA_ENV=()
out="$(env -i HOME="$FAKE_HOME" PATH="$PATH" "$RELSYM_ENTRY_DIR/eitri" --version 2>&1)"
assert_contains "$out" "STUB_SELF:$ROOT/lib/eitri/shell" "a relative symlink target still resolves LIBDIR to the real script's own lib/eitri"
assert_contains "$out" "STUB_VERSION:" "--version reached the stub through the relative symlink"

echo "== LIBDIR resolves when the launcher's own bindir is itself a symlinked directory =="
# GNU stow can symlink the bin directory while keeping the launcher file a plain copy.
# Keep directory symlinks logical: readlink -f on the whole path would locate lib/eitri beside
# the symlink target rather than beside the install's named bin directory. Only the launcher's
# own file symlink chain may be followed; logical bin/../lib/eitri selects its sibling.
reset_state
SYMDIR_REALBIN="$SCRATCH_ROOT/symdir-realbin"
SYMDIR_ROOT="$SCRATCH_ROOT/symdir-root"
rm -rf "$SYMDIR_REALBIN" "$SYMDIR_ROOT"
mkdir -p "$SYMDIR_REALBIN" "$SYMDIR_ROOT/lib/eitri"
cp "$LAUNCHER" "$SYMDIR_REALBIN/eitri"
chmod +x "$SYMDIR_REALBIN/eitri"
ln -s "$SYMDIR_REALBIN" "$SYMDIR_ROOT/bin"
cp "$ROOT/lib/eitri/shell" "$SYMDIR_ROOT/lib/eitri/shell"
chmod +x "$SYMDIR_ROOT/lib/eitri/shell"
EXTRA_ENV=()
out="$(env -i HOME="$FAKE_HOME" PATH="$PATH" "$SYMDIR_ROOT/bin/eitri" --version 2>&1)"
assert_contains "$out" "STUB_SELF:$SYMDIR_ROOT/lib/eitri/shell" "a symlinked bindir still resolves LIBDIR to this install's own sibling lib/eitri"
assert_contains "$out" "STUB_VERSION:" "--version reached the stub through the symlinked bindir"
assert_not_contains "$out" "No such file or directory" "no broken cd through the symlinked bindir"

echo "== ... and still resolves under POSIXLY_CORRECT=1 =="
# POSIX-mode cd resolves bin/../lib/eitri logically, removing bin as named even if symlinked.
# Only its extra physical retry is disabled in POSIX mode. The supported install layout must
# work without that retry; it would only help an unsupported layout with lib/eitri beside
# the directory symlink's target and no lib/eitri beside the named bin directory.
out="$(env -i HOME="$FAKE_HOME" POSIXLY_CORRECT=1 PATH="$PATH" "$SYMDIR_ROOT/bin/eitri" --version 2>&1)"
assert_contains "$out" "STUB_SELF:$SYMDIR_ROOT/lib/eitri/shell" "a symlinked bindir resolves LIBDIR under POSIXLY_CORRECT=1 too"
assert_not_contains "$out" "No such file or directory" "no broken cd through the symlinked bindir under POSIXLY_CORRECT=1"

echo "== an exported CDPATH does not corrupt LIBDIR through a RELATIVE invocation =="
# With a relative $0 (an unpacked bin/eitri or a relative PATH entry), an exported CDPATH
# containing . makes cd print its destination before pwd. Those two lines corrupt LIBDIR,
# so clear CDPATH for the directory lookup before exec "$LIBDIR/shell".
reset_state
CDPATH_ENTRY_DIR="$SCRATCH_ROOT/cdpath-relative"
rm -rf "$CDPATH_ENTRY_DIR"
mkdir -p "$CDPATH_ENTRY_DIR/inst/bin" "$CDPATH_ENTRY_DIR/inst/lib/eitri"
cp "$LAUNCHER" "$CDPATH_ENTRY_DIR/inst/bin/eitri"
chmod +x "$CDPATH_ENTRY_DIR/inst/bin/eitri"
cp "$ROOT/lib/eitri/shell" "$CDPATH_ENTRY_DIR/inst/lib/eitri/shell"
chmod +x "$CDPATH_ENTRY_DIR/inst/lib/eitri/shell"
out="$(cd "$CDPATH_ENTRY_DIR" && env -i HOME="$FAKE_HOME" CDPATH=.: PATH=/usr/bin:/bin bash inst/bin/eitri --version 2>&1)"
assert_contains "$out" "STUB_SELF:$CDPATH_ENTRY_DIR/inst/lib/eitri/shell" "a relative launch under an exported CDPATH still resolves LIBDIR to one clean line"
assert_contains "$out" "STUB_VERSION:" "--version reached the stub despite the exported CDPATH"
assert_not_contains "$out" "No such file or directory" "no broken cd from a CDPATH-doubled LIBDIR"
out="$(cd "$CDPATH_ENTRY_DIR" && env -i HOME="$FAKE_HOME" CDPATH=.: PATH="inst/bin:/usr/bin:/bin" bash -c 'eitri --version' 2>&1)"
assert_contains "$out" "STUB_SELF:$CDPATH_ENTRY_DIR/inst/lib/eitri/shell" "a relative PATH entry under an exported CDPATH also resolves cleanly"

echo "== LIBDIR resolves when the launcher's own directory is spelled '.' or ends in '/.' =="
# Append .. to the bindir as spelled: dirname . is ., not .. . From inside bin/, ./eitri
# must find ../lib/eitri, and from the release root bin/./eitri must find bin/./../lib/eitri.
reset_state
out="$(cd "$ROOT/bin" && env -i HOME="$FAKE_HOME" PATH=/usr/bin:/bin ./eitri --version 2>&1)"
assert_contains "$out" "STUB_SELF:$ROOT/lib/eitri/shell" "./eitri run from inside bin/ resolves LIBDIR to the sibling lib/eitri"
assert_not_contains "$out" "No such file or directory" "no broken cd for ./eitri run from inside bin/"
out="$(cd "$ROOT" && env -i HOME="$FAKE_HOME" PATH=/usr/bin:/bin bin/./eitri --version 2>&1)"
assert_contains "$out" "STUB_SELF:$ROOT/lib/eitri/shell" "bin/./eitri run from the release root resolves LIBDIR to lib/eitri"
out="$(cd "$ROOT" && env -i HOME="$FAKE_HOME" POSIXLY_CORRECT=1 CDPATH=.: PATH=/usr/bin:/bin bin/./eitri --version 2>&1)"
assert_contains "$out" "STUB_SELF:$ROOT/lib/eitri/shell" "bin/./eitri resolves LIBDIR under POSIXLY_CORRECT=1 with an exported CDPATH too"

echo "== --version works and skips project/hint output =="
reset_state
EXTRA_ENV=()
out="$(run_launcher --version)"
assert_not_contains "$out" "eitri    project" "--version never resolves or prints a project"
assert_not_contains "$out" "no sidecar built yet" "--version never prints the sidecar hint"

echo "== --legacy passes through to shell's argv, no env export =="
reset_state
EXTRA_ENV=()
out="$(run_launcher --legacy "$PROJECT_DIR")"
assert_contains "$out" "STUB_ARG:--legacy" "the stub received --legacy on argv"
assert_not_contains "$out" "EITRI_AGENT_BACKEND" "no EITRI_AGENT_BACKEND is exported for --legacy"

echo "== --clean passes through to shell's argv, before the project =="
reset_state
EXTRA_ENV=()
out="$(run_launcher --clean "$PROJECT_DIR")"
assert_contains "$out" "STUB_ARG:--clean" "the stub received --clean on argv"
assert_not_contains "$out" "unknown option" "--clean is not refused as an unknown option"
out="$(run_launcher --clean --legacy "$PROJECT_DIR")"
assert_contains "$out" "STUB_ARG:--clean" "--clean still reaches the stub beside --legacy"
assert_contains "$out" "STUB_ARG:--legacy" "--legacy still reaches the stub beside --clean"

echo "== no sidecar anywhere -> the hint is printed =="
reset_state
EXTRA_ENV=()
out="$(run_launcher "$PROJECT_DIR")"
assert_contains "$out" 'no sidecar built yet -- run "eitri setup"' "the hint fires with nothing installed"

echo "== EITRI_SIDECAR_BINARY set and executable -> no hint =="
reset_state
sidecar_bin="$SCRATCH_ROOT/explicit-sidecar"
printf '#!/bin/sh\n' >"$sidecar_bin"
chmod +x "$sidecar_bin"
EXTRA_ENV=("EITRI_SIDECAR_BINARY=$sidecar_bin")
out="$(run_launcher "$PROJECT_DIR")"
assert_not_contains "$out" "no sidecar built yet" "an explicit EITRI_SIDECAR_BINARY suppresses the hint"

echo "== a packaged sibling at \$LIBDIR/verdandi-claude-sidecar -> no hint =="
reset_state
EXTRA_ENV=()
printf '#!/bin/sh\n' >"$ROOT/lib/eitri/verdandi-claude-sidecar"
chmod +x "$ROOT/lib/eitri/verdandi-claude-sidecar"
out="$(run_launcher "$PROJECT_DIR")"
assert_not_contains "$out" "no sidecar built yet" "a packaged sibling suppresses the hint"

echo "== a RELEASE with no matching sidecar anywhere -> the hint still fires =="
reset_state
EXTRA_ENV=()
printf 'VERDANDI_REV=abcdef0123456789abcdef0123456789abcdef01\n' >"$ROOT/lib/eitri/RELEASE"
out="$(run_launcher "$PROJECT_DIR")"
assert_contains "$out" "no sidecar built yet" "RELEASE alone (no built sidecar) still hints"

echo "== relative XDG_DATA_HOME: the rev-keyed sidecar under \$HOME/.local/share suppresses the hint =="
reset_state
printf 'VERDANDI_REV=abcdef0123456789abcdef0123456789abcdef01\n' >"$ROOT/lib/eitri/RELEASE"
sidecar_dir="$FAKE_HOME/.local/share/eitri/sidecar/abcdef0"
mkdir -p "$sidecar_dir"
printf '#!/bin/sh\n' >"$sidecar_dir/verdandi-claude-sidecar"
chmod +x "$sidecar_dir/verdandi-claude-sidecar"
# Treat a relative XDG_DATA_HOME as unset, so the sidecar is found in $HOME/.local/share.
# Using ${XDG_DATA_HOME:-default} would search relative/xdg beneath the working directory
# and wrongly print the missing-sidecar hint.
EXTRA_ENV=("XDG_DATA_HOME=relative/xdg")
out="$(run_launcher "$PROJECT_DIR")"
assert_not_contains "$out" "no sidecar built yet" "a relative XDG_DATA_HOME falls back to \$HOME/.local/share, where the sidecar sits"

echo "== an EMPTY XDG_DATA_HOME also falls back to \$HOME/.local/share =="
EXTRA_ENV=("XDG_DATA_HOME=")
out="$(run_launcher "$PROJECT_DIR")"
assert_not_contains "$out" "no sidecar built yet" "an empty XDG_DATA_HOME falls back the same way"

echo "== an absolute XDG_DATA_HOME elsewhere is honoured, and finds nothing there -> the hint fires =="
elsewhere="$SCRATCH_ROOT/elsewhere-xdg"
mkdir -p "$elsewhere"
EXTRA_ENV=("XDG_DATA_HOME=$elsewhere")
out="$(run_launcher "$PROJECT_DIR")"
assert_contains "$out" "no sidecar built yet" "an absolute XDG_DATA_HOME is honoured, and the sidecar built under the old one is not found there"

# Explicit setup modes must pass through alone: prepending --sidecar-only to --nvim-only,
# --uninstall or --sidecar-only makes the installer reject conflicting modes, including the
# recovery command it prints for a failed nvim download. The stub records dispatch only.
#
# Plain setup runs --sidecar-only followed by --nvim-offer. Unlike --nvim-only, the offer
# skips an adequate PATH nvim and tolerates a download failure, so setup does not turn
# nothing-to-do or a temporary network failure into a hard failure. The stub cannot check
# that behaviour; packaging/tests/install/test_nvim.sh exercises the installer-side offer.
echo "== LuaLS setup reaches the binary without invoking the installer =="
for target in lua-ls lua_ls luals; do
	out="$(run_launcher setup "$target")"
	assert_contains "$out" "STUB_ARG:setup" "setup $target reaches shell setup"
	assert_contains "$out" "STUB_ARG:$target" "setup $target keeps the target"
	assert_not_contains "$out" "STUB_SETUP_CALL" "setup $target starts no installer"
	assert_not_contains "$out" "no sidecar built yet" "setup $target prints no sidecar hint"
done

echo "== plain 'eitri setup' builds the sidecar and then runs the nvim offer =="
EXTRA_ENV=()
out="$(run_launcher setup)"
assert_count "$out" "STUB_SETUP_CALL" 2 "eitri-setup ran twice"
assert_contains "$out" $'STUB_SETUP_ARG:--sidecar-only\nSTUB_SETUP_CALL\nSTUB_SETUP_ARG:--nvim-offer' \
	"the sidecar-only call precedes the nvim-offer call, each carrying only its own mode flag"

echo "== 'eitri setup --no-nvim' builds the sidecar only: --no-nvim is honoured =="
out="$(run_launcher setup --no-nvim)"
assert_count "$out" "STUB_SETUP_CALL" 1 "eitri-setup ran once"
assert_contains "$out" "STUB_SETUP_ARG:--sidecar-only" "the one call is --sidecar-only"
assert_contains "$out" "STUB_SETUP_ARG:--no-nvim" "--no-nvim reached it"
assert_not_contains "$out" "STUB_SETUP_ARG:--nvim-offer" "the nvim step never ran"

echo "== 'eitri setup --with-nvim' is no longer ignored: the nvim-offer step now runs too =="
out="$(run_launcher setup --with-nvim)"
assert_count "$out" "STUB_SETUP_CALL" 2 "eitri-setup ran twice"
assert_contains "$out" $'STUB_SETUP_ARG:--sidecar-only\nSTUB_SETUP_ARG:--with-nvim\nSTUB_SETUP_CALL\nSTUB_SETUP_ARG:--nvim-offer\nSTUB_SETUP_ARG:--with-nvim' \
	"both calls happen, and --with-nvim reaches both"

echo "== 'eitri setup --nvim-only' passes straight through as its own mode, never combined =="
out="$(run_launcher setup --nvim-only)"
assert_count "$out" "STUB_SETUP_CALL" 1 "eitri-setup ran once"
assert_contains "$out" "STUB_SETUP_ARG:--nvim-only" "the one call is --nvim-only"
assert_not_contains "$out" "STUB_SETUP_ARG:--sidecar-only" "--sidecar-only was never prepended (this used to die \"--sidecar-only and --nvim-only cannot be combined\")"
assert_not_contains "$out" "STUB_SETUP_ARG:--nvim-offer" "the explicit --nvim-only is never rewritten into --nvim-offer"

echo "== 'eitri setup --help' prints the installer's usage exactly once, never twice =="
out="$(run_launcher setup --help)"
assert_count "$out" "STUB_SETUP_CALL" 1 "eitri-setup ran once, not once per mode"
assert_contains "$out" "STUB_SETUP_ARG:--help" "--help reached it"
assert_not_contains "$out" "STUB_SETUP_ARG:--sidecar-only" "--sidecar-only was never prepended for --help"

echo "== 'eitri setup -h' prints the installer's usage exactly once too =="
out="$(run_launcher setup -h)"
assert_count "$out" "STUB_SETUP_CALL" 1 "eitri-setup ran once, not once per mode"
assert_contains "$out" "STUB_SETUP_ARG:-h" "-h reached it"
assert_not_contains "$out" "STUB_SETUP_ARG:--sidecar-only" "--sidecar-only was never prepended for -h"

echo "== 'eitri setup --uninstall --purge' passes straight through as its own mode =="
out="$(run_launcher setup --uninstall --purge)"
assert_count "$out" "STUB_SETUP_CALL" 1 "eitri-setup ran once"
assert_contains "$out" "STUB_SETUP_ARG:--uninstall" "--uninstall reached it"
assert_contains "$out" "STUB_SETUP_ARG:--purge" "--purge reached it"
assert_not_contains "$out" "STUB_SETUP_ARG:--sidecar-only" "--sidecar-only was never prepended"

echo "== 'eitri setup --sidecar-only' passes straight through, never doubled =="
out="$(run_launcher setup --sidecar-only)"
assert_count "$out" "STUB_SETUP_CALL" 1 "eitri-setup ran once"
assert_count "$out" "STUB_SETUP_ARG:--sidecar-only" 1 "--sidecar-only appears exactly once"

# An explicit --nvim-offer must pass through alone: prepending --sidecar-only creates
# conflicting modes even though --nvim-offer --dry-run is valid installer usage.
echo "== 'eitri setup --nvim-offer' passes straight through as its own mode =="
out="$(run_launcher setup --nvim-offer --dry-run)"
assert_count "$out" "STUB_SETUP_CALL" 1 "eitri-setup ran once"
assert_count "$out" "STUB_SETUP_ARG:--nvim-offer" 1 "--nvim-offer appears exactly once"
assert_contains "$out" "STUB_SETUP_ARG:--dry-run" "--dry-run reached it"
assert_not_contains "$out" "STUB_SETUP_ARG:--sidecar-only" "--sidecar-only was never prepended (this used to die \"--sidecar-only and --nvim-offer cannot be combined\")"

# The guard that keeps the list from drifting again: every mode packaging/install.sh's own parse_args
# hands to set_mode (whose refusal of a second mode is what a prepended --sidecar-only runs into) must
# reach eitri-setup alone, exactly once, with nothing prepended. The mode names are read out of
# the real installer, so a mode added there without being added to the launcher fails here.
echo "== every installer mode passes straight through 'eitri setup', read from install.sh itself =="
INSTALLER_MODES="$(grep -o 'set_mode [a-z][a-z-]*' "$HERE/install.sh" | sed 's/^set_mode /--/' | LC_ALL=C sort -u)"
mode_count="$(printf '%s\n' "$INSTALLER_MODES" | grep -c '^--')"
CHECKS=$((CHECKS + 1))
if [[ "$mode_count" -ge 6 && "$INSTALLER_MODES" == *--nvim-offer* && "$INSTALLER_MODES" == *--uninstall* ]]; then
	echo "ok - read $mode_count modes out of packaging/install.sh"
else
	echo "FAIL - could not read packaging/install.sh's modes (got: $INSTALLER_MODES): the guard below would be vacuous"
	FAILURES=$((FAILURES + 1))
fi
for mode in $INSTALLER_MODES; do
	out="$(run_launcher setup "$mode")"
	assert_count "$out" "STUB_SETUP_CALL" 1 "'eitri setup $mode' ran eitri-setup once"
	assert_count "$out" "STUB_SETUP_ARG:$mode" 1 "'eitri setup $mode' passed $mode exactly once"
	if [[ "$mode" != --sidecar-only ]]; then
		assert_not_contains "$out" "STUB_SETUP_ARG:--sidecar-only" "'eitri setup $mode' prepended no --sidecar-only"
	fi
done

# `eitri sidecar <verb>` is one call of eitri-setup with one mode, and what follows the verb goes with it.
echo "== 'eitri sidecar <verb>' reaches eitri-setup --sidecar-<verb> alone =="
reset_state
EXTRA_ENV=()
for verb in status check update; do
	out="$(run_launcher sidecar "$verb")"
	assert_count "$out" "STUB_SETUP_CALL" 1 "'eitri sidecar $verb' ran eitri-setup once"
	assert_contains "$out" "STUB_SETUP_ARG:--sidecar-$verb" "'eitri sidecar $verb' passed --sidecar-$verb"
	assert_count "$out" "STUB_SETUP_ARG:" 1 "'eitri sidecar $verb' passed nothing else"
	assert_not_contains "$out" "STUB_ARG:" "'eitri sidecar $verb' never reached shell"
	assert_not_contains "$out" "eitri    project" "'eitri sidecar $verb' printed no banner"
done
out="$(run_launcher sidecar use bundled)"
assert_count "$out" "STUB_SETUP_CALL" 1 "'eitri sidecar use bundled' ran eitri-setup once"
assert_eq "$(grep '^STUB_SETUP_ARG:' <<<"$out" | paste -sd'|' -)" "STUB_SETUP_ARG:--sidecar-use|STUB_SETUP_ARG:bundled" "use's argument follows its mode"
out="$(run_launcher sidecar use 0.3.2)"
assert_eq "$(grep '^STUB_SETUP_ARG:' <<<"$out" | paste -sd'|' -)" "STUB_SETUP_ARG:--sidecar-use|STUB_SETUP_ARG:0.3.2" "a version too"
out="$(run_launcher sidecar update --version 0.3.2)"
assert_eq "$(grep '^STUB_SETUP_ARG:' <<<"$out" | paste -sd'|' -)" "STUB_SETUP_ARG:--sidecar-update|STUB_SETUP_ARG:--version|STUB_SETUP_ARG:0.3.2" "update's --version follows its mode"

echo "== 'eitri sidecar use' with nothing to select =="
out="$(run_launcher sidecar use; echo "rc=$?")"
assert_contains "$out" "usage: eitri sidecar use bundled|VERSION" "it says what use takes"
assert_contains "$out" "rc=64" "it is a usage error"
assert_not_contains "$out" "--sidecar-use" "it names no flag nobody typed"
assert_not_contains "$out" "STUB_SETUP_CALL" "it runs nothing"
assert_not_contains "$out" "STUB_ARG:" "it opens no window"

echo "== 'eitri sidecar' with no verb, an unknown one, or asking for help =="
out="$(run_launcher sidecar; echo "rc=$?")"
assert_contains "$out" "eitri sidecar status" "no verb says what the verbs are"
assert_contains "$out" "rc=64" "no verb is a usage error"
assert_not_contains "$out" "STUB_SETUP_CALL" "no verb runs nothing"
assert_not_contains "$out" "STUB_ARG:" "no verb opens no window"
out="$(run_launcher sidecar frobnicate; echo "rc=$?")"
assert_contains "$out" "eitri sidecar: unknown command frobnicate" "an unknown verb is named"
assert_contains "$out" "rc=64" "an unknown verb is a usage error"
assert_not_contains "$out" "STUB_SETUP_CALL" "an unknown verb runs nothing"
for help in --help -h; do
	out="$(run_launcher sidecar "$help"; echo "rc=$?")"
	assert_contains "$out" "eitri sidecar use bundled" "'eitri sidecar $help' lists the verbs"
	assert_contains "$out" "rc=0" "'eitri sidecar $help' is no error"
done

echo "== a route with no eitri-setup is told its package updates the sidecar =="
mv "$ROOT/lib/eitri/eitri-setup" "$ROOT/lib/eitri/eitri-setup.away"
for verb in status check update "use bundled"; do
	# shellcheck disable=SC2086 # the verb's own words
	out="$(run_launcher sidecar $verb; echo "rc=$?")"
	assert_contains "$out" "eitri: this package updates its sidecar itself" "'eitri sidecar $verb' says so"
	assert_contains "$out" "rc=1" "'eitri sidecar $verb' fails"
	assert_not_contains "$out" "STUB_ARG:" "'eitri sidecar $verb' opens no window"
done
out="$(run_launcher sidecar --help; echo "rc=$?")"
assert_contains "$out" "eitri sidecar status" "--help still answers"
mv "$ROOT/lib/eitri/eitri-setup.away" "$ROOT/lib/eitri/eitri-setup"

echo "== a folder named sidecar opens with a path that says so =="
out="$(run_launcher ./sidecar 2>&1)"
assert_not_contains "$out" "STUB_SETUP_CALL" "./sidecar is a path"

echo "== the launcher's --help names the whole of 'eitri sidecar' =="
out="$(run_launcher --help)"
for words in "eitri sidecar status" "eitri sidecar check" "eitri sidecar update" "eitri sidecar use bundled"; do
	assert_contains "$out" "$words" "--help names $words"
done
assert_contains "$out" "eitri setup [args]" "--help still reaches its last usage line"
assert_not_contains "$out" "set -euo" "--help stops at the end of the header"

# The hint mirrors the order a tab uses: a selected update is a sidecar, when one would be used.
plant_selected_update() {
	local data="$1" version="$2" what="${3:-complete}"
	mkdir -p "$data/eitri/sidecar/updates/$version"
	printf 'version=%s\n' "$version" >"$data/eitri/sidecar/selected"
	if [[ $what != no-binary ]]; then
		printf '#!/bin/sh\n' >"$data/eitri/sidecar/updates/$version/verdandi-claude-sidecar"
		chmod +x "$data/eitri/sidecar/updates/$version/verdandi-claude-sidecar"
	fi
	if [[ $what != no-build ]]; then
		printf 'SIDECAR_VERSION_LINE=x\n' >"$data/eitri/sidecar/updates/$version/BUILD"
	fi
}
hint_with_release() {
	printf 'VERDANDI_REV=abcdef0000000000000000000000000000000000\nSIDECAR_VERSION=%s\n' "$1" >"$ROOT/lib/eitri/RELEASE"
}

echo "== sidecar_found: a selected update counts =="
reset_state
EXTRA_ENV=()
hint_with_release 0.2.0
plant_selected_update "$FAKE_HOME/.local/share" 0.3.2
out="$(run_launcher "$PROJECT_DIR")"
assert_not_contains "$out" "no sidecar built yet" "a complete, newer update suppresses the hint"

echo "== sidecar_found: an rc above the bundled version counts, and one below it does not =="
reset_state
hint_with_release 0.2.0
plant_selected_update "$FAKE_HOME/.local/share" 0.2.1-rc.1
out="$(run_launcher "$PROJECT_DIR")"
assert_not_contains "$out" "no sidecar built yet" "0.2.1-rc.1 is above 0.2.0"
reset_state
hint_with_release 0.2.1
plant_selected_update "$FAKE_HOME/.local/share" 0.2.1-rc.1
out="$(run_launcher "$PROJECT_DIR")"
assert_contains "$out" "no sidecar built yet" "0.2.1-rc.1 is below 0.2.1"
reset_state
hint_with_release 0.2.1-rc.1
plant_selected_update "$FAKE_HOME/.local/share" 0.2.1-rc.2
out="$(run_launcher "$PROJECT_DIR")"
assert_not_contains "$out" "no sidecar built yet" "rc.2 is above rc.1"
reset_state
hint_with_release 0.9.0
plant_selected_update "$FAKE_HOME/.local/share" 0.10.0
out="$(run_launcher "$PROJECT_DIR")"
assert_not_contains "$out" "no sidecar built yet" "numbers compare as numbers"

echo "== sidecar_found: an update a tab would pass over leaves the hint =="
for what in "0.2.0 complete" "0.1.9 complete" "0.3.2 no-binary" "0.3.2 no-build" "0.09.0 complete" \
	"0.3.2-rc.01 complete"; do
	reset_state
	hint_with_release 0.2.0
	# shellcheck disable=SC2086 # version, then state
	plant_selected_update "$FAKE_HOME/.local/share" $what
	out="$(run_launcher "$PROJECT_DIR")"
	assert_contains "$out" "no sidecar built yet" "selected $what leaves the hint"
done
for text in 'version=../x' 'junk' $'version=0.3.2\nmore' $'version=0.3.2\n' \
	"version=0.3.2$(printf '%300s' '' | tr ' ' '\n'; printf x)"; do
	reset_state
	hint_with_release 0.2.0
	plant_selected_update "$FAKE_HOME/.local/share" 0.3.2
	printf '%s\n' "$text" >"$FAKE_HOME/.local/share/eitri/sidecar/selected"
	out="$(run_launcher "$PROJECT_DIR")"
	assert_contains "$out" "no sidecar built yet" "a selected file reading [$text] leaves the hint"
done

reset_state
hint_with_release 0.2.0
plant_selected_update "$FAKE_HOME/.local/share" 0.3.2
printf 'version=0.3.2\000\n' >"$FAKE_HOME/.local/share/eitri/sidecar/selected"
out="$(run_launcher "$PROJECT_DIR")"
assert_contains "$out" "no sidecar built yet" "a selected file with a NUL in it leaves the hint"
reset_state
hint_with_release 0.2.0
plant_selected_update "$FAKE_HOME/.local/share" 0.3.2
printf 'version=0.3.2' >"$FAKE_HOME/.local/share/eitri/sidecar/selected"
out="$(run_launcher "$PROJECT_DIR")"
assert_not_contains "$out" "no sidecar built yet" "a selected file with no final newline still names it"

echo "== sidecar_found: no update applies with no eitri-setup beside the binary =="
reset_state
hint_with_release 0.2.0
plant_selected_update "$FAKE_HOME/.local/share" 0.3.2
mv "$ROOT/lib/eitri/eitri-setup" "$ROOT/lib/eitri/eitri-setup.away"
out="$(run_launcher "$PROJECT_DIR")"
mv "$ROOT/lib/eitri/eitri-setup.away" "$ROOT/lib/eitri/eitri-setup"
assert_contains "$out" "no sidecar built yet" "a selected update means nothing on such a route"

echo "== sidecar_found: the update is looked for where a tab looks =="
reset_state
hint_with_release 0.2.0
plant_selected_update "$FAKE_HOME/other-data" 0.3.2
EXTRA_ENV=("XDG_DATA_HOME=$FAKE_HOME/other-data")
out="$(run_launcher "$PROJECT_DIR")"
assert_not_contains "$out" "no sidecar built yet" "an absolute XDG_DATA_HOME is read"
EXTRA_ENV=()
out="$(run_launcher "$PROJECT_DIR")"
assert_contains "$out" "no sidecar built yet" "unset, it is not under that directory"
reset_state
hint_with_release 0.2.0
plant_selected_update "$FAKE_HOME/.local/share" 0.3.2
for value in "relative/dir" "relative/data" ""; do
	EXTRA_ENV=("XDG_DATA_HOME=$value")
	out="$(run_launcher "$PROJECT_DIR")"
	assert_not_contains "$out" "no sidecar built yet" "XDG_DATA_HOME [$value] means \$HOME/.local/share"
done
EXTRA_ENV=()

echo "== the launcher's --help mentions no internal design references =="
EXTRA_ENV=()
out="$(run_launcher --help)"
assert_not_contains "$out" "D16" "no ruling id in the help text"
assert_not_contains "$out" "LEGACY_NOT_IN_BUILD" "no Rust constant name in the help text"
assert_not_contains "$out" "spec " "no \"spec sec/\$N\" reference in the help text"

echo "== -h prints plain-word help, no internal ruling id =="
# -h/--help prints the launcher's header, so its user-facing help must avoid internal labels.
reset_state
EXTRA_ENV=()
out="$(run_launcher -h)"
assert_not_contains "$out" "D16" "the help text names no internal ruling id"
assert_not_contains "$out" "LEGACY_NOT_IN_BUILD" "the help text names no internal constant"
assert_contains "$out" "a release build refuses it" "the help text still says what a release build does, in plain words"
assert_contains "$out" "--clean" "the help text lists --clean"
assert_contains "$out" "eitri setup [args]" "the help text still ends with its last usage line"

echo "== the startup banner's project and account lines line up =="
# Pad eitri so account lines align with the project column at column 9.
reset_state
EXTRA_ENV=(VERDANDI_CLAUDE_ACCOUNT=work)
out="$(run_launcher)"
proj_line="$(printf '%s\n' "$out" | grep -m1 ' project ' || true)"
acct_line="$(printf '%s\n' "$out" | grep -m1 ' account ' || true)"
proj_col="${proj_line%%project*}"
acct_col="${acct_line%%account*}"
CHECKS=$((CHECKS + 1))
if [[ -n "$proj_line" && -n "$acct_line" && ${#proj_col} -eq ${#acct_col} ]]; then
	echo "ok - \"project\" and \"account\" start in the same column"
else
	echo "FAIL - \"project\" and \"account\" start in the same column"
	echo "  project line: $proj_line"
	echo "  account line: $acct_line"
	FAILURES=$((FAILURES + 1))
fi

echo "== eitri panel: the companion window's command line =="
# `eitri panel --nvim ADDR [DIR]` hands the address and the realpath of the project to `shell panel`;
# `--nvim` also exists in page mode; the one-window-only flags are refused in both.
reset_state
EXTRA_ENV=()
out="$(run_launcher panel --quiet --nvim /r/n.sock "$PROJECT_DIR")"
REAL_PROJECT="$(realpath -e -- "$PROJECT_DIR")"
args="$(printf '%s\n' "$out" | grep '^STUB_ARG:' | tr '\n' ' ')"
assert_eq "$args" "STUB_ARG:panel STUB_ARG:--nvim STUB_ARG:/r/n.sock STUB_ARG:-- STUB_ARG:$REAL_PROJECT " "panel execs shell panel --nvim ADDR -- <realpath>"
assert_contains "$out" "STUB_SELF:$ROOT/lib/eitri/shell" "panel runs the same shell binary"

out="$(run_launcher panel --quiet --nvim /r/n.sock -- "$PROJECT_DIR")"
args="$(printf '%s\n' "$out" | grep '^STUB_ARG:' | tr '\n' ' ')"
assert_eq "$args" "STUB_ARG:panel STUB_ARG:--nvim STUB_ARG:/r/n.sock STUB_ARG:-- STUB_ARG:$REAL_PROJECT " "a project after -- is taken the same way"

out="$(run_launcher panel --quiet "$PROJECT_DIR")"
args="$(printf '%s\n' "$out" | grep '^STUB_ARG:' | tr '\n' ' ')"
assert_eq "$args" "STUB_ARG:panel STUB_ARG:-- STUB_ARG:$REAL_PROJECT " "without --nvim no address is passed"

out="$(run_launcher panel --quiet --legacy "$PROJECT_DIR")"
assert_contains "$out" "eitri panel: unknown option --legacy" "panel refuses --legacy by name"
assert_not_contains "$out" "STUB_ARG" "panel --legacy never reaches shell"
(cd "$PROJECT_DIR" && env -i HOME="$FAKE_HOME" PATH="$PATH" "$ROOT/bin/eitri" panel --legacy >/dev/null 2>&1)
assert_eq "$?" "64" "panel --legacy exits 64"

(cd "$PROJECT_DIR" && env -i HOME="$FAKE_HOME" PATH="$PATH" "$ROOT/bin/eitri" panel --clean >/dev/null 2>&1)
assert_eq "$?" "64" "panel --clean exits 64"
out="$(run_launcher panel --quiet --clean)"
assert_contains "$out" "eitri panel: unknown option --clean" "panel refuses --clean by name"

out="$(run_launcher --nvim x "$PROJECT_DIR")"
assert_contains "$out" "eitri: unknown option --nvim" "--nvim in the integrated window mode is an unknown option"
(cd "$PROJECT_DIR" && env -i HOME="$FAKE_HOME" PATH="$PATH" "$ROOT/bin/eitri" --nvim x >/dev/null 2>&1)
assert_eq "$?" "64" "eitri --nvim x exits 64"

EXTRA_ENV=()
out="$(env -i HOME="$FAKE_HOME" PATH="$PATH" "$ROOT/bin/eitri" panel --nvim /r/n.sock 2>&1 </dev/null; true)"
assert_contains "$out" "project" "panel without --quiet prints the startup banner too"

# --account keeps working in panel mode: the stub reports the variable it was started with.
cat >"$ROOT/lib/eitri/shell" <<'STUB'
#!/usr/bin/env bash
echo "STUB_ACCOUNT:${VERDANDI_CLAUDE_ACCOUNT:-<unset>}"
for a in "$@"; do
	echo "STUB_ARG:$a"
done
STUB
out="$(run_launcher panel --quiet --account work --nvim /r/n.sock "$PROJECT_DIR")"
assert_contains "$out" "STUB_ACCOUNT:work" "eitri panel --account work exports VERDANDI_CLAUDE_ACCOUNT=work"
assert_contains "$out" "STUB_ARG:panel" "and still runs the panel"

# A project path with a space and a glob character survives as one argument.
SPACED="$SCRATCH_ROOT/my project [x]"
mkdir -p "$SPACED"
out="$(run_launcher panel --quiet --nvim "/r/a b.sock" "$SPACED")"
assert_contains "$out" "STUB_ARG:$(realpath -e -- "$SPACED")" "a project path with a space survives"
assert_contains "$out" "STUB_ARG:/r/a b.sock" "an address with a space survives"

# The one-window command line is unchanged.
out="$(run_launcher --quiet "$PROJECT_DIR")"
args="$(printf '%s\n' "$out" | grep '^STUB_ARG:' | tr '\n' ' ')"
assert_eq "$args" "STUB_ARG:$REAL_PROJECT " "without panel the shell gets only the project"

echo "== eitri open: the takeover command line =="
out="$(run_launcher open --quiet --nvim "/r/a b.sock" "$SPACED")"
args="$(printf '%s\n' "$out" | grep '^STUB_ARG:' | tr '\n' ' ')"
assert_eq "$args" "STUB_ARG:open STUB_ARG:--nvim STUB_ARG:/r/a b.sock STUB_ARG:-- STUB_ARG:$(realpath -e -- "$SPACED") " "open passes the address and directory separately"
out="$(run_launcher open --nvim /r/n.sock --new-window)"
assert_contains "$out" "eitri open: unknown option --new-window" "open refuses --new-window"
assert_not_contains "$out" "STUB_ARG" "a refused open runs no shell"
# No directory: nothing is filled in from the launcher's own directory, so the editor's working directory decides.
out="$(cd "$SPACED" && run_launcher open --quiet --nvim /r/n.sock)"
args="$(printf '%s\n' "$out" | grep '^STUB_ARG:' | tr '\n' ' ')"
assert_eq "$args" "STUB_ARG:open STUB_ARG:--nvim STUB_ARG:/r/n.sock " "open without a directory forwards none"
# Two directories are refused, with or without the separator; none is silently chosen.
out="$(run_launcher open --nvim /r/n.sock "$SPACED" "$PROJECT_DIR")"
assert_contains "$out" "eitri open: takes at most one project directory" "open refuses a second directory"
assert_not_contains "$out" "STUB_ARG" "a refused open runs no shell"
out="$(run_launcher open --nvim /r/n.sock -- "$SPACED" "$PROJECT_DIR")"
assert_contains "$out" "eitri open: takes at most one project directory" "open refuses a second directory after --"
assert_not_contains "$out" "STUB_ARG" "a refused open after -- runs no shell"

echo "== eitri split: the two-window command line =="
# `eitri split [DIR]` hands the realpath of the project to `shell split`; the options that belong to
# the integrated window or to the panel are refused by name, and --account still reaches the shell.
reset_state
EXTRA_ENV=()
out="$(run_launcher split --quiet "$PROJECT_DIR")"
args="$(printf '%s\n' "$out" | grep '^STUB_ARG:' | tr '\n' ' ')"
assert_eq "$args" "STUB_ARG:split STUB_ARG:-- STUB_ARG:$REAL_PROJECT " "split execs shell split -- <realpath>"

out="$(run_launcher split --quiet -- "$PROJECT_DIR")"
args="$(printf '%s\n' "$out" | grep '^STUB_ARG:' | tr '\n' ' ')"
assert_eq "$args" "STUB_ARG:split STUB_ARG:-- STUB_ARG:$REAL_PROJECT " "a project after -- is taken the same way"

for opt in --nvim --legacy --clean; do
	if [[ $opt == --nvim ]]; then
		out="$(run_launcher split --quiet --nvim /r/n.sock "$PROJECT_DIR")"
	else
		out="$(run_launcher split --quiet "$opt" "$PROJECT_DIR")"
	fi
	assert_contains "$out" "eitri split: unknown option $opt" "split refuses $opt by name"
	assert_not_contains "$out" "STUB_ARG" "split $opt never reaches shell"
	(cd "$PROJECT_DIR" && env -i HOME="$FAKE_HOME" PATH="$PATH" "$ROOT/bin/eitri" split "$opt" >/dev/null 2>&1)
	assert_eq "$?" "64" "split $opt exits 64"
done

out="$(run_launcher split --quiet --account work "$PROJECT_DIR")"
assert_contains "$out" "STUB_ACCOUNT:work" "eitri split --account work exports VERDANDI_CLAUDE_ACCOUNT=work"
assert_contains "$out" "STUB_ARG:split" "and still runs split"

out="$(run_launcher split --quiet "$SPACED")"
assert_contains "$out" "STUB_ARG:$(realpath -e -- "$SPACED")" "a project path with a space survives in split"

out="$(env -i HOME="$FAKE_HOME" PATH="$PATH" "$ROOT/bin/eitri" split 2>&1 </dev/null; true)"
assert_contains "$out" "project" "split without --quiet prints the startup banner too"

echo "== eitri page: the page window's command line =="
reset_state
EXTRA_ENV=()
out="$(run_launcher page notes --nvim "/r/编辑 器.sock" "$SPACED")"
args="$(grep '^STUB_ARG:' <<<"$out" | tr '\n' ' ')"
assert_eq "$args" "STUB_ARG:page STUB_ARG:notes STUB_ARG:--nvim STUB_ARG:/r/编辑 器.sock STUB_ARG:-- STUB_ARG:$(realpath -e -- "$SPACED") " "page passes the id, address and canonical directory separately"
assert_not_contains "$out" "account" "a page has no agent account banner"
assert_not_contains "$out" "no sidecar" "a page needs no agent sidecar"
out="$(run_launcher page --list)"
args="$(grep '^STUB_ARG:' <<<"$out" | tr '\n' ' ')"
assert_eq "$args" "STUB_ARG:page STUB_ARG:--list " "listing pages reaches shell without a project"
assert_not_contains "$out" "project" "listing pages prints no project banner"
for opt in --clean --legacy --new-window --quiet --account; do
    out="$(run_launcher page notes "$opt")"
    rc=$?
    assert_eq "$rc" "64" "page $opt exits 64"
    assert_contains "$out" "eitri page: unknown option $opt" "page refuses $opt by name"
    assert_not_contains "$out" "STUB_ARG" "page $opt never reaches shell"
done
out="$(run_launcher page notes --nvim)"
assert_eq "$?" "64" "page --nvim without an address exits 64"
assert_not_contains "$out" "STUB_ARG" "a missing address never reaches shell"
out="$(run_launcher page notes --nvim /r/n.sock -- "$PROJECT_DIR")"
args="$(grep '^STUB_ARG:' <<<"$out" | tr '\n' ' ')"
assert_eq "$args" "STUB_ARG:page STUB_ARG:notes STUB_ARG:--nvim STUB_ARG:/r/n.sock STUB_ARG:-- STUB_ARG:$REAL_PROJECT " "a page directory after -- is resolved"
for invalid in "" "Notes" "../notes" "notes-"; do
    run_launcher page "$invalid" >/dev/null
    assert_eq "$?" "64" "page refuses an invalid or missing id"
done
run_launcher page notes "$PROJECT_DIR" "$PROJECT_DIR" >/dev/null
assert_eq "$?" "64" "page refuses a second directory"
run_launcher page --list notes >/dev/null
assert_eq "$?" "64" "page listing refuses a positional id"

echo "== -h lists the panel and split subcommands =="
out="$(run_launcher -h)"
assert_contains "$out" "eitri agents" "the help text names eitri agents"
assert_contains "$out" "eitri remote" "the help text names eitri remote"
assert_contains "$out" "eitri panel" "the help text names eitri panel"
assert_contains "$out" "eitri split" "the help text names eitri split"
assert_contains "$out" "eitri setup [args]" "and still ends with the setup line"

echo "== eitri FILE: positionals pass through unresolved, in order, with the user's -- kept =="
reset_state
EXTRA_ENV=()
args_of() { grep '^STUB_ARG:' <<<"$1" | sed 's/^STUB_ARG://' | paste -sd'|' -; }
FILEDIR="$SCRATCH_ROOT/filedir"
mkdir -p "$FILEDIR"
run_in_filedir() { (cd "$FILEDIR" && env -i HOME="$FAKE_HOME" PATH="$PATH" "$ROOT/bin/eitri" "$@" 2>&1); }
out="$(run_in_filedir --quiet a.rs)"
assert_eq "$(args_of "$out")" "a.rs" "eitri a.rs hands a.rs to shell as typed"
out="$(run_in_filedir --quiet +42 a.rs)"
assert_eq "$(args_of "$out")" "+42|a.rs" "a +N before the file stays a line spec, ahead of it"
out="$(run_in_filedir --quiet a.rs b.rs)"
assert_eq "$(args_of "$out")" "a.rs|b.rs" "several files keep their order"
out="$(run_in_filedir --quiet -- -weird.txt)"
assert_eq "$(args_of "$out")" "--|-weird.txt" "a file named like an option follows the user's --"
out="$(run_in_filedir --quiet -- +42)"
assert_eq "$(args_of "$out")" "--|+42" "a +N after -- is a path and keeps the --"
out="$(run_in_filedir --quiet a.rs -- -b)"
assert_eq "$(args_of "$out")" "a.rs|--|-b" "files before and after the -- keep their sides"
out="$(run_in_filedir --quiet missing.rs)"
assert_eq "$(args_of "$out")" "missing.rs" "a file that does not exist is not refused here"
out="$(run_in_filedir --quiet)"
assert_eq "$(args_of "$out")" "$(realpath -e -- "$FILEDIR")" "no positional passes the resolved current directory, as before"
out="$(run_in_filedir --quiet --)"
assert_eq "$(args_of "$out")" "$(realpath -e -- "$FILEDIR")" "a bare -- is still the current directory"
out="$(run_in_filedir --quiet --new-window a.rs)"
assert_eq "$(args_of "$out")" "--new-window|a.rs" "--new-window passes through in window mode"
out="$(run_in_filedir --quiet --account x a.rs)"
assert_eq "$(args_of "$out")" "--new-window|a.rs" "--account as a flag asks for a new window"
assert_contains "$out" "STUB_ARG:--new-window" "and the stub saw it"
mkdir -p "$FILEDIR/choose-project"
for sub in panel split page run-guard linked-window choose-project agents remote; do
	out="$(run_in_filedir --quiet "$sub")"
	assert_eq "$(args_of "$out")" "./$sub" "a path named $sub is not read as the $sub subcommand"
	out="$(run_in_filedir --quiet "$sub" b.rs)"
	assert_eq "$(args_of "$out")" "./$sub|b.rs" "a path named $sub keeps its place among several"
	out="$(run_in_filedir --quiet --clean "$sub")"
	assert_eq "$(args_of "$out")" "--clean|$sub" "behind an option, $sub is already no subcommand and stays as typed"
	out="$(run_in_filedir --quiet -- "$sub")"
	assert_eq "$(args_of "$out")" "--|$sub" "after the user's --, $sub stays as typed"
	out="$(run_in_filedir --quiet "./$sub")"
	assert_eq "$(args_of "$out")" "./$sub" "an explicit ./$sub is untouched"
done
echo "== eitri agents: handed to shell as typed, with no banner and no project =="
# The subcommand has its own arguments (--json, --watch, --schema) and writes only its result: a banner
# on stderr or an unknown-option refusal would break `eitri agents --watch | status-bar-tool`.
reset_state
EXTRA_ENV=()
out="$(run_launcher agents)"
assert_eq "$?" "0" "eitri agents runs"
assert_eq "$(args_of "$out")" "agents" "plain eitri agents execs shell agents with nothing else"
assert_not_contains "$out" "eitri    " "eitri agents prints no startup banner"
assert_not_contains "$out" "no sidecar built yet" "eitri agents prints no sidecar hint"
out="$(run_launcher agents --json)"
assert_eq "$(args_of "$out")" "agents|--json" "--json reaches shell agents"
out="$(run_launcher agents --watch --schema 1)"
assert_eq "$(args_of "$out")" "agents|--watch|--schema|1" "--watch and --schema reach shell agents in order"
out="$(run_launcher agents --bogus)"
assert_eq "$(args_of "$out")" "agents|--bogus" "an option the subcommand refuses is shell's to refuse, not the launcher's"
echo "== eitri remote: handed to shell as typed, with no banner and no project =="
# Its commands take options of their own (`serve --listen`, `ssh-key add --name`) and `--help`, which are the
# binary's to read: the option loop would refuse them, or print the launcher's help instead.
reset_state
EXTRA_ENV=()
out="$(run_launcher remote status)"
assert_eq "$?" "0" "eitri remote status runs"
assert_eq "$(args_of "$out")" "remote|status" "eitri remote status execs shell remote status"
assert_not_contains "$out" "eitri    " "eitri remote prints no startup banner"
assert_not_contains "$out" "no sidecar built yet" "eitri remote prints no sidecar hint"
out="$(run_launcher remote serve --listen socket --origin desk.tail1.ts.net)"
assert_eq "$(args_of "$out")" "remote|serve|--listen|socket|--origin|desk.tail1.ts.net" "serve's options reach shell remote in order"
out="$(run_launcher remote --help)"
assert_eq "$(args_of "$out")" "remote|--help" "eitri remote --help is the binary's help, not the launcher's"
out="$(run_launcher remote add-device --help)"
assert_eq "$(args_of "$out")" "remote|add-device|--help" "a command's --help reaches it"
out="$(run_launcher remote)"
assert_eq "$(args_of "$out")" "remote" "plain eitri remote reaches the binary, which prints its usage"
out="$(run_in_filedir --quiet a.rs --clean)"
assert_eq "$(args_of "$out")" "--clean|a.rs" "an option after a file is still honoured"
out="$(run_in_filedir --quiet a.rs --bogus)"
assert_contains "$out" "eitri: unknown option --bogus" "an unknown option after a file is refused"
assert_not_contains "$out" "STUB_ARG" "and shell is not started"
out="$(run_in_filedir panel --quiet --new-window)"
assert_contains "$out" "eitri panel: unknown option --new-window" "panel refuses --new-window"
(cd "$FILEDIR" && env -i HOME="$FAKE_HOME" PATH="$PATH" "$ROOT/bin/eitri" panel --new-window >/dev/null 2>&1)
assert_eq "$?" "64" "panel --new-window exits 64"
(cd "$FILEDIR" && env -i HOME="$FAKE_HOME" PATH="$PATH" "$ROOT/bin/eitri" split --new-window >/dev/null 2>&1)
assert_eq "$?" "64" "split --new-window exits 64"
out="$(run_in_filedir panel --quiet missing)"
assert_contains "$out" "eitri: no such directory" "panel still resolves its directory and says so"
out="$(run_in_filedir +42 a.rs)"
assert_contains "$out" "eitri    open a.rs" "the banner names the file, not the line spec"
out="$(run_in_filedir -- -weird.txt)"
assert_contains "$out" "eitri    open -weird.txt" "the banner names a path given after --"
out="$(run_in_filedir --quiet a.rs)"
assert_not_contains "$out" "eitri    open" "--quiet drops the open line"
out="$(run_in_filedir)"
assert_contains "$out" "eitri    project $(realpath -e -- "$FILEDIR")" "with no positional the banner still names the project"

echo "== a launch that may end in a running window says nothing about an account =="
mkdir -p "$FILEDIR/d1" "$FILEDIR/d2"
out="$(run_in_filedir a.rs)"
assert_contains "$out" "eitri    open a.rs" "a file keeps the open line"
assert_not_contains "$out" "account" "a file names no account: a running window may take it"
out="$(cd "$FILEDIR" && env -i HOME="$FAKE_HOME" PATH="$PATH" VERDANDI_CLAUDE_ACCOUNT=work "$ROOT/bin/eitri" a.rs 2>&1)"
assert_not_contains "$out" "account" "nor does an account from the environment read as picked"
assert_contains "$out" "STUB_ACCOUNT:work" "which still reaches shell for a window of its own"
out="$(run_in_filedir d1 a.rs)"
assert_not_contains "$out" "account" "a folder with a file may be handed over as well"
out="$(run_in_filedir d1)"
assert_contains "$out" "account" "a folder alone always starts a window, so its account is named"
out="$(run_in_filedir --new-window a.rs)"
assert_contains "$out" "account" "--new-window starts a window, so its account is named"
out="$(run_in_filedir --clean a.rs)"
assert_contains "$out" "account" "--clean starts a window, so its account is named"
out="$(run_in_filedir --account work a.rs)"
assert_contains "$out" "account work (--account)" "--account names the account it picked"
out="$(run_in_filedir +42)"
assert_not_contains "$out" "eitri    open" "a line spec with no file names nothing to open"
assert_not_contains "$out" "account" "and no account"

echo "== the desktop entry without paths opens the project chooser =="
out="$(run_in_filedir --from-desktop)"
assert_eq "$?" "0" "a desktop launch without paths succeeds"
assert_eq "$(args_of "$out")" "choose-project" "a desktop launch without paths runs choose-project"
out="$(run_in_filedir --from-desktop --account work)"
assert_eq "$(args_of "$out")" "choose-project" "the chooser receives no --new-window from --account"
assert_contains "$out" "STUB_ACCOUNT:work" "the chooser inherits the selected account"
out="$(run_in_filedir --from-desktop --clean)"
assert_eq "$(args_of "$out")" "choose-project|--clean" "the chooser receives --clean"
DELETED_CWD="$SCRATCH_ROOT/deleted-cwd"
mkdir -p "$DELETED_CWD"
out="$(cd "$DELETED_CWD" && rmdir "$DELETED_CWD" &&
	env -i HOME="$FAKE_HOME" PATH="$PATH" "$ROOT/bin/eitri" --from-desktop 2>&1)"
assert_eq "$?" "0" "a desktop launch from a deleted current directory succeeds"
assert_eq "$(args_of "$out")" "choose-project" "a deleted current directory still reaches the chooser"
out="$(run_in_filedir)"
assert_eq "$(args_of "$out")" "$(realpath -e -- "$FILEDIR")" "a launch without flags or paths still opens the current directory"

echo "== the desktop entry's list: a folder after the first path gets a window of its own =="
launches_of() { grep -c '^STUB_ACCOUNT:' <<<"$1"; }
sorted_args_of() { grep '^STUB_ARG:' <<<"$1" | sed 's/^STUB_ARG://' | LC_ALL=C sort | paste -sd'|' -; }
D1="$FILEDIR/d1"
D2="$FILEDIR/d2"
out="$(run_in_filedir --quiet --from-desktop "$D1" "$D2")"
assert_eq "$(launches_of "$out")" "2" "two folders are two launches"
assert_eq "$(sorted_args_of "$out")" "--|--|$D1|$D2" "each with its own folder"
out="$(run_in_filedir --quiet --from-desktop "$FILEDIR/a.rs" "$D1" "$FILEDIR/b.rs")"
assert_eq "$(launches_of "$out")" "2" "the files of a mixed list share one launch"
assert_eq "$(sorted_args_of "$out")" "--|--|$FILEDIR/a.rs|$FILEDIR/b.rs|$D1" "beside the folder's"
out="$(run_in_filedir --quiet --from-desktop --new-window "$D1" "$D2")"
assert_eq "$(sorted_args_of "$out")" "--|--|--new-window|--new-window|$D1|$D2" "a launch's own flags go to each"
out="$(run_in_filedir --quiet --from-desktop "$D1" "$FILEDIR/a.rs")"
assert_eq "$(args_of "$out")" "$D1|$FILEDIR/a.rs" "a folder first with files after it is one launch, as eitri DIR FILE"
out="$(run_in_filedir --quiet --from-desktop "$FILEDIR/a.rs" "$FILEDIR/b.rs")"
assert_eq "$(args_of "$out")" "$FILEDIR/a.rs|$FILEDIR/b.rs" "files alone are one launch"
out="$(run_in_filedir --quiet --from-desktop "$D1")"
assert_eq "$(args_of "$out")" "$D1" "a folder alone is one launch"
out="$(run_in_filedir --quiet "$D1" "$D2")"
assert_eq "$(launches_of "$out")" "1" "typed, two folders stay one launch for shell to refuse"
out="$(run_in_filedir panel --quiet --from-desktop)"
assert_contains "$out" "eitri panel: unknown option --from-desktop" "panel refuses --from-desktop"

echo
echo "$CHECKS checks, $FAILURES failed"
[[ "$FAILURES" -eq 0 ]]
