# Building the sidecar (plan 2026-09-27-v1-dist, Task 10, spec §5.3, §5.2, §6.2, §6.5 steps 3-4).
# Sourced by harness.sh. Every test here that wants a real (stubbed) build redirects Node's own
# download to the local fixture server with sc_inst_net/sc_inst_setup -- by default (plain
# inst/inst_net, used by every other test file) NEOVIBE_INSTALL_TEST_NODE_BASE_URL is unset, and
# node_dist_base's own test-mode default (http://127.0.0.1:1, nothing listening) makes the very first
# download refuse instantly, so every pre-existing test's sidecar-less installs are unaffected by
# this file.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

# sc_inst_net [--set K=V]...: a normal inst_net network install/upgrade, with Node's own download
# redirected to the fixture server's copy of the fake tarball setup_sidecar_fixtures built (served at
# the same dist/vX.Y.Z/ layout the real nodejs.org uses). Every caller here passes only extra --set
# overrides, never installer arguments -- inst()'s own "a --set must sit before install.sh's own
# arguments" rule (every pre-existing caller in this suite ends its own --set run with a bare --,
# for exactly this reason) is satisfied by always appending our own -- --base-url ... afterwards,
# which also supplies the normal network install's --base-url/--release-signers.
sc_inst_net() {
	inst --set "NEOVIBE_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" "$@" \
		-- --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
}

# sc_inst_setup [--set K=V]... -- INSTALLER_ARGS...: for --sidecar-only, which needs both Node's
# redirected download and --base-url (for the Verdandi-source asset it fetches itself -- it never
# takes inst_net's own defaults, since it is not a network release install). The caller's own -- and
# installer arguments (--sidecar-only, --release-file, ...) come through in "$@"; --base-url is
# appended after them, landing on the installer side regardless.
sc_inst_setup() {
	inst --set "NEOVIBE_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" "$@" \
		--base-url "http://127.0.0.1:$PORT"
}

# sc_setup_release VERSION REV: an installed-style lib/neovibe/{neovibe-setup,RELEASE} under this
# test's own directory, from the release fixture mk_release already built -- so --sidecar-only's
# "RELEASE beside dirname $0" rule (spec §6.2) has something real to find. Sets INSTALLER_UNDER_TEST,
# which inst() reads.
sc_setup_release() {
	_ssr_dir=$T/setup-$1/lib/neovibe
	mkdir -p "$_ssr_dir"
	cp "$INSTALLER" "$_ssr_dir/neovibe-setup"
	chmod 0755 "$_ssr_dir/neovibe-setup"
	cp "$S/fix/v$1/RELEASE" "$_ssr_dir/RELEASE"
	INSTALLER_UNDER_TEST=$_ssr_dir/neovibe-setup
}

# ---------------------------------------------------------------------------------------------
# Placement, and a rebuild replacing a binary by rename rather than in place (spec §5.3 step 6)

TESTS="$TESTS t_sidecar_placement_and_atomic_swap"
t_sidecar_placement_and_atomic_swap() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	d=$(data_of)/neovibe/sidecar/aaaaaaa
	bin=$d/verdandi-claude-sidecar
	expect_exec "$bin"
	expect_file "$d/BUILD"
	expect_file "$d/LICENSE.md"
	if ! grep -q 'Anthropic PBC' "$d/LICENSE.md"; then fail "the SDK's LICENSE.md was not copied beside the sidecar"; fi
	want=$(sed -n 's/^SIDECAR_VERSION_LINE=//p' "$d/BUILD")
	have=$("$bin" --version | sed -n 1p)
	expect_eq "$have" "$want" "the binary's own --version line matches BUILD"
	# A rebuild of the same rev replaces the file by rename (a new inode), never truncated in place
	# -- what makes it safe against a copy that a currently-running old binary would refuse with
	# ETXTBSY (spec §5.3 step 6: "never copy onto the final path").
	before_ino=$(stat -c %i "$bin")
	rm -f "$d/BUILD"
	sc_inst_net --set NEOVIBE_INSTALL_TEST_BUILD_STAMP=deadbeefcafebabe
	expect_rc 0
	after_ino=$(stat -c %i "$bin")
	if [ "$before_ino" = "$after_ino" ]; then fail "the sidecar binary kept its inode across a rebuild: not replaced by rename"; fi
}

