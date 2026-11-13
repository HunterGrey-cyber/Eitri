//! The scenario driver: runs a scenario's `driver` list against a host, waiting on ground truth.
//!
//! A driver part is a JSON object with exactly one verb key and optional `requires`, `name` and `timeout`
//! (seconds, before the deadline scale). Actions go to the [`Host`]; waits poll until the awaited thing shows up
//! or the deadline passes. The ground truth is read from the scripted endpoint (requests, holds), the stdio tap's
//! logs and the CLI's own transcript; only the `projection`, `interaction`, `roster` and `card` waits ask the host,
//! and a timeout on one of them is a failure naming what was missing, never a pass.
//!
//! | verb | argument |
//! |------|----------|
//! | `send` | text, or `{text, typed?, images?}`: `images` names files of `tests/fixtures/claude_scripted_images/`, sent with the text in that order (a host with the `turn_content_v1` capability) |
//! | `steer` | text, or `{text, typed}` |
//! | `queue` | text, or `{text, images?}`: the message waits for the running turn and goes as a turn of its own |
//! | `slash` | text |
//! | `interrupt`, `approve_sweep`, `close`, `resume` | anything |
//! | `answer_card` | `{tool, allow, origin?}` (allow defaults to true; `origin` is `hook` or `cli_prompt`: only a card of that origin takes it) |
//! | `open_gate` | gate name |
//! | `release` | hold name |
//! | `stop_task`, `task_output` | task id; `task_output` also `{task, refused?, min_bytes?, contains?}`: the host must refuse, with that text in the refusal, or the output it returns must be at least `min_bytes` long and hold `contains` |
//! | `set_per_task_stop` | bool |
//! | `client_answer` | `{text, is_error?, tool?}` or `{fill: {unit, bytes}, is_error?, tool?}`: answers the oldest open call of a tool this host declared (of `tool` when given) with that text; `fill` repeats `unit` up to `bytes` bytes |
//! | `freeze`, `adopt`, `commit` | anything: the three steps of a window move of the tab, in the source and then the destination window (a host with the `chat_move` capability) |
//! | `answer_question` | `{picks: [[option...]...], other?: [text\|null...], notes?: [text\|null...], refused?: text}` |
//! | `decline_question` | `{kind: "cancel"\|"chat"}`, with `picks`, `other` and `notes` as the answers so far (chat only) and `refused?` |
//! | `set_mode` | `manual`, `auto` or `bypass`: the person's Shift+Tab to that mode of the tab (not while plan mode holds it) |
//! | `set_planning` | `{enter: bool}`: the person's own switch into plan mode, or out of it (a host with `plan_mode_v1`) |
//! | `resolve_plan` | `{choice: "approve_manual"\|"approve_auto"\|"approve_bypass"\|"keep", feedback?, refused?}` |
//! | `request`, `request_done` | step id or side kind, or `{id, count}` |
//! | `held` | hold name |
//! | `tap_line` | `{kind, fields?, count?}`; `kind` is a stdout line's `type` or `type/subtype` |
//! | `cli_results` | how many `result` lines at least |
//! | `session_state` | `idle`, `running` or `requires_action`: the newest state line since the last action |
//! | `transcript_line` | `{type, fields?, count?, subagents?}` over the CLI's own session files; `subagents: true` includes nested agent transcripts |
//! | `projection` | `{question: "waiting"\|"settled"}`, `{plan: "waiting"\|"settled"}`, `{planning: "executing"\|"planning"}`, or an object the host judges |
//! | `interaction` | an object the host judges (`{plan_outcome, mode_after?, feedback?, count?}` for plans) |
//! | `roster` | `{task, status}` |
//! | `card` | `{tool}`: a request of that tool needed (or got) a driver answer |
//! | `wall_floor` | `{after, seconds}`: `after` is a part's `name` or `<verb>:<target>` of an earlier wait |
//! | `client_call` | tool name (bare, or with the `mcp__<server>__` prefix), or `{tool, count?}`: the host has been handed a call of that tool, with its `tool_use_id` |
//! | `list_models` | `{holds: value}`: the session's list of models has that row |
//! | `set_model` | `{model, applied?, refused?}`: a change of the running session's model, what it left, or the words of its refusal |
//! | `apply_effort` | `{effort}`: a change of the running session's effort, which must be what it left |
//! | `tab_mode` | layer (b): the tab's mode reaches that word |
//! | `client_state` | `{open: n, tool?}`: the session's own read of its open calls (`GetClientToolCallState`) holds exactly `n` (of `tool` when given) |
//!
//! A part may carry `layer: "a"|"b"`: it runs only on a host of that layer and is skipped, with no `PENDING` line,
//! on the other. Layer (a) is the sidecar provider driven directly; layer (b) is Eitri's own host.
//!
//! String values may hold `{name}` captures of the scripted requests and `{RUN}`.
//!
//! **Questions.** `answer_question` and `decline_question` answer the oldest question still waiting: the CLI's
//! `can_use_tool` request for `AskUserQuestion` on the tap's stdout that has no `control_response` on its stdin.
//! `picks` holds, per question in the CLI's order, the option indexes picked (from 0); `other` the typed text
//! and `notes` the notes, per question, `null` where there is none; a question left out is unanswered. Before the
//! host is called the driver checks the arguments against that request (an option or a question it does not have
//! fails the part) and checks that nothing has answered the question already: no card and no automatic answer
//! for `AskUserQuestion`, and no request at the stand-in carrying the question's `tool_result`. With `refused`
//! the part expects the host to refuse, with that text in the refusal, and checks the newest question instead
//! (a late answer to one that has gone). A host that does not declare `structured_questions_v1` gets the call
//! unchecked, and fails it as missing.
//!
//! **Plans.** `resolve_plan` decides the plan the CLI's `can_use_tool` request for `ExitPlanMode` waits on, as
//! `answer_question` does a question, and with the same checks before the host is called: that request has no
//! answer yet, the permission gate never answered it (no card, no automatic answer for `ExitPlanMode`), and no
//! request at the stand-in carries its `tool_result`. `refused` expects the host to refuse. `set_planning` moves
//! the session into plan mode or out of it. A host that does not declare `plan_mode_v1` gets the call unchecked
//! and fails it as missing. `projection{plan: waiting}` holds while the host shows the newest plan as waiting
//! for the person, `settled` once it shows it ended any way; `projection{planning: planning}` once the CLI has
//! reported plan mode (a move into it under way is not yet planning), `executing` once it has reported the way
//! out of it, or when plan mode was never entered.
//!
//! `projection{question: waiting}` holds while the host shows the newest question as waiting for the person;
//! `settled` once it shows it answered, declined, or ended by anything else. The host judges it, and an
//! `interaction` object (`outcome`, `answers`, `notes`, `count`), in [`Host::interaction_holds`].

use super::{
    gate_answer::{Answered, Origin},
    script::Expander,
    RequestRecord, ScriptedModel,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Why a host action did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverError {
    /// The host does not implement what the part needs; the name is the capability's.
    CapabilityMissing(&'static str),
    Failed(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapabilityMissing(name) => write!(f, "capability `{name}` is missing"),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for DriverError {}

/// A background task as the host's roster lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RosterEntry {
    pub task: String,
    /// The task's kind in the roster's words (`shell`, `monitor`, `local_agent`, `workflow`, `other`).
    pub kind: String,
    /// The task's status in the roster's words (`running`, `stopping`, `completed`, `stopped`, ...).
    pub status: String,
    /// Whether the task is ambient: listed, but never counted as work left running.
    pub ambient: bool,
}

/// How a pending question is declined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclineKind {
    Cancel,
    Chat,
}

/// The answer to one question of a set, as the person gives it: the indexes of the options picked, in the
/// CLI's option order, the text typed into "Type something", and notes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuestionAnswer {
    /// The question's place in the set, from 0.
    pub question: usize,
    pub options: Vec<usize>,
    pub other: Option<String>,
    pub notes: Option<String>,
}

/// The tool whose calls are questions for the person, never permission requests.
pub const QUESTION_TOOL: &str = "AskUserQuestion";

/// The tool whose calls are plans for the person, never permission requests.
pub const PLAN_TOOL: &str = "ExitPlanMode";

/// The capability a host declares when it can decide plans and switch plan mode.
pub const PLAN_MODE: &str = "plan_mode_v1";

/// What the person does with a waiting plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanDecision {
    ApproveManual,
    ApproveAuto,
    ApproveBypass,
    /// "Keep planning", with feedback for the model, possibly empty.
    Keep {
        feedback: String,
    },
}

/// A call of a tool this host declared, as the host was handed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCallSeen {
    pub call_id: String,
    pub tool_use_id: Option<String>,
    /// The declared name, without the `mcp__<server>__` prefix.
    pub tool: String,
}

/// What a `client_answer` part gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientAnswer {
    /// The declared name of the call to answer, or the oldest open call when `None`.
    pub tool: Option<String>,
    pub text: String,
    pub is_error: bool,
}

/// The session's read of its open calls.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ClientState {
    pub through_sequence: u64,
    pub open: Vec<ClientCallSeen>,
}

/// What a driver needs from the thing under test. Layer (a) implements it over the sidecar provider; the
/// companion plans implement it over Eitri's host.
///
/// A host names the optional capabilities it implements in [`capabilities`](Host::capabilities): the method-backed
/// names (`task_control`, `task_output`, `per_task_stop`, `background_tasks`, `human_origin`,
/// `structured_questions_v1`) once the session's handshake also advertises the Verdandi capability, and
/// `eitri_bash_ceiling`, which no method backs. The defaults below fail with `CapabilityMissing`.
pub trait Host {
    /// Sends a prompt. `typed` marks it as typed by the person; a host without `human_origin` fails it.
    fn send(&mut self, text: &str, typed: bool) -> Result<(), DriverError>;
    /// Sends a prompt with images, named as files of the image fixtures directory, in that order. A host that
    /// cannot carry them fails it as missing the capability.
    fn send_images(&mut self, text: &str, images: &[String], typed: bool) -> Result<(), DriverError> {
        let _ = (text, images, typed);
        Err(DriverError::CapabilityMissing("turn_content_v1"))
    }
    /// Enter while a turn may run, for a message with images: it waits for the running turn and goes as a turn
    /// of its own, never into it.
    fn queue_images(&mut self, text: &str, images: &[String]) -> Result<(), DriverError> {
        let _ = (text, images);
        Err(DriverError::CapabilityMissing("turn_content_v1"))
    }
    /// Puts a message into the running turn. `typed` marks it as typed by the person; a host without
    /// `human_origin` fails it.
    fn steer(&mut self, text: &str, typed: bool) -> Result<(), DriverError>;
    /// Sends a prompt while the CLI may be busy; the CLI queues it.
    fn queue(&mut self, text: &str) -> Result<(), DriverError>;
    fn interrupt(&mut self) -> Result<(), DriverError>;
    /// Answers the oldest waiting card of `tool`, or the next one that comes.
    fn answer_card(&mut self, tool: &str, allow: bool) -> Result<(), DriverError>;
    /// As [`answer_card`](Host::answer_card), for a card of one origin only: the hook's request or the CLI's own
    /// prompt for the call. A host that cannot tell them apart fails the part.
    fn answer_card_from(&mut self, tool: &str, allow: bool, origin: Origin) -> Result<(), DriverError> {
        let _ = (tool, allow, origin);
        Err(DriverError::Failed(
            "this host cannot answer a card by its origin".into(),
        ))
    }
    /// Approves every pending card ("approve all pending").
    fn approve_sweep(&mut self) -> Result<(), DriverError>;
    fn slash(&mut self, command: &str) -> Result<(), DriverError>;
    fn close(&mut self) -> Result<(), DriverError>;
    fn resume(&mut self) -> Result<(), DriverError>;

    /// The layer this host is: `a` drives the sidecar provider directly, `b` is Eitri's host.
    fn layer(&self) -> &'static str {
        "a"
    }

    /// Called on every poll of a wait: the host pumps its events and answers what its mode answers by itself.
    fn poll(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
    /// A few lines on the layer under test, for a timeout report.
    fn projection_summary(&self) -> String;
    /// Whether the projection holds what `want` describes.
    fn projection_holds(&self, want: &Value) -> Result<bool, DriverError>;
    /// Every request that needed a driver answer, answered or not, oldest first.
    fn cards_shown(&self) -> Vec<Answered>;
    /// Every request the mode answered without the driver.
    fn automatic_answers(&self) -> Vec<Answered>;
    /// The tool of the oldest card still waiting for a driver answer.
    fn unanswered_card(&self) -> Option<String> {
        self.cards_shown()
            .into_iter()
            .find(|c| c.decision.is_none())
            .map(|c| c.request.tool_name)
    }
    fn capabilities(&self) -> BTreeSet<&'static str> {
        BTreeSet::new()
    }

    fn stop_task(&mut self, task: &str) -> Result<(), DriverError> {
        let _ = task;
        Err(DriverError::CapabilityMissing("task_control"))
    }
    fn task_output(&mut self, task: &str) -> Result<String, DriverError> {
        let _ = task;
        Err(DriverError::CapabilityMissing("task_output"))
    }
    fn set_per_task_stop(&mut self, on: bool) -> Result<(), DriverError> {
        let _ = on;
        Err(DriverError::CapabilityMissing("per_task_stop"))
    }
    fn roster(&self) -> Result<Vec<RosterEntry>, DriverError> {
        Err(DriverError::CapabilityMissing("background_tasks"))
    }
    /// Answers the oldest waiting question with `answers`, one entry per answered question.
    fn answer_question(&mut self, answers: &[QuestionAnswer]) -> Result<(), DriverError> {
        let _ = answers;
        Err(DriverError::CapabilityMissing("structured_questions_v1"))
    }
    /// Declines the oldest waiting question; `so_far` are the answers given before a chat request.
    fn decline_question(&mut self, kind: DeclineKind, so_far: &[QuestionAnswer]) -> Result<(), DriverError> {
        let _ = (kind, so_far);
        Err(DriverError::CapabilityMissing("structured_questions_v1"))
    }
    /// Puts the tab in `mode` as the person's Shift+Tab does.
    fn set_mode(&mut self, mode: &str) -> Result<(), DriverError> {
        let _ = mode;
        Err(DriverError::CapabilityMissing(PLAN_MODE))
    }
    /// Moves the session into plan mode (`enter`) or out of it, as the person's own switch.
    fn set_planning(&mut self, enter: bool) -> Result<(), DriverError> {
        let _ = enter;
        Err(DriverError::CapabilityMissing(PLAN_MODE))
    }
    /// Decides the plan the model is waiting on.
    fn resolve_plan(&mut self, decision: &PlanDecision) -> Result<(), DriverError> {
        let _ = decision;
        Err(DriverError::CapabilityMissing(PLAN_MODE))
    }
    /// Whether the host's plan records hold what `want` describes: `{plan: "waiting"|"settled"}` for the newest
    /// plan, `{planning: "executing"|"planning"}` for the session's phase, or `{plan_outcome, mode_after?,
    /// feedback?, count?}` for the plans settled with that outcome.
    fn plan_holds(&self, want: &Value) -> Result<bool, DriverError> {
        let _ = want;
        Err(DriverError::CapabilityMissing(PLAN_MODE))
    }
    /// Every call of a declared tool the host has been handed, settled or not, oldest first.
    fn client_calls_seen(&self) -> Result<Vec<ClientCallSeen>, DriverError> {
        Err(DriverError::CapabilityMissing("client_tools"))
    }
    /// Answers the oldest open call of a declared tool, of `answer.tool` when it names one.
    fn client_answer(&mut self, answer: &ClientAnswer) -> Result<(), DriverError> {
        let _ = answer;
        Err(DriverError::CapabilityMissing("client_tools"))
    }
    /// The session's own read of its open calls.
    fn client_state(&mut self) -> Result<ClientState, DriverError> {
        Err(DriverError::CapabilityMissing("client_tools"))
    }
    /// The values of the models the session's CLI offers, in its order.
    fn list_models(&mut self) -> Result<Vec<String>, DriverError> {
        Err(DriverError::CapabilityMissing("direct_settings"))
    }
    /// Changes the running session's model; answers the model the CLI says it will use.
    fn set_model(&mut self, model: &str) -> Result<String, DriverError> {
        let _ = model;
        Err(DriverError::CapabilityMissing("direct_settings"))
    }
    /// Changes the running session's effort; answers the level the CLI says it will use.
    fn apply_effort(&mut self, effort: &str) -> Result<String, DriverError> {
        let _ = effort;
        Err(DriverError::CapabilityMissing("direct_settings"))
    }
    /// The tab's mode, at the layer that has a tab.
    fn tab_mode(&self) -> Option<String> {
        None
    }
    /// A window move of the tab, in three steps. `freeze` stops the source window's side and hands the session
    /// over, `adopt` takes it up in the destination window, and `commit` lets the destination read and answer.
    fn move_freeze(&mut self) -> Result<(), DriverError> {
        Err(DriverError::CapabilityMissing("chat_move"))
    }
    fn move_adopt(&mut self) -> Result<(), DriverError> {
        Err(DriverError::CapabilityMissing("chat_move"))
    }
    fn move_commit(&mut self) -> Result<(), DriverError> {
        Err(DriverError::CapabilityMissing("chat_move"))
    }
    /// Whether the host's question records hold what `want` describes (`question: waiting|settled`, or an
    /// `interaction` wait's object).
    fn interaction_holds(&self, want: &Value) -> Result<bool, DriverError> {
        let _ = want;
        Err(DriverError::CapabilityMissing("structured_questions_v1"))
    }
}

