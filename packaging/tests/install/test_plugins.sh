# The plugin directories a release ships (today the Markdown preview): in the tarball at lib/eitri/plugins/<name>,
# installed with the binaries at $NV_LIB/plugins/<name> and swapped with them; a from-source install stages them from
# the tree it built. Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

PLUGIN_NAMES='markdown'

# plugin_dir: where the installer puts the Markdown preview plugin directory.
plugin_dir() { printf '%s\n' "$TH/.local/lib/eitri/plugins/markdown"; }

TESTS="$TESTS t_installer_shipped_plugins_list"
t_installer_shipped_plugins_list() {
	listed=$(sed -n "s/^NV_SHIPPED_PLUGINS='\\(.*\\)'\$/\\1/p" "$REAL_INSTALLER")
	expect_eq "$listed" "$PLUGIN_NAMES" "install.sh's NV_SHIPPED_PLUGINS"
}

TESTS="$TESTS t_plugin_directory_installed_with_the_binaries"
t_plugin_directory_installed_with_the_binaries() {
	serve 1.0.0
	inst_net
	expect_rc 0
	for f in eitri-plugin.toml web/dist/index.html web/dist/preview.js web/dist/katex/fonts/KaTeX_Main-Regular.woff2; do
		expect_file "$(plugin_dir)/$f"
	done
	if ! grep -q 'preview 1.0.0' "$(plugin_dir)/web/dist/index.html"; then
		fail "the installed page is not the 1.0.0 tarball's: $(cat "$(plugin_dir)/web/dist/index.html")"
	fi
	# The user's data directory is the user's own: an install never writes a plugin there.
	expect_absent "$(data_of)/eitri/plugins"
}

TESTS="$TESTS t_upgrade_replaces_the_plugin_directory_and_keeps_the_users_copy"
t_upgrade_replaces_the_plugin_directory_and_keeps_the_users_copy() {
	serve 1.0.0
	inst_net
	expect_rc 0
	printf 'stale\n' >"$(plugin_dir)/web/dist/stale.js"
	mkdir -p "$(data_of)/eitri/plugins/markdown"
	printf 'id = "markdown"\n# my own copy\n' >"$(data_of)/eitri/plugins/markdown/eitri-plugin.toml"
	mine=$(snap "$(data_of)/eitri/plugins")
	serve 1.1.0 1.0.0
	inst_net
	expect_rc 0
	if ! grep -q 'preview 1.1.0' "$(plugin_dir)/web/dist/index.html"; then
		fail "the upgrade did not bring 1.1.0's page: $(cat "$(plugin_dir)/web/dist/index.html")"
	fi
	expect_absent "$(plugin_dir)/web/dist/stale.js"
	expect_eq "$(snap "$(data_of)/eitri/plugins")" "$mine" "the user's own plugin copy after the upgrade"
}

TESTS="$TESTS t_tarball_without_the_plugin_directory_is_refused"
t_tarball_without_the_plugin_directory_is_refused() {
	serve 1.0.0
	d=$(served 1.0.0)
	top=eitri-1.0.0-x86_64-linux
	rm -rf "$T/bad"
	mkdir -p "$T/bad"
	tar -C "$T/bad" -xzf "$d/$top.tar.gz"
	rm "$T/bad/$top/lib/eitri/plugins/markdown/eitri-plugin.toml"
	tar -C "$T/bad" -czf "$d/$top.tar.gz" "$top"
	resign "$d"
	relatest 1.0.0
	before=$(snap "$TH")
	inst_net
	expect_fail "a tarball without the plugin's manifest"
	expect_out "has no lib/eitri/plugins/markdown/eitri-plugin.toml"
	expect_eq "$(snap "$TH")" "$before" "the home after the refusal"
}

TESTS="$TESTS t_release_before_the_shipped_plugins_is_refused_with_its_way_back"
t_release_before_the_shipped_plugins_is_refused_with_its_way_back() {
	serve 1.0.0
	d=$(served 1.0.0)
	top=eitri-1.0.0-x86_64-linux
	rm -rf "$T/old"
	mkdir -p "$T/old"
	tar -C "$T/old" -xzf "$d/$top.tar.gz"
	rm -rf "$T/old/$top/lib/eitri/plugins"
	tar -C "$T/old" -czf "$d/$top.tar.gz" "$top"
	resign "$d"
	relatest 1.0.0
	before=$(snap "$TH")
	inst_net
	expect_fail "a release from before the shipped plugins"
	expect_out "is from a release before Eitri shipped plugins"
	expect_out "use its own install.sh"
	expect_eq "$(snap "$TH")" "$before" "the home after the refusal"
}

