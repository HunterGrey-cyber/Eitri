//! `read_buffer`: the text as the editor holds it.

use std::path::{Path, PathBuf};

use rmpv::Value;

use super::diagnostics::name_of;
use super::path_value;
use crate::editor_lines::{self, FileFormat};
use crate::editor_rpc::EditorRpc;
use crate::editor_tools::cards::{CardLine, CardRequest};
use crate::editor_tools::catalog::ToolRow;
use crate::editor_tools::gate::{verdict, Need, Verdict};
use crate::editor_tools::paths::{self, Target};
use crate::editor_tools::stamp::{Stamp, EDITOR_BUDGET};
use crate::editor_tools::step::{
    encoded_len, EditorFailure, EditorReach, Step, StepEvent, Tool, ToolCx, ToolError, ToolResult, ToolRun,
    ANSWER_MAX_ENCODED,
};
use crate::editor_tools::way::EditorCall;
use crate::editor_tools::{args, words};
use crate::nvim_rpc::Pending;

pub const ROW: ToolRow = ToolRow {
    name: "read_buffer",
    description: "The text of a file as the editor holds it, unsaved changes included, for a file already open \
                  there. The answer says whether it differs from the file on disk. For a file not open, use Read.",
    schema: r#"{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "The file, absolute or relative to the project root." },
    "start_line": { "type": "integer", "minimum": 1, "description": "The first line to read, from 1." },
    "end_line": { "type": "integer", "minimum": 1, "description": "The last line to read." }
  },
  "required": ["path"],
  "additionalProperties": false
}"#,
    always_load: true,
    default_on: true,
    in_untrusted: true,
};

pub const READ_BUFFER_LUA: &str = chunk!("read_buffer.lua");

pub struct ReadBuffer;

impl Tool for ReadBuffer {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    fn begin(&self, args: &serde_json::Value, _cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        let raw = args::path(args, "path")?.to_owned();
        let (start, end) = args::line_range(args, "start_line", "end_line")?;
        Ok(Box::new(ReadRun {
            raw,
            start,
            end,
            stage: Stage::Judging,
        }))
    }
}

enum Stage {
    /// Nothing has run.
    Judging,
    /// A card asks whether a buffer of a file outside the project may be read; `shown` is where the card said it
    /// leads.
    OutsideCard {
        shown: PathBuf,
    },
    /// A call is out for this path. `outside`: it is an outside file that may be read, and the only one.
    Called {
        path: PathBuf,
        outside: bool,
    },
    Done,
}

struct ReadRun {
    raw: String,
    start: Option<u32>,
    end: Option<u32>,
    stage: Stage,
}

fn failure(text: impl Into<String>) -> ToolResult {
    ToolResult {
        text: text.into(),
        is_error: true,
    }
}

impl ReadRun {
    fn call(&mut self, path: PathBuf, outside: bool) -> Step {
        let call = EditorCall::ReadBuffer {
            path: path.clone(),
            start: self.start,
            end: self.end,
        };
        self.stage = Stage::Called { path, outside };
        Step::Call {
            call,
            budget: EDITOR_BUDGET,
        }
    }

    fn done(&mut self, result: ToolResult) -> Step {
        self.stage = Stage::Done;
        Step::Done(result)
    }

    fn judge(&mut self, cx: &ToolCx<'_>) -> Step {
        match paths::judge_for(cx, &self.raw) {
            Err(refused) => self.done(failure(refused.0)),
            Ok(Target::Refused(why)) => self.done(failure(why)),
            // A path with nothing on disk is asked all the same: an unsaved new file may be a buffer.
            Ok(Target::Inside(path) | Target::Missing(path)) => self.call(path, false),
            Ok(Target::Outside { shown }) => match verdict(Need::OutsideProject, cx.mode, cx.trust) {
                Verdict::Allow => self.outside_approved(cx, None),
                Verdict::Card => {
                    self.stage = Stage::OutsideCard { shown: shown.clone() };
                    Step::Ask(CardRequest::new(
                        format!("Read {} from your editor?", shown.display()),
                        vec![CardLine::Plain("It is outside the project".to_owned())],
                    ))
                }
            },
        }
    }

