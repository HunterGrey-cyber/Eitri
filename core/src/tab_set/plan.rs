//! A tab's side of plan mode: the person's own switch into and out of it, the decision on a plan, the mode the
//! tab takes up when its session executes again, and the guard on a move the CLI does not confirm.
//!
//! The session's phase is the projection's (`agent::PlanPhase`), moved only by the CLI's own reports. The tab
//! keeps the mode plan mode was entered from (`Tab::plan_origin`) and shows it underneath while planning; the
//! gate reads that mode as the plan table's origin (`agent_backend::plan_verdict`). When the session executes
//! again the tab takes up the mode an approved plan names, whichever window sent the approval
//! (`latest_approval`), or Manual after a leave of the person's own; until it has, nothing is answered
//! automatically (`GateMode::plan_followed`).

use super::*;
use crate::agent_backend::{PlanSend, PlanSendState, PlanningSwitch};
use agent::{PlanExecutionMode, PlanPhase, PLAN_TRANSITION_GUARD_MS};

/// What a person approves a plan into: the plan dock's three approvals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanChoice {
    ApproveManual,
    ApproveAuto,
    /// Offered only when plan mode started in Bypass, and never by default.
    ApproveBypass,
}

impl PlanChoice {
    fn mode(self) -> PlanExecutionMode {
        match self {
            PlanChoice::ApproveManual => PlanExecutionMode::Manual,
            PlanChoice::ApproveAuto => PlanExecutionMode::Auto,
            PlanChoice::ApproveBypass => PlanExecutionMode::Bypass,
        }
    }
}

/// What `cycle_mode` and `confirm_bypass` say while the session plans.
pub const PLAN_CYCLE_REFUSAL: &str = "in plan mode: <leader>p leaves plan mode";
/// What they say while a move into or out of plan mode is under way.
pub const PLAN_CHANGING_REFUSAL: &str = "the mode is changing; try again";
/// What a second approval says while the first still waits for calls Claude Code is deciding in plan mode.
pub const PLAN_APPROVAL_DRAINING: &str =
    "an approval of this plan is already waiting for Claude Code's calls to finish";

fn benign(message: impl Into<String>) -> BackendError {
    BackendError {
        message: message.into(),
        benign: true,
    }
}

/// What an approval says while a call it would wait for has a card up: the person decides the call first.
pub const PLAN_CARD_FIRST: &str =
    "Claude Code is asking about a call it made in plan mode: answer its card first; the plan is still waiting";

/// Whether a call deferred in plan mode waits for a person's answer on a card: an approval does not wait for a
/// person, so it is refused with [`PLAN_CARD_FIRST`] rather than counted down.
fn a_deferred_call_is_carded(session: &SessionState, projection: &agent::AgentSessionProjection) -> bool {
    let hidden = session.hidden_permissions(projection);
    projection.pending_permissions.values().any(|request| {
        !hidden.contains(&request.permission_id)
            && request
                .tool_use_id
                .as_ref()
                .is_some_and(|id| session.plan.deferred_in_flight.contains(id))
    })
}

/// Why an approval cannot go yet although no call holds it: the session is not in plan mode by the CLI's own
/// report, or a request it made has not been judged yet. The sidecar sends a plan ahead of its message events, so
/// the dock can show one before the CLI's `plan` report is folded; an approval sent then would find the phase
/// still `Entering`, lose its move out of plan mode, and leave the CLI's report of its creation mode unexplained.
/// A request still waiting for its call, or folded since the last drain and not looked at yet
/// (`PlanBook::looked_at`), may be one the CLI made in plan mode and is still deciding there: judged only after the
/// approval went out, a deferral to the CLI would no longer hold the approval back.
fn approval_not_ready(session: &SessionState, projection: &agent::AgentSessionProjection) -> Option<&'static str> {
    if projection.planning.phase != PlanPhase::Planning {
        return Some("Claude Code has not reported plan mode yet");
    }
    let waiting = session
        .plan
        .unsequenced
        .keys()
        .any(|id| projection.pending_permissions.contains_key(id));
    let unseen = projection
        .pending_permissions
        .keys()
        .any(|id| !session.plan.looked_at.contains(id));
    (waiting || unseen).then_some("a request Claude Code made has not been judged yet")
}

