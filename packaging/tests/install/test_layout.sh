# Layout, upgrade, dry run and offline install (spec §6.4-6.5). Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

EXPECTED_TREE='./.local d
./.local/bin d
./.local/bin/eitri f
./.local/lib d
./.local/lib/eitri d
./.local/lib/eitri/RELEASE f
./.local/lib/eitri/eitri-claude-handoff f
./.local/lib/eitri/eitri-setup f
./.local/lib/eitri/eitri-supervisor f
./.local/lib/eitri/eitri-tmux-shim f
./.local/lib/eitri/shell f
./.local/share d
./.local/share/applications d
./.local/share/applications/cn.huntergrey.eitri.Panel.desktop f
./.local/share/applications/cn.huntergrey.eitri.desktop f
./.local/share/eitri d
./.local/share/eitri/eitri.nvim d
./.local/share/eitri/eitri.nvim/doc d
./.local/share/eitri/eitri.nvim/doc/eitri.txt f
./.local/share/eitri/eitri.nvim/lua d
./.local/share/eitri/eitri.nvim/lua/eitri d
./.local/share/eitri/eitri.nvim/lua/eitri/init.lua f
./.local/share/eitri/eitri.nvim/plugin d
./.local/share/eitri/eitri.nvim/plugin/eitri.lua f
./.local/share/gnome-shell d
./.local/share/gnome-shell/extensions d
./.local/share/gnome-shell/extensions/eitri@huntergrey.cn d
./.local/share/gnome-shell/extensions/eitri@huntergrey.cn/direction.js f
./.local/share/gnome-shell/extensions/eitri@huntergrey.cn/extension.js f
./.local/share/gnome-shell/extensions/eitri@huntergrey.cn/metadata.json f
./.local/share/gnome-shell/extensions/eitri@huntergrey.cn/policy.js f
./.local/share/icons d
./.local/share/icons/hicolor d
./.local/share/icons/hicolor/128x128 d
./.local/share/icons/hicolor/128x128/apps d
./.local/share/icons/hicolor/128x128/apps/cn.huntergrey.eitri.png f
./.local/share/icons/hicolor/16x16 d
./.local/share/icons/hicolor/16x16/apps d
./.local/share/icons/hicolor/16x16/apps/cn.huntergrey.eitri.png f
./.local/share/icons/hicolor/24x24 d
./.local/share/icons/hicolor/24x24/apps d
./.local/share/icons/hicolor/24x24/apps/cn.huntergrey.eitri.png f
./.local/share/icons/hicolor/256x256 d
./.local/share/icons/hicolor/256x256/apps d
./.local/share/icons/hicolor/256x256/apps/cn.huntergrey.eitri.png f
./.local/share/icons/hicolor/32x32 d
./.local/share/icons/hicolor/32x32/apps d
./.local/share/icons/hicolor/32x32/apps/cn.huntergrey.eitri.png f
./.local/share/icons/hicolor/48x48 d
./.local/share/icons/hicolor/48x48/apps d
./.local/share/icons/hicolor/48x48/apps/cn.huntergrey.eitri.png f
./.local/share/icons/hicolor/512x512 d
./.local/share/icons/hicolor/512x512/apps d
./.local/share/icons/hicolor/512x512/apps/cn.huntergrey.eitri.png f
./.local/share/icons/hicolor/64x64 d
./.local/share/icons/hicolor/64x64/apps d
./.local/share/icons/hicolor/64x64/apps/cn.huntergrey.eitri.png f
./.local/share/icons/hicolor/scalable d
./.local/share/icons/hicolor/scalable/apps d
./.local/share/icons/hicolor/scalable/apps/cn.huntergrey.eitri.svg f
./.local/share/licenses d
./.local/share/licenses/eitri d
./.local/share/licenses/eitri/.installed-version f
./.local/share/licenses/eitri/LICENSE f
./.local/share/licenses/eitri/SOURCE f
./.local/share/licenses/eitri/THIRD-PARTY-LICENSES f'

