#!/usr/bin/env bash
# Headless checks for nvim/eitri.nvim: the command exists, `edge` is false with no panel attached, and a
# launcher that fails is reported as an error message. Needs a real nvim on PATH; spends nothing and needs no
# display. Standalone: `bash packaging/tests/test_eitri_nvim.sh`.
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN="$(cd "$HERE/../../nvim/eitri.nvim" && pwd)"

command -v nvim >/dev/null || { echo "SKIP - no nvim on PATH"; exit 0; }

# Scratch XDG dirs, never the owner's own config or state.
SCRATCH="$(mktemp -d "${TMPDIR:-$HOME/.cache}/eitri-nvim-test-XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
export XDG_CONFIG_HOME="$SCRATCH/config" XDG_DATA_HOME="$SCRATCH/data" XDG_STATE_HOME="$SCRATCH/state" XDG_CACHE_HOME="$SCRATCH/cache"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"

FAILURES=0
CHECKS=0

# check DESCRIPTION LUA-FILE: run a Lua script in a clean headless nvim with the plugin on the runtimepath; the
# script prints "OK" on success and anything else is a failure.
check() {
	CHECKS=$((CHECKS + 1))
	local out
	out="$(timeout 30 nvim --headless --clean -u NONE --cmd "set rtp+=$PLUGIN" -c 'runtime plugin/eitri.lua' \
		-c "luafile $2" -c qa 2>&1)"
	if [[ "$out" == *"OK"* && "$out" != *"FAIL"* && "$out" != *"E5108"* && "$out" != *"Error"* ]]; then
		echo "ok - $1"
	else
		echo "FAIL - $1"
		echo "  output: $out"
		FAILURES=$((FAILURES + 1))
	fi
}

cat >"$SCRATCH/basic.lua" <<'LUA'
assert(vim.fn.exists(":EitriPanel") == 2, "the :EitriPanel command is missing")
assert(require("eitri").edge("left") == false, "edge is true with no panel attached")
assert(vim.g.loaded_eitri == 1)
io.stdout:write("OK\n")
LUA
check "the command exists and edge() is false with no panel" "$SCRATCH/basic.lua"

cat >"$SCRATCH/edge.lua" <<'LUA'
local seen
_G.__eitri_companion = { edge = function(d) seen = d; return true end }
assert(require("eitri").edge("up") == true, "a panel's answer is passed through")
assert(seen == "up", "the direction reaches the panel")
_G.__eitri_companion = { edge = function() return false end }
assert(require("eitri").edge("up") == false)
_G.__eitri_companion = 5
assert(require("eitri").edge("up") == false, "a malformed global is not a panel")
io.stdout:write("OK\n")
LUA
check "edge() asks an attached panel, and survives a malformed global" "$SCRATCH/edge.lua"

cat >"$SCRATCH/fail.lua" <<'LUA'
local msgs = {}
vim.notify = function(msg, level) table.insert(msgs, { msg, level }) end
require("eitri").setup({ cmd = "false" })
vim.cmd("EitriPanel")
local ok = vim.wait(5000, function() return #msgs > 0 end, 20)
assert(ok, "no error message after a failing launcher")
assert(msgs[1][2] == vim.log.levels.ERROR, "the message is not an error")
assert(msgs[1][1]:find("eitri panel", 1, true), "the message does not name the panel: " .. msgs[1][1])
assert(msgs[1][1]:find("exited with code 1", 1, true), "a silent failure does not give its code: " .. msgs[1][1])
io.stdout:write("OK\n")
LUA
check ":EitriPanel with a failing launcher notifies an error" "$SCRATCH/fail.lua"

# A panel that started moves its output to a log, so the pipe ends early; a later failure is reported with the
# last line it wrote before that (where its log is).
cat >"$SCRATCH/late.lua" <<'LUA'
local msgs = {}
vim.notify = function(msg, level) table.insert(msgs, { msg, level }) end
require("eitri").setup({ cmd = vim.env.EITRI_TEST_SCRIPT })
vim.cmd("EitriPanel")
assert(vim.wait(5000, function() return #msgs > 0 end, 20), "no error message after a late failure")
assert(msgs[1][2] == vim.log.levels.ERROR)
assert(msgs[1][1]:find("its output goes to /log/x.log", 1, true), "the log is not named: " .. msgs[1][1])
io.stdout:write("OK\n")
LUA
cat >"$SCRATCH/late-eitri" <<'SH'
#!/bin/sh
echo "eitri panel: running; its output goes to /log/x.log" >&2
exec 2>/dev/null
sleep 0.3
echo "lost" >&2
exit 4
SH
chmod +x "$SCRATCH/late-eitri"
EITRI_TEST_SCRIPT="$SCRATCH/late-eitri" check ":EitriPanel reports a failure after the panel left the pipe" "$SCRATCH/late.lua"

cat >"$SCRATCH/missing.lua" <<'LUA'
local msgs = {}
vim.notify = function(msg, level) table.insert(msgs, { msg, level }) end
require("eitri").setup({ cmd = "/nonexistent/eitri" })
vim.cmd("EitriPanel")
assert(vim.wait(5000, function() return #msgs > 0 end, 20), "no error message for a missing launcher")
assert(msgs[1][2] == vim.log.levels.ERROR)
io.stdout:write("OK\n")
LUA
check ":EitriPanel with a missing launcher notifies an error" "$SCRATCH/missing.lua"

cat >"$SCRATCH/args.lua" <<'LUA'
local out = vim.env.EITRI_TEST_OUT
local script = vim.env.EITRI_TEST_SCRIPT
require("eitri").setup({ cmd = script })
vim.cmd("EitriPanel /some/dir")
assert(vim.wait(5000, function() return vim.fn.filereadable(out) == 1 and #vim.fn.readfile(out) >= 4 end, 20), "launcher not run")
local got = vim.fn.readfile(out)
assert(got[1] == "panel" and got[2] == "--nvim", vim.inspect(got))
assert(got[3] == vim.v.servername, "the address handed over is this nvim's own: " .. got[3] .. " vs " .. vim.v.servername)
assert(got[4] == "--" and got[5] == "/some/dir", vim.inspect(got))
io.stdout:write("OK\n")
LUA
cat >"$SCRATCH/fake-eitri" <<'SH'
#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done >"$EITRI_TEST_OUT.tmp"
mv "$EITRI_TEST_OUT.tmp" "$EITRI_TEST_OUT"
SH
chmod +x "$SCRATCH/fake-eitri"
EITRI_TEST_OUT="$SCRATCH/argv.txt" EITRI_TEST_SCRIPT="$SCRATCH/fake-eitri" check ":EitriPanel runs eitri panel --nvim <its own address> -- <dir>" "$SCRATCH/args.lua"

echo
echo "$CHECKS checks, $FAILURES failed"
[[ "$FAILURES" -eq 0 ]]
