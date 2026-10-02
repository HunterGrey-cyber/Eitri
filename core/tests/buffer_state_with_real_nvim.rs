//! `BUFFER_STATE_LUA` in a real `nvim --clean --embed`: which buffers show a file (through a hard
//! link or a symlink too), whether one has unsaved changes, and a range of its lines as the bytes
//! the buffer would write.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.
//! Files are opened with `nvim_cmd` and the path as a structured argument, never in Ex or Lua source.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use eitri_core::editor_lines::FileFormat;
use eitri_core::turn_review::{buffer_state_args, parse_buffer_state, BufferState, BUFFER_STATE_LUA};
use embed_client::{opts_map, Embed};
use rmpv::Value;

/// A directory of the test's own under the target dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("buffer_state")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("files")).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }

    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join("files").join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn nvim(&self) -> Embed {
        Embed::start(&self.0.join("nvim"), &[], &[])
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path_value(path: &Path) -> Value {
    Value::from(path.to_str().unwrap())
}

/// `:edit <path>` with the path as an argument and no file-name expansion.
fn edit(nvim: &mut Embed, path: &Path) {
    let cmd = opts_map(&[
        ("cmd", Value::from("edit")),
        ("args", Value::Array(vec![path_value(path)])),
        ("magic", opts_map(&[("file", Value::from(false))])),
    ]);
    nvim.request("nvim_cmd", vec![cmd, Value::Map(Vec::new())]);
}

/// Replaces the current buffer's first line, leaving it modified.
fn modify(nvim: &mut Embed) {
    nvim.request(
        "nvim_buf_set_lines",
        vec![
            Value::from(0),
            Value::from(0),
            Value::from(1),
            Value::from(false),
            Value::Array(vec![Value::from("edited in the buffer")]),
        ],
    );
}

fn state(nvim: &mut Embed, path: &Path, range: Option<(u32, u32)>) -> BufferState {
    let answer = nvim.lua(BUFFER_STATE_LUA, buffer_state_args(path, range));
    parse_buffer_state(&answer).unwrap_or_else(|e| panic!("{e}: {answer:?}"))
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_modified_buffer_of_the_file_is_found() {
    let scratch = Scratch::new("modified");
    let real = scratch.file("real.txt", b"one\ntwo\n");
    let hard = scratch.0.join("files/hard.txt");
    std::fs::hard_link(&real, &hard).unwrap();
    let soft = scratch.0.join("files/soft.txt");
    std::os::unix::fs::symlink(&real, &soft).unwrap();
    let mut nvim = scratch.nvim();

    edit(&mut nvim, &real);
    modify(&mut nvim);
    for asked in [&real, &hard, &soft] {
        let s = state(&mut nvim, asked, None);
        assert!(s.found && s.modified, "{}: {s:?}", asked.display());
    }

    // The buffer opened through the symlink, the question asked about the real name.
    let other = scratch.file("other.txt", b"x\n");
    let link = scratch.0.join("files/other-link.txt");
    std::os::unix::fs::symlink(&other, &link).unwrap();
    edit(&mut nvim, &link);
    modify(&mut nvim);
    let s = state(&mut nvim, &other, None);
    assert!(s.found && s.modified, "{s:?}");
    nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_unmodified_or_unloaded_buffer_is_clear() {
    let scratch = Scratch::new("clear");
    let shown = scratch.file("shown.txt", b"a\n");
    let never = scratch.file("never.txt", b"b\n");
    let mut nvim = scratch.nvim();

    edit(&mut nvim, &shown);
    let s = state(&mut nvim, &shown, None);
    assert!(s.found && !s.modified, "{s:?}");
    let s = state(&mut nvim, &never, None);
    assert!(!s.found && !s.modified, "{s:?}");
    let gone = scratch.0.join("files/not-on-disk.txt");
    assert!(!state(&mut nvim, &gone, None).found);

    // Unloaded: its text is not in memory, so nothing unsaved can be.
    let buf = nvim.request("nvim_get_current_buf", vec![]);
    edit(&mut nvim, &never);
    nvim.request("nvim_buf_delete", vec![buf, opts_map(&[("unload", Value::from(true))])]);
    let s = state(&mut nvim, &shown, None);
    assert!(!s.found && !s.modified, "{s:?}");
    nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_path_with_quotes_and_percent_is_an_argument() {
    let scratch = Scratch::new("quotes");
    let odd = scratch.file("it's 100%.txt", b"text\n");
    let mut nvim = scratch.nvim();
    edit(&mut nvim, &odd);
    modify(&mut nvim);
    let s = state(&mut nvim, &odd, None);
    assert!(s.found && s.modified, "{s:?}");
    let names = nvim.lua(
        "local out = {} for _, b in ipairs(vim.api.nvim_list_bufs()) do out[#out + 1] = vim.api.nvim_buf_get_name(b) end return out",
        vec![],
    );
    assert_eq!(
        names,
        Value::Array(vec![path_value(&odd)]),
        "one buffer, with the exact name"
    );
    assert_eq!(nvim.eval("1 + 1"), Value::from(2), "nvim is fine");
    nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_range_comes_back_as_lines() {
    let scratch = Scratch::new("range");
    let file = scratch.file("r.txt", b"a\nb\nc\nd\n");
    let mut nvim = scratch.nvim();
    edit(&mut nvim, &file);
    let s = state(&mut nvim, &file, Some((2, 2)));
    assert_eq!(s.lines, Some(vec![b"b".to_vec(), b"c".to_vec()]));
    assert_eq!(s.line_count, 4);
    assert_eq!(s.fileformat, FileFormat::Unix);
    assert!(s.eol);
    assert_eq!(state(&mut nvim, &file, Some((4, 2))).lines, None, "past the end");
    assert_eq!(state(&mut nvim, &file, Some((3, 0))).lines, Some(Vec::new()));
    nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn range_bytes_match_the_file_for_lf_crlf_and_a_missing_final_newline() {
    let scratch = Scratch::new("bytes");
    let cases: [(&str, &[u8], FileFormat, bool); 3] = [
        ("lf.txt", b"one\ntwo\nthree\n", FileFormat::Unix, true),
        ("crlf.txt", b"one\r\ntwo\r\nthree\r\n", FileFormat::Dos, true),
        ("noeol.txt", b"one\ntwo\nthree", FileFormat::Unix, false),
    ];
    let mut nvim = scratch.nvim();
    for (name, bytes, format, eol) in cases {
        let file = scratch.file(name, bytes);
        edit(&mut nvim, &file);
        let s = state(&mut nvim, &file, Some((2, 2)));
        assert!(s.found && !s.modified, "{name}: {s:?}");
        assert_eq!((s.fileformat, s.eol), (format, eol), "{name}");
        // Lines 2-3 end the buffer, so they are the file's bytes from line 2 on.
        let from_line_2: Vec<u8> = bytes
            .split_inclusive(|b| *b == b'\n')
            .skip(1)
            .flatten()
            .copied()
            .collect();
        assert_eq!(s.range_bytes(2), Some(from_line_2), "{name}");
        let first = state(&mut nvim, &file, Some((1, 1)));
        let line_1: Vec<u8> = bytes.split_inclusive(|b| *b == b'\n').next().unwrap().to_vec();
        assert_eq!(first.range_bytes(1), Some(line_1), "{name}");
    }
    nvim.quit();
}
