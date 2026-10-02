# The GNOME Shell extension `eitri split` uses: the four files installed to
# $XDG_DATA_HOME/gnome-shell/extensions/eitri@huntergrey.cn, and never anything that enables them.
# Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

EXT_ID='eitri@huntergrey.cn'
EXT_FILES='direction.js extension.js metadata.json policy.js'

# ext_dir: where the installer puts the extension.
ext_dir() { printf '%s\n' "$(data_of)/gnome-shell/extensions/$EXT_ID"; }

# ext_tmps: any temporary name left under the installed extension.
ext_tmps() { find "$(data_of)/gnome-shell" -name '*.tmp.*' 2>/dev/null; }

TESTS="$TESTS t_installer_extension_list_is_the_shipped_files"
t_installer_extension_list_is_the_shipped_files() {
	listed=$(sed -n "s/^NV_EXT_FILES='\\(.*\\)'\$/\\1/p" "$REAL_INSTALLER" | tr ' ' '\n' | LC_ALL=C sort)
	# shellcheck disable=SC2086 # the list is deliberately several words
	expect_eq "$listed" "$(printf '%s\n' $EXT_FILES)" "install.sh's NV_EXT_FILES"
	listed=$(sed -n "s/^NV_EXT_ID='\\(.*\\)'\$/\\1/p" "$REAL_INSTALLER")
	expect_eq "$listed" "$EXT_ID" "install.sh's NV_EXT_ID"
	for f in $EXT_FILES; do
		if [ ! -f "$PKG/../gnome-extension/$f" ]; then fail "gnome-extension/$f is listed and not committed"; fi
	done
}

TESTS="$TESTS t_extension_installed_and_nothing_enables_it"
t_extension_installed_and_nothing_enables_it() {
	serve 1.0.0
	inst_net
	expect_rc 0
	for f in $EXT_FILES; do
		expect_file "$(ext_dir)/$f"
		if ! cmp -s "$PKG/../gnome-extension/$f" "$(ext_dir)/$f"; then
			fail "the installed extension file $f is not gnome-extension/$f"
		fi
	done
	# Only the four files: testing.js and test/ never ride along.
	expect_eq "$(find "$(ext_dir)" | wc -l | tr -d ' ')" 5 "the entries under the extension directory"
	expect_absent "$(ext_dir)/testing.js"
	expect_absent "$(ext_dir)/test"
	expect_eq "$(ext_tmps)" "" "temporary names left behind"
	# Enabling is the user's step: the installer writes no dconf, gsettings or enabled-extensions state.
	expect_absent "$TH/.config/dconf"
	expect_no_out 'gnome-extensions'
}

TESTS="$TESTS t_extension_follows_xdg_data_home"
t_extension_follows_xdg_data_home() {
	use_home "$T/home-xdg"
	serve 1.0.0
	inst_net --set "XDG_DATA_HOME=$TH/xdg data" --
	expect_rc 0
	expect_file "$TH/xdg data/gnome-shell/extensions/$EXT_ID/extension.js"
	expect_absent "$TH/.local/share/gnome-shell"
}

TESTS="$TESTS t_tarball_missing_an_extension_file_is_refused"
t_tarball_missing_an_extension_file_is_refused() {
	for what in share/gnome-shell/extensions/$EXT_ID/policy.js share/gnome-shell/extensions/$EXT_ID/metadata.json; do
		serve 1.0.0
		d=$(served 1.0.0)
		top=eitri-1.0.0-x86_64-linux
		rm -rf "$T/bad"
		mkdir -p "$T/bad"
		tar -C "$T/bad" -xzf "$d/$top.tar.gz"
		rm "$T/bad/$top/$what"
		tar -C "$T/bad" -czf "$d/$top.tar.gz" "$top"
		resign "$d"
		relatest 1.0.0
		before=$(snap "$TH")
		inst_net
		expect_fail "a tarball without $what"
		expect_out "has no $what"
		expect_eq "$(snap "$TH")" "$before" "the home after the refusal ($what)"
	done
}