/// What the driver reads of the scripted endpoint.
pub trait Endpoint {
    fn records(&self) -> Vec<RequestRecord>;
    /// Arrival indexes of the requests whose answer is complete.
    fn finished(&self) -> BTreeSet<usize>;
    fn held(&self) -> Vec<String>;
    fn release(&self, name: &str);
    /// Values the scripted steps captured from requests.
    fn captures(&self) -> BTreeMap<String, String>;
}

impl Endpoint for ScriptedModel {
    fn records(&self) -> Vec<RequestRecord> {
        ScriptedModel::records(self)
    }
    fn finished(&self) -> BTreeSet<usize> {
        ScriptedModel::finished(self)
    }
    fn held(&self) -> Vec<String> {
        ScriptedModel::held(self)
    }
    fn release(&self, name: &str) {
        ScriptedModel::release(self, name);
    }
    fn captures(&self) -> BTreeMap<String, String> {
        ScriptedModel::captures(self)
    }
}

/// Timing and places of one driver run.
#[derive(Clone, Debug)]
pub struct Settings {
    pub scenario: String,
    /// `$RUN`: gates, the tap's logs under `log/` and the CLI's files under `claude/` are read from here.
    pub run: PathBuf,
    /// The deadline of one wait before the scale.
    pub base: Duration,
    /// `EITRI_HARNESS_DEADLINE_SCALE`.
    pub scale: f64,
    pub poll: Duration,
}

impl Settings {
    pub fn new(scenario: &str, run: &Path) -> Self {
        let scale = std::env::var("EITRI_HARNESS_DEADLINE_SCALE")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(1.0);
        Self {
            scenario: scenario.to_owned(),
            run: run.to_path_buf(),
            base: Duration::from_secs(60),
            scale,
            poll: Duration::from_millis(20),
        }
    }

    fn limit(&self, seconds: Option<f64>) -> Duration {
        Duration::from_secs_f64(seconds.unwrap_or(self.base.as_secs_f64()) * self.scale)
    }
}

/// What a finished driver run leaves.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// `PENDING <scenario>/<part>: needs <capability>` lines, one per part skipped for a missing capability.
    pub pending: Vec<String>,
    pub parts_run: usize,
}

const ACTIONS: &[&str] = &[
    "send",
    "steer",
    "queue",
    "interrupt",
    "answer_card",
    "approve_sweep",
    "slash",
    "close",
    "resume",
    "open_gate",
    "release",
    "stop_task",
    "task_output",
    "set_per_task_stop",
    "answer_question",
    "decline_question",
    "set_mode",
    "set_planning",
    "resolve_plan",
    "client_answer",
    "list_models",
    "set_model",
    "apply_effort",
    "freeze",
    "adopt",
    "commit",
];
const WAITS: &[&str] = &[
    "request",
    "request_done",
    "held",
    "tap_line",
    "cli_results",
    "session_state",
    "transcript_line",
    "projection",
    "interaction",
    "roster",
    "card",
    "wall_floor",
    "client_call",
    "client_state",
    "tab_mode",
];
const META: &[&str] = &["requires", "name", "timeout", "layer"];
/// Actions that start something the CLI answers: the session-state wait only looks at tap lines after the latest.
const STARTS_WORK: &[&str] = &["send", "steer", "queue", "interrupt", "slash", "resume"];

/// One line of a tap log.
#[derive(Clone, Debug)]
pub struct TapLine {
    /// The tap's arrival index. It starts again at 0 when a new CLI process (a resumed session) opens the log, so
    /// it orders lines only within one process; `position` orders the whole file.
    pub index: u64,
    /// The line's place in the log file, counting from 1.
    pub position: u64,
    pub time: f64,
    pub line: String,
    pub json: Option<Value>,
}

/// The lines of one tap log, in file order. A missing file is an empty log.
pub fn read_tap(path: &Path) -> Vec<TapLine> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .enumerate()
        .filter_map(|(n, l)| serde_json::from_str::<Value>(l).ok().map(|entry| (n as u64 + 1, entry)))
        .map(|(position, entry)| {
            let line = entry["line"].as_str().unwrap_or_default().to_owned();
            TapLine {
                index: entry["index"].as_u64().unwrap_or(0),
                position,
                time: entry["time"].as_f64().unwrap_or(0.0),
                json: serde_json::from_str(&line).ok(),
                line,
            }
        })
        .collect()
}

