#!/usr/bin/env bash
# Headless checks for nvim/eitri.nvim: the command exists, `edge` is false with no panel attached, and a
# launcher that fails is reported as an error message. Needs a real nvim on PATH; spends nothing and needs no
# display. Standalone: `bash packaging/tests/test_eitri_nvim.sh`.
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN="$(cd "$HERE/../../nvim/eitri.nvim" && pwd)"

command -v nvim >/dev/null || { echo "SKIP - no nvim on PATH"; exit 0; }

# Scratch XDG dirs, never the user's own config or state.
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

# A panel that started moves its output to a log, so the pipe ends early; a later end is a warning (an error on
# display makes nvim pause) naming the exit code and the log, shown from `~` to keep the line short.
cat >"$SCRATCH/late.lua" <<'LUA'
local msgs = {}
vim.notify = function(msg, level) table.insert(msgs, { msg, level }) end
require("eitri").setup({ cmd = vim.env.EITRI_TEST_SCRIPT })
-- Wide enough for the whole line: a narrower nvim names the log by its file alone.
vim.o.columns = 200
vim.cmd("EitriPanel")
assert(vim.wait(5000, function() return #msgs > 0 end, 20), "no message after a late failure")
assert(msgs[1][2] == vim.log.levels.WARN, "a window that ran and ended is not a warning: " .. vim.inspect(msgs))
assert(msgs[1][1] == "eitri panel: window ended (exit 4); log: ~/.local/state/eitri/companion/panel-ab.log",
  "the end is not reported with its code and log: " .. msgs[1][1])
io.stdout:write("OK\n")
LUA
cat >"$SCRATCH/late-eitri" <<'SH'
#!/bin/sh
echo "eitri panel: running; its output goes to $HOME/.local/state/eitri/companion/panel-ab.log" >&2
exec 2>/dev/null
sleep 0.3
echo "lost" >&2
exit 4
SH
chmod +x "$SCRATCH/late-eitri"
EITRI_TEST_SCRIPT="$SCRATCH/late-eitri" check ":EitriPanel reports a window that ran and ended as a warning" "$SCRATCH/late.lua"

# A refusal after the "running;" line is the last line the panel wrote: it stays an error carrying its text.
cat >"$SCRATCH/refused.lua" <<'LUA'
local msgs = {}
vim.notify = function(msg, level) table.insert(msgs, { msg, level }) end
require("eitri").setup({ cmd = vim.env.EITRI_TEST_SCRIPT })
vim.cmd("EitriPanel")
assert(vim.wait(5000, function() return #msgs > 0 end, 20), "no message after a refusal")
assert(msgs[1][2] == vim.log.levels.ERROR, "a refusal is not an error: " .. vim.inspect(msgs))
assert(msgs[1][1] == "eitri panel: the editor refused the panel; closing", "the refusal's text is lost: " .. msgs[1][1])
io.stdout:write("OK\n")
LUA
cat >"$SCRATCH/refused-eitri" <<'SH'
#!/bin/sh
echo "eitri panel: running; its output goes to /log/x.log" >&2
echo "eitri panel: the editor refused the panel; closing" >&2
exit 3
SH
chmod +x "$SCRATCH/refused-eitri"
EITRI_TEST_SCRIPT="$SCRATCH/refused-eitri" check ":EitriPanel reports a refusal after the running line as an error" "$SCRATCH/refused.lua"

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

# Drive a failed page's callbacks without starting a window or binding a socket.
cat >"$SCRATCH/log.lua" <<'LUA'
local uv = vim.uv
local dir = vim.env.EITRI_TEST_LOG_DIR
vim.fn.stdpath = function() return dir end
vim.fn.serverstart = function() return "unused-test-address" end
vim.o.columns = 20
local notices = 0
vim.notify = function() notices = notices + 1 end
local line = "eitri page: " .. string.rep("failure text ", 30)
vim.fn.jobstart = function(_, opts)
  opts.on_stderr(1, { line, "" })
  opts.on_exit(1, 1)
  return 1
end
local function report()
  local before = notices
  require("eitri").page("test")
  assert(vim.wait(1000, function() return notices > before end), "no shortened notice")
end
local path = dir .. "/eitri.log"
vim.fn.mkdir(dir, "p", 448)
local mode = tonumber(vim.env.EITRI_TEST_LOG_MODE)
assert(uv.fs_chmod(dir, mode))
if mode == 448 then
  report()
  assert(uv.fs_stat(path).mode % 512 == 384, "the log is not 0600")
  assert(#vim.fn.readfile(path) == 1 and vim.fn.readfile(path)[1]:find(line, 1, true))
  local inode = uv.fs_stat(path).ino
  assert(uv.fs_chmod(path, 420)) -- an existing 0644 log is replaced privately
  report()
  assert(#vim.fn.readfile(path) == 2, "previous lines were lost")
  assert(uv.fs_stat(path).ino ~= inode, "the log was not replaced atomically")
  assert(uv.fs_stat(path).mode % 512 == 384)
  local real_rename = uv.fs_rename
  uv.fs_rename = function() return nil, "refused" end
  report()
  uv.fs_rename = real_rename
  assert(#vim.fn.readfile(path) == 2, "a failed rename changed the old log")
  assert(#vim.fn.glob(dir .. "/*.tmp", false, true) == 0, "a temporary file was left behind")
  local real_lstat = uv.fs_lstat
  for _, foreign in ipairs({ dir, path }) do
    uv.fs_lstat = function(name)
      local stat = real_lstat(name)
      if name == foreign then stat.uid = uv.getuid() + 1 end
      return stat
    end
    report()
    assert(#vim.fn.readfile(path) == 2, "a foreign directory or file was used")
  end
  uv.fs_lstat = real_lstat
  assert(uv.fs_unlink(path))
  local target = dir .. "/target"
  vim.fn.writefile({ "leave alone" }, target)
  assert(uv.fs_symlink(target, path))
  report()
  assert(uv.fs_lstat(path).type == "link", "a linked log was used")
  assert(vim.fn.readfile(target)[1] == "leave alone")
else
  report()
  assert(uv.fs_lstat(path) == nil, "an accessible directory was used")
end
io.stdout:write("OK\n")
LUA
for mode in 448 488 455 493; do # 0700, 0750, 0707, 0755
  EITRI_TEST_LOG_DIR="$SCRATCH/log-$mode" EITRI_TEST_LOG_MODE="$mode" \
    check "page logs are private and atomic (directory mode $mode)" "$SCRATCH/log.lua"
done

echo
echo "$CHECKS checks, $FAILURES failed"
[[ "$FAILURES" -eq 0 ]]
