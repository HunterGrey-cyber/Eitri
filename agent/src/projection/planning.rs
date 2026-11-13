//! Plan mode on a Claude session: the plans the model proposed (`ExitPlanMode`, carried by the sidecar as a
//! `PLAN_APPROVAL` interaction) and where the session stands between executing and planning.
//!
//! **The CLI owns every mode move; this side only expects them.** A move starts here (the model's own
//! `EnterPlanMode` call, an approval, or the host's own switch, folded as `PlanTransitionStarted`) and is
//! confirmed only by the CLI's own report of its mode (`CliPlanMode`, `CliPermissionMode`), judged against
//! the one move expected. A report no move explains closes the session through the same tripwire as an
//! ungated mode (`ungated_cli_mode`), and a move the CLI does not confirm within
//! [`PLAN_TRANSITION_GUARD_MS`] is overdue ([`AgentSessionProjection::plan_transition_overdue`]).
//!
//! **A session without plan mode offered** (`PlanningState::offered` false) never leaves `Executing`, and
//! every CLI report on it is folded exactly as before: the phase rules below do not run.
//!
//! A `plan` report never changes `cli_mode_reported`: that stays the creation mode's latest report, so
//! `cli_runs_auto()` still says whether the session runs the CLI's own auto mode when it executes. Whether
//! the CLI runs plan mode now is [`AgentSessionProjection::plan_gate_applies`].
//!
//! Nothing here answers a plan. A record settles only from the provider's own `PlanSettled`, as a question
//! does.

use super::{AgentDomainEvent, AgentSessionProjection, UngatedCliModeRecord};
use crate::process::{CliExpectation, RequestedCliMode};
use crate::provider::InteractionResolution;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// How long a mode move may wait for the CLI's report of it before the session is failed.
pub const PLAN_TRANSITION_GUARD_MS: u64 = 15_000;

/// The longest plan the sidecar carries, in UTF-8 bytes. A longer one is refused whole, never cut.
pub const PLAN_MAX_BYTES: usize = 256 * 1024;
/// How many prompts a plan may say it will need.
pub const PLAN_ALLOWED_PROMPTS_MAX: usize = 32;
/// The longest `tool` or `prompt` of one of them, in UTF-16 code units.
pub const PLAN_ALLOWED_PROMPT_UNITS_MAX: usize = 1000;
/// The longest feedback a person may send with "keep planning", in UTF-8 bytes.
pub const PLAN_FEEDBACK_MAX_BYTES: usize = 16 * 1024;

/// What the model says it will need to ask for once the plan runs. Information for the person only:
/// approving the plan grants none of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanAllowedPrompt {
    pub tool: String,
    pub prompt: String,
}

/// How a plan ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanOutcome {
    /// The person approved it, and the model goes on to carry it out.
    Approved,
    /// The person kept planning, with or without feedback; the model stays in plan mode.
    KeptPlanning,
    Interrupted,
    /// The CLI withdrew the call.
    Expired,
    /// The session ended while it waited; also what a plan restored from history while it waited reads
    /// as, since nothing can answer it any more.
    SessionClosed,
    ProviderFailed,
    /// The plan could not be carried (too long, malformed), and the model was told so.
    HostRefused,
}

/// The execution mode a person approved a plan into, as the host names it. The CLI's own mode after an
/// approval is what it reports; this is the host's choice, which every window learns from the settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanExecutionMode {
    Manual,
    Auto,
    Bypass,
}

/// Where a plan stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanState {
    Waiting,
    /// A decision is on its way to Claude; back to `Waiting` if it is refused.
    Answering,
    Settled {
        outcome: PlanOutcome,
        /// `Approved` only: the mode it was approved into, when the settlement named one.
        #[serde(default)]
        mode_after: Option<PlanExecutionMode>,
        /// `KeptPlanning` only: the person's feedback exactly as sent, possibly empty.
        #[serde(default)]
        feedback: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
}

/// One plan the model proposed, as an item of the conversation. Kept apart from `pending_permissions`
/// and from `questions`: nothing that answers a card or a question answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRecord {
    /// Ordering key, shared with the other collections. See `AgentSessionProjection::apply`.
    pub seq: u64,
    pub interaction_id: String,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// The turn that was running when it was proposed, if one was.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Markdown, exactly as the model wrote it; empty when the plan could not be carried.
    #[serde(default)]
    pub plan: String,
    /// Lower-case hex SHA-256 of `plan`'s UTF-8 bytes: an approval names it back.
    #[serde(default)]
    pub plan_sha256: String,
    #[serde(default)]
    pub allowed_prompts: Vec<PlanAllowedPrompt>,
    pub state: PlanState,
    /// Set when the plan could not be carried; it is then settled `HostRefused`.
    #[serde(default)]
    pub unsupported_reason: Option<String>,
}

