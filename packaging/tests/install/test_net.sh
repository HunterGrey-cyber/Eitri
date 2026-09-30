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
		# Spec §6.2's own rule: no `@` anywhere in an http authority, checked before the host.
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
	# The sidecar build's own Node download is a separate, unrelated curl call (plan Task 10):
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

TESTS="$TESTS t_wget_never_used"
t_wget_never_used() {
	# Spec §6.2 names `wget --https-only` for a system without curl, but wget scopes --https-only to
	# recursive downloads: measured (GNU Wget 1.25.0), it follows an https -> http redirect, and
	# fetches a plain http:// URL, with exit 0. So without curl the installer refuses, for any base
	# URL, before any download; wget (a logging stub, first on PATH) is never run.
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
