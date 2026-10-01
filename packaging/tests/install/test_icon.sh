# The application icon and the desktop entry's new name (cn.huntergrey.eitri.desktop, the application
# id; the icon installed under the user's hicolor tree; the old eitri.desktop removed when it is the one
# this installer wrote). Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

# The icon files, from the committed tree: hicolor/<size>/apps/cn.huntergrey.eitri.{png,svg}.
icon_files() { (cd "$PKG/icons" && find hicolor -type f | LC_ALL=C sort); }

# plant_old_desktop [HOME]: the eitri.desktop an installer before the app icon wrote for HOME: the
# tarball's entry of those releases (packaging/legacy/eitri.desktop), its Exec line rewritten to the
# quoted launcher path the way that installer's awk did (test below: the real 0.2.0 installer's output
# is this, byte for byte).
plant_old_desktop() {
	_pod_h=${1:-$TH}
	mkdir -p "$_pod_h/.local/share/applications"
	NV_EXEC="Exec=\"$_pod_h/.local/bin/eitri\" --quiet %f" awk '
		/^Exec=/ { print ENVIRON["NV_EXEC"]; next }
		{ print }' "$PKG/legacy/eitri.desktop" >"$_pod_h/.local/share/applications/eitri.desktop"
}

# run_old_installer ARGS...: inst_net, with 0.2.0's installer (the harness's unkeyed copy of the file
# 0.2.0 shipped) as the installer under test.
run_old_installer() {
	INSTALLER_UNDER_TEST=$OLD_INSTALLER
	inst_net "$@"
	INSTALLER_UNDER_TEST=
}

# icon_tmps: any leftover temporary name under the data directory's icons.
icon_tmps() { find "$(data_of)/icons" -name '*.tmp.*' 2>/dev/null; }

TESTS="$TESTS t_installer_icon_list_is_the_committed_icons"
t_installer_icon_list_is_the_committed_icons() {
	# NV_ICON_FILES is the list install.sh installs and validates the tarball by; release_check.py's role
	# table holds the tarball to the same set and the nfpm profiles list it too -- this holds the
	# installer's own copy to the files that are committed.
	listed=$(sed -n "s/^NV_ICON_FILES='\\(.*\\)'\$/\\1/p" "$REAL_INSTALLER" | tr ' ' '\n' | LC_ALL=C sort)
	expect_eq "$listed" "$(icon_files)" "install.sh's NV_ICON_FILES against packaging/icons/hicolor"
	expect_eq "$(printf '%s\n' "$listed" | grep -c .)" 9 "the number of icon files"
}

TESTS="$TESTS t_icons_installed"
t_icons_installed() {
	serve 1.0.0
	inst_net
	expect_rc 0
	for f in $(icon_files); do
		expect_file "$(data_of)/icons/$f"
		if ! cmp -s "$PKG/icons/$f" "$(data_of)/icons/$f"; then fail "the installed icon $f is not packaging/icons/$f"; fi
	done
	expect_eq "$(find "$(data_of)/icons" -type f | wc -l | tr -d ' ')" 9 "the files under the icons directory"
	expect_eq "$(icon_tmps)" "" "temporary names left behind"
	# The cache of this user's hicolor directory, and only of that: refreshed once, after the files.
	expect_eq "$(cat "$S/logs/icon-cache.log")" "gtk-update-icon-cache -f -t $(data_of)/icons/hicolor" \
		"the icon cache refresh"
	# The entry names the icon and the window class by the application id.
	d=$(data_of)/applications/cn.huntergrey.eitri.desktop
	if ! grep -Fqx 'Icon=cn.huntergrey.eitri' "$d"; then fail "the installed entry has no Icon=cn.huntergrey.eitri"; fi
	if ! grep -Fqx 'StartupWMClass=cn.huntergrey.eitri' "$d"; then fail "the installed entry has no StartupWMClass"; fi
	expect_absent "$(data_of)/applications/eitri.desktop"
}

