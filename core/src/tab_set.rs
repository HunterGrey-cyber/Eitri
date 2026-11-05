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

mod agent_rows;
pub use agent_rows::{AgentWatch, RowContext, RowKey};

mod live_edits;
pub use live_edits::LiveEdit;
mod moving;
mod shell;
pub use moving::{FrozenChat, FrozenTab, MoveRefusedWhy, SendGate, Sent, TabHandoff};
pub use shell::{shell_command, ShellEnded};

#[cfg(test)]
mod broker_card_tests;
#[cfg(test)]
mod external_tab_tests;
#[cfg(test)]
mod move_tests;
#[cfg(test)]
mod prompt_origin_tests;
#[cfg(test)]
mod question_gate_tests;
#[cfg(test)]
mod steer_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod turn_news_tests;

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
    /// A `!` line the person entered: the session runs it instead of sending these words. Only the
    /// composer's `run_shell` sets it, and like [`Queued::shell_line`] it does not survive being saved.
    #[serde(skip)]
    pub shell_line: bool,
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
    /// First sightings of waiting items and the current summary state, owned by this tab.
    pub agent_watch: AgentWatch,
    /// The tab's name for its whole life, here and in any window it moves to. Private so that only the set gives a
    /// tab the key a move or a saved record brings; [`Tab::row_key`] reads it.
    row_key: RowKey,
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
    live_edits: Vec<live_edits::QueuedLiveEdit>,
    live_edits_on: bool,
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

    /// The tab's name for its whole life: minted with the tab, kept through a move, a break into another window and
    /// a restart.
    pub fn row_key(&self) -> &RowKey {
        &self.row_key
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
            agent_watch: AgentWatch::default(),
            row_key: RowKey::new(),
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
    /// start the panel never folded). `turn_running` does not say so, and a close and a handoff each
    /// need to.
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
    live_edits: bool,
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
            live_edits: false,
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
        calls.live_edits.clear();
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
        let mut tab = Tab::new(id, number, mode, agent, self.following_default);
        tab.session.client_calls.live_edits_on = self.live_edits;
        self.tabs.push(tab);
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
            tab.session.client_calls.live_edits_on = self.live_edits;
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
            live_edits: tab.session.client_calls.live_edits_on,
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
        live_edits::collect(live, &events);
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

    /// [`Self::running_count`] with the tabs whose turn Claude Code started by itself, whose start was never folded: what
    /// a window's summary says runs, since a close ends those turns too (the tab close counts them, `close_needs_confirm`).
    pub fn running_or_cli_busy_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| {
                matches!(t.backend, TabBackend::Starting(_))
                    || t.turn_running()
                    || t.question_waiting()
                    || t.cli_unseen_busy()
            })
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
                        message.shell_report = origin == PromptOrigin::ShellLine;
                        message.steerable = !message.shell_report;
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
                let outcome = if turn.shell_line {
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
                if self.card_waits(id) || self.shell_line_runs(id) {
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
                        draft: crate::saved_tabs::draft_to_save(tab.draft()),
                        row_key: Some(tab.row_key.as_str().to_string()),
                    },
                ))
            })
            .collect();
        let mut others_active = external.iter().position(|(id, _)| *id == self.active);
        let external_ids: Vec<(TabId, (AgentId, String))> = external
            .iter()
            .map(|(id, saved)| (*id, (saved.agent.clone(), saved.provider_session_id.clone())))
            .collect();
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
                draft: granted.planned.draft.clone(),
                row_key: granted.planned.row_key.clone(),
            });
        }
        // A skipped tab's draft has no tab to wait in until the answer comes: the file keeps the entry until then.
        let skipped_with_a_draft: Vec<SavedTab> = self
            .waiting_restore_skipped()
            .into_iter()
            .map(|tab| SavedTab {
                conversation_id: conversation_id.to_string(),
                ..tab.clone()
            })
            .collect();
        others.extend(skipped_with_a_draft.iter().filter(|t| t.agent.is_external()).cloned());
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
                        draft: crate::saved_tabs::draft_to_save(tab.draft()),
                        row_key: Some(tab.row_key.as_str().to_string()),
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
                    draft: granted.planned.draft.clone(),
                    row_key: granted.planned.row_key.clone(),
                },
            ));
        }
        listed.extend(
            skipped_with_a_draft
                .into_iter()
                .filter(|t| !t.agent.is_external())
                .map(|t| (TabId(u64::MAX), t)),
        );
        // The tab bar's order across both kinds: the tabs that exist by their place in it, then the sessions a
        // restore still waits to start, in the order they were listed.
        let mut bar: Vec<(usize, (AgentId, String))> = listed
            .iter()
            .filter(|(id, _)| *id != TabId(u64::MAX))
            .map(|(id, saved)| (id, saved.key()))
            .chain(external_ids.iter().map(|(id, key)| (id, key.clone())))
            .filter_map(|(id, key)| Some((self.position(*id)?, key)))
            .collect();
        bar.sort_by_key(|(place, _)| *place);
        let mut bar: Vec<(AgentId, String)> = bar.into_iter().map(|(_, key)| key).collect();
        for granted in &waiting {
            let key = (
                granted.planned.agent.clone(),
                granted.planned.provider_session_id.clone(),
            );
            if !bar.contains(&key) {
                bar.push(key);
            }
        }
        let mixed = !listed.is_empty() && !others.is_empty();
        let saved = SavedTabs::from_parts(
            listed.into_iter().map(|(_, saved)| saved).collect(),
            active,
            others,
            others_active,
        );
        Some(if mixed { saved.with_bar(bar) } else { saved })
    }

    /// Puts a saved draft in a tab's composer, after what the tab holds already.
    fn give_restored_draft(&mut self, id: TabId, draft: Option<String>) {
        let (Some(draft), Some(tab)) = (draft, self.get_mut(id)) else {
            return;
        };
        let text = match tab.draft().trim().is_empty() {
            true => draft,
            false => format!("{}\n\n{draft}", tab.draft()),
        };
        tab.replace_draft(text);
    }

    /// Tabs whose unsent text would go with the window: a tab that is not saved (it has no session yet, or `saving`
    /// says the window keeps no tabs), or text too long for the file. The window's close asks about these.
    ///
    /// `held` is the words a tab has that are not in its draft yet (a message waiting for the editor), which a close
    /// would put in the draft, after them what was typed since: the tab counts once.
    ///
    /// Counted without building the text: a panel's tick reads this, and a draft can be long.
    pub fn unsaved_draft_count(&self, saving: bool, held: &std::collections::BTreeMap<TabId, String>) -> usize {
        self.tabs
            .iter()
            .filter(|tab| {
                let draft = tab.draft();
                let typed = !draft.trim().is_empty();
                // What a close would put in the draft: the held words, then what was typed since, a blank line between.
                let (blank, len) = match held.get(&tab.id) {
                    Some(held) if typed => (false, held.len() + 2 + draft.len()),
                    Some(held) => (held.trim().is_empty(), held.len()),
                    None => (!typed, draft.len()),
                };
                !blank && (!saving || !self.saves(tab) || len > crate::saved_tabs::DRAFT_SAVE_MAX)
            })
            .count()
    }

    /// Whether [`Self::saved_snapshot`] lists `tab`.
    fn saves(&self, tab: &Tab) -> bool {
        let kept = match tab.agent.is_external() {
            true => tab.resumable(self.kind) || tab.failed_resume.is_some(),
            false => tab.agent == AgentId::Claude && self.kind == BackendKind::Sidecar,
        };
        kept && self.saved_session_id(tab).is_some()
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
    /// The saved tabs a waiting restore skipped and whose drafts it holds.
    fn waiting_restore_skipped(&self) -> Vec<&SavedTab> {
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
            .flat_map(|go| go.skipped.iter().filter_map(|skipped| skipped.kept.as_ref()))
            .collect()
    }

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
    pub fn start_restore<F>(&mut self, from: TabId, mut go: RestoreGo, mut connect: F) -> RestoreRun
    where
        F: FnMut(&AgentId, &str) -> Result<RestoreConnect, String>,
    {
        let total = go.tabs.len() + go.skipped.len();
        let into = self.restore_holder(from);
        for skipped in &mut go.skipped {
            self.give_restored_draft(into, skipped.kept.take().and_then(|tab| tab.draft));
        }
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
                self.give_restored_draft(open, planned.draft);
                run.restored += 1;
                started.push((id, open));
                continue;
            }
            let connected = match connect(&planned.agent, &id) {
                Ok(connected) => connected,
                Err(reason) => {
                    // The tab does not come back; what was typed for it does.
                    self.give_restored_draft(into, planned.draft);
                    run.failed.push(Skipped {
                        label: planned.label,
                        reason,
                        kept: None,
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
            self.give_saved_row_key(at, planned.row_key.as_deref());
            self.give_restored_draft(target, planned.draft);
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
        for skipped in go.skipped {
            self.give_restored_draft(from, skipped.kept.and_then(|tab| tab.draft));
        }
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
            self.give_saved_row_key(at, planned.row_key.as_deref());
            self.give_restored_draft(target, planned.draft);
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

    /// The tab at `at` takes the row key of the saved record whose session it takes up, so a tab broken out into
    /// another window or brought back after a restart keeps its name. A record without one (an older file or peer)
    /// leaves the tab the key it was made with.
    fn give_saved_row_key(&mut self, at: usize, saved: Option<&str>) {
        if let Some(key) = saved.and_then(RowKey::parse) {
            self.tabs[at].row_key = key;
        }
    }

    /// A restored tab whose resume failed. `back_to` is [`RestoreRun::back_to`]: an empty tab again, in
    /// the mode it had before the restore and under a fresh row key, if it is the one the restore began in; gone if
    /// the restore made it. What went wrong is reported once for the whole restore, not left as a dead tab each.
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
                    // The restore gave this tab the record's key, and the record goes back unrestored (a hand-over
                    // returns it to its source, which takes the key up again): the empty tab is named afresh.
                    t.row_key = RowKey::new();
                    self.give_mode(at, mode);
                }
            }
            None => {
                let draft = self.get(tab).map(|t| t.draft().to_string());
                self.remove(tab);
                self.give_restored_draft(self.active, draft.filter(|text| !text.trim().is_empty()));
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
    live_edits: bool,
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
