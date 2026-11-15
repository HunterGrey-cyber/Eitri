# Building the sidecar (Node and Verdandi source download, checksum and build, placement and pruning).
# Sourced by harness.sh. Every test here that wants a real (stubbed) build redirects Node's own
# download to the local fixture server with sc_inst_net/sc_inst_setup -- by default (plain
# inst/inst_net, used by every other test file) EITRI_INSTALL_TEST_NODE_BASE_URL is unset, and
# node_dist_base's test-mode default (http://127.0.0.1:1, nothing listening) refuses the first
# download instantly, keeping other test files' installs sidecar-less.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

# sc_inst_net [--set K=V]...: a normal inst_net network install/upgrade, with Node's own download
# redirected to the fixture server's copy of the fake tarball setup_sidecar_fixtures built (served at
# the same dist/vX.Y.Z/ layout the real nodejs.org uses). Every caller here passes only extra --set
# overrides, never installer arguments. Append -- --base-url ... so the overrides stay before
# inst()'s boundary and --base-url/--release-signers reach the installer.
sc_inst_net() {
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" "$@" \
		-- --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
}

# sc_inst_setup [--set K=V]... -- INSTALLER_ARGS...: for --sidecar-only, which needs both Node's
# redirected download and --base-url (for the Verdandi-source asset it fetches itself -- it never
# takes inst_net's own defaults, since it is not a network release install). The caller's own -- and
# installer arguments (--sidecar-only, --release-file, ...) come through in "$@"; --base-url is
# appended after them, landing on the installer side regardless.
sc_inst_setup() {
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" "$@" \
		--base-url "http://127.0.0.1:$PORT"
}

# sc_setup_release VERSION REV: an installed-style lib/eitri/{eitri-setup,RELEASE} under this
# test's own directory, from the release fixture mk_release already built -- so --sidecar-only's
# "RELEASE beside dirname $0" rule has something real to find. Sets INSTALLER_UNDER_TEST,
# which inst() reads.
sc_setup_release() {
	_ssr_dir=$T/setup-$1/lib/eitri
	mkdir -p "$_ssr_dir"
	cp "$INSTALLER" "$_ssr_dir/eitri-setup"
	chmod 0755 "$_ssr_dir/eitri-setup"
	cp "$S/fix/v$1/RELEASE" "$_ssr_dir/RELEASE"
	INSTALLER_UNDER_TEST=$_ssr_dir/eitri-setup
}

# ---------------------------------------------------------------------------------------------
# Placement, and a rebuild replacing a binary by rename rather than in place

TESTS="$TESTS t_sidecar_placement_and_atomic_swap"
t_sidecar_placement_and_atomic_swap() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	d=$(data_of)/eitri/sidecar/aaaaaaa
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
	# ETXTBSY (never copy onto the final path).
	before_ino=$(stat -c %i "$bin")
	rm -f "$d/BUILD"
	sc_inst_net --set EITRI_INSTALL_TEST_BUILD_STAMP=deadbeefcafebabe
	expect_rc 0
	after_ino=$(stat -c %i "$bin")
	if [ "$before_ino" = "$after_ino" ]; then fail "the sidecar binary kept its inode across a rebuild: not replaced by rename"; fi
}

# ---------------------------------------------------------------------------------------------
# Node and the Verdandi source: checked before use, and a mismatch refuses the whole install
# (once Node itself is down, every later failure is fatal)

TESTS="$TESTS t_sidecar_node_checksum_refuses"
t_sidecar_node_checksum_refuses() {
	serve 1.0.0
	mkdir -p "$S/srv/dist-bad-node/$NODE_FIXTURE_VERSION"
	head -c 4096 /dev/urandom >"$S/srv/dist-bad-node/$NODE_FIXTURE_VERSION/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
	before=$(snap_but_staging "$TH")
	sc_inst_net --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist-bad-node"
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
# The SDK notice.

TESTS="$TESTS t_sidecar_sdk_notice"
t_sidecar_sdk_notice() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	expect_out "downloading Anthropic's claude-agent-sdk 0.3.252 to this machine to build the sidecar"
	expect_out 'Anthropic PBC. All rights reserved.'
}

# ---------------------------------------------------------------------------------------------
# One line before the slow run, nothing for quick steps. With a cold npm cache, Node's download,
# npm ci (~120 MB), TypeScript and the single binary take about a minute together, network-bound;
# checksums, unpacking and copying take a second or two. Announce the whole run before downloading.

