# `eitri sidecar update` and `eitri sidecar check`: --sidecar-update and --sidecar-check. Sourced by
# harness.sh. The trust chain from the signature down, what is decided before anything is spent, the
# build with no install script, the artifact against the signed manifest, the probe, and the one write
# that makes a build the selected one. Every failure leaves `selected` and updates/ as they were.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

UPD_BUNDLED=0.2.0
UPD_ZEROS=0000000000000000000000000000000000000000000000000000000000000000

# upd_setup: an install that can take updates: eitri-setup with a RELEASE carrying the contract keys, a
# `shell` beside it answering --sidecar-probe, and a present bundled sidecar. Sets UPD_ROOT.
upd_setup() {
	sc_setup_release 1.0.0 "$REV_A"
	UPD_LIB=${INSTALLER_UNDER_TEST%/*}
	{
		echo "SIDECAR_VERSION=$UPD_BUNDLED"
		echo "SIDECAR_PROTOCOL_MAJOR=3"
		sed -n '/^SIDECAR_REQUIRED_CAPABILITIES=/p' "$PKG/sidecar-contract.env"
	} >>"$UPD_LIB/RELEASE"
	cat >"$UPD_LIB/shell" <<'SHELL_STUB'
#!/bin/sh
if [ "${1-}" != --sidecar-probe ] || [ ! -x "${2-}" ]; then
	echo "shell stub: unexpected argv: $*" >&2
	exit 64
fi
if [ -n "${EITRI_INSTALL_TEST_PROBE_REFUSAL-}" ]; then
	echo "an earlier stderr line" >&2
	echo "$EITRI_INSTALL_TEST_PROBE_REFUSAL" >&2
	exit 1
fi
if [ -n "${EITRI_INSTALL_TEST_PROBE_MUTE-}" ]; then exit 0; fi
echo "ok 0.0.0 protocol 3.24"
SHELL_STUB
	chmod 0755 "$UPD_LIB/shell"
	plant_sidecar aaaaaaa
	UPD_ROOT=$(data_of)/eitri/sidecar
}

# plant_update VERSION: a complete update at updates/VERSION.
plant_update() {
	_pu_dir=$UPD_ROOT/updates/$1
	_pu_line="verdandi-claude-sidecar $1 (protocol 3, node v22.23.2, build 0123456789abcdef)"
	mkdir -p "$_pu_dir"
	printf '#!/bin/sh\nprintf "%%s\\n" "%s" "claude-agent-sdk 0.3.252 (bundled claude code 2.1.252)" "supported claude code CLI: >=2.1.252 <3.0.0" "executable sources served: host_cli"\n' \
		"$_pu_line" >"$_pu_dir/verdandi-claude-sidecar"
	chmod 0755 "$_pu_dir/verdandi-claude-sidecar"
	printf 'VERDANDI_REV=%s\nSIDECAR_VERSION_LINE=%s\nBUILT_AT=2026-11-01T00:00:00Z\n' "$REV_A" "$_pu_line" >"$_pu_dir/BUILD"
}

plant_selected() {
	mkdir -p "$UPD_ROOT"
	printf 'version=%s\n' "$1" >"$UPD_ROOT/selected"
}

# upd_inst [WRAPPER-OPTION]... -- [INSTALLER-ARG]...: an installer run against the fixture server's
# sidecar releases, checked with the test key. UPD_V is the version the artifact will report.
upd_inst() {
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		--set "EITRI_INSTALL_TEST_GRPC_BASE_URL=http://127.0.0.1:$PORT/grpc" \
		--set "EITRI_INSTALL_TEST_SIDECAR_VERSION=${UPD_V:-0.0.0}" \
		--set "EITRI_INSTALL_TEST_NPM_LOG=$T/npm.log" \
		"$@" --base-url "http://127.0.0.1:$PORT/sc" --release-signers "$SIGNERS"
}
# shellcheck disable=SC2120 # wrapper options are optional
upd_update() { upd_inst "$@" -- --sidecar-update; }
# shellcheck disable=SC2120 # wrapper options are optional
upd_check() { upd_inst "$@" -- --sidecar-check; }
upd_version() {
	_uv=$1
	shift
	upd_inst "$@" -- --sidecar-update --version "$_uv"
}

# upd_state: upd_setup, with update 0.3.0 kept and selected, and that state remembered: a refusal
# has something to leave alone.
upd_state() {
	upd_setup
	plant_update 0.3.0
	plant_selected 0.3.0
	UPD_SNAP=$(snap "$UPD_ROOT")
}

# upd_refused WHAT TEXT: the last run failed saying TEXT, and it left no trace.
upd_refused() {
	expect_fail "$1"
	expect_out "$2"
	expect_eq "$(snap "$UPD_ROOT")" "$UPD_SNAP" "the sidecar directory after: $1"
	expect_absent "$TH/.cache/eitri"
}

upd_served() { printf '%s\n' "$S/srv/sc/releases/download/claude-sidecar-v$1"; }

# upd_snap_loose: snap of the sidecar directory, without the mtime of updates/ itself, which a run
# that made and removed a temporary directory there has touched.
upd_snap_loose() {
	snap "$UPD_ROOT" | sed -E 's#^(\./updates d [0-7]+ [0-9]+) [0-9.]+ #\1 - #'
}

# ---------------------------------------------------------------------------------------------
# The whole path

TESTS="$TESTS t_upd_happy_path"
t_upd_happy_path() {
	upd_setup
	# Node comes from the manifest. The RELEASE beside this eitri-setup names another Node, whose
	# download would fail: an update carries its own.
	sed -i -e 's/^NODE_VERSION=.*/NODE_VERSION=v22.1.1/' "$UPD_LIB/RELEASE"
	mk_sidecar_release 0.3.1
	srv_mark
	upd_update
	expect_rc 0
	d=$UPD_ROOT/updates/0.3.1
	expect_exec "$d/verdandi-claude-sidecar"
	expect_file "$d/BUILD"
	expect_file "$d/LICENSE.md"
	expect_file "$d/NODE-LICENSE"
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected"
	want=$(sed -n 's/^SIDECAR_VERSION_LINE=//p' "$d/BUILD")
	expect_eq "$("$d/verdandi-claude-sidecar" --version | sed -n 1p)" "$want" "the binary's first line against BUILD"
	expect_out 'signature on SHA256SUMS: good (release@eitri)'
	expect_out 'downloading Node and the Claude Agent SDK, then building the sidecar (1-3 min)'
	expect_out "downloading Anthropic's claude-agent-sdk 0.3.252 to this machine"
	expect_out 'sidecar 0.3.1 built, and this Eitri accepts it.'
	expect_out 'Open tabs keep the sidecar they started with. New Claude tabs use 0.3.1.'
	expect_out 'To go back: eitri sidecar use bundled'
	# Nothing of the run is left: no temporary name in updates/ or beside selected, no download, no lock.
	expect_eq "$(find "$UPD_ROOT" -name '.*' | wc -l)" 0 "temporary names under the sidecar directory"
	expect_absent "$TH/.cache/eitri"
	# Every file of the release comes from the tagged directory once its version is known.
	paths=$(srv_paths)
	for p in /sc/releases/latest/download/SHA256SUMS /sc/releases/latest/download/SHA256SUMS.sig \
		/sc/releases/latest/download/SIDECAR-RELEASE \
		/sc/releases/download/claude-sidecar-v0.3.1/claude-sidecar-0.3.1-source.tar.gz \
		"/dist/$NODE_FIXTURE_VERSION/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz" \
		"/grpc/grpc-tools/v$GRPC_FIXTURE_VERSION/linux-x64.tar.gz"; do
		if ! printf '%s\n' "$paths" | grep -F -x -e "$p" >/dev/null; then fail "$p was not requested: $paths"; fi
	done
}

TESTS="$TESTS t_upd_named_version"
t_upd_named_version() {
	upd_setup
	mk_sidecar_release 0.3.1
	mk_sidecar_release 0.3.2
	srv_mark
	UPD_V=0.3.1
	upd_version 0.3.1
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected"
	if srv_paths | grep -F latest >/dev/null; then fail "--version read latest: $(srv_paths)"; fi
}

TESTS="$TESTS t_upd_embedded_key"
t_upd_embedded_key() {
	upd_setup
	cp "$KEYED_INSTALLER" "$INSTALLER_UNDER_TEST"
	mk_sidecar_release 0.3.1
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		--set "EITRI_INSTALL_TEST_GRPC_BASE_URL=http://127.0.0.1:$PORT/grpc" \
		--set "EITRI_INSTALL_TEST_SIDECAR_VERSION=$UPD_V" \
		-- --sidecar-update --base-url "http://127.0.0.1:$PORT/sc"
	expect_rc 0
	expect_no_out '--release-signers'
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected"
}

# ---------------------------------------------------------------------------------------------
# The signature, and the paths around it

TESTS="$TESTS t_upd_bad_signature"
t_upd_bad_signature() {
	upd_state
	SC_KEY=other mk_sidecar_release 0.3.1
	upd_update
	upd_refused "a signature by another key" 'the signature on SHA256SUMS does not verify'
}

TESTS="$TESTS t_upd_signature_under_eitri_release"
t_upd_signature_under_eitri_release() {
	upd_state
	SC_NAMESPACE=eitri-release mk_sidecar_release 0.3.1
	upd_update
	upd_refused "a signature under the Eitri release namespace" 'the signature on SHA256SUMS does not verify'
}

# The other direction: a signature made for a sidecar release is no signature on an Eitri release.
TESTS="$TESTS t_upd_sidecar_signature_is_not_an_eitri_one"
t_upd_sidecar_signature_is_not_an_eitri_one() {
	serve 1.0.0
	for d in "$(served 1.0.0)" "$S/srv/releases/latest/download"; do
		rm -f "$d/SHA256SUMS.sig"
		ssh-keygen -q -Y sign -f "$S/keys/release" -n eitri-sidecar-release "$d/SHA256SUMS" 2>/dev/null
	done
	inst_net
	expect_fail "a sidecar signature on an Eitri release"
	expect_out 'the signature on SHA256SUMS does not verify'
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_upd_missing_sig"
t_upd_missing_sig() {
	upd_state
	mk_sidecar_release 0.3.1
	rm -f "$S/srv/sc/releases/latest/download/SHA256SUMS.sig"
	upd_update
	upd_refused "a release with no signature" 'a release without a signature is refused'
}

TESTS="$TESTS t_upd_no_ssh_keygen"
t_upd_no_ssh_keygen() {
	upd_state
	mk_sidecar_release 0.3.1
	srv_mark
	upd_update --path-tail "$S/path-no-ssh-keygen"
	upd_refused "no ssh-keygen" 'ssh-keygen was not found'
	expect_no_out '--insecure-skip-signature'
	expect_eq "$(srv_paths)" '' "requests made without ssh-keygen"
}

TESTS="$TESTS t_upd_empty_signers"
t_upd_empty_signers() {
	upd_state
	mk_sidecar_release 0.3.1
	srv_mark
	# This eitri-setup is the harness's unkeyed copy, so with no --release-signers no key is listed.
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		-- --sidecar-update --base-url "http://127.0.0.1:$PORT/sc"
	upd_refused "no key listed" 'lists no release key'
	expect_no_out 'checksums only detect corruption'
	expect_eq "$(srv_paths)" '' "requests made with no key listed"
	printf '# no key here\n' >"$T/nokey"
	inst -- --sidecar-update --base-url "http://127.0.0.1:$PORT/sc" --release-signers "$T/nokey"
	upd_refused "an empty --release-signers" 'lists no key'
}

TESTS="$TESTS t_upd_refused_flags"
t_upd_refused_flags() {
	upd_state
	mk_sidecar_release 0.3.1
	srv_mark
	for mode in --sidecar-update --sidecar-check; do
		upd_inst -- "$mode" --insecure-skip-signature
		upd_refused "--insecure-skip-signature on $mode" '--insecure-skip-signature does not apply'
		upd_inst -- "$mode" --dry-run
		upd_refused "--dry-run on $mode" '--dry-run does not apply'
		upd_inst -- "$mode" --tarball "$T/x"
		upd_refused "--tarball on $mode" '--tarball, --sums and --sig do not apply'
		upd_inst -- "$mode" --sums "$T/x"
		upd_refused "--sums on $mode" '--tarball, --sums and --sig do not apply'
		upd_inst -- "$mode" --sig "$T/x"
		upd_refused "--sig on $mode" '--tarball, --sums and --sig do not apply'
	done
	expect_eq "$(srv_paths)" '' "requests made by refused flags"
}

TESTS="$TESTS t_upd_needs_the_contract_keys"
t_upd_needs_the_contract_keys() {
	sc_setup_release 1.0.0 "$REV_A"
	plant_sidecar aaaaaaa
	mk_sidecar_release 0.3.1
	upd_inst -- --sidecar-update
	expect_fail "a RELEASE with no SIDECAR_* keys"
	expect_out 'cannot take sidecar updates'
}

# ---------------------------------------------------------------------------------------------
# Size limits, per file

TESTS="$TESTS t_upd_sums_over_the_limit"
t_upd_sums_over_the_limit() {
	upd_state
	mk_sidecar_release 0.3.1
	head -c 70000 /dev/zero >>"$S/srv/sc/releases/latest/download/SHA256SUMS"
	upd_update
	upd_refused "a SHA256SUMS over 64 KiB" 'could not download SHA256SUMS'
}

# --max-filesize cannot refuse in advance a response with no declared length: this curl never sees
# the option, so what stops the file is the size check after the download.
TESTS="$TESTS t_upd_size_checked_after_the_download"
t_upd_size_checked_after_the_download() {
	upd_state
	mk_sidecar_release 0.3.1
	head -c 70000 /dev/zero >>"$S/srv/sc/releases/latest/download/SHA256SUMS"
	mkdir "$T/nolimit"
	cat >"$T/nolimit/curl" <<'CURL_STUB'
#!/bin/sh
n=$#
while [ "$n" -gt 0 ]; do
	a=$1
	shift
	n=$((n - 1))
	if [ "$a" = --max-filesize ]; then
		shift
		n=$((n - 1))
		continue
	fi
	set -- "$@" "$a"
done
for c in /usr/bin/curl /bin/curl; do
	if [ -x "$c" ]; then exec "$c" "$@"; fi
done
exit 127
CURL_STUB
	chmod 0755 "$T/nolimit/curl"
	upd_update --stubs "$T/nolimit"
	upd_refused "a SHA256SUMS over 64 KiB, with no declared length" 'more than the 65536 bytes'
}

TESTS="$TESTS t_upd_limit_per_download"
t_upd_limit_per_download() {
	upd_setup
	mk_sidecar_release 0.3.1
	mkdir "$T/argv"
	cat >"$T/argv/curl" <<CURL_STUB
#!/bin/sh
printf '%s\n' "\$*" >>"$T/curl.args"
for c in /usr/bin/curl /bin/curl; do
	if [ -x "\$c" ]; then exec "\$c" "\$@"; fi
done
exit 127
CURL_STUB
	chmod 0755 "$T/argv/curl"
	upd_update --stubs "$T/argv"
	expect_rc 0
	for pair in 'SHA256SUMS 65536' 'SHA256SUMS.sig 65536' 'SIDECAR-RELEASE 65536' \
		'claude-sidecar-0.3.1-source.tar.gz 67108864' "linux-x64.tar.gz 67108864" \
		"node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz 1073741824"; do
		name=${pair% *}
		limit=${pair#* }
		line=$(grep -E "/$name\$" "$T/curl.args" | head -n 1)
		case $line in
		*" --max-filesize $limit "*) ;;
		*) fail "$name was fetched without the limit $limit: $line" ;;
		esac
	done
}

# ---------------------------------------------------------------------------------------------
# SIDECAR-RELEASE

# upd_bad_manifest ARG WHAT TEXT: a release with mk_sidecar_release's ARG is refused saying TEXT.
upd_bad_manifest() {
	mk_sidecar_release 0.3.1 "$1"
	upd_update
	upd_refused "$2" "$3"
}

TESTS="$TESTS t_upd_manifest_bad_values"
t_upd_manifest_bad_values() {
	upd_state
	upd_bad_manifest 'FORMAT=one' "a FORMAT that is no number" 'SIDECAR-RELEASE names no valid FORMAT'
	upd_bad_manifest 'SIDECAR_VERSION=../x' "a version with a path in it" 'names no valid SIDECAR_VERSION'
	upd_bad_manifest 'SIDECAR_VERSION=0.03.1' "a second spelling of a version" 'names no valid SIDECAR_VERSION'
	upd_bad_manifest 'SIDECAR_VERSION=0.3.1-rc.01' "a second spelling of a candidate" 'names no valid SIDECAR_VERSION'
	upd_bad_manifest 'SIDECAR_VERSION=99999999999999999999999.0.0' "a number no reader can hold" 'names no valid SIDECAR_VERSION'
	upd_bad_manifest 'SIDECAR_COMMIT=abc' "a short commit" 'names no valid SIDECAR_COMMIT'
	upd_bad_manifest 'SIDECAR_SOURCE=../evil.tar.gz' "a source name with a path in it" 'names no valid SIDECAR_SOURCE'
	upd_bad_manifest 'SIDECAR_SOURCE=claude-sidecar-9.9.9-source.tar.gz' "another version's source" 'names no valid SIDECAR_SOURCE'
	upd_bad_manifest 'PROTOCOL_MAJOR=three' "a major that is no number" 'names no valid PROTOCOL_MAJOR'
	upd_bad_manifest 'PROTOCOL_MINOR=' "an empty minor" 'names no valid PROTOCOL_MINOR'
	upd_bad_manifest 'CAPABILITIES=a;b' "a capability with punctuation" 'names no valid CAPABILITIES'
	upd_bad_manifest 'SDK_VERSION=1 2' "an SDK version with a space" 'names no valid SDK_VERSION'
	upd_bad_manifest 'NODE_VERSION=22.23.2' "a Node version with no v" 'names no valid NODE_VERSION'
	upd_bad_manifest 'NODE_SHA256_linux_x64=zz' "a bad Node hash" 'names no valid NODE_SHA256_linux_x64'
	upd_bad_manifest 'NODE_SHA256_linux_arm64=zz' "a bad Node hash" 'names no valid NODE_SHA256_linux_arm64'
	upd_bad_manifest 'GRPC_TOOLS_VERSION=v1.13.1' "a grpc-tools version with a v" 'names no valid GRPC_TOOLS_VERSION'
	upd_bad_manifest 'GRPC_TOOLS_SHA256_linux_x64=zz' "a bad protoc hash" 'names no valid GRPC_TOOLS_SHA256_linux_x64'
	upd_bad_manifest 'GRPC_TOOLS_SHA256_linux_arm64=zz' "a bad protoc hash" 'names no valid GRPC_TOOLS_SHA256_linux_arm64'
	upd_bad_manifest 'BUILD_STAMP=0123' "a short stamp" 'names no valid BUILD_STAMP'
	upd_bad_manifest 'CLI_RANGE=2.1.252' "half a range" 'names no valid CLI_RANGE'
	# shellcheck disable=SC2016 # a literal value
	upd_bad_manifest 'TESTED_CLI=$(x)' "a tested list that is no list" 'names no valid TESTED_CLI'
}

TESTS="$TESTS t_upd_manifest_missing_keys"
t_upd_manifest_missing_keys() {
	upd_state
	for k in SIDECAR_VERSION SIDECAR_COMMIT SIDECAR_SOURCE PROTOCOL_MAJOR PROTOCOL_MINOR CAPABILITIES \
		SDK_VERSION NODE_VERSION NODE_SHA256_linux_x64 NODE_SHA256_linux_arm64 GRPC_TOOLS_VERSION \
		GRPC_TOOLS_SHA256_linux_x64 GRPC_TOOLS_SHA256_linux_arm64 BUILD_STAMP CLI_RANGE TESTED_CLI; do
		upd_bad_manifest "-$k" "no $k" "names no valid $k"
	done
}

TESTS="$TESTS t_upd_manifest_wrong_format"
t_upd_manifest_wrong_format() {
	upd_state
	upd_bad_manifest 'FORMAT=2' "format 2" 'this release needs a newer Eitri'
	upd_bad_manifest '-FORMAT' "no format" 'SIDECAR-RELEASE names no valid FORMAT'
}

TESTS="$TESTS t_upd_manifest_unknown_key_ignored"
t_upd_manifest_unknown_key_ignored() {
	upd_setup
	# shellcheck disable=SC2016 # a literal value
	mk_sidecar_release 0.3.1 'A_LATER_KEY=anything at all; even $(this)'
	upd_update
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected"
}

TESTS="$TESTS t_upd_manifest_not_in_sums"
t_upd_manifest_not_in_sums() {
	upd_state
	mk_sidecar_release 0.3.1
	printf 'SDK_VERSION=6.6.6\n' >>"$S/srv/sc/releases/latest/download/SIDECAR-RELEASE"
	upd_update
	upd_refused "a manifest that is not the signed one" 'checksum mismatch for SIDECAR-RELEASE'
	expect_no_out '6.6.6'
}

# ---------------------------------------------------------------------------------------------
# What the manifest pins

TESTS="$TESTS t_upd_source_hash_mismatch"
t_upd_source_hash_mismatch() {
	upd_state
	mk_sidecar_release 0.3.1
	printf 'altered\n' >>"$(upd_served 0.3.1)/claude-sidecar-0.3.1-source.tar.gz"
	upd_update
	upd_refused "an altered source" 'checksum mismatch for claude-sidecar-0.3.1-source.tar.gz'
	expect_absent "$T/npm.log"
}

TESTS="$TESTS t_upd_node_hash_mismatch"
t_upd_node_hash_mismatch() {
	upd_state
	mk_sidecar_release 0.3.1 "NODE_SHA256_linux_x64=$UPD_ZEROS"
	upd_update
	upd_refused "a Node tarball that is not the pinned one" "checksum mismatch for node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
}

TESTS="$TESTS t_upd_protoc_hash_mismatch"
t_upd_protoc_hash_mismatch() {
	upd_state
	mk_sidecar_release 0.3.1 "GRPC_TOOLS_SHA256_linux_x64=$UPD_ZEROS"
	upd_update
	upd_refused "a protoc tarball that is not the pinned one" 'checksum mismatch for linux-x64.tar.gz'
	expect_absent "$T/npm.log"
}

# upd_protoc_shape NAME BUILD_DIR: a protoc tarball from BUILD_DIR, pinned by the manifest of release 0.3.1.
upd_protoc_shape() {
	mkdir -p "$S/srv/$1/grpc-tools/v$GRPC_FIXTURE_VERSION"
	mk_protoc_tarball "$2" "$S/srv/$1/grpc-tools/v$GRPC_FIXTURE_VERSION/linux-x64.tar.gz"
	_ups=$(sha256sum <"$S/srv/$1/grpc-tools/v$GRPC_FIXTURE_VERSION/linux-x64.tar.gz" | cut -d' ' -f1)
	mk_sidecar_release 0.3.1 "GRPC_TOOLS_SHA256_linux_x64=$_ups"
}

TESTS="$TESTS t_upd_protoc_extra_name"
t_upd_protoc_extra_name() {
	upd_state
	mkdir -p "$T/pb/bin"
	printf 'x\n' >"$T/pb/bin/protoc"
	printf 'x\n' >"$T/pb/bin/grpc_node_plugin"
	printf 'x\n' >"$T/pb/bin/another"
	chmod 0755 "$T/pb/bin/"*
	upd_protoc_shape grpc-extra "$T/pb"
	upd_update --set "EITRI_INSTALL_TEST_GRPC_BASE_URL=http://127.0.0.1:$PORT/grpc-extra"
	upd_refused "a protoc tarball with another name in it" 'holds other names than bin/protoc'
}

TESTS="$TESTS t_upd_protoc_is_a_link"
t_upd_protoc_is_a_link() {
	upd_state
	mkdir -p "$T/pb/bin"
	printf 'x\n' >"$T/pb/bin/grpc_node_plugin"
	chmod 0755 "$T/pb/bin/grpc_node_plugin"
	ln -s /bin/sh "$T/pb/bin/protoc"
	upd_protoc_shape grpc-link "$T/pb"
	upd_update --set "EITRI_INSTALL_TEST_GRPC_BASE_URL=http://127.0.0.1:$PORT/grpc-link"
	upd_refused "a protoc that is a link" 'not a plain executable'
}

TESTS="$TESTS t_upd_grpc_tools_other_version"
t_upd_grpc_tools_other_version() {
	upd_state
	mk_sidecar_release 0.3.1
	upd_update --set EITRI_INSTALL_TEST_GRPC_TOOLS_VERSION=1.12.0
	upd_refused "another grpc-tools installed" "grpc-tools 1.12.0, not the $GRPC_FIXTURE_VERSION"
}

TESTS="$TESTS t_upd_grpc_url_is_loopback_only"
t_upd_grpc_url_is_loopback_only() {
	upd_state
	mk_sidecar_release 0.3.1
	upd_update --set EITRI_INSTALL_TEST_GRPC_BASE_URL=https://example.invalid/grpc
	upd_refused "a test override off this machine" 'test mode downloads only from'
}

# Test mode with no override reaches a refused port, never grpc's own host.
TESTS="$TESTS t_upd_grpc_unset_goes_nowhere"
t_upd_grpc_unset_goes_nowhere() {
	upd_state
	mk_sidecar_release 0.3.1
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		--unset EITRI_INSTALL_TEST_GRPC_BASE_URL \
		-- --sidecar-update --base-url "http://127.0.0.1:$PORT/sc" --release-signers "$SIGNERS"
	upd_refused "no grpc override in test mode" 'http://127.0.0.1:1/grpc-tools/'
}

# ---------------------------------------------------------------------------------------------
# npm

TESTS="$TESTS t_upd_npm_ci_ignores_scripts"
t_upd_npm_ci_ignores_scripts() {
	upd_setup
	mk_sidecar_release 0.3.1
	upd_update
	expect_rc 0
	expect_eq "$(sed -n 1p "$T/npm.log")" 'ci --no-audit --no-fund --ignore-scripts' "the npm ci line"
	expect_eq "$(sed -n 2p "$T/npm.log")" 'build protoc=yes' "protoc before the build"
}

TESTS="$TESTS t_npm_retry_settings"
t_npm_retry_settings() {
	# npm ci is asked for five tries starting at 2 s, unless the user set npm's own values.
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	sc_inst_setup --set "EITRI_INSTALL_TEST_NPM_ENV_LOG=$T/npm-env.log" -- --sidecar-only
	expect_rc 0
	expect_eq "$(cat "$T/npm-env.log")" 'fetch_retries=5 fetch_retry_mintimeout=2000' "npm's retry settings"
	rm -r "$(data_of)/eitri/sidecar"
	sc_inst_setup --set "EITRI_INSTALL_TEST_NPM_ENV_LOG=$T/npm-env2.log" --set npm_config_fetch_retries=9 \
		--set npm_config_fetch_retry_mintimeout=15000 -- --sidecar-only
	expect_rc 0
	expect_eq "$(cat "$T/npm-env2.log")" 'fetch_retries=9 fetch_retry_mintimeout=15000' "the user's own npm retry settings"
}

TESTS="$TESTS t_upd_pinned_setup_ignores_scripts"
t_upd_pinned_setup_ignores_scripts() {
	serve 1.0.0
	sc_setup_release 1.0.0 "$REV_A"
	srv_mark
	sc_inst_setup --set "EITRI_INSTALL_TEST_NPM_LOG=$T/npm.log" -- --sidecar-only
	expect_rc 0
	expect_eq "$(sed -n 1p "$T/npm.log")" 'ci --no-audit --no-fund --ignore-scripts' "the npm ci line of a pinned setup"
	expect_eq "$(sed -n 2p "$T/npm.log")" 'build protoc=yes' "protoc before the setup build"
	if ! srv_paths | grep -Fx "/grpc/grpc-tools/v$GRPC_FIXTURE_VERSION/linux-x64.tar.gz" >/dev/null; then
		fail "a pinned setup did not fetch its compiler: $(srv_paths)"
	fi
}

TESTS="$TESTS t_upd_npm_failing"
t_upd_npm_failing() {
	upd_state
	mk_sidecar_release 0.3.1
	upd_update --set EITRI_INSTALL_TEST_NPM_MODE=fail-ci
	upd_refused "npm ci failing" 'simulated npm ci failure'
	upd_update --set EITRI_INSTALL_TEST_NPM_MODE=fail-build
	upd_refused "the build failing" 'simulated build failure'
}

# ---------------------------------------------------------------------------------------------
# Decided before anything is spent

# upd_decided WHAT TEXT: refused saying TEXT, having fetched nothing past the manifest.
upd_decided() {
	upd_refused "$1" "$2"
	if srv_paths | grep -v -E '/(SHA256SUMS|SHA256SUMS\.sig|SIDECAR-RELEASE)$' | grep . >/dev/null; then
		fail "$1 fetched more than the manifest: $(srv_paths)"
	fi
	srv_mark
}

TESTS="$TESTS t_upd_higher_major"
t_upd_higher_major() {
	upd_state
	mk_sidecar_release 0.3.1 PROTOCOL_MAJOR=4
	srv_mark
	upd_update
	upd_decided "a higher major" 'this release needs a newer Eitri'
}

TESTS="$TESTS t_upd_lower_major"
t_upd_lower_major() {
	upd_state
	mk_sidecar_release 0.3.1 PROTOCOL_MAJOR=2
	srv_mark
	upd_update
	upd_decided "a lower major" 'speaks protocol major 2'
}

TESTS="$TESTS t_upd_missing_required_capability"
t_upd_missing_required_capability() {
	upd_state
	mk_sidecar_release 0.3.1 'CAPABILITIES=provider_permission_prompts cli_auto_mode'
	srv_mark
	upd_update
	upd_decided "a required capability absent" 'permission_defer'
}

TESTS="$TESTS t_upd_tag_and_version_disagree"
t_upd_tag_and_version_disagree() {
	upd_state
	mk_sidecar_release 0.3.1
	cp -R "$(upd_served 0.3.1)" "$(upd_served 0.3.9)"
	srv_mark
	UPD_V=0.3.9
	upd_version 0.3.9
	upd_decided "a release served under another tag" 'asked for 0.3.9'
}

TESTS="$TESTS t_upd_not_newer_than_bundled"
t_upd_not_newer_than_bundled() {
	upd_state
	mk_sidecar_release "$UPD_BUNDLED"
	srv_mark
	upd_update
	upd_decided "the bundled version" 'not newer than the bundled sidecar'
	mk_sidecar_release 0.1.9
	srv_mark
	upd_version 0.1.9
	upd_decided "an older one, named" 'not newer than the bundled sidecar'
}

TESTS="$TESTS t_upd_not_newer_than_selected"
t_upd_not_newer_than_selected() {
	upd_state
	mk_sidecar_release 0.2.9
	srv_mark
	upd_update
	upd_decided "older than selected" 'not newer than 0.3.0, which is selected here'
}

# What `selected` names is the floor whether or not its directory is there: a release must not lower it
# by the accident of a copy being gone.
TESTS="$TESTS t_upd_not_newer_than_selected_with_no_directory"
t_upd_not_newer_than_selected_with_no_directory() {
	upd_setup
	plant_selected 0.3.1
	UPD_SNAP=$(snap "$UPD_ROOT")
	mk_sidecar_release 0.3.0
	srv_mark
	upd_update
	upd_decided "older than a selected version with no directory" 'not newer than 0.3.1, which is selected here'
	upd_version 0.3.0
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.0' "selected"
}

# The same version, selected and damaged: not a kept copy, so the message must not call it one, and
# naming the version replaces it.
TESTS="$TESTS t_upd_selected_damaged_copy_is_not_called_kept"
t_upd_selected_damaged_copy_is_not_called_kept() {
	upd_setup
	plant_update 0.3.1
	rm -f "$UPD_ROOT/updates/0.3.1/BUILD"
	plant_selected 0.3.1
	UPD_SNAP=$(snap "$UPD_ROOT")
	mk_sidecar_release 0.3.1
	srv_mark
	upd_update
	upd_decided "a selected, damaged copy of this version" 'not newer than 0.3.1, which is selected here'
	expect_no_out 'kept'
	upd_version 0.3.1
	expect_rc 0
	expect_file "$UPD_ROOT/updates/0.3.1/BUILD"
}

# A selection that a tab calls unusable holds nothing back: all of the file is read, not its first
# bytes. These are `version=99.0.0` with what follows it, and a tab refuses both.
TESTS="$TESTS t_upd_unusable_selected_is_no_floor"
t_upd_unusable_selected_is_no_floor() {
	upd_setup
	{
		printf 'version=99.0.0'
		printf '%300s' '' | tr ' ' '\n'
		echo junk
	} >"$UPD_ROOT/selected"
	mk_sidecar_release 0.3.1
	upd_update
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected, over a long file"
	printf 'version=99.0.0\n\n' >"$UPD_ROOT/selected"
	mk_sidecar_release 0.3.2
	upd_update
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.2' "selected, over a blank second line"
}

TESTS="$TESTS t_upd_not_newer_than_kept"
t_upd_not_newer_than_kept() {
	upd_state
	plant_update 0.3.5
	UPD_SNAP=$(snap "$UPD_ROOT")
	mk_sidecar_release 0.3.1
	srv_mark
	upd_update
	upd_decided "older than a kept update" 'not newer than 0.3.5, which is kept here'
}

# A named version may go below what is selected: the person chose it.
TESTS="$TESTS t_upd_named_version_may_go_down"
t_upd_named_version_may_go_down() {
	upd_state
	mk_sidecar_release 0.2.9
	upd_version 0.2.9
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.2.9' "selected"
}

TESTS="$TESTS t_upd_rc"
t_upd_rc() {
	upd_state
	mk_sidecar_release 0.4.0-rc.1
	srv_mark
	upd_update
	upd_decided "an rc at latest" 'candidate'
	upd_version 0.4.0-rc.1
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.4.0-rc.1' "selected"
	expect_dir "$UPD_ROOT/updates/0.4.0-rc.1"
}

TESTS="$TESTS t_upd_already_kept"
t_upd_already_kept() {
	upd_state
	plant_update 0.3.1
	UPD_SNAP=$(snap "$UPD_ROOT")
	mk_sidecar_release 0.3.1
	srv_mark
	upd_update
	upd_decided "a version already kept" 'already built: eitri sidecar use 0.3.1'
	upd_version 0.3.1
	upd_decided "the same, named" 'already built: eitri sidecar use 0.3.1'
}

# The version new tabs already run is the answer to `update`: nothing to do, and nothing went wrong.
TESTS="$TESTS t_upd_the_selected_version_is_up_to_date"
t_upd_the_selected_version_is_up_to_date() {
	upd_state
	mk_sidecar_release 0.3.0
	for how in latest named; do
		if [ "$how" = latest ]; then upd_update; else upd_version 0.3.0; fi
		expect_rc 0
		expect_out '0.3.0 is already what new tabs use'
		expect_no_out 'already built'
		expect_no_out 'error'
		expect_eq "$(snap "$UPD_ROOT")" "$UPD_SNAP" "the sidecar directory, $how"
		expect_absent "$TH/.cache/eitri/sidecar-update"
	done
}

# Nothing published at the address is no network failure: re-running cannot help, and the words say so.
TESTS="$TESTS t_upd_no_release_is_said_plainly"
t_upd_no_release_is_said_plainly() {
	upd_state
	rm -rf "$S/srv/sc/releases/latest"
	upd_update
	upd_refused "no release at all" 'no sidecar release is published at'
	expect_no_out 'could not download'
	upd_check
	upd_refused "no release at all, asked by check" 'no sidecar release is published at'
	upd_version 0.3.77
	upd_refused "a version nobody published" 'no release claude-sidecar-v0.3.77 at'
	expect_no_out 'could not download'
}

# The build peaks near 1.2 GiB, so an update asks for that before it starts.
TESTS="$TESTS t_upd_asks_for_the_space_a_build_takes"
t_upd_asks_for_the_space_a_build_takes() {
	upd_state
	mk_sidecar_release 0.3.1
	mkdir -p "$T/lowdisk"
	cat >"$T/lowdisk/df" <<'DF_STUB'
#!/bin/sh
echo 'Filesystem 1024-blocks Used Available Capacity Mounted on'
echo '/dev/stub 2000000 1300000 700000 65% /'
DF_STUB
	chmod 0755 "$T/lowdisk/df"
	upd_update --stubs "$T/lowdisk"
	upd_refused "700 MB free" 'building a sidecar update needs about 1.2 GiB'
}

# A link at updates/<version> is no copy of it, so `update` does not send a person to `use`, which
# refuses a link. The new directory takes the link's place, and what it led to stays.
TESTS="$TESTS t_upd_link_at_the_version_is_replaced"
t_upd_link_at_the_version_is_replaced() {
	for how in latest named; do
		upd_setup
		plant_update 0.3.1
		rm -rf "$T/elsewhere"
		mv "$UPD_ROOT/updates/0.3.1" "$T/elsewhere"
		ln -s "$T/elsewhere" "$UPD_ROOT/updates/0.3.1"
		mk_sidecar_release 0.3.1
		if [ "$how" = latest ]; then upd_update; else upd_version 0.3.1; fi
		expect_rc 0
		expect_no_out 'already built'
		d=$UPD_ROOT/updates/0.3.1
		if [ -L "$d" ]; then fail "$how: the link is still there"; fi
		expect_exec "$d/verdandi-claude-sidecar"
		expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected, $how"
		expect_file "$T/elsewhere/BUILD"
		rm -rf "${UPD_ROOT:?}/updates" "$UPD_ROOT/selected"
	done
}

# mv onto a link to a directory nests the file there and exits 0, having selected nothing. It is
# refused before anything is spent, like a directory.
TESTS="$TESTS t_upd_selected_is_a_link_to_a_directory"
t_upd_selected_is_a_link_to_a_directory() {
	upd_setup
	mkdir -p "$T/elsewhere"
	ln -s "$T/elsewhere" "$UPD_ROOT/selected"
	UPD_SNAP=$(snap "$UPD_ROOT")
	mk_sidecar_release 0.3.1
	srv_mark
	upd_update
	upd_refused "selected is a link to a directory" 'is a directory, or a link to one'
	expect_eq "$(srv_paths)" "" "what was fetched"
	expect_eq "$(find "$T/elsewhere" -mindepth 1 | wc -l)" 0 "files written through the link"
}

TESTS="$TESTS t_upd_selected_is_a_directory"
t_upd_selected_is_a_directory() {
	upd_setup
	mkdir -p "$UPD_ROOT/selected"
	UPD_SNAP=$(snap "$UPD_ROOT")
	mk_sidecar_release 0.3.1
	srv_mark
	upd_update
	upd_refused "selected is a directory" 'is a directory, or a link to one'
	expect_eq "$(srv_paths)" "" "what was fetched"
}

# A copy that cannot run is not a kept one, so a release replaces it.
TESTS="$TESTS t_upd_damaged_kept_is_replaced_with_no_nesting"
t_upd_damaged_kept_is_replaced_with_no_nesting() {
	upd_setup
	plant_update 0.3.1
	rm -f "$UPD_ROOT/updates/0.3.1/BUILD"
	printf 'left by a damaged copy\n' >"$UPD_ROOT/updates/0.3.1/stray"
	mk_sidecar_release 0.3.1
	upd_update
	expect_rc 0
	d=$UPD_ROOT/updates/0.3.1
	expect_file "$d/BUILD"
	expect_absent "$d/stray"
	expect_absent "$d/0.3.1"
	expect_eq "$(find "$d" -name '.*' | wc -l)" 0 "a temporary directory nested inside the new copy"
	expect_eq "$(find "$UPD_ROOT/updates" -mindepth 1 -maxdepth 1 | wc -l)" 1 "entries in updates/"
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected"
}

# ---------------------------------------------------------------------------------------------
# The artifact against the signed manifest, and the probe

TESTS="$TESTS t_upd_artifact_differs"
t_upd_artifact_differs() {
	upd_state
	mk_sidecar_release 0.3.1
	upd_update --set EITRI_INSTALL_TEST_BUILD_STAMP=fedcba9876543210
	upd_refused "another build stamp" 'build_stamp=fedcba9876543210'
	upd_update --set EITRI_INSTALL_TEST_SDK_VERSION=0.3.999
	upd_refused "another SDK" 'sdk_version=0.3.999'
	upd_update --set 'EITRI_INSTALL_TEST_EXECUTABLES=["host_cli","sdk_bundled"]'
	upd_refused "another executables list" 'executables=["host_cli","sdk_bundled"]'
	upd_update --set EITRI_INSTALL_TEST_PROTOCOL_MINOR=23
	upd_refused "another minor" 'protocol=3.23'
	upd_update --set EITRI_INSTALL_TEST_SIDECAR_VERSION=0.3.7
	upd_refused "another version" 'sidecar_version=0.3.7'
	upd_update --set EITRI_INSTALL_TEST_NPM_MODE=sdk-bundled
	upd_refused "an artifact that bundles the CLI" 'not host_cli only'
}

# The reader runs under the real Node where the host has one: the stub Node of these tests stands in
# for it everywhere else, and is held to the same output here.
TESTS="$TESTS t_upd_facts_reader_under_real_node"
t_upd_facts_reader_under_real_node() {
	if ! command -v node >/dev/null 2>&1; then return 0; fi
	sed -n "/^NV_SIDECAR_FACTS_JS='\$/,/^'\$/p" "$REAL_INSTALLER" | sed -e '1d' -e '$d' >"$T/reader.js"
	if [ ! -s "$T/reader.js" ]; then fail "no reader found in the installer"; fi
	cat >"$T/facts.json" <<'FACTS'
{"v":1,"sidecar_version":"0.3.1","protocol":{"major":3,"minor":24},"node":"v22.23.2","build_stamp":"0123456789abcdef","sdk_version":"0.3.252","executables":["host_cli"]}
FACTS
	real=$(node -e "$(cat "$T/reader.js")" <"$T/facts.json")
	stub=$("$S/fix/node-build/node-$NODE_FIXTURE_VERSION-linux-x64/bin/node" -e unused <"$T/facts.json")
	expect_eq "$real" "$stub" "the reader under real Node, against the stub"
	expect_eq "$(printf '%s\n' "$real" | sed -n 6p)" 'executables=["host_cli"]' "the executables line"
}

TESTS="$TESTS t_upd_probe_refusal"
t_upd_probe_refusal() {
	upd_state
	mk_sidecar_release 0.3.1
	upd_update --set "EITRI_INSTALL_TEST_PROBE_REFUSAL=this claude is too old"
	upd_refused "a probe refusal" 'built, but it will not start here: this claude is too old'
	expect_no_out 'an earlier stderr line'
}

# Only the sentence the probe prints on acceptance counts as acceptance.
TESTS="$TESTS t_upd_probe_says_nothing"
t_upd_probe_says_nothing() {
	upd_state
	mk_sidecar_release 0.3.1
	upd_update --set EITRI_INSTALL_TEST_PROBE_MUTE=1
	upd_refused "a probe that exits 0 and says nothing" 'built, but it will not start here'
}

TESTS="$TESTS t_upd_no_shell_beside_setup"
t_upd_no_shell_beside_setup() {
	upd_state
	rm -f "$UPD_LIB/shell"
	mk_sidecar_release 0.3.1
	upd_update
	upd_refused "no shell to try it with" 'built, but it will not start here'
}

# ---------------------------------------------------------------------------------------------
# The bundled sidecar

TESTS="$TESTS t_upd_aur_sibling_counts_as_bundled"
t_upd_aur_sibling_counts_as_bundled() {
	upd_setup
	rm -rf "${UPD_ROOT:?}/aaaaaaa"
	printf '#!/bin/sh\necho "verdandi-claude-sidecar 0.2.0"\n' >"$UPD_LIB/verdandi-claude-sidecar"
	chmod 0755 "$UPD_LIB/verdandi-claude-sidecar"
	mk_sidecar_release 0.3.1
	upd_update
	expect_rc 0
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.1' "selected"
	expect_absent "$UPD_ROOT/aaaaaaa"
}

TESTS="$TESTS t_upd_no_bundled_refuses"
t_upd_no_bundled_refuses() {
	upd_setup
	rm -rf "${UPD_ROOT:?}/aaaaaaa"
	mk_sidecar_release 0.3.1
	srv_mark
	upd_update
	expect_fail "no bundled sidecar"
	expect_out 'run "eitri setup" first'
	expect_absent "$UPD_ROOT/selected"
	expect_absent "$UPD_ROOT/updates"
	expect_eq "$(srv_paths)" '' "requests made with no fallback in place"
	upd_check
	expect_fail "check with no bundled sidecar"
	expect_out 'run "eitri setup" first'
}

# A sibling that does not run is no fallback.
TESTS="$TESTS t_upd_sibling_that_does_not_run"
t_upd_sibling_that_does_not_run() {
	upd_setup
	rm -rf "${UPD_ROOT:?}/aaaaaaa"
	printf '#!/bin/sh\nexit 1\n' >"$UPD_LIB/verdandi-claude-sidecar"
	chmod 0755 "$UPD_LIB/verdandi-claude-sidecar"
	mk_sidecar_release 0.3.1
	upd_update
	expect_fail "a sibling that does not run"
	expect_out 'run "eitri setup" first'
}

# ---------------------------------------------------------------------------------------------
# Interrupted, and side by side

# mk_mv_stub DIR BEFORE AFTER: an mv that does BEFORE (a shell command), runs the real one, then does
# AFTER, when the move is a new directory landing at updates/<version>.
mk_mv_stub() {
	mkdir -p "$1"
	cat >"$1/mv" <<MV_STUB
#!/bin/sh
real=
for m in /usr/bin/mv /bin/mv; do
	if [ -x "\$m" ]; then real=\$m; break; fi
done
for last do :; done
for first do
	case \$first in --) ;; *) break ;; esac
