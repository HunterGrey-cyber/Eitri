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

use eitri_core::companion::{install_args as companion_install_args, InstallKind, Sockets, INSTALL_LUA, TEARDOWN_LUA};
use eitri_core::review_editor::{
    clear_args, install_args, open_and_show_args, parse_revert_event, parse_unrevert_event, show_args, Owner, ShowHunk,
    ShowMeta, CLEAR_ALL_LUA, CLEAR_LUA, OPEN_AND_SHOW_LUA, REVIEW_LUA, REVIEW_LUA_VERSION, SHOW_LUA, TAKE_EVENTS_LUA,
};
use eitri_core::turn_review::Scope;
use embed_client::{opts_map, Embed};
use rmpv::Value;

/// `,` as the local leader, and `vim.notify` replaced by a recorder the test reads (nvim's own kept
/// as `_G.__own_notify` for a test that needs it).
const PREPARE_LUA: &str = "vim.g.maplocalleader = ',' _G.__said = {} _G.__own_notify = vim.notify \
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
local out = { count = 0, off_priority = 0, off_word_priority = 0, words = {}, virt_words = {}, chunks = {}, adds = {}, virt = {}, signs = {}, trackers = {} }
if not ns then return out end
for _, m in ipairs(vim.api.nvim_buf_get_extmarks(0, ns, 0, -1, { details = true })) do
  local row, d = m[2], m[4]
  out.count = out.count + 1
  -- nvim reports a priority only for a mark that draws something (not for the tracker).
  if (d.line_hl_group or d.virt_lines or d.sign_text or d.hl_group == 'DiffAdd') and d.priority ~= 90 then
    out.off_priority = out.off_priority + 1
  end
  if d.hl_group == 'DiffText' then
    table.insert(out.words, { row, m[3], d.end_col })
    if d.priority ~= 91 then out.off_word_priority = out.off_word_priority + 1 end
  end
  -- A line with word marks is filled by a text highlight: a line highlight would paint over them.
  if d.line_hl_group or d.hl_group == 'DiffAdd' then table.insert(out.adds, row) end
  if d.virt_lines then
    local texts = {}
    for _, l in ipairs(d.virt_lines) do
      local t, chunks = '', {}
      for _, c in ipairs(l) do
        t = t .. c[1]
        table.insert(chunks, { c[1], c[2] })
        if c[2] == 'DiffText' then table.insert(out.virt_words, c[1]) end
        if c[2] ~= 'DiffDelete' and c[2] ~= 'DiffText' then t = t .. '<not DiffDelete>' end
      end
      table.insert(texts, t)
      table.insert(out.chunks, chunks)
    end
    table.insert(out.virt, { row = row, above = d.virt_lines_above and true or false, lines = texts })
  end
  if d.sign_text then table.insert(out.signs, { row = row, text = vim.trim(d.sign_text), hl = d.sign_hl_group }) end
  if d.end_row and not d.hl_group then table.insert(out.trackers, { row = row, end_row = d.end_row }) end
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
    /// (row, byte start, byte end) of buffer DiffText marks.
    words: Vec<(i64, i64, i64)>,
    virt_words: Vec<String>,
    /// (text, highlight) chunks of each removed virtual line.
    chunks: Vec<Vec<(String, String)>>,
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
            Some((_, with)) => out.extend(with.iter().map(ToString::to_string)),
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
        assert_eq!(int(get(&v, "off_word_priority")), 0, "word marks have priority 91");
        Marks {
            words: items(get(&v, "words"))
                .iter()
                .map(|m| {
                    let m = items(m);
                    (int(&m[0]), int(&m[1]), int(&m[2]))
                })
                .collect(),
            virt_words: items(get(&v, "virt_words")).iter().map(text).collect(),
            chunks: items(get(&v, "chunks"))
                .iter()
                .map(|line| {
                    items(line)
                        .iter()
                        .map(|c| {
                            let c = items(c);
                            (text(&c[0]), text(&c[1]))
                        })
                        .collect()
                })
                .collect(),
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

    /// Waits until the module holds `n` events nobody took yet (an autocommand queued them).
    fn wait_for_events(&mut self, n: i64) {
        self.nvim.wait_until(&format!("{n} queued review events"), |nvim| {
            int(&nvim.lua("return #_G.__eitri_review.events", vec![])) == n
        });
    }

    /// The `kind` of every event taken, in order.
    fn event_kinds(&mut self) -> Vec<String> {
        self.events().iter().map(|e| text(get(e, "kind"))).collect()
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

/// Draws one whole-file hunk so word assertions can name byte columns directly.
fn word_hunk(fx: &mut Fx, old: &[&str], new: &[&str]) -> PathBuf {
    let base = numbered(old);
    let end = numbered(new);
    let path = fx.file("words.txt", &end);
    fx.open(&path);
    let hunk = hunk_of(1, &base, &end, (1, old.len() as u32), (1, new.len() as u32));
    assert_eq!(count(&fx.show(&path, &meta(2), &[hunk]), "drawn"), 1);
    path
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn one_changed_word_has_byte_marks_and_removed_line_chunks() {
    let mut fx = Fx::new("word-one");
    word_hunk(&mut fx, &["let total = a + b;"], &["let total = a + c;"]);
    let marks = fx.marks();
    assert_eq!(marks.words, vec![(0, 16, 17)]);
    assert_eq!(marks.virt_words, vec!["b"]);
    assert_eq!(
        marks.chunks,
        vec![vec![
            ("let total = a + ".to_owned(), "DiffDelete".to_owned()),
            ("b".to_owned(), "DiffText".to_owned()),
            (";".to_owned(), "DiffDelete".to_owned()),
        ]]
    );
    assert_eq!(marks.adds, vec![0]);
    assert_eq!(marks.virt, vec![(0, true, vec!["let total = a + b;".to_owned()])]);
    // The added line is filled to the window's edge by a text highlight (never a line highlight, which would be
    // painted over the word mark), so the word shows against the fill.
    let fill = fx.nvim.lua(
        "local ns = vim.api.nvim_get_namespaces().eitri_review \
         for _, m in ipairs(vim.api.nvim_buf_get_extmarks(0, ns, 0, -1, { details = true })) do \
           if m[4].hl_group == 'DiffAdd' then return { m[2], m[4].end_row, m[4].end_col, m[4].hl_eol and 1 or 0, m[4].line_hl_group and 1 or 0 } end \
         end",
        vec![],
    );
    assert_eq!(items(&fill).iter().map(int).collect::<Vec<_>>(), vec![0, 1, 0, 1, 0]);
    assert_eq!(
        marks.off_priority, 0,
        "line, virtual and sign marks stay at priority 90"
    );
    // The tracker, added line, removed line and sign now also have one word mark.
    assert_eq!(marks.count, 5);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn two_changed_words_with_common_text_between_stay_separate() {
    let mut fx = Fx::new("word-two");
    word_hunk(&mut fx, &["let a = keep + b;"], &["let c = keep + d;"]);
    let marks = fx.marks();
    assert_eq!(marks.words, vec![(0, 4, 5), (0, 15, 16)]);
    assert_eq!(marks.virt_words, vec!["a", "b"]);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn whitespace_between_changed_words_is_one_span() {
    let mut fx = Fx::new("word-whitespace");
    word_hunk(&mut fx, &["let a  b;"], &["let c  d;"]);
    let marks = fx.marks();
    assert_eq!(marks.words, vec![(0, 4, 8)]);
    assert_eq!(marks.virt_words, vec!["a  b"]);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn multibyte_words_use_byte_columns() {
    let mut fx = Fx::new("word-utf8");
    word_hunk(&mut fx, &["let café = keep;"], &["let 茶葉 = keep;"]);
    let marks = fx.marks();
    assert_eq!(marks.words, vec![(0, 4, 10)]);
    let lines = fx.lines();
    let (row, start, end) = marks.words[0];
    assert_eq!(&lines[row as usize][start as usize..end as usize], "茶葉".as_bytes());
    assert_eq!(marks.virt_words, vec!["café"]);
    fx.nvim.quit();
}

/// Draws one whole-file hunk in a buffer whose tab settings are `setup` (Lua run in that buffer
/// before the draw), so the removed line's tab expansion can be read back.
fn tab_hunk(fx: &mut Fx, setup: &str, old: &[&str], new: &[&str]) {
    let end = numbered(new);
    let path = fx.file("tabs.txt", &end);
    fx.open(&path);
    fx.nvim.lua(setup, vec![]);
    let hunk = hunk_of(1, &numbered(old), &end, (1, old.len() as u32), (1, new.len() as u32));
    assert_eq!(count(&fx.show(&path, &meta(2), &[hunk]), "drawn"), 1);
}

/// The chunks of the one removed line as plain strings.
fn shown_chunks(fx: &mut Fx) -> Vec<(String, String)> {
    let marks = fx.marks();
    assert_eq!(marks.chunks.len(), 1, "{:?}", marks.chunks);
    marks.chunks[0].clone()
}

fn chunk(text: &str, group: &str) -> (String, String) {
    (text.to_owned(), group.to_owned())
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn tab_indentation_of_a_removed_line_is_spaces_that_line_up_with_the_buffer() {
    for ts in [4, 8] {
        let mut fx = Fx::new(&format!("tabs-indent-{ts}"));
        tab_hunk(
            &mut fx,
            &format!("vim.bo.tabstop = {ts}"),
            &["\t\treturn \"proof\""],
            &["\t\treturn \"done\""],
        );
        let marks = fx.marks();
        let expected = format!("{}return \"proof\"", " ".repeat(2 * ts));
        assert_eq!(marks.virt, vec![(0, true, vec![expected])], "tabstop {ts}");
        // The buffer's own `return` starts at the same display column as the removed line's.
        assert_eq!(int(&fx.nvim.eval("virtcol([1, 3])")), 2 * ts as i64 + 1, "tabstop {ts}");
        fx.nvim.quit();
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_tab_after_text_goes_to_the_next_tab_stop() {
    let mut fx = Fx::new("tabs-mid");
    tab_hunk(&mut fx, "vim.bo.tabstop = 4", &["ab\tcd"], &["ab\tXY"]);
    // `ab` is two columns in, so the tab is two wide.
    assert_eq!(
        shown_chunks(&mut fx),
        vec![chunk("ab  ", "DiffDelete"), chunk("cd", "DiffText")]
    );
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn tab_width_carries_across_word_diff_chunk_boundaries() {
    let mut fx = Fx::new("tabs-chunks");
    tab_hunk(&mut fx, "vim.bo.tabstop = 4", &["x\ty\tz"], &["x\tQ\tz"]);
    // `x\t` ends at column 4, the changed `y` is column 4, and the next tab starts at column 5.
    assert_eq!(
        shown_chunks(&mut fx),
        vec![
            chunk("x   ", "DiffDelete"),
            chunk("y", "DiffText"),
            chunk("   z", "DiffDelete")
        ]
    );
    fx.nvim.quit();

    // A chunk that starts with a tab: the tab's width depends on the chunk before it.
    let mut fx = Fx::new("tabs-chunks-lead");
    tab_hunk(&mut fx, "vim.bo.tabstop = 4", &["a\tb\tc"], &["Z\tb\tc"]);
    assert_eq!(
        shown_chunks(&mut fx),
        vec![chunk("a", "DiffText"), chunk("   b   c", "DiffDelete")]
    );
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn variable_tab_stops_and_wide_characters_are_counted() {
    let mut fx = Fx::new("tabs-vts");
    tab_hunk(&mut fx, "vim.bo.vartabstop = '2,3'", &["\t\tx"], &["\t\ty"]);
    // Stops at columns 2 and 5, like the buffer's own line.
    assert_eq!(
        shown_chunks(&mut fx),
        vec![chunk("     ", "DiffDelete"), chunk("x", "DiffText")]
    );
    assert_eq!(int(&fx.nvim.eval("virtcol([1, 3])")), 6);
    fx.nvim.quit();

    let mut fx = Fx::new("tabs-wide");
    tab_hunk(&mut fx, "vim.bo.tabstop = 4", &["茶\tx"], &["茶\ty"]);
    // The wide character fills two columns, so the tab is two wide.
    assert_eq!(
        shown_chunks(&mut fx),
        vec![chunk("茶  ", "DiffDelete"), chunk("x", "DiffText")]
    );
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn other_control_characters_in_a_removed_line_stay_printable() {
    let mut fx = Fx::new("tabs-control");
    tab_hunk(
        &mut fx,
        "vim.bo.tabstop = 4",
        &["a\rb\x1bc", "\x1b\tz"],
        &["whole", "other"],
    );
    let marks = fx.marks();
    // `^[` is two columns wide, so the tab after it is two wide.
    assert_eq!(
        marks.virt,
        vec![(0, true, vec!["a^Mb^[c".to_owned(), "^[  z".to_owned()])]
    );
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_whole_line_rewrite_has_only_line_colours() {
    let mut fx = Fx::new("word-whole");
    word_hunk(&mut fx, &["before"], &["after"]);
    let marks = fx.marks();
    assert!(marks.words.is_empty());
    assert!(marks.virt_words.is_empty());
    assert_eq!(marks.adds, vec![0]);
    assert_eq!(marks.chunks, vec![vec![("before".to_owned(), "DiffDelete".to_owned())]]);
    assert_eq!(marks.count, 4);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn long_lines_skip_words_on_either_side_at_the_byte_limit() {
    for (old, new, marked) in [
        (format!("{} a", "x".repeat(398)), format!("{} b", "x".repeat(398)), true),
        (
            format!("{} aa", "x".repeat(398)),
            format!("{} b", "x".repeat(398)),
            false,
        ),
        (
            format!("{} a", "x".repeat(398)),
            format!("{} bb", "x".repeat(398)),
            false,
        ),
    ] {
        let mut fx = Fx::new("word-bytes");
        word_hunk(&mut fx, &[&old], &[&new]);
        let marks = fx.marks();
        assert_eq!(marks.words.len(), usize::from(marked));
        assert_eq!(marks.virt_words.len(), usize::from(marked));
        assert_eq!(marks.adds, vec![0]);
        assert_eq!(marks.virt[0].2, vec![old]);
        fx.nvim.quit();
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn only_the_first_hundred_changed_pairs_get_words() {
    let mut fx = Fx::new("word-pairs");
    let old: Vec<String> = (0..120).map(|i| format!("let value_{i} = old;")).collect();
    let new: Vec<String> = (0..120).map(|i| format!("let value_{i} = new;")).collect();
    word_hunk(
        &mut fx,
        &old.iter().map(String::as_str).collect::<Vec<_>>(),
        &new.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let marks = fx.marks();
    let expected: Vec<(i64, i64, i64)> = (0..100)
        .map(|i| {
            let col = format!("let value_{i} = ").len() as i64;
            (i, col, col + 3)
        })
        .collect();
    assert_eq!(marks.words, expected);
    assert_eq!(marks.virt_words, vec!["old"; 100]);
    assert_eq!(marks.adds.len(), 120);
    assert_eq!(marks.virt[0].2.len(), 120);
    assert_eq!(marks.count, 223);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn excessive_tokens_skip_words_on_either_side() {
    for (old, new) in [
        (format!("{}old;", "+".repeat(199)), format!("{}new;", "+".repeat(198))),
        (format!("{}old;", "+".repeat(198)), format!("{}new;", "+".repeat(199))),
    ] {
        let mut fx = Fx::new("word-tokens");
        word_hunk(&mut fx, &[&old], &[&new]);
        let marks = fx.marks();
        assert!(marks.words.is_empty());
        assert!(marks.virt_words.is_empty());
        assert_eq!(marks.count, 4);
        fx.nvim.quit();
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn unequal_changed_line_counts_get_no_word_marks() {
    let mut fx = Fx::new("word-unequal");
    word_hunk(
        &mut fx,
        &["let a = old;", "let b = old;"],
        &["let a = new;", "let b = new;", "let c = new;"],
    );
    let marks = fx.marks();
    assert!(marks.words.is_empty());
    assert!(marks.virt_words.is_empty());
    assert_eq!(marks.adds, vec![0, 1, 2]);
    assert_eq!(marks.count, 6);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn word_marks_follow_a_redrawn_hunk_and_clear_without_stale_marks() {
    let mut fx = Fx::new("word-follow");
    let path = word_hunk(&mut fx, &["let a = old;"], &["let a = new;"]);
    let drawn = fx.marks();
    assert_eq!(drawn.words, vec![(0, 8, 11)]);
    fx.cursor(1);
    fx.feed(",r");
    assert_eq!(fx.event_kinds(), vec!["revert"]);
    let reverted = fx.marks();
    assert!(reverted.words.is_empty());
    assert_eq!(reverted.count, 2, "only the two invisible tracking marks remain");
    fx.nvim
        .lua("vim.api.nvim_buf_set_lines(0, 0, 1, true, { 'let a = new;' })", vec![]);
    fx.wait_for_events(1);
    assert_eq!(fx.event_kinds(), vec!["unrevert"]);
    assert_eq!(fx.marks(), drawn, "the word marks were drawn again exactly once");
    assert_eq!(count(&fx.nvim.lua(CLEAR_LUA, clear_args(&path)), "active"), 0);
    assert_eq!(fx.marks().count, 0, "clearing also removes every DiffText mark");
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_failed_word_pair_rolls_back_its_marks_and_keeps_drawing() {
    let mut fx = Fx::new("word-failure");
    // Fail after the first word mark was installed, so a partial pair must be removed too.
    fx.nvim.lua(
        "local original, words = vim.api.nvim_buf_set_extmark, 0 \
         vim.api.nvim_buf_set_extmark = function(...) \
           local opts = select(5, ...) \
           if opts.hl_group == 'DiffText' then \
             words = words + 1 if words == 2 then error('word mark failed') end \
           end \
           return original(...) \
         end",
        vec![],
    );
    word_hunk(
        &mut fx,
        &["let a = keep + b;", "let e = old;"],
        &["let c = keep + d;", "let e = new;"],
    );
    let marks = fx.marks();
    assert_eq!(marks.words, vec![(1, 8, 11)], "only the second pair has word marks");
    assert_eq!(marks.virt_words, vec!["old"]);
    assert_eq!(
        marks.chunks[0],
        vec![("let a = keep + b;".to_owned(), "DiffDelete".to_owned())]
    );
    assert_eq!(marks.adds, vec![0, 1]);
    assert_eq!(marks.count, 6);
    fx.nvim.quit();
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
    let marks = fx.marks();
    assert!(
        marks.adds.is_empty() && marks.virt.is_empty() && marks.signs.is_empty() && marks.trackers.is_empty(),
        "nothing is drawn over a reverted hunk: {marks:?}"
    );
    assert_eq!(marks.count, 4, "two invisible marks follow each reverted hunk");

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
    assert_eq!(before.autocmds, 0, "no overlay, no autocommand: {before:?}");
    assert_eq!(before.globals, vec!["__eitri_review".to_owned()]);

    let answer = fx.nvim.lua(REVIEW_LUA, install_args(Owner::Embedded));
    assert_eq!(get(&answer, "installed"), &Value::from(false));
    assert_eq!(get(&answer, "replaced"), &Value::from(false));
    assert_eq!(fx.nvim.footprint_of(&["eitri_review"], &["__eitri_review"]), before);
    assert_eq!(fx.nvim.lua(spaces, vec![]), spaces_before);

    // The first overlay registers the module's autocommands; a second show of the same buffer, or
    // an overlay in another buffer, adds none: they are the module's, not a buffer's.
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    fx.show(&path, &meta(2), &t.hunks);
    let shown = fx.nvim.footprint_of(&["eitri_review"], &["__eitri_review"]);
    assert_eq!(shown.autocmds, 6, "{shown:?}");
    fx.show(&path, &meta(2), &t.hunks);
    assert_eq!(fx.nvim.footprint_of(&["eitri_review"], &["__eitri_review"]), shown);
    let other = fx.file("other.txt", &t.end);
    fx.open(&other);
    fx.show(&other, &meta(2), &t.hunks);
    assert_eq!(fx.nvim.footprint_of(&["eitri_review"], &["__eitri_review"]), shown);

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
    nvim.lua(
        INSTALL_LUA,
        companion_install_args(chan, &Sockets::default(), &InstallKind::Agent),
    );
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

/// The footprint the module leaves in the editor: its autocommands and whether its table is set.
fn leftovers(fx: &mut Fx) -> (usize, Vec<String>) {
    let f = fx.nvim.footprint_of(&["eitri_review"], &["__eitri_review"]);
    (f.autocmds, f.globals)
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn undoing_a_buffer_revert_draws_the_hunk_again_and_says_so() {
    let mut fx = Fx::new("undo-follow");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    let drawn = fx.marks();
    let after_first = edited(30, &[(20, &["Y20", "Y20b"])]);

    fx.cursor(5);
    fx.feed(",r");
    assert_eq!(fx.event_kinds(), vec!["revert".to_owned()]);
    assert_eq!(fx.lines(), as_lines(&after_first));

    // `u` brings the text back, and the overlay follows: the hunk is drawn again, an event says it
    // was un-reverted, and its revert works once more.
    fx.feed("u");
    fx.wait_for_events(1);
    assert_eq!(fx.lines(), as_lines(&t.end));
    assert_eq!(fx.marks(), drawn, "the hunk is drawn exactly as before the revert");
    let events = fx.events();
    assert_eq!(events.len(), 1);
    assert_eq!(text(get(&events[0], "kind")), "unrevert");
    let undone = parse_unrevert_event(&events[0]).expect("an unrevert event");
    assert_eq!(undone.hunk, t.hunks[0]);
    assert_eq!(undone.meta, meta(2));
    assert_eq!(undone.path, path.to_str().unwrap());
    assert_eq!(undone.at_line, 2, "where the hunk's lines are now");
    assert!(parse_revert_event(&events[0]).is_err(), "a different kind of event");

    fx.cursor(5);
    fx.feed(",r");
    assert!(fx.said().is_empty(), "{:?}", fx.said());
    assert_eq!(fx.lines(), as_lines(&after_first));
    assert_eq!(fx.event_kinds(), vec!["revert".to_owned()]);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn drawing_a_hunk_again_after_an_undo_made_with_the_overlay_closed_says_so_in_order() {
    let mut fx = Fx::new("undo-after-close");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    assert!(fx.events().is_empty(), "a first draw says nothing");

    fx.cursor(5);
    fx.feed(",r");
    // Closing the overlay ends the tracking: the undo below is seen by nobody.
    fx.feed(",q");
    fx.feed("u");
    fx.nvim.flush_scheduled();
    assert_eq!(fx.lines(), as_lines(&t.end));

    // Drawn again, the hunk's text is found back: one `unrevert`, queued behind the `revert` and
    // the close, and none for the hunk that was never reverted.
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    let events = fx.events();
    let kinds: Vec<String> = events.iter().map(|e| text(get(e, "kind"))).collect();
    assert_eq!(kinds, vec!["revert", "off", "unrevert"]);
    let back = parse_unrevert_event(&events[2]).expect("an unrevert event");
    assert_eq!(back.hunk, t.hunks[0]);
    assert_eq!(back.at_line, 2);

    // Said once: another draw has nothing more to say.
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    assert!(fx.events().is_empty());

    // The same through an open, which is how the panel's `o` reaches the editor.
    fx.cursor(5);
    fx.feed(",r");
    fx.feed(",q");
    fx.feed("u");
    fx.nvim.flush_scheduled();
    fx.events();
    let opened = fx.nvim.lua(
        OPEN_AND_SHOW_LUA,
        open_and_show_args(&path, Some(5), &meta(2), Some(&t.hunks)),
    );
    assert_eq!(get(&opened, "opened"), &Value::from(true), "{opened:?}");
    assert_eq!(fx.event_kinds(), vec!["unrevert".to_owned()]);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_hunk_still_reverted_when_drawn_again_is_not_reported_as_undone() {
    let mut fx = Fx::new("reshow-still-reverted");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    fx.cursor(21);
    fx.feed(",r");
    fx.feed(",q");
    // Its text is not back, so nothing is drawn over it and nothing is said.
    let again = fx.show(&path, &meta(2), &t.hunks);
    assert_eq!(count(&again, "drawn"), 1, "{again:?}");
    assert_eq!(fx.event_kinds(), vec!["revert".to_owned(), "off".to_owned()]);

    // A hunk that removed lines at the end of the file holds its new side and its old side at once:
    // its revert and its undo cannot be told apart, so none is claimed.
    let base = numbered(&["a", "b", "c", "d"]);
    let end = numbered(&["a", "b", "c"]);
    let hunk = hunk_of(1, &base, &end, (1, 4), (1, 3));
    let tail = fx.file("tail.txt", &end);
    fx.open(&tail);
    assert_eq!(
        count(&fx.show(&tail, &meta(2), std::slice::from_ref(&hunk)), "drawn"),
        1
    );
    fx.cursor(2);
    fx.feed(",r");
    assert_eq!(fx.lines().len(), 4, "the last line is back");
    fx.feed(",q");
    fx.events();
    fx.show(&tail, &meta(2), std::slice::from_ref(&hunk));
    assert!(fx.events().is_empty());
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn redoing_an_undone_revert_reads_as_the_revert_again() {
    let mut fx = Fx::new("redo-follow");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    let after_first = edited(30, &[(20, &["Y20", "Y20b"])]);

    fx.cursor(5);
    fx.feed(",r");
    fx.feed("u");
    fx.wait_for_events(2);
    assert_eq!(fx.event_kinds(), vec!["revert".to_owned(), "unrevert".to_owned()]);

    fx.feed("<C-r>");
    fx.wait_for_events(1);
    assert_eq!(fx.lines(), as_lines(&after_first));
    let events = fx.events();
    let again = parse_revert_event(&events[0]).expect("a revert event");
    assert_eq!(again.hunk, t.hunks[0]);
    assert_eq!(again.at_line, 2);
    let marks = fx.marks();
    assert!(
        marks.adds.len() == 2 && marks.signs.len() == 1,
        "only the second hunk is drawn: {marks:?}"
    );

    // And the whole round again: undo, redo, undo.
    fx.feed("u");
    fx.wait_for_events(1);
    assert_eq!(fx.event_kinds(), vec!["unrevert".to_owned()]);
    fx.feed("<C-r>");
    fx.wait_for_events(1);
    fx.feed("u");
    fx.wait_for_events(2);
    assert_eq!(fx.event_kinds(), vec!["revert".to_owned(), "unrevert".to_owned()]);
    assert_eq!(fx.lines(), as_lines(&t.end));
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn typing_the_hunk_back_or_away_is_followed_like_an_undo() {
    let mut fx = Fx::new("retype");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    fx.cursor(5);
    fx.feed(",r");
    assert_eq!(fx.event_kinds(), vec!["revert".to_owned()]);

    // The user types the hunk's new lines over the reverted ones: it is not reverted any more.
    fx.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 1, 8, true, { 'a2', 'a3', 'a4', 'X5', 'X5b', 'a6', 'a7', 'a8' })",
        vec![],
    );
    fx.wait_for_events(1);
    assert_eq!(fx.lines(), as_lines(&t.end));
    assert_eq!(fx.event_kinds(), vec!["unrevert".to_owned()]);
    assert_eq!(fx.marks().trackers.len(), 2, "drawn again");

    // And typing the base text over a drawn hunk reads as its revert.
    fx.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 1, 9, true, { 'a2', 'a3', 'a4', 'a5', 'a6', 'a7', 'a8' })",
        vec![],
    );
    fx.wait_for_events(1);
    let events = fx.events();
    assert_eq!(parse_revert_event(&events[0]).expect("a revert event").hunk, t.hunks[0]);
    assert_eq!(fx.marks().trackers.len(), 1, "only the second hunk is drawn");

    // Any other edit of a drawn hunk still just clears it, with no event.
    fx.cursor(18);
    fx.feed("A!<Esc>");
    fx.nvim.wait_until(
        "the second hunk cleared (the first hunk's two ghost marks remain)",
        |nvim| {
            int(&nvim.lua(
                "return #vim.api.nvim_buf_get_extmarks(0, vim.api.nvim_get_namespaces().eitri_review, 0, -1, \
             { details = true })",
                vec![],
            )) == 2
        },
    );
    assert!(fx.events().is_empty());
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reverted_hunk_is_still_followed_after_the_overlay_is_shown_again() {
    let mut fx = Fx::new("carried");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    fx.cursor(21);
    fx.feed(",r");
    assert_eq!(fx.event_kinds(), vec!["revert".to_owned()]);

    // The panel draws the file again (another hunk opened): the first hunk is drawn, the reverted
    // one is not, and is not reported as a hunk that no longer matches either.
    let again = fx.show(&path, &meta(2), &t.hunks);
    assert_eq!(count(&again, "drawn"), 1);
    assert_eq!(count(&again, "skipped"), 0, "{again:?}");
    assert!(fx.events().is_empty(), "no event: the host asked");

    fx.feed("u");
    fx.wait_for_events(1);
    let events = fx.events();
    assert_eq!(
        parse_unrevert_event(&events[0]).expect("an unrevert event").hunk,
        t.hunks[1]
    );
    assert_eq!(fx.marks().trackers.len(), 2);

    // A different review of the file does not inherit the ghost, and one with nothing drawn keeps
    // no mark.
    fx.cursor(21);
    fx.feed(",r");
    fx.events();
    let other = fx.show(&path, &meta(3), &t.hunks[1..]);
    assert_eq!(count(&other, "drawn"), 0, "{other:?}");
    assert_eq!(fx.marks().count, 0);
    assert_eq!(count(&other, "active"), 0);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn closing_the_last_overlay_removes_the_autocommands() {
    let mut fx = Fx::new("close-clean");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    let second = fx.file("g.txt", &t.end);
    assert_eq!(leftovers(&mut fx), (0, vec!["__eitri_review".to_owned()]));

    // `<localleader>q`.
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    assert_eq!(leftovers(&mut fx).0, 6);
    fx.feed(",q");
    assert_eq!(leftovers(&mut fx).0, 0, "closing the overlay leaves no autocommand");
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
    // What stays is the module table, with the close not yet taken by the host.
    assert_eq!(leftovers(&mut fx).1, vec!["__eitri_review".to_owned()]);
    let events = fx.events();
    assert_eq!(events.len(), 1);
    assert_eq!(text(get(&events[0], "why")), "user");

    // The next show brings the autocommands back, and they work.
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    assert_eq!(leftovers(&mut fx).0, 6);
    fx.cursor(5);
    fx.feed("A!<Esc>");
    fx.wait_for_marks(5);
    assert_eq!(fx.marks().trackers.len(), 1);

    // Two buffers: closing one keeps the autocommands the other needs.
    fx.open(&second);
    assert_eq!(count(&fx.show(&second, &meta(2), &t.hunks), "drawn"), 2);
    fx.feed(",q");
    assert_eq!(leftovers(&mut fx).0, 6);
    fx.open(&path);
    fx.feed(",q");
    assert_eq!(leftovers(&mut fx).0, 0);
    fx.events();

    // Every other way an overlay goes: the host's clear, its clear-all, a reload, the buffer's end.
    fx.show(&path, &meta(2), &t.hunks);
    assert_eq!(count(&fx.nvim.lua(CLEAR_LUA, clear_args(&path)), "active"), 0);
    assert_eq!(leftovers(&mut fx).0, 0, "clear");
    fx.show(&path, &meta(2), &t.hunks);
    assert_eq!(count(&fx.nvim.lua(CLEAR_ALL_LUA, vec![]), "active"), 0);
    assert_eq!(leftovers(&mut fx).0, 0, "clear all");
    fx.show(&path, &meta(2), &t.hunks);
    fx.nvim
        .lua("vim.api.nvim_cmd({ cmd = 'edit', bang = true }, {})", vec![]);
    fx.nvim.flush_scheduled();
    assert_eq!(leftovers(&mut fx).0, 0, "reload");
    assert_eq!(fx.event_kinds(), vec!["off".to_owned()]);
    fx.show(&path, &meta(2), &t.hunks);
    fx.nvim
        .lua("vim.api.nvim_cmd({ cmd = 'bwipeout', bang = true }, {})", vec![]);
    fx.nvim.flush_scheduled();
    assert_eq!(leftovers(&mut fx).0, 0, "the buffer wiped");
    assert_eq!(fx.event_kinds(), vec!["off".to_owned()]);

    // The module still answers, and the host's next call finds it.
    let answer = fx.nvim.lua(TAKE_EVENTS_LUA, vec![]);
    assert_eq!(get(&answer, "missing"), &Value::Nil);
    assert_eq!(count(&answer, "active"), 0);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_companion_review_closed_by_the_user_still_goes_with_the_glue() {
    let mut fx = Fx::bare("companion-close");
    let chan = install_companion(&mut fx.nvim);
    fx.nvim
        .lua(REVIEW_LUA, install_args(Owner::Companion { channel: chan }));
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks), "drawn"), 2);
    fx.feed(",q");
    assert_eq!(leftovers(&mut fx), (0, vec!["__eitri_review".to_owned()]));

    // The glue's teardown still removes the module, though the review had been closed.
    assert_eq!(fx.nvim.lua(TEARDOWN_LUA, vec![Value::from(chan)]), Value::from(true));
    assert_eq!(leftovers(&mut fx), (0, Vec::new()));
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_fully_reverted_file_is_still_followed_after_it_is_shown_again() {
    let mut fx = Fx::new("all-reverted");
    let t = two_hunks();
    let path = fx.file("f.txt", &t.end);
    fx.open(&path);
    assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks[..1]), "drawn"), 1);
    fx.cursor(5);
    fx.feed(",r");
    assert_eq!(fx.event_kinds(), vec!["revert".to_owned()]);

    // The panel opens the same file again: nothing is drawn, but the reverted hunk is still
    // followed, with its keys and the autocommands that notice its text coming back.
    let again = fx.show(&path, &meta(2), &t.hunks[..1]);
    assert_eq!(count(&again, "drawn"), 0, "{again:?}");
    assert_eq!(count(&again, "kept"), 1, "{again:?}");
    assert_eq!(count(&again, "active"), 1, "{again:?}");
    assert_eq!(leftovers(&mut fx).0, 6);
    assert_eq!(fx.review_maps().len(), 4);
    assert!(fx.events().is_empty(), "no event: the host asked");

    fx.feed("u");
    fx.wait_for_events(1);
    let events = fx.events();
    assert_eq!(
        parse_unrevert_event(&events[0]).expect("an unrevert event").hunk,
        t.hunks[0]
    );
    assert_eq!(fx.marks().trackers.len(), 1, "drawn again");

    // Closing the last overlay takes its marks and the autocommands with it, ghosts included.
    fx.feed(",r");
    fx.events();
    fx.show(&path, &meta(2), &t.hunks[..1]);
    fx.feed(",q");
    assert_eq!(leftovers(&mut fx).0, 0);
    assert_eq!(fx.marks().count, 0);
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_line_added_around_a_reverted_hunk_does_not_strand_its_undo_tracking() {
    // Rows (from 0) of the hunk's first line: 1; its seven base lines are rows 1..=7.
    for (what, edit) in [
        ("above", "vim.api.nvim_buf_set_lines(0, 1, 1, true, { 'ins' })"),
        ("below", "vim.api.nvim_buf_set_lines(0, 8, 8, true, { 'ins' })"),
        (
            "inside, then removed",
            "vim.api.nvim_buf_set_lines(0, 4, 4, true, { 'ins' }) vim.api.nvim_buf_set_lines(0, 4, 5, true, {})",
        ),
        (
            "above and below",
            "vim.api.nvim_buf_set_lines(0, 8, 8, true, { 'ins' }) vim.api.nvim_buf_set_lines(0, 1, 1, true, { 'ins' })",
        ),
    ] {
        let mut fx = Fx::new("gravity");
        let t = two_hunks();
        let path = fx.file("f.txt", &t.end);
        fx.open(&path);
        assert_eq!(count(&fx.show(&path, &meta(2), &t.hunks[..1]), "drawn"), 1, "{what}");
        fx.cursor(5);
        fx.feed(",r");
        assert_eq!(fx.event_kinds(), vec!["revert".to_owned()], "{what}");

        fx.nvim.lua(edit, vec![]);
        fx.nvim.flush_scheduled();
        assert!(fx.events().is_empty(), "{what}: the text is not back yet");

        // Where the hunk's lines are now: below a line added above it.
        let first = if what.starts_with("above") { 2 } else { 1 };
        fx.nvim.lua(
            &format!(
                "vim.api.nvim_buf_set_lines(0, {first}, {}, true, \
                 {{ 'a2', 'a3', 'a4', 'X5', 'X5b', 'a6', 'a7', 'a8' }})",
                first + 7
            ),
            vec![],
        );
        fx.wait_for_events(1);
        let events = fx.events();
        assert_eq!(
            parse_unrevert_event(&events[0])
                .unwrap_or_else(|e| panic!("{what}: {e}"))
                .at_line,
            first as u32 + 1,
            "{what}"
        );
        assert_eq!(fx.marks().trackers.len(), 1, "{what}: drawn again");
        fx.nvim.quit();
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn follow_edits_cost_does_not_grow_with_reverted_hunks() {
    const TIME_FOLLOW_LUA: &str = r#"
local best = math.huge
for _ = 1, 5 do
  collectgarbage('collect')
  local start = vim.uv.hrtime()
  vim.api.nvim_exec_autocmds('TextChanged', { buffer = 0 })
  best = math.min(best, vim.uv.hrtime() - start)
end
return best
"#;
    let mut fx = Fx::new("follow-cost");
    let base: Vec<u8> = (1..=10000)
        .flat_map(|i| {
            format!("line {i:05} old word with enough surrounding text to fill about seventy bytes\n").into_bytes()
        })
        .collect();
    let mut measure = |n: u32| {
        let mut lines = as_lines(&base);
        for i in 1..=n {
            let row = (i * 50 - 1) as usize;
            lines[row] = String::from_utf8(lines[row].clone())
                .unwrap()
                .replace("old", "new")
                .into_bytes();
        }
        let end = numbered(
            &lines
                .iter()
                .map(|l| std::str::from_utf8(l).unwrap())
                .collect::<Vec<_>>(),
        );
        let hunks: Vec<_> = (1..=n)
            .map(|i| hunk_of(i, &base, &end, (i * 50, 1), (i * 50, 1)))
            .collect();
        let path = fx.file(&format!("hunks-{n}.txt"), &end);
        fx.open(&path);
        assert_eq!(count(&fx.show(&path, &meta(2), &hunks), "drawn"), i64::from(n));
        for i in (1..=n).rev() {
            fx.cursor(i64::from(i * 50));
            fx.feed(",r");
        }
        assert_eq!(fx.events().len(), n as usize);
        assert_eq!(fx.lines(), as_lines(&base));
        let elapsed = int(&fx.nvim.lua(TIME_FOLLOW_LUA, vec![]));
        fx.nvim.lua(CLEAR_LUA, clear_args(&path));
        fx.write();
        elapsed
    };
    let t200 = measure(200);
    let t20 = measure(20);
    eprintln!("TextChanged best of 5: 200 reverted hunks {t200} ns; 20 reverted hunks {t20} ns");
    assert!(
        t200 <= 4 * t20,
        "search shares buffer work across hunks: t200={t200} ns, t20={t20} ns"
    );
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reverted_hunk_found_by_search_is_still_unreverted() {
    let mut fx = Fx::new("undo-search");
    let base = numbered(&["a", "OLD", "c"]);
    let end = numbered(&["a", "NEW", "c"]);
    let hunk = hunk_of(1, &base, &end, (1, 3), (1, 3));
    let mut moved = vec!["padding"; 100];
    moved.extend(["a", "NEW", "c"]);
    moved.extend(["tail"; 100]);
    let path = fx.file("f.txt", &numbered(&moved));
    fx.open(&path);
    let mut relocated = hunk.clone();
    relocated.old_start = 101;
    relocated.new_start = 101;
    assert_eq!(count(&fx.show(&path, &meta(2), &[relocated]), "drawn"), 1);
    fx.cursor(102);
    fx.feed(",r");
    assert_eq!(fx.event_kinds(), vec!["revert"]);
    // A whole-buffer edit leaves the two tracking marks at its boundaries. Undo must find the
    // hunk in the middle again, even though neither mark points to its first line. Restoring
    // the new text and replacing it before TextChanged leaves the hunk reverted throughout.
    fx.nvim.lua(
        "vim.cmd('undo') vim.api.nvim_buf_set_lines(0, 0, -1, true, { 'a', 'OLD', 'c' }) \
         vim.api.nvim_exec_autocmds('TextChanged', { buffer = 0 })",
        vec![],
    );
    assert!(fx.events().is_empty());
    assert_eq!(fx.marks().count, 2, "only the two tracking marks remain");
    fx.feed("u");
    fx.wait_for_events(1);
    let events = fx.events();
    let back = parse_unrevert_event(&events[0]).expect("an unrevert event");
    assert_eq!(back.at_line, 101);
    assert_eq!(back.hunk.new_lines, hunk.new_lines);
    assert_eq!(fx.marks().trackers.len(), 1);
    assert!(fx.events().is_empty(), "the hunk is reported once");
    fx.nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_whole_buffer_replacement_that_moves_the_text_is_searched_for_the_hunk() {
    let base = numbered(&["a", "OLD", "c"]);
    let end = numbered(&["a", "NEW", "c"]);
    let hunk = hunk_of(1, &base, &end, (1, 3), (1, 3));
    let setup = |name: &str| {
        let mut fx = Fx::new(name);
        let path = fx.file("f.txt", &end);
        fx.open(&path);
        assert_eq!(
            count(&fx.show(&path, &meta(2), std::slice::from_ref(&hunk)), "drawn"),
            1
        );
        fx.cursor(2);
        fx.feed(",r");
        assert_eq!(fx.event_kinds(), vec!["revert".to_owned()]);
        fx
    };

    // What a formatter or `:%!` does: every line is set again, with lines added around the hunk.
    let mut fx = setup("search-one");
    fx.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 0, -1, true, { 'intro', 'a', 'NEW', 'c', 'tail' })",
        vec![],
    );
    fx.wait_for_events(1);
    let events = fx.events();
    let back = parse_unrevert_event(&events[0]).expect("an unrevert event");
    assert_eq!(back.hunk, hunk);
    assert_eq!(back.at_line, 2, "where it was found");
    assert_eq!(fx.marks().trackers.len(), 1, "drawn again");
    fx.nvim.quit();

    // Two places that hold the text cannot be told apart: it stays reverted.
    let mut fx = setup("search-two");
    fx.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 0, -1, true, { 'intro', 'a', 'NEW', 'c', 'x', 'y', 'a', 'NEW', 'c', 'z' })",
        vec![],
    );
    fx.nvim.flush_scheduled();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(fx.events().is_empty());
    assert!(fx.marks().trackers.is_empty());
    fx.nvim.quit();

    // Nor is it found where the old text is also held.
    let mut fx = setup("search-none");
    fx.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 0, -1, true, { 'intro', 'a', 'something else', 'c', 'tail' })",
        vec![],
    );
    fx.nvim.flush_scheduled();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(fx.events().is_empty());
    fx.nvim.quit();
}

fn live_meta(turn_id: &str, read_lines: u64) -> eitri_core::review_editor::LiveMeta {
    eitri_core::review_editor::LiveMeta {
        tab: 4,
        session: "sess-1".to_owned(),
        turn_id: turn_id.to_owned(),
        turn: 2,
        running: false,
        read_lines,
    }
}

fn live_show(fx: &mut Fx, path: &Path, turn_id: &str, read_lines: u64, hunks: &[ShowHunk]) -> Value {
    use eitri_core::review_editor::{live_show_args, parse_live_show, LIVE_SHOW_LUA};
    let answer = fx.nvim.lua(
        LIVE_SHOW_LUA,
        live_show_args(path, &live_meta(turn_id, read_lines), hunks),
    );
    parse_live_show(&answer).expect("a complete live drawing answer");
    answer
}

fn live_two(fx: &mut Fx) -> (PathBuf, TwoHunks) {
    let two = two_hunks();
    let path = fx.file("live.txt", &two.end);
    fx.open(&path);
    fx.nvim.command("setlocal autoread");
    (path, two)
}

const EDITOR_POSITION_LUA: &str = "return { vim.api.nvim_get_current_win(), vim.api.nvim_get_current_buf(), \
    vim.api.nvim_win_get_cursor(0), vim.api.nvim_get_mode() }";
const OVERLAY_SNAPSHOT_LUA: &str = r#"
local b = vim.api.nvim_get_current_buf()
return vim.inspect({ vim.api.nvim_buf_get_extmarks(b, _G.__eitri_review.ns, 0, -1, { details = true }),
  vim.api.nvim_buf_get_keymap(b, 'n'), _G.__eitri_review.overlays[b] })
"#;

#[test]
#[ignore = "needs nvim"]
fn shown_answers_only_for_a_buffer_in_a_non_floating_window() {
    use eitri_core::review_editor::{live_shown_args, parse_live_shown, LIVE_SHOWN_LUA};
    let mut fx = Fx::bare("shown-windows");
    let hidden = fx.file("hidden.txt", b"hidden\n");
    let floating = fx.file("float.txt", b"float\n");
    let split = fx.file("split.txt", b"split\n");
    fx.nvim.lua(
        r#"local hidden, floating, split = ...
        vim.fn.bufload(vim.fn.bufadd(hidden))
        local b = vim.fn.bufadd(floating) vim.fn.bufload(b)
        vim.api.nvim_open_win(b, false, { relative = 'editor', row = 1, col = 1, width = 10, height = 2 })
        vim.cmd.tabnew()
        vim.api.nvim_cmd({ cmd = 'split', args = { split }, magic = { file = false } }, {})
        vim.cmd.tabprevious()"#,
        vec![
            Value::from(hidden.to_str().unwrap()),
            Value::from(floating.to_str().unwrap()),
            Value::from(split.to_str().unwrap()),
        ],
    );
    let answer = fx.nvim.lua(LIVE_SHOWN_LUA, live_shown_args(&[hidden, floating, split]));
    assert_eq!(parse_live_shown(&answer, 3).unwrap(), vec![false, false, true]);
    assert!(fx
        .nvim
        .lua("return _G.__eitri_review == nil", vec![])
        .as_bool()
        .unwrap());
}

#[test]
#[ignore = "needs nvim"]
fn live_show_draws_into_a_shown_buffer_and_changes_no_window_cursor_or_mode() {
    let mut fx = Fx::new("live-position");
    let (path, two) = live_two(&mut fx);
    fx.cursor(15);
    for insert in [false, true] {
        fx.nvim.lua(CLEAR_ALL_LUA, vec![]);
        if insert {
            fx.nvim.input("i");
            fx.nvim.wait_until("insert mode", |n| {
                get(&n.request("nvim_get_mode", vec![]), "mode").as_str() == Some("i")
            });
        }
        let before = fx.nvim.lua(EDITOR_POSITION_LUA, vec![]);
        assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
        assert_eq!(fx.nvim.lua(EDITOR_POSITION_LUA, vec![]), before);
        if insert {
            fx.nvim.input("<Esc>");
        }
    }
    // Drawing into another shown buffer must leave the active window alone too.
    fx.nvim.command("new");
    for insert in [false, true] {
        if insert {
            fx.nvim.input("i");
            fx.nvim.wait_until("insert mode in the other window", |n| {
                get(&n.request("nvim_get_mode", vec![]), "mode").as_str() == Some("i")
            });
        }
        let before = fx.nvim.lua(EDITOR_POSITION_LUA, vec![]);
        assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
        assert_eq!(fx.nvim.lua(EDITOR_POSITION_LUA, vec![]), before);
        if insert {
            fx.nvim.input("<Esc>");
        }
    }
}

#[test]
#[ignore = "needs nvim"]
fn live_show_reloads_an_unmodified_buffer_first() {
    let mut fx = Fx::new("live-reload");
    let two = two_hunks();
    let path = fx.file("reload.txt", &two.base);
    fx.open(&path);
    fx.nvim.command("setlocal autoread");
    std::fs::write(&path, &two.end).unwrap();
    let answer = live_show(&mut fx, &path, "t1", 32, &two.hunks);
    assert_eq!(count(&answer, "drawn"), 2);
    assert_eq!(fx.lines(), as_lines(&two.end));
    assert!(!fx.modified());
}

#[test]
#[ignore = "needs nvim"]
fn live_show_leaves_a_modified_buffer_and_its_overlay_alone() {
    let mut fx = Fx::new("live-modified");
    let (path, two) = live_two(&mut fx);
    fx.show(&path, &meta(2), &two.hunks);
    fx.cursor(5);
    fx.feed(",r");
    fx.wait_for_events(1);
    assert!(fx.modified());
    fx.nvim.input("i");
    fx.nvim.wait_until("insert mode", |n| {
        get(&n.request("nvim_get_mode", vec![]), "mode").as_str() == Some("i")
    });
    let before = fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]);
    let position = fx.nvim.lua(EDITOR_POSITION_LUA, vec![]);
    let answer = live_show(&mut fx, &path, "t2", 32, &two.hunks);
    assert_eq!(get(&answer, "left"), &Value::from(true));
    assert_eq!(fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]), before);
    assert_eq!(fx.nvim.lua(EDITOR_POSITION_LUA, vec![]), position);
    fx.nvim.input("<Esc>");
    fx.feed("u");
    fx.wait_for_events(2);
    assert_eq!(fx.event_kinds(), vec!["revert", "unrevert"]);
}

#[test]
#[ignore = "needs nvim"]
fn a_settle_over_a_modified_buffer_takes_its_own_running_drawing_off() {
    let mut fx = Fx::new("live-settle-modified");
    let (path, two) = live_two(&mut fx);
    draw_running(&mut fx, &path, &two.hunks);
    assert!(fx.marks().count > 0);
    fx.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 0, 0, true, { 'typed meanwhile' })",
        vec![],
    );
    assert!(fx.modified());
    let typed = fx.lines();

    // Another turn's settle leaves the drawing as it is.
    let before = fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]);
    let answer = live_show(&mut fx, &path, "t2", 32, &two.hunks);
    assert_eq!(get(&answer, "left"), &Value::from(true));
    assert_eq!(fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]), before);

    // Its own turn's settle cannot draw here, so the drawing that says "running" goes.
    let answer = live_show(&mut fx, &path, "t1", 32, &two.hunks);
    assert_eq!(get(&answer, "left"), &Value::from(true));
    assert_eq!(count(&answer, "drawn"), 0);
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
    assert_eq!(fx.lines(), typed, "the person's text is untouched");
    assert!(fx.modified());
    assert_eq!(fx.event_kinds(), vec!["off"], "the host learns the drawing went");
}

#[test]
#[ignore = "needs nvim"]
fn a_settle_takes_its_own_hidden_running_drawing_off() {
    let mut fx = Fx::new("live-settle-hidden");
    let (path, two) = live_two(&mut fx);
    draw_running(&mut fx, &path, &two.hunks);
    assert!(fx.marks().count > 0);
    fx.nvim.command("set hidden");
    fx.nvim.command("enew");
    let position = fx.nvim.lua(EDITOR_POSITION_LUA, vec![]);
    // A different turn cannot settle this drawing.
    live_show(&mut fx, &path, "t2", 32, &two.hunks);
    assert_eq!(
        fx.nvim.lua("return vim.tbl_count(_G.__eitri_review.overlays)", vec![]),
        Value::from(1)
    );
    let answer = live_show(&mut fx, &path, "t1", 32, &two.hunks);
    assert_eq!(get(&answer, "shown"), &Value::from(false));
    assert_eq!(count(&answer, "drawn"), 0);
    assert_eq!(fx.nvim.lua(EDITOR_POSITION_LUA, vec![]), position);
    assert_eq!(fx.event_kinds(), vec!["off"]);
    fx.open(&path);
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
}

#[test]
#[ignore = "needs nvim"]
fn live_show_never_replaces_an_overlay_that_is_not_live() {
    let mut fx = Fx::new("live-explicit");
    let (path, two) = live_two(&mut fx);
    fx.show(&path, &meta(2), &two.hunks);
    assert!(!fx.modified());
    let before = fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]);
    let answer = live_show(&mut fx, &path, "t1", 32, &two.hunks);
    assert_eq!(get(&answer, "left"), &Value::from(true));
    assert_eq!(fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]), before);
}

#[test]
#[ignore = "needs nvim"]
fn live_show_with_autoread_off_runs_no_checktime() {
    let mut fx = Fx::new("live-no-autoread");
    let (path, two) = live_two(&mut fx);
    fx.nvim.lua(
        "vim.bo.autoread = false vim.g.changed = 0 vim.api.nvim_create_autocmd('FileChangedShell', \
        { callback = function() vim.g.changed = vim.g.changed + 1 end })",
        vec![],
    );
    std::fs::write(&path, &two.base).unwrap();
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 0);
    assert_eq!(fx.nvim.eval("g:changed"), Value::from(0));
    assert_eq!(fx.lines(), as_lines(&two.end));
    assert_eq!(fx.marks().count, 0);
}

#[test]
#[ignore = "needs nvim"]
fn live_show_draws_nothing_when_the_line_count_differs() {
    let mut fx = Fx::new("live-line-count");
    let (path, two) = live_two(&mut fx);
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 33, &two.hunks), "drawn"), 0);
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
}

#[test]
#[ignore = "needs nvim"]
fn live_show_skips_kept_hunks_of_the_same_turn_only() {
    let mut fx = Fx::new("live-kept");
    let (path, two) = live_two(&mut fx);
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
    fx.nvim.lua("_G.__eitri_review.keep_current()", vec![]);
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 0);
    assert_eq!(count(&live_show(&mut fx, &path, "t2", 32, &two.hunks), "drawn"), 2);
    let mut changed = two.hunks.clone();
    changed[0].old_lines[0].text = b"another base".to_vec();
    let answer = live_show(&mut fx, &path, "t1", 32, &changed);
    assert_eq!(count(&answer, "drawn"), 1);
    assert_eq!(count(&answer, "skipped"), 0, "kept hunks are deliberately excluded");
    assert_eq!(count(&fx.show(&path, &meta(2), &two.hunks), "drawn"), 2);
}

#[test]
#[ignore = "needs nvim"]
fn an_emptied_file_counts_as_one_line() {
    let mut fx = Fx::new("live-empty");
    let path = fx.file("empty.txt", b"");
    fx.open(&path);
    fx.nvim.command("setlocal autoread");
    let hunk = hunk_of(1, b"gone\n", b"", (1, 1), (0, 0));
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 0, &[hunk]), "drawn"), 1);
    assert_eq!(fx.marks().virt, vec![(0, true, vec!["gone".to_owned()])]);
}

