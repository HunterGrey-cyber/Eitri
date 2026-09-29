# The nvim offer (plan 2026-09-27-v1-dist, Task 11, spec §7). Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

# nv_inst_net INSTALLER_ARGS...: a normal inst_net network install, with the nvim release download
# redirected to the fixture server's own copy of setup_nvim_fixtures' fake tarball (served at the
# same releases/download/vX.Y.Z/-shaped layout the real github.com/neovim/neovim uses). Every other
# test file's installs never set NEOVIBE_INSTALL_TEST_NVIM_BASE_URL, so nvim_dist_base's own
# test-mode default (a refused loopback port) keeps them from ever trying to fetch nvim at all --
# consistent with them never seeing the fixture's own nvim/RELEASE fields either way, since $S/stubs'
# own nvim (NVIM v0.11.4) is already adequate. Unlike sc_inst_net, the caller's own args ARE
# installer arguments (--with-nvim and friends), so they go after inst()'s own `--`, not before it.
nv_inst_net() {
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" \
		-- "$@" --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
}

# old_nvim_stubs: a directory holding just an nvim reporting NVIM v0.9.5 (older than NV_NVIM_FLOOR,
# 0.10.0) -- set as PRE_STUBS so it is searched before $S/stubs' own adequate one (harness.sh's own
# inst()).
old_nvim_stubs() {
	_ons_dir=$T/old-nvim
	mkdir -p "$_ons_dir"
	{
		printf '#!/bin/sh\n'
		# shellcheck disable=SC2016 # the literal line, variables and all
		printf 'case $1 in\n'
		printf -- '--version) echo "NVIM v0.9.5" ;;\n'
		# shellcheck disable=SC2016 # the literal line, variables and all
		printf '*) echo "old-nvim fixture: unexpected argv: $*" >&2; exit 1 ;;\n'
		printf 'esac\n'
	} >"$_ons_dir/nvim"
	chmod 0755 "$_ons_dir/nvim"
	printf '%s\n' "$_ons_dir"
}

# nvim_private_bin VERSION: the path the offer installs nvim to, spec §7's exact layout contract.
nvim_private_bin() { printf '%s\n' "$(data_of)/neovibe/nvim/$1/bin/nvim"; }

# ---------------------------------------------------------------------------------------------
# The offer itself: an old (or missing) PATH nvim, answered non-interactively

TESTS="$TESTS t_nvim_offer_with_nvim_flag_installs"
t_nvim_offer_with_nvim_flag_installs() {
	serve 1.0.0
	PRE_STUBS=$(old_nvim_stubs)
	nv_inst_net --with-nvim
	expect_rc 0
	bin=$(nvim_private_bin "$NVIM_FIXTURE_VERSION")
	expect_exec "$bin"
	have=$("$bin" --version)
	expect_eq "$have" "NVIM v$NVIM_FIXTURE_VERSION" "the installed nvim's own --version"
	# Never on PATH, never linked into ~/.local/bin, and no other nvim/vim/vi is created, replaced or
	# removed (spec §7; the planted ~/.local/bin/nvim and vim are checked byte-identical by
	# after_each on every test in this suite).
	expect_absent "$TH/.local/bin/nvim.new"
	if [ -e "$TH/.local/bin/nvim" ]; then
		if ! grep -q 'planted nvim' "$TH/.local/bin/nvim"; then fail "the planted .local/bin/nvim was replaced"; fi
	fi
	expect_out "installed nvim $NVIM_FIXTURE_VERSION into"
}

TESTS="$TESTS t_nvim_offer_yes_flag_also_installs"
t_nvim_offer_yes_flag_also_installs() {
	# --yes (spec §6.2) is the generic affirmative and answers this installer's only prompt too, not
	# just --with-nvim.
	serve 1.0.0
	PRE_STUBS=$(old_nvim_stubs)
	nv_inst_net --yes
	expect_rc 0
	expect_exec "$(nvim_private_bin "$NVIM_FIXTURE_VERSION")"
}

TESTS="$TESTS t_nvim_offer_no_tty_reports_only"
t_nvim_offer_no_tty_reports_only() {
	# No flag, and no controlling terminal (this whole suite runs with none -- `tty` reports "not a
	# tty" for every test here, the same shape a `curl | sh` pipe or a CI runner has): spec §6.2's
	# own words, "nvim is only reported, never fetched".
	serve 1.0.0
	PRE_STUBS=$(old_nvim_stubs)
	nv_inst_net
	expect_rc 0
	expect_absent "$(data_of)/neovibe/nvim"
	expect_out "no terminal to ask on: not fetching a private nvim"
	expect_out "nvim 0.9.5 is older than"
}

