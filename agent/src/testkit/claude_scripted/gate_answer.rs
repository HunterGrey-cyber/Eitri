//! How the layer (a) host answers the permission requests of a gated session.
//!
//! Every Claude session is gated: the sidecar asks the host about each tool call (a hook request) and, for the
//! checks the CLI keeps to itself, relays the CLI's own prompt as a second request for the same call. Eitri's
//! real host answers from its permission policy; this crate cannot reach that policy, so the layer (a) host
//! answers by the scenario's mode alone:
//!
//! | mode   | hook request | CLI prompt |
//! |--------|--------------|------------|
//! | bypass | `Allow`      | `Allow`    |
//! | auto   | `Defer`      | the driver |
//! | manual | the driver   | the driver |
//!
//! A Manual tab answers one hook request itself, as the host's policy table does: `ToolSearch` (it loads a tool's
//! schema and does nothing else), which the model must call before it can call a tool the CLI offers deferred.
//!
//! Auto defers every hook request because a session in the sandbox has no saved rules: the CLI's classifier
//! then sees a superset of what Eitri would defer. A request that needs the driver waits for an
//! `answer_card` of the same tool; a request still waiting when the driver is on any other wait fails that
//! wait, naming the tool.
//!
//! **While the session plans** (`Gate::set_planning`) the mode answers by the plan table instead, the same
//! rows the host's gate (`eitri_core::agent_backend::plan_verdict`) reads while the CLI runs plan mode: a hook
//! request for `Write` or `Bash` of the main thread is deferred to the CLI's own plan mode in every mode (at a
//! boundary root), one
//! for `Edit` or `NotebookEdit` is a card, every prompt the CLI raises is a card, and anything else is
//! judged as the mode it began in judges it. Nothing here answers `allow` for those four tools while
//! planning, and an approve-all sweep answers nothing. The table's two other inputs are the host's to set
//! (`Gate::set_cli_runs_auto`, `Gate::set_boundary`; both true until it does): where the root is no boundary no
//! request is left to the CLI, and a Bypass tab leaves its other requests to the CLI only while it runs auto.
//!
//! **Where the CLI stood for a request** is the host's to tell (`judged_phase`, as the product's gate reads it):
//! on a session that offered plan mode a request waits until its call has been folded or the wait is over (one
//! naming no call: the whole wait), unless the mode and the plan table answer it alike whatever the call is
//! ([`waits_for_its_call`]); then the call's own mark decides ([`Phase::Planning`] for a call the CLI made in plan
//! mode, [`Phase::Uncertain`] for one made during a move, answered by rule only where both tables agree). A call
//! that never came, and a request naming none, are judged in either mode: no other event on the message path says
//! where the CLI stood, since the sidecar's requests overtake the events sent before them.

use super::script::Mode;
use crate::provider::PermissionDecision;
use serde_json::{json, Value};

/// Where a request came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The sidecar's gate, which asks about every call.
    Hook,
    /// The CLI's own prompt, raised after the gate had answered.
    CliPrompt,
}

impl Origin {
    pub fn word(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::CliPrompt => "cli_prompt",
        }
    }
}

/// One permission request as the host saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub permission_id: String,
    pub tool_use_id: Option<String>,
    pub tool_name: String,
    pub input: Value,
    pub origin: Origin,
}

/// Who gave an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The mode's own rule.
    Automatic,
    /// The driver's `answer_card`.
    Driver,
}

/// A request and what became of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Answered {
    pub request: Request,
    /// `None` while the request waits for the driver.
    pub decision: Option<PermissionDecision>,
}

impl Answered {
    pub fn tool(&self) -> &str {
        &self.request.tool_name
    }

    pub fn to_json(&self) -> Value {
        json!({
            "permission_id": self.request.permission_id,
            "tool_use_id": self.request.tool_use_id,
            "tool": self.request.tool_name,
            "origin": self.request.origin.word(),
            "decision": self.decision.as_ref().map(decision_word),
        })
    }
}

