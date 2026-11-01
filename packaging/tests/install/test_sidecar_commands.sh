# `eitri sidecar status` and `eitri sidecar use`, and what an update prunes: --sidecar-status,
# --sidecar-use. Sourced by harness.sh, after test_sidecar_update.sh, whose fixtures these share.
# What `selected` may name, what is removed from updates/ and what is left there, the data directory
# by Rust's rule, and an Eitri upgrade that left the bundled sidecar unbuilt.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

cmd_status() { inst "$@" -- --sidecar-status; }
cmd_use() {
	_cu=$1
	shift
	inst "$@" -- --sidecar-use "$_cu"
}

# cmd_speak DIR VERSION SDK TESTED: the sidecar in DIR answers --version --json as a real one does, on
# one line. TESTED is the inside of tested_cli_versions. The --version line stays what BUILD says.
cmd_speak() {
	_cs_line=$(sed -n 's/^SIDECAR_VERSION_LINE=//p' "$1/BUILD")
	cat >"$1/verdandi-claude-sidecar" <<SIDECAR_STUB
#!/bin/sh
if [ "\${2-}" = --json ]; then
	printf '%s\n' '{"v":1,"sidecar_version":"$2","protocol":{"major":3,"minor":24},"node":"v22.23.2","build_stamp":"0123456789abcdef","sdk_version":"$3","tested_cli_versions":[$4],"executables":["host_cli"]}'
	exit 0
fi
printf '%s\n' "$_cs_line"
SIDECAR_STUB
	chmod 0755 "$1/verdandi-claude-sidecar"
}

# cmd_lock: a live process holds the lock. Sets CMD_HOLDER; cmd_unlock ends it.
cmd_lock() {
	mkdir -p "$TH/.cache/eitri/lock"
	sleep 300 &
	CMD_HOLDER=$!
	echo "$CMD_HOLDER" >"$TH/.cache/eitri/lock/pid"
}
cmd_unlock() {
	kill "$CMD_HOLDER" 2>/dev/null
	wait "$CMD_HOLDER" 2>/dev/null
	rm -rf "${TH:?}/.cache/eitri"
}

# cmd_unbuilt: the state a .deb upgrade leaves: this RELEASE's <rev7> was never built.
cmd_unbuilt() {
	rm -rf "${UPD_ROOT:?}/aaaaaaa"
}

# ---------------------------------------------------------------------------------------------
# status

TESTS="$TESTS t_cmd_status_bundled_only"
t_cmd_status_bundled_only() {
	upd_setup
	cmd_speak "$UPD_ROOT/aaaaaaa" 0.2.0 0.3.252 '"2.1.283"'
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   bundled 0.2.0 (protocol 3.24, Agent SDK 0.3.252)'
	expect_out 'bundled with Eitri     0.2.0 (protocol 3.24)'
	expect_no_out 'the fallback'
	expect_no_out 'selected'
	expect_no_out 'kept'
	expect_no_out 'eitri: '
}

TESTS="$TESTS t_cmd_status_update_selected"
t_cmd_status_update_selected() {
	upd_setup
	plant_update 0.3.1
	plant_update 0.3.2
	plant_selected 0.3.2
	cmd_speak "$UPD_ROOT/aaaaaaa" 0.2.0 0.3.252 '"2.1.283"'
	cmd_speak "$UPD_ROOT/updates/0.3.2" 0.3.2 0.3.311 '"2.1.283"'
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   update 0.3.2 (protocol 3.24, Agent SDK 0.3.311)'
	expect_out 'bundled with Eitri     0.2.0 (protocol 3.24), the fallback'
	expect_out 'kept                   0.3.1'
	expect_no_out 'unused'
}

# The sidecar an Eitri upgrade moved past stays selected, and says why nothing reads it.
TESTS="$TESTS t_cmd_status_selected_older_than_bundled"
t_cmd_status_selected_older_than_bundled() {
	upd_setup
	plant_update 0.1.9
	plant_selected 0.1.9
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   bundled 0.2.0'
	expect_out 'selected               0.1.9, older than the bundled sidecar, so unused'
	expect_no_out 'update 0.1.9'
	expect_no_out 'the fallback'
}

TESTS="$TESTS t_cmd_status_selected_same_as_bundled"
t_cmd_status_selected_same_as_bundled() {
	upd_setup
	plant_update 0.2.0
	plant_selected 0.2.0
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   bundled 0.2.0'
	expect_out 'selected               0.2.0, the same as the bundled sidecar, so unused'
}

TESTS="$TESTS t_cmd_status_selected_incomplete"
t_cmd_status_selected_incomplete() {
	upd_setup
	plant_update 0.3.2
	plant_selected 0.3.2
	rm "$UPD_ROOT/updates/0.3.2/BUILD"
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   bundled 0.2.0'
	expect_out 'selected               0.3.2, not complete, so unused'
}