# ---------------------------------------------------------------------------------------------
# Node and the Verdandi source: checked before use, and a mismatch refuses the whole install (spec
# §5.3 steps 2 and 4 -- once Node itself is down, every later failure is fatal, plan Task 10 review)

TESTS="$TESTS t_sidecar_node_checksum_refuses"
t_sidecar_node_checksum_refuses() {
	serve 1.0.0
	mkdir -p "$S/srv/dist-bad-node/$NODE_FIXTURE_VERSION"
	head -c 4096 /dev/urandom >"$S/srv/dist-bad-node/$NODE_FIXTURE_VERSION/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
	before=$(snap_but_staging "$TH")
	sc_inst_net --set "NEOVIBE_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist-bad-node"
	expect_fail "a wrong Node checksum"
	expect_out "checksum mismatch for node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
	expect_eq "$(snap_but_staging "$TH")" "$before" "nothing installed after a refused sidecar build"
}

TESTS="$TESTS t_sidecar_verdandi_source_checksum_refuses"
t_sidecar_verdandi_source_checksum_refuses() {
	serve 1.0.0
	printf 'corrupted\n' >>"$(served 1.0.0)/verdandi-aaaaaaa-source.tar.gz"
	before=$(snap_but_staging "$TH")
	sc_inst_net
	expect_fail "a corrupted Verdandi source"
	expect_out 'checksum mismatch for verdandi-aaaaaaa-source.tar.gz'
	expect_eq "$(snap_but_staging "$TH")" "$before" "nothing installed after a refused sidecar build"
}

# ---------------------------------------------------------------------------------------------
# The SDK notice (LIC-8)

TESTS="$TESTS t_sidecar_sdk_notice"
t_sidecar_sdk_notice() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	expect_out "downloading Anthropic's claude-agent-sdk 0.3.252 to this machine to build the sidecar"
	expect_out 'Anthropic PBC. All rights reserved.'
}

# ---------------------------------------------------------------------------------------------
# Artifact acceptance (spec §5.3 step 6): refused and not installed, and (being past Node's own
# download) fatal to the whole run.

TESTS="$TESTS t_sidecar_artifact_rejected_bad_protocol"
t_sidecar_artifact_rejected_bad_protocol() {
	serve 1.0.0
	before=$(snap_but_staging "$TH")
	sc_inst_net --set NEOVIBE_INSTALL_TEST_NPM_MODE=bad-protocol
	expect_fail "a bad protocol number"
	expect_out 'not protocol 3'
	expect_eq "$(snap_but_staging "$TH")" "$before" "nothing installed after a rejected artifact"
}

TESTS="$TESTS t_sidecar_artifact_rejected_bad_node"
t_sidecar_artifact_rejected_bad_node() {
	serve 1.0.0
	sc_inst_net --set NEOVIBE_INSTALL_TEST_NPM_MODE=bad-node
	expect_fail "a wrong node version"
	expect_out "not protocol 3 with node $NODE_FIXTURE_VERSION"
}

TESTS="$TESTS t_sidecar_artifact_rejected_sdk_bundled"
t_sidecar_artifact_rejected_sdk_bundled() {
	serve 1.0.0
	sc_inst_net --set NEOVIBE_INSTALL_TEST_NPM_MODE=sdk-bundled
	expect_fail "sdk_bundled served"
	expect_out 'not host_cli only'
}

# ---------------------------------------------------------------------------------------------
# Present or not (UPG-2): a binary without BUILD, or a mismatched BUILD, is rebuilt, never reported
# up to date -- exercised here with a real (stubbed) rebuild rather than a planted one.

TESTS="$TESTS t_sidecar_rebuilds_when_not_present"
t_sidecar_rebuilds_when_not_present() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	d=$(data_of)/neovibe/sidecar/aaaaaaa
	rm -f "$d/BUILD"
	sc_inst_net
	expect_rc 0
	expect_no_out 'is up to date'
	expect_file "$d/BUILD"
	printf 'VERDANDI_REV=aaaaaaa\nSIDECAR_VERSION_LINE=not the real one\n' >"$d/BUILD"
	sc_inst_net
	expect_rc 0
	expect_no_out 'is up to date'
}