TESTS="$TESTS t_nvim_offer_no_tty_explicit_setsid"
t_nvim_offer_no_tty_explicit_setsid() {
	# The same thing, spelled the way the brief's own acceptance bullet does: setsid, no controlling
	# terminal, stdin from /dev/null, no flag.
	serve 1.0.0
	PRE_STUBS=$(old_nvim_stubs)
	"$WRAP" --home "$TH" --cwd "$T/cwd" --stubs "$PRE_STUBS" --stubs "$S/stubs" \
		--set NEOVIBE_INSTALL_TEST=1 \
		--set "NEOVIBE_INSTALL_TEST_LIBDIRS=$S/libs/ok" \
		--set "NEOVIBE_INSTALL_TEST_SYSTEM_RELEASE=$S/system/RELEASE" \
		--set "NEOVIBE_INSTALL_TEST_OS_RELEASE=$S/osrel/ubuntu-24.04" \
		--set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" \
		-- setsid -w "$NV_SH" "$INSTALLER" --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS" \
		</dev/null >"$OUT" 2>&1
	RC=$?
	RUNS=$((RUNS + 1))
	cp "$OUT" "$T/out.$RUNS"
	expect_rc 0
	expect_absent "$(data_of)/neovibe/nvim"
	expect_out "no terminal to ask on: not fetching a private nvim"
}

TESTS="$TESTS t_nvim_offer_no_nvim_flag_skips"
t_nvim_offer_no_nvim_flag_skips() {
	serve 1.0.0
	PRE_STUBS=$(old_nvim_stubs)
	nv_inst_net --no-nvim
	expect_rc 0
	expect_absent "$(data_of)/neovibe/nvim"
}

TESTS="$TESTS t_nvim_offer_adequate_path_nvim_wins_even_with_flag"
t_nvim_offer_adequate_path_nvim_wins_even_with_flag() {
	# $S/stubs' own nvim (NVIM v0.11.4) is already adequate -- no PRE_STUBS override here. Even
	# --with-nvim must not replace it (spec: "an adequate PATH nvim always wins").
	serve 1.0.0
	nv_inst_net --with-nvim
	expect_rc 0
	expect_absent "$(data_of)/neovibe/nvim"
	expect_out "nvim 0.11.4: ok"
}

TESTS="$TESTS t_nvim_offer_checksum_mismatch_refuses"
t_nvim_offer_checksum_mismatch_refuses() {
	serve 1.0.0
	PRE_STUBS=$(old_nvim_stubs)
	# A served tarball that does not match RELEASE's own NVIM_SHA256_linux_x86_64.
	echo corrupt >"$S/srv/nvim-releases/v$NVIM_FIXTURE_VERSION/nvim-linux-x86_64.tar.gz"
	nv_inst_net --with-nvim
	expect_rc 1
	expect_out "checksum mismatch for nvim-linux-x86_64.tar.gz"
	expect_absent "$(data_of)/neovibe/nvim/$NVIM_FIXTURE_VERSION"
	# Restore it for every test that runs after this one in the same suite process.
	cp "$S/fix/nvim-linux-x86_64.tar.gz" "$S/srv/nvim-releases/v$NVIM_FIXTURE_VERSION/nvim-linux-x86_64.tar.gz"
}

TESTS="$TESTS t_nvim_offer_network_failure_warns_install_still_succeeds"
t_nvim_offer_network_failure_warns_install_still_succeeds() {
	# The install path's own offer is best-effort (TOLERANT=1): unlike --nvim-only, a network failure
	# here warns and the rest of the install still finishes.
	serve 1.0.0
	PRE_STUBS=$(old_nvim_stubs)
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:1" \
		-- --with-nvim --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	expect_rc 0
	expect_out "could not download nvim $NVIM_FIXTURE_VERSION"
	expect_out "installed neovibe 1.0.0 into"
	expect_absent "$(data_of)/neovibe/nvim"
	# installer-claude-1: this is install.sh:776's own recovery hint. Its exact command,
	# `neovibe setup --nvim-only`, used to die "--sidecar-only and --nvim-only cannot be combined"
	# through the real launcher (packaging/neovibe.launcher.sh always prepended --sidecar-only);
	# packaging/test_launcher.sh's own "passes straight through" tests hold that half now. This
	# assertion is the installer's own half: the hint must keep naming exactly that command.
	expect_out 'run "neovibe setup --nvim-only" once you have network access'
}