#[test]
#[ignore = "needs nvim"]
fn an_emptied_file_is_not_drawn_over_a_buffer_that_holds_text() {
    let mut fx = Fx::new("live-empty-text");
    let path = fx.file("rewritten.txt", b"rewritten after the read\n");
    fx.open(&path);
    fx.nvim.command("setlocal autoread");
    let hunk = hunk_of(1, b"gone\n", b"", (1, 1), (0, 0));
    let answer = live_show(&mut fx, &path, "t1", 0, &[hunk]);
    assert_eq!(count(&answer, "drawn"), 0);
    assert_eq!(count(&answer, "skipped"), 1);
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
}

#[test]
#[ignore = "needs nvim"]
fn live_show_says_not_shown_for_a_hidden_buffer_and_a_float_only_one() {
    let mut fx = Fx::new("live-not-shown");
    let two = two_hunks();
    let hidden = fx.file("hidden.txt", &two.end);
    let floating = fx.file("float.txt", &two.end);
    fx.nvim.command("set hidden");
    fx.open(&hidden);
    fx.nvim.command("enew");
    fx.nvim.lua(
        r#"local b = vim.fn.bufadd(...) vim.fn.bufload(b)
        vim.api.nvim_open_win(b, false, { relative = 'editor', row = 1, col = 1, width = 10, height = 2 })"#,
        vec![Value::from(floating.to_str().unwrap())],
    );
    for path in [&hidden, &floating] {
        let answer = live_show(&mut fx, path, "t1", 32, &two.hunks);
        assert_eq!(get(&answer, "shown"), &Value::from(false), "{answer:?}");
        assert_eq!(count(&answer, "drawn"), 0);
        assert_eq!(get(&answer, "left"), &Value::from(false));
    }
    assert!(fx
        .nvim
        .lua("return next(_G.__eitri_review.overlays) == nil", vec![])
        .as_bool()
        .unwrap());
}

