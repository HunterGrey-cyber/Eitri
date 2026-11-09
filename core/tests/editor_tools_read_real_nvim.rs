//! `read_buffer`'s chunk in a real `nvim`: the text a buffer holds, what it refuses, and that it loads nothing.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use agent::setting_sources::ProjectTrust;
use agent::PermissionMode;
use eitri_core::editor_context::feed::EditorContextFeed;
use eitri_core::editor_tools::memory::Memory;
use eitri_core::editor_tools::stamp::{wall_ms, Stamp};
use eitri_core::editor_tools::step::{AgentCall, EditorReach, EditorSight, Step, StepEvent, Tool, ToolCx, ToolResult};
use eitri_core::editor_tools::tools::read_buffer::{result_of, ReadBuffer, READ_BUFFER_LUA};
use eitri_core::editor_tools::way::EditorCall;
use embed_client::{opts_map, Embed};
use rmpv::Value;

const FAR: u64 = 60_000;

/// Counts of what exists, across every tabpage.
const COUNTS: &str = "
local named = 0
for _, b in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_get_name(b) ~= '' then named = named + 1 end
end
return { bufs = #vim.api.nvim_list_bufs(), named = named, wins = #vim.api.nvim_list_wins() }";

struct Nvim {
    embed: Embed,
    feed: Option<EditorContextFeed>,
    dir: PathBuf,
    project: PathBuf,
    seq: u64,
}

impl Drop for Nvim {
    fn drop(&mut self) {
        if let Some(feed) = &self.feed {
            feed.cleanup();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(case: &str) -> Nvim {
    let dir = std::env::temp_dir().join(format!("eto-read-{}-{case}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let project = dir.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let embed = Embed::start(&project, &[], &[]);
    Nvim {
        embed,
        feed: None,
        dir,
        project,
        seq: 0,
    }
}

/// [`start`] with the feed loaded as the embedded editor loads it, in a scratch directory with every link resolved.
fn start_fed(case: &str) -> Nvim {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = std::env::temp_dir().join(format!("eto-read-{}-{case}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("project")).expect("project dir");
    std::fs::create_dir_all(dir.join("beside")).expect("outside dir");
    let dir = std::fs::canonicalize(&dir).expect("scratch dir");
    let project = dir.join("project");
    let feed = EditorContextFeed::new().expect("the feed must bind");
    let embed = Embed::start_fed(&project, &feed, &[]);
    Nvim {
        embed,
        feed: Some(feed),
        dir,
        project,
        seq: 0,
    }
}

impl Nvim {
    fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.project.join(name);
        std::fs::write(&path, content).expect("write a file");
        path
    }

    fn stamp(&mut self) -> Stamp {
        self.seq += 1;
        Stamp {
            deadline_ms: wall_ms() + FAR,
            scope: "read".to_owned(),
            seq: self.seq,
        }
    }

    fn read_stamped(&mut self, stamp: &Stamp, path: &str, start: Option<u32>, end: Option<u32>) -> Value {
        use std::os::unix::ffi::OsStrExt;
        let mut args = vec![(
            Value::from("path"),
            Value::Binary(Path::new(path).as_os_str().as_bytes().to_vec()),
        )];
        if let Some(start) = start {
            args.push((Value::from("start"), Value::from(start)));
        }
        if let Some(end) = end {
            args.push((Value::from("end"), Value::from(end)));
        }
        self.embed
            .lua(READ_BUFFER_LUA, vec![stamp.to_value(), Value::Map(args)])
    }

    /// Calls the chunk as the broker would.
    fn read(&mut self, path: &Path) -> Value {
        self.read_range(path, None, None)
    }

    fn read_range(&mut self, path: &Path, start: Option<u32>, end: Option<u32>) -> Value {
        let stamp = self.stamp();
        self.read_stamped(&stamp, path.to_str().unwrap(), start, end)
    }

    fn edit(&mut self, path: &Path) {
        self.embed.command(&format!("edit {}", path.display()));
    }

    fn set_lines(&mut self, lines: &[&str]) {
        let lines = Value::Array(lines.iter().map(|line| Value::from(*line)).collect());
        self.embed
            .lua("vim.api.nvim_buf_set_lines(0, 0, -1, false, ...)", vec![lines]);
    }

    fn counts(&mut self) -> Value {
        self.embed.lua(COUNTS, vec![])
    }

    /// What the buffer in the current window keeps of where it was read from.
    fn record(&mut self) -> Value {
        self.embed.lua("return vim.b.eitri_read_from", vec![])
    }

    /// What the agent is told of `raw` through the tool itself: the judgement, the chunk, the judgement of its answer.
    fn told(&mut self, raw: &str) -> ToolResult {
        let root = self.project.clone();
        let memory = Memory::default();
        let cx = ToolCx {
            mode: PermissionMode::Manual,
            trust: ProjectTrust::Trusted,
            root: &root,
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
        let mut run = ReadBuffer
            .begin(&serde_json::json!({ "path": raw }), &cx)
            .expect("the call is well formed");
        let Step::Call {
            call: EditorCall::ReadBuffer { path, start, end },
            ..
        } = run.step(StepEvent::Start, &cx)
        else {
            panic!("the tool must ask the editor");
        };
        let stamp = self.stamp();
        let answer = self.read_stamped(&stamp, path.to_str().unwrap(), start, end);
        let Step::Done(result) = run.step(StepEvent::Answer(Ok(answer)), &cx) else {
            panic!("the tool must end on the editor's answer");
        };
        result
    }
}

fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(name, _)| name.as_str() == Some(key))
        .map(|(_, value)| value)
}

fn says(value: &Value, key: &str) -> bool {
    find(value, key).is_some_and(|found| *found == Value::from(true))
}

/// What the agent would be told, parsed.
fn text_of(path: &Path, end: Option<u32>, answer: &Value) -> serde_json::Value {
    let ToolResult { text, is_error } = result_of(path, end, answer);
    assert!(!is_error, "{text}");
    serde_json::from_str(&text).unwrap()
}

fn refusal_of(path: &Path, answer: &Value) -> String {
    let ToolResult { text, is_error } = result_of(path, None, answer);
    assert!(is_error, "{text}");
    text
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_buffer_read_through_a_link_keeps_where_it_came_from() {
    let mut nvim = start_fed("link-replaced");
    let secret = nvim.dir.join("beside/secret.env");
    std::fs::write(&secret, "TOKEN=from-outside\n").unwrap();
    let link = nvim.project.join("settings.env");
    symlink(&secret, &link).unwrap();
    nvim.edit(&link);
    assert_eq!(nvim.record(), Value::from(secret.to_str().unwrap()));

    std::fs::remove_file(&link).unwrap();
    std::fs::write(&link, "PLAIN=1\n").unwrap();
    let result = nvim.told("settings.env");
    assert!(result.is_error, "{}", result.text);
    assert!(!result.text.contains("TOKEN"), "{}", result.text);
    assert!(
        !result.text.contains("secret.env"),
        "the file goes unnamed: {}",
        result.text
    );
    assert!(result.text.contains("use Read"), "{}", result.text);
    assert_eq!(
        nvim.record(),
        Value::from(secret.to_str().unwrap()),
        "only a read moves it"
    );

    // What is read now is the project's own file, so the record moves and the text is told.
    nvim.embed.command("edit!");
    assert_eq!(nvim.record(), Value::from(link.to_str().unwrap()));
    let result = nvim.told("settings.env");
    assert!(!result.is_error, "{}", result.text);
    assert!(result.text.contains("PLAIN=1"), "{}", result.text);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_buffer_with_no_record_is_not_told() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let mut nvim = start("no-record");
    let feed = EditorContextFeed::new().expect("the feed must bind");
    let file = nvim.file("a.rs", "one\n");
    nvim.edit(&file);
    nvim.embed.inject(
        include_str!("../src/editor_context/nvim_editor_context.lua"),
        opts_map(&[("socket", Value::from(feed.socket_path().display().to_string()))]),
    );
    nvim.feed = Some(feed);
    assert_eq!(nvim.record(), Value::Nil, "the chunk came after the buffer");
    let result = nvim.told(file.to_str().unwrap());
    assert!(result.is_error, "{}", result.text);
    assert!(!result.text.contains("one"), "{}", result.text);

    nvim.embed.command("edit!");
    let result = nvim.told(file.to_str().unwrap());
    assert!(!result.is_error, "a reload records what it read: {}", result.text);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn an_ordinary_project_buffer_is_told() {
    let mut nvim = start_fed("ordinary");
    let file = nvim.file("a.rs", "one\ntwo\n");
    nvim.edit(&file);
    nvim.set_lines(&["unsaved"]);
    let result = nvim.told("a.rs");
    assert!(!result.is_error, "{}", result.text);
    assert!(result.text.contains("unsaved"), "{}", result.text);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_new_file_is_told_before_it_is_saved() {
    let mut nvim = start_fed("new-file");
    let file = nvim.project.join("new.rs");
    nvim.edit(&file);
    nvim.set_lines(&["fn fresh() {}"]);
    let result = nvim.told("new.rs");
    assert!(!result.is_error, "{}", result.text);
    assert!(result.text.contains("fn fresh"), "{}", result.text);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn unsaved_text_comes_back_with_modified_true() {
    let mut nvim = start("unsaved");
    let path = nvim.file("a.rs", "one\ntwo\n");
    nvim.edit(&path);
    nvim.set_lines(&["changed", "two"]);
    let answer = nvim.read(&path);
    let told = text_of(&path, None, &answer);
    assert_eq!(told["text"], "changed\ntwo\n");
    assert_eq!(told["modified"], true);
    assert_eq!(told["first_line"], 1);
    assert_eq!(told["last_line"], 2);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "one\ntwo\n",
        "the file on disk is not what was read"
    );
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_saved_buffer_says_modified_false() {
    let mut nvim = start("saved");
    let path = nvim.file("a.rs", "one\ntwo");
    nvim.edit(&path);
    let told = text_of(&path, None, &nvim.read(&path));
    assert_eq!(told["modified"], false);
    assert_eq!(
        told["text"], "one\ntwo",
        "a file without a final newline reads without one"
    );
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_range_is_read_from_its_first_line() {
    let mut nvim = start("range");
    let text: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    let path = nvim.file("a.rs", &text);
    nvim.edit(&path);
    let answer = nvim.read_range(&path, Some(3), Some(4));
    let told = text_of(&path, Some(4), &answer);
    assert_eq!(told["text"], "line 3\nline 4\n");
    assert_eq!(told["first_line"], 3);
    assert_eq!(told["last_line"], 4);
    assert!(told.get("left_out").is_none(), "{told}");
    let past = nvim.read_range(&path, Some(11), None);
    assert!(refusal_of(&path, &past).contains("past the end"));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn it_never_loads_a_file() {
    let mut nvim = start("never-loads");
    let path = nvim.file("unopened.rs", "text\n");
    let before = nvim.counts();
    let answer = nvim.read(&path);
    assert!(says(&answer, "not_loaded"), "{answer:?}");
    assert!(refusal_of(&path, &answer).ends_with("is not open in the editor; use Read"));
    assert_eq!(nvim.counts(), before, "no buffer was added");

    // Listed but not loaded is still not read.
    nvim.edit(&path);
    nvim.embed.command("enew");
    nvim.embed.command(&format!("bunload {}", path.display()));
    let after_unload = nvim.counts();
    assert!(says(&nvim.read(&path), "not_loaded"));
    assert_eq!(nvim.counts(), after_unload);
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_terminal_buffer_is_refused() {
    let mut nvim = start("terminal");
    nvim.embed.command("terminal cat");
    let name = nvim
        .embed
        .lua("return vim.api.nvim_buf_get_name(0)", vec![])
        .as_str()
        .unwrap()
        .to_owned();
    let stamp = nvim.stamp();
    let answer = nvim.read_stamped(&stamp, &name, None, None);
    assert_eq!(find(&answer, "refused"), Some(&Value::from("terminal")), "{answer:?}");
    assert!(refusal_of(Path::new(&name), &answer).contains("is a terminal buffer, not a file"));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn help_and_quickfix_are_refused() {
    let mut nvim = start("help-quickfix");
    nvim.embed.command("help");
    let help = nvim
        .embed
        .lua("return vim.api.nvim_buf_get_name(0)", vec![])
        .as_str()
        .unwrap()
        .to_owned();
    let stamp = nvim.stamp();
    let answer = nvim.read_stamped(&stamp, &help, None, None);
    assert_eq!(find(&answer, "refused"), Some(&Value::from("help")), "{answer:?}");

    nvim.embed.command("cexpr ['a.rs:1:1:something']");
    nvim.embed.command("copen");
    // The quickfix list has no name of its own; a plugin may give it one.
    let named = nvim.project.join("quickfix-list");
    nvim.embed.lua(
        "vim.api.nvim_buf_set_name(0, ...)",
        vec![Value::from(named.to_str().unwrap())],
    );
    let answer = nvim.read(&named);
    assert_eq!(find(&answer, "refused"), Some(&Value::from("quickfix")), "{answer:?}");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn an_older_stamp_reads_nothing() {
    let mut nvim = start("stamp");
    let path = nvim.file("a.rs", "x\n");
    nvim.edit(&path);
    let newer = nvim.stamp();
    let older = Stamp {
        seq: newer.seq - 1,
        ..newer.clone()
    };
    let name = path.to_str().unwrap();
    let answer = nvim.read_stamped(&newer, name, None, None);
    assert!(find(&answer, "lines").is_some(), "{answer:?}");
    for stale in [older, newer.clone()] {
        let answer = nvim.read_stamped(&stale, name, None, None);
        assert!(says(&answer, "expired"), "{answer:?}");
        assert_eq!(find(&answer, "lines"), None);
    }
    let late = Stamp {
        deadline_ms: wall_ms().saturating_sub(1),
        seq: newer.seq + 10,
        ..newer
    };
    assert!(says(&nvim.read_stamped(&late, name, None, None), "expired"));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_crlf_buffer_comes_back_as_its_bytes() {
    let mut nvim = start("crlf");
    let path = nvim.file("a.txt", "a\r\nb\r\n");
    nvim.embed.command(&format!("edit ++ff=dos {}", path.display()));
    let answer = nvim.read(&path);
    assert_eq!(find(&answer, "fileformat"), Some(&Value::from("dos")));
    let told = text_of(&path, None, &answer);
    assert_eq!(told["text"], "a\r\nb\r\n");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn the_cut_is_at_a_line_and_says_which_were_left_out() {
    let mut nvim = start("cut");

    // Over 4000 lines, short enough that the byte limit is not reached first.
    let many: String = (1..=4500).map(|n| format!("{n}\n")).collect();
    let path = nvim.file("many.txt", &many);
    nvim.edit(&path);
    let answer = nvim.read(&path);
    assert_eq!(find(&answer, "cut"), Some(&Value::from(4000)));
    let told = text_of(&path, None, &answer);
    assert_eq!(told["last_line"], 4000);
    assert_eq!(told["text"].as_str().unwrap().lines().count(), 4000);
    assert!(
        told["left_out"]
            .as_str()
            .unwrap()
            .starts_with("lines 4001 to 4500 of 4500 left out"),
        "{told}"
    );

    // Over 32 KiB: whole lines of 1000 bytes.
    let wide: String = (0..400).map(|_| format!("{}\n", "x".repeat(999))).collect();
    let path = nvim.file("wide.txt", &wide);
    nvim.edit(&path);
    let answer = nvim.read(&path);
    let told = text_of(&path, None, &answer);
    let last = told["last_line"].as_u64().unwrap();
    assert!((30..=33).contains(&last), "{last}");
    let text = told["text"].as_str().unwrap();
    assert!(text.len() <= 32 * 1024);
    assert!(text.ends_with('\n'));
    assert!(text.lines().all(|line| line.len() == 999), "only whole lines");
    assert!(
        told["left_out"]
            .as_str()
            .unwrap()
            .starts_with(&format!("lines {} to 400 of 400 left out", last + 1)),
        "{told}"
    );
}

#[test]
#[ignore = "needs nvim on PATH"]
fn an_unsaved_new_file_reads() {
    let mut nvim = start("new-file");
    let path = nvim.project.join("fresh.rs");
    nvim.edit(&path);
    nvim.set_lines(&["fn fresh() {}"]);
    assert!(!path.exists());
    let told = text_of(&path, None, &nvim.read(&path));
    assert_eq!(told["text"], "fn fresh() {}\n");
    assert_eq!(told["modified"], true);
}

/// The buffer an answer says it read.
fn name_in(answer: &Value) -> Vec<u8> {
    match find(answer, "name").expect("the answer names its buffer") {
        Value::String(text) => text.as_bytes().to_vec(),
        Value::Binary(bytes) => bytes.clone(),
        other => panic!("a name, got {other:?}"),
    }
}

#[test]
#[ignore = "needs nvim on PATH"]
fn the_answer_names_the_buffer_read() {
    use std::os::unix::ffi::OsStrExt;
    let mut nvim = start("names");
    let path = nvim.file("a.rs", "x\n");
    nvim.edit(&path);
    let answer = nvim.read(&path);
    assert_eq!(name_in(&answer), path.as_os_str().as_bytes());
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_path_that_became_a_link_names_the_buffer_it_found_through_the_link() {
    use std::os::unix::ffi::OsStrExt;
    let mut nvim = start("became-link");
    let secret = nvim.dir.join("secret.rs");
    std::fs::write(&secret, "secret\n").expect("write a file");
    nvim.edit(&secret);
    let swapped = nvim.project.join("a.rs");
    std::os::unix::fs::symlink(&secret, &swapped).expect("a link");

    let answer = nvim.read(&swapped);
    assert_eq!(
        name_in(&answer),
        secret.as_os_str().as_bytes(),
        "the name is what lets the host refuse it"
    );
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_mac_fileformat_buffer_is_refused_by_name() {
    let mut nvim = start("mac");
    let path = nvim.file("a.txt", "a\nb\n");
    nvim.edit(&path);
    nvim.embed.command("setlocal fileformat=mac");
    let answer = nvim.read(&path);
    assert_eq!(find(&answer, "fileformat"), Some(&Value::from("mac")));
    assert_eq!(refusal_of(&path, &answer), "this buffer's fileformat is mac; use Read");
}
