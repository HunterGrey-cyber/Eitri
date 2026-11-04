//! The exec module: the one chunk that evaluates an argument, and what a person's answer has to be before it may.
//!
//! The constant stays private to this file and `send` is its one call. The text it runs comes only from an
//! [`ApprovalRecord`], which only `from_card` makes, from the card's own [`CardAnswer`]. No test seam makes a record:
//! this file's tests get theirs through a driven card, and nothing outside the file can.

use std::cell::RefCell;
use std::fmt;
use std::time::Duration;

use rmpv::Value;
use sha2::{Digest, Sha256};

use crate::editor_rpc::EditorRpc;
use crate::editor_tools::cards::{CardAnswer, CardLine, CardOrigin, CardRequest};
use crate::editor_tools::catalog::ToolRow;
use crate::editor_tools::gate::{verdict, Need, Verdict};
use crate::editor_tools::stamp::{wall_ms, Stamp, STAMP_MARGIN};
use crate::editor_tools::step::{
    AgentCall, EditorFailure, EditorReach, Step, StepEvent, Tool, ToolCx, ToolError, ToolResult, ToolRun,
};
use crate::editor_tools::way::EditorCall;
use crate::editor_tools::words;
use crate::nvim_rpc::{Pending, RpcError};

pub const ROW: ToolRow = ToolRow {
    name: "nvim_exec",
    description: "Run Ex commands or Lua in the person's editor, with their full rights and no sandbox. Every call \
                  shows the person the code on a card, in every permission mode, and runs only on their answer. Use \
                  it when no other editor tool can do the job.",
    schema: r#"{
  "type": "object",
  "properties": {
    "kind": { "type": "string", "enum": ["ex", "lua"], "description": "ex: Ex commands. lua: a Lua chunk." },
    "code": { "type": "string", "description": "The text to run, at most 16 KiB. The person reads it whole." },
    "why": { "type": "string", "description": "Why this needs to run, in a sentence for the person." }
  },
  "required": ["kind", "code", "why"],
  "additionalProperties": false
}"#,
    always_load: false,
    default_on: false,
    in_untrusted: false,
};

const AGENT_EXEC_LUA: &str = chunk!("nvim_exec.lua");

/// Whether `code` is the approved-code chunk. The text itself never leaves this file: a caller learns only that a
/// string is it, so no other module can name the chunk and send it without a person's answer.
pub fn is_exec_chunk(code: &str) -> bool {
    code == AGENT_EXEC_LUA
}

/// The longest `code` a call may hold, in bytes.
pub const CODE_LIMIT: usize = 16 * 1024;
/// The longest `why` a call may hold, in bytes.
pub const WHY_LIMIT: usize = 4 * 1024;
/// The most an answer brings back, in bytes; `nvim_exec.lua` holds the same number.
pub const RESULT_LIMIT: usize = 64 * 1024;
/// How long after the person's answer the code may still start.
pub const APPROVAL_WINDOW: Duration = Duration::from_secs(5);

const CARD_TITLE: &str = "Run this in your editor?";

/// What every card says, whatever the code.
const WARNINGS: [&str; 4] = [
    "Runs with your user's full rights inside your editor. Nothing sandboxes it.",
    "The agent may have been steered by text it read: files, web pages, diagnostics, tool output.",
    "Changes it makes to buffers are not in turn review until saved; files it writes appear in turn review without a \
     call that explains them.",
    "Code that never ends freezes your editor; Eitri cannot stop it. If your editor is busy for more than 5 s after \
     you approve, it does not run.",
];

/// The card's last line, so a card with no hint is not read as a card that found nothing.
const HINTS_ARE_A_SEARCH: &str = "These hints are a text search, not an analysis: no hint does not mean safe.";

/// Past the window, or a chunk that did not answer: nothing may run any more, or what runs cannot be stopped.
const NOT_ANSWERED: &str = "the editor did not answer within 5 s: if it was busy, the code will not run; if the code \
                            itself is still running, it keeps running";

const CUT_MARKER: &str = "\n[cut at 64 KiB]";

/// The conditional lines of a card, each with what makes it show.
const HINTS: &[(&[&str], &str)] = &[
    (
        &[
            ":!",
            "system",
            "systemlist",
            "jobstart",
            "termopen",
            "vim.system",
            "vim.fn.system",
            "io.popen",
            "os.execute",
            "uv.spawn",
        ],
        "It can run shell commands.",
    ),
    (
        &[
            ":w",
            ":wa",
            ":sav",
            ":up",
            "writefile",
            "io.open",
            "os.remove",
            "os.rename",
            "fs_write",
            "fs_unlink",
            "delete(",
            "bufdo",
            "windo",
        ],
        "It can write or delete files outside Eitri's edit cards.",
    ),
    (
        &[
            "nvim_feedkeys",
            "nvim_input",
            ":normal",
            "nvim_buf_set_lines",
            "nvim_buf_set_text",
        ],
        "It can type keys or change your buffers.",
    ),
    (
        &["require", ":source", ":runtime", "loadfile", "dofile"],
        "It can load other code.",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecKind {
    Ex,
    Lua,
}

impl ExecKind {
    /// What the chunk and the digest call this kind.
    fn word(self) -> &'static str {
        match self {
            ExecKind::Ex => "ex",
            ExecKind::Lua => "lua",
        }
    }
}

/// SHA-256 over `eitri-exec-v1\0<kind>\0<code>`: `kind` cannot hold a NUL and `code` is last, so two pairs never
/// share their bytes.
pub fn digest(kind: ExecKind, code: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"eitri-exec-v1\0");
    hash.update(kind.word().as_bytes());
    hash.update(b"\0");
    hash.update(code.as_bytes());
    hash.finalize().into()
}

/// The conditional lines of the card, in their order, each once. A search for substrings of `code` and nothing
/// else: a needle in a comment counts, code that names none says nothing, and no line allows anything.
pub fn hints(code: &str) -> Vec<&'static str> {
    HINTS
        .iter()
        .filter(|(needles, _)| needles.iter().any(|needle| code.contains(needle)))
        .map(|(_, line)| *line)
        .collect()
}

/// What the card shows. Made when the call starts, consumed by the approval. The card's body and its binding come
/// from these accessors, so what is shown and what is bound are one text.
pub struct PendingExec {
    tab: u64,
    call: String,
    kind: ExecKind,
    code: String,
}

impl PendingExec {
    pub fn new(tab: u64, call: &str, kind: ExecKind, code: String) -> PendingExec {
        PendingExec {
            tab,
            call: call.to_owned(),
            kind,
            code,
        }
    }

    pub fn kind(&self) -> ExecKind {
        self.kind
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn digest(&self) -> [u8; 32] {
        digest(self.kind, &self.code)
    }
}

/// A person's approval of one call's exact text. Not `Clone`, no public field, and `from_card` is its one
/// constructor, private to this file.
pub struct ApprovalRecord {
    tab: u64,
    call: String,
    kind: ExecKind,
    code: String,
    digest: [u8; 32],
    origin: CardOrigin,
    answered_ms: u64,
}

impl ApprovalRecord {
    /// The one way to a record. The card was raised with `pending`'s digest as its `binding`, kept by the broker beside
    /// the parked card, so the comparison ties what was shown to what runs through a value this run did not hold.
    /// `answer` is taken by value: only the broker's `answer_card` makes one, for one person's key or click.
    fn from_card(
        pending: PendingExec,
        answer: CardAnswer,
        tab: u64,
        call: &str,
    ) -> Result<ApprovalRecord, ExecRefused> {
        if !answer.allowed() {
            return Err(ExecRefused::NotAllowed);
        }
        // No wildcard: a new way of answering stops the build here until someone says whether it is a person's.
        let origin = match answer.origin() {
            origin @ (CardOrigin::Key | CardOrigin::Click) => origin,
        };
        if answer.call_id() != pending.call || pending.call != call {
            return Err(ExecRefused::OtherCall);
        }
        if pending.tab != tab {
            return Err(ExecRefused::OtherTab);
        }
        let digest = pending.digest();
        if answer.binding() != Some(&digest) {
            return Err(ExecRefused::OtherText);
        }
        Ok(ApprovalRecord {
            tab,
            call: pending.call,
            kind: pending.kind,
            code: pending.code,
            digest,
            origin,
            answered_ms: answer.answered_ms(),
        })
    }

    /// The tab the card stood in.
    pub fn tab(&self) -> u64 {
        self.tab
    }

    /// The broker's call the card was raised for.
    pub fn call(&self) -> &str {
        &self.call
    }

    /// The digest of the text the card showed.
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// How the person answered.
    pub fn origin(&self) -> CardOrigin {
        self.origin
    }

    /// What is left of [`APPROVAL_WINDOW`] from the person's answer; `None` when that is at or under `STAMP_MARGIN`:
    /// the broker would send nothing. A clock set back counts as no time gone, never as more time.
    pub fn budget(&self, now_ms: u64) -> Option<Duration> {
        let gone = Duration::from_millis(now_ms.saturating_sub(self.answered_ms));
        let left = APPROVAL_WINDOW.saturating_sub(gone);
        (left > STAMP_MARGIN).then_some(left)
    }
}

/// Why an answer made no record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecRefused {
    NotAllowed,
    OtherCall,
    OtherTab,
    OtherText,
}