#[test]
#[ignore = "needs nvim"]
fn live_show_reloads_a_changed_file_while_its_window_is_in_insert_mode() {
    let mut fx = Fx::new("live-reload-insert");
    let two = two_hunks();
    let path = fx.file("reload-insert.txt", &two.base);
    fx.open(&path);
    fx.nvim.command("setlocal autoread");
    fx.nvim.input("i");
    fx.nvim.wait_until("insert mode", |n| {
        get(&n.request("nvim_get_mode", vec![]), "mode").as_str() == Some("i")
    });
    let before = fx.nvim.lua(EDITOR_POSITION_LUA, vec![]);
    std::fs::write(&path, &two.end).unwrap();
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
    assert_eq!(fx.nvim.lua(EDITOR_POSITION_LUA, vec![]), before);
    assert_eq!(fx.lines(), as_lines(&two.end));
    assert!(!fx.modified());
    fx.nvim.input("<Esc>");
}

#[test]
#[ignore = "needs nvim"]
fn typing_in_a_big_buffer_reads_it_whole_only_when_a_search_is_due() {
    let mut fx = Fx::new("live-read-cost");
    let (path, two) = live_two(&mut fx);
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
    let marks_before = fx.marks().count;
    assert!(marks_before > 0);
    let full_reads = |fx: &mut Fx| {
        fx.nvim
            .lua("local n = _G.__full _G.__full = 0 return n", vec![])
            .as_i64()
            .unwrap()
    };
    fx.nvim.lua(
        "_G.__full = 0 local orig = vim.api.nvim_buf_get_lines \
        vim.api.nvim_buf_get_lines = function(b, s, e, strict) \
          if s == 0 and e == -1 then _G.__full = _G.__full + 1 end \
          return orig(b, s, e, strict) end",
        vec![],
    );
    // Typing in a line the drawing does not cover reads only the rows each comparison needs.
    fx.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 9, 10, true, { 'typed' }) \
        vim.api.nvim_exec_autocmds('TextChangedI', { buffer = 0 })",
        vec![],
    );
    assert_eq!(full_reads(&mut fx), 0);
    assert_eq!(fx.marks().count, marks_before, "the drawing is still up");
    // A reverted hunk whose text may be back is searched for in one read of the buffer.
    fx.cursor(5);
    fx.feed(",r");
    full_reads(&mut fx);
    fx.nvim
        .lua("vim.api.nvim_exec_autocmds('TextChanged', { buffer = 0 })", vec![]);
    assert_eq!(full_reads(&mut fx), 1);
}

