# Root (ROOT-1), paths (SH-4), the launcher (MIN-2), the system checks and their distro hints.
# Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

TESTS="$TESTS t_root_refused"
t_root_refused() {
	serve 1.0.0
	inst_net --stubs "$S/stubs-root" --
	expect_fail "running as root"
	expect_out 'do not run this as root: run it as the user who will run Eitri'
	expect_out '--allow-root'
	expect_absent "$TH/.local/lib/eitri"
	inst --stubs "$S/stubs-root" -- --uninstall
	expect_fail "uninstalling as root"
}

TESTS="$TESTS t_root_allowed"
t_root_allowed() {
	serve 1.0.0
	inst_net --stubs "$S/stubs-root" -- --allow-root
	expect_rc 0
	expect_out 'running as root (--allow-root)'
	expect_eq "$(installed_version)" 1.0.0 "installed with --allow-root"
}

TESTS="$TESTS t_home_root_refused"
t_home_root_refused() {
	# A HOME that is / however it is spelled -- `//` passed a check made before trimming slashes --
	# or that holds a `.` or `..` component is refused before anything is read or locked: a purge
	# there would name /.config/eitri. Dry runs, so a missing refusal still changes nothing.
	for h in / // /./ /.. "$TH/.." "$TH/./"; do
		inst --home "$h" -- --uninstall --purge --dry-run
		expect_fail "HOME=$h"
		expect_out 'eitri: error: HOME '
		expect_no_out 'would take the lock'
	done
}

TESTS="$TESTS t_relative_xdg_data"
t_relative_xdg_data() {
	serve 1.0.0
	inst_net --set XDG_DATA_HOME=relative/data --set XDG_CACHE_HOME= --set XDG_STATE_HOME=. --
	expect_rc 0
	expect_eq "$(tree "$TH")" "$EXPECTED_TREE" "the tree with a relative XDG_DATA_HOME"
	# Nothing relative was used: the cwd stays empty (after_each), and so does anything named like
	# the relative values.
	if [ -n "$(find "$T" -name relative)" ]; then fail "a relative XDG path was used: $(find "$T" -name relative)"; fi
	# With XDG_* set to absolute paths elsewhere under HOME, those are used.
	use_home "$T/home2"
	inst_net --set "XDG_DATA_HOME=$T/home2/xdg data" --
	expect_rc 0
	expect_file "$T/home2/xdg data/applications/eitri.desktop"
	expect_file "$T/home2/xdg data/licenses/eitri/LICENSE"
	expect_absent "$T/home2/.local/share"
}

TESTS="$TESTS t_xdg_data_home_outside_home_refused_at_install"
t_xdg_data_home_outside_home_refused_at_install() {
	# installer-claude-9: an XDG_DATA_HOME outside $HOME used to write there anyway and leave
	# --uninstall permanently refusing (it never removes anything outside $HOME, whole run). Refused
	# up front now, before anything is written, on every mode that writes under it.
	serve 1.0.0
	inst_net --set "XDG_DATA_HOME=$T/outside-data" --
	expect_fail "XDG_DATA_HOME outside \$HOME"
	expect_out 'XDG_DATA_HOME resolves to'
	expect_out 'not inside'
	expect_absent "$TH/.local/lib/eitri"
	expect_absent "$T/outside-data"
	# --uninstall itself is deliberately exempt from this new refusal (it needs to keep removing an
	# install made before this check existed, or by hand): a bare --uninstall with nothing installed
	# still reaches its ordinary "nothing to remove", not this row's own message.
	inst --set "XDG_DATA_HOME=$T/outside-data" -- --uninstall
	expect_rc 0
	expect_no_out 'XDG_DATA_HOME resolves to'
}

TESTS="$TESTS t_space_home_end_to_end"
t_space_home_end_to_end() {
	use_home "$T/a home/with spaces"
	serve 1.0.0
	inst_net
	expect_rc 0
	expect_eq "$(tree "$TH")" "$EXPECTED_TREE" "the tree in a HOME with spaces"
	plant_sidecar aaaaaaa
	inst_net
	expect_rc 0
	expect_out 'is up to date'
	serve 1.1.0 1.0.0
	# F1 (v1-dist whole-branch review, 2026-09-28): plant_sidecar above makes the installed rev's
	# own sidecar genuinely "present", so ensure_sidecar's Node download is no longer tolerant here
	# -- a working install's own sidecar is being replaced. Node's own download is redirected (like
	# sc_inst_net) so the upgrade's real sidecar build succeeds.
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	expect_rc 0
	expect_eq "$(installed_version)" 1.1.0 "upgraded in a HOME with spaces"
	inst -- --uninstall
	expect_rc 0
	expect_eq "$(tree "$TH")" "./.local d
./.local/bin d
./.local/lib d
./.local/share d
./.local/share/applications d" "the tree after uninstalling from a HOME with spaces"
}