# ---------------------------------------------------------------------------------------------
# --nvim-only (spec §6.2): the offer alone, on an existing install, regardless of PATH

TESTS="$TESTS t_nvim_only_on_existing_install"
t_nvim_only_on_existing_install() {
	serve 1.0.0
	nv_inst_net --no-nvim
	expect_rc 0
	expect_absent "$(data_of)/neovibe/nvim"
	# --nvim-only reads RELEASE beside the running script, the same rule --sidecar-only holds -- the
	# script just installed itself as neovibe-setup right beside its own RELEASE.
	INSTALLER_UNDER_TEST=$TH/.local/lib/neovibe/neovibe-setup
	PRE_STUBS=$(old_nvim_stubs)
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" \
		-- --nvim-only
	expect_rc 0
	expect_exec "$(nvim_private_bin "$NVIM_FIXTURE_VERSION")"
	INSTALLER_UNDER_TEST=
}

TESTS="$TESTS t_nvim_only_idempotent_when_already_present"
t_nvim_only_idempotent_when_already_present() {
	serve 1.0.0
	PRE_STUBS=$(old_nvim_stubs)
	nv_inst_net --with-nvim
	expect_rc 0
	bin=$(nvim_private_bin "$NVIM_FIXTURE_VERSION")
	before_ino=$(stat -c %i "$bin")
	INSTALLER_UNDER_TEST=$TH/.local/lib/neovibe/neovibe-setup
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:1" -- --nvim-only
	expect_rc 0
	expect_out "already installed at"
	after_ino=$(stat -c %i "$bin")
	expect_eq "$after_ino" "$before_ino" "a present nvim is left alone by a second --nvim-only"
	INSTALLER_UNDER_TEST=
}

TESTS="$TESTS t_nvim_only_network_failure_is_fatal"
t_nvim_only_network_failure_is_fatal() {
	# Unlike the install path's own best-effort offer, --nvim-only is the explicit ask: a network
	# failure here is fatal (TOLERANT=0), not a warning.
	serve 1.0.0
	nv_inst_net --no-nvim
	expect_rc 0
	INSTALLER_UNDER_TEST=$TH/.local/lib/neovibe/neovibe-setup
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:1" -- --nvim-only
	expect_rc 1
	expect_absent "$(data_of)/neovibe/nvim"
	INSTALLER_UNDER_TEST=
}

TESTS="$TESTS t_nvim_only_reads_release_beside_the_running_script"
t_nvim_only_reads_release_beside_the_running_script() {
	# The same rule --sidecar-only holds (spec §6.2): a .deb/.rpm/tarball install's own neovibe-setup
	# never searches ~/.local and then /usr, it reads RELEASE beside itself (or --release-file).
	sc_setup_release 1.0.0 "$REV_A"
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" \
		-- --nvim-only
	expect_rc 0
	expect_exec "$(nvim_private_bin "$NVIM_FIXTURE_VERSION")"
}

# ---------------------------------------------------------------------------------------------
# --nvim-offer (fix round 1, review-2, installer-claude-1 / installer-codex-3 / [codex] duplicate):
# the SAME conditional offer a normal install runs, reachable standalone against an already
# -installed neovibe -- what plain `neovibe setup`'s second call runs since the fix, replacing the
# old --nvim-only there. Contrast every test here with --nvim-only's own tests just above: the
# report's own repro was plain `neovibe setup` on a machine with an already-adequate PATH nvim
# printing "installed nvim ... into ..." (repro 1) and, with the network unreachable, dying "could
# not download nvim" (repro 2) -- both because do_nvim_only ignores NV_NVIM_OK entirely.