#[test]
#[ignore = "needs nvim"]
fn a_second_live_draw_keeps_its_keys() {
    let mut fx = Fx::new("live-second");
    let (path, two) = live_two(&mut fx);
    live_show(&mut fx, &path, "t1", 32, &two.hunks);
    let keys = fx.review_maps();
    assert!(keys.contains(&",a".to_owned()));
    assert!(keys.contains(&"]h".to_owned()));
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
    assert_eq!(fx.review_maps(), keys);
}

#[test]
#[ignore = "needs nvim"]
fn live_clear_clears_a_live_overlay_and_never_an_o_one() {
    use eitri_core::review_editor::LIVE_CLEAR_LUA;
    let mut fx = Fx::new("live-clear");
    let (path, two) = live_two(&mut fx);
    live_show(&mut fx, &path, "t1", 32, &two.hunks);
    let answer = fx.nvim.lua(LIVE_CLEAR_LUA, clear_args(&path));
    assert_eq!(get(&answer, "cleared"), &Value::from(true));
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
    fx.show(&path, &meta(2), &two.hunks);
    let before = fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]);
    let answer = fx.nvim.lua(LIVE_CLEAR_LUA, clear_args(&path));
    assert_eq!(get(&answer, "cleared"), &Value::from(false));
    assert_eq!(fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]), before);
}

