# Checksums (MIN-3), version selection (MIN-1) and the signature (SIG-2, D13). Sourced by
# harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

# break_sum FILE NAME: SHA256SUMS's line for NAME gets a wrong (well-formed) hash.
break_sum() {
	awk -v n="$2" '$2 == n { $1 = "0000000000000000000000000000000000000000000000000000000000000000"; print $1 "  " $2; next } { print }' \
		"$1" >"$1.tmp" && mv "$1.tmp" "$1"
}

TESTS="$TESTS t_wrong_sha_refuses"
t_wrong_sha_refuses() {
	serve 1.0.0
	d=$(served 1.0.0)
	break_sum "$d/SHA256SUMS" eitri-1.0.0-x86_64-linux.tar.gz
	sign_sums "$d" release
	relatest 1.0.0
	before=$(snap "$TH")
	inst_net --stubs "$S/stubs-mvlog" --
	expect_fail "a wrong sha256"
	expect_out 'checksum mismatch for eitri-1.0.0-x86_64-linux.tar.gz'
	expect_out 'nothing was installed'
	# Nothing new anywhere -- not even the cache: its download directory goes with the run.
	expect_eq "$(snap "$TH")" "$before" "the home after a checksum mismatch"
	# The .part was never renamed.
	if grep -F 'eitri-1.0.0-x86_64-linux.tar.gz.part' "$S/logs/mv.log" >/dev/null; then
		fail "the mismatching .part was renamed: $(cat "$S/logs/mv.log")"
	fi
}

TESTS="$TESTS t_file_missing_from_sums"
t_file_missing_from_sums() {
	# The tarball's line is missing: no version can be taken, and nothing is fetched or unpacked.
	serve 1.0.0
	d=$(served 1.0.0)
	grep -v 'x86_64-linux.tar.gz' "$d/SHA256SUMS" >"$d/S" && mv "$d/S" "$d/SHA256SUMS"
	sign_sums "$d" release
	relatest 1.0.0
	srv_mark
	inst_net
	expect_fail "a SHA256SUMS without the tarball"
	expect_out 'SHA256SUMS lists no eitri-<version>-x86_64-linux.tar.gz'
	expect_absent "$TH/.local/lib/eitri"
	if srv_paths | grep -F '.tar.gz' >/dev/null; then fail "a tarball was fetched: $(srv_paths)"; fi
}

