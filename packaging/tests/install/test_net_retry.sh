# Downloads on a network that drops, stalls and refuses: net_get's retries, resumption and messages.
# Sourced by harness.sh.
# shellcheck shell=sh
# shellcheck disable=SC2034 # RC, TH and the like are read by harness.sh's helpers

# flaky_start PLAN: a loopback file server over $S/srv that answers each request whose path ends in
# a key of PLAN (a JSON object of path suffix -> list of actions) with the next action of that key's
# list, and serves normally (Range included) once the list is used up. Actions: "reset" (no answer,
# the connection reset), "500", "norange" (a whole 200 even when a range was asked for),
# "drop:F" / "stall:F" (the headers of the whole answer and fraction F of its body, then a reset, or
# silence for 30 s and then a reset), "dropnolen:N" (N bytes with no Content-Length, then a reset),
# "416" (a 416 to whatever was asked), "garble" (the range asked for, of the file's bytes reversed). Every request is logged to $T/flaky.log as
# "<path> range=<Range or -> act=<action>". Sets FPORT and FLAKY_PID.
flaky_start() {
	printf '%s\n' "$1" >"$T/flaky.plan"
	cat >"$T/flaky.py" <<'PY'
import http.server
import json
import pathlib
import socket
import struct
import sys
import threading
import time

root, plan_file, port_file, log_file = sys.argv[1:]
plan = json.loads(pathlib.Path(plan_file).read_text())
lock = threading.Lock()


class Flaky(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reset(self):
        self.close_connection = True
        self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        self.connection.close()

    def do_GET(self):
        path = self.path.split("?")[0]
        act = None
        with lock:
            for key, actions in plan.items():
                if path.endswith(key) and actions:
                    act = actions.pop(0)
                    break
            with open(log_file, "a") as log:
                log.write(f"{path} range={self.headers.get('Range', '-')} act={act or 'serve'}\n")
        if act == "reset":
            return self.reset()
        if act == "500":
            self.send_error(500)
            return
        if act == "416":
            size = (pathlib.Path(root) / path.lstrip("/")).stat().st_size
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{size}")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        file = pathlib.Path(root) / path.lstrip("/")
        if not file.is_file():
            self.send_error(404)
            return
        data = file.read_bytes()
        if act == "garble":
            data = data[::-1]
        start = 0
        asked = self.headers.get("Range", "")
        if asked.startswith("bytes=") and asked.endswith("-") and act != "norange":
            start = int(asked[len("bytes="):-1])
            if start >= len(data):
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{len(data)}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {start}-{len(data) - 1}/{len(data)}")
        else:
            self.send_response(200)
        body = data[start:]
        if act and act.startswith("dropnolen:"):
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()
            self.wfile.write(body[: int(act.split(":")[1])])
            self.wfile.flush()
            return self.reset()
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()
        if act and act.split(":")[0] in ("drop", "stall"):
            kind, fraction = act.split(":")
            self.wfile.write(body[: int(len(body) * float(fraction))])
            self.wfile.flush()
            if kind == "stall":
                time.sleep(30)
            return self.reset()
        self.wfile.write(body)


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Flaky)
server.daemon_threads = True
pathlib.Path(port_file).write_text(str(server.server_port))
server.serve_forever()
PY
	: >"$T/flaky.log"
	python3 "$T/flaky.py" "$S/srv" "$T/flaky.plan" "$T/flaky.port" "$T/flaky.log" >"$T/flaky.out" 2>&1 &
	FLAKY_PID=$!
	# A test body runs in a subshell of its own, which has no EXIT trap of the harness's: this one stops only
	# this server.
	trap 'kill "$FLAKY_PID" 2>/dev/null' EXIT
	_fs_tries=0
	while [ ! -s "$T/flaky.port" ] && [ "$_fs_tries" -lt 100 ]; do
		sleep 0.1
		_fs_tries=$((_fs_tries + 1))
	done
	FPORT=$(cat "$T/flaky.port" 2>/dev/null)
	if [ -z "$FPORT" ]; then fail "the flaky server did not start: $(cat "$T/flaky.out")"; fi
}

