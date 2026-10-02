# The companion panel's pieces: its hidden desktop entry (`eitri panel`) and the nvim plugin that adds
# :EitriPanel, installed to $XDG_DATA_HOME/eitri/eitri.nvim. Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

# plugin_files: the committed plugin's files, relative to nvim/eitri.nvim, in C order.
plugin_files() { (cd "$PKG/../nvim/eitri.nvim" && find . -type f | sed 's#^\./##' | LC_ALL=C sort); }

# plugin_tmps: any temporary name left under the installed plugin.
plugin_tmps() { find "$(data_of)/eitri/eitri.nvim" -name '*.tmp.*' 2>/dev/null; }

TESTS="$TESTS t_installer_plugin_list_is_the_committed_plugin"
t_installer_plugin_list_is_the_committed_plugin() {
	listed=$(sed -n "s/^NV_PLUGIN_FILES='\\(.*\\)'\$/\\1/p" "$REAL_INSTALLER" | tr ' ' '\n' | LC_ALL=C sort)
	expect_eq "$listed" "$(plugin_files)" "install.sh's NV_PLUGIN_FILES against nvim/eitri.nvim"
	listed=$(sed -n "s/^NV_DESKTOP='\\(.*\\)'\$/\\1/p" "$REAL_INSTALLER" | tr ' ' '\n' | LC_ALL=C sort)
	expect_eq "$listed" "$NEW_DESKTOPS" "install.sh's NV_DESKTOP"
}

TESTS="$TESTS t_panel_entry_and_plugin_installed"
t_panel_entry_and_plugin_installed() {
	serve 1.0.0
	inst_net
	expect_rc 0
	p=$(data_of)/applications/cn.huntergrey.eitri.Panel.desktop
	expect_file "$p"
	# The panel subcommand comes first, and the launcher is the absolute one, like the main entry's.
	expect_eq "$(grep '^Exec=' "$p")" "Exec=\"$TH/.local/bin/eitri\" panel --quiet %f" "the panel entry's Exec line"
	if [ "$(grep -v '^Exec=' "$p")" != "$(grep -v '^Exec=' "$PKG/cn.huntergrey.eitri.Panel.desktop")" ]; then
		fail "the panel entry differs from the tarball's in more than Exec"
	fi
	if ! grep -Fqx 'NoDisplay=true' "$p"; then fail "the panel entry is not hidden"; fi
	# The main entry is unchanged.
	expect_eq "$(grep '^Exec=' "$(data_of)/applications/cn.huntergrey.eitri.desktop")" \
		"Exec=\"$TH/.local/bin/eitri\" --quiet %f" "the main entry's Exec line"
	for f in $(plugin_files); do
		expect_file "$(data_of)/eitri/eitri.nvim/$f"
		if ! cmp -s "$PKG/../nvim/eitri.nvim/$f" "$(data_of)/eitri/eitri.nvim/$f"; then
			fail "the installed plugin file $f is not nvim/eitri.nvim/$f"
		fi
	done
	expect_eq "$(find "$(data_of)/eitri/eitri.nvim" -type f | wc -l | tr -d ' ')" 3 "the files under the plugin directory"
	# Not where the private nvim versions are scanned and removed.
	expect_absent "$(data_of)/eitri/nvim"
	expect_eq "$(plugin_tmps)" "" "temporary names left behind"
}

TESTS="$TESTS t_plugin_follows_xdg_data_home"
t_plugin_follows_xdg_data_home() {
	use_home "$T/home-xdg"
	serve 1.0.0
	inst_net --set "XDG_DATA_HOME=$TH/xdg data" --
	expect_rc 0
	expect_file "$TH/xdg data/eitri/eitri.nvim/plugin/eitri.lua"
	expect_file "$TH/xdg data/applications/cn.huntergrey.eitri.Panel.desktop"
	expect_absent "$TH/.local/share/eitri"
}

TESTS="$TESTS t_tarball_missing_the_panel_pieces_is_refused"
t_tarball_missing_the_panel_pieces_is_refused() {
	for what in share/applications/cn.huntergrey.eitri.Panel.desktop share/eitri/eitri.nvim/lua/eitri/init.lua; do
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

TESTS="$TESTS t_rerun_finishes_an_install_that_lost_a_panel_piece"
t_rerun_finishes_an_install_that_lost_a_panel_piece() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	for f in "$(data_of)/applications/cn.huntergrey.eitri.Panel.desktop" "$(data_of)/eitri/eitri.nvim/lua/eitri/init.lua"; do
		rm "$f"
		inst_net
		expect_rc 0
		expect_no_out 'is up to date'
		expect_out 'installed Eitri 1.0.0'
		expect_file "$f"
	done
}

