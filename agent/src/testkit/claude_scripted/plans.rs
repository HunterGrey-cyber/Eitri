//! What the C40 family's rows must show, checked the same way at both layers from what a run leaves: the stand-in's
//! request bodies, the CLI's two tap logs, the files of the project and the CLI's configuration directory, the
//! requests the host answered (as cards or by itself) and the plan events it folded.
//!
//! A plan never goes through the permission gate: the CLI's `can_use_tool` request for `ExitPlanMode` becomes a
//! plan interaction the person settles, so the checks here hold the rows to that (no card and no automatic answer for
//! it, a plain `allow` on the wire and nothing else), to what the model is told after each decision, to the modes
//! the CLI reports around it (plan, then the mode it was created in, and never a mode Eitri does not run), and to
//! the verdict the mode a plan lands in gives the next call.

use super::{
    driver::{read_tap, PLAN_TOOL},
    gate_answer::{Answered, Origin, PLAN_ASKED_TOOLS},
    questions::result_in,
    RequestRecord,
};
use crate::PermissionDecision;
use claude_runtime_protocol::v1::{session_event::Event as WireEvent, SessionEvent};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path};

/// What the model is told when the person settles one of a row's plans.
#[derive(Clone, Copy, Debug)]
pub struct Told {
    /// The `ExitPlanMode` call of the plan.
    pub call: &'static str,
    /// The scripted step whose request carries the call's result.
    pub step: &'static str,
    /// Whether that result is an error result; `None` leaves it open.
    pub is_error: Option<bool>,
    /// Texts the result holds.
    pub holds: &'static [&'static str],
    /// Texts it must not hold.
    pub lacks: &'static [&'static str],
    /// Whether a request of the step must carry a result for the call at all: an interrupted call may reach the
    /// model with none.
    pub required: bool,
}

/// The CLI's own words when a plan is kept (2.1.311, the TUI's "No, keep planning"): the result of a call whose
/// plan was not approved, with and without the person's feedback after it.
pub const KEPT: &str = "The user doesn't want to proceed with this tool use.";
pub const KEPT_WITH_FEEDBACK: &str = "To tell you how to proceed, the user said:";
/// What an approved plan's result starts with.
pub const APPROVED: &str = "User has approved your plan.";

macro_rules! approved {
    ($call:expr, $step:expr, $plan_text:expr) => {
        Told {
            call: $call,
            step: $step,
            is_error: Some(false),
            holds: &[APPROVED, $plan_text],
            lacks: &[KEPT],
            required: true,
        }
    };
}

/// The rows whose plan results the model is checked against, in the order the plans were decided.
pub fn told(id: &str) -> Vec<Told> {
    match id {
        "c40" => vec![approved!("toolu_c40_exit", "m4", "# C40 plan")],
        "c40_auto" => vec![approved!("toolu_c40a_exit", "m4", "# C40A plan")],
        "c40_bypass" => vec![approved!("toolu_c40b_exit", "m4", "# C40B plan")],
        "c40_fast" => vec![
            approved!("toolu_c40f_exit", "m4", "# C40F plan"),
            approved!("toolu_c40f_exit2", "m7", "# C40F plan"),
        ],
        "c40_bypass_bash" => vec![approved!("toolu_c40bb_exit", "m6", "# C40BB plan")],
        "c40k" => vec![
            Told {
                call: "toolu_c40k_exit1",
                step: "m4",
                is_error: Some(true),
                holds: &[KEPT, KEPT_WITH_FEEDBACK, "C40K-FEEDBACK also say hi"],
                lacks: &[APPROVED],
                required: true,
            },
            approved!("toolu_c40k_exit2", "m6", "C40K-TWO"),
        ],
        "c40r" => vec![
            Told {
                call: "toolu_c40r_exit1",
                step: "m4",
                is_error: Some(true),
                holds: &[KEPT],
                // Kept without feedback is the CLI's plain rejection: nothing follows it.
                lacks: &[APPROVED, KEPT_WITH_FEEDBACK],
                required: true,
            },
            approved!("toolu_c40r_exit2", "m7", "C40R-TWO"),
        ],
        "c40i" => vec![Told {
            call: "toolu_c40i_exit",
            step: "m4",
            is_error: None,
            holds: &[],
            lacks: &[APPROVED],
            required: false,
        }],
        "c40g" => vec![Told {
            call: "toolu_c40g_exit",
            step: "m4",
            is_error: None,
            holds: &[],
            lacks: &[APPROVED],
            required: false,
        }],
        "c40w" => vec![approved!("toolu_c40w_exit", "m5", "# C40W plan")],
        "c40d" => vec![approved!("toolu_c40d_exit", "approved", "# C40D plan")],
        _ => Vec::new(),
    }
}

/// The body of every request the stand-in matched to `step`, in arrival order.
pub fn bodies_of(run: &Path, records: &[RequestRecord], step: &str) -> Vec<Value> {
    let mut matched: Vec<&RequestRecord> = records
        .iter()
        .filter(|r| r.matched_step.as_deref() == Some(step))
        .collect();
    matched.sort_by_key(|r| r.arrival_index);
    matched
        .into_iter()
        .filter_map(|r| serde_json::from_str(&fs::read_to_string(run.join("standin").join(&r.body_file)).ok()?).ok())
        .collect()
}

/// Checks the plan's result in the requests of `told.step` and returns it.
pub fn check_told(told: &Told, bodies: &[Value]) -> Result<Option<(String, bool)>, String> {
    let Some((text, is_error)) = bodies.iter().find_map(|b| result_in(b, told.call)) else {
        return if told.required {
            Err(format!(
                "no request of step {} carries the result of {}",
                told.step, told.call
            ))
        } else {
            Ok(None)
        };
    };
    if let Some(want) = told.is_error {
        if is_error != want {
            return Err(format!(
                "the result of {} is{} an error result, and the decision given makes it{}: {text}",
                told.call,
                if is_error { "" } else { " not" },
                if want { " one" } else { " none" }
            ));
        }
    }
    if let Some(missing) = told.holds.iter().find(|w| !text.contains(**w)) {
        return Err(format!(
            "the model is not told `{missing}` as the result of {}: {text}",
            told.call
        ));
    }
    if let Some(extra) = told.lacks.iter().find(|w| text.contains(**w)) {
        return Err(format!(
            "the model is told `{extra}` as the result of {}, which nobody decided: {text}",
            told.call
        ));
    }
    Ok(Some((text, is_error)))
}

/// The modes the CLI reported: each turn's `system/init` and every `system/status` frame that names one, with
/// repeats collapsed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    pub inits: Vec<String>,
    pub moves: Vec<String>,
}

