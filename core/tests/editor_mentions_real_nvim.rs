//! The sources behind `@buffers`, `@quickfix` and `@diagnostics` in a real `nvim`: what the chunks read of a buffer
//! with unsaved changes, a quickfix list and `vim.diagnostic`, and the text the host makes of them, with a file kept
//! out by where it is or by a deny rule.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use agent::read_deny::ReadRules;
use eitri_core::editor_context::feed::EditorContextFeed;
use eitri_core::editor_tools::stamp::{wall_ms, Stamp};
use eitri_core::editor_tools::tools::diagnostics::{self, DIAGNOSTICS_LUA};
use eitri_core::editor_tools::tools::mention_reads::{
    self, BUFFERS_LISTED_MAX, BUFFERS_LUA, BUFFERS_TOTAL_MAX, BUFFER_BYTES_MAX, BUFFER_LINES_MAX, QUICKFIX_LUA,
    QUICKFIX_SCAN_MAX,
};
use eitri_core::mentions::blocks::{self, Scope};
use embed_client::Embed;
use rmpv::Value;

const FAR: u64 = 60_000;

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
    let dir = std::env::temp_dir().join(format!("eto-mention-{}-{case}", std::process::id()));
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

fn path_arg(path: &Path) -> Value {
    Value::Binary(path.as_os_str().as_bytes().to_vec())
}

impl Nvim {
    fn file(&self, dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("write a file");
        path
    }

    fn open(&mut self, path: &Path) {
        self.embed.command(&format!("edit {}", path.display()));
    }

    fn stamp(&mut self) -> Value {
        self.seq += 1;
        Stamp {
            deadline_ms: wall_ms() + FAR,
            scope: "mention".to_owned(),
            seq: self.seq,
        }
        .to_value()
    }

    fn buffers(&mut self) -> mention_reads::BuffersAnswer {
        let args = vec![
            (Value::from("root"), path_arg(&self.project)),
            (Value::from("listed_max"), Value::from(BUFFERS_LISTED_MAX)),
            (Value::from("lines_max"), Value::from(BUFFER_LINES_MAX)),
            (Value::from("bytes_max"), Value::from(BUFFER_BYTES_MAX)),
            (Value::from("total_max"), Value::from(BUFFERS_TOTAL_MAX)),
        ];
        let stamp = self.stamp();
        let reply = self.embed.lua(BUFFERS_LUA, vec![stamp, Value::Map(args)]);
        mention_reads::parse_buffers(&reply).unwrap_or_else(|why| panic!("{why}: {reply:?}"))
    }

    fn quickfix(&mut self) -> mention_reads::QuickfixAnswer {
        let args = vec![(Value::from("scan_max"), Value::from(QUICKFIX_SCAN_MAX))];
        let stamp = self.stamp();
        let reply = self.embed.lua(QUICKFIX_LUA, vec![stamp, Value::Map(args)]);
        mention_reads::parse_quickfix(&reply).unwrap_or_else(|why| panic!("{why}: {reply:?}"))
    }

    fn diagnostics(&mut self) -> diagnostics::DiagnosticsAnswer {
        let args = vec![
            (Value::from("root"), path_arg(&self.project)),
            (Value::from("min_severity"), Value::from("hint")),
            (Value::from("utf16"), Value::from(false)),
            (Value::from("limit"), Value::from(2000)),
            (Value::from("answer_max"), Value::from(diagnostics::answer_max())),
        ];
        let stamp = self.stamp();
        let reply = self.embed.lua(DIAGNOSTICS_LUA, vec![stamp, Value::Map(args)]);
        diagnostics::parse(&reply).unwrap_or_else(|why| panic!("{why}: {reply:?}"))
    }
}

fn scope_of<'a>(root: &'a Path, rules: &'a ReadRules) -> Scope<'a> {
    Scope::new(root, None, rules)
}

