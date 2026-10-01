#!/bin/sh
# Tests for the repository's root install.sh (the owner's own dev-loop wrapper over
# packaging/install.sh --from-source --checkout). Standalone: `sh packaging/tests/install/test_root_wrapper.sh`
# (or `bash` -- it is plain POSIX sh either way, matching the file under test).
#
# Not sourced by harness.sh: the root wrapper needs none of that fixture machinery (real releases,
# signers, a fixture server) -- it only ever decides which argv to hand to packaging/install.sh, so
# a recording stub standing in for packaging/install.sh is everything this needs. A real
# packaging/install.sh run from here would attempt a real --from-source build, which is exactly what
# this suite must never do.
#
# docs-codex-3: the root wrapper used to prepend `--from-source --checkout $here` unconditionally,
# so `sh install.sh --uninstall` (and --nvim-only, --sidecar-only) died
# "--from-source and --uninstall cannot be combined" in the real installer.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
PKG=$(cd "$HERE/../.." && pwd)
WRAPPER=$PKG/../install.sh

# A name no other run can share: a process id is not one (two runs in separate PID namespaces, or
# a later run after a pid wrapped, can have the same $$ and would delete each other's scratch).
mkdir -p "$HOME/.cache/nv-v1dist-t2"
SCRATCH_ROOT=$(mktemp -d "$HOME/.cache/nv-v1dist-t2/root-wrapper-test-XXXXXX")
trap 'rm -rf "$SCRATCH_ROOT"' EXIT

ROOT=$SCRATCH_ROOT/checkout
mkdir -p "$ROOT/packaging"
cp "$WRAPPER" "$ROOT/install.sh"
chmod +x "$ROOT/install.sh"

# The stub stands in for packaging/install.sh: it never builds anything, just records its own argv
# so this suite can assert on what the wrapper decided to hand it.
cat >"$ROOT/packaging/install.sh" <<'STUB'
#!/bin/sh
echo "STUB_CALL"
for a in "$@"; do
	printf 'STUB_ARG:%s\n' "$a"
done
STUB
chmod +x "$ROOT/packaging/install.sh"

FAILURES=0
CHECKS=0

# assert_contains OUTPUT NEEDLE DESCRIPTION
assert_contains() {
	CHECKS=$((CHECKS + 1))
	if printf '%s\n' "$1" | grep -Fq -- "$2"; then
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
	if printf '%s\n' "$1" | grep -Fq -- "$2"; then
		echo "FAIL - $3"
		echo "  expected NOT to find: $2"
		echo "  in: $1"
		FAILURES=$((FAILURES + 1))
	else
		echo "ok - $3"
	fi
}

run_wrapper() {
	env -i HOME="$HOME" PATH="$PATH" "$ROOT/install.sh" "$@" 2>&1
}

run_wrapper_with_verdandi_checkout() {
	_vc=$1
	shift
	env -i HOME="$HOME" PATH="$PATH" EITRI_VERDANDI_CHECKOUT="$_vc" "$ROOT/install.sh" "$@" 2>&1
}

echo "== plain './install.sh' still builds from source, unchanged =="
out=$(run_wrapper)
assert_contains "$out" "STUB_ARG:--from-source" "the default is still --from-source"
assert_contains "$out" "STUB_ARG:--checkout" "the default is still --checkout"
assert_contains "$out" "STUB_ARG:$ROOT" "--checkout names this checkout"

echo "== EITRI_VERDANDI_CHECKOUT still threads through --verdandi-checkout on the default path =="
out=$(run_wrapper_with_verdandi_checkout "$SCRATCH_ROOT/verdandi-src")
assert_contains "$out" "STUB_ARG:--verdandi-checkout" "--verdandi-checkout was added"
assert_contains "$out" "STUB_ARG:$SCRATCH_ROOT/verdandi-src" "it names the given directory"

echo "== './install.sh --uninstall' passes straight through, without --from-source --checkout =="
out=$(run_wrapper --uninstall)
assert_contains "$out" "STUB_ARG:--uninstall" "--uninstall reached the stub"
assert_not_contains "$out" "STUB_ARG:--from-source" "--from-source was not prepended"
assert_not_contains "$out" "STUB_ARG:--checkout" "--checkout was not prepended"

echo "== './install.sh --uninstall --purge' passes both mode flags through untouched =="
out=$(run_wrapper --uninstall --purge)
assert_contains "$out" "STUB_ARG:--uninstall" "--uninstall reached the stub"
assert_contains "$out" "STUB_ARG:--purge" "--purge reached the stub"
assert_not_contains "$out" "STUB_ARG:--from-source" "--from-source was not prepended"

echo "== './install.sh --nvim-only' passes straight through =="
out=$(run_wrapper --nvim-only)
assert_contains "$out" "STUB_ARG:--nvim-only" "--nvim-only reached the stub"
assert_not_contains "$out" "STUB_ARG:--from-source" "--from-source was not prepended"
assert_not_contains "$out" "STUB_ARG:--checkout" "--checkout was not prepended"

echo "== './install.sh --sidecar-only' passes straight through =="
out=$(run_wrapper --sidecar-only)
assert_contains "$out" "STUB_ARG:--sidecar-only" "--sidecar-only reached the stub"
assert_not_contains "$out" "STUB_ARG:--from-source" "--from-source was not prepended"

