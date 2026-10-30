# Uninstall and purge. Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

# populate: a complete install, plus files uninstall must keep.
populate() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	plant_sidecar eeeeeee
	mkdir -p "$(data_of)/eitri/nvim/0.11.4/bin"
	printf '#!/bin/sh\n' >"$(data_of)/eitri/nvim/0.11.4/bin/nvim"
	mkdir -p "$TH/.config/eitri" "$TH/.local/state/eitri/layout" "$TH/.cache/eitri/sidecar-build/aaaaaaa"
	echo 'print("mine")' >"$TH/.config/eitri/init.lua"
	echo '{}' >"$TH/.local/state/eitri/layout/0123456789abcdef.json"
	echo junk >"$TH/.cache/eitri/sidecar-build/aaaaaaa/junk"
}

KEPT_TREE='./.cache d
./.config d
./.config/eitri d
./.config/eitri/init.lua f
./.local d
./.local/bin d
./.local/lib d
./.local/share d
./.local/share/applications d
./.local/state d
./.local/state/eitri d
./.local/state/eitri/layout d
./.local/state/eitri/layout/0123456789abcdef.json f'

TESTS="$TESTS t_uninstall_exact"
t_uninstall_exact() {
	populate
	inst -- --uninstall
	expect_rc 0
	expect_eq "$(tree "$TH")" "$KEPT_TREE" "the tree after --uninstall"
	expect_out 'Eitri is uninstalled'
	expect_out "kept your settings ($TH/.config/eitri) and state ($TH/.local/state/eitri)"
	# Again: nothing left to remove.
	inst -- --uninstall
	expect_rc 0
	expect_out 'nothing to remove'
	expect_eq "$(tree "$TH")" "$KEPT_TREE" "the tree after a second --uninstall"
}

TESTS="$TESTS t_uninstall_foreign_launcher_kept"
t_uninstall_foreign_launcher_kept() {
	serve 1.0.0
	inst_net
	expect_rc 0
	printf '#!/bin/sh\necho mine\n' >"$TH/.local/bin/eitri"
	mine=$(sha256sum <"$TH/.local/bin/eitri")
	inst -- --uninstall
	expect_rc 0
	expect_out "$TH/.local/bin/eitri was left alone: it was not installed by this installer"
	expect_eq "$(sha256sum <"$TH/.local/bin/eitri")" "$mine" "the foreign launcher"
	expect_absent "$TH/.local/lib/eitri"
	# Nor is a symlink removed.
	rm "$TH/.local/bin/eitri"
	ln -s "$TH/.local/bin/nvim" "$TH/.local/bin/eitri"
	inst -- --uninstall
	expect_rc 0
	if [ ! -L "$TH/.local/bin/eitri" ]; then fail "a symlinked launcher was removed"; fi
}

TESTS="$TESTS t_uninstall_keeps_system_rev"
t_uninstall_keeps_system_rev() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	plant_sidecar ddddddd
	printf 'EITRI_VERSION=1.0.0\nVERDANDI_REV=ddddddd000000000000000000000000000000000\n' >"$S/system/RELEASE"
	inst -- --uninstall
	rm -f "$S/system/RELEASE"
	expect_rc 0
	expect_out 'keeping the sidecar for verdandi ddddddd'
	expect_absent "$(data_of)/eitri/sidecar/aaaaaaa"
	expect_exec "$(data_of)/eitri/sidecar/ddddddd/verdandi-claude-sidecar"
	expect_file "$(data_of)/eitri/sidecar/ddddddd/BUILD"
}