/// The word a decision is logged and compared by.
pub fn decision_word(decision: &PermissionDecision) -> &'static str {
    match decision {
        PermissionDecision::Allow => "allow",
        PermissionDecision::Deny { .. } => "deny",
        PermissionDecision::Defer => "defer",
    }
}

/// The answer a mode gives by itself, or `None` when only the driver can answer.
pub fn automatic(mode: Mode, origin: Origin) -> Option<PermissionDecision> {
    match (mode, origin) {
        (Mode::Bypass, _) => Some(PermissionDecision::Allow),
        (Mode::Auto, Origin::Hook) => Some(PermissionDecision::Defer),
        (Mode::Auto, Origin::CliPrompt) | (Mode::Manual, _) => None,
    }
}

/// The tools that write files or run commands: no mode answers `allow` for one while the CLI runs plan mode.
pub const PLAN_ASKED_TOOLS: [&str; 4] = ["Edit", "Write", "NotebookEdit", "Bash"];

/// Of [`PLAN_ASKED_TOOLS`], the ones a main-thread hook request for is left to the CLI's plan mode.
pub const PLAN_DEFERRED_TOOLS: [&str; 2] = ["Write", "Bash"];

/// What the plan table reads of the session besides the request: the same inputs `plan_verdict` takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanContext {
    /// The request's call is one of the main thread's.
    pub main_thread: bool,
    /// The CLI's latest report of its own mode says `auto` (`AgentSessionProjection::cli_runs_auto`).
    pub cli_runs_auto: bool,
    /// The project root is one the policy takes for a boundary (`agent::project_boundary::refusal` finds none).
    pub boundary: bool,
}

/// The answer the plan table gives a request by itself while the CLI runs plan mode, or `None` for a card.
/// A `Write` or `Bash` of the main thread is left to the CLI only at a boundary root; a Bypass tab leaves the rest
/// to it only at one too, and only when the CLI runs auto (otherwise nothing here knows what the CLI would decide,
/// and the request is a card). Auto and Manual stand as they are: the product judges those as the mode does
/// outside plan mode, which this host models by the mode alone.
pub fn plan_automatic(mode: Mode, request: &Request, context: PlanContext) -> Option<PermissionDecision> {
    if request.origin == Origin::CliPrompt {
        return None;
    }
    if PLAN_ASKED_TOOLS.contains(&request.tool_name.as_str()) {
        return (context.boundary && context.main_thread && PLAN_DEFERRED_TOOLS.contains(&request.tool_name.as_str()))
            .then_some(PermissionDecision::Defer);
    }
    match mode {
        Mode::Manual => None,
        Mode::Auto => Some(PermissionDecision::Defer),
        Mode::Bypass => (context.boundary && context.cli_runs_auto).then_some(PermissionDecision::Defer),
    }
}

/// How a request is judged: as the CLI made it outside plan mode, in it, or in either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Executing,
    Planning,
    Uncertain,
}

/// How the host's gate judges a request for the call `call` (`None` when it names none), from the projection: plan
/// mode where the call began in it or the CLI runs it now, either during a move or when the CLI plans now and the
/// call was never seen, and outside plan mode otherwise -- except that a request that waits to be placed (`ordered`)
/// and was judged with its call unseen is judged in either mode: a lagging message path may still hold the `plan`
/// report. A copy of the product's `eitri_core::agent_backend::Sequence::phase` (with `Sequence::read`), kept by
/// hand: this crate cannot depend on `eitri-core`. Change both together.
pub fn judged_phase(projection: &crate::AgentSessionProjection, call: Option<&str>, ordered: bool) -> Phase {
    use crate::CallPlanMark::{Moving, Planning};
    let seen = call.is_some_and(|id| projection.call_seen(id));
    let mark = call.filter(|_| seen).and_then(|id| projection.call_mark(id));
    let now = projection.plan_mark_now();
    match (mark, now) {
        (Some(Moving), _) | (_, Some(Moving)) => Phase::Uncertain,
        (_, Some(Planning)) if !seen => Phase::Uncertain,
        (Some(Planning), _) | (_, Some(Planning)) => Phase::Planning,
        (None, None) if ordered && !seen => Phase::Uncertain,
        (None, None) => Phase::Executing,
    }
}