/// What started a mode move: the model's own tool call (`EnterPlanMode`, or the `ExitPlanMode` whose plan
/// was approved), or the host's own switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanCause {
    Tool { tool_use_id: String },
    Host,
}

/// Where a session stands between executing and planning. `since_ms` is when the move started, in
/// milliseconds since the epoch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum PlanPhase {
    #[default]
    Executing,
    /// A move into plan mode started, and the CLI has not reported `plan` yet: it still runs its earlier
    /// mode.
    Entering {
        cause: PlanCause,
        since_ms: u64,
    },
    Planning,
    /// A move out of plan mode started, and the CLI has not reported its creation mode yet.
    /// `saw_default` is set when an auto session's CLI reported `default` on the way: a step the CLI
    /// may still correct, or its fallback out of auto.
    Leaving {
        cause: PlanCause,
        since_ms: u64,
        saw_default: bool,
    },
}

/// Where a session stood when one of its calls began, for a call that began anywhere but executing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallPlanMark {
    /// The CLI's own last report said `plan` (`plan_gate_applies`): the call is one the CLI made in plan mode.
    Planning,
    /// A move into plan mode was under way (`Entering`): the CLI may already have run plan mode for the call
    /// before its report of it. A move out is not one: until the CLI reports its way out it still runs plan mode
    /// (`Planning`), and afterwards it does not.
    Moving,
}

/// A session's plan mode: whether it was offered, the phase, and what the CLI last said about plan mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanningState {
    pub phase: PlanPhase,
    /// Whether the session was created with plan mode offered. Nothing below changes a session without
    /// it.
    pub offered: bool,
    /// Whether the CLI's latest report on the session says `plan`.
    #[serde(default)]
    pub cli_reports_plan: bool,
    /// The mode the session was created in, which a move out of plan mode returns to.
    #[serde(default)]
    pub creation: RequestedCliMode,
    /// The host's own move under way has a request whose reply nobody has read yet. Host state: the guard
    /// does not count such a move overdue, since the sidecar's own deadline answers only after as long as
    /// the guard waits, and a CLI that moved late still reports the move.
    #[serde(default)]
    pub host_reply_pending: bool,
    /// A request for the host's move under way timed out or was lost: the CLI may still make that move, so
    /// a later refusal of the same move (a resend) does not undo it.
    #[serde(default)]
    pub host_move_uncertain: bool,
    /// The calls that began while the CLI ran plan mode or the session was entering it (`plan_mark_now`), by
    /// tool-use id, subagents' calls included; any other call is not listed. The sidecar's message events keep the CLI's
    /// own order, so where the phase stood when a call's `ToolCallStarted` is folded is where the CLI stood when
    /// it made the call, which a permission request that arrived ahead of the call cannot tell. Host state, not
    /// saved: what reads it (the gate and the sweeps) reads it while the session lives. A chat move carries it beside
    /// the records (`ChatMoveCore::call_marks`): the window it lands in rebuilds marks only for the calls whose start
    /// it folds again, so it keeps the carried mark of every call its folds left unmarked.
    /// A call's mark is dropped once the call has completed with no request pending for it: nothing reads it then.
    #[serde(skip)]
    pub call_marks: BTreeMap<String, CallPlanMark>,
}

impl PlanningState {
    /// The CLI's word for the creation mode.
    fn creation_word(&self) -> &'static str {
        if self.auto_session() {
            "auto"
        } else {
            "default"
        }
    }

    fn auto_session(&self) -> bool {
        !matches!(self.creation, RequestedCliMode::Default)
    }
}

