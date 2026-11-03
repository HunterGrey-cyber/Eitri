#[path = "support/embed_nvim.rs"]
mod embed_nvim;

use std::path::PathBuf;
use std::process::Command;

use eitri_core::companion::{install_args, parse_install_report, InstallKind, Sockets, INSTALL_LUA, SLOT_CALL_LUA};
use embed_nvim::EmbedNvim;
use rmpv::Value;

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    &value
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .unwrap()
        .1
}

fn reload_source() -> String {
    let args = install_args(1, &Sockets::default(), &InstallKind::Agent);
    let part = args[1]
        .as_array()
        .unwrap()
        .iter()
        .find(|part| field(part, "name").as_str() == Some("buffer_reload"))
        .unwrap();
    field(part, "src").as_str().unwrap().to_owned()
}

fn opts(always: bool) -> Value {
    Value::Map(vec![(Value::from("always"), Value::from(always))])
}

struct FileBuffer {
    nvim: EmbedNvim,
    dir: PathBuf,
    file: PathBuf,
}

impl FileBuffer {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("rp-{}", uuid::Uuid::new_v4().simple()));
        agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        let file = dir.join("file with spaces.txt");
        agent::private_fs::write_private(&file, b"before\n").unwrap();
        let nvim = EmbedNvim::start();
        nvim.exec_lua(
            r#"
vim.api.nvim_cmd({cmd='edit', args={...}, magic={file=false}}, {})
local cmd = vim.cmd
_G.reload_checks = 0
_G.reload_after_torn = 0
vim.cmd = function(c)
  if c == 'checktime' then
    _G.reload_checks = _G.reload_checks + 1
    if _G.saved_reload_slot and _G.saved_reload_slot.torn then
      _G.reload_after_torn = _G.reload_after_torn + 1
    end
  end
  return cmd(c)
end
"#,
            vec![Value::from(file.to_str().unwrap())],
        );
        Self { nvim, dir, file }
    }

    fn install(&self, always: bool) {
        assert_eq!(self.nvim.exec_lua(
            "local src, opts = ...; _G.reload_part = assert(loadstring(src))(opts); return type(reload_part) == 'table' and type(reload_part.teardown) == 'function' and type(reload_part.calls.busy) == 'function' and type(reload_part.calls.now) == 'function'",
            vec![Value::from(reload_source()), opts(always)],
        ).as_bool(), Some(true), "the reload part exposes teardown, busy and now");
    }

    fn install_slot(&self, channel: u64) {
        let part = Value::Map(vec![
            (Value::from("name"), Value::from("buffer_reload")),
            (Value::from("src"), Value::from(reload_source())),
            (Value::from("opts"), opts(false)),
        ]);
        let report = self.nvim.exec_lua(
            INSTALL_LUA,
            vec![Value::from(channel), Value::Array(vec![part]), Value::Map(vec![])],
        );
        let report = parse_install_report(&report).unwrap();
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.installed, ["buffer_reload"]);
    }

    fn change_on_disk(&self) {
        let status = Command::new("sed")
            .args(["-i", "s/before/changed on disk/"])
            .arg(&self.file)
            .status()
            .unwrap();
        assert!(status.success(), "sed exited with {status}");
    }

    fn first_line(&self) -> String {
        self.nvim
            .exec_lua("return vim.api.nvim_buf_get_lines(0, 0, 1, false)[1]", vec![])
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn checks(&self) -> u64 {
        self.nvim.exec_lua("return reload_checks", vec![]).as_u64().unwrap()
    }

    fn handles(&self) -> u64 {
        self.nvim
            .exec_lua(
                "local n = 0; vim.uv.walk(function(h) if not h:is_closing() then n = n + 1 end end); return n",
                vec![],
            )
            .as_u64()
            .unwrap()
    }

    fn wait(&self, millis: u64) {
        self.nvim.exec_lua("vim.wait(...)", vec![Value::from(millis)]);
    }

    fn wait_for_reload(&self) {
        assert_eq!(self.nvim.exec_lua(
            "return vim.wait(1500, function() return vim.api.nvim_buf_get_lines(0, 0, 1, false)[1] == 'changed on disk' end, 10)",
            vec![],
        ).as_bool(), Some(true), "the timer reloads the displayed buffer");
    }

    fn slot_call(&self, channel: u64, name: &str, args: Vec<Value>) -> Value {
        self.nvim.exec_lua(
            SLOT_CALL_LUA,
            vec![
                Value::from("agent"),
                Value::from(channel),
                Value::from(name),
                Value::Array(args),
            ],
        )
    }
}