# What a tab does with such a file is the rule here: nothing checks the --version line at a spawn.
TESTS="$TESTS t_cmd_status_selected_differs_from_its_build"
t_cmd_status_selected_differs_from_its_build() {
	upd_setup
	plant_update 0.3.2
	plant_selected 0.3.2
	sed -i 's/^SIDECAR_VERSION_LINE=.*/SIDECAR_VERSION_LINE=something else/' "$UPD_ROOT/updates/0.3.2/BUILD"
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   update 0.3.2'
	expect_out 'selected               0.3.2, which does not match its BUILD file'
}

TESTS="$TESTS t_cmd_status_selected_is_junk"
t_cmd_status_selected_is_junk() {
	upd_setup
	for text in 'junk' 'version=../x' "version=0.3.2${NL}and more" "version=0.3.2${NL}" 'version=0.03.2' \
		'version=0.3.2-rc.01' 'version=99999999999999999999999.0.0'; do
		printf '%s\n' "$text" >"$UPD_ROOT/selected"
		cmd_status
		expect_rc 0
		expect_out 'running for new tabs   bundled 0.2.0'
		expect_out 'selected               names no version, so unused'
		expect_no_out '../x'
		expect_no_out 'and more'
	done
}

# Whatever a tab calls unusable, status does too: all of the file counts, not the first line of it.
TESTS="$TESTS t_cmd_status_reads_the_file_as_a_tab_does"
t_cmd_status_reads_the_file_as_a_tab_does() {
	upd_setup
	plant_update 0.3.2
	{
		printf 'version=0.3.2'
		printf '%300s' '' | tr ' ' '\n'
		echo junk
	} >"$UPD_ROOT/selected"
	cmd_status
	expect_rc 0
	expect_out 'selected               names no version, so unused'
	printf 'version=0.3.2\000\n' >"$UPD_ROOT/selected"
	cmd_status
	expect_rc 0
	expect_out 'selected               names no version, so unused'
	printf 'version=0.3.2' >"$UPD_ROOT/selected"
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   update 0.3.2'
}

# One version has one spelling: a directory named another way is not a version.
TESTS="$TESTS t_cmd_status_second_spelling_is_no_update"
t_cmd_status_second_spelling_is_no_update() {
	upd_setup
	plant_update 0.03.1
	plant_selected 0.03.1
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   bundled 0.2.0'
	expect_out 'selected               names no version, so unused'
	expect_no_out 'kept'
	cmd_use 0.03.1
	expect_fail "use of a second spelling"
	expect_out 'expected bundled or a version such as 0.3.2'
}

TESTS="$TESTS t_cmd_status_bundled_unbuilt"
t_cmd_status_bundled_unbuilt() {
	upd_setup
	cmd_unbuilt
	cmd_status
	expect_rc 0
	expect_out 'bundled 0.2.0: not built yet, run eitri setup'
	expect_out 'running for new tabs   nothing yet, run eitri setup'
	expect_no_out 'the fallback'
}

# A package that ships its sidecar: a sibling with no BUILD counts as bundled.
TESTS="$TESTS t_cmd_status_sibling_is_bundled"
t_cmd_status_sibling_is_bundled() {
	upd_setup
	cmd_unbuilt
	printf '#!/bin/sh\necho "%s"\n' "$SIDECAR_LINE" >"$UPD_LIB/verdandi-claude-sidecar"
	chmod 0755 "$UPD_LIB/verdandi-claude-sidecar"
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   bundled 0.2.0'
	expect_no_out 'not built yet'
}

TESTS="$TESTS t_cmd_status_claude_tested"
t_cmd_status_claude_tested() {
	upd_setup
	cmd_speak "$UPD_ROOT/aaaaaaa" 0.2.0 0.3.252 '"2.1.250","2.1.283"'
	cmd_status
	expect_rc 0
	expect_out 'claude on this machine 2.1.283, tested with this sidecar'
	expect_no_out 'not among'
}

TESTS="$TESTS t_cmd_status_claude_untested"
t_cmd_status_claude_untested() {
	upd_setup
	cmd_speak "$UPD_ROOT/aaaaaaa" 0.2.0 0.3.252 '"2.1.250","12.1.283","2.1.2830"'
	cmd_status
	expect_rc 0
	expect_out 'claude on this machine 2.1.283, not among the versions this sidecar was tested with'
}

# The sidecar that runs answers, not the bundled one.
TESTS="$TESTS t_cmd_status_claude_judged_by_the_running_sidecar"
t_cmd_status_claude_judged_by_the_running_sidecar() {
	upd_setup
	plant_update 0.3.2
	plant_selected 0.3.2
	cmd_speak "$UPD_ROOT/aaaaaaa" 0.2.0 0.3.252 '"2.1.283"'
	cmd_speak "$UPD_ROOT/updates/0.3.2" 0.3.2 0.3.311 '"2.1.300"'
	cmd_status
	expect_rc 0
	expect_out 'claude on this machine 2.1.283, not among the versions this sidecar was tested with'
}

# A sidecar with no --json at all reports no list, so nothing is claimed about one.
TESTS="$TESTS t_cmd_status_claude_with_no_list"
t_cmd_status_claude_with_no_list() {
	upd_setup
	cmd_status
	expect_rc 0
	expect_out 'claude on this machine 2.1.283'
	expect_no_out 'tested'
	expect_out 'running for new tabs   bundled 0.2.0'
	expect_no_out 'protocol'
}