TESTS="$TESTS t_fresh_layout"
t_fresh_layout() {
	serve 1.0.0
	inst_net
	expect_rc 0
	expect_eq "$(tree "$TH")" "$EXPECTED_TREE" "the installed tree"
	expect_eq "$(installed_version)" 1.0.0 "the installed version"
	for b in shell eitri-supervisor eitri-tmux-shim eitri-claude-handoff eitri-setup; do
		expect_exec "$TH/.local/lib/eitri/$b"
	done
	expect_exec "$TH/.local/bin/eitri"
	# The launcher carries the marker, and is otherwise the tarball's bin/eitri.
	if ! grep -Fqx '# eitri-launcher v1' "$TH/.local/bin/eitri"; then fail "the launcher has no marker line"; fi
	if ! cmp -s "$PKG/eitri.launcher.sh" "$TH/.local/bin/eitri" &&
		! head -c "$(wc -c <"$PKG/eitri.launcher.sh")" "$TH/.local/bin/eitri" | cmp -s - "$PKG/eitri.launcher.sh"; then
		fail "the launcher is not the tarball's bin/eitri plus the marker"
	fi
	# eitri-setup is this installer, byte for byte (spec §4.3).
	if ! cmp -s "$INSTALLER" "$TH/.local/lib/eitri/eitri-setup"; then fail "eitri-setup differs from install.sh"; fi
	expect_eq "$(grep '^Exec=' "$TH/.local/share/applications/cn.huntergrey.eitri.desktop")" \
		"Exec=\"$TH/.local/bin/eitri\" --quiet %f" "the desktop Exec line"
	expect_absent "$TH/.cache"
	expect_out 'installed Eitri 1.0.0'
	expect_no_out 'restart open Eitri windows'
}

TESTS="$TESTS t_symlinked_bindir_launches"
t_symlinked_bindir_launches() {
	# F5 (whole-branch review): a symlinked ~/.local/bin -- e.g. GNU stow folding it into a
	# dotfiles repo -- is a directory symlink, not a symlink on the launcher file itself. The
	# install still lands a real ~/.local/lib/eitri (untouched here), but the launcher's old
	# `readlink -f "$0"` canonicalized straight through the symlinked bindir, landing LIBDIR next
	# to the symlink's TARGET directory instead of this install's own sibling lib/eitri -- so
	# install reported success and every launch, `eitri setup` included, died `cd:
	# .../lib/eitri: No such file or directory`. Move the planted bindir (with its nvim/vim)
	# aside and replace it with a symlink before installing, so it stays reachable at the same
	# path after_each's byte-for-byte planted check reads.
	mv "$TH/.local/bin" "$T/realbin"
	ln -s "$T/realbin" "$TH/.local/bin"
	serve 1.0.0
	inst_net
	expect_rc 0
	expect_out 'installed Eitri 1.0.0'
	# The install must not have replaced the planted symlink with a real directory -- otherwise a
	# later change to the installer could make this test stop exercising F5 at all without ever
	# going red.
	if [ ! -L "$TH/.local/bin" ]; then fail "the installer replaced the symlinked bindir with a real directory"; fi
	launch_out=$(env -i HOME="$TH" PATH="$TH/.local/bin:/usr/bin:/bin" bash -c 'eitri --version' 2>&1)
	launch_rc=$?
	if [ "$launch_rc" != 0 ]; then
		fail "eitri --version through the symlinked bindir exited $launch_rc: $launch_out"
	fi
	case $launch_out in
	*'No such file or directory'*) fail "LIBDIR resolution broke through the symlinked bindir: $launch_out" ;;
	esac
	case $launch_out in
	*'stub shell 1.0.0'*) : ;;
	*) fail "the real stub was never reached through the symlinked bindir: $launch_out" ;;
	esac
}

TESTS="$TESTS t_desktop_exec_space_percent"
t_desktop_exec_space_percent() {
	use_home "$T/my home 100%"
	serve 1.0.0
	inst_net
	expect_rc 0
	expect_eq "$(grep '^Exec=' "$TH/.local/share/applications/cn.huntergrey.eitri.desktop")" \
		"Exec=\"$T/my home 100%%/.local/bin/eitri\" --quiet %f" "the escaped Exec line"
	# Every other line is the tarball's desktop file's.
	if [ "$(grep -v '^Exec=' "$TH/.local/share/applications/cn.huntergrey.eitri.desktop")" != "$(grep -v '^Exec=' "$PKG/cn.huntergrey.eitri.desktop")" ]; then
		fail "the desktop file differs from the tarball's in more than Exec"
	fi
}

