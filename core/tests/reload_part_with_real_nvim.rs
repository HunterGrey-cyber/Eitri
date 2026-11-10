#[path = "support/embed_client.rs"]
mod embed_client;
#[path = "support/embed_nvim.rs"]
mod embed_nvim;
#[path = "support/reload_cases.rs"]
mod reload_cases;

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
local cmd = vim.api.nvim_cmd
_G.reload_checks = 0
_G.reload_after_torn = 0
vim.api.nvim_cmd = function(c, options)
  if c.cmd == 'checktime' then
    _G.reload_checks = _G.reload_checks + 1
    if _G.saved_reload_slot and _G.saved_reload_slot.torn then
      _G.reload_after_torn = _G.reload_after_torn + 1
    end
  end
  return cmd(c, options)
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

    /// Open libuv handles, or `None` on nvim 0.10, where `vim.uv.walk` crashes an embedded nvim
    /// (SIGSEGV on 0.10.4) and the counts are not taken.
    fn handles(&self) -> Option<u64> {
        self.nvim
            .exec_lua(
                "if vim.fn.has('nvim-0.11') == 0 then return nil end; local n = 0; vim.uv.walk(function(h) if not h:is_closing() then n = n + 1 end end); return n",
                vec![],
            )
            .as_u64()
    }

    /// Active 1 s timers, or `None` where `handles` gives none.
    fn timers(&self) -> Option<u64> {
        self.nvim
            .exec_lua(
                "if vim.fn.has('nvim-0.11') == 0 then return nil end; local n = 0; vim.uv.walk(function(h) if h:get_type() == 'timer' and h:is_active() and h:get_repeat() == 1000 then n = n + 1 end end); return n",
                vec![],
            )
            .as_u64()
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
    assert_eq!(f.handles(), before.map(|n| n + 1));
    assert!(matches!(f.timers(), None | Some(1)), "one 1 s timer runs while busy");
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
    assert!(matches!(f.timers(), None | Some(1)), "the timer is running");
    assert_eq!(f.checks(), 0, "a modified buffer is not checked");
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
    assert_eq!(f.handles(), before.map(|n| n + 1));
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
    assert_eq!(f.handles(), before.map(|n| n + 1));
    f.nvim
        .exec_lua("reload_part.calls.busy(true); reload_part.calls.busy(false)", vec![]);
    assert_eq!(f.checks(), 0, "always mode ignores busy changes");
    assert_eq!(f.handles(), before.map(|n| n + 1));
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
  if vim.fn.has('nvim-0.11') == 1 then
    local timers = 0
    vim.uv.walk(function(h)
      if h:get_type() == 'timer' and h:is_active() and h:get_repeat() == 1000 then timers = timers + 1 end
    end)
    assert(timers == 1, 'the early install keeps its one timer')
  end
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

/// An nvim with a UI attached (80x24), `f.txt` (`one`, `two`) open, and the reload part loaded with `opts`
/// as `_G.rp`. Under a UI, `checktime` on a modified buffer is a `W12` dialog and on a `noautoread` one a
/// `W11` dialog, each taking the person's next keys.
fn ui_with_part(label: &str, always: bool) -> (embed_client::Embed, PathBuf, PathBuf) {
    ui_with_part_env(label, always, &[])
}

fn ui_with_part_env(label: &str, always: bool, env: &[(&str, &str)]) -> (embed_client::Embed, PathBuf, PathBuf) {
    use embed_client::{opts_map, Embed};

    let scratch = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("rp-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let file = scratch.join("f.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();
    let mut nvim = Embed::start(&scratch, &[], env);
    nvim.wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    nvim.request(
        "nvim_ui_attach",
        vec![
            Value::from(80),
            Value::from(24),
            opts_map(&[("ext_linegrid", Value::from(true))]),
        ],
    );
    nvim.command(&format!("edit {}", file.display()));
    nvim.lua(
        "local src, opts = ...; _G.rp = assert(loadstring(src))(opts)",
        vec![Value::from(reload_source()), opts(always)],
    );
    (nvim, scratch, file)
}

/// `nvim_get_mode`'s mode and whether nvim is blocked waiting for input.
fn ui_mode(nvim: &mut embed_client::Embed) -> (String, bool) {
    let m = nvim.request("nvim_get_mode", vec![]);
    let get = |key: &str| {
        m.as_map()
            .and_then(|entries| entries.iter().find(|(k, _)| k.as_str() == Some(key)))
            .map(|(_, v)| v.clone())
    };
    (
        get("mode")
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        get("blocking").and_then(|v| v.as_bool()).unwrap_or(false),
    )
}

/// A person typing in insert mode in their own terminal nvim while an agent's turn changes the file
/// stays in insert mode: the timer's tick leaves the modified buffer alone, with no dialog.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn typing_while_the_timer_runs_meets_no_dialog() {
    let (mut nvim, scratch, file) = ui_with_part("typ", true);
    nvim.input("ix");
    nvim.wait_until("insert mode", |n| ui_mode(n).0 == "i");
    std::fs::write(&file, "DISK\nchange\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert_eq!(ui_mode(&mut nvim), ("i".to_string(), false), "a dialog met the typing");
    nvim.input("<Esc>");
    nvim.wait_until("normal mode", |n| ui_mode(n).0 == "n");
    assert_eq!(
        nvim.eval("getline(1)").as_str(),
        Some("xone"),
        "the typed text was lost"
    );
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The turn-end and focus reload (`now`) meets a modified buffer the same way: no dialog.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_turn_end_on_a_modified_buffer_meets_no_dialog() {
    let (mut nvim, scratch, file) = ui_with_part("now", false);
    nvim.input("ix");
    nvim.wait_until("insert mode", |n| ui_mode(n).0 == "i");
    std::fs::write(&file, "DISK\nchange\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    nvim.lua("vim.schedule(function() rp.calls.now() end)", vec![]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(ui_mode(&mut nvim), ("i".to_string(), false), "a dialog met the typing");
    nvim.input("<Esc>");
    nvim.wait_until("normal mode", |n| ui_mode(n).0 == "n");
    assert_eq!(
        nvim.eval("getline(1)").as_str(),
        Some("xone"),
        "the typed text was lost"
    );
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// With `autoread` off the person chose no automatic reload: the part leaves such a buffer alone and
/// raises no `W11` dialog, while an unmodified buffer shown beside it still reloads.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_noautoread_buffer_meets_no_dialog_and_is_not_reloaded() {
    let (mut nvim, scratch, file) = ui_with_part("noar", true);
    let other = scratch.join("g.txt");
    std::fs::write(&other, "three\n").unwrap();
    nvim.command("setlocal noautoread");
    nvim.command(&format!("vsplit {}", other.display()));
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&file, "DISK\nchange\n").unwrap();
    std::fs::write(&other, "DISK too\n").unwrap();
    nvim.wait_until("the other buffer's reload", |n| {
        n.eval("getline(1)").as_str() == Some("DISK too")
    });
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert_eq!(ui_mode(&mut nvim), ("n".to_string(), false), "a dialog met the buffer");
    assert_eq!(
        nvim.lua(
            "return vim.api.nvim_buf_get_lines(vim.fn.bufnr(...), 0, 1, false)[1]",
            vec![Value::from(file.to_str().unwrap())]
        )
        .as_str(),
        Some("one"),
        "a noautoread buffer was reloaded"
    );
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// One tick of the part, as the timer, `FocusGained` and a turn end run it: scheduled, so a dialog
/// it raised would block nvim and not the request, then given time to run.
fn tick(nvim: &mut embed_client::Embed) {
    nvim.lua("vim.schedule(function() rp.calls.now() end)", vec![]);
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// A key typed after the tick reaches nvim: `i` puts it in Insert mode. A dialog would take it.
fn the_next_key_reaches_nvim(nvim: &mut embed_client::Embed) {
    assert_eq!(ui_mode(nvim), ("n".to_string(), false), "a dialog met the buffer");
    nvim.input("i");
    nvim.wait_until("insert mode", |n| ui_mode(n).0 == "i");
    nvim.input("<Esc>");
    nvim.wait_until("normal mode", |n| ui_mode(n).0 == "n");
}

fn buffer_line(nvim: &mut embed_client::Embed, file: &std::path::Path) -> Option<String> {
    nvim.lua(
        "return vim.api.nvim_buf_get_lines(vim.fn.bufnr(...), 0, 1, false)[1]",
        vec![Value::from(file.to_str().unwrap())],
    )
    .as_str()
    .map(str::to_string)
}

/// The person opens `:e src/new.rs` for a file that does not exist yet and the agent creates it: nvim
/// answers a check with `W13`, a dialog that takes the next key whatever `autoread` says. A tick that
/// had seen the file missing leaves the buffer alone.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_file_created_after_the_buffer_was_opened_meets_no_dialog() {
    let (mut nvim, scratch, _file) = ui_with_part("w13", false);
    let made = scratch.join("new.rs");
    nvim.command(&format!("edit {}", made.display()));
    tick(&mut nvim);
    std::fs::write(&made, "made by the agent\n").unwrap();
    tick(&mut nvim);
    tick(&mut nvim);
    assert_eq!(
        buffer_line(&mut nvim, &made).as_deref(),
        Some("made by the agent"),
        "the file is read"
    );
    the_next_key_reaches_nvim(&mut nvim);
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The same, with the file created before any tick has looked at the buffer.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_file_created_before_the_first_tick_meets_no_dialog() {
    let (mut nvim, scratch, _file) = ui_with_part("w13first", false);
    let made = scratch.join("new.rs");
    nvim.command(&format!("edit {}", made.display()));
    std::fs::write(&made, "made by the agent\n").unwrap();
    tick(&mut nvim);
    tick(&mut nvim);
    assert_eq!(
        buffer_line(&mut nvim, &made).as_deref(),
        Some("made by the agent"),
        "the file is read"
    );
    the_next_key_reaches_nvim(&mut nvim);
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A buffer opened on a missing path is read once its file appears, and from then on it is a buffer like
/// any other: the next change on disk reaches it, whether the person wrote it empty first or not.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_buffer_opened_on_a_missing_file_is_reloaded_like_any_other_afterwards() {
    let (mut nvim, scratch, _file) = ui_with_part("w13again", false);

    let read = scratch.join("read.rs");
    nvim.command(&format!("edit {}", read.display()));
    tick(&mut nvim);
    std::fs::write(&read, "made\n").unwrap();
    tick(&mut nvim);
    assert_eq!(buffer_line(&mut nvim, &read).as_deref(), Some("made"));
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&read, "DISK\n").unwrap();
    tick(&mut nvim);
    assert_eq!(
        buffer_line(&mut nvim, &read).as_deref(),
        Some("DISK"),
        "read, then reloaded"
    );

    let wrote = scratch.join("wrote.rs");
    nvim.command(&format!("edit {}", wrote.display()));
    tick(&mut nvim);
    nvim.command("write");
    tick(&mut nvim);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&wrote, "DISK\n").unwrap();
    tick(&mut nvim);
    assert_eq!(
        buffer_line(&mut nvim, &wrote).as_deref(),
        Some("DISK"),
        "written, then reloaded"
    );
    the_next_key_reaches_nvim(&mut nvim);
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The ways a buffer comes to be one over a missing file that the part never saw missing, or saw and
/// cannot tell from an ordinary one: a file created empty before the first tick, an empty file that was
/// always there and is filled, a file that is deleted and the buffer read again while it is missing.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn buffers_over_empty_or_vanished_files_meet_no_dialog_and_are_read() {
    let (mut nvim, scratch, file) = ui_with_part("w13empty", false);

    let touched = scratch.join("touched.rs");
    nvim.command(&format!("edit {}", touched.display()));
    std::fs::write(&touched, "").unwrap();
    tick(&mut nvim);
    the_next_key_reaches_nvim(&mut nvim);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&touched, "filled later\n").unwrap();
    tick(&mut nvim);
    assert_eq!(
        buffer_line(&mut nvim, &touched).as_deref(),
        Some("filled later"),
        "created empty, then filled"
    );

    let always = scratch.join("always.rs");
    std::fs::write(&always, "").unwrap();
    nvim.command(&format!("edit {}", always.display()));
    std::fs::write(&always, "filled before the first tick\n").unwrap();
    tick(&mut nvim);
    assert_eq!(
        buffer_line(&mut nvim, &always).as_deref(),
        Some("filled before the first tick")
    );
    the_next_key_reaches_nvim(&mut nvim);

    let reopened = scratch.join("reopened.rs");
    std::fs::write(&reopened, "one\n").unwrap();
    nvim.command(&format!("edit {}", reopened.display()));
    tick(&mut nvim);
    std::fs::remove_file(&reopened).unwrap();
    tick(&mut nvim);
    nvim.command("edit!");
    std::fs::write(&reopened, "back\n").unwrap();
    tick(&mut nvim);
    assert_eq!(
        buffer_line(&mut nvim, &reopened).as_deref(),
        Some("back"),
        "read again while missing, then back"
    );
    the_next_key_reaches_nvim(&mut nvim);
    let _ = file;
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A shown file the agent deletes: nvim answers a check with `E211` and, in Normal mode, waits a second.
/// The length of that wait is measured on the same nvim with a bare `checktime` first.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_deleted_file_does_not_stall_the_tick() {
    let (mut nvim, scratch, file) = ui_with_part("e211", false);
    nvim.command(&format!("cd {}", scratch.display()));
    let other = scratch.join("other.txt");
    std::fs::write(&other, "stock\n").unwrap();
    nvim.command("edit other.txt");
    std::fs::remove_file(&other).unwrap();
    let began = std::time::Instant::now();
    nvim.command("checktime");
    let stall = began.elapsed();
    assert!(
        stall >= std::time::Duration::from_millis(500),
        "calibration: a bare checktime on a deleted file waits ({stall:?})"
    );
    nvim.command("edit f.txt");
    nvim.command("silent! bwipeout! other.txt");
    // A bare check clears nvim's "already warned" flag, which the calibration's warning set and which would
    // turn the next warning into a plain error message.
    nvim.command("checktime");

    std::fs::remove_file(&file).unwrap();
    let began = std::time::Instant::now();
    nvim.lua("rp.calls.now()", vec![]);
    let ticked = began.elapsed();
    assert!(
        ticked < stall / 2,
        "the tick took {ticked:?}, a stalled one takes about {stall:?}"
    );

    std::fs::write(&file, "back\n").unwrap();
    tick(&mut nvim);
    assert_eq!(
        nvim.eval("getline(1)").as_str(),
        Some("back"),
        "a file that returns is reloaded"
    );
    the_next_key_reaches_nvim(&mut nvim);
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The same deletion, where the file's name is long: `E211` does not fit on the command line and nvim asks
/// for a key (hit-enter) after the wait. The calibration shows a bare `checktime` doing that here.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_deleted_file_with_a_long_name_raises_no_prompt() {
    let (mut nvim, scratch, _file) = ui_with_part("e211long", false);
    let dir =
        scratch.join("a-directory-name-long-enough-that-the-deleted-file-warning-needs-more-than-one-screen-line");
    std::fs::create_dir_all(&dir).unwrap();
    let stock = dir.join("stock.txt");
    std::fs::write(&stock, "stock\n").unwrap();
    nvim.command(&format!("edit {}", stock.display()));
    std::fs::remove_file(&stock).unwrap();
    nvim.lua("vim.schedule(function() vim.cmd('checktime') end)", vec![]);
    nvim.wait_until("calibration: a bare checktime on a deleted file asks for a key", |n| {
        ui_mode(n).1
    });
    nvim.input("<CR>");
    nvim.wait_until("normal mode", |n| ui_mode(n) == ("n".to_string(), false));
    nvim.command("silent! bwipeout! stock.txt");
    nvim.command("checktime");

    let file = dir.join("f.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();
    nvim.command(&format!("edit {}", file.display()));
    std::fs::remove_file(&file).unwrap();
    nvim.lua("vim.schedule(function() rp.calls.now() end)", vec![]);
    let until = std::time::Instant::now() + std::time::Duration::from_millis(2500);
    while std::time::Instant::now() < until {
        assert_eq!(
            ui_mode(&mut nvim),
            ("n".to_string(), false),
            "a prompt met the deletion"
        );
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A buffer that is not a file on disk -- unnamed, a `buftype`, a name with a scheme -- is not looked up
/// as a path; a file buffer costs one `fs_stat` a tick.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn only_file_buffers_are_stat_ed_once_a_tick() {
    let (mut nvim, scratch, _file) = ui_with_part("stat", false);
    nvim.lua(
        r#"
_G.stats = {}
local fs_stat = vim.uv.fs_stat
vim.uv.fs_stat = function(path, ...) _G.stats[#_G.stats + 1] = path; return fs_stat(path, ...) end
vim.cmd('split')
vim.cmd('enew')
vim.cmd('split')
vim.cmd('enew')
vim.api.nvim_buf_set_name(0, 'oil:///somewhere/else')
vim.bo.buftype = ''
vim.cmd('split')
vim.cmd('enew')
vim.api.nvim_buf_set_name(0, 'scratch-name-not-a-path')
vim.bo.buftype = 'nofile'
vim.cmd('split')
vim.cmd('terminal')
"#,
        vec![],
    );
    nvim.lua("_G.stats = {}; rp.calls.now()", vec![]);
    let stats = nvim.lua("return vim.json.encode(_G.stats)", vec![]);
    let stats: Vec<String> = serde_json::from_str(stats.as_str().unwrap()).unwrap();
    assert_eq!(stats.len(), 1, "{stats:?}");
    assert!(stats[0].ends_with("f.txt"), "{stats:?}");
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A file that holds one empty line is not a file that holds nothing: the buffer over it is in step with it
/// from the start and its change on disk is a reload like any other.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_file_holding_only_a_newline_is_reloaded() {
    let (mut nvim, scratch, _file) = ui_with_part("nl", false);
    let nl = scratch.join("nl.txt");
    std::fs::write(&nl, "\n").unwrap();
    nvim.command(&format!("edit {}", nl.display()));
    tick(&mut nvim);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&nl, "DISK\n").unwrap();
    tick(&mut nvim);
    assert_eq!(buffer_line(&mut nvim, &nl).as_deref(), Some("DISK"));
    the_next_key_reaches_nvim(&mut nvim);
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Editing a buffer opened on a missing path and undoing it leaves it unmodified and still marked new by
/// nvim: the file that appears then is read, with no dialog, whether or not a tick saw the file missing first.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_edited_and_undone_buffer_meets_no_dialog() {
    let (mut nvim, scratch, _file) = ui_with_part("undo", false);
    let seen = scratch.join("seen.rs");
    nvim.command(&format!("edit {}", seen.display()));
    tick(&mut nvim);
    std::fs::write(&seen, "made by the agent\n").unwrap();
    nvim.command("normal! ix");
    nvim.command("undo");
    assert_eq!(nvim.eval("&modified").as_i64(), Some(0));
    tick(&mut nvim);
    assert_eq!(buffer_line(&mut nvim, &seen).as_deref(), Some("made by the agent"));
    the_next_key_reaches_nvim(&mut nvim);

    let unseen = scratch.join("unseen.rs");
    nvim.command(&format!("edit {}", unseen.display()));
    nvim.command("normal! ix");
    nvim.command("undo");
    std::fs::write(&unseen, "made by the agent\n").unwrap();
    tick(&mut nvim);
    assert_eq!(buffer_line(&mut nvim, &unseen).as_deref(), Some("made by the agent"));
    the_next_key_reaches_nvim(&mut nvim);
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

fn part_case(
    label: &str,
    env: &[(&str, &str)],
    case: fn(&mut embed_client::Embed, &std::path::Path, reload_cases::Tick<'_>),
) {
    let (mut nvim, scratch, _file) = ui_with_part_env(label, false, env);
    case(&mut nvim, &scratch, &mut |n| tick(n));
    nvim.lua("rp.teardown()", vec![]);
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reread_in_a_split_that_is_not_the_current_window() {
    part_case("c-split", &[], reload_cases::a_split_that_is_not_the_current_window);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reread_of_a_file_shown_only_in_a_second_tab_page() {
    part_case("c-tab", &[], reload_cases::a_second_tab_page);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_failed_reread_is_not_followed_by_a_check() {
    part_case(
        "c-cmdwin",
        &[],
        reload_cases::the_command_line_window_open_when_the_file_appears,
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reread_meets_no_swap_file_dialog() {
    part_case(
        "c-swap",
        &[("EMBED_KEEP_SWAP", "1")],
        reload_cases::a_swap_file_left_by_another_nvim,
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_fifo_is_not_read_again() {
    part_case("c-fifo", &[], reload_cases::a_fifo);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn undo_after_a_reread_brings_back_the_empty_buffer() {
    part_case("c-undo", &[], reload_cases::undo_after_a_reread);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reread_leaves_the_directory_options_and_events_alone() {
    part_case("c-narrow", &[], reload_cases::the_persons_directory_options_and_events);
}