/// Milliseconds since the epoch; 0 on a clock before 1970.
pub(crate) fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl PlanRecord {
    /// Whether `resolution` may be sent for this plan now, and if not, why, in words the person is shown.
    /// Only a plan still waiting, that could be carried, takes one, and never a question's answer.
    pub fn check_resolution(&self, resolution: &InteractionResolution) -> Result<(), String> {
        match self.state {
            PlanState::Waiting => {}
            PlanState::Answering => return Err("a decision on this plan is already on its way".to_string()),
            PlanState::Settled { .. } => return Err("this plan is no longer waiting for a decision".to_string()),
        }
        if self.unsupported_reason.is_some() || self.plan.is_empty() {
            return Err("this plan could not be shown, so it takes no decision".to_string());
        }
        match resolution {
            InteractionResolution::Answers(_) | InteractionResolution::Decline { .. } => {
                Err("a plan is approved or kept planning, never answered as a question".to_string())
            }
            // The digest the person read: a plan that changed since is not the one approved.
            InteractionResolution::PlanApprove { plan_sha256, .. } if *plan_sha256 != self.plan_sha256 => {
                Err("this is not the plan that is waiting; read it again before approving".to_string())
            }
            InteractionResolution::PlanApprove { .. } => Ok(()),
            InteractionResolution::PlanKeep { feedback } if feedback.len() > PLAN_FEEDBACK_MAX_BYTES => Err(format!(
                "the feedback is longer than {} KiB; shorten it",
                PLAN_FEEDBACK_MAX_BYTES / 1024
            )),
            InteractionResolution::PlanKeep { .. } => Ok(()),
        }
    }
}

/// Whether `error`, the reply to a move this side asked for, says the move did not happen: the agent
/// refused it before making it. A timeout, a lost connection or the agent's own deadline say nothing
/// about the CLI, which may still move; only its report or the guard decides those. An approval
/// refused as already settled is not undone either: another window may have approved it.
pub fn refusal_undoes_move(error: &crate::ProviderError) -> bool {
    use crate::{ProviderError, ProviderErrorCode};
    match error {
        ProviderError::UnsupportedCapability(_) => true,
        ProviderError::Provider { code, .. } => matches!(
            code,
            ProviderErrorCode::PlanningRefused
                | ProviderErrorCode::TurnAlreadyActive
                | ProviderErrorCode::InvalidConfiguration
                | ProviderErrorCode::IdempotencyConflict
                | ProviderErrorCode::InteractionAnswerRefused
        ),
        ProviderError::Timeout | ProviderError::Transport(_) => false,
    }
}

impl AgentSessionProjection {
    /// Declares how this session's CLI reports are judged: the mode it was created in and whether plan
    /// mode was offered. Set when the session is created or resumed, before its first event; a session
    /// never declared has no plan mode. A CLI started for the session starts in its creation mode, so a
    /// phase restored from an earlier process does not carry over.
    pub fn offer_plan_mode(&mut self, expect: CliExpectation) {
        self.planning.offered = expect.plan_offered;
        self.planning.creation = expect.requested;
        self.planning.phase = PlanPhase::Executing;
        self.planning.cli_reports_plan = false;
        self.planning.host_reply_pending = false;
        self.planning.host_move_uncertain = false;
    }

    /// The reply to a request for the host's own move (`enter`) was read, `error` when it says the move was
    /// not taken, at `now_ms`. The guard's clock for that move starts again here, so a reply that came late
    /// leaves the CLI the whole guard to report the move. Returns whether the move is to be undone: only
    /// for a refusal that says the agent did not make it (`refusal_undoes_move`), and never once an
    /// earlier request for the same move timed out or was lost, since that one may still be made.
    pub fn planning_reply_read(&mut self, enter: bool, error: Option<&crate::ProviderError>, now_ms: u64) -> bool {
        self.planning.host_reply_pending = false;
        match &mut self.planning.phase {
            PlanPhase::Entering {
                cause: PlanCause::Host,
                since_ms,
            } if enter => *since_ms = now_ms,
            PlanPhase::Leaving {
                cause: PlanCause::Host,
                since_ms,
                ..
            } if !enter => *since_ms = now_ms,
            _ => {}
        }
        let Some(error) = error else {
            return false;
        };
        if !refusal_undoes_move(error) {
            self.planning.host_move_uncertain = true;
            return false;
        }
        !self.planning.host_move_uncertain
    }

    /// Another request for the host's move under way went out: its reply is unread again.
    pub fn planning_resent(&mut self) {
        self.planning.host_reply_pending = true;
    }

    /// The plan `interaction_id` names, settled or not.
    pub fn plan(&self, interaction_id: &str) -> Option<&PlanRecord> {
        self.plans.iter().find(|record| record.interaction_id == interaction_id)
    }