# ---------------------------------------------------------------------------------------------
# The work directory (spec §5.3 steps 1, 3 and 7)

TESTS="$TESTS t_sidecar_workdir_layout_and_cleanup"
t_sidecar_workdir_layout_and_cleanup() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	expect_absent "$TH/.cache/neovibe/sidecar-build/aaaaaaa"
	serve 1.1.0 1.0.0
	# --keep-build is install.sh's own option, so it must sit after inst()'s -- boundary, not before
	# it like a --set (sc_inst_net's own convention assumes only extra --sets are passed through).
	inst --set "NEOVIBE_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --keep-build --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	expect_rc 0
	wd=$TH/.cache/neovibe/sidecar-build/bbbbbbb
	expect_dir "$wd"
	expect_dir "$wd/repo/apps/claude-sidecar/build/node-cache"
	expect_file "$wd/repo/apps/claude-sidecar/build/node-cache/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
	expect_exec "$wd/node/node-$NODE_FIXTURE_VERSION-linux-x64/bin/node"
}

# ---------------------------------------------------------------------------------------------
# set -e inside build_sidecar (SH-2): a stub npm that fails, reached through main's real call path
# during an upgrade, gives a non-zero exit, no swap, and an old install left byte-identical.

TESTS="$TESTS t_sidecar_npm_ci_failure_blocks_upgrade"
t_sidecar_npm_ci_failure_blocks_upgrade() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	before=$(snap "$TH/.local/lib/neovibe")
	serve 1.1.0 1.0.0
	sc_inst_net --set NEOVIBE_INSTALL_TEST_NPM_MODE=fail-ci
	expect_fail "a failing npm ci during an upgrade"
	expect_out 'npm-sidecar-stub: simulated npm ci failure'
	expect_eq "$(installed_version)" 1.0.0 "the old install after a refused sidecar build"
	expect_eq "$(snap "$TH/.local/lib/neovibe")" "$before" "the old install is byte-identical"
	expect_absent "$TH/.local/lib/neovibe.new"
}

TESTS="$TESTS t_sidecar_build_binary_failure_blocks_upgrade"
t_sidecar_build_binary_failure_blocks_upgrade() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	serve 1.1.0 1.0.0
	sc_inst_net --set NEOVIBE_INSTALL_TEST_NPM_MODE=fail-build
	expect_fail "a failing npm run build:binary during an upgrade"
	expect_out 'npm-sidecar-stub: simulated build failure'
	expect_eq "$(installed_version)" 1.0.0 "the old install after a refused sidecar build"
}

# ---------------------------------------------------------------------------------------------
# F1 (v1-dist whole-branch review): ensure_sidecar's own tolerance of a failed Node download (spec
# §5.2 step 4, "a release with no sidecar still installs") is only for a FIRST install. Before this
# fix it was unconditional, so an upgrade whose new release pins a different Verdandi rev swapped in
# the new install and exited 0 even though the new sidecar's Node download never happened -- the old
# rev's sidecar was left on disk under the old REV7, but nothing looks for it there any more
# (agent::EXPECTED_VERDANDI_REVISION names the new rev), so a working agent panel silently became "no
# sidecar" on a machine with no network to nodejs.org at upgrade time.

TESTS="$TESTS t_sidecar_upgrade_node_unreachable_refuses"
t_sidecar_upgrade_node_unreachable_refuses() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	before=$(snap "$TH/.local/lib/neovibe")
	serve 1.1.0 1.0.0
	# node_dist_base's own test-mode default is this same http://127.0.0.1:1 -- nothing listens on
	# port 1, so the download refuses instantly with no fixture server needed.
	inst_net --set "NEOVIBE_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:1/dist" --
	expect_fail "an upgrade whose new sidecar's Node download cannot be reached, with a working install already in place"
	expect_eq "$(installed_version)" 1.0.0 "the old install after a refused sidecar swap"
	expect_eq "$(snap "$TH/.local/lib/neovibe")" "$before" "the old install is byte-identical"
	sc=$(data_of)/neovibe/sidecar
	expect_dir "$sc/aaaaaaa"
	expect_absent "$sc/bbbbbbb"
}

