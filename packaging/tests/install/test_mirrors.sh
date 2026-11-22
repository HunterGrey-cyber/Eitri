# User download mirrors and the verification that remains independent of their addresses.
# Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # shared harness state

# Separate copies let a test alter a mirror without changing a shared fixture.
mirror_setup() {
	serve 1.0.0
	for _ms_pair in release:releases node:dist nvim:nvim-releases grpc:grpc skia:skia; do
		_ms_name=${_ms_pair%:*}
		_ms_src=${_ms_pair#*:}
		rm -rf "$S/srv/mirror-$_ms_name"
		mkdir -p "$S/srv/mirror-$_ms_name"
		cp -R "$S/srv/$_ms_src/." "$S/srv/mirror-$_ms_name/"
	done
	# The release base contains releases/, unlike the dependency distribution bases.
	mv "$S/srv/mirror-release" "$S/srv/mirror-release-tree"
	mkdir -p "$S/srv/mirror-release"
	mv "$S/srv/mirror-release-tree" "$S/srv/mirror-release/releases"
	MBASE=http://127.0.0.1:$PORT
	srv_mark
}

mirror_requested() {
	if ! srv_paths | grep -Fx -e "$1" >/dev/null; then fail "mirror request missing: $1; got: $(srv_paths)"; fi
}
mirror_not_requested() {
	if srv_paths | grep -F -e "$1" >/dev/null; then fail "unexpected mirror request: $1; got: $(srv_paths)"; fi
}
mirror_no_curl() {
	expect_absent "$TH/.cache"
	expect_absent "$TH/.local/lib/eitri"
	if [ -s "$S/logs/curl.log" ]; then fail "a refused mirror reached curl"; fi
}

TESTS="$TESTS t_mirror_release_used t_mirror_release_cli_wins t_mirror_tarball_source_fallback"
t_mirror_release_used() {
	mirror_setup
	inst --set "EITRI_RELEASE_BASE_URL=$MBASE/mirror-release///" -- --release-signers "$SIGNERS"
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "release mirror installed"
	mirror_requested /mirror-release/releases/latest/download/SHA256SUMS
	mirror_requested /mirror-release/releases/download/v1.0.0/eitri-1.0.0-x86_64-linux.tar.gz
	mirror_not_requested /mirror-release//
}
t_mirror_release_cli_wins() {
	mirror_setup
	inst --set "EITRI_RELEASE_BASE_URL=$MBASE/mirror-unused" -- --base-url "$MBASE/mirror-release" --release-signers "$SIGNERS"
	expect_rc 0
	mirror_requested /mirror-release/releases/latest/download/SHA256SUMS
	mirror_not_requested /mirror-unused/
}
t_mirror_tarball_source_fallback() {
	mirror_setup
	mkdir -p "$T/offline"
	cp "$S/fix/v1.0.0/eitri-1.0.0-x86_64-linux.tar.gz" "$S/fix/v1.0.0/SHA256SUMS" "$S/fix/v1.0.0/SHA256SUMS.sig" "$T/offline/"
	inst --set "EITRI_RELEASE_BASE_URL=$MBASE/mirror-release" --set "EITRI_NODE_BASE_URL=$MBASE/mirror-node" \
		-- --tarball "$T/offline/eitri-1.0.0-x86_64-linux.tar.gz" --sums "$T/offline/SHA256SUMS" \
		--sig "$T/offline/SHA256SUMS.sig" --release-signers "$SIGNERS"
	expect_rc 0
	mirror_requested /mirror-release/releases/download/v1.0.0/verdandi-aaaaaaa-source.tar.gz
	expect_exec "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
}

TESTS="$TESTS t_mirror_node_used t_mirror_grpc_used t_mirror_nvim_used t_mirror_sidecar_used t_mirror_skia_used"
t_mirror_node_used() {
	mirror_setup
	sc_inst_net --set "EITRI_NODE_BASE_URL=$MBASE/mirror-node///"
	expect_rc 0
	mirror_requested "/mirror-node/$NODE_FIXTURE_VERSION/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
	mirror_not_requested /dist/
	expect_exec "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
}
t_mirror_grpc_used() {
	mirror_setup
	sc_inst_net --set "EITRI_GRPC_BASE_URL=$MBASE/mirror-grpc///"
	expect_rc 0
	mirror_requested "/mirror-grpc/grpc-tools/v$GRPC_FIXTURE_VERSION/linux-x64.tar.gz"
	mirror_not_requested /grpc/
	expect_exec "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
}
t_mirror_nvim_used() {
	mirror_setup
	PRE_STUBS=$(old_nvim_stubs)
	inst --set "EITRI_NVIM_BASE_URL=$MBASE/mirror-nvim///" \
		--set "EITRI_INSTALL_TEST_NVIM_BASE_URL=$MBASE/unused-nvim" \
		-- --base-url "$MBASE" --release-signers "$SIGNERS" --with-nvim
	expect_rc 0
	mirror_requested "/mirror-nvim/v$NVIM_FIXTURE_VERSION/nvim-linux-x86_64.tar.gz"
	mirror_not_requested /unused-nvim/
	expect_exec "$(nvim_private_bin "$NVIM_FIXTURE_VERSION")"
}
mirror_sidecar_setup() {
	mirror_setup
	upd_setup
	mk_sidecar_release 0.3.1
	rm -rf "$S/srv/mirror-sidecar"
	cp -R "$S/srv/sc" "$S/srv/mirror-sidecar"
}
t_mirror_sidecar_used() {
	mirror_sidecar_setup
	inst --set "EITRI_SIDECAR_BASE_URL=$MBASE/mirror-sidecar///" \
		--set "EITRI_NODE_BASE_URL=$MBASE/mirror-node" --set EITRI_INSTALL_TEST_SIDECAR_VERSION=0.3.1 \
		-- --sidecar-update --release-signers "$SIGNERS"
	expect_rc 0
	mirror_requested /mirror-sidecar/releases/latest/download/SHA256SUMS
	mirror_requested /mirror-sidecar/releases/download/claude-sidecar-v0.3.1/claude-sidecar-0.3.1-source.tar.gz
	expect_exec "$UPD_ROOT/updates/0.3.1/verdandi-claude-sidecar"
	# --base-url retains its sidecar meaning and wins over the user variable.
	srv_mark
	inst --set "EITRI_SIDECAR_BASE_URL=$MBASE/unused-sidecar" \
		-- --sidecar-check --base-url "$MBASE/mirror-sidecar" --release-signers "$SIGNERS"
	expect_rc 0
	mirror_requested /mirror-sidecar/releases/latest/download/SHA256SUMS
	mirror_not_requested /unused-sidecar/
}
mirror_source_setup() {
	mirror_setup
	FS_FORK_REV=eeeeeee111111111111111111111111111111111
	# A signed-release clone also proves that the release variable covers --from-source.
	SKIA_FIXTURE_URL=${MIRROR_SKIA_URL:-https://github.com/rust-skia/skia-binaries/releases/download/fixture-tag/skia-binaries-test.tar.gz}
	fs_repo 9.9.80 https://github.com/HunterGrey-cyber/verdandi.git ddddddd000000000000000000000000000000000
	mk_from_source_release 9.9.80 ddddddd000000000000000000000000000000000 "$FS_HEAD"
	serve 9.9.80
	rm -rf "$S/srv/mirror-release/releases"
	cp -R "$S/srv/releases" "$S/srv/mirror-release/releases"
	mkdir -p "$S/srv/mirror-skia/fixture-tag"
	cp "$S/fix/skia-binaries-test.tar.gz" "$S/srv/mirror-skia/fixture-tag/"
	PRE_STUBS=$(fs_cargo_stubs)
}
mirror_source_inst() {
	inst --set "EITRI_RELEASE_BASE_URL=$MBASE/mirror-release" --set "EITRI_NODE_BASE_URL=$MBASE/mirror-node" \
		--set "EITRI_SKIA_BASE_URL=$MBASE/mirror-skia///" \
		--set "EITRI_INSTALL_TEST_SKIA_BASE_URL=$MBASE/unused-skia" \
		--set "EITRI_INSTALL_TEST_REPO_URL=$FS_DIR" \
		-- --from-source --version 9.9.80 --release-signers "$SIGNERS"
}
t_mirror_skia_used() {
	mirror_source_setup
	mirror_source_inst
	expect_rc 0
	mirror_requested /mirror-skia/fixture-tag/skia-binaries-test.tar.gz
	mirror_requested /mirror-release/releases/download/v9.9.80/RELEASE
	mirror_not_requested /unused-skia/
	expect_eq "$(installed_version)" 9.9.80 "source install through mirrors"
}

TESTS="$TESTS t_mirror_release_tamper t_mirror_node_tamper t_mirror_nvim_tamper t_mirror_grpc_tamper t_mirror_sidecar_sums_tamper t_mirror_release_unknown_signer t_mirror_skia_tamper t_mirror_skia_unknown_prefix"
t_mirror_release_tamper() {
	mirror_setup
	printf altered >>"$S/srv/mirror-release/releases/download/v1.0.0/eitri-1.0.0-x86_64-linux.tar.gz"
	inst --set "EITRI_RELEASE_BASE_URL=$MBASE/mirror-release" -- --release-signers "$SIGNERS"
	expect_fail "altered release mirror tarball"
	expect_out 'checksum mismatch for eitri-1.0.0-x86_64-linux.tar.gz'
	expect_absent "$TH/.local/lib/eitri"
	expect_absent "$TH/.local/bin/eitri"
}
t_mirror_node_tamper() {
	mirror_setup
	printf altered >>"$S/srv/mirror-node/$NODE_FIXTURE_VERSION/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
	sc_inst_net --set "EITRI_NODE_BASE_URL=$MBASE/mirror-node"
	expect_fail "altered Node mirror tarball"
	expect_out "checksum mismatch for node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
	expect_absent "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
	expect_absent "$TH/.local/lib/eitri"
}
t_mirror_nvim_tamper() {
	mirror_setup
	printf altered >>"$S/srv/mirror-nvim/v$NVIM_FIXTURE_VERSION/nvim-linux-x86_64.tar.gz"
	PRE_STUBS=$(old_nvim_stubs)
	inst --set "EITRI_NVIM_BASE_URL=$MBASE/mirror-nvim" \
		-- --base-url "$MBASE" --release-signers "$SIGNERS" --with-nvim
	expect_fail "altered nvim mirror tarball"
	expect_out 'checksum mismatch for nvim-linux-x86_64.tar.gz'
	expect_absent "$(data_of)/eitri/nvim/$NVIM_FIXTURE_VERSION"
}
t_mirror_grpc_tamper() {
	mirror_setup
	printf altered >>"$S/srv/mirror-grpc/grpc-tools/v$GRPC_FIXTURE_VERSION/linux-x64.tar.gz"
	sc_inst_net --set "EITRI_GRPC_BASE_URL=$MBASE/mirror-grpc"
	expect_fail "altered protoc mirror archive"
	expect_out 'checksum mismatch for linux-x64.tar.gz'
	expect_absent "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
}
t_mirror_sidecar_sums_tamper() {
	mirror_sidecar_setup
	printf altered >>"$S/srv/mirror-sidecar/releases/latest/download/SHA256SUMS"
	inst --set "EITRI_SIDECAR_BASE_URL=$MBASE/mirror-sidecar" -- --sidecar-check --release-signers "$SIGNERS"
	expect_fail "altered sidecar mirror sums"
	expect_out 'signature on SHA256SUMS does not verify'
	expect_absent "$UPD_ROOT/updates"
}
t_mirror_release_unknown_signer() {
	mirror_setup
	d=$S/srv/mirror-release/releases/download/v1.0.0
	sign_sums "$d" other
	# The installer has a pinned test key; this run supplies no signer option or environment key.
	INSTALLER_UNDER_TEST=$KEYED_INSTALLER
	inst --set "EITRI_RELEASE_BASE_URL=$MBASE/mirror-release" --
	expect_fail "mirror cannot replace the pinned signer"
	expect_out 'signature on SHA256SUMS does not verify'
	expect_absent "$TH/.local/lib/eitri"
}
t_mirror_skia_tamper() {
	mirror_source_setup
	printf altered >>"$S/srv/mirror-skia/fixture-tag/skia-binaries-test.tar.gz"
	mirror_source_inst
	expect_fail "altered Skia mirror archive"
	expect_out 'checksum mismatch for skia-binaries-test.tar.gz'
	if grep -q '^cargo build ' "$S/logs/cargo.log"; then fail 'cargo built with an altered Skia archive'; fi
	expect_absent "$TH/.local/lib/eitri"
}
t_mirror_skia_unknown_prefix() {
	MIRROR_SKIA_URL=https://example.invalid/skia-binaries-test.tar.gz
	mirror_source_setup
	mirror_source_inst
	expect_fail "Skia mirror with unfamiliar pinned upstream"
	expect_out 'EITRI_SKIA_BASE_URL: the pinned SKIA_BINARIES_URL_UPSTREAM must start with'
	mirror_not_requested /mirror-skia/
}

# Refusal in a mode that would not use these values still precedes the lock and all I/O.
mirror_http_refused() {
	inst --stubs "$S/stubs-net" --set "$1=http://mirror.example/" -- --release-signers "$SIGNERS"
	expect_fail "$1 plain HTTP"
	expect_out "eitri: error: $1=http://mirror.example/: "
	mirror_no_curl
}
TESTS="$TESTS t_mirror_release_http_refused t_mirror_sidecar_http_refused t_mirror_nvim_http_refused t_mirror_node_http_refused t_mirror_grpc_http_refused t_mirror_skia_http_refused"
t_mirror_release_http_refused() { mirror_http_refused EITRI_RELEASE_BASE_URL; }
t_mirror_sidecar_http_refused() { mirror_http_refused EITRI_SIDECAR_BASE_URL; }
t_mirror_nvim_http_refused() { mirror_http_refused EITRI_NVIM_BASE_URL; }
t_mirror_node_http_refused() { mirror_http_refused EITRI_NODE_BASE_URL; }
t_mirror_grpc_http_refused() { mirror_http_refused EITRI_GRPC_BASE_URL; }
t_mirror_skia_http_refused() { mirror_http_refused EITRI_SKIA_BASE_URL; }

TESTS="$TESTS t_mirror_credentials_refused t_mirror_cli_credentials_refused t_mirror_query_refused t_mirror_url_characters_refused t_mirror_unused_bad_variable t_mirror_help t_mirror_empty_unset"
# The credential URLs are put together from $MIRROR_AT so the public tree's leak scan does not read them as
# email addresses.
MIRROR_AT=@
t_mirror_credentials_refused() {
	inst --stubs "$S/stubs-net" --set "EITRI_NODE_BASE_URL=https://user:s3cret${MIRROR_AT}mirror.example/dist" --
	expect_fail 'credentials in a mirror URL'
	expect_out 'eitri: error: EITRI_NODE_BASE_URL=https://***@mirror.example/dist:'
	# shellcheck disable=SC2088 # literal curl configuration paths in the diagnostic
	expect_out "~/.netrc via ~/.curlrc"
	expect_no_out s3cret
	expect_no_out user:
	mirror_no_curl
}
t_mirror_cli_credentials_refused() {
	inst --stubs "$S/stubs-net" -- --base-url "https://user:s3cret${MIRROR_AT}mirror.example"
	expect_fail 'credentials in --base-url'
	expect_out 'eitri: error: --base-url https://***@mirror.example:'
	expect_no_out s3cret
	expect_no_out user:
	# A password holding `/` ends what looks like the authority early; the `@` is still refused, and
	# nothing before the last `@` is printed.
	inst --stubs "$S/stubs-net" --set "EITRI_NODE_BASE_URL=https://user:s3/cret${MIRROR_AT}mirror.example/dist" --
	expect_fail 'credentials with a slash in a mirror URL'
	expect_out 'eitri: error: EITRI_NODE_BASE_URL=https://***@mirror.example/dist:'
	expect_no_out s3/
	expect_no_out "cret$MIRROR_AT"
	expect_no_out user:
	mirror_no_curl
}
t_mirror_query_refused() {
	# A refused query or fragment is printed as ?***, in a mode that never downloads too.
	inst --stubs "$S/stubs-net" --set 'EITRI_NODE_BASE_URL=https://mirror.example/dist?token=s3cret' -- --sidecar-status
	expect_fail 'query in mirror URL'
	expect_out 'eitri: error: EITRI_NODE_BASE_URL=https://mirror.example/dist?***:'
	expect_no_out s3cret
	# With no scheme, `://` may sit inside a password: nothing before the last `@` is shown.
	inst --stubs "$S/stubs-net" --set "EITRI_NODE_BASE_URL=user:pa://s3cret${MIRROR_AT}mirror.example" --
	expect_fail 'no scheme, credentials holding ://'
	expect_out "eitri: error: EITRI_NODE_BASE_URL=***${MIRROR_AT}mirror.example:"
	expect_no_out s3cret
	expect_no_out user:
	inst --stubs "$S/stubs-net" -- --base-url "https://user:pqz7${MIRROR_AT}mirror.example/r#token=s3cret"
	expect_fail 'fragment and user part in --base-url'
	expect_out 'eitri: error: --base-url https://***@mirror.example/r?***:'
	expect_no_out s3cret
	expect_no_out pqz7
	mirror_no_curl
}
t_mirror_url_characters_refused() {
	for u in 'https:///missing-host' 'https://:443/dist' 'https://mirror.example/#frag' \
		'https://mirror.example/a b' 'https://mirror.example/a\b' "https://mirror.example/$(printf '\001')"; do
		inst --stubs "$S/stubs-net" --set "EITRI_RELEASE_BASE_URL=$u" --
		expect_fail 'invalid HTTPS mirror URL'
		expect_out 'eitri: error: EITRI_RELEASE_BASE_URL='
		inst --stubs "$S/stubs-net" -- --base-url "$u"
		expect_fail 'invalid HTTPS --base-url'
		expect_out 'eitri: error: --base-url '
	done
	mirror_no_curl
}
t_mirror_unused_bad_variable() {
	before=$(snap "$TH")
	inst --stubs "$S/stubs-net" --set EITRI_SKIA_BASE_URL=http://mirror.example -- --base-url "http://127.0.0.1:$PORT"
	expect_fail 'bad Skia variable during a plain install'
	expect_out 'eitri: error: EITRI_SKIA_BASE_URL='
	expect_eq "$(snap "$TH")" "$before" 'HOME unchanged on unused variable refusal'
	mirror_no_curl
}
t_mirror_help() {
	# Help prints whatever the environment holds: it reads no address.
	inst --set EITRI_SKIA_BASE_URL=http://mirror.example -- --help
	expect_rc 0
	for v in RELEASE SIDECAR NODE NVIM GRPC SKIA; do expect_out "EITRI_${v}_BASE_URL"; done
	expect_out 'HTTPS only'
	expect_out 'checked against the signed release either way'
}
t_mirror_empty_unset() {
	mirror_setup
	inst --set EITRI_RELEASE_BASE_URL= --set EITRI_SIDECAR_BASE_URL= --set EITRI_NODE_BASE_URL= \
		--set EITRI_NVIM_BASE_URL= --set EITRI_GRPC_BASE_URL= --set EITRI_SKIA_BASE_URL= \
		-- --base-url "$MBASE" --release-signers "$SIGNERS"
	expect_rc 0
	mirror_requested /releases/latest/download/SHA256SUMS
	expect_absent "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
}

TESTS="$TESTS t_mirror_release_network_hint t_mirror_node_network_hint t_mirror_server_failure_source t_mirror_npm_registry_passthrough"
t_mirror_release_network_hint() {
	mirror_setup
	mkdir -p "$S/logs/curl-fail"
	printf '99\n' >"$S/logs/curl-fail/SHA256SUMS"
	inst --stubs "$S/stubs-net" --set EITRI_RELEASE_BASE_URL=https://mirror.example/mirror-release -- --release-signers "$SIGNERS"
	rm -rf "$S/logs/curl-fail"
	expect_fail 'release mirror network failure'
	expect_out 'https_proxy'
	expect_out '--base-url URL or EITRI_RELEASE_BASE_URL=URL downloads the release files from a mirror instead'
	expect_out '(from EITRI_RELEASE_BASE_URL)'
}
t_mirror_node_network_hint() {
	mirror_setup
	mkdir -p "$S/logs/curl-fail"
	printf '99\n' >"$S/logs/curl-fail/node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz"
	inst --stubs "$S/stubs-net" --set EITRI_NODE_BASE_URL=https://mirror.example/mirror-node \
		-- --base-url "$MBASE" --release-signers "$SIGNERS"
	rm -rf "$S/logs/curl-fail"
	expect_rc 0
	expect_out 'https_proxy'
	expect_out 'EITRI_NODE_BASE_URL=URL downloads this upstream from a mirror instead'
	expect_out '(from EITRI_NODE_BASE_URL)'
}
t_mirror_server_failure_source() {
	mirror_setup
	inst --set "EITRI_RELEASE_BASE_URL=$MBASE/missing-release" -- --release-signers "$SIGNERS"
	expect_fail 'release mirror 404'
	expect_out 'returned error: 404'
	expect_out '(from EITRI_RELEASE_BASE_URL)'
	inst -- --base-url "$MBASE/missing-release" --release-signers "$SIGNERS"
	expect_fail '--base-url 404'
	expect_out '(from --base-url)'
	inst --set "EITRI_NODE_BASE_URL=$MBASE/missing-node" -- --base-url "$MBASE" --release-signers "$SIGNERS"
	expect_rc 0
	expect_out 'returned error: 404'
	expect_out '(from EITRI_NODE_BASE_URL)'
}
t_mirror_npm_registry_passthrough() {
	mirror_setup
	sc_setup_release 1.0.0 "$REV_A"
	sc_inst_setup --set npm_config_registry=https://npm.example/company/ \
		--set "EITRI_INSTALL_TEST_NPM_ENV_LOG=$T/npm-env.log" -- --sidecar-only
	expect_rc 0
	expect_eq "$(cat "$T/npm-env.log")" 'fetch_retries=5 fetch_retry_mintimeout=2000 registry=https://npm.example/company/' 'user npm registry'
	rm -r "$(data_of)/eitri/sidecar"
	sc_inst_setup --set "EITRI_INSTALL_TEST_NPM_ENV_LOG=$T/npm-env-default.log" -- --sidecar-only
	expect_rc 0
	expect_eq "$(cat "$T/npm-env-default.log")" 'fetch_retries=5 fetch_retry_mintimeout=2000 registry=unset' 'no installer npm registry'
}

TESTS="$TESTS t_mirror_release_sidecar_only_used t_mirror_release_older_layout_hint t_mirror_bad_variable_all_modes"
t_mirror_release_sidecar_only_used() {
	mirror_setup
	sc_setup_release 1.0.0 "$REV_A"
	inst --set "EITRI_RELEASE_BASE_URL=$MBASE/mirror-release" --set "EITRI_NODE_BASE_URL=$MBASE/mirror-node" \
		-- --sidecar-only
	expect_rc 0
	mirror_requested /mirror-release/releases/download/v1.0.0/verdandi-aaaaaaa-source.tar.gz
	expect_exec "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
}
t_mirror_release_older_layout_hint() {
	inst --stubs "$S/stubs-net" --set EITRI_RELEASE_BASE_URL=https://mirror.example/project/// -- --version 0.3.0
	expect_fail 'older release under a user mirror'
	expect_out 'Eitri releases before 0.4.0 are no longer offered'
	expect_no_out 'releases/download/v0.3.0'
	expect_no_out 'mirror.example'
	expect_no_out 'a mirror'"'"'s copy of install.sh'
	mirror_no_curl
}
t_mirror_bad_variable_all_modes() {
	before=$(snap "$TH")
	for mode in --uninstall --sidecar-only --sidecar-status --sidecar-update --sidecar-check \
		--nvim-only --nvim-offer --from-source --dry-run; do
		inst --stubs "$S/stubs-net" --set EITRI_SKIA_BASE_URL=http://mirror.example -- "$mode"
		expect_fail "bad unused variable in $mode"
		expect_out 'eitri: error: EITRI_SKIA_BASE_URL='
	done
	inst --stubs "$S/stubs-net" --set EITRI_SKIA_BASE_URL=http://mirror.example -- --sidecar-use bundled
	expect_fail 'bad unused variable in --sidecar-use'
	expect_out 'eitri: error: EITRI_SKIA_BASE_URL='
	inst --stubs "$S/stubs-net" --set EITRI_SKIA_BASE_URL=http://mirror.example \
		-- --build-sidecar-into "$T/dest" --node "$T/node" --verdandi-source "$T/source"
	expect_fail 'bad unused variable in --build-sidecar-into'
	expect_out 'eitri: error: EITRI_SKIA_BASE_URL='
	expect_eq "$(snap "$TH")" "$before" 'HOME unchanged in all modes'
	mirror_no_curl
}