    /// The plan still waiting for a decision, a send awaiting acknowledgement included. One waits at a
    /// time; were there two, the newest.
    pub fn waiting_plan(&self) -> Option<&PlanRecord> {
        self.plans
            .iter()
            .rev()
            .find(|record| !matches!(record.state, PlanState::Settled { .. }))
    }

    /// Marks a local send without claiming that the provider accepted the decision.
    pub fn mark_plan_answering(&mut self, interaction_id: &str) -> bool {
        self.change_plan_send_state(interaction_id, PlanState::Waiting, PlanState::Answering)
    }

    /// A refused or failed send leaves the plan available for another decision.
    pub fn plan_send_failed(&mut self, interaction_id: &str) -> bool {
        self.change_plan_send_state(interaction_id, PlanState::Answering, PlanState::Waiting)
    }

    fn change_plan_send_state(&mut self, id: &str, from: PlanState, to: PlanState) -> bool {
        let Some(record) = self
            .plans
            .iter_mut()
            .find(|record| record.interaction_id == id && record.state == from)
        else {
            return false;
        };
        record.state = to;
        true
    }

    /// Whether the session is anywhere but executing: planning, or a move in either direction. What the
    /// mode badge, the mode key, the sweeps and a move read.
    pub fn is_planning(&self) -> bool {
        self.planning.phase != PlanPhase::Executing
    }

    /// Where the session stands now, as a call beginning now would be marked: `None` while the CLI runs no plan
    /// mode and is not entering it.
    pub fn plan_mark_now(&self) -> Option<CallPlanMark> {
        if self.plan_gate_applies() {
            Some(CallPlanMark::Planning)
        } else if matches!(self.planning.phase, PlanPhase::Entering { .. }) {
            Some(CallPlanMark::Moving)
        } else {
            None
        }
    }

    /// Whether the call `tool_use_id` has been folded: a call of the main thread's or of a subagent's.
    pub fn call_seen(&self, tool_use_id: &str) -> bool {
        super::subagents::locate(&self.tool_calls, tool_use_id, 0).is_some()
    }

    /// Where the session stood when the call `tool_use_id` began, when that was anywhere but executing.
    pub fn call_mark(&self, tool_use_id: &str) -> Option<CallPlanMark> {
        self.planning.call_marks.get(tool_use_id).copied()
    }

    /// Whether the pending request `permission_id` can be placed against plan mode: where the CLI stood when it
    /// asked is known. On a session that offered plan mode that needs the request's call folded: its mark says
    /// where. Nothing else places a request. The sidecar sends requests ahead of the message events before them, so
    /// no event folded after a request -- another call's start, a subagent's, a turn's end -- shows that the events
    /// sent before the request have all been folded: it may itself have been sent before the request, ahead of an
    /// `EnterPlanMode` and the CLI's `plan` report still on their way (and after a turn's end the CLI may start a
    /// turn of its own and enter plan mode in it). A request that names no call is therefore never placed. A session
    /// without the offer, and a request no longer pending, are always placed.
    pub fn request_placed(&self, permission_id: &str) -> bool {
        if !self.planning.offered {
            return true;
        }
        let Some(request) = self.pending_permissions.get(permission_id) else {
            return true;
        };
        request
            .tool_use_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .is_some_and(|call| self.call_seen(call))
    }

    /// Whether the call `tool_use_id` began while the CLI ran plan mode or the session was entering it: a request
    /// for it was raised under plan mode's restrictions, or may have been.
    pub fn began_outside_executing(&self, tool_use_id: Option<&str>) -> bool {
        tool_use_id.is_some_and(|id| self.planning.call_marks.contains_key(id))
    }

    /// Whether the CLI runs plan mode now, by its own last report: `Planning`, or `Leaving` before the CLI
    /// has reported the move. Not while `Entering`: the CLI still runs its earlier mode then, and that
    /// mode's own verdicts apply.
    pub fn plan_gate_applies(&self) -> bool {
        self.planning.offered
            && match self.planning.phase {
                PlanPhase::Planning => true,
                PlanPhase::Leaving { .. } => self.planning.cli_reports_plan,
                PlanPhase::Executing | PlanPhase::Entering { .. } => false,
            }
    }