TESTS="$TESTS t_sidecar_first_install_node_unreachable_still_succeeds"
t_sidecar_first_install_node_unreachable_still_succeeds() {
	# The other half of F1's fix: TOLERANT is still 1 on a first install (there is no working
	# install being replaced), so a machine with no network to nodejs.org still gets neovibe, with a
	# warning, exactly as spec §5.2 step 4 promises.
	serve 1.0.0
	inst_net --set "NEOVIBE_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:1/dist" --
	expect_rc 0
	expect_out 'neovibe is installed without one'
	expect_eq "$(installed_version)" 1.0.0
	expect_absent "$(data_of)/neovibe/sidecar/aaaaaaa"
}

# ---------------------------------------------------------------------------------------------
# Prune (UPG-1), exercised with a real build rather than a planted sidecar.

TESTS="$TESTS t_sidecar_prune_after_real_build"
t_sidecar_prune_after_real_build() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	sc=$(data_of)/neovibe/sidecar
	expect_dir "$sc/aaaaaaa"
	# A second, planted RELEASE (the test system RELEASE) survives.
	printf 'NEOVIBE_VERSION=0.9.0\nVERDANDI_REV=ddddddd000000000000000000000000000000000\n' >"$S/system/RELEASE"
	mkdir -p "$sc/ddddddd" "$sc/eeeeeee"
	serve 1.1.0 1.0.0
	sc_inst_net
	rm -f "$S/system/RELEASE"
	expect_rc 0
	expect_dir "$sc/aaaaaaa"
	expect_dir "$sc/ddddddd"
	expect_absent "$sc/eeeeeee"
	expect_dir "$sc/bbbbbbb"
	serve 1.2.0 1.1.0
	sc_inst_net
	expect_rc 0
	expect_absent "$sc/aaaaaaa"
	expect_dir "$sc/ccccccc"
}

TESTS="$TESTS t_prune_sidecars_symlinked_root_refused"
t_prune_sidecars_symlinked_root_refused() {
	# M3 (v1-dist whole-branch review, 2026-09-28): the same symlinked-sidecar-root gap as
	# do_uninstall's own (t_uninstall_symlinked_sidecar_root, test_uninstall.sh), reachable here in
	# a normal upgrade's own prune step. A symlinked $XDG_DATA_HOME/neovibe/sidecar, planted by the
	# user in their own data directory, was followed with no check at all: any 7-hex-named directory
	# behind the link that is not this upgrade's own new/old/system rev -- a user's own, unrelated
	# directory that merely happens to be named like a short git sha -- was removed.
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	sc=$(data_of)/neovibe/sidecar
	expect_dir "$sc/aaaaaaa"
	rm -rf "$sc"
	mkdir -p "$TH/backup/abcdef0"
	echo mine >"$TH/backup/abcdef0/keep"
	ln -s "$TH/backup" "$sc"
	serve 1.1.0 1.0.0
	# Node unreachable at the default (nothing serves it): tolerated, since neither old nor new
	# sidecar can be found present behind the now-symlinked root (F1's TOLERANT=1 case) -- so the
	# upgrade itself still succeeds with no sidecar build at all, keeping this test to prune_sidecars
	# alone.
	inst_net
	expect_rc 0
	expect_file "$TH/backup/abcdef0/keep"
	expect_out "$sc leads to $TH/backup, not to a directory named sidecar"
	if [ ! -L "$sc" ]; then fail "the link itself was removed"; fi
	expect_eq "$(installed_version)" 1.1.0 "the upgrade itself still succeeded"
}

TESTS="$TESTS t_sidecar_only_never_prunes"
t_sidecar_only_never_prunes() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	sc=$(data_of)/neovibe/sidecar
	mkdir -p "$sc/eeeeeee"
	rm -rf "$sc/aaaaaaa"
	sc_setup_release 1.0.0 "$REV_A"
	sc_inst_setup -- --sidecar-only
	expect_rc 0
	expect_dir "$sc/aaaaaaa"
	expect_dir "$sc/eeeeeee"
}