/// Whether a request waits to be placed before it is judged, on a session that offered plan mode: unless the mode
/// and the plan table give it the same answer whatever the call is (a hook request in a Manual or Auto tab for a
/// tool that neither writes nor runs). The caller leaves out a hook request for one of the session's own tools. A
/// copy of the product's `eitri_core::agent_backend::waits_for_its_call`, kept by hand; change both together.
pub fn waits_for_its_call(mode: Mode, request: &Request) -> bool {
    request.origin == Origin::CliPrompt
        || mode == Mode::Bypass
        || PLAN_ASKED_TOOLS.contains(&request.tool_name.as_str())
}

/// An answer to send now.
#[derive(Clone, Debug, PartialEq)]
pub struct ToSend {
    pub request: Request,
    pub decision: PermissionDecision,
    pub source: Source,
}

/// The state of every request of one session: what was answered by rule, what by the driver, what waits.
#[derive(Debug)]
pub struct Gate {
    mode: Mode,
    /// The CLI runs plan mode now: the plan table answers instead of the mode's own rule.
    planning: bool,
    /// The CLI's latest report of its own mode says `auto`. Every recorded session runs auto and a host that never
    /// says otherwise gets that.
    cli_runs_auto: bool,
    /// The project root is a boundary. Every recorded session's is.
    boundary: bool,
    /// Requests the driver answers, in arrival order, with the answer once it is given.
    cards: Vec<Answered>,
    /// Requests the mode answered, in arrival order.
    automatic: Vec<Answered>,
    /// Driver answers given before their request arrived: (tool, allow), oldest first.
    armed: Vec<(String, bool, Option<Origin>)>,
}

impl Gate {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            planning: false,
            cli_runs_auto: true,
            boundary: true,
            cards: Vec::new(),
            automatic: Vec::new(),
            armed: Vec::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The mode the tab takes up: after an approved plan, or after a leave of the person's own.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Whether the CLI runs plan mode now (`AgentSessionProjection::plan_gate_applies`).
    pub fn set_planning(&mut self, on: bool) {
        self.planning = on;
    }

    pub fn planning(&self) -> bool {
        self.planning
    }

    /// Whether the CLI's latest report of its mode says `auto` (`AgentSessionProjection::cli_runs_auto`).
    pub fn set_cli_runs_auto(&mut self, on: bool) {
        self.cli_runs_auto = on;
    }

    /// Whether the project root is a boundary the way the policy takes it.
    pub fn set_boundary(&mut self, boundary: bool) {
        self.boundary = boundary;
    }

    /// A request arrived for a call taken as the main thread's. See [`Gate::on_request_in`].
    pub fn on_request(&mut self, request: Request) -> Option<ToSend> {
        self.on_request_in(request, true)
    }

    /// A request arrived. Returns the answer to send now, if there is one: the mode's rule (the plan table's
    /// while planning), or an armed driver answer for the same tool.
    pub fn on_request_in(&mut self, request: Request, main_thread: bool) -> Option<ToSend> {
        let phase = if self.planning {
            Phase::Planning
        } else {
            Phase::Executing
        };
        self.on_request_judged(request, main_thread, phase)
    }

