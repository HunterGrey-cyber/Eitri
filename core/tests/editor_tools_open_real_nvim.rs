//! `open_file`'s chunk in a real `nvim`: what it shows, where, and everything it leaves alone.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eitri_core::editor_tools::stamp::{wall_ms, Stamp};
use eitri_core::editor_tools::tools::open_file::OPEN_FILE_LUA;
use embed_client::Embed;
use rmpv::Value;

const FAR: u64 = 60_000;

/// What a person would notice changing under their hands.
const PERSON: &str = "
local m = vim.api.nvim_get_mode()
local cursor = vim.api.nvim_win_get_cursor(0)
return {
  mode = m.mode,
  blocking = m.blocking,
  win = vim.api.nvim_get_current_win(),
  tab = vim.api.nvim_get_current_tabpage(),
  buf = vim.api.nvim_get_current_buf(),
  row = cursor[1],
  col = cursor[2],
  lines = vim.api.nvim_buf_get_lines(0, 0, -1, false),
  modified = vim.bo.modified,
  anchor = vim.fn.getpos('v'),
}";

/// Counts of what exists, across every tabpage.
const COUNTS: &str = "
local named = 0
for _, b in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_get_name(b) ~= '' then named = named + 1 end
end
return { bufs = #vim.api.nvim_list_bufs(), named = named, wins = #vim.api.nvim_list_wins(), tabs = #vim.api.nvim_list_tabpages() }";

/// The windows of the current tabpage that carry the agent mark: `{win, buf name, line}` for each.
const MARKED: &str = "
local out = {}
for _, w in ipairs(vim.api.nvim_tabpage_list_wins(vim.api.nvim_get_current_tabpage())) do
  local ok, mark = pcall(vim.api.nvim_win_get_var, w, 'eitri_agent_view')
  if ok and mark then
    table.insert(out, { win = w, name = vim.api.nvim_buf_get_name(vim.api.nvim_win_get_buf(w)),
      line = vim.api.nvim_win_get_cursor(w)[1] })
  end
end
return out";

struct Nvim {
    embed: Embed,
    dir: PathBuf,
    project: PathBuf,
    seq: u64,
}

impl Drop for Nvim {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start_in(case: &str, label: &str, extra: &[&str]) -> Nvim {
    let dir = std::env::temp_dir().join(format!("eto-{}-{case}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let project = dir.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let embed = Embed::start(&project, extra, &[]);
    Nvim {
        embed,
        dir,
        project,
        seq: 0,
    }
}

fn start(case: &str) -> Nvim {
    start_in(case, "a", &[])
}

impl Nvim {
    fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.project.join(name);
        std::fs::write(&path, content).expect("write a file");
        path
    }

    fn numbered(&self, name: &str, lines: u32) -> PathBuf {
        let text: String = (1..=lines).map(|n| format!("{name} line {n}\n")).collect();
        self.file(name, &text)
    }

    fn stamp(&mut self) -> Stamp {
        self.seq += 1;
        Stamp {
            deadline_ms: wall_ms() + FAR,
            scope: "open".to_owned(),
            seq: self.seq,
        }
    }

    /// Calls the chunk as the broker would.
    fn open(&mut self, path: &Path, line: Option<u32>, end_line: Option<u32>, load: bool) -> Value {
        let stamp = self.stamp();
        self.open_stamped(&stamp, path, line, end_line, load)
    }

    fn open_stamped(
        &mut self,
        stamp: &Stamp,
        path: &Path,
        line: Option<u32>,
        end_line: Option<u32>,
        load: bool,
    ) -> Value {
        self.embed.lua(
            OPEN_FILE_LUA,
            vec![stamp.to_value(), arguments(path, line, end_line, load)],
        )
    }

    fn edit(&mut self, path: &Path) {
        self.embed.command(&format!("edit {}", path.display()));
    }

    fn person(&mut self) -> Value {
        sorted(self.embed.lua(PERSON, vec![]))
    }

    fn counts(&mut self) -> Value {
        sorted(self.embed.lua(COUNTS, vec![]))
    }

    fn marked(&mut self) -> Vec<(i64, String, i64)> {
        let list = self.embed.lua(MARKED, vec![]);
        list.as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        (
                            get(item, "win").as_i64().unwrap(),
                            get(item, "name").as_str().unwrap().to_owned(),
                            get(item, "line").as_i64().unwrap(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn arguments(path: &Path, line: Option<u32>, end_line: Option<u32>, load: bool) -> Value {
    use std::os::unix::ffi::OsStrExt;
    let mut args = vec![
        (Value::from("path"), Value::Binary(path.as_os_str().as_bytes().to_vec())),
        (Value::from("load"), Value::from(load)),
    ];
    if let Some(line) = line {
        args.push((Value::from("line"), Value::from(line)));
    }
    if let Some(end_line) = end_line {
        args.push((Value::from("end_line"), Value::from(end_line)));
    }
    Value::Map(args)
}

/// A table's keys have no order in Lua: sort them so two answers compare equal when they hold the same.
fn sorted(value: Value) -> Value {
    match value {
        Value::Map(mut entries) => {
            for (_, inner) in entries.iter_mut() {
                *inner = sorted(std::mem::replace(inner, Value::Nil));
            }
            entries.sort_by(|a, b| a.0.as_str().cmp(&b.0.as_str()));
            Value::Map(entries)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(name, _)| name.as_str() == Some(key))
        .map(|(_, value)| value)
}

fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    find(value, key).unwrap_or_else(|| panic!("no {key:?} in {value:?}"))
}

fn says(value: &Value, key: &str) -> bool {
    find(value, key).is_some_and(|found| *found == Value::from(true))
}

/// The `where` of a shown answer.
fn place(answer: &Value) -> String {
    assert!(says(answer, "shown"), "{answer:?}");
    get(answer, "where").as_str().unwrap().to_owned()
}

fn line_of(answer: &Value) -> i64 {
    get(answer, "line").as_i64().unwrap()
}

#[test]
#[ignore = "needs nvim on PATH"]
fn from_insert_mode_nothing_of_the_persons_changes() {
    let mut nvim = start("insert");
    let mine = nvim.numbered("mine.rs", 5);
    let other = nvim.numbered("other.rs", 40);
    nvim.edit(&mine);
    nvim.embed.lua("vim.api.nvim_win_set_cursor(0, { 3, 2 })", vec![]);
    nvim.embed.input("ix");
    nvim.embed.wait_until("insert mode with the typed key", |embed| {
        let state = embed.lua(PERSON, vec![]);
        get(&state, "mode").as_str() == Some("i") && get(&state, "modified") == &Value::from(true)
    });
    let before = nvim.person();

    let answer = nvim.open(&other, Some(20), Some(22), true);
    assert_eq!(place(&answer), "new_split");
    assert_eq!(line_of(&answer), 20);

    assert_eq!(nvim.person(), before, "mode, window, cursor and text stayed");
    let marked = nvim.marked();
    assert_eq!(marked.len(), 1);
    assert_eq!(Path::new(&marked[0].1), other);
    assert_eq!(marked[0].2, 20);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn from_visual_and_terminal_mode_too() {
    let mut nvim = start("modes");
    let mine = nvim.numbered("mine.rs", 8);
    let other = nvim.numbered("other.rs", 8);
    nvim.edit(&mine);
    nvim.embed.lua("vim.api.nvim_win_set_cursor(0, { 2, 1 })", vec![]);
    nvim.embed.input("vj");
    nvim.embed.wait_until("visual mode", |embed| {
        get(&embed.lua(PERSON, vec![]), "mode").as_str() == Some("v")
    });
    let before = nvim.person();
    assert_eq!(place(&nvim.open(&other, Some(3), None, true)), "new_split");
    assert_eq!(nvim.person(), before, "visual mode and its selection stayed");
    nvim.embed.input("<Esc>");

    nvim.embed.command("terminal cat");
    nvim.embed.input("i");
    nvim.embed.wait_until("terminal mode", |embed| {
        get(&embed.lua(PERSON, vec![]), "mode").as_str() == Some("t")
    });
    // nvim's own first refresh of the terminal moves its cursor some milliseconds after the mode changes: the
    // person's state is read once it has stopped moving, so only the open can be what changes it afterwards.
    let (mut last, mut since) = (None, std::time::Instant::now());
    nvim.embed.wait_until("the terminal to settle", |embed| {
        let now = sorted(embed.lua(PERSON, vec![]));
        if last.as_ref() != Some(&now) {
            (last, since) = (Some(now), std::time::Instant::now());
        }
        since.elapsed() >= std::time::Duration::from_millis(200)
    });
    let before = nvim.person();
    let answer = nvim.open(&mine, Some(4), None, true);
    assert_eq!(
        place(&answer),
        "agent_window",
        "the person's window is not one to reuse"
    );
    let after = nvim.person();
    assert_eq!(
        after, before,
        "terminal mode and its window stayed: {after:?} against {before:?}"
    );
}

#[test]
#[ignore = "needs nvim on PATH"]
fn the_agent_window_is_reused() {
    let mut nvim = start("reuse");
    let mine = nvim.numbered("mine.rs", 10);
    let a = nvim.numbered("a.rs", 30);
    let b = nvim.numbered("b.rs", 30);
    nvim.edit(&mine);
    assert_eq!(place(&nvim.open(&a, Some(7), None, true)), "new_split");
    let first = nvim.marked();
    assert_eq!(place(&nvim.open(&b, Some(9), None, true)), "agent_window");
    let second = nvim.marked();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].0, first[0].0, "the same window");
    assert_eq!(Path::new(&second[0].1), b);
    assert_eq!(second[0].2, 9);
    assert_eq!(get(&nvim.counts(), "wins").as_i64(), Some(2));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_modified_buffer_shown_nowhere_else_is_never_replaced() {
    let mut nvim = start("modified-agent");
    let mine = nvim.numbered("mine.rs", 10);
    let a = nvim.numbered("a.rs", 10);
    let b = nvim.numbered("b.rs", 10);
    nvim.edit(&mine);
    assert_eq!(place(&nvim.open(&a, Some(2), None, true)), "new_split");
    // The person types into the agent's window, then goes back.
    nvim.embed.lua(
        "local w = vim.fn.win_findbuf(vim.fn.bufnr(...))[1]
         vim.api.nvim_buf_set_lines(vim.api.nvim_win_get_buf(w), 0, 1, false, { 'my change' })",
        vec![Value::from(a.to_str().unwrap())],
    );
    assert_eq!(
        place(&nvim.open(&b, Some(3), None, true)),
        "new_split",
        "a new split, not that window"
    );
    let shown = nvim.embed.lua(
        "local out = {}
         for _, w in ipairs(vim.api.nvim_tabpage_list_wins(0)) do
           local buf = vim.api.nvim_win_get_buf(w)
           table.insert(out, { name = vim.api.nvim_buf_get_name(buf), modified = vim.bo[buf].modified })
         end
         return out",
        vec![],
    );
    let items = shown.as_array().unwrap();
    assert_eq!(items.len(), 3);
    let kept = items
        .iter()
        .find(|item| Path::new(get(item, "name").as_str().unwrap()) == a)
        .expect("the modified buffer is still in a window");
    assert!(says(kept, "modified"));
    let marks = nvim.marked();
    assert_eq!(marks.len(), 1, "one agent window is left marked");
    assert_eq!(Path::new(&marks[0].1), b);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_modified_buffer_is_never_reloaded() {
    let mut nvim = start("noreload");
    let mine = nvim.file("mine.rs", "one\ntwo\nthree\n");
    nvim.edit(&mine);
    nvim.embed
        .lua("vim.api.nvim_buf_set_lines(0, 0, 1, false, { 'unsaved' })", vec![]);
    std::fs::write(&mine, "changed on disk\n").unwrap();
    let before = nvim.person();

    assert_eq!(place(&nvim.open(&mine, Some(2), None, false)), "new_split");
    assert_eq!(place(&nvim.open(&mine, Some(2), None, true)), "existing");
    assert_eq!(nvim.person(), before, "the buffer kept its unsaved text");
    let lines = nvim
        .embed
        .lua("return vim.api.nvim_buf_get_lines(0, 0, -1, false)", vec![]);
    assert_eq!(lines.as_array().unwrap()[0].as_str(), Some("unsaved"));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn the_current_window_is_never_reused_even_on_the_same_file() {
    let mut nvim = start("current");
    let mine = nvim.numbered("mine.rs", 60);
    nvim.edit(&mine);
    nvim.embed.lua("vim.api.nvim_win_set_cursor(0, { 30, 0 })", vec![]);
    let before = nvim.person();

    assert_eq!(place(&nvim.open(&mine, Some(5), None, true)), "new_split");
    assert_eq!(nvim.person(), before, "the person's view did not jump");
    assert_eq!(get(&nvim.counts(), "wins").as_i64(), Some(2));
    assert_eq!(place(&nvim.open(&mine, Some(50), None, true)), "existing");
    assert_eq!(nvim.person(), before);
    assert_eq!(get(&nvim.counts(), "wins").as_i64(), Some(2));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn one_agent_window_per_tabpage() {
    let mut nvim = start("tabs");
    let mine = nvim.numbered("mine.rs", 10);
    let files: Vec<PathBuf> = (0..4).map(|n| nvim.numbered(&format!("f{n}.rs"), 10)).collect();
    nvim.edit(&mine);
    for file in &files[..3] {
        nvim.open(file, Some(2), None, true);
    }
    assert_eq!(nvim.marked().len(), 1);
    nvim.embed.command("tabnew");
    assert_eq!(place(&nvim.open(&files[3], Some(2), None, true)), "new_split");
    assert_eq!(nvim.marked().len(), 1, "the new tabpage has its own");
    nvim.embed.command("tabprevious");
    let first = nvim.marked();
    assert_eq!(first.len(), 1);
    assert_eq!(
        Path::new(&first[0].1),
        files[2],
        "the first tabpage's window was left alone"
    );
}

/// Every `SwapExists` handler there is, by group: the editor has one of its own.
const SWAP_HANDLERS: &str = "
local out = {}
for _, a in ipairs(vim.api.nvim_get_autocmds({ event = 'SwapExists' })) do
  table.insert(out, tostring(a.group_name))
end
table.sort(out)
return out";

/// The files of `dir` that are swap files.
fn swap_files(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".sw"))
        .collect();
    found.sort();
    found
}

/// The options that make `nvim` keep its swap file in `swaps`, and that file's owner: another editor that dies
/// without cleaning up. Returns the options and the swap files it left for `target`.
fn leave_stale_swap(nvim: &Nvim, target: &Path) -> (PathBuf, String, Vec<String>) {
    leave_swap(nvim, target, true)
}

/// [`leave_stale_swap`], with the dead editor's unsaved text or without any.
fn leave_swap(nvim: &Nvim, target: &Path, unsaved: bool) -> (PathBuf, String, Vec<String>) {
    let swaps = nvim.dir.join("swaps");
    std::fs::create_dir_all(&swaps).unwrap();
    let options = format!("set swapfile updatecount=200 directory={}//", swaps.display());
    {
        let mut crashed = Embed::start(&nvim.dir.join("elsewhere"), &[], &[]);
        crashed.command(&options);
        crashed.command(&format!("edit {}", target.display()));
        if unsaved {
            crashed.lua("vim.api.nvim_buf_set_lines(0, 0, 1, false, { 'unsaved text' })", vec![]);
        }
        crashed.command("preserve");
    }
    let stale = swap_files(&swaps);
    assert_eq!(stale.len(), 1, "the other editor left a swap file: {stale:?}");
    (swaps, options, stale)
}

/// A person at a screen: the grid UI Eitri attaches, with the editor's own message grid (no external messages).
/// Without a UI nothing ever waits for a key, so a prompt cannot show.
fn attach_ui(nvim: &mut Nvim) {
    let options = Value::Map(vec![
        (Value::from("ext_linegrid"), Value::from(true)),
        (Value::from("ext_multigrid"), Value::from(true)),
        (Value::from("rgb"), Value::from(true)),
    ]);
    nvim.embed
        .request("nvim_ui_attach", vec![Value::from(120), Value::from(40), options]);
}

/// The mode after a settle time: a hit-enter prompt starts to wait a moment after the chunk returned.
fn settled_mode(nvim: &mut Nvim) -> Value {
    std::thread::sleep(Duration::from_millis(600));
    nvim.embed.request("nvim_get_mode", vec![])
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_stale_swap_raises_no_prompt_and_deletes_nothing() {
    let mut nvim = start("swap");
    let target = nvim.numbered("target.rs", 5);
    let (swaps, options, stale) = leave_stale_swap(&nvim, &target);

    attach_ui(&mut nvim);
    nvim.embed.command(&options);
    let handlers_before = nvim.embed.lua(SWAP_HANDLERS, vec![]);
    let before = nvim.counts();
    let answer = nvim.open(&target, Some(2), None, true);
    assert!(says(&answer, "swap"), "{answer:?}");
    assert!(find(&answer, "shown").is_none());

    let mode = settled_mode(&mut nvim);
    assert_eq!(find(&mode, "mode"), Some(&Value::from("n")), "{mode:?}");
    assert_eq!(
        find(&mode, "blocking"),
        Some(&Value::from(false)),
        "no prompt is waiting"
    );
    // The editor still answers: a prompt would hold every later chunk until a key.
    assert_eq!(nvim.embed.eval("1 + 1").as_i64(), Some(2));
    assert_eq!(swap_files(&swaps), stale, "the swap file is still there");
    assert_eq!(nvim.counts(), before, "no buffer was left behind");
    assert_eq!(nvim.marked().len(), 0);
    let handlers = nvim.embed.lua(SWAP_HANDLERS, vec![]);
    assert_eq!(handlers, handlers_before, "the one-shot handler is gone");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_swap_file_is_found_whatever_the_persons_shortmess() {
    let mut nvim = start("swap-shortmess");
    let target = nvim.numbered("target.rs", 5);
    let (swaps, options, stale) = leave_stale_swap(&nvim, &target);

    attach_ui(&mut nvim);
    nvim.embed.command(&options);
    for flags in ["set shortmess+=A", "set shortmess-=A"] {
        nvim.embed.command(flags);
        let shortmess = nvim.embed.eval("&shortmess");
        let answer = nvim.open(&target, Some(2), None, true);
        assert!(says(&answer, "swap"), "{flags}: {answer:?}");
        assert_eq!(
            nvim.embed.eval("&shortmess"),
            shortmess,
            "{flags}: the person's flags come back"
        );
        let mode = settled_mode(&mut nvim);
        assert_eq!(find(&mode, "blocking"), Some(&Value::from(false)), "{flags}: {mode:?}");
        assert_eq!(
            swap_files(&swaps),
            stale,
            "{flags}: neither a new swap file nor a deleted one"
        );
    }
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_swap_with_no_changes_refuses_the_file_and_stays() {
    let mut nvim = start("swap-clean");
    let target = nvim.numbered("target.rs", 5);
    let (swaps, options, stale) = leave_swap(&nvim, &target, false);

    attach_ui(&mut nvim);
    nvim.embed.command(&options);
    let before = nvim.counts();
    let answer = nvim.open(&target, Some(2), None, true);
    assert!(says(&answer, "swap"), "{answer:?}");
    let mode = settled_mode(&mut nvim);
    assert_eq!(find(&mode, "blocking"), Some(&Value::from(false)), "{mode:?}");
    assert_eq!(swap_files(&swaps), stale, "the load leaves the swap file alone");
    assert_eq!(nvim.counts(), before, "no buffer was left behind");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_hook_of_the_persons_keeps_what_it_did_to_shortmess() {
    let mut nvim = start("shortmess-hook");
    let target = nvim.numbered("target.rs", 5);
    nvim.embed.command("set shortmess-=c shortmess-=A");
    nvim.embed.command("autocmd BufReadPost * set shortmess+=c");
    let answer = nvim.open(&target, Some(2), None, true);
    assert_eq!(place(&answer), "new_split", "{answer:?}");
    let shortmess = nvim.embed.eval("&shortmess");
    let shortmess = shortmess.as_str().expect("an option's value is text");
    assert!(shortmess.contains('c'), "the hook's flag stays: {shortmess}");
    assert!(!shortmess.contains('A'), "the load's own flag goes: {shortmess}");
}

/// Shows `target` in an editor with a UI and the person's `hook` set, then holds what a person would see: the
/// file is on screen and nothing waits for a key. A prompt raised inside the load would keep the chunk from
/// answering at all.
fn shown_with_no_prompt(case: &str, hook: &str) {
    let mut nvim = start(case);
    let target = nvim.numbered("target.rs", 5);
    attach_ui(&mut nvim);
    nvim.embed.command(hook);
    let answer = nvim.open(&target, Some(2), None, true);
    assert_eq!(place(&answer), "new_split", "{hook}: {answer:?}");
    let mode = settled_mode(&mut nvim);
    assert_eq!(find(&mode, "mode"), Some(&Value::from("n")), "{hook}: {mode:?}");
    assert_eq!(find(&mode, "blocking"), Some(&Value::from(false)), "{hook}: {mode:?}");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_long_notice_from_a_filetype_hook_raises_no_prompt() {
    shown_with_no_prompt(
        "hook-filetype",
        "autocmd FileType rust lua vim.notify(string.rep('x', 200), vim.log.levels.WARN)",
    );
}

#[test]
#[ignore = "needs nvim on PATH"]
fn lines_printed_by_a_read_hook_raise_no_prompt() {
    shown_with_no_prompt(
        "hook-read",
        "autocmd BufReadPost * lua print('first line\\nsecond line')",
    );
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_hook_that_throws_refuses_the_load_with_no_prompt() {
    let mut nvim = start("hook-throws");
    let target = nvim.numbered("target.rs", 5);
    attach_ui(&mut nvim);
    nvim.embed
        .command("autocmd BufReadPost * lua error(string.rep('x', 200))");
    let before = nvim.counts();
    let answer = nvim.open(&target, Some(2), None, true);
    assert!(find(&answer, "refused").is_some(), "{answer:?}");
    let mode = settled_mode(&mut nvim);
    assert_eq!(find(&mode, "blocking"), Some(&Value::from(false)), "{mode:?}");
    assert_eq!(nvim.counts(), before, "no buffer was left behind");
}

/// Prints what does not fit the message line, so a person's screen would wait for a key if the message showed.
const TWO_LINES: &str = "lua print('first line\\nsecond line')";

#[test]
#[ignore = "needs nvim on PATH"]
fn lines_printed_on_entering_a_new_split_raise_no_prompt() {
    shown_with_no_prompt("hook-winenter", &format!("autocmd BufWinEnter * {TWO_LINES}"));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_long_notice_on_entering_a_new_split_raises_no_prompt() {
    shown_with_no_prompt(
        "hook-winenter-long",
        "autocmd BufWinEnter * lua vim.notify(string.rep('x', 200), vim.log.levels.WARN)",
    );
}

#[test]
#[ignore = "needs nvim on PATH"]
fn lines_printed_for_a_new_window_raise_no_prompt() {
    shown_with_no_prompt("hook-winnew", &format!("autocmd WinNew * {TWO_LINES}"));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn lines_printed_for_a_new_buffer_raise_no_prompt() {
    shown_with_no_prompt("hook-bufnew", &format!("autocmd BufNew * {TWO_LINES}"));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_loaded_buffer_raises_no_prompt_with_no_load_at_all() {
    let mut nvim = start("hook-loaded");
    let target = nvim.numbered("target.rs", 5);
    attach_ui(&mut nvim);
    let name = target.to_str().unwrap();
    nvim.embed
        .lua("vim.fn.bufload(vim.fn.bufadd(...))", vec![Value::from(name)]);
    nvim.embed.command(&format!("autocmd BufWinEnter * {TWO_LINES}"));
    let answer = nvim.open(&target, Some(2), None, false);
    assert_eq!(place(&answer), "new_split", "{answer:?}");
    let mode = settled_mode(&mut nvim);
    assert_eq!(find(&mode, "mode"), Some(&Value::from("n")), "{mode:?}");
    assert_eq!(find(&mode, "blocking"), Some(&Value::from(false)), "{mode:?}");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn reusing_the_agent_window_raises_no_prompt() {
    let mut nvim = start("hook-reuse");
    let first = nvim.numbered("first.rs", 5);
    let second = nvim.numbered("second.rs", 5);
    attach_ui(&mut nvim);
    assert_eq!(place(&nvim.open(&first, Some(2), None, true)), "new_split");
    nvim.embed.command(&format!("autocmd BufWinEnter * {TWO_LINES}"));
    nvim.embed.command(&format!("autocmd BufWinLeave * {TWO_LINES}"));
    let answer = nvim.open(&second, Some(2), None, true);
    assert_eq!(place(&answer), "agent_window", "{answer:?}");
    let mode = settled_mode(&mut nvim);
    assert_eq!(find(&mode, "mode"), Some(&Value::from("n")), "{mode:?}");
    assert_eq!(find(&mode, "blocking"), Some(&Value::from(false)), "{mode:?}");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn lines_printed_while_a_load_is_given_up_raise_no_prompt() {
    let mut nvim = start("hook-given-up");
    let target = nvim.numbered("target.rs", 5);
    let (swaps, options, stale) = leave_stale_swap(&nvim, &target);
    attach_ui(&mut nvim);
    nvim.embed.command(&options);
    for event in ["BufUnload", "BufDelete", "BufWipeout"] {
        nvim.embed.command(&format!("autocmd {event} * {TWO_LINES}"));
    }
    let before = nvim.counts();
    let answer = nvim.open(&target, Some(2), None, true);
    assert!(says(&answer, "swap"), "{answer:?}");
    let mode = settled_mode(&mut nvim);
    assert_eq!(find(&mode, "blocking"), Some(&Value::from(false)), "{mode:?}");
    assert_eq!(swap_files(&swaps), stale, "the swap file is still there");
    assert_eq!(nvim.counts(), before, "no buffer was left behind");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn from_insert_mode_a_printing_hook_changes_nothing_of_the_persons() {
    let mut nvim = start("hook-insert");
    let mine = nvim.numbered("mine.rs", 3);
    let target = nvim.numbered("target.rs", 5);
    attach_ui(&mut nvim);
    nvim.edit(&mine);
    nvim.embed.command(&format!("autocmd BufWinEnter * {TWO_LINES}"));
    nvim.embed.input("ohello");
    let before = nvim.person();
    assert_eq!(find(&before, "mode"), Some(&Value::from("i")), "{before:?}");
    let answer = nvim.open(&target, Some(2), None, true);
    assert_eq!(place(&answer), "new_split", "{answer:?}");
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(nvim.person(), before);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_hook_that_throws_on_entering_the_split_refuses_with_no_prompt() {
    let mut nvim = start("hook-winenter-throws");
    let target = nvim.numbered("target.rs", 5);
    attach_ui(&mut nvim);
    nvim.embed
        .command("autocmd BufWinEnter * lua error(string.rep('x', 200))");
    let answer = nvim.open(&target, Some(2), None, true);
    assert!(find(&answer, "refused").is_some(), "{answer:?}");
    let mode = settled_mode(&mut nvim);
    assert_eq!(find(&mode, "blocking"), Some(&Value::from(false)), "{mode:?}");
    let counts = nvim.counts();
    assert_eq!(find(&counts, "wins"), Some(&Value::from(1)), "{counts:?}");
    assert_eq!(nvim.marked().len(), 0);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_bad_modeline_refuses_the_file_with_no_prompt() {
    let mut nvim = start("bad-modeline");
    let target = nvim.file("target.rs", "one\ntwo\n// vim: set bogusopt:\n");
    attach_ui(&mut nvim);
    let before = nvim.counts();
    let answer = nvim.open(&target, Some(2), None, true);
    assert!(find(&answer, "refused").is_some(), "{answer:?}");
    let mode = settled_mode(&mut nvim);
    assert_eq!(find(&mode, "blocking"), Some(&Value::from(false)), "{mode:?}");
    assert_eq!(nvim.counts(), before, "no buffer was left behind");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_notice_at_error_level_from_a_hook_refuses_the_load_with_no_prompt() {
    let mut nvim = start("hook-error-notice");
    let target = nvim.numbered("target.rs", 5);
    attach_ui(&mut nvim);
    nvim.embed
        .command("autocmd FileType rust lua vim.notify('server failed', vim.log.levels.ERROR)");
    let before = nvim.counts();
    let answer = nvim.open(&target, Some(2), None, true);
    assert!(find(&answer, "refused").is_some(), "{answer:?}");
    let mode = settled_mode(&mut nvim);
    assert_eq!(find(&mode, "blocking"), Some(&Value::from(false)), "{mode:?}");
    assert_eq!(nvim.counts(), before, "no buffer was left behind");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_path_wider_than_the_screen_raises_no_prompt() {
    let mut nvim = start("long-path");
    let deep = nvim.project.join("d".repeat(100)).join("e".repeat(100));
    std::fs::create_dir_all(&deep).unwrap();
    let target = deep.join("target.rs");
    std::fs::write(&target, "one\ntwo\nthree\n").unwrap();
    attach_ui(&mut nvim);
    // Neither of the flags that shorten or drop the file message of a read.
    nvim.embed.command("set shortmess=filnxToOS");
    let answer = nvim.open(&target, Some(2), None, true);
    assert_eq!(place(&answer), "new_split", "{answer:?}");
    let mode = settled_mode(&mut nvim);
    assert_eq!(find(&mode, "blocking"), Some(&Value::from(false)), "{mode:?}");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_given_up_load_wipes_only_the_buffer_it_added() {
    let mut nvim = start("swap-wipe");
    let target = nvim.numbered("target.rs", 5);
    let (_swaps, options, _stale) = leave_stale_swap(&nvim, &target);
    nvim.embed.command(&options);
    // A hook of the person's, made while a file loads: a scratch buffer with text in it.
    nvim.embed.lua(
        "vim.api.nvim_create_autocmd('BufNew', { callback = function()
           if vim.g.hook_buf == nil then
             vim.g.hook_buf = 0
             local b = vim.api.nvim_create_buf(true, false)
             vim.api.nvim_buf_set_lines(b, 0, -1, false, { 'kept text' })
             vim.g.hook_buf = b
           end
         end })",
        vec![],
    );
    let answer = nvim.open(&target, Some(2), None, true);
    assert!(says(&answer, "swap"), "{answer:?}");
    let kept = nvim.embed.lua(
        "local b = vim.g.hook_buf
         return { valid = vim.api.nvim_buf_is_valid(b), lines = vim.api.nvim_buf_get_lines(b, 0, -1, false) }",
        vec![],
    );
    assert_eq!(find(&kept, "valid"), Some(&Value::from(true)), "{kept:?}");
    assert_eq!(
        find(&kept, "lines"),
        Some(&Value::Array(vec![Value::from("kept text")])),
        "{kept:?}"
    );
    let named = nvim.embed.lua(
        "local out = 0
         for _, b in ipairs(vim.api.nvim_list_bufs()) do
           if vim.api.nvim_buf_get_name(b):find('target.rs', 1, true) then out = out + 1 end
         end
         return out",
        vec![],
    );
    assert_eq!(named.as_i64(), Some(0), "the buffer the load added is gone");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_load_that_outlasts_the_deadline_shows_no_window() {
    let mut nvim = start("slow-load");
    let target = nvim.numbered("slow.rs", 5);
    nvim.embed.lua(
        "vim.api.nvim_create_autocmd('BufReadPost', { callback = function() vim.uv.sleep(800) end })",
        vec![],
    );
    let before = nvim.counts();
    let stamp = Stamp {
        deadline_ms: wall_ms() + 400,
        scope: "open".to_owned(),
        seq: 1,
    };
    let answer = nvim.open_stamped(&stamp, &target, Some(2), None, true);
    assert!(says(&answer, "expired"), "{answer:?}");
    let after = nvim.counts();
    assert_eq!(find(&after, "wins"), find(&before, "wins"), "no window appeared");
    assert_eq!(nvim.marked().len(), 0);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn pattern_like_paths_reach_their_own_buffer() {
    let mut nvim = start("patterns");
    nvim.file("plain.rs", "plain\n");
    let names = ["*", "%", "#", "[a]", "a"];
    let paths: Vec<PathBuf> = names
        .iter()
        .map(|name| nvim.file(name, &format!("content of {name}\n")))
        .collect();
    let mine = nvim.numbered("mine.rs", 3);
    nvim.edit(&mine);
    for path in &paths {
        let answer = nvim.open(path, Some(1), None, true);
        assert!(says(&answer, "shown"), "{path:?}: {answer:?}");
        let marked = nvim.marked();
        assert_eq!(marked.len(), 1);
        assert_eq!(Path::new(&marked[0].1), path, "its own buffer, not another file's");
        let text = nvim.embed.lua(
            "return vim.api.nvim_buf_get_lines(vim.api.nvim_win_get_buf(...), 0, -1, false)[1]",
            vec![Value::from(marked[0].0)],
        );
        let name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(text.as_str(), Some(format!("content of {name}").as_str()));
    }
}

/// Sends one request on a second connection and leaves its answer to be read later.
struct Raw {
    stream: UnixStream,
}

impl Raw {
    fn connect(socket: &Path) -> Raw {
        let stream = UnixStream::connect(socket).expect("nvim's listen socket");
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Raw { stream }
    }

    fn send_lua(&mut self, code: &str, args: Vec<Value>) {
        let message = Value::Array(vec![
            Value::from(0),
            Value::from(1),
            Value::from("nvim_exec_lua"),
            Value::Array(vec![Value::from(code), Value::Array(args)]),
        ]);
        rmpv::encode::write_value(&mut self.stream, &message).unwrap();
        self.stream.flush().unwrap();
    }

    fn answer(&mut self) -> Value {
        let Value::Array(items) = rmpv::decode::read_value(&mut self.stream).expect("an answer") else {
            panic!("not a response");
        };
        assert!(items[2].is_nil(), "nvim returned an error: {:?}", items[2]);
        items[3].clone()
    }
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_chunk_past_its_deadline_changes_nothing() {
    let probe = std::env::temp_dir().join(format!("eto-{}-deadline-sock", std::process::id()));
    let _ = std::fs::remove_file(&probe);
    let mut nvim = start_in("deadline", "a", &["--listen", probe.to_str().unwrap()]);
    let mine = nvim.numbered("mine.rs", 5);
    let other = nvim.numbered("other.rs", 5);
    nvim.edit(&mine);
    let before = (nvim.person(), nvim.counts());

    // nvim cannot read its socket while a callback sleeps: the chunk waits in the pipe past its deadline.
    nvim.embed
        .lua("vim.schedule(function() vim.uv.sleep(1500) end)", vec![]);
    std::thread::sleep(Duration::from_millis(150));
    let mut raw = Raw::connect(&probe);
    let stamp = Stamp {
        deadline_ms: wall_ms() + 200,
        scope: "open".to_owned(),
        seq: 1,
    };
    raw.send_lua(
        OPEN_FILE_LUA,
        vec![stamp.to_value(), arguments(&other, Some(2), None, true)],
    );

    let answer = raw.answer();
    assert!(says(&answer, "expired"), "{answer:?}");
    assert_eq!((nvim.person(), nvim.counts()), before, "no window, no buffer");
    assert_eq!(nvim.marked().len(), 0);
    let _ = std::fs::remove_file(&probe);

    // It used no number: the same sequence number still runs.
    let fresh = Stamp {
        deadline_ms: wall_ms() + FAR,
        scope: "open".to_owned(),
        seq: 1,
    };
    let answer = nvim.open_stamped(&fresh, &other, Some(2), None, true);
    assert_eq!(place(&answer), "new_split");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn an_older_sequence_changes_nothing() {
    let mut nvim = start("older");
    let mine = nvim.numbered("mine.rs", 5);
    let a = nvim.numbered("a.rs", 5);
    let b = nvim.numbered("b.rs", 5);
    nvim.edit(&mine);
    let newer = Stamp {
        deadline_ms: wall_ms() + FAR,
        scope: "open".to_owned(),
        seq: 5,
    };
    assert_eq!(place(&nvim.open_stamped(&newer, &a, Some(1), None, true)), "new_split");
    let before = (nvim.person(), nvim.counts(), nvim.marked());

    let older = Stamp { seq: 3, ..newer };
    let answer = nvim.open_stamped(&older, &b, Some(1), None, true);
    assert!(says(&answer, "expired"), "{answer:?}");
    assert_eq!((nvim.person(), nvim.counts(), nvim.marked()), before);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn load_false_adds_no_buffer() {
    let mut nvim = start("noload");
    let mine = nvim.numbered("mine.rs", 5);
    let other = nvim.numbered("other.rs", 5);
    nvim.edit(&mine);
    let before = (nvim.person(), nvim.counts());
    let answer = nvim.open(&other, Some(2), None, false);
    assert!(says(&answer, "needs_load"), "{answer:?}");
    assert!(find(&answer, "shown").is_none());
    assert_eq!((nvim.person(), nvim.counts()), before, "nothing was added");
    assert_eq!(nvim.marked().len(), 0);

    // A listed buffer that is not loaded is not loaded either.
    nvim.embed.command(&format!("badd {}", other.display()));
    let before = nvim.counts();
    let answer = nvim.open(&other, Some(2), None, false);
    assert!(says(&answer, "needs_load"), "{answer:?}");
    assert_eq!(nvim.counts(), before);
    let loaded = nvim.embed.lua(
        "return vim.api.nvim_buf_is_loaded(vim.fn.bufnr(...))",
        vec![Value::from("other.rs")],
    );
    assert_eq!(loaded, Value::from(false));
}

const EXTMARKS: &str = "
local ns = vim.api.nvim_create_namespace('eitri_agent_view')
local buf = vim.fn.bufnr(...)
return #vim.api.nvim_buf_get_extmarks(buf, ns, 0, -1, {})";

#[test]
#[ignore = "needs nvim on PATH"]
fn the_highlight_clears_on_entering_the_window() {
    let mut nvim = start("highlight");
    let mine = nvim.numbered("mine.rs", 5);
    let other = nvim.numbered("other.rs", 20);
    nvim.edit(&mine);
    nvim.open(&other, Some(5), Some(8), true);
    let name = Value::from("other.rs");
    assert_eq!(nvim.embed.lua(EXTMARKS, vec![name.clone()]).as_i64(), Some(1));

    let window = nvim.marked()[0].0;
    nvim.embed
        .lua("vim.api.nvim_set_current_win(...)", vec![Value::from(window)]);
    assert_eq!(nvim.embed.lua(EXTMARKS, vec![name]).as_i64(), Some(0));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn the_highlight_clears_after_four_seconds() {
    let mut nvim = start("highlight-timer");
    let mine = nvim.numbered("mine.rs", 5);
    let other = nvim.numbered("other.rs", 20);
    nvim.edit(&mine);
    nvim.open(&other, Some(5), None, true);
    let name = Value::from("other.rs");
    assert_eq!(nvim.embed.lua(EXTMARKS, vec![name.clone()]).as_i64(), Some(1));
    std::thread::sleep(Duration::from_millis(4600));
    assert_eq!(nvim.embed.lua(EXTMARKS, vec![name]).as_i64(), Some(0));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn the_line_is_clamped_to_the_buffer() {
    let mut nvim = start("clamp");
    let mine = nvim.numbered("mine.rs", 5);
    let other = nvim.numbered("other.rs", 7);
    nvim.edit(&mine);
    let answer = nvim.open(&other, Some(9_000), Some(9_500), true);
    assert_eq!(line_of(&answer), 7);
    assert_eq!(nvim.marked()[0].2, 7);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn an_unsaved_new_file_is_shown() {
    let mut nvim = start("unsaved");
    let mine = nvim.numbered("mine.rs", 5);
    nvim.edit(&mine);
    let fresh = nvim.project.join("fresh.rs");
    assert!(!fresh.exists());
    nvim.embed.lua(
        "local buf = vim.api.nvim_create_buf(true, false)
         vim.api.nvim_buf_set_name(buf, ...)
         vim.api.nvim_buf_set_lines(buf, 0, -1, false, { 'a', 'b', 'c' })",
        vec![Value::from(fresh.to_str().unwrap())],
    );
    let answer = nvim.open(&fresh, Some(2), None, false);
    assert_eq!(place(&answer), "new_split");
    assert_eq!(line_of(&answer), 2);
    assert!(!fresh.exists(), "nothing was written");
    // A name nothing has, on disk or in the editor, is only asked to be loaded.
    let nothing = nvim.project.join("nothing.rs");
    assert!(says(&nvim.open(&nothing, Some(1), None, false), "needs_load"));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_follow_from_insert_mode_leaves_the_person_alone() {
    use eitri_core::editor_tools::follow::{FollowRun, FollowTarget};
    use eitri_core::editor_tools::memory::Memory;
    use eitri_core::editor_tools::step::{AgentCall, EditorReach, EditorSight, Step, StepEvent, ToolCx, ToolRun};
    use eitri_core::editor_tools::way::EditorCall;
    let mut nvim = start("follow-insert");
    let mine = nvim.numbered("mine.rs", 3);
    let target = nvim.numbered("target.rs", 8);
    nvim.edit(&mine);
    nvim.embed.input("ohello");
    let before = nvim.person();
    assert_eq!(find(&before, "mode"), Some(&Value::from("i")));
    let root = nvim.project.clone();
    let memory = Memory::default();
    let cx = ToolCx {
        root: &root,
        home: None,
        memory: &memory,
        trust: agent::setting_sources::ProjectTrust::Trusted,
        mode: agent::PermissionMode::Manual,
        reach: EditorReach::Own,
        sight: EditorSight::OnScreen,
        context: None,
        tab: 1,
        session: "s1",
        call: "follow",
        agent_call: AgentCall::Host,
    };
    let mut run = FollowRun::new(
        &root,
        FollowTarget {
            rel: "target.rs".into(),
            line: Some(5),
        },
    );
    let Step::Call {
        call:
            EditorCall::OpenFile {
                path,
                line,
                end_line,
                load,
            },
        ..
    } = run.step(StepEvent::Start, &cx)
    else {
        panic!("one open call")
    };
    assert_eq!(path, target);
    let answer = nvim.open(&path, line, end_line, load);
    assert!(says(&answer, "shown"), "{answer:?}");
    assert!(matches!(run.step(StepEvent::Answer(Ok(answer)), &cx), Step::Done(_)));
    assert_eq!(nvim.person(), before);
    let marked = nvim.marked();
    assert_eq!(marked.len(), 1);
    assert_eq!(Path::new(&marked[0].1), target);
    assert_eq!(marked[0].2, 5);
}