/// `value`'s field at a dotted `path` (object keys, array positions).
pub fn lookup<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    let mut cur = value;
    for key in path.split('.') {
        cur = match cur {
            Value::Object(map) => map.get(key)?,
            Value::Array(items) => items.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Whether every entry of `fields` (dotted path to wanted value) holds in `value`. A wanted value is compared
/// for equality, except `{"contains": text}` (the field's text holds `text`) and `{"exists": bool}`.
pub fn fields_match(value: &Value, fields: &Value) -> bool {
    let Some(map) = fields.as_object() else {
        return fields.is_null();
    };
    map.iter().all(|(path, want)| {
        let got = lookup(value, path);
        match want.as_object().filter(|o| o.len() == 1) {
            Some(o) if o.contains_key("contains") => {
                let needle = o["contains"].as_str().unwrap_or_default();
                got.is_some_and(|g| g.as_str().map_or_else(|| g.to_string(), str::to_owned).contains(needle))
            }
            Some(o) if o.contains_key("exists") => got.is_some() == o["exists"].as_bool().unwrap_or(true),
            _ => got == Some(want),
        }
    })
}

fn is_kind(line: &Value, kind: &str) -> bool {
    let ty = line["type"].as_str().unwrap_or_default();
    ty == kind
        || line["subtype"]
            .as_str()
            .is_some_and(|sub| format!("{ty}/{sub}") == kind)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn clip(text: &str, chars: usize) -> String {
    let mut out: String = text.chars().take(chars).collect();
    if text.chars().count() > chars {
        out.push_str("...");
    }
    out
}

/// What a `task_output` the host answered must hold when the scenario states it: at least `min_bytes` bytes and the
/// text `contains`. Only the length is reported, never the output.
fn check_task_output(task: &str, output: &str, arg: &Value) -> Result<(), Failure> {
    if let Some(min) = arg.get("min_bytes").and_then(Value::as_u64) {
        if (output.len() as u64) < min {
            return Err(Failure::text(format!(
                "`task_output` of {task} returned {} bytes, and the scenario expects at least {min}",
                output.len()
            )));
        }
    }
    if let Some(want) = arg.get("contains").and_then(Value::as_str) {
        if !output.contains(want) {
            return Err(Failure::text(format!(
                "`task_output` of {task} returned {} bytes that do not hold the text the scenario expects",
                output.len()
            )));
        }
    }
    Ok(())
}

fn text_arg(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => m.get(key).and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

/// Runs a scenario's driver list.
pub struct Driver<'a> {
    settings: Settings,
    endpoint: &'a dyn Endpoint,
    host: &'a mut dyn Host,
    /// The tap index at the start of the latest action that starts work.
    mark: u64,
    reached: BTreeMap<String, Instant>,
    transcripts: BTreeMap<PathBuf, (u64, Vec<Value>)>,
    #[cfg(test)]
    clock: std::cell::Cell<Instant>,
    #[cfg(test)]
    waits: std::cell::Cell<usize>,
}

impl<'a> Driver<'a> {
    pub fn new(settings: Settings, endpoint: &'a dyn Endpoint, host: &'a mut dyn Host) -> Self {
        Self {
            settings,
            endpoint,
            host,
            mark: 0,
            reached: BTreeMap::new(),
            transcripts: BTreeMap::new(),
            #[cfg(test)]
            clock: std::cell::Cell::new(Instant::now()),
            #[cfg(test)]
            waits: std::cell::Cell::new(0),
        }
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        {
            self.clock.get()
        }
        #[cfg(not(test))]
        {
            Instant::now()
        }
    }

    fn pause(&self) {
        #[cfg(test)]
        {
            self.waits.set(self.waits.get() + 1);
            self.clock.set(self.clock.get() + self.settings.poll);
        }
        #[cfg(not(test))]
        std::thread::sleep(self.settings.poll);
    }

    /// Runs every part in order. The first failure ends the run with the part's label and the reason.
    pub fn run(&mut self, parts: &[Value]) -> Result<Report, String> {
        let mut report = Report::default();
        for (i, part) in parts.iter().enumerate() {
            let (verb, label) = self.describe(i, part)?;
            if part["layer"].as_str().is_some_and(|layer| layer != self.host.layer()) {
                continue;
            }
            let missing = self.missing(part);
            if !missing.is_empty() {
                for name in missing {
                    let line = format!("PENDING {}/{label}: needs {name}", self.settings.scenario);
                    println!("{line}");
                    report.pending.push(line);
                }
                continue;
            }
            let captures = self.endpoint.captures();
            let part = Expander::new(Some(&self.settings.run), &captures, now_secs()).value(part);
            self.run_part(verb, &label, &part)
                .map_err(|why| format!("driver part {label}: {why}"))?;
            report.parts_run += 1;
        }
        Ok(report)
    }

    /// The part's verb and its label (`name`, else `<index>-<verb>`).
    fn describe(&self, index: usize, part: &Value) -> Result<(&'static str, String), String> {
        let object = part
            .as_object()
            .ok_or_else(|| format!("driver part #{index} is not an object"))?;
        let mut verbs = Vec::new();
        for key in object.keys() {
            if let Some(v) = ACTIONS.iter().chain(WAITS.iter()).copied().find(|v| *v == key.as_str()) {
                verbs.push(v);
            } else if !META.contains(&key.as_str()) {
                return Err(format!("driver part #{index} has an unknown key `{key}`"));
            }
        }
        let [verb] = verbs[..] else {
            return Err(format!(
                "driver part #{index} needs exactly one verb, has {}",
                if verbs.is_empty() {
                    "none".to_owned()
                } else {
                    verbs.join(", ")
                }
            ));
        };
        let label = object
            .get("name")
            .and_then(Value::as_str)
            .map_or_else(|| format!("{index}-{verb}"), str::to_owned);
        Ok((verb, label))
    }

    /// The `requires` names the host does not declare.
    fn missing(&self, part: &Value) -> Vec<String> {
        let wanted: Vec<String> = match &part["requires"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(items) => items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
            _ => Vec::new(),
        };
        let have = self.host.capabilities();
        wanted
            .into_iter()
            .filter(|name| !have.contains(name.as_str()))
            .collect()
    }

    fn run_part(&mut self, verb: &'static str, label: &str, part: &Value) -> Result<(), String> {
        let arg = &part[verb];
        let declared = self.host.capabilities();
        let limit = self.settings.limit(part["timeout"].as_f64());
        let before = self.tap_end();
        let result = if ACTIONS.contains(&verb) {
            let done = self.action(verb, arg);
            if STARTS_WORK.contains(&verb) {
                self.mark = before;
            }
            done
        } else {
            self.wait(verb, arg, limit, part)
        };
        match result {
            Ok(()) => {
                let now = self.now();
                self.reached.insert(label.to_owned(), now);
                if let Some(target) = target_of(verb, arg) {
                    self.reached.insert(format!("{verb}:{target}"), now);
                }
                Ok(())
            }
            Err(Failure::Host(DriverError::CapabilityMissing(name))) => {
                let required = self.missing_in(part, name);
                Err(if declared.contains(name) {
                    format!("the host declares capability `{name}` but does not implement what `{verb}` needs")
                } else if required {
                    format!("capability `{name}` is missing")
                } else {
                    format!("`{verb}` needs capability `{name}`: add it to the part's `requires`")
                })
            }
            Err(Failure::Host(DriverError::Failed(why))) | Err(Failure::Text(why)) => Err(why),
        }
    }

    fn missing_in(&self, part: &Value, name: &str) -> bool {
        match &part["requires"] {
            Value::String(s) => s == name,
            Value::Array(items) => items.iter().any(|v| v.as_str() == Some(name)),
            _ => false,
        }
    }

    // ---- actions ----

    fn action(&mut self, verb: &str, arg: &Value) -> Result<(), Failure> {
        let need = |key: &str| text_arg(arg, key).ok_or_else(|| Failure::text(format!("`{verb}` needs a text")));
        match verb {
            "send" => {
                let typed = arg["typed"].as_bool().unwrap_or(false);
                let images = super::images::names_in(arg).map_err(Failure::text)?;
                if images.is_empty() {
                    Ok(self.host.send(&need("text")?, typed)?)
                } else {
                    Ok(self.host.send_images(&need("text")?, &images, typed)?)
                }
            }
            "steer" => {
                let typed = arg["typed"].as_bool().unwrap_or(false);
                Ok(self.host.steer(&need("text")?, typed)?)
            }
            "queue" => {
                let images = super::images::names_in(arg).map_err(Failure::text)?;
                if images.is_empty() {
                    Ok(self.host.queue(&need("text")?)?)
                } else {
                    Ok(self.host.queue_images(&need("text")?, &images)?)
                }
            }
            "slash" => Ok(self.host.slash(&need("command")?)?),
            "interrupt" => Ok(self.host.interrupt()?),
            "approve_sweep" => Ok(self.host.approve_sweep()?),
            "close" => Ok(self.host.close()?),
            "resume" => Ok(self.host.resume()?),
            "answer_card" => {
                let tool = text_arg(arg, "tool").ok_or_else(|| Failure::text("`answer_card` needs a tool"))?;
                let allow = arg["allow"].as_bool().unwrap_or(true);
                match arg["origin"].as_str() {
                    None if arg.get("origin").is_none() => Ok(self.host.answer_card(&tool, allow)?),
                    Some("hook") => Ok(self.host.answer_card_from(&tool, allow, Origin::Hook)?),
                    Some("cli_prompt") => Ok(self.host.answer_card_from(&tool, allow, Origin::CliPrompt)?),
                    _ => Err(Failure::text("`answer_card` origin is `hook` or `cli_prompt`")),
                }
            }
            "open_gate" => {
                let name = need("name")?;
                if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                    return Err(Failure::text(format!("gate name `{name}` is not [A-Za-z0-9_-]+")));
                }
                let path = self.settings.run.join("gates").join(format!("{name}.open"));
                std::fs::write(&path, b"").map_err(|e| Failure::text(format!("cannot open gate {name}: {e}")))
            }
            "release" => {
                self.endpoint.release(&need("name")?);
                Ok(())
            }
            "stop_task" => Ok(self.host.stop_task(&need("task")?)?),
            "task_output" => {
                // `refused` names the text the host's refusal holds: a task whose kind has no output to read.
                let task = need("task")?;
                let refused = arg.get("refused").and_then(Value::as_str);
                match (self.host.task_output(&task), refused) {
                    (Ok(output), None) => check_task_output(&task, &output, arg),
                    (Ok(_), Some(want)) => Err(Failure::text(format!(
                        "`task_output` was taken, and the scenario expects a refusal holding `{want}`"
                    ))),
                    (Err(DriverError::Failed(why)), Some(want)) if why.contains(want) => Ok(()),
                    (Err(DriverError::Failed(why)), Some(want)) => Err(Failure::text(format!(
                        "`task_output` was refused with `{why}`, which does not hold `{want}`"
                    ))),
                    (Err(e), _) => Err(e.into()),
                }
            }
            "set_per_task_stop" => {
                let on = arg
                    .as_bool()
                    .ok_or_else(|| Failure::text("`set_per_task_stop` needs true or false"))?;
                Ok(self.host.set_per_task_stop(on)?)
            }
            "answer_question" | "decline_question" => self.question_action(verb, arg),
            "set_mode" => match arg.as_str() {
                Some(mode @ ("manual" | "auto" | "bypass")) => Ok(self.host.set_mode(mode)?),
                _ => Err(Failure::text("`set_mode` is `manual`, `auto` or `bypass`")),
            },
            "set_planning" => {
                let enter = arg["enter"]
                    .as_bool()
                    .ok_or_else(|| Failure::text("`set_planning` needs `enter`: true or false"))?;
                if arg.as_object().is_some_and(|o| o.len() != 1) {
                    return Err(Failure::text("`set_planning` takes only `enter`"));
                }
                Ok(self.host.set_planning(enter)?)
            }
            "resolve_plan" => self.plan_action(arg),
            "client_answer" => {
                let answer = client_answer_of(arg)?;
                Ok(self.host.client_answer(&answer)?)
            }
            "list_models" => {
                let want = need("holds")?;
                let models = self.host.list_models()?;
                if models.contains(&want) {
                    Ok(())
                } else {
                    Err(Failure::text(format!(
                        "`list_models` gave {models:?}, without `{want}`"
                    )))
                }
            }
            "set_model" => {
                let model = need("model")?;
                match (self.host.set_model(&model), arg.get("refused").and_then(Value::as_str)) {
                    (Ok(applied), None) => match arg.get("applied").and_then(Value::as_str) {
                        Some(want) if want != applied => Err(Failure::text(format!(
                            "`set_model` left `{applied}`, and the scenario expects `{want}`"
                        ))),
                        _ => Ok(()),
                    },
                    (Ok(_), Some(want)) => Err(Failure::text(format!(
                        "`set_model` was taken, and the scenario expects a refusal holding `{want}`"
                    ))),
                    (Err(DriverError::Failed(why)), Some(want)) if why.contains(want) => Ok(()),
                    (Err(DriverError::Failed(why)), Some(want)) => Err(Failure::text(format!(
                        "`set_model` was refused with `{why}`, which does not hold `{want}`"
                    ))),
                    (Err(e), _) => Err(e.into()),
                }
            }
            "apply_effort" => {
                let effort = need("effort")?;
                let applied = self.host.apply_effort(&effort)?;
                if applied == effort {
                    Ok(())
                } else {
                    Err(Failure::text(format!(
                        "`apply_effort` left `{applied}`, not `{effort}`"
                    )))
                }
            }
            "freeze" => Ok(self.host.move_freeze()?),
            "adopt" => Ok(self.host.move_adopt()?),
            "commit" => Ok(self.host.move_commit()?),
            other => Err(Failure::text(format!("no action `{other}`"))),
        }
    }

    /// `answer_question` and `decline_question`: the arguments read and checked against the question they answer,
    /// the question checked as untouched, then the host called.
    fn question_action(&mut self, verb: &str, arg: &Value) -> Result<(), Failure> {
        let QuestionAction {
            decline,
            refused,
            answers,
        } = question_action_of(verb, arg)?;
        let call = |host: &mut dyn Host| match decline {
            None => host.answer_question(&answers),
            Some(kind) => host.decline_question(kind, &answers),
        };
        if !self.host.capabilities().contains("structured_questions_v1") {
            return Ok(call(&mut *self.host)?);
        }
        let asked = self.call_to_answer(QUESTION_TOOL, refused.is_some())?;
        check_against(verb, &answers, &asked)?;
        if refused.is_none() {
            self.check_untouched(QUESTION_TOOL, &asked)?;
        }
        match (call(&mut *self.host), refused) {
            (Ok(()), None) => Ok(()),
            (Ok(()), Some(want)) => Err(Failure::text(format!(
                "`{verb}` was taken, and the scenario expects a refusal holding `{want}`"
            ))),
            (Err(DriverError::Failed(why)), Some(want)) if why.contains(&want) => Ok(()),
            (Err(DriverError::Failed(why)), Some(want)) => Err(Failure::text(format!(
                "`{verb}` was refused with `{why}`, which does not hold `{want}`"
            ))),
            (Err(e), _) => Err(e.into()),
        }
    }

    /// `resolve_plan`: the arguments read, the plan checked as untouched, then the host called.
    fn plan_action(&mut self, arg: &Value) -> Result<(), Failure> {
        let (decision, refused) = plan_action_of(arg)?;
        if !self.host.capabilities().contains(PLAN_MODE) {
            return Ok(self.host.resolve_plan(&decision)?);
        }
        let asked = self.call_to_answer(PLAN_TOOL, refused.is_some())?;
        if refused.is_none() {
            self.check_untouched(PLAN_TOOL, &asked)?;
        }
        match (self.host.resolve_plan(&decision), refused) {
            (Ok(()), None) => Ok(()),
            (Ok(()), Some(want)) => Err(Failure::text(format!(
                "`resolve_plan` was taken, and the scenario expects a refusal holding `{want}`"
            ))),
            (Err(DriverError::Failed(why)), Some(want)) if why.contains(&want) => Ok(()),
            (Err(DriverError::Failed(why)), Some(want)) => Err(Failure::text(format!(
                "`resolve_plan` was refused with `{why}`, which does not hold `{want}`"
            ))),
            (Err(e), _) => Err(e.into()),
        }
    }

    /// The call of `tool` an answer goes to: the oldest one still waiting, or for a late answer the newest one.
    fn call_to_answer(&self, tool: &str, late: bool) -> Result<AskedQuestion, Failure> {
        let asked = asked_calls(&self.settings.run.join("log"), tool);
        let found = if late {
            asked.into_iter().next_back()
        } else {
            asked.into_iter().find(|q| !q.settled)
        };
        found.ok_or_else(|| {
            Failure::text(if late {
                format!("the CLI has made no {tool} request to answer late")
            } else {
                format!("no {tool} request of the CLI waits for an answer{}", self.report_tail())
            })
        })
    }

    /// Nothing but the person answers a question or a plan: no card and no automatic answer for its tool, and no
    /// request at the stand-in carries the call's result yet.
    fn check_untouched(&self, tool: &str, asked: &AskedQuestion) -> Result<(), Failure> {
        let (cards, automatic) = (self.host.cards_shown(), self.host.automatic_answers());
        let touched: Vec<Value> = cards
            .iter()
            .map(|a| ("card", a))
            .chain(automatic.iter().map(|a| ("automatic answer", a)))
            .filter(|(_, a)| a.tool() == tool)
            .map(|(what, a)| json!({"as": what, "answer": a.to_json()}))
            .collect();
        if !touched.is_empty() {
            return Err(Failure::text(format!(
                "the {tool} call went through the permission gate: {}",
                Value::Array(touched)
            )));
        }
        let standin = self.settings.run.join("standin");
        for record in self.endpoint.records() {
            let Ok(text) = std::fs::read_to_string(standin.join(&record.body_file)) else {
                continue;
            };
            let Ok(body) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let ids = tool_results_in(&body, tool);
            let hit = match &asked.tool_use_id {
                Some(id) => ids.contains(id),
                // Without the call's id, every result for a question has to belong to one the CLI got an answer
                // for before this one was asked.
                None => ids.len() > asked.answered_before,
            };
            if hit {
                return Err(Failure::text(format!(
                    "request #{} reached the stand-in with the {tool} call's tool_result before the driver answered",
                    record.arrival_index
                )));
            }
        }
        Ok(())
    }

    // ---- waits ----

    fn wait(&mut self, verb: &str, arg: &Value, limit: Duration, part: &Value) -> Result<(), Failure> {
        let what = format!("{verb} {}", clip(&arg.to_string(), 200));
        let run = self.settings.run.clone();
        let stdout = run.join("log/cli-stdout.jsonl");
        let want_count = |arg: &Value| arg["count"].as_u64().unwrap_or(1) as usize;
        match verb {
            "request" | "request_done" => {
                let id = text_arg(arg, "id")
                    .ok_or_else(|| Failure::text(format!("`{verb}` needs a step id or side kind")))?;
                let (count, done_only) = (want_count(arg), verb == "request_done");
                self.wait_for(&what, limit, false, move |d| {
                    let finished = d.endpoint.finished();
                    let n = d
                        .endpoint
                        .records()
                        .iter()
                        .filter(|r| r.matched_step.as_deref() == Some(&id) || r.side_kind.as_deref() == Some(&id))
                        .filter(|r| !done_only || finished.contains(&r.arrival_index))
                        .count();
                    Ok(n >= count)
                })
            }
            "held" => {
                let name = text_arg(arg, "name").ok_or_else(|| Failure::text("`held` needs a hold name"))?;
                self.wait_for(&what, limit, false, move |d| Ok(d.endpoint.held().contains(&name)))
            }
            "tap_line" => {
                let kind = text_arg(arg, "kind").ok_or_else(|| Failure::text("`tap_line` needs a kind"))?;
                let (fields, count) = (arg["fields"].clone(), want_count(arg));
                self.wait_for(&what, limit, false, move |_| {
                    let n = read_tap(&stdout)
                        .iter()
                        .filter_map(|l| l.json.as_ref())
                        .filter(|j| is_kind(j, &kind) && fields_match(j, &fields))
                        .count();
                    Ok(n >= count)
                })
            }
            "cli_results" => {
                let n = arg
                    .as_u64()
                    .ok_or_else(|| Failure::text("`cli_results` needs a count"))? as usize;
                self.wait_for(&what, limit, false, move |_| {
                    let got = read_tap(&stdout)
                        .iter()
                        .filter_map(|l| l.json.as_ref())
                        .filter(|j| is_kind(j, "result"))
                        .count();
                    Ok(got >= n)
                })
            }
            "session_state" => {
                let want = arg
                    .as_str()
                    .ok_or_else(|| Failure::text("`session_state` needs a state word"))?
                    .to_owned();
                self.wait_for(&what, limit, false, move |d| {
                    let latest = read_tap(&stdout)
                        .into_iter()
                        .filter(|l| l.position > d.mark)
                        .filter_map(|l| l.json)
                        .rfind(|j| is_kind(j, "system/session_state_changed"));
                    Ok(latest.is_some_and(|j| j["state"].as_str() == Some(&want)))
                })
            }
            "transcript_line" => {
                let ty = text_arg(arg, "type").ok_or_else(|| Failure::text("`transcript_line` needs a type"))?;
                let (fields, count) = (arg["fields"].clone(), want_count(arg));
                let subagents = arg["subagents"].as_bool().unwrap_or(false);
                self.wait_for(&what, limit, false, move |d| {
                    let n = d
                        .transcript_lines(subagents)
                        .iter()
                        .filter(|j| j["type"].as_str() == Some(&ty) && fields_match(j, &fields))
                        .count();
                    Ok(n >= count)
                })
            }
            "projection" if arg.get("question").is_some() => {
                let word = arg["question"].as_str().unwrap_or_default();
                if !["waiting", "settled"].contains(&word) || arg.as_object().is_some_and(|o| o.len() != 1) {
                    return Err(Failure::text(
                        "`projection` on a question is exactly `{\"question\": \"waiting\"}` or `{\"question\": \"settled\"}`",
                    ));
                }
                let want = json!({"question": word});
                self.wait_for(&what, limit, false, move |d| {
                    d.host.interaction_holds(&want).map_err(Failure::from)
                })
            }
            "projection" if arg.get("plan").is_some() || arg.get("planning").is_some() => {
                let (key, word) = match (arg["plan"].as_str(), arg["planning"].as_str()) {
                    (Some(word @ ("waiting" | "settled")), None) => ("plan", word),
                    (None, Some(word @ ("executing" | "planning"))) => ("planning", word),
                    _ => {
                        return Err(Failure::text(
                            "`projection` on plans is exactly `{\"plan\": \"waiting\"|\"settled\"}` or \
                             `{\"planning\": \"executing\"|\"planning\"}`",
                        ));
                    }
                };
                if arg.as_object().is_some_and(|o| o.len() != 1) {
                    return Err(Failure::text("`projection` on plans takes one key"));
                }
                let want = json!({ key: word });
                self.wait_for(&what, limit, false, move |d| {
                    d.host.plan_holds(&want).map_err(Failure::from)
                })
            }
            "projection" => {
                let want = arg.clone();
                self.wait_for(&what, limit, false, move |d| {
                    d.host.projection_holds(&want).map_err(Failure::from)
                })
            }
            "interaction" if arg.get("plan_outcome").is_some() => {
                let want = arg.clone();
                self.wait_for(&what, limit, false, move |d| {
                    d.host.plan_holds(&want).map_err(Failure::from)
                })
            }
            "interaction" => {
                let want = arg.clone();
                self.wait_for(&what, limit, false, move |d| {
                    d.host.interaction_holds(&want).map_err(Failure::from)
                })
            }
            "roster" => {
                let task = text_arg(arg, "task").ok_or_else(|| Failure::text("`roster` needs a task"))?;
                let status = arg["status"].as_str().map(str::to_owned);
                self.wait_for(&what, limit, false, move |d| {
                    Ok(d.host
                        .roster()?
                        .iter()
                        .any(|e| e.task == task && status.as_deref().is_none_or(|s| e.status == s)))
                })
            }
            "card" => {
                let tool = text_arg(arg, "tool").ok_or_else(|| Failure::text("`card` needs a tool"))?;
                self.wait_for(&what, limit, true, move |d| {
                    Ok(d.host.cards_shown().iter().any(|c| c.tool() == tool))
                })
            }
            "client_call" => {
                let tool =
                    bare_tool(&text_arg(arg, "tool").ok_or_else(|| Failure::text("`client_call` needs a tool"))?);
                let count = want_count(arg);
                self.wait_for(&what, limit, false, move |d| {
                    let seen = d.host.client_calls_seen()?;
                    Ok(seen.iter().filter(|c| c.tool == tool).count() >= count)
                })
            }
            "client_state" => {
                let open = arg["open"]
                    .as_u64()
                    .ok_or_else(|| Failure::text("`client_state` needs `open`: how many calls the read holds"))?
                    as usize;
                let tool = text_arg(arg, "tool").map(|t| bare_tool(&t));
                self.wait_for(&what, limit, false, move |d| {
                    let state = d.host.client_state()?;
                    let n = state
                        .open
                        .iter()
                        .filter(|c| tool.as_deref().is_none_or(|t| c.tool == t))
                        .count();
                    Ok(n == open)
                })
            }
            "tab_mode" => {
                let want = arg
                    .as_str()
                    .ok_or_else(|| Failure::text("`tab_mode` needs the mode"))?
                    .to_owned();
                self.wait_for(&what, limit, true, move |d| {
                    Ok(d.host.tab_mode().as_deref() == Some(&want))
                })
            }
            "wall_floor" => {
                let after = text_arg(arg, "after").ok_or_else(|| Failure::text("`wall_floor` needs `after`"))?;
                let seconds = arg["seconds"]
                    .as_f64()
                    .ok_or_else(|| Failure::text("`wall_floor` needs `seconds`"))?;
                let Some(start) = self.reached.get(&after).copied() else {
                    return Err(Failure::text(format!(
                        "`wall_floor` follows `{after}`, which no earlier part reached"
                    )));
                };
                let limit = Duration::from_secs_f64(seconds.max(0.0)) + self.settings.limit(part["timeout"].as_f64());
                self.wait_for(&what, limit, false, move |d| {
                    Ok(d.now().duration_since(start).as_secs_f64() >= seconds)
                })
            }
            other => Err(Failure::text(format!("no wait `{other}`"))),
        }
    }

    /// Polls `check` until it holds. Fails at once, even when `check` would hold, on an unmatched stand-in request, on
    /// the host's own failure and (unless `card_wait`) on a card nobody answers; fails at the deadline with the full
    /// report.
    fn wait_for<F>(&mut self, what: &str, limit: Duration, card_wait: bool, mut check: F) -> Result<(), Failure>
    where
        F: FnMut(&mut Self) -> Result<bool, Failure>,
    {
        let end = self.now() + limit;
        loop {
            self.host.poll()?;
            // The failure checks come first: a condition that became true with an unmatched request or an
            // unanswered card behind it does not make the part a success.
            if let Some(r) = self.endpoint.records().into_iter().find(|r| r.unmatched.is_some()) {
                return Err(Failure::text(format!(
                    "while waiting for {what}: the stand-in could not match request #{} {} ({}){}",
                    r.arrival_index,
                    r.path,
                    r.unmatched.unwrap_or_default(),
                    self.report_tail()
                )));
            }
            if !card_wait {
                if let Some(tool) = self.host.unanswered_card() {
                    return Err(Failure::text(format!(
                        "while waiting for {what}: a permission request for {tool} waits for a driver answer and the scenario gives none{}",
                        self.report_tail()
                    )));
                }
            }
            if check(self)? {
                return Ok(());
            }
            if self.now() >= end {
                return Err(Failure::text(format!(
                    "timed out after {:.1}s waiting for {what}{}",
                    limit.as_secs_f64(),
                    self.report_tail()
                )));
            }
            self.pause();
        }
    }

    /// The last 20 tap lines, the stand-in's last requests and the host's summary.
    fn report_tail(&self) -> String {
        let log = self.settings.run.join("log");
        let mut lines: Vec<(u64, &str, TapLine)> = Vec::new();
        for (name, tag) in [("cli-stdin.jsonl", "stdin"), ("cli-stdout.jsonl", "stdout")] {
            lines.extend(read_tap(&log.join(name)).into_iter().map(|l| (l.index, tag, l)));
        }
        lines.sort_by_key(|(index, ..)| *index);
        let skip = lines.len().saturating_sub(20);
        let mut out = String::from("\nlast 20 tap lines:");
        if lines.is_empty() {
            out.push_str(" (none)");
        }
        for (index, tag, line) in lines.into_iter().skip(skip) {
            out.push_str(&format!(
                "\n  #{index} {tag} {:.2}s {}",
                line.time,
                clip(&line.line, 300)
            ));
        }
        let finished = self.endpoint.finished();
        let records = self.endpoint.records();
        out.push_str("\nthe stand-in's last requests:");
        if records.is_empty() {
            out.push_str(" (none)");
        }
        for r in records.iter().rev().take(6).rev() {
            out.push_str(&format!(
                "\n  #{} {} step={} side={} {}{}",
                r.arrival_index,
                r.path,
                r.matched_step.as_deref().unwrap_or("-"),
                r.side_kind.as_deref().unwrap_or("-"),
                if finished.contains(&r.arrival_index) {
                    "done"
                } else {
                    "open"
                },
                r.unmatched
                    .as_ref()
                    .map(|u| format!(" UNMATCHED: {u}"))
                    .unwrap_or_default()
            ));
        }
        out.push_str("\nprojection: ");
        out.push_str(&self.host.projection_summary());
        out
    }

    fn tap_end(&self) -> u64 {
        read_tap(&self.settings.run.join("log/cli-stdout.jsonl"))
            .iter()
            .map(|l| l.position)
            .max()
            .unwrap_or(0)
    }

    /// Every parsed line of the CLI's session files `claude/projects/<slug>/<session>.jsonl`, re-read only when a
    /// file grew. Nested agent transcripts are included only when the wait explicitly asks for them.
    fn transcript_lines(&mut self, subagents: bool) -> Vec<Value> {
        fn collect(dir: &Path, nested: bool, files: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let Ok(kind) = entry.file_type() else { continue };
                if nested && kind.is_dir() {
                    collect(&entry.path(), nested, files);
                } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "jsonl") {
                    files.push(entry.path());
                }
            }
        }
        let root = self.settings.run.join("claude/projects");
        let mut files = Vec::new();
        for slug in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            collect(&slug.path(), subagents, &mut files);
        }
        files.sort();
        let mut all = Vec::new();
        for path in files {
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let fresh = self.transcripts.get(&path).is_some_and(|(seen, _)| *seen == len);
            if !fresh {
                let lines = std::fs::read_to_string(&path)
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                    .collect();
                self.transcripts.insert(path.clone(), (len, lines));
            }
            all.extend(self.transcripts[&path].1.iter().cloned());
        }
        all
    }
}