done
case \$first in *.aside) exec "\$real" "\$@" ;; esac
case \$first:\$last in
*/updates/.*.tmp.*:*/updates/[0-9]*)
	$2
	;;
esac
"\$real" "\$@" || exit
case \$first:\$last in
*/updates/.*.tmp.*:*/updates/[0-9]*)
	$3
	;;
esac
exit 0
MV_STUB
	chmod 0755 "$1/mv"
}

TESTS="$TESTS t_upd_killed_between_mv_and_selected"
t_upd_killed_between_mv_and_selected() {
	upd_state
	mk_sidecar_release 0.3.1
	# shellcheck disable=SC2016 # expanded by the stub
	mk_mv_stub "$T/mvkill" ':' 'kill -KILL "$PPID"'
	upd_update --stubs "$T/mvkill"
	expect_fail "killed after the directory landed"
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.0' "selected after the kill"
	expect_file "$UPD_ROOT/updates/0.3.1/BUILD"
	expect_exec "$UPD_ROOT/updates/0.3.1/verdandi-claude-sidecar"
	# The next run takes the dead run's lock, finds the build complete and says how to choose it.
	upd_update
	expect_fail "a build already there"
	expect_out 'taking over a lock left by an installer that is no longer running'
	expect_out 'already built: eitri sidecar use 0.3.1'
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.0' "selected after the next run"
	expect_absent "$TH/.cache/eitri/lock"
}

