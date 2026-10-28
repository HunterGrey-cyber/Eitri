//! Scenario files, the steps of a model script, templates and captures, and the matching of a request to a step.
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use super::{
    matchers::{self, preload_input, Availability, Ctx, Matcher, View, PRELOAD_TOOL},
    Hold, HttpError, Point, Reply, Script,
};

/// The Eitri tab mode a scenario runs in.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Manual,
    Auto,
    Bypass,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub allow: bool,
    pub reason: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum VerdictRepr {
    Word(String),
    Full {
        allow: bool,
        #[serde(default)]
        reason: Option<String>,
    },
}

impl TryFrom<VerdictRepr> for Verdict {
    type Error = String;
    fn try_from(repr: VerdictRepr) -> Result<Self, String> {
        match repr {
            VerdictRepr::Word(w) if w == "allow" || w == "block" => Ok(Self {
                allow: w == "allow",
                reason: format!("scripted {w}"),
            }),
            VerdictRepr::Word(w) => Err(format!("verdict `{w}` is neither `allow` nor `block`")),
            VerdictRepr::Full { allow, reason } => Ok(Self {
                allow,
                reason: reason.unwrap_or_else(|| "scripted verdict".into()),
            }),
        }
    }
}

impl<'de> Deserialize<'de> for Verdict {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        VerdictRepr::deserialize(d)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

/// How the auto-mode classifier's requests are answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Classifier {
    /// An allow verdict for every request.
    DefaultAllow,
    /// These verdicts, in order.
    Verdicts(Vec<Verdict>),
}

impl<'de> Deserialize<'de> for Classifier {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Name(String),
            List(Vec<Verdict>),
        }
        match Repr::deserialize(d)? {
            Repr::Name(n) if n == "default_allow" => Ok(Self::DefaultAllow),
            Repr::Name(n) => Err(serde::de::Error::custom(format!(
                "classifier `{n}` is not `default_allow` or a list of verdicts"
            ))),
            Repr::List(v) => Ok(Self::Verdicts(v)),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    #[serde(default = "empty_object")]
    pub input: Value,
}

fn empty_object() -> Value {
    json!({})
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReplyBlock {
    Text(String),
    Thinking(String),
    ToolUse(ToolUse),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    /// `tool_result_for:<tool use id>`, `tail` or `first_user`.
    pub from: String,
    /// The first group is the value; a regex without groups captures its whole match.
    pub regex: String,
}

fn yes() -> bool {
    true
}
fn end_turn() -> String {
    "end_turn".into()
}

/// One step of a model script: when it applies and what it answers.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    #[serde(default)]
    pub when: Matcher,
    #[serde(default)]
    pub reply: Vec<ReplyBlock>,
    #[serde(default = "end_turn")]
    pub stop: String,
    #[serde(default)]
    pub capture: BTreeMap<String, Capture>,
    #[serde(default)]
    pub once: Option<bool>,
    #[serde(default)]
    pub repeat: bool,
    #[serde(default = "yes")]
    pub required: bool,
    #[serde(default)]
    pub after: Vec<String>,
    /// A step with `side: false` never takes a request that carries a side marker.
    #[serde(default)]
    pub side: bool,
    /// `<set>:<group>`, both declared under the scenario's `branches`.
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub preload: Vec<String>,
    /// Serve a call of a tool the request offers only as deferred, without the load exchange a model would make: the
    /// scenario checks what the CLI does with such a call.
    #[serde(default)]
    pub direct_deferred: bool,
    #[serde(default)]
    pub hold: Option<Hold>,
    #[serde(default)]
    pub cut: Option<Point>,
    #[serde(default)]
    pub chunks: Option<usize>,
    #[serde(default)]
    pub error: Option<HttpError>,
    /// Rate-limit headers on the answer that succeeds (after any `error` answers).
    #[serde(default)]
    pub ok_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub usage: Option<Value>,
}

impl Step {
    fn branch_parts(&self) -> Option<(&str, &str)> {
        self.branch.as_deref().and_then(|b| b.split_once(':'))
    }
}

// ---- Scenario files ----

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserSettings {
    #[serde(default)]
    pub hooks: Option<Value>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    #[serde(default)]
    pub model: Option<Vec<Step>>,
    #[serde(default)]
    pub driver: Option<Vec<Value>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub user_settings: Option<UserSettings>,
    #[serde(default)]
    pub requires: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub id: String,
    #[serde(default)]
    pub model: Vec<Step>,
    #[serde(default)]
    pub driver: Vec<Value>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub user_settings: Option<UserSettings>,
    #[serde(default)]
    pub project: BTreeMap<String, String>,
    #[serde(default)]
    pub mode: Option<Mode>,
    #[serde(default)]
    pub classifier: Option<Classifier>,
    #[serde(default)]
    pub branches: BTreeMap<String, Vec<String>>,
    #[serde(default = "default_layers")]
    pub layers: Vec<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub gate: bool,
    #[serde(default)]
    pub slow: bool,
    #[serde(default)]
    pub variants: BTreeMap<String, Variant>,
}

fn default_layers() -> Vec<String> {
    vec!["a".into()]
}

/// Capabilities a driver part or variant may require.
pub const KNOWN_CAPABILITIES: [&str; 7] = [
    "task_control",
    "task_output",
    "per_task_stop",
    "background_tasks",
    "human_origin",
    "structured_questions_v1",
    "eitri_bash_ceiling",
];

/// Where an environment name sits in a scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvPlace {
    Env,
    UserSettingsEnv,
}

fn id_in(id: &str, ids: &[&str]) -> bool {
    ids.contains(&id)
}

