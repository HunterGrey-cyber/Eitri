//! `REVIEW_LUA` in a real `nvim --clean --embed`: a turn's hunks drawn over the buffer of a file,
//! the overlay's keys, a revert in the buffer and the events it queues, and the module going away
//! again without a trace.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.
//! Files live in a directory the test makes under the target dir; paths reach nvim only as
//! arguments (`nvim_cmd` with a structured argument), never in Ex or Lua source.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use eitri_core::companion::{install_args as companion_install_args, Sockets, INSTALL_LUA, TEARDOWN_LUA};
use eitri_core::review_editor::{
    clear_args, install_args, open_and_show_args, parse_revert_event, show_args, Owner, ShowHunk, ShowMeta,
    CLEAR_ALL_LUA, CLEAR_LUA, OPEN_AND_SHOW_LUA, REVIEW_LUA, REVIEW_LUA_VERSION, SHOW_LUA, TAKE_EVENTS_LUA,
};
use eitri_core::turn_review::Scope;
use embed_client::{opts_map, Embed};
use rmpv::Value;

/// `,` as the local leader, and `vim.notify` replaced by a recorder the test reads.
const PREPARE_LUA: &str = "vim.g.maplocalleader = ',' _G.__said = {} \
     vim.notify = function(msg) table.insert(_G.__said, msg) end";
/// `:edit <path>` with the path as an argument and no file-name expansion.
const OPEN_LUA: &str =
    "local path = ... vim.api.nvim_cmd({ cmd = 'edit', args = { path }, magic = { file = false } }, {})";
/// Keys as if typed, mappings included, run before this returns.
const FEED_LUA: &str =
    "local keys = ... vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes(keys, true, false, true), 'mx', false)";
const SAID_LUA: &str = "local s = _G.__said _G.__said = {} return s";
const LINES_LUA: &str = "return vim.api.nvim_buf_get_lines(0, 0, -1, true)";
const WRITE_LUA: &str = "vim.api.nvim_cmd({ cmd = 'write' }, {})";
/// The current buffer's marks in the review namespace, summarised.
const MARKS_LUA: &str = r#"
local ns = vim.api.nvim_get_namespaces().eitri_review
local out = { count = 0, off_priority = 0, adds = {}, virt = {}, signs = {}, trackers = {} }
if not ns then return out end
for _, m in ipairs(vim.api.nvim_buf_get_extmarks(0, ns, 0, -1, { details = true })) do
  local row, d = m[2], m[4]
  out.count = out.count + 1
  -- nvim reports a priority only for a mark that draws something (not for the tracker).
  if (d.line_hl_group or d.virt_lines or d.sign_text) and d.priority ~= 90 then
    out.off_priority = out.off_priority + 1
  end
  if d.line_hl_group then table.insert(out.adds, row) end
  if d.virt_lines then
    local texts = {}
    for _, l in ipairs(d.virt_lines) do
      local t = ''
      for _, c in ipairs(l) do
        t = t .. c[1]
        if c[2] ~= 'DiffDelete' then t = t .. '<not DiffDelete>' end
      end
      table.insert(texts, t)
    end
    table.insert(out.virt, { row = row, above = d.virt_lines_above and true or false, lines = texts })
  end
  if d.sign_text then table.insert(out.signs, { row = row, text = vim.trim(d.sign_text), hl = d.sign_hl_group }) end
  if d.end_row then table.insert(out.trackers, { row = row, end_row = d.end_row }) end
end
return out
"#;
/// `"<lhs> <desc>"` of every buffer-local normal map of the current buffer whose description is the
/// review module's.
const REVIEW_MAPS_LUA: &str = r#"
local out = {}
for _, m in ipairs(vim.api.nvim_buf_get_keymap(0, 'n')) do
  if (m.desc or ''):find('^eitri review') then table.insert(out, m.lhs) end
end
table.sort(out)
return out
"#;

fn meta(turn: u32) -> ShowMeta {
    ShowMeta {
        tab: 4,
        session: "sess-1".to_owned(),
        turn,
        scope: Scope::Turn,
    }
}

/// A directory of the test's own under the target dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("review_lua").join(format!(
            "{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("files")).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Default, PartialEq)]
struct Marks {
    count: usize,
    off_priority: usize,
    adds: Vec<i64>,
    /// (row, above, the removed lines shown)
    virt: Vec<(i64, bool, Vec<String>)>,
    /// (row, text, highlight)
    signs: Vec<(i64, String, String)>,
    /// (row, end row)
    trackers: Vec<(i64, i64)>,
}

fn get<'a>(value: &'a Value, name: &str) -> &'a Value {
    value
        .as_map()
        .unwrap_or_else(|| panic!("not a map: {value:?}"))
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
        .unwrap_or(&Value::Nil)
}

/// An array, or the empty map an empty Lua table can arrive as.
fn items(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::Map(entries) if entries.is_empty() => Vec::new(),
        other => panic!("not a list: {other:?}"),
    }
}

fn int(value: &Value) -> i64 {
    value.as_i64().unwrap_or_else(|| panic!("not an integer: {value:?}"))
}

/// The bytes of a Lua string nvim sent back: msgpack str whatever the bytes are.
fn bytes(value: &Value) -> Vec<u8> {
    match value {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Binary(b) => b.clone(),
        other => panic!("not a string: {other:?}"),
    }
}

fn text(value: &Value) -> String {
    String::from_utf8(bytes(value)).unwrap()
}