TESTS="$TESTS t_upd_terminated_between_mv_and_selected"
t_upd_terminated_between_mv_and_selected() {
	upd_state
	mk_sidecar_release 0.3.1
	# shellcheck disable=SC2016 # expanded by the stub
	mk_mv_stub "$T/mvterm" ':' 'kill -TERM "$PPID"'
	upd_update --stubs "$T/mvterm"
	expect_fail "terminated after the directory landed"
	expect_eq "$(cat "$UPD_ROOT/selected")" 'version=0.3.0' "selected after TERM"
	expect_file "$UPD_ROOT/updates/0.3.1/BUILD"
	expect_eq "$(find "$UPD_ROOT" -name '.*' | wc -l)" 0 "temporary names left under the sidecar directory"
	expect_absent "$TH/.cache/eitri"
}

# The move of the new directory fails after a damaged copy was set aside: the copy comes back.
TESTS="$TESTS t_upd_failed_move_restores_what_was_set_aside"
t_upd_failed_move_restores_what_was_set_aside() {
	upd_setup
	plant_update 0.3.1
	rm -f "$UPD_ROOT/updates/0.3.1/BUILD"
	mk_mv_stub "$T/mvfail" 'exit 1' ':'
	before=$(upd_snap_loose)
	mk_sidecar_release 0.3.1
	upd_update --stubs "$T/mvfail"
	expect_fail "the final move failing"
	expect_eq "$(upd_snap_loose)" "$before" "the sidecar directory after a failed move"
	expect_absent "$TH/.cache/eitri"
}