TESTS="$TESTS t_unreadable_system_release_stops"
t_unreadable_system_release_stops() {
	# An unreadable system RELEASE makes its sidecar rev unknowable: do not remove any on a guess.
	# Under bash outside POSIX mode, errexit is off inside $(...), so that read failure must be
	# propagated explicitly to prevent removing the system install's sidecar.
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	plant_sidecar ddddddd
	printf 'EITRI_VERSION=1.0.0\nVERDANDI_REV=ddddddd000000000000000000000000000000000\n' >"$S/system/RELEASE"
	chmod 000 "$S/system/RELEASE"
	if [ -r "$S/system/RELEASE" ]; then
		# Run as root, the file is readable anyway: nothing to test.
		rm -f "$S/system/RELEASE"
		return 0
	fi
	before=$(snap_but_lock "$TH")
	inst -- --uninstall
	expect_fail "an uninstall with an unreadable system RELEASE"
	expect_out "$S/system/RELEASE cannot be read"
	expect_eq "$(snap_but_lock "$TH")" "$before" "the home after a refused uninstall"
	# An upgrade stops the same way, before it downloads or changes anything.
	serve 1.1.0 1.0.0
	srv_mark
	before=$(snap_but_lock "$TH")
	inst_net
	expect_fail "an upgrade with an unreadable system RELEASE"
	expect_out "$S/system/RELEASE cannot be read"
	expect_eq "$(snap_but_lock "$TH")" "$before" "the home after a refused upgrade"
	if srv_paths | grep -F .tar.gz >/dev/null; then fail "a tarball was fetched: $(srv_paths)"; fi
	rm -f "$S/system/RELEASE"
}

# refused_with_system_release CONTENT: an uninstall, then an upgrade, with the system RELEASE
# holding CONTENT (printf format), each refused before it changes anything.
refused_with_system_release() {
	# shellcheck disable=SC2059 # CONTENT is a format by design: \n separates its lines
	printf "$1" >"$S/system/RELEASE"
	before=$(snap_but_lock "$TH")
	inst -- --uninstall
	expect_fail "an uninstall with a system RELEASE of [$1]"
	expect_out "$S/system/RELEASE names no valid VERDANDI_REV"
	expect_out 'reinstall or remove the Eitri package that owns it'
	expect_eq "$(snap_but_lock "$TH")" "$before" "the home after a refused uninstall ([$1])"
	serve 1.1.0 1.0.0
	srv_mark
	inst_net
	expect_fail "an upgrade with a system RELEASE of [$1]"
	expect_out "$S/system/RELEASE names no valid VERDANDI_REV"
	expect_eq "$(snap_but_lock "$TH")" "$before" "the home after a refused upgrade ([$1])"
	if srv_paths | grep -F .tar.gz >/dev/null; then fail "a tarball was fetched: $(srv_paths)"; fi
	rm -f "$S/system/RELEASE"
}

TESTS="$TESTS t_malformed_system_release_stops"
t_malformed_system_release_stops() {
	# A readable system RELEASE with no valid rev is as unsafe as an unreadable one: which sidecar
	# it needs is unknown, so none may be removed on a guess. A quoted rev must not mean "no sidecar".
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	plant_sidecar ddddddd
	refused_with_system_release 'EITRI_VERSION=1.0.0\nVERDANDI_REV="ddddddd000000000000000000000000000000000"\n'
	refused_with_system_release 'EITRI_VERSION=1.0.0\nVERDANDI_REV=\n'
	refused_with_system_release 'EITRI_VERSION=1.0.0\n'
	refused_with_system_release 'VERDANDI_REV=DDDDDDD000000000000000000000000000000000\n'
	expect_exec "$(data_of)/eitri/sidecar/ddddddd/verdandi-claude-sidecar"
	expect_exec "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
}

TESTS="$TESTS t_purge_exact"
t_purge_exact() {
	populate
	mkdir -p "$TH/other config"
	echo keep >"$TH/other config/init.lua"
	inst --set "EITRI_CONFIG_DIR=$TH/other config" -- --uninstall --purge
	expect_rc 0
	expect_eq "$(tree "$TH")" "./.cache d
./.config d
./.local d
./.local/bin d
./.local/lib d
./.local/share d
./.local/share/applications d
./.local/state d
./other config d
./other config/init.lua f" "the tree after --uninstall --purge"
	expect_no_out 'kept your settings'
}