SLOW_LINE='downloading Node and the Claude Agent SDK, then building the sidecar (1-3 min)'

TESTS="$TESTS t_sidecar_one_line_before_the_slow_steps"
t_sidecar_one_line_before_the_slow_steps() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	expect_count "$SLOW_LINE" 1
	# Exactly one status line of this kind in the whole run, and nothing else carrying a duration.
	expect_count '(1-3 min)' 1
	# Before the run it announces: the SDK notice is printed after npm ci, the "built" line after the build.
	_ln_slow=$(grep -n -F -e "$SLOW_LINE" "$OUT" | head -n 1 | cut -d: -f1)
	_ln_notice=$(grep -n -F -e "downloading Anthropic's claude-agent-sdk" "$OUT" | head -n 1 | cut -d: -f1)
	_ln_built=$(grep -n -F -e 'built the sidecar for verdandi' "$OUT" | head -n 1 | cut -d: -f1)
	if [ -z "$_ln_slow" ] || [ -z "$_ln_notice" ] || [ -z "$_ln_built" ]; then
		fail "a line is missing (slow $_ln_slow, notice $_ln_notice, built $_ln_built); output:$NL$(sed 's/^/      | /' "$OUT")"
	elif [ "$_ln_slow" -ge "$_ln_notice" ] || [ "$_ln_notice" -ge "$_ln_built" ]; then
		fail "the lines are out of order (slow $_ln_slow, notice $_ln_notice, built $_ln_built)"
	fi
}

TESTS="$TESTS t_sidecar_no_slow_line_when_nothing_is_built"
t_sidecar_no_slow_line_when_nothing_is_built() {
	serve 1.0.0
	# A dry run builds nothing, so announces nothing.
	inst_net --dry-run
	expect_rc 0
	expect_out 'would build the sidecar'
	expect_no_out '(1-3 min)'
	sc_inst_net
	expect_rc 0
	expect_count "$SLOW_LINE" 1
	# A sidecar already present: nothing slow runs, so nothing is announced (--sidecar-only).
	sc_setup_release 1.0.0 "$REV_A"
	sc_inst_setup -- --sidecar-only
	expect_rc 0
	expect_out 'sidecar for verdandi aaaaaaa: present'
	expect_no_out '(1-3 min)'
	# The same install run again, now up to date: the sidecar is present, so nothing is announced.
	sc_inst_net
	expect_rc 0
	expect_no_out '(1-3 min)'
}

# ---------------------------------------------------------------------------------------------
# Artifact acceptance: refused and not installed, and (being past Node's own
# download) fatal to the whole run.

TESTS="$TESTS t_sidecar_artifact_rejected_bad_protocol"
t_sidecar_artifact_rejected_bad_protocol() {
	serve 1.0.0
	before=$(snap_but_staging "$TH")
	sc_inst_net --set EITRI_INSTALL_TEST_NPM_MODE=bad-protocol
	expect_fail "a bad protocol number"
	expect_out 'not protocol 3'
	expect_eq "$(snap_but_staging "$TH")" "$before" "nothing installed after a rejected artifact"
}

TESTS="$TESTS t_sidecar_artifact_rejected_bad_node"
t_sidecar_artifact_rejected_bad_node() {
	serve 1.0.0
	sc_inst_net --set EITRI_INSTALL_TEST_NPM_MODE=bad-node
	expect_fail "a wrong node version"
	expect_out "not protocol 3 with node $NODE_FIXTURE_VERSION"
}

TESTS="$TESTS t_sidecar_artifact_rejected_sdk_bundled"
t_sidecar_artifact_rejected_sdk_bundled() {
	serve 1.0.0
	sc_inst_net --set EITRI_INSTALL_TEST_NPM_MODE=sdk-bundled
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
	d=$(data_of)/eitri/sidecar/aaaaaaa
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
# The work directory

TESTS="$TESTS t_sidecar_workdir_layout_and_cleanup"
t_sidecar_workdir_layout_and_cleanup() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	expect_absent "$TH/.cache/eitri/sidecar-build/aaaaaaa"
	serve 1.1.0 1.0.0
	# --keep-build is install.sh's own option, so it must sit after inst()'s -- boundary, not before
	# it like a --set (sc_inst_net's own convention assumes only extra --sets are passed through).
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --keep-build --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	expect_rc 0
	wd=$TH/.cache/eitri/sidecar-build/bbbbbbb
	expect_dir "$wd"
	expect_dir "$wd/repo/apps/claude-sidecar/build/node-cache"
	expect_file "$wd/repo/apps/claude-sidecar/build/node-cache/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
	expect_exec "$wd/node/node-$NODE_FIXTURE_VERSION-linux-x64/bin/node"
}

