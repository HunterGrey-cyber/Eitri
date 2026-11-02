//! One window's session tabs: one conversation, and so one `AgentBackend`,
//! per tab, every one of them running. GTK-free: `eitri_panel::agent_panel` spawns the connect and
//! handoff workers and hands this set their receivers; the panel's tick calls [`TabSet::pump_views`], which
//! drains EVERY tab -- `take_revised_ui_delivery` is where the permission policy answers what needs no
//! human, so a tab that was not pumped would stall on its first `Read`.
//!
//! **Lock order, inherited:** on the sidecar path `projection()` holds the ingestion mutex and
//! `provider_session_id()` takes it again. Never call the second while holding the first's guard
//! (the 2026-09-15 GTK freeze). Every method below reads them in separate statements.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Weak};

use agent::{AgentDomainEvent, PromptOrigin, RevisedDelivery};

pub use crate::agent_backend::AgentId;
use crate::agent_backend::{AgentBackend, BackendError, BackendKind, QuestionSend};
use crate::agent_bridge::{
    serialize_events_with_notes_and_revisions_for_js, serialize_handoff_for_js, serialize_snapshot_with_notes_for_js,
    serialize_tabs_for_js, CallNotes, SessionModeChoice, SnapshotView, TabRef, TabStateWire, TabView, TurnTime,
};
use crate::attention::{Attention, AttentionTracker};
use crate::saved_tabs::{SavedTab, SavedTabs};
use crate::tab_restore::{PlannedTab, RestorePlan, RestoreRun, Skipped};
pub use crate::tabs::ViewId;
use crate::tabs::{self, TabFacts, TabId};

mod moving;
mod shell;
pub use shell::{shell_command, ShellEnded};
pub use moving::{FrozenChat, FrozenTab, MoveRefusedWhy, SendGate, Sent, TabHandoff};

#[cfg(test)]
mod move_tests;

/// The work held by one chat view, including its background tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewActivity {
    pub running: u32,
    pub queued: u32,
}

/// The first turn of a session started by it, composed when Enter was pressed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FirstTurn {
    /// With the editor context composed in: what goes to the model.
    pub wire: String,
    /// As typed: what the panel shows and what titles the session.
    pub typed: String,
    /// Only a message entered at the composer carries the person's stamp.
    #[serde(default)]
    pub typed_by_person: bool,
}

impl FirstTurn {
    fn origin(&self) -> PromptOrigin {
        prompt_origin(self.typed_by_person)
    }
}

fn prompt_origin(typed_by_person: bool) -> PromptOrigin {
    if typed_by_person {
        PromptOrigin::Typed
    } else {
        PromptOrigin::Composed
    }
}

/// A backend being constructed on a worker. See `eitri_panel::agent_panel`'s connect worker.
pub struct PendingStart {
    /// The command (`send_message` or `resume`) whose `command_result` is owed when it finishes.
    pub request_id: String,
    pub result_rx: mpsc::Receiver<Result<AgentBackend, BackendError>>,
    pub first_turn: Option<FirstTurn>,
    /// The Claude session a resume is continuing: a second resume of it switches here.
    pub resume: Option<String>,
    /// That session's cut first-prompt title (`ConversationRecord.title`), for the label once it
    /// is installed.
    pub resumed_title: Option<String>,
    /// That session's rename (`ConversationRecord.name`), which the label puts first.
    /// Installed as the tab's own `name`, so the detail popover and `prefix ,` see it too, unless
    /// the tab was renamed while it connected.
    pub resumed_name: Option<String>,
}

/// A session start put off until the window has asked whether the project's own Claude configuration
/// may load. Nothing is spawned and no lease is taken while it waits: it holds only what the user
/// asked for, so that the answer (or an `Escape`) can carry it out or hand it back.
pub enum DeferredStart {
    /// The first message of an empty tab. The command's `command_result` is owed to `request_id`.
    FirstSend { request_id: String, first_turn: FirstTurn },
    /// A resume into this tab.
    Resume {
        request_id: String,
        provider_session_id: String,
        resumed_title: Option<String>,
        resumed_name: Option<String>,
    },
    /// A restore of the last window's tabs, already cleared by its bypass question. It has taken no
    /// lease yet: those are taken only when it starts.
    Restore { go: RestoreGo },
}

impl DeferredStart {
    /// The command this start owes a `command_result`, when it came from one.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            DeferredStart::FirstSend { request_id, .. } | DeferredStart::Resume { request_id, .. } => Some(request_id),
            DeferredStart::Restore { .. } => None,
        }
    }
}

/// A tab holding a [`DeferredStart`] while the trust question is asked or answered.
pub struct AwaitingTrust {
    pub start: DeferredStart,
    /// The nonce of the question on screen for this tab, if one is. Routing away from the tab clears
    /// it, so an answer typed after leaving can never reach the question again.
    pub asking: Option<u64>,
    /// A `y` was accepted and the worker is checking the disk again (and writing the record, when one
    /// is kept): the answer is spent, and the start waits for that result rather than for another
    /// answer.
    pub recording: bool,
}

/// A handoff whose session close is still running on a worker thread.
///
/// The command is built BEFORE the session is taken -- it is derived from the session's own state,
/// which stops being readable the moment ownership moves to the worker -- and dispatched only after
/// `closed_rx` reports. That ordering is the whole reason this struct exists rather than a straight
/// reply: the session must be closed and flushed before the CLI starts, so the CLI resumes a
/// complete transcript nobody else is writing, and the user cannot start it before they have seen
/// the command.
///
/// Closing runs off the GTK main loop for the same reason every other teardown here does: the
/// sidecar path's `close_session` is a unary RPC bounded at 10s, and blocking the main loop on it
/// would freeze the editor pane too.
pub struct PendingHandoff {
    pub request_id: String,
    pub command: agent::handoff::ClaudeResumeCommand,
    /// Reports when `AgentBackend::shutdown()` RETURNED, which is what the command waits on.
    pub closed_rx: mpsc::Receiver<()>,
    /// Reports when the backend has also been DROPPED -- on the sidecar path that is the child's
    /// actual death (`SpawnedSidecar::drop`), which `shutdown()` does not perform. Read only by
    /// `AgentPanelHandle::shutdown`, which holds the application until it arrives; the ordinary
    /// path ignores it and it is dropped with the rest of this struct when the command is
    /// dispatched, at which point the worker's send simply fails.
    pub dropped_rx: mpsc::Receiver<()>,
}

/// One message typed while a turn ran. `wire` is composed against the editor context of the
/// moment it was queued; `text` is what was typed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Queued {
    pub text: String,
    pub wire: String,
    pub queued_at_ms: u64,
    pub state: QueuedState,
    /// Whether Enter may put it into a running turn. False for a confirmed review, which waits for the
    /// turn's end: it describes what that turn did, so it belongs after it.
    pub steerable: bool,
    /// The ids it was sent into a turn under and reported dropped from, oldest first. A provider that
    /// gave up waiting on a message reports it dropped, and may still see the agent read it later: a
    /// delivery under one of these ids means the message reached the agent, so it leaves the queue
    /// instead of being sent again. Such a message is not put into a turn again; it waits for the turn's end.
    pub dropped_as: Vec<String>,
    /// A refused send (the agent was already busy) put it here: it goes into the running turn as soon as
    /// the projection shows one, and waits for the turn's end when that is refused. Cleared once the
    /// attempt was made.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub steer_when_running: bool,
    /// Typed while a card waited with no turn running: it goes out as a turn once the card is answered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub behind_card: bool,
    /// Kept in memory from keyboard admission; a restored queue is composed until the person resends it.
    #[serde(skip)]
    pub typed_by_person: bool,
    /// The CLI refused it: sending it again would be refused again, so the queue never sends it on its
    /// own. It stays, with the reason, until the person takes it back into the composer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refused: bool,
    /// A `!` line the person entered, not yet run. Only the composer's `run_shell` sets it, and it does not
    /// survive a restart: a restored queue holds words, and words are never run.
    #[serde(skip)]
    pub shell_line: bool,
    /// What a `!` line printed, for the agent: its row is in the conversation already.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shell_report: bool,
}

impl Queued {
    /// A message waiting for the turn to end.
    pub fn held(text: &str, wire: String, queued_at_ms: u64) -> Queued {
        Queued {
            text: text.to_string(),
            wire,
            queued_at_ms,
            state: QueuedState::Held,
            steerable: true,
            dropped_as: Vec::new(),
            steer_when_running: false,
            behind_card: false,
            typed_by_person: false,
            refused: false,
            shell_line: false,
            shell_report: false,
        }
    }

    fn origin(&self) -> PromptOrigin {
        prompt_origin(self.typed_by_person)
    }

    /// Whether this item goes alone: a `!` line and its report are never read with a message around them.
    fn is_shell(&self) -> bool {
        self.shell_line || self.shell_report
    }

    pub fn is_held(&self) -> bool {
        self.state == QueuedState::Held
    }

    /// Whether this item runs as a command of its agent: never in a running turn, and a turn of its own. A
    /// review is the panel's text, whatever it starts with.
    fn is_command(&self, reads_commands: bool) -> bool {
        reads_commands && self.steerable && is_slash_command(&self.text)
    }
}

/// Where a typed message is: waiting for the turn to end, or sent into the running turn and not yet
/// read by the agent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QueuedState {
    /// Goes out with the next turn (`TabSet::flush_queue`), or into this one when the agent can take it.
    Held,
    /// Sent into the running turn under `client_id` (`AgentBackend::steer`); it leaves the queue when
    /// the agent reads it, and is held again if the agent refuses it or the turn ends first.
    Steered { client_id: String, sent_at_ms: u64 },
}

/// The prompt still shown as a transcript row until the CLI says it is waiting to be read.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct OpeningPrompt {
    submitted: crate::agent_backend::SubmittedPrompt,
    message: Queued,
    typed_by_person: bool,
}

/// Synchronizes against the same projection used to draw a snapshot or decide whether the queue can go.
fn settle_opening(
    opening_prompt: &mut Option<OpeningPrompt>,
    queue: &mut Vec<Queued>,
    projection: &agent::AgentSessionProjection,
) -> bool {
    let Some(opening) = opening_prompt.as_ref() else {
        return false;
    };
    let id = opening.submitted.client_message_id.clone();
    let cycled = projection
        .turn_starts
        .iter()
        .rev()
        .take_while(|start| start.seq > opening.submitted.after_revision)
        .any(|start| {
            Some(&start.turn_id) == opening.submitted.turn_id.as_ref() && start.origin == agent::TurnOrigin::Cli
        });
    if cycled {
        let opening = opening_prompt.take().unwrap();
        let mut message = opening.message;
        message.typed_by_person = opening.typed_by_person;
        message.state = QueuedState::Steered {
            client_id: id.clone(),
            sent_at_ms: wall_clock_ms(),
        };
        queue.insert(0, message);
        return true;
    }
    // A prompt the agent dropped before any cycle ran was never taken out of the conversation: its
    // row is still there, so it is not also a queue line, and it is not sent again.
    if matches!(
        projection.steer_outcome(&id),
        Some(agent::SteerOutcome::Delivered | agent::SteerOutcome::Dropped { .. })
    ) || (projection.active_turn_id.is_none() && projection.opening_prompt.is_none())
    {
        *opening_prompt = None;
    }
    false
}

/// What a flush sent, and how the backend answered.
pub struct Flush {
    pub typed: String,
    pub outcome: Result<(), BackendError>,
}

pub enum SendNow {
    /// A turn was running: it was asked to stop; the queue goes out when it ends.
    Interrupting(Result<(), BackendError>),
    /// Idle: flushed at once (`None` when there was nothing to send).
    Flushed(Option<Flush>),
}

/// Not boxed: a window holds a handful of tabs, `Live` is the state a tab spends its life in, and
/// callers match on `Live(AgentBackend)` directly.
#[allow(clippy::large_enum_variant)]
pub enum TabBackend {
    /// An empty tab: a live composer and no process. A session is created on the first
    /// send or resume.
    NotStarted,
    Starting(PendingStart),
    /// A start that waits for the trust question; nothing has been spawned. Shown to the panel as
    /// `starting`.
    AwaitingTrust(AwaitingTrust),
    /// A backend, whether its session is running or has ended (the projection says which).
    Live(AgentBackend),
    /// Never worked: a start that failed, a session that never opened, or a fatal command.
    Failed {
        reason: String,
    },
}

/// A revert's or an undo's claim on its tab: the write it waits for is wanted only while the tab
/// exists with the same session and nobody cancelled it. Closing the tab cancels it at once
/// ([`TabSet::remove`], [`TabSet::take_all`]), so a worker that has not reached its last check sees
/// the flag without waiting for a tick.
#[derive(Debug, Clone)]
pub struct ReviewTicket {
    pub tab: TabId,
    pub session: String,
    /// Shared with the job that does the write, which reads it right before it writes.
    pub wanted: Arc<AtomicBool>,
}

impl ReviewTicket {
    /// The tab exists, still holds the session the ticket was made under, and the ticket was not
    /// cancelled.
    pub fn is_current(&self, tabs: &TabSet) -> bool {
        self.wanted.load(Ordering::SeqCst)
            && tabs
                .get(self.tab)
                .and_then(Tab::provider_session_id)
                .is_some_and(|session| session == self.session)
    }

    pub fn cancel(&self) {
        self.wanted.store(false, Ordering::SeqCst);
    }
}

pub struct Tab {
    /// Whether completed edits are shown in the editor without taking focus.
    pub following: bool,
    pub id: TabId,
    pub number: u16,
    /// The rename (`prefix ,`), already normalized.
    pub name: Option<String>,
    /// Was React's `startedPermissionMode`; now survives a panel reload.
    /// Private outside this module for the same reason `host_answered` is: the rule that
    /// entering bypass always asks first only holds if nothing but `cycle_mode`/`confirm_bypass` can
    /// ever move it there. `mode()` below is the read-only way out; this module's own tests still
    /// write the field directly.
    mode: SessionModeChoice,
    /// The agent this tab runs, chosen when it was opened (or by the session a restore brings back
    /// into it) and kept by `r`. Private so that only the set decides it; [`Tab::agent`] reads it.
    agent: AgentId,
    /// The chat view that shows this tab: the base view unless the tab was bound to an extra one.
    /// Private so that only the set binds a tab, which keeps the base view's active tab unbound.
    view: ViewId,
    /// Private so that a live backend leaves a tab only through the set, as a counted [`Retired`];
    /// [`Tab::backend`] reads it, [`TabSet::start`] puts a connect in.
    backend: TabBackend,
    pub session: SessionState,
    pub attention: AttentionTracker,
    /// Whether the session had been open when the tab went `Failed`, so the panel words the
    /// failure as a session that stopped rather than one that never started. Every place that
    /// swaps in `Failed` sets it; it means nothing while the backend is anything else.
    pub failed_after_start: bool,
    /// The saved session this tab was starting or resuming when it went `Failed` (a restored tab whose
    /// start was refused, a resume by hand that failed), so the saved-tabs file keeps the conversation
    /// until the tab is closed or started over. Read only while the backend is `Failed`; every place
    /// that swaps `Failed` in sets it.
    pub failed_resume: Option<String>,
    pub pending_handoff: Option<PendingHandoff>,
    /// Held here, not only in the WebView, so a panel reload cannot lose it (see the doc this field
    /// had on `AgentPanelState` before session tabs; it moved per tab with them).
    pub last_handoff: Option<agent::handoff::ClaudeResumeCommand>,
    /// The scratch edit of the draft in nvim, when one is open (set by the shell's round trip).
    pub editing_draft: Option<u64>,
    /// The comments and reverts collected in this tab's turn review. Held here, not in the WebView,
    /// so a panel reload keeps it; it goes with the tab and is not saved.
    pub review_draft: crate::turn_review::ReviewDraft,
    /// What the rows and cards say beyond the projection, kept for every later snapshot: who answered
    /// a call without a card ([`NoteLedger`]), and the `Write` cards whose file did not exist when
    /// they were raised (`note_new_files`).
    notes: CallNotes,
    /// The flags of the writes (a revert, an undo) waiting on this tab ([`TabSet::review_ticket`]).
    /// Weak, so a finished write's flag goes with it; closing the tab clears every one that is
    /// still alive. Private: only the ticket route registers or clears them.
    review_watchers: Vec<Weak<AtomicBool>>,
    /// Every request this tab's host answered itself, in order, for tests that need to know which
    /// calls the gate saw and who answered (`Tab::gate_answers`). Compiled only for tests: a long
    /// session would otherwise grow it for nothing.
    #[cfg(any(test, feature = "test-support"))]
    gate_log: Vec<crate::agent_backend::GateAnswer>,
}

/// State replaced together when a tab installs or starts over a session. Public only because
/// `Tab::session` is; the fields that decide what the page and the tray may show stay private, so
/// only this module can change them.
#[derive(Default)]
pub struct SessionState {
    /// The first prompt's title, or the resumed record's display title.
    pub title: Option<String>,
    /// `title` came from the record a resume or restore was started from, before the CLI's own title
    /// for the session may have been read: a later read replaces it ([`TabSet::retitle_from`]).
    pub title_from_record: bool,
    /// The view this tab was just moved out of, whose page may still post one `draft` for it: its
    /// composer mirrors on a timer and flushes when it loses the keys, which can land after the move.
    /// Cleared once that draft is taken, or as soon as the draft is set any other way (a newer
    /// mirror, an nvim edit, a send, a start-over): that copy is newer than the late one.
    left_view: Option<ViewId>,
    /// Which of the project's own configuration tiers the session of this tab was given; set when
    /// the session is spawned, so a later change of the window's answer leaves running sessions as
    /// they were. Kept when the backend is installed, since it is set while the tab is starting.
    pub project_tiers: Option<agent::setting_sources::ProjectTrust>,
    /// The page needs a snapshot because this session received events while hidden.
    pub stale: bool,
    /// Tracks a turn's progress from input through delivery to the page.
    pub turn_trace: Option<crate::turn_trace::TurnTrace>,
    /// Prevents reporting the same start failure more than once.
    pub reported_start_failure: bool,
    /// Messages typed while a turn ran or while the session started, oldest first, kept across page
    /// reloads. Flushed as one turn when the turn ends, never while a card waits.
    pub queue: Vec<Queued>,
    opening_prompt: Option<OpeningPrompt>,
    /// The last refused flush's reason, until the queue changes or is sent.
    pub queue_error: Option<String>,
    /// Explains why messages are held instead of entering the running turn.
    pub queue_note: Option<String>,
    /// The composer mirror survives tab switches and page reloads.
    draft: String,
    question_draft: BTreeMap<String, serde_json::Value>,
    editing_question: Option<(String, u32)>,
    /// Permission id to the saved rule offered on its waiting card.
    pub rule_offers: std::collections::BTreeMap<String, agent::PrefixRule>,
    /// Tracks delivered turn boundaries so a turn's end is reported once. Re-reading the projection
    /// instead could see a completion folded concurrently before its delivery reaches the pump.
    was_running: bool,
    /// Whether Claude Code was last seen working where this panel cannot show it. Its end is a turn's
    /// end for the queue (a message held for that work), reported once like [`Self::was_running`]'s.
    was_unseen_busy: bool,
    /// Calls answered without a card, waiting for completion before their row says who answered
    /// ([`NoteLedger`]).
    note_ledger: NoteLedger,
    /// File-creation notes that no events payload has carried yet. The note is made when the gate is
    /// answered, and a gate usually arrives in a batch of its own that the answer leaves empty (no
    /// payload), so it waits here for the next one; a `Resync`'s snapshot carries every note and
    /// empties it.
    creates_file_unsent: BTreeSet<String>,
    /// Sorted pending ids prevent recomputing unchanged rule offers on every tick.
    rule_offers_seen: Vec<String>,
    /// The running turn's id and when (wall clock, ms since the epoch) the first tick saw it running.
    /// The panel holds only the active tab's state, so without this a switch back to a tab mid-turn
    /// would restart its elapsed clock; the snapshot carries it as `turnStartedAtMs`. Stamped from the
    /// projection, not from a `TurnStarted` event, so a snapshot and a pumped batch use the same
    /// clock; it is at most one 33 ms tick late, on every backend.
    turn_clock: Option<(String, u64)>,
    /// The id of the newest turn reported to the broker as begun. Read against `turn_clock`, so a turn is reported
    /// once, whichever pump stamped it, and ending one clears nothing. It goes with `turn_clock` when a tab moves:
    /// left behind, it would differ from the clock at the first pump after the move and report the turn as begun
    /// again, mid-turn.
    last_began_turn: Option<String>,
    /// How many of the newest entries in `notes.turn_times` no payload has carried yet. The active
    /// tab's events payload takes exactly those; a snapshot carries all of them and zeroes this.
    turn_times_unsent: usize,
    /// Permission ids this tab answered `allow` on the user's behalf (in bypass, or under the
    /// classifier) and is still waiting on the provider's `PermissionResolved` for. Hidden from the
    /// snapshot and from the tray, so a request nobody needs to see never draws a card and never
    /// counts. Private: only `tab_set` may insert, so the hiding rule cannot be bypassed from outside
    /// this module.
    host_answered: BTreeSet<String>,
    /// The calls the user approved on a card in this tab, this turn, each with the tool and input the
    /// card showed: the CLI's own prompt for exactly such a call, which follows the gate's request
    /// for it, is answered `allow` without a second card, once, as the real CLI asks once. Written
    /// only by `TabSet::answer_card` on an Approve that reached the provider; emptied when the turn
    /// ends (the CLI asks within the turn that made the call), on every `Resync` (one can swallow a
    /// turn boundary), on a start-over and with the backend. Private for the reason `host_answered`
    /// is.
    human_allowed: crate::agent_backend::HumanApprovals,
    /// Permission ids the user answered on a card here (any decision) that the projection still lists
    /// as pending, on the sidecar path until the provider's own `PermissionResolved` arrives. A rule
    /// is never offered for one again and [`Tab::card_is_waiting`] says no, so a second response for
    /// the same id (a replay after a tab switch or a page reload) can never save a rule; removing the
    /// offer once was not enough, as the next pump that saw another id arrive put it back. Pruned
    /// with `host_answered` once the id leaves the projection. Private for the reason
    /// `host_answered` is.
    user_answered: BTreeSet<String>,
    /// Where the tab stands with the line about a server of the tool server's name that is not Eitri's.
    foreign_server_note: ForeignServerNote,
    /// This tab has followed the CLI out of its auto mode ([`TabSet::follow_cli_out_of_auto`]): once, so
    /// a person who cycles back to auto afterwards stays there.
    followed_out_of_auto: bool,
    /// The tab moved to manual by itself, and has not said why yet.
    left_auto_note_owed: bool,
    /// What the panel's broker has been told of this session's calls of declared tools.
    client_calls: ClientCalls,
    /// What an external agent's session was given. It is read on the broker's facts because that agent's calls
    /// reach the broker with whatever name it sends; a sidecar session keeps its own list on its conversation.
    editor_declared: BTreeSet<String>,
    /// The cards the panel's broker holds for this tab, for the snapshots that rebuild a page. A copy: the broker
    /// is the only thing that answers one. Not in [`CallNotes`], which a move carries: a card ends with its call
    /// when a tab leaves the window.
    editor_cards: Vec<crate::editor_tools::cards::EditorCardWire>,
}

/// What a pump found about a tab's calls of declared tools, for the panel's broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientCallNews {
    /// A call the broker has not heard of.
    Opened(agent::OpenClientCall),
    /// The call is over, however it ended. A lost stream has no event of its own: it empties the session's open
    /// calls, so every call the tab held ends here.
    Settled { call_id: String },
    /// A turn of the tab began. Reported once for each turn the tab's clock stamped, however the pump came to see it.
    TurnBegan { turn_id: String },
    /// A turn of the tab ended interrupted.
    TurnInterrupted,
}

/// The tab's side of the broker's calls. Call lifecycle news is read from the projection on each pump,
/// so a call, its end and an interrupted turn survive a `Resync`. Follow paths come from event batches
/// and may be lost on a resync.
#[derive(Default)]
struct ClientCalls {
    /// The calls reported open and not yet reported over.
    seen: BTreeSet<String>,
    /// While set, the session's open calls are not this window's to report; nothing is compared. The value is how
    /// far the session had folded when it arrived: what a failed read of its open calls falls back on.
    hold: Option<u64>,
    /// Calls at or below this position of the session's stream were settled before this window took the tab up, or
    /// were told to try again on its arrival. None is reported, however late it folds here: the session's events
    /// reach this window behind the read that counted them.
    arrived_through: u64,
    /// Waiting for the panel, each tab's in order.
    news: Vec<ClientCallNews>,
    /// Successful edits for the follower, separate from the broker's call lifecycle.
    edited: Vec<PathBuf>,
    /// The position of the newest interrupted turn already reported.
    last_interrupt: Option<u64>,
}

impl ClientCalls {
    /// Reports what changed since the last pump. An end comes first, so a call that finished makes room for one
    /// that opened beside it.
    fn note(&mut self, backend: &AgentBackend) {
        if self.hold.is_none() {
            let open = backend.open_client_calls();
            for call_id in std::mem::take(&mut self.seen) {
                if open.iter().any(|call| call.call_id == call_id) {
                    self.seen.insert(call_id);
                } else {
                    self.news.push(ClientCallNews::Settled { call_id });
                }
            }
            for call in open {
                if self.seen.insert(call.call_id.clone()) && call.sequence > self.arrived_through {
                    self.news.push(ClientCallNews::Opened(call));
                }
            }
        }
        // The newest interrupted ending, not the newest ending: one that fails straight after is still an interrupt.
        let interrupted = backend
            .projection()
            .turn_endings
            .iter()
            .rfind(|ending| ending.kind == agent::TurnEndingKind::Interrupted)
            .map(|ending| ending.seq);
        if interrupted > self.last_interrupt {
            self.last_interrupt = interrupted;
            self.news.push(ClientCallNews::TurnInterrupted);
        }
    }
}

/// The line a tab owes the person when its session's tool server turns out not to be Eitri's alone, or could
/// not be given because the name is taken, or was left out by the agent's provider, said once per session: it is
/// true for as long as the session lives, and repeating it would tell nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum ForeignServerNote {
    #[default]
    Unseen,
    /// Seen, and no page has shown it yet.
    Owed,
    Said,
    /// The start left the tool server out, and no page has shown that yet. Set before the backend lands.
    NameTakenOwed(&'static str),
    NameTakenSaid(&'static str),
    /// The provider left the tool server out and said this; no page has shown it yet.
    ProviderOwed(String),
    /// Shown. The words stay for a caller that hands the line back.
    ProviderSaid(String),
}

const FOREIGN_SERVER_LINE: &str =
    "a server named eitri that is not Eitri's is loaded; its tools get no special treatment";

/// What the line says when the start left the tool server out for a name it saw.
pub const NAME_TAKEN_LINE: &str =
    "a server named eitri is in your Codex configuration, so the editor tools are off in this tab; rename it to have them";

/// What the line says when no such name was seen: a file that could hold one went unread.
pub const NAME_UNKNOWN_LINE: &str = "Eitri could not read all of your Codex configuration, so a server named eitri \
                                     may be in it; the editor tools are off in this tab (see the agents guide)";

impl SessionState {
    /// Reports a turn the clock now names and has not been reported. The clock is stamped from the projection on every
    /// path (the early read and a resync's snapshot alike), so a turn whose `TurnStarted` a resync dropped is still
    /// found here.
    fn note_turn_began(&mut self) {
        let Some((turn_id, _)) = &self.turn_clock else {
            return;
        };
        if self.last_began_turn.as_ref() == Some(turn_id) {
            return;
        }
        let turn_id = turn_id.clone();
        self.last_began_turn = Some(turn_id.clone());
        self.client_calls.news.push(ClientCallNews::TurnBegan { turn_id });
    }

    /// What a tab without a backend forgets: the backend that answered, noted or timed these on the
    /// user's behalf is gone, so there is nothing left to hide or note. The title, tiers, draft and
    /// queue stay until the next session boundary.
    fn forget_answers(&mut self) {
        self.question_draft.clear();
        self.opening_prompt = None;
        self.was_running = false;
        self.was_unseen_busy = false;
        self.turn_clock = None;
        self.last_began_turn = None;
        self.host_answered.clear();
        self.human_allowed.clear();
        self.note_ledger.clear();
        self.creates_file_unsent.clear();
        self.user_answered.clear();
        self.client_calls = ClientCalls::default();
        self.editor_cards.clear();
    }

    /// The part a freshly installed backend starts with. What the tab gathered while its backend
    /// connected is kept: the tiers it was spawned with and the tools it was given, what the user typed
    /// or queued meanwhile, and whether the page's copy is behind. Everything the earlier session
    /// answered, noted or timed is dropped, and so is any field added later unless it is named here.
    fn for_installed_backend(self) -> SessionState {
        SessionState {
            project_tiers: self.project_tiers,
            editor_declared: self.editor_declared,
            left_view: self.left_view,
            draft: self.draft,
            queue: self.queue,
            queue_error: self.queue_error,
            queue_note: self.queue_note,
            stale: self.stale,
            // Owed by the start of this very session, so the landing does not end it.
            foreign_server_note: match self.foreign_server_note {
                ForeignServerNote::NameTakenOwed(line) => ForeignServerNote::NameTakenOwed(line),
                _ => ForeignServerNote::Unseen,
            },
            ..SessionState::default()
        }
    }
}

impl Tab {
    /// The composer mirror survives tab switches and page reloads.
    pub fn draft(&self) -> &str {
        &self.session.draft
    }

    /// A replacement is newer than a mirror still on its way from the previous view.
    fn replace_draft(&mut self, text: String) {
        self.session.draft = text;
        self.session.left_view = None;
    }

    /// Every request this tab's host answered itself (a deferral or an `allow`), recorded where the
    /// answer was sent, so it holds one a resolution had already removed from the projection.
    #[cfg(any(test, feature = "test-support"))]
    pub fn gate_answers(&self) -> &[crate::agent_backend::GateAnswer] {
        &self.gate_log
    }

    fn new(id: TabId, number: u16, mode: SessionModeChoice, agent: AgentId, following: bool) -> Self {
        Tab {
            following,
            id,
            number,
            name: None,
            mode,
            agent,
            view: ViewId::BASE,
            backend: TabBackend::NotStarted,
            session: SessionState::default(),
            attention: AttentionTracker::default(),
            failed_after_start: false,
            failed_resume: None,
            pending_handoff: None,
            last_handoff: None,
            editing_draft: None,
            review_draft: crate::turn_review::ReviewDraft::default(),
            notes: CallNotes::default(),
            review_watchers: Vec::new(),
            #[cfg(any(test, feature = "test-support"))]
            gate_log: Vec::new(),
        }
    }

    /// Tells every write waiting on this tab that it is no longer wanted.
    fn cancel_review_watchers(&mut self) {
        for watcher in self.review_watchers.drain(..) {
            if let Some(flag) = watcher.upgrade() {
                flag.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Whether `permission_id` is a card still waiting for the user here: its live backend's
    /// projection lists it as pending and the user has not already answered it (`user_answered`).
    /// What the panel's "Always allow" checks before saving a rule.
    pub fn card_is_waiting(&self, permission_id: &str) -> bool {
        !self.session.user_answered.contains(permission_id)
            && self
                .live()
                .is_some_and(|backend| backend.projection().pending_permissions.contains_key(permission_id))
    }

    /// When this tab's running turn started, if one is running (see `turn_clock`).
    pub fn turn_started_at_ms(&self) -> Option<u64> {
        self.session.turn_clock.as_ref().map(|(_, at)| *at)
    }

    /// The tab's own permission mode. Read-only outside this module -- see the field's own doc.
    pub fn mode(&self) -> SessionModeChoice {
        self.mode
    }

    #[cfg(test)]
    pub(crate) fn set_mode_for_test(&mut self, mode: SessionModeChoice) {
        self.mode = mode;
    }

    /// The chat view that shows this tab ([`ViewId::BASE`] unless it is bound to an extra view).
    pub fn view_id(&self) -> ViewId {
        self.view
    }

    /// The agent this tab runs.
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// Whether this tab's sessions can be continued later, judged by its agent first: a Claude tab
    /// only on the sidecar backend (`kind`), Codex always (its own threads), an ACP agent only where it
    /// said it can load a saved session. Before its session is live, an ACP tab that is resuming one
    /// holds a session its agent recorded as loadable; the agent is asked again when that resume starts.
    pub fn resumable(&self, kind: BackendKind) -> bool {
        match &self.agent {
            AgentId::Claude => kind == BackendKind::Sidecar,
            AgentId::Codex => true,
            AgentId::Acp(_) => match &self.backend {
                TabBackend::Live(AgentBackend::Conversation { conversation, .. }) => conversation.resumes(),
                TabBackend::Starting(pending) => pending.resume.is_some(),
                TabBackend::AwaitingTrust(AwaitingTrust {
                    start: DeferredStart::Resume { .. },
                    ..
                }) => true,
                _ => false,
            },
        }
    }

    /// What this tab holds or is doing to get a session.
    pub fn backend(&self) -> &TabBackend {
        &self.backend
    }

    /// Puts `backend` in place as it is and returns what was there, for a test that builds the state it
    /// needs; nothing is counted or shut down.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_backend(&mut self, backend: TabBackend) -> TabBackend {
        std::mem::replace(&mut self.backend, backend)
    }

    /// A tab the set no longer holds ([`TabSet::remove`], [`TabSet::take_all`]) hands over what is left
    /// of its backend: a connect or a put-off start, never a live session, which the set took out first.
    pub fn into_backend(self) -> TabBackend {
        self.backend
    }

    /// Puts `Failed { reason }` in place of the backend together with the two fields every such swap
    /// sets: whether the session had opened (`after_start`) and the saved session the file keeps for
    /// the tab (`failed_resume`). A live backend that was there leaves counted in `retiring`.
    fn fail_live(
        &mut self,
        retiring: &mut RetiringCounts,
        reason: String,
        after_start: bool,
        failed_resume: Option<String>,
    ) -> Option<Retired> {
        let reason = match self.live() {
            Some(backend) => backend.failure_with_provider_notes(reason),
            None => reason,
        };
        self.failed_after_start = after_start;
        self.failed_resume = failed_resume;
        match std::mem::replace(&mut self.backend, TabBackend::Failed { reason }) {
            TabBackend::Live(backend) => Some(Retired::new(retiring, self.id, backend)),
            _ => None,
        }
    }

    /// Takes a live backend out, counted in `retiring`, and leaves `NotStarted`; anything else stays.
    fn retire_live(&mut self, retiring: &mut RetiringCounts) -> Option<Retired> {
        match std::mem::replace(&mut self.backend, TabBackend::NotStarted) {
            TabBackend::Live(backend) => Some(Retired::new(retiring, self.id, backend)),
            other => {
                self.backend = other;
                None
            }
        }
    }

    pub fn live(&self) -> Option<&AgentBackend> {
        match &self.backend {
            TabBackend::Live(backend) => Some(backend),
            _ => None,
        }
    }

    pub fn live_mut(&mut self) -> Option<&mut AgentBackend> {
        match &mut self.backend {
            TabBackend::Live(backend) => Some(backend),
            _ => None,
        }
    }

    /// The Claude session this tab holds or is resuming.
    pub fn provider_session_id(&self) -> Option<String> {
        match &self.backend {
            TabBackend::Live(backend) => backend.provider_session_id(),
            TabBackend::Starting(pending) => pending.resume.clone(),
            TabBackend::AwaitingTrust(AwaitingTrust {
                start: DeferredStart::Resume {
                    provider_session_id, ..
                },
                ..
            }) => Some(provider_session_id.clone()),
            _ => None,
        }
    }

    /// Live non-ambient work left in the provider process, independent of a turn.
    pub fn background_calls(&self) -> u32 {
        self.live().map_or(0, |backend| backend.projection().background_calls)
    }

    pub fn question_waiting(&self) -> bool {
        self.live()
            .is_some_and(|backend| backend.projection().waiting_questions().next().is_some())
    }

    pub fn turn_running(&self) -> bool {
        self.live()
            .is_some_and(|backend| backend.projection().active_turn_id.is_some())
    }

    /// Claude Code is working where this panel cannot show it (a turn it started itself, and whose
    /// start the panel never folded). `turn_running` does not say so, and a close, a handoff and the
    /// supervisor's status each need to.
    pub fn cli_unseen_busy(&self) -> bool {
        self.live().is_some_and(|backend| backend.projection().cli_unseen_busy)
    }

    /// Applies what the provider reported about the messages this tab put into the running turn: one the
    /// agent read leaves the queue (its row is in the conversation now), one it did not is held again
    /// where it stood, with the reason. Read from the projection's record (`steer_outcome`), not from a
    /// batch of events, so a resync that dropped the events loses none of it. Whether the queue changed.
    fn settle_steered(&mut self) -> bool {
        if self.session.opening_prompt.is_none()
            && self
                .session
                .queue
                .iter()
                .all(|item| item.is_held() && item.dropped_as.is_empty())
        {
            return false;
        }
        let TabBackend::Live(backend) = &self.backend else {
            return false;
        };
        let projection = backend.projection();
        let opening_changed = settle_opening(&mut self.session.opening_prompt, &mut self.session.queue, &projection);
        settle_against(
            &self.agent,
            &mut self.session.queue,
            &mut self.session.queue_note,
            &projection,
        ) || opening_changed
    }

    /// The tab's session is being taken away (a tripwire, a fatal outcome, a session that never opened, the
    /// terminal handoff): what it reported is applied, and a message still in the turn is held again, since
    /// nothing will report on it any more.
    /// Whether the queue changed.
    fn hold_steered_on_failure(&mut self) -> bool {
        let settled = self.settle_steered();
        if self.session.queue.iter().all(Queued::is_held) {
            return settled;
        }
        for item in self.session.queue.iter_mut() {
            item.state = QueuedState::Held;
        }
        self.session.queue_note = Some(held_note("the session was closed before the agent read it"));
        true
    }

    /// Whether this tab's agent reads a message starting with `/` as a command of its own. Before the session
    /// is live only Claude is known to.
    fn reads_commands(&self) -> bool {
        match self.live() {
            Some(backend) => reads_slash_commands(&self.agent, &backend.projection()),
            None => matches!(self.agent, AgentId::Claude),
        }
    }

    /// The line under the queue: why what stays there stays. A command held says so itself, for as long as one
    /// is, beside what the tab recorded about its messages.
    pub fn queue_line(&self) -> Option<String> {
        let reads_commands = self.reads_commands();
        let commands = self
            .session
            .queue
            .iter()
            .any(|item| item.is_held() && !item.refused && item.is_command(reads_commands))
            .then_some(COMMANDS_NOTE);
        match (commands, self.session.queue_note.as_deref()) {
            (Some(commands), Some(note)) => Some(format!("{commands}; {note}")),
            (commands, note) => commands.or(note).map(str::to_string),
        }
    }

    /// Whether Enter while a turn runs puts the message into it: the live session's agent can take one now.
    /// `None` without a live session.
    pub fn steers(&self) -> Option<bool> {
        self.live().map(AgentBackend::steers)
    }

    /// The messages sent into the running turn that the agent has not been seen to read.
    pub fn steered_unconfirmed(&self) -> usize {
        self.session.queue.iter().filter(|q| !q.is_held()).count()
    }

    /// The session `r` continues on this ended tab: one its agent stopped itself with the conversation whole,
    /// on a tab whose sessions can be continued, by its own id. `None` for every other tab, where `r` empties
    /// the tab to a start screen.
    fn resumes_on_r(&self, resumable: bool) -> Option<String> {
        if !resumable || self.pending_handoff.is_some() || self.wire_state() != TabStateWire::Ended {
            return None;
        }
        self.live()
            .filter(|backend| backend.stopped_resumable())
            .and_then(AgentBackend::provider_session_id)
            .filter(|id| !id.is_empty())
    }

    pub fn wire_state(&self) -> TabStateWire {
        match &self.backend {
            TabBackend::NotStarted => TabStateWire::NotStarted,
            TabBackend::Starting(_) => TabStateWire::Starting,
            TabBackend::AwaitingTrust(_) => TabStateWire::AwaitingTrust,
            TabBackend::Failed { .. } => TabStateWire::Failed,
            TabBackend::Live(backend) => {
                let ended = matches!(
                    backend.projection().status,
                    agent::ProjectionStatus::Closed { .. } | agent::ProjectionStatus::Unavailable { .. }
                );
                if ended {
                    TabStateWire::Ended
                } else {
                    TabStateWire::Live
                }
            }
        }
    }

    pub fn facts(&self) -> TabFacts {
        let attention = self.attention.attention();
        let state = self.wire_state();
        TabFacts {
            pending: attention.pending,
            working: state == TabStateWire::Live && self.turn_running(),
            unread: attention.unread,
            ended: matches!(state, TabStateWire::Ended | TabStateWire::Failed),
        }
    }

    pub fn label_name(&self) -> String {
        let id = self.provider_session_id();
        tabs::label_name(self.name.as_deref(), self.session.title.as_deref(), id.as_deref())
    }

    fn view(&self, resumable: bool) -> TabView {
        let facts = self.facts();
        TabView {
            following: self.following,
            question_draft: self.session.question_draft.clone(),
            id: self.id,
            number: self.number,
            label: tabs::label(self.number, &self.label_name()),
            name: self.name.clone(),
            state: self.wire_state(),
            mode: self.mode,
            marker: tabs::marker(facts),
            pending: facts.pending,
            resumable,
            failure: match &self.backend {
                TabBackend::Failed { reason } => Some(reason.clone()),
                _ => None,
            },
            failed_after_start: matches!(self.backend, TabBackend::Failed { .. }) && self.failed_after_start,
            resumes_on_r: self.resumes_on_r(resumable).is_some(),
            title: self.session.title.clone(),
            agent: self.agent.record_name(),
        }
    }
}

pub struct PumpOutput {
    /// The active tab's `events` or `snapshot` envelope, if it had any.
    pub active_payload: Option<String>,
    /// How urgent `active_payload` is (`panel_cadence`): [`EnvelopeClass::Stream`] only for an
    /// `events` envelope made entirely of streamed content with no call notes; a snapshot, a card,
    /// a turn's start or end and everything else is [`EnvelopeClass::Immediate`]. Meaningless when
    /// there is no payload.
    pub active_class: crate::panel_cadence::EnvelopeClass,
    /// The active tab's turn trace saw its first text in this batch.
    pub first_text: bool,
    /// Tabs whose turn went from running to not running in this tick: the caller flushes their
    /// queues.
    pub turn_ended: Vec<TabId>,
    /// Tabs whose `rule_offers` changed in this tick: the caller sends them to the panel.
    pub offers_changed: Vec<TabId>,
    /// Tabs whose queue changed in this tick (a message put into the turn was read, or held again): the
    /// caller sends the active one's queue to the panel.
    pub queue_changed: Vec<TabId>,
    /// Tabs this tick failed because their CLI reported an ungated permission mode,
    /// each with the backend taken out of it, retired. The caller shuts each one down off the
    /// GTK thread exactly as it does a closed tab's (`eitri_panel::agent_panel`'s `spawn_teardown`).
    pub tripped: Vec<Tripped>,
    /// Turns whose end snapshot landed this tick, for the status band (empty without a turn
    /// review installed).
    pub review_hints: Vec<crate::turn_review::ReviewHint>,
    /// What each tab bound to an extra chat view had for its own page this tick, in tab order. The
    /// base view's active tab is never here: its payload is `active_payload`.
    pub view_payloads: Vec<ShownPayload>,
}

/// One payload of a tab bound to an extra chat view, for that view's page: what `active_payload`,
/// `active_class` and `first_text` are for the base view's active tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownPayload {
    pub view: ViewId,
    pub tab: TabId,
    pub payload: String,
    pub class: crate::panel_cadence::EnvelopeClass,
    /// The tab's turn trace saw its first text in this batch.
    pub first_text: bool,
}

/// One tab the CLI-mode tripwire failed. See `PumpOutput::tripped`.
pub struct Tripped {
    pub tab: TabId,
    /// The same text the tab's `Failed` reason carries.
    pub reason: String,
    /// The session, still running until the caller shuts it down: its shutdown is what denies the
    /// requests left pending in it.
    pub retired: Retired,
}

/// How many shutdowns are outstanding per tab id ([`TabSet::retire_done`] ends one).
type RetiringCounts = std::collections::BTreeMap<u64, usize>;

/// A live backend taken out of its tab to be shut down off the GTK thread. Only the set builds one,
/// and building one counts the tab as retiring: until the caller reports [`TabSet::retire_done`] for
/// [`Retired::tab`], the window's presence stays as if a turn of the tab ran, since an agent's
/// subagent can still write while its session shuts down. Every way a backend leaves a tab yields
/// one: a close, `r`, the tripwire, a lost session, the terminal handoff and the window's close.
#[must_use = "a retired backend is shut down off the GTK thread and its retirement reported"]
pub struct Retired {
    tab: TabId,
    backend: AgentBackend,
}

impl Retired {
    /// The one constructor: counts `tab` as retiring in `retiring`. The caller syncs the presence.
    fn new(retiring: &mut RetiringCounts, tab: TabId, backend: AgentBackend) -> Self {
        *retiring.entry(tab.0).or_insert(0) += 1;
        Retired { tab, backend }
    }

    /// The tab the backend was counted as retiring for.
    pub fn tab(&self) -> TabId {
        self.tab
    }

    pub fn backend(&self) -> &AgentBackend {
        &self.backend
    }

    /// The backend itself, for the worker that shuts it down and drops it.
    pub fn into_backend(self) -> AgentBackend {
        self.backend
    }
}

/// A tab taken out of the set ([`TabSet::remove`], [`TabSet::take_all`]): the tab, whose backend is
/// whatever was not live (a connect, a put-off start), and the live backend, retired.
pub struct RemovedTab {
    pub tab: Tab,
    pub retired: Option<Retired>,
}

/// How a tab's session was lost, for [`TabSet::fail_session`]: it decides how the panel words the
/// failure and whether the saved-tabs file keeps the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedHow {
    /// The session had opened and then stopped (a command's fatal outcome): the panel says it stopped,
    /// and the file no longer keeps it.
    AfterStart,
    /// The session ended before it ever opened: the panel says it never started, and the file keeps the
    /// conversation it was starting or resuming until the tab is closed or started over.
    StartNeverOpened,
}

/// What the queue says while a command in it waits.
const COMMANDS_NOTE: &str = "commands run when the turn ends";

/// The line under the queue that says why its messages wait for the turn to end.
fn held_note(why: &str) -> String {
    format!("held until the turn ends: {why}")
}

/// What the queue says about a message the CLI refused: it is not sent again on its own.
fn refused_note(reason: &str) -> String {
    format!("{reason}; it is not sent again until you take it back")
}

/// [`Tab::settle_steered`] against a projection the caller already reads, so whatever else it decides
/// from the same read (whether a turn runs) is consistent with what the agent reported.
fn settle_against(
    agent: &AgentId,
    queue: &mut Vec<Queued>,
    note: &mut Option<String>,
    projection: &agent::AgentSessionProjection,
) -> bool {
    let reads_commands = reads_slash_commands(agent, projection);
    let mut changed = false;
    let mut dropped = None;
    queue.retain_mut(|item| {
        // Reported dropped under an earlier id, and read by the agent after all.
        let read_late = item
            .dropped_as
            .iter()
            .any(|id| matches!(projection.steer_outcome(id), Some(agent::SteerOutcome::Delivered)));
        let QueuedState::Steered { client_id, .. } = &item.state else {
            if read_late {
                changed = true;
                return false;
            }
            return true;
        };
        match projection.steer_outcome(client_id) {
            Some(agent::SteerOutcome::Delivered) => {
                changed = true;
                false
            }
            // The agent already read an earlier copy, so this one's drop leaves nothing to send again.
            Some(agent::SteerOutcome::Dropped { .. }) if read_late => {
                changed = true;
                false
            }
            // Read under an earlier id while the newest is still open: it stays in the queue, shown as
            // sent, until that id's outcome comes. Once no turn runs nothing more will come for it, and
            // the copy the agent read is enough.
            None if read_late && projection.active_turn_id.is_none() => {
                changed = true;
                false
            }
            Some(agent::SteerOutcome::Dropped { reason }) => {
                item.refused = reason == agent::CLI_REFUSED_DROP;
                dropped = Some(if item.refused {
                    refused_note(reason)
                } else {
                    held_note(reason)
                });
                item.dropped_as.push(client_id.clone());
                item.state = QueuedState::Held;
                changed = true;
                true
            }
            None => true,
        }
    });
    if let Some(text) = dropped {
        *note = Some(text);
    } else if changed
        && !queue
            .iter()
            .any(|item| item.is_held() && (item.refused || !item.is_command(reads_commands)))
    {
        // A held command says so itself (`Tab::queue_line`); this note is about messages, and about what the
        // CLI refused, which stays until it is taken back whatever it starts with.
        *note = None;
    }
    changed
}

/// Why a tab whose CLI reported `reported` was closed. Names the setting most
/// likely responsible, because that is the one thing the user can go and change.
pub fn ungated_cli_mode_reason(reported: &str, detail: &str) -> String {
    format!(
        "the CLI reports permission mode '{reported}' ({detail}) — a project's permissions.defaultMode? \
         Eitri runs only sessions it gates itself; the session was closed"
    )
}

/// Why a tab whose agent echoed or showed a policy other than the one Eitri sent was closed. The two
/// strings are the agent's own words for what was asked and what it reported.
pub fn policy_mismatch_reason(requested: &str, reported: &str) -> String {
    format!("the agent reported {reported}, Eitri asked for {requested}; the session was closed")
}

pub enum StartCollected {
    Installed {
        tab: TabId,
        request_id: String,
        first_turn: Option<FirstTurn>,
    },
    Failed {
        tab: TabId,
        request_id: String,
        error: BackendError,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResumeRoute {
    /// Already open in this window: only ever switched to.
    SwitchTo(TabId),
    /// Held by another window's lease: nothing is started.
    Refuse(String),
    /// Start the resume in this tab (an empty current tab, or a new one).
    StartIn(TabId),
}

/// Which mode a bypass entry moves: one tab's own mode, or the window's remembered `default_mode`
/// for tabs not yet started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassScope {
    Tab(TabId),
    Default,
}

/// A pending "enter bypass" prompt, stored on `TabSet::pending_bypass` until `y`/`n` answers it.
/// `approve` never reaches the panel (`serialize_confirm_bypass_for_js` sends only
/// `lines`) -- Rust keeps the list so the panel never counts cards or echoes an id back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BypassPlan {
    pub scope: BypassScope,
    /// Answers a stale confirm: only the exact prompt shown, by this nonce, may be accepted.
    pub nonce: u64,
    /// The delivered cards this entry would approve if confirmed now, `seq`-ordered -- the same set
    /// `confirm_bypass` re-derives fresh (`waiting_cards`) and compares against before acting.
    pub approve: Vec<String>,
    /// What the panel shows (`tabs::bypass_prompt`), one or more lines (the y/n plus any warning).
    pub lines: Vec<String>,
    /// Which of `tabs::bypass_prompt`'s texts `lines` is -- what the user is agreeing to. A live
    /// tab's prompt says nothing about new sessions; an empty tab's says they will use bypass too.
    /// `confirm_bypass` re-derives it and reprompts when it no longer matches (a live tab handed
    /// off to a terminal becomes `NotStarted` under an open live-tab prompt, and its `y` would then
    /// move the window default into bypass).
    pub prompt: tabs::PromptScope,
    /// The chat page that shows this prompt: the view of the tab it is about, or for the window
    /// default the page that asked. Only that page's `y` is taken, and the prompt goes when that
    /// page loses the keys. Never sent to the page: a page knows which page it is.
    pub page: ViewId,
}

/// Where a tab is shown: the base view (as its active tab or a background tab) or the extra view it
/// is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabPlace {
    Base(TabId),
    View(ViewId, TabId),
}

/// Why a tab could not be bound to, or opened in, an extra chat view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewError {
    /// `ViewId::BASE` given where an extra view is meant.
    NotAnExtraView,
    /// The view already shows a tab.
    Taken(ViewId),
    /// The base view would be left with no tab: tmux's "can't break with only one pane".
    OneTabLeft,
}

impl std::fmt::Display for ViewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViewError::NotAnExtraView => f.write_str("the base view is not an extra chat view"),
            ViewError::Taken(view) => write!(f, "agent {} already shows a tab", view.number()),
            ViewError::OneTabLeft => f.write_str("can't break with only one tab"),
        }
    }
}

impl std::error::Error for ViewError {}

/// What a stop of the background says when the agent left nothing running.
pub const NOTHING_IN_THE_BACKGROUND: &str = "nothing is running in the background";

/// The band's note for a tab frozen for a move to the other window: it takes no input and answers no
/// card until the move is done or called off.
pub const MOVING: &str = "moving";

/// Why a message is refused while another of its tab's messages waits for the presence to mark its
/// turn as running.
pub const SEND_HELD: &str = "a message of this tab is still waiting to go out";

/// `1 command` or `N commands`: how the panel counts what an agent left running.
pub fn running_in_background(n: u32) -> String {
    if n == 1 {
        "1 command".to_string()
    } else {
        format!("{n} commands")
    }
}

/// What `cycle_mode`/`cycle_default_mode` return: manual -> auto and leaving bypass move at once;
/// auto -> bypass always needs a confirm first, never applied here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeCycle {
    Changed(SessionModeChoice),
    Confirm(BypassPlan),
}

/// What `confirm_bypass` did with a stored `BypassPlan`.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmOutcome {
    /// Entered bypass, having approved `approved` cards. Resolutions reach the page through the pump.
    Entered { approved: usize },
    /// The tab was already in bypass by the time this confirm arrived (a race, not an error):
    /// nothing to do.
    AlreadyBypass,
    /// A delivered card arrived after the plan was built and is not in `approve`: nothing
    /// changed; the caller should show the fresh plan's prompt instead.
    Reprompt(BypassPlan),
}

/// What a restore does with a tab that was saved in bypass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassPolicy {
    /// Ask first ([`TabSet::begin_restore`] returns the question).
    Ask,
    /// Give it back as saved: only for a user whose own `init.lua` already made bypass the default.
    Keep,
    /// Give it back in auto.
    Downgrade,
}

/// The question a restore asks when a saved tab was in bypass (`TabSet::begin_restore`), shown by
/// the panel and answered with `TabSet::answer_restore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePrompt {
    /// Answers a stale prompt: only the exact one shown, by this nonce, may be accepted.
    pub nonce: u64,
    pub lines: Vec<String>,
}

struct PendingRestore {
    nonce: u64,
    plan: RestorePlan,
}

/// A saved tab cleared to be started, with the mode it will get. Only this module makes one, so the
/// rule that a tab enters bypass only on a yes (or the user's own `init.lua`) cannot be skipped by
/// whoever starts the restore.
pub struct GrantedTab {
    planned: PlannedTab,
    mode: SessionModeChoice,
}

/// A restore that is cleared to start: [`TabSet::start_restore`].
pub struct RestoreGo {
    tabs: Vec<GrantedTab>,
    skipped: Vec<Skipped>,
    active_session: Option<String>,
    /// How many tabs saved in bypass come back in auto.
    pub downgraded: usize,
    /// Whether the user was asked (a quiet downgrade says so afterwards; an answered `n` need not).
    pub asked: bool,
    /// The saved tabs are still on offer (a restore asked for from the dashboard leaves them so): while the
    /// window has nothing else to save, a restore put off leaves its tab empty and the offer standing, and
    /// the saved file untouched, instead of keeping its sessions as failed tabs. It ends for good once
    /// another tab has something to save ([`TabSet::settle_waiting_restore`]).
    pub reoffer: bool,
}

impl RestoreGo {
    /// How many of the tabs to start had a turn running when they were saved, and it never ended.
    pub fn cut_turns(&self) -> usize {
        self.tabs.iter().filter(|tab| tab.planned.turn_running).count()
    }

    /// The sessions to start, in order: what a caller that takes its leases ahead of time needs.
    pub fn session_ids(&self) -> Vec<String> {
        self.tabs
            .iter()
            .map(|t| t.planned.provider_session_id.clone())
            .collect()
    }

    /// [`session_ids`](Self::session_ids), each with its agent, under whose name its lease is taken.
    pub fn sessions(&self) -> Vec<(AgentId, String)> {
        self.tabs
            .iter()
            .map(|t| (t.planned.agent.clone(), t.planned.provider_session_id.clone()))
            .collect()
    }

    /// The agent whose trust answer the restore itself waits for: Claude's when any Claude tab comes
    /// back (those are started on that one answer), else the first tab's agent's. `None` for a restore
    /// with no tab to start.
    pub fn trust_agent(&self) -> Option<AgentId> {
        if self.tabs.iter().any(|t| t.planned.agent == AgentId::Claude) {
            return Some(AgentId::Claude);
        }
        self.tabs.first().map(|t| t.planned.agent.clone())
    }
}

/// How [`TabSet::start_restore`]'s `connect` begins one saved tab.
pub enum RestoreConnect {
    /// The resume is begun; its result arrives here.
    Started(mpsc::Receiver<Result<AgentBackend, BackendError>>),
    /// Not begun: the tab comes back holding its resume, waiting for its own agent's trust answer
    /// (a tab of another agent than the one the restore's own answer was about).
    AwaitTrust,
}

pub enum RestoreStep {
    Go(RestoreGo),
    Confirm(RestorePrompt),
}

pub struct TabSet {
    tabs: Vec<Tab>,
    active: TabId,
    last_active: Option<TabId>,
    next_id: u64,
    kind: BackendKind,
    default_mode: SessionModeChoice,
    following_default: bool,
    /// The agent a tab the set opens by itself runs (the base view's fresh tab once its last one
    /// left): Claude until `agent.default` is applied ([`TabSet::apply_configured_agent`]).
    configured_agent: AgentId,
    /// An "enter bypass" prompt on screen, if any: at most one at a time, a fresh press on the
    /// mode key (or any tab-switching call) superseding or dropping whatever was open.
    pending_bypass: Option<BypassPlan>,
    /// The next `BypassPlan::nonce`, starting at 1 so `0` is never a valid nonce to compare against.
    next_nonce: u64,
    /// Numbers the messages put into a running turn, for their client ids.
    next_steer: u64,
    /// Every card the tabs removed from this set had been handed, so the window's `arrived`
    /// (`attention`) never goes down when a tab closes: [`crate::attention::react`] reads only its
    /// growth.
    removed_arrived: u64,
    /// The project's saved prefix rules, window-level: every tab's pump applies them.
    rules: agent::PrefixRules,
    /// The tabs that were active, least recent first and each once: where the saved active tab falls
    /// back to when the active one has no session to save.
    recency: Vec<TabId>,
    /// A restore's bypass question on screen, if any: at most one prompt at a time, as `pending_bypass`.
    pending_restore: Option<PendingRestore>,
    /// Said once on the page when a tab could not take the window's mode (an agent that offers no
    /// manual mode starts in auto instead): [`TabSet::take_mode_note`].
    mode_note: Option<String>,
    /// Set once the window has taken the tabs away to close them (`take_all`). A set that has none
    /// left must not be mistaken for a user who closed every tab.
    closing: bool,
    /// Watches every tab's turns and snapshots the project around them; `None` until installed,
    /// and never on the legacy backend, which does not support turn review. Only the
    /// pump and the tab bookkeeping feed it: no answer or send path reads it.
    turn_review: Option<crate::turn_review::TurnReview>,
    /// The user switched turn review off (`review.enabled = false`): no review is installed and a
    /// request for one says why, instead of the misleading "needs the sidecar".
    turn_review_off: bool,
    /// This window's presence for other windows' reverts: held for every backend and with review
    /// on or off, since a window that merely has the project open, or runs a turn, blocks another
    /// window's revert. `None` until [`TabSet::hold_presence`].
    presence: Option<crate::turn_review::PresenceHolder>,
    /// Tabs whose backend left them still live and is being shut down elsewhere (a closed tab, `r`,
    /// a tripped or failed session, a terminal handoff, the window's close), each with how many such
    /// shutdowns are outstanding. Only [`Retired`]'s constructor adds to it. Their presence is kept as
    /// if a turn ran: an agent's subagent can still write during the shutdown's grace.
    /// [`TabSet::retire_done`] ends one.
    retiring: RetiringCounts,
    /// Tabs a turn of Eitri's is about to start in, or started in without its `TurnStarted` folded
    /// yet: entered just before the prompt goes out ([`TabSet::begin_turn_send`]) and left on that
    /// `TurnStarted`, a failed send, or a lifecycle event or turn ending folded after the entry. The
    /// presence counts them as running, so no window reverts while a turn may be starting, and a
    /// linked window's prompt waits until the presence thread holds their files.
    starting: BTreeSet<u64>,
    /// For each tab in `starting`, the projection's revision when it entered: only an event folded
    /// after it can say the start is over.
    starting_since: BTreeMap<u64, u64>,
    /// Prompts held until the presence says they may go out (a linked window), by tab: at most one
    /// per tab ([`TabSet::poll_turn_sends`]).
    pending_sends: BTreeMap<u64, moving::PendingSend>,
    /// Whether this window is linked to another one (`TabSet::set_partner`, `TabSet::add_partner`): then a
    /// prompt waits for its presence file before it goes out.
    linked: bool,
    /// The partners named one by one (`TabSet::add_partner`), by pid: the window stays linked until the last goes.
    partner_pids: BTreeSet<u32>,
    /// While linked with no turn review installed: the turns running here as the partner was last told of them,
    /// by tab, with each one's session and `CLOCK_MONOTONIC` start ([`TabSet::take_untracked_turn_edges`]).
    untracked_turns: BTreeMap<u64, (String, u64)>,
    /// While no turn review is installed: the tabs whose delivered events held a turn's start since the partner was
    /// last told ([`TabSet::take_untracked_turn_edges`]). A turn that started and ended in between leaves only this.
    untracked_starts: BTreeSet<u64>,
    /// A prompt's file could not be confirmed: the partner is to be told `presence{held: false}`
    /// ([`TabSet::take_presence_false_owed`]).
    presence_false_owed: bool,
    /// Tabs frozen for a move to the other window: they take no input and answer no card until the
    /// move is done ([`TabSet::hand_over`]) or does not happen ([`TabSet::unfreeze`]).
    moving: BTreeSet<u64>,
    /// Chats arriving from the other window, by move id: each session and the tab id held for it.
    /// Presence counts each hold before the other window lets go, until that move's drop after adoption
    /// ([`TabSet::hold_arrival`], [`TabSet::drop_arrival`]).
    arrivals: BTreeMap<u64, (String, TabId)>,
    /// When a tab's move was first refused because its running turn's session is not known yet, and when
    /// it was last refused so: the move waits for it a bounded while, then is refused for good.
    unbound_since: BTreeMap<u64, (std::time::Instant, std::time::Instant)>,
    /// The project root the last pump judged paths against, which a move's last pump uses too.
    project_root: Option<PathBuf>,
    /// What a move's last pump produced for the page that still shows the tab, handed to the caller
    /// with the next pump's output.
    held_pump: Option<moving::HeldPump>,
    /// Tabs whose held queue should go out as if their turn had just ended: one whose turn ended while
    /// it was frozen for a move, or one that arrived idle with messages waiting.
    flush_owed: BTreeSet<u64>,
    /// What every live backend calls when it folded something, so the window need not poll for it
    /// ([`set_ui_wake`](Self::set_ui_wake)).
    ui_wake: Option<agent::UiWake>,
    /// The `!` lines running, and where each tab's next one starts.
    shells: shell::Shells,
    /// The extra chat views the window's layout holds, in any workspace ([`TabSet::set_layout_views`]):
    /// the only views a restored tab may come back into.
    layout_views: Vec<ViewId>,
    /// Test seam: run once, right after `pump`'s next drain of a live tab's queue,
    /// so a test folds an event exactly where the race is -- after a drain and before the next
    /// snapshot read, `active_state_payloads`' or the `Resync` arm's own a few lock cycles later --
    /// rather than hoping a timing probe lands there.
    #[cfg(test)]
    after_drain: Option<BackendSeam>,
    /// Test-only: deliver what a snapshot already carried as well, as before the drain left those
    /// events out, so a test can show what leaving them out prevents (and record both for the reducer
    /// test).
    #[cfg(test)]
    redeliver_covered: bool,
    /// Test seam: run once, right before the `Resync` arm's own snapshot read --
    /// after its early turn-clock read and its own further lock cycles (its one read of the projection
    /// and `approve_pending`'s bypass sweep), which is exactly where that race is and where
    /// `after_drain` (fired right after the drain, before any of those) cannot reach.
    #[cfg(test)]
    before_resync_snapshot: Option<BackendSeam>,
}

/// A test seam's one-shot hook: what a test folds into a backend at the exact point the seam names.
#[cfg(test)]
type BackendSeam = Box<dyn FnOnce(&AgentBackend)>;

impl TabSet {
    /// One empty tab 1. Earlier windows' tabs are not brought back by this: they come back only when
    /// asked for (`begin_restore`).
    ///
    /// **Never in bypass**: a `Bypass` here is taken as `Auto`,
    /// with a log line, so "a launch never starts in bypass" holds by construction rather than
    /// because the one caller (`agent_panel`, passing `agent_prefs::startup_mode`) happens never to
    /// pass it. Without this, every tab of the window -- tab 1 and every later `open()` -- would sit
    /// in bypass with no `y` ever pressed, the one thing `Tab::mode` being private exists to stop.
    /// The one way a window starts in bypass is the user's own `init.lua`
    /// ([`TabSet::apply_configured_default`]), a separate call that says so.
    pub fn new(kind: BackendKind, default_mode: SessionModeChoice) -> Self {
        let default_mode = match default_mode {
            SessionModeChoice::Bypass => {
                eprintln!("[permission] a window never starts in bypass: new tabs start in auto");
                SessionModeChoice::Auto
            }
            mode => mode,
        };
        let mut set = TabSet {
            tabs: Vec::new(),
            active: TabId(0),
            last_active: None,
            next_id: 1,
            kind,
            default_mode,
            following_default: false,
            configured_agent: AgentId::Claude,
            pending_bypass: None,
            next_nonce: 1,
            next_steer: 1,
            removed_arrived: 0,
            rules: agent::PrefixRules::default(),
            recency: Vec::new(),
            pending_restore: None,
            mode_note: None,
            closing: false,
            turn_review: None,
            turn_review_off: false,
            presence: None,
            retiring: RetiringCounts::new(),
            starting: BTreeSet::new(),
            starting_since: BTreeMap::new(),
            pending_sends: BTreeMap::new(),
            linked: false,
            partner_pids: BTreeSet::new(),
            untracked_turns: BTreeMap::new(),
            untracked_starts: BTreeSet::new(),
            presence_false_owed: false,
            moving: BTreeSet::new(),
            arrivals: BTreeMap::new(),
            unbound_since: BTreeMap::new(),
            project_root: None,
            held_pump: None,
            flush_owed: BTreeSet::new(),
            ui_wake: None,
            shells: shell::Shells::default(),
            layout_views: Vec::new(),
            #[cfg(test)]
            after_drain: None,
            #[cfg(test)]
            redeliver_covered: false,
            #[cfg(test)]
            before_resync_snapshot: None,
        };
        set.open(AgentId::Claude);
        set.last_active = None;
        set
    }

    /// The configured following choice for tabs opened after this call.
    pub fn set_following_default(&mut self, following: bool) {
        self.following_default = following;
    }

    /// Changes one tab's following choice; an absent or frozen tab cannot be changed.
    pub fn toggle_following(&mut self, tab: TabId) -> bool {
        if self.is_moving(tab) {
            return false;
        }
        let Some(tab) = self.get_mut(tab) else { return false };
        tab.following = !tab.following;
        if !tab.following {
            tab.session.client_calls.edited.clear();
        }
        true
    }

    pub fn kind(&self) -> BackendKind {
        self.kind
    }

    /// Has every live backend, and every one installed later, call `wake` when it folded something for
    /// [`pump`](Self::pump) to take.
    pub fn set_ui_wake(&mut self, wake: agent::UiWake) {
        for tab in &self.tabs {
            if let TabBackend::Live(backend) = &tab.backend {
                backend.ensure_ui_wake(&wake);
            }
        }
        self.ui_wake = Some(wake);
    }

    /// A title read since a tab was started from a record replaces the record's, once: `title_of` answers a
    /// session's newest known title (from memory), and a tab that has one stops looking. Only tabs whose
    /// title still is the record's are touched; one titled by its first prompt is not, and a rename is the tab's `name`, which the label
    /// puts first anyway. Whether any title changed.
    pub fn retitle_from(&mut self, title_of: impl Fn(&str) -> Option<String>) -> bool {
        let mut changed = false;
        for tab in &mut self.tabs {
            if !tab.session.title_from_record {
                continue;
            }
            let Some(session) = tab.live().and_then(AgentBackend::provider_session_id) else {
                continue;
            };
            // Once: the tab takes the CLI's title as it is when first known (the old synchronous read at the
            // resume did no more), or keeps its own when that already is it, and follows no later retitle.
            if let Some(title) = title_of(&session) {
                tab.session.title_from_record = false;
                if tab.session.title.as_ref() != Some(&title) {
                    tab.session.title = Some(title);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Whether something here can become ready without telling anyone, so the window has to keep
    /// pumping on its tick: a connect or a terminal handoff running on a worker, a live backend that does
    /// not announce its events (the legacy one, or any before [`set_ui_wake`](Self::set_ui_wake)), turn
    /// review waiting for its workers, or a prompt held for the presence (the presence thread's
    /// confirmation and the hold's own deadline raise no wake; [`TabSet::poll_turn_sends`] finds them).
    /// When this is false, nothing changes here until a backend's wake, or a command from the window, says
    /// so.
    pub fn needs_polling(&self) -> bool {
        if !self.pending_sends.is_empty() {
            return true;
        }
        self.tabs.iter().any(|tab| {
            tab.pending_handoff.is_some()
                || match &tab.backend {
                    TabBackend::Starting(_) => true,
                    TabBackend::Live(backend) => !backend.announces_to_ui(),
                    TabBackend::NotStarted | TabBackend::AwaitingTrust(_) | TabBackend::Failed { .. } => false,
                }
        }) || self
            .turn_review
            .as_ref()
            .is_some_and(crate::turn_review::TurnReview::has_work_in_flight)
    }

    /// Starts reviewing turns on a backend that supports turn review. Legacy remains unsupported.
    pub fn install_turn_review(&mut self, review: crate::turn_review::TurnReview) -> Result<(), &'static str> {
        if self.kind == BackendKind::Legacy {
            return Err(TURN_REVIEW_NEEDS_SIDECAR);
        }
        self.turn_review = Some(review);
        Ok(())
    }

    /// Turns review off for good: nothing is installed, and a request for one answers
    /// [`TURN_REVIEW_OFF`].
    pub fn turn_review_off(&mut self) {
        self.turn_review = None;
        self.turn_review_off = true;
    }

    /// Holds this window's presence from now on: the tabs' running turns are handed to it by every
    /// pump. Any backend, and independent of [`TabSet::install_turn_review`].
    pub fn hold_presence(&mut self, holder: crate::turn_review::PresenceHolder) {
        self.presence = Some(holder);
        self.sync_presence();
    }

    /// What a revert's worker asks before it writes; `None` when no presence was ever held, so
    /// that every write is refused.
    pub fn presence_guard(&self) -> Option<crate::turn_review::PresenceGuard> {
        self.presence.as_ref().map(crate::turn_review::PresenceHolder::guard)
    }

    /// Hands the presence the ids of the tabs whose turns run, of those a turn is starting in
    /// (`starting`), and of those whose backend is still being shut down (a [`Retired`] not yet reported
    /// done). Reads only the tabs and touches no disk: the presence thread does that.
    fn sync_presence(&self) {
        if let Some(holder) = &self.presence {
            holder.sync(&self.running_set());
        }
    }

    /// The tabs the presence counts as running a turn: see [`TabSet::sync_presence`].
    fn running_set(&self) -> BTreeSet<u64> {
        let mut running: BTreeSet<u64> = self
            .tabs
            .iter()
            .filter(|tab| tab.turn_running())
            .map(|tab| tab.id.0)
            .collect();
        running.extend(self.retiring.keys().copied());
        running.extend(self.starting.iter().copied());
        running.extend(self.arrivals.values().map(|(_, tab)| tab.0));
        running
    }

    /// A live backend of `tab` taken away to be shut down elsewhere, counted: until that shutdown is
    /// reported ([`TabSet::retire_done`]), the window's presence stays as if a turn of `tab` ran, so
    /// no window reverts a file the agent may still be writing. Every backend counts; Claude's
    /// shutdown is simply quick.
    fn retire(&mut self, tab: TabId, backend: AgentBackend) -> Retired {
        let retired = Retired::new(&mut self.retiring, tab, backend);
        self.sync_presence();
        retired
    }

    /// The shutdown of one [`Retired`] counted for `tab` has returned. A tab with none outstanding is
    /// left alone.
    pub fn retire_done(&mut self, tab: TabId) {
        if let Some(count) = self.retiring.get_mut(&tab.0) {
            *count -= 1;
            if *count == 0 {
                self.retiring.remove(&tab.0);
            }
        }
        self.sync_presence();
    }

    /// The session of `tab` is lost (`reason` says why): whatever it reported about the messages put into
    /// its turn is applied and a message it never read is held again, since nothing will report on it any
    /// more; then its cards end (`shown`: whether the tab is on screen), `Failed { reason }` takes its
    /// place, and `failed_after_start` and `failed_resume` are set as `how` says. A live backend is
    /// returned retired, for the caller to shut down off the GTK thread and report with
    /// [`TabSet::retire_done`]. `None` for a tab that is gone or held no live backend.
    pub fn fail_session(&mut self, tab: TabId, reason: String, how: FailedHow, shown: bool) -> Option<Retired> {
        let at = self.position(tab)?;
        let t = &mut self.tabs[at];
        t.hold_steered_on_failure();
        // The cards of a session that is gone can no longer be answered.
        t.attention.session_ended(shown);
        let retired = match how {
            FailedHow::AfterStart => t.fail_live(&mut self.retiring, reason, true, None),
            FailedHow::StartNeverOpened => {
                t.session.reported_start_failure = true;
                // The session that never opened is still the one the file should keep for this tab.
                let kept = t.live().and_then(AgentBackend::provider_session_id);
                t.fail_live(&mut self.retiring, reason, false, kept)
            }
        };
        self.sync_presence();
        retired
    }

    /// The live session of `tab`, taken for the terminal handoff: a message it never read is held again,
    /// as for any session taken away, the tab becomes `NotStarted` (so no further turn can be sent), and
    /// the backend is returned retired, counted until the caller reports its shutdown with
    /// [`TabSet::retire_done`]. `None`, and the tab untouched, when it holds no live session.
    /// A tab frozen for a move, busy with a visible or unseen CLI turn, or holding background work or a
    /// waiting question is not taken.
    pub fn take_for_handoff(&mut self, tab: TabId) -> Option<Retired> {
        if self.moving.contains(&tab.0) {
            return None;
        }
        let t = self.get_mut(tab)?;
        if !matches!(t.backend, TabBackend::Live(_))
            || t.turn_running()
            || t.cli_unseen_busy()
            || t.background_calls() > 0
            || t.question_waiting()
        {
            return None;
        }
        t.hold_steered_on_failure();
        let TabBackend::Live(backend) = std::mem::replace(&mut t.backend, TabBackend::NotStarted) else {
            unreachable!("matched as live above")
        };
        Some(self.retire(tab, backend))
    }

    /// The installed turn review, for building review jobs.
    pub fn turn_review(&self) -> Option<&crate::turn_review::TurnReview> {
        self.turn_review.as_ref()
    }

    /// The installed turn review, for what a linked partner tells it (its turns' starts and ends) and what this
    /// window keeps for the partner.
    pub fn turn_review_mut(&mut self) -> Option<&mut crate::turn_review::TurnReview> {
        self.turn_review.as_mut()
    }

    /// The session `tab` reviews under and the review itself, or why there is neither.
    fn review_target(&self, tab: TabId) -> Result<(&crate::turn_review::TurnReview, String), String> {
        if self.turn_review_off {
            return Err(TURN_REVIEW_OFF.to_string());
        }
        let review = self.turn_review.as_ref().ok_or(TURN_REVIEW_NEEDS_SIDECAR)?;
        let tab = self.get(tab).ok_or_else(|| format!("protocol: no tab {}", tab.0))?;
        let session = tab
            .provider_session_id()
            .ok_or_else(|| "this tab has no session yet, so there is no turn to review".to_string())?;
        Ok((review, session))
    }

    /// The review and the session `tab` reviews under, or why there is neither: what the review
    /// flow asks before anything else, so a window with the review off or on the legacy backend
    /// answers with the reason whatever the request was.
    pub fn review_session(&self, tab: TabId) -> Result<(&crate::turn_review::TurnReview, String), String> {
        self.review_target(tab)
    }

    /// Why this window has no turn review, if it has none; a question about no tab in particular.
    pub fn review_unavailable(&self) -> Option<&'static str> {
        if self.turn_review_off {
            Some(TURN_REVIEW_OFF)
        } else if self.turn_review.is_none() {
            Some(TURN_REVIEW_NEEDS_SIDECAR)
        } else {
            None
        }
    }

    /// A claim for a write on `tab`'s session. The tab must exist and hold a session. It is
    /// registered with the tab, which cancels it when it closes.
    pub fn review_ticket(&mut self, tab: TabId) -> Result<ReviewTicket, String> {
        let found = self.get_mut(tab).ok_or_else(|| format!("protocol: no tab {}", tab.0))?;
        let session = found
            .provider_session_id()
            .ok_or_else(|| "this tab has no session yet, so there is no turn to review".to_string())?;
        found.review_watchers.retain(|watcher| watcher.strong_count() > 0);
        let wanted = Arc::new(AtomicBool::new(true));
        found.review_watchers.push(Arc::downgrade(&wanted));
        Ok(ReviewTicket { tab, session, wanted })
    }

    /// A review of `tab`'s session at `turn` in `scope`, ready to run on a worker. It attributes
    /// with the edit calls of `tab`'s own projection, for the turns the scope covers; a turn this
    /// run has not seen has no calls here, so its files come out unattributed rather than credited
    /// to the agent. Builds the job without git, waiting or touching the file system: the named
    /// paths are made relative to the project by the job, on its worker.
    pub fn review_overview_job(
        &self,
        tab: TabId,
        turn: crate::turn_review::TurnRef,
        scope: crate::turn_review::Scope,
    ) -> Result<crate::turn_review::OverviewJob, String> {
        use crate::turn_review::{Scope, TurnRef};
        let (review, session) = self.review_target(tab)?;
        let records = review.turns(&session);
        let turn_ids: Vec<&str> = match scope {
            Scope::Session => records.iter().map(|r| r.turn_id.as_str()).collect(),
            Scope::Turn => match turn {
                TurnRef::Latest => records.iter().max_by_key(|r| r.n),
                TurnRef::N(n) => records.iter().find(|r| r.n == n),
            }
            .map(|r| vec![r.turn_id.as_str()])
            .unwrap_or_default(),
        };
        let named = match self.get(tab).and_then(|t| t.live()) {
            Some(backend) => {
                // The projection's guard is dropped before anything else is read from the backend.
                let projection = backend.projection();
                crate::turn_review::named_paths(&projection.tool_calls, &turn_ids)
            }
            None => crate::turn_review::NamedPaths::default(),
        };
        Ok(review.overview_job(&session, turn, scope, named))
    }

    /// One file's patch in a review of `tab`'s session, ready to run on a worker.
    pub fn review_diff_job(
        &self,
        tab: TabId,
        turn: u32,
        scope: crate::turn_review::Scope,
        path: &str,
    ) -> Result<crate::turn_review::DiffJob, String> {
        let (review, session) = self.review_target(tab)?;
        Ok(review.diff_job(&session, turn, scope, path))
    }

    pub fn active(&self) -> TabId {
        self.active
    }
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }
    /// Reorders the base view's complete tab list, preserving extra views and their numbers. A tab that was here
    /// keeps its own number while that number is free and still follows the tab before it; a tab in `arrived`, and
    /// one whose number cannot be kept, takes the lowest free number after the tab before it.
    pub fn order_base_tabs(&mut self, order: &[TabId], arrived: &[TabId]) -> bool {
        let base = self.base_tabs();
        let wanted: std::collections::BTreeSet<_> = order.iter().copied().collect();
        if order.len() != base.len() || wanted.len() != base.len() || base.iter().any(|id| !wanted.contains(id)) {
            return false;
        }
        let used: Vec<_> = self
            .tabs
            .iter()
            .filter(|t| !t.view.is_base())
            .map(|t| t.number)
            .collect();
        let mut number = 1;
        for id in order {
            let own = self.get(*id).unwrap().number;
            if !arrived.contains(id) && own >= number && !used.contains(&own) {
                number = own;
            }
            while used.contains(&number) {
                number += 1;
            }
            self.get_mut(*id).unwrap().number = number;
            number += 1;
        }
        self.tabs.sort_by_key(|t| t.number);
        true
    }
    pub fn get(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }
    fn position(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }
    pub fn get_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }
    pub fn active_tab(&self) -> &Tab {
        self.get(self.active).expect("the active tab always exists")
    }
    pub fn active_tab_mut(&mut self) -> &mut Tab {
        let active = self.active;
        self.get_mut(active).expect("the active tab always exists")
    }
    pub fn default_mode(&self) -> SessionModeChoice {
        self.default_mode
    }

    /// `wanted` if a tab of `agent` offers it, else the mode it takes instead, noting once for the
    /// page that tab `number` did not get the mode it was to have. Only Manual can be missing (an ACP
    /// agent's own mode is set once at its start, so Eitri cannot make it stricter), and Auto stands in.
    fn mode_offered_to(&mut self, agent: &AgentId, wanted: SessionModeChoice, number: u16) -> SessionModeChoice {
        let mode = agent.offered_mode(wanted);
        if mode != wanted {
            self.mode_note = Some(format!(
                "{} tabs have no {} mode (the agent's own mode is set once, at its start): tab {number} is in {}",
                agent.label(),
                wanted.as_str(),
                mode.as_str()
            ));
        }
        mode
    }

    /// Puts the tab at `at` in `wanted`, or in what its agent offers instead, noting it for the page as
    /// [`TabSet::mode_offered_to`] does. Every route that gives an existing tab a mode goes through
    /// here, so a tab that did not get the mode it was to have is always said, and only that tab.
    fn give_mode(&mut self, at: usize, wanted: SessionModeChoice) {
        let (agent, number) = (self.tabs[at].agent.clone(), self.tabs[at].number);
        let mode = self.mode_offered_to(&agent, wanted, number);
        self.tabs[at].mode = mode;
    }

    /// The note [`TabSet::mode_offered_to`] left, once: the page shows it in its transient slot.
    pub fn take_mode_note(&mut self) -> Option<String> {
        self.mode_note.take()
    }

    /// What each tab owes the person about its session's tool server, taken: a caller with no page to
    /// show a line on hands it back with [`TabSet::owe_editor_note`].
    pub fn take_editor_notes(&mut self) -> Vec<(TabId, String)> {
        self.tabs
            .iter_mut()
            .filter_map(|tab| {
                let (said, line) = match &tab.session.foreign_server_note {
                    ForeignServerNote::Owed => (ForeignServerNote::Said, FOREIGN_SERVER_LINE.to_string()),
                    ForeignServerNote::NameTakenOwed(line) => {
                        (ForeignServerNote::NameTakenSaid(line), (*line).to_string())
                    }
                    ForeignServerNote::ProviderOwed(line) => {
                        (ForeignServerNote::ProviderSaid(line.clone()), line.clone())
                    }
                    _ => return None,
                };
                tab.session.foreign_server_note = said;
                Some((tab.id, line))
            })
            .collect()
    }

    /// The tabs that moved to manual by themselves and have not said why, taken: a caller with no page
    /// to show the line on hands it back with [`TabSet::owe_left_auto_note`].
    pub fn take_left_auto_notes(&mut self) -> Vec<TabId> {
        self.tabs
            .iter_mut()
            .filter_map(|tab| std::mem::take(&mut tab.session.left_auto_note_owed).then_some(tab.id))
            .collect()
    }

    /// `tab`'s line stays owed: the tab is on no page that can show it yet.
    pub fn owe_left_auto_note(&mut self, tab: TabId) {
        if let Some(tab) = self.get_mut(tab) {
            tab.session.left_auto_note_owed = true;
        }
    }

    /// `tab`'s line stays owed: the tab is on no page that can show it yet.
    pub fn owe_editor_note(&mut self, tab: TabId) {
        if let Some(tab) = self.get_mut(tab) {
            tab.session.foreign_server_note = match std::mem::take(&mut tab.session.foreign_server_note) {
                ForeignServerNote::NameTakenSaid(line) | ForeignServerNote::NameTakenOwed(line) => {
                    ForeignServerNote::NameTakenOwed(line)
                }
                ForeignServerNote::ProviderSaid(line) | ForeignServerNote::ProviderOwed(line) => {
                    ForeignServerNote::ProviderOwed(line)
                }
                _ => ForeignServerNote::Owed,
            };
        }
    }

    /// `tab`'s session is starting without the tool server, because the agent's own configuration holds a server of
    /// its name or may hold one, and `line` says which. A session already told is not told again.
    pub fn owe_name_taken_note(&mut self, tab: TabId, line: &'static str) {
        if let Some(tab) = self.get_mut(tab) {
            if !matches!(tab.session.foreign_server_note, ForeignServerNote::NameTakenSaid(_)) {
                tab.session.foreign_server_note = ForeignServerNote::NameTakenOwed(line);
            }
        }
    }

    /// What the pumps found about calls of declared tools, in order within each tab. A tab frozen for a move keeps
    /// its own: nothing of it may start here now.
    pub fn take_client_news(&mut self) -> Vec<(TabId, ClientCallNews)> {
        let moving = &self.moving;
        self.tabs
            .iter_mut()
            .filter(|tab| !moving.contains(&tab.id.0))
            .flat_map(|tab| {
                let id = tab.id;
                std::mem::take(&mut tab.session.client_calls.news)
                    .into_iter()
                    .map(move |news| (id, news))
            })
            .collect()
    }

    /// Successful edits observed while following was on, in order within each tab. A frozen tab
    /// keeps its pending paths until its move ends, just as it keeps its call news.
    pub fn take_follow_edits(&mut self) -> Vec<(TabId, PathBuf)> {
        let moving = &self.moving;
        self.tabs
            .iter_mut()
            .filter(|tab| !moving.contains(&tab.id.0))
            .flat_map(|tab| {
                let id = tab.id;
                std::mem::take(&mut tab.session.client_calls.edited)
                    .into_iter()
                    .map(move |path| (id, path))
            })
            .collect()
    }

    /// `tab` is leaving and the broker will not take a call of it, so what the pumps found goes unreported. Returned
    /// are the calls the broker never heard of, for the caller to answer: each found since the last report and, on a
    /// tab still waiting for its read, each open one. They stay in `seen`: a move called off runs none of them.
    pub fn take_unreported_client_calls(&mut self, tab: TabId) -> Vec<String> {
        let Some(tab) = self.get_mut(tab) else {
            return Vec::new();
        };
        let calls = &mut tab.session.client_calls;
        calls.edited.clear();
        let mut unreported = Vec::new();
        for news in std::mem::take(&mut calls.news) {
            if let ClientCallNews::Opened(call) = news {
                unreported.push(call.call_id);
            }
        }
        if let (Some(_), TabBackend::Live(backend)) = (calls.hold, &tab.backend) {
            unreported.extend(backend.open_client_calls().into_iter().map(|call| call.call_id));
            calls.seen.extend(unreported.iter().cloned());
        }
        unreported
    }

    /// `tab` arrived with its session folded through `folded_through`: its open calls are not reported until
    /// [`Self::release_client_calls`], because some of them were answered in the window it came from.
    pub fn hold_client_calls(&mut self, tab: TabId, folded_through: u64) {
        if let Some(tab) = self.get_mut(tab) {
            tab.session.client_calls.hold = Some(folded_through);
        }
    }

    /// The position `tab` holds its calls at; `None` when it holds none, a tab that left or changed session included.
    pub fn held_client_calls(&self, tab: TabId) -> Option<u64> {
        self.get(tab)?.session.client_calls.hold
    }

    /// `tab`'s calls are reported again, from where the read of its open calls stands: `open` was told to try again,
    /// and nothing at or below `through` will run here. Compared at once, since a call that folded during the hold
    /// raises no wake of its own.
    pub fn release_client_calls(&mut self, tab: TabId, through: u64, open: &[String]) {
        let Some(tab) = self.get_mut(tab) else { return };
        let calls = &mut tab.session.client_calls;
        let Some(folded_through) = calls.hold.take() else {
            return;
        };
        calls.arrived_through = through.max(folded_through);
        calls.seen.extend(open.iter().cloned());
        if let TabBackend::Live(backend) = &tab.backend {
            calls.note(backend);
        }
    }

    /// Hands `result` to the agent. Never waits on it, so the GTK thread may call this. A tab with no session has
    /// nobody left to tell.
    pub fn answer_client_call(
        &self,
        tab: TabId,
        call_id: &str,
        result: &crate::editor_tools::step::ToolResult,
    ) -> Result<(), BackendError> {
        match self.get(tab).and_then(Tab::live) {
            Some(backend) => backend.answer_client_tool_call(call_id, vec![result.text.clone()], result.is_error),
            None => Ok(()),
        }
    }

    /// The cards the panel's broker now holds for `tab`, the whole list: what the tray counts and a snapshot carries.
    /// They stay out of every list a sweep reads, so this is all a card ever is to the tab set.
    pub fn set_editor_cards(&mut self, tab: TabId, cards: Vec<crate::editor_tools::cards::EditorCardWire>) {
        if let Some(tab) = self.get_mut(tab) {
            tab.attention
                .sync_editor_cards(cards.iter().map(|card| card.id.clone()));
            tab.session.editor_cards = cards;
        }
    }

    /// What `tab`'s external session was given, set before it starts or as it arrives from the other window: the
    /// broker refuses any other name.
    pub fn set_editor_declared(&mut self, tab: TabId, declared: BTreeSet<String>) {
        if let Some(tab) = self.get_mut(tab) {
            tab.session.editor_declared = declared;
        }
    }

    /// What [`TabSet::set_editor_declared`] last said for `tab`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn editor_declared(&self, tab: TabId) -> Option<&BTreeSet<String>> {
        self.get(tab).map(|tab| &tab.session.editor_declared)
    }

    /// Whether `tab`'s newest turn was interrupted and no turn has started since. A bridge call read now belongs to
    /// that turn, however late: an agent calls from within a turn.
    pub fn last_turn_was_interrupted(&self, tab: TabId) -> bool {
        self.get(tab)
            .and_then(Tab::live)
            .is_some_and(|backend| backend.projection().last_turn_ending == Some(agent::TurnEndingKind::Interrupted))
    }

    /// What the broker reads of `tab`; `None` when it has no session. Each read is a statement of its own: on the
    /// sidecar both the session's id and the projection take the ingestion mutex. `/clear` empties the id's slot
    /// until the new conversation opens, and the projection names that conversation meanwhile: the process goes on
    /// through it, and so do its calls. An external agent's session that ended leaves its backend in the tab and
    /// never opens again, and nothing else ends its calls, so it has no facts: a sidecar's calls settle by themselves.
    pub fn broker_facts(&self, tab: TabId) -> Option<crate::editor_tools::broker::TabFacts> {
        let tab = self.get(tab)?;
        let backend = tab.live()?;
        if backend.agent().is_external() && tab.wire_state() == TabStateWire::Ended {
            return None;
        }
        let (revision, named) = {
            let projection = backend.projection();
            (projection.last_revision, projection.provider_session_id.clone())
        };
        let session = backend.provider_session_id().or(named)?;
        let process = backend.session_id()?.to_owned();
        let declared = if backend.agent().is_external() {
            &tab.session.editor_declared
        } else {
            backend.declared_client_tools()
        };
        Some(crate::editor_tools::broker::TabFacts {
            mode: tab.mode.into(),
            trust: tab
                .session
                .project_tiers
                .unwrap_or(agent::setting_sources::ProjectTrust::Untrusted),
            session,
            process,
            revision,
            declared: std::rc::Rc::new(declared.clone()),
        })
    }

    /// The rules apply to every tab from the next pump; a card already on screen is not
    /// answered by a rule added after it arrived.
    pub fn set_rules(&mut self, rules: agent::PrefixRules) {
        self.rules = rules;
    }

    pub fn rules(&self) -> &agent::PrefixRules {
        &self.rules
    }

    /// `prefix c`: a new empty tab at the lowest free number, selected, running `agent`.
    pub fn open(&mut self, agent: AgentId) -> TabId {
        self.open_tab(agent, true)
    }

    /// [`TabSet::open`]; `told` false leaves the page untold about a mode the agent does not offer, for
    /// a caller that gives the tab its own agent and mode at once (a restore), which says it then.
    fn open_tab(&mut self, agent: AgentId, told: bool) -> TabId {
        let id = self.insert_tab(agent, told);
        self.select(id);
        debug_assert!(self.v1_holds());
        id
    }

    /// A new empty tab in the base view at the lowest free number, in tab order, NOT selected: the
    /// caller selects it or binds it. A tab opened straight into an extra view goes through here so
    /// it is never the base view's active tab, not even for a moment.
    fn insert_tab(&mut self, agent: AgentId, told: bool) -> TabId {
        let id = TabId(self.next_id);
        self.next_id += 1;
        self.insert_tab_as(id, agent, told)
    }

    /// [`TabSet::insert_tab`] under an id already taken from `next_id` (an arrival's, held for it).
    fn insert_tab_as(&mut self, id: TabId, agent: AgentId, told: bool) -> TabId {
        let used: Vec<u16> = self.tabs.iter().map(|t| t.number).collect();
        let number = tabs::lowest_free_number(&used);
        let mode = if told {
            self.mode_offered_to(&agent, self.default_mode, number)
        } else {
            agent.offered_mode(self.default_mode)
        };
        self.tabs
            .push(Tab::new(id, number, mode, agent, self.following_default));
        self.tabs.sort_by_key(|t| t.number);
        id
    }

    /// Makes `id` the base view's active tab. A tab bound to an extra view is never selected (it is
    /// shown in its own view): `false`, and nothing changes.
    pub fn select(&mut self, id: TabId) -> bool {
        let Some(tab) = self.get(id) else {
            return false;
        };
        if !tab.view.is_base() {
            return false;
        }
        if id != self.active {
            self.last_active = Some(self.active);
            self.active = id;
            self.note_recent(id);
            // Every method that MOVES the active tab drops a pending bypass prompt --
            // it was about the tab the user is leaving, and a `y` typed after switching away must
            // never land on it. Only the base page's: an extra view still shows what it showed.
            self.drop_bypass_prompt_in(ViewId::BASE);
        }
        debug_assert!(self.v1_holds());
        true
    }

    fn note_recent(&mut self, id: TabId) {
        self.recency.retain(|t| *t != id);
        self.recency.push(id);
    }

    /// Tab `number`: a base-view tab is selected; a tab bound to an extra view is only reported, with
    /// its view, so the caller can give that view the keys. `None` for a number no tab has.
    pub fn select_number(&mut self, number: u16) -> Option<TabPlace> {
        let id = self.tabs.iter().find(|t| t.number == number)?.id;
        let place = self.select_place(id);
        debug_assert!(self.v1_holds());
        place
    }

    /// `n` (+1) / `p` (-1), in number order, wrapping, over the base view's tabs only: a
    /// bound tab is in its own view and never becomes the base view's.
    pub fn step(&mut self, delta: i32) -> Option<TabId> {
        let numbers: Vec<u16> = self
            .tabs
            .iter()
            .filter(|t| t.view.is_base())
            .map(|t| t.number)
            .collect();
        let next = tabs::step(&numbers, self.active_tab().number, delta)?;
        let id = self.tabs.iter().find(|t| t.number == next && t.view.is_base())?.id;
        self.select(id);
        debug_assert!(self.v1_holds());
        Some(id)
    }

    pub fn has_last(&self) -> bool {
        self.last_active
            .is_some_and(|id| self.get(id).is_some_and(|t| t.view.is_base()))
    }

    pub fn select_last(&mut self) -> Option<TabId> {
        let last = self
            .last_active
            .filter(|id| self.get(*id).is_some_and(|t| t.view.is_base()))?;
        self.select(last);
        debug_assert!(self.v1_holds());
        Some(last)
    }

    /// Where the base view falls back to when its active tab leaves it (closed or broken out): the
    /// last active tab if it is still a base-view tab, else the base-view tab now at the leaving tab's
    /// place among them (or the last one). `base_index` is the leaving tab's index among the base
    /// view's tabs before it left; `None` when no base-view tab is left.
    fn base_fallback(&self, base_index: usize) -> Option<TabId> {
        if let Some(last) = self
            .last_active
            .filter(|id| self.get(*id).is_some_and(|t| t.view.is_base()))
        {
            return Some(last);
        }
        let base = self.base_tabs();
        base.get(base_index.min(base.len().checked_sub(1)?)).copied()
    }

    /// Takes a tab out and hands it back: its live backend, retired, its pending start or handoff are
    /// the caller's to shut down or watch; the caller reports [`TabSet::retire_done`] once the retired
    /// backend's shutdown has returned. When the base view's last tab goes, a fresh tab 1 opens there
    /// whatever the extra views still hold.
    ///
    /// A tab frozen for a move is not taken out (`None`, as for a tab that is not here): its backend is
    /// the other window's to take up, and retiring it here would shut that session down. It leaves
    /// through [`TabSet::hand_over`], or takes input again through [`TabSet::unfreeze`].
    pub fn remove(&mut self, id: TabId) -> Option<RemovedTab> {
        if self.moving.contains(&id.0) {
            return None;
        }
        let at = self.tabs.iter().position(|t| t.id == id)?;
        let base_index = self.base_tabs().iter().position(|t| *t == id);
        let before = self.active;
        let mut tab = self.tabs.remove(at);
        // First, before anything that can take time: a write that has not reached its last check
        // must see that its tab is gone.
        tab.cancel_review_watchers();
        if let Some(review) = self.turn_review.as_mut() {
            review.close_tab(id.0, wall_clock_ms());
        }
        // Counted before the sync below, so the presence never drops between the turn and the shutdown.
        let retired = tab.retire_live(&mut self.retiring);
        self.forget_move_state(id);
        self.sync_presence();
        self.tab_left(&tab, base_index, before);
        Some(RemovedTab { tab, retired })
    }

    /// What the set does once `tab` has left it, closed or handed to the other window: its arrivals
    /// still count, nothing remembers it, a prompt about it goes, and the base view keeps an active tab
    /// (a fresh tab 1 when its last one left). `base_index` is where the tab stood among the
    /// base view's tabs, `before` the active tab before it left.
    fn tab_left(&mut self, tab: &Tab, base_index: Option<usize>, before: TabId) {
        let id = tab.id;
        self.removed_arrived += tab.attention.attention().arrived;
        self.recency.retain(|t| *t != id);
        if self.last_active == Some(id) {
            self.last_active = None;
        }
        // A prompt about a tab that is gone can never be answered: it goes with it.
        if self
            .pending_bypass
            .as_ref()
            .is_some_and(|plan| plan.scope == BypassScope::Tab(id))
        {
            self.pending_bypass = None;
        }
        if !self.tabs.iter().any(|t| t.view.is_base()) {
            self.open(self.configured_agent.clone());
            self.last_active = None;
        } else if self.active == id {
            let fallback = self
                .base_fallback(base_index.unwrap_or(0))
                .expect("a base-view tab is left");
            self.last_active = None;
            self.active = fallback;
            self.note_recent(fallback);
        }
        // `open()` above already dropped it through `select`; this covers the fallback path, which
        // assigns `self.active` directly.
        if self.active != before {
            self.drop_bypass_prompt_in(ViewId::BASE);
        }
        debug_assert!(self.v1_holds());
    }

    /// The invariant: the base view's active tab exists and is a base-view tab (so the base view always holds
    /// one), and no extra view holds more than one tab. Checked after every path that moves `active`
    /// or a binding.
    fn v1_holds(&self) -> bool {
        if !self.get(self.active).is_some_and(|t| t.view.is_base()) {
            return false;
        }
        let mut seen = BTreeSet::new();
        self.tabs
            .iter()
            .filter(|t| !t.view.is_base())
            .all(|t| seen.insert(t.view))
    }

    /// The chat view that shows `tab`: [`ViewId::BASE`] for a tab it does not have.
    pub fn view_of(&self, tab: TabId) -> ViewId {
        self.get(tab).map_or(ViewId::BASE, |t| t.view)
    }

    /// The tab an extra view shows; `None` for the base view (its tab is [`TabSet::active`]) and for
    /// a view that shows none.
    pub fn tab_in(&self, view: ViewId) -> Option<TabId> {
        if view.is_base() {
            return None;
        }
        self.tabs.iter().find(|t| t.view == view).map(|t| t.id)
    }

    /// The extra views that hold a tab, ascending.
    pub fn views(&self) -> Vec<ViewId> {
        let mut views: Vec<ViewId> = self.tabs.iter().filter(|t| !t.view.is_base()).map(|t| t.view).collect();
        views.sort();
        views
    }

    /// Activity of every view holding a tab, in view order.
    pub fn view_activity(&self) -> Vec<(ViewId, ViewActivity)> {
        self.activity_and_turns().0
    }

    /// [`Self::view_activity`] and the tabs a turn runs in, each in its view, from one read of each tab, so the two agree:
    /// the ingest thread can end a turn at any moment, and a view counted as running beside a turn set without it would
    /// be reported busy with nothing left running in it.
    pub fn activity_and_turns(&self) -> (Vec<(ViewId, ViewActivity)>, BTreeSet<(ViewId, TabId)>) {
        let mut activity = BTreeMap::new();
        let mut turns = BTreeSet::new();
        for tab in &self.tabs {
            let turn_running = tab.turn_running();
            if turn_running {
                turns.insert((tab.view, tab.id));
            }
            let entry = activity
                .entry(tab.view)
                .or_insert(ViewActivity { running: 0, queued: 0 });
            entry.running = entry.running.saturating_add(u32::from(
                matches!(tab.backend, TabBackend::Starting(_)) || turn_running,
            ));
            entry.queued = entry
                .queued
                .saturating_add(tab.session.queue.len().try_into().unwrap_or(u32::MAX));
        }
        (activity.into_iter().collect(), turns)
    }

    /// The base view's tabs, in tab order.
    pub fn base_tabs(&self) -> Vec<TabId> {
        self.tabs.iter().filter(|t| t.view.is_base()).map(|t| t.id).collect()
    }

    /// Every tab on screen in some view: the base view's active tab first, then each extra view's
    /// tab, by view.
    pub fn shown(&self) -> Vec<(ViewId, TabId)> {
        let mut shown = vec![(ViewId::BASE, self.active)];
        shown.extend(
            self.views()
                .into_iter()
                .filter_map(|view| self.tab_in(view).map(|tab| (view, tab))),
        );
        shown
    }

    /// Whether `tab` is on screen in its view: the base view's active tab, or a bound tab.
    pub fn is_shown(&self, tab: TabId) -> bool {
        self.get(tab).is_some_and(|t| t.id == self.active || !t.view.is_base())
    }

    /// Where `tab` is shown; `None` for a tab it does not have.
    pub fn place_of(&self, tab: TabId) -> Option<TabPlace> {
        let t = self.get(tab)?;
        Some(if t.view.is_base() {
            TabPlace::Base(tab)
        } else {
            TabPlace::View(t.view, tab)
        })
    }

    /// Whether an answer about `tab` may be taken from chat page `page`: the tab exists and is in
    /// that page's view. Never compared with the active tab: a base-view tab in the background is
    /// still the base page's to answer, and a tab that moved to another view is not its old page's.
    pub fn may_answer(&self, tab: TabId, page: ViewId) -> bool {
        self.get(tab).is_some_and(|t| t.view == page)
    }

    /// `prefix !` (tmux's `break-pane`): binds the base view's active tab to the empty extra view
    /// `view`; the base view then shows its last active tab, or the neighbour. Returns the moved tab.
    pub fn break_out(&mut self, view: ViewId) -> Result<TabId, ViewError> {
        if view.is_base() {
            return Err(ViewError::NotAnExtraView);
        }
        if self.tab_in(view).is_some() {
            return Err(ViewError::Taken(view));
        }
        let base = self.base_tabs();
        if base.len() < 2 {
            return Err(ViewError::OneTabLeft);
        }
        let moved = self.active;
        let base_index = base
            .iter()
            .position(|t| *t == moved)
            .expect("the active tab is a base-view tab");
        let at = self.position(moved).expect("the active tab exists");
        self.tabs[at].view = view;
        self.tabs[at].session.left_view = Some(ViewId::BASE);
        let fallback = self.base_fallback(base_index).expect("two base-view tabs were checked");
        self.last_active = None;
        self.active = fallback;
        self.note_recent(fallback);
        self.drop_prompts_about(moved);
        eprintln!("[tabs] tab {} -> agent {}", self.tabs[at].number, view.number());
        debug_assert!(self.v1_holds());
        Ok(moved)
    }

    /// The tab bound to `view` comes back into the base view as its active tab; the tab that was
    /// active becomes the last active one. `None` when the view shows no tab.
    pub fn join_back(&mut self, view: ViewId) -> Option<TabId> {
        let tab = self.tab_in(view)?;
        let at = self.position(tab).expect("tab_in names a tab it has");
        self.tabs[at].view = ViewId::BASE;
        self.tabs[at].session.left_view = Some(view);
        self.drop_prompts_about(tab);
        // The view is left showing nothing: whatever it was asking goes too.
        if self.pending_bypass.as_ref().is_some_and(|plan| plan.page == view) {
            self.pending_bypass = None;
        }
        self.select(tab);
        debug_assert!(self.v1_holds());
        Some(tab)
    }

    /// A new chat view's tab: a new empty tab of `agent` at the lowest free number, in the window's
    /// default mode as [`TabSet::open`] gives it, bound to the empty extra view `view`. The base
    /// view's active tab is unchanged.
    pub fn open_in(&mut self, agent: AgentId, view: ViewId) -> Result<TabId, ViewError> {
        if view.is_base() {
            return Err(ViewError::NotAnExtraView);
        }
        if self.tab_in(view).is_some() {
            return Err(ViewError::Taken(view));
        }
        let id = self.insert_tab(agent, true);
        let at = self.position(id).expect("just inserted");
        self.tabs[at].view = view;
        debug_assert!(self.v1_holds());
        Ok(id)
    }

    /// A pick of `tab` (the chooser, a number key): a base-view tab is selected; a bound tab is only
    /// reported with its view. `None` for a tab it does not have.
    pub fn select_place(&mut self, tab: TabId) -> Option<TabPlace> {
        let place = self.place_of(tab)?;
        if let TabPlace::Base(id) = place {
            self.select(id);
        }
        debug_assert!(self.v1_holds());
        Some(place)
    }

    /// Drops what was asked about `tab` where it was shown: its trust question and a bypass prompt
    /// about it. Used when the tab changes view, so neither page answers it from there.
    fn drop_prompts_about(&mut self, tab: TabId) {
        if self
            .pending_bypass
            .as_ref()
            .is_some_and(|plan| plan.scope == BypassScope::Tab(tab))
        {
            self.pending_bypass = None;
        }
        if let Some(Tab {
            backend: TabBackend::AwaitingTrust(waiting),
            ..
        }) = self.get_mut(tab)
        {
            waiting.asking = None;
        }
    }

    /// `prefix ,`: normalized and handed to a live sidecar's record. A tab that
    /// has no backend yet gets it written when one is installed (`collect_starts`).
    pub fn rename(&mut self, id: TabId, raw: &str) -> bool {
        let Some(tab) = self.get_mut(id) else { return false };
        tab.name = tabs::normalize_name(raw);
        let name = tab.name.clone();
        if let Some(backend) = tab.live_mut() {
            backend.note_name(name);
        }
        true
    }

    /// The confirmed `<leader>k` of tab `id`: stops the commands its agent left running after its turns
    /// ended: a Claude tab sends one stop per live task and keeps its session, another agent's tab
    /// ends its session. `shown` is the count the question named; when the tab's count is
    /// another now, nothing is stopped and the user is asked to look again, since the answer was to a
    /// different question. Every refusal is benign and leaves the session as it was: no such tab, no
    /// live session, a turn running, nothing left running.
    pub fn stop_background(&mut self, id: TabId, shown: u32) -> Result<(), BackendError> {
        let benign = |message: String| BackendError { message, benign: true };
        if self.moving.contains(&id.0) {
            return Err(benign(MOVING.to_string()));
        }
        let Some(tab) = self.get_mut(id) else {
            return Err(benign(format!("no tab {}", id.0)));
        };
        let Some(backend) = tab.live_mut() else {
            return Err(benign("no active session".to_string()));
        };
        let turn_running = backend.projection().active_turn_id.is_some();
        let running = backend.projection().background_calls;
        if turn_running {
            return Err(benign(
                "a turn is running: wait for it to end, then ask again".to_string(),
            ));
        }
        if running == 0 {
            return Err(benign(NOTHING_IN_THE_BACKGROUND.to_string()));
        }
        if running != shown {
            return Err(benign(format!(
                "{} now running in the background, not {shown}: nothing was stopped; ask again",
                running_in_background(running)
            )));
        }
        backend.stop_background(shown)
    }

    /// `Shift+Tab` / `<leader>m`: cycles the tab's mode in Claude Code's order, manual -> auto ->
    /// bypass -> manual, skipping manual on a tab whose agent does not offer it
    /// ([`AgentId::permission_modes`]: auto <-> bypass there). An explicit `match` on the mode, never
    /// `SessionModeChoice::cycled`, because the steps are not alike:
    ///
    /// - manual -> auto moves at once, on a tab whose session has not ended;
    /// - auto -> bypass never applies here: it always needs a `confirm_bypass` first, and only the tab
    ///   on screen can ask, so this returns a plan to show rather than a changed mode;
    /// - leaving bypass moves at once, in every tab state including `ended`/`failed`, to manual (or
    ///   auto where manual is not offered): the strictest mode the tab has.
    ///
    /// An empty tab's move carries the window default with it (there is no session yet for the mode
    /// to be "just this tab's own"); leaving bypass on any other tab returns a bypass window default
    /// to auto, so it is never stranded there once the tab that went with it leaves. Leaving bypass on
    /// an empty tab whose agent has no manual puts the tab in auto but the window default in manual,
    /// as on any other empty tab: auto there is only what that agent offers instead.
    pub fn cycle_mode(&mut self, id: TabId, page: ViewId) -> Result<ModeCycle, String> {
        if self.moving.contains(&id.0) {
            return Err(MOVING.to_string());
        }
        // A press from a page that does not show the tab is about something else: refused before
        // anything changes.
        if let Some(tab) = self.get(id) {
            if !self.may_answer(id, page) {
                return Err(format!("tab {} is not shown in this page", tab.number));
            }
        }
        // A fresh press supersedes whatever prompt was already open; `Confirm` below stores a
        // new one if it builds one.
        self.supersede_prompts(page);
        let at = self
            .tabs
            .iter()
            .position(|t| t.id == id)
            .ok_or_else(|| format!("no tab {}", id.0))?;
        let not_started = matches!(self.tabs[at].backend, TabBackend::NotStarted);
        match self.tabs[at].mode {
            SessionModeChoice::Bypass => {
                let next = self.tabs[at].agent.offered_mode(SessionModeChoice::Manual);
                let tab = &mut self.tabs[at];
                tab.mode = next;
                let number = tab.number;
                let default_moved = if not_started {
                    self.default_mode = SessionModeChoice::Manual;
                    true
                } else if self.default_mode == SessionModeChoice::Bypass {
                    self.default_mode = SessionModeChoice::Auto;
                    true
                } else {
                    false
                };
                eprintln!(
                    "[permission] mode is now {} (tab {number}{})",
                    next.as_str(),
                    if default_moved {
                        format!("; new sessions {}", self.default_mode.as_str())
                    } else {
                        String::new()
                    }
                );
                Ok(ModeCycle::Changed(next))
            }
            SessionModeChoice::Manual => {
                let tab = &mut self.tabs[at];
                if matches!(tab.wire_state(), TabStateWire::Ended | TabStateWire::Failed) {
                    return Err("the session has ended".to_string());
                }
                tab.mode = SessionModeChoice::Auto;
                let number = tab.number;
                if not_started {
                    self.default_mode = SessionModeChoice::Auto;
                }
                eprintln!(
                    "[permission] mode is now auto (tab {number}{})",
                    if not_started { "; new sessions too" } else { "" }
                );
                Ok(ModeCycle::Changed(SessionModeChoice::Auto))
            }
            SessionModeChoice::Auto => {
                let tab = &self.tabs[at];
                match tab.wire_state() {
                    TabStateWire::Ended | TabStateWire::Failed => Err("the session has ended".to_string()),
                    _ if !self.is_shown(id) => Err(format!("tab {} is not the one on screen", tab.number)),
                    _ => {
                        let scope_kind = if not_started {
                            tabs::PromptScope::EmptyTab
                        } else {
                            tabs::PromptScope::LiveTab
                        };
                        let approve = waiting_cards(tab);
                        let shown_in = tab.view;
                        let nonce = self.next_nonce;
                        self.next_nonce += 1;
                        let plan = BypassPlan {
                            scope: BypassScope::Tab(id),
                            nonce,
                            lines: vec![tabs::bypass_prompt(scope_kind, approve.len())],
                            approve,
                            prompt: scope_kind,
                            page: shown_in,
                        };
                        self.pending_bypass = Some(plan.clone());
                        Ok(ModeCycle::Confirm(plan))
                    }
                }
            }
        }
    }

    /// `Shift+Tab` on the chooser's `New session` row or a record, once the active tab is not itself
    /// `NotStarted`: the same cycle as `cycle_mode` (manual ->
    /// auto -> bypass -> manual, entering bypass asked first), applied to the window's remembered
    /// `default_mode` rather than any one tab's own mode. Every open tab keeps whatever mode it
    /// already has; a new tab of an agent that offers no manual mode starts in auto.
    pub fn cycle_default_mode(&mut self, page: ViewId) -> ModeCycle {
        self.supersede_prompts(page);
        match self.default_mode {
            SessionModeChoice::Bypass => {
                self.default_mode = SessionModeChoice::Manual;
                eprintln!("[permission] mode is now manual (new sessions)");
                ModeCycle::Changed(SessionModeChoice::Manual)
            }
            SessionModeChoice::Manual => {
                self.default_mode = SessionModeChoice::Auto;
                eprintln!("[permission] mode is now auto (new sessions)");
                ModeCycle::Changed(SessionModeChoice::Auto)
            }
            SessionModeChoice::Auto => {
                let nonce = self.next_nonce;
                self.next_nonce += 1;
                let plan = BypassPlan {
                    scope: BypassScope::Default,
                    nonce,
                    approve: Vec::new(),
                    lines: vec![tabs::bypass_prompt(tabs::PromptScope::Default, 0)],
                    prompt: tabs::PromptScope::Default,
                    page,
                };
                self.pending_bypass = Some(plan.clone());
                ModeCycle::Confirm(plan)
            }
        }
    }

    /// `y`/`Y` to a `confirm_bypass` prompt. Compares against the STORED plan
    /// before taking it: a mismatched scope or a stale nonce leaves the CURRENT prompt exactly as it
    /// was, so an answer to an old prompt can never cancel a newer, still-open one.
    pub fn confirm_bypass(&mut self, scope: BypassScope, nonce: u64, page: ViewId) -> Result<ConfirmOutcome, String> {
        match &self.pending_bypass {
            Some(plan) if plan.scope == scope && plan.nonce == nonce && plan.page == page => {}
            _ => return Err("that prompt is no longer current".to_string()),
        }
        if let BypassScope::Tab(tab) = scope {
            if self.moving.contains(&tab.0) {
                return Err(MOVING.to_string());
            }
        }
        let plan = self.pending_bypass.take().expect("checked above");
        match scope {
            BypassScope::Tab(id) => {
                let Some(at) = self.tabs.iter().position(|t| t.id == id) else {
                    return Err(format!("no tab {}", id.0));
                };
                if !self.may_answer(id, page) {
                    return Err(format!("tab {} is not shown in this page", self.tabs[at].number));
                }
                if !self.is_shown(id) {
                    return Err(format!("tab {} is not the one on screen", self.tabs[at].number));
                }
                if matches!(self.tabs[at].wire_state(), TabStateWire::Ended | TabStateWire::Failed) {
                    return Err("the session has ended".to_string());
                }
                if self.tabs[at].mode == SessionModeChoice::Bypass {
                    return Ok(ConfirmOutcome::AlreadyBypass);
                }
                // Bypass is entered only from auto: a prompt left over from before the tab moved
                // (to manual, by a path that did not drop it) is not a yes to leaving manual.
                if self.tabs[at].mode != SessionModeChoice::Auto {
                    return Err("that prompt is no longer current".to_string());
                }
                // Re-derive what `y` would approve NOW, not what the plan said when it was
                // built -- a card delivered while the prompt was up must never be silently approved.
                let now = waiting_cards(&self.tabs[at]);
                // And re-derive WHICH question it is: the tab can change kind under an open prompt
                // (a terminal handoff turns a live tab into `NotStarted`), and an empty tab's `y`
                // also moves the window default -- which a live tab's prompt never said (the only way
                // into bypass is a confirmed prompt or a confirmed window default).
                let scope_kind = if matches!(self.tabs[at].backend, TabBackend::NotStarted) {
                    tabs::PromptScope::EmptyTab
                } else {
                    tabs::PromptScope::LiveTab
                };
                if now.iter().any(|id| !plan.approve.contains(id)) || scope_kind != plan.prompt {
                    let fresh_nonce = self.next_nonce;
                    self.next_nonce += 1;
                    let fresh = BypassPlan {
                        scope,
                        nonce: fresh_nonce,
                        lines: vec![tabs::bypass_prompt(scope_kind, now.len())],
                        approve: now,
                        prompt: scope_kind,
                        page: plan.page,
                    };
                    self.pending_bypass = Some(fresh.clone());
                    return Ok(ConfirmOutcome::Reprompt(fresh));
                }
                // `now` is already a subset of `plan.approve` (checked above), so it IS the
                // intersection a stale prompt needs -- nothing besides a still-pending, still-delivered card
                // is ever approved.
                let tab = &mut self.tabs[at];
                tab.mode = SessionModeChoice::Bypass;
                if matches!(tab.backend, TabBackend::NotStarted) {
                    // Disjoint fields of `self` (`tabs` vs `default_mode`): `tab` stays borrowed.
                    self.default_mode = SessionModeChoice::Bypass;
                }
                let answered = match tab.live_mut() {
                    Some(backend) => backend.approve_pending(&now, "on entering bypass"),
                    None => crate::agent_backend::Approved::default(),
                };
                tab.session.host_answered.extend(answered.ids.iter().cloned());
                // The CLI's own prompts it answered say so on their calls' rows once those complete.
                tab.session
                    .note_ledger
                    .record_report(NoteReport::prompts(answered.prompts));
                let answered_set: BTreeSet<String> = answered.ids.iter().cloned().collect();
                tab.attention.retain_pending(|id| !answered_set.contains(id));
                let approved = answered.ids.len();
                let number = tab.number;
                eprintln!("[permission] mode is now bypass (tab {number}, approved {approved} waiting)");
                Ok(ConfirmOutcome::Entered { approved })
            }
            BypassScope::Default => {
                self.default_mode = SessionModeChoice::Bypass;
                eprintln!("[permission] mode is now bypass (new sessions)");
                Ok(ConfirmOutcome::Entered { approved: 0 })
            }
        }
    }

    /// The user's own answer to one card (`permission_response` from the panel): the one route a
    /// human decision takes, so the tab can remember which calls the user approved.
    ///
    /// An Approve that reached the provider records the call -- tool-use id, tool and input, as the
    /// card showed them -- in `human_allowed`, and the CLI's own prompt for exactly that call (it
    /// follows the gate's request, with a new permission id) is then answered by the pump without a
    /// second card, once. A Deny, an answer the provider refused, and a card with no tool-use id
    /// record nothing. The id is read before answering, in
    /// its own statement (the lock-order rule this module inherits).
    ///
    /// Errors as the panel's own path always did: `no active session` (benign) when the tab is
    /// gone or holds no live backend. An answer sent by a chat page that does not show the tab (the
    /// tab moved to another view after the card was drawn there) is refused before anything is read
    /// or answered, and not benign: the card stays waiting and the agent hears nothing.
    pub fn answer_card(
        &mut self,
        id: TabId,
        page: ViewId,
        permission_id: &str,
        decision: agent::PermissionDecision,
    ) -> Result<(), BackendError> {
        let no_session = || BackendError {
            message: "no active session".to_string(),
            benign: true,
        };
        // A card of a tab being moved is answered by no one here: it waits for the window the tab
        // lands in, or for this one again if the move does not happen.
        if self.moving.contains(&id.0) {
            return Err(BackendError {
                message: MOVING.to_string(),
                benign: true,
            });
        }
        if let Some(tab) = self.get(id) {
            if !self.may_answer(id, page) {
                return Err(BackendError {
                    message: format!("tab {} is not shown in this page", tab.number),
                    benign: false,
                });
            }
        }
        let tab = self.get_mut(id).ok_or_else(no_session)?;
        let backend = tab.live_mut().ok_or_else(no_session)?;
        // The call exactly as the card showed it: id, tool and input, so the approval answers that call only.
        let call: Option<(String, String, serde_json::Value)> = backend
            .projection()
            .pending_permissions
            .get(permission_id)
            .and_then(|p| {
                let id = p.tool_use_id.clone().filter(|id| !id.is_empty())?;
                Some((id, p.tool_name.clone(), p.input.clone()))
            });
        let approves = decision.allows();
        backend.respond_permission(permission_id, decision)?;
        if approves {
            if let Some((tool_use_id, tool_name, input)) = call {
                tab.session.human_allowed.record(&tool_use_id, &tool_name, &input);
            }
        }
        // The card is answered, whatever the decision, so its rule
        // offer goes now and is never made again (`user_answered`'s doc). Only on success: a refused
        // answer leaves the card open and its offer with it.
        tab.session.rule_offers.remove(permission_id);
        tab.session.user_answered.insert(permission_id.to_string());
        Ok(())
    }

    /// Answers Claude's question set `interaction_id` in tab `id` from `page`
    /// (`AgentBackend::answer_question`): the panel's `question_answer`, after admission. The page is
    /// checked again here, as `answer_card` does, so no later change to admission can skip it, and a
    /// tab being moved answers nothing: its question waits for the window it lands in.
    pub fn answer_question(
        &mut self,
        id: TabId,
        page: ViewId,
        interaction_id: &str,
        answers: Vec<agent::QuestionAnswer>,
    ) -> Result<QuestionSend, BackendError> {
        self.question_backend(id, page)?
            .answer_question(interaction_id, answers)
    }

    /// Declines Claude's question set `interaction_id` in tab `id` from `page`
    /// (`AgentBackend::decline_question`), under the same checks as [`Self::answer_question`].
    pub fn decline_question(
        &mut self,
        id: TabId,
        page: ViewId,
        interaction_id: &str,
        kind: agent::QuestionDeclineKind,
        so_far: Vec<agent::QuestionAnswer>,
    ) -> Result<QuestionSend, BackendError> {
        self.question_backend(id, page)?
            .decline_question(interaction_id, kind, so_far)
    }

    /// What became of `send`, an answer sent from tab `id` (`AgentBackend::finish_question_send`):
    /// `None` while it is still on its way. A refusal or failure puts the question back to waiting in
    /// the tab's session; a tab closed meanwhile has nothing to put back, and its reply is still read
    /// so the page that answered is told.
    pub fn finish_question_send(&mut self, id: TabId, send: &QuestionSend) -> Option<Result<(), BackendError>> {
        match self.get_mut(id).and_then(|tab| tab.live_mut()) {
            Some(backend) => backend.finish_question_send(send),
            None => send.reply(),
        }
    }

    /// The live session of tab `id`, when `page` may answer its question now.
    fn question_backend(&mut self, id: TabId, page: ViewId) -> Result<&mut AgentBackend, BackendError> {
        if self.moving.contains(&id.0) {
            return Err(BackendError {
                message: MOVING.to_string(),
                benign: true,
            });
        }
        if let Some(tab) = self.get(id) {
            // Benign: the session is fine; a page asked about a tab it does not show.
            if !self.may_answer(id, page) {
                return Err(BackendError {
                    message: format!("tab {} is not shown in this page", tab.number),
                    benign: true,
                });
            }
        }
        let no_session = || BackendError {
            message: "no active session".to_string(),
            benign: true,
        };
        self.get_mut(id)
            .ok_or_else(no_session)?
            .live_mut()
            .ok_or_else(no_session)
    }

    /// Clears every prompt of the window: the bypass prompt, the restore question
    /// and every tab's trust question, in every chat page. Only for the whole window losing the keys;
    /// a switch within one view drops that view's prompts ([`TabSet::drop_bypass_prompt_in`]).
    pub fn drop_bypass_prompt(&mut self) {
        self.pending_bypass = None;
        self.pending_restore = None;
        for tab in &mut self.tabs {
            if let TabBackend::AwaitingTrust(waiting) = &mut tab.backend {
                waiting.asking = None;
            }
        }
    }

    /// Clears the prompts chat page `page` shows: the bypass prompt if it is that page's, the restore
    /// question if `page` is the base page (the only one that asks it), and the trust questions of
    /// the tabs in that view. Called when the page's view switches tabs or the page loses the keys,
    /// so a stale `y` typed there can never land on a prompt about a tab the user is no longer
    /// looking at; another page's prompts stay.
    pub fn drop_bypass_prompt_in(&mut self, page: ViewId) {
        if self.pending_bypass.as_ref().is_some_and(|plan| plan.page == page) {
            self.pending_bypass = None;
        }
        if page.is_base() {
            self.pending_restore = None;
        }
        self.drop_trust_questions_in(page);
    }

    /// A fresh press of the mode key in `page`: the window's one bypass prompt is superseded, whichever
    /// page showed it, and `page`'s own questions go.
    fn supersede_prompts(&mut self, page: ViewId) {
        self.pending_bypass = None;
        if page.is_base() {
            self.pending_restore = None;
        }
        self.drop_trust_questions_in(page);
    }

    /// Takes the question off every tab of `view` that waits for the trust answer: the tab keeps its
    /// start and is asked again, with a fresh nonce, when it is next on screen. An answer to the old
    /// nonce is refused from here on. A tab in another view keeps its question: it is still shown.
    fn drop_trust_questions_in(&mut self, view: ViewId) {
        for tab in self.tabs.iter_mut().filter(|t| t.view == view) {
            if let TabBackend::AwaitingTrust(waiting) = &mut tab.backend {
                waiting.asking = None;
            }
        }
    }

    /// Puts a start off until the trust question is answered. Only an empty tab can wait: a tab with
    /// a session, one that is connecting and one already waiting each have their own start.
    /// A tab frozen for a move starts nothing here: its record is already taken.
    pub fn defer_start(&mut self, tab: TabId, start: DeferredStart) -> Result<(), String> {
        if self.moving.contains(&tab.0) {
            return Err(MOVING.to_string());
        }
        let t = self.get_mut(tab).ok_or_else(|| format!("no tab {}", tab.0))?;
        match t.backend {
            TabBackend::NotStarted => {}
            TabBackend::AwaitingTrust(_) => return Err("this tab is already waiting for the trust question".into()),
            _ => return Err("only an empty tab can wait for the trust question".into()),
        }
        t.backend = TabBackend::AwaitingTrust(AwaitingTrust {
            start,
            asking: None,
            recording: false,
        });
        Ok(())
    }

    /// Opens a question for `tab` under a fresh nonce, replacing any earlier one. It also ends a
    /// `recording` wait: a record that could not be written asks the tab again.
    pub fn ask_trust(&mut self, tab: TabId) -> Result<u64, String> {
        let nonce = self.next_nonce;
        let t = self.get_mut(tab).ok_or_else(|| format!("no tab {}", tab.0))?;
        let TabBackend::AwaitingTrust(waiting) = &mut t.backend else {
            return Err("this tab is not waiting for the trust question".into());
        };
        waiting.asking = Some(nonce);
        waiting.recording = false;
        self.next_nonce += 1;
        Ok(nonce)
    }

    /// Whether an answer carrying `nonce` may be taken for `tab` now, sent by chat page `page`: the
    /// tab is in that page's view and on screen there, waiting, not already recording, and the
    /// question it shows is this one. Changes nothing.
    /// A tab frozen for a move takes no answer: its question is asked again where it lands.
    pub fn check_trust_answer(&self, tab: TabId, nonce: u64, page: ViewId) -> Result<(), String> {
        let t = self.get(tab).ok_or_else(|| format!("no tab {}", tab.0))?;
        if self.moving.contains(&tab.0) {
            return Err(MOVING.to_string());
        }
        if !self.may_answer(tab, page) {
            return Err(format!("tab {} is not shown in this page", t.number));
        }
        if !self.is_shown(tab) {
            return Err(format!("tab {} is not the one on screen", t.number));
        }
        let TabBackend::AwaitingTrust(waiting) = &t.backend else {
            return Err("this tab is not waiting for the trust question".into());
        };
        if waiting.recording {
            return Err("the answer is already being recorded".into());
        }
        if waiting.asking != Some(nonce) {
            return Err("that question is no longer current".into());
        }
        Ok(())
    }

    /// An accepted `y`: the question is spent and the tab keeps its start until the worker's second
    /// look at the disk (and the record, when one is kept) decides what happens to it.
    pub fn mark_recording(&mut self, tab: TabId, nonce: u64, page: ViewId) -> Result<(), String> {
        self.check_trust_answer(tab, nonce, page)?;
        if let Some(Tab {
            backend: TabBackend::AwaitingTrust(waiting),
            ..
        }) = self.get_mut(tab)
        {
            waiting.asking = None;
            waiting.recording = true;
        }
        Ok(())
    }

    /// An answer taken at once (an `n`, which loads nothing): hands the start over and returns the tab
    /// to empty.
    pub fn take_answered(&mut self, tab: TabId, nonce: u64, page: ViewId) -> Result<DeferredStart, String> {
        self.check_trust_answer(tab, nonce, page)?;
        Ok(self.take_start(tab).expect("checked to be waiting"))
    }

    /// The start whose `y` the worker confirmed, even when the user has since moved to another tab.
    pub fn take_recorded(&mut self, tab: TabId) -> Option<DeferredStart> {
        if self.moving.contains(&tab.0) {
            return None;
        }
        match &self.get(tab)?.backend {
            TabBackend::AwaitingTrust(AwaitingTrust { recording: true, .. }) => self.take_start(tab),
            _ => None,
        }
    }

    /// The route that asks nothing (the answer is already known): hands the start over. Refused
    /// while a record is being written, whose result decides.
    /// Nothing is handed over for a tab frozen for a move: it starts where it lands.
    pub fn take_deferred(&mut self, tab: TabId) -> Option<DeferredStart> {
        if self.moving.contains(&tab.0) {
            return None;
        }
        match &self.get(tab)?.backend {
            TabBackend::AwaitingTrust(AwaitingTrust { recording: false, .. }) => self.take_start(tab),
            _ => None,
        }
    }

    /// `Escape` on the question: puts the start off and returns the tab to empty. The same checks as
    /// an answer.
    pub fn cancel_trust(&mut self, tab: TabId, nonce: u64, page: ViewId) -> Result<DeferredStart, String> {
        self.check_trust_answer(tab, nonce, page)?;
        Ok(self.take_start(tab).expect("checked to be waiting"))
    }

    /// The tabs that hold a start for the trust question, in tab order.
    pub fn awaiting_trust(&self) -> Vec<TabId> {
        self.tabs
            .iter()
            .filter(|t| matches!(t.backend, TabBackend::AwaitingTrust(_)))
            .map(|t| t.id)
            .collect()
    }

    /// A connect for `tab` begins: `pending` is what it reports to. A tab that holds a live session
    /// keeps it, and the connect is dropped (its worker then drops what it builds): nothing reaches
    /// here for a live tab, since a start begins from an empty tab or one waiting for the trust
    /// question.
    pub fn start(&mut self, tab: TabId, pending: PendingStart) {
        if self.moving.contains(&tab.0) {
            eprintln!("[tabs] tab {}: a start for a tab being moved was dropped", tab.0);
            return;
        }
        let Some(t) = self.get_mut(tab) else { return };
        if matches!(t.backend, TabBackend::Live(_)) {
            eprintln!(
                "[tabs] tab {}: a start for a tab with a live session was dropped",
                tab.0
            );
            return;
        }
        t.backend = TabBackend::Starting(pending);
    }

    fn take_start(&mut self, tab: TabId) -> Option<DeferredStart> {
        let t = self.get_mut(tab)?;
        match std::mem::replace(&mut t.backend, TabBackend::NotStarted) {
            TabBackend::AwaitingTrust(waiting) => Some(waiting.start),
            other => {
                t.backend = other;
                None
            }
        }
    }

    /// [`Tab::resumes_on_r`] of tab `id`: the session a reset of it continues in place, if any.
    pub fn resumes_on_r(&self, id: TabId) -> Option<String> {
        self.get(id).and_then(|t| t.resumes_on_r(t.resumable(self.kind)))
    }

    /// `r`: an ended or failed tab back to empty, keeping its number, name and mode.
    /// A live backend comes back retired, for the caller to shut down and report.
    pub fn reset(&mut self, id: TabId) -> Result<Option<Retired>, String> {
        if self.moving.contains(&id.0) {
            return Err(MOVING.to_string());
        }
        let tab = self.get_mut(id).ok_or_else(|| format!("no tab {}", id.0))?;
        let waiting = matches!(tab.backend, TabBackend::AwaitingTrust(_));
        match tab.wire_state() {
            TabStateWire::Ended | TabStateWire::Failed => {}
            // A tab waiting for the trust question starts over too: its put-off start is dropped.
            _ if waiting => {}
            _ => return Err("only an ended or failed tab starts over".to_string()),
        }
        // The queue and the unsent draft come back as the draft, oldest first. A first
        // message that was put off comes back with them, ahead of the rest: it was typed first.
        let mut parts: Vec<String> = Vec::new();
        if let TabBackend::AwaitingTrust(AwaitingTrust {
            start: DeferredStart::FirstSend { first_turn, .. },
            ..
        }) = &tab.backend
        {
            parts.push(first_turn.typed.clone());
        }
        parts.extend(std::mem::take(&mut tab.session.queue).into_iter().map(|q| q.text));
        if !tab.session.draft.trim().is_empty() {
            parts.push(tab.draft().to_string());
        }
        let old = std::mem::replace(&mut tab.backend, TabBackend::NotStarted);
        tab.session = SessionState::default();
        tab.replace_draft(parts.join("\n\n"));
        tab.attention.restart();
        self.pending_bypass = None;
        Ok(match old {
            TabBackend::Live(backend) => Some(self.retire(id, backend)),
            _ => None,
        })
    }

    /// A command that names no tab, or a tab that is not here, is refused rather than run on another.
    pub fn resolve(&self, tab_ref: TabRef) -> Result<Option<TabId>, String> {
        match tab_ref {
            TabRef::WindowLevel => Ok(None),
            TabRef::Missing => Err("protocol: this command names no tab".to_string()),
            TabRef::Named(id) if self.get(id).is_some() => Ok(Some(id)),
            TabRef::Named(id) => Err(format!("protocol: no tab {}", id.0)),
        }
    }

    /// The tab holding `agent`'s session `provider_session_id`. Session ids are each agent's own, and two
    /// agents may well use the same one, so the agent is part of what is looked for.
    pub fn tab_with_session(&self, agent: &AgentId, provider_session_id: &str) -> Option<TabId> {
        self.tabs
            .iter()
            .find(|t| t.agent == *agent && t.provider_session_id().as_deref() == Some(provider_session_id))
            .map(|t| t.id)
    }

    /// Every session this window has open or just handed off, each with its agent, as session ids are
    /// each agent's own and two agents may use the same one: filtered out of `hello` and of the
    /// chooser's records, and left out of a restore. A handed-off session is Claude's, the only agent
    /// a terminal takes over.
    pub fn open_sessions(&self) -> Vec<(AgentId, String)> {
        let mut open: Vec<(AgentId, String)> = self
            .tabs
            .iter()
            .filter_map(|t| t.provider_session_id().map(|id| (t.agent.clone(), id)))
            .collect();
        open.extend(self.tabs.iter().filter_map(|t| {
            t.last_handoff
                .as_ref()
                .map(|c| (AgentId::Claude, c.provider_session_id().to_string()))
        }));
        open
    }

    pub fn route_resume(
        &mut self,
        from: TabId,
        agent: &AgentId,
        provider_session_id: &str,
        held_elsewhere: bool,
    ) -> ResumeRoute {
        self.route_resume_told(from, agent, provider_session_id, held_elsewhere, true)
    }

    /// [`TabSet::route_resume`]; `told` as for [`TabSet::open_tab`].
    fn route_resume_told(
        &mut self,
        from: TabId,
        agent: &AgentId,
        provider_session_id: &str,
        held_elsewhere: bool,
        told: bool,
    ) -> ResumeRoute {
        if let Some(open) = self.tab_with_session(agent, provider_session_id) {
            // A tab bound to an extra view is not selected (`select` refuses it): the caller gives
            // that view the keys instead.
            self.select(open);
            debug_assert!(self.v1_holds());
            return ResumeRoute::SwitchTo(open);
        }
        if held_elsewhere {
            return ResumeRoute::Refuse("open in another window".to_string());
        }
        let empty = self
            .get(from)
            .is_some_and(|t| matches!(t.backend, TabBackend::NotStarted));
        let target = if empty {
            from
        } else {
            let agent = self.get(from).map_or(AgentId::Claude, |t| t.agent.clone());
            self.open_tab(agent, told)
        };
        debug_assert!(self.v1_holds());
        ResumeRoute::StartIn(target)
    }

    /// [`TabSet::pump_views`] with every chat view mapped or none: `panel_mapped` for all of them.
    pub fn pump(&mut self, project_root: &Path, panel_mapped: bool) -> PumpOutput {
        self.pump_views(project_root, &|_| panel_mapped)
    }

    /// One tick over EVERY tab, run when something is due (at most one per 33 ms). Every
    /// tab on screen in some view has its delivery serialized for that view's page: the base view's
    /// active tab into `active_payload`, each tab bound to an extra view into `view_payloads`, mapped
    /// or not, so a page hidden for a while is still current when it comes back. A background
    /// base-view tab's feeds its attention and marks it stale, and nothing of it is sent. `mapped`
    /// says whether a view's page is on screen now: what finishes in a tab whose page is not is left
    /// unread.
    pub fn pump_views(&mut self, project_root: &Path, mapped: &dyn Fn(ViewId) -> bool) -> PumpOutput {
        let active = self.active;
        self.project_root = Some(project_root.to_path_buf());
        let mut out = match self.held_pump.take() {
            Some(held) => self.still_wanted(held),
            None => PumpOutput::empty(),
        };
        let owed: Vec<u64> = std::mem::take(&mut self.flush_owed).into_iter().collect();
        for id in owed {
            if self.get(TabId(id)).is_some_and(|t| !t.turn_running()) && !self.moving.contains(&id) {
                out.turn_ended.push(TabId(id));
            }
        }
        // A turn a send started since the last pump (a legacy turn begins this way) is seen here.
        self.sync_presence();
        // Before this tick's events: a base that landed by now is not pending for them.
        if let Some(review) = self.turn_review.as_mut() {
            out.review_hints = review.poll();
        }
        let mut review = self.turn_review.as_mut();
        let mut tick = PumpTick {
            project_root,
            rules: &self.rules,
            ui_wake: self.ui_wake.as_ref(),
            retiring: &mut self.retiring,
            starting: &mut self.starting,
            starting_since: &mut self.starting_since,
            held_sends: &self.pending_sends,
            untracked_starts: &mut self.untracked_starts,
            #[cfg(test)]
            after_drain: &mut self.after_drain,
            #[cfg(test)]
            redeliver_covered: self.redeliver_covered,
            #[cfg(test)]
            before_resync_snapshot: &mut self.before_resync_snapshot,
        };
        for tab in &mut self.tabs {
            // A tab frozen for a move delivers nothing more here: its last pump ran at the freeze, and
            // whatever it folds from now on is the other window's to deliver.
            if self.moving.contains(&tab.id.0) {
                continue;
            }
            let is_active = tab.id == active;
            // `TabSet::is_shown`, read off the tab itself: the set is borrowed by this loop.
            let in_a_page = is_active || !tab.view.is_base();
            let place = PumpPlace {
                is_active,
                in_a_page,
                on_screen: in_a_page && mapped(tab.view),
            };
            Self::pump_tab(tab, &mut tick, review.as_deref_mut(), place, &mut out);
        }
        for id in self.steer_waiting(wall_clock_ms()) {
            if !out.queue_changed.contains(&id) {
                out.queue_changed.push(id);
            }
        }
        // A start whose tab is gone or no longer holds a session is over.
        let tabs = &self.tabs;
        self.starting
            .retain(|id| tabs.iter().any(|t| t.id.0 == *id && t.live().is_some()));
        let starting = &self.starting;
        self.starting_since.retain(|id, _| starting.contains(id));
        // A `TurnStarted` folded in this pump.
        self.sync_presence();
        out
    }

    /// The CLI left its own auto mode by itself, on a change to a model that cannot run it: the tab
    /// moves to manual at once, so what it shows is what it does. Eitri's own policy then answers every
    /// request, a file edit is a card, and nothing is left to a classifier that no longer runs. Only
    /// `Auto` moves: bypass and manual tabs never relied on that classifier. Once per move: a person may
    /// cycle back to auto, which on this session means Eitri's own policy, as on any CLI that fell back.
    fn follow_cli_out_of_auto(tab: &mut Tab) {
        let TabBackend::Live(backend) = &tab.backend else {
            return;
        };
        if !backend.cli_left_auto() {
            tab.session.followed_out_of_auto = false;
            return;
        }
        if std::mem::replace(&mut tab.session.followed_out_of_auto, true) {
            return;
        }
        if tab.mode == SessionModeChoice::Auto {
            tab.mode = SessionModeChoice::Manual;
            tab.session.left_auto_note_owed = true;
        }
    }

    /// One tab's share of [`TabSet::pump_views`]: drains its backend, fails it if its CLI reported a
    /// mode the gate cannot hold, hands the delivery to the arm that matches it, and settles what is
    /// left once that arm is done.
    fn pump_tab(
        tab: &mut Tab,
        tick: &mut PumpTick<'_>,
        mut review: Option<&mut crate::turn_review::TurnReview>,
        place: PumpPlace,
        out: &mut PumpOutput,
    ) {
        Self::follow_cli_out_of_auto(tab);
        let TabBackend::Live(backend) = &mut tab.backend else {
            if let Some(review) = review.as_deref_mut() {
                review.tab_has_no_session(tab.id.0, wall_clock_ms());
            }
            // No backend holds no card: the backstop `agent_panel` had for `session.is_none()`.
            tab.attention.retain_pending(|_| false);
            tab.session.forget_answers();
            return;
        };
        // A backend put in place some other way than `collect_starts` is told about the wake here.
        if let Some(wake) = tick.ui_wake {
            backend.ensure_ui_wake(wake);
        }
        // `tab.mode`: the classifier's `Auto` path, or bypass answering everything, and
        // `tab.host_answered` collects every id either path answers so the Resync arm and
        // `active_state_payloads` can hide them. `tab.human_allowed`: in `Auto`, the CLI's own prompt
        // for exactly a call the user approved on a card here is answered without a second card, once.
        // What was answered without a card becomes a note on its call's row when the call completes.
        let mut answered_for_you = crate::agent_backend::AnsweredForYou::default();
        let delivery = backend.take_revised_ui_delivery(
            tick.project_root,
            tick.rules,
            crate::agent_backend::GateMode {
                mode: tab.mode.into(),
                followed_out_of_auto: tab.session.followed_out_of_auto,
            },
            &mut tab.session.host_answered,
            &mut tab.session.human_allowed,
            &mut answered_for_you,
        );
        #[cfg(test)]
        {
            if let Some(hook) = tick.after_drain.take() {
                hook(backend);
            }
        }
        Self::follow_cli_out_of_auto(tab);
        let TabBackend::Live(backend) = &mut tab.backend else {
            return;
        };
        // What the last snapshot of this backend the panel was sent already carried: a snapshot and
        // the drain before it are separate lock acquisitions, and the sidecar's ingestion thread
        // folds and queues in between, so such an event is in that snapshot AND in this drain. Every
        // queued event carries the revision its own fold produced, and the fold, the queueing and
        // the snapshot's read of `last_revision` each happen under the one ingestion mutex: an event
        // tagged at or below the snapshot's revision is exactly one it carried, and all of them were
        // queued before it was read -- so this drain holds every one. Taken here, so nothing later
        // compares against it; kept by the backend (`AgentBackend::note_ui_snapshot`), so it never
        // outlives the session it was read from.
        let covered = backend.take_ui_snapshot_revision();
        #[cfg(test)]
        let covered = covered.filter(|_| !tick.redeliver_covered);
        Self::end_start_if_over(
            tab.id,
            backend,
            &delivery,
            tick.starting,
            tick.starting_since,
            tick.held_sends,
        );
        #[cfg(any(test, feature = "test-support"))]
        tab.gate_log.extend(answered_for_you.gate_answers);
        // What the fast path answered, what a saved rule answered and the CLI prompts answered
        // without a card, as recorded at the answer itself, wait in the ledger for their calls to
        // complete. Each `Write` answered without a card over no file is true already, and waits
        // only for a payload to carry it (`creates_file_unsent`).
        tab.session.note_ledger.record_report(NoteReport {
            prompts: answered_for_you.prompts,
            by_the_fast_path: answered_for_you.by_the_fast_path,
            by_rule: answered_for_you.by_rule,
        });
        for id in answered_for_you.creates_file {
            if tab.notes.auto_creates_file.insert(id.clone()) {
                tab.session.creates_file_unsent.insert(id);
            }
        }
        if let Some(reason) = Self::pump_tripped_reason(&delivery, backend) {
            Self::pump_trip(tab, tick.retiring, reason, place.on_screen, out);
            return;
        }
        // From the projection, never the delivery: a `Resync` loses the report that says so.
        if tab.session.foreign_server_note == ForeignServerNote::Unseen && backend.foreign_tool_server() {
            tab.session.foreign_server_note = ForeignServerNote::Owed;
        }
        // Read here, not by the worker that started the session: only the tab knows which page it is on.
        if let Some(line) = backend.editor_note() {
            tab.session.foreign_server_note = ForeignServerNote::ProviderOwed(line);
        }
        tab.session.client_calls.note(backend);
        // Keeps `tab.turn_clock` in step for the `Events` arm, which builds its payload straight
        // from the drained batch and takes no further lock after this -- there is no later,
        // fresher read to prefer. The `Resync` arm is different (see its own comment) and
        // re-observes the clock itself from the snapshot it actually sends, rather than reading
        // `tab.turn_clock` as this call left it.
        {
            let projection = backend.projection();
            observe_turn_clock(
                &mut tab.session.turn_clock,
                &mut tab.notes.turn_times,
                &mut tab.session.turn_times_unsent,
                &projection,
                wall_clock_ms(),
            );
        }
        let resynced = matches!(delivery, RevisedDelivery::Resync);
        let mut live = LiveTab {
            following: tab.following,
            id: tab.id,
            mode: tab.mode,
            backend,
            session: &mut tab.session,
            notes: &mut tab.notes,
            attention: &mut tab.attention,
        };
        let arm = match delivery {
            RevisedDelivery::Nothing => ArmOutcome::default(),
            RevisedDelivery::Events(tagged) => Self::pump_arm_events(
                &mut live,
                tagged,
                covered,
                tick.project_root,
                review.as_deref_mut(),
                place,
            ),
            RevisedDelivery::Resync => {
                let (turn_ended, pending) = Self::pump_arm_resync(&mut live);
                // A resync in bypass sweeps whatever the overflow dropped through the same
                // `approve_pending` a confirm uses, so a request never drawn as a card is never rebuilt
                // as one by the very mechanism meant to rebuild the panel honestly. This also hides
                // what the classifier already answered in `Auto`, closing the gap `attention`'s module
                // doc used to record for `resync` itself. Answered here, outside every guard of the
                // projection: answering a permission must happen outside the lock.
                if live.mode == SessionModeChoice::Bypass {
                    let sweep: Vec<String> = pending
                        .iter()
                        .filter(|id| !live.session.host_answered.contains(*id))
                        .cloned()
                        .collect();
                    // The events (legacy's own resolutions) are not needed here: the snapshot is read
                    // after they were folded, so it already shows those cards answered.
                    let answered = live.backend.approve_pending(&sweep, "in bypass after a resync");
                    live.session.host_answered.extend(answered.ids);
                    live.session
                        .note_ledger
                        .record_report(NoteReport::prompts(answered.prompts));
                }
                Self::pump_resync_show(&mut live, &pending, turn_ended, tick, place)
            }
        };
        if arm.untracked_start {
            tick.untracked_starts.insert(live.id.0);
        }
        let still = Self::pump_after_match(live.backend, live.id, review, resynced);
        tab.session.note_turn_began();
        Self::pump_settle(tab, arm, place, &still, tick.project_root, out);
    }

    /// The CLI-mode tripwire, before anything of this delivery reaches the panel or the attention
    /// count: from the batch itself, or -- for a report a `Resync` dropped, or one folded ahead of
    /// its delivery -- from the projection's record.
    fn pump_tripped_reason(delivery: &RevisedDelivery, backend: &AgentBackend) -> Option<String> {
        let in_batch = match delivery {
            RevisedDelivery::Events(events) => events.iter().find_map(|(_, event)| match event {
                AgentDomainEvent::UngatedCliMode { reported, detail } => {
                    Some(ungated_cli_mode_reason(reported, detail))
                }
                AgentDomainEvent::PolicyMismatch { requested, reported } => {
                    Some(policy_mismatch_reason(requested, reported))
                }
                _ => None,
            }),
            _ => None,
        };
        in_batch.or_else(|| {
            let projection = backend.projection();
            if let Some(r) = &projection.ungated_cli_mode {
                return Some(ungated_cli_mode_reason(&r.reported, &r.detail));
            }
            projection
                .policy_mismatch
                .as_ref()
                .map(|(requested, reported)| policy_mismatch_reason(requested, reported))
        })
    }

    /// Fails a tab the tripwire caught. The whole batch is withheld, so no card in it is ever drawn
    /// to be approved; the session is handed out to be shut down, which denies what is still
    /// pending in it.
    fn pump_trip(tab: &mut Tab, retiring: &mut RetiringCounts, reason: String, on_screen: bool, out: &mut PumpOutput) {
        eprintln!("[permission] tab {}: {reason}", tab.id.0);
        // Nothing will report on a message still in the turn once the session goes.
        if tab.hold_steered_on_failure() {
            out.queue_changed.push(tab.id);
        }
        tab.attention.session_ended(on_screen);
        tab.session.forget_answers();
        if let Some(retired) = tab.fail_live(retiring, reason.clone(), true, None) {
            out.tripped.push(Tripped {
                tab: tab.id,
                reason,
                retired,
            });
        }
    }

    /// The `Events` arm: folds the drained batch into the tab's own bookkeeping and, for a tab a page
    /// shows, builds the payload of what the last snapshot did not already carry.
    fn pump_arm_events(
        live: &mut LiveTab<'_>,
        tagged: Vec<(u64, AgentDomainEvent)>,
        covered: Option<u64>,
        project_root: &Path,
        review: Option<&mut crate::turn_review::TurnReview>,
        place: PumpPlace,
    ) -> ArmOutcome {
        let mut arm = ArmOutcome::default();
        // The tags strictly increase, so what the last snapshot already carried is a prefix of the
        // batch: `events[..shown]` reached the panel inside that snapshot, `events[shown..]` has not
        // reached it at all. Everything below folds the whole batch into this tab's own bookkeeping
        // -- none of it has been seen here yet -- and only the payload leaves the prefix out.
        let shown = covered.map_or(0, |through| {
            tagged.partition_point(|(revision, _)| *revision <= through)
        });
        // The envelope's bracket, from the events' own fold revisions: the revision before the first
        // one it carries, and that of the batch's last.
        let through_revision = tagged.last().map_or(0, |(revision, _)| *revision);
        let from_revision = tagged
            .get(shown)
            .map_or(through_revision, |(revision, _)| revision.saturating_sub(1));
        let (revisions, events): (Vec<u64>, Vec<AgentDomainEvent>) = tagged.into_iter().unzip();
        let (in_snapshot, fresh) = events.split_at(shown);
        if live.following {
            for event in &events {
                let mut event = event;
                while let AgentDomainEvent::SubagentFrame { frame, .. } = event {
                    event = frame.as_ref();
                }
                if let AgentDomainEvent::ToolCallCompleted {
                    tool_use_id,
                    is_error: false,
                    ..
                } = event
                {
                    // Release the projection's guard before touching any other backend state.
                    let paths = live.backend.edited_by(tool_use_id);
                    live.session.client_calls.edited.extend(paths);
                }
            }
        }
        for event in &events {
            match event {
                AgentDomainEvent::TurnStarted { .. } => live.session.was_running = true,
                AgentDomainEvent::CliWorkUnseen => live.session.was_unseen_busy = true,
                // A completion that came with no turn of the backend's own open ends none of the panel's:
                // a turn running here runs on, with the calls the user approved in it, which the CLI may
                // still ask about. It ends the work the panel could not show, which a held message
                // waits for, so it is reported as a turn's end where no turn runs.
                AgentDomainEvent::TurnCompleted { turn_id, .. } if turn_id == agent::NO_OPEN_TURN_ID => {
                    live.session.was_unseen_busy = false;
                    arm.turn_ended = true;
                }
                // An interrupt's end and the legacy backend's synthesized end are `TurnCompleted`s
                // too. Reported whatever `was_running` says: a legacy `TurnStarted` is returned by
                // `send_turn` and never pumped.
                AgentDomainEvent::TurnCompleted { .. } => {
                    live.session.was_running = false;
                    live.session.was_unseen_busy = false;
                    arm.turn_ended = true;
                    // The CLI asks within the turn that made the call.
                    live.session.human_allowed.clear();
                }
                // The session ended: its queue waits for `r`, not a flush.
                AgentDomainEvent::SessionClosed { .. } | AgentDomainEvent::SessionUnavailable { .. } => {
                    live.session.was_running = false;
                    live.session.was_unseen_busy = false;
                }
                // `Tab::mode` is the only authority: nothing a provider reports moves it.
                // A `PermissionModeChanged` that got this far named `default` (anything else became
                // `UngatedCliMode`, handled by the tripwire), so there is nothing to do with it.
                //
                // A resolution for an id this pump (or an earlier one) already answered on the
                // user's behalf: forgotten now, since there is nothing left to hide it from.
                AgentDomainEvent::PermissionResolved { permission_id, .. } => {
                    live.session.host_answered.remove(permission_id);
                }
                _ => {}
            }
        }
        arm.untracked_start = review.is_none()
            && events
                .iter()
                .any(|event| matches!(event, AgentDomainEvent::TurnStarted { .. }));
        // The whole batch, what the snapshot already carried included: the review has seen neither
        // part. Only enqueues.
        if let Some(review) = review {
            let session = live.backend.provider_session_id().unwrap_or_default();
            let now = wall_clock_ms();
            for event in &events {
                review.observe(live.id.0, &session, event, now);
            }
        }
        let mut fresh_notes = CallNotes {
            creates_file: note_new_files(&mut live.notes.creates_file, &events, project_root),
            auto_creates_file: std::mem::take(&mut live.session.creates_file_unsent),
            // Only a shown tab's payload leaves here; a background tab's turns wait for the snapshot
            // its next switch sends.
            turn_times: if place.in_a_page {
                let times = &live.notes.turn_times;
                let fresh = times[times.len() - live.session.turn_times_unsent.min(times.len())..].to_vec();
                live.session.turn_times_unsent = 0;
                fresh
            } else {
                Vec::new()
            },
            ..CallNotes::default()
        };
        live.session.note_ledger.fold(&events, live.notes, &mut fresh_notes);
        // Every card in the batch is counted, those the snapshot drew included: they are pending all
        // the same, and this tab has not counted them yet.
        live.attention.observe(&events, place.on_screen);
        if let Some(trace) = live.session.turn_trace.as_mut() {
            if !in_snapshot.is_empty() {
                trace.observe_in_snapshot(in_snapshot);
            }
            let first_text = trace.observe(fresh);
            if place.in_a_page {
                arm.saw_first_text = first_text;
            } else if first_text {
                trace.mark_background();
            }
        }
        if place.in_a_page {
            // Nothing to send when the snapshot carried the whole batch -- unless the batch produced
            // notes, which that snapshot (serialized before this tab had folded these events) did not
            // carry; the panel applies notes to what it already holds (`applyCallNotes`), so they go
            // on their own.
            if !fresh.is_empty() || !fresh_notes.is_empty() {
                arm.delivered = Some((
                    serialize_events_with_notes_and_revisions_for_js(
                        live.id,
                        from_revision,
                        through_revision,
                        fresh,
                        &fresh_notes,
                        Some(&revisions[shown..]),
                    ),
                    crate::panel_cadence::classify_events(fresh, !fresh_notes.is_empty()),
                ));
            }
        } else {
            live.session.stale = true;
        }
        arm
    }

    /// The `Resync` arm up to its bypass sweep: the events are gone, and the snapshot is what the
    /// panel is shown now. Returns whether the tab's turn ended and the ids of the cards pending.
    ///
    /// The arm drains (`take_revised_ui_delivery`, before it) and then reads a fresh
    /// `SnapshotView::of` (in [`TabSet::pump_resync_show`], for a tab a page shows) as two separate
    /// lock acquisitions on `Mutex<IngestState>`, with this read and `approve_pending`'s own lock
    /// cycle for a bypass tab in between (answering a permission must happen outside the lock). A
    /// batch folded in that gap is queued for the next tick AND already in the snapshot sent: the
    /// snapshot's revision, read under the guard it is serialized from, is noted on the backend, and
    /// the next drain leaves that batch out of the payload, exactly as after `active_state_payloads`'
    /// snapshot.
    fn pump_arm_resync(live: &mut LiveTab<'_>) -> (bool, Vec<String>) {
        // Everything the arm reads before answering, from one read of the projection, so the turn it
        // sees running, the notes the ledger settles and the cards still pending agree.
        let (turn_ended, pending) = {
            let projection = live.backend.projection();
            let running = projection.active_turn_id.is_some();
            // The end of work this panel never saw start counts too: a message held for it goes then.
            let turn_ended =
                (live.session.was_running && !running) || (live.session.was_unseen_busy && !projection.cli_unseen_busy);
            live.session.was_running = running;
            live.session.was_unseen_busy = projection.cli_unseen_busy;
            // Unconditionally: the dropped events may have held a turn's end and the next one's
            // start, which `was_running` cannot see, and an approval must never outlive its turn.
            // Costs at most a card.
            live.session.human_allowed.clear();
            // The ledger stands in for the dropped events: a candidate whose call finished among them
            // becomes its note now, so the snapshot already carries it, and a gate candidate whose
            // call has a card still pending is dropped (`NoteLedger::resync`).
            live.session
                .note_ledger
                .resync(&projection, &live.session.host_answered, live.notes);
            (turn_ended, projection.pending_permissions.keys().cloned().collect())
        };
        // The snapshot carries every note.
        live.session.creates_file_unsent.clear();
        (turn_ended, pending)
    }

    /// The `Resync` arm after its bypass sweep: counts the cards still to be answered and, for a tab
    /// a page shows, sends the snapshot.
    fn pump_resync_show(
        live: &mut LiveTab<'_>,
        pending: &[String],
        turn_ended: bool,
        tick: &mut PumpTick<'_>,
        place: PumpPlace,
    ) -> ArmOutcome {
        let mut arm = ArmOutcome {
            turn_ended,
            ..ArmOutcome::default()
        };
        live.attention.resync(
            pending
                .iter()
                .filter(|id| !live.session.host_answered.contains(*id))
                .cloned(),
        );
        if !place.in_a_page {
            live.session.stale = true;
            return arm;
        }
        #[cfg(test)]
        {
            if let Some(hook) = tick.before_resync_snapshot.take() {
                hook(live.backend);
            }
        }
        #[cfg(not(test))]
        let _ = tick;
        let (payload, revision) = {
            let view = SnapshotView::of(live.backend, &live.session.host_answered);
            // Observed from this snapshot's own view, under the guard it is serialized from -- not
            // the early read before the arm, which the arm's own further lock cycles (its read of the
            // projection and `approve_pending`'s bypass sweep) can leave stale: if a turn ends and the
            // next starts in that gap, the early read still names the first turn while this snapshot
            // already names the second, and the panel takes the stamped time as exact (as
            // `active_state_payloads` does, whose own snapshot has the identical shape).
            observe_turn_clock(
                &mut live.session.turn_clock,
                &mut live.notes.turn_times,
                &mut live.session.turn_times_unsent,
                &view.projection,
                wall_clock_ms(),
            );
            let payload = serialize_snapshot_with_notes_for_js(
                live.id,
                &view,
                live.session.turn_clock.as_ref().map(|(_, at)| *at),
                live.notes,
                &live.session.editor_cards,
            );
            live.session.turn_times_unsent = 0;
            // Under the guard the snapshot is serialized from (see `pump_arm_resync`'s doc).
            (payload, view.projection.last_revision)
        };
        live.backend.note_ui_snapshot(revision);
        // A snapshot replaces the panel's state: never held behind a cadence slot.
        arm.delivered = Some((payload, crate::panel_cadence::EnvelopeClass::Immediate));
        arm
    }

    /// What a tab's tick reads after its arm, from one read of the projection: the cards still
    /// pending and, after a resync, what the turn review learns from the rebuilt projection that the
    /// dropped events would have told it. Returns the pending ids.
    fn pump_after_match(
        backend: &AgentBackend,
        tab: TabId,
        review: Option<&mut crate::turn_review::TurnReview>,
        resynced: bool,
    ) -> std::collections::HashSet<String> {
        let review = review.filter(|_| resynced);
        // Its own statement, before the read below: it takes the same lock.
        let session = review
            .as_ref()
            .map(|_| backend.provider_session_id().unwrap_or_default());
        let (still, running, background, at_end) = {
            let projection = backend.projection();
            (
                projection.pending_permissions.keys().cloned().collect(),
                projection.active_turn_id.clone(),
                projection.background_calls,
                projection.background_at_last_turn_end,
            )
        };
        if let (Some(review), Some(session)) = (review, session) {
            let now = wall_clock_ms();
            // A turn the resync ends ended with the count it had then, not with the one now: the
            // command may have finished among the dropped events.
            let count = |running| AgentDomainEvent::BackgroundCalls { running };
            review.observe(tab.0, &session, &count(at_end), now);
            review.observe_resync(tab.0, &session, running.as_deref(), now);
            review.observe(tab.0, &session, &count(background), now);
        }
        still
    }

    /// The end of a tab's tick: hands its payload to the page that shows it, prints a trace nothing
    /// will print, forgets the cards that are no longer pending, and reports offers, steered messages
    /// and a turn's end.
    fn pump_settle(
        tab: &mut Tab,
        arm: ArmOutcome,
        place: PumpPlace,
        still: &std::collections::HashSet<String>,
        project_root: &Path,
        out: &mut PumpOutput,
    ) {
        // Whether this tab's payload goes to the panel this tick (then `shell` prints a complete
        // trace after dispatching it; otherwise this function does, below).
        let dispatched = arm.delivered.is_some();
        if place.is_active {
            out.first_text |= arm.saw_first_text;
            if let Some((payload, class)) = arm.delivered {
                if out.active_payload.is_some() {
                    // A move's last pump already left this page a payload, which goes first: this one
                    // follows it, addressed to the same page.
                    out.view_payloads.push(ShownPayload {
                        view: ViewId::BASE,
                        tab: tab.id,
                        payload,
                        class,
                        first_text: arm.saw_first_text,
                    });
                } else {
                    out.active_payload = Some(payload);
                    out.active_class = class;
                }
            }
        } else if let Some((payload, class)) = arm.delivered {
            out.view_payloads.push(ShownPayload {
                view: tab.view,
                tab: tab.id,
                payload,
                class,
                first_text: arm.saw_first_text,
            });
        } else if arm.saw_first_text {
            // No payload carries it, so no page will stamp its paint.
            if let Some(trace) = tab.session.turn_trace.as_mut() {
                trace.mark_background();
            }
        }
        // A trace with no dispatch to wait for is printed here once it finishes: a background
        // tab's, and the active tab's when nothing of it reached the panel this tick -- its end
        // may have arrived only inside a snapshot. A dispatched one is printed
        // by `shell`, after it stamps the dispatch.
        if !dispatched {
            if let Some(trace) = tab.session.turn_trace.as_mut() {
                if trace.is_complete() {
                    trace.emit();
                }
            }
        }
        if let Some(backend) = tab.live() {
            let waiting: Vec<String> = backend
                .projection()
                .waiting_questions()
                .map(|q| q.interaction_id.clone())
                .collect();
            tab.session.question_draft.retain(|id, _| waiting.contains(id));
            tab.attention.sync_questions(waiting);
        } else {
            tab.session.question_draft.clear();
            tab.attention.sync_questions(Vec::new());
        }
        tab.attention.retain_pending(|id| still.contains(id));
        tab.session.host_answered.retain(|id| still.contains(id));
        tab.session.user_answered.retain(|id| still.contains(id));
        if refresh_offers(tab, project_root) {
            out.offers_changed.push(tab.id);
        }
        // What the agent did with the messages put into its turn, read off the projection this
        // delivery (or resync) left: delivered ones leave the queue as their rows arrive.
        if tab.settle_steered() {
            out.queue_changed.push(tab.id);
        }
        // `turn_running()` guards a turn that has already started again (a send that raced the
        // completion): the queue waits for that one's end instead.
        if arm.turn_ended && !tab.turn_running() {
            out.turn_ended.push(tab.id);
        }
    }

    /// Collects every connect that finished. An installed backend clears the tab's previous
    /// handoff and attention, takes the resumed title, and is told the tab's rename.
    pub fn collect_starts(&mut self) -> Vec<StartCollected> {
        let mut collected = Vec::new();
        let mut clamped: Vec<(AgentId, SessionModeChoice, u16)> = Vec::new();
        for tab in &mut self.tabs {
            let TabBackend::Starting(pending) = &tab.backend else {
                continue;
            };
            let result = match pending.result_rx.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => continue,
                Err(mpsc::TryRecvError::Disconnected) => Err(BackendError {
                    message: "the backend connect worker stopped without reporting a result".to_string(),
                    benign: false,
                }),
            };
            let TabBackend::Starting(pending) = std::mem::replace(&mut tab.backend, TabBackend::NotStarted) else {
                unreachable!("matched Starting above")
            };
            match result {
                Ok(mut backend) => {
                    // A rename given while it connected is the newer one and is written to the
                    // record; otherwise the record's own rename is the tab's (no write: the record
                    // already holds it).
                    if tab.name.is_some() {
                        backend.note_name(tab.name.clone());
                    } else {
                        tab.name = pending.resumed_name;
                    }
                    // What runs is what the tab says it runs, whichever way it got there, in a mode
                    // its agent offers.
                    tab.agent = backend.agent();
                    let wanted = tab.mode;
                    let number = tab.number;
                    let agent = tab.agent.clone();
                    tab.mode = agent.offered_mode(wanted);
                    if tab.mode != wanted {
                        clamped.push((agent, wanted, number));
                    }
                    if let Some(wake) = &self.ui_wake {
                        backend.ensure_ui_wake(wake);
                    }
                    tab.backend = TabBackend::Live(backend);
                    tab.session = std::mem::take(&mut tab.session).for_installed_backend();
                    tab.attention.restart();
                    tab.notes = CallNotes::default();
                    tab.last_handoff = None;
                    tab.session.title = pending.resumed_title;
                    // Only Claude's sessions have a transcript the CLI titles.
                    tab.session.title_from_record = tab.session.title.is_some() && tab.agent == AgentId::Claude;
                    collected.push(StartCollected::Installed {
                        tab: tab.id,
                        request_id: pending.request_id,
                        first_turn: pending.first_turn,
                    });
                }
                Err(error) => {
                    tab.failed_after_start = false;
                    // A refused resume leaves the session in the saved-tabs file, under the name it had.
                    tab.failed_resume = pending.resume.clone();
                    if tab.name.is_none() {
                        tab.name = pending.resumed_name;
                    }
                    tab.backend = TabBackend::Failed {
                        reason: error.message.clone(),
                    };
                    collected.push(StartCollected::Failed {
                        tab: tab.id,
                        request_id: pending.request_id,
                        error,
                    });
                }
            }
        }
        for (agent, wanted, number) in clamped {
            self.mode_offered_to(&agent, wanted, number);
        }
        collected
    }

    /// After a switch or on `ready`: the active tab's own state. Its snapshot if it has a backend, or
    /// its handoff card if it has one. Always sent, not only when stale: the page holds only the active tab's state.
    ///
    /// **Reads; never drains.** The sidecar's ingestion
    /// thread folds and queues on its own, so this snapshot can already carry events the tab's queue
    /// still holds -- whatever folded after the last tick's drain. Its revision, read under the guard
    /// it is serialized from, is noted on the backend (`AgentBackend::note_ui_snapshot`), and the next
    /// `pump` leaves every event tagged at or below it out of the panel's payload while still folding
    /// all of them into the tab's own bookkeeping: its turn's end, its attention, its notes, its trace.
    /// So nothing here moves the window's attention, which `shell` reports around the tick and a
    /// switch but not around a panel reload. The one cost, the same as before any of this: a request
    /// the policy will answer at the next tick, folded in the ~33 ms before a switch or a reload, is
    /// drawn as a card until its resolution arrives.
    pub fn active_state_payloads(&mut self) -> Vec<String> {
        self.state_payloads(self.active)
    }

    /// [`TabSet::active_state_payloads`] for any tab on screen in some view, for the page that shows
    /// it. Empty for a tab it does not have, and for a background base-view tab: no page shows that
    /// one, and its state goes out when it is selected.
    pub fn state_payloads(&mut self, tab: TabId) -> Vec<String> {
        if !self.is_shown(tab) {
            return Vec::new();
        }
        let tab = self.get_mut(tab).expect("a shown tab exists");
        tab.session.stale = false;
        let mut payloads = Vec::new();
        match &mut tab.backend {
            TabBackend::Live(backend) => {
                let (payload, revision) = {
                    let view = SnapshotView::of(backend, &tab.session.host_answered);
                    settle_opening(
                        &mut tab.session.opening_prompt,
                        &mut tab.session.queue,
                        &view.projection,
                    );
                    settle_against(
                        &tab.agent,
                        &mut tab.session.queue,
                        &mut tab.session.queue_note,
                        &view.projection,
                    );
                    // The clock follows the turn this very snapshot names: one that started since
                    // the last tick is stamped now, as that tick would have, and a clock left from
                    // the turn before it is never sent with this one's id (the panel trusts it as
                    // exact).
                    observe_turn_clock(
                        &mut tab.session.turn_clock,
                        &mut tab.notes.turn_times,
                        &mut tab.session.turn_times_unsent,
                        &view.projection,
                        wall_clock_ms(),
                    );
                    let payload = serialize_snapshot_with_notes_for_js(
                        tab.id,
                        &view,
                        tab.session.turn_clock.as_ref().map(|(_, at)| *at),
                        &tab.notes,
                        &tab.session.editor_cards,
                    );
                    tab.session.turn_times_unsent = 0;
                    (payload, view.projection.last_revision)
                };
                backend.note_ui_snapshot(revision);
                payloads.push(payload);
            }
            TabBackend::NotStarted => payloads.extend(
                tab.last_handoff
                    .iter()
                    .map(|command| serialize_handoff_for_js(tab.id, command)),
            ),
            _ => {}
        }
        payloads.push(crate::agent_bridge::serialize_queue_for_js(
            tab.id,
            &tab.session.queue,
            tab.session.queue_error.as_deref(),
            tab.queue_line().as_deref(),
            tab.steers(),
        ));
        payloads.push(crate::agent_bridge::serialize_draft_for_js(tab.id, &tab.session.draft));
        if tab.live().is_some() {
            payloads.push(crate::agent_bridge::serialize_rule_offers_for_js(
                tab.id,
                &tab.session.rule_offers,
            ));
        }
        payloads.push(crate::agent_bridge::serialize_scratch_for_js(
            tab.id,
            tab.editing_draft.is_some(),
        ));
        payloads
    }

    /// The base page's `tabs` envelope: [`TabSet::tabs_payload_for`] the base view.
    pub fn tabs_payload(&self) -> String {
        self.tabs_payload_for(ViewId::BASE)
    }

    /// The `tabs` envelope for `view`'s page. The base page lists the base view's tabs, with its
    /// active one; a tab bound elsewhere is left out, its number a gap as a closed tab's is. An extra
    /// view's page lists its one tab, as the active one, so a page with one tab shows no strip. A
    /// view that shows no tab gets an empty list naming no tab as active.
    pub fn tabs_payload_for(&self, view: ViewId) -> String {
        let listed: Vec<TabView> = self
            .tabs
            .iter()
            .filter(|t| t.view == view)
            .map(|t| t.view(t.resumable(self.kind)))
            .collect();
        let active = if view.is_base() {
            self.active
        } else {
            // Ids start at 1, so 0 names no tab.
            self.tab_in(view).unwrap_or(TabId(0))
        };
        serialize_tabs_for_js(active, &listed, self.default_mode)
    }

    /// `tab` is on screen: what finished in it while away has been seen. Nothing for a tab it does
    /// not have.
    pub fn mark_seen(&mut self, tab: TabId) {
        if let Some(tab) = self.get_mut(tab) {
            tab.attention.seen();
        }
    }

    /// The attention of `view`'s tabs: for the base view, its tabs summed; for an extra view, its one
    /// tab's (nothing when it shows none). Only the window's sum, [`TabSet::attention`], carries
    /// what closed tabs had been handed: no view held them any more.
    pub fn attention_of(&self, view: ViewId) -> Attention {
        let each: Vec<Attention> = self
            .tabs
            .iter()
            .filter(|t| t.view == view)
            .map(|t| t.attention.attention())
            .collect();
        tabs::sum_attention(&each)
    }

    /// Every tab's own attention with the view showing it, in tab order: what
    /// [`tabs::views_handed_a_card`] compares between two reports.
    pub fn tab_attentions(&self) -> Vec<tabs::TabAttention> {
        self.tabs
            .iter()
            .map(|t| tabs::TabAttention {
                tab: t.id,
                view: t.view,
                attention: t.attention.attention(),
                newest_card: t.attention.newest_pending_stamp(),
            })
            .collect()
    }

    /// The tab of `view` holding its newest pending card.
    pub fn newest_card_tab_in(&self, view: ViewId) -> Option<TabId> {
        let stamps: Vec<(TabId, Option<u64>)> = self
            .tabs
            .iter()
            .filter(|t| t.view == view)
            .map(|t| (t.id, t.attention.newest_pending_stamp()))
            .collect();
        tabs::newest_card(&stamps)
    }

    /// Where the oldest waiting card is shown: [`TabSet::oldest_card_tab`] with its place.
    pub fn oldest_card_place(&self) -> Option<TabPlace> {
        self.oldest_card_tab().and_then(|tab| self.place_of(tab))
    }

    /// The window's attention: every tab's, summed. Its `arrived` never goes down -- not when a tab
    /// installs a new session or resets (`AttentionTracker::restart`), nor when one closes
    /// (`removed_arrived`) -- so a card arriving in the same tick as either still raises it.
    pub fn attention(&self) -> Attention {
        let each: Vec<Attention> = self.tabs.iter().map(|t| t.attention.attention()).collect();
        let mut sum = tabs::sum_attention(&each);
        sum.arrived += self.removed_arrived;
        sum
    }

    pub fn oldest_card_tab(&self) -> Option<TabId> {
        let stamps: Vec<(TabId, Option<u64>)> = self
            .tabs
            .iter()
            .map(|t| (t.id, t.attention.oldest_pending_stamp()))
            .collect();
        tabs::oldest_card(&stamps)
    }

    pub fn newest_card_tab(&self) -> Option<TabId> {
        let stamps: Vec<(TabId, Option<u64>)> = self
            .tabs
            .iter()
            .map(|t| (t.id, t.attention.newest_pending_stamp()))
            .collect();
        tabs::newest_card(&stamps)
    }

    /// A turn in progress, a connect or a waiting question: closing abandons outstanding work.
    pub fn running_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| matches!(t.backend, TabBackend::Starting(_)) || t.turn_running() || t.question_waiting())
            .count()
    }

    /// Tabs with live background work; scheduled wakeups and ambient tasks do not count.
    pub fn background_count(&self) -> usize {
        self.tabs.iter().filter(|tab| tab.background_calls() > 0).count()
    }

    /// `<leader>bd`: vim's `:bd` closes a buffer at once unless it has changes to lose (E89); here the
    /// things to lose include a running turn, a connect, queued messages, live background work and a
    /// waiting question -- the facts `close_prompt` names. `prefix &` does not ask this: it is tmux's `confirm-before` and always asks.
    pub fn close_needs_confirm(&self, id: TabId) -> bool {
        self.get(id).is_some_and(|t| {
            matches!(t.backend, TabBackend::Starting(_) | TabBackend::AwaitingTrust(_))
                || t.turn_running()
                || t.cli_unseen_busy()
                || t.background_calls() > 0
                || t.question_waiting()
                || !t.session.queue.is_empty()
        })
    }

    pub fn close_facts(&self, id: TabId) -> Option<tabs::CloseFacts> {
        let tab = self.get(id)?;
        Some(tabs::CloseFacts {
            number: tab.number,
            label_name: tab.label_name(),
            turn_running: tab.turn_running(),
            unseen_work: tab.cli_unseen_busy(),
            background: tab
                .live()
                .and_then(|backend| tabs::background_summary(&backend.projection())),
            question_waiting: tab.question_waiting(),
            queued: tab.session.queue.len() - tab.steered_unconfirmed(),
            steered: tab.steered_unconfirmed(),
            legacy: self.kind == BackendKind::Legacy && tab.agent == AgentId::Claude,
            has_backend: tab.live().is_some(),
        })
    }

    /// `<leader>bo`: the ids `tabs::close_others` would close, and the y/n
    /// naming them, over every base-view tab but the active one: a tab shown in an extra view is not
    /// in the base page's strip, so it is not one of its others. Same "running" test as
    /// [`Self::running_count`] (a turn in progress, or still connecting). `None` when the active tab
    /// is the base view's only one.
    pub fn close_others_plan(&self) -> Option<(Vec<TabId>, String)> {
        let entries: Vec<(TabId, bool)> = self
            .tabs
            .iter()
            .filter(|t| t.view.is_base())
            .map(|t| {
                (
                    t.id,
                    matches!(t.backend, TabBackend::Starting(_) | TabBackend::AwaitingTrust(_))
                        || t.turn_running()
                        || t.question_waiting(),
                )
            })
            .collect();
        let (ids, mut prompt) = tabs::close_others(&entries, self.active())?;
        let background = ids
            .iter()
            .filter(|id| self.get(**id).is_some_and(|t| t.background_calls() > 0))
            .count();
        if background > 0 {
            let running = entries.iter().any(|(id, running)| *id != self.active() && *running);
            let suffix = if running { "," } else { "" };
            prompt = format!(
                "{}{} {background} with background tasks (y/n)",
                prompt.trim_end_matches(" (y/n)"),
                suffix
            );
        }
        Some((ids, prompt))
    }

    /// Allowed on a starting, trust-waiting or live tab; an empty, failed or ended tab refuses it, and so
    /// does a tab that is moving to the other window or being handed off to a terminal. `wire` is already
    /// composed with the editor context of this moment, the context the message was written against.
    /// `steerable` false keeps the message for the turn's end whatever Enter does later (a confirmed review).
    /// `origin` records who wrote it now, and stays with the message through every deferred send.
    pub fn queue_message(
        &mut self,
        id: TabId,
        text: &str,
        wire: String,
        now_ms: u64,
        steerable: bool,
        origin: PromptOrigin,
    ) -> Result<usize, String> {
        if self.moving.contains(&id.0) {
            return Err(MOVING.to_string());
        }
        let tab = self.get_mut(id).ok_or_else(|| format!("no tab {}", id.0))?;
        if tab.pending_handoff.is_some() {
            return Err("this tab is being handed off to a terminal".to_string());
        }
        match tab.wire_state() {
            TabStateWire::Starting | TabStateWire::AwaitingTrust | TabStateWire::Live => {}
            TabStateWire::NotStarted => return Err("nothing is running to queue behind; Enter sends".to_string()),
            TabStateWire::Ended | TabStateWire::Failed => {
                return Err("this tab's session has ended; press r to start over".to_string())
            }
        }
        tab.session.queue.push(Queued {
            text: text.to_string(),
            wire,
            queued_at_ms: now_ms,
            state: QueuedState::Held,
            steerable,
            dropped_as: Vec::new(),
            steer_when_running: false,
            behind_card: false,
            typed_by_person: origin == PromptOrigin::Typed,
            refused: false,
            shell_line: false,
            shell_report: false,
        });
        Ok(tab.session.queue.len())
    }

    /// Enter while a turn runs: the tab's held messages, oldest first, go into the running turn when its
    /// agent can take messages mid-turn (`AgentBackend::steer`); each leaves the queue once the agent
    /// reads it. Only into the turn the page showed running when Enter was pressed (`shown_turn`): a message
    /// typed in a turn that has ended since waits for the next end. The first refusal stops the rest, so a
    /// later message never reaches the agent before an earlier one (the provider keeps that order for the
    /// refusals it learns of later), and the held ones wait for the turn's end with the reason under them.
    /// A confirmed review stops them too, for the same order, and so does a message the provider reported
    /// dropped once (`Queued::dropped_as`): the agent may still read that copy. A card waiting does not stop
    /// it: the agent reads the message once that call is answered, sooner than the turn's end, and the card
    /// keeps its own keys. A command on an agent that reads commands (`reads_slash_commands`) is stepped over:
    /// only the command waits, in its place, and the messages behind it go in, as Claude Code does with what
    /// is typed during a turn. A message the CLI refused (`Queued::refused`) is stepped over too: it never
    /// goes out on its own, so it holds nothing back. Whether the queue changed.
    pub fn steer_held(&mut self, id: TabId, now_ms: u64, shown_turn: Option<&str>) -> bool {
        if self.moving.contains(&id.0) {
            return false;
        }
        let mut next = self.next_steer;
        let Some(tab) = self.get_mut(id) else { return false };
        let mut changed = tab.settle_steered();
        if tab.pending_handoff.is_some() || !tab.turn_running() || !tab.session.queue.iter().any(Queued::is_held) {
            return changed;
        }
        let label = tab.agent.label();
        let TabBackend::Live(backend) = &mut tab.backend else {
            return changed;
        };
        let reads_commands = reads_slash_commands(&tab.agent, &backend.projection());
        let mut refusal = None;
        for item in tab.session.queue.iter_mut().filter(|q| q.is_held()) {
            // The CLI refused it, so it never goes out again on its own and holds nothing back: what
            // follows it is steered as if it were not there.
            if item.refused {
                continue;
            }
            // A provider that stopped waiting on a message reports it dropped although the agent may
            // still read it, and sent again under a new id it could run twice. It waits for the turn's
            // end instead, where a late delivery under its old id takes it out of the queue.
            if !item.dropped_as.is_empty() {
                refusal = Some(
                    "it went into the turn once already, so it waits for the turn's end, and what follows it waits too"
                        .to_string(),
                );
                break;
            }
            if !item.steerable {
                refusal = Some("a review waits for the turn's end, and what follows it waits too".to_string());
                break;
            }
            if item.is_command(reads_commands) {
                continue;
            }
            let Some(turn) = shown_turn else {
                refusal = Some("no turn was running when it was typed".to_string());
                break;
            };
            let client_id = format!("eitri-{}-{next}", id.0);
            match backend.steer(turn, &client_id, &item.wire, &item.text, item.origin()) {
                Ok(()) => {
                    next += 1;
                    item.state = QueuedState::Steered {
                        client_id,
                        sent_at_ms: now_ms,
                    };
                    changed = true;
                }
                Err(crate::agent_backend::SteerRefusal::Unsupported) => {
                    refusal = Some(format!("{label} cannot take messages mid-turn"));
                    break;
                }
                Err(crate::agent_backend::SteerRefusal::Refused(why)) => {
                    refusal = Some(why);
                    break;
                }
            }
        }
        // A refused message in the queue keeps saying why it stays there, beside any reason the rest waits.
        let refused = tab
            .session
            .queue
            .iter()
            .any(|item| item.refused)
            .then(|| refused_note(agent::CLI_REFUSED_DROP));
        let note = match (refused, refusal.as_deref().map(held_note)) {
            (Some(refused), Some(held)) => Some(format!("{refused}; {held}")),
            (refused, held) => held.or(refused),
        };
        changed |= tab.session.queue_note != note;
        tab.session.queue_note = note;
        self.next_steer = next;
        changed
    }

    /// The queue's oldest work as one turn, only when the tab can take one: a command alone, or the messages
    /// up to the next command (`next_turn`). What stays goes out at that turn's end, in the order typed.
    ///
    /// Only held messages go out. A message sent into the turn that just ended has its outcome reported
    /// before that end, or else the agent runs it in a follow-up turn whose start arrives with that end
    /// (`AgentProvider::steer_turn`), so by now it has left the queue or is held again, or a turn runs and
    /// nothing is sent; one a provider never reported on goes out too, so that it may reach the agent
    /// twice rather than not at all.
    ///
    /// What the agent reported and whether a turn runs are read under one hold of the projection: read
    /// apart, a delivery and the turn's end folded between the two reads would make a message the agent
    /// read look unreported, and it would be sent again.
    pub fn flush_queue(&mut self, id: TabId) -> Option<Flush> {
        // A turn already starting in the tab takes the queue at its end, as a running turn does: sent
        // now, the queue would be a second turn the agent refuses.
        if self.starting.contains(&id.0) || self.pending_sends.contains_key(&id.0) || !self.queue_can_go(id) {
            return None;
        }
        let gate = self.begin_turn_send(id);
        match self.send_when_allowed(gate, moving::TurnSend::Queue)? {
            Sent::Flushed { flush, .. } => Some(flush),
            Sent::First { .. } | Sent::Prompt { .. } | Sent::NotSent { .. } => None,
        }
    }

    /// Whether the queue of `id` can go out as a turn now: it holds something, the tab is live and
    /// idle with no card waiting, and it is not handed off or moving. Settles what the agent reported
    /// about the messages put into its turn first, under the same hold of the projection.
    ///
    /// Work Claude Code does where this panel cannot see it (`cli_unseen_busy`) does not hold the queue:
    /// a background agent's messages raise the same note, and holding for it would keep a message
    /// waiting for that agent's whole life; the sidecar takes a turn sent meanwhile.
    fn queue_can_go(&mut self, id: TabId) -> bool {
        if self.moving.contains(&id.0) || self.shell_line_runs(id) {
            return false;
        }
        let Some(tab) = self.get_mut(id) else {
            return false;
        };
        if tab.session.queue.is_empty() || tab.pending_handoff.is_some() {
            return false;
        }
        {
            let TabBackend::Live(backend) = &tab.backend else {
                return false;
            };
            let projection = backend.projection();
            settle_opening(&mut tab.session.opening_prompt, &mut tab.session.queue, &projection);
            settle_against(
                &tab.agent,
                &mut tab.session.queue,
                &mut tab.session.queue_note,
                &projection,
            );
            if projection.active_turn_id.is_some()
                || !projection.pending_permissions.is_empty()
                || projection.waiting_questions().next().is_some()
            {
                return false;
            }
        }
        // A message the CLI refused waits for the person; it is not a reason to send a turn.
        tab.session.queue.iter().any(|item| !item.refused)
    }

    /// The one place a turn of Eitri's goes out to an agent: the tab's queue as one turn, or a new
    /// session's first message. Takes the [`SendGate`] that let it go, which only
    /// [`TabSet::begin_turn_send`] makes, so no turn reaches an agent without its tab having entered
    /// `starting` first. `None` when the queue can no longer go (a turn started meanwhile, the tab
    /// left): its start is over, and the queue waits as it would have.
    fn release_turn_send(&mut self, gate: SendGate, what: moving::TurnSend) -> Option<Sent> {
        let root = self.project_root.clone();
        let id = gate.tab();
        let sent = match what {
            moving::TurnSend::Queue => {
                if !self.queue_can_go(id) {
                    self.end_start_of(&gate);
                    return None;
                }
                let tab = self.get_mut(id).expect("checked by queue_can_go");
                if !tab.session.queue.iter().all(Queued::is_held) {
                    eprintln!(
                        "[agent_panel] tab {}: the agent never said whether it read a message sent into its turn; it is sent again",
                        tab.id.0
                    );
                    for item in tab.session.queue.iter_mut() {
                        item.state = QueuedState::Held;
                    }
                }
                let turn = next_turn(&tab.session.queue, tab.reads_commands());
                if let [index] = turn[..] {
                    if tab.session.queue[index].shell_line {
                        let item = tab.session.queue.remove(index);
                        let outcome = self.start_shell_line(id, &item.text);
                        self.end_start_of(&gate);
                        return Some(Sent::Flushed {
                            tab: id,
                            flush: Flush {
                                typed: item.text,
                                outcome,
                            },
                        });
                    }
                }
                let items = || turn.iter().map(|index| &tab.session.queue[*index]);
                let wire = items().map(|q| q.wire.as_str()).collect::<Vec<_>>().join("\n\n");
                let typed = items().map(|q| q.text.as_str()).collect::<Vec<_>>().join("\n\n");
                let origin = if items().any(|item| item.shell_report) {
                    PromptOrigin::ShellLine
                } else {
                    prompt_origin(items().all(|item| item.typed_by_person))
                };
                let trace = crate::turn_trace::TurnTrace::start();
                let mode = tab.mode;
                let outcome =
                    tab.live_mut()
                        .expect("checked above")
                        .send_turn_in(&wire, &typed, mode, root.as_deref(), origin);
                match &outcome {
                    Ok(submitted) => {
                        let mut message = Queued::held(&typed, wire, wall_clock_ms());
                        message.typed_by_person = origin == PromptOrigin::Typed;
                        tab.session.opening_prompt = Some(OpeningPrompt {
                            submitted: submitted.clone(),
                            typed_by_person: message.typed_by_person,
                            message,
                        });
                        for index in turn.iter().rev() {
                            tab.session.queue.remove(*index);
                        }
                        tab.session.queue_error = None;
                        tab.session.queue_note = tab
                            .session
                            .queue
                            .iter()
                            .any(|item| item.refused)
                            .then(|| refused_note(agent::CLI_REFUSED_DROP));
                        tab.session.turn_trace = trace;
                    }
                    Err(error) => tab.session.queue_error = Some(error.message.clone()),
                }
                tab.session.was_running = tab.turn_running();
                let failed = outcome.is_err();
                if failed {
                    self.end_start_of(&gate);
                }
                Sent::Flushed {
                    tab: id,
                    flush: Flush {
                        typed,
                        outcome: outcome.map(|_| ()),
                    },
                }
            }
            moving::TurnSend::First { request_id, turn } => {
                let outcome = if self.shells.take_first(&request_id) {
                    let outcome = self.start_shell_line(id, &turn.typed);
                    self.end_start_of(&gate);
                    outcome
                } else {
                    self.send_one(&gate, &turn)
                };
                Sent::First {
                    tab: id,
                    request_id,
                    typed: turn.typed,
                    outcome,
                }
            }
            moving::TurnSend::Prompt { request_id, page, turn } => {
                // A card or question that arrived during the presence wait holds this message too:
                // the session is stopped on the user.
                if self.card_waits(id) {
                    self.end_start_of(&gate);
                    let outcome = self.hold_behind_card(id, &turn);
                    return Some(Sent::Prompt {
                        tab: id,
                        page,
                        request_id,
                        typed: turn.typed,
                        outcome,
                    });
                }
                let mut outcome = self.send_one(&gate, &turn);
                // The agent had a turn open that this side had not seen: the message waits to steer
                // into it, and the page is not told of an error.
                if outcome.as_ref().is_err_and(BackendError::is_turn_busy) && self.hold_as_steer(id, &turn) {
                    outcome = Ok(());
                }
                Sent::Prompt {
                    tab: id,
                    page,
                    request_id,
                    typed: turn.typed,
                    outcome,
                }
            }
        };
        Some(sent)
    }

    /// Whether a card or question holds sends in this Claude session. External agents keep their
    /// existing send policy.
    pub(super) fn card_waits(&self, id: TabId) -> bool {
        self.get(id).is_some_and(|tab| {
            !tab.agent.is_external()
                && tab.live().is_some_and(|backend| {
                    let projection = backend.projection();
                    !projection.pending_permissions.is_empty() || projection.waiting_questions().next().is_some()
                })
        })
    }

    /// A message the agent refused because a turn was open: kept at the end of `id`'s queue to go into
    /// that turn (`steer_waiting`), tried at once when the projection already shows it. False when the
    /// tab cannot hold it (an external agent's tab keeps its refusal).
    fn hold_as_steer(&mut self, id: TabId, turn: &FirstTurn) -> bool {
        let Some(tab) = self.get_mut(id) else { return false };
        if tab.agent.is_external() {
            return false;
        }
        let mut item = Queued::held(&turn.typed, turn.wire.clone(), wall_clock_ms());
        item.steer_when_running = true;
        item.typed_by_person = turn.typed_by_person;
        tab.session.queue.push(item);
        tab.session.queue_error = None;
        // Only this tab: the tick's `steer_waiting` reports every other tab whose queue it changes, so
        // the page is sent that queue again; this one's caller sends its queue itself.
        self.steer_waiting_in(Some(id), wall_clock_ms());
        true
    }

    /// Puts the messages a refused send left to steer (`Queued::steer_when_running`) into the turn their
    /// tab now shows running: one attempt each, and what the agent refuses waits for the turn's end with
    /// the reason under it. The tabs whose queue changed, for the panel to be sent it again.
    pub fn steer_waiting(&mut self, now_ms: u64) -> Vec<TabId> {
        self.steer_waiting_in(None, now_ms)
    }

    /// [`Self::steer_waiting`] over one tab (`Some`) or every tab (`None`).
    fn steer_waiting_in(&mut self, only: Option<TabId>, now_ms: u64) -> Vec<TabId> {
        let ready: Vec<(TabId, String)> = self
            .tabs
            .iter()
            .filter(|t| only.is_none_or(|id| t.id == id))
            .filter(|t| t.pending_handoff.is_none() && !self.moving.contains(&t.id.0))
            .filter(|t| t.session.queue.iter().any(|q| q.steer_when_running && q.is_held()))
            .filter_map(|t| Some((t.id, t.live()?.projection().active_turn_id.clone()?)))
            .collect();
        let mut changed = Vec::new();
        for (id, turn) in ready {
            self.steer_held(id, now_ms, Some(&turn));
            if let Some(tab) = self.get_mut(id) {
                for item in tab.session.queue.iter_mut() {
                    item.steer_when_running = false;
                }
            }
            changed.push(id);
        }
        changed
    }

    /// Sends, as a turn, the messages typed while a card waited with no turn running, once no card
    /// waits. A message the queue cannot send yet stays marked and is tried again.
    pub fn release_after_card(&mut self) -> Vec<(TabId, Flush)> {
        let ready: Vec<TabId> = self
            .tabs
            .iter()
            .filter(|t| t.session.queue.iter().any(|q| q.behind_card))
            .map(|t| t.id)
            .filter(|id| !self.card_waits(*id))
            .collect();
        let mut sent = Vec::new();
        for id in ready {
            let flush = self.flush_queue(id);
            let running = self.get(id).is_some_and(Tab::turn_running);
            if flush.is_some() || running {
                if let Some(tab) = self.get_mut(id) {
                    for item in tab.session.queue.iter_mut() {
                        item.behind_card = false;
                    }
                }
            }
            sent.extend(flush.map(|flush| (id, flush)));
        }
        sent
    }

    /// One message of `id`, as it was composed, sent as its own turn under the tab's mode, traced from
    /// now. Only [`TabSet::release_turn_send`] calls it, once the gate let the message go.
    fn send_one(&mut self, gate: &SendGate, turn: &FirstTurn) -> Result<(), BackendError> {
        let id = gate.tab();
        let root = self.project_root.clone();
        let outcome = match self.get_mut(id) {
            Some(tab) => {
                tab.session.turn_trace = crate::turn_trace::TurnTrace::start();
                let mode = tab.mode;
                match tab.live_mut() {
                    Some(backend) => {
                        backend.send_turn_in(&turn.wire, &turn.typed, mode, root.as_deref(), turn.origin())
                    }
                    None => Err(BackendError {
                        message: "no active session".to_string(),
                        benign: true,
                    }),
                }
            }
            None => Err(BackendError {
                message: format!("no tab {}", id.0),
                benign: true,
            }),
        };
        if let Ok(submitted) = &outcome {
            if let Some(tab) = self.get_mut(id) {
                let mut message = Queued::held(&turn.typed, turn.wire.clone(), wall_clock_ms());
                message.typed_by_person = turn.typed_by_person;
                tab.session.opening_prompt = Some(OpeningPrompt {
                    submitted: submitted.clone(),
                    typed_by_person: message.typed_by_person,
                    message,
                });
            }
        } else {
            self.end_start_of(gate);
        }
        outcome.map(|_| ())
    }

    /// The tab's review draft (memory only).
    pub fn review_draft(&self, id: TabId) -> Result<&crate::turn_review::ReviewDraft, String> {
        self.get(id)
            .map(|t| &t.review_draft)
            .ok_or_else(|| format!("no tab {}", id.0))
    }

    pub fn review_draft_mut(&mut self, id: TabId) -> Result<&mut crate::turn_review::ReviewDraft, String> {
        self.get_mut(id)
            .map(|t| &mut t.review_draft)
            .ok_or_else(|| format!("no tab {}", id.0))
    }

    /// The user's confirmed review, sent as the user's own message: queued like a typed prompt and
    /// flushed at once, with `text` as both what is sent and what the transcript shows. `Some` means
    /// it went out now (whatever the backend answered, see `Flush::outcome`; a refused send stays
    /// queued); `None` means it waits behind a running turn or a pending card.
    ///
    /// Only the review flow's confirmed send calls this. It adds nothing to the prompt history.
    pub fn send_review(&mut self, id: TabId, text: &str, now_ms: u64) -> Result<Option<Flush>, String> {
        self.queue_message(id, text, text.to_owned(), now_ms, false, PromptOrigin::Composed)?;
        Ok(self.flush_queue(id))
    }

    /// `Ctrl+Enter`: while a turn runs, the text is queued and the turn interrupted, and the queue goes
    /// out when it ends; while idle, it is queued and flushed at once.
    pub fn send_now(&mut self, id: TabId, text: &str, wire: String, now_ms: u64) -> Result<SendNow, String> {
        if self.moving.contains(&id.0) {
            return Err(MOVING.to_string());
        }
        if !text.trim().is_empty() {
            self.queue_message(id, text, wire, now_ms, true, PromptOrigin::Typed)?;
        }
        let tab = self.get_mut(id).ok_or_else(|| format!("no tab {}", id.0))?;
        if tab.turn_running() {
            let backend = tab.live_mut().expect("a running turn has a backend");
            return Ok(SendNow::Interrupting(backend.interrupt()));
        }
        Ok(SendNow::Flushed(self.flush_queue(id)))
    }

    /// `id` is the scratch request now holding `tab`'s draft.
    ///
    /// Known limit: an edit whose buffer is never wiped (nvim quit with `:qa!`, where `BufWipeout`
    /// does not fire, or crashed) writes no marker, so `editing_draft` stays set and every later
    /// `Ctrl+g` on this tab is refused for the rest of the window. Nothing times it out, and
    /// `reset` leaves it alone on purpose: clearing it there would orphan an edit still open.
    pub fn begin_scratch_edit(&mut self, tab: TabId, id: u64) -> Result<(), String> {
        let t = self.get_mut(tab).ok_or_else(|| format!("no tab {}", tab.0))?;
        if t.editing_draft.is_some() {
            return Err("this tab's draft is already open in nvim".to_string());
        }
        t.editing_draft = Some(id);
        Ok(())
    }

    /// The tab edit `id` belonged to, and its new draft when one came back. A closed tab's edit
    /// finds no owner and is dropped: the caller removes its scratch files on the same tick, so
    /// text `:wq`'d for a tab closed while the edit was out is gone (the tab's draft went with it).
    pub fn finish_scratch_edit(&mut self, id: u64, done: &crate::scratch::EditDone) -> Option<(TabId, Option<String>)> {
        let tab = self.tabs.iter_mut().find(|t| t.editing_draft == Some(id))?;
        tab.editing_draft = None;
        if tab.session.editing_question.take().is_some() {
            return Some((tab.id, None));
        }
        let draft = match done {
            crate::scratch::EditDone::Written(text) => {
                tab.replace_draft(text.clone());
                Some(text.clone())
            }
            _ => None,
        };
        Some((tab.id, draft))
    }

    /// Ends every draft edit still out in nvim, oldest edit first, for an editor that went away.
    /// `results` holds what each edit came back as, by edit id; a tab with no entry is treated as
    /// discarded. A tab's `editing_draft` is cleared whether or not it has an entry, so the next
    /// `Ctrl+g` on it works again (the limit `begin_scratch_edit` documents).
    ///
    /// A saved body replaces the draft and is returned; a body equal to the draft, or to it without
    /// its final newline (the shell strips one before it hands the text over), saved nothing, so
    /// the draft is left alone and `None` comes back.
    pub fn cancel_scratch_edits(
        &mut self,
        results: &[(u64, crate::scratch::EditDone)],
    ) -> Vec<(TabId, Option<String>)> {
        let mut open: Vec<(u64, usize)> = self
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| tab.editing_draft.map(|id| (id, index)))
            .collect();
        open.sort_by_key(|(id, _)| *id);
        let mut out = Vec::new();
        for (id, index) in open {
            let tab = &mut self.tabs[index];
            tab.editing_draft = None;
            if tab.session.editing_question.take().is_some() {
                out.push((tab.id, None));
                continue;
            }
            let saved = match results.iter().find(|(result_id, _)| *result_id == id) {
                Some((_, crate::scratch::EditDone::Written(text)))
                    if *text != tab.session.draft && Some(text.as_str()) != tab.session.draft.strip_suffix('\n') =>
                {
                    tab.replace_draft(text.clone());
                    Some(text.clone())
                }
                _ => None,
            };
            out.push((tab.id, saved));
        }
        out
    }

    /// The held messages, oldest first, for the panel to merge into the box. A message already
    /// sent into the running turn stays: the agent may have read it, and taking it back would not unsay it.
    pub fn take_back_queue(&mut self, id: TabId) -> Vec<String> {
        let Some(tab) = self.get_mut(id) else { return Vec::new() };
        tab.settle_steered();
        tab.session.queue_error = None;
        tab.session.queue_note = None;
        let (held, steered): (Vec<Queued>, Vec<Queued>) = std::mem::take(&mut tab.session.queue)
            .into_iter()
            .partition(Queued::is_held);
        tab.session.queue = steered;
        held.into_iter().map(|q| q.text).collect()
    }

    /// The panel's mirror. Never echoed.
    /// A copy set here is newer than anything the page the tab last left could still post, so that
    /// page's late mirror ([`TabSet::take_late_draft`]) is no longer taken.
    pub fn set_draft(&mut self, id: TabId, text: &str) -> bool {
        let Some(tab) = self.get_mut(id) else { return false };
        tab.replace_draft(text.to_string());
        true
    }

    /// Store only live question mirrors, bounded independently of the page.
    pub fn set_question_draft(
        &mut self,
        id: TabId,
        page: ViewId,
        interaction: &str,
        draft: serde_json::Value,
    ) -> Result<(), String> {
        if serde_json::to_vec(&draft).map_err(|e| e.to_string())?.len() > 64 * 1024 {
            return Err("the question draft exceeds 64 KiB".into());
        }
        self.check_question_edit(id, page, interaction, None)?;
        let tab = self.get_mut(id).unwrap();
        let mut drafts = tab.session.question_draft.clone();
        drafts.insert(interaction.into(), draft);
        if serde_json::to_vec(&drafts).unwrap().len() > 64 * 1024 {
            return Err("the question drafts exceed 64 KiB".into());
        }
        tab.session.question_draft = drafts;
        Ok(())
    }

    /// Writes `text`, a question's typed answer edited in nvim, into the tab's stored question draft: answer
    /// `index` takes the text and becomes the typed answer (on a single-select question it replaces the picked
    /// option, as typing in the dock does). The page that shows the tab may be showing another tab when the edit
    /// comes back and so cannot take it; the stored draft is what the dock restores when the tab is shown again.
    /// The draft stays the page's own shape; only that answer's `text`, `other` and `options` change.
    pub fn store_question_other(&mut self, id: TabId, interaction: &str, index: u32, text: &str) -> Result<(), String> {
        let tab = self.get(id).ok_or("no such tab")?;
        let (count, multi_select) = {
            let backend = tab.live().ok_or("this tab has no live session")?;
            let projection = backend.projection();
            let record = projection
                .waiting_questions()
                .find(|q| q.interaction_id == interaction)
                .ok_or("that question is no longer waiting")?;
            let question = record.questions.get(index as usize).ok_or("no such question index")?;
            (record.questions.len(), question.multi_select)
        };
        let mut draft = tab
            .session
            .question_draft
            .get(interaction)
            .cloned()
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        let empty = || serde_json::json!({"options": [], "other": false, "text": "", "notes": "", "showNotes": false});
        let mut answers = match draft.get("answers") {
            Some(serde_json::Value::Array(answers)) => answers.clone(),
            _ => Vec::new(),
        };
        answers.truncate(count);
        while answers.len() < count {
            answers.push(empty());
        }
        let answer = &mut answers[index as usize];
        if !answer.is_object() {
            *answer = empty();
        }
        answer["text"] = serde_json::Value::String(text.to_string());
        answer["other"] = serde_json::Value::Bool(true);
        if !multi_select {
            answer["options"] = serde_json::json!([]);
        }
        draft["answers"] = serde_json::Value::Array(answers);
        let page = tab.view;
        self.set_question_draft(id, page, interaction, draft)
    }

    pub fn check_question_edit(
        &self,
        id: TabId,
        page: ViewId,
        interaction: &str,
        index: Option<u32>,
    ) -> Result<(), String> {
        if self.moving.contains(&id.0) {
            return Err(MOVING.into());
        }
        let tab = self.get(id).ok_or("no such tab")?;
        if tab.view != page {
            return Err(format!("tab {} is not shown in this page", id.0));
        }
        let backend = tab.live().ok_or("this tab has no live session")?;
        let projection = backend.projection();
        let record = projection
            .waiting_questions()
            .find(|q| q.interaction_id == interaction)
            .ok_or("that question is no longer waiting")?;
        if index.is_some_and(|i| i as usize >= record.questions.len()) {
            return Err("no such question index".into());
        }
        Ok(())
    }

    pub fn begin_question_edit(
        &mut self,
        tab: TabId,
        edit: u64,
        interaction: String,
        index: u32,
    ) -> Result<(), String> {
        self.begin_scratch_edit(tab, edit)?;
        self.get_mut(tab).unwrap().session.editing_question = Some((interaction, index));
        Ok(())
    }

    pub fn question_edit(&self, edit: u64) -> Option<(TabId, String, u32)> {
        let tab = self.tabs.iter().find(|t| t.editing_draft == Some(edit))?;
        let (interaction, index) = tab.session.editing_question.as_ref()?;
        Some((tab.id, interaction.clone(), *index))
    }

    /// Whether a `draft` for `tab` from `from`'s page is the one late mirror of the page the tab was
    /// just moved out of (`break_out`, `join_back`): true once, and only while no newer copy has come
    /// in since the move. Any other draft for a tab another page shows is not this page's to give.
    pub fn take_late_draft(&mut self, tab: TabId, from: ViewId) -> bool {
        let Some(t) = self.get_mut(tab) else { return false };
        if t.session.left_view != Some(from) {
            return false;
        }
        t.session.left_view = None;
        true
    }

    /// Clear the mirrored draft when `id` sends a prompt. The composer clears on Enter but mirrors
    /// `""` only 300 ms later. `active_state_payloads` can run when the first connect finishes and
    /// send this stale copy back to the composer, restoring text already sent. Clear it here
    /// rather than waiting for the mirror.
    pub fn note_sent(&mut self, id: TabId) {
        if let Some(tab) = self.get_mut(id) {
            tab.replace_draft(String::new());
        }
    }

    /// The queued texts of one tab, oldest first (the close path reads them before removing it).
    pub fn queue_texts(&self, id: TabId) -> Vec<String> {
        self.get(id)
            .map(|t| t.session.queue.iter().map(|q| q.text.clone()).collect())
            .unwrap_or_default()
    }

    /// Every tab's queue, summed (the window-close prompt): held messages and those sent into a
    /// running turn that the agent has not been seen to read.
    pub fn queued_count(&self) -> usize {
        self.tabs.iter().map(|t| t.session.queue.len()).sum()
    }

    /// The window is closing; every queued text, tab by tab, for the history.
    pub fn take_every_queued_text(&mut self) -> Vec<String> {
        self.tabs
            .iter_mut()
            .flat_map(|t| std::mem::take(&mut t.session.queue).into_iter().map(|q| q.text))
            .collect()
    }

    /// The window is closing: every tab, for the close path to tear down, each live backend retired
    /// like a closed tab's. Nothing reports those retirements done: the presence they keep ends with
    /// the window's.
    ///
    /// `take_all` leaves the set without tabs, so `active_tab()` would panic afterwards. Only the
    /// close path calls it, after setting `shutting_down`, and nothing reads the set after that
    /// (`eitri_panel::agent_panel` checks `shutting_down` first on every path that reaches the set).
    pub fn take_all(&mut self) -> Vec<RemovedTab> {
        self.closing = true;
        for tab in &mut self.tabs {
            tab.cancel_review_watchers();
        }
        if let Some(review) = self.turn_review.as_mut() {
            let now = wall_clock_ms();
            for tab in &self.tabs {
                review.close_tab(tab.id.0, now);
            }
        }
        let retiring = &mut self.retiring;
        let removed: Vec<RemovedTab> = std::mem::take(&mut self.tabs)
            .into_iter()
            .map(|mut tab| {
                let retired = tab.retire_live(retiring);
                RemovedTab { tab, retired }
            })
            .collect();
        for tab in &removed {
            self.forget_move_state(tab.tab.id);
        }
        self.sync_presence();
        removed
    }

    /// Whether no tab of this window has started a session (or is starting one): the state a launch
    /// is in until something is typed or resumed.
    pub fn is_pristine(&self) -> bool {
        self.tabs.iter().all(|t| matches!(t.backend, TabBackend::NotStarted))
    }

    /// The tabs worth bringing back next launch (`saved_tabs`), or `None` while the window is closing
    /// and has handed its tabs over to be torn down -- closing every tab is not the user closing
    /// them. A tab is listed when it has a session, which includes one still being resumed. Claude
    /// tabs are listed only on the sidecar backend, the only one that resumes; another agent's tab
    /// whose agent resumes is listed whatever the window's Claude backend, after the Claude tabs. A tab
    /// bound to an extra chat view is saved with that view's number, so a restore can put it back
    /// there; a restore still waiting for its trust answer keeps the views it was saved with.
    ///
    /// `conversation_id` is the project's (`agent::conversation_id_for_cwd`): every session of one
    /// project shares it, and a tab whose backend has not been built yet has none of its own.
    pub fn saved_snapshot(&self, conversation_id: &str) -> Option<SavedTabs> {
        if self.closing {
            return None;
        }
        let external: Vec<(TabId, SavedTab)> = self
            .tabs
            .iter()
            .filter(|tab| tab.agent.is_external() && (tab.resumable(self.kind) || tab.failed_resume.is_some()))
            .filter_map(|tab| {
                let session = self.saved_session_id(tab)?;
                Some((
                    tab.id,
                    SavedTab {
                        agent: tab.agent.clone(),
                        conversation_id: conversation_id.to_string(),
                        provider_session_id: session,
                        name: tab.name.clone(),
                        mode: tab.mode,
                        view: saved_view(tab.view),
                        window: None,
                        turn_running: tab.turn_running(),
                    },
                ))
            })
            .collect();
        let mut others_active = external.iter().position(|(id, _)| *id == self.active);
        let waiting_active = self.waiting_restore_active();
        let mut others: Vec<SavedTab> = external.into_iter().map(|(_, saved)| saved).collect();
        // A restore waiting for the trust question has no tab for its sessions yet; they stay in the
        // file meanwhile, so a session started in another tab does not write over them. A dashboard
        // restore's are left out only while the window has nothing else to save: then nothing is written
        // and the file still holds them (`settle_waiting_restore`).
        let waiting = self.waiting_restore_tabs();
        for granted in waiting.iter().filter(|g| g.planned.agent.is_external()) {
            if others_active.is_none() && waiting_active == Some(granted.planned.provider_session_id.as_str()) {
                others_active = Some(others.len());
            }
            others.push(SavedTab {
                agent: granted.planned.agent.clone(),
                conversation_id: conversation_id.to_string(),
                provider_session_id: granted.planned.provider_session_id.clone(),
                name: granted.planned.name.clone(),
                mode: granted.mode,
                view: granted.planned.view.and_then(saved_view),
                window: None,
                turn_running: granted.planned.turn_running,
            });
        }
        if self.kind != BackendKind::Sidecar {
            return Some(SavedTabs::from_parts(Vec::new(), 0, others, others_active));
        }
        // `provider_session_id` is read in its own statement per tab (the lock-order rule above).
        let mut listed: Vec<(TabId, SavedTab)> = self
            .tabs
            .iter()
            .filter(|tab| tab.agent == AgentId::Claude)
            .filter_map(|tab| {
                let session = self.saved_session_id(tab)?;
                Some((
                    tab.id,
                    SavedTab {
                        agent: AgentId::Claude,
                        conversation_id: conversation_id.to_string(),
                        provider_session_id: session,
                        name: tab.name.clone(),
                        mode: tab.mode,
                        view: saved_view(tab.view),
                        window: None,
                        turn_running: tab.turn_running(),
                    },
                ))
            })
            .collect();
        // The tab on screen if it is listed, else the listed tab that was on screen most recently.
        let mut active = listed
            .iter()
            .position(|(id, _)| *id == self.active)
            .or_else(|| {
                self.recency
                    .iter()
                    .rev()
                    .find_map(|recent| listed.iter().position(|(id, _)| id == recent))
            })
            .unwrap_or(0);
        let had_tabs = !listed.is_empty();
        for granted in waiting.iter().filter(|g| g.planned.agent == AgentId::Claude) {
            // Behind the tabs that exist; when none does, the restore's own active tab is the one.
            if !had_tabs && self.waiting_restore_active() == Some(granted.planned.provider_session_id.as_str()) {
                active = listed.len();
            }
            listed.push((
                TabId(u64::MAX),
                SavedTab {
                    agent: AgentId::Claude,
                    conversation_id: conversation_id.to_string(),
                    provider_session_id: granted.planned.provider_session_id.clone(),
                    name: granted.planned.name.clone(),
                    mode: granted.mode,
                    view: granted.planned.view.and_then(saved_view),
                    window: None,
                    turn_running: granted.planned.turn_running,
                },
            ));
        }
        Some(SavedTabs::from_parts(
            listed.into_iter().map(|(_, saved)| saved).collect(),
            active,
            others,
            others_active,
        ))
    }

    /// The session `tab` is saved under: the one it holds, or, for a tab that failed starting a saved
    /// session, that session, so a refused start does not lose the conversation from the file. Not
    /// when the same session is open and running in another tab (then that tab is the entry).
    fn saved_session_id(&self, tab: &Tab) -> Option<String> {
        if let TabBackend::Failed { .. } = tab.backend {
            let id = tab.failed_resume.clone().filter(|id| !id.is_empty())?;
            let open_elsewhere = self.tabs.iter().any(|other| {
                other.id != tab.id
                    && other.agent == tab.agent
                    && other.provider_session_id().as_deref() == Some(id.as_str())
            });
            // Two failed tabs of the same session (a refused restore, then a refused resume of it by hand)
            // are one entry, the first; else the next restore would bring it back twice.
            let failed_before = self.tabs.iter().take_while(|other| other.id != tab.id).any(|other| {
                matches!(other.backend, TabBackend::Failed { .. })
                    && other.agent == tab.agent
                    && other.failed_resume.as_deref() == Some(id.as_str())
            });
            return (!open_elsewhere && !failed_before).then_some(id);
        }
        // Between a `/clear` and the new conversation's first `SessionOpened` the session holds no id;
        // the projection carries the one the CLI named for the new conversation, so the tab is saved
        // under it rather than dropped from the file. Read in its own statement (the lock-order rule).
        let held = tab.provider_session_id();
        held.or_else(|| {
            tab.live()
                .and_then(|backend| backend.projection().provider_session_id.clone())
        })
        .filter(|id| !id.is_empty())
    }

    /// The sessions of a restore that is waiting for the trust question and so has no tab of its own
    /// yet, except the ones already open in a tab.
    fn waiting_restore_tabs(&self) -> Vec<&GrantedTab> {
        self.tabs
            .iter()
            .filter_map(|tab| match &tab.backend {
                TabBackend::AwaitingTrust(AwaitingTrust {
                    start: DeferredStart::Restore { go },
                    ..
                }) => Some(go),
                _ => None,
            })
            .filter(|go| !go.reoffer)
            .flat_map(|go| go.tabs.iter())
            .filter(|granted| {
                let (agent, id) = (&granted.planned.agent, granted.planned.provider_session_id.as_str());
                self.tab_with_session(agent, id).is_none()
                    && !self.tabs.iter().any(|t| {
                        matches!(t.backend, TabBackend::Failed { .. })
                            && t.agent == *agent
                            && t.failed_resume.as_deref() == Some(id)
                    })
            })
            .collect()
    }

    /// Whether the window has anything to save besides a dashboard restore that is waiting for the trust
    /// question: a write of the file would then leave that restore's sessions out.
    fn saves_more_than_the_offer(&self) -> bool {
        self.saved_snapshot("").is_some_and(|saved| !saved.is_empty())
    }

    /// A dashboard restore waiting for the trust question stops being on offer once another tab has
    /// something to save: from then on its sessions are listed in the file like a launch restore's, and a
    /// put-off keeps them as failed tabs. Call it before each write of the file, so no write leaves
    /// them out; it is for good, as a later close of that other tab must not drop them again. Returns
    /// whether the offer ended now (its source is spent).
    pub fn settle_waiting_restore(&mut self) -> bool {
        let waiting_offer = self.tabs.iter().any(|tab| {
            matches!(&tab.backend, TabBackend::AwaitingTrust(AwaitingTrust {
                start: DeferredStart::Restore { go },
                ..
            }) if go.reoffer)
        });
        if !waiting_offer || !self.saves_more_than_the_offer() {
            return false;
        }
        for tab in &mut self.tabs {
            if let TabBackend::AwaitingTrust(AwaitingTrust {
                start: DeferredStart::Restore { go },
                ..
            }) = &mut tab.backend
            {
                go.reoffer = false;
            }
        }
        true
    }

    /// The session the waiting restore would put on screen, if it names one.
    fn waiting_restore_active(&self) -> Option<&str> {
        self.tabs.iter().find_map(|tab| match &tab.backend {
            TabBackend::AwaitingTrust(AwaitingTrust {
                start: DeferredStart::Restore { go },
                ..
            }) if !go.reoffer => go.active_session.as_deref(),
            _ => None,
        })
    }

    /// The user's own `init.lua` choice of the mode new tabs start in (`agent.default_mode`): the
    /// window's default, and every empty tab it has so far. This is the one route into bypass that
    /// needs no question, because the question has been answered in a file the user wrote.
    pub fn apply_configured_default(&mut self, mode: SessionModeChoice) {
        self.default_mode = mode;
        let empty: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| matches!(tab.backend, TabBackend::NotStarted))
            .map(|(at, _)| at)
            .collect();
        for at in empty {
            self.give_mode(at, mode);
        }
    }

    /// The user's own `init.lua` choice of the agent new tabs run (`agent.default`), applied once at
    /// startup to the tabs that exist by then and have not started: a tab's agent is fixed from the
    /// first message on, so a started or named tab keeps its own.
    pub fn apply_configured_agent(&mut self, agent: AgentId) {
        self.configured_agent = agent.clone();
        let empty: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| {
                matches!(tab.backend, TabBackend::NotStarted) && tab.name.is_none() && tab.session.title.is_none()
            })
            .map(|(at, _)| at)
            .collect();
        for at in empty {
            self.tabs[at].agent = agent.clone();
            let mode = self.tabs[at].mode;
            self.give_mode(at, mode);
        }
    }

    /// A launch's saved tabs, ready to come back. A tab saved in bypass is not given back in bypass
    /// unprompted: with [`BypassPolicy::Ask`] and any such tab this returns the question
    /// ([`RestoreStep::Confirm`], answered by [`TabSet::answer_restore`]); otherwise it goes ahead.
    pub fn begin_restore(&mut self, plan: RestorePlan, policy: BypassPolicy) -> RestoreStep {
        self.pending_bypass = None;
        self.pending_restore = None;
        let in_bypass = plan
            .items
            .iter()
            .filter(|i| i.saved_mode == SessionModeChoice::Bypass)
            .count();
        if policy == BypassPolicy::Ask && in_bypass > 0 {
            let nonce = self.next_nonce;
            self.next_nonce += 1;
            let tabs = |n: usize| if n == 1 { "tab" } else { "tabs" };
            let prompt = RestorePrompt {
                nonce,
                lines: vec![
                    format!(
                        "Restore {} {} ({in_bypass} in bypass)? y/n",
                        plan.items.len(),
                        tabs(plan.items.len())
                    ),
                    format!("n brings the bypass {} back in auto", tabs(in_bypass)),
                ],
            };
            self.pending_restore = Some(PendingRestore { nonce, plan });
            return RestoreStep::Confirm(prompt);
        }
        RestoreStep::Go(grant(plan, policy, false))
    }

    /// `y` (`keep_bypass`) or `n` to the question [`TabSet::begin_restore`] asked. Compared against the
    /// stored prompt before it is taken, so an answer to an old prompt cannot cancel a newer one.
    pub fn answer_restore(&mut self, nonce: u64, keep_bypass: bool) -> Result<RestoreGo, String> {
        match &self.pending_restore {
            Some(pending) if pending.nonce == nonce => {}
            _ => return Err("that prompt is no longer current".to_string()),
        }
        let pending = self.pending_restore.take().expect("checked above");
        let policy = if keep_bypass {
            BypassPolicy::Keep
        } else {
            BypassPolicy::Downgrade
        };
        Ok(grant(pending.plan, policy, true))
    }

    /// Starts every cleared tab, in order: the first into `from` (the empty tab the restore began in)
    /// and the rest into new tabs, by the rules a resume by hand follows (a session already open here
    /// is never resumed twice). `connect` begins one resume and hands back where its result will
    /// arrive, or says why it cannot (a session another window holds); it is called once per tab to
    /// start, and a tab it refuses is skipped before any tab is made for it. Afterwards the saved
    /// active tab is the one on screen, or the first that was started when that one was skipped.
    ///
    /// A tab saved in an extra chat view comes back into that view when the window's layout holds it
    /// ([`TabSet::set_layout_views`]) and no tab is bound there yet: the first such tab in saved
    /// order wins. It always comes back as a new tab, never selected and never in `from`, so the base
    /// view's active tab is never one about to be bound. Every other tab comes back in the base view.
    /// The base view then shows the saved active tab if it came back there, else the first tab that
    /// came back there.
    ///
    /// The returned [`RestoreRun`] is fed the results `collect_starts` reports and says when the last
    /// resume has returned.
    pub fn start_restore<F>(&mut self, from: TabId, go: RestoreGo, mut connect: F) -> RestoreRun
    where
        F: FnMut(&AgentId, &str) -> Result<RestoreConnect, String>,
    {
        let total = go.tabs.len() + go.skipped.len();
        let into = self.restore_holder(from);
        let mut run = RestoreRun::new(
            into,
            self.get(into).map_or(self.default_mode, |t| t.mode),
            self.get(into).map_or(AgentId::Claude, |t| t.agent.clone()),
            total,
            go.skipped,
        );
        let mut started: Vec<(String, TabId)> = Vec::new();
        for (n, granted) in go.tabs.into_iter().enumerate() {
            let GrantedTab { planned, mode } = granted;
            let id = planned.provider_session_id.clone();
            if let Some(open) = self.tab_with_session(&planned.agent, &id) {
                run.restored += 1;
                started.push((id, open));
                continue;
            }
            let connected = match connect(&planned.agent, &id) {
                Ok(connected) => connected,
                Err(reason) => {
                    run.failed.push(Skipped {
                        label: planned.label,
                        reason,
                    });
                    continue;
                }
            };
            let into_view = self.restored_view(&planned);
            let target = match into_view {
                Some(view) => self.insert_restored_into(planned.agent.clone(), view),
                None => {
                    let ResumeRoute::StartIn(target) = self.route_resume_told(into, &planned.agent, &id, false, false)
                    else {
                        continue;
                    };
                    target
                }
            };
            let at = self.position(target).expect("route_resume names a tab it has");
            self.tabs[at].agent = planned.agent;
            self.give_mode(at, mode);
            let tab = &mut self.tabs[at];
            let request_id = format!("restore-{}", n + 1);
            match connected {
                RestoreConnect::Started(result_rx) => {
                    tab.backend = TabBackend::Starting(PendingStart {
                        request_id,
                        result_rx,
                        first_turn: None,
                        resume: Some(id.clone()),
                        resumed_title: planned.title,
                        resumed_name: planned.name,
                    });
                    run.pending.insert(target, planned.label);
                }
                // Back as a tab holding its session, so it is listed among the saved tabs while it
                // waits, and counted as brought back: its start is now that tab's own, as a resume by
                // hand would be.
                RestoreConnect::AwaitTrust => {
                    tab.backend = TabBackend::AwaitingTrust(AwaitingTrust {
                        start: DeferredStart::Resume {
                            request_id,
                            provider_session_id: id.clone(),
                            resumed_title: planned.title,
                            resumed_name: planned.name,
                        },
                        asking: None,
                        recording: false,
                    });
                    run.restored += 1;
                    run.awaiting.push(target);
                }
            }
            // The fallback for a failed active tab must be a tab `select` accepts: one bound to an
            // extra view is already on screen there and never becomes the base view's active tab.
            if into_view.is_none() {
                run.order.push(target);
            }
            started.push((id, target));
        }
        // Only a tab that came back in the base view can be its active tab: one bound to an extra
        // view is already on screen there.
        started.retain(|(_, tab)| self.view_of(*tab).is_base());
        let on_screen = go
            .active_session
            .as_ref()
            .and_then(|wanted| started.iter().find(|(id, _)| id == wanted))
            .or_else(|| started.first())
            .map(|(_, tab)| *tab);
        if let Some(tab) = on_screen {
            self.select(tab);
        }
        debug_assert!(self.v1_holds());
        run
    }

    /// The extra view a restored tab comes back into: the one it was saved in, when the window's
    /// layout holds that view and no tab is bound there yet. `None` is the base view.
    fn restored_view(&self, planned: &PlannedTab) -> Option<ViewId> {
        planned
            .view
            .filter(|view| self.layout_views.contains(view) && self.tab_in(*view).is_none())
    }

    /// A new tab for a restored session, bound to `view`. Never the tab the restore began in and never
    /// selected, so the base view's active tab is never one about to be bound.
    fn insert_restored_into(&mut self, agent: AgentId, view: ViewId) -> TabId {
        let target = self.insert_tab(agent, false);
        let at = self.position(target).expect("just inserted");
        self.tabs[at].view = view;
        eprintln!(
            "[tabs] tab {} restored into agent {}",
            self.tabs[at].number,
            view.number()
        );
        target
    }

    /// Whether the restore's bypass question is open (asked in the base page, not yet answered).
    pub fn restore_question_open(&self) -> bool {
        self.pending_restore.is_some()
    }

    /// The extra chat views the window's layout holds, in any workspace. The window says so before any
    /// restore, so a restored tab saved in one of them can come back into it.
    pub fn set_layout_views(&mut self, views: Vec<ViewId>) {
        let mut views: Vec<ViewId> = views.into_iter().filter(|view| !view.is_base()).collect();
        views.sort();
        views.dedup();
        self.layout_views = views;
    }

    /// The tab a restore asked from `from` routes its base-view sessions into. The tab the restore was
    /// asked from can have moved to an extra view while the restore waited (the trust question): its
    /// base-view sessions then come back into the base view's active tab instead, which is reused when
    /// empty like any tab a resume starts from.
    fn restore_holder(&self, from: TabId) -> TabId {
        if self.view_of(from).is_base() {
            from
        } else {
            self.active
        }
    }

    /// A restore whose start was put off (a trust question answered `Escape`, too large to show, or
    /// refused): its sessions come back as failed tabs holding their resume ids, the first in `from`
    /// (the base view's active tab when `from` has since been broken out) and the rest in new tabs, so the saved-tabs file keeps them until the user closes a tab or
    /// starts it over with `r`. Without this the restore's sessions would be in no tab and the next
    /// write of the file would leave them out. A session already open in a tab is not listed twice.
    /// A tab saved in an extra chat view comes back into it under [`TabSet::start_restore`]'s rule, and
    /// the base view then shows the saved active tab if it came back there, else its first.
    ///
    /// A dashboard restore (`RestoreGo::reoffer`) is the exception when the window is otherwise empty
    /// (its tab already is): the file was never written, so the tab is only emptied and the offer stands,
    /// shown again by the dashboard. A tab whose session is still starting has nothing saved yet but will
    /// write the file once its id arrives, so it counts. Returns whether the offer stands; when not, the
    /// saved tabs are no longer on offer.
    pub fn keep_put_off_restore(&mut self, from: TabId, go: RestoreGo, reason: &str) -> bool {
        if go.reoffer && self.is_pristine() {
            return true;
        }
        let from = self.restore_holder(from);
        let mut kept: Vec<(String, TabId)> = Vec::new();
        for granted in go.tabs {
            let GrantedTab { planned, mode } = granted;
            let id = planned.provider_session_id.clone();
            if self.tab_with_session(&planned.agent, &id).is_some() {
                continue;
            }
            // Into its saved view by the rule a started restore follows, so the next write of the
            // file still names that view.
            let target = match self.restored_view(&planned) {
                Some(view) => self.insert_restored_into(planned.agent.clone(), view),
                None => {
                    let ResumeRoute::StartIn(target) = self.route_resume_told(from, &planned.agent, &id, false, false)
                    else {
                        continue;
                    };
                    target
                }
            };
            let at = self.position(target).expect("route_resume names a tab it has");
            self.tabs[at].agent = planned.agent;
            self.give_mode(at, mode);
            let tab = &mut self.tabs[at];
            tab.failed_after_start = false;
            tab.failed_resume = Some(id.clone());
            if tab.name.is_none() {
                tab.name = planned.name;
            }
            tab.backend = TabBackend::Failed {
                reason: reason.to_string(),
            };
            kept.push((id, target));
        }
        kept.retain(|(_, tab)| self.view_of(*tab).is_base());
        let on_screen = go
            .active_session
            .as_ref()
            .and_then(|wanted| kept.iter().find(|(id, _)| id == wanted))
            .or_else(|| kept.first())
            .map(|(_, tab)| *tab);
        if let Some(tab) = on_screen {
            self.select(tab);
        }
        debug_assert!(self.v1_holds());
        false
    }

    /// A restored tab whose resume failed. `back_to` is [`RestoreRun::back_to`]: an empty tab again, in
    /// the mode it had before the restore, if it is the one the restore began in; gone if the restore
    /// made it. What went wrong is reported once for the whole restore, not left as a dead tab each.
    pub fn discard_failed_restore(&mut self, tab: TabId, back_to: Option<(SessionModeChoice, AgentId)>) {
        if !matches!(self.get(tab).map(|t| &t.backend), Some(TabBackend::Failed { .. })) {
            return;
        }
        match back_to {
            Some((mode, agent)) => {
                if let Some(at) = self.position(tab) {
                    let t = &mut self.tabs[at];
                    t.backend = TabBackend::NotStarted;
                    t.session.reported_start_failure = false;
                    t.agent = agent;
                    self.give_mode(at, mode);
                }
            }
            None => {
                self.remove(tab);
            }
        }
        debug_assert!(self.v1_holds());
    }
}

/// What the saved-tabs file records of a tab's view: its number for an extra view, nothing for the
/// base view, so a window with no chat view writes what it always wrote.
fn saved_view(view: ViewId) -> Option<u8> {
    (!view.is_base()).then(|| view.number())
}

/// The modes a restore's tabs get: saved as they were, except that bypass is given back only when
/// the policy says so.
fn grant(plan: RestorePlan, policy: BypassPolicy, asked: bool) -> RestoreGo {
    let mut downgraded = 0;
    let tabs = plan
        .items
        .into_iter()
        .map(|planned| {
            let mode = match (planned.saved_mode, policy) {
                (SessionModeChoice::Bypass, BypassPolicy::Keep) => SessionModeChoice::Bypass,
                (SessionModeChoice::Bypass, _) => {
                    downgraded += 1;
                    SessionModeChoice::Auto
                }
                (mode, _) => mode,
            };
            GrantedTab { planned, mode }
        })
        .collect();
    RestoreGo {
        tabs,
        skipped: plan.skipped,
        active_session: plan.active_session,
        downgraded,
        asked,
        reoffer: false,
    }
}

/// The exact `(tool_name, input)` a rule's or the fast path's own answer saw, kept alongside a
/// candidate so a later card for the very same call that names no tool-use id can still be matched
/// by content and drop the candidate: the sidecar sends an empty tool-use id when the CLI gave none,
/// and such a card has no id for the ordinary "same id" removal to find. Equality is exact (`Value`
/// equality, which ignores object key order), as `HumanApprovals` compares calls: small, short-lived
/// sets, and no canonical form to get wrong.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct CandidateCall {
    tool_name: String,
    input: serde_json::Value,
}

/// Which note a candidate becomes on its call's row once the call completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
enum NoteKind {
    /// A saved prefix rule answered the call's gate: "allowed by rule `Bash(npm ci *)`".
    Rule,
    /// The acceptEdits fast path answered the call's gate: "allowed by auto".
    AutoEdit,
    /// The CLI's own prompt for the call was answered without a card, with the note to show.
    PromptNote,
}

impl NoteKind {
    /// Whether a later card for the same call means a human answered after all. A gate answered by a
    /// rule or the fast path can still be followed by the CLI's own prompt for the call, drawn as a
    /// card; a CLI prompt answered without a card is the last question about its call.
    fn dropped_by_a_later_card(self) -> bool {
        !matches!(self, NoteKind::PromptNote)
    }
}

/// What one candidate turns into, and the call a later card is matched against by content.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum NoteCandidate {
    Rule { rule: String, call: CandidateCall },
    AutoEdit(CandidateCall),
    PromptNote(String),
}

impl NoteCandidate {
    fn kind(&self) -> NoteKind {
        match self {
            NoteCandidate::Rule { .. } => NoteKind::Rule,
            NoteCandidate::AutoEdit(_) => NoteKind::AutoEdit,
            NoteCandidate::PromptNote(_) => NoteKind::PromptNote,
        }
    }

    fn call(&self) -> Option<&CandidateCall> {
        match self {
            NoteCandidate::Rule { call, .. } | NoteCandidate::AutoEdit(call) => Some(call),
            NoteCandidate::PromptNote(_) => None,
        }
    }

    /// Writes this candidate's note for call `id` into `notes`.
    fn note_into(self, id: String, notes: &mut CallNotes) {
        match self {
            NoteCandidate::Rule { rule, .. } => {
                notes.allowed_by_rule.insert(id, rule);
            }
            NoteCandidate::AutoEdit(_) => {
                notes.allowed_by_auto.insert(id);
            }
            NoteCandidate::PromptNote(note) => {
                notes.prompt_notes.insert(id, note);
            }
        }
    }
}

/// What a host answer reports for the ledger. `answer_what_needs_no_human` reports all three;
/// `approve_pending` answers only cards and reports only the CLI prompts among them, so the other two
/// stay empty.
#[derive(Default)]
struct NoteReport {
    prompts: Vec<crate::agent_backend::PromptAnsweredForYou>,
    by_the_fast_path: Vec<crate::agent_backend::FastPathAnsweredForYou>,
    by_rule: Vec<crate::agent_backend::RuleAnsweredForYou>,
}

impl NoteReport {
    fn prompts(prompts: Vec<crate::agent_backend::PromptAnsweredForYou>) -> NoteReport {
        NoteReport {
            prompts,
            ..NoteReport::default()
        }
    }
}

/// Calls answered without a card, each waiting for its call to complete before its row gets a note
/// saying who answered. Only what the host reports it answered is recorded, at the answer itself, and
/// never inferred from a card's absence: a call can fail the CLI's own validation before any gate
/// runs, or a switch to bypass can answer the gate instead, and either would then read as allowed by
/// something that never allowed it.
///
/// Keyed by call and kind, since one call can wait for more than one note (a gate a rule answered,
/// then the CLI's own prompt for it). Every way a candidate ends goes through here: a completed call
/// turns it into its note ([`NoteLedger::fold`], [`NoteLedger::resync`]); a later card for the same
/// call drops a gate's candidate, since a human answered too; and the whole ledger goes with the
/// session that answered ([`NoteLedger::clear`]).
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct NoteLedger {
    /// Written as a list of entries: a JSON object's keys are strings, and these keys are pairs.
    #[serde(with = "ledger_entries")]
    candidates: std::collections::BTreeMap<(String, NoteKind), NoteCandidate>,
}

/// [`NoteLedger::candidates`] as a list of `(key, candidate)` entries, in key order.
mod ledger_entries {
    use super::{NoteCandidate, NoteKind};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    type Entries = BTreeMap<(String, NoteKind), NoteCandidate>;

    pub(super) fn serialize<S: Serializer>(entries: &Entries, serializer: S) -> Result<S::Ok, S::Error> {
        entries.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Entries, D::Error> {
        Ok(Vec::<((String, NoteKind), NoteCandidate)>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

impl NoteLedger {
    /// Notes that call `id` was answered without a card; a second answer of the same kind for the
    /// same call replaces the first.
    fn record(&mut self, id: String, candidate: NoteCandidate) {
        self.candidates.insert((id, candidate.kind()), candidate);
    }

    /// Records everything one host answer reports.
    fn record_report(&mut self, report: NoteReport) {
        for answered in report.prompts {
            self.record(answered.tool_use_id, NoteCandidate::PromptNote(answered.note));
        }
        for answered in report.by_the_fast_path {
            let call = CandidateCall {
                tool_name: answered.tool_name,
                input: answered.input,
            };
            self.record(answered.tool_use_id, NoteCandidate::AutoEdit(call));
        }
        for answered in report.by_rule {
            let call = CandidateCall {
                tool_name: answered.tool_name,
                input: answered.input,
            };
            let rule = answered.rule;
            self.record(answered.tool_use_id, NoteCandidate::Rule { rule, call });
        }
    }

    /// Folds one batch of events in order. A `PermissionRequested` drops the gate candidate of each
    /// kind it matches ([`NoteLedger::dropped_by_card`]). A `ToolCallCompleted` turns every candidate
    /// of its call into a note, kept in `kept` for later snapshots and added to `fresh`, this batch's
    /// own.
    fn fold(&mut self, events: &[AgentDomainEvent], kept: &mut CallNotes, fresh: &mut CallNotes) {
        for event in events {
            // A subagent's calls are answered by the same gate, so their notes land on their rows too.
            let event = match event {
                AgentDomainEvent::SubagentFrame { frame, .. } => frame.as_ref(),
                event => event,
            };
            match event {
                AgentDomainEvent::PermissionRequested {
                    tool_use_id,
                    tool_name,
                    input,
                    ..
                } => {
                    for kind in [NoteKind::Rule, NoteKind::AutoEdit] {
                        if let Some(key) = self.dropped_by_card(kind, tool_use_id, tool_name, input) {
                            self.candidates.remove(&key);
                        }
                    }
                }
                AgentDomainEvent::ToolCallCompleted { tool_use_id, .. } => {
                    for kind in [NoteKind::Rule, NoteKind::AutoEdit, NoteKind::PromptNote] {
                        if let Some(candidate) = self.candidates.remove(&(tool_use_id.clone(), kind)) {
                            candidate.clone().note_into(tool_use_id.clone(), kept);
                            candidate.note_into(tool_use_id.clone(), fresh);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The gate candidate of `kind` a card drops: the one naming the card's own tool-use id; or, for a
    /// card naming none, the first in id order whose call has the card's exact tool and input. Two
    /// identical concurrent calls answered the same way could match the wrong one of the two, but
    /// their rows' notes would read the same either way.
    fn dropped_by_card(
        &self,
        kind: NoteKind,
        tool_use_id: &Option<String>,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Option<(String, NoteKind)> {
        if let Some(id) = tool_use_id {
            let key = (id.clone(), kind);
            return self.candidates.contains_key(&key).then_some(key);
        }
        self.candidates
            .iter()
            .find(|((_, k), candidate)| {
                *k == kind
                    && candidate
                        .call()
                        .is_some_and(|call| call.tool_name == tool_name && call.input == *input)
            })
            .map(|(key, _)| key.clone())
    }

    /// What a `Resync` does in place of the events it dropped, from the projection read once. A
    /// candidate whose call finished becomes its note in `kept`, so the snapshot sent next carries it.
    /// A gate candidate whose call still has a pending card nobody answered for the user is dropped:
    /// the card that would have dropped it was among the dropped events, and whatever answers that
    /// card now is a human. A request this tab answered itself (`host_answered`) can still be listed
    /// as pending until the provider's resolution folds, and is no card: it is left out, or a
    /// `Resync` that merely came early would drop an answer recorded at the answer itself
    /// (`a_resync_before_the_call_finishes_keeps_its_fast_path_answer` and `..._keeps_its_rule_answer`
    /// each fail without that filter).
    fn resync(
        &mut self,
        projection: &agent::AgentSessionProjection,
        host_answered: &BTreeSet<String>,
        kept: &mut CallNotes,
    ) {
        let finished: Vec<(String, NoteKind)> = self
            .candidates
            .keys()
            .filter(|(id, _)| {
                projection
                    .tool_calls
                    .iter()
                    .any(|call| &call.tool_use_id == id && call.result.is_some())
            })
            .cloned()
            .collect();
        for key in finished {
            if let Some(candidate) = self.candidates.remove(&key) {
                candidate.note_into(key.0, kept);
            }
        }
        let still_pending: BTreeSet<&str> = projection
            .pending_permissions
            .values()
            .filter(|p| !host_answered.contains(&p.permission_id))
            .filter_map(|p| p.tool_use_id.as_deref())
            .collect();
        self.candidates
            .retain(|(id, kind), _| !(kind.dropped_by_a_later_card() && still_pending.contains(id.as_str())));
    }

    /// Forgets every candidate: the session that answered them is gone.
    fn clear(&mut self) {
        self.candidates.clear();
    }

    #[cfg(test)]
    fn get(&self, id: &str, kind: NoteKind) -> Option<&NoteCandidate> {
        self.candidates.get(&(id.to_string(), kind))
    }

    #[cfg(test)]
    fn has(&self, id: &str, kind: NoteKind) -> bool {
        self.get(id, kind).is_some()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    #[cfg(test)]
    fn count(&self, kind: NoteKind) -> usize {
        self.candidates.keys().filter(|(_, k)| *k == kind).count()
    }

    #[cfg(test)]
    fn prompt_note(&self, id: &str) -> Option<&str> {
        match self.get(id, NoteKind::PromptNote)? {
            NoteCandidate::PromptNote(note) => Some(note),
            _ => None,
        }
    }
}

/// The `Write` cards in this batch whose file does not exist now, when the card is
/// raised -- recorded in `notes` for later snapshots and returned as this batch's own. A relative
/// `file_path` (the tool asks for an absolute one) is read against the project root. Checked once,
/// here: after an approval the file exists, and the card must go on meaning what it meant.
fn note_new_files(
    notes: &mut std::collections::BTreeMap<String, Option<String>>,
    events: &[AgentDomainEvent],
    project_root: &Path,
) -> std::collections::BTreeMap<String, Option<String>> {
    let mut fresh = std::collections::BTreeMap::new();
    for event in events {
        let AgentDomainEvent::PermissionRequested {
            permission_id,
            tool_use_id,
            tool_name,
            input,
            ..
        } = event
        else {
            continue;
        };
        if tool_name != "Write" {
            continue;
        }
        let Some(file_path) = input
            .get("file_path")
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty())
        else {
            continue;
        };
        if crate::agent_backend::file_absent(&project_root.join(file_path)) {
            notes.insert(permission_id.clone(), tool_use_id.clone());
            fresh.insert(permission_id.clone(), tool_use_id.clone());
        }
    }
    fresh
}

/// Recomputes `rule_offers` when the set of pending permission ids changed; `true` if the offers did.
/// Only `projection()` is read, once, and released before anything else is touched.
/// The delivered cards this tab could approve right now: its own attention
/// tray, intersected with the projection's still-pending ids and with anything it has not already
/// host-answered, `seq`-ordered (the order the panel shows them in). The CLI's own prompts are among
/// them whatever they say about why they asked: bypass approves every card. Empty unless the tab is
/// `Live` -- a `NotStarted`/`Starting`/`Failed` tab has no session to hold a pending request at all.
///
/// This one function builds every `BypassPlan::approve` and every re-check `confirm_bypass`
/// makes against a stored plan: both need exactly the same answer to "what would `y` approve right
/// now", so there is one place that answer comes from.
fn waiting_cards(tab: &Tab) -> Vec<String> {
    let Some(backend) = tab.live() else {
        return Vec::new();
    };
    let cards: BTreeSet<String> = tab.attention.card_ids().into_iter().collect();
    // Its own statement, guard dropped at the end of the block: the lock-order rule this whole
    // module inherits (`projection()` before any `respond_permission`-adjacent call).
    let mut ordered: Vec<(u64, String)> = {
        let projection = backend.projection();
        projection
            .pending_permissions
            .values()
            .filter(|p| cards.contains(&p.permission_id) && !tab.session.host_answered.contains(&p.permission_id))
            .map(|p| (p.seq, p.permission_id.clone()))
            .collect()
    };
    ordered.sort_by_key(|(seq, _)| *seq);
    ordered.into_iter().map(|(_, id)| id).collect()
}

/// What one [`TabSet::pump_views`] shares across its tabs, borrowed from the set beside its tabs. The
/// turn review is not in it: it is handed only to the steps that feed it, so the arm that answers
/// permissions cannot reach it.
struct PumpTick<'a> {
    project_root: &'a Path,
    rules: &'a agent::PrefixRules,
    ui_wake: Option<&'a agent::UiWake>,
    retiring: &'a mut RetiringCounts,
    starting: &'a mut BTreeSet<u64>,
    starting_since: &'a mut BTreeMap<u64, u64>,
    /// The prompts held for the presence: their tabs stay in `starting` whatever is folded.
    held_sends: &'a BTreeMap<u64, moving::PendingSend>,
    /// `TabSet::untracked_starts`.
    untracked_starts: &'a mut BTreeSet<u64>,
    #[cfg(test)]
    after_drain: &'a mut Option<BackendSeam>,
    #[cfg(test)]
    redeliver_covered: bool,
    #[cfg(test)]
    before_resync_snapshot: &'a mut Option<BackendSeam>,
}

/// Where a tab is shown during one pump.
#[derive(Clone, Copy)]
struct PumpPlace {
    /// The base view's active tab.
    is_active: bool,
    /// Shown in some view's page: the base view's active tab, or a tab bound to an extra view.
    in_a_page: bool,
    /// Shown in a page that is mapped now.
    on_screen: bool,
}

/// The parts of a live tab a pump arm works on, borrowed apart so the backend is used beside them.
struct LiveTab<'a> {
    following: bool,
    id: TabId,
    mode: SessionModeChoice,
    backend: &'a mut AgentBackend,
    session: &'a mut SessionState,
    notes: &'a mut CallNotes,
    attention: &'a mut AttentionTracker,
}

/// What a pump arm leaves for the rest of its tab's tick.
#[derive(Default)]
struct ArmOutcome {
    /// The tab's turn went from running to not running.
    turn_ended: bool,
    /// This tab's payload for the page that shows it, and its class.
    delivered: Option<(String, crate::panel_cadence::EnvelopeClass)>,
    /// The tab's turn trace saw its first text in this batch, in a tab a page shows.
    saw_first_text: bool,
    /// The batch held a turn's start, and no turn review saw it.
    untracked_start: bool,
}

fn refresh_offers(tab: &mut Tab, project_root: &Path) -> bool {
    let Some(backend) = tab.live() else {
        let had = !tab.session.rule_offers.is_empty();
        tab.session.rule_offers.clear();
        tab.session.rule_offers_seen.clear();
        return had;
    };
    let pending: Vec<(String, String, serde_json::Value)> = {
        let projection = backend.projection();
        // The CLI's own prompts are never offered a rule: no saved rule answers one,
        // and the CLI itself does not let a rule silence its check.
        // A card the user already answered is never offered a rule again (`user_answered`'s doc).
        let mut cards: Vec<&agent::PermissionRequestRecord> = projection
            .pending_permissions
            .values()
            .filter(|p| p.provider_prompt.is_none() && !tab.session.user_answered.contains(&p.permission_id))
            .collect();
        cards.sort_by(|a, b| a.permission_id.cmp(&b.permission_id));
        // Compared by id before anything is copied: this runs on every tick, and a card's input
        // can be a whole file's content.
        let same = cards.len() == tab.session.rule_offers_seen.len()
            && cards
                .iter()
                .zip(&tab.session.rule_offers_seen)
                .all(|(p, seen)| &p.permission_id == seen);
        if same {
            return false;
        }
        cards
            .into_iter()
            .map(|p| (p.permission_id.clone(), p.tool_name.clone(), p.input.clone()))
            .collect()
    };
    tab.session.rule_offers_seen = pending.iter().map(|p| p.0.clone()).collect();
    let offers: std::collections::BTreeMap<String, agent::PrefixRule> = pending
        .into_iter()
        .filter_map(|(id, tool, input)| agent::permission_rules::offer(&tool, &input, project_root).map(|r| (id, r)))
        .collect();
    let changed = offers != tab.session.rule_offers;
    tab.session.rule_offers = offers;
    changed
}

/// Keeps `clock` in step with the projection's running turn: stamped `now_ms` the first time a
/// turn id is seen, kept while that id runs, cleared when none does.
///
/// A turn the clock was stamped for that stops running -- ended well or badly, or replaced by the
/// next turn's id -- is appended to `times` (and counted in `unsent`), so the panel can say how long
/// it took. A turn nobody stamped (it began before this process, or inside one tick) leaves no entry.
fn observe_turn_clock(
    clock: &mut Option<(String, u64)>,
    times: &mut Vec<TurnTime>,
    unsent: &mut usize,
    projection: &agent::AgentSessionProjection,
    now_ms: u64,
) {
    let active_turn_id = projection.active_turn_id.as_deref();
    if active_turn_id.is_some_and(|id| clock.as_ref().is_some_and(|(seen, _)| seen == id)) {
        return;
    }
    if let Some((turn_id, started_at_ms)) = clock.take() {
        times.push(TurnTime {
            turn_id,
            started_at_ms,
            ended_at_ms: now_ms.max(started_at_ms),
            // The newest row on the page is where the turn stopped -- unless another turn is already
            // running, whose rows may be the newest by now: then no row can be named, and the panel
            // shows no duration line for this turn.
            anchor_seq: if active_turn_id.is_none() {
                newest_item_seq(projection)
            } else {
                None
            },
        });
        *unsent += 1;
        if times.len() > MAX_TURN_TIMES {
            let excess = times.len() - MAX_TURN_TIMES;
            times.drain(..excess);
            *unsent = (*unsent).min(MAX_TURN_TIMES);
        }
    }
    if let Some(id) = active_turn_id {
        *clock = Some((id.to_string(), now_ms));
    }
}

/// Whether the tab's agent reads a message starting with `/` as a command of its own: Claude always, and
/// any agent that listed its commands (an ACP agent's `available_commands_update`). Such a message is never
/// put into a running turn, where it would arrive as text; it waits for the turn's end and goes out as a
/// turn of its own, as Claude Code runs a command typed while it works (`next_turn`). Codex lists none, and takes a
/// message into its turn as typed.
fn reads_slash_commands(agent: &AgentId, projection: &agent::AgentSessionProjection) -> bool {
    matches!(agent, AgentId::Claude)
        || projection
            .available_commands
            .as_ref()
            .is_some_and(|list| !list.is_empty())
}

/// Where in the queue the next turn comes from: the first item the CLI did not refuse when it is a command, or
/// else every message up to the next command. So what waits goes out in the order typed, and a command is never
/// read with a message for its arguments. Ascending.
fn next_turn(queue: &[Queued], reads_commands: bool) -> Vec<usize> {
    let mut turn = Vec::new();
    for (index, item) in queue.iter().enumerate().filter(|(_, item)| !item.refused) {
        if item.is_command(reads_commands) || item.is_shell() {
            if turn.is_empty() {
                turn.push(index);
            }
            break;
        }
        turn.push(index);
    }
    turn
}

/// Whether typed text is a slash command: it starts with `/`, after any leading blanks (the page's `/`
/// menu reads it the same way, and holding a message that was not one only delays it to the turn's end).
fn is_slash_command(text: &str) -> bool {
    text.trim_start().starts_with('/')
}

/// The `seq` of the newest row the agent wrote (a reply, a thought, a call, a card or an ending):
/// the one a turn that just ended finished on. The
/// user's prompts are left out, because a queued message can be sent the moment a turn ends, and
/// the line saying how long that turn took belongs above it, not under it.
fn newest_item_seq(projection: &agent::AgentSessionProjection) -> Option<u64> {
    [
        projection.transcript.last().map(|m| m.seq),
        projection.thinking.last().map(|t| t.seq),
        projection.tool_calls.last().map(|c| c.seq),
        projection.turn_endings.last().map(|e| e.seq),
        projection.pending_permissions.values().map(|p| p.seq).max(),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// How many finished turns' durations a tab remembers; the oldest are dropped first.
const MAX_TURN_TIMES: usize = 1000;

/// Why [`TabSet::install_turn_review`] refused, as the panel says it.
pub const TURN_REVIEW_NEEDS_SIDECAR: &str = "turn review needs the sidecar backend";

/// What a request for a review says when `init.lua` switched turn review off.
pub const TURN_REVIEW_OFF: &str = "turn review is off (init.lua: review.enabled = false)";

/// Milliseconds since the Unix epoch: the same clock the panel's `Date.now()` reads.
fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_providers::RecordingProvider;
    use agent::{AgentConversation, AgentDomainEvent};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// The start owes this line before its backend exists, and a page may not have shown it by the time the
    /// backend lands. What an earlier session owed or said ends with that session.
    #[test]
    fn a_line_the_start_owes_outlives_the_landing() {
        let landed = |note| {
            let session = SessionState {
                foreign_server_note: note,
                ..SessionState::default()
            };
            session.for_installed_backend().foreign_server_note
        };
        assert_eq!(
            landed(ForeignServerNote::NameTakenOwed(NAME_UNKNOWN_LINE)),
            ForeignServerNote::NameTakenOwed(NAME_UNKNOWN_LINE)
        );
        assert_eq!(
            landed(ForeignServerNote::NameTakenSaid(NAME_TAKEN_LINE)),
            ForeignServerNote::Unseen
        );
        assert_eq!(landed(ForeignServerNote::Owed), ForeignServerNote::Unseen);
        assert_eq!(landed(ForeignServerNote::Said), ForeignServerNote::Unseen);
    }

    fn workspace(label: &str) -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        dir
    }

    fn live(dir: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
        let provider = Arc::new(RecordingProvider::default());
        let conversation =
            AgentConversation::create(provider.clone(), dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        (
            provider,
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            },
        )
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn read(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
            provider_prompt: None,
        }
    }

    /// `.git/main.rs` is a protected `Write` target that reliably produces a card. An ordinary
    /// in-project write would be answered by the acceptEdits fast path, leaving queueing, attention
    /// and confirmation tests waiting for a nonexistent card. Those mechanisms need an asking
    /// request; the fast path has separate tests in `agent::permission_policy`. Using `.git/`
    /// keeps this fixture recognizably a write.
    fn write(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
            provider_prompt: None,
        }
    }

    /// A `Read` OUTSIDE the project root: still cards under the classifier, and bypass still
    /// answers it (bypass never consults the path boundary at all).
    fn outside(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "/etc/hostname" }),
            provider_prompt: None,
        }
    }

    #[test]
    fn a_new_tab_takes_the_default() {
        let mut set = set();
        let first = set.active();
        assert!(!set.get(first).unwrap().following);
        set.set_following_default(true);
        let second = set.open(AgentId::Claude);
        assert!(set.get(second).unwrap().following);
        assert!(!set.get(first).unwrap().following);
        set.set_following_default(false);
        let third = set.open(AgentId::Codex);
        assert!(!set.get(third).unwrap().following);
        assert!(set.get(second).unwrap().following);
    }

    #[test]
    fn the_toggle_is_per_tab() {
        let mut set = set();
        let first = set.active();
        let second = set.open(AgentId::Claude);
        assert!(set.toggle_following(first));
        assert!(set.get(first).unwrap().following);
        assert!(!set.get(second).unwrap().following);
        set.get_mut(first)
            .unwrap()
            .session
            .client_calls
            .edited
            .push(PathBuf::from("a.rs"));
        assert!(set.toggle_following(first));
        assert!(!set.get(first).unwrap().following);
        assert!(set.take_follow_edits().is_empty());
        assert!(!set.toggle_following(TabId(u64::MAX)));
        set.freeze_for_move(first).unwrap();
        assert!(!set.toggle_following(first));
        assert!(!set.get(first).unwrap().following);
    }

    fn set() -> TabSet {
        TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto)
    }

    /// Unwraps a `Confirm` plan out of `cycle_mode`/`cycle_default_mode`'s `Result<ModeCycle, _>`,
    /// panicking with the actual value on anything else -- every bypass-entry test wants the plan,
    /// never `Changed` or an `Err`.
    fn plan(result: Result<ModeCycle, String>) -> BypassPlan {
        match result {
            Ok(ModeCycle::Confirm(plan)) => plan,
            other => panic!("expected Ok(ModeCycle::Confirm(_)), got {other:?}"),
        }
    }

    fn shut_down_all(set: &mut TabSet) {
        for tab in set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// An idle set needs no pumping; a live backend needs it only until it announces its folds, and then
    /// each fold calls the wake, after which the pump finds it. A connect running on a worker needs pumping.
    #[test]
    fn a_set_needs_pumping_only_for_what_cannot_announce_itself() {
        let dir = workspace("tabs-needs-polling");
        let mut set = set();
        assert!(!set.needs_polling(), "an empty tab");
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert!(set.needs_polling(), "a live backend nobody is told about");

        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let wakes = wakes.clone();
            set.set_ui_wake(Arc::new(move || {
                wakes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
        }
        assert!(!set.needs_polling(), "the backend announces its folds now");
        let installed = wakes.load(std::sync::atomic::Ordering::SeqCst);
        assert!(installed >= 1, "installing the wake takes in what was folded before");

        provider.queue(started("t1"));
        until("the wake after the fold", || {
            wakes.load(std::sync::atomic::Ordering::SeqCst) > installed
        });
        let out = set.pump(&dir, true);
        assert!(out.active_payload.is_some(), "the woken pump finds the fold");
        assert!(!set.needs_polling());

        let other = set.open(AgentId::Claude);
        let (_tx, result_rx) = mpsc::channel();
        set.get_mut(other).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "req-1".into(),
            result_rx,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        });
        assert!(set.needs_polling(), "a connect on a worker reports through a channel");
        shut_down_all(&mut set);
    }

    /// A terminal handoff's close runs on a worker that reports through channels nobody is told about: the
    /// set needs pumping until the handoff is collected, and not after.
    #[test]
    fn a_handoff_in_flight_needs_pumping() {
        let mut set = set();
        assert!(!set.needs_polling());
        let tab = set.active();
        let (_closed_tx, closed_rx) = mpsc::channel();
        let (_dropped_tx, dropped_rx) = mpsc::channel();
        set.get_mut(tab).unwrap().pending_handoff = Some(PendingHandoff {
            request_id: "req-handoff".into(),
            command: agent::handoff::ClaudeResumeCommand::for_session("/home/user/project", "1857dcd5-973b-46a2")
                .unwrap(),
            closed_rx,
            dropped_rx,
        });
        assert!(set.needs_polling(), "a handoff's close is still running");
        set.get_mut(tab).unwrap().pending_handoff = None;
        assert!(!set.needs_polling(), "collected");
    }

    /// A tab resumed with its record's title takes the CLI's title once it is known; a tab titled by its
    /// own first prompt keeps it.
    #[test]
    fn a_title_read_later_replaces_a_resumed_tabs_record_title() {
        let dir = workspace("tabs-retitle");
        let mut set = set();
        let resumed = set.active();
        let (provider, backend) = live(&dir);
        let (tx, result_rx) = mpsc::channel();
        set.get_mut(resumed).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "req-resume".into(),
            result_rx,
            first_turn: None,
            resume: Some("s-resumed".into()),
            resumed_title: Some("first prompt".into()),
            resumed_name: None,
        });
        tx.send(Ok(backend)).unwrap();
        assert_eq!(set.collect_starts().len(), 1);
        provider.open_session("s-resumed", &dir);
        until("the session id", || {
            set.get(resumed)
                .unwrap()
                .live()
                .and_then(AgentBackend::provider_session_id)
                .is_some()
        });
        let fresh = set.open(AgentId::Claude);
        let (other_provider, other) = live(&dir);
        set.get_mut(fresh).unwrap().backend = TabBackend::Live(other);
        set.get_mut(fresh).unwrap().session.title = Some("its own prompt".into());
        other_provider.open_session("s-fresh", &dir);
        until("the other session id", || {
            set.get(fresh)
                .unwrap()
                .live()
                .and_then(AgentBackend::provider_session_id)
                .is_some()
        });

        let titles = |id: &str| Some(format!("CLI title of {id}"));
        assert!(!set.retitle_from(|_| None), "nothing known yet");
        assert!(set.retitle_from(titles));
        assert_eq!(
            set.get(resumed).unwrap().session.title.as_deref(),
            Some("CLI title of s-resumed")
        );
        assert_eq!(set.get(fresh).unwrap().session.title.as_deref(), Some("its own prompt"));
        assert!(!set.retitle_from(titles), "the same title again changes nothing");
        assert!(
            !set.retitle_from(|_| Some("retitled again".into())),
            "a later retitle is not followed, as the read at the resume was not"
        );
        assert_eq!(
            set.get(resumed).unwrap().session.title.as_deref(),
            Some("CLI title of s-resumed")
        );
        assert!(!set.get(resumed).unwrap().session.title_from_record);
        // A record whose title already was the CLI's (the cache was warm at the resume): kept, and done.
        set.get_mut(resumed).unwrap().session.title_from_record = true;
        assert!(!set.retitle_from(|_| Some("CLI title of s-resumed".into())));
        assert!(!set.get(resumed).unwrap().session.title_from_record);
        shut_down_all(&mut set);
    }

    #[test]
    fn a_new_window_has_one_empty_tab_numbered_one() {
        let set = set();
        assert_eq!(set.tabs().len(), 1);
        let tab = set.active_tab();
        assert_eq!(tab.number, 1);
        assert!(matches!(tab.backend, TabBackend::NotStarted));
        assert_eq!(tab.label_name(), "new");
        assert_eq!(tab.mode, SessionModeChoice::Auto);
    }

    #[test]
    fn tabs_open_at_the_lowest_free_number_and_the_last_close_opens_a_fresh_tab_one() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        let three = set.open(AgentId::Claude);
        assert_eq!(set.active(), three, "tmux's new-window selects the new window");
        assert_eq!(set.get(three).unwrap().number, 3);
        set.remove(two).unwrap();
        let again = set.open(AgentId::Claude);
        assert_eq!(set.get(again).unwrap().number, 2, "the lowest free number, reused");
        assert_ne!(again, two, "the id is never reused");
        for id in [one, again, three] {
            set.remove(id).unwrap();
        }
        assert_eq!(set.tabs().len(), 1, "the panel is never tab-less");
        assert_eq!(set.active_tab().number, 1);
    }

    /// The fresh tab the last close opens runs the configured `agent.default`, as every other tab made
    /// without naming an agent does.
    #[test]
    fn the_last_close_opens_a_tab_of_the_configured_agent() {
        let mut set = set();
        set.apply_configured_agent(AgentId::Codex);
        let one = set.active();
        assert_eq!(set.active_tab().agent(), &AgentId::Codex);
        set.remove(one).unwrap();
        assert_eq!(set.tabs().len(), 1);
        assert_eq!(
            set.active_tab().agent(),
            &AgentId::Codex,
            "the fresh tab runs the default agent"
        );
    }

    #[test]
    fn tab_has_last_only_when_the_previous_tab_still_belongs_to_the_base_view() {
        let mut set = set();
        assert!(!set.has_last());
        assert_eq!(set.select_last(), None);

        let one = set.active();
        let two = set.open(AgentId::Claude);
        let active = set.active();
        assert!(set.has_last());
        assert_eq!(set.active(), active);
        assert_eq!(set.last_active, Some(one));
        assert_eq!(set.select_last(), Some(one));

        assert_eq!(set.break_out(view(2)), Ok(one));
        set.last_active = Some(one);
        assert!(!set.has_last());
        assert_eq!(set.select_last(), None);

        set.remove(one).unwrap();
        set.last_active = Some(one);
        assert!(!set.has_last());
        assert_eq!(set.select_last(), None);
        assert_eq!(set.active(), two);
    }

    #[test]
    fn next_and_previous_wrap_last_goes_back_and_a_missing_number_selects_nothing() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        let three = set.open(AgentId::Claude);
        assert_eq!(set.step(1), Some(one), "wraps from the last");
        assert_eq!(set.step(-1), Some(three));
        assert!(set.select(two));
        assert_eq!(set.select_last(), Some(three), "tmux last-window");
        assert_eq!(set.select_last(), Some(two));
        assert_eq!(set.select_number(1), Some(TabPlace::Base(one)));
        assert_eq!(set.select_number(9), None, "no tab 9: the caller flashes");
        assert_eq!(set.active(), one);
    }

    #[test]
    fn a_command_without_a_tab_or_naming_an_unknown_one_is_refused() {
        let set = set();
        let active = set.active();
        assert_eq!(set.resolve(TabRef::Named(active)), Ok(Some(active)));
        assert_eq!(set.resolve(TabRef::WindowLevel), Ok(None));
        assert_eq!(
            set.resolve(TabRef::Missing),
            Err("protocol: this command names no tab".to_string()),
            "never the active one"
        );
        assert_eq!(
            set.resolve(TabRef::Named(TabId(99))),
            Err("protocol: no tab 99".to_string())
        );
    }

    /// Pumping every tab through `take_revised_ui_delivery` answers a background tab's Read
    /// without dispatching that tab's events to the page.
    #[test]
    fn the_pump_answers_what_needs_no_human_in_a_background_tab() {
        let dir = workspace("tabs-background-pump");
        let mut set = set();
        let background = set.active();
        let (background_provider, backend) = live(&dir);
        set.get_mut(background).unwrap().backend = TabBackend::Live(backend);
        let foreground = set.open(AgentId::Claude);
        let (_foreground_provider, backend) = live(&dir);
        set.get_mut(foreground).unwrap().backend = TabBackend::Live(backend);

        background_provider.queue(read("perm-read"));
        background_provider.queue(write("perm-write"));
        until("the background tab's card", || {
            let out = set.pump(&dir, true);
            assert!(
                out.active_payload.is_none(),
                "nothing of a background tab is dispatched"
            );
            set.get(background).unwrap().attention.attention().pending == 1
        });
        assert_eq!(background_provider.resolutions(), vec![("perm-read".to_string(), true)]);
        assert!(set.get(background).unwrap().session.stale);
        assert_eq!(set.attention().pending, 1, "the tray sums every tab");
        let tabs: serde_json::Value = serde_json::from_str(&set.tabs_payload()).unwrap();
        assert_eq!(tabs["active"], foreground.0);
        assert_eq!(tabs["tabs"][0]["marker"], "needs_input");

        // The switch: the new tab's snapshot, and it is no longer stale.
        assert!(set.select(background));
        let payloads = set.active_state_payloads();
        let snapshot: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert_eq!(snapshot["kind"], "snapshot");
        assert_eq!(snapshot["tab"], background.0);
        // The projection retains both requests until the Read's resolution returns, but
        // `host_answered` hides the already answered Read from snapshots and attention.
        // Only the Write still waiting for a human is shown.
        assert_eq!(
            snapshot["state"]["pendingPermissions"].as_array().unwrap().len(),
            1,
            "the auto-answered Read is hidden; only the Write is a real card"
        );
        assert_eq!(snapshot["state"]["pendingPermissions"][0]["permissionId"], "perm-write");
        assert!(!set.get(background).unwrap().session.stale);
        shut_down_all(&mut set);
    }

    fn text_delta(turn: &str, text: &str) -> AgentDomainEvent {
        AgentDomainEvent::ContentDelta {
            turn_id: turn.into(),
            kind: agent::ContentKind::Text,
            text: text.into(),
        }
    }

    /// The assistant text an `events` payload carries, concatenated in order; empty for no payload
    /// or any other envelope.
    fn events_text(payload: Option<&str>) -> String {
        let Some(value) = payload.and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok()) else {
            return String::new();
        };
        if value["kind"] != "events" {
            return String::new();
        }
        value["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "content_delta")
            .filter_map(|e| e["text"].as_str())
            .collect()
    }

    /// The transcript a `snapshot` payload carries, concatenated.
    fn snapshot_text(payload: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(value["kind"], "snapshot", "{payload}");
        value["state"]["transcript"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["text"].as_str())
            .collect()
    }

    /// The `snapshot` envelope among a live tab's `active_state_payloads`.
    fn the_snapshot(payloads: Vec<String>) -> String {
        payloads
            .into_iter()
            .find(|p| p.contains("\"kind\":\"snapshot\""))
            .expect("a live tab's snapshot")
    }

    /// Waits until the tab's projection holds `text` (folded by the ingestion thread, as the
    /// provider's own stream is).
    fn wait_for_fold(set: &TabSet, tab: TabId, text: &str) {
        until("the ingestion thread to fold the text", || {
            let backend = set.get(tab).unwrap().live().unwrap();
            let projection = backend.projection();
            projection.transcript.iter().any(|m| m.text.contains(text))
        });
    }

    /// Arms the seam (`TabSet::after_drain`): `pump`'s next drain of a live tab's queue is followed,
    /// before anything else, by `text` folding into that tab -- the provider streams it and this
    /// waits for the ingestion thread to fold it. The fold lands right after a drain and before the
    /// next snapshot read, where the race is, not wherever a timing probe happens to.
    fn fold_after_next_drain(set: &mut TabSet, provider: &Arc<RecordingProvider>, text: &'static str) {
        let provider = Arc::clone(provider);
        set.after_drain = Some(Box::new(move |backend: &AgentBackend| {
            provider.queue(text_delta("t1", text));
            until("the fold right after the drain", || {
                backend.projection().transcript.iter().any(|m| m.text.contains(text))
            });
        }));
    }

    /// One run of the double-delivery race: the snapshot a switch or a reload sent (`active_state_payloads`), then the
    /// active payload of each of the two ticks after it.
    struct P1a2Run {
        snapshot: String,
        next: Option<String>,
        after: Option<String>,
    }

    /// The race as it first showed: the sidecar's ingestion thread folds on
    /// its own thread, so a turn's text folds with NO tick in between -- the real gap between a tick
    /// and a switch or reload -- and the switch/reload snapshot carries it while the tab's queue
    /// still holds it. `redeliver` turns the `covered` filter off (`TabSet::redeliver_covered`).
    fn p1a2_mid_stream(redeliver: bool) -> P1a2Run {
        let dir = workspace("tabs-p1a2-mid-stream");
        let mut set = set();
        set.redeliver_covered = redeliver;
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        provider.queue(text_delta("t1", "P1-unique-text"));
        wait_for_fold(&set, tab, "P1-unique-text");
        let run = P1a2Run {
            snapshot: the_snapshot(set.active_state_payloads()),
            next: set.pump(&dir, true).active_payload,
            after: set.pump(&dir, true).active_payload,
        };
        shut_down_all(&mut set);
        run
    }

    /// The fold-after-a-drain shape, driven by the seam: a tick has delivered the turn's first text, `<between>`
    /// folds right after the next tick's drain, the switch/reload snapshot carries it, and `<after>`
    /// folds after that snapshot. `redeliver` as for [`p1a2_mid_stream`].
    fn p1a2_fold_after_a_drain(redeliver: bool) -> P1a2Run {
        let dir = workspace("tabs-p1a2-fold-after-drain");
        let mut set = set();
        set.redeliver_covered = redeliver;
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        provider.queue(text_delta("t1", "<first>"));
        until("the turn's first text to reach the panel", || {
            events_text(set.pump(&dir, true).active_payload.as_deref()).contains("<first>")
        });
        fold_after_next_drain(&mut set, &provider, "<between>");
        assert_eq!(
            set.pump(&dir, true).active_payload,
            None,
            "the tick the fold follows had drained nothing new"
        );
        assert!(set.after_drain.is_none(), "the seam ran, right after that tick's drain");
        let snapshot = the_snapshot(set.active_state_payloads());
        provider.queue(text_delta("t1", "<after>"));
        wait_for_fold(&set, tab, "<after>");
        let run = P1a2Run {
            snapshot,
            next: set.pump(&dir, true).active_payload,
            after: set.pump(&dir, true).active_payload,
        };
        shut_down_all(&mut set);
        run
    }

    /// A switch or reload taken mid-stream must not be followed by a tick
    /// delivering the snapshot's own text again, or the panel renders the reply twice. The
    /// snapshot carries the text; the ticks after it send nothing, the whole batch having been in it.
    #[test]
    fn a_mid_stream_snapshots_text_is_never_delivered_again() {
        let run = p1a2_mid_stream(false);
        assert!(
            snapshot_text(&run.snapshot).contains("P1-unique-text"),
            "the snapshot carries the text: {}",
            snapshot_text(&run.snapshot)
        );
        assert_eq!(
            run.next, None,
            "the next tick must not redeliver what the snapshot carried"
        );
        assert_eq!(run.after, None);
    }

    /// A drain and a later snapshot read are separate lock
    /// acquisitions, and an event folded between them is in the snapshot AND still queued, so the
    /// next tick would deliver it again. Driven by the seam, not by timing. Also the no-loss direction:
    /// what folds after the snapshot reaches the panel on the next tick, once, and nothing is left.
    #[test]
    fn a_fold_between_a_ticks_drain_and_a_switchs_snapshot_is_never_delivered_again() {
        let run = p1a2_fold_after_a_drain(false);
        assert!(
            snapshot_text(&run.snapshot).contains("<first><between>"),
            "the snapshot carries the fold: {}",
            snapshot_text(&run.snapshot)
        );
        let delivered = events_text(run.next.as_deref());
        assert!(
            !delivered.contains("<between>"),
            "the next tick must never redeliver what the snapshot carried: {delivered:?}"
        );
        assert_eq!(
            delivered, "<after>",
            "a fold after the snapshot reaches the panel, once, and nothing else does"
        );
        assert_eq!(events_text(run.after.as_deref()), "");
    }

    /// Both shapes with the `covered` filter off -- the semantics before it -- repeat the snapshot's
    /// text on the next tick: the filter, and nothing else, keeps it out. And the fixture
    /// `agent-ui/web/src/reducer.p1a2.test.ts` replays through the real `reducer.ts`: each shape's
    /// real snapshot and the real `events` of the tick after it, with the filter and without --
    /// never a hand-typed guess at their shape.
    ///
    /// Compared on every run, rewritten only under `EITRI_WRITE_FIXTURES=1` and only when it
    /// differs: a test in this crate writing into `../agent-ui/web/src` dirtied a tracked file
    /// whenever it went red, and a newer file under `src` sets off `shell/build.rs`'s npm rebuild. A
    /// checkout without the web tree skips the comparison, having no reducer test to feed.
    #[test]
    fn with_the_filter_off_the_tick_repeats_the_snapshot_and_the_reducer_fixture_records_both() {
        let events_of = |payload: Option<&str>| -> serde_json::Value {
            payload
                .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                .map_or(serde_json::json!([]), |v| v["events"].clone())
        };
        let record = |text: &str, filtered: P1a2Run, unfiltered: P1a2Run| {
            let snapshot: serde_json::Value = serde_json::from_str(&filtered.snapshot).unwrap();
            let unfiltered_snapshot: serde_json::Value = serde_json::from_str(&unfiltered.snapshot).unwrap();
            // `conversationId` is `AgentConversation::create`'s own random uuid, which the reducer
            // test never reads: pinned so the bytes are the same on every run.
            let mut state = snapshot["state"].clone();
            state["conversationId"] = serde_json::json!("fixture-conversation-id");
            let mut unfiltered_state = unfiltered_snapshot["state"].clone();
            unfiltered_state["conversationId"] = serde_json::json!("fixture-conversation-id");
            assert_eq!(state, unfiltered_state, "the two runs send the same snapshot");
            assert_eq!(snapshot["throughRevision"], unfiltered_snapshot["throughRevision"]);
            assert!(
                snapshot_text(&filtered.snapshot).contains(text),
                "the snapshot carries {text:?}"
            );
            assert!(
                !events_text(filtered.next.as_deref()).contains(text),
                "with the filter the next tick leaves {text:?} out: {:?}",
                filtered.next
            );
            assert!(
                events_text(unfiltered.next.as_deref()).contains(text),
                "without the filter the next tick sends {text:?} again: {:?}",
                unfiltered.next
            );
            serde_json::json!({
                "text": text,
                "snapshot": state,
                "throughRevision": snapshot["throughRevision"],
                "nextEvents": events_of(filtered.next.as_deref()),
                "nextEventsUnfiltered": events_of(unfiltered.next.as_deref()),
            })
        };
        let fixture = serde_json::json!({
            "writtenBy": "core/src/tab_set.rs, tests::with_the_filter_off_the_tick_repeats_the_snapshot_and_the_reducer_fixture_records_both \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "midStream": record("P1-unique-text", p1a2_mid_stream(false), p1a2_mid_stream(true)),
            "foldAfterADrain": record("<between>", p1a2_fold_after_a_drain(false), p1a2_fold_after_a_drain(true)),
        });
        let fixture_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/p1a2-mid-stream.json");
        let fixture_json = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&fixture_path).ok();
        if committed.as_deref() == Some(fixture_json.as_str()) {
            return;
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&fixture_path, &fixture_json).expect("write the p1a2 reducer fixture");
        } else if let Some(committed) = committed {
            assert_eq!(
                committed, fixture_json,
                "the committed p1a2 reducer fixture is not what this test produces; if the change is \
                 intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }

    /// An earlier catch-up drain at a switch or reload lost turn ends:
    /// a `TurnCompleted` a switch or reload took off the queue never reached
    /// `PumpOutput::turn_ended` -- the only trigger `eitri_panel::agent_panel`'s queue flush has -- so
    /// a message queued behind the turn sat there until something else poked the tab. That drain is
    /// gone; what this holds now is that a `TurnCompleted` the snapshot carried,
    /// and the next tick therefore leaves out of its payload, is still folded by that tick into
    /// `turn_ended`, exactly once. The completion folds with NO tick in between, as in the real race.
    #[test]
    fn a_turn_end_a_switch_snapshot_carried_still_flushes_the_queue() {
        let dir = workspace("tabs-p1a2-round1-turn-end");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        provider.queue(started("t1"));
        until("the turn to be delivered as running", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().turn_running()
        });

        set.queue_message(tab, "follow-up", "follow-up-wire".into(), 0, true, PromptOrigin::Typed)
            .expect("queuing behind a running turn");

        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        until("the completion to fold into the projection", || {
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .active_turn_id
                .is_none()
        });

        // The switch/reload snapshot carries the completion; the queue still holds it.
        let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
        assert_eq!(snapshot["state"]["activeTurnId"], serde_json::Value::Null);

        let mut turn_ended_reported = Vec::new();
        for _ in 0..20 {
            let out = set.pump(&dir, true);
            assert!(
                !out.active_payload
                    .as_deref()
                    .unwrap_or_default()
                    .contains("turn_completed"),
                "the snapshot carried the completion; no tick sends it again"
            );
            turn_ended_reported.extend(out.turn_ended);
        }
        assert_eq!(
            turn_ended_reported,
            vec![tab],
            "the turn's end must still be reported exactly once after a snapshot carried it"
        );
        assert_eq!(
            set.get(tab).unwrap().session.queue.len(),
            1,
            "the queued message must still be waiting for the flush this enables"
        );
        let flush = set
            .flush_queue(tab)
            .expect("the turn having ended must let the queue flush");
        assert_eq!(flush.typed, "follow-up");
        shut_down_all(&mut set);
    }

    /// Without a drain ahead of the snapshot, its turn clock must still match its own turn: a turn
    /// that started since the last tick is named by the snapshot, and the snapshot says when it
    /// started -- never the start of the turn before it, which the panel would take as exact.
    #[test]
    fn a_snapshot_names_the_start_of_the_turn_it_carries_even_before_a_tick_saw_it() {
        let dir = workspace("tabs-p1a2-turn-clock");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        let first = set.get(tab).unwrap().turn_started_at_ms().expect("t1 stamped");
        std::thread::sleep(Duration::from_millis(20));

        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        provider.queue(started("t2"));
        until("t2 to fold, with no tick", || {
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .active_turn_id
                .as_deref()
                == Some("t2")
        });
        let before = wall_clock_ms();
        let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
        assert_eq!(snapshot["state"]["activeTurnId"], "t2");
        let at = snapshot["turnStartedAtMs"]
            .as_u64()
            .expect("the snapshot says when t2 started");
        assert!(at > first && at >= before, "t2's start, not t1's ({first}): {at}");
        set.pump(&dir, true);
        assert_eq!(
            set.get(tab).unwrap().turn_started_at_ms(),
            Some(at),
            "the tick keeps the clock the snapshot sent"
        );
        shut_down_all(&mut set);
    }

    /// `pump`'s own `Resync` arm has the same shape -- the drain returns
    /// `Resync`, and the snapshot it sends is read after further lock cycles (and a bypass sweep) --
    /// so a fold in between would be in that snapshot and queued for the next tick. Driven the same way:
    /// an overflow owes the tab a resync, and the seam folds a unique delta right after that drain.
    #[test]
    fn a_fold_between_a_resyncs_drain_and_its_snapshot_read_is_never_delivered_again() {
        let dir = workspace("tabs-p1a2-round2-resync");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        let base = set.get(tab).unwrap().live().unwrap().projection().last_revision;
        let overflow = agent::UI_EVENT_QUEUE_CAPACITY + 10;
        provider.queue(started("t1"));
        for _ in 0..overflow {
            provider.queue(text_delta("t1", "."));
        }
        until("the overflow to fold with nothing draining it", || {
            set.get(tab).unwrap().live().unwrap().projection().last_revision > base + overflow as u64
        });

        fold_after_next_drain(&mut set, &provider, "<between>");
        let payload = set
            .pump(&dir, true)
            .active_payload
            .expect("a resync sends the active tab a snapshot");
        assert!(
            set.after_drain.is_none(),
            "the seam ran, between the drain and the read"
        );
        assert!(
            snapshot_text(&payload).ends_with("<between>"),
            "the drain was a Resync, and its snapshot carries the fold"
        );

        provider.queue(text_delta("t1", "<after>"));
        wait_for_fold(&set, tab, "<after>");
        let delivered = events_text(set.pump(&dir, true).active_payload.as_deref());
        assert!(
            !delivered.contains("<between>"),
            "the next pump must never redeliver what the resync's snapshot carried: {delivered:?}"
        );
        assert_eq!(
            delivered, "<after>",
            "a fold after the snapshot reaches the panel, once"
        );
        shut_down_all(&mut set);
    }

    /// The `Resync` arm's own early turn-clock read -- taken right after the
    /// drain, before this arm's further lock cycles (its one read of the projection and
    /// `approve_pending`'s bypass sweep) -- was used to stamp the payload this arm actually sends,
    /// which is read only afterward, from a fresh `SnapshotView::of`. If a turn ends and the next
    /// starts in that gap, the snapshot names the new turn while the payload still carries the old
    /// one's start time, and the panel takes that stamp as exact. Driven by a seam positioned exactly
    /// where `after_drain` (fired right after the drain, before any of this arm's own lock cycles)
    /// cannot reach: right before this arm's own snapshot read.
    #[test]
    fn a_resyncs_snapshot_names_the_start_of_the_turn_it_carries_even_when_the_race_ends_it_first() {
        let dir = workspace("tabs-p1a2-round4-resync-clock");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        let first = set.get(tab).unwrap().turn_started_at_ms().expect("t1 stamped");
        std::thread::sleep(Duration::from_millis(20));

        // Overflows the queue so this tick's drain is a `Resync`, with nothing draining it meanwhile.
        let base = set.get(tab).unwrap().live().unwrap().projection().last_revision;
        let overflow = agent::UI_EVENT_QUEUE_CAPACITY + 10;
        for _ in 0..overflow {
            provider.queue(text_delta("t1", "."));
        }
        until("the overflow to fold with nothing draining it", || {
            set.get(tab).unwrap().live().unwrap().projection().last_revision >= base + overflow as u64
        });

        let seam_provider = Arc::clone(&provider);
        set.before_resync_snapshot = Some(Box::new(move |backend: &AgentBackend| {
            seam_provider.queue(completed("t1", agent::TurnOutcome::Completed));
            seam_provider.queue(started("t2"));
            until("t1 to end and t2 to start, right before the snapshot read", || {
                backend.projection().active_turn_id.as_deref() == Some("t2")
            });
        }));

        let before = wall_clock_ms();
        let payload = set
            .pump(&dir, true)
            .active_payload
            .expect("a resync sends the active tab a snapshot");
        assert!(
            set.before_resync_snapshot.is_none(),
            "the seam ran, right before the snapshot read"
        );

        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            value["state"]["activeTurnId"], "t2",
            "the resync's own snapshot names t2, not t1"
        );
        let at = value["turnStartedAtMs"]
            .as_u64()
            .expect("the snapshot says when t2 started");
        assert!(at > first && at >= before, "t2's start, not t1's ({first}): {at}");

        shut_down_all(&mut set);
    }

    /// A snapshot's revision belongs to its session. A tab whose session is replaced
    /// right after a snapshot, with no pump in between, must not measure the new session's events
    /// -- whose revisions start again from its own seed -- against the old one's snapshot. Here
    /// through `collect_starts`; the next test replaces it the way `shell` does.
    #[test]
    fn a_new_sessions_events_are_never_measured_against_the_old_sessions_snapshot() {
        let dir = workspace("tabs-p1a2-round2-install");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        let base = backend.projection().last_revision;
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        for _ in 0..5 {
            provider.queue(text_delta("t1", "."));
        }
        provider.queue(text_delta("t1", "<old>"));
        wait_for_fold(&set, tab, "<old>");
        let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
        assert!(
            snapshot["throughRevision"].as_u64().is_some_and(|r| r >= base + 7),
            "the old session's snapshot covers more revisions than the new one will reach below"
        );

        let old = std::mem::replace(&mut set.get_mut(tab).unwrap().backend, TabBackend::NotStarted);
        if let TabBackend::Live(mut old) = old {
            old.shutdown();
        }
        let tx = starting(&mut set, tab, None, None);
        let (new_provider, new_backend) = live(&dir);
        tx.send(Ok(new_backend)).unwrap();
        assert_eq!(set.collect_starts().len(), 1, "installed");
        new_provider.queue(started("t2"));
        new_provider.queue(text_delta("t2", "<new-session>"));
        // Folded before the first drain, so that drain holds revisions the old snapshot would cover.
        wait_for_fold(&set, tab, "<new-session>");
        let delivered = events_text(set.pump(&dir, true).active_payload.as_deref());
        assert_eq!(
            delivered, "<new-session>",
            "the new session's first text reaches the panel"
        );
        shut_down_all(&mut set);
    }

    /// A snapshot's watermark belongs to the session it was read
    /// from and goes with it, whatever replaces the backend. `shell`'s fatal-command and
    /// never-opened paths swap in `Failed` themselves, passing through neither `collect_starts` nor
    /// the pump; here the next session is installed straight after, with no tick in between either.
    #[test]
    fn a_replaced_backend_takes_its_snapshot_watermark_with_it() {
        let dir = workspace("tabs-p1a2-watermark-replaced");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        let base = backend.projection().last_revision;
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        for _ in 0..5 {
            provider.queue(text_delta("t1", "."));
        }
        provider.queue(text_delta("t1", "<old>"));
        wait_for_fold(&set, tab, "<old>");
        let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
        assert!(
            snapshot["throughRevision"].as_u64().is_some_and(|r| r >= base + 7),
            "the old session's snapshot covers more revisions than the new one will reach below"
        );

        // As `eitri_panel::agent_panel` retires a session whose command failed fatally.
        let old = std::mem::replace(
            &mut set.get_mut(tab).unwrap().backend,
            TabBackend::Failed {
                reason: "a fatal command".into(),
            },
        );
        if let TabBackend::Live(mut old) = old {
            old.shutdown();
        }
        let (new_provider, new_backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(new_backend);
        new_provider.queue(started("t2"));
        new_provider.queue(text_delta("t2", "<new-session>"));
        wait_for_fold(&set, tab, "<new-session>");
        assert_eq!(
            events_text(set.pump(&dir, true).active_payload.as_deref()),
            "<new-session>",
            "the new session's text is measured against no snapshot of the old one's"
        );
        shut_down_all(&mut set);
    }

    /// `shell` reports the tray's attention around the tick and a
    /// switch, never around a panel reload (`InboundMessage::Ready`) -- so reading the active tab's
    /// state must not move it. A card folded since the last tick is drawn by the snapshot and
    /// counted by the next pump, which the tick reports; the pump does not send its request again.
    #[test]
    fn reading_the_active_tabs_state_leaves_the_windows_attention_alone() {
        let dir = workspace("tabs-p1a2-attention");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(write("perm-write"));
        until("the card to fold, with no pump", || {
            let backend = set.get(tab).unwrap().live().unwrap();
            let projection = backend.projection();
            projection.pending_permissions.contains_key("perm-write")
        });

        let before = set.attention();
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        assert_eq!(
            set.attention(),
            before,
            "reading a tab's state reports nothing to the tray"
        );
        assert_eq!(snapshot["state"]["pendingPermissions"][0]["permissionId"], "perm-write");

        let payload = set.pump(&dir, true).active_payload;
        assert_eq!(set.attention().pending, 1, "the tick counts the card");
        assert!(
            !payload.as_deref().unwrap_or_default().contains("permission_requested"),
            "the snapshot drew the card; the tick does not send it again: {payload:?}"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn view_activity_counts_running_and_queued_per_view() {
        let dir = workspace("tabs-view-activity");
        let mut set = set();
        let base = set.active();
        let queued = set.open(AgentId::Claude);
        let two = ViewId::new(2).unwrap();
        assert_eq!(set.break_out(two), Ok(queued));
        let (provider, backend) = live(&dir);
        set.get_mut(base).unwrap().backend = TabBackend::Live(backend);
        let (_queued_provider, backend) = live(&dir);
        set.get_mut(queued).unwrap().backend = TabBackend::Live(backend);
        set.queue_message(queued, "later", "later".into(), 1, false, PromptOrigin::Typed)
            .unwrap();
        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        until("the turn starts", || set.running_count() == 1);
        assert_eq!(
            set.view_activity(),
            vec![
                (ViewId::BASE, ViewActivity { running: 1, queued: 0 }),
                (two, ViewActivity { running: 0, queued: 1 }),
            ]
        );
        assert_eq!(set.queued_count(), 1);
        shut_down_all(&mut set);
    }

    #[test]
    fn the_active_tabs_events_are_dispatched_with_its_tab() {
        let dir = workspace("tabs-active-pump");
        let mut set = set();
        let active = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(active).unwrap().backend = TabBackend::Live(backend);
        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        let mut payload = None;
        until("the active tab's events", || {
            payload = set.pump(&dir, true).active_payload;
            payload.is_some()
        });
        let value: serde_json::Value = serde_json::from_str(&payload.unwrap()).unwrap();
        assert_eq!(value["kind"], "events");
        assert_eq!(value["tab"], active.0);
        assert!(!set.get(active).unwrap().session.stale);
        assert_eq!(set.running_count(), 1, "a turn is running");
        shut_down_all(&mut set);
    }

    /// A connect finishing after its tab closes is never installed elsewhere; the closed tab's
    /// receiver returns the backend for shutdown.
    #[test]
    fn a_late_start_for_a_closed_tab_is_returned_for_shutdown() {
        let dir = workspace("tabs-late-start");
        let mut set = set();
        let keep = set.active();
        let doomed = set.open(AgentId::Claude);
        let (tx, result_rx) = mpsc::channel();
        set.get_mut(doomed).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "req-1".into(),
            result_rx,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        });
        assert_eq!(set.running_count(), 1, "a connect counts as running");
        let closed = set.remove(doomed).unwrap();
        let (_provider, backend) = live(&dir);
        tx.send(Ok(backend)).unwrap();
        assert!(set.collect_starts().is_empty(), "no tab in the set was waiting for it");
        assert!(set.tabs().iter().all(|t| t.live().is_none()));
        assert_eq!(set.active(), keep);
        let TabBackend::Starting(pending) = closed.tab.backend else {
            panic!("the closed tab kept its pending start")
        };
        let mut orphan = pending.result_rx.try_recv().unwrap().map_err(|e| e.message).unwrap();
        orphan.shutdown();
    }

    fn starting(set: &mut TabSet, tab: TabId, title: Option<&str>, name: Option<&str>) -> mpsc::Sender<StartResult> {
        let (tx, result_rx) = mpsc::channel();
        set.get_mut(tab).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "req".into(),
            result_rx,
            first_turn: None,
            resume: Some("5f0c1e2d-resumed".into()),
            resumed_title: title.map(str::to_string),
            resumed_name: name.map(str::to_string),
        });
        tx
    }

    type StartResult = Result<AgentBackend, BackendError>;

    /// A resumed record's rename wins over its title and becomes the tab's own `name`, also used
    /// in the `prefix ,` detail popover. A rename made while connecting is newer and wins over both.
    #[test]
    fn a_resume_installs_the_records_rename_as_the_tabs_name_unless_renamed_meanwhile() {
        let dir = workspace("tabs-resume-name");
        let mut set = set();
        let first = set.active();
        let tx = starting(&mut set, first, Some("fix the parser"), Some("parser work"));
        tx.send(Ok(live(&dir).1)).unwrap();
        set.collect_starts();
        let tab = set.get(first).unwrap();
        assert_eq!(tab.name.as_deref(), Some("parser work"));
        assert_eq!(tab.session.title.as_deref(), Some("fix the parser"));
        assert_eq!(tab.label_name(), "parser work");

        let second = set.open(AgentId::Claude);
        let tx = starting(&mut set, second, Some("fix the parser"), Some("parser work"));
        set.rename(second, "mine");
        tx.send(Ok(live(&dir).1)).unwrap();
        set.collect_starts();
        assert_eq!(set.get(second).unwrap().name.as_deref(), Some("mine"));
        shut_down_all(&mut set);
    }

    /// `react` acts only when `arrived` grows. The window total must not decrease on session install,
    /// reset or tab close, or a simultaneous card in another tab could fail to raise it.
    #[test]
    fn the_windows_arrived_count_never_goes_down() {
        let dir = workspace("tabs-arrived");
        let mut set = set();
        let first = set.active();
        set.get_mut(first).unwrap().attention.observe(&[write("p1")], false);
        assert_eq!(set.attention().arrived, 1);

        let tx = starting(&mut set, first, None, None);
        tx.send(Ok(live(&dir).1)).unwrap();
        set.collect_starts();
        assert_eq!(set.attention().pending, 0, "a new session holds no card");
        assert_eq!(set.attention().arrived, 1, "installing a session kept the count");

        let second = set.open(AgentId::Claude);
        set.get_mut(second).unwrap().attention.observe(&[write("p2")], false);
        set.get_mut(second).unwrap().backend = TabBackend::Failed { reason: "x".into() };
        set.reset(second).unwrap();
        assert_eq!(set.attention().arrived, 2, "a reset kept the count");
        set.remove(second).unwrap();
        assert_eq!(set.attention().arrived, 2, "a close kept the count");
        shut_down_all(&mut set);
    }

    #[test]
    fn a_new_session_starts_with_no_per_session_state() {
        let dir = workspace("tabs-fresh-session");
        let mut set = set();
        let id = set.active();
        let (_, backend) = live(&dir);
        let tab = set.get_mut(id).unwrap();
        tab.backend = TabBackend::Live(backend);
        tab.session.title = Some("old title".into());
        tab.session.title_from_record = true;
        tab.attention.observe(&[write("old-card")], false);
        tab.session.turn_trace = Some(crate::turn_trace::TurnTrace::started_now());
        tab.session.reported_start_failure = true;
        tab.session
            .rule_offers
            .insert("old-card".into(), agent::PrefixRule::parse("Bash(ls *)").unwrap());
        tab.session.rule_offers_seen.push("old-card".into());
        tab.session.was_running = true;
        let call = CandidateCall {
            tool_name: "Write".into(),
            input: serde_json::json!({"file_path": "old.rs"}),
        };
        tab.session.note_ledger.record(
            "old-call".into(),
            NoteCandidate::Rule {
                rule: "Bash(ls *)".into(),
                call: call.clone(),
            },
        );
        tab.session
            .note_ledger
            .record("old-call".into(), NoteCandidate::AutoEdit(call));
        tab.session.creates_file_unsent.insert("old-call".into());
        tab.session.turn_clock = Some(("old-turn".into(), 1));
        tab.session.turn_times_unsent = 1;
        tab.session.host_answered.insert("old-card".into());
        tab.session
            .human_allowed
            .record("old-call", "Write", &serde_json::json!({}));
        tab.session
            .note_ledger
            .record("old-call".into(), NoteCandidate::PromptNote("old note".into()));
        tab.session.user_answered.insert("old-card".into());
        let TabBackend::Live(mut backend) = std::mem::replace(&mut tab.backend, TabBackend::NotStarted) else {
            unreachable!()
        };
        backend.shutdown();
        let tx = starting(&mut set, id, None, None);
        tx.send(Ok(live(&dir).1)).unwrap();
        assert!(matches!(
            set.collect_starts().as_slice(),
            [StartCollected::Installed { .. }]
        ));
        let tab = set.get(id).unwrap();
        assert!(tab.session.human_allowed.is_empty());
        assert!(tab.session.title.is_none());
        assert!(!tab.session.title_from_record);
        assert_eq!(tab.attention.attention().pending, 0);
        assert!(!tab.attention.attention().unread);
        assert!(tab.session.turn_trace.is_none());
        assert!(!tab.session.reported_start_failure);
        assert!(tab.session.rule_offers.is_empty());
        assert!(tab.session.rule_offers_seen.is_empty());
        assert!(!tab.session.was_running);
        assert!(tab.session.note_ledger.is_empty());
        assert!(tab.session.creates_file_unsent.is_empty());
        assert!(tab.session.turn_clock.is_none());
        assert_eq!(tab.session.turn_times_unsent, 0);
        assert!(tab.session.host_answered.is_empty());
        assert!(tab.session.user_answered.is_empty());
        shut_down_all(&mut set);
    }

    /// The tiers are named as the start begins, and the user may type and queue while the backend
    /// connects; installing the backend must not throw any of that away.
    #[test]
    fn a_tab_keeps_what_it_gathered_while_its_backend_started() {
        let dir = workspace("tabs-kept-while-starting");
        let mut set = set();
        let id = set.active();
        let tx = starting(&mut set, id, None, None);
        set.queue_message(
            id,
            "queued while starting",
            "queued while starting".into(),
            1,
            true,
            PromptOrigin::Typed,
        )
        .unwrap();
        let tab = set.get_mut(id).unwrap();
        tab.session.project_tiers = Some(agent::setting_sources::ProjectTrust::Untrusted);
        tab.session.stale = true;
        tab.session.queue_error = Some("refused flush".into());
        tab.session.queue_note = Some("held note".into());
        tab.replace_draft("typed while starting".into());
        tab.session.left_view = Some(ViewId::BASE);
        tx.send(Ok(live(&dir).1)).unwrap();
        assert!(matches!(
            set.collect_starts().as_slice(),
            [StartCollected::Installed { .. }]
        ));
        let tab = set.get(id).unwrap();
        assert!(matches!(tab.backend(), TabBackend::Live(_)));
        assert_eq!(
            tab.session.project_tiers,
            Some(agent::setting_sources::ProjectTrust::Untrusted)
        );
        assert_eq!(tab.session.left_view, Some(ViewId::BASE));
        assert!(tab.session.stale);
        assert_eq!(
            tab.session.queue.iter().map(|q| q.text.as_str()).collect::<Vec<_>>(),
            ["queued while starting"]
        );
        assert_eq!(tab.session.queue_error.as_deref(), Some("refused flush"));
        assert_eq!(tab.session.queue_note.as_deref(), Some("held note"));
        assert_eq!(tab.session.draft, "typed while starting");
        shut_down_all(&mut set);
    }

    #[test]
    fn a_handed_off_tab_keeps_its_resume_command_and_title_across_a_pump() {
        let dir = workspace("tabs-handoff-pump");
        let mut set = set();
        let id = set.active();
        let tab = set.get_mut(id).unwrap();
        tab.backend = TabBackend::Live(live(&dir).1);
        tab.session.title = Some("continue this conversation".into());
        let command =
            agent::handoff::ClaudeResumeCommand::for_session(dir.to_str().unwrap(), "1857dcd5-973b-46a2").unwrap();
        let mut backend = set.take_for_handoff(id).unwrap().into_backend();
        backend.shutdown();
        set.retire_done(id);
        set.get_mut(id).unwrap().last_handoff = Some(command);
        let before = set.active_state_payloads();
        for _ in 0..2 {
            set.pump_views(&dir, &|_| true);
        }
        let tab = set.get(id).unwrap();
        assert!(matches!(tab.backend, TabBackend::NotStarted));
        assert_eq!(tab.session.title.as_deref(), Some("continue this conversation"));
        assert_eq!(
            tab.last_handoff.as_ref().unwrap().provider_session_id(),
            "1857dcd5-973b-46a2"
        );
        assert_eq!(set.active_state_payloads(), before);
    }

    #[test]
    fn a_finished_start_installs_into_its_own_tab_with_its_first_turn() {
        let dir = workspace("tabs-install");
        let mut set = set();
        let first = set.active();
        let second = set.open(AgentId::Claude);
        set.rename(first, "docs");
        let (tx, result_rx) = mpsc::channel();
        set.get_mut(first).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "req-7".into(),
            result_rx,
            first_turn: Some(FirstTurn {
                typed_by_person: true,
                wire: "ctx\n\nhi".into(),
                typed: "hi".into(),
            }),
            resume: None,
            resumed_title: None,
            resumed_name: None,
        });
        let (_provider, backend) = live(&dir);
        tx.send(Ok(backend)).unwrap();
        let collected = set.collect_starts();
        assert!(matches!(
            collected.as_slice(),
            [StartCollected::Installed { tab, request_id, first_turn: Some(turn) }]
                if *tab == first && request_id == "req-7" && turn.typed == "hi"
        ));
        assert!(set.get(first).unwrap().live().is_some());
        assert_eq!(set.active(), second, "installing does not switch tabs");

        let (tx, result_rx) = mpsc::channel();
        set.get_mut(second).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "req-8".into(),
            result_rx,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        });
        drop(tx);
        let collected = set.collect_starts();
        assert!(matches!(collected.as_slice(), [StartCollected::Failed { tab, .. }] if *tab == second));
        assert!(
            matches!(set.get(second).unwrap().backend, TabBackend::Failed { .. }),
            "failures stay in their tab"
        );
        assert_eq!(set.get(second).unwrap().wire_state(), TabStateWire::Failed);
        assert!(
            set.get(first).unwrap().live().is_some(),
            "and the other tab is untouched"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn a_resume_of_a_session_open_in_a_tab_switches_to_it_and_one_held_elsewhere_is_refused() {
        let dir = workspace("tabs-resume-route");
        let mut set = set();
        let holder = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(holder).unwrap().backend = TabBackend::Live(backend);
        provider.open_session("claude-open", &dir);
        until("the session id", || {
            set.get(holder).unwrap().provider_session_id().is_some()
        });
        assert_eq!(set.open_sessions(), vec![(AgentId::Claude, "claude-open".to_string())]);
        let other = set.open(AgentId::Claude);

        assert_eq!(
            set.route_resume(other, &AgentId::Claude, "claude-open", true),
            ResumeRoute::SwitchTo(holder)
        );
        assert_eq!(set.active(), holder, "switched, nothing started");
        // Session ids are each agent's own: another agent's session of the same id is not this one.
        assert_eq!(
            set.route_resume(other, &AgentId::Codex, "claude-open", false),
            ResumeRoute::StartIn(other),
            "another agent's session of the same id is resumed, not switched to"
        );
        set.select(holder);
        assert_eq!(
            set.route_resume(holder, &AgentId::Claude, "claude-elsewhere", true),
            ResumeRoute::Refuse("open in another window".to_string())
        );
        assert_eq!(
            set.route_resume(other, &AgentId::Claude, "claude-free", false),
            ResumeRoute::StartIn(other),
            "an empty tab takes it"
        );
        let before = set.tabs().len();
        let ResumeRoute::StartIn(new_tab) = set.route_resume(holder, &AgentId::Claude, "claude-free", false) else {
            panic!("a busy tab resumes into a new one")
        };
        assert_ne!(new_tab, holder);
        assert_eq!(set.tabs().len(), before + 1);
        shut_down_all(&mut set);
    }

    /// Entering bypass asks for confirmation both before and after a session starts. A live active
    /// Auto tab receives `Confirm`, just like an empty tab; leaving bypass is immediate in either state.
    #[test]
    fn a_rename_is_normalized_and_the_mode_can_still_be_switched_after_the_session_starts() {
        let dir = workspace("tabs-rename-mode");
        let mut set = set();
        let tab = set.active();
        assert!(set.rename(tab, "  docs  "));
        assert_eq!(set.get(tab).unwrap().label_name(), "docs");
        assert!(set.rename(tab, ""));
        assert_eq!(set.get(tab).unwrap().name, None);
        assert!(!set.rename(TabId(99), "x"));

        // Before the session starts (NotStarted, auto), entering bypass asks and confirming also
        // moves the window default, so a fresh tab inherits bypass without another question.
        let first_plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(first_plan.scope, BypassScope::Tab(tab));
        assert_eq!(
            set.confirm_bypass(first_plan.scope, first_plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 0 })
        );
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Bypass,
            "new tabs take the remembered mode"
        );
        let next = set.open(AgentId::Claude);
        assert_eq!(set.get(next).unwrap().mode, SessionModeChoice::Bypass);

        let (_provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        // Leaving bypass works even when `next`, rather than `tab`, is on screen.
        // Only entering bypass requires the tab to be active.
        assert_eq!(
            set.cycle_mode(tab, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Manual)),
            "leaving bypass on a live tab moves at once, to manual"
        );
        // Manual -> auto moves at once too, and needs no tab on screen.
        assert_eq!(
            set.cycle_mode(tab, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Auto))
        );
        // Entering bypass again asks just as before the session existed, once `tab` is active again.
        set.select(tab);
        let second_plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(second_plan.scope, BypassScope::Tab(tab));
        shut_down_all(&mut set);
    }

    #[test]
    fn cycle_default_mode_moves_the_window_default_and_leaves_open_tabs_alone() {
        let mut set = set();
        let tab = set.active();
        assert_eq!(set.default_mode(), SessionModeChoice::Auto);
        assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Auto);

        let plan = match set.cycle_default_mode(ViewId::BASE) {
            ModeCycle::Confirm(plan) => plan,
            other => panic!("auto -> bypass always asks first: {other:?}"),
        };
        assert_eq!(plan.scope, BypassScope::Default);
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Auto,
            "nothing moves until confirmed"
        );

        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 0 })
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Bypass);
        assert_eq!(
            set.get(tab).unwrap().mode,
            SessionModeChoice::Auto,
            "the open tab's own mode is untouched"
        );
        assert_eq!(
            set.cycle_default_mode(ViewId::BASE),
            ModeCycle::Changed(SessionModeChoice::Manual),
            "leaving bypass moves at once, to manual"
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Manual);
        assert_eq!(
            set.cycle_default_mode(ViewId::BASE),
            ModeCycle::Changed(SessionModeChoice::Auto),
            "manual -> auto moves at once"
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Auto);
        assert!(
            matches!(set.cycle_default_mode(ViewId::BASE), ModeCycle::Confirm(_)),
            "and auto -> bypass asks again"
        );
        assert_eq!(
            set.get(tab).unwrap().mode,
            SessionModeChoice::Auto,
            "the open tab's own mode is untouched throughout"
        );
        shut_down_all(&mut set);
    }

    // Entering bypass asks first, then Eitri answers every request in it.

    #[test]
    fn entering_bypass_on_a_live_tab_asks_and_changes_nothing_until_confirmed() {
        let dir = workspace("bypass-ask-first");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        let _plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(
            set.get(tab).unwrap().mode,
            SessionModeChoice::Auto,
            "nothing changes until confirmed"
        );
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Auto,
            "the window default is untouched too"
        );

        provider.queue(write("p1"));
        until("the card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        assert!(
            provider.resolutions().is_empty(),
            "a queued write still cards in auto: {:?}",
            provider.resolutions()
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn confirming_approves_exactly_the_listed_cards_on_that_tab() {
        let dir = workspace("bypass-approves-exactly-listed");
        let mut set = set();
        let first = set.active();
        let (first_provider, backend) = live(&dir);
        set.get_mut(first).unwrap().backend = TabBackend::Live(backend);
        let second = set.open(AgentId::Claude);
        let (second_provider, backend) = live(&dir);
        set.get_mut(second).unwrap().backend = TabBackend::Live(backend);

        first_provider.queue(write("p1"));
        second_provider.queue(write("p2"));
        until("both cards", || {
            set.pump(&dir, true);
            set.get(first).unwrap().attention.attention().pending == 1
                && set.get(second).unwrap().attention.attention().pending == 1
        });

        // `open()` above left `second` active, and entering bypass on a tab that is not on screen
        // is refused -- select back to `first` first.
        set.select(first);
        let plan = plan(set.cycle_mode(first, ViewId::BASE));
        assert_eq!(plan.approve, vec!["p1".to_string()]);
        assert!(plan.lines[0].contains("1 waiting card"), "{:?}", plan.lines);

        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 1 })
        );
        assert_eq!(first_provider.resolutions(), vec![("p1".to_string(), true)]);
        assert!(second_provider.resolutions().is_empty());
        assert_eq!(
            set.get(first).unwrap().attention.attention().pending,
            0,
            "cleared at once, no extra pump needed"
        );
        assert_eq!(set.attention().pending, 1, "only the second tab's own card remains");
        shut_down_all(&mut set);
    }

    #[test]
    fn a_card_arriving_while_the_prompt_is_up_reprompts() {
        let dir = workspace("bypass-reprompt-on-new-card");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        provider.queue(write("p1"));
        until("p1's card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        let old_plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(old_plan.approve, vec!["p1".to_string()]);

        provider.queue(write("p2"));
        until("p2's card too", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 2
        });

        let reprompted = match set.confirm_bypass(old_plan.scope, old_plan.nonce, ViewId::BASE) {
            Ok(ConfirmOutcome::Reprompt(plan)) => plan,
            other => panic!("a card arrived while the prompt was up: {other:?}"),
        };
        assert_eq!(reprompted.approve, vec!["p1".to_string(), "p2".to_string()]);
        assert_ne!(reprompted.nonce, old_plan.nonce);
        assert!(
            reprompted.lines[0].contains("2 waiting cards"),
            "{:?}",
            reprompted.lines
        );
        assert_eq!(
            set.get(tab).unwrap().mode,
            SessionModeChoice::Auto,
            "nothing changed yet"
        );
        assert!(provider.resolutions().is_empty(), "no resolution sent by a reprompt");

        assert_eq!(
            set.confirm_bypass(old_plan.scope, old_plan.nonce, ViewId::BASE),
            Err("that prompt is no longer current".to_string()),
            "the OLD nonce is dead now"
        );
        assert_eq!(
            set.confirm_bypass(reprompted.scope, reprompted.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 2 })
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn in_bypass_every_request_is_answered_and_never_delivered() {
        let dir = workspace("bypass-answers-everything");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 0 })
        );

        provider.queue(write("p3"));
        provider.queue(outside("p4"));
        until("both resolved", || {
            let out = set.pump(&dir, true);
            if let Some(payload) = &out.active_payload {
                assert!(!payload.contains("\"p3\"") && !payload.contains("\"p4\""), "{payload}");
            }
            let mut resolved: Vec<String> = provider.resolutions().into_iter().map(|(id, _)| id).collect();
            resolved.sort();
            resolved == vec!["p3".to_string(), "p4".to_string()]
        });
        assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
        assert_eq!(
            set.get(tab).unwrap().session.host_answered,
            BTreeSet::from(["p3".to_string(), "p4".to_string()])
        );

        provider.queue(AgentDomainEvent::PermissionResolved {
            permission_id: "p3".into(),
            outcome: agent::PermissionOutcome::Allowed,
        });
        provider.queue(AgentDomainEvent::PermissionResolved {
            permission_id: "p4".into(),
            outcome: agent::PermissionOutcome::Allowed,
        });
        until("host_answered empties", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.host_answered.is_empty()
        });
        shut_down_all(&mut set);
    }

    #[test]
    fn leaving_bypass_is_immediate_and_the_classifier_decides_again() {
        let dir = workspace("bypass-leave-immediate");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 0 })
        );

        // Leaving bypass lands on manual, where the policy answers again (and an edit cards).
        assert_eq!(
            set.cycle_mode(tab, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Manual))
        );

        provider.queue(write("p5"));
        provider.queue(read("p6"));
        // Both halves are waited for: the ingestion thread may deliver p5 and p6 in different polls,
        // so p5's card being up does not yet mean p6 was answered (seen once under a loaded host).
        until("p5 cards and p6 is answered", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1 && !provider.resolutions().is_empty()
        });
        assert_eq!(provider.resolutions(), vec![("p6".to_string(), true)]);
        shut_down_all(&mut set);
    }

    #[test]
    fn an_empty_tab_confirm_moves_the_window_default_and_new_tabs_inherit_it_unasked() {
        let mut set = set();
        let tab = set.active();
        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(plan.scope, BypassScope::Tab(tab));
        assert!(
            plan.lines[0].contains("New sessions in this window start in bypass too"),
            "{:?}",
            plan.lines
        );

        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 0 })
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Bypass);

        let next = set.open(AgentId::Claude);
        assert_eq!(
            set.get(next).unwrap().mode,
            SessionModeChoice::Bypass,
            "inherited unasked"
        );
        assert_eq!(
            set.confirm_bypass(BypassScope::Tab(next), 0, ViewId::BASE),
            Err("that prompt is no longer current".to_string()),
            "open() left no stray prompt behind"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn the_chooser_default_asks_before_bypass_and_not_before_auto() {
        let mut set = set();
        let tab = set.active();
        match set.cycle_default_mode(ViewId::BASE) {
            ModeCycle::Confirm(plan) => {
                assert_eq!(plan.scope, BypassScope::Default);
                assert_eq!(
                    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
                    Ok(ConfirmOutcome::Entered { approved: 0 })
                );
            }
            other => panic!("auto -> bypass must ask first: {other:?}"),
        }
        assert_eq!(set.default_mode(), SessionModeChoice::Bypass);
        assert_eq!(
            set.get(tab).unwrap().mode,
            SessionModeChoice::Auto,
            "an open tab never changes"
        );

        assert_eq!(
            set.cycle_default_mode(ViewId::BASE),
            ModeCycle::Changed(SessionModeChoice::Manual),
            "bypass -> manual never asks"
        );
        assert_eq!(
            set.cycle_default_mode(ViewId::BASE),
            ModeCycle::Changed(SessionModeChoice::Auto),
            "manual -> auto never asks"
        );
        assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Auto, "still untouched");
        shut_down_all(&mut set);
    }

    #[test]
    fn entering_bypass_on_ended_and_failed_tabs_refuses_and_leaving_it_works() {
        let dir = workspace("bypass-ended-failed");

        // An ENDED AUTO tab refuses to enter bypass.
        {
            let mut set = set();
            let ended_auto = set.active();
            let (provider, backend) = live(&dir);
            set.get_mut(ended_auto).unwrap().backend = TabBackend::Live(backend);
            set.get_mut(ended_auto).unwrap().live_mut().unwrap().shutdown();
            provider.queue(AgentDomainEvent::SessionClosed {
                reason: "closed".into(),
            });
            until("ended", || {
                set.pump(&dir, true);
                set.get(ended_auto).unwrap().wire_state() == TabStateWire::Ended
            });
            assert_eq!(
                set.cycle_mode(ended_auto, ViewId::BASE),
                Err("the session has ended".to_string())
            );
            shut_down_all(&mut set);
        }

        // A STARTING tab asks like a live one: `Confirm`, LiveTab text, zero cards. If starting
        // fails, the FAILED bypass tab still leaves bypass immediately.
        {
            let mut set = set();
            let starting_tab = set.active();
            let tx = starting(&mut set, starting_tab, None, None);
            let plan = plan(set.cycle_mode(starting_tab, ViewId::BASE));
            assert_eq!(plan.approve, Vec::<String>::new());
            assert_eq!(
                plan.lines,
                vec!["Switch to bypass? (y/n)".to_string()],
                "a starting tab asks like a live one"
            );
            assert_eq!(
                set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
                Ok(ConfirmOutcome::Entered { approved: 0 })
            );
            tx.send(Err(BackendError {
                message: "boom".into(),
                benign: false,
            }))
            .unwrap();
            set.collect_starts();
            assert_eq!(set.get(starting_tab).unwrap().wire_state(), TabStateWire::Failed);
            assert_eq!(
                set.get(starting_tab).unwrap().mode,
                SessionModeChoice::Bypass,
                "the mode survived the failed start"
            );
            assert_eq!(
                set.cycle_mode(starting_tab, ViewId::BASE),
                Ok(ModeCycle::Changed(SessionModeChoice::Manual)),
                "a failed bypass tab still leaves at once"
            );
            assert_eq!(
                set.cycle_mode(starting_tab, ViewId::BASE),
                Err("the session has ended".to_string()),
                "a failed manual tab stays manual, as an ended auto tab stays auto"
            );
            shut_down_all(&mut set);
        }

        // An ENDED bypass tab also leaves at once...
        {
            let mut set = set();
            let ended_bypass = set.active();
            let (provider, backend) = live(&dir);
            set.get_mut(ended_bypass).unwrap().backend = TabBackend::Live(backend);
            let plan = plan(set.cycle_mode(ended_bypass, ViewId::BASE));
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
            set.get_mut(ended_bypass).unwrap().live_mut().unwrap().shutdown();
            provider.queue(AgentDomainEvent::SessionClosed {
                reason: "closed".into(),
            });
            until("ended", || {
                set.pump(&dir, true);
                set.get(ended_bypass).unwrap().wire_state() == TabStateWire::Ended
            });
            assert_eq!(
                set.cycle_mode(ended_bypass, ViewId::BASE),
                Ok(ModeCycle::Changed(SessionModeChoice::Manual)),
                "an ended bypass tab still leaves at once"
            );
            shut_down_all(&mut set);
        }

        // `reset` on an ended bypass tab keeps its mode and leaves the window default untouched.
        {
            let mut set = set();
            let ended_bypass = set.active();
            let (provider, backend) = live(&dir);
            set.get_mut(ended_bypass).unwrap().backend = TabBackend::Live(backend);
            let plan = plan(set.cycle_mode(ended_bypass, ViewId::BASE));
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
            let default_after_confirm = set.default_mode();
            set.get_mut(ended_bypass).unwrap().live_mut().unwrap().shutdown();
            provider.queue(AgentDomainEvent::SessionClosed {
                reason: "closed".into(),
            });
            until("ended", || {
                set.pump(&dir, true);
                set.get(ended_bypass).unwrap().wire_state() == TabStateWire::Ended
            });
            set.reset(ended_bypass).unwrap();
            assert_eq!(
                set.get(ended_bypass).unwrap().mode,
                SessionModeChoice::Bypass,
                "reset keeps bypass"
            );
            assert_eq!(
                set.default_mode(),
                default_after_confirm,
                "reset does not touch the window default"
            );
            shut_down_all(&mut set);
        }
    }

    #[test]
    fn a_resync_in_bypass_sweeps_the_pending_cards_and_draws_none() {
        let dir = workspace("bypass-resync-sweep");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 0 })
        );

        provider.queue(write("pre"));
        until(
            "pre answered, still pending (the double no test provides its own resolution)",
            || {
                set.pump(&dir, true);
                set.get(tab).unwrap().session.host_answered.contains("pre")
                    && set
                        .get(tab)
                        .unwrap()
                        .live()
                        .unwrap()
                        .projection()
                        .pending_permissions
                        .contains_key("pre")
            },
        );

        // Without pumping in between: enough to overflow the sidecar's own UI queue.
        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        for i in 0..300 {
            provider.queue(AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: agent::ContentKind::Text,
                text: format!("chunk {i}"),
            });
        }
        provider.queue(write("lost1"));
        provider.queue(write("lost2"));
        // The CLI's own prompt, forced by the user's ask rule: bypass sweeps it like the gate's own.
        provider.queue(cli_prompt(
            "lost-ask",
            "toolu_lost_ask",
            "Bash",
            serde_json::json!({ "command": "cat notes.txt" }),
            Some("cat:*"),
        ));

        until("all four pending in the projection", || {
            let Some(backend) = set.get(tab).unwrap().live() else {
                return false;
            };
            backend.projection().pending_permissions.len() == 4
        });

        let out = set.pump(&dir, true);
        let payload = out.active_payload.expect("the active tab's own resync payload");
        let snapshot: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(snapshot["kind"], "snapshot");

        let mut resolved: Vec<String> = provider.resolutions().into_iter().map(|(id, _)| id).collect();
        resolved.sort();
        assert_eq!(
            resolved,
            vec![
                "lost-ask".to_string(),
                "lost1".to_string(),
                "lost2".to_string(),
                "pre".to_string()
            ]
        );
        assert_eq!(
            provider.resolutions().iter().filter(|(id, _)| id == "pre").count(),
            1,
            "pre was already host-answered before the resync -- never answered twice"
        );

        assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
        let pending = snapshot["state"]["pendingPermissions"].as_array().unwrap();
        assert!(pending.is_empty(), "no swept id is drawn: {pending:?}");
        assert_eq!(
            set.get(tab).unwrap().session.note_ledger.prompt_note("toolu_lost_ask"),
            Some("your ask rule: Bash(cat:*) — allowed in bypass"),
            "the swept CLI prompt's call will say so on its row"
        );
        shut_down_all(&mut set);
    }

    /// The gap `attention`'s module doc used to record for `resync` is closed in `Auto` too: an id
    /// the classifier already answered is swept from the snapshot and the tray exactly as bypass's
    /// own sweep is, even though nothing here re-answers it (it was never in `Bypass`).
    #[test]
    fn a_resync_in_auto_hides_what_the_classifier_answered() {
        let dir = workspace("auto-resync-hides-classifier-answer");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        provider.queue(read("seen"));
        until("the classifier answered it", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.host_answered.contains("seen")
        });

        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        for i in 0..300 {
            provider.queue(AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: agent::ContentKind::Text,
                text: format!("chunk {i}"),
            });
        }
        provider.queue(write("lost"));

        until("both pending in the projection", || {
            let Some(backend) = set.get(tab).unwrap().live() else {
                return false;
            };
            backend.projection().pending_permissions.len() == 2
        });

        let out = set.pump(&dir, true);
        let payload = out.active_payload.expect("the active tab's own resync payload");
        let snapshot: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(snapshot["kind"], "snapshot");
        let pending = snapshot["state"]["pendingPermissions"].as_array().unwrap();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0]["permissionId"], "lost");
        assert_eq!(set.get(tab).unwrap().attention.attention().pending, 1);
        shut_down_all(&mut set);
    }

    /// `Tab::mode` remains the authority regardless of provider reports. A real provider turns an
    /// unrestricted mode report into `UngatedCliMode` rather than a raw `PermissionModeChanged`;
    /// even a raw event must not move the tab's mode.
    #[test]
    fn a_provider_mode_report_moves_nothing() {
        let dir = workspace("mode-report-is-inert");
        let mut set = set();
        let auto_tab = set.active();
        let (auto_provider, backend) = live(&dir);
        set.get_mut(auto_tab).unwrap().backend = TabBackend::Live(backend);
        auto_provider.queue(AgentDomainEvent::PermissionModeChanged {
            mode: agent::PermissionMode::Bypass,
            provider_mode: "default".into(),
            floor_applied: false,
        });
        auto_provider.queue(write("still-a-card"));
        until("the write still cards", || {
            set.pump(&dir, true);
            set.get(auto_tab).unwrap().attention.attention().pending == 1
        });
        assert_eq!(
            set.get(auto_tab).unwrap().mode,
            SessionModeChoice::Auto,
            "a provider report never moves Tab::mode"
        );

        let bypass_tab = set.open(AgentId::Claude);
        let (bypass_provider, backend) = live(&dir);
        set.get_mut(bypass_tab).unwrap().backend = TabBackend::Live(backend);
        let plan = plan(set.cycle_mode(bypass_tab, ViewId::BASE));
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
        bypass_provider.queue(AgentDomainEvent::PermissionModeChanged {
            mode: agent::PermissionMode::Auto,
            provider_mode: "default".into(),
            floor_applied: false,
        });
        bypass_provider.queue(write("answered-anyway"));
        until("it is answered anyway", || {
            set.pump(&dir, true);
            bypass_provider
                .resolutions()
                .iter()
                .any(|(id, _)| id == "answered-anyway")
        });
        assert_eq!(
            set.get(bypass_tab).unwrap().mode,
            SessionModeChoice::Bypass,
            "still bypass"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn a_prompt_dies_with_a_tab_switch() {
        let dir = workspace("bypass-prompt-dies-on-switch");

        fn two_tabs(dir: &Path) -> (TabSet, TabId, TabId) {
            let mut set = set();
            let first = set.active();
            let (_p, backend) = live(dir);
            set.get_mut(first).unwrap().backend = TabBackend::Live(backend);
            let second = set.open(AgentId::Claude);
            let (_p, backend) = live(dir);
            set.get_mut(second).unwrap().backend = TabBackend::Live(backend);
            set.select(first);
            (set, first, second)
        }

        for mutate in [
            "select_second",
            "open",
            "step",
            "select_number",
            "select_last",
            "remove_first",
        ] {
            let (mut set, first, second) = two_tabs(&dir);
            let plan = plan(set.cycle_mode(first, ViewId::BASE));
            match mutate {
                "select_second" => {
                    set.select(second);
                }
                "open" => {
                    set.open(AgentId::Claude);
                }
                "step" => {
                    set.step(1);
                }
                "select_number" => {
                    set.select_number(set.get(second).unwrap().number);
                }
                "select_last" => {
                    set.select(second);
                    set.select(first);
                    set.select_last();
                }
                "remove_first" => {
                    set.remove(first);
                }
                _ => unreachable!(),
            }
            assert_eq!(
                set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
                Err("that prompt is no longer current".to_string()),
                "{mutate}"
            );
            if mutate != "remove_first" {
                assert_eq!(set.get(first).unwrap().mode, SessionModeChoice::Auto, "{mutate}");
            }
            shut_down_all(&mut set);
        }

        // The strongest case: select(second) then back to select(first) -- the SAME tab ends up
        // active again -- still drops the prompt, since the active id genuinely changed in between.
        let (mut set, first, second) = two_tabs(&dir);
        let first_plan = plan(set.cycle_mode(first, ViewId::BASE));
        set.select(second);
        set.select(first);
        assert_eq!(
            set.confirm_bypass(first_plan.scope, first_plan.nonce, ViewId::BASE),
            Err("that prompt is no longer current".to_string())
        );

        let second_plan = plan(set.cycle_mode(first, ViewId::BASE));
        set.drop_bypass_prompt();
        assert_eq!(
            set.confirm_bypass(second_plan.scope, second_plan.nonce, ViewId::BASE),
            Err("that prompt is no longer current".to_string())
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn entering_bypass_on_a_tab_that_is_not_active_is_refused() {
        let dir = workspace("bypass-not-active-refused");
        let mut set = set();
        let first = set.active();
        let (_p, backend) = live(&dir);
        set.get_mut(first).unwrap().backend = TabBackend::Live(backend);
        let second = set.open(AgentId::Claude);
        let (_p, backend) = live(&dir);
        set.get_mut(second).unwrap().backend = TabBackend::Live(backend);
        set.select(first);
        assert_eq!(
            set.cycle_mode(second, ViewId::BASE),
            Err(format!(
                "tab {} is not the one on screen",
                set.get(second).unwrap().number
            ))
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn the_count_is_the_delivered_cards_not_the_projection() {
        let dir = workspace("bypass-count-is-delivered-cards");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        // Stays pending in the projection until its resolution is queued (`RecordingProvider` never
        // emits one on its own) -- this holds the classifier's own answer back, so a prompt the
        // classifier answered sits pending in the projection yet never becomes a card.
        provider.queue(read("classifier-answered"));
        provider.queue(write("real-card"));
        until("only the real card is a card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        assert!(
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .pending_permissions
                .contains_key("classifier-answered"),
            "the premise: it has not resolved yet"
        );

        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(plan.approve, vec!["real-card".to_string()]);
        assert!(plan.lines[0].contains("1 waiting card"), "{:?}", plan.lines);
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 1 }),
            "must not Reprompt for the classifier-answered id"
        );
        let mut resolutions = provider.resolutions();
        resolutions.sort();
        assert_eq!(
            resolutions,
            vec![
                ("classifier-answered".to_string(), true),
                ("real-card".to_string(), true)
            ],
            "each answered exactly once"
        );
        shut_down_all(&mut set);
    }

    /// The attention half of `waiting_cards`' filter, isolated from the `host_answered` half: a
    /// request the background ingestion has already folded into the projection, but that no pump has
    /// yet delivered as a card (`tab.attention.card_ids()`), must not be offered for bypass approval
    /// either. Without this test, removing `cards.contains(&p.permission_id)` from `waiting_cards` and
    /// keeping only the `host_answered` filter left the whole suite passing, because
    /// `the_count_is_the_delivered_cards_not_the_projection`'s held-back id is also `host_answered`
    /// (the classifier already allowed it), which masks the attention half entirely.
    #[test]
    fn waiting_cards_excludes_a_request_not_yet_delivered_as_a_card() {
        let dir = workspace("bypass-waiting-cards-excludes-undelivered");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        provider.queue(write("real-card"));
        until(
            "the projection holds it, before any pump has delivered it as a card",
            || {
                set.get(tab)
                    .unwrap()
                    .live()
                    .unwrap()
                    .projection()
                    .pending_permissions
                    .contains_key("real-card")
            },
        );
        assert_eq!(
            set.get(tab).unwrap().attention.attention().pending,
            0,
            "the premise: nothing has been pumped, so nothing is a delivered card yet"
        );

        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert!(
            plan.approve.is_empty(),
            "a request only in the projection, never delivered as a card, must not be offered for \
             bypass approval: {:?}",
            plan.approve
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn leaving_bypass_on_a_live_tab_drops_the_window_bypass_default() {
        let dir = workspace("bypass-leaving-live-drops-default");
        let mut set = set();
        let tab = set.active();
        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 0 })
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Bypass);

        let (_p, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(
            set.get(tab).unwrap().mode,
            SessionModeChoice::Bypass,
            "starting the session kept the mode"
        );

        assert_eq!(
            set.cycle_mode(tab, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Manual))
        );
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Auto,
            "leaving bypass on ANY tab drops an in-window default too, to auto"
        );

        let next = set.open(AgentId::Claude);
        assert_eq!(set.get(next).unwrap().mode, SessionModeChoice::Auto);
        shut_down_all(&mut set);
    }

    /// A live-tab bypass prompt says nothing about new sessions. If a terminal handoff changes the
    /// tab to `NotStarted` while that prompt waits, accepting it must not silently change the window
    /// default and put later new tabs into bypass. Confirmation remembers the question shown and
    /// asks again when it no longer describes the tab.
    #[test]
    fn a_live_tab_prompt_answered_after_the_tab_became_empty_reprompts_and_moves_nothing() {
        let dir = workspace("bypass-prompt-kind-changed");
        let mut set = set();
        let tab = set.active();
        let (_provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        let live_plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(live_plan.prompt, tabs::PromptScope::LiveTab);
        assert_eq!(
            live_plan.lines,
            vec![tabs::bypass_prompt(tabs::PromptScope::LiveTab, 0)]
        );

        // What `agent_panel`'s `HandoffToTerminal` arm does to the tab (its own drop of the prompt
        // is the other half of the protection; this is the half that holds even without it).
        if let TabBackend::Live(mut backend) =
            std::mem::replace(&mut set.get_mut(tab).unwrap().backend, TabBackend::NotStarted)
        {
            backend.shutdown();
        }

        let fresh = match set.confirm_bypass(live_plan.scope, live_plan.nonce, ViewId::BASE) {
            Ok(ConfirmOutcome::Reprompt(fresh)) => fresh,
            other => panic!("the question changed, so the answer must be asked again: {other:?}"),
        };
        assert_eq!(fresh.prompt, tabs::PromptScope::EmptyTab);
        assert_eq!(fresh.lines, vec![tabs::bypass_prompt(tabs::PromptScope::EmptyTab, 0)]);
        assert_ne!(fresh.nonce, live_plan.nonce);
        assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Auto, "nothing entered");
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Auto,
            "and the window default never moved"
        );
        assert_eq!(
            set.confirm_bypass(live_plan.scope, live_plan.nonce, ViewId::BASE),
            Err("that prompt is no longer current".to_string())
        );
        let next = set.open(AgentId::Claude);
        assert_eq!(
            set.get(next).unwrap().mode,
            SessionModeChoice::Auto,
            "a new tab is still auto"
        );
        shut_down_all(&mut set);
    }

    /// The same re-check the other way: an empty tab's prompt (which DOES say new sessions follow)
    /// answered once the tab has started is asked again as a live tab's, which moves only the tab.
    #[test]
    fn an_empty_tab_prompt_answered_after_the_tab_started_reprompts_as_a_live_tab() {
        let dir = workspace("bypass-prompt-kind-changed-2");
        let mut set = set();
        let tab = set.active();
        let empty_plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(empty_plan.prompt, tabs::PromptScope::EmptyTab);
        let (_provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        let fresh = match set.confirm_bypass(empty_plan.scope, empty_plan.nonce, ViewId::BASE) {
            Ok(ConfirmOutcome::Reprompt(fresh)) => fresh,
            other => panic!("expected a reprompt: {other:?}"),
        };
        assert_eq!(fresh.prompt, tabs::PromptScope::LiveTab);
        assert_eq!(
            set.confirm_bypass(fresh.scope, fresh.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 0 })
        );
        assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Bypass);
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Auto,
            "a live tab's entry moves only that tab"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn reset_drops_an_open_bypass_prompt() {
        let mut set = set();
        let tab = set.active();
        set.get_mut(tab).unwrap().backend = TabBackend::Failed { reason: "test".into() };
        let default_plan = match set.cycle_default_mode(ViewId::BASE) {
            ModeCycle::Confirm(plan) => plan,
            other => panic!("auto -> bypass asks: {other:?}"),
        };
        set.reset(tab).unwrap();
        assert_eq!(
            set.confirm_bypass(default_plan.scope, default_plan.nonce, ViewId::BASE),
            Err("that prompt is no longer current".to_string())
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Auto);
    }

    /// A launch must never start in bypass, even if a caller bypasses `agent_prefs::startup_mode`.
    /// The tab set enforces this independently of the caller's startup-mode normalization.
    #[test]
    fn a_window_never_starts_in_bypass_whatever_it_is_handed() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Bypass);
        assert_eq!(set.default_mode(), SessionModeChoice::Auto);
        assert_eq!(set.active_tab().mode(), SessionModeChoice::Auto);
        let next = set.open(AgentId::Claude);
        assert_eq!(set.get(next).unwrap().mode(), SessionModeChoice::Auto);
    }

    #[test]
    fn reset_is_refused_while_a_session_runs_and_keeps_the_number_and_the_name() {
        let dir = workspace("tabs-reset");
        let mut set = set();
        let tab = set.active();
        set.rename(tab, "docs");
        assert!(set.reset(tab).is_err(), "an empty tab has nothing to reset");
        let (_provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert!(set.reset(tab).is_err(), "r is only offered on an ended or failed tab");
        set.get_mut(tab).unwrap().live_mut().unwrap().shutdown();
        until("the ended state", || {
            set.get(tab).unwrap().wire_state() == TabStateWire::Ended
        });
        let old = set
            .reset(tab)
            .unwrap()
            .expect("the ended backend comes back for shutdown");
        drop(old);
        let reset = set.get(tab).unwrap();
        assert!(matches!(reset.backend, TabBackend::NotStarted));
        assert_eq!((reset.number, reset.name.as_deref()), (1, Some("docs")));
        set.get_mut(tab).unwrap().backend = TabBackend::Failed {
            reason: "refused".into(),
        };
        assert!(
            matches!(set.reset(tab), Ok(None)),
            "a failed tab resets with no backend to shut down"
        );
    }

    #[test]
    fn the_oldest_card_across_tabs_names_the_tab_that_holds_it() {
        let mut set = set();
        let first = set.active();
        let second = set.open(AgentId::Claude);
        set.get_mut(second).unwrap().attention.observe(&[write("older")], false);
        set.get_mut(first).unwrap().attention.observe(&[write("newer")], false);
        assert_eq!(set.oldest_card_tab(), Some(second));
        assert_eq!(set.newest_card_tab(), Some(first));
        assert_eq!(set.attention().pending, 2);
        set.select(first);
        set.mark_seen(first);
        assert!(!set.get(first).unwrap().attention.attention().unread);
    }

    #[test]
    fn the_close_facts_say_what_closing_a_tab_involves() {
        let mut set = set();
        let tab = set.active();
        set.rename(tab, "docs");
        let facts = set.close_facts(tab).unwrap();
        assert_eq!(facts.number, 1);
        assert_eq!(facts.label_name, "docs");
        assert!(!facts.turn_running && !facts.has_backend && !facts.legacy);
        let legacy = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
        assert!(legacy.close_facts(legacy.active()).unwrap().legacy);
    }

    #[test]
    fn close_others_plan_names_every_tab_but_the_active_one_and_is_none_alone() {
        let mut set = set();
        let active = set.active();
        assert_eq!(set.close_others_plan(), None, "the active tab is the only one open");
        let second = set.open(AgentId::Claude);
        let third = set.open(AgentId::Claude);
        set.select(active);
        let (ids, prompt) = set.close_others_plan().unwrap();
        assert_eq!(ids, vec![second, third]);
        assert_eq!(prompt, "close 2 other tabs? (y/n)");
    }

    #[test]
    fn close_others_plan_leaves_out_a_tab_shown_in_an_extra_view() {
        let mut set = set();
        let active = set.active();
        let second = set.open(AgentId::Claude);
        let third = set.open(AgentId::Claude);
        set.select(third);
        let view = ViewId::new(2).unwrap();
        assert_eq!(set.break_out(view), Ok(third));
        set.select(active);
        let (ids, prompt) = set.close_others_plan().unwrap();
        assert_eq!(ids, vec![second], "the bound tab is not one of the base strip's others");
        assert_eq!(prompt, "close 1 other tab? (y/n)");
        set.remove(second);
        assert_eq!(
            set.close_others_plan(),
            None,
            "the active tab is the base view's only one"
        );
    }

    /// Pumps until the active tab has a payload, returning it with its class.
    fn pump_until_payload(set: &mut TabSet, dir: &Path) -> (String, crate::panel_cadence::EnvelopeClass) {
        let mut got = None;
        until("a payload for the active tab", || {
            let out = set.pump(dir, true);
            if let Some(payload) = out.active_payload {
                got = Some((payload, out.active_class));
            }
            got.is_some()
        });
        got.unwrap()
    }

    #[test]
    fn the_pump_says_which_payloads_may_wait_for_the_typing_cadence() {
        use crate::panel_cadence::EnvelopeClass::{Immediate, Stream};
        let dir = workspace("tabs-pump-class");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        // A turn starting is state the chrome hangs on.
        provider.queue(started("t1"));
        let (payload, class) = pump_until_payload(&mut set, &dir);
        assert!(payload.contains("turn_started"), "{payload}");
        assert_eq!(class, Immediate);

        // Plain streamed text is what the cadence exists to pace.
        provider.queue(text_delta("t1", "hello"));
        let (payload, class) = pump_until_payload(&mut set, &dir);
        assert_eq!(events_text(Some(&payload)), "hello");
        assert_eq!(class, Stream);

        // A card is never held behind a slot.
        provider.queue(write("perm-1"));
        let (payload, class) = pump_until_payload(&mut set, &dir);
        assert!(payload.contains("permission_requested"), "{payload}");
        assert_eq!(class, Immediate);

        // Nor is the end of the turn.
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        let (payload, class) = pump_until_payload(&mut set, &dir);
        assert!(payload.contains("turn_completed"), "{payload}");
        assert_eq!(class, Immediate);
        shut_down_all(&mut set);
    }

    #[test]
    fn a_resync_snapshot_is_never_paced() {
        use crate::panel_cadence::EnvelopeClass::Immediate;
        let dir = workspace("tabs-pump-class-resync");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        // Without pumping in between: enough to overflow the sidecar's own UI queue.
        provider.queue(started("t1"));
        for i in 0..300 {
            provider.queue(text_delta("t1", &format!("chunk {i}")));
        }
        until("the whole stream folded", || {
            let backend = set.get(tab).unwrap().live().unwrap();
            let projection = backend.projection();
            projection.transcript.iter().any(|m| m.text.contains("chunk 299"))
        });
        let (payload, class) = pump_until_payload(&mut set, &dir);
        assert!(
            payload.contains("\"kind\":\"snapshot\""),
            "an overflow resyncs: {payload:.120}"
        );
        assert_eq!(class, Immediate);
        shut_down_all(&mut set);
    }

    fn interruptible_live(dir: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
        let provider = Arc::new(RecordingProvider::interruptible());
        let conversation =
            AgentConversation::create(provider.clone(), dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        (
            provider,
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            },
        )
    }

    fn started(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::TurnStarted { turn_id: id.into() }
    }

    fn completed(id: &str, outcome: agent::TurnOutcome) -> AgentDomainEvent {
        AgentDomainEvent::TurnCompleted {
            turn_id: id.into(),
            outcome,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        }
    }

    /// Pumps until the pump has DELIVERED the turn state `running` for `tab`, returning every tab
    /// whose turn ended meanwhile. Waiting on `turn_running()` instead races the ingestion thread:
    /// the projection can be ahead of what the pump has delivered.
    fn pump_until_running(set: &mut TabSet, dir: &Path, tab: TabId, running: bool) -> Vec<TabId> {
        let mut ended = Vec::new();
        until("the turn state", || {
            ended.extend(set.pump(dir, true).turn_ended);
            set.get(tab).unwrap().session.was_running == running
        });
        ended
    }

    #[test]
    fn queued_messages_go_out_as_one_turn_when_the_turn_completes() {
        let dir = workspace("tabs-queue-flush");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);

        assert_eq!(
            set.queue_message(
                tab,
                "and the tests",
                "ctx A\n\nand the tests".into(),
                1,
                true,
                agent::PromptOrigin::Typed
            ),
            Ok(1)
        );
        assert_eq!(
            set.queue_message(
                tab,
                "then commit",
                "ctx B\n\nthen commit".into(),
                2,
                true,
                agent::PromptOrigin::Typed
            ),
            Ok(2)
        );
        assert!(set.flush_queue(tab).is_none(), "a running turn holds the queue");
        assert_eq!(set.queued_count(), 2);

        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        let ended = pump_until_running(&mut set, &dir, tab, false);
        assert_eq!(ended, vec![tab], "the pump reports the end of the turn once");
        let flush = set.flush_queue(tab).expect("the queue goes out");
        assert!(flush.outcome.is_ok());
        assert_eq!(flush.typed, "and the tests\n\nthen commit");
        assert_eq!(
            provider.turns(),
            vec!["ctx A\n\nand the tests\n\nctx B\n\nthen commit".to_string()],
            "one turn, each item with the context captured when it was queued"
        );
        assert!(set.get(tab).unwrap().session.queue.is_empty());
        shut_down_all(&mut set);
    }

    /// Like vim's `:bd` (E89), `<leader>bd` asks only when closing would lose something.
    #[test]
    fn closing_asks_only_for_a_running_turn_a_connect_or_a_queue() {
        let dir = workspace("tabs-close-needs-confirm");
        let mut set = set();
        let empty = set.active();
        assert!(!set.close_needs_confirm(empty), "an empty tab has nothing to lose");
        assert!(!set.close_needs_confirm(TabId(999)), "no such tab");

        let connecting = set.open(AgentId::Claude);
        let (_tx, result_rx) = mpsc::channel();
        set.get_mut(connecting).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "req-1".into(),
            result_rx,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        });
        assert!(
            set.close_needs_confirm(connecting),
            "a connect in flight (it counts as running)"
        );

        let (provider, backend) = live(&dir);
        set.get_mut(empty).unwrap().backend = TabBackend::Live(backend);
        assert!(!set.close_needs_confirm(empty), "an idle live tab: nothing to lose");
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, empty, true);
        assert!(set.close_needs_confirm(empty), "a running turn");
        assert_eq!(
            set.queue_message(empty, "later", "later".into(), 1, true, agent::PromptOrigin::Typed),
            Ok(1)
        );
        assert!(set.close_needs_confirm(empty), "a running turn with a queue");
        shut_down_all(&mut set);
    }

    /// Seen in a GUI pass on 2026-09-25: the elapsed clock read `0s+` after a tab switch, because
    /// the panel holds only the active tab's state and restarted the clock at the snapshot. The tab
    /// now keeps when its running turn started, from the first tick that delivered it, and the
    /// snapshot a switch (or a reload) sends carries it.
    #[test]
    fn a_running_turn_keeps_its_start_time_across_ticks_and_the_snapshot_carries_it() {
        let dir = workspace("tabs-turn-clock");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(set.get(tab).unwrap().turn_started_at_ms(), None);
        let before = wall_clock_ms();
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        let stamp = set
            .get(tab)
            .unwrap()
            .turn_started_at_ms()
            .expect("stamped when the turn is delivered");
        assert!(stamp >= before && stamp <= wall_clock_ms());
        std::thread::sleep(Duration::from_millis(20));
        set.pump(&dir, true);
        assert_eq!(
            set.get(tab).unwrap().turn_started_at_ms(),
            Some(stamp),
            "a later tick keeps it"
        );

        let other = set.open(AgentId::Claude);
        set.pump(&dir, true);
        set.select(tab);
        let payloads = set.active_state_payloads();
        let snapshot: serde_json::Value = payloads
            .iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
            .find(|v| v["kind"] == "snapshot")
            .expect("a live tab's snapshot");
        assert_eq!(snapshot["turnStartedAtMs"], serde_json::json!(stamp));
        set.remove(other).unwrap();

        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        pump_until_running(&mut set, &dir, tab, false);
        until("the clock to clear", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().turn_started_at_ms().is_none()
        });
        shut_down_all(&mut set);
    }

    /// Pumps `tab` until a payload for the active tab carries `turnTimes`, returning that payload's
    /// `turnTimes`. Every payload the pump hands back is looked at, since the entry rides on whichever
    /// events envelope the tick that saw the turn end builds.
    fn pump_until_turn_times(set: &mut TabSet, dir: &Path) -> serde_json::Value {
        let mut found = None;
        until("an events payload carrying turnTimes", || {
            if let Some(payload) = set.pump(dir, true).active_payload {
                let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
                if value["kind"] == "events" && value.get("turnTimes").is_some() {
                    found = Some(value["turnTimes"].clone());
                }
            }
            found.is_some()
        });
        found.unwrap()
    }

    fn snapshot_turn_times(set: &mut TabSet) -> serde_json::Value {
        let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
        snapshot["turnTimes"].clone()
    }

    /// A turn that ends records how long it ran, on the events envelope of the batch that ended it
    /// and in every later snapshot; a second end is a second entry.
    #[test]
    fn a_completed_turn_records_its_duration_in_the_events_and_the_snapshot() {
        let dir = workspace("tabs-turn-times-completed");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(
            snapshot_turn_times(&mut set),
            serde_json::json!([]),
            "nothing has ended yet"
        );

        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        let started_at = set.get(tab).unwrap().turn_started_at_ms().expect("t1 stamped");
        std::thread::sleep(Duration::from_millis(25));
        provider.queue(text_delta("t1", "done"));
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        let times = pump_until_turn_times(&mut set, &dir);
        let times = times.as_array().unwrap();
        assert_eq!(times.len(), 1, "{times:?}");
        assert_eq!(times[0]["turnId"], "t1");
        assert_eq!(times[0]["startedAtMs"], serde_json::json!(started_at));
        let ended_at = times[0]["endedAtMs"].as_u64().unwrap();
        assert!(
            ended_at >= started_at + 25,
            "the turn ran at least the sleep: {started_at} -> {ended_at}"
        );
        assert_eq!(
            set.get(tab).unwrap().session.turn_times_unsent,
            0,
            "the events carried it"
        );
        let newest = {
            let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
            let transcript = snapshot["state"]["transcript"].as_array().unwrap().clone();
            transcript.last().expect("the reply is on the page")["seq"]
                .as_u64()
                .unwrap()
        };
        assert_eq!(
            times[0]["anchorSeq"],
            serde_json::json!(newest),
            "the duration line stands under the newest row"
        );

        // A tick with nothing new does not send it again.
        assert!(set.pump(&dir, true).active_payload.is_none());

        provider.queue(started("t2"));
        pump_until_running(&mut set, &dir, tab, true);
        provider.queue(completed("t2", agent::TurnOutcome::Completed));
        let second = pump_until_turn_times(&mut set, &dir);
        assert_eq!(
            second
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["turnId"].clone())
                .collect::<Vec<_>>(),
            vec![serde_json::json!("t2")],
            "only the turn that ended in this batch rides on the events envelope"
        );

        let all = snapshot_turn_times(&mut set);
        assert_eq!(
            all.as_array()
                .unwrap()
                .iter()
                .map(|t| t["turnId"].clone())
                .collect::<Vec<_>>(),
            vec![serde_json::json!("t1"), serde_json::json!("t2")],
            "the snapshot carries every recorded turn, oldest first"
        );
        assert_eq!(all[0]["endedAtMs"].as_u64().unwrap(), ended_at);
        shut_down_all(&mut set);
    }

    #[test]
    fn an_auto_answered_request_keeps_the_reply_revision_and_duration_anchor() {
        let dir = workspace("tabs-turn-times-revision-gap");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        // Leave a folded event queued while its snapshot is drawn: only the suffix needs tags.
        provider.queue(text_delta("t1", "covered"));
        wait_for_fold(&set, tab, "covered");
        set.active_state_payloads();
        let base = set.get(tab).unwrap().live().unwrap().projection().last_revision;
        provider.queue(call_started(
            "read",
            "Read",
            serde_json::json!({ "file_path": "main.rs" }),
        ));
        provider.queue(read("perm-read"));
        provider.queue(text_delta("t1", "done"));
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        until("the whole batch to fold before delivery", || {
            set.get(tab).unwrap().live().unwrap().projection().last_revision >= base + 4
        });
        let payload = set.pump(&dir, true).active_payload.expect("the completed batch");
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let events = value["events"].as_array().unwrap();
        let revisions = value["revisions"].as_array().unwrap();
        assert_eq!(events.len(), revisions.len());
        assert_eq!(
            revisions,
            &vec![
                serde_json::json!(base + 1),
                serde_json::json!(base + 3),
                serde_json::json!(base + 4)
            ]
        );
        assert!(events.iter().all(|event| event["type"] != "permission_requested"));
        assert_eq!(provider.resolutions(), vec![("perm-read".into(), true)]);
        let reply = events
            .iter()
            .position(|event| event["type"] == "content_delta")
            .unwrap();
        assert_eq!(
            value["turnTimes"][0]["anchorSeq"],
            revisions[reply].as_u64().unwrap() - 1
        );
        shut_down_all(&mut set);
    }

    /// A window can end with no chance to say anything, so a saved tab has to say a turn is running while it runs.
    #[test]
    fn a_saved_tab_says_a_turn_is_running_for_as_long_as_it_runs() {
        let dir = workspace("tabs-saved-turn");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(AgentDomainEvent::SessionOpened {
            session_id: "verdandi-1".into(),
            provider_session_id: "s-1".into(),
            model: "m".into(),
            cwd: dir.to_string_lossy().into_owned(),
        });
        until("the session", || !set.saved_snapshot(PROJECT).unwrap().is_empty());
        let running = |set: &TabSet| set.saved_snapshot(PROJECT).unwrap().tabs[0].turn_running;
        assert!(!running(&set));

        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        assert!(running(&set));

        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        pump_until_running(&mut set, &dir, tab, false);
        assert!(!running(&set));
        shut_down_all(&mut set);
    }

    /// A turn that ends badly still ran for some time, and the turn-ending row says how long.
    #[test]
    fn an_interrupted_turn_records_its_duration_too() {
        let dir = workspace("tabs-turn-times-interrupted");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        provider.queue(completed("t1", agent::TurnOutcome::Interrupted));
        let times = pump_until_turn_times(&mut set, &dir);
        assert_eq!(times[0]["turnId"], "t1");
        shut_down_all(&mut set);
    }

    /// A turn no tick saw running (it began and ended between two ticks) has no stamped start, so it
    /// has no duration: the panel shows nothing rather than a guess.
    #[test]
    fn a_turn_no_tick_saw_running_records_no_duration() {
        let dir = workspace("tabs-turn-times-unstamped");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let base = set.get(tab).unwrap().live().unwrap().projection().last_revision;
        provider.queue(started("t1"));
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        until("both events to fold, with no tick", || {
            set.get(tab).unwrap().live().unwrap().projection().last_revision >= base + 2
        });
        let mut carried = false;
        for _ in 0..5 {
            if let Some(payload) = set.pump(&dir, true).active_payload {
                carried |= payload.contains("turnTimes");
            }
        }
        assert!(!carried, "no payload names a duration");
        assert_eq!(snapshot_turn_times(&mut set), serde_json::json!([]));
        shut_down_all(&mut set);
    }

    /// Each tab keeps its own durations; a background tab's finished turn waits for the snapshot a
    /// switch sends instead of going out as an envelope for a tab nobody is looking at.
    #[test]
    fn a_background_tabs_turn_durations_wait_for_its_snapshot() {
        let dir = workspace("tabs-turn-times-background");
        let mut set = set();
        let background = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(background).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, background, true);
        let foreground = set.open(AgentId::Claude);
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        until("the background tab's turn to end", || {
            set.pump(&dir, true);
            !set.get(background).unwrap().session.was_running
        });
        assert_eq!(set.get(background).unwrap().session.turn_times_unsent, 1);
        set.select(background);
        let times = snapshot_turn_times(&mut set);
        assert_eq!(times[0]["turnId"], "t1");
        assert_eq!(set.get(background).unwrap().session.turn_times_unsent, 0);
        set.remove(foreground).unwrap();
        shut_down_all(&mut set);
    }

    #[test]
    fn the_turn_log_drops_its_oldest_entries_past_the_cap() {
        let mut clock = None;
        let mut times = Vec::new();
        let mut unsent = 0;
        let mut projection = agent::AgentSessionProjection::default();
        let mut observe = |active: Option<&str>, now: u64| {
            projection.active_turn_id = active.map(str::to_string);
            observe_turn_clock(&mut clock, &mut times, &mut unsent, &projection, now);
        };
        for n in 0..(MAX_TURN_TIMES + 5) {
            observe(Some(&format!("t{n}")), 1_000 + n as u64);
            observe(None, 1_500 + n as u64);
        }
        // The next turn id replacing a running one also ends it, but names no row to stand under.
        observe(Some("a"), 9_000);
        observe(Some("b"), 9_400);
        assert_eq!(times.len(), MAX_TURN_TIMES);
        assert_eq!(
            times[0].turn_id, "t6",
            "the oldest went, one per entry added past the cap"
        );
        assert_eq!(unsent, MAX_TURN_TIMES);
        let last = times.last().unwrap();
        assert_eq!(
            (last.turn_id.as_str(), last.ended_at_ms, last.anchor_seq),
            ("a", 9_400, None)
        );
    }

    #[test]
    fn a_turn_ends_on_the_agents_last_row_even_with_a_prompt_already_after_it() {
        let mut clock = None;
        let mut times = Vec::new();
        let mut unsent = 0;
        let mut projection = agent::AgentSessionProjection::default();
        projection.active_turn_id = Some("t1".to_string());
        observe_turn_clock(&mut clock, &mut times, &mut unsent, &projection, 1_000);
        projection.transcript.push(agent::TranscriptMessage {
            seq: 4,
            text: "done".to_string(),
        });
        projection.user_prompts.push(agent::UserPromptRecord {
            seq: 5,
            text: "next".to_string(),
            delivered: false,
        });
        projection.active_turn_id = None;
        observe_turn_clock(&mut clock, &mut times, &mut unsent, &projection, 3_000);
        assert_eq!(times[0].anchor_seq, Some(4));
    }

    /// A refused queue flush stays queued with the refusal reason and is not retried every tick.
    #[test]
    fn a_refused_flush_keeps_the_queue_and_is_not_retried_every_tick() {
        let dir = workspace("tabs-queue-refused");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        set.queue_message(tab, "later", "later".into(), 1, true, agent::PromptOrigin::Typed)
            .unwrap();
        provider.refuse_sends(true);
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        pump_until_running(&mut set, &dir, tab, false);
        let flush = set.flush_queue(tab).unwrap();
        assert!(flush.outcome.is_err());
        let t = set.get(tab).unwrap();
        assert_eq!(t.session.queue.len(), 1, "never dropped");
        assert!(t.session.queue_error.is_some(), "the line saying why");
        for _ in 0..10 {
            assert!(
                set.pump(&dir, true).turn_ended.is_empty(),
                "no new turn end, so no retry"
            );
        }
        assert_eq!(provider.turns().len(), 1, "one attempt");
        shut_down_all(&mut set);
    }

    /// A pending card holds the queue: a queued message waits until the card is answered.
    #[test]
    fn the_queue_waits_while_a_card_is_pending() {
        let dir = workspace("tabs-queue-card");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        provider.queue(write("perm-w"));
        until("the card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        set.queue_message(
            tab,
            "after the card",
            "after the card".into(),
            1,
            true,
            agent::PromptOrigin::Typed,
        )
        .unwrap();
        assert!(set.flush_queue(tab).is_none());
        assert!(provider.turns().is_empty());
        shut_down_all(&mut set);
    }

    /// After an interrupt the queue still goes out, including what `Ctrl+Enter` added.
    #[test]
    fn send_now_interrupts_and_the_queue_goes_out_when_the_turn_ends() {
        let dir = workspace("tabs-send-now");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = interruptible_live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        set.queue_message(tab, "one", "one".into(), 1, true, agent::PromptOrigin::Typed)
            .unwrap();
        match set.send_now(tab, "two", "two".into(), 2).unwrap() {
            SendNow::Interrupting(outcome) => assert!(outcome.is_ok()),
            SendNow::Flushed(_) => panic!("a running turn is interrupted first"),
        }
        assert_eq!(provider.interrupts(), 1);
        assert!(
            provider.turns().is_empty(),
            "nothing is sent before the interrupt lands"
        );
        provider.queue(completed("t1", agent::TurnOutcome::Interrupted));
        assert_eq!(pump_until_running(&mut set, &dir, tab, false), vec![tab]);
        set.flush_queue(tab).unwrap().outcome.unwrap();
        assert_eq!(provider.turns(), vec!["one\n\ntwo".to_string()]);
        shut_down_all(&mut set);
    }

    #[test]
    fn send_now_on_an_idle_tab_sends_at_once() {
        let dir = workspace("tabs-send-now-idle");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        match set.send_now(tab, "now", "ctx\n\nnow".into(), 1).unwrap() {
            SendNow::Flushed(Some(flush)) => assert_eq!(flush.typed, "now"),
            _ => panic!("idle: flushed at once"),
        }
        assert_eq!(provider.turns(), vec!["ctx\n\nnow".to_string()]);
        shut_down_all(&mut set);
    }

    #[test]
    fn take_back_returns_every_item_in_order_and_empties_the_queue() {
        let dir = workspace("tabs-take-back");
        let mut set = set();
        let tab = set.active();
        let (_provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        set.queue_message(tab, "a", "a".into(), 1, true, agent::PromptOrigin::Typed)
            .unwrap();
        set.queue_message(tab, "b", "b".into(), 2, true, agent::PromptOrigin::Typed)
            .unwrap();
        assert_eq!(set.take_back_queue(tab), vec!["a".to_string(), "b".to_string()]);
        assert!(set.get(tab).unwrap().session.queue.is_empty());
        assert!(set.take_back_queue(tab).is_empty());
        shut_down_all(&mut set);
    }

    /// Queueing requires a starting or live session; empty and failed tabs refuse it.
    #[test]
    fn queueing_needs_a_session_that_is_starting_or_alive() {
        let mut set = set();
        let tab = set.active();
        assert!(
            set.queue_message(tab, "x", "x".into(), 1, true, agent::PromptOrigin::Typed)
                .is_err(),
            "an empty tab sends, it does not queue"
        );
        let (_tx, result_rx) = mpsc::channel();
        set.get_mut(tab).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "r".into(),
            result_rx,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        });
        assert_eq!(
            set.queue_message(tab, "x", "x".into(), 1, true, agent::PromptOrigin::Typed),
            Ok(1),
            "the box is live while it starts"
        );
        set.get_mut(tab).unwrap().backend = TabBackend::Failed { reason: "no".into() };
        assert!(set
            .queue_message(tab, "y", "y".into(), 2, true, agent::PromptOrigin::Typed)
            .is_err());
    }

    /// `r` on an ended tab restores its queue to the draft.
    #[test]
    fn reset_folds_the_queue_and_the_old_draft_into_the_draft() {
        let mut set = set();
        let tab = set.active();
        {
            let t = set.get_mut(tab).unwrap();
            t.session.queue.push(Queued::held("q1", "q1".into(), 1));
            t.session.queue.push(Queued::held("q2", "q2".into(), 2));
            t.replace_draft("half".into());
            t.backend = TabBackend::Failed { reason: "gone".into() };
        }
        set.reset(tab).unwrap();
        let t = set.get(tab).unwrap();
        assert_eq!(t.session.draft, "q1\n\nq2\n\nhalf");
        assert!(t.session.queue.is_empty());
        assert_eq!(set.close_facts(tab).unwrap().queued, 0);
    }

    #[test]
    fn close_facts_and_the_window_count_the_queue_and_a_close_hands_the_texts_back() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        for (tab, text) in [(one, "a"), (two, "b"), (two, "c")] {
            set.get_mut(tab)
                .unwrap()
                .session
                .queue
                .push(Queued::held(text, text.into(), 0));
        }
        assert_eq!(set.close_facts(two).unwrap().queued, 2);
        assert_eq!(set.queued_count(), 3);
        assert_eq!(
            set.take_every_queued_text(),
            vec!["a".to_string(), "b".into(), "c".into()]
        );
        assert_eq!(set.queued_count(), 0);
    }

    /// Saved rules answer background tabs too, and a card eligible for a rule offers one.
    #[test]
    fn rules_answer_a_background_call_and_a_card_is_offered_the_rule_that_would() {
        let dir = workspace("tabs-rules");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let background = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(background).unwrap().backend = TabBackend::Live(backend);
        set.open(AgentId::Claude);
        let bash = |id: &str, command: &str| AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": command }),
            provider_prompt: None,
        };
        provider.queue(bash("perm-npm", "npm ci"));
        provider.queue(bash("perm-cargo", "cargo test --lib"));
        let mut changed = Vec::new();
        until("the cargo card", || {
            changed.extend(set.pump(&dir, true).offers_changed);
            set.get(background).unwrap().attention.attention().pending == 1
                && set
                    .get(background)
                    .unwrap()
                    .session
                    .rule_offers
                    .contains_key("perm-cargo")
        });
        assert_eq!(provider.resolutions(), vec![("perm-npm".to_string(), true)]);
        let offers = &set.get(background).unwrap().session.rule_offers;
        assert_eq!(offers["perm-cargo"].display(), "cargo test *");
        assert!(changed.contains(&background));
        shut_down_all(&mut set);
    }

    /// A call a saved rule answered is named in the events payload and in every later
    /// snapshot; a call the user was asked about, and one no rule matched, are not.
    #[test]
    fn a_call_a_rule_answered_is_named_in_events_and_snapshots() {
        let dir = workspace("tabs-rule-notes");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let call = |id: &str, command: &str| AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            name: "Bash".into(),
            input: serde_json::json!({ "command": command }),
        };
        let gate = |perm: &str, id: &str, command: &str| AgentDomainEvent::PermissionRequested {
            permission_id: perm.into(),
            tool_use_id: Some(id.into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": command }),
            provider_prompt: None,
        };
        let done = |id: &str| AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            content: serde_json::json!("ok"),
            is_error: false,
        };
        provider.queue(started("t1"));
        provider.queue(call("toolu_npm", "npm ci"));
        provider.queue(gate("perm-npm", "toolu_npm", "npm ci"));
        provider.queue(done("toolu_npm"));
        provider.queue(call("toolu_ls", "ls"));
        provider.queue(done("toolu_ls"));
        provider.queue(call("toolu_cargo", "cargo test"));
        provider.queue(gate("perm-cargo", "toolu_cargo", "cargo test"));
        provider.queue(done("toolu_cargo"));
        let mut payloads = Vec::new();
        until("the three calls to finish", || {
            payloads.extend(set.pump(&dir, true).active_payload);
            payloads
                .iter()
                .any(|p| p.contains("toolu_cargo") && p.contains("tool_call_completed"))
        });
        let notes: Vec<serde_json::Value> = payloads
            .iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
            .filter_map(|p| p.get("ruleNotes").cloned())
            .collect();
        assert_eq!(
            notes,
            vec![serde_json::json!([{ "toolUseId": "toolu_npm", "rule": "Bash(npm ci *)" }])]
        );
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        let calls = snapshot["state"]["toolCalls"].as_array().unwrap();
        let rule_of = |id: &str| {
            calls
                .iter()
                .find(|c| c["toolUseId"] == id)
                .unwrap()
                .get("allowedByRule")
                .cloned()
        };
        assert_eq!(rule_of("toolu_npm"), Some(serde_json::json!("Bash(npm ci *)")));
        assert_eq!(rule_of("toolu_ls"), None, "the classifier allowed it, not a rule");
        assert_eq!(rule_of("toolu_cargo"), None, "a card was shown");
        shut_down_all(&mut set);
    }

    /// An in-project `Write` the acceptEdits fast path answers with no card is
    /// named "allowed by auto" -- on the events envelope as it happens, and on the snapshot after --
    /// while a `Write` into a protected path (`.git/`, still carded, `write()`'s own reason for
    /// existing) carries no such note and stays a pending card.
    #[test]
    fn an_edit_the_fast_path_allowed_is_named_auto_in_events_and_snapshots() {
        let dir = workspace("tabs-auto-notes");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let started_call = |id: &str, name: &str, input: serde_json::Value| AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            name: name.into(),
            input,
        };
        let gate = |perm: &str, id: &str, name: &str, input: serde_json::Value| AgentDomainEvent::PermissionRequested {
            permission_id: perm.into(),
            tool_use_id: Some(id.into()),
            tool_name: name.into(),
            input,
            provider_prompt: None,
        };
        let done = |id: &str| AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            content: serde_json::json!("ok"),
            is_error: false,
        };
        provider.queue(started("t1"));
        provider.queue(started_call(
            "toolu_write",
            "Write",
            serde_json::json!({ "file_path": "new.rs", "content": "fn new() {}" }),
        ));
        provider.queue(gate(
            "perm-write",
            "toolu_write",
            "Write",
            serde_json::json!({ "file_path": "new.rs", "content": "fn new() {}" }),
        ));
        provider.queue(done("toolu_write"));
        provider.queue(started_call(
            "toolu_gitwrite",
            "Write",
            serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
        ));
        provider.queue(gate(
            "perm-gitwrite",
            "toolu_gitwrite",
            "Write",
            serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
        ));
        let mut payloads = Vec::new();
        until(
            "the fast path to answer the in-project write, and the protected one to card",
            || {
                payloads.extend(set.pump(&dir, true).active_payload);
                payloads
                    .iter()
                    .any(|p| p.contains("toolu_write") && p.contains("tool_call_completed"))
                    && payloads.iter().any(|p| p.contains("perm-gitwrite"))
            },
        );
        let auto_notes: Vec<serde_json::Value> = payloads
            .iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
            .filter_map(|p| p.get("autoNotes").cloned())
            .collect();
        assert_eq!(auto_notes, vec![serde_json::json!(["toolu_write"])]);
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        let calls = snapshot["state"]["toolCalls"].as_array().unwrap();
        let auto_of = |id: &str| {
            calls
                .iter()
                .find(|c| c["toolUseId"] == id)
                .unwrap()
                .get("allowedByAuto")
                .cloned()
        };
        assert_eq!(auto_of("toolu_write"), Some(serde_json::json!(true)));
        assert_eq!(auto_of("toolu_gitwrite"), None, "a card was shown, not the fast path");
        let pending = snapshot["state"]["pendingPermissions"].as_array().unwrap();
        assert!(
            pending.iter().any(|p| p["permissionId"] == "perm-gitwrite"),
            "the protected write is still a card, not silently allowed"
        );
        shut_down_all(&mut set);
    }

    /// A rule can never fire for `Write`/`Edit`/`NotebookEdit` at all
    /// (`agent::permission_policy`'s module doc), so a fast-path allow can never carry a rule note
    /// too -- pinned directly rather than left to follow from the two features never overlapping in
    /// practice.
    #[test]
    fn an_auto_allowed_edit_never_also_carries_a_rule_note() {
        let dir = workspace("tabs-auto-notes-no-rule");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        provider.queue(AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_edit".into(),
            name: "Edit".into(),
            input: serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" }),
        });
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-edit".into(),
            tool_use_id: Some("toolu_edit".into()),
            tool_name: "Edit".into(),
            input: serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" }),
            provider_prompt: None,
        });
        provider.queue(AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_edit".into(),
            content: serde_json::json!("ok"),
            is_error: false,
        });
        until("the edit to finish", || {
            set.pump(&dir, true)
                .active_payload
                .is_some_and(|p| p.contains("toolu_edit") && p.contains("tool_call_completed"))
        });
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        let call = &snapshot["state"]["toolCalls"][0];
        assert_eq!(call["allowedByAuto"], serde_json::json!(true));
        assert!(call.get("allowedByRule").is_none());
        shut_down_all(&mut set);
    }

    /// A `Write` answered by the acceptEdits fast path still marks its completed row as creating a
    /// file when the target did not exist. `note_new_files` runs from delivered permission requests,
    /// which automatic answering suppresses. The existence check must therefore run where
    /// `answer_what_needs_no_human` sends its allow, so a newly created file never gets an overwrite
    /// warning. Inferring creation from `ToolCallStarted` in Auto would miss other answering modes.
    #[test]
    fn an_auto_allowed_write_over_no_file_is_marked_as_creating_one() {
        let dir = workspace("tabs-auto-new-file");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        provider.queue(AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_new".into(),
            name: "Write".into(),
            input: serde_json::json!({ "file_path": "brand_new.rs", "content": "fn x() {}" }),
        });
        // The fast-path note is recorded at the gate answer. Without a gate request this fixture
        // would model a call rejected before permission checking, leaving no answer to annotate.
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-new".into(),
            tool_use_id: Some("toolu_new".into()),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": "brand_new.rs", "content": "fn x() {}" }),
            provider_prompt: None,
        });
        provider.queue(AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_new".into(),
            content: serde_json::json!("ok"),
            is_error: false,
        });
        let mut payloads = Vec::new();
        until("the fast-path write to complete", || {
            payloads.extend(set.pump(&dir, true).active_payload);
            payloads
                .iter()
                .any(|p| p.contains("toolu_new") && p.contains("tool_call_completed"))
        });
        let auto_creates: Vec<serde_json::Value> = payloads
            .iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
            .filter_map(|p| p.get("autoCreatesFile").cloned())
            .collect();
        assert_eq!(auto_creates, vec![serde_json::json!(["toolu_new"])]);
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        let call = snapshot["state"]["toolCalls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["toolUseId"] == "toolu_new")
            .unwrap();
        assert_eq!(call["createsFile"], serde_json::json!(true));
        assert_eq!(call["allowedByAuto"], serde_json::json!(true));
        shut_down_all(&mut set);
    }

    /// A queue overflow may drop a follow-up card's `PermissionRequested`, which normally removes
    /// a fast-path note candidate. The complete projection still has that permission pending,
    /// so `Resync` must draw the card and sweep its candidate. Otherwise human approval could
    /// complete the call with a false "allowed by auto" note.
    ///
    /// The fixture must first edit an ordinary in-project file the fast path actually answers.
    /// A protected `.git/probe` write cards immediately and creates no candidate, so it cannot
    /// detect a broken sweep. Overflowing the CLI's follow-up prompt for the same allowed call
    /// leaves a real candidate for the sweep to remove.
    #[test]
    fn a_resync_drops_a_fast_path_candidate_whose_further_card_survived_the_overflow() {
        let dir = workspace("tabs-resync-drops-candidate");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let input =
            serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" });

        provider.queue(started("t1"));
        provider.queue(call_started("toolu_edit", "Edit", input.clone()));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-gate".into(),
            tool_use_id: Some("toolu_edit".into()),
            tool_name: "Edit".into(),
            input: input.clone(),
            provider_prompt: None,
        });
        until("the fast path to answer the gate", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.host_answered.contains("perm-gate")
        });
        assert!(set
            .get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_edit", NoteKind::AutoEdit));

        // The CLI's own follow-up prompt for the same call, plus filler enough to overflow the
        // queue so the whole batch -- the prompt's `PermissionRequested` included -- is dropped and
        // this tick's drain is a `Resync`. Queued as one batch (`queue_all`'s doc): a `queue` call
        // per event would let the ingestion thread fold the follow-up card alone, well before the
        // filler loop even finishes pushing the rest, so the `until` below could succeed before the
        // batch had actually overflowed.
        let mut batch = vec![
            resolved("perm-gate"),
            cli_prompt("perm-edit", "toolu_edit", "Edit", input, None),
        ];
        batch.extend((0..(agent::UI_EVENT_QUEUE_CAPACITY + 20)).map(|i| text_delta("t1", &format!("chunk {i}"))));
        provider.queue_all(batch);
        until("the projection to carry the follow-up card", || {
            let backend = set.get(tab).unwrap().live().unwrap();
            backend.projection().pending_permissions.contains_key("perm-edit")
        });
        let out = set.pump(&dir, true);
        let payload: serde_json::Value = serde_json::from_str(&out.active_payload.expect("a payload")).unwrap();
        assert_eq!(payload["kind"], "snapshot", "the overflow must have become a Resync");
        assert!(
            !set.get(tab)
                .unwrap()
                .session
                .note_ledger
                .has("toolu_edit", NoteKind::AutoEdit),
            "the still-pending sweep must drop a candidate whose own further card is genuinely pending"
        );

        set.answer_card(tab, ViewId::BASE, "perm-edit", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        provider.queue(resolved("perm-edit"));
        provider.queue(AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_edit".into(),
            content: serde_json::json!("ok"),
            is_error: false,
        });
        until("the edit to complete", || {
            set.pump(&dir, true)
                .active_payload
                .is_some_and(|p| p.contains("toolu_edit") && p.contains("tool_call_completed"))
        });
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        let call = snapshot["state"]["toolCalls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["toolUseId"] == "toolu_edit")
            .unwrap();
        assert!(
            call.get("allowedByAuto").is_none(),
            "a human approved this on screen; it must never read allowed by auto"
        );
        shut_down_all(&mut set);
    }

    /// Helpers for exercising gate answers, follow-up prompts and completion independently.
    fn call_started(id: &str, name: &str, input: serde_json::Value) -> AgentDomainEvent {
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            name: name.into(),
            input,
        }
    }

    fn call_done(id: &str, is_error: bool) -> AgentDomainEvent {
        AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            content: serde_json::json!(if is_error { "File has not been read yet." } else { "ok" }),
            is_error,
        }
    }

    /// Pumps until `id`'s completion was delivered, returning every payload the pumps produced.
    fn pump_until_done(set: &mut TabSet, dir: &Path, id: &str) -> Vec<String> {
        let mut payloads = Vec::new();
        until("the call to complete", || {
            payloads.extend(set.pump(dir, true).active_payload);
            payloads
                .iter()
                .any(|p| p.contains(id) && p.contains("tool_call_completed"))
        });
        payloads
    }

    fn call_in_snapshot(set: &mut TabSet, id: &str) -> serde_json::Value {
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        snapshot["state"]["toolCalls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["toolUseId"] == id)
            .cloned()
            .unwrap()
    }

    fn auto_notes_in(payloads: &[String]) -> Vec<serde_json::Value> {
        payloads
            .iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
            .filter_map(|p| p.get("autoNotes").cloned())
            .collect()
    }

    /// The CLI validates input before `PreToolUse`. An Edit of a file not read yet or lacking
    /// `old_string` can finish with `is_error` without asking the gate. It must not claim
    /// "allowed by auto": no card does not imply an automatic answer.
    #[test]
    fn an_edit_that_failed_validation_before_the_gate_is_never_named_auto() {
        let dir = workspace("tabs-auto-validation-failed");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        provider.queue(call_started(
            "toolu_hosts",
            "Edit",
            serde_json::json!({ "file_path": "/etc/hosts", "old_string": "a", "new_string": "b" }),
        ));
        provider.queue(call_done("toolu_hosts", true));
        provider.queue(call_started(
            "toolu_main",
            "Edit",
            serde_json::json!({ "file_path": "main.rs", "old_string": "not there", "new_string": "b" }),
        ));
        provider.queue(call_done("toolu_main", true));
        let payloads = pump_until_done(&mut set, &dir, "toolu_main");
        assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
        for id in ["toolu_hosts", "toolu_main"] {
            assert!(call_in_snapshot(&mut set, id).get("allowedByAuto").is_none(), "{id}");
        }
        shut_down_all(&mut set);
    }

    /// A card without a tool-use id maps to `None` and still removes a matching note candidate.
    /// Human approval of an out-of-project Write must not complete it as "allowed by auto" merely
    /// because id matching cannot connect the card to its call.
    #[test]
    fn a_carded_edit_whose_request_names_no_tool_use_id_is_never_named_auto() {
        let dir = workspace("tabs-auto-card-without-id");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let input = serde_json::json!({ "file_path": "/etc/eitri-out.txt", "content": "x" });
        provider.queue(started("t1"));
        provider.queue(call_started("toolu_out", "Write", input.clone()));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-out".into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input,
            provider_prompt: None,
        });
        until("the card to be delivered", || {
            set.pump(&dir, true)
                .active_payload
                .is_some_and(|p| p.contains("perm-out"))
        });
        set.answer_card(tab, ViewId::BASE, "perm-out", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        provider.queue(resolved("perm-out"));
        provider.queue(call_done("toolu_out", false));
        let payloads = pump_until_done(&mut set, &dir, "toolu_out");
        assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
        assert!(call_in_snapshot(&mut set, "toolu_out").get("allowedByAuto").is_none());
        shut_down_all(&mut set);
    }

    /// A switch to bypass between call start and gate makes bypass answer the call, never the
    /// fast path. An Auto candidate must not survive into a note when the fast path answered nothing.
    #[test]
    fn an_edit_bypass_answered_after_a_switch_is_never_named_auto() {
        let dir = workspace("tabs-auto-then-bypass");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let input =
            serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" });
        provider.queue(started("t1"));
        provider.queue(call_started("toolu_sw", "Edit", input.clone()));
        until("the call to be delivered in Auto", || {
            set.pump(&dir, true)
                .active_payload
                .is_some_and(|p| p.contains("toolu_sw"))
        });
        set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-sw".into(),
            tool_use_id: Some("toolu_sw".into()),
            tool_name: "Edit".into(),
            input,
            provider_prompt: None,
        });
        provider.queue(call_done("toolu_sw", false));
        let payloads = pump_until_done(&mut set, &dir, "toolu_sw");
        assert!(
            set.get(tab).unwrap().session.host_answered.contains("perm-sw"),
            "bypass answered it"
        );
        assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
        assert!(call_in_snapshot(&mut set, "toolu_sw").get("allowedByAuto").is_none());
        shut_down_all(&mut set);
    }

    /// Bypass answers a Write's gate and suppresses its permission request, so `note_new_files`
    /// cannot rely on delivered requests. Checking file existence where the answer is sent in
    /// both Auto and bypass marks newly created files correctly; an existing file gets no creation
    /// note. Otherwise a new bypass-written file would keep a misleading overwrite warning.
    #[test]
    fn a_bypass_write_over_no_file_is_marked_as_creating_one() {
        let dir = workspace("tabs-bypass-new-file");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;
        provider.queue(started("t1"));
        for (id, perm, path) in [
            ("toolu_new", "perm-new", "fresh.rs"),
            ("toolu_old", "perm-old", "main.rs"),
        ] {
            let input = serde_json::json!({ "file_path": path, "content": "fn x() {}" });
            provider.queue(call_started(id, "Write", input.clone()));
            provider.queue(AgentDomainEvent::PermissionRequested {
                permission_id: perm.into(),
                tool_use_id: Some(id.into()),
                tool_name: "Write".into(),
                input,
                provider_prompt: None,
            });
            provider.queue(call_done(id, false));
        }
        pump_until_done(&mut set, &dir, "toolu_old");
        let fresh = call_in_snapshot(&mut set, "toolu_new");
        assert_eq!(fresh["createsFile"], serde_json::json!(true));
        assert!(fresh.get("allowedByAuto").is_none(), "bypass, not the fast path");
        assert!(call_in_snapshot(&mut set, "toolu_old").get("createsFile").is_none());
        shut_down_all(&mut set);
    }

    /// The created-file note must reach the panel on the events path too, not only in a later snapshot.
    /// A gate usually arrives in a batch of its own, after its call's start; answered and dropped, it
    /// leaves that batch empty (`RevisedDelivery::Nothing`), so a note made from the answer is held
    /// for the next events payload rather than lost with the batch.
    #[test]
    fn a_created_file_note_answered_in_a_batch_of_its_own_reaches_the_next_events_payload() {
        let dir = workspace("tabs-creates-file-alone");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;
        let input = serde_json::json!({ "file_path": "alone.rs", "content": "fn x() {}" });
        provider.queue(started("t1"));
        provider.queue(call_started("toolu_alone", "Write", input.clone()));
        let mut payloads = Vec::new();
        until("the call to be delivered", || {
            payloads.extend(set.pump(&dir, true).active_payload);
            payloads.iter().any(|p| p.contains("toolu_alone"))
        });
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-alone".into(),
            tool_use_id: Some("toolu_alone".into()),
            tool_name: "Write".into(),
            input,
            provider_prompt: None,
        });
        until("bypass to answer the gate", || {
            payloads.extend(set.pump(&dir, true).active_payload);
            set.get(tab).unwrap().session.host_answered.contains("perm-alone")
        });
        provider.queue(call_done("toolu_alone", false));
        payloads.extend(pump_until_done(&mut set, &dir, "toolu_alone"));
        let creates: Vec<serde_json::Value> = payloads
            .iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
            .filter_map(|p| p.get("autoCreatesFile").cloned())
            .collect();
        assert_eq!(creates, vec![serde_json::json!(["toolu_alone"])]);
        shut_down_all(&mut set);
    }

    /// A fast-path answer remains a candidate until its call completes. If queue overflow drops
    /// the completion, `Resync` must still turn it into a note. Finishing before the resync makes
    /// the finished sweep consume the candidate before the still-pending `host_answered` filter;
    /// `a_resync_before_the_call_finishes_keeps_its_fast_path_answer` pins that filter separately.
    #[test]
    fn a_resync_turns_a_fast_path_answer_whose_call_finished_into_its_note() {
        let dir = workspace("tabs-resync-keeps-auto-answer");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let input =
            serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" });
        provider.queue(started("t1"));
        provider.queue(call_started("toolu_fp", "Edit", input.clone()));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-fp".into(),
            tool_use_id: Some("toolu_fp".into()),
            tool_name: "Edit".into(),
            input,
            provider_prompt: None,
        });
        until("the fast path to answer the edit", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.host_answered.contains("perm-fp")
        });
        assert!(set
            .get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_fp", NoteKind::AutoEdit));
        // One batch, not one `queue` call per event (`queue_all`'s doc): otherwise the ingestion
        // thread can fold the completion alone, before the filler loop even finishes pushing the
        // rest of the batch, so the `until` below could succeed before the overflow actually did.
        let mut batch = vec![call_done("toolu_fp", false)];
        batch.extend((0..(agent::UI_EVENT_QUEUE_CAPACITY + 20)).map(|i| text_delta("t1", &format!("chunk {i}"))));
        provider.queue_all(batch);
        until("the projection to carry the completion", || {
            let backend = set.get(tab).unwrap().live().unwrap();
            backend
                .projection()
                .tool_calls
                .iter()
                .any(|c| c.tool_use_id == "toolu_fp" && c.result.is_some())
        });
        let out = set.pump(&dir, true);
        let payload: serde_json::Value = serde_json::from_str(&out.active_payload.expect("a payload")).unwrap();
        assert_eq!(payload["kind"], "snapshot", "the overflow must have become a Resync");
        assert_eq!(
            call_in_snapshot(&mut set, "toolu_fp")["allowedByAuto"],
            serde_json::json!(true)
        );
        assert!(!set
            .get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_fp", NoteKind::AutoEdit));
        shut_down_all(&mut set);
    }

    /// A `Resync` after the fast path answers an edit's gate but before completion must keep its
    /// note candidate. The projection may still list the unresolved request as pending;
    /// `host_answered` must exclude it from the still-pending sweep, or an early resync would
    /// remove the finished call's "allowed by auto" note.
    #[test]
    fn a_resync_before_the_call_finishes_keeps_its_fast_path_answer() {
        let dir = workspace("tabs-resync-before-completion");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let input =
            serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" });
        provider.queue(started("t1"));
        provider.queue(call_started("toolu_pa", "Edit", input.clone()));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-pa".into(),
            tool_use_id: Some("toolu_pa".into()),
            tool_name: "Edit".into(),
            input,
            provider_prompt: None,
        });
        until("the fast path to answer the edit", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.host_answered.contains("perm-pa")
        });
        // One batch (`queue_all`'s doc), for the same reason every other prefix+filler sequence in
        // this file uses it, even though this particular `until` loops on `pump` itself below and so
        // has no premature-success race of its own.
        provider.queue_all(
            (0..(agent::UI_EVENT_QUEUE_CAPACITY + 20))
                .map(|i| text_delta("t1", &format!("chunk {i}")))
                .collect(),
        );
        let mut saw_snapshot = false;
        until("a resync", || {
            if let Some(p) = set.pump(&dir, true).active_payload {
                let v: serde_json::Value = serde_json::from_str(&p).unwrap();
                saw_snapshot |= v["kind"] == "snapshot";
            }
            saw_snapshot
        });
        assert!(
            set.get(tab)
                .unwrap()
                .session
                .note_ledger
                .has("toolu_pa", NoteKind::AutoEdit),
            "the resync dropped a fast-path candidate whose call had not finished"
        );
        provider.queue(call_done("toolu_pa", false));
        pump_until_done(&mut set, &dir, "toolu_pa");
        assert_eq!(
            call_in_snapshot(&mut set, "toolu_pa")["allowedByAuto"],
            serde_json::json!(true)
        );
        shut_down_all(&mut set);
    }

    /// When the fast path allows the gate and a human then approves the CLI's follow-up prompt
    /// for that call, the completed row must not say "allowed by auto".
    /// `NoteLedger::fold` removes the candidate when `PermissionRequested` arrives.
    #[test]
    fn the_clis_own_prompt_after_the_fast_path_drops_the_auto_note() {
        let dir = workspace("tabs-cli-prompt-drops-auto");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let input =
            serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" });
        provider.queue(started("t1"));
        provider.queue(call_started("toolu_pb", "Edit", input.clone()));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-pb-gate".into(),
            tool_use_id: Some("toolu_pb".into()),
            tool_name: "Edit".into(),
            input: input.clone(),
            provider_prompt: None,
        });
        until("the fast path to answer the gate", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.host_answered.contains("perm-pb-gate")
        });
        assert!(set
            .get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_pb", NoteKind::AutoEdit));
        provider.queue(resolved("perm-pb-gate"));
        provider.queue(cli_prompt("perm-pb-cli", "toolu_pb", "Edit", input, None));
        until("the CLI's own prompt as a card", || {
            set.pump(&dir, true)
                .active_payload
                .is_some_and(|p| p.contains("perm-pb-cli"))
        });
        set.answer_card(tab, ViewId::BASE, "perm-pb-cli", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        provider.queue(resolved("perm-pb-cli"));
        provider.queue(call_done("toolu_pb", false));
        let payloads = pump_until_done(&mut set, &dir, "toolu_pb");
        assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
        assert!(call_in_snapshot(&mut set, "toolu_pb").get("allowedByAuto").is_none());
        shut_down_all(&mut set);
    }

    /// Only fast-path answers for `Write`/`Edit`/`NotebookEdit` (`ACCEPT_EDITS_TOOLS`) become auto-note
    /// candidates. A `Read` allowed by `answer_what_needs_no_human` must not say "allowed by auto":
    /// that note identifies the acceptEdits fast path rather than the read-only rules shared by modes.
    #[test]
    fn a_classifier_allowed_read_is_never_named_auto() {
        let dir = workspace("tabs-read-not-auto");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let input = serde_json::json!({ "file_path": "main.rs" });
        provider.queue(started("t1"));
        provider.queue(call_started("toolu_pc", "Read", input.clone()));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-pc".into(),
            tool_use_id: Some("toolu_pc".into()),
            tool_name: "Read".into(),
            input,
            provider_prompt: None,
        });
        until("the classifier to answer the read", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.host_answered.contains("perm-pc")
        });
        assert!(!set
            .get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_pc", NoteKind::AutoEdit));
        provider.queue(call_done("toolu_pc", false));
        let payloads = pump_until_done(&mut set, &dir, "toolu_pc");
        assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
        assert!(call_in_snapshot(&mut set, "toolu_pc").get("allowedByAuto").is_none());
        shut_down_all(&mut set);
    }

    /// A queue overflow may drop the CLI follow-up prompt's `PermissionRequested`, which would
    /// normally remove a saved-rule note candidate. The complete projection still lists the prompt
    /// pending, so `Resync` draws its card and must sweep the candidate. Otherwise the human's later
    /// approval could produce a false "allowed by rule" note.
    ///
    /// The fixture must answer an actual gate request through the rule before overflowing the
    /// follow-up prompt for the same call. A bare `ToolCallStarted` cannot establish a candidate:
    /// notes are recorded only from answers `AgentBackend::answer_what_needs_no_human` actually gave.
    #[test]
    fn a_resync_drops_a_rule_candidate_whose_further_card_survived_the_overflow() {
        let dir = workspace("tabs-resync-drops-rule-candidate");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let input = serde_json::json!({ "command": "npm ci" });

        provider.queue(started("t1"));
        provider.queue(call_started("toolu_npm", "Bash", input.clone()));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-gate".into(),
            tool_use_id: Some("toolu_npm".into()),
            tool_name: "Bash".into(),
            input: input.clone(),
            provider_prompt: None,
        });
        until("the rule to answer the gate as a candidate", || {
            set.pump(&dir, true);
            set.get(tab)
                .unwrap()
                .session
                .note_ledger
                .has("toolu_npm", NoteKind::Rule)
        });

        // The CLI's own follow-up prompt for the same call, plus filler enough to overflow the
        // queue so the whole batch -- the prompt's `PermissionRequested` included -- is dropped and
        // this tick's drain is a `Resync`. Queued as one batch (`queue_all`'s doc): a `queue` call
        // per event would let the ingestion thread fold the follow-up card alone, well before the
        // filler loop even finishes pushing the rest, so the `until` below could succeed before the
        // batch had actually overflowed.
        let mut batch = vec![
            resolved("perm-gate"),
            cli_prompt("perm-npm", "toolu_npm", "Bash", input, None),
        ];
        batch.extend((0..(agent::UI_EVENT_QUEUE_CAPACITY + 20)).map(|i| text_delta("t1", &format!("chunk {i}"))));
        provider.queue_all(batch);
        until("the projection to carry the follow-up card", || {
            let backend = set.get(tab).unwrap().live().unwrap();
            backend.projection().pending_permissions.contains_key("perm-npm")
        });
        let out = set.pump(&dir, true);
        let payload: serde_json::Value = serde_json::from_str(&out.active_payload.expect("a payload")).unwrap();
        assert_eq!(payload["kind"], "snapshot", "the overflow must have become a Resync");
        assert!(
            !set.get(tab)
                .unwrap()
                .session
                .note_ledger
                .has("toolu_npm", NoteKind::Rule),
            "the still-pending sweep must drop a candidate whose own further card is genuinely pending"
        );

        set.answer_card(tab, ViewId::BASE, "perm-npm", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        provider.queue(resolved("perm-npm"));
        provider.queue(AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_npm".into(),
            content: serde_json::json!("ok"),
            is_error: false,
        });
        until("the npm call to complete", || {
            set.pump(&dir, true)
                .active_payload
                .is_some_and(|p| p.contains("toolu_npm") && p.contains("tool_call_completed"))
        });
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        let call = snapshot["state"]["toolCalls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["toolUseId"] == "toolu_npm")
            .unwrap();
        assert!(
            call.get("allowedByRule").is_none(),
            "a human approved this on screen; it must never read allowed by rule"
        );
        shut_down_all(&mut set);
    }

    /// When a snapshot already carried every event of the next drain, the payload
    /// leaves them all out -- but a note those events produce was not in that snapshot (it was read
    /// before this tab folded them), so it still goes to the panel, on an `events` envelope with no
    /// events, which the panel applies to what it already holds (`applyCallNotes`). The completion
    /// folds through the seam, right after a tick's drain and before the switch's snapshot read.
    #[test]
    fn a_note_from_events_a_snapshot_carried_still_reaches_the_panel() {
        let dir = workspace("tabs-p1a2-round2-notes");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        provider.queue(AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_npm".into(),
            name: "Bash".into(),
            input: serde_json::json!({ "command": "npm ci" }),
        });
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-npm".into(),
            tool_use_id: Some("toolu_npm".into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "npm ci" }),
            provider_prompt: None,
        });
        until("the rule to answer the call's gate", || {
            set.pump(&dir, true);
            provider.resolutions() == vec![("perm-npm".to_string(), true)]
        });

        // The call completes right after a tick's drain, before the switch's snapshot read.
        let completion = Arc::clone(&provider);
        set.after_drain = Some(Box::new(move |backend: &AgentBackend| {
            completion.queue(AgentDomainEvent::ToolCallCompleted {
                turn_id: "t1".into(),
                tool_use_id: "toolu_npm".into(),
                content: serde_json::json!("ok"),
                is_error: false,
            });
            until("the completion to fold", || {
                backend
                    .projection()
                    .tool_calls
                    .iter()
                    .any(|call| call.tool_use_id == "toolu_npm" && call.result.is_some())
            });
        }));
        set.pump(&dir, true);
        assert!(set.after_drain.is_none(), "the seam ran");
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        let call = &snapshot["state"]["toolCalls"][0];
        assert_eq!(call["toolUseId"], "toolu_npm");
        assert!(!call["result"].is_null(), "the snapshot carries the completion");
        assert!(
            call.get("allowedByRule").is_none(),
            "but not its note: nothing had folded it yet"
        );

        let payload: serde_json::Value = serde_json::from_str(
            &set.pump(&dir, true)
                .active_payload
                .expect("the note is owed to the panel"),
        )
        .unwrap();
        assert_eq!(payload["kind"], "events");
        assert_eq!(
            payload["events"],
            serde_json::json!([]),
            "the completion is not delivered twice"
        );
        assert_eq!(
            payload["ruleNotes"],
            serde_json::json!([{ "toolUseId": "toolu_npm", "rule": "Bash(npm ci *)" }])
        );
        shut_down_all(&mut set);
    }

    /// A `Write` card whose file is not there when it is raised says it creates one,
    /// in the events payload and in later snapshots (on the card and on its call); a card over an
    /// existing file does not, nor does its file appearing later change what the card said.
    #[test]
    fn a_write_card_over_no_file_is_marked_as_creating_one() {
        // Both targets use `.git/` so they card regardless of whether the file exists. Ordinary
        // `new.txt` and `old.txt` would take the acceptEdits fast path; this test needs real cards
        // to pin their `createsFile` flags.
        let dir = workspace("tabs-new-file");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/old.txt"), "there").unwrap();
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let write = |perm: &str, id: &str, path: std::path::PathBuf| AgentDomainEvent::PermissionRequested {
            permission_id: perm.into(),
            tool_use_id: Some(id.into()),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": path, "content": "x" }),
            provider_prompt: None,
        };
        provider.queue(AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_new".into(),
            name: "Write".into(),
            input: serde_json::json!({ "file_path": dir.join(".git/new.txt"), "content": "x" }),
        });
        provider.queue(write("perm-new", "toolu_new", dir.join(".git/new.txt")));
        provider.queue(write("perm-old", "toolu_old", dir.join(".git/old.txt")));
        let mut payloads = Vec::new();
        until("both cards", || {
            payloads.extend(set.pump(&dir, true).active_payload);
            set.get(tab).unwrap().attention.attention().pending == 2
        });
        let marked: Vec<serde_json::Value> = payloads
            .iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
            .filter_map(|p| p.get("createsFile").cloned())
            .collect();
        assert_eq!(
            marked,
            vec![serde_json::json!([{ "permissionId": "perm-new", "toolUseId": "toolu_new" }])]
        );
        std::fs::write(dir.join(".git/new.txt"), "now there").unwrap();
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        let cards = snapshot["state"]["pendingPermissions"].as_array().unwrap();
        let creates = |id: &str| {
            cards
                .iter()
                .find(|c| c["permissionId"] == id)
                .unwrap()
                .get("createsFile")
                .cloned()
        };
        assert_eq!(creates("perm-new"), Some(serde_json::json!(true)));
        assert_eq!(creates("perm-old"), None);
        assert_eq!(
            snapshot["state"]["toolCalls"][0]["createsFile"],
            serde_json::json!(true)
        );
        shut_down_all(&mut set);
    }

    /// Only "not found" is no file. A path this process cannot
    /// look at (a parent it may not search) or one through a regular file says nothing about what
    /// the write replaces, so its card keeps the overwrite warning. A dangling symlink is something.
    #[test]
    fn only_a_path_that_is_not_found_counts_as_a_new_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = workspace("tabs-new-file-errors");
        std::fs::write(dir.join("plain.txt"), "a file").unwrap();
        std::os::unix::fs::symlink(dir.join("nowhere"), dir.join("dangling")).unwrap();
        let locked = dir.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("inside.txt"), "hidden").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let card = |perm: &str, path: &str| AgentDomainEvent::PermissionRequested {
            permission_id: perm.into(),
            tool_use_id: Some(format!("toolu_{perm}")),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": path, "content": "x" }),
            provider_prompt: None,
        };
        let events = vec![
            card("absent", "absent.txt"),
            card("locked", "locked/inside.txt"),
            card("through-a-file", "plain.txt/child"),
            card("dangling", "dangling"),
            card("existing", "plain.txt"),
        ];
        let mut notes = std::collections::BTreeMap::new();
        let fresh = note_new_files(&mut notes, &events, &dir);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Unmarked either way for `locked`: as a user its lookup is refused (EACCES), and root, who
        // may search a 000 directory, finds the file there.
        assert_eq!(fresh.keys().map(String::as_str).collect::<Vec<_>>(), vec!["absent"]);
        assert_eq!(notes, fresh);
    }

    fn npm_card(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: format!("perm-{id}"),
            tool_use_id: Some(id.into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "npm ci" }),
            provider_prompt: None,
        }
    }

    fn npm_rule_candidate() -> NoteCandidate {
        NoteCandidate::Rule {
            rule: "Bash(npm ci *)".to_string(),
            call: CandidateCall {
                tool_name: "Bash".to_string(),
                input: serde_json::json!({ "command": "npm ci" }),
            },
        }
    }

    /// A rule's answer is noted only once its call completes, and only if no card for the same call
    /// came first: such a card means a human answered too (the rule's own answer failed, or the CLI
    /// asked again after the gate).
    #[test]
    fn a_card_for_a_rule_answered_call_drops_its_note_and_a_surviving_one_is_noted_on_completion() {
        let mut ledger = NoteLedger::default();
        ledger.record("toolu_carded".to_string(), npm_rule_candidate());
        ledger.record("toolu_ruled".to_string(), npm_rule_candidate());
        let mut kept = CallNotes::default();
        // Spread over two batches, as a card and its answer are: the candidate outlives the first.
        let mut first = CallNotes::default();
        ledger.fold(&[npm_card("toolu_carded")], &mut kept, &mut first);
        assert!(first.is_empty());
        let mut second = CallNotes::default();
        ledger.fold(
            &[call_done("toolu_carded", false), call_done("toolu_ruled", false)],
            &mut kept,
            &mut second,
        );
        assert_eq!(
            second.allowed_by_rule.into_iter().collect::<Vec<_>>(),
            vec![("toolu_ruled".to_string(), "Bash(npm ci *)".to_string())]
        );
        assert!(ledger.is_empty());
        assert!(
            !kept.allowed_by_rule.contains_key("toolu_carded"),
            "a card was delivered for it"
        );
        assert_eq!(
            kept.allowed_by_rule.get("toolu_ruled"),
            Some(&"Bash(npm ci *)".to_string())
        );
    }

    /// A subagent's call is answered by the same gate as the parent's, and arrives inside a frame: its
    /// completion still turns its candidate into the note, and nothing is left waiting in the ledger.
    #[test]
    fn a_subagent_call_answered_by_a_rule_is_noted_when_its_framed_completion_arrives() {
        let mut ledger = NoteLedger::default();
        ledger.record("toolu_child".to_string(), npm_rule_candidate());
        let mut kept = CallNotes::default();
        let mut fresh = CallNotes::default();
        ledger.fold(
            &[AgentDomainEvent::SubagentFrame {
                parent_tool_use_id: "toolu_agent".to_string(),
                frame: Box::new(call_done("toolu_child", false)),
            }],
            &mut kept,
            &mut fresh,
        );
        assert_eq!(
            fresh.allowed_by_rule.get("toolu_child"),
            Some(&"Bash(npm ci *)".to_string())
        );
        assert!(ledger.is_empty(), "the candidate is not kept once its call completed");
    }

    /// One call can wait for two notes at once: its gate answered by a rule, then the CLI's own prompt
    /// for it answered without a card. A card for the call drops the gate's note (someone answered
    /// after the rule), never the prompt's own, which the call's completion then shows.
    #[test]
    fn a_call_keeps_its_prompt_note_when_a_card_drops_its_rule_note() {
        let mut ledger = NoteLedger::default();
        ledger.record("toolu_both".to_string(), npm_rule_candidate());
        ledger.record(
            "toolu_both".to_string(),
            NoteCandidate::PromptNote("Claude Code asked — allowed in bypass".to_string()),
        );
        let mut kept = CallNotes::default();
        let mut fresh = CallNotes::default();
        ledger.fold(
            &[npm_card("toolu_both"), call_done("toolu_both", false)],
            &mut kept,
            &mut fresh,
        );
        assert!(fresh.allowed_by_rule.is_empty(), "the card dropped the rule's note");
        assert_eq!(
            fresh.prompt_notes.get("toolu_both").map(String::as_str),
            Some("Claude Code asked — allowed in bypass")
        );
        assert_eq!(kept, fresh);
        assert!(ledger.is_empty());
    }

    /// A resync reads the projection in place of the events it dropped: every candidate whose call
    /// finished becomes its note; a gate's candidate whose call still has a card nobody answered for
    /// the user is dropped, while one whose only pending request this tab answered itself, and a CLI
    /// prompt's note, wait on.
    #[test]
    fn a_resync_notes_finished_calls_and_drops_gate_candidates_with_a_card_still_pending() {
        let edit = |path: &str| {
            NoteCandidate::AutoEdit(CandidateCall {
                tool_name: "Edit".to_string(),
                input: serde_json::json!({ "file_path": path }),
            })
        };
        let mut ledger = NoteLedger::default();
        ledger.record("toolu_done".to_string(), edit("done.rs"));
        ledger.record("toolu_done".to_string(), npm_rule_candidate());
        ledger.record("toolu_carded".to_string(), edit("carded.rs"));
        ledger.record("toolu_carded".to_string(), npm_rule_candidate());
        ledger.record(
            "toolu_carded".to_string(),
            NoteCandidate::PromptNote("Claude Code asked — allowed with your approval".to_string()),
        );
        ledger.record("toolu_ours".to_string(), edit("ours.rs"));
        let begun = |id: &str| AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            name: "Edit".into(),
            input: serde_json::json!({}),
        };
        let mut projection = agent::AgentSessionProjection::default();
        for event in [
            started("t1"),
            begun("toolu_done"),
            call_done("toolu_done", false),
            begun("toolu_carded"),
            npm_card("toolu_carded"),
            begun("toolu_ours"),
            npm_card("toolu_ours"),
        ] {
            projection.apply(&event);
        }
        assert_eq!(projection.pending_permissions.len(), 2, "both cards are still pending");
        let host_answered: BTreeSet<String> = ["perm-toolu_ours".to_string()].into();
        let mut kept = CallNotes::default();
        ledger.resync(&projection, &host_answered, &mut kept);
        assert!(kept.allowed_by_auto.contains("toolu_done"));
        assert_eq!(
            kept.allowed_by_rule.get("toolu_done").map(String::as_str),
            Some("Bash(npm ci *)")
        );
        assert_eq!(kept.allowed_by_auto.len() + kept.allowed_by_rule.len(), 2);
        assert!(kept.prompt_notes.is_empty(), "no prompt's call finished");
        assert!(
            !ledger.has("toolu_carded", NoteKind::AutoEdit),
            "a human answers its card"
        );
        assert!(!ledger.has("toolu_carded", NoteKind::Rule), "a human answers its card");
        assert!(
            ledger.has("toolu_carded", NoteKind::PromptNote),
            "a card never drops a prompt's note"
        );
        assert!(
            ledger.has("toolu_ours", NoteKind::AutoEdit),
            "the only pending request is one this tab answered"
        );
    }

    /// A saved-rule note must come from an actual gate answer, never from reclassifying a started
    /// call's arguments and seeing no card. The CLI validates input before `PreToolUse`, so a Bash
    /// call can finish with `is_error` without ever requesting permission. Such a call must not
    /// claim "allowed by rule npm ci *" when the rule answered nothing.
    #[test]
    fn a_bash_call_that_failed_validation_before_the_gate_is_never_named_by_rule() {
        let dir = workspace("tabs-rule-validation-failed");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        provider.queue(call_started(
            "toolu_ci",
            "Bash",
            serde_json::json!({ "command": "npm ci" }),
        ));
        provider.queue(call_done("toolu_ci", true));
        let payloads = pump_until_done(&mut set, &dir, "toolu_ci");
        let notes: Vec<serde_json::Value> = payloads
            .iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
            .filter_map(|p| p.get("ruleNotes").cloned())
            .collect();
        assert!(notes.is_empty(), "{payloads:?}");
        assert!(call_in_snapshot(&mut set, "toolu_ci").get("allowedByRule").is_none());
        shut_down_all(&mut set);
    }

    /// Switching to bypass between call start and its gate makes bypass answer the call without
    /// classification. A speculative rule candidate from the started call's arguments must not
    /// survive as a rule note when no rule answered the gate.
    #[test]
    fn a_bash_call_bypass_answered_after_a_switch_is_never_named_by_rule() {
        let dir = workspace("tabs-rule-then-bypass");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        provider.queue(call_started(
            "toolu_sw",
            "Bash",
            serde_json::json!({ "command": "npm ci" }),
        ));
        until("the call to be delivered in Auto", || {
            set.pump(&dir, true)
                .active_payload
                .is_some_and(|p| p.contains("toolu_sw"))
        });
        set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-sw".into(),
            tool_use_id: Some("toolu_sw".into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "npm ci" }),
            provider_prompt: None,
        });
        provider.queue(call_done("toolu_sw", false));
        let payloads = pump_until_done(&mut set, &dir, "toolu_sw");
        assert!(
            set.get(tab).unwrap().session.host_answered.contains("perm-sw"),
            "bypass answered it"
        );
        let notes: Vec<serde_json::Value> = payloads
            .iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
            .filter_map(|p| p.get("ruleNotes").cloned())
            .collect();
        assert!(notes.is_empty(), "{payloads:?}");
        assert!(call_in_snapshot(&mut set, "toolu_sw").get("allowedByRule").is_none());
        shut_down_all(&mut set);
    }

    /// The CLI's follow-up prompt for a rule-answered call can omit its tool-use id: Verdandi sends
    /// `toolUseId: ''`, and `translate.rs` maps it to `None`. Id matching alone cannot remove that
    /// call's note candidate. `NoteLedger::dropped_by_card` matches content instead, so human
    /// approval of an id-less card cannot leave a false "allowed by rule" note on completion.
    #[test]
    fn an_id_less_follow_up_card_a_human_answers_drops_the_rule_candidate_by_content() {
        let dir = workspace("tabs-rule-id-less-follow-up");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let input = serde_json::json!({ "command": "npm ci" });

        provider.queue(started("t1"));
        provider.queue(call_started("toolu_idless", "Bash", input.clone()));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-gate".into(),
            tool_use_id: Some("toolu_idless".into()),
            tool_name: "Bash".into(),
            input: input.clone(),
            provider_prompt: None,
        });
        until("the rule to answer the gate as a candidate", || {
            set.pump(&dir, true);
            set.get(tab)
                .unwrap()
                .session
                .note_ledger
                .has("toolu_idless", NoteKind::Rule)
        });

        // The CLI follow-up prompt omits its tool-use id. `needs_its_own_card()` is false because
        // it has a reason, no ask rule and a known origin. Even so, `HumanApprovals::matches` never
        // matches an id-less prompt, so Auto draws a card rather than re-answering it through a rule.
        provider.queue(resolved("perm-gate"));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-follow-up".into(),
            tool_use_id: None,
            tool_name: "Bash".into(),
            input: input.clone(),
            provider_prompt: Some(agent::ProviderPrompt {
                reason: Some("Claude requested permissions to run this command.".into()),
                description: None,
                blocked_path: None,
                matched_ask_rule: None,
                unrecognized_origin: None,
            }),
        });
        until("the id-less follow-up card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        assert!(
            !set.get(tab)
                .unwrap()
                .session
                .note_ledger
                .has("toolu_idless", NoteKind::Rule),
            "the id-less follow-up card must still drop the candidate, matched by content"
        );

        set.answer_card(tab, ViewId::BASE, "perm-follow-up", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        provider.queue(resolved("perm-follow-up"));
        provider.queue(call_done("toolu_idless", false));
        pump_until_done(&mut set, &dir, "toolu_idless");
        assert!(
            call_in_snapshot(&mut set, "toolu_idless")
                .get("allowedByRule")
                .is_none(),
            "a human approved the follow-up card; it must never read allowed by rule"
        );
        shut_down_all(&mut set);
    }

    /// A rule-eligible call whose automatic answer the provider refuses stays a card. `by_rule`
    /// records candidates only after a successful answer, so this refusal creates none. An empty
    /// tool-use id also exercises content matching in `NoteLedger::dropped_by_card`: it must find
    /// nothing to drop, and human approval must not yield an "allowed by rule" note.
    #[test]
    fn a_carded_rule_request_a_human_approves_is_never_named_by_rule() {
        let dir = workspace("tabs-rule-refused-answer");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.refuse_resolutions(true);
        provider.queue(started("t1"));
        provider.queue(call_started(
            "toolu_refused",
            "Bash",
            serde_json::json!({ "command": "npm ci" }),
        ));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-refused".into(),
            tool_use_id: None,
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "npm ci" }),
            provider_prompt: None,
        });
        until("the refused rule answer to fall back to a card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        assert!(
            set.get(tab).unwrap().session.note_ledger.count(NoteKind::Rule) == 0,
            "the answer failed; no candidate was ever recorded"
        );
        provider.refuse_resolutions(false);
        set.answer_card(tab, ViewId::BASE, "perm-refused", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        provider.queue(resolved("perm-refused"));
        provider.queue(call_done("toolu_refused", false));
        pump_until_done(&mut set, &dir, "toolu_refused");
        assert!(call_in_snapshot(&mut set, "toolu_refused")
            .get("allowedByRule")
            .is_none());
        shut_down_all(&mut set);
    }

    /// A rule-answered call whose completion was dropped by queue overflow must become a note
    /// through `Resync`'s finished sweep, just like prompt and fast-path candidates.
    /// Without sweeping rule candidates too, they remain live indefinitely and never display a note.
    #[test]
    fn a_resync_turns_a_rule_answer_whose_call_finished_into_its_note() {
        let dir = workspace("tabs-resync-keeps-rule-answer");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        provider.queue(call_started(
            "toolu_rp",
            "Bash",
            serde_json::json!({ "command": "npm ci" }),
        ));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-rp".into(),
            tool_use_id: Some("toolu_rp".into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "npm ci" }),
            provider_prompt: None,
        });
        until("the rule to answer the call", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.host_answered.contains("perm-rp")
        });
        assert!(set
            .get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_rp", NoteKind::Rule));
        // One batch, not one `queue` call per event (`queue_all`'s doc): otherwise the ingestion
        // thread can fold the completion alone, before the filler loop even finishes pushing the
        // rest of the batch, so the `until` below could succeed before the overflow actually did.
        let mut batch = vec![call_done("toolu_rp", false)];
        batch.extend((0..(agent::UI_EVENT_QUEUE_CAPACITY + 20)).map(|i| text_delta("t1", &format!("chunk {i}"))));
        provider.queue_all(batch);
        until("the projection to carry the completion", || {
            let backend = set.get(tab).unwrap().live().unwrap();
            backend
                .projection()
                .tool_calls
                .iter()
                .any(|c| c.tool_use_id == "toolu_rp" && c.result.is_some())
        });
        let out = set.pump(&dir, true);
        let payload: serde_json::Value = serde_json::from_str(&out.active_payload.expect("a payload")).unwrap();
        assert_eq!(payload["kind"], "snapshot", "the overflow must have become a Resync");
        assert_eq!(
            call_in_snapshot(&mut set, "toolu_rp")["allowedByRule"],
            serde_json::json!("Bash(npm ci *)")
        );
        assert!(!set
            .get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_rp", NoteKind::Rule));
        shut_down_all(&mut set);
    }

    /// A resync after a rule answers the gate but before call completion must keep the rule
    /// candidate even if the projection still lists the unresolved request as pending. The
    /// `host_answered` filter excludes that request from the still-pending sweep, preserving the
    /// finished call's "allowed by rule" note. This complements the corresponding fast-path test
    /// and detects removal of that filter.
    #[test]
    fn a_resync_before_the_call_finishes_keeps_its_rule_answer() {
        let dir = workspace("tabs-resync-before-rule-completion");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        provider.queue(call_started(
            "toolu_rb",
            "Bash",
            serde_json::json!({ "command": "npm ci" }),
        ));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-rb".into(),
            tool_use_id: Some("toolu_rb".into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "npm ci" }),
            provider_prompt: None,
        });
        until("the rule to answer the call", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.host_answered.contains("perm-rb")
        });
        // One batch (`queue_all`'s doc), for the same reason every other prefix+filler sequence in
        // this file uses it, even though this particular `until` loops on `pump` itself below and so
        // has no premature-success race of its own.
        provider.queue_all(
            (0..(agent::UI_EVENT_QUEUE_CAPACITY + 20))
                .map(|i| text_delta("t1", &format!("chunk {i}")))
                .collect(),
        );
        let mut saw_snapshot = false;
        until("a resync", || {
            if let Some(p) = set.pump(&dir, true).active_payload {
                let v: serde_json::Value = serde_json::from_str(&p).unwrap();
                saw_snapshot |= v["kind"] == "snapshot";
            }
            saw_snapshot
        });
        assert!(
            set.get(tab)
                .unwrap()
                .session
                .note_ledger
                .has("toolu_rb", NoteKind::Rule),
            "the resync dropped a rule candidate whose call had not finished"
        );
        provider.queue(call_done("toolu_rb", false));
        pump_until_done(&mut set, &dir, "toolu_rb");
        assert_eq!(
            call_in_snapshot(&mut set, "toolu_rb")["allowedByRule"],
            serde_json::json!("Bash(npm ci *)")
        );
        shut_down_all(&mut set);
    }

    /// Turn traces must be observed for background tabs as well as the active tab.
    #[test]
    fn a_background_tabs_trace_is_observed_marked_and_emitted() {
        let dir = workspace("tabs-bg-trace");
        let mut set = set();
        let background = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(background).unwrap().backend = TabBackend::Live(backend);
        set.get_mut(background).unwrap().session.turn_trace = Some(crate::turn_trace::TurnTrace::started_now());
        set.open(AgentId::Claude);
        provider.queue(started("t1"));
        provider.queue(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: "hi".into(),
        });
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        until("the background turn to end", || {
            set.pump(&dir, true);
            !set.get(background).unwrap().turn_running()
                && set
                    .get(background)
                    .unwrap()
                    .session
                    .turn_trace
                    .as_ref()
                    .is_some_and(crate::turn_trace::TurnTrace::is_finished)
        });
        let trace = set.get(background).unwrap().session.turn_trace.as_ref().unwrap();
        assert!(trace.line().contains("first_paint_frame=bg"), "{}", trace.line());
        assert!(
            trace.is_emitted(),
            "the pump emits a background trace: nothing else will"
        );
        shut_down_all(&mut set);
    }

    /// Nothing previously pinned how `pump` feeds a drained batch into the turn trace. A turn that
    /// folds entirely before the very first tick sees it (the double-delivery race's shape) is delivered to the
    /// active tab wholly `in_snapshot`, via `active_state_payloads`, and never reaches the panel as
    /// `events` at all -- so no paint report is ever coming for it, and `observe_in_snapshot` (not
    /// plain `observe`) is what lets the trace complete anyway (`first_paint_frame=snapshot`).
    /// `emit`'s own gate is `!dispatched`, not `!is_active`: this tab IS active and never dispatches
    /// (the whole batch was already shown), so a gate keyed on `is_active` would never print it.
    #[test]
    fn a_turn_folded_entirely_inside_a_snapshot_still_completes_and_prints_its_trace() {
        let dir = workspace("tabs-trace-in-snapshot");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        set.get_mut(tab).unwrap().session.turn_trace = Some(crate::turn_trace::TurnTrace::started_now());

        // The whole turn folds with no tick draining it -- the ingestion thread folds on its own,
        // exactly as in the double-delivery races.
        provider.queue(started("t1"));
        provider.queue(text_delta("t1", "hi"));
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        wait_for_fold(&set, tab, "hi");

        // Reads the snapshot and notes its revision on the backend -- the watermark the next
        // pump's `covered` filter reads.
        let snapshot = the_snapshot(set.active_state_payloads());
        assert!(snapshot_text(&snapshot).contains("hi"), "{snapshot}");

        // The next pump drains the identical batch the snapshot already carried: `shown` covers
        // all of it, `fresh` is empty, nothing dispatches.
        let out = set.pump(&dir, true);
        assert!(
            out.active_payload.is_none(),
            "the whole batch was already in the snapshot"
        );

        let trace = set.get(tab).unwrap().session.turn_trace.as_ref().unwrap();
        assert!(
            trace.is_emitted(),
            "a turn folded entirely inside a snapshot must still be printed: nothing else will"
        );
        assert!(trace.line().contains("first_paint_frame=snapshot"), "{}", trace.line());
        shut_down_all(&mut set);
    }

    /// Every tab's own queue, draft and offers follow its snapshot on a switch, so the panel never
    /// shows the tab it left.
    #[test]
    fn a_switch_sends_the_tabs_own_queue_draft_and_offers() {
        let dir = workspace("tabs-switch-payloads");
        let mut set = set();
        let first = set.active();
        let (_provider, backend) = live(&dir);
        set.get_mut(first).unwrap().backend = TabBackend::Live(backend);
        set.queue_message(first, "queued", "queued".into(), 5, true, agent::PromptOrigin::Typed)
            .unwrap();
        set.set_draft(first, "typing");
        let second = set.open(AgentId::Claude);
        let kinds = |payloads: Vec<String>| -> Vec<(String, serde_json::Value)> {
            payloads
                .into_iter()
                .map(|p| {
                    let v: serde_json::Value = serde_json::from_str(&p).unwrap();
                    (v["kind"].as_str().unwrap().to_string(), v)
                })
                .collect()
        };
        let empty = kinds(set.active_state_payloads());
        assert_eq!(
            empty.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["queue", "draft", "scratch"],
            "an empty tab still resets the panel's queue and draft"
        );
        assert!(empty.iter().all(|(_, v)| v["tab"] == second.0));
        set.select(first);
        let live_payloads = kinds(set.active_state_payloads());
        assert_eq!(
            live_payloads.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["snapshot", "queue", "draft", "rule_offers", "scratch"]
        );
        assert_eq!(live_payloads[1].1["items"][0]["text"], "queued");
        assert_eq!(live_payloads[2].1["text"], "typing");
        shut_down_all(&mut set);
    }

    /// Sending the first message from an empty tab clears its mirrored draft, so state delivered
    /// after connection does not put the sent text back into the new conversation's composer.
    #[test]
    fn a_sent_prompt_is_no_longer_the_tabs_draft() {
        let mut set = set();
        let tab = set.active();
        set.set_draft(tab, "STREAM2 abc");
        set.note_sent(tab);
        let draft = set
            .active_state_payloads()
            .into_iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(&p).unwrap())
            .find(|v| v["kind"] == "draft")
            .expect("a draft payload");
        assert_eq!(draft["text"], "");
    }

    /// A draft edited in nvim returns to its originating tab even after a tab switch.
    #[test]
    fn a_scratch_edit_returns_to_its_own_tab() {
        let mut set = set();
        let first = set.active();
        set.set_draft(first, "before");
        set.begin_scratch_edit(first, 7).unwrap();
        assert!(set.begin_scratch_edit(first, 8).is_err(), "one edit per tab at a time");
        let second = set.open(AgentId::Claude);
        set.set_draft(second, "second's own");
        let (tab, draft) = set
            .finish_scratch_edit(7, &crate::scratch::EditDone::Written("after".into()))
            .expect("edit 7 belongs to a tab");
        assert_eq!((tab, draft.as_deref()), (first, Some("after")));
        assert_eq!(set.get(first).unwrap().session.draft, "after");
        assert_eq!(
            set.get(second).unwrap().session.draft,
            "second's own",
            "never the active tab"
        );
        assert_eq!(set.get(first).unwrap().editing_draft, None);
        assert!(
            set.finish_scratch_edit(7, &crate::scratch::EditDone::Discarded)
                .is_none(),
            "already finished"
        );

        set.begin_scratch_edit(second, 9).unwrap();
        let (_, draft) = set
            .finish_scratch_edit(9, &crate::scratch::EditDone::Discarded)
            .unwrap();
        assert_eq!(draft, None, ":q! changes nothing");
        assert_eq!(set.get(second).unwrap().session.draft, "second's own");
    }

    #[test]
    fn cancel_scratch_edits_takes_a_saved_body_and_keeps_the_draft_otherwise() {
        use crate::scratch::EditDone;
        let mut set = set();
        let changed = set.active();
        let identical = set.open(AgentId::Claude);
        let discarded = set.open(AgentId::Claude);
        let silent = set.open(AgentId::Claude);
        set.set_draft(changed, "old");
        set.set_draft(identical, "same\n");
        set.set_draft(discarded, "kept");
        set.set_draft(silent, "also kept");
        // Opened out of order: the answer is ordered by edit id.
        set.begin_scratch_edit(silent, 40).unwrap();
        set.begin_scratch_edit(changed, 10).unwrap();
        set.begin_scratch_edit(discarded, 30).unwrap();
        set.begin_scratch_edit(identical, 20).unwrap();
        let out = set.cancel_scratch_edits(&[
            (10, EditDone::Written("new".into())),
            // The shell strips one trailing newline off a body; the draft still has it.
            (20, EditDone::Written("same".into())),
            (30, EditDone::Discarded),
        ]);
        assert_eq!(
            out,
            vec![
                (changed, Some("new".to_string())),
                (identical, None),
                (discarded, None),
                (silent, None),
            ]
        );
        assert_eq!(set.get(changed).unwrap().session.draft, "new");
        assert_eq!(set.get(identical).unwrap().session.draft, "same\n");
        assert_eq!(set.get(discarded).unwrap().session.draft, "kept");
        assert_eq!(set.get(silent).unwrap().session.draft, "also kept");
        for tab in [changed, identical, discarded, silent] {
            assert_eq!(set.get(tab).unwrap().editing_draft, None);
        }
    }

    #[test]
    fn after_cancel_ctrl_g_works_again() {
        let mut set = set();
        let tab = set.active();
        set.begin_scratch_edit(tab, 7).unwrap();
        assert!(set.begin_scratch_edit(tab, 12).is_err());
        set.cancel_scratch_edits(&[]);
        assert!(set.begin_scratch_edit(tab, 12).is_ok());
    }

    // A CLI reporting an ungated mode closes the session.

    /// `UngatedCliMode` fails the tab with a reason naming the setting and mode, returns its backend
    /// for shutdown, and delivers none of that batch. A queued Write cannot become an approvable
    /// card; shutdown denies it instead.
    #[test]
    fn a_cli_reporting_an_ungated_mode_closes_the_session() {
        let dir = workspace("tabs-ungated-cli-mode");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        provider.queue(AgentDomainEvent::UngatedCliMode {
            reported: "acceptEdits".into(),
            detail: "SessionReady".into(),
        });
        provider.queue(write("behind-the-trip"));

        let mut tripped = Vec::new();
        let mut payloads = Vec::new();
        until("the trip", || {
            let out = set.pump(&dir, true);
            payloads.extend(out.active_payload);
            tripped.extend(out.tripped);
            !tripped.is_empty()
        });
        let state = set.get(tab).unwrap();
        let TabBackend::Failed { reason } = &state.backend else {
            panic!("the tab must be failed");
        };
        assert!(reason.contains("permissions.defaultMode"), "{reason}");
        assert!(reason.contains("'acceptEdits'"), "{reason}");
        assert_eq!(state.wire_state(), TabStateWire::Failed);
        assert_eq!(state.attention.attention().pending, 0, "no card is counted as waiting");

        // Handed out, and exactly once, for the caller to shut down -- which closes the session.
        assert_eq!(tripped.len(), 1);
        assert_eq!(tripped[0].tab, tab);
        assert_eq!(&tripped[0].reason, reason);
        assert_eq!(
            provider.closes(),
            0,
            "the pump shuts nothing down on the GTK thread itself"
        );
        for t in tripped {
            t.retired.into_backend().shutdown();
        }
        assert_eq!(provider.closes(), 1);

        // Nothing more arrives for it, and nothing that did named the Write.
        for _ in 0..5 {
            let out = set.pump(&dir, true);
            assert!(out.tripped.is_empty());
            payloads.extend(out.active_payload);
        }
        for payload in &payloads {
            assert!(
                !payload.contains("behind-the-trip"),
                "a card could have been drawn: {payload}"
            );
        }
        assert!(
            provider.resolutions().is_empty(),
            "nothing was answered on the host's side"
        );
        shut_down_all(&mut set);
    }

    /// A `PolicyMismatch` from another agent closes the tab the way the ungated mode does: the
    /// reason carries both sides, the backend is handed out for the shutdown, the batch it came in is
    /// withheld (the `Write` behind it never becomes a card) and nothing is answered on the host's
    /// side.
    #[test]
    fn policy_mismatch_withholds_the_batch_and_fails_the_tab() {
        let dir = workspace("tabs-policy-mismatch");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        provider.queue(AgentDomainEvent::PolicyMismatch {
            requested: "sandbox workspaceWrite".into(),
            reported: "sandbox dangerFullAccess".into(),
        });
        provider.queue(write("behind-the-mismatch"));

        let mut tripped = Vec::new();
        let mut payloads = Vec::new();
        until("the trip", || {
            let out = set.pump(&dir, true);
            payloads.extend(out.active_payload);
            tripped.extend(out.tripped);
            !tripped.is_empty()
        });
        let state = set.get(tab).unwrap();
        let TabBackend::Failed { reason } = &state.backend else {
            panic!("the tab must be failed");
        };
        assert!(reason.contains("sandbox dangerFullAccess"), "{reason}");
        assert!(reason.contains("sandbox workspaceWrite"), "{reason}");
        assert_eq!(state.wire_state(), TabStateWire::Failed);
        assert_eq!(state.attention.attention().pending, 0, "no card is counted as waiting");

        assert_eq!(tripped.len(), 1);
        assert_eq!(tripped[0].tab, tab);
        assert_eq!(&tripped[0].reason, reason);
        assert_eq!(
            provider.closes(),
            0,
            "the pump shuts nothing down on the GTK thread itself"
        );
        for t in tripped {
            t.retired.into_backend().shutdown();
        }
        assert_eq!(provider.closes(), 1);

        for _ in 0..5 {
            let out = set.pump(&dir, true);
            assert!(out.tripped.is_empty());
            payloads.extend(out.active_payload);
        }
        for payload in &payloads {
            assert!(
                !payload.contains("behind-the-mismatch"),
                "a card could have been drawn: {payload}"
            );
        }
        assert!(
            provider.resolutions().is_empty(),
            "nothing was answered on the host's side"
        );
        shut_down_all(&mut set);
    }

    /// What the page is sent for a tab a tripwire failed, and for one whose start never happened:
    /// the `error` envelope with the reason, and a `tabs` envelope that says whether the session
    /// had been open (`failedAfterStart`), so the page can word the two differently and keep the
    /// conversation of the first. `agent-ui/web/src/fixtures/failed-tabs.json` records both for the
    /// page's tests. Compared on every run; `EITRI_WRITE_FIXTURES=1` rewrites it.
    #[test]
    fn a_failed_tab_says_whether_its_session_had_been_open_and_the_fixture_records_it() {
        let dir = workspace("tabs-failed-after-start");
        let mut set = set();
        let tripped_tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tripped_tab).unwrap().backend = TabBackend::Live(backend);
        let never_started_tab = set.open(AgentId::Claude);
        provider.queue(AgentDomainEvent::PolicyMismatch {
            requested: "sandbox workspaceWrite".into(),
            reported: "sandbox dangerFullAccess".into(),
        });
        let mut tripped = Vec::new();
        until("the trip", || {
            tripped.extend(set.pump(&dir, true).tripped);
            !tripped.is_empty()
        });
        let reason = tripped[0].reason.clone();
        for t in tripped {
            t.retired.into_backend().shutdown();
        }
        {
            let tab = set.get_mut(never_started_tab).unwrap();
            tab.failed_after_start = false;
            tab.backend = TabBackend::Failed {
                reason: "failed to start session: codex not found".into(),
            };
        }

        let tabs_of = |active: TabId, set: &TabSet| -> serde_json::Value {
            let mut value: serde_json::Value = serde_json::from_str(&set.tabs_payload()).unwrap();
            value["active"] = serde_json::json!(active.0);
            value
        };
        let tripped_tabs = tabs_of(tripped_tab, &set);
        let by_id = |id: TabId| {
            tripped_tabs["tabs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == id.0)
                .unwrap()
                .clone()
        };
        assert_eq!(by_id(tripped_tab)["state"], "failed");
        assert_eq!(by_id(tripped_tab)["failure"], reason.as_str());
        assert_eq!(by_id(tripped_tab)["failedAfterStart"], true);
        assert_eq!(by_id(never_started_tab)["state"], "failed");
        assert_eq!(by_id(never_started_tab)["failedAfterStart"], false);

        let fixture = serde_json::json!({
            "writtenBy": "core/src/tab_set.rs, tests::a_failed_tab_says_whether_its_session_had_been_open_and_the_fixture_records_it \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "trippedTab": tripped_tab.0,
            "neverStartedTab": never_started_tab.0,
            "error": serde_json::from_str::<serde_json::Value>(&crate::agent_bridge::serialize_error_for_js(tripped_tab, &reason)).unwrap(),
            "tabs": tripped_tabs,
        });
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/failed-tabs.json");
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&path).ok();
        if committed.as_deref() != Some(written.as_str()) {
            if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
                std::fs::write(&path, &written).expect("write the failed tabs fixture");
            } else {
                assert_eq!(
                    committed.as_deref(),
                    Some(written.as_str()),
                    "the committed failed tabs fixture is not what this test produces; if the change is \
                     intended, regenerate it with EITRI_WRITE_FIXTURES=1"
                );
            }
        }
        shut_down_all(&mut set);
    }

    /// The report survives a `Resync`: a UI queue that overflowed drops its events, the tripwire
    /// among them, and the projection's record is what still closes the session.
    #[test]
    fn a_trip_lost_to_a_resync_still_closes_the_session() {
        let dir = workspace("tabs-ungated-cli-mode-resync");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        provider.queue(AgentDomainEvent::UngatedCliMode {
            reported: "bypassPermissions".into(),
            detail: "PermissionModeChanged".into(),
        });
        // More than the UI queue holds, so the first delivery is a `Resync`.
        for _ in 0..(agent::UI_EVENT_QUEUE_CAPACITY + 20) {
            provider.open_session("claude-1", &dir);
        }
        until("an overflowed UI queue", || {
            let AgentBackend::Conversation { conversation, .. } = &backend else {
                unreachable!("`live` builds a sidecar")
            };
            let stats = conversation.ingest_stats();
            stats.resyncs >= 1 && stats.events_ingested as usize > agent::UI_EVENT_QUEUE_CAPACITY + 20
        });
        assert!(
            backend.projection().ungated_cli_mode.is_some(),
            "the report was folded, and only the projection still has it"
        );
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        let out = set.pump(&dir, true);
        assert_eq!(out.tripped.len(), 1, "the first pump already closes it");
        assert!(matches!(set.get(tab).unwrap().backend, TabBackend::Failed { .. }));
        for t in out.tripped {
            t.retired.into_backend().shutdown();
        }
        shut_down_all(&mut set);
    }

    /// `Tab::mode` is the only authority: a `PermissionModeChanged` reaching the pump, which only
    /// a report naming `default` can do, must not change it.
    #[test]
    fn a_reported_mode_change_moves_nothing() {
        let dir = workspace("tabs-mode-report-moves-nothing");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;

        provider.queue(AgentDomainEvent::PermissionModeChanged {
            mode: agent::PermissionMode::Auto,
            provider_mode: "default".into(),
            floor_applied: false,
        });
        provider.queue(AgentDomainEvent::SessionClosed { reason: "x".into() });
        until("the session's end", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().wire_state() == TabStateWire::Ended
        });
        assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Bypass);
        shut_down_all(&mut set);
    }

    // ---- The CLI's own permission prompts through the tab set -------------------------------------

    fn gate_write(permission_id: &str, tool_use_id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: permission_id.into(),
            tool_use_id: Some(tool_use_id.into()),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/probe2", "content": "o3" }),
            provider_prompt: None,
        }
    }

    /// The CLI's own prompt for a call, as the sidecar translation produces it.
    fn cli_prompt(
        permission_id: &str,
        tool_use_id: &str,
        tool_name: &str,
        input: serde_json::Value,
        ask_rule: Option<&str>,
    ) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: permission_id.into(),
            tool_use_id: Some(tool_use_id.into()),
            tool_name: tool_name.into(),
            input,
            provider_prompt: Some(agent::ProviderPrompt {
                reason: Some("Claude requested permissions to edit .git/probe2 which is a sensitive file.".into()),
                description: Some(".git/probe2".into()),
                blocked_path: None,
                matched_ask_rule: ask_rule.map(|content| agent::MatchedAskRule {
                    source: "projectSettings".into(),
                    tool_name: tool_name.into(),
                    rule_content: Some(content.into()),
                }),
                unrecognized_origin: None,
            }),
        }
    }

    fn resolved(permission_id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionResolved {
            permission_id: permission_id.into(),
            outcome: agent::PermissionOutcome::Allowed,
        }
    }

    /// The gate card, human approval through `answer_card`, and gate resolution can be followed by
    /// the CLI's prompt for the same call under a new permission id. That request is accepted and
    /// answered `allow` using the prior human approval, producing exactly one card for the call.
    #[test]
    fn approving_the_card_for_a_call_answers_the_clis_own_prompt_that_follows() {
        let dir = workspace("o3-auto-one-card");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        provider.queue(gate_write("perm-hook", "toolu_probe2"));
        until("the gate's card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        set.answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        assert_eq!(provider.resolutions(), vec![("perm-hook".to_string(), true)]);

        provider.queue(resolved("perm-hook"));
        provider.queue(cli_prompt(
            "perm-cli",
            "toolu_probe2",
            "Write",
            serde_json::json!({ "file_path": ".git/probe2", "content": "o3" }),
            None,
        ));
        until("the CLI's own prompt answered", || {
            if let Some(payload) = set.pump(&dir, true).active_payload {
                assert!(!payload.contains("perm-cli"), "never drawn: {payload}");
            }
            provider.resolutions().len() == 2
        });
        assert_eq!(provider.resolutions()[1], ("perm-cli".to_string(), true));
        let attention = set.get(tab).unwrap().attention.attention();
        assert_eq!(attention.pending, 0);
        assert_eq!(attention.arrived, 1, "exactly one card for the call");
        shut_down_all(&mut set);
    }

    /// Only an Approve counts: a denied card, and a card answered behind the tab set's back (not
    /// through `answer_card`), leave the CLI's own prompt for that call a card.
    #[test]
    fn only_an_approve_through_the_tab_set_answers_the_clis_own_prompt() {
        let dir = workspace("o3-auto-deny");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        provider.queue(gate_write("perm-denied", "toolu_a"));
        provider.queue(gate_write("perm-direct", "toolu_b"));
        until("both gate cards", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 2
        });
        set.answer_card(
            tab,
            ViewId::BASE,
            "perm-denied",
            agent::PermissionDecision::Deny {
                reason: Some("no".into()),
            },
        )
        .map_err(|e| e.message)
        .unwrap();
        set.get_mut(tab)
            .unwrap()
            .live_mut()
            .unwrap()
            .respond_permission("perm-direct", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        assert!(set.get(tab).unwrap().session.human_allowed.is_empty());

        provider.queue(resolved("perm-denied"));
        provider.queue(resolved("perm-direct"));
        let input = serde_json::json!({ "file_path": ".git/probe2", "content": "o3" });
        provider.queue(cli_prompt("perm-cli-a", "toolu_a", "Write", input.clone(), None));
        provider.queue(cli_prompt("perm-cli-b", "toolu_b", "Write", input, None));
        until("both CLI prompts card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 2
        });
        assert_eq!(provider.resolutions().len(), 2, "{:?}", provider.resolutions());
        shut_down_all(&mut set);
    }

    /// The set of approved calls lives for the turn: the CLI asks within the turn that made the call,
    /// so it is emptied when the turn ends rather than growing for the life of the tab.
    #[test]
    fn approved_calls_are_forgotten_when_the_turn_ends() {
        let dir = workspace("o3-auto-turn-end");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        provider.queue(gate_write("perm-hook", "toolu_c"));
        until("the gate's card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        set.answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        assert!(set.get(tab).unwrap().session.human_allowed.contains("toolu_c"));
        provider.queue(resolved("perm-hook"));
        provider.queue(AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        until("the turn ends", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.human_allowed.is_empty()
        });
        shut_down_all(&mut set);
    }

    /// A completion that came with no turn of the backend's own open (Claude Code ran a turn of its own)
    /// ends none of the panel's: the calls approved in the running turn stay approved, so the CLI's own
    /// prompt for one of them still needs no second card.
    #[test]
    fn approved_calls_outlive_a_completion_of_no_open_turn() {
        let dir = workspace("o3-auto-foreign-end");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        provider.queue(gate_write("perm-hook", "toolu_c"));
        until("the gate's card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        set.answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        provider.queue(resolved("perm-hook"));
        provider.queue(AgentDomainEvent::TurnCompleted {
            turn_id: agent::NO_OPEN_TURN_ID.into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        // A later card shows the completion before it was pumped.
        provider.queue(gate_write("perm-later", "toolu_d"));
        until("the later card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.card_ids() == ["perm-later"]
        });
        let t = set.get(tab).unwrap();
        assert!(t.turn_running(), "the panel's turn runs on");
        assert!(t.session.human_allowed.contains("toolu_c"), "and so does its approval");
        shut_down_all(&mut set);
    }

    /// In bypass the CLI's prompt is answered without being drawn as a card.
    #[test]
    fn in_bypass_the_clis_own_prompt_is_answered_and_never_delivered() {
        let dir = workspace("o3-bypass");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 0 })
        );
        provider.queue(cli_prompt(
            "perm-cli",
            "toolu_d",
            "Write",
            serde_json::json!({ "file_path": ".claude/probe.json", "content": "{}" }),
            None,
        ));
        until("answered", || {
            if let Some(payload) = set.pump(&dir, true).active_payload {
                assert!(!payload.contains("perm-cli"), "{payload}");
            }
            !provider.resolutions().is_empty()
        });
        assert_eq!(provider.resolutions(), vec![("perm-cli".to_string(), true)]);
        assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
        shut_down_all(&mut set);
    }

    /// A waiting card is not copied by a tick that finds it unchanged: the offers are worked out again
    /// only when the set of cards changed. A 1 MiB card used to be copied on every tick (30 a second).
    #[test]
    fn an_unchanged_card_is_not_copied_by_a_tick() {
        let dir = workspace("offers-no-copy");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        // A pipe makes it a card whatever the command, and the card is the size of a large edit.
        let command = format!("echo {} | wc -c", "x".repeat(1 << 20));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-big".into(),
            tool_use_id: None,
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": command }),
            provider_prompt: None,
        });
        until("the card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        set.pump(&dir, true);
        let (_, bytes) = crate::alloc_count::bytes_allocated_by(|| set.pump(&dir, true));
        assert!(bytes < 64 * 1024, "one quiet tick allocated {bytes} bytes");
        shut_down_all(&mut set);
    }

    /// A saved rule cannot silence the CLI's prompt, so that card never offers "Always allow".
    /// The gate card for the same command still offers a rule.
    #[test]
    fn the_clis_own_prompt_is_never_offered_a_rule() {
        let dir = workspace("o3-no-offer");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let npm = serde_json::json!({ "command": "npm ci" });
        provider.queue(cli_prompt("perm-cli", "toolu_e", "Bash", npm.clone(), None));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-gate".into(),
            tool_use_id: Some("toolu_f".into()),
            tool_name: "Bash".into(),
            input: npm,
            provider_prompt: None,
        });
        until("both cards", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 2
        });
        let offers = &set.get(tab).unwrap().session.rule_offers;
        assert!(offers.contains_key("perm-gate"), "{offers:?}");
        assert!(!offers.contains_key("perm-cli"), "{offers:?}");
        shut_down_all(&mut set);
    }

    /// Entering bypass counts and approves every waiting card: the gate's request and CLI prompts
    /// forced by an ask rule, of unknown kind, without a reason, describing only a subject or path,
    /// or carrying a reason. They are listed in `seq` order and approved by `y`.
    /// The prompt needs only one line because no card remains waiting. Completed calls whose CLI
    /// prompt the entry approved carry the same bypass note as calls answered in a bypass tab.
    #[test]
    fn entering_bypass_approves_every_waiting_card_the_clis_own_prompts_included() {
        let dir = workspace("bypass-entry-every-prompt");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let changed = |id: &str, tool_use_id: &str, change: &dyn Fn(&mut agent::ProviderPrompt)| {
            let mut event = cli_prompt(
                id,
                tool_use_id,
                "Bash",
                serde_json::json!({ "command": format!("echo {id}") }),
                None,
            );
            if let AgentDomainEvent::PermissionRequested { provider_prompt, .. } = &mut event {
                change(provider_prompt.as_mut().unwrap());
            }
            event
        };
        provider.queue(started("t1"));
        provider.queue(call_started(
            "toolu_g",
            "Bash",
            serde_json::json!({ "command": "cat notes.txt" }),
        ));
        provider.queue(cli_prompt(
            "perm-ask",
            "toolu_g",
            "Bash",
            serde_json::json!({ "command": "cat notes.txt" }),
            Some("cat:*"),
        ));
        provider.queue(changed("perm-unknown", "toolu_u", &|p| p.unrecognized_origin = Some(7)));
        provider.queue(changed("perm-silent", "toolu_s", &|p| {
            p.reason = None;
            p.description = Some("Print the word zebra".into());
        }));
        provider.queue(changed("perm-path", "toolu_p", &|p| {
            p.reason = None;
            p.blocked_path = Some("/p/.git/x".into());
        }));
        provider.queue(changed("perm-reason", "toolu_r", &|_| {}));
        provider.queue(write("perm-write"));
        let all: Vec<String> = [
            "perm-ask",
            "perm-unknown",
            "perm-silent",
            "perm-path",
            "perm-reason",
            "perm-write",
        ]
        .map(str::to_string)
        .to_vec();
        until("six cards", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 6
        });
        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(plan.approve, all);
        assert_eq!(
            plan.lines,
            vec!["Switch to bypass and approve the 6 waiting cards? (y/n)".to_string()]
        );
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 6 })
        );
        assert_eq!(
            provider.resolutions(),
            all.iter().map(|id| (id.clone(), true)).collect::<Vec<_>>()
        );
        assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
        assert!(all
            .iter()
            .all(|id| set.get(tab).unwrap().session.host_answered.contains(id)));
        provider.queue(call_done("toolu_g", false));
        pump_until_done(&mut set, &dir, "toolu_g");
        assert_eq!(
            call_in_snapshot(&mut set, "toolu_g")["promptNote"],
            "your ask rule: Bash(cat:*) — allowed in bypass"
        );

        // In the bypass tab itself such a prompt is answered at once and never drawn.
        provider.queue(changed("perm-silent-2", "toolu_s2", &|p| p.reason = None));
        until("the second silent prompt answered", || {
            if let Some(payload) = set.pump(&dir, true).active_payload {
                assert!(!payload.contains("perm-silent-2"), "never drawn: {payload}");
            }
            provider.resolutions().iter().any(|(id, _)| id == "perm-silent-2")
        });
        assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
        shut_down_all(&mut set);
    }

    /// A card arriving while the bypass prompt is open, even one forced by an ask rule, cannot be
    /// approved by a `y` to a question that omitted it. Confirmation asks again with the new count;
    /// only the second `y` approves it.
    #[test]
    fn a_cli_prompt_arriving_under_an_open_bypass_prompt_asks_again() {
        let dir = workspace("bypass-entry-late-ask");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(write("perm-write"));
        until("the first card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        let first = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(first.approve, vec!["perm-write".to_string()]);
        provider.queue(cli_prompt(
            "perm-ask",
            "toolu_late",
            "Bash",
            serde_json::json!({ "command": "cat notes.txt" }),
            Some("cat:*"),
        ));
        until("the late card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 2
        });
        let again = match set.confirm_bypass(first.scope, first.nonce, ViewId::BASE) {
            Ok(ConfirmOutcome::Reprompt(again)) => again,
            other => panic!("a card the prompt did not count asks again, got {other:?}"),
        };
        assert!(provider.resolutions().is_empty(), "{:?}", provider.resolutions());
        assert_eq!(again.approve, vec!["perm-write".to_string(), "perm-ask".to_string()]);
        assert_eq!(
            set.confirm_bypass(again.scope, again.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 2 })
        );
        assert_eq!(
            provider.resolutions(),
            vec![("perm-write".to_string(), true), ("perm-ask".to_string(), true)]
        );
        shut_down_all(&mut set);
    }

    /// `answer_card` names what it could not answer the way the panel's own path always did.
    #[test]
    fn answering_a_card_on_a_tab_with_no_session_is_refused() {
        let mut set = set();
        let tab = set.active();
        let error = set
            .answer_card(tab, ViewId::BASE, "perm-x", agent::PermissionDecision::Allow)
            .expect_err("no session");
        assert_eq!(error.message, "no active session");
        assert!(error.benign);
        assert!(set
            .answer_card(TabId(999), ViewId::BASE, "perm-x", agent::PermissionDecision::Allow)
            .is_err());
    }

    /// A `Resync` can swallow a turn boundary (T1's `TurnCompleted` and T2's
    /// `TurnStarted` both dropped with the overflow), so the turn-end clear never runs. Every Resync
    /// forgets the tab's approvals, so a T2 prompt reusing T1's id -- same tool, same input -- is a
    /// card, not answered on T1's approval.
    #[test]
    fn a_resync_forgets_every_approval_even_across_a_turn_it_swallowed() {
        let dir = workspace("o3-resync-forgets");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        provider.queue(gate_write("perm-hook", "toolu_reused"));
        until("the gate's card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        set.answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        assert!(set.get(tab).unwrap().session.human_allowed.contains("toolu_reused"));

        provider.queue(resolved("perm-hook"));
        provider.queue(AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
        for i in 0..300 {
            provider.queue(AgentDomainEvent::ContentDelta {
                turn_id: "t2".into(),
                kind: agent::ContentKind::Text,
                text: format!("chunk {i}"),
            });
        }
        until("t2 folded", || {
            let backend = set.get(tab).unwrap().live().unwrap();
            let projection = backend.projection();
            projection.active_turn_id.as_deref() == Some("t2")
                && projection
                    .transcript
                    .last()
                    .is_some_and(|m| m.text.ends_with("chunk 299"))
        });
        let out = set.pump(&dir, true);
        let payload: serde_json::Value = serde_json::from_str(&out.active_payload.expect("a payload")).unwrap();
        assert_eq!(payload["kind"], "snapshot", "the overflow must have become a Resync");
        assert!(set.get(tab).unwrap().session.human_allowed.is_empty());

        provider.queue(cli_prompt(
            "perm-cli-t2",
            "toolu_reused",
            "Write",
            serde_json::json!({ "file_path": ".git/probe2", "content": "o3" }),
            None,
        ));
        until("the T2 prompt cards", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        assert_eq!(provider.resolutions(), vec![("perm-hook".to_string(), true)]);
        shut_down_all(&mut set);
    }

    /// `r` forgets the approvals at once, before any pump.
    #[test]
    fn reset_forgets_the_approvals() {
        let mut set = set();
        let tab = set.active();
        let t = set.get_mut(tab).unwrap();
        t.backend = TabBackend::Failed { reason: "gone".into() };
        t.session
            .human_allowed
            .record("toolu_x", "Write", &serde_json::json!({}));
        set.reset(tab).unwrap();
        assert!(set.get(tab).unwrap().session.human_allowed.is_empty());
    }

    /// A tab with no backend holds no approvals: the pump's non-live arm forgets them.
    #[test]
    fn a_tab_without_a_backend_holds_no_approvals() {
        let dir = workspace("o3-no-backend");
        let mut set = set();
        let tab = set.active();
        set.get_mut(tab)
            .unwrap()
            .session
            .human_allowed
            .record("toolu_x", "Write", &serde_json::json!({}));
        set.pump(&dir, true);
        assert!(set.get(tab).unwrap().session.human_allowed.is_empty());
    }

    /// An Approve the provider refused approved nothing, so nothing is recorded.
    #[test]
    fn a_refused_approve_records_no_approval() {
        let dir = workspace("o3-refused-approve");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(gate_write("perm-hook", "toolu_r"));
        until("the gate's card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        provider.refuse_resolutions(true);
        assert!(set
            .answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
            .is_err());
        assert!(set.get(tab).unwrap().session.human_allowed.is_empty());
        shut_down_all(&mut set);
    }

    /// A call whose CLI prompt was answered without a card says
    /// so, muted, on its row -- in the events payload when the call completes, and in every later
    /// snapshot -- as a call a saved rule answered does.
    #[test]
    fn a_call_answered_without_a_card_says_so_on_its_row() {
        let dir = workspace("o3-row-note");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 0 })
        );
        let input = serde_json::json!({ "file_path": ".git/probe2", "content": "o3" });
        provider.queue(AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_n".into(),
            name: "Write".into(),
            input: input.clone(),
        });
        provider.queue(cli_prompt("perm-cli", "toolu_n", "Write", input, None));
        until("answered", || {
            set.pump(&dir, true);
            !provider.resolutions().is_empty()
        });
        provider.queue(resolved("perm-cli"));
        provider.queue(AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_n".into(),
            content: serde_json::json!("File created successfully"),
            is_error: false,
        });
        let mut notes = serde_json::Value::Null;
        until("the note", || {
            if let Some(payload) = set.pump(&dir, true).active_payload {
                let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
                if value.get("promptNotes").is_some() {
                    notes = value["promptNotes"].clone();
                }
            }
            !notes.is_null()
        });
        assert_eq!(
            notes,
            serde_json::json!([{ "toolUseId": "toolu_n", "note": "Claude Code asked — allowed in bypass" }])
        );
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        assert_eq!(
            snapshot["state"]["toolCalls"][0]["promptNote"],
            "Claude Code asked — allowed in bypass"
        );
        shut_down_all(&mut set);
    }

    // ---- saving the tabs, and bringing them back --------------------------------------------

    /// A tab holding a live session whose Claude id is `session` (waits for the id to be adopted).
    fn give_session(set: &mut TabSet, tab: TabId, dir: &Path, session: &str) -> Arc<RecordingProvider> {
        let (provider, backend) = live(dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.open_session(session, dir);
        until("the session id", || {
            set.get(tab).unwrap().provider_session_id().as_deref() == Some(session)
        });
        provider
    }

    const PROJECT: &str = "conv-0123456789abcdef";

    fn refused(message: &str) -> BackendError {
        BackendError {
            message: message.to_string(),
            benign: false,
        }
    }

    fn ids(saved: &SavedTabs) -> Vec<&str> {
        saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect()
    }

    /// After `/clear` the tab is saved under the new conversation: in the moment before its session
    /// opens (the session holds no id then) and after, so a restore resumes what the tab shows.
    #[test]
    fn a_cleared_tab_is_saved_under_the_new_conversation() {
        let dir = workspace("clear-saved-id");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let opened = |id: &str| AgentDomainEvent::SessionOpened {
            session_id: "verdandi-1".into(),
            provider_session_id: id.into(),
            model: "m".into(),
            cwd: dir.to_string_lossy().into_owned(),
        };
        let saved = |set: &TabSet| {
            ids(&set.saved_snapshot(PROJECT).unwrap())
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        provider.queue(opened("sess-old"));
        until("the old session", || {
            set.pump(&dir, true);
            saved(&set) == ["sess-old"]
        });
        provider.queue_all(vec![
            AgentDomainEvent::TurnStarted { turn_id: "t2".into() },
            AgentDomainEvent::ConversationReset {
                new_conversation_id: "sess-new".into(),
                trigger: Some("clear".into()),
            },
        ]);
        until("the reset", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().provider_session_id().is_none()
        });
        assert_eq!(
            saved(&set),
            ["sess-new"],
            "saved under the new conversation before it opens"
        );
        provider.queue(opened("sess-new"));
        until("the new session", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().provider_session_id().as_deref() == Some("sess-new")
        });
        assert_eq!(saved(&set), ["sess-new"]);
        shut_down_all(&mut set);
    }

    #[test]
    fn only_tabs_with_a_session_are_listed_in_tab_bar_order_with_their_name_and_mode() {
        let dir = workspace("tabs-snapshot");
        let mut set = set();
        assert_eq!(
            set.saved_snapshot(PROJECT),
            Some(SavedTabs::default()),
            "a fresh window has nothing to save"
        );
        let first = set.active();
        give_session(&mut set, first, &dir, "s-one");
        set.rename(first, "api");
        let empty = set.open(AgentId::Claude);
        let third = set.open(AgentId::Claude);
        give_session(&mut set, third, &dir, "s-three");
        set.get_mut(third).unwrap().mode = SessionModeChoice::Bypass;

        let saved = set.saved_snapshot(PROJECT).unwrap();
        assert_eq!(ids(&saved), ["s-one", "s-three"], "the empty tab is not listed");
        assert_eq!(saved.tabs[0].name.as_deref(), Some("api"));
        assert_eq!(saved.tabs[0].mode, SessionModeChoice::Auto);
        assert_eq!(saved.tabs[1].mode, SessionModeChoice::Bypass);
        assert!(saved.tabs.iter().all(|t| t.conversation_id == PROJECT));
        assert_eq!(saved.active, Some(1), "the tab on screen, among the listed ones");

        set.select(empty);
        assert_eq!(
            set.saved_snapshot(PROJECT).unwrap().active,
            Some(1),
            "an empty tab on screen falls back to the last listed one"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn the_saved_active_tab_falls_back_to_the_listed_tab_that_was_active_most_recently() {
        let dir = workspace("tabs-snapshot-recent");
        let mut set = set();
        let a = set.active();
        give_session(&mut set, a, &dir, "s-a");
        let b = set.open(AgentId::Claude);
        give_session(&mut set, b, &dir, "s-b");
        let c = set.open(AgentId::Claude); // empty
        let d = set.open(AgentId::Claude); // empty
                                           // a, b, c, d in that order; now b, then d: the empty d is on screen.
        set.select(b);
        set.select(d);
        assert_eq!(
            set.saved_snapshot(PROJECT).unwrap().active,
            Some(1),
            "b was the last listed tab on screen"
        );
        set.select(a);
        set.select(c);
        assert_eq!(set.saved_snapshot(PROJECT).unwrap().active, Some(0), "now it was a");
        // Closing the tab it fell back to hands the fallback to the one before it.
        set.remove(a);
        assert_eq!(set.saved_snapshot(PROJECT).unwrap().active, Some(0));
        assert_eq!(ids(&set.saved_snapshot(PROJECT).unwrap()), ["s-b"]);
        shut_down_all(&mut set);
    }

    #[test]
    fn a_tab_still_resuming_is_listed_and_a_failed_one_is_not() {
        let mut set = set();
        let resuming = set.active();
        let (_tx, result_rx) = mpsc::channel();
        set.get_mut(resuming).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "r".into(),
            result_rx,
            first_turn: None,
            resume: Some("s-coming".into()),
            resumed_title: None,
            resumed_name: None,
        });
        let failed = set.open(AgentId::Claude);
        set.get_mut(failed).unwrap().backend = TabBackend::Failed { reason: "no".into() };
        assert_eq!(ids(&set.saved_snapshot(PROJECT).unwrap()), ["s-coming"]);
    }

    #[test]
    fn the_legacy_backend_resumes_nothing_so_it_lists_nothing() {
        let dir = workspace("tabs-snapshot-legacy");
        let mut legacy = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
        let tab = legacy.active();
        give_session(&mut legacy, tab, &dir, "s-legacy");
        assert_eq!(legacy.saved_snapshot(PROJECT), Some(SavedTabs::default()));
        shut_down_all(&mut legacy);
    }

    /// Closing the window takes every tab away to tear it down; that is not the user closing them.
    #[test]
    fn a_window_that_has_taken_its_tabs_to_close_them_has_no_snapshot() {
        let dir = workspace("tabs-snapshot-closing");
        let mut set = set();
        let tab = set.active();
        give_session(&mut set, tab, &dir, "s-one");
        assert!(set.saved_snapshot(PROJECT).is_some());
        let taken = set.take_all();
        assert_eq!(set.saved_snapshot(PROJECT), None);
        for tab in taken {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn a_pristine_window_has_no_tab_with_a_session_in_any_state() {
        let mut set = set();
        assert!(set.is_pristine());
        set.open(AgentId::Claude);
        assert!(set.is_pristine(), "empty tabs do not count");
        let tab = set.active();
        set.get_mut(tab).unwrap().backend = TabBackend::Failed { reason: "x".into() };
        assert!(!set.is_pristine(), "a start that was attempted does");
        let (_tx, result_rx) = mpsc::channel();
        set.get_mut(tab).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "r".into(),
            result_rx,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        });
        assert!(!set.is_pristine());
    }

    #[test]
    fn the_configured_default_mode_is_the_one_way_a_window_starts_in_bypass() {
        let mut set = set();
        set.apply_configured_default(SessionModeChoice::Bypass);
        assert_eq!(set.default_mode(), SessionModeChoice::Bypass);
        assert_eq!(
            set.active_tab().mode(),
            SessionModeChoice::Bypass,
            "the empty tab it already has"
        );
        let next = set.open(AgentId::Claude);
        assert_eq!(
            set.get(next).unwrap().mode(),
            SessionModeChoice::Bypass,
            "and every new one"
        );
        // Leaving bypass is as it always was: at once, and an empty tab's default goes with it (to
        // manual, where leaving bypass lands).
        let tab = set.active();
        assert_eq!(
            set.cycle_mode(tab, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Manual)),
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Manual);
    }

    // ---- Manual mode: the three-way cycle ------------------------------------------------------

    /// A Claude tab cycles manual -> auto -> bypass -> manual; only the move into bypass asks, and it
    /// is offered only from auto. Leaving bypass lands on manual.
    #[test]
    fn a_claude_tab_cycles_manual_auto_bypass_and_back_to_manual() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
        let tab = set.active();
        assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Manual);
        assert_eq!(
            set.cycle_mode(tab, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Auto))
        );
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Auto,
            "an empty tab carries the default"
        );
        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        assert_eq!(
            set.get(tab).unwrap().mode(),
            SessionModeChoice::Auto,
            "nothing moves before y"
        );
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
        assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Bypass);
        assert_eq!(
            set.cycle_mode(tab, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Manual))
        );
        assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Manual);
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Manual,
            "the empty tab's default went with it"
        );
        shut_down_all(&mut set);
    }

    /// From manual the key never asks and never reaches bypass: bypass is entered only from auto, and
    /// a confirm left over from a plan made in auto is no yes once the tab is in manual.
    #[test]
    fn bypass_is_entered_only_from_auto() {
        let dir = workspace("manual-bypass-only-from-auto");
        let mut set = set();
        let tab = set.active();
        let (_provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        set.get_mut(tab).unwrap().mode = SessionModeChoice::Manual;
        assert_eq!(
            set.cycle_mode(tab, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Auto))
        );
        let plan = plan(set.cycle_mode(tab, ViewId::BASE));
        // Something other than the key moved the tab under the open prompt.
        set.get_mut(tab).unwrap().mode = SessionModeChoice::Manual;
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Err("that prompt is no longer current".to_string())
        );
        assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Manual);
        shut_down_all(&mut set);
    }

    /// An ACP agent's own mode is set once at its start, so its tab offers no manual: the key goes
    /// auto <-> bypass, leaving bypass lands on auto.
    #[test]
    fn an_acp_tab_skips_manual() {
        let mut set = set();
        let acp = set.open(AgentId::Acp("gem".into()));
        assert_eq!(set.get(acp).unwrap().mode(), SessionModeChoice::Auto);
        let plan = plan(set.cycle_mode(acp, ViewId::BASE));
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
        assert_eq!(
            set.cycle_mode(acp, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Auto))
        );
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Manual,
            "leaving bypass leaves the window default in manual; the tab's auto is only its agent's stand-in"
        );
        assert!(
            matches!(set.cycle_mode(acp, ViewId::BASE), Ok(ModeCycle::Confirm(_))),
            "auto -> bypass asks"
        );
        shut_down_all(&mut set);
    }

    /// A new ACP tab under a manual window default starts in auto, and the page is told once; a Claude
    /// or Codex tab takes manual and says nothing.
    #[test]
    fn a_new_acp_tab_under_a_manual_default_starts_in_auto_and_says_so_once() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
        assert_eq!(set.take_mode_note(), None);
        let codex = set.open(AgentId::Codex);
        assert_eq!(set.get(codex).unwrap().mode(), SessionModeChoice::Manual);
        assert_eq!(set.take_mode_note(), None);
        let acp = set.open(AgentId::Acp("gem".into()));
        assert_eq!(set.get(acp).unwrap().mode(), SessionModeChoice::Auto);
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Manual,
            "the window default is unchanged"
        );
        let note = set.take_mode_note().expect("the page is told");
        assert!(note.contains("gem tabs have no manual mode"), "{note}");
        let number = set.get(acp).unwrap().number;
        assert!(note.ends_with(&format!("tab {number} is in auto")), "{note}");
        assert_eq!(set.take_mode_note(), None, "once");
    }

    /// The configured default and the configured agent reach the empty tab the window already has in
    /// a mode its agent offers, saying so when that is not the one asked for.
    #[test]
    fn the_configured_default_and_agent_give_an_acp_tab_auto_for_manual() {
        let mut set = set();
        set.apply_configured_agent(AgentId::Acp("gem".into()));
        set.apply_configured_default(SessionModeChoice::Manual);
        assert_eq!(set.default_mode(), SessionModeChoice::Manual);
        assert_eq!(set.active_tab().mode(), SessionModeChoice::Auto);
        assert!(set.take_mode_note().is_some());

        let mut other = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        other.apply_configured_default(SessionModeChoice::Manual);
        assert_eq!(other.active_tab().mode(), SessionModeChoice::Manual);
        assert_eq!(other.take_mode_note(), None);
        other.apply_configured_agent(AgentId::Acp("gem".into()));
        assert_eq!(other.active_tab().mode(), SessionModeChoice::Auto);
        assert!(other.take_mode_note().is_some());
    }

    /// The window default cycles in the same order; a bypass default is left for manual.
    #[test]
    fn the_window_default_cycles_manual_auto_bypass() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
        assert_eq!(
            set.cycle_default_mode(ViewId::BASE),
            ModeCycle::Changed(SessionModeChoice::Auto)
        );
        let ModeCycle::Confirm(plan) = set.cycle_default_mode(ViewId::BASE) else {
            panic!("auto -> bypass asks")
        };
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
        assert_eq!(set.default_mode(), SessionModeChoice::Bypass);
        assert_eq!(
            set.cycle_default_mode(ViewId::BASE),
            ModeCycle::Changed(SessionModeChoice::Manual)
        );
    }

    /// A tab saved in manual comes back in manual with no question, whichever way the restore runs:
    /// it is the strictest mode.
    #[test]
    fn a_tab_saved_in_manual_comes_back_in_manual_unasked() {
        for policy in [BypassPolicy::Ask, BypassPolicy::Downgrade, BypassPolicy::Keep] {
            let mut set = set();
            let plan = plan_of(
                vec![
                    planned("m", SessionModeChoice::Manual),
                    planned("a", SessionModeChoice::Auto),
                ],
                None,
            );
            let RestoreStep::Go(go) = set.begin_restore(plan, policy) else {
                panic!("{policy:?}: nothing in bypass, nothing to ask")
            };
            assert_eq!((go.downgraded, go.asked), (0, false));
            let from = set.active();
            set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
            assert_eq!(mode_of(&set, "m"), SessionModeChoice::Manual, "{policy:?}");
            assert_eq!(mode_of(&set, "a"), SessionModeChoice::Auto, "{policy:?}");
        }
    }

    fn planned(id: &str, mode: SessionModeChoice) -> PlannedTab {
        PlannedTab {
            agent: AgentId::Claude,
            provider_session_id: id.to_string(),
            name: None,
            title: Some(format!("title of {id}")),
            saved_mode: mode,
            label: format!("title of {id}"),
            view: None,
            turn_running: false,
        }
    }

    fn plan_of(items: Vec<PlannedTab>, active: Option<&str>) -> RestorePlan {
        RestorePlan {
            items,
            skipped: Vec::new(),
            active_session: active.map(str::to_string),
        }
    }

    fn mode_of(set: &TabSet, session: &str) -> SessionModeChoice {
        let tab = set
            .tabs()
            .iter()
            .find(|t| t.provider_session_id().as_deref() == Some(session))
            .map(|t| t.id)
            .expect("the session has a tab");
        set.get(tab).unwrap().mode()
    }

    #[test]
    fn tabs_saved_in_auto_go_ahead_without_a_question() {
        let mut set = set();
        let plan = plan_of(
            vec![
                planned("a", SessionModeChoice::Auto),
                planned("b", SessionModeChoice::Auto),
            ],
            None,
        );
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("nothing in bypass, nothing to ask")
        };
        assert_eq!((go.downgraded, go.asked), (0, false));
    }

    #[test]
    fn a_tab_saved_in_bypass_asks_first_and_y_gives_it_back_in_bypass() {
        let mut set = set();
        let plan = plan_of(
            vec![
                planned("a", SessionModeChoice::Auto),
                planned("b", SessionModeChoice::Bypass),
                planned("c", SessionModeChoice::Auto),
            ],
            None,
        );
        let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("a bypass tab is asked about")
        };
        assert_eq!(prompt.lines[0], "Restore 3 tabs (1 in bypass)? y/n");
        assert!(
            prompt.lines[1].contains("n brings the bypass tab back in auto"),
            "{:?}",
            prompt.lines
        );
        let go = set.answer_restore(prompt.nonce, true).unwrap();
        assert_eq!((go.downgraded, go.asked), (0, true));
        let from = set.active();
        let mut asked = Vec::new();
        set.start_restore(from, go, |_, id| {
            asked.push(id.to_string());
            Ok(RestoreConnect::Started(mpsc::channel().1))
        });
        assert_eq!(asked, ["a", "b", "c"]);
        assert_eq!(mode_of(&set, "a"), SessionModeChoice::Auto);
        assert_eq!(mode_of(&set, "b"), SessionModeChoice::Bypass, "after a yes");
        assert_eq!(mode_of(&set, "c"), SessionModeChoice::Auto);
    }

    #[test]
    fn n_gives_the_bypass_tabs_back_in_auto_and_nothing_enters_bypass_without_a_yes() {
        let mut set = set();
        let plan = plan_of(
            vec![
                planned("a", SessionModeChoice::Bypass),
                planned("b", SessionModeChoice::Bypass),
            ],
            None,
        );
        let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("asked")
        };
        assert_eq!(prompt.lines[0], "Restore 2 tabs (2 in bypass)? y/n");
        let go = set.answer_restore(prompt.nonce, false).unwrap();
        assert_eq!(go.downgraded, 2);
        let from = set.active();
        set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
        assert_eq!(mode_of(&set, "a"), SessionModeChoice::Auto);
        assert_eq!(mode_of(&set, "b"), SessionModeChoice::Auto);
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Auto,
            "the window's default never moved"
        );
    }

    #[test]
    fn a_stale_or_dropped_answer_restores_nothing_and_a_newer_question_replaces_an_older() {
        let mut set = set();
        let bypass = || plan_of(vec![planned("a", SessionModeChoice::Bypass)], None);
        let RestoreStep::Confirm(first) = set.begin_restore(bypass(), BypassPolicy::Ask) else {
            panic!("asked")
        };
        let RestoreStep::Confirm(second) = set.begin_restore(bypass(), BypassPolicy::Ask) else {
            panic!("asked")
        };
        assert_ne!(first.nonce, second.nonce);
        assert!(
            set.answer_restore(first.nonce, true).is_err(),
            "answers the question that was replaced"
        );
        assert!(set.answer_restore(0, true).is_err());
        // Moving to another tab ends the question, as it does a bypass prompt.
        let other = set.open(AgentId::Claude);
        let _ = other;
        assert!(
            set.answer_restore(second.nonce, true).is_err(),
            "the question was about the tab that was left"
        );
        // And an answer is taken once.
        let RestoreStep::Confirm(third) = set.begin_restore(bypass(), BypassPolicy::Ask) else {
            panic!("asked")
        };
        assert!(set.answer_restore(third.nonce, false).is_ok());
        assert!(set.answer_restore(third.nonce, false).is_err());
    }

    #[test]
    fn a_quiet_restore_downgrades_bypass_and_says_so_and_the_users_own_default_keeps_it() {
        let mut set = set();
        let plan = || {
            plan_of(
                vec![
                    planned("a", SessionModeChoice::Bypass),
                    planned("b", SessionModeChoice::Auto),
                ],
                None,
            )
        };
        let RestoreStep::Go(quiet) = set.begin_restore(plan(), BypassPolicy::Downgrade) else {
            panic!("no question")
        };
        assert_eq!((quiet.downgraded, quiet.asked), (1, false));
        let RestoreStep::Go(kept) = set.begin_restore(plan(), BypassPolicy::Keep) else {
            panic!("no question")
        };
        assert_eq!((kept.downgraded, kept.asked), (0, false));
        let from = set.active();
        set.start_restore(from, kept, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
        assert_eq!(
            mode_of(&set, "a"),
            SessionModeChoice::Bypass,
            "init.lua said bypass is fine"
        );
    }

    #[test]
    fn replacing_the_draft_ends_the_late_mirror_allowance() {
        let mut set = set();
        let tab = set.active();
        set.set_draft(tab, "mirrored before the move");
        set.open(AgentId::Claude);
        assert!(set.select(tab));
        let view = ViewId::new(2).unwrap();
        assert_eq!(set.break_out(view), Ok(tab));
        assert!(set.take_late_draft(tab, ViewId::BASE));

        assert_eq!(set.join_back(view), Some(tab));
        set.get_mut(tab).unwrap().replace_draft("newer draft".into());
        assert!(!set.take_late_draft(tab, view));
    }

    #[test]
    fn a_late_draft_is_taken_once_from_the_view_the_tab_left_and_never_after_a_newer_one() {
        let mut set = set();
        let first = set.active();
        set.open(AgentId::Claude);
        assert!(set.select(first));
        let view = ViewId::new(2).unwrap();
        assert_eq!(set.break_out(view), Ok(first));
        assert!(
            !set.take_late_draft(first, ViewId::new(3).unwrap()),
            "not the view it left"
        );
        assert!(set.take_late_draft(first, ViewId::BASE));
        assert!(!set.take_late_draft(first, ViewId::BASE), "taken once");
        assert_eq!(set.join_back(view), Some(first));
        assert!(set.set_draft(first, "newer, from the page that shows it"));
        assert!(!set.take_late_draft(first, view), "a newer copy came in since the move");
    }

    #[test]
    fn a_late_draft_is_not_taken_once_the_draft_was_set_any_other_way() {
        use crate::scratch::EditDone;
        let mut set = set();
        let first = set.active();
        set.open(AgentId::Claude);
        assert!(set.select(first));
        let view = ViewId::new(2).unwrap();
        let joined = |set: &mut TabSet| {
            assert_eq!(set.break_out(view), Ok(first));
            assert_eq!(set.join_back(view), Some(first));
        };

        joined(&mut set);
        set.begin_scratch_edit(first, 1).unwrap();
        set.finish_scratch_edit(1, &EditDone::Written("edited in nvim".into()))
            .unwrap();
        assert!(
            !set.take_late_draft(first, view),
            "an nvim edit came back after the move"
        );
        assert_eq!(set.get(first).unwrap().session.draft, "edited in nvim");

        joined(&mut set);
        set.begin_scratch_edit(first, 2).unwrap();
        set.cancel_scratch_edits(&[(2, EditDone::Written("saved as nvim went".into()))]);
        assert!(
            !set.take_late_draft(first, view),
            "an edit saved as the editor went away"
        );

        joined(&mut set);
        set.note_sent(first);
        assert!(!set.take_late_draft(first, view), "the draft was just sent");

        joined(&mut set);
        set.begin_scratch_edit(first, 3).unwrap();
        set.finish_scratch_edit(3, &EditDone::Discarded).unwrap();
        assert!(
            set.take_late_draft(first, view),
            "an edit that saved nothing leaves the late mirror due"
        );
    }

    #[test]
    fn a_put_off_restore_whose_holder_broke_out_keeps_its_sessions_in_the_base_view() {
        let mut set = set();
        let first = set.active();
        set.open(AgentId::Claude);
        assert!(set.select(first));
        let from = set.active();
        let view = ViewId::new(2).unwrap();
        assert_eq!(set.break_out(view), Ok(from));
        let plan = plan_of(
            vec![
                planned("a", SessionModeChoice::Auto),
                planned("b", SessionModeChoice::Auto),
            ],
            Some("a"),
        );
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("no question")
        };
        assert!(!set.keep_put_off_restore(from, go, "put off"));
        assert!(set.v1_holds());
        for id in ["a", "b"] {
            let tab = set
                .tabs()
                .iter()
                .find(|t| t.failed_resume.as_deref() == Some(id))
                .map(|t| t.id)
                .unwrap_or_else(|| panic!("session {id} is kept"));
            assert_eq!(set.view_of(tab), ViewId::BASE, "session {id} is kept in the base view");
        }
        assert_eq!(set.get(from).unwrap().failed_resume, None, "the moved tab holds none");
        assert_eq!(set.view_of(from), view);
        let saved = set.saved_snapshot("conv").unwrap();
        assert!(
            saved.tabs.iter().all(|t| t.view.is_none()),
            "no kept session is saved as agent#2's"
        );
    }

    #[test]
    fn a_restore_whose_holder_broke_out_resumes_in_the_base_view() {
        let mut set = set();
        let first = set.active();
        set.open(AgentId::Claude);
        assert!(set.select(first));
        let from = set.active();
        let view = ViewId::new(2).unwrap();
        assert_eq!(set.break_out(view), Ok(from));
        assert_eq!(set.view_of(from), view);
        let plan = plan_of(
            vec![
                planned("a", SessionModeChoice::Auto),
                planned("b", SessionModeChoice::Auto),
            ],
            Some("a"),
        );
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("no question")
        };
        let run = set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
        for id in ["a", "b"] {
            let tab = set
                .tab_with_session(&AgentId::Claude, id)
                .unwrap_or_else(|| panic!("session {id} came back"));
            assert_eq!(
                set.view_of(tab),
                ViewId::BASE,
                "session {id} came back in the base view"
            );
            assert!(run.owns(tab));
        }
        assert_eq!(
            set.get(from).unwrap().provider_session_id(),
            None,
            "the moved tab holds none"
        );
        assert!(!run.began_in(from));
        assert!(set.view_of(set.active()).is_base());
        assert_eq!(set.active_tab().provider_session_id().as_deref(), Some("a"));
    }

    #[test]
    fn restored_tabs_start_in_order_the_first_in_the_empty_tab_and_the_saved_active_one_is_shown() {
        let mut set = set();
        let from = set.active();
        let plan = plan_of(
            vec![
                planned("a", SessionModeChoice::Auto),
                planned("b", SessionModeChoice::Auto),
                planned("c", SessionModeChoice::Auto),
            ],
            Some("b"),
        );
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("no question")
        };
        let run = set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
        let numbers: Vec<(u16, Option<String>)> =
            set.tabs().iter().map(|t| (t.number, t.provider_session_id())).collect();
        assert_eq!(
            numbers,
            vec![
                (1, Some("a".to_string())),
                (2, Some("b".to_string())),
                (3, Some("c".to_string()))
            ],
            "the first went into the tab the restore began in, the rest into new ones, in order"
        );
        assert_eq!(set.get(from).unwrap().provider_session_id().as_deref(), Some("a"));
        assert_eq!(set.active_tab().provider_session_id().as_deref(), Some("b"));
        assert!(run.owns(from) && run.began_in(from));
        assert_eq!(run.outcome(), None, "nothing has connected yet");
    }

    #[test]
    fn when_the_saved_active_tab_is_skipped_the_first_restored_tab_is_shown() {
        let mut set = set();
        let from = set.active();
        let mut plan = plan_of(
            vec![
                planned("a", SessionModeChoice::Auto),
                planned("b", SessionModeChoice::Auto),
            ],
            Some("gone"),
        );
        plan.skipped.push(Skipped {
            label: "gone".into(),
            reason: "its saved record is gone".into(),
        });
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("no question")
        };
        set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
        assert_eq!(set.active_tab().provider_session_id().as_deref(), Some("a"));
    }

    #[test]
    fn a_session_already_open_here_is_switched_to_and_never_resumed_twice() {
        let dir = workspace("tabs-restore-dedupe");
        let mut set = set();
        let holder = set.active();
        give_session(&mut set, holder, &dir, "s-open");
        let from = set.open(AgentId::Claude);
        let plan = plan_of(
            vec![
                planned("s-open", SessionModeChoice::Auto),
                planned("s-new", SessionModeChoice::Auto),
            ],
            None,
        );
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("no question")
        };
        let mut connected = Vec::new();
        let run = set.start_restore(from, go, |_, id| {
            connected.push(id.to_string());
            Ok(RestoreConnect::Started(mpsc::channel().1))
        });
        assert_eq!(connected, ["s-new"], "the open one is not connected again");
        assert_eq!(set.tab_with_session(&AgentId::Claude, "s-open"), Some(holder));
        assert_eq!(set.tabs().len(), 2, "and no tab was made for it");
        assert_eq!(run.outcome(), None, "s-new is still connecting");
        shut_down_all(&mut set);
    }

    #[test]
    fn a_tab_that_cannot_connect_is_skipped_before_any_tab_is_made_for_it() {
        let mut set = set();
        let from = set.active();
        let plan = plan_of(
            vec![
                planned("held", SessionModeChoice::Auto),
                planned("free", SessionModeChoice::Auto),
            ],
            None,
        );
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("no question")
        };
        let run = set.start_restore(from, go, |_, id| {
            if id == "held" {
                Err("open in another window".to_string())
            } else {
                Ok(RestoreConnect::Started(mpsc::channel().1))
            }
        });
        assert_eq!(
            set.tabs().len(),
            1,
            "the one that connected took the empty tab; none was made for the other"
        );
        assert_eq!(set.get(from).unwrap().provider_session_id().as_deref(), Some("free"));
        assert!(run.owns(from));
        assert_eq!(run.failed.len(), 1);
    }

    /// The whole of a restore as the panel drives it: every resume reports through `collect_starts`,
    /// and one message ends it.
    #[test]
    fn a_restore_ends_in_one_message_once_every_resume_has_returned() {
        let dir = workspace("tabs-restore-outcome");
        let mut set = set();
        let from = set.active();
        let mut plan = plan_of(
            vec![
                planned("a", SessionModeChoice::Auto),
                planned("b", SessionModeChoice::Auto),
                planned("c", SessionModeChoice::Auto),
            ],
            Some("c"),
        );
        plan.skipped.push(Skipped {
            label: "docs".into(),
            reason: "open in another window".into(),
        });
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("no question")
        };
        let mut senders = Vec::new();
        let mut run = set.start_restore(from, go, |_, _| {
            let (tx, rx) = mpsc::channel();
            senders.push(tx);
            Ok(RestoreConnect::Started(rx))
        });
        assert_eq!(run.outcome(), None);

        // a connects, b is refused, c has not answered yet.
        let (_provider, backend) = live(&dir);
        senders[0].send(Ok(backend)).unwrap();
        senders[1]
            .send(Err(refused("could not continue the previous session: no such session")))
            .unwrap();
        for collected in set.collect_starts() {
            match collected {
                StartCollected::Installed { tab, .. } => run.note_installed(tab),
                StartCollected::Failed { tab, error, .. } => {
                    run.note_failed(tab, &error.message);
                    set.discard_failed_restore(tab, run.back_to(tab));
                }
            }
        }
        assert_eq!(run.outcome(), None, "c is still connecting");
        assert_eq!(set.tabs().len(), 2, "the refused tab is gone, not left behind dead");
        assert!(set.tab_with_session(&AgentId::Claude, "b").is_none());

        let (_provider, backend) = live(&dir);
        senders[2].send(Ok(backend)).unwrap();
        for collected in set.collect_starts() {
            if let StartCollected::Installed { tab, .. } = collected {
                run.note_installed(tab);
            }
        }
        assert_eq!(
            run.outcome().unwrap().message(),
            "Restored 2 of 4 tabs; 2 could not be: docs (open in another window), title of b (could not continue the previous session: no such session)"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn when_the_tab_the_restore_began_in_fails_it_is_an_empty_tab_again() {
        let mut set = set();
        let from = set.active();
        let plan = plan_of(vec![planned("a", SessionModeChoice::Auto)], None);
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("no question")
        };
        let (tx, rx) = mpsc::channel();
        let mut rx = Some(rx);
        let mut run = set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(rx.take().unwrap())));
        tx.send(Err(refused("refused"))).unwrap();
        for collected in set.collect_starts() {
            if let StartCollected::Failed { tab, error, .. } = collected {
                run.note_failed(tab, &error.message);
                set.discard_failed_restore(tab, run.back_to(tab));
            }
        }
        assert!(matches!(set.get(from).unwrap().backend, TabBackend::NotStarted));
        assert!(set.is_pristine());
        assert_eq!(
            run.outcome().unwrap().message(),
            "Restored 0 of 1 tab; 1 could not be: title of a (refused)"
        );
    }

    /// Only a failed restored tab is cleaned up: a live session is never closed by this.
    #[test]
    fn discarding_leaves_any_tab_that_is_not_failed_alone() {
        let mut set = set();
        let tab = set.active();
        set.discard_failed_restore(tab, Some((SessionModeChoice::Auto, AgentId::Claude)));
        let other = set.open(AgentId::Claude);
        set.discard_failed_restore(other, None);
        assert_eq!(set.tabs().len(), 2);
    }

    /// What `shell` does every tick: show the memory the window's tabs. Each listed change reaches the
    /// file once; an empty tab, a repeat and the window closing reach it not at all.
    #[test]
    fn the_saved_file_follows_the_tabs_and_never_records_the_window_closing() {
        use crate::saved_tabs::{load, Loaded, TabMemory};
        let dir = workspace("tabs-memory");
        let state = crate::test_scratch_dir::ScratchDir::new("nv-tab-memory", "follows");
        let root = PathBuf::from("/home/user/project");
        let mut memory = TabMemory::new(Some(state.to_path_buf()), root.clone());
        let mut set = set();
        let show = |set: &TabSet, memory: &mut TabMemory| memory.observe(set.saved_snapshot(PROJECT)).is_some();
        let on_disk = |state: &Path| match load(state, &root) {
            Loaded::Saved(saved) => saved,
            other => panic!("expected a saved file, found {other:?}"),
        };

        assert!(
            !show(&set, &mut memory),
            "a launch that has done nothing writes nothing"
        );

        let first = set.active();
        give_session(&mut set, first, &dir, "s-one");
        assert!(show(&set, &mut memory), "a session adopted");
        assert_eq!(ids(&on_disk(&state)), ["s-one"]);
        assert!(!show(&set, &mut memory), "the same tabs again");

        set.rename(first, "api");
        assert!(show(&set, &mut memory), "renamed");
        assert_eq!(on_disk(&state).tabs[0].name.as_deref(), Some("api"));

        let empty = set.open(AgentId::Claude);
        assert!(
            !show(&set, &mut memory),
            "an empty tab is not listed, and neither is its being active"
        );
        let second = set.open(AgentId::Claude);
        give_session(&mut set, second, &dir, "s-two");
        assert!(show(&set, &mut memory), "a second session");
        assert_eq!(on_disk(&state).active, Some(1));

        set.select(first);
        assert!(show(&set, &mut memory), "the active tab switched");
        assert_eq!(on_disk(&state).active, Some(0));

        set.get_mut(second).unwrap().mode = SessionModeChoice::Bypass;
        assert!(show(&set, &mut memory), "its mode changed");
        assert_eq!(on_disk(&state).tabs[1].mode, SessionModeChoice::Bypass);

        let gone = set.remove(empty);
        assert!(gone.is_some());
        assert!(!show(&set, &mut memory), "closing an empty tab changes nothing listed");
        set.remove(second);
        assert!(show(&set, &mut memory), "a listed tab closed");
        assert_eq!(ids(&on_disk(&state)), ["s-one"]);

        // The window closing takes every tab to tear them down: nothing is recorded.
        let before = std::fs::read_to_string(state.join(crate::layout::persist::file_name(&root))).unwrap();
        let taken = set.take_all();
        assert!(!show(&set, &mut memory));
        for tab in taken {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
        assert!(!show(&set, &mut memory));
        let after = std::fs::read_to_string(state.join(crate::layout::persist::file_name(&root))).unwrap();
        assert_eq!(before, after, "the file still says what was open");
    }

    // ---- Turn review through the pump ------------------------------------------------------------

    /// A directory of a test's own inside the target dir (beside the test binary), never under the
    /// user's state directory.
    fn target_scratch(label: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let dir = exe
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join("turn-review-tests")
            .join(format!(
                "{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
        std::fs::create_dir_all(dir.join("project")).unwrap();
        std::fs::write(dir.join("project/main.rs"), "fn main() {}\n").unwrap();
        dir.canonicalize().unwrap()
    }

    /// Holds the worker on its first base snapshot until released.
    #[derive(Clone, Default)]
    struct HeldBase(Arc<(std::sync::Mutex<(bool, bool)>, std::sync::Condvar)>);

    impl HeldBase {
        fn hook(&self) -> crate::turn_review::JobHook {
            let held = self.clone();
            Arc::new(move |event: &crate::turn_review::JobEvent| {
                if let crate::turn_review::JobEvent::Starting {
                    kind: crate::turn_review::SnapshotKind::Base,
                    ..
                } = event
                {
                    let (lock, cvar) = &*held.0;
                    let mut state = lock.lock().unwrap();
                    state.1 = true;
                    cvar.notify_all();
                    while state.0 {
                        state = cvar.wait(state).unwrap();
                    }
                }
            })
        }
        fn hold(&self) {
            self.0 .0.lock().unwrap().0 = true;
        }
        fn release(&self) {
            self.0 .0.lock().unwrap().0 = false;
            self.0 .1.notify_all();
        }
        fn parked(&self) -> bool {
            self.0 .0.lock().unwrap().1
        }
    }

    /// One auto tab on a session whose CLI runs auto: a turn whose in-project `Write` is deferred to
    /// the CLI, which removes the request from the batch. Returns the answers the provider got and,
    /// with a review installed, the turn's record once its base landed.
    fn auto_turn_through_the_pump(
        scratch: &Path,
        review: Option<crate::turn_review::TurnReview>,
        held: &HeldBase,
    ) -> (
        Vec<(String, agent::PermissionDecision)>,
        Option<crate::turn_review::TurnRecord>,
    ) {
        let dir = scratch.join("project");
        let mut set = set();
        let reviewing = review.is_some();
        if let Some(review) = review {
            set.install_turn_review(review).unwrap();
        }
        let tab = set.active();
        let provider = Arc::new(RecordingProvider::cli_auto());
        let conversation =
            AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        });
        let input = serde_json::json!({ "file_path": dir.join("main.rs"), "content": "fn main() { }\n" });
        provider.queue_all(vec![
            AgentDomainEvent::CliPermissionMode {
                reported: "auto".into(),
            },
            AgentDomainEvent::SessionOpened {
                session_id: "fake-session".into(),
                provider_session_id: "sess-auto".into(),
                model: "m".into(),
                cwd: dir.to_string_lossy().into_owned(),
            },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "tu-w".into(),
                name: "Write".into(),
                input: input.clone(),
            },
            AgentDomainEvent::PermissionRequested {
                permission_id: "perm-w".into(),
                tool_use_id: Some("tu-w".into()),
                tool_name: "Write".into(),
                input,
                provider_prompt: None,
            },
        ]);
        until("the deferral", || {
            set.pump(&dir, true);
            !provider.decisions().is_empty()
        });
        let mut record = None;
        if reviewing {
            until("the base held on the worker", || held.parked());
            held.release();
            until("the base", || {
                set.pump(&dir, true);
                record = set.turn_review().unwrap().turns("sess-auto").into_iter().next();
                record
                    .as_ref()
                    .is_some_and(|r| r.base != crate::turn_review::Snap::Pending)
            });
        }
        let decisions = provider.decisions();
        shut_down_all(&mut set);
        (decisions, record)
    }

    #[test]
    fn an_auto_answered_write_with_a_slow_base_is_marked_late_through_the_pump() {
        let scratch = target_scratch("late-through-pump");
        let held = HeldBase::default();
        held.hold();
        let review = crate::turn_review::TurnReview::with_options(
            Some(scratch.join("state/review")),
            &scratch.join("project"),
            crate::turn_review::ReviewOptions {
                excludes: Some(None),
                on_job: Some(held.hook()),
                trim_every: Duration::MAX,
                ..Default::default()
            },
        );
        let (with_review, record) = auto_turn_through_the_pump(&scratch, Some(review), &held);
        let record = record.unwrap();
        assert!(
            matches!(record.base, crate::turn_review::Snap::Taken { .. }),
            "{record:#?}"
        );
        assert!(
            record.late,
            "the deferred Write could have run before the base: {record:#?}"
        );
        assert_eq!(
            with_review,
            vec![("perm-w".to_string(), agent::PermissionDecision::Defer)],
            "the CLI decided, no card was drawn"
        );

        // The review changes no answer: the same turn with none installed is answered the same.
        let (without_review, _) = auto_turn_through_the_pump(&scratch, None, &HeldBase::default());
        assert_eq!(with_review, without_review);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_review_with_review_off_says_so() {
        use crate::turn_review::{Scope, TurnRef};
        let mut set = set();
        let tab = set.active();
        set.turn_review_off();
        assert_eq!(
            set.review_overview_job(tab, TurnRef::Latest, Scope::Turn)
                .err()
                .as_deref(),
            Some(TURN_REVIEW_OFF)
        );
        assert_eq!(
            set.review_diff_job(tab, 1, Scope::Turn, "main.rs").err().as_deref(),
            Some(TURN_REVIEW_OFF)
        );
        assert!(set.turn_review().is_none());
    }

    #[test]
    fn review_jobs_are_refused_without_a_review_a_tab_or_a_session() {
        use crate::turn_review::{Scope, TurnRef};
        let scratch = target_scratch("review-refused");
        let dir = scratch.join("project");
        let mut set = set();
        let tab = set.active();
        let refused = |set: &TabSet, tab: TabId| {
            (
                set.review_overview_job(tab, TurnRef::Latest, Scope::Turn).err(),
                set.review_diff_job(tab, 1, Scope::Turn, "main.rs").err(),
            )
        };
        let (overview, diff) = refused(&set, tab);
        assert_eq!(overview.as_deref(), Some(TURN_REVIEW_NEEDS_SIDECAR));
        assert_eq!(diff.as_deref(), Some(TURN_REVIEW_NEEDS_SIDECAR));

        set.install_turn_review(crate::turn_review::TurnReview::with_options(
            Some(scratch.join("state/review")),
            &dir,
            crate::turn_review::ReviewOptions {
                excludes: Some(None),
                ..Default::default()
            },
        ))
        .unwrap();
        let (overview, diff) = refused(&set, tab);
        assert!(overview.unwrap().contains("no session yet"));
        assert!(diff.unwrap().contains("no session yet"));
        let (overview, _) = refused(&set, TabId(9999));
        assert!(overview.unwrap().contains("no tab 9999"));
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// The whole route a review request takes in the shell, minus the GTK thread: the pump observes a
    /// turn that edited a file, the tab builds the job from its own tool calls, and a worker runs it.
    #[test]
    fn a_review_job_attributes_with_the_tabs_own_edit_calls() {
        use crate::turn_review::{Origin, Scope, TurnRef};
        let scratch = target_scratch("review-job");
        let dir = scratch.join("project");
        let mut set = set();
        set.install_turn_review(crate::turn_review::TurnReview::with_options(
            Some(scratch.join("state/review")),
            &dir,
            crate::turn_review::ReviewOptions {
                excludes: Some(None),
                trim_every: Duration::MAX,
                ..Default::default()
            },
        ))
        .unwrap();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.open_session("sess-job", &dir);
        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        until("the base", || {
            set.pump(&dir, true);
            set.turn_review()
                .unwrap()
                .turns("sess-job")
                .first()
                .is_some_and(|r| matches!(r.base, crate::turn_review::Snap::Taken { .. }))
        });
        // Two files change on disk during the turn; only one is named by a finished edit call.
        std::fs::write(dir.join("main.rs"), "fn main() { run(); }\n").unwrap();
        std::fs::write(dir.join("other.txt"), "from a shell command\n").unwrap();
        let input = serde_json::json!({ "file_path": dir.join("main.rs"), "content": "x" });
        provider.queue_all(vec![
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "tu-w".into(),
                name: "Write".into(),
                input,
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: "t1".into(),
                tool_use_id: "tu-w".into(),
                content: serde_json::json!("ok"),
                is_error: false,
            },
            completed("t1", agent::TurnOutcome::Completed),
        ]);
        until("the end", || {
            set.pump(&dir, true);
            set.turn_review()
                .unwrap()
                .turns("sess-job")
                .first()
                .is_some_and(|r| matches!(r.end, crate::turn_review::Snap::Taken { .. }))
        });
        let overview = set
            .review_overview_job(tab, TurnRef::Latest, Scope::Turn)
            .unwrap()
            .run()
            .unwrap();
        let origin = |name: &str| {
            overview
                .files
                .iter()
                .find(|f| f.path == Path::new(name))
                .unwrap_or_else(|| panic!("{name} not in {:#?}", overview.files))
                .origin
        };
        assert_eq!(origin("main.rs"), Origin::Agent);
        assert_eq!(origin("other.txt"), Origin::Workspace);
        let diff = set
            .review_diff_job(tab, overview.current, Scope::Turn, "main.rs")
            .unwrap()
            .run()
            .unwrap();
        assert!(diff.hunks.is_some_and(|h| !h.is_empty()));
        shut_down_all(&mut set);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// With a turn review installed, the set needs pumping while the review's worker owes a snapshot: from
    /// the turn's end until the pump has collected its end snapshot (held here on the worker), and not
    /// after both snapshots are in.
    #[test]
    fn a_turn_reviews_snapshots_in_flight_need_pumping() {
        let scratch = target_scratch("review-in-flight");
        let dir = scratch.join("project");
        let gate: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)> = Arc::default();
        let hook: crate::turn_review::JobHook = {
            let gate = gate.clone();
            Arc::new(move |event: &crate::turn_review::JobEvent| {
                if let crate::turn_review::JobEvent::Starting {
                    kind: crate::turn_review::SnapshotKind::End,
                    ..
                } = event
                {
                    let (held, cvar) = &*gate;
                    let mut held = held.lock().unwrap();
                    while *held {
                        held = cvar.wait(held).unwrap();
                    }
                }
            })
        };
        *gate.0.lock().unwrap() = true;
        let mut set = set();
        set.install_turn_review(crate::turn_review::TurnReview::with_options(
            Some(scratch.join("state/review")),
            &dir,
            crate::turn_review::ReviewOptions {
                excludes: Some(None),
                trim_every: Duration::MAX,
                on_job: Some(hook),
                ..Default::default()
            },
        ))
        .unwrap();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        set.set_ui_wake(Arc::new(|| {}));
        provider.open_session("sess-in-flight", &dir);
        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        let record = |set: &TabSet| set.turn_review().unwrap().turns("sess-in-flight").first().cloned();
        until("the base, and nothing left in flight", || {
            set.pump(&dir, true);
            record(&set).is_some_and(|r| matches!(r.base, crate::turn_review::Snap::Taken { .. }))
                && !set.needs_polling()
        });
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        until("the turn's end observed", || {
            set.pump(&dir, true);
            record(&set).is_some_and(|r| r.ended_ms.is_some())
        });
        for _ in 0..5 {
            assert!(set.needs_polling(), "the end snapshot is still with the worker");
            set.pump(&dir, true);
            std::thread::sleep(Duration::from_millis(10));
        }
        {
            let (held, cvar) = &*gate;
            *held.lock().unwrap() = false;
            cvar.notify_all();
        }
        until("both snapshots collected and nothing left in flight", || {
            set.pump(&dir, true);
            !set.needs_polling()
        });
        assert!(record(&set).is_some_and(|r| matches!(r.end, crate::turn_review::Snap::Taken { .. })));
        shut_down_all(&mut set);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// A review directory for a presence test, in its own scratch directory.
    fn presence_dir(label: &str) -> (PathBuf, PathBuf) {
        let scratch = target_scratch(label);
        let review = scratch.join("state/eitri/review/0123456789abcdef");
        (scratch, review)
    }

    /// What another window (pid 100) finds when it looks at the directory.
    fn seen_from_elsewhere(review: &Path) -> Option<crate::turn_review::Busy> {
        crate::turn_review::busy_elsewhere(review, 100).unwrap()
    }

    #[test]
    fn pump_feeds_presence_off_the_gtk_thread() {
        use crate::turn_review::{Blocked, Busy, PresenceHolder};
        let (scratch, review) = presence_dir("presence-pump");
        let dir = scratch.join("project");
        let mut set = set();
        let (holder, release) = PresenceHolder::start_parked(review.clone(), 4242);
        set.hold_presence(holder);
        let guard = set.presence_guard().expect("held");
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        // The presence thread has not opened anything, and the pump that folds the turn start
        // neither waits for it nor reads the disk.
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        assert!(!review.join("windows").exists(), "nothing was opened yet");
        assert!(
            matches!(guard.check(), Err(Blocked::NotHeld(_))),
            "a window that holds nothing refuses"
        );

        release.send(()).unwrap();
        until("the turn is seen elsewhere", || {
            seen_from_elsewhere(&review) == Some(Busy::TurnRunning { pid: 4242 })
        });
        assert_eq!(guard.check(), Err(Blocked::TurnRunningHere));

        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        pump_until_running(&mut set, &dir, tab, false);
        until("the turn is over", || {
            seen_from_elsewhere(&review) == Some(Busy::OtherWindow { pid: 4242 })
        });
        assert_eq!(guard.check(), Ok(()), "the count went back to nothing");
        shut_down_all(&mut set);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_legacy_window_holds_presence_and_its_running_turn() {
        use crate::turn_review::{Busy, PresenceHolder};
        let (scratch, review) = presence_dir("presence-legacy");
        let dir = scratch.join("project");
        let mut set = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
        let off = crate::turn_review::TurnReview::with_options(
            None,
            Path::new("/nonexistent"),
            crate::turn_review::ReviewOptions::default(),
        );
        assert_eq!(set.install_turn_review(off), Err(TURN_REVIEW_NEEDS_SIDECAR));
        set.hold_presence(PresenceHolder::start(review.clone(), 4242));
        until("the window is seen elsewhere", || {
            seen_from_elsewhere(&review) == Some(Busy::OtherWindow { pid: 4242 })
        });

        // A send can fold a legacy turn before the pump sees it; presence must observe the
        // projection even when no review is installed.
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        until("the projection has the turn", || set.get(tab).unwrap().turn_running());
        set.pump(&dir, true);
        until("the turn is seen elsewhere", || {
            seen_from_elsewhere(&review) == Some(Busy::TurnRunning { pid: 4242 })
        });
        shut_down_all(&mut set);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn presence_needs_no_turn_review() {
        use crate::turn_review::{Busy, PresenceHolder};
        let (scratch, review) = presence_dir("presence-review-off");
        let dir = scratch.join("project");
        let mut set = set();
        set.turn_review_off();
        set.hold_presence(PresenceHolder::start(review.clone(), 4242));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        until("the turn is seen elsewhere", || {
            seen_from_elsewhere(&review) == Some(Busy::TurnRunning { pid: 4242 })
        });
        assert!(
            !review.join("git").exists(),
            "presence does not make a shadow repository"
        );
        shut_down_all(&mut set);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_set_without_presence_has_no_guard() {
        assert!(set().presence_guard().is_none());
    }

    #[test]
    fn removing_a_running_tab_drops_its_running_file() {
        use crate::turn_review::{Busy, PresenceHolder};
        let (scratch, review) = presence_dir("presence-remove");
        let dir = scratch.join("project");
        let mut set = set();
        set.hold_presence(PresenceHolder::start(review.clone(), 4242));
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        until("the turn is seen elsewhere", || {
            seen_from_elsewhere(&review) == Some(Busy::TurnRunning { pid: 4242 })
        });
        set.open(AgentId::Claude);
        let mut closed = set.remove(tab).unwrap();
        // Its backend may still be writing until its shutdown returns: the file stays until then.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(seen_from_elsewhere(&review), Some(Busy::TurnRunning { pid: 4242 }));
        if let Some(retired) = closed.retired.take() {
            retired.into_backend().shutdown();
        }
        set.retire_done(tab);
        until("the turn's file is gone", || {
            seen_from_elsewhere(&review) == Some(Busy::OtherWindow { pid: 4242 })
        });
        shut_down_all(&mut set);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn the_legacy_backend_refuses_a_turn_review() {
        let mut set = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
        let review = crate::turn_review::TurnReview::with_options(
            None,
            Path::new("/nonexistent"),
            crate::turn_review::ReviewOptions::default(),
        );
        assert_eq!(set.install_turn_review(review), Err(TURN_REVIEW_NEEDS_SIDECAR));
        assert!(set.turn_review().is_none());
    }

    #[test]
    fn closing_a_tab_ends_its_turn_in_the_review() {
        let scratch = target_scratch("close-ends");
        let dir = scratch.join("project");
        let mut set = set();
        let options = crate::turn_review::ReviewOptions {
            excludes: Some(None),
            trim_every: Duration::MAX,
            ..Default::default()
        };
        set.install_turn_review(crate::turn_review::TurnReview::with_options(
            Some(scratch.join("state/review")),
            &dir,
            options,
        ))
        .unwrap();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.open_session("sess-close", &dir);
        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        until("the base", || {
            set.pump(&dir, true);
            set.turn_review()
                .unwrap()
                .turns("sess-close")
                .first()
                .is_some_and(|r| matches!(r.base, crate::turn_review::Snap::Taken { .. }))
        });
        let other = set.open(AgentId::Claude);
        let mut closed = set.remove(tab).unwrap();
        if let Some(retired) = closed.retired.take() {
            retired.into_backend().shutdown();
        }
        assert_eq!(set.active(), other);
        // The tab's session stays tracked until its end snapshot is reported.
        until("the end", || {
            set.pump(&dir, true);
            set.turn_review().unwrap().turns("sess-close").is_empty()
        });
        let refs = std::fs::read_dir(scratch.join("state/review/git/refs/eitri/sess-close/1")).unwrap();
        let mut kinds: Vec<String> = refs.map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        kinds.sort();
        assert_eq!(kinds, vec!["base".to_string(), "end".to_string()]);
        shut_down_all(&mut set);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    fn review_draft_revert(path: &str, undo: Option<crate::turn_review::UndoData>) -> crate::turn_review::NewRevert {
        use crate::turn_review::{NewRevert, RevertShape, RevertSource};
        NewRevert {
            turn: 1,
            path: path.into(),
            hunk: None,
            shape: RevertShape::Deleted,
            source: RevertSource::Panel,
            at_line: 1,
            reverted_to: Vec::new(),
            replaced: b"x\n".to_vec(),
            undo,
        }
    }

    #[test]
    fn the_draft_survives_a_panel_reload() {
        use crate::turn_review::{Overview, ReviewDraft, Scope};
        let mut set = set();
        let tab = set.active();
        set.review_draft_mut(tab)
            .unwrap()
            .add_comment(1, "a.rs", 3, 4, vec!["let x = 1;".into()], "why?")
            .unwrap();
        let overview = Overview {
            session: "s".into(),
            scope: Scope::Turn,
            current: 1,
            turns: Vec::new(),
            files: Vec::new(),
            compared: false,
            pending_no_result: 0,
            notes: Vec::new(),
        };
        // A reload asks again: the answer carries what the tab holds, not what the WebView kept.
        let sent = crate::agent_bridge::serialize_review_for_js("r1", tab, &overview, set.review_draft(tab).unwrap());
        let sent: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(sent["draft"]["comments"][0]["text"], "why?");
        assert_eq!(sent["draft"]["comments"][0]["anchor"][0], "let x = 1;");
        let empty = crate::agent_bridge::serialize_review_for_js("r2", tab, &overview, &ReviewDraft::default());
        let empty: serde_json::Value = serde_json::from_str(&empty).unwrap();
        assert_eq!(empty["draft"]["comments"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn undo_data_keeps_kind_and_mode() {
        use crate::turn_review::{UndoData, UndoState};
        let mut set = set();
        let tab = set.active();
        let undo = UndoData {
            pre: UndoState::Regular {
                blob: "pre".into(),
                mode: 0o755,
            },
            post: UndoState::Absent,
        };
        let id = set
            .review_draft_mut(tab)
            .unwrap()
            .record_revert(review_draft_revert("run.sh", Some(undo.clone())));
        let draft = set.review_draft(tab).unwrap();
        let last = draft.last_undoable().unwrap();
        assert_eq!(last.id, id);
        assert_eq!(last.new.undo, Some(undo));
    }

    #[test]
    fn the_draft_is_dropped_with_its_tab() {
        let mut set = set();
        let first = set.active();
        set.review_draft_mut(first)
            .unwrap()
            .record_revert(review_draft_revert("a", None));
        let second = set.open(AgentId::Claude);
        assert!(set.review_draft(second).unwrap().is_empty(), "a new tab starts empty");
        set.remove(first).unwrap();
        assert!(set.review_draft(first).is_err(), "the draft went with its tab");
        assert!(set.review_draft_mut(first).is_err());
        assert!(set.review_draft(second).unwrap().is_empty());
    }

    #[test]
    fn send_review_folds_the_message_as_typed() {
        let dir = workspace("tabs-send-review");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let text = "Review of your last turn: 1 comment, 0 reverts.\n\nComments:\n1. a.rs:3-4\n   > x\n   why?";
        let flush = set
            .send_review(tab, text, 5)
            .unwrap()
            .expect("an idle tab sends at once");
        assert!(flush.outcome.is_ok());
        assert_eq!(flush.typed, text);
        assert_eq!(provider.turns(), vec![text.to_string()], "wire == typed == text");
        until("the prompt in the transcript", || {
            let backend = set.get(tab).unwrap().live().unwrap();
            let projection = backend.projection();
            projection.user_prompts.last().is_some_and(|p| p.text == text)
        });
        assert!(set.get(tab).unwrap().session.queue.is_empty());
        shut_down_all(&mut set);
    }

    #[test]
    fn send_review_queues_behind_a_running_turn() {
        let dir = workspace("tabs-send-review-queued");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);

        assert!(
            set.send_review(tab, "the review", 5).unwrap().is_none(),
            "queued, not sent"
        );
        assert!(provider.turns().is_empty());
        assert_eq!(set.queue_texts(tab), vec!["the review".to_string()]);

        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        pump_until_running(&mut set, &dir, tab, false);
        let flush = set.flush_queue(tab).expect("it goes out when the turn ends");
        assert_eq!(flush.typed, "the review");
        assert_eq!(provider.turns(), vec!["the review".to_string()]);
        shut_down_all(&mut set);
    }

    #[test]
    fn send_review_on_a_tab_with_no_session_is_refused() {
        let mut set = set();
        let tab = set.active();
        assert!(
            set.send_review(tab, "the review", 5).is_err(),
            "an empty tab has no session"
        );
        assert_eq!(set.queued_count(), 0, "nothing was left behind");
        assert!(set.send_review(TabId(999), "the review", 5).is_err(), "no such tab");
    }

    #[test]
    fn a_ticket_needs_a_tab_with_a_session_and_is_current_until_it_ends() {
        let dir = target_scratch("ticket-current").join("project");
        let mut set = set();
        let tab = set.active();
        assert!(set.review_ticket(tab).is_err(), "an empty tab has no session");
        assert_eq!(
            set.review_ticket(TabId(999)).unwrap_err(),
            "protocol: no tab 999",
            "the text a missing tab always gets"
        );
        give_session(&mut set, tab, &dir, "sess-ticket");
        let ticket = set.review_ticket(tab).unwrap();
        assert_eq!(ticket.session, "sess-ticket");
        assert!(ticket.is_current(&set));
        ticket.cancel();
        assert!(!ticket.is_current(&set), "a cancelled ticket is never current");
        assert!(!ticket.wanted.load(Ordering::SeqCst));
        shut_down_all(&mut set);
    }

    #[test]
    fn a_ticket_is_not_current_for_another_session_of_the_same_tab() {
        let dir = target_scratch("ticket-session").join("project");
        let mut set = set();
        let tab = set.active();
        give_session(&mut set, tab, &dir, "sess-one");
        let ticket = set.review_ticket(tab).unwrap();
        let old = std::mem::replace(&mut set.get_mut(tab).unwrap().backend, TabBackend::NotStarted);
        if let TabBackend::Live(mut backend) = old {
            backend.shutdown();
        }
        assert!(!ticket.is_current(&set), "the tab holds no session now");
        give_session(&mut set, tab, &dir, "sess-two");
        assert!(!ticket.is_current(&set), "another session is not the one asked about");
        assert!(set.review_ticket(tab).unwrap().is_current(&set));
        shut_down_all(&mut set);
    }

    #[test]
    fn removing_a_tab_cancels_its_tickets() {
        let dir = target_scratch("ticket-remove").join("project");
        let mut set = set();
        let tab = set.active();
        give_session(&mut set, tab, &dir, "sess-remove");
        let other = set.open(AgentId::Claude);
        let ticket = set.review_ticket(tab).unwrap();
        let kept = set.review_ticket(tab).unwrap();
        assert!(ticket.wanted.load(Ordering::SeqCst));
        let mut closed = set.remove(tab).unwrap();
        // Right after the removal, before any pump: the flag is what a worker's last check reads.
        assert!(!ticket.wanted.load(Ordering::SeqCst));
        assert!(
            !kept.wanted.load(Ordering::SeqCst),
            "every ticket of the tab, not the first"
        );
        assert!(!ticket.is_current(&set));
        assert_eq!(set.active(), other);
        if let Some(retired) = closed.retired.take() {
            retired.into_backend().shutdown();
        }
        shut_down_all(&mut set);
    }

    #[test]
    fn removing_one_tab_leaves_another_tabs_tickets_alone() {
        let dir = target_scratch("ticket-other").join("project");
        let mut set = set();
        let first = set.active();
        give_session(&mut set, first, &dir, "sess-first");
        let second = set.open(AgentId::Claude);
        give_session(&mut set, second, &dir, "sess-second");
        let kept = set.review_ticket(second).unwrap();
        let mut closed = set.remove(first).unwrap();
        assert!(kept.is_current(&set));
        if let Some(retired) = closed.retired.take() {
            retired.into_backend().shutdown();
        }
        shut_down_all(&mut set);
    }

    #[test]
    fn take_all_cancels_every_ticket() {
        let dir = target_scratch("ticket-take-all").join("project");
        let mut set = set();
        let first = set.active();
        give_session(&mut set, first, &dir, "sess-a");
        let second = set.open(AgentId::Claude);
        give_session(&mut set, second, &dir, "sess-b");
        let a = set.review_ticket(first).unwrap();
        let b = set.review_ticket(second).unwrap();
        let tabs = set.take_all();
        assert!(!a.wanted.load(Ordering::SeqCst));
        assert!(!b.wanted.load(Ordering::SeqCst));
        for tab in tabs {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn a_finished_ticket_leaves_no_watcher_behind() {
        let dir = target_scratch("ticket-drop").join("project");
        let mut set = set();
        let tab = set.active();
        give_session(&mut set, tab, &dir, "sess-drop");
        for _ in 0..50 {
            drop(set.review_ticket(tab).unwrap());
        }
        let _kept = set.review_ticket(tab).unwrap();
        assert_eq!(
            set.get(tab).unwrap().review_watchers.len(),
            1,
            "dropped tickets are swept the next time one is made"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn the_availability_question_names_the_reason_without_a_tab() {
        let mut set = set();
        assert_eq!(set.review_unavailable(), Some(TURN_REVIEW_NEEDS_SIDECAR));
        set.turn_review_off();
        assert_eq!(set.review_unavailable(), Some(TURN_REVIEW_OFF));
        let tab = set.active();
        assert_eq!(set.review_session(tab).err().as_deref(), Some(TURN_REVIEW_OFF));
    }
    // ---- the trust question: a start put off, and who may answer it ----

    fn first_send(request_id: &str, typed: &str) -> DeferredStart {
        DeferredStart::FirstSend {
            request_id: request_id.to_string(),
            first_turn: FirstTurn {
                typed_by_person: true,
                wire: format!("wire: {typed}"),
                typed: typed.to_string(),
            },
        }
    }

    fn resume_of(request_id: &str, session: &str) -> DeferredStart {
        DeferredStart::Resume {
            request_id: request_id.to_string(),
            provider_session_id: session.to_string(),
            resumed_title: Some("a title".into()),
            resumed_name: None,
        }
    }

    fn pending_connect() -> PendingStart {
        PendingStart {
            request_id: "r-pending".into(),
            result_rx: mpsc::channel().1,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        }
    }

    /// A set whose active tab waits for the trust question, already asked: `(set, tab, nonce)`.
    fn asked(start: DeferredStart) -> (TabSet, TabId, u64) {
        let mut set = set();
        let tab = set.active();
        set.defer_start(tab, start).unwrap();
        let nonce = set.ask_trust(tab).unwrap();
        (set, tab, nonce)
    }

    fn awaiting(set: &TabSet, tab: TabId) -> &AwaitingTrust {
        match &set.get(tab).unwrap().backend {
            TabBackend::AwaitingTrust(waiting) => waiting,
            _ => panic!("the tab is not awaiting trust"),
        }
    }

    #[test]
    fn defer_start_needs_an_empty_tab() {
        let dir = workspace("trust-defer-needs-empty");
        let mut set = set();
        let starting = set.active();
        set.get_mut(starting).unwrap().backend = TabBackend::Starting(pending_connect());
        assert!(set.defer_start(starting, first_send("r1", "hi")).is_err(), "Starting");
        let running = set.open(AgentId::Claude);
        give_session(&mut set, running, &dir, "sess-live");
        assert!(set.defer_start(running, first_send("r2", "hi")).is_err(), "Live");
        let empty = set.open(AgentId::Claude);
        set.defer_start(empty, first_send("r3", "hi")).unwrap();
        assert!(
            set.defer_start(empty, first_send("r4", "again")).is_err(),
            "AwaitingTrust"
        );
        assert!(
            set.defer_start(TabId(999), first_send("r5", "hi")).is_err(),
            "no such tab"
        );
        assert!(
            matches!(set.get(starting).unwrap().backend, TabBackend::Starting(_)),
            "a refused deferral changes nothing"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn a_waiting_tab_is_shown_as_waiting_for_trust_not_as_starting() {
        let (set, tab, _) = asked(first_send("r1", "hi"));
        assert_eq!(set.get(tab).unwrap().wire_state(), TabStateWire::AwaitingTrust);
        assert_eq!(TabStateWire::AwaitingTrust.as_str(), "awaiting_trust");
        assert!(
            set.close_needs_confirm(tab),
            "closing it would lose the message it holds"
        );
    }

    #[test]
    fn ask_trust_rotates_the_nonce() {
        let (mut set, tab, first) = asked(first_send("r1", "hi"));
        let second = set.ask_trust(tab).unwrap();
        assert_ne!(first, second);
        assert_ne!(first, 0, "0 is never a valid nonce");
        assert_eq!(awaiting(&set, tab).asking, Some(second));
        assert!(set.ask_trust(TabId(999)).is_err());
        let empty = set.open(AgentId::Claude);
        assert!(set.ask_trust(empty).is_err(), "a tab with nothing to ask about");
    }

    #[test]
    fn an_old_nonce_is_refused() {
        let (mut set, tab, first) = asked(first_send("r1", "hi"));
        let second = set.ask_trust(tab).unwrap();
        assert!(set.check_trust_answer(tab, first, ViewId::BASE).is_err());
        assert!(set.mark_recording(tab, first, ViewId::BASE).is_err());
        assert!(set.take_answered(tab, first, ViewId::BASE).is_err());
        assert!(set.cancel_trust(tab, first, ViewId::BASE).is_err());
        assert!(
            awaiting(&set, tab).asking == Some(second),
            "refusals leave the question as it was"
        );
        assert!(set.check_trust_answer(tab, second, ViewId::BASE).is_ok());
        assert!(set.check_trust_answer(tab, 0, ViewId::BASE).is_err());
    }

    #[test]
    fn an_answer_for_a_tab_not_on_screen_is_refused() {
        let (mut set, first, _) = asked(first_send("r1", "hi"));
        let other = set.open(AgentId::Claude);
        assert_eq!(set.active(), other);
        let nonce = set.ask_trust(first).unwrap();
        let why = set.check_trust_answer(first, nonce, ViewId::BASE).unwrap_err();
        assert!(why.contains("not the one on screen"), "{why}");
        assert!(set.mark_recording(first, nonce, ViewId::BASE).is_err());
        assert!(set.take_answered(first, nonce, ViewId::BASE).is_err());
        assert!(set.cancel_trust(first, nonce, ViewId::BASE).is_err());
        assert!(matches!(set.get(first).unwrap().backend, TabBackend::AwaitingTrust(_)));
        assert!(
            set.check_trust_answer(other, nonce, ViewId::BASE).is_err(),
            "and not for a tab that waits for nothing"
        );
        set.select(first);
        let nonce = set.ask_trust(first).unwrap();
        assert!(
            set.check_trust_answer(first, nonce, ViewId::BASE).is_ok(),
            "on screen again, asked again"
        );
    }

    #[test]
    fn routing_away_invalidates_the_prompt() {
        for how in ["select", "drop_bypass_prompt", "cycle_mode", "cycle_default_mode"] {
            let (mut set, tab, nonce) = asked(first_send("r1", "hi"));
            match how {
                "select" => {
                    let other = set.open(AgentId::Claude);
                    assert_ne!(other, tab);
                    set.select(tab);
                }
                "drop_bypass_prompt" => set.drop_bypass_prompt(),
                "cycle_mode" => {
                    let _ = set.cycle_mode(tab, ViewId::BASE);
                }
                _ => {
                    let _ = set.cycle_default_mode(ViewId::BASE);
                }
            }
            assert!(
                set.check_trust_answer(tab, nonce, ViewId::BASE).is_err(),
                "{how}: the old nonce is refused"
            );
            assert!(set.take_answered(tab, nonce, ViewId::BASE).is_err(), "{how}");
            let waiting = awaiting(&set, tab);
            assert_eq!(waiting.asking, None, "{how}");
            assert!(!waiting.recording, "{how}");
            let fresh = set.ask_trust(tab).unwrap();
            assert!(
                set.check_trust_answer(tab, fresh, ViewId::BASE).is_ok(),
                "{how}: showing it again asks again"
            );
        }
    }

    #[test]
    fn check_trust_answer_consumes_nothing() {
        let (set, tab, nonce) = asked(first_send("r1", "hi"));
        assert!(set.check_trust_answer(tab, nonce + 1, ViewId::BASE).is_err());
        assert!(set.check_trust_answer(tab, nonce, ViewId::BASE).is_ok());
        assert!(
            set.check_trust_answer(tab, nonce, ViewId::BASE).is_ok(),
            "a second look still passes"
        );
        let waiting = awaiting(&set, tab);
        assert_eq!(waiting.asking, Some(nonce));
        assert!(!waiting.recording);
        assert!(matches!(waiting.start, DeferredStart::FirstSend { .. }));
    }

    #[test]
    fn an_accepted_answer_hands_the_start_over_once() {
        let (mut set, tab, nonce) = asked(first_send("r1", "hi"));
        let start = set.take_answered(tab, nonce, ViewId::BASE).unwrap();
        assert_eq!(start.request_id(), Some("r1"));
        assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
        assert!(
            set.take_answered(tab, nonce, ViewId::BASE).is_err(),
            "the answer is spent"
        );

        let (mut set, tab, nonce) = asked(resume_of("r2", "sess-9"));
        let start = set.cancel_trust(tab, nonce, ViewId::BASE).unwrap();
        assert!(
            matches!(start, DeferredStart::Resume { ref provider_session_id, .. } if provider_session_id == "sess-9")
        );
        assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
        assert!(set.awaiting_trust().is_empty());
    }

    #[test]
    fn a_recording_tab_waits_for_its_record() {
        let (mut set, tab, nonce) = asked(first_send("r1", "hi"));
        set.mark_recording(tab, nonce, ViewId::BASE).unwrap();
        let waiting = awaiting(&set, tab);
        assert!(waiting.recording);
        assert_eq!(waiting.asking, None);
        assert!(
            set.check_trust_answer(tab, nonce, ViewId::BASE).is_err(),
            "the nonce is spent"
        );
        assert!(set.take_answered(tab, nonce, ViewId::BASE).is_err());
        assert!(set.cancel_trust(tab, nonce, ViewId::BASE).is_err());
        assert!(
            set.mark_recording(tab, nonce, ViewId::BASE).is_err(),
            "no second record for one answer"
        );
        assert!(
            set.take_deferred(tab).is_none(),
            "the record's result decides, not the no-ask route"
        );
        assert!(matches!(set.get(tab).unwrap().backend, TabBackend::AwaitingTrust(_)));

        // The user went to another tab meanwhile: the record's result still finds its start.
        let other = set.open(AgentId::Claude);
        assert_eq!(set.active(), other);
        let start = set.take_recorded(tab).expect("the start is still held");
        assert_eq!(start.request_id(), Some("r1"));
        assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
        assert!(set.take_recorded(tab).is_none(), "handed over once");

        // A record that failed asks the tab again, which ends the wait.
        let (mut set, tab, nonce) = asked(first_send("r2", "hi"));
        set.mark_recording(tab, nonce, ViewId::BASE).unwrap();
        let again = set.ask_trust(tab).unwrap();
        assert_ne!(again, nonce);
        let waiting = awaiting(&set, tab);
        assert!(!waiting.recording);
        assert_eq!(waiting.asking, Some(again));
        assert!(set.take_recorded(tab).is_none(), "nothing is being recorded any more");
    }

    #[test]
    fn take_recorded_and_take_deferred_need_the_right_state() {
        let mut set = set();
        let tab = set.active();
        assert!(set.take_deferred(tab).is_none(), "an empty tab holds nothing");
        assert!(set.take_recorded(tab).is_none());
        set.defer_start(tab, first_send("r1", "hi")).unwrap();
        assert!(set.take_recorded(tab).is_none(), "not recording");
        let start = set.take_deferred(tab).expect("the no-ask route needs no question");
        assert_eq!(start.request_id(), Some("r1"));
        assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
        assert!(set.take_deferred(TabId(999)).is_none());
    }

    #[test]
    fn reset_hands_back_a_deferred_first_send() {
        let (mut set, tab, _) = asked(first_send("r1", "fix the build"));
        set.get_mut(tab).unwrap().replace_draft("and then the tests".into());
        let old = set.reset(tab).expect("a waiting tab starts over");
        assert!(old.is_none(), "nothing was spawned");
        let t = set.get(tab).unwrap();
        assert!(matches!(t.backend, TabBackend::NotStarted));
        assert_eq!(
            t.session.draft, "fix the build\n\nand then the tests",
            "the typed text comes back first"
        );
        assert_eq!(t.session.project_tiers, None);
        assert!(set.awaiting_trust().is_empty());

        // A resume has no text to give back: it is dropped.
        let (mut set, tab, _) = asked(resume_of("r2", "sess-1"));
        assert!(set.reset(tab).unwrap().is_none());
        assert!(set.get(tab).unwrap().session.draft.is_empty());
        assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
    }

    #[test]
    fn closing_an_awaiting_tab_drops_its_start() {
        let (mut set, tab, nonce) = asked(first_send("r1", "hi"));
        let other = set.open(AgentId::Claude);
        let closed = set.remove(tab).expect("the tab closes");
        match closed.tab.backend {
            TabBackend::AwaitingTrust(waiting) => assert_eq!(waiting.start.request_id(), Some("r1")),
            _ => panic!("the tab was waiting"),
        }
        assert!(set.get(tab).is_none());
        assert!(set.awaiting_trust().is_empty());
        assert!(
            set.check_trust_answer(tab, nonce, ViewId::BASE).is_err(),
            "no answer reaches a closed tab"
        );
        assert_eq!(set.active(), other);
    }

    #[test]
    fn an_awaiting_tab_refuses_a_second_send_and_a_resume() {
        let (mut set, tab, _) = asked(resume_of("r1", "sess-wait"));
        assert!(
            set.defer_start(tab, first_send("r2", "again")).is_err(),
            "a second start"
        );
        assert_eq!(
            set.get(tab).unwrap().provider_session_id().as_deref(),
            Some("sess-wait"),
            "the session it will resume is known"
        );
        let other = set.open(AgentId::Claude);
        // Resuming the same session again switches to the tab that already holds it: no second start.
        assert!(
            matches!(set.route_resume(other, &AgentId::Claude, "sess-wait", false), ResumeRoute::SwitchTo(t) if t == tab)
        );
        // A tab with a message waiting is not "empty": a resume from it opens another tab.
        let (mut set, waiting, _) = asked(first_send("r3", "hi"));
        match set.route_resume(waiting, &AgentId::Claude, "sess-new", false) {
            ResumeRoute::StartIn(target) => assert_ne!(target, waiting),
            _ => panic!("expected a new tab"),
        }
        assert!(matches!(
            set.get(waiting).unwrap().backend,
            TabBackend::AwaitingTrust(_)
        ));
    }

    #[test]
    fn a_deferred_restore_starts_nothing() {
        let mut set = set();
        let from = set.active();
        let plan = plan_of(
            vec![
                planned("a", SessionModeChoice::Auto),
                planned("b", SessionModeChoice::Auto),
            ],
            None,
        );
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("nothing in bypass, nothing to ask")
        };
        assert_eq!(go.session_ids(), ["a", "b"]);
        set.defer_start(from, DeferredStart::Restore { go }).unwrap();
        assert_eq!(set.awaiting_trust(), [from]);
        assert_eq!(set.tabs().len(), 1, "no tab was opened for the sessions");
        assert!(
            set.tabs().iter().all(|t| !matches!(t.backend, TabBackend::Starting(_))),
            "nothing is starting"
        );
        assert!(set.open_sessions().is_empty(), "no lease or session is named yet");
        let nonce = set.ask_trust(from).unwrap();
        let DeferredStart::Restore { go } = set.take_answered(from, nonce, ViewId::BASE).unwrap() else {
            panic!("the restore comes back")
        };
        assert_eq!(
            go.session_ids(),
            ["a", "b"],
            "intact, ready to start when the answer allows it"
        );
    }

    // ---- chat views: a tab bound to an extra view ------------------------------------------------

    fn view(n: u8) -> ViewId {
        ViewId::new(n).unwrap()
    }

    #[test]
    fn breaking_out_binds_the_active_tab_and_the_base_view_keeps_a_base_tab() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        let three = set.open(AgentId::Claude);
        assert_eq!(set.active(), three);

        assert_eq!(set.break_out(view(2)), Ok(three));
        assert_eq!(set.view_of(three), view(2));
        assert_eq!(set.get(three).unwrap().view_id(), view(2));
        assert_eq!(set.tab_in(view(2)), Some(three));
        assert_eq!(set.active(), two, "the base view shows its last active tab");
        assert_eq!(set.views(), vec![view(2)]);
        assert_eq!(set.base_tabs(), vec![one, two]);
        assert_eq!(set.shown(), vec![(ViewId::BASE, two), (view(2), three)]);
        assert!(set.is_shown(three) && set.is_shown(two) && !set.is_shown(one));
        assert_eq!(set.place_of(three), Some(TabPlace::View(view(2), three)));
        assert_eq!(set.place_of(one), Some(TabPlace::Base(one)));
        assert_eq!(set.select_last(), None, "the last active tab is cleared by a break");

        assert_eq!(set.break_out(ViewId::BASE), Err(ViewError::NotAnExtraView));
        assert_eq!(set.break_out(view(2)), Err(ViewError::Taken(view(2))));
        assert_eq!(set.break_out(view(3)), Ok(two));
        assert_eq!(set.active(), one, "the neighbour among the base view's tabs");
        assert_eq!(set.break_out(view(4)), Err(ViewError::OneTabLeft));
        assert_eq!(set.active(), one);
        assert_eq!(set.views(), vec![view(2), view(3)]);
        assert_eq!(set.tab_in(ViewId::BASE), None, "the base view's tab is `active`");

        assert_eq!(
            ViewError::NotAnExtraView.to_string(),
            "the base view is not an extra chat view"
        );
        assert_eq!(ViewError::Taken(view(2)).to_string(), "agent 2 already shows a tab");
        assert_eq!(ViewError::OneTabLeft.to_string(), "can't break with only one tab");
    }

    #[test]
    fn joining_back_makes_the_tab_the_base_views_active_one() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        assert_eq!(set.break_out(view(2)), Ok(two));
        let three = set.open(AgentId::Claude);
        assert_eq!(set.active(), three);

        assert_eq!(set.join_back(view(2)), Some(two));
        assert_eq!(set.active(), two);
        assert_eq!(set.view_of(two), ViewId::BASE);
        assert_eq!(set.tab_in(view(2)), None);
        assert!(set.views().is_empty());
        assert_eq!(set.base_tabs(), vec![one, two, three]);
        assert_eq!(
            set.select_last(),
            Some(three),
            "the tab that was active is the last active one"
        );

        assert_eq!(set.join_back(view(2)), None, "the view shows no tab");
        assert_eq!(set.join_back(ViewId::BASE), None);
        assert_eq!(set.active(), three);
    }

    #[test]
    fn may_answer_compares_views_never_the_active_tab() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        let three = set.open(AgentId::Claude);
        let four = set.open(AgentId::Claude);
        assert_eq!(set.break_out(view(2)), Ok(four));
        assert_eq!(set.break_out(view(3)), Ok(three));
        assert_eq!(set.active(), two);

        let expected = [
            (one, ViewId::BASE),
            (two, ViewId::BASE),
            (three, view(3)),
            (four, view(2)),
        ];
        for (tab, its_view) in expected {
            assert_eq!(set.view_of(tab), its_view);
            for page in [ViewId::BASE, view(2), view(3), view(4)] {
                assert_eq!(
                    set.may_answer(tab, page),
                    page == its_view,
                    "tab {tab:?} from page {page:?}"
                );
            }
        }
        assert!(
            set.may_answer(one, ViewId::BASE),
            "a base tab in the background is the base page's"
        );
        assert_ne!(set.active(), one);
        for page in [ViewId::BASE, view(2)] {
            assert!(!set.may_answer(TabId(999), page), "no such tab");
        }
        assert_eq!(set.view_of(TabId(999)), ViewId::BASE);
    }

    #[test]
    fn an_answer_from_the_page_a_tab_just_left_is_refused_and_the_card_waits() {
        let dir = workspace("views-answer-from-the-left-page");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let _other = set.open(AgentId::Claude);
        set.select(tab);

        provider.queue(gate_write("perm-hook", "toolu_views"));
        until("the gate's card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        assert_eq!(set.break_out(view(2)), Ok(tab));

        let number = set.get(tab).unwrap().number;
        let error = set
            .answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
            .expect_err("the base page no longer shows the tab");
        assert_eq!(error.message, format!("tab {number} is not shown in this page"));
        assert!(!error.benign);
        assert!(provider.resolutions().is_empty(), "nothing reached the agent");
        let t = set.get(tab).unwrap();
        assert!(t
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .contains_key("perm-hook"));
        assert!(t.card_is_waiting("perm-hook"), "the card still waits");

        set.answer_card(tab, view(2), "perm-hook", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        assert_eq!(provider.resolutions(), vec![("perm-hook".to_string(), true)]);
        shut_down_all(&mut set);
    }

    #[test]
    fn the_base_view_keeps_the_active_tab_through_every_path_that_moves_it() {
        // Every base-view tab closed while an extra view is live: a fresh base tab opens.
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        let three = set.open(AgentId::Claude);
        assert_eq!(set.break_out(view(2)), Ok(three));
        set.remove(two).unwrap();
        assert_eq!(set.active(), one);
        set.remove(one).unwrap();
        let fresh = set.active();
        assert!(fresh != one && fresh != two && fresh != three);
        assert_eq!(set.view_of(fresh), ViewId::BASE);
        assert_eq!(set.get(fresh).unwrap().number, 1);
        assert_eq!(set.tab_in(view(2)), Some(three), "the view keeps its tab");
        assert!(set.v1_holds());

        // A break-out whose last active tab is bound: the fallback skips it.
        let mut set = super::tests::set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        let three = set.open(AgentId::Claude);
        assert_eq!(set.break_out(view(2)), Ok(three));
        assert_eq!(set.active(), two);
        set.last_active = Some(three);
        let four = set.open(AgentId::Claude);
        set.last_active = Some(three);
        assert_eq!(set.break_out(view(3)), Ok(four));
        assert!(set.view_of(set.active()).is_base());
        assert_eq!(
            set.active(),
            two,
            "the neighbour among the base view's tabs, never the bound tab"
        );
        assert_eq!(set.select_last(), None);
        assert_eq!(set.base_tabs(), vec![one, two]);
        assert!(set.v1_holds());

        // A resume of the session a bound tab holds: switched to, and `active` stays.
        let mut set = super::tests::set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        set.defer_start(two, resume_of("r1", "sess-bound")).unwrap();
        assert_eq!(set.break_out(view(2)), Ok(two));
        assert_eq!(set.active(), one);
        assert_eq!(
            set.route_resume(one, &AgentId::Claude, "sess-bound", false),
            ResumeRoute::SwitchTo(two)
        );
        assert_eq!(set.active(), one);
        assert_eq!(set.view_of(two), view(2));
        assert!(set.v1_holds());

        // The active tab closed when its neighbour in tab order is bound: the fallback skips it.
        let mut set = super::tests::set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        let three = set.open(AgentId::Claude);
        set.select(two);
        assert_eq!(set.break_out(view(2)), Ok(two));
        set.select(one);
        set.last_active = None;
        set.remove(one).unwrap();
        assert_eq!(set.active(), three, "not tab 2, which is bound");
        assert!(set.v1_holds());
        let four = set.open(AgentId::Claude);
        set.last_active = Some(two);
        set.remove(four).unwrap();
        assert_eq!(set.active(), three, "a bound last active tab is not the fallback");
        assert!(set.v1_holds());
    }

    #[test]
    fn select_number_on_a_bound_tab_reports_its_view_and_leaves_active_alone() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        assert_eq!(set.break_out(view(2)), Ok(two));
        assert_eq!(set.active(), one);
        let n_two = set.get(two).unwrap().number;
        assert_eq!(set.select_number(n_two), Some(TabPlace::View(view(2), two)));
        assert_eq!(set.active(), one);
        assert!(!set.select(two), "a bound tab is never selected");
        assert_eq!(set.active(), one);
        assert_eq!(set.select_place(two), Some(TabPlace::View(view(2), two)));
        assert_eq!(set.active(), one);
        assert_eq!(set.select_place(TabId(999)), None);
        assert_eq!(set.select_number(9), None);

        let three = set.open(AgentId::Claude);
        assert_eq!(set.select_number(1), Some(TabPlace::Base(one)));
        assert_eq!(set.active(), one);
        assert_eq!(set.select_place(three), Some(TabPlace::Base(three)));
        assert_eq!(set.active(), three);
        assert_eq!(set.step(1), Some(one), "`n` skips the bound tab 2 and wraps");
        assert_eq!(set.step(1), Some(three));
        assert_eq!(set.step(-1), Some(one));
    }

    #[test]
    fn a_base_view_switch_drops_only_base_view_trust_questions() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        let bound = set.open_in(AgentId::Claude, view(2)).unwrap();
        set.defer_start(bound, first_send("r-bound", "hi")).unwrap();
        let bound_nonce = set.ask_trust(bound).unwrap();
        set.select(one);
        set.defer_start(one, first_send("r-base", "hello")).unwrap();
        let base_nonce = set.ask_trust(one).unwrap();
        assert!(set.check_trust_answer(one, base_nonce, ViewId::BASE).is_ok());

        assert!(set.select(two));
        assert_eq!(
            awaiting(&set, one).asking,
            None,
            "the base view's question went with its tab"
        );
        assert_eq!(
            awaiting(&set, bound).asking,
            Some(bound_nonce),
            "the extra view still shows its own"
        );
        assert!(set.check_trust_answer(bound, bound_nonce, view(2)).is_ok());
        let number = set.get(bound).unwrap().number;
        assert_eq!(
            set.check_trust_answer(bound, bound_nonce, ViewId::BASE),
            Err(format!("tab {number} is not shown in this page"))
        );
        set.mark_recording(bound, bound_nonce, view(2)).unwrap();
        assert!(awaiting(&set, bound).recording);
    }

    #[test]
    fn the_bypass_prompt_belongs_to_its_page() {
        let mut set = set();
        let base = set.active();
        let other = set.open(AgentId::Claude);
        set.select(base);
        let bound = set.open_in(AgentId::Claude, view(2)).unwrap();

        let built = plan(set.cycle_mode(bound, view(2)));
        assert_eq!(built.page, view(2));
        assert_eq!(
            set.confirm_bypass(built.scope, built.nonce, ViewId::BASE),
            Err("that prompt is no longer current".to_string())
        );
        set.drop_bypass_prompt_in(ViewId::BASE);
        assert!(set.select(other), "a base-view switch");
        assert!(
            set.pending_bypass.is_some(),
            "the base page's drops leave view 2's prompt"
        );
        set.drop_bypass_prompt_in(view(2));
        assert!(set.pending_bypass.is_none());
        assert_eq!(
            set.confirm_bypass(built.scope, built.nonce, view(2)),
            Err("that prompt is no longer current".to_string())
        );

        let again = plan(set.cycle_mode(bound, view(2)));
        assert!(matches!(
            set.confirm_bypass(again.scope, again.nonce, view(2)),
            Ok(ConfirmOutcome::Entered { .. })
        ));
        assert_eq!(set.get(bound).unwrap().mode(), SessionModeChoice::Bypass);

        // The window default's prompt is the page's that asked.
        set.default_mode = SessionModeChoice::Auto;
        let ModeCycle::Confirm(default_plan) = set.cycle_default_mode(view(2)) else {
            panic!("auto -> bypass asks first");
        };
        assert_eq!(default_plan.page, view(2));
        assert_eq!(
            set.confirm_bypass(default_plan.scope, default_plan.nonce, ViewId::BASE),
            Err("that prompt is no longer current".to_string())
        );
        assert!(matches!(
            set.confirm_bypass(default_plan.scope, default_plan.nonce, view(2)),
            Ok(ConfirmOutcome::Entered { .. })
        ));

        // The trust questions go only with the page that shows them.
        let waiting = set.open_in(AgentId::Claude, view(3)).unwrap();
        set.defer_start(waiting, first_send("r-view", "hi")).unwrap();
        let view_nonce = set.ask_trust(waiting).unwrap();
        let shown = set.active();
        set.defer_start(shown, first_send("r-base", "hello")).unwrap();
        let _ = set.ask_trust(shown).unwrap();
        set.drop_bypass_prompt_in(ViewId::BASE);
        assert_eq!(awaiting(&set, shown).asking, None);
        assert_eq!(awaiting(&set, waiting).asking, Some(view_nonce));
        assert!(set.check_trust_answer(waiting, view_nonce, view(3)).is_ok());
        let base_nonce = set.ask_trust(shown).unwrap();
        set.drop_bypass_prompt_in(view(3));
        assert_eq!(awaiting(&set, waiting).asking, None);
        assert_eq!(awaiting(&set, shown).asking, Some(base_nonce));
    }

    #[test]
    fn open_in_never_makes_its_tab_active() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        assert_eq!(set.active(), two);
        let opened = set.open_in(AgentId::Codex, view(2)).unwrap();
        assert_eq!(set.active(), two, "the base view's active tab is unchanged");
        assert_eq!(set.view_of(opened), view(2));
        assert_eq!(set.tab_in(view(2)), Some(opened));
        assert_eq!(set.get(opened).unwrap().number, 3, "the lowest free number");
        assert_eq!(*set.get(opened).unwrap().agent(), AgentId::Codex);
        assert!(matches!(set.get(opened).unwrap().backend, TabBackend::NotStarted));
        assert!(!set.recency.contains(&opened), "never active, not even for a moment");
        assert_eq!(set.select_last(), Some(one), "the last active tab is unchanged too");

        assert_eq!(
            set.open_in(AgentId::Claude, ViewId::BASE),
            Err(ViewError::NotAnExtraView)
        );
        assert_eq!(set.open_in(AgentId::Claude, view(2)), Err(ViewError::Taken(view(2))));
        assert_eq!(set.tabs().len(), 3, "a refused open makes no tab");
    }

    #[test]
    fn cycle_mode_and_confirm_bypass_accept_a_shown_tab_from_its_own_page() {
        let mut set = set();
        let base = set.active();
        let background = set.open(AgentId::Claude);
        set.select(base);
        let bound = set.open_in(AgentId::Claude, view(2)).unwrap();
        let n = |set: &TabSet, tab: TabId| set.get(tab).unwrap().number;

        assert_eq!(
            set.cycle_mode(bound, ViewId::BASE),
            Err(format!("tab {} is not shown in this page", n(&set, bound)))
        );
        assert_eq!(
            set.cycle_mode(base, view(2)),
            Err(format!("tab {} is not shown in this page", n(&set, base)))
        );
        assert_eq!(
            set.cycle_mode(background, ViewId::BASE),
            Err(format!("tab {} is not the one on screen", n(&set, background)))
        );

        let bound_plan = plan(set.cycle_mode(bound, view(2)));
        assert_eq!(bound_plan.page, view(2));
        assert!(matches!(
            set.confirm_bypass(bound_plan.scope, bound_plan.nonce, view(2)),
            Ok(ConfirmOutcome::Entered { .. })
        ));
        let base_plan = plan(set.cycle_mode(base, ViewId::BASE));
        assert_eq!(base_plan.page, ViewId::BASE);
        assert!(matches!(
            set.confirm_bypass(base_plan.scope, base_plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { .. })
        ));
        assert_eq!(set.get(bound).unwrap().mode(), SessionModeChoice::Bypass);
        assert_eq!(set.get(base).unwrap().mode(), SessionModeChoice::Bypass);
        assert_eq!(set.get(background).unwrap().mode(), SessionModeChoice::Auto);
    }

    #[cfg(feature = "legacy-backend")]
    #[test]
    fn legacy_events_reach_the_page_on_the_next_pump() {
        let dir = workspace("legacy-next-pump");
        let mut set = set();
        let tab = set.active();
        let mut backend = AgentBackend::Legacy(agent::AgentSession::for_tests());
        let outcome: Result<(), BackendError> = backend
            .send_turn("wire text", "typed text", PromptOrigin::Composed)
            .map(|_| ());
        assert!(
            outcome.is_ok(),
            "the send succeeds while event delivery waits for the pump"
        );
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let out = set.pump_views(&dir, &|_| true);
        let payload: serde_json::Value = serde_json::from_str(out.active_payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["events"][0]["type"], "user_prompt_submitted");
        assert_eq!(payload["events"][0]["text"], "typed text");
        assert_eq!(payload["events"][1]["type"], "turn_started");
        assert!(set.pump_views(&dir, &|_| true).active_payload.is_none());
        shut_down_all(&mut set);
    }

    #[cfg(feature = "legacy-backend")]
    #[test]
    fn a_rejected_legacy_send_keeps_its_prompt_in_the_pump() {
        let dir = workspace("legacy-refused-pump");
        let mut set = set();
        let tab = set.active();
        let mut backend = AgentBackend::Legacy(agent::AgentSession::for_tests());
        backend.send_turn("first", "first", PromptOrigin::Composed).unwrap();
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        set.pump_views(&dir, &|_| true);
        let error = set
            .get_mut(tab)
            .unwrap()
            .live_mut()
            .unwrap()
            .send_turn("second", "second", PromptOrigin::Composed)
            .unwrap_err();
        assert!(
            error.benign,
            "a running turn refuses another send without ending the session"
        );
        let out = set.pump_views(&dir, &|_| true);
        let payload: serde_json::Value = serde_json::from_str(out.active_payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["events"].as_array().unwrap().len(), 1);
        assert_eq!(payload["events"][0]["text"], "second");
        shut_down_all(&mut set);
    }

    #[cfg(feature = "legacy-backend")]
    #[test]
    fn a_legacy_snapshot_covers_queued_local_events() {
        let dir = workspace("legacy-snapshot-pump");
        let mut set = set();
        let tab = set.active();
        let mut backend = AgentBackend::Legacy(agent::AgentSession::for_tests());
        backend.send_turn("first", "first", PromptOrigin::Composed).unwrap();
        let revision = backend.projection().last_revision;
        backend.note_ui_snapshot(revision);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert!(
            set.pump_views(&dir, &|_| true).active_payload.is_none(),
            "a snapshot already drew the queued prompt and turn start"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn a_bound_tabs_events_go_to_its_view_and_it_is_never_stale() {
        let dir = workspace("views-bound-tab-events");
        let mut set = set();
        let base = set.active();
        let (base_provider, backend) = live(&dir);
        set.get_mut(base).unwrap().backend = TabBackend::Live(backend);
        let bound = set.open(AgentId::Claude);
        let (bound_provider, backend) = live(&dir);
        set.get_mut(bound).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(set.break_out(view(2)), Ok(bound));
        assert_eq!(set.active(), base);

        bound_provider.queue(started("v1"));
        bound_provider.queue(text_delta("v1", "from view two"));
        base_provider.queue(started("b1"));
        base_provider.queue(text_delta("b1", "from the base view"));
        let (mut base_text, mut bound_text) = (String::new(), String::new());
        until("both tabs' text", || {
            let out = set.pump(&dir, true);
            base_text += &events_text(out.active_payload.as_deref());
            for shown in &out.view_payloads {
                assert_eq!(
                    (shown.view, shown.tab),
                    (view(2), bound),
                    "only the bound tab, in its view"
                );
                bound_text += &events_text(Some(&shown.payload));
            }
            assert!(out.view_payloads.len() <= 1);
            assert!(!set.get(bound).unwrap().session.stale, "a bound tab is never stale");
            base_text.contains("from the base view") && bound_text.contains("from view two")
        });
        assert!(
            !base_text.contains("from view two"),
            "the base page gets only its own tab"
        );
        assert!(!bound_text.contains("from the base view"));

        // Its view mapped: the turn's end is seen as it happens.
        bound_provider.queue(completed("v1", agent::TurnOutcome::Completed));
        until("the bound tab's turn end", || {
            set.pump_views(&dir, &|_| true);
            !set.get(bound).unwrap().session.was_running
        });
        assert!(!set.get(bound).unwrap().attention.attention().unread);

        // Its view unmapped: the next turn's end is left unread, and the base page is still told
        // nothing of it; the tab stays current for its own page.
        bound_provider.queue(started("v2"));
        bound_provider.queue(text_delta("v2", "while hidden"));
        bound_provider.queue(completed("v2", agent::TurnOutcome::Completed));
        let mut hidden_text = String::new();
        until("the hidden turn's end", || {
            let out = set.pump_views(&dir, &|v| v.is_base());
            assert!(!events_text(out.active_payload.as_deref()).contains("while hidden"));
            for shown in &out.view_payloads {
                assert_eq!((shown.view, shown.tab), (view(2), bound));
                hidden_text += &events_text(Some(&shown.payload));
            }
            assert!(!set.get(bound).unwrap().session.stale, "a bound tab is never stale");
            set.get(bound).unwrap().attention.attention().unread
        });
        assert!(hidden_text.contains("while hidden"), "{hidden_text}");
        assert!(set.attention_of(view(2)).unread);
        assert!(!set.attention_of(ViewId::BASE).unread);
        assert!(set.attention().unread, "the window's sum carries it");

        // The bound tab's own state, for its page; nothing for a background base-view tab.
        let snapshot = the_snapshot(set.state_payloads(bound));
        assert!(snapshot_text(&snapshot).contains("while hidden"));
        let third = set.open(AgentId::Claude);
        set.select(base);
        assert!(set.state_payloads(third).is_empty(), "no page shows a background tab");
        assert!(set.state_payloads(TabId(999)).is_empty());
        shut_down_all(&mut set);
    }

    /// A bound tab's finished turn carries its duration on the envelope to its own page, and a resync
    /// sends that page the snapshot, never paced and never to the base page.
    #[test]
    fn a_bound_tabs_turn_times_and_resync_snapshot_go_to_its_view() {
        use crate::panel_cadence::EnvelopeClass::Immediate;
        let dir = workspace("views-bound-tab-resync");
        let mut set = set();
        let base = set.active();
        let bound = set.open(AgentId::Claude);
        let (provider, backend) = live(&dir);
        set.get_mut(bound).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(set.break_out(view(2)), Ok(bound));
        assert_eq!(set.active(), base);

        provider.queue(started("t1"));
        until("the bound tab's turn to start", || {
            set.pump_views(&dir, &|_| true);
            set.get(bound).unwrap().session.was_running
        });
        provider.queue(text_delta("t1", "done"));
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        let mut times = None;
        until("an events payload for view 2 carrying turnTimes", || {
            let out = set.pump_views(&dir, &|_| true);
            assert!(out.active_payload.is_none(), "the base page is told nothing of it");
            for shown in out.view_payloads {
                assert_eq!((shown.view, shown.tab), (view(2), bound));
                let value: serde_json::Value = serde_json::from_str(&shown.payload).unwrap();
                if value["kind"] == "events" && value.get("turnTimes").is_some() {
                    times = Some(value["turnTimes"].clone());
                }
            }
            times.is_some()
        });
        assert_eq!(times.unwrap()[0]["turnId"], "t1");
        assert_eq!(
            set.get(bound).unwrap().session.turn_times_unsent,
            0,
            "the envelope carried it"
        );

        // Without pumping in between: enough to overflow the sidecar's own UI queue.
        provider.queue(started("t2"));
        for i in 0..300 {
            provider.queue(text_delta("t2", &format!("chunk {i}")));
        }
        until("the whole stream folded", || {
            let backend = set.get(bound).unwrap().live().unwrap();
            let projection = backend.projection();
            projection.transcript.iter().any(|m| m.text.contains("chunk 299"))
        });
        let mut snapshot = None;
        until("a payload for view 2", || {
            let out = set.pump_views(&dir, &|_| true);
            assert!(out.active_payload.is_none(), "the base page is told nothing of it");
            if let Some(shown) = out.view_payloads.into_iter().next() {
                assert_eq!((shown.view, shown.tab), (view(2), bound));
                snapshot = Some((shown.payload, shown.class));
            }
            snapshot.is_some()
        });
        let (payload, class) = snapshot.unwrap();
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["kind"], "snapshot", "an overflow resyncs: {payload:.120}");
        assert_eq!(class, Immediate);
        assert_eq!(
            value["turnTimes"][0]["turnId"], "t1",
            "the snapshot carries the durations"
        );
        assert!(snapshot_text(&payload).contains("chunk 299"));
        assert!(!set.get(bound).unwrap().session.stale, "a bound tab is never stale");
        shut_down_all(&mut set);
    }

    #[test]
    fn the_base_page_lists_base_tabs_and_an_extra_page_its_own() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        let three = set.open(AgentId::Claude);
        set.select(two);
        assert_eq!(set.break_out(view(2)), Ok(two));
        assert_eq!(set.active(), three);
        let parse = |payload: String| -> (u64, Vec<u64>) {
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(value["kind"], "tabs");
            let numbers = value["tabs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["number"].as_u64().unwrap())
                .collect();
            (value["active"].as_u64().unwrap(), numbers)
        };
        assert_eq!(
            parse(set.tabs_payload_for(ViewId::BASE)),
            (three.0, vec![1, 3]),
            "the bound tab's number is a gap"
        );
        assert_eq!(set.tabs_payload(), set.tabs_payload_for(ViewId::BASE));
        assert_eq!(
            parse(set.tabs_payload_for(view(2))),
            (two.0, vec![2]),
            "its one tab, as active"
        );
        assert_eq!(
            parse(set.tabs_payload_for(view(3))),
            (0, vec![]),
            "a view with no tab names none"
        );
        assert_eq!(set.get(one).unwrap().number, 1);
    }

    #[test]
    fn mark_seen_marks_the_tab_it_names() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        assert_eq!(set.break_out(view(2)), Ok(two));
        let ended = [completed("t", agent::TurnOutcome::Completed)];
        set.get_mut(one).unwrap().attention.observe(&ended, false);
        set.get_mut(two).unwrap().attention.observe(&ended, false);
        set.mark_seen(two);
        assert!(!set.get(two).unwrap().attention.attention().unread, "the tab it names");
        assert!(
            set.get(one).unwrap().attention.attention().unread,
            "the active tab is not it"
        );
        set.mark_seen(TabId(999));
        set.mark_seen(one);
        assert!(!set.attention().unread);
    }

    #[test]
    fn attention_of_a_view_is_its_own_tabs() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        let three = set.open(AgentId::Claude);
        let four = set.open(AgentId::Claude);
        assert_eq!(set.break_out(view(3)), Ok(four));
        set.select(two);
        assert_eq!(set.break_out(view(2)), Ok(two));
        assert_eq!(set.base_tabs(), vec![one, three]);
        set.get_mut(two).unwrap().attention.observe(&[write("oldest")], true);
        set.get_mut(one).unwrap().attention.observe(&[write("a")], false);
        set.get_mut(three).unwrap().attention.observe(&[write("b")], false);
        set.get_mut(three)
            .unwrap()
            .attention
            .observe(&[completed("t", agent::TurnOutcome::Completed)], false);

        let base = set.attention_of(ViewId::BASE);
        assert_eq!((base.pending, base.unread, base.arrived), (2, true, 2));
        let bound = set.attention_of(view(2));
        assert_eq!((bound.pending, bound.unread, bound.arrived), (1, false, 1));
        assert_eq!(set.attention_of(view(3)).pending, 0);
        assert_eq!(set.attention_of(view(4)), Attention::default(), "a view with no tab");
        assert_eq!(set.attention().pending, 3, "the window sums every view");
        assert_eq!(set.oldest_card_tab(), Some(two));
        assert_eq!(set.oldest_card_place(), Some(TabPlace::View(view(2), two)));
        assert_eq!(set.newest_card_tab_in(ViewId::BASE), Some(three));
        assert_eq!(set.newest_card_tab_in(view(2)), Some(two));
        assert_eq!(set.newest_card_tab_in(view(3)), None);
        let each = set.tab_attentions();
        let of = |tab: TabId| each.iter().find(|t| t.tab == tab).copied().unwrap();
        assert_eq!((of(two).view, of(two).attention.pending), (view(2), 1));
        assert_eq!(
            (of(four).view, of(four).attention.pending, of(four).newest_card),
            (view(3), 0, None)
        );
        assert!(of(three).attention.unread);

        // A closed tab's cards stay in the window's `arrived` only.
        set.remove(one).unwrap();
        assert_eq!(set.attention().arrived, 3);
        assert_eq!(set.attention_of(ViewId::BASE).arrived, 1);
        set.remove(two).unwrap();
        assert_eq!(set.oldest_card_place(), Some(TabPlace::Base(three)));
    }

    /// A restore waiting for its trust answer is written as it stands, and a crash meanwhile leaves that file to the next
    /// start, which still has the cut turn to tell of.
    #[test]
    fn a_restore_waiting_for_its_answer_goes_on_saying_a_turn_never_finished() {
        let mut set = set();
        let from = set.active();
        let mut cut = planned("a", SessionModeChoice::Auto);
        cut.turn_running = true;
        let plan = plan_of(vec![cut, planned("b", SessionModeChoice::Auto)], Some("a"));
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("nothing to ask")
        };
        set.defer_start(from, DeferredStart::Restore { go }).unwrap();

        let saved = set.saved_snapshot("conv").unwrap();
        let running: Vec<(&str, bool)> = saved
            .tabs
            .iter()
            .map(|t| (t.provider_session_id.as_str(), t.turn_running))
            .collect();
        assert_eq!(running, [("a", true), ("b", false)]);
    }

    #[test]
    fn a_saved_snapshot_writes_each_bound_tabs_view() {
        let mut set = set();
        let failed_with = |set: &mut TabSet, tab: TabId, session: &str| {
            let t = set.get_mut(tab).unwrap();
            t.backend = TabBackend::Failed {
                reason: "refused".to_string(),
            };
            t.failed_resume = Some(session.to_string());
        };
        let one = set.active();
        failed_with(&mut set, one, "s-1");
        let two = set.open(AgentId::Claude);
        failed_with(&mut set, two, "s-2");
        let three = set.open(AgentId::Claude);
        failed_with(&mut set, three, "s-3");
        let codex = set.open(AgentId::Codex);
        failed_with(&mut set, codex, "thread-1");
        let other_codex = set.open(AgentId::Codex);
        failed_with(&mut set, other_codex, "thread-2");
        assert_eq!(set.break_out(view(4)), Ok(other_codex));
        set.select(two);
        assert_eq!(set.break_out(view(3)), Ok(two));
        set.select(three);

        let saved = set.saved_snapshot("conv").unwrap();
        let claude: Vec<(&str, Option<u8>)> = saved
            .claude_tabs()
            .map(|t| (t.provider_session_id.as_str(), t.view))
            .collect();
        assert_eq!(claude, [("s-1", None), ("s-2", Some(3)), ("s-3", None)]);
        assert_eq!(saved.active, Some(2), "the base view's active tab, as before");
        let external: Vec<(&str, Option<u8>)> = saved
            .other_tabs()
            .map(|t| (t.provider_session_id.as_str(), t.view))
            .collect();
        assert_eq!(external, [("thread-1", None), ("thread-2", Some(4))]);

        // A restore waiting for its trust answer keeps the views it was saved with meanwhile.
        let mut set = super::tests::set();
        let from = set.active();
        let mut a = planned("a", SessionModeChoice::Auto);
        a.view = ViewId::new(2);
        let plan = plan_of(vec![a, planned("b", SessionModeChoice::Auto)], None);
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("nothing to ask")
        };
        set.defer_start(from, DeferredStart::Restore { go }).unwrap();
        let saved = set.saved_snapshot("conv").unwrap();
        let waiting: Vec<(&str, Option<u8>)> = saved
            .tabs
            .iter()
            .map(|t| (t.provider_session_id.as_str(), t.view))
            .collect();
        assert_eq!(waiting, [("a", Some(2)), ("b", None)]);
    }

    fn planned_in(id: &str, view: u8) -> PlannedTab {
        PlannedTab {
            view: ViewId::new(view),
            turn_running: false,
            ..planned(id, SessionModeChoice::Auto)
        }
    }

    fn restore_into(set: &mut TabSet, items: Vec<PlannedTab>, active: Option<&str>) -> (TabId, RestoreRun) {
        let from = set.active();
        let RestoreStep::Go(go) = set.begin_restore(plan_of(items, active), BypassPolicy::Ask) else {
            panic!("nothing to ask")
        };
        let run = set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
        assert!(set.v1_holds());
        (from, run)
    }

    fn tab_of(set: &TabSet, session: &str) -> TabId {
        set.tabs()
            .iter()
            .find(|t| t.provider_session_id().as_deref() == Some(session))
            .map(|t| t.id)
            .expect("the session has a tab")
    }

    #[test]
    fn restored_tabs_go_to_their_views_once_and_otherwise_to_the_base_view() {
        let mut set = set();
        set.set_layout_views(vec![view(3), ViewId::BASE, view(2), view(3)]);
        let (from, run) = restore_into(
            &mut set,
            vec![planned_in("a", 2), planned_in("b", 2), planned_in("c", 4)],
            Some("a"),
        );
        let (a, b, c) = (tab_of(&set, "a"), tab_of(&set, "b"), tab_of(&set, "c"));
        assert_eq!(set.view_of(a), view(2), "the first tab naming agent#2 gets it");
        assert_ne!(
            a, from,
            "a tab bound to a view is a new tab, never the one the restore began in"
        );
        assert_eq!(set.view_of(b), ViewId::BASE, "agent#2 is taken by then");
        assert_eq!(b, from, "the first base-view tab goes into the empty tab");
        assert_eq!(set.view_of(c), ViewId::BASE, "the layout holds no agent#4");
        assert_eq!(set.tab_in(view(3)), None, "nothing was saved in agent#3");
        assert_eq!(
            set.active(),
            b,
            "the saved active tab is bound, so the base view shows its first restored tab"
        );
        assert!(set.view_of(set.active()).is_base());
        assert_eq!(set.base_tabs(), vec![b, c]);
        assert!(run.owns(a) && run.owns(b) && run.owns(c));
        assert!(
            run.began_in(from) && run.back_to(a).is_none(),
            "a failed bound tab goes away"
        );

        // Not told which views the layout holds: every tab comes back in the base view.
        let mut set = super::tests::set();
        let (from, _run) = restore_into(&mut set, vec![planned_in("a", 2), planned_in("b", 3)], Some("b"));
        assert!(set.views().is_empty());
        assert_eq!(tab_of(&set, "a"), from);
        assert_eq!(
            set.active(),
            tab_of(&set, "b"),
            "the saved active tab, in the base view"
        );

        // A view already showing a tab is not given a second one.
        let mut set = super::tests::set();
        let _one = set.active();
        let held = set.open(AgentId::Claude);
        assert_eq!(set.break_out(view(2)), Ok(held));
        set.set_layout_views(vec![view(2)]);
        let (_from, _run) = restore_into(&mut set, vec![planned_in("a", 2)], Some("a"));
        assert_eq!(set.tab_in(view(2)), Some(held));
        assert_eq!(set.view_of(tab_of(&set, "a")), ViewId::BASE);
        assert_eq!(set.active(), tab_of(&set, "a"));
    }

    /// The tab the screen falls back to when the restored active tab fails is a base-view tab: one that
    /// came back bound to an extra view cannot be the base view's active tab.
    #[test]
    fn a_failed_restored_active_tab_falls_back_to_a_base_view_tab() {
        let mut set = set();
        set.set_layout_views(vec![view(2)]);
        let (from, mut run) = restore_into(
            &mut set,
            vec![
                planned_in("a", 2),
                planned("b", SessionModeChoice::Auto),
                planned("c", SessionModeChoice::Auto),
            ],
            Some("b"),
        );
        let (a, b, c) = (tab_of(&set, "a"), tab_of(&set, "b"), tab_of(&set, "c"));
        assert_eq!(set.view_of(a), view(2));
        assert_eq!((b, set.active()), (from, from));
        assert_eq!(run.first_survivor(), Some(b), "the bound tab is no place to go back to");

        let back_to = run.back_to(b);
        run.note_failed(b, "refused");
        set.get_mut(b).unwrap().backend = TabBackend::Failed {
            reason: "refused".to_string(),
        };
        set.discard_failed_restore(b, back_to);
        assert_eq!(run.first_survivor(), Some(c));
        assert!(set.select(c), "the fallback is a tab the base view can show");
        assert_eq!(set.active(), c);
        assert_eq!(set.view_of(a), view(2));
    }

    /// A put-off restore keeps its sessions as failed tabs, each in the view it was saved in under the
    /// rule a started restore follows, so the next write of the file still names those views.
    #[test]
    fn a_put_off_restore_keeps_each_tabs_saved_view() {
        let mut set = set();
        set.set_layout_views(vec![view(2)]);
        let from = set.active();
        let RestoreStep::Go(go) = set.begin_restore(
            plan_of(
                vec![
                    planned_in("a", 2),
                    planned("b", SessionModeChoice::Auto),
                    planned_in("c", 2),
                ],
                Some("a"),
            ),
            BypassPolicy::Ask,
        ) else {
            panic!("nothing to ask")
        };
        assert!(!set.keep_put_off_restore(from, go, "put off"));
        assert!(set.v1_holds());
        let kept = |set: &TabSet, session: &str| {
            set.tabs()
                .iter()
                .find(|t| t.failed_resume.as_deref() == Some(session))
                .map(|t| t.id)
                .expect("the session is kept in a failed tab")
        };
        let (a, b, c) = (kept(&set, "a"), kept(&set, "b"), kept(&set, "c"));
        assert_eq!(set.view_of(a), view(2));
        assert_ne!(a, from);
        assert_eq!(
            b, from,
            "the first base-view tab goes into the tab the restore began in"
        );
        assert_eq!(set.view_of(c), ViewId::BASE, "agent#2 is taken by then");
        assert_eq!(
            set.active(),
            b,
            "the saved active tab is bound, so the base view shows its first kept tab"
        );
        assert!(matches!(set.get(a).unwrap().backend, TabBackend::Failed { .. }));
        let saved = set.saved_snapshot("conv").unwrap();
        let mut views: Vec<(&str, Option<u8>)> = saved
            .tabs
            .iter()
            .map(|t| (t.provider_session_id.as_str(), t.view))
            .collect();
        views.sort();
        assert_eq!(views, [("a", Some(2)), ("b", None), ("c", None)]);

        // Not told which views the layout holds: every kept tab is in the base view.
        let mut set = super::tests::set();
        let from = set.active();
        let RestoreStep::Go(go) = set.begin_restore(plan_of(vec![planned_in("a", 2)], Some("a")), BypassPolicy::Ask)
        else {
            panic!("nothing to ask")
        };
        set.keep_put_off_restore(from, go, "put off");
        assert_eq!(set.get(from).unwrap().failed_resume.as_deref(), Some("a"));
        assert!(set.views().is_empty());
    }

    /// The tab half of the layout's invariant run: random opens, breaks, joins, opens in a view,
    /// closes and selections over empty tabs, checking after every one that the base view's active
    /// tab is a base-view tab, the base view holds a tab, an extra view holds at most one, and
    /// `shown()` lists each shown tab once.
    #[test]
    fn views_invariant_run() {
        struct XorShift(u64);
        impl XorShift {
            fn next(&mut self) -> u64 {
                let mut x = self.0;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.0 = x;
                x
            }
            fn below(&mut self, n: usize) -> usize {
                (self.next() % n as u64) as usize
            }
        }

        fn check(set: &TabSet, step: usize, op: &str) {
            let active = set.active();
            let at = |what: &str| format!("step {step} ({op}): {what}");
            assert!(
                set.get(active).is_some_and(|t| t.view_id().is_base()),
                "{}",
                at("active is a base-view tab")
            );
            assert!(!set.base_tabs().is_empty(), "{}", at("the base view holds a tab"));
            let mut per_view = std::collections::BTreeMap::<ViewId, usize>::new();
            for t in set.tabs().iter().filter(|t| !t.view_id().is_base()) {
                *per_view.entry(t.view_id()).or_default() += 1;
            }
            assert!(per_view.values().all(|n| *n == 1), "{}", at("one tab per extra view"));
            let shown = set.shown();
            assert_eq!(shown.first(), Some(&(ViewId::BASE, active)), "{}", at("active first"));
            assert_eq!(
                shown.iter().filter(|(_, t)| *t == active).count(),
                1,
                "{}",
                at("active once")
            );
            for t in set.tabs().iter().filter(|t| !t.view_id().is_base()) {
                assert_eq!(
                    shown.iter().filter(|s| **s == (t.view_id(), t.id)).count(),
                    1,
                    "{}",
                    at("every bound tab once")
                );
            }
            assert_eq!(shown.len(), 1 + per_view.len(), "{}", at("nothing else shown"));
            assert!(set.v1_holds(), "{}", at("v1"));
        }

        let mut rng = XorShift(0x2f2f_2f2f);
        let mut set = set();
        let pick_tab = |set: &TabSet, rng: &mut XorShift| set.tabs()[rng.below(set.tabs().len())].id;
        let pick_view = |rng: &mut XorShift| ViewId::new(2 + rng.below(8) as u8).unwrap();
        let mut base_kills = 0;
        for step in 0..5_000 {
            let op = if rng.below(20) == 0 { 10 } else { rng.below(10) };
            let name = match op {
                0 => {
                    if set.tabs().len() < 14 {
                        set.open(AgentId::Claude);
                    }
                    "open"
                }
                1 => {
                    let free: Vec<ViewId> = (2..=9u8)
                        .filter_map(ViewId::new)
                        .filter(|v| set.tab_in(*v).is_none())
                        .collect();
                    if !free.is_empty() {
                        let v = free[rng.below(free.len())];
                        let before = set.active();
                        match set.break_out(v) {
                            Ok(moved) => assert_eq!(moved, before),
                            Err(e) => assert_eq!(e, ViewError::OneTabLeft),
                        }
                    }
                    "break_out"
                }
                2 => {
                    let v = pick_view(&mut rng);
                    let held = set.tab_in(v);
                    assert_eq!(set.join_back(v), held);
                    if let Some(tab) = held {
                        assert_eq!(set.active(), tab);
                    }
                    "join_back"
                }
                3 => {
                    let v = pick_view(&mut rng);
                    let before = set.active();
                    if set.tabs().len() < 14 {
                        let taken = set.tab_in(v).is_some();
                        match set.open_in(AgentId::Claude, v) {
                            Ok(tab) => {
                                assert!(!taken);
                                assert_eq!(set.tab_in(v), Some(tab));
                            }
                            Err(e) => assert_eq!(e, ViewError::Taken(v)),
                        }
                    }
                    assert_eq!(set.active(), before, "open_in never moves active");
                    "open_in"
                }
                4 => {
                    let tab = pick_tab(&set, &mut rng);
                    let its_view = set.view_of(tab);
                    let views: Vec<ViewId> = set.views();
                    set.remove(tab).unwrap();
                    if !its_view.is_base() {
                        assert_eq!(set.tab_in(its_view), None, "its view is left empty");
                    } else {
                        assert_eq!(set.views(), views, "closing a base-view tab leaves the views");
                    }
                    "remove"
                }
                5 => {
                    let tab = pick_tab(&set, &mut rng);
                    let base = set.view_of(tab).is_base();
                    let before = set.active();
                    assert_eq!(set.select(tab), base);
                    assert_eq!(set.active(), if base { tab } else { before });
                    "select"
                }
                6 => {
                    let n = 1 + rng.below(16) as u16;
                    let before = set.active();
                    match set.select_number(n) {
                        Some(TabPlace::Base(tab)) => assert_eq!(set.active(), tab),
                        Some(TabPlace::View(v, tab)) => {
                            assert_eq!(set.tab_in(v), Some(tab));
                            assert_eq!(set.active(), before);
                        }
                        None => assert_eq!(set.active(), before),
                    }
                    "select_number"
                }
                7 => {
                    set.step(if rng.below(2) == 0 { 1 } else { -1 });
                    "step"
                }
                8 => {
                    set.select_last();
                    "select_last"
                }
                9 => {
                    let tab = pick_tab(&set, &mut rng);
                    let before = set.active();
                    match set.select_place(tab) {
                        Some(TabPlace::Base(id)) => assert_eq!(set.active(), id),
                        Some(TabPlace::View(_, _)) => assert_eq!(set.active(), before),
                        None => panic!("the tab exists"),
                    }
                    "select_place"
                }
                _ => {
                    // Every base-view tab closed in turn while the views are live: a fresh base
                    // tab opens, and the views keep theirs.
                    let views: Vec<(ViewId, TabId)> = set.shown().into_iter().skip(1).collect();
                    if !views.is_empty() {
                        base_kills += 1;
                    }
                    for tab in set.base_tabs() {
                        set.remove(tab).unwrap();
                        check(&set, step, "a base-view kill");
                    }
                    assert_eq!(set.shown().into_iter().skip(1).collect::<Vec<_>>(), views);
                    assert_eq!(set.base_tabs().len(), 1, "one fresh base tab");
                    "kill the base view"
                }
            };
            check(&set, step, name);
        }
        assert!(base_kills > 0, "the run killed the base view while views were live");
    }
}

/// Tabs running another agent than Claude: the reviewer each turn carries, how a mode change reaches
/// the answers and the turns, what is saved and how it comes back, and the presence of a closed tab.
#[cfg(test)]
mod external_tab_tests {
    use super::*;
    use crate::tab_restore::PlannedTab;
    use crate::test_providers::{empty_trust_plan, ExternalRecordingProvider};
    use agent::{Reviewer, TurnPolicy};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// The other agents' tabs of a saved set, in order.
    fn others(saved: &SavedTabs) -> Vec<&SavedTab> {
        saved.other_tabs().collect()
    }

    /// Where the tab that comes back on screen stands among the other agents' tabs, when it is one.
    fn others_active(saved: &SavedTabs) -> Option<usize> {
        let index = saved.active.filter(|i| saved.tabs[*i].agent.is_external())?;
        Some(index - saved.claude_tabs().count())
    }

    fn workspace(label: &str) -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        dir
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A window with one Codex tab, live on a scripted provider, in `mode`.
    fn codex_tab(dir: &Path, mode: SessionModeChoice) -> (TabSet, TabId, Arc<ExternalRecordingProvider>) {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.open(AgentId::Codex);
        let provider = Arc::new(ExternalRecordingProvider::codex());
        let backend =
            AgentBackend::start_external_with(AgentId::Codex, provider.clone(), dir, empty_trust_plan(), mode, None)
                .unwrap();
        let t = set.get_mut(tab).unwrap();
        t.mode = mode;
        t.backend = TabBackend::Live(backend);
        (set, tab, provider)
    }

    fn turn_completed(turn: &str) -> AgentDomainEvent {
        AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        }
    }

    fn command(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: Some(format!("item-{id}")),
            tool_name: "commandExecution".into(),
            input: serde_json::json!({ "command": "cargo publish" }),
            provider_prompt: None,
        }
    }

    fn send(set: &mut TabSet, tab: TabId, text: &str) {
        match set.send_now(tab, text, text.to_string(), 0).unwrap() {
            SendNow::Flushed(Some(Flush { outcome: Ok(_), .. })) => {}
            SendNow::Flushed(Some(Flush { outcome: Err(e), .. })) => panic!("refused: {}", e.message),
            _ => panic!("nothing was sent"),
        }
    }

    fn reviewer(r: Reviewer) -> TurnPolicy {
        TurnPolicy {
            reviewer: r,
            sandbox: agent::Sandbox::Project,
        }
    }

    fn background_count(set: &TabSet, tab: TabId) -> u32 {
        set.get(tab).unwrap().live().unwrap().projection().background_calls
    }

    /// The turn ids the tab set has reported as begun since the last call.
    fn began(set: &mut TabSet) -> Vec<String> {
        set.take_client_news()
            .into_iter()
            .filter_map(|(_, news)| match news {
                ClientCallNews::TurnBegan { turn_id } => Some(turn_id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_external_tab_reports_its_turns() {
        let dir = workspace("codex-turn-news");
        let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Auto);
        let mut told = Vec::new();
        for turn in ["turn-1", "turn-2"] {
            provider
                .inner
                .queue(AgentDomainEvent::TurnStarted { turn_id: turn.into() });
            until("the turn to be reported", || {
                set.pump(&dir, true);
                told.extend(began(&mut set));
                told.last().is_some_and(|last| last == turn)
            });
            provider.inner.queue(turn_completed(turn));
            until("the turn to end", || {
                set.pump(&dir, true);
                set.get(tab)
                    .unwrap()
                    .live()
                    .unwrap()
                    .projection()
                    .active_turn_id
                    .is_none()
            });
            told.extend(began(&mut set));
        }
        assert_eq!(
            told,
            ["turn-1", "turn-2"],
            "one report for each turn, none for an ending"
        );
        for taken in set.take_all() {
            if let Some(retired) = taken.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn a_codex_tab_with_commands_left_running_asks() {
        let dir = workspace("codex-background-close");
        let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Auto);
        provider.inner.queue(AgentDomainEvent::BackgroundCalls { running: 2 });
        until("background commands", || background_count(&set, tab) == 2);
        assert!(set.close_needs_confirm(tab));
        assert_eq!(set.background_count(), 1);
        assert_eq!(
            tabs::close_prompt(&set.close_facts(tab).unwrap())[1],
            "2 commands running in the background"
        );
        for taken in set.take_all() {
            if let Some(retired) = taken.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// The confirmed stop reaches the conversation of the tab it names, and only when the count it was
    /// asked about is the tab's count and no turn runs. Every refusal is benign and leaves the session
    /// live; a tab with no session, and one that is gone, stop nothing.
    #[test]
    fn stopping_the_background_reaches_the_named_tab_and_a_refusal_leaves_it_alive() {
        let dir = workspace("external-stop-background");
        let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Auto);
        let idle = set.open(AgentId::Codex);
        let refused = set.stop_background(tab, 1).unwrap_err();
        assert!(refused.benign);
        assert_eq!(refused.message, NOTHING_IN_THE_BACKGROUND);

        provider.inner.queue(AgentDomainEvent::BackgroundCalls { running: 2 });
        until("the count is folded", || background_count(&set, tab) == 2);
        let refused = set.stop_background(tab, 1).unwrap_err();
        assert!(refused.benign);
        assert!(
            refused
                .message
                .starts_with("2 commands now running in the background, not 1"),
            "{}",
            refused.message
        );
        let refused = set.stop_background(idle, 2).unwrap_err();
        assert!(refused.benign);
        assert_eq!(refused.message, "no active session");
        assert!(set.stop_background(TabId(9_999), 2).unwrap_err().benign);

        provider.inner.queue(AgentDomainEvent::TurnStarted {
            turn_id: "turn-1".into(),
        });
        until("the turn is folded", || {
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .active_turn_id
                .is_some()
        });
        let refused = set.stop_background(tab, 2).unwrap_err();
        assert!(refused.benign);
        assert!(refused.message.starts_with("a turn is running"), "{}", refused.message);
        provider.inner.queue(turn_completed("turn-1"));
        until("the turn ends", || {
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .active_turn_id
                .is_none()
        });

        // The agent refused it after all (the commands ended a moment ago): said, and nothing else.
        provider.refuse_next_stop(agent::ProviderErrorCode::NoActiveTurn);
        let refused = set.stop_background(tab, 2).unwrap_err();
        assert!(refused.benign, "{}", refused.message);
        assert!(set.get(tab).unwrap().live().is_some());
        assert_eq!(provider.stops(), 0, "nothing reached the agent's stop yet");

        set.stop_background(tab, 2).unwrap();
        assert_eq!(provider.stops(), 1);
        until("the count drops", || background_count(&set, tab) == 0);
    }

    /// The window's root, as the gate is given it at every pump, is what a turn is judged by: past a start, a
    /// conversation's own path is what a replaced folder resolved to.
    #[test]
    fn a_turn_is_judged_by_the_root_the_window_holds() {
        let target = workspace("external-window-root-target");
        let window_root = target.with_file_name(format!("{}-link", target.file_name().unwrap().to_string_lossy()));
        std::os::unix::fs::symlink(&target, &window_root).unwrap();
        let (mut set, tab, provider) = codex_tab(&window_root, SessionModeChoice::Auto);
        set.pump(&window_root, true);
        send(&mut set, tab, "first");
        assert_eq!(
            provider.policies(),
            vec![TurnPolicy {
                reviewer: Reviewer::User,
                sandbox: agent::Sandbox::ReadOnly,
            }]
        );
    }

    #[test]
    fn send_turn_carries_reviewer_from_mode() {
        let dir = workspace("external-reviewer-from-mode");
        let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Auto);
        assert_eq!(set.get(tab).unwrap().agent(), &AgentId::Codex);
        send(&mut set, tab, "first");
        provider.inner.queue(AgentDomainEvent::TurnStarted {
            turn_id: "turn-1".into(),
        });
        provider.inner.queue(turn_completed("turn-1"));
        until("the first turn ends", || set.pump(&dir, true).turn_ended.contains(&tab));
        // Bypass is entered as always: asked first, then confirmed.
        let plan = match set.cycle_mode(tab, ViewId::BASE) {
            Ok(ModeCycle::Confirm(plan)) => plan,
            other => panic!("expected a question, got {other:?}"),
        };
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
        send(&mut set, tab, "second");
        assert_eq!(
            provider.policies(),
            vec![reviewer(Reviewer::Agent), reviewer(Reviewer::User)],
            "Auto's turn is reviewed by the agent, Bypass's reaches Eitri"
        );
        for t in set.take_all() {
            if let Some(retired) = t.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn mode_switch_mid_turn_changes_answers_now_and_reviewer_next_turn() {
        let dir = workspace("external-mid-turn-switch");
        let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Auto);
        send(&mut set, tab, "first");
        provider.inner.queue(AgentDomainEvent::TurnStarted {
            turn_id: "turn-1".into(),
        });
        provider.inner.queue(command("p1"));
        until("the Auto card is waiting", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 1
        });
        assert!(
            provider.inner.decisions().is_empty(),
            "Auto drew a card and answered nothing"
        );

        // Entering bypass mid-turn: the waiting card is approved, and what arrives next is allowed
        // at once, while the running turn keeps the reviewer it started with.
        let plan = match set.cycle_mode(tab, ViewId::BASE) {
            Ok(ModeCycle::Confirm(plan)) => plan,
            other => panic!("expected a question, got {other:?}"),
        };
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
        provider.inner.queue(command("p2"));
        until("both answered", || {
            set.pump(&dir, true);
            provider.inner.decisions().len() == 2
        });
        assert_eq!(
            provider.inner.decisions(),
            vec![
                ("p1".to_string(), agent::PermissionDecision::Allow),
                ("p2".to_string(), agent::PermissionDecision::Allow)
            ]
        );
        assert_eq!(provider.policies(), vec![reviewer(Reviewer::Agent)], "no new turn yet");

        // Out of bypass before the turn ends (to manual): the next request is a card again, at once.
        assert_eq!(
            set.cycle_mode(tab, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Manual))
        );
        provider.inner.queue(command("p3"));
        until("the card after leaving bypass", || {
            set.pump(&dir, true);
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .pending_permissions
                .contains_key("p3")
        });
        set.pump(&dir, true);
        assert_eq!(provider.inner.decisions().len(), 2, "p3 waits for the user");
        // The provider reports each answer's resolution, and the user's interrupt ends p3.
        for (id, outcome) in [
            ("p1", agent::PermissionOutcome::Allowed),
            ("p2", agent::PermissionOutcome::Allowed),
            ("p3", agent::PermissionOutcome::CancelledByInterrupt),
        ] {
            provider.inner.queue(AgentDomainEvent::PermissionResolved {
                permission_id: id.into(),
                outcome,
            });
        }
        provider.inner.queue(turn_completed("turn-1"));
        until("the turn ends", || set.pump(&dir, true).turn_ended.contains(&tab));
        // The next turn carries the mode the tab is in now: manual hands every review to the user.
        send(&mut set, tab, "second");
        assert_eq!(
            provider.policies(),
            vec![reviewer(Reviewer::Agent), reviewer(Reviewer::User)]
        );
        for t in set.take_all() {
            if let Some(retired) = t.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// A Codex tab in manual: its turns hand every review to the user (reviewer `user`), and every
    /// request that reaches Eitri is a card, none answered by Eitri.
    #[test]
    fn a_manual_codex_tab_cards_every_request_and_reviews_nothing_itself() {
        let dir = workspace("external-manual");
        let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Manual);
        send(&mut set, tab, "first");
        provider.inner.queue(AgentDomainEvent::TurnStarted {
            turn_id: "turn-1".into(),
        });
        provider.inner.queue(command("p1"));
        provider.inner.queue(command("p2"));
        until("both cards are waiting", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 2
        });
        set.pump(&dir, true);
        assert!(provider.inner.decisions().is_empty(), "manual answers nothing itself");
        assert_eq!(provider.policies(), vec![reviewer(Reviewer::User)]);
        // Manual -> auto moves at once; the reviewer follows at the next turn.
        assert_eq!(
            set.cycle_mode(tab, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Auto))
        );
        for id in ["p1", "p2"] {
            provider.inner.queue(AgentDomainEvent::PermissionResolved {
                permission_id: id.into(),
                outcome: agent::PermissionOutcome::CancelledByInterrupt,
            });
        }
        provider.inner.queue(turn_completed("turn-1"));
        until("the turn ends", || set.pump(&dir, true).turn_ended.contains(&tab));
        send(&mut set, tab, "second");
        assert_eq!(
            provider.policies(),
            vec![reviewer(Reviewer::User), reviewer(Reviewer::Agent)]
        );
        for t in set.take_all() {
            if let Some(retired) = t.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// An ACP tab saved in manual (a hand-edited file: no ACP tab is ever in manual) comes back in
    /// auto, a mode its agent offers.
    #[test]
    fn an_acp_tab_saved_in_manual_comes_back_in_auto() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let plan = RestorePlan {
            items: vec![planned(AgentId::Acp("gem".into()), "g", SessionModeChoice::Manual)],
            skipped: Vec::new(),
            active_session: None,
        };
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("nothing to ask")
        };
        let from = set.active();
        set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
        assert_eq!(set.get(from).unwrap().mode(), SessionModeChoice::Auto);
        assert_eq!(set.get(from).unwrap().agent(), &AgentId::Acp("gem".into()));
        let note = set
            .take_mode_note()
            .expect("the restore says the tab is not in its saved mode");
        let number = set.get(from).unwrap().number;
        assert!(note.starts_with("gem tabs have no manual mode"), "{note}");
        assert!(note.ends_with(&format!("tab {number} is in auto")), "{note}");
        assert_eq!(set.take_mode_note(), None, "once");
    }

    /// Under a manual window default, a restore whose first tab is an ACP agent's brings the next tabs back
    /// into new tabs of their own agents, in their saved modes, and says nothing: no tab was kept from the
    /// mode it was to have (the new tab is opened for the ACP agent of the tab before it, and only then
    /// given its own agent and mode).
    #[test]
    fn a_restore_after_an_acp_tab_names_no_tab_it_did_not_clamp() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
        assert_eq!(set.take_mode_note(), None);
        let plan = RestorePlan {
            items: vec![
                planned(AgentId::Acp("gem".into()), "g", SessionModeChoice::Auto),
                planned(AgentId::Claude, "c", SessionModeChoice::Manual),
                planned(AgentId::Codex, "x", SessionModeChoice::Manual),
            ],
            skipped: Vec::new(),
            active_session: None,
        };
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("nothing to ask")
        };
        let from = set.active();
        set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
        let of = |id: &str| {
            set.tabs()
                .iter()
                .find(|t| matches!(&t.backend, TabBackend::Starting(p) if p.resume.as_deref() == Some(id)))
                .map(|t| (t.agent().clone(), t.mode()))
                .expect("a tab for the session")
        };
        assert_eq!(of("g"), (AgentId::Acp("gem".into()), SessionModeChoice::Auto));
        assert_eq!(of("c"), (AgentId::Claude, SessionModeChoice::Manual));
        assert_eq!(of("x"), (AgentId::Codex, SessionModeChoice::Manual));
        assert_eq!(set.take_mode_note(), None, "no tab was kept from its mode");
    }

    /// A session that turns out to be an ACP agent's when its backend arrives, in a tab that was in manual,
    /// takes auto, and the page is told which tab.
    #[test]
    fn a_backend_arriving_in_a_mode_its_agent_lacks_is_said() {
        let dir = workspace("arrival-clamp");
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
        let tab = set.active();
        assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Manual);
        let agent = AgentId::Acp("gem".into());
        let provider = Arc::new(ExternalRecordingProvider::named("acp:gem", false));
        let backend = AgentBackend::start_external_with(
            agent.clone(),
            provider.clone(),
            &dir,
            empty_trust_plan(),
            SessionModeChoice::Auto,
            None,
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(backend)).unwrap();
        set.get_mut(tab).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "r1".into(),
            result_rx: rx,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        });
        set.collect_starts();
        assert_eq!(set.get(tab).unwrap().agent(), &agent);
        assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Auto);
        let note = set.take_mode_note().expect("the page is told");
        let number = set.get(tab).unwrap().number;
        assert!(note.starts_with("gem tabs have no manual mode"), "{note}");
        assert!(note.ends_with(&format!("tab {number} is in auto")), "{note}");
        for t in set.take_all() {
            if let Some(retired) = t.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    fn planned(agent: AgentId, id: &str, mode: SessionModeChoice) -> PlannedTab {
        PlannedTab {
            agent,
            provider_session_id: id.to_string(),
            name: None,
            title: Some(format!("title of {id}")),
            saved_mode: mode,
            label: format!("title of {id}"),
            view: None,
            turn_running: false,
        }
    }

    #[test]
    fn restore_asks_bypass_then_trust_then_starts_for_external_tabs() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let from = set.active();
        let plan = RestorePlan {
            items: vec![
                planned(AgentId::Claude, "claude-a", SessionModeChoice::Auto),
                planned(AgentId::Codex, "codex-b", SessionModeChoice::Bypass),
            ],
            skipped: Vec::new(),
            active_session: Some("codex-b".into()),
        };
        // 1. The bypass question, since a Codex tab was saved in bypass.
        let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("a saved bypass tab is asked about first")
        };
        let go = set.answer_restore(prompt.nonce, true).unwrap();
        assert_eq!(
            go.sessions(),
            vec![
                (AgentId::Claude, "claude-a".to_string()),
                (AgentId::Codex, "codex-b".to_string())
            ]
        );
        // 2. The trust question, before anything is started.
        set.defer_start(from, DeferredStart::Restore { go }).unwrap();
        let mut asked: Vec<(AgentId, String)> = Vec::new();
        assert!(set.tabs().iter().all(|t| !matches!(t.backend, TabBackend::Starting(_))));
        let nonce = set.ask_trust(from).unwrap();
        set.mark_recording(from, nonce, ViewId::BASE).unwrap();
        let Some(DeferredStart::Restore { go }) = set.take_recorded(from) else {
            panic!("the recorded answer hands the restore back")
        };
        // 3. Then the starts, each told its agent.
        let run = set.start_restore(from, go, |agent, id| {
            asked.push((agent.clone(), id.to_string()));
            Ok(RestoreConnect::Started(mpsc::channel().1))
        });
        assert_eq!(
            asked,
            vec![
                (AgentId::Claude, "claude-a".to_string()),
                (AgentId::Codex, "codex-b".to_string())
            ]
        );
        let codex = set
            .tab_with_session(&AgentId::Codex, "codex-b")
            .expect("the Codex tab is starting");
        let t = set.get(codex).unwrap();
        assert_eq!(t.agent(), &AgentId::Codex);
        assert_eq!(t.mode(), SessionModeChoice::Bypass, "answered y, so bypass comes back");
        assert_eq!(set.active(), codex, "the saved active tab is shown");
        assert_eq!(set.get(from).unwrap().agent(), &AgentId::Claude);
        // A failed start puts the first tab back as it was, its agent included.
        assert_eq!(run.back_to(from), Some((SessionModeChoice::Auto, AgentId::Claude)));
    }

    /// A saved tab of another agent than the one the restore's own trust answer was about comes back
    /// holding its resume for its own agent's question: listed among the saved tabs while it waits,
    /// in the mode the bypass question gave it, and counted as brought back.
    #[test]
    fn a_restored_external_tab_waits_for_its_own_trust_answer() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let from = set.active();
        let plan = RestorePlan {
            items: vec![
                planned(AgentId::Claude, "claude-a", SessionModeChoice::Auto),
                planned(AgentId::Codex, "codex-b", SessionModeChoice::Bypass),
            ],
            skipped: Vec::new(),
            active_session: Some("codex-b".into()),
        };
        let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("a saved bypass tab is asked about first")
        };
        let go = set.answer_restore(prompt.nonce, true).unwrap();
        assert_eq!(
            go.trust_agent(),
            Some(AgentId::Claude),
            "the Claude tabs start on its answer"
        );
        let mut started: Vec<String> = Vec::new();
        let run = set.start_restore(from, go, |agent, id| {
            if agent.is_external() {
                return Ok(RestoreConnect::AwaitTrust);
            }
            started.push(id.to_string());
            Ok(RestoreConnect::Started(mpsc::channel().1))
        });
        assert_eq!(started, ["claude-a"]);
        let codex = set
            .tab_with_session(&AgentId::Codex, "codex-b")
            .expect("the Codex tab came back");
        assert_eq!(run.awaiting_trust(), &[codex]);
        let t = set.get(codex).unwrap();
        assert_eq!(t.agent(), &AgentId::Codex);
        assert_eq!(t.mode(), SessionModeChoice::Bypass);
        assert!(matches!(
            &t.backend,
            TabBackend::AwaitingTrust(AwaitingTrust {
                start: DeferredStart::Resume { provider_session_id, .. },
                asking: None,
                recording: false,
            }) if provider_session_id == "codex-b"
        ));
        assert_eq!(set.active(), codex, "the saved active tab is shown");
        let saved = set.saved_snapshot("conv").unwrap();
        assert_eq!(others(&saved).len(), 1, "kept in the saved file while it waits");
        assert_eq!(others(&saved)[0].provider_session_id, "codex-b");
        assert_eq!(others_active(&saved), Some(0));
        assert_eq!(run.outcome(), None, "claude-a is still connecting");
        assert_eq!(run.restored, 1, "the waiting tab is back");

        // A restore of another agent's tabs alone waits for that agent's answer.
        let only = RestorePlan {
            items: vec![planned(AgentId::Codex, "codex-c", SessionModeChoice::Auto)],
            skipped: Vec::new(),
            active_session: None,
        };
        let RestoreStep::Go(go) = set.begin_restore(only, BypassPolicy::Ask) else {
            panic!("nothing in bypass, nothing to ask")
        };
        assert_eq!(go.trust_agent(), Some(AgentId::Codex));
        drop(go);
    }

    #[test]
    fn an_external_tab_is_saved_beside_the_claude_tabs_and_is_the_active_one() {
        let dir = workspace("external-saved");
        let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Bypass);
        until("the thread is named", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().provider_session_id().is_some()
        });
        set.select(tab);
        let saved = set.saved_snapshot("conv").unwrap();
        assert!(
            saved.claude_tabs().next().is_none(),
            "the empty Claude tab has nothing to save"
        );
        assert_eq!(
            saved.tabs,
            vec![SavedTab {
                agent: AgentId::Codex,
                conversation_id: "conv".into(),
                provider_session_id: "thread-1".into(),
                name: None,
                mode: SessionModeChoice::Bypass,
                view: None,
                window: None,
                turn_running: false,
            }]
        );
        assert_eq!(others_active(&saved), Some(0));
        // A window on the legacy Claude backend still keeps its Codex tabs.
        set.kind = BackendKind::Legacy;
        assert_eq!(others(&set.saved_snapshot("conv").unwrap()).len(), 1);
        drop(provider);
        for t in set.take_all() {
            if let Some(retired) = t.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// A restored tab whose start is refused (a relative `CODEX_HOME`, a missing binary) stays on screen
    /// as a failed tab, and the saved-tabs file keeps its conversation, under its name and mode, until
    /// the tab is closed; starting it over drops it, as the user chose a fresh session.
    #[test]
    fn a_restored_tab_whose_start_fails_stays_in_the_saved_tabs_until_it_is_closed() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let from = set.active();
        let mut planned_codex = planned(AgentId::Codex, "codex-b", SessionModeChoice::Bypass);
        planned_codex.name = Some("parser work".into());
        let plan = RestorePlan {
            items: vec![planned_codex],
            skipped: Vec::new(),
            active_session: Some("codex-b".into()),
        };
        let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("a saved bypass tab is asked about first")
        };
        let go = set.answer_restore(prompt.nonce, true).unwrap();
        set.start_restore(from, go, |_, _| Ok(RestoreConnect::AwaitTrust));
        let codex = set
            .tab_with_session(&AgentId::Codex, "codex-b")
            .expect("the Codex tab came back");

        // Its own agent's trust answer starts it as a resume by hand, and the start is refused.
        let (tx, result_rx) = mpsc::channel();
        set.get_mut(codex).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "restore-1".into(),
            result_rx,
            first_turn: None,
            resume: Some("codex-b".into()),
            resumed_title: None,
            resumed_name: Some("parser work".into()),
        });
        tx.send(Err(BackendError {
            message: "Codex could not start: CODEX_HOME is relative".into(),
            benign: false,
        }))
        .unwrap();
        let collected = set.collect_starts();
        assert!(matches!(collected.as_slice(), [StartCollected::Failed { tab, .. }] if *tab == codex));
        assert_eq!(set.get(codex).unwrap().wire_state(), TabStateWire::Failed);

        let saved = set.saved_snapshot("conv").unwrap();
        assert_eq!(others(&saved).len(), 1, "the conversation is still in the file");
        assert_eq!(others(&saved)[0].agent, AgentId::Codex);
        assert_eq!(others(&saved)[0].provider_session_id, "codex-b");
        assert_eq!(others(&saved)[0].name.as_deref(), Some("parser work"));
        assert_eq!(others(&saved)[0].mode, SessionModeChoice::Bypass);
        assert_eq!(others_active(&saved), Some(0));

        // The same session opened in another tab is that tab's entry, not listed twice.
        let other = set.open(AgentId::Codex);
        set.get_mut(other).unwrap().backend = TabBackend::AwaitingTrust(AwaitingTrust {
            start: DeferredStart::Resume {
                request_id: "r".into(),
                provider_session_id: "codex-b".into(),
                resumed_title: None,
                resumed_name: None,
            },
            asking: None,
            recording: false,
        });
        assert_eq!(others(&set.saved_snapshot("conv").unwrap()).len(), 1);
        set.remove(other).unwrap();

        // Starting it over is the user's choice of a fresh session; closing it is the user's choice to let go.
        let mut again = set.saved_snapshot("conv").unwrap();
        assert_eq!(others(&again).len(), 1);
        set.reset(codex).unwrap();
        again = set.saved_snapshot("conv").unwrap();
        assert!(others(&again).is_empty(), "a tab started over holds no session");

        set.get_mut(codex).unwrap().backend = TabBackend::Failed { reason: "x".into() };
        set.get_mut(codex).unwrap().failed_resume = Some("codex-b".into());
        assert_eq!(others(&set.saved_snapshot("conv").unwrap()).len(), 1);
        // A second failed tab of the same session (its resume by hand refused too) is not a second entry.
        let twin = set.open(AgentId::Codex);
        set.get_mut(twin).unwrap().backend = TabBackend::Failed { reason: "y".into() };
        set.get_mut(twin).unwrap().failed_resume = Some("codex-b".into());
        assert_eq!(others(&set.saved_snapshot("conv").unwrap()).len(), 1);
        set.remove(twin).unwrap();
        set.remove(codex).unwrap();
        assert!(others(&set.saved_snapshot("conv").unwrap()).is_empty());
    }

    /// An ACP tab is saved, and comes back, exactly when its agent said it can load a saved session.
    #[test]
    fn an_acp_tab_is_saved_only_when_its_agent_loads_sessions() {
        let dir = workspace("acp-saved");
        for (resumes, saved_count) in [(true, 1), (false, 0)] {
            let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
            let agent = AgentId::Acp("gem".into());
            let tab = set.open(agent.clone());
            assert!(
                !set.get(tab).unwrap().resumable(BackendKind::Sidecar),
                "nothing said yet"
            );
            let provider = Arc::new(ExternalRecordingProvider::named("acp:gem", resumes));
            let backend = AgentBackend::start_external_with(
                agent.clone(),
                provider.clone(),
                &dir,
                empty_trust_plan(),
                SessionModeChoice::Auto,
                None,
            )
            .unwrap();
            set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
            until("the session is named", || {
                set.pump(&dir, true);
                set.get(tab).unwrap().provider_session_id().is_some()
            });
            assert_eq!(set.get(tab).unwrap().resumable(BackendKind::Sidecar), resumes);
            set.select(tab);
            let saved = set.saved_snapshot("conv").unwrap();
            assert_eq!(others(&saved).len(), saved_count, "loadSession {resumes}");
            if resumes {
                assert_eq!(others(&saved)[0].agent, agent);
                assert_eq!(others(&saved)[0].provider_session_id, "thread-1");
                assert_eq!(others_active(&saved), Some(0));
            } else {
                assert_eq!(others_active(&saved), None);
            }
            drop(provider);
            for t in set.take_all() {
                if let Some(retired) = t.retired {
                    retired.into_backend().shutdown();
                }
            }
        }
        // A tab brought back holding a saved session waits as resumable, so it stays in the file.
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let from = set.active();
        let plan = RestorePlan {
            items: vec![planned(AgentId::Acp("gem".into()), "gem-s", SessionModeChoice::Auto)],
            skipped: Vec::new(),
            active_session: None,
        };
        let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("nothing in bypass, nothing to ask")
        };
        assert_eq!(go.trust_agent(), Some(AgentId::Acp("gem".into())));
        set.start_restore(from, go, |_, _| Ok(RestoreConnect::AwaitTrust));
        let waiting = set
            .tab_with_session(&AgentId::Acp("gem".into()), "gem-s")
            .expect("the ACP tab came back");
        assert!(set.get(waiting).unwrap().resumable(BackendKind::Sidecar));
        let saved = set.saved_snapshot("conv").unwrap();
        assert_eq!(others(&saved).len(), 1, "kept in the saved file while it waits");
        assert_eq!(others(&saved)[0].provider_session_id, "gem-s");
    }

    #[test]
    fn a_removed_live_tab_keeps_presence_until_retire_done() {
        let dir = workspace("external-retiring-count");
        let (mut set, tab, _provider) = codex_tab(&dir, SessionModeChoice::Auto);
        let review = dir.join(".review-state");
        set.hold_presence(crate::turn_review::PresenceHolder::start(review, std::process::id()));
        let guard = set.presence_guard().unwrap();
        until("the presence is held", || {
            !matches!(guard.check(), Err(crate::turn_review::Blocked::NotHeld(_)))
        });
        assert_eq!(guard.check(), Ok(()));
        let mut removed = set.remove(tab).unwrap();
        assert_eq!(
            guard.check(),
            Err(crate::turn_review::Blocked::TurnRunningHere),
            "the backend is still being shut down"
        );
        if let Some(retired) = removed.retired.take() {
            retired.into_backend().shutdown();
        }
        set.retire_done(tab);
        assert_eq!(guard.check(), Ok(()));
        // A second report for the same tab changes nothing.
        set.retire_done(tab);
        assert_eq!(guard.check(), Ok(()));
    }

    /// The terminal handoff takes the live session away like every other way: until its shutdown has
    /// returned, the window's presence stays as if a turn ran, so no revert writes meanwhile.
    #[test]
    fn a_handed_off_session_counts_as_retiring_until_its_shutdown_returns() {
        let dir = workspace("external-handoff-retiring");
        let (mut set, tab, _provider) = codex_tab(&dir, SessionModeChoice::Auto);
        let review = dir.join(".review-state");
        set.hold_presence(crate::turn_review::PresenceHolder::start(review, std::process::id()));
        let guard = set.presence_guard().unwrap();
        until("the presence is held", || {
            !matches!(guard.check(), Err(crate::turn_review::Blocked::NotHeld(_)))
        });
        assert_eq!(guard.check(), Ok(()));
        let mut backend = set
            .take_for_handoff(tab)
            .expect("the live session is taken")
            .into_backend();
        assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
        assert_eq!(
            guard.check(),
            Err(crate::turn_review::Blocked::TurnRunningHere),
            "the handed-off session is still being shut down"
        );
        assert!(set.take_for_handoff(tab).is_none(), "nothing live is left to take");
        backend.shutdown();
        set.retire_done(tab);
        assert_eq!(guard.check(), Ok(()));
    }

    /// A session handed to a terminal leaves its tab the way every other one does: as a counted
    /// `Retired` naming the tab, whose presence stays until its shutdown is reported, not merely until
    /// the shutdown returns.
    #[test]
    fn the_handoff_rule_is_the_retiring_rule() {
        let dir = workspace("external-handoff-retired");
        let (mut set, tab, _provider) = codex_tab(&dir, SessionModeChoice::Auto);
        let review = dir.join(".review-state");
        set.hold_presence(crate::turn_review::PresenceHolder::start(review, std::process::id()));
        let guard = set.presence_guard().unwrap();
        until("the presence is held", || {
            !matches!(guard.check(), Err(crate::turn_review::Blocked::NotHeld(_)))
        });
        assert_eq!(guard.check(), Ok(()));
        let retired = set.take_for_handoff(tab).expect("the live session is taken");
        assert_eq!(
            retired.tab(),
            tab,
            "the retired backend names the tab it was counted for"
        );
        assert_eq!(
            guard.check(),
            Err(crate::turn_review::Blocked::TurnRunningHere),
            "the handed-off session is still being shut down"
        );
        let mut backend = retired.into_backend();
        backend.shutdown();
        drop(backend);
        assert_eq!(
            guard.check(),
            Err(crate::turn_review::Blocked::TurnRunningHere),
            "the presence ends when the shutdown is reported, as for every retired backend"
        );
        set.retire_done(tab);
        assert_eq!(guard.check(), Ok(()));
    }
}

/// Enter while a turn runs: a message goes into the running turn when the tab's agent can take it, and
/// is held for the turn's end otherwise.
#[cfg(test)]
mod steer_tests {
    use super::*;
    use crate::test_providers::{empty_trust_plan, ExternalRecordingProvider, RecordingProvider};
    use agent::{AgentConversation, ProviderErrorCode};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn workspace(label: &str) -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        dir
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn pump_until_running(set: &mut TabSet, dir: &Path, tab: TabId, running: bool) -> PumpOutput {
        let mut all = None::<PumpOutput>;
        until("the turn state", || {
            let out = set.pump(dir, true);
            match all.as_mut() {
                None => all = Some(out),
                Some(all) => {
                    all.turn_ended.extend(out.turn_ended);
                    all.queue_changed.extend(out.queue_changed);
                }
            }
            set.get(tab).unwrap().session.was_running == running
        });
        all.unwrap()
    }

    fn turn_completed(turn: &str, outcome: agent::TurnOutcome) -> AgentDomainEvent {
        AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        }
    }

    /// A Codex tab on a scripted provider, with `turn-1` running and delivered.
    fn running_codex(label: &str, steering: bool) -> (PathBuf, TabSet, TabId, Arc<ExternalRecordingProvider>) {
        let dir = workspace(label);
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.open(AgentId::Codex);
        let provider = Arc::new(if steering {
            ExternalRecordingProvider::steering_codex()
        } else {
            ExternalRecordingProvider::codex()
        });
        let backend = AgentBackend::start_external_with(
            AgentId::Codex,
            provider.clone(),
            &dir,
            empty_trust_plan(),
            SessionModeChoice::Auto,
            None,
        )
        .unwrap();
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.inner.queue(AgentDomainEvent::TurnStarted {
            turn_id: "turn-1".into(),
        });
        pump_until_running(&mut set, &dir, tab, true);
        (dir, set, tab, provider)
    }

    /// Enter on `text` while the turn runs, with the editor context of the moment in front of it.
    fn enter(set: &mut TabSet, tab: TabId, text: &str) {
        set.queue_message(tab, text, format!("ctx\n\n{text}"), 1, true, PromptOrigin::Typed)
            .unwrap();
        set.steer_held(tab, 2, Some("turn-1"));
    }

    /// The queue as `(typed text, sent into the turn)`.
    fn queue(set: &TabSet, tab: TabId) -> Vec<(String, bool)> {
        set.get(tab)
            .unwrap()
            .session
            .queue
            .iter()
            .map(|q| (q.text.clone(), !q.is_held()))
            .collect()
    }

    fn note(set: &TabSet, tab: TabId) -> Option<String> {
        set.get(tab).unwrap().queue_line()
    }

    fn prompts(set: &TabSet, tab: TabId) -> Vec<String> {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .user_prompts
            .iter()
            .map(|p| p.text.clone())
            .collect()
    }

    fn shut_down(set: &mut TabSet) {
        for tab in set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn enter_puts_the_message_into_the_turn_and_its_row_comes_when_the_agent_reads_it() {
        let (dir, mut set, tab, provider) = running_codex("steer-delivered", true);
        enter(&mut set, tab, "look at the parser too");
        let sent = provider.steers();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].expected_turn_id, "turn-1", "the turn the user saw running");
        assert_eq!(
            sent[0].text, "ctx\n\nlook at the parser too",
            "the wire text carries the context"
        );
        assert_eq!(sent[0].as_typed, "look at the parser too");
        assert_eq!(sent[0].client_message_id, format!("eitri-{}-1", tab.0));
        assert_eq!(queue(&set, tab), [("look at the parser too".to_string(), true)]);
        assert_eq!(note(&set, tab), None);
        assert!(prompts(&set, tab).is_empty(), "no row before the agent read it");

        provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
            client_message_id: sent[0].client_message_id.clone(),
            turn_id: "turn-1".into(),
            text: "look at the parser too".into(),
        });
        let mut changed = Vec::new();
        until("the message leaves the queue", || {
            changed.extend(set.pump(&dir, true).queue_changed);
            set.get(tab).unwrap().session.queue.is_empty()
        });
        assert_eq!(changed, [tab], "the panel is sent the queue again");
        assert_eq!(prompts(&set, tab), ["look at the parser too"]);
        assert!(set.get(tab).unwrap().turn_running(), "the turn was never interrupted");
        assert_eq!(provider.inner.interrupts(), 0);
        shut_down(&mut set);
    }

    #[test]
    fn a_message_the_agent_did_not_read_is_held_again_and_goes_out_after_the_turn() {
        let (dir, mut set, tab, provider) = running_codex("steer-dropped", true);
        enter(&mut set, tab, "and the tests");
        let id = provider.steers()[0].client_message_id.clone();
        provider.inner.queue_all(vec![
            AgentDomainEvent::UserMessageDropped {
                client_message_id: id,
                reason: "the turn ended before Codex read it".into(),
            },
            turn_completed("turn-1", agent::TurnOutcome::Completed),
        ]);
        let out = pump_until_running(&mut set, &dir, tab, false);
        assert_eq!(queue(&set, tab), [("and the tests".to_string(), false)]);
        assert_eq!(
            note(&set, tab).as_deref(),
            Some("held until the turn ends: the turn ended before Codex read it")
        );
        assert!(prompts(&set, tab).is_empty(), "a dropped message has no row");
        assert_eq!(out.turn_ended, [tab]);
        let flush = set.flush_queue(tab).expect("it goes out now");
        assert!(flush.outcome.is_ok());
        assert_eq!(flush.typed, "and the tests");
        assert_eq!(provider.inner.turns(), ["ctx\n\nand the tests"]);
        assert!(set.get(tab).unwrap().session.queue.is_empty());
        assert_eq!(note(&set, tab), None);
        shut_down(&mut set);
    }

    /// The agent runs a message it could not fold into the turn as a follow-up turn of its own (Claude after
    /// a plain-text answer or an interrupt): the provider returns the turn's end and the follow-up's start
    /// from one `pump()`, so the host never sees the turn over with the message unreported, sends nothing,
    /// and the message leaves the queue when the follow-up reads it. Had the end been folded on its own,
    /// the flush would have sent the message again while the agent ran it too.
    #[test]
    fn a_message_run_in_a_follow_up_turn_is_never_sent_again() {
        let (dir, mut set, tab, provider) = running_codex("steer-follow-up", true);
        enter(&mut set, tab, "and the tests");
        let id = provider.steers()[0].client_message_id.clone();
        provider.inner.queue_all(vec![
            turn_completed("turn-1", agent::TurnOutcome::Completed),
            AgentDomainEvent::TurnStarted {
                turn_id: "turn-2".into(),
            },
        ]);
        let mut ended = Vec::new();
        until("the follow-up turn is folded", || {
            ended.extend(set.pump(&dir, true).turn_ended);
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .active_turn_id
                .as_deref()
                == Some("turn-2")
        });
        assert!(ended.is_empty(), "a turn runs, so nothing is flushed: {ended:?}");
        assert!(set.flush_queue(tab).is_none(), "the follow-up runs");
        assert_eq!(
            queue(&set, tab),
            [("and the tests".to_string(), true)],
            "still in the agent's hands"
        );

        provider.inner.queue_all(vec![
            AgentDomainEvent::UserMessageDelivered {
                client_message_id: id,
                turn_id: "turn-2".into(),
                text: "and the tests".into(),
            },
            turn_completed("turn-2", agent::TurnOutcome::Completed),
        ]);
        let out = pump_until_running(&mut set, &dir, tab, false);
        assert_eq!(out.turn_ended, [tab]);
        assert!(set.get(tab).unwrap().session.queue.is_empty());
        assert!(set.flush_queue(tab).is_none());
        assert!(provider.inner.turns().is_empty(), "never sent as a turn of its own");
        assert_eq!(
            prompts(&set, tab),
            ["and the tests"],
            "one row, where the agent read it"
        );
        shut_down(&mut set);
    }

    /// The provider gave up waiting on a message (its follow-up turn came too late) and reported it
    /// dropped, so the host held it again; the flush was refused while the late follow-up ran, and then
    /// the delivery came after all under the old id. The message leaves the queue and is never sent a
    /// second time.
    #[test]
    fn a_message_reported_dropped_and_then_read_late_is_not_sent_again() {
        let (dir, mut set, tab, provider) = running_codex("steer-read-late", true);
        enter(&mut set, tab, "and the tests");
        let id = provider.steers()[0].client_message_id.clone();
        provider.inner.queue_all(vec![
            AgentDomainEvent::UserMessageDropped {
                client_message_id: id.clone(),
                reason: "Claude did not say whether it read it".into(),
            },
            turn_completed("turn-1", agent::TurnOutcome::Completed),
        ]);
        let out = pump_until_running(&mut set, &dir, tab, false);
        assert_eq!(out.turn_ended, [tab]);
        assert_eq!(queue(&set, tab), [("and the tests".to_string(), false)], "held again");
        // The agent is already running the message as a turn of its own, so the send is refused.
        provider.inner.refuse_sends(true);
        let flush = set.flush_queue(tab).expect("a flush is tried");
        assert!(flush.outcome.is_err());
        provider.inner.refuse_sends(false);
        assert_eq!(queue(&set, tab), [("and the tests".to_string(), false)]);

        provider.inner.queue_all(vec![
            AgentDomainEvent::TurnStarted {
                turn_id: "turn-2".into(),
            },
            AgentDomainEvent::UserMessageDelivered {
                client_message_id: id,
                turn_id: "turn-2".into(),
                text: "and the tests".into(),
            },
        ]);
        until("the late delivery takes the message out of the queue", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.queue.is_empty()
        });
        assert_eq!(note(&set, tab), None);
        provider
            .inner
            .queue(turn_completed("turn-2", agent::TurnOutcome::Completed));
        let out = pump_until_running(&mut set, &dir, tab, false);
        assert_eq!(out.turn_ended, [tab]);
        assert!(set.flush_queue(tab).is_none(), "nothing left to send");
        assert_eq!(
            provider.inner.turns(),
            ["ctx\n\nand the tests"],
            "only the refused attempt; the agent ran it once"
        );
        // The refused send still shows its prompt (a rejected turn does, by design, with its error), and
        // the delivery is the row where the agent read it.
        assert_eq!(prompts(&set, tab), ["and the tests", "and the tests"]);
        shut_down(&mut set);
    }

    /// The provider gave up on a message and reported it dropped, and the agent's follow-up turn started
    /// before the host could flush, so the message is held again while a turn runs. Enter does not put it
    /// into that turn under a new id, since the agent may still read the first copy and would then read
    /// it twice: it waits for the turn's end with what was typed after it, and the late delivery under its
    /// old id takes it out of the queue.
    #[test]
    fn a_message_reported_dropped_is_not_put_into_the_turn_again() {
        const WAITS: &str = "held until the turn ends: it went into the turn once already, so it waits for the \
             turn's end, and what follows it waits too";
        let (dir, mut set, tab, provider) = running_codex("steer-given-up-enter", true);
        enter(&mut set, tab, "and the tests");
        let id = provider.steers()[0].client_message_id.clone();
        provider.inner.queue_all(vec![
            AgentDomainEvent::UserMessageDropped {
                client_message_id: id.clone(),
                reason: "Claude did not say whether it read it".into(),
            },
            turn_completed("turn-1", agent::TurnOutcome::Completed),
            AgentDomainEvent::TurnStarted {
                turn_id: "turn-2".into(),
            },
        ]);
        until("the follow-up turn is folded and pumped", || {
            // Read before the pump, not after: the conversation folds on its own thread, so a turn seen
            // started after a pump returned may have been folded after that pump read the projection,
            // and the drop folded in the same batch would not have reached the queue yet.
            let folded = set
                .get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .active_turn_id
                .as_deref()
                == Some("turn-2");
            set.pump(&dir, true);
            folded
        });
        assert_eq!(queue(&set, tab), [("and the tests".to_string(), false)], "held again");
        assert!(set.flush_queue(tab).is_none(), "the follow-up runs");

        set.queue_message(
            tab,
            "and the docs",
            "ctx\n\nand the docs".into(),
            3,
            true,
            PromptOrigin::Typed,
        )
        .unwrap();
        set.steer_held(tab, 4, Some("turn-2"));
        assert_eq!(provider.steers().len(), 1, "nothing more went into a turn");
        assert_eq!(
            queue(&set, tab),
            [
                ("and the tests".to_string(), false),
                ("and the docs".to_string(), false)
            ]
        );
        assert_eq!(note(&set, tab).as_deref(), Some(WAITS));

        provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
            client_message_id: id,
            turn_id: "turn-2".into(),
            text: "and the tests".into(),
        });
        until("the late delivery takes the message out of the queue", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.queue.len() == 1
        });
        assert_eq!(queue(&set, tab), [("and the docs".to_string(), false)]);
        provider
            .inner
            .queue(turn_completed("turn-2", agent::TurnOutcome::Completed));
        let out = pump_until_running(&mut set, &dir, tab, false);
        assert_eq!(out.turn_ended, [tab]);
        let flush = set.flush_queue(tab).expect("what was typed after it goes out");
        assert!(flush.outcome.is_ok());
        assert_eq!(
            provider.inner.turns(),
            ["ctx\n\nand the docs"],
            "the agent read the first message once, and it was never sent again"
        );
        assert_eq!(provider.steers().len(), 1);
        shut_down(&mut set);
    }

    /// A message read under an id it was reported dropped from, while a newer copy is still with the
    /// agent, stays in the queue until the newer copy's outcome comes, or until no turn runs; then it
    /// leaves without being sent again, whatever became of the newer copy.
    #[test]
    fn a_late_delivery_of_an_old_copy_waits_for_the_newer_copy() {
        let steered = Queued {
            state: QueuedState::Steered {
                client_id: "new".into(),
                sent_at_ms: 2,
            },
            dropped_as: vec!["old".into()],
            ..Queued::held("x", "x".into(), 1)
        };
        let delivered = |id: &str| AgentDomainEvent::UserMessageDelivered {
            client_message_id: id.into(),
            turn_id: "turn-2".into(),
            text: "x".into(),
        };
        let mut projection = agent::AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::TurnStarted {
            turn_id: "turn-2".into(),
        });
        projection.apply(&delivered("old"));
        let mut queue = vec![steered.clone()];
        let mut note = None;
        assert!(!settle_against(&AgentId::Codex, &mut queue, &mut note, &projection));
        assert_eq!(queue, [steered.clone()], "the newer copy is still open");

        let mut dropped = projection.clone();
        dropped.apply(&AgentDomainEvent::UserMessageDropped {
            client_message_id: "new".into(),
            reason: "Claude discarded it".into(),
        });
        assert!(settle_against(&AgentId::Codex, &mut queue, &mut note, &dropped));
        assert!(queue.is_empty(), "the copy the agent read is enough");
        assert_eq!(note, None, "not held again");

        let mut ended = projection.clone();
        ended.apply(&turn_completed("turn-2", agent::TurnOutcome::Completed));
        let mut queue = vec![steered.clone()];
        assert!(settle_against(&AgentId::Codex, &mut queue, &mut note, &ended));
        assert!(queue.is_empty(), "no turn runs, and the agent read it");

        let mut read_twice = projection;
        read_twice.apply(&delivered("new"));
        let mut queue = vec![steered];
        assert!(settle_against(&AgentId::Codex, &mut queue, &mut note, &read_twice));
        assert!(queue.is_empty());
    }

    /// An agent that listed its commands: the tab reads a leading `/` as one.
    fn lists_commands(set: &mut TabSet, dir: &Path, tab: TabId, provider: &ExternalRecordingProvider) {
        provider.inner.queue(AgentDomainEvent::CommandsAvailable {
            commands: vec![agent::SlashCommand {
                name: "compact".into(),
                description: String::new(),
                hint: None,
            }],
        });
        until("the list of commands is folded", || {
            set.pump(dir, true);
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .available_commands
                .is_some()
        });
    }

    fn typed(set: &mut TabSet, tab: TabId, text: &str) {
        set.queue_message(tab, text, text.into(), 1, true, PromptOrigin::Typed)
            .unwrap();
    }

    /// Ends `turn`, and sends what the tab's queue sends then, as the tick does.
    fn turn_ends_and_flushes(
        set: &mut TabSet,
        dir: &Path,
        tab: TabId,
        provider: &ExternalRecordingProvider,
        turn: &str,
    ) {
        provider
            .inner
            .queue(turn_completed(turn, agent::TurnOutcome::Completed));
        assert_eq!(pump_until_running(set, dir, tab, false).turn_ended, [tab]);
        set.flush_queue(tab).expect("the queue goes").outcome.unwrap();
    }

    fn turn_runs(set: &mut TabSet, dir: &Path, tab: TabId, provider: &ExternalRecordingProvider, turn: &str) {
        provider
            .inner
            .queue(AgentDomainEvent::TurnStarted { turn_id: turn.into() });
        pump_until_running(set, dir, tab, true);
    }

    /// Only the command waits: on an agent that reads commands it is never put into the turn, the message
    /// typed behind it is, and the command stays in its place. On Codex, which lists none, `/status` goes into
    /// the turn as typed.
    #[test]
    fn a_message_behind_a_command_goes_into_the_turn_and_the_command_waits_in_its_place() {
        let (dir, mut set, tab, provider) = running_codex("steer-command-listed", true);
        lists_commands(&mut set, &dir, tab, &provider);
        typed(&mut set, tab, "/compact");
        typed(&mut set, tab, "then this");
        assert!(set.steer_held(tab, 2, Some("turn-1")), "the message went in");
        let texts: Vec<String> = provider.steers().into_iter().map(|s| s.as_typed).collect();
        assert_eq!(texts, ["then this"], "the command is not sent as text");
        assert_eq!(
            queue(&set, tab),
            [("/compact".to_string(), false), ("then this".to_string(), true)]
        );
        assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
        assert_eq!(
            set.get(tab).unwrap().session.queue_note,
            None,
            "the note is the held command's, and goes with it"
        );

        // The agent reads the message out of order with the queue; the command stays, and says why.
        let id = provider.steers()[0].client_message_id.clone();
        provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
            client_message_id: id,
            turn_id: "turn-1".into(),
            text: "then this".into(),
        });
        until("the message leaves the queue", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.queue.len() == 1
        });
        assert_eq!(queue(&set, tab), [("/compact".to_string(), false)]);
        assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
        shut_down(&mut set);

        // Claude reads commands whatever it listed; an ACP agent that withdrew its list (an empty one) reads none.
        let mut none_listed = agent::AgentSessionProjection::default();
        assert!(reads_slash_commands(&AgentId::Claude, &none_listed));
        assert!(!reads_slash_commands(&AgentId::Codex, &none_listed));
        none_listed.available_commands = Some(Vec::new());
        assert!(!reads_slash_commands(&AgentId::Acp("x".into()), &none_listed));

        // Codex lists none: the text goes in as typed, and a slash later in a message is no command.
        let (_dir, mut set, tab, provider) = running_codex("steer-command-codex", true);
        enter(&mut set, tab, "/status");
        let texts: Vec<String> = provider.steers().into_iter().map(|s| s.as_typed).collect();
        assert_eq!(texts, ["/status"]);
        assert_eq!(note(&set, tab), None);
        shut_down(&mut set);
        assert!(is_slash_command("/x") && is_slash_command("  /x") && !is_slash_command("a/b"));
    }

    /// A command does not stop what comes after it, and a message does not stop a command behind it; the
    /// messages reach the agent in the order typed.
    #[test]
    fn messages_around_commands_go_into_the_turn_in_order_and_the_commands_stay() {
        let (dir, mut set, tab, provider) = running_codex("steer-around-commands", true);
        lists_commands(&mut set, &dir, tab, &provider);
        for text in ["one", "/a", "two", "/b", "three"] {
            typed(&mut set, tab, text);
        }
        set.steer_held(tab, 2, Some("turn-1"));
        let texts: Vec<String> = provider.steers().into_iter().map(|s| s.as_typed).collect();
        assert_eq!(texts, ["one", "two", "three"]);
        assert_eq!(
            queue(&set, tab),
            [
                ("one".to_string(), true),
                ("/a".to_string(), false),
                ("two".to_string(), true),
                ("/b".to_string(), false),
                ("three".to_string(), true),
            ]
        );
        assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
        shut_down(&mut set);
    }

    /// The other stops still stop: what follows them waits, and the line says both things.
    #[test]
    fn a_refusal_behind_a_command_holds_what_follows_it_and_says_both() {
        let (dir, mut set, tab, provider) = running_codex("steer-command-refused", true);
        lists_commands(&mut set, &dir, tab, &provider);
        provider.refuse_steers(Some(ProviderErrorCode::NoActiveTurn));
        typed(&mut set, tab, "/compact");
        typed(&mut set, tab, "then this");
        typed(&mut set, tab, "and this");
        set.steer_held(tab, 2, Some("turn-1"));
        assert!(provider.steers().is_empty());
        assert_eq!(
            note(&set, tab).as_deref(),
            Some("commands run when the turn ends; held until the turn ends: the scripted agent took no message")
        );

        // The refused message is steered at the next Enter, and only the command's words stay.
        provider.refuse_steers(None);
        set.steer_held(tab, 3, Some("turn-1"));
        let texts: Vec<String> = provider.steers().into_iter().map(|s| s.as_typed).collect();
        assert_eq!(texts, ["then this", "and this"]);
        assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
        shut_down(&mut set);
    }

    /// A message dropped beside a held command: the line keeps both, and loses the drop's reason once the
    /// message has left, while the command still waits.
    #[test]
    fn a_dropped_message_leaves_the_commands_words_and_goes_on_its_own() {
        let (dir, mut set, tab, provider) = running_codex("steer-command-dropped", true);
        lists_commands(&mut set, &dir, tab, &provider);
        typed(&mut set, tab, "/compact");
        typed(&mut set, tab, "then this");
        set.steer_held(tab, 2, Some("turn-1"));
        let id = provider.steers()[0].client_message_id.clone();
        provider.inner.queue(AgentDomainEvent::UserMessageDropped {
            client_message_id: id.clone(),
            reason: "gave up waiting".into(),
        });
        until("the message is held again", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.queue.iter().all(Queued::is_held)
        });
        assert_eq!(
            note(&set, tab).as_deref(),
            Some("commands run when the turn ends; held until the turn ends: gave up waiting")
        );
        provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
            client_message_id: id,
            turn_id: "turn-1".into(),
            text: "then this".into(),
        });
        until("the late delivery takes it out", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.queue.len() == 1
        });
        assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
        shut_down(&mut set);
    }

    /// A command the CLI refused stays until it is taken back, and the line goes on saying so after the message
    /// behind it has left: it is not a held command, which would run at the turn's end.
    #[test]
    fn a_refused_command_keeps_its_words_when_the_message_behind_it_leaves() {
        let (dir, mut set, tab, provider) = running_codex("steer-refused-command", true);
        lists_commands(&mut set, &dir, tab, &provider);
        typed(&mut set, tab, "/unknown");
        typed(&mut set, tab, "then this");
        {
            let session = &mut set.get_mut(tab).unwrap().session;
            session.queue[0].refused = true;
            session.queue_note = Some(refused_note(agent::CLI_REFUSED_DROP));
        }
        set.steer_held(tab, 2, Some("turn-1"));
        let words = refused_note(agent::CLI_REFUSED_DROP);
        assert_eq!(note(&set, tab).as_deref(), Some(words.as_str()));
        let id = provider.steers()[0].client_message_id.clone();
        provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
            client_message_id: id,
            turn_id: "turn-1".into(),
            text: "then this".into(),
        });
        until("the message leaves the queue", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.queue.len() == 1
        });
        assert_eq!(note(&set, tab).as_deref(), Some(words.as_str()));
        shut_down(&mut set);
    }

    /// What a turn's end finds in the queue goes out in the order typed, a command alone: read with a message
    /// behind it, the message would be the command's arguments and never reach the agent.
    #[test]
    fn at_the_turns_end_a_command_goes_alone_and_what_follows_it_waits_for_its_end() {
        let (dir, mut set, tab, provider) = running_codex("flush-commands", false);
        lists_commands(&mut set, &dir, tab, &provider);
        for text in ["one", "two", "/a", "/b", "three", "four"] {
            typed(&mut set, tab, text);
        }
        set.steer_held(tab, 2, Some("turn-1"));
        assert_eq!(
            note(&set, tab).as_deref(),
            Some("commands run when the turn ends; held until the turn ends: Codex cannot take messages mid-turn")
        );
        turn_ends_and_flushes(&mut set, &dir, tab, &provider, "turn-1");
        assert_eq!(provider.inner.turns(), ["one\n\ntwo"]);
        assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
        turn_runs(&mut set, &dir, tab, &provider, "turn-2");
        turn_ends_and_flushes(&mut set, &dir, tab, &provider, "turn-2");
        assert_eq!(provider.inner.turns(), ["one\n\ntwo", "/a"]);
        turn_runs(&mut set, &dir, tab, &provider, "turn-3");
        turn_ends_and_flushes(&mut set, &dir, tab, &provider, "turn-3");
        assert_eq!(provider.inner.turns(), ["one\n\ntwo", "/a", "/b"]);
        assert_eq!(
            queue(&set, tab),
            [("three".to_string(), false), ("four".to_string(), false)]
        );
        assert_eq!(note(&set, tab), None, "no command is held any more");
        turn_runs(&mut set, &dir, tab, &provider, "turn-4");
        turn_ends_and_flushes(&mut set, &dir, tab, &provider, "turn-4");
        assert_eq!(provider.inner.turns(), ["one\n\ntwo", "/a", "/b", "three\n\nfour"]);
        assert!(set.get(tab).unwrap().session.queue.is_empty());
        shut_down(&mut set);
    }

    /// Codex lists no commands: the whole queue is one turn, as typed. A message the CLI refused stays put, and
    /// a review is a message whatever it starts with.
    #[test]
    fn next_turn_splits_only_at_a_command_the_agent_reads() {
        let item = |text: &str| Queued::held(text, text.into(), 1);
        let mut refused = item("/refused");
        refused.refused = true;
        let mut review = item("/a review");
        review.steerable = false;
        let queue = [refused, item("one"), review, item("/a"), item("two")];
        assert_eq!(next_turn(&queue, false), [1, 2, 3, 4]);
        assert_eq!(next_turn(&queue, true), [1, 2]);
        assert_eq!(next_turn(&queue[3..], true), [0]);
        assert_eq!(next_turn(&queue[4..], true), [0]);
        assert!(next_turn(&queue[..1], true).is_empty());
    }

    #[test]
    fn a_refusal_holds_the_message_and_every_one_after_it_keeping_their_order() {
        let (dir, mut set, tab, provider) = running_codex("steer-refused", true);
        provider.refuse_steers(Some(ProviderErrorCode::NoActiveTurn));
        set.queue_message(tab, "first", "first".into(), 1, true, PromptOrigin::Typed)
            .unwrap();
        set.queue_message(tab, "second", "second".into(), 1, true, PromptOrigin::Typed)
            .unwrap();
        assert!(set.steer_held(tab, 2, Some("turn-1")), "the note changed");
        assert_eq!(
            queue(&set, tab),
            [("first".to_string(), false), ("second".to_string(), false)]
        );
        assert_eq!(
            note(&set, tab).as_deref(),
            Some("held until the turn ends: the scripted agent took no message")
        );
        assert!(provider.steers().is_empty(), "nothing was sent");

        // The next Enter tries again, oldest first, and the note goes once nothing is held.
        provider.refuse_steers(None);
        enter(&mut set, tab, "third");
        let texts: Vec<String> = provider.steers().into_iter().map(|s| s.as_typed).collect();
        assert_eq!(texts, ["first", "second", "third"]);
        assert!(queue(&set, tab).iter().all(|(_, steered)| *steered));
        assert_eq!(note(&set, tab), None);
        let ids: std::collections::BTreeSet<String> =
            provider.steers().into_iter().map(|s| s.client_message_id).collect();
        assert_eq!(ids.len(), 3, "every message has its own id");
        let _ = dir;
        shut_down(&mut set);
    }

    #[test]
    fn an_agent_that_cannot_take_messages_mid_turn_holds_them_and_says_so() {
        let (_dir, mut set, tab, provider) = running_codex("steer-unsupported-codex", false);
        enter(&mut set, tab, "later");
        assert_eq!(queue(&set, tab), [("later".to_string(), false)]);
        assert_eq!(
            note(&set, tab).as_deref(),
            Some("held until the turn ends: Codex cannot take messages mid-turn")
        );
        assert!(provider.steers().is_empty());
        shut_down(&mut set);

        // A Claude tab: the sidecar does not take them yet either.
        let dir = workspace("steer-unsupported-claude");
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        let provider = Arc::new(RecordingProvider::default());
        let conversation =
            AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        });
        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        pump_until_running(&mut set, &dir, tab, true);
        enter(&mut set, tab, "later");
        assert_eq!(queue(&set, tab), [("later".to_string(), false)]);
        assert_eq!(
            note(&set, tab).as_deref(),
            Some("held until the turn ends: Claude cannot take messages mid-turn")
        );
        shut_down(&mut set);
    }

    #[test]
    fn nothing_is_put_into_a_turn_when_none_runs() {
        let dir = workspace("steer-idle");
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.open(AgentId::Codex);
        let provider = Arc::new(ExternalRecordingProvider::steering_codex());
        let backend = AgentBackend::start_external_with(
            AgentId::Codex,
            provider.clone(),
            &dir,
            empty_trust_plan(),
            SessionModeChoice::Auto,
            None,
        )
        .unwrap();
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        set.queue_message(tab, "x", "x".into(), 1, true, PromptOrigin::Typed)
            .unwrap();
        assert!(!set.steer_held(tab, 2, Some("turn-1")));
        assert!(provider.steers().is_empty());
        shut_down(&mut set);
    }

    #[test]
    fn taking_the_queue_back_leaves_what_was_sent_into_the_turn() {
        let (_dir, mut set, tab, provider) = running_codex("steer-take-back", true);
        enter(&mut set, tab, "sent");
        provider.refuse_steers(Some(ProviderErrorCode::NoActiveTurn));
        enter(&mut set, tab, "held");
        assert_eq!(
            queue(&set, tab),
            [("sent".to_string(), true), ("held".to_string(), false)]
        );
        assert_eq!(set.take_back_queue(tab), ["held"]);
        assert_eq!(queue(&set, tab), [("sent".to_string(), true)]);
        assert_eq!(note(&set, tab), None, "nothing is held any more");
        assert!(set.take_back_queue(tab).is_empty());
        shut_down(&mut set);
    }

    /// Codex forgets a message it had not read when the turn is interrupted (measured on 0.162.1), and says
    /// so as a drop before the turn's end: `Ctrl+Enter` then sends it ahead of what was typed with the key.
    #[test]
    fn send_now_with_a_message_in_the_turn_sends_it_again_once_the_agent_drops_it() {
        let (dir, mut set, tab, provider) = running_codex("steer-send-now", true);
        enter(&mut set, tab, "first");
        let id = provider.steers()[0].client_message_id.clone();
        match set.send_now(tab, "now", "now".into(), 3).unwrap() {
            SendNow::Interrupting(Ok(_)) => {}
            _ => panic!("a running turn is interrupted"),
        }
        assert_eq!(provider.inner.interrupts(), 1);
        assert_eq!(
            queue(&set, tab),
            [("first".to_string(), true), ("now".to_string(), false)]
        );
        provider.inner.queue_all(vec![
            AgentDomainEvent::UserMessageDropped {
                client_message_id: id,
                reason: "the turn ended before Codex read it".into(),
            },
            turn_completed("turn-1", agent::TurnOutcome::Interrupted),
        ]);
        let out = pump_until_running(&mut set, &dir, tab, false);
        assert_eq!(out.turn_ended, [tab]);
        let flush = set.flush_queue(tab).expect("the queue goes out after the interrupt");
        assert_eq!(flush.typed, "first\n\nnow");
        assert_eq!(provider.inner.turns(), ["ctx\n\nfirst\n\nnow"]);
        shut_down(&mut set);
    }

    /// A provider that never said what became of a message: it goes out with the next turn rather than
    /// staying in the queue for good.
    #[test]
    fn a_message_no_outcome_was_reported_for_goes_out_with_the_next_turn() {
        let (dir, mut set, tab, provider) = running_codex("steer-no-outcome", true);
        enter(&mut set, tab, "unanswered");
        provider
            .inner
            .queue(turn_completed("turn-1", agent::TurnOutcome::Completed));
        pump_until_running(&mut set, &dir, tab, false);
        let flush = set.flush_queue(tab).expect("sent");
        assert_eq!(flush.typed, "unanswered");
        shut_down(&mut set);
    }

    /// What the agent did with a message is read off the projection, so a path that never sees the events
    /// (a resync, or a take-back before the next pump) still finds it.
    #[test]
    fn the_outcome_is_read_from_the_projection_not_only_from_a_delivered_batch() {
        let (_dir, mut set, tab, provider) = running_codex("steer-from-projection", true);
        enter(&mut set, tab, "read");
        let id = provider.steers()[0].client_message_id.clone();
        provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
            client_message_id: id.clone(),
            turn_id: "turn-1".into(),
            text: "read".into(),
        });
        until("the projection folds it", || {
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .steer_outcome(&id)
                .is_some()
        });
        assert!(set.take_back_queue(tab).is_empty());
        assert!(set.get(tab).unwrap().session.queue.is_empty(), "settled without a pump");
        shut_down(&mut set);
    }

    #[test]
    fn closing_says_a_message_in_the_turn_may_not_have_reached_the_agent() {
        let (_dir, mut set, tab, provider) = running_codex("steer-close", true);
        enter(&mut set, tab, "sent");
        provider.refuse_steers(Some(ProviderErrorCode::NoActiveTurn));
        enter(&mut set, tab, "held");
        let facts = set.close_facts(tab).unwrap();
        assert_eq!((facts.queued, facts.steered), (1, 1));
        let lines = tabs::close_prompt(&facts);
        assert!(
            lines.contains(&"1 queued message goes to history".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(
                &"1 message sent into the turn may not have reached the agent; it goes to history".to_string()
            ),
            "{lines:?}"
        );
        assert_eq!(set.queue_texts(tab), ["sent", "held"], "both go to history");
        assert_eq!(set.queued_count(), 2, "the window counts both");
        assert!(tabs::close_steps(&facts).contains(&tabs::CloseStep::QueueToHistory));
        shut_down(&mut set);
    }

    /// The flush reads what the agent reported and whether a turn runs in one read: a delivery and the turn's
    /// end folded after the last pump and before the flush are seen together, and the message the agent read
    /// is not sent again.
    #[test]
    fn a_delivery_folded_just_before_the_flush_is_not_sent_again() {
        let (_dir, mut set, tab, provider) = running_codex("steer-fold-before-flush", true);
        enter(&mut set, tab, "read just now");
        let id = provider.steers()[0].client_message_id.clone();
        provider.inner.queue_all(vec![
            AgentDomainEvent::UserMessageDelivered {
                client_message_id: id,
                turn_id: "turn-1".into(),
                text: "read just now".into(),
            },
            turn_completed("turn-1", agent::TurnOutcome::Completed),
        ]);
        until("the projection folds both, with no pump", || {
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .active_turn_id
                .is_none()
        });
        assert_eq!(
            queue(&set, tab),
            [("read just now".to_string(), true)],
            "not settled yet"
        );
        assert!(set.flush_queue(tab).is_none(), "nothing left to send");
        assert!(
            provider.inner.turns().is_empty(),
            "the message the agent read was not sent again"
        );
        assert!(set.get(tab).unwrap().session.queue.is_empty());
        shut_down(&mut set);
    }

    /// A delivery folded with the turn still running: the flush sends nothing, and the message leaves the
    /// queue as read.
    #[test]
    fn a_flush_during_the_turn_settles_and_sends_nothing() {
        let (_dir, mut set, tab, provider) = running_codex("steer-flush-running", true);
        enter(&mut set, tab, "mid-turn");
        let id = provider.steers()[0].client_message_id.clone();
        provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
            client_message_id: id.clone(),
            turn_id: "turn-1".into(),
            text: "mid-turn".into(),
        });
        until("the delivery is folded", || {
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .steer_outcome(&id)
                .is_some()
        });
        assert!(set.flush_queue(tab).is_none());
        assert!(set.get(tab).unwrap().session.queue.is_empty());
        assert!(provider.inner.turns().is_empty());
        shut_down(&mut set);
    }

    /// Only into the turn the page showed: one typed in a turn that has ended since, or with none showing,
    /// is held.
    #[test]
    fn a_message_goes_only_into_the_turn_the_page_showed() {
        let (_dir, mut set, tab, provider) = running_codex("steer-shown-turn", true);
        set.queue_message(
            tab,
            "for the old turn",
            "for the old turn".into(),
            1,
            true,
            PromptOrigin::Typed,
        )
        .unwrap();
        set.steer_held(tab, 2, Some("turn-0"));
        assert!(provider.steers().is_empty());
        assert_eq!(
            note(&set, tab).as_deref(),
            Some("held until the turn ends: the turn it was typed in has ended")
        );
        set.steer_held(tab, 3, None);
        assert!(provider.steers().is_empty());
        assert_eq!(
            note(&set, tab).as_deref(),
            Some("held until the turn ends: no turn was running when it was typed")
        );
        set.steer_held(tab, 4, Some("turn-1"));
        assert_eq!(provider.steers().len(), 1);
        assert_eq!(note(&set, tab), None);
        shut_down(&mut set);
    }

    /// A confirmed review held behind the turn waits for its end, and so does what is typed after it.
    #[test]
    fn a_review_waits_for_the_turns_end_and_keeps_what_follows_behind_it() {
        let (_dir, mut set, tab, provider) = running_codex("steer-review", true);
        assert!(set.send_review(tab, "the review", 1).unwrap().is_none(), "it waits");
        enter(&mut set, tab, "typed after it");
        assert!(
            provider.steers().is_empty(),
            "nothing goes into the turn ahead of the review"
        );
        assert_eq!(
            queue(&set, tab),
            [("the review".to_string(), false), ("typed after it".to_string(), false)]
        );
        assert_eq!(
            note(&set, tab).as_deref(),
            Some("held until the turn ends: a review waits for the turn's end, and what follows it waits too")
        );
        shut_down(&mut set);
    }

    /// A tripwire takes the session away: a message still in the turn is held again with the reason, not
    /// left as `↪` for good.
    #[test]
    fn a_tab_failed_by_a_tripwire_holds_what_was_in_the_turn() {
        let (dir, mut set, tab, provider) = running_codex("steer-tripwire", true);
        enter(&mut set, tab, "in the turn");
        provider.inner.queue(AgentDomainEvent::PolicyMismatch {
            requested: "on-request".into(),
            reported: "never".into(),
        });
        let mut changed = Vec::new();
        until("the tab fails", || {
            let mut out = set.pump(&dir, true);
            changed.append(&mut out.queue_changed);
            for tripped in out.tripped {
                let mut backend = tripped.retired.into_backend();
                backend.shutdown();
            }
            matches!(set.get(tab).unwrap().backend, TabBackend::Failed { .. })
        });
        assert_eq!(queue(&set, tab), [("in the turn".to_string(), false)]);
        assert_eq!(
            note(&set, tab).as_deref(),
            Some("held until the turn ends: the session was closed before the agent read it")
        );
        assert_eq!(changed, [tab], "the panel is sent the queue again");
        shut_down(&mut set);
    }

    /// A command that fails fatally takes the session away like a tripwire: a message still in the turn
    /// is held again with the reason, and `↑` takes it back.
    #[test]
    fn a_fatal_command_holds_a_steered_message_again() {
        let (_dir, mut set, tab, provider) = running_codex("steer-fatal", true);
        enter(&mut set, tab, "in the turn");
        assert_eq!(provider.steers().len(), 1, "it went into the turn");
        let mut backend = set
            .fail_session(tab, "gone".into(), FailedHow::AfterStart, true)
            .expect("the live backend is handed back to be shut down")
            .into_backend();
        backend.shutdown();
        assert!(matches!(
            &set.get(tab).unwrap().backend,
            TabBackend::Failed { reason } if reason == "gone"
        ));
        assert!(set.get(tab).unwrap().failed_after_start);
        assert_eq!(queue(&set, tab), [("in the turn".to_string(), false)]);
        // Read before the take-back, which clears the note.
        assert_eq!(
            note(&set, tab),
            Some(held_note("the session was closed before the agent read it"))
        );
        assert_eq!(set.take_back_queue(tab), ["in the turn"]);
        set.retire_done(tab);
        shut_down(&mut set);
    }

    /// What the agent reported before its session went is applied first: the message it read leaves the
    /// queue, and only the one it did not is held again, where it stood.
    #[test]
    fn only_the_steered_messages_the_agent_did_not_take_are_held_again_in_order() {
        let (_dir, mut set, tab, provider) = running_codex("steer-fatal-partly-read", true);
        enter(&mut set, tab, "read");
        enter(&mut set, tab, "not read");
        let ids: Vec<String> = provider.steers().into_iter().map(|s| s.client_message_id).collect();
        assert_eq!(ids.len(), 2, "both went into the turn");
        provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
            client_message_id: ids[0].clone(),
            turn_id: "turn-1".into(),
            text: "read".into(),
        });
        until("the delivery is folded", || {
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .steer_outcome(&ids[0])
                .is_some()
        });
        assert_eq!(
            queue(&set, tab),
            [("read".to_string(), true), ("not read".to_string(), true)],
            "not settled before the failure"
        );
        let mut backend = set
            .fail_session(tab, "gone".into(), FailedHow::AfterStart, false)
            .unwrap()
            .into_backend();
        backend.shutdown();
        assert_eq!(queue(&set, tab), [("not read".to_string(), false)]);
        assert_eq!(set.take_back_queue(tab), ["not read"]);
        set.retire_done(tab);
        shut_down(&mut set);
    }

    #[test]
    fn the_queue_envelope_says_whether_enter_steers_now() {
        let (_dir, mut set, tab, _provider) = running_codex("steer-envelope", true);
        let payloads = set.active_state_payloads();
        let queue: serde_json::Value = payloads
            .iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
            .find(|v| v["kind"] == "queue")
            .unwrap();
        assert_eq!(queue["enterSteers"], true);
        assert_eq!(set.get(tab).unwrap().steers(), Some(true));
        shut_down(&mut set);
        let (_dir, mut set, tab, _provider) = running_codex("steer-envelope-no", false);
        assert_eq!(set.get(tab).unwrap().steers(), Some(false));
        shut_down(&mut set);
    }

    #[test]
    fn ordering_base_tabs_keeps_the_numbers_of_tabs_already_here_and_numbers_arrivals_after_them() {
        let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
        let one = tabs.active();
        let two = tabs.open(AgentId::Claude);
        let three = tabs.open(AgentId::Claude);
        tabs.remove(two).unwrap();
        // The lowest free number is the gap, so the first arrival lands between the tabs that were here.
        let a = tabs.open(AgentId::Claude);
        let b = tabs.open(AgentId::Claude);
        assert_eq!(tabs.get(a).unwrap().number, 2);
        assert!(tabs.order_base_tabs(&[one, three, a, b], &[a, b]));
        let numbers: Vec<_> = [one, three, a, b]
            .iter()
            .map(|t| tabs.get(*t).unwrap().number)
            .collect();
        assert_eq!(numbers, vec![1, 3, 4, 5]);
        assert_eq!(tabs.base_tabs(), vec![one, three, a, b]);
        assert!(
            !tabs.order_base_tabs(&[one, three, a], &[]),
            "an order missing a tab is refused"
        );
    }
}

#[cfg(test)]
mod question_gate_tests {
    //! Every path of the tab set that answers cards by itself leaves Claude's questions waiting: a
    //! question is never in `pending_permissions` nor drawn as a card, and only `answer_question` and
    //! `decline_question` answer one.
    use super::*;
    use crate::tab_restore::{PlannedTab, RestorePlan};
    use crate::test_providers::RecordingProvider;
    use agent::{AgentConversation, AgentDomainEvent, QuestionOptionSpec, QuestionSpec, QuestionState};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn workspace(label: &str) -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        dir
    }

    fn live(dir: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
        let provider = Arc::new(RecordingProvider::default());
        let conversation =
            AgentConversation::create(provider.clone(), dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        (
            provider,
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            },
        )
    }

    fn set() -> TabSet {
        TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto)
    }

    /// A `Write` into `.git`: a card in Manual and Auto, answered in Bypass.
    fn card(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
            provider_prompt: None,
        }
    }

    fn asked(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::QuestionAsked {
            interaction_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            questions: vec![QuestionSpec {
                question: "Which layout?".into(),
                header: "Layout".into(),
                multi_select: false,
                options: ["Grid", "List"]
                    .into_iter()
                    .map(|label| QuestionOptionSpec {
                        label: label.into(),
                        description: String::new(),
                        preview: None,
                    })
                    .collect(),
            }],
            unsupported_reason: None,
        }
    }

    fn question(set: &TabSet, tab: TabId, id: &str) -> Option<QuestionState> {
        let backend = set.get(tab)?.live()?;
        let state = backend.projection().question(id).map(|record| record.state.clone());
        state
    }

    /// Pumps the set as the panel's tick does until `done`.
    fn pump_until(set: &mut TabSet, dir: &Path, what: &str, done: impl Fn(&TabSet) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(set) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            set.pump(dir, true);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn enter_bypass(set: &mut TabSet, tab: TabId) -> ConfirmOutcome {
        let plan = match set.cycle_mode(tab, ViewId::BASE) {
            Ok(ModeCycle::Confirm(plan)) => plan,
            other => panic!("expected a bypass prompt, got {other:?}"),
        };
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap()
    }

    fn shut_down_all(set: &mut TabSet) {
        for tab in set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    fn waiting(label: &str) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
        let dir = workspace(label);
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(asked("q1"));
        pump_until(&mut set, &dir, "question attention", |s| {
            s.get(tab).unwrap().attention.attention().pending == 1
        });
        (dir, set, tab, provider)
    }

    fn settle(provider: &RecordingProvider) {
        provider.queue(AgentDomainEvent::QuestionSettled {
            interaction_id: "q1".into(),
            outcome: agent::QuestionOutcome::Declined,
            answers: Vec::new(),
            reason: None,
        });
    }

    #[test]
    fn closing_a_tab_with_a_question_asks() {
        let (_dir, mut set, tab, _) = waiting("question-close");
        assert!(!set.get(tab).unwrap().turn_running());
        assert!(set.close_needs_confirm(tab));
        assert!(tabs::close_prompt(&set.close_facts(tab).unwrap())
            .contains(&"closing cancels Claude's question".to_string()));
        assert_eq!(
            tabs::window_close_prompt(set.running_count(), 0, 0).as_deref(),
            Some("close window? 1 running (y/n)")
        );
        set.open(AgentId::Claude);
        assert_eq!(set.close_others_plan().unwrap().1, "close 1 other tab? 1 running (y/n)");
        assert!(set.take_for_handoff(tab).is_none());
        shut_down_all(&mut set);
    }

    #[test]
    fn close_others_names_background_tabs_beside_outstanding_questions() {
        let (dir, mut set, tab, provider) = waiting("question-background-others");
        provider.queue(AgentDomainEvent::BackgroundCalls { running: 2 });
        pump_until(&mut set, &dir, "background count", |s| s.background_count() == 1);
        set.open(AgentId::Claude);
        assert_eq!(
            set.close_others_plan(),
            Some((
                vec![tab],
                "close 1 other tab? 1 running, 1 with background tasks (y/n)".into()
            ))
        );
        shut_down_all(&mut set);
    }

    fn task(id: &str, kind: agent::TaskKind, ambient: bool) -> agent::BackgroundTask {
        agent::BackgroundTask {
            task_id: id.into(),
            run_id: None,
            kind,
            task_type: String::new(),
            description: "untrusted description".into(),
            subagent_type: None,
            parent_task_id: None,
            tool_use_id: None,
            ambient,
            workflow_name: None,
        }
    }

    fn background_tab(label: &str, tasks: Vec<agent::BackgroundTask>) -> (PathBuf, TabSet, TabId) {
        let dir = workspace(label);
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let count = tasks.iter().filter(|task| !task.ambient).count() as u32;
        provider.queue(AgentDomainEvent::BackgroundTasksChanged {
            tasks,
            occurred_at: None,
        });
        provider.queue(AgentDomainEvent::BackgroundCalls { running: count });
        pump_until(&mut set, &dir, "background roster", |s| {
            s.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .background_tasks
                .rows
                .len()
                > 0
                && s.get(tab).unwrap().background_calls() == count
        });
        (dir, set, tab)
    }

    #[test]
    fn close_asks_with_live_background_tasks_and_names_them() {
        let (_dir, mut set, tab) = background_tab(
            "background-close",
            vec![
                task("s1", agent::TaskKind::Shell, false),
                task("s2", agent::TaskKind::Shell, false),
                task("wf", agent::TaskKind::Workflow, false),
                task("ambient", agent::TaskKind::Monitor, true),
            ],
        );
        assert!(!set.get(tab).unwrap().turn_running());
        assert!(set.close_needs_confirm(tab));
        assert_eq!(
            tabs::close_prompt(&set.close_facts(tab).unwrap())[1],
            "2 shells, 1 background dynamic workflow running in the background"
        );
        assert!(set.take_for_handoff(tab).is_none());
        shut_down_all(&mut set);
    }

    #[test]
    fn ambient_tasks_do_not_ask() {
        let (_dir, mut set, tab) =
            background_tab("ambient-close", vec![task("monitor", agent::TaskKind::Monitor, true)]);
        assert!(!set.close_needs_confirm(tab));
        assert_eq!(set.background_count(), 0);
        assert!(set.close_facts(tab).unwrap().background.is_none());
        assert_eq!(
            tabs::window_close_prompt(set.running_count(), set.queued_count(), set.background_count()),
            None
        );
        let retired = set.take_for_handoff(tab).expect("ambient work does not block handoff");
        retired.into_backend().shutdown();
        shut_down_all(&mut set);
    }

    #[test]
    fn window_close_counts_background_tabs() {
        let (_dir, mut set, _) = background_tab(
            "background-window",
            vec![
                task("s1", agent::TaskKind::Shell, false),
                task("s2", agent::TaskKind::Shell, false),
            ],
        );
        set.open(AgentId::Claude);
        assert_eq!(set.background_count(), 1);
        assert_eq!(
            tabs::window_close_prompt(set.running_count(), set.queued_count(), set.background_count()).as_deref(),
            Some("close window? 1 with background tasks (y/n)")
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn close_others_counts_background_tabs() {
        let (_dir, mut set, tab) = background_tab("background-others", vec![task("s1", agent::TaskKind::Shell, false)]);
        assert_eq!(set.close_others_plan(), None);
        set.open(AgentId::Claude);
        assert_eq!(
            set.close_others_plan(),
            Some((vec![tab], "close 1 other tab? 1 with background tasks (y/n)".into()))
        );
        set.select(tab);
        assert_eq!(set.break_out(ViewId::new(2).unwrap()).unwrap(), tab);
        assert_eq!(set.close_others_plan(), None);
        shut_down_all(&mut set);
    }

    #[test]
    fn a_typed_message_waits_behind_a_waiting_question() {
        let (dir, mut set, tab, provider) = waiting("question-send");
        let turn = FirstTurn {
            typed: "next".into(),
            wire: "next with context".into(),
            typed_by_person: true,
        };
        assert!(matches!(
            set.send_prompt(tab, "r1", ViewId::BASE, turn),
            Some(Sent::Prompt { outcome: Ok(()), .. })
        ));
        assert!(provider.turns().is_empty());
        assert!(set.flush_queue(tab).is_none());
        assert!(set.release_after_card().is_empty());
        settle(&provider);
        pump_until(&mut set, &dir, "question settled", |s| {
            s.get(tab).unwrap().attention.attention().pending == 0
        });
        assert_eq!(set.release_after_card().len(), 1);
        assert_eq!(provider.turns(), vec!["next with context"]);
        assert!(provider.interactions().is_empty());
        shut_down_all(&mut set);
    }

    #[test]
    fn a_companion_send_waits_behind_a_waiting_question() {
        let (dir, mut set, tab, provider) = waiting("question-companion");
        set.queue_message(
            tab,
            "companion",
            "companion".into(),
            1,
            false,
            agent::PromptOrigin::Composed,
        )
        .unwrap();
        assert!(set.flush_queue(tab).is_none());
        assert!(provider.turns().is_empty());
        settle(&provider);
        pump_until(&mut set, &dir, "question settled", |s| {
            s.get(tab).unwrap().attention.attention().pending == 0
        });
        assert!(set.flush_queue(tab).is_some());
        assert_eq!(provider.turns(), vec!["companion"]);
        shut_down_all(&mut set);
    }

    #[test]
    fn question_draft_survives_a_tab_switch() {
        let (_dir, mut set, tab, provider) = waiting("question-switch");
        let draft = serde_json::json!({"page": 0, "picks": [1], "text": "typed", "notes": "notes"});
        set.set_question_draft(tab, ViewId::BASE, "q1", draft.clone()).unwrap();
        set.open(AgentId::Claude);
        set.select(tab);
        assert_eq!(set.get(tab).unwrap().view(true).question_draft["q1"], draft);
        let payload: serde_json::Value = serde_json::from_str(&set.tabs_payload()).unwrap();
        assert_eq!(payload["tabs"][0]["questionDraft"]["q1"], draft);
        assert!(provider.interactions().is_empty());
        assert!(set
            .set_question_draft(tab, ViewId::BASE, "q1", serde_json::json!("x".repeat(65536)))
            .is_err());
        shut_down_all(&mut set);
    }

    #[test]
    fn a_typed_answer_edited_in_nvim_is_stored_for_the_dock_to_restore() {
        let (_dir, mut set, tab, _) = waiting("question-other-stored");
        set.set_question_draft(
            tab,
            ViewId::BASE,
            "q1",
            serde_json::json!({"page": 0, "focus": 1, "answers": [{"options": [1], "other": false, "text": "old", "notes": "n", "showNotes": false}]}),
        )
        .unwrap();
        set.store_question_other(tab, "q1", 0, "edited in nvim").unwrap();
        let draft = &set.get(tab).unwrap().view(true).question_draft["q1"];
        assert_eq!(draft["focus"], 1, "the rest of the draft is kept");
        assert_eq!(draft["answers"][0]["text"], "edited in nvim");
        assert_eq!(draft["answers"][0]["other"], true);
        assert_eq!(
            draft["answers"][0]["options"],
            serde_json::json!([]),
            "a typed answer replaces the picked option of a single-select question"
        );
        assert_eq!(draft["answers"][0]["notes"], "n");
        assert!(set.store_question_other(tab, "q1", 1, "x").is_err(), "no such question");
        assert!(set.store_question_other(tab, "q9", 0, "x").is_err(), "not waiting");
        shut_down_all(&mut set);
    }

    #[test]
    fn a_typed_answer_edited_in_nvim_is_stored_without_an_earlier_draft() {
        let (_dir, mut set, tab, _) = waiting("question-other-fresh");
        set.store_question_other(tab, "q1", 0, "typed").unwrap();
        let draft = &set.get(tab).unwrap().view(true).question_draft["q1"];
        assert_eq!(draft["answers"][0]["text"], "typed");
        assert_eq!(draft["answers"][0]["other"], true);
        shut_down_all(&mut set);
    }

    #[test]
    fn the_draft_is_dropped_on_settle() {
        let (dir, mut set, tab, provider) = waiting("question-settle-draft");
        set.set_question_draft(tab, ViewId::BASE, "q1", serde_json::json!({"text":"typed"}))
            .unwrap();
        settle(&provider);
        pump_until(&mut set, &dir, "draft dropped", |s| {
            s.get(tab).unwrap().session.question_draft.is_empty()
        });
        assert!(set
            .set_question_draft(tab, ViewId::BASE, "q1", serde_json::json!({}))
            .is_err());
        shut_down_all(&mut set);
    }

    #[test]
    fn a_moved_tab_keeps_its_question() {
        let (dir, mut source, tab, provider) = waiting("question-move");
        provider.queue(AgentDomainEvent::SessionOpened {
            session_id: "fake-session".into(),
            provider_session_id: "sess-move".into(),
            model: "m".into(),
            cwd: dir.to_string_lossy().into_owned(),
        });
        pump_until(&mut source, &dir, "session id", |s| {
            s.get(tab).unwrap().provider_session_id().is_some()
        });
        source
            .set_question_draft(tab, ViewId::BASE, "q1", serde_json::json!({"text":"kept"}))
            .unwrap();
        let frozen = loop {
            match source.freeze_for_move(tab) {
                Err(MoveRefusedWhy::Folding) => std::thread::sleep(Duration::from_millis(1)),
                result => break result.unwrap(),
            }
        };
        assert!(source
            .decline_question(tab, ViewId::BASE, "q1", agent::QuestionDeclineKind::Cancel, Vec::new())
            .is_err());
        let record = serde_json::from_str(&serde_json::to_string(&frozen.tab).unwrap()).unwrap();
        let chat = frozen.chat.unwrap();
        let backend = AgentBackend::adopt_moved(
            chat.core,
            chat.conversation,
            chat.backend,
            agent::MovedProvider {
                agent: provider.clone(),
                external: None,
                lease: None,
            },
        )
        .unwrap();
        let mut dest = set();
        let arrived = dest.adopt_moved(record, backend, ViewId::BASE);
        assert_eq!(question(&dest, arrived, "q1"), Some(QuestionState::Waiting));
        assert_eq!(
            dest.get(arrived).unwrap().view(true).question_draft["q1"]["text"],
            "kept"
        );
        assert_eq!(dest.get(arrived).unwrap().attention.attention().pending, 1);
        assert!(dest
            .decline_question(
                arrived,
                ViewId::new(2).unwrap(),
                "q1",
                agent::QuestionDeclineKind::Cancel,
                Vec::new()
            )
            .is_err());
        dest.decline_question(
            arrived,
            ViewId::BASE,
            "q1",
            agent::QuestionDeclineKind::Cancel,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(provider.interactions().len(), 1);
        source.hand_over(tab).unwrap();
        shut_down_all(&mut source);
        shut_down_all(&mut dest);
    }

    #[test]
    fn confirm_bypass_leaves_a_waiting_question() {
        let dir = workspace("question-confirm-bypass");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue_all(vec![card("c1"), asked("q1")]);
        pump_until(&mut set, &dir, "the card and the question", |set| {
            set.get(tab).unwrap().attention.attention().pending == 2 && question(set, tab, "q1").is_some()
        });

        assert_eq!(enter_bypass(&mut set, tab), ConfirmOutcome::Entered { approved: 1 });
        assert_eq!(provider.resolutions(), vec![("c1".to_string(), true)], "the card only");
        assert!(provider.interactions().is_empty());
        assert_eq!(question(&set, tab, "q1"), Some(QuestionState::Waiting));
        shut_down_all(&mut set);
    }

    /// `Ctrl+y` and BROWSE's `a`/`d` arrive as a card answer naming an id; a question's id is no card's,
    /// so either is refused and the question waits.
    #[test]
    fn ctrl_y_never_answers_a_question() {
        let dir = workspace("question-ctrl-y");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(asked("q1"));
        pump_until(&mut set, &dir, "the question", |set| question(set, tab, "q1").is_some());

        set.pump(&dir, true);
        for decision in [
            agent::PermissionDecision::Allow,
            agent::PermissionDecision::Deny { reason: None },
        ] {
            assert!(set.answer_card(tab, ViewId::BASE, "q1", decision).is_err());
        }
        assert!(provider.resolutions().is_empty());
        assert!(provider.interactions().is_empty());
        assert_eq!(question(&set, tab, "q1"), Some(QuestionState::Waiting));
        assert_eq!(
            set.get(tab).unwrap().attention.attention().pending,
            1,
            "the question counts without becoming a card"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn a_bypass_resync_and_a_restore_in_bypass_leave_a_question() {
        // The resync: a batch that overflows the page's queue, swept in bypass (`approve_pending`).
        let dir = workspace("question-bypass-resync");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(enter_bypass(&mut set, tab), ConfirmOutcome::Entered { approved: 0 });
        let mut overflow = vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }];
        for i in 0..300 {
            overflow.push(AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: agent::ContentKind::Text,
                text: format!("chunk {i}"),
            });
        }
        overflow.extend([asked("q1"), card("lost1")]);
        provider.queue_all(overflow);
        let deadline = Instant::now() + Duration::from_secs(5);
        let snapshot = loop {
            assert!(Instant::now() < deadline, "timed out waiting for the resync");
            let out = set.pump(&dir, true);
            if let Some(payload) = out.active_payload {
                let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
                if value["kind"] == "snapshot" && value["state"]["questions"].is_array() {
                    break value;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        pump_until(&mut set, &dir, "the swept card", |set| {
            set.get(tab).unwrap().session.host_answered.contains("lost1")
        });
        assert_eq!(provider.resolutions(), vec![("lost1".to_string(), true)]);
        assert!(provider.interactions().is_empty());
        assert_eq!(question(&set, tab, "q1"), Some(QuestionState::Waiting));
        assert_eq!(
            snapshot["state"]["questions"][0]["state"]["kind"], "waiting",
            "{snapshot}"
        );
        shut_down_all(&mut set);

        // A restore whose `y` kept a saved tab in bypass: its session's events meet bypass on arrival.
        let mut set = set_restored_in_bypass();
        let tab = set.active();
        assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Bypass, "the premise");
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue_all(vec![card("c1"), asked("q1")]);
        pump_until(&mut set, &dir, "the card answered and the question folded", |set| {
            set.get(tab).unwrap().session.host_answered.contains("c1") && question(set, tab, "q1").is_some()
        });
        assert_eq!(provider.resolutions(), vec![("c1".to_string(), true)]);
        assert!(provider.interactions().is_empty());
        assert_eq!(question(&set, tab, "q1"), Some(QuestionState::Waiting));
        shut_down_all(&mut set);
    }

    /// A set whose one saved tab, saved in bypass, came back in bypass on the restore question's `y`.
    fn set_restored_in_bypass() -> TabSet {
        let mut set = set();
        let from = set.active();
        let plan = RestorePlan {
            items: vec![PlannedTab {
                agent: AgentId::Claude,
                provider_session_id: "r1".into(),
                name: None,
                title: Some("saved".into()),
                saved_mode: SessionModeChoice::Bypass,
                label: "saved".into(),
                view: None,
                turn_running: false,
            }],
            skipped: Vec::new(),
            active_session: Some("r1".into()),
        };
        let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
            panic!("a tab saved in bypass is asked about");
        };
        let go = set.answer_restore(prompt.nonce, true).unwrap();
        set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
        set
    }

    /// The ungated-mode tripwire stops the tab before anything in its batch is answered, a waiting
    /// question included.
    #[test]
    fn the_ungated_mode_tripwire_answers_no_question() {
        let dir = workspace("question-tripwire");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(enter_bypass(&mut set, tab), ConfirmOutcome::Entered { approved: 0 });
        provider.queue(asked("q1"));
        pump_until(&mut set, &dir, "the question", |set| question(set, tab, "q1").is_some());
        provider.queue_all(vec![
            AgentDomainEvent::UngatedCliMode {
                reported: "bypassPermissions".into(),
                detail: "SessionReady".into(),
            },
            card("behind-the-trip"),
        ]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut tripped = Vec::new();
        while tripped.is_empty() {
            assert!(Instant::now() < deadline, "timed out waiting for the trip");
            tripped.extend(set.pump(&dir, true).tripped);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(provider.interactions().is_empty(), "the question was not answered");
        assert!(provider.resolutions().is_empty(), "nor was anything else");
        for t in tripped {
            t.retired.into_backend().shutdown();
        }
        shut_down_all(&mut set);
    }
}

#[cfg(test)]
mod broker_card_tests {
    //! A broker card is drawn and counted and nothing else: it is in no list the tab set's own sweeps read, so the
    //! paths that answer cards by themselves cannot reach one, and a move does not carry one.
    use super::move_tests::{freeze_settled, take_up};
    use super::*;
    use crate::editor_tools::cards::EditorCardWire;
    use crate::test_providers::RecordingProvider;
    use agent::{AgentConversation, AgentDomainEvent};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const CARD: &str = "ec-0123456789abcdef-1";

    fn editor_card(id: &str) -> EditorCardWire {
        EditorCardWire {
            id: id.to_owned(),
            seq: 7,
            tool: "open_file".to_owned(),
            tool_use_id: Some("toolu_open".to_owned()),
            title: "Show /etc/hosts in your editor?".to_owned(),
            body: Vec::new(),
            proposal: None,
        }
    }

    /// A `Write` into `.git`: a card in Manual and Auto, answered in Bypass.
    fn provider_card(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
            provider_prompt: None,
        }
    }

    fn live(label: &str) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        let provider = Arc::new(RecordingProvider::default());
        let conversation =
            AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        });
        (dir, set, tab, provider)
    }

    fn pump_until(set: &mut TabSet, dir: &Path, what: &str, done: impl Fn(&TabSet) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(set) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            set.pump(dir, true);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn pending(set: &TabSet, tab: TabId) -> usize {
        set.get(tab).unwrap().attention.attention().pending
    }

    fn enter_bypass(set: &mut TabSet, tab: TabId) -> ConfirmOutcome {
        let plan = match set.cycle_mode(tab, ViewId::BASE) {
            Ok(ModeCycle::Confirm(plan)) => plan,
            other => panic!("expected a bypass prompt, got {other:?}"),
        };
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap()
    }

    fn shut_down_all(set: &mut TabSet) {
        for tab in set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn waiting_cards_never_names_a_broker_card() {
        let (dir, mut set, tab, provider) = live("broker-card-waiting");
        provider.queue(provider_card("perm-1"));
        pump_until(&mut set, &dir, "the provider's card", |set| pending(set, tab) == 1);
        set.set_editor_cards(tab, vec![editor_card(CARD)]);
        assert_eq!(pending(&set, tab), 2, "the premise: both wait");
        assert_eq!(waiting_cards(set.get(tab).unwrap()), ["perm-1"]);
        assert!(
            !set.get(tab).unwrap().card_is_waiting(CARD),
            "a provider's answer cannot name it"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn entering_bypass_leaves_a_broker_card_waiting() {
        let (dir, mut set, tab, provider) = live("broker-card-enter-bypass");
        provider.queue(provider_card("perm-1"));
        pump_until(&mut set, &dir, "the provider's card", |set| pending(set, tab) == 1);
        set.set_editor_cards(tab, vec![editor_card(CARD)]);

        assert_eq!(enter_bypass(&mut set, tab), ConfirmOutcome::Entered { approved: 1 });
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), true)]);
        assert_eq!(pending(&set, tab), 1, "the broker's card still counts");
        assert_eq!(set.get(tab).unwrap().session.editor_cards, [editor_card(CARD)]);
        shut_down_all(&mut set);
    }

    #[test]
    fn a_bypass_resync_leaves_a_broker_card_waiting() {
        let (dir, mut set, tab, provider) = live("broker-card-resync");
        assert_eq!(enter_bypass(&mut set, tab), ConfirmOutcome::Entered { approved: 0 });
        set.set_editor_cards(tab, vec![editor_card(CARD)]);
        let mut overflow = vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }];
        for i in 0..300 {
            overflow.push(AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: agent::ContentKind::Text,
                text: format!("chunk {i}"),
            });
        }
        overflow.push(provider_card("lost1"));
        provider.queue_all(overflow);
        let deadline = Instant::now() + Duration::from_secs(5);
        let snapshot = loop {
            assert!(Instant::now() < deadline, "timed out waiting for the resync");
            let out = set.pump(&dir, true);
            if let Some(payload) = out.active_payload {
                let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
                if value["kind"] == "snapshot" && value["state"]["pendingPermissions"].is_array() {
                    break value;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        pump_until(&mut set, &dir, "the swept card", |set| {
            set.get(tab).unwrap().session.host_answered.contains("lost1")
        });
        assert_eq!(provider.resolutions(), vec![("lost1".to_string(), true)]);
        assert_eq!(pending(&set, tab), 1, "the sweep took the provider's card only");
        assert_eq!(snapshot["state"]["editorCards"][0]["id"], CARD, "{snapshot}");
        assert_eq!(set.get(tab).unwrap().session.editor_cards, [editor_card(CARD)]);
        shut_down_all(&mut set);
    }

    #[test]
    fn the_tray_counts_a_broker_card() {
        let (dir, mut set, tab, _provider) = live("broker-card-tray");
        set.pump(&dir, true);
        let before = set.attention().arrived;
        set.set_editor_cards(tab, vec![editor_card(CARD)]);
        let attention = set.attention();
        assert_eq!((attention.pending, attention.arrived), (1, before + 1));
        assert_eq!(
            set.oldest_card_tab(),
            Some(tab),
            "the key for the oldest card reaches it"
        );

        let frozen = freeze_settled(&mut set, tab).expect("the tab freezes");
        let record = serde_json::to_value(&frozen.tab).unwrap();
        assert_eq!(record["attention"]["cards"], serde_json::json!([]));
        assert_eq!(record["attention"]["questions"], serde_json::json!([]));
        shut_down_all(&mut set);
    }

    #[test]
    fn a_cards_mirror_is_not_in_the_call_notes_a_move_carries() {
        let (dir, mut source, tab, provider) = live("broker-card-move");
        source.pump(&dir, true);
        let notes = source.get(tab).unwrap().notes.clone();
        source.set_editor_cards(tab, vec![editor_card(CARD)]);
        assert_eq!(source.get(tab).unwrap().notes, notes);

        let frozen = freeze_settled(&mut source, tab).expect("the tab freezes");
        let record = serde_json::to_string(&frozen.tab).unwrap();
        assert!(!record.contains(CARD), "{record}");

        let mut dest = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let landed = take_up(frozen, &mut dest, provider.clone(), ViewId::BASE);
        assert!(dest.get(landed).unwrap().session.editor_cards.is_empty());
        assert_eq!(pending(&dest, landed), 0);
        shut_down_all(&mut dest);
    }
}

#[cfg(test)]
mod turn_news_tests {
    //! A turn's beginning is reported to the broker once, from the tab's clock.
    use super::*;
    use crate::test_providers::RecordingProvider;
    use agent::{AgentConversation, AgentDomainEvent};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn live(label: &str) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        let provider = Arc::new(RecordingProvider::default());
        let conversation =
            AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        });
        (dir, set, tab, provider)
    }

    fn began(set: &mut TabSet) -> Vec<String> {
        set.take_client_news()
            .into_iter()
            .filter_map(|(_, news)| match news {
                ClientCallNews::TurnBegan { turn_id } => Some(turn_id),
                _ => None,
            })
            .collect()
    }

    /// Pumps, collecting what is reported, until `done` holds of the reports so far.
    fn pump_until_told(set: &mut TabSet, dir: &Path, what: &str, done: impl Fn(&[String]) -> bool) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut told = Vec::new();
        while !done(&told) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            set.pump(dir, true);
            told.extend(began(set));
            std::thread::sleep(Duration::from_millis(5));
        }
        told
    }

    fn pump_a_while(set: &mut TabSet, dir: &Path) -> Vec<String> {
        let mut told = Vec::new();
        for _ in 0..20 {
            set.pump(dir, true);
            told.extend(began(set));
            std::thread::sleep(Duration::from_millis(5));
        }
        told
    }

    fn completed(turn: &str) -> AgentDomainEvent {
        AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        }
    }

    fn editing(id: &str, is_error: bool) -> Vec<AgentDomainEvent> {
        vec![
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: id.into(),
                name: "Edit".into(),
                input: serde_json::json!({"file_path":"main.rs"}),
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: "t1".into(),
                tool_use_id: id.into(),
                content: serde_json::json!("ok"),
                is_error,
            },
        ]
    }

    fn edit_news(set: &mut TabSet, dir: &Path, id: &str) -> Vec<PathBuf> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut paths = Vec::new();
        loop {
            set.pump(dir, true);
            paths.extend(set.take_follow_edits().into_iter().map(|(_, path)| path));
            if set
                .active_tab()
                .live()
                .unwrap()
                .projection()
                .tool_calls
                .iter()
                .any(|c| c.tool_use_id == id && c.result.is_some())
            {
                // The projection can finish folding after this pump took its delivery. Drain that
                // delivery before changing the tab's following flag.
                set.pump(dir, true);
                paths.extend(set.take_follow_edits().into_iter().map(|(_, path)| path));
                return paths;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn follow_edits_are_independent_of_client_call_news() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        let calls = &mut set.get_mut(tab).unwrap().session.client_calls;
        calls.edited.push(PathBuf::from("main.rs"));
        calls.news.push(ClientCallNews::TurnInterrupted);
        assert_eq!(set.take_client_news(), [(tab, ClientCallNews::TurnInterrupted)]);
        assert_eq!(set.take_follow_edits(), [(tab, PathBuf::from("main.rs"))]);
        assert!(set.take_follow_edits().is_empty());
    }

    #[test]
    fn a_leaving_tab_discards_unreported_follow_edits() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        set.get_mut(tab)
            .unwrap()
            .session
            .client_calls
            .edited
            .push(PathBuf::from("main.rs"));
        assert!(set.take_unreported_client_calls(tab).is_empty());
        assert!(set.take_follow_edits().is_empty());
    }

    #[test]
    fn an_edit_is_news_only_while_following() {
        let (dir, mut set, tab, provider) = live("edit-news");
        assert!(!set.get(tab).unwrap().following);
        provider.queue_all(editing("off", false));
        assert!(edit_news(&mut set, &dir, "off").is_empty());
        set.get_mut(tab).unwrap().following = true;
        provider.queue_all(editing("on", false));
        assert_eq!(edit_news(&mut set, &dir, "on"), [PathBuf::from("main.rs")]);
    }

    #[test]
    fn a_failed_edit_is_no_news() {
        let (dir, mut set, tab, provider) = live("failed-edit-news");
        set.get_mut(tab).unwrap().following = true;
        provider.queue_all(editing("failed", true));
        assert!(edit_news(&mut set, &dir, "failed").is_empty());
    }

    #[test]
    fn a_turn_start_is_news_once() {
        let (dir, mut set, tab, provider) = live("turn-news-once");
        assert!(pump_a_while(&mut set, &dir).is_empty(), "no turn, no news");
        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        let told = pump_until_told(&mut set, &dir, "the turn's news", |told| !told.is_empty());
        assert_eq!(told, ["t1"]);
        assert!(
            pump_a_while(&mut set, &dir).is_empty(),
            "a turn running on is not news again"
        );

        provider.queue(completed("t1"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while set
            .get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .active_turn_id
            .is_some()
        {
            assert!(Instant::now() < deadline, "timed out waiting for the turn to end");
            set.pump(&dir, true);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(pump_a_while(&mut set, &dir).is_empty(), "an ending is no news");

        provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
        let told = pump_until_told(&mut set, &dir, "the next turn's news", |told| !told.is_empty());
        assert_eq!(told, ["t2"]);
        for tab in set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// A resync drops the events a pump would have read, and the clock is stamped from the projection all the same.
    #[test]
    fn a_turn_seen_only_through_a_resync_is_news() {
        let (dir, mut set, tab, provider) = live("turn-news-resync");
        let mut flood = vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }];
        for i in 0..300 {
            flood.push(AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: agent::ContentKind::Text,
                text: format!("chunk {i}"),
            });
        }
        provider.queue_all(flood);
        // One hold of the lock folds the whole flood, so a projection naming the turn has overflowed too. A tick that
        // drained before the fold and read the projection after it would stamp the clock a tick ahead of the snapshot.
        let deadline = Instant::now() + Duration::from_secs(5);
        while set
            .get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .active_turn_id
            .is_none()
        {
            assert!(Instant::now() < deadline, "timed out waiting for the fold");
            std::thread::sleep(Duration::from_millis(5));
        }
        let out = set.pump(&dir, true);
        let payload = out.active_payload.expect("the tab is on screen");
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["kind"], "snapshot", "the events were delivered as a snapshot");
        assert_eq!(began(&mut set), ["t1"]);
        assert!(pump_a_while(&mut set, &dir).is_empty(), "and it is reported once");
        for tab in set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }
}

#[cfg(test)]
mod prompt_origin_tests {
    use super::*;
    use crate::test_providers::RecordingProvider;
    use std::time::{Duration, Instant};

    struct Fixture {
        dir: PathBuf,
        set: TabSet,
        tab: TabId,
        provider: Arc<RecordingProvider>,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            agent::state_dirs::redirect_state_to_a_test_root();
            let dir = agent::state_dirs::test_workspace_dir(label);
            let provider = Arc::new(RecordingProvider::steering());
            let conversation = agent::AgentConversation::create(
                provider.clone(),
                &dir,
                agent::setting_sources::ProjectTrust::Untrusted,
            )
            .unwrap();
            let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
            let tab = set.active();
            set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            });
            Self {
                dir,
                set,
                tab,
                provider,
            }
        }

        fn wait(&mut self, mut done: impl FnMut(&Tab) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                self.set.pump(&self.dir, true);
                if done(self.set.get(self.tab).unwrap()) {
                    return;
                }
                assert!(Instant::now() < deadline, "the tab did not reach the expected state");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn prompt(&mut self, text: &str, origin: PromptOrigin, first: bool) {
            let turn = FirstTurn {
                wire: format!("editor context\n\n{text}"),
                typed: text.into(),
                typed_by_person: origin == PromptOrigin::Typed,
            };
            let sent = if first {
                self.set.send_first_turn(self.tab, "first", turn)
            } else {
                self.set.send_prompt(self.tab, "prompt", ViewId::BASE, turn)
            };
            assert!(matches!(
                sent,
                Some(Sent::First { outcome: Ok(()), .. } | Sent::Prompt { outcome: Ok(()), .. })
            ));
        }

        fn start(&mut self, cli: bool) {
            let mut events = vec![AgentDomainEvent::TurnStarted {
                turn_id: "turn-1".into(),
            }];
            if cli {
                events.push(AgentDomainEvent::TurnAttributed {
                    turn_id: "turn-1".into(),
                    origin: agent::TurnOrigin::Cli,
                    cause: None,
                });
            }
            self.provider.queue_all(events);
            self.wait(Tab::turn_running);
        }

        fn end(&mut self) {
            self.provider.queue(AgentDomainEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                outcome: agent::TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            });
            self.wait(|tab| !tab.turn_running());
        }

        fn cycle(&mut self) -> String {
            let id = self
                .provider
                .turn_stamps()
                .last()
                .unwrap()
                .client_message_id
                .clone()
                .unwrap();
            self.provider.queue(AgentDomainEvent::CliCycleStarted {
                turn_id: "turn-1".into(),
                cause: None,
            });
            self.wait(|tab| tab.session.queue.len() == 1);
            id
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for removed in self.set.take_all() {
                if let Some(retired) = removed.retired {
                    retired.into_backend().shutdown();
                }
            }
        }
    }

    #[test]
    fn a_held_steer_goes_into_a_cli_turn() {
        let mut f = Fixture::new("held-steer-cli-turn");
        f.provider.refuse_sends(true);
        f.prompt("and the tests", PromptOrigin::Typed, false);
        assert!(f.set.get(f.tab).unwrap().session.queue[0].steer_when_running);
        f.start(true);
        f.wait(|tab| tab.steered_unconfirmed() == 1);
        let sent = f.provider.steers();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].expected_turn_id, "turn-1");
        assert_eq!(sent[0].as_typed, "and the tests");
        assert!(f.provider.steer_stamps()[0].typed_by_person);
    }

    #[test]
    fn a_case_2_prompt_becomes_a_steered_queue_line() {
        let mut f = Fixture::new("opening-prompt-cli-cycle");
        f.prompt("fix the parser", PromptOrigin::Typed, false);
        f.start(false);
        assert_eq!(
            f.set
                .get(f.tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .user_prompts
                .len(),
            1
        );
        let id = f.cycle();
        let tab = f.set.get(f.tab).unwrap();
        let queue: serde_json::Value = serde_json::from_str(&crate::agent_bridge::serialize_queue_for_js(
            f.tab,
            &tab.session.queue,
            None,
            None,
            tab.steers(),
        ))
        .unwrap();
        assert_eq!(queue["items"].as_array().unwrap().len(), 1);
        assert_eq!(queue["items"][0]["steered"], true);
        assert_eq!(queue["items"][0]["text"], "fix the parser");
        assert!(tab.live().unwrap().projection().user_prompts.is_empty());
        assert!(matches!(&tab.session.queue[0].state, QueuedState::Steered { client_id, .. } if client_id == &id));
        f.provider.queue(AgentDomainEvent::ContentDelta {
            turn_id: "turn-1".into(),
            kind: agent::ContentKind::Text,
            text: "background work first".into(),
        });
        f.provider.queue(AgentDomainEvent::UserMessageDelivered {
            client_message_id: id,
            turn_id: "turn-1".into(),
            text: "fix the parser".into(),
        });
        f.wait(|tab| tab.session.queue.is_empty());
        let projection = f.set.get(f.tab).unwrap().live().unwrap().projection();
        assert_eq!(projection.user_prompts.len(), 1);
        assert_eq!(projection.user_prompts[0].text, "fix the parser");
        assert!(projection.user_prompts[0].delivered);
        assert!(projection.user_prompts[0].seq > projection.transcript[0].seq);
    }

    #[test]
    fn a_dropped_prompt_returns_to_the_queue_with_its_reason() {
        let mut f = Fixture::new("opening-prompt-dropped");
        f.prompt("fix it", PromptOrigin::Typed, false);
        f.start(false);
        let id = f.cycle();
        f.provider.queue(AgentDomainEvent::UserMessageDropped {
            client_message_id: id.clone(),
            reason: "Claude cancelled it".into(),
        });
        f.wait(|tab| tab.session.queue[0].is_held());
        let tab = f.set.get(f.tab).unwrap();
        assert_eq!(tab.session.queue[0].text, "fix it");
        assert_eq!(tab.session.queue[0].dropped_as, [id.clone()]);
        assert_eq!(
            tab.session.queue_note.as_deref(),
            Some("held until the turn ends: Claude cancelled it")
        );
        f.set.steer_held(f.tab, 2, Some("turn-1"));
        assert!(f.provider.steers().is_empty());
        f.provider.queue(AgentDomainEvent::UserMessageDelivered {
            client_message_id: id,
            turn_id: "turn-1".into(),
            text: "fix it".into(),
        });
        f.wait(|tab| tab.session.queue.is_empty());
    }

    #[test]
    fn an_idle_prompt_the_cli_refuses_stays_one_row_and_is_never_sent_again() {
        let mut f = Fixture::new("opening-prompt-refused-idle");
        f.prompt("/unknown-command", PromptOrigin::Typed, false);
        f.start(false);
        let id = f.provider.turn_stamps()[0].client_message_id.clone().unwrap();
        f.provider.queue_all(vec![
            AgentDomainEvent::UserMessageDropped {
                client_message_id: id,
                reason: agent::CLI_REFUSED_DROP.into(),
            },
            AgentDomainEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                outcome: agent::TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            },
        ]);
        let mut ended = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while ended.is_empty() {
            ended.extend(f.set.pump(&f.dir, true).turn_ended);
            assert!(Instant::now() < deadline, "the turn's end was never reported");
            std::thread::sleep(Duration::from_millis(5));
        }
        let tab = f.set.get(f.tab).unwrap();
        assert!(
            tab.session.queue.is_empty(),
            "a prompt still shown as a row is not also a queue line"
        );
        assert_eq!(tab.live().unwrap().projection().user_prompts.len(), 1);
        assert!(f.set.flush_queue(f.tab).is_none(), "nothing is left to send");
        assert_eq!(f.provider.turn_stamps().len(), 1, "the refused prompt went out once");
    }

    #[test]
    fn a_prompt_the_cli_refuses_after_a_cycle_waits_for_the_person() {
        let mut f = Fixture::new("opening-prompt-refused-cycle");
        f.prompt("/unknown-command", PromptOrigin::Typed, false);
        f.start(false);
        let id = f.cycle();
        f.provider.queue(AgentDomainEvent::UserMessageDropped {
            client_message_id: id,
            reason: agent::CLI_REFUSED_DROP.into(),
        });
        f.wait(|tab| tab.session.queue[0].is_held());
        f.end();
        let tab = f.set.get(f.tab).unwrap();
        assert!(tab.session.queue[0].refused);
        assert_eq!(
            tab.session.queue_note.as_deref(),
            Some("Claude refused it; it is not sent again until you take it back")
        );
        assert!(
            f.set.flush_queue(f.tab).is_none(),
            "a refused message is not sent again"
        );
        assert_eq!(f.provider.turn_stamps().len(), 1);
        assert_eq!(f.set.take_back_queue(f.tab), ["/unknown-command"]);
        assert!(f.set.get(f.tab).unwrap().session.queue.is_empty());
    }

    #[test]
    fn a_refused_message_stays_behind_when_the_rest_of_the_queue_goes() {
        let mut f = Fixture::new("opening-prompt-refused-rest");
        f.prompt("/unknown-command", PromptOrigin::Typed, false);
        f.start(false);
        let id = f.cycle();
        f.provider.queue(AgentDomainEvent::UserMessageDropped {
            client_message_id: id,
            reason: agent::CLI_REFUSED_DROP.into(),
        });
        f.wait(|tab| tab.session.queue[0].is_held());
        f.end();
        f.set
            .queue_message(
                f.tab,
                "and the docs",
                "and the docs".into(),
                2,
                true,
                PromptOrigin::Typed,
            )
            .unwrap();
        let flush = f.set.flush_queue(f.tab).expect("the new message goes out");
        assert_eq!(flush.typed, "and the docs");
        let queue = &f.set.get(f.tab).unwrap().session.queue;
        assert_eq!(queue.len(), 1);
        assert!(queue[0].refused);
    }

    #[test]
    fn a_refused_message_does_not_hold_back_a_steer_typed_after_it() {
        let mut f = Fixture::new("opening-prompt-refused-steer");
        f.prompt("/unknown-command", PromptOrigin::Typed, false);
        f.start(false);
        let id = f.cycle();
        f.provider.queue(AgentDomainEvent::UserMessageDropped {
            client_message_id: id,
            reason: agent::CLI_REFUSED_DROP.into(),
        });
        f.wait(|tab| tab.session.queue[0].is_held());
        assert!(f.set.get(f.tab).unwrap().turn_running());
        f.set
            .queue_message(
                f.tab,
                "and the docs",
                "and the docs".into(),
                2,
                true,
                PromptOrigin::Typed,
            )
            .unwrap();
        f.set.steer_held(f.tab, 3, Some("turn-1"));
        let steers = f.provider.steers();
        assert_eq!(
            steers.len(),
            1,
            "the message typed after the refused one goes into the turn"
        );
        assert_eq!(steers[0].as_typed, "and the docs");
        let tab = f.set.get(f.tab).unwrap();
        assert_eq!(tab.session.queue.len(), 2);
        assert!(tab.session.queue[0].refused && tab.session.queue[0].is_held());
        assert!(matches!(tab.session.queue[1].state, QueuedState::Steered { .. }));
        assert_eq!(
            tab.session.queue_note.as_deref(),
            Some("Claude refused it; it is not sent again until you take it back"),
            "the note still says why the refused message stays"
        );
    }

    #[test]
    fn an_opening_prompt_survives_a_cycle_snapshot_and_resync_without_a_second_copy() {
        for overflow in [false, true] {
            let mut f = Fixture::new("opening-prompt-snapshot");
            f.prompt("keep this prompt", PromptOrigin::Typed, false);
            f.start(false);
            let id = f.provider.turn_stamps()[0].client_message_id.clone().unwrap();
            let mut batch = vec![AgentDomainEvent::CliCycleStarted {
                turn_id: "turn-1".into(),
                cause: None,
            }];
            if overflow {
                batch.extend(
                    (0..agent::UI_EVENT_QUEUE_CAPACITY + 10)
                        .map(|_| AgentDomainEvent::CommandsAvailable { commands: vec![] }),
                );
            }
            let through = f.set.get(f.tab).unwrap().live().unwrap().projection().last_revision + batch.len() as u64;
            f.provider.queue_all(batch);
            let deadline = Instant::now() + Duration::from_secs(5);
            while f.set.get(f.tab).unwrap().live().unwrap().projection().last_revision < through {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            if overflow {
                let output = f.set.pump(&f.dir, true);
                let payload: serde_json::Value =
                    serde_json::from_str(output.active_payload.as_deref().unwrap()).unwrap();
                assert_eq!(payload["kind"], "snapshot");
                assert!(output.queue_changed.contains(&f.tab));
            }
            let payloads: Vec<serde_json::Value> = f
                .set
                .active_state_payloads()
                .iter()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect();
            let snapshot = payloads.iter().find(|v| v["kind"] == "snapshot").unwrap();
            let queue = payloads.iter().find(|v| v["kind"] == "queue").unwrap();
            assert!(snapshot["state"]["userPrompts"].as_array().unwrap().is_empty());
            assert_eq!(queue["items"].as_array().unwrap().len(), 1);
            assert_eq!(queue["items"][0]["steered"], true);
            f.set.pump(&f.dir, true);
            assert_eq!(f.set.get(f.tab).unwrap().session.queue.len(), 1);
            f.provider.queue(AgentDomainEvent::UserMessageDelivered {
                client_message_id: id,
                turn_id: "turn-1".into(),
                text: "keep this prompt".into(),
            });
            f.wait(|tab| tab.session.queue.is_empty());
            assert_eq!(
                f.set
                    .get(f.tab)
                    .unwrap()
                    .live()
                    .unwrap()
                    .projection()
                    .user_prompts
                    .len(),
                1
            );
        }
    }

    #[test]
    fn composer_send_and_steer_are_typed() {
        for first in [false, true] {
            let mut f = Fixture::new("typed-composer-opening");
            f.prompt("hello", PromptOrigin::Typed, first);
            let stamp = &f.provider.turn_stamps()[0];
            assert!(stamp.typed_by_person);
            assert_eq!(stamp.as_typed.as_deref(), Some("hello"));
            assert!(stamp.client_message_id.is_some());
            f.start(false);
            f.set
                .queue_message(
                    f.tab,
                    "also this",
                    "ctx: also this".into(),
                    1,
                    true,
                    PromptOrigin::Typed,
                )
                .unwrap();
            assert!(f.set.steer_held(f.tab, 2, Some("turn-1")));
            assert!(f.provider.steer_stamps()[0].typed_by_person);
        }
    }

    #[test]
    fn send_now_and_delayed_composer_queue_are_typed() {
        for running in [false, true] {
            let mut f = Fixture::new("typed-send-now");
            if running {
                f.start(true);
            }
            f.set.send_now(f.tab, "send now", "ctx: send now".into(), 1).unwrap();
            if running {
                assert!(f.provider.turn_stamps().is_empty());
                f.end();
                assert!(f.set.flush_queue(f.tab).unwrap().outcome.is_ok());
            }
            assert!(f.provider.turn_stamps()[0].typed_by_person);
        }
        let mut f = Fixture::new("typed-delayed-queue");
        f.start(true);
        f.set
            .queue_message(f.tab, "later", "ctx: later".into(), 1, true, PromptOrigin::Typed)
            .unwrap();
        f.end();
        assert!(f.set.flush_queue(f.tab).unwrap().outcome.is_ok());
        assert!(f.provider.turn_stamps()[0].typed_by_person);
    }

    #[test]
    fn turn_review_and_plugin_sends_are_composed() {
        let mut f = Fixture::new("composed-review");
        assert!(f
            .set
            .send_review(f.tab, "review of the changes", 1)
            .unwrap()
            .unwrap()
            .outcome
            .is_ok());
        assert!(!f.provider.turn_stamps()[0].typed_by_person);
        let mut f = Fixture::new("composed-plugin-send");
        f.prompt("a plugin's prepared prompt", PromptOrigin::Composed, false);
        assert!(!f.provider.turn_stamps()[0].typed_by_person);
        f.start(true);
        f.set
            .queue_message(
                f.tab,
                "prepared follow-up",
                "wire follow-up".into(),
                1,
                true,
                PromptOrigin::Composed,
            )
            .unwrap();
        f.set.steer_held(f.tab, 2, Some("turn-1"));
        assert!(!f.provider.steer_stamps()[0].typed_by_person);
    }

    #[test]
    fn a_queue_containing_a_review_is_not_stamped_as_the_persons_text() {
        let mut f = Fixture::new("mixed-origin-queue");
        f.start(true);
        assert!(f.set.send_review(f.tab, "generated review", 1).unwrap().is_none());
        f.set
            .queue_message(
                f.tab,
                "my follow-up",
                "my follow-up".into(),
                2,
                true,
                PromptOrigin::Typed,
            )
            .unwrap();
        f.end();
        let flush = f.set.flush_queue(f.tab).unwrap();
        assert!(flush.outcome.is_ok());
        assert_eq!(flush.typed, "generated review\n\nmy follow-up");
        assert!(!f.provider.turn_stamps()[0].typed_by_person);
    }

    #[test]
    fn a_restored_queue_is_not_typed() {
        let mut f = Fixture::new("composed-restored-queue");
        f.set
            .queue_message(
                f.tab,
                "a saved message",
                "ctx: saved".into(),
                1,
                true,
                PromptOrigin::Typed,
            )
            .unwrap();
        let json = serde_json::to_string(&f.set.get(f.tab).unwrap().session.queue).unwrap();
        f.set.get_mut(f.tab).unwrap().session.queue = serde_json::from_str(&json).unwrap();
        assert!(f.set.flush_queue(f.tab).unwrap().outcome.is_ok());
        assert!(!f.provider.turn_stamps()[0].typed_by_person);
    }

    #[test]
    fn a_restored_message_the_person_resends_is_typed() {
        let mut f = Fixture::new("restored-queue-resend");
        f.set
            .get_mut(f.tab)
            .unwrap()
            .session
            .queue
            .push(Queued::held("saved", "old context".into(), 1));
        let text = f.set.take_back_queue(f.tab);
        assert_eq!(text, ["saved"]);
        f.prompt(&text.join("\n\n"), PromptOrigin::Typed, false);
        assert!(f.provider.turn_stamps()[0].typed_by_person);
    }

    #[test]
    fn a_composer_message_held_behind_a_card_keeps_its_origin() {
        for origin in [PromptOrigin::Typed, PromptOrigin::Composed] {
            let mut f = Fixture::new("prompt-origin-card-wait");
            f.provider.queue(AgentDomainEvent::PermissionRequested {
                permission_id: "card".into(),
                tool_use_id: Some("call".into()),
                tool_name: "Write".into(),
                input: serde_json::json!({"file_path": ".git/probe", "content": "x"}),
                provider_prompt: None,
            });
            f.wait(|tab| tab.card_is_waiting("card"));
            f.prompt("after the card", origin, false);
            assert!(f.provider.turn_stamps().is_empty());
            f.provider.queue(AgentDomainEvent::PermissionResolved {
                permission_id: "card".into(),
                outcome: agent::PermissionOutcome::Allowed,
            });
            f.wait(|tab| !tab.card_is_waiting("card"));
            let sent = f.set.release_after_card();
            assert_eq!(sent.len(), 1);
            assert!(sent[0].1.outcome.is_ok());
            assert_eq!(
                f.provider.turn_stamps()[0].typed_by_person,
                origin == PromptOrigin::Typed
            );
        }
    }

    #[test]
    fn ctrl_c_interrupts_a_cli_turn() {
        let mut f = Fixture::new("interrupt-cli-turn");
        f.start(true);
        f.set.get_mut(f.tab).unwrap().live_mut().unwrap().interrupt().unwrap();
        assert_eq!(f.provider.interrupts(), 1);
    }

    #[test]
    fn close_asks_during_a_cli_turn() {
        let mut f = Fixture::new("close-cli-turn");
        f.start(true);
        assert!(f.set.close_needs_confirm(f.tab));
        assert!(f.set.close_facts(f.tab).unwrap().turn_running);
    }

    #[test]
    fn handoff_refused_during_a_cli_turn() {
        let mut f = Fixture::new("handoff-cli-turn");
        f.start(true);
        assert!(f.set.take_for_handoff(f.tab).is_none());
        assert!(f.set.get(f.tab).unwrap().turn_running());
        assert_eq!(f.provider.closes(), 0);
    }

    #[test]
    fn the_queue_holds_during_a_cli_turn() {
        let mut f = Fixture::new("queue-cli-turn");
        f.start(true);
        f.set
            .queue_message(f.tab, "after this", "after this".into(), 1, true, PromptOrigin::Typed)
            .unwrap();
        assert!(f.set.flush_queue(f.tab).is_none());
        assert!(f.provider.turns().is_empty());
        assert_eq!(f.set.queue_texts(f.tab), ["after this"]);
    }

    fn send_calls(code: &str) -> Vec<String> {
        use crate::source_scan::word_ends;
        let mut found = Vec::new();
        for name in [
            "send_turn",
            "send_turn_in",
            "send_turn_as",
            "send_turn_with",
            "send_turn_stamped",
            "steer",
            "steer_turn",
            "steer_turn_stamped",
        ] {
            for end in word_ends(code, name) {
                let before = code[..end - name.len()].trim_end();
                if !(before.ends_with('.') || before.ends_with("::")) || !code[end..].trim_start().starts_with('(') {
                    continue;
                }
                let open = end + code[end..].find('(').unwrap();
                let mut depth = 0;
                let close = code[open..]
                    .char_indices()
                    .find_map(|(at, c)| {
                        match c {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            _ => {}
                        }
                        (depth == 0).then_some(open + at)
                    })
                    .expect("balanced call arguments");
                found.push(
                    code[end - name.len()..=close]
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect(),
                );
            }
        }
        found.sort();
        found
    }

    #[test]
    fn every_send_caller_names_its_origin() {
        use crate::source_scan::{code_only, modules, require_anchors, rust_sources, without, word_ends};
        let sources = rust_sources(&["core/src", "panel/src", "shell/src"]);
        require_anchors(
            &sources,
            &[
                "fn send_one",
                "fn release_turn_send",
                "fn send_turn_in",
                "fn steer_held",
            ],
        );
        let mut actual = Vec::new();
        for source in &sources {
            if ["core/src/test_providers.rs", "core/src/tab_set/move_tests.rs"].contains(&source.path.as_str()) {
                continue;
            }
            let code = code_only(&source.text);
            let names: Vec<&str> = word_ends(&code, "mod")
                .iter()
                .map(|end| {
                    code[*end..]
                        .trim_start()
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .next()
                        .unwrap()
                })
                .filter(|name| name.ends_with("tests"))
                .collect();
            let bodies: Vec<&str> = names.iter().flat_map(|name| modules(&code, name)).collect();
            let production = without(&code, &bodies);
            actual.extend(
                send_calls(&production)
                    .into_iter()
                    .map(|call| (source.path.as_str(), call)),
            );
        }
        let mut expected: Vec<(&str, String)> = [
            ("core/src/agent_backend.rs", "send_turn(wire_text)"),
            ("core/src/agent_backend.rs", "send_turn(wire_text,as_typed,origin)"),
            (
                "core/src/agent_backend.rs",
                "send_turn_as(wire_text,as_typed,origin,&client_message_id)",
            ),
            ("core/src/agent_backend.rs", "send_turn_with(wire_text,policy)"),
            (
                "core/src/agent_backend.rs",
                "steer_turn(shown_turn,client_message_id,wire_text,as_typed,origin)",
            ),
            (
                "core/src/tab_set.rs",
                "send_turn_in(&wire,&typed,mode,root.as_deref(),origin)",
            ),
            (
                "core/src/tab_set.rs",
                "send_turn_in(&turn.wire,&turn.typed,mode,root.as_deref(),turn.origin())",
            ),
            (
                "core/src/tab_set.rs",
                "steer(turn,&client_id,&item.wire,&item.text,item.origin())",
            ),
        ]
        .into_iter()
        .map(|(path, call)| (path, call.to_string()))
        .collect();
        actual.sort();
        expected.sort();
        assert_eq!(
            actual, expected,
            "every new send or steer caller must name how its prompt was written"
        );
        assert_eq!(
            send_calls("backend\n . send_turn_in (\n wire, typed, mode,\n origin\n )"),
            ["send_turn_in(wire,typed,mode,origin)"]
        );
        assert_eq!(
            send_calls("AgentBackend::send_turn(&mut backend, wire, typed)"),
            ["send_turn(&mutbackend,wire,typed)"]
        );
        assert_eq!(
            send_calls("provider.send_turn_stamped(request, stamp)"),
            ["send_turn_stamped(request,stamp)"]
        );
    }
}
