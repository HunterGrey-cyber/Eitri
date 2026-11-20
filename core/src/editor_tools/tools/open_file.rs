//! `open_file`: show the person a file at a line.

use std::path::{Path, PathBuf};

use rmpv::Value;

use super::path_value;
use crate::editor_rpc::EditorRpc;
use crate::editor_tools::cards::{CardLine, CardRequest};
use crate::editor_tools::catalog::ToolRow;
use crate::editor_tools::gate::{verdict, Need, Verdict};
use crate::editor_tools::paths::{self, Target};
use crate::editor_tools::stamp::{Stamp, EDITOR_BUDGET};
use crate::editor_tools::step::{
    EditorFailure, EditorReach, EditorSight, Step, StepEvent, Tool, ToolCx, ToolError, ToolResult, ToolRun,
};
use crate::editor_tools::way::EditorCall;
use crate::editor_tools::{args, words};
use crate::nvim_rpc::Pending;

pub const ROW: ToolRow = ToolRow {
    name: "open_file",
    description: "Show the person a file at a line in their editor, in a side window. Their cursor, window and mode \
                  do not change. The path is absolute or relative to the project root.",
    schema: r#"{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "The file, absolute or relative to the project root." },
    "line": { "type": "integer", "minimum": 1, "description": "The line to show, from 1." },
    "end_line": { "type": "integer", "minimum": 1, "description": "The last line to highlight." }
  },
  "required": ["path"],
  "additionalProperties": false
}"#,
    always_load: true,
    default_on: true,
    in_untrusted: true,
};

pub const OPEN_FILE_LUA: &str = chunk_loading!("open_file.lua");

pub struct OpenFile;

impl Tool for OpenFile {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    fn begin(&self, args: &serde_json::Value, _cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        let raw = args::path(args, "path")?.to_owned();
        let (line, end_line) = args::line_range(args, "line", "end_line")?;
        Ok(Box::new(OpenRun {
            raw,
            line,
            end_line,
            stage: Stage::Judging,
        }))
    }
}

enum Stage {
    /// Nothing has run.
    Judging,
    /// A card asks whether a path outside the project may be shown; `shown` is where the card said it leads.
    OutsideCard {
        shown: PathBuf,
    },
    /// A call is out. `missing`: nothing is on disk, so only a buffer already open can answer.
    Called {
        path: PathBuf,
        load: bool,
        missing: bool,
    },
    /// A card asks whether a file that is not open may be loaded.
    LoadCard {
        path: PathBuf,
    },
    Done,
}