/// One `AskUserQuestion` request the CLI made through its control channel.
#[derive(Clone, Debug)]
struct AskedQuestion {
    tool_use_id: Option<String>,
    /// The `questions` of the call's input, in the CLI's order.
    questions: Vec<Value>,
    answered: bool,
    /// Nothing more can answer it: it was answered, the CLI withdrew it (`control_cancel_request`), or the
    /// process that asked it has been replaced by another (a resumed session).
    settled: bool,
    /// How many earlier question requests had an answer by the time this one came.
    answered_before: usize,
}

/// Every `can_use_tool` request for `tool` on the tap's stdout, oldest first, each marked answered when
/// a `control_response` with its request id went to the CLI's stdin, and settled when it was answered, the CLI
/// withdrew it, or a later process started: the tap's index is one counter across both directions and every
/// process of the run, so a question whose index is below a later `initialize` request on stdin was asked by a
/// process that is gone.
fn asked_calls(log: &Path, tool: &str) -> Vec<AskedQuestion> {
    let stdin: Vec<(u64, Value)> = read_tap(&log.join("cli-stdin.jsonl"))
        .into_iter()
        .filter_map(|l| l.json.map(|j| (l.index, j)))
        .collect();
    let responses: BTreeSet<String> = stdin
        .iter()
        .filter(|(_, j)| j["type"] == "control_response")
        .filter_map(|(_, j)| j["response"]["request_id"].as_str().map(str::to_owned))
        .collect();
    let last_start = stdin
        .iter()
        .filter(|(_, j)| j["type"] == "control_request" && j["request"]["subtype"] == "initialize")
        .map(|(index, _)| *index)
        .max();
    let stdout = read_tap(&log.join("cli-stdout.jsonl"));
    let withdrawn: BTreeSet<String> = stdout
        .iter()
        .filter_map(|l| l.json.as_ref())
        .filter(|j| j["type"] == "control_cancel_request")
        .filter_map(|j| j["request_id"].as_str().map(str::to_owned))
        .collect();
    let mut out: Vec<AskedQuestion> = Vec::new();
    for line in stdout {
        let index = line.index;
        let Some(j) = line.json else { continue };
        let request = &j["request"];
        if j["type"] != "control_request" || request["subtype"] != "can_use_tool" || request["tool_name"] != tool {
            continue;
        }
        let id = j["request_id"].as_str().unwrap_or_default();
        let answered_before = out.iter().filter(|q| q.answered).count();
        let answered = responses.contains(id);
        let gone = last_start.is_some_and(|start| start > index);
        out.push(AskedQuestion {
            tool_use_id: request["tool_use_id"].as_str().map(str::to_owned),
            questions: request["input"]["questions"].as_array().cloned().unwrap_or_default(),
            answered,
            settled: answered || withdrawn.contains(id) || gone,
            answered_before,
        });
    }
    out
}

fn asked_questions(log: &Path) -> Vec<AskedQuestion> {
    asked_calls(log, QUESTION_TOOL)
}

/// Whether a plan the CLI asked through its control channel (`ExitPlanMode`'s `can_use_tool`) still waits for
/// its answer, by the tap logs in `log`.
pub fn plan_waits(log: &Path) -> bool {
    asked_calls(log, PLAN_TOOL).iter().any(|q| !q.settled)
}

/// Whether a question the CLI asked through its control channel still waits for its answer, by the tap logs in
/// `log`: a `can_use_tool` request for the question tool of the live process, neither answered nor withdrawn.
pub fn question_waits(log: &Path) -> bool {
    asked_questions(log).iter().any(|q| !q.settled)
}

/// The ids of `tool`'s calls whose `tool_result` a request body carries.
fn tool_results_in(body: &Value, tool: &str) -> Vec<String> {
    let blocks = || {
        body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
    };
    let calls: BTreeSet<&str> = blocks()
        .filter(|b| b["type"] == "tool_use" && b["name"] == tool)
        .filter_map(|b| b["id"].as_str())
        .collect();
    blocks()
        .filter(|b| b["type"] == "tool_result")
        .filter_map(|b| b["tool_use_id"].as_str())
        .filter(|id| calls.contains(id))
        .map(str::to_owned)
        .collect()
}