TESTS="$TESTS t_upd_lock_held"
t_upd_lock_held() {
	upd_state
	mk_sidecar_release 0.3.1
	mkdir -p "$TH/.cache/eitri/lock"
	sleep 300 &
	holder=$!
	echo "$holder" >"$TH/.cache/eitri/lock/pid"
	srv_mark
	upd_update
	kill "$holder" 2>/dev/null
	wait "$holder" 2>/dev/null
	expect_fail "the lock held"
	expect_out "another Eitri installer (pid $holder) is running"
	expect_eq "$(snap "$UPD_ROOT")" "$UPD_SNAP" "the sidecar directory with the lock held"
	expect_eq "$(srv_paths)" '' "requests made without the lock"
	expect_file "$TH/.cache/eitri/lock/pid"
}

# `$NV_LIB.new` and `.old` belong to the install path. A run here, on a .deb, is not that install's.
TESTS="$TESTS t_upd_nv_lib_new_untouched"
t_upd_nv_lib_new_untouched() {
	upd_setup
	mkdir -p "$TH/.local/lib/eitri.new" "$TH/.local/lib/eitri.old"
	printf 'a half-made install\n' >"$TH/.local/lib/eitri.new/f"
	printf 'the install before it\n' >"$TH/.local/lib/eitri.old/f"
	before=$(snap "$TH/.local/lib")
	mk_sidecar_release 0.3.1
	upd_update --set EITRI_INSTALL_TEST_NPM_MODE=fail-ci
	expect_fail "a failed update"
	expect_eq "$(snap "$TH/.local/lib")" "$before" ".local/lib after a failed update"
	upd_update
	expect_rc 0
	expect_eq "$(snap "$TH/.local/lib")" "$before" ".local/lib after an update"
	rm -rf "${TH:?}/.local/lib/eitri.new" "${TH:?}/.local/lib/eitri.old"
	upd_update --set EITRI_INSTALL_TEST_NPM_MODE=fail-ci
	expect_dir "$TH/.local/lib"
}

