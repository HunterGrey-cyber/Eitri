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
//! | `send` | text, or `{text, typed}` |
//! | `steer`, `queue`, `slash` | text |
//! | `interrupt`, `approve_sweep`, `close`, `resume` | anything |
//! | `answer_card` | `{tool, allow}` (allow defaults to true) |
//! | `open_gate` | gate name |
//! | `release` | hold name |
//! | `stop_task`, `task_output` | task id |
//! | `set_per_task_stop` | bool |
//! | `answer_question` | `{picks: [[label...]...], other?: {index: text}, notes?: text}` |
//! | `decline_question` | `{kind: "cancel"\|"chat"}` |
//! | `request`, `request_done` | step id or side kind, or `{id, count}` |
//! | `held` | hold name |
//! | `tap_line` | `{kind, fields?, count?}`; `kind` is a stdout line's `type` or `type/subtype` |
//! | `cli_results` | how many `result` lines at least |
//! | `session_state` | `idle`, `running` or `requires_action`: the newest state line since the last action |
//! | `transcript_line` | `{type, fields?, count?}` over the CLI's own session files |
//! | `projection`, `interaction` | an object the host judges |
//! | `roster` | `{task, status}` |
//! | `card` | `{tool}`: a request of that tool needed (or got) a driver answer |
//! | `wall_floor` | `{after, seconds}`: `after` is a part's `name` or `<verb>:<target>` of an earlier wait |
//!
//! String values may hold `{name}` captures of the scripted requests and `{RUN}`.

use super::{gate_answer::Answered, script::Expander, RequestRecord, ScriptedModel};
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
    pub status: String,
}

