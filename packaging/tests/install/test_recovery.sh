# Recovery, locking, set -e inside functions, truncation and piped stdin. Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

TESTS="$TESTS t_recover_stale_new"
t_recover_stale_new() {
	mkdir -p "$TH/.local/lib/eitri.new/leftover"
	echo junk >"$TH/.local/lib/eitri.new/leftover/file"
	serve 1.0.0
	inst_net
	expect_rc 0
	expect_out "removing $TH/.local/lib/eitri.new, left by an interrupted run"
	expect_absent "$TH/.local/lib/eitri.new"
	expect_eq "$(installed_version)" 1.0.0 "the version installed after the recovery"
}

TESTS="$TESTS t_recover_old_moved_back"
t_recover_old_moved_back() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	# An upgrade interrupted between its two renames: eitri missing, eitri.old present.
	mv "$TH/.local/lib/eitri" "$TH/.local/lib/eitri.old"
	before=$(snap "$TH/.local/lib/eitri.old")
	inst_net
	expect_rc 0
	expect_out "restoring $TH/.local/lib/eitri from $TH/.local/lib/eitri.old"
	# Moved back first, so the same version is then up to date and nothing else changes.
	expect_out 'Eitri 1.0.0 is up to date'
	expect_absent "$TH/.local/lib/eitri.old"
	expect_eq "$(snap "$TH/.local/lib/eitri")" "$before" "the restored install"
}

TESTS="$TESTS t_recover_both_old_removed_first"
t_recover_both_old_removed_first() {
	serve 1.0.0
	inst_net
	expect_rc 0
	mkdir -p "$TH/.local/lib/eitri.old/stale"
	echo stale >"$TH/.local/lib/eitri.old/stale/file"
	serve 1.1.0 1.0.0
	inst_net --stubs "$S/stubs-mvlog" --
	expect_rc 0
	expect_out "removing $TH/.local/lib/eitri.old, left by an interrupted upgrade"
	expect_eq "$(installed_version)" 1.1.0 "the version after the upgrade"
	expect_absent "$TH/.local/lib/eitri.old"
	expect_absent "$TH/.local/lib/eitri/eitri"
	# No mv ever targeted eitri.old while it existed as a directory (which would move the tree
	# inside it), and the swap did happen through mv.
	if grep -F "$TH/.local/lib/eitri.old INTO-EXISTING-DIR" "$S/logs/mv.log" >/dev/null; then
		fail "a tree was moved inside eitri.old: $(cat "$S/logs/mv.log")"
	fi
	if ! grep -F -x "mv -- $TH/.local/lib/eitri $TH/.local/lib/eitri.old" "$S/logs/mv.log" >/dev/null; then
		fail "the swap's first rename was not seen: $(cat "$S/logs/mv.log")"
	fi
}

TESTS="$TESTS t_lock_live_refuses"
t_lock_live_refuses() {
	serve 1.0.0
	mkdir -p "$TH/.cache/eitri/lock"
	sleep 300 &
	holder=$!
	echo "$holder" >"$TH/.cache/eitri/lock/pid"
	inst_net
	kill "$holder" 2>/dev/null
	wait "$holder" 2>/dev/null
	expect_fail "a lock held by a live pid"
	expect_out "another Eitri installer (pid $holder) is running"
	expect_absent "$TH/.local/lib/eitri"
	# The lock belongs to the live holder: a refused run leaves it.
	expect_file "$TH/.cache/eitri/lock/pid"
}

TESTS="$TESTS t_lock_dead_taken_over"
t_lock_dead_taken_over() {
	serve 1.0.0
	sh -c 'exit 0' &
	dead=$!
	wait "$dead"
	mkdir -p "$TH/.cache/eitri/lock"
	echo "$dead" >"$TH/.cache/eitri/lock/pid"
	inst_net
	expect_rc 0
	expect_out "taking over a lock left by an installer that is no longer running (pid $dead)"
	expect_eq "$(installed_version)" 1.0.0 "the version installed after taking the lock over"
	expect_absent "$TH/.cache/eitri/lock"
}

