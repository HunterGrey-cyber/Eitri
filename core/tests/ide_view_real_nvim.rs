#![cfg(target_os = "linux")]
//! The proposal view in a real `nvim`, through the real chunks and the arguments `view.rs` builds.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.
//!
//! Run: `cargo test -p eitri-core --test ide_view_real_nvim -- --ignored --test-threads=1`

#[path = "support/embed_client.rs"]
mod embed_client;

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::editor_tools::stamp::{wall_ms, Stamp};
use eitri_core::ide_server::lines::{join_values, line_values, split_lines};
use eitri_core::ide_server::view::{
    parse_events, parse_open, send_close, send_open, send_take, Opened, ViewEvent, IDE_DIFF_CLOSE, IDE_TAKE_EVENTS,
};
use eitri_core::ide_server::DiffRequest;
use eitri_core::nvim_rpc::Pending;
use embed_client::Embed;
use rmpv::Value;

const FAR: u64 = 60_000;
const SAME: &str = "same as the file: accepting now rejects";

/// Keeps the one call it is sent, so a test runs what `view.rs` would really send.
#[derive(Default)]
struct Capture(RefCell<Option<(&'static str, Vec<Value>)>>);

impl EditorRpc for Capture {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        assert!(
            self.0.borrow_mut().replace((code, args)).is_none(),
            "one call at a time"
        );
        Pending::pair().1
    }

    fn target(&self) -> Option<u64> {
        Some(1)
    }
}

struct Nvim {
    embed: Embed,
    dir: PathBuf,
    seq: u64,
}

impl Drop for Nvim {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(case: &str) -> Nvim {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("ide-view-{}-{case}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let embed = Embed::start(&dir, &[], &[]);
    Nvim { embed, dir, seq: 0 }
}

fn diff(nvim: &Nvim, rel: &str, file: &[u8], proposal: &[u8]) -> DiffRequest {
    let (file_lines, _) = split_lines(file);
    let (proposal_lines, shape) = split_lines(proposal);
    DiffRequest {
        tab_name: "a tab".to_owned(),
        real_path: nvim.dir.join(rel),
        rel: rel.to_owned(),
        file_text: file.to_vec(),
        file_lines: line_values(file_lines),
        proposal_lines: line_values(proposal_lines),
        shape,
    }
}

impl Nvim {
    fn stamp(&mut self, deadline_ms: u64) -> Stamp {
        self.seq += 1;
        Stamp {
            deadline_ms,
            scope: "ide-1".to_owned(),
            seq: self.seq,
        }
    }

    fn run(&mut self, send: impl FnOnce(&Capture) -> Pending) -> Value {
        let editor = Capture::default();
        let _pending = send(&editor);
        let (code, args) = editor.0.take().expect("the call");
        self.embed.lua(code, args)
    }

    fn open_stamped(&mut self, stamp: Stamp, view: u64, request: DiffRequest) -> Opened {
        let answer = self.run(|editor| send_open(editor, &stamp, view, request));
        parse_open(&answer).expect("an answer the chunk gives")
    }

    fn open(&mut self, view: u64, rel: &str, file: &[u8], proposal: &[u8]) -> Opened {
        let stamp = self.stamp(wall_ms() + FAR);
        let request = diff(self, rel, file, proposal);
        self.open_stamped(stamp, view, request)
    }

    fn opened(&mut self, view: u64, rel: &str, file: &[u8], proposal: &[u8]) -> Pair {
        assert_eq!(self.open(view, rel, file, proposal), Opened::Opened);
        self.pair(view)
    }

    fn close(&mut self, view: u64) {
        self.run(|editor| send_close(editor, view));
    }

    fn take(&mut self) -> Vec<ViewEvent> {
        parse_events(self.run(|editor| send_take(editor))).expect("events")
    }

    /// Everything typed so far has run, and what that scheduled.
    fn settle(&mut self) {
        self.embed.command("let g:settled = 0");
        self.embed.input("<Cmd>let g:settled = 1<CR>");
        self.embed
            .wait_until("the typed keys", |embed| embed.eval("g:settled").as_i64() == Some(1));
        self.embed.flush_scheduled();
    }

    /// Waits for the first event, then gives it with whatever else the same keys queued.
    fn answered(&mut self) -> Vec<ViewEvent> {
        let mut events = Vec::new();
        self.embed.wait_until("an event", |embed| {
            events.extend(parse_events(embed.lua(IDE_TAKE_EVENTS, vec![])).expect("events"));
            !events.is_empty()
        });
        self.settle();
        events.extend(self.take());
        events
    }

    fn enter(&mut self, win: i64) {
        self.embed
            .lua("vim.api.nvim_set_current_win((...))", vec![Value::from(win)]);
    }

    fn modified(&mut self, buf: i64) -> bool {
        self.buffer_option("modified", buf) == Value::from(true)
    }

    /// Types `keys` and waits for the change to land with 'modified' off again.
    fn edit(&mut self, buf: i64, keys: &str) {
        let read = |embed: &mut Embed| {
            let answer = embed.lua(
                "local buf = ... return { vim.api.nvim_buf_get_changedtick(buf), vim.bo[buf].modified }",
                vec![Value::from(buf)],
            );
            let answer = answer.as_array().expect("two values");
            (
                answer[0].as_i64().expect("a tick"),
                answer[1].as_bool().expect("a flag"),
            )
        };
        let (before, _) = read(&mut self.embed);
        self.embed.input(keys);
        self.embed.wait_until(&format!("`{keys}` to land nomodified"), |embed| {
            let (tick, modified) = read(embed);
            tick != before && !modified
        });
    }

    fn assert_nothing_answered_and_open(&mut self, pair: Pair, what: &str) {
        self.settle();
        assert_eq!(self.take(), [], "{what}: no event");
        assert!(self.window_valid(pair.file.0), "{what}: the view stays");
        assert!(self.window_valid(pair.proposal.0), "{what}: the view stays");
    }

    fn assert_gone(&mut self, pair: Pair) {
        assert!(!self.window_valid(pair.file.0), "the file's window went");
        assert!(!self.window_valid(pair.proposal.0), "the proposal's window went");
        assert!(!self.buffer_valid(pair.file.1), "the file's buffer went");
        assert!(!self.buffer_valid(pair.proposal.1), "the proposal's buffer went");
    }

    /// The accept's lines joined back into bytes as the host joins them, with the proposal's own shape.
    fn accepted_text(&mut self, view: u64, sent: &[u8]) -> Vec<u8> {
        let events = self.answered();
        let [ViewEvent::Accept { view: answered, lines }] = events.as_slice() else {
            panic!("one accept: {events:?}");
        };
        assert_eq!(*answered, view);
        let (_, shape) = split_lines(sent);
        join_values(lines, shape).expect("lines are bytes")
    }

    fn cursor_line(&mut self, win: i64) -> i64 {
        self.number("return vim.api.nvim_win_get_cursor((...))[1]", vec![Value::from(win)])
    }

    fn buffer_keys(&mut self, buf: i64) -> Vec<String> {
        let keys = self.embed.lua(
            "local keys = {}
             for _, map in ipairs(vim.api.nvim_buf_get_keymap((...), 'n')) do keys[#keys + 1] = map.lhs end
             table.sort(keys)
             return keys",
            vec![Value::from(buf)],
        );
        strings(&keys)
    }

    /// The view's windows and buffers, found by the mark on the windows.
    fn pair(&mut self, view: u64) -> Pair {
        const PAIR: &str = "
local view = ...
local found = {}
for _, win in ipairs(vim.api.nvim_list_wins()) do
  local ok, id = pcall(vim.api.nvim_win_get_var, win, 'eitri_ide_view')
  if ok and id == view then
    local buf = vim.api.nvim_win_get_buf(win)
    found[#found + 1] = { win, buf, vim.api.nvim_buf_get_name(buf):find('^eitri%-file://') ~= nil }
  end
end
return found";
        let found = self.embed.lua(PAIR, vec![Value::from(view)]);
        let found = found.as_array().expect("a list");
        assert_eq!(found.len(), 2, "two windows carry the mark");
        let side = |file: bool| {
            let row = found
                .iter()
                .map(|row| row.as_array().expect("a row"))
                .find(|row| row[2].as_bool() == Some(file))
                .expect("both sides");
            (row[0].as_i64().expect("a window"), row[1].as_i64().expect("a buffer"))
        };
        Pair {
            file: side(true),
            proposal: side(false),
        }
    }

    fn windows(&mut self) -> i64 {
        self.number("return #vim.api.nvim_list_wins()", vec![])
    }

    fn buffers(&mut self) -> i64 {
        self.number("return #vim.api.nvim_list_bufs()", vec![])
    }

    fn number(&mut self, code: &str, args: Vec<Value>) -> i64 {
        self.embed.lua(code, args).as_i64().expect("a number")
    }

    fn truth(&mut self, code: &str, args: Vec<Value>) -> bool {
        self.embed.lua(code, args).as_bool().expect("a boolean")
    }

    fn current_window(&mut self) -> i64 {
        self.number("return vim.api.nvim_get_current_win()", vec![])
    }

    fn window_valid(&mut self, win: i64) -> bool {
        self.truth("return vim.api.nvim_win_is_valid((...))", vec![Value::from(win)])
    }

    fn buffer_valid(&mut self, buf: i64) -> bool {
        self.truth("return vim.api.nvim_buf_is_valid((...))", vec![Value::from(buf)])
    }

    fn window_option(&mut self, name: &str, win: i64) -> Value {
        self.embed.lua(
            "local name, win = ... return vim.api.nvim_get_option_value(name, { win = win })",
            vec![Value::from(name), Value::from(win)],
        )
    }

    fn buffer_option(&mut self, name: &str, buf: i64) -> Value {
        self.embed.lua(
            "local name, buf = ... return vim.api.nvim_get_option_value(name, { buf = buf })",
            vec![Value::from(name), Value::from(buf)],
        )
    }

    fn lines(&mut self, buf: i64) -> Vec<String> {
        let lines = self.embed.lua(
            "return vim.api.nvim_buf_get_lines((...), 0, -1, false)",
            vec![Value::from(buf)],
        );
        strings(&lines)
    }

    fn position(&mut self, win: i64) -> (i64, i64) {
        let at = self
            .embed
            .lua("return vim.api.nvim_win_get_position((...))", vec![Value::from(win)]);
        let at = at.as_array().expect("row and column");
        (at[0].as_i64().expect("a row"), at[1].as_i64().expect("a column"))
    }

    fn bar(&mut self, win: i64) -> String {
        bar(&mut self.embed, win)
    }

    /// What a redraw shows: the bar cut to the window's own width.
    fn drawn(&mut self, win: i64) -> String {
        drawn(&mut self.embed, win)
    }

    /// `:confirm` shows its dialog only to a UI, and without one nvim just goes on.
    fn attach_ui(&mut self) {
        self.embed.request(
            "nvim_ui_attach",
            vec![
                Value::from(80),
                Value::from(24),
                Value::Map(vec![(Value::from("ext_linegrid"), Value::from(true))]),
            ],
        );
    }

    /// `:confirm qall` ends nvim with no dialog, and so with nothing left to write.
    fn quits_with_no_prompt(&mut self) {
        self.embed.command("autocmd VimLeave * call writefile([], 'left')");
        let left = self.dir.join("left");
        self.embed.input(":confirm qall<CR>");
        // Asked only once it has failed: a request racing the exit could not be answered.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !left.exists() {
            if Instant::now() >= deadline {
                let mode = self.embed.request("nvim_get_mode", vec![]);
                panic!("nvim did not exit: {mode}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_nomodified(&mut self, buf: i64) {
        self.embed.wait_until("'modified' off", |embed| {
            embed.lua("return vim.bo[...].modified", vec![Value::from(buf)]) == Value::from(false)
        });
    }

    /// `diff_hlID` of a line's first column, which reads the current window.
    fn diff_mark(&mut self, win: i64, line: i64) -> i64 {
        self.number(
            "local win, line = ...
             return vim.api.nvim_win_call(win, function() return vim.fn.diff_hlID(line, 1) end)",
            vec![Value::from(win), Value::from(line)],
        )
    }

    fn set_agent_mark(&mut self, win: i64) {
        self.embed.lua(
            "vim.api.nvim_win_set_var((...), 'eitri_agent_view', true)",
            vec![Value::from(win)],
        );
    }

    /// A second window, with the person still in the first.
    fn another_window(&mut self) -> i64 {
        self.number(
            "local buf = vim.api.nvim_create_buf(true, false)
             return vim.api.nvim_open_win(buf, false, { split = 'right', win = 0 })",
            vec![],
        )
    }
}

const BAR: &str = "
local win, room = ...
local bar = vim.api.nvim_get_option_value('winbar', { win = win })
local width = room == 0 and vim.api.nvim_win_get_width(win) or room
return vim.api.nvim_eval_statusline(bar, { winid = win, use_winbar = true, maxwidth = width }).str";

/// What the window's bar says, evaluated as a redraw evaluates it, with room for all of it.
fn bar(embed: &mut Embed, win: i64) -> String {
    let bar = embed.lua(BAR, vec![Value::from(win), Value::from(1000)]);
    bar.as_str().expect("the bar's text").to_owned()
}

/// The same with the room the window really has.
fn drawn(embed: &mut Embed, win: i64) -> String {
    let bar = embed.lua(BAR, vec![Value::from(win), Value::from(0)]);
    bar.as_str().expect("the bar's text").to_owned()
}

fn names_in(dir: &std::path::Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .expect("the directory")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    names.sort();
    names
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("a list")
        .iter()
        .map(|line| line.as_str().expect("text").to_owned())
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct Pair {
    file: (i64, i64),
    proposal: (i64, i64),
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn opening_from_insert_leaves_the_mode_the_window_and_the_tabpage() {
    let mut nvim = start("insert");
    nvim.embed.input("i");
    nvim.embed.wait_until("insert mode", |embed| {
        embed.lua("return vim.api.nvim_get_mode().mode", vec![]).as_str() == Some("i")
    });
    let read = |nvim: &mut Nvim| {
        (
            nvim.embed.request("nvim_get_mode", vec![]),
            nvim.embed.request("nvim_get_current_win", vec![]),
            nvim.embed.request("nvim_get_current_tabpage", vec![]),
        )
    };
    let before = read(&mut nvim);
    nvim.opened(1, "x.txt", b"one\n", b"two\n");
    assert_eq!(read(&mut nvim), before);
    assert_eq!(nvim.windows(), 3, "two more windows exist");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_pair_goes_right_on_a_wide_window_and_below_on_a_narrow_one() {
    let mut nvim = start("sides");
    nvim.embed.command("set columns=200 lines=50");
    let person = nvim.current_window();
    let pair = nvim.opened(1, "x.txt", b"one\n", b"two\n");
    let (mine, file, proposal) = (
        nvim.position(person),
        nvim.position(pair.file.0),
        nvim.position(pair.proposal.0),
    );
    assert_eq!(file.0, mine.0, "beside the person's window");
    assert!(file.1 > mine.1, "to its right: {mine:?} {file:?}");
    assert_eq!(proposal.0, file.0, "the pair is side by side");
    assert!(proposal.1 > file.1, "the proposal on the right: {file:?} {proposal:?}");
    nvim.close(1);

    nvim.embed.command("set columns=159");
    let pair = nvim.opened(2, "x.txt", b"one\n", b"two\n");
    let (mine, file, proposal) = (
        nvim.position(person),
        nvim.position(pair.file.0),
        nvim.position(pair.proposal.0),
    );
    assert!(file.0 > mine.0, "below the person's window: {mine:?} {file:?}");
    assert_eq!(proposal.0, file.0, "the pair is side by side");
    assert!(proposal.1 > file.1, "the proposal on the right: {file:?} {proposal:?}");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_pair_takes_the_agent_windows_place() {
    let mut nvim = start("agent");
    let person = nvim.current_window();
    let agent = nvim.another_window();
    nvim.set_agent_mark(agent);
    nvim.opened(1, "x.txt", b"one\n", b"two\n");
    assert!(!nvim.window_valid(agent), "the agent window went");
    assert_eq!(nvim.windows(), 3);
    assert_eq!(nvim.current_window(), person);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_person_in_the_agent_window_keeps_it() {
    let mut nvim = start("agent-current");
    let person = nvim.current_window();
    nvim.set_agent_mark(person);
    nvim.opened(1, "x.txt", b"one\n", b"two\n");
    assert!(nvim.window_valid(person));
    assert_eq!(nvim.current_window(), person);
    assert_eq!(nvim.windows(), 3);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn an_agent_window_holding_the_only_view_of_unsaved_text_stands_beside_the_pair() {
    let mut nvim = start("agent-unsaved");
    let agent = nvim.another_window();
    nvim.set_agent_mark(agent);
    nvim.embed.lua(
        "local buf = vim.api.nvim_win_get_buf((...))
         vim.api.nvim_buf_set_lines(buf, 0, -1, false, { 'unsaved' })",
        vec![Value::from(agent)],
    );
    nvim.opened(1, "x.txt", b"one\n", b"two\n");
    assert!(nvim.window_valid(agent), "'hidden' on would hide the text if it closed");
    assert_eq!(nvim.windows(), 4);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_floating_current_window_keeps_the_person_and_the_pair_opens_under_it() {
    let mut nvim = start("float");
    let under = nvim.current_window();
    let float = nvim.number(
        "local buf = vim.api.nvim_create_buf(false, true)
         return vim.api.nvim_open_win(buf, true, { relative = 'editor', row = 1, col = 1, width = 10, height = 2 })",
        vec![],
    );
    nvim.opened(1, "x.txt", b"one\n", b"two\n");
    assert_eq!(nvim.current_window(), float);
    assert!(nvim.window_valid(under));
    assert_eq!(nvim.windows(), 4);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_crlf_file_shows_only_the_real_change() {
    let mut nvim = start("crlf");
    let on_disk = b"one\r\ntwo\r\nthree\r\n";
    let as_the_cli_reads_it = String::from_utf8_lossy(on_disk).replace("\r\n", "\n");
    let pair = nvim.opened(1, "x.txt", as_the_cli_reads_it.as_bytes(), b"one\nTWO\nthree\n");
    for win in [pair.file.0, pair.proposal.0] {
        assert_eq!(nvim.diff_mark(win, 1), 0, "an unchanged line");
        assert_ne!(nvim.diff_mark(win, 2), 0, "the changed line");
        assert_eq!(nvim.diff_mark(win, 3), 0, "an unchanged line");
    }
    assert_eq!(nvim.lines(pair.file.1), ["one", "two", "three"]);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_new_file_shows_an_empty_file_side() {
    let mut nvim = start("new-file");
    let pair = nvim.opened(1, "new.txt", b"", b"hello\n");
    assert_eq!(nvim.lines(pair.file.1), [""]);
    assert_eq!(nvim.lines(pair.proposal.1), ["hello"]);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_file_named_like_a_winbar_expression_runs_nothing() {
    let mut nvim = start("winbar");
    // The same text undoubled does run, so what is asserted below can fail.
    nvim.embed.lua(
        "vim.api.nvim_eval_statusline(...)",
        vec![
            Value::from("%{execute('call writefile([],\"control\")')}"),
            Value::Map(vec![]),
        ],
    );
    assert!(nvim.dir.join("control").exists(), "a raw expression runs");

    for (view, rel) in [(1, "%{execute('call writefile([],\"x\")')}.txt"), (2, "%!x")] {
        nvim.embed.command("set columns=500");
        let pair = nvim.opened(view, rel, b"one\n", b"two\n");
        nvim.embed.lua("vim.api.nvim__redraw({ flush = true })", vec![]);
        assert!(nvim.bar(pair.file.0).starts_with(&format!("{rel}: on disk")));
        assert!(nvim
            .bar(pair.proposal.0)
            .starts_with(&format!("{rel}: claude's proposal, editable")));
        assert!(!nvim.dir.join("x").exists(), "{rel} ran");
        nvim.close(view);

        // A narrow window builds its bar another way.
        nvim.embed.command("set columns=60");
        let pair = nvim.opened(view + 10, rel, b"one\n", b"two\n");
        nvim.embed.lua("vim.api.nvim__redraw({ flush = true })", vec![]);
        let end = &rel[rel.len() - 3..];
        assert!(nvim.drawn(pair.file.0).contains(end), "{}", nvim.drawn(pair.file.0));
        assert!(!nvim.dir.join("x").exists(), "{rel} ran");
        nvim.close(view + 10);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_bars_name_the_keys_with_the_local_leader_and_stay_on_their_windows() {
    let mut nvim = start("bars");
    let person = nvim.current_window();
    let pair = nvim.opened(1, "x.txt", b"one\n", b"two\n");
    for win in [pair.file.0, pair.proposal.0] {
        let bar = nvim.bar(win);
        for keys in ["\\a", "\\d rejects and stops claude's turn", "\\q"] {
            assert!(bar.contains(keys), "{keys} in {bar}");
        }
    }
    assert_eq!(nvim.window_option("winbar", person), Value::from(""));
    assert_eq!(nvim.embed.lua("return vim.go.winbar", vec![]), Value::from(""));
    nvim.close(1);

    nvim.embed.lua("vim.g.maplocalleader = '%'", vec![]);
    let pair = nvim.opened(2, "x.txt", b"one\n", b"two\n");
    let bar = nvim.bar(pair.file.0);
    assert!(bar.contains("%a"), "{bar}");
    nvim.close(2);

    nvim.embed.lua("vim.g.maplocalleader = ' '", vec![]);
    let pair = nvim.opened(3, "x.txt", b"one\n", b"two\n");
    let bar = nvim.bar(pair.file.0);
    assert!(bar.contains("<Space>a"), "{bar}");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn no_filetype_fires() {
    let mut nvim = start("filetype");
    nvim.embed
        .command("autocmd FileType * let g:fired = get(g:, 'fired', 0) + 1");
    let pair = nvim.opened(1, "x.py", b"one = 1\n", b"one = 2\n");
    assert_eq!(nvim.embed.eval("get(g:, 'fired', 0)"), Value::from(0));
    for buf in [pair.file.1, pair.proposal.1] {
        assert_eq!(nvim.buffer_option("syntax", buf), Value::from("python"));
        assert_eq!(nvim.buffer_option("filetype", buf), Value::from(""));
    }
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_name_with_no_syntax_sets_none() {
    let mut nvim = start("no-syntax");
    let pair = nvim.opened(1, "no-such-kind-of-file", b"one\n", b"two\n");
    assert_eq!(nvim.buffer_option("syntax", pair.file.1), Value::from(""));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn neither_buffer_has_a_swap_or_a_modeline() {
    let mut nvim = start("modeline");
    nvim.embed.command("autocmd User Probe let g:probed = 1");
    let text = b"vim: tabstop=3\n";
    let pair = nvim.opened(1, "x.txt", text, text);
    for (win, buf) in [pair.file, pair.proposal] {
        assert_eq!(nvim.buffer_option("modeline", buf), Value::from(false));
        assert_eq!(nvim.buffer_option("swapfile", buf), Value::from(false));
        assert_eq!(
            nvim.embed.lua("return vim.fn.swapname((...))", vec![Value::from(buf)]),
            Value::from("")
        );
        // `:doautocmd` reads modelines afterwards, when the buffer has them on.
        nvim.embed.lua(
            "vim.api.nvim_win_call((...), function() vim.cmd('doautocmd User Probe') end)",
            vec![Value::from(win)],
        );
        assert_eq!(nvim.embed.eval("g:probed"), Value::from(1));
        assert_eq!(nvim.buffer_option("tabstop", buf), Value::from(8));
    }
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn each_buffer_is_what_it_says_and_the_real_file_is_not_loaded() {
    let mut nvim = start("buffers");
    let rel = "real.txt";
    std::fs::write(nvim.dir.join(rel), "one\n").expect("the file");
    let real = nvim.dir.join(rel);
    let pair = nvim.opened(0x2a, rel, b"one\n", b"two\n");
    let name = |nvim: &mut Nvim, buf: i64| {
        let name = nvim
            .embed
            .lua("return vim.api.nvim_buf_get_name((...))", vec![Value::from(buf)]);
        name.as_str().expect("a name").to_owned()
    };
    assert_eq!(name(&mut nvim, pair.file.1), "eitri-file://00002a/real.txt");
    assert_eq!(name(&mut nvim, pair.proposal.1), "eitri-proposal://00002a/real.txt");
    for (option, file, proposal) in [
        ("buftype", Value::from("nofile"), Value::from("acwrite")),
        ("bufhidden", Value::from("wipe"), Value::from("wipe")),
        ("buflisted", Value::from(false), Value::from(false)),
        ("modified", Value::from(false), Value::from(false)),
        ("readonly", Value::from(true), Value::from(false)),
        ("modifiable", Value::from(false), Value::from(true)),
    ] {
        assert_eq!(nvim.buffer_option(option, pair.file.1), file, "{option}");
        assert_eq!(nvim.buffer_option(option, pair.proposal.1), proposal, "{option}");
    }
    let loaded = nvim.embed.lua(
        "return vim.fn.bufexists((...))",
        vec![Value::from(real.to_str().expect("a text path"))],
    );
    assert_eq!(loaded, Value::from(0));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn undo_in_a_new_proposal_has_nothing_to_undo() {
    let mut nvim = start("undo");
    let pair = nvim.opened(1, "x.txt", b"one\n", b"two\n");
    nvim.embed.lua(
        "vim.api.nvim_buf_call((...), function() vim.cmd('silent! undo') end)",
        vec![Value::from(pair.proposal.1)],
    );
    assert_eq!(nvim.lines(pair.proposal.1), ["two"]);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_tabpage_with_a_diff_refuses_and_leaves_its_windows_alone() {
    let mut nvim = start("own-diff");
    nvim.embed.command("vsplit");
    nvim.embed.command("windo diffthis");
    let state = |nvim: &mut Nvim| {
        nvim.embed.lua(
            "local seen = {}
             for _, win in ipairs(vim.api.nvim_list_wins()) do
               local get = function(name) return vim.api.nvim_get_option_value(name, { win = win }) end
               seen[#seen + 1] = { win, get('diff'), get('scrollbind'), get('foldmethod') }
             end
             return seen",
            vec![],
        )
    };
    let before = state(&mut nvim);
    let buffers = nvim.buffers();
    assert!(matches!(nvim.open(1, "x.txt", b"one\n", b"two\n"), Opened::Refused(_)));
    assert_eq!(state(&mut nvim), before);
    assert_eq!(nvim.windows(), 2);
    assert_eq!(nvim.buffers(), buffers);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_second_view_beside_an_open_one_is_refused() {
    let mut nvim = start("second");
    let pair = nvim.opened(1, "x.txt", b"one\n", b"two\n");
    let buffers = nvim.buffers();
    assert!(matches!(nvim.open(2, "y.txt", b"one\n", b"two\n"), Opened::Refused(_)));
    assert_eq!(nvim.windows(), 3);
    assert_eq!(nvim.buffers(), buffers);
    assert!(nvim.window_valid(pair.file.0));
    assert!(nvim.window_valid(pair.proposal.0));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_chunk_past_its_deadline_changes_nothing() {
    let mut nvim = start("deadline");
    let buffers = nvim.buffers();
    let stamp = nvim.stamp(wall_ms() - 1);
    let request = diff(&nvim, "x.txt", b"one\n", b"two\n");
    assert_eq!(nvim.open_stamped(stamp, 1, request), Opened::Expired);
    assert_eq!(nvim.windows(), 1);
    assert_eq!(nvim.buffers(), buffers);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn an_older_sequence_changes_nothing() {
    let mut nvim = start("sequence");
    let older = nvim.stamp(wall_ms() + FAR);
    nvim.opened(1, "x.txt", b"one\n", b"two\n");
    nvim.close(1);
    let request = diff(&nvim, "x.txt", b"one\n", b"two\n");
    assert_eq!(nvim.open_stamped(older, 2, request), Opened::Expired);
    assert_eq!(nvim.windows(), 1);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_late_close_still_closes() {
    let mut nvim = start("late-close");
    let deadline = wall_ms() + 1_000;
    let stamp = nvim.stamp(deadline);
    let request = diff(&nvim, "x.txt", b"one\n", b"two\n");
    assert_eq!(nvim.open_stamped(stamp, 1, request), Opened::Opened);
    while wall_ms() <= deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    nvim.close(1);
    assert_eq!(nvim.windows(), 1, "the close carries no stamp");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn close_of_no_such_view_does_nothing() {
    let mut nvim = start("no-view");
    nvim.embed.lua(IDE_DIFF_CLOSE, vec![Value::from(9)]);
    assert_eq!(nvim.windows(), 1, "no view was ever opened");
    nvim.opened(1, "x.txt", b"one\n", b"two\n");
    nvim.close(9);
    assert_eq!(nvim.windows(), 3, "another view stays");
    nvim.close(1);
    nvim.close(1);
    assert_eq!(nvim.windows(), 1, "a second close finds nothing to do");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_buffers_go_with_the_windows() {
    let mut nvim = start("buffers-go");
    let person = nvim.current_window();
    let pair = nvim.opened(1, "x.txt", b"one\n", b"two\n");
    nvim.close(1);
    assert!(!nvim.buffer_valid(pair.file.1));
    assert!(!nvim.buffer_valid(pair.proposal.1));
    assert!(!nvim.window_valid(pair.file.0));
    assert!(!nvim.window_valid(pair.proposal.0));
    assert_eq!(nvim.current_window(), person);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_third_window_on_the_proposal_goes_with_the_view() {
    let mut nvim = start("third-window");
    let pair = nvim.opened(1, "x.txt", b"one\n", b"two\n");
    let third = nvim.number(
        "return vim.api.nvim_open_win((...), false, { split = 'below', win = 0 })",
        vec![Value::from(pair.proposal.1)],
    );
    nvim.close(1);
    assert!(!nvim.buffer_valid(pair.proposal.1));
    assert!(!nvim.window_valid(third));
    assert_eq!(nvim.windows(), 1);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn closing_queues_no_event() {
    let mut nvim = start("no-event");
    nvim.opened(1, "x.txt", b"one\n", b"two\n");
    nvim.close(1);
    assert_eq!(nvim.take(), []);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_last_window_of_all_stays_on_an_empty_buffer() {
    let mut nvim = start("last-window");
    // With 'closeoff' nvim ends diff mode itself when one window of a diff is left, and this would show nothing.
    nvim.embed.command("set diffopt-=closeoff");
    let pair = nvim.opened(1, "x.txt", b"one\n", b"two\n");
    nvim.embed.lua(
        "vim.api.nvim_set_current_win((...))
         vim.api.nvim_buf_set_lines(vim.api.nvim_get_current_buf(), 0, -1, false, { 'an edit' })
         vim.cmd('only')",
        vec![Value::from(pair.proposal.0)],
    );
    nvim.close(1);
    assert_eq!(nvim.windows(), 1);
    assert!(nvim.window_valid(pair.proposal.0));
    let left = nvim.number("return vim.api.nvim_get_current_buf()", vec![]);
    assert_ne!(left, pair.proposal.1);
    assert_eq!(nvim.lines(left), [""]);
    assert!(!nvim.buffer_valid(pair.file.1));
    assert!(!nvim.buffer_valid(pair.proposal.1));
    assert_eq!(nvim.window_option("diff", pair.proposal.0), Value::from(false));
    assert_eq!(nvim.window_option("winbar", pair.proposal.0), Value::from(""));
    assert_eq!(nvim.embed.eval("exists('w:eitri_ide_view')"), Value::from(0));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_window_the_person_moved_to_another_buffer_stays_theirs() {
    let mut nvim = start("moved");
    // With 'closeoff' nvim ends diff mode itself when one window of a diff is left, and this would show nothing.
    nvim.embed.command("set diffopt-=closeoff");
    let pair = nvim.opened(1, "x.txt", b"one\n", b"two\n");
    let own = nvim.number("return vim.api.nvim_create_buf(true, false)", vec![]);
    nvim.embed.lua(
        "local win, buf = ... vim.api.nvim_win_set_buf(win, buf)",
        vec![Value::from(pair.proposal.0), Value::from(own)],
    );
    nvim.close(1);
    assert!(nvim.window_valid(pair.proposal.0), "it shows the person's buffer now");
    assert_eq!(nvim.window_option("diff", pair.proposal.0), Value::from(false));
    assert_eq!(nvim.window_option("winbar", pair.proposal.0), Value::from(""));
    assert!(!nvim.window_valid(pair.file.0));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_name_taken_by_a_buffer_undoes_what_was_made() {
    let mut nvim = start("name-taken");
    nvim.embed.lua(
        "vim.api.nvim_buf_set_name(vim.api.nvim_create_buf(true, false), 'eitri-proposal://000001/x.txt')",
        vec![],
    );
    let buffers = nvim.buffers();
    assert!(matches!(nvim.open(1, "x.txt", b"one\n", b"two\n"), Opened::Refused(_)));
    assert_eq!(nvim.buffers(), buffers);
    assert_eq!(nvim.windows(), 1);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn no_room_for_the_second_window_undoes_the_first() {
    let mut nvim = start("no-room");
    nvim.embed
        .command("set columns=161 lines=50 winwidth=60 winminwidth=60");
    let buffers = nvim.buffers();
    let Opened::Refused(why) = nvim.open(1, "x.txt", b"one\n", b"two\n") else {
        panic!("two more windows of 60 do not fit in 161");
    };
    assert!(!why.is_empty());
    assert_eq!(nvim.windows(), 1);
    assert_eq!(nvim.buffers(), buffers);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn take_gives_at_least_one_event_and_keeps_what_is_past_its_bound() {
    let mut nvim = start("take");
    assert_eq!(nvim.take(), [], "no view was ever opened");
    nvim.embed.lua(
        "local line = string.rep('a', 5 * 1024 * 1024)
         _G.__eitri_ide = { views = {}, events = {
           { view = 1, kind = 'accept', lines = { line } },
           { view = 2, kind = 'accept', lines = { line } },
           { view = 3, kind = 'reject' },
           { view = 4, kind = 'closed' },
         } }",
        vec![],
    );
    let views = |events: Vec<ViewEvent>| -> Vec<u64> {
        events
            .into_iter()
            .map(|event| match event {
                ViewEvent::Accept { view, .. } | ViewEvent::Reject { view } | ViewEvent::Closed { view } => view,
            })
            .collect()
    };
    let first = nvim.take();
    assert_eq!(views(first), [1], "a second large text would pass the bound");
    let second = nvim.take();
    assert_eq!(views(second), [2, 3, 4], "what was kept comes next, in order");
    assert_eq!(nvim.take(), []);

    nvim.embed.lua(
        "_G.__eitri_ide.events = { { view = 5, kind = 'accept', lines = { string.rep('a', 9 * 1024 * 1024) } } }",
        vec![],
    );
    assert_eq!(views(nvim.take()), [5], "one event past the bound still goes");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn accept_returns_the_proposals_bytes_for_a_crlf_file() {
    let mut nvim = start("accept-crlf");
    let on_disk = b"one\r\ntwo\r\n";
    let as_the_cli_reads_it = String::from_utf8_lossy(on_disk).replace("\r\n", "\n");
    let proposal = b"one\nTWO\n";
    let pair = nvim.opened(1, "x.txt", as_the_cli_reads_it.as_bytes(), proposal);
    nvim.enter(pair.proposal.0);
    nvim.embed.input("\\a");
    assert_eq!(nvim.accepted_text(1, proposal), proposal);
    nvim.assert_gone(pair);
    assert_eq!(nvim.windows(), 1);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn accept_returns_a_proposal_without_a_final_newline_as_sent() {
    let mut nvim = start("accept-no-eol");
    let proposal = b"one\ntwo";
    let pair = nvim.opened(1, "x.txt", b"one\n", proposal);
    nvim.enter(pair.file.0);
    nvim.embed.input("\\a");
    assert_eq!(nvim.accepted_text(1, proposal), proposal);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn accept_of_an_edited_proposal_returns_the_edit() {
    let mut nvim = start("accept-edit");
    let sent = b"one\ntwo\n";
    let pair = nvim.opened(1, "x.txt", b"one\n", sent);
    nvim.enter(pair.proposal.0);
    nvim.edit(pair.proposal.1, "A!<Esc>");
    nvim.embed.input("\\a");
    assert_eq!(nvim.accepted_text(1, sent), b"one!\ntwo\n");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_commands_answer_from_either_buffer() {
    let mut nvim = start("commands");
    let sent = b"two\n";
    let pair = nvim.opened(1, "x.txt", b"one\n", sent);
    nvim.enter(pair.file.0);
    nvim.embed.command("EitriDiffAccept");
    assert_eq!(nvim.accepted_text(1, sent), sent);

    let pair = nvim.opened(2, "x.txt", b"one\n", sent);
    nvim.enter(pair.proposal.0);
    nvim.embed.command("EitriDiffReject");
    assert_eq!(nvim.answered(), [ViewEvent::Reject { view: 2 }]);
    let exists = nvim.embed.eval("exists(':EitriDiffAccept')");
    assert_eq!(exists, Value::from(0), "the commands are the buffers'");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn save_returns_the_edited_text_and_writes_nothing() {
    let mut nvim = start("save");
    let sent = b"one\n";
    let pair = nvim.opened(1, "x.txt", b"zero\n", sent);
    nvim.settle();
    let before = names_in(&nvim.dir);
    nvim.enter(pair.proposal.0);
    nvim.edit(pair.proposal.1, "A!<Esc>");
    nvim.embed.input(":w<CR>");
    assert_eq!(nvim.accepted_text(1, sent), b"one!\n");
    assert_eq!(names_in(&nvim.dir), before, "the working directory is as it was");
    assert!(!nvim.dir.join("x.txt").exists());
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_proposal_stays_nomodified_through_every_edit() {
    let mut nvim = start("nomodified");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\ntwo\n");
    let proposal = pair.proposal.1;
    nvim.enter(pair.proposal.0);
    nvim.edit(proposal, "iq");
    assert_eq!(
        nvim.embed.lua("return vim.api.nvim_get_mode().mode", vec![]),
        Value::from("i"),
        "off while still inserting"
    );
    nvim.embed.input("<Esc>");
    for keys in ["x", "dd", "u", "p"] {
        nvim.edit(proposal, keys);
    }
    assert_eq!(nvim.lines(proposal), ["one", "one", "two"]);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn wa_update_and_a_focus_lost_autosave_answer_nothing() {
    let mut nvim = start("save-all");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\n");
    nvim.embed.command("autocmd FocusLost * silent! wa");
    nvim.enter(pair.proposal.0);
    nvim.edit(pair.proposal.1, "A!<Esc>");
    for command in ["wa", "update", "doautocmd FocusLost"] {
        nvim.embed.command(command);
        nvim.assert_nothing_answered_and_open(pair, command);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn an_edit_and_a_save_all_in_one_command_answer_nothing() {
    let mut nvim = start("chained");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"xxx\n");
    nvim.enter(pair.proposal.0);
    for command in ["s/x/y/ | wa", "s/x/y/ | update"] {
        nvim.embed.command(command);
        assert!(!nvim.modified(pair.proposal.1), "{command}");
        nvim.assert_nothing_answered_and_open(pair, command);
    }
    nvim.embed.request(
        "nvim_feedkeys",
        vec![Value::from("x:wa\r"), Value::from("tx"), Value::from(false)],
    );
    assert!(!nvim.modified(pair.proposal.1));
    nvim.assert_nothing_answered_and_open(pair, "keys fed as one");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_save_all_with_the_popup_menu_open_answers_nothing() {
    let mut nvim = start("popup");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"apple\napricot\n");
    nvim.embed.command("autocmd FocusLost * silent! wa");
    nvim.enter(pair.proposal.0);
    let proposal = pair.proposal.1;
    nvim.edit(proposal, "Goap<C-n>");
    nvim.embed
        .wait_until("the menu", |embed| embed.eval("pumvisible()") == Value::from(1));
    nvim.embed.wait_until("'modified' off with the menu up", |embed| {
        embed.lua("return vim.bo[...].modified", vec![Value::from(proposal)]) == Value::from(false)
    });
    nvim.embed.command("doautocmd FocusLost");
    nvim.embed.flush_scheduled();
    assert_eq!(nvim.embed.eval("pumvisible()"), Value::from(1), "the menu stayed up");
    assert_eq!(nvim.take(), []);
    assert!(nvim.window_valid(pair.proposal.0));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn q_closes_with_the_request_still_open_after_an_edit() {
    let mut nvim = start("q-after-edit");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\n");
    nvim.enter(pair.proposal.0);
    nvim.edit(pair.proposal.1, "A!<Esc>");
    // An E37 would fail this request.
    nvim.embed.command("q");
    assert_eq!(nvim.answered(), [ViewEvent::Closed { view: 1 }]);
    nvim.assert_gone(pair);
    assert_eq!(nvim.windows(), 1);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn confirm_qall_shows_no_prompt_for_it() {
    let mut nvim = start("confirm-qall");
    nvim.attach_ui();
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\n");
    nvim.enter(pair.proposal.0);
    nvim.edit(pair.proposal.1, "A!<Esc>");
    nvim.quits_with_no_prompt();
}

/// Holds up the two around it: with a UI, a modified `acwrite` buffer is what stops `:confirm qall`.
#[test]
#[ignore = "needs a real nvim on PATH"]
fn confirm_qall_stops_on_a_modified_acwrite_buffer() {
    let mut nvim = start("confirm-control");
    nvim.attach_ui();
    nvim.embed.lua(
        "local buf = vim.api.nvim_get_current_buf()
         vim.bo[buf].buftype = 'acwrite'
         vim.api.nvim_buf_set_name(buf, 'eitri-proposal://000001/x.txt')
         vim.api.nvim_buf_set_lines(buf, 0, -1, false, { 'an edit' })",
        vec![],
    );
    nvim.embed.input(":confirm qall<CR>");
    nvim.embed.wait_until("the prompt", |embed| {
        let mode = embed.request("nvim_get_mode", vec![]);
        let fields = mode.as_map().expect("a map");
        let field = fields.iter().find(|(name, _)| name.as_str() == Some("mode"));
        field.is_some_and(|(_, mode)| mode.as_str() == Some("r?"))
    });
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn confirm_qall_after_a_put_from_the_file_side_shows_no_prompt() {
    let mut nvim = start("confirm-dp");
    nvim.attach_ui();
    let pair = nvim.opened(1, "x.txt", b"a\nb\nc\n", b"a\nB\nc\n");
    nvim.enter(pair.file.0);
    nvim.embed.input("]cdp");
    nvim.embed.wait_until("the hunk to land", |embed| {
        let line = embed.lua(
            "return vim.api.nvim_buf_get_lines((...), 1, 2, false)[1]",
            vec![Value::from(pair.proposal.1)],
        );
        line == Value::from("b")
    });
    nvim.wait_nomodified(pair.proposal.1);
    nvim.quits_with_no_prompt();
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn reject_queues_a_rejection() {
    let mut nvim = start("reject");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\n");
    nvim.enter(pair.proposal.0);
    nvim.embed.input("\\d");
    assert_eq!(nvim.answered(), [ViewEvent::Reject { view: 1 }]);
    nvim.assert_gone(pair);
    assert_eq!(nvim.windows(), 1);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_close_key_and_q_answer_nothing() {
    let mut nvim = start("close-key");
    for (view, keys, side) in [
        (1, "\\q", true),
        (2, "\\q", false),
        (3, ":q<CR>", true),
        (4, ":q<CR>", false),
    ] {
        let pair = nvim.opened(view, "x.txt", b"zero\n", b"one\n");
        nvim.enter(if side { pair.file.0 } else { pair.proposal.0 });
        nvim.embed.input(keys);
        assert_eq!(nvim.answered(), [ViewEvent::Closed { view }], "{keys}");
        nvim.assert_gone(pair);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn closing_either_window_closes_the_pair() {
    let mut nvim = start("close-either");
    let person = nvim.current_window();
    let close_window = "vim.api.nvim_win_close((...), true)";
    let wipe_buffer = "vim.api.nvim_buf_delete((...), { force = true })";
    for (view, code, side) in [
        (1, close_window, true),
        (2, close_window, false),
        (3, wipe_buffer, true),
        (4, wipe_buffer, false),
    ] {
        let pair = nvim.opened(view, "x.txt", b"zero\n", b"one\n");
        let (win, buf) = if side { pair.file } else { pair.proposal };
        let target = if code == close_window { win } else { buf };
        nvim.embed.lua(code, vec![Value::from(target)]);
        assert_eq!(nvim.answered(), [ViewEvent::Closed { view }], "{code}");
        nvim.assert_gone(pair);
        assert_eq!(nvim.current_window(), person);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_same_as_the_file_winbar_comes_and_goes() {
    let mut nvim = start("same");
    nvim.embed.command("set columns=500");
    let on_disk = b"one\r\ntwo\r\n";
    let as_the_cli_reads_it = String::from_utf8_lossy(on_disk).replace("\r\n", "\n");
    let pair = nvim.opened(1, "x.txt", as_the_cli_reads_it.as_bytes(), b"one\nTWO\n");
    let win = pair.proposal.0;
    assert!(!nvim.bar(win).contains(SAME));
    nvim.enter(win);
    nvim.edit(pair.proposal.1, ":2s/TWO/two/<CR>");
    nvim.embed
        .wait_until("the bar to say so", |embed| bar(embed, win).contains(SAME));
    let shown = nvim.bar(win);
    assert!(shown.trim_end().ends_with(SAME), "{shown}");
    assert!(shown.contains("d rejects and stops claude's turn"), "{shown}");
    assert!(!nvim.bar(pair.file.0).contains(SAME), "only the proposal's bar");
    nvim.edit(pair.proposal.1, "A!<Esc>");
    nvim.embed
        .wait_until("the words to go", |embed| !bar(embed, win).contains(SAME));
    assert!(nvim.bar(win).contains("d rejects and stops claude's turn"));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_proposal_equal_to_the_file_says_so_from_the_start() {
    let mut nvim = start("same-at-open");
    nvim.embed.command("set columns=500");
    let pair = nvim.opened(1, "x.txt", b"one\n", b"one\n");
    assert!(nvim.bar(pair.proposal.0).contains(SAME));
    nvim.close(1);
    let pair = nvim.opened(2, "x.txt", b"", b"");
    assert!(
        nvim.bar(pair.proposal.0).contains(SAME),
        "the one empty line of an empty proposal is no text"
    );
    nvim.close(2);
    let pair = nvim.opened(3, "x.txt", b"one", b"one\n");
    assert!(
        !nvim.bar(pair.proposal.0).contains(SAME),
        "a final newline is a difference"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn only_in_the_proposal_closes_the_view_and_leaves_an_empty_window() {
    let mut nvim = start("only");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\n");
    nvim.enter(pair.proposal.0);
    nvim.embed.command("only");
    assert_eq!(nvim.answered(), [ViewEvent::Closed { view: 1 }]);
    assert_eq!(nvim.windows(), 1);
    assert!(nvim.window_valid(pair.proposal.0), "the last window of all stays");
    let left = nvim.number("return vim.api.nvim_get_current_buf()", vec![]);
    assert_ne!(left, pair.proposal.1);
    assert_eq!(nvim.lines(left), [""]);
    assert!(!nvim.buffer_valid(pair.proposal.1));
    assert_eq!(nvim.window_option("diff", pair.proposal.0), Value::from(false));
    assert_eq!(nvim.window_option("winbar", pair.proposal.0), Value::from(""));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_hunk_motions_are_neovims_own() {
    let mut nvim = start("motions");
    let pair = nvim.opened(1, "x.txt", b"a\nb\nc\nd\n", b"a\nb\nc\nD\n");
    let keys = ["\\a", "\\d", "\\q"];
    assert_eq!(nvim.buffer_keys(pair.file.1), keys, "nothing else is mapped");
    assert_eq!(nvim.buffer_keys(pair.proposal.1), keys, "nothing else is mapped");
    nvim.enter(pair.proposal.0);
    let win = pair.proposal.0;
    assert_eq!(nvim.cursor_line(win), 1);
    assert_eq!(nvim.embed.eval("maparg(']c', 'n')"), Value::from(""));
    assert_eq!(nvim.embed.eval("maparg('[c', 'n')"), Value::from(""));
    nvim.embed.input("]c");
    nvim.embed.wait_until("the cursor at the hunk", |embed| {
        embed.lua("return vim.api.nvim_win_get_cursor((...))[1]", vec![Value::from(win)]) == Value::from(4)
    });
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_view_in_another_tabpage_keeps_its_own_events() {
    let mut nvim = start("tabpages");
    let first = nvim.opened(1, "x.txt", b"zero\n", b"first\n");
    nvim.embed.command("tabnew");
    let second = nvim.opened(2, "y.txt", b"zero\n", b"second\n");
    nvim.embed.command("tabprevious");
    nvim.enter(first.proposal.0);
    nvim.embed.input("\\a");
    assert_eq!(nvim.accepted_text(1, b"first\n"), b"first\n");
    nvim.assert_gone(first);
    assert!(nvim.window_valid(second.proposal.0), "the other view stays");
    nvim.embed.command("tabnext");
    nvim.enter(second.proposal.0);
    nvim.embed.input("\\d");
    assert_eq!(nvim.answered(), [ViewEvent::Reject { view: 2 }]);
    nvim.assert_gone(second);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_second_answer_before_the_view_closes_is_nothing() {
    let mut nvim = start("second-answer");
    let sent = b"one\n";
    let pair = nvim.opened(1, "x.txt", b"zero\n", sent);
    nvim.enter(pair.proposal.0);
    // One chunk: nothing scheduled can run between these.
    nvim.embed.lua(
        "vim.cmd('EitriDiffAccept')
         vim.cmd('write')
         vim.cmd('EitriDiffReject')",
        vec![],
    );
    assert_eq!(nvim.accepted_text(1, sent), sent);
    nvim.assert_gone(pair);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn wq_in_the_proposal_accepts_once() {
    let mut nvim = start("wq");
    let sent = b"one\n";
    let pair = nvim.opened(1, "x.txt", b"zero\n", sent);
    nvim.enter(pair.proposal.0);
    nvim.embed.input(":wq<CR>");
    assert_eq!(nvim.accepted_text(1, sent), sent);
    nvim.assert_gone(pair);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn saving_under_another_name_is_no_answer() {
    let mut nvim = start("save-as");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\n");
    nvim.enter(pair.proposal.0);
    for command in ["write other.txt", "saveas again.txt"] {
        nvim.embed.command(command);
        nvim.assert_nothing_answered_and_open(pair, command);
    }
    assert!(!nvim.dir.join("other.txt").exists());
    assert!(!nvim.dir.join("again.txt").exists());
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_partial_write_is_no_answer() {
    let mut nvim = start("partial");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\ntwo\n");
    nvim.enter(pair.proposal.0);
    for (command, code) in [("1write", "E140"), ("1write!", "E676")] {
        let error = nvim
            .embed
            .lua("return select(2, pcall(vim.cmd, (...)))", vec![Value::from(command)]);
        let error = error.as_str().expect("the write's error");
        assert!(error.contains(code), "{error}");
        nvim.assert_nothing_answered_and_open(pair, command);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_closed_view_leaves_no_timer_and_no_autocommand() {
    let mut nvim = start("footprint");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\n");
    let group = "eitri_ide_view_1";
    assert!(
        nvim.embed.footprint(&[group]).autocmds > 0,
        "the hooks are in that group"
    );
    nvim.enter(pair.proposal.0);
    nvim.edit(pair.proposal.1, "A!<Esc>");
    nvim.close(1);
    let left = nvim.embed.footprint(&[group]);
    assert_eq!((left.autocmds, left.active_timers), (0, 0));
    nvim.settle();
    assert_eq!(nvim.take(), [], "a close from the host is no answer");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_put_from_the_file_side_keeps_the_proposal_nomodified_and_the_bar_true() {
    let mut nvim = start("dp");
    nvim.embed.command("set columns=500");
    let pair = nvim.opened(1, "x.txt", b"a\nb\nc\n", b"a\nB\nc\n");
    let (win, proposal) = pair.proposal;
    assert!(!nvim.bar(win).contains(SAME));
    nvim.enter(pair.file.0);
    nvim.embed.input("]cdp");
    nvim.embed.wait_until("the hunk to land", |embed| {
        let line = embed.lua(
            "return vim.api.nvim_buf_get_lines((...), 1, 2, false)[1]",
            vec![Value::from(proposal)],
        );
        line == Value::from("b")
    });
    nvim.wait_nomodified(proposal);
    nvim.embed
        .wait_until("the bar to say so", |embed| bar(embed, win).contains(SAME));
    nvim.assert_nothing_answered_and_open(pair, "a put");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn an_edit_and_an_explicit_write_in_one_command_answer_nothing() {
    let mut nvim = start("chained-write");
    let sent = b"xxx\n";
    let pair = nvim.opened(1, "x.txt", b"zero\n", sent);
    nvim.enter(pair.proposal.0);
    nvim.embed.command("s/x/y/ | write");
    assert!(!nvim.modified(pair.proposal.1));
    nvim.assert_nothing_answered_and_open(pair, "an edit behind a write");
    nvim.embed.command("write");
    assert_eq!(nvim.accepted_text(1, sent), b"yxx\n");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_bars_keep_the_path_and_the_accept_key_at_the_windows_own_width() {
    let mut nvim = start("widths");
    let deep = "a/directory/far/too/deep/for/any/bar/to/hold/from/one/end/to/the/other/module.rs";
    let mut view = 0;
    for columns in [80, 120, 200, 240] {
        nvim.embed.command(&format!("set columns={columns}"));
        for rel in ["src/some/dir/module.rs", deep] {
            view += 1;
            let pair = nvim.opened(view, rel, b"one\n", b"two\n");
            for (win, what) in [(pair.file.0, "disk"), (pair.proposal.0, "proposal")] {
                let shown = nvim.drawn(win);
                assert!(shown.contains("module.rs"), "{columns}: the path in {shown}");
                assert!(shown.contains("\\a accepts"), "{columns}: the key in {shown}");
                assert!(shown.contains(what), "{columns}: what it is in {shown}");
            }
            nvim.close(view);

            view += 1;
            let pair = nvim.opened(view, rel, b"one\n", b"one\n");
            let shown = nvim.drawn(pair.proposal.0);
            assert!(shown.contains("module.rs"), "{columns}: the path in {shown}");
            assert!(shown.contains("\\a"), "{columns}: the key in {shown}");
            assert!(
                shown.contains(SAME) || shown.contains("\\a rejects: same as file"),
                "{columns}: the warning in {shown}"
            );
            nvim.close(view);
        }
    }
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_bar_follows_its_window() {
    let mut nvim = start("resize");
    nvim.attach_ui();
    nvim.embed
        .request("nvim_ui_try_resize", vec![Value::from(500), Value::from(24)]);
    let rel = "src/some/dir/module.rs";
    let pair = nvim.opened(1, rel, b"one\n", b"two\n");
    let win = pair.proposal.0;
    let full = format!("{rel}: claude's proposal, editable");
    assert!(nvim.drawn(win).starts_with(&full));
    let set_width = "local win, width = ... vim.api.nvim_win_set_width(win, width)";
    nvim.embed.lua(set_width, vec![Value::from(win), Value::from(45)]);
    nvim.embed.wait_until("the short bar", |embed| {
        let shown = drawn(embed, win);
        shown.contains("module.rs: proposal") && shown.contains("\\a accepts")
    });
    nvim.embed.lua(set_width, vec![Value::from(win), Value::from(160)]);
    nvim.embed
        .wait_until("the full bar", |embed| drawn(embed, win).starts_with(&full));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn an_accept_past_the_bound_is_refused_and_the_view_stays() {
    let mut nvim = start("too-large");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\n");
    let proposal = pair.proposal.1;
    nvim.enter(pair.proposal.0);
    nvim.embed.lua(
        "local line, lines = string.rep('a', 1024 * 1024), {}
         for index = 1, 9 do lines[index] = line end
         vim.api.nvim_buf_set_lines((...), 0, -1, false, lines)",
        vec![Value::from(proposal)],
    );
    nvim.wait_nomodified(proposal);
    for keys in ["\\a", ":w<CR>", ":EitriDiffAccept<CR>"] {
        nvim.embed.input(keys);
        nvim.assert_nothing_answered_and_open(pair, keys);
    }
    let messages = nvim.embed.lua(
        "return vim.api.nvim_exec2('messages', { output = true }).output",
        vec![],
    );
    assert!(messages.as_str().expect("text").contains("8 MiB"), "{messages}");

    nvim.embed.lua(
        "vim.api.nvim_buf_set_lines((...), 1, -1, false, {})",
        vec![Value::from(proposal)],
    );
    nvim.wait_nomodified(proposal);
    nvim.embed.input("\\a");
    let events = nvim.answered();
    let [ViewEvent::Accept { view: 1, lines }] = events.as_slice() else {
        panic!("one accept of what is left");
    };
    assert_eq!(lines.len(), 1);
    nvim.assert_gone(pair);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn edit_in_the_proposal_changes_nothing() {
    let mut nvim = start("reload");
    nvim.embed
        .command("autocmd FileType * let g:fired = get(g:, 'fired', 0) + 1");
    let pair = nvim.opened(1, "x.py", b"zero\n", b"def one\ntwo\n");
    let proposal = pair.proposal.1;
    nvim.enter(pair.proposal.0);
    nvim.edit(proposal, "A!<Esc>");
    let colour = "return vim.fn.synIDattr(vim.fn.synID(1, 1, 1), 'name')";
    let before = nvim.embed.lua(colour, vec![]);
    assert_ne!(before, Value::from(""), "`def` has a colour");
    for command in ["edit", "edit!"] {
        nvim.embed.command(command);
        assert_eq!(nvim.lines(proposal), ["def one!", "two"], "{command}");
        nvim.wait_nomodified(proposal);
        nvim.assert_nothing_answered_and_open(pair, command);
        assert_eq!(nvim.embed.lua(colour, vec![]), before, "{command}");
        assert_eq!(nvim.window_option("diff", pair.proposal.0), Value::from(true));
    }
    assert_eq!(nvim.embed.eval("get(g:, 'fired', 0)"), Value::from(0));
    for (option, value) in [("swapfile", false), ("modeline", false)] {
        assert_eq!(nvim.buffer_option(option, proposal), Value::from(value), "{option}");
    }

    // A change from the other window is still seen after the reload.
    nvim.enter(pair.file.0);
    nvim.embed.input("dp");
    nvim.embed.wait_until("the hunk to land", |embed| {
        let line = embed.lua(
            "return vim.api.nvim_buf_get_lines((...), 0, 1, false)[1]",
            vec![Value::from(proposal)],
        );
        line == Value::from("zero")
    });
    nvim.wait_nomodified(proposal);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_close_under_the_command_window_finishes_when_it_goes() {
    let mut nvim = start("cmdwin");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\n");
    nvim.enter(pair.proposal.0);
    nvim.embed.input("q:");
    nvim.embed.wait_until("the command window", |embed| {
        embed.eval("getcmdwintype()") == Value::from(":")
    });
    nvim.close(1);
    nvim.embed.input(":quit<CR>");
    let proposal = pair.proposal.0;
    nvim.embed.wait_until("the view to go", |embed| {
        embed.lua("return vim.api.nvim_win_is_valid((...))", vec![Value::from(proposal)]) == Value::from(false)
    });
    nvim.assert_gone(pair);
    assert_eq!(nvim.take(), [], "a close from the host is no answer");
    assert_eq!(nvim.open(2, "x.txt", b"zero\n", b"one\n"), Opened::Opened);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_diff_of_the_persons_own_in_a_moved_window_stays() {
    let mut nvim = start("own-diff-moved");
    nvim.embed.command("set diffopt-=closeoff");
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\n");
    nvim.embed.lua(
        "local win = ...
         vim.api.nvim_win_set_buf(win, vim.api.nvim_create_buf(true, false))
         vim.api.nvim_win_call(win, function() vim.cmd('diffthis') end)",
        vec![Value::from(pair.proposal.0)],
    );
    nvim.close(1);
    assert_eq!(nvim.window_option("diff", pair.proposal.0), Value::from(true));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_close_from_the_host_leaves_the_mode_alone() {
    let mut nvim = start("close-insert");
    let person = nvim.current_window();
    let pair = nvim.opened(1, "x.txt", b"zero\n", b"one\n");
    nvim.enter(pair.proposal.0);
    nvim.embed.input("i");
    nvim.embed.wait_until("insert mode", |embed| {
        embed.lua("return vim.api.nvim_get_mode().mode", vec![]).as_str() == Some("i")
    });
    nvim.close(1);
    nvim.settle();
    assert_eq!(
        nvim.embed.lua("return vim.api.nvim_get_mode().mode", vec![]),
        Value::from("i")
    );
    assert_eq!(nvim.current_window(), person);
}