# ---------------------------------------------------------------------------------------------
# set -e inside build_sidecar: a stub npm that fails, reached through main's real call path
# during an upgrade, gives a non-zero exit, no swap, and an old install left byte-identical.

TESTS="$TESTS t_sidecar_npm_ci_failure_blocks_upgrade"
t_sidecar_npm_ci_failure_blocks_upgrade() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	before=$(snap "$TH/.local/lib/eitri")
	serve 1.1.0 1.0.0
	sc_inst_net --set EITRI_INSTALL_TEST_NPM_MODE=fail-ci
	expect_fail "a failing npm ci during an upgrade"
	expect_out 'npm-sidecar-stub: simulated npm ci failure'
	expect_eq "$(installed_version)" 1.0.0 "the old install after a refused sidecar build"
	expect_eq "$(snap "$TH/.local/lib/eitri")" "$before" "the old install is byte-identical"
	expect_absent "$TH/.local/lib/eitri.new"
}

TESTS="$TESTS t_sidecar_build_binary_failure_blocks_upgrade"
t_sidecar_build_binary_failure_blocks_upgrade() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	serve 1.1.0 1.0.0
	sc_inst_net --set EITRI_INSTALL_TEST_NPM_MODE=fail-build
	expect_fail "a failing npm run build:binary during an upgrade"
	expect_out 'npm-sidecar-stub: simulated build failure'
	expect_eq "$(installed_version)" 1.0.0 "the old install after a refused sidecar build"
}

# ---------------------------------------------------------------------------------------------
# ensure_sidecar's own tolerance of a failed Node download
# ("a release with no sidecar still installs") holds only where no working install is replaced: a
# first install, or an upgrade from one whose own sidecar is absent. An upgrade over a working
# install whose new release pins a different Verdandi rev refuses instead: the old rev's sidecar
# would stay on disk under the old REV7, but nothing looks for it there
# (agent::BUNDLED_SIDECAR_REVISION names the new rev), so a working agent panel would silently
# become "no sidecar" on a machine with no network to nodejs.org at upgrade time.

TESTS="$TESTS t_sidecar_upgrade_node_unreachable_refuses"
t_sidecar_upgrade_node_unreachable_refuses() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	before=$(snap "$TH/.local/lib/eitri")
	serve 1.1.0 1.0.0
	# node_dist_base's own test-mode default is this same http://127.0.0.1:1 -- nothing listens on
	# port 1, so the download refuses instantly with no fixture server needed.
	inst_net --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:1/dist" --
	expect_fail "an upgrade whose new sidecar's Node download cannot be reached, with a working install already in place"
	expect_eq "$(installed_version)" 1.0.0 "the old install after a refused sidecar swap"
	expect_eq "$(snap "$TH/.local/lib/eitri")" "$before" "the old install is byte-identical"
	sc=$(data_of)/eitri/sidecar
	expect_dir "$sc/aaaaaaa"
	expect_absent "$sc/bbbbbbb"
}

TESTS="$TESTS t_sidecar_first_install_node_unreachable_still_succeeds"
t_sidecar_first_install_node_unreachable_still_succeeds() {
	# A first install has no working sidecar to replace, so TOLERANT stays 1 and an unreachable
	# nodejs.org still permits installing Eitri with a warning.
	serve 1.0.0
	inst_net --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:1/dist" --
	expect_rc 0
	expect_out 'Eitri is installed without one'
	# The line is said before the download, so it is there even when the download then fails.
	expect_count "$SLOW_LINE" 1
	expect_eq "$(installed_version)" 1.0.0
	expect_absent "$(data_of)/eitri/sidecar/aaaaaaa"
}

# ---------------------------------------------------------------------------------------------
# Prune (UPG-1), exercised with a real build rather than a planted sidecar.