TESTS="$TESTS t_lock_takeover_blocked_while_live"
t_lock_takeover_blocked_while_live() {
	# Serialize stale-lock takeover with its own mkdir-based sub-lock ($NV_LOCK.takeover).
	# A second run finding the same stale lock must wait: racing takeovers can both believe they hold
	# it. acquire_lock's sub-lock comment describes the five-step race.
	serve 1.0.0
	sh -c 'exit 0' &
	dead=$!
	wait "$dead"
	mkdir -p "$TH/.cache/eitri/lock"
	echo "$dead" >"$TH/.cache/eitri/lock/pid"
	sleep 300 &
	holder=$!
	mkdir -p "$TH/.cache/eitri/lock.takeover"
	echo "$holder" >"$TH/.cache/eitri/lock.takeover/pid"
	inst_net
	kill "$holder" 2>/dev/null
	wait "$holder" 2>/dev/null
	expect_fail "a stale lock whose own takeover another run is already mid-way through"
	# Contention for lock.takeover does not prove the outer lock is stale; the message must not
	# claim staleness before this run checks it.
	expect_out "another Eitri installer (pid $holder) is already checking the lock $TH/.cache/eitri/lock"
	expect_absent "$TH/.local/lib/eitri"
	# Neither lock was disturbed: the stale one still has its own original (dead) pid, and the
	# takeover sub-lock still has the other run's (then-live) one -- nothing was renamed, restored
	# or removed.
	expect_eq "$(cat "$TH/.cache/eitri/lock/pid" 2>/dev/null)" "$dead" "the stale lock itself, untouched"
	expect_eq "$(cat "$TH/.cache/eitri/lock.takeover/pid" 2>/dev/null)" "$holder" "the other run's takeover sub-lock, untouched"
}

TESTS="$TESTS t_lock_takeover_self_heals_abandoned_sub_lock"
t_lock_takeover_self_heals_abandoned_sub_lock() {
	# The takeover sub-lock is recoverable by the same
	# dead-pid check as the main lock -- a run that died mid-takeover (SIGKILL, say) must not
	# permanently block every later run from ever recovering the stale lock behind it.
	serve 1.0.0
	sh -c 'exit 0' &
	dead1=$!
	wait "$dead1"
	sh -c 'exit 0' &
	dead2=$!
	wait "$dead2"
	mkdir -p "$TH/.cache/eitri/lock"
	echo "$dead1" >"$TH/.cache/eitri/lock/pid"
	mkdir -p "$TH/.cache/eitri/lock.takeover"
	echo "$dead2" >"$TH/.cache/eitri/lock.takeover/pid"
	inst_net
	expect_rc 0
	expect_out "taking over a lock left by an installer that is no longer running (pid $dead1)"
	expect_eq "$(installed_version)" 1.0.0 "the version installed after self-healing both locks"
	expect_absent "$TH/.cache/eitri/lock"
	expect_absent "$TH/.cache/eitri/lock.takeover"
}

TESTS="$TESTS t_lock_takeover_sub_lock_without_pid_refused"
t_lock_takeover_sub_lock_without_pid_refused() {
	# A run killed between mkdir "$NV_LOCK.takeover" and its pid write leaves a pid-less sub-lock
	# that cannot be recovered automatically. The message must identify lock.takeover, not lock,
	# for manual removal, rather than report ordinary live contention.
	serve 1.0.0
	mkdir -p "$TH/.cache/eitri/lock"
	sh -c 'exit 0' &
	dead=$!
	wait "$dead"
	echo "$dead" >"$TH/.cache/eitri/lock/pid"
	mkdir -p "$TH/.cache/eitri/lock.takeover"
	inst_net
	expect_fail "a takeover sub-lock that never got a pid"
	expect_out "$TH/.cache/eitri/lock.takeover holds no pid"
	expect_out "remove $TH/.cache/eitri/lock.takeover and re-run"
	expect_dir "$TH/.cache/eitri/lock.takeover"
	expect_absent "$TH/.local/lib/eitri"
	# Neither lock was disturbed: the stale main lock still names its own dead pid, and the pid-less
	# sub-lock is exactly as it was left.
	expect_eq "$(cat "$TH/.cache/eitri/lock/pid" 2>/dev/null)" "$dead" "the stale lock itself, untouched"
	expect_absent "$TH/.cache/eitri/lock.takeover/pid"
}