# ---------------------------------------------------------------------------------------------
# check

TESTS="$TESTS t_upd_check_says_what_is_there"
t_upd_check_says_what_is_there() {
	upd_state
	mk_sidecar_release 0.3.1 SDK_VERSION=0.3.320 PROTOCOL_MINOR=25
	before=$(snap "$TH")
	srv_mark
	upd_check
	expect_rc 0
	expect_out 'newest release: 0.3.1 (protocol 3.25, Agent SDK 0.3.320). You run 0.3.0.'
	expect_out 'eitri sidecar update takes it: 1-3 minutes, about 1.2 GiB free, and network access to nodejs.org, npm and grpc.io.'
	expect_eq "$(snap "$TH")" "$before" "HOME after a check"
	if srv_paths | grep -v -E '/(SHA256SUMS|SHA256SUMS\.sig|SIDECAR-RELEASE)$' | grep . >/dev/null; then
		fail "check fetched more than the manifest: $(srv_paths)"
	fi
}

TESTS="$TESTS t_upd_check_with_only_the_bundled_sidecar"
t_upd_check_with_only_the_bundled_sidecar() {
	upd_setup
	mk_sidecar_release 0.3.1
	upd_check
	expect_rc 0
	expect_out 'You run 0.2.0.'
}

TESTS="$TESTS t_upd_check_prints_only_signed_fields"
t_upd_check_prints_only_signed_fields() {
	upd_state
	mk_sidecar_release 0.3.1 'UNKNOWN_KEY=sentinel-from-a-signed-unknown-key'
	upd_check
	expect_rc 0
	expect_no_out 'sentinel-from-a-signed-unknown-key'
	# A manifest that is not the signed one prints nothing of what it says.
	printf 'SDK_VERSION=sentinel-from-the-server\n' >"$S/srv/sc/releases/latest/download/SIDECAR-RELEASE"
	upd_check
	expect_fail "a manifest outside the signature"
	expect_out 'checksum mismatch for SIDECAR-RELEASE'
	expect_no_out 'sentinel-from-the-server'
	# A bad signature stops before the manifest is read at all.
	SC_KEY=other mk_sidecar_release 0.3.1 'SDK_VERSION=9.9.9'
	upd_check
	expect_fail "a bad signature"
	expect_no_out '9.9.9'
}