TESTS="$TESTS t_desktop_exec_all_specials"
t_desktop_exec_all_specials() {
	# A HOME holding every character the Exec quoting rule names -- " ` $ \ -- plus a space and a %.
	# test_install.py also reads this file back through GLib's own parser (on the host).
	# shellcheck disable=SC2016 # literal characters, deliberately unexpanded
	use_home "$T/"'q"b`t$d\s 5%'
	printf '%s\n' "$TH" >"$T/home-path"
	serve 1.0.0
	inst_net
	expect_rc 0
	# Quoting rule first (a backslash before each), then the string-value rule doubles every
	# backslash, then % becomes %%.
	# shellcheck disable=SC2016 # literal characters, deliberately unexpanded
	expect_eq "$(grep '^Exec=' "$TH/.local/share/applications/cn.huntergrey.eitri.desktop")" \
		"Exec=\"$T/"'q\\"b\\`t\\$d\\\\s 5%%/.local/bin/eitri" --quiet %f' "the escaped Exec line"
	expect_exec "$TH/.local/bin/eitri"
	expect_eq "$(installed_version)" 1.0.0 "installed into a HOME with every special character"
}

TESTS="$TESTS t_rerun_up_to_date"
t_rerun_up_to_date() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	before=$(snap "$TH")
	sleep 1
	inst_net
	expect_rc 0
	expect_out 'Eitri 1.0.0 is up to date'
	expect_eq "$(snap "$TH")" "$before" "the home after an up-to-date re-run (mtimes included)"
}

TESTS="$TESTS t_rerun_finishes_incomplete_install"
t_rerun_finishes_incomplete_install() {
	# "Up to date" means finished: the same version with its sidecar present, and the launcher (with
	# its marker), the desktop entry and the three licence files all there. Missing any of them, a
	# re-run installs again -- what the installer's own errors promise ("re-run to finish").
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	for f in "$TH/.local/bin/eitri" "$(data_of)/applications/cn.huntergrey.eitri.desktop" \
		"$(data_of)/licenses/eitri/LICENSE" "$(data_of)/licenses/eitri/THIRD-PARTY-LICENSES" \
		"$(data_of)/licenses/eitri/SOURCE"; do
		rm "$f"
		inst_net
		expect_rc 0
		expect_no_out 'is up to date'
		expect_out 'installed Eitri 1.0.0'
		expect_file "$f"
	done
	# A launcher without the marker is not this install's: not finished either.
	printf '#!/bin/sh\necho mine\n' >"$TH/.local/bin/eitri"
	inst_net
	expect_rc 0
	expect_no_out 'is up to date'
	if ! grep -Fqx '# eitri-launcher v1' "$TH/.local/bin/eitri"; then fail "the launcher was not reinstalled"; fi
	inst_net
	expect_rc 0
	expect_out 'Eitri 1.0.0 is up to date'
}

TESTS="$TESTS t_same_version_without_sidecar_reinstalls"
t_same_version_without_sidecar_reinstalls() {
	serve 1.0.0
	inst_net
	expect_rc 0
	# A sidecar whose BUILD does not match its binary is not *present* (spec §5.3 step 6).
	plant_sidecar aaaaaaa
	printf 'VERDANDI_REV=x\nSIDECAR_VERSION_LINE=something else\n' >"$(data_of)/eitri/sidecar/aaaaaaa/BUILD"
	inst_net
	expect_rc 0
	expect_no_out 'is up to date'
	expect_out 'installed Eitri 1.0.0'
}

TESTS="$TESTS t_upgrade_swap"
t_upgrade_swap() {
	serve 1.0.0
	inst_net
	expect_rc 0
	serve 1.1.0 1.0.0
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.1.0 "the version after the upgrade"
	expect_out 'installed Eitri 1.1.0'
	expect_out 'restart open Eitri windows'
	expect_absent "$TH/.local/lib/eitri.old"
	expect_absent "$TH/.local/lib/eitri.new"
	expect_eq "$(tree "$TH")" "$EXPECTED_TREE" "the tree after an upgrade"
	if ! grep -q 'stub shell 1.1.0' "$TH/.local/lib/eitri/shell"; then fail "shell was not replaced"; fi
	expect_eq "$(sed -n 's/^LICENSE for //p' "$TH/.local/share/licenses/eitri/LICENSE")" 1.1.0 "the licences"
}

