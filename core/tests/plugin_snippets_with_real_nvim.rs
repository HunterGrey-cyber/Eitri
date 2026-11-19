//! The plugin host's editor snippets in a real `nvim --clean --embed --headless`: assist applies and
//! clears exactly, keeps the first baseline, leaves a value the user changed, re-asserts after an
//! `LspAttach` that turns hints on, and reports a missing API; open moves the cursor by characters and
//! splits over a modified buffer; cwd restores tab baselines and leaves user changes alone; follow defines and
//! removes only its own commands, reports only the buffers it may follow, scrolls only the followed window, holds a
//! reveal's cursor without scrolling under 'scrolloff' and 'wrap', and refuses text past its caps.
//!
//! Every case is `#[ignore]`d: it needs `nvim` (>= 0.11) on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eitri_core::plugin::editor::{PLUGIN_ASSIST_LUA, PLUGIN_CWD_LUA, PLUGIN_FOLLOW_LUA, PLUGIN_OPEN_LUA};
use embed_client::{opts_map, Embed};
use rmpv::Value;

struct Case {
    dir: PathBuf,
    root: PathBuf,
    a: PathBuf,
    b: PathBuf,
    outside: PathBuf,
    nvim: Embed,
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn case(name: &str) -> Case {
    case_with(name, &[])
}

/// [`case`], with `args` added to nvim's command line.
fn case_with(name: &str, args: &[&str]) -> Case {
    let dir = std::env::temp_dir().join(format!("pas-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("work/ex")).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    let root = dir.join("work/ex");
    let a = root.join("a.rs");
    let b = root.join("b.rs");
    let outside = dir.join("outside.rs");
    std::fs::write(&a, "fn main() {\nlet s = \"\u{4e2d}\u{6587}\"; x\n}\n").unwrap();
    std::fs::write(&b, "one\ntwo\n").unwrap();
    std::fs::write(&outside, "out\n").unwrap();
    let mut nvim = Embed::start(&dir, args, &[]);
    nvim.manual_timers();
    Case {
        dir,
        root,
        a,
        b,
        outside,
        nvim,
    }
}

fn s(path: &Path) -> Value {
    Value::from(path.to_str().unwrap())
}

fn edit(c: &mut Case, path: &Path) {
    c.nvim.command(&format!("edit {}", path.display()));
}

/// `(diagnostics enabled, inlay hints enabled)` for the buffer of `path`.
fn state(c: &mut Case, path: &Path) -> (bool, bool) {
    let v = c.nvim.lua(
        "local b = vim.fn.bufnr(...) return { vim.diagnostic.is_enabled({ bufnr = b }), vim.lsp.inlay_hint.is_enabled({ bufnr = b }) }",
        vec![s(path)],
    );
    let a = v.as_array().unwrap();
    (a[0].as_bool().unwrap(), a[1].as_bool().unwrap())
}

fn set(c: &mut Case, path: &Path, diagnostics: bool, hints: bool) {
    c.nvim.lua(
        "local p, d, h = ... local b = vim.fn.bufnr(p) vim.diagnostic.enable(d, { bufnr = b }) vim.lsp.inlay_hint.enable(h, { bufnr = b })",
        vec![s(path), diagnostics.into(), hints.into()],
    );
}

/// Starts an in-process server that only answers initialize and shutdown, attached to the buffer of `path`,
/// and waits until the client is there.
fn attach(c: &mut Case, path: &Path) {
    c.nvim.lua(
        "local id = 0 \
         local function fake(_) local closing = false return { \
           request = function(method, _, callback) id = id + 1 \
             if method == 'initialize' then vim.schedule(function() callback(nil, { capabilities = { inlayHintProvider = true } }) end) \
             elseif method == 'shutdown' then vim.schedule(function() callback(nil, nil) end) end \
             return true, id end, \
           notify = function(method) if method == 'exit' then closing = true end return true end, \
           is_closing = function() return closing end, \
           terminate = function() closing = true end } end \
         vim.lsp.start({ name = 'fake', cmd = fake, root_dir = vim.fs.dirname(...) }, { bufnr = vim.fn.bufnr(...) })",
        vec![s(path)],
    );
    c.nvim.lua(
        "local b = vim.fn.bufnr(...) vim.wait(2000, function() return #vim.lsp.get_clients({ bufnr = b }) > 0 end)",
        vec![s(path)],
    );
}

fn apply(c: &mut Case, owner: &str, diagnostics: bool, hints: bool) -> Value {
    let opts = opts_map(&[
        ("owner", owner.into()),
        ("root", s(&c.root.clone())),
        ("diagnostics", diagnostics.into()),
        ("inlay_hints", hints.into()),
    ]);
    c.nvim.lua(PLUGIN_ASSIST_LUA, vec!["apply".into(), opts])
}

fn clear(c: &mut Case, owner: &str) -> (u64, u64) {
    let v = c.nvim.lua(
        PLUGIN_ASSIST_LUA,
        vec!["clear".into(), opts_map(&[("owner", owner.into())])],
    );
    let get = |k: &str| {
        v.as_map()
            .unwrap()
            .iter()
            .find(|(x, _)| x.as_str() == Some(k))
            .unwrap()
            .1
            .as_u64()
            .unwrap()
    };
    (get("restored"), get("left_alone"))
}

/// An assist call that says when it was made: `maker` and `order`, as the host sends them.
fn made(c: &mut Case, op: &str, owner: &str, diagnostics: bool, hints: bool, maker: &str, order: u64) -> Value {
    let mut pairs = vec![
        ("owner", owner.into()),
        ("maker", maker.into()),
        ("order", order.into()),
    ];
    if op == "apply" {
        pairs.push(("root", s(&c.root.clone())));
        pairs.push(("diagnostics", diagnostics.into()));
        pairs.push(("inlay_hints", hints.into()));
    }
    c.nvim.lua(PLUGIN_ASSIST_LUA, vec![op.into(), opts_map(&pairs)])
}

fn field<'a>(v: &'a Value, k: &str) -> &'a Value {
    &v.as_map()
        .unwrap()
        .iter()
        .find(|(x, _)| x.as_str() == Some(k))
        .unwrap()
        .1
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn assist_applies_then_clears_exactly_and_leaves_buffers_outside_alone() {
    let mut c = case("exact");
    let (a, outside) = (c.a.clone(), c.outside.clone());
    edit(&mut c, &outside);
    edit(&mut c, &a);
    attach(&mut c, &a);
    let before_a = state(&mut c, &a);
    let before_out = state(&mut c, &outside);
    let applied = apply(&mut c, "demo", false, false);
    assert_eq!(field(&applied, "buffers").as_u64(), Some(1));
    assert_eq!(field(&applied, "partial").as_bool(), Some(false));
    assert_eq!(state(&mut c, &a), (false, false));
    assert_eq!(state(&mut c, &outside), before_out, "outside the root is untouched");
    assert_eq!(clear(&mut c, "demo"), (2, 0));
    assert_eq!(state(&mut c, &a), before_a);
    assert_eq!(state(&mut c, &outside), before_out);
    let gone = c.nvim.lua(
        "return pcall(vim.api.nvim_get_autocmds, { group = 'EitriPluginAssist_demo' })",
        vec![],
    );
    assert_eq!(gone, Value::from(false), "the augroup is deleted");
}

/// Two calls of one window can reach nvim swapped; the one made last must be what stays in force, so a stop's
/// clear that overtook its apply leaves nothing set behind it.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_assist_call_overtaken_by_a_later_one_changes_nothing() {
    let mut c = case("order");
    let a = c.a.clone();
    edit(&mut c, &a);
    attach(&mut c, &a);
    set(&mut c, &a, true, true);
    // The clear made second arrives first, then the apply made before it.
    let cleared = made(&mut c, "clear", "demo", false, false, "m", 2);
    assert_eq!(field(&cleared, "restored").as_u64(), Some(0));
    let late = made(&mut c, "apply", "demo", false, false, "m", 1);
    assert_eq!(field(&late, "skipped").as_bool(), Some(true));
    assert_eq!(field(&late, "buffers").as_u64(), Some(0));
    assert_eq!(state(&mut c, &a), (true, true), "the late apply set nothing");
    let gone = c.nvim.lua(
        "return pcall(vim.api.nvim_get_autocmds, { group = 'EitriPluginAssist_demo' })",
        vec![],
    );
    assert_eq!(gone, Value::from(false), "the late apply left no augroup");
    // Two applies swapped: the later one's values stay.
    made(&mut c, "apply", "demo", false, true, "m", 4);
    let older = made(&mut c, "apply", "demo", true, false, "m", 3);
    assert_eq!(field(&older, "skipped").as_bool(), Some(true));
    assert_eq!(state(&mut c, &a), (false, true));
    // Another process counts on its own: its calls run whatever its numbers.
    let other = made(&mut c, "clear", "demo", false, false, "n", 1);
    assert_eq!(field(&other, "restored").as_u64(), Some(2), "both settings put back");
    assert_eq!(state(&mut c, &a), (true, true));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn raising_a_setting_keeps_the_first_baseline() {
    let mut c = case("raise");
    let a = c.a.clone();
    edit(&mut c, &a);
    attach(&mut c, &a);
    set(&mut c, &a, true, true);
    apply(&mut c, "demo", false, false);
    apply(&mut c, "demo", true, false);
    assert_eq!(state(&mut c, &a), (true, false));
    clear(&mut c, "demo");
    assert_eq!(
        state(&mut c, &a),
        (true, true),
        "the baseline from before the first apply"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_value_the_user_changed_is_left_alone() {
    let mut c = case("user");
    let a = c.a.clone();
    edit(&mut c, &a);
    attach(&mut c, &a);
    set(&mut c, &a, true, false);
    apply(&mut c, "demo", false, false);
    set(&mut c, &a, true, false);
    assert_eq!(
        clear(&mut c, "demo"),
        (1, 1),
        "hints restored, diagnostics left as the user set them"
    );
    assert_eq!(state(&mut c, &a), (true, false));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn reassert_holds_after_an_lsp_attach_that_turns_hints_on() {
    let mut c = case("attach");
    let (a, b) = (c.a.clone(), c.b.clone());
    // A configuration that turns hints on when a server attaches: at once, from a scheduled call, and once
    // more 300 ms later the way a capability registration does.
    c.nvim.lua(
        "vim.api.nvim_create_autocmd('LspAttach', { callback = function(ev) \
           vim.lsp.inlay_hint.enable(true, { bufnr = ev.buf }) \
           vim.schedule(function() vim.lsp.inlay_hint.enable(true, { bufnr = ev.buf }) end) \
           vim.defer_fn(function() vim.lsp.inlay_hint.enable(true, { bufnr = ev.buf }) end, 300) end })",
        vec![],
    );
    edit(&mut c, &a);
    apply(&mut c, "demo", true, false);
    attach(&mut c, &a);
    c.nvim.flush_scheduled();
    // Past the first retry, then past the late enable and the retry after it.
    c.nvim.advance_timers(150);
    assert_eq!(state(&mut c, &a), (true, false));
    c.nvim.advance_timers(550);
    assert_eq!(state(&mut c, &a), (true, false));
    // A buffer under the root opened after the apply gets the setting too.
    edit(&mut c, &b);
    c.nvim.flush_scheduled();
    assert!(!state(&mut c, &b).1);
    clear(&mut c, "demo");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_hint_baseline_read_before_a_server_attaches_settles_after_it() {
    let mut c = case("settle");
    let a = c.a.clone();
    // A configuration that turns hints on from a scheduled call once a server attaches.
    c.nvim.lua(
        "vim.api.nvim_create_autocmd('LspAttach', { callback = function(ev) \
           vim.schedule(function() vim.lsp.inlay_hint.enable(true, { bufnr = ev.buf }) end) end })",
        vec![],
    );
    edit(&mut c, &a);
    // Applied before any server is attached: the hint baseline read now is not the one to restore.
    apply(&mut c, "demo", true, false);
    attach(&mut c, &a);
    c.nvim.advance_timers(600);
    assert_eq!(
        state(&mut c, &a),
        (true, false),
        "the owner's value holds after the attach"
    );
    clear(&mut c, "demo");
    assert_eq!(
        state(&mut c, &a),
        (true, true),
        "clear restores what the configuration wanted after the attach"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_owner_value_applied_before_an_attach_is_not_taken_as_the_baseline() {
    let mut c = case("own-value");
    let a = c.a.clone();
    // No configuration enables hints, and they start off.
    edit(&mut c, &a);
    apply(&mut c, "demo", true, true);
    attach(&mut c, &a);
    c.nvim.advance_timers(600);
    assert_eq!(
        state(&mut c, &a),
        (true, true),
        "the owner's value holds after the attach"
    );
    clear(&mut c, "demo");
    assert_eq!(
        state(&mut c, &a),
        (true, false),
        "clear restores off, not the value the owner wrote"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_buffer_event_before_the_first_retry_does_not_hide_the_configurations_hints() {
    let mut c = case("buf-enter");
    let a = c.a.clone();
    c.nvim.lua(
        "vim.api.nvim_create_autocmd('LspAttach', { callback = function(ev) \
           vim.schedule(function() vim.lsp.inlay_hint.enable(true, { bufnr = ev.buf }) end) end })",
        vec![],
    );
    edit(&mut c, &a);
    apply(&mut c, "demo", true, false);
    attach(&mut c, &a);
    // After the configuration's scheduled enable and before the first retry at 100 ms.
    c.nvim.advance_timers(30);
    c.nvim.lua(
        "vim.api.nvim_exec_autocmds('BufEnter', { buffer = vim.fn.bufnr(...), modeline = false })",
        vec![s(&a)],
    );
    c.nvim.flush_scheduled();
    c.nvim.advance_timers(600);
    assert_eq!(state(&mut c, &a), (true, false), "the owner's value holds");
    clear(&mut c, "demo");
    assert_eq!(
        state(&mut c, &a),
        (true, true),
        "clear restores what the configuration enabled"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn two_owners_restore_the_buffers_own_baseline_whichever_clears_last() {
    for (first, last) in [("one", "two"), ("two", "one")] {
        let mut c = case(&format!("owners-{first}"));
        let a = c.a.clone();
        edit(&mut c, &a);
        attach(&mut c, &a);
        set(&mut c, &a, true, true);
        apply(&mut c, "one", false, false);
        apply(&mut c, "two", false, false);
        assert_eq!(state(&mut c, &a), (false, false));
        assert_eq!(
            clear(&mut c, first),
            (0, 0),
            "another owner still holds the buffer: nothing is restored"
        );
        assert_eq!(state(&mut c, &a), (false, false), "the remaining owner's values hold");
        c.nvim.lua(
            "vim.api.nvim_exec_autocmds('BufEnter', { buffer = vim.fn.bufnr(...), modeline = false })",
            vec![s(&a)],
        );
        c.nvim.flush_scheduled();
        assert_eq!(state(&mut c, &a), (false, false));
        assert_eq!(clear(&mut c, last), (2, 0), "clearing {first} then {last}");
        assert_eq!(
            state(&mut c, &a),
            (true, true),
            "the buffer's values from before any owner, clearing {first} then {last}"
        );
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_missing_api_reports_partial() {
    let mut c = case("partial");
    let a = c.a.clone();
    edit(&mut c, &a);
    c.nvim.lua("vim.lsp.inlay_hint = {}", vec![]);
    let applied = apply(&mut c, "demo", false, false);
    assert_eq!(field(&applied, "partial").as_bool(), Some(true));
    let diag = c.nvim.lua(
        "return vim.diagnostic.is_enabled({ bufnr = vim.fn.bufnr(...) })",
        vec![s(&a)],
    );
    assert_eq!(diag, Value::from(false), "what exists is still applied");
    clear(&mut c, "demo");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn open_moves_the_cursor_by_characters_and_splits_over_a_modified_buffer() {
    let mut c = case("open");
    let (a, b) = (c.a.clone(), c.b.clone());
    let opened = c.nvim.lua(
        PLUGIN_OPEN_LUA,
        vec![opts_map(&[("path", s(&a)), ("line", 2.into()), ("col", 13.into())])],
    );
    assert_eq!(field(&opened, "how").as_str(), Some("edit"));
    // `let s = "中文"` is 12 characters and 16 bytes: column 13 is byte 16.
    assert_eq!(
        c.nvim.lua("return vim.api.nvim_win_get_cursor(0)", vec![]),
        Value::Array(vec![2.into(), 16.into()])
    );
    c.nvim
        .lua("vim.api.nvim_buf_set_lines(0, 0, 0, false, { '// edited' })", vec![]);
    let opened = c.nvim.lua(
        PLUGIN_OPEN_LUA,
        vec![opts_map(&[("path", s(&b)), ("line", 999.into())])],
    );
    assert_eq!(field(&opened, "how").as_str(), Some("split"));
    assert_eq!(
        field(&opened, "line").as_u64(),
        Some(2),
        "the line is clamped to the buffer"
    );
    assert_eq!(c.nvim.lua("return #vim.api.nvim_list_wins()", vec![]), Value::from(2));
    let again = c
        .nvim
        .lua(PLUGIN_OPEN_LUA, vec![opts_map(&[("path", s(&b)), ("line", 1.into())])]);
    assert_eq!(field(&again, "how").as_str(), Some("current"));
    // `Embed::lua` panics on an nvim error, so the refusal is caught inside nvim; `loadstring` is the test's own
    // way to call the chunk with arguments under `pcall`.
    let refused = c.nvim.lua(
        "local code, p = ... local ok = pcall(function() return assert(loadstring(code))({ path = p }) end) return ok",
        vec![PLUGIN_OPEN_LUA.into(), "relative.rs".into()],
    );
    assert_eq!(refused, Value::from(false), "a relative path is refused");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn open_returns_the_name_of_a_buffer_reused_through_a_symlink() {
    let mut c = case("open-reused-alias");
    let (a, b) = (c.a.clone(), c.b.clone());
    let alias = c.root.join("alias.rs");
    std::os::unix::fs::symlink(&a, &alias).unwrap();
    c.nvim.lua(
        "vim.api.nvim_cmd({cmd='edit', args={...}, magic={file=false}}, {}); vim.bo.filetype = 'rust'",
        vec![s(&alias)],
    );
    c.nvim.lua(PLUGIN_OPEN_LUA, vec![opts_map(&[("path", s(&b))])]);
    let opened = c.nvim.lua(PLUGIN_OPEN_LUA, vec![opts_map(&[("path", s(&a))])]);
    assert_eq!(field(&opened, "how").as_str(), Some("edit"));
    assert_eq!(field(&opened, "path"), &s(&alias));
    assert_eq!(field(&opened, "filetype").as_str(), Some("rust"));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn open_refuses_a_checked_file_replaced_by_a_symlink() {
    let mut c = case("open-symlink");
    let (a, b, outside) = (c.a.clone(), c.b.clone(), c.outside.clone());
    std::fs::remove_file(&a).unwrap();
    std::os::unix::fs::symlink(&outside, &a).unwrap();
    let refused = c.nvim.lua(
        "local code, path = ... return pcall(function() return assert(loadstring(code))({ path = path }) end)",
        vec![PLUGIN_OPEN_LUA.into(), s(&a)],
    );
    assert_eq!(refused, Value::from(false));
    let names = c.nvim.lua(
        "local names = {} for _, b in ipairs(vim.api.nvim_list_bufs()) do names[#names + 1] = vim.api.nvim_buf_get_name(b) end return names",
        vec![],
    );
    assert!(!names.as_array().unwrap().contains(&s(&outside)));
    assert!(!names.as_array().unwrap().contains(&s(&a)));
    let opened = c.nvim.lua(PLUGIN_OPEN_LUA, vec![opts_map(&[("path", s(&b))])]);
    assert_eq!(field(&opened, "how").as_str(), Some("edit"));
}

/// Writes a line into the FIFO at `path` once something opens it for reading, trying for `for_how_long`: an editor
/// that reads the FIFO is let go again instead of hanging the test, and reads what this wrote.
fn release_a_fifo_reader(path: &Path, for_how_long: Duration) -> std::thread::JoinHandle<bool> {
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + for_how_long;
        while std::time::Instant::now() < deadline {
            // SAFETY: `name` is a terminated path in the test's own scratch directory; the descriptor is closed below.
            let fd = unsafe { libc::open(name.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
            if fd >= 0 {
                // SAFETY: `fd` was just opened for writing and is written once, then closed.
                unsafe {
                    libc::write(fd, b"x\n".as_ptr().cast(), 2);
                    libc::close(fd);
                }
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    })
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn open_refuses_a_checked_file_replaced_by_a_fifo_and_the_editor_stays_responsive() {
    let mut c = case("open-fifo");
    let (a, b) = (c.a.clone(), c.b.clone());
    std::fs::remove_file(&a).unwrap();
    let name = std::ffi::CString::new(a.as_os_str().as_bytes()).unwrap();
    // SAFETY: `name` is a terminated path in this test's private scratch directory.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let writer = release_a_fifo_reader(&a, Duration::from_secs(3));
    let answer = c.nvim.lua(
        "local code, path = ... local ok, err = pcall(function() return assert(loadstring(code))({ path = path }) end) \
         return { ok, tostring(err) }",
        vec![PLUGIN_OPEN_LUA.into(), s(&a)],
    );
    let answer = answer.as_array().unwrap();
    assert_eq!(answer[0], Value::from(false), "a FIFO is not opened: {answer:?}");
    assert!(answer[1].as_str().unwrap().contains("not a regular file"), "{answer:?}");
    assert!(!writer.join().unwrap(), "nothing opened the FIFO for reading");
    assert_eq!(c.nvim.eval("1 + 1"), Value::from(2));
    let names = c.nvim.lua(
        "local names = {} for _, b in ipairs(vim.api.nvim_list_bufs()) do names[#names + 1] = vim.api.nvim_buf_get_name(b) end return names",
        vec![],
    );
    assert!(!names.as_array().unwrap().contains(&s(&a)));
    let opened = c.nvim.lua(PLUGIN_OPEN_LUA, vec![opts_map(&[("path", s(&b))])]);
    assert_eq!(field(&opened, "how").as_str(), Some("edit"));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn open_from_a_terminal_window_splits_and_the_terminal_keeps_its_window() {
    let mut c = case("open-terminal");
    let b = c.b.clone();
    // A terminal buffer with no process behind it: nothing runs on the test's machine.
    let terminal = c.nvim.lua(
        "vim.api.nvim_open_term(0, {}) return vim.api.nvim_get_current_buf()",
        vec![],
    );
    let opened = c.nvim.lua(PLUGIN_OPEN_LUA, vec![opts_map(&[("path", s(&b))])]);
    assert_eq!(field(&opened, "how").as_str(), Some("split"));
    assert_eq!(c.nvim.lua("return #vim.api.nvim_list_wins()", vec![]), Value::from(2));
    assert_eq!(
        c.nvim.lua("return #vim.fn.win_findbuf(...)", vec![terminal]),
        Value::from(1),
        "the terminal keeps its window"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn open_from_a_scratch_window_splits_and_the_scratch_text_stays() {
    let mut c = case("open-scratch");
    let b = c.b.clone();
    // A plugin's scratch buffer that is wiped once hidden: an :edit over it would lose its text.
    let scratch = c.nvim.lua(
        "vim.bo.buftype = 'nofile' vim.bo.bufhidden = 'wipe' \
         vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'notes' }) return vim.api.nvim_get_current_buf()",
        vec![],
    );
    let opened = c.nvim.lua(PLUGIN_OPEN_LUA, vec![opts_map(&[("path", s(&b))])]);
    assert_eq!(field(&opened, "how").as_str(), Some("split"));
    assert_eq!(
        c.nvim.lua(
            "local b = ... return vim.api.nvim_buf_is_valid(b) and vim.api.nvim_buf_get_lines(b, 0, -1, false)[1] or ''",
            vec![scratch],
        ),
        Value::from("notes")
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn open_checks_the_edited_buffer_and_wipes_only_a_new_buffer() {
    for existing in [false, true] {
        let mut c = case(if existing {
            "open-existing-swap"
        } else {
            "open-new-swap"
        });
        let (a, b, outside) = (c.a.clone(), c.b.clone(), c.outside.clone());
        edit(&mut c, &b);
        let buf = c.nvim.lua(
            "local path, target, existing = ... \
             local buf = existing and vim.fn.bufadd(path) or -1 \
             vim.api.nvim_create_autocmd('BufReadPost', { once = true, callback = function() \
               local uv = vim.uv or vim.loop assert(uv.fs_unlink(path)) assert(uv.fs_symlink(target, path)) \
             end }) return buf",
            vec![s(&a), s(&outside), existing.into()],
        );
        let refused = c.nvim.lua(
            "local code, path = ... return pcall(function() return assert(loadstring(code))({ path = path }) end)",
            vec![PLUGIN_OPEN_LUA.into(), s(&a)],
        );
        assert_eq!(refused, Value::from(false));
        if existing {
            assert_eq!(
                c.nvim.lua("return vim.api.nvim_buf_is_valid(...)", vec![buf]),
                Value::from(true)
            );
        } else {
            assert_eq!(c.nvim.lua("return vim.fn.bufnr(...)", vec![s(&a)]), Value::from(-1));
        }
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_refused_edit_restores_the_window_and_unloads_an_existing_unloaded_buffer() {
    let mut c = case("open-unloaded-swap");
    let (a, b, outside) = (c.a.clone(), c.b.clone(), c.outside.clone());
    edit(&mut c, &b);
    let previous = c.nvim.lua("return vim.api.nvim_get_current_buf()", vec![]);
    let win = current_win(&mut c);
    c.nvim.command(&format!("badd {}", a.display()));
    let buf = c.nvim.lua("return vim.fn.bufnr(...)", vec![s(&a)]);
    assert_eq!(
        c.nvim.lua("return vim.api.nvim_buf_is_loaded(...)", vec![buf.clone()]),
        false.into()
    );
    c.nvim.lua(
        "local path, target = ... \
         vim.api.nvim_create_autocmd('BufReadPre', { once = true, callback = function() \
           local uv = vim.uv or vim.loop assert(uv.fs_unlink(path)) assert(uv.fs_symlink(target, path)) \
         end })",
        vec![s(&a), s(&outside)],
    );
    let refused = c.nvim.lua(
        "local code, path = ... return pcall(function() return assert(loadstring(code))({ path = path, how = 'edit' }) end)",
        vec![PLUGIN_OPEN_LUA.into(), s(&a)],
    );
    assert_eq!(refused, false.into());
    assert_eq!(current_win(&mut c), win);
    assert_eq!(c.nvim.lua("return vim.api.nvim_get_current_buf()", vec![]), previous);
    assert_eq!(
        c.nvim.lua("return vim.api.nvim_buf_is_valid(...)", vec![buf.clone()]),
        true.into()
    );
    assert_eq!(
        c.nvim.lua("return vim.api.nvim_buf_is_loaded(...)", vec![buf]),
        false.into()
    );
}

/// Swaps `path` for a link to `target` as the next buffer reads it, then runs the open snippet on `path`, which
/// must refuse; returns the error text.
fn open_refused_by_a_swap(c: &mut Case, path: &Path, target: &Path) -> String {
    c.nvim.lua(
        "local path, target = ... \
         vim.api.nvim_create_autocmd('BufReadPre', { once = true, callback = function() \
           local uv = vim.uv or vim.loop assert(uv.fs_unlink(path)) assert(uv.fs_symlink(target, path)) \
         end })",
        vec![s(path), s(target)],
    );
    let answer = c.nvim.lua(
        "local code, path = ... local ok, err = pcall(function() return assert(loadstring(code))({ path = path }) end) \
         return { ok, tostring(err) }",
        vec![PLUGIN_OPEN_LUA.into(), s(path)],
    );
    let answer = answer.as_array().unwrap();
    assert_eq!(answer[0], false.into(), "the open is refused");
    answer[1].as_str().unwrap().to_owned()
}

/// Every loaded buffer's name and lines.
fn loaded_buffers(c: &mut Case) -> Vec<(String, Vec<String>)> {
    let v = c.nvim.lua(
        "local out = {} for _, b in ipairs(vim.api.nvim_list_bufs()) do if vim.api.nvim_buf_is_loaded(b) then \
           out[#out + 1] = { vim.api.nvim_buf_get_name(b), vim.api.nvim_buf_get_lines(b, 0, -1, false) } end end \
         return out",
        vec![],
    );
    v.as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            let pair = pair.as_array().unwrap();
            let lines = pair[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_str().unwrap().to_owned())
                .collect();
            (pair[0].as_str().unwrap().to_owned(), lines)
        })
        .collect()
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_refused_edit_from_the_starting_empty_buffer_keeps_no_trace_of_the_swapped_file() {
    let mut c = case("open-reused-empty-swap");
    let (a, outside) = (c.a.clone(), c.outside.clone());
    let win = current_win(&mut c);
    let err = open_refused_by_a_swap(&mut c, &a, &outside);
    assert!(err.contains("no longer resolves to the checked path"), "{err}");
    assert_eq!(current_win(&mut c), win);
    assert_eq!(
        c.nvim.lua("return vim.api.nvim_buf_get_name(0)", vec![]),
        Value::from("")
    );
    for (name, lines) in loaded_buffers(&mut c) {
        assert_ne!(name, a.to_str().unwrap());
        assert!(!lines.contains(&"out".to_owned()), "{name} holds the swapped-in file");
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_refused_edit_over_a_buffer_wiped_on_leaving_keeps_no_trace_of_the_swapped_file() {
    let mut c = case("open-wiped-current-swap");
    let (a, outside) = (c.a.clone(), c.outside.clone());
    c.nvim.lua(
        "vim.cmd('enew') vim.bo.bufhidden = 'wipe' \
         vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'dashboard' }) vim.bo.modified = false",
        vec![],
    );
    let win = current_win(&mut c);
    let err = open_refused_by_a_swap(&mut c, &a, &outside);
    assert!(err.contains("no longer resolves to the checked path"), "{err}");
    assert_eq!(current_win(&mut c), win);
    for (name, lines) in loaded_buffers(&mut c) {
        assert_ne!(name, a.to_str().unwrap());
        assert!(!lines.contains(&"out".to_owned()), "{name} holds the swapped-in file");
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_refused_split_restores_the_windows_and_preserves_an_already_loaded_buffer() {
    let mut c = case("open-loaded-split-swap");
    let (a, b, outside) = (c.a.clone(), c.b.clone(), c.outside.clone());
    c.nvim.command("set hidden");
    edit(&mut c, &a);
    let buf = c.nvim.lua("return vim.api.nvim_get_current_buf()", vec![]);
    let contents = c.nvim.lua("return vim.api.nvim_buf_get_lines(0, 0, -1, false)", vec![]);
    edit(&mut c, &b);
    c.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'keep this edit' })",
        vec![],
    );
    let previous = c.nvim.lua("return vim.api.nvim_get_current_buf()", vec![]);
    let windows = c.nvim.lua("return vim.api.nvim_list_wins()", vec![]);
    let win = current_win(&mut c);
    c.nvim.lua(
        "local buf, path, target = ... \
         vim.api.nvim_create_autocmd('BufEnter', { buffer = buf, once = true, callback = function() \
           local uv = vim.uv or vim.loop assert(uv.fs_unlink(path)) assert(uv.fs_symlink(target, path)) \
         end })",
        vec![buf.clone(), s(&a), s(&outside)],
    );
    let refused = c.nvim.lua(
        "local code, path = ... return pcall(function() return assert(loadstring(code))({ path = path }) end)",
        vec![PLUGIN_OPEN_LUA.into(), s(&a)],
    );
    assert_eq!(refused, false.into());
    assert_eq!(current_win(&mut c), win);
    assert_eq!(c.nvim.lua("return vim.api.nvim_list_wins()", vec![]), windows);
    assert_eq!(c.nvim.lua("return vim.api.nvim_get_current_buf()", vec![]), previous);
    assert_eq!(
        c.nvim.lua("return vim.api.nvim_buf_is_loaded(...)", vec![buf.clone()]),
        true.into()
    );
    assert_eq!(
        c.nvim
            .lua("return vim.api.nvim_buf_get_lines(..., 0, -1, false)", vec![buf]),
        contents
    );
    assert_eq!(
        c.nvim.lua("return vim.api.nvim_buf_get_lines(0, 0, -1, false)", vec![]),
        Value::Array(vec!["keep this edit".into()])
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn open_counts_an_ill_formed_byte_sequence_as_one_character() {
    let mut c = case("open-ill-formed");
    let a = c.a.clone();
    c.nvim.lua(PLUGIN_OPEN_LUA, vec![opts_map(&[("path", s(&a))])]);
    // Set the bytes in the buffer because reading them from a file may convert them from latin1.
    c.nvim.lua(
        "local a, b = ... vim.api.nvim_buf_set_lines(0, 0, 2, true, { a, b })",
        vec![Value::Binary(b"\xc0ab".to_vec()), Value::Binary(b"\xe2ab".to_vec())],
    );
    for line in [1, 2] {
        let opened = c.nvim.lua(
            PLUGIN_OPEN_LUA,
            vec![opts_map(&[("path", s(&a)), ("line", line.into()), ("col", 2.into())])],
        );
        assert_eq!(field(&opened, "how").as_str(), Some("current"));
        assert_eq!(cursor(&mut c), Value::Array(vec![line.into(), 1.into()]));
    }
}

// ---- the follow snippet ----

const OWNER: &str = "demo";
const SETTLE: Duration = Duration::from_millis(150);

/// A path as the host sends it: bytes.
fn bytes(path: &Path) -> Value {
    Value::Binary(path.as_os_str().as_bytes().to_vec())
}

/// A case with the follow socket listening at `<case dir>/f.sock`.
fn follow_case(name: &str) -> (Case, UnixListener, PathBuf) {
    let mut c = case(&format!("f-{name}"));
    c.nvim.manual_timers();
    c.nvim.lua(
        "vim.uv.hrtime = function() return test_timers.now() * 1000000 end",
        vec![],
    );
    let socket = c.dir.join("f.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    (c, listener, socket)
}

/// Every line sent while manually advancing the snippet's timers, parsed in accept order.
fn lines_within(nvim: &mut Embed, listener: &UnixListener, wait: Duration) -> Vec<serde_json::Value> {
    nvim.advance_timers(wait.as_millis() as u64);
    let mut out = Vec::new();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let mut text = String::new();
                stream.read_to_string(&mut text).expect("a line, then the end");
                for line in text.lines() {
                    out.push(serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}")));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("accept: {e}"),
        }
    }
    out
}

fn drain(c: &mut Case, listener: &UnixListener) {
    lines_within(&mut c.nvim, listener, SETTLE);
}

fn views(lines: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    lines.iter().filter(|l| l["k"] == "view").collect()
}

fn install(c: &mut Case, socket: &Path, auto: bool, commands: &[(&str, &str)], scratch: Option<&Path>) -> Value {
    let commands = Value::Array(
        commands
            .iter()
            .map(|(name, does)| Value::Array(vec![Value::from(*name), Value::from(*does)]))
            .collect(),
    );
    let mut entries = vec![
        ("owner", Value::from(OWNER)),
        ("socket", bytes(socket)),
        ("filetypes", Value::Array(vec![Value::from("markdown")])),
        ("auto", Value::from(auto)),
        ("commands", commands),
    ];
    if let Some(dir) = scratch {
        entries.push(("scratch_dir", bytes(dir)));
    }
    c.nvim
        .lua(PLUGIN_FOLLOW_LUA, vec![Value::from("install"), opts_map(&entries)])
}

/// The follow chunk with `(name, opts)`.
fn op(c: &mut Case, name: &str, opts: Value) -> Value {
    c.nvim.lua(PLUGIN_FOLLOW_LUA, vec![Value::from(name), opts])
}

fn follow(c: &mut Case, on: bool) {
    let answer = op(c, "follow", opts_map(&[("owner", OWNER.into()), ("on", on.into())]));
    assert_eq!(field(&answer, "ok"), &Value::from(true));
}

fn write_doc(c: &Case, name: &str, lines: &[String]) -> PathBuf {
    let path = c.dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn numbered(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("line {i}")).collect()
}

fn buf_of(c: &mut Case, path: &Path) -> u64 {
    c.nvim.lua("return vim.fn.bufnr(...)", vec![s(path)]).as_u64().unwrap()
}

fn current_win(c: &mut Case) -> u64 {
    c.nvim
        .lua("return vim.api.nvim_get_current_win()", vec![])
        .as_u64()
        .unwrap()
}

fn top_line(c: &mut Case, win: u64) -> u64 {
    c.nvim
        .lua("return vim.fn.line('w0', ...)", vec![Value::from(win)])
        .as_u64()
        .unwrap()
}

fn cursor(c: &mut Case) -> Value {
    c.nvim.lua("return vim.api.nvim_win_get_cursor(0)", vec![])
}

fn at(buf: u64, line: u64, extra: (&str, u64)) -> Value {
    opts_map(&[
        ("owner", OWNER.into()),
        ("buf", buf.into()),
        ("line", line.into()),
        (extra.0, extra.1.into()),
    ])
}

fn applied(answer: &Value) -> bool {
    field(answer, "applied").as_bool().unwrap()
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn install_before_vimenter_defines_nothing_and_says_early() {
    // Startup is held in `--cmd`, before a user's config would run; nvim still answers requests inside vim.wait.
    let mut c = case_with(
        "f-early",
        &[
            "--cmd",
            "lua vim.g.hold = true",
            "--cmd",
            "lua vim.wait(10000, function() return not vim.g.hold end, 10)",
        ],
    );
    let socket = c.dir.join("f.sock");
    assert_eq!(c.nvim.eval("v:vim_did_enter"), Value::from(0));
    let early = install(&mut c, &socket, false, &[("EitriMarkdownToggle", "toggle")], None);
    assert_eq!(field(&early, "ok"), &Value::from(false));
    assert_eq!(field(&early, "early"), &Value::from(true));
    assert_eq!(c.nvim.eval("exists(':EitriMarkdownToggle')"), Value::from(0));
    let group = c.nvim.lua(
        "return pcall(vim.api.nvim_get_autocmds, { group = 'EitriPluginFollow_demo' })",
        vec![],
    );
    assert_eq!(group, Value::from(false), "no augroup before VimEnter");
    c.nvim.lua("vim.g.hold = false", vec![]);
    c.nvim
        .wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    let done = install(&mut c, &socket, false, &[("EitriMarkdownToggle", "toggle")], None);
    assert_eq!(field(&done, "ok"), &Value::from(true));
    assert_eq!(c.nvim.eval("exists(':EitriMarkdownToggle')"), Value::from(2));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn install_defines_the_commands_and_skips_ones_the_user_has() {
    let (mut c, listener, socket) = follow_case("define");
    c.nvim.command("command! EitriMarkdown echo 'other'");
    c.nvim.command("command! -buffer DemoLocal echo 'x'");
    let commands = [
        ("EitriMarkdown", "show"),
        ("EitriMarkdownToggle", "toggle"),
        ("DemoLocal", "hide"),
    ];
    let answer = install(&mut c, &socket, false, &commands, None);
    assert_eq!(field(&answer, "ok"), &Value::from(true));
    let skipped = Value::Array(vec!["EitriMarkdown".into(), "DemoLocal".into()]);
    assert_eq!(field(&answer, "skipped"), &skipped);
    assert_eq!(c.nvim.eval("exists(':EitriMarkdownToggle')"), Value::from(2));
    c.nvim.command("EitriMarkdownToggle");
    assert_eq!(
        lines_within(&mut c.nvim, &listener, SETTLE),
        vec![serde_json::json!({"v": 1, "k": "cmd", "owner": "demo", "cmd": "toggle"})]
    );
    let out = c.nvim.eval("execute('EitriMarkdown')");
    assert!(
        out.as_str().unwrap().contains("other"),
        "the user's command ran: {out:?}"
    );
    assert!(lines_within(&mut c.nvim, &listener, SETTLE).is_empty());
    let again = install(&mut c, &socket, false, &commands, None);
    assert_eq!(
        field(&again, "skipped"),
        &skipped,
        "a reinstall does not take its own commands for the user's"
    );
    let ours = c.nvim.lua(
        "local all = vim.api.nvim_get_commands({ builtin = false }) \
         return { all.EitriMarkdownToggle.definition, all.EitriMarkdown.definition }",
        vec![],
    );
    assert_eq!(
        ours,
        Value::Array(vec!["eitri plugin demo toggle".into(), "echo 'other'".into()]),
        "the toggle command is the reinstalled one, and the user's EitriMarkdown untouched"
    );
    drain(&mut c, &listener);
    c.nvim.command("EitriMarkdownToggle");
    assert_eq!(
        lines_within(&mut c.nvim, &listener, SETTLE),
        vec![serde_json::json!({"v": 1, "k": "cmd", "owner": "demo", "cmd": "toggle"})],
        "the reinstalled command sends once"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn teardown_leaves_no_augroup_timer_or_command_and_spares_a_redefined_one() {
    let (mut c, listener, socket) = follow_case("teardown");
    let a = write_doc(&c, "a.md", &numbered(60));
    edit(&mut c, &a);
    let timers = c.nvim.footprint_of(&["EitriPluginFollow_demo"], &[]).active_timers;
    install(
        &mut c,
        &socket,
        true,
        &[("EitriMarkdownToggle", "toggle"), ("EitriMarkdownHide", "hide")],
        None,
    );
    follow(&mut c, true);
    c.nvim.command("normal! 10G");
    c.nvim.command("normal! 20G");
    // The user redefines one of the commands.
    c.nvim.command("command! EitriMarkdownHide echo 'mine'");
    let answer = op(&mut c, "teardown", opts_map(&[("owner", OWNER.into())]));
    assert_eq!(field(&answer, "ok"), &Value::from(true));
    let footprint = c.nvim.footprint_of(&["EitriPluginFollow_demo"], &[]);
    assert_eq!(footprint.autocmds, 0);
    assert_eq!(footprint.active_timers, timers);
    assert_eq!(c.nvim.eval("exists(':EitriMarkdownToggle')"), Value::from(0));
    assert_eq!(
        c.nvim.eval("exists(':EitriMarkdownHide')"),
        Value::from(2),
        "the user's command is left"
    );
    assert_eq!(
        c.nvim.lua(
            "return rawget(_G, '__eitri_plugin_follow') == nil or _G.__eitri_plugin_follow.demo == nil",
            vec![]
        ),
        Value::from(true)
    );
    // Nothing more is sent once it is torn down.
    drain(&mut c, &listener);
    c.nvim.command("normal! 30G");
    let after = lines_within(&mut c.nvim, &listener, SETTLE);
    assert!(after.is_empty(), "{after:?}");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_non_markdown_buffer_sends_no_name() {
    let (mut c, listener, socket) = follow_case("non-md");
    let a = write_doc(&c, "a.md", &numbered(20));
    let x = write_doc(&c, "x.lua", &["return 1".to_string()]);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    drain(&mut c, &listener);
    edit(&mut c, &x);
    c.nvim.command("normal! G");
    for line in lines_within(&mut c.nvim, &listener, SETTLE) {
        assert!(!line["name"].as_str().unwrap_or("").ends_with("x.lua"), "{line}");
        assert_ne!(line["filetype"], "lua", "{line}");
    }
    edit(&mut c, &a);
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let a_name = a.to_str().unwrap();
    assert!(
        views(&lines)
            .iter()
            .any(|l| l["name"] == a_name && l["filetype"] == "markdown" && l["current"] == true),
        "{lines:?}"
    );
    edit(&mut c, &x);
    for line in lines_within(&mut c.nvim, &listener, SETTLE) {
        assert!(!line["name"].as_str().unwrap_or("").ends_with("x.lua"), "{line}");
        if line["name"] == a_name {
            assert_eq!(line["current"], false, "{line}");
        }
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_scratch_buffer_is_never_followed() {
    let (mut c, listener, socket) = follow_case("scratch");
    let scratch = c.dir.join("scratch");
    let draft = write_doc(&c, "scratch/1-draft.md", &["# draft".to_string()]);
    let a = write_doc(&c, "a.md", &numbered(20));
    install(&mut c, &socket, false, &[], Some(&scratch));
    follow(&mut c, true);
    drain(&mut c, &listener);
    edit(&mut c, &draft);
    assert_eq!(
        c.nvim.eval("&filetype"),
        Value::from("markdown"),
        "the scratch buffer is Markdown"
    );
    c.nvim.command("normal! G");
    for line in lines_within(&mut c.nvim, &listener, SETTLE) {
        assert!(!line.to_string().contains("1-draft.md"), "{line}");
    }
    edit(&mut c, &a);
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    assert!(
        views(&lines).iter().any(|l| l["name"] == a.to_str().unwrap()),
        "{lines:?}"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_closed_fold_and_a_smoothscroll_fraction_at_the_top() {
    let (mut c, listener, socket) = follow_case("fold");
    let mut doc = numbered(200);
    doc[29] = "x".repeat(2000);
    let a = write_doc(&c, "a.md", &doc);
    edit(&mut c, &a);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    drain(&mut c, &listener);

    c.nvim.command("setlocal foldmethod=manual");
    c.nvim.command("10,20fold");
    c.nvim.command("call winrestview({'topline': 15, 'lnum': 25})");
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let last = *views(&lines).last().expect("a view line after the scroll");
    assert_eq!(last["top"], 10, "a closed fold reports its first line: {lines:?}");

    // A fold closed over the top line leaves the window's top inside it until the next redraw; a view sent before
    // then still reports the fold's first line.
    c.nvim.command("normal! zE");
    c.nvim.command("call winrestview({'topline': 15, 'lnum': 25})");
    drain(&mut c, &listener);
    let inside = c.nvim.lua(
        "vim.api.nvim_command('10,20fold') \
         local top = vim.fn.winsaveview().topline \
         vim.api.nvim_exec_autocmds('CursorMoved', {}) \
         return top",
        vec![],
    );
    assert_eq!(
        inside,
        Value::from(15),
        "the top is still inside the fold when the view is sent"
    );
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let first = *views(&lines).first().expect("a view line after the fold");
    assert_eq!(
        first["top"], 10,
        "the fold's first line, not the line inside it: {lines:?}"
    );

    c.nvim.command("normal! zR");
    c.nvim.command("setlocal wrap smoothscroll");
    c.nvim.command("normal! 30Gzt");
    c.nvim.command("normal! \u{5}");
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let rows = c
        .nvim
        .lua(
            "return vim.api.nvim_win_text_height(0, { start_row = 29, end_row = 29 }).all",
            vec![],
        )
        .as_u64()
        .unwrap() as f64;
    let last = *views(&lines).last().expect("a view line after <C-e>");
    assert_eq!(last["top"], 30, "{lines:?}");
    let fraction = last["top_fraction"].as_f64().unwrap();
    assert!(rows > 1.0, "the long line wraps");
    assert!((fraction - 1.0 / rows).abs() < 1e-9, "{fraction} against 1/{rows}");

    c.nvim.command("setlocal nosmoothscroll");
    c.nvim.command("normal! l");
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let last = *views(&lines).last().expect("a view line after the move");
    assert_eq!(last["top_fraction"].as_f64(), Some(0.0), "{lines:?}");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn reveal_does_not_change_the_current_window_and_its_echo_carries_the_token() {
    let (mut c, listener, socket) = follow_case("reveal");
    let a = write_doc(&c, "a.md", &numbered(300));
    let x = write_doc(&c, "x.lua", &["return 1".to_string()]);
    edit(&mut c, &a);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    c.nvim.command(&format!("split {}", x.display()));
    let a_buf = buf_of(&mut c, &a);
    let x_buf = buf_of(&mut c, &x);
    let a_win = c
        .nvim
        .lua("return vim.fn.bufwinid(...)", vec![Value::from(a_buf)])
        .as_u64()
        .unwrap();
    let before = current_win(&mut c);
    assert_ne!(before, a_win);
    drain(&mut c, &listener);

    let answer = op(&mut c, "reveal", at(a_buf, 50, ("token", 9)));
    assert!(applied(&answer));
    assert_eq!(current_win(&mut c), before, "the current window is unchanged");
    assert_eq!(top_line(&mut c, a_win), 50);
    let mode = c.nvim.lua("return vim.api.nvim_get_mode().mode", vec![]);
    assert_eq!(mode, Value::from("n"));
    let in_view = c.nvim.lua(
        "local w = ... local l = vim.api.nvim_win_get_cursor(w)[1] \
         return l >= vim.fn.line('w0', w) and l <= vim.fn.line('w$', w)",
        vec![Value::from(a_win)],
    );
    assert_eq!(in_view, Value::from(true), "the cursor stays in view");
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    assert!(
        views(&lines)
            .iter()
            .any(|l| l["top"] == 50 && l["cause"] == "reveal" && l["token"] == 9),
        "{lines:?}"
    );

    // A cursor below the new view is brought up into it, or nvim would scroll back to it.
    c.nvim.lua(
        "local w = ... vim.api.nvim_win_set_cursor(w, { 250, 0 })",
        vec![Value::from(a_win)],
    );
    assert!(applied(&op(&mut c, "reveal", at(a_buf, 10, ("token", 11)))));
    assert_eq!(top_line(&mut c, a_win), 10);
    let in_view = c.nvim.lua(
        "local w = ... local l = vim.api.nvim_win_get_cursor(w)[1] \
         return l >= vim.fn.line('w0', w) and l <= vim.fn.line('w$', w)",
        vec![Value::from(a_win)],
    );
    assert_eq!(in_view, Value::from(true), "the cursor is moved into view");
    assert_eq!(current_win(&mut c), before);

    // At the buffer's top 'scrolloff' keeps no margin, so a cursor near the top stays where it is.
    c.nvim.lua(
        "local w = ... vim.wo[w].scrolloff = 5 \
         vim.api.nvim_win_call(w, function() vim.fn.winrestview({ topline = 1, lnum = 3, col = 0 }) end)",
        vec![Value::from(a_win)],
    );
    assert!(applied(&op(&mut c, "reveal", at(a_buf, 1, ("token", 12)))));
    let at_top = c.nvim.lua(
        "local w = ... return { vim.fn.line('w0', w), vim.api.nvim_win_get_cursor(w)[1] }",
        vec![Value::from(a_win)],
    );
    assert_eq!(
        at_top,
        Value::Array(vec![1.into(), 3.into()]),
        "the cursor stays on line 3"
    );

    let other = op(&mut c, "reveal", at(x_buf, 10, ("token", 10)));
    assert!(!applied(&other), "another buffer is not revealed");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reveal_echo_belongs_to_the_buffer_it_scrolled() {
    let (mut c, listener, socket) = follow_case("echo-buf");
    let a = write_doc(&c, "a.md", &numbered(300));
    let b = write_doc(&c, "b.md", &numbered(300));
    edit(&mut c, &a);
    let a_buf = buf_of(&mut c, &a);
    edit(&mut c, &b);
    let b_buf = buf_of(&mut c, &b);
    edit(&mut c, &a);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    drain(&mut c, &listener);
    // Reveal a.md's line 40, then at once show b.md in the same window starting on line 40: b.md's view comes
    // well within the echo window, and its top equals the revealed one.
    assert!(applied(&op(&mut c, "reveal", at(a_buf, 40, ("token", 5)))));
    c.nvim.lua(
        "vim.api.nvim_set_current_buf(...) vim.fn.winrestview({ topline = 40, lnum = 45 })",
        vec![Value::from(b_buf)],
    );
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let of_b: Vec<_> = views(&lines).into_iter().filter(|l| l["buf"] == b_buf).collect();
    assert!(!of_b.is_empty(), "b.md is followed: {lines:?}");
    for l in of_b {
        assert_eq!(l["cause"], "user", "{l}");
        assert!(l.get("token").is_none(), "{l}");
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn reveal_scrolls_only_the_followed_window_of_a_buffer_shown_twice() {
    let (mut c, _listener, socket) = follow_case("twice");
    let a = write_doc(&c, "a.md", &numbered(300));
    let x = write_doc(&c, "x.lua", &["return 1".to_string()]);
    edit(&mut c, &a);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    c.nvim.command("split");
    let w1 = current_win(&mut c);
    let w2 = c
        .nvim
        .lua("return vim.fn.win_getid(vim.fn.winnr('j'))", vec![])
        .as_u64()
        .unwrap();
    assert_ne!(w1, w2);
    c.nvim.command(&format!("tabnew {}", a.display()));
    let w3 = current_win(&mut c);
    c.nvim.command("tabprevious");
    assert_eq!(current_win(&mut c), w1);
    let a_buf = buf_of(&mut c, &a);

    let (w2_top, w3_top) = (top_line(&mut c, w2), top_line(&mut c, w3));
    assert!(applied(&op(&mut c, "reveal", at(a_buf, 80, ("token", 1)))));
    assert_eq!(top_line(&mut c, w1), 80);
    assert_eq!(top_line(&mut c, w2), w2_top, "the other window in this tab page stays");
    assert_eq!(top_line(&mut c, w3), w3_top, "the window in the other tab page stays");

    edit(&mut c, &x);
    assert!(applied(&op(&mut c, "reveal", at(a_buf, 120, ("token", 2)))));
    assert_eq!(top_line(&mut c, w2), 120, "a window of the current tab page");
    assert_eq!(top_line(&mut c, w3), w3_top);

    c.nvim.lua("vim.api.nvim_win_close(..., true)", vec![Value::from(w2)]);
    assert!(applied(&op(&mut c, "reveal", at(a_buf, 140, ("token", 3)))));
    assert_eq!(top_line(&mut c, w3), 140, "any window showing it");
    assert_eq!(current_win(&mut c), w1);
}

/// A reveal's options with the cursor line the page reads at.
fn reveal_at(buf: u64, line: u64, cursor: u64, token: u64) -> Value {
    opts_map(&[
        ("owner", OWNER.into()),
        ("buf", buf.into()),
        ("line", line.into()),
        ("token", token.into()),
        ("cursor", cursor.into()),
    ])
}

/// The window's top as nvim validates it (`line('w0')` brings the top in line with 'scrolloff' first) and the
/// cursor line.
fn top_and_cursor(c: &mut Case, win: u64) -> (u64, u64) {
    let v = c.nvim.lua(
        "local w = ... return { vim.fn.line('w0', w), vim.api.nvim_win_get_cursor(w)[1] }",
        vec![Value::from(win)],
    );
    let v = v.as_array().unwrap();
    (v[0].as_u64().unwrap(), v[1].as_u64().unwrap())
}

/// A reader's nvim: 'scrolloff' 4, 'smoothscroll', 'wrap', line numbers both ways.
fn readers_window(c: &mut Case) -> u64 {
    c.nvim
        .command("setlocal scrolloff=4 smoothscroll wrap number relativenumber");
    current_win(c)
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reveal_with_a_cursor_keeps_the_top_and_holds_the_cursor_without_scrolling() {
    let (mut c, listener, socket) = follow_case("reveal-cursor");
    let mut doc = numbered(300);
    // Line 100 wraps over several screen lines.
    doc[99] = "word ".repeat(80);
    let a = write_doc(&c, "a.md", &doc);
    edit(&mut c, &a);
    let w = readers_window(&mut c);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    drain(&mut c, &listener);
    let a_buf = buf_of(&mut c, &a);

    // A reading line outside the 'scrolloff' band: the cursor is put on it and the top stays.
    assert!(applied(&op(&mut c, "reveal", reveal_at(a_buf, 50, 60, 1))));
    assert_eq!(top_and_cursor(&mut c, w), (50, 60));
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let echo = views(&lines)
        .into_iter()
        .find(|l| l["token"] == 1)
        .unwrap_or_else(|| panic!("the echo: {lines:?}"));
    assert_eq!(echo["top"], 50);
    assert_eq!(echo["cursor"]["line"], 60);
    assert_eq!(echo["scrolloff"], 4, "the view says the window's 'scrolloff'");

    // Enter's jump to the line the cursor is on moves no window.
    assert!(applied(&op(&mut c, "jump", at(a_buf, 60, ("col", 0)))));
    assert_eq!(top_and_cursor(&mut c, w), (50, 60));
    // Nor does a jump to another line the window holds outside the band.
    assert!(applied(&op(&mut c, "jump", at(a_buf, 57, ("col", 0)))));
    assert_eq!(top_and_cursor(&mut c, w), (50, 57));

    // A reading line inside the band at the top: the nearest line nvim can hold there, and the top stays.
    assert!(applied(&op(&mut c, "reveal", reveal_at(a_buf, 80, 81, 2))));
    assert_eq!(top_and_cursor(&mut c, w), (80, 84));
    // At the bottom edge likewise, from the other side.
    assert!(applied(&op(&mut c, "reveal", reveal_at(a_buf, 200, 200, 3))));
    let bottom = c
        .nvim
        .lua("return vim.fn.line('w$', ...)", vec![Value::from(w)])
        .as_u64()
        .unwrap();
    assert!(applied(&op(&mut c, "reveal", reveal_at(a_buf, 200, bottom, 6))));
    assert_eq!(top_and_cursor(&mut c, w), (200, bottom - 4));

    // Screen lines, not buffer lines: line 100 at the top takes more than four of them, so line 101 already has its
    // 'scrolloff' above it.
    assert!(applied(&op(&mut c, "reveal", reveal_at(a_buf, 100, 100, 4))));
    assert_eq!(top_and_cursor(&mut c, w), (100, 101));
    assert!(applied(&op(&mut c, "jump", at(a_buf, 101, ("col", 0)))));
    assert_eq!(
        top_and_cursor(&mut c, w),
        (100, 101),
        "Enter on the held line keeps the window"
    );
    let skip = c.nvim.lua(
        "local w = ... return vim.api.nvim_win_call(w, vim.fn.winsaveview).skipcol",
        vec![Value::from(w)],
    );
    assert_eq!(skip, Value::from(0), "no part of the top line is scrolled away");

    // A reveal without a cursor keeps the old behaviour: the cursor moves only as far as 'scrolloff' needs.
    assert!(applied(&op(&mut c, "reveal", at(a_buf, 140, ("token", 5)))));
    assert_eq!(top_and_cursor(&mut c, w), (140, 144));
}

/// A reveal whose cursor line starts on a row of the whole nvim grid.
fn reveal_on_row(buf: u64, cursor: u64, row: u64, token: u64) -> Value {
    opts_map(&[
        ("owner", OWNER.into()),
        ("buf", buf.into()),
        ("line", 1.into()),
        ("token", token.into()),
        ("cursor", cursor.into()),
        ("row", row.into()),
    ])
}

/// The window's first grid row and the first screen row of the given buffer line.
fn screen_rows(c: &mut Case, win: u64, line: u64) -> (u64, u64) {
    let v = c.nvim.lua(
        "local w, l = ... return { vim.fn.win_screenpos(w)[1], vim.fn.screenpos(w, l, 1).row }",
        vec![win.into(), line.into()],
    );
    let v = v.as_array().unwrap();
    (v[0].as_u64().unwrap(), v[1].as_u64().unwrap())
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_view_counts_the_cursor_lines_row_when_its_first_column_is_scrolled_out_sideways() {
    let (mut c, listener, socket) = follow_case("view-row-leftcol");
    let mut doc = numbered(300);
    doc[89] = "y".repeat(400);
    let a = write_doc(&c, "a.md", &doc);
    edit(&mut c, &a);
    c.nvim.command("set lines=50 columns=100");
    c.nvim.command("botright split");
    c.nvim.command("setlocal nowrap scrolloff=0 sidescrolloff=0");
    c.nvim.command("normal! 80Gzt10j300|");
    let w = current_win(&mut c);
    let (first, start) = screen_rows(&mut c, w, 90);
    let leftcol = c
        .nvim
        .lua("return vim.fn.winsaveview().leftcol", vec![])
        .as_u64()
        .unwrap();
    assert!(
        leftcol > 0 && start == 0,
        "column 1 of line 90 is scrolled out sideways: leftcol {leftcol}, row {start}"
    );
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let last = *views(&lines).last().expect("a view of the window scrolled sideways");
    assert_eq!(last["cursor"]["line"], 90);
    assert_eq!(last["cursor_row"], first + 10, "line 90 is the window's eleventh row");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reveal_takes_in_a_tall_line_rather_than_leave_its_cursor_within_scrolloff_of_the_top() {
    let (mut c, listener, socket) = follow_case("reveal-row-tall");
    let mut doc = numbered(300);
    doc[94] = "x".repeat(1200);
    let a = write_doc(&c, "a.md", &doc);
    edit(&mut c, &a);
    c.nvim.command("set lines=50 columns=100");
    c.nvim.command("botright split");
    c.nvim
        .command("setlocal wrap nosmoothscroll scrolloff=3 nonumber norelativenumber");
    let w = current_win(&mut c);
    let first = screen_rows(&mut c, w, 1).0;
    let tall = c
        .nvim
        .lua(
            "local w = ... return vim.api.nvim_win_text_height(w, { start_row = 94, end_row = 94 }).all",
            vec![w.into()],
        )
        .as_u64()
        .unwrap();
    assert!(
        tall > 7,
        "line 95 takes more rows than the seven asked for above line 96: {tall}"
    );
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    drain(&mut c, &listener);
    let buf = buf_of(&mut c, &a);
    assert!(applied(&op(&mut c, "reveal", reveal_on_row(buf, 96, first + 7, 43))));
    let (top, held) = top_and_cursor(&mut c, w);
    assert_eq!(held, 96, "the cursor stays on the line asked for");
    assert_eq!(top, 95, "the tall line above it is taken in whole");
    assert_eq!(
        screen_rows(&mut c, w, 96).1,
        first + tall,
        "line 96 starts below the tall line"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reveal_of_a_line_inside_a_closed_fold_starts_the_fold_on_the_row() {
    let (mut c, listener, socket) = follow_case("reveal-row-own-fold");
    let a = write_doc(&c, "a.md", &numbered(300));
    edit(&mut c, &a);
    c.nvim.command("set lines=50 columns=100");
    c.nvim.command("botright split");
    c.nvim.command("setlocal scrolloff=0 foldmethod=manual");
    c.nvim.command("90,99fold");
    let w = current_win(&mut c);
    let first = screen_rows(&mut c, w, 1).0;
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    drain(&mut c, &listener);
    let buf = buf_of(&mut c, &a);
    assert!(applied(&op(&mut c, "reveal", reveal_on_row(buf, 96, first + 9, 44))));
    let (top, held) = top_and_cursor(&mut c, w);
    assert_eq!(top, 81, "nine rows above the fold, none for the fold itself");
    assert!((90..=99).contains(&held), "the cursor is in the fold: {held}");
    assert_eq!(
        screen_rows(&mut c, w, 90).1,
        first + 9,
        "the fold starts on the row asked for"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_view_reports_grid_rows_and_the_cursor_lines_screen_row_in_a_lower_split() {
    let (mut c, listener, socket) = follow_case("view-rows");
    let a = write_doc(&c, "a.md", &numbered(300));
    edit(&mut c, &a);
    c.nvim.command("set lines=50 columns=100");
    c.nvim.command("botright split");
    c.nvim.command("normal! 70Gzt5j");
    let w = current_win(&mut c);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let last = *views(&lines).last().expect("the lower split's view");
    let grid_rows = c.nvim.lua("return vim.o.lines", vec![]).as_u64().unwrap();
    let (first, row) = screen_rows(&mut c, w, 75);
    assert!(first > 1, "the followed split starts below the grid's first row");
    assert!(row > first, "the cursor is inside the followed split");
    assert_eq!(last["cursor"]["line"], 75);
    assert_eq!(last["rows"], grid_rows);
    assert_eq!(last["cursor_row"], row);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_view_uses_the_windows_first_row_when_the_cursor_line_starts_above_it() {
    let (mut c, listener, socket) = follow_case("view-row-smoothscroll");
    let mut doc = numbered(300);
    doc[29] = "x".repeat(2000);
    let a = write_doc(&c, "a.md", &doc);
    edit(&mut c, &a);
    c.nvim.command("botright split");
    c.nvim.command("setlocal wrap smoothscroll scrolloff=0");
    c.nvim.command("normal! 30Gzt");
    c.nvim.command("normal! \u{5}");
    let w = current_win(&mut c);
    let (first, row) = screen_rows(&mut c, w, 30);
    assert!(first > 1);
    assert_eq!(row, 0, "the long cursor line starts above the visible window");
    assert_eq!(top_and_cursor(&mut c, w).1, 30);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let last = *views(&lines).last().expect("a partially scrolled cursor line's view");
    assert_eq!(last["cursor_row"], first);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reveal_places_its_cursor_line_on_a_grid_row_with_wrap_folds_and_clamps() {
    for kind in ["plain", "wrap", "fold", "scrolloff", "buffer-start"] {
        let (mut c, listener, socket) = follow_case(&format!("reveal-row-{kind}"));
        let mut doc = numbered(300);
        if kind == "wrap" {
            doc[93] = "x".repeat(80);
            doc[94] = "x".repeat(80);
        }
        let a = write_doc(&c, "a.md", &doc);
        edit(&mut c, &a);
        c.nvim.command("set lines=50 columns=100");
        c.nvim.command("botright split");
        c.nvim
            .command("setlocal scrolloff=0 wrap nonumber norelativenumber foldmethod=manual");
        if kind == "wrap" {
            c.nvim.command("vsplit");
            c.nvim.command("vertical resize 30");
        }
        if kind == "fold" {
            c.nvim.command("90,94fold");
        }
        if kind == "scrolloff" {
            c.nvim.command("setlocal scrolloff=3");
        }
        let w = current_win(&mut c);
        let first = screen_rows(&mut c, w, 1).0;
        assert!(first > 1, "{kind}: rows count from the whole grid");
        let (cursor, row, expected) = match kind {
            "wrap" => (96, first + 7, first + 7),
            "scrolloff" => (96, 1, first + 3),
            "buffer-start" => (3, first + 8, first + 2),
            _ => (96, first + 6, first + 6),
        };
        install(&mut c, &socket, false, &[], None);
        follow(&mut c, true);
        drain(&mut c, &listener);
        let buf = buf_of(&mut c, &a);
        assert!(
            applied(&op(&mut c, "reveal", reveal_on_row(buf, cursor, row, 42))),
            "{kind}"
        );
        let (top, held_cursor) = top_and_cursor(&mut c, w);
        assert_eq!(held_cursor, cursor, "{kind}: the reveal's cursor");
        assert_eq!(
            screen_rows(&mut c, w, cursor).1,
            expected,
            "{kind}: the cursor line's grid row"
        );
        match kind {
            "plain" => assert_eq!(top, 90),
            "wrap" => {
                let height = c
                    .nvim
                    .lua(
                        "local w = ... return vim.api.nvim_win_text_height(w, { start_row = 93, end_row = 94 }).all",
                        vec![w.into()],
                    )
                    .as_u64()
                    .unwrap();
                assert!(height > 2, "the two long lines really wrap");
                assert_eq!(top, 93, "seven screen rows above the cursor include both wrapped lines");
            }
            "fold" => assert_eq!(top, 86, "the five folded lines take one screen row"),
            "scrolloff" => assert_eq!(top, 93),
            "buffer-start" => assert_eq!(top, 1),
            _ => unreachable!(),
        }
        let lines = lines_within(&mut c.nvim, &listener, SETTLE);
        let echo = views(&lines)
            .into_iter()
            .find(|l| l["token"] == 42)
            .unwrap_or_else(|| panic!("{kind}: the echo: {lines:?}"));
        assert_eq!(echo["cause"], "reveal", "{kind}");
        assert_eq!(echo["cursor_row"], expected, "{kind}");
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn jump_lands_on_the_first_non_blank_and_makes_the_window_current() {
    let (mut c, _listener, socket) = follow_case("jump");
    let mut doc = numbered(10);
    doc[4] = "   indented".to_string();
    doc[5] = "\u{4e2d}\u{6587}x".to_string();
    let a = write_doc(&c, "a.md", &doc);
    let x = write_doc(&c, "x.lua", &["return 1".to_string()]);
    edit(&mut c, &a);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    // Lines 7 and 8 get bytes that are not UTF-8 (set in the buffer, as a file holding them would be read as
    // latin1): a lone 0xFF, and a three-byte sequence cut after two bytes.
    c.nvim.lua(
        "local a, b = ... vim.api.nvim_buf_set_lines(0, 6, 8, true, { a, b })",
        vec![Value::Binary(b"x\xffyz".to_vec()), Value::Binary(b"\xe4\xb8z".to_vec())],
    );
    c.nvim.command(&format!("split {}", x.display()));
    let a_buf = buf_of(&mut c, &a);
    let a_win = c
        .nvim
        .lua("return vim.fn.bufwinid(...)", vec![Value::from(a_buf)])
        .as_u64()
        .unwrap();

    assert!(applied(&op(&mut c, "jump", at(a_buf, 5, ("col", 0)))));
    assert_eq!(current_win(&mut c), a_win);
    assert_eq!(cursor(&mut c), Value::Array(vec![5.into(), 3.into()]));
    assert!(applied(&op(&mut c, "jump", at(a_buf, 6, ("col", 3)))));
    assert_eq!(
        cursor(&mut c),
        Value::Array(vec![6.into(), 6.into()]),
        "two 3-byte characters before the third"
    );
    // The page sees each ill-formed subsequence as one U+FFFD and counts columns over that text.
    assert!(applied(&op(&mut c, "jump", at(a_buf, 7, ("col", 3)))));
    assert_eq!(
        cursor(&mut c),
        Value::Array(vec![7.into(), 2.into()]),
        "'y' after x and 0xFF"
    );
    assert!(applied(&op(&mut c, "jump", at(a_buf, 8, ("col", 2)))));
    assert_eq!(
        cursor(&mut c),
        Value::Array(vec![8.into(), 2.into()]),
        "'z' after the cut sequence"
    );
    assert!(applied(&op(&mut c, "jump", at(a_buf, 999, ("col", 0)))));
    assert_eq!(cursor(&mut c).as_array().unwrap()[0], Value::from(10));

    c.nvim.command(&format!("bwipe! {}", a.display()));
    assert!(!applied(&op(&mut c, "jump", at(a_buf, 1, ("col", 0)))));
}

fn mode(c: &mut Case) -> String {
    c.nvim
        .lua("return vim.api.nvim_get_mode().mode", vec![])
        .as_str()
        .unwrap()
        .to_string()
}

/// A jump from (`line`, `col`) selecting to (`to`, `to_col`) in `mode` (`char` or `line`).
fn select(buf: u64, line: u64, col: u64, to: u64, to_col: u64, mode: &str) -> Value {
    opts_map(&[
        ("owner", OWNER.into()),
        ("buf", buf.into()),
        ("line", line.into()),
        ("col", col.into()),
        ("select_line", to.into()),
        ("select_col", to_col.into()),
        ("select_mode", mode.into()),
    ])
}

/// Waits until nvim is in `want` mode.
fn wait_mode(c: &mut Case, want: &str) {
    c.nvim.wait_until(want, |n| {
        n.lua("return vim.api.nvim_get_mode().mode", vec![]) == Value::from(want)
    });
}

/// What the selection holds and where it started, by typing `y` as the user would (the yank ends visual mode, and a
/// key typed after a hand-off must reach Visual mode, not text).
fn yank_selection(c: &mut Case) -> (String, Value) {
    let start = c.nvim.lua("return vim.fn.getpos('v')", vec![]);
    c.nvim.input("y");
    wait_mode(c, "n");
    let text = c
        .nvim
        .lua("return vim.fn.getreg('\"')", vec![])
        .as_str()
        .unwrap()
        .to_string();
    (text, start)
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_jump_with_select_selects_in_visual_mode_without_scrolling() {
    let (mut c, _listener, socket) = follow_case("select");
    let mut doc = numbered(300);
    doc[159] = "\u{4e2d}\u{6587}xyz".to_string();
    let a = write_doc(&c, "a.md", &doc);
    edit(&mut c, &a);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    let a_buf = buf_of(&mut c, &a);
    let win = current_win(&mut c);
    c.nvim.command("150");
    c.nvim.command("normal! zz");
    let top = top_line(&mut c, win);
    assert!(top > 30, "line 150 centred puts the top well below line 20 ({top})");

    // Charwise, from a line far above the window to one inside it: nvim's cursor ends on the second place, and
    // the window does not move.
    assert!(applied(&op(&mut c, "jump", select(a_buf, 20, 2, 151, 4, "char"))));
    assert_eq!(mode(&mut c), "v");
    assert_eq!(cursor(&mut c), Value::Array(vec![151.into(), 3.into()]));
    assert_eq!(top_line(&mut c, win), top, "the first place is never visited");
    let (text, start) = yank_selection(&mut c);
    assert_eq!(start, Value::Array(vec![0.into(), 20.into(), 2.into(), 0.into()]));
    assert!(text.starts_with("ine 20\n") && text.ends_with("\nline"), "{text:?}");

    // Backward, on one line of multi-byte characters: columns count characters.
    assert!(applied(&op(&mut c, "jump", select(a_buf, 160, 4, 160, 2, "char"))));
    assert_eq!(cursor(&mut c), Value::Array(vec![160.into(), 3.into()]));
    assert_eq!(yank_selection(&mut c).0, "\u{6587}xy");

    // Linewise from insert mode: nvim leaves insert mode and selects as its next keys, and the buffer is unchanged.
    let tick = c.nvim.lua("return vim.b.changedtick", vec![]);
    c.nvim.input("i");
    wait_mode(&mut c, "i");
    assert!(applied(&op(&mut c, "jump", select(a_buf, 152, 0, 154, 0, "line"))));
    wait_mode(&mut c, "V");
    assert_eq!(cursor(&mut c).as_array().unwrap()[0], Value::from(154));
    assert_eq!(yank_selection(&mut c).0, "line 152\nline 153\nline 154\n");
    assert_eq!(
        c.nvim.lua("return vim.b.changedtick", vec![]),
        tick,
        "nothing was typed into the text"
    );

    // From visual mode of the other kind, and with lines and columns past the buffer: held to its last line's
    // last character.
    c.nvim.input("V");
    wait_mode(&mut c, "V");
    assert!(applied(&op(&mut c, "jump", select(a_buf, 299, 999, 9999, 999, "char"))));
    assert_eq!(mode(&mut c), "v");
    assert_eq!(cursor(&mut c), Value::Array(vec![300.into(), 7.into()]));
    assert_eq!(yank_selection(&mut c).0, "9\nline 300");

    // An exclusive selection still holds the character the page selected last.
    c.nvim.command("set selection=exclusive");
    assert!(applied(&op(&mut c, "jump", select(a_buf, 160, 1, 160, 3, "char"))));
    assert_eq!(yank_selection(&mut c).0, "\u{4e2d}\u{6587}x");
    assert!(applied(&op(&mut c, "jump", select(a_buf, 160, 5, 160, 2, "char"))));
    assert_eq!(yank_selection(&mut c).0, "\u{6587}xyz");
    // An empty line has no character to go on to.
    c.nvim
        .lua("vim.api.nvim_buf_set_lines(0, 160, 161, true, { '' })", vec![]);
    assert!(applied(&op(&mut c, "jump", select(a_buf, 160, 1, 161, 1, "char"))));
    assert_eq!(mode(&mut c), "v");
    c.nvim.input("<Esc>");
    wait_mode(&mut c, "n");
    c.nvim.command("set selection=inclusive");

    // A command line being typed is the user's: nothing is selected and nothing moves.
    let before = cursor(&mut c);
    c.nvim.input(":");
    wait_mode(&mut c, "c");
    assert!(!applied(&op(&mut c, "jump", select(a_buf, 10, 0, 12, 0, "line"))));
    assert_eq!(mode(&mut c), "c");
    c.nvim.input("<Esc>");
    wait_mode(&mut c, "n");
    assert_eq!(cursor(&mut c), before);

    // A closed fold at either end is opened, so the selection is the one asked for, not the whole fold.
    c.nvim.command("set foldmethod=manual");
    c.nvim.command("20,24fold");
    c.nvim.command("40,44fold");
    assert!(applied(&op(&mut c, "jump", select(a_buf, 22, 2, 42, 3, "char"))));
    let (text, _) = yank_selection(&mut c);
    assert!(text.starts_with("ine 22\n") && text.ends_with("\nlin"), "{text:?}");
    c.nvim.command("normal! zE");

    // A jump without `select` is the plain jump it was.
    assert!(applied(&op(&mut c, "jump", at(a_buf, 5, ("col", 0)))));
    assert_eq!(mode(&mut c), "n");
    assert_eq!(cursor(&mut c), Value::Array(vec![5.into(), 0.into()]));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn text_refuses_past_the_caps_without_reading_lines() {
    let (mut c, _listener, socket) = follow_case("text");
    let a = write_doc(&c, "a.md", &vec!["123456789".to_string(); 100]);
    edit(&mut c, &a);
    install(&mut c, &socket, false, &[], None);
    let a_buf = buf_of(&mut c, &a);
    let text = |c: &mut Case, max_bytes: u64, max_lines: u64| {
        let opts = opts_map(&[
            ("owner", OWNER.into()),
            ("buf", a_buf.into()),
            ("max_bytes", max_bytes.into()),
            ("max_lines", max_lines.into()),
        ]);
        op(c, "text", opts)
    };

    let big = text(&mut c, 500, 50_000);
    assert_eq!(field(&big, "too_large"), &Value::from(true));
    assert_eq!(field(&big, "bytes"), &Value::from(1000));
    assert_eq!(field(&big, "lines"), &Value::from(100), "a count, not the lines");
    let long = text(&mut c, 4_194_304, 50);
    assert_eq!(field(&long, "too_large"), &Value::from(true));

    let whole = text(&mut c, 4_194_304, 50_000);
    assert_eq!(field(&whole, "lines").as_array().map(Vec::len), Some(100));
    let tick = c.nvim.lua(
        "return vim.api.nvim_buf_get_var(..., 'changedtick')",
        vec![Value::from(a_buf)],
    );
    assert_eq!(field(&whole, "tick"), &tick);
    assert_eq!(field(&whole, "buf"), &Value::from(a_buf));

    c.nvim.command(&format!("bwipe {}", a.display()));
    let gone = text(&mut c, 4_194_304, 50_000);
    assert_eq!(field(&gone, "gone"), &Value::from(true));
    assert_eq!(field(&gone, "buf"), &Value::from(a_buf));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_auto_watch_reports_each_markdown_buffer_once_until_another_one() {
    let (mut c, listener, socket) = follow_case("auto");
    let a = write_doc(&c, "a.md", &numbered(5));
    let b = write_doc(&c, "b.md", &numbered(5));
    let x = write_doc(&c, "x.lua", &["return 1".to_string()]);
    install(&mut c, &socket, true, &[], None);
    drain(&mut c, &listener);

    edit(&mut c, &a);
    let a_buf = buf_of(&mut c, &a);
    assert_eq!(
        lines_within(&mut c.nvim, &listener, SETTLE),
        vec![serde_json::json!({"v": 1, "k": "cmd", "owner": "demo", "cmd": "auto", "buf": a_buf})]
    );
    edit(&mut c, &x);
    assert!(lines_within(&mut c.nvim, &listener, SETTLE).is_empty());
    edit(&mut c, &a);
    assert!(
        lines_within(&mut c.nvim, &listener, SETTLE).is_empty(),
        "the same buffer again is not reported"
    );
    edit(&mut c, &b);
    let b_buf = buf_of(&mut c, &b);
    assert_eq!(
        lines_within(&mut c.nvim, &listener, SETTLE),
        vec![serde_json::json!({"v": 1, "k": "cmd", "owner": "demo", "cmd": "auto", "buf": b_buf})]
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_wiped_followed_buffer_sends_gone() {
    let (mut c, listener, socket) = follow_case("gone");
    let a = write_doc(&c, "a.md", &numbered(5));
    let x = write_doc(&c, "x.lua", &["return 1".to_string()]);
    edit(&mut c, &a);
    let a_buf = buf_of(&mut c, &a);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    edit(&mut c, &x);
    drain(&mut c, &listener);
    c.nvim.command(&format!("bwipe {}", a.display()));
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    assert!(
        lines.contains(&serde_json::json!({"v": 1, "k": "gone", "owner": "demo", "buf": a_buf})),
        "{lines:?}"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_unloaded_followed_buffer_is_kept_and_followed_again_when_loaded() {
    let (mut c, listener, socket) = follow_case("unload");
    let a = write_doc(&c, "a.md", &numbered(50));
    let x = write_doc(&c, "x.lua", &["return 1".to_string()]);
    c.nvim.command("set nohidden");
    edit(&mut c, &a);
    let a_buf = buf_of(&mut c, &a);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    drain(&mut c, &listener);
    // Without 'hidden', leaving a.md unloads it; its handle stays.
    edit(&mut c, &x);
    c.nvim.command("normal! G");
    let state = c.nvim.lua(
        "local b = ... return { vim.api.nvim_buf_is_valid(b), vim.api.nvim_buf_is_loaded(b) }",
        vec![Value::from(a_buf)],
    );
    assert_eq!(state, Value::Array(vec![true.into(), false.into()]));
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    assert!(lines.iter().all(|l| l["k"] != "gone"), "{lines:?}");
    edit(&mut c, &a);
    assert_eq!(buf_of(&mut c, &a), a_buf, "the same handle comes back");
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    assert!(
        views(&lines).iter().any(|l| l["buf"] == a_buf && l["current"] == true),
        "{lines:?}"
    );

    // With 'hidden', an explicit :bunload of the followed buffer is kept the same way.
    c.nvim.command("set hidden");
    edit(&mut c, &x);
    drain(&mut c, &listener);
    c.nvim.command(&format!("bunload {}", a.display()));
    c.nvim.command("normal! gg");
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    assert!(lines.iter().all(|l| l["k"] != "gone"), "{lines:?}");
    // A wipe still ends it.
    c.nvim.command(&format!("bwipe {}", a.display()));
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    assert!(
        lines.contains(&serde_json::json!({"v": 1, "k": "gone", "owner": "demo", "buf": a_buf})),
        "{lines:?}"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_loaded_followed_buffer_with_no_window_still_reports_its_edits() {
    let (mut c, listener, socket) = follow_case("no-window");
    let a = write_doc(&c, "a.md", &numbered(30));
    let x = write_doc(&c, "x.lua", &["return 1".to_string()]);
    c.nvim.command("set hidden");
    edit(&mut c, &a);
    let a_buf = buf_of(&mut c, &a);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    c.nvim.command("normal! 10G");
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let shown = views(&lines)
        .last()
        .copied()
        .cloned()
        .expect("a view while a window shows it");
    assert_eq!(shown["current"], true, "{lines:?}");

    // The code buffer takes a.md's only window; a.md stays loaded.
    edit(&mut c, &x);
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let left = views(&lines);
    assert_eq!(left.len(), 1, "one view says it is no longer current: {lines:?}");
    assert_eq!(left[0]["buf"], a_buf);
    assert_eq!(left[0]["current"], false);
    assert_eq!(left[0]["top"], shown["top"], "the last position stands");
    assert_eq!(left[0]["tick"], shown["tick"]);
    for key in ["rows", "cursor_row"] {
        assert!(shown[key].is_u64(), "the window reported {key}");
        assert_eq!(left[0][key], shown[key], "the last {key} stands without a window");
    }

    // Moving about in the code buffer says nothing new about a.md.
    c.nvim.command("normal! G");
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    assert!(views(&lines).is_empty(), "{lines:?}");

    // An edit with no window and no view event still moves the tick the host pulls on.
    c.nvim.lua(
        "local b = ... vim.api.nvim_buf_set_lines(b, 0, 1, false, { '# changed' })",
        vec![Value::from(a_buf)],
    );
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    let edited = views(&lines);
    assert_eq!(edited.len(), 1, "{lines:?}");
    assert_eq!(edited[0]["buf"], a_buf);
    assert_eq!(edited[0]["current"], false);
    assert!(edited[0]["tick"].as_u64() > shown["tick"].as_u64(), "{lines:?}");

    // Once a.md is followed no more, its edits send nothing.
    let b = write_doc(&c, "b.md", &numbered(5));
    edit(&mut c, &b);
    drain(&mut c, &listener);
    c.nvim.lua(
        "local b = ... vim.api.nvim_buf_set_lines(b, 0, 1, false, { '# again' })",
        vec![Value::from(a_buf)],
    );
    let lines = lines_within(&mut c.nvim, &listener, SETTLE);
    assert!(views(&lines).iter().all(|l| l["buf"] != a_buf), "{lines:?}");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn views_are_throttled_to_one_per_40_ms_and_the_last_one_is_sent() {
    let (mut c, listener, socket) = follow_case("throttle");
    let a = write_doc(&c, "a.md", &numbered(100));
    edit(&mut c, &a);
    install(&mut c, &socket, false, &[], None);
    follow(&mut c, true);
    drain(&mut c, &listener);
    c.nvim.lua(
        "for i = 1, 20 do \
           vim.api.nvim_win_set_cursor(0, { i, 0 }) \
           vim.api.nvim_exec_autocmds('CursorMoved', {}) \
         end",
        vec![],
    );
    let lines = lines_within(&mut c.nvim, &listener, Duration::from_millis(200));
    let seen = views(&lines);
    assert!(!seen.is_empty() && seen.len() <= 3, "{lines:?}");
    assert_eq!(seen.last().unwrap()["cursor"]["line"], 20, "{lines:?}");
}

fn cwd_call(c: &mut Case, op: &str, dir: Option<&Path>) -> Value {
    let mut opts = vec![("owner", Value::from("demo"))];
    if let Some(dir) = dir {
        opts.push(("dir", Value::Binary(dir.as_os_str().as_bytes().to_vec())));
    }
    c.nvim.lua(PLUGIN_CWD_LUA, vec![op.into(), opts_map(&opts)])
}

fn tab_cwd(c: &mut Case) -> Value {
    c.nvim.lua(
        "return {dir = vim.fn.getcwd(-1, 0), localdir = vim.fn.haslocaldir(-1, 0)}",
        vec![],
    )
}

fn tcd(c: &mut Case, dir: &Path) {
    c.nvim.lua(
        "vim.api.nvim_cmd({cmd = 'tcd', args = {...}, magic = {file = false}}, {})",
        vec![s(dir)],
    );
}

#[test]
#[ignore = "runs a real headless nvim"]
fn editor_cd_sets_the_tabs_directory() {
    let mut c = case("cwd-set");
    let root = c.root.clone();
    let tab = c.nvim.lua("return vim.api.nvim_get_current_tabpage()", vec![]);
    let reply = cwd_call(&mut c, "cd", Some(&root));
    assert_eq!(field(&reply, "dir"), &s(&root));
    assert_eq!(field(&reply, "tab"), &tab);
    let cwd = tab_cwd(&mut c);
    assert_eq!(field(&cwd, "dir"), &s(&root));
    assert_eq!(field(&cwd, "localdir").as_u64(), Some(1));
    let state = c.nvim.lua("return vim.t.eitri_plugin_cwd_demo", vec![]);
    assert_eq!(field(&state, "baseline_local").as_u64(), Some(0));
    assert_eq!(field(&state, "baseline_dir"), &s(&c.dir));
    assert_eq!(field(&state, "last_set"), &s(&root));
}

#[test]
#[ignore = "runs a real headless nvim"]
fn clear_restores_a_tab_local_baseline() {
    let mut c = case("cwd-local");
    let baseline = c.dir.clone();
    tcd(&mut c, &baseline);
    let root = c.root.clone();
    cwd_call(&mut c, "cd", Some(&root));
    // Repeated changes keep the first baseline.
    let other = c.dir.join("work");
    cwd_call(&mut c, "cd", Some(&other));
    let reply = cwd_call(&mut c, "clear", None);
    assert_eq!(field(&reply, "restored").as_u64(), Some(1));
    assert_eq!(field(&reply, "moved").as_u64(), Some(0));
    let cwd = tab_cwd(&mut c);
    assert_eq!(field(&cwd, "dir"), &s(&baseline));
    assert_eq!(field(&cwd, "localdir").as_u64(), Some(1));
    assert!(c.nvim.lua("return vim.t.eitri_plugin_cwd_demo", vec![]).is_nil());
    assert_eq!(field(&cwd_call(&mut c, "clear", None), "restored").as_u64(), Some(0));
}

#[test]
#[ignore = "runs a real headless nvim"]
fn clear_restores_a_global_baseline_with_tcd_of_the_global_cwd() {
    let mut c = case("cwd-global");
    let root = c.root.clone();
    cwd_call(&mut c, "cd", Some(&root));
    // Another tab changes the global directory while the plugin's local one stays put.
    c.nvim.command("tabnew");
    c.nvim.lua(
        "vim.api.nvim_cmd({cmd = 'cd', args = {...}, magic = {file = false}}, {})",
        vec![s(&root)],
    );
    let reply = cwd_call(&mut c, "clear", None);
    assert_eq!(field(&reply, "restored").as_u64(), Some(1));
    assert_eq!(field(&reply, "moved").as_u64(), Some(0));
    c.nvim.command("tabprevious");
    let cwd = tab_cwd(&mut c);
    assert_eq!(field(&cwd, "dir"), &s(&root), "restore the global cwd as it stands now");
    assert_eq!(field(&cwd, "localdir").as_u64(), Some(1));
}

#[test]
#[ignore = "runs a real headless nvim"]
fn clear_leaves_a_tab_the_user_moved_and_counts_it() {
    let mut c = case("cwd-moved");
    let root = c.root.clone();
    cwd_call(&mut c, "cd", Some(&root));
    let moved = c.dir.join("work");
    tcd(&mut c, &moved);
    let reply = cwd_call(&mut c, "clear", None);
    assert_eq!(field(&reply, "restored").as_u64(), Some(0));
    assert_eq!(field(&reply, "moved").as_u64(), Some(1));
    assert_eq!(field(&tab_cwd(&mut c), "dir"), &s(&moved));
    assert!(c.nvim.lua("return vim.t.eitri_plugin_cwd_demo", vec![]).is_nil());
}

/// The process directory, `getcwd()` and the tab's own directory as the current tab sees them.
fn current_cwds(c: &mut Case) -> Value {
    c.nvim.lua(
        "return {uv = vim.uv.cwd(), plain = vim.fn.getcwd(), tab = vim.fn.getcwd(-1, 0)}",
        vec![],
    )
}

#[test]
#[ignore = "runs a real headless nvim"]
fn editor_cd_from_an_owners_diff_tab_acts_in_its_origin_tab() {
    // Neither tab has a local directory, so the diff tab's directory is the process's own: a change made
    // from it must not move the process directory under it.
    let mut c = case("cwd-diff");
    let origin = c.nvim.lua("return vim.api.nvim_get_current_tabpage()", vec![]);
    c.nvim.command("tabnew");
    let diff = c.nvim.lua(
        "vim.t.eitri_plugin_diff = 'demo'; vim.t.eitri_plugin_diff_from = ...; return vim.api.nvim_get_current_tabpage()",
        vec![origin.clone()],
    );
    let before = current_cwds(&mut c);
    let old = s(&c.dir);
    assert_eq!(field(&before, "uv"), &old);
    let root = c.root.clone();
    let reply = cwd_call(&mut c, "cd", Some(&root));
    assert_eq!(field(&reply, "tab"), &origin);
    assert_eq!(
        c.nvim.lua("return vim.api.nvim_get_current_tabpage()", vec![]),
        origin,
        "the change is made in the origin tab, which is left selected"
    );
    let now = current_cwds(&mut c);
    for key in ["uv", "plain", "tab"] {
        assert_eq!(field(&now, key), &s(&root), "{key} in the origin tab");
    }
    assert_eq!(
        c.nvim.lua(
            "return vim.fn.getcwd(-1, vim.api.nvim_tabpage_get_number(...))",
            vec![diff.clone()]
        ),
        old,
        "the diff tab keeps the old directory"
    );
    c.nvim.lua("vim.api.nvim_set_current_tabpage(...)", vec![diff]);
    assert_eq!(
        current_cwds(&mut c),
        before,
        "the diff tab, entered again, is untouched"
    );
}

#[test]
#[ignore = "runs a real headless nvim"]
fn a_dir_with_a_newline_or_backslash_is_passed_as_an_argument() {
    let mut c = case("cwd-argument");
    for name in ["line\nbreak", r"back\slash | let g:injected = 1"] {
        let dir = c.root.join(name);
        std::fs::create_dir(&dir).unwrap();
        cwd_call(&mut c, "cd", Some(&dir));
        assert_eq!(field(&tab_cwd(&mut c), "dir"), &s(&dir));
        assert_eq!(c.nvim.lua("return vim.g.injected == nil", vec![]), Value::from(true));
    }
}

#[test]
#[ignore = "runs a real headless nvim"]
fn clear_visits_every_owned_tab_and_leaves_the_current_tab_selected() {
    let mut c = case("cwd-tabs");
    let baseline = c.dir.clone();
    let root = c.root.clone();
    cwd_call(&mut c, "cd", Some(&root));
    c.nvim.command("tabnew");
    cwd_call(&mut c, "cd", Some(&root));
    tcd(&mut c, &baseline);
    let selected = c.nvim.lua("return vim.api.nvim_get_current_tabpage()", vec![]);
    let reply = cwd_call(&mut c, "clear", None);
    assert_eq!(field(&reply, "restored").as_u64(), Some(1));
    assert_eq!(field(&reply, "moved").as_u64(), Some(1));
    assert_eq!(
        c.nvim.lua("return vim.api.nvim_get_current_tabpage()", vec![]),
        selected
    );
    assert_eq!(c.nvim.lua("for _, t in ipairs(vim.api.nvim_list_tabpages()) do if pcall(vim.api.nvim_tabpage_get_var, t, 'eitri_plugin_cwd_demo') then return false end end return true", vec![]), Value::from(true));
}

#[test]
#[ignore = "runs a real headless nvim"]
fn editor_cd_overtaken_by_its_clear_cannot_change_the_directory() {
    let mut c = case("cwd-order");
    let root = c.root.clone();
    let before = tab_cwd(&mut c);
    let opts = opts_map(&[("owner", "demo".into()), ("maker", "test".into()), ("order", 2.into())]);
    c.nvim.lua(PLUGIN_CWD_LUA, vec!["clear".into(), opts]);
    let opts = opts_map(&[
        ("owner", "demo".into()),
        ("maker", "test".into()),
        ("order", 1.into()),
        ("dir", s(&root)),
    ]);
    c.nvim.lua(PLUGIN_CWD_LUA, vec!["cd".into(), opts]);
    assert_eq!(tab_cwd(&mut c), before);
    assert!(c.nvim.lua("return vim.t.eitri_plugin_cwd_demo", vec![]).is_nil());
}

#[test]
#[ignore = "runs a real headless nvim"]
fn editor_cd_refuses_a_retargeted_directory_before_recording_a_baseline() {
    let mut c = case("cwd-retargeted");
    let dir = c.root.join("replaced");
    std::fs::create_dir(&dir).unwrap();
    std::fs::remove_dir(&dir).unwrap();
    std::os::unix::fs::symlink(&c.root, &dir).unwrap();
    let before = tab_cwd(&mut c);
    let result = c.nvim.lua(
        "local src, dir = ... local ok, err = pcall(assert(loadstring(src)), 'cd', {owner = 'demo', dir = dir}) return {ok = ok, err = tostring(err)}",
        vec![PLUGIN_CWD_LUA.into(), s(&dir)],
    );
    assert_eq!(field(&result, "ok"), &Value::from(false));
    assert!(field(&result, "err")
        .as_str()
        .unwrap()
        .contains("no longer resolves to itself"));
    assert_eq!(tab_cwd(&mut c), before);
    assert!(c.nvim.lua("return vim.t.eitri_plugin_cwd_demo", vec![]).is_nil());
}

#[test]
#[ignore = "runs a real headless nvim"]
fn editor_cd_from_a_diff_whose_origin_is_gone_acts_in_that_tab() {
    let mut c = case("cwd-gone-origin");
    let root = c.root.clone();
    let tab = c.nvim.lua(
        "vim.t.eitri_plugin_diff = 'demo'; vim.t.eitri_plugin_diff_from = 999999; return vim.api.nvim_get_current_tabpage()",
        vec![],
    );
    let reply = cwd_call(&mut c, "cd", Some(&root));
    assert_eq!(field(&reply, "tab"), &tab);
    assert_eq!(c.nvim.lua("return vim.api.nvim_get_current_tabpage()", vec![]), tab);
    let cwd = tab_cwd(&mut c);
    assert_eq!(field(&cwd, "dir"), &s(&root));
    assert_eq!(field(&cwd, "localdir").as_u64(), Some(1));
}

#[test]
#[ignore = "runs a real headless nvim"]
fn clear_goes_on_past_a_baseline_that_no_longer_resolves() {
    let mut c = case("cwd-baseline-gone");
    let root = c.root.clone();
    let gone = c.dir.join("gone");
    std::fs::create_dir(&gone).unwrap();
    tcd(&mut c, &gone);
    cwd_call(&mut c, "cd", Some(&root));
    // A new tab inherits the tab-local directory; this one starts from its own.
    c.nvim.command("tabnew");
    let kept = c.dir.clone();
    tcd(&mut c, &kept);
    cwd_call(&mut c, "cd", Some(&root));
    std::fs::remove_dir(&gone).unwrap();
    let reply = cwd_call(&mut c, "clear", None);
    assert_eq!(field(&reply, "restored").as_u64(), Some(1), "{reply:?}");
    assert_eq!(field(&reply, "moved").as_u64(), Some(1), "{reply:?}");
    assert_eq!(
        field(&tab_cwd(&mut c), "dir"),
        &s(&c.dir),
        "the later tab is still restored"
    );
    assert_eq!(c.nvim.lua("for _, t in ipairs(vim.api.nvim_list_tabpages()) do if pcall(vim.api.nvim_tabpage_get_var, t, 'eitri_plugin_cwd_demo') then return false end end return true", vec![]), Value::from(true));
    c.nvim.command("tabprevious");
    assert_eq!(
        field(&tab_cwd(&mut c), "dir"),
        &s(&root),
        "the unrestorable tab stays where it is"
    );
}

#[test]
#[ignore = "runs a real headless nvim"]
fn clear_leaves_a_tab_the_user_moved_with_a_global_cd() {
    let mut c = case("cwd-global-cd");
    let root = c.root.clone();
    cwd_call(&mut c, "cd", Some(&root));
    // A global :cd in the tab drops its tab-local directory, even to the same place.
    c.nvim.lua(
        "vim.api.nvim_cmd({cmd = 'cd', args = {...}, magic = {file = false}}, {})",
        vec![s(&root)],
    );
    c.nvim.command("tabnew");
    let reply = cwd_call(&mut c, "clear", None);
    assert_eq!(field(&reply, "restored").as_u64(), Some(0));
    assert_eq!(field(&reply, "moved").as_u64(), Some(1));
    assert_eq!(field(&current_cwds(&mut c), "uv"), &s(&root));
    c.nvim.command("tabprevious");
    let cwd = tab_cwd(&mut c);
    assert_eq!(field(&cwd, "dir"), &s(&root));
    assert_eq!(
        field(&cwd, "localdir").as_u64(),
        Some(0),
        "no tab-local directory is put back"
    );
}

fn buffers(c: &mut Case, op: &str, pairs: &[(&str, Value)]) -> Value {
    let mut args = vec![("owner", Value::from("demo"))];
    args.extend_from_slice(pairs);
    c.nvim.lua(
        eitri_core::plugin::editor::PLUGIN_BUFFERS_LUA,
        vec![op.into(), opts_map(&args)],
    )
}

fn preload(c: &mut Case, path: &Path, diagnostics: bool, hints: bool) -> Value {
    buffers(
        c,
        "preload",
        &[
            ("path", s(path)),
            ("diagnostics", diagnostics.into()),
            ("inlay_hints", hints.into()),
        ],
    )
}

fn wipe(c: &mut Case) -> Value {
    buffers(c, "wipe_preload", &[])
}

fn preload_server(c: &mut Case) {
    c.nvim.lua(
        "vim.api.nvim_create_autocmd('FileType', { pattern = 'rust', callback = function(ev) \
         local seq = 0 local closing = false \
         vim.lsp.start({ name = 'fake-preload', root_dir = vim.fs.dirname(vim.api.nvim_buf_get_name(ev.buf)), \
           cmd = function(dispatchers) return { \
           request = function(method, _, cb) seq = seq + 1 \
             if method == 'initialize' then vim.schedule(function() cb(nil, { capabilities = { inlayHintProvider = true } }) end) \
             elseif method == 'shutdown' then vim.schedule(function() cb(nil, nil) end) end \
             return true, seq end, \
           notify = function(method) if method == 'exit' then closing = true vim.schedule(function() dispatchers.on_exit(0, 0) end) end return true end, \
           is_closing = function() return closing end, terminate = function() closing = true dispatchers.on_exit(0, 0) end } end \
           }, { bufnr = ev.buf, reuse_client = function() return false end }) end })",
        vec![],
    );
}

fn wait_preload_server(c: &mut Case, buf: &Value) -> Value {
    c.nvim.lua(
        "local b = ... assert(vim.wait(2000, function() return #vim.lsp.get_clients({bufnr=b}) > 0 end)) \
         test_timers.advance(600) return vim.lsp.get_clients({bufnr=b})[1].id",
        vec![buf.clone()],
    )
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn preload_loads_a_hidden_buffer_and_marks_it() {
    let mut c = case("preload-hidden");
    let path = c.a.clone();
    let result = preload(&mut c, &path, false, false);
    let buf = field(&result, "buf").clone();
    let got = c.nvim.lua(
        "local b = ... return {vim.api.nvim_buf_is_loaded(b), #vim.fn.win_findbuf(b), vim.b[b].eitri_plugin_preload}",
        vec![buf],
    );
    assert_eq!(got, Value::Array(vec![true.into(), 0.into(), "demo".into()]));
    assert_eq!(field(&result, "replaced"), &false.into());
    assert_eq!(state(&mut c, &path), (false, false));
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn preload_attaches_the_fake_language_server() {
    let mut c = case("preload-attach");
    preload_server(&mut c);
    let path = c.a.clone();
    let result = preload(&mut c, &path, false, false);
    let buf = field(&result, "buf");
    let id = wait_preload_server(&mut c, buf);
    let got = c.nvim.lua(
        "local b = ... return {vim.bo[b].filetype, vim.b[b].eitri_plugin_preload_clients}",
        vec![buf.clone()],
    );
    assert_eq!(got, Value::Array(vec!["rust".into(), Value::Array(vec![id])]));
    assert_eq!(state(&mut c, &path), (false, false));
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn a_second_preload_wipes_the_first_if_still_hidden_unmodified_and_marked() {
    let mut c = case("preload-replace");
    let (a, b) = (c.a.clone(), c.b.clone());
    let first = preload(&mut c, &a, true, false);
    let next = preload(&mut c, &b, true, false);
    assert_eq!(field(&next, "replaced"), &true.into());
    assert_eq!(
        c.nvim.lua(
            "return vim.api.nvim_buf_is_valid(...)",
            vec![field(&first, "buf").clone()]
        ),
        false.into()
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn an_opened_preload_keeps_its_buffer_and_is_never_wiped() {
    let mut c = case("preload-open");
    let (a, b) = (c.a.clone(), c.b.clone());
    let first = preload(&mut c, &a, true, false);
    c.nvim.lua(PLUGIN_OPEN_LUA, vec![opts_map(&[("path", s(&a))])]);
    // Even after the user hides it again, opening surrendered the preload's right to wipe it.
    c.nvim.command("enew");
    let next = preload(&mut c, &b, true, false);
    assert_eq!(field(&next, "replaced"), &false.into());
    assert_eq!(
        c.nvim.lua(
            "local b = ... return {vim.api.nvim_buf_is_valid(b), vim.b[b].eitri_plugin_preload == nil}",
            vec![field(&first, "buf").clone()]
        ),
        Value::Array(vec![true.into(), true.into()])
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn a_modified_preload_is_not_wiped() {
    let mut c = case("preload-modified");
    let (a, b) = (c.a.clone(), c.b.clone());
    let first = preload(&mut c, &a, true, false);
    c.nvim.lua(
        "vim.api.nvim_buf_set_lines(..., 0, -1, false, {'unsaved'})",
        vec![field(&first, "buf").clone()],
    );
    let next = preload(&mut c, &b, true, false);
    assert_eq!(field(&next, "replaced"), &false.into());
    assert_eq!(
        c.nvim.lua(
            "return vim.api.nvim_buf_get_lines(..., 0, -1, false)",
            vec![field(&first, "buf").clone()]
        ),
        Value::Array(vec!["unsaved".into()])
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn assist_on_copy_a_then_preload_of_b_leaves_a_and_the_assist_record_unchanged() {
    let mut c = case("preload-assist-independent");
    let a = c.a.clone();
    let other = c.dir.join("copy-b");
    std::fs::create_dir(&other).unwrap();
    let b = other.join("lib.rs");
    std::fs::write(&b, "fn main() {}\n").unwrap();
    edit(&mut c, &a);
    attach(&mut c, &a);
    apply(&mut c, "demo", false, false);
    let before = c.nvim.lua("return vim.inspect(_G.__eitri_plugin_assist)", vec![]);
    // A server for B, as a preload exists to start one: hints are held only once one is attached.
    preload_server(&mut c);
    let result = preload(&mut c, &b, true, true);
    wait_preload_server(&mut c, field(&result, "buf"));
    assert_eq!(
        c.nvim.lua("return vim.inspect(_G.__eitri_plugin_assist)", vec![]),
        before
    );
    assert_eq!(state(&mut c, &a), (false, false));
    assert_eq!(state(&mut c, &b), (true, true));
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn assist_on_a_preloaded_buffer_takes_its_own_values_from_the_preload_record() {
    let mut c = case("preload-baseline");
    preload_server(&mut c);
    let a = c.a.clone();
    c.nvim.lua("local b = vim.fn.bufadd(...) vim.diagnostic.enable(true, {bufnr=b}) vim.lsp.inlay_hint.enable(true, {bufnr=b})", vec![s(&a)]);
    let result = preload(&mut c, &a, false, false);
    wait_preload_server(&mut c, field(&result, "buf"));
    apply(&mut c, "demo", false, false);
    clear(&mut c, "demo");
    c.nvim.lua("test_timers.advance(1100)", vec![]);
    assert_eq!(state(&mut c, &a), (true, true));
    let got = c.nvim.lua("local b = ... return {vim.b[b].eitri_plugin_preload_own == nil, not pcall(vim.api.nvim_get_autocmds, {group='eitri_plugin_preload_demo'})}", vec![field(&result, "buf").clone()]);
    assert_eq!(got, Value::Array(vec![true.into(), true.into()]));
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn a_preload_takes_the_hint_value_its_configuration_sets_when_a_server_attaches() {
    let mut c = case("preload-config-hints");
    // A configuration that turns hints on for every buffer a server attaches to.
    c.nvim.lua(
        "vim.api.nvim_create_autocmd('LspAttach', { callback = function(ev) \
           vim.lsp.inlay_hint.enable(true, { bufnr = ev.buf }) end })",
        vec![],
    );
    preload_server(&mut c);
    let a = c.a.clone();
    let result = preload(&mut c, &a, false, false);
    wait_preload_server(&mut c, field(&result, "buf"));
    assert_eq!(
        state(&mut c, &a),
        (false, false),
        "the preload's policy holds after the attach"
    );
    apply(&mut c, "demo", false, false);
    clear(&mut c, "demo");
    c.nvim.lua("test_timers.advance(1100)", vec![]);
    assert_eq!(
        state(&mut c, &a),
        (true, true),
        "clear restores what the configuration wanted after the attach, not the unattached read"
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn a_wipe_stops_the_clients_only_its_preload_started() {
    let mut c = case("preload-client-stop");
    let outside = c.outside.clone();
    edit(&mut c, &outside);
    attach(&mut c, &outside);
    preload_server(&mut c);
    let a = c.a.clone();
    let result = preload(&mut c, &a, true, false);
    let id = wait_preload_server(&mut c, field(&result, "buf"));
    assert_eq!(field(&wipe(&mut c), "stopped_clients"), &1.into());
    let gone = c.nvim.lua(
        "local id = ... return vim.wait(2000, function() return vim.lsp.get_client_by_id(id) == nil end)",
        vec![id],
    );
    assert_eq!(gone, true.into());
    assert_eq!(
        c.nvim.lua(
            "return #vim.lsp.get_clients({bufnr=vim.fn.bufnr(...)})",
            vec![s(&outside)]
        ),
        1.into()
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn a_wipe_keeps_a_client_another_buffer_holds() {
    let mut c = case("preload-client-shared");
    preload_server(&mut c);
    let (a, b) = (c.a.clone(), c.outside.clone());
    let result = preload(&mut c, &a, true, false);
    let id = wait_preload_server(&mut c, field(&result, "buf"));
    c.nvim.lua(
        "local p, id = ... local b = vim.fn.bufadd(p) vim.fn.bufload(b) assert(vim.lsp.buf_attach_client(b, id))",
        vec![s(&b), id.clone()],
    );
    assert_eq!(field(&wipe(&mut c), "stopped_clients"), &0.into());
    assert_eq!(
        c.nvim.lua("return vim.lsp.get_client_by_id(...) ~= nil", vec![id]),
        true.into()
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn stop_wipes_an_only_preloaded_buffer_and_stops_its_clients() {
    let mut c = case("preload-stop");
    preload_server(&mut c);
    let a = c.a.clone();
    let result = preload(&mut c, &a, true, false);
    let id = wait_preload_server(&mut c, field(&result, "buf"));
    let got = wipe(&mut c);
    assert_eq!(field(&got, "replaced"), &true.into());
    assert_eq!(field(&got, "stopped_clients"), &1.into());
    assert_eq!(
        c.nvim.lua(
            "return vim.api.nvim_buf_is_valid(...)",
            vec![field(&result, "buf").clone()]
        ),
        false.into()
    );
    assert_eq!(
        c.nvim.lua(
            "local id = ... return vim.wait(2000, function() return vim.lsp.get_client_by_id(id) == nil end)",
            vec![id]
        ),
        true.into()
    );
    assert_eq!(field(&wipe(&mut c), "replaced"), &false.into());
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn modified_lists_unsaved_buffers_under_root_only() {
    let mut c = case("buffers-modified");
    let (a, outside, root) = (c.a.clone(), c.outside.clone(), c.root.clone());
    for path in [&a, &outside] {
        c.nvim.lua(
            "local b = vim.fn.bufadd(...) vim.fn.bufload(b) vim.api.nvim_buf_set_lines(b, 0, -1, false, {'unsaved'})",
            vec![s(path)],
        );
    }
    assert_eq!(
        field(&buffers(&mut c, "modified", &[("root", s(&root))]), "paths"),
        &Value::Array(vec![s(&a)])
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn reload_reloads_unmodified_changed_buffers_and_counts_kept_modified() {
    let mut c = case("buffers-reload");
    let (a, b, outside, root) = (c.a.clone(), c.b.clone(), c.outside.clone(), c.root.clone());
    let unchanged = root.join("unchanged.rs");
    std::fs::write(&unchanged, "unchanged\n").unwrap();
    c.nvim.lua("vim.fn.bufload(vim.fn.bufadd(...))", vec![s(&unchanged)]);
    for path in [&a, &b, &outside] {
        c.nvim.lua("vim.fn.bufload(vim.fn.bufadd(...))", vec![s(path)]);
        std::fs::write(path, "disk changed\n").unwrap();
    }
    c.nvim.lua(
        "vim.api.nvim_buf_set_lines(vim.fn.bufnr(...), 0, -1, false, {'unsaved'})",
        vec![s(&b)],
    );
    c.nvim.lua("vim.bo[vim.fn.bufnr(...)].autoread = false", vec![s(&a)]);
    let got = buffers(&mut c, "reload", &[("root", s(&root))]);
    assert_eq!(field(&got, "reloaded"), &1.into());
    assert_eq!(field(&got, "kept_modified"), &1.into());
    assert_eq!(
        c.nvim.lua("return vim.bo[vim.fn.bufnr(...)].autoread", vec![s(&a)]),
        false.into()
    );
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "disk changed\n");
    for (path, want) in [(&a, "disk changed"), (&b, "unsaved"), (&outside, "out")] {
        assert_eq!(
            c.nvim.lua(
                "return vim.api.nvim_buf_get_lines(vim.fn.bufnr(...), 0, 1, false)[1]",
                vec![s(path)]
            ),
            want.into()
        );
    }
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn reload_reloads_a_buffer_opened_through_a_symlink_to_a_file_under_the_root() {
    let mut c = case("buffers-reload-symlink");
    let root = c.root.clone();
    let file = root.join("linked.rs");
    std::fs::write(&file, "before\n").unwrap();
    // nvim names a buffer opened through a symlinked file by the link, not by the file it points to.
    let through = c.dir.join("linked.rs");
    std::os::unix::fs::symlink(&file, &through).unwrap();
    c.nvim.lua("vim.fn.bufload(vim.fn.bufadd(...))", vec![s(&through)]);
    std::fs::write(&file, "disk changed\n").unwrap();
    let got = buffers(&mut c, "reload", &[("root", s(&root))]);
    assert_eq!(
        field(&got, "reloaded"),
        &1.into(),
        "the file under the root is the same file: {got:?}"
    );
    assert_eq!(
        c.nvim.lua(
            "return vim.api.nvim_buf_get_lines(vim.fn.bufnr(...), 0, 1, false)[1]",
            vec![s(&through)]
        ),
        "disk changed".into()
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn a_wipe_leaves_a_preexisting_client_running_even_without_another_buffer() {
    let mut c = case("preload-client-preexisting");
    let (a, outside) = (c.a.clone(), c.outside.clone());
    edit(&mut c, &outside);
    attach(&mut c, &outside);
    c.nvim.lua(
        "local p = ... local id = vim.lsp.get_clients({bufnr=vim.fn.bufnr(p)})[1].id \
         _G.__preexisting = id \
         vim.api.nvim_create_autocmd('FileType', {pattern='rust', callback=function(ev) \
           assert(vim.lsp.buf_attach_client(ev.buf, id)) end})",
        vec![s(&outside)],
    );
    preload(&mut c, &a, true, false);
    c.nvim.lua(
        "local p = ... vim.lsp.buf_detach_client(vim.fn.bufnr(p), _G.__preexisting)",
        vec![s(&outside)],
    );
    assert_eq!(field(&wipe(&mut c), "stopped_clients"), &0.into());
    assert_eq!(
        c.nvim
            .lua("return vim.lsp.get_client_by_id(_G.__preexisting) ~= nil", vec![]),
        true.into()
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn a_preload_overtaken_by_its_wipe_never_loads_a_buffer() {
    let mut c = case("preload-order");
    buffers(&mut c, "wipe_preload", &[("maker", "m".into()), ("order", 2.into())]);
    let a = c.a.clone();
    buffers(
        &mut c,
        "preload",
        &[
            ("path", s(&a)),
            ("diagnostics", true.into()),
            ("inlay_hints", false.into()),
            ("maker", "m".into()),
            ("order", 1.into()),
        ],
    );
    assert_eq!(c.nvim.lua("return vim.fn.bufnr(...)", vec![s(&a)]), (-1).into());
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn modified_includes_unsaved_buffers_whose_parent_directory_was_removed() {
    let mut c = case("buffers-modified-removed");
    let parent = c.root.join("src/nested");
    std::fs::create_dir_all(&parent).unwrap();
    let file = parent.join("lib.rs");
    std::fs::write(&file, "original\n").unwrap();
    c.nvim.lua(
        "local b = vim.fn.bufadd(...) vim.fn.bufload(b) vim.api.nvim_buf_set_lines(b, 0, -1, false, {'unsaved'})",
        vec![s(&file)],
    );
    std::fs::remove_dir_all(c.root.join("src")).unwrap();
    let root = c.root.clone();
    assert_eq!(
        field(&buffers(&mut c, "modified", &[("root", s(&root))]), "paths"),
        &Value::Array(vec![s(&file)])
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn preload_rejects_a_retargeted_path_before_loading() {
    let mut c = case("preload-retargeted");
    let (a, outside) = (c.a.clone(), c.outside.clone());
    std::fs::remove_file(&a).unwrap();
    std::os::unix::fs::symlink(&outside, &a).unwrap();
    let result = c.nvim.lua(
        "local source, path = ... return pcall(assert(loadstring(source)), 'preload', \
         {owner='demo', path=path, diagnostics=true, inlay_hints=false})",
        vec![eitri_core::plugin::editor::PLUGIN_BUFFERS_LUA.into(), s(&a)],
    );
    assert_eq!(result, false.into());
    assert_eq!(c.nvim.lua("return vim.fn.bufnr(...)", vec![s(&outside)]), (-1).into());
}

fn diff_sides(c: &mut Case, left: Value, right: Value) -> Value {
    buffers(c, "diff", &[("left", left), ("right", right)])
}

fn text_side() -> Value {
    opts_map(&[
        ("title", "Attempt".into()),
        ("text", "fn main() { }\n".into()),
        ("filetype", "rust".into()),
    ])
}

fn file_side(path: &Path) -> Value {
    opts_map(&[("path", s(path))])
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn diff_of_a_file_opened_through_a_symlink_shows_that_buffer_with_its_unsaved_edits() {
    let mut c = case("diff-symlink-unsaved");
    let link = c.root.join("link.rs");
    std::os::unix::fs::symlink(&c.a, &link).unwrap();
    edit(&mut c, &link);
    let buf = c.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 0, -1, false, {'unsaved edits'}) return vim.api.nvim_get_current_buf()",
        vec![],
    );
    let a = c.a.clone();
    let result = diff_sides(&mut c, text_side(), file_side(&a));
    let got = c.nvim.lua(
        "local buf, tab = ... assert(vim.api.nvim_buf_is_valid(buf)) \
         local wins = vim.api.nvim_tabpage_list_wins(tab) \
         return {vim.api.nvim_buf_get_name(buf), vim.bo[buf].modified, \
         vim.api.nvim_buf_get_lines(buf, 0, -1, false), vim.api.nvim_win_get_buf(wins[2]) == buf}",
        vec![buf, field(&result, "tab").clone()],
    );
    assert_eq!(
        got,
        Value::Array(vec![
            s(&link),
            true.into(),
            Value::Array(vec!["unsaved edits".into()]),
            true.into(),
        ])
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn diff_builds_a_new_tab_with_two_windows_in_diff_mode_the_right_one_vertical() {
    let mut c = case("diff-layout");
    let a = c.a.clone();
    let result = diff_sides(&mut c, text_side(), file_side(&a));
    let got = c.nvim.lua(
        "local tab = ... assert(vim.api.nvim_get_current_tabpage() == tab) \
         local w = vim.api.nvim_tabpage_list_wins(tab) local p = vim.api.nvim_win_get_position(w[1]) \
         local q = vim.api.nvim_win_get_position(w[2]) return {#vim.api.nvim_list_tabpages(), #w, \
         vim.wo[w[1]].diff, vim.wo[w[2]].diff, p[1] == q[1], p[2] < q[2], vim.api.nvim_buf_get_name(vim.api.nvim_win_get_buf(w[2]))}",
        vec![field(&result, "tab").clone()],
    );
    assert_eq!(
        got,
        Value::Array(vec![
            2.into(),
            2.into(),
            true.into(),
            true.into(),
            true.into(),
            true.into(),
            s(&a)
        ])
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn the_text_side_is_nofile_unmodifiable_and_named_plugin_id_title() {
    let mut c = case("diff-text");
    let a = c.a.clone();
    diff_sides(&mut c, text_side(), file_side(&a));
    let got = c.nvim.lua(
        "local w = vim.api.nvim_tabpage_list_wins(0)[1] local b = vim.api.nvim_win_get_buf(w) \
         return {vim.fn.bufname(b), vim.bo[b].buftype, vim.bo[b].bufhidden, vim.bo[b].swapfile, \
         vim.bo[b].modifiable, vim.bo[b].filetype, vim.api.nvim_buf_get_lines(b, 0, -1, false)}",
        vec![],
    );
    assert_eq!(
        got,
        Value::Array(vec![
            "plugin:demo: Attempt".into(),
            "nofile".into(),
            "wipe".into(),
            false.into(),
            false.into(),
            "rust".into(),
            Value::Array(vec!["fn main() { }".into()])
        ])
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn a_text_side_without_a_filetype_gets_no_filetype_and_no_syntax() {
    let mut c = case("diff-no-filetype");
    let a = c.a.clone();
    let side = opts_map(&[("title", "notes.md".into()), ("text", "# a\n".into())]);
    diff_sides(&mut c, side, file_side(&a));
    let got = c.nvim.lua(
        "local b = vim.api.nvim_win_get_buf(vim.api.nvim_tabpage_list_wins(0)[1]) \
         return {vim.fn.bufname(b), vim.bo[b].filetype, vim.bo[b].syntax, vim.api.nvim_buf_get_lines(b, 0, -1, false)}",
        vec![],
    );
    assert_eq!(
        got,
        Value::Array(vec![
            "plugin:demo: notes.md".into(),
            "".into(),
            "".into(),
            Value::Array(vec!["# a".into()])
        ])
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn diffs_leave_no_empty_listed_buffers_and_list_the_file_they_show() {
    let mut c = case("diff-listed");
    let a = c.a.clone();
    let listed = "local l = {} for _, i in ipairs(vim.fn.getbufinfo({buflisted = 1})) do l[#l + 1] = i.name end \
                  table.sort(l) return l";
    let before = c.nvim.lua(listed, vec![]);
    for _ in 0..2 {
        diff_sides(&mut c, text_side(), file_side(&a));
    }
    let after = c.nvim.lua(listed, vec![]);
    c.nvim.command("tabclose");
    let closed = c.nvim.lua(listed, vec![]);
    let mut want = before.as_array().unwrap().clone();
    want.push(s(&a));
    want.sort_by(|x, y| x.as_str().cmp(&y.as_str()));
    assert_eq!(
        after,
        Value::Array(want.clone()),
        "each diff must leave only the shown file listed"
    );
    assert_eq!(
        closed,
        Value::Array(want),
        "closing a diff tab must not leave empty buffers listed"
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn a_file_side_failing_its_recheck_is_removed_though_a_longer_name_is_open() {
    let mut c = case("diff-exact-name");
    let a = c.a.clone();
    let renamed = c.root.join("renamed.rs");
    c.nvim.lua(
        "local a, renamed = ... vim.fn.bufadd(a .. '.orig') \
         vim.api.nvim_create_autocmd('BufReadPost', {pattern = a, once = true, \
           callback = function(ev) vim.api.nvim_buf_set_name(ev.buf, renamed) end})",
        vec![s(&a), s(&renamed)],
    );
    let got = c.nvim.lua(
        "local source, left, right, renamed = ... \
         local ok = pcall(assert(loadstring(source)), 'diff', {owner='demo', left=left, right=right}) \
         local left_behind = false \
         for _, b in ipairs(vim.api.nvim_list_bufs()) do \
           if vim.api.nvim_buf_get_name(b) == renamed then left_behind = true end \
         end \
         return {ok, left_behind, #vim.api.nvim_list_tabpages()}",
        vec![
            eitri_core::plugin::editor::PLUGIN_BUFFERS_LUA.into(),
            text_side(),
            file_side(&a),
            s(&renamed),
        ],
    );
    assert_eq!(
        got,
        Value::Array(vec![false.into(), false.into(), 1.into()]),
        "a buffer the diff created must go, whatever other buffer a name pattern would match"
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn the_fake_server_does_not_attach_to_the_text_side() {
    let mut c = case("diff-lsp");
    preload_server(&mut c);
    let a = c.a.clone();
    diff_sides(&mut c, text_side(), file_side(&a));
    let got = c.nvim.lua(
        "local w = vim.api.nvim_tabpage_list_wins(0) local l, r = vim.api.nvim_win_get_buf(w[1]), vim.api.nvim_win_get_buf(w[2]) \
         assert(vim.wait(2000, function() return #vim.lsp.get_clients({bufnr=r}) > 0 end)) \
         return {#vim.lsp.get_clients({bufnr=l}), #vim.lsp.get_clients({bufnr=r})}", vec![]);
    assert_eq!(got, Value::Array(vec![0.into(), 1.into()]));
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn text_with_control_characters_and_a_path_with_a_newline_are_refused() {
    let mut c = case("diff-controls");
    let bad_path = c.root.join("a\nb.rs");
    std::fs::write(&bad_path, "a").unwrap();
    let bad_text = opts_map(&[
        ("title", "Attempt".into()),
        ("text", "a\u{7}b".into()),
        ("filetype", "rust".into()),
    ]);
    for side in [bad_text, file_side(&bad_path)] {
        let got = c.nvim.lua(
            "local source, left, right = ... local ok = pcall(assert(loadstring(source)), 'diff', \
             {owner='demo', left=left, right=right}) return {ok, #vim.api.nvim_list_tabpages()}",
            vec![
                eitri_core::plugin::editor::PLUGIN_BUFFERS_LUA.into(),
                side,
                file_side(&c.a),
            ],
        );
        assert_eq!(got, Value::Array(vec![false.into(), 1.into()]));
    }
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn editor_open_from_an_owners_diff_tab_opens_in_its_origin_tab() {
    let mut c = case("diff-open-origin");
    let origin = c.nvim.lua("return vim.api.nvim_get_current_tabpage()", vec![]);
    let (a, b) = (c.a.clone(), c.b.clone());
    let tab = field(&diff_sides(&mut c, text_side(), file_side(&a)), "tab").clone();
    c.nvim.lua(
        PLUGIN_OPEN_LUA,
        vec![opts_map(&[("owner", "demo".into()), ("path", s(&b))])],
    );
    assert_eq!(c.nvim.lua("return vim.api.nvim_get_current_tabpage()", vec![]), origin);
    assert_eq!(c.nvim.lua("return vim.api.nvim_buf_get_name(0)", vec![]), s(&b));
    assert_eq!(
        c.nvim
            .lua("local t = ... return #vim.api.nvim_tabpage_list_wins(t)", vec![tab]),
        Value::from(2)
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn a_refused_open_from_an_owners_diff_tab_leaves_the_person_in_that_tab() {
    let mut c = case("diff-open-refused");
    let (a, b, outside) = (c.a.clone(), c.b.clone(), c.outside.clone());
    let tab = field(&diff_sides(&mut c, text_side(), file_side(&a)), "tab").clone();
    // The file passes the first check and becomes a link to a file outside while nvim reads it.
    c.nvim.lua(
        "local path, target = ... vim.api.nvim_create_autocmd('BufReadPost', { once = true, callback = function() \
           local uv = vim.uv or vim.loop assert(uv.fs_unlink(path)) assert(uv.fs_symlink(target, path)) end })",
        vec![s(&b), s(&outside)],
    );
    let refused = c.nvim.lua(
        "local code, path = ... return pcall(function() return assert(loadstring(code))({ owner = 'demo', path = path }) end)",
        vec![PLUGIN_OPEN_LUA.into(), s(&b)],
    );
    assert_eq!(refused, Value::from(false));
    assert_eq!(c.nvim.lua("return vim.api.nvim_get_current_tabpage()", vec![]), tab);
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn stop_closes_the_owners_diff_tabs_except_the_current_one() {
    let mut c = case("diff-close");
    let a = c.a.clone();
    let first = field(&diff_sides(&mut c, text_side(), file_side(&a)), "tab").clone();
    let current = field(&diff_sides(&mut c, text_side(), file_side(&a)), "tab").clone();
    let other = c.nvim.lua(
        eitri_core::plugin::editor::PLUGIN_BUFFERS_LUA,
        vec![
            "diff".into(),
            opts_map(&[
                ("owner", "other".into()),
                ("left", text_side()),
                ("right", file_side(&a)),
            ]),
        ],
    );
    c.nvim
        .lua("vim.api.nvim_set_current_tabpage(...)", vec![current.clone()]);
    assert_eq!(field(&buffers(&mut c, "close_diffs", &[]), "closed"), &Value::from(1));
    let got = c.nvim.lua("local a,b,c = ... return {vim.api.nvim_tabpage_is_valid(a), vim.api.nvim_tabpage_is_valid(b), vim.api.nvim_tabpage_is_valid(c), vim.api.nvim_get_current_tabpage()}", vec![first, current.clone(), field(&other, "tab").clone()]);
    assert_eq!(got, Value::Array(vec![false.into(), true.into(), true.into(), current]));
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn diff_cleanup_keeps_modified_file_buffers_and_closes_their_tabs() {
    let mut c = case("diff-modified-close");
    let origin = c.nvim.lua("return vim.api.nvim_get_current_tabpage()", vec![]);
    let a = c.a.clone();
    let tab = field(&diff_sides(&mut c, text_side(), file_side(&a)), "tab").clone();
    let buf = c.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 0, -1, false, {'unsaved'}) return vim.api.nvim_get_current_buf()",
        vec![],
    );
    c.nvim.lua("vim.api.nvim_set_current_tabpage(...)", vec![origin]);
    let result = buffers(&mut c, "close_diffs", &[]);
    assert_eq!(field(&result, "closed"), &Value::from(1));
    let got = c.nvim.lua("local t,b = ... return {vim.api.nvim_tabpage_is_valid(t), vim.api.nvim_buf_is_loaded(b), vim.bo[b].modified, vim.api.nvim_buf_get_lines(b, 0, -1, false)}", vec![tab, buf]);
    assert_eq!(
        got,
        Value::Array(vec![
            false.into(),
            true.into(),
            true.into(),
            Value::Array(vec!["unsaved".into()])
        ])
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn diff_overtaken_by_its_cleanup_never_creates_a_tab() {
    let mut c = case("diff-order");
    buffers(&mut c, "close_diffs", &[("maker", "m".into()), ("order", 2.into())]);
    let a = c.a.clone();
    buffers(
        &mut c,
        "diff",
        &[
            ("maker", "m".into()),
            ("order", 1.into()),
            ("left", text_side()),
            ("right", file_side(&a)),
        ],
    );
    assert_eq!(
        c.nvim.lua("return #vim.api.nvim_list_tabpages()", vec![]),
        Value::from(1)
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn editor_open_from_a_diff_whose_origin_is_gone_acts_in_that_tab() {
    let mut c = case("diff-open-no-origin");
    let (a, b) = (c.a.clone(), c.b.clone());
    let tab = field(&diff_sides(&mut c, text_side(), file_side(&a)), "tab").clone();
    c.nvim.command("tabclose 1");
    c.nvim.lua(
        PLUGIN_OPEN_LUA,
        vec![opts_map(&[("owner", "demo".into()), ("path", s(&b))])],
    );
    assert_eq!(c.nvim.lua("return vim.api.nvim_get_current_tabpage()", vec![]), tab);
    assert_eq!(c.nvim.lua("return vim.api.nvim_buf_get_name(0)", vec![]), s(&b));
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn diff_requests_arriving_out_of_order_both_create_tabs() {
    let mut c = case("diff-reordered");
    let a = c.a.clone();
    for order in [2, 1] {
        buffers(
            &mut c,
            "diff",
            &[
                ("maker", "m".into()),
                ("order", order.into()),
                ("left", text_side()),
                ("right", file_side(&a)),
            ],
        );
    }
    assert_eq!(
        c.nvim.lua("return #vim.api.nvim_list_tabpages()", vec![]),
        Value::from(3)
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn diff_cleanup_arriving_after_newer_diffs_leaves_them_open() {
    let mut c = case("diff-newer-than-cleanup");
    let a = c.a.clone();
    for order in [1, 3, 4] {
        buffers(
            &mut c,
            "diff",
            &[
                ("maker", "m".into()),
                ("order", order.into()),
                ("left", text_side()),
                ("right", file_side(&a)),
            ],
        );
    }
    let result = buffers(&mut c, "close_diffs", &[("maker", "m".into()), ("order", 2.into())]);
    assert_eq!(field(&result, "closed"), &Value::from(1));
    assert_eq!(
        c.nvim.lua("return #vim.api.nvim_list_tabpages()", vec![]),
        Value::from(3)
    );
}

#[test]
#[ignore = "needs real nvim; no display or tokens"]
fn diff_refuses_a_path_retargeted_by_a_read_autocommand_and_removes_its_buffer() {
    let mut c = case("diff-retargeted-on-read");
    c.nvim.lua(
        "local path, target = ... vim.api.nvim_create_autocmd('BufReadPost', {pattern='*', once=true, \
         callback=function() assert(vim.uv.fs_unlink(path)) assert(vim.uv.fs_symlink(target, path)) end})",
        vec![s(&c.a), s(&c.outside)],
    );
    let got = c.nvim.lua(
        "local source, left, right = ... local ok = pcall(assert(loadstring(source)), 'diff', \
         {owner='demo', left=left, right=right}) return {ok, #vim.api.nvim_list_tabpages(), vim.fn.bufnr(right.path)}",
        vec![
            eitri_core::plugin::editor::PLUGIN_BUFFERS_LUA.into(),
            text_side(),
            file_side(&c.a),
        ],
    );
    assert_eq!(got, Value::Array(vec![false.into(), 1.into(), (-1).into()]));
}

#[test]
#[ignore = "needs a real nvim on PATH and a listening socket; spends no tokens and needs no display"]
fn every_follow_op_without_slot_behaves_as_before() {
    let (mut c, listener, socket) = follow_case("no-slot");
    let path = write_doc(&c, "doc.md", &numbered(100));
    edit(&mut c, &path);
    c.nvim.lua("vim.bo.filetype = 'markdown'", vec![]);
    let installed = install(&mut c, &socket, false, &[("EitriFollowDemo", "show")], None);
    assert_eq!(field(&installed, "ok"), &true.into());
    follow(&mut c, true);
    let messages = lines_within(&mut c.nvim, &listener, SETTLE);
    assert!(!views(&messages).is_empty());
    let buf = buf_of(&mut c, &path);
    let pulled = op(
        &mut c,
        "text",
        opts_map(&[
            ("owner", OWNER.into()),
            ("buf", buf.into()),
            ("max_bytes", 10000.into()),
            ("max_lines", 1000.into()),
        ]),
    );
    assert_eq!(field(&pulled, "lines").as_array().unwrap().len(), 100);
    let revealed = op(&mut c, "reveal", at(buf, 20, ("token", 1)));
    assert_eq!(field(&revealed, "applied"), &true.into());
    let jumped = op(&mut c, "jump", at(buf, 30, ("col", 1)));
    assert_eq!(field(&jumped, "applied"), &true.into());
    assert_eq!(cursor(&mut c).as_array().unwrap()[0].as_u64(), Some(30));
    follow(&mut c, false);
    drain(&mut c, &listener);
    let torn = op(&mut c, "teardown", opts_map(&[("owner", OWNER.into())]));
    assert_eq!(field(&torn, "ok"), &true.into());
    assert_eq!(
        c.nvim.lua("return vim.fn.exists(':EitriFollowDemo')", vec![]).as_u64(),
        Some(0)
    );
}
