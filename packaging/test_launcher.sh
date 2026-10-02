#!/usr/bin/env bash
# Tests for packaging/eitri.launcher.sh (Task 8). Standalone: `bash packaging/test_launcher.sh`.
# Builds a fake install tree under ~/.cache/nv-v1dist-t8/launcher-test-XXXXXX/ (never /tmp), with a
# `bin/eitri` symlink to the REAL launcher script and a stub `lib/eitri/shell` that records its
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
# A real install's own bin/eitri is a plain copy (as built above), never a symlink -- but a user
# or a distro's alternatives system may point a SEPARATE entry point at it (e.g.
# `~/.local/bin/eitri -> /usr/bin/eitri`, or `~/bin/eitri -> ~/.local/bin/eitri`, F5's own
# second case). The launcher follows that FILE's own symlink chain, so LIBDIR must come out as the
# copy's OWN sibling `lib/eitri` -- the target's, not some unrelated directory next to wherever
# the symlink itself happens to sit.
reset_state
SYMLINK_ENTRY_DIR="$SCRATCH_ROOT/other-bin"
mkdir -p "$SYMLINK_ENTRY_DIR"
ln -s "$ROOT/bin/eitri" "$SYMLINK_ENTRY_DIR/eitri"
EXTRA_ENV=()
out="$(env -i HOME="$FAKE_HOME" PATH="$PATH" "$SYMLINK_ENTRY_DIR/eitri" --version 2>&1)"
assert_contains "$out" "STUB_SELF:$ROOT/lib/eitri/shell" "invoked through a symlink, LIBDIR still resolves to the real script's own lib/eitri"
assert_contains "$out" "STUB_VERSION:" "--version reached the stub through the symlink"

echo "== LIBDIR resolves through a RELATIVE symlink target (GNU stow's own shape) =="
# fix round 1 (whole-branch review, finding 3a): the test above only ever used an ABSOLUTE
# `ln -s "$ROOT/bin/eitri" ...` target. `resolve_launcher_path`'s relative branch
# (`path="$(dirname "$path")/$target"`) is what a real GNU stow tree actually produces --
# `~/bin/eitri -> ../.local/bin/eitri` -- and was never exercised until now.
reset_state
RELSYM_ENTRY_DIR="$SCRATCH_ROOT/relsym-bin"
mkdir -p "$RELSYM_ENTRY_DIR"
(cd "$RELSYM_ENTRY_DIR" && ln -s ../install/bin/eitri eitri)
EXTRA_ENV=()
out="$(env -i HOME="$FAKE_HOME" PATH="$PATH" "$RELSYM_ENTRY_DIR/eitri" --version 2>&1)"
assert_contains "$out" "STUB_SELF:$ROOT/lib/eitri/shell" "a relative symlink target still resolves LIBDIR to the real script's own lib/eitri"
assert_contains "$out" "STUB_VERSION:" "--version reached the stub through the relative symlink"

echo "== LIBDIR resolves when the launcher's own bindir is itself a symlinked directory =="
# F5's other case: ~/.local/bin (or here, bin/) is a SYMLINKED DIRECTORY -- e.g. GNU stow folding
# it into a dotfiles repo -- while the launcher FILE inside it is a plain copy, not a symlink, and
# lib/eitri is a real sibling of the unresolved (logical) bindir. The old `readlink -f "$0"`
# canonicalized straight through a symlinked directory component too, landing LIBDIR next to the
# symlink's TARGET instead of this install's own lib/eitri, so `cd` died "No such file or
# directory" on every launch. Only the FILE's own symlink chain may be followed; a directory
# symlink earlier in the path must be left alone, and `cd`'s logical resolution of the appended
# `../lib/eitri` then removes the bindir as named, landing on its own sibling.
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
# A guard, not a reproduction: this case has always passed. POSIX-mode `cd` resolves the `..` in
# `bin/../lib/eitri` logically (it removes `bin` as named, symlinked or not), exactly as bash
# does by default. What POSIX mode turns off is bash's extra physical retry when that logical path
# does not exist. Only the unsupported inverse shape (lib/eitri next to the symlink's target, none
# next to the bindir as named) would need that retry. This check pins that F5's own case never
# comes to depend on it.
out="$(env -i HOME="$FAKE_HOME" POSIXLY_CORRECT=1 PATH="$PATH" "$SYMDIR_ROOT/bin/eitri" --version 2>&1)"
assert_contains "$out" "STUB_SELF:$SYMDIR_ROOT/lib/eitri/shell" "a symlinked bindir resolves LIBDIR under POSIXLY_CORRECT=1 too"
assert_not_contains "$out" "No such file or directory" "no broken cd through the symlinked bindir under POSIXLY_CORRECT=1"