/// What the chunk answered.
enum Reply {
    Expired,
    NeedsLoad,
    Swap,
    Refused(String),
    Shown { place: &'static str, line: u64 },
    Unreadable,
}

struct OpenRun {
    raw: String,
    line: Option<u32>,
    end_line: Option<u32>,
    stage: Stage,
}

fn failure(text: impl Into<String>) -> Step {
    Step::Done(ToolResult {
        text: text.into(),
        is_error: true,
    })
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// The answer for a file now in the editor. A file in an editor the person cannot see is opened all the same, and
/// the highlight will be gone before they look, so the agent is told not to say it is in front of them.
fn shown_answer(place: &str, line: u64, sight: EditorSight) -> String {
    let told = match sight {
        EditorSight::OnScreen => "",
        EditorSight::OffScreen => {
            r#","on_screen":false,"note":"the editor is not on screen, so the person does not see this file until they bring the editor back""#
        }
        EditorSight::OtherWindow => {
            r#","other_window":true,"note":"the editor is in Eitri's first window, which did not come forward and may be covered, so the person may not see this file yet""#
        }
    };
    format!(r#"{{"shown":true,"where":"{place}","line":{line}{told}}}"#)
}

fn read_reply(value: &Value) -> Reply {
    if field(value, "expired").is_some() {
        return Reply::Expired;
    }
    if field(value, "needs_load").is_some() {
        return Reply::NeedsLoad;
    }
    if field(value, "swap").is_some() {
        return Reply::Swap;
    }
    if let Some(why) = field(value, "refused") {
        return Reply::Refused(why.as_str().unwrap_or("the editor refused").chars().take(300).collect());
    }
    if field(value, "shown").is_some() {
        let place = match field(value, "where").and_then(Value::as_str) {
            Some("existing") => "existing",
            Some("agent_window") => "agent_window",
            Some("new_split") => "new_split",
            _ => return Reply::Unreadable,
        };
        return match field(value, "line").and_then(Value::as_u64) {
            Some(line) => Reply::Shown { place, line },
            None => Reply::Unreadable,
        };
    }
    Reply::Unreadable
}

impl OpenRun {
    fn call(&mut self, path: PathBuf, load: bool, missing: bool) -> Step {
        let call = EditorCall::OpenFile {
            path: path.clone(),
            line: self.line,
            end_line: self.end_line,
            load,
        };
        self.stage = Stage::Called { path, load, missing };
        Step::Call {
            call,
            budget: EDITOR_BUDGET,
        }
    }

    fn judge(&mut self, cx: &ToolCx<'_>) -> Step {
        match paths::judge_for(cx, &self.raw) {
            Err(refused) => failure(refused.0),
            Ok(Target::Refused(why)) => failure(why),
            Ok(Target::Inside(path)) => match paths::file_check(&path) {
                Err(refused) => failure(refused.0),
                Ok(()) => {
                    let load = verdict(Need::LoadFile, cx.mode, cx.trust) == Verdict::Allow;
                    self.call(path, load, false)
                }
            },
            Ok(Target::Missing(path)) => self.call(path, false, true),
            Ok(Target::Outside { shown }) => match verdict(Need::OutsideProject, cx.mode, cx.trust) {
                Verdict::Allow => self.outside_approved(cx, None),
                Verdict::Card => {
                    self.stage = Stage::OutsideCard { shown: shown.clone() };
                    Step::Ask(CardRequest::new(
                        format!("Show {} in your editor?", shown.display()),
                        vec![CardLine::Plain(
                            "It is outside the project; opening it can start language servers and filetype plugins there"
                                .to_owned(),
                        )],
                    ))
                }
            },
        }
    }

    /// An outside path that may be shown: where it leads, checked, and loaded if it is not open. After a card,
    /// `approved` is the place the card named: a path that leads elsewhere by now was not approved.
    fn outside_approved(&mut self, cx: &ToolCx<'_>, approved: Option<&Path>) -> Step {
        let path = match paths::approved_outside(cx.root, &self.raw, approved, "nothing was shown") {
            Ok(path) => path,
            Err(refused) => return failure(refused.0),
        };
        match paths::file_check(&path) {
            Ok(()) => self.call(path, true, false),
            Err(refused) => failure(refused.0),
        }
    }

    /// A file the person allowed to be loaded: the card may have waited minutes, so the path is judged again and
    /// has to be the same file, still a regular file within the limit, before the editor is asked to read it.
    fn load_approved(&mut self, path: PathBuf, cx: &ToolCx<'_>) -> Step {
        match paths::judge_for(cx, &self.raw) {
            Ok(Target::Inside(now)) if now == path => match paths::file_check(&path) {
                Ok(()) => self.call(path, true, false),
                Err(refused) => failure(refused.0),
            },
            _ => failure(format!(
                "{} is no longer the file that was to be loaded; it was not opened",
                path.display()
            )),
        }
    }

    /// What the person decided on the card the run asked.
    fn card_answered(&mut self, allowed: bool, cx: &ToolCx<'_>) -> Step {
        if !allowed {
            self.stage = Stage::Done;
            return failure(words::NOT_ALLOWED);
        }
        match std::mem::replace(&mut self.stage, Stage::Done) {
            Stage::OutsideCard { shown } => self.outside_approved(cx, Some(shown.as_path())),
            Stage::LoadCard { path } => self.load_approved(path, cx),
            _ => failure("the editor was asked nothing"),
        }
    }

    fn answered(&mut self, answer: Result<Value, EditorFailure>, cx: &ToolCx<'_>) -> Step {
        let Stage::Called { path, load, missing } = std::mem::replace(&mut self.stage, Stage::Done) else {
            return failure("the editor was asked nothing");
        };
        let value = match answer {
            Ok(value) => value,
            Err(EditorFailure::NotAnswered) => return failure(words::NOT_SHOWN),
            Err(EditorFailure::Unavailable(_)) if cx.reach == EditorReach::Link => {
                return failure(words::WINDOW_CLOSED)
            }
            Err(EditorFailure::Unavailable(text) | EditorFailure::Refused(text)) => return failure(text),
        };
        match read_reply(&value) {
            Reply::Expired => failure(words::NOT_SHOWN),
            Reply::NeedsLoad if missing => failure(format!("no such file: {}", path.display())),
            Reply::NeedsLoad if load => failure(format!("the editor did not load {}", path.display())),
            Reply::NeedsLoad => {
                let shown = path.display();
                self.stage = Stage::LoadCard { path: path.clone() };
                Step::Ask(CardRequest::new(
                    format!("Load {shown} in your editor?"),
                    vec![CardLine::Plain(
                        "It is not open there; loading it can start language servers that run code from this repository"
                            .to_owned(),
                    )],
                ))
            }
            Reply::Swap => failure(format!("{} {}", path.display(), words::HAS_SWAP)),
            Reply::Refused(why) => failure(why),
            Reply::Shown { place, line } => Step::Done(ToolResult {
                text: shown_answer(place, line, cx.sight),
                is_error: false,
            }),
            Reply::Unreadable => failure("the editor's answer could not be read; nothing is known to be shown"),
        }
    }
}

impl ToolRun for OpenRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match event {
            StepEvent::Start if matches!(self.stage, Stage::Judging) => self.judge(cx),
            StepEvent::Answer(answer) => self.answered(answer, cx),
            StepEvent::Card(answer) => self.card_answered(answer.allowed(), cx),
            StepEvent::Start | StepEvent::Poll => failure("the editor was asked nothing"),
        }
    }
}

pub fn send(
    rpc: &dyn EditorRpc,
    stamp: &Stamp,
    path: &Path,
    line: Option<u32>,
    end_line: Option<u32>,
    load: bool,
) -> Pending {
    let mut args = vec![
        (Value::from("path"), path_value(path)),
        (Value::from("load"), Value::from(load)),
    ];
    if let Some(line) = line {
        args.push((Value::from("line"), Value::from(line)));
    }
    if let Some(end_line) = end_line {
        args.push((Value::from("end_line"), Value::from(end_line)));
    }
    rpc.exec_lua(OPEN_FILE_LUA, vec![stamp.to_value(), Value::Map(args)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tools::memory::Memory;
    use crate::editor_tools::step::AgentCall;
    use crate::test_scratch_dir::ScratchDir;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use serde_json::json;
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    struct Project {
        _scratch: ScratchDir,
        root: PathBuf,
        beside: PathBuf,
    }

    fn project(label: &str) -> Project {
        let scratch = ScratchDir::new("editor-tools-open", label);
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

    struct Setting {
        mode: PermissionMode,
        trust: ProjectTrust,
        reach: EditorReach,
        sight: EditorSight,
    }

    fn setting(mode: PermissionMode, trust: ProjectTrust) -> Setting {
        Setting {
            mode,
            trust,
            reach: EditorReach::Own,
            sight: EditorSight::OnScreen,
        }
    }

    fn with_cx<R>(root: &Path, setting: &Setting, f: impl FnOnce(&ToolCx<'_>) -> R) -> R {
        let memory = Memory::default();
        let cx = ToolCx {
            mode: setting.mode,
            trust: setting.trust,
            root,
            home: None,
            reach: setting.reach,
            sight: setting.sight,
            context: None,
            tab: 1,
            session: "session",
            call: "call",
            agent_call: AgentCall::Host,
            memory: &memory,
        };
        f(&cx)
    }

    fn run_of(raw: &str, line: Option<u32>) -> OpenRun {
        OpenRun {
            raw: raw.to_owned(),
            line,
            end_line: None,
            stage: Stage::Judging,
        }
    }

    fn start(project: &Project, setting: &Setting, raw: &str) -> (OpenRun, Step) {
        let mut run = run_of(raw, Some(2));
        let step = with_cx(&project.root, setting, |cx| run.step(StepEvent::Start, cx));
        (run, step)
    }

    fn the_call(step: &Step) -> (PathBuf, bool) {
        match step {
            Step::Call {
                call: EditorCall::OpenFile { path, load, .. },
                ..
            } => (path.clone(), *load),
            other => panic!("expected a call to open a file, got {other:?}"),
        }
    }

    fn the_card(step: &Step) -> &CardRequest {
        match step {
            Step::Ask(card) => card,
            other => panic!("expected a card, got {other:?}"),
        }
    }

    fn the_failure(step: &Step) -> &str {
        match step {
            Step::Done(ToolResult { text, is_error: true }) => text,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn map(entries: Vec<(&str, Value)>) -> Value {
        Value::Map(entries.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
    }

    fn answer(run: &mut OpenRun, project: &Project, setting: &Setting, value: Value) -> Step {
        with_cx(&project.root, setting, |cx| run.step(StepEvent::Answer(Ok(value)), cx))
    }

    fn after_card(run: &mut OpenRun, project: &Project, setting: &Setting, allowed: bool) -> Step {
        with_cx(&project.root, setting, |cx| run.card_answered(allowed, cx))
    }

    #[test]
    fn outside_is_a_card_in_manual_and_auto_and_allowed_in_bypass() {
        let project = project("outside");
        let outside = project.beside.join("b.rs");
        let raw = outside.to_str().unwrap();
        for mode in [PermissionMode::Manual, PermissionMode::Auto] {
            let setting = setting(mode, ProjectTrust::Trusted);
            let (mut run, step) = start(&project, &setting, raw);
            let card = the_card(&step);
            assert_eq!(card.title, format!("Show {} in your editor?", outside.display()));
            assert_eq!(
                card.body,
                vec![CardLine::Plain(
                    "It is outside the project; opening it can start language servers and filetype plugins there"
                        .to_owned()
                )]
            );
            let denied = after_card(&mut run, &project, &setting, false);
            assert_eq!(the_failure(&denied), words::NOT_ALLOWED);

            let (mut run, _) = start(&project, &setting, raw);
            let allowed = after_card(&mut run, &project, &setting, true);
            assert_eq!(the_call(&allowed), (outside.clone(), true), "{mode:?}");
        }
        let (_, step) = start(&project, &setting(PermissionMode::Bypass, ProjectTrust::Untrusted), raw);
        assert_eq!(the_call(&step), (outside, true));
    }

    #[test]
    fn an_unloaded_file_is_a_card_in_manual_and_in_an_n_session() {
        let project = project("unloaded");
        let file = project.root.join("src/a.rs");
        for setting in [
            setting(PermissionMode::Manual, ProjectTrust::Trusted),
            setting(PermissionMode::Auto, ProjectTrust::Untrusted),
        ] {
            let (mut run, step) = start(&project, &setting, "src/a.rs");
            assert_eq!(the_call(&step), (file.clone(), false), "the first ask loads nothing");
            let asked = answer(
                &mut run,
                &project,
                &setting,
                map(vec![("needs_load", Value::from(true))]),
            );
            let card = the_card(&asked);
            assert_eq!(card.title, format!("Load {} in your editor?", file.display()));
            assert_eq!(
                card.body,
                vec![CardLine::Plain(
                    "It is not open there; loading it can start language servers that run code from this repository"
                        .to_owned()
                )]
            );
            let denied = after_card(&mut run, &project, &setting, false);
            assert_eq!(the_failure(&denied), words::NOT_ALLOWED);

            let (mut run, _) = start(&project, &setting, "src/a.rs");
            answer(
                &mut run,
                &project,
                &setting,
                map(vec![("needs_load", Value::from(true))]),
            );
            let loaded = after_card(&mut run, &project, &setting, true);
            assert_eq!(the_call(&loaded), (file.clone(), true));
        }
    }

    #[test]
    fn an_auto_y_tab_loads_without_a_card() {
        let project = project("auto-y");
        let (_, step) = start(
            &project,
            &setting(PermissionMode::Auto, ProjectTrust::Trusted),
            "src/a.rs",
        );
        assert_eq!(the_call(&step), (project.root.join("src/a.rs"), true));
    }

    #[test]
    fn bypass_loads_without_a_card() {
        let project = project("bypass");
        for trust in [ProjectTrust::Trusted, ProjectTrust::Untrusted] {
            let (_, step) = start(&project, &setting(PermissionMode::Bypass, trust), "src/a.rs");
            assert_eq!(the_call(&step), (project.root.join("src/a.rs"), true), "{trust:?}");
        }
    }

    #[test]
    fn a_window_2_tab_outside_is_refused_with_no_card() {
        let project = project("link");
        let mut link = setting(PermissionMode::Manual, ProjectTrust::Trusted);
        link.reach = EditorReach::Link;
        let outside = project.beside.join("b.rs");
        let (_, step) = start(&project, &link, outside.to_str().unwrap());
        assert_eq!(the_failure(&step), words::LINK);
        link.mode = PermissionMode::Bypass;
        let (_, step) = start(&project, &link, outside.to_str().unwrap());
        assert_eq!(
            the_failure(&step),
            words::LINK,
            "bypass opens no way out of the project from window 2"
        );
    }

    #[test]
    fn a_fifo_never_reaches_the_editor() {
        let project = project("fifo");
        let fifo = project.root.join("pipe");
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        for mode in [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass] {
            let (_, step) = start(&project, &setting(mode, ProjectTrust::Trusted), "pipe");
            assert!(the_failure(&step).contains("not a regular file"), "{mode:?}");
        }
        let (_, step) = start(&project, &setting(PermissionMode::Bypass, ProjectTrust::Trusted), "src");
        assert!(the_failure(&step).contains("not a regular file"), "a directory too");
    }

    #[test]
    fn the_answer_names_the_clamped_line() {
        let project = project("clamp");
        let setting = setting(PermissionMode::Bypass, ProjectTrust::Trusted);
        let mut run = run_of("src/a.rs", Some(9_000));
        with_cx(&project.root, &setting, |cx| run.step(StepEvent::Start, cx));
        let done = answer(
            &mut run,
            &project,
            &setting,
            map(vec![
                ("shown", Value::from(true)),
                ("where", Value::from("new_split")),
                ("line", Value::from(3)),
            ]),
        );
        let Step::Done(ToolResult { text, is_error: false }) = done else {
            panic!("expected an answer: {done:?}");
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, json!({"shown": true, "where": "new_split", "line": 3}));
    }

    #[test]
    fn a_missing_file_is_refused_with_no_card() {
        let project = project("missing");
        for mode in [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass] {
            let setting = setting(mode, ProjectTrust::Trusted);
            let (mut run, step) = start(&project, &setting, "src/new.rs");
            let wanted = project.root.join("src/new.rs");
            assert_eq!(
                the_call(&step),
                (wanted.clone(), false),
                "an unsaved buffer may be open"
            );
            let asked = answer(
                &mut run,
                &project,
                &setting,
                map(vec![("needs_load", Value::from(true))]),
            );
            assert_eq!(the_failure(&asked), format!("no such file: {}", wanted.display()));
        }
    }

    #[test]
    fn a_load_card_rechecks_the_file_before_the_editor_is_asked() {
        let project = project("load-recheck");
        let file = project.root.join("src/a.rs");
        let setting = setting(PermissionMode::Manual, ProjectTrust::Trusted);
        let needs_load = || map(vec![("needs_load", Value::from(true))]);

        // The card waits; the file becomes a FIFO, which would block the editor that read it.
        let (mut run, _) = start(&project, &setting, "src/a.rs");
        answer(&mut run, &project, &setting, needs_load());
        fs::remove_file(&file).unwrap();
        let fifo = CString::new(file.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let allowed = after_card(&mut run, &project, &setting, true);
        assert!(the_failure(&allowed).contains("not a regular file"), "{allowed:?}");

        // The card waits; the file becomes a link to a file outside the project.
        fs::remove_file(&file).unwrap();
        fs::write(&file, "a").unwrap();
        let (mut run, _) = start(&project, &setting, "src/a.rs");
        answer(&mut run, &project, &setting, needs_load());
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(project.beside.join("b.rs"), &file).unwrap();
        let allowed = after_card(&mut run, &project, &setting, true);
        assert!(the_failure(&allowed).contains("no longer the file"), "{allowed:?}");

        // The card waits; the file grows past the limit.
        fs::remove_file(&file).unwrap();
        fs::write(&file, "a").unwrap();
        let (mut run, _) = start(&project, &setting, "src/a.rs");
        answer(&mut run, &project, &setting, needs_load());
        fs::File::create(&file).unwrap().set_len(17 * 1024 * 1024).unwrap();
        let allowed = after_card(&mut run, &project, &setting, true);
        assert!(!matches!(allowed, Step::Call { .. }), "{allowed:?}");
        assert!(the_failure(&allowed).contains(&file.display().to_string()));
    }

    #[test]
    fn the_outside_card_approves_only_the_place_it_named() {
        let project = project("outside-retarget");
        let other = project.beside.join("other.txt");
        fs::write(&other, "other").unwrap();
        let link = project.root.join("link.rs");
        std::os::unix::fs::symlink(project.beside.join("b.rs"), &link).unwrap();
        let setting = setting(PermissionMode::Manual, ProjectTrust::Trusted);

        let (mut run, step) = start(&project, &setting, "link.rs");
        let named = project.beside.join("b.rs");
        assert_eq!(
            the_card(&step).title,
            format!("Show {} in your editor?", named.display())
        );
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        let allowed = after_card(&mut run, &project, &setting, true);
        assert!(!matches!(allowed, Step::Call { .. }), "{allowed:?}");
        assert!(
            the_failure(&allowed).contains("now leads somewhere other"),
            "{allowed:?}"
        );

        // Left alone, the same card opens the place it named.
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&named, &link).unwrap();
        let (mut run, _) = start(&project, &setting, "link.rs");
        let allowed = after_card(&mut run, &project, &setting, true);
        assert_eq!(the_call(&allowed), (named, true));
    }

    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "APFS refuses file names that are not UTF-8; this test needs one"
    )]
    fn two_outside_names_that_read_alike_are_not_one_place() {
        let project = project("outside-alike");
        let named = project.beside.join(std::ffi::OsStr::from_bytes(b"secret\xff"));
        let other = project.beside.join(std::ffi::OsStr::from_bytes(b"secret\xfe"));
        fs::write(&named, "named").unwrap();
        fs::write(&other, "other").unwrap();
        assert_eq!(named.display().to_string(), other.display().to_string());
        let link = project.root.join("link.rs");
        std::os::unix::fs::symlink(&named, &link).unwrap();
        let setting = setting(PermissionMode::Manual, ProjectTrust::Trusted);

        let (mut run, step) = start(&project, &setting, "link.rs");
        the_card(&step);
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        let allowed = after_card(&mut run, &project, &setting, true);
        assert!(!matches!(allowed, Step::Call { .. }), "{allowed:?}");
        assert!(
            the_failure(&allowed).contains("now leads somewhere other"),
            "{allowed:?}"
        );
    }

    /// The file is opened either way; only the agent's sentence to the person depends on this.
    #[test]
    fn the_answer_says_when_the_person_cannot_see_the_editor() {
        let project = project("sight");
        let shown = |sight: EditorSight| {
            let mut setting = setting(PermissionMode::Bypass, ProjectTrust::Trusted);
            setting.sight = sight;
            let (mut run, _) = start(&project, &setting, "src/a.rs");
            let value = map(vec![
                ("shown", Value::from(true)),
                ("where", Value::from("new_split")),
                ("line", Value::from(4)),
            ]);
            let Step::Done(result) = answer(&mut run, &project, &setting, value) else {
                panic!("the answer ends the run");
            };
            assert!(!result.is_error, "{}", result.text);
            result.text
        };
        assert_eq!(
            shown(EditorSight::OnScreen),
            r#"{"shown":true,"where":"new_split","line":4}"#
        );

        let hidden: serde_json::Value = serde_json::from_str(&shown(EditorSight::OffScreen)).unwrap();
        assert_eq!(hidden["shown"], true);
        assert_eq!(hidden["on_screen"], false);
        assert!(hidden["note"].as_str().unwrap().contains("not on screen"), "{hidden}");

        let elsewhere: serde_json::Value = serde_json::from_str(&shown(EditorSight::OtherWindow)).unwrap();
        assert_eq!(elsewhere["shown"], true);
        assert_eq!(elsewhere["other_window"], true);
        assert!(
            elsewhere.get("on_screen").is_none(),
            "nothing here can tell: {elsewhere}"
        );
    }

    #[test]
    fn an_answer_of_the_editor_becomes_a_sentence() {
        let project = project("sentences");
        let own = setting(PermissionMode::Bypass, ProjectTrust::Trusted);
        let mut link = setting(PermissionMode::Bypass, ProjectTrust::Trusted);
        link.reach = EditorReach::Link;
        let file = project.root.join("src/a.rs");
        let fail = |setting: &Setting, failure: EditorFailure| {
            let (mut run, _) = start(&project, setting, "src/a.rs");
            with_cx(&project.root, setting, |cx| {
                run.step(StepEvent::Answer(Err(failure)), cx)
            })
        };
        assert_eq!(the_failure(&fail(&own, EditorFailure::NotAnswered)), words::NOT_SHOWN);
        assert_eq!(
            the_failure(&fail(&link, EditorFailure::Unavailable("gone".to_owned()))),
            words::WINDOW_CLOSED
        );
        assert_eq!(
            the_failure(&fail(&own, EditorFailure::Unavailable("gone".to_owned()))),
            "gone"
        );
        assert_eq!(the_failure(&fail(&own, EditorFailure::Refused("no".to_owned()))), "no");

        let (mut run, _) = start(&project, &own, "src/a.rs");
        let swapped = answer(&mut run, &project, &own, map(vec![("swap", Value::from(true))]));
        assert_eq!(
            the_failure(&swapped),
            format!(
                "{} has a swap file (another editor, or a crash); it was not opened",
                file.display()
            )
        );
        let (mut run, _) = start(&project, &own, "src/a.rs");
        let expired = answer(&mut run, &project, &own, map(vec![("expired", Value::from(true))]));
        assert_eq!(the_failure(&expired), words::NOT_SHOWN);
        let (mut run, _) = start(&project, &own, "src/a.rs");
        let refused = answer(&mut run, &project, &own, map(vec![("refused", Value::from("why"))]));
        assert_eq!(the_failure(&refused), "why");
        let (mut run, _) = start(&project, &own, "src/a.rs");
        let odd = answer(&mut run, &project, &own, Value::Nil);
        assert!(the_failure(&odd).contains("could not be read"));
    }

    #[test]
    fn the_arguments_are_read_by_type() {
        let project = project("args");
        let own = setting(PermissionMode::Bypass, ProjectTrust::Trusted);
        let begin = |args: serde_json::Value| {
            with_cx(&project.root, &own, |cx| {
                OpenFile.begin(&args, cx).map(|_| ()).unwrap_err().0
            })
        };
        assert_eq!(begin(json!({})), "path is required");
        assert_eq!(begin(json!({"path": 3})), "path must be a string");
        assert_eq!(begin(json!({"path": "a", "line": 0})), "line starts at 1");
        assert_eq!(
            begin(json!({"path": "a", "line": 5, "end_line": 4})),
            "end_line may not precede line"
        );
        with_cx(&project.root, &own, |cx| {
            assert!(OpenFile
                .begin(&json!({"path": "a", "line": 2, "end_line": 4}), cx)
                .is_ok());
        });
    }
}