/// The tab mode an approval names. Bypass is kept only for a round that started in Bypass; any other round
/// lands in Manual, the stricter side.
fn mode_after(named: Option<PlanExecutionMode>, origin: Option<SessionModeChoice>) -> SessionModeChoice {
    match named {
        Some(PlanExecutionMode::Auto) => SessionModeChoice::Auto,
        Some(PlanExecutionMode::Bypass) if origin == Some(SessionModeChoice::Bypass) => SessionModeChoice::Bypass,
        Some(PlanExecutionMode::Bypass) | Some(PlanExecutionMode::Manual) | None => SessionModeChoice::Manual,
    }
}

impl TabSet {
    /// Why `id`'s mode may not be cycled or confirmed into Bypass now: its session plans, or a move into or
    /// out of plan mode has not settled for the tab yet.
    pub(super) fn plan_mode_refusal(&self, id: TabId) -> Option<&'static str> {
        let tab = self.get(id)?;
        let backend = tab.live()?;
        let phase = backend.projection().planning.phase.clone();
        match phase {
            PlanPhase::Planning => Some(PLAN_CYCLE_REFUSAL),
            PlanPhase::Entering { .. } | PlanPhase::Leaving { .. } => Some(PLAN_CHANGING_REFUSAL),
            PlanPhase::Executing if tab.plan_cycle.open => Some(PLAN_CHANGING_REFUSAL),
            PlanPhase::Executing => None,
        }
    }

    /// The tab `id`, when `page` may act on it now. `on_screen`: the act loosens what the session may do (an
    /// approval, a switch into or out of plan mode), so the tab must also be the one its page shows, as for
    /// entering Bypass: a base-page tab in the background has nobody looking at its plan.
    fn plan_tab(&mut self, id: TabId, page: ViewId, on_screen: bool) -> Result<&mut Tab, BackendError> {
        if self.moving.contains(&id.0) {
            return Err(benign(MOVING));
        }
        if let Some(tab) = self.get(id) {
            if !self.may_answer(id, page) {
                return Err(benign(format!("tab {} is not shown in this page", tab.number)));
            }
            if on_screen && !self.is_shown(id) {
                return Err(benign(format!("tab {} is not the one on screen", tab.number)));
            }
        }
        self.get_mut(id).ok_or_else(|| benign("no active session"))
    }

    /// The person's own switch of tab `id` into plan mode (`enter`) or out of it, from `page`.
    ///
    /// On a tab with no session yet it only marks the tab (`plan_on_start`, `None`): its first message then
    /// waits until the session it opens has entered plan mode. On a live Claude session that offers plan
    /// mode it sends the switch, but only while the tab is idle: no turn, no card, question or plan waiting,
    /// no message queued. Every refusal is benign.
    pub fn set_planning(
        &mut self,
        id: TabId,
        page: ViewId,
        enter: bool,
        now_ms: u64,
    ) -> Result<Option<PlanningSwitch>, BackendError> {
        let sidecar = self.kind == BackendKind::Sidecar;
        let tab = self.plan_tab(id, page, true)?;
        if tab.agent != AgentId::Claude || !sidecar {
            return Err(benign("plan mode needs a Claude tab on a sidecar that offers it"));
        }
        if matches!(tab.backend, TabBackend::NotStarted) {
            tab.plan_on_start = enter;
            return Ok(None);
        }
        // A start in plan mode that the agent refused: entering tries again, leaving lets the waiting
        // message go as it is.
        let start_refused = tab.plan_on_start && tab.plan_start.is_none();
        if !enter && start_refused {
            tab.plan_on_start = false;
            tab.session.queue_note = None;
            return Ok(None);
        }
        let backend = tab.live().ok_or_else(|| benign("the session is not running"))?;
        let (offered, busy) = {
            let projection = backend.projection();
            let cards = projection.pending_permissions.keys().any(|id| {
                !tab.session.host_answered.contains(id)
                    && !tab.session.plan.held.contains(id)
                    && !tab.session.plan.unsequenced.contains_key(id)
            });
            let busy = projection.active_turn_id.is_some()
                || projection.cli_unseen_busy
                || cards
                || projection.waiting_questions().next().is_some()
                || projection.waiting_plan().is_some()
                || (!tab.session.queue.is_empty() && !start_refused);
            (projection.planning.offered, busy)
        };
        if !offered || !backend.capabilities().plan_mode_v1 {
            return Err(benign("plan mode is not offered on this session"));
        }
        if busy {
            return Err(benign(if enter {
                "plan mode starts when the tab is idle"
            } else {
                "plan mode ends when the tab is idle"
            }));
        }
        let backend = tab.live_mut().expect("checked live above");
        let switch = backend.set_planning(enter, now_ms)?;
        if enter && start_refused {
            tab.session.queue_note = None;
        }
        Ok(Some(switch))
    }

    /// What became of `switch`, sent for tab `id`: `None` while it is on its way. The reply goes to the
    /// session, so a refusal undoes the move and the guard counts from the reply; the CLI's report
    /// confirms a move that was taken.
    pub fn finish_planning_switch(&mut self, id: TabId, switch: &PlanningSwitch) -> Option<Result<(), BackendError>> {
        match self.get_mut(id).and_then(Tab::live_mut) {
            Some(backend) => backend.finish_planning_switch(switch),
            None => switch
                .reply()
                .map(|reply| reply.map(|_| ()).map_err(|e| benign(e.to_string()))),
        }
    }

    /// Approves the plan `interaction_id` of tab `id` from `page` into `choice`'s mode, naming the digest of
    /// the plan the person read.
    ///
    /// The decision is checked against the plan first. While a call the CLI was left to decide in plan mode
    /// is still running, the approval is not sent: it waits (`PlanSend::waiting_calls`) until those calls
    /// finish, read by [`Self::finish_plan_send`], and is refused once [`PLAN_TRANSITION_GUARD_MS`] has
    /// passed, the plan still waiting. It also waits, the same way, until the CLI's report of plan mode has been
    /// folded and no request of the session waits for its call (`approval_not_ready`). "Approve and keep Bypass" is
    /// refused unless plan mode started in Bypass. The tab's mode afterwards comes from the plan's settlement ([`Self::follow_plan_mode`]).
    pub fn approve_plan(
        &mut self,
        id: TabId,
        page: ViewId,
        interaction_id: &str,
        choice: PlanChoice,
        plan_sha256: &str,
        now_ms: u64,
    ) -> Result<PlanSend, BackendError> {
        let tab = self.plan_tab(id, page, true)?;
        // One approval at a time: a second one while the first waits for calls would be refused only once
        // the first had gone out, so the person's later choice would lose to the earlier one.
        if tab.session.plan.draining.as_deref() == Some(interaction_id) {
            return Err(benign(PLAN_APPROVAL_DRAINING));
        }
        if choice == PlanChoice::ApproveBypass && tab.plan_origin != Some(SessionModeChoice::Bypass) {
            return Err(benign(
                "Approve and keep Bypass is offered only when plan mode started in Bypass",
            ));
        }
        let mode = choice.mode();
        let TabBackend::Live(backend) = &mut tab.backend else {
            return Err(benign("no active session"));
        };
        let (waiting, not_ready) = {
            let projection = backend.projection();
            let checked = match projection.plan(interaction_id) {
                Some(record) => record.check_resolution(&agent::InteractionResolution::PlanApprove {
                    mode,
                    plan_sha256: plan_sha256.to_string(),
                }),
                None => Err("no plan with that id is waiting for a decision".to_string()),
            };
            checked.map_err(benign)?;
            tab.session.plan.prune_deferred(&projection);
            if a_deferred_call_is_carded(&tab.session, &projection) {
                return Err(benign(PLAN_CARD_FIRST));
            }
            (
                tab.session.plan.deferred_in_flight.len(),
                approval_not_ready(&tab.session, &projection).is_some(),
            )
        };
        if waiting > 0 || not_ready {
            tab.session.plan.draining = Some(interaction_id.to_string());
            return Ok(PlanSend::draining(interaction_id, mode, plan_sha256, now_ms, waiting));
        }
        backend.approve_plan(interaction_id, mode, plan_sha256)
    }

    /// "Keep planning" on the plan `interaction_id` of tab `id` from `page`, with the person's `feedback`.
    pub fn keep_planning(
        &mut self,
        id: TabId,
        page: ViewId,
        interaction_id: &str,
        feedback: String,
    ) -> Result<PlanSend, BackendError> {
        let tab = self.plan_tab(id, page, false)?;
        tab.live_mut()
            .ok_or_else(|| benign("no active session"))?
            .keep_planning(interaction_id, feedback)
    }

    /// What became of `send`, a decision on a plan of tab `id`, at `now_ms`: `None` while it is on its way
    /// or still waiting for calls to finish (an approval goes out here once they have). A refused decision
    /// leaves the plan waiting and the tab's mode as it was. An approval whose outcome is unknown (a timeout
    /// or a lost connection after it left) puts the tab in Manual unless its settlement already set a mode:
    /// never back in the mode plan mode started from, which may be Bypass. A settlement that arrives later
    /// still sets its mode.
    pub fn finish_plan_send(
        &mut self,
        id: TabId,
        send: &mut PlanSend,
        now_ms: u64,
    ) -> Option<Result<(), BackendError>> {
        let draining = matches!(send.state, PlanSendState::Draining { .. });
        let outcome = self.finish_plan_send_inner(id, send, now_ms);
        // Once a waiting approval went out or was refused, another may be made.
        if draining && (outcome.is_some() || !matches!(send.state, PlanSendState::Draining { .. })) {
            if let Some(tab) = self.get_mut(id) {
                if tab.session.plan.draining.as_deref() == Some(send.interaction_id()) {
                    tab.session.plan.draining = None;
                }
            }
        }
        outcome
    }

    fn finish_plan_send_inner(
        &mut self,
        id: TabId,
        send: &mut PlanSend,
        now_ms: u64,
    ) -> Option<Result<(), BackendError>> {
        let moving = self.moving.contains(&id.0);
        let approval = send.approval();
        let interaction_id = send.interaction_id().to_string();
        let Some(tab) = self.get_mut(id) else {
            return match &send.state {
                PlanSendState::Draining { .. } => Some(Err(benign("the tab is gone"))),
                PlanSendState::Sent(_) => send.reply().map(|reply| reply.map_err(|e| benign(e.to_string()))),
            };
        };
        if let PlanSendState::Draining {
            plan_sha256,
            since_ms,
            waiting,
        } = &mut send.state
        {
            let TabBackend::Live(backend) = &mut tab.backend else {
                return Some(Err(benign("no active session")));
            };
            let not_ready = {
                let projection = backend.projection();
                tab.session.plan.prune_deferred(&projection);
                if a_deferred_call_is_carded(&tab.session, &projection) {
                    return Some(Err(benign(PLAN_CARD_FIRST)));
                }
                approval_not_ready(&tab.session, &projection)
            };
            *waiting = tab.session.plan.deferred_in_flight.len();
            if *waiting > 0 || not_ready.is_some() || moving {
                if now_ms.saturating_sub(*since_ms) > PLAN_TRANSITION_GUARD_MS {
                    let seconds = PLAN_TRANSITION_GUARD_MS / 1000;
                    return Some(Err(benign(match not_ready.filter(|_| *waiting == 0) {
                        Some(why) => format!("{why} after {seconds} s; the plan is still waiting"),
                        None => {
                            let calls = if *waiting == 1 { "call" } else { "calls" };
                            format!(
                                "{waiting} {calls} Claude Code was deciding in plan mode did not finish within \
                                 {seconds} s; the plan is still waiting"
                            )
                        }
                    })));
                }
                return None;
            }
            let mode = approval.expect("only an approval waits");
            let plan_sha256 = plan_sha256.clone();
            let sent = match backend.approve_plan(&interaction_id, mode, &plan_sha256) {
                Ok(sent) => sent,
                Err(error) => return Some(Err(error)),
            };
            send.state = sent.state;
        }
        let TabBackend::Live(backend) = &mut tab.backend else {
            return send.reply().map(|reply| reply.map_err(|e| benign(e.to_string())));
        };
        match backend.finish_plan_send(send)? {
            Ok(()) => Some(Ok(())),
            Err(failure) => {
                if failure.outcome_unknown && approval.is_some() {
                    let settled = backend
                        .projection()
                        .latest_approval()
                        .is_some_and(|(seq, _)| Some(seq) > tab.plan_mode_applied_seq);
                    if !settled {
                        tab.mode = SessionModeChoice::Manual;
                        tab.plan_origin = None;
                    }
                }
                Some(Err(failure.error))
            }
        }
    }

    /// Fails every tab whose move into or out of plan mode the CLI did not confirm within
    /// [`PLAN_TRANSITION_GUARD_MS`], through the same path as an ungated mode: the session is handed out
    /// retired, for the caller to shut down. Run on the panel's tick.
    pub fn check_plan_transitions(&mut self, now_ms: u64) -> Vec<Tripped> {
        let mut out = PumpOutput::empty();
        let shown: BTreeSet<u64> = self.shown().into_iter().map(|(_, tab)| tab.0).collect();
        for tab in &mut self.tabs {
            if self.moving.contains(&tab.id.0) {
                continue;
            }
            let Some(reason) = tab.live().and_then(|backend| backend.plan_transition_overdue(now_ms)) else {
                continue;
            };
            let on_screen = shown.contains(&tab.id.0);
            Self::pump_trip(tab, &mut self.retiring, reason, on_screen, &mut out);
        }
        if !out.tripped.is_empty() {
            self.sync_presence();
        }
        out.tripped
    }

    /// Takes up what plan mode did to tab `tab`'s mode, from the projection; run on every pump, before and
    /// after the drain. A round opens when the session leaves executing and the tab's mode then is its
    /// origin. An approved plan's mode is taken up once, whichever window sent the approval. When the
    /// session executes again the round closes: after a round that reached plan mode and took up no
    /// approval (a leave of the person's own), the tab is in Manual; an entry that came to nothing, or a
    /// session that ended, leaves the mode as it was.
    pub(super) fn follow_plan_mode(tab: &mut Tab) {
        let ended = matches!(tab.wire_state(), TabStateWire::Ended | TabStateWire::Failed);
        let TabBackend::Live(backend) = &tab.backend else {
            return;
        };
        let (planning, reached, latest) = {
            let projection = backend.projection();
            (
                projection.is_planning(),
                matches!(projection.planning.phase, PlanPhase::Planning)
                    || matches!(projection.planning.phase, PlanPhase::Leaving { .. })
                    || projection.plan_gate_applies(),
                projection.latest_approval(),
            )
        };
        let latest_seq = latest.map(|(seq, _)| seq);
        let Some(applied) = tab.plan_mode_applied_seq else {
            // The first look at this session: whatever was approved before is not this window's to take up.
            tab.plan_mode_applied_seq = Some(latest_seq.unwrap_or(0));
            if !planning {
                return;
            }
            return Self::open_plan_cycle(tab, reached);
        };
        if planning && !tab.plan_cycle.open {
            Self::open_plan_cycle(tab, reached);
        }
        if planning && reached {
            tab.plan_cycle.reached = true;
            tab.plan_on_start = false;
        }
        if let Some((seq, named)) = latest.filter(|(seq, _)| *seq > applied) {
            tab.plan_mode_applied_seq = Some(seq);
            tab.mode = mode_after(named, tab.plan_cycle.origin);
            tab.plan_cycle.taken = true;
        }
        if !planning && tab.plan_cycle.open {
            if !ended && tab.plan_cycle.reached && !tab.plan_cycle.taken {
                tab.mode = SessionModeChoice::Manual;
            }
            tab.plan_cycle.open = false;
            tab.plan_origin = None;
        }
    }

    /// Notes where tab `tab`'s session stands in plan mode, for the page: the projection's phase and the tab's
    /// origin, after the pump took up what plan mode did to the tab. A change is marked for the next events
    /// envelope (`planningChanged`); a session that does not offer plan mode has no note.
    pub(super) fn note_planning(tab: &mut Tab) {
        let note = tab.live().and_then(|backend| {
            let projection = backend.projection();
            projection
                .planning
                .offered
                .then(|| crate::agent_bridge::PlanningNote::of(&projection.planning.phase, tab.plan_origin))
        });
        if note != tab.notes.planning {
            tab.notes.planning = note;
            // A note that went away (the session behind the tab no longer offers plan mode) is said too, as
            // `planningChanged: null`: the page would otherwise keep showing plan mode until a snapshot.
            tab.notes.planning_dirty = true;
        }
    }

    /// The page's mirror of the "keep planning" feedback typed for the plan `interaction_id` of tab `id`, from
    /// `page`. Kept only for a plan still waiting, at most [`agent::PLAN_FEEDBACK_MAX_BYTES`]; empty text drops
    /// it. A mirror only: nothing here sends a decision. It goes with the tab on a move and is dropped when the
    /// plan settles.
    pub fn set_plan_feedback_draft(
        &mut self,
        id: TabId,
        page: ViewId,
        interaction_id: &str,
        text: String,
    ) -> Result<(), String> {
        if self.moving.contains(&id.0) {
            return Err(MOVING.into());
        }
        let tab = self.get(id).ok_or("no such tab")?;
        if !self.may_answer(id, page) {
            return Err(format!("tab {} is not shown in this page", tab.number));
        }
        if text.len() > agent::PLAN_FEEDBACK_MAX_BYTES {
            return Err(format!(
                "the feedback is longer than {} KiB",
                agent::PLAN_FEEDBACK_MAX_BYTES / 1024
            ));
        }
        let waiting = tab
            .live()
            .ok_or("this tab has no live session")?
            .projection()
            .waiting_plan()
            .is_some_and(|plan| plan.interaction_id == interaction_id);
        if !waiting {
            return Err("that plan is no longer waiting".into());
        }
        let drafts = &mut self.get_mut(id).expect("checked above").session.plan_feedback_draft;
        if text.is_empty() {
            drafts.remove(interaction_id);
        } else {
            drafts.insert(interaction_id.to_string(), text);
        }
        Ok(())
    }

    fn open_plan_cycle(tab: &mut Tab, reached: bool) {
        tab.plan_cycle = PlanCycle {
            open: true,
            reached,
            taken: false,
            origin: Some(tab.mode),
        };
        tab.plan_origin = Some(tab.mode);
        if reached {
            tab.plan_on_start = false;
        }
    }

    /// Reads the reply to the switch sent for a start in plan mode. A refusal keeps the first message
    /// waiting and says why: the person tries again or lets it go as it is (`set_planning`).
    pub(super) fn poll_plan_start(tab: &mut Tab) {
        let Some(switch) = tab.plan_start.as_ref() else {
            return;
        };
        let TabBackend::Live(backend) = &mut tab.backend else {
            tab.plan_start = None;
            return;
        };
        match backend.finish_planning_switch(switch) {
            None => {}
            Some(Ok(())) => tab.plan_start = None,
            Some(Err(error)) => {
                tab.plan_start = None;
                tab.session.queue_note = Some(format!(
                    "plan mode did not start ({}); <leader>p tries again, or leave plan mode to send the message as \
                     it is",
                    error.message
                ));
            }
        }
    }

    /// For a tab asked to start in plan mode, before its first message goes: sends the switch and says the
    /// message must wait (`true`). `false` when the tab was not asked, or its session does not offer plan
    /// mode (the request is then dropped and the message goes as it is).
    pub(super) fn start_in_plan(&mut self, id: TabId, now_ms: u64) -> bool {
        let Some(tab) = self.get_mut(id) else {
            return false;
        };
        if !tab.plan_on_start {
            return false;
        }
        let TabBackend::Live(backend) = &mut tab.backend else {
            return false;
        };
        let (offered, applies, phase) = {
            let projection = backend.projection();
            (
                projection.planning.offered && backend.capabilities().plan_mode_v1,
                projection.plan_gate_applies(),
                projection.planning.phase.clone(),
            )
        };
        if applies {
            tab.plan_on_start = false;
            return false;
        }
        if !offered {
            tab.plan_on_start = false;
            tab.session.queue_note = Some("plan mode is not offered on this session".to_string());
            return false;
        }
        if tab.plan_start.is_some() || phase != PlanPhase::Executing {
            return true;
        }
        match backend.set_planning(true, now_ms) {
            Ok(switch) => tab.plan_start = Some(switch),
            Err(error) => {
                tab.session.queue_note = Some(format!(
                    "plan mode did not start ({}); <leader>p tries again, or leave plan mode to send the message as \
                     it is",
                    error.message
                ));
            }
        }
        true
    }
}