#[test]
#[ignore = "needs nvim"]
fn no_message_from_a_live_draw_a_skip_or_a_timer_reload() {
    let mut fx = Fx::new("live-quiet");
    let (path, two) = live_two(&mut fx);
    fx.nvim.command("messages clear");
    let mut hunks = two.hunks.clone();
    hunks[1].new_lines[0].text = b"mismatch".to_vec();
    let answer = live_show(&mut fx, &path, "t1", 32, &hunks);
    assert_eq!(count(&answer, "drawn"), 1);
    assert_eq!(count(&answer, "skipped"), 1);
    assert!(get(&answer, "notice").is_nil());
    // A reload within live_show must not queue an off event for the drawing it replaces.
    std::fs::write(&path, &two.base).unwrap();
    let answer = live_show(&mut fx, &path, "t1", 30, &[]);
    assert_eq!(count(&answer, "pending"), 0);
    std::fs::write(&path, &two.end).unwrap();
    live_show(&mut fx, &path, "t1", 32, &two.hunks);
    std::fs::write(&path, &two.base).unwrap();
    fx.nvim.command("checktime");
    assert_eq!(fx.marks().count, 0);
    let events = fx.events();
    assert_eq!(events.len(), 1);
    assert_eq!(text(get(&events[0], "why")), "reload");
    assert!(fx.said().is_empty());
    assert_eq!(
        fx.nvim.lua(
            "return vim.api.nvim_exec2('messages', { output = true }).output",
            vec![]
        ),
        Value::from("")
    );
}

