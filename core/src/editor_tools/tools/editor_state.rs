//! `editor_state`: what the editor holds.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rmpv::Value;

use super::path_value;
use crate::editor_context::compose::truncate;
use crate::editor_rpc::EditorRpc;
use crate::editor_tools::catalog::ToolRow;
use crate::editor_tools::paths;
use crate::editor_tools::stamp::{Stamp, EDITOR_BUDGET};
use crate::editor_tools::step::{
    encoded_len, EditorFailure, EditorReach, Step, StepEvent, Tool, ToolCx, ToolError, ToolResult, ToolRun,
    ANSWER_MAX_ENCODED,
};
use crate::editor_tools::way::EditorCall;
use crate::editor_tools::words;
use crate::nvim_rpc::Pending;

pub const ROW: ToolRow = ToolRow {
    name: "editor_state",
    description: "What the person's editor holds: this project's open files and which have unsaved changes, the \
                  current file and line, the mode, and the live selection.",
    schema: r#"{
  "type": "object",
  "properties": {},
  "additionalProperties": false
}"#,
    always_load: true,
    default_on: true,
    in_untrusted: true,
};

pub const EDITOR_STATE_LUA: &str = chunk!("editor_state.lua");

/// What a selection is told as when its place and text are not the agent's to read.
const SELECTION_OUTSIDE: &str = "a selection in a file outside the project";
const SELECTION_NO_FILE: &str = "a selection in a buffer with no file";
/// What the person's place is told as when its path and line are not the agent's to read.
const CURRENT_OUTSIDE: &str = "a file outside the project";
const CURRENT_NO_FILE: &str = "a buffer with no file";

pub struct EditorState;

impl Tool for EditorState {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    fn begin(&self, _args: &serde_json::Value, _cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        Ok(Box::new(StateRun { asked: false }))
    }
}

struct StateRun {
    asked: bool,
}

fn failure(text: impl Into<String>) -> ToolResult {
    ToolResult {
        text: text.into(),
        is_error: true,
    }
}

impl ToolRun for StateRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match event {
            StepEvent::Start if !self.asked => {
                self.asked = true;
                Step::Call {
                    call: EditorCall::EditorState {
                        root: cx.root.to_path_buf(),
                    },
                    budget: EDITOR_BUDGET,
                }
            }
            StepEvent::Answer(answer) if self.asked => Step::Done(match answer {
                Ok(value) => result_of(cx, &value),
                Err(EditorFailure::NotAnswered) => failure(words::NOT_READ),
                Err(EditorFailure::Unavailable(_)) if cx.reach == EditorReach::Link => failure(words::WINDOW_CLOSED),
                Err(EditorFailure::Unavailable(text) | EditorFailure::Refused(text)) => failure(text),
            }),
            _ => Step::Done(failure("the editor was asked nothing")),
        }
    }
}

/// A buffer the chunk listed under the root.
struct Listed {
    path: PathBuf,
    modified: bool,
    lines: u32,
    filetype: String,
    shown: bool,
}