/// Whether a scenario (and variant) may carry this environment name. Any other name is refused.
pub fn check_env_name(id: &str, variant: Option<&str>, name: &str, place: EnvPlace) -> Result<(), String> {
    let allowed = match name {
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW"
        | "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"
        | "CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS" => true,
        // Raised or lowered only to check the time limit, never in an ordinary run.
        "BASH_DEFAULT_TIMEOUT_MS" | "BASH_MAX_TIMEOUT_MS" | "CLAUDE_CODE_AUTO_BACKGROUND_TIMEOUT_MS" => {
            (id == "c01t" && matches!(variant, Some("pre-fix-sim" | "user-env")))
                || (id == "c01" && variant == Some("h9b-settings") && place == EnvPlace::UserSettingsEnv)
        }
        "CLAUDE_CODE_WORKFLOWS" => {
            id_in(
                id,
                &[
                    "c00",
                    "c00_auto",
                    "c00_tools",
                    "c01",
                    "c01_auto",
                    "c16w",
                    "c16w_stop",
                    "c16w_manual",
                    "c17",
                ],
            ) || id.starts_with("probe_")
        }
        "CLAUDE_CODE_MAX_RETRIES" => id_in(id, &["c13a", "c13b"]),
        "CLAUDE_CODE_QUESTION_EXTENDED" => id == "c20x",
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        let where_ = match place {
            EnvPlace::Env => "env",
            EnvPlace::UserSettingsEnv => "user_settings.env",
        };
        let variant = variant.map(|v| format!(" variant `{v}`")).unwrap_or_default();
        Err(format!(
            "scenario `{id}`{variant}: {where_} name `{name}` is not allowed here"
        ))
    }
}

fn check_requires(names: &[String], what: &str) -> Result<(), String> {
    for n in names {
        if !KNOWN_CAPABILITIES.contains(&n.as_str()) {
            return Err(format!("{what}: `requires` names unknown capability `{n}`"));
        }
    }
    Ok(())
}

fn requires_of(part: &Value) -> Vec<String> {
    match &part["requires"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    }
}

impl Scenario {
    pub fn parse(text: &str) -> Result<Self, String> {
        let scenario: Self = serde_json::from_str(text).map_err(|e| format!("scenario file: {e}"))?;
        scenario.validate()?;
        Ok(scenario)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn validate(&self) -> Result<(), String> {
        let id = &self.id;
        if self.mode.is_none() {
            return Err(format!("scenario `{id}`: `mode` is required (manual, auto or bypass)"));
        }
        if self.layers.is_empty() || self.layers.iter().any(|l| l != "a" && l != "b") {
            return Err(format!("scenario `{id}`: `layers` holds only `a` and `b`"));
        }
        if let Some(p) = &self.priority {
            if !["p0", "p1", "p2"].contains(&p.as_str()) {
                return Err(format!("scenario `{id}`: `priority` is p0, p1 or p2"));
            }
        }
        for name in self.env.keys() {
            check_env_name(id, None, name, EnvPlace::Env)?;
        }
        if let Some(us) = &self.user_settings {
            for name in us.env.keys() {
                check_env_name(id, None, name, EnvPlace::UserSettingsEnv)?;
            }
        }
        for part in &self.driver {
            if !part.is_object() {
                return Err(format!("scenario `{id}`: every driver part is an object"));
            }
            check_requires(&requires_of(part), &format!("scenario `{id}` driver"))?;
        }
        check_branches(id, &self.branches)?;
        validate_steps(id, &self.model, &self.branches)?;
        for (name, v) in &self.variants {
            for env in v.env.keys() {
                check_env_name(id, Some(name), env, EnvPlace::Env)?;
            }
            if let Some(us) = &v.user_settings {
                for env in us.env.keys() {
                    check_env_name(id, Some(name), env, EnvPlace::UserSettingsEnv)?;
                }
            }
            check_requires(&v.requires, &format!("scenario `{id}` variant `{name}`"))?;
            for part in v.driver.iter().flatten() {
                check_requires(&requires_of(part), &format!("scenario `{id}` variant `{name}` driver"))?;
            }
            if let Some(steps) = &v.model {
                validate_steps(&format!("{id}/{name}"), steps, &self.branches)?;
            }
        }
        Ok(())
    }

    /// The environment the CLI gets from this scenario and variant.
    pub fn effective_env(&self, variant: Option<&str>) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        if let Some(v) = variant.and_then(|v| self.variants.get(v)) {
            env.extend(v.env.clone());
        }
        env
    }

    /// The model script of this scenario and variant, with the scenario's mode, classifier and branches.
    /// The key, log directory and model ids are the caller's to set.
    pub fn script(&self, variant: Option<&str>) -> Result<Script, String> {
        let steps = match variant {
            None => self.model.clone(),
            Some(name) => {
                let v = self
                    .variants
                    .get(name)
                    .ok_or_else(|| format!("scenario `{}` has no variant `{name}`", self.id))?;
                v.model.clone().unwrap_or_else(|| self.model.clone())
            }
        };
        Ok(Script {
            steps,
            branches: self.branches.clone(),
            mode: self.mode,
            classifier: self.classifier.clone(),
            ..Script::default()
        })
    }
}

/// What `eitri-claude-scripted serve` reads from `script.json`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptFile {
    #[serde(default)]
    pub model: Vec<Step>,
    #[serde(default)]
    pub branches: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub mode: Option<Mode>,
    #[serde(default)]
    pub classifier: Option<Classifier>,
    #[serde(default)]
    pub main_model: Option<String>,
    #[serde(default)]
    pub small_model: Option<String>,
    #[serde(default)]
    pub run_dir: Option<PathBuf>,
}

