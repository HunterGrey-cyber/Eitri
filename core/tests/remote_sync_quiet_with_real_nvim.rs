//! `UNSAVED_UNDER_LUA` in a real `nvim --clean --embed`: a buffer under the root with unsaved changes keeps the
//! project from being quiet even when no window shows it, a buffer outside the root does not, and a save moves the
//! save generation.
//!
//! `#[ignore]`d: it needs `nvim` on `PATH`; it spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eitri_core::remote_sync::quiet::{self, ActivitySource, EditorAnswer, EditorState, NotQuiet, UNSAVED_UNDER_LUA};
use embed_client::{opts_map, Embed};
use rmpv::Value;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("remote_sync_quiet")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project/src")).unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn edit(nvim: &mut Embed, path: &Path) {
    let cmd = opts_map(&[
        ("cmd", Value::from("edit")),
        ("args", Value::Array(vec![Value::from(path.to_str().unwrap())])),
        ("magic", opts_map(&[("file", Value::from(false))])),
    ]);
    nvim.request("nvim_cmd", vec![cmd, Value::Map(Vec::new())]);
}

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

/// One answer from the editor, as the window's editor feeds it to the quiet check.
struct Answered(EditorAnswer);

impl EditorState for Answered {
    fn unsaved_under(&self, root: &Path) -> Option<Vec<PathBuf>> {
        Some(self.0.unsaved.iter().filter(|p| p.starts_with(root)).cloned().collect())
    }
    fn saves_since(&self, generation: u64) -> (u64, Vec<PathBuf>) {
        (self.0.generation().max(generation), self.0.saved_after(generation))
    }
}

struct Idle;
impl ActivitySource for Idle {
    fn busy(&self) -> Result<bool, String> {
        Ok(false)
    }
}

fn ask(nvim: &mut Embed, root: &Path) -> EditorAnswer {
    let value = nvim.lua(UNSAVED_UNDER_LUA, vec![Value::from(root.to_str().unwrap())]);
    EditorAnswer::from_value(&value).unwrap_or_else(|| panic!("not an answer: {value:?}"))
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_hidden_modified_buffer_blocks_quiet() {
    let scratch = Scratch::new("hidden");
    let root = scratch.0.join("project");
    let inside = root.join("src/lib.rs");
    let other = root.join("README.md");
    let outside = scratch.0.join("elsewhere/notes.txt");
    for path in [&inside, &other, &outside] {
        std::fs::write(path, "one\n").unwrap();
    }
    let mut nvim = Embed::start(&scratch.0.join("nvim"), &[], &[]);
    nvim.command("set hidden");
    let long_ago = Instant::now() - Duration::from_secs(60);

    edit(&mut nvim, &outside);
    modify(&mut nvim);
    let answer = ask(&mut nvim, &root);
    assert!(
        answer.unsaved.is_empty(),
        "a buffer outside the root does not count: {answer:?}"
    );
    assert_eq!(
        quiet::check(&Idle, &Answered(answer), &root, long_ago, Duration::from_secs(2)),
        Ok(())
    );

    // Modified, then hidden behind another buffer: no window shows it, it still holds unsaved changes.
    edit(&mut nvim, &inside);
    modify(&mut nvim);
    edit(&mut nvim, &other);
    let answer = ask(&mut nvim, &root);
    assert_eq!(answer.unsaved, vec![inside.clone()]);
    assert_eq!(
        quiet::check(
            &Idle,
            &Answered(answer.clone()),
            &root,
            long_ago,
            Duration::from_secs(2)
        ),
        Err(NotQuiet::Unsaved(vec![inside.clone()]))
    );

    // Saving it clears it and moves the save generation past the earlier one.
    let before = answer.generation();
    std::thread::sleep(Duration::from_millis(20));
    edit(&mut nvim, &inside);
    nvim.command("write");
    let answer = ask(&mut nvim, &root);
    assert!(answer.unsaved.is_empty(), "{answer:?}");
    let editor = Answered(answer);
    let (generation, saved) = editor.saves_since(before);
    assert!(generation > before, "{generation} <= {before}");
    assert_eq!(saved, vec![inside]);
    nvim.quit();
}