TESTS="$TESTS t_sidecar_prune_after_real_build"
t_sidecar_prune_after_real_build() {
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	sc=$(data_of)/eitri/sidecar
	expect_dir "$sc/aaaaaaa"
	# A second, planted RELEASE (the test system RELEASE) survives.
	printf 'EITRI_VERSION=0.9.0\nVERDANDI_REV=ddddddd000000000000000000000000000000000\n' >"$S/system/RELEASE"
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
	# Pruning must not follow a sidecar-root symlink to an unrelated directory. A 7-hex-named child
	# there may be a user's unrelated backup, not an Eitri sidecar, even if it resembles a short git sha.
	# Exercise this on upgrade as test_uninstall.sh's t_uninstall_symlinked_sidecar_root does on removal.
	serve 1.0.0
	sc_inst_net
	expect_rc 0
	sc=$(data_of)/eitri/sidecar
	expect_dir "$sc/aaaaaaa"
	rm -rf "$sc"
	mkdir -p "$TH/backup/abcdef0"
	echo mine >"$TH/backup/abcdef0/keep"
	ln -s "$TH/backup" "$sc"
	serve 1.1.0 1.0.0
	# Node unreachable at the default (nothing serves it): tolerated, since neither old nor new
	# sidecar is present behind the symlinked root (TOLERANT=1), so the upgrade succeeds without a
	# sidecar build, keeping this test focused on prune_sidecars.
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
	sc=$(data_of)/eitri/sidecar
	mkdir -p "$sc/eeeeeee"
	rm -rf "$sc/aaaaaaa"
	sc_setup_release 1.0.0 "$REV_A"
	sc_inst_setup -- --sidecar-only
	expect_rc 0
	expect_dir "$sc/aaaaaaa"
	expect_dir "$sc/eeeeeee"
}

# ---------------------------------------------------------------------------------------------
# --sidecar-only

TESTS="$TESTS t_sidecar_only_dirname_release"
t_sidecar_only_dirname_release() {
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	# A different RELEASE planted in ~/.local/lib/eitri/ and at the test system path is not read.
	mkdir -p "$TH/.local/lib/eitri"
	printf 'EITRI_VERSION=9.9.9\nVERDANDI_REV=fffffff000000000000000000000000000000000\n' >"$TH/.local/lib/eitri/RELEASE"
	printf 'EITRI_VERSION=8.8.8\nVERDANDI_REV=eeeeeee000000000000000000000000000000000\n' >"$S/system/RELEASE"
	sc_inst_setup -- --sidecar-only
	expect_rc 0
	rm -f "$S/system/RELEASE"
	expect_dir "$(data_of)/eitri/sidecar/aaaaaaa"
	expect_absent "$(data_of)/eitri/sidecar/fffffff"
	expect_absent "$(data_of)/eitri/sidecar/eeeeeee"
}

