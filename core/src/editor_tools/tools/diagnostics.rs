//! `diagnostics`: what the language servers and linters said, as the editor holds it. The types and `send` need
//! no tab and no broker.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rmpv::Value;

use super::baseline::{appeared, BaselineKey, TurnBaseline, BASELINE_LIMIT, NO_BASELINE};
use super::path_value;
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
    name: "diagnostics",
    description: "The editor's diagnostics, from language servers and linters, for one open file or for every open \
                  file of this project, the most severe first. The answer names the files with unsaved changes: \
                  their diagnostics describe text not yet on disk.",
    schema: r#"{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "One open file; every open file of the project when left out." },
    "min_severity": { "type": "string", "enum": ["error", "warning", "info", "hint"], "description": "The least severe to include." },
    "since": { "type": "string", "enum": ["turn"], "description": "Only the diagnostics that appeared since this turn began." }
  },
  "additionalProperties": false
}"#,
    always_load: true,
    default_on: true,
    in_untrusted: true,
};

pub const DIAGNOSTICS_LUA: &str = chunk!("diagnostics.lua");

/// The most entries the tool asks for.
pub const DIAGNOSTICS_LIMIT: u32 = 500;

/// What of a reply's cap the chunk's answer leaves to the frame's own fields and the answer's outer keys.
const REPLY_ROOM: usize = 1024 * 1024;

/// The most the chunk's answer may come to as the JSON of a reply to the other window: past the link's cap the reply
/// is refused whole, so the chunk stops short of it and counts the rest. `limit` cannot do this: a control byte is
/// six bytes of JSON, and a string that is not UTF-8 travels as a list of numbers.
pub fn answer_max() -> usize {
    crate::window_link::msg::body_cap("editor_reply") - REPLY_ROOM
}

/// A `since` call about a file the baseline cannot hold.
const BASELINE_INSIDE_ONLY: &str =
    "since \"turn\" covers the project's files only; leave since out for a file outside it";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    /// The word the schema and the chunk use.
    pub fn word(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
            Severity::Hint => "hint",
        }
    }

    pub fn from_word(word: &str) -> Option<Severity> {
        match word {
            "error" => Some(Severity::Error),
            "warning" => Some(Severity::Warning),
            "info" => Some(Severity::Info),
            "hint" => Some(Severity::Hint),
            _ => None,
        }
    }
}