TESTS="$TESTS t_from_source_stages_the_plugin_directory"
t_from_source_stages_the_plugin_directory() {
	FS_FORK_REV=eeeeeee111111111111111111111111111111111
	fs_verdandi_checkout "$T/verdandi-src"
	fs_repo 1.0.0 https://github.com/HunterGrey-cyber/verdandi.git "$FS_VC_HEAD"
	PRE_STUBS=$(fs_cargo_stubs)
	inst --set "EITRI_INSTALL_TEST_VERDANDI_REPO_URL=$T/verdandi-src" \
		--set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		--set "EITRI_INSTALL_TEST_SKIA_BASE_URL=http://127.0.0.1:$PORT/skia" \
		-- --from-source --checkout "$FS_DIR"
	expect_rc 0
	if ! cmp -s "$FS_DIR/plugins/markdown/eitri-plugin.toml" "$(plugin_dir)/eitri-plugin.toml"; then
		fail "the installed manifest is not the checkout's"
	fi
	if ! cmp -s "$FS_DIR/plugins/markdown/web/dist/index.html" "$(plugin_dir)/web/dist/index.html"; then
		fail "the installed page is not the one the build wrote"
	fi
	expect_file "$(plugin_dir)/web/dist/preview.js"
	expect_absent "$(plugin_dir)/web/dist/.inputs-sha256"
	expect_absent "$(plugin_dir)/web/src"
}

TESTS="$TESTS t_from_source_without_the_plugin_site_refuses"
t_from_source_without_the_plugin_site_refuses() {
	FS_FORK_REV=eeeeeee111111111111111111111111111111111
	fs_verdandi_checkout "$T/verdandi-src"
	fs_repo 1.0.0 https://github.com/HunterGrey-cyber/verdandi.git "$FS_VC_HEAD"
	PRE_STUBS=$(fs_cargo_stubs)
	inst --set "EITRI_INSTALL_TEST_VERDANDI_REPO_URL=$T/verdandi-src" \
		--set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		--set "EITRI_INSTALL_TEST_SKIA_BASE_URL=http://127.0.0.1:$PORT/skia" \
		--set "EITRI_INSTALL_TEST_CARGO_MODE=no-plugin-site" \
		-- --from-source --checkout "$FS_DIR"
	expect_fail "a build that did not write the plugin's site"
	expect_out "the build did not produce plugins/markdown/web/dist/index.html"
	# A checkout's web/dist/ removed by hand is not rebuilt while target/ is warm: the refusal says how to recover.
	expect_out "run cargo clean -p shell --release --target-dir "
	expect_out "/target and install again"
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_from_source_tree_before_the_shipped_plugins_is_refused_with_its_way_back"
t_from_source_tree_before_the_shipped_plugins_is_refused_with_its_way_back() {
	FS_FORK_REV=eeeeeee111111111111111111111111111111111
	fs_verdandi_checkout "$T/verdandi-src"
	FS_DIR=$T/fs-repo
	fs_scaffold "$FS_DIR" 1.0.0 https://github.com/HunterGrey-cyber/verdandi.git "$FS_VC_HEAD"
	rm -rf "$FS_DIR/plugins"
	fs_commit_and_tag "$FS_DIR" 1.0.0
	PRE_STUBS=$(fs_cargo_stubs)
	inst --set "EITRI_INSTALL_TEST_VERDANDI_REPO_URL=$T/verdandi-src" \
		--set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		--set "EITRI_INSTALL_TEST_SKIA_BASE_URL=http://127.0.0.1:$PORT/skia" \
		-- --from-source --checkout "$FS_DIR"
	expect_fail "a source tree from before the shipped plugins"
	expect_out "is a source tree from before Eitri shipped plugins"
	expect_out "packaging/install.sh --from-source in that tree"
	expect_no_out "report it"
	expect_absent "$TH/.local/lib/eitri"
}
