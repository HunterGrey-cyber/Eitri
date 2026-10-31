//! `diagnostics`' chunk in a real `nvim`: what `vim.diagnostic.set` becomes in an answer, the order and the cap, one
//! buffer or every loaded one under the root, the unsaved ones named, UTF-16 characters, that it loads nothing, and
//! that an answer stops short of what the link between windows refuses; and the turn baseline taken from it and
//! subtracted from a later reading.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use agent::setting_sources::ProjectTrust;
use agent::PermissionMode;
use eitri_core::editor_context::feed::EditorContextFeed;
use eitri_core::editor_tools::memory::Memory;
use eitri_core::editor_tools::stamp::{wall_ms, Stamp};
use eitri_core::editor_tools::step::{
    encoded_len, AgentCall, EditorReach, EditorSight, Step, StepEvent, ToolCx, ToolRun,
};
use eitri_core::editor_tools::tools::baseline::{appeared, counted, BaselineRun, TurnBaseline, BASELINE_LIMIT};
use eitri_core::editor_tools::tools::diagnostics::{
    answer_max, parse, result_of, DiagnosticEntry, DiagnosticsAnswer, Severity, DIAGNOSTICS_LUA,
};
use embed_client::Embed;
use rmpv::Value;

const FAR: u64 = 60_000;

const ERROR: i64 = 1;
const WARN: i64 = 2;
const INFO: i64 = 3;
const HINT: i64 = 4;

