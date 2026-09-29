# Uninstall and purge (spec §6.6). Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

# populate: an install with everything §6.6 names, plus what it must keep.
populate() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	plant_sidecar eeeeeee
	mkdir -p "$(data_of)/neovibe/nvim/0.11.4/bin"
	printf '#!/bin/sh\n' >"$(data_of)/neovibe/nvim/0.11.4/bin/nvim"
	mkdir -p "$TH/.config/neovibe" "$TH/.local/state/neovibe/layout" "$TH/.cache/neovibe/sidecar-build/aaaaaaa"
	echo 'print("mine")' >"$TH/.config/neovibe/init.lua"
	echo '{}' >"$TH/.local/state/neovibe/layout/0123456789abcdef.json"
	echo junk >"$TH/.cache/neovibe/sidecar-build/aaaaaaa/junk"
}

KEPT_TREE='./.cache d
./.config d
./.config/neovibe d
./.config/neovibe/init.lua f
./.local d
./.local/bin d
./.local/lib d
./.local/share d
./.local/share/applications d
./.local/state d
./.local/state/neovibe d
./.local/state/neovibe/layout d
./.local/state/neovibe/layout/0123456789abcdef.json f'

TESTS="$TESTS t_uninstall_exact"
t_uninstall_exact() {
	populate
	inst -- --uninstall
	expect_rc 0
	expect_eq "$(tree "$TH")" "$KEPT_TREE" "the tree after --uninstall"
	expect_out 'neovibe is uninstalled'
	expect_out "kept your settings ($TH/.config/neovibe) and state ($TH/.local/state/neovibe)"
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
	printf '#!/bin/sh\necho mine\n' >"$TH/.local/bin/neovibe"
	mine=$(sha256sum <"$TH/.local/bin/neovibe")
	inst -- --uninstall
	expect_rc 0
	expect_out "$TH/.local/bin/neovibe was left alone: it was not installed by this installer"
	expect_eq "$(sha256sum <"$TH/.local/bin/neovibe")" "$mine" "the foreign launcher"
	expect_absent "$TH/.local/lib/neovibe"
	# Nor is a symlink removed.
	rm "$TH/.local/bin/neovibe"
	ln -s "$TH/.local/bin/nvim" "$TH/.local/bin/neovibe"
	inst -- --uninstall
	expect_rc 0
	if [ ! -L "$TH/.local/bin/neovibe" ]; then fail "a symlinked launcher was removed"; fi
}

TESTS="$TESTS t_uninstall_keeps_system_rev"
t_uninstall_keeps_system_rev() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	plant_sidecar ddddddd
	printf 'NEOVIBE_VERSION=1.0.0\nVERDANDI_REV=ddddddd000000000000000000000000000000000\n' >"$S/system/RELEASE"
	inst -- --uninstall
	rm -f "$S/system/RELEASE"
	expect_rc 0
	expect_out 'keeping the sidecar for verdandi ddddddd'
	expect_absent "$(data_of)/neovibe/sidecar/aaaaaaa"
	expect_exec "$(data_of)/neovibe/sidecar/ddddddd/verdandi-claude-sidecar"
	expect_file "$(data_of)/neovibe/sidecar/ddddddd/BUILD"
}