TESTS="$TESTS t_upgrade_corrupt_tarball"
t_upgrade_corrupt_tarball() {
	serve 1.0.0
	inst_net
	expect_rc 0
	# v1.1.0's tarball is not a tarball, but SHA256SUMS lists it truthfully and is signed: the
	# checksum passes and the unpack is what fails.
	serve 1.1.0 1.0.0
	d=$(served 1.1.0)
	head -c 4096 /dev/urandom >"$d/eitri-1.1.0-x86_64-linux.tar.gz"
	resign "$d"
	relatest 1.1.0
	before=$(snap "$TH")
	inst_net
	expect_fail "a corrupt tarball"
	expect_out 'could not unpack eitri-1.1.0-x86_64-linux.tar.gz'
	expect_eq "$(snap "$TH")" "$before" "the home after a failed upgrade"
}

TESTS="$TESTS t_upgrade_bad_layout"
t_upgrade_bad_layout() {
	serve 1.0.0
	inst_net
	expect_rc 0
	# A valid tarball without lib/eitri/shell: the unpack passes, the layout check refuses.
	serve 1.1.0 1.0.0
	d=$(served 1.1.0)
	rm -rf "$T/bad"
	mkdir -p "$T/bad"
	tar -C "$T/bad" -xzf "$d/eitri-1.1.0-x86_64-linux.tar.gz"
	rm "$T/bad/eitri-1.1.0-x86_64-linux/lib/eitri/shell"
	tar -C "$T/bad" -czf "$d/eitri-1.1.0-x86_64-linux.tar.gz" eitri-1.1.0-x86_64-linux
	resign "$d"
	relatest 1.1.0
	before=$(snap "$TH")
	inst_net
	expect_fail "a tarball without shell"
	expect_out 'has no executable lib/eitri/shell'
	expect_eq "$(snap "$TH")" "$before" "the home after a refused upgrade"
}

TESTS="$TESTS t_release_malformed"
t_release_malformed() {
	# A RELEASE naming no valid VERDANDI_REV refuses, and the message names the file -- in a real
	# run the unpacked lib/eitri/RELEASE, in a dry run the release's RELEASE asset.
	serve 1.0.0
	inst_net
	expect_rc 0
	serve 1.1.0 1.0.0
	d=$(served 1.1.0)
	top=eitri-1.1.0-x86_64-linux
	rm -rf "$T/bad"
	mkdir -p "$T/bad"
	tar -C "$T/bad" -xzf "$d/$top.tar.gz"
	sed -i 's/^VERDANDI_REV=.*/VERDANDI_REV=not-a-rev/' "$T/bad/$top/lib/eitri/RELEASE"
	tar -C "$T/bad" -czf "$d/$top.tar.gz" "$top"
	cp "$T/bad/$top/lib/eitri/RELEASE" "$d/RELEASE"
	resign "$d"
	relatest 1.1.0
	before=$(snap "$TH")
	inst_net --dry-run
	expect_fail "a dry run with a malformed RELEASE"
	expect_out 'eitri: error: the RELEASE of 1.1.0 names no valid VERDANDI_REV (40 hex)'
	expect_no_out 'EITRI_VERSION=1.1.0 names'
	inst_net
	expect_fail "an upgrade with a malformed RELEASE"
	expect_out "/$top/lib/eitri/RELEASE names no valid VERDANDI_REV (40 hex)"
	expect_eq "$(installed_version)" 1.0.0 "the version after a refused upgrade"
	expect_eq "$(snap "$TH")" "$before" "the home after a refused upgrade"
}