    /// A request judged as one the CLI made in `phase`: by the mode's rule, the plan table, or (uncertain) by
    /// either only where both give the same answer; otherwise a card, or an armed driver answer for the same tool.
    pub fn on_request_judged(&mut self, request: Request, main_thread: bool, phase: Phase) -> Option<ToSend> {
        let tool_search = |rule: Option<PermissionDecision>| {
            rule.or_else(|| {
                (self.mode == Mode::Manual && request.origin == Origin::Hook && request.tool_name == "ToolSearch")
                    .then_some(PermissionDecision::Allow)
            })
        };
        let in_plan = tool_search(plan_automatic(
            self.mode,
            &request,
            PlanContext {
                main_thread,
                cli_runs_auto: self.cli_runs_auto,
                boundary: self.boundary,
            },
        ));
        let executing = tool_search(automatic(self.mode, request.origin));
        let rule = match phase {
            Phase::Executing => executing,
            Phase::Planning => in_plan,
            Phase::Uncertain => in_plan.filter(|decision| executing.as_ref() == Some(decision)),
        };
        if let Some(decision) = rule {
            self.automatic.push(Answered {
                request: request.clone(),
                decision: Some(decision.clone()),
            });
            return Some(ToSend {
                request,
                decision,
                source: Source::Automatic,
            });
        }
        let armed = self
            .armed
            .iter()
            .position(|(tool, _, origin)| *tool == request.tool_name && origin.is_none_or(|o| o == request.origin));
        let decision = armed.map(|i| decide(self.armed.remove(i).1));
        self.cards.push(Answered {
            request: request.clone(),
            decision: decision.clone(),
        });
        decision.map(|decision| ToSend {
            request,
            decision,
            source: Source::Driver,
        })
    }

    /// A hook request for a tool the session declared as its own, while the CLI reports one in-process server of
    /// that name: allowed in every mode, as the host's gate leaves such a call to the tool's own broker. The CLI's
    /// prompt for the same call is not covered: it goes through [`Gate::on_request`], so a prompt that follows this
    /// allow shows as a request the driver must answer.
    pub fn allow_own_tool(&mut self, request: Request) -> ToSend {
        let decision = PermissionDecision::Allow;
        self.automatic.push(Answered {
            request: request.clone(),
            decision: Some(decision.clone()),
        });
        ToSend {
            request,
            decision,
            source: Source::Automatic,
        }
    }

    /// The driver answers a card of `tool`. The oldest waiting request of that tool is answered now; with none
    /// waiting the answer is kept for the next one.
    pub fn driver_answer(&mut self, tool: &str, allow: bool) -> Option<ToSend> {
        self.driver_answer_from(tool, allow, None)
    }

    /// As [`Gate::driver_answer`], for a card of one `origin` only: the answer is not given to, or kept for, a
    /// request of the other (a hook request and the CLI's own prompt for the same call are two cards).
    pub fn driver_answer_from(&mut self, tool: &str, allow: bool, origin: Option<Origin>) -> Option<ToSend> {
        let waiting = self.cards.iter_mut().find(|c| {
            c.decision.is_none() && c.request.tool_name == tool && origin.is_none_or(|o| o == c.request.origin)
        });
        match waiting {
            Some(card) => {
                let decision = decide(allow);
                card.decision = Some(decision.clone());
                Some(ToSend {
                    request: card.request.clone(),
                    decision,
                    source: Source::Driver,
                })
            }
            None => {
                self.armed.push((tool.to_owned(), allow, origin));
                None
            }
        }
    }

    /// Answers every waiting request `Allow` (the "approve all pending" path): nothing while planning, where a
    /// sweep answers no card.
    pub fn approve_all(&mut self) -> Vec<ToSend> {
        if self.planning {
            return Vec::new();
        }
        self.cards
            .iter_mut()
            .filter(|c| c.decision.is_none())
            .map(|card| {
                card.decision = Some(PermissionDecision::Allow);
                ToSend {
                    request: card.request.clone(),
                    decision: PermissionDecision::Allow,
                    source: Source::Driver,
                }
            })
            .collect()
    }

    /// The oldest request that waits for a driver answer, if any.
    pub fn unanswered(&self) -> Option<&Request> {
        self.cards.iter().find(|c| c.decision.is_none()).map(|c| &c.request)
    }

    /// Every request that needed the driver, answered or not.
    pub fn cards_shown(&self) -> Vec<Answered> {
        self.cards.clone()
    }

