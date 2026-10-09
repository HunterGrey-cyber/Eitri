# --base-url and how downloads are made (NET-1). Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

TESTS="$TESTS t_http_nonloopback_refused"
t_http_nonloopback_refused() {
	for u in http://example.com/ http://10.0.0.1:8080/ http://127.0.0.2/ "http://[::1]/" ftp://127.0.0.1/ \
		HTTP://127.0.0.1/ http://127.0.0.1:/ http://localhost:80x/; do
		# The logging curl stub is first on PATH: a refusal must come before any download.
		inst --stubs "$S/stubs-net" -- --base-url "$u" --release-signers "$SIGNERS"
		expect_fail "--base-url $u"
		expect_out "eitri: error: --base-url $u: "
	done
	if [ -s "$S/logs/curl.log" ]; then fail "a refused base URL was fetched: $(cat "$S/logs/curl.log")"; fi
	expect_absent "$TH/.local/lib/eitri"
	expect_absent "$TH/.cache"
}

TESTS="$TESTS t_http_tricky_hosts_refused"
t_http_tricky_hosts_refused() {
	# Spelled with $EVIL so the public tree's leak scan does not read "localhost@..." as an email.
	EVIL=evil.example
	for u in "http://localhost@$EVIL/" "http://127.0.0.1.$EVIL/" "http://127.0.0.1@$EVIL/" \
		"http://localhost.$EVIL:8080/" "http://127.0.0.1:$PORT@$EVIL/" "http://x@127.0.0.1:$PORT/"; do
		inst --stubs "$S/stubs-net" -- --base-url "$u" --release-signers "$SIGNERS"
		expect_fail "--base-url $u"
		expect_out "eitri: error: --base-url $u: "
		# Reject `@` anywhere in an HTTP authority before checking the hostname.
		case $u in *@*) expect_out "may not carry a user part ('@')" ;; esac
	done
	if [ -s "$S/logs/curl.log" ]; then fail "a refused base URL was fetched: $(cat "$S/logs/curl.log")"; fi
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_http_loopback_accepted"
t_http_loopback_accepted() {
	serve 1.0.0
	inst -- --base-url "http://127.0.0.1:$PORT/" --release-signers "$SIGNERS"
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "installed from http://127.0.0.1:<port>/"
	use_home "$T/home2"
	inst -- --base-url "http://localhost:$PORT" --release-signers "$SIGNERS"
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "installed from http://localhost:<port>"
}

TESTS="$TESTS t_https_curl_flags"
t_https_curl_flags() {
	# The sidecar build's own Node download is a separate, unrelated curl call:
	# node_dist_base's own test-mode default (http://127.0.0.1:1, nothing listening) makes it fail
	# instantly and legitimately uses plain http (it is a loopback URL, by that same default), so it
	# is filtered out here rather than asserted against the release mirror's own flags/URL.
	serve 1.0.0
	inst --stubs "$S/stubs-net" -- --base-url https://mirror.example --release-signers "$SIGNERS"
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "installed through the https mirror"
	grep -v '127.0.0.1:1/' "$S/logs/curl.log" >"$T/curl-release-only.log"
	expect_eq "$(wc -l <"$T/curl-release-only.log" | tr -d ' ')" 4 "curl calls to the release mirror"
	while IFS= read -r line; do
		case $line in
		*" --proto =https --proto-redir =https --tlsv1.2 "*) ;;
		*) fail "a curl call without --proto =https --proto-redir =https --tlsv1.2: $line" ;;
		esac
		case $line in *https://mirror.example/releases/*) ;; *) fail "unexpected URL: $line" ;; esac
	done <"$T/curl-release-only.log"
}

TESTS="$TESTS t_downloads_bounded"
t_downloads_bounded() {
	# A server that never ends a download, or stalls in the middle of one, must not hold the installer
	# for good: every curl call, the release's and the plain-http Node one alike, carries a size cap and
	# stall detection. The limits are generous (a slow link still finishes); only their presence is
	# checked here.
	serve 1.0.0
	inst --stubs "$S/stubs-net" -- --base-url https://mirror.example --release-signers "$SIGNERS"
	expect_rc 0
	if ! grep -F '127.0.0.1:1/' "$S/logs/curl.log" >/dev/null; then fail "no plain-http curl call was made"; fi
	while IFS= read -r line; do
		for want in " --connect-timeout 60 " " --speed-limit 1024 --speed-time 300 " " --max-filesize 1073741824 "; do
			case $line in
			*"$want"*) ;;
			*) fail "a curl call without$want: $line" ;;
			esac
		done
	done <"$S/logs/curl.log"
}

TESTS="$TESTS t_wget_never_used"
t_wget_never_used() {
	# wget's --https-only applies to recursive downloads. GNU Wget 1.25.0 can still follow an
	# https-to-http redirect or fetch a plain http URL successfully without recursive mode.
	# Without curl the installer refuses before downloading, regardless of the base URL;
	# the logging wget stub must never run.
	serve 1.0.0
	for u in https://mirror.example "http://127.0.0.1:$PORT"; do
		inst --stubs "$S/stubs-wget" --path-tail "$S/path-no-curl" -- --base-url "$u" --release-signers "$SIGNERS"
		expect_fail "no curl, --base-url $u"
		expect_out 'curl was not found: this installer downloads only with curl'
		expect_out 'Install curl (sudo apt install curl) and re-run'
	done
	if [ -s "$S/logs/wget.log" ]; then fail "wget was run: $(cat "$S/logs/wget.log")"; fi
	expect_absent "$TH/.local/lib/eitri"
	expect_absent "$TH/.cache"
	# An offline install downloads nothing, so it needs no curl.
	d=$S/fix/v1.0.0
	inst --stubs "$S/stubs-wget" --path-tail "$S/path-no-curl" -- --tarball "$d/eitri-1.0.0-x86_64-linux.tar.gz" \
		--sums "$d/SHA256SUMS" --sig "$d/SHA256SUMS.sig" --release-signers "$SIGNERS"
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "installed from files with no curl"
	if [ -s "$S/logs/wget.log" ]; then fail "wget was run: $(cat "$S/logs/wget.log")"; fi
}

TESTS="$TESTS t_test_mode_overrides_reach_only_loopback"
t_test_mode_overrides_reach_only_loopback() {
	# Test mode's download overrides lead only to a loopback server and its repository overrides only to
	# a local path, so an EITRI_INSTALL_TEST=1 left in an environment cannot send a download, a clone or a
	# build anywhere else. Each refusal comes before any download.
	EVIL=evil.example
	for v in "EITRI_INSTALL_TEST_NODE_BASE_URL=http://$EVIL/dist" \
		"EITRI_INSTALL_TEST_NVIM_BASE_URL=https://$EVIL/nvim" \
		"EITRI_INSTALL_TEST_SKIA_BASE_URL=http://127.0.0.1.$EVIL/skia" \
		"EITRI_INSTALL_TEST_NODE_BASE_URL=http://localhost@$EVIL/dist" \
		"EITRI_INSTALL_TEST_REPO_URL=https://$EVIL/eitri.git" \
		"EITRI_INSTALL_TEST_VERDANDI_REPO_URL=relative/verdandi"; do
		inst --stubs "$S/stubs-net" --set "$v" -- --base-url "http://127.0.0.1:$PORT/" --release-signers "$SIGNERS"
		expect_fail "$v"
		expect_out "eitri: error: ${v%%=*}"
	done
	if [ -s "$S/logs/curl.log" ]; then fail "a refused override was fetched: $(cat "$S/logs/curl.log")"; fi
	expect_absent "$TH/.local/lib/eitri"
}