impl Drop for FileBuffer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn no_timer_while_idle() {
    let f = FileBuffer::new();
    let before = f.handles();
    f.install(false);
    assert_eq!(f.handles(), before);
    assert_eq!(
        f.nvim
            .exec_lua("return rawget(_G, 'eitri_reload_timer') == nil", vec![])
            .as_bool(),
        Some(true)
    );
    f.change_on_disk();
    f.wait(1200);
    assert_eq!(f.first_line(), "before");
    assert_eq!(f.checks(), 0);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn busy_reloads_a_file_changed_by_sed_within_1_5_s() {
    let f = FileBuffer::new();
    f.install(false);
    let before = f.handles();
    f.nvim.exec_lua("reload_part.calls.busy(true); reload_part.calls.busy(true); collectgarbage('collect'); collectgarbage('collect')", vec![]);
    assert_eq!(f.handles(), before + 1);
    assert_eq!(f.nvim.exec_lua("local n = 0; vim.uv.walk(function(h) if h:get_type() == 'timer' and h:is_active() and h:get_repeat() == 1000 then n = n + 1 end end); return n", vec![]).as_u64(), Some(1));
    f.change_on_disk();
    assert_eq!(f.first_line(), "before");
    f.wait_for_reload();
    assert_eq!(
        f.nvim
            .exec_lua("return rawget(_G, 'eitri_reload_timer') == nil", vec![])
            .as_bool(),
        Some(true)
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_modified_buffer_is_left_alone() {
    let f = FileBuffer::new();
    f.install(false);
    f.nvim.exec_lua(
        "vim.api.nvim_buf_set_lines(0, 0, -1, false, {'unsaved edit'}); reload_part.calls.busy(true)",
        vec![],
    );
    f.change_on_disk();
    f.wait(1200);
    assert!(f.checks() > 0);
    assert_eq!(f.first_line(), "unsaved edit");
    assert_eq!(f.nvim.exec_lua("return vim.bo.modified", vec![]).as_bool(), Some(true));
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn now_reloads_at_once() {
    let f = FileBuffer::new();
    f.install(false);
    let before = f.handles();
    f.change_on_disk();
    f.nvim.exec_lua("reload_part.calls.now()", vec![]);
    assert_eq!(f.first_line(), "changed on disk");
    assert_eq!(f.checks(), 1);
    assert_eq!(f.handles(), before);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn busy_off_reloads_once_then_closes_the_timer() {
    let f = FileBuffer::new();
    f.install(false);
    let before = f.handles();
    f.nvim.exec_lua("reload_part.calls.busy(true)", vec![]);
    assert_eq!(f.handles(), before + 1);
    f.change_on_disk();
    let checks = f.checks();
    f.nvim.exec_lua("reload_part.calls.busy(false)", vec![]);
    assert_eq!(f.first_line(), "changed on disk");
    assert_eq!(f.checks(), checks + 1);
    assert_eq!(f.handles(), before);
    f.wait(1200);
    assert_eq!(f.checks(), checks + 1);
    agent::private_fs::write_private(&f.file, b"after restarting\n").unwrap();
    f.nvim.exec_lua("reload_part.calls.busy(true)", vec![]);
    f.wait(1200);
    assert_eq!(f.first_line(), "after restarting");
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_closed_channel_while_busy_reloads_through_the_teardown() {
    let f = FileBuffer::new();
    let chan = f.nvim.helper_channel();
    let before = f.handles();
    f.install_slot(chan);
    f.slot_call(chan, "busy", vec![Value::from(true)]);
    f.nvim.exec_lua("_G.saved_reload_slot = __eitri_companion", vec![]);
    f.change_on_disk();
    f.nvim.close_channel(chan);
    f.nvim.exec_lua(
        "assert(vim.wait(1000, function() return __eitri_companion == nil end, 10))",
        vec![],
    );
    f.wait(20);
    assert_eq!(f.first_line(), "changed on disk");
    assert_eq!(
        f.nvim
            .exec_lua(
                "return saved_reload_slot.torn and saved_reload_slot.timer:is_closing()",
                vec![]
            )
            .as_bool(),
        Some(true)
    );
    let checks = f.checks();
    assert!(checks > 0);
    assert_eq!(f.nvim.exec_lua("return reload_after_torn", vec![]).as_u64(), Some(1));
    f.wait(1200);
    assert_eq!(f.checks(), checks);
    assert_eq!(f.handles(), before, "both Lua timers have closed");
    assert_eq!(f.slot_call(chan, "now", vec![]).as_bool(), Some(false));
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn focus_gained_reloads() {
    let f = FileBuffer::new();
    f.install(false);
    f.change_on_disk();
    f.nvim.exec_lua("vim.api.nvim_exec_autocmds('FocusGained', {})", vec![]);
    assert_eq!(f.first_line(), "changed on disk");
    assert_eq!(f.checks(), 1);
    agent::private_fs::write_private(&f.file, b"after another focus\n").unwrap();
    f.nvim.exec_lua("vim.api.nvim_exec_autocmds('FocusGained', {})", vec![]);
    assert_eq!(f.first_line(), "after another focus");
    assert_eq!(f.checks(), 2);
    f.nvim
        .exec_lua("reload_part.teardown(); reload_part.teardown()", vec![]);
    f.wait(20);
    assert_eq!(f.checks(), 3, "teardown schedules one final check");
    f.nvim.exec_lua(
        "vim.api.nvim_exec_autocmds('FocusGained', {}); reload_part.calls.now(); reload_part.calls.busy(true)",
        vec![],
    );
    f.wait(1200);
    assert_eq!(f.checks(), 3, "torn callbacks and calls do nothing");
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn always_keeps_the_timer_on_through_busy_off() {
    let f = FileBuffer::new();
    let before = f.handles();
    f.install(true);
    assert_eq!(f.handles(), before + 1);
    f.nvim
        .exec_lua("reload_part.calls.busy(true); reload_part.calls.busy(false)", vec![]);
    assert_eq!(f.checks(), 0, "always mode ignores busy changes");
    assert_eq!(f.handles(), before + 1);
    f.change_on_disk();
    f.wait_for_reload();
    f.nvim.exec_lua("reload_part.teardown()", vec![]);
    f.wait(20);
    assert_eq!(f.handles(), before);
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn a_slot_call_from_another_channel_does_nothing() {
    let f = FileBuffer::new();
    let chan = f.nvim.helper_channel();
    let other = f.nvim.helper_channel();
    f.install_slot(chan);
    let before = f.handles();
    f.change_on_disk();
    assert_eq!(
        f.slot_call(other, "busy", vec![Value::from(true)]).as_bool(),
        Some(false)
    );
    assert_eq!(f.slot_call(other, "now", vec![]).as_bool(), Some(false));
    assert_eq!(f.handles(), before);
    f.wait(1200);
    assert_eq!(f.checks(), 0);
    assert_eq!(f.first_line(), "before");
    f.slot_call(chan, "now", vec![]);
    assert_eq!(f.first_line(), "changed on disk");
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn queued_ticks_do_nothing_after_stopping_or_teardown() {
    let f = FileBuffer::new();
    f.install(false);
    f.nvim.exec_lua(
        r#"
local schedule = vim.schedule
local queued = {}
vim.schedule = function(fn) queued[#queued + 1] = fn end
reload_part.calls.busy(true)
assert(vim.wait(1500, function() return #queued > 0 end, 10))
vim.schedule = schedule
reload_part.calls.busy(false)
reload_part.calls.busy(true)
local checks = reload_checks
for _, fn in ipairs(queued) do fn() end
assert(reload_checks == checks, 'a tick from the stopped timer does nothing after restart')

queued = {}
vim.schedule = function(fn) queued[#queued + 1] = fn end
assert(vim.wait(1500, function() return #queued > 0 end, 10))
reload_part.teardown()
vim.schedule = schedule
assert(#queued >= 2, 'a timer tick and the final reload are queued')
checks = reload_checks
for _, fn in ipairs(queued) do fn() end
assert(reload_checks == checks + 1, 'only the final teardown check runs after torn')
"#,
        vec![],
    );
}

#[test]
#[ignore = "needs nvim on PATH; uses stdio and no display"]
fn an_install_before_vim_enter_keeps_working_after_startup() {
    let f = FileBuffer::new();
    agent::private_fs::write_private(&f.dir.join("reload_source.lua"), reload_source().as_bytes()).unwrap();
    assert_eq!(
        f.nvim.exec_lua(
            r#"
local dir = ...
local chan = vim.fn.jobstart({vim.v.progpath, '--clean', '--headless', '--embed', '-n', '-i', 'NONE',
  '--cmd', [[lua assert(vim.v.vim_did_enter == 0); _G.reload_part = assert(loadstring(table.concat(vim.fn.readfile('reload_source.lua'), '\n')))({always=true})]]
}, {rpc=true, cwd=dir})
assert(chan > 0, 'a helper starts over stdio')
local result = vim.rpcrequest(chan, 'nvim_exec_lua', [[
  assert(vim.v.vim_did_enter == 1, 'startup has completed')
  reload_part.calls.busy(false)
  local timers = 0
  vim.uv.walk(function(h)
    if h:get_type() == 'timer' and h:is_active() and h:get_repeat() == 1000 then timers = timers + 1 end
  end)
  assert(timers == 1, 'the early install keeps its one timer')
  assert(_G.eitri_reload_timer == nil, 'the timer belongs only to the part')
  assert(#vim.api.nvim_get_autocmds({group='EitriBufferReload', event='FocusGained'}) == 1)
  reload_part.teardown()
  return true
]], {})
vim.fn.jobstop(chan)
return result
"#,
            vec![Value::from(f.dir.to_str().unwrap())],
        ).as_bool(),
        Some(true)
    );
}