#[test]
#[ignore = "needs nvim"]
fn a_reload_in_a_live_draw_that_then_leaves_the_file_says_the_old_drawing_went() {
    let mut fx = Fx::new("live-reload-left");
    let (path, two) = live_two(&mut fx);
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
    // The person's own autocommand modifies the buffer once it is reloaded, so the new drawing is
    // not made.
    fx.nvim.lua(
        "vim.api.nvim_create_autocmd('FileChangedShellPost', { once = true, callback = function(a) \
         vim.api.nvim_buf_set_lines(a.buf, 0, 0, false, { 'added' }) end })",
        vec![],
    );
    std::fs::write(&path, &two.base).unwrap();
    let answer = live_show(&mut fx, &path, "t1", 30, &[]);
    assert_eq!(get(&answer, "left"), &Value::from(true));
    assert_eq!(fx.marks().count, 0);
    let events = fx.events();
    assert_eq!(events.len(), 1);
    assert_eq!(text(get(&events[0], "kind")), "off");
    assert_eq!(text(get(&events[0], "why")), "reload");
    // Its number is the answer's: an answer that drew the file after it would carry a later one.
    assert_eq!(get(&events[0], "seq"), get(&answer, "off_seq"));
    assert_eq!(get(&answer, "off_seq"), &Value::from(1));
}

#[test]
#[ignore = "needs nvim"]
fn an_empty_live_overlay_is_removed_with_its_keys() {
    let mut fx = Fx::new("live-empty-overlay");
    let (path, two) = live_two(&mut fx);
    let mut hunks = two.hunks.clone();
    for h in &mut hunks {
        h.new_lines[0].text = b"mismatch".to_vec();
    }
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &hunks), "active"), 0);
    assert!(fx.review_maps().is_empty());
    live_show(&mut fx, &path, "t1", 32, &two.hunks);
    fx.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 0, -1, true, { 'edited elsewhere' }) \
        vim.api.nvim_exec_autocmds('TextChanged', { buffer = 0 })",
        vec![],
    );
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
    assert_eq!(fx.nvim.footprint(&["eitri_review"]).autocmds, 0);
}

#[test]
#[ignore = "needs nvim"]
fn a_live_drawing_maps_no_key_another_map_holds() {
    let mut fx = Fx::new("live-held-keys");
    let (path, two) = live_two(&mut fx);
    fx.nvim.lua(
        "vim.keymap.set('n', ']h', 'zz', { buffer = true, desc = 'existing local' }) \
        vim.keymap.set('n', '<localleader>q', 'gg', { desc = 'existing global' })",
        vec![],
    );
    let maps = "return vim.inspect({ vim.fn.maparg(']h', 'n', false, true), vim.fn.maparg('<localleader>q', 'n', false, true) })";
    let before = fx.nvim.lua(maps, vec![]);
    live_show(&mut fx, &path, "t1", 32, &two.hunks);
    assert_eq!(fx.nvim.lua(maps, vec![]), before);
    assert!(fx.review_maps().contains(&",a".to_owned()));
    fx.show(&path, &meta(2), &two.hunks);
    assert_ne!(fx.nvim.lua(maps, vec![]), before);
    fx.nvim.lua(CLEAR_LUA, clear_args(&path));
    assert_eq!(fx.nvim.lua(maps, vec![]), before);
}

#[test]
#[ignore = "needs nvim"]
fn every_answer_carries_pending_and_live_only() {
    use eitri_core::review_editor::LIVE_CLEAR_LUA;
    let mut fx = Fx::new("live-answers");
    let (path, two) = live_two(&mut fx);
    let assert_status = |v: &Value, pending: u64, live: bool| {
        assert_eq!(get(v, "pending").as_u64(), Some(pending), "{v:?}");
        assert_eq!(get(v, "live_only").as_bool(), Some(live), "{v:?}");
    };
    assert_status(&fx.show(&path, &meta(2), &two.hunks), 0, false);
    assert_status(
        &fx.nvim
            .lua(OPEN_AND_SHOW_LUA, open_and_show_args(&path, None, &meta(2), None)),
        0,
        false,
    );
    assert_status(&fx.nvim.lua(CLEAR_LUA, clear_args(&path)), 0, true);
    assert_status(&live_show(&mut fx, &path, "t1", 32, &two.hunks), 0, true);
    fx.feed(",q");
    assert_status(&fx.nvim.lua(LIVE_CLEAR_LUA, clear_args(&path)), 1, true);
    assert_status(&fx.nvim.lua(CLEAR_ALL_LUA, vec![]), 1, true);
    assert_status(&fx.nvim.lua(TAKE_EVENTS_LUA, vec![]), 0, true);
    let missing = fx.file("not-open.txt", b"x\n");
    assert_status(&fx.show(&missing, &meta(2), &[]), 0, true);
    let absent = fx.scratch.0.join("absent.txt");
    assert_status(
        &fx.nvim
            .lua(OPEN_AND_SHOW_LUA, open_and_show_args(&absent, None, &meta(2), None)),
        0,
        true,
    );
}

#[test]
#[ignore = "needs nvim"]
fn live_show_takes_a_path_with_spaces_cjk_and_newline() {
    let mut fx = Fx::new("live-path");
    let two = two_hunks();
    let path = fx.file("space 中\nfile.txt", &two.end);
    fx.open(&path);
    fx.nvim.command("setlocal autoread");
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
    assert_eq!(fx.lines(), as_lines(&two.end));
    assert!(fx.said().is_empty());
}

#[test]
fn live_arguments_bound_paths_and_preserve_turn_metadata() {
    use eitri_core::review_editor::{live_show_args, live_shown_args, MAX_SHOWN_PATHS};
    let paths: Vec<_> = (0..MAX_SHOWN_PATHS + 1)
        .map(|i| PathBuf::from(format!("/file {i}")))
        .collect();
    let args = live_shown_args(&paths);
    assert_eq!(args.len(), 1);
    assert_eq!(items(&args[0]).len(), MAX_SHOWN_PATHS);
    assert_eq!(items(&args[0])[0], Value::Binary(b"/file 0".to_vec()));
    let meta = live_meta("stable-turn", 0);
    let args = live_show_args(Path::new("/space 中\nfile"), &meta, &[]);
    assert_eq!(args[0], Value::Binary("/space 中\nfile".as_bytes().to_vec()));
    assert_eq!(get(&args[1], "scope"), &Value::from("turn"));
    assert_eq!(get(&args[1], "turn_id"), &Value::from("stable-turn"));
    assert_eq!(get(&args[1], "running"), &Value::from(meta.running));
    assert_eq!(get(&args[1], "read_lines"), &Value::from(meta.read_lines));
}

#[test]
fn live_answer_parsers_refuse_incomplete_or_malformed_answers() {
    use eitri_core::review_editor::{parse_live_show, parse_live_shown};
    let shown = |values| opts_map(&[("shown", Value::Array(values))]);
    assert_eq!(
        parse_live_shown(&shown(vec![Value::from(false), Value::from(true)]), 2).unwrap(),
        vec![false, true]
    );
    assert!(parse_live_shown(&shown(vec![Value::from(true)]), 2).is_err());
    assert!(parse_live_shown(&shown(vec![Value::from(1)]), 1).is_err());
    assert_eq!(
        parse_live_shown(&opts_map(&[("shown", Value::Map(vec![]))]), 0).unwrap(),
        Vec::<bool>::new()
    );
    let answer = opts_map(&[
        ("drawn", Value::from(1)),
        ("shown", Value::from(true)),
        ("left", Value::from(false)),
        ("skipped", Value::from(0)),
        ("active", Value::from(1)),
        ("pending", Value::from(2)),
        ("live_only", Value::from(true)),
    ]);
    assert_eq!(parse_live_show(&answer).unwrap().pending, 2);
    let entries = answer.as_map().unwrap();
    for i in 0..entries.len() {
        let mut bad = entries.clone();
        bad.remove(i);
        assert!(parse_live_show(&Value::Map(bad)).is_err());
    }
    for name in ["drawn", "skipped"] {
        let mut bad = entries.clone();
        bad.iter_mut().find(|(k, _)| k.as_str() == Some(name)).unwrap().1 = Value::from(u64::MAX);
        assert!(parse_live_show(&Value::Map(bad)).is_err());
    }
}