# ---------------------------------------------------------------------------------------------
# --sidecar-only (REL-1, spec §6.2, §9)

TESTS="$TESTS t_sidecar_only_dirname_release"
t_sidecar_only_dirname_release() {
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	# A different RELEASE planted in ~/.local/lib/neovibe/ and at the test system path is not read.
	mkdir -p "$TH/.local/lib/neovibe"
	printf 'NEOVIBE_VERSION=9.9.9\nVERDANDI_REV=fffffff000000000000000000000000000000000\n' >"$TH/.local/lib/neovibe/RELEASE"
	printf 'NEOVIBE_VERSION=8.8.8\nVERDANDI_REV=eeeeeee000000000000000000000000000000000\n' >"$S/system/RELEASE"
	sc_inst_setup -- --sidecar-only
	expect_rc 0
	rm -f "$S/system/RELEASE"
	expect_dir "$(data_of)/neovibe/sidecar/aaaaaaa"
	expect_absent "$(data_of)/neovibe/sidecar/fffffff"
	expect_absent "$(data_of)/neovibe/sidecar/eeeeeee"
}

TESTS="$TESTS t_sidecar_only_release_file"
t_sidecar_only_release_file() {
	serve 1.0.0
	sc_inst_setup -- --sidecar-only --release-file "$(served 1.0.0)/RELEASE"
	expect_rc 0
	expect_dir "$(data_of)/neovibe/sidecar/aaaaaaa"
}

TESTS="$TESTS t_sidecar_only_needs_a_release"
t_sidecar_only_needs_a_release() {
	inst -- --sidecar-only
	expect_fail "no RELEASE and no --release-file"
	expect_out 'no RELEASE found beside'
	expect_out '--release-file'
}

TESTS="$TESTS t_sidecar_only_no_sha256sums_fetched"
t_sidecar_only_no_sha256sums_fetched() {
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	srv_mark
	sc_inst_setup -- --sidecar-only
	expect_rc 0
	if srv_paths | grep -F SHA256SUMS >/dev/null; then fail "SHA256SUMS was fetched: $(srv_paths)"; fi
}

TESTS="$TESTS t_sidecar_only_present_reports_and_returns"
t_sidecar_only_present_reports_and_returns() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	sc_setup_release 1.0.0 "$REV_A"
	srv_mark
	sc_inst_setup -- --sidecar-only
	expect_rc 0
	expect_out 'sidecar for verdandi aaaaaaa: present'
	if srv_paths | grep -F verdandi- >/dev/null; then fail "a present sidecar was rebuilt: $(srv_paths)"; fi
}

# ---------------------------------------------------------------------------------------------
# --tarball builds a sidecar too (docs-claude-2)

TESTS="$TESTS t_tarball_install_builds_sidecar_from_beside_file"
t_tarball_install_builds_sidecar_from_beside_file() {
	# obtain_release's own --tarball branch never set NV_REL_URL, and --base-url is refused together
	# with --tarball (parse_args), so nothing else did either: a --tarball install whose sidecar was
	# not already present crashed with "NV_REL_URL: unbound variable" the moment ensure_sidecar tried
	# to build one. Node's download is still redirected to the fixture server, matching the report's
	# own repro (a machine that can reach nodejs.org); the Verdandi source is never fetched over the
	# network at all here -- mk_release plants verdandi-aaaaaaa-source.tar.gz beside every --tarball
	# fixture (spec §4.3's own asset layout), and do_install now uses it through
	# NV_VERDANDI_SOURCE_TARBALL_OVERRIDE.
	d=$S/fix/v1.0.0
	srv_mark
	inst --set "NEOVIBE_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --tarball "$d/neovibe-1.0.0-x86_64-linux.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" \
		--release-signers "$SIGNERS"
	expect_rc 0
	expect_out "installed neovibe 1.0.0 into"
	expect_out "using the Verdandi source beside"
	expect_exec "$(data_of)/neovibe/sidecar/aaaaaaa/verdandi-claude-sidecar"
	if srv_paths | grep -F verdandi- >/dev/null; then fail "the Verdandi source was fetched over the network: $(srv_paths)"; fi
}