TESTS="$TESTS t_upd_check_reports_a_refusal"
t_upd_check_reports_a_refusal() {
	upd_state
	mk_sidecar_release 0.3.0
	upd_check
	expect_rc 0
	expect_out '0.3.0 is already what new tabs use'
	expect_no_out 'already built'
	mk_sidecar_release 0.3.1 PROTOCOL_MAJOR=4
	upd_check
	expect_rc 0
	expect_out 'this release needs a newer Eitri'
	expect_no_out 'eitri sidecar update takes it'
}

TESTS="$TESTS t_upd_check_while_the_lock_is_held"
t_upd_check_while_the_lock_is_held() {
	upd_state
	mk_sidecar_release 0.3.1
	mkdir -p "$TH/.cache/eitri/lock"
	sleep 300 &
	holder=$!
	echo "$holder" >"$TH/.cache/eitri/lock/pid"
	upd_check
	kill "$holder" 2>/dev/null
	wait "$holder" 2>/dev/null
	expect_rc 0
	expect_out 'newest release: 0.3.1'
	expect_eq "$(cat "$TH/.cache/eitri/lock/pid")" "$holder" "the lock's pid after a check"
	# Its own directory is gone, and the holder's cache is as it was.
	expect_eq "$(ls -A "$TH/.cache/eitri")" lock "the cache after a check"
	expect_eq "$(snap "$UPD_ROOT")" "$UPD_SNAP" "the sidecar directory after a check"
}

TESTS="$TESTS t_upd_check_failing_cleans_up"
t_upd_check_failing_cleans_up() {
	upd_state
	mk_sidecar_release 0.3.1
	rm -f "$S/srv/sc/releases/latest/download/SHA256SUMS.sig"
	upd_check
	upd_refused "a check with no signature to read" 'a release without a signature is refused'
}