    /// An outside path that may be read: where it leads, with every link followed. After a card, `approved` is the
    /// place the card named: a path that leads elsewhere by now was not approved.
    fn outside_approved(&mut self, cx: &ToolCx<'_>, approved: Option<&Path>) -> Step {
        let path = match paths::outside_target(cx.root, &self.raw) {
            Ok(path) => path,
            Err(refused) => return self.done(failure(refused.0)),
        };
        if let Some(approved) = approved {
            if path != approved {
                return self.done(failure(format!(
                    "{} now leads somewhere other than {}; nothing was read",
                    self.raw,
                    approved.display()
                )));
            }
        }
        self.call(path, true)
    }

    fn card_answered(&mut self, allowed: bool, cx: &ToolCx<'_>) -> Step {
        let Stage::OutsideCard { shown } = std::mem::replace(&mut self.stage, Stage::Done) else {
            return self.done(failure("the editor was asked nothing"));
        };
        if !allowed {
            return self.done(failure(words::NOT_ALLOWED));
        }
        self.outside_approved(cx, Some(shown.as_path()))
    }

    fn answered(&mut self, answer: Result<Value, EditorFailure>, cx: &ToolCx<'_>) -> Step {
        let Stage::Called { path, outside } = std::mem::replace(&mut self.stage, Stage::Done) else {
            return Step::Done(failure("the editor was asked nothing"));
        };
        let result = match answer {
            Ok(value) if !may_tell(cx, &value, outside.then_some(path.as_path())) => failure(format!(
                "the editor's buffer for {} is another file's; nothing was read",
                self.raw
            )),
            Ok(value) => result_of(&path, self.end, &value),
            Err(EditorFailure::NotAnswered) => failure(words::NOT_READ),
            Err(EditorFailure::Unavailable(_)) if cx.reach == EditorReach::Link => failure(words::WINDOW_CLOSED),
            Err(EditorFailure::Unavailable(text) | EditorFailure::Refused(text)) => failure(text),
        };
        Step::Done(result)
    }
}

impl ToolRun for ReadRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match event {
            StepEvent::Start if matches!(self.stage, Stage::Judging) => self.judge(cx),
            StepEvent::Answer(answer) => self.answered(answer, cx),
            StepEvent::Card(answer) => self.card_answered(answer.allowed(), cx),
            StepEvent::Start | StepEvent::Poll => Step::Done(failure("the editor was asked nothing")),
        }
    }
}

/// Whether the lines of a chunk's answer may be told. What was judged before the call is a path, and the chunk finds a
/// buffer by that path as it runs, through any link made since: so the buffer it read is judged here, by its own name.
/// An answer with no lines tells nothing of a buffer.
fn may_tell(cx: &ToolCx<'_>, value: &Value, outside: Option<&Path>) -> bool {
    field(value, "lines").is_none()
        || field(value, "name")
            .and_then(name_of)
            .is_some_and(|name| paths::buffer_told(cx, &name, outside))
}

/// What the chunk read of a buffer: whole lines, from line `first`.
struct Lines {
    lines: Vec<Vec<u8>>,
    first: u32,
    line_count: u32,
    modified: bool,
    fileformat: String,
    eol: bool,
}