    /// Why the move under way is overdue at `now_ms`, if it is: started longer than
    /// [`PLAN_TRANSITION_GUARD_MS`] ago and not confirmed by the CLI. The one exception ends here: an auto
    /// session whose CLI reported `default` on its way out of plan mode and has stayed there is the CLI's
    /// own fallback out of auto, positively reported; the session is executing then, out of auto
    /// (`cli_left_auto`), and nothing is overdue.
    pub fn plan_transition_overdue(&mut self, now_ms: u64) -> Option<String> {
        let (since_ms, leaving, saw_default) = match &self.planning.phase {
            PlanPhase::Executing | PlanPhase::Planning => return None,
            // The model's entry call still waits for a person's answer on its card: the CLI cannot move
            // before that, and a person may take as long as they like.
            PlanPhase::Entering {
                cause: PlanCause::Tool { tool_use_id },
                ..
            } if self.entry_waits_for_a_person(tool_use_id) => return None,
            // The host's own move while the reply to its request is unread: the sidecar answers within its own
            // deadline, and the clock starts again once the reply is read.
            PlanPhase::Entering {
                cause: PlanCause::Host, ..
            }
            | PlanPhase::Leaving {
                cause: PlanCause::Host, ..
            } if self.planning.host_reply_pending => return None,
            PlanPhase::Entering { since_ms, .. } => (*since_ms, false, false),
            PlanPhase::Leaving {
                since_ms, saw_default, ..
            } => (*since_ms, true, *saw_default),
        };
        if now_ms.saturating_sub(since_ms) <= PLAN_TRANSITION_GUARD_MS {
            return None;
        }
        if leaving && saw_default && self.planning.auto_session() {
            self.planning.phase = PlanPhase::Executing;
            self.planning.cli_reports_plan = false;
            self.cli_left_auto = true;
            return None;
        }
        let seconds = PLAN_TRANSITION_GUARD_MS / 1000;
        Some(if leaving {
            format!("Claude Code did not leave plan mode within {seconds} s")
        } else {
            format!("Claude Code did not enter plan mode within {seconds} s")
        })
    }

    /// Whether a permission request for the call `tool_use_id` is still waiting for its answer.
    fn entry_waits_for_a_person(&self, tool_use_id: &str) -> bool {
        self.pending_permissions
            .values()
            .any(|request| request.tool_use_id.as_deref() == Some(tool_use_id))
    }

    /// The `seq` and the mode of the newest plan settled `Approved`, whichever window sent the approval.
    pub fn latest_approval(&self) -> Option<(u64, Option<PlanExecutionMode>)> {
        self.plans.iter().rev().find_map(|record| match &record.state {
            PlanState::Settled {
                outcome: PlanOutcome::Approved,
                mode_after,
                ..
            } => Some((record.seq, *mode_after)),
            _ => None,
        })
    }
}

/// The CLI said something its expected move does not explain: the session closes, as for an ungated mode.
/// First report wins, as the tripwire's does.
fn trip(state: &mut AgentSessionProjection, reported: &str, detail: &str) {
    if state.ungated_cli_mode.is_none() {
        state.ungated_cli_mode = Some(UngatedCliModeRecord {
            reported: reported.to_string(),
            detail: detail.to_string(),
        });
    }
}

/// A move into plan mode that ended without the CLI entering it: the session executes in the mode it was
/// in. A report of that mode that arrived meanwhile was set aside as a stale one; it is read now, so an
/// auto session whose CLI reported `default` follows out of auto.
fn entry_ended(state: &mut AgentSessionProjection) {
    state.planning.phase = PlanPhase::Executing;
    if state.planning.auto_session() {
        match state.cli_mode_reported.as_deref() {
            Some("auto") => state.cli_left_auto = false,
            Some("default") => state.cli_left_auto = true,
            _ => {}
        }
    }
}

/// Whether the model's entry call `tool_use_id` has its result.
fn call_finished(state: &AgentSessionProjection, tool_use_id: &str) -> bool {
    state
        .tool_calls
        .iter()
        .any(|call| call.tool_use_id == tool_use_id && call.result.is_some())
}

