//! The plugin host's two editor snippets in a real `nvim --clean --embed --headless`: assist applies and
//! clears exactly, keeps the first baseline, leaves a value the user changed, re-asserts after an
//! `LspAttach` that turns hints on, and reports a missing API; open moves the cursor by characters and
//! splits over a modified buffer.
//!
//! Every case is `#[ignore]`d: it needs `nvim` (>= 0.10) on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::path::{Path, PathBuf};

use eitri_core::plugin::editor::{PLUGIN_ASSIST_LUA, PLUGIN_OPEN_LUA};
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
    let nvim = Embed::start(&dir, &[], &[]);
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