TESTS="$TESTS t_purge_removes_the_config_directory_xdg_config_home_names"
t_purge_removes_the_config_directory_xdg_config_home_names() {
	# Eitri reads its settings from $XDG_CONFIG_HOME/eitri when that variable is absolute, so that
	# is the directory a purge removes; ~/.config/eitri is then some other copy and stays.
	populate
	mkdir -p "$TH/xdgconf/eitri"
	echo 'print("xdg")' >"$TH/xdgconf/eitri/init.lua"
	inst --set "XDG_CONFIG_HOME=$TH/xdgconf" -- --uninstall --purge
	expect_rc 0
	expect_out "removing $TH/xdgconf/eitri"
	expect_absent "$TH/xdgconf/eitri"
	expect_dir "$TH/xdgconf"
	expect_eq "$(cat "$TH/.config/eitri/init.lua")" 'print("mine")' "the settings under ~/.config"
	expect_absent "$TH/.local/state/eitri"
}

TESTS="$TESTS t_purge_trims_a_trailing_slash_from_xdg_config_home"
t_purge_trims_a_trailing_slash_from_xdg_config_home() {
	# Trailing slashes in XDG_CONFIG_HOME name the same directory. Trim them so the settings path
	# compares equal to EITRI_CONFIG_DIR naming that directory, rather than falsely reporting it kept.
	for trailing in / //; do
		populate
		mkdir -p "$TH/xdgconf/eitri"
		echo 'print("xdg")' >"$TH/xdgconf/eitri/init.lua"
		inst --set "XDG_CONFIG_HOME=$TH/xdgconf$trailing" -- --uninstall
		expect_rc 0
		expect_out "kept your settings ($TH/xdgconf/eitri) and state ($TH/.local/state/eitri)"
		inst --set "XDG_CONFIG_HOME=$TH/xdgconf$trailing" --set "EITRI_CONFIG_DIR=$TH/xdgconf/eitri" -- --uninstall --purge
		expect_rc 0
		expect_out "removing $TH/xdgconf/eitri"
		expect_no_out '//eitri'
		expect_no_out 'alone: EITRI_CONFIG_DIR names it'
		expect_absent "$TH/xdgconf/eitri"
		expect_eq "$(cat "$TH/.config/eitri/init.lua")" 'print("mine")' "the settings under ~/.config"
		rm -rf "$TH/xdgconf"
	done
}

TESTS="$TESTS t_purge_ignores_a_relative_or_empty_xdg_config_home"
t_purge_ignores_a_relative_or_empty_xdg_config_home() {
	# A relative or empty XDG_CONFIG_HOME is ignored, as every XDG reader ignores one: the settings
	# are ~/.config/eitri, never a directory under wherever the installer happens to run.
	# Record and check the tried values after the loop: sh has no locals, so a helper reusing the
	# loop variable could silently make both rounds exercise another value.
	_pr_ran=
	for _pr_xdg in '' xdgconf; do
		populate
		mkdir -p "$T/cwd/xdgconf/eitri"
		echo keep >"$T/cwd/xdgconf/eitri/keep"
		inst --set "XDG_CONFIG_HOME=$_pr_xdg" -- --uninstall --purge
		expect_rc 0 "XDG_CONFIG_HOME='$_pr_xdg'"
		expect_out "removing $TH/.config/eitri"
		expect_absent "$TH/.config/eitri"
		expect_file "$T/cwd/xdgconf/eitri/keep"
		rm -rf "$T/cwd/xdgconf"
		_pr_ran="${_pr_ran}[${_pr_xdg}]"
	done
	expect_eq "$_pr_ran" "[][xdgconf]" "the XDG_CONFIG_HOME values tried"
}

TESTS="$TESTS t_purge_never_removes_eitri_config_dir"
t_purge_never_removes_eitri_config_dir() {
	# EITRI_CONFIG_DIR may name a directory shared with another program, so a purge leaves it and
	# says so -- unless it is the settings directory itself (or inside it), which goes as a whole.
	populate
	mkdir -p "$TH/shared"
	echo keep >"$TH/shared/init.lua"
	inst --set "EITRI_CONFIG_DIR=$TH/shared" -- --uninstall --purge
	expect_rc 0
	expect_file "$TH/shared/init.lua"
	expect_absent "$TH/.config/eitri"
	expect_out "left $TH/shared alone: EITRI_CONFIG_DIR names it"
	populate
	inst --set "EITRI_CONFIG_DIR=$TH/.config/eitri/" -- --uninstall --purge
	expect_rc 0
	expect_absent "$TH/.config/eitri"
	expect_no_out 'alone: EITRI_CONFIG_DIR names it'
}