flaky_stop() {
	kill "$FLAKY_PID" 2>/dev/null
	wait "$FLAKY_PID" 2>/dev/null
	trap - EXIT
}

# flaky_count SUFFIX: how many requests the flaky server saw for paths ending in SUFFIX.
flaky_count() {
	awk -v s="$1" '{ p = $1; if (substr(p, length(p) - length(s) + 1) == s) n++ } END { print n + 0 }' "$T/flaky.log"
}

# flaky_inst [--set K=V]...: an install from the flaky server, as inst_net is from the fixture server.
flaky_inst() {
	inst "$@" -- --base-url "http://127.0.0.1:$FPORT" --release-signers "$SIGNERS"
}

TESTS="$TESTS t_net_drops_resume"
t_net_drops_resume() {
	serve 1.0.0
	tb=eitri-1.0.0-x86_64-linux.tar.gz
	flaky_start '{"/SHA256SUMS": ["reset"], "/SHA256SUMS.sig": ["500"], "/'"$tb"'": ["drop:0.3", "drop:0.5"]}'
	flaky_inst
	flaky_stop
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "installed through a network that dropped three downloads"
	# The tarball: a whole first try, then two resumed from where the file stopped.
	expect_eq "$(flaky_count "/$tb")" 3 "tarball requests"
	expect_eq "$(grep -F "/$tb" "$T/flaky.log" | sed -n 1p | sed 's/.* range=\([^ ]*\) .*/\1/')" - "the first tarball request's range"
	for i in 2 3; do
		r=$(grep -F "/$tb" "$T/flaky.log" | sed -n "${i}p" | sed 's/.* range=\([^ ]*\) .*/\1/')
		case $r in
		bytes=[1-9]*-) ;;
		*) fail "tarball request $i asked for range '$r', not a resumption" ;;
		esac
	done
	expect_out "eitri: $tb from 127.0.0.1:$FPORT: curl: ("
	expect_out 'trying again in 0 s (attempt 2), resuming at'
	expect_out 'eitri: SHA256SUMS.sig from 127.0.0.1:'"$FPORT"': curl: (22) The requested URL returned error: 500'
	expect_eq "$(flaky_count /SHA256SUMS.sig)" 2 "signature requests (a 500, then the file)"
	expect_no_out 'eitri: error:'
}

TESTS="$TESTS t_net_stall_resumes"
t_net_stall_resumes() {
	serve 1.0.0
	tb=eitri-1.0.0-x86_64-linux.tar.gz
	flaky_start '{"/'"$tb"'": ["stall:0.4"]}'
	flaky_inst --set EITRI_INSTALL_TEST_NET_STALL=2
	flaky_stop
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "installed after a stalled download"
	expect_out "eitri: $tb from 127.0.0.1:$FPORT: curl: (28)"
	expect_eq "$(flaky_count "/$tb")" 2 "tarball requests (the stalled one, then the rest)"
	case $(grep -F "/$tb" "$T/flaky.log" | sed -n 2p) in
	*' range=bytes='[1-9]*) ;;
	*) fail "the request after the stall was no resumption: $(cat "$T/flaky.log")" ;;
	esac
}

TESTS="$TESTS t_net_no_ranges_starts_over"
t_net_no_ranges_starts_over() {
	serve 1.0.0
	tb=eitri-1.0.0-x86_64-linux.tar.gz
	flaky_start '{"/'"$tb"'": ["drop:0.5", "norange"]}'
	flaky_inst
	flaky_stop
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "installed from a server that cannot resume"
	expect_out "eitri: $tb from 127.0.0.1:$FPORT: the server cannot resume it (curl: (33)"
	expect_out 'downloading it again from the start'
	expect_eq "$(flaky_count "/$tb")" 3 "tarball requests (dropped, refused range, whole)"
	expect_eq "$(grep -F "/$tb" "$T/flaky.log" | sed -n 3p | sed 's/.* range=\([^ ]*\) .*/\1/')" - "the last request's range"
}