/// Folds `event` into the plans and the plan phase. Runs before the event's own arm in `apply`, so a
/// record it creates takes the `seq` that arm would; `now` dates a move this event starts.
pub(super) fn fold(state: &mut AgentSessionProjection, event: &AgentDomainEvent, now: &dyn Fn() -> u64) {
    let seq = state.last_revision;
    match event {
        AgentDomainEvent::PlanProposed {
            interaction_id,
            tool_use_id,
            plan,
            plan_sha256,
            allowed_prompts,
            unsupported_reason,
        } => {
            state.assistant_message_open = false;
            state.thinking_open = false;
            // A replayed request replaces its earlier copy and takes its new arrival position.
            state.plans.retain(|record| record.interaction_id != *interaction_id);
            state.plans.push(PlanRecord {
                seq,
                interaction_id: interaction_id.clone(),
                tool_use_id: tool_use_id.clone(),
                turn_id: state.active_turn_id.clone(),
                plan: plan.clone(),
                plan_sha256: plan_sha256.clone(),
                allowed_prompts: allowed_prompts.clone(),
                state: PlanState::Waiting,
                unsupported_reason: unsupported_reason.clone(),
            });
        }
        AgentDomainEvent::PlanSettled {
            interaction_id,
            outcome,
            mode_after,
            feedback,
            reason,
        } => {
            let Some(record) = state.plans.iter_mut().find(|record| {
                record.interaction_id == *interaction_id && !matches!(record.state, PlanState::Settled { .. })
            }) else {
                return;
            };
            let approved = *outcome == PlanOutcome::Approved;
            let record_call = record.tool_use_id.clone().unwrap_or_default();
            record.state = PlanState::Settled {
                outcome: *outcome,
                mode_after: if approved { *mode_after } else { None },
                feedback: if *outcome == PlanOutcome::KeptPlanning {
                    feedback.clone()
                } else {
                    None
                },
                reason: reason.clone(),
            };
            // An approval another window sent: the CLI is about to leave plan mode.
            if approved && state.planning.offered && state.planning.phase == PlanPhase::Planning {
                let tool_use_id = record.tool_use_id.clone().unwrap_or_default();
                state.planning.phase = PlanPhase::Leaving {
                    cause: PlanCause::Tool { tool_use_id },
                    since_ms: now(),
                    saw_default: false,
                };
            }
            // The plan ended some other way before the approval sent from here took (an interrupt, an
            // expiry, a decision from another window): the CLI never left plan mode, and the move out that
            // approval started is over. Once the CLI has reported its way out it has left, and the report
            // decides.
            if !approved && state.planning.cli_reports_plan {
                if let PlanPhase::Leaving {
                    cause: PlanCause::Tool { tool_use_id },
                    ..
                } = &state.planning.phase
                {
                    if *tool_use_id == record_call {
                        state.planning.phase = PlanPhase::Planning;
                    }
                }
            }
        }
        AgentDomainEvent::ToolCallStarted {
            turn_id,
            tool_use_id,
            name,
            ..
        } => {
            mark_call(state, tool_use_id);
            on_call_started(state, tool_use_id, turn_id, seq);
            if name == "EnterPlanMode" && state.planning.offered && state.planning.phase == PlanPhase::Executing {
                state.planning.phase = PlanPhase::Entering {
                    cause: PlanCause::Tool {
                        tool_use_id: tool_use_id.clone(),
                    },
                    since_ms: now(),
                };
            }
        }
        AgentDomainEvent::ToolCallCompleted {
            tool_use_id, is_error, ..
        } => {
            unmark_finished_call(state, tool_use_id);
            // The model's entry failed: the CLI never left the mode it was in.
            if *is_error
                && matches!(&state.planning.phase,
                    PlanPhase::Entering { cause: PlanCause::Tool { tool_use_id: entering }, .. } if entering == tool_use_id)
            {
                entry_ended(state);
            }
        }
        // A turn that ended (interrupted, failed) before the model's entry call had its result: the CLI
        // never ran the call, so it never entered plan mode.
        AgentDomainEvent::TurnCompleted { .. } => {
            let entering = match &state.planning.phase {
                PlanPhase::Entering {
                    cause: PlanCause::Tool { tool_use_id },
                    ..
                } => Some(tool_use_id.clone()),
                _ => None,
            };
            if entering.is_some_and(|id| !call_finished(state, &id)) {
                entry_ended(state);
            }
        }
        // The entry call's card was answered: the CLI may move from now on, so the guard's clock starts
        // here rather than when the call began.
        AgentDomainEvent::PermissionResolved { permission_id, .. } => {
            let answered = state
                .pending_permissions
                .get(permission_id)
                .and_then(|request| request.tool_use_id.clone());
            if let PlanPhase::Entering {
                cause: PlanCause::Tool { tool_use_id },
                since_ms,
            } = &mut state.planning.phase
            {
                if answered.as_deref() == Some(tool_use_id.as_str()) {
                    *since_ms = now();
                }
            }
        }
        AgentDomainEvent::PlanTransitionStarted { enter, cause, at_ms } => {
            if !state.planning.offered {
                return;
            }
            let started = match (enter, &state.planning.phase) {
                (true, PlanPhase::Executing) => PlanPhase::Entering {
                    cause: cause.clone(),
                    since_ms: *at_ms,
                },
                (false, PlanPhase::Planning) => PlanPhase::Leaving {
                    cause: cause.clone(),
                    since_ms: *at_ms,
                    saw_default: false,
                },
                _ => return,
            };
            state.planning.phase = started;
            // A move of the host's own goes with a request whose reply is unread until someone reads it.
            state.planning.host_reply_pending = *cause == PlanCause::Host;
            state.planning.host_move_uncertain = false;
        }
        // A move this side started that the agent refused: the CLI never left where it was.
        AgentDomainEvent::PlanTransitionUndone { enter, cause } => match &state.planning.phase {
            PlanPhase::Entering { cause: started, .. } if *enter && started == cause => entry_ended(state),
            PlanPhase::Leaving { cause: started, .. }
                if !*enter && started == cause && state.planning.cli_reports_plan =>
            {
                state.planning.phase = PlanPhase::Planning;
            }
            _ => {}
        },
        AgentDomainEvent::CliPlanMode { .. } => {
            if !state.planning.offered {
                trip(state, "plan", "plan mode the host did not expect");
                return;
            }
            match state.planning.phase {
                PlanPhase::Entering { .. } => {
                    state.planning.phase = PlanPhase::Planning;
                    state.planning.cli_reports_plan = true;
                }
                // Each turn's `SessionReady` while planning; and while leaving, the CLI has not moved yet,
                // which the guard decides. Its last report is `plan` again, even after a `default` on the
                // way out: that step was not its fallback.
                PlanPhase::Planning => state.planning.cli_reports_plan = true,
                PlanPhase::Leaving { .. } => {
                    if let PlanPhase::Leaving { saw_default, .. } = &mut state.planning.phase {
                        *saw_default = false;
                    }
                    state.planning.cli_reports_plan = true;
                }
                PlanPhase::Executing => trip(state, "plan", "plan mode the host did not expect"),
            }
        }
        AgentDomainEvent::CliPermissionMode { reported } if state.planning.offered => {
            on_cli_mode(state, reported);
        }
        // A subagent's call carries the mode its session stood in as much as the main thread's does.
        AgentDomainEvent::SubagentFrame { frame, .. } => {
            let mut leaf = frame.as_ref();
            while let AgentDomainEvent::SubagentFrame { frame, .. } = leaf {
                leaf = frame;
            }
            match leaf {
                AgentDomainEvent::ToolCallStarted { tool_use_id, .. } => mark_call(state, tool_use_id),
                AgentDomainEvent::ToolCallCompleted { tool_use_id, .. } => unmark_finished_call(state, tool_use_id),
                _ => {}
            }
        }
        _ => {}
    }
}

