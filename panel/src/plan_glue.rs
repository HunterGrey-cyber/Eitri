//! The panel's half of plan mode: a page's decision on Claude's plan and its own switch into or out of plan
//! mode, each on its way to the agent while the tick polls for the reply, and the guard on a move the CLI does
//! not confirm. Only a `plan_resolve` from the page that shows the tab, past the window's answer gate
//! (`admission`), reaches `TabSet::approve_plan` / `keep_planning`: nothing else settles a plan.

use super::*;
use eitri_core::agent_backend::{PlanSend, PlanningSwitch};
use eitri_core::agent_bridge::{serialize_plan_waiting_for_js, PlanDecision, PlanResolveMessage};

/// A decision on one of Claude's plans on its way (or, for an approval, waiting for calls the CLI is still
/// deciding in plan mode), and the page request its reply answers.
pub(super) struct PendingPlanSend {
    tab: TabId,
    view: ViewId,
    request_id: String,
    send: PlanSend,
    /// The page that sent it was reloaded: its request is answered by nobody, and a refusal refreshes the tab.
    pub(super) orphaned: bool,
    /// How many calls the page was last told the approval waits for.
    told_waiting: usize,
}

impl PendingPlanSend {
    pub(super) fn view(&self) -> ViewId {
        self.view
    }
}

/// A person's switch into or out of plan mode on its way, and the page request its reply answers.
pub(super) struct PendingPlanningSwitch {
    tab: TabId,
    view: ViewId,
    request_id: String,
    switch: PlanningSwitch,
    pub(super) orphaned: bool,
}

impl PendingPlanningSwitch {
    pub(super) fn view(&self) -> ViewId {
        self.view
    }
}

/// A `plan_resolve` already admitted: checked as a decision, then taken by the tab set, which checks the page,
/// the plan and the digest again. A refusal is told now and leaves the plan waiting; a decision on its way is
/// told by the tick (`poll_plan_sends`).
pub(super) fn resolve_plan(
    state: &mut AgentPanelState,
    screen: &Screen,
    view: ViewId,
    tab: TabId,
    message: PlanResolveMessage,
    out: &mut Outbox,
) {
    let request_id = message.request_id.clone();
    let decision = match message.decision() {
        Ok(decision) => decision,
        Err(protocol) => {
            out.send(view, serialize_command_result_for_js(&request_id, Err(&protocol)));
            return;
        }
    };
    let sent = match decision {
        PlanDecision::Approve(choice) => state.tabs.approve_plan(
            tab,
            view,
            &message.interaction_id,
            choice,
            &message.plan_sha256,
            now_ms(),
        ),
        PlanDecision::KeepPlanning { feedback } => {
            state.tabs.keep_planning(tab, view, &message.interaction_id, feedback)
        }
    };
    match sent {
        Ok(send) => {
            let waiting = send.waiting_calls();
            if waiting > 0 {
                out.send(view, serialize_plan_waiting_for_js(&request_id, tab, waiting));
            }
            state.plan_sends.push(PendingPlanSend {
                tab,
                view,
                request_id,
                send,
                orphaned: false,
                told_waiting: waiting,
            });
        }
        Err(error) => apply_command_outcome(state, screen, view, tab, &request_id, Err(error), out),
    }
}

/// A `set_planning` already admitted. On a tab with no session it only marks the tab, and is answered at once;
/// a switch sent is answered by the tick (`poll_planning_switches`), and the CLI's report confirms the move.
pub(super) fn set_planning(
    state: &mut AgentPanelState,
    screen: &Screen,
    view: ViewId,
    tab: TabId,
    request_id: &str,
    enter: bool,
    out: &mut Outbox,
) {
    match state.tabs.set_planning(tab, view, enter, now_ms()) {
        Ok(None) => {
            send_tabs(state, out);
            out.send(view, serialize_command_result_for_js(request_id, Ok(())));
        }
        Ok(Some(switch)) => state.planning_switches.push(PendingPlanningSwitch {
            tab,
            view,
            request_id: request_id.to_string(),
            switch,
            orphaned: false,
        }),
        Err(error) => apply_command_outcome(state, screen, view, tab, request_id, Err(error), out),
    }
}