TESTS="$TESTS t_cmd_status_no_network_no_lock"
t_cmd_status_no_network_no_lock() {
	upd_state
	cmd_lock
	lock_before=$(snap "$TH/.cache/eitri")
	srv_mark
	cmd_status --path-tail "$S/path-no-curl"
	expect_rc 0
	expect_out 'running for new tabs   update 0.3.0'
	expect_no_out 'another Eitri installer'
	expect_eq "$(srv_paths)" '' "requests made by status"
	expect_eq "$(snap "$TH/.cache/eitri")" "$lock_before" "the cache with the lock held"
	expect_eq "$(snap "$UPD_ROOT")" "$UPD_SNAP" "the sidecar directory after status"
	cmd_unlock
	cmd_status
	expect_rc 0
	expect_absent "$TH/.cache/eitri"
}

# Tabs read a data directory the installer would not write to, so status reads it too.
TESTS="$TESTS t_cmd_status_data_home_outside_home"
t_cmd_status_data_home_outside_home() {
	upd_setup
	plant_sidecar aaaaaaa "$T/outside"
	UPD_ROOT=$T/outside/eitri/sidecar
	plant_update 0.3.2
	plant_selected 0.3.2
	cmd_status --set "XDG_DATA_HOME=$T/outside"
	expect_rc 0
	expect_out 'running for new tabs   update 0.3.2'
}

# A named binary or checkout wins over an update, and a tab never falls back from one.
TESTS="$TESTS t_cmd_status_named_program_wins"
t_cmd_status_named_program_wins() {
	upd_state
	cmd_status --set "EITRI_SIDECAR_BINARY=$T/my-sidecar"
	expect_rc 0
	expect_out "running for new tabs   $T/my-sidecar, named by EITRI_SIDECAR_BINARY"
	expect_no_out 'running for new tabs   update'
	expect_no_out 'the fallback'
	cmd_status --set "EITRI_VERDANDI_CHECKOUT=$T/verdandi"
	expect_rc 0
	expect_out "running for new tabs   the checkout $T/verdandi, named by EITRI_VERDANDI_CHECKOUT"
	expect_no_out 'running for new tabs   update'
	# Blank is not named, as a tab reads it.
	cmd_status --set 'EITRI_SIDECAR_BINARY=  '
	expect_rc 0
	expect_out 'running for new tabs   update 0.3.0'
}

TESTS="$TESTS t_cmd_status_needs_the_contract_keys"
t_cmd_status_needs_the_contract_keys() {
	upd_setup
	sed -i -e '/^SIDECAR_/d' "$UPD_LIB/RELEASE"
	cmd_status
	expect_fail "a RELEASE with no sidecar contract"
	expect_out 'cannot take sidecar updates'
}

# ---------------------------------------------------------------------------------------------
# use bundled

TESTS="$TESTS t_cmd_use_bundled_removes_selected"
t_cmd_use_bundled_removes_selected() {
	upd_state
	srv_mark
	cmd_use bundled --path-tail "$S/path-no-curl"
	expect_rc 0
	expect_absent "$UPD_ROOT/selected"
	expect_exec "$UPD_ROOT/updates/0.3.0/verdandi-claude-sidecar"
	expect_out 'New Claude tabs use the bundled sidecar 0.2.0. Update 0.3.0 stays built: eitri sidecar use 0.3.0'
	expect_eq "$(srv_paths)" '' "requests made by use"
	expect_absent "$TH/.cache/eitri"
}

TESTS="$TESTS t_cmd_use_bundled_with_nothing_selected"
t_cmd_use_bundled_with_nothing_selected() {
	upd_setup
	cmd_use bundled
	expect_rc 0
	expect_out 'New Claude tabs use the bundled sidecar 0.2.0.'
	expect_no_out 'stays built'
}

TESTS="$TESTS t_cmd_use_bundled_with_a_selected_update_gone"
t_cmd_use_bundled_with_a_selected_update_gone() {
	upd_setup
	plant_selected 0.3.0
	cmd_use bundled
	expect_rc 0
	expect_absent "$UPD_ROOT/selected"
	expect_no_out 'stays built'
}

TESTS="$TESTS t_cmd_use_bundled_junk_selected_goes"
t_cmd_use_bundled_junk_selected_goes() {
	upd_setup
	printf 'junk\n' >"$UPD_ROOT/selected"
	cmd_use bundled
	expect_rc 0
	expect_absent "$UPD_ROOT/selected"
}

# A symlink at `selected` goes as a link: what it pointed at is someone else's.
TESTS="$TESTS t_cmd_use_bundled_leaves_a_link_target"
t_cmd_use_bundled_leaves_a_link_target() {
	upd_setup
	printf 'mine\n' >"$T/mine"
	ln -s "$T/mine" "$UPD_ROOT/selected"
	cmd_use bundled
	expect_rc 0
	expect_absent "$UPD_ROOT/selected"
	expect_eq "$(cat "$T/mine")" mine "what the link led to"
}