TESTS="$TESTS t_unreadable_system_release_stops"
t_unreadable_system_release_stops() {
	# A system RELEASE that exists but cannot be read: which sidecar its install uses cannot be
	# told, so none may be removed on a guess. Under bash outside POSIX mode (this harness's bash
	# half runs `bash install.sh`), errexit is off inside $(...), so the read failure was once lost
	# there and the uninstall removed that install's sidecar.
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	plant_sidecar ddddddd
	printf 'NEOVIBE_VERSION=1.0.0\nVERDANDI_REV=ddddddd000000000000000000000000000000000\n' >"$S/system/RELEASE"
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
	expect_out 'reinstall or remove the neovibe package that owns it'
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
	# A system RELEASE that can be read but names no valid rev is the unreadable case again: which
	# sidecar that install uses cannot be told, so none is removed on a guess. A quoted value once
	# read as "names no sidecar", and --uninstall removed ddddddd.
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	plant_sidecar ddddddd
	refused_with_system_release 'NEOVIBE_VERSION=1.0.0\nVERDANDI_REV="ddddddd000000000000000000000000000000000"\n'
	refused_with_system_release 'NEOVIBE_VERSION=1.0.0\nVERDANDI_REV=\n'
	refused_with_system_release 'NEOVIBE_VERSION=1.0.0\n'
	refused_with_system_release 'VERDANDI_REV=DDDDDDD000000000000000000000000000000000\n'
	expect_exec "$(data_of)/neovibe/sidecar/ddddddd/verdandi-claude-sidecar"
	expect_exec "$(data_of)/neovibe/sidecar/aaaaaaa/verdandi-claude-sidecar"
}

TESTS="$TESTS t_purge_exact"
t_purge_exact() {
	populate
	mkdir -p "$TH/other config" "$TH/xdgconf/neovibe"
	echo keep >"$TH/other config/init.lua"
	echo keep >"$TH/xdgconf/neovibe/keep"
	inst --set "NEOVIBE_CONFIG_DIR=$TH/other config" --set "XDG_CONFIG_HOME=$TH/xdgconf" -- --uninstall --purge
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
./other config/init.lua f
./xdgconf d
./xdgconf/neovibe d
./xdgconf/neovibe/keep f" "the tree after --uninstall --purge"
	expect_no_out 'kept your settings'
}

TESTS="$TESTS t_purge_outside_home_refused"
t_purge_outside_home_refused() {
	populate
	mkdir -p "$T/outside-state/neovibe"
	echo keep >"$T/outside-state/neovibe/keep"
	before=$(snap_but_lock "$TH")
	inst --set "XDG_STATE_HOME=$T/outside-state" -- --uninstall --purge
	expect_fail "a purge path outside HOME"
	expect_out "refusing to uninstall: these paths are not inside $TH"
	expect_out "'$T/outside-state/neovibe'"
	expect_out 'Nothing was removed'
	expect_file "$T/outside-state/neovibe/keep"
	expect_eq "$(snap_but_lock "$TH")" "$before" "the home after a refused purge"
	# A data directory outside HOME is refused the same way, purge or not.
	mkdir -p "$T/outside-data/neovibe/nvim"
	inst --set "XDG_DATA_HOME=$T/outside-data" -- --uninstall
	expect_fail "an uninstall path outside HOME"
	expect_dir "$T/outside-data/neovibe/nvim"
	expect_eq "$(snap_but_lock "$TH")" "$before" "the home after a refused uninstall"
	# And `..` cannot climb out.
	inst --set "XDG_STATE_HOME=$TH/../outside-state" -- --uninstall --purge
	expect_fail "a purge path with .."
	expect_file "$T/outside-state/neovibe/keep"
}

TESTS="$TESTS t_uninstall_dry_run"
t_uninstall_dry_run() {
	populate
	before=$(snap "$TH")
	inst -- --uninstall --purge --dry-run
	expect_rc 0
	expect_eq "$(snap "$TH")" "$before" "the home after a dry-run uninstall"
	for p in "$TH/.local/lib/neovibe" "$TH/.local/bin/neovibe" "$(data_of)/applications/neovibe.desktop" \
		"$(data_of)/licenses/neovibe" "$(data_of)/neovibe/nvim" "$(data_of)/neovibe/sidecar/aaaaaaa" \
		"$TH/.cache/neovibe/sidecar-build" "$TH/.config/neovibe" "$TH/.local/state/neovibe"; do
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
		--set NEOVIBE_INSTALL_TEST=1 \
		--set "NEOVIBE_INSTALL_TEST_LIBDIRS=$S/libs/ok" \
		--set "NEOVIBE_INSTALL_TEST_SYSTEM_RELEASE=$S/system/RELEASE" \
		--set "NEOVIBE_INSTALL_TEST_OS_RELEASE=$S/osrel/ubuntu-24.04" \
		"$@" </dev/null >"$OUT" 2>&1
	RC=$?
	RUNS=$((RUNS + 1))
	cp "$OUT" "$T/out.$RUNS"
	case $RC in 96 | 97 | 98) fail "run-in-env.sh refused or its guard fired (exit $RC): $(cat "$OUT")" ;; esac
}

