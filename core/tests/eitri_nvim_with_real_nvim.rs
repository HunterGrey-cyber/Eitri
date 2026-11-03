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
    let dir = std::env::temp_dir().join(format!("ep-{}-{case}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let scratch = Scratch(dir.clone());
    agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
    let stub = dir.join("eitri");
    agent::private_fs::write_private(
        &stub,
        br##"#!/bin/sh
printf '%s\n' "$@" >> "$ARGV_LOG"
if [ "$1" = --version ]; then echo 'eitri 9.8.7'; exit 0; fi
if [ "$2" = --list ]; then printf 'notes\npreview\n'; exit 0; fi
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
    let mut child = Command::new("nvim")
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
fn the_plugin_file_defines_two_commands_and_one_global() {
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
assert(vim.deep_equal(added, {'EitriPage', 'EitriPanel'}), vim.inspect(added))
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