TESTS="$TESTS t_cmd_use_bundled_selected_is_a_directory"
t_cmd_use_bundled_selected_is_a_directory() {
	upd_setup
	mkdir -p "$UPD_ROOT/selected/x"
	cmd_use bundled
	expect_fail "selected is a directory"
	expect_out 'is a directory'
	expect_dir "$UPD_ROOT/selected/x"
}

# ---------------------------------------------------------------------------------------------
# use <version>

TESTS="$TESTS t_cmd_use_version_selects_it"
t_cmd_use_version_selects_it() {
	upd_state
	plant_update 0.3.1
	srv_mark
	cmd_use 0.3.1 --path-tail "$S/path-no-curl"
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected"
	expect_out 'New Claude tabs use 0.3.1.'
	expect_out 'Open tabs keep the sidecar they started with.'
	expect_out 'To go back: eitri sidecar use bundled'
	# Nothing is pruned: what `use` went back from stays built to come back to.
	expect_exec "$UPD_ROOT/updates/0.3.0/verdandi-claude-sidecar"
	expect_eq "$(srv_paths)" '' "requests made by use"
	expect_eq "$(find "$UPD_ROOT" -name '.*' | wc -l)" 0 "temporary names left"
	expect_absent "$TH/.cache/eitri"
}

TESTS="$TESTS t_cmd_use_rc_by_name"
t_cmd_use_rc_by_name() {
	upd_setup
	plant_update 0.3.1-rc.2
	cmd_use 0.3.1-rc.2
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1-rc.2' "selected"
}

TESTS="$TESTS t_cmd_use_older_refused"
t_cmd_use_older_refused() {
	upd_state
	plant_update 0.1.9
	plant_update 0.2.0
	plant_update 0.2.0-rc.1
	UPD_SNAP=$(snap "$UPD_ROOT")
	for v in 0.1.9 0.2.0 0.2.0-rc.1; do
		cmd_use "$v"
		upd_refused "use $v" "is not newer than the bundled sidecar (0.2.0)"
	done
}

TESTS="$TESTS t_cmd_use_incomplete_refused"
t_cmd_use_incomplete_refused() {
	upd_state
	plant_update 0.3.1
	plant_update 0.3.2
	rm "$UPD_ROOT/updates/0.3.1/BUILD"
	rm "$UPD_ROOT/updates/0.3.2/verdandi-claude-sidecar"
	UPD_SNAP=$(snap "$UPD_ROOT")
	for v in 0.3.1 0.3.2; do
		cmd_use "$v"
		upd_refused "use $v" "update $v is not built here, or not all of it is"
	done
}

TESTS="$TESTS t_cmd_use_unknown_version_refused"
t_cmd_use_unknown_version_refused() {
	upd_state
	cmd_use 0.9.9
	upd_refused "a version never built" "update 0.9.9 is not built here, or not all of it is"
	expect_out 'eitri sidecar update --version 0.9.9'
}

TESTS="$TESTS t_cmd_use_bad_names_refused"
t_cmd_use_bad_names_refused() {
	upd_state
	mkdir -p "$UPD_ROOT/updates/x"
	UPD_SNAP=$(snap "$UPD_ROOT")
	for v in ../x ../../x x /etc 0.3 0.3.0.1 v0.3.0 0.3.0-rc 0.3.0-beta.1 '0.3.0/../0.3.0' ' 0.3.0' \
		00.3.0 0.03.0 0.3.00 0.3.0-rc.01 99999999999999999999999.0.0 \
		"0.3.0${NL}" "0.3.0${NL}x" Bundled ''; do
		cmd_use "$v"
		upd_refused "use [$v]" 'expected bundled or a version such as 0.3.2'
	done
}

TESTS="$TESTS t_cmd_use_build_line_mismatch_refused"
t_cmd_use_build_line_mismatch_refused() {
	upd_state
	plant_update 0.3.1
	sed -i 's/^SIDECAR_VERSION_LINE=.*/SIDECAR_VERSION_LINE=something else/' "$UPD_ROOT/updates/0.3.1/BUILD"
	UPD_SNAP=$(snap "$UPD_ROOT")
	cmd_use 0.3.1
	upd_refused "the binary and BUILD disagree" "update 0.3.1 is not built here, or not all of it is"
}

TESTS="$TESTS t_cmd_use_probe_refusal_refused"
t_cmd_use_probe_refusal_refused() {
	upd_state
	plant_update 0.3.1
	UPD_SNAP=$(snap "$UPD_ROOT")
	cmd_use 0.3.1 --set 'EITRI_INSTALL_TEST_PROBE_REFUSAL=sidecar 0.3.1 lacks permission_gate'
	upd_refused "the probe refused it" 'update 0.3.1 will not start here: sidecar 0.3.1 lacks permission_gate'
}

TESTS="$TESTS t_cmd_use_probe_saying_nothing_refused"
t_cmd_use_probe_saying_nothing_refused() {
	upd_state
	plant_update 0.3.1
	UPD_SNAP=$(snap "$UPD_ROOT")
	cmd_use 0.3.1 --set EITRI_INSTALL_TEST_PROBE_MUTE=1
	upd_refused "the probe said nothing" 'update 0.3.1 will not start here'
}