    /// Every request the mode answered by itself.
    pub fn automatic_answers(&self) -> Vec<Answered> {
        self.automatic.clone()
    }
}

fn decide(allow: bool) -> PermissionDecision {
    if allow {
        PermissionDecision::Allow
    } else {
        PermissionDecision::Deny {
            reason: Some("denied by the harness driver".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str, tool: &str, origin: Origin) -> Request {
        Request {
            permission_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            tool_name: tool.into(),
            input: json!({}),
            origin,
        }
    }

    #[test]
    fn bypass_allows_hook_requests_and_cli_prompts() {
        for origin in [Origin::Hook, Origin::CliPrompt] {
            let mut gate = Gate::new(Mode::Bypass);
            let sent = gate
                .on_request(request("p1", "Bash", origin))
                .expect("answered at once");
            assert_eq!(sent.decision, PermissionDecision::Allow);
            assert_eq!(sent.source, Source::Automatic);
            assert_eq!(gate.automatic_answers().len(), 1);
            assert!(gate.cards_shown().is_empty());
            assert!(gate.unanswered().is_none());
        }
    }

    #[test]
    fn auto_defers_hook_requests_and_leaves_cli_prompts_to_the_driver() {
        let mut gate = Gate::new(Mode::Auto);
        let sent = gate
            .on_request(request("p1", "Bash", Origin::Hook))
            .expect("deferred at once");
        assert_eq!(sent.decision, PermissionDecision::Defer);
        assert_eq!(sent.source, Source::Automatic);
        assert!(gate.on_request(request("p2", "Bash", Origin::CliPrompt)).is_none());
        assert_eq!(gate.unanswered().map(|r| r.permission_id.as_str()), Some("p2"));
        assert_eq!(gate.automatic_answers().len(), 1);
        assert_eq!(gate.cards_shown().len(), 1);
        let sent = gate
            .driver_answer("Bash", true)
            .expect("the waiting prompt is answered");
        assert_eq!(sent.request.permission_id, "p2");
        assert_eq!(sent.source, Source::Driver);
        assert!(gate.unanswered().is_none());
    }

    #[test]
    fn manual_leaves_both_origins_to_the_driver() {
        for origin in [Origin::Hook, Origin::CliPrompt] {
            let mut gate = Gate::new(Mode::Manual);
            assert!(gate.on_request(request("p1", "Write", origin)).is_none());
            assert_eq!(gate.unanswered().map(|r| r.tool_name.as_str()), Some("Write"));
            assert!(gate.automatic_answers().is_empty());
            let sent = gate.driver_answer("Write", false).expect("answered");
            assert!(matches!(sent.decision, PermissionDecision::Deny { .. }));
            assert_eq!(gate.cards_shown()[0].decision, Some(sent.decision));
        }
    }

    #[test]
    fn a_manual_request_with_no_driver_answer_stays_unanswered_and_names_its_tool() {
        let mut gate = Gate::new(Mode::Manual);
        assert!(gate.on_request(request("p1", "Bash", Origin::Hook)).is_none());
        assert!(
            gate.driver_answer("Edit", true).is_none(),
            "an answer for another tool does not apply"
        );
        assert_eq!(gate.unanswered().map(|r| r.tool_name.as_str()), Some("Bash"));
    }

    #[test]
    fn an_answer_given_first_is_kept_for_the_next_request_of_its_tool() {
        let mut gate = Gate::new(Mode::Manual);
        assert!(gate.driver_answer("Bash", true).is_none());
        let sent = gate
            .on_request(request("p1", "Bash", Origin::Hook))
            .expect("armed answer applies");
        assert_eq!(sent.decision, PermissionDecision::Allow);
        assert_eq!(sent.source, Source::Driver);
        assert_eq!(gate.cards_shown().len(), 1);
        assert!(
            gate.on_request(request("p2", "Bash", Origin::Hook)).is_none(),
            "used once"
        );
    }

    #[test]
    fn a_declared_tool_of_the_hosts_own_is_allowed_by_the_hook_in_every_mode_and_its_prompt_is_not() {
        for mode in [Mode::Manual, Mode::Auto, Mode::Bypass] {
            let mut gate = Gate::new(mode);
            let sent = gate.allow_own_tool(request("p1", "mcp__eitri__open_file", Origin::Hook));
            assert_eq!(sent.decision, PermissionDecision::Allow, "{mode:?}");
            assert_eq!(sent.source, Source::Automatic);
            assert_eq!(gate.automatic_answers().len(), 1);
            assert!(gate.cards_shown().is_empty());
        }
        let mut gate = Gate::new(Mode::Manual);
        gate.allow_own_tool(request("p1", "mcp__eitri__open_file", Origin::Hook));
        assert!(gate
            .on_request(request("p2", "mcp__eitri__open_file", Origin::CliPrompt))
            .is_none());
        assert_eq!(gate.unanswered().map(|r| r.permission_id.as_str()), Some("p2"));
    }

    #[test]
    fn approve_all_answers_every_waiting_request() {
        let mut gate = Gate::new(Mode::Manual);
        gate.on_request(request("p1", "Bash", Origin::Hook));
        gate.on_request(request("p2", "Edit", Origin::Hook));
        let sent = gate.approve_all();
        assert_eq!(sent.len(), 2);
        assert!(gate.unanswered().is_none());
        assert!(gate.approve_all().is_empty());
    }

    #[test]
    fn answered_requests_are_logged_with_their_decision_word() {
        let mut gate = Gate::new(Mode::Manual);
        gate.on_request(request("p1", "Bash", Origin::CliPrompt));
        assert_eq!(gate.cards_shown()[0].to_json()["decision"], Value::Null);
        gate.driver_answer("Bash", true);
        let line = gate.cards_shown()[0].to_json();
        assert_eq!(line["decision"], "allow");
        assert_eq!(line["origin"], "cli_prompt");
        assert_eq!(line["tool"], "Bash");
    }
    #[test]
    fn a_manual_tab_allows_the_tool_search_hook_itself_and_nothing_else_new() {
        for planning in [false, true] {
            let mut gate = Gate::new(Mode::Manual);
            gate.set_planning(planning);
            let sent = gate.on_request(request("p1", "ToolSearch", Origin::Hook)).unwrap();
            assert_eq!(sent.decision, PermissionDecision::Allow);
            assert_eq!(sent.source, Source::Automatic);
            assert!(gate
                .on_request(request("p2", "ToolSearch", Origin::CliPrompt))
                .is_none());
            assert!(gate.on_request(request("p3", "Read", Origin::Hook)).is_none());
        }
    }

    #[test]
    fn while_planning_write_and_bash_are_left_to_the_cli_in_every_mode() {
        for mode in [Mode::Manual, Mode::Auto, Mode::Bypass] {
            let mut gate = Gate::new(mode);
            gate.set_planning(true);
            for tool in ["Write", "Bash"] {
                let sent = gate
                    .on_request(request("p1", tool, Origin::Hook))
                    .unwrap_or_else(|| panic!("{mode:?} {tool}: the plan table answers it"));
                assert_eq!(sent.decision, PermissionDecision::Defer, "{mode:?} {tool}");
                assert_eq!(sent.source, Source::Automatic);
            }
        }
    }

    #[test]
    fn while_planning_nothing_is_left_to_the_cli_at_a_root_that_is_no_boundary_and_bypass_needs_the_cli_on_auto() {
        let mut no_boundary = Gate::new(Mode::Bypass);
        no_boundary.set_planning(true);
        no_boundary.set_boundary(false);
        for tool in ["Write", "Bash", "Read"] {
            assert!(
                no_boundary.on_request(request("p1", tool, Origin::Hook)).is_none(),
                "{tool}"
            );
        }
        // Bypass leaves its other requests to the CLI only while the CLI runs auto; Write and Bash of the main
        // thread are left to the CLI's plan mode whatever it runs.
        let mut on_default = Gate::new(Mode::Bypass);
        on_default.set_planning(true);
        on_default.set_cli_runs_auto(false);
        assert!(on_default.on_request(request("p2", "Read", Origin::Hook)).is_none());
        assert_eq!(
            on_default
                .on_request(request("p3", "Write", Origin::Hook))
                .map(|s| s.decision),
            Some(PermissionDecision::Defer)
        );
        let mut on_auto = Gate::new(Mode::Bypass);
        on_auto.set_planning(true);
        assert_eq!(
            on_auto
                .on_request(request("p4", "Read", Origin::Hook))
                .map(|s| s.decision),
            Some(PermissionDecision::Defer)
        );
    }

    #[test]
    fn while_planning_no_mode_allows_a_tool_that_writes_or_runs() {
        for mode in [Mode::Manual, Mode::Auto, Mode::Bypass] {
            for tool in PLAN_ASKED_TOOLS {
                for origin in [Origin::Hook, Origin::CliPrompt] {
                    for main_thread in [true, false] {
                        let mut gate = Gate::new(mode);
                        gate.set_planning(true);
                        let sent = gate.on_request_in(request("p1", tool, origin), main_thread);
                        assert_ne!(
                            sent.map(|s| s.decision),
                            Some(PermissionDecision::Allow),
                            "{mode:?} {tool} {origin:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn while_planning_edit_a_subagents_call_and_every_prompt_are_cards() {
        let mut gate = Gate::new(Mode::Bypass);
        gate.set_planning(true);
        assert!(gate.on_request(request("p1", "Edit", Origin::Hook)).is_none());
        assert!(gate
            .on_request_in(request("p2", "Write", Origin::Hook), false)
            .is_none());
        assert!(gate.on_request(request("p3", "Write", Origin::CliPrompt)).is_none());
        assert!(gate.on_request(request("p4", "Read", Origin::CliPrompt)).is_none());
        assert_eq!(gate.cards_shown().len(), 4);
    }

    #[test]
    fn while_planning_other_requests_are_the_modes_own_and_a_sweep_answers_nothing() {
        let mut gate = Gate::new(Mode::Manual);
        gate.set_planning(true);
        assert!(gate.on_request(request("p1", "Read", Origin::Hook)).is_none());
        assert!(gate.approve_all().is_empty(), "a sweep answers no card while planning");
        gate.set_planning(false);
        assert_eq!(gate.approve_all().len(), 1, "and does again once the session executes");
        let mut auto = Gate::new(Mode::Auto);
        auto.set_planning(true);
        assert_eq!(
            auto.on_request(request("p2", "Read", Origin::Hook)).map(|s| s.decision),
            Some(PermissionDecision::Defer)
        );
        auto.set_mode(Mode::Manual);
        assert_eq!(auto.mode(), Mode::Manual);
    }
    #[test]
    fn an_answer_for_one_origin_is_not_given_to_a_card_of_the_other() {
        let mut gate = Gate::new(Mode::Manual);
        // Armed for the CLI's own prompt: the hook request waits, the prompt takes it.
        assert!(gate
            .driver_answer_from("Write", false, Some(Origin::CliPrompt))
            .is_none());
        assert!(gate.on_request(request("p1", "Write", Origin::Hook)).is_none());
        let sent = gate
            .on_request(request("p2", "Write", Origin::CliPrompt))
            .expect("the armed answer applies to the prompt");
        assert_eq!(sent.request.permission_id, "p2");
        assert!(matches!(sent.decision, PermissionDecision::Deny { .. }));
        // A waiting card of the other origin is not answered either.
        assert!(gate
            .driver_answer_from("Write", true, Some(Origin::CliPrompt))
            .is_none());
        let sent = gate
            .driver_answer_from("Write", true, Some(Origin::Hook))
            .expect("the hook card");
        assert_eq!(sent.request.permission_id, "p1");
    }
}