TESTS="$TESTS t_sidecar_only_release_file"
t_sidecar_only_release_file() {
	serve 1.0.0
	sc_inst_setup -- --sidecar-only --release-file "$(served 1.0.0)/RELEASE"
	expect_rc 0
	expect_dir "$(data_of)/eitri/sidecar/aaaaaaa"
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
# --tarball builds a sidecar too.

TESTS="$TESTS t_tarball_install_builds_sidecar_from_beside_file"
t_tarball_install_builds_sidecar_from_beside_file() {
	# A --tarball install must set NV_REL_URL for a sidecar build even though --base-url is refused
	# with --tarball. Redirect Node to the fixture server. Verdandi needs no network here:
	# mk_release plants verdandi-aaaaaaa-source.tar.gz beside the tarball, and do_install uses it
	# through NV_VERDANDI_SOURCE_TARBALL_OVERRIDE.
	d=$S/fix/v1.0.0
	srv_mark
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --tarball "$d/eitri-1.0.0-x86_64-linux.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" \
		--release-signers "$SIGNERS"
	expect_rc 0
	expect_out "installed Eitri 1.0.0 into"
	expect_out "using the Verdandi source beside"
	expect_exec "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
	if srv_paths | grep -F verdandi- >/dev/null; then fail "the Verdandi source was fetched over the network: $(srv_paths)"; fi
}

TESTS="$TESTS t_tarball_verdandi_source_beside_file_copied_before_use"
t_tarball_verdandi_source_beside_file_copied_before_use() {
	# Copy the Verdandi source beside --tarball into this run's 0700 $NV_DL before hashing and
	# extracting it; reopening the original in a shared directory would allow a swap to run unverified
	# source and npm scripts after a valid checksum. Only the private copy may be read again.
	# Use a private tarball directory so corruption cannot reach shared fixtures. cp-swap corrupts the
	# original immediately after the real cp reads it, as the --tarball/--sums races in test_verify.sh do.
	d=$T/tarball-race
	mkdir -p "$d"
	cp -- "$S/fix/v1.0.0/eitri-1.0.0-x86_64-linux.tar.gz" "$S/fix/v1.0.0/SHA256SUMS" \
		"$S/fix/v1.0.0/SHA256SUMS.sig" "$S/fix/v1.0.0/verdandi-aaaaaaa-source.tar.gz" "$d/"
	printf 'evil: this must never reach npm ci or npm run build:binary\n' >"$T/evil-verdandi-source.tar.gz"
	printf '%s\n%s\n' "$d/verdandi-aaaaaaa-source.tar.gz" "$T/evil-verdandi-source.tar.gz" >"$S/logs/swap-verdandi"
	inst --stubs "$S/stubs-cpswap" --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --tarball "$d/eitri-1.0.0-x86_64-linux.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" \
		--release-signers "$SIGNERS"
	expect_rc 0
	expect_out "installed Eitri 1.0.0 into"
	expect_out "using the Verdandi source beside"
	expect_exec "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
	if [ ! -s "$S/logs/swap-verdandi.log" ]; then fail "the beside file was never swapped: the test did not run the race"; fi
	if [ -e "$d/verdandi-aaaaaaa-source.tar.gz" ] && ! grep -q evil "$d/verdandi-aaaaaaa-source.tar.gz" 2>/dev/null; then
		fail "the beside file was not actually corrupted after the read: the stub did not fire as intended"
	fi
}

TESTS="$TESTS t_tarball_install_falls_back_to_network_without_beside_file"
t_tarball_install_falls_back_to_network_without_beside_file() {
	# Without verdandi-<rev>-source.tar.gz beside --tarball, fetch it through NV_REL_URL.
	# --tarball refuses --base-url, so this uses NV_DEFAULT_BASE_URL (github.com). The curl-mirror
	# stub strips scheme and host and serves the path from $S/srv; staging the asset at that path
	# exercises the fallback with no real network.
	mkdir -p "$S/srv/HunterGrey-cyber/eitri/releases/download/v1.0.0"
	cp -- "$S/fix/v1.0.0/verdandi-aaaaaaa-source.tar.gz" \
		"$S/srv/HunterGrey-cyber/eitri/releases/download/v1.0.0/verdandi-aaaaaaa-source.tar.gz"
	d=$T/tarball-only
	mkdir -p "$d"
	cp -- "$S/fix/v1.0.0/eitri-1.0.0-x86_64-linux.tar.gz" "$S/fix/v1.0.0/SHA256SUMS" \
		"$S/fix/v1.0.0/SHA256SUMS.sig" "$d/"
	inst --stubs "$S/stubs-net" --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --tarball "$d/eitri-1.0.0-x86_64-linux.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" \
		--release-signers "$SIGNERS"
	expect_rc 0
	expect_out "installed Eitri 1.0.0 into"
	expect_no_out "using the Verdandi source beside"
	expect_exec "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
	if ! grep -qF 'HunterGrey-cyber/eitri/releases/download/v1.0.0/verdandi-aaaaaaa-source.tar.gz' "$S/logs/curl.log"; then
		fail "the Verdandi source was not fetched through the network fallback: $(cat "$S/logs/curl.log")"
	fi
}

# ---------------------------------------------------------------------------------------------
# --build-sidecar-into (the AUR entry point)

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
	# The AUR's entry point announces nothing about the wait: the PKGBUILD's own msg2 does, once
	# (packaging/tests/test_aur_pkgbuilds.py), so a line here would print it twice.
	expect_no_out '(1-3 min)'
	expect_no_out "$SLOW_LINE"
}