/// `limit`: the most entries the chunk returns, cutting at it or at [`answer_max`] and counting `left_out`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsArgs {
    pub root: PathBuf,
    pub path: Option<PathBuf>,
    pub min_severity: Severity,
    pub utf16: bool,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticEntry {
    /// The buffer's name.
    pub path: PathBuf,
    /// From 1.
    pub line: u32,
    /// From 1, in bytes.
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub severity: Severity,
    pub source: Option<String>,
    pub code: Option<String>,
    /// At most 2 KiB, cut by the chunk.
    pub message: String,
    /// With `utf16`: from 0, in UTF-16 units.
    pub start_character: Option<u32>,
    pub end_character: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsAnswer {
    pub entries: Vec<DiagnosticEntry>,
    pub modified: Vec<PathBuf>,
    pub left_out: u32,
}

pub struct Diagnostics;

impl Tool for Diagnostics {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    fn begin(&self, args: &serde_json::Value, _cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        let raw = args::optional_path(args, "path")?.map(str::to_owned);
        let min_severity = args::severity(args, "min_severity")?.unwrap_or(Severity::Hint);
        let since = args::since(args, "since")?;
        Ok(Box::new(DiagnosticsRun {
            raw,
            min_severity,
            since: since.then(Since::default),
            approved: None,
            stage: Stage::Judging,
        }))
    }
}

enum Stage {
    /// Nothing has run.
    Judging,
    /// A card asks whether the diagnostics of a buffer of a file outside the project may be read; `shown` is where
    /// the card said it leads.
    OutsideCard {
        shown: PathBuf,
    },
    /// A call is out.
    Called,
    Done,
}

/// A call that asks for what appeared since the turn began, and the baseline it subtracts.
#[derive(Default)]
struct Since {
    /// The turn the baseline was taken for, held from the start of the call to its answer.
    turn: String,
    counts: BTreeMap<BaselineKey, u32>,
    /// The baseline hit its limit.
    cut: bool,
}

struct DiagnosticsRun {
    raw: Option<String>,
    min_severity: Severity,
    since: Option<Since>,
    /// The outside file the person allowed the diagnostics of: the only outside buffer the answer may name.
    approved: Option<PathBuf>,
    stage: Stage,
}

fn failure(text: impl Into<String>) -> ToolResult {
    ToolResult {
        text: text.into(),
        is_error: true,
    }
}

impl DiagnosticsRun {
    fn call(&mut self, cx: &ToolCx<'_>, path: Option<PathBuf>) -> Step {
        self.stage = Stage::Called;
        // What appeared is a difference of two full readings, so neither may be cut at the answer's size.
        let limit = if self.since.is_some() {
            BASELINE_LIMIT
        } else {
            DIAGNOSTICS_LIMIT
        };
        Step::Call {
            call: EditorCall::Diagnostics {
                root: cx.root.to_path_buf(),
                path,
                min_severity: self.min_severity,
                limit,
            },
            budget: EDITOR_BUDGET,
        }
    }

    fn done(&mut self, result: ToolResult) -> Step {
        self.stage = Stage::Done;
        Step::Done(result)
    }

    /// The baseline of the turn the call is in, taken from the tab's memory; the call ends here if there is none.
    fn take_baseline(&mut self, cx: &ToolCx<'_>) -> Result<(), Step> {
        let Some(since) = self.since.as_mut() else {
            return Ok(());
        };
        let held = cx.memory.with(cx.tab, |memory| match &memory.baseline {
            Some(TurnBaseline::Taken { turn, counts, cut }) => Some((turn.clone(), counts.clone(), *cut)),
            _ => None,
        });
        match held {
            Some((turn, counts, cut)) => {
                *since = Since { turn, counts, cut };
                Ok(())
            }
            None => Err(self.done(failure(NO_BASELINE))),
        }
    }

    fn judge(&mut self, cx: &ToolCx<'_>) -> Step {
        if let Err(step) = self.take_baseline(cx) {
            return step;
        }
        let Some(raw) = self.raw.clone() else {
            return self.call(cx, None);
        };
        match paths::judge_for(cx, &raw) {
            Err(refused) => self.done(failure(refused.0)),
            Ok(Target::Refused(why)) => self.done(failure(why)),
            // A path with nothing on disk is asked all the same: an unsaved new file may be a buffer.
            Ok(Target::Inside(path) | Target::Missing(path)) => self.call(cx, Some(path)),
            // The baseline covers the project's buffers: an outside file would read as all new.
            Ok(Target::Outside { .. }) if self.since.is_some() => self.done(failure(BASELINE_INSIDE_ONLY)),
            Ok(Target::Outside { shown }) => match verdict(Need::OutsideProject, cx.mode, cx.trust) {
                Verdict::Allow => self.outside_approved(cx, &raw, None),
                Verdict::Card => {
                    self.stage = Stage::OutsideCard { shown: shown.clone() };
                    Step::Ask(CardRequest::new(
                        format!("Read the diagnostics of {} from your editor?", shown.display()),
                        vec![CardLine::Plain("It is outside the project".to_owned())],
                    ))
                }
            },
        }
    }

    /// An outside path that may be read: where it leads, with every link followed. After a card, `shown` is the
    /// place the card named: a path that leads elsewhere by now was not approved.
    fn outside_approved(&mut self, cx: &ToolCx<'_>, raw: &str, shown: Option<&Path>) -> Step {
        let path = match paths::outside_target(cx.root, raw) {
            Ok(path) => path,
            Err(refused) => return self.done(failure(refused.0)),
        };
        if let Some(shown) = shown {
            if path != shown {
                return self.done(failure(format!(
                    "{raw} now leads somewhere other than {}; nothing was read",
                    shown.display()
                )));
            }
        }
        self.approved = Some(path.clone());
        self.call(cx, Some(path))
    }

    fn card_answered(&mut self, allowed: bool, cx: &ToolCx<'_>) -> Step {
        let stage = std::mem::replace(&mut self.stage, Stage::Done);
        let (Stage::OutsideCard { shown }, Some(raw)) = (stage, self.raw.clone()) else {
            return self.done(failure("the editor was asked nothing"));
        };
        if !allowed {
            return self.done(failure(words::NOT_ALLOWED));
        }
        self.outside_approved(cx, &raw, Some(shown.as_path()))
    }

    fn answered(&mut self, answer: Result<Value, EditorFailure>, cx: &ToolCx<'_>) -> Step {
        if !matches!(self.stage, Stage::Called) {
            return self.done(failure("the editor was asked nothing"));
        }
        let result = match answer {
            Ok(value) => match self.since.take() {
                // A baseline of another turn than the one the call began in, or none by now, is no baseline.
                Some(since) if !baseline_is_of(cx, &since.turn) => failure(NO_BASELINE),
                Some(since) => since_result(cx, &value, since),
                None => result_for(cx, &value, self.approved.as_deref()),
            },
            Err(EditorFailure::NotAnswered) => failure(words::NOT_READ),
            Err(EditorFailure::Unavailable(_)) if cx.reach == EditorReach::Link => failure(words::WINDOW_CLOSED),
            Err(EditorFailure::Unavailable(text) | EditorFailure::Refused(text)) => failure(text),
        };
        self.done(result)
    }
}

impl ToolRun for DiagnosticsRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match event {
            StepEvent::Start if matches!(self.stage, Stage::Judging) => self.judge(cx),
            StepEvent::Answer(answer) => self.answered(answer, cx),
            StepEvent::Card(answer) => self.card_answered(answer.allowed(), cx),
            StepEvent::Start | StepEvent::Poll => Step::Done(failure("the editor was asked nothing")),
        }
    }
}

pub fn send(rpc: &dyn EditorRpc, stamp: &Stamp, args: &DiagnosticsArgs) -> Pending {
    let mut fields = vec![
        (Value::from("root"), path_value(&args.root)),
        (Value::from("min_severity"), Value::from(args.min_severity.word())),
        (Value::from("utf16"), Value::from(args.utf16)),
        (Value::from("limit"), Value::from(args.limit)),
        (Value::from("answer_max"), Value::from(answer_max())),
    ];
    if let Some(path) = &args.path {
        fields.push((Value::from("path"), path_value(path)));
    }
    rpc.exec_lua(DIAGNOSTICS_LUA, vec![stamp.to_value(), Value::Map(fields)])
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

/// The bytes of a string the editor sent: it sends a Lua string as msgpack str whatever its bytes are.
fn bytes_of(value: &Value) -> Option<&[u8]> {
    match value {
        Value::String(text) => Some(text.as_bytes()),
        Value::Binary(bytes) => Some(bytes.as_slice()),
        _ => None,
    }
}

pub(super) fn name_of(value: &Value) -> Option<PathBuf> {
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes_of(value)?)))
}

fn text_of(value: &Value) -> Option<String> {
    Some(String::from_utf8_lossy(bytes_of(value)?).into_owned())
}

