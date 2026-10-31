//! `editor_state`'s chunk in a real `nvim`: the mode words, which buffers are listed and which only counted, and
//! what the answer never names.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::path::{Path, PathBuf};

use agent::setting_sources::ProjectTrust;
use agent::PermissionMode;
use eitri_core::editor_context::compose::TRUNCATION_MARKER;
use eitri_core::editor_context::feed::{EditorContextFeed, EditorContextReader};
use eitri_core::editor_context::EditorContext;
use eitri_core::editor_tools::memory::Memory;
use eitri_core::editor_tools::stamp::{wall_ms, Stamp};
use eitri_core::editor_tools::step::{AgentCall, EditorReach, EditorSight, ToolCx, ToolResult};
use eitri_core::editor_tools::tools::editor_state::{result_of, EDITOR_STATE_LUA};
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
    let dir = std::env::temp_dir().join(format!("eto-state-{}-{case}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let project = dir.join("project");
    let beside = dir.join("beside");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::create_dir_all(&beside).expect("outside dir");
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

impl Nvim {
    fn file(&self, directory: &Path, name: &str, content: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, content).expect("write a file");
        path
    }

    fn inside(&self, name: &str) -> PathBuf {
        self.file(&self.project, name, "one\ntwo\nthree\n")
    }

    fn outside(&self, name: &str) -> PathBuf {
        self.file(&self.beside, name, "elsewhere\n")
    }

    fn edit(&mut self, path: &Path) {
        self.embed.command(&format!("edit {}", path.display()));
    }

    /// Calls the chunk as the broker would.
    fn ask(&mut self) -> Value {
        use std::os::unix::ffi::OsStrExt;
        self.seq += 1;
        let stamp = Stamp {
            deadline_ms: wall_ms() + FAR,
            scope: "state".to_owned(),
            seq: self.seq,
        };
        let args = vec![(
            Value::from("root"),
            Value::Binary(self.project.as_os_str().as_bytes().to_vec()),
        )];
        self.embed
            .lua(EDITOR_STATE_LUA, vec![stamp.to_value(), Value::Map(args)])
    }

    /// What the agent would be told, parsed.
    fn told(&mut self) -> serde_json::Value {
        self.told_with(None)
    }

    /// What the agent would be told with `context` as the feed's last line.
    fn told_with(&mut self, context: Option<&EditorContext>) -> serde_json::Value {
        let answer = self.ask();
        let memory = Memory::default();
        let cx = ToolCx {
            mode: PermissionMode::Manual,
            trust: ProjectTrust::Trusted,
            root: &self.project,
            home: None,
            reach: EditorReach::Own,
            sight: EditorSight::OnScreen,
            context,
            tab: 1,
            session: "session",
            call: "call",
            agent_call: AgentCall::Host,
            memory: &memory,
        };
        let ToolResult { text, is_error } = result_of(&cx, &answer);
        assert!(!is_error, "{text}");
        serde_json::from_str(&text).unwrap()
    }

    /// The reader of what the feed sends to the host.
    fn reader(&mut self) -> EditorContextReader {
        EditorContextReader::new(self.feed.take_listener().expect("the feed's listener"))
    }

    /// The feed's line once it carries a selection.
    fn fed_selection(&mut self, reader: &mut EditorContextReader) -> EditorContext {
        let fed = std::cell::RefCell::new(None);
        self.embed.wait_until("the feed's selection", |_| {
            if let Some(context) = reader.poll().filter(|context| context.selection.is_some()) {
                *fed.borrow_mut() = Some(context);
            }
            fed.borrow().is_some()
        });
        fed.into_inner().expect("the wait ended on a selection")
    }

    fn wait_for_mode(&mut self, wanted: &str) {
        self.embed.wait_until(&format!("mode {wanted:?}"), |nvim| {
            nvim.eval("mode(1)")
                .as_str()
                .is_some_and(|mode| mode.starts_with(wanted))
        });
    }
}

fn paths_of(told: &serde_json::Value) -> Vec<String> {
    told["buffers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|buffer| buffer["path"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
#[ignore = "needs nvim on PATH"]
fn the_mode_word_for_each_mode() {
    let mut nvim = start("modes");
    let file = nvim.inside("a.rs");
    nvim.edit(&file);

    assert_eq!(nvim.told()["mode"], "normal");

    for (keys, entered, word) in [
        ("i", "i", "insert"),
        ("R", "R", "insert"),
        ("v", "v", "visual"),
        ("V", "V", "visual"),
        ("<C-v>", "\u{16}", "visual"),
        ("gh", "s", "visual"),
        ("gH", "S", "visual"),
        (":", "c", "command"),
        ("d", "no", "normal"),
    ] {
        nvim.embed.input("<Esc><Esc>");
        nvim.wait_for_mode("n");
        nvim.embed.input(keys);
        nvim.wait_for_mode(entered);
        assert_eq!(nvim.told()["mode"], word, "after {keys:?}");
    }

    nvim.embed.input("<Esc><Esc>");
    nvim.wait_for_mode("n");
    nvim.embed.command("terminal cat");
    nvim.embed.input("i");
    nvim.wait_for_mode("t");
    assert_eq!(nvim.told()["mode"], "terminal");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn outside_buffers_are_only_a_count() {
    let mut nvim = start("outside");
    let mine = nvim.inside("a.rs");
    let first = nvim.outside("secret_notes.txt");
    let second = nvim.outside("passwords.txt");
    for file in [&first, &second, &mine] {
        nvim.edit(file);
    }
    let answer = nvim.told();
    assert_eq!(paths_of(&answer), ["a.rs"]);
    assert_eq!(answer["other_buffers"], 2);
    assert_eq!(answer["buffers"][0]["modified"], false);
    assert_eq!(answer["buffers"][0]["lines"], 3);
    assert_eq!(answer["buffers"][0]["shown"], true);
    let text = answer.to_string();
    assert!(!text.contains("secret_notes"), "{text}");
    assert!(!text.contains("passwords"), "{text}");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_buffer_read_from_outside_is_only_counted() {
    use std::os::unix::fs::symlink;
    let mut nvim = start("read-elsewhere");
    let secret = nvim.outside("secret.rs");
    let link = nvim.project.join("link.rs");
    symlink(&secret, &link).unwrap();
    nvim.edit(&link);
    std::fs::remove_file(&link).unwrap();
    std::fs::write(&link, "plain\n").unwrap();
    let told = nvim.told();
    assert_eq!(paths_of(&told), Vec::<String>::new());
    assert_eq!(told["other_buffers"], 1);
    assert_eq!(told["current"], "a file outside the project");
}

/// The reload timer runs while the person stays in Visual mode, nothing moves and the feed says nothing, so the feed's
/// last line can hold text of a file the buffer no longer shows, under a record that passes.
#[test]
#[ignore = "needs nvim on PATH"]
fn a_selection_the_buffer_no_longer_holds_is_not_told() {
    use std::os::unix::fs::symlink;
    let mut nvim = start("selection-reloaded");
    let mut reader = nvim.reader();
    let secret = nvim.file(&nvim.beside.clone(), "secret.rs", "secret words\n");
    let link = nvim.project.join("link.rs");
    symlink(&secret, &link).unwrap();
    nvim.edit(&link);
    nvim.embed.input("V");
    nvim.wait_for_mode("V");
    let fed = nvim.fed_selection(&mut reader);
    assert_eq!(fed.selection.as_ref().unwrap().text, "secret words");

    std::fs::remove_file(&link).unwrap();
    std::fs::write(&link, "plain\n").unwrap();
    nvim.embed
        .lua("vim.schedule(function() pcall(vim.cmd, 'checktime') end)", vec![]);
    let name = link.to_str().unwrap().to_owned();
    nvim.embed.wait_until("the reload", |nvim| {
        nvim.lua("return vim.b.eitri_read_from", vec![]).as_str() == Some(name.as_str())
    });
    nvim.wait_for_mode("V");

    let told = nvim.told_with(Some(&fed));
    assert_eq!(told["current"]["path"], "link.rs", "the buffer itself now passes");
    assert!(told["selection"].is_null(), "{told}");
    assert!(!told.to_string().contains("secret words"), "{told}");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_selection_given_up_is_not_told() {
    let mut nvim = start("selection-left");
    let mut reader = nvim.reader();
    let file = nvim.inside("a.rs");
    nvim.edit(&file);
    nvim.embed.input("V");
    nvim.wait_for_mode("V");
    let fed = nvim.fed_selection(&mut reader);
    nvim.embed.input("<Esc>");
    nvim.wait_for_mode("n");
    assert!(nvim.told_with(Some(&fed))["selection"].is_null());
}

/// The feed's text, whichever way it was cut out of its lines, is what the buffer holds; a block that cuts a <Tab> gets
/// spaces in its place, which the buffer does not hold and which say nothing.
#[test]
#[ignore = "needs nvim on PATH"]
fn a_live_selection_is_told_in_every_visual_mode() {
    let plain = "oh\u{e9}llo\nsecond\n";
    for (case, body, keys, text) in [
        ("char", plain, "gg0lvjl", "h\u{e9}llo\nsec"),
        ("line", plain, "ggVj", "oh\u{e9}llo\nsecond"),
        ("block", plain, "gg0l<C-v>jl", "h\u{e9}\nec"),
        ("tab", "abcdefghij\nab\tXY\nabcdefghij\n", "gg03l<C-v>jjl", "de\n  \nde"),
    ] {
        let mut nvim = start(&format!("selection-{case}"));
        let mut reader = nvim.reader();
        let file = nvim.file(&nvim.project.clone(), "a.rs", body);
        nvim.edit(&file);
        nvim.embed.input(keys);
        let fed = nvim.fed_selection(&mut reader);
        assert_eq!(fed.selection.as_ref().unwrap().text, text, "{case}");
        let told = nvim.told_with(Some(&fed));
        assert_eq!(told["selection"]["path"], "a.rs", "{case}: {told}");
        assert_eq!(told["selection"]["text"], text, "{case}: {told}");
    }
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_selection_cut_by_the_feed_is_still_told() {
    let mut nvim = start("selection-cut");
    let mut reader = nvim.reader();
    let lines: Vec<String> = (0..600).map(|line| format!("line number {line}")).collect();
    let file = nvim.file(&nvim.project.clone(), "a.rs", &lines.join("\n"));
    nvim.edit(&file);
    nvim.embed.input("ggVG");
    let fed = nvim.fed_selection(&mut reader);
    let told = nvim.told_with(Some(&fed));
    let text = told["selection"]["text"].as_str().unwrap_or_else(|| panic!("{told}"));
    assert!(text.ends_with(TRUNCATION_MARKER), "{text}");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_buffer_not_loaded_is_named_by_its_name_alone() {
    let mut nvim = start("not-loaded");
    let file = nvim.inside("later.rs");
    nvim.embed.command(&format!("badd {}", file.display()));
    assert_eq!(paths_of(&nvim.told()), ["later.rs"], "it holds no text to judge");
}

#[test]
#[ignore = "needs nvim on PATH"]
fn an_unnamed_and_an_unlisted_buffer_are_left_out() {
    let mut nvim = start("unlisted");
    let kept = nvim.inside("kept.rs");
    let hidden = nvim.inside("hidden.rs");
    nvim.edit(&hidden);
    nvim.embed.command("setlocal nobuflisted");
    nvim.edit(&kept);
    nvim.embed.command("enew");
    nvim.embed.command("enew");
    let answer = nvim.told();
    assert_eq!(paths_of(&answer), ["kept.rs"]);
    assert_eq!(answer["other_buffers"], 0, "{answer}");
    assert_eq!(answer["buffers"][0]["shown"], false);
    assert!(!answer.to_string().contains("hidden"));
}

#[test]
#[ignore = "needs nvim on PATH"]
fn a_current_file_outside_is_not_named() {
    let mut nvim = start("current-outside");
    let mine = nvim.inside("a.rs");
    let elsewhere = nvim.outside("private_plans.txt");
    nvim.edit(&mine);
    let inside = nvim.told();
    assert_eq!(inside["current"]["path"], "a.rs");
    assert_eq!(inside["current"]["line"], 1);

    nvim.edit(&elsewhere);
    let outside = nvim.told();
    assert_eq!(outside["current"], "a file outside the project", "{outside}");
    assert_eq!(outside["other_buffers"], 1);
    let text = outside.to_string();
    assert!(!text.contains("private_plans"), "{text}");
}