impl ExecRefused {
    fn sentence(self) -> &'static str {
        match self {
            ExecRefused::NotAllowed => words::NOT_ALLOWED,
            ExecRefused::OtherCall => "the answer was for another call; nothing ran",
            ExecRefused::OtherTab => "the answer came from another tab; nothing ran",
            ExecRefused::OtherText => "the card did not show this text; nothing ran",
        }
    }
}

/// One run's hold on the record: taken once, by [`send`].
pub struct ExecSlot(RefCell<Option<ApprovalRecord>>);

impl ExecSlot {
    pub fn new(record: ApprovalRecord) -> ExecSlot {
        ExecSlot(RefCell::new(Some(record)))
    }
}

/// By hand and naming only the type: `EditorCall` derives `Debug`, and approved code stays out of a log line.
impl fmt::Debug for ExecSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExecSlot")
    }
}

/// Runs the slot's record, once: the chunk's only call, and its arguments come from nowhere else.
pub fn send(rpc: &dyn EditorRpc, stamp: &Stamp, slot: &ExecSlot) -> Pending {
    let Some(record) = slot.0.borrow_mut().take() else {
        return Pending::failed(RpcError::Encode("this approval was spent".to_owned()));
    };
    rpc.exec_lua(AGENT_EXEC_LUA, exec_args(stamp, record))
}

fn exec_args(stamp: &Stamp, record: ApprovalRecord) -> Vec<Value> {
    vec![
        stamp.to_value(),
        Value::Map(vec![
            (Value::from("kind"), Value::from(record.kind.word())),
            (Value::from("code"), Value::from(record.code)),
        ]),
    ]
}

/// What the chunk said of a text it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecAnswer {
    pub text: String,
    pub failed: bool,
    pub cut: bool,
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// Reads the chunk's answer by its bytes, never through `as_str()`, which has nothing for bytes that are not UTF-8.
/// The code has run by now and its output need not be text, so a failure here would tell the agent of an error in
/// code that did run.
pub fn parse(value: &Value) -> Result<ExecAnswer, String> {
    let (bytes, failed) = match (field(value, "failed"), field(value, "output")) {
        (Some(failed), _) => (failed.as_slice(), true),
        (None, Some(output)) => (output.as_slice(), false),
        (None, None) => (None, false),
    };
    let bytes = bytes.ok_or_else(|| "the editor's answer could not be read".to_owned())?;
    Ok(ExecAnswer {
        text: String::from_utf8_lossy(bytes).into_owned(),
        failed,
        cut: field(value, "cut").and_then(Value::as_bool).unwrap_or(false),
    })
}

pub struct NvimExec;

impl Tool for NvimExec {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    /// Refuses in this order, and only a call that passes everything is recorded as seen, so a call turned away for
    /// its arguments can be made right under the same id.
    fn begin(&self, args: &serde_json::Value, cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        if cx.reach == EditorReach::Link {
            return Err(ToolError(words::MOVE_TO_EDITOR.to_owned()));
        }
        if cx.trust.refused() {
            return Err(ToolError("not offered in a session started with n".to_owned()));
        }
        let key = call_key(cx.agent_call)?;
        let kind = match required_text(args, "kind")? {
            "ex" => ExecKind::Ex,
            "lua" => ExecKind::Lua,
            _ => return Err(ToolError("kind must be ex or lua".to_owned())),
        };
        let code = required_text(args, "code")?;
        if code.is_empty() {
            return Err(ToolError("code may not be empty".to_owned()));
        }
        if code.len() > CODE_LIMIT {
            return Err(ToolError("code is over 16 KiB".to_owned()));
        }
        let why = required_text(args, "why")?;
        if why.len() > WHY_LIMIT {
            return Err(ToolError("why is over 4 KiB".to_owned()));
        }
        if !cx.memory.with(cx.tab, |tab| tab.first_exec_of(key)) {
            return Err(ToolError("this call was already handled".to_owned()));
        }
        Ok(Box::new(ExecRun {
            stage: Stage::Asking {
                kind,
                code: code.to_owned(),
                why: why.to_owned(),
            },
            clock: wall_ms,
        }))
    }
}

/// A required argument, read by type: nothing upstream checks the declared schema.
fn required_text<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolError> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Err(ToolError(format!("{key} is required"))),
        Some(value) => value
            .as_str()
            .ok_or_else(|| ToolError(format!("{key} must be a string"))),
    }
}

/// What ties a call to the agent's own: its model's id, or on a bridge with none, the bridge's request. A digest, so a
/// tab's memory stays a fixed size however long the ids an agent sends, and a word apart, so an id cannot pass for a
/// request.
fn call_key(call: AgentCall<'_>) -> Result<[u8; 32], ToolError> {
    let key = match call {
        AgentCall::Claude(Some(id)) | AgentCall::Bridge { call_id: Some(id), .. } => format!("id\0{id}"),
        AgentCall::Bridge { call_id: None, request } => format!("request\0{}\0{}", request.connection, request.call),
        AgentCall::Claude(None) => {
            return Err(ToolError(
                "this call has no tool_use_id, so it cannot be tied to an approval".to_owned(),
            ))
        }
        AgentCall::Host => {
            return Err(ToolError(
                "a run the host started has no call to tie to an approval".to_owned(),
            ))
        }
    };
    Ok(Sha256::digest(key.as_bytes()).into())
}

enum Stage {
    Asking { kind: ExecKind, code: String, why: String },
    OnCard(PendingExec),
    Called,
    Over,
}

struct ExecRun {
    stage: Stage,
    /// The wall clock the person's answer is read against.
    clock: fn() -> u64,
}

fn failure(text: impl Into<String>) -> Step {
    Step::Done(ToolResult {
        text: text.into(),
        is_error: true,
    })
}

fn card_body(pending: &PendingExec, why: String) -> Vec<CardLine> {
    let named = match pending.kind() {
        ExecKind::Lua => "Lua",
        ExecKind::Ex => "Ex commands",
    };
    let mut body = vec![
        CardLine::Plain(named.to_owned()),
        CardLine::Code(pending.code().to_owned()),
        CardLine::AgentWords(why),
    ];
    let warnings = WARNINGS.iter().copied();
    let found = hints(pending.code());
    body.extend(
        warnings
            .chain(found)
            .chain([HINTS_ARE_A_SEARCH])
            .map(|line| CardLine::Warning(line.to_owned())),
    );
    body
}

/// What the agent is told of the chunk. Nothing here sends it again: a retry is a new call and a new card.
fn told(reply: Result<Value, EditorFailure>) -> Step {
    match reply {
        Ok(value) => match parse(&value) {
            Ok(ExecAnswer { mut text, failed, cut }) => {
                if cut {
                    text.push_str(CUT_MARKER);
                }
                Step::Done(ToolResult { text, is_error: failed })
            }
            Err(why) => failure(why),
        },
        Err(EditorFailure::NotAnswered) => failure(NOT_ANSWERED),
        Err(EditorFailure::Unavailable(why) | EditorFailure::Refused(why)) => failure(why),
    }
}

impl ExecRun {
    fn ask(&mut self, kind: ExecKind, code: String, why: String, cx: &ToolCx<'_>) -> Step {
        match verdict(Need::Exec, cx.mode, cx.trust) {
            Verdict::Card => {}
            Verdict::Allow => return failure("no card was asked for, and this tool runs nothing without one"),
        }
        let pending = PendingExec::new(cx.tab, cx.call, kind, code);
        let request = CardRequest {
            binding: Some(pending.digest()),
            ..CardRequest::new(CARD_TITLE, card_body(&pending, why))
        };
        self.stage = Stage::OnCard(pending);
        Step::Ask(request)
    }

    fn run_approved(&mut self, pending: PendingExec, answer: CardAnswer, cx: &ToolCx<'_>) -> Step {
        let record = match ApprovalRecord::from_card(pending, answer, cx.tab, cx.call) {
            Ok(record) => record,
            Err(refused) => return failure(refused.sentence()),
        };
        let Some(budget) = record.budget((self.clock)()) else {
            return failure(NOT_ANSWERED);
        };
        self.stage = Stage::Called;
        Step::Call {
            call: EditorCall::Exec(ExecSlot::new(record)),
            budget,
        }
    }
}

impl ToolRun for ExecRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match (std::mem::replace(&mut self.stage, Stage::Over), event) {
            (Stage::Asking { kind, code, why }, StepEvent::Start) => self.ask(kind, code, why, cx),
            (Stage::OnCard(pending), StepEvent::Card(answer)) => self.run_approved(pending, answer, cx),
            (Stage::Called, StepEvent::Answer(reply)) => told(reply),
            _ => failure("the call was out of step; nothing more runs"),
        }
    }
}