TESTS="$TESTS t_tarball_verdandi_source_beside_file_copied_before_use"
t_tarball_verdandi_source_beside_file_copied_before_use() {
	# review-2 TOCTOU finding (+[codex] duplicate): use_verdandi_source_beside_tarball used to point
	# NV_VERDANDI_SOURCE_TARBALL_OVERRIDE straight at the file beside --tarball, which
	# sidecar_download_verdandi_source then hashed in place and sidecar_build_core reopened a SECOND
	# time (after Node's own extraction) to extract -- a file in --tarball's own, possibly shared,
	# directory swapped between those two reads ran unverified source and its npm scripts despite
	# every checksum passing. It is copied into this run's own 0700 $NV_DL first now, and only the
	# copy is ever read again. --tarball's own directory is this test's own private $d (never the
	# shared fixture under $S/fix), so the corruption below never leaks into another test; the
	# cp-swap stub corrupts the ORIGINAL beside-tarball file the instant AFTER this installer's own
	# (single) read of it, the same "swap right after the read" shape test_verify.sh's own
	# --tarball/--sums races use for sha256sum/cat.
	d=$T/tarball-race
	mkdir -p "$d"
	cp -- "$S/fix/v1.0.0/neovibe-1.0.0-x86_64-linux.tar.gz" "$S/fix/v1.0.0/SHA256SUMS" \
		"$S/fix/v1.0.0/SHA256SUMS.sig" "$S/fix/v1.0.0/verdandi-aaaaaaa-source.tar.gz" "$d/"
	printf 'evil: this must never reach npm ci or npm run build:binary\n' >"$T/evil-verdandi-source.tar.gz"
	printf '%s\n%s\n' "$d/verdandi-aaaaaaa-source.tar.gz" "$T/evil-verdandi-source.tar.gz" >"$S/logs/swap-verdandi"
	inst --stubs "$S/stubs-cpswap" --set "NEOVIBE_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --tarball "$d/neovibe-1.0.0-x86_64-linux.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" \
		--release-signers "$SIGNERS"
	expect_rc 0
	expect_out "installed neovibe 1.0.0 into"
	expect_out "using the Verdandi source beside"
	expect_exec "$(data_of)/neovibe/sidecar/aaaaaaa/verdandi-claude-sidecar"
	if [ ! -s "$S/logs/swap-verdandi.log" ]; then fail "the beside file was never swapped: the test did not run the race"; fi
	if [ -e "$d/verdandi-aaaaaaa-source.tar.gz" ] && ! grep -q evil "$d/verdandi-aaaaaaa-source.tar.gz" 2>/dev/null; then
		fail "the beside file was not actually corrupted after the read: the stub did not fire as intended"
	fi
}

TESTS="$TESTS t_tarball_install_falls_back_to_network_without_beside_file"
t_tarball_install_falls_back_to_network_without_beside_file() {
	# review-2, docs-claude-2's own untested fallback: every existing --tarball test above plants
	# verdandi-<rev>-source.tar.gz beside --tarball (mk_release's own fixture layout), so
	# obtain_release's --tarball branch's own NV_REL_URL assignment (the fix for the original
	# "NV_REL_URL: unbound variable" crash) has never actually been exercised by a real fetch. A
	# user who downloaded only the tarball, SHA256SUMS and .sig has no such file beside it. --tarball
	# refuses --base-url (parse_args), so NV_REL_URL here is genuinely NV_DEFAULT_BASE_URL
	# (github.com) -- the curl-mirror stub (stubs-net) strips scheme and host from any URL and
	# serves its path from $S/srv, so staging the genuine asset at that literal path lets this run
	# with no real network access, exactly as the report's own suggested fix says.
	mkdir -p "$S/srv/HunterGrey-cyber/neovibe/releases/download/v1.0.0"
	cp -- "$S/fix/v1.0.0/verdandi-aaaaaaa-source.tar.gz" \
		"$S/srv/HunterGrey-cyber/neovibe/releases/download/v1.0.0/verdandi-aaaaaaa-source.tar.gz"
	d=$T/tarball-only
	mkdir -p "$d"
	cp -- "$S/fix/v1.0.0/neovibe-1.0.0-x86_64-linux.tar.gz" "$S/fix/v1.0.0/SHA256SUMS" \
		"$S/fix/v1.0.0/SHA256SUMS.sig" "$d/"
	inst --stubs "$S/stubs-net" --set "NEOVIBE_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --tarball "$d/neovibe-1.0.0-x86_64-linux.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" \
		--release-signers "$SIGNERS"
	expect_rc 0
	expect_out "installed neovibe 1.0.0 into"
	expect_no_out "using the Verdandi source beside"
	expect_exec "$(data_of)/neovibe/sidecar/aaaaaaa/verdandi-claude-sidecar"
	if ! grep -qF 'HunterGrey-cyber/neovibe/releases/download/v1.0.0/verdandi-aaaaaaa-source.tar.gz' "$S/logs/curl.log"; then
		fail "the Verdandi source was not fetched through the network fallback: $(cat "$S/logs/curl.log")"
	fi
}