TESTS="$TESTS t_dry_run_fresh"
t_dry_run_fresh() {
	serve 1.0.0
	before=$(snap "$TH")
	inst_net --dry-run
	expect_rc 0
	expect_eq "$(snap "$TH")" "$before" "the home after a dry run"
	expect_out 'would take the lock'
	expect_out "would download http://127.0.0.1:$PORT/releases/download/v1.0.0/eitri-1.0.0-x86_64-linux.tar.gz"
	expect_out 'would unpack eitri-1.0.0-x86_64-linux.tar.gz'
	expect_out "would run: 'mv' '--' '$TH/.local/lib/eitri.new' '$TH/.local/lib/eitri'"
	expect_out "would write the launcher $TH/.local/bin/eitri"
	expect_out "would write $TH/.local/share/applications/cn.huntergrey.eitri.desktop"
	expect_out "would write $TH/.local/share/applications/cn.huntergrey.eitri.Panel.desktop"
	expect_out "would write $TH/.local/share/eitri/eitri.nvim/plugin/eitri.lua"
	expect_out "would write $TH/.local/share/licenses/eitri/LICENSE"
	expect_out 'would download http'
	expect_out '/SHA256SUMS.sig and check it with ssh-keygen -Y verify'
	expect_out 'dry run: nothing was changed'
	# Without a key (the harness's unkeyed installer; the real one lists the owner's), no .sig is fetched,
	# and it says so.
	inst -- --base-url "http://127.0.0.1:$PORT" --dry-run
	expect_rc 0
	expect_out 'this installer carries no release key'
	expect_no_out 'SHA256SUMS.sig'
	expect_eq "$(snap "$TH")" "$before" "the home after a keyless dry run"
}

TESTS="$TESTS t_dry_run_upgrade"
t_dry_run_upgrade() {
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	# 1.1.0's own sidecar (bbbbbbb) is already there: a real upgrade keeps it and finds it present,
	# so a dry run must report the same, never removing it.
	plant_sidecar bbbbbbb
	mkdir -p "$(data_of)/eitri/sidecar/0123456"
	serve 1.1.0 1.0.0
	before=$(snap "$TH")
	inst_net --dry-run
	expect_rc 0
	expect_eq "$(snap "$TH")" "$before" "the home after a dry-run upgrade"
	expect_out "would run: 'mv' '--' '$TH/.local/lib/eitri' '$TH/.local/lib/eitri.old'"
	expect_out "the new release's RELEASE (checked against SHA256SUMS) names verdandi bbbbbbb"
	expect_out 'sidecar for verdandi bbbbbbb: present'
	expect_out "would run: 'rm' '-rf' '--' '$(data_of)/eitri/sidecar/0123456'"
	expect_no_out "sidecar/aaaaaaa'"
	expect_no_out "sidecar/bbbbbbb'"
	# Offline, the RELEASE is read out of the --tarball itself.
	d=$S/fix/v1.1.0
	inst -- --tarball "$d/eitri-1.1.0-x86_64-linux.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" \
		--release-signers "$SIGNERS" --dry-run
	expect_rc 0
	expect_eq "$(snap "$TH")" "$before" "the home after an offline dry-run upgrade"
	# With the key in a file already (--release-signers), the dry run checks the local .sig; only
	# the embedded key, which a dry run never writes out, is left unchecked (t_embedded_key_dry_run).
	expect_out 'signature on SHA256SUMS: good (release@eitri)'
	expect_out "the new release's RELEASE (read from eitri-1.1.0-x86_64-linux.tar.gz) names verdandi bbbbbbb"
	expect_out 'sidecar for verdandi bbbbbbb: present'
	expect_no_out "sidecar/bbbbbbb'"
	# A release whose SHA256SUMS lists no RELEASE: the rev cannot be told, and the report says so.
	dd=$(served 1.1.0)
	grep -v '  RELEASE$' "$dd/SHA256SUMS" >"$dd/S" && mv "$dd/S" "$dd/SHA256SUMS"
	sign_sums "$dd" release
	relatest 1.1.0
	inst_net --dry-run
	expect_rc 0
	expect_eq "$(snap "$TH")" "$before" "the home after a dry run that cannot read RELEASE"
	expect_out 'this dry run could not obtain the new release'
	expect_out "would run: 'rm' '-rf' '--' '$(data_of)/eitri/sidecar/bbbbbbb'"
	expect_out 'removing the sidecar for verdandi bbbbbbb, unless the new release uses it'
	# A RELEASE asset that does not match SHA256SUMS refuses, as a download that does not would.
	serve 1.1.0 1.0.0
	printf 'EXTRA=1\n' >>"$(served 1.1.0)/RELEASE"
	inst_net --dry-run
	expect_fail "a RELEASE asset that does not match SHA256SUMS"
	expect_out 'checksum mismatch for RELEASE'
	expect_eq "$(snap "$TH")" "$before" "the home after a dry run refused a RELEASE"
}

