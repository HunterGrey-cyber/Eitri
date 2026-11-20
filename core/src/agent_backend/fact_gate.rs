//! The gate's steps that read the server's path facts on a remote tab that has them (`GateRoot::RemoteWithFacts`):
//! what it holds of the facts when it judges a request ([`InHand`]), the hold while a fact the decision needs has
//! not come ([`when_missing`]), the fresh check before an automatic answer and the comparison once it is over
//! ([`AgentBackend::fresh_step`]), and the parts of `decide` that read the request's fact. The drain
//! (`AgentBackend::answer_what_needs_no_human`) keeps the order of the steps and what each outcome does to the
//! request.

use super::fact_waits::{FactRead, FreshPlan, FreshWait, MODE_CHANGED_DURING_THE_CHECK};
use super::{AgentBackend, HumanApprovals, Judge, PlanGate, PlanVerdict, PlannedAnswer};
use agent::gate_facts::{FreshFact, GateFactState, GATE_FACT_WAIT};
use agent::{Classification, GateFact, GateRoot, PermissionDecision, PermissionMode};

/// What the gate holds of the server's facts for one request when it judges it.
pub(super) struct InHand {
    /// The provider's fact for the request, looked up on a remote tab with facts when no fresh check is over.
    looked_up: Option<GateFactState>,
    /// A fresh check that is over and brought both facts: what it was started with, the server's first fact and the
    /// fresh one.
    checked: Option<(FreshPlan, GateFact, GateFact)>,
}

impl InHand {
    /// What the gate judges request `id` with: the fresh check's two facts once one is over, else the provider's
    /// fact on a remote tab with facts. `Err` with the reason for a card when the check settles nothing: it did not
    /// bring both facts, or it ended after the tab changed mode.
    pub(super) fn gather(
        backend: &AgentBackend,
        project_root: GateRoot<'_>,
        id: &str,
        fresh: Option<(FreshWait, FreshFact)>,
        tab_mode: PermissionMode,
    ) -> Result<InHand, String> {
        let checked = match fresh {
            None => None,
            Some((wait, FreshFact::Both { first, fresh })) if wait.plan.mode == tab_mode => {
                Some((wait.plan, first, fresh))
            }
            Some((_, FreshFact::Both { .. })) => return Err(MODE_CHANGED_DURING_THE_CHECK.to_owned()),
            Some((_, FreshFact::Refused(why))) => return Err(why),
            Some((_, FreshFact::Unavailable)) => return Err(agent::REASON_SERVER_FACT_MISSING.to_owned()),
        };
        let looked_up =
            (matches!(project_root, GateRoot::RemoteWithFacts) && checked.is_none()).then(|| backend.gate_fact(id));
        Ok(InHand { looked_up, checked })
    }

    /// The fact the decision is made with: the fresh one, or the provider's.
    pub(super) fn judged(&self) -> Option<&GateFact> {
        match (&self.checked, &self.looked_up) {
            (Some((_, _, fresh)), _) => Some(fresh),
            (None, Some(GateFactState::Have(have))) => Some(&have.fact),
            (None, _) => None,
        }
    }

    /// The fact the server held since it first saw the request, once a fresh check is over: the decision is made
    /// again with it.
    pub(super) fn first(&self) -> Option<&GateFact> {
        self.checked.as_ref().map(|(_, first, _)| first)
    }
}

/// What becomes of a request whose decision needed the server's fact and had none.
pub(super) enum Missing {
    /// Drawn as no card while the fact may still come; judged again when it does or its time is up.
    Hold,
    /// A card; why was logged.
    Card,
}

/// Where the decision of request `id` needed the server's fact and had none (`read.missing()`): held while the fact
/// may still come (`GATE_FACT_WAIT` from when the gate first held it), and a card after that or when it cannot (a
/// refused fact, a tab without facts). `None` when the decision had what it needed: the request no longer waits for
/// its fact.
pub(super) fn when_missing(
    approvals: &mut HumanApprovals,
    id: &str,
    tool_name: &str,
    in_hand: &InHand,
    read: &FactRead<'_>,
) -> Option<Missing> {
    if !read.missing() {
        approvals.facts.fact_settled(id);
        return None;
    }
    let why = match &in_hand.looked_up {
        Some(GateFactState::Waiting) => {
            let waited = approvals.facts.hold_for_fact(id, std::time::Instant::now());
            if waited < GATE_FACT_WAIT {
                if waited.is_zero() {
                    eprintln!("[permission] holding {tool_name} for the server's path check");
                }
                return Some(Missing::Hold);
            }
            agent::REASON_SERVER_FACT_MISSING.to_owned()
        }
        Some(GateFactState::Refused(why)) => why.clone(),
        _ => agent::REASON_SERVER_FACT_MISSING.to_owned(),
    };
    approvals.facts.fact_settled(id);
    eprintln!("[permission] asking the user: {tool_name} ({why})");
    Some(Missing::Card)
}

/// What the fresh check says of an automatic answer.
pub(super) enum FreshStep {
    /// It goes out; with the fact it rests on and the request's digest when it read one (checked fresh again if it
    /// is lost and sent a second time).
    Answer(Option<(GateFact, [u8; 32])>),
    /// A fresh check began: the request is drawn as no card and judged again once it is over.
    Started,
    /// A card; why was logged.
    Card,
}