/// An approval cannot be copied, so one answer can run its text once.
///
/// ```compile_fail
/// use eitri_core::editor_tools::tools::nvim_exec::ApprovalRecord;
/// fn needs_clone<T: Clone>() {}
/// needs_clone::<ApprovalRecord>();
/// ```
#[cfg(doctest)]
fn a_record_is_not_clone() {}

/// A record cannot be written out, so holding one means a person's answer made it. The paths are the ones
/// [`a_record_is_read_through_these_paths`] proves good, so this fails for the private fields and nothing else.
///
/// ```compile_fail
/// use eitri_core::editor_tools::cards::CardOrigin;
/// use eitri_core::editor_tools::tools::nvim_exec::{ApprovalRecord, ExecKind};
/// let _forged = ApprovalRecord {
///     tab: 1,
///     call: String::new(),
///     kind: ExecKind::Lua,
///     code: String::new(),
///     digest: [0; 32],
///     origin: CardOrigin::Key,
///     answered_ms: 0,
/// };
/// ```
#[cfg(doctest)]
fn no_record_without_a_card_answer() {}

/// ```
/// use eitri_core::editor_tools::cards::CardOrigin;
/// use eitri_core::editor_tools::tools::nvim_exec::{ApprovalRecord, ExecKind};
/// fn read(record: &ApprovalRecord) -> CardOrigin {
///     record.origin()
/// }
/// let _ = (read, ExecKind::Lua);
/// ```
#[cfg(doctest)]
fn a_record_is_read_through_these_paths() {}

#[cfg(test)]
#[path = "../../../tests/support/embed_client.rs"]
mod embed_client;

#[cfg(test)]
mod tests {
    use agent::setting_sources::ProjectTrust;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::Instant;

    use agent::PermissionMode;
    use sha2::{Digest, Sha256};

    use super::embed_client::Embed;
    use super::*;
    use crate::editor_context::EditorContext;
    use crate::editor_tools::broker::{Broker, BrokerEnv, BrokerOut, Caller, TabFacts};
    use crate::editor_tools::cards::{CardDecision, CardRequest};
    use crate::editor_tools::catalog::{client_tool_set, declared};
    use crate::editor_tools::memory::Memory;
    use crate::editor_tools::step::{BridgeCallId, EditorSight};
    use crate::editor_tools::switches::Switches;
    use crate::editor_tools::testing::drive_with_cards;
    use crate::editor_tools::way::{EditorWay, LocalWay, RemoteWay};
    use crate::tabs::TabId;

    const TAB: u64 = 7;
    const CALL: &str = "call-1";

    const SHELL: &str = "It can run shell commands.";
    const FILES: &str = "It can write or delete files outside Eitri's edit cards.";
    const KEYS: &str = "It can type keys or change your buffers.";
    const LOADS: &str = "It can load other code.";

    /// Raises one card and keeps its answer: the only way a test here comes by a [`CardAnswer`].
    struct Asks {
        request: Option<CardRequest>,
        got: Rc<RefCell<Option<CardAnswer>>>,
    }

    impl ToolRun for Asks {
        fn step(&mut self, event: StepEvent, _cx: &ToolCx<'_>) -> Step {
            match event {
                StepEvent::Start => Step::Ask(self.request.take().expect("one card")),
                StepEvent::Card(answer) => {
                    *self.got.borrow_mut() = Some(answer);
                    Step::Done(ToolResult {
                        text: String::new(),
                        is_error: false,
                    })
                }
                other => panic!("a card's run heard {other:?}"),
            }
        }
    }

    fn answer_to(request: CardRequest, decision: CardDecision, origin: CardOrigin, call: &str) -> CardAnswer {
        let got = Rc::new(RefCell::new(None));
        let run = Asks {
            request: Some(request),
            got: got.clone(),
        };
        let memory = Memory::default();
        let cx = ToolCx {
            mode: PermissionMode::Manual,
            trust: ProjectTrust::Trusted,
            root: Path::new("/p"),
            home: None,
            reach: EditorReach::Own,
            sight: EditorSight::OnScreen,
            context: None,
            tab: TAB,
            session: "session",
            call,
            agent_call: AgentCall::Host,
            memory: &memory,
        };
        drive_with_cards(
            Box::new(run),
            &cx,
            |call| panic!("a card's run asked the editor for {call:?}"),
            |_| (decision, origin),
        );
        let answer = got.borrow_mut().take();
        answer.expect("the card was answered")
    }

    fn card_bound_to(kind: ExecKind, code: &str) -> CardRequest {
        CardRequest {
            binding: Some(digest(kind, code)),
            ..CardRequest::new("Run this in your editor?", vec![])
        }
    }

    fn pending(kind: ExecKind, code: &str) -> PendingExec {
        PendingExec::new(TAB, CALL, kind, code.to_owned())
    }

    fn allow(request: CardRequest) -> CardAnswer {
        answer_to(request, CardDecision::Allow, CardOrigin::Key, CALL)
    }

    /// A record of `code`, through a card raised for it and answered with a key.
    fn approved(kind: ExecKind, code: &str) -> ApprovalRecord {
        let answer = allow(card_bound_to(kind, code));
        match ApprovalRecord::from_card(pending(kind, code), answer, TAB, CALL) {
            Ok(record) => record,
            Err(refused) => panic!("a matching card was refused: {refused:?}"),
        }
    }

    fn refusal(result: Result<ApprovalRecord, ExecRefused>) -> ExecRefused {
        match result {
            Ok(_) => panic!("a record was made"),
            Err(refused) => refused,
        }
    }