TESTS="$TESTS t_icons_follow_xdg_data_home"
t_icons_follow_xdg_data_home() {
	serve 1.0.0
	inst_net --set "XDG_DATA_HOME=$TH/xdg data" --
	expect_rc 0
	expect_file "$TH/xdg data/icons/hicolor/scalable/apps/cn.huntergrey.eitri.svg"
	expect_file "$TH/xdg data/icons/hicolor/48x48/apps/cn.huntergrey.eitri.png"
	expect_absent "$TH/.local/share/icons"
	expect_eq "$(cat "$S/logs/icon-cache.log")" "gtk-update-icon-cache -f -t $TH/xdg data/icons/hicolor" \
		"the icon cache refresh"
	inst --set "XDG_DATA_HOME=$TH/xdg data" -- --uninstall
	expect_rc 0
	expect_absent "$TH/xdg data/icons"
	expect_absent "$TH/xdg data/applications/cn.huntergrey.eitri.desktop"
}

TESTS="$TESTS t_icon_cache_failure_is_a_warning"
t_icon_cache_failure_is_a_warning() {
	serve 1.0.0
	PRE_STUBS=$S/stubs-iccache-fail
	inst_net
	PRE_STUBS=
	expect_rc 0 "gtk-update-icon-cache failing must never fail the install"
	expect_out 'warning: gtk-update-icon-cache failed on'
	expect_out 'installed Eitri 1.0.0'
	expect_file "$(data_of)/icons/hicolor/scalable/apps/cn.huntergrey.eitri.svg"
	expect_eq "$(installed_version)" 1.0.0 "the installed version"
}

TESTS="$TESTS t_icon_cache_tool_absent"
t_icon_cache_tool_absent() {
	serve 1.0.0
	# The harness's stub is taken off the PATH for this one run, and the PATH tail has no real one.
	mv "$S/stubs/gtk-update-icon-cache" "$S/stubs/.gtk-update-icon-cache.off"
	inst_net --path-tail "$S/path-no-gtk-update-icon-cache" --
	mv "$S/stubs/.gtk-update-icon-cache.off" "$S/stubs/gtk-update-icon-cache"
	expect_rc 0
	expect_out 'installed Eitri 1.0.0'
	expect_no_out 'gtk-update-icon-cache'
	expect_file "$(data_of)/icons/hicolor/scalable/apps/cn.huntergrey.eitri.svg"
	expect_eq "$(cat "$S/logs/icon-cache.log")" "" "no icon cache tool ran"
	expect_absent "$(data_of)/icons/hicolor/icon-theme.cache"
}

TESTS="$TESTS t_dry_run_lists_the_icons"
t_dry_run_lists_the_icons() {
	serve 1.0.0
	before=$(snap "$TH")
	inst_net --dry-run
	expect_rc 0
	for f in $(icon_files); do expect_out "would write $TH/.local/share/icons/$f"; done
	expect_out "would run: gtk-update-icon-cache -f -t $TH/.local/share/icons/hicolor"
	expect_eq "$(cat "$S/logs/icon-cache.log")" "" "a dry run runs no icon cache tool"
	expect_eq "$(snap "$TH")" "$before" "the home after a dry run"
}

TESTS="$TESTS t_upgrade_from_before_the_icon_replaces_the_desktop_entry"
t_upgrade_from_before_the_icon_replaces_the_desktop_entry() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	plant_sidecar bbbbbbb
	# The home an earlier release left: the entry under its old name, and no icons at all.
	rm -rf "$(data_of)/icons"
	rm "$(data_of)/applications/cn.huntergrey.eitri.desktop"
	plant_old_desktop
	serve 1.1.0 1.0.0
	inst_net
	expect_rc 0
	expect_out "removing the old desktop entry $(data_of)/applications/eitri.desktop"
	expect_out 'pinned again once'
	expect_absent "$(data_of)/applications/eitri.desktop"
	expect_file "$(data_of)/applications/cn.huntergrey.eitri.desktop"
	expect_file "$(data_of)/icons/hicolor/scalable/apps/cn.huntergrey.eitri.svg"
	expect_eq "$(find "$(data_of)/icons" -type f | wc -l | tr -d ' ')" 9 "the icon files after the upgrade"
	expect_eq "$(ls "$(data_of)/applications")" cn.huntergrey.eitri.desktop "the desktop entries after the upgrade"
	# The same re-run is up to date, and removes nothing more.
	inst_net
	expect_rc 0
	expect_out 'Eitri 1.1.0 is up to date'
}