echo "== './install.sh --help' and '-h' pass straight through =="
out=$(run_wrapper --help)
assert_contains "$out" "STUB_ARG:--help" "--help reached the stub"
assert_not_contains "$out" "STUB_ARG:--from-source" "--from-source was not prepended for --help"
out=$(run_wrapper -h)
assert_contains "$out" "STUB_ARG:-h" "-h reached the stub"
assert_not_contains "$out" "STUB_ARG:--from-source" "--from-source was not prepended for -h"

# Fix round 2 (review-2): fix round 1 added --nvim-offer to packaging/install.sh (and to the usage
# text `./install.sh --help` prints) without adding it here, so `./install.sh --nvim-offer` still
# died "--from-source and --nvim-offer cannot be combined".
echo "== './install.sh --nvim-offer' passes straight through =="
out=$(run_wrapper --nvim-offer)
assert_contains "$out" "STUB_ARG:--nvim-offer" "--nvim-offer reached the stub"
assert_not_contains "$out" "STUB_ARG:--from-source" "--from-source was not prepended"
assert_not_contains "$out" "STUB_ARG:--checkout" "--checkout was not prepended"

echo "== './install.sh --from-source' still builds THIS checkout, not a fresh clone =="
out=$(run_wrapper --from-source)
assert_contains "$out" "STUB_ARG:--checkout" "--checkout is still added"
assert_contains "$out" "STUB_ARG:$ROOT" "--checkout names this checkout"

# The guard that keeps the list from drifting again: every mode packaging/install.sh's own parse_args
# hands to set_mode (which refuses a second one, so a prepended --from-source dies against it) must
# reach packaging/install.sh without --from-source --checkout, except --from-source itself, which is
# this script's own default mode and so combines with it (set_mode accepts the same mode twice).
# The modes are read out of the real packaging/install.sh, so one added there and not here fails.
echo "== every installer mode but --from-source passes straight through, read from install.sh itself =="
modes=$(grep -o 'set_mode [a-z][a-z-]*' "$PKG/install.sh" | sed 's/^set_mode /--/' | LC_ALL=C sort -u)
mode_count=$(printf '%s\n' "$modes" | grep -c '^--')
CHECKS=$((CHECKS + 1))
case $modes in
*--nvim-offer*--uninstall* | *--uninstall*--nvim-offer*)
	if [ "$mode_count" -ge 6 ]; then
		echo "ok - read $mode_count modes out of packaging/install.sh"
	else
		echo "FAIL - only $mode_count modes read out of packaging/install.sh: the guard below would be thin"
		FAILURES=$((FAILURES + 1))
	fi
	;;
*)
	echo "FAIL - could not read packaging/install.sh's modes (got: $modes): the guard below would be vacuous"
	FAILURES=$((FAILURES + 1))
	;;
esac
for mode in $modes; do
	if [ "$mode" = --from-source ]; then continue; fi
	out=$(run_wrapper "$mode")
	assert_contains "$out" "STUB_ARG:$mode" "'./install.sh $mode' reached the stub"
	assert_not_contains "$out" "STUB_ARG:--from-source" "'./install.sh $mode' prepended no --from-source"
	assert_not_contains "$out" "STUB_ARG:--checkout" "'./install.sh $mode' prepended no --checkout"
done

# Fix round 2 (review-2): `./install.sh --help` printed only packaging/install.sh's usage, whose
# "(no option)" line ("install or upgrade to the latest release") is not what this script does with
# no option. It now says so first, in plain words.
echo "== './install.sh --help' first says what this script does with no option =="
out=$(run_wrapper --help)
assert_contains "$out" "With no option, it builds and installs this checkout from source" \
	"the root script's own default is described"
assert_contains "$out" "STUB_ARG:--help" "packaging/install.sh's usage still follows"
assert_not_contains "$out" "spec " "no spec section reference in the help"
assert_not_contains "$out" "D11" "no ruling id in the help"
out=$(run_wrapper -h)
assert_contains "$out" "With no option, it builds and installs this checkout from source" \
	"-h describes the root script's own default too"

echo "== the root script's header names no private design document =="
CHECKS=$((CHECKS + 1))
if sed -n '1,/^set -eu/p' "$WRAPPER" | grep -E 'docs/superpowers|spec |D11' >/dev/null; then
	echo "FAIL - the root install.sh's header still points at a private design document or ruling id"
	FAILURES=$((FAILURES + 1))
else
	echo "ok - the root install.sh's header points at no private design document"
fi

echo "== a mode flag bypasses EITRI_VERDANDI_CHECKOUT too (it is a --from-source-only concern) =="
out=$(run_wrapper_with_verdandi_checkout "$SCRATCH_ROOT/verdandi-src" --uninstall)
assert_contains "$out" "STUB_ARG:--uninstall" "--uninstall reached the stub"
assert_not_contains "$out" "STUB_ARG:--verdandi-checkout" "--verdandi-checkout was not added for --uninstall"

echo
echo "$CHECKS checks, $FAILURES failed"
[ "$FAILURES" -eq 0 ]