TESTS="$TESTS t_purge_outside_home_still_asks_first"
t_purge_outside_home_still_asks_first() {
	# A settings directory outside HOME is refused as any other path outside it is: nothing is
	# removed until the user removes that one by hand.
	populate
	mkdir -p "$T/outside-conf/eitri"
	echo keep >"$T/outside-conf/eitri/init.lua"
	before=$(snap_but_lock "$TH")
	inst --set "XDG_CONFIG_HOME=$T/outside-conf" -- --uninstall --purge
	expect_fail "a settings directory outside HOME"
	expect_out "refusing to uninstall: these paths are not inside $TH"
	expect_out "'$T/outside-conf/eitri'"
	expect_out 'Nothing was removed'
	expect_file "$T/outside-conf/eitri/init.lua"
	expect_eq "$(snap_but_lock "$TH")" "$before" "the home after a refused purge"
	rm -rf "$T/outside-conf"
}

TESTS="$TESTS t_keep_message_names_the_directory_kept"
t_keep_message_names_the_directory_kept() {
	populate
	mkdir -p "$TH/xdgconf/eitri"
	inst --set "XDG_CONFIG_HOME=$TH/xdgconf" -- --uninstall
	expect_rc 0
	expect_out "kept your settings ($TH/xdgconf/eitri) and state ($TH/.local/state/eitri)"
	expect_dir "$TH/xdgconf/eitri"
	expect_dir "$TH/.config/eitri"
	inst --set "XDG_CONFIG_HOME=xdgconf" -- --uninstall
	expect_rc 0
	expect_out "kept your settings ($TH/.config/eitri) and state ($TH/.local/state/eitri)"
}

TESTS="$TESTS t_purge_outside_home_refused"
t_purge_outside_home_refused() {
	populate
	mkdir -p "$T/outside-state/eitri"
	echo keep >"$T/outside-state/eitri/keep"
	before=$(snap_but_lock "$TH")
	inst --set "XDG_STATE_HOME=$T/outside-state" -- --uninstall --purge
	expect_fail "a purge path outside HOME"
	expect_out "refusing to uninstall: these paths are not inside $TH"
	expect_out "'$T/outside-state/eitri'"
	expect_out 'Nothing was removed'
	expect_file "$T/outside-state/eitri/keep"
	expect_eq "$(snap_but_lock "$TH")" "$before" "the home after a refused purge"
	# A data directory outside HOME is refused the same way, purge or not.
	mkdir -p "$T/outside-data/eitri/nvim"
	inst --set "XDG_DATA_HOME=$T/outside-data" -- --uninstall
	expect_fail "an uninstall path outside HOME"
	expect_dir "$T/outside-data/eitri/nvim"
	expect_eq "$(snap_but_lock "$TH")" "$before" "the home after a refused uninstall"
	# And `..` cannot climb out.
	inst --set "XDG_STATE_HOME=$TH/../outside-state" -- --uninstall --purge
	expect_fail "a purge path with .."
	expect_file "$T/outside-state/eitri/keep"
}

TESTS="$TESTS t_uninstall_dry_run"
t_uninstall_dry_run() {
	populate
	before=$(snap "$TH")
	inst -- --uninstall --purge --dry-run
	expect_rc 0
	expect_eq "$(snap "$TH")" "$before" "the home after a dry-run uninstall"
	for p in "$TH/.local/lib/eitri" "$TH/.local/bin/eitri" "$(data_of)/applications/cn.huntergrey.eitri.desktop" \
		"$(data_of)/licenses/eitri" "$(data_of)/eitri/nvim" "$(data_of)/eitri/sidecar/aaaaaaa" \
		"$TH/.cache/eitri/sidecar-build" "$TH/.config/eitri" "$TH/.local/state/eitri"; do
		expect_out "would run: 'rm' '-rf' '--' '$p'"
	done
	expect_out 'dry run: nothing was changed'
}