TESTS="$TESTS t_old_desktop_entry_that_is_not_ours_is_left_alone"
t_old_desktop_entry_that_is_not_ours_is_left_alone() {
	# A file called eitri.desktop that this installer did not write is the user's, whatever it holds:
	# an edited Exec line, an added key, another launcher, a symlink, a directory -- and a copy with
	# lines REMOVED, or only reordered, or with different line endings: ours means byte for byte what
	# an earlier installer wrote, so every one of those is the user's.
	n=0
	for variant in exec_edited extra_key other_launcher symlink directory no_comment no_mimetype no_startup_notify \
		no_final_newline blank_line_added reordered crlf comment_text_edited; do
		n=$((n + 1))
		use_home "$T/home-$n"
		mkdir -p "$TH/.local/share/applications"
		old=$TH/.local/share/applications/eitri.desktop
		case $variant in
		exec_edited)
			plant_old_desktop
			sed -i 's#--quiet %f#--quiet --clean %f#' "$old"
			;;
		extra_key)
			plant_old_desktop
			printf 'X-GNOME-Autostart-enabled=false\n' >>"$old"
			;;
		other_launcher)
			plant_old_desktop
			sed -i "s#^Exec=.*#Exec=/opt/other/eitri --quiet %f#" "$old"
			;;
		symlink)
			printf '[Desktop Entry]\nType=Application\nName=mine\nExec=true\n' >"$TH/mine.desktop"
			ln -s "$TH/mine.desktop" "$old"
			;;
		directory) mkdir "$old" ;;
		no_comment)
			plant_old_desktop
			sed -i '/^Comment=/d' "$old"
			;;
		no_mimetype)
			plant_old_desktop
			sed -i '/^MimeType=/d' "$old"
			;;
		no_startup_notify)
			plant_old_desktop
			sed -i '/^StartupNotify=/d' "$old"
			;;
		no_final_newline)
			plant_old_desktop
			printf '%s' "$(cat "$old")" >"$old.cut"
			mv "$old.cut" "$old"
			;;
		blank_line_added)
			plant_old_desktop
			printf '\n' >>"$old"
			;;
		reordered)
			plant_old_desktop
			sed -i -e '2{h;d}' -e '3{G}' "$old"
			;;
		crlf)
			plant_old_desktop
			sed -i 's/$/\r/' "$old"
			;;
		comment_text_edited)
			plant_old_desktop
			sed -i 's/^Comment=.*/Comment=my own editor/' "$old"
			;;
		esac
		before=$(snap "$TH/.local/share/applications")
		serve 1.0.0
		inst_net
		expect_rc 0 "$variant"
		expect_out "$old was left alone: it is not the entry an earlier Eitri installed"
		expect_no_out 'removing the old desktop entry'
		# Everything but the new entry is exactly as it was.
		if [ -d "$old" ]; then
			expect_dir "$old"
		else
			expect_eq "$(snap "$TH/.local/share/applications" | grep -v 'cn.huntergrey.eitri.desktop')" \
				"$(printf '%s\n' "$before" | grep -v 'cn.huntergrey.eitri.desktop')" "the old entry ($variant)"
		fi
		expect_file "$TH/.local/share/applications/cn.huntergrey.eitri.desktop"
		inst -- --uninstall
		expect_rc 0 "$variant"
		expect_out "$old was left alone: it is not the entry an earlier Eitri installed"
		if [ ! -e "$old" ] && [ ! -L "$old" ]; then fail "--uninstall removed a foreign $variant eitri.desktop"; fi
		expect_absent "$TH/.local/share/applications/cn.huntergrey.eitri.desktop"
	done
}

