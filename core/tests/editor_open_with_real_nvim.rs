//! `editor_open::open` over an `NvimLink` to a real `nvim --embed --listen`: what opens, in which window, where the
//! cursor lands, and a name no layer reads as anything but a name.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eitri_core::editor_open;
use eitri_core::nvim_rpc::{NvimLink, RpcError};
use embed_client::Embed;
use rmpv::Value;

const WAIT: Duration = Duration::from_secs(5);

struct Case {
    dir: PathBuf,
    nvim: Embed,
    link: NvimLink,
}

impl Case {
    /// Under `std::env::temp_dir()` and short: the socket inside it is capped at 103 bytes. Canonical, as the
    /// paths the window is given are.
    fn start(case: &str) -> Case {
        let dir = std::env::temp_dir().join(format!("eopen-{}-{case}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let sock = agent::socket_path::in_dir(&dir, "n.sock").unwrap();
        let nvim = Embed::start(&dir, &["--listen", sock.to_str().unwrap()], &[]);
        let deadline = Instant::now() + WAIT;
        loop {
            match NvimLink::connect(&sock, Duration::from_secs(1)) {
                Ok((link, _events)) => return Case { dir, nvim, link },
                Err(e) => {
                    assert!(Instant::now() < deadline, "could not connect: {e}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn open(&self, path: &Path, line: Option<u32>, col: Option<u32>) -> Result<Value, RpcError> {
        editor_open::open(&self.link, path, line, col)
            .wait(WAIT)
            .expect("an answer")
    }

    fn how(&self, path: &Path) -> String {
        field(&self.open(path, None, None).expect("it opens"), "how")
            .as_str()
            .unwrap()
            .to_string()
    }

    fn current_name(&mut self) -> Vec<u8> {
        text(&self.nvim.lua("return vim.api.nvim_buf_get_name(0)", vec![]))
    }

    fn windows(&mut self) -> u64 {
        self.nvim
            .lua("return #vim.api.nvim_list_wins()", vec![])
            .as_u64()
            .unwrap()
    }

    fn buffers(&mut self) -> u64 {
        self.nvim
            .lua("return #vim.api.nvim_list_bufs()", vec![])
            .as_u64()
            .unwrap()
    }

    fn cursor(&mut self) -> (u64, u64) {
        let at = self.nvim.lua("return vim.api.nvim_win_get_cursor(0)", vec![]);
        let at = at.as_array().unwrap();
        (at[0].as_u64().unwrap(), at[1].as_u64().unwrap())
    }

    fn lines(&mut self) -> String {
        let text = self.nvim.lua(
            "return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), '|')",
            vec![],
        );
        String::from_utf8(self::text(&text)).unwrap()
    }

    fn end(self) {
        let dir = self.dir.clone();
        drop(self);
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn field<'a>(map: &'a Value, name: &str) -> &'a Value {
    map.as_map()
        .unwrap_or_else(|| panic!("not a map: {map:?}"))
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
        .unwrap_or_else(|| panic!("no {name} in {map:?}"))
}

/// The bytes of a Lua string nvim sent back: msgpack str for text, bin for anything else.
fn text(value: &Value) -> Vec<u8> {
    match value {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Binary(b) => b.clone(),
        other => panic!("not a string: {other:?}"),
    }
}

fn name_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn opens_over_an_unmodified_buffer_with_edit() {
    let mut c = Case::start("edit");
    let path = c.file("a.txt", "one\ntwo\n");
    assert_eq!(c.how(&path), "edit");
    assert_eq!(c.windows(), 1);
    assert_eq!(c.buffers(), 1, "the empty first buffer is taken over");
    assert_eq!(c.current_name(), name_bytes(&path));
    assert_eq!(c.lines(), "one|two");
    assert_eq!(c.cursor(), (1, 0), "no line given, the cursor stays where nvim put it");
    c.end();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn opens_over_a_modified_buffer_with_a_split_and_keeps_it() {
    let mut c = Case::start("modified");
    let path = c.file("a.txt", "file\n");
    c.nvim
        .lua("vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'unsaved' })", vec![]);
    let modified = c.nvim.lua("return vim.api.nvim_get_current_buf()", vec![]);
    assert_eq!(c.how(&path), "split");
    assert_eq!(c.windows(), 2);
    assert_eq!(c.current_name(), name_bytes(&path));
    let kept = c.nvim.lua(
        "local b = ... return { vim.bo[b].modified, vim.api.nvim_buf_get_lines(b, 0, -1, false)[1] }",
        vec![modified],
    );
    let kept = kept.as_array().unwrap();
    assert_eq!(kept[0], Value::from(true), "the unsaved buffer stays modified");
    assert_eq!(text(&kept[1]), b"unsaved");
    c.end();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn opens_from_a_terminal_buffer_window_with_a_split() {
    let mut c = Case::start("terminal");
    let path = c.file("a.txt", "file\n");
    // A terminal buffer with no process behind it: nothing runs on the test's machine.
    c.nvim.lua("vim.api.nvim_open_term(0, {})", vec![]);
    let terminal = c.nvim.lua("return vim.api.nvim_get_current_buf()", vec![]);
    assert_eq!(c.how(&path), "split");
    assert_eq!(c.windows(), 2);
    assert_eq!(c.current_name(), name_bytes(&path));
    let shown = c.nvim.lua(
        "local b = ... return { #vim.fn.win_findbuf(b), vim.bo[b].buftype }",
        vec![terminal],
    );
    let shown = shown.as_array().unwrap();
    assert_eq!(shown[0], Value::from(1), "the terminal keeps its window");
    assert_eq!(text(&shown[1]), b"terminal");
    c.end();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_current_file_stays_and_only_the_cursor_moves() {
    let mut c = Case::start("current");
    let path = c.file("a.txt", "one\ntwo\nthree\n");
    assert_eq!(c.how(&path), "edit");
    // An unsaved change an :edit would stop at, and a re-read would lose.
    c.nvim
        .lua("vim.api.nvim_buf_set_lines(0, 0, 1, false, { 'changed' })", vec![]);
    let answer = c.open(&path, Some(3), None).expect("it opens");
    assert_eq!(field(&answer, "how").as_str(), Some("current"));
    assert_eq!(c.windows(), 1);
    assert_eq!(c.buffers(), 1);
    assert_eq!(c.cursor(), (3, 0));
    assert_eq!(c.lines(), "changed|two|three");
    c.nvim.lua("vim.api.nvim_win_set_cursor(0, { 2, 1 })", vec![]);
    assert_eq!(c.how(&path), "current");
    assert_eq!(c.cursor(), (2, 1), "with no line the cursor stays");
    c.end();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_new_file_opens_as_an_empty_named_buffer() {
    let mut c = Case::start("new");
    let path = c.dir.join("new.txt");
    assert_eq!(c.how(&path), "edit");
    assert_eq!(c.current_name(), name_bytes(&path));
    assert_eq!(c.lines(), "");
    assert!(!path.exists(), "only a write creates it");
    assert_eq!(c.how(&path), "current", "a new file already open is the current file");
    c.end();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_path_that_no_longer_resolves_to_itself_errors_and_leaves_no_buffer() {
    let mut c = Case::start("resolve");
    let target = c.file("target.txt", "target\n");
    let link = c.dir.join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let dangling = c.dir.join("dangling.txt");
    std::os::unix::fs::symlink(c.dir.join("nothing"), &dangling).unwrap();
    let elsewhere = c.dir.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let parent = c.dir.join("parent");
    std::os::unix::fs::symlink(&elsewhere, &parent).unwrap();
    let gone_parent = c.dir.join("no/such/dir/a.txt");
    for path in [
        &link,
        &dangling,
        &parent.join("new.txt"),
        &gone_parent,
        &c.dir.join("."),
    ] {
        let refused = c.open(path, None, None);
        assert!(refused.is_err(), "{path:?} opened: {refused:?}");
        assert_eq!(c.buffers(), 1, "{path:?} left a buffer");
        assert_eq!(c.windows(), 1, "{path:?} left a window");
        assert_eq!(c.current_name(), b"", "{path:?} named the buffer");
    }
    c.end();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_relative_path_errors() {
    let c = Case::start("relative");
    assert!(c.open(Path::new("a.txt"), None, None).is_err());
    c.end();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn something_that_is_not_a_file_errors_before_nvim_reads_it() {
    let mut c = Case::start("kind");
    assert!(c.open(&c.dir, None, None).is_err());
    assert_eq!(c.buffers(), 1);
    assert_eq!(c.current_name(), b"");
    c.end();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_line_past_the_end_clamps() {
    let mut c = Case::start("clamp");
    let path = c.file("a.txt", "one\ntwo\nthree\n");
    c.open(&path, Some(99), None).expect("it opens");
    assert_eq!(c.cursor(), (3, 0));
    c.open(&path, Some(0), None).expect("it opens");
    assert_eq!(c.cursor(), (1, 0), "line zero is the first line");
    c.end();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_largest_line_is_the_last_line() {
    let mut c = Case::start("last");
    let path = c.file("a.txt", "one\ntwo\nthree\n");
    let answer = c.open(&path, Some(u32::MAX), None).expect("it opens");
    assert_eq!(field(&answer, "line").as_u64(), Some(3));
    assert_eq!(c.cursor(), (3, 0));
    c.end();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_column_counts_bytes_on_multibyte_text() {
    let mut c = Case::start("bytes");
    let path = c.file("a.txt", "\u{65e5}\u{672c}\u{8a9e}x\n");
    c.open(&path, Some(1), Some(10)).expect("it opens");
    assert_eq!(c.cursor(), (1, 9), "byte ten, from one, is the x");
    c.open(&path, Some(1), Some(5)).expect("it opens");
    assert_eq!(c.cursor(), (1, 3), "a byte inside a character lands on that character");
    c.open(&path, Some(1), Some(u32::MAX)).expect("it opens");
    assert_eq!(c.cursor(), (1, 9), "a column past the end is the last character");
    c.end();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn names_no_layer_reads_as_anything_but_a_name() {
    let mut c = Case::start("names");
    for name in [
        "a b.txt",
        "100%.txt",
        "#1.txt",
        "new\nline.txt",
        "bar|bar.txt",
        "back\\slash.txt",
        "quo\"te.txt",
        "+plus.txt",
        "-dash.txt",
        "*star?.txt",
        "$HOME.txt",
        "~tilde.txt",
    ] {
        let path = c.file(name, "the file\n");
        c.nvim.lua("vim.cmd.enew({ bang = true })", vec![]);
        assert_eq!(c.how(&path), "edit", "{name:?}");
        assert_eq!(c.current_name(), name_bytes(&path), "{name:?}");
        assert_eq!(c.lines(), "the file", "{name:?}");
    }
    let unwritten = c.dir.join("not yet % # written.txt");
    c.nvim.lua("vim.cmd.enew({ bang = true })", vec![]);
    assert_eq!(c.how(&unwritten), "edit");
    assert_eq!(c.current_name(), name_bytes(&unwritten));
    c.end();
}