pub fn modes(run: &Path) -> Modes {
    let mut out = Modes::default();
    for line in read_tap(&run.join("log/cli-stdout.jsonl")) {
        let Some(j) = line.json else { continue };
        if j["type"] != "system" {
            continue;
        }
        let Some(mode) = j["permissionMode"].as_str() else {
            continue;
        };
        match j["subtype"].as_str() {
            Some("init") => out.inits.push(mode.to_owned()),
            Some("status") if out.moves.last().map(String::as_str) != Some(mode) => out.moves.push(mode.to_owned()),
            _ => {}
        }
    }
    out
}

/// The moves a row shows, with the mode the session was created in written `creation`.
pub fn expected_moves(id: &str) -> &'static [&'static str] {
    match id {
        "c40" | "c40_auto" | "c40_bypass" | "c40_bypass_bash" | "c40k" | "c40u" | "c40i" | "c40w" => {
            &["plan", "creation"]
        }
        // Closed while the plan waits: the resumed CLI starts in its creation mode and reports no move.
        "c40g" => &["plan"],
        // Planning, closed and resumed, the person enters again (the two CLI processes' `plan` reports follow each
        // other), and the approval leaves.
        "c40r" => &["plan", "creation"],
        "c40d" | "c40_fast" => &["plan", "creation", "plan", "creation"],
        _ => &[],
    }
}

/// The modes Eitri runs the CLI in: the CLI's plan mode and the two modes a session is created in. A report of any
/// other mode is one Eitri never asked for.
const RUN_MODES: [&str; 3] = ["plan", "default", "auto"];

/// What the CLI's reports and Eitri's answers must never show: a mode Eitri does not run, a turn or a move into a
/// mode other than plan mode and the one the session was created in, an `updatedPermissions` or an `updatedInput`
/// on the wire to the CLI, and an answer to the plan's call that is not a plain `allow` or a denial.
pub fn check_wire(run: &Path, id: &str) -> Result<Modes, String> {
    let seen = modes(run);
    for mode in seen.inits.iter().chain(&seen.moves) {
        if !RUN_MODES.contains(&mode.as_str()) {
            return Err(format!("the CLI reported the mode `{mode}`, which Eitri never runs"));
        }
    }
    // The mode the session was created in: the first mode any report names that is not plan mode (a row that enters
    // plan mode before its first turn has `plan` for its first init).
    let creation = seen
        .inits
        .iter()
        .chain(&seen.moves)
        .find(|m| m.as_str() != "plan")
        .cloned()
        .unwrap_or_default();
    if let Some(other) = seen
        .inits
        .iter()
        .chain(&seen.moves)
        .find(|m| m.as_str() != "plan" && **m != creation)
    {
        return Err(format!(
            "the CLI reported `{other}` in a session created in `{creation}` (inits {:?}, moves {:?})",
            seen.inits, seen.moves
        ));
    }
    let moves: Vec<&str> = seen
        .moves
        .iter()
        .map(|m| if *m == creation { "creation" } else { m.as_str() })
        .collect();
    let want = expected_moves(id);
    if !want.is_empty() && moves != want {
        return Err(format!(
            "the CLI reported the moves {moves:?}, and the row expects {want:?} (created in `{creation}`; inits {:?})",
            seen.inits
        ));
    }
    // The requests for a plan's approval, whose answer is a plain `allow` or a denial and nothing more (a question's
    // answer rides in `updatedInput`, which is the question's own).
    let plan_ids: std::collections::BTreeSet<String> = read_tap(&run.join("log/cli-stdout.jsonl"))
        .into_iter()
        .filter_map(|l| l.json)
        .filter(|j| j["type"] == "control_request" && j["request"]["tool_name"] == PLAN_TOOL)
        .filter_map(|j| j["request_id"].as_str().map(str::to_owned))
        .collect();
    for line in read_tap(&run.join("log/cli-stdin.jsonl")) {
        let Some(j) = line.json else { continue };
        if j["type"] != "control_response" {
            continue;
        }
        let answer = &j["response"]["response"];
        if answer.get("updatedPermissions").is_some() {
            return Err(format!("an answer to the CLI carries `updatedPermissions`: {answer}"));
        }
        let to_a_plan = j["response"]["request_id"]
            .as_str()
            .is_some_and(|id| plan_ids.contains(id));
        if to_a_plan
            && answer
                .as_object()
                .is_some_and(|o| o.keys().any(|k| k != "behavior" && k != "toolUseID" && k != "message"))
        {
            return Err(format!(
                "the answer to a plan carries more than a plain allow or a denial: {answer}"
            ));
        }
        if to_a_plan && answer["behavior"] == "allow" && answer.get("message").is_some() {
            return Err(format!("an allow of a plan carries a message: {answer}"));
        }
    }
    Ok(seen)
}

/// The `ExitPlanMode` calls the CLI asked about, with whether each got a `control_response` back.
pub fn plan_requests(run: &Path) -> Vec<(Option<String>, bool)> {
    let responded: std::collections::BTreeSet<String> = read_tap(&run.join("log/cli-stdin.jsonl"))
        .into_iter()
        .filter_map(|l| l.json)
        .filter(|j| j["type"] == "control_response")
        .filter_map(|j| j["response"]["request_id"].as_str().map(str::to_owned))
        .collect();
    read_tap(&run.join("log/cli-stdout.jsonl"))
        .into_iter()
        .filter_map(|l| l.json)
        .filter(|j| {
            j["type"] == "control_request"
                && j["request"]["subtype"] == "can_use_tool"
                && j["request"]["tool_name"] == PLAN_TOOL
        })
        .map(|j| {
            (
                j["request"]["tool_use_id"].as_str().map(str::to_owned),
                j["request_id"].as_str().is_some_and(|id| responded.contains(id)),
            )
        })
        .collect()
}

/// No card and no automatic answer named the plan's tool: a plan is the person's alone.
pub fn gate_untouched(cards: &[Answered], automatic: &[Answered]) -> Result<(), String> {
    let touched: Vec<String> = cards
        .iter()
        .map(|a| ("card", a))
        .chain(automatic.iter().map(|a| ("automatic answer", a)))
        .filter(|(_, a)| a.tool() == PLAN_TOOL)
        .map(|(what, a)| format!("{what}: {}", a.to_json()))
        .collect();
    if touched.is_empty() {
        Ok(())
    } else {
        Err(format!("a plan went through the permission gate: {touched:?}"))
    }
}

/// How the host answered the requests of one call: its cards (with the answer the driver gave) and its automatic
/// answers, as `(origin, decision word)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    pub cards: Vec<(String, String)>,
    pub automatic: Vec<(String, String)>,
}

fn word(decision: &Option<PermissionDecision>) -> String {
    match decision {
        Some(PermissionDecision::Allow) => "allow",
        Some(PermissionDecision::Deny { .. }) => "deny",
        Some(PermissionDecision::Defer) => "defer",
        None => "unanswered",
    }
    .to_owned()
}

pub fn verdict(call: &str, cards: &[Answered], automatic: &[Answered]) -> Verdict {
    let of = |list: &[Answered]| -> Vec<(String, String)> {
        list.iter()
            .filter(|a| a.request.tool_use_id.as_deref() == Some(call))
            .map(|a| (a.request.origin.word().to_owned(), word(&a.decision)))
            .collect()
    };
    Verdict {
        cards: of(cards),
        automatic: of(automatic),
    }
}