impl ScriptFile {
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: Self = serde_json::from_str(text).map_err(|e| format!("script file: {e}"))?;
        check_branches("script", &file.branches)?;
        validate_steps("script", &file.model, &file.branches)?;
        Ok(file)
    }

    /// The script this file describes; the key, log directory and census flag stay the caller's.
    pub fn into_script(self) -> Script {
        Script {
            steps: self.model,
            branches: self.branches,
            mode: self.mode,
            classifier: self.classifier,
            main_model: self.main_model,
            small_model: self.small_model,
            run_dir: self.run_dir,
            ..Script::default()
        }
    }
}

fn check_branches(id: &str, branches: &BTreeMap<String, Vec<String>>) -> Result<(), String> {
    for (set, groups) in branches {
        let unique: BTreeSet<_> = groups.iter().collect();
        if groups.len() < 2 || unique.len() != groups.len() {
            return Err(format!(
                "scenario `{id}`: branch set `{set}` needs two or more distinct groups"
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_steps(id: &str, steps: &[Step], branches: &BTreeMap<String, Vec<String>>) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    for step in steps {
        let sid = &step.id;
        if sid.is_empty() || !ids.insert(sid.as_str()) {
            return Err(format!("scenario `{id}`: step id `{sid}` is empty or repeated"));
        }
    }
    for step in steps {
        let sid = &step.id;
        if step.once == Some(true) && step.repeat {
            return Err(format!("scenario `{id}` step `{sid}`: `once` and `repeat` contradict"));
        }
        for a in &step.after {
            if !ids.contains(a.as_str()) {
                return Err(format!(
                    "scenario `{id}` step `{sid}`: `after` names unknown step `{a}`"
                ));
            }
        }
        if let Some(b) = &step.branch {
            let ok = step
                .branch_parts()
                .is_some_and(|(set, group)| branches.get(set).is_some_and(|g| g.iter().any(|x| x == group)));
            if !ok {
                return Err(format!(
                    "scenario `{id}` step `{sid}`: branch `{b}` is not `<set>:<group>` of a declared set"
                ));
            }
        }
        if step.preload.iter().any(String::is_empty) {
            return Err(format!("scenario `{id}` step `{sid}`: `preload` holds an empty name"));
        }
        if !["end_turn", "tool_use", "max_tokens", "stop_sequence"].contains(&step.stop.as_str()) {
            return Err(format!("scenario `{id}` step `{sid}`: unknown `stop` `{}`", step.stop));
        }
        for (name, capture) in &step.capture {
            let from = capture.from.as_str();
            if !(from == "tail"
                || from == "first_user"
                || from.strip_prefix("tool_result_for:").is_some_and(|i| !i.is_empty()))
            {
                return Err(format!(
                    "scenario `{id}` step `{sid}`: capture `{name}` has an unknown `from`"
                ));
            }
            Regex::new(&capture.regex).map_err(|e| format!("scenario `{id}` step `{sid}`: capture `{name}`: {e}"))?;
        }
        // A reply the transport would refuse is refused here, with the step's name.
        let probe = build_reply(step, &Expander::new(None, &BTreeMap::new(), 0))
            .map_err(|e| format!("scenario `{id}` step `{sid}`: {e}"))?;
        super::validate(&probe).map_err(|e| format!("scenario `{id}` step `{sid}`: {e}"))?;
    }
    Ok(())
}

// ---- Templates ----

/// Expands `{RUN}`, `{GATE name}`, `{CRON_NEXT_MINUTE +N}` and captured `{name}` values.
pub struct Expander<'a> {
    run_dir: Option<&'a Path>,
    vars: &'a BTreeMap<String, String>,
    now: u64,
}

impl<'a> Expander<'a> {
    /// `now` is the stand-in's clock in seconds since the epoch (UTC).
    pub fn new(run_dir: Option<&'a Path>, vars: &'a BTreeMap<String, String>, now: u64) -> Self {
        Self { run_dir, vars, now }
    }

    fn run(&self) -> String {
        self.run_dir.map_or_else(|| "$RUN".into(), |d| d.display().to_string())
    }

    pub fn expand(&self, text: &str) -> String {
        let text = matchers::subst_vars(text, self.vars);
        let text = text.replace("{RUN}", &self.run());
        let gate = Regex::new(r"\{GATE ([^{}\s]+)\}").unwrap();
        let text = gate
            .replace_all(&text, |c: &regex::Captures| {
                format!("\"{}/gates/wait\" {}", self.run(), &c[1])
            })
            .into_owned();
        let cron = Regex::new(r"\{CRON_NEXT_MINUTE \+(\d+)\}").unwrap();
        cron.replace_all(&text, |c: &regex::Captures| {
            cron_next_minute(self.now, c[1].parse().unwrap_or(0))
        })
        .into_owned()
    }

    /// Every string inside `v` expanded.
    pub fn value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.expand(s)),
            Value::Array(items) => Value::Array(items.iter().map(|i| self.value(i)).collect()),
            Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), self.value(v))).collect()),
            other => other.clone(),
        }
    }
}

/// A five-field cron expression for the first whole minute at least `plus` seconds after `now` (UTC), with the
/// day, month and minute written out and the weekday left open.
pub fn cron_next_minute(now: u64, plus: u64) -> String {
    let target = (now + plus).div_ceil(60) * 60;
    let days = (target / 86_400) as i64;
    let secs = target % 86_400;
    // Civil date from days since 1970-01-01 (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let _year = yoe + era * 400 + i64::from(month <= 2);
    format!("{} {} {day} {month} *", secs % 3_600 / 60, secs / 3_600)
}