struct Reply {
    buffers: Vec<Listed>,
    other: u32,
    left_out: u32,
    line: u32,
    current: Option<PathBuf>,
    mode: String,
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

fn count(value: &Value, name: &str) -> Option<u32> {
    u32::try_from(field(value, name)?.as_u64()?).ok()
}

/// A name from the editor. nvim sends a Lua string as msgpack str whatever its bytes are, and they are kept.
fn name_of(value: &Value) -> Option<PathBuf> {
    let bytes = match value {
        Value::String(text) => text.as_bytes(),
        Value::Binary(bytes) => bytes.as_slice(),
        _ => return None,
    };
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

/// A word the editor named, kept to plain characters so it can sit in a sentence.
fn word(value: &Value) -> String {
    let text = value.as_str().unwrap_or("");
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        .take(32)
        .collect()
}

fn listed(value: &Value) -> Option<Listed> {
    Some(Listed {
        path: name_of(field(value, "path")?)?,
        modified: field(value, "modified")?.as_bool()?,
        lines: count(value, "lines")?,
        filetype: word(field(value, "filetype")?),
        shown: field(value, "shown")?.as_bool()?,
    })
}

fn parse(value: &Value) -> Option<Reply> {
    let buffers = match field(value, "buffers")? {
        // An empty Lua table may arrive as either.
        Value::Map(entries) if entries.is_empty() => Vec::new(),
        Value::Array(items) => items.iter().map(listed).collect::<Option<Vec<_>>>()?,
        _ => return None,
    };
    let here = field(value, "current")?;
    Some(Reply {
        buffers,
        other: count(value, "other")?,
        left_out: count(value, "left_out")?,
        line: count(here, "line")?,
        current: field(here, "path").and_then(name_of),
        mode: field(value, "mode")?.as_str()?.to_owned(),
    })
}

/// The word for nvim's mode string. Its first character names the family: `no` and its kin are
/// operator-pending, and a mode this does not know reads as normal.
fn mode_word(raw: &str) -> &'static str {
    match raw.chars().next() {
        Some('i' | 'R') => "insert",
        Some('v' | 'V' | '\u{16}' | 's' | 'S' | '\u{13}') => "visual",
        Some('c') => "command",
        Some('t') => "terminal",
        _ => "normal",
    }
}

/// Where the project holds `name`, relative to the root. A name that is not absolute is never a file of the
/// project: a terminal buffer's `term://` name would otherwise read as one under the root.
fn inside(cx: &ToolCx<'_>, name: &Path) -> Option<String> {
    if !name.is_absolute() || !paths::buffer_inside(cx, name) {
        return None;
    }
    let shown = name.strip_prefix(cx.root).unwrap_or(name);
    Some(shown.to_string_lossy().into_owned())
}

/// The live selection the feed last pushed: none, the words for one whose place the agent is not told, or its
/// place and its text cut as the feed cuts it.
fn selection(cx: &ToolCx<'_>) -> serde_json::Value {
    let Some(context) = cx.context else {
        return serde_json::Value::Null;
    };
    let Some(selection) = &context.selection else {
        return serde_json::Value::Null;
    };
    if context.file.is_empty() {
        return serde_json::Value::from(SELECTION_NO_FILE);
    }
    match inside(cx, Path::new(&context.file)) {
        Some(path) => serde_json::json!({
            "path": path,
            "start_line": selection.start_line,
            "end_line": selection.end_line,
            "text": truncate(&selection.text),
        }),
        None => serde_json::Value::from(SELECTION_OUTSIDE),
    }
}

/// The answer's JSON with the first `kept` buffers; the buffers after them, and those the chunk cut, are counted.
fn answer_text(
    cx: &ToolCx<'_>,
    reply: &Reply,
    told: &[serde_json::Value],
    other: u32,
    kept: usize,
    selection: &serde_json::Value,
) -> String {
    let current = match reply.current.as_deref().map(|name| inside(cx, name)) {
        Some(Some(path)) => serde_json::json!({ "path": path, "line": reply.line }),
        Some(None) => serde_json::Value::from(CURRENT_OUTSIDE),
        None => serde_json::Value::from(CURRENT_NO_FILE),
    };
    let mut answer = serde_json::json!({
        "root": cx.root.to_string_lossy(),
        "buffers": &told[..kept],
        "other_buffers": other,
        "current": current,
        "mode": mode_word(&reply.mode),
        "selection": selection,
    });
    let cut = u32::try_from(told.len() - kept)
        .unwrap_or(u32::MAX)
        .saturating_add(reply.left_out);
    if cut > 0 {
        answer["buffers_left_out"] = serde_json::Value::from(cut);
    }
    answer.to_string()
}

/// The sentence or the JSON a chunk's answer comes to for the agent. Every path is judged here, whatever the
/// chunk filtered: one that fails joins the count of the others.
pub fn result_of(cx: &ToolCx<'_>, value: &Value) -> ToolResult {
    if field(value, "expired").is_some() {
        return failure(words::NOT_READ);
    }
    let Some(reply) = parse(value) else {
        return failure("the editor's answer could not be read; nothing is known of its buffers");
    };
    let mut other = reply.other;
    let mut told = Vec::new();
    for buffer in &reply.buffers {
        match inside(cx, &buffer.path) {
            Some(path) => told.push(serde_json::json!({
                "path": path,
                "modified": buffer.modified,
                "lines": buffer.lines,
                "filetype": buffer.filetype,
                "shown": buffer.shown,
            })),
            None => other = other.saturating_add(1),
        }
    }
    let selection = selection(cx);
    let ready = |kept: usize| answer_text(cx, &reply, &told, other, kept, &selection);
    // Whole buffers come off the end until the answer fits. What fits only shrinks as buffers are added, so the
    // largest count that does is found by halving.
    let all = told.len();
    let mut kept = all;
    if encoded_len(&ready(all)) > ANSWER_MAX_ENCODED {
        let (mut fits, mut over) = (0, all);
        while over - fits > 1 {
            let middle = fits + (over - fits) / 2;
            if encoded_len(&ready(middle)) <= ANSWER_MAX_ENCODED {
                fits = middle;
            } else {
                over = middle;
            }
        }
        kept = fits;
    }
    ToolResult {
        text: ready(kept),
        is_error: false,
    }
}

pub fn send(rpc: &dyn EditorRpc, stamp: &Stamp, root: &Path) -> Pending {
    let args = vec![(Value::from("root"), path_value(root))];
    rpc.exec_lua(EDITOR_STATE_LUA, vec![stamp.to_value(), Value::Map(args)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_context::{EditorContext, Selection, CONTENT_LIMIT, TRUNCATION_MARKER};
    use crate::editor_tools::memory::Memory;
    use crate::editor_tools::step::{AgentCall, EditorSight};
    use crate::test_scratch_dir::ScratchDir;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use std::fs;

    struct Project {
        _scratch: ScratchDir,
        root: PathBuf,
        beside: PathBuf,
    }

    fn project(label: &str) -> Project {
        let scratch = ScratchDir::new("editor-tools-state", label);
        let base = fs::canonicalize(&*scratch).unwrap();
        let root = base.join("project");
        let beside = base.join("beside");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&beside).unwrap();
        fs::write(root.join("src/a.rs"), "a\n").unwrap();
        fs::write(beside.join("b.rs"), "b").unwrap();
        Project {
            _scratch: scratch,
            root,
            beside,
        }
    }

    fn told(
        project: &Project,
        reach: EditorReach,
        context: Option<&EditorContext>,
        value: &Value,
    ) -> (ToolResult, serde_json::Value) {
        let memory = Memory::default();
        let cx = ToolCx {
            mode: PermissionMode::Manual,
            trust: ProjectTrust::Trusted,
            root: &project.root,
            home: None,
            reach,
            sight: EditorSight::OnScreen,
            context,
            tab: 1,
            session: "session",
            call: "call",
            agent_call: AgentCall::Host,
            memory: &memory,
        };
        let result = result_of(&cx, value);
        let json = serde_json::from_str(&result.text).unwrap_or(serde_json::Value::Null);
        (result, json)
    }

    fn map(entries: Vec<(&str, Value)>) -> Value {
        Value::Map(entries.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
    }

    fn buffer(path: &Path, modified: bool) -> Value {
        map(vec![
            ("path", Value::from(path.to_str().unwrap())),
            ("modified", Value::from(modified)),
            ("lines", Value::from(3)),
            ("filetype", Value::from("rust")),
            ("shown", Value::from(true)),
        ])
    }

    /// A chunk's answer for the buffers given, the person on line 2 in normal mode.
    fn reply(buffers: Vec<Value>, other: u32, current: Option<&Path>) -> Value {
        let mut here = vec![("line", Value::from(2))];
        if let Some(path) = current {
            here.push(("path", Value::from(path.to_str().unwrap())));
        }
        map(vec![
            ("buffers", Value::Array(buffers)),
            ("other", Value::from(other)),
            ("left_out", Value::from(0)),
            ("current", map(here)),
            ("mode", Value::from("n")),
        ])
    }

    fn selected(file: &Path, text: &str) -> EditorContext {
        EditorContext {
            file: file.to_str().unwrap().to_owned(),
            selection: Some(Selection {
                start_line: 3,
                end_line: 5,
                text: text.to_owned(),
            }),
        }
    }

    #[test]
    fn a_selection_inside_carries_its_text() {
        let p = project("selection-inside");
        let context = selected(&p.root.join("src/a.rs"), "let x = 1;\nlet y = 2;");
        let (result, json) = told(&p, EditorReach::Own, Some(&context), &reply(vec![], 0, None));
        assert!(!result.is_error, "{}", result.text);
        assert_eq!(
            json["selection"],
            serde_json::json!({
                "path": "src/a.rs", "start_line": 3, "end_line": 5, "text": "let x = 1;\nlet y = 2;",
            })
        );
    }

    #[test]
    fn a_selection_outside_carries_no_path_or_text() {
        let p = project("selection-outside");
        let context = selected(&p.beside.join("b.rs"), "secret words");
        let (result, json) = told(&p, EditorReach::Own, Some(&context), &reply(vec![], 0, None));
        assert_eq!(json["selection"], "a selection in a file outside the project");
        assert!(!result.text.contains("secret words"));
        assert!(!result.text.contains("b.rs"));
    }

    #[test]
    fn no_selection_is_none_not_empty() {
        let p = project("selection-none");
        let (_, json) = told(&p, EditorReach::Own, None, &reply(vec![], 0, None));
        assert!(json["selection"].is_null(), "{json}");
        let nothing = EditorContext {
            file: p.root.join("src/a.rs").to_str().unwrap().to_owned(),
            selection: None,
        };
        let (_, json) = told(&p, EditorReach::Own, Some(&nothing), &reply(vec![], 0, None));
        assert!(json["selection"].is_null(), "{json}");
    }

    #[test]
    fn a_link_under_the_root_that_leaves_it_is_counted() {
        let p = project("link");
        let link = p.root.join("src/out.rs");
        std::os::unix::fs::symlink(p.beside.join("b.rs"), &link).unwrap();
        let value = reply(
            vec![buffer(&p.root.join("src/a.rs"), false), buffer(&link, true)],
            2,
            None,
        );
        let (_, json) = told(&p, EditorReach::Own, None, &value);
        let names: Vec<_> = json["buffers"].as_array().unwrap().iter().map(|b| &b["path"]).collect();
        assert_eq!(names, ["src/a.rs"]);
        assert_eq!(json["other_buffers"], 3, "the two outside and the link: {json}");
        assert!(!json.to_string().contains("out.rs"));
    }

    #[test]
    fn a_window_2_tab_reads_the_forwarded_context() {
        let p = project("window-2");
        let context = selected(&p.root.join("src/a.rs"), "forwarded");
        let value = reply(
            vec![buffer(&p.root.join("src/a.rs"), true)],
            0,
            Some(&p.root.join("src/a.rs")),
        );
        let (result, json) = told(&p, EditorReach::Link, Some(&context), &value);
        assert!(!result.is_error, "{}", result.text);
        assert_eq!(json["selection"]["text"], "forwarded");
        assert_eq!(json["current"], serde_json::json!({ "line": 2, "path": "src/a.rs" }));
    }

    #[test]
    fn a_long_selection_carries_the_limit_and_its_marker() {
        let p = project("long");
        let context = selected(&p.root.join("src/a.rs"), &"x".repeat(CONTENT_LIMIT + 50));
        let (_, json) = told(&p, EditorReach::Own, Some(&context), &reply(vec![], 0, None));
        let text = json["selection"]["text"].as_str().unwrap();
        assert!(text.ends_with(TRUNCATION_MARKER), "{text:?}");
        assert_eq!(text.chars().count(), CONTENT_LIMIT + TRUNCATION_MARKER.chars().count());
    }

    #[test]
    fn an_unsaved_new_file_is_listed() {
        let p = project("new-file");
        let fresh = p.root.join("src/not_yet_saved.rs");
        let value = reply(vec![buffer(&fresh, true)], 0, Some(&fresh));
        let (_, json) = told(&p, EditorReach::Own, None, &value);
        assert_eq!(json["buffers"][0]["path"], "src/not_yet_saved.rs");
        assert_eq!(json["buffers"][0]["modified"], true);
        assert_eq!(json["other_buffers"], 0);
        assert_eq!(json["current"]["path"], "src/not_yet_saved.rs");
    }

    #[test]
    fn a_current_name_that_is_not_in_the_project_is_not_named() {
        let p = project("current");
        let outside = p.beside.join("b.rs");
        let (_, json) = told(&p, EditorReach::Own, None, &reply(vec![], 0, Some(&outside)));
        assert_eq!(json["current"], "a file outside the project", "no name and no line");
        let terminal = Path::new("term://project//1:bash");
        let (_, json) = told(&p, EditorReach::Own, None, &reply(vec![], 0, Some(terminal)));
        assert_eq!(json["current"], "a file outside the project");
        let (_, json) = told(&p, EditorReach::Own, None, &reply(vec![], 0, None));
        assert_eq!(json["current"], "a buffer with no file");
    }

    #[test]
    fn the_answer_fits() {
        let p = project("fits");
        let deep = "d/".repeat(1500);
        let buffers: Vec<Value> = (0..500)
            .map(|n| buffer(&p.root.join(format!("{deep}f{n}.rs")), false))
            .collect();
        let (result, json) = told(&p, EditorReach::Own, None, &reply(buffers, 0, None));
        assert!(!result.is_error, "{}", result.text);
        assert!(encoded_len(&result.text) <= ANSWER_MAX_ENCODED);
        let kept = json["buffers"].as_array().unwrap().len();
        assert!(kept > 0 && kept < 500, "{kept}");
        assert_eq!(json["buffers_left_out"], 500 - kept);
    }

    #[test]
    fn the_modes_have_their_words() {
        for (raw, word) in [
            ("n", "normal"),
            ("no", "normal"),
            ("nt", "normal"),
            ("i", "insert"),
            ("R", "insert"),
            ("v", "visual"),
            ("V", "visual"),
            ("\u{16}", "visual"),
            ("s", "visual"),
            ("S", "visual"),
            ("c", "command"),
            ("t", "terminal"),
            ("!", "normal"),
            ("", "normal"),
        ] {
            assert_eq!(mode_word(raw), word, "{raw:?}");
        }
    }

    #[test]
    fn an_expired_chunk_and_an_unreadable_answer_are_errors() {
        let p = project("errors");
        let (expired, _) = told(&p, EditorReach::Own, None, &map(vec![("expired", Value::from(true))]));
        assert!(expired.is_error);
        assert_eq!(expired.text, words::NOT_READ);
        let (unreadable, _) = told(&p, EditorReach::Own, None, &Value::Nil);
        assert!(unreadable.is_error);
    }
}