fn hook(word: &str) -> (String, String) {
    (Origin::Hook.word().to_owned(), word.to_owned())
}

/// What the mode a plan lands in does with the next turn's `Write`, by row: the call, and the verdict. A Manual
/// tab cards it, an Auto tab leaves it to the CLI's own auto mode, a Bypass tab allows it itself.
pub fn expected_write(id: &str) -> Option<(&'static str, Verdict)> {
    let (call, verdict) = match id {
        "c40" => (
            "toolu_c40_w",
            Verdict {
                cards: vec![hook("allow")],
                automatic: Vec::new(),
            },
        ),
        "c40_auto" => (
            "toolu_c40a_w",
            Verdict {
                cards: Vec::new(),
                automatic: vec![hook("defer")],
            },
        ),
        "c40_bypass" => (
            "toolu_c40b_w",
            Verdict {
                cards: Vec::new(),
                automatic: vec![hook("allow")],
            },
        ),
        // The person's own leave lands the tab in Manual, though it entered plan mode from Auto.
        "c40u" => (
            "toolu_c40u_w",
            Verdict {
                cards: vec![hook("allow")],
                automatic: Vec::new(),
            },
        ),
        _ => return None,
    };
    Some((call, verdict))
}

/// The call a classifier request judged: the last transcript line before its closing tag, which holds the action.
/// The request's earlier lines are the conversation so far.
pub fn classifier_judged(body: &Value) -> Option<String> {
    let blocks: Vec<&str> = body["messages"]
        .as_array()?
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter_map(|b| b["text"].as_str())
        .collect();
    let end = blocks
        .iter()
        .position(|t| t.trim_start().starts_with("</transcript>"))?;
    blocks
        .get(end.checked_sub(1)?)?
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

/// The auto-mode classifier's requests, in arrival order, each with what it judged.
pub fn classifier_requests(run: &Path, records: &[RequestRecord]) -> Vec<(usize, Option<String>)> {
    let mut asked: Vec<&RequestRecord> = records
        .iter()
        .filter(|r| r.side_kind.as_deref() == Some("auto_classifier"))
        .collect();
    asked.sort_by_key(|r| r.arrival_index);
    asked
        .into_iter()
        .map(|r| {
            let body = fs::read_to_string(run.join("standin").join(&r.body_file))
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok());
            (r.arrival_index, body.as_ref().and_then(classifier_judged))
        })
        .collect()
}

/// What Eitri sent back on the CLI's stdin, by the request id it answers: the `response` of each
/// `control_response`.
fn stdin_answers(run: &Path) -> BTreeMap<String, Value> {
    read_tap(&run.join("log/cli-stdin.jsonl"))
        .into_iter()
        .filter_map(|l| l.json)
        .filter(|j| j["type"] == "control_response")
        .filter_map(|j| {
            let id = j["response"]["request_id"].as_str()?.to_owned();
            Some((id, j["response"]["response"].clone()))
        })
        .collect()
}

/// The answer the CLI got to its `ExitPlanMode` request for `call`, if it got one.
pub fn plan_answer(run: &Path, call: &str) -> Option<Value> {
    let answers = stdin_answers(run);
    read_tap(&run.join("log/cli-stdout.jsonl"))
        .into_iter()
        .filter_map(|l| l.json)
        .filter(|j| {
            j["type"] == "control_request"
                && j["request"]["subtype"] == "can_use_tool"
                && j["request"]["tool_name"] == PLAN_TOOL
                && j["request"]["tool_use_id"] == call
        })
        .find_map(|j| answers.get(j["request_id"].as_str()?).cloned())
}

/// One request the CLI made of its host, a hook request (`hook_callback`) or a prompt of its own (`can_use_tool`),
/// with the answer it got on its stdin.
#[derive(Clone, Debug, PartialEq)]
pub struct HookAsk {
    pub call: String,
    pub tool: String,
    pub request_id: String,
    /// The permission mode the hook's input states; a prompt states none.
    pub cli_mode: Option<String>,
    /// Whether the CLI ran plan mode when it asked: the request falls between a report of `plan` and the next
    /// report of any other mode (a `system` frame naming a mode, a turn's `init` included), or the hook's input
    /// itself says `plan`.
    pub in_plan: bool,
    pub answer: Option<Value>,
}

/// Every request of one `subtype` the CLI made, in the order it made them.
fn cli_asks(run: &Path, subtype: &str) -> Vec<HookAsk> {
    let answers = stdin_answers(run);
    let mut in_plan = false;
    let mut out = Vec::new();
    for line in read_tap(&run.join("log/cli-stdout.jsonl")) {
        let Some(j) = line.json else { continue };
        if j["type"] == "system" {
            if let Some(mode) = j["permissionMode"].as_str() {
                in_plan = mode == "plan";
            }
            continue;
        }
        if j["type"] != "control_request" || j["request"]["subtype"] != subtype {
            continue;
        }
        let request = &j["request"];
        let request_id = j["request_id"].as_str().unwrap_or_default().to_owned();
        // A hook's input names the tool and the mode; a prompt names the tool beside its input.
        let (call, tool, cli_mode) = if subtype == "hook_callback" {
            let input = &request["input"];
            (
                input["tool_use_id"].as_str().or(request["tool_use_id"].as_str()),
                input["tool_name"].as_str(),
                input["permission_mode"].as_str().map(str::to_owned),
            )
        } else {
            (request["tool_use_id"].as_str(), request["tool_name"].as_str(), None)
        };
        out.push(HookAsk {
            call: call.unwrap_or_default().to_owned(),
            tool: tool.unwrap_or_default().to_owned(),
            in_plan: in_plan || cli_mode.as_deref() == Some("plan"),
            cli_mode,
            answer: answers.get(&request_id).cloned(),
            request_id,
        });
    }
    out
}

/// Every hook request the CLI made, in the order it made them.
pub fn hook_asks(run: &Path) -> Vec<HookAsk> {
    cli_asks(run, "hook_callback")
}

/// Every prompt the CLI raised itself (`can_use_tool`), in the order it raised them. Whether one fell in plan mode
/// is known only from the CLI's reports of its mode.
pub fn prompt_asks(run: &Path) -> Vec<HookAsk> {
    cli_asks(run, "can_use_tool")
}

/// The tool-use ids of every prompt the CLI raised itself (`can_use_tool`), in order.
pub fn cli_prompt_calls(run: &Path) -> Vec<String> {
    read_tap(&run.join("log/cli-stdout.jsonl"))
        .into_iter()
        .filter_map(|l| l.json)
        .filter(|j| j["type"] == "control_request" && j["request"]["subtype"] == "can_use_tool")
        .filter_map(|j| j["request"]["tool_use_id"].as_str().map(str::to_owned))
        .collect()
}