TESTS="$TESTS t_lock_without_pid_refused"
t_lock_without_pid_refused() {
	# A lock with no pid, even after the second a holder gets to write one: whether its holder is
	# alive cannot be told, so it is not taken over.
	serve 1.0.0
	mkdir -p "$TH/.cache/eitri/lock"
	inst_net
	expect_fail "a lock with no pid"
	expect_out "$TH/.cache/eitri/lock holds no pid"
	expect_out "remove $TH/.cache/eitri/lock and re-run"
	expect_dir "$TH/.cache/eitri/lock"
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_lock_pid_write_failure_cleaned_up"
t_lock_pid_write_failure_cleaned_up() {
	# The pid write can fail after the lock directory and NV_LOCK/pid exist. Remove the pid file
	# before rmdir, or later runs are permanently blocked by a non-empty lock with no pid.
	serve 1.0.0
	mkdir -p "$T/stubs-lockpidfail"
	cp "$FIXTURES/mkdir-lock-pid-conflict" "$T/stubs-lockpidfail/mkdir"
	chmod 0755 "$T/stubs-lockpidfail/mkdir"
	inst_net --stubs "$T/stubs-lockpidfail" --
	expect_fail "the pid write fails"
	expect_out "cannot write $TH/.cache/eitri/lock/pid"
	expect_absent "$TH/.cache/eitri/lock"
	expect_absent "$TH/.local/lib/eitri"
	# The lock is gone, so an ordinary re-run (the real mkdir, no stub) is not stuck behind
	# "the lock holds no pid": it proceeds and installs normally.
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the version installed on the re-run"
}

TESTS="$TESTS t_swap_fails_restores"
t_swap_fails_restores() {
	serve 1.0.0
	inst_net
	expect_rc 0
	serve 1.1.0 1.0.0
	before=$(snap_but_staging "$TH")
	# The swap's second rename (eitri.new -> eitri) fails after the first has moved the old
	# install to eitri.old: on_exit moves it back.
	inst_net --stubs "$S/stubs-mvfail" --
	expect_fail "a failed swap"
	expect_out 'mv: simulated failure of the swap'
	expect_no_out 'installed Eitri 1.1.0'
	expect_eq "$(installed_version)" 1.0.0 "the version after a failed swap"
	expect_eq "$(snap_but_staging "$TH")" "$before" "the home after a failed swap"
}

TESTS="$TESTS t_swap_interrupted_restores"
t_swap_interrupted_restores() {
	serve 1.0.0
	inst_net
	expect_rc 0
	serve 1.1.0 1.0.0
	before=$(snap_but_staging "$TH")
	# SIGTERM reaches the installer at the same moment: its trap exits 143, and on_exit moves the old
	# install back.
	inst_net --stubs "$S/stubs-mvterm" --
	expect_rc 143 "an upgrade interrupted during its swap"
	expect_eq "$(installed_version)" 1.0.0 "the version after an interrupted swap"
	expect_eq "$(snap_but_staging "$TH")" "$before" "the home after an interrupted swap"
}

TESTS="$TESTS t_set_e_tar_fails"
t_set_e_tar_fails() {
	serve 1.0.0
	inst_net
	expect_rc 0
	serve 1.1.0 1.0.0
	before=$(snap "$TH")
	# tar fails inside unpack_new, reached through main's real call path.
	inst_net --stubs "$S/stubs-tarfail" --
	expect_fail "tar failing inside a function"
	expect_out 'tar: simulated failure'
	expect_out 'could not unpack'
	expect_no_out 'installed Eitri 1.1.0'
	expect_eq "$(snap "$TH")" "$before" "the home after tar failed"
}

TESTS="$TESTS t_truncated_script"
t_truncated_script() {
	serve 1.0.0
	# The script cut just before its last line, piped into sh.
	lines=$(wc -l <"$INSTALLER")
	head -n "$((lines - 1))" "$INSTALLER" >"$T/truncated.sh"
	before=$(snap "$TH")
	"$WRAP" --home "$TH" --cwd "$T/cwd" --stubs "$S/stubs" --set EITRI_INSTALL_TEST=1 \
		-- "$NV_SH" -s -- --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS" \
		<"$T/truncated.sh" >"$OUT" 2>&1
	RC=$?
	expect_fail "a truncated script"
	expect_eq "$(snap "$TH")" "$before" "the home after a truncated script ran"
	expect_no_out 'eitri:'
	# And cut in the middle, too.
	head -c "$(($(wc -c <"$INSTALLER") / 2))" "$INSTALLER" >"$T/half.sh"
	"$WRAP" --home "$TH" --cwd "$T/cwd" --stubs "$S/stubs" -- "$NV_SH" -s -- --uninstall <"$T/half.sh" >"$OUT" 2>&1
	RC=$?
	expect_fail "a half script"
	expect_eq "$(snap "$TH")" "$before" "the home after half a script ran"
}

TESTS="$TESTS t_truncated_every_cut_point"
t_truncated_every_cut_point() {
	# Every cut of the last three lines, piped to sh with --help: no prefix ending before the final
	# `}` may run anything. The complete script must receive its arguments, with or without the final
	# newline; otherwise --dry-run or --uninstall could become a real latest-release install.
	# A stub uname catches a main with no arguments before any download or change.
	size=$(wc -c <"$INSTALLER")
	tail3=$(tail -n 3 "$INSTALLER" | wc -c)
	before=$(snap "$TH")
	k=0
	while [ "$k" -le "$tail3" ]; do
		n=$((size - tail3 + k))
		head -c "$n" "$INSTALLER" >"$T/cut.sh"
		"$WRAP" --home "$TH" --cwd "$T/cwd" --stubs "$S/stubs" --stubs "$S/stubs-probeuname" --set EITRI_INSTALL_TEST=1 \
			-- "$NV_SH" -s -- --help <"$T/cut.sh" >"$OUT" 2>&1
		RC=$?
		if grep -F -e PROBE-UNAME -e 'eitri:' "$OUT" >/dev/null; then
			fail "cut at byte $n of $size ran main without its arguments: $(cat "$OUT")"
		fi
		if [ "$n" -ge "$((size - 1))" ]; then
			# The whole script: `}` is there, with or without the newline after it.
			expect_rc 0 "the whole script (cut at byte $n of $size)"
			expect_out 'usage: sh install.sh'
		else
			expect_fail "a script cut at byte $n of $size"
			expect_no_out 'usage: sh install.sh'
		fi
		k=$((k + 1))
	done
	expect_eq "$(snap "$TH")" "$before" "the home after every cut"
}

TESTS="$TESTS t_piped_stdin"
t_piped_stdin() {
	serve 1.0.0
	# cat install.sh | sh -s -- --yes ...: the script arrives on stdin, which it never reads.
	cat "$INSTALLER" | "$WRAP" --home "$TH" --cwd "$T/cwd" --stubs "$S/stubs" \
		--set EITRI_INSTALL_TEST=1 --set "EITRI_INSTALL_TEST_LIBDIRS=$S/libs/ok" \
		--set "EITRI_INSTALL_TEST_SYSTEM_RELEASE=$S/system/RELEASE" \
		-- "$NV_SH" -s -- --yes --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS" >"$OUT" 2>&1
	RC=$?
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the version installed through a pipe"
	expect_eq "$(tree "$TH")" "$EXPECTED_TREE" "the tree installed through a pipe"
}

TESTS="$TESTS t_script_never_reads_stdin"
t_script_never_reads_stdin() {
	if grep -n -e '\[ -t 0 \]' -e 'test -t 0' "$INSTALLER"; then fail "the installer tests fd 0 for a terminal"; fi
	# Every `read` (the command, not the word inside a comment or a string) takes </dev/tty.
	grep -n -E '(^|[;&|({[:space:]])read[[:space:]]' "$INSTALLER" | grep -v -E '^[0-9]+:[[:space:]]*#' |
		grep -v -F '</dev/tty' >"$T/reads" || :
	if [ -s "$T/reads" ]; then fail "a read without </dev/tty: $(cat "$T/reads")"; fi
	# main runs with stdin from /dev/null, as the last command inside the group, whose `}` is the
	# script's last line: no prefix of the script can run main (t_truncated_every_cut_point).
	expect_eq "$(tail -n 2 "$INSTALLER")" 'main "$@" </dev/null
}' "the installer's last two lines"
}