# A first build peaks near 1.3 GiB (three measured: 1.2 to 1.25 GiB in the work directory, 1.3 with the artifact
# copied out), so every route into the shared build asks for about 1.4 GiB before it starts.
TESTS="$TESTS t_first_build_asks_for_the_space_a_build_takes"
t_first_build_asks_for_the_space_a_build_takes() {
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	mkdir -p "$T/lowdisk"
	cat >"$T/lowdisk/df" <<'DF_STUB'
#!/bin/sh
echo 'Filesystem 1024-blocks Used Available Capacity Mounted on'
echo '/dev/stub 2000000 1000000 1000000 50% /'
DF_STUB
	chmod 0755 "$T/lowdisk/df"
	PRE_STUBS=$T/lowdisk
	sc_inst_setup -- --sidecar-only
	expect_fail "about 970 MiB free, --sidecar-only"
	expect_out 'only 976 MiB free'
	expect_out 'building the sidecar needs about 1.4 GiB'
	expect_absent "$(data_of)/eitri/sidecar/aaaaaaa"
	dest=$T/aur-sidecar-low
	inst -- --build-sidecar-into "$dest" --node "$NODE_FIXTURE_TARBALL" \
		--verdandi-source "$(served 1.0.0)/verdandi-aaaaaaa-source.tar.gz"
	expect_fail "about 970 MiB free, --build-sidecar-into"
	expect_out 'building the sidecar needs about 1.4 GiB'
	expect_absent "$dest"
	PRE_STUBS=
}

TESTS="$TESTS t_build_sidecar_into_node_cache_versioned_name"
t_build_sidecar_into_node_cache_versioned_name() {
	# Prime buildBinary.mjs's Node cache with the versioned filename, not node.tar.xz: it preserves
	# the basename handed to it but only looks up the versioned name. A different name would cause a
	# second download, discarding the verified copy. --keep-build leaves this mode's workdir under
	# NV_CACHE_NV so the cache filename can be asserted, as on the network path.
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	dest=$T/aur-sidecar-cache-name
	inst -- --keep-build --build-sidecar-into "$dest" --node "$NODE_FIXTURE_TARBALL" \
		--verdandi-source "$(served 1.0.0)/verdandi-aaaaaaa-source.tar.gz"
	expect_rc 0
	expect_exec "$dest/verdandi-claude-sidecar"
	wd=$TH/.cache/eitri/sidecar-build/aaaaaaa
	expect_file "$wd/repo/apps/claude-sidecar/build/node-cache/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
}

TESTS="$TESTS t_build_sidecar_into_node_swapped_after_hash"
t_build_sidecar_into_node_swapped_after_hash() {
	# Copy --node and --verdandi-source into the work directory before hashing and extracting them;
	# reopening the original after its hash would allow unverified content to be swapped in.
	# sha256sum-swap replaces --node's original path immediately after the first hash (of the copy;
	# resolve_setup_release/parse_release hash nothing). The build must use the genuine private copy.
	# Use a private copy of NODE_FIXTURE_TARBALL as --node so the swap cannot corrupt the shared fixture.
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
	# The original --node path holds the evil content, proving the swap happened; the successful
	# build must have used the genuine copy read before the swap.
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
	# --build-sidecar-into reads and writes no NV_DATA/XDG_DATA_HOME: its workdir is under NV_CACHE_NV
	# and its output is the caller's DIR. AUR's build() may set XDG_DATA_HOME outside HOME, so this mode
	# must not refuse on account of desktop entries, licences, private nvim or sidecars it never touches.
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

TESTS="$TESTS t_bundled_protoc_legacy_warning t_bundled_protoc_missing_refuses t_bundled_protoc_download_fatal"
t_bundled_protoc_legacy_warning() {
	# This installer no longer installs 0.3.0 itself (its layout is older), but an installed 0.3.0's
	# eitri setup still builds its sidecar, with the legacy warning and npm's install scripts.
	serve 0.3.0
	sc_setup_release 0.3.0 "$REV_A"
	sc_inst_setup --set "EITRI_INSTALL_TEST_NPM_LOG=$T/npm.log" -- --sidecar-only
	expect_rc 0
	expect_out "warning:"
	expect_out "unverified compiler download"
	if grep -q -- --ignore-scripts "$T/npm.log"; then fail "legacy build disabled scripts"; fi
}

t_bundled_protoc_missing_refuses() {
	serve 0.3.1
	sc_inst_net
	expect_fail
	expect_out "has no protoc pin: refusing"
	expect_absent "$TH/.local/lib/eitri/shell"
	sc_setup_release 0.3.1 "$REV_A"
	sc_inst_setup -- --sidecar-only
	expect_fail
	expect_out "has no protoc pin: refusing"
}

t_bundled_protoc_download_fatal() {
	serve 1.0.0
	sc_inst_net --set "EITRI_INSTALL_TEST_GRPC_BASE_URL=http://127.0.0.1:$PORT/missing-grpc"
	expect_fail
	expect_out "protoc"
	expect_absent "$TH/.local/lib/eitri/shell"
}