# ---------------------------------------------------------------------------------------------
# --build-sidecar-into (the AUR entry point, spec §9)

TESTS="$TESTS t_build_sidecar_into"
t_build_sidecar_into() {
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	dest=$T/aur-sidecar
	inst -- --build-sidecar-into "$dest" --node "$NODE_FIXTURE_TARBALL" \
		--verdandi-source "$(served 1.0.0)/verdandi-aaaaaaa-source.tar.gz"
	expect_rc 0
	expect_exec "$dest/verdandi-claude-sidecar"
	expect_file "$dest/verdandi-claude-sidecar.rev"
	expect_eq "$(cat "$dest/verdandi-claude-sidecar.rev")" "$REV_A" "the .rev file names the full rev"
	expect_file "$dest/LICENSE.md"
	expect_file "$dest/NODE-LICENSE"
	if ! grep -q 'Test double Node LICENSE' "$dest/NODE-LICENSE"; then fail "Node's own LICENSE was not copied"; fi
}

TESTS="$TESTS t_build_sidecar_into_node_cache_versioned_name"
t_build_sidecar_into_node_cache_versioned_name() {
	# M1 fix-round-1 regression (v1-dist whole-branch review, fix round 2, 2026-09-28; [codex-t1-1]):
	# the --node copy do_build_sidecar_into makes used to keep the generic basename node.tar.xz.
	# sidecar_build_core's own node-cache priming (spec §5.3 step 3, "buildBinary.mjs reuses this
	# exact cache and re-verifies it against its own pinned checksum") preserves whatever basename
	# it is handed, but buildBinary.mjs itself looks in that cache only for the versioned name
	# sidecar_download_node's own network path already writes -- so the AUR build() path found
	# nothing under the name it actually checks and downloaded Node a second time from nodejs.org,
	# discarding the byte-verified copy sitting right next to it. --keep-build (this mode's own
	# workdir lives under NV_CACHE_NV regardless of --build-sidecar-into's own DIR argument, the
	# same layout install_sidecar_for_rev uses) leaves the workdir behind so the cache's own
	# filename can be asserted directly, the same way t_sidecar_workdir_layout_and_cleanup does for
	# the network path.
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	dest=$T/aur-sidecar-cache-name
	inst -- --keep-build --build-sidecar-into "$dest" --node "$NODE_FIXTURE_TARBALL" \
		--verdandi-source "$(served 1.0.0)/verdandi-aaaaaaa-source.tar.gz"
	expect_rc 0
	expect_exec "$dest/verdandi-claude-sidecar"
	wd=$TH/.cache/neovibe/sidecar-build/aaaaaaa
	expect_file "$wd/repo/apps/claude-sidecar/build/node-cache/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
}