TESTS="$TESTS t_upgrade_replaces_the_plugin_files"
t_upgrade_replaces_the_plugin_files() {
	serve 1.0.0
	inst_net
	expect_rc 0
	printf 'stale\n' >"$(data_of)/eitri/eitri.nvim/plugin/eitri.lua"
	serve 1.1.0 1.0.0
	inst_net
	expect_rc 0
	if ! cmp -s "$PKG/../nvim/eitri.nvim/plugin/eitri.lua" "$(data_of)/eitri/eitri.nvim/plugin/eitri.lua"; then
		fail "the upgrade did not put the plugin file back"
	fi
	expect_eq "$(plugin_tmps)" "" "temporary names left behind"
}

TESTS="$TESTS t_symlinked_plugin_directory_is_left_alone"
t_symlinked_plugin_directory_is_left_alone() {
	# A link at eitri/eitri.nvim is the user's own checkout of the plugin: nothing is written through it,
	# the rest of the install finishes, and --uninstall leaves the link and what it leads to.
	mkdir -p "$TH/.local/share/eitri" "$TH/my-checkout/plugin"
	printf 'mine\n' >"$TH/my-checkout/plugin/eitri.lua"
	ln -s "$TH/my-checkout" "$TH/.local/share/eitri/eitri.nvim"
	mine=$(snap "$TH/my-checkout")
	serve 1.0.0
	inst_net
	expect_rc 0
	expect_out 'eitri.nvim is a symlink'
	expect_eq "$(snap "$TH/my-checkout")" "$mine" "the user's own plugin checkout after an install"
	expect_file "$(data_of)/applications/cn.huntergrey.eitri.Panel.desktop"
	plant_sidecar aaaaaaa
	inst_net
	expect_rc 0
	expect_out 'Eitri 1.0.0 is up to date'
	inst -- --uninstall
	expect_rc 0
	if [ ! -L "$(data_of)/eitri/eitri.nvim" ]; then fail "--uninstall removed the user's link"; fi
	expect_eq "$(snap "$TH/my-checkout")" "$mine" "the user's own plugin checkout after an uninstall"
	expect_absent "$(data_of)/applications/cn.huntergrey.eitri.Panel.desktop"
}

TESTS="$TESTS t_uninstall_removes_the_panel_entry_and_the_plugin"
t_uninstall_removes_the_panel_entry_and_the_plugin() {
	serve 1.0.0
	inst_net
	expect_rc 0
	inst -- --uninstall --dry-run
	expect_rc 0
	expect_out "would run: 'rm' '-rf' '--' '$(data_of)/applications/cn.huntergrey.eitri.Panel.desktop'"
	expect_out "would run: 'rm' '-rf' '--' '$(data_of)/eitri/eitri.nvim'"
	expect_file "$(data_of)/eitri/eitri.nvim/plugin/eitri.lua"
	inst -- --uninstall
	expect_rc 0
	expect_out 'Eitri is uninstalled'
	expect_absent "$(data_of)/applications/cn.huntergrey.eitri.Panel.desktop"
	expect_absent "$(data_of)/eitri/eitri.nvim"
	expect_absent "$(data_of)/applications/cn.huntergrey.eitri.desktop"
}

TESTS="$TESTS t_uninstall_leaves_a_neighbour_of_the_plugin_alone"
t_uninstall_leaves_a_neighbour_of_the_plugin_alone() {
	# Only the plugin's own directory goes, not a sibling the user keeps beside it under eitri/.
	serve 1.0.0
	inst_net
	expect_rc 0
	mkdir -p "$(data_of)/eitri/mine"
	printf 'keep\n' >"$(data_of)/eitri/mine/file"
	inst -- --uninstall
	expect_rc 0
	expect_file "$(data_of)/eitri/mine/file"
	expect_absent "$(data_of)/eitri/eitri.nvim"
}

TESTS="$TESTS t_dry_run_install_names_the_panel_pieces"
t_dry_run_install_names_the_panel_pieces() {
	serve 1.0.0
	before=$(snap "$TH")
	inst_net --dry-run
	expect_rc 0
	expect_out "would write $TH/.local/share/applications/cn.huntergrey.eitri.Panel.desktop with Exec=\"$TH/.local/bin/eitri\" panel --quiet %f"
	for f in $(plugin_files); do expect_out "would write $TH/.local/share/eitri/eitri.nvim/$f"; done
	expect_eq "$(snap "$TH")" "$before" "the home after a dry run"
}