# inst_nl VAR [INSTALLER-ARG]...: inst, with VAR set to "$TH/Documents", a newline, "$TH/y" --
# inside the wrapped command (helpers/env-nl), since run-in-env.sh refuses a newline in --set.
inst_nl() {
	_nl_v=$1
	shift
	set -- -- "$S/helpers/env-nl" "$_nl_v" "$TH/Documents" "$TH/y" "$NV_SH" "${INSTALLER_UNDER_TEST:-$INSTALLER}" "$@"
	if [ -n "$REAL_HOME" ]; then set -- --guard-real-home "$REAL_HOME" "$@"; fi
	"$WRAP" --home "$TH" --cwd "$T/cwd" --stubs "$S/stubs" \
		--set EITRI_INSTALL_TEST=1 \
		--set "EITRI_INSTALL_TEST_LIBDIRS=$S/libs/ok" \
		--set "EITRI_INSTALL_TEST_SYSTEM_RELEASE=$S/system/RELEASE" \
		--set "EITRI_INSTALL_TEST_OS_RELEASE=$S/osrel/ubuntu-24.04" \
		"$@" </dev/null >"$OUT" 2>&1
	RC=$?
	RUNS=$((RUNS + 1))
	cp "$OUT" "$T/out.$RUNS"
	case $RC in 96 | 97 | 98) fail "run-in-env.sh refused or its guard fired (exit $RC): $(cat "$OUT")" ;; esac
}

TESTS="$TESTS t_xdg_newline_refused"
t_xdg_newline_refused() {
	# A newline in an XDG_*_HOME must be refused like a newline in HOME: it would split uninstall's
	# newline-joined target list and could remove unrelated directories even with nothing installed.
	# Do not replace it with a default that disagrees with the Rust side, which takes the given value.
	# This includes XDG_CONFIG_HOME, which names the settings a purge removes.
	populate
	mkdir -p "$TH/Documents" "$TH/y"
	echo mine >"$TH/Documents/keep"
	before=$(snap "$TH")
	for v in XDG_DATA_HOME XDG_CACHE_HOME XDG_STATE_HOME XDG_CONFIG_HOME; do
		for m in '--uninstall --dry-run' --uninstall '--uninstall --purge'; do
			# shellcheck disable=SC2086 # $m is two words by design
			inst_nl "$v" $m
			expect_fail "$v holding a newline, $m"
			expect_out "eitri: error: $v contains a newline"
			expect_no_out 'removing'
			expect_eq "$(snap "$TH")" "$before" "the home after $m with a newline in $v"
		done
	done
	expect_file "$TH/Documents/keep"
	# An install refuses it the same way, before it reads or writes anything.
	serve 1.1.0 1.0.0
	inst_nl XDG_DATA_HOME --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	expect_fail "an install with a newline in XDG_DATA_HOME"
	expect_out 'eitri: error: XDG_DATA_HOME contains a newline'
	expect_eq "$(snap "$TH")" "$before" "the home after a refused install"
}

TESTS="$TESTS t_uninstall_through_foreign_symlink"
t_uninstall_through_foreign_symlink() {
	# A link at $XDG_DATA_HOME/eitri pointing to ~/.config must not let private-nvim removal reach
	# ~/.config/nvim, the user's own editor config. Leave a link target not named eitri alone and say
	# so, while uninstalling the rest.
	serve 1.0.0
	inst_net
	expect_rc 0
	mkdir -p "$TH/.config/nvim" "$TH/.config/sidecar/aaaaaaa"
	echo 'vim.o.number = true' >"$TH/.config/nvim/init.lua"
	echo mine >"$TH/.config/sidecar/aaaaaaa/keep"
	# The install made a real eitri directory (it holds the plugin); `ln -s` would put the link inside it.
	rm -rf "$(data_of)/eitri"
	ln -s "$TH/.config" "$(data_of)/eitri"
	inst -- --uninstall
	expect_rc 0
	expect_file "$TH/.config/nvim/init.lua"
	expect_file "$TH/.config/sidecar/aaaaaaa/keep"
	expect_out "$(data_of)/eitri leads to $TH/.config, not to a directory named eitri"
	if [ ! -L "$(data_of)/eitri" ]; then fail "the link itself was removed"; fi
	expect_absent "$TH/.local/lib/eitri"
	expect_absent "$TH/.local/bin/eitri"
	# A link that leads to a directory named eitri (Eitri's data moved elsewhere) is followed.
	rm "$(data_of)/eitri"
	mkdir -p "$T/elsewhere/eitri/nvim/0.11.4" "$T/elsewhere/eitri/sidecar/aaaaaaa"
	ln -s "$T/elsewhere/eitri" "$(data_of)/eitri"
	inst -- --uninstall
	expect_rc 0
	expect_absent "$T/elsewhere/eitri/nvim"
	expect_absent "$T/elsewhere/eitri/sidecar/aaaaaaa"
	expect_no_out 'not to a directory named eitri'
}