TESTS="$TESTS t_dry_run_says_what_it_would_do_with_the_old_desktop_entry"
t_dry_run_says_what_it_would_do_with_the_old_desktop_entry() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_old_desktop
	before=$(snap "$TH")
	serve 1.1.0 1.0.0
	inst_net --dry-run
	expect_rc 0
	expect_out "would run: 'rm' '-f' '--' '$(data_of)/applications/eitri.desktop'"
	expect_eq "$(snap "$TH")" "$before" "the home after a dry run"
}

# serve_old_layout VERSION [OLDER...]: serve VERSION (and OLDER) as the fixtures, then make VERSION's tarball
# the layout of a release from before the icon: its entry share/applications/eitri.desktop (the one every
# release tarball carries, packaging/legacy/eitri.desktop), no new entry and no icons.
serve_old_layout() {
	serve "$@"
	_sol_d=$(served "$1")
	_sol_top=eitri-$1-x86_64-linux
	rm -rf "$T/old"
	mkdir -p "$T/old"
	tar -C "$T/old" -xzf "$_sol_d/$_sol_top.tar.gz"
	rm "$T/old/$_sol_top/share/applications/cn.huntergrey.eitri.desktop"
	rm -rf "$T/old/$_sol_top/share/icons"
	tar -C "$T/old" -czf "$_sol_d/$_sol_top.tar.gz" "$_sol_top"
	resign "$_sol_d"
	relatest "$1"
}

TESTS="$TESTS t_old_layout_tarball_is_refused_with_a_pointer"
t_old_layout_tarball_is_refused_with_a_pointer() {
	# A release from before the icon (its entry share/applications/eitri.desktop, no icons): this
	# installer does not lay it out, says why, and changes nothing. The advice it gives is safe to
	# follow: that release's own install.sh would leave this installer's entry and icons beside its
	# own, so this installer's --uninstall comes first, and the message says what that removes.
	serve_old_layout 1.0.0
	before=$(snap "$TH")
	inst_net
	expect_fail "a tarball from before the app icon"
	expect_out "is from a release before Eitri's app icon"
	expect_out "would leave any desktop entry and icons this installer installed beside its own"
	expect_out 'first run: eitri setup --uninstall (or, with this installer: sh install.sh --uninstall)'
	expect_out "It removes the launcher, the program in $TH/.local/lib/eitri, the desktop entries and icons, the licences, the private nvim and the sidecars Eitri built"
	expect_out "keeps your settings ($TH/.config/eitri) and state ($TH/.local/state/eitri)"
	expect_out "then install that release with its own install.sh, a release asset"
	expect_eq "$(snap "$TH")" "$before" "the home after the refusal"
}

TESTS="$TESTS t_old_layout_advice_leaves_no_new_entry_or_icon_behind"
t_old_layout_advice_leaves_no_new_entry_or_icon_behind() {
	# Following the refusal's advice on a home that has this installer's layout: --uninstall removes
	# what the message says it does -- and so no cn.huntergrey.eitri.desktop and no icon is left to sit
	# beside the old release's eitri.desktop -- and keeps what it says it keeps.
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	mkdir -p "$TH/.config/eitri" "$TH/.local/state/eitri/layout"
	printf 'eitri.config.set("agent.font_size", "16")\n' >"$TH/.config/eitri/init.lua"
	printf '{}\n' >"$TH/.local/state/eitri/layout/0123456789abcdef.json"
	serve_old_layout 1.1.0 1.0.0
	inst_net --version 1.1.0
	expect_fail "a tarball from before the app icon"
	expect_out 'first run: eitri setup --uninstall'
	inst -- --uninstall
	expect_rc 0
	expect_absent "$TH/.local/lib/eitri"
	expect_absent "$TH/.local/bin/eitri"
	expect_absent "$(data_of)/applications/cn.huntergrey.eitri.desktop"
	expect_absent "$(data_of)/icons"
	expect_absent "$(data_of)/licenses"
	expect_absent "$(data_of)/eitri/sidecar/aaaaaaa"
	expect_eq "$(cat "$TH/.config/eitri/init.lua")" 'eitri.config.set("agent.font_size", "16")' "the settings"
	expect_file "$TH/.local/state/eitri/layout/0123456789abcdef.json"
	# ... and then that release's own installer (0.2.0's, here) leaves exactly its own layout: one entry,
	# eitri.desktop, no icon, and the settings and state as they were.
	run_old_installer --version 1.1.0
	expect_rc 0 "the older installer after the advice's --uninstall"
	expect_out 'installed Eitri 1.1.0'
	expect_eq "$(ls "$(data_of)/applications")" eitri.desktop "the desktop entries after the old installer"
	expect_absent "$(data_of)/icons"
	expect_eq "$(cat "$TH/.config/eitri/init.lua")" 'eitri.config.set("agent.font_size", "16")' "the settings"
}