# A link in updates/ is nothing the update path made, and the binary behind it would run for every tab.
TESTS="$TESTS t_cmd_use_a_link_refused"
t_cmd_use_a_link_refused() {
	upd_state
	plant_update 0.3.1
	mv "$UPD_ROOT/updates/0.3.1" "$T/elsewhere-0.3.1"
	ln -s "$T/elsewhere-0.3.1" "$UPD_ROOT/updates/0.3.1"
	UPD_SNAP=$(snap "$UPD_ROOT")
	cmd_use 0.3.1
	upd_refused "a symlink in updates/" "update 0.3.1 is not built here, or not all of it is"
}

# mv onto a link to a directory would nest the file there and exit 0, having selected nothing.
TESTS="$TESTS t_cmd_use_selected_is_a_link_to_a_directory"
t_cmd_use_selected_is_a_link_to_a_directory() {
	upd_setup
	plant_update 0.3.1
	mkdir -p "$T/elsewhere"
	ln -s "$T/elsewhere" "$UPD_ROOT/selected"
	UPD_SNAP=$(snap "$UPD_ROOT")
	cmd_use 0.3.1
	upd_refused "selected is a link to a directory" 'is a directory, or a link to one'
	expect_eq "$(find "$T/elsewhere" -mindepth 1 | wc -l)" 0 "files written through the link"
}

# A link to a file, or to nothing, is replaced by the rename, and what it led to stays.
TESTS="$TESTS t_cmd_use_selected_is_a_link_to_a_file"
t_cmd_use_selected_is_a_link_to_a_file() {
	upd_setup
	plant_update 0.3.1
	echo mine >"$T/mine"
	for target in "$T/mine" "$T/nothing"; do
		rm -f "$UPD_ROOT/selected"
		ln -s "$target" "$UPD_ROOT/selected"
		cmd_use 0.3.1
		expect_rc 0
		if [ -L "$UPD_ROOT/selected" ]; then fail "the link to $target is still there"; fi
		expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected"
	done
	expect_eq "$(cat "$T/mine")" mine "what the link led to"
}

TESTS="$TESTS t_cmd_use_lock_held"
t_cmd_use_lock_held() {
	upd_state
	plant_update 0.3.1
	UPD_SNAP=$(snap "$UPD_ROOT")
	cmd_lock
	cmd_use 0.3.1
	expect_fail "use with the lock held"
	expect_out "another Eitri installer (pid $CMD_HOLDER) is running"
	expect_eq "$(snap "$UPD_ROOT")" "$UPD_SNAP" "the sidecar directory with the lock held"
	cmd_use bundled
	expect_fail "use bundled with the lock held"
	expect_out "another Eitri installer (pid $CMD_HOLDER) is running"
	expect_eq "$(snap "$UPD_ROOT")" "$UPD_SNAP" "the sidecar directory with the lock held"
	expect_file "$TH/.cache/eitri/lock/pid"
	cmd_unlock
}

TESTS="$TESTS t_cmd_refused_flags"
t_cmd_refused_flags() {
	upd_state
	plant_update 0.3.1
	UPD_SNAP=$(snap "$UPD_ROOT")
	for use in 0.3.1 bundled; do
		inst -- --sidecar-use "$use" --dry-run
		upd_refused "--dry-run on use $use" '--dry-run does not apply'
		inst -- --sidecar-use "$use" --version 0.3.1
		upd_refused "--version on use $use" '--version goes with eitri sidecar update and check'
		inst -- --sidecar-use "$use" --insecure-skip-signature
		upd_refused "--insecure-skip-signature on use $use" '--insecure-skip-signature does not apply'
		inst -- --sidecar-use "$use" --tarball "$T/x"
		upd_refused "--tarball on use $use" '--tarball, --sums and --sig do not apply'
	done
	inst -- --sidecar-status --dry-run
	upd_refused "--dry-run on status" '--dry-run does not apply'
	inst -- --sidecar-status --version 0.3.1
	upd_refused "--version on status" '--version goes with eitri sidecar update and check'
	inst -- --sidecar-use
	upd_refused "use with no argument" '--sidecar-use needs a value'
	inst -- --sidecar-use 0.3.1 --uninstall
	upd_refused "use beside another mode" 'cannot be combined'
}

# ---------------------------------------------------------------------------------------------
# Two commands at once

# cmd_slow_probe: the Eitri binary's probe waits, so a `use` can be held between its lock and its write.
cmd_slow_probe() {
	cat >"$UPD_LIB/shell" <<SHELL_STUB
#!/bin/sh
: >"$T/probe.started"
n=0
while [ ! -e "$T/probe.go" ] && [ "\$n" -lt 300 ]; do
	sleep 0.1
	n=\$((n + 1))
done
echo "ok 0.0.0 protocol 3.24"
SHELL_STUB
	chmod 0755 "$UPD_LIB/shell"
}