#[test]
#[ignore = "needs nvim"]
fn live_show_uses_the_global_autoread_value_when_the_buffer_inherits_it() {
    let mut fx = Fx::new("live-global-autoread");
    let (path, two) = live_two(&mut fx);
    fx.nvim.command("setglobal noautoread");
    fx.nvim.command("setlocal autoread<");
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 0);
    fx.nvim.command("setglobal autoread");
    fx.nvim.command("setlocal autoread<");
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
}

/// The drawn buffer inherits the global `autoread` (on) while the current buffer is another one with
/// `noautoread` set locally: the drawn buffer's own value decides, so it is reloaded and drawn.
#[test]
#[ignore = "needs nvim"]
fn live_show_reads_the_global_autoread_not_the_current_buffers() {
    let mut fx = Fx::new("live-global-not-current");
    let (path, two) = live_two(&mut fx);
    fx.nvim.command("setglobal autoread");
    // `set {option}<` leaves the buffer with no local value of its own (`setlocal {option}<` would copy
    // the global one into it, and the fallback would never be read).
    fx.nvim.command("set autoread<");
    fx.nvim.command("vsplit");
    fx.nvim.command("enew");
    fx.nvim.command("setlocal noautoread");
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
}

#[test]
#[ignore = "needs nvim"]
fn live_show_leaves_saved_reverted_hunks_followed() {
    let mut fx = Fx::new("live-saved-ghost");
    let (path, two) = live_two(&mut fx);
    live_show(&mut fx, &path, "t1", 32, &two.hunks);
    fx.cursor(5);
    fx.feed(",r");
    fx.wait_for_events(1);
    fx.write();
    assert!(!fx.modified());
    let before = fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]);
    std::fs::write(&path, &two.end).unwrap();
    let answer = live_show(&mut fx, &path, "t2", 32, &two.hunks);
    assert_eq!(get(&answer, "left"), &Value::from(true));
    assert_eq!(fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]), before);
    fx.feed("u");
    fx.wait_for_events(2);
    assert_eq!(fx.event_kinds(), vec!["revert", "unrevert"]);
}

#[test]
#[ignore = "needs nvim"]
fn live_show_kept_fingerprints_drop_the_oldest_turn_at_the_bound() {
    use eitri_core::review_editor::live_show_args;
    let mut fx = Fx::new("live-kept-bound");
    let path = fx.file("kept.txt", b"new\n");
    let other = fx.file("other.txt", b"new\n");
    fx.open(&path);
    fx.nvim.command("setlocal autoread");
    fx.nvim.lua(
        "local p = ... vim.api.nvim_cmd({ cmd = 'split', args = { p } }, {}) vim.bo.autoread = true",
        vec![Value::from(other.to_str().unwrap())],
    );
    let hunk = hunk_of(1, b"old\n", b"new\n", (1, 1), (1, 1));
    let mut args = live_show_args(&path, &live_meta("start", 1), &[hunk]);
    args.push(Value::from(other.to_str().unwrap()));
    let result = fx.nvim.lua(
        r#"
        local path, meta, hunks, other = ...
        local r = _G.__eitri_review
        local function drawn(file, turn)
          meta.turn_id = tostring(turn)
          hunks[1].old_lines[1] = 'old' .. turn
          return r.live_show(file, meta, hunks).drawn
        end
        local function keep(file, turn)
          assert(drawn(file, turn) == 1)
          vim.api.nvim_buf_call(vim.fn.bufnr(file), r.keep_current)
        end
        for turn = 1, 4095 do keep(path, turn) end
        -- A later keep of the oldest turn on another file belongs to that same record.
        keep(other, 1)
        keep(path, 4096)
        return { drawn(path, 1), drawn(other, 1), drawn(path, 2), drawn(path, 4096) }
        "#,
        args,
    );
    assert_eq!(
        items(&result),
        vec![Value::from(1), Value::from(1), Value::from(0), Value::from(0)]
    );
}

fn three_hunks() -> TwoHunks {
    let base = edited(50, &[]);
    let end = edited(
        50,
        &[(5, &["X5", "X5b"]), (20, &["Y20", "Y20b"]), (40, &["Z40", "Z40b"])],
    );
    let hunks = vec![
        hunk_of(1, &base, &end, (2, 7), (2, 8)),
        hunk_of(2, &base, &end, (17, 7), (18, 8)),
        hunk_of(3, &base, &end, (37, 7), (39, 8)),
    ];
    TwoHunks { base, end, hunks }
}

const REVIEW_COMMANDS_LUA: &str = r#"
local local_commands = vim.api.nvim_buf_get_commands(0, {})
local global_commands = vim.api.nvim_get_commands({})
return { local_commands.EitriReviewKeep ~= nil, local_commands.EitriReviewUndo ~= nil,
  global_commands.EitriReviewClear ~= nil }
"#;

fn assert_review_commands(fx: &mut Fx, local: bool, global: bool) {
    assert_eq!(
        items(&fx.nvim.lua(REVIEW_COMMANDS_LUA, vec![])),
        vec![Value::from(local), Value::from(local), Value::from(global)]
    );
}

#[test]
#[ignore = "needs nvim"]
fn keep_takes_the_drawing_off_and_remembers_it() {
    for command in [false, true] {
        let mut fx = Fx::new("keep-all");
        let (path, two) = live_two(&mut fx);
        live_show(&mut fx, &path, "t1", 32, &two.hunks);
        let before = fx.lines();
        assert_eq!(
            fx.nvim
                .lua("return vim.fn.maparg('<localleader>a', 'n', false, true).desc", vec![]),
            Value::from("eitri review: keep all changes in this buffer")
        );
        if command {
            fx.nvim.command("EitriReviewKeep");
        } else {
            fx.feed(",a");
        }
        assert_eq!(fx.marks().count, 0);
        assert!(fx.review_maps().is_empty());
        assert_eq!(fx.lines(), before);
        assert!(!fx.modified());
        assert_eq!(std::fs::read(&path).unwrap(), two.end);
        assert_eq!(fx.event_kinds(), vec!["off"]);
        assert!(fx.said().is_empty());
        assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 0);
        assert_eq!(count(&live_show(&mut fx, &path, "t2", 32, &two.hunks), "drawn"), 2);
    }
}

#[test]
#[ignore = "needs nvim"]
fn close_takes_it_off_and_remembers_nothing() {
    let mut fx = Fx::new("close-live");
    let (path, two) = live_two(&mut fx);
    live_show(&mut fx, &path, "t1", 32, &two.hunks);
    fx.feed(",q");
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
    assert_eq!(fx.lines(), as_lines(&two.end));
    assert!(!fx.modified());
    assert_eq!(fx.event_kinds(), vec!["off"]);
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
}

#[test]
#[ignore = "needs nvim"]
fn undo_all_reverts_every_drawn_hunk_bottom_up_and_one_u_restores_all() {
    for command in [false, true] {
        let mut fx = Fx::new("undo-all");
        let three = three_hunks();
        let path = fx.file("three.txt", &three.end);
        fx.open(&path);
        fx.nvim.command("setlocal autoread");
        let mut reversed = three.hunks.clone();
        reversed.reverse();
        assert_eq!(count(&live_show(&mut fx, &path, "t1", 53, &reversed), "drawn"), 3);
        assert_eq!(
            fx.nvim
                .lua("return vim.fn.maparg('<localleader>d', 'n', false, true).desc", vec![]),
            Value::from("eitri review: undo all changes in this buffer")
        );
        if command {
            fx.nvim.command("EitriReviewUndo");
        } else {
            fx.feed(",d");
        }
        assert_eq!(fx.lines(), as_lines(&three.base));
        assert!(fx.modified());
        assert_eq!(std::fs::read(&path).unwrap(), three.end);
        let events = fx.events();
        assert_eq!(events.len(), 3);
        for (event, hunk) in events.iter().zip(&three.hunks) {
            let parsed = parse_revert_event(event).expect("a buffer revert");
            assert_eq!(&parsed.hunk, hunk);
            // The events run top to bottom, so each one is placed where its lines start once the
            // hunks above it are already reverted: the base's own line.
            assert_eq!(parsed.at_line, hunk.old_start);
            assert_eq!(parsed.meta.scope, Scope::Turn);
        }
        fx.feed("u");
        fx.wait_for_events(3);
        assert_eq!(fx.lines(), as_lines(&three.end), "one undo restores every hunk");
        let events = fx.events();
        let mut restored: Vec<_> = events
            .iter()
            .map(|e| parse_unrevert_event(e).expect("a buffer undo").hunk.id)
            .collect();
        restored.sort_unstable();
        assert_eq!(restored, vec![1, 2, 3]);
    }
}

#[test]
#[ignore = "needs nvim"]
fn undo_all_answers_the_count_in_the_exact_words() {
    for (count_hunks, expected) in [
        (
            3,
            "3 changes undone in this buffer only; :w writes them, u brings them back",
        ),
        (
            1,
            "1 change undone in this buffer only; :w writes them, u brings them back",
        ),
    ] {
        let mut fx = Fx::new("undo-count");
        let three = three_hunks();
        let path = fx.file("count.txt", &three.end);
        fx.open(&path);
        fx.nvim.command("setlocal autoread");
        live_show(&mut fx, &path, "t1", 53, &three.hunks[..count_hunks]);
        fx.feed(",d");
        assert_eq!(fx.said(), vec![expected]);
        assert_eq!(fx.events().len(), count_hunks);
    }
}

#[test]
#[ignore = "needs nvim"]
fn undo_all_refuses_and_changes_nothing() {
    let two = two_hunks();
    let mut ending_hunks = two.hunks.clone();
    ending_hunks[1].old_lines[0].eol = eitri_core::editor_lines::Eol::Missing;
    let wide_base = wide(b'o', 2_200_000);
    let wide_end = wide(b'n', 2_200_000);
    let mut wide_hunks = vec![
        hunk_of(1, &wide_base, &wide_end, (1, 1), (1, 1)),
        hunk_of(2, &wide_base, &wide_end, (12, 7), (12, 7)),
    ];
    wide_hunks[0].old_lines[0].text = b"before".to_vec();
    let mut skipped_hunks = two.hunks.clone();
    skipped_hunks[1].new_lines[0].text = b"not in the buffer".to_vec();
    let cases = [
        (
            b"new\n".to_vec(),
            vec![hunk_of(1, b"", b"new\n", (0, 0), (1, 1))],
            "this change creates or empties the file; undo it in the panel (x)",
        ),
        (
            b"".to_vec(),
            vec![hunk_of(1, b"old\n", b"", (1, 1), (0, 0))],
            "this change creates or empties the file; undo it in the panel (x)",
        ),
        (
            two.end.clone(),
            ending_hunks,
            "this change alters the file's line endings; undo it in the panel (x)",
        ),
        (
            wide_end,
            wide_hunks,
            "this change is too large to undo here; undo it in the panel (x)",
        ),
        (
            two.end.clone(),
            skipped_hunks,
            "1 changes of this turn are not drawn here; undo them in the panel (x)",
        ),
    ];
    for (end, hunks, expected) in cases {
        let mut fx = Fx::new("undo-refuse");
        let path = fx.file("refuse.txt", &end);
        fx.open(&path);
        fx.nvim.command("setlocal autoread");
        let line_count = as_lines(&end).len() as u64;
        assert!(count(&live_show(&mut fx, &path, "t1", line_count, &hunks), "drawn") > 0);
        let before_lines = fx.lines();
        let before = fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]);
        fx.feed(",d");
        assert_eq!(fx.said(), vec![expected]);
        assert_eq!(fx.lines(), before_lines);
        assert!(!fx.modified());
        assert_eq!(std::fs::read(&path).unwrap(), end);
        assert!(fx.events().is_empty());
        assert_eq!(fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]), before);
    }
}