TESTS="$TESTS t_nvim_offer_mode_skips_when_path_nvim_is_adequate"
t_nvim_offer_mode_skips_when_path_nvim_is_adequate() {
	# Reproduces the report's repro 1 and 2 together: an adequate PATH nvim (the default
	# $S/stubs/nvim, NVIM v0.11.4) must suppress the offer before the network is ever touched, with
	# no --yes/--with-nvim/--no-nvim and no tty at all -- exactly plain `neovibe setup`'s own second
	# call. srv_paths (the fixture server's real request log), not just NV_DATA/neovibe/nvim being
	# absent, is what tells a genuine skip apart from an attempt that merely failed to reach it.
	sc_setup_release 1.0.0 "$REV_A"
	srv_mark
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" -- --nvim-offer
	expect_rc 0
	expect_absent "$(data_of)/neovibe/nvim"
	if srv_paths | grep -F nvim-releases >/dev/null; then
		fail "nvim was fetched despite an adequate PATH nvim: $(srv_paths)"
	fi
}

TESTS="$TESTS t_nvim_offer_mode_no_tty_no_flag_reports_only"
t_nvim_offer_mode_no_tty_no_flag_reports_only() {
	# An inadequate PATH nvim, no flag, no controlling terminal (this whole suite has none): spec
	# §6.2's own words, "nvim is only reported, never fetched" -- the main install path's own
	# t_nvim_offer_no_tty_reports_only, run through the standalone --nvim-offer entry point instead.
	sc_setup_release 1.0.0 "$REV_A"
	PRE_STUBS=$(old_nvim_stubs)
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" -- --nvim-offer
	expect_rc 0
	expect_absent "$(data_of)/neovibe/nvim"
	expect_out "no terminal to ask on: not fetching a private nvim"
	expect_out "nvim 0.9.5 is older than"
}

TESTS="$TESTS t_nvim_offer_mode_with_yes_installs"
t_nvim_offer_mode_with_yes_installs() {
	# --yes (the generic affirmative) and --with-nvim both still reach the offer through this mode --
	# the launcher's own comment claims it needs no special handling for either, since --nvim-offer
	# reads them itself.
	sc_setup_release 1.0.0 "$REV_A"
	PRE_STUBS=$(old_nvim_stubs)
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" -- --nvim-offer --yes
	expect_rc 0
	expect_exec "$(nvim_private_bin "$NVIM_FIXTURE_VERSION")"
	expect_out "installed nvim $NVIM_FIXTURE_VERSION into"
}