/// A `verdandi_policy` notice about a plan-mode answer, with the call whose answer it followed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanNotice {
    pub subtype: String,
    pub call: Option<String>,
}

/// The sidecar's plan-mode notices (`verdandi_policy`, `plan_*`) in a session's wire events, each tied to the call of
/// the permission resolved just before it: the sidecar sends the notice as it turns the host's answer into what
/// the CLI gets.
pub fn plan_notices(wire: &[SessionEvent]) -> Vec<PlanNotice> {
    let mut calls: BTreeMap<String, String> = BTreeMap::new();
    let mut last_resolved: Option<String> = None;
    let mut out = Vec::new();
    for event in wire {
        match &event.event {
            Some(WireEvent::PermissionRequested(asked)) => {
                calls.insert(asked.permission_id.clone(), asked.tool_use_id.clone());
            }
            Some(WireEvent::PermissionResolved(resolved)) => last_resolved = Some(resolved.permission_id.clone()),
            Some(WireEvent::ProviderNotice(notice))
                if notice.kind == "verdandi_policy"
                    && notice.subtype.as_deref().is_some_and(|s| s.starts_with("plan_")) =>
            {
                out.push(PlanNotice {
                    subtype: notice.subtype.clone().unwrap_or_default(),
                    call: last_resolved.as_ref().and_then(|id| calls.get(id).cloned()),
                });
            }
            _ => {}
        }
    }
    out
}

/// The host's answers to what the CLI asked in plan mode for a tool that writes or runs: no hook request got an
/// `allow` of the host's own, no prompt of the CLI's own got one at all (Verdandi
/// passes an allow on a prompt through, so that is where a write would run); and the calls the sidecar's
/// `plan_allow_*` notices name are, as a multiset, the calls of the hook requests that got an allow, the person's on
/// a card and the host's own. A person's allow on a prompt is the person's to give and is not checked.
pub fn check_plan_window(
    run: &Path,
    id: &str,
    cards: &[Answered],
    automatic: &[Answered],
    notices: &[PlanNotice],
) -> Result<Value, String> {
    let asks: Vec<HookAsk> = hook_asks(run)
        .into_iter()
        .filter(|a| a.in_plan && PLAN_ASKED_TOOLS.contains(&a.tool.as_str()))
        .collect();
    for ask in prompt_asks(run)
        .iter()
        .filter(|a| a.in_plan && PLAN_ASKED_TOOLS.contains(&a.tool.as_str()))
    {
        let allowed = automatic.iter().any(|a| {
            a.request.origin == Origin::CliPrompt
                && a.request.tool_use_id.as_deref() == Some(ask.call.as_str())
                && matches!(a.decision, Some(PermissionDecision::Allow))
        });
        if allowed {
            return Err(format!(
                "the host allowed {} {} by itself on the CLI's own prompt while the CLI planned: {ask:?}",
                ask.tool, ask.call
            ));
        }
    }
    let is_allow =
        |a: &&Answered| a.request.origin == Origin::Hook && matches!(a.decision, Some(PermissionDecision::Allow));
    // The calls of the hook requests that got an allow, once per allow: the person's card and the host's own.
    let mut allowed_calls: Vec<String> = Vec::new();
    let mut own_allows = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for ask in &asks {
        if seen.contains(&ask.call.as_str()) {
            continue;
        }
        seen.push(ask.call.as_str());
        let of_call = |a: &&Answered| a.request.tool_use_id.as_deref() == Some(ask.call.as_str());
        let by_card = cards.iter().filter(of_call).filter(is_allow).count();
        let own = automatic.iter().filter(of_call).filter(is_allow).count();
        allowed_calls.extend(std::iter::repeat_n(ask.call.clone(), by_card + own));
        if own > 0 {
            own_allows.push(ask);
        }
    }
    allowed_calls.sort();
    if let Some(ask) = own_allows.first() {
        return Err(format!(
            "{id}: the host allowed {} {} by itself while the CLI planned: {ask:?}",
            ask.tool, ask.call
        ));
    }
    let mut noticed_calls: Vec<String> = notices
        .iter()
        .filter(|n| n.subtype.starts_with("plan_allow_"))
        .map(|n| n.call.clone().unwrap_or_else(|| "<no call>".to_owned()))
        .collect();
    noticed_calls.sort();
    if noticed_calls != allowed_calls {
        return Err(format!(
            "the sidecar's plan_allow_* notices name the calls {noticed_calls:?}, and the hook requests made in plan \
             mode that got an allow are the calls {allowed_calls:?}: {notices:?}; asks {asks:?}"
        ));
    }
    Ok(json!({
        "asked_in_plan": asks.iter().map(|a| json!({"call": a.call, "tool": a.tool})).collect::<Vec<_>>(),
        "allows": allowed_calls.len(),
        "plan_allow_notices": noticed_calls.len(),
    }))
}

/// Rows that are release gates for plan mode: they never pass with their plan parts pending.
pub const RELEASE_GATES: [&str; 1] = ["c40d"];

/// The rules C40d seeds in the user's settings: the dangerous one auto mode must set aside, and the benign one it
/// keeps.
pub const C40D_RULES: [&str; 2] = ["Bash(*)", "Bash(truncate -s 0 c40d-*)"];

/// The rules the CLI reports in its answer to the sidecar's `get_settings`: the effective allow rules, and those it
/// read from the user's settings.
pub fn reported_allow_rules(run: &Path) -> Result<(Vec<String>, Vec<String>), String> {
    let asked: Vec<String> = read_tap(&run.join("log/cli-stdin.jsonl"))
        .into_iter()
        .filter_map(|l| l.json)
        .filter(|j| j["type"] == "control_request" && j["request"]["subtype"] == "get_settings")
        .filter_map(|j| j["request_id"].as_str().map(str::to_owned))
        .collect();
    let response = read_tap(&run.join("log/cli-stdout.jsonl"))
        .into_iter()
        .filter_map(|l| l.json)
        .find(|j| {
            j["type"] == "control_response"
                && j["response"]["request_id"]
                    .as_str()
                    .is_some_and(|id| asked.iter().any(|a| a == id))
        })
        .ok_or_else(|| format!("no answer to a get_settings request ({asked:?})"))?;
    let settings = &response["response"]["response"];
    let words = |v: &Value| -> Vec<String> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(str::to_owned))
            .collect()
    };
    let effective = words(&settings["effective"]["permissions"]["allow"]);
    let user = settings["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["source"] == "userSettings")
        .flat_map(|s| words(&s["settings"]["permissions"]["allow"]))
        .collect();
    Ok((effective, user))
}

