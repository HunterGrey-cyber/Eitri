//! Non-blocking LSP requests and bounded, single-use collections.

use crate::editor_tools::stamp::monotonic_now;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use rmpv::Value;
use serde::Serialize;

use super::path_value;
use crate::editor_lines::FileFormat;
use crate::editor_rpc::EditorRpc;
use crate::editor_tools::cards::{CardLine, CardRequest};
use crate::editor_tools::gate::{verdict, Need, Verdict};
use crate::editor_tools::paths::{self, Target};
use crate::editor_tools::stamp::{Stamp, EDITOR_BUDGET};
use crate::editor_tools::step::{EditorFailure, EditorReach, Step, StepEvent, ToolCx, ToolResult};
pub use crate::editor_tools::text_edits::Encoding;
use crate::editor_tools::way::EditorCall;
use crate::editor_tools::words;
use crate::nvim_rpc::Pending;
use crate::project_trust::nofollow::{read_regular_nofollow, Owner};

pub const LSP_BUDGET: Duration = Duration::from_secs(10);
/// The last collect's stamp budget and the extra life that lets it find the entry.
pub const COLLECT_MARGIN: Duration = EDITOR_BUDGET;
pub const MAX_RESULTS: usize = 200;
/// How long a started ask rests between collects.
pub const POLL: Duration = Duration::from_millis(100);
/// How long a file just loaded may take to get its language server: a client attaches only once `initialize` has
/// answered, after the load has returned.
pub const ATTACH_WAIT: Duration = Duration::from_secs(5);
/// How long the run rests between asks while a client attaches.
const ATTACH_POLL: Duration = Duration::from_millis(200);
/// The most of a file read for one item's line, the most the editor would load.
const LINE_FILE_MAX: u64 = 16 * 1024 * 1024;
/// The most characters told of one line.
const LINE_CHARS_MAX: usize = 300;
/// How long a run rests while a [`worker`] makes the answer.
const TELLING_POLL: Duration = Duration::from_millis(20);
/// How long a run waits for its worker. The work takes no time on a disk that answers; a dead network mount holds its
/// calls for as long as it stays dead.
const TELLING_LIMIT: Duration = Duration::from_secs(10);

const RESTARTED: &str = "the editor restarted; nothing was read";
const NOTHING_ASKED: &str = "the editor was asked nothing";
/// What a tool says of an answer it cannot make out.
pub const UNREADABLE: &str = "the language server's answer could not be read";

pub const LSP_ASK_LUA: &str = concat!(
    include_str!("../prelude.lua"),
    "\n",
    include_str!("../../find_buf.lua"),
    "\n",
    include_str!("load_file.lua"),
    "\n",
    include_str!("lsp_common.lua"),
    "\n",
    include_str!("lsp_ask.lua")
);
pub const LSP_COLLECT_LUA: &str = concat!(
    include_str!("../prelude.lua"),
    "\n",
    include_str!("../../find_buf.lua"),
    "\n",
    include_str!("load_file.lua"),
    "\n",
    include_str!("lsp_common.lua"),
    "\n",
    include_str!("lsp_collect.lua")
);