TESTS="$TESTS t_prune_sidecars"
t_prune_sidecars() {
	# Spec §6.5 step 4: kept are the new rev, the replaced install's rev, and a rev another installed
	# RELEASE names; anything else named like a rev goes, and anything else is left alone.
	serve 1.0.0
	inst_net
	expect_rc 0
	sc=$(data_of)/eitri/sidecar
	for r in aaaaaaa ddddddd eeeeeee; do plant_sidecar "$r"; done
	mkdir -p "$sc/not-a-rev"
	printf 'EITRI_VERSION=0.9.0\nVERDANDI_REV=ddddddd000000000000000000000000000000000\n' >"$S/system/RELEASE"
	serve 1.1.0 1.0.0
	# F1 (v1-dist whole-branch review, 2026-09-28): plant_sidecar aaaaaaa above makes the
	# now-installed rev's own sidecar genuinely "present" (sidecar_state matches its BUILD file), so
	# ensure_sidecar's Node download is no longer tolerant here -- a working install's own sidecar is
	# being replaced. Node's own download is redirected (like sc_inst_net) so the upgrade's real
	# sidecar build succeeds; this test's own subject (which OTHER rev directories prune keeps or
	# drops) is unaffected either way.
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	rm -f "$S/system/RELEASE"
	expect_rc 0
	expect_dir "$sc/aaaaaaa"
	expect_dir "$sc/ddddddd"
	expect_dir "$sc/not-a-rev"
	expect_absent "$sc/eeeeeee"
	# The next upgrade drops the rev of the install replaced one upgrade ago.
	serve 1.2.0 1.1.0
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	expect_rc 0
	expect_absent "$sc/aaaaaaa"
	expect_absent "$sc/ddddddd"
}

TESTS="$TESTS t_malformed_own_release_stops"
t_malformed_own_release_stops() {
	# The install being replaced keeps its sidecar until the next upgrade (spec §6.5 step 4). When
	# its RELEASE names no valid rev, which one that is cannot be told: the upgrade stops before it
	# downloads or changes anything, rather than prune on a guess.
	serve 1.0.0
	inst_net
	expect_rc 0
	plant_sidecar aaaaaaa
	sed -i 's/^VERDANDI_REV=\(.*\)$/VERDANDI_REV="\1"/' "$TH/.local/lib/eitri/RELEASE"
	serve 1.1.0 1.0.0
	srv_mark
	before=$(snap_but_lock "$TH")
	inst_net
	expect_fail "an upgrade over an install whose RELEASE names no valid rev"
	expect_out "$TH/.local/lib/eitri/RELEASE names no valid VERDANDI_REV"
	expect_out "remove $TH/.local/lib/eitri"
	expect_eq "$(snap_but_lock "$TH")" "$before" "the home after a refused upgrade"
	if srv_paths | grep -F .tar.gz >/dev/null; then fail "a tarball was fetched: $(srv_paths)"; fi
	# Once the damaged install is removed, as the message says, the upgrade proceeds.
	rm -rf "$TH/.local/lib/eitri"
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.1.0 "the version after removing the damaged install"
}

TESTS="$TESTS t_offline_install"
t_offline_install() {
	d=$S/fix/v1.0.0
	inst -- --tarball "$d/eitri-1.0.0-x86_64-linux.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" \
		--release-signers "$SIGNERS"
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the version installed from files"
	expect_eq "$(tree "$TH")" "$EXPECTED_TREE" "the tree installed from files"
	expect_out 'signature on SHA256SUMS: good'
}

TESTS="$TESTS t_offline_sig_required"
t_offline_sig_required() {
	d=$S/fix/v1.0.0
	inst -- --tarball "$d/eitri-1.0.0-x86_64-linux.tar.gz" --sums "$d/SHA256SUMS" --release-signers "$SIGNERS"
	expect_fail "--tarball without --sig while a key is listed"
	expect_out '--tarball needs --sig'
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_path_warning"
t_path_warning() {
	serve 1.0.0
	inst_net --stubs "$S/stubs-patheitri" --
	expect_rc 0
	expect_out "\`eitri\` on this PATH runs $S/stubs-patheitri/eitri, not the one just installed"
}