TESTS="$TESTS t_the_tarball_carries_the_legacy_entry_and_this_installer_ignores_it"
t_the_tarball_carries_the_legacy_entry_and_this_installer_ignores_it() {
	# Every fixture release carries 0.2.0's entry as share/applications/eitri.desktop, as the real
	# tarball does (packaging/legacy/README.md). It is read by 0.2.0's installer only: this one installs
	# the entry named by the application id and nothing under the old name.
	serve 1.0.0
	d=$(served 1.0.0)
	top=eitri-1.0.0-x86_64-linux
	tar -xzOf "$d/$top.tar.gz" "$top/share/applications/eitri.desktop" >"$T/legacy.desktop"
	if ! cmp -s "$T/legacy.desktop" "$PKG/legacy/eitri.desktop"; then fail "the tarball's eitri.desktop is not packaging/legacy/eitri.desktop"; fi
	inst_net
	expect_rc 0
	expect_eq "$(ls "$(data_of)/applications")" cn.huntergrey.eitri.desktop "the desktop entries this installer installed"
	expect_no_out 'old desktop entry'
	expect_no_out 'was left alone'
	# A later run over a tarball that still carries it changes nothing.
	plant_sidecar aaaaaaa
	before=$(snap "$TH")
	inst_net
	expect_rc 0
	expect_out 'Eitri 1.0.0 is up to date'
	expect_eq "$(snap "$TH")" "$before" "the home after a re-run"
}