fn denying(rule: &str, root: &Path) -> ReadRules {
    let mut rules = ReadRules::default();
    rules.add_settings(
        format!(r#"{{"permissions":{{"deny":["{rule}"]}}}}"#).as_bytes(),
        root,
        None,
        Path::new("settings.json"),
    );
    rules
}

#[test]
#[ignore]
fn an_unsaved_buffer_is_told_as_it_is_on_screen_and_a_file_outside_is_only_counted() {
    let mut nvim = start("buffers");
    let a = nvim.file(&nvim.project.clone(), "a.rs", "fn on_disk() {}\n");
    let b = nvim.file(&nvim.project.clone(), "b.rs", "fn b() {}\n");
    let outside = nvim.file(&nvim.beside.clone(), "o.rs", "OUTSIDE-TEXT\n");
    nvim.open(&b);
    nvim.open(&outside);
    nvim.open(&a);
    nvim.embed.lua(
        "vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'fn on_screen() {}', '@/home/me/.ssh/id_rsa' })",
        vec![],
    );
    let answer = nvim.buffers();
    assert_eq!(answer.other, 1, "the file outside the root is counted");
    assert!(answer
        .buffers
        .iter()
        .all(|buffer| buffer.path.starts_with(&nvim.project)));
    let first = &answer.buffers[0];
    assert_eq!(first.path, a, "the current buffer leads");
    assert!(first.modified && first.current && first.loaded);
    assert_eq!(
        first.text,
        ["fn on_screen() {}", "@/home/me/.ssh/id_rsa"],
        "the text on screen, not the disk's"
    );

    let rules = ReadRules::default();
    let section = blocks::buffers(&answer, &scope_of(&nvim.project, &rules));
    assert!(section.contains("1 with unsaved changes"), "{section}");
    assert!(section.contains("--- a.rs (unsaved changes, current, 2 lines) ---\n1\tfn on_screen() {}\n2\t\u{2060}@/home/me/.ssh/id_rsa\n"), "{section}");
    assert!(section.contains("--- b.rs (1 line) ---\n1\tfn b() {}\n"), "{section}");
    assert!(
        !section.contains("OUTSIDE-TEXT") && !section.contains("o.rs"),
        "{section}"
    );
    assert!(!section.contains("on_disk"), "{section}");
}

#[test]
#[ignore]
fn a_denied_file_open_in_the_editor_is_never_named() {
    let mut nvim = start("deny");
    let a = nvim.file(&nvim.project.clone(), "a.rs", "ok\n");
    let secret = nvim.file(&nvim.project.clone(), "secret.env", "KEY=SECRET-VALUE\n");
    nvim.open(&secret);
    nvim.open(&a);
    nvim.embed.lua(
        "local path = ...
         for _, b in ipairs(vim.api.nvim_list_bufs()) do
           if vim.api.nvim_buf_get_name(b) == path then
             vim.diagnostic.set(vim.api.nvim_create_namespace('t'), b, { { lnum = 0, col = 0, end_lnum = 0, end_col = 3, severity = 1, message = 'secret problem' } })
           end
         end
         vim.fn.setqflist({ { filename = path, lnum = 1, text = 'secret entry' } })",
        vec![path_arg(&secret)],
    );
    let rules = denying("Read(**/*.env)", &nvim.project);
    let root = nvim.project.clone();
    let scope = scope_of(&root, &rules);
    for section in [
        blocks::buffers(&nvim.buffers(), &scope),
        blocks::quickfix(&nvim.quickfix(), &scope),
        blocks::diagnostics(&nvim.diagnostics(), &scope),
    ] {
        for forbidden in ["secret.env", "SECRET-VALUE", "secret problem", "secret entry"] {
            assert!(!section.contains(forbidden), "{forbidden}: {section}");
        }
        assert!(
            section.contains("1 covered by a Read deny rule")
                || section.contains("no diagnostics")
                || section.contains("empty"),
            "{section}"
        );
    }
}

#[test]
#[ignore]
fn a_quickfix_list_and_the_language_servers_words_are_told_with_file_and_line() {
    let mut nvim = start("lists");
    let a = nvim.file(&nvim.project.clone(), "a.rs", "one\ntwo\nthree\n");
    nvim.open(&a);
    nvim.embed.lua(
        "local path = ...
         vim.diagnostic.set(vim.api.nvim_create_namespace('t'), 0, {
           { lnum = 1, col = 2, end_lnum = 1, end_col = 5, severity = 2, message = 'unused variable', source = 'rustc', code = 'W1' },
           { lnum = 0, col = 0, end_lnum = 0, end_col = 3, severity = 1, message = 'mismatched types', source = 'rust-analyzer', code = 'E0308' },
         })
         vim.fn.setqflist({}, ' ', { title = 'make', items = {
           { filename = path, lnum = 3, col = 1, text = 'boom', type = 'E' },
           { text = 'make: *** Error 1' },
         } })",
        vec![path_arg(&a)],
    );
    let rules = ReadRules::default();
    let root = nvim.project.clone();
    let scope = scope_of(&root, &rules);
    let quickfix = blocks::quickfix(&nvim.quickfix(), &scope);
    assert!(quickfix.contains("quickfix list \"make\""), "{quickfix}");
    assert!(
        quickfix.contains("\n  a.rs:3:1 error boom\n  make: *** Error 1\n"),
        "{quickfix}"
    );
    let diagnostics = blocks::diagnostics(&nvim.diagnostics(), &scope);
    assert!(
        diagnostics.contains(
            "  a.rs:1:1 error [rust-analyzer E0308]: mismatched types\n  a.rs:2:3 warning [rustc W1]: unused variable\n"
        ),
        "{diagnostics}"
    );
    assert!(diagnostics.contains("2 diagnostics, 2 below"), "{diagnostics}");
}

#[test]
#[ignore]
fn a_long_buffer_and_a_thousand_diagnostics_stop_at_their_bounds_and_say_so() {
    let mut nvim = start("bounds");
    let long = nvim.file(
        &nvim.project.clone(),
        "long.txt",
        &(1..=3000).map(|n| format!("line {n}\n")).collect::<String>(),
    );
    nvim.open(&long);
    nvim.embed.lua(
        "local list = {}
         for i = 1, 1200 do
           list[i] = { lnum = i % 100, col = 0, end_lnum = i % 100, end_col = 1, severity = 4, message = 'hint ' .. i }
         end
         vim.diagnostic.set(vim.api.nvim_create_namespace('t'), 0, list, { underline = false, signs = false, virtual_text = false })",
        vec![],
    );
    let rules = ReadRules::default();
    let root = nvim.project.clone();
    let scope = scope_of(&root, &rules);
    let buffers = nvim.buffers();
    assert_eq!(
        buffers.buffers[0].text.len() as u32,
        BUFFER_LINES_MAX,
        "the line bound, exactly"
    );
    assert!(buffers.buffers[0].cut.is_some(), "3000 lines do not fit");
    let section = blocks::buffers(&buffers, &scope);
    assert!(
        section.contains("not attached: the bound of 1000 lines or 32 KiB a file)"),
        "{section}"
    );
    let listed = nvim.diagnostics();
    let section = blocks::diagnostics(&listed, &scope);
    assert!(section.contains("1200 diagnostics, 200 below"), "{section}");
    assert!(
        section.contains("1000 more, the least severe, not attached: the bound is 200 entries or 48 KiB."),
        "{section}"
    );
    assert_eq!(section.matches("long.txt:").count(), 200, "{section}");
}

#[test]
#[ignore]
fn a_loaded_buffer_with_no_record_and_a_wiped_quickfix_buffer_keep_their_text_out() {
    let mut nvim = start("records");
    let a = nvim.file(&nvim.project.clone(), "a.rs", "ok\n");
    let outside = nvim.file(&nvim.beside.clone(), "o.rs", "OUTSIDE-TEXT\n");
    nvim.open(&outside);
    // Renamed to a file of the project and its record forgotten, as a buffer loaded before the feed was there is.
    nvim.embed.lua(
        "vim.api.nvim_buf_set_name(0, ...) vim.b.eitri_read_from = nil
         vim.fn.setqflist({ { filename = ..., lnum = 1, text = 'OUTSIDE-ENTRY' } })
         vim.cmd('bwipeout!')",
        vec![path_arg(&a)],
    );
    let answer = nvim.buffers();
    assert!(
        answer
            .buffers
            .iter()
            .all(|buffer| buffer.read_from.is_some() || !buffer.loaded),
        "a loaded buffer with no record has none: {answer:?}"
    );
    let rules = ReadRules::default();
    let root = nvim.project.clone();
    let scope = scope_of(&root, &rules);
    let section = blocks::buffers(&answer, &scope);
    assert!(!section.contains("OUTSIDE-TEXT"), "{section}");
    let section = blocks::quickfix(&nvim.quickfix(), &scope);
    assert!(
        !section.contains("OUTSIDE-ENTRY"),
        "an entry whose buffer was wiped: {section}"
    );
}

/// What the told text of a buffer comes to in bytes, as the host counts it: the number, a tab, the line, a line break.
fn told_bytes(buffer: &mention_reads::BufferRead) -> usize {
    buffer
        .text
        .iter()
        .enumerate()
        .map(|(n, line)| format!("{}\t{line}\n", n + 1).len())
        .sum()
}

#[test]
#[ignore]
fn a_buffer_past_its_byte_bound_stops_there_and_says_so() {
    let mut nvim = start("bytes");
    let wide = nvim.file(
        &nvim.project.clone(),
        "wide.txt",
        &(1..=100)
            .map(|n| format!("{n:03} {}\n", "w".repeat(600)))
            .collect::<String>(),
    );
    nvim.open(&wide);
    let answer = nvim.buffers();
    let first = &answer.buffers[0];
    assert!(
        first.text.len() < 100,
        "60 KiB do not fit in 32 KiB: {} lines",
        first.text.len()
    );
    assert_eq!(first.cut, Some(first.text.len() as u32));
    assert!(!first.budget, "its own bound, not the total");
    assert!(told_bytes(first) <= BUFFER_BYTES_MAX as usize, "{}", told_bytes(first));
    assert!(
        told_bytes(first) > BUFFER_BYTES_MAX as usize - 2048,
        "and not much less"
    );
    let rules = ReadRules::default();
    let root = nvim.project.clone();
    let section = blocks::buffers(&answer, &scope_of(&root, &rules));
    assert!(
        section.contains("the bound of 1000 lines or 32 KiB a file)"),
        "{section}"
    );
    assert!(section.len() < BUFFER_BYTES_MAX as usize + 2048, "{}", section.len());
}

#[test]
#[ignore]
fn buffers_together_stop_at_the_total_and_the_last_say_so() {
    let mut nvim = start("total");
    for n in 0..6 {
        let file = nvim.file(
            &nvim.project.clone(),
            &format!("b{n}.txt"),
            &(1..=60)
                .map(|l| format!("{l:02} {}\n", "x".repeat(500)))
                .collect::<String>(),
        );
        nvim.open(&file);
    }
    let answer = nvim.buffers();
    assert_eq!(answer.buffers.len(), 6, "all are listed");
    let told: usize = answer.buffers.iter().map(told_bytes).sum();
    assert!(told <= BUFFERS_TOTAL_MAX as usize, "{told}");
    assert!(told > BUFFERS_TOTAL_MAX as usize - 4096, "{told}: the budget was spent");
    assert!(
        answer.buffers.iter().any(|buffer| buffer.budget),
        "one stopped at the total"
    );
    assert!(
        answer.buffers.last().unwrap().text.is_empty(),
        "the last had nothing left"
    );
    let rules = ReadRules::default();
    let root = nvim.project.clone();
    let section = blocks::buffers(&answer, &scope_of(&root, &rules));
    assert!(
        section.contains("the 96 KiB bound on all files together was reached"),
        "{section}"
    );
    assert!(section.len() < BUFFERS_TOTAL_MAX as usize + 8192, "{}", section.len());
}

#[test]
#[ignore]
fn a_hundred_and_five_open_files_are_a_hundred_and_five_counted() {
    let mut nvim = start("hundred");
    for n in 0..105 {
        let file = nvim.file(&nvim.project.clone(), &format!("f{n:03}.txt"), "x\n");
        nvim.open(&file);
    }
    let answer = nvim.buffers();
    assert_eq!(answer.buffers.len(), BUFFERS_LISTED_MAX as usize);
    assert_eq!(answer.left_out, 5);
    let rules = ReadRules::default();
    let root = nvim.project.clone();
    let section = blocks::buffers(&answer, &scope_of(&root, &rules));
    assert!(section.contains("5 more files past the 100-file bound"), "{section}");
}