TESTS="$TESTS t_xdg_newline_refused"
t_xdg_newline_refused() {
	# An XDG_DATA_HOME/XDG_CACHE_HOME/XDG_STATE_HOME holding a newline once split the uninstall's
	# newline-joined target list: XDG_DATA_HOME="$HOME/Documents<NL>$HOME/y" removed ~/Documents,
	# with nothing of neovibe installed at all. It is refused as a newline in HOME is -- never
	# replaced by the default, where the Rust side, which takes the value as given, would disagree.
	populate
	mkdir -p "$TH/Documents" "$TH/y"
	echo mine >"$TH/Documents/keep"
	before=$(snap "$TH")
	for v in XDG_DATA_HOME XDG_CACHE_HOME XDG_STATE_HOME; do
		for m in '--uninstall --dry-run' --uninstall '--uninstall --purge'; do
			# shellcheck disable=SC2086 # $m is two words by design
			inst_nl "$v" $m
			expect_fail "$v holding a newline, $m"
			expect_out "neovibe: error: $v contains a newline"
			expect_no_out 'removing'
			expect_eq "$(snap "$TH")" "$before" "the home after $m with a newline in $v"
		done
	done
	expect_file "$TH/Documents/keep"
	# An install refuses it the same way, before it reads or writes anything.
	serve 1.1.0 1.0.0
	inst_nl XDG_DATA_HOME --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	expect_fail "an install with a newline in XDG_DATA_HOME"
	expect_out 'neovibe: error: XDG_DATA_HOME contains a newline'
	expect_eq "$(snap "$TH")" "$before" "the home after a refused install"
}

TESTS="$TESTS t_uninstall_through_foreign_symlink"
t_uninstall_through_foreign_symlink() {
	# $XDG_DATA_HOME/neovibe made a link to ~/.config: removing its nvim once removed ~/.config/nvim,
	# the user's own editor config (spec §6.6: uninstall never touches anything named nvim outside
	# neovibe's own directory). What is under a link that does not lead to a directory named
	# neovibe is left alone, and said so; the rest is uninstalled.
	serve 1.0.0
	inst_net
	expect_rc 0
	mkdir -p "$TH/.config/nvim" "$TH/.config/sidecar/aaaaaaa"
	echo 'vim.o.number = true' >"$TH/.config/nvim/init.lua"
	echo mine >"$TH/.config/sidecar/aaaaaaa/keep"
	ln -s "$TH/.config" "$(data_of)/neovibe"
	inst -- --uninstall
	expect_rc 0
	expect_file "$TH/.config/nvim/init.lua"
	expect_file "$TH/.config/sidecar/aaaaaaa/keep"
	expect_out "$(data_of)/neovibe leads to $TH/.config, not to a directory named neovibe"
	if [ ! -L "$(data_of)/neovibe" ]; then fail "the link itself was removed"; fi
	expect_absent "$TH/.local/lib/neovibe"
	expect_absent "$TH/.local/bin/neovibe"
	# A link that leads to a directory named neovibe (neovibe's data moved elsewhere) is followed.
	rm "$(data_of)/neovibe"
	mkdir -p "$T/elsewhere/neovibe/nvim/0.11.4" "$T/elsewhere/neovibe/sidecar/aaaaaaa"
	ln -s "$T/elsewhere/neovibe" "$(data_of)/neovibe"
	inst -- --uninstall
	expect_rc 0
	expect_absent "$T/elsewhere/neovibe/nvim"
	expect_absent "$T/elsewhere/neovibe/sidecar/aaaaaaa"
	expect_no_out 'not to a directory named neovibe'
}

