# The AppArmor user-namespace restriction (docs/superpowers/plans/2026-09-28-v1-dist-ubuntu-userns.md):
# where /proc/sys/kernel/apparmor_restrict_unprivileged_userns reads 1, the installer writes this
# install's profile under its data directory and prints the one-time sudo steps at the end; it never
# runs sudo itself (after_each's forbidden-command check). A fake /proc and /etc/apparmor.d stand in
# for the host's (EITRI_INSTALL_TEST_PROC / EITRI_INSTALL_TEST_APPARMOR_D). Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

# aa_proc VALUE: a fake /proc whose restriction sysctl reads VALUE; prints its path.
aa_proc() {
	_aap=$T/proc-$1
	mkdir -p "$_aap/sys/kernel"
	printf '%s\n' "$1" >"$_aap/sys/kernel/apparmor_restrict_unprivileged_userns"
	printf '%s\n' "$_aap"
}

# aa_expected NAME EXE: packaging/apparmor/eitri for NAME and EXE -- the one profile text, with the
# two lines a per-user install renders differently. EXE must hold no character sed or AppArmor
# treats specially (these tests' own paths do not); shell/src/webkit_sandbox.rs's tests cover those.
aa_expected() {
	sed -e "s|^profile eitri \"/usr/lib/eitri/shell\" flags=(unconfined) {\$|profile $1 \"$2\" flags=(unconfined) {|" \
		-e "s|<local/eitri>|<local/$1>|" "$PKG/apparmor/eitri"
}

# aa_q WORD: WORD as the steps print it (install.sh's sh_quote, restated for the expectation).
aa_q() {
	case $1 in
	*[!abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_./-]*) printf "'%s'\n" "$1" ;;
	*) printf '%s\n' "$1" ;;
	esac
}

# aa_inst ARGS: inst_net with the restriction on and an empty fake /etc/apparmor.d.
aa_inst() {
	mkdir -p "$T/apparmor.d"
	inst_net --set "EITRI_INSTALL_TEST_PROC=$(aa_proc 1)" --set "EITRI_INSTALL_TEST_APPARMOR_D=$T/apparmor.d" "$@"
}

# line_of TEXT: the line number of TEXT's first appearance in the last run's output.
line_of() {
	grep -n -F -e "$1" "$OUT" | head -n 1 | cut -d: -f1
}

TESTS="$TESTS t_apparmor_steps_when_restricted"
t_apparmor_steps_when_restricted() {
	serve 1.0.0
	aa_inst --
	expect_rc 0
	name=eitri-user-$(id -u)
	src=$TH/.local/share/eitri/apparmor/$name
	lib=$(cd -P "$TH/.local/lib/eitri" && pwd -P)
	expect_file "$src"
	expect_eq "$(cat "$src")" "$(aa_expected "$name" "$lib/shell")" "the rendered profile"
	expect_eq "$(tail -c 1 "$src" | od -An -c | tr -d ' ')" '\n' "the profile's last byte"
	expect_out "needs an AppArmor profile for this install. It is written to $src; install and load it once:"
	expect_out "eitri:     sudo install -m 0644 $(aa_q "$src") $(aa_q "$T/apparmor.d/$name")"
	expect_out "eitri:     sudo apparmor_parser -r $(aa_q "$T/apparmor.d/$name")"
	expect_out 'WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 also works, but that removes the operating system'"'"'s sandbox from the process that renders the model'"'"'s output'
	# At the end: after the line that says the install is done, and nothing after the steps.
	installed=$(line_of 'installed Eitri 1.0.0 into')
	steps=$(line_of 'sudo install -m 0644')
	if [ -z "$installed" ] || [ -z "$steps" ] || [ "$installed" -ge "$steps" ]; then
		fail "the steps are not after the install's last line (installed at ${installed:-none}, steps at ${steps:-none})"
	fi
	case $(tail -n 1 "$OUT") in
	'eitri: until then Eitri shows these steps in the agent panel'*) ;;
	*) fail "the last line is not the steps' own: $(tail -n 1 "$OUT")" ;;
	esac
	# Nothing else in the tree: the three entries of the profile's own directory.
	expect_eq "$(tree "$TH" | grep -v '^\./\.local/share/eitri')" "$EXPECTED_TREE" "the rest of the tree"
	expect_eq "$(tree "$TH" | grep '^\./\.local/share/eitri')" "./.local/share/eitri d
./.local/share/eitri/apparmor d
./.local/share/eitri/apparmor/$name f" "the profile's directory"
}

TESTS="$TESTS t_apparmor_nothing_without_the_restriction"
t_apparmor_nothing_without_the_restriction() {
	serve 1.0.0
	# 0 (the sysctl present and off), no such file (Fedora, Arch, Debian), and test mode's own
	# default (a /proc that does not exist): no profile, no word about AppArmor.
	mkdir -p "$T/emptyproc"
	for p in "$(aa_proc 0)" "$T/emptyproc" default; do
		use_home "$T/home-${p##*/}"
		if [ "$p" = default ]; then
			inst_net --
		else
			inst_net --set "EITRI_INSTALL_TEST_PROC=$p" --
		fi
		expect_rc 0
		expect_no_out 'AppArmor'
		expect_no_out 'sudo'
		expect_eq "$(tree "$TH")" "$EXPECTED_TREE" "the tree with /proc $p"
	done
}