TESTS="$TESTS t_0_2_0_installer_upgrades_over_a_release_with_the_legacy_entry_and_this_one_finishes_it"
t_0_2_0_installer_upgrades_over_a_release_with_the_legacy_entry_and_this_one_finishes_it() {
	# A user who saved 0.2.0's install.sh and reruns it to upgrade (INSTALL.md recommends it): the new
	# release's tarball has to pass that installer's checks, which want share/applications/eitri.desktop.
	# Then this installer, run once more, recognises the entry the old one wrote -- byte for byte, in a
	# HOME with ordinary and with every special character -- replaces it with the new entry and the icons,
	# and a third run is up to date.
	n=0
	for variant in plain special; do
		n=$((n + 1))
		# shellcheck disable=SC2016 # literal characters, deliberately unexpanded
		if [ "$variant" = special ]; then use_home "$T/"'q"b`t$d\s 5%'; else use_home "$T/home-$n"; fi
		# 0.2.0 itself: a release of the old layout, installed by 0.2.0's installer.
		serve_old_layout 1.0.0
		run_old_installer
		expect_rc 0 "$variant: 0.2.0's installer over a release of the old layout"
		expect_out 'installed Eitri 1.0.0'
		expect_eq "$(ls "$(data_of)/applications")" eitri.desktop "$variant: the entry 0.2.0 installed"
		expect_absent "$(data_of)/icons"
		if [ "$variant" = plain ]; then
			# (plant_old_desktop does not escape an Exec path: only the plain HOME can be compared. The
			# special one is held by this installer removing the entry below.)
			cp "$(data_of)/applications/eitri.desktop" "$T/written-by-0.2.0.desktop"
			plant_old_desktop
			if ! cmp -s "$T/written-by-0.2.0.desktop" "$(data_of)/applications/eitri.desktop"; then
				fail "$variant: 0.2.0's entry is not what the harness plants as one"
			fi
		fi
		# The upgrade through the old installer: this release's tarball (new layout plus the legacy entry).
		serve 1.1.0 1.0.0
		run_old_installer
		expect_rc 0 "$variant: 0.2.0's installer over this release's tarball"
		expect_out 'installed Eitri 1.1.0'
		expect_eq "$(installed_version)" 1.1.0 "$variant: the version the old installer installed"
		expect_eq "$(ls "$(data_of)/applications")" eitri.desktop "$variant: the entries after the old installer's upgrade"
		expect_absent "$(data_of)/icons"
		plant_sidecar aaaaaaa
		plant_sidecar bbbbbbb
		# This installer, once more.
		inst_net
		expect_rc 0 "$variant: this installer after the old one"
		expect_out 'installed but not finished'
		expect_out "removing the old desktop entry $(data_of)/applications/eitri.desktop"
		expect_no_out 'was left alone'
		expect_eq "$(ls "$(data_of)/applications")" cn.huntergrey.eitri.desktop "$variant: the entries after this installer"
		expect_eq "$(find "$(data_of)/icons" -type f | wc -l | tr -d ' ')" 9 "$variant: the icon files"
		expect_eq "$(installed_version)" 1.1.0 "$variant: the installed version"
		inst_net
		expect_rc 0
		expect_out 'Eitri 1.1.0 is up to date'
	done
}

TESTS="$TESTS t_tarball_missing_an_icon_is_refused"
t_tarball_missing_an_icon_is_refused() {
	serve 1.0.0
	d=$(served 1.0.0)
	top=eitri-1.0.0-x86_64-linux
	rm -rf "$T/bad"
	mkdir -p "$T/bad"
	tar -C "$T/bad" -xzf "$d/$top.tar.gz"
	rm "$T/bad/$top/share/icons/hicolor/64x64/apps/cn.huntergrey.eitri.png"
	tar -C "$T/bad" -czf "$d/$top.tar.gz" "$top"
	resign "$d"
	relatest 1.0.0
	before=$(snap "$TH")
	inst_net
	expect_fail "a tarball without one icon"
	expect_out 'has no share/icons/hicolor/64x64/apps/cn.huntergrey.eitri.png'
	expect_eq "$(snap "$TH")" "$before" "the home after the refusal"
}

TESTS="$TESTS t_upgrade_with_an_unwritable_icon_directory_leaves_no_temporary_names"
t_upgrade_with_an_unwritable_icon_directory_leaves_no_temporary_names() {
	# The icons are staged (to temporary names beside their destinations) before the swap, like the
	# launcher: a directory that cannot be written stops the upgrade with the old install untouched, and
	# the names already staged in the sizes before it are removed on the way out.
	serve 1.0.0
	inst_net
	expect_rc 0
	serve 1.1.0 1.0.0
	before=$(snap_but_staging "$TH")
	chmod 0555 "$(data_of)/icons/hicolor/48x48/apps"
	if [ -w "$(data_of)/icons/hicolor/48x48/apps" ]; then
		chmod 0755 "$(data_of)/icons/hicolor/48x48/apps"
		return 0
	fi
	inst_net
	chmod 0755 "$(data_of)/icons/hicolor/48x48/apps"
	expect_fail "an upgrade with an unwritable icon directory"
	expect_out "cannot write $(data_of)/icons/hicolor/48x48/apps/.cn.huntergrey.eitri.png.tmp."
	expect_no_out 'installed Eitri 1.1.0'
	expect_eq "$(icon_tmps)" "" "temporary names left behind"
	expect_eq "$(installed_version)" 1.0.0 "the version after the refused upgrade"
	expect_eq "$(snap_but_staging "$TH")" "$before" "the home after the refused upgrade"
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.1.0 "the version after the re-run"
}