pub const LSP_TARGETS_LUA: &str = chunk!("lsp_targets.lua");

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LspKind {
    #[default]
    Usages,
    Hover,
    DocumentSymbols,
    WorkspaceSymbols,
    Rename,
    CodeActions,
    ActionEdit,
    Format,
}
impl LspKind {
    fn word(self) -> &'static str {
        match self {
            Self::Usages => "usages",
            Self::Hover => "hover",
            Self::DocumentSymbols => "document_symbols",
            Self::WorkspaceSymbols => "workspace_symbols",
            Self::Rename => "rename",
            Self::CodeActions => "code_actions",
            Self::ActionEdit => "action_edit",
            Self::Format => "format",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsagePart {
    Definitions,
    References,
    Implementations,
}
impl UsagePart {
    fn word(self) -> &'static str {
        match self {
            Self::Definitions => "definitions",
            Self::References => "references",
            Self::Implementations => "implementations",
        }
    }

    pub fn from_word(word: &str) -> Option<UsagePart> {
        match word {
            "definitions" => Some(Self::Definitions),
            "references" => Some(Self::References),
            "implementations" => Some(Self::Implementations),
            _ => None,
        }
    }
}
#[derive(Debug, Default, Clone)]
pub struct AskArgs {
    pub id: String,
    pub kind: LspKind,
    pub root: PathBuf,
    pub path: Option<PathBuf>,
    /// One-based line number.
    pub line: Option<u32>,
    pub symbol: Option<String>,
    /// An empty list asks all three parts.
    pub include: Vec<UsagePart>,
    pub query: Option<String>,
    /// A query per client name, asked of that client in place of `query`. Empty: every client gets `query`.
    pub query_for: Vec<(String, String)>,
    pub new_name: Option<String>,
    pub action: Option<String>,
    /// Remaining budget plus `COLLECT_MARGIN`, in milliseconds.
    pub budget_ms: u32,
    pub load: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Asked {
    Started {
        unsupported: Vec<String>,
        action: Option<ActionFacts>,
        /// The names of the clients a request went to (one without the method was not asked), each at most 64 bytes, at
        /// most 8.
        clients: Vec<String>,
        /// Those of `clients` with an unfinished work-done progress when the ask went out.
        busy: Vec<String>,
    },
    ActionGone,
    NoEdit {
        has_command: bool,
    },
    NoClient {
        loaded_now: bool,
    },
    NeedsLoad,
    Swap,
    NoSymbol,
    NoLine {
        line_count: u32,
    },
    Refused(String),
}
#[derive(Debug)]
pub struct RawItem {
    pub path: String,
    pub line: u32,
    pub character: u32,
    /// The units `character` counts in: the answering client's position encoding.
    pub encoding: Encoding,
    pub text: Option<String>,
    /// The actual buffer's name, kept as bytes for the caller's boundary check.
    pub text_from: Option<PathBuf>,
    /// What that buffer's text was read from, when the editor keeps a record of it.
    pub read_from: Option<PathBuf>,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub container: Option<String>,
}
#[derive(Debug)]
pub struct RawPart {
    pub name: String,
    pub items: Vec<RawItem>,
    pub total: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTextEdit {
    /// Zero-based positions in the server's units, against the text at the ask.
    pub start: (u32, u32),
    pub end: (u32, u32),
    pub new_text: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFileEdit {
    pub path: String,
    pub edits: Vec<RawTextEdit>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEdit {
    pub encoding: Encoding,
    pub files: Vec<RawFileEdit>,
    pub resource_ops: Vec<String>,
    pub too_large: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAction {
    pub id: String,
    pub title: String,
    pub kind: Option<String>,
    pub has_edit: bool,
    pub has_command: bool,
    pub disabled: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionFacts {
    pub title: String,
    pub has_command: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetState {
    pub found: bool,
    pub modified: bool,
    pub buf: Option<u64>,
    pub changedtick: Option<u64>,
    pub sha256: Option<String>,
    pub format: FileFormat,
    /// nvim's `'endofline'`: whether the language server was told the last line ends. False for a target not loaded.
    pub eol: bool,
}

pub fn targets(rpc: &dyn EditorRpc, stamp: &Stamp, paths: &[PathBuf]) -> Pending {
    if paths.len() > 64 {
        return Pending::failed(crate::nvim_rpc::RpcError::Encode(
            "at most 64 LSP targets may be checked".into(),
        ));
    }
    rpc.exec_lua(
        LSP_TARGETS_LUA,
        vec![
            stamp.to_value(),
            Value::Map(vec![(
                "paths".into(),
                Value::Array(paths.iter().map(|p| path_value(p)).collect()),
            )]),
        ],
    )
}

pub fn parse_targets(value: &Value) -> Result<Vec<TargetState>, String> {
    array(value)?
        .iter()
        .map(|item| {
            let found = required(item, "found")?
                .as_bool()
                .ok_or("the target has an invalid found")?;
            Ok(if found {
                TargetState {
                    found,
                    modified: required(item, "modified")?
                        .as_bool()
                        .ok_or("the target has an invalid modified")?,
                    buf: Some(wide_number(item, "buf")?),
                    changedtick: Some(wide_number(item, "changedtick")?),
                    sha256: Some(text(required(item, "sha256")?)?),
                    format: match text(required(item, "fileformat")?)?.as_str() {
                        "unix" => FileFormat::Unix,
                        "dos" => FileFormat::Dos,
                        _ => FileFormat::Other,
                    },
                    eol: required(item, "eol")?
                        .as_bool()
                        .ok_or("the target has an invalid eol")?,
                }
            } else {
                TargetState {
                    found,
                    modified: false,
                    buf: None,
                    changedtick: None,
                    sha256: None,
                    format: FileFormat::Other,
                    eol: false,
                }
            })
        })
        .collect()
}

#[derive(Debug)]
pub enum Collected {
    Pending,
    Gone,
    Done {
        parts: Vec<RawPart>,
        hover: Option<String>,
        /// The hover text was cut at its cap.
        cut: bool,
        missing: Vec<String>,
        edit: Option<RawEdit>,
        actions: Vec<RawAction>,
        action: Option<ActionFacts>,
        ticks: BTreeMap<u64, u64>,
        /// When the ask was made, on the clock of a file's modification time. A stored action keeps its first ask's.
        asked_ms: Option<u64>,
    },
}

pub fn ask(rpc: &dyn EditorRpc, stamp: &Stamp, args: &AskArgs) -> Pending {
    let mut values = vec![
        ("id".into(), args.id.as_str().into()),
        ("kind".into(), args.kind.word().into()),
        ("root".into(), path_value(&args.root)),
        ("budget_ms".into(), args.budget_ms.into()),
        ("load".into(), args.load.into()),
        (
            "include".into(),
            Value::Array(args.include.iter().map(|p| p.word().into()).collect()),
        ),
    ];
    if let Some(path) = &args.path {
        values.push(("path".into(), path_value(path)));
    }
    if let Some(line) = args.line {
        values.push(("line".into(), line.into()));
    }
    if let Some(symbol) = &args.symbol {
        values.push(("symbol".into(), symbol.as_str().into()));
    }
    if let Some(query) = &args.query {
        values.push(("query".into(), query.as_str().into()));
    }
    if !args.query_for.is_empty() {
        values.push((
            "query_for".into(),
            Value::Map(
                args.query_for
                    .iter()
                    .map(|(client, query)| (client.as_str().into(), query.as_str().into()))
                    .collect(),
            ),
        ));
    }
    if let Some(name) = &args.new_name {
        values.push(("new_name".into(), name.as_str().into()));
    }
    if let Some(action) = &args.action {
        values.push(("action".into(), action.as_str().into()));
    }
    rpc.exec_lua(LSP_ASK_LUA, vec![stamp.to_value(), Value::Map(values)])
}
pub fn collect(rpc: &dyn EditorRpc, stamp: &Stamp, id: &str, last: bool) -> Pending {
    rpc.exec_lua(
        LSP_COLLECT_LUA,
        vec![
            stamp.to_value(),
            Value::Map(vec![("id".into(), id.into()), ("last".into(), last.into())]),
        ],
    )
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(key, _)| key.as_slice() == Some(name.as_bytes()))
        .map(|(_, v)| v)
}
fn required<'a>(value: &'a Value, name: &str) -> Result<&'a Value, String> {
    field(value, name).ok_or_else(|| format!("the LSP answer has no {name}"))
}
fn text(value: &Value) -> Result<String, String> {
    value
        .as_slice()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .ok_or_else(|| "the LSP answer has a non-string value".into())
}
fn optional_text(value: &Value, name: &str) -> Result<Option<String>, String> {
    field(value, name).filter(|v| !v.is_nil()).map(text).transpose()
}
fn number(value: &Value, name: &str) -> Result<u32, String> {
    required(value, name)?
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| format!("the LSP answer has an invalid {name}"))
}
fn array(value: &Value) -> Result<&[Value], String> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| "the LSP answer has a non-list value".into())
}
fn texts(value: &Value) -> Result<Vec<String>, String> {
    array(value)?.iter().map(text).collect()
}
/// A list of strings that may be absent, or empty as a map: what nvim sends for an empty table.
fn optional_texts(value: &Value, name: &str) -> Result<Vec<String>, String> {
    match field(value, name) {
        Some(Value::Array(items)) => items.iter().map(text).collect(),
        Some(Value::Map(entries)) if entries.is_empty() => Ok(Vec::new()),
        Some(Value::Nil) | None => Ok(Vec::new()),
        Some(_) => Err(format!("the LSP answer has an invalid {name}")),
    }
}
fn yes(value: &Value, name: &str) -> bool {
    field(value, name).and_then(Value::as_bool) == Some(true)
}

fn wide_number(value: &Value, name: &str) -> Result<u64, String> {
    required(value, name)?
        .as_u64()
        .ok_or_else(|| format!("the LSP answer has an invalid {name}"))
}
fn action_facts(value: &Value) -> Result<Option<ActionFacts>, String> {
    field(value, "action")
        .filter(|v| !v.is_nil())
        .map(|action| {
            Ok(ActionFacts {
                title: text(required(action, "title")?)?,
                has_command: yes(action, "has_command"),
            })
        })
        .transpose()
}
fn parse_edit(value: &Value) -> Result<RawEdit, String> {
    let encoding = match text(required(value, "encoding")?)?.as_str() {
        "utf-8" => Encoding::Utf8,
        "utf-16" => Encoding::Utf16,
        "utf-32" => Encoding::Utf32,
        _ => return Err("the LSP answer has an invalid encoding".into()),
    };
    let files = array(required(value, "files")?)?
        .iter()
        .map(|file| {
            let edits = array(required(file, "edits")?)?
                .iter()
                .map(|edit| {
                    let start = required(edit, "start")?;
                    let end = required(edit, "end")?;
                    Ok(RawTextEdit {
                        start: (number(start, "line")?, number(start, "character")?),
                        end: (number(end, "line")?, number(end, "character")?),
                        new_text: text(required(edit, "new_text")?)?,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(RawFileEdit {
                path: text(required(file, "path")?)?,
                edits,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(RawEdit {
        encoding,
        files,
        resource_ops: texts(required(value, "resource_ops")?)?,
        too_large: yes(value, "too_large"),
    })
}

pub fn parse_asked(value: &Value) -> Result<Asked, String> {
    if yes(value, "started") {
        return Ok(Asked::Started {
            unsupported: field(value, "unsupported").map(texts).transpose()?.unwrap_or_default(),
            action: action_facts(value)?,
            clients: optional_texts(value, "clients")?,
            busy: optional_texts(value, "busy")?,
        });
    }
    if yes(value, "action_gone") {
        return Ok(Asked::ActionGone);
    }
    if yes(value, "no_edit") {
        return Ok(Asked::NoEdit {
            has_command: yes(value, "has_command"),
        });
    }
    if yes(value, "no_client") {
        return Ok(Asked::NoClient {
            loaded_now: yes(value, "loaded_now"),
        });
    }
    if yes(value, "needs_load") {
        return Ok(Asked::NeedsLoad);
    }
    if yes(value, "swap") {
        return Ok(Asked::Swap);
    }
    if yes(value, "no_symbol") {
        return Ok(Asked::NoSymbol);
    }
    if yes(value, "no_line") {
        return Ok(Asked::NoLine {
            line_count: number(value, "line_count")?,
        });
    }
    if let Some(why) = field(value, "refused") {
        return Ok(Asked::Refused(text(why)?));
    }
    Err("the editor's LSP ask answer could not be read".into())
}
pub fn parse_collected(value: &Value) -> Result<Collected, String> {
    match text(required(value, "status")?)?.as_str() {
        "pending" => Ok(Collected::Pending),
        "gone" => Ok(Collected::Gone),
        "done" => {
            let parts = array(required(value, "parts")?)?
                .iter()
                .map(|part| {
                    let items = array(required(part, "items")?)?
                        .iter()
                        .map(|item| {
                            let name_in = |key: &str| {
                                field(item, key)
                                    .filter(|v| !v.is_nil())
                                    .map(|v| {
                                        v.as_slice()
                                            .map(|b| PathBuf::from(OsStr::from_bytes(b)))
                                            .ok_or_else(|| format!("the LSP answer has an invalid {key}"))
                                    })
                                    .transpose()
                            };
                            let text_from = name_in("text_from")?;
                            let read_from = name_in("read_from")?;
                            Ok(RawItem {
                                path: text(required(item, "path")?)?,
                                line: number(item, "line")?,
                                character: number(item, "character")?,
                                // LSP's default when a client names none, or a unit outside the three.
                                encoding: field(item, "encoding")
                                    .and_then(Value::as_slice)
                                    .and_then(|name| std::str::from_utf8(name).ok())
                                    .and_then(Encoding::from_name)
                                    .unwrap_or(Encoding::Utf16),
                                text: optional_text(item, "text")?,
                                text_from,
                                read_from,
                                name: optional_text(item, "name")?,
                                kind: optional_text(item, "kind")?,
                                container: optional_text(item, "container")?,
                            })
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    Ok(RawPart {
                        name: text(required(part, "name")?)?,
                        items,
                        total: number(part, "total")?,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(Collected::Done {
                parts,
                hover: optional_text(value, "hover")?,
                cut: yes(value, "cut"),
                missing: texts(required(value, "missing")?)?,
                edit: field(value, "edit")
                    .filter(|v| !v.is_nil())
                    .map(parse_edit)
                    .transpose()?,
                actions: field(value, "actions")
                    .map(|actions| {
                        array(actions)?
                            .iter()
                            .map(|action| {
                                Ok(RawAction {
                                    id: text(required(action, "id")?)?,
                                    title: text(required(action, "title")?)?,
                                    kind: optional_text(action, "kind")?,
                                    has_edit: yes(action, "has_edit"),
                                    has_command: yes(action, "has_command"),
                                    disabled: optional_text(action, "disabled")?,
                                })
                            })
                            .collect::<Result<Vec<_>, String>>()
                    })
                    .transpose()?
                    .unwrap_or_default(),
                action: action_facts(value)?,
                ticks: field(value, "ticks")
                    .map(|ticks| {
                        array(ticks)?
                            .iter()
                            .map(|tick| Ok((wide_number(tick, "buf")?, wide_number(tick, "tick")?)))
                            .collect::<Result<BTreeMap<_, _>, String>>()
                    })
                    .transpose()?
                    .unwrap_or_default(),
                asked_ms: field(value, "asked_ms")
                    .is_some()
                    .then(|| wide_number(value, "asked_ms"))
                    .transpose()?,
            })
        }
        _ => Err("the editor's LSP collect status could not be read".into()),
    }
}

/// What a step of [`AskCollect`] comes to.
#[derive(Debug)]
pub enum Progress<Stop = Asked> {
    /// The broker's next step.
    Step(Step),
    /// The answer is in; what it says is the tool's to word.
    Finished(Collected),
    /// A stored action cannot supply an edit; the tool chooses the sentence.
    Stopped(Stop),
    Failed(ToolResult),
}

/// How long a language server has. It counts from the run's first turn and starts again after a card's answer: a
/// card waits on a person and leaves the editor free, so its wait is nobody's slowness.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    started: Instant,
    limit: Duration,
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_BUDGET_EXPIRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_BUDGET_NOW: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_test_budget_expired() {
    TEST_BUDGET_EXPIRED.set(false);
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_budget_expired() -> bool {
    TEST_BUDGET_EXPIRED.get()
}

#[cfg(any(test, feature = "test-support"))]
pub struct TestBudgetClock(Option<Instant>);

#[cfg(any(test, feature = "test-support"))]
impl TestBudgetClock {
    pub fn new() -> Self {
        Self(TEST_BUDGET_NOW.replace(Some(Instant::now())))
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestBudgetClock {
    fn drop(&mut self) {
        TEST_BUDGET_NOW.set(self.0);
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn advance_test_budget_clock(delta: Duration) -> Option<Instant> {
    let now = TEST_BUDGET_NOW.get()? + delta;
    TEST_BUDGET_NOW.set(Some(now));
    Some(now)
}

fn budget_now() -> Instant {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(now) = TEST_BUDGET_NOW.get() {
        return now;
    }
    // The editor tools' own monotonic clock, which unit tests can also drive.
    monotonic_now()
}

impl Budget {
    pub fn new(limit: Duration) -> Budget {
        Budget {
            started: budget_now(),
            limit,
        }
    }

    pub fn spent(&self) -> bool {
        self.left().is_zero()
    }

    fn left(&self) -> Duration {
        let left = self
            .limit
            .saturating_sub(budget_now().saturating_duration_since(self.started));
        #[cfg(any(test, feature = "test-support"))]
        if left.is_zero() {
            TEST_BUDGET_EXPIRED.set(true);
        }
        left
    }

    fn restart(&mut self) {
        self.started = budget_now();
    }
}

enum Stage {
    /// Nothing has run.
    Judging,
    /// A card asks whether a path outside the project may reach a server; `shown` is where the card said it leads.
    OutsideCard {
        shown: PathBuf,
    },
    /// A card asks whether a file that is not open may be loaded.
    LoadCard {
        path: PathBuf,
    },
    /// An ask is out. `missing`: nothing is on disk, so only a buffer already open can answer.
    Asking {
        load: bool,
        missing: bool,
    },
    /// Resting until the file just loaded has its client.
    Attaching,
    /// Resting until the next collect.
    Waiting,
    /// A collect is out.
    Collecting {
        last: bool,
    },
    Over,
}

/// One ask and its collects, as a state machine a tool's run steps: the path's judgement and its cards, the ask,
/// the wait for a client, the collects until the budget ends. What the answer says is the tool's.
pub struct AskCollect {
    template: AskArgs,
    raw_path: Option<String>,
    budget: Budget,
    stage: Stage,
    attempt: u32,
    /// This run loaded the file, so a client may still attach. Only the first ask's answer can say so: to the ones
    /// after it the file is already loaded.
    loaded_now: bool,
    attach_since: Option<Instant>,
    unsupported: Vec<String>,
    action: Option<ActionFacts>,
    refuse_outside: bool,
    /// The one outside file this run may tell of.
    allowed: Option<PathBuf>,
}

fn failed(text: impl Into<String>) -> Progress {
    Progress::Failed(ToolResult {
        text: text.into(),
        is_error: true,
    })
}

impl AskCollect {
    /// `raw_path`: as the agent gave it. `None`: `WorkspaceSymbols` and `ActionEdit`, which name no file.
    pub fn new(template: AskArgs, raw_path: Option<String>, budget: Budget) -> AskCollect {
        AskCollect {
            template,
            raw_path,
            budget,
            stage: Stage::Judging,
            attempt: 0,
            loaded_now: false,
            attach_since: None,
            unsupported: Vec::new(),
            action: None,
            refuse_outside: false,
            allowed: None,
        }
    }

    /// Outside files cannot be proposed for, so refuse them before a card or editor call.
    pub fn refuse_outside(mut self) -> AskCollect {
        self.refuse_outside = true;
        self
    }

    /// The parts no attached client offers, once the ask has started.
    pub fn unsupported(&self) -> &[String] {
        &self.unsupported
    }

    /// Read tools have no stored action to stop on; an edit-only answer is unreadable to them.
    pub fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Progress<std::convert::Infallible> {
        match self.step_with_stops(event, cx) {
            Progress::Step(step) => Progress::Step(step),
            Progress::Finished(answer) => Progress::Finished(answer),
            Progress::Failed(result) => Progress::Failed(result),
            Progress::Stopped(_) => Progress::Failed(ToolResult {
                text: UNREADABLE.to_owned(),
                is_error: true,
            }),
        }
    }

    /// Edit tools word stored-action failures themselves.
    pub fn step_with_stops(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Progress {
        let stage = std::mem::replace(&mut self.stage, Stage::Over);
        match (stage, event) {
            (Stage::Judging, StepEvent::Start) => {
                self.budget.restart();
                self.judge(cx)
            }
            (stage @ (Stage::OutsideCard { .. } | Stage::LoadCard { .. }), StepEvent::Card(answer)) => {
                self.budget.restart();
                self.card_answered(stage, answer.allowed(), cx)
            }
            (Stage::Asking { load, missing }, StepEvent::Answer(answer)) => self.asked(answer, load, missing, cx),
            // The file is loaded by now, and only the first ask may load: nothing was checked since. One that went
            // meanwhile answers that it needs a load, which ends the run.
            (Stage::Attaching, StepEvent::Poll) => self.ask(false, false, cx),
            (Stage::Waiting, StepEvent::Poll) => {
                let last = self.budget.spent();
                self.collect(last)
            }
            (Stage::Collecting { last }, StepEvent::Answer(answer)) => self.collected(answer, last, cx),
            _ => failed(NOTHING_ASKED),
        }
    }

    fn judge(&mut self, cx: &ToolCx<'_>) -> Progress {
        let Some(raw) = self.raw_path.as_deref() else {
            return self.ask(false, false, cx);
        };
        match paths::judge_for(cx, raw) {
            Err(refused) => failed(refused.0),
            Ok(Target::Refused(why)) => failed(why),
            Ok(Target::Inside(path)) => match paths::file_check(&path) {
                Err(refused) => failed(refused.0),
                Ok(()) => {
                    let load = verdict(Need::LoadFile, cx.mode, cx.trust) == Verdict::Allow;
                    self.template.path = Some(path);
                    self.ask(load, false, cx)
                }
            },
            Ok(Target::Missing(path)) => {
                self.template.path = Some(path);
                self.ask(false, true, cx)
            }
            Ok(Target::Outside { shown }) if self.refuse_outside => failed(format!(
                "{} is outside the project; nothing can be proposed there",
                shown.display()
            )),
            Ok(Target::Outside { shown }) => match verdict(Need::OutsideProject, cx.mode, cx.trust) {
                Verdict::Allow => self.outside_approved(None, cx),
                Verdict::Card => {
                    let title = format!("Ask the language server about {}?", shown.display());
                    self.stage = Stage::OutsideCard { shown };
                    Progress::Step(Step::Ask(CardRequest::new(
                        title,
                        vec![CardLine::Plain(
                            "It is outside the project; opening it can start language servers and filetype plugins there"
                                .to_owned(),
                        )],
                    )))
                }
            },
        }
    }

    /// An outside path that may be asked about: where it leads, checked, and loaded if it is not open. After a card,
    /// `approved` is the place the card named: a path that leads elsewhere by now was not approved.
    fn outside_approved(&mut self, approved: Option<&Path>, cx: &ToolCx<'_>) -> Progress {
        let raw = self.raw_path.clone().unwrap_or_default();
        let path = match paths::outside_target(cx.root, &raw) {
            Ok(path) => path,
            Err(refused) => return failed(refused.0),
        };
        if let Some(approved) = approved {
            if path != approved {
                return failed(format!(
                    "{raw} now leads somewhere other than {}; nothing was read",
                    approved.display()
                ));
            }
        }
        match paths::file_check(&path) {
            Ok(()) => {
                self.allowed = Some(path.clone());
                self.template.path = Some(path);
                self.ask(true, false, cx)
            }
            Err(refused) => failed(refused.0),
        }
    }

    /// A file the person allowed to be loaded: the card may have waited minutes, so the path is judged again and
    /// has to be the same file, still a regular file within the limit, before the editor is asked to read it.
    fn load_approved(&mut self, path: PathBuf, cx: &ToolCx<'_>) -> Progress {
        let raw = self.raw_path.clone().unwrap_or_default();
        match paths::judge_for(cx, &raw) {
            Ok(Target::Inside(now)) if now == path => match paths::file_check(&path) {
                Ok(()) => self.ask(true, false, cx),
                Err(refused) => failed(refused.0),
            },
            _ => failed(format!(
                "{} is no longer the file that was to be loaded; nothing was read",
                path.display()
            )),
        }
    }

    fn card_answered(&mut self, stage: Stage, allowed: bool, cx: &ToolCx<'_>) -> Progress {
        if !allowed {
            return failed(words::NOT_ALLOWED);
        }
        match stage {
            Stage::OutsideCard { shown } => self.outside_approved(Some(shown.as_path()), cx),
            Stage::LoadCard { path } => self.load_approved(path, cx),
            _ => failed(NOTHING_ASKED),
        }
    }

    /// `budget_ms` is the entry's life in the editor: it has to outlast the budget by what the last collect may take.
    fn ask(&mut self, load: bool, missing: bool, cx: &ToolCx<'_>) -> Progress {
        self.attempt += 1;
        self.template.id = format!("{}-{}", cx.call, self.attempt);
        self.template.root = cx.root.to_path_buf();
        self.template.load = load;
        let life = self.budget.left() + COLLECT_MARGIN;
        self.template.budget_ms = u32::try_from(life.as_millis()).unwrap_or(u32::MAX);
        self.stage = Stage::Asking { load, missing };
        Progress::Step(Step::Call {
            call: EditorCall::LspAsk(self.template.clone()),
            budget: EDITOR_BUDGET,
        })
    }

    /// The last collect's budget is its own and never what is left: stamped with that it could only answer that it
    /// is past, or arrive after the entry went. The entry outlives the budget by the same margin, so it is there.
    /// Every other collect ends with the budget: one held past it would spend that margin, and the last collect,
    /// sent late, could reach the editor after the entry went and read as a restart.
    fn collect(&mut self, last: bool) -> Progress {
        self.stage = Stage::Collecting { last };
        Progress::Step(Step::Call {
            call: EditorCall::LspCollect {
                id: self.template.id.clone(),
                last,
            },
            budget: if last {
                COLLECT_MARGIN
            } else {
                EDITOR_BUDGET.min(self.budget.left())
            },
        })
    }

    fn rest_before_a_collect(&mut self) -> Progress {
        if self.budget.spent() {
            return self.collect(true);
        }
        self.stage = Stage::Waiting;
        Progress::Step(Step::PollAfter(POLL))
    }

    /// What the file is called in a sentence.
    fn named(&self) -> String {
        match &self.template.path {
            Some(path) => path.display().to_string(),
            None => "the project".to_owned(),
        }
    }

    fn no_client(&mut self, loaded_now: bool) -> Progress {
        self.loaded_now |= loaded_now;
        let named = self.named();
        if !self.loaded_now {
            return failed(format!("no language server is attached to {named}"));
        }
        let since = *self.attach_since.get_or_insert_with(monotonic_now);
        if monotonic_now().saturating_duration_since(since) >= ATTACH_WAIT || self.budget.spent() {
            return failed(format!("no language server attached to {named} within 5 s"));
        }
        self.stage = Stage::Attaching;
        Progress::Step(Step::PollAfter(ATTACH_POLL))
    }

    fn editor_failed(failure: EditorFailure, cx: &ToolCx<'_>) -> Progress {
        match failure {
            EditorFailure::NotAnswered => failed(words::NOT_READ),
            EditorFailure::Unavailable(_) if cx.reach == EditorReach::Link => failed(words::WINDOW_CLOSED),
            EditorFailure::Unavailable(text) | EditorFailure::Refused(text) => failed(text),
        }
    }

    /// Whether the buffer the ask found may be told of. A server's answer, and whether a line or a symbol is there, come
    /// from the buffer's text: an answer that names no buffer read none. What was judged before the ask is a path, so the
    /// buffer is judged here, by its own name and by what it was read from.
    fn may_tell(&self, value: &Value, cx: &ToolCx<'_>) -> bool {
        let Some(buffer) = field(value, "buffer") else {
            return true;
        };
        let path_in = |key: &str| {
            field(buffer, key)
                .and_then(Value::as_slice)
                .map(|bytes| PathBuf::from(OsStr::from_bytes(bytes)))
        };
        let read_from = path_in("read_from");
        path_in("name").is_some_and(|name| paths::buffer_told(cx, &name, read_from.as_deref(), self.allowed.as_deref()))
    }

    fn asked(&mut self, answer: Result<Value, EditorFailure>, load: bool, missing: bool, cx: &ToolCx<'_>) -> Progress {
        let value = match answer {
            Ok(value) => value,
            Err(failure) => return Self::editor_failed(failure, cx),
        };
        let asked = match parse_asked(&value) {
            Ok(asked) => asked,
            Err(why) => return failed(why),
        };
        let named = self.named();
        if !self.may_tell(&value, cx) {
            return failed(format!("the editor's buffer for {named} {}", words::NOT_TOLD));
        }
        match asked {
            Asked::Started {
                unsupported, action, ..
            } => {
                self.unsupported = unsupported;
                self.action = action;
                self.stage = Stage::Waiting;
                Progress::Step(Step::PollAfter(POLL))
            }
            stopped @ (Asked::ActionGone | Asked::NoEdit { .. }) => Progress::Stopped(stopped),
            Asked::NoClient { loaded_now } => self.no_client(loaded_now),
            Asked::NeedsLoad if missing => failed(format!("no such file: {named}")),
            Asked::NeedsLoad if load => failed(format!("the editor did not load {named}")),
            Asked::NeedsLoad if self.attach_since.is_some() => failed(format!(
                "{named} was closed in the editor while its language server attached; nothing was read"
            )),
            Asked::NeedsLoad => {
                let Some(path) = self.template.path.clone() else {
                    return failed(NOTHING_ASKED);
                };
                self.stage = Stage::LoadCard { path };
                Progress::Step(Step::Ask(CardRequest::new(
                    format!("Load {named} in your editor?"),
                    vec![CardLine::Plain(
                        "It is not open there; loading it can start language servers that run code from this repository"
                            .to_owned(),
                    )],
                )))
            }
            Asked::Swap => failed(format!("{named} {}", words::HAS_SWAP)),
            Asked::NoSymbol => failed(format!(
                "`{}` is not on line {} of {named}",
                self.template.symbol.as_deref().unwrap_or_default(),
                self.template.line.unwrap_or(1)
            )),
            Asked::NoLine { line_count } => failed(format!(
                "line {} is past the end of {named}, which has {line_count} {}",
                self.template.line.unwrap_or(1),
                if line_count == 1 { "line" } else { "lines" }
            )),
            Asked::Refused(why) => failed(why),
        }
    }

    fn collected(&mut self, answer: Result<Value, EditorFailure>, last: bool, cx: &ToolCx<'_>) -> Progress {
        let value = match answer {
            Ok(value) => value,
            // Held for now, and an editor held at a prompt lets go: what its server sent is still there.
            Err(EditorFailure::NotAnswered) if !last => return self.rest_before_a_collect(),
            Err(failure) => return Self::editor_failed(failure, cx),
        };
        match parse_collected(&value) {
            Err(why) => failed(why),
            Ok(Collected::Gone) => failed(RESTARTED),
            Ok(Collected::Pending) if last => failed(words::NOT_READ),
            Ok(Collected::Pending) => self.rest_before_a_collect(),
            Ok(mut done) => {
                if let Collected::Done { action, .. } = &mut done {
                    // The answer's facts come first: a resolved action may carry a command the listed one lacked.
                    *action = action.take().or(self.action.take());
                }
                Progress::Finished(done)
            }
        }
    }
}

/// What a part's item becomes in an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShownItem {
    pub path: String,
    pub line: u32,
    /// The server's `character` plus one, in the server's own units: a hint, since the call names a symbol.
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

/// Where a server's path leads when that is in the project. A path that is not absolute is a URI, outside as it
/// stands. A name with nothing on disk under the root is an unsaved new file, inside as `paths::buffer_inside` has it.
fn inside_the_project(root: &Path, home: Option<&Path>, path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    path.is_absolute()
        .then(|| paths::name_inside(root, home, path))
        .flatten()
}

fn some_chars(text: &str) -> String {
    text.chars().take(LINE_CHARS_MAX).collect()
}

/// The lines `wanted` (from 1) of `path`, as far as it can be read. `path` was judged with every link followed, so a
/// link anywhere on it now came after the judgement and may lead out of the project: no link is followed here. The
/// file comes from a language server, so it may be anything: a FIFO is never opened, and only a regular file of a
/// loadable size is read.
fn lines_of(path: &Path, wanted: &BTreeSet<u32>) -> HashMap<u32, String> {
    let Ok(bytes) = read_regular_nofollow(path, LINE_FILE_MAX, Owner::Anyone) else {
        return HashMap::new();
    };
    let last = wanted.last().copied().unwrap_or(0) as usize;
    bytes
        .split(|byte| *byte == b'\n')
        .take(last)
        .enumerate()
        .filter_map(|(index, line)| {
            let number = u32::try_from(index + 1).ok()?;
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            wanted
                .contains(&number)
                .then(|| (number, some_chars(&String::from_utf8_lossy(line))))
        })
        .collect()
}

/// `items` as an answer tells them. It reads files and walks paths, so it runs on a [`worker`]. Each path is judged:
/// an item inside the project keeps its line, and one outside is cut to its path and position, whatever a loaded
/// buffer or the server said of it. What comes from a buffer is told only when that buffer's own name is in the project
/// and what it was read from passes: a lookup by path can reach another file's buffer through a link, and a name says
/// nothing of the file a buffer's text came from. A server's name, kind and container come from that text too.
pub fn shown(root: &Path, home: Option<&Path>, items: Vec<RawItem>) -> Vec<ShownItem> {
    shown_with(root, home, items, true)
}

/// `shown` for an answer that tells no source line: the same path judgement, but no file is read for a line.
pub fn shown_places(root: &Path, home: Option<&Path>, items: Vec<RawItem>) -> Vec<ShownItem> {
    shown_with(root, home, items, false)
}

fn shown_with(root: &Path, home: Option<&Path>, items: Vec<RawItem>, read_lines: bool) -> Vec<ShownItem> {
    let mut judged: HashMap<String, Option<PathBuf>> = HashMap::new();
    for item in &items {
        judged
            .entry(item.path.clone())
            .or_insert_with(|| inside_the_project(root, home, &item.path));
    }
    let mut wanted: HashMap<&Path, BTreeSet<u32>> = HashMap::new();
    for item in items.iter().filter(|item| read_lines && item.text.is_none()) {
        if let Some(Some(file)) = judged.get(&item.path) {
            wanted.entry(file.as_path()).or_default().insert(item.line);
        }
    }
    // Only the wanted lines stay, so a list of large files is never in memory at once.
    let lines: HashMap<&Path, HashMap<u32, String>> = wanted
        .iter()
        .map(|(file, numbers)| (*file, lines_of(file, numbers)))
        .collect();
    items
        .iter()
        .map(|item| {
            let column = item.character.saturating_add(1);
            let Some(Some(file)) = judged.get(&item.path) else {
                return ShownItem {
                    path: item.path.clone(),
                    line: item.line,
                    column,
                    text: None,
                    name: None,
                    kind: None,
                    container: None,
                };
            };
            let told = item.text_from.as_deref().is_none_or(|from| {
                from.is_absolute()
                    && paths::name_inside(root, home, from).is_some()
                    && paths::record_passes(root, home, from, item.read_from.as_deref(), None)
            });
            let text = match (&item.text, &item.text_from) {
                (Some(text), Some(_)) if told => Some(some_chars(text)),
                (Some(_), _) => None,
                (None, _) => lines
                    .get(file.as_path())
                    .and_then(|numbers| numbers.get(&item.line))
                    .cloned(),
            };
            ShownItem {
                path: file.strip_prefix(root).unwrap_or(file).to_string_lossy().into_owned(),
                line: item.line,
                column,
                text,
                name: item.name.clone().filter(|_| told),
                kind: item.kind.clone().filter(|_| told),
                container: item.container.clone().filter(|_| told),
            }
        })
        .collect()
}

/// Runs `job` off the GTK thread. The receiver answers `Disconnected` if the job could not run or panicked.
pub fn worker<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> mpsc::Receiver<T> {
    let (sender, receiver) = mpsc::channel();
    let started = std::thread::Builder::new()
        .name("eitri-lsp-items".to_owned())
        .spawn(move || {
            let _ = sender.send(job());
        });
    // With no thread the sender has gone with the closure, which the receiver hears.
    drop(started);
    receiver
}

/// A [`worker`] making a tool's answer, as its run polls it. The run's polls hold the tab's flight, and every editor
/// call of the tab queues behind them, so the run stops waiting at [`TELLING_LIMIT`]. A thread stuck in the kernel
/// cannot be stopped; it ends when its call returns, and its answer goes nowhere.
pub struct Telling {
    answer: mpsc::Receiver<ToolResult>,
    since: Instant,
}

impl Telling {
    pub fn start(job: impl FnOnce() -> ToolResult + Send + 'static) -> Telling {
        Telling {
            answer: worker(job),
            since: Instant::now(),
        }
    }

    pub fn step(&self) -> Step {
        match self.answer.try_recv() {
            Ok(result) => Step::Done(result),
            Err(TryRecvError::Empty) if self.since.elapsed() >= TELLING_LIMIT => Step::Done(ToolResult {
                text: "the files in the language server's answer could not be read within 10 s".to_owned(),
                is_error: true,
            }),
            Err(TryRecvError::Empty) => Step::PollAfter(TELLING_POLL),
            Err(TryRecvError::Disconnected) => Step::Done(ToolResult {
                text: UNREADABLE.to_owned(),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use rmpv::Value;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::Map(entries.iter().map(|(k, v)| ((*k).into(), v.clone())).collect())
    }

    fn with_cx(f: impl FnOnce(&ToolCx<'_>)) {
        use crate::editor_tools::memory::Memory;
        use crate::editor_tools::step::{AgentCall, EditorSight};
        let memory = Memory::default();
        let cx = ToolCx {
            mode: agent::PermissionMode::Manual,
            trust: agent::setting_sources::ProjectTrust::Trusted,
            root: Path::new("/project"),
            home: None,
            reach: EditorReach::Own,
            sight: EditorSight::OnScreen,
            context: None,
            tab: 1,
            session: "session",
            call: "one",
            agent_call: AgentCall::Host,
            memory: &memory,
        };
        f(&cx);
    }

    #[test]
    fn a_phase_c_kind_never_goes_through_the_remote_way() {
        use crate::editor_tools::way::{EditorWay, RemoteWay};
        use crate::nvim_rpc::RpcError;
        let remote = RemoteWay::new(Path::new("/project"), Box::new(|_| panic!("no call crosses the link")));
        let stamp = Stamp {
            scope: "s".into(),
            seq: 1,
            deadline_ms: 1,
        };
        for kind in [
            LspKind::Rename,
            LspKind::CodeActions,
            LspKind::ActionEdit,
            LspKind::Format,
        ] {
            let pending = remote.call(
                "session",
                &EditorCall::LspAsk(AskArgs {
                    kind,
                    ..Default::default()
                }),
                &stamp,
            );
            assert!(matches!(pending.try_take(), Some(Err(RpcError::Encode(_)))));
        }
        let pending = remote.call("session", &EditorCall::LspTargets { paths: vec![] }, &stamp);
        assert!(matches!(pending.try_take(), Some(Err(RpcError::Encode(_)))));
    }

    #[test]
    fn parse_targets_refuses_a_non_list() {
        for value in [Value::Nil, map(&[]), true.into(), "no".into()] {
            assert!(parse_targets(&value).is_err());
        }
        assert!(parse_targets(&Value::Array(vec![map(&[("found", true.into())])])).is_err());
        assert!(parse_targets(&Value::Array(vec![map(&[("found", false.into())])])).is_ok());
    }

    #[test]
    fn a_target_that_does_not_say_how_it_ends_is_not_believed() {
        let target = |eol: Option<Value>| {
            let mut entries = vec![
                ("found", true.into()),
                ("modified", false.into()),
                ("buf", 1.into()),
                ("changedtick", 2.into()),
                ("sha256", "00".into()),
                ("fileformat", "unix".into()),
            ];
            entries.extend(eol.map(|eol| ("eol", eol)));
            Value::Array(vec![map(&entries)])
        };
        assert!(parse_targets(&target(None)).is_err());
        assert!(parse_targets(&target(Some("yes".into()))).is_err());
        assert!(!parse_targets(&target(Some(false.into()))).unwrap()[0].eol);
    }

    #[test]
    fn collected_edits_and_action_strings_decode_lossily() {
        let bad = || Value::Binary(b"value\xff".to_vec());
        let pos = map(&[("line", 0.into()), ("character", 1.into())]);
        let value = map(&[
            ("status", "done".into()),
            ("parts", Value::Array(vec![])),
            ("missing", Value::Array(vec![])),
            (
                "edit",
                map(&[
                    ("encoding", "utf-8".into()),
                    ("too_large", false.into()),
                    ("resource_ops", Value::Array(vec![bad()])),
                    (
                        "files",
                        Value::Array(vec![map(&[
                            ("path", bad()),
                            (
                                "edits",
                                Value::Array(vec![map(&[("start", pos.clone()), ("end", pos), ("new_text", bad())])]),
                            ),
                        ])]),
                    ),
                ]),
            ),
            (
                "actions",
                Value::Array(vec![map(&[
                    ("id", "id".into()),
                    ("title", bad()),
                    ("kind", bad()),
                    ("disabled", bad()),
                    ("has_edit", true.into()),
                    ("has_command", true.into()),
                ])]),
            ),
            ("action", map(&[("title", bad()), ("has_command", true.into())])),
            (
                "ticks",
                Value::Array(vec![map(&[("buf", 9.into()), ("tick", u64::MAX.into())])]),
            ),
            ("asked_ms", 1_800_000_000_000u64.into()),
        ]);
        match parse_collected(&value).unwrap() {
            Collected::Done {
                edit: Some(edit),
                actions,
                action: Some(action),
                ticks,
                asked_ms,
                ..
            } => {
                assert_eq!(edit.encoding, Encoding::Utf8);
                assert_eq!(edit.files[0].path, "value�");
                assert_eq!(edit.files[0].edits[0].new_text, "value�");
                assert_eq!(edit.resource_ops, ["value�"]);
                assert_eq!(actions[0].title, "value�");
                assert_eq!(actions[0].kind.as_deref(), Some("value�"));
                assert_eq!(actions[0].disabled.as_deref(), Some("value�"));
                assert!(actions[0].has_edit && actions[0].has_command);
                assert_eq!(action.title, "value�");
                assert!(action.has_command);
                assert_eq!(ticks.get(&9), Some(&u64::MAX));
                assert_eq!(asked_ms, Some(1_800_000_000_000));
            }
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn action_gone_and_no_edit_stop_the_machine() {
        with_cx(|cx| {
            for (value, expected) in [
                (map(&[("action_gone", true.into())]), Asked::ActionGone),
                (
                    map(&[("no_edit", true.into()), ("has_command", true.into())]),
                    Asked::NoEdit { has_command: true },
                ),
            ] {
                let mut run = AskCollect::new(
                    AskArgs {
                        kind: LspKind::ActionEdit,
                        ..Default::default()
                    },
                    None,
                    Budget::new(LSP_BUDGET),
                );
                run.stage = Stage::Asking {
                    load: false,
                    missing: false,
                };
                assert!(
                    matches!(run.step_with_stops(StepEvent::Answer(Ok(value)), cx), Progress::Stopped(asked) if asked == expected)
                );
                assert!(matches!(run.step_with_stops(StepEvent::Poll, cx), Progress::Failed(_)));
            }
        });
    }

    #[test]
    fn read_tools_keep_three_outcomes_and_refuse_edit_only_answers() {
        with_cx(|cx| {
            for value in [
                map(&[("action_gone", true.into())]),
                map(&[("no_edit", true.into()), ("has_command", true.into())]),
            ] {
                let mut run = AskCollect::new(AskArgs::default(), None, Budget::new(LSP_BUDGET));
                run.stage = Stage::Asking {
                    load: false,
                    missing: false,
                };
                match run.step(StepEvent::Answer(Ok(value)), cx) {
                    Progress::Failed(result) => {
                        assert!(result.is_error);
                        assert_eq!(result.text, UNREADABLE);
                    }
                    Progress::Step(_) | Progress::Finished(_) => panic!("an edit-only answer must fail"),
                }
                assert!(matches!(run.step(StepEvent::Poll, cx), Progress::Failed(_)));
            }
        });
    }

    #[test]
    fn the_action_facts_reach_done() {
        with_cx(|cx| {
            let mut run = AskCollect::new(AskArgs::default(), None, Budget::new(LSP_BUDGET));
            run.stage = Stage::Asking {
                load: false,
                missing: false,
            };
            let facts = map(&[("title", "Fix".into()), ("has_command", true.into())]);
            let started = map(&[
                ("started", true.into()),
                ("unsupported", Value::Array(vec![])),
                ("action", facts),
            ]);
            assert!(matches!(
                run.step(StepEvent::Answer(Ok(started)), cx),
                Progress::Step(Step::PollAfter(_))
            ));
            assert!(matches!(
                run.step(StepEvent::Poll, cx),
                Progress::Step(Step::Call { .. })
            ));
            let done = map(&[
                ("status", "done".into()),
                ("parts", Value::Array(vec![])),
                ("missing", Value::Array(vec![])),
            ]);
            assert!(
                matches!(run.step(StepEvent::Answer(Ok(done)), cx), Progress::Finished(Collected::Done { action: Some(facts), .. }) if facts.title == "Fix" && facts.has_command)
            );
        });
    }

    #[test]
    fn refuse_outside_raises_no_card() {
        with_cx(|cx| {
            for mode in [
                agent::PermissionMode::Manual,
                agent::PermissionMode::Auto,
                agent::PermissionMode::Bypass,
            ] {
                let cx = ToolCx { mode, ..*cx };
                let mut run = AskCollect::new(AskArgs::default(), Some("/outside.rs".into()), Budget::new(LSP_BUDGET))
                    .refuse_outside();
                assert!(
                    matches!(run.step(StepEvent::Start, &cx), Progress::Failed(result) if result.text == "/outside.rs is outside the project; nothing can be proposed there")
                );
            }
        });
    }

    #[test]
    fn without_it_outside_is_still_a_card() {
        with_cx(|cx| {
            let mut run = AskCollect::new(AskArgs::default(), Some("/outside.rs".into()), Budget::new(LSP_BUDGET));
            assert!(matches!(run.step(StepEvent::Start, cx), Progress::Step(Step::Ask(_))));
        });
    }

    #[test]
    fn parse_asked_reads_every_form() {
        for (value, expected) in [
            (
                map(&[
                    ("started", true.into()),
                    ("unsupported", Value::Array(vec!["references".into()])),
                ]),
                Asked::Started {
                    unsupported: vec!["references".into()],
                    action: None,
                    clients: Vec::new(),
                    busy: Vec::new(),
                },
            ),
            (
                map(&[
                    ("started", true.into()),
                    ("unsupported", Value::Array(vec![])),
                    ("clients", Value::Array(vec!["rust-analyzer".into(), "lua_ls".into()])),
                    ("busy", Value::Array(vec!["rust-analyzer".into()])),
                ]),
                Asked::Started {
                    unsupported: Vec::new(),
                    action: None,
                    clients: vec!["rust-analyzer".into(), "lua_ls".into()],
                    busy: vec!["rust-analyzer".into()],
                },
            ),
            (
                map(&[
                    ("started", true.into()),
                    ("clients", Value::Map(vec![])),
                    ("busy", Value::Map(vec![])),
                ]),
                Asked::Started {
                    unsupported: Vec::new(),
                    action: None,
                    clients: Vec::new(),
                    busy: Vec::new(),
                },
            ),
            (
                map(&[("no_client", true.into()), ("loaded_now", true.into())]),
                Asked::NoClient { loaded_now: true },
            ),
            (map(&[("needs_load", true.into())]), Asked::NeedsLoad),
            (map(&[("swap", true.into())]), Asked::Swap),
            (map(&[("no_symbol", true.into())]), Asked::NoSymbol),
            (
                map(&[("no_line", true.into()), ("line_count", 7.into())]),
                Asked::NoLine { line_count: 7 },
            ),
            (
                map(&[("refused", Value::Binary(b"bad\xff".to_vec()))]),
                Asked::Refused("bad�".into()),
            ),
        ] {
            assert_eq!(parse_asked(&value).unwrap(), expected);
        }
        assert!(parse_asked(&map(&[])).is_err());
        assert!(
            parse_asked(&map(&[("started", true.into()), ("clients", "gopls".into())])).is_err(),
            "a client list that is not a list is not believed"
        );
    }

    /// An editor that keeps what each chunk was sent.
    #[derive(Default)]
    struct Kept(std::cell::RefCell<Vec<Vec<Value>>>);
    impl EditorRpc for Kept {
        fn exec_lua(&self, _code: &'static str, args: Vec<Value>) -> Pending {
            self.0.borrow_mut().push(args);
            Pending::failed(crate::nvim_rpc::RpcError::Closed)
        }
        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    fn sent_args(args: &AskArgs) -> Value {
        let kept = Kept::default();
        let stamp = Stamp {
            scope: "s".into(),
            seq: 1,
            deadline_ms: u64::MAX,
        };
        drop(ask(&kept, &stamp, args));
        let sent = kept.0.borrow();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].len(),
            2,
            "the stamp and one map: nothing takes a third argument's place"
        );
        sent[0][1].clone()
    }

    #[test]
    fn ask_sends_a_query_per_client_only_when_given() {
        let plain = AskArgs {
            id: "a".into(),
            kind: LspKind::WorkspaceSymbols,
            query: Some("RegisterPreview".into()),
            ..Default::default()
        };
        assert!(
            field(&sent_args(&plain), "query_for").is_none(),
            "absent, the tools ask as before"
        );
        let worded = AskArgs {
            query_for: vec![
                ("gopls".into(), "Server.RegisterPreview".into()),
                ("rust-analyzer".into(), "RegisterPreview#".into()),
            ],
            ..plain
        };
        let value = sent_args(&worded);
        assert_eq!(field(&value, "query").and_then(Value::as_str), Some("RegisterPreview"));
        let per = field(&value, "query_for").and_then(Value::as_map).expect("a map");
        let pairs: Vec<(&str, &str)> = per
            .iter()
            .map(|(k, v)| (k.as_str().unwrap(), v.as_str().unwrap()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("gopls", "Server.RegisterPreview"),
                ("rust-analyzer", "RegisterPreview#")
            ]
        );
    }

    #[test]
    fn parse_collected_refuses_a_missing_status() {
        assert!(parse_collected(&map(&[])).is_err());
        assert!(parse_collected(&map(&[("status", "unknown".into())])).is_err());
        assert!(matches!(
            parse_collected(&map(&[("status", "pending".into())])).unwrap(),
            Collected::Pending
        ));
        assert!(matches!(
            parse_collected(&map(&[("status", "gone".into())])).unwrap(),
            Collected::Gone
        ));
        let raw = map(&[
            ("status", "done".into()),
            (
                "parts",
                Value::Array(vec![map(&[
                    ("name", "definitions".into()),
                    ("total", 1.into()),
                    (
                        "items",
                        Value::Array(vec![map(&[
                            ("path", Value::Binary(b"/p/\xff".to_vec())),
                            ("line", 1.into()),
                            ("character", 2.into()),
                            ("text", Value::Binary(b"line\xff".to_vec())),
                            ("text_from", Value::Binary(b"/p/\xfe".to_vec())),
                            ("name", Value::Binary(b"name\xff".to_vec())),
                            ("kind", Value::Binary(b"kind\xff".to_vec())),
                            ("container", Value::Binary(b"container\xff".to_vec())),
                        ])]),
                    ),
                ])]),
            ),
            ("hover", Value::Binary(b"hover\xff".to_vec())),
            ("cut", true.into()),
            ("missing", Value::Array(vec![Value::Binary(b"part\xff".to_vec())])),
        ]);
        match parse_collected(&raw).unwrap() {
            Collected::Done {
                parts,
                hover,
                cut,
                missing,
                ..
            } => {
                assert!(cut);
                use std::os::unix::ffi::OsStrExt;
                let item = &parts[0].items[0];
                assert_eq!(item.path, "/p/�");
                assert_eq!(item.text.as_deref(), Some("line�"));
                assert_eq!(item.text_from.as_ref().unwrap().as_os_str().as_bytes(), b"/p/\xfe");
                assert_eq!(item.name.as_deref(), Some("name�"));
                assert_eq!(item.kind.as_deref(), Some("kind�"));
                assert_eq!(item.container.as_deref(), Some("container�"));
                assert_eq!(hover.as_deref(), Some("hover�"));
                assert_eq!(missing, ["part�"]);
            }
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn the_attach_wait_counts_from_the_first_answer() {
        let mut run = AskCollect::new(AskArgs::default(), None, Budget::new(LSP_BUDGET));
        assert!(matches!(
            run.no_client(true),
            Progress::Step(Step::PollAfter(wait)) if wait == ATTACH_POLL
        ));
        assert!(matches!(run.no_client(false), Progress::Step(Step::PollAfter(_))));
        run.attach_since = Some(monotonic_now() - ATTACH_WAIT);
        match run.no_client(false) {
            Progress::Failed(result) => {
                assert_eq!(result.text, "no language server attached to the project within 5 s");
                assert!(result.is_error);
            }
            other => panic!("{other:?}"),
        }
    }

    fn the_budget_of(progress: Progress) -> Duration {
        match progress {
            Progress::Step(Step::Call { budget, .. }) => budget,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_collect_before_the_last_ends_with_the_budget() {
        let _clock = crate::editor_tools::testing::clock::Clock::new();
        let mut run = AskCollect::new(AskArgs::default(), None, Budget::new(Duration::from_secs(2)));
        let held_to = the_budget_of(run.collect(false));
        assert_eq!(held_to, Duration::from_secs(2));
        assert_eq!(the_budget_of(run.collect(true)), COLLECT_MARGIN);
        let mut early = AskCollect::new(AskArgs::default(), None, Budget::new(LSP_BUDGET));
        assert_eq!(the_budget_of(early.collect(false)), EDITOR_BUDGET);
    }

    #[test]
    fn a_path_swapped_for_a_link_after_its_judgement_is_not_read() {
        let scratch = ScratchDir::new("editor-tools-lsp", "swapped");
        let base = fs::canonicalize(&*scratch).unwrap();
        let (root, beside) = (base.join("project"), base.join("beside"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&beside).unwrap();
        fs::write(beside.join("a.rs"), "outside line\n").unwrap();
        let file = root.join("src/a.rs");
        let line_one = BTreeSet::from([1]);

        fs::write(&file, "inside line\n").unwrap();
        assert_eq!(lines_of(&file, &line_one)[&1], "inside line");

        fs::remove_file(&file).unwrap();
        symlink(beside.join("a.rs"), &file).unwrap();
        assert!(lines_of(&file, &line_one).is_empty(), "the file itself became a link");

        fs::remove_dir_all(root.join("src")).unwrap();
        symlink(&beside, root.join("src")).unwrap();
        assert!(
            lines_of(&file, &line_one).is_empty(),
            "a directory on the way became a link"
        );
    }

    #[test]
    fn a_worker_stuck_on_a_file_is_given_up() {
        let (sender, answer) = mpsc::channel();
        let telling = Telling {
            answer,
            since: monotonic_now(),
        };
        assert!(matches!(telling.step(), Step::PollAfter(wait) if wait == TELLING_POLL));
        let telling = Telling {
            since: telling.since - TELLING_LIMIT,
            ..telling
        };
        match telling.step() {
            Step::Done(result) => {
                assert!(result.is_error);
                assert!(result.text.contains("within 10 s"), "{}", result.text);
            }
            other => panic!("{other:?}"),
        }
        let done = ToolResult {
            text: "the answer".to_owned(),
            is_error: false,
        };
        sender.send(done.clone()).unwrap();
        assert!(
            matches!(telling.step(), Step::Done(result) if result == done),
            "an answer that came is told, however late"
        );
        drop(sender);
        assert!(matches!(telling.step(), Step::Done(result) if result.text == UNREADABLE));
    }

    #[test]
    fn no_chunk_waits() {
        for chunk in [LSP_ASK_LUA, LSP_COLLECT_LUA, LSP_TARGETS_LUA] {
            assert!(chunk.starts_with(include_str!("../prelude.lua")));
            mlua::Lua::new()
                .load(chunk)
                .into_function()
                .expect("constant Lua source");
            for forbidden in ["buf_request_sync", "request_sync", "vim.wait"] {
                assert!(!chunk.contains(forbidden), "{forbidden}");
            }
        }
    }

    #[test]
    fn shown_places_reads_no_line_that_shown_does() {
        let scratch = ScratchDir::new("editor-tools-lsp", "places");
        let root = std::fs::canonicalize(&*scratch).unwrap();
        std::fs::write(root.join("a.py"), "one\n").unwrap();
        let item = || RawItem {
            path: root.join("a.py").to_string_lossy().into_owned(),
            line: 1,
            character: 0,
            encoding: Encoding::Utf16,
            text: None,
            text_from: None,
            read_from: None,
            name: Some("n".to_owned()),
            kind: None,
            container: None,
        };
        assert_eq!(shown(&root, None, vec![item()])[0].text.as_deref(), Some("one"));
        let places = shown_places(&root, None, vec![item()]);
        assert_eq!(places[0].text, None);
        assert_eq!(places[0].path, "a.py");
        assert_eq!(places[0].name.as_deref(), Some("n"));
    }
}