TESTS="$TESTS t_upgrade_unwritable_bindir"
t_upgrade_unwritable_bindir() {
	# The launcher, desktop entry and licences are written (to temporary names beside their
	# destinations) before the swap, so an unwritable ~/.local/bin stops an upgrade while the old
	# install is still in place, untouched -- not after the swap, leaving the new lib with the old
	# launcher and licences and no eitri.old to go back to.
	serve 1.0.0
	inst_net
	expect_rc 0
	serve 1.1.0 1.0.0
	before=$(snap_but_staging "$TH")
	# Put back the mode the directory had, not a fixed one: the snapshot holds it, and it follows the
	# umask the run started under (0700 under a umask of 077).
	bin_mode=$(stat -c %a "$TH/.local/bin")
	chmod 0555 "$TH/.local/bin"
	if [ -w "$TH/.local/bin" ]; then
		# Run as root, the directory is writable anyway: nothing to test.
		chmod "$bin_mode" "$TH/.local/bin"
		return 0
	fi
	inst_net
	chmod "$bin_mode" "$TH/.local/bin"
	expect_fail "an upgrade with an unwritable ~/.local/bin"
	expect_out "cannot write $TH/.local/bin/.eitri.tmp."
	expect_no_out 'installed Eitri 1.1.0'
	expect_eq "$(installed_version)" 1.0.0 "the version after the refused upgrade"
	expect_eq "$(snap_but_staging "$TH")" "$before" "the home after the refused upgrade"
	# Once the directory is writable, the same run goes through.
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.1.0 "the version after the re-run"
	expect_eq "$(sed -n 's/^LICENSE for //p' "$TH/.local/share/licenses/eitri/LICENSE")" 1.1.0 "the licences"
	expect_eq "$(tree "$TH")" "$EXPECTED_TREE" "the tree after the re-run"
}