/// Notes where the session stands as the call `tool_use_id` begins, when that is anywhere but executing. Read
/// before the call's own effect on the phase: an `EnterPlanMode` call begins while the session still executes.
fn mark_call(state: &mut AgentSessionProjection, tool_use_id: &str) {
    if tool_use_id.is_empty() || !state.planning.offered {
        return;
    }
    let Some(mark) = state.plan_mark_now() else {
        return;
    };
    state.planning.call_marks.entry(tool_use_id.to_string()).or_insert(mark);
}

/// Drops the mark of the call `tool_use_id`, which has completed, unless a request for it is still pending (a sweep
/// still reads the mark for that one).
fn unmark_finished_call(state: &mut AgentSessionProjection, tool_use_id: &str) {
    if state
        .pending_permissions
        .values()
        .any(|p| p.tool_use_id.as_deref() == Some(tool_use_id))
    {
        return;
    }
    state.planning.call_marks.remove(tool_use_id);
}

/// A mode the session accepts, reported on a plan-offered session. Runs before the report replaces
/// `cli_mode_reported`, and in place of the fold that follows a session out of auto on a session without
/// the offer.
fn on_cli_mode(state: &mut AgentSessionProjection, reported: &str) {
    let creation = state.planning.creation_word();
    let auto_session = state.planning.auto_session();
    match state.planning.phase {
        PlanPhase::Leaving { .. } => {
            if reported == creation {
                state.planning.phase = PlanPhase::Executing;
                state.planning.cli_reports_plan = false;
            } else if auto_session && reported == "default" {
                // A step the CLI may still correct to `auto`; staying past the guard is its fallback.
                if let PlanPhase::Leaving { saw_default, .. } = &mut state.planning.phase {
                    *saw_default = true;
                }
                state.planning.cli_reports_plan = false;
            }
        }
        PlanPhase::Planning => trip(state, reported, "left plan mode without an approval"),
        // Reported before the CLI took the move in.
        PlanPhase::Entering { .. } => {}
        PlanPhase::Executing => {
            if reported == "auto" {
                state.cli_left_auto = false;
            } else if auto_session && reported == "default" {
                state.cli_left_auto = true;
            }
        }
    }
}