/// `picks`, `other` and `notes` as one answer per question that has any of them. Only the shape is checked here.
fn answers_of(verb: &str, arg: &Value) -> Result<Vec<QuestionAnswer>, Failure> {
    let list = |key: &str| -> Result<Vec<Value>, Failure> {
        match &arg[key] {
            Value::Null => Ok(Vec::new()),
            Value::Array(items) => Ok(items.clone()),
            _ => Err(Failure::text(format!(
                "`{verb}` `{key}` is a list with one entry per question"
            ))),
        }
    };
    let picks = list("picks")?
        .iter()
        .enumerate()
        .map(|(q, p)| {
            p.as_array()
                .and_then(|l| {
                    l.iter()
                        .map(|v| v.as_u64().map(|i| i as usize))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| {
                    Failure::text(format!(
                        "`{verb}` picks for question {q} is not a list of option indexes"
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let texts = |key: &str| -> Result<Vec<Option<String>>, Failure> {
        list(key)?
            .iter()
            .enumerate()
            .map(|(q, v)| match v {
                Value::Null => Ok(None),
                Value::String(s) => Ok(Some(s.clone())),
                _ => Err(Failure::text(format!(
                    "`{verb}` {key} for question {q} is neither a text nor null"
                ))),
            })
            .collect()
    };
    let (other, notes) = (texts("other")?, texts("notes")?);
    let count = picks.len().max(other.len()).max(notes.len());
    Ok((0..count)
        .map(|q| QuestionAnswer {
            question: q,
            options: picks.get(q).cloned().unwrap_or_default(),
            other: other.get(q).cloned().flatten(),
            notes: notes.get(q).cloned().flatten(),
        })
        .filter(|a| !a.options.is_empty() || a.other.is_some() || a.notes.is_some())
        .collect())
}

/// A question action's arguments, read and checked for shape.
struct QuestionAction {
    /// `None` for an answer; the kind for a decline.
    decline: Option<DeclineKind>,
    /// The text a refusal of a late answer holds.
    refused: Option<String>,
    answers: Vec<QuestionAnswer>,
}

fn question_action_of(verb: &str, arg: &Value) -> Result<QuestionAction, Failure> {
    let decline = match verb {
        "answer_question" => None,
        _ => Some(match arg["kind"].as_str() {
            Some("cancel") => DeclineKind::Cancel,
            Some("chat") => DeclineKind::Chat,
            other => {
                return Err(Failure::text(format!(
                    "`decline_question` kind is cancel or chat, not {other:?}"
                )));
            }
        }),
    };
    for key in arg.as_object().into_iter().flat_map(|o| o.keys()) {
        let known =
            ["picks", "other", "notes", "refused"].contains(&key.as_str()) || (decline.is_some() && key == "kind");
        if !known {
            return Err(Failure::text(format!("`{verb}` has an unknown key `{key}`")));
        }
    }
    let refused = match &arg["refused"] {
        Value::Null => None,
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => {
            return Err(Failure::text(format!(
                "`{verb}` `refused` is the text the refusal holds"
            )));
        }
    };
    let answers = answers_of(verb, arg)?;
    if decline == Some(DeclineKind::Cancel) && !answers.is_empty() {
        return Err(Failure::text(
            "`decline_question` with kind cancel takes no answers: only a chat request sends the answers so far",
        ));
    }
    if verb == "answer_question" && arg.get("picks").is_none() {
        return Err(Failure::text(
            "`answer_question` needs `picks`: a list of option indexes per question",
        ));
    }
    Ok(QuestionAction {
        decline,
        refused,
        answers,
    })
}

/// A `resolve_plan` part's decision and the text a refusal must hold, read and checked for shape.
fn plan_action_of(arg: &Value) -> Result<(PlanDecision, Option<String>), Failure> {
    for key in arg.as_object().into_iter().flat_map(|o| o.keys()) {
        if !["choice", "feedback", "refused"].contains(&key.as_str()) {
            return Err(Failure::text(format!("`resolve_plan` has an unknown key `{key}`")));
        }
    }
    let feedback = match &arg["feedback"] {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        _ => return Err(Failure::text("`resolve_plan` `feedback` is a text")),
    };
    let decision = match arg["choice"].as_str() {
        Some("keep") => PlanDecision::Keep {
            feedback: feedback.clone().unwrap_or_default(),
        },
        Some(approve @ ("approve_manual" | "approve_auto" | "approve_bypass")) => {
            if feedback.is_some() {
                return Err(Failure::text("`resolve_plan` takes `feedback` only with `keep`"));
            }
            match approve {
                "approve_manual" => PlanDecision::ApproveManual,
                "approve_auto" => PlanDecision::ApproveAuto,
                _ => PlanDecision::ApproveBypass,
            }
        }
        other => {
            return Err(Failure::text(format!(
                "`resolve_plan` choice is keep, approve_manual, approve_auto or approve_bypass, not {other:?}"
            )));
        }
    };
    let refused = match &arg["refused"] {
        Value::Null => None,
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => return Err(Failure::text("`resolve_plan` `refused` is the text the refusal holds")),
    };
    Ok((decision, refused))
}

/// Every answer names a question of the set and options that question has.
fn check_against(verb: &str, answers: &[QuestionAnswer], asked: &AskedQuestion) -> Result<(), Failure> {
    let n = asked.questions.len();
    for a in answers {
        let Some(question) = asked.questions.get(a.question) else {
            let what = if !a.options.is_empty() {
                "picks"
            } else if a.other.is_some() {
                "other"
            } else {
                "notes"
            };
            return Err(Failure::text(format!(
                "`{verb}` {what} for question {}, and the CLI asked {n} question(s)",
                a.question
            )));
        };
        let options = question["options"].as_array().map_or(0, Vec::len);
        if let Some(bad) = a.options.iter().find(|&&i| i >= options) {
            return Err(Failure::text(format!(
                "`{verb}` picks option {bad} of question {}, which has {options} option(s)",
                a.question
            )));
        }
        let mut seen = BTreeSet::new();
        if let Some(again) = a.options.iter().find(|&&i| !seen.insert(i)) {
            return Err(Failure::text(format!(
                "`{verb}` picks option {again} of question {} twice",
                a.question
            )));
        }
        // A question the CLI asks without multiSelect takes one option; a typed text stands in for a pick.
        let single = !question["multiSelect"].as_bool().unwrap_or(false);
        let chosen = a.options.len() + usize::from(a.other.is_some());
        if single && chosen > 1 {
            return Err(Failure::text(format!(
                "`{verb}` gives question {} {chosen} answers (picks and typed text), and it takes one: the CLI \
                 asks it without multiSelect",
                a.question
            )));
        }
    }
    Ok(())
}

/// A tool name without the `mcp__<server>__` prefix the model sees it under.
fn bare_tool(name: &str) -> String {
    match name.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
        Some((_server, tool)) => tool.to_owned(),
        None => name.to_owned(),
    }
}

/// A `client_answer` part's arguments: the text given whole, or built by repeating a unit up to a size.
fn client_answer_of(arg: &Value) -> Result<ClientAnswer, Failure> {
    let tool = text_arg(arg, "tool").map(|t| bare_tool(&t));
    let is_error = arg["is_error"].as_bool().unwrap_or(false);
    let text = if let Some(fill) = arg.get("fill") {
        let unit = fill["unit"].as_str().filter(|u| !u.is_empty() && u.is_ascii());
        let bytes = fill["bytes"].as_u64();
        let (Some(unit), Some(bytes)) = (unit, bytes) else {
            return Err(Failure::text(
                "`client_answer` `fill` needs an ASCII `unit` and a `bytes` count",
            ));
        };
        let mut text = unit.repeat(bytes.div_ceil(unit.len() as u64) as usize);
        text.truncate(bytes as usize);
        text
    } else {
        text_arg(arg, "text").ok_or_else(|| Failure::text("`client_answer` needs a `text` or a `fill`"))?
    };
    Ok(ClientAnswer { tool, text, is_error })
}

fn target_of(verb: &str, arg: &Value) -> Option<String> {
    match verb {
        "request" | "request_done" => text_arg(arg, "id"),
        "held" => text_arg(arg, "name"),
        "tap_line" | "card" | "client_call" => text_arg(arg, "kind").or_else(|| text_arg(arg, "tool")),
        "session_state" => arg.as_str().map(str::to_owned),
        _ => None,
    }
}

/// Why a part failed: the host said so, or the driver did.
enum Failure {
    Host(DriverError),
    Text(String),
}

impl Failure {
    fn text(why: impl Into<String>) -> Self {
        Self::Text(why.into())
    }
}

impl From<DriverError> for Failure {
    fn from(e: DriverError) -> Self {
        Self::Host(e)
    }
}

/// A one-line JSON description of a card record list, for logs.
pub fn cards_json(cards: &[Answered]) -> Value {
    Value::Array(cards.iter().map(Answered::to_json).collect())
}

/// The `json!` shape the manifest keeps of a driver run.
pub fn report_json(report: &Report) -> Value {
    json!({"parts_run": report.parts_run, "pending": report.pending})
}

#[cfg(test)]
mod tests {
    use super::super::gate_answer::{Origin, Request};
    use super::*;
    use crate::provider::PermissionDecision;
    use std::{
        cell::RefCell,
        sync::{Arc, Mutex},
    };

    struct Fake {
        calls: Arc<Mutex<Vec<String>>>,
        caps: BTreeSet<&'static str>,
        summary: String,
        projection: Vec<Value>,
        cards: RefCell<Vec<Answered>>,
        roster: Vec<RosterEntry>,
        interaction: bool,
        /// A question answer is refused with this text.
        refuse: Option<String>,
        /// What a task's output reads as.
        output: String,
        /// The calls of declared tools the host was handed; the last `open` of them are still open.
        client_calls: Vec<ClientCallSeen>,
        open: usize,
        /// The layer this host says it is.
        layer: &'static str,
        /// Whether the host's plan records hold what a plan wait describes.
        plan: bool,
        /// The host cannot carry images: a send with them fails as missing the capability.
        no_images: bool,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                calls: Arc::default(),
                caps: BTreeSet::new(),
                summary: "FAKE-PROJECTION-SUMMARY".into(),
                projection: Vec::new(),
                cards: RefCell::new(Vec::new()),
                roster: Vec::new(),
                interaction: false,
                refuse: None,
                output: "tail".into(),
                client_calls: Vec::new(),
                open: 0,
                layer: "a",
                plan: false,
                no_images: false,
            }
        }
        fn note(&self, text: impl Into<String>) {
            self.calls.lock().unwrap().push(text.into());
        }
    }

    impl Host for Fake {
        fn send(&mut self, text: &str, typed: bool) -> Result<(), DriverError> {
            if typed {
                return Err(DriverError::CapabilityMissing("human_origin"));
            }
            self.note(format!("send {text}"));
            Ok(())
        }
        fn send_images(&mut self, text: &str, images: &[String], _typed: bool) -> Result<(), DriverError> {
            if self.no_images {
                return Err(DriverError::CapabilityMissing("turn_content_v1"));
            }
            self.note(format!("send {text} with {}", images.join("+")));
            Ok(())
        }
        fn queue_images(&mut self, text: &str, images: &[String]) -> Result<(), DriverError> {
            self.note(format!("queue {text} with {}", images.join("+")));
            Ok(())
        }
        fn steer(&mut self, text: &str, typed: bool) -> Result<(), DriverError> {
            if typed {
                return Err(DriverError::CapabilityMissing("human_origin"));
            }
            self.note(format!("steer {text}"));
            Ok(())
        }
        fn queue(&mut self, text: &str) -> Result<(), DriverError> {
            self.note(format!("queue {text}"));
            Ok(())
        }
        fn interrupt(&mut self) -> Result<(), DriverError> {
            self.note("interrupt");
            Ok(())
        }
        fn answer_card(&mut self, tool: &str, allow: bool) -> Result<(), DriverError> {
            self.note(format!("answer_card {tool} {allow}"));
            for card in self
                .cards
                .borrow_mut()
                .iter_mut()
                .filter(|c| c.request.tool_name == tool)
            {
                card.decision = Some(PermissionDecision::Allow);
            }
            Ok(())
        }
        fn answer_card_from(&mut self, tool: &str, allow: bool, origin: Origin) -> Result<(), DriverError> {
            self.note(format!("answer_card {tool} {allow} {}", origin.word()));
            Ok(())
        }
        fn approve_sweep(&mut self) -> Result<(), DriverError> {
            self.note("approve_sweep");
            Ok(())
        }
        fn slash(&mut self, command: &str) -> Result<(), DriverError> {
            self.note(format!("slash {command}"));
            Ok(())
        }
        fn close(&mut self) -> Result<(), DriverError> {
            self.note("close");
            Ok(())
        }
        fn resume(&mut self) -> Result<(), DriverError> {
            self.note("resume");
            Ok(())
        }
        fn projection_summary(&self) -> String {
            self.summary.clone()
        }
        fn projection_holds(&self, want: &Value) -> Result<bool, DriverError> {
            Ok(self.projection.iter().any(|p| fields_match(p, want)))
        }
        fn cards_shown(&self) -> Vec<Answered> {
            self.cards.borrow().clone()
        }
        fn automatic_answers(&self) -> Vec<Answered> {
            Vec::new()
        }
        fn capabilities(&self) -> BTreeSet<&'static str> {
            self.caps.clone()
        }
        fn task_output(&mut self, task: &str) -> Result<String, DriverError> {
            if !self.caps.contains("task_output") {
                return Err(DriverError::CapabilityMissing("task_output"));
            }
            self.note(format!("task_output {task}"));
            self.refuse
                .clone()
                .map_or(Ok(self.output.clone()), |why| Err(DriverError::Failed(why)))
        }
        fn roster(&self) -> Result<Vec<RosterEntry>, DriverError> {
            if self.caps.contains("background_tasks") {
                Ok(self.roster.clone())
            } else {
                Err(DriverError::CapabilityMissing("background_tasks"))
            }
        }
        fn answer_question(&mut self, answers: &[QuestionAnswer]) -> Result<(), DriverError> {
            if !self.caps.contains("structured_questions_v1") {
                return Err(DriverError::CapabilityMissing("structured_questions_v1"));
            }
            self.note(format!("answer {answers:?}"));
            self.refuse.clone().map_or(Ok(()), |why| Err(DriverError::Failed(why)))
        }
        fn decline_question(&mut self, kind: DeclineKind, so_far: &[QuestionAnswer]) -> Result<(), DriverError> {
            if !self.caps.contains("structured_questions_v1") {
                return Err(DriverError::CapabilityMissing("structured_questions_v1"));
            }
            self.note(format!("decline {kind:?} {so_far:?}"));
            self.refuse.clone().map_or(Ok(()), |why| Err(DriverError::Failed(why)))
        }
        fn interaction_holds(&self, _: &Value) -> Result<bool, DriverError> {
            if self.caps.contains("structured_questions_v1") {
                Ok(self.interaction)
            } else {
                Err(DriverError::CapabilityMissing("structured_questions_v1"))
            }
        }
        fn set_mode(&mut self, mode: &str) -> Result<(), DriverError> {
            self.note(format!("set_mode {mode}"));
            Ok(())
        }
        fn set_planning(&mut self, enter: bool) -> Result<(), DriverError> {
            if !self.caps.contains(PLAN_MODE) {
                return Err(DriverError::CapabilityMissing(PLAN_MODE));
            }
            self.note(format!("set_planning {enter}"));
            Ok(())
        }
        fn resolve_plan(&mut self, decision: &PlanDecision) -> Result<(), DriverError> {
            if !self.caps.contains(PLAN_MODE) {
                return Err(DriverError::CapabilityMissing(PLAN_MODE));
            }
            self.note(format!("resolve_plan {decision:?}"));
            self.refuse.clone().map_or(Ok(()), |why| Err(DriverError::Failed(why)))
        }
        fn plan_holds(&self, _: &Value) -> Result<bool, DriverError> {
            if self.caps.contains(PLAN_MODE) {
                Ok(self.plan)
            } else {
                Err(DriverError::CapabilityMissing(PLAN_MODE))
            }
        }
        fn layer(&self) -> &'static str {
            self.layer
        }
        fn client_calls_seen(&self) -> Result<Vec<ClientCallSeen>, DriverError> {
            Ok(self.client_calls.clone())
        }
        fn client_answer(&mut self, answer: &ClientAnswer) -> Result<(), DriverError> {
            self.note(format!(
                "client_answer {:?} {} bytes error={}",
                answer.tool,
                answer.text.len(),
                answer.is_error
            ));
            self.open = self.open.saturating_sub(1);
            Ok(())
        }
        fn client_state(&mut self) -> Result<ClientState, DriverError> {
            let start = self.client_calls.len() - self.open;
            Ok(ClientState {
                through_sequence: 7,
                open: self.client_calls[start..].to_vec(),
            })
        }
        fn move_freeze(&mut self) -> Result<(), DriverError> {
            self.note("freeze");
            Ok(())
        }
        fn move_adopt(&mut self) -> Result<(), DriverError> {
            self.note("adopt");
            Ok(())
        }
        fn move_commit(&mut self) -> Result<(), DriverError> {
            self.note("commit");
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeEndpoint {
        records: Mutex<Vec<RequestRecord>>,
        finished: Mutex<BTreeSet<usize>>,
        held: Mutex<Vec<String>>,
        released: Mutex<Vec<String>>,
        captures: Mutex<BTreeMap<String, String>>,
    }

    impl Endpoint for FakeEndpoint {
        fn records(&self) -> Vec<RequestRecord> {
            self.records.lock().unwrap().clone()
        }
        fn finished(&self) -> BTreeSet<usize> {
            self.finished.lock().unwrap().clone()
        }
        fn held(&self) -> Vec<String> {
            self.held.lock().unwrap().clone()
        }
        fn release(&self, name: &str) {
            self.released.lock().unwrap().push(name.into());
        }
        fn captures(&self) -> BTreeMap<String, String> {
            self.captures.lock().unwrap().clone()
        }
    }

    fn record(index: usize, step: Option<&str>, side: Option<&str>, unmatched: Option<&str>) -> RequestRecord {
        RequestRecord {
            arrival_index: index,
            time: 0.0,
            path: "/v1/messages".into(),
            headers: BTreeMap::new(),
            matched_step: step.map(str::to_owned),
            side_kind: side.map(str::to_owned),
            unmatched: unmatched.map(str::to_owned),
            census: Vec::new(),
            response_events: Vec::new(),
            aborted_by_client: false,
            body_file: format!("body_{index}.json"),
        }
    }

    /// A run directory of the test's own, removed when the test ends however it ends.
    fn run_dir() -> crate::test_tmp::TmpDir {
        let run = crate::test_tmp::TmpDir::new("driver", "run");
        std::fs::create_dir_all(run.join("log")).unwrap();
        std::fs::create_dir_all(run.join("gates")).unwrap();
        run
    }

    fn tap(run: &Path, name: &str, lines: &[(u64, Value)]) {
        let text: String = lines
            .iter()
            .map(|(i, v)| {
                format!(
                    "{}\n",
                    json!({"index": i, "time": *i as f64 / 10.0, "line": v.to_string()})
                )
            })
            .collect();
        std::fs::write(run.join("log").join(name), text).unwrap();
    }

    fn settings(run: &Path) -> Settings {
        Settings {
            scenario: "fake".into(),
            run: run.to_path_buf(),
            base: Duration::from_millis(250),
            scale: 1.0,
            poll: Duration::from_millis(5),
        }
    }

    fn drive(host: &mut Fake, endpoint: &FakeEndpoint, run: &Path, parts: Value) -> Result<Report, String> {
        Driver::new(settings(run), endpoint, host).run(parts.as_array().unwrap())
    }

    #[test]
    fn every_action_reaches_the_host() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        drive(
            &mut host,
            &endpoint,
            &run,
            json!([
                {"send": "hello"}, {"steer": "more"}, {"queue": {"text": "later"}}, {"interrupt": true},
                {"answer_card": {"tool": "Bash", "allow": false}}, {"approve_sweep": true}, {"slash": "/clear"},
                {"close": true}, {"resume": true}, {"release": "h1"}, {"open_gate": "bg1"}
            ]),
        )
        .unwrap();
        let calls = host.calls.lock().unwrap().join("|");
        assert_eq!(
            calls,
            "send hello|steer more|queue later|interrupt|answer_card Bash false|approve_sweep|slash /clear|close|resume"
        );
        assert_eq!(endpoint.released.lock().unwrap().as_slice(), ["h1"]);
        assert!(run.join("gates/bg1.open").exists());
    }

    #[test]
    fn a_card_answer_may_name_the_origin_it_is_for() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        drive(
            &mut host,
            &endpoint,
            &run,
            json!([
                {"answer_card": {"tool": "Write", "allow": false, "origin": "cli_prompt"}},
                {"answer_card": {"tool": "Write", "origin": "hook"}}
            ]),
        )
        .unwrap();
        assert_eq!(
            host.calls.lock().unwrap().as_slice(),
            ["answer_card Write false cli_prompt", "answer_card Write true hook"]
        );
        let err = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"answer_card": {"tool": "Write", "origin": "prompt"}}]),
        )
        .unwrap_err();
        assert!(err.contains("origin is `hook` or `cli_prompt`"), "{err}");
    }

    #[test]
    fn a_send_or_queue_with_images_reaches_the_host_with_their_names_in_order() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        drive(
            &mut host,
            &endpoint,
            &run,
            json!([
                {"send": {"text": "look", "images": ["a.png", "b.jpg"]}},
                {"queue": {"text": "later", "images": ["c.gif"]}},
                {"send": {"text": "plain", "images": []}}
            ]),
        )
        .unwrap();
        assert_eq!(
            host.calls.lock().unwrap().join("|"),
            "send look with a.png+b.jpg|queue later with c.gif|send plain"
        );
        let err = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"send": {"text": "x", "images": "a.png"}}]),
        )
        .unwrap_err();
        assert!(err.contains("images"), "{err}");
    }

    #[test]
    fn a_host_that_carries_no_images_fails_a_send_with_them_as_missing_the_capability() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.no_images = true;
        let err = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"send": {"text": "look", "images": ["a.png"]}}]),
        )
        .unwrap_err();
        assert!(err.contains("turn_content_v1") && err.contains("requires"), "{err}");
    }

    #[test]
    fn a_typed_send_fails_as_human_origin_until_the_host_has_it() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        let err = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"send": {"text": "hi", "typed": true}}]),
        )
        .unwrap_err();
        assert!(err.contains("human_origin") && err.contains("requires"), "{err}");
        let report = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"send": {"text": "hi", "typed": true}, "requires": "human_origin"}]),
        )
        .unwrap();
        assert_eq!(report.pending, ["PENDING fake/0-send: needs human_origin"]);
    }

    #[test]
    fn captures_expand_in_the_arguments() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        endpoint.captures.lock().unwrap().insert("bg1".into(), "task-7".into());
        drive(&mut host, &endpoint, &run, json!([{"steer": "stop {bg1} in {RUN}"}])).unwrap();
        let calls = host.calls.lock().unwrap().clone();
        assert_eq!(calls, [format!("steer stop task-7 in {}", run.display())]);
    }

    #[test]
    fn request_waits_see_steps_side_kinds_and_completion() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        *endpoint.records.lock().unwrap() = vec![
            record(1, Some("m1"), None, None),
            record(2, None, Some("count_tokens"), None),
        ];
        endpoint.finished.lock().unwrap().insert(1);
        let parts = json!([{"request": "m1"}, {"request": "count_tokens"}, {"request_done": "m1"}]);
        assert_eq!(drive(&mut host, &endpoint, &run, parts).unwrap().parts_run, 3);
        let err = drive(&mut host, &endpoint, &run, json!([{"request_done": "count_tokens"}])).unwrap_err();
        assert!(err.contains("timed out") && err.contains("request_done"), "{err}");
        let err = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"request": {"id": "m1", "count": 2}}]),
        )
        .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn held_waits_for_the_hold_to_show() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        assert!(drive(&mut host, &endpoint, &run, json!([{"held": "h1"}]))
            .unwrap_err()
            .contains("timed out"));
        endpoint.held.lock().unwrap().push("h1".into());
        drive(&mut host, &endpoint, &run, json!([{"held": "h1"}])).unwrap();
    }

    #[test]
    fn tap_waits_read_kinds_fields_results_and_session_state() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                (
                    2,
                    json!({"type": "system", "subtype": "session_state_changed", "state": "idle"}),
                ),
                (
                    5,
                    json!({"type": "system", "subtype": "task_started", "task": {"id": "t1"}}),
                ),
                (6, json!({"type": "result", "origin": {"kind": "human"}})),
                (
                    9,
                    json!({"type": "system", "subtype": "session_state_changed", "state": "running"}),
                ),
                (11, json!({"type": "result", "origin": {"kind": "task-notification"}})),
            ],
        );
        let parts = json!([
            {"tap_line": {"kind": "system/task_started", "fields": {"task.id": "t1"}}},
            {"tap_line": {"kind": "result", "fields": {"origin.kind": {"contains": "notification"}}}},
            {"tap_line": {"kind": "result", "count": 2}},
            {"cli_results": 2},
        ]);
        assert_eq!(drive(&mut host, &endpoint, &run, parts).unwrap().parts_run, 4);
        assert!(drive(&mut host, &endpoint, &run, json!([{"cli_results": 3}]))
            .unwrap_err()
            .contains("timed out"));
        // The newest state line is `running`, so `idle` has not come yet; after a send the older lines do not count.
        assert!(drive(&mut host, &endpoint, &run, json!([{"session_state": "idle"}]))
            .unwrap_err()
            .contains("timed out"));
        drive(&mut host, &endpoint, &run, json!([{"session_state": "running"}])).unwrap();
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                (
                    2,
                    json!({"type": "system", "subtype": "session_state_changed", "state": "idle"}),
                ),
                (
                    9,
                    json!({"type": "system", "subtype": "session_state_changed", "state": "running"}),
                ),
                (
                    12,
                    json!({"type": "system", "subtype": "session_state_changed", "state": "idle"}),
                ),
            ],
        );
        drive(&mut host, &endpoint, &run, json!([{"session_state": "idle"}])).unwrap();
    }

    #[test]
    fn a_session_state_wait_ignores_state_lines_from_before_the_latest_send() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        tap(
            &run,
            "cli-stdout.jsonl",
            &[(
                3,
                json!({"type": "system", "subtype": "session_state_changed", "state": "idle"}),
            )],
        );
        let err = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"send": "go"}, {"session_state": "idle"}]),
        )
        .unwrap_err();
        assert!(err.contains("timed out"), "the idle line came before the send: {err}");
    }

    #[test]
    fn transcript_waits_tail_the_session_file_when_it_appears() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        let part = json!([{"transcript_line": {"type": "queue-operation", "fields": {"operation": "enqueue", "content": {"contains": "PROMPT"}}}}]);
        assert!(drive(&mut host, &endpoint, &run, part.clone())
            .unwrap_err()
            .contains("timed out"));
        let dir = run.join("claude/projects/-slug");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("s1.jsonl"),
            format!(
                "{}\nnot json\n{}\n",
                json!({"type": "user"}),
                json!({"type": "queue-operation", "operation": "enqueue", "content": "a PROMPT b"})
            ),
        )
        .unwrap();
        drive(&mut host, &endpoint, &run, part).unwrap();
    }

    #[test]
    fn a_workflow_stop_fence_waits_for_the_nested_tool_result() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        let scenario: Value =
            serde_json::from_str(include_str!("../../../tests/claude_scenarios/c16w_stop.json")).unwrap();
        for parts in [&scenario["driver"], &scenario["variants"]["run1"]["driver"]] {
            let parts = parts.as_array().unwrap();
            let gate = parts
                .iter()
                .rposition(|part| part.get("open_gate") == Some(&json!("wb")))
                .unwrap();
            let fence = &parts[gate + 1];
            assert_eq!(fence["name"], "wb-ended");
            let mut waiting = json!([fence]);
            waiting[0]["timeout"] = json!(0.01);
            let dir = run.join("claude/projects/-slug/session/subagents/workflows/wf");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("agent-b.jsonl");
            std::fs::write(&path, "").unwrap();
            assert!(drive(&mut host, &endpoint, &run, waiting.clone())
                .unwrap_err()
                .contains("timed out"));
            // An unrelated tool's error cannot satisfy B's completion fence.
            let mut result = json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_wa", "is_error": true}]}});
            std::fs::write(&path, format!("{result}\n")).unwrap();
            assert!(drive(&mut host, &endpoint, &run, waiting.clone())
                .unwrap_err()
                .contains("timed out"));
            result["message"]["content"][0]["tool_use_id"] = json!("toolu_wb");
            std::fs::write(&path, format!("{result}\n")).unwrap();
            drive(&mut host, &endpoint, &run, waiting).unwrap();
        }
    }

    #[test]
    fn projection_interaction_roster_and_card_waits_ask_the_host() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.projection = vec![json!({"type": "turn_started", "turn_id": "t1"})];
        host.caps.extend(["background_tasks", "structured_questions_v1"]);
        host.interaction = true;
        host.roster = vec![RosterEntry {
            task: "bg1".into(),
            kind: "shell".into(),
            status: "running".into(),
            ambient: false,
        }];
        host.cards.borrow_mut().push(Answered {
            request: Request {
                permission_id: "p1".into(),
                tool_use_id: None,
                tool_name: "Bash".into(),
                input: json!({}),
                origin: Origin::Hook,
            },
            decision: Some(PermissionDecision::Allow),
        });
        let parts = json!([
            {"projection": {"type": "turn_started"}}, {"interaction": {"question": "waiting"}},
            {"roster": {"task": "bg1", "status": "running"}}, {"card": {"tool": "Bash"}}
        ]);
        assert_eq!(drive(&mut host, &endpoint, &run, parts).unwrap().parts_run, 4);
        for (part, word) in [
            (json!({"projection": {"type": "turn_completed"}}), "projection"),
            (json!({"roster": {"task": "bg1", "status": "stopped"}}), "roster"),
            (json!({"card": {"tool": "Edit"}}), "card"),
        ] {
            let err = drive(&mut host, &endpoint, &run, json!([part])).unwrap_err();
            assert!(err.contains("timed out") && err.contains(word), "{err}");
        }
        host.interaction = false;
        assert!(drive(&mut host, &endpoint, &run, json!([{"interaction": {}}]))
            .unwrap_err()
            .contains("timed out"));
    }

    #[test]
    fn a_wall_floor_waits_out_the_time_since_an_earlier_part() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        endpoint.held.lock().unwrap().push("h1".into());
        let mut driver = Driver::new(settings(&run), &endpoint, &mut host);
        let started = driver.now();
        driver
            .run(&[
                json!({"held": "h1", "name": "gate-up"}),
                json!({"wall_floor": {"after": "gate-up", "seconds": 0.15}}),
            ])
            .unwrap();
        assert_eq!(driver.now().duration_since(started), Duration::from_millis(150));
        assert_eq!(driver.waits.get(), 30);
        drop(driver);
        let err = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"wall_floor": {"after": "nothing", "seconds": 1}}]),
        )
        .unwrap_err();
        assert!(err.contains("nothing"), "{err}");
    }

    #[test]
    fn a_timeout_reports_what_was_awaited_the_tap_the_requests_and_the_projection() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        tap(
            &run,
            "cli-stdout.jsonl",
            &(1..=25)
                .map(|i| (i, json!({"type": "assistant", "n": i})))
                .collect::<Vec<_>>(),
        );
        tap(
            &run,
            "cli-stdin.jsonl",
            &[(0, json!({"type": "user", "marker": "FIRST-STDIN"}))],
        );
        *endpoint.records.lock().unwrap() = vec![record(1, Some("m1"), None, None)];
        let err = drive(&mut host, &endpoint, &run, json!([{"held": "never-held"}])).unwrap_err();
        assert!(err.contains("driver part 0-held"), "{err}");
        assert!(err.contains("timed out") && err.contains("never-held"), "{err}");
        assert!(err.contains("last 20 tap lines") && err.contains("\"n\":25"), "{err}");
        assert!(
            !err.contains("\"n\":5,") && !err.contains("\"n\":5}"),
            "only the last 20 lines: {err}"
        );
        assert!(!err.contains("FIRST-STDIN"), "{err}");
        assert!(err.contains("#1 /v1/messages step=m1"), "{err}");
        assert!(err.contains("projection: FAKE-PROJECTION-SUMMARY"), "{err}");
    }

    #[test]
    fn an_unmatched_record_fails_a_pending_wait_at_once() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        *endpoint.records.lock().unwrap() = vec![record(4, None, None, Some("no scripted step takes this request"))];
        let mut slow = settings(&run);
        slow.base = Duration::from_secs(30);
        let mut driver = Driver::new(slow, &endpoint, &mut host);
        let err = driver.run(&[json!({"held": "h1"})]).unwrap_err();
        assert_eq!(driver.waits.get(), 0, "an unmatched request fails before waiting");
        assert!(
            err.contains("could not match request #4") && err.contains("no scripted step"),
            "{err}"
        );
    }

    #[test]
    fn a_card_nobody_answers_fails_a_wait_for_something_else_and_not_a_card_wait() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.cards.borrow_mut().push(Answered {
            request: Request {
                permission_id: "p1".into(),
                tool_use_id: None,
                tool_name: "Write".into(),
                input: json!({}),
                origin: Origin::Hook,
            },
            decision: None,
        });
        let err = drive(&mut host, &endpoint, &run, json!([{"held": "h1"}])).unwrap_err();
        assert!(
            err.contains("permission request for Write") && !err.contains("timed out"),
            "{err}"
        );
        drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"card": {"tool": "Write"}}, {"answer_card": {"tool": "Write"}}]),
        )
        .unwrap();
    }

    #[test]
    fn a_wait_whose_condition_holds_still_fails_on_an_unmatched_request_or_an_unanswered_card() {
        let projection = json!([{"projection": {"type": "turn_started"}}]);
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.projection = vec![json!({"type": "turn_started", "turn_id": "t1"})];
        *endpoint.records.lock().unwrap() = vec![record(2, None, None, Some("no scripted step takes this request"))];
        let err = drive(&mut host, &endpoint, &run, projection.clone()).unwrap_err();
        assert!(err.contains("could not match request #2"), "{err}");

        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.projection = vec![json!({"type": "turn_started", "turn_id": "t1"})];
        host.cards.borrow_mut().push(Answered {
            request: Request {
                permission_id: "p1".into(),
                tool_use_id: None,
                tool_name: "Write".into(),
                input: json!({}),
                origin: Origin::Hook,
            },
            decision: None,
        });
        let err = drive(&mut host, &endpoint, &run, projection).unwrap_err();
        assert!(err.contains("permission request for Write"), "{err}");
    }

    #[test]
    fn a_part_requiring_a_capability_the_host_lacks_is_pending_not_failed() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        let parts = json!([
            {"stop_task": "t1", "requires": "task_control", "name": "stop-it"},
            {"roster": {"task": "t1"}, "requires": ["background_tasks"]},
            {"answer_question": {"picks": [["A"]]}, "requires": "structured_questions_v1"},
            {"send": "after", "name": "ceiling", "requires": "eitri_bash_ceiling"},
            {"send": "runs"}
        ]);
        let report = drive(&mut host, &endpoint, &run, parts).unwrap();
        assert_eq!(
            report.pending,
            [
                "PENDING fake/stop-it: needs task_control",
                "PENDING fake/1-roster: needs background_tasks",
                "PENDING fake/2-answer_question: needs structured_questions_v1",
                "PENDING fake/ceiling: needs eitri_bash_ceiling",
            ]
        );
        assert_eq!(report.parts_run, 1);
        assert_eq!(host.calls.lock().unwrap().as_slice(), ["send runs"]);
    }

    #[test]
    fn a_declared_capability_whose_method_is_missing_fails_the_part() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.caps.insert("task_control");
        let err = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"stop_task": "t1", "requires": "task_control"}]),
        )
        .unwrap_err();
        assert!(
            err.contains("declares capability `task_control`") && err.contains("0-stop_task"),
            "{err}"
        );
    }

    #[test]
    fn a_part_that_uses_an_undeclared_capability_without_requiring_it_fails() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        let err = drive(&mut host, &endpoint, &run, json!([{"set_per_task_stop": true}])).unwrap_err();
        assert!(err.contains("per_task_stop") && err.contains("requires"), "{err}");
        let err = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"decline_question": {"kind": "cancel"}}]),
        )
        .unwrap_err();
        assert!(err.contains("structured_questions_v1"), "{err}");
    }

    /// A tap where the CLI asked one question set of two questions (two options, then four), `answered` saying
    /// whether its control response went back.
    fn asked(run: &Path, answered: bool) {
        let options = |n: usize| -> Vec<Value> {
            (0..n)
                .map(|i| json!({"label": format!("L{i}"), "description": "d"}))
                .collect()
        };
        tap(
            run,
            "cli-stdout.jsonl",
            &[(
                4,
                json!({"type": "control_request", "request_id": "req-q1", "request": {
                "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "toolu_q1",
                "input": {"questions": [
                    {"question": "One?", "header": "One", "multiSelect": false, "options": options(2)},
                    {"question": "Two?", "header": "Two", "multiSelect": true, "options": options(4)}
                ]}}}),
            )],
        );
        let mut stdin = vec![(0, json!({"type": "user", "message": {"role": "user", "content": "go"}}))];
        if answered {
            stdin.push((
                6,
                json!({"type": "control_response", "response": {"subtype": "success", "request_id": "req-q1", "response": {}}}),
            ));
        }
        tap(run, "cli-stdin.jsonl", &stdin);
    }

    fn questions_host() -> Fake {
        let mut host = Fake::new();
        host.caps.insert("structured_questions_v1");
        host
    }

    #[test]
    fn an_answer_reaches_the_host_as_option_indexes_typed_text_and_notes_per_question() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), questions_host());
        asked(&run, false);
        drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"answer_question": {"picks": [[1], [0, 2]], "other": [null, "typed"], "notes": ["a note"]}}]),
        )
        .unwrap();
        let want = vec![
            QuestionAnswer {
                question: 0,
                options: vec![1],
                other: None,
                notes: Some("a note".into()),
            },
            QuestionAnswer {
                question: 1,
                options: vec![0, 2],
                other: Some("typed".into()),
                notes: None,
            },
        ];
        assert_eq!(host.calls.lock().unwrap().as_slice(), [format!("answer {want:?}")]);
    }

    #[test]
    fn an_answer_picking_an_option_or_a_question_the_cli_did_not_ask_fails_before_the_host() {
        for (answer, word) in [
            (json!({"picks": [[2]]}), "option 2 of question 0, which has 2 option(s)"),
            (
                json!({"picks": [[0], [4]]}),
                "option 4 of question 1, which has 4 option(s)",
            ),
            (
                json!({"picks": [[0], [1], [0]]}),
                "picks for question 2, and the CLI asked 2 question(s)",
            ),
            (
                json!({"picks": [[0]], "other": [null, null, "x"]}),
                "other for question 2",
            ),
            (
                json!({"picks": [], "notes": [null, null, null, "n"]}),
                "notes for question 3",
            ),
            (json!({"picks": [["L0"]]}), "not a list of option indexes"),
            (json!({"picks": [[0]], "other": {"1": "x"}}), "`other` is a list"),
            (json!({"picks": [[0]], "label": "x"}), "unknown key `label`"),
            (json!({"other": ["x"]}), "needs `picks`"),
            (json!({"picks": [[0, 1]]}), "gives question 0 2 answers"),
            (json!({"picks": [[0]], "other": ["x"]}), "gives question 0 2 answers"),
            (json!({"picks": [[0], [2, 2]]}), "picks option 2 of question 1 twice"),
        ] {
            let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), questions_host());
            asked(&run, false);
            let err = drive(&mut host, &endpoint, &run, json!([{ "answer_question": answer }])).unwrap_err();
            assert!(err.contains(word), "{answer}: {err}");
            assert!(host.calls.lock().unwrap().is_empty(), "{answer}: the host was called");
        }
    }

    /// Every question answer a scenario file scripts fits the question its model step asks, so a slip in a row's
    /// arguments shows here and not only when a host that answers questions first runs the row.
    #[test]
    fn every_scripted_question_answer_fits_the_question_its_scenario_asks() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/claude_scenarios");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let scenario: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let asked: Vec<AskedQuestion> = scenario["model"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|step| step["reply"].as_array().into_iter().flatten())
                .filter(|r| r["tool_use"]["name"] == QUESTION_TOOL)
                .map(|r| AskedQuestion {
                    tool_use_id: r["tool_use"]["id"].as_str().map(str::to_owned),
                    questions: r["tool_use"]["input"]["questions"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default(),
                    answered: false,
                    settled: false,
                    answered_before: 0,
                })
                .collect();
            let actions = scenario["driver"].as_array().into_iter().flatten().filter_map(|part| {
                ["answer_question", "decline_question"]
                    .into_iter()
                    .find_map(|verb| part.get(verb).map(|arg| (verb, arg)))
            });
            for (n, (verb, arg)) in actions.enumerate() {
                let name = path.display();
                // Answers go to the scripted calls in order; a late answer goes to the last one.
                let question = asked
                    .get(n)
                    .or(asked.last())
                    .unwrap_or_else(|| panic!("{name}: `{verb}` and no scripted {QUESTION_TOOL} call"));
                let why = |f: Failure| match f {
                    Failure::Text(t) => t,
                    Failure::Host(e) => format!("{e:?}"),
                };
                let action = question_action_of(verb, arg).unwrap_or_else(|f| panic!("{name}: {}", why(f)));
                check_against(verb, &action.answers, question).unwrap_or_else(|f| panic!("{name}: {}", why(f)));
                checked += 1;
            }
        }
        assert!(checked > 0, "the scenario files script question answers");
    }

    #[test]
    fn an_answer_needs_a_question_still_waiting() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), questions_host());
        let part = json!([{"answer_question": {"picks": [[0]]}}]);
        let err = drive(&mut host, &endpoint, &run, part.clone()).unwrap_err();
        assert!(err.contains("no AskUserQuestion request of the CLI waits"), "{err}");
        asked(&run, true);
        let err = drive(&mut host, &endpoint, &run, part).unwrap_err();
        assert!(err.contains("no AskUserQuestion request of the CLI waits"), "{err}");
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_question_the_gate_or_anything_else_answered_fails_the_answer() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), questions_host());
        asked(&run, false);
        host.cards.borrow_mut().push(Answered {
            request: Request {
                permission_id: "p1".into(),
                tool_use_id: Some("toolu_q1".into()),
                tool_name: "AskUserQuestion".into(),
                input: json!({}),
                origin: Origin::Hook,
            },
            decision: Some(PermissionDecision::Allow),
        });
        let part = json!([{"answer_question": {"picks": [[0]]}}]);
        let err = drive(&mut host, &endpoint, &run, part.clone()).unwrap_err();
        assert!(err.contains("went through the permission gate"), "{err}");

        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), questions_host());
        asked(&run, false);
        std::fs::create_dir_all(run.join("standin")).unwrap();
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "go"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_q1", "name": "AskUserQuestion", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_q1", "content": "answered"}]}
        ]});
        std::fs::write(run.join("standin/body_3.json"), body.to_string()).unwrap();
        *endpoint.records.lock().unwrap() = vec![record(3, Some("m2"), None, None)];
        let err = drive(&mut host, &endpoint, &run, part).unwrap_err();
        assert!(
            err.contains("request #3") && err.contains("before the driver answered"),
            "{err}"
        );
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_late_answer_passes_only_on_the_refusal_it_names() {
        let late = json!([{"answer_question": {"picks": [[0]], "refused": "INTERACTION_NOT_FOUND"}}]);
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), questions_host());
        asked(&run, true);
        host.refuse = Some("resolve_interaction: INTERACTION_NOT_FOUND".into());
        drive(&mut host, &endpoint, &run, late.clone()).unwrap();
        host.refuse = Some("resolve_interaction: INTERACTION_ANSWER_REFUSED".into());
        let err = drive(&mut host, &endpoint, &run, late.clone()).unwrap_err();
        assert!(err.contains("does not hold `INTERACTION_NOT_FOUND`"), "{err}");
        host.refuse = None;
        let err = drive(&mut host, &endpoint, &run, late).unwrap_err();
        assert!(err.contains("expects a refusal"), "{err}");
    }

    /// A tap where the CLI asked for the approval of one plan (`ExitPlanMode`'s permission callback),
    /// `answered` saying whether its control response went back.
    fn plan_asked(run: &Path, answered: bool) {
        tap(
            run,
            "cli-stdout.jsonl",
            &[(
                4,
                json!({"type": "control_request", "request_id": "req-p1", "request": {
                "subtype": "can_use_tool", "tool_name": "ExitPlanMode", "tool_use_id": "toolu_exit",
                "input": {"plan": "# Plan"}}}),
            )],
        );
        let mut stdin = vec![(0, json!({"type": "user", "message": {"role": "user", "content": "go"}}))];
        if answered {
            stdin.push((
                6,
                json!({"type": "control_response", "response": {"subtype": "success", "request_id": "req-p1", "response": {}}}),
            ));
        }
        tap(run, "cli-stdin.jsonl", &stdin);
    }

    fn plans_host() -> Fake {
        let mut host = Fake::new();
        host.caps.insert(PLAN_MODE);
        host
    }

    #[test]
    fn a_plan_decision_reaches_the_host_as_the_choice_and_its_feedback() {
        for (part, want) in [
            (json!({"choice": "approve_manual"}), "resolve_plan ApproveManual"),
            (json!({"choice": "approve_auto"}), "resolve_plan ApproveAuto"),
            (json!({"choice": "approve_bypass"}), "resolve_plan ApproveBypass"),
            (
                json!({"choice": "keep", "feedback": "also the README"}),
                "resolve_plan Keep { feedback: \"also the README\" }",
            ),
            (json!({"choice": "keep"}), "resolve_plan Keep { feedback: \"\" }"),
        ] {
            let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), plans_host());
            plan_asked(&run, false);
            drive(&mut host, &endpoint, &run, json!([{ "resolve_plan": part }])).unwrap();
            assert_eq!(host.calls.lock().unwrap().as_slice(), [want], "{part}");
        }
    }

    #[test]
    fn a_plan_decision_that_is_malformed_fails_before_the_host() {
        for (part, word) in [
            (json!({}), "choice is keep, approve_manual"),
            (json!({"choice": "approve"}), "choice is keep, approve_manual"),
            (json!({"choice": "approve_auto", "feedback": "x"}), "only with `keep`"),
            (json!({"choice": "keep", "feedback": 3}), "`feedback` is a text"),
            (json!({"choice": "keep", "mode": "auto"}), "unknown key `mode`"),
            (json!({"choice": "keep", "refused": ""}), "`refused` is the text"),
        ] {
            let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), plans_host());
            plan_asked(&run, false);
            let err = drive(&mut host, &endpoint, &run, json!([{ "resolve_plan": part }])).unwrap_err();
            assert!(err.contains(word), "{part}: {err}");
            assert!(host.calls.lock().unwrap().is_empty(), "{part}: the host was called");
        }
    }

    #[test]
    fn a_plan_decision_needs_a_plan_still_waiting() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), plans_host());
        let part = json!([{"resolve_plan": {"choice": "approve_manual"}}]);
        let err = drive(&mut host, &endpoint, &run, part.clone()).unwrap_err();
        assert!(err.contains("no ExitPlanMode request of the CLI waits"), "{err}");
        plan_asked(&run, true);
        let err = drive(&mut host, &endpoint, &run, part).unwrap_err();
        assert!(err.contains("no ExitPlanMode request of the CLI waits"), "{err}");
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_plan_the_gate_or_anything_else_answered_fails_the_decision() {
        let part = json!([{"resolve_plan": {"choice": "approve_manual"}}]);
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), plans_host());
        plan_asked(&run, false);
        host.cards.borrow_mut().push(Answered {
            request: Request {
                permission_id: "p1".into(),
                tool_use_id: Some("toolu_exit".into()),
                tool_name: "ExitPlanMode".into(),
                input: json!({}),
                origin: Origin::Hook,
            },
            decision: Some(PermissionDecision::Allow),
        });
        let err = drive(&mut host, &endpoint, &run, part.clone()).unwrap_err();
        assert!(
            err.contains("the ExitPlanMode call went through the permission gate"),
            "{err}"
        );

        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), plans_host());
        plan_asked(&run, false);
        std::fs::create_dir_all(run.join("standin")).unwrap();
        let body = json!({"messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_exit", "name": "ExitPlanMode", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_exit", "content": "approved"}]}
        ]});
        std::fs::write(run.join("standin/body_3.json"), body.to_string()).unwrap();
        *endpoint.records.lock().unwrap() = vec![record(3, Some("m2"), None, None)];
        let err = drive(&mut host, &endpoint, &run, part).unwrap_err();
        assert!(
            err.contains("request #3") && err.contains("ExitPlanMode call's tool_result before the driver answered"),
            "{err}"
        );
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_late_plan_decision_passes_only_on_the_refusal_it_names() {
        let late = json!([{"resolve_plan": {"choice": "keep", "refused": "INTERACTION_NOT_FOUND"}}]);
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), plans_host());
        plan_asked(&run, true);
        host.refuse = Some("resolve_plan: INTERACTION_NOT_FOUND".into());
        drive(&mut host, &endpoint, &run, late.clone()).unwrap();
        host.refuse = Some("resolve_plan: INTERACTION_ANSWER_REFUSED".into());
        let err = drive(&mut host, &endpoint, &run, late.clone()).unwrap_err();
        assert!(err.contains("does not hold `INTERACTION_NOT_FOUND`"), "{err}");
        host.refuse = None;
        let err = drive(&mut host, &endpoint, &run, late).unwrap_err();
        assert!(err.contains("expects a refusal"), "{err}");
    }

    #[test]
    fn the_plan_verbs_need_the_capability_a_host_declares() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        for part in [
            json!({"set_planning": {"enter": true}}),
            json!({"resolve_plan": {"choice": "keep"}}),
            json!({"projection": {"plan": "waiting"}}),
            json!({"projection": {"planning": "planning"}}),
            json!({"interaction": {"plan_outcome": "approved"}}),
        ] {
            let err = drive(&mut host, &endpoint, &run, json!([part.clone()])).unwrap_err();
            assert!(err.contains(PLAN_MODE) && err.contains("requires"), "{part}: {err}");
            let mut required = part.clone();
            required["requires"] = json!(PLAN_MODE);
            let report = drive(&mut host, &endpoint, &run, json!([required])).unwrap();
            assert_eq!(report.pending.len(), 1, "{part}");
        }
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn set_planning_and_the_plan_waits_take_exactly_their_shapes() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), plans_host());
        host.plan = true;
        drive(
            &mut host,
            &endpoint,
            &run,
            json!([
                {"set_planning": {"enter": true}}, {"set_planning": {"enter": false}}, {"set_mode": "auto"},
                {"projection": {"plan": "waiting"}}, {"projection": {"plan": "settled"}},
                {"projection": {"planning": "planning"}}, {"projection": {"planning": "executing"}},
                {"interaction": {"plan_outcome": "approved", "mode_after": "auto"}}
            ]),
        )
        .unwrap();
        assert_eq!(
            host.calls.lock().unwrap().as_slice(),
            ["set_planning true", "set_planning false", "set_mode auto"]
        );
        for (part, word) in [
            (json!({"set_planning": true}), "needs `enter`"),
            (
                json!({"set_mode": "plan"}),
                "`set_mode` is `manual`, `auto` or `bypass`",
            ),
            (json!({"set_planning": {"enter": true, "x": 1}}), "takes only `enter`"),
            (
                json!({"projection": {"plan": "answering"}}),
                "projection` on plans is exactly",
            ),
            (
                json!({"projection": {"planning": "entering"}}),
                "projection` on plans is exactly",
            ),
            (
                json!({"projection": {"plan": "waiting", "planning": "planning"}}),
                "projection` on plans is exactly",
            ),
        ] {
            let err = drive(&mut host, &endpoint, &run, json!([part.clone()])).unwrap_err();
            assert!(err.contains(word), "{part}: {err}");
        }
    }

    #[test]
    fn a_task_output_that_states_what_it_must_hold_fails_when_the_value_does_not() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.caps.insert("task_output");
        let stated = json!([{"task_output": {"task": "t1", "min_bytes": 40, "contains": "MARK"}}]);
        host.output = String::new();
        let err = drive(&mut host, &endpoint, &run, stated.clone()).unwrap_err();
        assert!(err.contains("returned 0 bytes") && err.contains("at least 40"), "{err}");
        host.output = "x".repeat(40);
        let err = drive(&mut host, &endpoint, &run, stated.clone()).unwrap_err();
        assert!(err.contains("do not hold the text"), "{err}");
        assert!(!err.contains("xxxx"), "the output itself is not repeated: {err}");
        host.output = format!("{}MARK", "x".repeat(40));
        drive(&mut host, &endpoint, &run, stated).unwrap();
        // A task_output that states nothing takes whatever the host returned.
        host.output = String::new();
        drive(&mut host, &endpoint, &run, json!([{"task_output": "t1"}])).unwrap();
    }

    #[test]
    fn a_task_output_passes_only_on_the_refusal_it_names() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.caps.insert("task_output");
        drive(&mut host, &endpoint, &run, json!([{"task_output": "t1"}])).unwrap();
        let refused = json!([{"task_output": {"task": "t1", "refused": "no readable output"}}]);
        let err = drive(&mut host, &endpoint, &run, refused.clone()).unwrap_err();
        assert!(err.contains("expects a refusal"), "{err}");
        host.refuse = Some("task_output: no readable output for background task t1".into());
        drive(&mut host, &endpoint, &run, refused).unwrap();
        let other = json!([{"task_output": {"task": "t1", "refused": "not found"}}]);
        let err = drive(&mut host, &endpoint, &run, other).unwrap_err();
        assert!(err.contains("does not hold `not found`"), "{err}");
        let err = drive(&mut host, &endpoint, &run, json!([{"task_output": "t1"}])).unwrap_err();
        assert!(err.contains("no readable output"), "{err}");
    }

    #[test]
    fn a_decline_takes_cancel_or_chat_and_only_a_chat_sends_answers_so_far() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), questions_host());
        asked(&run, false);
        drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"decline_question": {"kind": "chat", "picks": [[1]]}}, {"decline_question": {"kind": "cancel"}}]),
        )
        .unwrap();
        let so_far = vec![QuestionAnswer {
            question: 0,
            options: vec![1],
            other: None,
            notes: None,
        }];
        assert_eq!(
            host.calls.lock().unwrap().as_slice(),
            [format!("decline Chat {so_far:?}"), "decline Cancel []".to_owned()]
        );
        for (part, word) in [
            (json!({"kind": "cancel", "picks": [[0]]}), "takes no answers"),
            (json!({"kind": "dismiss"}), "cancel or chat"),
            (json!({"kind": "chat", "picks": [[5]]}), "option 5 of question 0"),
        ] {
            let err = drive(&mut host, &endpoint, &run, json!([{ "decline_question": part }])).unwrap_err();
            assert!(err.contains(word), "{part}: {err}");
        }
    }

    #[test]
    fn a_question_wait_asks_the_host_question_records_and_takes_two_words() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        let err = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"projection": {"question": "waiting"}}]),
        )
        .unwrap_err();
        assert!(
            err.contains("structured_questions_v1") && err.contains("requires"),
            "{err}"
        );
        let report = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"projection": {"question": "settled"}, "requires": "structured_questions_v1"}]),
        )
        .unwrap();
        assert_eq!(
            report.pending,
            ["PENDING fake/0-projection: needs structured_questions_v1"]
        );
        host.caps.insert("structured_questions_v1");
        host.interaction = true;
        drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"projection": {"question": "waiting"}}]),
        )
        .unwrap();
        for bad in [
            json!({"question": "answered"}),
            json!({"question": "waiting", "count": 2}),
        ] {
            let err = drive(&mut host, &endpoint, &run, json!([{ "projection": bad }])).unwrap_err();
            assert!(err.contains("exactly"), "{bad}: {err}");
        }
    }

    fn a_call(tool: &str) -> ClientCallSeen {
        ClientCallSeen {
            call_id: format!("call-{tool}"),
            tool_use_id: Some(format!("toolu_{tool}")),
            tool: tool.into(),
        }
    }

    #[test]
    fn client_waits_and_answers_reach_the_host_by_bare_tool_name() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.client_calls = vec![a_call("open_file")];
        host.open = 1;
        drive(
            &mut host,
            &endpoint,
            &run,
            json!([
                {"client_call": "mcp__eitri__open_file"},
                {"client_call": {"tool": "open_file", "count": 1}},
                {"client_state": {"open": 1, "tool": "mcp__eitri__open_file"}},
                {"client_answer": {"text": "shown", "tool": "open_file"}},
                {"client_answer": {"fill": {"unit": "abc\n", "bytes": 10}, "is_error": true}},
                {"client_state": {"open": 0}}
            ]),
        )
        .unwrap();
        assert_eq!(
            host.calls.lock().unwrap().join("|"),
            "client_answer Some(\"open_file\") 5 bytes error=false|client_answer None 10 bytes error=true"
        );
    }

    #[test]
    fn a_client_call_wait_for_a_tool_never_called_times_out() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.client_calls = vec![a_call("editor_state")];
        let err = drive(&mut host, &endpoint, &run, json!([{"client_call": "open_file"}])).unwrap_err();
        assert!(err.contains("timed out") && err.contains("client_call"), "{err}");
    }

    #[test]
    fn a_client_state_wait_holds_only_for_the_exact_count() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.client_calls = vec![a_call("open_file"), a_call("diagnostics")];
        host.open = 2;
        let err = drive(&mut host, &endpoint, &run, json!([{"client_state": {"open": 1}}])).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn the_three_steps_of_a_move_reach_the_host_in_order() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"freeze": true}, {"adopt": true}, {"commit": true}]),
        )
        .unwrap();
        assert_eq!(host.calls.lock().unwrap().join("|"), "freeze|adopt|commit");
    }

    #[test]
    fn a_part_for_the_other_layer_is_skipped_without_a_pending_line() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.layer = "b";
        let report = drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"send": "only a", "layer": "a"}, {"send": "only b", "layer": "b"}, {"send": "both"}]),
        )
        .unwrap();
        assert_eq!(host.calls.lock().unwrap().join("|"), "send only b|send both");
        assert_eq!((report.parts_run, report.pending.len()), (2, 0));
    }

    #[test]
    fn an_answer_with_no_text_or_with_a_unit_that_is_not_ascii_fails_before_the_host() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        for part in [
            json!({"client_answer": {}}),
            json!({"client_answer": {"fill": {"unit": "é", "bytes": 4}}}),
        ] {
            let err = drive(&mut host, &endpoint, &run, json!([part])).unwrap_err();
            assert!(err.contains("client_answer"), "{err}");
        }
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_part_needs_one_known_verb() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        for (part, word) in [
            (json!({"send": "a", "steer": "b"}), "exactly one verb"),
            (json!({"name": "x"}), "exactly one verb"),
            (json!({"send": "a", "bogus": 1}), "unknown key `bogus`"),
            (json!("send"), "not an object"),
        ] {
            let err = drive(&mut host, &endpoint, &run, json!([part])).unwrap_err();
            assert!(err.contains(word), "{err}");
        }
    }

    #[test]
    fn field_matching_follows_dotted_paths_and_the_two_operators() {
        let v = json!({"a": {"b": [10, {"c": "hello world"}]}, "n": 1});
        assert!(fields_match(&v, &json!({"a.b.0": 10, "n": 1})));
        assert!(fields_match(&v, &json!({"a.b.1.c": {"contains": "lo wo"}})));
        assert!(fields_match(
            &v,
            &json!({"zzz": {"exists": false}, "n": {"exists": true}})
        ));
        assert!(!fields_match(&v, &json!({"n": 2})));
        assert!(!fields_match(&v, &json!({"a.b.9": {"exists": true}})));
        assert!(fields_match(&v, &Value::Null));
    }

    /// A question the CLI withdrew (`control_cancel_request`), or one a process that has since been replaced
    /// asked (a later `initialize` on stdin), no longer waits; only the live process's open question does.
    #[test]
    fn a_withdrawn_question_or_one_from_a_closed_process_no_longer_waits() {
        let run = run_dir();
        let log = run.join("log");
        let init =
            |id: &str| json!({"type": "control_request", "request_id": id, "request": {"subtype": "initialize"}});
        let ask = |id: &str, call: &str| {
            json!({"type": "control_request", "request_id": id, "request": {
                "subtype": "can_use_tool", "tool_name": QUESTION_TOOL, "tool_use_id": call,
                "input": {"questions": [{"question": format!("{call}?")}]}
            }})
        };
        tap(&run, "cli-stdin.jsonl", &[(0, init("i1"))]);
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                (1, ask("q1", "toolu_old")),
                (2, ask("q2", "toolu_cancelled")),
                (3, json!({"type": "control_cancel_request", "request_id": "q2"})),
            ],
        );
        let waiting = |log: &Path| -> Vec<String> {
            asked_questions(log)
                .into_iter()
                .filter(|q| !q.settled)
                .filter_map(|q| q.tool_use_id)
                .collect()
        };
        assert_eq!(waiting(&log), ["toolu_old"], "a withdrawn question does not wait");
        assert!(question_waits(&log));

        tap(&run, "cli-stdin.jsonl", &[(0, init("i1")), (4, init("i2"))]);
        assert!(waiting(&log).is_empty(), "the closed process's question does not wait");
        assert!(!question_waits(&log));

        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                (1, ask("q1", "toolu_old")),
                (2, ask("q2", "toolu_cancelled")),
                (3, json!({"type": "control_cancel_request", "request_id": "q2"})),
                (5, ask("q3", "toolu_live")),
            ],
        );
        assert_eq!(waiting(&log), ["toolu_live"]);
        let _ = std::fs::remove_dir_all(&run);
    }
}