enum Reply {
    Expired,
    NotLoaded,
    Refused(String),
    Lines(Lines),
    Unreadable,
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

/// A word the editor named, kept to plain letters so it can sit in a sentence.
fn word(value: &Value) -> String {
    let text = value.as_str().unwrap_or("unknown");
    text.chars().filter(char::is_ascii_alphanumeric).take(16).collect()
}

fn read_reply(value: &Value) -> Reply {
    if field(value, "expired").is_some() {
        return Reply::Expired;
    }
    if field(value, "not_loaded").is_some() {
        return Reply::NotLoaded;
    }
    if let Some(kind) = field(value, "refused") {
        return Reply::Refused(word(kind));
    }
    let read = || -> Option<Lines> {
        let lines = match field(value, "lines")? {
            // An empty Lua table may arrive as either.
            Value::Map(entries) if entries.is_empty() => Vec::new(),
            Value::Array(items) => items
                .iter()
                .map(|item| match item {
                    // nvim sends a Lua string as msgpack str whatever its bytes are; they are kept.
                    Value::String(text) => Some(text.as_bytes().to_vec()),
                    Value::Binary(bytes) => Some(bytes.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?,
            _ => return None,
        };
        Some(Lines {
            lines,
            first: count(value, "first")?,
            line_count: count(value, "line_count")?,
            modified: field(value, "modified")?.as_bool()?,
            fileformat: word(field(value, "fileformat")?),
            eol: field(value, "eol")?.as_bool()?,
        })
    };
    read().map_or(Reply::Unreadable, Reply::Lines)
}

/// The sentence for lines the answer does not hold.
fn marker(from: u32, to: u32, total: u32) -> String {
    let which = if from == to {
        format!("line {from}")
    } else {
        format!("lines {from} to {to}")
    };
    format!("{which} of {total} left out: a call holds at most 4000 lines and 32 KiB; read on with start_line {from}")
}

/// The answer's JSON with the first `kept` lines of the reply, or `None` for a buffer whose lines cannot be
/// turned back into file bytes.
fn answer_text(shown: &Path, end: Option<u32>, reply: &Lines, kept: usize) -> Option<String> {
    let format = editor_lines::parse_file_format(&reply.fileformat);
    let last = reply.first + u32::try_from(kept).unwrap_or(u32::MAX) - 1;
    let ends_buffer = kept > 0 && last == reply.line_count;
    let bytes = editor_lines::buffer_range_bytes(&reply.lines[..kept], format, reply.eol, ends_buffer)?;
    let wanted = end.map_or(reply.line_count, |end| end.min(reply.line_count));
    let mut answer = serde_json::json!({
        "path": shown.to_string_lossy(),
        "modified": reply.modified,
        "first_line": reply.first,
        "last_line": last,
        "text": String::from_utf8_lossy(&bytes),
    });
    if last < wanted {
        answer["left_out"] = serde_json::Value::from(marker(last + 1, wanted, reply.line_count));
    }
    Some(answer.to_string())
}

fn lines_result(shown: &Path, end: Option<u32>, reply: &Lines) -> ToolResult {
    if reply.first > reply.line_count {
        return failure(format!(
            "start_line {} is past the end of the buffer, which has {} lines",
            reply.first, reply.line_count
        ));
    }
    if editor_lines::parse_file_format(&reply.fileformat) == FileFormat::Other {
        return failure(format!("this buffer's fileformat is {}; use Read", reply.fileformat));
    }
    let ready = |kept: usize| answer_text(shown, end, reply, kept).expect("a unix or dos buffer");
    // JSON-escaped, twice, a line can cost seven times its bytes: whole lines come off the end until the
    // answer fits. What fits only shrinks as lines are added, so the largest count that does is found by halving.
    let all = reply.lines.len();
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

/// The sentence or the text a chunk's answer comes to for the agent. `shown` is the path the chunk was asked
/// about and `end` the call's `end_line`.
pub fn result_of(shown: &Path, end: Option<u32>, value: &Value) -> ToolResult {
    let name = shown.display();
    match read_reply(value) {
        Reply::Expired => failure(words::NOT_READ),
        Reply::NotLoaded => failure(format!("{name} is not open in the editor; use Read")),
        Reply::Refused(kind) => failure(format!("{name} is a {kind} buffer, not a file; use Read")),
        Reply::Lines(reply) => lines_result(shown, end, &reply),
        Reply::Unreadable => failure("the editor's answer could not be read; nothing is known of the buffer"),
    }
}

pub fn send(rpc: &dyn EditorRpc, stamp: &Stamp, path: &Path, start: Option<u32>, end: Option<u32>) -> Pending {
    let mut args = vec![(Value::from("path"), path_value(path))];
    if let Some(start) = start {
        args.push((Value::from("start"), Value::from(start)));
    }
    if let Some(end) = end {
        args.push((Value::from("end"), Value::from(end)));
    }
    rpc.exec_lua(READ_BUFFER_LUA, vec![stamp.to_value(), Value::Map(args)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tools::memory::Memory;
    use crate::editor_tools::step::{AgentCall, EditorSight};
    use crate::test_scratch_dir::ScratchDir;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use serde_json::json;
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    struct Project {
        _scratch: ScratchDir,
        root: PathBuf,
        beside: PathBuf,
    }

    fn project(label: &str) -> Project {
        let scratch = ScratchDir::new("editor-tools-read", label);
        let base = fs::canonicalize(&*scratch).unwrap();
        let root = base.join("project");
        let beside = base.join("beside");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&beside).unwrap();
        fs::write(root.join("src/a.rs"), "a\nb\nc\n").unwrap();
        fs::write(beside.join("b.rs"), "b").unwrap();
        Project {
            _scratch: scratch,
            root,
            beside,
        }
    }

    fn with_cx<R>(root: &Path, mode: PermissionMode, f: impl FnOnce(&ToolCx<'_>) -> R) -> R {
        let memory = Memory::default();
        let cx = ToolCx {
            mode,
            trust: ProjectTrust::Trusted,
            root,
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
        f(&cx)
    }

    fn run_of(raw: &str) -> ReadRun {
        ReadRun {
            raw: raw.to_owned(),
            start: None,
            end: None,
            stage: Stage::Judging,
        }
    }

    fn start(project: &Project, mode: PermissionMode, raw: &str) -> (ReadRun, Step) {
        let mut run = run_of(raw);
        let step = with_cx(&project.root, mode, |cx| run.step(StepEvent::Start, cx));
        (run, step)
    }

    fn the_call(step: &Step) -> PathBuf {
        match step {
            Step::Call {
                call: EditorCall::ReadBuffer { path, .. },
                ..
            } => path.clone(),
            other => panic!("expected a call to read a buffer, got {other:?}"),
        }
    }

    fn map(entries: Vec<(&str, Value)>) -> Value {
        Value::Map(entries.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
    }

    /// A chunk's answer for `lines` read from line 1 of a buffer of `line_count` lines.
    fn reply(lines: &[&[u8]], line_count: u32, format: &str) -> Value {
        map(vec![
            (
                "lines",
                Value::Array(lines.iter().map(|line| Value::Binary(line.to_vec())).collect()),
            ),
            ("first", Value::from(1)),
            ("line_count", Value::from(line_count)),
            ("modified", Value::from(true)),
            ("fileformat", Value::from(format)),
            ("eol", Value::from(true)),
        ])
    }

    fn parsed(result: &ToolResult) -> serde_json::Value {
        assert!(!result.is_error, "{}", result.text);
        serde_json::from_str(&result.text).unwrap()
    }

    #[test]
    fn outside_is_a_card_and_bypass_is_not() {
        let project = project("outside");
        let outside = project.beside.join("b.rs");
        let raw = outside.to_str().unwrap();
        for mode in [PermissionMode::Manual, PermissionMode::Auto] {
            let (mut run, step) = start(&project, mode, raw);
            let Step::Ask(card) = &step else {
                panic!("expected a card, got {step:?}");
            };
            assert_eq!(card.title, format!("Read {} from your editor?", outside.display()));
            assert_eq!(card.body, vec![CardLine::Plain("It is outside the project".to_owned())]);
            let denied = with_cx(&project.root, mode, |cx| run.card_answered(false, cx));
            assert!(
                matches!(denied, Step::Done(ToolResult { ref text, is_error: true }) if text == words::NOT_ALLOWED)
            );

            let (mut run, _) = start(&project, mode, raw);
            let allowed = with_cx(&project.root, mode, |cx| run.card_answered(true, cx));
            assert_eq!(the_call(&allowed), outside, "{mode:?}");
        }
        let (_, step) = start(&project, PermissionMode::Bypass, raw);
        assert_eq!(the_call(&step), outside);
    }

    #[test]
    fn the_outside_card_approves_only_the_place_it_named() {
        let project = project("outside-retarget");
        let named = project.beside.join("b.rs");
        let other = project.beside.join("other.txt");
        fs::write(&other, "other").unwrap();
        let link = project.root.join("link.rs");
        std::os::unix::fs::symlink(&named, &link).unwrap();

        let (mut run, step) = start(&project, PermissionMode::Manual, "link.rs");
        assert!(matches!(step, Step::Ask(_)), "{step:?}");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        let allowed = with_cx(&project.root, PermissionMode::Manual, |cx| run.card_answered(true, cx));
        assert!(
            matches!(&allowed, Step::Done(ToolResult { text, is_error: true }) if text.contains("now leads somewhere other")),
            "{allowed:?}"
        );

        // Left alone, the same card reads the place it named.
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&named, &link).unwrap();
        let (mut run, _) = start(&project, PermissionMode::Manual, "link.rs");
        let allowed = with_cx(&project.root, PermissionMode::Manual, |cx| run.card_answered(true, cx));
        assert_eq!(the_call(&allowed), named);
    }

    #[test]
    fn two_outside_names_that_read_alike_are_not_one_place() {
        let project = project("outside-alike");
        let named = project.beside.join(OsStr::from_bytes(b"secret\xff"));
        let other = project.beside.join(OsStr::from_bytes(b"secret\xfe"));
        fs::write(&named, "named").unwrap();
        fs::write(&other, "other").unwrap();
        assert_eq!(named.display().to_string(), other.display().to_string());
        let link = project.root.join("link.rs");
        std::os::unix::fs::symlink(&named, &link).unwrap();

        let (mut run, step) = start(&project, PermissionMode::Manual, "link.rs");
        assert!(matches!(step, Step::Ask(_)), "{step:?}");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        let allowed = with_cx(&project.root, PermissionMode::Manual, |cx| run.card_answered(true, cx));
        assert!(
            matches!(&allowed, Step::Done(ToolResult { text, is_error: true }) if text.contains("now leads somewhere other")),
            "{allowed:?}"
        );
    }

    /// A chunk's answer of the text `secret`, read from the buffer called `name`.
    fn read_from(name: &Path) -> Value {
        let mut answer = reply(&[b"secret"], 1, "unix");
        let Value::Map(entries) = &mut answer else {
            unreachable!()
        };
        entries.push((Value::from("name"), path_value(name)));
        answer
    }

    fn answered(project: &Project, run: &mut ReadRun, answer: Value) -> ToolResult {
        let step = with_cx(&project.root, PermissionMode::Manual, |cx| {
            run.step(StepEvent::Answer(Ok(answer)), cx)
        });
        let Step::Done(result) = step else {
            panic!("expected the end of the call, got {step:?}");
        };
        result
    }

    #[test]
    fn a_project_path_that_found_an_outside_buffer_tells_nothing_of_it() {
        let project = project("found-outside");
        let outside = project.beside.join("b.rs");
        let (mut run, _) = start(&project, PermissionMode::Manual, "src/a.rs");
        let result = answered(&project, &mut run, read_from(&outside));
        assert!(result.is_error, "{result:?}");
        assert!(!result.text.contains("secret"), "{result:?}");
        assert!(
            !result.text.contains("b.rs"),
            "the outside file goes unnamed: {result:?}"
        );
    }

    #[test]
    fn a_project_buffer_named_through_a_link_that_leaves_tells_nothing() {
        let project = project("found-link");
        let link = project.root.join("link.rs");
        std::os::unix::fs::symlink(project.beside.join("b.rs"), &link).unwrap();
        let (mut run, _) = start(&project, PermissionMode::Manual, "src/a.rs");
        let result = answered(&project, &mut run, read_from(&link));
        assert!(result.is_error && !result.text.contains("secret"), "{result:?}");
    }

    #[test]
    fn lines_from_no_named_buffer_are_not_told() {
        let project = project("found-nameless");
        let (mut run, _) = start(&project, PermissionMode::Manual, "src/a.rs");
        let result = answered(&project, &mut run, reply(&[b"secret"], 1, "unix"));
        assert!(result.is_error && !result.text.contains("secret"), "{result:?}");
    }

    #[test]
    fn a_buffer_of_the_project_is_told_by_any_name_that_stays_inside() {
        let project = project("found-inside");
        let link = project.root.join("again.rs");
        std::os::unix::fs::symlink(project.root.join("src/a.rs"), &link).unwrap();
        for name in [project.root.join("src/a.rs"), link, project.root.join("src/unsaved.rs")] {
            let (mut run, _) = start(&project, PermissionMode::Manual, "src/a.rs");
            let told = parsed(&answered(&project, &mut run, read_from(&name)));
            assert_eq!(told["text"], "secret\n", "{}", name.display());
        }
    }

    #[test]
    fn an_allowed_outside_file_is_told_and_no_other_outside_buffer() {
        let project = project("found-allowed");
        let allowed = project.beside.join("b.rs");
        let other = project.beside.join("other.txt");
        fs::write(&other, "other").unwrap();
        let through = project.beside.join("through.rs");
        std::os::unix::fs::symlink(&allowed, &through).unwrap();
        let raw = allowed.to_str().unwrap();
        for (name, told) in [(&allowed, true), (&through, true), (&other, false)] {
            let (mut run, _) = start(&project, PermissionMode::Manual, raw);
            let called = with_cx(&project.root, PermissionMode::Manual, |cx| run.card_answered(true, cx));
            assert_eq!(the_call(&called), allowed);
            let result = answered(&project, &mut run, read_from(name));
            assert_eq!(result.is_error, !told, "{}: {result:?}", name.display());
            assert_eq!(result.text.contains("secret"), told, "{}", name.display());
        }
        let (mut run, step) = start(&project, PermissionMode::Bypass, raw);
        assert_eq!(the_call(&step), allowed);
        assert!(
            answered(&project, &mut run, read_from(&other)).is_error,
            "bypass reaches the file it named"
        );
    }

    #[test]
    fn a_path_inside_and_an_unsaved_new_file_both_ask_the_editor() {
        let project = project("inside");
        let (_, step) = start(&project, PermissionMode::Manual, "src/a.rs");
        assert_eq!(the_call(&step), project.root.join("src/a.rs"));
        let (_, step) = start(&project, PermissionMode::Manual, "src/new.rs");
        assert_eq!(the_call(&step), project.root.join("src/new.rs"));
    }

    #[test]
    fn the_marker_names_the_left_out_lines() {
        let lines: Vec<Vec<u8>> = (1..=3).map(|n| format!("line {n}").into_bytes()).collect();
        let refs: Vec<&[u8]> = lines.iter().map(Vec::as_slice).collect();
        let answer = reply(&refs, 10, "unix");
        let shown = Path::new("/p/a.rs");

        let whole = parsed(&result_of(shown, Some(3), &answer));
        assert!(whole.get("left_out").is_none(), "{whole}");
        assert_eq!(whole["last_line"], 3);

        let cut = parsed(&result_of(shown, None, &answer));
        assert_eq!(cut["first_line"], 1);
        assert_eq!(cut["last_line"], 3);
        assert_eq!(cut["text"], "line 1\nline 2\nline 3\n");
        let marker = cut["left_out"].as_str().unwrap();
        assert!(marker.starts_with("lines 4 to 10 of 10 left out"), "{marker}");
        assert!(marker.ends_with("start_line 4"), "{marker}");

        let one = parsed(&result_of(shown, Some(4), &answer));
        assert!(one["left_out"].as_str().unwrap().starts_with("line 4 of 10 left out"));
    }

    fn without_final_newline(mut answer: Value) -> Value {
        let Value::Map(entries) = &mut answer else {
            unreachable!()
        };
        entries.retain(|(key, _)| key.as_str() != Some("eol"));
        entries.push((Value::from("eol"), Value::from(false)));
        answer
    }

    #[test]
    fn the_last_line_of_a_buffer_without_a_final_newline_has_none() {
        let whole = without_final_newline(reply(&[b"a", b"b"], 2, "unix"));
        assert_eq!(parsed(&result_of(Path::new("/p/a"), None, &whole))["text"], "a\nb");
        let first_only = without_final_newline(reply(&[b"a"], 2, "unix"));
        assert_eq!(
            parsed(&result_of(Path::new("/p/a"), Some(1), &first_only))["text"],
            "a\n",
            "only the buffer's own end may lack one"
        );
    }

    #[test]
    fn invalid_utf8_does_not_fail_the_call() {
        let answer = reply(&[b"ok", b"bad \xff\xfe bytes"], 2, "unix");
        let result = parsed(&result_of(Path::new("/p/a"), None, &answer));
        assert_eq!(result["text"], "ok\nbad \u{fffd}\u{fffd} bytes\n");
    }

    #[test]
    fn a_buffer_of_quotes_and_control_bytes_still_fits() {
        // 4000 lines of 64 bytes, escaped twice, is far over the answer's limit.
        let line: Vec<u8> = (0..64).map(|n| if n % 2 == 0 { b'"' } else { 1 }).collect();
        let refs: Vec<&[u8]> = (0..4000).map(|_| line.as_slice()).collect();
        let result = result_of(Path::new("/p/a"), None, &reply(&refs, 4000, "unix"));
        assert!(!result.is_error);
        assert!(
            encoded_len(&result.text) <= ANSWER_MAX_ENCODED,
            "{} over {ANSWER_MAX_ENCODED}",
            encoded_len(&result.text)
        );
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        let last = answer["last_line"].as_u64().unwrap();
        assert!(last > 100 && last < 4000, "{last}");
        let text = answer["text"].as_str().unwrap();
        assert_eq!(text.lines().count() as u64, last, "whole lines only");
        let marker = answer["left_out"].as_str().unwrap();
        assert!(marker.starts_with(&format!("lines {} to 4000", last + 1)), "{marker}");
        // One line more would not have fitted.
        let more = answer_text(Path::new("/p/a"), None, &read_lines(&refs, 4000), last as usize + 1).unwrap();
        assert!(encoded_len(&more) > ANSWER_MAX_ENCODED);
    }

    fn read_lines(lines: &[&[u8]], line_count: u32) -> Lines {
        Lines {
            lines: lines.iter().map(|line| line.to_vec()).collect(),
            first: 1,
            line_count,
            modified: false,
            fileformat: "unix".to_owned(),
            eol: true,
        }
    }

    #[test]
    fn the_editor_s_other_answers_become_sentences() {
        let shown = Path::new("/p/a.rs");
        let sentence = |value: Value| {
            let result = result_of(shown, None, &value);
            assert!(result.is_error);
            result.text
        };
        assert_eq!(sentence(map(vec![("expired", Value::from(true))])), words::NOT_READ);
        assert_eq!(
            sentence(map(vec![("not_loaded", Value::from(true))])),
            "/p/a.rs is not open in the editor; use Read"
        );
        assert_eq!(
            sentence(map(vec![("refused", Value::from("terminal"))])),
            "/p/a.rs is a terminal buffer, not a file; use Read"
        );
        assert_eq!(
            sentence(reply(&[b"a"], 1, "mac")),
            "this buffer's fileformat is mac; use Read"
        );
        assert!(sentence(Value::Nil).contains("could not be read"));
        let mut past = reply(&[], 3, "unix");
        let Value::Map(entries) = &mut past else { unreachable!() };
        entries.retain(|(key, _)| key.as_str() != Some("first"));
        entries.push((Value::from("first"), Value::from(9)));
        assert_eq!(
            sentence(past),
            "start_line 9 is past the end of the buffer, which has 3 lines"
        );
    }

    #[test]
    fn the_arguments_are_read_by_type() {
        let project = project("args");
        let begin = |args: serde_json::Value| {
            with_cx(&project.root, PermissionMode::Manual, |cx| {
                ReadBuffer.begin(&args, cx).map(|_| ()).unwrap_err().0
            })
        };
        assert_eq!(begin(json!({})), "path is required");
        assert_eq!(begin(json!({"path": "a", "start_line": 0})), "start_line starts at 1");
        assert_eq!(
            begin(json!({"path": "a", "start_line": 5, "end_line": 4})),
            "end_line may not precede start_line"
        );
        with_cx(&project.root, PermissionMode::Manual, |cx| {
            assert!(ReadBuffer
                .begin(&json!({"path": "a", "start_line": 2, "end_line": 4}), cx)
                .is_ok());
        });
    }

    #[test]
    fn an_unavailable_editor_is_a_sentence() {
        let project = project("unavailable");
        let (mut run, _) = start(&project, PermissionMode::Manual, "src/a.rs");
        let step = with_cx(&project.root, PermissionMode::Manual, |cx| {
            run.step(StepEvent::Answer(Err(EditorFailure::NotAnswered)), cx)
        });
        assert!(matches!(step, Step::Done(ToolResult { ref text, is_error: true }) if text == words::NOT_READ));
    }
}