cmd_wait_for_probe() {
	_wp=0
	while [ ! -e "$T/probe.started" ] && [ "$_wp" -lt 300 ]; do
		sleep 0.1
		_wp=$((_wp + 1))
	done
	if [ ! -e "$T/probe.started" ]; then fail "the first command never reached its probe"; fi
}

TESTS="$TESTS t_cmd_two_uses_at_once"
t_cmd_two_uses_at_once() {
	upd_state
	plant_update 0.3.1
	plant_update 0.3.2
	cmd_slow_probe
	(
		OUT=$T/first.out
		RUNS=100
		cmd_use 0.3.1
		printf '%s\n' "$RC" >"$T/first.rc"
	) &
	first=$!
	cmd_wait_for_probe
	# The first holds the lock, with selected not yet written.
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.0' "selected while the first command runs"
	cmd_use 0.3.2
	expect_fail "a second use while the first runs"
	expect_out 'another Eitri installer (pid '
	expect_out 'is running: wait for it to finish and re-run'
	cmd_use bundled
	expect_fail "use bundled while the first runs"
	expect_out 'is running: wait for it to finish and re-run'
	# An update stops at the same lock, before it spends anything.
	mk_sidecar_release 0.3.5
	srv_mark
	upd_update
	expect_fail "an update while the first runs"
	expect_out 'is running: wait for it to finish and re-run'
	expect_eq "$(srv_paths)" '' "requests made by the refused update"
	# status neither waits nor refuses, and says what is true right now.
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   update 0.3.0'
	: >"$T/probe.go"
	wait "$first"
	expect_eq "$(cat "$T/first.rc")" 0 "the first command"
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected after both"
	expect_absent "$TH/.cache/eitri"
}

TESTS="$TESTS t_cmd_use_while_an_update_runs"
t_cmd_use_while_an_update_runs() {
	upd_state
	plant_update 0.3.2
	mk_sidecar_release 0.3.5
	cmd_slow_probe
	(
		OUT=$T/first.out
		RUNS=100
		upd_update
		printf '%s\n' "$RC" >"$T/first.rc"
	) &
	first=$!
	cmd_wait_for_probe
	cmd_use 0.3.2
	expect_fail "use while an update builds"
	expect_out 'is running: wait for it to finish and re-run'
	: >"$T/probe.go"
	wait "$first"
	expect_eq "$(cat "$T/first.rc")" 0 "the update"
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.5' "selected after both"
	expect_absent "$UPD_ROOT/updates/0.3.2"
}

# ---------------------------------------------------------------------------------------------
# What an update keeps

TESTS="$TESTS t_cmd_prune_keeps_selected_and_previous"
t_cmd_prune_keeps_selected_and_previous() {
	upd_state
	u=$UPD_ROOT/updates
	plant_update 0.2.8
	plant_update 0.3.1
	plant_update 0.3.2-rc.1
	# Not complete, but named like one: nothing keeps it.
	plant_update 0.3.3
	rm "$u/0.3.3/BUILD"
	# Names this installer never wrote stay.
	mkdir -p "$u/notes" "$u/0.3" "$u/0.3.1.1" "$u/v0.3.1" "$u/0.3.1-beta.1" "$u/0.03.1" "$u/.hidden"
	echo mine >"$u/notes/mine"
	echo mine >"$u/0.4.0"
	mkdir -p "$T/linked"
	echo mine >"$T/linked/keep"
	ln -s "$T/linked" "$u/0.4.1"
	# What a killed run left.
	mkdir -p "$u/.0.3.4.tmp.4242" "$u/.0.3.0.tmp.4243.aside"
	echo half >"$u/.0.3.4.tmp.4242/verdandi-claude-sidecar"
	printf 'version=0.3.4\n' >"$UPD_ROOT/.selected.tmp.4242"
	mk_sidecar_release 0.3.5
	upd_update
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.5' "selected"
	expect_exec "$u/0.3.5/verdandi-claude-sidecar"
	expect_exec "$u/0.3.0/verdandi-claude-sidecar"
	expect_file "$u/0.3.0/BUILD"
	for gone in 0.2.8 0.3.1 0.3.2-rc.1 0.3.3 .0.3.4.tmp.4242 .0.3.0.tmp.4243.aside; do
		expect_absent "$u/$gone"
	done
	expect_absent "$UPD_ROOT/.selected.tmp.4242"
	expect_file "$u/notes/mine"
	expect_file "$u/0.4.0"
	expect_file "$T/linked/keep"
	for kept in 0.3 0.3.1.1 v0.3.1 0.3.1-beta.1 0.03.1 .hidden; do
		expect_dir "$u/$kept"
	done
	if [ ! -L "$u/0.4.1" ]; then fail "the link in updates/ was removed"; fi
	expect_exec "$UPD_ROOT/aaaaaaa/verdandi-claude-sidecar"
}