fn build_reply(step: &Step, ex: &Expander) -> Result<Reply, String> {
    let content = step
        .reply
        .iter()
        .map(|b| match b {
            ReplyBlock::Text(t) => json!({"type":"text","text":ex.expand(t)}),
            ReplyBlock::Thinking(t) => json!({"type":"thinking","thinking":ex.expand(t)}),
            ReplyBlock::ToolUse(u) => {
                json!({"type":"tool_use","id":ex.expand(&u.id),"name":u.name,"input":ex.value(&u.input)})
            }
        })
        .collect();
    let mut reply = Reply::text("");
    reply.id = step.id.clone();
    reply.content = content;
    reply.stop_reason = step.stop.clone();
    if let Some(usage) = &step.usage {
        reply.usage = usage.clone();
    }
    if let Some(chunks) = step.chunks {
        reply.chunks = chunks;
    }
    reply.cut = step.cut.clone();
    reply.hold = step.hold.clone();
    reply.error = step.error.clone();
    reply.ok_headers = step.ok_headers.clone();
    Ok(reply)
}

// ---- Matching ----

/// Model ids and clock inputs a selection needs.
pub(super) struct Cfg<'a> {
    pub main_model: Option<&'a str>,
    pub small_model: Option<&'a str>,
    pub census: bool,
    pub run_dir: Option<&'a Path>,
    pub now: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Preload {
    Pending,
    Awaiting(String),
    Done,
}

struct StepState {
    step: Step,
    used: usize,
    errors_left: usize,
    preload: Preload,
}

pub(super) struct Picked {
    pub step_id: String,
    pub reply: Reply,
}

#[derive(Default)]
pub(super) struct Runtime {
    steps: Vec<StepState>,
    branches: BTreeMap<String, Vec<String>>,
    pub vars: BTreeMap<String, String>,
}

impl Runtime {
    pub fn new(steps: Vec<Step>, branches: BTreeMap<String, Vec<String>>) -> Self {
        Self {
            steps: steps
                .into_iter()
                .map(|step| StepState {
                    errors_left: step.error.as_ref().map_or(0, |e| e.times),
                    preload: if step.preload.is_empty() {
                        Preload::Done
                    } else {
                        Preload::Pending
                    },
                    used: 0,
                    step,
                })
                .collect(),
            branches,
            vars: BTreeMap::new(),
        }
    }

    pub fn has_steps(&self) -> bool {
        !self.steps.is_empty()
    }

    pub fn uses(&self) -> BTreeMap<String, usize> {
        self.steps.iter().map(|s| (s.step.id.clone(), s.used)).collect()
    }

    /// For each branch set, the groups with a used step.
    pub fn branches_used(&self) -> BTreeMap<String, Vec<String>> {
        self.branches
            .iter()
            .map(|(set, groups)| {
                let used = groups
                    .iter()
                    .filter(|g| {
                        self.steps
                            .iter()
                            .any(|s| s.used > 0 && s.step.branch_parts() == Some((set.as_str(), g.as_str())))
                    })
                    .cloned()
                    .collect();
                (set.clone(), used)
            })
            .collect()
    }

    /// Required steps never used, except those of a branch group that did not run.
    pub fn unused_required(&self) -> Vec<String> {
        let used = self.branches_used();
        self.steps
            .iter()
            .filter(|s| s.step.required && s.used == 0)
            .filter(|s| match s.step.branch_parts() {
                Some((set, group)) => used.get(set).is_some_and(|g| g.iter().any(|x| x == group)),
                None => true,
            })
            .map(|s| s.step.id.clone())
            .collect()
    }

    /// Branch sets that did not run exactly one group.
    pub fn branch_problems(&self) -> Vec<String> {
        self.branches_used()
            .into_iter()
            .filter(|(_, used)| used.len() != 1)
            .map(|(set, used)| {
                if used.is_empty() {
                    format!("branch set `{set}`: no group ran")
                } else {
                    format!("branch set `{set}`: groups {} all ran", used.join(", "))
                }
            })
            .collect()
    }

    fn candidate(&self, i: usize, view: &View, cfg: &Cfg, side_marker: bool) -> bool {
        let s = &self.steps[i];
        if s.used > 0 && !s.step.repeat {
            return false;
        }
        if s.step
            .after
            .iter()
            .any(|a| !self.steps.iter().any(|o| &o.step.id == a && o.used > 0))
        {
            return false;
        }
        if side_marker && !s.step.side {
            return false;
        }
        match &s.preload {
            Preload::Awaiting(id) => view.tool_result_ids().contains(&id.as_str()),
            _ => s.step.when.matches(
                view,
                &Ctx {
                    main_model: cfg.main_model,
                    small_model: cfg.small_model,
                    census: cfg.census,
                    vars: &self.vars,
                },
            ),
        }
    }

    /// The first step that takes this request, and its reply. `None`: no step applies. `Some(Err)`: a step applies
    /// but cannot be served as scripted; nothing is consumed.
    pub fn select(&mut self, view: &View, cfg: &Cfg, side_marker: bool) -> Option<Result<Picked, String>> {
        let i = (0..self.steps.len()).find(|&i| self.candidate(i, view, cfg, side_marker))?;
        Some(self.serve(i, view, cfg))
    }