/// How a pending question is declined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclineKind {
    Cancel,
    Chat,
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
    /// Puts a message into the running turn.
    fn steer(&mut self, text: &str) -> Result<(), DriverError>;
    /// Sends a prompt while the CLI may be busy; the CLI queues it.
    fn queue(&mut self, text: &str) -> Result<(), DriverError>;
    fn interrupt(&mut self) -> Result<(), DriverError>;
    /// Answers the oldest waiting card of `tool`, or the next one that comes.
    fn answer_card(&mut self, tool: &str, allow: bool) -> Result<(), DriverError>;
    /// Approves every pending card ("approve all pending").
    fn approve_sweep(&mut self) -> Result<(), DriverError>;
    fn slash(&mut self, command: &str) -> Result<(), DriverError>;
    fn close(&mut self) -> Result<(), DriverError>;
    fn resume(&mut self) -> Result<(), DriverError>;

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
    fn answer_question(
        &mut self,
        picks: &[Vec<String>],
        other: &BTreeMap<usize, String>,
        notes: Option<&str>,
    ) -> Result<(), DriverError> {
        let _ = (picks, other, notes);
        Err(DriverError::CapabilityMissing("structured_questions_v1"))
    }
    fn decline_question(&mut self, kind: DeclineKind) -> Result<(), DriverError> {
        let _ = kind;
        Err(DriverError::CapabilityMissing("structured_questions_v1"))
    }
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
];
const META: &[&str] = &["requires", "name", "timeout"];
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
        }
    }

    /// Runs every part in order. The first failure ends the run with the part's label and the reason.
    pub fn run(&mut self, parts: &[Value]) -> Result<Report, String> {
        let mut report = Report::default();
        for (i, part) in parts.iter().enumerate() {
            let (verb, label) = self.describe(i, part)?;
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
                let now = Instant::now();
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
                Ok(self.host.send(&need("text")?, typed)?)
            }
            "steer" => Ok(self.host.steer(&need("text")?)?),
            "queue" => Ok(self.host.queue(&need("text")?)?),
            "slash" => Ok(self.host.slash(&need("command")?)?),
            "interrupt" => Ok(self.host.interrupt()?),
            "approve_sweep" => Ok(self.host.approve_sweep()?),
            "close" => Ok(self.host.close()?),
            "resume" => Ok(self.host.resume()?),
            "answer_card" => {
                let tool = text_arg(arg, "tool").ok_or_else(|| Failure::text("`answer_card` needs a tool"))?;
                Ok(self.host.answer_card(&tool, arg["allow"].as_bool().unwrap_or(true))?)
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
            "task_output" => self.host.task_output(&need("task")?).map(drop).map_err(Failure::from),
            "set_per_task_stop" => {
                let on = arg
                    .as_bool()
                    .ok_or_else(|| Failure::text("`set_per_task_stop` needs true or false"))?;
                Ok(self.host.set_per_task_stop(on)?)
            }
            "answer_question" => {
                let picks = arg["picks"]
                    .as_array()
                    .ok_or_else(|| Failure::text("`answer_question` needs `picks`: a list of lists of labels"))?
                    .iter()
                    .map(|q| {
                        q.as_array()
                            .map(|l| {
                                l.iter()
                                    .filter_map(|v| v.as_str().map(str::to_owned))
                                    .collect::<Vec<_>>()
                            })
                            .ok_or_else(|| Failure::text("`picks` holds a list of labels per question"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let mut other = BTreeMap::new();
                for (index, text) in arg["other"].as_object().into_iter().flatten() {
                    let index = index
                        .parse::<usize>()
                        .map_err(|_| Failure::text(format!("`other` key `{index}` is not a question number")))?;
                    other.insert(index, text.as_str().unwrap_or_default().to_owned());
                }
                Ok(self.host.answer_question(&picks, &other, arg["notes"].as_str())?)
            }
            "decline_question" => {
                let kind = match arg["kind"].as_str() {
                    Some("cancel") => DeclineKind::Cancel,
                    Some("chat") => DeclineKind::Chat,
                    other => {
                        return Err(Failure::text(format!(
                            "`decline_question` kind is cancel or chat, not {other:?}"
                        )))
                    }
                };
                Ok(self.host.decline_question(kind)?)
            }
            other => Err(Failure::text(format!("no action `{other}`"))),
        }
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
                self.wait_for(&what, limit, false, move |d| {
                    let n = d
                        .transcript_lines()
                        .iter()
                        .filter(|j| j["type"].as_str() == Some(&ty) && fields_match(j, &fields))
                        .count();
                    Ok(n >= count)
                })
            }
            "projection" => {
                let want = arg.clone();
                self.wait_for(&what, limit, false, move |d| {
                    d.host.projection_holds(&want).map_err(Failure::from)
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
                self.wait_for(&what, limit, false, move |_| {
                    Ok(start.elapsed().as_secs_f64() >= seconds)
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
        let end = Instant::now() + limit;
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
            if Instant::now() >= end {
                return Err(Failure::text(format!(
                    "timed out after {:.1}s waiting for {what}{}",
                    limit.as_secs_f64(),
                    self.report_tail()
                )));
            }
            std::thread::sleep(self.settings.poll);
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
    /// file grew.
    fn transcript_lines(&mut self) -> Vec<Value> {
        let root = self.settings.run.join("claude/projects");
        let mut files = Vec::new();
        for slug in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            for file in std::fs::read_dir(slug.path()).into_iter().flatten().flatten() {
                if file.path().extension().is_some_and(|e| e == "jsonl") {
                    files.push(file.path());
                }
            }
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

fn target_of(verb: &str, arg: &Value) -> Option<String> {
    match verb {
        "request" | "request_done" => text_arg(arg, "id"),
        "held" => text_arg(arg, "name"),
        "tap_line" | "card" => text_arg(arg, "kind").or_else(|| text_arg(arg, "tool")),
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
        fn steer(&mut self, text: &str) -> Result<(), DriverError> {
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
        fn roster(&self) -> Result<Vec<RosterEntry>, DriverError> {
            if self.caps.contains("background_tasks") {
                Ok(self.roster.clone())
            } else {
                Err(DriverError::CapabilityMissing("background_tasks"))
            }
        }
        fn interaction_holds(&self, _: &Value) -> Result<bool, DriverError> {
            if self.caps.contains("structured_questions_v1") {
                Ok(self.interaction)
            } else {
                Err(DriverError::CapabilityMissing("structured_questions_v1"))
            }
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

    fn run_dir() -> PathBuf {
        let run = std::env::temp_dir().join(format!("driver-{}", uuid::Uuid::new_v4()));
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
    fn projection_interaction_roster_and_card_waits_ask_the_host() {
        let (run, endpoint, mut host) = (run_dir(), FakeEndpoint::default(), Fake::new());
        host.projection = vec![json!({"type": "turn_started", "turn_id": "t1"})];
        host.caps.extend(["background_tasks", "structured_questions_v1"]);
        host.interaction = true;
        host.roster = vec![RosterEntry {
            task: "bg1".into(),
            status: "running".into(),
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
        let started = Instant::now();
        drive(
            &mut host,
            &endpoint,
            &run,
            json!([{"held": "h1", "name": "gate-up"}, {"wall_floor": {"after": "gate-up", "seconds": 0.15}}]),
        )
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(150));
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
        let started = Instant::now();
        let err = Driver::new(slow, &endpoint, &mut host)
            .run(&[json!({"held": "h1"})])
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "failed only at the deadline"
        );
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
}