# With nothing selected before there is no previous one, and every older update goes.
TESTS="$TESTS t_cmd_prune_with_nothing_selected_before"
t_cmd_prune_with_nothing_selected_before() {
	upd_setup
	plant_update 0.3.0
	plant_update 0.3.1
	mk_sidecar_release 0.3.5
	upd_update
	expect_rc 0
	expect_absent "$UPD_ROOT/updates/0.3.0"
	expect_absent "$UPD_ROOT/updates/0.3.1"
	expect_exec "$UPD_ROOT/updates/0.3.5/verdandi-claude-sidecar"
}

# Going down by name keeps what ran, the way going up does.
TESTS="$TESTS t_cmd_prune_after_a_named_older_version"
t_cmd_prune_after_a_named_older_version() {
	upd_state
	plant_selected 0.3.5
	plant_update 0.3.5
	plant_update 0.3.2
	mk_sidecar_release 0.3.1
	upd_version 0.3.1
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected"
	expect_exec "$UPD_ROOT/updates/0.3.5/verdandi-claude-sidecar"
	expect_exec "$UPD_ROOT/updates/0.3.1/verdandi-claude-sidecar"
	expect_absent "$UPD_ROOT/updates/0.3.2"
	expect_absent "$UPD_ROOT/updates/0.3.0"
}

# Every failure leaves updates/ alone, a killed run's leftovers and all: only a success prunes.
TESTS="$TESTS t_cmd_prune_only_after_a_success"
t_cmd_prune_only_after_a_success() {
	upd_state
	plant_update 0.3.1
	mkdir -p "$UPD_ROOT/updates/.0.3.4.tmp.4242"
	UPD_SNAP=$(snap "$UPD_ROOT")
	mk_sidecar_release 0.3.5
	upd_update --set 'EITRI_INSTALL_TEST_PROBE_REFUSAL=no'
	expect_fail "a probe refusal"
	expect_eq "$(snap "$UPD_ROOT")" "$UPD_SNAP" "the sidecar directory after a failure"
}

# A prune that cannot remove something has already selected the new update, which stands.
TESTS="$TESTS t_cmd_prune_failing_keeps_the_update"
t_cmd_prune_failing_keeps_the_update() {
	upd_state
	plant_update 0.3.1
	mkdir -p "$T/rmfail"
	cat >"$T/rmfail/rm" <<'RM_STUB'
#!/bin/sh
for a do
	case $a in */updates/0.3.1) echo "rm: refused" >&2; exit 1 ;; esac
done
for m in /usr/bin/rm /bin/rm; do
	if [ -x "$m" ]; then exec "$m" "$@"; fi
done
exit 127
RM_STUB
	chmod 0755 "$T/rmfail/rm"
	mk_sidecar_release 0.3.5
	upd_update --stubs "$T/rmfail"
	expect_rc 0
	expect_out "could not remove $UPD_ROOT/updates/0.3.1"
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.5' "selected"
	expect_out 'sidecar 0.3.5 built, and this Eitri accepts it.'
}

# A sidecar directory that leads elsewhere is the user's: nothing behind it is judged by name.
TESTS="$TESTS t_cmd_prune_through_a_foreign_root"
t_cmd_prune_through_a_foreign_root() {
	upd_state
	plant_update 0.3.1
	mv "$UPD_ROOT" "$T/not-that-name"
	ln -s "$T/not-that-name" "$UPD_ROOT"
	mk_sidecar_release 0.3.5
	upd_update
	expect_rc 0
	expect_out 'not to a directory named sidecar'
	expect_exec "$T/not-that-name/updates/0.3.1/verdandi-claude-sidecar"
	expect_eq "$(cat "$T/not-that-name/selected")" 'version=0.3.5' "selected"
}

# And a link that leads to a directory really named sidecar is followed, as prune_sidecars follows it.
TESTS="$TESTS t_cmd_prune_through_a_moved_root"
t_cmd_prune_through_a_moved_root() {
	upd_state
	plant_update 0.3.1
	mkdir -p "$T/disk"
	mv "$UPD_ROOT" "$T/disk/sidecar"
	ln -s "$T/disk/sidecar" "$UPD_ROOT"
	mk_sidecar_release 0.3.5
	upd_update
	expect_rc 0
	expect_absent "$T/disk/sidecar/updates/0.3.1"
	expect_exec "$T/disk/sidecar/updates/0.3.0/verdandi-claude-sidecar"
}

TESTS="$TESTS t_cmd_prune_through_a_linked_updates"
t_cmd_prune_through_a_linked_updates() {
	upd_state
	plant_update 0.3.1
	mv "$UPD_ROOT/updates" "$T/somewhere"
	ln -s "$T/somewhere" "$UPD_ROOT/updates"
	mk_sidecar_release 0.3.5
	upd_update
	expect_rc 0
	expect_out 'is a symlink, so nothing behind it was removed'
	expect_exec "$T/somewhere/0.3.1/verdandi-claude-sidecar"
}

# ---------------------------------------------------------------------------------------------
# The data directory, by the rule a tab and the launcher use