TESTS="$TESTS t_apparmor_profile_already_in_place"
t_apparmor_profile_already_in_place() {
	serve 1.0.0
	name=eitri-user-$(id -u)
	mkdir -p "$T/apparmor.d"
	# Where the install will be, resolved as the installer resolves it (nothing under $TH is a link).
	lib=$(cd -P "$TH" && pwd -P)/.local/lib/eitri
	aa_expected "$name" "$lib/shell" >"$T/apparmor.d/$name"
	aa_inst --
	expect_rc 0
	expect_out "the AppArmor profile $T/apparmor.d/$name for this install is in place; it loads at boot, or now with: sudo apparmor_parser -r $(aa_q "$T/apparmor.d/$name")"
	expect_no_out 'sudo install'
	expect_no_out 'install and load it once'
	# A different file there (another path, an older text) is not "in place": the steps again.
	use_home "$T/home2"
	aa_inst --
	expect_rc 0
	expect_out 'sudo install -m 0644'
}

TESTS="$TESTS t_apparmor_up_to_date_rerun_says_it_again"
t_apparmor_up_to_date_rerun_says_it_again() {
	serve 1.0.0
	name=eitri-user-$(id -u)
	src=$TH/.local/share/eitri/apparmor/$name
	aa_inst --
	expect_rc 0
	plant_sidecar aaaaaaa
	# An install whose profile was never written (an installer from before these steps existed):
	# the re-run that finds it up to date writes it and says the steps, last.
	rm -r "$TH/.local/share/eitri/apparmor"
	aa_inst --
	expect_rc 0
	expect_out 'Eitri 1.0.0 is up to date'
	expect_file "$src"
	expect_out "eitri:     sudo install -m 0644 $(aa_q "$src") $(aa_q "$T/apparmor.d/$name")"
	case $(tail -n 1 "$OUT") in
	'eitri: until then Eitri shows these steps in the agent panel'*) ;;
	*) fail "the last line is not the steps' own: $(tail -n 1 "$OUT")" ;;
	esac
	# Once the steps were run: the same re-run says the profile is in place, and no steps.
	cp "$src" "$T/apparmor.d/$name"
	aa_inst --
	expect_rc 0
	expect_out 'Eitri 1.0.0 is up to date'
	expect_out "the AppArmor profile $T/apparmor.d/$name for this install is in place"
	expect_no_out 'sudo install'
}

TESTS="$TESTS t_apparmor_dry_run_writes_nothing"
t_apparmor_dry_run_writes_nothing() {
	serve 1.0.0
	aa_inst -- --dry-run
	expect_rc 0
	name=eitri-user-$(id -u)
	expect_out "needs an AppArmor profile for this install: would write it to $TH/.local/share/eitri/apparmor/$name"
	expect_no_out 'sudo install'
	expect_absent "$TH/.local/share/eitri"
}

TESTS="$TESTS t_apparmor_home_with_spaces_is_quoted"
t_apparmor_home_with_spaces_is_quoted() {
	use_home "$T/a home"
	serve 1.0.0
	aa_inst --
	expect_rc 0
	name=eitri-user-$(id -u)
	src=$TH/.local/share/eitri/apparmor/$name
	lib=$(cd -P "$TH/.local/lib/eitri" && pwd -P)
	expect_out "eitri:     sudo install -m 0644 '$src' $(aa_q "$T/apparmor.d/$name")"
	expect_eq "$(sed -n 's/^profile .* flags/&/p' "$src")" "profile $name \"$lib/shell\" flags=(unconfined) {" "the attachment line"
}

TESTS="$TESTS t_apparmor_uninstall"
t_apparmor_uninstall() {
	serve 1.0.0
	aa_inst --
	expect_rc 0
	name=eitri-user-$(id -u)
	expect_file "$TH/.local/share/eitri/apparmor/$name"
	# As if the steps were run: the installed copy is root's, and stays.
	cp "$TH/.local/share/eitri/apparmor/$name" "$T/apparmor.d/$name"
	inst --set "EITRI_INSTALL_TEST_APPARMOR_D=$T/apparmor.d" -- --uninstall
	expect_rc 0
	expect_out "removing $TH/.local/share/eitri/apparmor"
	expect_out "the AppArmor profile $T/apparmor.d/$name stays: remove it with: sudo apparmor_parser -R $(aa_q "$T/apparmor.d/$name") && sudo rm $(aa_q "$T/apparmor.d/$name")"
	expect_absent "$TH/.local/share/eitri"
	expect_file "$T/apparmor.d/$name"
	# With nothing under /etc/apparmor.d, nothing is said about it.
	inst_net --set "EITRI_INSTALL_TEST_PROC=$(aa_proc 1)" --set "EITRI_INSTALL_TEST_APPARMOR_D=$T/none" --
	expect_rc 0
	inst --set "EITRI_INSTALL_TEST_APPARMOR_D=$T/none" -- --uninstall
	expect_rc 0
	expect_no_out 'stays: remove it with'
	expect_absent "$TH/.local/share/eitri"
}