TESTS="$TESTS t_nvim_offer_mode_aarch64_no_x86_64_nvim_offered"
t_nvim_offer_mode_aarch64_no_x86_64_nvim_offered() {
	# installer-codex-5's own regression (the pinned nvim is x86_64 only, and this best-effort offer
	# had no architecture guard) used to be exercised through --from-source, the only mode that
	# reached maybe_offer_nvim with no check_platform gate anywhere above it. F4 (v1-dist
	# whole-branch review, 2026-09-28) made --from-source itself refuse up front on non-x86_64, which
	# moved this coverage here: --nvim-offer reaches maybe_offer_nvim just as directly (do_nvim_offer
	# calls no check_platform either), so the architecture guard inside maybe_offer_nvim itself stays
	# under test.
	sc_setup_release 1.0.0 "$REV_A"
	d_stubs=$T/stubs-aarch64-nvim-offer
	rm -rf "$d_stubs"
	mkdir -p "$d_stubs"
	cp "$(old_nvim_stubs)/nvim" "$d_stubs/nvim"
	cp "$FIXTURES/uname-aarch64" "$d_stubs/uname"
	chmod 0755 "$d_stubs"/*
	PRE_STUBS=$d_stubs
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" -- --nvim-offer --yes
	expect_rc 0
	expect_out 'no prebuilt nvim for aarch64 (only x86_64 is offered)'
	expect_absent "$(data_of)/neovibe/nvim"
}

TESTS="$TESTS t_nvim_offer_mode_no_nvim_flag_skips"
t_nvim_offer_mode_no_nvim_flag_skips() {
	sc_setup_release 1.0.0 "$REV_A"
	PRE_STUBS=$(old_nvim_stubs)
	srv_mark
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" -- --nvim-offer --no-nvim
	expect_rc 0
	expect_absent "$(data_of)/neovibe/nvim"
	if srv_paths | grep -F nvim-releases >/dev/null; then
		fail "nvim was fetched despite --no-nvim: $(srv_paths)"
	fi
}

TESTS="$TESTS t_nvim_offer_mode_skips_when_private_copy_present"
t_nvim_offer_mode_skips_when_private_copy_present() {
	# Fix round 2 (review-2): --nvim-offer never checked nvim_private_state, so on the spec's own
	# flagship case -- a PATH nvim older than the floor (Ubuntu 24.04's apt 0.9.5) with neovibe's own
	# private copy of the pinned version already installed -- every later plain `neovibe setup`
	# warned "install a newer nvim" (neovibe does not use that one: core/src/nvim_bin.rs picks the
	# private copy), asked again, and with --with-nvim downloaded and replaced the very same version.
	# The reproduction: `--dry-run --with-nvim` printed "would install nvim 0.11.2", where
	# --nvim-only on the same state printed "already installed".
	sc_setup_release 1.0.0 "$REV_A"
	PRE_STUBS=$(old_nvim_stubs)
	bin=$(nvim_private_bin "$NVIM_FIXTURE_VERSION")
	mkdir -p "$(dirname "$bin")"
	printf '#!/bin/sh\necho "NVIM v%s"\n' "$NVIM_FIXTURE_VERSION" >"$bin"
	chmod 0755 "$bin"
	before_ino=$(stat -c %i "$bin")
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" \
		-- --nvim-offer --dry-run --with-nvim
	expect_rc 0
	expect_out "nvim $NVIM_FIXTURE_VERSION: already installed at $bin"
	expect_no_out "would install nvim"
	expect_no_out "is older than"
	srv_mark
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" \
		-- --nvim-offer --with-nvim
	expect_rc 0
	expect_out "nvim $NVIM_FIXTURE_VERSION: already installed at $bin"
	expect_no_out "installed nvim $NVIM_FIXTURE_VERSION into"
	if srv_paths | grep -F nvim-releases >/dev/null; then
		fail "nvim was fetched again despite the private copy: $(srv_paths)"
	fi
	after_ino=$(stat -c %i "$bin")
	expect_eq "$after_ino" "$before_ino" "a present private nvim is left alone by --nvim-offer"
}

TESTS="$TESTS t_nvim_offer_upgrade_skips_when_private_copy_present"
t_nvim_offer_upgrade_skips_when_private_copy_present() {
	# The same gap on the install path's own offer (maybe_offer_nvim, which predates --nvim-offer):
	# an upgrade whose new RELEASE pins the nvim version already installed privately re-fetched and
	# replaced it on --with-nvim/--yes, and asked again on a tty.
	serve 1.1.0 1.0.0
	PRE_STUBS=$(old_nvim_stubs)
	nv_inst_net --version 1.0.0 --with-nvim
	expect_rc 0
	bin=$(nvim_private_bin "$NVIM_FIXTURE_VERSION")
	expect_exec "$bin"
	before_ino=$(stat -c %i "$bin")
	srv_mark
	nv_inst_net --with-nvim
	expect_rc 0
	expect_out "installed neovibe 1.1.0 into"
	expect_out "nvim $NVIM_FIXTURE_VERSION: already installed at $bin"
	expect_no_out "installed nvim $NVIM_FIXTURE_VERSION into"
	if srv_paths | grep -F nvim-releases >/dev/null; then
		fail "nvim was fetched again on upgrade despite the private copy: $(srv_paths)"
	fi
	after_ino=$(stat -c %i "$bin")
	expect_eq "$after_ino" "$before_ino" "a present private nvim is left alone by an upgrade's offer"
}

TESTS="$TESTS t_nvim_offer_up_to_date_still_offers"
t_nvim_offer_up_to_date_still_offers() {
	# M5 (v1-dist whole-branch review, 2026-09-28): the "up to date" branch used to return before
	# maybe_offer_nvim ever ran at all, so re-running the same installer with --with-nvim/--yes
	# after a --no-nvim install fetched nothing and said nothing. Reproduced (installer-codex-7, the
	# verifier's t_vprobe_with_nvim_when_up_to_date): "nvim 0.9.5 is older...", then "neovibe 1.0.0
	# is up to date", rc=0, no private nvim. The sidecar must genuinely be present after the first
	# install (via NODE_BASE_URL, like sc_inst_net) so the second run actually reaches the fast
	# "up to date" path (NV_SIDECAR_PRESENT=1 && NV_COMPLETE=1) rather than the ordinary reinstall
	# one, which already calls maybe_offer_nvim on its own and would not exercise this fix.
	serve 1.0.0
	PRE_STUBS=$(old_nvim_stubs)
	inst --set "NEOVIBE_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		--set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" \
		-- --no-nvim --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	expect_rc 0
	expect_out "nvim 0.9.5 is older than"
	expect_absent "$(data_of)/neovibe/nvim"
	sc=$(data_of)/neovibe/sidecar
	expect_dir "$sc/aaaaaaa"
	srv_mark
	inst --set "NEOVIBE_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		--set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:$PORT/nvim-releases" \
		-- --with-nvim --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	expect_rc 0
	expect_out "neovibe 1.0.0 is up to date"
	expect_out "installed nvim $NVIM_FIXTURE_VERSION into"
	expect_exec "$(nvim_private_bin "$NVIM_FIXTURE_VERSION")"
	if ! srv_paths | grep -F nvim-releases >/dev/null; then
		fail "nvim was never fetched on the up-to-date re-run: $(srv_paths)"
	fi
}

TESTS="$TESTS t_nvim_offer_mode_network_failure_warns_not_fatal"
t_nvim_offer_mode_network_failure_warns_not_fatal() {
	# The exact contrast with --nvim-only's own t_nvim_only_network_failure_is_fatal just above, and
	# with the report's repro 2 (which used --nvim-only's own forced fetch and died): --nvim-offer is
	# TOLERANT=1, the same as a normal install's own best-effort offer, so an unreachable nvim host
	# warns and the run still exits 0 rather than dying.
	sc_setup_release 1.0.0 "$REV_A"
	PRE_STUBS=$(old_nvim_stubs)
	inst --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:1" -- --nvim-offer --with-nvim
	expect_rc 0
	expect_out "could not download nvim $NVIM_FIXTURE_VERSION"
	expect_absent "$(data_of)/neovibe/nvim"
}

# ---------------------------------------------------------------------------------------------
# Uninstall removes the private copy only -- already exercised by test_uninstall.sh's own
# t_uninstall_exact et al. (populate plants $(data_of)/neovibe/nvim/0.11.4/bin/nvim; the KEPT_TREE
# after --uninstall has no neovibe/ under .local/share at all). Nothing further to add here.

# ---------------------------------------------------------------------------------------------
# SH-3 (spec §6.1): a real controlling terminal exists while stdin still carries the script itself
# (`cat install.sh | sh -s -- ...` under `script -qc`). Answering 'y' on that terminal runs the
# offer, and the rest of the script still executes -- proving the prompt reads /dev/tty and never
# tests whether stdin (fd 0, here entirely the script's own text) is a terminal.

TESTS="$TESTS t_nvim_offer_piped_with_real_tty_accepts"
t_nvim_offer_piped_with_real_tty_accepts() {
	if ! command -v script >/dev/null 2>&1; then
		fail "script (util-linux) is not installed: cannot exercise SH-3's real-tty case"
		return 0
	fi
	serve 1.0.0
	PRE_STUBS=$(old_nvim_stubs)
	mkdir -p "$T/cwd2"
	_pty_runner=$T/pty-run.sh
	{
		printf '#!/bin/sh\n'
		printf 'exec "%s" --home "%s" --cwd "%s" --stubs "%s" --stubs "%s" \\\n' \
			"$WRAP" "$TH" "$T/cwd2" "$PRE_STUBS" "$S/stubs"
		printf ' --set NEOVIBE_INSTALL_TEST=1 --set "NEOVIBE_INSTALL_TEST_LIBDIRS=%s/libs/ok" \\\n' "$S"
		printf ' --set "NEOVIBE_INSTALL_TEST_SYSTEM_RELEASE=%s/system/RELEASE" \\\n' "$S"
		printf ' --set "NEOVIBE_INSTALL_TEST_OS_RELEASE=%s/osrel/ubuntu-24.04" \\\n' "$S"
		printf ' --set "NEOVIBE_INSTALL_TEST_NVIM_BASE_URL=http://127.0.0.1:%s/nvim-releases" \\\n' "$PORT"
		printf -- ' -- "%s" -s -- --base-url "http://127.0.0.1:%s" --release-signers "%s" < "%s"\n' \
			"$NV_SH" "$PORT" "$SIGNERS" "$INSTALLER"
	} >"$_pty_runner"
	chmod 0755 "$_pty_runner"
	printf 'y\n' | script -qec "$_pty_runner" "$T/script.session" >"$OUT" 2>&1
	RC=$?
	RUNS=$((RUNS + 1))
	cp "$OUT" "$T/out.$RUNS"
	expect_rc 0 "the piped, real-tty install (SH-3)"
	expect_exec "$(nvim_private_bin "$NVIM_FIXTURE_VERSION")"
	expect_out "installed neovibe 1.0.0 into"
}