TESTS="$TESTS t_cmd_data_home_unset_relative_or_empty"
t_cmd_data_home_unset_relative_or_empty() {
	upd_setup
	plant_update 0.3.1
	plant_selected 0.3.1
	for how in unset empty relative/dir relative/data; do
		case $how in
		unset) set -- --unset XDG_DATA_HOME ;;
		empty) set -- --set XDG_DATA_HOME= ;;
		*) set -- --set "XDG_DATA_HOME=$how" ;;
		esac
		cmd_status "$@"
		expect_rc 0
		expect_out 'running for new tabs   update 0.3.1'
		cmd_use bundled "$@"
		expect_rc 0
		expect_absent "$UPD_ROOT/selected"
		cmd_use 0.3.1 "$@"
		expect_rc 0
		expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected, XDG_DATA_HOME $how"
	done
	expect_absent "$T/cwd/relative"
}

TESTS="$TESTS t_cmd_data_home_absolute"
t_cmd_data_home_absolute() {
	upd_setup
	plant_update 0.3.1
	plant_selected 0.3.1
	other=$TH/data
	plant_sidecar aaaaaaa "$other"
	home_root=$UPD_ROOT
	UPD_ROOT=$other/eitri/sidecar
	plant_update 0.3.2
	plant_selected 0.3.2
	cmd_status --set "XDG_DATA_HOME=$other"
	expect_rc 0
	expect_out 'running for new tabs   update 0.3.2'
	expect_no_out 'update 0.3.1'
	cmd_use bundled --set "XDG_DATA_HOME=$other"
	expect_rc 0
	expect_absent "$other/eitri/sidecar/selected"
	expect_eq "$(cat "$home_root/selected")" 'version=0.3.1' "selected under ~/.local/share"
	# Changing the variable between two commands is two data directories, as it is for a window.
	cmd_use 0.3.2 --set "XDG_DATA_HOME=$other"
	expect_rc 0
	expect_eq "$(cat "$other/eitri/sidecar/selected")" 'version=0.3.2' "selected under XDG_DATA_HOME"
	expect_eq "$(cat "$home_root/selected")" 'version=0.3.1' "selected under ~/.local/share"
}

TESTS="$TESTS t_cmd_use_outside_home_refused"
t_cmd_use_outside_home_refused() {
	upd_setup
	plant_sidecar aaaaaaa "$T/outside"
	UPD_ROOT=$T/outside/eitri/sidecar
	plant_update 0.3.2
	cmd_use 0.3.2 --set "XDG_DATA_HOME=$T/outside"
	expect_fail "use writing outside HOME"
	expect_out 'is not inside'
	expect_absent "$UPD_ROOT/selected"
}

# ---------------------------------------------------------------------------------------------
# A .deb or .rpm upgrade: the new bundled sidecar stays unbuilt until `eitri setup`

# The update is newer than the new bundled sidecar: it runs, with nothing to fall back to.
TESTS="$TESTS t_cmd_deb_update_newer_than_unbuilt_bundled"
t_cmd_deb_update_newer_than_unbuilt_bundled() {
	upd_setup
	plant_update 0.3.2
	plant_selected 0.3.2
	cmd_unbuilt
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   update 0.3.2'
	expect_out 'bundled 0.2.0: not built yet, run eitri setup'
	expect_no_out 'the fallback'
	# Choosing what already runs is allowed, and it says what is missing behind it.
	cmd_use 0.3.2
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.2' "selected"
	expect_out 'The bundled sidecar is not built yet, so nothing stands behind it: run eitri setup'
	# Going back to a sidecar that is not there says so, and goes back.
	cmd_use bundled
	expect_rc 0
	expect_absent "$UPD_ROOT/selected"
	expect_out 'The bundled sidecar 0.2.0 is not built yet: run eitri setup'
	expect_no_out 'New Claude tabs use the bundled sidecar'
	expect_out 'Update 0.3.2 stays built: eitri sidecar use 0.3.2'
}

# The update is older than the new bundled sidecar: it is skipped, never used as a last resort.
TESTS="$TESTS t_cmd_deb_update_older_than_unbuilt_bundled"
t_cmd_deb_update_older_than_unbuilt_bundled() {
	upd_setup
	plant_update 0.1.9
	plant_selected 0.1.9
	cmd_unbuilt
	cmd_status
	expect_rc 0
	expect_out 'running for new tabs   nothing yet, run eitri setup'
	expect_out 'selected               0.1.9, older than the bundled sidecar, so unused'
	expect_out 'bundled 0.2.0: not built yet, run eitri setup'
	expect_no_out 'update 0.1.9'
	UPD_SNAP=$(snap "$UPD_ROOT")
	cmd_use 0.1.9
	upd_refused "use of an update below an unbuilt bundled sidecar" 'is not newer than the bundled sidecar (0.2.0)'
}

# The `update` command needs the fallback first, so this state cannot be reached through it.
TESTS="$TESTS t_cmd_deb_update_command_still_refuses"
t_cmd_deb_update_command_still_refuses() {
	upd_setup
	plant_update 0.3.2
	plant_selected 0.3.2
	cmd_unbuilt
	UPD_SNAP=$(snap "$UPD_ROOT")
	mk_sidecar_release 0.3.5
	upd_update
	upd_refused "update with no fallback built" 'run "eitri setup" first'
}