    fn serve(&mut self, i: usize, view: &View, cfg: &Cfg) -> Result<Picked, String> {
        let step = self.steps[i].step.clone();
        // A repeating step runs once per request; a later request that still offers a preloaded tool as deferred
        // needs its own load exchange.
        if self.steps[i].preload == Preload::Done
            && step.repeat
            && step
                .preload
                .iter()
                .any(|name| view.declared().get(name) == Some(&Availability::Deferred))
        {
            self.steps[i].preload = Preload::Pending;
        }
        let awaiting = matches!(self.steps[i].preload, Preload::Awaiting(_));
        if !awaiting {
            self.capture(&step, view)?;
        }
        if self.steps[i].preload == Preload::Pending {
            require_tool(view, PRELOAD_TOOL, &[])?;
            let call = format!("toolu_preload_{}", step.id);
            let mut reply = Reply::text("");
            reply.id = format!("{}#preload", step.id);
            reply.content =
                vec![json!({"type":"tool_use","id":call,"name":PRELOAD_TOOL,"input":preload_input(&step.preload)})];
            reply.stop_reason = "tool_use".into();
            self.steps[i].preload = Preload::Awaiting(call);
            return Ok(Picked {
                step_id: reply.id.clone(),
                reply,
            });
        }
        let ex = Expander::new(cfg.run_dir, &self.vars, cfg.now);
        let mut reply = build_reply(&step, &ex)?;
        let preloaded: &[String] = if awaiting { &step.preload } else { &[] };
        for block in &reply.content {
            if block["type"] == "tool_use" {
                let name = block["name"].as_str().unwrap_or("");
                if step.direct_deferred && view.declared().get(name) == Some(&Availability::Deferred) {
                    continue;
                }
                require_tool(view, name, preloaded)?;
            }
        }
        let state = &mut self.steps[i];
        if let Some(spec) = reply.error.as_mut() {
            if state.errors_left > 0 {
                spec.times = state.errors_left;
                state.errors_left -= 1;
                return Ok(Picked {
                    step_id: step.id,
                    reply,
                });
            }
        }
        reply.error = None;
        state.used += 1;
        state.preload = Preload::Done;
        Ok(Picked {
            step_id: step.id,
            reply,
        })
    }

    fn capture(&mut self, step: &Step, view: &View) -> Result<(), String> {
        for (name, c) in &step.capture {
            let source = match c.from.as_str() {
                "tail" => view.tail_text(),
                "first_user" => view.first_user_text(),
                from => {
                    let id = matchers::subst_vars(from.strip_prefix("tool_result_for:").unwrap_or(""), &self.vars);
                    view.tool_result_text(&id)
                        .ok_or_else(|| format!("capture `{name}`: the request has no tool result for `{id}`"))?
                }
            };
            let re = Regex::new(&c.regex).map_err(|e| e.to_string())?;
            let found = re
                .captures(&source)
                .map(|m| {
                    m.get(1)
                        .or_else(|| m.get(0))
                        .map(|g| g.as_str().to_owned())
                        .unwrap_or_default()
                })
                .ok_or_else(|| format!("capture `{name}`: `{}` does not match the request", c.regex))?;
            self.vars.insert(name.clone(), found);
        }
        Ok(())
    }
}

