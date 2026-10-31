//! `:EitriMention` in a private headless embedded nvim, its lines read from the real socket.
#![cfg(target_os = "linux")]

#[path = "support/embed_client.rs"]
mod embed_client;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use eitri_core::ide_server::mention::{LiveMentions, IDE_MENTION_DEFINE, IDE_MENTION_OUTSIDE, IDE_MENTION_REMOVE};
use eitri_core::ide_server::{MentionLine, MentionPort};
use embed_client::Embed;
use rmpv::Value;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "ide-mention-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A headless nvim with `:EitriMention` defined on a real socket, showing `file`'s ten lines.
struct Session {
    nvim: Embed,
    mentions: LiveMentions,
    file: PathBuf,
    _scratch: Scratch,
}

impl Session {
    fn new(name: &str) -> Session {
        let scratch = Scratch::new();
        let file = scratch.0.join(name);
        let text: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&file, text).unwrap();
        let mut nvim = Embed::start(&scratch.0, &[], &[]);
        let mut mentions = LiveMentions::new();
        let socket = mentions.open().expect("the socket binds");
        nvim.lua(IDE_MENTION_DEFINE, vec![Value::from(socket.to_str().unwrap())]);
        nvim.lua(
            "vim.cmd('edit ' .. vim.fn.fnameescape((...)))",
            vec![Value::from(file.to_str().unwrap())],
        );
        Session {
            nvim,
            mentions,
            file,
            _scratch: scratch,
        }
    }

    fn name(&self) -> &str {
        self.file.to_str().unwrap()
    }

    /// Polls the socket until `want` lines came, or for 5 s.
    fn lines(&mut self, want: usize) -> Vec<MentionLine> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut lines = Vec::new();
        while lines.len() < want && Instant::now() < deadline {
            lines.extend(self.mentions.poll(Instant::now()));
            std::thread::sleep(Duration::from_millis(5));
        }
        lines
    }
}

fn mention(file: &str, line_start: u32, line_end: u32) -> MentionLine {
    MentionLine {
        file: file.to_string(),
        line_start,
        line_end,
    }
}

fn is_defined(nvim: &mut Embed) -> bool {
    nvim.eval("exists(':EitriMention')").as_i64() == Some(2)
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_range_sends_its_lines_0_based() {
    let mut session = Session::new("a.txt");
    session.nvim.command("2,4EitriMention");
    let want = mention(session.name(), 1, 3);
    assert_eq!(session.lines(1), vec![want]);
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_visual_selection_sends_its_lines() {
    let mut session = Session::new("a.txt");
    session.nvim.command("3");
    session.nvim.input("Vjj:EitriMention<CR>");
    let want = mention(session.name(), 2, 4);
    assert_eq!(session.lines(1), vec![want]);
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn no_range_sends_the_cursor_line() {
    let mut session = Session::new("a.txt");
    session.nvim.command("7");
    session.nvim.command("EitriMention");
    let want = mention(session.name(), 6, 6);
    assert_eq!(session.lines(1), vec![want]);
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn two_in_a_row_arrive_in_order() {
    let mut session = Session::new("a.txt");
    // One chunk, so nvim's loop runs neither connect's callback between the two commands.
    session
        .nvim
        .lua("vim.cmd('1EitriMention') vim.cmd('9,10EitriMention')", vec![]);
    let name = session.name().to_string();
    assert_eq!(session.lines(2), vec![mention(&name, 0, 0), mention(&name, 8, 9)]);
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_name_with_quotes_and_accents_arrives_whole() {
    let mut session = Session::new("it's \"é\" \\ here.txt");
    session.nvim.command("EitriMention");
    let want = mention(session.name(), 0, 0);
    assert_eq!(session.lines(1), vec![want]);
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_buffer_that_is_no_file_sends_no_name() {
    let mut session = Session::new("a.txt");
    session.nvim.lua(
        "vim.cmd('enew') vim.bo.buftype = 'nofile' vim.api.nvim_buf_set_name(0, 'term://~/x//1:sh')",
        vec![],
    );
    session.nvim.command("EitriMention");
    assert_eq!(session.lines(1), vec![mention("", 0, 0)]);
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_name_that_is_not_utf8_comes_through_as_no_name() {
    let mut session = Session::new("a.txt");
    let directory = session.file.parent().unwrap().to_str().unwrap().to_owned();
    session.nvim.lua(
        "vim.api.nvim_buf_set_name(0, (...) .. '/caf\\233.txt')",
        vec![Value::from(directory)],
    );
    session.nvim.command("EitriMention");
    assert_eq!(session.lines(1), vec![mention("", 0, 0)]);
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn the_command_goes_with_remove() {
    let mut session = Session::new("a.txt");
    assert!(is_defined(&mut session.nvim));
    session.nvim.lua(IDE_MENTION_REMOVE, vec![]);
    assert!(!is_defined(&mut session.nvim));
    session.nvim.lua(IDE_MENTION_REMOVE, vec![]);
    assert!(!is_defined(&mut session.nvim), "a second remove is harmless");
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_command_outliving_its_socket_stays_quiet() {
    let mut session = Session::new("a.txt");
    session.mentions.close();
    session.nvim.command("EitriMention");
    session.nvim.flush_scheduled();
    let errors = session.nvim.eval("v:errmsg");
    assert_eq!(errors.as_str(), Some(""), "the connect's failure stays quiet");
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn outside_says_so_in_the_message_area() {
    let mut session = Session::new("a.txt");
    session.nvim.lua(IDE_MENTION_OUTSIDE, vec![]);
    let messages = session.nvim.eval("execute('messages')");
    assert!(
        messages
            .as_str()
            .unwrap()
            .contains("Eitri: that file is outside the project, or its name cannot be sent; nothing was sent"),
        "{messages:?}"
    );
    assert!(session.lines(0).is_empty());
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn no_key_is_bound() {
    let mut session = Session::new("a.txt");
    let bound = session.nvim.lua(
        r#"
        local found = {}
        for _, mode in ipairs({ 'n', 'x', 'i', 'v', 'o', 't' }) do
          for _, map in ipairs(vim.api.nvim_get_keymap(mode)) do
            if (map.rhs or ''):find('EitriMention', 1, true) then found[#found + 1] = map.lhs end
          end
        end
        return #found
        "#,
        vec![],
    );
    assert_eq!(bound.as_i64(), Some(0));
}