TESTS="$TESTS t_uninstall_symlinked_sidecar_root"
t_uninstall_symlinked_sidecar_root() {
	# A symlinked $NV_SIDECAR_ROOT must not expose unrelated directories to removal: a 7-hex name
	# behind a link may be a backup folder, not an Eitri sidecar. Exercise ~/backup/abcdef0/ as the
	# unrelated child, separately from the link check on $NV_DATA/eitri itself.
	serve 1.0.0
	inst_net
	expect_rc 0
	mkdir -p "$TH/backup/abcdef0"
	echo mine >"$TH/backup/abcdef0/keep"
	mkdir -p "$(data_of)/eitri"
	rm -rf "$(data_of)/eitri/sidecar"
	ln -s "$TH/backup" "$(data_of)/eitri/sidecar"
	inst -- --uninstall
	expect_rc 0
	expect_file "$TH/backup/abcdef0/keep"
	expect_out "$(data_of)/eitri/sidecar leads to $TH/backup, not to a directory named sidecar"
	if [ ! -L "$(data_of)/eitri/sidecar" ]; then fail "the link itself was removed"; fi
	expect_absent "$TH/.local/lib/eitri"
	# A link that leads to a directory actually named sidecar (moved to another disk, say) is
	# followed, the same rule $NV_DATA/eitri's own link already holds to.
	rm "$(data_of)/eitri/sidecar"
	mkdir -p "$T/elsewhere/sidecar/aaaaaaa"
	echo mine >"$T/elsewhere/sidecar/aaaaaaa/keep"
	ln -s "$T/elsewhere/sidecar" "$(data_of)/eitri/sidecar"
	inst -- --uninstall
	expect_rc 0
	expect_absent "$T/elsewhere/sidecar/aaaaaaa"
	expect_no_out 'not to a directory named sidecar'
}

TESTS="$TESTS t_uninstall_outside_cache_untouched"
t_uninstall_outside_cache_untouched() {
	# When XDG_CACHE_HOME is outside HOME, uninstall refuses and must remove nothing, including in
	# exit cleanup. Once its download/unpack directories are removed by hand as advised, uninstall
	# can proceed.
	populate
	oc=$T/outside-cache
	for p in download unpack sidecar-build; do
		mkdir -p "$oc/eitri/$p"
		echo keep >"$oc/eitri/$p/keep"
	done
	inst --set "XDG_CACHE_HOME=$oc" -- --uninstall
	expect_fail "an uninstall with Eitri's cache outside HOME"
	expect_out 'Nothing was removed'
	expect_out "'$oc/eitri/download'"
	for p in download unpack sidecar-build; do expect_file "$oc/eitri/$p/keep"; done
	expect_absent "$oc/eitri/lock"
	expect_dir "$TH/.local/lib/eitri"
	rm -rf "$oc/eitri"
	inst --set "XDG_CACHE_HOME=$oc" -- --uninstall
	expect_rc 0
	expect_out 'Eitri is uninstalled'
	expect_absent "$TH/.local/lib/eitri"
	expect_absent "$oc/eitri"
	expect_dir "$oc"
}