/// A scripted call needs a tool the request offers, loaded or, after a preload, deferred.
fn require_tool(view: &View, name: &str, preloaded: &[String]) -> Result<(), String> {
    match view.declared().get(name) {
        None => Err(format!(
            "scripted tool {name} is not offered by the CLI in this request"
        )),
        Some(Availability::Deferred) if !preloaded.iter().any(|p| p == name) => {
            Err(format!("{name} is deferred in this request: add `preload`"))
        }
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> Result<Scenario, String> {
        Scenario::parse(&v.to_string())
    }

    fn scenario(id: &str, extra: Value) -> Value {
        let mut base = json!({"id": id, "mode": "bypass", "model": [], "driver": []});
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn cron_next_minute_rounds_up_to_the_first_whole_minute_at_least_n_seconds_ahead() {
        // 2026-10-28 12:00:54 UTC
        let base = 1_793_188_800 + 54;
        assert_eq!(cron_next_minute(base, 5), "1 12 28 10 *");
        assert_eq!(cron_next_minute(base + 2, 5), "2 12 28 10 *");
        // exactly on a minute boundary stays
        assert_eq!(cron_next_minute(base + 6, 0), "1 12 28 10 *");
        // crossing midnight and a month end
        assert_eq!(cron_next_minute(1_793_188_800 + 12 * 3_600 - 1, 1), "0 0 29 10 *");
        assert_eq!(cron_next_minute(1_793_491_199, 1), "0 0 1 11 *");
    }

    #[test]
    fn templates_expand_run_gate_cron_and_captures() {
        let mut vars = BTreeMap::new();
        vars.insert("bg1".to_string(), "bx9".to_string());
        let dir = PathBuf::from("/runs/a");
        let ex = Expander::new(Some(&dir), &vars, 1_793_188_800 + 54);
        assert_eq!(
            ex.expand("{GATE g1}; echo {bg1} {RUN}"),
            "\"/runs/a/gates/wait\" g1; echo bx9 /runs/a"
        );
        assert_eq!(ex.expand("{CRON_NEXT_MINUTE +5} C15B-FIRE"), "1 12 28 10 * C15B-FIRE");
        assert_eq!(ex.expand("{CRON_NEXT_MINUTE +5}"), ex.expand("{CRON_NEXT_MINUTE +5}"));
        let ex2 = Expander::new(Some(&dir), &vars, 1_793_188_800 + 56);
        assert_eq!(ex2.expand("{CRON_NEXT_MINUTE +5}"), "2 12 28 10 *");
        let bare = Expander::new(None, &vars, 0);
        assert_eq!(bare.expand("{GATE g}"), "\"$RUN/gates/wait\" g");
        assert_eq!(bare.expand("{\"a\": 1} {unknown}"), "{\"a\": 1} {unknown}");
    }

    #[test]
    fn the_loader_refuses_a_missing_mode_and_unknown_fields() {
        let e = Scenario::parse(r#"{"id":"c00","model":[]}"#).unwrap_err();
        assert!(e.contains("`mode` is required"), "{e}");
        let e = Scenario::parse(r#"{"id":"c00","mode":"bypass","surprise":1}"#).unwrap_err();
        assert!(e.contains("surprise"), "{e}");
        let e = Scenario::parse(r#"{"id":"c00","mode":"fast"}"#).unwrap_err();
        assert!(e.contains("fast"), "{e}");
        assert!(Scenario::parse(r#"{"id":"c00","mode":"bypass","model":[{"id":"a","bogus":1}]}"#).is_err());
        // A subagent's tool calls reach the classifier in any tab mode, so any mode may script one.
        assert!(parse(scenario("c00", json!({"classifier": "default_allow"}))).is_ok());
        assert!(parse(scenario(
            "c00_auto",
            json!({"mode": "auto", "classifier": "default_allow"})
        ))
        .is_ok());
    }

    #[test]
    fn the_loader_refuses_environment_names_off_the_list() {
        for (id, env) in [
            ("c00", json!({"ANTHROPIC_BASE_URL": "x"})),
            ("c00", json!({"BASH_MAX_TIMEOUT_MS": "1"})),
            ("c01t", json!({"BASH_MAX_TIMEOUT_MS": "1"})),
            ("c01", json!({"BASH_MAX_TIMEOUT_MS": "1"})),
            ("c05", json!({"CLAUDE_CODE_WORKFLOWS": "1"})),
            ("c05", json!({"CLAUDE_CODE_MAX_RETRIES": "0"})),
            ("c20", json!({"CLAUDE_CODE_QUESTION_EXTENDED": "1"})),
        ] {
            let e = parse(scenario(id, json!({"env": env}))).unwrap_err();
            assert!(e.contains("not allowed"), "{id} {env}: {e}");
        }
        for (id, env) in [
            ("c09", json!({"CLAUDE_CODE_AUTO_COMPACT_WINDOW": "1000"})),
            ("c09", json!({"CLAUDE_AUTOCOMPACT_PCT_OVERRIDE": "20"})),
            ("c16w", json!({"CLAUDE_CODE_WORKFLOWS": "1"})),
            ("c13a", json!({"CLAUDE_CODE_MAX_RETRIES": "2"})),
            ("c20x", json!({"CLAUDE_CODE_QUESTION_EXTENDED": "1"})),
            ("c04a", json!({"CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS": "1"})),
        ] {
            parse(scenario(id, json!({"env": env}))).unwrap_or_else(|e| panic!("{id} {env}: {e}"));
        }
    }

    #[test]
    fn the_time_limit_names_are_allowed_only_in_their_named_variants() {
        let var = |name: &str, env: Value, settings: Value| {
            let mut v = json!({"env": env});
            if !settings.is_null() {
                v["user_settings"] = settings;
            }
            let mut s = scenario(name, json!({}));
            s["variants"] = json!({ "VAR": v });
            s
        };
        let with_name = |mut s: Value, variant: &str| {
            let v = s["variants"]["VAR"].take();
            s["variants"] = json!({ variant: v });
            s
        };
        let env = json!({"BASH_MAX_TIMEOUT_MS": "2147483647"});
        for variant in ["pre-fix-sim", "user-env"] {
            parse(with_name(var("c01t", env.clone(), Value::Null), variant)).unwrap();
        }
        assert!(parse(with_name(var("c01t", env.clone(), Value::Null), "other")).is_err());
        assert!(parse(with_name(var("c01", env.clone(), Value::Null), "pre-fix-sim")).is_err());
        // h9b-settings: only in user_settings.env
        parse(with_name(
            var("c01", json!({}), json!({"env": env.clone()})),
            "h9b-settings",
        ))
        .unwrap();
        assert!(parse(with_name(var("c01", env.clone(), Value::Null), "h9b-settings")).is_err());
        assert!(parse(with_name(
            var("c01t", json!({}), json!({"env": env.clone()})),
            "h9b-settings"
        ))
        .is_err());
        // the scenario's own user_settings carries none of them
        assert!(parse(scenario("c01", json!({"user_settings": {"env": env}}))).is_err());
    }

    #[test]
    fn user_settings_hold_hooks_and_env_only() {
        parse(scenario(
            "c14",
            json!({"user_settings": {"hooks": {"UserPromptSubmit": []}}}),
        ))
        .unwrap();
        let e = parse(scenario("c14", json!({"user_settings": {"permissions": {}}}))).unwrap_err();
        assert!(e.contains("permissions"), "{e}");
        let e = parse(scenario("c14", json!({"user_settings": {"env": {"PATH": "/x"}}}))).unwrap_err();
        assert!(e.contains("user_settings.env"), "{e}");
    }

    #[test]
    fn steps_are_checked_at_load() {
        let step = |v: Value| scenario("c05", json!({"model": [v]}));
        assert!(parse(step(json!({"id": "a", "reply": [{"text": "hi"}]}))).is_ok());
        assert!(parse(scenario("c05", json!({"model": [{"id": "a"}, {"id": "a"}]}))).is_err());
        assert!(parse(step(json!({"id": "a", "after": ["nope"]}))).is_err());
        assert!(parse(step(json!({"id": "a", "once": true, "repeat": true}))).is_err());
        assert!(parse(step(json!({"id": "a", "branch": "s:x"}))).is_err());
        assert!(parse(step(
            json!({"id": "a", "capture": {"v": {"from": "elsewhere", "regex": "x"}}})
        ))
        .is_err());
        assert!(parse(step(
            json!({"id": "a", "capture": {"v": {"from": "tail", "regex": "("}}})
        ))
        .is_err());
        assert!(parse(step(
            json!({"id": "a", "reply": [{"tool_use": {"id": "t", "name": "Bash"}}], "stop": "nope"})
        ))
        .is_err());
        assert!(parse(step(
            json!({"id": "a", "hold": {"name": "h", "at": {"after_block": 3}}, "reply": [{"text": "x"}]})
        ))
        .is_err());
        let mut ok = scenario("c05", json!({"branches": {"order": ["a", "b"]}}));
        ok["model"] = json!([{"id": "x", "branch": "order:a"}, {"id": "y", "branch": "order:b"}]);
        parse(ok).unwrap();
        assert!(parse(scenario("c05", json!({"branches": {"order": ["a"]}}))).is_err());
    }

    #[test]
    fn requires_names_only_known_capabilities() {
        parse(scenario("c07b", json!({"driver": [{"requires": ["per_task_stop"]}]}))).unwrap();
        assert!(parse(scenario("c07b", json!({"driver": [{"requires": "made_up"}]}))).is_err());
    }

    // ---- selection ----

    fn runtime(steps: Value, branches: Value) -> Runtime {
        let steps: Vec<Step> = serde_json::from_value(steps).unwrap();
        let branches = serde_json::from_value(branches).unwrap();
        Runtime::new(steps, branches)
    }

    fn cfg() -> Cfg<'static> {
        Cfg {
            main_model: None,
            small_model: Some("m-small"),
            census: false,
            run_dir: None,
            now: 0,
        }
    }

    fn view(tail: Value) -> View {
        View::new(
            "/v1/messages",
            &json!({"model":"m","tools":[{"name":"Bash","input_schema":{}},{"name":"ToolSearch","input_schema":{}},
                {"name":"Monitor","defer_loading":true}],"messages":tail}),
        )
    }

    fn prompt(text: &str) -> View {
        view(json!([{"role":"user","content":text}]))
    }

    #[test]
    fn once_repeat_after_and_required_follow_the_options() {
        let mut rt = runtime(
            json!([
                {"id":"a","when":{"first_user_contains":"go"},"reply":[{"text":"A"}]},
                {"id":"b","when":{"first_user_contains":"go"},"after":["a"],"reply":[{"text":"B"}],"repeat":true},
                {"id":"c","when":{"first_user_contains":"never"},"reply":[{"text":"C"}]},
                {"id":"d","when":{"first_user_contains":"never"},"reply":[{"text":"D"}],"required":false},
            ]),
            json!({}),
        );
        let v = prompt("go");
        let ids: Vec<_> = (0..4)
            .map(|_| rt.select(&v, &cfg(), false).map(|p| p.unwrap().step_id))
            .collect();
        assert_eq!(
            ids,
            [Some("a".into()), Some("b".into()), Some("b".into()), Some("b".into())]
        );
        assert_eq!(rt.unused_required(), ["c"]);
        assert!(rt.select(&prompt("other"), &cfg(), false).is_none());
    }

    #[test]
    fn a_step_without_the_side_option_never_takes_a_request_with_a_side_marker() {
        let mut rt = runtime(
            json!([{"id":"a","reply":[{"text":"A"}]},{"id":"s","side":true,"reply":[{"text":"S"}]}]),
            json!({}),
        );
        assert_eq!(rt.select(&prompt("x"), &cfg(), true).unwrap().unwrap().step_id, "s");
        assert_eq!(rt.select(&prompt("x"), &cfg(), false).unwrap().unwrap().step_id, "a");
    }

    #[test]
    fn captures_feed_later_matchers_and_replies() {
        let mut rt = runtime(
            json!([
                {"id":"m2","when":{"tool_result_for":"toolu_bg1"},
                 "capture":{"bg1":{"from":"tool_result_for:toolu_bg1","regex":"with ID: ([A-Za-z0-9_-]+)\\."}},
                 "reply":[{"text":"STARTED {bg1}"}]},
                {"id":"m3","when":{"task_notification":{"contains":"{bg1}"}},
                 "reply":[{"tool_use":{"id":"toolu_w1","name":"Bash","input":{"command":"echo {bg1}"}}}],"stop":"tool_use"},
            ]),
            json!({}),
        );
        let started = view(
            json!([{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_bg1",
            "content":"Command running in background with ID: bx7. Output is being written to: /x"}]}]),
        );
        let p = rt.select(&started, &cfg(), false).unwrap().unwrap();
        assert_eq!(p.reply.content[0]["text"], "STARTED bx7");
        let wake = view(json!([{"role":"user","content":"<task-notification>bx7 done</task-notification>"}]));
        let p = rt.select(&wake, &cfg(), false).unwrap().unwrap();
        assert_eq!(p.reply.content[0]["input"]["command"], "echo bx7");
        let other = view(json!([{"role":"user","content":"<task-notification>bx8 done</task-notification>"}]));
        assert!(rt.select(&other, &cfg(), false).is_none());
    }

    #[test]
    fn branch_accounting_needs_exactly_one_group_and_ignores_the_group_that_did_not_run() {
        let steps = json!([
            {"id":"p","when":{"first_user_contains":"go"},"reply":[{"text":"P"}]},
            {"id":"a1","when":{"first_user_contains":"left"},"branch":"order:a","reply":[{"text":"A1"}]},
            {"id":"a2","when":{"first_user_contains":"left2"},"branch":"order:a","reply":[{"text":"A2"}]},
            {"id":"b1","when":{"first_user_contains":"right"},"branch":"order:b","reply":[{"text":"B1"}]},
        ]);
        let branches = json!({"order":["a","b"]});
        let mut rt = runtime(steps.clone(), branches.clone());
        assert_eq!(rt.branch_problems(), ["branch set `order`: no group ran"]);
        rt.select(&prompt("go"), &cfg(), false).unwrap().unwrap();
        rt.select(&prompt("right"), &cfg(), false).unwrap().unwrap();
        assert!(rt.branch_problems().is_empty());
        assert_eq!(rt.branches_used()["order"], ["b"]);
        // a1 and a2 belong to the group that did not run
        assert!(rt.unused_required().is_empty());

        let mut rt = runtime(steps.clone(), branches.clone());
        rt.select(&prompt("left"), &cfg(), false).unwrap().unwrap();
        assert_eq!(rt.unused_required(), ["p", "a2"]);

        let mut rt = runtime(steps, branches);
        rt.select(&prompt("left"), &cfg(), false).unwrap().unwrap();
        rt.select(&prompt("right"), &cfg(), false).unwrap().unwrap();
        assert_eq!(rt.branch_problems(), ["branch set `order`: groups a, b all ran"]);
    }

    #[test]
    fn a_tool_the_request_does_not_offer_fails_loudly_and_consumes_nothing() {
        let mut rt = runtime(
            json!([{"id":"a","reply":[{"tool_use":{"id":"t","name":"AskUserQuestion","input":{}}}],"stop":"tool_use"},
                   {"id":"b","reply":[{"tool_use":{"id":"t","name":"Monitor","input":{}}}],"stop":"tool_use"}]),
            json!({}),
        );
        let e = rt.select(&prompt("x"), &cfg(), false).unwrap().err().unwrap();
        assert_eq!(
            e,
            "scripted tool AskUserQuestion is not offered by the CLI in this request"
        );
        assert_eq!(rt.uses()["a"], 0);
    }

    #[test]
    fn a_deferred_tool_fails_without_preload_and_a_preload_runs_the_tool_search_exchange_first() {
        let mut rt = runtime(
            json!([{"id":"b","when":{"first_user_contains":"mon"},"reply":[{"tool_use":{"id":"t","name":"Monitor","input":{}}}],
                    "stop":"tool_use","preload":["Monitor","TaskStop"]}]),
            json!({}),
        );
        let first = rt.select(&prompt("mon"), &cfg(), false).unwrap().unwrap();
        assert_eq!(first.step_id, "b#preload");
        assert_eq!(first.reply.content[0]["name"], "ToolSearch");
        assert_eq!(
            first.reply.content[0]["input"],
            json!({"query":"select:Monitor,TaskStop","max_results":5})
        );
        assert_eq!(rt.uses()["b"], 0);
        let call = first.reply.content[0]["id"].as_str().unwrap().to_owned();
        let after = view(json!([
            {"role":"user","content":"mon"},
            {"role":"assistant","content":[{"type":"tool_use","id":call,"name":"ToolSearch","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":call,"content":"loaded"}]},
        ]));
        let second = rt.select(&after, &cfg(), false).unwrap().unwrap();
        assert_eq!(second.step_id, "b");
        assert_eq!(second.reply.content[0]["name"], "Monitor");
        assert_eq!(rt.uses()["b"], 1);

        let mut plain = runtime(
            json!([{"id":"b","reply":[{"tool_use":{"id":"t","name":"Monitor","input":{}}}],"stop":"tool_use"}]),
            json!({}),
        );
        let e = plain.select(&prompt("x"), &cfg(), false).unwrap().err().unwrap();
        assert_eq!(e, "Monitor is deferred in this request: add `preload`");
    }

    #[test]
    fn a_repeating_step_with_a_preload_loads_again_when_the_tool_is_still_deferred() {
        let mut rt = runtime(
            json!([{"id":"b","repeat":true,"when":{"first_user_contains":"mon"},
                    "reply":[{"tool_use":{"id":"t","name":"Monitor","input":{}}}],
                    "stop":"tool_use","preload":["Monitor"]}]),
            json!({}),
        );
        for round in 0..2 {
            let first = rt.select(&prompt("mon"), &cfg(), false).unwrap().unwrap();
            assert_eq!(first.step_id, "b#preload", "round {round}");
            let call = first.reply.content[0]["id"].as_str().unwrap().to_owned();
            let after = view(json!([
                {"role":"user","content":"mon"},
                {"role":"assistant","content":[{"type":"tool_use","id":call,"name":"ToolSearch","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":call,"content":"loaded"}]},
            ]));
            let second = rt.select(&after, &cfg(), false).unwrap().unwrap();
            assert_eq!(second.step_id, "b", "round {round}");
            assert_eq!(rt.uses()["b"], round + 1);
        }
    }

    #[test]
    fn a_direct_call_of_a_deferred_tool_is_served_only_when_the_step_says_so() {
        let call = json!([{"id":"d","when":{"first_user_contains":"mon"},"direct_deferred":true,
            "reply":[{"tool_use":{"id":"t","name":"Monitor","input":{}}}],"stop":"tool_use"}]);
        let mut allowed = runtime(call, json!({}));
        assert!(allowed.select(&prompt("mon"), &cfg(), false).unwrap().is_ok());
        let mut refused = runtime(
            json!([{"id":"d","when":{"first_user_contains":"mon"},
                "reply":[{"tool_use":{"id":"t","name":"Monitor","input":{}}}],"stop":"tool_use"}]),
            json!({}),
        );
        let e = refused
            .select(&prompt("mon"), &cfg(), false)
            .unwrap()
            .err()
            .expect("refused");
        assert!(e.contains("deferred"), "{e}");
        // The option does not excuse a tool the request does not offer at all.
        let mut missing = runtime(
            json!([{"id":"d","when":{"first_user_contains":"mon"},"direct_deferred":true,
                "reply":[{"tool_use":{"id":"t","name":"NoSuchTool","input":{}}}],"stop":"tool_use"}]),
            json!({}),
        );
        let e = missing
            .select(&prompt("mon"), &cfg(), false)
            .unwrap()
            .err()
            .expect("refused");
        assert!(e.contains("not offered"), "{e}");
    }

    #[test]
    fn errors_are_served_the_set_number_of_times_before_the_step_is_used() {
        let mut rt = runtime(
            json!([{"id":"e","reply":[{"text":"ok"}],"error":{"status":529,"times":2}}]),
            json!({}),
        );
        for left in [2, 1] {
            let p = rt.select(&prompt("x"), &cfg(), false).unwrap().unwrap();
            assert_eq!(p.reply.error.unwrap().times, left);
            assert_eq!(rt.uses()["e"], 0);
        }
        let p = rt.select(&prompt("x"), &cfg(), false).unwrap().unwrap();
        assert!(p.reply.error.is_none());
        assert_eq!(rt.uses()["e"], 1);
    }
}