    #[derive(Default)]
    struct Recording(RefCell<Vec<(&'static str, Vec<Value>)>>);

    impl EditorRpc for Recording {
        fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
            self.0.borrow_mut().push((code, args));
            Pending::failed(RpcError::Closed)
        }

        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    fn stamp() -> Stamp {
        Stamp {
            deadline_ms: 9,
            scope: "s-1".to_owned(),
            seq: 3,
        }
    }

    fn the_args(kind: &str, code: &str) -> Value {
        Value::Map(vec![
            (Value::from("kind"), Value::from(kind)),
            (Value::from("code"), Value::from(code)),
        ])
    }

    #[test]
    fn the_chunk_opens_with_the_stamp_check() {
        assert!(AGENT_EXEC_LUA.starts_with(include_str!("../prelude.lua")));
    }

    #[test]
    fn the_chunk_compiles() {
        let lua = mlua::Lua::new();
        lua.load(AGENT_EXEC_LUA).into_function().expect("Lua source");
    }

    fn wrapped(needle: &str) -> String {
        format!("local a = 1\n{needle}\nlocal b = 2\n")
    }

    #[test]
    fn each_hint_is_found() {
        let table: [(&str, &[&str]); 4] = [
            (
                SHELL,
                &[
                    ":!",
                    "system",
                    "systemlist",
                    "jobstart",
                    "termopen",
                    "vim.system",
                    "vim.fn.system",
                    "io.popen",
                    "os.execute",
                    "uv.spawn",
                ],
            ),
            (
                FILES,
                &[
                    ":w",
                    ":wa",
                    ":sav",
                    ":up",
                    "writefile",
                    "io.open",
                    "os.remove",
                    "os.rename",
                    "fs_write",
                    "fs_unlink",
                    "delete(",
                    "bufdo",
                    "windo",
                ],
            ),
            (
                KEYS,
                &[
                    "nvim_feedkeys",
                    "nvim_input",
                    ":normal",
                    "nvim_buf_set_lines",
                    "nvim_buf_set_text",
                ],
            ),
            (LOADS, &["require", ":source", ":runtime", "loadfile", "dofile"]),
        ];
        for (line, needles) in table {
            for needle in needles {
                assert_eq!(hints(&wrapped(needle)), [line], "{needle}");
            }
        }
    }

    #[test]
    fn no_hint_for_plain_code() {
        assert!(hints("").is_empty());
        assert!(hints(&wrapped("return vim.bo.filetype")).is_empty());
    }

    #[test]
    fn a_hint_is_listed_once() {
        assert_eq!(hints("vim.fn.system('a')\nos.execute('b')\nio.popen('c')"), [SHELL]);
    }

    #[test]
    fn hints_come_in_their_order() {
        assert_eq!(
            hints("require('x')\nnvim_input('y')\nwritefile({}, 'z')\njobstart('w')"),
            [SHELL, FILES, KEYS, LOADS]
        );
    }

    #[test]
    fn hints_search_code_and_never_parse() {
        assert_eq!(hints("local text = 'os.execute'"), [SHELL]);
        assert_eq!(hints("-- require('x')"), [LOADS]);
        assert_eq!(hints("\" :normal gg"), [KEYS]);
    }

    #[test]
    fn hints_match_the_case_as_written() {
        assert!(hints("REQUIRE SYSTEM BUFDO").is_empty());
    }

    #[test]
    fn the_digest_changes_with_the_kind() {
        assert_ne!(digest(ExecKind::Ex, "x"), digest(ExecKind::Lua, "x"));
    }

    #[test]
    fn the_digest_changes_with_one_byte_of_code() {
        assert_ne!(digest(ExecKind::Lua, "return 1"), digest(ExecKind::Lua, "return 2"));
        assert_ne!(digest(ExecKind::Lua, "return 1"), digest(ExecKind::Lua, "return 1 "));
    }

    #[test]
    fn no_two_pairs_share_bytes() {
        let expected: [u8; 32] = Sha256::digest(b"eitri-exec-v1\0lua\0return 1").into();
        assert_eq!(digest(ExecKind::Lua, "return 1"), expected);

        let pairs = [
            (ExecKind::Ex, ""),
            (ExecKind::Lua, ""),
            (ExecKind::Ex, "lua"),
            (ExecKind::Ex, "\0"),
            (ExecKind::Ex, "\0lua"),
            (ExecKind::Ex, "lua\0"),
            (ExecKind::Lua, "\0"),
            (ExecKind::Lua, "ex\0"),
            (ExecKind::Lua, "\0ex"),
        ];
        for (at, one) in pairs.iter().enumerate() {
            for other in &pairs[at + 1..] {
                assert_ne!(digest(one.0, one.1), digest(other.0, other.1), "{one:?} {other:?}");
            }
        }
    }

    #[test]
    fn a_record_holds_what_was_approved() {
        let record = approved(ExecKind::Lua, "return 1");
        assert_eq!((record.tab(), record.call()), (TAB, CALL));
        assert_eq!(record.digest(), digest(ExecKind::Lua, "return 1"));
        assert_eq!(record.kind, ExecKind::Lua);
        assert_eq!(record.code, "return 1");
    }

    #[test]
    fn a_deny_records_nothing() {
        let request = card_bound_to(ExecKind::Lua, "return 1");
        let answer = answer_to(request, CardDecision::Deny, CardOrigin::Key, CALL);
        let result = ApprovalRecord::from_card(pending(ExecKind::Lua, "return 1"), answer, TAB, CALL);
        assert_eq!(refusal(result), ExecRefused::NotAllowed);
    }

    #[test]
    fn a_wrong_hash_is_refused() {
        let answer = allow(card_bound_to(ExecKind::Lua, "return 1"));
        let result = ApprovalRecord::from_card(pending(ExecKind::Lua, "return 2"), answer, TAB, CALL);
        assert_eq!(refusal(result), ExecRefused::OtherText);
    }

    #[test]
    fn a_card_with_no_binding_is_refused() {
        let answer = allow(CardRequest::new("Run this in your editor?", vec![]));
        let result = ApprovalRecord::from_card(pending(ExecKind::Lua, "return 1"), answer, TAB, CALL);
        assert_eq!(refusal(result), ExecRefused::OtherText);
    }

    #[test]
    fn a_changed_kind_is_refused() {
        let answer = allow(card_bound_to(ExecKind::Lua, "echo 1"));
        let result = ApprovalRecord::from_card(pending(ExecKind::Ex, "echo 1"), answer, TAB, CALL);
        assert_eq!(refusal(result), ExecRefused::OtherText);
    }

    #[test]
    fn another_tab_is_refused() {
        let answer = allow(card_bound_to(ExecKind::Lua, "return 1"));
        let result = ApprovalRecord::from_card(pending(ExecKind::Lua, "return 1"), answer, TAB + 1, CALL);
        assert_eq!(refusal(result), ExecRefused::OtherTab);
    }

    #[test]
    fn another_calls_answer_is_refused() {
        let code = "return 1";
        let bound = || card_bound_to(ExecKind::Lua, code);

        let answer = answer_to(bound(), CardDecision::Allow, CardOrigin::Key, "call-2");
        let result = ApprovalRecord::from_card(pending(ExecKind::Lua, code), answer, TAB, CALL);
        assert_eq!(refusal(result), ExecRefused::OtherCall, "the answer is another call's");

        let result = ApprovalRecord::from_card(pending(ExecKind::Lua, code), allow(bound()), TAB, "call-2");
        assert_eq!(refusal(result), ExecRefused::OtherCall, "the run is another call's");

        let elsewhere = PendingExec::new(TAB, "call-2", ExecKind::Lua, code.to_owned());
        let result = ApprovalRecord::from_card(elsewhere, allow(bound()), TAB, CALL);
        assert_eq!(refusal(result), ExecRefused::OtherCall, "the text is another call's");
    }

    #[test]
    fn the_origin_is_kept() {
        for origin in [CardOrigin::Key, CardOrigin::Click] {
            let code = "return 1";
            let answer = answer_to(card_bound_to(ExecKind::Lua, code), CardDecision::Allow, origin, CALL);
            let record = ApprovalRecord::from_card(pending(ExecKind::Lua, code), answer, TAB, CALL);
            let Ok(record) = record else {
                panic!("a matching card was refused");
            };
            assert_eq!(record.origin(), origin);
        }
    }

    #[test]
    fn the_slot_runs_once() {
        let rpc = Recording::default();
        let slot = ExecSlot::new(approved(ExecKind::Lua, "return 1"));
        let stamp = stamp();
        let _first = send(&rpc, &stamp, &slot);
        let second = send(&rpc, &stamp, &slot);
        assert!(
            matches!(second.try_take(), Some(Err(RpcError::Encode(text))) if text == "this approval was spent"),
            "a spent slot answers so"
        );
        let calls = rpc.0.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, AGENT_EXEC_LUA);
        assert_eq!(calls[0].1, vec![stamp.to_value(), the_args("lua", "return 1")]);
    }

    #[test]
    fn each_kind_goes_by_its_word() {
        for (kind, word) in [(ExecKind::Ex, "ex"), (ExecKind::Lua, "lua")] {
            let args = exec_args(&stamp(), approved(kind, "x"));
            assert_eq!(args, vec![stamp().to_value(), the_args(word, "x")]);
        }
    }

    #[test]
    fn this_windows_way_runs_the_slot() {
        let rpc = Rc::new(Recording::default());
        let way = LocalWay(rpc.clone());
        let call = EditorCall::Exec(ExecSlot::new(approved(ExecKind::Ex, "echo 1")));
        let _answer = way.call("session", &call, &stamp());
        let calls = rpc.0.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, AGENT_EXEC_LUA);
    }

    #[test]
    fn no_code_crosses_the_link() {
        let way = RemoteWay::new(Path::new("/p"), Box::new(|op| panic!("an op crossed the link: {op:?}")));
        let call = EditorCall::Exec(ExecSlot::new(approved(ExecKind::Lua, "return 1")));
        let answer = way.call("session", &call, &stamp());
        assert!(
            matches!(answer.try_take(), Some(Err(RpcError::Unavailable(text))) if text == words::MOVE_TO_EDITOR),
            "the tab is told where it can run"
        );
        let EditorCall::Exec(slot) = &call else {
            panic!("the call changed");
        };
        assert!(slot.0.borrow().is_some(), "the approval was not spent");
    }

    #[test]
    fn the_window_counts_from_the_answer() {
        let record = approved(ExecKind::Lua, "return 1");
        let at = record.answered_ms;
        assert_eq!(record.budget(at), Some(APPROVAL_WINDOW));
        assert_eq!(record.budget(at + 3_000), Some(Duration::from_secs(2)));
        assert_eq!(
            record.budget(at - 1),
            Some(APPROVAL_WINDOW),
            "a clock set back does not lengthen the window"
        );
    }

    #[test]
    fn past_the_window_there_is_no_budget() {
        let record = approved(ExecKind::Lua, "return 1");
        let at = record.answered_ms;
        assert_eq!(record.budget(at + 5_000), None);
        assert_eq!(record.budget(at + 60_000), None);
        assert_eq!(record.budget(u64::MAX), None);
    }

    #[test]
    fn a_budget_at_the_margin_is_none() {
        let record = approved(ExecKind::Lua, "return 1");
        let at = record.answered_ms;
        let margin = STAMP_MARGIN.as_millis() as u64;
        assert_eq!(record.budget(at + 5_000 - margin), None);
        assert_eq!(
            record.budget(at + 5_000 - margin - 1),
            Some(STAMP_MARGIN + Duration::from_millis(1))
        );
    }