#[test]
#[ignore = "needs nvim"]
fn undo_all_refuses_after_a_hunk_was_edited_away() {
    let mut fx = Fx::new("undo-dropped");
    let (path, two) = live_two(&mut fx);
    let mut hunks = two.hunks.clone();
    hunks.push(hunk_of(3, b"before\n", b"absent\n", (1, 1), (1, 1)));
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &hunks), "skipped"), 1);
    fx.nvim.lua(
        "vim.api.nvim_buf_set_lines(0, 4, 5, true, { 'personal edit' }) \
        vim.api.nvim_exec_autocmds('TextChanged', { buffer = 0 })",
        vec![],
    );
    assert!(fx.events().is_empty());
    let before = fx.lines();
    fx.feed(",d");
    assert_eq!(
        fx.said(),
        vec!["2 changes of this turn are not drawn here; undo them in the panel (x)"]
    );
    assert_eq!(fx.lines(), before);
    assert!(fx.events().is_empty());
}

#[test]
#[ignore = "needs nvim"]
fn a_live_revert_round_trips_through_parse_revert_event() {
    let mut fx = Fx::new("live-revert-event");
    let (path, two) = live_two(&mut fx);
    live_show(&mut fx, &path, "stable-turn", 32, &two.hunks);
    fx.feed(",d");
    let events = fx.events();
    assert_eq!(events.len(), 2);
    let event = parse_revert_event(&events[0]).expect("live events carry turn scope");
    assert_eq!(event.meta, meta(2));
    assert_eq!(event.meta.scope, Scope::Turn);
    assert_eq!(event.hunk, two.hunks[0]);
    assert_eq!(event.path, path.to_str().unwrap());
}

fn draw_running(fx: &mut Fx, path: &Path, hunks: &[ShowHunk]) {
    use eitri_core::review_editor::{live_show_args, LIVE_SHOW_LUA};
    let mut meta = live_meta("t1", 32);
    meta.running = true;
    fx.nvim.lua(LIVE_SHOW_LUA, live_show_args(path, &meta, hunks));
}

/// With a UI attached and nvim's own notify, a refusal wider than the command line would stop nvim at
/// a hit-enter prompt that eats the person's next key. It is echoed cut to fit instead.
#[test]
#[ignore = "needs nvim"]
fn a_message_wider_than_the_command_line_is_cut_to_fit_and_stops_nothing() {
    let mut fx = Fx::new("cut-message");
    fx.nvim.lua("vim.notify = _G.__own_notify", vec![]);
    fx.nvim.request(
        "nvim_ui_attach",
        vec![
            Value::from(60),
            Value::from(20),
            opts_map(&[("ext_linegrid", Value::from(true))]),
        ],
    );
    let (path, two) = live_two(&mut fx);
    draw_running(&mut fx, &path, &two.hunks);
    fx.nvim.command("messages clear");
    fx.nvim.input(",d");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mode = fx.nvim.request("nvim_get_mode", vec![]);
    assert_eq!(get(&mode, "mode"), &Value::from("n"), "{mode:?}");
    assert_eq!(get(&mode, "blocking"), &Value::from(false), "{mode:?}");
    let room = int(&fx.nvim.eval("v:echospace"));
    let said = fx.nvim.lua(
        "return vim.api.nvim_exec2('messages', { output = true }).output",
        vec![],
    );
    let said = said.as_str().unwrap_or_default().to_string();
    assert!(
        said.starts_with("the turn is") && said.contains("...") && said.ends_with("in the panel interrupts it)"),
        "the start and the instruction at the end stay: {said:?}"
    );
    assert!(said.chars().count() as i64 <= room, "{said:?} is wider than {room}");
    assert_eq!(fx.lines(), as_lines(&two.end), "a refusal changes nothing");
}

/// A `vim.notify` that is a callable table (a metatable `__call`) is a replacement: the line goes to it
/// whole, and nothing asks `debug.getinfo` about a table.
#[test]
#[ignore = "needs nvim"]
fn a_callable_table_notify_gets_the_whole_line() {
    let mut fx = Fx::new("callable-notify");
    fx.nvim.lua(
        "vim.notify = setmetatable({}, { __call = function(_, msg) table.insert(_G.__said, msg) end })",
        vec![],
    );
    fx.nvim.request(
        "nvim_ui_attach",
        vec![
            Value::from(60),
            Value::from(20),
            opts_map(&[("ext_linegrid", Value::from(true))]),
        ],
    );
    let (path, two) = live_two(&mut fx);
    draw_running(&mut fx, &path, &two.hunks);
    fx.nvim.input(",d");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mode = fx.nvim.request("nvim_get_mode", vec![]);
    assert_eq!(get(&mode, "mode"), &Value::from("n"), "{mode:?}");
    assert_eq!(
        fx.said(),
        vec!["the turn is still running; undo when it ends (Esc in the panel interrupts it)"]
    );
}

#[test]
#[ignore = "needs nvim"]
fn undo_and_revert_refuse_while_running() {
    let mut fx = Fx::new("undo-running");
    let (path, two) = live_two(&mut fx);
    draw_running(&mut fx, &path, &two.hunks);
    fx.cursor(5);
    let before = fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]);
    fx.feed(",r");
    fx.feed(",d");
    fx.nvim.command("EitriReviewUndo");
    assert_eq!(
        fx.said(),
        vec!["the turn is still running; undo when it ends (Esc in the panel interrupts it)"; 3]
    );
    assert_eq!(fx.lines(), as_lines(&two.end));
    assert!(!fx.modified());
    assert!(fx.events().is_empty());
    assert_eq!(fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]), before);
    fx.feed(",a");
    assert_eq!(fx.marks().count, 0, "keep is available while running");
}

#[test]
#[ignore = "needs nvim"]
fn work_after_a_running_false_draw() {
    for all in [false, true] {
        let mut fx = Fx::new("undo-settled");
        let (path, two) = live_two(&mut fx);
        draw_running(&mut fx, &path, &two.hunks);
        live_show(&mut fx, &path, "t1", 32, &two.hunks);
        fx.cursor(5);
        fx.feed(if all { ",d" } else { ",r" });
        assert!(fx.modified());
        assert_eq!(fx.events().len(), if all { 2 } else { 1 });
        if all {
            assert_eq!(fx.lines(), as_lines(&two.base));
        }
    }
}

#[test]
#[ignore = "needs nvim"]
fn clear_command_takes_every_live_drawing_off_and_no_o_overlay() {
    let mut fx = Fx::new("clear-live-command");
    let (path, two) = live_two(&mut fx);
    live_show(&mut fx, &path, "t1", 32, &two.hunks);
    for (name, explicit) in [("second.txt", false), ("explicit.txt", true)] {
        let other = fx.file(name, &two.end);
        fx.nvim.lua(
            "local p = ... vim.api.nvim_cmd({ cmd = 'split', args = { p } }, {}) vim.bo.autoread = true",
            vec![Value::from(other.to_str().unwrap())],
        );
        if explicit {
            fx.show(&other, &meta(2), &two.hunks);
        } else {
            live_show(&mut fx, &other, "t1", 32, &two.hunks);
        }
    }
    let before = fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]);
    let position = fx.nvim.lua(EDITOR_POSITION_LUA, vec![]);
    fx.nvim.command("EitriReviewClear");
    assert_eq!(fx.nvim.lua(OVERLAY_SNAPSHOT_LUA, vec![]), before);
    assert_eq!(fx.nvim.lua(EDITOR_POSITION_LUA, vec![]), position);
    assert_review_commands(&mut fx, false, false);
    assert_eq!(fx.review_maps(), vec![",q", ",r", "[h", "]h"]);
    let events = fx.events();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| text(get(e, "kind")) == "off"));
    fx.open(&path);
    assert_eq!(fx.marks().count, 0);
    assert!(fx.review_maps().is_empty());
    assert_eq!(count(&live_show(&mut fx, &path, "t1", 32, &two.hunks), "drawn"), 2);
    assert_eq!(fx.lines(), as_lines(&two.end));
    assert!(!fx.modified());
}

#[test]
#[ignore = "needs nvim"]
fn commands_go_with_the_last_live_overlay() {
    for remove in [",q", ",a", "clear", "replace", "teardown", "wipe"] {
        let mut fx = Fx::new("live-command-lifetime");
        assert_review_commands(&mut fx, false, false);
        let (path, two) = live_two(&mut fx);
        live_show(&mut fx, &path, "t1", 32, &two.hunks);
        assert_review_commands(&mut fx, true, true);
        match remove {
            "clear" => {
                fx.nvim.lua(CLEAR_LUA, clear_args(&path));
            }
            "replace" => {
                fx.show(&path, &meta(2), &two.hunks);
            }
            "teardown" => {
                fx.nvim.lua("_G.__eitri_review.teardown()", vec![]);
            }
            "wipe" => fx.nvim.command("bwipeout!"),
            keys => fx.feed(keys),
        }
        assert_review_commands(&mut fx, false, false);
    }
    let mut fx = Fx::new("live-command-last-buffer");
    let (path, two) = live_two(&mut fx);
    live_show(&mut fx, &path, "t1", 32, &two.hunks);
    let other = fx.file("other-live.txt", &two.end);
    fx.nvim.lua(
        "local p = ... vim.api.nvim_cmd({ cmd = 'split', args = { p } }, {}) vim.bo.autoread = true",
        vec![Value::from(other.to_str().unwrap())],
    );
    live_show(&mut fx, &other, "t1", 32, &two.hunks);
    fx.nvim.command("EitriReviewKeep");
    assert_review_commands(&mut fx, false, true);
    assert_eq!(fx.marks().count, 0);
    fx.open(&path);
    assert_review_commands(&mut fx, true, true);
    assert!(fx.marks().count > 0, "keeping another buffer leaves this drawing up");
    fx.nvim.command("EitriReviewUndo");
    assert_eq!(fx.lines(), as_lines(&two.base));
    assert_review_commands(&mut fx, true, true);
    fx.nvim.command("EitriReviewClear");
    assert_review_commands(&mut fx, false, false);
    fx.open(&other);
    assert_eq!(fx.lines(), as_lines(&two.end), "undo affects only its buffer");

    let mut fx = Fx::new("live-command-ownership");
    let (path, two) = live_two(&mut fx);
    live_show(&mut fx, &path, "t1", 32, &two.hunks);
    fx.nvim.lua(
        "vim.api.nvim_create_user_command('EitriReviewClear', function() end, { desc = 'personal clear' }) \
        vim.api.nvim_buf_create_user_command(0, 'EitriReviewKeep', function() end, { desc = 'personal keep' })",
        vec![],
    );
    fx.feed(",q");
    assert_eq!(
        fx.nvim.lua(
            "return { vim.api.nvim_get_commands({}).EitriReviewClear.definition, \
        vim.api.nvim_buf_get_commands(0, {}).EitriReviewKeep.definition }",
            vec![]
        ),
        Value::Array(vec![Value::from("personal clear"), Value::from("personal keep")])
    );
    assert_eq!(
        fx.nvim.lua(
            "return vim.api.nvim_buf_get_commands(0, {}).EitriReviewUndo == nil",
            vec![]
        ),
        Value::from(true)
    );
}