TESTS="$TESTS t_net_corrupt_resume_refused"
t_net_corrupt_resume_refused() {
	# A server that resumes with other bytes than it began with: the joined file fails its hash and
	# nothing is installed, as with any corrupt download.
	serve 1.0.0
	tb=eitri-1.0.0-x86_64-linux.tar.gz
	flaky_start '{"/'"$tb"'": ["drop:0.5", "garble"]}'
	flaky_inst
	flaky_stop
	expect_fail "a resumption that joined two different files"
	case $(grep -F "/$tb" "$T/flaky.log" | sed -n 2p) in
	*' range=bytes='[1-9]*' act=garble') ;;
	*) fail "the second tarball request was no garbled resumption: $(cat "$T/flaky.log")" ;;
	esac
	expect_out "checksum mismatch for $tb"
	expect_absent "$TH/.local/lib/eitri"
	expect_absent "$TH/.local/bin/eitri"
}

TESTS="$TESTS t_net_gives_up_and_says_why"
t_net_gives_up_and_says_why() {
	serve 1.0.0
	flaky_start '{"/SHA256SUMS": ["reset", "reset", "reset", "reset", "reset", "reset", "reset", "reset", "reset", "reset"]}'
	flaky_inst
	flaky_stop
	expect_fail "a SHA256SUMS that never arrives"
	expect_eq "$(flaky_count /SHA256SUMS)" 8 "SHA256SUMS requests before giving up"
	expect_out "eitri: error: could not download SHA256SUMS from http://127.0.0.1:$FPORT/releases/latest/download/SHA256SUMS: curl: ("
	expect_out "; 8 attempts over "
	expect_out " s to 127.0.0.1:$FPORT; check that the server at 127.0.0.1:$FPORT is running"
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_net_answers_not_retried"
t_net_answers_not_retried() {
	# A 404 is an answer: asked once, and the message is curl's alone (no network advice).
	serve 1.0.0
	flaky_start '{}'
	inst -- --base-url "http://127.0.0.1:$FPORT" --release-signers "$SIGNERS" --version 9.9.9
	flaky_stop
	expect_fail "a release that does not exist"
	expect_eq "$(flaky_count /SHA256SUMS)" 1 "SHA256SUMS requests for a 404"
	expect_out 'returned error: 404'
	expect_no_out 'trying again'
	expect_no_out 'attempts over'
}

TESTS="$TESTS t_net_node_download_resumes"
t_net_node_download_resumes() {
	# The sidecar build's own downloads (Node here) go through the same retries.
	serve 1.0.0
	node=node-$NODE_FIXTURE_VERSION-linux-x64.tar.xz
	flaky_start '{"/'"$node"'": ["reset", "drop:0.5"]}'
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$FPORT/dist" \
		-- --base-url "http://127.0.0.1:$FPORT" --release-signers "$SIGNERS"
	flaky_stop
	expect_rc 0
	expect_exec "$(data_of)/eitri/sidecar/aaaaaaa/verdandi-claude-sidecar"
	expect_eq "$(flaky_count "/$node")" 3 "Node requests (reset, dropped, resumed)"
	case $(grep -F "/$node" "$T/flaky.log" | sed -n 3p) in
	*' range=bytes='[1-9]*) ;;
	*) fail "the third Node request was no resumption: $(cat "$T/flaky.log")" ;;
	esac
}

TESTS="$TESTS t_net_https_failure_names_host_proxy_and_base_url"
t_net_https_failure_names_host_proxy_and_base_url() {
	# Through the https stand-in: a connection that keeps breaking ends in a message naming the file,
	# the host, what was tried, a proxy variable and --base-url.
	serve 1.0.0
	mkdir -p "$S/logs/curl-fail"
	printf '99\n' >"$S/logs/curl-fail/SHA256SUMS"
	inst --stubs "$S/stubs-net" -- --base-url https://mirror.example --release-signers "$SIGNERS"
	rm -rf "$S/logs/curl-fail"
	expect_fail "an https SHA256SUMS that never arrives"
	expect_eq "$(grep -c -F 'https://mirror.example/releases/latest/download/SHA256SUMS' "$S/logs/curl.log")" 8 "SHA256SUMS attempts"
	expect_out "eitri: error: could not download SHA256SUMS from https://mirror.example/releases/latest/download/SHA256SUMS: curl: (56) Recv failure: Connection reset by peer; 8 attempts over "
	expect_out " s to mirror.example. If this network reaches mirror.example only through a proxy, set https_proxy (for example https_proxy=http://127.0.0.1:7890) and re-run; --base-url URL downloads the release files from a mirror of the release page instead"
	expect_absent "$TH/.local/lib/eitri"
}