    #[test]
    fn the_slots_debug_shows_no_code() {
        let call = EditorCall::Exec(ExecSlot::new(approved(ExecKind::Lua, "return 'a secret'")));
        let shown = format!("{call:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert_eq!(shown, "Exec(ExecSlot)");
    }

    fn map(entries: Vec<(&str, Value)>) -> Value {
        Value::Map(
            entries
                .into_iter()
                .map(|(key, value)| (Value::from(key), value))
                .collect(),
        )
    }

    /// A msgpack str holding `bytes`, as it comes off the wire whatever the bytes are.
    fn str_on_the_wire(bytes: &[u8]) -> Value {
        let mut frame = vec![0xa0 | bytes.len() as u8];
        frame.extend_from_slice(bytes);
        rmpv::decode::read_value(&mut frame.as_slice()).expect("a msgpack str")
    }

    #[test]
    fn parse_keeps_output_that_is_not_utf8() {
        let raw = str_on_the_wire(b"a\xffb");
        assert_eq!(raw.as_str(), None, "these bytes are no str to rmpv");
        let answer = parse(&map(vec![("output", raw.clone())])).expect("the code ran, so there is an answer");
        assert_eq!(
            answer,
            ExecAnswer {
                text: "a\u{fffd}b".to_owned(),
                failed: false,
                cut: false,
            }
        );
        let answer = parse(&map(vec![("failed", raw)])).expect("the code ran, so there is an answer");
        assert_eq!(answer.text, "a\u{fffd}b");
        assert!(answer.failed);
        let answer = parse(&map(vec![("output", Value::Binary(b"bin".to_vec()))])).expect("bytes");
        assert_eq!(answer.text, "bin");
    }

    #[test]
    fn parse_reads_what_was_cut() {
        let answer = parse(&map(vec![("output", Value::from("a")), ("cut", Value::from(true))])).unwrap();
        assert!(answer.cut);
    }

    #[test]
    fn parse_refuses_an_answer_with_neither_field() {
        assert!(parse(&map(vec![("other", Value::from(1))])).is_err());
        assert!(parse(&Value::from(1)).is_err());
        assert!(parse(&map(vec![("output", Value::from(1))])).is_err());
    }

    const WHY: &str = "to count the buffers";
    const EXEC_ON: &str = "agent.editor.tool.nvim_exec";

    fn lua(code: &str) -> serde_json::Value {
        serde_json::json!({ "kind": "lua", "code": code, "why": WHY })
    }

    fn exec_cx(memory: &Memory) -> ToolCx<'_> {
        ToolCx {
            mode: PermissionMode::Manual,
            trust: ProjectTrust::Trusted,
            root: Path::new("/p"),
            home: None,
            reach: EditorReach::Own,
            sight: EditorSight::OnScreen,
            context: None,
            tab: TAB,
            session: "session",
            call: CALL,
            agent_call: AgentCall::Claude(Some("toolu-1")),
            memory,
        }
    }

    fn begun(args: &serde_json::Value, cx: &ToolCx<'_>) -> Box<dyn ToolRun> {
        match NvimExec.begin(args, cx) {
            Ok(run) => run,
            Err(ToolError(why)) => panic!("refused: {why}"),
        }
    }

    fn refused(args: &serde_json::Value, cx: &ToolCx<'_>) -> String {
        match NvimExec.begin(args, cx) {
            Ok(_) => panic!("the call was taken"),
            Err(ToolError(why)) => why,
        }
    }

    fn result_of(steps: &[Step]) -> &ToolResult {
        match steps.last() {
            Some(Step::Done(result)) => result,
            other => panic!("no result: {other:?}"),
        }
    }

    fn the_card(steps: &[Step]) -> &CardRequest {
        match steps.first() {
            Some(Step::Ask(request)) => request,
            other => panic!("no card first: {other:?}"),
        }
    }

    /// Every step of a call of `code`, the card answered `decision` and the editor saying `reply`.
    fn driven(code: &str, decision: CardDecision, reply: Result<Value, EditorFailure>) -> Vec<Step> {
        let memory = Memory::default();
        let cx = exec_cx(&memory);
        let mut reply = Some(reply);
        drive_with_cards(
            begun(&lua(code), &cx),
            &cx,
            |_| reply.take().expect("one call on the editor"),
            |_| (decision, CardOrigin::Key),
        )
    }

    fn silent() -> Result<Value, EditorFailure> {
        Ok(map(vec![("output", Value::from(""))]))
    }

    fn every_line(warnings: &[&str]) -> Vec<CardLine> {
        warnings
            .iter()
            .map(|line| CardLine::Warning((*line).to_owned()))
            .collect()
    }

    #[test]
    fn every_mode_shows_the_card() {
        for mode in [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass] {
            for trust in [ProjectTrust::Trusted, ProjectTrust::Untrusted] {
                assert_eq!(verdict(Need::Exec, mode, trust), Verdict::Card, "{mode:?} {trust:?}");
            }
            let memory = Memory::default();
            let cx = ToolCx {
                mode,
                ..exec_cx(&memory)
            };
            let mut cards = 0;
            let steps = drive_with_cards(
                begun(&lua("return 1"), &cx),
                &cx,
                |call| panic!("{mode:?} reached the editor: {call:?}"),
                |_| {
                    cards += 1;
                    (CardDecision::Deny, CardOrigin::Key)
                },
            );
            assert_eq!(cards, 1, "{mode:?}");
            assert_eq!(the_card(&steps).title, CARD_TITLE, "{mode:?}");
        }
    }

    #[test]
    fn bypass_runs_nothing_before_a_person_answers() {
        let world = World::local(PermissionMode::Bypass);
        let mut broker = Broker::new();
        world.submit(&mut broker, "a", &lua("return 1"));
        for _ in 0..4 {
            broker.tick(&world, Instant::now());
        }
        assert_eq!(world.cards(&broker).len(), 1);
        assert!(world.rpc.0.borrow().is_empty(), "nothing ran before the answer");
    }

    #[test]
    fn the_card_shows_the_code_whole_and_its_lines() {
        let code = "local n = 0\nfor _ in pairs(vim.api.nvim_list_bufs()) do n = n + 1 end\nreturn n";
        let steps = driven(code, CardDecision::Deny, silent());
        let card = the_card(&steps);
        assert_eq!(card.title, "Run this in your editor?");
        let mut expected = vec![
            CardLine::Plain("Lua".to_owned()),
            CardLine::Code(code.to_owned()),
            CardLine::AgentWords(WHY.to_owned()),
        ];
        expected.extend(every_line(&[
            "Runs with your user's full rights inside your editor. Nothing sandboxes it.",
            "The agent may have been steered by text it read: files, web pages, diagnostics, tool output.",
            "Changes it makes to buffers are not in turn review until saved; files it writes appear in turn review \
             without a call that explains them.",
            "Code that never ends freezes your editor; Eitri cannot stop it. If your editor is busy for more than 5 s \
             after you approve, it does not run.",
            "These hints are a text search, not an analysis: no hint does not mean safe.",
        ]));
        assert_eq!(card.body, expected);

        let memory = Memory::default();
        let cx = exec_cx(&memory);
        let ex = serde_json::json!({ "kind": "ex", "code": "echo 1", "why": WHY });
        let steps = drive_with_cards(
            begun(&ex, &cx),
            &cx,
            |call| panic!("{call:?}"),
            |_| (CardDecision::Deny, CardOrigin::Key),
        );
        assert_eq!(the_card(&steps).body[0], CardLine::Plain("Ex commands".to_owned()));
    }

    #[test]
    fn a_hint_shows_its_line_and_the_closing_line_always_shows() {
        let lines = |code: &str| {
            let steps = driven(code, CardDecision::Deny, silent());
            the_card(&steps).body[3 + WARNINGS.len()..].to_vec()
        };
        assert_eq!(
            lines("os.execute('ls')\nrequire('x')"),
            every_line(&[SHELL, LOADS, HINTS_ARE_A_SEARCH])
        );
        assert_eq!(lines("return 1"), every_line(&[HINTS_ARE_A_SEARCH]));
    }

    #[test]
    fn a_card_offers_no_always() {
        let world = World::local(PermissionMode::Manual);
        let mut broker = Broker::new();
        world.submit(&mut broker, "a", &lua("return 1"));
        broker.tick(&world, Instant::now());
        let wire = serde_json::to_value(&world.cards(&broker)[0]).unwrap();
        let mut fields: Vec<&str> = wire.as_object().unwrap().keys().map(String::as_str).collect();
        fields.sort_unstable();
        assert_eq!(fields, ["body", "id", "seq", "title", "tool", "toolUseId"]);
        assert!(!wire.to_string().to_lowercase().contains("always"), "{wire}");
    }

    #[test]
    fn a_deny_runs_nothing() {
        let memory = Memory::default();
        let cx = exec_cx(&memory);
        let steps = drive_with_cards(
            begun(&lua("return 1"), &cx),
            &cx,
            |call| panic!("a denied call reached the editor: {call:?}"),
            |_| (CardDecision::Deny, CardOrigin::Key),
        );
        assert_eq!(
            *result_of(&steps),
            ToolResult {
                text: "the person did not allow it".to_owned(),
                is_error: true,
            }
        );
    }