impl AgentBackend {
    /// An automatic answer whose decision read the server's fact (`read_a_fact`: its places check, its boundary, its
    /// classification, a rule over it, a deferral at its boundary, plan mode's Defer) goes out only after a fresh
    /// check on the server: the first time the gate plans it, the check begins and never waits here (its answer wakes
    /// the tab). Once it is over, the answer goes out only when the decision with the fresh fact and with the
    /// server's first fact (`again`) both equal the one planned, that first fact equals the one the answer was
    /// planned from, and the request's id still names the request the fact was about (the provider's digest for it
    /// is the one the check was asked with). Any other way is a card.
    pub(super) fn fresh_step(
        &self,
        approvals: &mut HumanApprovals,
        in_hand: InHand,
        read_a_fact: bool,
        planned: &PlannedAnswer,
        again: Option<PermissionDecision>,
        mode: PermissionMode,
    ) -> FreshStep {
        if !read_a_fact {
            return FreshStep::Answer(None);
        }
        let (id, tool_name, decision) = (planned.permission_id.as_str(), &planned.tool_name, &planned.decision);
        let why = match (in_hand.checked, in_hand.looked_up) {
            (None, Some(GateFactState::Have(have))) => {
                let plan = FreshPlan {
                    first: have.fact.clone(),
                    digest: have.request_digest,
                    mode,
                    decision: decision.clone(),
                };
                let done = approvals.facts.start_fresh(id, plan, std::time::Instant::now());
                eprintln!("[permission] checking {tool_name} on the server before answering");
                self.check_fact(id, have.request_digest, done);
                return FreshStep::Started;
            }
            (Some((plan, first, _)), _) => {
                if self.gate_fact_digest(id) != Some(plan.digest) {
                    agent::gate_facts::REFUSED_FOR_ANOTHER_REQUEST
                } else if *decision == plan.decision && again.as_ref() == Some(&plan.decision) && first == plan.first {
                    return FreshStep::Answer(Some((plan.first, plan.digest)));
                } else {
                    agent::REASON_SERVER_FACT_CHANGED
                }
            }
            // Neither a fact to check nor a check that brought both: nothing an answer could rest on.
            _ => agent::REASON_SERVER_FACT_CHANGED,
        };
        eprintln!("[permission] asking the user: {tool_name} ({why})");
        FreshStep::Card
    }
}

/// What the tool's name alone decides on a remote tab with the server's facts (`classify_by_name`); `None` elsewhere
/// and for a call judged by its paths.
pub(super) fn decided_by_name(
    project_root: GateRoot<'_>,
    tool_name: &str,
    input: &serde_json::Value,
) -> Option<Classification> {
    match project_root {
        GateRoot::RemoteWithFacts => agent::permission_policy::classify_by_name(tool_name, input),
        _ => None,
    }
}

/// On a remote tab with the server's facts, the places check from the request's fact (`places_card`), where a local
/// tab runs it on its own disk: in every mode but Bypass, and in plan mode whatever the tab's mode. What the tool's
/// name decides needs none: the places check never judges a tool allowed by name, and one carded by its name is a
/// card whatever the places say, unless a boundary could leave it to the CLI (Auto on a session running the CLI's
/// auto mode, or plan mode), where the check comes first as it does locally. `Some(true)` for a card (logged),
/// `Some(false)` for none, `None` when the check needs the fact and there is none.
pub(super) fn places_card_from_fact(
    tool_name: &str,
    by_name: Option<Classification>,
    judge: Judge<'_>,
) -> Option<bool> {
    let Judge {
        project_root,
        mode,
        may_defer,
        planning,
        facts,
        ..
    } = judge;
    let checked =
        matches!(project_root, GateRoot::RemoteWithFacts) && (mode != PermissionMode::Bypass || planning.is_some());
    let needs_no_fact = by_name.is_some_and(|by_name| !by_name.needs_a_human() || (!may_defer && planning.is_none()));
    if !checked || needs_no_fact {
        return Some(false);
    }
    match facts.get()?.places_card {
        Some(card) => {
            eprintln!("[permission] asking the user: {tool_name} ({})", card.reason);
            Some(true)
        }
        None => Some(false),
    }
}

/// Plan mode's table for `tool_name` at the tab's root: on a remote tab with the server's facts the root's boundary
/// comes from the request's fact, read only where it changes the table's answer (`None` when it does and there is no
/// fact); a local root is asked on this machine, and a remote one without facts is no boundary.
pub(super) fn plan_verdict_at(
    tool_name: &str,
    gate: PlanGate,
    project_root: GateRoot<'_>,
    facts: &FactRead<'_>,
) -> Option<PlanVerdict> {
    let at = |boundary| super::plan_verdict(tool_name, false, gate, boundary);
    match project_root {
        GateRoot::RemoteWithFacts if at(true) != at(false) => facts.get().map(|fact| at(fact.root_is_boundary)),
        GateRoot::RemoteWithFacts => Some(at(false)),
        _ => Some(at(super::gate_root_is_a_boundary(project_root))),
    }
}

/// On a remote tab with the server's facts, where an Auto tab on a session running the CLI's auto mode leaves the
/// call to the CLI: exactly where a local tab would, at a root the request's fact calls a boundary. `Some(Some(fact))`
/// there, `Some(None)` where it does not defer, `None` when it may and the fact is missing.
pub(super) fn remote_deferral<'f>(
    may_defer: bool,
    project_root: GateRoot<'_>,
    facts: &FactRead<'f>,
) -> Option<Option<&'f GateFact>> {
    if !may_defer || !matches!(project_root, GateRoot::RemoteWithFacts) {
        return Some(None);
    }
    let fact = facts.get()?;
    Some(fact.root_is_boundary.then_some(fact))
}