/// A list the chunk built: an empty Lua table may arrive as either an array or a map.
fn list<'a>(value: &'a Value, name: &str) -> Option<&'a [Value]> {
    match field(value, name)? {
        Value::Map(entries) if entries.is_empty() => Some(&[]),
        Value::Array(items) => Some(items),
        _ => None,
    }
}

fn entry_of(value: &Value) -> Option<DiagnosticEntry> {
    // The editor counts lines and columns from 0.
    let from_one = |name: &str| count(value, name).map(|number| number.saturating_add(1));
    let optional = |name: &str| field(value, name).and_then(text_of);
    Some(DiagnosticEntry {
        path: name_of(field(value, "path")?)?,
        line: from_one("lnum")?,
        column: from_one("col")?,
        end_line: from_one("end_lnum")?,
        end_column: from_one("end_col")?,
        severity: Severity::from_word(field(value, "severity")?.as_str()?)?,
        source: optional("source"),
        code: optional("code"),
        message: text_of(field(value, "message")?)?,
        start_character: count(value, "start_character"),
        end_character: count(value, "end_character"),
    })
}

/// What the chunk answered, or the reason it answered nothing usable.
pub fn parse(value: &Value) -> Result<DiagnosticsAnswer, String> {
    if field(value, "expired").is_some() {
        return Err(words::NOT_READ.to_owned());
    }
    let unreadable = || "the editor's answer could not be read; nothing is known of its diagnostics".to_owned();
    let entries = list(value, "entries")
        .ok_or_else(unreadable)?
        .iter()
        .map(entry_of)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(unreadable)?;
    let modified = list(value, "modified")
        .ok_or_else(unreadable)?
        .iter()
        .map(name_of)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(unreadable)?;
    Ok(DiagnosticsAnswer {
        entries,
        modified,
        left_out: count(value, "left_out").ok_or_else(unreadable)?,
    })
}