/// File lines `[start, start + len)` (from 1) of `file`, terminators kept.
fn cut(file: &[u8], start: u32, len: u32) -> Vec<Vec<u8>> {
    let lines: Vec<Vec<u8>> = file.split_inclusive(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
    let from = (start.max(1) - 1) as usize;
    lines[from..from + len as usize].to_vec()
}

/// The hunk of `old`/`new` ranges cut from the base and end files.
fn hunk_of(id: u32, base: &[u8], end: &[u8], old: (u32, u32), new: (u32, u32)) -> ShowHunk {
    ShowHunk::from_file_lines(id, old, new, &cut(base, old.0, old.1), &cut(end, new.0, new.1))
}

fn numbered(lines: &[&str]) -> Vec<u8> {
    lines.iter().flat_map(|l| format!("{l}\n").into_bytes()).collect()
}

/// `a1` .. `a<n>` with some lines replaced: `(line, replacement lines)`; an empty replacement
/// removes the line.
fn edited(n: u32, changes: &[(u32, &[&str])]) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 1..=n {
        match changes.iter().find(|(at, _)| *at == i) {
            Some((_, with)) => out.extend(with.iter().map(|s| s.to_string())),
            None => out.push(format!("a{i}")),
        }
    }
    numbered(&out.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The two-change file most cases use: thirty lines, line 5 becomes two, line 20 becomes two.
/// Hunk 1 is `@@ -2,7 +2,8 @@`, hunk 2 `@@ -17,7 +18,8 @@`.
struct TwoHunks {
    base: Vec<u8>,
    end: Vec<u8>,
    hunks: Vec<ShowHunk>,
}

fn two_hunks() -> TwoHunks {
    let base = edited(30, &[]);
    let end = edited(30, &[(5, &["X5", "X5b"]), (20, &["Y20", "Y20b"])]);
    let hunks = vec![
        hunk_of(1, &base, &end, (2, 7), (2, 8)),
        hunk_of(2, &base, &end, (17, 7), (18, 8)),
    ];
    TwoHunks { base, end, hunks }
}

struct Fx {
    nvim: Embed,
    scratch: Scratch,
}

impl Fx {
    /// An nvim with nothing installed.
    fn bare(name: &str) -> Fx {
        let scratch = Scratch::new(name);
        let home = scratch.0.join("nvim");
        let mut nvim = Embed::start(&home, &[], &[("HOME", home.to_str().unwrap())]);
        nvim.lua(PREPARE_LUA, vec![]);
        Fx { nvim, scratch }
    }

    /// An nvim with the module installed as the integrated window's.
    fn new(name: &str) -> Fx {
        let mut fx = Fx::bare(name);
        let answer = fx.nvim.lua(REVIEW_LUA, install_args(Owner::Embedded));
        assert_eq!(get(&answer, "installed"), &Value::from(true), "{answer:?}");
        assert_eq!(get(&answer, "replaced"), &Value::from(false));
        fx
    }

    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.scratch.0.join("files").join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn open(&mut self, path: &Path) {
        self.nvim.lua(OPEN_LUA, vec![Value::from(path.to_str().unwrap())]);
    }

    fn show(&mut self, path: &Path, meta: &ShowMeta, hunks: &[ShowHunk]) -> Value {
        self.nvim.lua(SHOW_LUA, show_args(path, meta, hunks))
    }

    fn feed(&mut self, keys: &str) {
        self.nvim.lua(FEED_LUA, vec![Value::from(keys)]);
    }

    fn cursor(&mut self, line: i64) {
        self.nvim.request(
            "nvim_win_set_cursor",
            vec![Value::from(0), Value::Array(vec![Value::from(line), Value::from(0)])],
        );
    }

    fn cursor_line(&mut self) -> i64 {
        int(&self.nvim.eval("line('.')"))
    }

    fn said(&mut self) -> Vec<String> {
        items(&self.nvim.lua(SAID_LUA, vec![])).iter().map(text).collect()
    }

    fn lines(&mut self) -> Vec<Vec<u8>> {
        items(&self.nvim.lua(LINES_LUA, vec![])).iter().map(bytes).collect()
    }

    fn events(&mut self) -> Vec<Value> {
        let answer = self.nvim.lua(TAKE_EVENTS_LUA, vec![]);
        items(get(&answer, "events"))
    }

    fn review_maps(&mut self) -> Vec<String> {
        items(&self.nvim.lua(REVIEW_MAPS_LUA, vec![]))
            .iter()
            .map(text)
            .collect()
    }

    fn modified(&mut self) -> bool {
        self.nvim.eval("&modified").as_i64() == Some(1)
    }

    fn write(&mut self) {
        self.nvim.lua(WRITE_LUA, vec![]);
    }

    fn marks(&mut self) -> Marks {
        let v = self.nvim.lua(MARKS_LUA, vec![]);
        Marks {
            count: int(get(&v, "count")) as usize,
            off_priority: int(get(&v, "off_priority")) as usize,
            adds: items(get(&v, "adds")).iter().map(int).collect(),
            virt: items(get(&v, "virt"))
                .iter()
                .map(|m| {
                    (
                        int(get(m, "row")),
                        get(m, "above").as_bool().unwrap(),
                        items(get(m, "lines")).iter().map(text).collect(),
                    )
                })
                .collect(),
            signs: items(get(&v, "signs"))
                .iter()
                .map(|m| (int(get(m, "row")), text(get(m, "text")), text(get(m, "hl"))))
                .collect(),
            trackers: items(get(&v, "trackers"))
                .iter()
                .map(|m| (int(get(m, "row")), int(get(m, "end_row"))))
                .collect(),
        }
    }

    /// Waits until the current buffer has `n` marks in the review namespace (a `TextChanged` ran).
    fn wait_for_marks(&mut self, n: usize) {
        let mut seen = 0;
        self.nvim.wait_until(&format!("{n} review marks"), |nvim| {
            let v = nvim.lua(MARKS_LUA, vec![]);
            seen = int(get(&v, "count")) as usize;
            seen == n
        });
    }
}

fn as_lines(file: &[u8]) -> Vec<Vec<u8>> {
    file.split_inclusive(|b| *b == b'\n')
        .map(|l| l.strip_suffix(b"\n").unwrap_or(l).to_vec())
        .collect()
}

fn count(answer: &Value, name: &str) -> i64 {
    int(get(answer, name))
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn extmarks_land_in_the_namespace_on_the_right_lines() {
    let mut fx = Fx::bare("extmarks");
    // Another plugin's mark, which must come through untouched.
    fx.nvim
        .lua("_G.__other = vim.api.nvim_create_namespace('someone_else')", vec![]);
    let spaces = |fx: &mut Fx| {
        int(&fx
            .nvim
            .lua("return vim.tbl_count(vim.api.nvim_get_namespaces())", vec![]))
    };
    let spaces_before = spaces(&mut fx);
    let answer = fx.nvim.lua(REVIEW_LUA, install_args(Owner::Embedded));
    assert_eq!(get(&answer, "installed"), &Value::from(true));
    assert_eq!(
        spaces(&mut fx),
        spaces_before + 1,
        "the install makes exactly one namespace"
    );

    // One hunk holding two changes with context between them: a6 became B6 and B6b, a11 went.
    let base = edited(20, &[]);
    let end = edited(20, &[(6, &["B6", "B6b"]), (11, &[])]);
    let path = fx.file("two-changes.txt", &end);
    fx.open(&path);
    fx.nvim.lua(
        "_G.__other_id = vim.api.nvim_buf_set_extmark(0, _G.__other, 4, 0, { end_row = 6, hl_group = 'Search' }) \
         _G.__other_was = vim.api.nvim_buf_get_extmark_by_id(0, _G.__other, _G.__other_id, { details = true })",
        vec![],
    );
    let hunk = hunk_of(1, &base, &end, (3, 12), (3, 12));
    let spaces_shown = spaces(&mut fx);
    let answer = fx.show(&path, &meta(2), &[hunk]);
    assert_eq!(count(&answer, "drawn"), 1, "{answer:?}");
    assert_eq!(count(&answer, "skipped"), 0);
    assert_eq!(get(&answer, "notice"), &Value::Nil);
    assert_eq!(count(&answer, "active"), 1);

    let marks = fx.marks();
    assert_eq!(marks.off_priority, 0, "every mark at priority 90: {marks:?}");
    assert_eq!(marks.trackers, vec![(2, 14)], "the hunk's new range, rows 2..14");
    assert_eq!(marks.adds, vec![5, 6], "B6 and B6b only, no context row");
    assert_eq!(
        marks.virt,
        vec![(5, true, vec!["a6".to_owned()]), (11, true, vec!["a11".to_owned()]),],
        "the removed lines only, above the line that follows them"
    );
    assert_eq!(
        marks.signs,
        vec![(5, "▎".to_owned(), "DiffChange".to_owned())],
        "one sign, on the first changed line"
    );
    assert_eq!(marks.count, 1 + 2 + 2 + 1);

    let other_same = fx.nvim.lua(
        "return vim.deep_equal(_G.__other_was, vim.api.nvim_buf_get_extmark_by_id(0, _G.__other, _G.__other_id, { details = true }))",
        vec![],
    );
    assert_eq!(other_same, Value::from(true), "another namespace's mark is untouched");
    assert_eq!(spaces(&mut fx), spaces_shown, "showing makes no namespace");
    let defined = fx.nvim.lua("return #vim.fn.sign_getdefined()", vec![]);
    assert_eq!(defined, Value::from(0), "no sign was defined");

    // A removal at the end of the file is shown below the last line.
    let base = numbered(&["a1", "a2", "a3", "a4", "a5"]);
    let end = numbered(&["a1", "a2", "a3", "a4"]);
    let path = fx.file("eof.txt", &end);
    fx.open(&path);
    let answer = fx.show(&path, &meta(2), &[hunk_of(1, &base, &end, (2, 4), (2, 3))]);
    assert_eq!(count(&answer, "drawn"), 1, "{answer:?}");
    let marks = fx.marks();
    assert_eq!(marks.virt, vec![(3, false, vec!["a5".to_owned()])]);
    assert_eq!(marks.adds, Vec::<i64>::new());
    assert_eq!(marks.signs.len(), 1);
    assert_eq!(marks.signs[0].0, 3);
    assert_eq!(count(&answer, "active"), 2);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_hunk_that_no_longer_matches_is_skipped_with_a_notice() {
    let mut fx = Fx::new("skipped");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    // The user changed a line of the first hunk before looking.
    fx.nvim.request(
        "nvim_buf_set_lines",
        vec![
            Value::from(0),
            Value::from(4),
            Value::from(5),
            Value::from(true),
            Value::Array(vec![Value::from("typed by the user")]),
        ],
    );
    let answer = fx.show(&path, &meta(2), &t.hunks);
    assert_eq!(count(&answer, "drawn"), 1);
    assert_eq!(count(&answer, "skipped"), 1);
    assert_eq!(text(get(&answer, "notice")), "1 hunk no longer matches this buffer");
    let marks = fx.marks();
    assert_eq!(marks.trackers, vec![(17, 25)], "only the second hunk is drawn");

    // Both changed: nothing drawn, no overlay, no keys.
    fx.nvim.request(
        "nvim_buf_set_lines",
        vec![
            Value::from(0),
            Value::from(20),
            Value::from(21),
            Value::from(true),
            Value::Array(vec![Value::from("typed too")]),
        ],
    );
    let answer = fx.show(&path, &meta(2), &t.hunks);
    assert_eq!(count(&answer, "drawn"), 0);
    assert_eq!(count(&answer, "skipped"), 2);
    assert_eq!(text(get(&answer, "notice")), "2 hunks no longer match this buffer");
    assert_eq!(count(&answer, "active"), 0);
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
    assert!(fx.events().is_empty(), "a replacement the host asked for is no event");
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_edit_inside_a_hunk_clears_that_hunk_only() {
    let mut fx = Fx::new("edit-inside");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    let before = fx.marks().count;
    assert_eq!(fx.marks().trackers.len(), 2);

    // A line added just after the first hunk widens nothing and clears nothing.
    fx.cursor(10);
    fx.feed("Oinserted<Esc>");
    fx.nvim.flush_scheduled();
    assert_eq!(fx.marks().count, before);
    assert_eq!(
        fx.marks().trackers,
        vec![(1, 9), (18, 26)],
        "the second hunk moved down by one"
    );

    // Typing inside the first hunk clears it, and only it.
    fx.cursor(5);
    fx.feed("A!<Esc>");
    let marks_of_second = 1 + 2 + 1 + 1;
    fx.wait_for_marks(marks_of_second);
    assert_eq!(fx.marks().trackers, vec![(18, 26)]);
    assert_eq!(fx.review_maps().len(), 4, "the overlay and its keys stay");
    assert!(fx.events().is_empty(), "a cleared hunk is no event");

    // Its revert now finds no hunk under the cursor.
    fx.feed(",r");
    assert_eq!(fx.said(), vec!["there is no review hunk under the cursor".to_owned()]);
    fx.nvim.quit();
}

/// The user's own maps that the overlay's keys meet: a buffer-local `]h` (an Ex rhs, or a Lua
/// callback) and a global `[h`, as they were before.
const SHADOW_EX_LUA: &str = "vim.keymap.set('n', ']h', ':let g:user_hit = 1<CR>', { buffer = 0, desc = 'user ex' }) \
     vim.keymap.set('n', '[h', function() vim.g.user_global = 1 end, { desc = 'user global' }) \
     _G.__before = vim.fn.maparg(']h', 'n', false, true) \
     _G.__global_before = vim.fn.maparg('[h', 'n', false, true)";
const SHADOW_LUA_CALLBACK_LUA: &str =
    "vim.keymap.set('n', ']h', function() vim.g.user_hit = 2 end, { buffer = 0, desc = 'user callback' }) \
     _G.__before = vim.fn.maparg(']h', 'n', false, true)";
const SAME_AS_BEFORE_LUA: &str = "return vim.deep_equal(_G.__before, vim.fn.maparg(']h', 'n', false, true))";

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn keys_are_buffer_local_and_shadowed_maps_come_back() {
    for (shadow, hit) in [(SHADOW_EX_LUA, 1), (SHADOW_LUA_CALLBACK_LUA, 2)] {
        let mut fx = Fx::new("keys");
        let t = two_hunks();
        let path = fx.file("f.txt", &t.end);
        let other = fx.file("other.txt", b"other\n");
        fx.open(&path);
        fx.nvim.lua(shadow, vec![]);
        assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
        assert_eq!(
            fx.review_maps(),
            vec![",q".to_owned(), ",r".to_owned(), "[h".to_owned(), "]h".to_owned()]
        );

        // ]h and [h move between the hunks' first changed lines, with a count, without wrapping.
        fx.cursor(1);
        fx.feed("]h");
        assert_eq!(fx.cursor_line(), 5);
        fx.feed("]h");
        assert_eq!(fx.cursor_line(), 21);
        fx.feed("]h");
        assert_eq!(fx.cursor_line(), 21, "no wrap");
        fx.feed("[h");
        assert_eq!(fx.cursor_line(), 5);
        fx.cursor(1);
        fx.feed("2]h");
        assert_eq!(fx.cursor_line(), 21);

        // Another buffer has none of the keys.
        fx.open(&other);
        assert!(fx.review_maps().is_empty());
        fx.open(&path);

        fx.feed(",q");
        assert!(fx.review_maps().is_empty());
        assert_eq!(
            fx.nvim.lua(SAME_AS_BEFORE_LUA, vec![]),
            Value::from(true),
            "the shadowed map is back"
        );
        fx.feed("]h");
        assert_eq!(fx.nvim.eval("g:user_hit"), Value::from(hit), "and still works");
        if shadow == SHADOW_EX_LUA {
            assert_eq!(
                fx.nvim.lua(
                    "return vim.deep_equal(_G.__global_before, vim.fn.maparg('[h', 'n', false, true))",
                    vec![]
                ),
                Value::from(true),
                "a global map is neither saved nor changed"
            );
        }
        assert_eq!(fx.marks().count, 0);
        let events = fx.events();
        assert_eq!(events.len(), 1);
        assert_eq!(text(get(&events[0], "kind")), "off");
        assert_eq!(text(get(&events[0], "why")), "user");
        assert_eq!(text(get(&events[0], "path")), path.to_str().unwrap());
        fx.nvim.quit();
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn revert_replaces_exactly_the_hunk_and_u_undoes_it() {
    let mut fx = Fx::new("revert");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);

    fx.cursor(5);
    fx.feed(",r");
    assert!(fx.said().is_empty());
    let after_first = edited(30, &[(20, &["Y20", "Y20b"])]);
    assert_eq!(fx.lines(), as_lines(&after_first), "the first hunk is the base again");
    assert!(fx.modified());
    assert_eq!(std::fs::read(&path).unwrap(), t.end, "the file on disk is unchanged");
    // The second hunk moved up a line and still matches.
    fx.nvim.flush_scheduled();
    assert_eq!(fx.marks().trackers, vec![(16, 24)]);

    fx.cursor(19);
    fx.feed(",r");
    assert!(fx.said().is_empty());
    assert_eq!(fx.lines(), as_lines(&t.base));
    assert_eq!(fx.marks().count, 0);

    fx.feed("u");
    assert_eq!(fx.lines(), as_lines(&after_first), "u undoes the last revert");
    assert!(fx.modified());
    assert_eq!(std::fs::read(&path).unwrap(), t.end);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn revert_refuses_when_the_lines_differ() {
    let mut fx = Fx::new("refuse");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);

    // A line changes and the key comes before any TextChanged could clear the hunk.
    fx.cursor(5);
    fx.nvim.lua(
        "local keys = ... vim.api.nvim_buf_set_lines(0, 3, 4, true, { 'changed' }) \
         vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes(keys, true, false, true), 'mx', false)",
        vec![Value::from(",r")],
    );
    assert_eq!(
        fx.said(),
        vec!["this hunk no longer matches the buffer; nothing was reverted".to_owned()]
    );
    assert_eq!(fx.lines()[3], b"changed".to_vec());
    assert_eq!(fx.lines()[4], b"X5".to_vec(), "nothing else changed");
    assert!(fx.events().is_empty());

    fx.cursor(1);
    fx.feed(",r");
    assert_eq!(fx.said(), vec!["there is no review hunk under the cursor".to_owned()]);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn reverts_are_queued_in_order_and_taken_once() {
    let mut fx = Fx::new("queue");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(5), &t.hunks), "drawn"), 2);
    fx.cursor(21);
    fx.feed(",r");
    fx.cursor(5);
    fx.feed(",r");
    assert_eq!(fx.lines(), as_lines(&t.base));

    let answer = fx.nvim.lua(TAKE_EVENTS_LUA, vec![]);
    assert_eq!(
        count(&answer, "active"),
        1,
        "the overlay is still on, with no hunk left"
    );
    let events = items(get(&answer, "events"));
    assert_eq!(events.len(), 2);
    let first = parse_revert_event(&events[0]).expect("a revert event");
    let second = parse_revert_event(&events[1]).expect("a revert event");
    assert_eq!(first.hunk, t.hunks[1], "in the order they happened");
    assert_eq!(second.hunk, t.hunks[0]);
    assert_eq!(first.meta, meta(5));
    assert_eq!(first.path, path.to_str().unwrap());
    assert!(fx.events().is_empty(), "taken once");
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reload_clears_the_overlay() {
    let mut fx = Fx::new("reload");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    // `:edit!` unloads the buffer and reads it again.
    fx.nvim
        .lua("vim.api.nvim_cmd({ cmd = 'edit', bang = true }, {})", vec![]);
    fx.nvim.flush_scheduled();
    let events = fx.events();
    assert_eq!(events.len(), 1, "one event, not a close as well: {events:?}");
    assert_eq!(text(get(&events[0], "why")), "reload");
    assert_eq!(
        fx.said(),
        vec!["the review overlay was cleared: the file was reloaded".to_owned()]
    );
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
    assert_eq!(count(&fx.nvim.lua(CLEAR_ALL_LUA, vec![]), "active"), 0);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn bunload_clears_it_as_closed() {
    let mut fx = Fx::new("bunload");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    let other = fx.file("other.txt", b"other\n");
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    let buf = int(&fx.nvim.lua("return vim.api.nvim_get_current_buf()", vec![]));
    fx.open(&other);
    fx.nvim.lua(
        "local b = ... vim.api.nvim_cmd({ cmd = 'bunload', args = { tostring(b) } }, {})",
        vec![Value::from(buf)],
    );
    fx.nvim.flush_scheduled();
    let events = fx.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(text(get(&events[0], "why")), "closed");
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn closing_the_buffer_clears_it() {
    let mut fx = Fx::new("closing");
    let t = two_hunks();
    for (name, cmd) in [("deleted.txt", "bdelete"), ("wiped.txt", "bwipeout")] {
        let path = fx.file(name, &t.end);
        fx.open(&path);
        assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
        fx.nvim.lua(
            "local c = ... vim.api.nvim_cmd({ cmd = c }, {})",
            vec![Value::from(cmd)],
        );
        fx.nvim.flush_scheduled();
        let events = fx.events();
        assert_eq!(events.len(), 1, "{cmd}: {events:?}");
        assert_eq!(text(get(&events[0], "why")), "closed", "{cmd}");
        assert_eq!(text(get(&events[0], "path")), path.to_str().unwrap());
    }
    assert_eq!(count(&fx.nvim.lua(TAKE_EVENTS_LUA, vec![]), "active"), 0);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn show_for_another_turn_replaces() {
    let mut fx = Fx::new("replace");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    let answer = fx.show(&path, &meta(3), &t.hunks[1..]);
    assert_eq!(count(&answer, "drawn"), 1);
    assert_eq!(count(&answer, "active"), 1);
    assert_eq!(fx.marks().trackers, vec![(17, 25)], "only the new review's hunk");
    assert_eq!(fx.review_maps().len(), 4, "the keys once");
    assert!(fx.events().is_empty(), "a replacement is no event");

    fx.cursor(21);
    fx.feed(",r");
    let events = fx.events();
    assert_eq!(events.len(), 1);
    assert_eq!(parse_revert_event(&events[0]).unwrap().meta, meta(3));

    // And `clear` takes it off without an event.
    assert_eq!(count(&fx.nvim.lua(CLEAR_LUA, clear_args(&path)), "active"), 0);
    assert!(fx.review_maps().is_empty());
    assert!(fx.events().is_empty());
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_module_is_installed_once() {
    let mut fx = Fx::new("once");
    let spaces = "return vim.tbl_count(vim.api.nvim_get_namespaces())";
    let before = fx.nvim.footprint_of(&["eitri_review"], &["__eitri_review"]);
    let spaces_before = fx.nvim.lua(spaces, vec![]);
    assert_eq!(before.autocmds, 6, "{before:?}");
    assert_eq!(before.globals, vec!["__eitri_review".to_owned()]);

    let answer = fx.nvim.lua(REVIEW_LUA, install_args(Owner::Embedded));
    assert_eq!(get(&answer, "installed"), &Value::from(false));
    assert_eq!(get(&answer, "replaced"), &Value::from(false));
    assert_eq!(fx.nvim.footprint_of(&["eitri_review"], &["__eitri_review"]), before);
    assert_eq!(fx.nvim.lua(spaces, vec![]), spaces_before);

    // A show on top changes no count either: the autocommands are the module's, not a buffer's.
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    fx.show(&path, &meta(2), &t.hunks);
    fx.show(&path, &meta(2), &t.hunks);
    assert_eq!(fx.nvim.footprint_of(&["eitri_review"], &["__eitri_review"]), before);

    // A call without a module says so instead of failing.
    fx.nvim.lua("_G.__eitri_review.teardown()", vec![]);
    let answer = fx.nvim.lua(TAKE_EVENTS_LUA, vec![]);
    assert_eq!(get(&answer, "missing"), &Value::from(true));
    assert_eq!(fx.nvim.footprint_of(&["eitri_review"], &["__eitri_review"]).autocmds, 0);
    fx.nvim.quit();
}

fn channel(nvim: &mut Embed) -> u64 {
    let info = nvim.request("nvim_get_api_info", vec![]);
    info.as_array().unwrap()[0].as_u64().unwrap()
}

fn install_companion(nvim: &mut Embed) -> u64 {
    let chan = channel(nvim);
    nvim.lua(INSTALL_LUA, companion_install_args(chan, &Sockets::default()));
    chan
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn companion_teardown_removes_everything() {
    let mut fx = Fx::bare("companion");
    let chan = install_companion(&mut fx.nvim);
    let answer = fx
        .nvim
        .lua(REVIEW_LUA, install_args(Owner::Companion { channel: chan }));
    assert_eq!(get(&answer, "installed"), &Value::from(true), "{answer:?}");

    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    fx.nvim.lua(SHADOW_EX_LUA, vec![]);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    assert!(fx.marks().count > 0);

    assert_eq!(fx.nvim.lua(TEARDOWN_LUA, vec![Value::from(chan)]), Value::from(true));
    let left = fx.nvim.footprint_of(&["eitri_review"], &["__eitri_review"]);
    assert_eq!(left.autocmds, 0, "{left:?}");
    assert!(left.globals.is_empty(), "{left:?}");
    assert!(fx.review_maps().is_empty());
    assert_eq!(fx.nvim.lua(SAME_AS_BEFORE_LUA, vec![]), Value::from(true));
    assert_eq!(fx.marks().count, 0);
    let group = fx.nvim.lua(
        "return (pcall(vim.api.nvim_get_autocmds, { group = 'eitri_review' }))",
        vec![],
    );
    assert_eq!(group, Value::from(false), "the group itself is gone");
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn another_owner_is_torn_down_first() {
    let mut fx = Fx::new("owner");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    fx.nvim.lua(SHADOW_EX_LUA, vec![]);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);

    // Installing into a companion glue that is not there is refused, and changes nothing.
    fx.nvim.lua("_G.__src = ...", vec![Value::from(REVIEW_LUA)]);
    let refused = fx.nvim.lua(
        "return (pcall(assert(loadstring(_G.__src)), ...))",
        vec![Value::from("companion:999"), Value::from(1)],
    );
    assert_eq!(refused, Value::from(false));
    assert_eq!(fx.marks().count, 10, "the embedded overlay is still on");

    let chan = install_companion(&mut fx.nvim);
    let teardowns = "return #_G.__eitri_companion.teardowns";
    // The glue's own parts (those that need no socket) have teardowns of their own.
    let glue_only = int(&fx.nvim.lua(teardowns, vec![]));
    let answer = fx
        .nvim
        .lua(REVIEW_LUA, install_args(Owner::Companion { channel: chan }));
    assert_eq!(get(&answer, "installed"), &Value::from(true));
    assert_eq!(get(&answer, "replaced"), &Value::from(true));
    assert_eq!(fx.marks().count, 0, "the old marks are gone");
    assert!(fx.review_maps().is_empty());
    assert_eq!(fx.nvim.lua(SAME_AS_BEFORE_LUA, vec![]), Value::from(true));
    assert_eq!(int(&fx.nvim.lua(teardowns, vec![])), glue_only + 1);

    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    let next_version = vec![
        Value::from(format!("companion:{chan}")),
        Value::from(REVIEW_LUA_VERSION + 1),
    ];
    let answer = fx.nvim.lua(REVIEW_LUA, next_version);
    assert_eq!(get(&answer, "installed"), &Value::from(true));
    assert_eq!(get(&answer, "replaced"), &Value::from(true));
    assert_eq!(fx.marks().count, 0);
    assert_eq!(
        int(&fx.nvim.lua(teardowns, vec![])),
        glue_only + 1,
        "the old one left the list"
    );
    assert!(fx.events().is_empty());
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn paths_and_lines_are_arguments_not_source() {
    let mut fx = Fx::new("args");
    let dir = fx.scratch.0.join("files").join("a [b] *c");
    std::fs::create_dir_all(&dir).unwrap();
    let decoy = dir.join("1.txt");
    std::fs::write(&decoy, b"decoy\n").unwrap();
    let path = dir.join("it's 100% #1.txt");
    let tricky = "x = [[ ]] ' \" ]]..vim.cmd('qa!')";
    let base = numbered(&["one", tricky, "three"]);
    let end = numbered(&["one", "two", "three"]);
    std::fs::write(&path, &end).unwrap();
    fx.open(&decoy);
    let hunks = [hunk_of(1, &base, &end, (1, 3), (1, 3))];

    let answer = fx.nvim.lua(
        OPEN_AND_SHOW_LUA,
        open_and_show_args(&path, Some(2), &meta(2), Some(&hunks)),
    );
    assert_eq!(get(&answer, "opened"), &Value::from(true), "{answer:?}");
    assert_eq!(count(&answer, "drawn"), 1);
    let name = fx.nvim.lua("return vim.api.nvim_buf_get_name(0)", vec![]);
    assert_eq!(
        bytes(&name),
        path.to_str().unwrap().as_bytes(),
        "the file itself, literally"
    );
    assert_eq!(fx.cursor_line(), 2);
    assert_eq!(fx.marks().virt, vec![(1, true, vec![tricky.to_owned()])]);

    fx.feed(",r");
    assert!(fx.said().is_empty());
    assert_eq!(fx.lines(), as_lines(&base));
    let event = parse_revert_event(&fx.events()[0]).unwrap();
    assert_eq!(event.path, path.to_str().unwrap());
    assert_eq!(event.hunk.old_bytes(), base);
    fx.write();
    assert_eq!(std::fs::read(&path).unwrap(), base);
    assert_eq!(std::fs::read(&decoy).unwrap(), b"decoy\n");
    fx.nvim.quit();
}

/// Draws a whole-file hunk of `base` → `end`, reverts it with the cursor on `line` and checks the
/// event's bytes and what `:w` writes.
fn draw_revert_write(fx: &mut Fx, name: &str, base: &[u8], end: &[u8], line: i64, fileformat: &str) {
    let path = fx.file(name, end);
    fx.open(&path);
    assert_eq!(text(&fx.nvim.eval("&fileformat")), fileformat, "{name}");
    let (old_n, new_n) = (
        base.split_inclusive(|b| *b == b'\n').count() as u32,
        end.split_inclusive(|b| *b == b'\n').count() as u32,
    );
    let hunk = hunk_of(1, base, end, (1, old_n), (1, new_n));
    let answer = fx.show(&path, &meta(2), &[hunk]);
    assert_eq!(count(&answer, "drawn"), 1, "{name}: {answer:?}");
    fx.cursor(line);
    fx.feed(",r");
    assert!(fx.said().is_empty(), "{name}");
    let events = fx.events();
    assert_eq!(events.len(), 1, "{name}");
    let event = parse_revert_event(&events[0]).unwrap();
    assert_eq!(event.hunk.old_bytes(), base, "{name}: the event echoes the base bytes");
    assert_eq!(event.hunk.new_bytes(), end, "{name}");
    // nvim would add a final newline to a buffer without one ('fixeol'); a file that had none
    // keeps none only with it off.
    fx.nvim.lua("vim.bo.fixeol = false", vec![]);
    fx.write();
    assert_eq!(std::fs::read(&path).unwrap(), base, "{name}: `:w` writes the base");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn lf_hunks_draw_revert_and_echo_exact_bytes() {
    let mut fx = Fx::new("lf");
    draw_revert_write(
        &mut fx,
        "lf.txt",
        b"one\ntwo\nthree\nfour\n",
        b"one\nTWO\nmore\nthree\nfour\n",
        2,
        "unix",
    );
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn crlf_hunks_in_a_dos_buffer_draw_revert_and_echo_exact_bytes() {
    let mut fx = Fx::new("dos");
    draw_revert_write(
        &mut fx,
        "dos.txt",
        b"one\r\ntwo\r\nthree\r\n",
        b"one\r\nTWO\r\nmore\r\nthree\r\n",
        2,
        "dos",
    );
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn crlf_lines_in_a_mixed_unix_buffer_keep_their_cr() {
    let mut fx = Fx::new("mixed");
    let base: &[u8] = b"one\nx\r\ny\r\nend\n";
    let end: &[u8] = b"one\nX\r\ny\r\nend\n";
    let path = fx.file("mixed-peek.txt", end);
    fx.open(&path);
    assert_eq!(
        fx.lines()[1],
        b"X\r".to_vec(),
        "nvim keeps the CR in a unix buffer's text"
    );
    draw_revert_write(&mut fx, "mixed.txt", base, end, 2, "unix");
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_missing_final_newline_is_matched_and_kept() {
    let mut fx = Fx::new("noeol");
    draw_revert_write(&mut fx, "noeol.txt", b"a\nb\nc", b"a\nB\nc", 2, "unix");
    assert_eq!(fx.nvim.eval("&eol"), Value::from(0));

    // A hunk that disagrees with the buffer about the final newline is not drawn, either way.
    for (on_disk, in_hunk) in [(&b"a\nB\nc"[..], &b"a\nB\nc\n"[..]), (b"a\nB\nc\n", b"a\nB\nc")] {
        let path = fx.file("disagrees.txt", on_disk);
        fx.open(&path);
        fx.nvim
            .lua("vim.api.nvim_cmd({ cmd = 'edit', bang = true }, {})", vec![]);
        let hunk = hunk_of(1, b"a\nb\nc\n", in_hunk, (1, 3), (1, 3));
        let answer = fx.show(&path, &meta(2), &[hunk]);
        assert_eq!(count(&answer, "drawn"), 0, "{on_disk:?} against {in_hunk:?}");
        assert_eq!(count(&answer, "skipped"), 1);
    }
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_hunk_that_changes_the_final_newline_is_refused_in_nvim() {
    let mut fx = Fx::new("eol-change");
    let base: &[u8] = b"a\nb\n";
    let end: &[u8] = b"a\nb";
    let path = fx.file("f.txt", end);
    fx.open(&path);
    let answer = fx.show(&path, &meta(2), &[hunk_of(1, base, end, (1, 2), (1, 2))]);
    assert_eq!(count(&answer, "drawn"), 1, "{answer:?}");
    fx.cursor(2);
    fx.feed(",r");
    assert_eq!(
        fx.said(),
        vec!["this hunk changes the file's line endings; revert it in the panel (x)".to_owned()]
    );
    assert_eq!(fx.lines(), vec![b"a".to_vec(), b"b".to_vec()]);
    assert!(!fx.modified());
    assert!(fx.events().is_empty());

    // And the other way round: the base had none, the end has one.
    let path = fx.file("g.txt", base);
    fx.open(&path);
    let answer = fx.show(&path, &meta(2), &[hunk_of(1, end, base, (1, 2), (1, 2))]);
    assert_eq!(count(&answer, "drawn"), 1);
    fx.cursor(1);
    fx.feed(",r");
    assert_eq!(
        fx.said(),
        vec!["this hunk changes the file's line endings; revert it in the panel (x)".to_owned()]
    );
    assert!(!fx.modified());
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_lf_hunk_in_a_dos_buffer_is_skipped_with_the_notice() {
    let mut fx = Fx::new("lf-in-dos");
    let path = fx.file("f.txt", b"one\r\ntwo\r\n");
    fx.open(&path);
    assert_eq!(text(&fx.nvim.eval("&fileformat")), "dos");
    let hunk = ShowHunk::from_file_lines(
        1,
        (1, 2),
        (1, 2),
        &[b"one\n".to_vec(), b"zwei\n".to_vec()],
        &[b"one\n".to_vec(), b"two\n".to_vec()],
    );
    let answer = fx.show(&path, &meta(2), &[hunk]);
    assert_eq!(count(&answer, "drawn"), 0);
    assert_eq!(count(&answer, "skipped"), 1);
    assert_eq!(text(get(&answer, "notice")), "1 hunk no longer matches this buffer");
    assert_eq!(fx.marks().count, 0);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn e37_falls_back_to_a_split() {
    let mut fx = Fx::new("e37");
    let first = fx.file("first.txt", b"first\n");
    let second = fx.file("second.txt", b"second\n");
    fx.nvim.lua("vim.o.hidden = false", vec![]);
    fx.open(&first);
    fx.nvim.request(
        "nvim_buf_set_lines",
        vec![
            Value::from(0),
            Value::from(0),
            Value::from(1),
            Value::from(true),
            Value::Array(vec![Value::from("unsaved")]),
        ],
    );
    let answer = fx
        .nvim
        .lua(OPEN_AND_SHOW_LUA, open_and_show_args(&second, Some(99), &meta(2), None));
    assert_eq!(get(&answer, "opened"), &Value::from(true), "{answer:?}");
    assert_eq!(fx.nvim.lua("return #vim.api.nvim_list_wins()", vec![]), Value::from(2));
    assert_eq!(
        bytes(&fx.nvim.lua("return vim.api.nvim_buf_get_name(0)", vec![])),
        second.to_str().unwrap().as_bytes()
    );
    assert_eq!(fx.cursor_line(), 1, "the line is clamped to the buffer");
    let first_modified = fx.nvim.lua(
        "local p = ... for _, b in ipairs(vim.api.nvim_list_bufs()) do \
           if vim.api.nvim_buf_get_name(b) == p then return vim.bo[b].modified end end return nil",
        vec![Value::from(first.to_str().unwrap())],
    );
    assert_eq!(first_modified, Value::from(true), "the unsaved buffer kept its change");
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn show_into_a_file_not_open_draws_nothing() {
    let mut fx = Fx::new("not-open");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    let bufs = "return #vim.api.nvim_list_bufs()";
    let before = fx.nvim.lua(bufs, vec![]);
    let answer = fx.show(&path, &meta(2), &t.hunks);
    assert_eq!(count(&answer, "drawn"), 0);
    assert_eq!(text(get(&answer, "notice")), "the file is not open in the editor");
    assert_eq!(count(&answer, "active"), 0);
    assert_eq!(fx.nvim.lua(bufs, vec![]), before, "no buffer was made for it");

    let gone = fx.scratch.0.join("files").join("never-there.txt");
    let answer = fx
        .nvim
        .lua(OPEN_AND_SHOW_LUA, open_and_show_args(&gone, None, &meta(2), None));
    assert_eq!(get(&answer, "opened"), &Value::from(false));
    assert_eq!(text(get(&answer, "error")), "the file does not exist");
    assert_eq!(fx.nvim.lua(bufs, vec![]), before);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_zero_length_side_is_refused_in_nvim() {
    let mut fx = Fx::new("zero");
    // The turn created the file.
    let end = b"x\ny\n";
    let path = fx.file("made.txt", end);
    fx.open(&path);
    let made = hunk_of(1, b"", end, (0, 0), (1, 2));
    let answer = fx.show(&path, &meta(2), &[made]);
    assert_eq!(count(&answer, "drawn"), 1, "{answer:?}");
    assert_eq!(fx.marks().adds, vec![0, 1]);
    fx.cursor(1);
    fx.feed(",r");
    assert_eq!(
        fx.said(),
        vec!["this hunk creates or empties the file; revert it in the panel (x)".to_owned()]
    );
    assert_eq!(fx.lines(), vec![b"x".to_vec(), b"y".to_vec()]);
    assert!(!fx.modified());

    // The turn emptied a file: there is nothing to draw over.
    let emptied = fx.file("emptied.txt", b"");
    fx.open(&emptied);
    let answer = fx.show(&emptied, &meta(2), &[hunk_of(1, end, b"", (1, 2), (0, 0))]);
    assert_eq!(count(&answer, "drawn"), 0);
    assert_eq!(count(&answer, "skipped"), 0);
    assert_eq!(get(&answer, "notice"), &Value::Nil);
    assert!(fx.events().is_empty());
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_malformed_call_is_an_error_not_a_skip() {
    let mut fx = Fx::new("malformed");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    fx.nvim.lua("_G.__show = ...", vec![Value::from(SHOW_LUA)]);
    let call = "return (pcall(assert(loadstring(_G.__show)), ...))";
    let mut args = show_args(&path, &meta(2), &t.hunks);
    // An eol name the module does not know.
    if let Value::Array(hunks) = &mut args[2] {
        if let Value::Map(entries) = &mut hunks[0] {
            for (k, v) in entries.iter_mut() {
                if k.as_str() == Some("new_eols") {
                    if let Value::Array(eols) = v {
                        eols[0] = Value::from("cr");
                    }
                }
            }
        }
    }
    assert_eq!(fx.nvim.lua(call, args), Value::from(false));
    let relative = show_args(Path::new("files/f.txt"), &meta(2), &t.hunks);
    assert_eq!(fx.nvim.lua(call, relative), Value::from(false), "a relative path");
    let mut bad_meta = show_args(&path, &meta(2), &t.hunks);
    bad_meta[1] = opts_map(&[("tab", Value::from(1))]);
    assert_eq!(fx.nvim.lua(call, bad_meta), Value::from(false));
    assert_eq!(fx.marks().count, 0);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_revert_reports_where_the_hunk_is_now() {
    let mut fx = Fx::new("moved");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    // A line added above both hunks moves them down by one.
    fx.nvim.request(
        "nvim_buf_set_lines",
        vec![
            Value::from(0),
            Value::from(0),
            Value::from(0),
            Value::from(true),
            Value::Array(vec![Value::from("added")]),
        ],
    );
    fx.cursor(22);
    fx.feed(",r");
    let mut want = vec![b"added".to_vec()];
    want.extend(as_lines(&edited(30, &[(5, &["X5", "X5b"])])));
    assert_eq!(fx.lines(), want, "the hunk was found where it moved to");
    let events = fx.events();
    assert_eq!(events.len(), 1);
    let event = parse_revert_event(&events[0]).expect("a revert event");
    assert_eq!(event.hunk, t.hunks[1], "the header as it was shown");
    assert_eq!(event.hunk.new_start, 18);
    assert_eq!(event.at_line, 19, "where the reverted lines start in the buffer now");
    let at = event.at_line as usize - 1;
    assert_eq!(
        fx.lines()[at..at + event.hunk.old_len as usize],
        as_lines(&event.hunk.old_bytes())[..],
        "the reverted lines are at at_line"
    );
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_loaded_buffer_through_a_link_wins_over_an_unloaded_one_of_the_name() {
    let mut fx = Fx::new("twin");
    let t = two_hunks();
    let path = fx.scratch.0.join("files").join("f.txt");
    // Listed before the file exists, so nvim does not know it as the same file as the link later.
    fx.nvim.lua(
        "local p = ... vim.api.nvim_cmd({ cmd = 'badd', args = { p }, magic = { file = false } }, {})",
        vec![Value::from(path.to_str().unwrap())],
    );
    std::fs::write(&path, &t.end).unwrap();
    let link = fx.scratch.0.join("files").join("link.txt");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    fx.open(&link);
    let bufs = fx.nvim.lua(
        "local out = {} for _, b in ipairs(vim.api.nvim_list_bufs()) do \
           table.insert(out, { vim.api.nvim_buf_get_name(b), vim.api.nvim_buf_is_loaded(b) }) end return out",
        vec![],
    );
    let bufs: Vec<(String, bool)> = items(&bufs)
        .iter()
        .map(|b| {
            let pair = items(b);
            (text(&pair[0]), pair[1].as_bool().unwrap())
        })
        .collect();
    assert!(
        bufs.contains(&(path.to_str().unwrap().to_owned(), false))
            && bufs.contains(&(link.to_str().unwrap().to_owned(), true)),
        "two buffers, the one of the name unloaded: {bufs:?}"
    );

    let answer = fx.show(&path, &meta(2), &t.hunks);
    assert_eq!(count(&answer, "drawn"), 2, "{answer:?}");
    assert!(fx.marks().count > 0, "drawn into the buffer on screen");
    assert_eq!(count(&fx.nvim.lua(CLEAR_LUA, clear_args(&path)), "active"), 0);
    assert_eq!(fx.marks().count, 0, "and cleared from it by the same name");
    fx.nvim.quit();
}

/// Thirty lines with lines 5, 15 and 25 each `width` bytes long, all of `fill`.
fn wide(fill: u8, width: usize) -> Vec<u8> {
    let long = String::from_utf8(vec![fill; width]).unwrap();
    let mut out = Vec::new();
    for i in 1..=30 {
        if i % 10 == 5 {
            out.extend_from_slice(long.as_bytes());
        } else {
            out.extend_from_slice(format!("a{i}").as_bytes());
        }
        out.push(b'\n');
    }
    out
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_revert_too_large_for_one_answer_is_refused() {
    let mut fx = Fx::new("too-large");
    // One hunk whose two sides hold 4.4 MB between them: its event could not travel.
    let base = wide(b'o', 2_200_000);
    let end = wide(b'n', 2_200_000);
    let hunks = vec![hunk_of(1, &base, &end, (2, 7), (2, 7))];
    let path = fx.file("f.txt", &end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &hunks), "drawn"), 1);
    fx.said();
    fx.cursor(5);
    fx.feed(",r");
    assert_eq!(
        fx.said(),
        vec!["this hunk is too large to revert here; revert it in the panel (x)".to_owned()]
    );
    assert_eq!(fx.lines(), as_lines(&end), "the buffer is unchanged");
    assert!(!fx.modified());
    assert!(fx.events().is_empty());
    assert!(fx.marks().count > 0, "the hunk is still drawn");
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_long_queue_comes_out_over_several_answers_in_order() {
    let mut fx = Fx::new("long-queue");
    // Three hunks of about 3 MB each: two fit in one answer, the third waits for the next.
    let base = wide(b'o', 1_500_000);
    let end = wide(b'n', 1_500_000);
    let hunks = vec![
        hunk_of(1, &base, &end, (2, 7), (2, 7)),
        hunk_of(2, &base, &end, (12, 7), (12, 7)),
        hunk_of(3, &base, &end, (22, 7), (22, 7)),
    ];
    let path = fx.file("f.txt", &end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &hunks), "drawn"), 3);
    for line in [25, 5, 15] {
        fx.cursor(line);
        fx.feed(",r");
    }
    assert_eq!(fx.lines(), as_lines(&base));

    let first = fx.nvim.lua(TAKE_EVENTS_LUA, vec![]);
    assert_eq!(get(&first, "more"), &Value::from(true));
    let second = fx.nvim.lua(TAKE_EVENTS_LUA, vec![]);
    assert_eq!(get(&second, "more"), &Value::from(false));
    let ids: Vec<Vec<u32>> = [&first, &second]
        .iter()
        .map(|answer| {
            items(get(answer, "events"))
                .iter()
                .map(|e| parse_revert_event(e).expect("a revert event").hunk.id)
                .collect()
        })
        .collect();
    assert_eq!(ids, vec![vec![3, 1], vec![2]], "in the order they happened, each once");
    assert!(fx.events().is_empty());
    fx.nvim.quit();
}