TESTS="$TESTS t_rerun_finishes_an_install_that_lost_an_extension_file"
t_rerun_finishes_an_install_that_lost_an_extension_file() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	rm "$(ext_dir)/direction.js"
	inst_net
	expect_rc 0
	expect_no_out 'is up to date'
	expect_out 'installed Eitri 1.0.0'
	expect_file "$(ext_dir)/direction.js"
}

TESTS="$TESTS t_upgrade_replaces_the_extension_files"
t_upgrade_replaces_the_extension_files() {
	serve 1.0.0
	inst_net
	expect_rc 0
	printf 'stale\n' >"$(ext_dir)/extension.js"
	serve 1.1.0 1.0.0
	inst_net
	expect_rc 0
	if ! cmp -s "$PKG/../gnome-extension/extension.js" "$(ext_dir)/extension.js"; then
		fail "the upgrade did not put the extension file back"
	fi
	expect_eq "$(ext_tmps)" "" "temporary names left behind"
}

TESTS="$TESTS t_symlinked_extension_directory_is_left_alone"
t_symlinked_extension_directory_is_left_alone() {
	# A link at gnome-shell/extensions/eitri@huntergrey.cn is the user's own checkout of the extension: nothing
	# is written through it, the rest of the install finishes, and --uninstall leaves the link and its target.
	mkdir -p "$TH/.local/share/gnome-shell/extensions" "$TH/my-extension"
	printf 'mine\n' >"$TH/my-extension/extension.js"
	ln -s "$TH/my-extension" "$(ext_dir)"
	mine=$(snap "$TH/my-extension")
	serve 1.0.0
	inst_net
	expect_rc 0
	expect_out "$EXT_ID is a symlink"
	expect_eq "$(snap "$TH/my-extension")" "$mine" "the user's own extension checkout after an install"
	expect_file "$(data_of)/applications/cn.huntergrey.eitri.Panel.desktop"
	plant_sidecar aaaaaaa
	inst_net
	expect_rc 0
	expect_out 'Eitri 1.0.0 is up to date'
	inst -- --uninstall
	expect_rc 0
	if [ ! -L "$(ext_dir)" ]; then fail "--uninstall removed the user's link"; fi
	expect_eq "$(snap "$TH/my-extension")" "$mine" "the user's own extension checkout after an uninstall"
	expect_out "$EXT_ID is a symlink, so it was left alone"
	expect_absent "$(data_of)/applications/cn.huntergrey.eitri.Panel.desktop"
}

TESTS="$TESTS t_uninstall_removes_the_extension_and_tidies_empty_parents"
t_uninstall_removes_the_extension_and_tidies_empty_parents() {
	serve 1.0.0
	inst_net
	expect_rc 0
	inst -- --uninstall --dry-run
	expect_rc 0
	expect_out "would run: 'rm' '-rf' '--' '$(ext_dir)'"
	expect_file "$(ext_dir)/extension.js"
	inst -- --uninstall
	expect_rc 0
	expect_out 'Eitri is uninstalled'
	expect_absent "$(ext_dir)"
	expect_absent "$(data_of)/gnome-shell"
}

TESTS="$TESTS t_uninstall_keeps_another_extension_beside_ours"
t_uninstall_keeps_another_extension_beside_ours() {
	serve 1.0.0
	inst_net
	expect_rc 0
	mkdir -p "$(data_of)/gnome-shell/extensions/other@example.org"
	printf '{}\n' >"$(data_of)/gnome-shell/extensions/other@example.org/metadata.json"
	inst -- --uninstall
	expect_rc 0
	expect_file "$(data_of)/gnome-shell/extensions/other@example.org/metadata.json"
	expect_absent "$(ext_dir)"
}

TESTS="$TESTS t_dry_run_install_names_the_extension_files"
t_dry_run_install_names_the_extension_files() {
	serve 1.0.0
	before=$(snap "$TH")
	inst_net --dry-run
	expect_rc 0
	for f in $EXT_FILES; do expect_out "would write $TH/.local/share/gnome-shell/extensions/$EXT_ID/$f"; done
	expect_eq "$(snap "$TH")" "$before" "the home after a dry run"
}