TESTS="$TESTS t_build_sidecar_into_node_swapped_after_hash"
t_build_sidecar_into_node_swapped_after_hash() {
	# M1 (v1-dist whole-branch review, 2026-09-28): --node and --verdandi-source used to be hashed
	# where they lay, then extracted from that same path by sidecar_build_core -- a file swapped for
	# a different one right after the hash was extracted unverified. They are copied into the work
	# directory first now and hashed and used from the copy alone, the same discipline --tarball
	# already holds (t_offline_tarball_swapped_after_hash, test_verify.sh). The sha256sum-swap stub
	# swaps the file at --node's own path for evil content right after the first sha256sum call
	# (do_build_sidecar_into's own hash of the copy, since resolve_setup_release/parse_release hash
	# nothing) -- with the fix, that swap changes nothing, because nothing reads --node's own path a
	# second time. A private copy of the shared NODE_FIXTURE_TARBALL is used as --node's own
	# argument, so the swap (which really does move a file onto that path) never corrupts the one
	# fixture every other test in this file shares.
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	dest=$T/aur-sidecar-swap
	node_copy=$T/node-for-swap-test.tar.xz
	cp "$NODE_FIXTURE_TARBALL" "$node_copy"
	evil=$T/evil-node.tar.xz
	printf 'EVIL not a real node tarball\n' >"$evil"
	printf '%s\n%s\n' "$evil" "$node_copy" >"$S/logs/swap"
	inst --stubs "$S/stubs-swapsha" -- --build-sidecar-into "$dest" --node "$node_copy" \
		--verdandi-source "$(served 1.0.0)/verdandi-aaaaaaa-source.tar.gz"
	expect_rc 0
	if [ ! -s "$S/logs/swap.log" ]; then fail "--node was never swapped: the test did not run the race"; fi
	expect_exec "$dest/verdandi-claude-sidecar"
	# --node's own path itself now holds the evil content (the swap really happened): confirms
	# sidecar_build_core never read it a second time, since the build still succeeded on genuine
	# content copied out before the swap.
	if ! grep -qF EVIL "$node_copy"; then fail "the --node path was not actually swapped: the test did not run the race"; fi
}

TESTS="$TESTS t_build_sidecar_into_checksum_mismatch"
t_build_sidecar_into_checksum_mismatch() {
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	dest=$T/aur-sidecar-bad
	bad=$T/bad-node.tar.xz
	head -c 1024 /dev/urandom >"$bad"
	inst -- --build-sidecar-into "$dest" --node "$bad" \
		--verdandi-source "$(served 1.0.0)/verdandi-aaaaaaa-source.tar.gz"
	expect_fail "a --node file that does not match the release's pin"
	expect_out 'checksum mismatch for --node'
	expect_absent "$dest"
}

TESTS="$TESTS t_build_sidecar_into_needs_node_and_source"
t_build_sidecar_into_needs_node_and_source() {
	inst -- --build-sidecar-into "$T/x"
	expect_fail "--build-sidecar-into without --node/--verdandi-source"
	expect_out 'needs --node FILE and --verdandi-source FILE too'
	inst -- --node "$T/x"
	expect_fail "--node without --build-sidecar-into"
	expect_out '--node and --verdandi-source go with --build-sidecar-into'
}

TESTS="$TESTS t_build_sidecar_into_ignores_xdg_data_home_outside_home"
t_build_sidecar_into_ignores_xdg_data_home_outside_home() {
	# installer-codex review-2 (fix round 1): check_data_home (installer-claude-9) used to gate
	# every mode except --uninstall, including --build-sidecar-into -- which never reads or writes
	# anything under NV_DATA/XDG_DATA_HOME (its workdir is under NV_CACHE_NV, and its output goes
	# to the caller's own --build-sidecar-into DIR, never $XDG_DATA_HOME). The AUR package's
	# build() runs exactly this mode, in a sandbox that may set XDG_DATA_HOME outside $HOME with
	# no reason this mode would care. It used to refuse with a message naming the desktop entry,
	# licences, private nvim and sidecar -- none of which this mode touches.
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	dest=$T/aur-sidecar-outside-data
	inst --set "XDG_DATA_HOME=$T/outside-data" -- --build-sidecar-into "$dest" --node "$NODE_FIXTURE_TARBALL" \
		--verdandi-source "$(served 1.0.0)/verdandi-aaaaaaa-source.tar.gz"
	expect_rc 0
	expect_no_out 'XDG_DATA_HOME resolves to'
	expect_exec "$dest/verdandi-claude-sidecar"
	expect_absent "$T/outside-data"
}
