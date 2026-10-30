# --base-url and how downloads are made. Sourced by harness.sh.
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
		# The rule: no `@` anywhere in an http authority, checked before the host.
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
		case $line in
		*' --proto =http '*)
			case $line in
			'curl -q --max-redirs 0 '*) ;;
			*) fail "a loopback curl call reads config or permits redirects: $line" ;;
			esac
			;;
		esac
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
	# GNU Wget 1.25.0 follows an https -> http redirect and fetches a plain http:// URL with exit 0:
	# `wget --https-only` applies only to recursive downloads. Without curl the installer must refuse any
	# base URL before downloading; the logging wget stub must never run.
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

TESTS="$TESTS t_loopback_ignores_curl_config_redirects"
t_loopback_ignores_curl_config_redirects() {
	serve 1.0.0
	# A config curl must not read: it would follow redirects and mark every request it sends.
	printf 'location\nheader = "X-Curlrc: 1"\n' >"$TH/.curlrc"
	# Both servers stay on loopback. The first records that SHA256SUMS was asked for, and whether the request
	# carried the config's header, then redirects; the second records any request before serving a real fixture.
	cat >"$T/redirect.py" <<'PY'
import functools
import http.server
import pathlib
import sys
import threading

root, ports, contacted, asked, configured = sys.argv[1:]

class Destination(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        pathlib.Path(contacted).touch()
        super().do_GET()

destination = http.server.ThreadingHTTPServer(
    ("127.0.0.1", 0), functools.partial(Destination, directory=root))

class Redirect(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.headers.get("X-Curlrc"):
            pathlib.Path(configured).touch()
        if self.path.endswith("/SHA256SUMS"):
            pathlib.Path(asked).touch()
            self.send_response(302)
            self.send_header("Location", f"http://127.0.0.1:{destination.server_port}{self.path}")
            self.end_headers()
        else:
            super().do_GET()

source = http.server.ThreadingHTTPServer(
    ("127.0.0.1", 0), functools.partial(Redirect, directory=root))
threading.Thread(target=destination.serve_forever, daemon=True).start()
pathlib.Path(ports).write_text(str(source.server_port))
source.serve_forever()
PY
	python3 "$T/redirect.py" "$S/srv" "$T/redirect.port" "$T/redirect.contacted" "$T/redirect.asked" \
		"$T/redirect.configured" >"$T/redirect.log" 2>&1 &
	redirect_pid=$!
	# A test body runs in a subshell of its own, which has no EXIT trap of the harness's: this one stops only the
	# redirect server (stop_server here would kill the fixture server the later tests still use).
	trap 'kill "$redirect_pid" 2>/dev/null' EXIT
	redirect_tries=0
	while [ ! -s "$T/redirect.port" ] && [ "$redirect_tries" -lt 100 ]; do
		sleep 0.1
		redirect_tries=$((redirect_tries + 1))
	done
	if [ ! -s "$T/redirect.port" ]; then
		stop_redirect_server
		fail "the redirect server did not start"
		return
	fi
	redirect_port=$(cat "$T/redirect.port")
	inst -- --base-url "http://127.0.0.1:$redirect_port" --release-signers "$SIGNERS"
	expect_fail "a redirected loopback SHA256SUMS download"
	stop_redirect_server
	expect_file "$T/redirect.asked"
	expect_absent "$T/redirect.configured"
	expect_absent "$T/redirect.contacted"
	expect_absent "$TH/.local/lib/eitri"
}

stop_redirect_server() {
	kill "$redirect_pid" 2>/dev/null
	wait "$redirect_pid" 2>/dev/null
	trap - EXIT
}
