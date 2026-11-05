//! The optional nvim launcher against a real headless nvim and an argv-recording program.
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Removes the case's directory however the case ends, a timeout or a failed assertion included.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn check(case: &str, body: &str) {
    check_with_nvim(case, body, std::ffi::OsStr::new("nvim"));
}

fn check_with_nvim(case: &str, body: &str, nvim: &std::ffi::OsStr) {
    let dir = std::env::temp_dir().join(format!("ep-{}-{case}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let scratch = Scratch(dir.clone());
    agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
    let stub = dir.join("eitri");
    agent::private_fs::write_private(
        &stub,
        br##"#!/bin/sh
printf '%s\n' "$@" >> "$ARGV_LOG"
if [ -n "$STDOUT_LOG" ]; then readlink "/proc/$$/fd/1" >> "$STDOUT_LOG"; fi
if [ -n "$REFUSED_AFTER" ]; then
  echo "eitri $1: running; its output goes to /x/$1.log" >&2
  echo "eitri $1: $REFUSED_AFTER" >&2
  exit 3
fi
if [ -n "$RUNNING_THEN" ]; then echo "eitri $1: running; its output goes to ${LOG_DIR:-/x}/$1.log" >&2; exit "$RUNNING_THEN"; fi
if [ "$1" = --version ]; then echo 'eitri 9.8.7'; exit 0; fi
if [ "$2" = --list ]; then printf 'notes\npreview\n'; exit 0; fi
if [ -n "$OPEN_REFUSAL" ]; then echo "earlier diagnostic" >&2; echo "$OPEN_REFUSAL" >&2; exit 64; fi
if [ "$OLD_EITRI" = yes ]; then echo 'eitri: unknown option --nvim' >&2; exit 64; fi
"##,
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let plugin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("nvim/eitri.nvim");
    let socket = agent::socket_path::in_dir(&dir, "n.sock").unwrap();
    let script = format!(
        r#"local ok, err = pcall(function()
vim.opt.rtp:prepend({plugin})
assert(vim.fn.serverstart({socket}) ~= '')
local messages = {{}}
vim.notify = function(text) table.insert(messages, text) end
local function said(text)
  assert(vim.wait(3000, function() return table.concat(messages, '\n'):find(text, 1, true) ~= nil end),
    vim.inspect(messages))
end
local function argv()
  return vim.fn.filereadable(vim.env.ARGV_LOG) == 1 and vim.fn.readfile(vim.env.ARGV_LOG) or {{}}
end
local jobs = 0
local real_jobstart = vim.fn.jobstart
vim.fn.jobstart = function(...) jobs = jobs + 1; return real_jobstart(...) end
local MODES = {{'n', 'i', 'v', 'x', 's', 'o', 'c', 't', 'l'}}
local function all_maps()
  local maps = {{}}
  for _, mode in ipairs(MODES) do maps[mode] = vim.api.nvim_get_keymap(mode) end
  return maps
end
local globals_before = vim.fn.eval('copy(g:)')
local maps_before = all_maps()
local commands_before = vim.api.nvim_get_commands({{}})
local autocmds_before = vim.api.nvim_get_autocmds({{}})
vim.cmd('runtime plugin/eitri.lua')
{body}
end)
if not ok then io.stderr:write(tostring(err), '\n'); vim.cmd('cquit 1') else vim.cmd('qa!') end
"#,
        plugin = serde_json::to_string(plugin.to_str().unwrap()).unwrap(),
        socket = serde_json::to_string(socket.to_str().unwrap()).unwrap(),
    );
    let script_path = dir.join("check.lua");
    agent::private_fs::write_private(&script_path, script.as_bytes()).unwrap();
    let mut child = Command::new(nvim)
        .args(["--headless", "-u", "NONE", "-i", "NONE", "-n", "-l"])
        .arg(&script_path)
        .current_dir(&dir)
        .env("PATH", format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()))
        .env("ARGV_LOG", dir.join("argv"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env_remove("OLD_EITRI")
        .env_remove("OPEN_REFUSAL")
        .env_remove("NVIM")
        .env_remove("NVIM_LISTEN_ADDRESS")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("nvim must be on PATH");
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("nvim did not finish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    drop(scratch);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
#[ignore = "needs a real nvim with serverstart() socket access"]
fn eitri_page_registers_and_says_so_without_an_id() {
    check(
        "zero",
        r#"
assert(vim.api.nvim_get_commands({}).EitriPage.nargs == '*')
vim.cmd('EitriPage')
said(':EitriPage needs a plugin id, for example :EitriPage markdown')
assert(jobs == 0 and #argv() == 0)
"#,
    );
}

#[test]
#[ignore = "needs a real nvim with serverstart() socket access"]
fn eitri_page_with_three_arguments_says_so() {
    check(
        "three",
        r#"
vim.cmd('EitriPage notes one two')
said(':EitriPage takes a plugin id and an optional directory')
assert(jobs == 0 and #argv() == 0)
"#,
    );
}

#[test]
#[ignore = "needs a real nvim with serverstart() socket access"]
fn eitri_page_runs_the_stub_with_the_exact_argv() {
    check(
        "argv",
        r#"
local start = vim.fn.jobstart
vim.fn.jobstart = function(args, opts) assert(opts.detach == true); return start(args, opts) end
vim.cmd([[EitriPage notes a\ directory]])
assert(vim.wait(3000, function() return #argv() == 6 end))
assert(vim.deep_equal(argv(), {'page', 'notes', '--nvim', vim.v.servername, '--', 'a directory'}), vim.inspect(argv()))
vim.fn.delete(vim.env.ARGV_LOG)
require('eitri').setup({page_mappings = {['<F8>'] = 'preview'}})
local map = vim.fn.maparg('<F8>', 'n', false, true)
assert(type(map.callback) == 'function')
map.callback()
assert(vim.wait(3000, function() return #argv() == 6 end))
assert(vim.deep_equal(argv(), {'page', 'preview', '--nvim', vim.v.servername, '--', vim.fn.getcwd()}))
vim.fn.delete(vim.env.ARGV_LOG)
for _, address in ipairs(vim.fn.serverlist()) do vim.fn.serverstop(address) end
assert(vim.v.servername == '')
require('eitri').page('notes')
assert(vim.v.servername ~= '')
assert(vim.wait(3000, function() return #argv() == 6 end))
assert(argv()[4] == vim.v.servername)
"#,
    );
}

/// A window moves only a pipe into its log (a file or `/dev/null` is left alone), and a detached job with no
/// `on_stdout` gets `/dev/null` for stdout: both the page and the panel are started with a stdout pipe, so
/// what the window prints reaches its log too.
#[test]
#[ignore = "needs a real nvim with serverstart() socket access"]
fn the_page_and_the_panel_start_with_a_stdout_pipe() {
    check(
        "stdout",
        r#"
vim.env.STDOUT_LOG = vim.fn.getcwd() .. '/stdout'
local function links()
  return vim.fn.filereadable(vim.env.STDOUT_LOG) == 1 and vim.fn.readfile(vim.env.STDOUT_LOG) or {}
end
require('eitri').page('notes')
assert(vim.wait(3000, function() return #links() == 1 end), vim.inspect(links()))
require('eitri').panel()
assert(vim.wait(3000, function() return #links() == 2 end), vim.inspect(links()))
for _, link in ipairs(links()) do
  -- nvim 0.12 hands the job a pipe, 0.10 and 0.11 a socket pair; the window moves either off itself.
  assert(link:match('^pipe:') or link:match('^socket:'), 'stdout was ' .. link)
end
"#,
    );
}

/// A window that started and later ended (a crash, a kill) is news, not an error: an error message on display makes
/// nvim pause about a second at its next redraw, in the middle of whatever the person is typing.
#[test]
#[ignore = "needs a real nvim with serverstart() socket access"]
fn a_window_that_ran_and_ended_is_a_warning_naming_its_log() {
    check(
        "ended",
        r#"
local seen = {}
vim.notify = function(text, level) table.insert(seen, {text = text, level = level}) end
vim.env.RUNNING_THEN = '137'
require('eitri').page('notes')
assert(vim.wait(3000, function() return #seen == 1 end), vim.inspect(seen))
require('eitri').panel()
assert(vim.wait(3000, function() return #seen == 2 end), vim.inspect(seen))
assert(vim.deep_equal(seen, {
  {text = 'eitri page: window ended (exit 137); log: /x/page.log', level = vim.log.levels.WARN},
  {text = 'eitri panel: window ended (exit 137); log: /x/panel.log', level = vim.log.levels.WARN},
}), vim.inspect(seen))
"#,
    );
}

/// The warning is one short line: the log path is shown from `~` where the line has room for it (a narrower nvim
/// gets the log's file name alone, `eitri_open_end_with_real_nvim.rs`).
#[test]
#[ignore = "needs a real nvim with serverstart() socket access"]
fn the_warning_shows_the_log_from_the_home_directory() {
    check(
        "home",
        r#"
local seen = {}
vim.notify = function(text, level) table.insert(seen, {text = text, level = level}) end
vim.env.RUNNING_THEN = '4'
vim.env.LOG_DIR = vim.env.HOME .. '/.local/state/eitri/companion'
vim.o.columns = 200
require('eitri').page('notes')
assert(vim.wait(3000, function() return #seen == 1 end), vim.inspect(seen))
assert(vim.deep_equal(seen, {
  {text = 'eitri page: window ended (exit 4); log: ~/.local/state/eitri/companion/page.log', level = vim.log.levels.WARN},
}), vim.inspect(seen))
"#,
    );
}

/// What the window says after its "running;" line is the last line it wrote: a refusal found once it was up stays
/// an error carrying that text, for the page and the panel alike.
#[test]
#[ignore = "needs a real nvim with serverstart() socket access"]
fn a_refusal_after_the_running_line_stays_an_error_with_its_text() {
    check(
        "refused",
        r#"
local seen = {}
vim.notify = function(text, level) table.insert(seen, {text = text, level = level}) end
vim.env.REFUSED_AFTER = 'the editor refused the follow snippet; closing'
require('eitri').page('notes')
assert(vim.wait(3000, function() return #seen == 1 end), vim.inspect(seen))
require('eitri').panel()
assert(vim.wait(3000, function() return #seen == 2 end), vim.inspect(seen))
assert(vim.deep_equal(seen, {
  {text = 'eitri page: the editor refused the follow snippet; closing', level = vim.log.levels.ERROR},
  {text = 'eitri panel: the editor refused the follow snippet; closing', level = vim.log.levels.ERROR},
}), vim.inspect(seen))
"#,
    );
}

#[test]
#[ignore = "needs a real nvim with serverstart() socket access"]
fn an_older_eitri_gets_the_update_message() {
    check(
        "old",
        r#"
vim.env.OLD_EITRI = 'yes'
require('eitri').page('notes')
said('eitri page: this Eitri has no page windows yet; update Eitri')
"#,
    );
}

#[test]
#[ignore = "needs a real nvim with serverstart() socket access"]
fn inside_eitris_own_nvim_it_refuses_and_runs_nothing() {
    check(
        "own",
        r#"
_G.__eitri_window = {}
vim.cmd('EitriPage notes')
said('this window opens pages itself: press the prefix and the page\'s key')
assert(jobs == 0 and #argv() == 0)
"#,
    );
}

#[test]
#[ignore = "needs a real nvim with serverstart() socket access"]
fn the_plugin_file_defines_three_commands_and_one_global() {
    check(
        "load",
        r#"
assert(package.loaded.eitri == nil and package.loaded['eitri.health'] == nil)
local commands_after = vim.api.nvim_get_commands({})
local added = {}
for name in pairs(commands_after) do
  if commands_before[name] == nil then table.insert(added, name) end
end
table.sort(added)
assert(vim.deep_equal(added, {'EitriOpen', 'EitriPage', 'EitriPanel'}), vim.inspect(added))
for name in pairs(commands_before) do assert(commands_after[name] ~= nil, name) end
assert(vim.g.loaded_eitri == 1)
local globals_after = vim.fn.eval('copy(g:)')
globals_after.loaded_eitri = nil
assert(vim.deep_equal(globals_before, globals_after), vim.inspect(globals_after))
assert(vim.deep_equal(maps_before, all_maps()))
assert(#vim.fn.timer_info() == 0)
assert(vim.deep_equal(autocmds_before, vim.api.nvim_get_autocmds({})))
vim.cmd('runtime plugin/eitri.lua')
assert(jobs == 0 and #argv() == 0)
"#,
    );
}

#[test]
#[ignore = "needs a real nvim with serverstart() socket access"]
fn checkhealth_names_the_program_and_its_page_ids() {
    check(
        "health",
        r#"
require('eitri').setup({cmd = vim.fn.exepath('eitri')})
vim.cmd('checkhealth eitri')
local text = table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), '\n')
for _, word in ipairs({vim.fn.exepath('eitri'), 'eitri 9.8.7', 'notes', 'preview', 'until a command'}) do
  assert(text:find(word, 1, true), text)
end
assert(vim.deep_equal(argv(), {'--version', 'page', '--list'}), vim.inspect(argv()))
local eitri = require('eitri')
local function complete(lead, line) return eitri.complete_pages(lead, line, #line) end
assert(vim.deep_equal(complete('n', 'EitriPage n'), {'notes'}))
assert(vim.deep_equal(complete('p', 'EitriPage p'), {'preview'}))
assert(#argv() == 5, vim.inspect(argv()))
vim.fn.mkdir('a-dir')
local dirs = complete('', 'EitriPage notes ')
assert(vim.tbl_contains(dirs, 'a-dir/'), vim.inspect(dirs))
assert(not vim.tbl_contains(dirs, 'notes'), vim.inspect(dirs))
assert(vim.tbl_contains(complete('a', 'EitriPage notes a'), 'a-dir/'))
assert(#argv() == 5, vim.inspect(argv()))
vim.cmd('doautocmd CmdlineLeave')
complete('', 'EitriPage ')
assert(#argv() == 7, vim.inspect(argv()))
eitri.teardown()
assert(#complete('', 'EitriPage ') == 0)
"#,
    );
}

#[test]
#[ignore = "needs real nvim 0.11.2 with serverstart() socket access"]
fn eitri_open_below_0_12_prints_the_line_and_runs_nothing() {
    let nvim = std::env::var_os("EITRI_TEST_NVIM_0_11").unwrap_or_else(|| {
        let target = std::env::var_os("CARGO_TARGET_DIR").expect("CARGO_TARGET_DIR must name the release cache");
        let dest = std::path::PathBuf::from(target).join("nvim-releases/0.11.2");
        let output = Command::new("bash")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../tools/nvim-releases/fetch.sh"))
            .arg("0.11.2")
            .arg(dest)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
            .join("nvim")
            .into_os_string()
    });
    check_with_nvim(
        "takeover-old",
        r#"
assert(vim.fn.has('nvim-0.12') == 0)
vim.cmd('EitriOpen')
said("Eitri's takeover needs nvim 0.12 or later (this is 0.11.2)")
assert(jobs == 0 and #argv() == 0)
"#,
        &nvim,
    );
}

#[test]
#[ignore = "needs a real nvim 0.12 with serverstart() socket access"]
fn eitri_open_reports_a_refusal_line() {
    check(
        "takeover-refusal",
        r#"
vim.env.OPEN_REFUSAL = 'takeover is off (editor.takeover in init.lua)'
local start = vim.fn.jobstart
vim.fn.jobstart = function(args, opts) assert(opts.detach == true); return start(args, opts) end
vim.cmd('EitriOpen a directory')
said('takeover is off (editor.takeover in init.lua)')
assert(vim.deep_equal(argv(), {'open', '--nvim', vim.v.servername, '--', 'a directory'}), vim.inspect(argv()))
assert(jobs == 1)
require('eitri').open()
said('takeover is off (editor.takeover in init.lua)')
assert(vim.wait(3000, function() return #argv() == 8 end))
assert(argv()[6] == 'open' and argv()[7] == '--nvim' and argv()[8] == vim.v.servername)
"#,
    );
}