TESTS="$TESTS t_rerun_finishes_an_install_that_lost_an_icon"
t_rerun_finishes_an_install_that_lost_an_icon() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	rm "$(data_of)/icons/hicolor/scalable/apps/cn.huntergrey.eitri.svg"
	inst_net
	expect_rc 0
	expect_no_out 'is up to date'
	expect_out 'installed Eitri 1.0.0'
	expect_file "$(data_of)/icons/hicolor/scalable/apps/cn.huntergrey.eitri.svg"
}

TESTS="$TESTS t_uninstall_removes_the_icons_and_nothing_else_in_hicolor"
t_uninstall_removes_the_icons_and_nothing_else_in_hicolor() {
	serve 1.0.0
	inst_net
	expect_rc 0
	# Another application's icon, a file of the user's own in a size directory Eitri also uses, and
	# an index.theme: all theirs.
	h=$(data_of)/icons/hicolor
	mkdir -p "$h/48x48/apps" "$h/22x22/apps"
	printf 'other\n' >"$h/48x48/apps/other-app.png"
	printf 'other\n' >"$h/22x22/apps/other-app.png"
	printf '[Icon Theme]\nName=mine\n' >"$h/index.theme"
	printf 'mine\n' >"$h/48x48/apps/cn.huntergrey.eitri-not-ours.png"
	mine=$(snap "$h/22x22")
	: >"$S/logs/icon-cache.log"
	inst -- --uninstall
	expect_rc 0
	expect_out 'Eitri is uninstalled'
	for f in $(icon_files); do expect_absent "$(data_of)/icons/$f"; done
	expect_file "$h/48x48/apps/other-app.png"
	expect_file "$h/48x48/apps/cn.huntergrey.eitri-not-ours.png"
	expect_file "$h/index.theme"
	expect_eq "$(snap "$h/22x22")" "$mine" "another application's icons"
	# The size directories Eitri emptied are gone; the ones still holding something are not.
	expect_absent "$h/16x16"
	expect_absent "$h/scalable"
	expect_dir "$h/48x48/apps"
	# What stays is cached again, once, by the same tool and flags.
	expect_eq "$(cat "$S/logs/icon-cache.log")" "gtk-update-icon-cache -f -t $h" "the cache after --uninstall"
}

TESTS="$TESTS t_uninstall_with_only_eitris_icons_leaves_no_icon_directories"
t_uninstall_with_only_eitris_icons_leaves_no_icon_directories() {
	serve 1.0.0
	PRE_STUBS=$S/stubs-iccache-writes
	inst_net
	PRE_STUBS=
	expect_rc 0
	expect_file "$(data_of)/icons/hicolor/icon-theme.cache"
	inst -- --uninstall
	expect_rc 0
	# The cache the tool wrote was the only other thing there: it goes with the directories.
	expect_absent "$(data_of)/icons"
	expect_absent "$(data_of)/applications/cn.huntergrey.eitri.desktop"
	expect_absent "$(data_of)/licenses"
	expect_eq "$(grep -c . "$S/logs/icon-cache.log")" 1 "the tool ran once (the install's), not again"
}

TESTS="$TESTS t_uninstall_removes_the_old_desktop_entry_when_it_is_ours"
t_uninstall_removes_the_old_desktop_entry_when_it_is_ours() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_old_desktop
	inst -- --uninstall
	expect_rc 0
	expect_absent "$(data_of)/applications/eitri.desktop"
	expect_absent "$(data_of)/applications/cn.huntergrey.eitri.desktop"
	expect_no_out 'was left alone: it is not the entry an earlier Eitri installed'
}

TESTS="$TESTS t_uninstall_dry_run_names_the_icons"
t_uninstall_dry_run_names_the_icons() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_old_desktop
	before=$(snap "$TH")
	inst -- --uninstall --dry-run
	expect_rc 0
	for f in $(icon_files); do expect_out "would run: 'rm' '-rf' '--' '$(data_of)/icons/$f'"; done
	expect_out "would run: 'rm' '-rf' '--' '$(data_of)/applications/eitri.desktop'"
	expect_eq "$(snap "$TH")" "$before" "the home after a dry-run uninstall"
}