TESTS="$TESTS t_net_https_recovers"
t_net_https_recovers() {
	serve 1.0.0
	mkdir -p "$S/logs/curl-fail"
	printf '2\n' >"$S/logs/curl-fail/SHA256SUMS"
	inst --stubs "$S/stubs-net" -- --base-url https://mirror.example --release-signers "$SIGNERS"
	rm -rf "$S/logs/curl-fail"
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "installed after two broken https attempts"
	expect_count 'trying again' 2
}

TESTS="$TESTS t_net_416_on_resume_starts_over"
t_net_416_on_resume_starts_over() {
	# curl exits 0 on a 416 to a resumption and keeps the partial file; the installer starts over.
	serve 1.0.0
	tb=eitri-1.0.0-x86_64-linux.tar.gz
	flaky_start '{"/'"$tb"'": ["drop:0.5", "416"]}'
	flaky_inst
	flaky_stop
	expect_rc 0
	expect_eq "$(installed_version)" 1.0.0 "installed after a 416 to the resumption"
	expect_out 'the server cannot resume it (the server answered 416 to the resumption); downloading it again from the start'
	expect_eq "$(flaky_count "/$tb")" 3 "tarball requests (dropped, 416, whole)"
	expect_eq "$(grep -F "/$tb" "$T/flaky.log" | sed -n 3p | sed 's/.* range=\([^ ]*\) .*/\1/')" - "the last request's range"
}

TESTS="$TESTS t_net_resumed_attempts_held_to_the_limit"
t_net_resumed_attempts_held_to_the_limit() {
	# Each attempt stays under the 64 KiB a sidecar release's SHA256SUMS may be, but two resumed
	# attempts add up past it: the file is refused after the second, not grown over twenty.
	upd_state
	mk_sidecar_release 0.3.1
	head -c 100000 /dev/zero >>"$S/srv/sc/releases/latest/download/SHA256SUMS"
	flaky_start '{"/latest/download/SHA256SUMS": ["dropnolen:40000", "dropnolen:40000", "dropnolen:40000", "dropnolen:40000"]}'
	# shellcheck disable=SC2153 # PORT is the harness's own fixture server (Node and protoc come from it)
	inst --set "EITRI_INSTALL_TEST_NODE_BASE_URL=http://127.0.0.1:$PORT/dist" \
		--set "EITRI_INSTALL_TEST_GRPC_BASE_URL=http://127.0.0.1:$PORT/grpc" \
		-- --sidecar-update --base-url "http://127.0.0.1:$FPORT/sc" --release-signers "$SIGNERS"
	flaky_stop
	upd_refused "resumed attempts past 64 KiB" 'could not download SHA256SUMS'
	expect_eq "$(flaky_count /latest/download/SHA256SUMS)" 2 "SHA256SUMS requests"
}

TESTS="$TESTS t_net_dry_run_text_retries"
t_net_dry_run_text_retries() {
	# A dry run reads SHA256SUMS into text, not a file: it is retried too, and writes nothing.
	serve 1.0.0
	flaky_start '{"/latest/download/SHA256SUMS": ["reset"]}'
	before=$(snap "$TH")
	inst -- --base-url "http://127.0.0.1:$FPORT" --release-signers "$SIGNERS" --dry-run
	flaky_stop
	expect_rc 0
	expect_out 'dry run: nothing was changed'
	expect_out "eitri: SHA256SUMS from 127.0.0.1:$FPORT: curl exited with status"
	expect_out '(its message is above); trying again in 0 s (attempt 2)'
	expect_eq "$(flaky_count /latest/download/SHA256SUMS)" 2 "SHA256SUMS requests in the dry run"
	expect_eq "$(snap "$TH")" "$before" "the home after a dry run"
}