TESTS="$TESTS t_uninstall_symlinked_sidecar_root"
t_uninstall_symlinked_sidecar_root() {
	# M3 (v1-dist whole-branch review, 2026-09-28): unlike $NV_DATA/neovibe itself (the test just
	# above), $NV_SIDECAR_ROOT ($NV_DATA/neovibe/sidecar) had no link check of its own at all -- a
	# symlinked sidecar root, planted by the user in their own $XDG_DATA_HOME, was followed with no
	# check, so a 7-hex-named directory BEHIND the link (a user's own, unrelated directory that
	# merely happens to be named like a short git sha, e.g. a backup folder) was removed as if it
	# were one of neovibe's own sidecars. Reproduced (installer-codex-2, the verifier's
	# t_vprobe_symlinked_sidecar_root_uninstall): ~/backup/abcdef0/ was deleted.
	serve 1.0.0
	inst_net
	expect_rc 0
	mkdir -p "$TH/backup/abcdef0"
	echo mine >"$TH/backup/abcdef0/keep"
	mkdir -p "$(data_of)/neovibe"
	rm -rf "$(data_of)/neovibe/sidecar"
	ln -s "$TH/backup" "$(data_of)/neovibe/sidecar"
	inst -- --uninstall
	expect_rc 0
	expect_file "$TH/backup/abcdef0/keep"
	expect_out "$(data_of)/neovibe/sidecar leads to $TH/backup, not to a directory named sidecar"
	if [ ! -L "$(data_of)/neovibe/sidecar" ]; then fail "the link itself was removed"; fi
	expect_absent "$TH/.local/lib/neovibe"
	# A link that leads to a directory actually named sidecar (moved to another disk, say) is
	# followed, the same rule $NV_DATA/neovibe's own link already holds to.
	rm "$(data_of)/neovibe/sidecar"
	mkdir -p "$T/elsewhere/sidecar/aaaaaaa"
	echo mine >"$T/elsewhere/sidecar/aaaaaaa/keep"
	ln -s "$T/elsewhere/sidecar" "$(data_of)/neovibe/sidecar"
	inst -- --uninstall
	expect_rc 0
	expect_absent "$T/elsewhere/sidecar/aaaaaaa"
	expect_no_out 'not to a directory named sidecar'
}

TESTS="$TESTS t_uninstall_outside_cache_untouched"
t_uninstall_outside_cache_untouched() {
	# With XDG_CACHE_HOME outside HOME and neovibe's cache directories there, uninstall refuses and
	# says nothing was removed -- and nothing is: its exit cleanup once removed the download and
	# unpack directories it had just refused to touch. Once they are gone (removed by hand, as the
	# message says), the uninstall goes through.
	populate
	oc=$T/outside-cache
	for p in download unpack sidecar-build; do
		mkdir -p "$oc/neovibe/$p"
		echo keep >"$oc/neovibe/$p/keep"
	done
	inst --set "XDG_CACHE_HOME=$oc" -- --uninstall
	expect_fail "an uninstall with neovibe's cache outside HOME"
	expect_out 'Nothing was removed'
	expect_out "'$oc/neovibe/download'"
	for p in download unpack sidecar-build; do expect_file "$oc/neovibe/$p/keep"; done
	expect_absent "$oc/neovibe/lock"
	expect_dir "$TH/.local/lib/neovibe"
	rm -rf "$oc/neovibe"
	inst --set "XDG_CACHE_HOME=$oc" -- --uninstall
	expect_rc 0
	expect_out 'neovibe is uninstalled'
	expect_absent "$TH/.local/lib/neovibe"
	expect_absent "$oc/neovibe"
	expect_dir "$oc"
}