TESTS="$TESTS t_uninstall_removes_exactly_the_icon_files_it_installs"
t_uninstall_removes_exactly_the_icon_files_it_installs() {
	# --uninstall removes the files NV_ICON_FILES names, by path, and nothing a pattern would also
	# match: the user's own icon of Eitri's name in a size this installer never writes (96x96), the
	# other extension in a size it does (48x48 .svg, scalable .png), and a path of the list that holds a
	# symlink or a directory rather than the file this installer wrote.
	serve 1.0.0
	inst_net
	expect_rc 0
	h=$(data_of)/icons/hicolor
	mkdir -p "$h/96x96/apps" "$h/22x22/apps"
	printf 'mine 96\n' >"$h/96x96/apps/cn.huntergrey.eitri.png"
	printf 'mine svg\n' >"$h/48x48/apps/cn.huntergrey.eitri.svg"
	printf 'mine png\n' >"$h/scalable/apps/cn.huntergrey.eitri.png"
	printf 'mine 22\n' >"$h/22x22/apps/cn.huntergrey.eitri.svg"
	# A symlink in place of one installed file, and a directory in place of another.
	printf 'target\n' >"$TH/their-icon.png"
	rm "$h/32x32/apps/cn.huntergrey.eitri.png"
	ln -s "$TH/their-icon.png" "$h/32x32/apps/cn.huntergrey.eitri.png"
	rm "$h/64x64/apps/cn.huntergrey.eitri.png"
	mkdir "$h/64x64/apps/cn.huntergrey.eitri.png"
	printf 'inside\n' >"$h/64x64/apps/cn.huntergrey.eitri.png/keep"
	inst -- --uninstall --dry-run
	expect_rc 0
	for f in $(icon_files); do
		case $f in
		hicolor/32x32/* | hicolor/64x64/*) expect_no_out "would run: 'rm' '-rf' '--' '$(data_of)/icons/$f'" ;;
		*) expect_out "would run: 'rm' '-rf' '--' '$(data_of)/icons/$f'" ;;
		esac
	done
	for f in 96x96/apps/cn.huntergrey.eitri.png 48x48/apps/cn.huntergrey.eitri.svg scalable/apps/cn.huntergrey.eitri.png \
		22x22/apps/cn.huntergrey.eitri.svg; do
		expect_no_out "$h/$f"
	done
	inst -- --uninstall
	expect_rc 0
	expect_out 'Eitri is uninstalled'
	for f in $(icon_files); do
		case $f in
		hicolor/32x32/* | hicolor/64x64/*) ;;
		*) expect_absent "$(data_of)/icons/$f" ;;
		esac
	done
	expect_eq "$(cat "$h/96x96/apps/cn.huntergrey.eitri.png")" 'mine 96' "the user's 96x96 icon"
	expect_eq "$(cat "$h/48x48/apps/cn.huntergrey.eitri.svg")" 'mine svg' "the user's 48x48 svg"
	expect_eq "$(cat "$h/scalable/apps/cn.huntergrey.eitri.png")" 'mine png' "the user's scalable png"
	expect_eq "$(cat "$h/22x22/apps/cn.huntergrey.eitri.svg")" 'mine 22' "the user's 22x22 svg"
	if [ ! -L "$h/32x32/apps/cn.huntergrey.eitri.png" ]; then fail "the symlink at an icon path was removed"; fi
	expect_eq "$(cat "$TH/their-icon.png")" target "the file the symlink led to"
	expect_eq "$(cat "$h/64x64/apps/cn.huntergrey.eitri.png/keep")" inside "the directory at an icon path"
	# What was left, said once each.
	expect_out "$h/32x32/apps/cn.huntergrey.eitri.png was left alone"
	expect_out "$h/64x64/apps/cn.huntergrey.eitri.png was left alone"
}