echo "== an exported CDPATH does not corrupt LIBDIR through a RELATIVE invocation (fix round 1, finding 1) =="
# Reproduced before this fix, on the launcher as first fixed: with $0 relative (an unpacked
# release directory's own bin/eitri run in place, or any PATH entry that is itself relative --
# both real shapes this launcher ships in, see its header comment),
# `cd "$(dirname "$EITRI_LAUNCHER_PATH")/../lib/eitri"` without `CDPATH=''` handed `cd` a
# relative directory, so an exported CDPATH containing a matching entry (here, `.`) made `cd` print
# its destination to stdout on top of `pwd`'s own line -- two lines glued into one LIBDIR, and the
# final `exec "$LIBDIR/shell"` died "No such file or directory". `CDPATH=''` on that one `cd`
# closes it.
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

echo "== LIBDIR resolves when the launcher's own directory is spelled '.' or ends in '/.' (fix round 2) =="
# Reproduced against the next version, which computed the parent of the bindir with a second `dirname`
# instead of appending `..`: `dirname .` is `.`, not `..`, so running an unpacked release's
# bin/eitri in place from inside bin/ (`./eitri`, $0's directory `.`) looked for `./lib/eitri`
# inside bin/, and `bin/./eitri` run from the release root looked for `bin/lib/eitri`. Both died
# "cd: ...: No such file or directory". Appending `..` to the directory as spelled is right for any
# spelling of it.
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
# A RELATIVE XDG_DATA_HOME must be treated as though it were unset (spec sec 6.4's three-case
# rule) -- so the sidecar has to be found under $HOME/.local/share, not under a directory literally
# named "relative/xdg" relative to wherever the launcher happens to run from. A naive
# `${XDG_DATA_HOME:-default}` would miss it here and wrongly print the hint.
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

# installer-claude-1 (+installer-codex-3, docs-codex-4): `eitri setup` used to force
# `--sidecar-only` onto every call, so `eitri setup --nvim-only`/`--uninstall`/`--sidecar-only`
# all died "--sidecar-only and X cannot be combined" in the real installer -- exactly the recovery
# command install.sh itself prints on a failed nvim download. These tests never reach
# packaging/install.sh's own logic (the stub above just records its argv), so they hold the
# launcher's own dispatch to account regardless of what install.sh does with it.
#
# fix round 1 (review-2): plain `eitri setup` used to run --sidecar-only and then --nvim-only for
# its second call -- do_nvim_only is the EXPLICIT ask (packaging/install.sh's own comment on it:
# "an already-adequate PATH nvim does not suppress it ... a download failure is fatal"), so a plain
# `eitri setup` with an adequate nvim already on PATH, or no network right now, turned "nothing to
# do" into a hard failure. The second call is now --nvim-offer, the SAME conditional offer a normal
# install runs (packaging/install.sh's own do_nvim_offer/maybe_offer_nvim); these argv-recording
# tests cannot see that installer-side difference (the stub does not distinguish the two modes'
# behaviour), so packaging/tests/install/test_nvim.sh holds do_nvim_offer itself to account.
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

# Fix round 2 (review-2): fix round 1 added --nvim-offer to packaging/install.sh (and to its usage
# text, which `eitri setup --help` prints) but not to the pass-through list above, so
# `eitri setup --nvim-offer --dry-run` died "--sidecar-only and --nvim-offer cannot be combined" --
# the very defect class this dispatch exists to close, recreated by a hand-kept subset.
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

echo "== the launcher's --help mentions no internal design references (leaks-claude-1, docs-claude-4) =="
EXTRA_ENV=()
out="$(run_launcher --help)"
assert_not_contains "$out" "D16" "no ruling id in the help text"
assert_not_contains "$out" "LEGACY_NOT_IN_BUILD" "no Rust constant name in the help text"
assert_not_contains "$out" "spec " "no \"spec sec/\$N\" reference in the help text"

echo "== -h prints plain-word help, no internal ruling id =="
# leaks-claude-1 (review2 Task 4): -h/--help prints this script's own header comment (lines 2-10),
# read by anyone who runs `eitri -h` -- it used to say "a release build exits 1 naming
# LEGACY_NOT_IN_BUILD, D16", an internal constant name and ruling id.
reset_state
EXTRA_ENV=()
out="$(run_launcher -h)"
assert_not_contains "$out" "D16" "the help text names no internal ruling id"
assert_not_contains "$out" "LEGACY_NOT_IN_BUILD" "the help text names no internal constant"
assert_contains "$out" "a release build refuses it" "the help text still says what a release build does, in plain words"
assert_contains "$out" "--clean" "the help text lists --clean"
assert_contains "$out" "eitri setup [args]" "the help text still ends with its last usage line"

echo "== the startup banner's project and account lines line up =="
# The 2026-09-30 rename shortened the name to "eitri"; the account lines are indented to the
# column "project" starts in, so the name's padding has to keep that column at 9.
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

echo
echo "$CHECKS checks, $FAILURES failed"
[[ "$FAILURES" -eq 0 ]]