/// A call to `tool_use_id` began in turn `turn_id`. A plan the CLI proposed before the frames announcing
/// its turn arrived was folded under the turn before; the call names the turn it belongs to, and the plan
/// takes it and its place in the record, which stays in `seq` order.
fn on_call_started(state: &mut AgentSessionProjection, tool_use_id: &str, turn_id: &str, seq: u64) {
    if tool_use_id.is_empty() {
        return;
    }
    let mut moved = false;
    for record in &mut state.plans {
        if record.tool_use_id.as_deref() == Some(tool_use_id) && record.turn_id.as_deref() != Some(turn_id) {
            record.turn_id = Some(turn_id.to_string());
            record.seq = seq;
            moved = true;
        }
    }
    if moved {
        state.plans.sort_by_key(|record| record.seq);
    }
}

/// The session ended: every plan still waiting ends with it, and so does any move under way.
pub(super) fn on_session_end(state: &mut AgentSessionProjection) {
    #[cfg(test)]
    super::count_hook("planning::on_session_end");
    for record in &mut state.plans {
        if !matches!(record.state, PlanState::Settled { .. }) {
            record.state = PlanState::Settled {
                outcome: PlanOutcome::SessionClosed,
                mode_after: None,
                feedback: None,
                reason: None,
            };
        }
    }
    state.planning.phase = PlanPhase::Executing;
    state.planning.cli_reports_plan = false;
    state.planning.host_reply_pending = false;
    state.planning.host_move_uncertain = false;
}

/// The plans, when there are any, and the phase, on a session that offered plan mode.
pub(super) fn snapshot_json(state: &AgentSessionProjection, snapshot: &mut Value) {
    #[cfg(test)]
    super::count_hook("planning::snapshot_json");
    if !state.plans.is_empty() {
        snapshot["plans"] = state.plans.iter().map(record_json).collect();
    }
    if state.planning.offered {
        snapshot["planning"] = json!({ "phase": phase_word(&state.planning.phase) });
    }
}

/// The phase as the page names it.
pub fn phase_word(phase: &PlanPhase) -> &'static str {
    match phase {
        PlanPhase::Executing => "executing",
        PlanPhase::Entering { .. } => "entering",
        PlanPhase::Planning => "planning",
        PlanPhase::Leaving { .. } => "leaving",
    }
}

fn record_json(record: &PlanRecord) -> Value {
    let state = match &record.state {
        PlanState::Waiting => json!({ "kind": "waiting" }),
        PlanState::Answering => json!({ "kind": "answering" }),
        PlanState::Settled {
            outcome,
            mode_after,
            feedback,
            reason,
        } => json!({
            "kind": "settled", "outcome": outcome, "modeAfter": mode_after, "feedback": feedback, "reason": reason,
        }),
    };
    json!({
        "seq": record.seq, "interactionId": record.interaction_id, "toolUseId": record.tool_use_id,
        "turnId": record.turn_id, "plan": record.plan, "planSha256": record.plan_sha256,
        "allowedPrompts": record.allowed_prompts.iter().map(|prompt| json!({
            "tool": prompt.tool, "prompt": prompt.prompt,
        })).collect::<Vec<_>>(),
        "state": state, "unsupportedReason": record.unsupported_reason,
    })
}

#[cfg(test)]
#[path = "../testkit/plans_fixture.rs"]
mod fixture;

#[cfg(test)]
mod tests;