TESTS="$TESTS t_launcher_foreign_backed_up"
t_launcher_foreign_backed_up() {
	printf '#!/bin/sh\necho "my own eitri wrapper"\n' >"$TH/.local/bin/eitri"
	chmod 0755 "$TH/.local/bin/eitri"
	mine=$(sha256sum <"$TH/.local/bin/eitri")
	serve 1.0.0
	inst_net
	expect_rc 0
	expect_out 'was not installed by this installer, so it is moved to'
	set -- "$TH"/.local/bin/eitri.bak-*
	expect_eq "$#" 1 "backups"
	expect_file "$1"
	expect_eq "$(sha256sum <"$1")" "$mine" "the backup's content"
	if ! grep -Fqx '# eitri-launcher v1' "$TH/.local/bin/eitri"; then fail "the new launcher has no marker"; fi
	# A second run finds the marker and replaces its own launcher, with no second backup.
	serve 1.1.0 1.0.0
	inst_net
	expect_rc 0
	set -- "$TH"/.local/bin/eitri.bak-*
	expect_eq "$#" 1 "backups after a second run"
	# A symlink is not the installer's either.
	rm "$TH/.local/bin/eitri"
	ln -s /nonexistent/eitri "$TH/.local/bin/eitri"
	serve 1.2.0 1.1.0
	inst_net
	expect_rc 0
	set -- "$TH"/.local/bin/eitri.bak-*
	expect_eq "$#" 2 "backups after a symlink was moved aside"
}

TESTS="$TESTS t_launcher_old_install_sh_replaced"
t_launcher_old_install_sh_replaced() {
	# The launcher the old root install.sh wrote (fixtures/old-install-sh-launcher, copied out of
	# its heredoc before plan Task 11 turns that script into a wrapper), recognised by its second
	# line (spec §6.5).
	cp "$FIXTURES/old-install-sh-launcher" "$TH/.local/bin/eitri"
	expect_eq "$(sed -n 2p "$TH/.local/bin/eitri")" \
		'# neovibe, installed by install.sh. Everything it decides is printed before the window opens.' \
		"the fixture is the old install.sh launcher"
	serve 1.0.0
	inst_net
	expect_rc 0
	expect_no_out 'was not installed by this installer'
	set -- "$TH"/.local/bin/eitri.bak-*
	expect_eq "$1" "$TH/.local/bin/eitri.bak-*" "no backup of the old install.sh launcher"
	if ! grep -Fqx '# eitri-launcher v1' "$TH/.local/bin/eitri"; then fail "the old launcher was not replaced"; fi
}

TESTS="$TESTS t_gtk_412_ubuntu_hint"
t_gtk_412_ubuntu_hint() {
	serve 1.0.0
	inst_net --set "EITRI_INSTALL_TEST_LIBDIRS=$S/libs/gtk412" --set "EITRI_INSTALL_TEST_OS_RELEASE=$S/osrel/ubuntu-22.04" --
	expect_fail "GTK 4.12"
	expect_out 'this system has GTK 4.12.3, and Eitri needs GTK 4.14 or newer'
	expect_out 'Ubuntu 22.04 ships GTK 4.12.3; Ubuntu 24.04 or newer has 4.14: upgrade to it'
	expect_absent "$TH/.local/lib/eitri"
	expect_absent "$TH/.cache"
}

TESTS="$TESTS t_missing_webkit_fedora_hint"
t_missing_webkit_fedora_hint() {
	serve 1.0.0
	inst_net --set "EITRI_INSTALL_TEST_LIBDIRS=$S/libs/nowebkit" --set "EITRI_INSTALL_TEST_OS_RELEASE=$S/osrel/fedora-41" --
	expect_fail "no WebKitGTK"
	expect_out 'WebKitGTK 6.0 (libwebkitgtk-6.0.so.4) was not found'
	expect_out 'sudo dnf install gtk4 webkitgtk6.0'
	expect_absent "$TH/.local/lib/eitri"
	# And the Ubuntu hint for missing GTK altogether.
	inst_net --set "EITRI_INSTALL_TEST_LIBDIRS=$T/nolibs" --
	expect_fail "no GTK"
	expect_out 'sudo apt install libgtk-4-1 libwebkitgtk-6.0-4'
}

TESTS="$TESTS t_old_glibc"
t_old_glibc() {
	serve 1.0.0
	inst_net --stubs "$S/stubs-oldglibc" --
	expect_fail "glibc 2.35"
	expect_out 'this system has 2.35'
	expect_out 'Ubuntu 24.04, Debian 13, Fedora 40'
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_bad_options_and_help"
t_bad_options_and_help() {
	# --nvim-only, --from-source and --with-nvim used to exit 2 here, "not in this installer yet"
	# (plan Task 11 implements them for real now; test_nvim.sh and test_from_source.sh exercise them).
	inst -- --bogus
	expect_rc 1
	expect_out 'unknown option: --bogus'
	inst -- --purge
	expect_fail "--purge without --uninstall"
	inst -- --help
	expect_rc 0
	expect_out '--uninstall [--purge]'
	expect_out '--from-source [--checkout DIR]'
	expect_absent "$TH/.cache"
}

TESTS="$TESTS t_help_has_no_internal_references"
t_help_has_no_internal_references() {
	# leaks-claude-1, docs-claude-4: --help used to show "spec §7" and "(the owner's dev loop, D11)"
	# -- internal design-doc/ruling references with no meaning to a user running this installer.
	inst -- --help
	expect_rc 0
	expect_no_out 'spec §'
	expect_no_out ', D11)'
}
