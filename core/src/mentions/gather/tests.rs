use super::*;
use crate::editor_tools::testing::FakeEditor;
use crate::test_scratch_dir::ScratchDir;
use serde_json::json;
use std::time::Duration;

struct Project {
    _scratch: ScratchDir,
    root: PathBuf,
}

fn project(label: &str, deny: &[&str]) -> Project {
    let scratch = ScratchDir::new("mentions-gather", label);
    let root = std::fs::canonicalize(&*scratch).unwrap().join("project");
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    std::fs::write(root.join("a.rs"), "a\n").unwrap();
    std::fs::write(root.join("secret.txt"), "s\n").unwrap();
    let list: Vec<String> = deny.iter().map(|rule| format!("{rule:?}")).collect();
    std::fs::write(
        root.join(".claude/settings.json"),
        format!(r#"{{"permissions":{{"deny":[{}]}}}}"#, list.join(",")),
    )
    .unwrap();
    Project {
        _scratch: scratch,
        root,
    }
}

/// A settings directory of the account, which the reader needs to be told of.
fn config(project: &Project) -> PathBuf {
    let dir = project.root.with_file_name("config");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sequencer() -> Sequencer {
    Sequencer::new("mention-test".to_owned())
}

/// Polls until the sections are in; the settings are read on a worker.
fn finished(gather: &mut Gather, now: Instant) -> Gathered {
    for _ in 0..400 {
        if let Some(done) = gather.poll(now) {
            return done;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the gather never finished");
}

fn buffer_reply(path: &Path, text: &[&str]) -> serde_json::Value {
    json!({
        "buffers": [{
            "path": path.to_str().unwrap(), "read_from": path.to_str().unwrap(), "modified": true,
            "loaded": true, "current": true, "shown": true, "lines": text.len(), "filetype": "rust",
            "text": text,
        }],
        "other": 0, "left_out": 0,
    })
}

fn quickfix_reply(path: &Path) -> serde_json::Value {
    json!({
        "title": ":make", "size": 2, "left_out": 0,
        "items": [
            {"path": path.to_str().unwrap(), "read_from": path.to_str().unwrap(), "lnum": 3, "end_lnum": 0, "col": 1,
             "kind": "E", "valid": true, "text": "boom"},
            {"path": project_file(path, "secret.txt"), "read_from": project_file(path, "secret.txt"), "lnum": 1,
             "end_lnum": 0, "col": 0, "kind": "", "valid": true, "text": "SECRET-ENTRY"},
        ],
    })
}

fn project_file(of: &Path, name: &str) -> String {
    of.with_file_name(name).to_str().unwrap().to_owned()
}

fn diagnostics_reply(path: &Path) -> serde_json::Value {
    json!({
        "entries": [{
            "path": path.to_str().unwrap(), "read_from": path.to_str().unwrap(), "lnum": 11, "col": 4,
            "end_lnum": 11, "end_col": 8, "severity": "error", "source": "rust-analyzer", "code": "E0308",
            "message": "mismatched types",
        }],
        "modified": [path.to_str().unwrap()], "left_out": 0,
    })
}

#[test]
fn each_name_is_read_once_and_the_sections_come_in_the_order_written() {
    let project = project("order", &["Read(secret.txt)"]);
    let editor = FakeEditor::default();
    let now = Instant::now();
    let a = project.root.join("a.rs");
    let mut gather = Gather::start(
        Some(&editor),
        &mut sequencer(),
        &[Kind::Diagnostics, Kind::Buffers, Kind::Quickfix],
        &project.root,
        None,
        Some(config(&project)),
        now,
    );
    assert_eq!(editor.asked().len(), 3, "one chunk for each name");
    assert!(gather.poll(now).is_none(), "nothing has answered");
    editor.answer_oldest(&diagnostics_reply(&a));
    assert!(gather.poll(now).is_none(), "two are still out");
    editor.answer_oldest(&buffer_reply(&a, &["fn a() {}"]));
    editor.answer_oldest(&quickfix_reply(&a));
    let done = finished(&mut gather, now);
    let kinds: Vec<Kind> = done.sections.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, [Kind::Diagnostics, Kind::Buffers, Kind::Quickfix]);
    let text = done.appended_to("why?");
    assert!(text.starts_with("why?\n\n[@diagnostics: "), "{text}");
    assert!(
        text.contains("a.rs:12:5 error [rust-analyzer E0308]: mismatched types"),
        "{text}"
    );
    assert!(
        text.contains("--- a.rs (unsaved changes, current, 1 line) ---\n1\tfn a() {}"),
        "{text}"
    );
    assert!(text.contains("a.rs:3:1 error boom"), "{text}");
    assert!(
        !text.contains("SECRET-ENTRY") && !text.contains("secret.txt"),
        "the deny rule held: {text}"
    );
    assert!(text.ends_with("[@quickfix end]"), "{text}");
}

#[test]
fn with_no_editor_each_name_says_it_was_not_attached() {
    let project = project("no-editor", &[]);
    let now = Instant::now();
    let mut gather = Gather::start(
        None,
        &mut sequencer(),
        &[Kind::Buffers],
        &project.root,
        None,
        Some(config(&project)),
        now,
    );
    let done = finished(&mut gather, now);
    assert_eq!(
        done.appended_to("x"),
        "x\n\n[@buffers: not attached: the editor is not running]\n[@buffers end]"
    );
}

#[test]
fn an_editor_that_does_not_answer_in_time_is_given_up_on() {
    let project = project("late", &[]);
    let editor = FakeEditor::default();
    let now = Instant::now();
    let mut gather = Gather::start(
        Some(&editor),
        &mut sequencer(),
        &[Kind::Quickfix],
        &project.root,
        None,
        Some(config(&project)),
        now,
    );
    assert!(gather.poll(now + Duration::from_secs(4)).is_none());
    let done = finished(&mut gather, now + EDITOR_BUDGET);
    assert!(
        done.appended_to("")
            .contains("not attached: the editor did not answer within 5 s"),
        "{done:?}"
    );
}

#[test]
fn an_answer_that_is_not_the_chunks_is_a_section_that_says_so() {
    let project = project("garbled", &[]);
    let editor = FakeEditor::default();
    let now = Instant::now();
    let mut gather = Gather::start(
        Some(&editor),
        &mut sequencer(),
        &[Kind::Buffers],
        &project.root,
        None,
        Some(config(&project)),
        now,
    );
    editor.answer_oldest(&json!({"buffers": "no"}));
    let done = finished(&mut gather, now);
    assert!(done
        .appended_to("")
        .contains("not attached: the editor's answer about its open buffers could not be read"));
}

#[test]
fn a_settings_file_that_is_not_json_keeps_every_file_out() {
    let project = project("closed", &[]);
    std::fs::write(project.root.join(".claude/settings.local.json"), "{ nope").unwrap();
    let editor = FakeEditor::default();
    let now = Instant::now();
    let a = project.root.join("a.rs");
    let mut gather = Gather::start(
        Some(&editor),
        &mut sequencer(),
        &[Kind::Buffers],
        &project.root,
        None,
        Some(config(&project)),
        now,
    );
    editor.answer_oldest(&buffer_reply(&a, &["TEXT"]));
    let done = finished(&mut gather, now).appended_to("");
    assert!(
        done.contains("not attached: ") && done.contains("settings.local.json could not be read"),
        "{done}"
    );
    assert!(!done.contains("TEXT"), "{done}");
}