/// What the project, or the one outside file that was allowed, calls the buffer `name`: its path relative to the
/// root, or the allowed file's own path. A name that is not absolute is never a file of the project.
fn inside(
    cx: &ToolCx<'_>,
    name: &Path,
    approved: Option<&Path>,
    judged: &mut HashMap<PathBuf, Option<String>>,
) -> Option<String> {
    if let Some(known) = judged.get(name) {
        return known.clone();
    }
    let shown = if !name.is_absolute() {
        None
    } else if paths::buffer_inside(cx, name) {
        Some(
            name.strip_prefix(cx.root)
                .unwrap_or(name)
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        // The buffer of an outside file is told only when that very file was allowed, whatever link names it.
        approved
            .filter(|allowed| paths::names_the_file(name, allowed))
            .map(|allowed| allowed.to_string_lossy().into_owned())
    };
    judged.insert(name.to_path_buf(), shown.clone());
    shown
}

fn entry_json(path: &str, entry: &DiagnosticEntry) -> serde_json::Value {
    let mut told = serde_json::json!({
        "path": path,
        "line": entry.line,
        "column": entry.column,
        "end_line": entry.end_line,
        "end_column": entry.end_column,
        "severity": entry.severity.word(),
        "message": entry.message,
    });
    if let Some(source) = &entry.source {
        told["source"] = serde_json::Value::from(source.as_str());
    }
    if let Some(code) = &entry.code {
        told["code"] = serde_json::Value::from(code.as_str());
    }
    told
}

/// The answer's JSON with the first `kept` entries; the ones after them are counted in `left_out`.
#[cfg(test)]
fn answer_text(told: &[serde_json::Value], modified: &[String], left_out: u32, kept: usize) -> String {
    answer_json(told, modified, left_out, kept, false)
}

/// [`answer_text`], saying `baseline_cut` when a difference was taken between readings that hit their limit.
fn answer_json(
    told: &[serde_json::Value],
    modified: &[String],
    left_out: u32,
    kept: usize,
    baseline_cut: bool,
) -> String {
    let cut = u32::try_from(told.len() - kept)
        .unwrap_or(u32::MAX)
        .saturating_add(left_out);
    let mut answer = serde_json::json!({
        "entries": &told[..kept],
        "modified_files": modified,
        "left_out": cut,
    });
    if baseline_cut {
        answer["baseline_cut"] = serde_json::Value::Bool(true);
    }
    answer.to_string()
}

/// The sentence or the JSON a chunk's answer comes to for the agent. Every path is judged here, whatever the chunk
/// filtered: an entry whose buffer fails is dropped without a word.
pub fn result_of(cx: &ToolCx<'_>, value: &Value) -> ToolResult {
    result_for(cx, value, None)
}

/// [`result_of`] for a call whose person allowed the diagnostics of one file outside the project: that file's
/// buffer is told by its path, every other outside buffer still dropped.
pub fn result_for(cx: &ToolCx<'_>, value: &Value, approved: Option<&Path>) -> ToolResult {
    match parse(value) {
        Ok(answer) => told(cx, &answer, approved, None),
        Err(sentence) => failure(sentence),
    }
}

/// What appeared since the baseline: the editor's reading less the baseline's entries, then judged and cut as
/// any answer is. The cap comes after the difference, so old entries never take the places of new ones.
fn since_result(cx: &ToolCx<'_>, value: &Value, since: Since) -> ToolResult {
    let mut answer = match parse(value) {
        Ok(answer) => answer,
        Err(sentence) => return failure(sentence),
    };
    let cut = since.cut || answer.left_out > 0;
    answer.entries = appeared(std::mem::take(&mut answer.entries), &since.counts);
    told(cx, &answer, None, Some(Appeared { cut }))
}

/// Whether the tab still keeps the baseline of `turn`.
fn baseline_is_of(cx: &ToolCx<'_>, turn: &str) -> bool {
    cx.memory.with(
        cx.tab,
        |memory| matches!(&memory.baseline, Some(TurnBaseline::Taken { turn: held, .. }) if held == turn),
    )
}

/// How a `since` answer differs from a plain one.
struct Appeared {
    /// Either reading hit its limit, so an old entry can read as new.
    cut: bool,
}

/// The JSON a parsed answer comes to for the agent.
fn told(cx: &ToolCx<'_>, answer: &DiagnosticsAnswer, approved: Option<&Path>, since: Option<Appeared>) -> ToolResult {
    let mut judged = HashMap::new();
    let mut told: Vec<serde_json::Value> = answer
        .entries
        .iter()
        .filter_map(|entry| inside(cx, &entry.path, approved, &mut judged).map(|path| entry_json(&path, entry)))
        .collect();
    let modified: Vec<String> = answer
        .modified
        .iter()
        .filter_map(|name| inside(cx, name, approved, &mut judged))
        .collect();
    // The chunk cut a plain answer at `DIAGNOSTICS_LIMIT`; a difference is cut here.
    let mut left_out = answer.left_out;
    if since.is_some() && told.len() > DIAGNOSTICS_LIMIT as usize {
        left_out = left_out.saturating_add(u32::try_from(told.len() - DIAGNOSTICS_LIMIT as usize).unwrap_or(u32::MAX));
        told.truncate(DIAGNOSTICS_LIMIT as usize);
    }
    let baseline_cut = since.is_some_and(|since| since.cut);
    let ready = |kept: usize| answer_json(&told, &modified, left_out, kept, baseline_cut);
    // Whole entries come off the end until the answer fits. What fits only shrinks as entries are added, so the
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tools::memory::Memory;
    use crate::editor_tools::step::{AgentCall, EditorSight};
    use crate::test_scratch_dir::ScratchDir;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use serde_json::json;
    use std::fs;

    struct Project {
        _scratch: ScratchDir,
        root: PathBuf,
        beside: PathBuf,
    }

    fn project(label: &str) -> Project {
        let scratch = ScratchDir::new("editor-tools-diagnostics", label);
        let base = fs::canonicalize(&*scratch).unwrap();
        let root = base.join("project");
        let beside = base.join("beside");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&beside).unwrap();
        fs::write(root.join("src/a.rs"), "a\n").unwrap();
        fs::write(beside.join("b.rs"), "b").unwrap();
        std::os::unix::fs::symlink(beside.join("b.rs"), root.join("src/out.rs")).unwrap();
        Project {
            _scratch: scratch,
            root,
            beside,
        }
    }

    fn with_cx<R>(root: &Path, f: impl FnOnce(&ToolCx<'_>) -> R) -> R {
        with_memory_cx(root, &Memory::default(), f)
    }

    fn with_memory_cx<R>(root: &Path, memory: &Memory, f: impl FnOnce(&ToolCx<'_>) -> R) -> R {
        let cx = ToolCx {
            mode: PermissionMode::Manual,
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
            memory,
        };
        f(&cx)
    }

    fn map(entries: Vec<(&str, Value)>) -> Value {
        Value::Map(entries.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
    }

    fn raw_entry(path: &Path, lnum: u32, severity: &str, message: &str) -> Value {
        map(vec![
            ("path", Value::Binary(path.as_os_str().as_bytes().to_vec())),
            ("lnum", Value::from(lnum)),
            ("col", Value::from(4)),
            ("end_lnum", Value::from(lnum + 1)),
            ("end_col", Value::from(9)),
            ("severity", Value::from(severity)),
            ("message", Value::from(message)),
        ])
    }

    fn reply(entries: Vec<Value>, modified: &[&Path], left_out: u32) -> Value {
        map(vec![
            ("entries", Value::Array(entries)),
            (
                "modified",
                Value::Array(
                    modified
                        .iter()
                        .map(|path| Value::Binary(path.as_os_str().as_bytes().to_vec()))
                        .collect(),
                ),
            ),
            ("left_out", Value::from(left_out)),
        ])
    }

    fn told(root: &Path, value: &Value) -> serde_json::Value {
        let result = with_cx(root, |cx| result_of(cx, value));
        assert!(!result.is_error, "{}", result.text);
        serde_json::from_str(&result.text).unwrap()
    }

    #[test]
    fn parse_round_trips_every_field() {
        let mut full = raw_entry(Path::new("/p/src/a.rs"), 2, "warning", "unused variable");
        let Value::Map(fields) = &mut full else { unreachable!() };
        fields.push((Value::from("source"), Value::from("rustc")));
        fields.push((Value::from("code"), Value::from("E0001")));
        fields.push((Value::from("start_character"), Value::from(3)));
        fields.push((Value::from("end_character"), Value::from(8)));
        let answer = parse(&reply(vec![full], &[Path::new("/p/src/a.rs")], 7)).unwrap();
        assert_eq!(
            answer,
            DiagnosticsAnswer {
                entries: vec![DiagnosticEntry {
                    path: "/p/src/a.rs".into(),
                    line: 3,
                    column: 5,
                    end_line: 4,
                    end_column: 10,
                    severity: Severity::Warning,
                    source: Some("rustc".to_owned()),
                    code: Some("E0001".to_owned()),
                    message: "unused variable".to_owned(),
                    start_character: Some(3),
                    end_character: Some(8),
                }],
                modified: vec!["/p/src/a.rs".into()],
                left_out: 7,
            }
        );
        let bare = parse(&reply(vec![raw_entry(Path::new("/p/x"), 0, "hint", "m")], &[], 0)).unwrap();
        assert_eq!(bare.entries[0].source, None);
        assert_eq!(bare.entries[0].start_character, None);
        // An empty Lua table may arrive as a map.
        let empty = map(vec![
            ("entries", Value::Map(vec![])),
            ("modified", Value::Map(vec![])),
            ("left_out", Value::from(0)),
        ]);
        assert_eq!(parse(&empty).unwrap().entries, vec![]);
        assert_eq!(
            parse(&map(vec![("expired", Value::from(true))])),
            Err(words::NOT_READ.to_owned())
        );
        assert!(parse(&Value::Nil).is_err());
        let bad = reply(vec![raw_entry(Path::new("/p/x"), 0, "fatal", "m")], &[], 0);
        assert!(parse(&bad).is_err(), "a severity the schema does not know");
    }

    #[test]
    fn lines_are_one_based() {
        let project = project("one-based");
        let file = project.root.join("src/a.rs");
        let answer = told(
            &project.root,
            &reply(vec![raw_entry(&file, 0, "error", "first line")], &[], 0),
        );
        let entry = &answer["entries"][0];
        assert_eq!(entry["path"], "src/a.rs");
        assert_eq!(entry["line"], 1);
        assert_eq!(entry["column"], 5);
        assert_eq!(entry["end_line"], 2);
        assert_eq!(entry["end_column"], 10);
        assert_eq!(entry["severity"], "error");
        assert_eq!(answer["left_out"], 0);
        assert_eq!(answer["modified_files"], json!([]));
    }

    #[test]
    fn the_answer_never_names_an_outside_buffer() {
        let project = project("outside");
        let inside = project.root.join("src/a.rs");
        let elsewhere = project.beside.join("b.rs");
        let through_link = project.root.join("src/out.rs");
        let value = reply(
            vec![
                raw_entry(&elsewhere, 0, "error", "outside"),
                raw_entry(&inside, 1, "error", "kept"),
                raw_entry(&through_link, 2, "error", "through a link"),
                raw_entry(Path::new("term://~/x//1:bash"), 3, "error", "not a file"),
            ],
            &[&elsewhere, &through_link, &inside],
            0,
        );
        let answer = told(&project.root, &value);
        assert_eq!(answer["entries"].as_array().unwrap().len(), 1, "{answer}");
        assert_eq!(answer["entries"][0]["message"], "kept");
        assert_eq!(answer["modified_files"], json!(["src/a.rs"]));
        let text = answer.to_string();
        assert!(!text.contains("beside"), "{text}");
        assert!(!text.contains("out.rs"), "{text}");
        assert!(!text.contains("term://"), "{text}");
    }

    #[test]
    fn an_allowed_outside_file_is_told_and_no_other_outside_buffer() {
        let project = project("allowed");
        let elsewhere = project.beside.join("b.rs");
        let other = project.beside.join("c.rs");
        let inside = project.root.join("src/a.rs");
        let value = reply(
            vec![
                raw_entry(&elsewhere, 0, "error", "the allowed one"),
                raw_entry(&other, 1, "error", "another outside"),
                raw_entry(&inside, 2, "warning", "inside"),
            ],
            &[&elsewhere, &other],
            0,
        );
        let result = with_cx(&project.root, |cx| result_for(cx, &value, Some(&elsewhere)));
        assert!(!result.is_error, "{}", result.text);
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        let messages: Vec<&str> = answer["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["message"].as_str().unwrap())
            .collect();
        assert_eq!(messages, ["the allowed one", "inside"]);
        assert_eq!(answer["entries"][0]["path"], elsewhere.to_string_lossy().as_ref());
        assert_eq!(answer["modified_files"], json!([elsewhere.to_string_lossy()]));
        // Nothing allowed: the outside buffers are dropped as before.
        let none = told(&project.root, &value);
        assert_eq!(none["entries"].as_array().unwrap().len(), 1, "{none}");
    }

    #[test]
    fn an_allowed_outside_file_named_through_a_link_is_told_by_its_real_path() {
        let project = project("allowed-link");
        let real = project.beside.join("b.rs");
        let through_link = project.root.join("src/out.rs");
        let value = reply(vec![raw_entry(&through_link, 0, "error", "via link")], &[], 0);
        let result = with_cx(&project.root, |cx| result_for(cx, &value, Some(&real)));
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        assert_eq!(answer["entries"].as_array().unwrap().len(), 1, "{answer}");
        assert_eq!(answer["entries"][0]["path"], real.to_string_lossy().as_ref());
    }

    #[test]
    fn an_allowed_outside_path_gets_its_diagnostics_back() {
        let project = project("card-then-answer");
        let elsewhere = project.beside.join("b.rs");
        with_cx(&project.root, |cx| {
            let mut run = DiagnosticsRun {
                raw: Some(elsewhere.display().to_string()),
                min_severity: Severity::Hint,
                since: None,
                approved: None,
                stage: Stage::Judging,
            };
            assert!(matches!(run.judge(cx), Step::Ask(_)));
            let Step::Call { .. } = run.card_answered(true, cx) else {
                panic!("an allowed card asks the editor");
            };
            let value = reply(vec![raw_entry(&elsewhere, 0, "error", "seen")], &[], 0);
            let Step::Done(result) = run.answered(Ok(value), cx) else {
                panic!("the answer ends the run");
            };
            assert!(!result.is_error, "{}", result.text);
            let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
            assert_eq!(answer["entries"][0]["message"], "seen", "{answer}");
        });
    }

    #[test]
    fn an_unsaved_new_files_entries_stay() {
        let project = project("unsaved");
        let unsaved = project.root.join("src/not_yet_saved.rs");
        assert!(!unsaved.exists());
        let answer = told(
            &project.root,
            &reply(vec![raw_entry(&unsaved, 0, "hint", "new")], &[&unsaved], 0),
        );
        assert_eq!(answer["entries"][0]["path"], "src/not_yet_saved.rs");
        assert_eq!(answer["modified_files"], json!(["src/not_yet_saved.rs"]));
    }

    #[test]
    fn five_hundred_long_messages_still_fit() {
        let project = project("fit");
        let file = project.root.join("src/a.rs");
        // 2 KiB of quotes, each escaped twice by the time it is in a frame.
        let message = "\"".repeat(2048);
        let entries = (0..500).map(|n| raw_entry(&file, n, "error", &message)).collect();
        let result = with_cx(&project.root, |cx| result_of(cx, &reply(entries, &[], 0)));
        assert!(!result.is_error);
        assert!(
            encoded_len(&result.text) <= ANSWER_MAX_ENCODED,
            "{} over {ANSWER_MAX_ENCODED}",
            encoded_len(&result.text)
        );
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        let kept = answer["entries"].as_array().unwrap().len() as u64;
        assert!(kept > 0 && kept < 500, "{kept}");
        assert_eq!(answer["left_out"].as_u64().unwrap(), 500 - kept);
        // One entry more would not have fitted.
        let all: Vec<serde_json::Value> = (0..500)
            .map(|n| {
                let entry = parse(&reply(vec![raw_entry(&file, n, "error", &message)], &[], 0)).unwrap();
                entry_json("src/a.rs", &entry.entries[0])
            })
            .collect();
        assert!(encoded_len(&answer_text(&all, &[], 0, kept as usize + 1)) > ANSWER_MAX_ENCODED);
    }

    #[test]
    fn min_severity_is_checked() {
        let project = project("severity");
        with_cx(&project.root, |cx| {
            let begin = |args: serde_json::Value| Diagnostics.begin(&args, cx).map(|_| ()).unwrap_err().0;
            assert_eq!(
                begin(json!({"min_severity": "fatal"})),
                "min_severity must be one of error, warning, info, hint"
            );
            assert_eq!(begin(json!({"path": ""})), "path may not be empty");
            assert!(Diagnostics.begin(&json!({}), cx).is_ok());
            assert!(Diagnostics
                .begin(&json!({"min_severity": "warning", "path": "a"}), cx)
                .is_ok());
        });
    }

    #[test]
    fn two_outside_names_that_read_alike_are_not_one_place() {
        use std::os::unix::ffi::OsStrExt;
        let project = project("outside-alike");
        let named = project.beside.join(std::ffi::OsStr::from_bytes(b"secret\xff"));
        let other = project.beside.join(std::ffi::OsStr::from_bytes(b"secret\xfe"));
        fs::write(&named, "named").unwrap();
        fs::write(&other, "other").unwrap();
        assert_eq!(named.display().to_string(), other.display().to_string());
        let link = project.root.join("alike.rs");
        std::os::unix::fs::symlink(&named, &link).unwrap();
        with_cx(&project.root, |cx| {
            let mut run = DiagnosticsRun {
                raw: Some("alike.rs".to_owned()),
                min_severity: Severity::Hint,
                since: None,
                approved: None,
                stage: Stage::Judging,
            };
            assert!(matches!(run.judge(cx), Step::Ask(_)));
            fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&other, &link).unwrap();
            let allowed = run.card_answered(true, cx);
            assert!(
                matches!(&allowed, Step::Done(ToolResult { text, is_error: true }) if text.contains("now leads somewhere other")),
                "{allowed:?}"
            );
            assert_eq!(run.approved, None, "no outside buffer may be told");
        });
    }

    #[test]
    fn the_outside_card_approves_only_the_place_it_named() {
        let project = project("outside-retarget");
        let named = project.beside.join("b.rs");
        let other = project.beside.join("other.txt");
        fs::write(&other, "other").unwrap();
        let link = project.root.join("src/out.rs");
        let run_of = || DiagnosticsRun {
            raw: Some("src/out.rs".to_owned()),
            min_severity: Severity::Hint,
            since: None,
            approved: None,
            stage: Stage::Judging,
        };
        with_cx(&project.root, |cx| {
            let mut run = run_of();
            assert!(matches!(run.judge(cx), Step::Ask(_)));
            fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&other, &link).unwrap();
            let allowed = run.card_answered(true, cx);
            assert!(
                matches!(&allowed, Step::Done(ToolResult { text, is_error: true }) if text.contains("now leads somewhere other")),
                "{allowed:?}"
            );
            assert_eq!(run.approved, None, "no outside buffer may be told");

            // Left alone, the same card asks about the place it named.
            fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&named, &link).unwrap();
            let mut run = run_of();
            assert!(matches!(run.judge(cx), Step::Ask(_)));
            assert!(matches!(run.card_answered(true, cx), Step::Call { .. }));
            assert_eq!(run.approved, Some(named.clone()));
        });
    }

    fn start(project: &Project, args: serde_json::Value) -> (Box<dyn ToolRun>, Step) {
        with_cx(&project.root, |cx| {
            let mut run = Diagnostics.begin(&args, cx).unwrap();
            let step = run.step(StepEvent::Start, cx);
            (run, step)
        })
    }

    #[test]
    fn the_run_asks_for_the_whole_project_or_one_judged_path() {
        let project = project("run");
        let (_, step) = start(&project, json!({"min_severity": "error"}));
        match step {
            Step::Call {
                call:
                    EditorCall::Diagnostics {
                        root,
                        path: None,
                        min_severity: Severity::Error,
                        limit: DIAGNOSTICS_LIMIT,
                    },
                ..
            } => assert_eq!(root, project.root),
            other => panic!("expected a call for the whole project, got {other:?}"),
        }
        for (raw, expected) in [("src/a.rs", "src/a.rs"), ("src/new.rs", "src/new.rs")] {
            let (_, step) = start(&project, json!({ "path": raw }));
            let Step::Call {
                call: EditorCall::Diagnostics { path: Some(path), .. },
                ..
            } = step
            else {
                panic!("expected a call for {raw}, got {step:?}");
            };
            assert_eq!(path, project.root.join(expected));
        }
        let outside = project.beside.join("b.rs");
        let (mut run, step) = start(&project, json!({ "path": outside }));
        let Step::Ask(card) = &step else {
            panic!("expected a card, got {step:?}");
        };
        assert!(card.title.contains(&outside.display().to_string()), "{}", card.title);
        let again = with_cx(&project.root, |cx| run.step(StepEvent::Start, cx));
        assert!(
            matches!(again, Step::Done(ToolResult { is_error: true, .. })),
            "a second start asks nothing"
        );
    }

    /// The baseline a tab keeps after a turn began with `files` holding `before` entries, each `(file, message)`.
    fn taken(root: &Path, before: &[(&str, &str)], cut: bool) -> Memory {
        let mut counts = BTreeMap::new();
        for (file, message) in before {
            let entry = parse(&reply(vec![raw_entry(&root.join(file), 0, "error", message)], &[], 0))
                .unwrap()
                .entries
                .remove(0);
            *counts.entry(BaselineKey::of(&entry)).or_insert(0) += 1;
        }
        let memory = Memory::default();
        memory.with(1, |tab| {
            tab.baseline = Some(TurnBaseline::Taken {
                turn: "t1".to_owned(),
                counts,
                cut,
            })
        });
        memory
    }

    /// Runs `since: "turn"` (and `extra` arguments) over a tab's `memory`, the editor answering `value`: the call it
    /// made and what it ended with.
    fn since_run(
        project: &Project,
        memory: &Memory,
        extra: serde_json::Value,
        value: &Value,
    ) -> (Option<EditorCall>, ToolResult) {
        let mut args = json!({"since": "turn"});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        with_memory_cx(&project.root, memory, |cx| {
            let mut run = Diagnostics.begin(&args, cx).unwrap();
            match run.step(StepEvent::Start, cx) {
                Step::Done(result) => (None, result),
                Step::Call { call, .. } => {
                    let Step::Done(result) = run.step(StepEvent::Answer(Ok(value.clone())), cx) else {
                        panic!("the answer ends the run");
                    };
                    (Some(call), result)
                }
                other => panic!("unexpected {other:?}"),
            }
        })
    }

    fn messages_of(result: &ToolResult) -> Vec<String> {
        assert!(!result.is_error, "{}", result.text);
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        answer["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["message"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn since_turn_asks_for_the_whole_reading_and_tells_what_appeared() {
        let project = project("since");
        let a = project.root.join("src/a.rs");
        let memory = taken(&project.root, &[("src/a.rs", "old"), ("src/a.rs", "gone")], false);
        let now = reply(
            vec![raw_entry(&a, 7, "error", "new"), raw_entry(&a, 30, "error", "old")],
            &[],
            0,
        );
        let (call, result) = since_run(&project, &memory, json!({"min_severity": "error"}), &now);
        let Some(EditorCall::Diagnostics {
            path: None,
            min_severity: Severity::Error,
            limit: BASELINE_LIMIT,
            ..
        }) = call
        else {
            panic!("expected the whole reading, got {call:?}");
        };
        assert_eq!(
            messages_of(&result),
            ["new"],
            "an entry that moved lines is not new, one that went is not told"
        );
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        assert!(answer.get("baseline_cut").is_none(), "{answer}");
        assert_eq!(answer["left_out"], 0);
    }

    #[test]
    fn a_cut_side_is_said() {
        let project = project("since-cut");
        let a = project.root.join("src/a.rs");
        let now = |left_out| reply(vec![raw_entry(&a, 7, "error", "new")], &[], left_out);
        for (baseline_cut, current_left_out) in [(true, 0), (false, 4)] {
            let memory = taken(&project.root, &[], baseline_cut);
            let (_, result) = since_run(&project, &memory, json!({}), &now(current_left_out));
            let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
            assert_eq!(answer["baseline_cut"], true, "{answer}");
            assert_eq!(answer["left_out"], current_left_out);
        }
    }

    #[test]
    fn no_baseline_says_so() {
        let project = project("since-none");
        let a = project.root.join("src/a.rs");
        let now = reply(vec![raw_entry(&a, 7, "error", "new")], &[], 0);
        let said = |memory: &Memory| {
            let (call, result) = since_run(&project, memory, json!({}), &now);
            assert!(call.is_none(), "nothing is asked of the editor: {call:?}");
            assert!(result.is_error);
            result.text
        };
        assert_eq!(said(&Memory::default()), "no baseline for this turn");
    }

    #[test]
    fn a_taking_baseline_says_no_baseline() {
        let project = project("since-taking");
        let memory = Memory::default();
        memory.with(1, |tab| {
            tab.baseline = Some(TurnBaseline::Taking { turn: "t1".to_owned() })
        });
        let (call, result) = since_run(&project, &memory, json!({}), &Value::Nil);
        assert!(call.is_none());
        assert_eq!(
            (result.is_error, result.text.as_str()),
            (true, "no baseline for this turn")
        );
    }

    #[test]
    fn a_failed_baseline_says_no_baseline() {
        let project = project("since-failed");
        let memory = Memory::default();
        memory.with(1, |tab| {
            tab.baseline = Some(TurnBaseline::Failed { turn: "t1".to_owned() })
        });
        let (call, result) = since_run(&project, &memory, json!({}), &Value::Nil);
        assert!(call.is_none());
        assert_eq!(
            (result.is_error, result.text.as_str()),
            (true, "no baseline for this turn")
        );
    }

    #[test]
    fn a_baseline_replaced_while_the_editor_answers_is_no_baseline() {
        let project = project("since-replaced");
        let a = project.root.join("src/a.rs");
        let memory = taken(&project.root, &[], false);
        with_memory_cx(&project.root, &memory, |cx| {
            let mut run = Diagnostics.begin(&json!({"since": "turn"}), cx).unwrap();
            assert!(matches!(run.step(StepEvent::Start, cx), Step::Call { .. }));
            // The next turn began while the editor was asked.
            memory.with(1, |tab| {
                tab.baseline = Some(TurnBaseline::Taking { turn: "t2".to_owned() })
            });
            let value = reply(vec![raw_entry(&a, 1, "error", "m")], &[], 0);
            let Step::Done(result) = run.step(StepEvent::Answer(Ok(value)), cx) else {
                panic!("the answer ends the run");
            };
            assert_eq!(
                (result.is_error, result.text.as_str()),
                (true, "no baseline for this turn")
            );
        });
    }

    #[test]
    fn an_outside_entry_is_dropped() {
        let project = project("since-outside");
        let elsewhere = project.beside.join("b.rs");
        let a = project.root.join("src/a.rs");
        let memory = taken(&project.root, &[], false);
        let now = reply(
            vec![
                raw_entry(&elsewhere, 0, "error", "outside"),
                raw_entry(&a, 1, "error", "inside"),
            ],
            &[&elsewhere],
            0,
        );
        let (_, result) = since_run(&project, &memory, json!({}), &now);
        assert_eq!(messages_of(&result), ["inside"]);
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        assert_eq!(answer["modified_files"], json!([]));
        assert!(!result.text.contains("beside"), "{}", result.text);
    }

    #[test]
    fn the_cap_applies_after_the_difference() {
        let project = project("since-cap");
        let a = project.root.join("src/a.rs");
        let old: Vec<String> = (0..600).map(|n| format!("old {n}")).collect();
        let before: Vec<(&str, &str)> = old.iter().map(|message| ("src/a.rs", message.as_str())).collect();
        let memory = taken(&project.root, &before, false);
        let mut entries: Vec<Value> = old
            .iter()
            .enumerate()
            .map(|(n, message)| raw_entry(&a, n as u32, "error", message))
            .collect();
        entries.extend((0..3).map(|n| raw_entry(&a, 700 + n, "error", &format!("new {n}"))));
        let (_, result) = since_run(&project, &memory, json!({}), &reply(entries, &[], 0));
        assert_eq!(messages_of(&result), ["new 0", "new 1", "new 2"]);
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        assert_eq!(answer["left_out"], 0);
    }

    #[test]
    fn the_cap_counts_what_it_leaves_out() {
        let project = project("since-cap-over");
        let a = project.root.join("src/a.rs");
        let memory = taken(&project.root, &[], false);
        let entries = (0..DIAGNOSTICS_LIMIT + 7)
            .map(|n| raw_entry(&a, n, "error", &format!("new {n}")))
            .collect();
        let (_, result) = since_run(&project, &memory, json!({}), &reply(entries, &[], 0));
        assert_eq!(messages_of(&result).len(), DIAGNOSTICS_LIMIT as usize);
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        assert_eq!(answer["left_out"], 7);
    }

    #[test]
    fn since_with_a_path_compares_that_file() {
        let project = project("since-path");
        let a = project.root.join("src/a.rs");
        let memory = taken(&project.root, &[("src/a.rs", "old"), ("src/b.rs", "elsewhere")], false);
        let now = reply(
            vec![raw_entry(&a, 3, "error", "old"), raw_entry(&a, 4, "error", "new")],
            &[],
            0,
        );
        let (call, result) = since_run(&project, &memory, json!({"path": "src/a.rs"}), &now);
        let Some(EditorCall::Diagnostics {
            path: Some(path),
            limit: BASELINE_LIMIT,
            ..
        }) = call
        else {
            panic!("expected a call for the one file, got {call:?}");
        };
        assert_eq!(path, a);
        assert_eq!(messages_of(&result), ["new"]);
    }

    #[test]
    fn since_for_a_file_outside_the_project_is_refused_before_a_card() {
        let project = project("since-outside-path");
        let memory = taken(&project.root, &[], false);
        let outside = project.beside.join("b.rs");
        let (call, result) = since_run(&project, &memory, json!({"path": outside}), &Value::Nil);
        assert!(call.is_none());
        assert!(
            result.is_error && result.text.contains("project's files only"),
            "{}",
            result.text
        );
    }

    #[test]
    fn any_other_since_is_refused() {
        let project = project("since-refused");
        with_cx(&project.root, |cx| {
            let begin = |args: serde_json::Value| Diagnostics.begin(&args, cx).map(|_| ()).unwrap_err().0;
            for bad in [json!("session"), json!("TURN"), json!(1), json!(true)] {
                assert_eq!(begin(json!({ "since": bad })), "since must be \"turn\"");
            }
            assert!(Diagnostics.begin(&json!({"since": "turn"}), cx).is_ok());
            assert!(Diagnostics.begin(&json!({"since": null}), cx).is_ok());
        });
    }

    #[test]
    fn the_schema_offers_since_turn_only() {
        let schema: serde_json::Value = serde_json::from_str(ROW.schema).unwrap();
        assert_eq!(schema["properties"]["since"]["enum"], json!(["turn"]));
    }
}