TESTS="$TESTS t_file_listed_twice"
t_file_listed_twice() {
	# Two identical tarball lines.
	serve 1.0.0
	d=$(served 1.0.0)
	grep 'x86_64-linux.tar.gz' "$d/SHA256SUMS" >"$T/line"
	cat "$T/line" >>"$d/SHA256SUMS"
	sign_sums "$d" release
	relatest 1.0.0
	inst_net
	expect_fail "a tarball listed twice"
	expect_out 'SHA256SUMS lists more than one Eitri tarball'
	expect_absent "$TH/.local/lib/eitri"
	# A second line for the same file that the version pattern does not see (one space, not two):
	# the per-file rule -- exactly one line whose second field is the name -- refuses it.
	serve 1.0.0
	d=$(served 1.0.0)
	grep 'x86_64-linux.tar.gz' "$d/SHA256SUMS" | sed 's/  / /' >"$T/line"
	cat "$T/line" >>"$d/SHA256SUMS"
	sign_sums "$d" release
	relatest 1.0.0
	inst_net
	expect_fail "a file listed twice"
	expect_out 'SHA256SUMS lists eitri-1.0.0-x86_64-linux.tar.gz 2 times'
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_two_tarball_lines"
t_two_tarball_lines() {
	serve 1.1.0 1.0.0
	d=$(served 1.1.0)
	grep 'x86_64-linux.tar.gz' "$S/fix/v1.0.0/SHA256SUMS" >>"$d/SHA256SUMS"
	sign_sums "$d" release
	relatest 1.1.0
	inst_net
	expect_fail "two tarball lines"
	expect_out 'SHA256SUMS lists more than one Eitri tarball'
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_later_files_from_versioned_dir"
t_later_files_from_versioned_dir() {
	serve 1.0.0
	srv_mark
	inst_net
	expect_rc 0
	srv_paths >"$T/paths"
	expect_eq "$(head -n 1 "$T/paths")" /releases/latest/download/SHA256SUMS "the first request"
	expect_eq "$(tail -n +2 "$T/paths")" "/releases/download/v1.0.0/SHA256SUMS
/releases/download/v1.0.0/SHA256SUMS.sig
/releases/download/v1.0.0/eitri-1.0.0-x86_64-linux.tar.gz" "every later request"
	# With --version, there is no `latest` request at all.
	srv_mark
	use_home "$T/home2"
	inst_net --version 1.0.0
	expect_rc 0
	if srv_paths | grep -v '^/releases/download/v1.0.0/' >/dev/null; then fail "--version fetched outside v1.0.0/: $(srv_paths)"; fi
}

TESTS="$TESTS t_older_version_refused"
t_older_version_refused() {
	serve 1.1.0
	inst_net
	expect_rc 0
	serve 1.0.0 1.1.0
	before=$(snap "$TH")
	inst_net
	expect_fail "a downgrade without --version"
	expect_out 'Eitri 1.1.0 is installed, and the release offered is older (1.0.0)'
	expect_out '--version 1.0.0'
	expect_eq "$(snap "$TH")" "$before" "the home after a refused downgrade"
	inst_net --version 1.0.0
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the version after an explicit downgrade"
}

TESTS="$TESTS t_rc_ordering"
t_rc_ordering() {
	serve 1.0.0-rc.1
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0-rc.1 "the release candidate"
	serve 1.0.0 1.0.0-rc.1
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "a final release upgrades its candidate"
	serve 1.0.0-rc.1 1.0.0
	inst_net
	expect_fail "a candidate is older than its final release"
	expect_out 'the release offered is older (1.0.0-rc.1)'
}

TESTS="$TESTS t_bad_signature"
t_bad_signature() {
	serve 1.0.0
	d=$(served 1.0.0)
	# The signature is for different content.
	printf 'tampered\n' >>"$d/SHA256SUMS"
	relatest 1.0.0
	inst_net
	expect_fail "a bad signature"
	expect_out 'the signature on SHA256SUMS does not verify'
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_deleted_sig"
t_deleted_sig() {
	serve 1.0.0
	rm "$(served 1.0.0)/SHA256SUMS.sig" "$S/srv/releases/latest/download/SHA256SUMS.sig"
	inst_net
	expect_fail "a missing .sig with a key listed"
	expect_out 'SHA256SUMS.sig'
	expect_out 'a release without a signature is refused'
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_unlisted_key"
t_unlisted_key() {
	serve 1.0.0
	d=$(served 1.0.0)
	sign_sums "$d" other
	relatest 1.0.0
	inst_net
	expect_fail "a signature by an unlisted key"
	expect_out 'the signature on SHA256SUMS does not verify'
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_missing_ssh_keygen"
t_missing_ssh_keygen() {
	serve 1.0.0
	inst_net --path-tail "$S/path-no-ssh-keygen" --
	expect_rc 0
	expect_count 'ssh-keygen was not found, so the release signature cannot be checked' 1
	expect_eq "$(installed_version)" 1.0.0 "the version installed without ssh-keygen"
}

TESTS="$TESTS t_no_embedded_key"
t_no_embedded_key() {
	# $INSTALLER is the harness's unkeyed copy (setup_keys): the committed release-signers lists the
	# owner's release key, so the real install.sh carries one; this is the branch without a key.
	serve 1.0.0
	rm "$(served 1.0.0)/SHA256SUMS.sig"
	srv_mark
	inst -- --base-url "http://127.0.0.1:$PORT"
	expect_rc 0
	expect_out 'this installer carries no release key: checksums only detect corruption'
	expect_eq "$(installed_version)" 1.0.0 "the version installed without a key"
	if srv_paths | grep -F SHA256SUMS.sig >/dev/null; then fail "a keyless installer fetched a .sig"; fi
}

TESTS="$TESTS t_release_signers_warning"
t_release_signers_warning() {
	serve 1.0.0
	inst_net
	expect_rc 0
	expect_out "checking signatures against $SIGNERS (--release-signers) instead of the key built into this installer"
	expect_out 'signature on SHA256SUMS: good (release@eitri)'
}

TESTS="$TESTS t_embedded_signers_byte_equal"
t_embedded_signers_byte_equal() {
	awk '
		/^EITRI_RELEASE_SIGNERS$/ { inside = 0 }
		inside { print }
		/^[[:space:]]*cat <<.EITRI_RELEASE_SIGNERS.$/ { inside = 1; n++ }
		END { if (n != 1) exit 1 }' "$REAL_INSTALLER" >"$T/embedded" || fail "not exactly one embedded signers block"
	if ! cmp -s "$T/embedded" "$PKG/release-signers"; then
		fail "the embedded signers block differs from packaging/release-signers: $(diff "$T/embedded" "$PKG/release-signers")"
	fi
	# The committed file lists the release key (spec §4.4): exactly one key line, in the shape the
	# file's own header gives -- identity, namespaces option, key type, key, and no comment field.
	_eb_keys=$(grep -E '^[[:space:]]*[^#[:space:]]' "$PKG/release-signers")
	expect_eq "$(printf '%s\n' "$_eb_keys" | grep -c .)" 1 "the number of key lines in packaging/release-signers"
	expect_eq "$(printf '%s\n' "$_eb_keys" | awk '{ print NF, $1, $2, $3 }')" \
		'4 release@eitri namespaces="eitri-release" ssh-ed25519' "the shape of the key line"
}

# The real install.sh, the file every release ships and the only one that holds the owner's key:
# the keys its embedded block lists are exactly the key lines of packaging/release-signers, and run
# with no --release-signers it checks a release against them alone. The harness's test key signs
# every fixture release, so a release it signed must be refused, and so must an unsigned one.

TESTS="$TESTS t_real_installer_embeds_the_listed_key"
t_real_installer_embeds_the_listed_key() {
	awk '
		/^EITRI_RELEASE_SIGNERS$/ { inside = 0 }
		inside { print }
		/^[[:space:]]*cat <<.EITRI_RELEASE_SIGNERS.$/ { inside = 1 }' "$REAL_INSTALLER" |
		grep -E '^[[:space:]]*[^#[:space:]]' >"$T/embedded-keys"
	grep -E '^[[:space:]]*[^#[:space:]]' "$PKG/release-signers" >"$T/listed-keys"
	if [ ! -s "$T/listed-keys" ]; then fail "packaging/release-signers lists no key: the first final release needs the owner's release key"; fi
	if ! cmp -s "$T/embedded-keys" "$T/listed-keys"; then
		fail "install.sh embeds $(cat "$T/embedded-keys"), release-signers lists $(cat "$T/listed-keys")"
	fi
	# The harness's own key, which signs every fixture release, is not among them.
	if grep -F -f "$SIGNERS" "$T/embedded-keys" >/dev/null; then fail "the real installer embeds the harness's test key"; fi
}

TESTS="$TESTS t_real_installer_refuses_what_its_key_did_not_sign"
t_real_installer_refuses_what_its_key_did_not_sign() {
	INSTALLER_UNDER_TEST=$REAL_INSTALLER
	# A release signed by the harness's test key, with no --release-signers to say otherwise.
	serve 1.0.0
	srv_mark
	inst -- --base-url "http://127.0.0.1:$PORT"
	expect_fail "a release signed by a key the real installer does not list"
	expect_out 'the signature on SHA256SUMS does not verify'
	expect_no_out 'carries no release key'
	expect_absent "$TH/.local/lib/eitri"
	if ! srv_paths | grep -F -x /releases/download/v1.0.0/SHA256SUMS.sig >/dev/null; then fail "no .sig was fetched: $(srv_paths)"; fi
	# A release with no SHA256SUMS.sig at all.
	serve 1.0.0
	rm "$(served 1.0.0)/SHA256SUMS.sig" "$S/srv/releases/latest/download/SHA256SUMS.sig"
	inst -- --base-url "http://127.0.0.1:$PORT"
	expect_fail "an unsigned release under the real installer"
	expect_out 'a release without a signature is refused'
	expect_absent "$TH/.local/lib/eitri"
	# The offline route: the test key's signature with --tarball, and --tarball with no --sig.
	d=$S/fix/v1.0.0
	tb=$d/eitri-1.0.0-x86_64-linux.tar.gz
	inst -- --tarball "$tb" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig"
	expect_fail "--sig by the test key under the real installer"
	expect_out 'the signature on SHA256SUMS does not verify'
	expect_absent "$TH/.local/lib/eitri"
	inst -- --tarball "$tb" --sums "$d/SHA256SUMS"
	expect_fail "--tarball without --sig under the real installer"
	expect_out '--tarball needs --sig'
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_no_check_novalidate"
t_no_check_novalidate() {
	if grep -n 'check-novalidate' "$REAL_INSTALLER"; then fail "the installer names check-novalidate"; fi
	# shellcheck disable=SC2016 # the literal line, variables and all
	expect_eq "$(grep -c -F 'ssh-keygen -Y verify -f "$NV_SIGNERS" -I "$NV_SIGNER_IDENTITY" -n "$NV_SIGNATURE_NAMESPACE" -s "$2" <"$1"' "$REAL_INSTALLER")" 1 \
		"the one pinned verify line"
	expect_eq "$(sed -n "s/^NV_SIGNER_IDENTITY='\(.*\)'$/\1/p; s/^NV_SIGNATURE_NAMESPACE='\(.*\)'$/\1/p" "$REAL_INSTALLER" | tr '\n' ' ')" \
		'release@eitri eitri-release ' "the identity and namespace"
}

# The embedded-key branch (D13), the one every final release's installer takes: $KEYED_INSTALLER
# is the unkeyed installer with the test key added to its signers block (harness.sh's setup_keys),
# run with no --release-signers at all.

TESTS="$TESTS t_embedded_key_good_signature"
t_embedded_key_good_signature() {
	INSTALLER_UNDER_TEST=$KEYED_INSTALLER
	serve 1.0.0
	srv_mark
	inst -- --base-url "http://127.0.0.1:$PORT"
	expect_rc 0
	expect_out 'signature on SHA256SUMS: good (release@eitri)'
	expect_no_out '--release-signers'
	expect_no_out 'carries no release key'
	expect_eq "$(installed_version)" 1.0.0 "installed under the embedded key"
	if ! srv_paths | grep -F -x /releases/download/v1.0.0/SHA256SUMS.sig >/dev/null; then fail "no .sig was fetched: $(srv_paths)"; fi
}

TESTS="$TESTS t_embedded_key_refusals"
t_embedded_key_refusals() {
	INSTALLER_UNDER_TEST=$KEYED_INSTALLER
	# A deleted .sig.
	serve 1.0.0
	rm "$(served 1.0.0)/SHA256SUMS.sig" "$S/srv/releases/latest/download/SHA256SUMS.sig"
	inst -- --base-url "http://127.0.0.1:$PORT"
	expect_fail "a missing .sig under the embedded key"
	expect_out 'a release without a signature is refused'
	expect_absent "$TH/.local/lib/eitri"
	# A .sig by a key the installer does not list.
	serve 1.0.0
	sign_sums "$(served 1.0.0)" other
	relatest 1.0.0
	inst -- --base-url "http://127.0.0.1:$PORT"
	expect_fail "a .sig by an unlisted key under the embedded key"
	expect_out 'the signature on SHA256SUMS does not verify'
	expect_absent "$TH/.local/lib/eitri"
	# A SHA256SUMS changed after it was signed.
	serve 1.0.0
	printf 'tampered\n' >>"$(served 1.0.0)/SHA256SUMS"
	relatest 1.0.0
	inst -- --base-url "http://127.0.0.1:$PORT"
	expect_fail "a tampered SHA256SUMS under the embedded key"
	expect_out 'the signature on SHA256SUMS does not verify'
	expect_absent "$TH/.local/lib/eitri"
	expect_absent "$TH/.cache"
}

TESTS="$TESTS t_embedded_key_offline"
t_embedded_key_offline() {
	INSTALLER_UNDER_TEST=$KEYED_INSTALLER
	d=$S/fix/v1.0.0
	tb=$d/eitri-1.0.0-x86_64-linux.tar.gz
	inst -- --tarball "$tb" --sums "$d/SHA256SUMS"
	expect_fail "--tarball without --sig under the embedded key"
	expect_out '--tarball needs --sig'
	expect_absent "$TH/.local/lib/eitri"
	mkdir -p "$T/other"
	cp "$d/SHA256SUMS" "$T/other/SHA256SUMS"
	sign_sums "$T/other" other
	inst -- --tarball "$tb" --sums "$d/SHA256SUMS" --sig "$T/other/SHA256SUMS.sig"
	expect_fail "--sig by an unlisted key under the embedded key"
	expect_out 'the signature on SHA256SUMS does not verify'
	expect_absent "$TH/.local/lib/eitri"
	inst -- --tarball "$tb" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig"
	expect_rc 0
	expect_out 'signature on SHA256SUMS: good (release@eitri)'
	expect_eq "$(installed_version)" 1.0.0 "installed from files under the embedded key"
}

TESTS="$TESTS t_embedded_key_dry_run"
t_embedded_key_dry_run() {
	# A dry run writes nothing, so the embedded key never reaches the file ssh-keygen reads: it says
	# it would check the .sig, as the network dry run does. It once ran ssh-keygen against the
	# missing file and reported a good signature as one that "does not verify".
	INSTALLER_UNDER_TEST=$KEYED_INSTALLER
	d=$S/fix/v1.0.0
	tb=$d/eitri-1.0.0-x86_64-linux.tar.gz
	before=$(snap "$TH")
	inst -- --tarball "$tb" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" --dry-run
	expect_rc 0
	expect_out "would check $d/SHA256SUMS.sig with ssh-keygen -Y verify against the release key built into this installer"
	expect_no_out 'does not verify'
	expect_no_out 'signature on SHA256SUMS: good'
	expect_out 'dry run: nothing was changed'
	expect_eq "$(snap "$TH")" "$before" "the home after an offline dry run under the embedded key"
	# The .sig is still required, dry run or not.
	inst -- --tarball "$tb" --sums "$d/SHA256SUMS" --dry-run
	expect_fail "an offline dry run without --sig under the embedded key"
	expect_out '--tarball needs --sig'
	# Over the network: the .sig would be fetched and checked, and nothing is.
	serve 1.0.0
	srv_mark
	inst -- --base-url "http://127.0.0.1:$PORT" --dry-run
	expect_rc 0
	expect_out "would download http://127.0.0.1:$PORT/releases/download/v1.0.0/SHA256SUMS.sig and check it with ssh-keygen -Y verify (not checked in a dry run)"
	expect_no_out 'does not verify'
	if srv_paths | grep -F .sig >/dev/null; then fail "a dry run fetched the .sig: $(srv_paths)"; fi
	expect_eq "$(snap "$TH")" "$before" "the home after a network dry run under the embedded key"
}

# evil_dir DIR: DIR holds v1.0.0's tarball with an `EVIL` shell under the genuine name, as
# .evil.tar.gz, and the genuine release files (tarball, SHA256SUMS, SHA256SUMS.sig); .evil-sums is a
# SHA256SUMS listing the evil tarball instead.
evil_dir() {
	_ed_top=eitri-1.0.0-x86_64-linux
	rm -rf "$T/evil-build" "$1"
	mkdir -p "$T/evil-build" "$1"
	tar -C "$T/evil-build" -xzf "$S/fix/v1.0.0/$_ed_top.tar.gz"
	printf '#!/bin/sh\necho "EVIL shell"\n' >"$T/evil-build/$_ed_top/lib/eitri/shell"
	tar -C "$T/evil-build" -czf "$1/.evil.tar.gz" "$_ed_top"
	cp "$S/fix/v1.0.0/$_ed_top.tar.gz" "$S/fix/v1.0.0/SHA256SUMS" "$S/fix/v1.0.0/SHA256SUMS.sig" "$1/"
	{
		printf '%s  %s\n' "$(sha256sum <"$1/.evil.tar.gz" | cut -d' ' -f1)" "$_ed_top.tar.gz"
		grep -v 'x86_64-linux\.tar\.gz$' "$1/SHA256SUMS"
	} >"$1/.evil-sums"
}

TESTS="$TESTS t_offline_sums_swapped_after_read"
t_offline_sums_swapped_after_read() {
	# --tarball/--sums/--sig name files in a directory someone else can rename files in. The
	# installer once read --sums twice: its text (which chose the tarball's hash) on the first
	# read, the signature check on a second -- so SHA256SUMS swapped for the genuine one right after
	# the first read passed the signature and installed the evil tarball. It reads, checks and uses
	# only its own copy now. The cat stub makes the swap right after the file is read.
	INSTALLER_UNDER_TEST=$KEYED_INSTALLER
	d=$T/shared
	evil_dir "$d"
	top=eitri-1.0.0-x86_64-linux
	mv "$d/.evil.tar.gz" "$d/$top.tar.gz"
	mv "$d/SHA256SUMS" "$d/.legit-sums"
	cp "$d/.evil-sums" "$d/SHA256SUMS"
	printf '%s\n%s\n' "$d/.legit-sums" "$d/SHA256SUMS" >"$S/logs/swap"
	inst --stubs "$S/stubs-swapcat" -- --tarball "$d/$top.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig"
	expect_fail "a SHA256SUMS swapped after the installer read it"
	expect_out 'the signature on SHA256SUMS does not verify'
	expect_absent "$TH/.local/lib/eitri"
	if grep -r EVIL "$TH" >/dev/null 2>&1; then fail "the evil tarball was installed"; fi
}

TESTS="$TESTS t_offline_tarball_swapped_after_hash"
t_offline_tarball_swapped_after_hash() {
	# The same for --tarball: it was hashed on one read and unpacked on another, so a tarball
	# swapped right after it was hashed was installed. The sha256sum stub makes the swap right after
	# the first hash; the installer hashes and unpacks its own copy, so the swap changes nothing.
	INSTALLER_UNDER_TEST=$KEYED_INSTALLER
	d=$T/shared
	evil_dir "$d"
	top=eitri-1.0.0-x86_64-linux
	printf '%s\n%s\n' "$d/.evil.tar.gz" "$d/$top.tar.gz" >"$S/logs/swap"
	inst --stubs "$S/stubs-swapsha" -- --tarball "$d/$top.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig"
	expect_rc 0
	expect_out 'signature on SHA256SUMS: good (release@eitri)'
	if [ ! -s "$S/logs/swap.log" ]; then fail "the tarball was never swapped: the test did not run the race"; fi
	if grep -r EVIL "$TH" >/dev/null 2>&1; then fail "the swapped-in tarball was installed"; fi
	if ! grep -q 'stub shell 1.0.0' "$TH/.local/lib/eitri/shell"; then fail "the genuine shell was not installed"; fi
}

TESTS="$TESTS t_release_signers_unusable_refused"
t_release_signers_unusable_refused() {
	# --release-signers replaces the key built in. A file that cannot be read was once taken for
	# one that lists no key: signature checking was skipped -- even by an installer with a key
	# built in, even with a --sig given -- and an evil tarball installed. A file that lists no key
	# would turn checking off the same way, which is never what the option is for.
	INSTALLER_UNDER_TEST=$KEYED_INSTALLER
	d=$T/shared
	evil_dir "$d"
	top=eitri-1.0.0-x86_64-linux
	mv "$d/.evil.tar.gz" "$d/$top.tar.gz"
	cp "$d/.evil-sums" "$d/SHA256SUMS"
	cp "$SIGNERS" "$T/signers"
	chmod 000 "$T/signers"
	if [ ! -r "$T/signers" ]; then
		inst -- --tarball "$d/$top.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" --release-signers "$T/signers"
		expect_fail "an unreadable --release-signers"
		expect_out "--release-signers $T/signers cannot be read"
		expect_no_out 'carries no release key'
		expect_absent "$TH/.local/lib/eitri"
		serve 1.0.0
		inst -- --base-url "http://127.0.0.1:$PORT" --release-signers "$T/signers"
		expect_fail "an unreadable --release-signers, over the network"
		expect_out "--release-signers $T/signers cannot be read"
		expect_absent "$TH/.local/lib/eitri"
	fi
	chmod 0644 "$T/signers"
	printf '# no key here\n\n' >"$T/nokey"
	inst -- --tarball "$d/$top.tar.gz" --sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" --release-signers "$T/nokey"
	expect_fail "a --release-signers that lists no key"
	expect_out "--release-signers $T/nokey lists no key"
	expect_no_out 'carries no release key'
	expect_absent "$TH/.local/lib/eitri"
	if grep -r EVIL "$TH" >/dev/null 2>&1; then fail "the evil tarball was installed"; fi
}

TESTS="$TESTS t_cache_writable_by_others_refused"
t_cache_writable_by_others_refused() {
	# The downloads are checked, then used, in <cache>/eitri/download: a directory other users can
	# write to (a pre-made /tmp-style one) would let them replace a file between the two.
	serve 1.0.0
	mkdir -p "$TH/.cache/eitri"
	chmod 0777 "$TH/.cache/eitri"
	inst_net
	expect_fail "a cache directory other users can write to"
	expect_out "other users can write to $TH/.cache/eitri"
	expect_absent "$TH/.local/lib/eitri"
	chmod 0755 "$TH/.cache/eitri"
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the version once the cache is private"
}

# shared_group_stubs: a PRE_STUBS directory whose getent reports this user's primary group with a
# second member -- a group really shared with someone else, whatever this machine's own layout is
# (the host and the dash image both give the test user a private group of its own).
shared_group_stubs() {
	mkdir -p "$T/stubs-getent"
	cat >"$T/stubs-getent/getent" <<'STUB'
#!/bin/sh
if [ "$1" = group ]; then printf '%s:x:%s:%s,someone-else\n' "$2" "$(id -g)" "$(id -un)"; exit 0; fi
exec /usr/bin/getent "$@"
STUB
	chmod +x "$T/stubs-getent/getent"
	printf '%s\n' "$T/stubs-getent"
}

# shared_primary_gid_stubs: a PRE_STUBS directory whose getent lists a second account ("bob") whose
# primary group is this user's own -- a member getent's group entry never shows.
shared_primary_gid_stubs() {
	mkdir -p "$T/stubs-getent-passwd"
	cat >"$T/stubs-getent-passwd/getent" <<'STUB'
#!/bin/sh
if [ "$1" = passwd ] && [ $# -eq 1 ]; then
	/usr/bin/getent passwd
	printf 'bob:x:%s:%s::/home/bob:/bin/sh\n' 59999 "$(id -g)"
	exit 0
fi
exec /usr/bin/getent "$@"
STUB
	chmod +x "$T/stubs-getent-passwd/getent"
	printf '%s\n' "$T/stubs-getent-passwd"
}

# duplicate_gid_stubs: a PRE_STUBS directory whose getent lists a second group ("shared") with this
# user's primary GID -- the groupadd --non-unique case a lookup by name never sees.
duplicate_gid_stubs() {
	mkdir -p "$T/stubs-getent-dupgid"
	cat >"$T/stubs-getent-dupgid/getent" <<'STUB'
#!/bin/sh
if [ "$1" = group ] && [ $# -eq 1 ]; then
	/usr/bin/getent group
	printf 'shared:x:%s:bob\n' "$(id -g)"
	exit 0
fi
exec /usr/bin/getent "$@"
STUB
	chmod +x "$T/stubs-getent-dupgid/getent"
	printf '%s\n' "$T/stubs-getent-dupgid"
}

# own_private_group_here: 0 when the test user has a user-private group (the case own_private_group
# accepts), so the tests that rely on it can say when they are not exercising it.
own_private_group_here() {
	[ "$(id -un)" = "$(id -gn)" ] || return 1
	case $(getent group "$(id -gn)" | cut -d: -f4) in '' | "$(id -un)") return 0 ;; *) return 1 ;; esac
}

TESTS="$TESTS t_cache_group_writable_refused"
t_cache_group_writable_refused() {
	# installer-claude-8: the original check tested only the other-write bit (`-perm -0002`), so a
	# group-writable (0770) cache passed it even though every member of that group can still replace
	# a download between check and use.
	serve 1.0.0
	mkdir -p "$TH/.cache/eitri"
	chmod 0770 "$TH/.cache/eitri"
	PRE_STUBS=$(shared_group_stubs)
	inst_net
	expect_fail "a group-writable cache directory"
	expect_out "other users can write to $TH/.cache/eitri"
	expect_absent "$TH/.local/lib/eitri"
	chmod 0755 "$TH/.cache/eitri"
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the version once the cache is private"
}

TESTS="$TESTS t_cache_symlinked_refused"
t_cache_symlinked_refused() {
	# installer-claude-8 (+installer-codex-1): a symlink at <cache>/eitri -- planted by another
	# user in a shared, sticky XDG_CACHE_HOME such as /tmp -- can be repointed at any time, after
	# whatever the permission checks saw when they ran.
	serve 1.0.0
	mkdir -p "$T/elsewhere" "$TH/.cache"
	ln -s "$T/elsewhere" "$TH/.cache/eitri"
	inst_net
	expect_fail "a symlinked cache directory"
	expect_out "$TH/.cache/eitri is a symlink"
	expect_absent "$TH/.local/lib/eitri"
	rm -f "$TH/.cache/eitri"
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the version once the cache is a real directory"
}

TESTS="$TESTS t_cache_parent_writable_without_sticky_refused"
t_cache_parent_writable_without_sticky_refused() {
	# installer-claude-8: a world-writable, non-sticky XDG_CACHE_HOME lets another user rename
	# <cache>/eitri away and put their own directory of that name in its place between any two
	# checks -- the original guard only ever looked at NV_CACHE_NV's own permissions, never its
	# parent's.
	serve 1.0.0
	mkdir -p "$TH/.cache"
	chmod 0777 "$TH/.cache"
	inst_net
	expect_fail "a non-sticky, world-writable cache parent"
	expect_out "$TH/.cache is writable by its group or by anyone, and not sticky"
	expect_absent "$TH/.local/lib/eitri"
	chmod +t "$TH/.cache"
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the version once the parent is sticky"
}

TESTS="$TESTS t_cache_parent_group_writable_without_sticky_refused"
t_cache_parent_group_writable_without_sticky_refused() {
	# M2 (v1-dist whole-branch review, 2026-09-28): the parent check only ever tested the
	# other-write bit (-perm -0002), the same gap installer-claude-8 already closed on
	# NV_CACHE_NV's own permissions (t_cache_group_writable_refused) but never on its parent's -- a
	# group-writable (0770), non-sticky XDG_CACHE_HOME lets any member of that group rename
	# <cache>/eitri away between checks, exactly as a world-writable one does.
	serve 1.0.0
	mkdir -p "$TH/.cache"
	chmod 0770 "$TH/.cache"
	PRE_STUBS=$(shared_group_stubs)
	inst_net
	expect_fail "a group-writable, non-sticky cache parent"
	expect_out "$TH/.cache is writable by its group or by anyone, and not sticky"
	expect_absent "$TH/.local/lib/eitri"
	chmod 0755 "$TH/.cache"
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the version once the parent is private"
}

TESTS="$TESTS t_cache_group_writable_own_private_group_allowed"
t_cache_group_writable_own_private_group_allowed() {
	# rc.2's e2e (2026-09-28): a default Ubuntu user has a group of their own and umask 002, so tools
	# leave ~/.cache (and anything under it) at 0775. Refusing that refused `eitri setup` right
	# after a plain .deb install. Group-write on the user's own private group lets no one else in.
	if ! own_private_group_here; then
		printf 'note: the test user has no private group here; nothing to check\n'
		return 0
	fi
	serve 1.0.0
	mkdir -p "$TH/.cache/eitri"
	chmod 0775 "$TH/.cache" "$TH/.cache/eitri"
	inst_net
	expect_rc 0 "a 0775 ~/.cache and ~/.cache/eitri on the user's own private group"
	expect_eq "$(installed_version)" 1.0.0 "the version with a 0775 cache on a private group"
}

TESTS="$TESTS t_cache_group_writable_shared_primary_gid_refused"
t_cache_group_writable_shared_primary_gid_refused() {
	# Codex's review of the private-group allowance: another account whose primary group is this
	# user's group is absent from getent's member list, but can write to a 0775 directory of that
	# group all the same -- so the account list decides too.
	if ! own_private_group_here; then
		printf 'note: the test user has no private group here; nothing to check\n'
		return 0
	fi
	serve 1.0.0
	mkdir -p "$TH/.cache"
	chmod 0775 "$TH/.cache"
	PRE_STUBS=$(shared_primary_gid_stubs)
	inst_net
	expect_fail "a 0775 cache parent whose group is another account's primary group too"
	expect_out "$TH/.cache is writable by its group or by anyone, and not sticky"
}

TESTS="$TESTS t_cache_parent_acl_named_writer_refused"
t_cache_parent_acl_named_writer_refused() {
	# Codex's rc.2 review: an extended ACL entry for another user makes a directory writable to them
	# while its mode reads 0775 (the group bits are the ACL mask), so the private-group allowance must
	# never apply to a directory that has one.
	if ! own_private_group_here; then
		printf 'note: the test user has no private group here; nothing to check\n'
		return 0
	fi
	if ! command -v setfacl >/dev/null 2>&1; then
		printf 'note: no setfacl here; nothing to check\n'
		return 0
	fi
	serve 1.0.0
	mkdir -p "$TH/.cache"
	chmod 0755 "$TH/.cache"
	if ! setfacl -m "u:$(id -un):rwx" -m m::rwx "$TH/.cache" 2>/dev/null; then
		printf 'note: this filesystem takes no ACLs; nothing to check\n'
		return 0
	fi
	inst_net
	expect_fail "a cache parent with an extended ACL, on the user's own private group"
	expect_out "$TH/.cache is writable by its group or by anyone, and not sticky"
	setfacl -b "$TH/.cache"
	chmod 0775 "$TH/.cache"
	inst_net
	expect_rc 0 "the same directory once its ACL is gone"
}

TESTS="$TESTS t_cache_symlinked_parent_acl_refused_without_getfacl"
t_cache_symlinked_parent_acl_refused_without_getfacl() {
	# Codex's rc.3 review: with ~/.cache a symlink to a directory carrying an ACL, `ls -ld` described
	# the link, not the directory, so without getfacl the ACL went unseen. A getfacl stub that reports
	# no ACL leaves ls as the only check here.
	if ! own_private_group_here; then
		printf 'note: the test user has no private group here; nothing to check\n'
		return 0
	fi
	if ! command -v setfacl >/dev/null 2>&1; then
		printf 'note: no setfacl here; nothing to check\n'
		return 0
	fi
	serve 1.0.0
	mkdir -p "$T/realcache"
	chmod 0755 "$T/realcache"
	if ! setfacl -m "u:$(id -un):rwx" -m m::rwx "$T/realcache" 2>/dev/null; then
		printf 'note: this filesystem takes no ACLs; nothing to check\n'
		return 0
	fi
	rm -rf "$TH/.cache"
	ln -s "$T/realcache" "$TH/.cache"
	mkdir -p "$T/stubs-getfacl"
	printf '#!/bin/sh\nprintf "user::rwx\\ngroup::rwx\\nother::r-x\\n"\n' >"$T/stubs-getfacl/getfacl"
	chmod +x "$T/stubs-getfacl/getfacl"
	PRE_STUBS=$T/stubs-getfacl
	inst_net
	expect_fail "a symlinked cache whose target has an ACL, with getfacl reporting none"
	expect_out "is writable by its group or by anyone, and not sticky"
}

TESTS="$TESTS t_cache_group_writable_duplicate_gid_refused"
t_cache_group_writable_duplicate_gid_refused() {
	# Codex's rc.2 review: a second group sharing the private group's GID gives its members the same
	# access without appearing under the private group's name.
	if ! own_private_group_here; then
		printf 'note: the test user has no private group here; nothing to check\n'
		return 0
	fi
	serve 1.0.0
	mkdir -p "$TH/.cache"
	chmod 0775 "$TH/.cache"
	PRE_STUBS=$(duplicate_gid_stubs)
	inst_net
	expect_fail "a 0775 cache parent whose GID another group shares"
	expect_out "$TH/.cache is writable by its group or by anyone, and not sticky"
}

TESTS="$TESTS t_cache_group_list_cut_short_refused"
t_cache_group_list_cut_short_refused() {
	# Codex's rc.3 review: a group list that stops early (getent exiting non-zero after printing the
	# user's own group) must not count as "exactly one group with this GID".
	if ! own_private_group_here; then
		printf 'note: the test user has no private group here; nothing to check\n'
		return 0
	fi
	serve 1.0.0
	mkdir -p "$TH/.cache"
	chmod 0775 "$TH/.cache"
	mkdir -p "$T/stubs-getent-short"
	cat >"$T/stubs-getent-short/getent" <<'STUB'
#!/bin/sh
if [ "$1" = group ] && [ $# -eq 1 ]; then
	/usr/bin/getent group "$(id -gn)"
	exit 1
fi
exec /usr/bin/getent "$@"
STUB
	chmod +x "$T/stubs-getent-short/getent"
	PRE_STUBS=$T/stubs-getent-short
	inst_net
	expect_fail "a 0775 cache parent when the group list is cut short"
	expect_out "$TH/.cache is writable by its group or by anyone, and not sticky"
}

TESTS="$TESTS t_cache_other_writable_own_private_group_still_refused"
t_cache_other_writable_own_private_group_still_refused() {
	# The private-group allowance covers the group bit only: other-write without the sticky bit is
	# refused whoever the group is.
	serve 1.0.0
	mkdir -p "$TH/.cache"
	chmod 0777 "$TH/.cache"
	inst_net
	expect_fail "a world-writable, non-sticky cache parent even on a private group"
	expect_out "$TH/.cache is writable by its group or by anyone, and not sticky"
}

TESTS="$TESTS t_cache_owned_by_another_user_refused"
t_cache_owned_by_another_user_refused() {
	# M2 (v1-dist whole-branch review, 2026-09-28): `mkdir -p` is a silent no-op on a
	# <cache>/eitri that already exists, so one planted in advance by another user (in a shared
	# XDG_CACHE_HOME) was never checked for who owns it, only for its own write bits -- which say
	# nothing about whether that owner can already read, or later replace, whatever a download
	# writes inside it. This sandbox cannot chown a real directory to another uid without root (the
	# Global Constraints forbid sudo here), so find-fake-owner reports $TH/.cache/eitri itself as
	# not ours, exactly as the real find would for one actually owned by someone else.
	serve 1.0.0
	mkdir -p "$TH/.cache/eitri"
	chmod 0700 "$TH/.cache/eitri"
	printf '%s\n' "$TH/.cache/eitri" >"$S/logs/fake-owner-path"
	inst --stubs "$S/stubs-fakeowner" -- --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	expect_fail "a cache directory owned by another user"
	expect_out "$TH/.cache/eitri is owned by another user"
	expect_absent "$TH/.local/lib/eitri"
	rm -f "$S/logs/fake-owner-path"
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the version once the cache is ours"
}

TESTS="$TESTS t_cache_parent_owned_by_another_user_refused"
t_cache_parent_owned_by_another_user_refused() {
	# M2 (v1-dist whole-branch review, 2026-09-28): a sticky 1777 <cache> owned by another user
	# passed the old check (which only ever looked at write bits): the sticky bit stops other
	# non-owner users from renaming entries away, but never stops the directory's OWN owner from
	# doing exactly that -- so a shared, sticky XDG_CACHE_HOME owned by someone else is still unsafe.
	# Same sandbox limitation as the test above: find-fake-owner stands in for a real chown.
	serve 1.0.0
	mkdir -p "$TH/.cache"
	chmod 1777 "$TH/.cache"
	printf '%s\n' "$TH/.cache" >"$S/logs/fake-owner-path"
	inst --stubs "$S/stubs-fakeowner" -- --base-url "http://127.0.0.1:$PORT" --release-signers "$SIGNERS"
	expect_fail "a sticky cache parent owned by another user"
	expect_out "$TH/.cache is owned by another user"
	expect_absent "$TH/.local/lib/eitri"
	rm -f "$S/logs/fake-owner-path"
	inst_net
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "the version once the parent is ours"
}