/// Tells each page whose plan decision the agent replied to (`command_result`), and the page of an approval
/// still waiting for calls how many are left (`plan_waiting`) when that changes. Every refusal or failure is
/// benign: the plan waits again and the session goes on. The reply goes to the session first
/// (`TabSet::finish_plan_send`), which puts the plan back to waiting and drops the tab to Manual after an
/// approval whose outcome is unknown.
pub(super) fn poll_plan_sends(state: &mut AgentPanelState, out: &mut Outbox) {
    if state.plan_sends.is_empty() {
        return;
    }
    let now = now_ms();
    let mut refresh = Vec::new();
    let mut changed = false;
    let AgentPanelState { plan_sends, tabs, .. } = state;
    plan_sends.retain_mut(
        |pending| match tabs.finish_plan_send(pending.tab, &mut pending.send, now) {
            None => {
                let waiting = pending.send.waiting_calls();
                if waiting != pending.told_waiting && !pending.orphaned {
                    pending.told_waiting = waiting;
                    out.send(
                        pending.view,
                        serialize_plan_waiting_for_js(&pending.request_id, pending.tab, waiting),
                    );
                }
                true
            }
            Some(reply) => {
                changed = true;
                if pending.orphaned {
                    if reply.is_err() {
                        refresh.push(pending.tab);
                    }
                    return false;
                }
                if let Err(error) = &reply {
                    eprintln!(
                        "[agent_panel] tab {}: plan decision not taken (session stays alive): {}",
                        pending.tab.0, error.message
                    );
                }
                let told = match &reply {
                    Ok(()) => serialize_command_result_for_js(&pending.request_id, Ok(())),
                    Err(error) => serialize_command_result_for_js(&pending.request_id, Err(&error.message)),
                };
                out.send(pending.view, told);
                false
            }
        },
    );
    for tab in refresh {
        if let Some(view) = state.page_of(tab) {
            for payload in state.tabs.refresh_payloads(tab) {
                out.send(view, payload);
            }
        }
    }
    // An approval whose outcome is unknown may have moved the tab's mode.
    if changed {
        send_tabs(state, out);
    }
}

/// Tells each page whose switch into or out of plan mode the agent replied to. The reply goes to the session
/// first (`TabSet::finish_planning_switch`): a refusal undoes the move, and the guard counts from the reply.
pub(super) fn poll_planning_switches(state: &mut AgentPanelState, out: &mut Outbox) {
    if state.planning_switches.is_empty() {
        return;
    }
    let AgentPanelState {
        planning_switches,
        tabs,
        ..
    } = state;
    planning_switches.retain(
        |pending| match tabs.finish_planning_switch(pending.tab, &pending.switch) {
            None => true,
            Some(_) if pending.orphaned => false,
            Some(reply) => {
                let told = match &reply {
                    Ok(()) => serialize_command_result_for_js(&pending.request_id, Ok(())),
                    Err(error) => serialize_command_result_for_js(&pending.request_id, Err(&error.message)),
                };
                out.send(pending.view, told);
                false
            }
        },
    );
}

/// Fails every tab whose move into or out of plan mode the CLI did not confirm in time
/// (`TabSet::check_plan_transitions`), as the CLI-mode tripwire does: the retired session is shut down off
/// this thread, which denies what is still pending in it, and a page that shows the tab says why.
pub(super) fn check_plan_transitions(state: &mut AgentPanelState, out: &mut Outbox) {
    for tripped in state.tabs.check_plan_transitions(now_ms()) {
        let shown = state.shown_in(tripped.tab);
        state.retiring.backend(tripped.retired);
        if let Some(view) = shown {
            out.send(view, serialize_error_for_js(tripped.tab, &tripped.reason));
        }
    }
}