    #[test]
    fn an_approval_runs_the_shown_text() {
        let rpc = Recording::default();
        let memory = Memory::default();
        let cx = exec_cx(&memory);
        let code = "return 'shown'";
        let steps = drive_with_cards(
            begun(&lua(code), &cx),
            &cx,
            |call| {
                let EditorCall::Exec(slot) = call else {
                    panic!("another call: {call:?}");
                };
                let _ = send(&rpc, &stamp(), slot);
                Ok(map(vec![("output", Value::from("\"shown\""))]))
            },
            |_| (CardDecision::Allow, CardOrigin::Click),
        );
        assert_eq!(
            *result_of(&steps),
            ToolResult {
                text: "\"shown\"".to_owned(),
                is_error: false,
            }
        );
        assert!(the_card(&steps).body.contains(&CardLine::Code(code.to_owned())));
        let calls = rpc.0.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1[1], the_args("lua", code));
    }

    #[test]
    fn the_card_is_raised_with_the_digest_of_what_it_shows() {
        let code = "return 'bound'";
        let steps = driven(code, CardDecision::Deny, silent());
        assert_eq!(the_card(&steps).binding, Some(digest(ExecKind::Lua, code)));
    }

    fn deadline_of(world: &World) -> u64 {
        let calls = world.rpc.0.borrow();
        field(&calls[0].1[0], "deadline_ms")
            .and_then(Value::as_u64)
            .expect("a stamp")
    }

    #[test]
    fn the_deadline_is_the_answer_plus_five_seconds_less_the_margin() {
        let world = World::local(PermissionMode::Manual);
        let mut broker = Broker::new();
        world.submit(&mut broker, "a", &lua("return 1"));
        broker.tick(&world, Instant::now());
        let card = world.cards(&broker)[0].id.clone();

        let before = wall_ms();
        broker
            .answer_card(TAB_ID, &card, CardDecision::Allow, CardOrigin::Key, Instant::now())
            .unwrap();
        let after = wall_ms();
        std::thread::sleep(Duration::from_secs(3));
        broker.tick(&world, Instant::now());

        // Wide enough for a slow machine, and far under the 3 s a count from the admission would add.
        let slack = 1_000;
        let stamped = (APPROVAL_WINDOW - STAMP_MARGIN).as_millis() as u64;
        let deadline = deadline_of(&world);
        assert!(deadline + slack >= before + stamped, "{deadline} {before}");
        assert!(deadline <= after + stamped + slack, "{deadline} {after}");
    }

    #[test]
    fn an_approval_with_little_left_sends_nothing() {
        let world = World::local(PermissionMode::Manual);
        let mut broker = Broker::new();
        world.submit(&mut broker, "a", &lua("return 1"));
        broker.tick(&world, Instant::now());
        let card = world.cards(&broker)[0].id.clone();

        broker
            .answer_card(TAB_ID, &card, CardDecision::Allow, CardOrigin::Key, Instant::now())
            .unwrap();
        std::thread::sleep(APPROVAL_WINDOW - STAMP_MARGIN + Duration::from_millis(50));
        let out = broker.tick(&world, Instant::now());

        assert_eq!(only_answer(&out).text, NOT_ANSWERED);
        assert!(world.rpc.0.borrow().is_empty(), "nothing was sent");
    }

    /// The window has time left when the run reads it, and the tick that admitted the run has used all of that.
    #[test]
    fn a_tick_that_ran_past_the_budget_sends_nothing() {
        let world = World::local(PermissionMode::Manual);
        let mut broker = Broker::new();
        let began = Instant::now();
        world.submit(&mut broker, "a", &lua("return 1"));
        broker.tick(&world, began);
        let card = world.cards(&broker)[0].id.clone();

        broker
            .answer_card(TAB_ID, &card, CardDecision::Allow, CardOrigin::Key, began)
            .unwrap();
        let long_ago = began.checked_sub(APPROVAL_WINDOW * 2).expect("an uptime past 10 s");
        let out = broker.tick(&world, long_ago);

        assert_eq!(only_answer(&out).text, NOT_ANSWERED);
        assert!(world.rpc.0.borrow().is_empty(), "nothing was sent");
    }

    #[test]
    fn an_approval_past_its_window_runs_nothing_and_says_so() {
        let memory = Memory::default();
        let cx = exec_cx(&memory);
        let run = ExecRun {
            stage: Stage::Asking {
                kind: ExecKind::Lua,
                code: "return 1".to_owned(),
                why: WHY.to_owned(),
            },
            clock: || wall_ms() + APPROVAL_WINDOW.as_millis() as u64,
        };
        let steps = drive_with_cards(
            Box::new(run),
            &cx,
            |call| panic!("a late approval reached the editor: {call:?}"),
            |_| (CardDecision::Allow, CardOrigin::Key),
        );
        assert_eq!(result_of(&steps).text, NOT_ANSWERED);
        assert!(result_of(&steps).is_error);
    }

    #[test]
    fn a_late_chunk_is_the_timeout_sentence() {
        let steps = driven("return 1", CardDecision::Allow, Err(EditorFailure::NotAnswered));
        assert_eq!(
            *result_of(&steps),
            ToolResult {
                text:
                    "the editor did not answer within 5 s: if it was busy, the code will not run; if the code itself \
                       is still running, it keeps running"
                        .to_owned(),
                is_error: true,
            }
        );
    }

    #[test]
    fn a_failure_on_the_way_is_said_as_it_came() {
        for failed in [
            EditorFailure::Unavailable("the editor is not running".to_owned()),
            EditorFailure::Refused("nvim said no".to_owned()),
        ] {
            let text = match &failed {
                EditorFailure::Unavailable(text) | EditorFailure::Refused(text) => text.clone(),
                EditorFailure::NotAnswered => unreachable!("not among these"),
            };
            let steps = driven("return 1", CardDecision::Allow, Err(failed));
            assert_eq!(*result_of(&steps), ToolResult { text, is_error: true });
        }
    }

    #[test]
    fn an_error_from_the_code_is_an_error_result() {
        let reply = map(vec![("failed", Value::from("eitri-agent:1: boom"))]);
        let steps = driven("error('boom')", CardDecision::Allow, Ok(reply));
        assert_eq!(
            *result_of(&steps),
            ToolResult {
                text: "eitri-agent:1: boom".to_owned(),
                is_error: true,
            }
        );
    }

    #[test]
    fn a_cut_answer_says_so() {
        let reply = map(vec![("output", Value::from("aaa")), ("cut", Value::from(true))]);
        let steps = driven("return 1", CardDecision::Allow, Ok(reply));
        assert_eq!(result_of(&steps).text, "aaa\n[cut at 64 KiB]");
        assert!(!result_of(&steps).is_error);
    }

    #[test]
    fn an_answer_that_cannot_be_read_is_an_error() {
        let steps = driven("return 1", CardDecision::Allow, Ok(Value::from(1)));
        assert_eq!(result_of(&steps).text, "the editor's answer could not be read");
        assert!(result_of(&steps).is_error);
    }

    #[test]
    fn a_window_2_tab_is_refused_and_remote_way_is_never_asked() {
        let world = World::remote();
        let mut broker = Broker::new();
        let out = world.submit(&mut broker, "a", &lua("return 1"));
        assert_eq!(only_answer(&out).text, "move the tab to the editor's window");
        assert!(broker.tick(&world, Instant::now()).is_empty());
        assert!(world.cards(&broker).is_empty(), "no card for a call that cannot run");
    }

    #[test]
    fn the_tool_stays_declared_for_a_window_2_tab() {
        let on = turned_on();
        let set = client_tool_set(&on, ProjectTrust::Trusted).expect("a server");
        assert!(set.tools.iter().any(|tool| tool.name == "nvim_exec"));

        // Told to move, not told the tool is missing: a session keeps its list across a move.
        let world = World::remote();
        let out = world.submit(&mut Broker::new(), "a", &lua("return 1"));
        assert!(!only_answer(&out).text.contains("not available"));
    }

    #[test]
    fn an_n_session_does_not_declare_it() {
        let names: Vec<&str> = declared(&turned_on(), ProjectTrust::Untrusted)
            .iter()
            .map(|row| row.name)
            .collect();
        assert!(!names.contains(&"nvim_exec"), "{names:?}");
        const { assert!(!ROW.in_untrusted) };
    }

    #[test]
    fn a_project_nobody_was_asked_about_may_have_it() {
        let names: Vec<&str> = declared(&turned_on(), ProjectTrust::NothingFound)
            .iter()
            .map(|row| row.name)
            .collect();
        assert!(names.contains(&"nvim_exec"), "{names:?}");
        let memory = Memory::default();
        let cx = ToolCx {
            trust: ProjectTrust::NothingFound,
            ..exec_cx(&memory)
        };
        drop(begun(&lua("return 1"), &cx));
    }

    #[test]
    fn an_n_session_is_refused_if_it_is_called_anyway() {
        let memory = Memory::default();
        let cx = ToolCx {
            trust: ProjectTrust::Untrusted,
            ..exec_cx(&memory)
        };
        assert_eq!(
            refused(&lua("return 1"), &cx),
            "not offered in a session started with n"
        );
    }

    #[test]
    fn it_is_off_by_default() {
        const { assert!(!ROW.default_on) };
        for switches in [Switches::on_by_default(), Switches::parse(None, &[]).unwrap()] {
            let names: Vec<&str> = declared(&switches, ProjectTrust::Trusted)
                .iter()
                .map(|row| row.name)
                .collect();
            assert!(!names.contains(&"nvim_exec"), "{names:?}");
        }
    }

    fn turned_on() -> Switches {
        Switches::parse(None, &[(EXEC_ON.to_owned(), "true".to_owned())]).unwrap()
    }

    #[test]
    fn the_key_turns_it_on() {
        for word in ["true", "on"] {
            let switches = Switches::parse(None, &[(EXEC_ON.to_owned(), word.to_owned())]).unwrap();
            assert!(switches.tool_on(&ROW), "{word}");
        }
        let off = Switches::parse(Some("off"), &[(EXEC_ON.to_owned(), "on".to_owned())]).unwrap();
        assert!(!off.tool_on(&ROW), "the key for every tool still turns it off");
    }

    #[test]
    fn exec_stays_deferred() {
        let set = client_tool_set(&turned_on(), ProjectTrust::Trusted).expect("a server");
        let row = set
            .tools
            .iter()
            .find(|tool| tool.name == "nvim_exec")
            .expect("declared");
        assert!(!row.always_load);
    }

    #[test]
    fn a_claude_call_with_no_tool_use_id_is_refused() {
        let memory = Memory::default();
        let cx = ToolCx {
            agent_call: AgentCall::Claude(None),
            ..exec_cx(&memory)
        };
        assert_eq!(
            refused(&lua("return 1"), &cx),
            "this call has no tool_use_id, so it cannot be tied to an approval"
        );
    }

    #[test]
    fn a_run_the_host_started_is_refused() {
        let memory = Memory::default();
        let cx = ToolCx {
            agent_call: AgentCall::Host,
            ..exec_cx(&memory)
        };
        assert!(!refused(&lua("return 1"), &cx).is_empty());
    }

    fn request(call: u64) -> BridgeCallId {
        BridgeCallId {
            tab: TAB,
            connection: 1,
            call,
        }
    }

    #[test]
    fn a_codex_call_keys_on_its_call_id() {
        let memory = Memory::default();
        let (first, second) = (request(1), request(2));
        let bridged = |call_id, request| ToolCx {
            agent_call: AgentCall::Bridge { call_id, request },
            ..exec_cx(&memory)
        };
        let _run = begun(&lua("return 1"), &bridged(Some("call_a"), &first));
        assert_eq!(
            refused(&lua("return 1"), &bridged(Some("call_a"), &second)),
            "this call was already handled",
            "the same call by the model, under a new request"
        );
        let _run = begun(&lua("return 1"), &bridged(Some("call_b"), &first));
    }

    #[test]
    fn an_acp_call_keys_on_the_bridge_request() {
        let memory = Memory::default();
        let (first, second) = (request(1), request(2));
        let bridged = |request| ToolCx {
            agent_call: AgentCall::Bridge { call_id: None, request },
            ..exec_cx(&memory)
        };
        let _run = begun(&lua("return 1"), &bridged(&first));
        assert_eq!(
            refused(&lua("return 1"), &bridged(&first)),
            "this call was already handled"
        );
        let _run = begun(&lua("return 1"), &bridged(&second));
    }

    #[test]
    fn an_id_cannot_pass_for_a_request() {
        let bridge = request(1);
        let keyed = |call_id| {
            call_key(AgentCall::Bridge {
                call_id,
                request: &bridge,
            })
        };
        let by_request = keyed(None).unwrap_or_else(|ToolError(why)| panic!("{why}"));
        let by_id = keyed(Some("request\x001\x001")).unwrap_or_else(|ToolError(why)| panic!("{why}"));
        assert_ne!(by_request, by_id);
    }

    #[test]
    fn a_reused_id_raises_no_second_card() {
        let world = World::local(PermissionMode::Manual);
        let mut broker = Broker::new();
        assert!(world.submit(&mut broker, "a", &lua("return 1")).is_empty());
        broker.tick(&world, Instant::now());

        let out = world.submit(&mut broker, "a", &lua("return 1"));
        assert_eq!(only_answer(&out).text, "this call was already handled");
        broker.tick(&world, Instant::now());
        assert_eq!(world.cards(&broker).len(), 1, "the first card stands alone");
    }

    #[test]
    fn a_retry_with_a_new_id_raises_a_new_card() {
        let world = World::local(PermissionMode::Manual);
        let mut broker = Broker::new();
        world.submit(&mut broker, "a", &lua("return 1"));
        broker.tick(&world, Instant::now());
        let card = world.cards(&broker)[0].id.clone();
        broker
            .answer_card(TAB_ID, &card, CardDecision::Deny, CardOrigin::Key, Instant::now())
            .unwrap();
        broker.tick(&world, Instant::now());
        assert!(world.cards(&broker).is_empty());

        world.submit(&mut broker, "b", &lua("return 1"));
        broker.tick(&world, Instant::now());
        let cards = world.cards(&broker);
        assert_eq!(cards.len(), 1);
        assert_ne!(cards[0].id, card);
    }

    #[test]
    fn a_call_turned_away_for_its_arguments_is_not_seen() {
        let memory = Memory::default();
        let cx = exec_cx(&memory);
        assert_eq!(refused(&lua(""), &cx), "code may not be empty");
        let _run = begun(&lua("return 1"), &cx);
    }

    #[test]
    fn code_over_16_kib_is_refused() {
        let memory = Memory::default();
        let cx = exec_cx(&memory);
        let over = "a".repeat(CODE_LIMIT + 1);
        assert_eq!(refused(&lua(&over), &cx), "code is over 16 KiB");
        let _run = begun(&lua(&over[1..]), &cx);
    }

    #[test]
    fn an_empty_code_is_refused() {
        let memory = Memory::default();
        assert_eq!(refused(&lua(""), &exec_cx(&memory)), "code may not be empty");
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        let memory = Memory::default();
        let args = serde_json::json!({ "kind": "vimscript", "code": "x", "why": WHY });
        assert_eq!(refused(&args, &exec_cx(&memory)), "kind must be ex or lua");
    }

    #[test]
    fn a_code_of_the_wrong_type_is_refused() {
        let memory = Memory::default();
        let cx = exec_cx(&memory);
        for wrong in [
            serde_json::json!(1),
            serde_json::json!(["x"]),
            serde_json::json!({ "x": 1 }),
        ] {
            let args = serde_json::json!({ "kind": "lua", "code": wrong, "why": WHY });
            assert_eq!(refused(&args, &cx), "code must be a string");
        }
        let args = serde_json::json!({ "kind": 1, "code": "x", "why": WHY });
        assert_eq!(refused(&args, &cx), "kind must be a string");
    }

    #[test]
    fn why_is_required() {
        let memory = Memory::default();
        let cx = exec_cx(&memory);
        let args = serde_json::json!({ "kind": "lua", "code": "x" });
        assert_eq!(refused(&args, &cx), "why is required");
        let args = serde_json::json!({ "kind": "lua", "code": "x", "why": null });
        assert_eq!(refused(&args, &cx), "why is required");
        let args = serde_json::json!({ "kind": "lua", "code": "x", "why": "a".repeat(WHY_LIMIT + 1) });
        assert_eq!(refused(&args, &cx), "why is over 4 KiB");
    }

    const TAB_ID: TabId = TabId(TAB);

    /// A window as the real broker sees it, with one tab whose session was given `nvim_exec`.
    struct World {
        way: Box<dyn EditorWay>,
        rpc: Rc<Recording>,
        mode: PermissionMode,
    }

    impl World {
        fn local(mode: PermissionMode) -> World {
            let rpc = Rc::new(Recording::default());
            World {
                way: Box::new(LocalWay(rpc.clone())),
                rpc,
                mode,
            }
        }

        fn remote() -> World {
            World {
                way: Box::new(RemoteWay::new(
                    Path::new("/p"),
                    Box::new(|op| panic!("an op crossed the link: {op:?}")),
                )),
                ..World::local(PermissionMode::Manual)
            }
        }

        fn submit(&self, broker: &mut Broker, id: &str, args: &serde_json::Value) -> Vec<BrokerOut> {
            let caller = Caller::Sidecar { call_id: id.to_owned() };
            let tool_use_id = Some(format!("toolu-{id}"));
            broker.submit(
                TAB_ID,
                caller,
                tool_use_id,
                "nvim_exec",
                &args.to_string(),
                self,
                Instant::now(),
            )
        }

        fn cards(&self, broker: &Broker) -> Vec<crate::editor_tools::cards::EditorCardWire> {
            broker.cards(TAB_ID)
        }
    }

    impl BrokerEnv for World {
        fn way(&self) -> Option<&dyn EditorWay> {
            Some(self.way.as_ref())
        }

        fn facts(&self, tab: TabId) -> Option<TabFacts> {
            (tab == TAB_ID).then(|| TabFacts {
                mode: self.mode,
                trust: ProjectTrust::Trusted,
                session: "session".to_owned(),
                process: "process".to_owned(),
                revision: 1,
                declared: Rc::new(BTreeSet::from(["nvim_exec".to_owned()])),
            })
        }

        fn root(&self) -> &Path {
            Path::new("/p")
        }

        fn home(&self) -> Option<&Path> {
            None
        }

        fn context(&self) -> Option<EditorContext> {
            None
        }
    }

    fn only_answer(out: &[BrokerOut]) -> ToolResult {
        let answers: Vec<&ToolResult> = out
            .iter()
            .filter_map(|out| match out {
                BrokerOut::Answer { result, .. } => Some(result),
                BrokerOut::CardsChanged { .. } => None,
            })
            .collect();
        assert_eq!(answers.len(), 1, "{out:?}");
        answers[0].clone()
    }

    const FAR_MS: u64 = 60_000;

    struct Nvim {
        embed: Embed,
        dir: PathBuf,
        seq: u64,
    }

    impl Drop for Nvim {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Nvim {
        fn start(case: &str) -> Nvim {
            let dir = std::env::temp_dir().join(format!("etx-{}-{case}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let embed = Embed::start(&dir, &[], &[]);
            Nvim { embed, dir, seq: 0 }
        }

        /// The chunk's reply to an approved text.
        fn reply(&mut self, kind: ExecKind, code: &str) -> Value {
            self.seq += 1;
            let stamp = Stamp {
                deadline_ms: wall_ms() + FAR_MS,
                scope: "exec".to_owned(),
                seq: self.seq,
            };
            self.embed.lua(AGENT_EXEC_LUA, exec_args(&stamp, approved(kind, code)))
        }

        fn run(&mut self, kind: ExecKind, code: &str) -> ExecAnswer {
            parse(&self.reply(kind, code)).expect("the chunk's answer")
        }

        fn global(&mut self, name: &str) -> Value {
            self.embed.eval(&format!("get(g:, '{name}', 'unset')"))
        }
    }

    fn expired(reply: &Value) -> bool {
        reply.as_map().is_some_and(|map| {
            map.iter()
                .any(|(key, value)| key.as_str() == Some("expired") && *value == Value::from(true))
        })
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn ex_returns_its_output() {
        let mut nvim = Nvim::start("ex");
        let answer = nvim.run(ExecKind::Ex, "echo 'hello'");
        assert_eq!(answer.text, "hello");
        assert!(!answer.failed);
        assert!(!answer.cut);
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn an_ex_error_is_an_answer() {
        let mut nvim = Nvim::start("ex-error");
        let answer = nvim.run(ExecKind::Ex, "nosuchcommand");
        assert!(answer.failed, "{answer:?}");
        assert!(answer.text.contains("E492"), "{answer:?}");
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn lua_returns_its_values() {
        let mut nvim = Nvim::start("lua");
        let answer = nvim.run(ExecKind::Lua, "return 1, 'two', { three = 3 }");
        assert_eq!(answer.text, "1\n\"two\"\n{\n  three = 3\n}");
        assert!(!answer.failed);
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn a_lua_error_is_an_answer() {
        let mut nvim = Nvim::start("lua-error");
        let answer = nvim.run(ExecKind::Lua, "error('boom')");
        assert!(answer.failed, "{answer:?}");
        assert!(answer.text.contains("eitri-agent:1: boom"), "{answer:?}");

        let thrown = "error(setmetatable({}, { __tostring = function() error('again') end }))";
        let answer = nvim.run(ExecKind::Lua, thrown);
        assert!(answer.failed, "{answer:?}");
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn a_syntax_error_is_an_answer() {
        let mut nvim = Nvim::start("syntax");
        let answer = nvim.run(ExecKind::Lua, "return (");
        assert!(answer.failed, "{answer:?}");
        assert!(answer.text.contains("eitri-agent"), "{answer:?}");
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn nil_returns_nothing_and_many_return_each() {
        let mut nvim = Nvim::start("counts");
        assert_eq!(nvim.run(ExecKind::Lua, "local a = 1").text, "", "no value, no line");
        assert_eq!(nvim.run(ExecKind::Lua, "return nil").text, "nil", "one value, one line");
        assert_eq!(
            nvim.run(ExecKind::Lua, "return 1, nil, 'x', nil").text,
            "1\nnil\n\"x\"\nnil"
        );
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn a_value_that_cannot_be_shown_is_said_to_be() {
        let mut nvim = Nvim::start("inspect");
        let deep = "local t = {} for _ = 1, 100000 do t = { t } end return 1, t, 3";
        let answer = nvim.run(ExecKind::Lua, deep);
        assert!(!answer.failed, "the code ran: {answer:?}");
        let lines: Vec<&str> = answer.text.split('\n').collect();
        assert_eq!(lines.first(), Some(&"1"));
        assert_eq!(lines.last(), Some(&"3"));
        assert!(lines[1].starts_with("<could not be shown"), "{answer:?}");
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn output_past_the_limit_is_cut() {
        let mut nvim = Nvim::start("cut");
        let answer = nvim.run(ExecKind::Lua, "return string.rep('a', 70000)");
        assert!(answer.cut);
        assert_eq!(answer.text.len(), RESULT_LIMIT);

        let whole = format!("return string.rep('a', {})", RESULT_LIMIT - 2);
        let answer = nvim.run(ExecKind::Lua, &whole);
        assert!(!answer.cut, "the quotes bring it to the limit exactly");
        assert_eq!(answer.text.len(), RESULT_LIMIT);

        let answer = nvim.run(ExecKind::Lua, "error(string.rep('a', 70000), 0)");
        assert!(answer.failed && answer.cut, "an error is held to the same limit");
        assert_eq!(answer.text.len(), RESULT_LIMIT);
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn a_character_across_the_limit_stays_whole() {
        let mut nvim = Nvim::start("char");
        // The opening quote and these bytes end two short of the limit, so the three-byte character crosses it.
        let code = format!("return string.rep('a', {}) .. '\u{4e2d}' .. 'tail'", RESULT_LIMIT - 3);
        let answer = nvim.run(ExecKind::Lua, &code);
        assert!(answer.cut);
        assert_eq!(answer.text.len(), RESULT_LIMIT - 2);
        assert!(answer.text.ends_with('a'), "the character left whole, not as a piece");
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn non_utf8_output_still_answers_its_text() {
        let mut nvim = Nvim::start("bytes");
        let answer = nvim.run(ExecKind::Lua, "return '\\255abc'");
        assert!(!answer.failed, "{answer:?}");
        assert_eq!(answer.text, "\"\u{fffd}abc\"");

        let answer = nvim.run(ExecKind::Lua, "error('\\255bad', 0)");
        assert!(answer.failed, "{answer:?}");
        assert_eq!(answer.text, "\u{fffd}bad");
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn a_chunk_past_its_deadline_runs_nothing() {
        let mut nvim = Nvim::start("deadline");
        // `vim.uv.sleep` holds the main loop, and what is scheduled here runs ahead of the next request. A
        // `getchar()` would hold nothing: nvim answers requests while one waits.
        nvim.embed.lua("vim.schedule(function() vim.uv.sleep(800) end)", vec![]);
        let stamp = Stamp {
            deadline_ms: wall_ms() + 200,
            scope: "exec".to_owned(),
            seq: 1,
        };
        let args = exec_args(&stamp, approved(ExecKind::Lua, "vim.g.ran = 1"));
        let reply = nvim.embed.lua(AGENT_EXEC_LUA, args);
        assert!(expired(&reply), "{reply:?}");
        assert_eq!(nvim.global("ran"), Value::from("unset"));
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn an_older_sequence_runs_nothing() {
        let mut nvim = Nvim::start("sequence");
        nvim.seq = 5;
        nvim.run(ExecKind::Lua, "vim.g.first = 1");
        nvim.seq = 3;
        let reply = nvim.reply(ExecKind::Lua, "vim.g.ran = 1");
        assert!(expired(&reply), "{reply:?}");
        assert_eq!(nvim.global("first"), Value::from(1));
        assert_eq!(nvim.global("ran"), Value::from("unset"));
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn the_arguments_are_not_spliced() {
        let mut nvim = Nvim::start("spliced");
        let lua = "vim.g.quoted = \"it's ]] \\\"here\\\"\"\nlocal long = [==[\nsecond ]] line]==]\nvim.g.long = long";
        let answer = nvim.run(ExecKind::Lua, lua);
        assert!(!answer.failed, "{answer:?}");
        assert_eq!(nvim.global("quoted"), Value::from("it's ]] \"here\""));
        assert_eq!(nvim.global("long"), Value::from("second ]] line"));

        let ex = "let g:first = \"a'b]]\"\nlet g:second = 'c\"d'";
        let answer = nvim.run(ExecKind::Ex, ex);
        assert!(!answer.failed, "{answer:?}");
        assert_eq!(nvim.global("first"), Value::from("a'b]]"));
        assert_eq!(nvim.global("second"), Value::from("c\"d"));
    }

    #[test]
    #[ignore = "needs nvim on PATH"]
    fn an_unknown_kind_runs_nothing() {
        let mut nvim = Nvim::start("kind");
        let stamp = Stamp {
            deadline_ms: wall_ms() + FAR_MS,
            scope: "exec".to_owned(),
            seq: 1,
        };
        let reply = nvim.embed.lua(
            AGENT_EXEC_LUA,
            vec![stamp.to_value(), the_args("vimscript", "let g:ran = 1")],
        );
        let answer = parse(&reply).expect("the chunk's answer");
        assert!(answer.failed);
        assert_eq!(answer.text, "unknown kind");
        assert_eq!(nvim.global("ran"), Value::from("unset"));
    }
}
