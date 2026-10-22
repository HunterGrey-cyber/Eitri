//! The plugin host's editor snippets in a real `nvim --clean --embed --headless`: assist applies and
//! clears exactly, keeps the first baseline, leaves a value the user changed, re-asserts after an
//! `LspAttach` that turns hints on, and reports a missing API; open moves the cursor by characters and
//! splits over a modified buffer; follow defines and removes only its own commands, reports only the buffers it
//! may follow, scrolls only the followed window, and refuses text past its caps.
//!
//! Every case is `#[ignore]`d: it needs `nvim` (>= 0.10) on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eitri_core::plugin::editor::{PLUGIN_ASSIST_LUA, PLUGIN_FOLLOW_LUA, PLUGIN_OPEN_LUA};
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
    let nvim = Embed::start(&dir, args, &[]);
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
    c.nvim.lua("vim.wait(150)", vec![]);
    assert_eq!(state(&mut c, &a), (true, false));
    c.nvim.lua("vim.wait(550)", vec![]);
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
    c.nvim.lua("vim.wait(600)", vec![]);
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
    c.nvim.lua("vim.wait(600)", vec![]);
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
    c.nvim.lua("vim.wait(30)", vec![]);
    c.nvim.lua(
        "vim.api.nvim_exec_autocmds('BufEnter', { buffer = vim.fn.bufnr(...), modeline = false })",
        vec![s(&a)],
    );
    c.nvim.flush_scheduled();
    c.nvim.lua("vim.wait(600)", vec![]);
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

// ---- the follow snippet ----

const OWNER: &str = "demo";
const SETTLE: Duration = Duration::from_millis(150);

/// A path as the host sends it: bytes.
fn bytes(path: &Path) -> Value {
    Value::Binary(path.as_os_str().as_bytes().to_vec())
}

/// A case with the follow socket listening at `<case dir>/f.sock`.
fn follow_case(name: &str) -> (Case, UnixListener, PathBuf) {
    let c = case(&format!("f-{name}"));
    let socket = c.dir.join("f.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    (c, listener, socket)
}

/// Every line nvim sent to `listener` within `wait`, parsed, in accept order. nvim writes from its own loop, so
/// the test gives it the time with `vim.wait` before reading.
fn lines_within(nvim: &mut Embed, listener: &UnixListener, wait: Duration) -> Vec<serde_json::Value> {
    nvim.lua(
        "local ms = ... vim.wait(ms)",
        vec![Value::from(wait.as_millis() as u64)],
    );
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
    let early = install(&mut c, &socket, false, &[("MarkdownPreviewToggle", "toggle")], None);
    assert_eq!(field(&early, "ok"), &Value::from(false));
    assert_eq!(field(&early, "early"), &Value::from(true));
    assert_eq!(c.nvim.eval("exists(':MarkdownPreviewToggle')"), Value::from(0));
    let group = c.nvim.lua(
        "return pcall(vim.api.nvim_get_autocmds, { group = 'EitriPluginFollow_demo' })",
        vec![],
    );
    assert_eq!(group, Value::from(false), "no augroup before VimEnter");
    c.nvim.lua("vim.g.hold = false", vec![]);
    c.nvim
        .wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    let done = install(&mut c, &socket, false, &[("MarkdownPreviewToggle", "toggle")], None);
    assert_eq!(field(&done, "ok"), &Value::from(true));
    assert_eq!(c.nvim.eval("exists(':MarkdownPreviewToggle')"), Value::from(2));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn install_defines_the_commands_and_skips_ones_the_user_has() {
    let (mut c, listener, socket) = follow_case("define");
    c.nvim.command("command! MarkdownPreview echo 'other'");
    c.nvim.command("command! -buffer DemoLocal echo 'x'");
    let commands = [
        ("MarkdownPreview", "show"),
        ("MarkdownPreviewToggle", "toggle"),
        ("DemoLocal", "hide"),
    ];
    let answer = install(&mut c, &socket, false, &commands, None);
    assert_eq!(field(&answer, "ok"), &Value::from(true));
    let skipped = Value::Array(vec!["MarkdownPreview".into(), "DemoLocal".into()]);
    assert_eq!(field(&answer, "skipped"), &skipped);
    assert_eq!(c.nvim.eval("exists(':MarkdownPreviewToggle')"), Value::from(2));
    c.nvim.command("MarkdownPreviewToggle");
    assert_eq!(
        lines_within(&mut c.nvim, &listener, SETTLE),
        vec![serde_json::json!({"v": 1, "k": "cmd", "owner": "demo", "cmd": "toggle"})]
    );
    let out = c.nvim.eval("execute('MarkdownPreview')");
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
         return { all.MarkdownPreviewToggle.definition, all.MarkdownPreview.definition }",
        vec![],
    );
    assert_eq!(
        ours,
        Value::Array(vec!["eitri plugin demo toggle".into(), "echo 'other'".into()]),
        "the toggle command is the reinstalled one, and the user's MarkdownPreview untouched"
    );
    drain(&mut c, &listener);
    c.nvim.command("MarkdownPreviewToggle");
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
        &[("MarkdownPreviewToggle", "toggle"), ("MarkdownPreviewStop", "hide")],
        None,
    );
    follow(&mut c, true);
    c.nvim.command("normal! 10G");
    c.nvim.command("normal! 20G");
    // The user redefines one of the commands.
    c.nvim.command("command! MarkdownPreviewStop echo 'mine'");
    let answer = op(&mut c, "teardown", opts_map(&[("owner", OWNER.into())]));
    assert_eq!(field(&answer, "ok"), &Value::from(true));
    let footprint = c.nvim.footprint_of(&["EitriPluginFollow_demo"], &[]);
    assert_eq!(footprint.autocmds, 0);
    assert_eq!(footprint.active_timers, timers);
    assert_eq!(c.nvim.eval("exists(':MarkdownPreviewToggle')"), Value::from(0));
    assert_eq!(
        c.nvim.eval("exists(':MarkdownPreviewStop')"),
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