/// A call the host left to the CLI's auto mode: its one hook request was answered by the host's own `defer` and by
/// no card, the CLI got an empty answer for it, raised no prompt of its own for the call, and stated auto mode in the
/// hook's input.
fn check_deferred(
    call: &str,
    asks: &[HookAsk],
    prompts: &[String],
    cards: &[Answered],
    automatic: &[Answered],
) -> Result<(), String> {
    let got = verdict(call, cards, automatic);
    let want = Verdict {
        cards: Vec::new(),
        automatic: vec![hook("defer")],
    };
    if got != want {
        return Err(format!("{call}: the host answered {got:?}, not {want:?}"));
    }
    let of_call: Vec<&HookAsk> = asks.iter().filter(|a| a.call == call).collect();
    let [ask] = of_call[..] else {
        return Err(format!("{call}: {} hook requests, not one: {of_call:?}", of_call.len()));
    };
    if ask.answer != Some(json!({})) {
        return Err(format!(
            "{call}: the CLI got {:?} for its hook request, not an empty answer",
            ask.answer
        ));
    }
    if ask.cli_mode.as_deref() != Some("auto") {
        return Err(format!(
            "{call}: the hook's input states the mode {:?}, not auto",
            ask.cli_mode
        ));
    }
    if prompts.iter().any(|p| p == call) {
        return Err(format!("{call}: the CLI raised a prompt of its own for the call"));
    }
    Ok(())
}

/// One route of C40d back to execution: the call the benign rule must let through, the one that must reach the
/// classifier, and their marks.
struct Route {
    name: &'static str,
    ruled: (&'static str, &'static str),
    judged: (&'static str, &'static str),
}

const C40D_ROUTES: [Route; 3] = [
    Route {
        name: "control",
        ruled: ("toolu_c40d_c1", "c40d-control-1.mark"),
        judged: ("toolu_c40d_c2", "C40D-C-2"),
    },
    Route {
        name: "approve",
        ruled: ("toolu_c40d_a1", "c40d-approve-1.mark"),
        judged: ("toolu_c40d_a2", "C40D-A-2"),
    },
    Route {
        name: "leave",
        ruled: ("toolu_c40d_b1", "c40d-leave-1.mark"),
        judged: ("toolu_c40d_b2", "C40D-B-2"),
    },
];

/// The control route's `truncate` that no seeded rule covers: it must reach the classifier, so that a `truncate`
/// let through elsewhere is the benign rule's doing and not the CLI's own. (The CLI's auto mode runs `echo` as a
/// read-only command and `touch` in the working directory as a file edit without asking anyone, so neither can show
/// a rule at work; `truncate` is in neither of those sets.)
const C40D_UNRULED: (&str, &str) = ("toolu_c40d_c3", "unruled-c40d.mark");