/// Counts of what exists, across every tabpage.
const COUNTS: &str = "
local named = 0
for _, b in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_get_name(b) ~= '' then named = named + 1 end
end
return { bufs = #vim.api.nvim_list_bufs(), named = named, wins = #vim.api.nvim_list_wins() }";

/// Puts `list` on the buffer named `path`, as a language server would.
const SET: &str = "
local path, list = ...
for _, b in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_get_name(b) == path then
    vim.diagnostic.set(vim.api.nvim_create_namespace('test'), b, list)
  end
end";

/// As `SET`, in the namespace `name`: two producers, each setting its own.
const SET_IN: &str = "
local path, name, list = ...
for _, b in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_get_name(b) == path then
    vim.diagnostic.set(vim.api.nvim_create_namespace(name), b, list)
  end
end";

/// Puts `count` errors on the buffer named `path`, each message `unit` repeated `times`: built here, since sending
/// them would take as long as the answer. The display handlers stay off, as they say nothing of the answer.
const SET_MANY: &str = "
local path, count, unit, times = ...
local text = string.rep(unit, times)
local list = {}
for i = 1, count do
  list[i] = { lnum = 0, col = i, end_lnum = 0, end_col = i + 1, severity = 1, message = text }
end
for _, b in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_get_name(b) == path then
    vim.diagnostic.set(vim.api.nvim_create_namespace('test'), b, list,
      { underline = false, signs = false, virtual_text = false })
  end
end";

/// One diagnostic as `vim.diagnostic.set` takes it: 0-based line and column.
struct Diag {
    lnum: i64,
    col: i64,
    end_lnum: i64,
    end_col: i64,
    severity: i64,
    message: String,
}

fn diag(lnum: i64, col: i64, severity: i64, message: &str) -> Diag {
    Diag {
        lnum,
        col,
        end_lnum: lnum,
        end_col: col + 2,
        severity,
        message: message.to_owned(),
    }
}

impl Diag {
    fn value(&self) -> Value {
        Value::Map(vec![
            (Value::from("lnum"), Value::from(self.lnum)),
            (Value::from("col"), Value::from(self.col)),
            (Value::from("end_lnum"), Value::from(self.end_lnum)),
            (Value::from("end_col"), Value::from(self.end_col)),
            (Value::from("severity"), Value::from(self.severity)),
            (Value::from("message"), Value::from(self.message.as_str())),
            (Value::from("source"), Value::from("tester")),
            (Value::from("code"), Value::from("T1")),
        ])
    }
}

struct Nvim {
    embed: Embed,
    feed: EditorContextFeed,
    dir: PathBuf,
    project: PathBuf,
    beside: PathBuf,
    seq: u64,
}

impl Drop for Nvim {
    fn drop(&mut self) {
        self.feed.cleanup();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(case: &str) -> Nvim {
    let dir = std::env::temp_dir().join(format!("eto-diag-{}-{case}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("project")).expect("project dir");
    std::fs::create_dir_all(dir.join("beside")).expect("outside dir");
    let dir = std::fs::canonicalize(&dir).expect("scratch dir");
    let project = dir.join("project");
    let beside = dir.join("beside");
    let feed = EditorContextFeed::new().expect("the feed must bind");
    let embed = Embed::start_fed(&project, &feed, &[]);
    Nvim {
        embed,
        feed,
        dir,
        project,
        beside,
        seq: 0,
    }
}

fn bytes(path: &Path) -> Value {
    Value::Binary(path.as_os_str().as_bytes().to_vec())
}

impl Nvim {
    fn file(&self, directory: &Path, name: &str, content: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, content).expect("write a file");
        path
    }

    fn inside(&self, name: &str) -> PathBuf {
        self.file(&self.project, name, "one\ntwo\nthree\nfour\nfive\n")
    }

    fn edit(&mut self, path: &Path) {
        self.embed.command(&format!("edit {}", path.display()));
    }

    fn set(&mut self, path: &Path, diags: &[Diag]) {
        let list = Value::Array(diags.iter().map(Diag::value).collect());
        self.embed.lua(SET, vec![bytes(path), list]);
    }

    fn set_in(&mut self, path: &Path, namespace: &str, diags: &[Diag]) {
        let list = Value::Array(diags.iter().map(Diag::value).collect());
        self.embed.lua(SET_IN, vec![bytes(path), namespace.into(), list]);
    }

    /// What `vim.diagnostic` itself still holds on the current buffer, whatever the tool tells of it.
    fn standing(&mut self) -> Value {
        self.embed.lua("return #vim.diagnostic.get(0)", vec![])
    }

    fn stamp(&mut self) -> Stamp {
        self.seq += 1;
        Stamp {
            deadline_ms: wall_ms() + FAR,
            scope: "diag".to_owned(),
            seq: self.seq,
        }
    }

    fn ask_stamped(
        &mut self,
        stamp: &Stamp,
        path: Option<&Path>,
        min_severity: &str,
        utf16: bool,
        limit: u32,
    ) -> Value {
        let mut args = vec![
            (Value::from("root"), bytes(&self.project)),
            (Value::from("min_severity"), Value::from(min_severity)),
            (Value::from("utf16"), Value::from(utf16)),
            (Value::from("limit"), Value::from(limit)),
            (Value::from("answer_max"), Value::from(answer_max())),
        ];
        if let Some(path) = path {
            args.push((Value::from("path"), bytes(path)));
        }
        self.embed
            .lua(DIAGNOSTICS_LUA, vec![stamp.to_value(), Value::Map(args)])
    }

    /// Calls the chunk as the tool does and parses what comes back.
    fn answer(&mut self, path: Option<&Path>) -> DiagnosticsAnswer {
        self.answer_with(path, "hint", false, 500)
    }

    fn answer_with(&mut self, path: Option<&Path>, min_severity: &str, utf16: bool, limit: u32) -> DiagnosticsAnswer {
        let stamp = self.stamp();
        let raw = self.ask_stamped(&stamp, path, min_severity, utf16, limit);
        parse(&raw).unwrap_or_else(|why| panic!("{why}: {raw:?}"))
    }

    fn counts(&mut self) -> Value {
        self.embed.lua(COUNTS, vec![])
    }

    /// The reading `Baseline` asks for: everything, up to the baseline's limit.
    fn whole_reading(&mut self) -> DiagnosticsAnswer {
        self.answer_with(None, "hint", false, BASELINE_LIMIT)
    }

    /// What appeared since `before` was read.
    fn appeared_since(&mut self, before: &DiagnosticsAnswer) -> Vec<DiagnosticEntry> {
        let now = self.whole_reading();
        appeared(now.entries, &counted(&before.entries))
    }
}

/// What `value` comes to as the JSON of a reply to the other window: a map is a list of pairs under `map`, and a
/// string that is not UTF-8 a list of numbers under `str`.
fn on_link(value: &Value) -> usize {
    match value {
        Value::String(text) => match text.as_str() {
            Some(text) => encoded_len(text),
            None => {
                text.as_bytes()
                    .iter()
                    .map(|byte| byte.to_string().len() + 1)
                    .sum::<usize>()
                    + 10
            }
        },
        Value::Array(items) => items.iter().map(|item| on_link(item) + 1).sum::<usize>() + 2,
        Value::Map(pairs) => pairs.iter().map(|(k, v)| on_link(k) + on_link(v) + 4).sum::<usize>() + 10,
        other => other.to_string().len(),
    }
}

fn messages(answer: &DiagnosticsAnswer) -> Vec<&str> {
    answer.entries.iter().map(|entry| entry.message.as_str()).collect()
}

#[test]
#[ignore = "needs nvim on PATH"]
fn entries_after_vim_diagnostic_set() {
    let mut nvim = start("set");
    let file = nvim.inside("a.rs");
    nvim.edit(&file);
    nvim.set(&file, &[diag(2, 3, WARN, "unused variable")]);
    let answer = nvim.answer(None);
    assert_eq!(
        answer,
        DiagnosticsAnswer {
            entries: vec![DiagnosticEntry {
                path: file.clone(),
                read_from: Some(file.clone()),
                line: 3,
                column: 4,
                end_line: 3,
                end_column: 6,
                severity: Severity::Warning,
                source: Some("tester".to_owned()),
                code: Some("T1".to_owned()),
                message: "unused variable".to_owned(),
                start_character: None,
                end_character: None,
            }],
            modified: vec![],
            left_out: 0,
        }
    );
    assert_eq!(nvim.answer(Some(&file)), answer, "the same buffer asked for by name");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn the_most_severe_come_first_and_the_cap_counts_the_rest() {
    let mut nvim = start("order");
    let file = nvim.inside("a.rs");
    nvim.edit(&file);
    nvim.set(
        &file,
        &[
            diag(0, 0, HINT, "hint"),
            diag(1, 0, WARN, "warn late"),
            diag(0, 5, ERROR, "error"),
            diag(0, 1, WARN, "warn early"),
            diag(4, 0, INFO, "info"),
        ],
    );
    let all = nvim.answer(None);
    assert_eq!(messages(&all), ["error", "warn early", "warn late", "info", "hint"]);
    assert_eq!(all.left_out, 0);

    let capped = nvim.answer_with(None, "hint", false, 3);
    assert_eq!(messages(&capped), ["error", "warn early", "warn late"]);
    assert_eq!(capped.left_out, 2);

    let warnings = nvim.answer_with(None, "warning", false, 500);
    assert_eq!(messages(&warnings), ["error", "warn early", "warn late"]);
    assert_eq!(warnings.left_out, 0, "what the filter leaves is not left out");
    let errors = nvim.answer_with(None, "error", false, 500);
    assert_eq!(messages(&errors), ["error"]);
    let none = nvim.answer_with(None, "hint", false, 0);
    assert!(none.entries.is_empty());
    assert_eq!(none.left_out, 5);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn an_answer_stops_short_of_what_the_link_refuses_and_counts_the_rest() {
    let mut nvim = start("link-size");
    let file = nvim.inside("a.rs");
    nvim.edit(&file);
    // Under the entry limit and the message limit, and far past the link's cap as JSON: control bytes, bytes that are
    // not UTF-8, and control bytes in a valid string that has a byte above 0x7F.
    let mixed = format!("\u{e9}{}", "\u{1}".repeat(30));
    let units = [
        (Value::from("\u{1}"), 2048),
        (Value::Binary(vec![0xff]), 2048),
        (Value::from(mixed.as_str()), 2048 / mixed.len()),
    ];
    for (unit, times) in units {
        let list = vec![bytes(&file), Value::from(BASELINE_LIMIT), unit, Value::from(times)];
        nvim.embed.lua(SET_MANY, list);
        let stamp = nvim.stamp();
        let raw = nvim.ask_stamped(&stamp, None, "hint", false, BASELINE_LIMIT);
        let size = on_link(&raw);
        assert!(size <= answer_max(), "{size} bytes on the link");
        let answer = parse(&raw).unwrap();
        assert!(!answer.entries.is_empty());
        assert!(answer.left_out > 0, "a reading cut by size must say so");
        assert_eq!(answer.entries.len() as u32 + answer.left_out, BASELINE_LIMIT);
    }
}

#[test]
#[ignore = "needs nvim on PATH"]
fn an_answer_the_link_can_carry_is_not_cut_for_its_size() {
    let mut nvim = start("link-fits");
    let file = nvim.inside("a.rs");
    nvim.edit(&file);
    let list = vec![
        bytes(&file),
        Value::from(BASELINE_LIMIT),
        Value::from("x"),
        Value::from(2048),
    ];
    nvim.embed.lua(SET_MANY, list);
    let answer = nvim.answer_with(None, "hint", false, BASELINE_LIMIT);
    assert_eq!(answer.entries.len() as u32, BASELINE_LIMIT);
    assert_eq!(answer.left_out, 0);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn one_buffer_or_every_loaded_one_in_the_project() {
    let mut nvim = start("scope");
    let first = nvim.inside("a.rs");
    let second = nvim.inside("b.rs");
    let elsewhere = nvim.file(&nvim.beside.clone(), "c.rs", "x\n");
    for file in [&first, &second, &elsewhere] {
        nvim.edit(file);
        nvim.set(
            file,
            &[diag(
                0,
                0,
                ERROR,
                &format!("in {}", file.file_name().unwrap().to_str().unwrap()),
            )],
        );
    }
    let one = nvim.answer(Some(&first));
    assert_eq!(messages(&one), ["in a.rs"]);
    let every = nvim.answer(None);
    let mut seen = messages(&every);
    seen.sort_unstable();
    assert_eq!(seen, ["in a.rs", "in b.rs"], "the buffer outside the root is not asked");
    let outside = nvim.answer(Some(&elsewhere));
    assert_eq!(
        messages(&outside),
        ["in c.rs"],
        "a named buffer is answered; Rust judges whether it may be told"
    );
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_buffer_that_leaves_through_a_link_takes_no_place_under_the_cap() {
    let mut nvim = start("link-cap");
    let secret = nvim.file(&nvim.beside.clone(), "secret.rs", "x\ny\n");
    let link = nvim.project.join("link.rs");
    std::os::unix::fs::symlink(&secret, &link).expect("a link");
    let real = nvim.inside("real.rs");
    nvim.edit(&link);
    nvim.set(
        &link,
        &[diag(0, 0, ERROR, "outside one"), diag(1, 0, ERROR, "outside two")],
    );
    nvim.edit(&real);
    nvim.set(&real, &[diag(0, 0, WARN, "the project's")]);

    let answer = nvim.answer_with(None, "hint", false, 2);
    assert_eq!(messages(&answer), ["the project's"]);
    assert_eq!(answer.left_out, 0, "nothing of the project was left out");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_new_file_under_a_directory_that_became_a_link_out_takes_no_place_under_the_cap() {
    let mut nvim = start("new-under-link");
    let sub = nvim.project.join("sub");
    std::fs::create_dir(&sub).expect("a directory");
    let stranded = sub.join("new.rs");
    let fresh = nvim.project.join("fresh.rs");
    nvim.edit(&stranded);
    nvim.set(
        &stranded,
        &[diag(0, 0, ERROR, "outside one"), diag(0, 1, ERROR, "outside two")],
    );
    nvim.edit(&fresh);
    nvim.set(&fresh, &[diag(0, 0, WARN, "the project's unsaved file")]);
    std::fs::remove_dir(&sub).expect("the directory goes");
    std::os::unix::fs::symlink(&nvim.beside, &sub).expect("a link");

    let answer = nvim.answer_with(None, "hint", false, 2);
    assert_eq!(messages(&answer), ["the project's unsaved file"]);
    assert_eq!(answer.left_out, 0, "nothing of the project was left out");
}

/// A buffer of the project opened through a link to a file outside it, the link since replaced by an ordinary file.
fn read_from_outside(nvim: &mut Nvim, name: &str) -> PathBuf {
    let secret = nvim.file(&nvim.beside.clone(), "secret.rs", "secret\n");
    let link = nvim.project.join(name);
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    nvim.edit(&link);
    std::fs::remove_file(&link).unwrap();
    std::fs::write(&link, "plain\n").unwrap();
    link
}

#[test]
#[ignore = "needs nvim on PATH"]
fn diagnostics_of_a_buffer_read_from_outside_are_dropped() {
    let mut nvim = start("read-elsewhere");
    let own = nvim.inside("own.rs");
    nvim.edit(&own);
    nvim.set(&own, &[diag(0, 0, WARN, "the project's own")]);
    let linked = read_from_outside(&mut nvim, "linked.rs");
    nvim.set(&linked, &[diag(0, 0, ERROR, "from the secret file")]);

    let answer = nvim.answer_with(None, "hint", false, 1);
    assert_eq!(
        messages(&answer),
        ["the project's own"],
        "it takes no place under the cap"
    );
    assert_eq!(answer.left_out, 0, "and it is not counted");

    let stamp = nvim.stamp();
    let value = nvim.ask_stamped(&stamp, Some(&linked), "hint", false, 500);
    let memory = Memory::default();
    let cx = ToolCx {
        mode: PermissionMode::Manual,
        trust: ProjectTrust::Trusted,
        root: &nvim.project,
        home: None,
        reach: EditorReach::Own,
        sight: EditorSight::OnScreen,
        context: None,
        tab: 1,
        session: "session",
        call: "call",
        agent_call: AgentCall::Host,
        memory: &memory,
    };
    let told = result_of(&cx, &value);
    assert!(!told.is_error, "{}", told.text);
    let told: serde_json::Value = serde_json::from_str(&told.text).unwrap();
    assert_eq!(
        told["entries"],
        serde_json::json!([]),
        "asked for by name, the entry stays out"
    );
}

/// A project buffer opened through a link to an outside file, with an entry set on that file's text, the link since
/// replaced by an ordinary file. The buffer still holds the outside text; nothing has reloaded it yet.
fn set_on_a_link_then_replaced(nvim: &mut Nvim, name: &str, namespace: &str) -> PathBuf {
    let secret = nvim.file(&nvim.beside.clone(), "secret.rs", "secret\n");
    let link = nvim.project.join(name);
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    nvim.edit(&link);
    nvim.set_in(&link, namespace, &[diag(0, 0, ERROR, "unused: secret words")]);
    std::fs::remove_file(&link).unwrap();
    std::fs::write(&link, "plain\n").unwrap();
    link
}

/// Both ways a file comes back into a buffer, and `checktime` is what the reload timer runs every second.
#[test]
#[ignore = "needs nvim on PATH"]
fn an_entry_standing_through_a_reload_is_not_told() {
    for (case, reload) in [("reload-edit", "edit!"), ("reload-checktime", "checktime")] {
        let mut nvim = start(case);
        let own = nvim.inside("own.rs");
        nvim.edit(&own);
        nvim.set(&own, &[diag(0, 0, WARN, "the project's own")]);
        let linked = set_on_a_link_then_replaced(&mut nvim, "linked.rs", "test");
        nvim.embed.command(reload);

        let line = nvim.embed.lua("return vim.api.nvim_get_current_line()", vec![]);
        assert_eq!(line.as_str(), Some("plain"), "{reload} read the new file");
        let record = nvim.embed.lua("return vim.b.eitri_read_from", vec![]);
        assert_eq!(record.as_str(), linked.to_str(), "{reload} moved the record");
        assert_eq!(
            nvim.standing(),
            Value::from(1),
            "vim.diagnostic keeps it through {reload}"
        );

        let whole = nvim.answer_with(None, "hint", false, 1);
        assert_eq!(messages(&whole), ["the project's own"], "{reload}: it takes no place");
        assert_eq!(whole.left_out, 0, "{reload}: and it is not counted");
        let named = nvim.answer(Some(&linked));
        assert!(messages(&named).is_empty(), "{reload}: asked for by name");
    }
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_modified_buffer_with_only_stale_entries_is_not_named() {
    let mut nvim = start("stale-modified");
    let linked = set_on_a_link_then_replaced(&mut nvim, "linked.rs", "test");
    nvim.embed.command("edit!");
    nvim.embed.command("normal! ix");
    let answer = nvim.answer(None);
    assert!(answer.modified.is_empty(), "{:?}", answer.modified);
    assert!(messages(&nvim.answer(Some(&linked))).is_empty());
}

/// A deleted link leaves nothing to read, and the text is gone from the buffer all the same.
#[test]
#[ignore = "needs nvim on PATH"]
fn an_entry_standing_through_a_reload_of_a_deleted_link_is_not_told() {
    let mut nvim = start("reload-deleted");
    let linked = set_on_a_link_then_replaced(&mut nvim, "linked.rs", "test");
    std::fs::remove_file(&linked).unwrap();
    nvim.embed.command("edit!");
    let record = nvim.embed.lua("return vim.b.eitri_read_from", vec![]);
    assert_eq!(record.as_str(), linked.to_str(), "the record is the name");
    assert_eq!(nvim.standing(), Value::from(1));
    assert!(messages(&nvim.answer(Some(&linked))).is_empty());
}

/// `:bdelete` drops the buffer's variables and leaves what `vim.diagnostic` holds, so a record that is not there has
/// moved as much as one that differs.
#[test]
#[ignore = "needs nvim on PATH"]
fn an_entry_standing_through_a_deleted_buffer_is_not_told() {
    let mut nvim = start("reload-bdelete");
    let linked = set_on_a_link_then_replaced(&mut nvim, "linked.rs", "test");
    nvim.embed.command("bdelete!");
    nvim.edit(&linked);
    assert_eq!(
        nvim.standing(),
        Value::from(1),
        "the entry outlives the buffer's variables"
    );
    assert!(messages(&nvim.answer(Some(&linked))).is_empty());
}

/// Each producer's entries come back when that producer sets again, and no sooner.
#[test]
#[ignore = "needs nvim on PATH"]
fn a_producer_that_sets_again_is_told_again() {
    let mut nvim = start("sets-again");
    let linked = set_on_a_link_then_replaced(&mut nvim, "linked.rs", "first");
    nvim.set_in(&linked, "second", &[diag(0, 0, WARN, "unused: more secret words")]);
    nvim.embed.command("edit!");
    assert_eq!(nvim.standing(), Value::from(2));
    assert!(messages(&nvim.answer(Some(&linked))).is_empty());

    nvim.set_in(&linked, "first", &[]);
    assert!(
        messages(&nvim.answer(Some(&linked))).is_empty(),
        "emptying one says nothing of the other"
    );
    nvim.set_in(&linked, "first", &[diag(0, 0, ERROR, "about the plain text")]);
    assert_eq!(messages(&nvim.answer(Some(&linked))), ["about the plain text"]);
    nvim.set_in(&linked, "second", &[diag(0, 0, WARN, "also about it")]);
    let answer = nvim.answer(Some(&linked));
    assert_eq!(messages(&answer), ["about the plain text", "also about it"]);
    let note = nvim.embed.lua("return vim.b.eitri_diagnostics_stood", vec![]);
    assert_eq!(note, Value::Nil, "nothing is left to hold back");
}

/// The record stays where it was, so the text is the file's as before and so are the entries.
#[test]
#[ignore = "needs nvim on PATH"]
fn a_reload_of_the_same_file_keeps_its_entries() {
    let mut nvim = start("same-file");
    let own = nvim.inside("own.rs");
    nvim.edit(&own);
    nvim.set(&own, &[diag(0, 0, WARN, "the project's own")]);
    std::fs::write(&own, "changed on disk\n").unwrap();
    nvim.embed.command("edit!");
    assert_eq!(messages(&nvim.answer(None)), ["the project's own"]);
}

/// Writing replaces no text: the entries are about what the file now holds.
#[test]
#[ignore = "needs nvim on PATH"]
fn a_write_that_moves_the_record_keeps_the_entries() {
    let mut nvim = start("write-keeps");
    let linked = set_on_a_link_then_replaced(&mut nvim, "linked.rs", "test");
    nvim.embed.command("silent write!");
    let record = nvim.embed.lua("return vim.b.eitri_read_from", vec![]);
    assert_eq!(record.as_str(), linked.to_str());
    assert_eq!(messages(&nvim.answer(Some(&linked))), ["unused: secret words"]);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_buffer_with_no_record_takes_no_place_under_the_cap() {
    let mut nvim = start("no-record");
    let own = nvim.inside("own.rs");
    nvim.edit(&own);
    nvim.set(&own, &[diag(0, 0, WARN, "the project's own")]);
    let other = nvim.inside("other.rs");
    nvim.edit(&other);
    nvim.embed.command("unlet b:eitri_read_from");
    nvim.set(&other, &[diag(0, 0, ERROR, "no record")]);
    assert_eq!(
        messages(&nvim.answer_with(None, "hint", false, 1)),
        ["the project's own"]
    );
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_modified_buffer_is_named() {
    let mut nvim = start("modified");
    let saved = nvim.inside("saved.rs");
    let edited = nvim.inside("edited.rs");
    for file in [&saved, &edited] {
        nvim.edit(file);
        nvim.set(file, &[diag(0, 0, ERROR, "something")]);
    }
    assert_eq!(nvim.answer(None).modified, Vec::<PathBuf>::new());
    nvim.embed
        .lua("vim.api.nvim_buf_set_lines(0, 0, 1, false, { 'changed' })", vec![]);
    let answer = nvim.answer(None);
    assert_eq!(answer.modified, std::slice::from_ref(&edited));
    assert_eq!(answer.entries.len(), 2);
    assert_eq!(nvim.answer(Some(&saved)).modified, Vec::<PathBuf>::new());
    assert_eq!(nvim.answer(Some(&edited)).modified, [edited]);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn it_loads_nothing() {
    let mut nvim = start("loads-nothing");
    let opened = nvim.inside("opened.rs");
    let unopened = nvim.inside("unopened.rs");
    nvim.edit(&opened);
    nvim.set(&opened, &[diag(0, 0, ERROR, "here")]);
    let before = nvim.counts();
    let answer = nvim.answer(Some(&unopened));
    assert!(answer.entries.is_empty());
    assert_eq!(answer.left_out, 0);
    assert_eq!(messages(&nvim.answer(None)), ["here"]);
    assert_eq!(nvim.counts(), before, "no buffer was added");

    // Listed but not loaded is still not read.
    nvim.edit(&unopened);
    nvim.set(&unopened, &[diag(0, 0, ERROR, "gone")]);
    nvim.embed.command("enew");
    nvim.embed.command(&format!("bunload {}", unopened.display()));
    let after_unload = nvim.counts();
    assert!(nvim.answer(Some(&unopened)).entries.is_empty());
    assert_eq!(messages(&nvim.answer(None)), ["here"]);
    assert_eq!(nvim.counts(), after_unload);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn utf16_counts_units_not_bytes() {
    let mut nvim = start("utf16");
    let file = nvim.file(&nvim.project.clone(), "a.txt", "x\n");
    nvim.edit(&file);
    // U+1F600 is four bytes and two UTF-16 units; 'é' is two bytes and one unit.
    nvim.embed.lua(
        "vim.api.nvim_buf_set_lines(0, 0, -1, false, { '\u{1F600}é = 1' })",
        vec![],
    );
    let mut wide = diag(0, 6, ERROR, "after both");
    wide.end_col = 9;
    nvim.set(&file, &[wide]);

    let plain = nvim.answer(Some(&file));
    assert_eq!(plain.entries[0].start_character, None);
    assert_eq!(plain.entries[0].column, 7, "the column stays in bytes");

    let units = nvim.answer_with(Some(&file), "hint", true, 500);
    let entry = &units.entries[0];
    assert_eq!(entry.column, 7);
    assert_eq!(entry.start_character, Some(3), "two units and one more");
    assert_eq!(entry.end_character, Some(6));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn an_expired_stamp_reads_nothing() {
    let mut nvim = start("stamp");
    let file = nvim.inside("a.rs");
    nvim.edit(&file);
    nvim.set(&file, &[diag(0, 0, ERROR, "here")]);
    let newer = nvim.stamp();
    let older = Stamp {
        seq: newer.seq - 1,
        ..newer.clone()
    };
    let fresh = nvim.ask_stamped(&newer, None, "hint", false, 500);
    assert!(parse(&fresh).is_ok(), "{fresh:?}");
    for stale in [older, newer.clone()] {
        let answer = nvim.ask_stamped(&stale, None, "hint", false, 500);
        assert!(parse(&answer).is_err(), "{answer:?}");
        let Value::Map(fields) = &answer else {
            panic!("a table: {answer:?}")
        };
        assert!(fields.iter().any(|(key, _)| key.as_str() == Some("expired")));
        assert!(!fields.iter().any(|(key, _)| key.as_str() == Some("entries")));
    }
    let late = Stamp {
        deadline_ms: wall_ms().saturating_sub(1),
        seq: newer.seq + 10,
        ..newer
    };
    assert!(parse(&nvim.ask_stamped(&late, None, "hint", false, 500)).is_err());
}

#[test]
#[ignore = "needs nvim on PATH"]
fn the_baseline_counts_what_is_there() {
    let mut nvim = start("baseline-counts");
    let first = nvim.inside("a.rs");
    let second = nvim.inside("b.rs");
    nvim.edit(&first);
    nvim.set(
        &first,
        &[
            diag(0, 0, ERROR, "same"),
            diag(2, 0, ERROR, "same"),
            diag(3, 0, WARN, "other"),
        ],
    );
    nvim.edit(&second);
    nvim.set(&second, &[diag(0, 0, ERROR, "same")]);
    let baseline = nvim.whole_reading();
    assert_eq!(baseline.left_out, 0);
    assert_eq!(baseline.entries.len(), 4);
    let counts = counted(&baseline.entries);
    assert_eq!(counts.len(), 3, "the file, severity and text make a key");
    assert_eq!(counts.values().copied().sum::<u32>(), 4);
    assert_eq!(counts.values().copied().max(), Some(2));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn since_turn_answers_only_what_appeared() {
    let mut nvim = start("baseline-appeared");
    let file = nvim.inside("a.rs");
    nvim.edit(&file);
    nvim.set(&file, &[diag(0, 0, ERROR, "before")]);
    let baseline = nvim.whole_reading();
    nvim.set(&file, &[diag(0, 0, ERROR, "before"), diag(1, 0, WARN, "after")]);
    let new = nvim.appeared_since(&baseline);
    assert_eq!(
        new.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(),
        ["after"]
    );
    assert_eq!(new[0].line, 2);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn an_entry_that_moved_lines_is_not_new() {
    let mut nvim = start("baseline-moved");
    let file = nvim.inside("a.rs");
    nvim.edit(&file);
    nvim.set(&file, &[diag(1, 0, ERROR, "moves")]);
    let baseline = nvim.whole_reading();
    nvim.set(&file, &[diag(4, 3, ERROR, "moves")]);
    assert!(nvim.appeared_since(&baseline).is_empty());
}

#[test]
#[ignore = "needs nvim on PATH"]
fn two_equal_entries_and_a_third_gives_one() {
    let mut nvim = start("baseline-third");
    let file = nvim.inside("a.rs");
    nvim.edit(&file);
    nvim.set(&file, &[diag(0, 0, ERROR, "same"), diag(1, 0, ERROR, "same")]);
    let baseline = nvim.whole_reading();
    nvim.set(
        &file,
        &[
            diag(0, 0, ERROR, "same"),
            diag(1, 0, ERROR, "same"),
            diag(3, 0, ERROR, "same"),
        ],
    );
    let new = nvim.appeared_since(&baseline);
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].line, 4, "the last in the order of the reading");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_gone_entry_is_not_reported() {
    let mut nvim = start("baseline-gone");
    let file = nvim.inside("a.rs");
    nvim.edit(&file);
    nvim.set(&file, &[diag(0, 0, ERROR, "fixed"), diag(1, 0, WARN, "stays")]);
    let baseline = nvim.whole_reading();
    nvim.set(&file, &[diag(1, 0, WARN, "stays")]);
    assert!(nvim.appeared_since(&baseline).is_empty());
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_baseline_past_its_deadline_reads_nothing() {
    let mut nvim = start("baseline-late");
    let file = nvim.inside("a.rs");
    nvim.edit(&file);
    nvim.set(&file, &[diag(0, 0, ERROR, "here")]);
    let late = Stamp {
        deadline_ms: wall_ms().saturating_sub(1),
        scope: "late".to_owned(),
        seq: 1,
    };
    let raw = nvim.ask_stamped(&late, None, "hint", false, BASELINE_LIMIT);

    let memory = Memory::default();
    memory.with(3, |tab| {
        tab.baseline = Some(TurnBaseline::Taking { turn: "t1".to_owned() })
    });
    let cx = ToolCx {
        mode: PermissionMode::Manual,
        trust: ProjectTrust::Trusted,
        root: &nvim.project,
        home: None,
        reach: EditorReach::Own,
        sight: EditorSight::OnScreen,
        context: None,
        tab: 3,
        session: "session",
        call: "call",
        agent_call: AgentCall::Host,
        memory: &memory,
    };
    let mut run = BaselineRun::new("t1");
    assert!(matches!(run.step(StepEvent::Start, &cx), Step::Call { .. }));
    run.step(StepEvent::Answer(Ok(raw)), &cx);
    assert_eq!(
        memory.with(3, |tab| tab.baseline.clone()),
        Some(TurnBaseline::Failed { turn: "t1".to_owned() }),
        "an answer that only says it expired is no empty baseline"
    );
}