TESTS="$TESTS t_interrupted_after_swap_rerun_finishes"
t_interrupted_after_swap_rerun_finishes() {
	# An interrupt just after the swap's second rename leaves the new lib in place, before completion.
	# A sidecar may already exist (from a .deb setup or the pre-swap build), so its presence alone must
	# not make a re-run report up to date with no launcher, desktop entry or licences.
	plant_sidecar aaaaaaa
	serve 1.0.0
	inst_net --stubs "$S/stubs-mvafterswap" --
	expect_rc 143 "an install interrupted right after its swap"
	expect_eq "$(installed_version)" 1.0.0 "the lib in place after the interrupt"
	expect_absent "$TH/.local/bin/eitri"
	inst_net
	expect_rc 0
	expect_no_out 'is up to date'
	expect_out 'installed Eitri 1.0.0'
	expect_exec "$TH/.local/bin/eitri"
	expect_file "$(data_of)/applications/cn.huntergrey.eitri.desktop"
	for f in LICENSE THIRD-PARTY-LICENSES SOURCE; do expect_file "$(data_of)/licenses/eitri/$f"; done
	# And now it is finished, a re-run is up to date.
	inst_net
	expect_rc 0
	expect_out 'Eitri 1.0.0 is up to date'
}

TESTS="$TESTS t_interrupted_upgrade_stale_old_files_not_up_to_date"
t_interrupted_upgrade_stale_old_files_not_up_to_date() {
	# An upgrade interrupted after the swap but before commit_files leaves the older install's
	# launcher, desktop entry and licences in place. Existence checks alone cannot mark the new lib
	# complete: a re-run must replace those files, including SOURCE naming the wrong release.
	serve 1.0.0 1.1.0
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the first install"
	before_source=$(cat "$(data_of)/licenses/eitri/SOURCE")
	plant_sidecar bbbbbbb
	inst_net --stubs "$S/stubs-mvafterswap" -- --version 1.1.0
	expect_rc 143 "an upgrade interrupted right after its swap"
	expect_eq "$(installed_version)" 1.1.0 "the lib in place after the interrupt"
	expect_eq "$(cat "$(data_of)/licenses/eitri/SOURCE")" "$before_source" "SOURCE is still 1.0.0's own, untouched by the interrupt"
	inst_net --version 1.1.0
	expect_rc 0
	expect_no_out 'is up to date'
	expect_out 'installed Eitri 1.1.0'
	expect_eq "$(sed -n 's/^SOURCE for //p' "$(data_of)/licenses/eitri/SOURCE")" 1.1.0 "SOURCE, now the new release's own"
	# And now it really is finished: a further re-run is up to date.
	inst_net --version 1.1.0
	expect_rc 0
	expect_out 'Eitri 1.1.0 is up to date'
}