/// C40d: the CLI read both seeded rules; after each route back to execution (the control's fresh session, the
/// approved plan, the person's own enter and leave) the `truncate` the benign rule covers ran with no classifier
/// request and `python3` went to the classifier, which judged exactly that call, while a `truncate` no rule covers
/// went to it too. Every one of those calls was left to the CLI by the host's own `defer` (one hook request, an empty
/// answer, no prompt of the CLI's, the hook stating auto mode), and every turn started in auto mode. Any other
/// evidence fails.
pub fn check_dangerous_rules(
    run: &Path,
    project: &Path,
    records: &[RequestRecord],
    cards: &[Answered],
    automatic: &[Answered],
) -> Result<Value, String> {
    let (effective, user) = reported_allow_rules(run)?;
    for rule in C40D_RULES {
        if !effective.iter().any(|r| r == rule) || !user.iter().any(|r| r == rule) {
            return Err(format!(
                "the CLI did not report the seeded rule `{rule}` (effective {effective:?}, user settings {user:?})"
            ));
        }
    }
    let judged = classifier_requests(run, records);
    let asks = hook_asks(run);
    let prompts = cli_prompt_calls(run);
    let result_of = |call: &str| -> Option<(usize, String, bool)> {
        records
            .iter()
            .filter_map(|r| {
                let body: Value =
                    serde_json::from_str(&fs::read_to_string(run.join("standin").join(&r.body_file)).ok()?).ok()?;
                result_in(&body, call).map(|(text, is_error)| (r.arrival_index, text, is_error))
            })
            .min_by_key(|(at, _, _)| *at)
    };
    let judged_once = |route: &str, mark: &str| -> Result<(usize, String), String> {
        let hits: Vec<&(usize, Option<String>)> = judged
            .iter()
            .filter(|(_, what)| what.as_deref().is_some_and(|w| w.contains(mark)))
            .collect();
        let [(at, Some(what))] = hits[..] else {
            return Err(format!(
                "route {route}: the classifier was asked about `{mark}` {} times, not once: {judged:?}",
                hits.len()
            ));
        };
        if !what.starts_with("{\"Bash\":") {
            return Err(format!(
                "route {route}: the classifier judged something other than the Bash call: {what:?}"
            ));
        }
        Ok((*at, what.clone()))
    };
    // A call that ran: its result reached the model as no error, holding `holds`, and after `after` when given.
    let ran = |route: &str, call: &str, holds: &str, after: Option<usize>| -> Result<(), String> {
        let Some((at, text, is_error)) = result_of(call) else {
            return Err(format!("route {route}: no request carries the result of {call}"));
        };
        if is_error || !text.contains(holds) {
            return Err(format!("route {route}: {call} did not run (error {is_error}): {text}"));
        }
        if let Some(asked_at) = after.filter(|asked_at| at < *asked_at) {
            return Err(format!(
                "route {route}: the result of {call} reached the model (request #{at}) before the classifier's request \
                 #{asked_at}"
            ));
        }
        Ok(())
    };
    let made = |route: &str, file: &str| -> Result<(), String> {
        if project.join(file).is_file() {
            Ok(())
        } else {
            Err(format!("route {route}: `truncate -s 0 {file}` made no file"))
        }
    };
    let mut routes = Vec::new();
    for route in &C40D_ROUTES {
        let (ruled_call, ruled_file) = route.ruled;
        let (judged_call, mark) = route.judged;
        for call in [ruled_call, judged_call] {
            check_deferred(call, &asks, &prompts, cards, automatic)
                .map_err(|why| format!("route {}: {why}", route.name))?;
        }
        if let Some(seen) = judged
            .iter()
            .find(|(_, what)| what.as_deref().is_some_and(|w| w.contains(ruled_file)))
        {
            return Err(format!(
                "route {}: the `truncate` the benign rule covers reached the classifier: {seen:?}",
                route.name
            ));
        }
        ran(route.name, ruled_call, "", None)?;
        made(route.name, ruled_file)?;
        let (asked_at, what) = judged_once(route.name, mark)?;
        ran(route.name, judged_call, mark, Some(asked_at))?;
        routes.push(json!({"route": route.name, "classifier_request": asked_at, "judged": what}));
    }
    let (unruled_call, unruled_file) = C40D_UNRULED;
    check_deferred(unruled_call, &asks, &prompts, cards, automatic).map_err(|why| format!("route control: {why}"))?;
    let (unruled_at, unruled_what) = judged_once("control", unruled_file)?;
    ran("control", unruled_call, "", Some(unruled_at))?;
    made("control", unruled_file)?;
    if judged.len() != C40D_ROUTES.len() + 1 {
        return Err(format!(
            "the classifier was asked {} times, not once per route and once for the unruled `truncate`: {judged:?}",
            judged.len()
        ));
    }
    let seen = modes(run);
    if seen.inits.is_empty() || seen.inits.iter().any(|m| m != "auto") {
        return Err(format!("the turns did not all start in auto mode: {:?}", seen.inits));
    }
    Ok(json!({
        "rules": {"effective": effective, "user_settings": user},
        "routes": routes,
        "unruled": {"classifier_request": unruled_at, "judged": unruled_what},
        "turn_modes": seen.inits,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::claude_scripted::gate_answer::Request;

    fn body(call: &str, content: &str, is_error: bool) -> Value {
        json!({"messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": call, "name": "ExitPlanMode", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": call, "content": content, "is_error": is_error}]}
        ]})
    }

    const APPROVED_TEXT: &str =
        "User has approved your plan. You can now start coding.\n\n## Approved Plan:\n# C40 plan";

    #[test]
    fn an_approved_plan_passes_and_a_kept_one_does_not_pass_for_it() {
        let told = &told("c40")[0];
        assert!(check_told(told, &[body("toolu_c40_exit", APPROVED_TEXT, false)])
            .unwrap()
            .is_some());
        let kept = format!("{KEPT} STOP what you are doing.");
        let err = check_told(told, &[body("toolu_c40_exit", &kept, true)]).unwrap_err();
        assert!(err.contains("is an error result"), "{err}");
        let err = check_told(told, &[body("toolu_c40_exit", &kept, false)]).unwrap_err();
        assert!(err.contains("not told"), "{err}");
        let err = check_told(told, &[json!({"messages": []})]).unwrap_err();
        assert!(err.contains("no request of step m4"), "{err}");
    }

    #[test]
    fn feedback_is_told_after_the_clis_words_and_a_plain_keep_carries_none() {
        let rows = told("c40k");
        let with = format!("{KEPT} (eg. a file edit). {KEPT_WITH_FEEDBACK}\nC40K-FEEDBACK also say hi");
        assert!(check_told(&rows[0], &[body("toolu_c40k_exit1", &with, true)]).is_ok());
        let without = format!("{KEPT} STOP.");
        assert!(check_told(&rows[0], &[body("toolu_c40k_exit1", &without, true)]).is_err());
        let plain = &told("c40r")[0];
        assert!(check_told(plain, &[body("toolu_c40r_exit1", &without, true)]).is_ok());
        let err = check_told(plain, &[body("toolu_c40r_exit1", &with, true)]).unwrap_err();
        assert!(err.contains("which nobody decided"), "{err}");
    }

    #[test]
    fn a_result_that_may_be_missing_passes_when_it_is_and_is_checked_when_it_is_not() {
        let told = &told("c40i")[0];
        assert_eq!(check_told(told, &[json!({"messages": []})]), Ok(None));
        assert!(check_told(told, &[body("toolu_c40i_exit", APPROVED_TEXT, false)]).is_err());
    }

    fn tap(run: &Path, name: &str, lines: &[Value]) {
        let text: String = lines
            .iter()
            .enumerate()
            .map(|(i, v)| {
                format!(
                    "{}\n",
                    json!({"index": i, "time": i as f64 / 10.0, "line": v.to_string()})
                )
            })
            .collect();
        fs::create_dir_all(run.join("log")).unwrap();
        fs::write(run.join("log").join(name), text).unwrap();
    }

    fn run_dir(tag: &str) -> crate::test_tmp::TmpDir {
        crate::test_tmp::TmpDir::new("plans", tag)
    }

    fn status(mode: &str) -> Value {
        json!({"type": "system", "subtype": "status", "status": null, "permissionMode": mode})
    }

    #[test]
    fn the_moves_are_read_from_status_frames_with_repeats_collapsed() {
        let run = run_dir("moves");
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                json!({"type": "system", "subtype": "init", "permissionMode": "auto"}),
                status("plan"),
                status("plan"),
                json!({"type": "system", "subtype": "init", "permissionMode": "plan"}),
                status("auto"),
                json!({"type": "system", "subtype": "status", "status": "requesting"}),
            ],
        );
        tap(&run, "cli-stdin.jsonl", &[]);
        let seen = check_wire(&run, "c40").unwrap();
        assert_eq!(seen.moves, ["plan", "auto"]);
        assert_eq!(seen.inits, ["auto", "plan"]);
        let err = check_wire(&run, "c40d").unwrap_err();
        assert!(err.contains("the row expects"), "{err}");
    }

    #[test]
    fn a_session_entered_into_plan_mode_before_its_first_turn_is_still_created_in_the_other_mode() {
        let run = run_dir("host-entered");
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                status("plan"),
                json!({"type": "system", "subtype": "init", "permissionMode": "plan"}),
                status("auto"),
                json!({"type": "system", "subtype": "init", "permissionMode": "auto"}),
            ],
        );
        tap(&run, "cli-stdin.jsonl", &[]);
        assert!(check_wire(&run, "c40u").is_ok());
    }

    #[test]
    fn a_mode_eitri_never_runs_and_an_answer_with_more_than_allow_fail() {
        let run = run_dir("never");
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                json!({"type": "system", "subtype": "init", "permissionMode": "auto"}),
                status("dontAsk"),
            ],
        );
        tap(&run, "cli-stdin.jsonl", &[]);
        assert!(check_wire(&run, "c40").unwrap_err().contains("never runs"));
        // A mode Eitri runs, but not the one this session was created in: a turn that starts in it fails too.
        let run = run_dir("other-creation");
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                json!({"type": "system", "subtype": "init", "permissionMode": "auto"}),
                status("plan"),
                status("auto"),
                json!({"type": "system", "subtype": "init", "permissionMode": "default"}),
            ],
        );
        tap(&run, "cli-stdin.jsonl", &[]);
        let err = check_wire(&run, "c40").unwrap_err();
        assert!(err.contains("created in `auto`"), "{err}");

        let run = run_dir("answers");
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                json!({"type": "system", "subtype": "init", "permissionMode": "auto"}),
                status("plan"),
                json!({"type": "control_request", "request_id": "r", "request": {"subtype": "can_use_tool", "tool_name": "ExitPlanMode"}}),
                status("auto"),
            ],
        );
        let response = |answer: Value| json!({"type": "control_response", "response": {"subtype": "success", "request_id": "r", "response": answer}});
        tap(
            &run,
            "cli-stdin.jsonl",
            &[response(json!({"behavior": "allow", "toolUseID": "t"}))],
        );
        assert!(check_wire(&run, "c40").is_ok());
        tap(
            &run,
            "cli-stdin.jsonl",
            &[response(json!({"behavior": "deny", "message": "no", "toolUseID": "t"}))],
        );
        assert!(check_wire(&run, "c40").is_ok(), "a denial carries its message");
        for bad in [
            json!({"behavior": "allow", "updatedPermissions": []}),
            json!({"behavior": "allow", "updatedInput": {}}),
            json!({"behavior": "allow", "message": "x"}),
        ] {
            tap(&run, "cli-stdin.jsonl", &[response(bad.clone())]);
            assert!(check_wire(&run, "c40").is_err(), "{bad}");
        }
        // A question's answer rides in `updatedInput`: the check is about plans.
        let other = json!({"type": "control_response", "response": {"subtype": "success", "request_id": "q", "response": {"behavior": "allow", "updatedInput": {}}}});
        tap(&run, "cli-stdin.jsonl", &[other]);
        assert!(check_wire(&run, "c40").is_ok());
    }

    fn answered(tool: &str, call: &str, origin: Origin, decision: Option<PermissionDecision>) -> Answered {
        Answered {
            request: Request {
                permission_id: format!("p-{call}-{}", origin.word()),
                tool_use_id: Some(call.into()),
                tool_name: tool.into(),
                input: json!({}),
                origin,
            },
            decision,
        }
    }

    #[test]
    fn a_plan_that_went_through_the_gate_fails_and_a_call_has_its_own_verdict() {
        let card = answered("Write", "toolu_c40_w", Origin::Hook, Some(PermissionDecision::Allow));
        assert!(gate_untouched(std::slice::from_ref(&card), &[]).is_ok());
        let plan = answered(
            "ExitPlanMode",
            "toolu_c40_exit",
            Origin::Hook,
            Some(PermissionDecision::Allow),
        );
        assert!(gate_untouched(&[], std::slice::from_ref(&plan)).is_err());
        let seen = verdict(
            "toolu_c40_w",
            &[card, answered("Write", "toolu_other", Origin::Hook, None)],
            &[],
        );
        assert_eq!(seen, expected_write("c40").unwrap().1);
        assert_ne!(seen, expected_write("c40_auto").unwrap().1);
    }

    #[test]
    fn the_judged_call_is_the_transcript_line_before_the_closing_tag() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "<transcript>\n"},
            {"type": "text", "text": "{\"user\":\"go\"}\n{\"Bash\":\"echo C40D-A-1\"}\n"},
            {"type": "text", "text": "{\"Bash\":\"python3 -c \\\"print('C40D-A-2')\\\"\"}\n"},
            {"type": "text", "text": "</transcript>\n"},
            {"type": "text", "text": "Respond with <severity>N</severity>"}
        ]}]});
        let judged = classifier_judged(&body).unwrap();
        assert!(judged.contains("C40D-A-2") && !judged.contains("C40D-A-1"), "{judged}");
        assert_eq!(classifier_judged(&json!({"messages": []})), None);
        // The block before the closing tag may hold several lines: the action is its last one.
        let joined = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "<transcript>\n{\"user\":\"go\"}\n{\"Bash\":\"truncate -s 0 c40d-a-1.mark\"}\n\n"},
            {"type": "text", "text": "</transcript>\n"}
        ]}]});
        assert_eq!(
            classifier_judged(&joined).as_deref(),
            Some("{\"Bash\":\"truncate -s 0 c40d-a-1.mark\"}")
        );
    }

    fn hook_request(id: &str, call: &str, tool: &str, mode: &str) -> Value {
        json!({"type": "control_request", "request_id": id, "request": {
            "subtype": "hook_callback", "callback_id": "hook_0", "tool_use_id": call,
            "input": {"hook_event_name": "PreToolUse", "permission_mode": mode, "tool_name": tool, "tool_use_id": call}
        }})
    }

    fn answer(id: &str, response: Value) -> Value {
        json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": response}})
    }

    #[test]
    fn a_hook_request_is_in_plan_between_a_plan_report_and_the_next_other_mode_or_when_it_says_so() {
        let run = run_dir("window");
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                json!({"type": "system", "subtype": "init", "permissionMode": "auto"}),
                hook_request("r1", "w1", "Write", "auto"),
                status("plan"),
                json!({"type": "system", "subtype": "status", "status": "requesting"}),
                hook_request("r2", "w2", "Write", "auto"),
                status("auto"),
                hook_request("r3", "w3", "Write", "plan"),
                hook_request("r4", "w4", "Write", "auto"),
            ],
        );
        tap(&run, "cli-stdin.jsonl", &[answer("r2", json!({}))]);
        let asks = hook_asks(&run);
        let seen: Vec<(&str, bool)> = asks.iter().map(|a| (a.call.as_str(), a.in_plan)).collect();
        assert_eq!(seen, [("w1", false), ("w2", true), ("w3", true), ("w4", false)]);
        assert_eq!(asks[1].answer, Some(json!({})));
        assert_eq!(asks[0].answer, None);
        assert_eq!(asks[1].cli_mode.as_deref(), Some("auto"));
    }

    fn notices(wire: &[(&str, &str)]) -> Vec<PlanNotice> {
        wire.iter()
            .map(|(subtype, call)| PlanNotice {
                subtype: (*subtype).into(),
                call: Some((*call).into()),
            })
            .collect()
    }

    /// A Bypass row's plan-file `Write` asked in plan mode, allowed by the host itself, abstained by the sidecar.
    fn bypass_plan_window(tag: &str, answered: Value) -> crate::test_tmp::TmpDir {
        let run = run_dir(tag);
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                json!({"type": "system", "subtype": "init", "permissionMode": "auto"}),
                status("plan"),
                hook_request("r1", "toolu_c40b_planw", "Write", "plan"),
                hook_request("r2", "toolu_c40b_search", "ToolSearch", "plan"),
                status("auto"),
            ],
        );
        tap(&run, "cli-stdin.jsonl", &[answer("r1", answered)]);
        run
    }

    #[test]
    fn the_host_allowing_a_plan_mode_write_itself_fails_in_every_row() {
        let run = bypass_plan_window("own-allow", json!({}));
        let own = [
            answered(
                "Write",
                "toolu_c40b_planw",
                Origin::Hook,
                Some(PermissionDecision::Allow),
            ),
            answered(
                "ToolSearch",
                "toolu_c40b_search",
                Origin::Hook,
                Some(PermissionDecision::Allow),
            ),
        ];
        let abstained = notices(&[("plan_allow_deferred", "toolu_c40b_planw")]);
        // Even where the sidecar abstained and the CLI got an empty answer: no person was asked.
        for row in ["c40_bypass", "c40"] {
            let err = check_plan_window(&run, row, &[], &own, &abstained).unwrap_err();
            assert!(err.contains("allowed Write toolu_c40b_planw by itself"), "{err}");
        }
        // Deferred to the CLI's plan mode, it passes.
        let deferred = [answered(
            "Write",
            "toolu_c40b_planw",
            Origin::Hook,
            Some(PermissionDecision::Defer),
        )];
        let seen = check_plan_window(&run, "c40_bypass", &[], &deferred, &[]).unwrap();
        assert_eq!(seen["allows"], json!(0));
    }

    #[test]
    fn every_plan_allow_notice_is_one_allow_a_plan_mode_request_got() {
        let run = bypass_plan_window("count", json!({}));
        let card = [answered(
            "Write",
            "toolu_c40b_planw",
            Origin::Hook,
            Some(PermissionDecision::Allow),
        )];
        let one = notices(&[("plan_allow_deferred", "toolu_c40b_planw")]);
        assert!(
            check_plan_window(&run, "c40", &card, &[], &one).is_ok(),
            "the person's allow on a card"
        );
        let err = check_plan_window(&run, "c40", &card, &[], &[]).unwrap_err();
        assert!(err.contains("name the calls []"), "{err}");
        let two = notices(&[
            ("plan_allow_deferred", "toolu_c40b_planw"),
            ("plan_allow_asked", "toolu_other"),
        ]);
        assert!(check_plan_window(&run, "c40", &card, &[], &two).is_err());
    }

    #[test]
    fn the_notices_name_the_calls_that_got_an_allow_so_a_missing_one_and_a_stray_one_do_not_cancel() {
        let run = bypass_plan_window("cancel", json!({}));
        let card = [answered(
            "Write",
            "toolu_c40b_planw",
            Origin::Hook,
            Some(PermissionDecision::Allow),
        )];
        // One allow without its notice and one notice for a call that got no allow: the counts agree, the calls
        // do not.
        let stray = notices(&[("plan_allow_deferred", "toolu_other")]);
        let err = check_plan_window(&run, "c40", &card, &[], &stray).unwrap_err();
        assert!(
            err.contains("name the calls [\"toolu_other\"]")
                && err.contains("allow are the calls [\"toolu_c40b_planw\"]"),
            "{err}"
        );
        // The same call twice noticed against one allow fails too; the matching notice passes.
        let twice = notices(&[
            ("plan_allow_deferred", "toolu_c40b_planw"),
            ("plan_allow_deferred", "toolu_c40b_planw"),
        ]);
        assert!(check_plan_window(&run, "c40", &card, &[], &twice).is_err());
        let own = notices(&[("plan_allow_deferred", "toolu_c40b_planw")]);
        assert!(check_plan_window(&run, "c40", &card, &[], &own).is_ok());
    }

    /// A plan window holding the CLI's own prompts: a `Write` and a `Bash` asked in plan mode, a `Write` asked
    /// before it, and a `Read` asked in it.
    fn prompts_in_plan(tag: &str) -> crate::test_tmp::TmpDir {
        let run = run_dir(tag);
        let prompt = |id: &str, call: &str, tool: &str| {
            json!({"type": "control_request", "request_id": id, "request": {
                "subtype": "can_use_tool", "tool_name": tool, "tool_use_id": call, "input": {}
            }})
        };
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                json!({"type": "system", "subtype": "init", "permissionMode": "auto"}),
                prompt("q0", "toolu_before", "Write"),
                status("plan"),
                prompt("q1", "toolu_write", "Write"),
                prompt("q2", "toolu_bash", "Bash"),
                prompt("q3", "toolu_read", "Read"),
                status("auto"),
            ],
        );
        run
    }

    #[test]
    fn an_automatic_allow_on_the_clis_own_prompt_in_the_plan_window_fails_and_the_persons_does_not() {
        let run = prompts_in_plan("prompts");
        let allow = |tool: &str, call: &str| answered(tool, call, Origin::CliPrompt, Some(PermissionDecision::Allow));
        for (tool, call) in [("Write", "toolu_write"), ("Bash", "toolu_bash")] {
            let err = check_plan_window(&run, "c40w", &[], &[allow(tool, call)], &[]).unwrap_err();
            assert!(err.contains("on the CLI's own prompt") && err.contains(call), "{err}");
        }
        // The person's allow on a card, the host's deny or defer, an allow outside the window and an allow for a
        // tool that neither writes nor runs are not that.
        assert!(check_plan_window(&run, "c40w", &[allow("Write", "toolu_write")], &[], &[]).is_ok());
        let deny = answered(
            "Bash",
            "toolu_bash",
            Origin::CliPrompt,
            Some(PermissionDecision::Deny { reason: None }),
        );
        assert!(check_plan_window(&run, "c40w", &[], &[deny], &[]).is_ok());
        assert!(check_plan_window(&run, "c40w", &[], &[allow("Write", "toolu_before")], &[]).is_ok());
        assert!(check_plan_window(&run, "c40w", &[], &[allow("Read", "toolu_read")], &[]).is_ok());
    }

    #[test]
    fn a_plan_notice_is_tied_to_the_call_resolved_just_before_it() {
        use claude_runtime_protocol::v1::{PermissionRequested, PermissionResolved, ProviderNotice};
        let event = |event: WireEvent| SessionEvent {
            event: Some(event),
            ..SessionEvent::default()
        };
        let asked = |permission_id: &str, call: &str| {
            event(WireEvent::PermissionRequested(PermissionRequested {
                permission_id: permission_id.into(),
                tool_use_id: call.into(),
                ..PermissionRequested::default()
            }))
        };
        let resolved = |permission_id: &str| {
            event(WireEvent::PermissionResolved(PermissionResolved {
                permission_id: permission_id.into(),
                ..PermissionResolved::default()
            }))
        };
        let notice = |kind: &str, subtype: &str| {
            event(WireEvent::ProviderNotice(ProviderNotice {
                kind: kind.into(),
                subtype: Some(subtype.into()),
            }))
        };
        let wire = [
            asked("p1", "w1"),
            asked("p2", "w2"),
            resolved("p2"),
            notice("verdandi_policy", "plan_allow_deferred"),
            notice("system", "status"),
            resolved("p1"),
            notice("verdandi_policy", "plan_allow_asked"),
            notice("verdandi_policy", "something_else"),
        ];
        assert_eq!(
            plan_notices(&wire),
            [
                PlanNotice {
                    subtype: "plan_allow_deferred".into(),
                    call: Some("w2".into())
                },
                PlanNotice {
                    subtype: "plan_allow_asked".into(),
                    call: Some("w1".into())
                },
            ]
        );
    }

    #[test]
    fn a_plan_answer_is_read_by_the_plans_request() {
        let run = run_dir("plan-answer");
        tap(
            &run,
            "cli-stdout.jsonl",
            &[
                json!({"type": "control_request", "request_id": "q", "request": {"subtype": "can_use_tool", "tool_name": "ExitPlanMode", "tool_use_id": "toolu_x"}}),
            ],
        );
        tap(
            &run,
            "cli-stdin.jsonl",
            &[answer("q", json!({"behavior": "deny", "message": "m"}))],
        );
        assert_eq!(plan_answer(&run, "toolu_x").unwrap()["behavior"], "deny");
        assert_eq!(plan_answer(&run, "toolu_y"), None);
        assert_eq!(cli_prompt_calls(&run), ["toolu_x"]);
    }
}
